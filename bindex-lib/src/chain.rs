use std::fmt::Debug;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use bitcoin::{hashes::Hash, Network};
use bitcoin::{BlockHash, Txid};
use log::*;

use crate::{client, db, fmt, headers, index, Location, TxBytesRef};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("client failed: {0}")]
    Client(#[from] client::Error),

    #[error("use https://github.com/bitcoin/bitcoin/pull/33657")]
    NotSupported,

    #[error("indexing failed: {0:?}")]
    Index(#[from] index::Error),

    #[error("decoding failed: {0}")]
    Decode(#[from] bitcoin::consensus::encode::Error),

    #[error("RocksDB failed: {0}")]
    RocksDB(#[from] rust_rocksdb::Error),

    #[error("Genesis block hash mismatch: {0} != {1}")]
    ChainMismatch(bitcoin::BlockHash, bitcoin::BlockHash),

    #[error("invalid address: {0}")]
    Address(#[from] bitcoin::address::ParseError),

    #[error("block not found: {0}")]
    BlockNotFound(#[from] headers::Reorg),

    #[error("format error: {0}")]
    Fmt(#[from] fmt::Error),
}

#[derive(Debug)]
pub struct Stats {
    pub tip: bitcoin::BlockHash,
    pub indexed_blocks: usize,
    pub size_read: usize,
    pub elapsed: Duration,
}

impl Stats {
    fn new(tip: bitcoin::BlockHash) -> Self {
        Self {
            tip,
            indexed_blocks: 0,
            size_read: 0,
            elapsed: Duration::ZERO,
        }
    }
}

pub struct IndexedChain {
    genesis_hash: bitcoin::BlockHash,
    headers: headers::Headers,
    client: client::Client,
    store: db::DB,
}

#[derive(Debug)]
pub struct Config {
    pub db_path: PathBuf,
    pub url: String,
    pub secondary_path: Option<PathBuf>,
}

struct HeaderChunk {
    hashes: Vec<BlockHash>,
    to_skip: usize,
}

impl HeaderChunk {
    fn get(&self) -> &[BlockHash] {
        &self.hashes[self.to_skip..]
    }
}

struct PerBlockData {
    blockhash: BlockHash,
    block_bytes: index::BlockBytes,
    spent_bytes: index::SpentBytes,
    txs_count: u32,
}

struct Builder<'a> {
    items: Vec<(u32, index::TxNumRange, &'a PerBlockData)>,
    tip: BlockHash,
}

impl<'a> Builder<'a> {
    fn new(tip: Option<&index::IndexedHeader>, items: &'a [PerBlockData]) -> Self {
        let mut next_txnum = tip.map_or_else(index::TxNum::default, |header| header.next_txnum());
        let mut height = tip.map_or(0, |header| header.height() + 1);
        let tip = tip.map_or_else(bitcoin::BlockHash::all_zeros, |header| header.hash());

        let items = items
            .iter()
            .map(|data| {
                let first_txnum = next_txnum;
                next_txnum.increment_by(data.txs_count);
                let range = index::TxNumRange::new(first_txnum, next_txnum);
                let h = height;
                height += 1;
                (h, range, data)
            })
            .collect();
        Self { items, tip }
    }

    fn build(self) -> Result<Vec<index::Batch>, Error> {
        use rayon::prelude::*;

        for pair in self.items.windows(2) {
            assert!(index::TxNumRange::adjacent(&pair[0].1, &pair[1].1));
            assert_eq!(pair[0].0 + 1, pair[1].0);
        }
        let batches = self
            .items
            .into_par_iter()
            .map(|(height, txnum_range, data)| {
                let PerBlockData {
                    blockhash,
                    block_bytes,
                    spent_bytes,
                    txs_count,
                } = data;
                assert_eq!(txnum_range.len(), *txs_count);
                index::Batch::build(height, txnum_range, *blockhash, block_bytes, spent_bytes)
            })
            .collect::<Result<Vec<_>, index::Error>>()?;
        let mut tip = self.tip;
        for batch in &batches {
            let header = &batch.header;
            assert_eq!(tip, header.prev_blockhash());
            tip = header.hash();
        }
        Ok(batches)
    }
}

impl IndexedChain {
    /// Open an existing DB, or create if missing.
    /// Use binary format REST API for fetching the data from bitcoind.
    /// Allow setting REST server URL (e.g. "http://hostname:8332").
    pub fn open(
        db_dir: impl AsRef<Path>,
        network: Network,
        url: Option<String>,
    ) -> Result<Self, Error> {
        let db_path = db_dir.as_ref().to_path_buf().join(db_name(network));
        let url = url.unwrap_or_else(|| format!("http://localhost:{}", default_rpc_port(network)));
        Self::from_config(Config {
            db_path,
            url,
            secondary_path: None,
        })
    }

    pub fn open_with_rest_url(
        db_dir: impl AsRef<Path>,
        network: Network,
        rest_url: impl Into<String>,
    ) -> Result<Self, Error> {
        let db_path = db_dir.as_ref().to_path_buf().join(db_name(network));
        Self::from_config(Config {
            db_path,
            url: rest_url.into(),
            secondary_path: None,
        })
    }

    pub fn open_secondary_with_rest_url(
        db_dir: impl AsRef<Path>,
        network: Network,
        rest_url: impl Into<String>,
        secondary_path: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let db_path = db_dir.as_ref().to_path_buf().join(db_name(network));
        Self::from_config(Config {
            db_path,
            url: rest_url.into(),
            secondary_path: Some(secondary_path.as_ref().to_path_buf()),
        })
    }

    /// Open (or create) the index under `db_dir/<name>` as the primary writer.
    pub fn open_named(
        db_dir: impl AsRef<Path>,
        name: &str,
        rest_url: impl Into<String>,
    ) -> Result<Self, Error> {
        Self::from_config(Config {
            db_path: db_dir.as_ref().to_path_buf().join(name),
            url: rest_url.into(),
            secondary_path: None,
        })
    }

    /// Open the index under `db_dir/<name>` as a read-only secondary.
    pub fn open_secondary_named(
        db_dir: impl AsRef<Path>,
        name: &str,
        rest_url: impl Into<String>,
        secondary_path: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        Self::from_config(Config {
            db_path: db_dir.as_ref().to_path_buf().join(name),
            url: rest_url.into(),
            secondary_path: Some(secondary_path.as_ref().to_path_buf()),
        })
    }

    pub fn from_config(config: Config) -> Result<Self, Error> {
        info!("index: {:?}", config);
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .max_response_header_size(usize::MAX) // Disabled as a workaround
                // Blocks are fetched in parallel via rayon (one request per worker
                // thread). ureq's default idle pool (per_host=3) is far below the
                // thread count, so most connections are closed instead of reused —
                // on a fast localhost node that churns through the ephemeral port
                // range and fails with EADDRNOTAVAIL. Keep enough idle connections
                // pooled to cover the rayon pool so connections are reused.
                .max_idle_connections(256)
                .max_idle_connections_per_host(256)
                .build(),
        );
        let client = client::Client::new(agent, config.url);
        let genesis_hash = client.get_blockhash_by_height(0)?;
        let genesis_block = client.get_block_bytes(genesis_hash)?;

        // make sure bitcoind supports the required REST API endpoints
        // * /rest/getspenttxouts/ (added in https://github.com/bitcoin/bitcoin/pull/32540)
        match client.get_spent_bytes(genesis_hash) {
            Err(client::Error::Http(ureq::Error::StatusCode(404))) => Err(Error::NotSupported)?,
            res => res?,
        };
        // * /rest/blockpart/ (added in https://github.com/bitcoin/bitcoin/pull/33657)
        let txpos = index::TxBlockPos {
            offset: 0,
            size: genesis_block
                .len()
                .try_into()
                .expect("too large genesis block"),
        };
        match client.get_block_part(genesis_hash, txpos) {
            Err(client::Error::Http(ureq::Error::StatusCode(404))) => Err(Error::NotSupported)?,
            res => assert_eq!(index::BlockBytes::new(res?), genesis_block),
        };

        let store = match config.secondary_path.as_ref() {
            Some(secondary_path) => db::DB::open_as_secondary(&config.db_path, secondary_path)?,
            None => db::DB::open(&config.db_path)?,
        };
        let headers = headers::Headers::new(store.headers()?);
        if let Some(indexed_genesis) = headers.genesis() {
            if indexed_genesis.hash() != genesis_hash {
                return Err(Error::ChainMismatch(indexed_genesis.hash(), genesis_hash));
            }
            info!(
                "block={} height={} headers loaded",
                headers.tip_hash(),
                headers.tip_height().unwrap(),
            );
        }
        Ok(IndexedChain {
            genesis_hash,
            headers,
            client,
            store,
        })
    }

    /// Follow the primary. Usually that only appended blocks, so the rows
    /// from our tip onwards are read and added (a Liquid chain has 4M headers,
    /// and reloading them all took ~0.9 s under the chain's write lock on every
    /// refresh). If our tip row changed or vanished, the primary reorged, and
    /// the whole chain is reloaded.
    pub fn refresh_secondary(&mut self) -> Result<(), Error> {
        self.store.catch_up_with_primary()?;
        if let Some(tip) = self.headers.tip().cloned() {
            let rows = self.store.headers_from(&tip.key(), tip.height() as usize)?;
            let extends_tip = rows.first().map(index::IndexedHeader::hash) == Some(tip.hash())
                && rows
                    .windows(2)
                    .all(|pair| pair[1].prev_blockhash() == pair[0].hash());
            if extends_tip {
                for row in rows.into_iter().skip(1) {
                    self.headers.add(row);
                }
                return Ok(());
            }
        }
        self.headers = headers::Headers::new(self.store.headers()?);
        Ok(())
    }

    fn drop_tip(&mut self) -> Result<bitcoin::BlockHash, Error> {
        let stale = self
            .headers
            .pop()
            .expect("cannot drop tip of an empty chain");
        // "Re-index" stale block in order to delete its entries from the DB
        // let stale_hash = stale.hash();
        // let mut builder: index::IndexBuilder = index::IndexBuilder::new(self.headers.tip());
        // builder.add(stale_hash, &self.fetch_data(stale_hash)?)?;
        self.store.delete(&self.index(&[stale.hash()], None)?)?;
        Ok(stale.hash())
    }

    fn fetch_new_headers(&mut self, limit: usize) -> Result<HeaderChunk, Error> {
        loop {
            // usually the first header is already part of the current chain
            let (mut blockhash, mut to_skip) = (self.headers.tip_hash(), 1);
            if blockhash == bitcoin::BlockHash::all_zeros() {
                // but if the chain is empty, we need also to fetch the genesis header
                (blockhash, to_skip) = (self.genesis_hash, 0)
            }
            let headers = self.client.get_headers(blockhash, limit)?;
            if !headers.is_empty() {
                let hashes = headers.into_iter().map(|h| h.hash).collect();
                return Ok(HeaderChunk { hashes, to_skip });
            }
            warn!(
                "block={} height={} was rolled back",
                blockhash,
                self.headers.tip_height().unwrap(),
            );
            // drop stale tip and retry fetching
            assert_eq!(blockhash, self.drop_tip()?);
        }
    }

    fn fetch_data(&self, blockhash: BlockHash) -> Result<PerBlockData, Error> {
        // TODO: can be done concurrently
        let block_bytes = self.client.get_block_bytes(blockhash)?;
        let spent_bytes = self.client.get_spent_bytes(blockhash)?;
        let txs_count = block_bytes.txs_count();
        assert_eq!(txs_count, spent_bytes.txs_count());
        Ok(PerBlockData {
            blockhash,
            block_bytes,
            spent_bytes,
            txs_count,
        })
    }

    fn index(
        &self,
        hashes: &[BlockHash],
        mut stats: Option<&mut Stats>,
    ) -> Result<Vec<index::Batch>, Error> {
        use rayon::prelude::*;

        let mut batches = Vec::with_capacity(hashes.len());
        for chunk in hashes.chunks(10) {
            let items: Vec<_> = chunk
                .par_iter()
                .map(|hash| self.fetch_data(*hash))
                .collect::<Result<Vec<_>, Error>>()?;

            let tip = batches
                .last()
                .map_or_else(|| self.headers.tip(), |b: &index::Batch| Some(&b.header));
            batches.extend(Builder::new(tip, &items).build()?);

            for item in items {
                if let Some(s) = stats.as_mut() {
                    s.tip = item.blockhash;
                    s.size_read += item.block_bytes.len() + item.spent_bytes.len();
                    s.indexed_blocks += 1;
                }
            }
        }
        Ok(batches)
    }

    /// Synchornize index with bitcoind.
    /// Compactions are started when no new blocks are indexed.
    pub fn sync(&mut self, limit: usize) -> Result<Stats, Error> {
        let t = std::time::Instant::now();
        // get new headers (and drop stale ones if needed)
        let chunk = self.fetch_new_headers(limit)?;
        // start indexing from a valid tip
        let mut stats = Stats::new(self.headers.tip_hash());
        let batches = self.index(chunk.get(), Some(&mut stats))?;
        self.store.write(&batches)?;
        for batch in batches {
            self.headers.add(batch.header);
        }

        stats.elapsed = t.elapsed();
        if stats.indexed_blocks > 0 {
            self.store.flush()?;
            info!(
                "block={} height={}: indexed {} blocks, {:.3}[MB], dt = {:.3}[s]: {:.3} [ms/block], {:.3} [MB/block], {:.3} [MB/s]",
                self.headers.tip_hash(),
                self.headers.tip_height().unwrap(),
                stats.indexed_blocks,
                stats.size_read as f64 / (1e6),
                stats.elapsed.as_secs_f64(),
                stats.elapsed.as_secs_f64() * 1e3 / (stats.indexed_blocks as f64),
                stats.size_read as f64 / (1e6 * stats.indexed_blocks as f64),
                stats.size_read as f64 / (1e6 * stats.elapsed.as_secs_f64()),
            );
        } else {
            // Start autocompactions when there are no new indexed blocks
            self.store.start_compactions()?;
        }
        Ok(stats)
    }

    /// Collect transactions' locations spending/funding this scripthash.
    /// False-positive may occur, so post-filtering should be applied.
    pub fn locations_by_scripthash(
        &self,
        script_hash: &index::ScriptHash,
        latest_header: Option<&index::IndexedHeader>,
    ) -> Result<impl Iterator<Item = Location<'_>>, Error> {
        let from = latest_header
            .map(|header| header.next_txnum())
            .unwrap_or_default();
        let txnums = self.store.scan_by_script_hash(script_hash, from)?;
        Ok(txnums
            .into_iter()
            // chain and store must be in sync
            .map(|txnum| self.headers.find_by_txnum(txnum)))
    }

    /// Collect transactions' locations matching this txid.
    /// False-positive may occur, so post-filtering should be applied.
    pub fn locations_by_txid(
        &self,
        txid: &bitcoin::Txid,
    ) -> Result<impl Iterator<Item = Location<'_>>, Error> {
        let txnums = self.store.scan_by_txid(txid)?;
        Ok(txnums
            .into_iter()
            // chain and store must be in sync
            .map(|txnum| self.headers.find_by_txnum(txnum)))
    }

    /// Fetch transaction's bytes from bitcoind.
    pub fn get_tx_bytes(&self, location: &Location) -> Result<Vec<u8>, Error> {
        // Lookup tx position within its block (offset & size)
        let pos = self.store.get_tx_block_pos(location.txnum)?;
        // Fetch the bytes from bitcoind
        Ok(self
            .client
            .get_block_part(location.indexed_header.hash(), pos)?)
    }

    /// Resolve a location to the byte range holding its transaction, fetching
    /// nothing. Callers that hold a lock over the chain can collect these,
    /// release the lock, and fetch the bodies themselves.
    pub fn tx_bytes_ref(&self, location: &Location) -> Result<TxBytesRef, Error> {
        let pos = self.store.get_tx_block_pos(location.txnum)?;
        Ok(TxBytesRef {
            block_hash: location.indexed_header.hash(),
            block_height: location.block_height(),
            block_position: location.block_position(),
            offset: pos.offset,
            size: pos.size,
        })
    }

    /// Return the active-chain transaction ids in block order for a height.
    pub fn block_txids_at_height(&self, height: usize) -> Result<Option<Vec<Txid>>, Error> {
        let Some(header) = self.headers.header_at_height(height) else {
            return Ok(None);
        };
        let mut txnum = self
            .headers
            .header_at_height(height.saturating_sub(1))
            .filter(|_| height > 0)
            .map_or_else(index::TxNum::default, index::IndexedHeader::next_txnum);
        let txs_count = header
            .next_txnum()
            .offset_from(txnum)
            .expect("invalid indexed header txnum range");
        let mut txids = Vec::with_capacity(txs_count as usize);
        for _ in 0..txs_count {
            let location = self.headers.find_by_txnum(txnum);
            let raw = self.get_tx_bytes(&location)?;
            txids.push(fmt::txid(&raw)?);
            txnum.increment_by(1);
        }
        Ok(Some(txids))
    }

    pub fn block_hash_at_height(&self, height: usize) -> Option<bitcoin::BlockHash> {
        self.headers.block_hash_at_height(height)
    }

    /// Decoded Bitcoin header (Bitcoin format only).
    #[cfg(not(feature = "liquid"))]
    pub fn block_header_at_height(
        &self,
        height: usize,
    ) -> Result<Option<bitcoin::block::Header>, Error> {
        Ok(self
            .block_header_raw_at_height(height)?
            .map(|raw| bitcoin::consensus::deserialize(&raw))
            .transpose()?)
    }

    /// Raw header bytes as the node serializes them: read from the row on
    /// Bitcoin, fetched from the REST API on Liquid (not stored).
    pub fn block_header_raw_at_height(&self, height: usize) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.block_headers_raw(height, 1)?.into_iter().next())
    }

    /// Raw headers for `count` consecutive heights starting at `start`
    /// (fewer if the chain ends first).
    pub fn block_headers_raw(&self, start: usize, count: usize) -> Result<Vec<Vec<u8>>, Error> {
        let Some(first) = self.headers.header_at_height(start) else {
            return Ok(vec![]);
        };
        let available = self.headers.tip_height().map_or(0, |tip| tip + 1 - start);
        let count = count.min(available);
        if count == 0 {
            return Ok(vec![]);
        }
        if cfg!(feature = "liquid") {
            // one REST round trip; verify each returned header is the one we indexed
            let fetched = self.client.get_headers(first.hash(), count - 1)?;
            let mut out = Vec::with_capacity(count);
            for (i, raw) in fetched.into_iter().take(count).enumerate() {
                let expected = self.headers.header_at_height(start + i).expect("height in range");
                if raw.hash != expected.hash() {
                    return Err(Error::BlockNotFound(headers::Reorg::Stale(raw.hash, start + i)));
                }
                out.push(raw.raw);
            }
            Ok(out)
        } else {
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let header = self.headers.header_at_height(start + i).expect("height in range");
                let value = self
                    .store
                    .get_header_value(&header.key())?
                    .ok_or(headers::Reorg::Missing(header.hash(), start + i))?;
                out.push(raw_from_bitcoin_value(&value));
            }
            Ok(out)
        }
    }

    pub fn indexed_header_at_height(&self, height: usize) -> Option<&index::IndexedHeader> {
        self.headers.header_at_height(height)
    }

    pub fn headers(&self) -> &headers::Headers {
        &self.headers
    }
}

#[cfg(not(feature = "liquid"))]
fn raw_from_bitcoin_value(value: &[u8]) -> Vec<u8> {
    index::IndexedHeader::raw_from_value(value).to_vec()
}

#[cfg(feature = "liquid")]
fn raw_from_bitcoin_value(_value: &[u8]) -> Vec<u8> {
    unreachable!("Liquid header rows carry no raw header")
}

/// Directory name of the index under `db_dir`: the network name on Bitcoin
/// (`bitcoin`, `signet`, ...), the format name on Liquid.
fn db_name(network: Network) -> String {
    if fmt::NAME == "bitcoin" {
        network.to_string()
    } else {
        fmt::NAME.to_string()
    }
}

fn default_rpc_port(nework: Network) -> u16 {
    match nework {
        Network::Bitcoin => 8332,
        Network::Testnet => 18332,
        Network::Testnet4 => 48332,
        Network::Signet => 38332,
        Network::Regtest => 18443,
    }
}

#[cfg(all(test, not(feature = "liquid")))]
mod tests {
    use super::*;
    use bitcoin::{consensus::deserialize, Amount, Transaction};
    use corepc_node::{exe_path, Conf, Node};

    #[test]
    fn test_chain_bitcoind() -> Result<(), Box<dyn std::error::Error>> {
        let mut conf = Conf::default();
        conf.args.push("-rest");

        let node = Node::with_conf(exe_path().unwrap(), &conf).unwrap();

        let get_tip = || {
            node.client
                .get_best_block_hash()
                .unwrap()
                .block_hash()
                .unwrap()
        };
        let addr = node.client.new_address().unwrap();

        const BLOCKS: usize = 101; // so that the first coinbase will be spendable
        node.client.generate_to_address(BLOCKS, &addr).unwrap();

        let dir = tempfile::TempDir::with_prefix("bindex_db").unwrap();
        let config = Config {
            db_path: dir.path().to_path_buf(),
            url: format!("http://{}", node.params.rpc_socket),
            secondary_path: None,
        };
        let mut chain = IndexedChain::from_config(config).unwrap();
        let stats = chain.sync(1000).unwrap();
        assert_eq!(stats.indexed_blocks, BLOCKS + 1);
        assert_eq!(stats.tip, get_tip());

        let addr1 = node.client.new_address().unwrap();
        let addr2 = node.client.new_address().unwrap();

        let txid1 = node
            .client
            .send_to_address(&addr1, Amount::from_int_btc(20))
            .unwrap()
            .txid()
            .unwrap();
        let tx1 = node
            .client
            .get_raw_transaction(txid1)
            .unwrap()
            .transaction()
            .unwrap();
        let txid2 = node
            .client
            .send_to_address(&addr2, Amount::from_int_btc(40))
            .unwrap()
            .txid()
            .unwrap();
        let tx2 = node
            .client
            .get_raw_transaction(txid2)
            .unwrap()
            .transaction()
            .unwrap();
        node.client.generate_to_address(1, &addr).unwrap();
        assert_eq!(node.client.get_mempool_info().unwrap().size, 0);

        let stats = chain.sync(1000).unwrap();
        assert_eq!(stats.indexed_blocks, 1);
        assert_eq!(stats.tip, get_tip());
        let stats = chain.sync(1000).unwrap();
        assert_eq!(stats.indexed_blocks, 0);
        assert_eq!(stats.tip, get_tip());

        let loc1 = exactly_one(chain.locations_by_txid(&txid1).unwrap());
        assert_eq!(loc1.block_height, BLOCKS + 1);
        let tx_bytes = chain.get_tx_bytes(&loc1).unwrap();
        assert_eq!(deserialize::<Transaction>(&tx_bytes).unwrap(), tx1);

        let loc2 = exactly_one(chain.locations_by_txid(&txid2).unwrap());
        assert_eq!(loc1.block_height, BLOCKS + 1);
        let tx_bytes = chain.get_tx_bytes(&loc2).unwrap();
        assert_eq!(deserialize::<Transaction>(&tx_bytes).unwrap(), tx2);

        let txs: Vec<_> = chain
            .locations_by_scripthash(&index::ScriptHash::new(&addr.script_pubkey()), None)
            .unwrap()
            .collect();
        assert_eq!(txs.len(), BLOCKS + 2);

        let locations: Vec<_> = chain
            .locations_by_scripthash(&index::ScriptHash::new(&addr1.script_pubkey()), None)
            .unwrap()
            .collect();
        assert_eq!(locations, vec![loc1, loc2]);

        let locations: Vec<_> = chain
            .locations_by_scripthash(&index::ScriptHash::new(&addr2.script_pubkey()), None)
            .unwrap()
            .collect();
        assert_eq!(locations, vec![loc2]);

        // check reorg
        let old_tip = get_tip();
        node.client.invalidate_block(old_tip).unwrap();
        let stats = chain.sync(1000).unwrap();
        assert!(old_tip != stats.tip);
        assert_eq!(stats.indexed_blocks, 0);
        assert_eq!(stats.tip, get_tip());

        assert_eq!(chain.locations_by_txid(&txid1).unwrap().next(), None);
        assert_eq!(chain.locations_by_txid(&txid2).unwrap().next(), None);
        assert_eq!(
            chain
                .locations_by_scripthash(&index::ScriptHash::new(&addr1.script_pubkey()), None)
                .unwrap()
                .next(),
            None,
        );

        assert_eq!(
            chain
                .locations_by_scripthash(&index::ScriptHash::new(&addr2.script_pubkey()), None)
                .unwrap()
                .next(),
            None,
        );

        Ok(())
    }

    fn exactly_one<T>(mut iter: impl Iterator<Item = T>) -> T {
        let res = iter.next().unwrap();
        assert!(iter.next().is_none());
        res
    }
}
