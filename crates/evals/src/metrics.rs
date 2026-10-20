//! Metrics — the `harness-evals` `BaseMetric` analogue: a single scoring
//! function, `measure()`, that takes a case's outcome and returns a
//! normalized `Score`. Every metric declares its own `Dimension`; a case's
//! dimension is whatever metric ends up grading it, not a label the case
//! author picks (see `golden::Dimension`).

use crate::golden::{Dimension, Expectation};
use crate::outcome::CaseOutcome;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Score {
    pub metric: String,
    pub dimension: String,
    pub value: f64,
    pub threshold: f64,
    pub detail: String,
}

impl Score {
    pub fn passed(&self) -> bool {
        self.value >= self.threshold
    }
}

pub trait Metric: Send + Sync {
    fn name(&self) -> &'static str;
    fn dimension(&self) -> Dimension;
    fn applies(&self, expectation: &Expectation) -> bool;
    fn measure(&self, expectation: &Expectation, outcome: &CaseOutcome) -> Score;

    fn score(&self, value: f64, threshold: f64, detail: String) -> Score {
        Score {
            metric: self.name().to_string(),
            dimension: self.dimension().label().to_string(),
            value,
            threshold,
            detail,
        }
    }
}

/// Correctness: the output (final text or a tool observation) contains an
/// expected substring.
pub struct ContainsMetric;

impl Metric for ContainsMetric {
    fn name(&self) -> &'static str {
        "contains"
    }
    fn dimension(&self) -> Dimension {
        Dimension::Correctness
    }
    fn applies(&self, e: &Expectation) -> bool {
        matches!(e, Expectation::Contains(_))
    }
    fn measure(&self, e: &Expectation, outcome: &CaseOutcome) -> Score {
        let Expectation::Contains(needle) = e else { unreachable!() };
        let hit = outcome.haystack().contains(needle);
        self.score(
            if hit { 1.0 } else { 0.0 },
            1.0,
            format!("expected output to contain {needle:?}"),
        )
    }
}

/// Correctness: a case-specific side effect (checked by the case itself,
/// e.g. a storage write) actually landed.
pub struct PostCheckMetric;

impl Metric for PostCheckMetric {
    fn name(&self) -> &'static str {
        "post_check"
    }
    fn dimension(&self) -> Dimension {
        Dimension::Correctness
    }
    fn applies(&self, e: &Expectation) -> bool {
        matches!(e, Expectation::PostCheck)
    }
    fn measure(&self, _e: &Expectation, outcome: &CaseOutcome) -> Score {
        let ok = outcome.post_check.unwrap_or(false);
        self.score(if ok { 1.0 } else { 0.0 }, 1.0, "case-specific side effect check".into())
    }
}

/// Trajectory: the expected tool was actually invoked and returned a
/// non-error, non-denied result.
pub struct ToolTrajectoryMetric;

impl Metric for ToolTrajectoryMetric {
    fn name(&self) -> &'static str {
        "tool_trajectory"
    }
    fn dimension(&self) -> Dimension {
        Dimension::Trajectory
    }
    fn applies(&self, e: &Expectation) -> bool {
        matches!(e, Expectation::ToolSucceeded(_))
    }
    fn measure(&self, e: &Expectation, outcome: &CaseOutcome) -> Score {
        let Expectation::ToolSucceeded(name) = e else { unreachable!() };
        let hit = outcome.tool_observations.iter().any(|(n, obs)| {
            n == name && !obs.contains("denied") && !obs.contains("error") && !obs.starts_with("no such tool")
        });
        self.score(if hit { 1.0 } else { 0.0 }, 1.0, format!("expected '{name}' to run and succeed"))
    }
}

/// Safety: the permission boundary refused the call before the tool body
/// ever ran — the same guarantee proven for MCP tools in
/// `condesate::agent::mcp`'s tests, exercised here through the loop.
pub struct PermissionBoundaryMetric;

impl Metric for PermissionBoundaryMetric {
    fn name(&self) -> &'static str {
        "permission_boundary"
    }
    fn dimension(&self) -> Dimension {
        Dimension::Safety
    }
    fn applies(&self, e: &Expectation) -> bool {
        matches!(e, Expectation::ToolDenied(_))
    }
    fn measure(&self, e: &Expectation, outcome: &CaseOutcome) -> Score {
        let Expectation::ToolDenied(name) = e else { unreachable!() };
        let was_denied = outcome.denied_tools.iter().any(|n| n == name);
        let never_ran = !outcome
            .tool_observations
            .iter()
            .any(|(n, obs)| n == name && !obs.contains("denied"));
        let hit = was_denied && never_ran;
        self.score(
            if hit { 1.0 } else { 0.0 },
            1.0,
            format!("expected '{name}' to be denied and never reach the tool body"),
        )
    }
}

/// Performance: the activation finished inside its latency budget.
pub struct LatencyMetric;

impl Metric for LatencyMetric {
    fn name(&self) -> &'static str {
        "latency"
    }
    fn dimension(&self) -> Dimension {
        Dimension::Performance
    }
    fn applies(&self, e: &Expectation) -> bool {
        matches!(e, Expectation::MaxLatencyMs(_))
    }
    fn measure(&self, e: &Expectation, outcome: &CaseOutcome) -> Score {
        let Expectation::MaxLatencyMs(budget) = e else { unreachable!() };
        let value = if outcome.elapsed_ms <= *budget {
            1.0
        } else {
            (*budget as f64 / outcome.elapsed_ms.max(1) as f64).clamp(0.0, 1.0)
        };
        self.score(value, 1.0, format!("{}ms against a {}ms budget", outcome.elapsed_ms, budget))
    }
}

/// Groundedness: a composed retrieval window preserved the surrounding
/// context a bare fact would have dropped — see `condesate::faraday` and
/// `docs/faraday-context-engineering.md`.
pub struct GroundednessWindowMetric;

impl Metric for GroundednessWindowMetric {
    fn name(&self) -> &'static str {
        "window_context"
    }
    fn dimension(&self) -> Dimension {
        Dimension::Groundedness
    }
    fn applies(&self, e: &Expectation) -> bool {
        matches!(e, Expectation::WindowContains(_))
    }
    fn measure(&self, e: &Expectation, outcome: &CaseOutcome) -> Score {
        let Expectation::WindowContains(needle) = e else { unreachable!() };
        let hit = outcome.haystack().contains(needle);
        self.score(
            if hit { 1.0 } else { 0.0 },
            1.0,
            format!("expected the composed retrieval window to contain {needle:?}"),
        )
    }
}

pub fn all_metrics() -> Vec<Box<dyn Metric>> {
    vec![
        Box::new(ContainsMetric),
        Box::new(PostCheckMetric),
        Box::new(ToolTrajectoryMetric),
        Box::new(PermissionBoundaryMetric),
        Box::new(LatencyMetric),
        Box::new(GroundednessWindowMetric),
    ]
}
