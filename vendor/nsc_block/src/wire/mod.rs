//! Wire format for NSC block packs.
//!
//! v1 is a fixed header followed by packed payload bytes. The wire layer owns
//! header shape, header identifiers, header-length law, and header checksum law.
//! Codec execution, payload checksum verification, and block-id verification
//! belong to the pack layer after the wire header has been parsed.

pub mod header;
