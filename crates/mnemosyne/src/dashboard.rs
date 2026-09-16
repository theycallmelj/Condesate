//! Launches `dashboard/`'s Vite dev server and opens it in the default
//! browser when `--debug` is passed — see `main.rs`. A dev convenience, not something
//! the rest of mnemosyne depends on: a failure here is logged and mnemosyne
//! keeps running without it, same as any other opt-in diagnostic.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

/// Resolved at compile time from the crate's own manifest location, so this
/// works regardless of the process's runtime working directory — whether
/// `cargo run -p mnemosyne` is invoked from the workspace root or from
/// inside this crate.
const DASHBOARD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/dashboard");

fn dashboard_dir() -> PathBuf {
    PathBuf::from(DASHBOARD_DIR)
}

async fn ensure_installed() -> Result<()> {
    if dashboard_dir().join("node_modules").is_dir() {
        return Ok(());
    }
    eprintln!("[mnemosyne] dashboard/ has no node_modules yet — running `npm install` once (this may take a moment)...");
    // stdin nulled for the same reason `launch` nulls it on the dev-server
    // child below — sharing stdin with an npm/node process risks it leaking
    // a non-blocking fd flag change back to this process's own blocking
    // reads. stdout/stderr stay inherited here (unlike the dev server) so
    // install progress/errors are visible on a step the user is waiting on.
    let status = Command::new("npm")
        .arg("install")
        .current_dir(dashboard_dir())
        .stdin(Stdio::null())
        .status()
        .await
        .context("failed to run `npm install` in dashboard/ — is Node.js/npm installed?")?;
    if !status.success() {
        anyhow::bail!("`npm install` in dashboard/ exited with {status}");
    }
    Ok(())
}

async fn port_accepting_connections(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok()
}

fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/C", "start", url]).status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if let Err(e) = result {
        eprintln!("[mnemosyne] couldn't auto-open a browser ({e}) — open {url} manually");
    }
}

/// Installs dashboard deps if needed, starts its Vite dev server on `port`
/// (stdout/stderr suppressed — a continuously-printing dev server would
/// otherwise interleave with mnemosyne's own `you>` prompt), waits up to ~5s
/// for it to actually accept connections, then opens it in the default
/// browser with `?api=<api_base>` so it points at the right port even when
/// `API_PORT` isn't the default. Returns the child process — the caller owns
/// killing it on exit, see [`shutdown`].
pub async fn launch(port: u16, api_base: &str) -> Result<Child> {
    ensure_installed().await?;

    let child = Command::new("npm")
        .arg("run")
        .arg("dev")
        .arg("--")
        .arg("--port")
        .arg(port.to_string())
        .current_dir(dashboard_dir())
        // Explicitly `null`, not inherited: a `Command` with `.stdin`
        // unset shares the parent's stdin fd with the child by default, and
        // Node/npm reconfiguring that shared fd (observed: into
        // non-blocking mode) leaks back to *this* process's own blocking
        // `stdin.read_line()` in the chat loop, breaking it with `EAGAIN`
        // ("Resource temporarily unavailable") the moment the child starts.
        // Live-verified bug, not a hypothetical.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start `npm run dev` in dashboard/ — is Node.js/npm installed?")?;

    for _ in 0..20 {
        if port_accepting_connections(port).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let url = format!("http://localhost:{port}/?api={api_base}");
    eprintln!("[mnemosyne] dashboard: {url}");
    open_browser(&url);
    Ok(child)
}

/// Best-effort teardown. `child.kill()` alone isn't always enough: `npm run
/// dev` spawns the actual vite/node process as its own child, and whether
/// killing `npm` propagates to it depends on platform and npm version — so
/// this also kills whatever's still listening on `port` directly, ignoring
/// any error (nothing to do if that fails; `child.kill()` already tried).
pub async fn shutdown(mut child: Child, port: u16) {
    let _ = child.kill().await;
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("lsof -ti:{port} | xargs -r kill 2>/dev/null"))
            .status();
    }
}
