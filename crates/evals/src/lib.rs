//! An evaluation harness for `condesate`, in the spirit of
//! [harness-evals](https://github.com/harness/harness-evals) and the tools
//! indexed in
//! [Awesome-AI-Evaluations-Tools](https://github.com/danielrosehill/Awesome-AI-Evaluations-Tools).
//! See `docs/references.md` and `crates/evals/README.md`.
//!
//! Two independent ways to evaluate `condesate`, sharing the modules below:
//!
//! * **`bin/evals.rs`** (`cargo run -p evals`) — a self-authored native suite
//!   (`golden`/`outcome`/`metrics`/`report`/`suite`) that drives the real
//!   agent loop directly, in-process, with full access to tool observations
//!   and denials. No external dependency; this is what `harness-evals`
//!   itself calls a `PromptTarget`/direct API eval.
//! * **`bin/eval_target_server.rs`** + **`bin/harness_evals_run.rs`**
//!   (`server`) — a real integration: the server exposes the same real agent
//!   loop over HTTP as a `harness-evals` `HttpTarget`
//!   (`{"input": ...} -> {"output": ...}`), and the runner shells out to the
//!   actual, pip-installed `harness-evals` CLI against
//!   `harness_evals/condesate.eval.yaml`, using its real deterministic
//!   metrics (`contains`, `latency`). This is the literal integration of the
//!   linked project, not a reimplementation of its vocabulary.

pub mod golden;
pub mod harness;
pub mod metrics;
pub mod outcome;
pub mod report;
pub mod server;
pub mod suite;
