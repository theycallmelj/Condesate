//! `CaseOutcome` — the `harness-evals` `EvalCase` analogue: a golden case
//! enriched with what actually happened when it ran against the real
//! `condesate` agent loop.

/// What a case's `run` function reports back, in enough detail for every
/// metric in `crate::metrics` to grade it without re-running anything.
#[derive(Clone, Debug, Default)]
pub struct CaseOutcome {
    /// The loop's final assistant text.
    pub final_text: String,
    /// Every tool call that actually reached `Tool::call`, in order, paired
    /// with its observation text.
    pub tool_observations: Vec<(String, String)>,
    /// Tool names refused by `GuardedServices::authorize_tool` before the
    /// tool body ever ran.
    pub denied_tools: Vec<String>,
    pub elapsed_ms: u64,
    pub steps: usize,
    /// Result of a case-specific side-effect check (e.g. did a storage
    /// write really land), when the case's `Expectation` is `PostCheck`.
    pub post_check: Option<bool>,
}

impl CaseOutcome {
    /// Final text plus every tool observation, concatenated — what a
    /// `Contains`/`WindowContains` metric searches.
    pub fn haystack(&self) -> String {
        let mut s = self.final_text.clone();
        for (_, obs) in &self.tool_observations {
            s.push('\n');
            s.push_str(obs);
        }
        s
    }
}
