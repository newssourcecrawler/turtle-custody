//! Raw no-compression codec.
//!
//! This codec performs an exact byte-for-byte pass-through. It exists so the pack
//! format can be explicit about codec choice even when no compression is applied.

use crate::error::{BlockError, BlockErrorKind};
use std::convert::TryFrom;

/// Encode raw payload bytes.
///
/// `max_out` is a hard cap on the output length.
pub fn encode_raw(payload: &[u8], max_out: u32) -> Result<Vec<u8>, BlockError> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| BlockErrorKind::SizeTooLarge)?;
    if payload_len > max_out {
        return Err(BlockErrorKind::SizeTooLarge.into());
    }
    Ok(payload.to_vec())
}

/// Decode raw payload bytes into `out`.
///
/// Returns the number of decoded bytes written.
pub fn decode_raw(packed: &[u8], out: &mut [u8], expected_len: u32) -> Result<usize, BlockError> {
    let expected_len_usize =
        usize::try_from(expected_len).map_err(|_| BlockErrorKind::SizeTooLarge)?;
    if packed.len() != expected_len_usize {
        return Err(BlockErrorKind::Corrupt.into());
    }
    if out.len() < expected_len_usize {
        return Err(BlockErrorKind::OutputTooSmall.into());
    }
    out[..expected_len_usize].copy_from_slice(packed);
    Ok(expected_len_usize)
}
