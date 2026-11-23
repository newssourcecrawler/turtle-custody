//! Pack codec identifiers.
//!
//! v0 enables Raw only. Other identifiers are reserved wire values and must not
//! be silently accepted by pack encode/decode unless their codec implementation is
//! explicitly enabled.

pub mod raw;

/// Codec identifier used on the wire.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackCodecId {
    Raw = 0,
    /// Reserved for future use (not enabled in v0).
    Deflate = 1,
}

impl PackCodecId {
    /// Stable numeric representation.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Stable lowercase codec label.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Deflate => "deflate",
        }
    }

    /// Parse a stable numeric representation.
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Raw),
            1 => Some(Self::Deflate),
            _ => None,
        }
    }
}
