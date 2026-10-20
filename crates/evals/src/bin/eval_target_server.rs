//! Stand-alone eval target server: `cargo run -p evals --bin eval_target_server`.
//! See `evals::server` for what it actually serves and why.
//!
//! Runs until killed (Ctrl-C) or the listener errors. Normally you don't run
//! this directly — `harness_evals_run` starts and stops it for you — but it's
//! useful on its own for pointing `harness-evals run` (or `curl`) at manually.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("EVAL_TARGET_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    evals::server::serve(&addr).await
}
