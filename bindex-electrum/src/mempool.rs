use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    sync::RwLock,
};

use bitcoin::{OutPoint, Txid};
use serde::Serialize;

use crate::{
    corerest::CoreRest,
    protocol::{ElectrumScripthash, HistoryEntry},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MempoolEntry {
    pub tx_hash: String,
    pub height: i64,
    pub fee: u64,
}

/// One script's share of the mempool, as the Electrum methods need it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptMempool {
    /// Transactions paying or spending from the script, height `0` (all inputs
    /// confirmed) before `-1` (an unconfirmed input), then by txid as hex: the
    /// protocol's canonical order.
    pub entries: Vec<MempoolEntry>,
    /// What the mempool pays the script minus what it spends of the script's
    /// outputs, confirmed or not.
    pub balance_delta: i64,
    /// Unconfirmed outputs paying the script that nothing in the mempool
    /// spends, by txid as hex then output index.
    pub utxos: Vec<(OutPoint, u64)>,
    /// The script's confirmed outputs that a mempool transaction spends.
    pub spent_confirmed: BTreeSet<OutPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolTx {
    pub txid: Txid,
    pub raw: Vec<u8>,
    pub fee: u64,
    pub vsize: u64,
    pub ancestor_fees: u64,
    pub ancestor_vsize: u64,
    /// When the node first accepted it, for newest-first ordering.
    pub time: u64,
    pub touches: BTreeSet<ElectrumScripthash>,
    pub spends: BTreeSet<OutPoint>,
    pub outputs: BTreeMap<u32, (ElectrumScripthash, u64)>,
}

impl MempoolTx {
    /// Total value of the transaction's outputs.
    #[cfg_attr(feature = "liquid", allow(dead_code))]
    pub fn value(&self) -> u64 {
        self.outputs.values().map(|(_, value)| value).sum()
    }
}

/// One entry of the bounded "recently added" queue behind `/mempool/recent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentTx {
    pub txid: String,
    pub fee: u64,
    pub vsize: u64,
    /// Not reported on Liquid, where output values are mostly blinded.
    #[cfg(not(feature = "liquid"))]
    pub value: u64,
}

#[derive(Debug, Default, Clone)]
pub struct MempoolIndex {
    txs: HashMap<Txid, MempoolTx>,
    by_scripthash: HashMap<ElectrumScripthash, BTreeSet<Txid>>,
    spent_prevouts: HashMap<OutPoint, Txid>,
    /// Newest first, capped by the poller.
    recent: VecDeque<RecentTx>,
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

    /// The mempool transactions in a script's history: those paying it, and
    /// those spending one of its outputs. Spends of confirmed outputs are found
    /// through `confirmed_outputs`, the script's confirmed unspent outputs,
    /// because the poller records only which scripts a transaction pays.
    pub fn script_txids(
        &self,
        scripthash: ElectrumScripthash,
        confirmed_outputs: &[OutPoint],
    ) -> BTreeSet<Txid> {
        let mut txids = self
            .by_scripthash
            .get(&scripthash)
            .cloned()
            .unwrap_or_default();
        let unconfirmed_outputs = txids
            .iter()
            .filter_map(|txid| self.txs.get(txid))
            .flat_map(|tx| {
                tx.outputs
                    .iter()
                    .filter(|(_, (output_scripthash, _))| *output_scripthash == scripthash)
                    .map(|(vout, _)| OutPoint {
                        txid: tx.txid,
                        vout: *vout,
                    })
            })
            .collect::<Vec<_>>();
        for outpoint in confirmed_outputs.iter().chain(&unconfirmed_outputs) {
            if let Some(spender) = self.spent_prevouts.get(outpoint) {
                txids.insert(*spender);
            }
        }
        txids
    }

    /// The script's share of the mempool, given its confirmed unspent outputs.
    /// Transactions in `mined` are skipped: between an index refresh and the
    /// next mempool poll, a newly mined transaction is in both.
    pub fn script_view(
        &self,
        scripthash: ElectrumScripthash,
        confirmed_utxos: &[(OutPoint, u64)],
        mined: &HashSet<Txid>,
    ) -> ScriptMempool {
        let confirmed_outpoints = confirmed_utxos
            .iter()
            .map(|(outpoint, _)| *outpoint)
            .collect::<Vec<_>>();
        let txs = self
            .script_txids(scripthash, &confirmed_outpoints)
            .into_iter()
            .filter(|txid| !mined.contains(txid))
            .filter_map(|txid| self.txs.get(&txid))
            .collect::<Vec<_>>();

        let mut view = ScriptMempool::default();
        let mut outputs = Vec::new();
        for tx in &txs {
            let unconfirmed_parent = tx.spends.iter().any(|prevout| {
                self.txs.contains_key(&prevout.txid) && !mined.contains(&prevout.txid)
            });
            view.entries.push(MempoolEntry {
                tx_hash: tx.txid.to_string(),
                height: if unconfirmed_parent { -1 } else { 0 },
                fee: tx.fee,
            });
            for (vout, (output_scripthash, value)) in &tx.outputs {
                if *output_scripthash == scripthash {
                    outputs.push((
                        OutPoint {
                            txid: tx.txid,
                            vout: *vout,
                        },
                        *value,
                    ));
                    view.balance_delta += *value as i64;
                }
            }
        }
        view.entries
            .sort_by(|a, b| (-a.height, &a.tx_hash).cmp(&(-b.height, &b.tx_hash)));

        let spent = |outpoint: &OutPoint| {
            self.spent_prevouts
                .get(outpoint)
                .is_some_and(|spender| !mined.contains(spender))
        };
        for (outpoint, value) in confirmed_utxos {
            if spent(outpoint) {
                view.spent_confirmed.insert(*outpoint);
                view.balance_delta -= *value as i64;
            }
        }
        for (outpoint, value) in outputs {
            if spent(&outpoint) {
                view.balance_delta -= value as i64;
            } else {
                view.utxos.push((outpoint, value));
            }
        }
        view.utxos
            .sort_by_key(|(outpoint, _)| (outpoint.txid.to_string(), outpoint.vout));
        view
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

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn get(&self, txid: &Txid) -> Option<&MempoolTx> {
        self.txs.get(txid)
    }

    pub fn contains(&self, txid: &Txid) -> bool {
        self.txs.contains_key(txid)
    }

    /// All txids, in `Txid` order (what a `BTreeMap`-backed mempool pages in).
    pub fn sorted_txids(&self) -> Vec<Txid> {
        let mut txids: Vec<Txid> = self.txs.keys().copied().collect();
        txids.sort_unstable();
        txids
    }

    /// `(count, vsize, total_fee)`.
    pub fn totals(&self) -> (usize, u64, u64) {
        let mut vsize = 0;
        let mut fee = 0;
        for tx in self.txs.values() {
            vsize += tx.vsize;
            fee += tx.fee;
        }
        (self.txs.len(), vsize, fee)
    }

    /// The mempool transaction spending `outpoint`, if any.
    pub fn spender(&self, outpoint: &OutPoint) -> Option<Txid> {
        self.spent_prevouts.get(outpoint).copied()
    }

    /// Mempool transactions paying this scripthash (funding only; spends are
    /// found through `spender`, since the index never resolves prevouts).
    pub fn funding_txids(&self, scripthash: ElectrumScripthash) -> Vec<Txid> {
        self.by_scripthash
            .get(&scripthash)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Unconfirmed outputs paying this scripthash, as `(outpoint, value)`.
    pub fn outputs_of(&self, scripthash: ElectrumScripthash) -> Vec<(OutPoint, u64)> {
        let mut out = Vec::new();
        for txid in self.funding_txids(scripthash) {
            let Some(tx) = self.txs.get(&txid) else {
                continue;
            };
            for (vout, (script, value)) in &tx.outputs {
                if *script == scripthash {
                    out.push((OutPoint { txid, vout: *vout }, *value));
                }
            }
        }
        out.sort_by_key(|(outpoint, _)| (outpoint.txid, outpoint.vout));
        out
    }

    pub fn recent(&self) -> Vec<RecentTx> {
        self.recent.iter().cloned().collect()
    }

    /// `/mempool`'s histogram: descending fee rate, one bin per ~50 kvB.
    pub fn fee_histogram_bins(&self) -> Vec<(f32, u32)> {
        let mut rates: Vec<(f32, u64)> = self
            .txs
            .values()
            .filter(|tx| tx.vsize > 0)
            .map(|tx| (tx.fee as f32 / tx.vsize as f32, tx.vsize))
            .collect();
        rates.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut histogram = Vec::new();
        let mut bin_size: u64 = 0;
        let mut last_rate = 0.0f32;
        for (rate, vsize) in rates {
            if bin_size > 50_000 && (last_rate - rate).abs() > f32::EPSILON {
                histogram.push((last_rate, bin_size.min(u32::MAX as u64) as u32));
                bin_size = 0;
            }
            last_rate = rate;
            bin_size += vsize;
        }
        if bin_size > 0 {
            histogram.push((last_rate, bin_size.min(u32::MAX as u64) as u32));
        }
        histogram
    }

    fn push_recent(&mut self, entry: RecentTx, cap: usize) {
        self.recent.push_front(entry);
        while self.recent.len() > cap {
            self.recent.pop_back();
        }
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

/// Entry metadata from `getrawmempool`-style verbose output.
#[derive(Debug, Clone, PartialEq)]
pub struct MempoolEntryInfo {
    pub fee: u64,
    pub vsize: u64,
    pub ancestor_fees: u64,
    pub ancestor_vsize: u64,
    pub time: u64,
}

fn btc_to_sats(value: Option<&serde_json::Value>) -> u64 {
    value
        .and_then(serde_json::Value::as_f64)
        .map(|btc| (btc * 1e8).round() as u64)
        .unwrap_or(0)
}

/// Parse one `/rest/mempool/contents.json?verbose=true` entry.
pub fn parse_entry(value: &serde_json::Value) -> MempoolEntryInfo {
    let fees = value.get("fees");
    MempoolEntryInfo {
        fee: btc_to_sats(fees.and_then(|f| f.get("base")).or_else(|| value.get("fee"))),
        vsize: value
            .get("vsize")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        ancestor_fees: btc_to_sats(fees.and_then(|f| f.get("ancestor"))),
        ancestor_vsize: value
            .get("ancestorsize")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        time: value
            .get("time")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    }
}

/// Build the index entry for one raw mempool transaction.
pub fn build_tx(
    raw: Vec<u8>,
    info: &MempoolEntryInfo,
) -> Result<MempoolTx, bindex::fmt::Error> {
    let parsed = bindex::fmt::parse_tx(&raw)?;
    let mut touches = BTreeSet::new();
    let mut outputs = BTreeMap::new();
    for (vout, output) in parsed.outputs.iter().enumerate() {
        let scripthash = ElectrumScripthash::from_script(bitcoin::Script::from_bytes(
            &output.script_pubkey,
        ));
        touches.insert(scripthash);
        outputs.insert(vout as u32, (scripthash, output.value));
    }
    Ok(MempoolTx {
        txid: parsed.txid,
        raw,
        fee: info.fee,
        // weight / 4, rounded down, as electrs computes it for /mempool, the
        // fee histogram and /mempool/recent; the node's own vsize rounds up
        // (and on Bitcoin Core also counts sigops)
        vsize: parsed.weight / 4,
        ancestor_fees: info.ancestor_fees,
        ancestor_vsize: info.ancestor_vsize,
        time: info.time,
        touches,
        spends: parsed.inputs.iter().copied().collect(),
        outputs,
    })
}

/// One mempool refresh: diff the node's mempool against the index, fetch the
/// bodies of everything new, and swap the result in.
///
/// Only the arrivals are fetched, one `/rest/tx` round trip each, spread over a
/// few threads; a steady-state poll costs as many requests as there were new
/// transactions. Prevouts are deliberately *not* resolved here — an input is
/// attributed to a script by looking its outpoint up in that script's UTXO set
/// at query time, which keeps the poll to one request per transaction.
pub fn poll_once(
    core: &CoreRest,
    index: &RwLock<MempoolIndex>,
    recent_cap: usize,
    workers: usize,
) -> anyhow::Result<(usize, usize)> {
    let contents = core.mempool_contents()?;
    let entries = contents
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("mempool contents is not an object"))?;

    let known: BTreeSet<Txid> = index
        .read()
        .map_err(|_| anyhow::anyhow!("mempool lock poisoned"))?
        .txs
        .keys()
        .copied()
        .collect();

    let mut current = BTreeSet::new();
    let mut wanted = Vec::new();
    for (txid, entry) in entries {
        let Ok(txid) = txid.parse::<Txid>() else {
            continue;
        };
        current.insert(txid);
        if !known.contains(&txid) {
            wanted.push((txid, parse_entry(entry)));
        }
    }
    let removed: Vec<Txid> = known.difference(&current).copied().collect();

    let workers = workers.max(1).min(wanted.len().max(1));
    let chunk = wanted.len().div_ceil(workers).max(1);
    let fetched: Vec<(MempoolTx, MempoolEntryInfo)> = std::thread::scope(|scope| {
        let handles: Vec<_> = wanted
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    let mut out = Vec::with_capacity(slice.len());
                    for (txid, info) in slice {
                        // a transaction can leave the mempool mid-poll
                        let Ok(raw) = core.tx_raw(txid) else {
                            continue;
                        };
                        match build_tx(raw, info) {
                            Ok(tx) if tx.txid == *txid => out.push((tx, info.clone())),
                            Ok(tx) => log::warn!("mempool tx {txid} decoded as {}", tx.txid),
                            Err(err) => log::warn!("mempool tx {txid} failed to decode: {err}"),
                        }
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .flatten()
            .collect()
    });

    let mut arrivals = fetched;
    arrivals.sort_by_key(|(_, info)| info.time);

    let mut index = index
        .write()
        .map_err(|_| anyhow::anyhow!("mempool lock poisoned"))?;
    for txid in &removed {
        index.remove(txid);
    }
    let added = arrivals.len();
    for (tx, _) in arrivals {
        let entry = RecentTx {
            txid: tx.txid.to_string(),
            fee: tx.fee,
            vsize: tx.vsize,
            #[cfg(not(feature = "liquid"))]
            value: tx.value(),
        };
        index.insert(tx);
        index.push_recent(entry, recent_cap);
    }
    Ok((added, removed.len()))
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
            time: 0,
            touches: [sh].into_iter().collect(),
            spends: BTreeSet::new(),
            outputs: BTreeMap::new(),
        });
        assert_eq!(index.entries(sh).len(), 1);
        assert!(index.remove(&txid).is_some());
        assert!(index.entries(sh).is_empty());
    }

    /// A script's mempool history holds what pays it and what spends its
    /// outputs, confirmed or not; the poller records only the payments.
    #[test]
    fn script_txids_include_spends_of_confirmed_and_unconfirmed_outputs() {
        let sh = ElectrumScripthash::parse(&"22".repeat(32)).unwrap();
        let other = ElectrumScripthash::parse(&"33".repeat(32)).unwrap();
        let txid = |name: &[u8]| Txid::from_raw_hash(sha256d::Hash::hash(name));
        let tx = |id: Txid, spends: &[OutPoint], pays: &[ElectrumScripthash]| MempoolTx {
            txid: id,
            raw: Vec::new(),
            fee: 100,
            vsize: 50,
            ancestor_fees: 100,
            ancestor_vsize: 50,
            time: 0,
            touches: pays.iter().copied().collect(),
            spends: spends.iter().copied().collect(),
            outputs: pays
                .iter()
                .enumerate()
                .map(|(vout, sh)| (vout as u32, (*sh, 1_000)))
                .collect(),
        };
        let confirmed = OutPoint {
            txid: txid(b"confirmed funding"),
            vout: 0,
        };
        let (pays_it, spends_confirmed, spends_unconfirmed, unrelated) = (
            txid(b"pays"),
            txid(b"spends confirmed"),
            txid(b"spends unconfirmed"),
            txid(b"unrelated"),
        );
        let mut index = MempoolIndex::default();
        index.insert(tx(pays_it, &[], &[sh]));
        index.insert(tx(spends_confirmed, &[confirmed], &[other]));
        index.insert(tx(
            spends_unconfirmed,
            &[OutPoint {
                txid: pays_it,
                vout: 0,
            }],
            &[other],
        ));
        index.insert(tx(unrelated, &[], &[other]));

        let expected: BTreeSet<Txid> = [pays_it, spends_confirmed, spends_unconfirmed]
            .into_iter()
            .collect();
        assert_eq!(index.script_txids(sh, &[confirmed]), expected);
        // without the confirmed outputs, their spenders cannot be found
        assert_eq!(
            index.script_txids(sh, &[]),
            [pays_it, spends_unconfirmed].into_iter().collect()
        );
    }

    /// Canonical order and heights, the balance delta, and which outputs are
    /// still unspent, for a confirmed output spent by a mempool transaction
    /// whose own output is spent again by a chained one.
    #[test]
    fn script_view_orders_values_and_skips_mined() {
        let sh = ElectrumScripthash::parse(&"22".repeat(32)).unwrap();
        let other = ElectrumScripthash::parse(&"33".repeat(32)).unwrap();
        let txid = |name: &[u8]| Txid::from_raw_hash(sha256d::Hash::hash(name));
        let tx = |id: Txid, spends: &[OutPoint], pays: &[(ElectrumScripthash, u64)]| MempoolTx {
            txid: id,
            raw: Vec::new(),
            fee: 100,
            vsize: 50,
            ancestor_fees: 100,
            ancestor_vsize: 50,
            time: 0,
            touches: pays.iter().map(|(sh, _)| *sh).collect(),
            spends: spends.iter().copied().collect(),
            outputs: pays
                .iter()
                .enumerate()
                .map(|(vout, pay)| (vout as u32, *pay))
                .collect(),
        };
        let confirmed = OutPoint {
            txid: txid(b"confirmed funding"),
            vout: 0,
        };
        let (spend, chained, incoming) = (txid(b"spend"), txid(b"chained"), txid(b"incoming"));
        let mut index = MempoolIndex::default();
        // spends the confirmed 10_000 output: 6_000 back to the script
        index.insert(tx(spend, &[confirmed], &[(sh, 6_000), (other, 3_000)]));
        // spends that change: height -1
        index.insert(tx(
            chained,
            &[OutPoint {
                txid: spend,
                vout: 0,
            }],
            &[(other, 5_000)],
        ));
        index.insert(tx(incoming, &[], &[(sh, 2_000)]));

        let view = index.script_view(sh, &[(confirmed, 10_000)], &HashSet::new());
        let mut height_zero = [spend, incoming].map(|txid| txid.to_string());
        height_zero.sort();
        assert_eq!(
            view.entries
                .iter()
                .map(|e| (e.tx_hash.clone(), e.height))
                .collect::<Vec<_>>(),
            vec![
                (height_zero[0].clone(), 0),
                (height_zero[1].clone(), 0),
                (chained.to_string(), -1),
            ]
        );
        // +6_000 +2_000 paid, -10_000 confirmed and -6_000 unconfirmed spent
        assert_eq!(view.balance_delta, -8_000);
        assert_eq!(
            view.utxos,
            vec![(
                OutPoint {
                    txid: incoming,
                    vout: 0
                },
                2_000
            )]
        );
        assert_eq!(view.spent_confirmed, [confirmed].into_iter().collect());

        // once the index has `spend` as mined, it leaves the view; `chained`
        // then has only a confirmed parent
        let mined = [spend].into_iter().collect();
        let view = index.script_view(
            sh,
            &[(
                OutPoint {
                    txid: spend,
                    vout: 0,
                },
                6_000,
            )],
            &mined,
        );
        assert!(view
            .entries
            .iter()
            .all(|entry| entry.tx_hash != spend.to_string()));
        assert!(view
            .entries
            .iter()
            .any(|entry| entry.tx_hash == chained.to_string() && entry.height == 0));
        assert_eq!(view.balance_delta, 2_000 - 6_000);
    }

    #[test]
    fn verbose_entries_convert_to_satoshis() {
        let entry = serde_json::json!({
            "vsize": 141,
            "time": 1700000000u64,
            "ancestorsize": 141,
            "fees": {"base": 0.00000282, "ancestor": 0.00000282},
        });
        let info = parse_entry(&entry);
        assert_eq!(info.fee, 282);
        assert_eq!(info.vsize, 141);
        assert_eq!(info.ancestor_fees, 282);
        assert_eq!(info.time, 1_700_000_000);
    }

    #[test]
    fn recent_queue_is_newest_first_and_bounded() {
        let mut index = MempoolIndex::default();
        for i in 0..5u8 {
            index.push_recent(
                RecentTx {
                    txid: i.to_string(),
                    fee: i as u64,
                    vsize: 1,
                    #[cfg(not(feature = "liquid"))]
                    value: 1,
                },
                3,
            );
        }
        let recent = index.recent();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].txid, "4");
        assert_eq!(recent[2].txid, "2");
    }

    #[test]
    fn fee_histogram_bins_descend_by_rate() {
        let mut index = MempoolIndex::default();
        for (i, (fee, vsize)) in [(10_000u64, 1_000u64), (100, 1_000), (1_000, 60_000)]
            .into_iter()
            .enumerate()
        {
            index.insert(MempoolTx {
                txid: Txid::from_raw_hash(sha256d::Hash::hash(&[i as u8])),
                raw: vec![],
                fee,
                vsize,
                ancestor_fees: fee,
                ancestor_vsize: vsize,
                time: 0,
                touches: BTreeSet::new(),
                spends: BTreeSet::new(),
                outputs: BTreeMap::new(),
            });
        }
        let bins = index.fee_histogram_bins();
        assert!(!bins.is_empty());
        assert!(bins.windows(2).all(|pair| pair[0].0 >= pair[1].0), "{bins:?}");
        let total: u64 = bins.iter().map(|(_, vsize)| *vsize as u64).sum();
        assert_eq!(total, 62_000);
    }
}
