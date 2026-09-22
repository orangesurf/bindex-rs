//! Data access shared by the handlers: parsing path and query parameters,
//! locating transactions, resolving prevouts, and reading blocks.

use std::str::FromStr;

use bitcoin::{
    consensus::deserialize, Address, Block, BlockHash, Network, OutPoint, ScriptBuf, Transaction,
    TxOut, Txid,
};
use serde_json::Value;

use crate::{
    corerest,
    protocol::ElectrumScripthash,
    rest::{
        http::HttpRequest,
        types::{TransactionStatus, TransactionValue},
        HttpError, RestApi, Result,
    },
};

pub fn parse_usize(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| HttpError::bad_request("Invalid number"))
}

pub fn parse_u32(value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .map_err(|_| HttpError::bad_request("Invalid number"))
}

pub fn parse_txid(value: &str) -> Result<Txid> {
    Txid::from_str(value).map_err(|_| HttpError::bad_request("Invalid hex hash"))
}

pub fn parse_block_hash(value: &str) -> Result<BlockHash> {
    BlockHash::from_str(value).map_err(|_| HttpError::bad_request("Invalid hex hash"))
}

/// A `?max_txs=` style parameter: unparseable or absent means "use the default".
pub fn query_usize(request: &HttpRequest, name: &str) -> Option<usize> {
    request.param(name).and_then(|value| value.parse().ok())
}

pub fn tip_height(api: &RestApi) -> Result<usize> {
    api.server
        .chain()
        .tip_height()?
        .ok_or_else(|| HttpError::server_error("chain has no headers"))
}

pub fn block_time(api: &RestApi, height: usize) -> Result<u32> {
    let header = api
        .server
        .chain()
        .header_at_height(height)?
        .ok_or_else(|| HttpError::not_found("Block not found"))?;
    Ok(header.time)
}

/// A transaction plus where it lives.
pub struct FoundTx {
    pub tx: Transaction,
    pub raw: Vec<u8>,
    pub status: TransactionStatus,
    /// Index within its block, for confirmed transactions.
    pub position: Option<u32>,
}

/// Look a transaction up in the chain index, then in the mempool.
pub fn find_tx(api: &RestApi, txid: &Txid) -> Result<Option<FoundTx>> {
    if let Some(located) = api.server.chain().located_transaction_by_txid(txid)? {
        let tx: Transaction = deserialize(&located.raw)
            .map_err(|err| HttpError::server_error(format!("decode {txid}: {err}")))?;
        let block_hash = parse_block_hash(&located.block_hash)?;
        let status = TransactionStatus::confirmed(
            located.height,
            &block_hash,
            block_time(api, located.height)?,
        );
        return Ok(Some(FoundTx {
            tx,
            raw: located.raw,
            status,
            position: Some(located.position),
        }));
    }

    let raw = {
        let mempool = api.server.mempool().read().expect("mempool lock");
        mempool.raw_transaction(txid).map(<[u8]>::to_vec)
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    let tx: Transaction = deserialize(&raw)
        .map_err(|err| HttpError::server_error(format!("decode mempool {txid}: {err}")))?;
    Ok(Some(FoundTx {
        tx,
        raw,
        status: TransactionStatus::unconfirmed(),
        position: None,
    }))
}

/// The output an outpoint refers to, from the mempool or the chain.
pub fn prevout(api: &RestApi, outpoint: &OutPoint) -> Result<Option<TxOut>> {
    let raw = {
        let mempool = api.server.mempool().read().expect("mempool lock");
        mempool.raw_transaction(&outpoint.txid).map(<[u8]>::to_vec)
    };
    let raw = match raw {
        Some(raw) => Some(raw),
        None => api
            .server
            .chain()
            .transaction_by_txid(&outpoint.txid)?,
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    let tx: Transaction = deserialize(&raw)
        .map_err(|err| HttpError::server_error(format!("decode {}: {err}", outpoint.txid)))?;
    Ok(tx.output.get(outpoint.vout as usize).cloned())
}

/// One entry per input; `None` where the previous output could not be found.
pub fn resolve_prevouts(api: &RestApi, tx: &Transaction) -> Result<Vec<Option<TxOut>>> {
    if tx.is_coinbase() {
        return Ok(vec![None; tx.input.len()]);
    }
    tx.input
        .iter()
        .map(|txin| prevout(api, &txin.previous_output))
        .collect()
}

/// Full transaction JSON. Missing prevouts are the reference's 500.
pub fn tx_value(api: &RestApi, found: &FoundTx) -> Result<TransactionValue> {
    let prevouts = resolve_prevouts(api, &found.tx)?;
    if !found.tx.is_coinbase() && prevouts.iter().any(Option::is_none) {
        return Err(HttpError::server_error("Transaction missing prevouts").cached(0));
    }
    Ok(TransactionValue::new(
        &found.tx,
        &prevouts,
        found.status.clone(),
        api.network,
    ))
}

/// Same, for list endpoints: anything whose prevouts cannot be resolved is
/// silently dropped, as `prepare_txs` does in the reference.
pub fn tx_value_opt(api: &RestApi, txid: &Txid) -> Result<Option<TransactionValue>> {
    let Some(found) = find_tx(api, txid)? else {
        return Ok(None);
    };
    match tx_value(api, &found) {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.status == 500 && err.message == "Transaction missing prevouts" => Ok(None),
        Err(err) => Err(err),
    }
}

/// `/rest/block/notxdetails/<hash>.json`, 404 `Block not found` when unknown.
pub fn block_json(api: &RestApi, hash: &BlockHash) -> Result<Value> {
    api.core.block_json(hash).map_err(|err| match err {
        corerest::Error::NotFound => HttpError::not_found("Block not found"),
        other => HttpError::server_error(other.to_string()),
    })
}

/// The block's txids in block order, from the same JSON.
pub fn block_txids(json: &Value) -> Result<Vec<Txid>> {
    json.get("tx")
        .and_then(Value::as_array)
        .ok_or_else(|| HttpError::server_error("block JSON carried no transaction list"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| HttpError::server_error("malformed txid in block JSON"))
                .and_then(parse_txid)
        })
        .collect()
}

/// Every transaction of a block with the outputs it spends, in block order.
///
/// Two REST round trips for the whole block: the block itself and
/// `/rest/spenttxouts`, which is exactly the prevout set the transaction JSON
/// needs (and what bindex already indexes blocks from).
pub fn block_transactions(
    api: &RestApi,
    hash: &BlockHash,
) -> Result<(Vec<Transaction>, Vec<Vec<TxOut>>)> {
    let raw = api.core.block_raw(hash).map_err(|err| match err {
        corerest::Error::NotFound => HttpError::not_found("Block not found"),
        other => HttpError::server_error(other.to_string()),
    })?;
    let block: Block = deserialize(&raw)
        .map_err(|err| HttpError::server_error(format!("decode block {hash}: {err}")))?;
    let spent_bytes = api.core.spent_txouts(hash)?;
    let spent = corerest::parse_spent_txouts(&spent_bytes)
        .map_err(|err| HttpError::server_error(err.to_string()))?;
    if spent.len() != block.txdata.len() {
        return Err(HttpError::server_error(format!(
            "block {hash} has {} transactions but {} spent-output lists",
            block.txdata.len(),
            spent.len()
        )));
    }
    Ok((block.txdata, spent))
}

/// Build the JSON for every transaction of a block, using the block's own
/// spent-output stream for prevouts.
pub fn block_tx_values(
    api: &RestApi,
    hash: &BlockHash,
    status: &TransactionStatus,
    range: std::ops::Range<usize>,
) -> Result<Vec<TransactionValue>> {
    let (txs, spent) = block_transactions(api, hash)?;
    let end = range.end.min(txs.len());
    let mut out = Vec::new();
    for index in range.start.min(end)..end {
        let tx = &txs[index];
        let prevouts = align_prevouts(tx, &spent[index]);
        if !tx.is_coinbase() && prevouts.iter().any(Option::is_none) {
            // the reference drops such transactions from list endpoints
            continue;
        }
        out.push(TransactionValue::new(
            tx,
            &prevouts,
            status.clone(),
            api.network,
        ));
    }
    Ok(out)
}

fn align_prevouts(tx: &Transaction, spent: &[TxOut]) -> Vec<Option<TxOut>> {
    if tx.is_coinbase() {
        return vec![None; tx.input.len()];
    }
    (0..tx.input.len())
        .map(|index| spent.get(index).cloned())
        .collect()
}

/// An address or a REST scripthash from the path, with the label the response
/// echoes it back under.
#[derive(Debug, Clone)]
pub enum ScriptKey {
    Address(String, ScriptBuf),
    Scripthash(String),
}

impl ScriptKey {
    pub fn label(&self) -> (&'static str, String) {
        match self {
            ScriptKey::Address(address, _) => ("address", address.clone()),
            ScriptKey::Scripthash(hash) => ("scripthash", hash.clone()),
        }
    }

    /// The Electrum-side (reversed) scripthash the index is keyed by.
    pub fn electrum(&self) -> Result<ElectrumScripthash> {
        match self {
            ScriptKey::Address(_, script) => {
                Ok(ElectrumScripthash::from_script(script.as_script()))
            }
            ScriptKey::Scripthash(hash) => {
                let mut bytes = hex::decode(hash)
                    .map_err(|_| HttpError::bad_request("Invalid hex string"))?;
                if bytes.len() != 32 {
                    return Err(HttpError::bad_request("Invalid scripthash"));
                }
                // REST takes sha256(script) as given; Electrum wants it reversed
                bytes.reverse();
                Ok(ElectrumScripthash(
                    bytes.try_into().expect("32 bytes checked above"),
                ))
            }
        }
    }
}

pub fn address_key(value: &str, network: Network) -> Result<ScriptKey> {
    let address = Address::from_str(value)
        .map_err(|_| HttpError::bad_request("Invalid Bitcoin address"))?;
    if !network_accepts(&address, network) {
        return Err(HttpError::bad_request("Address on invalid network"));
    }
    let script = address.assume_checked().script_pubkey();
    Ok(ScriptKey::Address(value.to_string(), script))
}

pub fn scripthash_key(value: &str) -> Result<ScriptKey> {
    let bytes = hex::decode(value).map_err(|_| HttpError::bad_request("Invalid hex string"))?;
    if bytes.len() != 32 {
        return Err(HttpError::bad_request("Invalid scripthash"));
    }
    Ok(ScriptKey::Scripthash(value.to_ascii_lowercase()))
}

/// Mainnet addresses only on mainnet; the test networks accept each other, as
/// the reference does.
fn network_accepts(address: &Address<bitcoin::address::NetworkUnchecked>, network: Network) -> bool {
    let candidates: &[Network] = match network {
        Network::Bitcoin => &[Network::Bitcoin],
        _ => &[
            Network::Testnet,
            Network::Testnet4,
            Network::Signet,
            Network::Regtest,
        ],
    };
    candidates
        .iter()
        .any(|candidate| address.is_valid_for_network(*candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripthash_key_round_trips_through_the_electrum_form() {
        let script = ScriptBuf::from_bytes(
            hex::decode("76a914c398efa9c392ba6013c5e04ee729755ef7f58b3288ac").unwrap(),
        );
        let rest = hex::encode(crate::rest::types::rest_scripthash(script.as_script()));
        let from_rest = scripthash_key(&rest).unwrap().electrum().unwrap();
        assert_eq!(
            from_rest,
            ElectrumScripthash::from_script(script.as_script())
        );
    }

    #[test]
    fn scripthash_key_rejects_bad_input() {
        assert_eq!(
            scripthash_key("zz").unwrap_err().message,
            "Invalid hex string"
        );
        assert_eq!(
            scripthash_key("00").unwrap_err().message,
            "Invalid scripthash"
        );
    }

    #[test]
    fn addresses_are_checked_against_the_network() {
        assert!(address_key("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", Network::Bitcoin).is_ok());
        assert_eq!(
            address_key("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", Network::Regtest)
                .unwrap_err()
                .message,
            "Address on invalid network"
        );
        assert_eq!(
            address_key("not-an-address", Network::Bitcoin)
                .unwrap_err()
                .message,
            "Invalid Bitcoin address"
        );
        // the test networks accept each other
        assert!(address_key("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx", Network::Signet).is_ok());
    }
}
