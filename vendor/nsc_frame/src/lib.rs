//! nsc_frame
//!
//! Deterministic *frame-based* execution law.
//!
//! Modified for Turtle to use numbered gate identifiers.
//!
//! - A backend implements [`FrameStepper`] and performs exactly **one** bounded semantic step per call.
//! - The [`Driver`] owns the scheduling loop and calls the backend stepper.
//! - Every step returns a uniform [`StepResult`] envelope (no hidden loops).
//! - [`FrameLimits::max_step_ms`] can surface slow steps as a receipt (no semantic changes).
//! - Optional token bookkeeping lives in [`TokenTape`], so non-LLM frame uses stay clean.
//!

use std::time::Instant;
#[cfg(feature = "frame_arbiter")]
pub mod frame_arbiter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    Gate1,
    Gate2,
    Gate3,
    Gate4,
    Gate5,

    Finished,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Advanced,
    Yielded,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    MaxTokens,
    Eos,
    Cancelled,
    BackendError,
    Completed,
}

#[derive(Debug, Clone)]
pub struct Receipt {
    pub kind: &'static str,
    pub value_u64: u64,
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub outcome: StepOutcome,
    pub emitted_token: Option<u32>,
    pub stop_reason: Option<StopReason>,
    pub receipts: Vec<Receipt>,
}

impl StepResult {
    pub fn advanced(token: Option<u32>) -> Self {
        Self {
            outcome: StepOutcome::Advanced,
            emitted_token: token,
            stop_reason: None,
            receipts: Vec::new(),
        }
    }
    pub fn finished(reason: StopReason) -> Self {
        Self {
            outcome: StepOutcome::Finished,
            emitted_token: None,
            stop_reason: Some(reason),
            receipts: Vec::new(),
        }
    }
    pub fn yielded() -> Self {
        Self {
            outcome: StepOutcome::Yielded,
            emitted_token: None,
            stop_reason: None,
            receipts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FrameCursor {
    pub position: u32,
}

impl Default for FrameCursor {
    fn default() -> Self {
        Self { position: 0 }
    }
}

#[derive(Debug, Clone)]
pub struct FrameLimits {
    pub max_tokens: usize,

    /// Optional wall-clock budget for a single `Driver::step()` call.
    ///
    /// - `0` disables budgeting.
    /// - When enabled and the step exceeds the budget, the driver will append
    ///   a receipt `frame/step/budget_exceeded` with `value_u64 = elapsed_ms`.
    ///
    /// This surfaces overruns without changing step semantics.
    pub max_step_ms: u64,
}

#[derive(Debug, Clone)]
pub struct TokenTape {
    /// Prompt tokens owned by the frame for posterity-safe replay.
    pub prompt_token_ids: Vec<u32>,
    /// Cursor into `prompt_token_ids` during prefill.
    pub prompt_index: usize,

    /// Optional raw prompt text owned by the frame.
    pub prompt_text: Option<String>,

    /// Output log (token ids). Keep as a generic inspection surface.
    pub generated_token_ids: Vec<u32>,
    pub tokens_generated: usize,
}

impl Default for TokenTape {
    fn default() -> Self {
        Self {
            prompt_token_ids: Vec::new(),
            prompt_index: 0,
            prompt_text: None,
            generated_token_ids: Vec::new(),
            tokens_generated: 0,
        }
    }
}

impl TokenTape {
    pub fn with_prompt(prompt_token_ids: Vec<u32>) -> Self {
        Self {
            prompt_token_ids,
            prompt_text: None,
            ..Default::default()
        }
    }

    pub fn with_prompt_text(prompt_text: impl Into<String>) -> Self {
        Self {
            prompt_text: Some(prompt_text.into()),
            ..Default::default()
        }
    }

    pub fn prompt_text(&self) -> Option<&str> {
        self.prompt_text.as_deref()
    }

    /// Number of tokens generated so far.
    pub fn generated_len(&self) -> usize {
        self.generated_token_ids.len()
    }

    /// Last generated token, if any.
    pub fn last_token(&self) -> Option<u32> {
        self.generated_token_ids.last().copied()
    }

    /// Total token surface (prompt + generated).
    pub fn total_len(&self) -> usize {
        self.prompt_token_ids.len() + self.generated_token_ids.len()
    }

    /// Append a generated token and update bookkeeping.
    pub fn push_generated(&mut self, tok: u32) {
        self.generated_token_ids.push(tok);
        self.tokens_generated += 1;
    }
}

#[derive(Debug)]
pub struct Frame<M> {
    pub state: FrameState,
    pub cursor: FrameCursor,
    pub limits: FrameLimits,
    pub mem: M,

    /// Final stop reason recorded by the driver/backend when the frame finishes.
    ///
    /// This makes finish idempotent: once a stop reason is decided, subsequent
    /// calls can surface the same reason without guessing.
    pub stop_reason: Option<StopReason>,

    /// Wall-clock start time for this frame's run (set on first `Driver::step()`).
    ///
    /// Used to emit per-request end-to-end latency as a CDR metric.
    pub run_started_at: Option<Instant>,

    /// Optional LLM-style token bookkeeping (prompt + generated).
    ///
    /// Keeping this in a dedicated struct isolates token-specific concerns,
    /// while the frame law remains generic.
    pub tape: TokenTape,
}

impl<M> Frame<M> {
    pub fn new(mem: M, max_tokens: usize) -> Self {
        Self {
            state: FrameState::Gate1,
            cursor: FrameCursor::default(),
            limits: FrameLimits {
                max_tokens,
                max_step_ms: 0,
            },
            mem,
            stop_reason: None,
            run_started_at: None,
            tape: TokenTape::default(),
        }
    }

    pub fn cancel(&mut self) {
        if self.run_started_at.is_none() {
            self.run_started_at = Some(Instant::now());
        }
        self.state = FrameState::Cancelled;
        self.stop_reason = Some(StopReason::Cancelled);
    }

    pub fn with_prompt(mem: M, max_tokens: usize, prompt_token_ids: Vec<u32>) -> Self {
        Self {
            state: FrameState::Gate1,
            cursor: FrameCursor::default(),
            limits: FrameLimits {
                max_tokens,
                max_step_ms: 0,
            },
            mem,
            stop_reason: None,
            run_started_at: None,
            tape: TokenTape::with_prompt(prompt_token_ids),
        }
    }

    pub fn with_prompt_text(mem: M, max_tokens: usize, prompt_text: impl Into<String>) -> Self {
        Self {
            state: FrameState::Gate1,
            cursor: FrameCursor::default(),
            limits: FrameLimits {
                max_tokens,
                max_step_ms: 0,
            },
            mem,
            stop_reason: None,
            run_started_at: None,
            tape: TokenTape::with_prompt_text(prompt_text),
        }
    }

    pub fn generated_len(&self) -> usize {
        self.tape.generated_len()
    }

    pub fn last_token(&self) -> Option<u32> {
        self.tape.last_token()
    }

    pub fn total_len(&self) -> usize {
        self.tape.total_len()
    }

    pub fn prompt_text(&self) -> Option<&str> {
        self.tape.prompt_text()
    }

    pub fn choose_token<C: TokenChooser>(
        &self,
        chooser: &mut C,
        logits: &[f32],
    ) -> Result<u32, String> {
        chooser.choose(logits)
    }
}

/// Policy oracle. Must never execute. Called once per driver step.
pub trait Arbiter<M> {
    fn decide(&mut self, frame: &Frame<M>) -> Decision;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Yield,
    Refuse,
}

/// Frame-owned token selection policy.
///
/// Backends should surface logits; token choice belongs above them so the
/// runtime can later swap argmax, temperature, structural gates, or other
/// policies without changing backend execution code.
pub trait TokenChooser {
    fn choose(&mut self, logits: &[f32]) -> Result<u32, String>;
}

/// Minimal default chooser: greedy argmax.
#[derive(Debug, Default, Clone, Copy)]
pub struct ArgmaxChooser;

impl TokenChooser for ArgmaxChooser {
    fn choose(&mut self, logits: &[f32]) -> Result<u32, String> {
        if logits.is_empty() {
            return Err("chooser received empty logits".to_string());
        }

        let mut best_i = 0usize;
        let mut best_v = f32::NEG_INFINITY;

        for (i, &v) in logits.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best_i = i;
            }
        }

        Ok(best_i as u32)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoArbiter;

impl<M> Arbiter<M> for NoArbiter {
    fn decide(&mut self, _frame: &Frame<M>) -> Decision {
        Decision::Allow
    }
}

/// Backend stepper: does exactly one bounded semantic step.
pub trait FrameStepper<M> {
    fn step(&mut self, frame: &mut Frame<M>) -> Result<StepResult, String>;
}

/// Driver owns the loop (scheduling). Backend owns one-step execution.
pub struct Driver<M, S, A = NoArbiter>
where
    S: FrameStepper<M>,
    A: Arbiter<M>,
{
    pub frame: Frame<M>,
    pub stepper: S,
    pub arbiter: A,
}

impl<M, S> Driver<M, S, NoArbiter>
where
    S: FrameStepper<M>,
{
    pub fn new(frame: Frame<M>, stepper: S) -> Self {
        Self {
            frame,
            stepper,
            arbiter: NoArbiter,
        }
    }
}

impl<M, S, A> Driver<M, S, A>
where
    S: FrameStepper<M>,
    A: Arbiter<M>,
{
    pub fn with_arbiter(frame: Frame<M>, stepper: S, arbiter: A) -> Self {
        Self {
            frame,
            stepper,
            arbiter,
        }
    }

    pub fn step(&mut self) -> Result<StepResult, String> {
        match self.frame.state {
            FrameState::Finished => {
                return Ok(StepResult::finished(
                    self.frame.stop_reason.unwrap_or(StopReason::BackendError),
                ))
            }
            FrameState::Cancelled => {
                return Ok(StepResult::finished(
                    self.frame.stop_reason.unwrap_or(StopReason::Cancelled),
                ))
            }
            _ => {}
        }

        if self.frame.run_started_at.is_none() {
            self.frame.run_started_at = Some(Instant::now());
        }

        let t0 = Instant::now();

        let mut r = match self.arbiter.decide(&self.frame) {
            Decision::Allow => self.stepper.step(&mut self.frame)?,
            Decision::Yield => {
                let mut r = StepResult::yielded();
                r.receipts.push(Receipt {
                    kind: "arbiter.yield",
                    value_u64: 1,
                });
                r
            }
            Decision::Refuse => {
                self.frame.cancel();
                StepResult::finished(StopReason::Cancelled)
            }
        };

        if let Some(tok) = r.emitted_token {
            self.frame.tape.push_generated(tok);
            self.frame.cursor.position = self.frame.cursor.position.saturating_add(1);

            if self.frame.tape.generated_len() >= self.frame.limits.max_tokens {
                r.outcome = StepOutcome::Finished;
                r.stop_reason = Some(StopReason::MaxTokens);
                r.receipts.push(Receipt {
                    kind: "frame/limit/max_tokens",
                    value_u64: self.frame.tape.generated_len() as u64,
                });
            }
        }

        if r.outcome == StepOutcome::Finished {
            if let Some(sr) = r.stop_reason {
                self.frame.stop_reason = Some(sr);
                if sr == StopReason::Cancelled {
                    self.frame.state = FrameState::Cancelled;
                } else {
                    self.frame.state = FrameState::Finished;
                }
            } else {
                // Defensive: a Finished outcome should carry a stop reason.
                self.frame.stop_reason = Some(StopReason::BackendError);
                self.frame.state = FrameState::Finished;
            }
            // Emit per-request end-to-end latency once (CDR).
            if !r.receipts.iter().any(|x| x.kind == "cdr.run.ns_total") {
                if let Some(t_start) = self.frame.run_started_at {
                    let ns_total = t_start.elapsed().as_nanos() as u64;
                    r.receipts.push(Receipt {
                        kind: "cdr.run.ns_total",
                        value_u64: ns_total,
                    });
                }
            }
        }

        let max_step_ms = self.frame.limits.max_step_ms;
        if max_step_ms != 0 {
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            if elapsed_ms > max_step_ms {
                r.receipts.push(Receipt {
                    kind: "frame/step/budget_exceeded",
                    value_u64: elapsed_ms,
                });
            }
        }

        Ok(r)
    }

    pub fn run_until_blocked(&mut self) -> Result<StepOutcome, String> {
        loop {
            let r = self.step()?;

            match r.outcome {
                StepOutcome::Advanced => {}
                StepOutcome::Yielded => return Ok(StepOutcome::Yielded),
                StepOutcome::Finished => return Ok(StepOutcome::Finished),
            }
        }
    }

    pub fn run_to_completion(&mut self) -> Result<(), String> {
        match self.run_until_blocked()? {
            StepOutcome::Advanced => {
                unreachable!("run_until_blocked never returns Advanced")
            }
            StepOutcome::Yielded => {
                Err("driver yielded before completion; resume with run_until_blocked()".to_string())
            }
            StepOutcome::Finished => Ok(()),
        }
    }
}

/// A tiny noop backend (public-friendly): proves the law compiles and runs.
#[derive(Debug, Default)]
pub struct NoopStepper;

#[derive(Debug, Default)]
pub struct NoopMem;

impl FrameStepper<NoopMem> for NoopStepper {
    fn step(&mut self, frame: &mut Frame<NoopMem>) -> Result<StepResult, String> {
        match frame.state {
            FrameState::Gate1 => {
                frame.state = FrameState::Gate2;
                Ok(StepResult::advanced(None))
            }
            FrameState::Gate2 => {
                frame.state = FrameState::Gate3;
                Ok(StepResult::advanced(None))
            }
            FrameState::Gate3 => {
                frame.state = FrameState::Gate4;
                Ok(StepResult {
                    outcome: StepOutcome::Advanced,
                    emitted_token: None,
                    stop_reason: None,
                    receipts: vec![Receipt {
                        kind: "phase.Gate3",
                        value_u64: 1,
                    }],
                })
            }
            FrameState::Gate4 => {
                // “Generate” a deterministic token id (toy).
                let tok = (frame.cursor.position % 256) as u32;
                Ok(StepResult::advanced(Some(tok)))
            }
            FrameState::Gate5 => {
                frame.state = FrameState::Finished;
                frame.stop_reason = Some(StopReason::Completed);
                Ok(StepResult::finished(StopReason::Completed))
            }
            FrameState::Finished => Ok(StepResult::finished(StopReason::MaxTokens)),
            FrameState::Cancelled => Ok(StepResult::finished(StopReason::Cancelled)),
        }
    }
}
