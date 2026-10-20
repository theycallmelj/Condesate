//! A minimal client for the [A2A ("Agent2Agent")](https://a2a-protocol.org/)
//! protocol — JSON-RPC 2.0 over HTTP, the way an agent hosted on a website
//! advertises itself (an "Agent Card" at a well-known path) and is talked to
//! (a `message/send` JSON-RPC call to the endpoint URL the card names).
//! Client-only, the same scope [`super::mcp`] keeps for MCP: this reaches out
//! to agents other people run, it doesn't host one.
//!
//! Two tools built on [`A2aClient`] are meant for an `Agent`'s toolbelt:
//! [`DiscoverA2aAgent`] fetches and summarizes a site's agent card so the
//! model can decide whether/how to use it; [`SendA2aMessage`] sends one
//! message to the endpoint the card named and returns whatever text the
//! agent replied with. Both go through the ordinary `Action::Invoke` /
//! `Resource::Tool` gate every tool call already goes through — no second,
//! protocol-specific permission check here, for the same reason `mcp.rs` has
//! none: a parallel gate is a gate that can drift out of sync with the real
//! one; by the time `call` runs, `GuardedServices::authorize_tool` has
//! already decided whether this call happens at all.
//!
//! Agent cards and JSON-RPC results are walked as plain `serde_json::Value`
//! rather than deserialized into strict typed structs — real agents vary in
//! which optional fields they send, and a client that hard-fails on an
//! unrecognized shape is worse here than one that pulls out what it can and
//! says so. Deliberately not implemented: streaming (`message/stream`, SSE),
//! push-notification webhooks, task cancellation/resubscription, and any
//! authentication scheme beyond what a plain HTTPS GET/POST carries — this
//! is enough to hold one request/response exchange with a well-behaved
//! agent, not a full A2A-compliant peer.

use super::tool::Tool;
use crate::security::kernel::GuardedServices;
use crate::types::ToolSpec;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

/// What a site publishes about the agent it hosts. Only the fields a caller
/// actually needs to decide whether/how to talk to the agent are modeled;
/// everything else in the card is ignored.
#[derive(Debug, Clone)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    /// The JSON-RPC endpoint to actually send messages to — not necessarily
    /// the same URL the card itself was fetched from.
    pub url: String,
    pub version: String,
    pub skills: Vec<AgentSkill>,
}

#[derive(Debug, Clone)]
pub struct AgentSkill {
    pub name: String,
    pub description: String,
}

impl AgentCard {
    fn from_value(v: &Value) -> Result<Self> {
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("agent card has no 'name' field"))?
            .to_string();
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("agent card has no 'url' field (the endpoint to message)"))?
            .to_string();
        let description = v.get("description").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let version = v.get("version").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let skills = v
            .get("skills")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|s| AgentSkill {
                        name: s.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                        description: s.get("description").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self { name, description, url, version, skills })
    }

    /// A short, model-readable summary — what [`DiscoverA2aAgent`] returns.
    pub fn summarize(&self) -> String {
        let mut out = format!("{} — {}\nendpoint: {}", self.name, self.version, self.url);
        if !self.description.is_empty() {
            out.push('\n');
            out.push_str(&self.description);
        }
        if !self.skills.is_empty() {
            out.push_str("\nskills:");
            for s in &self.skills {
                out.push_str(&format!("\n- {}: {}", s.name, s.description));
            }
        }
        out
    }
}

/// Best-effort text extraction from a `message/send` JSON-RPC result, which
/// the spec allows to be either a `Message` (`parts` directly on it) or a
/// `Task` (`parts` nested under `artifacts`, or under `status.message` while
/// still working/failed). Prefers artifact text; falls back to the status
/// message; reports the task's state in place of text if neither carried any.
fn extract_text(result: &Value) -> String {
    fn collect_parts(parts: &Value, out: &mut Vec<String>) {
        if let Some(arr) = parts.as_array() {
            for p in arr {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    out.push(t.to_string());
                }
            }
        }
    }

    let mut out = Vec::new();
    let is_task = result.get("kind").and_then(|v| v.as_str()) == Some("task") || result.get("status").is_some();

    if is_task {
        if let Some(artifacts) = result.get("artifacts").and_then(|v| v.as_array()) {
            for a in artifacts {
                if let Some(parts) = a.get("parts") {
                    collect_parts(parts, &mut out);
                }
            }
        }
        if out.is_empty() {
            if let Some(parts) = result.get("status").and_then(|s| s.get("message")).and_then(|m| m.get("parts")) {
                collect_parts(parts, &mut out);
            }
        }
        if out.is_empty() {
            let state = result.get("status").and_then(|s| s.get("state")).and_then(|v| v.as_str()).unwrap_or("unknown");
            return format!("[task state: {state}, no text content yet]");
        }
    } else if let Some(parts) = result.get("parts") {
        collect_parts(parts, &mut out);
    }

    if out.is_empty() {
        "[agent returned no text content]".to_string()
    } else {
        out.join("\n")
    }
}

/// A client to talk to A2A agents over HTTP. Holds nothing per-agent — every
/// call names the site or endpoint it's for — so one instance is fine to
/// share across every `discover_a2a_agent`/`send_a2a_message` call an agent
/// makes.
pub struct A2aClient {
    http: reqwest::Client,
}

impl A2aClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self { http })
    }

    /// Fetch the agent card hosted at `base_url`. Tries the current spec's
    /// well-known path first, then the earlier draft's path — both are seen
    /// on real, live agents, since the path changed mid-spec.
    pub async fn fetch_agent_card(&self, base_url: &str) -> Result<AgentCard> {
        let base = base_url.trim_end_matches('/');
        for path in ["/.well-known/agent-card.json", "/.well-known/agent.json"] {
            let url = format!("{base}{path}");
            let Ok(resp) = self.http.get(&url).send().await else { continue };
            if !resp.status().is_success() {
                continue;
            }
            let body: Value = resp.json().await.with_context(|| format!("{url} did not return valid JSON"))?;
            return AgentCard::from_value(&body).with_context(|| format!("{url} is not a valid agent card"));
        }
        Err(anyhow!("no agent card found at {base} (tried both well-known paths)"))
    }

    /// Send one message to an agent's JSON-RPC endpoint (the `url` field
    /// from its [`AgentCard`], not necessarily the site's own root URL) and
    /// return whatever text it replied with. One request, one response — no
    /// streaming, no follow-up task polling if the agent answers `working`
    /// instead of `completed`.
    pub async fn send_message(&self, agent_url: &str, text: &str) -> Result<String> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "kind": "message",
                    "parts": [{ "kind": "text", "text": text }],
                }
            }
        });

        let resp: Value = self
            .http
            .post(agent_url)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("request to {agent_url} failed"))?
            .json()
            .await
            .with_context(|| format!("{agent_url} did not return valid JSON-RPC"))?;

        if let Some(err) = resp.get("error") {
            return Err(anyhow!("agent at {agent_url} returned a JSON-RPC error: {err}"));
        }
        let result =
            resp.get("result").ok_or_else(|| anyhow!("agent at {agent_url} returned neither 'result' nor 'error'"))?;
        Ok(extract_text(result))
    }
}

/// Fetches and summarizes a site's A2A agent card.
pub struct DiscoverA2aAgent {
    client: A2aClient,
}

impl DiscoverA2aAgent {
    pub fn new() -> Result<Self> {
        Ok(Self { client: A2aClient::new()? })
    }
}

#[async_trait]
impl Tool for DiscoverA2aAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "discover_a2a_agent".into(),
            description: "Fetch the A2A agent card for the site at {url} (its root URL, e.g. \
                https://example.com) and summarize the agent it hosts: name, description, \
                skills, and the endpoint to actually message it at. Use that endpoint (not this \
                input url) with send_a2a_message. Args: {\"url\": <string>}."
                .into(),
        }
    }

    async fn call(&self, args: Value, _svc: &GuardedServices) -> Result<String> {
        let url = args.get("url").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'url'"))?;
        let card = self.client.fetch_agent_card(url).await?;
        Ok(card.summarize())
    }
}

/// Sends one message to a remote A2A agent and returns its reply.
pub struct SendA2aMessage {
    client: A2aClient,
}

impl SendA2aMessage {
    pub fn new() -> Result<Self> {
        Ok(Self { client: A2aClient::new()? })
    }
}

#[async_trait]
impl Tool for SendA2aMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_a2a_message".into(),
            description: "Send {text} to a remote A2A agent's endpoint (the url from \
                discover_a2a_agent's summary, not the site's root url) and return its reply. \
                One request/response — the remote agent won't remember this as a conversation \
                across separate calls unless it manages that itself. Args: \
                {\"agent_url\": <string>, \"text\": <string>}."
                .into(),
        }
    }

    async fn call(&self, args: Value, _svc: &GuardedServices) -> Result<String> {
        let agent_url =
            args.get("agent_url").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'agent_url'"))?;
        let text = args.get("text").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'text'"))?;
        self.client.send_message(agent_url, text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A tiny one-shot-per-connection HTTP/1.1 responder: reads a request,
    /// answers `get_body` to any GET and `post_body` to anything else. Just
    /// enough to exercise `A2aClient` over a real socket without pulling in
    /// an HTTP server crate.
    async fn stub_server(get_body: &'static str, post_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = if req.starts_with("GET") { get_body } else { post_body };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetch_agent_card_parses_name_url_and_skills() {
        let card = r#"{"name":"Echo Agent","description":"echoes back","url":"http://echo.example/rpc","version":"1.0","skills":[{"id":"echo","name":"Echo","description":"repeats input"}]}"#;
        let base = stub_server(card, "{}").await;
        let client = A2aClient::new().unwrap();
        let card = client.fetch_agent_card(&base).await.unwrap();
        assert_eq!(card.name, "Echo Agent");
        assert_eq!(card.url, "http://echo.example/rpc");
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].name, "Echo");
    }

    #[tokio::test]
    async fn fetch_agent_card_errors_when_none_is_found() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let client = A2aClient::new().unwrap();
        let err = client.fetch_agent_card(&format!("http://{addr}")).await.unwrap_err();
        assert!(err.to_string().contains("no agent card found"));
    }

    #[tokio::test]
    async fn send_message_extracts_text_from_a_message_result() {
        let rpc = r#"{"jsonrpc":"2.0","id":"1","result":{"kind":"message","role":"agent","parts":[{"kind":"text","text":"hello back"}]}}"#;
        let base = stub_server("{}", rpc).await;
        let client = A2aClient::new().unwrap();
        let reply = client.send_message(&format!("{base}/rpc"), "hi").await.unwrap();
        assert_eq!(reply, "hello back");
    }

    #[tokio::test]
    async fn send_message_extracts_text_from_a_completed_task_artifact() {
        let rpc = r#"{"jsonrpc":"2.0","id":"1","result":{"kind":"task","id":"t1","status":{"state":"completed"},"artifacts":[{"artifactId":"a1","parts":[{"kind":"text","text":"task done: 42"}]}]}}"#;
        let base = stub_server("{}", rpc).await;
        let client = A2aClient::new().unwrap();
        let reply = client.send_message(&format!("{base}/rpc"), "compute").await.unwrap();
        assert_eq!(reply, "task done: 42");
    }

    #[tokio::test]
    async fn send_message_falls_back_to_the_working_status_message() {
        let rpc = r#"{"jsonrpc":"2.0","id":"1","result":{"kind":"task","id":"t1","status":{"state":"working","message":{"role":"agent","parts":[{"kind":"text","text":"still thinking"}]}}}}"#;
        let base = stub_server("{}", rpc).await;
        let client = A2aClient::new().unwrap();
        let reply = client.send_message(&format!("{base}/rpc"), "hi").await.unwrap();
        assert_eq!(reply, "still thinking");
    }

    #[tokio::test]
    async fn send_message_surfaces_a_jsonrpc_error() {
        let rpc = r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32601,"message":"method not found"}}"#;
        let base = stub_server("{}", rpc).await;
        let client = A2aClient::new().unwrap();
        let err = client.send_message(&format!("{base}/rpc"), "hi").await.unwrap_err();
        assert!(err.to_string().contains("method not found"));
    }
}
