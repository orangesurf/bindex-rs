//! Route table and handlers.

use bitcoin::{OutPoint, Transaction, Txid};
use serde_json::Value;

use crate::{
    deadline::Deadline,
    merkle,
    protocol::ElectrumScripthash,
    rest::{
        address,
        http::HttpResponse,
        json,
        query::{self, FoundTx},
        text,
        types::{
            merkleblock_hex, BlockStatus, BlockValue, MempoolInfo, MerkleProof, SpendingValue,
            TransactionStatus, TransactionValue,
        },
        ttl_by_depth, HttpError, Req, RestApi, Result, TTL_LONG, TTL_SHORT,
    },
};

/// `GET /txs/outspends?txids=` accepts at most this many.
const MAX_BATCH_TXIDS: usize = 50;

/// `POST /txs/test` and `POST /txs/package` accept at most this many.
const MAX_SUBMIT_TXS: usize = 25;

/// Hex-string length bounds the reference applies to each submitted transaction.
const MIN_TX_HEX: usize = 120;
const MAX_TX_HEX: usize = 800_000;

/// Confirmation targets `/fee-estimates` reports on.
const CONF_TARGETS: &[usize] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 144,
    504, 1008,
];

/// Dispatch one request.
///
/// Everything but broadcasting is blocking (index reads and bitcoind round
/// trips), so it runs under `block_in_place`; the REST server therefore needs a
/// multi-threaded runtime, which is what `main` and the tests use.
pub async fn route(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let segments = request.segments();
    // the routes that talk to bitcoind (or to the onion push endpoint) are async
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["tx"]) => return post_tx(api, request).await,
        ("GET", ["broadcast"]) => return get_broadcast(api, request).await,
        ("POST", ["txs", "test"]) => return txs_test(api, request).await,
        ("POST", ["txs", "package"]) => return txs_package(api, request).await,
        ("GET", ["fee-estimates"]) => return fee_estimates(api).await,
        _ => {}
    }
    tokio::task::block_in_place(|| route_blocking(api, request, &segments))
}

/// Routes that can replay a whole script history, scan for a spender, or build
/// every transaction of a block or of the mempool. These queue behind the query
/// semaphore; everything else answers from the header index or the mempool
/// index in memory and must stay available while they run.
pub(crate) fn is_expensive(method: &str, segments: &[&str]) -> bool {
    matches!(
        (method, segments),
        ("GET", ["block", _, "txs", ..])
            | ("GET", ["internal", "block", _, "txs"])
            | ("GET", ["tx", _, "outspend", _])
            | ("GET", ["tx", _, "outspends"])
            | ("GET", ["txs", "outspends"])
            | ("POST", ["internal", "txs", ..])
            | ("GET", ["address" | "scripthash", ..])
            | ("POST", ["addresses" | "scripthashes", ..])
            | ("GET", ["internal", "mempool", "txs", ..])
            | ("POST", ["internal", "mempool", "txs"])
    )
}

fn route_blocking(
    api: &RestApi,
    request: &Req<'_>,
    segments: &[&str],
) -> Result<HttpResponse> {
    match (request.method.as_str(), segments) {
        ("GET", ["blocks", "tip", "hash"]) => blocks_tip_hash(api),
        ("GET", ["blocks", "tip", "height"]) => blocks_tip_height(api),
        ("GET", ["blocks"]) => blocks(api, None),
        ("GET", ["blocks", start]) => blocks(api, Some(start)),
        ("GET", ["block-height", height]) => block_height(api, height),
        ("GET", ["block", hash]) => block(api, hash),
        ("GET", ["block", hash, "status"]) => block_status(api, hash),
        ("GET", ["block", hash, "txids"]) => block_txids(api, hash),
        ("GET", ["block", hash, "header"]) => block_header(api, hash),
        ("GET", ["block", hash, "raw"]) => block_raw(api, hash),
        ("GET", ["block", hash, "txid", index]) => block_txid_at(api, hash, index),
        ("GET", ["block", hash, "txs"]) => block_txs(api, request, hash, None),
        ("GET", ["block", hash, "txs", start]) => block_txs(api, request, hash, Some(start)),
        ("GET", ["internal", "block", hash, "txs"]) => internal_block_txs(api, request, hash),

        ("GET", ["tx", txid]) => tx(api, txid),
        ("GET", ["tx", txid, "hex"]) => tx_hex(api, txid),
        ("GET", ["tx", txid, "raw"]) => tx_raw(api, txid),
        ("GET", ["tx", txid, "status"]) => tx_status(api, txid),
        ("GET", ["tx", txid, "merkle-proof"]) => tx_merkle_proof(api, txid),
        ("GET", ["tx", txid, "merkleblock-proof"]) => tx_merkleblock_proof(api, txid),
        ("GET", ["tx", txid, "outspend", vout]) => tx_outspend(api, request, txid, vout),
        ("GET", ["tx", txid, "outspends"]) => tx_outspends(api, request, txid),
        ("GET", ["txs", "outspends"]) => txs_outspends(api, request),
        ("POST", ["internal", "txs"]) => internal_txs(api, request),
        ("POST", ["internal", "txs", "outspends", "by-txid"]) => {
            internal_outspends_by_txid(api, request)
        }
        ("POST", ["internal", "txs", "outspends", "by-outpoint"]) => {
            internal_outspends_by_outpoint(api, request)
        }

        ("GET", ["mempool"]) => mempool(api),
        ("GET", ["mempool", "txids"]) => mempool_txids(api),
        ("GET", ["mempool", "txids", "page"]) => mempool_txids_page(api, request, None),
        ("GET", ["mempool", "txids", "page", cursor]) => {
            mempool_txids_page(api, request, Some(cursor))
        }
        ("GET", ["mempool", "recent"]) => mempool_recent(api),
        ("GET", ["internal", "mempool", "txs", "all"]) => internal_mempool_txs_all(api),
        ("GET", ["internal", "mempool", "txs"]) => internal_mempool_txs(api, request, None),
        ("GET", ["internal", "mempool", "txs", cursor]) => {
            internal_mempool_txs(api, request, Some(cursor))
        }
        ("POST", ["internal", "mempool", "txs"]) => internal_mempool_txs_batch(api, request),

        ("GET", [prefix @ ("address" | "scripthash"), value, rest @ ..]) => {
            address::get_route(api, request, script_key(api, prefix, value)?, rest)
        }
        ("POST", [prefix @ ("addresses" | "scripthashes"), rest @ ..]) => {
            address::post_route(api, request, prefix, rest)
        }
        ("GET", ["address-prefix"]) | ("GET", ["address-prefix", _]) => {
            address::address_prefix(api)
        }

        _ => Err(unrouted(request)),
    }
}

/// The reference's fallthrough: no 405, a wrong method is just an unknown path.
pub(crate) fn unrouted(request: &Req<'_>) -> HttpError {
    HttpError::not_found(format!("endpoint does not exist {:?}", request.raw_target))
}

fn script_key(api: &RestApi, prefix: &str, value: &str) -> Result<query::ScriptKey> {
    match prefix {
        "address" => query::address_key(value, api.network),
        _ => query::scripthash_key(value),
    }
}

// ---------------------------------------------------------------- blocks

fn blocks_tip_hash(api: &RestApi) -> Result<HttpResponse> {
    let tip = query::tip_height(api)?;
    let hash = block_hash_at(api, tip)?;
    Ok(text(hash.to_string(), TTL_SHORT))
}

fn blocks_tip_height(api: &RestApi) -> Result<HttpResponse> {
    Ok(text(query::tip_height(api)?.to_string(), TTL_SHORT))
}

fn blocks(api: &RestApi, start: Option<&str>) -> Result<HttpResponse> {
    let tip = query::tip_height(api)?;
    // a non-numeric start (including `/blocks/tip`) means "from the tip"
    let start = start.and_then(|value| value.parse::<usize>().ok()).unwrap_or(tip);
    let mut values = Vec::new();
    let mut height = start;
    while values.len() < api.config.rest_default_block_limit {
        let Some(hash) = api.server.chain().hash_at_height(height)? else {
            if values.is_empty() {
                return Err(HttpError::not_found("Block not found"));
            }
            break;
        };
        values.push(block_value(api, &hash)?);
        if height == 0 {
            break;
        }
        height -= 1;
    }
    json(&values, TTL_SHORT)
}

fn block_height(api: &RestApi, height: &str) -> Result<HttpResponse> {
    let height = query::parse_usize(height)?;
    let hash = api
        .server
        .chain()
        .hash_at_height(height)?
        .ok_or_else(|| HttpError::not_found("Block not found"))?;
    let tip = query::tip_height(api)?;
    Ok(text(hash.to_string(), ttl_by_depth(Some(height), tip)))
}

fn block(api: &RestApi, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    // TTL_LONG unconditionally, even for the tip and for orphans
    json(&block_value(api, &hash)?, TTL_LONG)
}

fn block_status(api: &RestApi, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let json_block = query::block_json(api, &hash)?;
    let height = json_block
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| HttpError::server_error("block JSON carried no height"))? as usize;
    let in_best_chain = api.server.chain().hash_at_height(height)? == Some(hash);
    let status = if in_best_chain {
        BlockStatus {
            in_best_chain: true,
            height: Some(height),
            next_best: api
                .server
                .chain()
                .hash_at_height(height + 1)?
                .map(|hash| hash.to_string()),
        }
    } else {
        BlockStatus {
            in_best_chain: false,
            height: None,
            next_best: None,
        }
    };
    let tip = query::tip_height(api)?;
    json(&status, ttl_by_depth(status.height, tip))
}

fn block_txids(api: &RestApi, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let txids = query::block_txids(&query::block_json(api, &hash)?)?;
    let txids: Vec<String> = txids.iter().map(ToString::to_string).collect();
    json(&txids, TTL_LONG)
}

fn block_header(api: &RestApi, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let raw = api.core.header_raw(&hash).map_err(map_block_error)?;
    Ok(text(hex::encode(raw), TTL_LONG))
}

fn block_raw(api: &RestApi, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let raw = api.core.block_raw(&hash).map_err(map_block_error)?;
    Ok(HttpResponse::binary(raw, Some(TTL_LONG)))
}

fn block_txid_at(api: &RestApi, hash: &str, index: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let index = query::parse_usize(index)?;
    let txids = query::block_txids(&query::block_json(api, &hash)?)?;
    let txid = txids
        .get(index)
        .ok_or_else(|| HttpError::not_found("tx index out of range"))?;
    Ok(text(txid.to_string(), TTL_LONG))
}

fn block_txs(api: &RestApi, request: &Req<'_>, hash: &str, start: Option<&str>) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let page = api.config.rest_default_chain_txs_per_page.max(1);
    let start = start.map(query::parse_usize).transpose()?.unwrap_or(0);
    let (status, tx_count, in_best_chain, height) = block_context(api, &hash)?;

    if start >= tx_count {
        return Err(HttpError::not_found("start index out of range"));
    }
    if start % page != 0 {
        // the reference's typo, preserved
        return Err(HttpError::bad_request(format!(
            "start index must be a multipication of {page}"
        )));
    }

    let values = query::block_tx_values(api, &hash, &status, start..start + page, &request.deadline)?;
    let tip = query::tip_height(api)?;
    let ttl = if in_best_chain {
        ttl_by_depth(Some(height), tip)
    } else {
        TTL_SHORT
    };
    json(&values, ttl)
}

fn internal_block_txs(api: &RestApi, request: &Req<'_>, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let (status, tx_count, in_best_chain, height) = block_context(api, &hash)?;
    let values = query::block_tx_values(api, &hash, &status, 0..tx_count, &request.deadline)?;
    let tip = query::tip_height(api)?;
    let ttl = if in_best_chain {
        ttl_by_depth(Some(height), tip)
    } else {
        TTL_SHORT
    };
    json(&values, ttl)
}

// ---------------------------------------------------------------- helpers

fn map_block_error(err: crate::corerest::Error) -> HttpError {
    match err {
        crate::corerest::Error::NotFound => HttpError::not_found("Block not found"),
        other => HttpError::server_error(other.to_string()),
    }
}

fn block_hash_at(api: &RestApi, height: usize) -> Result<bitcoin::BlockHash> {
    api.server
        .chain()
        .hash_at_height(height)?
        .ok_or_else(|| HttpError::not_found("Block not found"))
}

fn block_value(api: &RestApi, hash: &bitcoin::BlockHash) -> Result<BlockValue> {
    let json = query::block_json(api, hash)?;
    BlockValue::from_core_json(&json)
        .ok_or_else(|| HttpError::server_error(format!("incomplete block JSON for {hash}")))
}

/// `(status of its transactions, tx count, whether it is in the best chain, height)`
fn block_context(
    api: &RestApi,
    hash: &bitcoin::BlockHash,
) -> Result<(crate::rest::types::TransactionStatus, usize, bool, usize)> {
    let json = query::block_json(api, hash)?;
    let height = json
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| HttpError::server_error("block JSON carried no height"))? as usize;
    let time = json
        .get("time")
        .and_then(Value::as_u64)
        .ok_or_else(|| HttpError::server_error("block JSON carried no time"))? as u32;
    let tx_count = json
        .get("nTx")
        .and_then(Value::as_u64)
        .ok_or_else(|| HttpError::server_error("block JSON carried no nTx"))? as usize;
    let in_best_chain = api.server.chain().hash_at_height(height)? == Some(*hash);
    Ok((
        crate::rest::types::TransactionStatus::confirmed(height, hash, time),
        tx_count,
        in_best_chain,
        height,
    ))
}

// ---------------------------------------------------------------- transactions

fn tx(api: &RestApi, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let found = found_tx(api, &txid)?;
    let value = query::tx_value(api, &found)?;
    let tip = query::tip_height(api)?;
    json(&value, ttl_by_depth(value.status.height(), tip))
}

fn tx_hex(api: &RestApi, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let found = found_tx(api, &txid)?;
    let tip = query::tip_height(api)?;
    Ok(text(
        hex::encode(&found.raw),
        ttl_by_depth(found.status.height(), tip),
    ))
}

fn tx_raw(api: &RestApi, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let found = found_tx(api, &txid)?;
    let tip = query::tip_height(api)?;
    Ok(HttpResponse::binary(
        found.raw,
        Some(ttl_by_depth(found.status.height(), tip)),
    ))
}

fn tx_status(api: &RestApi, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let found = found_tx(api, &txid)?;
    let tip = query::tip_height(api)?;
    json(&found.status, ttl_by_depth(found.status.height(), tip))
}

fn tx_merkle_proof(api: &RestApi, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let (height, txids, pos) = confirmed_block_position(api, &txid)?;
    let leaves: Vec<_> = txids.iter().map(|txid| *txid.as_raw_hash()).collect();
    let proof = merkle::branch_and_root(&leaves, pos);
    let tip = query::tip_height(api)?;
    json(
        &MerkleProof {
            block_height: height,
            merkle: proof.branch.iter().map(ToString::to_string).collect(),
            pos,
        },
        ttl_by_depth(Some(height), tip),
    )
}

fn tx_merkleblock_proof(api: &RestApi, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let (height, txids, _) = confirmed_block_position(api, &txid)?;
    let header = api
        .server
        .chain()
        .header_at_height(height)?
        .ok_or_else(|| HttpError::not_found("Block not found"))?;
    let proof =
        merkleblock_hex(&header, &txids, txid).map_err(HttpError::server_error)?;
    let tip = query::tip_height(api)?;
    Ok(text(proof, ttl_by_depth(Some(height), tip)))
}

fn tx_outspend(api: &RestApi, request: &Req<'_>, txid: &str, vout: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let vout = query::parse_u32(vout)?;
    let spending = outspend(api, &txid, vout, &request.deadline)?;
    let tip = query::tip_height(api)?;
    let ttl = ttl_by_depth(
        spending.status.as_ref().and_then(TransactionStatus::height),
        tip,
    );
    json(&spending, ttl)
}

fn tx_outspends(api: &RestApi, request: &Req<'_>, txid: &str) -> Result<HttpResponse> {
    let txid = query::parse_txid(txid)?;
    let found = found_tx(api, &txid)?;
    json(&outspends_of(api, &txid, &found.tx, &request.deadline)?, TTL_SHORT)
}

fn txs_outspends(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let Some(param) = request.param("txids") else {
        return Err(HttpError::bad_request("No txids specified"));
    };
    let txids: Vec<&str> = param.split(',').filter(|s| !s.is_empty()).collect();
    if txids.len() > MAX_BATCH_TXIDS {
        return Err(HttpError::bad_request("Too many txids requested"));
    }
    let mut out = Vec::with_capacity(txids.len());
    for txid in txids {
        out.push(outspends_or_empty(api, txid, &request.deadline)?);
    }
    json(&out, TTL_SHORT)
}

fn internal_txs(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let txids = txid_body(request)?;
    let mut out = Vec::new();
    for txid in txids {
        query::check_deadline(&request.deadline)?;
        if let Some(value) = query::tx_value_opt(api, &txid)? {
            out.push(value);
        }
    }
    json(&out, 0)
}

fn internal_outspends_by_txid(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let txids = txid_body(request)?;
    let mut out = Vec::with_capacity(txids.len());
    for txid in txids {
        out.push(match query::find_tx(api, &txid)? {
            Some(found) => outspends_of(api, &txid, &found.tx, &request.deadline)?,
            None => Vec::new(),
        });
    }
    json(&out, TTL_SHORT)
}

fn internal_outspends_by_outpoint(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let outpoints: Vec<String> = serde_json::from_slice(&request.body)
        .map_err(|err| HttpError::bad_request(err.to_string()).cached(0))?;
    let mut out = Vec::with_capacity(outpoints.len());
    for entry in outpoints {
        out.push(match parse_outpoint(&entry) {
            Some(outpoint) => outspend(api, &outpoint.txid, outpoint.vout, &request.deadline)?,
            None => SpendingValue::unspent(),
        });
    }
    json(&out, TTL_SHORT)
}

// ---------------------------------------------------------------- tx helpers

fn found_tx(api: &RestApi, txid: &Txid) -> Result<FoundTx> {
    query::find_tx(api, txid)?.ok_or_else(|| HttpError::not_found("Transaction not found"))
}

fn txid_body(request: &Req<'_>) -> Result<Vec<Txid>> {
    let raw: Vec<String> = serde_json::from_slice(&request.body)
        .map_err(|err| HttpError::bad_request(err.to_string()).cached(0))?;
    raw.iter()
        .map(|value| {
            query::parse_txid(value).map_err(|err| HttpError::bad_request(err.message).cached(0))
        })
        .collect()
}

fn parse_outpoint(value: &str) -> Option<OutPoint> {
    let (txid, vout) = value.split_once(':')?;
    Some(OutPoint {
        txid: txid.parse().ok()?,
        vout: vout.parse().ok()?,
    })
}

/// The block a confirmed transaction sits in, its txids, and its position.
fn confirmed_block_position(
    api: &RestApi,
    txid: &Txid,
) -> Result<(usize, Vec<Txid>, usize)> {
    let found = query::find_tx(api, txid)?
        .filter(|found| found.status.confirmed)
        .ok_or_else(|| HttpError::not_found("Transaction not found or is unconfirmed"))?;
    let height = found
        .status
        .height()
        .ok_or_else(|| HttpError::server_error("confirmed transaction without a height"))?;
    let hash = api
        .server
        .chain()
        .hash_at_height(height)?
        .ok_or_else(|| HttpError::not_found("Block not found"))?;
    let txids = query::block_txids(&query::block_json(api, &hash)?)?;
    let pos = match found.position {
        Some(position) if txids.get(position as usize) == Some(txid) => position as usize,
        _ => txids
            .iter()
            .position(|candidate| candidate == txid)
            .ok_or_else(|| HttpError::server_error("transaction missing from its block"))?,
    };
    Ok((height, txids, pos))
}

/// Who spent one output, if anyone.
///
/// bindex has no spending index (the `spending` column family is unused), so
/// the spender is found through the funding script: a transaction spending an
/// output is indexed under that output's scripthash, from the funding block
/// onwards. The cost is therefore proportional to how often the script has been
/// reused, which is why this is the one route that can be slow on a hot address.
fn outspend(
    api: &RestApi,
    txid: &Txid,
    vout: u32,
    deadline: &Deadline,
) -> Result<SpendingValue> {
    query::check_deadline(deadline)?;
    let Some(found) = query::find_tx(api, txid)? else {
        return Ok(SpendingValue::unspent());
    };
    let Some(output) = found.tx.output.get(vout as usize) else {
        return Ok(SpendingValue::unspent());
    };
    let script = output.script_pubkey.as_script();
    if script.is_empty() || script.is_op_return() {
        // never indexed, and unspendable anyway
        return Ok(SpendingValue::unspent());
    }
    let outpoint = OutPoint { txid: *txid, vout };

    let mempool_spender = {
        let mempool = api.server.mempool().read().expect("mempool lock");
        mempool.spender(&outpoint).and_then(|spender| {
            mempool
                .raw_transaction(&spender)
                .map(<[u8]>::to_vec)
                .map(|raw| (spender, raw))
        })
    };
    if let Some((spender, raw)) = mempool_spender {
        let tx: Transaction = bitcoin::consensus::deserialize(&raw)
            .map_err(|err| HttpError::server_error(format!("decode {spender}: {err}")))?;
        let vin = tx
            .input
            .iter()
            .position(|input| input.previous_output == outpoint)
            .unwrap_or(0) as u32;
        return Ok(SpendingValue::spent(
            spender,
            vin,
            TransactionStatus::unconfirmed(),
        ));
    }

    let Some(funding_height) = found.status.height() else {
        // an unconfirmed output cannot have a confirmed spender
        return Ok(SpendingValue::unspent());
    };
    let scripthash = ElectrumScripthash::from_script(script);
    let Some(spender) = api
        .server
        .chain()
        .find_spender(outpoint, scripthash, funding_height, deadline)?
    else {
        return Ok(SpendingValue::unspent());
    };
    let time = query::block_time(api, spender.height)?;
    Ok(SpendingValue::spent(
        spender.txid,
        spender.vin,
        TransactionStatus::confirmed(spender.height, &spender.block_hash, time),
    ))
}

fn outspends_of(
    api: &RestApi,
    txid: &Txid,
    tx: &Transaction,
    deadline: &Deadline,
) -> Result<Vec<SpendingValue>> {
    (0..tx.output.len() as u32)
        .map(|vout| outspend(api, txid, vout, deadline))
        .collect()
}

fn outspends_or_empty(
    api: &RestApi,
    txid: &str,
    deadline: &Deadline,
) -> Result<Vec<SpendingValue>> {
    let Ok(txid) = query::parse_txid(txid) else {
        return Ok(Vec::new());
    };
    match query::find_tx(api, &txid)? {
        Some(found) => outspends_of(api, &txid, &found.tx, deadline),
        None => Ok(Vec::new()),
    }
}

// ---------------------------------------------------------------- broadcast

/// `POST /tx` takes the raw hex as the body, not as JSON.
async fn post_tx(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let raw = String::from_utf8(request.body.clone())
        .map_err(|err| HttpError::bad_request(err.to_string()))?;
    broadcast(api, raw.trim()).await
}

async fn get_broadcast(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let raw = request
        .param("tx")
        .ok_or_else(|| HttpError::bad_request("Missing tx"))?
        .to_string();
    broadcast(api, raw.trim()).await
}

/// Honours `--broadcast-via`: with `tor` this is the same fresh-circuit push
/// the Electrum `blockchain.transaction.broadcast` method makes, and nothing is
/// submitted to the local node.
async fn broadcast(api: &RestApi, raw_tx_hex: &str) -> Result<HttpResponse> {
    let value = api
        .server
        .broadcast(raw_tx_hex)
        .await
        .map_err(|err| HttpError::bad_request(protocol_error_text(err)))?;
    let txid = value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string());
    Ok(HttpResponse::text(200, txid, Some(0)))
}

async fn txs_test(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    // With --broadcast-via tor nothing may reach the local node: handing it the
    // client's hex would leak exactly what the tor path exists to hide, and
    // mempool.space's onion has no testmempoolaccept endpoint to forward to.
    if api.server.config().broadcast_via == crate::config::BroadcastVia::Tor {
        return Err(HttpError::bad_request(
            "testmempoolaccept is unavailable with --broadcast-via tor",
        ));
    }
    let txs = submitted_txs(request)?;
    let maxfeerate = amount_param(request, "maxfeerate")?;
    check_tx_hex(&txs)?;
    let params = match maxfeerate {
        Some(maxfeerate) => serde_json::json!([txs, maxfeerate]),
        None => serde_json::json!([txs]),
    };
    let result = api
        .server
        .bitcoind()
        .call("testmempoolaccept", params)
        .await
        .map_err(|err| rpc_error("testmempoolaccept", err))?;
    json(&result, TTL_SHORT)
}

async fn txs_package(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let txs = submitted_txs(request)?;
    let maxfeerate = amount_param(request, "maxfeerate")?;
    let maxburnamount = amount_param(request, "maxburnamount")?;
    check_tx_hex(&txs)?;

    // with --broadcast-via tor the package goes to the onion package endpoint
    // on its own circuit, exactly as blockchain.transaction.broadcast_package
    // does; maxfeerate/maxburnamount have nowhere to go on that path
    if api.server.config().broadcast_via == crate::config::BroadcastVia::Tor {
        let result = api
            .server
            .broadcast_package(&txs)
            .await
            .map_err(|err| HttpError::bad_request(protocol_error_text(err)))?;
        return json(&result, TTL_SHORT);
    }

    let params = match (maxfeerate, maxburnamount) {
        (None, None) => serde_json::json!([txs]),
        (Some(feerate), None) => serde_json::json!([txs, feerate]),
        (feerate, Some(burn)) => serde_json::json!([
            txs,
            feerate.unwrap_or_else(|| "0.10000000".to_string()),
            burn
        ]),
    };
    let result = api
        .server
        .bitcoind()
        .call("submitpackage", params)
        .await
        .map_err(|err| rpc_error("submitpackage", err))?;
    json(&result, TTL_SHORT)
}

async fn fee_estimates(api: &RestApi) -> Result<HttpResponse> {
    if let Some(cached) = api.fee_estimates_cached() {
        return json(&cached, TTL_SHORT);
    }
    let mut estimates = serde_json::Map::new();
    for target in CONF_TARGETS {
        match api.server.bitcoind().estimate_fee(*target).await {
            // bitcoind reports BTC/kvB; Esplora reports sat/vB
            Ok(feerate) if feerate > 0.0 => {
                let sat_per_vb = feerate * 100_000_000.0 / 1000.0;
                estimates.insert(
                    target.to_string(),
                    serde_json::Number::from_f64(sat_per_vb)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                );
            }
            Ok(_) => {}
            Err(err) => return Err(rpc_error("estimatesmartfee", err)),
        }
    }
    let value = Value::Object(estimates);
    api.store_fee_estimates(value.clone());
    json(&value, TTL_SHORT)
}

/// The count check comes first, then the parameters, then the per-item checks.
fn submitted_txs(request: &Req<'_>) -> Result<Vec<String>> {
    let txs: Vec<String> = serde_json::from_slice(&request.body)
        .map_err(|err| HttpError::bad_request(err.to_string()))?;
    if txs.len() > MAX_SUBMIT_TXS {
        return Err(HttpError::bad_request(format!(
            "Exceeded maximum of {MAX_SUBMIT_TXS} transactions"
        )));
    }
    Ok(txs)
}

fn check_tx_hex(txs: &[String]) -> Result<()> {
    for (index, tx) in txs.iter().enumerate() {
        if tx.len() < MIN_TX_HEX || tx.len() > MAX_TX_HEX || hex::decode(tx).is_err() {
            return Err(HttpError::bad_request(format!(
                "Invalid transaction size/hex for item {index}"
            )));
        }
    }
    Ok(())
}

/// A BTC amount query parameter, forwarded to bitcoind with eight decimals.
fn amount_param(request: &Req<'_>, name: &str) -> Result<Option<String>> {
    let Some(raw) = request.param(name) else {
        return Ok(None);
    };
    let value: f64 = raw
        .parse()
        .map_err(|_| HttpError::bad_request(format!("Invalid {name}")))?;
    Ok(Some(format!("{value:.8}")))
}

fn rpc_error(method: &str, err: crate::bitcoind::Error) -> HttpError {
    let detail = match err {
        crate::bitcoind::Error::Rpc(message) => message,
        other => other.to_string(),
    };
    HttpError::bad_request(format!("{method} RPC error: {detail}"))
}

fn protocol_error_text(err: crate::protocol::Error) -> String {
    match err {
        crate::protocol::Error::Server(message) => message
            .strip_prefix("bitcoind RPC error: ")
            .unwrap_or(&message)
            .to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- mempool

fn mempool(api: &RestApi) -> Result<HttpResponse> {
    let mempool = api.server.mempool().read().expect("mempool lock");
    let (count, vsize, total_fee) = mempool.totals();
    json(
        &MempoolInfo {
            count,
            vsize,
            total_fee,
            fee_histogram: mempool.fee_histogram_bins(),
        },
        TTL_SHORT,
    )
}

fn mempool_txids(api: &RestApi) -> Result<HttpResponse> {
    let txids = sorted_mempool_txids(api);
    let txids: Vec<String> = txids.iter().map(ToString::to_string).collect();
    json(&txids, TTL_SHORT)
}

fn mempool_txids_page(
    api: &RestApi,
    request: &Req<'_>,
    cursor: Option<&str>,
) -> Result<HttpResponse> {
    let limit = api.config.capped_max_txs(
        query::query_usize(request, "max_txs"),
        api.config.rest_max_mempool_txid_page_size,
        api.config.rest_max_mempool_txid_page_size,
    );
    let page = paged_txids(api, cursor, limit)?;
    let page: Vec<String> = page.iter().map(ToString::to_string).collect();
    json(&page, TTL_SHORT)
}

fn mempool_recent(api: &RestApi) -> Result<HttpResponse> {
    let recent = api.server.mempool().read().expect("mempool lock").recent();
    json(&recent, crate::rest::TTL_MEMPOOL_RECENT)
}

fn internal_mempool_txs(
    api: &RestApi,
    request: &Req<'_>,
    cursor: Option<&str>,
) -> Result<HttpResponse> {
    let limit = api.config.capped_max_txs(
        query::query_usize(request, "max_txs"),
        api.config.rest_max_mempool_page_size,
        api.config.rest_max_mempool_page_size,
    );
    let page = paged_txids(api, cursor, limit)?;
    json(&mempool_tx_values(api, &page, &request.deadline)?, TTL_SHORT)
}

fn internal_mempool_txs_all(api: &RestApi) -> Result<HttpResponse> {
    let txids = sorted_mempool_txids(api);
    json(&mempool_tx_values(api, &txids, &Deadline::never())?, TTL_SHORT)
}

fn internal_mempool_txs_batch(api: &RestApi, request: &Req<'_>) -> Result<HttpResponse> {
    let requested = txid_body(request)?;
    let known: Vec<Txid> = {
        let mempool = api.server.mempool().read().expect("mempool lock");
        requested
            .into_iter()
            .filter(|txid| mempool.contains(txid))
            .collect()
    };
    json(&mempool_tx_values(api, &known, &request.deadline)?, 0)
}

fn sorted_mempool_txids(api: &RestApi) -> Vec<Txid> {
    api.server
        .mempool()
        .read()
        .expect("mempool lock")
        .sorted_txids()
}

/// One page of mempool txids in `Txid` order; the cursor names the last txid
/// of the previous page.
fn paged_txids(api: &RestApi, cursor: Option<&str>, limit: usize) -> Result<Vec<Txid>> {
    let txids = sorted_mempool_txids(api);
    let start = match cursor {
        Some(cursor) => {
            let cursor = query::parse_txid(cursor)?;
            match txids.binary_search(&cursor) {
                Ok(index) => index + 1,
                Err(index) => index,
            }
        }
        None => 0,
    };
    Ok(txids
        .into_iter()
        .skip(start)
        .take(limit)
        .collect())
}

fn mempool_tx_values(
    api: &RestApi,
    txids: &[Txid],
    deadline: &Deadline,
) -> Result<Vec<TransactionValue>> {
    let mut out = Vec::with_capacity(txids.len());
    for txid in txids {
        query::check_deadline(deadline)?;
        if let Some(value) = query::tx_value_opt(api, txid)? {
            out.push(value);
        }
    }
    Ok(out)
}
