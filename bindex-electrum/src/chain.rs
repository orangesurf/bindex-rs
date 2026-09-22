use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use bindex::fmt;
use bitcoin::{OutPoint, Txid};
use serde::Serialize;

use crate::{config::Config, merkle, protocol::ElectrumScripthash};

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
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct ConfirmedTx {
    pub tx_hash: String,
    pub height: usize,
    pub position: u32,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfirmedUtxo {
    pub tx_hash: String,
    pub tx_pos: u32,
    pub height: i64,
    pub value: u64,
    pub script_pubkey: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ConfirmedScripthash {
    pub history: Vec<ConfirmedTx>,
    pub utxos: Vec<ConfirmedUtxo>,
}

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
    refresh_interval: Duration,
    last_refresh: Arc<RwLock<Instant>>,
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
            refresh_interval: config.secondary_refresh_interval(),
            last_refresh: Arc::new(RwLock::new(Instant::now())),
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

    pub fn confirmed_history(
        &self,
        scripthash: ElectrumScripthash,
    ) -> Result<Vec<ConfirmedTx>, Error> {
        Ok(self.confirmed_scripthash(scripthash)?.history)
    }

    pub fn confirmed_scripthash(
        &self,
        scripthash: ElectrumScripthash,
    ) -> Result<ConfirmedScripthash, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let bindex_hash = scripthash
            .to_bindex()
            .map_err(|_| Error::InvalidScripthash)?;
        let mut locations = chain
            .locations_by_scripthash(&bindex_hash, None)?
            .collect::<Vec<_>>();
        locations.sort();
        locations.dedup();

        let mut rows = Vec::new();
        let mut outputs = BTreeMap::<OutPoint, ConfirmedUtxo>::new();
        let mut spent = BTreeSet::<OutPoint>::new();

        for location in locations {
            let raw = chain.get_tx_bytes(&location)?;
            let tx = fmt::parse_tx(&raw)?;
            let txid = tx.txid;
            let mut touches = false;

            for (vout, output) in tx.outputs.iter().enumerate() {
                if ElectrumScripthash::from_script(bitcoin::Script::from_bytes(&output.script_pubkey)) == scripthash {
                    touches = true;
                    outputs.insert(
                        OutPoint {
                            txid,
                            vout: vout as u32,
                        },
                        ConfirmedUtxo {
                            tx_hash: txid.to_string(),
                            tx_pos: vout as u32,
                            height: location.block_height() as i64,
                            value: output.value,
                            script_pubkey: output.script_pubkey.clone(),
                        },
                    );
                }
            }

            for prevout in &tx.inputs {
                if self.prevout_matches_scripthash(&chain, *prevout, scripthash)? {
                    touches = true;
                    spent.insert(*prevout);
                }
            }

            if touches {
                rows.push(ConfirmedTx {
                    tx_hash: txid.to_string(),
                    height: location.block_height(),
                    position: location.block_position(),
                    raw,
                });
            }
        }
        rows.sort_by_key(|row| (row.height, row.position));
        rows.dedup_by_key(|row| row.tx_hash.clone());

        let utxos = outputs
            .into_iter()
            .filter_map(|(outpoint, utxo)| (!spent.contains(&outpoint)).then_some(utxo))
            .collect();

        Ok(ConfirmedScripthash {
            history: rows,
            utxos,
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
    pub fn script_history(
        &self,
        scripthash: ElectrumScripthash,
    ) -> Result<ScriptHistory, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let bindex_hash = scripthash
            .to_bindex()
            .map_err(|_| Error::InvalidScripthash)?;
        let mut locations = chain
            .locations_by_scripthash(&bindex_hash, None)?
            .collect::<Vec<_>>();
        locations.sort();
        locations.dedup();

        let mut history = ScriptHistory::default();
        // every output ever paid to this script, for valuing the spends
        let mut funded = BTreeMap::<OutPoint, u64>::new();
        let mut live = BTreeMap::<OutPoint, ScriptUtxo>::new();

        for location in locations {
            let raw = chain.get_tx_bytes(&location)?;
            let tx = fmt::parse_tx(&raw)?;
            let txid = tx.txid;
            let mut row = ScriptHistoryRow {
                txid,
                height: location.block_height(),
                position: location.block_position(),
                funded: 0,
                funded_count: 0,
                spent: 0,
                spent_count: 0,
            };

            for prevout in &tx.inputs {
                if let Some(value) = funded.get(prevout) {
                    row.spent += value;
                    row.spent_count += 1;
                    live.remove(prevout);
                }
            }
            for (vout, output) in tx.outputs.iter().enumerate() {
                if ElectrumScripthash::from_script(bitcoin::Script::from_bytes(
                    &output.script_pubkey,
                )) != scripthash
                {
                    continue;
                }
                let outpoint = OutPoint {
                    txid,
                    vout: vout as u32,
                };
                row.funded += output.value;
                row.funded_count += 1;
                funded.insert(outpoint, output.value);
                live.insert(
                    outpoint,
                    ScriptUtxo {
                        outpoint,
                        value: output.value,
                        height: location.block_height(),
                        position: location.block_position(),
                    },
                );
            }

            history.peak_live_utxos = history.peak_live_utxos.max(live.len());
            // an index prefix collision touches neither side
            if row.funded_count > 0 || row.spent_count > 0 {
                history.funded_txo_count += row.funded_count;
                history.funded_txo_sum += row.funded;
                history.spent_txo_count += row.spent_count;
                history.spent_txo_sum += row.spent;
                history.rows.push(row);
            }
        }

        history.rows.sort_by_key(|row| (row.height, row.position));
        history.utxos = live.into_values().collect();
        Ok(history)
    }

    /// The confirmed transaction spending `outpoint`, if there is one.
    ///
    /// Scans the funding script's index rows from the funding block onwards:
    /// the spender is indexed under the same scripthash (blocks index their
    /// spent outputs), so nothing before the funding block can match.
    pub fn find_spender(
        &self,
        outpoint: OutPoint,
        scripthash: ElectrumScripthash,
        funding_height: usize,
    ) -> Result<Option<Spender>, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let bindex_hash = scripthash
            .to_bindex()
            .map_err(|_| Error::InvalidScripthash)?;
        let previous = funding_height
            .checked_sub(1)
            .and_then(|height| chain.indexed_header_at_height(height));
        let mut locations = chain
            .locations_by_scripthash(&bindex_hash, previous)?
            .collect::<Vec<_>>();
        locations.sort();
        locations.dedup();

        for location in locations {
            let raw = chain.get_tx_bytes(&location)?;
            let tx = fmt::parse_tx(&raw)?;
            if tx.txid == outpoint.txid {
                continue;
            }
            let Some(vin) = tx.inputs.iter().position(|input| *input == outpoint) else {
                continue;
            };
            return Ok(Some(Spender {
                txid: tx.txid,
                vin: vin as u32,
                height: location.block_height(),
                position: location.block_position(),
                block_hash: location.block_hash(),
            }));
        }
        Ok(None)
    }

    fn prevout_matches_scripthash(
        &self,
        chain: &bindex::IndexedChain,
        outpoint: OutPoint,
        scripthash: ElectrumScripthash,
    ) -> Result<bool, Error> {
        for location in chain.locations_by_txid(&outpoint.txid)? {
            let raw = chain.get_tx_bytes(&location)?;
            let tx = fmt::parse_tx(&raw)?;
            if tx.txid != outpoint.txid {
                continue;
            }
            let Some(output) = tx.outputs.get(outpoint.vout as usize) else {
                return Ok(false);
            };
            return Ok(
                ElectrumScripthash::from_script(bitcoin::Script::from_bytes(&output.script_pubkey)) == scripthash,
            );
        }
        Ok(false)
    }
}
