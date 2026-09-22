//! Liquid response shapes: what electrs builds with `--features liquid`.
//!
//! Field order and omission rules follow `mempool/electrs` (`rest.rs`,
//! `elements/mod.rs`, `elements/peg.rs`) exactly, since clients diff the JSON:
//! values and assets are either explicit or a commitment, never both; peg-in
//! inputs carry no prevout; issuances and peg-outs get their own objects; the
//! explicit fee output is typed `fee`; address stats carry no sums and address
//! summaries a zero value, because a confidential amount cannot be added up.

use bitcoin::{hashes::Hash as _, OutPoint};
use elements::{
    confidential::{Asset, Nonce, Value},
    encode::serialize,
    issuance::ContractHash,
    secp256k1_zkp::ZERO_TWEAK,
    AssetId, Script, TxIn,
};
use serde::Serialize;
use serde_json::Value as Json;

use crate::rest::{
    format::{self, Params, Tx, TxOut},
    types::TransactionStatus,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PegoutValue {
    pub genesis_hash: String,
    pub scriptpubkey: String,
    pub scriptpubkey_asm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scriptpubkey_address: Option<String>,
}

impl PegoutValue {
    /// Only an explicit L-BTC output naming the parent chain counts as a
    /// peg-out; any other OP_RETURN of the same form is just data.
    fn from_txout(txout: &TxOut, params: Params) -> Option<Self> {
        let pegout = txout.pegout_data()?;
        if pegout.asset != Asset::Explicit(params.policy_asset)
            || pegout.genesis_hash != params.parent_genesis
        {
            return None;
        }
        let script = pegout.script_pubkey;
        Some(Self {
            genesis_hash: pegout.genesis_hash.to_string(),
            scriptpubkey: hex::encode(script.as_bytes()),
            scriptpubkey_asm: script.to_asm_string(),
            scriptpubkey_address: bitcoin::Address::from_script(&script, params.parent)
                .ok()
                .map(|address| address.to_string()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxOutValue {
    pub scriptpubkey: String,
    pub scriptpubkey_asm: String,
    pub scriptpubkey_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scriptpubkey_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valuecommitment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assetcommitment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pegout: Option<PegoutValue>,
}

impl TxOutValue {
    pub fn new(txout: &TxOut, params: Params) -> Self {
        let script = &txout.script_pubkey;
        Self {
            scriptpubkey: hex::encode(script.as_bytes()),
            scriptpubkey_asm: script.asm(),
            scriptpubkey_type: script_type(txout).to_string(),
            scriptpubkey_address: script_address(script, params),
            value: txout.value.explicit(),
            valuecommitment: value_commitment(&txout.value),
            asset: explicit_asset(&txout.asset),
            assetcommitment: asset_commitment(&txout.asset),
            pegout: PegoutValue::from_txout(txout, params),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssuanceValue {
    pub asset_id: String,
    pub is_reissuance: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_blinding_nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_hash: Option<String>,
    pub asset_entropy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assetamount: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assetamountcommitment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokenamount: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokenamountcommitment: Option<String>,
}

impl IssuanceValue {
    fn new(txin: &TxIn) -> Self {
        let issuance = &txin.asset_issuance;
        let is_reissuance = issuance.asset_blinding_nonce != ZERO_TWEAK;
        let entropy = if is_reissuance {
            elements::hashes::sha256::Midstate::from_byte_array(issuance.asset_entropy)
        } else {
            AssetId::generate_asset_entropy(
                txin.previous_output,
                ContractHash::from_byte_array(issuance.asset_entropy),
            )
        };
        Self {
            asset_id: AssetId::from_entropy(entropy).to_string(),
            is_reissuance,
            asset_blinding_nonce: is_reissuance
                .then(|| hex::encode(issuance.asset_blinding_nonce.as_ref())),
            contract_hash: (!is_reissuance)
                .then(|| ContractHash::from_byte_array(issuance.asset_entropy).to_string()),
            asset_entropy: entropy.to_string(),
            assetamount: issuance_amount(&issuance.amount),
            assetamountcommitment: value_commitment(&issuance.amount),
            tokenamount: issuance_amount(&issuance.inflation_keys),
            tokenamountcommitment: value_commitment(&issuance.inflation_keys),
        }
    }
}

/// An absent (null) issuance amount reads as an explicit zero.
fn issuance_amount(value: &Value) -> Option<u64> {
    match value {
        Value::Explicit(amount) => Some(*amount),
        Value::Null => Some(0),
        Value::Confidential(..) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxInValue {
    pub txid: String,
    pub vout: u32,
    /// `null` for the coinbase and for peg-ins; never omitted.
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
    pub is_pegin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuance: Option<IssuanceValue>,
}

impl TxInValue {
    pub fn new(txin: &TxIn, prevout: Option<&TxOut>, params: Params) -> Self {
        let witness = &txin.witness.script_witness;
        let (inner_redeemscript_asm, inner_witnessscript_asm) = match prevout {
            Some(prevout) => inner_scripts(txin, prevout),
            None => (None, None),
        };
        Self {
            txid: txin.previous_output.txid.to_string(),
            vout: txin.previous_output.vout,
            prevout: prevout.map(|out| TxOutValue::new(out, params)),
            scriptsig: hex::encode(txin.script_sig.as_bytes()),
            scriptsig_asm: txin.script_sig.asm(),
            witness: (!witness.is_empty()).then(|| witness.iter().map(hex::encode).collect()),
            is_coinbase: txin.is_coinbase(),
            sequence: txin.sequence.to_consensus_u32(),
            inner_redeemscript_asm,
            inner_witnessscript_asm,
            is_pegin: txin.is_pegin,
            issuance: txin.has_issuance().then(|| IssuanceValue::new(txin)),
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
    /// `prevouts` holds one entry per input, `None` where the input has no
    /// prevout to show (see `format::prevout_outpoints`).
    pub fn new(
        tx: &Tx,
        prevouts: &[Option<TxOut>],
        status: TransactionStatus,
        params: Params,
    ) -> Self {
        Self {
            txid: format::txid(tx).to_string(),
            version: tx.version,
            locktime: tx.lock_time.to_consensus_u32(),
            vin: tx
                .input
                .iter()
                .enumerate()
                .map(|(index, txin)| {
                    TxInValue::new(txin, prevouts.get(index).and_then(Option::as_ref), params)
                })
                .collect(),
            vout: tx
                .output
                .iter()
                .map(|txout| TxOutValue::new(txout, params))
                .collect(),
            size: tx.size(),
            weight: tx.weight() as u64,
            sigops: sigop_cost(tx, prevouts),
            // the fee is whatever the explicit fee outputs pay in the policy asset
            fee: tx.fee_in(params.policy_asset),
            status,
        }
    }
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
    /// Explicitly `null` at genesis.
    pub previousblockhash: Option<String>,
    pub mediantime: u32,
    /// The header's signblock/dynafed data; `/block/:hash` only, the list
    /// routes leave it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Json>,
}

impl BlockValue {
    /// Build from the node's `/rest/block/notxdetails/<hash>.json` (`getblock
    /// <hash> 1`), which on Elements has no nonce, bits or difficulty.
    pub fn from_core_json(json: &Json) -> Option<Self> {
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
                .and_then(Json::as_str)
                .map(ToString::to_string),
            mediantime: json.get("mediantime")?.as_u64()? as u32,
            ext: None,
        })
    }
}

/// The header's extension data (dynafed parameters or the legacy signblock
/// challenge and solution), serialized as rust-elements does, which is what
/// electrs emits.
pub fn header_ext(raw_header: &[u8]) -> Result<Json, String> {
    let header: elements::BlockHeader =
        elements::encode::deserialize(raw_header).map_err(|err| err.to_string())?;
    serde_json::to_value(&header.ext).map_err(|err| err.to_string())
}

/// Block time from a raw header.
pub fn header_time(raw_header: &[u8]) -> Result<u32, String> {
    let header: elements::BlockHeader =
        elements::encode::deserialize(raw_header).map_err(|err| err.to_string())?;
    Ok(header.time)
}

/// What liquid.network serves for a UTXO. The mempool/electrs source also
/// emits `surjection_proof` and `range_proof` (about 4 KB per entry); the
/// deployed reference does not, and nothing here reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UtxoValue {
    pub txid: String,
    pub vout: u32,
    pub status: TransactionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valuecommitment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assetcommitment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noncecommitment: Option<String>,
}

impl UtxoValue {
    pub fn new(outpoint: OutPoint, status: TransactionStatus, txout: &TxOut) -> Self {
        Self {
            txid: outpoint.txid.to_string(),
            vout: outpoint.vout,
            status,
            value: txout.value.explicit(),
            valuecommitment: value_commitment(&txout.value),
            asset: explicit_asset(&txout.asset),
            assetcommitment: asset_commitment(&txout.asset),
            nonce: match txout.nonce {
                Nonce::Explicit(nonce) => Some(hex::encode(nonce)),
                _ => None,
            },
            noncecommitment: match txout.nonce {
                Nonce::Confidential(..) => Some(hex::encode(serialize(&txout.nonce))),
                _ => None,
            },
        }
    }
}

/// Liquid address stats carry counts only, in the order liquid.network
/// serves them (the mempool/electrs source puts `tx_count` first).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ScriptStats {
    pub funded_txo_count: usize,
    pub spent_txo_count: usize,
    pub tx_count: usize,
}

impl ScriptStats {
    /// The sums are accepted and dropped: a blinded amount cannot be summed.
    pub fn new(
        tx_count: usize,
        funded_txo_count: usize,
        spent_txo_count: usize,
        _funded_txo_sum: u64,
        _spent_txo_sum: u64,
    ) -> Self {
        Self {
            tx_count,
            funded_txo_count,
            spent_txo_count,
        }
    }
}

/// `/address/:a/txs/summary` reports no value on Liquid.
pub fn summary_value(_funded: u64, _spent: u64) -> i64 {
    0
}

fn value_commitment(value: &Value) -> Option<String> {
    matches!(value, Value::Confidential(..)).then(|| hex::encode(serialize(value)))
}

fn explicit_asset(asset: &Asset) -> Option<String> {
    match asset {
        Asset::Explicit(id) => Some(id.to_string()),
        _ => None,
    }
}

fn asset_commitment(asset: &Asset) -> Option<String> {
    matches!(asset, Asset::Confidential(..)).then(|| hex::encode(serialize(asset)))
}

/// The unconfidential address of a script (no blinding key), as electrs
/// renders it.
pub fn script_address(script: &Script, params: Params) -> Option<String> {
    elements::Address::from_script(script, None, params.address).map(|a| a.to_string())
}

/// `scriptpubkey_type` in electrs's Liquid vocabulary.
pub fn script_type(txout: &TxOut) -> &'static str {
    let script = &txout.script_pubkey;
    let bytes = script.as_bytes();
    if txout.is_fee() {
        "fee"
    } else if script.is_empty() {
        "empty"
    } else if script.is_provably_unspendable() {
        "op_return"
    } else if script.is_p2pk() {
        "p2pk"
    } else if script.is_p2pkh() {
        "p2pkh"
    } else if script.is_p2sh() {
        "p2sh"
    } else if script.is_v0_p2wpkh() {
        "v0_p2wpkh"
    } else if script.is_v0_p2wsh() {
        "v0_p2wsh"
    } else if script.is_v1_p2tr() {
        "v1_p2tr"
    } else if bytes == [0x51, 0x02, 0x4e, 0x73] {
        "anchor"
    } else if is_bare_multisig(bytes) {
        "multisig"
    } else {
        "unknown"
    }
}

/// electrs's heuristic: `OP_M … OP_N OP_CHECKMULTISIG`, pubkeys unchecked.
fn is_bare_multisig(bytes: &[u8]) -> bool {
    const OP_PUSHNUM_1: u8 = 0x51;
    const OP_PUSHNUM_15: u8 = 0x5f;
    const OP_CHECKMULTISIG: u8 = 0xae;
    let len = bytes.len();
    len >= 37
        && bytes[len - 1] == OP_CHECKMULTISIG
        && (OP_PUSHNUM_1..=OP_PUSHNUM_15).contains(&bytes[len - 2])
        && bytes[0] >= OP_PUSHNUM_1
        && bytes[0] <= bytes[len - 2]
}

/// The redeem script of a P2SH spend and the witness script of a P2WSH, a
/// P2SH-wrapped P2WSH or a taproot script-path spend.
fn inner_scripts(txin: &TxIn, prevout: &TxOut) -> (Option<String>, Option<String>) {
    let witness = &txin.witness.script_witness;
    let redeem = if prevout.script_pubkey.is_p2sh() {
        match txin.script_sig.instructions().last() {
            Some(Ok(elements::script::Instruction::PushBytes(bytes))) => {
                Some(Script::from(bytes.to_vec()))
            }
            _ => None,
        }
    } else {
        None
    };
    let wants_witness_script = prevout.script_pubkey.is_v0_p2wsh()
        || prevout.script_pubkey.is_v1_p2tr()
        || redeem.as_ref().is_some_and(Script::is_v0_p2wsh);
    let witness_script = if !wants_witness_script {
        None
    } else if prevout.script_pubkey.is_v1_p2tr() {
        // BIP341: the script is second from last, third when an annex
        // (a last element starting 0x50) is present
        let len = witness.len();
        witness
            .last()
            .map(|last| if len >= 2 && last.first() == Some(&0x50) { 3 } else { 2 })
            .filter(|from_last| len >= *from_last)
            .and_then(|from_last| witness.get(len - from_last))
    } else {
        witness.last()
    };
    (
        redeem.map(|script| script.asm()),
        witness_script.map(|bytes| Script::from(bytes.clone()).asm()),
    )
}

// ---------------------------------------------------------------- sigops

/// Sigop cost, counted exactly as electrs counts it for Liquid: legacy sigops
/// times four, plus P2SH and witness sigops when every prevout is known. A
/// coinbase or any peg-in stops at the legacy count.
fn sigop_cost(tx: &Tx, prevouts: &[Option<TxOut>]) -> usize {
    let legacy: usize = tx
        .input
        .iter()
        .map(|txin| count_sigops(txin.script_sig.as_bytes(), false))
        .chain(
            tx.output
                .iter()
                .map(|txout| count_sigops(txout.script_pubkey.as_bytes(), false)),
        )
        .sum();
    let mut cost = legacy * 4;
    if tx.is_coinbase() || tx.input.iter().any(|txin| txin.is_pegin) {
        return cost;
    }
    let Some(prevouts) = prevouts
        .iter()
        .map(Option::as_ref)
        .collect::<Option<Vec<&TxOut>>>()
    else {
        return cost;
    };
    if prevouts.len() != tx.input.len() {
        return cost;
    }
    for (txin, prevout) in tx.input.iter().zip(&prevouts) {
        if prevout.script_pubkey.is_p2sh() {
            if let Some(Ok(elements::script::Instruction::PushBytes(redeem))) =
                txin.script_sig.instructions().last()
            {
                cost += count_sigops(redeem, true) * 4;
            }
        }
    }
    for (txin, prevout) in tx.input.iter().zip(&prevouts) {
        cost += witness_sigops(txin, prevout);
    }
    cost
}

fn count_sigops(script: &[u8], accurate: bool) -> usize {
    use bitcoin::blockdata::script::Instruction;
    use bitcoin::opcodes::all::{
        OP_CHECKMULTISIG, OP_CHECKMULTISIGVERIFY, OP_CHECKSIG, OP_CHECKSIGVERIFY,
    };
    let mut n = 0;
    let mut pushnum = None;
    for instruction in bitcoin::Script::from_bytes(script).instructions() {
        match instruction {
            Ok(Instruction::Op(op)) if op == OP_CHECKSIG || op == OP_CHECKSIGVERIFY => n += 1,
            Ok(Instruction::Op(op)) if op == OP_CHECKMULTISIG || op == OP_CHECKMULTISIGVERIFY => {
                n += match (accurate, pushnum) {
                    (true, Some(keys)) => usize::from(keys),
                    // MAX_PUBKEYS_PER_MULTISIG
                    _ => 20,
                };
            }
            Ok(Instruction::Op(op)) => {
                pushnum = match op.to_u8() {
                    byte @ 0x51..=0x60 => Some(byte - 0x50),
                    _ => None,
                };
            }
            _ => pushnum = None,
        }
    }
    n
}

fn witness_sigops(txin: &TxIn, prevout: &TxOut) -> usize {
    let script_sig = txin.script_sig.as_bytes();
    let push_only = || {
        bitcoin::Script::from_bytes(script_sig)
            .instructions()
            .all(|i| matches!(i, Ok(bitcoin::blockdata::script::Instruction::PushBytes(_))))
    };
    let program: Vec<u8> = if prevout.script_pubkey.is_witness_program() {
        prevout.script_pubkey.as_bytes().to_vec()
    } else if prevout.script_pubkey.is_p2sh() && !script_sig.is_empty() && push_only() {
        match bitcoin::Script::from_bytes(script_sig).instructions().last() {
            Some(Ok(bitcoin::blockdata::script::Instruction::PushBytes(bytes))) => {
                bytes.as_bytes().to_vec()
            }
            _ => return 0,
        }
    } else {
        return 0;
    };
    let script = Script::from(program);
    if script.is_v0_p2wsh() {
        txin.witness
            .script_witness
            .last()
            .map(|witness_script| count_sigops(witness_script, true))
            .unwrap_or(0)
    } else if script.is_v0_p2wpkh() {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Liquid transactions with the outputs they spend, and what
    /// liquid.network (electrs-liquid) serves for them, byte for byte.
    const FIXTURES: &[(&str, &str)] = &[
        ("coinbase", include_str!("../../tests/fixtures/liquid/coinbase.json")),
        ("confidential", include_str!("../../tests/fixtures/liquid/confidential.json")),
        ("issuance", include_str!("../../tests/fixtures/liquid/issuance.json")),
        ("reissuance", include_str!("../../tests/fixtures/liquid/reissuance.json")),
        ("pegin", include_str!("../../tests/fixtures/liquid/pegin.json")),
        ("pegout", include_str!("../../tests/fixtures/liquid/pegout.json")),
    ];

    #[test]
    fn transactions_match_electrs_liquid_byte_for_byte() {
        let params = format::params(bitcoin::Network::Bitcoin).unwrap();
        for (name, fixture) in FIXTURES {
            let fixture: Json = serde_json::from_str(fixture).unwrap();
            let raw = hex::decode(fixture["hex"].as_str().unwrap()).unwrap();
            let tx = format::decode_tx(&raw).unwrap();
            let prevouts: Vec<Option<TxOut>> = format::prevout_outpoints(&tx)
                .into_iter()
                .map(|outpoint| {
                    let outpoint = outpoint?;
                    let hex = fixture["prevouts"][format!("{}:{}", outpoint.txid, outpoint.vout)]
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}: fixture lacks {outpoint}"));
                    Some(elements::encode::deserialize(&hex::decode(hex).unwrap()).unwrap())
                })
                .collect();
            let expected = fixture["expected"].as_str().unwrap();
            let status: Json = serde_json::from_str::<Json>(expected).unwrap()["status"].clone();
            let status = TransactionStatus::confirmed(
                status["block_height"].as_u64().unwrap() as usize,
                &status["block_hash"].as_str().unwrap().parse().unwrap(),
                status["block_time"].as_u64().unwrap() as u32,
            );
            let value = TransactionValue::new(&tx, &prevouts, status, params);
            assert_eq!(serde_json::to_string(&value).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn bare_multisig_heuristic_matches_electrs() {
        // 1-of-1: OP_1 <33-byte key> OP_1 OP_CHECKMULTISIG
        let mut script = vec![0x51, 0x21];
        script.extend([2u8; 33]);
        script.extend([0x51, 0xae]);
        assert!(is_bare_multisig(&script));
        script[0] = 0x52; // 2-of-1
        assert!(!is_bare_multisig(&script));
    }

    #[test]
    fn sigops_count_multisig_keys_when_accurate() {
        // OP_2 <k> <k> <k> OP_3 OP_CHECKMULTISIG
        let mut script = vec![0x52];
        for _ in 0..3 {
            script.push(0x21);
            script.extend([2u8; 33]);
        }
        script.extend([0x53, 0xae]);
        assert_eq!(count_sigops(&script, true), 3);
        assert_eq!(count_sigops(&script, false), 20);
        assert_eq!(count_sigops(&[0xac], false), 1);
    }
}
