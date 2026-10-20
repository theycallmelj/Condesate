//! A prompted (ReAct-style) tool-calling convention layered over any
//! [`ModelProvider`].
//!
//! [`super::wire`] doesn't implement either vendor's native structured
//! tool-use wire format yet (see the `>>> TODO <<<` markers there), so a
//! real network model (`AnthropicModel`/`OpenAiModel`, feature `remote`)
//! never emits a `ToolCall` on its own. [`PromptedToolModel`] bridges that
//! gap with an ordinary text convention instead — the model still makes the
//! real decision, it's just conveyed as a line of text
//! (`TOOL_CALL: <name> <json args>`) rather than a vendor tool_use block.
//! This predates and parallels native function-calling APIs; it's a
//! standard ReAct-style pattern, not a shortcut. Provider-agnostic (wraps
//! any `ModelProvider`), so it isn't gated behind `remote`.

use super::model::ModelProvider;
use crate::types::{CompletionRequest, CompletionResponse, Message, Role, ToolCall, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

const TOOL_CALL_PREFIX: &str = "TOOL_CALL:";

/// Wraps a real [`ModelProvider`] with the convention above.
///
/// Every tool-result turn is rewritten with a `[TOOL RESULT]` prefix before
/// being forwarded to the inner model: [`super::wire`] folds every
/// non-system role into a plain "user" turn for these text APIs, so a bare
/// tool result (e.g. `"4"`) would otherwise be indistinguishable from a new
/// instruction — a model left to guess will happily call the same tool
/// again rather than answer. See [`tool_instructions`] for the matching
/// system-prompt fragment that explains the convention; append it to your
/// agent's own system prompt.
pub struct PromptedToolModel {
    inner: Arc<dyn ModelProvider>,
}

impl PromptedToolModel {
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ModelProvider for PromptedToolModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let messages = req
            .messages
            .into_iter()
            .map(|m| {
                if m.role == Role::Tool {
                    Message::tool(format!("[TOOL RESULT] {}", m.content))
                } else {
                    m
                }
            })
            .collect();
        let req = CompletionRequest { messages, ..req };

        let resp = self.inner.complete(req).await?;
        for line in resp.content.lines() {
            let Some(rest) = line.trim().strip_prefix(TOOL_CALL_PREFIX) else { continue };
            let rest = rest.trim();
            let (name, args_str) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
            let args_str = if args_str.trim().is_empty() { "{}" } else { args_str.trim() };
            if let Ok(args) = serde_json::from_str::<Value>(args_str) {
                return Ok(CompletionResponse {
                    content: format!("calling {}", name.trim()),
                    tool_calls: vec![ToolCall { name: name.trim().to_string(), args }],
                });
            }
        }
        Ok(resp)
    }
}

/// System-prompt fragment describing the tool-calling convention and the
/// given tools. It's a fragment, not a full prompt — append the result to
/// your agent's own system prompt so it composes with whatever
/// persona/task instructions the agent already has.
pub fn tool_instructions(tools: &[ToolSpec]) -> String {
    let tool_list = tools.iter().map(|t| format!("- {}: {}", t.name, t.description)).collect::<Vec<_>>().join("\n");
    format!(
        "You have these tools available:\n{tool_list}\n\n\
         To call a tool, respond with a line of the exact form:\n\
         TOOL_CALL: <tool_name> <json_arguments>\n\
         with no other text on that line. The next message you receive will start with \
         \"[TOOL RESULT]\" — that IS the result of the call you just made, not a new \
         instruction from the user. Once you see it, answer directly using that result in \
         plain text with no TOOL_CALL line. Never call the same tool twice for the same \
         request. If a tool result says the call was denied, explain that plainly — do not \
         retry it. Keep answers short."
    )
}
