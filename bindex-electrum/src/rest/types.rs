//! Response shapes.
//!
//! Every struct here mirrors a mempool/electrs REST response field for field
//! and in field order, because clients (and the explorer front-ends) compare
//! the JSON literally. `Option` fields that the reference omits when empty
//! carry `skip_serializing_if`; the ones it emits as `null` do not.

use bitcoin::{
    consensus::Encodable as _, hashes::Hash as _, Address, Network, OutPoint, Script, Transaction,
    TxIn, TxOut, Txid,
};
use serde::Serialize;
use serde_json::Value;

/// Anchor output (`OP_1 OP_PUSHBYTES_2 4e73`), P2A.
const ANCHOR_SCRIPT: [u8; 4] = [0x51, 0x02, 0x4e, 0x73];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionStatus {
    pub confirmed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_height: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_time: Option<u32>,
}

impl TransactionStatus {
    pub fn unconfirmed() -> Self {
        Self {
            confirmed: false,
            block_height: None,
            block_hash: None,
            block_time: None,
        }
    }

    pub fn confirmed(height: usize, block_hash: &bitcoin::BlockHash, block_time: u32) -> Self {
        Self {
            confirmed: true,
            block_height: Some(height),
            block_hash: Some(block_hash.to_string()),
            block_time: Some(block_time),
        }
    }

    pub fn height(&self) -> Option<usize> {
        self.block_height
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxOutValue {
    pub scriptpubkey: String,
    pub scriptpubkey_asm: String,
    pub scriptpubkey_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scriptpubkey_address: Option<String>,
    pub value: u64,
}

impl TxOutValue {
    pub fn new(txout: &TxOut, network: Network) -> Self {
        let script = txout.script_pubkey.as_script();
        Self {
            scriptpubkey: hex::encode(script.as_bytes()),
            scriptpubkey_asm: script.to_asm_string(),
            scriptpubkey_type: script_type(script).to_string(),
            scriptpubkey_address: script_address(script, network),
            value: txout.value.to_sat(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxInValue {
    pub txid: String,
    pub vout: u32,
    /// `null` when the input is a coinbase; never omitted.
    pub prevout: Option<TxOutValue>,
    pub scriptsig: String,
    pub scriptsig_asm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<Vec<String>>,
    pub is_coinbase: bool,
    pub sequence: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_redeemscript_asm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_witnessscript_asm: Option<String>,
}

impl TxInValue {
    pub fn new(txin: &TxIn, prevout: Option<&TxOut>, network: Network) -> Self {
        let is_coinbase = txin.previous_output.is_null();
        let (inner_redeemscript_asm, inner_witnessscript_asm) = inner_scripts(txin, prevout);
        Self {
            txid: txin.previous_output.txid.to_string(),
            vout: txin.previous_output.vout,
            prevout: (!is_coinbase)
                .then(|| prevout.map(|out| TxOutValue::new(out, network)))
                .flatten(),
            scriptsig: hex::encode(txin.script_sig.as_bytes()),
            scriptsig_asm: txin.script_sig.to_asm_string(),
            witness: (!txin.witness.is_empty())
                .then(|| txin.witness.iter().map(hex::encode).collect()),
            is_coinbase,
            sequence: txin.sequence.0,
            inner_redeemscript_asm,
            inner_witnessscript_asm,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionValue {
    pub txid: String,
    pub version: u32,
    pub locktime: u32,
    pub vin: Vec<TxInValue>,
    pub vout: Vec<TxOutValue>,
    pub size: usize,
    pub weight: u64,
    pub sigops: usize,
    pub fee: u64,
    pub status: TransactionStatus,
}

impl TransactionValue {
    /// `prevouts` must hold one entry per non-coinbase input, keyed by input index.
    pub fn new(
        tx: &Transaction,
        prevouts: &[Option<TxOut>],
        status: TransactionStatus,
        network: Network,
    ) -> Self {
        let is_coinbase = tx.is_coinbase();
        let fee = if is_coinbase {
            0
        } else {
            let inputs: u64 = prevouts
                .iter()
                .filter_map(|out| out.as_ref().map(|out| out.value.to_sat()))
                .sum();
            let outputs: u64 = tx.output.iter().map(|out| out.value.to_sat()).sum();
            inputs.saturating_sub(outputs)
        };
        let sigops = tx.total_sigop_cost(|outpoint| lookup_prevout(tx, prevouts, outpoint));
        Self {
            txid: tx.compute_txid().to_string(),
            version: tx.version.0 as u32,
            locktime: tx.lock_time.to_consensus_u32(),
            vin: tx
                .input
                .iter()
                .enumerate()
                .map(|(index, txin)| {
                    TxInValue::new(txin, prevouts.get(index).and_then(Option::as_ref), network)
                })
                .collect(),
            vout: tx
                .output
                .iter()
                .map(|txout| TxOutValue::new(txout, network))
                .collect(),
            size: tx.total_size(),
            weight: tx.weight().to_wu(),
            sigops,
            fee,
            status,
        }
    }
}

fn lookup_prevout(tx: &Transaction, prevouts: &[Option<TxOut>], outpoint: &OutPoint) -> Option<TxOut> {
    tx.input
        .iter()
        .position(|txin| txin.previous_output == *outpoint)
        .and_then(|index| prevouts.get(index))
        .and_then(Clone::clone)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockValue {
    pub id: String,
    pub height: u32,
    pub version: u32,
    pub timestamp: u32,
    pub tx_count: u32,
    pub size: u32,
    pub weight: u64,
    pub merkle_root: String,
    /// Explicitly `null` at genesis (the reference has no skip attribute here).
    pub previousblockhash: Option<String>,
    pub mediantime: u32,
    pub nonce: u32,
    pub bits: u32,
    pub difficulty: f64,
}

impl BlockValue {
    /// Build from bitcoind's `/rest/block/notxdetails/<hash>.json`.
    pub fn from_core_json(json: &Value) -> Option<Self> {
        Some(Self {
            id: json.get("hash")?.as_str()?.to_string(),
            height: json.get("height")?.as_u64()? as u32,
            version: json.get("version")?.as_u64()? as u32,
            timestamp: json.get("time")?.as_u64()? as u32,
            tx_count: json.get("nTx")?.as_u64()? as u32,
            size: json.get("size")?.as_u64()? as u32,
            weight: json.get("weight")?.as_u64()?,
            merkle_root: json.get("merkleroot")?.as_str()?.to_string(),
            previousblockhash: json
                .get("previousblockhash")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            mediantime: json.get("mediantime")?.as_u64()? as u32,
            nonce: json.get("nonce")?.as_u64()? as u32,
            bits: json
                .get("bits")
                .and_then(Value::as_str)
                .and_then(|bits| u32::from_str_radix(bits, 16).ok())?,
            difficulty: json.get("difficulty")?.as_f64()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockStatus {
    pub in_best_chain: bool,
    pub height: Option<usize>,
    pub next_best: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UtxoValue {
    pub txid: String,
    pub vout: u32,
    pub status: TransactionStatus,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpendingValue {
    pub spent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vin: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TransactionStatus>,
}

impl SpendingValue {
    pub fn unspent() -> Self {
        Self {
            spent: false,
            txid: None,
            vin: None,
            status: None,
        }
    }

    pub fn spent(txid: Txid, vin: u32, status: TransactionStatus) -> Self {
        Self {
            spent: true,
            txid: Some(txid.to_string()),
            vin: Some(vin),
            status: Some(status),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ScriptStats {
    pub tx_count: usize,
    pub funded_txo_count: usize,
    pub spent_txo_count: usize,
    pub funded_txo_sum: u64,
    pub spent_txo_sum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxHistorySummary {
    pub txid: String,
    pub height: usize,
    pub value: i64,
    pub time: u32,
    pub tx_position: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MempoolInfo {
    pub count: usize,
    pub vsize: u64,
    pub total_fee: u64,
    pub fee_histogram: Vec<(f32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MempoolRecentTx {
    pub txid: String,
    pub fee: u64,
    pub vsize: u64,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MerkleProof {
    pub block_height: usize,
    pub merkle: Vec<String>,
    pub pos: usize,
}

/// `scriptpubkey_type`, using the reference's vocabulary.
pub fn script_type(script: &Script) -> &'static str {
    if script.is_empty() {
        "empty"
    } else if script.is_op_return() {
        "op_return"
    } else if script.is_p2pk() {
        "p2pk"
    } else if script.is_p2pkh() {
        "p2pkh"
    } else if script.is_p2sh() {
        "p2sh"
    } else if script.is_p2wpkh() {
        "v0_p2wpkh"
    } else if script.is_p2wsh() {
        "v0_p2wsh"
    } else if script.is_p2tr() {
        "v1_p2tr"
    } else if script.as_bytes() == ANCHOR_SCRIPT {
        "anchor"
    } else if script.is_multisig() {
        "multisig"
    } else {
        "unknown"
    }
}

/// The address for a script, when one exists for this network.
pub fn script_address(script: &Script, network: Network) -> Option<String> {
    Address::from_script(script, network)
        .ok()
        .map(|address| address.to_string())
}

fn inner_scripts(txin: &TxIn, prevout: Option<&TxOut>) -> (Option<String>, Option<String>) {
    let Some(prevout) = prevout else {
        return (None, None);
    };
    let prev_script = prevout.script_pubkey.as_script();
    if prev_script.is_p2sh() {
        let Some(push) = last_scriptsig_push(txin) else {
            return (None, None);
        };
        let redeem = Script::from_bytes(&push);
        let witness_asm = redeem
            .is_p2wsh()
            .then(|| last_witness_script_asm(txin))
            .flatten();
        (Some(redeem.to_asm_string()), witness_asm)
    } else if prev_script.is_p2wsh() {
        (None, last_witness_script_asm(txin))
    } else {
        (None, None)
    }
}

fn last_scriptsig_push(txin: &TxIn) -> Option<Vec<u8>> {
    let mut last = None;
    for instruction in txin.script_sig.instructions() {
        match instruction {
            Ok(bitcoin::blockdata::script::Instruction::PushBytes(bytes)) => {
                last = Some(bytes.as_bytes().to_vec());
            }
            Ok(_) => last = None,
            Err(_) => return None,
        }
    }
    last
}

fn last_witness_script_asm(txin: &TxIn) -> Option<String> {
    txin.witness
        .last()
        .map(|script| Script::from_bytes(script).to_asm_string())
}

/// A `MerkleBlock` proving `txid` is in the block, as the reference serializes it.
#[cfg(not(feature = "liquid"))]
pub fn merkleblock_hex(
    header: &bitcoin::block::Header,
    txids: &[Txid],
    txid: Txid,
) -> Result<String, String> {
    let proof = bitcoin::merkle_tree::MerkleBlock::from_header_txids_with_predicate(
        header,
        txids,
        |candidate| *candidate == txid,
    );
    let mut bytes = Vec::new();
    proof
        .consensus_encode(&mut bytes)
        .map_err(|err| err.to_string())?;
    Ok(hex::encode(bytes))
}

/// Esplora's scripthash: `sha256(scriptPubKey)` in natural byte order, unlike
/// the Electrum protocol's reversed form.
pub fn rest_scripthash(script: &Script) -> [u8; 32] {
    bitcoin::hashes::sha256::Hash::hash(script.as_bytes()).to_byte_array()
}

#[cfg(all(test, not(feature = "liquid")))]
mod tests {
    use super::*;
    use bitcoin::{consensus::deserialize, Amount, ScriptBuf};

    fn script(hex_str: &str) -> ScriptBuf {
        ScriptBuf::from_bytes(hex::decode(hex_str).unwrap())
    }

    #[test]
    fn script_types_cover_the_reference_vocabulary() {
        assert_eq!(script_type(&script("")), "empty");
        assert_eq!(script_type(&script("6a0568656c6c6f")), "op_return");
        assert_eq!(
            script_type(&script(
                "410479be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8ac"
            )),
            "p2pk"
        );
        assert_eq!(
            script_type(&script("76a914c398efa9c392ba6013c5e04ee729755ef7f58b3288ac")),
            "p2pkh"
        );
        assert_eq!(
            script_type(&script("a914748284390f9e263a4b766a75d0633c50426eb87587")),
            "p2sh"
        );
        assert_eq!(
            script_type(&script("0014751e76e8199196d454941c45d1b3a323f1433bd6")),
            "v0_p2wpkh"
        );
        assert_eq!(
            script_type(&script(
                "00201863143c14c5166804bd19203356da136c985678cd4d27a1b8c6329604903262"
            )),
            "v0_p2wsh"
        );
        assert_eq!(
            script_type(&script(
                "5120a60869f0dbcf1dc659c9cecbaf8050135ea9e8cdc487053f1dc6880949dc684c"
            )),
            "v1_p2tr"
        );
        assert_eq!(script_type(&script("51024e73")), "anchor");
        assert_eq!(script_type(&script("00")), "unknown");
    }

    #[test]
    fn asm_uses_the_rust_bitcoin_spelling() {
        assert_eq!(
            script("76a914c398efa9c392ba6013c5e04ee729755ef7f58b3288ac").to_asm_string(),
            "OP_DUP OP_HASH160 OP_PUSHBYTES_20 c398efa9c392ba6013c5e04ee729755ef7f58b32 OP_EQUALVERIFY OP_CHECKSIG"
        );
    }

    #[test]
    fn addresses_are_derived_for_standard_scripts() {
        assert_eq!(
            script_address(&script("0014751e76e8199196d454941c45d1b3a323f1433bd6"), Network::Bitcoin),
            Some("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4".to_string())
        );
        assert_eq!(script_address(&script("6a0568656c6c6f"), Network::Bitcoin), None);
    }

    #[test]
    fn status_omits_null_fields() {
        let json = serde_json::to_string(&TransactionStatus::unconfirmed()).unwrap();
        assert_eq!(json, r#"{"confirmed":false}"#);
    }

    #[test]
    fn unspent_spending_value_is_just_the_flag() {
        assert_eq!(
            serde_json::to_string(&SpendingValue::unspent()).unwrap(),
            r#"{"spent":false}"#
        );
    }

    #[test]
    fn transaction_value_matches_the_reference_shape() {
        // Block 100000's second transaction, and the output it spends.
        let raw = hex::decode("0100000001032e38e9c0a84c6046d687d10556dcacc41d275ec55fc00779ac88fdf357a187000000008c493046022100c352d3dd993a981beba4a63ad15c209275ca9470abfcd57da93b58e4eb5dce82022100840792bc1f456062819f15d33ee7055cf7b5ee1af1ebcc6028d9cdb1c3af7748014104f46db5e9d61a9dc27b8d64ad23e7383a4e6ca164593c2527c038c0857eb67ee8e825dca65046b82c9331586c82e0fd1f633f25f87c161bc6f8a630121df2b3d3ffffffff0200e32321000000001976a914c398efa9c392ba6013c5e04ee729755ef7f58b3288ac000fe208010000001976a914948c765a6914d43f2a7ac177da2c2f6b52de3d7c88ac00000000").unwrap();
        let tx: Transaction = deserialize(&raw).unwrap();
        let prevout = TxOut {
            value: Amount::from_sat(5_000_000_000),
            script_pubkey: script("76a91471d7dd96d9edda09180fe9d57a477b5acc9cad1188ac"),
        };
        let value = TransactionValue::new(
            &tx,
            &[Some(prevout)],
            TransactionStatus::confirmed(
                100_000,
                &"000000000003ba27aa200b1cecaad478d2b00432346c3f1f3986da1afd33e506"
                    .parse()
                    .unwrap(),
                1_293_623_863,
            ),
            Network::Bitcoin,
        );
        assert_eq!(
            value.txid,
            "fff2525b8931402dd09222c50775608f75787bd2b87e56995a7bdd30f79702c4"
        );
        assert_eq!(value.version, 1);
        assert_eq!(value.locktime, 0);
        assert_eq!(value.size, 259);
        assert_eq!(value.weight, 1036);
        assert_eq!(value.fee, 0);
        assert_eq!(value.vin.len(), 1);
        assert!(!value.vin[0].is_coinbase);
        assert!(value.vin[0].witness.is_none());
        assert_eq!(
            value.vin[0].prevout.as_ref().unwrap().scriptpubkey_address.as_deref(),
            Some("1BNwxHGaFbeUBitpjy2AsKpJ29Ybxntqvb")
        );
        assert_eq!(value.vout.len(), 2);
        assert_eq!(value.vout[0].value, 556_000_000);
        // two p2pkh outputs, one legacy sigop each, at four weight units apiece
        assert_eq!(value.sigops, 8);

        // field order on the wire, which is what clients diff against
        let json = serde_json::to_string(&value).unwrap();
        let mut cursor = 0;
        for field in [
            "\"txid\"", "\"version\"", "\"locktime\"", "\"vin\"", "\"vout\"", "\"size\"",
            "\"weight\"", "\"sigops\"", "\"fee\"", "\"status\"",
        ] {
            let at = json[cursor..]
                .find(field)
                .unwrap_or_else(|| panic!("{field} missing or out of order in {json}"));
            cursor += at + field.len();
        }
    }

    #[test]
    fn rest_scripthash_is_not_reversed() {
        let spk = script("76a914c398efa9c392ba6013c5e04ee729755ef7f58b3288ac");
        let rest = hex::encode(rest_scripthash(&spk));
        let electrum = crate::protocol::ElectrumScripthash::from_script(&spk).to_string();
        let mut reversed = hex::decode(&rest).unwrap();
        reversed.reverse();
        assert_eq!(hex::encode(reversed), electrum);
        assert_ne!(rest, electrum);
    }
}
