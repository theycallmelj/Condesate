//! Model providers: the "how is the agent actually called" abstraction.
//!
//! This is the seam you asked about between a **local** model and a **cloud**
//! model. Both implement the same `ModelProvider` trait, so an agent doesn't
//! know or care which it's talking to — you swap them at construction time.
//!
//! The two impls here are deterministic mocks so the demo runs fully offline.
//! Each contains a clearly-marked spot showing where a real inference call
//! (llama.cpp / Ollama for local, an HTTP API for cloud) would go.

use crate::types::{CompletionRequest, CompletionResponse, Role, ToolCall};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;
    fn name(&self) -> &str;
}

// --- small helpers shared by the mocks -------------------------------------

fn count_assistant(req: &CompletionRequest) -> usize {
    req.messages.iter().filter(|m| m.role == Role::Assistant).count()
}

fn last_user(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

fn last_tool_obs(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

fn after<'a>(haystack: &'a str, marker: &str) -> &'a str {
    haystack.split(marker).nth(1).unwrap_or("").trim()
}

// ---------------------------------------------------------------------------
// LOCAL MODEL
// ---------------------------------------------------------------------------

/// Stands in for an on-device model (Ollama, llama.cpp, mlx, ...).
pub struct LocalModel {
    name: String,
}

impl LocalModel {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl ModelProvider for LocalModel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        // >>> REAL LOCAL INFERENCE WOULD GO HERE <<<
        // e.g. POST to http://localhost:11434/api/chat (Ollama) and map the
        // JSON back into CompletionResponse. Below is a deterministic script
        // so the worker agent behaves predictably in the demo.

        let turns = count_assistant(&req);
        let user = last_user(&req);
        let obs = last_tool_obs(&req);

        let resp = match turns {
            // First turn: we've been asked to count words -> call the tool.
            0 if user.contains("count words in:") => CompletionResponse {
                content: "Counting the words now.".into(),
                tool_calls: vec![ToolCall {
                    name: "word_count".into(),
                    args: json!({ "text": after(&user, "count words in:") }),
                }],
            },
            // Second turn: we have the count -> report it back to the caller.
            1 => CompletionResponse {
                content: "Reporting the count back.".into(),
                tool_calls: vec![ToolCall {
                    name: "send_message".into(),
                    args: json!({
                        "to": "planner-cloud",
                        "text": format!("word_count={obs}")
                    }),
                }],
            },
            // Done.
            _ => CompletionResponse {
                content: "Finished; word count delivered.".into(),
                tool_calls: vec![],
            },
        };
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// CLOUD MODEL
// ---------------------------------------------------------------------------

/// Stands in for a hosted API model (Anthropic, OpenAI, ...).
pub struct CloudModel {
    name: String,
}

impl CloudModel {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl ModelProvider for CloudModel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        // >>> REAL CLOUD INFERENCE WOULD GO HERE <<<
        // e.g. reqwest POST to https://api.anthropic.com/v1/messages with your
        // key, tools serialized from req.tools, then parse the tool_use blocks
        // into ToolCall values. Deterministic script below for the demo.

        let turns = count_assistant(&req);
        let user = last_user(&req);
        let obs = last_tool_obs(&req);

        // The planner is activated twice: once to delegate, once to record the
        // verdict. We branch on the content of the incoming user message.
        let resp = if user.contains("word_count=") {
            // ---- verdict activation ----
            let n = after(&user, "word_count=");
            match turns {
                0 => CompletionResponse {
                    content: "Got the count; recording a verdict.".into(),
                    tool_calls: vec![ToolCall {
                        name: "remember".into(),
                        args: json!({
                            "key": "slogan/verdict",
                            "value": format!("{n} words — concise enough to ship")
                        }),
                    }],
                },
                1 => CompletionResponse {
                    content: "Verdict stored; winding the swarm down.".into(),
                    tool_calls: vec![ToolCall {
                        name: "shutdown_swarm".into(),
                        args: json!({}),
                    }],
                },
                _ => CompletionResponse {
                    content: "All done.".into(),
                    tool_calls: vec![],
                },
            }
        } else {
            // ---- delegation activation ----
            match turns {
                0 => CompletionResponse {
                    content: "Delegating the word count to the local worker.".into(),
                    tool_calls: vec![ToolCall {
                        name: "send_message".into(),
                        args: json!({
                            "to": "worker-local",
                            "text": "count words in: ship fast stay safe"
                        }),
                    }],
                },
                _ => CompletionResponse {
                    // Referencing obs keeps the variable meaningful even when empty.
                    content: format!("Delegated (last obs: '{obs}'); awaiting reply."),
                    tool_calls: vec![],
                },
            }
        };
        Ok(resp)
    }
}
