//! Error model for nsc_block.
//!
//! Errors are explicit, deterministic, and fail-closed.
//! No error should depend on platform-specific behavior.

use core::fmt;

/// Stable error kinds for block identity and pack decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockErrorKind {
    // --- header / wire errors ---
    BadMagic,
    UnknownVersion,
    UnknownFlags,
    InvalidHeaderLength,
    InvalidReservedByte,
    UnsupportedHash,
    UnsupportedCodec,
    DisabledCodec,

    // --- integrity errors ---
    HeaderCrcMismatch,
    PayloadCrcMismatch,
    BlockIdMismatch,
    MissingHeaderChecksum,
    MissingPayloadChecksum,

    // --- size / bounds errors ---
    SizeTooLarge,
    TotalSizeMismatch,
    OutputTooSmall,

    // --- semantic / structural errors ---
    InvalidHeader,
    MissingRequiredHash,
    NonCanonicalBlockIdentity,
    Corrupt,
}

impl BlockErrorKind {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadMagic => "bad magic",
            Self::UnknownVersion => "unknown version",
            Self::UnknownFlags => "unknown flags",
            Self::InvalidHeaderLength => "invalid header length",
            Self::InvalidReservedByte => "invalid reserved byte",
            Self::UnsupportedHash => "unsupported hash",
            Self::UnsupportedCodec => "unsupported codec",
            Self::DisabledCodec => "disabled codec",
            Self::HeaderCrcMismatch => "header crc mismatch",
            Self::PayloadCrcMismatch => "payload crc mismatch",
            Self::BlockIdMismatch => "block id mismatch",
            Self::MissingHeaderChecksum => "missing header checksum",
            Self::MissingPayloadChecksum => "missing payload checksum",
            Self::SizeTooLarge => "size too large",
            Self::TotalSizeMismatch => "total size mismatch",
            Self::OutputTooSmall => "output too small",
            Self::InvalidHeader => "invalid header",
            Self::MissingRequiredHash => "missing required hash",
            Self::NonCanonicalBlockIdentity => "non-canonical block identity",
            Self::Corrupt => "corrupt block data",
        }
    }
}

/// Error type carrying a stable kind.
///
/// Intentionally small and copyable so it can be propagated cheaply
/// without allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BlockError {
    kind: BlockErrorKind,
}

impl BlockError {
    #[inline]
    pub const fn new(kind: BlockErrorKind) -> Self {
        Self { kind }
    }

    #[inline]
    pub const fn kind(&self) -> BlockErrorKind {
        self.kind
    }
}

impl fmt::Debug for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockError")
            .field("kind", &self.kind)
            .finish()
    }
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl std::error::Error for BlockError {}

// Convenience constructors
impl From<BlockErrorKind> for BlockError {
    #[inline]
    fn from(kind: BlockErrorKind) -> Self {
        BlockError::new(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_strings_are_stable() {
        assert_eq!(
            BlockErrorKind::InvalidHeaderLength.as_str(),
            "invalid header length"
        );
        assert_eq!(
            BlockErrorKind::InvalidReservedByte.as_str(),
            "invalid reserved byte"
        );
        assert_eq!(BlockErrorKind::DisabledCodec.as_str(), "disabled codec");
        assert_eq!(
            BlockErrorKind::NonCanonicalBlockIdentity.as_str(),
            "non-canonical block identity"
        );
    }

    #[test]
    fn display_uses_stable_kind_string() {
        let err = BlockError::new(BlockErrorKind::DisabledCodec);
        assert_eq!(err.to_string(), "disabled codec");
    }
}
