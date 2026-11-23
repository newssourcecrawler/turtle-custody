//! NSC Block
//!
//! Deterministic block identity (`BlockId`), block-location indexing, and a
//! portable block pack wire format.
//!
//! ## Design goals
//! - Determinism across platforms
//! - Hostile-input safety: caps, bounds checks, fail-closed parsing
//! - Explicit modes: no automatic codec or hash fallback
//! - Auditability: small, stable, versioned wire format
//! - Custody compatibility: Turtle ledger checksum shims stay non-canonical
//!
//! ## Boundary
//! `Blake3_256` is the canonical block identity path. `Crc32Ieee` exists only
//! as a Turtle ledger checksum compatibility carrier. Block locators are
//! coordinates, not authority. Codec IDs may be known without being enabled.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

mod block_id;
mod block_index;
mod error;
mod pack;

pub mod codecs;
pub mod wire;

pub use crate::block_id::{
    block_id_from_ledger_sum, block_id_of_bytes, block_id_with_algo, parse_hash_algo_or_err,
    validate_block_id_field, BlockId, HashAlgoId,
};
pub use crate::block_index::{BlockIndex, BlockLocator};
pub use crate::codecs::PackCodecId;
pub use crate::error::{BlockError, BlockErrorKind};
pub use crate::pack::{
    pack_decode, pack_encode, required_out_len, PackDecodeParams, PackEncodeParams,
};
