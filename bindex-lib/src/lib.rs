pub use bitcoin;
pub use bitcoin_slices;

#[cfg(all(feature = "cache", not(feature = "liquid")))]
pub mod cache;

mod chain;
mod client;
mod db;
pub mod fmt;
mod headers;
mod index;

pub use chain::{Config as ChainConfig, Error as ChainError, IndexedChain, Stats};

/// Where a transaction's bytes live, without fetching them.
///
/// Resolving a [`Location`] to this is a pure index lookup, so a caller holding
/// a lock over the index can release it before paying for the (much slower)
/// body fetch from the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxBytesRef {
    pub block_hash: bitcoin::BlockHash,
    pub block_height: usize,
    /// Transaction index within its block.
    pub block_position: u32,
    /// Byte offset of the transaction within the raw block.
    pub offset: u32,
    pub size: u32,
}
pub use index::IndexedHeader;
pub use headers::Headers;
pub use index::ScriptHash;

#[derive(PartialEq, Eq, PartialOrd, Clone, Copy, Debug)]
pub struct Location<'a> {
    txnum: index::TxNum, // tx number (position within the chain)
    block_height: usize, // block height
    block_offset: u32,   // tx position within its block
    indexed_header: &'a index::IndexedHeader,
}

impl Location<'_> {
    pub fn block_hash(&self) -> bitcoin::BlockHash {
        self.indexed_header.hash()
    }

    pub fn block_height(&self) -> usize {
        self.block_height
    }

    /// Transaction index within its containing block.
    pub fn block_position(&self) -> u32 {
        self.block_offset
    }
}

impl Ord for Location<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.txnum.cmp(&other.txnum)
    }
}
