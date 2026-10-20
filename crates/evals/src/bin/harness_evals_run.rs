//! The real integration: starts the eval target server (`evals::server`) in
//! the background, waits for it to accept connections, then shells out to
//! the actual `harness-evals` CLI — <https://github.com/harness/harness-evals>,
//! not a reimplementation of it — against `harness_evals/condesate.eval.yaml`.
//! `harness-evals` itself prints its own table and writes
//! `harness_evals/results.jsonl` per that config's `sinks:`.
//!
//! Requires `harness-evals` on `PATH` (`pip install harness-evals`); see
//! `crates/evals/README.md`.

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::sleep;

const HARNESS_EVALS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/harness_evals");

#[tokio::main]
async fn main() -> Result<()> {
    let addr = std::env::var("EVAL_TARGET_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let config = std::env::var("HARNESS_EVALS_CONFIG").unwrap_or_else(|_| "condesate.eval.yaml".to_string());

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
    println!("running: harness-evals run {config}  (cwd: {HARNESS_EVALS_DIR})");
    println!();

    let status = Command::new("harness-evals")
        .arg("run")
        .arg(&config)
        .current_dir(HARNESS_EVALS_DIR)
        .status()
        .await
        .context(
            "failed to run `harness-evals` — is it installed and on PATH? \
             (pip install harness-evals)",
        )?;

    std::process::exit(status.code().unwrap_or(1));
}
