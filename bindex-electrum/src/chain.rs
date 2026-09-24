use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use bindex::fmt;
use bitcoin::{OutPoint, Txid};
use serde::Serialize;

use crate::{
    config::Config, corerest::CoreRest, deadline::Deadline, merkle,
    protocol::ElectrumScripthash,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bindex error: {0}")]
    Bindex(#[from] bindex::ChainError),

    #[error("chain lock poisoned")]
    Lock,

    #[error("block height not found: {0}")]
    Height(usize),

    #[error("transaction decode failed: {0}")]
    Decode(#[from] fmt::Error),

    #[error("invalid scripthash")]
    InvalidScripthash,

    #[error("bitcoind REST failed: {0}")]
    Rest(#[from] crate::corerest::Error),

    #[error("request deadline exceeded")]
    Deadline,

    #[error("the chain moved under the query; try again")]
    Reorg,

    #[error("body fetch worker panicked")]
    Worker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeaderNotification {
    pub height: usize,
    pub hex: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockHeaders {
    pub count: usize,
    pub hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocatedTx {
    pub raw: Vec<u8>,
    pub height: usize,
    pub block_hash: String,
    pub position: u32,
    pub confirmations: usize,
}

/// Bodies fetched (and deadline-checked) per round.
const FETCH_BATCH: usize = 512;
/// Threads used for those fetches.
const FETCH_WORKERS: usize = 8;
/// How often a query is redone when the chain moves under it.
const REORG_RETRIES: usize = 3;

/// One transaction in a script's confirmed history, with what it moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptHistoryRow {
    pub txid: Txid,
    pub height: usize,
    pub position: u32,
    pub funded: u64,
    pub funded_count: usize,
    pub spent: u64,
    pub spent_count: usize,
}

impl ScriptHistoryRow {
    /// Net effect on the script's balance (the `value` of a history summary).
    pub fn net_value(&self) -> i64 {
        self.funded as i64 - self.spent as i64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptUtxo {
    pub outpoint: OutPoint,
    pub value: u64,
    pub height: usize,
    pub position: u32,
}

/// The full confirmed history of one script, folded in chain order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptHistory {
    /// Ascending by `(height, position)`.
    pub rows: Vec<ScriptHistoryRow>,
    /// Confirmed and still unspent, ascending by outpoint.
    pub utxos: Vec<ScriptUtxo>,
    pub funded_txo_count: usize,
    pub funded_txo_sum: u64,
    pub spent_txo_count: usize,
    pub spent_txo_sum: u64,
    /// Largest the live UTXO set ever got, which is what the REST utxo cap
    /// applies to.
    pub peak_live_utxos: usize,
}

impl ScriptHistory {
    pub fn tx_count(&self) -> usize {
        self.rows.len()
    }
}

/// Where a confirmed transaction spending a given outpoint sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spender {
    pub txid: Txid,
    pub vin: u32,
    pub height: usize,
    pub position: u32,
    pub block_hash: bitcoin::BlockHash,
}

#[derive(Clone)]
pub struct ChainAdapter {
    chain: Arc<RwLock<bindex::IndexedChain>>,
    /// Used to fetch transaction bodies without holding the chain lock.
    core: CoreRest,
    refresh_interval: Duration,
    last_refresh: Arc<RwLock<Instant>>,
    scripts: Arc<Mutex<ScriptCache>>,
}

impl ChainAdapter {
    pub fn open(config: &Config) -> Result<Self, Error> {
        let secondary_path = config.secondary_path();
        let chain = bindex::IndexedChain::open_secondary_named(
            &config.bindex_db_path,
            &config.db_name(),
            config.bitcoind_rest_url.clone(),
            secondary_path,
        )?;
        log::info!("chain format: {}", fmt::NAME);
        Ok(Self {
            chain: Arc::new(RwLock::new(chain)),
            core: CoreRest::new(config.bitcoind_rest_url.clone()),
            refresh_interval: config.secondary_refresh_interval(),
            last_refresh: Arc::new(RwLock::new(Instant::now())),
            scripts: Arc::new(Mutex::new(ScriptCache::new(config.script_cache_refs))),
        })
    }

    pub fn sync(&self, limit: usize) -> Result<bindex::Stats, Error> {
        self.chain
            .write()
            .map_err(|_| Error::Lock)?
            .sync(limit)
            .map_err(Error::Bindex)
    }

    pub fn refresh(&self) -> Result<Duration, Error> {
        let started = Instant::now();
        self.chain
            .write()
            .map_err(|_| Error::Lock)?
            .refresh_secondary()
            .map_err(Error::Bindex)?;
        *self.last_refresh.write().map_err(|_| Error::Lock)? = Instant::now();
        Ok(started.elapsed())
    }

    pub fn refresh_interval(&self) -> Duration {
        self.refresh_interval
    }

    pub fn seconds_since_refresh(&self) -> Result<u64, Error> {
        Ok(self
            .last_refresh
            .read()
            .map_err(|_| Error::Lock)?
            .elapsed()
            .as_secs())
    }

    pub fn tip(&self) -> Result<Option<HeaderNotification>, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let Some(height) = chain.headers().tip_height() else {
            return Ok(None);
        };
        let header = chain
            .block_header_raw_at_height(height)?
            .ok_or(Error::Height(height))?;
        Ok(Some(HeaderNotification {
            height,
            hex: hex::encode(header),
        }))
    }

    pub fn genesis_hash(&self) -> Result<Option<bitcoin::BlockHash>, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        Ok(chain.block_hash_at_height(0))
    }

    pub fn block_header_hex(&self, height: usize) -> Result<String, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let header = chain
            .block_header_raw_at_height(height)?
            .ok_or(Error::Height(height))?;
        Ok(hex::encode(header))
    }

    pub fn block_headers(
        &self,
        start_height: usize,
        count: usize,
        cp_height: usize,
    ) -> Result<BlockHeaders, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let tip_height = chain.headers().tip_height().unwrap_or(0);
        if start_height > tip_height {
            return Ok(BlockHeaders {
                count: 0,
                hex: String::new(),
                max: Some(2016),
                branch: None,
                root: None,
            });
        }
        let count = count.min(2016).min(tip_height - start_height + 1);
        let headers = chain.block_headers_raw(start_height, count)?;
        if headers.len() != count {
            return Err(Error::Height(start_height + headers.len()));
        }
        let bytes = headers.concat();

        let (branch, root) = if cp_height > 0 {
            let hashes = (0..=cp_height.min(tip_height))
                .map(|height| {
                    chain
                        .block_hash_at_height(height)
                        .ok_or(Error::Height(height))
                        .map(|hash| hash.to_raw_hash())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let proof = merkle::branch_and_root(&hashes, start_height.min(hashes.len() - 1));
            (
                Some(
                    proof
                        .branch
                        .into_iter()
                        .map(|hash| hash.to_string())
                        .collect(),
                ),
                Some(proof.root.to_string()),
            )
        } else {
            (None, None)
        };

        Ok(BlockHeaders {
            count,
            hex: hex::encode(bytes),
            max: Some(2016),
            branch,
            root,
        })
    }

    pub fn transaction_by_txid(&self, txid: &bitcoin::Txid) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.located_transaction_by_txid(txid)?.map(|t| t.raw))
    }

    /// Raw bytes plus where the transaction sits in the chain.
    pub fn located_transaction_by_txid(
        &self,
        txid: &bitcoin::Txid,
    ) -> Result<Option<LocatedTx>, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let tip = chain.headers().tip_height().unwrap_or(0);
        for location in chain.locations_by_txid(txid)? {
            let raw = chain.get_tx_bytes(&location)?;
            if fmt::txid(&raw)? == *txid {
                return Ok(Some(LocatedTx {
                    raw,
                    height: location.block_height(),
                    block_hash: location.block_hash().to_string(),
                    position: location.block_position(),
                    confirmations: tip.saturating_sub(location.block_height()) + 1,
                }));
            }
        }
        Ok(None)
    }

    pub fn block_txids(&self, height: usize) -> Result<Vec<Txid>, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        chain
            .block_txids_at_height(height)?
            .ok_or(Error::Height(height))
    }

    pub fn tip_height(&self) -> Result<Option<usize>, Error> {
        Ok(self
            .chain
            .read()
            .map_err(|_| Error::Lock)?
            .headers()
            .tip_height())
    }

    pub fn hash_at_height(&self, height: usize) -> Result<Option<bitcoin::BlockHash>, Error> {
        Ok(self
            .chain
            .read()
            .map_err(|_| Error::Lock)?
            .block_hash_at_height(height))
    }

    /// Raw header bytes of an active-chain block, in either format.
    pub fn header_raw_at_height(&self, height: usize) -> Result<Option<Vec<u8>>, Error> {
        self.chain
            .read()
            .map_err(|_| Error::Lock)?
            .block_header_raw_at_height(height)
            .map_err(Error::Bindex)
    }

    /// Decoded header of an active-chain block (Bitcoin format only).
    #[cfg(not(feature = "liquid"))]
    pub fn header_at_height(&self, height: usize) -> Result<Option<bitcoin::block::Header>, Error> {
        self.chain
            .read()
            .map_err(|_| Error::Lock)?
            .block_header_at_height(height)
            .map_err(Error::Bindex)
    }

    /// Fold a script's whole confirmed history: which transactions touched it,
    /// how much each moved, what is still unspent, and how large the live set
    /// ever got.
    ///
    /// The index yields every transaction that funds *or* spends the script (it
    /// indexes the spent outputs of each block too), in chain order, so a spend
    /// is always seen after the output it spends. That makes one pass enough:
    /// outputs paying the script are remembered as they appear, and an input is
    /// ours exactly when it spends one of them — no prevout lookups, unlike the
    /// per-input probing `confirmed_scripthash` does.
    ///
    /// The chain lock is held only for the index lookups. Bodies are fetched
    /// from the node with the lock released — a reused address can need tens of
    /// thousands of them, and holding a read lock for that long queues the
    /// secondary refresh (which needs the write lock) and every other reader
    /// behind it. The tip is re-read afterwards: if the chain moved under the
    /// fetch the whole query is redone rather than folded from a mixed view.
    ///
    /// Folds are cached. When the index still lists the same transactions for
    /// the script, the cached history is returned after one index scan; when it
    /// lists the cached ones plus some at the end (new blocks), only those are
    /// fetched and folded on. Anything else, such as a reorg replacing a block
    /// the script had a transaction in, refolds from scratch.
    pub fn script_history(
        &self,
        scripthash: ElectrumScripthash,
        deadline: &Deadline,
    ) -> Result<ScriptHistory, Error> {
        for _ in 0..REORG_RETRIES {
            let (tip, refs) = self.script_refs(scripthash, None)?;
            let cached = self
                .scripts
                .lock()
                .map_err(|_| Error::Lock)?
                .get(scripthash);
            let (mut fold, start) = match cached {
                Some(cached) if refs.starts_with(&cached.refs) => {
                    if refs.len() == cached.refs.len() {
                        return Ok(cached.history.clone());
                    }
                    (cached.fold.clone(), cached.refs.len())
                }
                _ => (Fold::default(), 0),
            };
            let tail = &refs[start..];
            let mut bodies = Vec::with_capacity(tail.len());
            for batch in tail.chunks(FETCH_BATCH) {
                bodies.extend(self.fetch_bodies(batch, deadline)?);
            }
            if self.tip_hash()? != tip {
                continue;
            }
            for (reference, raw) in tail.iter().zip(&bodies) {
                fold.apply(scripthash, reference, raw)?;
            }
            let history = fold.finish();
            self.scripts.lock().map_err(|_| Error::Lock)?.put(
                scripthash,
                CachedScript {
                    refs,
                    fold,
                    history: history.clone(),
                },
            );
            return Ok(history);
        }
        Err(Error::Reorg)
    }

    /// The confirmed transaction spending `outpoint`, if there is one.
    ///
    /// Scans the funding script's index rows from the funding block onwards:
    /// the spender is indexed under the same scripthash (blocks index their
    /// spent outputs), so nothing before the funding block can match. Bodies
    /// are fetched a batch at a time outside the lock, stopping at the first
    /// match, and the deadline is checked between batches.
    pub fn find_spender(
        &self,
        outpoint: OutPoint,
        scripthash: ElectrumScripthash,
        funding_height: usize,
        deadline: &Deadline,
    ) -> Result<Option<Spender>, Error> {
        for _ in 0..REORG_RETRIES {
            let (tip, refs) = self.script_refs(scripthash, Some(funding_height))?;
            let mut found = None;
            'scan: for batch in refs.chunks(FETCH_BATCH) {
                let bodies = self.fetch_bodies(batch, deadline)?;
                for (reference, raw) in batch.iter().zip(bodies) {
                    let tx = fmt::parse_tx(&raw)?;
                    if tx.txid == outpoint.txid {
                        continue;
                    }
                    if let Some(vin) = tx.inputs.iter().position(|input| *input == outpoint) {
                        found = Some(Spender {
                            txid: tx.txid,
                            vin: vin as u32,
                            height: reference.block_height,
                            position: reference.block_position,
                            block_hash: reference.block_hash,
                        });
                        break 'scan;
                    }
                }
            }
            if self.tip_hash()? != tip {
                continue;
            }
            return Ok(found);
        }
        Err(Error::Reorg)
    }

    fn tip_hash(&self) -> Result<bitcoin::BlockHash, Error> {
        Ok(self
            .chain
            .read()
            .map_err(|_| Error::Lock)?
            .headers()
            .tip_hash())
    }

    /// Every index row for a scripthash, resolved to a byte range. Pure index
    /// work: this is the only part that holds the chain lock.
    fn script_refs(
        &self,
        scripthash: ElectrumScripthash,
        from_height: Option<usize>,
    ) -> Result<(bitcoin::BlockHash, Vec<bindex::TxBytesRef>), Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let bindex_hash = scripthash
            .to_bindex()
            .map_err(|_| Error::InvalidScripthash)?;
        let previous = from_height
            .and_then(|height| height.checked_sub(1))
            .and_then(|height| chain.indexed_header_at_height(height));
        let mut locations = chain
            .locations_by_scripthash(&bindex_hash, previous)?
            .collect::<Vec<_>>();
        locations.sort();
        locations.dedup();
        let refs = locations
            .iter()
            .map(|location| chain.tx_bytes_ref(location))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((chain.headers().tip_hash(), refs))
    }

    /// Fetch transaction bodies from the node, in order, with no lock held.
    fn fetch_bodies(
        &self,
        refs: &[bindex::TxBytesRef],
        deadline: &Deadline,
    ) -> Result<Vec<Vec<u8>>, Error> {
        if deadline.expired() {
            return Err(Error::Deadline);
        }
        if refs.len() < 2 {
            return refs.iter().map(|r| self.fetch_body(r)).collect();
        }
        let workers = FETCH_WORKERS.min(refs.len());
        let chunk = refs.len().div_ceil(workers);
        let batches: Vec<Result<Vec<Vec<u8>>, Error>> = std::thread::scope(|scope| {
            let handles: Vec<_> = refs
                .chunks(chunk)
                .map(|slice| {
                    scope.spawn(move || {
                        slice
                            .iter()
                            .map(|r| self.fetch_body(r))
                            .collect::<Result<Vec<_>, Error>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or(Err(Error::Worker)))
                .collect()
        });
        let mut out = Vec::with_capacity(refs.len());
        for batch in batches {
            out.extend(batch?);
        }
        Ok(out)
    }

    fn fetch_body(&self, reference: &bindex::TxBytesRef) -> Result<Vec<u8>, Error> {
        Ok(self
            .core
            .block_part(&reference.block_hash, reference.offset, reference.size)?)
    }

}

/// A script's history folded up to some transaction, kept whole so that a
/// longer history (the same transactions plus new blocks) can be folded on from
/// where it stopped. No locks, no IO.
#[derive(Debug, Clone, Default)]
struct Fold {
    /// Rows and counters so far; `utxos` is filled in by [`Fold::finish`].
    history: ScriptHistory,
    /// Every output ever paid to the script, for valuing its spends.
    funded: BTreeMap<OutPoint, u64>,
    live: BTreeMap<OutPoint, ScriptUtxo>,
}

impl Fold {
    /// Fold in the next transaction, in chain order.
    fn apply(
        &mut self,
        scripthash: ElectrumScripthash,
        reference: &bindex::TxBytesRef,
        raw: &[u8],
    ) -> Result<(), Error> {
        let tx = fmt::parse_tx(raw)?;
        let txid = tx.txid;
        let mut row = ScriptHistoryRow {
            txid,
            height: reference.block_height,
            position: reference.block_position,
            funded: 0,
            funded_count: 0,
            spent: 0,
            spent_count: 0,
        };

        for prevout in &tx.inputs {
            if let Some(value) = self.funded.get(prevout) {
                row.spent += value;
                row.spent_count += 1;
                self.live.remove(prevout);
            }
        }
        for (vout, output) in tx.outputs.iter().enumerate() {
            if ElectrumScripthash::from_script(bitcoin::Script::from_bytes(&output.script_pubkey))
                != scripthash
            {
                continue;
            }
            let outpoint = OutPoint {
                txid,
                vout: vout as u32,
            };
            row.funded += output.value;
            row.funded_count += 1;
            self.funded.insert(outpoint, output.value);
            self.live.insert(
                outpoint,
                ScriptUtxo {
                    outpoint,
                    value: output.value,
                    height: reference.block_height,
                    position: reference.block_position,
                },
            );
        }

        self.history.peak_live_utxos = self.history.peak_live_utxos.max(self.live.len());
        // an index prefix collision touches neither side
        if row.funded_count > 0 || row.spent_count > 0 {
            self.history.funded_txo_count += row.funded_count;
            self.history.funded_txo_sum += row.funded;
            self.history.spent_txo_count += row.spent_count;
            self.history.spent_txo_sum += row.spent;
            self.history.rows.push(row);
        }
        Ok(())
    }

    fn finish(&self) -> ScriptHistory {
        let mut history = self.history.clone();
        history.rows.sort_by_key(|row| (row.height, row.position));
        history.utxos = self.live.values().cloned().collect();
        history
    }
}

/// A script's fold and the index references it was built from.
struct CachedScript {
    refs: Vec<bindex::TxBytesRef>,
    fold: Fold,
    history: ScriptHistory,
}

/// Recently folded scripts, least recently used evicted first. The bound is
/// on index references, which is what a fold's memory grows with.
struct ScriptCache {
    entries: HashMap<ElectrumScripthash, (Arc<CachedScript>, u64)>,
    refs: usize,
    max_refs: usize,
    clock: u64,
}

impl ScriptCache {
    fn new(max_refs: usize) -> Self {
        Self {
            entries: HashMap::new(),
            refs: 0,
            max_refs,
            clock: 0,
        }
    }

    fn get(&mut self, scripthash: ElectrumScripthash) -> Option<Arc<CachedScript>> {
        self.clock += 1;
        let (cached, used) = self.entries.get_mut(&scripthash)?;
        *used = self.clock;
        Some(Arc::clone(cached))
    }

    fn put(&mut self, scripthash: ElectrumScripthash, cached: CachedScript) {
        if let Some((old, _)) = self.entries.remove(&scripthash) {
            self.refs -= old.refs.len();
        }
        // a script larger than the whole cache is not worth evicting everything for
        if cached.refs.len() > self.max_refs {
            return;
        }
        self.clock += 1;
        self.refs += cached.refs.len();
        self.entries
            .insert(scripthash, (Arc::new(cached), self.clock));
        while self.refs > self.max_refs {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(scripthash, _)| *scripthash)
            else {
                break;
            };
            if let Some((evicted, _)) = self.entries.remove(&oldest) {
                self.refs -= evicted.refs.len();
            }
        }
    }
}

#[cfg(test)]
mod script_cache_tests {
    use super::*;
    use bitcoin::hashes::Hash as _;

    fn cached(refs: usize) -> CachedScript {
        let reference = bindex::TxBytesRef {
            block_hash: bitcoin::BlockHash::all_zeros(),
            block_height: 0,
            block_position: 0,
            offset: 0,
            size: 0,
        };
        CachedScript {
            refs: vec![reference; refs],
            fold: Fold::default(),
            history: ScriptHistory::default(),
        }
    }

    fn scripthash(byte: u8) -> ElectrumScripthash {
        ElectrumScripthash([byte; 32])
    }

    #[test]
    fn evicts_least_recently_used_and_skips_oversized() {
        let mut cache = ScriptCache::new(10);
        cache.put(scripthash(1), cached(4));
        cache.put(scripthash(2), cached(4));
        assert!(cache.get(scripthash(1)).is_some()); // 2 is now the oldest
        cache.put(scripthash(3), cached(4));
        assert!(cache.get(scripthash(2)).is_none());
        assert!(cache.get(scripthash(1)).is_some());
        assert!(cache.get(scripthash(3)).is_some());
        assert_eq!(cache.refs, 8);

        // replacing an entry swaps its size rather than adding to it
        cache.put(scripthash(1), cached(6));
        assert_eq!(cache.refs, 10);

        // larger than the whole cache: dropped, and nothing else is evicted
        cache.put(scripthash(4), cached(11));
        assert!(cache.get(scripthash(4)).is_none());
        assert_eq!(cache.refs, 10);
    }
}
