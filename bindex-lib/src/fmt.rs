//! Chain-format boundary.
//!
//! Everything in bindex that must understand how a block header or a
//! transaction is serialized goes through this module, so the rest of the
//! index only ever sees hashes, byte offsets and output scripts. Two
//! implementations exist behind a cargo feature:
//!
//! * default: Bitcoin, parsed with `rust-bitcoin`
//! * `liquid`: Elements/Liquid (dynafed headers, confidential outputs, peg-in
//!   inputs), parsed with `rust-elements`
//!
//! Both formats serialize the previous block hash at bytes 4..36 of the
//! header, which the header chain relies on. Hashes are always exposed as
//! `bitcoin::BlockHash` / `bitcoin::Txid` so callers stay format-agnostic.

use bitcoin::{hashes::Hash as _, BlockHash, OutPoint, Txid};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("bitcoin decoding failed: {0}")]
    Bitcoin(#[from] bitcoin::consensus::encode::Error),

    #[cfg(feature = "liquid")]
    #[error("elements decoding failed: {0}")]
    Elements(#[from] elements::encode::Error),

    #[error("malformed block: {0}")]
    Malformed(&'static str),
}

/// A block header as fetched from the node: its hash, its parent and its raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHeader {
    pub hash: BlockHash,
    pub prev_blockhash: BlockHash,
    pub raw: Vec<u8>,
}

/// What the index needs from one transaction inside a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxSummary {
    pub txid: Txid,
    /// Byte offset of the transaction within the raw block.
    pub offset: u32,
    pub size: u32,
    /// scriptPubKey of every output, in order.
    pub scripts: Vec<Vec<u8>>,
}

/// What an Electrum-style server needs from one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTx {
    pub txid: Txid,
    /// Previous outputs spent on *this* chain (coinbase and peg-in inputs are omitted).
    pub inputs: Vec<OutPoint>,
    pub outputs: Vec<TxOutView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOutView {
    pub script_pubkey: Vec<u8>,
    /// Explicit amount in satoshis; 0 for a confidential (blinded) output.
    pub value: u64,
}

/// Name of the format, used for the on-disk database directory.
pub const NAME: &str = imp::NAME;

/// Previous block hash from a raw header (bytes 4..36 in both formats).
pub fn prev_blockhash(raw_header: &[u8]) -> Result<BlockHash, Error> {
    let bytes = raw_header
        .get(4..36)
        .ok_or(Error::Malformed("header shorter than 36 bytes"))?;
    Ok(BlockHash::from_slice(bytes).expect("32 bytes"))
}

/// Length of the header at the start of a raw block.
pub fn header_len(block: &[u8]) -> Result<usize, Error> {
    imp::header_len(block)
}

/// Hash of a raw header.
pub fn header_hash(raw_header: &[u8]) -> Result<BlockHash, Error> {
    imp::header_hash(raw_header)
}

/// Split a concatenation of raw headers (the `/rest/headers` response).
pub fn split_headers(data: &[u8]) -> Result<Vec<RawHeader>, Error> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let len = imp::header_len(&data[pos..])?;
        let raw = data
            .get(pos..pos + len)
            .ok_or(Error::Malformed("truncated header"))?;
        out.push(RawHeader {
            hash: imp::header_hash(raw)?,
            prev_blockhash: prev_blockhash(raw)?,
            raw: raw.to_vec(),
        });
        pos += len;
    }
    Ok(out)
}

/// Transaction id of a raw transaction.
pub fn txid(raw_tx: &[u8]) -> Result<Txid, Error> {
    imp::txid(raw_tx)
}

/// Inputs and outputs of a raw transaction.
pub fn parse_tx(raw_tx: &[u8]) -> Result<ParsedTx, Error> {
    imp::parse_tx(raw_tx)
}

/// Walk a raw block: `(header length, one summary per transaction)`.
pub fn walk_block(block: &[u8]) -> Result<(usize, Vec<TxSummary>), Error> {
    imp::walk_block(block)
}

#[cfg(not(feature = "liquid"))]
mod imp {
    use super::*;
    use bitcoin::consensus::{deserialize, deserialize_partial};

    pub const NAME: &str = "bitcoin";

    pub fn header_len(_block: &[u8]) -> Result<usize, Error> {
        Ok(bitcoin::block::Header::SIZE)
    }

    pub fn header_hash(raw: &[u8]) -> Result<BlockHash, Error> {
        let header: bitcoin::block::Header = deserialize(raw)?;
        Ok(header.block_hash())
    }

    pub fn txid(raw: &[u8]) -> Result<Txid, Error> {
        let tx: bitcoin::Transaction = deserialize(raw)?;
        Ok(tx.compute_txid())
    }

    pub fn parse_tx(raw: &[u8]) -> Result<ParsedTx, Error> {
        let tx: bitcoin::Transaction = deserialize(raw)?;
        Ok(ParsedTx {
            txid: tx.compute_txid(),
            inputs: tx
                .input
                .iter()
                .map(|i| i.previous_output)
                .filter(|o| !o.is_null())
                .collect(),
            outputs: tx
                .output
                .iter()
                .map(|o| TxOutView {
                    script_pubkey: o.script_pubkey.as_bytes().to_vec(),
                    value: o.value.to_sat(),
                })
                .collect(),
        })
    }

    pub fn walk_block(block: &[u8]) -> Result<(usize, Vec<TxSummary>), Error> {
        let header_len = header_len(block)?;
        let mut pos = header_len;
        let (count, used) = deserialize_partial::<bitcoin::VarInt>(&block[pos..])?;
        pos += used;
        let mut txs = Vec::with_capacity(count.0 as usize);
        for _ in 0..count.0 {
            let (tx, used) = deserialize_partial::<bitcoin::Transaction>(&block[pos..])?;
            txs.push(TxSummary {
                txid: tx.compute_txid(),
                offset: pos as u32,
                size: used as u32,
                scripts: tx
                    .output
                    .iter()
                    .map(|o| o.script_pubkey.as_bytes().to_vec())
                    .collect(),
            });
            pos += used;
        }
        if pos != block.len() {
            return Err(Error::Malformed("trailing bytes after last transaction"));
        }
        Ok((header_len, txs))
    }
}

#[cfg(feature = "liquid")]
mod imp {
    use super::*;
    use elements::encode::{deserialize, deserialize_partial};

    pub const NAME: &str = "liquid";

    fn block_hash(h: elements::BlockHash) -> BlockHash {
        BlockHash::from_raw_hash(h.to_raw_hash())
    }

    fn txid_of(t: elements::Txid) -> Txid {
        Txid::from_raw_hash(t.to_raw_hash())
    }

    pub fn header_len(block: &[u8]) -> Result<usize, Error> {
        let (_header, used) = deserialize_partial::<elements::BlockHeader>(block)?;
        Ok(used)
    }

    pub fn header_hash(raw: &[u8]) -> Result<BlockHash, Error> {
        let header: elements::BlockHeader = deserialize(raw)?;
        Ok(block_hash(header.block_hash()))
    }

    pub fn txid(raw: &[u8]) -> Result<Txid, Error> {
        let tx: elements::Transaction = deserialize(raw)?;
        Ok(txid_of(tx.txid()))
    }

    pub fn parse_tx(raw: &[u8]) -> Result<ParsedTx, Error> {
        let tx: elements::Transaction = deserialize(raw)?;
        Ok(ParsedTx {
            txid: txid_of(tx.txid()),
            inputs: tx
                .input
                .iter()
                // coinbase and peg-in inputs do not spend a Liquid output
                .filter(|i| !i.is_pegin && !i.previous_output.is_null())
                .map(|i| OutPoint {
                    txid: txid_of(i.previous_output.txid),
                    vout: i.previous_output.vout,
                })
                .collect(),
            outputs: tx
                .output
                .iter()
                .map(|o| TxOutView {
                    script_pubkey: o.script_pubkey.as_bytes().to_vec(),
                    value: o.value.explicit().unwrap_or(0),
                })
                .collect(),
        })
    }

    pub fn walk_block(block: &[u8]) -> Result<(usize, Vec<TxSummary>), Error> {
        let header_len = header_len(block)?;
        let mut pos = header_len;
        let (count, used) = deserialize_partial::<elements::encode::VarInt>(&block[pos..])?;
        pos += used;
        let mut txs = Vec::with_capacity(count.0 as usize);
        for _ in 0..count.0 {
            let (tx, used) = deserialize_partial::<elements::Transaction>(&block[pos..])?;
            txs.push(TxSummary {
                txid: txid_of(tx.txid()),
                offset: pos as u32,
                size: used as u32,
                scripts: tx
                    .output
                    .iter()
                    .map(|o| o.script_pubkey.as_bytes().to_vec())
                    .collect(),
            });
            pos += used;
        }
        if pos != block.len() {
            return Err(Error::Malformed("trailing bytes after last transaction"));
        }
        Ok((header_len, txs))
    }
}

#[cfg(all(test, feature = "liquid"))]
mod liquid_tests {
    use super::*;

    const BLOCK_HEX: &str = include_str!("../tests/fixtures/liquid-block-4053000.hex");
    const BLOCK_JSON: &str = include_str!("../tests/fixtures/liquid-block-4053000.json");

    fn fixture() -> (Vec<u8>, serde_json::Value) {
        let block = hex_decode(BLOCK_HEX.trim());
        let json: serde_json::Value = serde_json::from_str(BLOCK_JSON).unwrap();
        (block, json)
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn header_hash_and_prev_match_the_node() {
        let (block, json) = fixture();
        let len = header_len(&block).unwrap();
        assert_eq!(len, 1464, "dynafed header length");
        let raw = &block[..len];
        assert_eq!(header_hash(raw).unwrap().to_string(), json["hash"]);
        assert_eq!(
            prev_blockhash(raw).unwrap().to_string(),
            json["previousblockhash"]
        );
        // split_headers on a single header round-trips
        let headers = split_headers(raw).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].raw, raw);
        assert_eq!(headers[0].hash.to_string(), json["hash"]);
    }

    #[test]
    fn walk_block_yields_every_txid_in_order() {
        let (block, json) = fixture();
        let (len, txs) = walk_block(&block).unwrap();
        assert_eq!(len, 1464);
        let expected: Vec<String> = json["tx"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            txs.iter().map(|t| t.txid.to_string()).collect::<Vec<_>>(),
            expected
        );
        // offsets tile the block exactly
        let mut pos = len as u32 + 1; // 1-byte varint (4 txs)
        for t in &txs {
            assert_eq!(t.offset, pos);
            pos += t.size;
            assert_eq!(txid(&block[t.offset as usize..(t.offset + t.size) as usize]).unwrap(), t.txid);
        }
        assert_eq!(pos as usize, block.len());
        // the coinbase has a fee output with an empty script; every tx has at least one output
        assert!(txs.iter().all(|t| !t.scripts.is_empty()));
    }

    #[test]
    fn parse_tx_skips_coinbase_input_and_reads_explicit_values() {
        let (block, _) = fixture();
        let (_, txs) = walk_block(&block).unwrap();
        let cb = &txs[0];
        let parsed = parse_tx(&block[cb.offset as usize..(cb.offset + cb.size) as usize]).unwrap();
        assert!(parsed.inputs.is_empty(), "coinbase spends nothing on-chain");
        assert_eq!(parsed.txid, cb.txid);
        let t1 = &txs[1];
        let parsed = parse_tx(&block[t1.offset as usize..(t1.offset + t1.size) as usize]).unwrap();
        assert!(!parsed.inputs.is_empty());
        // confidential outputs read as 0, the explicit fee output as its amount
        assert!(parsed.outputs.iter().any(|o| o.value == 0));
        assert!(parsed.outputs.iter().any(|o| o.script_pubkey.is_empty() && o.value > 0), "fee output");
    }
}

#[cfg(all(test, not(feature = "liquid")))]
mod bitcoin_tests {
    use super::*;

    // Bitcoin block 100000 (also used by the scripthash index tests)
    const BLOCK_HEX: &str = "0100000050120119172a610421a6c3011dd330d9df07b63616c2cc1f1cd00200000000006657a9252aacd5c0b2940996ecff952228c3067cc38d4885efb5a4ac4247e9f337221b4d4c86041b0f2b57100401000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4444acb83c4ec7a0e2f99dd7457516c5817242da796924ca4e99947d087fedf9ce467cb9f7c6287078f801df276fdf84ac000000000100000001032e38e9c0a84c6046d687d10556dcacc41d275ec55fc00779ac88fdf357a187000000008c493046022100c352d3dd993a981beba4a63ad15c209275ca9470abfcd57da93b58e4eb5dce82022100840792bc1f456062819f15d33ee7055cf7b5ee1af1ebcc6028d9cdb1c3af7748014104f46db5e9d61a9dc27b8d64ad23e7383a4e6ca164593c2527c038c0857eb67ee8e825dca65046b82c9331586c82e0fd1f633f25f87c161bc6f8a630121df2b3d3ffffffff0200e32321000000001976a914c398efa9c392ba6013c5e04ee729755ef7f58b3288ac000fe208010000001976a914948c765a6914d43f2a7ac177da2c2f6b52de3d7c88ac000000000100000001c33ebff2a709f13d9f9a7569ab16a32786af7d7e2de09265e41c61d078294ecf010000008a4730440220032d30df5ee6f57fa46cddb5eb8d0d9fe8de6b342d27942ae90a3231e0ba333e02203deee8060fdc70230a7f5b4ad7d7bc3e628cbe219a886b84269eaeb81e26b4fe014104ae31c31bf91278d99b8377a35bbce5b27d9fff15456839e919453fc7b3f721f0ba403ff96c9deeb680e5fd341c0fc3a7b90da4631ee39560639db462e9cb850fffffffff0240420f00000000001976a914b0dcbf97eabf4404e31d952477ce822dadbe7e1088acc060d211000000001976a9146b1281eec25ab4e1e0793ff4e08ab1abb3409cd988ac0000000001000000010b6072b386d4a773235237f64c1126ac3b240c84b917a3909ba1c43ded5f51f4000000008c493046022100bb1ad26df930a51cce110cf44f7a48c3c561fd977500b1ae5d6b6fd13d0b3f4a022100c5b42951acedff14abba2736fd574bdb465f3e6f8da12e2c5303954aca7f78f3014104a7135bfe824c97ecc01ec7d7e336185c81e2aa2c41ab175407c09484ce9694b44953fcb751206564a9c24dd094d42fdbfdd5aad3e063ce6af4cfaaea4ea14fbbffffffff0140420f00000000001976a91439aa3d569e06a1d7926dc4be1193c99bf2eb9ee088ac00000000";

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn bitcoin_walk_matches_consensus_decoding() {
        let block = hex_decode(BLOCK_HEX);
        let parsed: bitcoin::Block = bitcoin::consensus::deserialize(&block).unwrap();
        let (len, txs) = walk_block(&block).unwrap();
        assert_eq!(len, 80);
        assert_eq!(header_hash(&block[..80]).unwrap(), parsed.block_hash());
        assert_eq!(prev_blockhash(&block[..80]).unwrap(), parsed.header.prev_blockhash);
        assert_eq!(
            txs.iter().map(|t| t.txid).collect::<Vec<_>>(),
            parsed.txdata.iter().map(|t| t.compute_txid()).collect::<Vec<_>>()
        );
        let headers = split_headers(&block[..80]).unwrap();
        assert_eq!(headers[0].hash, parsed.block_hash());
        let p = parse_tx(&block[txs[1].offset as usize..(txs[1].offset + txs[1].size) as usize]).unwrap();
        assert_eq!(p.inputs.len(), 1);
        assert_eq!(p.outputs[0].value, parsed.txdata[1].output[0].value.to_sat());
    }
}
