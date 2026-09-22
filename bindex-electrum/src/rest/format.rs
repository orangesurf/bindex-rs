//! The chain format the REST handlers see.
//!
//! The handlers only need a handful of things from a transaction: its txid,
//! which outpoints its inputs spend, and the scripts of its outputs. This
//! module answers those for the transaction type of the build (rust-bitcoin by
//! default, rust-elements under `liquid`), so the route logic is shared and
//! only the response shapes in `types` differ.
//!
//! Txids and outpoints always come back as `bitcoin::Txid` / `bitcoin::OutPoint`,
//! the index's own types.

pub use imp::*;

#[cfg(not(feature = "liquid"))]
mod imp {
    use bitcoin::{consensus::deserialize, Block, OutPoint, Txid};

    pub type Tx = bitcoin::Transaction;
    pub type TxOut = bitcoin::TxOut;

    /// What the response shapes need to know about the network: on Bitcoin,
    /// just which addresses to render and accept.
    pub type Params = bitcoin::Network;

    pub fn params(network: bitcoin::Network) -> anyhow::Result<Params> {
        Ok(network)
    }

    pub fn decode_tx(raw: &[u8]) -> Result<Tx, String> {
        deserialize(raw).map_err(|err| err.to_string())
    }

    pub fn decode_block_txs(raw: &[u8]) -> Result<Vec<Tx>, String> {
        let block: Block = deserialize(raw).map_err(|err| err.to_string())?;
        Ok(block.txdata)
    }

    pub fn txid(tx: &Tx) -> Txid {
        tx.compute_txid()
    }

    pub fn is_coinbase(tx: &Tx) -> bool {
        tx.is_coinbase()
    }

    /// Per input, the outpoint whose output the JSON shows as `prevout`; `None`
    /// where there is none to show (the coinbase input).
    pub fn prevout_outpoints(tx: &Tx) -> Vec<Option<OutPoint>> {
        let coinbase = tx.is_coinbase();
        tx.input
            .iter()
            .map(|txin| (!coinbase).then_some(txin.previous_output))
            .collect()
    }

    pub fn output_count(tx: &Tx) -> usize {
        tx.output.len()
    }

    pub fn output(tx: &Tx, vout: u32) -> Option<&TxOut> {
        tx.output.get(vout as usize)
    }

    pub fn output_script(txout: &TxOut) -> &[u8] {
        txout.script_pubkey.as_bytes()
    }

    /// Outputs the index never records and nothing can spend.
    pub fn is_unspendable(txout: &TxOut) -> bool {
        txout.script_pubkey.is_empty() || txout.script_pubkey.is_op_return()
    }

    /// Which input of `tx` spends `outpoint`.
    pub fn spending_input(tx: &Tx, outpoint: &OutPoint) -> Option<u32> {
        tx.input
            .iter()
            .position(|input| input.previous_output == *outpoint)
            .map(|index| index as u32)
    }
}

#[cfg(feature = "liquid")]
mod imp {
    use bitcoin::{OutPoint, Txid};
    use elements::{address::AddressParams, encode::deserialize, AssetId, Block};

    pub type Tx = elements::Transaction;
    pub type TxOut = elements::TxOut;

    /// What the Liquid response shapes need to know about the network.
    #[derive(Debug, Clone, Copy)]
    pub struct Params {
        /// How Liquid addresses are rendered and accepted.
        pub address: &'static AddressParams,
        /// The parent chain, for peg-out addresses.
        pub parent: bitcoin::Network,
        /// The parent chain's genesis, which a peg-out must name to count as one.
        pub parent_genesis: bitcoin::BlockHash,
        /// The asset fees are paid in (L-BTC on Liquid).
        pub policy_asset: AssetId,
        /// Whether outputs can be peg-outs. electrs knows a pegged asset only
        /// for Liquid itself, so it shows `pegout` nowhere else.
        pub pegouts: bool,
    }

    /// `--network` names the parent chain, as it does for the index directory:
    /// `bitcoin` is Liquid, `testnet` Liquid testnet and `regtest` an
    /// `elementsregtest` chain with its default policy asset.
    pub fn params(network: bitcoin::Network) -> anyhow::Result<Params> {
        let (address, policy_asset) = match network {
            bitcoin::Network::Bitcoin => (
                &AddressParams::LIQUID,
                "6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d",
            ),
            bitcoin::Network::Testnet | bitcoin::Network::Testnet4 => (
                &AddressParams::LIQUID_TESTNET,
                "144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49",
            ),
            bitcoin::Network::Regtest => (
                &AddressParams::ELEMENTS,
                "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225",
            ),
            other => anyhow::bail!("no Liquid network has {other} as its parent chain"),
        };
        Ok(Params {
            address,
            parent: network,
            parent_genesis: bitcoin::constants::genesis_block(network).block_hash(),
            policy_asset: policy_asset.parse().expect("valid asset id"),
            pegouts: network == bitcoin::Network::Bitcoin,
        })
    }

    /// The initial-issuance "prevouts" of the Liquid testnet and regtest
    /// genesis, which spend nothing that exists.
    const INITIAL_ISSUANCE_PREVOUTS: [&str; 2] = [
        "50cdc410c9d0d61eeacc531f52d2c70af741da33af127c364e52ac1ee7c030a5",
        "0c52d2526a5c9f00e9fb74afd15dd3caaf17c823159a514f929ae25193a43a52",
    ];

    pub fn to_txid(txid: elements::Txid) -> Txid {
        Txid::from_raw_hash(txid.to_raw_hash())
    }

    pub fn decode_tx(raw: &[u8]) -> Result<Tx, String> {
        deserialize(raw).map_err(|err| err.to_string())
    }

    pub fn decode_block_txs(raw: &[u8]) -> Result<Vec<Tx>, String> {
        let block: Block = deserialize(raw).map_err(|err| err.to_string())?;
        Ok(block.txdata)
    }

    pub fn txid(tx: &Tx) -> Txid {
        to_txid(tx.txid())
    }

    pub fn is_coinbase(tx: &Tx) -> bool {
        tx.is_coinbase()
    }

    fn has_prevout(txin: &elements::TxIn) -> bool {
        if txin.is_coinbase() || txin.is_pegin {
            return false;
        }
        let txid = txin.previous_output.txid.to_string();
        !INITIAL_ISSUANCE_PREVOUTS.contains(&txid.as_str())
    }

    /// Per input, the outpoint whose output the JSON shows as `prevout`; `None`
    /// for the coinbase, a peg-in (its prevout is on the parent chain) and the
    /// genesis issuance.
    pub fn prevout_outpoints(tx: &Tx) -> Vec<Option<OutPoint>> {
        tx.input
            .iter()
            .map(|txin| {
                has_prevout(txin).then(|| OutPoint {
                    txid: to_txid(txin.previous_output.txid),
                    vout: txin.previous_output.vout,
                })
            })
            .collect()
    }

    pub fn output_count(tx: &Tx) -> usize {
        tx.output.len()
    }

    pub fn output(tx: &Tx, vout: u32) -> Option<&TxOut> {
        tx.output.get(vout as usize)
    }

    pub fn output_script(txout: &TxOut) -> &[u8] {
        txout.script_pubkey.as_bytes()
    }

    /// Outputs the index never records and nothing can spend: fee outputs,
    /// other empty scripts, and OP_RETURN (peg-outs included).
    pub fn is_unspendable(txout: &TxOut) -> bool {
        txout.script_pubkey.is_empty() || txout.script_pubkey.is_provably_unspendable()
    }

    /// Which input of `tx` spends `outpoint`. A peg-in names a parent-chain
    /// outpoint, so it never matches.
    pub fn spending_input(tx: &Tx, outpoint: &OutPoint) -> Option<u32> {
        tx.input
            .iter()
            .position(|input| {
                !input.is_pegin
                    && to_txid(input.previous_output.txid) == outpoint.txid
                    && input.previous_output.vout == outpoint.vout
            })
            .map(|index| index as u32)
    }
}

#[cfg(all(test, feature = "liquid"))]
mod tests {
    use super::*;

    #[test]
    fn parent_network_selects_the_liquid_chain() {
        let liquid = params(bitcoin::Network::Bitcoin).unwrap();
        assert_eq!(
            liquid.policy_asset.to_string(),
            "6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d"
        );
        assert_eq!(
            liquid.parent_genesis.to_string(),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
        assert!(params(bitcoin::Network::Signet).is_err());
    }
}
