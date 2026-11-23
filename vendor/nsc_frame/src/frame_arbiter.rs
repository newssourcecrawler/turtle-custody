//! FrameArbiter
//!
//! A deliberately small “Casio” policy helper for frame runtimes.
//!
//! It watches only the frame-law receipt:
//! - `frame/step/budget_exceeded`
//!
//! And keeps exactly one piece of state:
//! - `budget_streak`
//!
//! Then it maps streak → decision:
//! - streak < soft_n  => Allow
//! - soft_n <= streak < hard_n => Yield
//! - streak >= hard_n => Refuse
//!
//! This module does **not** depend on Tokio and does **not** attempt to measure time.
//! The driver measures time and surfaces it as a receipt (via `FrameLimits::max_step_ms`).

use crate::{Decision, StepResult};

/// Receipt kind emitted by `nsc_frame::Driver` when a step exceeds `max_step_ms`.
pub const FRAME_BUDGET_EXCEEDED_KIND: &str = "frame/step/budget_exceeded";

/// Configuration for [`FrameArbiter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameArbiterCfg {
    /// Start yielding after this many consecutive budget-exceeded steps.
    pub soft_n: u32,
    /// Refuse (cancel) after this many consecutive budget-exceeded steps.
    pub hard_n: u32,
}

impl Default for FrameArbiterCfg {
    fn default() -> Self {
        // Conservative defaults; tune at the integration layer.
        Self {
            soft_n: 3,
            hard_n: 8,
        }
    }
}

/// Stateful frame budget arbiter.
///
/// The integration pattern is intentionally simple:
/// - call [`FrameArbiter::observe_step`] once per completed step (using the step's receipts)
/// - call [`FrameArbiter::decide_next`] to decide the next scheduling action
#[derive(Debug, Default, Clone)]
pub struct FrameArbiter {
    cfg: FrameArbiterCfg,
    budget_streak: u32,
}

impl FrameArbiter {
    /// Create with explicit configuration.
    pub fn new(cfg: FrameArbiterCfg) -> Self {
        Self {
            cfg,
            budget_streak: 0,
        }
    }

    /// Current consecutive budget-exceeded streak.
    pub fn budget_streak(&self) -> u32 {
        self.budget_streak
    }

    /// Observe the outcome of a completed step.
    ///
    /// If the step contains `frame/step/budget_exceeded`, the streak increments.
    /// Otherwise, the streak resets to 0.
    pub fn observe_step(&mut self, step: &StepResult) {
        if has_budget_exceeded(step) {
            self.budget_streak = self.budget_streak.saturating_add(1);
        } else {
            self.budget_streak = 0;
        }
    }

    /// Decide what to do on the next tick, based on the current streak.
    pub fn decide_next(&self) -> Decision {
        let s = self.budget_streak;
        if s >= self.cfg.hard_n {
            Decision::Refuse
        } else if s >= self.cfg.soft_n {
            Decision::Yield
        } else {
            Decision::Allow
        }
    }

    /// Convenience: observe a step, then decide the next action.
    pub fn observe_and_decide(&mut self, step: &StepResult) -> Decision {
        self.observe_step(step);
        self.decide_next()
    }

    /// Reset internal streak state.
    pub fn reset(&mut self) {
        self.budget_streak = 0;
    }
}

#[inline]
fn has_budget_exceeded(step: &StepResult) -> bool {
    step.receipts
        .iter()
        .any(|r| r.kind == FRAME_BUDGET_EXCEEDED_KIND)
}
