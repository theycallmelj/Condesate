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
//!
//! **The model.** If `PROVIDER`/`ANTHROPIC_API_KEY` (or `OPENAI_API_KEY`)
//! are set — including via a `.env` file, loaded automatically — this
//! serves real inference: `AnthropicModel`/`OpenAiModel` actually decide
//! whether and which tool to call, given the input, via
//! `condesate::PromptedToolModel` (see its docs for why that wrapper exists
//! instead of a vendor's native tool-use wire format). With no key
//! configured, this falls back to [`HttpEvalModel`], the original
//! deterministic script, so the eval paths still work offline.

use anyhow::Result;
use async_trait::async_trait;
use condesate::{
    tool_instructions, Action, Agent, AgentContext, AgentLoop, AnthropicModel, BasicAgent,
    CompletionRequest, CompletionResponse, Message, ModelProvider, OpenAiModel, Pattern,
    PromptedToolModel, ReActLoop, ResourcePattern, Role, Rule, ShutdownSwarm, SubjectMatch, Tool,
    ToolCall, WordCount,
};
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::harness::build_services;

/// Deterministic fallback used when no real provider is configured. Scripts
/// exactly two intents recognized by the goldens in
/// `harness_evals/goldens.jsonl`, plus an echo fallback:
///   - `"count words in: <text>"` -> calls `word_count`, reports the count.
///   - `"shutdown"` -> calls `shutdown_swarm` (ungranted -> denied).
struct HttpEvalModel;

#[async_trait]
impl ModelProvider for HttpEvalModel {
    fn name(&self) -> &str {
        "http-eval-scripted"
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

fn system_prompt() -> String {
    tool_instructions(&[WordCount.spec(), ShutdownSwarm.spec()])
}

/// Picks a real provider from `PROVIDER`/`ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/
/// `MODEL` (a `.env` file is loaded automatically — see `serve()`), falling
/// back to the deterministic [`HttpEvalModel`] if none is configured. Mirrors
/// `chat-app`'s `choose_provider()`.
fn choose_model() -> (Arc<dyn ModelProvider>, String) {
    let provider = std::env::var("PROVIDER").unwrap_or_default().to_lowercase();
    let model_name = std::env::var("MODEL").ok();

    match provider.as_str() {
        "anthropic" => {
            let name = model_name.unwrap_or_else(|| "claude-sonnet-4-6".to_string());
            match AnthropicModel::from_env(name.clone()) {
                Ok(m) => (
                    Arc::new(PromptedToolModel::new(Arc::new(m))) as Arc<dyn ModelProvider>,
                    format!("live: anthropic ({name})"),
                ),
                Err(e) => {
                    eprintln!("[eval target] anthropic unavailable ({e}); falling back to the scripted offline model");
                    (Arc::new(HttpEvalModel), "scripted (offline)".to_string())
                }
            }
        }
        "openai" => {
            let name = model_name.unwrap_or_else(|| "gpt-4o".to_string());
            match OpenAiModel::from_env(name.clone()) {
                Ok(m) => (
                    Arc::new(PromptedToolModel::new(Arc::new(m))) as Arc<dyn ModelProvider>,
                    format!("live: openai ({name})"),
                ),
                Err(e) => {
                    eprintln!("[eval target] openai unavailable ({e}); falling back to the scripted offline model");
                    (Arc::new(HttpEvalModel), "scripted (offline)".to_string())
                }
            }
        }
        _ => {
            eprintln!(
                "[eval target] set PROVIDER=anthropic|openai (+ API key, e.g. in .env) to use a \
                 live model; using the scripted offline model"
            );
            (Arc::new(HttpEvalModel), "scripted (offline)".to_string())
        }
    }
}

/// Wraps a model and records every tool name it actually decides to call —
/// not what a client expects it to call. A real model's decision isn't
/// guaranteed the way the scripted fallback's was, so callers (e.g.
/// `strands_eval.py`'s `ToolCalled` check) need the real answer, not a
/// guess inferred from the input text.
struct RecordingModel {
    inner: Arc<dyn ModelProvider>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl ModelProvider for RecordingModel {
    fn name(&self) -> &str {
        self.inner.name()
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let resp = self.inner.complete(req).await?;
        if !resp.tool_calls.is_empty() {
            let mut calls = self.calls.lock().unwrap();
            calls.extend(resp.tool_calls.iter().map(|c| c.name.clone()));
        }
        Ok(resp)
    }
}

async fn run_target(model: Arc<dyn ModelProvider>, system_prompt: Arc<String>, input: &str) -> Result<(String, Vec<String>)> {
    let grants = vec![Rule::allow(
        "word_count",
        SubjectMatch::agent("http-eval"),
        &[Action::Invoke],
        ResourcePattern::Tool(Pattern::Exact("word_count".into())),
    )];
    let (svc, _inboxes) = build_services("http-eval", &["http-eval"], grants);
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recording_model = Arc::new(RecordingModel { inner: model, calls: calls.clone() });
    let agent = BasicAgent::new(
        "http-eval",
        system_prompt.as_str(),
        recording_model,
        vec![Arc::new(WordCount), Arc::new(ShutdownSwarm)],
    );
    let mut ctx = AgentContext::new(svc);
    ctx.transcript.push(Message::user(input.to_string()));
    // Capped well below the default (8): with a real model in the loop, a
    // runaway retry burns real API calls, so failing fast here is cheaper
    // than letting a confused model keep trying.
    let outcome = ReActLoop { max_steps: 4 }.run(&agent as &dyn Agent, &mut ctx).await?;
    let tools_called = calls.lock().unwrap().clone();
    Ok((outcome.final_text, tools_called))
}

async fn handle(stream: TcpStream, model: Arc<dyn ModelProvider>, system_prompt: Arc<String>) -> Result<()> {
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
                match run_target(model, system_prompt, &input).await {
                    Ok((output, tools_called)) => {
                        (200, "OK", json!({ "output": output, "tools_called": tools_called }).to_string())
                    }
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
    dotenvy::dotenv().ok();
    let (model, label) = choose_model();
    eprintln!("condesate eval target model: {label}");
    let system_prompt = Arc::new(system_prompt());

    let listener = TcpListener::bind(addr).await?;
    eprintln!("condesate eval target listening on http://{addr}/run");
    loop {
        let (stream, _) = listener.accept().await?;
        let model = model.clone();
        let system_prompt = system_prompt.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, model, system_prompt).await {
                eprintln!("eval target connection error: {e}");
            }
        });
    }
}
