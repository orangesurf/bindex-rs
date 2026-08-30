use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use bitcoin::{
    consensus::{deserialize, serialize},
    OutPoint, Transaction, Txid,
};
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
    Decode(#[from] bitcoin::consensus::encode::Error),

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

#[derive(Clone)]
pub struct ChainAdapter {
    chain: Arc<RwLock<bindex::IndexedChain>>,
    refresh_interval: Duration,
    last_refresh: Arc<RwLock<Instant>>,
}

impl ChainAdapter {
    pub fn open(config: &Config) -> Result<Self, Error> {
        let secondary_path = config
            .bindex_db_path
            .join(format!("{}-electrum-secondary", config.network));
        let chain = bindex::IndexedChain::open_secondary_with_rest_url(
            &config.bindex_db_path,
            config.network,
            config.bitcoind_rest_url.clone(),
            secondary_path,
        )?;
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
            .block_header_at_height(height)
            .ok_or(Error::Height(height))?;
        Ok(Some(HeaderNotification {
            height,
            hex: hex::encode(serialize(header)),
        }))
    }

    pub fn genesis_hash(&self) -> Result<Option<bitcoin::BlockHash>, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        Ok(chain.block_hash_at_height(0))
    }

    pub fn block_header_hex(&self, height: usize) -> Result<String, Error> {
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        let header = chain
            .block_header_at_height(height)
            .ok_or(Error::Height(height))?;
        Ok(hex::encode(serialize(header)))
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
        let mut bytes = Vec::with_capacity(count * bitcoin::block::Header::SIZE);
        for height in start_height..start_height + count {
            let header = chain
                .block_header_at_height(height)
                .ok_or(Error::Height(height))?;
            bytes.extend(serialize(header));
        }

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
            let tx: Transaction = deserialize(&raw)?;
            let txid = tx.compute_txid();
            let mut touches = false;

            for (vout, output) in tx.output.iter().enumerate() {
                if ElectrumScripthash::from_script(output.script_pubkey.as_script()) == scripthash {
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
                            value: output.value.to_sat(),
                            script_pubkey: output.script_pubkey.as_bytes().to_vec(),
                        },
                    );
                }
            }

            for input in &tx.input {
                if input.previous_output.is_null() {
                    continue;
                }
                if self.prevout_matches_scripthash(&chain, input.previous_output, scripthash)? {
                    touches = true;
                    spent.insert(input.previous_output);
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
        let chain = self.chain.read().map_err(|_| Error::Lock)?;
        for location in chain.locations_by_txid(txid)? {
            let raw = chain.get_tx_bytes(&location)?;
            let tx: Transaction = deserialize(&raw)?;
            if tx.compute_txid() == *txid {
                return Ok(Some(raw));
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

    fn prevout_matches_scripthash(
        &self,
        chain: &bindex::IndexedChain,
        outpoint: OutPoint,
        scripthash: ElectrumScripthash,
    ) -> Result<bool, Error> {
        for location in chain.locations_by_txid(&outpoint.txid)? {
            let raw = chain.get_tx_bytes(&location)?;
            let tx: Transaction = deserialize(&raw)?;
            if tx.compute_txid() != outpoint.txid {
                continue;
            }
            let Some(output) = tx.output.get(outpoint.vout as usize) else {
                return Ok(false);
            };
            return Ok(
                ElectrumScripthash::from_script(output.script_pubkey.as_script()) == scripthash,
            );
        }
        Ok(false)
    }
}
