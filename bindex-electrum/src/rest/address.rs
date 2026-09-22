//! Address and scripthash routes.
//!
//! Every route exists under both `/address/:addr` and `/scripthash/:hash`, and
//! in a multi-entry POST form under `/addresses` and `/scripthashes`. They all
//! run over the same thing: a fold of each script's whole confirmed history
//! (`ChainAdapter::script_history`) plus the mempool index.
//!
//! bindex indexes a script by an 8-byte prefix of its hash and has no per-address
//! statistics, so "what has this address done" is always a full history replay.
//! That is the reference's `ScriptStats` model too; it is simply not cached here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bitcoin::{BlockHash, Txid};
use serde::Serialize;

use crate::{
    chain::{ScriptHistory, ScriptUtxo},
    protocol::ElectrumScripthash,
    rest::{
        http::{HttpRequest, HttpResponse},
        json,
        query::{self, ScriptKey},
        types::{ScriptStats, TransactionStatus, TransactionValue, TxHistorySummary, UtxoValue},
        HttpError, RestApi, Result, TTL_SHORT,
    },
};

/// Cap on `?max_txs` for the transaction-list routes.
const MAX_HISTORY_TXS: usize = 100;
/// Entries accepted by the multi-address POST routes.
const MULTI_ADDRESS_LIMIT: usize = 300;
/// Body bytes accepted by the same routes.
const MULTI_ADDRESS_BODY_LIMIT: usize = 21600;

#[derive(Debug, Serialize)]
struct AddressStats {
    address: String,
    chain_stats: ScriptStats,
    mempool_stats: ScriptStats,
}

#[derive(Debug, Serialize)]
struct ScripthashStats {
    scripthash: String,
    chain_stats: ScriptStats,
    mempool_stats: ScriptStats,
}

/// `GET /address/:addr/…` and `GET /scripthash/:hash/…`.
pub fn get_route(
    api: &RestApi,
    request: &HttpRequest,
    key: ScriptKey,
    rest: &[&str],
) -> Result<HttpResponse> {
    match rest {
        [] => stats(api, key),
        ["txs"] => txs(api, request, &[key]),
        ["txs", "chain"] => txs_chain(api, request, &[key], None),
        ["txs", "chain", cursor] => txs_chain(api, request, &[key], Some(cursor)),
        ["txs", "mempool"] => txs_mempool(api, request, &[key]),
        ["txs", "summary"] => txs_summary(api, request, &[key], None),
        ["txs", "summary", cursor] => txs_summary(api, request, &[key], Some(cursor)),
        ["utxo"] => utxo(api, key),
        _ => Err(crate::rest::handlers::unrouted(request)),
    }
}

/// `POST /addresses/…` and `POST /scripthashes/…`.
pub fn post_route(
    api: &RestApi,
    request: &HttpRequest,
    prefix: &str,
    rest: &[&str],
) -> Result<HttpResponse> {
    let keys = multi_keys(api, request, prefix)?;
    match rest {
        ["txs"] => txs(api, request, &keys),
        ["txs", "summary"] => txs_summary(api, request, &keys, None),
        ["txs", "summary", cursor] => txs_summary(api, request, &keys, Some(cursor)),
        _ => Err(crate::rest::handlers::unrouted(request)),
    }
}

/// `GET /address-prefix/:prefix`.
///
/// bindex stores an 8-byte prefix of each scripthash and no addresses at all,
/// so there is nothing to enumerate: the route answers as the reference does
/// when address search is switched off.
pub fn address_prefix(api: &RestApi) -> Result<HttpResponse> {
    if !api.config.address_search {
        return Err(HttpError::bad_request("address search disabled"));
    }
    Err(HttpError::bad_request(
        "address search is not supported by this backend",
    ))
}

fn multi_keys(api: &RestApi, request: &HttpRequest, prefix: &str) -> Result<Vec<ScriptKey>> {
    if request.body.len() > MULTI_ADDRESS_BODY_LIMIT {
        return Err(HttpError::unprocessable("body too long"));
    }
    let entries: Vec<String> = serde_json::from_slice(&request.body)
        .map_err(|err| HttpError::bad_request(err.to_string()))?;
    if entries.len() > MULTI_ADDRESS_LIMIT {
        return Err(HttpError::unprocessable("body too long"));
    }
    // malformed entries are dropped, not rejected
    Ok(entries
        .iter()
        .filter_map(|entry| match prefix {
            "addresses" => query::address_key(entry, api.network).ok(),
            _ => query::scripthash_key(entry).ok(),
        })
        .collect())
}

// ---------------------------------------------------------------- routes

fn stats(api: &RestApi, key: ScriptKey) -> Result<HttpResponse> {
    let scripthash = key.electrum()?;
    let history = api.server.chain().script_history(scripthash)?;
    let chain_stats = ScriptStats {
        tx_count: history.tx_count(),
        funded_txo_count: history.funded_txo_count,
        spent_txo_count: history.spent_txo_count,
        funded_txo_sum: history.funded_txo_sum,
        spent_txo_sum: history.spent_txo_sum,
    };
    let mempool_stats = mempool_stats(api, scripthash, &history);
    match key {
        ScriptKey::Address(address, _) => json(
            &AddressStats {
                address,
                chain_stats,
                mempool_stats,
            },
            TTL_SHORT,
        ),
        ScriptKey::Scripthash(scripthash) => json(
            &ScripthashStats {
                scripthash,
                chain_stats,
                mempool_stats,
            },
            TTL_SHORT,
        ),
    }
}

fn txs(api: &RestApi, request: &HttpRequest, keys: &[ScriptKey]) -> Result<HttpResponse> {
    let histories = Histories::load(api, keys)?;
    let limit = api.config.capped_max_txs(
        query::query_usize(request, "max_txs"),
        api.config.rest_default_max_mempool_txs,
        MAX_HISTORY_TXS,
    );
    let cursor = request
        .param("after_txid")
        .map(query::parse_txid)
        .transpose()?;

    let mempool = histories.mempool_txids(api);
    let chain: Vec<Txid> = histories.rows_desc().iter().map(|row| row.txid).collect();

    let txids: Vec<Txid> = match cursor {
        None => mempool.into_iter().chain(chain).collect(),
        Some(cursor) => match mempool.iter().position(|txid| *txid == cursor) {
            // the cursor is in the mempool: finish the mempool page, then the
            // newest confirmed transactions
            Some(index) => mempool[index + 1..].iter().copied().chain(chain).collect(),
            None => match chain.iter().position(|txid| *txid == cursor) {
                // the cursor is confirmed: skip the mempool and resume there
                Some(index) => chain[index + 1..].to_vec(),
                None => return Err(HttpError::unprocessable("after_txid not found")),
            },
        },
    };

    json(&tx_values(api, &txids, limit)?, TTL_SHORT)
}

fn txs_chain(
    api: &RestApi,
    request: &HttpRequest,
    keys: &[ScriptKey],
    cursor: Option<&str>,
) -> Result<HttpResponse> {
    let histories = Histories::load(api, keys)?;
    let limit = api.config.capped_max_txs(
        query::query_usize(request, "max_txs"),
        api.config.rest_default_chain_txs_per_page,
        MAX_HISTORY_TXS,
    );
    let chain: Vec<Txid> = histories.rows_desc().iter().map(|row| row.txid).collect();
    // an unparseable cursor is ignored; a parseable one that is not in the
    // history has nothing after it
    let txids = match cursor.and_then(|cursor| cursor.parse::<Txid>().ok()) {
        Some(cursor) => match chain.iter().position(|txid| *txid == cursor) {
            Some(index) => chain[index + 1..].to_vec(),
            None => Vec::new(),
        },
        None => chain,
    };
    json(&tx_values(api, &txids, limit)?, TTL_SHORT)
}

fn txs_mempool(api: &RestApi, request: &HttpRequest, keys: &[ScriptKey]) -> Result<HttpResponse> {
    let histories = Histories::load(api, keys)?;
    let limit = api.config.capped_max_txs(
        query::query_usize(request, "max_txs"),
        api.config.rest_default_max_mempool_txs,
        MAX_HISTORY_TXS,
    );
    let txids = histories.mempool_txids(api);
    json(&tx_values(api, &txids, limit)?, TTL_SHORT)
}

fn txs_summary(
    api: &RestApi,
    request: &HttpRequest,
    keys: &[ScriptKey],
    cursor: Option<&str>,
) -> Result<HttpResponse> {
    let histories = Histories::load(api, keys)?;
    let limit = api.config.capped_max_txs(
        query::query_usize(request, "max_txs"),
        api.config.rest_default_max_address_summary_txs,
        api.config.rest_default_max_address_summary_txs,
    );
    let rows = histories.rows_desc();
    let start = match cursor {
        Some(cursor) => {
            let cursor = query::parse_txid(cursor)?;
            rows.iter()
                .position(|row| row.txid == cursor)
                .map(|index| index + 1)
                .ok_or_else(|| HttpError::unprocessable("after_txid not found"))?
        }
        None => 0,
    };

    let mut times: HashMap<usize, u32> = HashMap::new();
    let mut summaries = Vec::new();
    for row in rows.iter().skip(start).take(limit) {
        let time = match times.get(&row.height) {
            Some(time) => *time,
            None => {
                let time = query::block_time(api, row.height)?;
                times.insert(row.height, time);
                time
            }
        };
        summaries.push(TxHistorySummary {
            txid: row.txid.to_string(),
            height: row.height,
            value: row.funded as i64 - row.spent as i64,
            time,
            tx_position: row.position.min(u16::MAX as u32) as u16,
        });
    }
    json(&summaries, TTL_SHORT)
}

fn utxo(api: &RestApi, key: ScriptKey) -> Result<HttpResponse> {
    let scripthash = key.electrum()?;
    let history = api.server.chain().script_history(scripthash)?;
    if history.peak_live_utxos > api.config.utxos_limit {
        return Err(HttpError::bad_request(format!(
            "Too many UTXOs: {} exceeds the limit of {}",
            history.peak_live_utxos, api.config.utxos_limit
        )));
    }

    let (mempool_outputs, spent_in_mempool) = {
        let mempool = api.server.mempool().read().expect("mempool lock");
        let outputs = mempool.outputs_of(scripthash);
        let mut spent = BTreeSet::new();
        for utxo in &history.utxos {
            if mempool.spender(&utxo.outpoint).is_some() {
                spent.insert(utxo.outpoint);
            }
        }
        for (outpoint, _) in &outputs {
            if mempool.spender(outpoint).is_some() {
                spent.insert(*outpoint);
            }
        }
        (outputs, spent)
    };

    let mut blocks: HashMap<usize, (BlockHash, u32)> = HashMap::new();
    let mut values = Vec::new();
    for ScriptUtxo {
        outpoint,
        value,
        height,
        ..
    } in &history.utxos
    {
        if spent_in_mempool.contains(outpoint) {
            continue;
        }
        let (hash, time) = match blocks.get(height) {
            Some(entry) => *entry,
            None => {
                let hash = api
                    .server
                    .chain()
                    .hash_at_height(*height)?
                    .ok_or_else(|| HttpError::server_error("utxo block vanished"))?;
                let entry = (hash, query::block_time(api, *height)?);
                blocks.insert(*height, entry);
                entry
            }
        };
        values.push(UtxoValue {
            txid: outpoint.txid.to_string(),
            vout: outpoint.vout,
            status: TransactionStatus::confirmed(*height, &hash, time),
            value: *value,
        });
    }
    for (outpoint, value) in mempool_outputs {
        if spent_in_mempool.contains(&outpoint) {
            continue;
        }
        values.push(UtxoValue {
            txid: outpoint.txid.to_string(),
            vout: outpoint.vout,
            status: TransactionStatus::unconfirmed(),
            value,
        });
    }
    json(&values, TTL_SHORT)
}

// ---------------------------------------------------------------- shared

/// One transaction's effect on the requested scripts, merged across them.
#[derive(Debug, Clone)]
struct MergedRow {
    txid: Txid,
    height: usize,
    position: u32,
    funded: u64,
    spent: u64,
}

/// The confirmed histories of every script a request names.
struct Histories {
    entries: Vec<(ElectrumScripthash, ScriptHistory)>,
}

impl Histories {
    fn load(api: &RestApi, keys: &[ScriptKey]) -> Result<Self> {
        let mut entries = Vec::with_capacity(keys.len());
        let mut seen = BTreeSet::new();
        for key in keys {
            let scripthash = key.electrum()?;
            if !seen.insert(scripthash) {
                continue;
            }
            entries.push((scripthash, api.server.chain().script_history(scripthash)?));
        }
        Ok(Self { entries })
    }

    /// Newest first, by `(height, position)`, one row per transaction.
    fn rows_desc(&self) -> Vec<MergedRow> {
        let mut merged: BTreeMap<Txid, MergedRow> = BTreeMap::new();
        for (_, history) in &self.entries {
            for row in &history.rows {
                let entry = merged.entry(row.txid).or_insert(MergedRow {
                    txid: row.txid,
                    height: row.height,
                    position: row.position,
                    funded: 0,
                    spent: 0,
                });
                entry.funded += row.funded;
                entry.spent += row.spent;
            }
        }
        let mut rows: Vec<MergedRow> = merged.into_values().collect();
        rows.sort_by_key(|row| std::cmp::Reverse((row.height, row.position)));
        rows
    }

    /// Unconfirmed transactions touching any of the scripts, newest first.
    ///
    /// Funding transactions come straight from the mempool's scripthash index;
    /// spending ones are found by looking each of the script's outputs up in
    /// the mempool's spent-prevout map, which is why the poller never has to
    /// resolve a prevout.
    fn mempool_txids(&self, api: &RestApi) -> Vec<Txid> {
        let mempool = api.server.mempool().read().expect("mempool lock");
        let mut found: BTreeMap<Txid, u64> = BTreeMap::new();
        for (scripthash, history) in &self.entries {
            for txid in mempool.funding_txids(*scripthash) {
                if let Some(tx) = mempool.get(&txid) {
                    found.insert(txid, tx.time);
                }
            }
            let candidates = history
                .utxos
                .iter()
                .map(|utxo| utxo.outpoint)
                .chain(mempool.outputs_of(*scripthash).into_iter().map(|(o, _)| o));
            for outpoint in candidates {
                if let Some(spender) = mempool.spender(&outpoint) {
                    let time = mempool.get(&spender).map(|tx| tx.time).unwrap_or(0);
                    found.insert(spender, time);
                }
            }
        }
        let mut txids: Vec<(Txid, u64)> = found.into_iter().collect();
        txids.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        txids.into_iter().map(|(txid, _)| txid).collect()
    }
}

/// Mempool-side `ScriptStats` for one script.
fn mempool_stats(api: &RestApi, scripthash: ElectrumScripthash, history: &ScriptHistory) -> ScriptStats {
    let mempool = api.server.mempool().read().expect("mempool lock");
    let mut stats = ScriptStats::default();
    let mut txs = BTreeSet::new();

    let outputs = mempool.outputs_of(scripthash);
    for (outpoint, value) in &outputs {
        stats.funded_txo_count += 1;
        stats.funded_txo_sum += value;
        txs.insert(outpoint.txid);
    }

    let candidates = history
        .utxos
        .iter()
        .map(|utxo| (utxo.outpoint, utxo.value))
        .chain(outputs.iter().copied());
    for (outpoint, value) in candidates {
        if let Some(spender) = mempool.spender(&outpoint) {
            stats.spent_txo_count += 1;
            stats.spent_txo_sum += value;
            txs.insert(spender);
        }
    }

    stats.tx_count = txs.len();
    stats
}

fn tx_values(api: &RestApi, txids: &[Txid], limit: usize) -> Result<Vec<TransactionValue>> {
    let mut out = Vec::new();
    for txid in txids.iter().take(limit) {
        if let Some(value) = query::tx_value_opt(api, txid)? {
            out.push(value);
        }
    }
    Ok(out)
}
