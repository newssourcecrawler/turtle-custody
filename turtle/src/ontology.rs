//turtle/src/ontology.rs
//! Five ordered event phases for Turtle.

use serde::{Deserialize, Serialize};

/// Five ordered event phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OntologyPhase {
    Gate1,
    Gate2,
    Gate3,
    Gate4,
    Gate5,
}

impl OntologyPhase {
    /// Returns true if `next` is a valid forward transition from `self`.
    ///
    /// The lifecycle is strictly linear:
    /// Gate1 → Gate2 → Gate3 → Gate4 → Gate5
    pub fn can_transition_to(self, next: OntologyPhase) -> bool {
        use OntologyPhase::*;
        matches!(
            (self, next),
            (Gate1, Gate2) | (Gate2, Gate3) | (Gate3, Gate4) | (Gate4, Gate5)
        )
    }
}

/// Return the canonical string representation of a phase.
pub fn phase_str(phase: OntologyPhase) -> &'static str {
    match phase {
        OntologyPhase::Gate1 => "Gate1",
        OntologyPhase::Gate2 => "Gate2",
        OntologyPhase::Gate3 => "Gate3",
        OntologyPhase::Gate4 => "Gate4",
        OntologyPhase::Gate5 => "Gate5",
    }
}

/// Format a brownout event key:
/// event/brownout/{job}/{step}/{phase}
pub fn brownout_key(job: &str, step: u32, phase: OntologyPhase) -> String {
    format!("event/brownout/{}/{:06}/{}", job, step, phase_str(phase))
}

/// Format a brownout event key with attempt suffix:
/// event/brownout/{job}/{step}/{phase}/{attempt}
pub fn brownout_key_attempt(job: &str, step: u32, phase: OntologyPhase, attempt: u32) -> String {
    format!(
        "event/brownout/{}/{:06}/{}/{:04}",
        job,
        step,
        phase_str(phase),
        attempt
    )
}
