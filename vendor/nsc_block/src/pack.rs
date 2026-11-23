//! Pack encode/decode.
//!
//! v0 ships:
//! - explicit hash algo: `None` or canonical `Blake3_256` for new block packs
//! - legacy CRC32 may be parsed as a Turtle checksum carrier but is not a
//!   canonical pack identity choice
//! - explicit codec: Raw only; known future codecs are disabled until implemented
//! - strict header validation + CRCs
//! - no hidden work: caller-provided output buffer for decode

use crate::block_id::{block_id_with_algo, validate_block_id_field, HashAlgoId};
use crate::codecs::{raw, PackCodecId};
use crate::error::{BlockError, BlockErrorKind};
use crate::wire::header::{WireHeader, HEADER_LEN_V1, MAGIC, VERSION_V1};

/// Parameters for pack encoding.
#[derive(Debug, Clone, Copy)]
pub struct PackEncodeParams {
    pub hash: HashAlgoId,
    pub codec: PackCodecId,
    /// Reserved for future codecs (ignored for Raw).
    pub level: u8,
    /// Hard cap on total encoded output size (header + packed payload).
    pub max_out: u32,
    pub reserved: u32,
}

/// Parameters for pack decoding.
#[derive(Debug, Clone, Copy)]
pub struct PackDecodeParams {
    pub require_hash: bool,
    /// Hard cap on input size.
    pub max_in: u32,
    /// Hard cap on decoded payload size.
    pub max_out: u32,
    pub reserved: u32,
}

fn ensure_enabled_codec(codec: PackCodecId) -> Result<(), BlockError> {
    match codec {
        PackCodecId::Raw => Ok(()),
        PackCodecId::Deflate => Err(BlockErrorKind::DisabledCodec.into()),
    }
}

fn ensure_pack_hash_choice(hash: HashAlgoId) -> Result<(), BlockError> {
    if hash == HashAlgoId::None || hash.is_canonical_block_identity() {
        return Ok(());
    }

    Err(BlockErrorKind::NonCanonicalBlockIdentity.into())
}

/// Compute the required decoded payload length for a packed block.
pub fn required_out_len(bytes: &[u8]) -> Result<usize, BlockError> {
    let h = WireHeader::parse(bytes)?;
    Ok(h.payload_len as usize)
}

/// Encode a payload into a packed block.
///
/// Determinism: same payload+params => identical output bytes.
pub fn pack_encode(params: &PackEncodeParams, payload: &[u8]) -> Result<Vec<u8>, BlockError> {
    ensure_enabled_codec(params.codec)?;
    ensure_pack_hash_choice(params.hash)?;

    if params.level != 0 || params.reserved != 0 {
        return Err(BlockErrorKind::InvalidHeader.into());
    }

    if params.max_out < HEADER_LEN_V1 as u32 {
        return Err(BlockErrorKind::SizeTooLarge.into());
    }

    // Size caps
    if payload.len() > u32::MAX as usize {
        return Err(BlockErrorKind::SizeTooLarge.into());
    }

    // Encode (raw pass-through)
    let max_payload_out = params.max_out - HEADER_LEN_V1 as u32;
    let packed = raw::encode_raw(payload, max_payload_out)?;

    let payload_len = payload.len() as u32;
    let packed_len = packed.len() as u32;

    // Total output cap
    let total_len = (HEADER_LEN_V1 as u32)
        .checked_add(packed_len)
        .ok_or(BlockErrorKind::SizeTooLarge)?;

    if total_len > params.max_out {
        return Err(BlockErrorKind::SizeTooLarge.into());
    }

    // Integrity fields
    let payload_crc32 = crc32fast::hash(payload);

    // If hash is None, enforce zero block_id for a single canonical encoding.
    let block_id = if params.hash == HashAlgoId::None {
        [0u8; 32]
    } else {
        block_id_with_algo(params.hash, payload)?.into_bytes()
    };

    // Build header bytes
    let mut header = [0u8; HEADER_LEN_V1 as usize];
    header[0..4].copy_from_slice(&MAGIC);
    header[4] = VERSION_V1;
    header[5] = 0;
    header[6] = params.hash.as_u8();
    header[7] = params.codec.as_u8();
    header[8..10].copy_from_slice(&(HEADER_LEN_V1 as u16).to_le_bytes());
    header[10..14].copy_from_slice(&payload_len.to_le_bytes());
    header[14..18].copy_from_slice(&packed_len.to_le_bytes());
    // header_crc32 filled after computing
    header[22..26].copy_from_slice(&payload_crc32.to_le_bytes());
    header[26..58].copy_from_slice(&block_id);

    let header_crc32 = crc32fast::hash(&header[0..18]);
    header[18..22].copy_from_slice(&header_crc32.to_le_bytes());

    // Assemble final output
    let mut out = Vec::with_capacity(total_len as usize);
    out.extend_from_slice(&header);
    out.extend_from_slice(&packed);
    Ok(out)
}

/// Decode a packed block into a caller-provided output buffer.
///
/// Returns the decoded payload length.
pub fn pack_decode(
    params: &PackDecodeParams,
    bytes: &[u8],
    out: &mut [u8],
) -> Result<usize, BlockError> {
    // Input cap
    if bytes.len() > params.max_in as usize {
        return Err(BlockErrorKind::SizeTooLarge.into());
    }

    if params.reserved != 0 {
        return Err(BlockErrorKind::InvalidHeader.into());
    }

    let h = WireHeader::parse(bytes)?;
    h.validate_total_size(bytes.len())?;

    // Output cap
    if h.payload_len > params.max_out {
        return Err(BlockErrorKind::SizeTooLarge.into());
    }

    // Codec support. Known future codecs are still disabled until their
    // implementation is explicitly enabled.
    let codec = h.codec_id;
    ensure_enabled_codec(codec)?;

    // Hash requirements
    if params.require_hash && h.hash_algo == HashAlgoId::None {
        return Err(BlockErrorKind::MissingRequiredHash.into());
    }

    if h.hash_algo != HashAlgoId::None && !h.hash_algo.is_canonical_block_identity() {
        return Err(BlockErrorKind::NonCanonicalBlockIdentity.into());
    }

    // Enforce canonical zero block_id when hash is None.
    if h.hash_algo == HashAlgoId::None && h.block_id != [0u8; 32] {
        return Err(BlockErrorKind::Corrupt.into());
    }

    let packed_start = h.header_len as usize;
    let packed_end = packed_start + (h.packed_len as usize);
    let packed_payload = &bytes[packed_start..packed_end];

    // Decode
    let decoded_len = match codec {
        PackCodecId::Raw => raw::decode_raw(packed_payload, out, h.payload_len)?,
        PackCodecId::Deflate => return Err(BlockErrorKind::DisabledCodec.into()),
    };

    let decoded = &out[..decoded_len];

    // Verify payload CRC over decoded bytes
    let crc = crc32fast::hash(decoded);
    if crc != h.payload_crc32 {
        return Err(BlockErrorKind::PayloadCrcMismatch.into());
    }

    // Verify block id (if algo != None). If algo is None, zero-block id already enforced above.
    validate_block_id_field(h.hash_algo, decoded, &h.block_id)?;

    Ok(decoded_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_params(hash: HashAlgoId, codec: PackCodecId) -> PackEncodeParams {
        PackEncodeParams {
            hash,
            codec,
            level: 0,
            max_out: 4096,
            reserved: 0,
        }
    }

    fn decode_params(require_hash: bool) -> PackDecodeParams {
        PackDecodeParams {
            require_hash,
            max_in: 4096,
            max_out: 4096,
            reserved: 0,
        }
    }

    #[test]
    fn encode_rejects_disabled_codec() {
        let err = pack_encode(
            &encode_params(HashAlgoId::Blake3_256, PackCodecId::Deflate),
            b"abc",
        )
        .expect_err("deflate should be disabled");
        assert_eq!(err.kind(), BlockErrorKind::DisabledCodec);
    }

    #[test]
    fn encode_rejects_crc32_as_pack_identity() {
        let err = pack_encode(
            &encode_params(HashAlgoId::Crc32Ieee, PackCodecId::Raw),
            b"abc",
        )
        .expect_err("crc32 should not be a canonical pack identity");
        assert_eq!(err.kind(), BlockErrorKind::NonCanonicalBlockIdentity);
    }

    #[test]
    fn encode_decode_accepts_raw_blake3() {
        let encoded = pack_encode(
            &encode_params(HashAlgoId::Blake3_256, PackCodecId::Raw),
            b"abc",
        )
        .expect("encode");
        let mut out = [0u8; 16];
        let n = pack_decode(&decode_params(true), &encoded, &mut out).expect("decode");
        assert_eq!(&out[..n], b"abc");
    }

    #[test]
    fn decode_rejects_disabled_codec_from_header() {
        let mut encoded = pack_encode(
            &encode_params(HashAlgoId::Blake3_256, PackCodecId::Raw),
            b"abc",
        )
        .expect("encode");
        encoded[7] = PackCodecId::Deflate.as_u8();
        let header_crc32 = crc32fast::hash(&encoded[0..18]);
        encoded[18..22].copy_from_slice(&header_crc32.to_le_bytes());

        let mut out = [0u8; 16];
        let err = pack_decode(&decode_params(true), &encoded, &mut out)
            .expect_err("deflate should be disabled");
        assert_eq!(err.kind(), BlockErrorKind::DisabledCodec);
    }

    #[test]
    fn decode_rejects_crc32_identity_from_header() {
        let mut encoded = pack_encode(
            &encode_params(HashAlgoId::Blake3_256, PackCodecId::Raw),
            b"abc",
        )
        .expect("encode");
        encoded[6] = HashAlgoId::Crc32Ieee.as_u8();
        let header_crc32 = crc32fast::hash(&encoded[0..18]);
        encoded[18..22].copy_from_slice(&header_crc32.to_le_bytes());

        let mut out = [0u8; 16];
        let err = pack_decode(&decode_params(true), &encoded, &mut out)
            .expect_err("crc32 should not be canonical pack identity");
        assert_eq!(err.kind(), BlockErrorKind::NonCanonicalBlockIdentity);
    }
}
