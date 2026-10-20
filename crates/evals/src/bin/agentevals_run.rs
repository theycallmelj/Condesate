//! Real integration with LangChain's
//! [`agentevals`](https://github.com/langchain-ai/agentevals): dumps two
//! *real* `condesate` runs (a correct one, and a genuinely regressed one —
//! same starting plan, real ReActLoop, one real step silently dropped) plus
//! a hand-authored reference plan as OpenAI-message-shaped JSON, then shells
//! out to `python3 grade_trajectory.py`, which grades them with the actual,
//! pip-installed `agentevals` `create_trajectory_match_evaluator`. Not a
//! reimplementation of its matching logic — the real library makes the real
//! pass/fail calls.
//!
//! Requires `agentevals` on the `python3` used (`pip install agentevals`);
//! see `crates/evals/README.md`.

use anyhow::{Context, Result};
use condesate::{
    Agent, AgentContext, AgentLoop, BasicAgent, CompletionResponse, Message, ReActLoop, Role,
    SendMessage, ToolCall, WordCount,
};
use evals::harness::{build_services, invoke_any_tool, send_to_any_peer, PlaybackModel};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Serialize, Clone)]
struct FunctionDump {
    name: String,
    /// JSON-encoded, matching the OpenAI tool-call wire format `agentevals`
    /// expects — not a nested object.
    arguments: String,
}

#[derive(Serialize, Clone)]
struct ToolCallDump {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: FunctionDump,
}

#[derive(Serialize, Clone)]
struct TraceMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallDump>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

fn user(content: &str) -> TraceMessage {
    TraceMessage { role: "user".into(), content: content.into(), tool_calls: None, tool_call_id: None }
}

fn assistant(content: &str) -> TraceMessage {
    TraceMessage { role: "assistant".into(), content: content.into(), tool_calls: None, tool_call_id: None }
}

fn assistant_with_call(content: &str, id: &str, name: &str, args: &Value) -> TraceMessage {
    TraceMessage {
        role: "assistant".into(),
        content: content.into(),
        tool_calls: Some(vec![ToolCallDump {
            id: id.into(),
            kind: "function".into(),
            function: FunctionDump { name: name.into(), arguments: args.to_string() },
        }]),
        tool_call_id: None,
    }
}

fn tool_result(id: &str, content: &str) -> TraceMessage {
    TraceMessage { role: "tool".into(), content: content.into(), tool_calls: None, tool_call_id: Some(id.into()) }
}

fn tool_messages(ctx: &AgentContext) -> Vec<String> {
    ctx.transcript.iter().filter(|m| m.role == Role::Tool).map(|m| m.content.clone()).collect()
}

const INPUT: &str = "count words in: ship fast stay safe";

/// The intended plan, authored independently of any particular run: count
/// the words, then report the count to a peer. What real runs get graded
/// against.
fn reference_trajectory() -> Vec<TraceMessage> {
    vec![
        user(INPUT),
        assistant_with_call("counting", "1", "word_count", &json!({"text": "ship fast stay safe"})),
        tool_result("1", "4"),
        assistant_with_call(
            "reporting the count",
            "2",
            "send_message",
            &json!({"to": "recorder", "text": "count=4"}),
        ),
        tool_result("2", "sent to 'recorder'"),
        assistant("done"),
    ]
}

/// Runs the real `condesate` `ReActLoop` through the intended plan.
async fn run_good() -> Result<Vec<TraceMessage>> {
    let (svc, _inboxes) = build_services(
        "evaluator",
        &["evaluator", "recorder"],
        vec![invoke_any_tool("evaluator"), send_to_any_peer("evaluator")],
    );
    let agent = BasicAgent::new(
        "evaluator",
        "system",
        Arc::new(PlaybackModel::new(vec![
            CompletionResponse {
                content: "counting".into(),
                tool_calls: vec![ToolCall {
                    name: "word_count".into(),
                    args: json!({"text": "ship fast stay safe"}),
                }],
            },
            CompletionResponse {
                content: "reporting the count".into(),
                tool_calls: vec![ToolCall {
                    name: "send_message".into(),
                    args: json!({"to": "recorder", "text": "count=4"}),
                }],
            },
            CompletionResponse { content: "done".into(), tool_calls: vec![] },
        ])),
        vec![Arc::new(WordCount), Arc::new(SendMessage)],
    );
    let mut ctx = AgentContext::new(svc);
    ctx.transcript.push(Message::user(INPUT.to_string()));
    ReActLoop::default().run(&agent as &dyn Agent, &mut ctx).await?;

    let obs = tool_messages(&ctx);
    Ok(vec![
        user(INPUT),
        assistant_with_call("counting", "1", "word_count", &json!({"text": "ship fast stay safe"})),
        tool_result("1", &obs[0]),
        assistant_with_call(
            "reporting the count",
            "2",
            "send_message",
            &json!({"to": "recorder", "text": format!("count={}", obs[0])}),
        ),
        tool_result("2", &obs[1]),
        assistant("done"),
    ])
}

/// A real regression: same starting plan, real loop, but the model stops
/// after counting and never reports back — one real step silently dropped.
async fn run_regressed() -> Result<Vec<TraceMessage>> {
    let (svc, _inboxes) = build_services("evaluator", &["evaluator"], vec![invoke_any_tool("evaluator")]);
    let agent = BasicAgent::new(
        "evaluator",
        "system",
        Arc::new(PlaybackModel::new(vec![
            CompletionResponse {
                content: "counting".into(),
                tool_calls: vec![ToolCall {
                    name: "word_count".into(),
                    args: json!({"text": "ship fast stay safe"}),
                }],
            },
            CompletionResponse { content: "done (forgot to report)".into(), tool_calls: vec![] },
        ])),
        vec![Arc::new(WordCount)],
    );
    let mut ctx = AgentContext::new(svc);
    ctx.transcript.push(Message::user(INPUT.to_string()));
    ReActLoop::default().run(&agent as &dyn Agent, &mut ctx).await?;

    let obs = tool_messages(&ctx);
    Ok(vec![
        user(INPUT),
        assistant_with_call("counting", "1", "word_count", &json!({"text": "ship fast stay safe"})),
        tool_result("1", &obs[0]),
        assistant("done (forgot to report)"),
    ])
}

const AGENT_EVALS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/agent_evals");

#[tokio::main]
async fn main() -> Result<()> {
    let reference = reference_trajectory();
    let good = run_good().await?;
    let regressed = run_regressed().await?;

    std::fs::create_dir_all(AGENT_EVALS_DIR)?;
    let dump = json!({ "reference": reference, "actual_good": good, "actual_regressed": regressed });
    let path = format!("{AGENT_EVALS_DIR}/trajectory.json");
    std::fs::write(&path, serde_json::to_string_pretty(&dump)?)?;
    println!("wrote real condesate trajectories to {path}");

    let script = std::env::var("AGENTEVALS_SCRIPT").unwrap_or_else(|_| "grade_trajectory.py".to_string());
    println!("running: python3 {script}  (cwd: {AGENT_EVALS_DIR})\n");

    let status = tokio::process::Command::new("python3")
        .arg(&script)
        .current_dir(AGENT_EVALS_DIR)
        .status()
        .await
        .context("failed to run `python3` — is `agentevals` installed? (pip install agentevals)")?;

    std::process::exit(status.code().unwrap_or(1));
}
