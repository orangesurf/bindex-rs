//! Route table and handlers.

use serde_json::Value;

use crate::rest::{
    http::{HttpRequest, HttpResponse},
    json,
    query::{self},
    text,
    types::{BlockStatus, BlockValue},
    ttl_by_depth, HttpError, RestApi, Result, TTL_LONG, TTL_SHORT,
};

/// Dispatch one request.
///
/// Everything but broadcasting is blocking (index reads and bitcoind round
/// trips), so it runs under `block_in_place`; the REST server therefore needs a
/// multi-threaded runtime, which is what `main` and the tests use.
pub async fn route(api: &RestApi, request: &HttpRequest) -> Result<HttpResponse> {
    let segments = request.segments();
    tokio::task::block_in_place(|| route_blocking(api, request, &segments))
}

fn route_blocking(
    api: &RestApi,
    request: &HttpRequest,
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
        ("GET", ["block", hash, "txs"]) => block_txs(api, hash, None),
        ("GET", ["block", hash, "txs", start]) => block_txs(api, hash, Some(start)),
        ("GET", ["internal", "block", hash, "txs"]) => internal_block_txs(api, hash),
        _ => Err(unrouted(request)),
    }
}

/// The reference's fallthrough: no 405, a wrong method is just an unknown path.
fn unrouted(request: &HttpRequest) -> HttpError {
    HttpError::not_found(format!("endpoint does not exist {:?}", request.raw_target))
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

fn block_txs(api: &RestApi, hash: &str, start: Option<&str>) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let page = api.config.rest_default_chain_txs_per_page;
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

    let values = query::block_tx_values(api, &hash, &status, start..start + page)?;
    let tip = query::tip_height(api)?;
    let ttl = if in_best_chain {
        ttl_by_depth(Some(height), tip)
    } else {
        TTL_SHORT
    };
    json(&values, ttl)
}

fn internal_block_txs(api: &RestApi, hash: &str) -> Result<HttpResponse> {
    let hash = query::parse_block_hash(hash)?;
    let (status, tx_count, in_best_chain, height) = block_context(api, &hash)?;
    let values = query::block_tx_values(api, &hash, &status, 0..tx_count)?;
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
