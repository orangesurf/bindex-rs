use bitcoin::{hashes::Hash as _, BlockHash};

use crate::{
    fmt,
    index::{self, TxNum},
};

/// The in-memory header row: 72 bytes per block, whatever the chain. Raw
/// header bytes are not kept in memory (a Liquid dynafed header is ~1.4 KB and
/// there are 4M of them); see `IndexedChain::block_header_raw_at_height`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct IndexedHeader {
    height: u32,
    next_txnum: TxNum,
    hash: BlockHash,
    prev_blockhash: BlockHash,
}

const BLOCK_HASH_LEN: usize = BlockHash::LEN;

/// DB row.
///
/// Bitcoin: key = next_txnum, value = blockhash || raw 80-byte header: the
/// original layout, so existing indexes keep working. Every Bitcoin block has a
/// coinbase, so next_txnum is unique, and the height is the row's position.
///
/// Liquid: key = height, value = next_txnum || blockhash || prev_blockhash. Raw
/// headers are fetched from the node on demand instead of being stored. A
/// Liquid index built on a pruned node sees pruned heights as empty blocks, so
/// next_txnum is not unique there and cannot be the key.
type SerializedHeaderRow = ([u8; 4], Vec<u8>);

impl IndexedHeader {
    pub fn new(height: u32, next_txnum: TxNum, hash: BlockHash, prev_blockhash: BlockHash) -> Self {
        Self {
            height,
            next_txnum,
            hash,
            prev_blockhash,
        }
    }

    pub(crate) fn from_block(
        height: u32,
        next_txnum: TxNum,
        hash: BlockHash,
        block_bytes: &index::BlockBytes,
    ) -> Self {
        let prev = fmt::prev_blockhash(block_bytes.header()).expect("invalid header bytes");
        Self::new(height, next_txnum, hash, prev)
    }

    /// RocksDB key of this row.
    pub fn key(&self) -> [u8; 4] {
        if cfg!(feature = "liquid") {
            self.height.to_be_bytes()
        } else {
            self.next_txnum.serialize()
        }
    }

    #[cfg(not(feature = "liquid"))]
    pub fn serialize(&self, raw_header: &[u8]) -> SerializedHeaderRow {
        let mut value = Vec::with_capacity(BLOCK_HASH_LEN + raw_header.len());
        value.extend_from_slice(self.hash.as_byte_array());
        value.extend_from_slice(raw_header);
        (self.key(), value)
    }

    /// `position` is the row's index in key order, which is its height.
    #[cfg(not(feature = "liquid"))]
    pub fn deserialize(key: &[u8], value: &[u8], position: usize) -> Self {
        let key: [u8; TxNum::LEN] = key.try_into().expect("invalid header key");
        let hash = BlockHash::from_byte_array(value[..BLOCK_HASH_LEN].try_into().unwrap());
        let prev = fmt::prev_blockhash(&value[BLOCK_HASH_LEN..]).expect("invalid header bytes");
        Self::new(
            u32::try_from(position).expect("height overflow"),
            TxNum::deserialize(key),
            hash,
            prev,
        )
    }

    /// Raw header bytes stored in a Bitcoin row value.
    #[cfg(not(feature = "liquid"))]
    pub fn raw_from_value(value: &[u8]) -> &[u8] {
        &value[BLOCK_HASH_LEN..]
    }

    #[cfg(feature = "liquid")]
    pub fn serialize(&self, _raw_header: &[u8]) -> SerializedHeaderRow {
        let mut value = Vec::with_capacity(TxNum::LEN + 2 * BLOCK_HASH_LEN);
        value.extend_from_slice(&self.next_txnum.serialize());
        value.extend_from_slice(self.hash.as_byte_array());
        value.extend_from_slice(self.prev_blockhash.as_byte_array());
        (self.key(), value)
    }

    #[cfg(feature = "liquid")]
    pub fn deserialize(key: &[u8], value: &[u8], position: usize) -> Self {
        let height = u32::from_be_bytes(key.try_into().expect("invalid header key"));
        assert_eq!(height as usize, position, "header rows must be contiguous by height");
        let next_txnum = TxNum::deserialize(value[..TxNum::LEN].try_into().unwrap());
        let hash = BlockHash::from_byte_array(
            value[TxNum::LEN..TxNum::LEN + BLOCK_HASH_LEN].try_into().unwrap(),
        );
        let prev = BlockHash::from_byte_array(
            value[TxNum::LEN + BLOCK_HASH_LEN..TxNum::LEN + 2 * BLOCK_HASH_LEN].try_into().unwrap(),
        );
        Self::new(height, next_txnum, hash, prev)
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn next_txnum(&self) -> TxNum {
        self.next_txnum
    }

    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    pub fn prev_blockhash(&self) -> BlockHash {
        self.prev_blockhash
    }
}
