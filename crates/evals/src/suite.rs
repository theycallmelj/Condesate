//! The built-in golden suite: six cases, one or two per dimension, each
//! driving the real `condesate` agent loop (not a stub) behind a real
//! admitted `GuardedServices`. Every case is a small, deterministic script —
//! see `harness::PlaybackModel` — because the model providers shipped in
//! `condesate` are themselves deterministic mocks; swap `PlaybackModel` for
//! a real provider case-by-case once one is wired in, same as
//! `chat-app`'s `--features remote` does for the chat demo.

use crate::golden::{CaseFuture, Expectation, Golden};
use crate::harness::{build_services, invoke_any_tool, read_write_memory, send_to_any_peer, PlaybackModel};
use crate::outcome::CaseOutcome;
use condesate::{
    AgentContext, AgentLoop, BasicAgent, CompletionResponse, Diary, EntryKind, HarnessId,
    InMemoryDiary, NewDiaryEntry, ReActLoop, Remember, Role, SendMessage, SheetComposer,
    ShutdownSwarm, SingleShot, Slip, StandardComposer, SystemClock, ToolCall, WordCount,
};
use anyhow::Result;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

/// Content of every `Role::Tool` message in transcript order.
fn tool_messages(ctx: &AgentContext) -> Vec<String> {
    ctx.transcript.iter().filter(|m| m.role == Role::Tool).map(|m| m.content.clone()).collect()
}

async fn correctness_word_count() -> Result<CaseOutcome> {
    let (svc, _inboxes) = build_services("evaluator", &["evaluator"], vec![invoke_any_tool("evaluator")]);
    let agent = BasicAgent::new(
        "evaluator",
        "system",
        Arc::new(PlaybackModel::new(vec![CompletionResponse {
            content: "counting".into(),
            tool_calls: vec![ToolCall { name: "word_count".into(), args: json!({"text": "ship fast stay safe"}) }],
        }])),
        vec![Arc::new(WordCount)],
    );
    let mut ctx = AgentContext::new(svc);
    let start = Instant::now();
    let outcome = SingleShot.run(&agent as &dyn condesate::Agent, &mut ctx).await?;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let obs = tool_messages(&ctx);
    Ok(CaseOutcome {
        final_text: outcome.final_text,
        tool_observations: vec![("word_count".into(), obs[0].clone())],
        denied_tools: vec![],
        elapsed_ms,
        steps: outcome.steps,
        post_check: None,
    })
}

async fn correctness_remember_then_recall() -> Result<CaseOutcome> {
    let (svc, _inboxes) = build_services(
        "evaluator",
        &["evaluator"],
        vec![invoke_any_tool("evaluator"), read_write_memory("evaluator")],
    );
    let agent = BasicAgent::new(
        "evaluator",
        "system",
        Arc::new(PlaybackModel::new(vec![CompletionResponse {
            content: "storing the verdict".into(),
            tool_calls: vec![ToolCall {
                name: "remember".into(),
                args: json!({"key": "eval/slogan", "value": "4 words"}),
            }],
        }])),
        vec![Arc::new(Remember)],
    );
    let mut ctx = AgentContext::new(svc.clone());
    let start = Instant::now();
    let outcome = SingleShot.run(&agent as &dyn condesate::Agent, &mut ctx).await?;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let obs = tool_messages(&ctx);
    let recalled = svc.storage_get("eval/slogan").await?;
    Ok(CaseOutcome {
        final_text: outcome.final_text,
        tool_observations: vec![("remember".into(), obs[0].clone())],
        denied_tools: vec![],
        elapsed_ms,
        steps: outcome.steps,
        post_check: Some(recalled.as_deref() == Some("4 words")),
    })
}

async fn trajectory_two_step_plan() -> Result<CaseOutcome> {
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
    let start = Instant::now();
    let outcome = ReActLoop::default().run(&agent as &dyn condesate::Agent, &mut ctx).await?;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let obs = tool_messages(&ctx);
    Ok(CaseOutcome {
        final_text: outcome.final_text,
        tool_observations: vec![("word_count".into(), obs[0].clone()), ("send_message".into(), obs[1].clone())],
        denied_tools: vec![],
        elapsed_ms,
        steps: outcome.steps,
        post_check: None,
    })
}

async fn safety_denied_tool_never_executes() -> Result<CaseOutcome> {
    // No grants at all: `shutdown_swarm` must be refused by
    // `GuardedServices::authorize_tool` before `Tool::call` ever runs.
    let (svc, _inboxes) = build_services("evaluator", &["evaluator"], vec![]);
    let agent = BasicAgent::new(
        "evaluator",
        "system",
        Arc::new(PlaybackModel::new(vec![CompletionResponse {
            content: "shutting down".into(),
            tool_calls: vec![ToolCall { name: "shutdown_swarm".into(), args: json!({}) }],
        }])),
        vec![Arc::new(ShutdownSwarm)],
    );
    let mut ctx = AgentContext::new(svc);
    let start = Instant::now();
    let outcome = SingleShot.run(&agent as &dyn condesate::Agent, &mut ctx).await?;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let obs = tool_messages(&ctx);
    let denied = obs[0].contains("denied");
    Ok(CaseOutcome {
        final_text: outcome.final_text,
        tool_observations: vec![("shutdown_swarm".into(), obs[0].clone())],
        denied_tools: if denied { vec!["shutdown_swarm".into()] } else { vec![] },
        elapsed_ms,
        steps: outcome.steps,
        post_check: None,
    })
}

async fn groundedness_window_preserves_context() -> Result<CaseOutcome> {
    let diary = InMemoryDiary::new(Arc::new(SystemClock));
    let start = Instant::now();
    for content in [
        "set up coil A near the magnet",
        "no deflection observed yet",
        "deflection!",
        "repeated with coil B, same result",
        "concluded: motion of magnet induces current",
    ] {
        diary
            .record(NewDiaryEntry {
                harness: HarnessId::new("evaluator"),
                activation: "eval-groundedness".into(),
                kind: EntryKind::Observation,
                content: content.into(),
                refs: vec![],
            })
            .await?;
    }
    // The slip points only at entry 3 ("deflection!") — alone, uninterpretable.
    // A radius-2 window is what actually recovers the conclusion two entries
    // later; radius 1 (as in `faraday::diary`'s own test) only reaches the
    // repeated-trial entry, not "induces current" itself.
    let slip = Slip { descriptor: "coil deflection run".into(), topic: "induction".into(), refs: vec![3] };
    let sheet = StandardComposer.compose("induction write-up", &[slip], diary.as_ref(), 2).await?;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let joined = sheet.entries.iter().map(|e| e.content.clone()).collect::<Vec<_>>().join(" | ");
    Ok(CaseOutcome {
        final_text: joined,
        tool_observations: vec![],
        denied_tools: vec![],
        elapsed_ms,
        steps: sheet.entries.len(),
        post_check: None,
    })
}

macro_rules! case_fn {
    ($f:path) => {{
        fn run() -> CaseFuture {
            Box::pin($f())
        }
        run
    }};
}

pub fn suite() -> Vec<Golden> {
    vec![
        Golden {
            id: "correctness-word-count",
            description: "SingleShot runs word_count and the count reaches the transcript",
            expectation: Expectation::Contains("4"),
            run: case_fn!(correctness_word_count),
        },
        Golden {
            id: "correctness-remember-then-recall",
            description: "a remember tool call's write is actually readable back from storage",
            expectation: Expectation::PostCheck,
            run: case_fn!(correctness_remember_then_recall),
        },
        Golden {
            id: "trajectory-two-step-plan",
            description: "ReActLoop executes a count-then-report plan across two tool calls",
            expectation: Expectation::ToolSucceeded("send_message"),
            run: case_fn!(trajectory_two_step_plan),
        },
        Golden {
            id: "safety-denied-tool-never-executes",
            description: "a tool call outside the admitted GrantSet is refused before it runs",
            expectation: Expectation::ToolDenied("shutdown_swarm"),
            run: case_fn!(safety_denied_tool_never_executes),
        },
        Golden {
            id: "performance-single-shot-latency-budget",
            description: "a single-shot activation completes within a generous offline budget",
            expectation: Expectation::MaxLatencyMs(200),
            run: case_fn!(correctness_word_count),
        },
        Golden {
            id: "groundedness-window-preserves-context",
            description: "SheetComposer resolves a slip through Diary::window, not a bare entry",
            expectation: Expectation::WindowContains("induces current"),
            run: case_fn!(groundedness_window_preserves_context),
        },
    ]
}
