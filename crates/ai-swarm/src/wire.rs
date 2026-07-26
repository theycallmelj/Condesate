//! Pure request/response mapping for the remote providers.
//!
//! These functions contain the actual provider-specific logic (how our neutral
//! `CompletionRequest`/`CompletionResponse` map onto each vendor's JSON). They
//! deliberately depend on nothing but `serde_json`, so they compile and are
//! unit-tested with or without the `remote` (reqwest) feature enabled — the
//! network client in `remote.rs` just calls into these.

use crate::types::{CompletionRequest, CompletionResponse, Role};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

pub fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        // Tool observations are folded into the user turn for these text APIs.
        Role::Tool => "user",
    }
}

// --- Anthropic --------------------------------------------------------------

/// Build the JSON body for the Anthropic Messages API.
///
/// Anthropic takes `system` as a top-level field and only user/assistant turns
/// in `messages`, so the system prompt is extracted here.
pub fn build_anthropic_body(model: &str, max_tokens: u32, req: &CompletionRequest) -> Value {
    let system = req
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");

    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| json!({ "role": role_str(m.role), "content": m.content }))
        .collect();

    json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": messages,
        "temperature": req.temperature,
    })
}

/// Parse an Anthropic Messages API response into a `CompletionResponse`.
pub fn parse_anthropic_response(body: &Value) -> Result<CompletionResponse> {
    if let Some(err) = body.get("error") {
        return Err(anyhow!("anthropic error: {err}"));
    }
    // `content` is a list of blocks; concatenate the text ones.
    let text = body
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    // >>> To support tool use: also scan for blocks with type == "tool_use"
    // and map each into a ToolCall { name, args } here. <<<
    Ok(CompletionResponse { content: text, tool_calls: vec![] })
}

// --- OpenAI -----------------------------------------------------------------

/// Build the JSON body for the OpenAI Chat Completions API.
pub fn build_openai_body(model: &str, req: &CompletionRequest) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| json!({ "role": role_str(m.role), "content": m.content }))
        .collect();
    json!({
        "model": model,
        "messages": messages,
        "temperature": req.temperature,
    })
}

/// Parse an OpenAI Chat Completions response into a `CompletionResponse`.
pub fn parse_openai_response(body: &Value) -> Result<CompletionResponse> {
    if let Some(err) = body.get("error") {
        return Err(anyhow!("openai error: {err}"));
    }
    let text = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();

    // >>> To support tool use: read choices[0].message.tool_calls and map each
    // into a ToolCall { name, args } here. <<<
    Ok(CompletionResponse { content: text, tool_calls: vec![] })
}

// --- tests (pure mapping, no network) --------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    fn sample_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![
                Message::system("be terse"),
                Message::user("hello"),
                Message::assistant("hi"),
                Message::tool("42"),
            ],
            tools: vec![],
            temperature: 0.3,
        }
    }

    #[test]
    fn anthropic_body_extracts_system_and_drops_it_from_messages() {
        let body = build_anthropic_body("claude-x", 256, &sample_request());
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["max_tokens"], 256);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3); // system removed; tool folded into user
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn anthropic_parse_concatenates_text_blocks() {
        let body = json!({
            "content": [
                { "type": "text", "text": "part one " },
                { "type": "text", "text": "part two" }
            ]
        });
        assert_eq!(parse_anthropic_response(&body).unwrap().content, "part one part two");
    }

    #[test]
    fn anthropic_parse_surfaces_errors() {
        let body = json!({ "error": { "message": "boom" } });
        assert!(parse_anthropic_response(&body).is_err());
    }

    #[test]
    fn openai_body_keeps_all_messages() {
        let body = build_openai_body("gpt-x", &sample_request());
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn openai_parse_reads_first_choice() {
        let body = json!({
            "choices": [ { "message": { "role": "assistant", "content": "yo" } } ]
        });
        assert_eq!(parse_openai_response(&body).unwrap().content, "yo");
    }

    #[test]
    fn openai_parse_surfaces_errors() {
        let body = json!({ "error": { "message": "nope" } });
        assert!(parse_openai_response(&body).is_err());
    }
}
