//! Wire header definition for NSC block packs.
//!
//! This module defines the fixed, versioned header layout and performs structural
//! validation only: magic, version, flags, algorithm identifiers, lengths, and
//! header CRC. Payload decoding and codec execution live elsewhere.

use crate::block_id::HashAlgoId;
use crate::codecs::PackCodecId;
use crate::error::{BlockError, BlockErrorKind};

/// Magic bytes: "NSCB"
pub const MAGIC: [u8; 4] = *b"NSCB";

/// Current wire version.
pub const VERSION_V1: u8 = 1;

/// Fixed header length for v1.
pub const HEADER_LEN_V1: u16 = 59;

/// On-wire header representation (parsed form).
#[derive(Debug, Clone)]
pub struct WireHeader {
    pub version: u8,
    pub flags: u8,
    pub hash_algo: HashAlgoId,
    pub codec_id: PackCodecId,
    pub header_len: u16,
    pub payload_len: u32,
    pub packed_len: u32,
    pub header_crc32: u32,
    pub payload_crc32: u32,
    pub block_id: [u8; 32],
}

impl WireHeader {
    /// Parse and validate a wire header from the start of `bytes`.
    ///
    /// This performs *structural* validation only:
    /// - magic, version, flags
    /// - header length
    /// - bounds and consistency checks
    /// - header CRC
    pub fn parse(bytes: &[u8]) -> Result<Self, BlockError> {
        // Minimum length check
        if bytes.len() < HEADER_LEN_V1 as usize {
            return Err(BlockErrorKind::TotalSizeMismatch.into());
        }

        // Magic
        if bytes[0..4] != MAGIC {
            return Err(BlockErrorKind::BadMagic.into());
        }

        let version = bytes[4];
        if version != VERSION_V1 {
            return Err(BlockErrorKind::UnknownVersion.into());
        }

        let flags = bytes[5];
        if flags != 0 {
            return Err(BlockErrorKind::UnknownFlags.into());
        }

        let hash_algo_raw = bytes[6];
        let hash_algo =
            HashAlgoId::from_u8(hash_algo_raw).ok_or_else(|| BlockErrorKind::UnsupportedHash)?;

        let codec_id = PackCodecId::from_u8(bytes[7]).ok_or(BlockErrorKind::UnsupportedCodec)?;

        let header_len = u16::from_le_bytes([bytes[8], bytes[9]]);
        if header_len != HEADER_LEN_V1 {
            return Err(BlockErrorKind::InvalidHeaderLength.into());
        }

        let payload_len = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
        let packed_len = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);

        let header_crc32 = u32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
        let payload_crc32 = u32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);

        let mut block_id = [0u8; 32];
        block_id.copy_from_slice(&bytes[26..58]);

        // Header CRC check covers all fixed header fields before the CRC fields.
        let computed_crc = crc32fast::hash(&bytes[0..18]);
        if computed_crc != header_crc32 {
            return Err(BlockErrorKind::HeaderCrcMismatch.into());
        }

        Ok(WireHeader {
            version,
            flags,
            hash_algo,
            codec_id,
            header_len,
            payload_len,
            packed_len,
            header_crc32,
            payload_crc32,
            block_id,
        })
    }

    /// Validate total size consistency against the full byte slice.
    #[inline]
    pub fn validate_total_size(&self, total_len: usize) -> Result<(), BlockError> {
        let expected = usize::from(self.header_len)
            .checked_add(
                usize::try_from(self.packed_len).map_err(|_| BlockErrorKind::SizeTooLarge)?,
            )
            .ok_or(BlockErrorKind::SizeTooLarge)?;
        if total_len != expected {
            return Err(BlockErrorKind::TotalSizeMismatch.into());
        }
        Ok(())
    }
}
