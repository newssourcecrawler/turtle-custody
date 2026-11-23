//! ptr/v0 — Pointer record (last-write-wins, rebuildable, non-authoritative).
//!
//! Pointer is a *hint* of “where am I now?” for a {group,id} entity.
//! Truth always resides in the append-only custody lake.
//!
//! Canonical key:
//!     event/{group}/{id}/ptr/v0
//!
//! Contract:
//! - Writer appends a new ptr record when state advances (typically after Gate5).
//! - Reader treats ptr as non-authoritative; can be rebuilt from custody.
//! - ptr is last-write-wins by observed order (e.g. CSN), not by content.
//!
//! Minimal v0 shape is intentionally small and stringly to avoid schema wars.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtrV0 {
    /// Always "ptr/v0".
    pub schema: &'static str,

    /// Namespace group: e.g. "brownout".
    pub group: String,

    /// Entity id inside group: job_id/session_id/blob_id.
    pub id: String,

    /// Monotonic revision for this entity, advanced by the writer.
    pub rev: u64,

    /// Committed sequence marker observed when writing this ptr (0 if unknown).
    pub at_csn: u64,

    /// Caller-supplied phase marker, such as Gate1 through Gate5, done, or failed.
    pub phase: String,

    /// Opaque resume cursor (stringly on purpose).
    pub cursor: String,

    /// Sticky error bit (0/1). Details belong in custody facts.
    pub error: u64,
}

impl PtrV0 {
    pub const SCHEMA: &'static str = "ptr/v0";

    pub fn key(group: &str, id: &str) -> String {
        format!("event/{}/{}/ptr/v0", group, id)
    }

    /// Minimal JSON encoding without adding deps.
    /// Assumes group/id/phase/cursor are already “safe” identifiers (no quotes/newlines).
    pub fn to_json_bytes(&self) -> Vec<u8> {
        // Keep field order stable.
        format!(
            "{{\"schema\":\"{}\",\"group\":\"{}\",\"id\":\"{}\",\"rev\":{},\"at_csn\":{},\"phase\":\"{}\",\"cursor\":\"{}\",\"error\":{}}}",
            self.schema,
            self.group,
            self.id,
            self.rev,
            self.at_csn,
            self.phase,
            self.cursor,
            self.error
        ).into_bytes()
    }
}
