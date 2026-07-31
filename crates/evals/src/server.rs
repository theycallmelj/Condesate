//! An HTTP endpoint exposing the real `condesate` agent loop in the exact
//! shape `harness-evals`' `HttpTarget` expects: `POST /run` with
//! `{"input": "..."}`, `200 {"output": "..."}` back. See
//! `examples/http-agent.eval.yaml` in
//! <https://github.com/harness/harness-evals> for the contract this mirrors,
//! and `harness_evals/condesate.eval.yaml` for the real config that targets
//! this server.
//!
//! Hand-rolled rather than pulling in an HTTP framework: the contract is one
//! fixed route with a tiny JSON body, and the workspace otherwise stays
//! dependency-light (see the rest of `condesate`'s `Cargo.toml`).
//!
//! Every request builds a fresh admitted `GuardedServices` and runs a real
//! `ReActLoop` — the same production loop, tools, and permission boundary
//! the native suite (`suite.rs`) exercises in-process. `shutdown_swarm` is
//! deliberately left ungranted, so the "shutdown" golden proves the same
//! thing `suite::safety_denied_tool_never_executes` does, but reached over
//! real HTTP instead of a direct function call.

use anyhow::Result;
use async_trait::async_trait;
use condesate::{
    Action, Agent, AgentContext, AgentLoop, BasicAgent, CompletionRequest, CompletionResponse,
    Message, ModelProvider, Pattern, ReActLoop, ResourcePattern, Role, Rule, ShutdownSwarm,
    SubjectMatch, ToolCall, WordCount,
};
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::harness::build_services;

/// Scripts exactly two intents recognized by the goldens in
/// `harness_evals/goldens.jsonl`, plus an echo fallback:
///   - `"count words in: <text>"` -> calls `word_count`, reports the count.
///   - `"shutdown"` -> calls `shutdown_swarm` (ungranted -> denied).
struct HttpEvalModel;

#[async_trait]
impl ModelProvider for HttpEvalModel {
    fn name(&self) -> &str {
        "http-eval"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let turns = req.messages.iter().filter(|m| m.role == Role::Assistant).count();
        let user =
            req.messages.iter().rev().find(|m| m.role == Role::User).map(|m| m.content.clone()).unwrap_or_default();
        let obs =
            req.messages.iter().rev().find(|m| m.role == Role::Tool).map(|m| m.content.clone()).unwrap_or_default();

        if turns == 0 {
            if let Some(text) = user.strip_prefix("count words in:") {
                return Ok(CompletionResponse {
                    content: "counting".into(),
                    tool_calls: vec![ToolCall { name: "word_count".into(), args: json!({"text": text.trim()}) }],
                });
            }
            if user.trim() == "shutdown" {
                return Ok(CompletionResponse {
                    content: "shutting down".into(),
                    tool_calls: vec![ToolCall { name: "shutdown_swarm".into(), args: json!({}) }],
                });
            }
            return Ok(CompletionResponse { content: user, tool_calls: vec![] });
        }
        Ok(CompletionResponse { content: obs, tool_calls: vec![] })
    }
}

async fn run_target(input: &str) -> Result<String> {
    let grants = vec![Rule::allow(
        "word_count",
        SubjectMatch::agent("http-eval"),
        &[Action::Invoke],
        ResourcePattern::Tool(Pattern::Exact("word_count".into())),
    )];
    let (svc, _inboxes) = build_services("http-eval", &["http-eval"], grants);
    let agent =
        BasicAgent::new("http-eval", "system", Arc::new(HttpEvalModel), vec![Arc::new(WordCount), Arc::new(ShutdownSwarm)]);
    let mut ctx = AgentContext::new(svc);
    ctx.transcript.push(Message::user(input.to_string()));
    let outcome = ReActLoop::default().run(&agent as &dyn Agent, &mut ctx).await?;
    Ok(outcome.final_text)
}

async fn handle(stream: TcpStream) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

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
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let (status, reason, payload) = if request_line.starts_with("POST /run") {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => {
                let input = v.get("input").and_then(|s| s.as_str()).unwrap_or("").to_string();
                match run_target(&input).await {
                    Ok(output) => (200, "OK", json!({ "output": output }).to_string()),
                    Err(e) => (500, "Internal Server Error", json!({ "error": e.to_string() }).to_string()),
                }
            }
            Err(e) => (400, "Bad Request", json!({ "error": e.to_string() }).to_string()),
        }
    } else {
        (404, "Not Found", json!({ "error": "no such route" }).to_string())
    };

    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len(),
    );
    write_half.write_all(response.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Serve forever. Returns only on a listener-level error (e.g. the address
/// is already in use) — per-connection errors are logged and otherwise
/// ignored so one bad request can't take the server down.
pub async fn serve(addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("condesate eval target listening on http://{addr}/run");
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle(stream).await {
                eprintln!("eval target connection error: {e}");
            }
        });
    }
}
