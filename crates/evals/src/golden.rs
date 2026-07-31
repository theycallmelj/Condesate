//! `Golden` and `Expectation` — what an eval case is, borrowed directly from
//! `harness-evals`' vocabulary (a `Golden` is what you author; a metric turns
//! it plus the actual run into a `Score`) and its five-dimension framing
//! (Correctness / Trajectory / Safety / Performance / Groundedness). See
//! `docs/references.md`.
//!
//! Dimensions are set by the metric that measures a case, not chosen per
//! case — same rule `harness-evals` uses, so a case's dimension can't drift
//! from what actually graded it.

use crate::outcome::CaseOutcome;
use std::future::Future;
use std::pin::Pin;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dimension {
    Correctness,
    Trajectory,
    Safety,
    Performance,
    Groundedness,
}

impl Dimension {
    pub fn label(&self) -> &'static str {
        match self {
            Dimension::Correctness => "correctness",
            Dimension::Trajectory => "trajectory",
            Dimension::Safety => "safety",
            Dimension::Performance => "performance",
            Dimension::Groundedness => "groundedness",
        }
    }
}

/// What a case is expected to demonstrate. Each variant is measured by
/// exactly one metric in `crate::metrics` today; the split exists so a
/// stricter or additional metric can be registered against the same
/// variant later without touching cases.
#[derive(Clone, Debug)]
pub enum Expectation {
    /// The final text or a tool observation contains this substring.
    Contains(&'static str),
    /// A case-specific side effect (e.g. a storage write) landed.
    PostCheck,
    /// This tool was actually invoked and returned a non-error result.
    ToolSucceeded(&'static str),
    /// This tool call was refused by the permission boundary before it
    /// reached the tool body.
    ToolDenied(&'static str),
    /// The activation completed within this budget.
    MaxLatencyMs(u64),
    /// A composed retrieval window contains this substring — proof that
    /// context survived retrieval rather than being reduced to a bare fact.
    WindowContains(&'static str),
}

pub type CaseFuture = Pin<Box<dyn Future<Output = anyhow::Result<CaseOutcome>> + Send>>;

/// One eval case: an id, a human description, what it expects, and the
/// function that actually drives `condesate` to produce a `CaseOutcome`.
pub struct Golden {
    pub id: &'static str,
    pub description: &'static str,
    pub expectation: Expectation,
    pub run: fn() -> CaseFuture,
}
