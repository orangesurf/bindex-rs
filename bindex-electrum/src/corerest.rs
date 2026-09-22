//! Blocking client for bitcoind's unauthenticated REST interface.
//!
//! bindex already requires `-rest` (it fetches every transaction body through
//! `/rest/blockpart`), so the REST API can lean on the same interface for the
//! things the index does not store: whole blocks, per-block spent outputs, and
//! the mempool. Everything here is blocking and must be called from a blocking
//! context (`spawn_blocking` / `block_in_place`).

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bitcoind REST request failed: {0}")]
    Transport(String),

    #[error("not found")]
    NotFound,

    #[error("bitcoind REST response decode failed: {0}")]
    Decode(String),
}

#[derive(Debug, Clone)]
pub struct CoreRest {
    agent: ureq::Agent,
    url: String,
}

impl CoreRest {
    pub fn new(url: String) -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
            url: url.trim_end_matches('/').to_string(),
        }
    }

    fn get_bytes(&self, path: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}{}", self.url, path);
        match self.agent.get(&url).call() {
            Ok(mut response) => response
                .body_mut()
                .with_config()
                .limit(1 << 30)
                .read_to_vec()
                .map_err(|err| Error::Decode(err.to_string())),
            Err(ureq::Error::StatusCode(404)) => Err(Error::NotFound),
            Err(err) => Err(Error::Transport(err.to_string())),
        }
    }

    fn get_json(&self, path: &str) -> Result<Value, Error> {
        let bytes = self.get_bytes(path)?;
        serde_json::from_slice(&bytes).map_err(|err| Error::Decode(err.to_string()))
    }

    /// Block metadata plus the txid list, without transaction bodies.
    pub fn block_json(&self, hash: &bitcoin::BlockHash) -> Result<Value, Error> {
        self.get_json(&format!("/rest/block/notxdetails/{hash}.json"))
    }

    pub fn block_raw(&self, hash: &bitcoin::BlockHash) -> Result<Vec<u8>, Error> {
        self.get_bytes(&format!("/rest/block/{hash}.bin"))
    }

    /// The outputs spent by each transaction of a block, in block order
    /// (`/rest/spenttxouts`, added in Core 30.0 and required by bindex).
    pub fn spent_txouts(&self, hash: &bitcoin::BlockHash) -> Result<Vec<u8>, Error> {
        self.get_bytes(&format!("/rest/spenttxouts/{hash}.bin"))
    }

    pub fn header_raw(&self, hash: &bitcoin::BlockHash) -> Result<Vec<u8>, Error> {
        self.get_bytes(&format!("/rest/headers/1/{hash}.bin"))
    }

    /// A byte range of a block (`/rest/blockpart`, the endpoint bindex fetches
    /// every transaction body through).
    pub fn block_part(
        &self,
        hash: &bitcoin::BlockHash,
        offset: u32,
        size: u32,
    ) -> Result<Vec<u8>, Error> {
        self.get_bytes(&format!(
            "/rest/blockpart/{hash}.bin?offset={offset}&size={size}"
        ))
    }

    /// Raw transaction bytes. Without `-txindex` this only answers for
    /// transactions in the mempool, which is exactly what the REST API needs it
    /// for (confirmed bodies come from the index).
    pub fn tx_raw(&self, txid: &bitcoin::Txid) -> Result<Vec<u8>, Error> {
        self.get_bytes(&format!("/rest/tx/{txid}.bin"))
    }

    /// `{ "<txid>": { fees: {base, ancestor, ...}, vsize, ... } }`
    pub fn mempool_contents(&self) -> Result<Value, Error> {
        self.get_json("/rest/mempool/contents.json?verbose=true")
    }
}

/// Split the `/rest/spenttxouts` stream into one `TxOut` list per transaction.
///
/// The encoding is a `CompactSize` transaction count, then per transaction a
/// `CompactSize` output count followed by that many consensus-encoded `TxOut`s.
/// The coinbase contributes an empty list.
#[cfg(not(feature = "liquid"))]
pub fn parse_spent_txouts(bytes: &[u8]) -> Result<Vec<Vec<bitcoin::TxOut>>, Error> {
    use bitcoin::consensus::Decodable;

    let mut cursor = bitcoin::io::Cursor::new(bytes);
    let txs_count = bitcoin::VarInt::consensus_decode_from_finite_reader(&mut cursor)
        .map_err(|err| Error::Decode(err.to_string()))?
        .0;
    let mut out = Vec::with_capacity(txs_count as usize);
    for _ in 0..txs_count {
        let outputs_count = bitcoin::VarInt::consensus_decode_from_finite_reader(&mut cursor)
            .map_err(|err| Error::Decode(err.to_string()))?
            .0;
        let mut outputs = Vec::with_capacity(outputs_count as usize);
        for _ in 0..outputs_count {
            outputs.push(
                bitcoin::TxOut::consensus_decode_from_finite_reader(&mut cursor)
                    .map_err(|err| Error::Decode(err.to_string()))?,
            );
        }
        out.push(outputs);
    }
    let consumed: usize = cursor.position().try_into().expect("cursor position");
    if consumed != bytes.len() {
        return Err(Error::Decode(format!(
            "{} trailing bytes after the spent outputs",
            bytes.len() - consumed
        )));
    }
    Ok(out)
}

#[cfg(all(test, not(feature = "liquid")))]
mod tests {
    use super::*;

    // The block-100000 fixture used by the scripthash index tests: four
    // transactions, the coinbase spending nothing.
    const SPENT_HEX: &str = "04000100f2052a010000001976a91471d7dd96d9edda09180fe9d57a477b5acc9cad1188ac0100a3e111000000001976a91435fbee6a3bf8d99f17724ec54787567393a8a6b188ac0140420f00000000001976a914c4eb47ecfdcf609a1848ee79acc2fa49d3caad7088ac";

    #[test]
    fn spent_outputs_split_per_transaction() {
        let parsed = parse_spent_txouts(&hex::decode(SPENT_HEX).unwrap()).unwrap();
        assert_eq!(parsed.len(), 4);
        assert!(parsed[0].is_empty(), "coinbase spends nothing");
        assert_eq!(parsed[1].len(), 1);
        assert_eq!(parsed[1][0].value.to_sat(), 5_000_000_000);
        assert_eq!(parsed[2].len(), 1);
        assert_eq!(parsed[3].len(), 1);
        assert_eq!(parsed[3][0].value.to_sat(), 1_000_000);
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = hex::decode(SPENT_HEX).unwrap();
        bytes.push(0);
        assert!(parse_spent_txouts(&bytes).is_err());
    }
}
