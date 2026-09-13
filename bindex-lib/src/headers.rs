use crate::{
    index::{self, IndexedHeader},
    Location,
};

use bitcoin::{hashes::Hash, BlockHash};

#[derive(thiserror::Error, Debug)]
pub enum Reorg {
    #[error("missing block={0} at height={1}")]
    Missing(bitcoin::BlockHash, usize),

    #[error("stale block={0} at height={1}")]
    Stale(bitcoin::BlockHash, usize),
}

pub struct Headers {
    rows: Vec<index::IndexedHeader>,
}

impl Headers {
    /// Build a chain from a list of headers (sorted by height).
    pub fn new(rows: Vec<index::IndexedHeader>) -> Self {
        let mut block_hash = bitcoin::BlockHash::all_zeros();
        for (height, row) in rows.iter().enumerate() {
            assert_eq!(row.prev_blockhash(), block_hash);
            assert_eq!(row.height() as usize, height);
            block_hash = row.hash();
        }
        Self { rows }
    }

    /// Return tip block hash (or `all_zeros` if no blocks).
    pub fn tip_hash(&self) -> bitcoin::BlockHash {
        self.rows
            .last()
            .map(index::IndexedHeader::hash)
            .unwrap_or_else(bitcoin::BlockHash::all_zeros)
    }

    /// Chain height (the genesis block is exluded)
    pub fn tip_height(&self) -> Option<usize> {
        self.rows.len().checked_sub(1)
    }

    /// Add new tip.
    pub fn add(&mut self, tip: index::IndexedHeader) {
        assert_eq!(tip.prev_blockhash(), self.tip_hash());
        assert_eq!(tip.height() as usize, self.rows.len());
        self.rows.push(tip)
    }

    /// Pop current tip.
    pub fn pop(&mut self) -> Option<index::IndexedHeader> {
        self.rows.pop()
    }

    pub fn tip(&self) -> Option<&index::IndexedHeader> {
        self.rows.last()
    }

    pub fn genesis(&self) -> Option<&index::IndexedHeader> {
        self.rows.first()
    }

    pub fn iter_headers(&self) -> impl Iterator<Item = &IndexedHeader> {
        self.rows.iter()
    }

    pub fn header_at_height(&self, height: usize) -> Option<&index::IndexedHeader> {
        self.rows.get(height)
    }

    pub fn block_hash_at_height(&self, height: usize) -> Option<bitcoin::BlockHash> {
        self.header_at_height(height)
            .map(index::IndexedHeader::hash)
    }

    pub fn get_header(
        &self,
        hash: BlockHash,
        height: usize,
    ) -> Result<&index::IndexedHeader, Reorg> {
        let header = self.rows.get(height).ok_or(Reorg::Missing(hash, height))?;
        if header.hash() == hash {
            Ok(header)
        } else {
            Err(Reorg::Stale(hash, height))
        }
    }

    /// Find transaction's chain location.
    pub fn find_by_txnum(&self, txnum: index::TxNum) -> Location<'_> {
        // The first block whose `next_txnum` is past `txnum` contains it. Empty
        // blocks (possible on Liquid) share a `next_txnum`, so a plain binary
        // search could land on any of them; `partition_point` cannot.
        let block_height = self
            .rows
            .partition_point(|header| header.next_txnum() <= txnum);

        let indexed_header = self.rows.get(block_height).expect("missing height");
        assert!(
            txnum < indexed_header.next_txnum(),
            "binary search failed to find the correct position"
        );

        let prev_txnum = self
            .rows
            .get(block_height - 1)
            .map_or_else(index::TxNum::default, index::IndexedHeader::next_txnum);

        // txnum offset within its block
        let block_offset = txnum
            .offset_from(prev_txnum)
            .expect("binary search failed to find the correct position");

        Location {
            txnum,
            block_height,
            block_offset,
            indexed_header,
        }
    }
}
