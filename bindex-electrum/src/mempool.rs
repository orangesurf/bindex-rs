use std::collections::{BTreeMap, BTreeSet, HashMap};

use bitcoin::{OutPoint, Txid};
use serde::Serialize;

use crate::protocol::{ElectrumScripthash, HistoryEntry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MempoolEntry {
    pub tx_hash: String,
    pub height: i64,
    pub fee: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolTx {
    pub txid: Txid,
    pub raw: Vec<u8>,
    pub fee: u64,
    pub vsize: u64,
    pub ancestor_fees: u64,
    pub ancestor_vsize: u64,
    pub touches: BTreeSet<ElectrumScripthash>,
    pub spends: BTreeSet<OutPoint>,
    pub outputs: BTreeMap<u32, (ElectrumScripthash, u64)>,
}

#[derive(Debug, Default, Clone)]
pub struct MempoolIndex {
    txs: HashMap<Txid, MempoolTx>,
    by_scripthash: HashMap<ElectrumScripthash, BTreeSet<Txid>>,
    spent_prevouts: HashMap<OutPoint, Txid>,
}

impl MempoolIndex {
    pub fn replace_all(&mut self, txs: impl IntoIterator<Item = MempoolTx>) {
        self.txs.clear();
        self.by_scripthash.clear();
        self.spent_prevouts.clear();

        for tx in txs {
            self.insert(tx);
        }
    }

    pub fn insert(&mut self, tx: MempoolTx) {
        let txid = tx.txid;
        for scripthash in &tx.touches {
            self.by_scripthash
                .entry(*scripthash)
                .or_default()
                .insert(txid);
        }
        for prevout in &tx.spends {
            self.spent_prevouts.insert(*prevout, txid);
        }
        self.txs.insert(txid, tx);
    }

    pub fn remove(&mut self, txid: &Txid) -> Option<MempoolTx> {
        let tx = self.txs.remove(txid)?;
        for scripthash in &tx.touches {
            if let Some(set) = self.by_scripthash.get_mut(scripthash) {
                set.remove(txid);
                if set.is_empty() {
                    self.by_scripthash.remove(scripthash);
                }
            }
        }
        for prevout in &tx.spends {
            self.spent_prevouts.remove(prevout);
        }
        Some(tx)
    }

    pub fn touched_scripthashes(&self) -> impl Iterator<Item = ElectrumScripthash> + '_ {
        self.by_scripthash.keys().copied()
    }

    pub fn entries(&self, scripthash: ElectrumScripthash) -> Vec<MempoolEntry> {
        let Some(txids) = self.by_scripthash.get(&scripthash) else {
            return Vec::new();
        };
        txids
            .iter()
            .filter_map(|txid| self.txs.get(txid))
            .map(|tx| MempoolEntry {
                tx_hash: tx.txid.to_string(),
                height: 0,
                fee: tx.fee,
            })
            .collect()
    }

    pub fn history_entries(&self, scripthash: ElectrumScripthash) -> Vec<HistoryEntry> {
        self.entries(scripthash)
            .into_iter()
            .map(|entry| HistoryEntry {
                tx_hash: entry.tx_hash,
                height: entry.height,
                fee: Some(entry.fee),
            })
            .collect()
    }

    pub fn raw_transaction(&self, txid: &Txid) -> Option<&[u8]> {
        self.txs.get(txid).map(|tx| tx.raw.as_slice())
    }

    pub fn fee_histogram(&self) -> Vec<(f64, u64)> {
        let mut buckets: BTreeMap<u64, u64> = BTreeMap::new();
        for tx in self.txs.values() {
            if tx.vsize == 0 {
                continue;
            }
            let sats_per_vbyte = tx.fee.saturating_mul(1000) / tx.vsize;
            *buckets.entry(sats_per_vbyte).or_default() += tx.vsize;
        }
        buckets
            .into_iter()
            .rev()
            .map(|(rate, vsize)| (rate as f64 / 1000.0, vsize))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::{sha256d, Hash as _};

    #[test]
    fn insert_and_remove_updates_secondary_indexes() {
        let mut index = MempoolIndex::default();
        let sh = ElectrumScripthash::parse(&"22".repeat(32)).unwrap();
        let txid = Txid::from_raw_hash(sha256d::Hash::hash(b"tx"));
        index.insert(MempoolTx {
            txid,
            raw: vec![1, 2, 3],
            fee: 100,
            vsize: 50,
            ancestor_fees: 100,
            ancestor_vsize: 50,
            touches: [sh].into_iter().collect(),
            spends: BTreeSet::new(),
            outputs: BTreeMap::new(),
        });
        assert_eq!(index.entries(sh).len(), 1);
        assert!(index.remove(&txid).is_some());
        assert!(index.entries(sh).is_empty());
    }
}
