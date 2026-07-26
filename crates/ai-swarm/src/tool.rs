//! Tools the agent loop can invoke.
//!
//! A `Tool` receives the parsed JSON args *and* the `ServiceHandle`, so a tool
//! can reach shared storage or the message bus. That's what makes tools like
//! `send_message` and `shutdown_swarm` possible without special-casing them in
//! the loop — they're just tools that happen to use swarm services.

use crate::service::ServiceHandle;
use crate::types::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, args: Value, svc: &ServiceHandle) -> Result<String>;
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing string arg '{key}'"))
}

/// Pure computation tool: counts whitespace-separated words.
pub struct WordCount;

#[async_trait]
impl Tool for WordCount {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "word_count".into(),
            description: "Count the words in {text}.".into(),
        }
    }
    async fn call(&self, args: Value, _svc: &ServiceHandle) -> Result<String> {
        let text = arg(&args, "text")?;
        Ok(text.split_whitespace().count().to_string())
    }
}

/// Writes a value into shared storage.
pub struct Remember;

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "Store {value} under {key} in shared storage.".into(),
        }
    }
    async fn call(&self, args: Value, svc: &ServiceHandle) -> Result<String> {
        let key = arg(&args, "key")?;
        let value = arg(&args, "value")?;
        svc.storage.set(key, value).await?;
        Ok(format!("stored '{key}'"))
    }
}

/// Sends a task/reply to another harness over the bus.
pub struct SendMessage;

#[async_trait]
impl Tool for SendMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_message".into(),
            description: "Send {text} to harness {to}.".into(),
        }
    }
    async fn call(&self, args: Value, svc: &ServiceHandle) -> Result<String> {
        let to = arg(&args, "to")?;
        let text = arg(&args, "text")?;
        svc.send_task(to, text)?;
        Ok(format!("sent to '{to}'"))
    }
}

/// Broadcasts a cooperative shutdown to the whole swarm.
pub struct ShutdownSwarm;

#[async_trait]
impl Tool for ShutdownSwarm {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shutdown_swarm".into(),
            description: "Signal every harness to stop.".into(),
        }
    }
    async fn call(&self, _args: Value, svc: &ServiceHandle) -> Result<String> {
        svc.broadcast_shutdown()?;
        Ok("shutdown broadcast".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Bus, Inbox, Payload};
    use crate::storage::InMemoryStorage;
    use crate::types::HarnessId;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Build a ServiceHandle for `me`, plus inboxes for every id, for testing.
    fn services(me: &str, ids: &[&str]) -> (ServiceHandle, HashMap<HarnessId, Inbox>) {
        let mut routes = HashMap::new();
        let mut inboxes = HashMap::new();
        for id in ids {
            let (tx, rx) = mpsc::unbounded_channel();
            let hid = HarnessId::new(*id);
            routes.insert(hid.clone(), tx);
            inboxes.insert(hid, rx);
        }
        let svc = ServiceHandle {
            me: HarnessId::new(me),
            roster: Arc::new(ids.iter().map(|s| HarnessId::new(*s)).collect()),
            storage: InMemoryStorage::new(),
            bus: Bus::new(routes),
        };
        (svc, inboxes)
    }

    #[tokio::test]
    async fn word_count_counts_whitespace_tokens() {
        let (svc, _) = services("a", &["a"]);
        let out = WordCount
            .call(json!({ "text": "ship fast stay safe" }), &svc)
            .await
            .unwrap();
        assert_eq!(out, "4");
    }

    #[tokio::test]
    async fn word_count_missing_arg_errors() {
        let (svc, _) = services("a", &["a"]);
        assert!(WordCount.call(json!({}), &svc).await.is_err());
    }

    #[tokio::test]
    async fn remember_writes_to_shared_storage() {
        let (svc, _) = services("a", &["a"]);
        Remember
            .call(json!({ "key": "k", "value": "v" }), &svc)
            .await
            .unwrap();
        assert_eq!(svc.storage.get("k").await.unwrap().as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn send_message_delivers_over_bus() {
        let (svc, mut inboxes) = services("a", &["a", "b"]);
        SendMessage
            .call(json!({ "to": "b", "text": "ping" }), &svc)
            .await
            .unwrap();
        let got = inboxes.get_mut(&HarnessId::new("b")).unwrap().recv().await.unwrap();
        assert_eq!(got.from, HarnessId::new("a"));
        assert!(matches!(got.payload, Payload::Task(t) if t == "ping"));
    }
}
