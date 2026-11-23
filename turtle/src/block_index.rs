//! BlockIndex: custody-layer index from BlockId -> where the bytes live.
//!
//! Turtle delegates the canonical BlockIndex implementation to the shared `nsc_block` crate
//! so Turtle, tooling, and semantic layers share one deterministic, policy-free index.
//!
//! This file is a compatibility shim; do not add new functionality here.

pub use nsc_block::{BlockIndex, BlockLocator};
