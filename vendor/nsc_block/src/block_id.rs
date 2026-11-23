// Block identity.
//
// A `BlockId` is a stable 32-byte identifier for exact payload bytes.
// It is byte identity, not semantic identity.
//
// Determinism rules:
// - same bytes + same algorithm => identical `BlockId` across platforms
// - no hidden fallback hashing
// - unknown algorithm identifiers are errors
// - `HashAlgoId::None` is explicit and maps only to the all-zero id
// - CRC32 is accepted only as a legacy Turtle ledger checksum identity shim,
//   not as collision-strong canonical block identity

use crate::error::{BlockError, BlockErrorKind};

/// Stable block identity (32 bytes).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub [u8; 32]);

impl BlockId {
    /// All-zero identity used only when the caller explicitly selects `HashAlgoId::None`.
    #[inline]
    pub const fn zero() -> Self {
        BlockId([0u8; 32])
    }

    /// Borrow the raw 32-byte identifier.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the raw 32-byte identifier.
    #[inline]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl core::fmt::Debug for BlockId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Keep Debug deterministic but short.
        // (We avoid pulling in hex formatting dependencies here.)
        write!(f, "BlockId([..;32])")
    }
}

/// Hash algorithm identifier used by block ids and compatible Turtle ledger checksums.
///
/// Turtle's existing ledger uses algorithm IDs: CRC32=1, BLAKE3=2.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgoId {
    None = 0,
    Crc32Ieee = 1,
    Blake3_256 = 2,
}

impl HashAlgoId {
    /// Stable numeric representation.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Stable lowercase algorithm label.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Crc32Ieee => "crc32_ieee",
            Self::Blake3_256 => "blake3_256",
        }
    }

    /// True only for algorithms that provide canonical collision-strong block identity.
    ///
    /// `Crc32Ieee` is intentionally false: it is supported only for Turtle ledger
    /// checksum compatibility, not as a canonical block identity algorithm.
    #[inline]
    pub const fn is_canonical_block_identity(self) -> bool {
        matches!(self, Self::Blake3_256)
    }

    /// True for legacy checksum shims that can be mapped into `BlockId` for
    /// compatibility, but should not be treated as canonical block identity.
    #[inline]
    pub const fn is_ledger_checksum_shim(self) -> bool {
        matches!(self, Self::Crc32Ieee)
    }

    /// Parse a stable numeric representation.
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::Crc32Ieee),
            2 => Some(Self::Blake3_256),
            _ => None,
        }
    }
}

/// v0 convenience: canonical `BlockId` for arbitrary bytes.
///
/// This is intentionally explicit in behavior: v0 uses `Blake3_256`.
#[inline]
pub fn block_id_of_bytes(bytes: &[u8]) -> Result<BlockId, BlockError> {
    block_id_with_algo(HashAlgoId::Blake3_256, bytes)
}

/// Compute a `BlockId` using an explicit algorithm.
///
/// `Blake3_256` is the canonical collision-strong block identity path. `Crc32Ieee`
/// remains available only so legacy Turtle ledger checksums can be represented in
/// the same 32-byte `BlockId` carrier. Callers that need canonical block identity
/// should require `algo.is_canonical_block_identity()`.
pub fn block_id_with_algo(algo: HashAlgoId, bytes: &[u8]) -> Result<BlockId, BlockError> {
    match algo {
        HashAlgoId::None => Ok(BlockId::zero()),
        HashAlgoId::Crc32Ieee => {
            let crc = crc32fast::hash(bytes);
            let mut out = [0u8; 32];
            out[0..4].copy_from_slice(&crc.to_le_bytes());
            Ok(BlockId(out))
        }
        HashAlgoId::Blake3_256 => {
            // blake3 outputs 32 bytes.
            let hash = blake3::hash(bytes);
            Ok(BlockId(*hash.as_bytes()))
        }
    }
}

/// Convert a Turtle ledger checksum into the 32-byte `BlockId` carrier.
///
/// Turtle stores a fixed 32-byte checksum along with an algorithm id.
/// We map those to `BlockId` deterministically:
/// - BLAKE3: use all 32 bytes
/// - CRC32: first 4 bytes are CRC32 (little-endian), rest zero
///   This is a compatibility carrier only, not canonical collision-strong identity.
/// - None: all-zero id
/// - unknown algorithm id: None
pub fn block_id_from_ledger_sum(algo_id: u32, sum32: &[u8; 32]) -> Option<BlockId> {
    let algo = HashAlgoId::from_u8(u8::try_from(algo_id).ok()?)?;

    match algo {
        HashAlgoId::Blake3_256 => Some(BlockId(*sum32)),
        HashAlgoId::Crc32Ieee => {
            let mut out = [0u8; 32];
            out[0..4].copy_from_slice(&sum32[0..4]);
            Some(BlockId(out))
        }
        HashAlgoId::None => Some(BlockId::zero()),
    }
}

/// Validate that a stored `block_id` field matches the decoded payload under the given algo.
///
/// - If `algo == None`, the stored `block_id` must be all zeros.
/// - Otherwise, it must match the computed hash bytes.
pub fn validate_block_id_field(
    algo: HashAlgoId,
    payload: &[u8],
    stored: &[u8; 32],
) -> Result<(), BlockError> {
    let expected = block_id_with_algo(algo, payload)?;

    if expected.0 != *stored {
        return Err(BlockErrorKind::BlockIdMismatch.into());
    }

    Ok(())
}

/// Map unknown hash algo byte to a stable error.
#[inline]
pub fn parse_hash_algo_or_err(v: u8) -> Result<HashAlgoId, BlockError> {
    HashAlgoId::from_u8(v).ok_or_else(|| BlockErrorKind::UnsupportedHash.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_is_the_only_canonical_block_identity_algo() {
        assert!(HashAlgoId::Blake3_256.is_canonical_block_identity());
        assert!(!HashAlgoId::Crc32Ieee.is_canonical_block_identity());
        assert!(!HashAlgoId::None.is_canonical_block_identity());
    }

    #[test]
    fn crc32_is_only_a_ledger_checksum_shim() {
        assert!(HashAlgoId::Crc32Ieee.is_ledger_checksum_shim());
        assert!(!HashAlgoId::Blake3_256.is_ledger_checksum_shim());
        assert!(!HashAlgoId::None.is_ledger_checksum_shim());
    }

    #[test]
    fn crc32_ledger_sum_uses_first_four_bytes_only() {
        let mut sum = [9u8; 32];
        sum[0..4].copy_from_slice(&0xAABBCCDDu32.to_le_bytes());

        let id = block_id_from_ledger_sum(HashAlgoId::Crc32Ieee.as_u8() as u32, &sum)
            .expect("crc32 ledger sum should convert");

        let mut expected = [0u8; 32];
        expected[0..4].copy_from_slice(&0xAABBCCDDu32.to_le_bytes());
        assert_eq!(id.into_bytes(), expected);
    }
}
