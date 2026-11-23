//! BlockIndex: policy-free custody index for blocks.
//!
//! This maps `BlockId` -> locator metadata describing where the block bytes live.
//! It is intentionally:
//! - deterministic
//! - allocation-explicit
//! - synchronous
//! - free of ledger / KV / mmap assumptions

use crate::BlockId;
use core::fmt;
use std::collections::BTreeMap;

/// Locator describing where a block's bytes reside.
///
/// This is custody metadata only; interpretation is up to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockLocator {
    /// Content sequence number (or similar monotonic id).
    pub csn: u64,
    /// Byte position in the underlying storage where payload begins.
    pub payload_pos: u64,
    /// Payload length in bytes.
    pub payload_len: u32,
}

/// Deterministic index mapping `BlockId` -> `BlockLocator`.
///
/// Uses `BTreeMap` for:
/// - deterministic iteration order
/// - stable behavior across platforms
#[derive(Default, Clone)]
pub struct BlockIndex {
    map: BTreeMap<BlockId, BlockLocator>,
}

impl BlockIndex {
    /// Create an empty index.
    #[inline]
    pub fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// Number of entries in the index.
    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the index is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Total payload bytes referenced by all locators.
    ///
    /// This is custody accounting only. It does not imply billing, memory pressure,
    /// or lifecycle policy.
    pub fn total_payload_len(&self) -> u64 {
        self.map
            .values()
            .map(|loc| u64::from(loc.payload_len))
            .sum()
    }

    /// Insert a block locator only if the block id is absent.
    ///
    /// Returns `true` if the locator was inserted and `false` if an entry already
    /// existed for the same block id.
    #[inline]
    pub fn insert_absent(&mut self, id: BlockId, loc: BlockLocator) -> bool {
        use std::collections::btree_map::Entry;

        match self.map.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert(loc);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    /// Insert or replace a block locator.
    #[inline]
    pub fn insert(&mut self, id: BlockId, loc: BlockLocator) -> Option<BlockLocator> {
        self.map.insert(id, loc)
    }

    /// Remove a block entry.
    #[inline]
    pub fn remove(&mut self, id: &BlockId) -> Option<BlockLocator> {
        self.map.remove(id)
    }

    /// Check whether a block exists.
    #[inline]
    pub fn contains(&self, id: &BlockId) -> bool {
        self.map.contains_key(id)
    }

    /// Get a block locator.
    #[inline]
    pub fn get(&self, id: &BlockId) -> Option<&BlockLocator> {
        self.map.get(id)
    }

    /// Iterate over entries in deterministic order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&BlockId, &BlockLocator)> {
        self.map.iter()
    }

    /// Extend from an iterator of `(BlockId, BlockLocator)`.
    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = (BlockId, BlockLocator)>,
    {
        for (id, loc) in iter {
            self.map.insert(id, loc);
        }
    }
}

impl fmt::Debug for BlockIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockIndex")
            .field("len", &self.map.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_id_of_bytes;

    fn bid(n: u8) -> BlockId {
        block_id_of_bytes(&[n]).expect("block_id_of_bytes should succeed")
    }

    #[test]
    fn insert_get_remove() {
        let mut idx = BlockIndex::new();
        let id = bid(1);
        let loc = BlockLocator {
            csn: 1,
            payload_pos: 10,
            payload_len: 5,
        };

        assert!(idx.is_empty());
        idx.insert(id, loc);
        assert_eq!(idx.len(), 1);
        assert!(idx.contains(&id));
        assert_eq!(idx.get(&id), Some(&loc));

        let removed = idx.remove(&id);
        assert_eq!(removed, Some(loc));
        assert!(idx.is_empty());
    }

    #[test]
    fn deterministic_iteration_order() {
        let mut idx = BlockIndex::new();

        idx.insert(
            bid(2),
            BlockLocator {
                csn: 2,
                payload_pos: 0,
                payload_len: 1,
            },
        );
        idx.insert(
            bid(1),
            BlockLocator {
                csn: 1,
                payload_pos: 0,
                payload_len: 1,
            },
        );

        let ids: Vec<_> = idx.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![bid(1), bid(2)]);
    }

    #[test]
    fn insert_absent_preserves_first_locator() {
        let mut idx = BlockIndex::new();
        let id = bid(1);
        let first = BlockLocator {
            csn: 1,
            payload_pos: 10,
            payload_len: 5,
        };
        let second = BlockLocator {
            csn: 2,
            payload_pos: 20,
            payload_len: 7,
        };

        assert!(idx.insert_absent(id, first));
        assert!(!idx.insert_absent(id, second));
        assert_eq!(idx.get(&id), Some(&first));
    }

    #[test]
    fn total_payload_len_sums_locator_lengths() {
        let mut idx = BlockIndex::new();

        assert_eq!(idx.total_payload_len(), 0);
        idx.insert(
            bid(1),
            BlockLocator {
                csn: 1,
                payload_pos: 0,
                payload_len: 5,
            },
        );
        idx.insert(
            bid(2),
            BlockLocator {
                csn: 2,
                payload_pos: 5,
                payload_len: 7,
            },
        );

        assert_eq!(idx.total_payload_len(), 12);
    }
}
