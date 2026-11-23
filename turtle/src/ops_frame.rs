//turtle/src/ops_frame.rs

//! Frame-driven Turtle operations.
//!
//! This module is an adapter layer:
//! - `turtle::ledger` remains the custody substrate (append/scan rules, commit magic, no healing).
//! - This module maps CLI/ops into a **one-step-per-call** state machine using `nsc_frame`.
//!
//! Keep it boring:
//! - No hidden loops inside a step.
//! - Emit receipts (later) instead of printing.
//! - Preserve ledger invariants (footer magic is the only commit gate).

use std::fs::OpenOptions;
use std::path::PathBuf;

use crate::ledger::{Ledger, LEDGER_FILE_NAME};

use nsc_frame::{
    Driver, Frame, FrameState, FrameStepper, Receipt, StepOutcome, StepResult, StopReason,
};

/// Public operation request.
///
/// Keep ops declarative; effects happen via stepper steps.
#[derive(Debug, Clone)]
pub enum TurtleOp {
    /// Scan the committed suffix of the ledger file (read-only).
    ScanCommitted,

    /// Placeholder for future: append an already-formed frame payload.
    ///
    /// NOTE: The actual append must remain ledger-lawful:
    /// - write footer with zero magic
    /// - fsync
    /// - overwrite footer magic
    /// - fsync
    /// - only then ACK
    AppendBytes { payload: Vec<u8> },

    /// Append a key/value envelope as bytes.
    ///
    /// This is a v0 building block for object/facet semantics (e.g. head pointers).
    /// The ledger remains append-only; "head" semantics are defined by scanning for the latest
    /// committed envelope for a given key.
    AppendKeyed { key: String, value: Vec<u8> },

    /// Read a bounded view of the committed tail of the ledger.
    ///
    /// This is a v0 primitive. It does NOT mutate state.
    /// It returns summary receipts; payload decoding happens at higher layers.
    ReadTail { max_frames: u32, max_bytes: u32 },
}

/// Frame-owned memory for Turtle ops.
///
/// This is where we carry operation context across frame steps.
#[derive(Debug, Clone)]
pub struct TurtleMem {
    pub world_dir: PathBuf,
    pub op: TurtleOp,

    // Step-local progression
    pub phase: OpPhase,

    // Results (kept in-memory for now; later turn into receipts / outputs)
    pub committed_frames: u64,
    pub committed_start: u64,

    // Append results (for AppendBytes op)
    pub appended_csn: u64,
    pub appended_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpPhase {
    Start,
    Done,
}

impl TurtleMem {
    pub fn new(world_dir: PathBuf, op: TurtleOp) -> Self {
        Self {
            world_dir,
            op,
            phase: OpPhase::Start,
            committed_frames: 0,
            committed_start: 0,
            appended_csn: 0,
            appended_len: 0,
        }
    }
}

/// Stepper that advances Turtle operations by exactly one bounded semantic step.
#[derive(Debug, Default)]
pub struct TurtleStepper;

impl FrameStepper<TurtleMem> for TurtleStepper {
    #[allow(unused_imports)]
    fn step(&mut self, frame: &mut Frame<TurtleMem>) -> Result<StepResult, String> {
        // Hard stop conditions.
        match frame.state {
            FrameState::Cancelled => return Ok(StepResult::finished(StopReason::Cancelled)),
            FrameState::Finished => return Ok(StepResult::finished(StopReason::MaxTokens)),
            _ => {}
        }

        // One-step progression.
        match frame.mem.phase {
            OpPhase::Start => {
                match &frame.mem.op {
                    TurtleOp::ScanCommitted => {
                        // Wire into turtle::ledger scan (read-only semantics).
                        let ledger_path = frame.mem.world_dir.join(LEDGER_FILE_NAME);
                        let mut ledger = Ledger::open(&ledger_path)
                            .map_err(|e| format!("ledger open failed: {e:?}"))?;
                        let scan = ledger
                            .scan_committed()
                            .map_err(|e| format!("ledger scan_committed failed: {e:?}"))?;

                        // Persist results in frame-owned memory.
                        frame.mem.committed_frames = scan.committed_csn;
                        frame.mem.committed_start = scan.committed_start;

                        // Emit structured receipts (facts), not stdout.
                        let mut r = StepResult::advanced(None);
                        r.receipts.push(Receipt {
                            kind: "turtle.scan_committed.csn",
                            value_u64: scan.committed_csn,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.scan_committed.bytes",
                            value_u64: scan.committed_bytes,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.scan_committed.start",
                            value_u64: scan.committed_start,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.scan_committed.file_len",
                            value_u64: scan.file_len,
                        });

                        // This op completes in one step.
                        frame.mem.phase = OpPhase::Done;
                        frame.state = FrameState::Finished;
                        Ok(r)
                    }

                    TurtleOp::ReadTail {
                        max_frames,
                        max_bytes,
                    } => {
                        let ledger_path = frame.mem.world_dir.join(LEDGER_FILE_NAME);
                        let mut ledger = Ledger::open(&ledger_path)
                            .map_err(|e| format!("ledger open failed: {e:?}"))?;

                        let scan = ledger
                            .scan_committed()
                            .map_err(|e| format!("ledger scan_committed failed: {e:?}"))?;

                        // v0: bounded summary only (no frame decoding yet)
                        let frames = std::cmp::min(scan.committed_csn, *max_frames as u64);
                        let bytes = std::cmp::min(scan.committed_bytes, *max_bytes as u64);

                        let mut r = StepResult::advanced(None);
                        r.receipts.push(Receipt {
                            kind: "turtle.read_tail.frames",
                            value_u64: frames,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.read_tail.bytes",
                            value_u64: bytes,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.read_tail.total_csn",
                            value_u64: scan.committed_csn,
                        });

                        frame.mem.phase = OpPhase::Done;
                        frame.state = FrameState::Finished;
                        Ok(r)
                    }
                    TurtleOp::AppendBytes { payload } => {
                        // Best-effort: ensure world dir exists.
                        std::fs::create_dir_all(&frame.mem.world_dir)
                            .map_err(|e| format!("create_dir_all failed: {e}"))?;

                        let ledger_path = frame.mem.world_dir.join(LEDGER_FILE_NAME);

                        // Best-effort: ensure the ledger file exists and is not trivially small.
                        // Some fresh-world paths may create a 0-byte file, causing FileTooSmall on first append.
                        const MIN_LEDGER_BYTES: u64 = 4096;
                        {
                            let f = OpenOptions::new()
                                .read(true)
                                .write(true)
                                .create(true)
                                .open(&ledger_path)
                                .map_err(|e| format!("ledger preopen failed: {e}"))?;
                            let len = f
                                .metadata()
                                .map_err(|e| format!("ledger metadata failed: {e}"))?
                                .len();
                            if len < MIN_LEDGER_BYTES {
                                f.set_len(MIN_LEDGER_BYTES)
                                    .map_err(|e| format!("ledger prealloc failed: {e}"))?;
                            }
                        }

                        let mut ledger = Ledger::open(&ledger_path)
                            .map_err(|e| format!("ledger open failed: {e:?}"))?;

                        // The lawful commit gate is enforced by the ledger implementation.
                        // This frame step performs exactly one append request.
                        let csn = match ledger.append_batch(payload) {
                            Ok(csn) => csn,
                            Err(e) => {
                                let msg = format!("{e:?}");
                                if msg.contains("FileTooSmall") {
                                    // One retry after prealloc in case the ledger implementation requires a minimum size.
                                    let mut ledger2 = Ledger::open(&ledger_path)
                                        .map_err(|e| format!("ledger reopen failed: {e:?}"))?;
                                    ledger2
                                        .append_batch(payload)
                                        .map_err(|e| format!("ledger append_batch failed: {e:?}"))?
                                } else {
                                    return Err(format!("ledger append_batch failed: {e:?}"));
                                }
                            }
                        };

                        frame.mem.appended_csn = csn;
                        frame.mem.appended_len = payload.len() as u64;

                        let mut r = StepResult::advanced(None);
                        r.receipts.push(Receipt {
                            kind: "turtle.append.csn",
                            value_u64: csn,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.append.len",
                            value_u64: frame.mem.appended_len,
                        });

                        frame.mem.phase = OpPhase::Done;
                        frame.state = FrameState::Finished;
                        Ok(r)
                    }
                    TurtleOp::AppendKeyed { key, value } => {
                        // Build a tiny, deterministic envelope.
                        // Wire format (v0):
                        //  - 4 bytes: magic "TKV0"
                        //  - 4 bytes: key_len (u32 LE)
                        //  - N bytes: key UTF-8
                        //  - 4 bytes: value_len (u32 LE)
                        //  - M bytes: value
                        #[allow(unused_imports)]
                        use std::convert::TryInto;
                        let key_bytes = key.as_bytes();
                        let key_len_u32: u32 = key_bytes
                            .len()
                            .try_into()
                            .map_err(|_| "key too long".to_string())?;
                        let val_len_u32: u32 = value
                            .len()
                            .try_into()
                            .map_err(|_| "value too large".to_string())?;

                        let mut envelope =
                            Vec::with_capacity(4 + 4 + key_bytes.len() + 4 + value.len());
                        envelope.extend_from_slice(b"TKV0");
                        envelope.extend_from_slice(&key_len_u32.to_le_bytes());
                        envelope.extend_from_slice(key_bytes);
                        envelope.extend_from_slice(&val_len_u32.to_le_bytes());
                        envelope.extend_from_slice(value);

                        // Best-effort: ensure world dir exists.
                        std::fs::create_dir_all(&frame.mem.world_dir)
                            .map_err(|e| format!("create_dir_all failed: {e}"))?;

                        let ledger_path = frame.mem.world_dir.join(LEDGER_FILE_NAME);

                        // Best-effort: ensure the ledger file exists and is not trivially small.
                        // Some fresh-world paths may create a 0-byte file, causing FileTooSmall on first append.
                        const MIN_LEDGER_BYTES: u64 = 4096;
                        {
                            let f = OpenOptions::new()
                                .read(true)
                                .write(true)
                                .create(true)
                                .open(&ledger_path)
                                .map_err(|e| format!("ledger preopen failed: {e}"))?;
                            let len = f
                                .metadata()
                                .map_err(|e| format!("ledger metadata failed: {e}"))?
                                .len();
                            if len < MIN_LEDGER_BYTES {
                                f.set_len(MIN_LEDGER_BYTES)
                                    .map_err(|e| format!("ledger prealloc failed: {e}"))?;
                            }
                        }

                        let mut ledger = Ledger::open(&ledger_path)
                            .map_err(|e| format!("ledger open failed: {e:?}"))?;

                        // The lawful commit gate is enforced by the ledger implementation.
                        let csn = match ledger.append_batch(&envelope) {
                            Ok(csn) => csn,
                            Err(e) => {
                                let msg = format!("{e:?}");
                                if msg.contains("FileTooSmall") {
                                    let mut ledger2 = Ledger::open(&ledger_path)
                                        .map_err(|e| format!("ledger reopen failed: {e:?}"))?;
                                    ledger2
                                        .append_batch(&envelope)
                                        .map_err(|e| format!("ledger append_batch failed: {e:?}"))?
                                } else {
                                    return Err(format!("ledger append_batch failed: {e:?}"));
                                }
                            }
                        };

                        frame.mem.appended_csn = csn;
                        frame.mem.appended_len = envelope.len() as u64;

                        let mut r = StepResult::advanced(None);
                        r.receipts.push(Receipt {
                            kind: "turtle.append.csn",
                            value_u64: csn,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.append.len",
                            value_u64: frame.mem.appended_len,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.append.key_len",
                            value_u64: key_bytes.len() as u64,
                        });
                        r.receipts.push(Receipt {
                            kind: "turtle.append.value_len",
                            value_u64: value.len() as u64,
                        });

                        frame.mem.phase = OpPhase::Done;
                        frame.state = FrameState::Finished;
                        return Ok(r);
                    }
                }
            }
            OpPhase::Done => {
                frame.state = FrameState::Finished;
                Ok(StepResult::finished(StopReason::MaxTokens))
            }
        }
    }
}

/// Convenience runner for an op.
///
/// This is deliberately small. A CLI can call this and decide how to surface outputs.
pub fn run_op(world_dir: PathBuf, op: TurtleOp) -> Result<TurtleMem, String> {
    let mem = TurtleMem::new(world_dir, op);
    // max_tokens is not meaningful for Turtle ops; pick a small bound.
    let frame = Frame::new(mem, 64);
    let stepper = TurtleStepper::default();
    let mut driver = Driver::new(frame, stepper);

    loop {
        let out = driver.step()?;
        if matches!(out.outcome, StepOutcome::Finished) {
            break;
        }
    }

    Ok(driver.frame.mem)
}
