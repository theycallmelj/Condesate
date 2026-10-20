//! Real integration with AWS's
//! [`strands-agents-evals`](https://github.com/strands-agents/evals): starts
//! the same eval target server (`evals::server`) used by the `harness-evals`
//! integration, then shells out to `python3 strands_eval.py`, which grades
//! the real HTTP responses with the real, pip-installed
//! `strands_evals.Experiment` and its deterministic evaluators
//! (`Equals`, `Contains`, `ToolCalled`) — no LLM judge, no reimplementation.
//!
//! Requires `strands-agents-evals` on the `python3` used
//! (`pip install strands-agents-evals`); see `crates/evals/README.md`.

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::sleep;

const STRANDS_EVALS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/strands_evals");

#[tokio::main]
async fn main() -> Result<()> {
    let addr = std::env::var("EVAL_TARGET_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let script = std::env::var("STRANDS_EVALS_SCRIPT").unwrap_or_else(|_| "strands_eval.py".to_string());

    let server_addr = addr.clone();
    tokio::spawn(async move {
        if let Err(e) = evals::server::serve(&server_addr).await {
            eprintln!("eval target server error: {e}");
        }
    });

    let mut ready = false;
    for _ in 0..50 {
        if TcpStream::connect(&addr).await.is_ok() {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    if !ready {
        bail!("eval target server never became ready on {addr}");
    }

    println!("eval target ready at http://{addr}/run");
    println!("running: python3 {script}  (cwd: {STRANDS_EVALS_DIR})\n");

    let status = Command::new("python3")
        .arg(&script)
        .current_dir(STRANDS_EVALS_DIR)
        .status()
        .await
        .context(
            "failed to run `python3` — is `strands-agents-evals` installed? \
             (pip install strands-agents-evals)",
        )?;

    std::process::exit(status.code().unwrap_or(1));
}
