//! Tools the agent loop can invoke.
//!
//! A `Tool` receives the parsed JSON args *and* a `&GuardedServices`, so a tool
//! can reach shared storage or the message bus — but only as far as the
//! calling principal's grants allow. There is no raw `ServiceHandle` reachable
//! from tool code, so a tool cannot forget to go through the boundary; the
//! call-time check that gates *which* tools may run at all happens one layer
//! up, in the loop, before `call` is ever invoked (see `loops::execute_tools`).

use crate::security::kernel::GuardedServices;
use crate::types::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, args: Value, svc: &GuardedServices) -> Result<String>;
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
    async fn call(&self, args: Value, _svc: &GuardedServices) -> Result<String> {
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
    async fn call(&self, args: Value, svc: &GuardedServices) -> Result<String> {
        let key = arg(&args, "key")?;
        let value = arg(&args, "value")?;
        svc.storage_set(key, value).await?;
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
    async fn call(&self, args: Value, svc: &GuardedServices) -> Result<String> {
        let to = arg(&args, "to")?;
        let text = arg(&args, "text")?;
        svc.send_task(to, text).await?;
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
    async fn call(&self, _args: Value, svc: &GuardedServices) -> Result<String> {
        svc.broadcast_shutdown().await?;
        Ok("shutdown broadcast".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::audit::{FixedClock, MemoryAudit};
    use crate::security::identity::{ModelClass, TenantId, TrustTier};
    use crate::security::kernel::{AgentManifest, Kernel};
    use crate::security::policy::{
        Action, GrantSet, Pattern, ResourcePattern, Rule, RuleSetPolicy, SubjectMatch,
    };
    use crate::swarm::bus::{Bus, Inbox, Payload};
    use crate::swarm::service::ServiceHandle;
    use crate::swarm::storage::InMemoryStorage;
    use crate::types::HarnessId;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn test_class() -> ModelClass {
        ModelClass {
            provider: "test".into(),
            family: "test".into(),
            revision: "test".into(),
            embedding_space: None,
            quantization: None,
        }
    }

    /// A grant set wide enough to exercise tool bodies without itself being a
    /// policy test — narrower-grant behaviour is covered in `kernel::tests`
    /// and `policy::tests`.
    fn full_access(agent: &str) -> Vec<Rule> {
        vec![
            Rule::allow(
                "mem",
                SubjectMatch::agent(agent),
                &[Action::Read, Action::Write],
                ResourcePattern::Memory(Pattern::Any),
            ),
            Rule::allow(
                "talk",
                SubjectMatch::agent(agent),
                &[Action::Send],
                ResourcePattern::Peer(Pattern::Any),
            ),
            Rule::allow(
                "control",
                SubjectMatch::agent(agent),
                &[Action::Control],
                ResourcePattern::Swarm,
            ),
        ]
    }

    /// Build a guarded handle for `me`, plus inboxes for every id, for testing.
    fn guarded(me: &str, ids: &[&str]) -> (Arc<GuardedServices>, HashMap<HarnessId, Inbox>) {
        let mut routes = HashMap::new();
        let mut inboxes = HashMap::new();
        for id in ids {
            let (tx, rx) = mpsc::unbounded_channel();
            let hid = HarnessId::new(*id);
            routes.insert(hid.clone(), tx);
            inboxes.insert(hid, rx);
        }
        let raw = ServiceHandle {
            me: HarnessId::new(me),
            roster: Arc::new(ids.iter().map(|s| HarnessId::new(*s)).collect()),
            storage: InMemoryStorage::new(),
            bus: Bus::new(routes),
        };

        let requested = full_access(me);
        let kernel = Kernel::new(
            GrantSet::new(requested.clone()),
            Arc::new(RuleSetPolicy::new()),
            MemoryAudit::new(),
            Arc::new(FixedClock(0)),
        );
        let manifest = AgentManifest {
            harness: HarnessId::new(me),
            agent: me.to_string(),
            model_class: test_class(),
            tenant: TenantId::new("test"),
            requested_trust: TrustTier::Privileged,
            requested,
            cache_classes: vec![],
        };
        let admission = kernel.admit(&manifest, None);
        let svc = kernel.attach(&admission, raw);
        svc.begin_activation("test", u64::MAX);
        (Arc::new(svc), inboxes)
    }

    #[tokio::test]
    async fn word_count_counts_whitespace_tokens() {
        let (svc, _) = guarded("a", &["a"]);
        let out = WordCount
            .call(json!({ "text": "ship fast stay safe" }), &svc)
            .await
            .unwrap();
        assert_eq!(out, "4");
    }

    #[tokio::test]
    async fn word_count_missing_arg_errors() {
        let (svc, _) = guarded("a", &["a"]);
        assert!(WordCount.call(json!({}), &svc).await.is_err());
    }

    #[tokio::test]
    async fn remember_writes_to_shared_storage() {
        let (svc, _) = guarded("a", &["a"]);
        Remember
            .call(json!({ "key": "k", "value": "v" }), &svc)
            .await
            .unwrap();
        assert_eq!(svc.storage_get("k").await.unwrap().as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn send_message_delivers_over_bus() {
        let (svc, mut inboxes) = guarded("a", &["a", "b"]);
        SendMessage
            .call(json!({ "to": "b", "text": "ping" }), &svc)
            .await
            .unwrap();
        let got = inboxes.get_mut(&HarnessId::new("b")).unwrap().recv().await.unwrap();
        assert_eq!(got.from, HarnessId::new("a"));
        assert!(matches!(got.payload, Payload::Task(t) if t == "ping"));
    }

    #[tokio::test]
    async fn shutdown_swarm_is_refused_without_a_control_grant() {
        // A principal with no Control grant cannot broadcast shutdown, even
        // though the tool is otherwise callable.
        let raw = ServiceHandle {
            me: HarnessId::new("a"),
            roster: Arc::new(vec![HarnessId::new("a")]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(HashMap::new()),
        };
        let requested: Vec<Rule> =
            full_access("a").into_iter().filter(|r| r.id.0 != "control").collect();
        let kernel = Kernel::new(
            GrantSet::new(requested.clone()),
            Arc::new(RuleSetPolicy::new()),
            MemoryAudit::new(),
            Arc::new(FixedClock(0)),
        );
        let manifest = AgentManifest {
            harness: HarnessId::new("a"),
            agent: "a".into(),
            model_class: test_class(),
            tenant: TenantId::new("test"),
            requested_trust: TrustTier::Privileged,
            requested,
            cache_classes: vec![],
        };
        let admission = kernel.admit(&manifest, None);
        let no_control = kernel.attach(&admission, raw);
        no_control.begin_activation("test", u64::MAX);

        assert!(ShutdownSwarm.call(json!({}), &no_control).await.is_err());
    }
}
