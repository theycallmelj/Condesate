//! The kernel: setup, enforcement, and audit for the whole swarm.
//!
//! Everything in [`super::policy`], [`super::audit`], and [`super::identity`] is
//! inert data until something puts it in the path of a real call. That is this
//! module. The kernel owns the boundary; agents and tools never touch it.
//!
//! ## The lifecycle
//!
//! ```text
//!   1. DECLARE   AgentManifest      what the agent says it needs
//!   2. ADMIT     Kernel::admit      intersect with authority actually held
//!                                   → Principal + GrantSet   (never a union)
//!   3. ATTACH    Kernel::attach     wrap ServiceHandle → GuardedServices,
//!                                   carrying the admitted GrantSet with it
//!   4. ADVERTISE may_advertise      the model is shown only permitted tools
//!   5. CALL      guarded syscall    tool or agent reaches for something
//!   6. CHECK     PolicyEngine       evaluated against *this* GrantSet
//!                                   → Allow / Deny / Escalate (+ obligations)
//!   7. RECORD    AuditSink          every outcome, allow and deny alike
//!   8. EFFECT    inner service      only now does anything actually happen
//!   9. REVOKE    Kernel::revoke     bump the epoch; live handles go stale
//! ```
//!
//! Steps 6 and 7 are not optional and not reorderable. An effect that could not
//! be audited does not run — see [`GuardedServices::check`].
//!
//! Step 6 is evaluated against the `GrantSet` computed at step 2, not against
//! some separate swarm-wide rule list. That is what makes attenuation real:
//! rules a manifest asked for and did not receive (see [`Admission::dropped`])
//! are simply absent from what gets checked, not merely mismatched by a
//! `SubjectMatch` pattern that happens to be looser elsewhere.
//!
//! ## Why the guard is here and not in the tools
//!
//! [`crate::agent::tool::Tool`] implementations receive a `&GuardedServices` and can
//! do only what it permits — there is no raw `ServiceHandle` reachable from
//! tool code. Wrapping the handle instead of trusting each tool to remember a
//! check is what makes the boundary the *only* gate rather than an optional
//! extra one.

use super::audit::{AuditEvent, AuditSink, Clock, Outcome, PrincipalRef};
use super::identity::{Compatibility, GovernanceLabel, ModelClass, Principal, TenantId, TrustTier};
use super::policy::{
    AccessRequest, Action, Approver, Decision, DenyReason, GrantSet, Obligation, PolicyEngine,
    RequestContext, Resource, Rule, RuleId,
};
use crate::cache::{CacheRegistry, Candidate, Demand, ValueClass};
use crate::swarm::service::ServiceHandle;
use crate::types::HarnessId;
use anyhow::Result;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A denied access, in the form a caller sees.
#[derive(Clone, Debug)]
pub struct Denied {
    pub principal: String,
    pub action: Action,
    pub resource: String,
    pub reason: DenyReason,
    pub rule: Option<RuleId>,
}

/// What a failed check returns. Separate from [`anyhow::Error`] so callers can
/// distinguish "not permitted" from "broke".
#[derive(Clone, Debug)]
pub enum Refusal {
    Denied(Denied),
    /// Held pending approval. Not a failure — the caller should retry after the
    /// approver answers, or surface it as a request for consent.
    Escalated { to: Approver, rule: RuleId, resource: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Denied(d) => write!(
                f,
                "denied: {} may not {:?} {} ({:?}{})",
                d.principal,
                d.action,
                d.resource,
                d.reason,
                d.rule.as_ref().map(|r| format!(", rule {r}")).unwrap_or_default()
            ),
            Refusal::Escalated { to, rule, resource } => {
                write!(f, "escalated to {to:?}: {resource} (rule {rule})")
            }
        }
    }
}

impl std::error::Error for Refusal {}

// ---------------------------------------------------------------------------
// Manifests and admission
// ---------------------------------------------------------------------------

/// What an agent declares it needs, before it is allowed to exist.
///
/// A manifest is a *request*, never a grant. Admission intersects it with the
/// authority the spawner actually holds, so a manifest asking for the world
/// produces a principal with nothing. This applies even at the top level: a
/// manifest with no parent still attenuates against the kernel's root, so a
/// harness that forgets to declare a rule it needs simply does not get it —
/// there is no implicit "top-level agents get everything" path.
#[derive(Clone, Debug)]
pub struct AgentManifest {
    pub harness: HarnessId,
    pub agent: String,
    pub model_class: ModelClass,
    pub tenant: TenantId,
    /// The tier the spawner is asking for. Capped at the spawner's own tier.
    pub requested_trust: TrustTier,
    /// The rules the agent wants in force for itself. Each must be covered —
    /// resource, actions, *and* subject narrowing — by a rule the authority
    /// above already holds; see [`super::policy::GrantSet::attenuate`].
    pub requested: Vec<Rule>,
    /// Cache classes this agent expects to read from. Recorded for review — a
    /// manifest that asks for a class it has no business in is a design smell
    /// worth seeing before it becomes a denial in production.
    pub cache_classes: Vec<ModelClass>,
}

/// The result of admission: an identity plus exactly the authority it holds.
#[derive(Clone, Debug)]
pub struct Admission {
    pub principal: Principal,
    pub grants: GrantSet,
    /// Rules the manifest asked for that were not covered by the spawner's own
    /// authority. Not an error — the agent simply runs without them — but the
    /// list is the single most useful thing to look at when an agent starts
    /// failing in ways nobody expected.
    pub dropped: Vec<RuleId>,
}

// ---------------------------------------------------------------------------
// The kernel
// ---------------------------------------------------------------------------

/// Owns policy, audit, time, and the cache registry. One per swarm.
pub struct Kernel {
    pub policy: Arc<dyn PolicyEngine>,
    pub audit: Arc<dyn AuditSink>,
    pub clock: Arc<dyn Clock>,
    /// Present when the swarm runs a shared cache.
    pub cache: Option<Arc<dyn CacheRegistry>>,
    /// The authority everything else is attenuated from. Nothing in the swarm
    /// can hold a permission that is not covered here.
    root: GrantSet,
    seq: AtomicU64,
    epoch: AtomicU64,
}

impl Kernel {
    pub fn new(
        root: GrantSet,
        policy: Arc<dyn PolicyEngine>,
        audit: Arc<dyn AuditSink>,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            policy,
            audit,
            clock,
            cache: None,
            root,
            seq: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Invalidate every outstanding handle. The blunt instrument: cheap,
    /// immediate, and it stops in-flight work at the next syscall rather than
    /// mid-write.
    pub fn revoke_all(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Step 2: turn a manifest into a principal with attenuated authority.
    ///
    /// `parent` is the spawner's admission, or `None` for a top-level agent
    /// admitted directly against the kernel's root.
    pub fn admit(&self, manifest: &AgentManifest, parent: Option<&Admission>) -> Admission {
        let authority = parent.map(|p| &p.grants).unwrap_or(&self.root);
        let granted = authority.attenuate(&manifest.requested);

        let kept: Vec<&RuleId> = granted.rules.iter().map(|r| &r.id).collect();
        let dropped = manifest
            .requested
            .iter()
            .filter(|r| !kept.contains(&&r.id))
            .map(|r| r.id.clone())
            .collect();

        // Trust is capped at the spawner's tier: no agent creates a child more
        // privileged than itself.
        let trust = match parent {
            Some(p) => manifest.requested_trust.min(p.principal.trust),
            None => manifest.requested_trust.min(TrustTier::Privileged),
        };

        Admission {
            principal: Principal {
                harness: manifest.harness.clone(),
                agent: manifest.agent.clone(),
                model_class: manifest.model_class.clone(),
                tenant: manifest.tenant.clone(),
                trust,
                parent: parent.map(|p| p.principal.harness.clone()),
            },
            grants: GrantSet { epoch: self.epoch(), ..granted },
            dropped,
        }
    }

    /// Step 3: wrap the raw syscall surface in the guard, carrying the
    /// admitted `GrantSet` with it so every check evaluates against exactly
    /// what this principal holds.
    pub fn attach(self: &Arc<Self>, admission: &Admission, inner: ServiceHandle) -> GuardedServices {
        GuardedServices {
            inner,
            principal: admission.principal.clone(),
            grants: admission.grants.clone(),
            kernel: self.clone(),
            activation: Mutex::new(String::from("boot")),
            steps: AtomicU32::new(0),
            byte_budget: AtomicU64::new(0),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// The guarded syscall surface
// ---------------------------------------------------------------------------

/// What an agent and its tools actually hold.
///
/// Every method is: build request → evaluate against `self.grants` → audit →
/// act. There is no method that skips the middle two, and no raw
/// `ServiceHandle` reachable from here except through [`Self::unguarded`],
/// which exists only as a migration escape hatch.
pub struct GuardedServices {
    inner: ServiceHandle,
    principal: Principal,
    /// The exact authority admitted for this principal — never the kernel's
    /// full root, never a peer's. This is what [`Self::check`] evaluates
    /// against.
    grants: GrantSet,
    kernel: Arc<Kernel>,
    /// Correlation id for the message currently being handled.
    activation: Mutex<String>,
    steps: AtomicU32,
    byte_budget: AtomicU64,
}

impl GuardedServices {
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Start a new activation: fresh correlation id, fresh step count, fresh
    /// byte budget. Called by the harness once per inbound message.
    pub fn begin_activation(&self, id: impl Into<String>, byte_budget: u64) {
        *self.activation.lock().expect("activation lock") = id.into();
        self.steps.store(0, Ordering::SeqCst);
        self.byte_budget.store(byte_budget, Ordering::SeqCst);
    }

    /// Whether this principal's grants permit the model to even be told a
    /// tool exists. Advertise-time filtering — see the module note on the two
    /// enforcement points in [`super::policy`].
    pub fn may_advertise(&self, tool: &str) -> bool {
        self.kernel.policy.may_advertise(&self.grants, &self.principal, tool)
    }

    fn context(&self) -> RequestContext {
        RequestContext {
            activation: self.activation.lock().expect("activation lock").clone(),
            steps: self.steps.load(Ordering::SeqCst),
            byte_budget_remaining: self.byte_budget.load(Ordering::SeqCst),
            ..Default::default()
        }
    }

    /// The one path every syscall goes through.
    ///
    /// Returns the obligations attached to an allow. A failure to *record* the
    /// decision is a failure of the whole call: an effect nobody can see having
    /// happened is worse than an effect that did not happen.
    pub async fn check(
        &self,
        action: Action,
        resource: &Resource,
    ) -> std::result::Result<Vec<Obligation>, Refusal> {
        self.check_with(action, resource, self.context()).await
    }

    /// [`Self::check`] with extra runtime facts — value size, age, and model
    /// compatibility for cache requests.
    pub async fn check_with(
        &self,
        action: Action,
        resource: &Resource,
        ctx: RequestContext,
    ) -> std::result::Result<Vec<Obligation>, Refusal> {
        let current = self.kernel.epoch();
        let decision = if current != self.grants.epoch {
            Decision::Deny {
                rule: None,
                reason: DenyReason::StaleEpoch { held: self.grants.epoch, current },
            }
        } else {
            self.kernel
                .policy
                .evaluate(
                    &self.grants,
                    &AccessRequest { principal: &self.principal, action, resource, context: &ctx },
                )
                .await
        };

        let obligations = match &decision {
            Decision::Allow { obligations, .. } => obligations.clone(),
            _ => Vec::new(),
        };

        let event = AuditEvent {
            seq: self.kernel.next_seq(),
            at_ms: self.kernel.clock.now_ms(),
            principal: PrincipalRef::from(&self.principal),
            action,
            resource: resource.describe(),
            decision: decision.clone(),
            obligations: obligations.clone(),
            outcome: if decision.is_allow() { Outcome::Completed } else { Outcome::NotAttempted },
            activation: ctx.activation,
            epoch: current,
            prev_digest: None,
        };
        if self.kernel.audit.append(event).await.is_err() {
            // Unauditable: refuse rather than act invisibly.
            return Err(Refusal::Denied(Denied {
                principal: self.principal.to_string(),
                action,
                resource: resource.describe(),
                reason: DenyReason::ConditionFailed("audit sink unavailable".into()),
                rule: None,
            }));
        }

        match decision {
            Decision::Allow { .. } => Ok(obligations),
            Decision::Escalate { rule, to } => {
                Err(Refusal::Escalated { to, rule, resource: resource.describe() })
            }
            Decision::Deny { rule, reason } => Err(Refusal::Denied(Denied {
                principal: self.principal.to_string(),
                action,
                resource: resource.describe(),
                reason,
                rule,
            })),
        }
    }

    // -- memory -------------------------------------------------------------

    pub async fn storage_get(&self, key: &str) -> Result<Option<String>> {
        self.check(Action::Read, &Resource::Memory { key: key.to_string() }).await?;
        self.inner.storage.get(key).await
    }

    pub async fn storage_set(&self, key: &str, value: &str) -> Result<()> {
        self.check(Action::Write, &Resource::Memory { key: key.to_string() }).await?;
        self.inner.storage.set(key, value).await
    }

    /// Prefix listing, filtered to the keys this principal may actually read.
    ///
    /// The filter matters: an unfiltered listing leaks the *shape* of another
    /// agent's memory even when the values stay hidden.
    pub async fn storage_keys(&self, prefix: &str) -> Result<Vec<String>> {
        self.check(Action::Read, &Resource::Memory { key: prefix.to_string() }).await?;
        let all = self.inner.storage.keys(prefix).await?;
        let mut out = Vec::with_capacity(all.len());
        for key in all {
            if self.check(Action::Read, &Resource::Memory { key: key.clone() }).await.is_ok() {
                out.push(key);
            }
        }
        Ok(out)
    }

    // -- messaging ----------------------------------------------------------

    pub async fn send_task(&self, to: &str, text: &str) -> Result<()> {
        let peer = Resource::Peer { id: HarnessId::new(to) };
        self.check(Action::Send, &peer).await?;
        self.inner.send_task(to, text)
    }

    pub async fn send_reply(&self, to: &str, text: &str) -> Result<()> {
        let peer = Resource::Peer { id: HarnessId::new(to) };
        self.check(Action::Send, &peer).await?;
        self.inner.send_reply(to, text)
    }

    pub async fn broadcast_shutdown(&self) -> Result<()> {
        self.check(Action::Control, &Resource::Swarm).await?;
        self.inner.broadcast_shutdown()
    }

    // -- tools --------------------------------------------------------------

    /// Call-time authorization for a tool. The authoritative gate;
    /// [`Self::may_advertise`] (and [`super::policy::ToolBroker`]) only decide
    /// what the model gets *told* about.
    pub async fn authorize_tool(&self, name: &str) -> std::result::Result<Vec<Obligation>, Refusal> {
        self.steps.fetch_add(1, Ordering::SeqCst);
        self.check(Action::Invoke, &Resource::Tool { name: name.to_string() }).await
    }

    // -- shared cache -------------------------------------------------------

    /// Read the shared cache, applying all three gates in order.
    ///
    /// Geometry and compatibility come from the pool; authorization is applied
    /// here, per candidate, because a single lookup can return values under
    /// several different labels.
    pub async fn cache_lookup(&self, demand: &Demand) -> Result<Vec<Candidate>> {
        let Some(registry) = self.kernel.cache.clone() else {
            return Ok(Vec::new());
        };

        // Gate 2 first at the pool level: only consider pools this model class
        // may reuse at all, given the weakest class of value being asked for.
        let floor = demand
            .value_class
            .map(|v| v.min_compatibility())
            .unwrap_or(Compatibility::SameEmbeddingSpace);
        let pools = registry.compatible_pools(&demand.asker, floor).await?;

        let now = self.kernel.clock.now_ms();
        let mut cleared = Vec::new();
        for pool in pools {
            // Gate 1: geometry.
            for candidate in pool.lookup(demand).await? {
                // Gate 2, per value class — a pool can hold several classes and
                // the floor differs for each.
                if candidate.compatibility < candidate.entry.sidecar.value_class.min_compatibility()
                {
                    continue;
                }
                // Gate 3: authorization, against this candidate's own label.
                let resource = Resource::Cache {
                    class: pool.class().clone(),
                    value_class: Some(candidate.entry.sidecar.value_class),
                    label: Some(candidate.entry.sidecar.label.clone()),
                };
                let ctx = RequestContext {
                    value_bytes: Some(candidate.entry.sidecar.bytes),
                    value_age_ms: Some(candidate.entry.sidecar.age_ms(now)),
                    compatibility: Some(candidate.compatibility),
                    ..self.context()
                };
                if self.check_with(Action::Read, &resource, ctx).await.is_ok() {
                    cleared.push(candidate);
                }
            }
        }
        Ok(cleared)
    }

    /// Authorize a cache write. The pool still validates the entry itself; this
    /// only answers whether this principal may put this label in this pool.
    pub async fn authorize_cache_write(
        &self,
        class: &ModelClass,
        value_class: ValueClass,
        label: &GovernanceLabel,
    ) -> std::result::Result<Vec<Obligation>, Refusal> {
        let resource = Resource::Cache {
            class: class.clone(),
            value_class: Some(value_class),
            label: Some(label.clone()),
        };
        self.check(Action::Write, &resource).await
    }

    /// Authorize advertising a pool's contents to peers. Separate from `Read`
    /// because a sketch leaks shape, not values, and some deployments will want
    /// to allow local reuse while forbidding any outward advertisement.
    pub async fn authorize_publish(
        &self,
        class: &ModelClass,
    ) -> std::result::Result<Vec<Obligation>, Refusal> {
        let resource =
            Resource::Cache { class: class.clone(), value_class: None, label: None };
        self.check(Action::Publish, &resource).await
    }

    /// Escape hatch for code not yet ported to the guarded surface. Every call
    /// site is a hole in the boundary; there should eventually be none.
    pub fn unguarded(&self) -> &ServiceHandle {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::super::audit::{FixedClock, MemoryAudit};
    use super::super::policy::{Effect, Pattern, ResourcePattern, RuleSetPolicy, SubjectMatch};
    use super::*;
    use crate::swarm::bus::Bus;
    use crate::swarm::storage::InMemoryStorage;
    use std::collections::HashMap;

    fn class() -> ModelClass {
        ModelClass {
            provider: "local".into(),
            family: "llama-3".into(),
            revision: "llama-3-8b".into(),
            embedding_space: Some("emb-v1".into()),
            quantization: None,
        }
    }

    /// Root authority: scratch memory, peer messaging, and shutdown control
    /// (privileged only) for anyone whose *requested* manifest rules match
    /// this shape closely enough to be covered.
    fn root() -> GrantSet {
        GrantSet::new(vec![
            Rule::allow(
                "mem-scratch",
                SubjectMatch::default(),
                &[Action::Read, Action::Write],
                ResourcePattern::Memory(Pattern::parse("scratch/*")),
            ),
            Rule::allow(
                "talk",
                SubjectMatch::default(),
                &[Action::Send],
                ResourcePattern::Peer(Pattern::Any),
            ),
            Rule::allow(
                "shutdown",
                SubjectMatch { min_trust: Some(TrustTier::Privileged), ..Default::default() },
                &[Action::Control],
                ResourcePattern::Swarm,
            ),
        ])
    }

    fn kernel_with(root_grants: GrantSet) -> (Arc<Kernel>, Arc<MemoryAudit>) {
        let audit = MemoryAudit::new();
        let policy = Arc::new(RuleSetPolicy::new());
        let k = Kernel::new(root_grants, policy, audit.clone(), Arc::new(FixedClock(1_000)));
        (k, audit)
    }

    fn services(me: &str) -> ServiceHandle {
        ServiceHandle {
            me: HarnessId::new(me),
            roster: Arc::new(vec![HarnessId::new(me)]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(HashMap::new()),
        }
    }

    fn manifest(agent: &str, trust: TrustTier, requested: Vec<Rule>) -> AgentManifest {
        AgentManifest {
            harness: HarnessId::new(agent),
            agent: agent.to_string(),
            model_class: class(),
            tenant: TenantId::new("acme"),
            requested_trust: trust,
            requested,
            cache_classes: vec![class()],
        }
    }

    /// A manifest requesting exactly the scratch-memory rule `root()` grants,
    /// scoped to `agent`. The common case in these tests.
    fn scratch_rw(agent: &str) -> Rule {
        Rule::allow(
            "w-mem",
            SubjectMatch::agent(agent),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("scratch/*")),
        )
    }

    #[tokio::test]
    async fn admission_drops_what_the_root_never_held() {
        let (kernel, _) = kernel_with(root());
        let m = manifest(
            "worker",
            TrustTier::Standard,
            vec![
                Rule::allow(
                    "w-scratch",
                    SubjectMatch::agent("worker"),
                    &[Action::Write],
                    ResourcePattern::Memory(Pattern::parse("scratch/worker/*")),
                ),
                Rule::allow(
                    "w-everything",
                    SubjectMatch::agent("worker"),
                    &[Action::Read],
                    ResourcePattern::Memory(Pattern::Any),
                ),
            ],
        );
        let adm = kernel.admit(&m, None);
        assert_eq!(adm.dropped, vec![RuleId::new("w-everything")]);
        assert!(adm.grants.rules.iter().any(|r| r.id.0 == "w-scratch"));
    }

    #[tokio::test]
    async fn a_child_cannot_outrank_its_parent() {
        let (kernel, _) = kernel_with(root());
        let parent = kernel.admit(&manifest("planner", TrustTier::Standard, vec![]), None);
        let child = kernel.admit(
            &manifest("worker", TrustTier::Privileged, vec![]),
            Some(&parent),
        );
        assert_eq!(child.principal.trust, TrustTier::Standard);
        assert_eq!(child.principal.parent, Some(HarnessId::new("planner")));
    }

    #[tokio::test]
    async fn guarded_write_outside_the_grant_is_refused_and_audited() {
        let (kernel, audit) = kernel_with(root());
        let adm = kernel.admit(&manifest("worker", TrustTier::Standard, vec![scratch_rw("worker")]), None);
        let svc = kernel.attach(&adm, services("worker"));
        svc.begin_activation("act-1", 4096);

        svc.storage_set("scratch/ok", "v").await.unwrap();
        let err = svc.storage_set("secrets/key", "v").await.unwrap_err();
        assert!(err.to_string().contains("denied"), "{err}");

        // The value was never written...
        assert_eq!(svc.unguarded().storage.get("secrets/key").await.unwrap(), None);
        // ...and both the allow and the deny are on the record.
        let events = audit.events().await;
        assert_eq!(events.len(), 2);
        assert!(events[0].decision.is_allow());
        assert_eq!(audit.refusals().await.len(), 1);
        assert_eq!(events[1].activation, "act-1");
        assert_eq!(events[1].resource, "mem:secrets/key");
    }

    #[tokio::test]
    async fn trust_tier_gates_the_shutdown_syscall() {
        let (kernel, _) = kernel_with(root());
        let worker = kernel.admit(&manifest("worker", TrustTier::Sandboxed, vec![]), None);
        let svc = kernel.attach(&worker, services("worker"));
        assert!(svc.broadcast_shutdown().await.is_err());

        let planner_requested = vec![Rule::allow(
            "p-control",
            SubjectMatch { agent: Pattern::parse("planner"), min_trust: Some(TrustTier::Privileged), ..Default::default() },
            &[Action::Control],
            ResourcePattern::Swarm,
        )];
        let planner = kernel.admit(&manifest("planner", TrustTier::Privileged, planner_requested), None);
        let psvc = kernel.attach(&planner, services("planner"));
        // Permitted by policy; the empty bus makes the effect itself a no-op.
        assert!(psvc.check(Action::Control, &Resource::Swarm).await.is_ok());
    }

    #[tokio::test]
    async fn a_dropped_trust_floor_cannot_be_used_at_call_time() {
        // Mirrors policy::tests::attenuation_cannot_drop_a_parents_trust_floor,
        // but exercised end to end through admit -> attach -> check, to prove
        // the dropped rule is not just missing from `Admission::dropped` but
        // genuinely unusable.
        let (kernel, _) = kernel_with(root());
        let requested = vec![Rule::allow(
            "c-control",
            SubjectMatch::agent("worker"), // no min_trust: cannot cover root's floor
            &[Action::Control],
            ResourcePattern::Swarm,
        )];
        let adm = kernel.admit(&manifest("worker", TrustTier::Privileged, requested), None);
        assert!(adm.grants.rules.is_empty());
        let svc = kernel.attach(&adm, services("worker"));
        assert!(svc.check(Action::Control, &Resource::Swarm).await.is_err());
    }

    #[tokio::test]
    async fn revocation_makes_live_handles_stale() {
        let (kernel, audit) = kernel_with(root());
        let adm = kernel.admit(&manifest("worker", TrustTier::Standard, vec![scratch_rw("worker")]), None);
        let svc = kernel.attach(&adm, services("worker"));
        svc.begin_activation("act-1", 4096);

        assert!(svc.storage_set("scratch/a", "1").await.is_ok());
        kernel.revoke_all();
        let err = svc.storage_set("scratch/b", "2").await.unwrap_err();
        assert!(err.to_string().contains("StaleEpoch"), "{err}");

        let last = audit.events().await.pop().unwrap();
        assert!(matches!(
            last.decision,
            Decision::Deny { reason: DenyReason::StaleEpoch { held: 0, current: 1 }, .. }
        ));
    }

    #[tokio::test]
    async fn escalation_is_refused_but_named() {
        let escalate_prod = |subject: SubjectMatch| Rule {
            effect: Effect::Escalate,
            ..Rule::allow("ask-first", subject, &[Action::Write], ResourcePattern::Memory(Pattern::parse("prod/*")))
        };
        let (kernel, _) = kernel_with(GrantSet::new(vec![escalate_prod(SubjectMatch::default())]));
        let adm = kernel.admit(
            &manifest("worker", TrustTier::Standard, vec![escalate_prod(SubjectMatch::agent("worker"))]),
            None,
        );
        let svc = kernel.attach(&adm, services("worker"));

        let refusal = svc
            .check(Action::Write, &Resource::Memory { key: "prod/config".into() })
            .await
            .unwrap_err();
        assert!(matches!(
            refusal,
            Refusal::Escalated { to: Approver::Operator, ref rule, .. } if rule.0 == "ask-first"
        ));
    }

    #[tokio::test]
    async fn key_listing_hides_unreadable_keys() {
        let (kernel, _) = kernel_with(root());
        let adm = kernel.admit(&manifest("worker", TrustTier::Standard, vec![scratch_rw("worker")]), None);
        let svc = kernel.attach(&adm, services("worker"));

        // Seed through the raw handle: two readable, one not.
        let raw = svc.unguarded();
        raw.storage.set("scratch/a", "1").await.unwrap();
        raw.storage.set("scratch/b", "2").await.unwrap();
        raw.storage.set("secrets/c", "3").await.unwrap();

        let mut visible = svc.storage_keys("scratch/").await.unwrap();
        visible.sort();
        assert_eq!(visible, vec!["scratch/a".to_string(), "scratch/b".to_string()]);
        assert!(svc.storage_keys("").await.is_err(), "listing everything is not granted");
    }
}
