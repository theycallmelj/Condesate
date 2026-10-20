//! Shared plumbing for eval cases: a deterministic `ModelProvider` that
//! plays back a scripted list of turns, and a helper that admits a
//! principal through the real `Kernel` so every case runs behind the same
//! permission boundary production code does.

use condesate::{
    Action, AgentManifest, CompletionRequest, CompletionResponse, GrantSet, GuardedServices,
    HarnessId, Kernel, MemoryAudit, ModelClass, ModelProvider, Pattern, ResourcePattern, Rule,
    RuleSetPolicy, ServiceHandle, SubjectMatch, SystemClock, TenantId, TrustTier,
};
use anyhow::Result;
use async_trait::async_trait;
use condesate::{Bus, InMemoryStorage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Returns the next scripted response on every call, ignoring the request —
/// the eval cases already know exactly what plan they're driving, so there's
/// nothing to branch on (unlike `condesate::LocalModel`/`CloudModel`, which
/// script off message content because the chat-app demo shares one provider
/// across turns with real user input).
pub struct PlaybackModel {
    turns: Vec<CompletionResponse>,
    cursor: AtomicUsize,
}

impl PlaybackModel {
    pub fn new(turns: Vec<CompletionResponse>) -> Self {
        Self { turns, cursor: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl ModelProvider for PlaybackModel {
    fn name(&self) -> &str {
        "playback-eval"
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse> {
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        Ok(self.turns.get(i).cloned().unwrap_or_else(|| self.turns.last().cloned().unwrap_or_default()))
    }
}

fn eval_model_class() -> ModelClass {
    ModelClass {
        provider: "evals".into(),
        family: "evals".into(),
        revision: "evals".into(),
        embedding_space: None,
        quantization: None,
    }
}

/// Admit `me` into a fresh `Kernel` with exactly `grants`, and build a
/// guarded handle plus one inbox per id in `peers` (which should include
/// `me`). Mirrors the `guarded()` helper `condesate`'s own tool/loop tests
/// use, so an eval case exercises the identical admission path production
/// code does — no shortcuts around the boundary.
pub fn build_services(
    me: &str,
    peers: &[&str],
    grants: Vec<Rule>,
) -> (Arc<GuardedServices>, HashMap<HarnessId, mpsc::UnboundedReceiver<condesate::Envelope>>) {
    let mut routes = HashMap::new();
    let mut inboxes = HashMap::new();
    for id in peers {
        let (tx, rx) = mpsc::unbounded_channel();
        let hid = HarnessId::new(*id);
        routes.insert(hid.clone(), tx);
        inboxes.insert(hid, rx);
    }

    let raw = ServiceHandle {
        me: HarnessId::new(me),
        roster: Arc::new(peers.iter().map(|s| HarnessId::new(*s)).collect()),
        storage: InMemoryStorage::new(),
        bus: Bus::new(routes),
    };

    let kernel = Kernel::new(
        GrantSet::new(grants.clone()),
        Arc::new(RuleSetPolicy::new()),
        MemoryAudit::new(),
        Arc::new(SystemClock),
    );
    let manifest = AgentManifest {
        harness: HarnessId::new(me),
        agent: me.to_string(),
        model_class: eval_model_class(),
        tenant: TenantId::new("evals"),
        requested_trust: TrustTier::Privileged,
        requested: grants,
        cache_classes: vec![],
    };
    let admission = kernel.admit(&manifest, None);
    let svc = kernel.attach(&admission, raw);
    svc.begin_activation("eval", u64::MAX);
    (Arc::new(svc), inboxes)
}

/// A broad tool-invocation grant — most cases just need "can call tools",
/// with narrower per-resource grants (memory, peer) layered on where the
/// case actually needs them.
pub fn invoke_any_tool(agent: &str) -> Rule {
    Rule::allow(
        "tools",
        SubjectMatch::agent(agent),
        &[Action::Invoke],
        ResourcePattern::Tool(Pattern::Any),
    )
}

pub fn read_write_memory(agent: &str) -> Rule {
    Rule::allow(
        "mem",
        SubjectMatch::agent(agent),
        &[Action::Read, Action::Write],
        ResourcePattern::Memory(Pattern::Any),
    )
}

pub fn send_to_any_peer(agent: &str) -> Rule {
    Rule::allow("talk", SubjectMatch::agent(agent), &[Action::Send], ResourcePattern::Peer(Pattern::Any))
}

/// Scopes an Invoke grant to exactly one tool name — e.g. one namespaced MCP
/// tool (`mcp:<server>:<tool>`) rather than every tool a server advertises.
pub fn invoke_exact_tool(agent: &str, tool_name: &str) -> Rule {
    Rule::allow(
        "tool",
        SubjectMatch::agent(agent),
        &[Action::Invoke],
        ResourcePattern::Tool(Pattern::Exact(tool_name.to_string())),
    )
}
