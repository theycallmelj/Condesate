//! The audit trail: an append-only record of every boundary crossing.
//!
//! The rule is simple and load-bearing: **every decision is recorded, including
//! the denials.** Denials are the interesting half — they are how you find a
//! prompt-injected agent probing for reach, and how you discover that a grant
//! is too narrow for honest work.
//!
//! An audit record answers five questions:
//!   *who* ([`AuditEvent::principal`]), *what* (action + resource),
//!   *decided how* ([`AuditEvent::decision`]), *with what result*
//!   ([`AuditEvent::outcome`]), and *because of which inbound message*
//!   ([`AuditEvent::activation`]).
//!
//! That last one matters more than it looks: without a correlation id you can
//! see that an agent read a secret, but not which message talked it into
//! doing so.

use super::policy::{Action, Decision, Obligation};
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Timestamps come from a trait so tests can be deterministic and so a
/// distributed deployment can substitute a logical clock.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Always returns the same instant. For tests and replay.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What actually happened after the decision was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Allowed and the effect completed.
    Completed,
    /// Allowed but the effect itself failed (tool error, closed inbox, ...).
    Failed(String),
    /// Denied or escalated — no effect was attempted.
    NotAttempted,
}

/// Flattened principal fields. Denormalized on purpose: an audit record must
/// stay readable years later, without resolving live objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalRef {
    /// Full [`super::identity::AgentUid`], stringified — the precise,
    /// collision-proof identity. [`AuditEvent::summarize`] prints only its
    /// first 8 characters; this field always has the whole thing.
    pub uid: String,
    pub harness: String,
    pub agent: String,
    pub tenant: String,
    pub model_class: String,
    pub trust: String,
}

impl From<&super::identity::Principal> for PrincipalRef {
    fn from(p: &super::identity::Principal) -> Self {
        Self {
            uid: p.uid.to_string(),
            harness: p.harness.0.clone(),
            agent: p.agent.clone(),
            tenant: p.tenant.0.clone(),
            model_class: p.model_class.pool_key(),
            trust: format!("{:?}", p.trust),
        }
    }
}

/// One immutable line in the trail.
#[derive(Clone, Debug)]
pub struct AuditEvent {
    /// Monotonic per-sink sequence number. Gaps mean loss.
    pub seq: u64,
    pub at_ms: u64,
    pub principal: PrincipalRef,
    pub action: Action,
    /// [`super::policy::Resource::describe`] output.
    pub resource: String,
    pub decision: Decision,
    pub obligations: Vec<Obligation>,
    pub outcome: Outcome,
    /// Correlation id: the activation (inbound message) that caused this.
    pub activation: String,
    /// Grant epoch in force. A change here is a revocation boundary.
    pub epoch: u64,
    /// Optional hash chain over the previous record, for tamper evidence. A
    /// sink that fills this in makes silent deletion detectable.
    pub prev_digest: Option<String>,
}

impl AuditEvent {
    /// One-line human form. Denials are prefixed so they grep out cleanly.
    pub fn summarize(&self) -> String {
        let verdict = match &self.decision {
            Decision::Allow { rule, .. } => format!("ALLOW[{rule}]"),
            Decision::Deny { rule, reason } => match rule {
                Some(r) => format!("DENY[{r}] {reason:?}"),
                None => format!("DENY {reason:?}"),
            },
            Decision::Escalate { rule, to } => format!("ESCALATE[{rule}] -> {to:?}"),
        };
        // First 8 chars of the uid, git-short-hash style — enough to tell
        // two same-named instances apart at a glance; the full value lives
        // in `self.principal.uid` for anything that needs precision.
        let short_uid = &self.principal.uid[..8.min(self.principal.uid.len())];
        format!(
            "#{} {} {}/{} {:?} {} {} ({:?})",
            self.seq,
            self.at_ms,
            self.principal.agent,
            short_uid,
            self.action,
            self.resource,
            verdict,
            self.outcome,
        )
    }
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// Where audit records go.
///
/// Implementations must be append-only and must not silently drop: if the sink
/// cannot record, the kernel treats the whole operation as failed rather than
/// performing an unlogged effect. An unauditable action does not happen.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn append(&self, event: AuditEvent) -> Result<()>;

    /// Force durability. Called at activation boundaries and at shutdown.
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Keeps everything in memory. For tests and the demo.
#[derive(Default)]
pub struct MemoryAudit {
    events: Mutex<Vec<AuditEvent>>,
}

impl MemoryAudit {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().await.clone()
    }

    /// Every record whose decision was not an allow.
    pub async fn refusals(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .await
            .iter()
            .filter(|e| !e.decision.is_allow())
            .cloned()
            .collect()
    }
}

#[async_trait]
impl AuditSink for MemoryAudit {
    async fn append(&self, event: AuditEvent) -> Result<()> {
        self.events.lock().await.push(event);
        Ok(())
    }
}

/// Mirrors records to stderr as they arrive, then forwards to an inner sink.
/// Useful while developing policy: you see refusals the moment they happen.
pub struct TracingAudit {
    pub inner: Arc<dyn AuditSink>,
}

#[async_trait]
impl AuditSink for TracingAudit {
    async fn append(&self, event: AuditEvent) -> Result<()> {
        eprintln!("audit {}", event.summarize());
        self.inner.append(event).await
    }

    async fn flush(&self) -> Result<()> {
        self.inner.flush().await
    }
}

/// Appends each record as one `summarize()` line to a file, then forwards to
/// an inner sink. Unlike [`TracingAudit`], this survives past the process
/// exiting — opens (creating if needed) once in append mode and keeps the
/// handle for the sink's lifetime, so a crash mid-run still leaves every
/// record written before it on disk.
pub struct FileAudit {
    file: Mutex<File>,
    inner: Arc<dyn AuditSink>,
}

impl FileAudit {
    pub async fn open(path: impl AsRef<Path>, inner: Arc<dyn AuditSink>) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path).await?;
        Ok(Self { file: Mutex::new(file), inner })
    }
}

#[async_trait]
impl AuditSink for FileAudit {
    async fn append(&self, event: AuditEvent) -> Result<()> {
        let mut line = event.summarize();
        line.push('\n');
        self.file.lock().await.write_all(line.as_bytes()).await?;
        self.inner.append(event).await
    }

    async fn flush(&self) -> Result<()> {
        self.file.lock().await.flush().await?;
        self.inner.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::super::identity::{ModelClass, Principal, TenantId, TrustTier};
    use super::super::policy::{DenyReason, RuleId};
    use super::*;
    use crate::types::HarnessId;

    fn principal() -> Principal {
        Principal {
            uid: super::super::identity::AgentUid::new(),
            harness: HarnessId::new("w"),
            agent: "worker".into(),
            model_class: ModelClass {
                provider: "local".into(),
                family: "llama-3".into(),
                revision: "llama-3-8b".into(),
                embedding_space: None,
                quantization: None,
            },
            tenant: TenantId::new("acme"),
            trust: TrustTier::Sandboxed,
            parent: None,
            parent_uid: None,
        }
    }

    fn event(seq: u64, decision: Decision) -> AuditEvent {
        AuditEvent {
            seq,
            at_ms: FixedClock(42).now_ms(),
            principal: PrincipalRef::from(&principal()),
            action: Action::Read,
            resource: "mem:proj/x".into(),
            decision,
            obligations: vec![],
            outcome: Outcome::NotAttempted,
            activation: "act-1".into(),
            epoch: 0,
            prev_digest: None,
        }
    }

    #[tokio::test]
    async fn refusals_filters_out_allows() {
        let sink = MemoryAudit::new();
        sink.append(event(1, Decision::Allow { rule: RuleId::new("r"), obligations: vec![] }))
            .await
            .unwrap();
        sink.append(event(2, Decision::unmatched())).await.unwrap();

        assert_eq!(sink.events().await.len(), 2);
        let refused = sink.refusals().await;
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].seq, 2);
        assert!(matches!(
            refused[0].decision,
            Decision::Deny { reason: DenyReason::NoMatchingRule, .. }
        ));
    }

    #[tokio::test]
    async fn summary_marks_denials() {
        let e = event(7, Decision::unmatched());
        let s = e.summarize();
        assert!(s.contains("DENY"), "{s}");
        assert!(s.contains("mem:proj/x"), "{s}");
    }

    #[tokio::test]
    async fn file_audit_writes_one_summarize_line_per_event_and_still_forwards() {
        let path = std::env::temp_dir().join(format!("condesate-file-audit-test-{}.log", std::process::id()));
        let _ = tokio::fs::remove_file(&path).await;

        let inner = MemoryAudit::new();
        let sink = FileAudit::open(&path, inner.clone()).await.unwrap();
        sink.append(event(1, Decision::Allow { rule: RuleId::new("r"), obligations: vec![] }))
            .await
            .unwrap();
        sink.append(event(2, Decision::unmatched())).await.unwrap();
        sink.flush().await.unwrap();

        // Forwarded to the inner sink exactly like TracingAudit does.
        assert_eq!(inner.events().await.len(), 2);

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "{contents}");
        assert!(lines[0].contains("ALLOW[r]"), "{}", lines[0]);
        assert!(lines[1].contains("DENY"), "{}", lines[1]);

        tokio::fs::remove_file(&path).await.unwrap();
    }
}
