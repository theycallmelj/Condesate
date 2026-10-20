//! A read-only diagnostic HTTP API over mnemosyne's live state — what a
//! browser-based dashboard (see `dashboard/`) polls to show every agent
//! running, everything `faraday` currently holds, and the audit trail.
//!
//! Hand-rolled rather than pulling in an HTTP framework, the same call
//! `crates/evals/src/server.rs` already made for this workspace and the same
//! reasoning: a handful of fixed `GET` routes with no request body doesn't
//! need one, and the workspace otherwise stays dependency-light. `.await`s
//! `MemoryBank`/`GuardedServices`/`MemoryAudit` directly — nothing here
//! mutates anything, so there's no permission boundary to cross (every
//! *write* to memory already goes through morpheus's own guarded tools; this
//! only ever reads).
//!
//! `Access-Control-Allow-Origin: *` is sent unconditionally so a dashboard
//! served from a different origin (a Vite dev server on another port) can
//! call this directly. Fine for a local diagnostic tool talked to from
//! `localhost`; tighten it before ever exposing this beyond that.

use anyhow::Result;
use condesate::{Diary, GuardedServices, IdeaBook, Menu, SlipIndex};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::memory::MemoryBank;

async fn agents_json(services: &GuardedServices) -> Result<Value> {
    let agents = services.list_agents().await?;
    Ok(json!(agents
        .iter()
        .map(|a| json!({
            "uid": a.uid.to_string(),
            "name": a.name,
            "harness": a.harness.to_string(),
            "parent": a.parent.map(|p| p.to_string()),
            "tenant": a.tenant.to_string(),
            "trust": format!("{:?}", a.trust),
            "spawned_at_ms": a.spawned_at_ms,
        }))
        .collect::<Vec<_>>()))
}

async fn diary_json(bank: &MemoryBank) -> Result<Value> {
    let len = bank.diary.len().await?;
    let mut entries = Vec::with_capacity(len as usize);
    for seq in 1..=len {
        if let Some(e) = bank.diary.entry(seq).await? {
            entries.push(json!({
                "seq": e.seq,
                "at_ms": e.at_ms,
                "harness": e.harness.to_string(),
                "activation": e.activation,
                "kind": format!("{:?}", e.kind),
                "content": e.content,
                "refs": e.refs,
            }));
        }
    }
    Ok(json!(entries))
}

async fn topics_json(bank: &MemoryBank) -> Result<Value> {
    Ok(json!(bank.slips.topics().await?))
}

async fn slips_json(bank: &MemoryBank) -> Result<Value> {
    let mut out = Vec::new();
    for topic in bank.slips.topics().await? {
        for slip in bank.slips.by_topic(&topic).await? {
            out.push(json!({ "descriptor": slip.descriptor, "topic": slip.topic, "refs": slip.refs }));
        }
    }
    Ok(json!(out))
}

async fn ideas_json(bank: &MemoryBank) -> Result<Value> {
    let mut out = Vec::new();
    for topic in bank.ideas.topics().await? {
        for s in bank.ideas.by_topic(&topic).await? {
            out.push(json!({
                "id": s.id,
                "topic": s.topic,
                "content": s.content,
                "struck": s.struck,
                "supersedes": s.supersedes,
            }));
        }
    }
    Ok(json!(out))
}

async fn audit_json(audit: &condesate::MemoryAudit) -> Result<Value> {
    let events = audit.events().await;
    Ok(json!(events
        .iter()
        .map(|e| json!({
            "seq": e.seq,
            "at_ms": e.at_ms,
            "principal": {
                "uid": e.principal.uid,
                "harness": e.principal.harness,
                "agent": e.principal.agent,
                "tenant": e.principal.tenant,
                "trust": e.principal.trust,
            },
            "action": format!("{:?}", e.action),
            "resource": e.resource,
            "decision": format!("{:?}", e.decision),
            "outcome": format!("{:?}", e.outcome),
            "activation": e.activation,
        }))
        .collect::<Vec<_>>()))
}

struct State {
    services: std::sync::Arc<GuardedServices>,
    bank: MemoryBank,
    audit: std::sync::Arc<condesate::MemoryAudit>,
}

async fn route(path: &str, state: &State) -> Result<Value> {
    match path {
        "/api/health" => Ok(json!({ "status": "ok" })),
        "/api/agents" => agents_json(&state.services).await,
        "/api/memory/diary" => diary_json(&state.bank).await,
        "/api/memory/topics" => topics_json(&state.bank).await,
        "/api/memory/slips" => slips_json(&state.bank).await,
        "/api/memory/ideas" => ideas_json(&state.bank).await,
        "/api/audit" => audit_json(&state.audit).await,
        _ => anyhow::bail!("no such route"),
    }
}

async fn handle(stream: TcpStream, state: std::sync::Arc<State>) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // Drain headers (and any body, for completeness — every real route here
    // is GET/OPTIONS with none, but a client that sends one shouldn't hang).
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header).await?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;
    }

    let (status, reason, payload) = if method == "OPTIONS" {
        (204, "No Content", String::new())
    } else if method == "GET" {
        match route(&path, &state).await {
            Ok(v) => (200, "OK", v.to_string()),
            Err(e) if e.to_string() == "no such route" => {
                (404, "Not Found", json!({ "error": "no such route" }).to_string())
            }
            Err(e) => (500, "Internal Server Error", json!({ "error": e.to_string() }).to_string()),
        }
    } else {
        (405, "Method Not Allowed", json!({ "error": "only GET is supported" }).to_string())
    };

    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: *\r\n\
         Connection: close\r\n\r\n{payload}",
        payload.len(),
    );
    write_half.write_all(response.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Serve forever. Returns only on a listener-level error (e.g. the address
/// is already in use) — per-connection errors are logged and otherwise
/// ignored so one bad request can't take the server down, matching
/// `evals::server::serve`.
pub async fn serve(
    addr: &str,
    services: std::sync::Arc<GuardedServices>,
    bank: MemoryBank,
    audit: std::sync::Arc<condesate::MemoryAudit>,
) -> Result<()> {
    let state = std::sync::Arc::new(State { services, bank, audit });
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, state).await {
                eprintln!("[mnemosyne] diagnostic API connection error: {e}");
            }
        });
    }
}
