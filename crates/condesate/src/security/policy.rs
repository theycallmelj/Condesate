//! The permission boundary: capabilities, rules, and decisions.
//!
//! This is the swarm's equivalent of an OS access-control layer. Every crossing
//! of the syscall surface ([`crate::swarm::service::ServiceHandle`]) becomes an
//! [`AccessRequest`] — *subject, action, resource, context* — that a
//! [`PolicyEngine`] turns into a [`Decision`], evaluated against one
//! principal's own [`GrantSet`] (never a global set matched only by pattern).
//!
//! ## Three properties the model is built around
//!
//! 1. **Default deny.** A request that matches no rule is denied
//!    ([`DenyReason::NoMatchingRule`]). Adding a tool never silently widens
//!    reach; someone has to grant it.
//! 2. **Deny wins.** Within a [`GrantSet`], an explicit `Deny` beats an
//!    `Escalate` which beats an `Allow`. Order of insertion is irrelevant, so
//!    grants compose without ordering bugs.
//! 3. **Attenuation only.** A principal that spawns or delegates can hand over
//!    a *subset* of its own authority and never more — see
//!    [`GrantSet::attenuate`]. This is what stops a planner from bootstrapping
//!    a worker with powers the planner does not itself hold. The check is not
//!    just "does the resource fit inside the parent's" — a requested rule must
//!    also be at least as narrow *in who it matches* ([`SubjectMatch::contains`]).
//!    Without that half, a child could drop a parent's `min_trust: Privileged`
//!    restriction from its own copy of a rule and keep the rule anyway.
//!
//! Every admission — including a top-level one with no parent — attenuates
//! against *some* authority (a parent's grants, or the kernel's root). There is
//! no special case that hands a top-level agent the whole root unfiltered: a
//! manifest declares copies of the specific rules it wants, shaped to match
//! what the authority above it already allows. "Declare what you need" is
//! enforced uniformly, not just for spawned children.
//!
//! ## Two enforcement points, on purpose
//!
//! * **Advertise time** — [`ToolBroker`] (or [`super::kernel::GuardedServices::may_advertise`])
//!   filters the toolbelt *before* the model is told what exists. An agent
//!   cannot be talked into calling a tool it was never shown, which shrinks the
//!   blast radius of prompt injection.
//! * **Call time** — the guarded service handle re-checks. This one is
//!   authoritative; advertise-time filtering is only a reduction in temptation.
//!
//! Everything here is data plus one pure evaluator. Wiring it into the running
//! harness is [`super::kernel`]'s job.

use super::identity::{Compatibility, GovernanceLabel, ModelClass, Principal, TenantId, TrustTier};
use crate::types::HarnessId;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Verbs and objects
// ---------------------------------------------------------------------------

/// What a principal is trying to do. Deliberately small — a dozen verbs is
/// enough to cover the syscall surface, and a small set keeps rules readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// Read a memory key, or read a cache value.
    Read,
    /// Write a memory key, or admit a value into a cache pool.
    Write,
    /// Call a tool, or call a model.
    Invoke,
    /// Send a message to a peer.
    Send,
    /// Advertise local cache ownership to peers (metadata, not values).
    Publish,
    /// Pull values from a peer's cache pool.
    Pull,
    /// Create a new harness.
    Spawn,
    /// Lifecycle control: shutdown, revoke, kill.
    Control,
}

/// The concrete thing being acted on, as named at the call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    /// A tool, by advertised name.
    Tool { name: String },
    /// A shared-storage key. Rules usually match a prefix.
    Memory { key: String },
    /// A cache pool, optionally narrowed to one value class, plus the label of
    /// the data in question when it is already known.
    Cache {
        class: ModelClass,
        value_class: Option<crate::cache::ValueClass>,
        label: Option<GovernanceLabel>,
    },
    /// Another harness, as a message target.
    Peer { id: HarnessId },
    /// A model provider, as an inference target.
    Model { class: ModelClass },
    /// The right to create a harness running a named agent.
    Spawn { agent: String },
    /// Swarm-wide lifecycle.
    Swarm,
    /// A live agent's registry record, named by its role — see
    /// `super::kernel::AgentInfo`. Never named by uid: a rule is authored
    /// before the instance it might match exists, so only the stable role
    /// name can appear in a pattern.
    Agent { agent: String },
}

impl Resource {
    /// Short stable string for audit records.
    pub fn describe(&self) -> String {
        match self {
            Resource::Tool { name } => format!("tool:{name}"),
            Resource::Memory { key } => format!("mem:{key}"),
            Resource::Cache { class, value_class, .. } => match value_class {
                Some(v) => format!("cache:{class}#{v:?}"),
                None => format!("cache:{class}"),
            },
            Resource::Peer { id } => format!("peer:{id}"),
            Resource::Model { class } => format!("model:{class}"),
            Resource::Spawn { agent } => format!("spawn:{agent}"),
            Resource::Swarm => "swarm".to_string(),
            Resource::Agent { agent } => format!("agent:{agent}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

/// String matcher used by rules. `"fs_*"` parses to a prefix, anything else to
/// an exact match, `"*"` to [`Pattern::Any`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Any,
    Exact(String),
    Prefix(String),
}

impl Pattern {
    pub fn parse(s: &str) -> Self {
        match s.strip_suffix('*') {
            Some("") => Pattern::Any,
            Some(prefix) => Pattern::Prefix(prefix.to_string()),
            None => Pattern::Exact(s.to_string()),
        }
    }

    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Exact(s) => s == candidate,
            Pattern::Prefix(p) => candidate.starts_with(p.as_str()),
        }
    }

    /// True when everything `other` matches, `self` also matches. The
    /// containment test behind attenuation.
    pub fn contains(&self, other: &Pattern) -> bool {
        match (self, other) {
            (Pattern::Any, _) => true,
            (_, Pattern::Any) => false,
            (Pattern::Prefix(p), Pattern::Prefix(q)) => q.starts_with(p.as_str()),
            (Pattern::Prefix(p), Pattern::Exact(e)) => e.starts_with(p.as_str()),
            (Pattern::Exact(a), Pattern::Exact(b)) => a == b,
            (Pattern::Exact(_), Pattern::Prefix(_)) => false,
        }
    }
}

/// Matcher over [`ModelClass`]. `None` fields match anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelClassPattern {
    pub provider: Option<String>,
    pub family: Option<String>,
    pub revision: Option<String>,
    pub embedding_space: Option<String>,
}

impl ModelClassPattern {
    pub fn matches(&self, class: &ModelClass) -> bool {
        let eq = |want: &Option<String>, have: &str| want.as_deref().is_none_or(|w| w == have);
        eq(&self.provider, &class.provider)
            && eq(&self.family, &class.family)
            && eq(&self.revision, &class.revision)
            && self
                .embedding_space
                .as_deref()
                .is_none_or(|w| class.embedding_space.as_deref() == Some(w))
    }

    fn contains(&self, other: &ModelClassPattern) -> bool {
        let ok = |mine: &Option<String>, theirs: &Option<String>| match mine {
            None => true,
            Some(m) => theirs.as_deref() == Some(m.as_str()),
        };
        ok(&self.provider, &other.provider)
            && ok(&self.family, &other.family)
            && ok(&self.revision, &other.revision)
            && ok(&self.embedding_space, &other.embedding_space)
    }
}

/// What kind of resource a rule covers, and which instances of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourcePattern {
    /// Matches every resource. Use sparingly; mostly for kernel grants.
    AnyResource,
    Tool(Pattern),
    /// Matches memory keys by prefix — `Memory(Pattern::Prefix("proj/"))` is
    /// the swarm's answer to a per-process address space.
    Memory(Pattern),
    Cache {
        class: ModelClassPattern,
        value_class: Option<crate::cache::ValueClass>,
    },
    Peer(Pattern),
    Model(ModelClassPattern),
    Spawn(Pattern),
    Swarm,
    Agent(Pattern),
}

impl ResourcePattern {
    pub fn matches(&self, resource: &Resource) -> bool {
        match (self, resource) {
            (ResourcePattern::AnyResource, _) => true,
            (ResourcePattern::Tool(p), Resource::Tool { name }) => p.matches(name),
            (ResourcePattern::Memory(p), Resource::Memory { key }) => p.matches(key),
            (
                ResourcePattern::Cache { class: cp, value_class: vp },
                Resource::Cache { class, value_class, .. },
            ) => cp.matches(class) && (vp.is_none() || vp == value_class),
            (ResourcePattern::Peer(p), Resource::Peer { id }) => p.matches(&id.0),
            (ResourcePattern::Model(cp), Resource::Model { class }) => cp.matches(class),
            (ResourcePattern::Spawn(p), Resource::Spawn { agent }) => p.matches(agent),
            (ResourcePattern::Swarm, Resource::Swarm) => true,
            (ResourcePattern::Agent(p), Resource::Agent { agent }) => p.matches(agent),
            _ => false,
        }
    }

    /// Containment for attenuation: does `self` cover everything `other` does?
    pub fn contains(&self, other: &ResourcePattern) -> bool {
        match (self, other) {
            (ResourcePattern::AnyResource, _) => true,
            (_, ResourcePattern::AnyResource) => false,
            (ResourcePattern::Tool(a), ResourcePattern::Tool(b)) => a.contains(b),
            (ResourcePattern::Memory(a), ResourcePattern::Memory(b)) => a.contains(b),
            (
                ResourcePattern::Cache { class: a, value_class: av },
                ResourcePattern::Cache { class: b, value_class: bv },
            ) => a.contains(b) && (av.is_none() || av == bv),
            (ResourcePattern::Peer(a), ResourcePattern::Peer(b)) => a.contains(b),
            (ResourcePattern::Model(a), ResourcePattern::Model(b)) => a.contains(b),
            (ResourcePattern::Spawn(a), ResourcePattern::Spawn(b)) => a.contains(b),
            (ResourcePattern::Swarm, ResourcePattern::Swarm) => true,
            (ResourcePattern::Agent(a), ResourcePattern::Agent(b)) => a.contains(b),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Subjects
// ---------------------------------------------------------------------------

/// Which principals a rule applies to. Every `None`/`Any` field widens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectMatch {
    pub agent: Pattern,
    pub tenant: Option<TenantId>,
    pub min_trust: Option<TrustTier>,
    pub model_class: Option<ModelClassPattern>,
}

impl Default for SubjectMatch {
    fn default() -> Self {
        Self { agent: Pattern::Any, tenant: None, min_trust: None, model_class: None }
    }
}

impl SubjectMatch {
    pub fn agent(pat: &str) -> Self {
        Self { agent: Pattern::parse(pat), ..Default::default() }
    }

    pub fn matches(&self, p: &Principal) -> bool {
        self.agent.matches(&p.agent)
            && self.tenant.as_ref().is_none_or(|t| *t == p.tenant)
            && self.min_trust.is_none_or(|t| p.trust >= t)
            && self.model_class.as_ref().is_none_or(|m| m.matches(&p.model_class))
    }

    /// Does `self` (a would-be parent rule's subject) match at least every
    /// principal `other` (a requested rule's subject) would? The subject half
    /// of attenuation containment — see the module-level note on why this
    /// matters as much as [`ResourcePattern::contains`].
    fn contains(&self, other: &SubjectMatch) -> bool {
        let trust_ok = match self.min_trust {
            None => true,
            // A narrower-or-equal floor on the child's side is fine; a
            // missing or lower floor would let the child's copy match
            // principals the parent's rule never covered.
            Some(parent_floor) => other.min_trust.is_some_and(|c| c >= parent_floor),
        };
        let tenant_ok = match &self.tenant {
            None => true,
            Some(t) => other.tenant.as_ref() == Some(t),
        };
        let class_ok = match &self.model_class {
            None => true,
            Some(p) => other.model_class.as_ref().is_some_and(|c| p.contains(c)),
        };
        self.agent.contains(&other.agent) && tenant_ok && trust_ok && class_ok
    }
}

// ---------------------------------------------------------------------------
// Conditions and obligations
// ---------------------------------------------------------------------------

/// Extra predicates evaluated against the request context. These are the hooks
/// where budget, freshness, and data-label checks attach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// Reject once this activation has spent more than N bytes of value pulls.
    ByteBudgetRemaining { min_bytes: u64 },
    /// Reject after N tool calls in one activation. Runaway-loop backstop.
    StepsUnder { max_steps: u32 },
    /// Only permit when the data label carries none of these scopes.
    LabelExcludesScopes(Vec<String>),
    /// Only permit when the data label carries all of these scopes.
    LabelIncludesScopes(Vec<String>),
    /// Only permit reuse at or above this compatibility level. The rule that
    /// keeps raw KV blocks from crossing between model revisions.
    MinCompatibility(Compatibility),
    /// Only permit when the value is younger than this.
    FreshnessUnder { max_age_ms: u64 },
}

/// A condition attached to an *allow*: permitted, but only if the caller
/// honours this. The kernel applies obligations; tools never see them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Obligation {
    /// Strip these scopes' content before handing the value to the model.
    Redact(Vec<String>),
    /// Cap what a single call may return.
    LimitBytes(u64),
    /// Force a shorter TTL on anything admitted through this path.
    ExpireAfter { ms: u64 },
    /// Record at elevated detail (full args and result, not just a summary).
    AuditVerbose,
    /// Notify this principal out of band after the fact.
    NotifyOnUse(HarnessId),
}

// ---------------------------------------------------------------------------
// Rules and grants
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleId(pub String);

impl RuleId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for RuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Allow,
    /// Refuse outright. Beats everything else.
    Deny,
    /// Refuse *for now* and hand the request to an approver — a supervising
    /// harness or a human. The swarm's version of a privileged prompt.
    Escalate,
}

/// One entry in a grant set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub id: RuleId,
    pub effect: Effect,
    pub subject: SubjectMatch,
    pub actions: Vec<Action>,
    pub resource: ResourcePattern,
    pub conditions: Vec<Condition>,
    pub obligations: Vec<Obligation>,
}

impl Rule {
    /// Terse constructor for the common case: an allow with no conditions.
    pub fn allow(
        id: &str,
        subject: SubjectMatch,
        actions: &[Action],
        resource: ResourcePattern,
    ) -> Self {
        Self {
            id: RuleId::new(id),
            effect: Effect::Allow,
            subject,
            actions: actions.to_vec(),
            resource,
            conditions: Vec::new(),
            obligations: Vec::new(),
        }
    }

    /// Terse constructor for a hard denial.
    pub fn deny(
        id: &str,
        subject: SubjectMatch,
        actions: &[Action],
        resource: ResourcePattern,
    ) -> Self {
        Self { effect: Effect::Deny, ..Self::allow(id, subject, actions, resource) }
    }

    pub fn with_conditions(mut self, c: Vec<Condition>) -> Self {
        self.conditions = c;
        self
    }

    pub fn with_obligations(mut self, o: Vec<Obligation>) -> Self {
        self.obligations = o;
        self
    }

    /// Does this rule speak to the request at all? Conditions are evaluated
    /// separately by the engine, which owns the runtime context.
    pub fn applies(&self, req: &AccessRequest<'_>) -> bool {
        self.subject.matches(req.principal)
            && self.actions.contains(&req.action)
            && self.resource.matches(req.resource)
    }

    /// Would `self` permit at least everything `other` permits? Requires
    /// containment on *both* halves of the rule — the resource it reaches and
    /// the subject it matches — plus every action `other` claims. Checking
    /// resource alone is not enough: a parent rule scoped to
    /// `min_trust: Privileged` does not cover a child's copy of the same rule
    /// with the trust floor dropped, even though the resource and actions
    /// match exactly.
    fn covers(&self, other: &Rule) -> bool {
        self.effect == Effect::Allow
            && self.subject.contains(&other.subject)
            && other.actions.iter().all(|a| self.actions.contains(a))
            && self.resource.contains(&other.resource)
    }
}

/// The set of rules in force for one principal, plus the epoch that makes
/// revocation possible.
///
/// Held by the kernel, never by the agent. Bumping the kernel's epoch via
/// [`super::kernel::Kernel::revoke_all`] invalidates every handle issued under
/// the old epoch, which is how a misbehaving agent is defanged without killing
/// it mid-write.
#[derive(Clone, Debug, Default)]
pub struct GrantSet {
    pub rules: Vec<Rule>,
    pub epoch: u64,
}

impl GrantSet {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules, epoch: 0 }
    }

    /// Union of two grant sets. Denials from either side survive, which is why
    /// union is still safe: `Deny` outranks `Allow` at evaluation time.
    pub fn merge(mut self, other: GrantSet) -> Self {
        self.rules.extend(other.rules);
        self.epoch = self.epoch.max(other.epoch);
        self
    }

    /// Delegate a subset of this authority to a child.
    ///
    /// A requested allow survives only if some allow already in this set
    /// covers it *including* its subject narrowing (see [`Rule::covers`]).
    /// Every denial in this set is inherited unconditionally — a parent cannot
    /// delegate away a restriction it is itself under. This is the
    /// no-escalation-by-spawning rule, and it applies uniformly: a top-level
    /// admission attenuates against the kernel's root the same way a spawned
    /// child attenuates against its parent. There is no unfiltered path.
    ///
    /// `Escalate` and `Deny` requests are kept as asked, without a coverage
    /// check — both only ever narrow what a principal may do (an escalation
    /// still requires approval; a denial only restricts), so free-adding them
    /// cannot itself grant reach the authority above did not already gate in
    /// some way. This is a deliberate scope boundary, not an oversight: it
    /// does mean a child can mint an `Escalate` rule for a resource its parent
    /// never mentioned at all, which is a softer guarantee than the `Allow`
    /// path gets. Tightening that is tracked as a follow-up, not done here.
    pub fn attenuate(&self, requested: &[Rule]) -> GrantSet {
        let mut out: Vec<Rule> = self
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Deny)
            .cloned()
            .collect();

        for want in requested {
            match want.effect {
                Effect::Deny | Effect::Escalate => out.push(want.clone()),
                Effect::Allow => {
                    if self.rules.iter().any(|mine| mine.covers(want)) {
                        out.push(want.clone());
                    }
                    // Otherwise silently dropped: the child simply never gets
                    // the authority, and the first attempt to use it audits as
                    // NoMatchingRule.
                }
            }
        }
        GrantSet { rules: out, epoch: self.epoch }
    }
}

// ---------------------------------------------------------------------------
// Requests and decisions
// ---------------------------------------------------------------------------

/// Runtime facts a [`Condition`] may need. Supplied by the kernel, not the
/// caller, so an agent cannot fake its own budget.
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    /// Ties every check back to the inbound message that triggered it.
    pub activation: String,
    /// Tool calls taken so far in this activation.
    pub steps: u32,
    /// Bytes of value movement still affordable this round.
    pub byte_budget_remaining: u64,
    /// Size of the value in play, when known ahead of the call.
    pub value_bytes: Option<u64>,
    /// Age of the candidate value, when the request is a cache read.
    pub value_age_ms: Option<u64>,
    /// Reuse ceiling between requester and value owner, for cache requests.
    pub compatibility: Option<Compatibility>,
}

/// One access check.
pub struct AccessRequest<'a> {
    pub principal: &'a Principal,
    pub action: Action,
    pub resource: &'a Resource,
    pub context: &'a RequestContext,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// Default deny — nothing granted this.
    NoMatchingRule,
    /// An explicit deny rule fired.
    ExplicitDeny,
    /// A rule matched but its condition failed.
    ConditionFailed(String),
    /// The handle was issued under a revoked epoch.
    StaleEpoch { held: u64, current: u64 },
    /// Structural impossibility (cross-tenant, incompatible model class).
    Isolation(String),
}

/// Who an escalation goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approver {
    /// Another harness in the swarm, typically a supervisor.
    Harness(HarnessId),
    /// Out to a human operator.
    Operator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow { rule: RuleId, obligations: Vec<Obligation> },
    Deny { rule: Option<RuleId>, reason: DenyReason },
    Escalate { rule: RuleId, to: Approver },
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }

    /// Default-deny constructor, used whenever nothing matched.
    pub fn unmatched() -> Self {
        Decision::Deny { rule: None, reason: DenyReason::NoMatchingRule }
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Turns requests into decisions against a specific principal's [`GrantSet`].
///
/// The grant set is passed in per call rather than fixed at construction: one
/// engine instance serves every principal in the swarm, each evaluated against
/// its own attenuated authority, never a shared global set matched only by
/// [`SubjectMatch`] pattern.
///
/// Async because a real deployment may consult a remote decision point or a
/// human approver. [`RuleSetPolicy`] is the local, pure implementation.
#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn evaluate(&self, grants: &GrantSet, req: &AccessRequest<'_>) -> Decision;

    /// Cheap synchronous pre-filter for advertise-time tool listing. Defaults
    /// to permissive so the authoritative async check stays the only gate that
    /// matters; override it to shrink what the model is shown.
    fn may_advertise(&self, _grants: &GrantSet, _principal: &Principal, _tool: &str) -> bool {
        true
    }
}

/// The reference engine: deny-wins evaluation of whatever [`GrantSet`] it is
/// handed.
pub struct RuleSetPolicy {
    /// Where escalations are routed when a rule says `Effect::Escalate`.
    pub approver: Approver,
}

impl Default for RuleSetPolicy {
    fn default() -> Self {
        Self { approver: Approver::Operator }
    }
}

impl RuleSetPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_approver(mut self, approver: Approver) -> Self {
        self.approver = approver;
        self
    }

    /// Evaluate a condition against the request. Unknown/absent context fails
    /// closed: if the kernel could not supply the fact, the rule does not fire.
    fn condition_holds(c: &Condition, req: &AccessRequest<'_>) -> bool {
        let ctx = req.context;
        match c {
            Condition::ByteBudgetRemaining { min_bytes } => ctx.byte_budget_remaining >= *min_bytes,
            Condition::StepsUnder { max_steps } => ctx.steps < *max_steps,
            Condition::MinCompatibility(min) => ctx.compatibility.is_some_and(|c| c >= *min),
            Condition::FreshnessUnder { max_age_ms } => {
                ctx.value_age_ms.is_some_and(|a| a <= *max_age_ms)
            }
            Condition::LabelExcludesScopes(scopes) => match label_of(req.resource) {
                Some(l) => !scopes.iter().any(|s| l.scopes.contains(s)),
                None => false,
            },
            Condition::LabelIncludesScopes(scopes) => match label_of(req.resource) {
                Some(l) => scopes.iter().all(|s| l.scopes.contains(s)),
                None => false,
            },
        }
    }
}

fn label_of(resource: &Resource) -> Option<&GovernanceLabel> {
    match resource {
        Resource::Cache { label, .. } => label.as_ref(),
        _ => None,
    }
}

#[async_trait]
impl PolicyEngine for RuleSetPolicy {
    async fn evaluate(&self, grants: &GrantSet, req: &AccessRequest<'_>) -> Decision {
        // Structural isolation first — no rule can grant across tenants.
        if let Resource::Cache { label: Some(l), .. } = req.resource {
            if !l.could_flow_to(req.principal) {
                return Decision::Deny {
                    rule: None,
                    reason: DenyReason::Isolation(format!(
                        "value labelled tenant '{}' cannot flow to '{}'",
                        l.tenant, req.principal.tenant
                    )),
                };
            }
        }

        let mut allow: Option<&Rule> = None;
        let mut escalate: Option<&Rule> = None;
        let mut obligations: Vec<Obligation> = Vec::new();
        let mut failed_condition: Option<(&Rule, &Condition)> = None;

        for rule in &grants.rules {
            if !rule.applies(req) {
                continue;
            }
            if let Some(c) = rule
                .conditions
                .iter()
                .find(|c| !Self::condition_holds(c, req))
            {
                // Record why a would-be match did not fire, for the audit trail.
                if rule.effect == Effect::Allow && failed_condition.is_none() {
                    failed_condition = Some((rule, c));
                }
                continue;
            }
            match rule.effect {
                // Deny short-circuits: nothing later can rescue the request.
                Effect::Deny => {
                    return Decision::Deny {
                        rule: Some(rule.id.clone()),
                        reason: DenyReason::ExplicitDeny,
                    }
                }
                Effect::Escalate => escalate.get_or_insert(rule),
                Effect::Allow => {
                    obligations.extend(rule.obligations.iter().cloned());
                    allow.get_or_insert(rule)
                }
            };
        }

        if let Some(rule) = escalate {
            return Decision::Escalate { rule: rule.id.clone(), to: self.approver.clone() };
        }
        if let Some(rule) = allow {
            return Decision::Allow { rule: rule.id.clone(), obligations };
        }
        if let Some((rule, cond)) = failed_condition {
            return Decision::Deny {
                rule: Some(rule.id.clone()),
                reason: DenyReason::ConditionFailed(format!("{cond:?}")),
            };
        }
        Decision::unmatched()
    }

    fn may_advertise(&self, grants: &GrantSet, principal: &Principal, tool: &str) -> bool {
        let resource = Resource::Tool { name: tool.to_string() };
        let mut seen_allow = false;
        for rule in &grants.rules {
            if !rule.subject.matches(principal)
                || !rule.actions.contains(&Action::Invoke)
                || !rule.resource.matches(&resource)
            {
                continue;
            }
            match rule.effect {
                Effect::Deny => return false,
                Effect::Allow | Effect::Escalate => seen_allow = true,
            }
        }
        seen_allow
    }
}

// ---------------------------------------------------------------------------
// Advertise-time filtering
// ---------------------------------------------------------------------------

/// Narrows a toolbelt to what a principal may actually invoke, before the model
/// is told the tools exist.
///
/// [`super::kernel::GuardedServices::may_advertise`] is the equivalent
/// convenience for code that already holds a live guarded handle; this type is
/// for building a filtered view without one (e.g. listing a principal's
/// available tools for a UI).
pub struct ToolBroker {
    pub principal: Principal,
    pub grants: GrantSet,
    pub policy: std::sync::Arc<dyn PolicyEngine>,
}

impl ToolBroker {
    pub fn new(principal: Principal, grants: GrantSet, policy: std::sync::Arc<dyn PolicyEngine>) -> Self {
        Self { principal, grants, policy }
    }

    /// The subset of `all` this principal may be shown.
    pub fn visible<'a>(
        &self,
        all: &'a [std::sync::Arc<dyn crate::agent::tool::Tool>],
    ) -> Vec<&'a std::sync::Arc<dyn crate::agent::tool::Tool>> {
        all.iter()
            .filter(|t| self.policy.may_advertise(&self.grants, &self.principal, &t.spec().name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::identity::TenantId;
    use super::*;

    fn class() -> ModelClass {
        ModelClass {
            provider: "anthropic".into(),
            family: "claude-opus".into(),
            revision: "claude-opus-5".into(),
            embedding_space: Some("emb-v1".into()),
            quantization: None,
        }
    }

    fn principal(agent: &str, trust: TrustTier) -> Principal {
        Principal {
            uid: super::super::identity::AgentUid::new(),
            harness: HarnessId::new(agent),
            agent: agent.to_string(),
            model_class: class(),
            tenant: TenantId::new("acme"),
            trust,
            parent: None,
            parent_uid: None,
        }
    }

    async fn decide(
        policy: &RuleSetPolicy,
        grants: &GrantSet,
        p: &Principal,
        a: Action,
        r: &Resource,
    ) -> Decision {
        let ctx = RequestContext::default();
        policy
            .evaluate(grants, &AccessRequest { principal: p, action: a, resource: r, context: &ctx })
            .await
    }

    #[tokio::test]
    async fn unmatched_request_is_denied() {
        let policy = RuleSetPolicy::new();
        let grants = GrantSet::default();
        let p = principal("worker", TrustTier::Standard);
        let d = decide(&policy, &grants, &p, Action::Invoke, &Resource::Tool { name: "rm".into() })
            .await;
        assert_eq!(d, Decision::Deny { rule: None, reason: DenyReason::NoMatchingRule });
    }

    #[tokio::test]
    async fn deny_beats_allow_regardless_of_order() {
        let rules = vec![
            Rule::allow(
                "allow-all-tools",
                SubjectMatch::default(),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Any),
            ),
            Rule::deny(
                "no-shutdown",
                SubjectMatch::default(),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact("shutdown_swarm".into())),
            ),
        ];
        let p = principal("worker", TrustTier::Standard);
        let policy = RuleSetPolicy::new();

        for order in [rules.clone(), rules.into_iter().rev().collect()] {
            let grants = GrantSet::new(order);
            let d = decide(
                &policy,
                &grants,
                &p,
                Action::Invoke,
                &Resource::Tool { name: "shutdown_swarm".into() },
            )
            .await;
            assert!(matches!(
                d,
                Decision::Deny { reason: DenyReason::ExplicitDeny, .. }
            ));
            // ...while an unrelated tool still passes.
            let ok = decide(
                &policy,
                &grants,
                &p,
                Action::Invoke,
                &Resource::Tool { name: "word_count".into() },
            )
            .await;
            assert!(ok.is_allow());
        }
    }

    #[tokio::test]
    async fn memory_prefix_scopes_the_address_space() {
        let policy = RuleSetPolicy::new();
        let grants = GrantSet::new(vec![Rule::allow(
            "own-scratch",
            SubjectMatch::agent("worker"),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("scratch/worker/*")),
        )]);
        let p = principal("worker", TrustTier::Standard);

        let mine = Resource::Memory { key: "scratch/worker/notes".into() };
        assert!(decide(&policy, &grants, &p, Action::Write, &mine).await.is_allow());

        let theirs = Resource::Memory { key: "scratch/planner/notes".into() };
        assert!(!decide(&policy, &grants, &p, Action::Write, &theirs).await.is_allow());
    }

    #[tokio::test]
    async fn min_trust_gates_swarm_control() {
        let policy = RuleSetPolicy::new();
        let grants = GrantSet::new(vec![Rule::allow(
            "privileged-shutdown",
            SubjectMatch { min_trust: Some(TrustTier::Privileged), ..Default::default() },
            &[Action::Control],
            ResourcePattern::Swarm,
        )]);

        let planner = principal("planner", TrustTier::Privileged);
        let worker = principal("worker", TrustTier::Sandboxed);
        assert!(decide(&policy, &grants, &planner, Action::Control, &Resource::Swarm).await.is_allow());
        assert!(!decide(&policy, &grants, &worker, Action::Control, &Resource::Swarm).await.is_allow());
    }

    #[tokio::test]
    async fn cross_tenant_cache_read_is_structurally_denied() {
        // Even a wide-open allow cannot move a value across tenants.
        let policy = RuleSetPolicy::new();
        let grants = GrantSet::new(vec![Rule::allow(
            "everything",
            SubjectMatch::default(),
            &[Action::Read],
            ResourcePattern::AnyResource,
        )]);
        let p = principal("worker", TrustTier::Privileged);
        let foreign = Resource::Cache {
            class: class(),
            value_class: None,
            label: Some(GovernanceLabel::plain(TenantId::new("other-corp"))),
        };
        assert!(matches!(
            decide(&policy, &grants, &p, Action::Read, &foreign).await,
            Decision::Deny { reason: DenyReason::Isolation(_), .. }
        ));
    }

    #[tokio::test]
    async fn condition_failure_reports_the_rule_that_nearly_matched() {
        let policy = RuleSetPolicy::new();
        let grants = GrantSet::new(vec![Rule::allow(
            "kv-blocks-need-identical-model",
            SubjectMatch::default(),
            &[Action::Pull],
            ResourcePattern::Cache {
                class: ModelClassPattern::default(),
                value_class: Some(crate::cache::ValueClass::KvBlock),
            },
        )
        .with_conditions(vec![Condition::MinCompatibility(Compatibility::Identical)])]);

        let p = principal("worker", TrustTier::Standard);
        let resource = Resource::Cache {
            class: class(),
            value_class: Some(crate::cache::ValueClass::KvBlock),
            label: None,
        };
        let ctx = RequestContext {
            compatibility: Some(Compatibility::SameFamily),
            ..Default::default()
        };
        let d = policy
            .evaluate(&grants, &AccessRequest {
                principal: &p,
                action: Action::Pull,
                resource: &resource,
                context: &ctx,
            })
            .await;
        assert!(matches!(
            d,
            Decision::Deny {
                rule: Some(ref id),
                reason: DenyReason::ConditionFailed(_)
            } if id.0 == "kv-blocks-need-identical-model"
        ));
    }

    #[test]
    fn attenuation_cannot_widen_authority() {
        let parent = GrantSet::new(vec![
            Rule::allow(
                "p-mem",
                SubjectMatch::default(),
                &[Action::Read, Action::Write],
                ResourcePattern::Memory(Pattern::parse("proj/*")),
            ),
            Rule::deny(
                "p-no-secrets",
                SubjectMatch::default(),
                &[Action::Read],
                ResourcePattern::Memory(Pattern::parse("proj/secrets/*")),
            ),
        ]);

        let child = parent.attenuate(&[
            // narrower than the parent's grant -> kept
            Rule::allow(
                "c-mem",
                SubjectMatch::agent("child"),
                &[Action::Read],
                ResourcePattern::Memory(Pattern::parse("proj/notes/*")),
            ),
            // wider than the parent's grant -> dropped
            Rule::allow(
                "c-all-mem",
                SubjectMatch::agent("child"),
                &[Action::Read],
                ResourcePattern::Memory(Pattern::Any),
            ),
            // an action the parent never held -> dropped
            Rule::allow(
                "c-spawn",
                SubjectMatch::agent("child"),
                &[Action::Spawn],
                ResourcePattern::Spawn(Pattern::Any),
            ),
        ]);

        let ids: Vec<&str> = child.rules.iter().map(|r| r.id.0.as_str()).collect();
        assert_eq!(ids, vec!["p-no-secrets", "c-mem"]);
    }

    #[test]
    fn attenuation_cannot_drop_a_parents_trust_floor() {
        // The parent only grants Control to Privileged principals. A child
        // requesting the same action+resource but omitting that floor must
        // not receive the rule just because the resource matched.
        let parent = GrantSet::new(vec![Rule::allow(
            "p-control",
            SubjectMatch { min_trust: Some(TrustTier::Privileged), ..Default::default() },
            &[Action::Control],
            ResourcePattern::Swarm,
        )]);

        let child = parent.attenuate(&[Rule::allow(
            "c-control",
            SubjectMatch::agent("worker"),
            &[Action::Control],
            ResourcePattern::Swarm,
        )]);

        assert!(child.rules.is_empty(), "dropping the parent's trust floor must not be grantable");
    }

    #[test]
    fn pattern_containment() {
        let any = Pattern::Any;
        let pre = Pattern::parse("proj/*");
        let exact = Pattern::parse("proj/a");
        assert!(any.contains(&pre) && any.contains(&exact));
        assert!(pre.contains(&exact));
        assert!(!exact.contains(&pre));
        assert!(!pre.contains(&any));
    }

    #[test]
    fn advertise_filter_hides_denied_tools() {
        let policy = RuleSetPolicy::new();
        let grants = GrantSet::new(vec![
            Rule::allow(
                "tools",
                SubjectMatch::default(),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Any),
            ),
            Rule::deny(
                "no-shutdown",
                SubjectMatch::default(),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact("shutdown_swarm".into())),
            ),
        ]);
        let p = principal("worker", TrustTier::Standard);
        assert!(policy.may_advertise(&grants, &p, "word_count"));
        assert!(!policy.may_advertise(&grants, &p, "shutdown_swarm"));
    }
}
