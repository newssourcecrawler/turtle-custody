//turtle/src/block_id.rs
//! BlockId: content identity for portable blocks.
//!
//! Turtle delegates canonical block identity to the shared `nsc_block` crate so
//! Turtle, semantic search, and tooling share one definition.
//!
//! This module is a *compatibility shim* that preserves Turtle’s historical API
//! signatures:
//! - `block_id_of_bytes(&[u8]) -> BlockId` (infallible)
//! - `block_id_from_ledger_sum(u32, [u8; 32]) -> Option<BlockId>` (sum by value)

pub use nsc_block::{BlockId, HashAlgoId as BlockIdAlgo};

/// Compute the canonical BlockId for the given bytes.
///
/// Canonical rule: any byte change => different BlockId.
///
/// Note: `nsc_block::block_id_of_bytes` is fallible (`Result`), but for the
/// canonical Blake3 path it is not expected to fail. We preserve Turtle’s
/// historical infallible API.
#[inline]
pub fn block_id_of_bytes(block_bytes: &[u8]) -> BlockId {
    match nsc_block::block_id_of_bytes(block_bytes) {
        Ok(id) => id,
        Err(e) => {
            // Unreachable in practice for Blake3 hashing.
            panic!("nsc_block::block_id_of_bytes unexpectedly failed: {e}");
        }
    }
}

/// Interpret a ledger-stored 32-byte checksum/sum as a BlockId.
///
/// Turtle’s ledger stores a fixed 32-byte sum plus an algo id (CRC32=1, BLAKE3=2).
/// For BLAKE3, we can treat the sum as a stable BlockId.
#[inline]
pub fn block_id_from_ledger_sum(algo_u32: u32, sum: [u8; 32]) -> Option<BlockId> {
    nsc_block::block_id_from_ledger_sum(algo_u32, &sum)
}
