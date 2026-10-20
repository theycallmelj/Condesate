//! Who is acting, and what they are made of.
//!
//! In OS terms this is the *credential* layer: before anything can be permitted
//! or denied, the kernel needs a stable answer to "which subject is making this
//! call, under whose authority, running which model?".
//!
//! Three ideas live here and they are deliberately separate:
//!
//!   * [`Principal`] — the subject of every access check (a harness + the agent
//!     role it runs + the model behind it + its tenant + its trust tier).
//!   * [`ModelClass`] — *what kind of model* a principal is. This is the key
//!     that decides which agents may share a cache pool: models of the same
//!     type share, models of different types do not.
//!   * [`GovernanceLabel`] — the classification carried by data (tenant, branch,
//!     scope, policy version). Labels travel *with* cached values so a semantic
//!     hit can still be refused on authorization grounds.
//!
//! Nothing here performs a check. [`super::policy`] does that.

use std::fmt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------------

/// Coarse privilege band, ordered low → high. Analogous to CPU rings.
///
/// Rules can require a minimum tier (`min_trust`), which is how you express
/// "only privileged agents may broadcast a shutdown" without enumerating names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TrustTier {
    /// Content-derived or externally-influenced agents. Assume prompt injection.
    Untrusted,
    /// Normal worker agents. Read narrow, write narrow.
    Sandboxed,
    /// Trusted workers: broader memory, may message peers.
    Standard,
    /// Planners/supervisors: may spawn, may delegate, may revoke.
    Privileged,
    /// The swarm kernel itself. Never granted to a model-driven agent.
    Kernel,
}

// ---------------------------------------------------------------------------
// Tenancy and data classification
// ---------------------------------------------------------------------------

/// Isolation domain. Two principals in different tenants must not share cache
/// values, memory, or messages unless a rule says so explicitly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(pub String);

impl TenantId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Classification attached to a stored value or a message.
///
/// This is the field that keeps "semantically similar" from silently meaning
/// "authorized". A cache lookup may find a perfect geometric match and still be
/// refused because the label does not clear the reader's grants.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GovernanceLabel {
    pub tenant: TenantId,
    /// Repository branch / workspace the value was derived from, if any.
    pub branch: Option<String>,
    /// Free-form scope tags: `"secrets"`, `"customer-data"`, `"public-docs"`.
    pub scopes: Vec<String>,
    /// Version of the policy in force when the value was written. A reader
    /// under a newer policy version may be required to re-derive rather than
    /// reuse.
    pub policy_version: u32,
}

impl GovernanceLabel {
    /// The least sensitive label: one tenant, no branch, no scopes.
    pub fn plain(tenant: TenantId) -> Self {
        Self { tenant, branch: None, scopes: Vec::new(), policy_version: 0 }
    }

    /// Structural pre-filter only — cheap, conservative, and **not** a decision.
    ///
    /// Returns `false` for combinations no policy should ever allow (cross
    /// tenant). Returning `true` means "worth asking the policy engine", never
    /// "permitted".
    pub fn could_flow_to(&self, reader: &Principal) -> bool {
        self.tenant == reader.tenant
    }
}

// ---------------------------------------------------------------------------
// Model identity
// ---------------------------------------------------------------------------

/// The identity of a model, at the granularity that matters for cache sharing.
///
/// Two agents may share a cache pool only when their classes are compatible —
/// see [`ModelClass::compatibility`]. The `embedding_space` field is separate
/// from the model id on purpose: semantic reuse is governed by which embedding
/// model produced the vectors, not by which model consumes the text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelClass {
    /// `"anthropic"`, `"openai"`, `"local"`, ...
    pub provider: String,
    /// Family within the provider: `"claude-opus"`, `"gpt-4"`, `"llama-3"`.
    pub family: String,
    /// Exact revision, e.g. `"claude-opus-5"`. Required for tensor-level reuse.
    pub revision: String,
    /// Identifier of the embedding model whose vectors index this pool.
    /// `None` means the pool is exact-key only (blind mode).
    pub embedding_space: Option<String>,
    /// Weight quantization / serving variant. Differs → KV blocks differ.
    pub quantization: Option<String>,
}

impl ModelClass {
    /// How much reuse is safe between two classes.
    pub fn compatibility(&self, other: &Self) -> Compatibility {
        if self == other {
            return Compatibility::Identical;
        }
        let same_family = self.provider == other.provider && self.family == other.family;
        let same_space = self.embedding_space.is_some()
            && self.embedding_space == other.embedding_space;
        if same_family && same_space {
            Compatibility::SameFamily
        } else if same_space {
            Compatibility::SameEmbeddingSpace
        } else {
            Compatibility::Incompatible
        }
    }

    /// Stable string used as the cache-pool namespace.
    pub fn pool_key(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            self.provider,
            self.family,
            self.revision,
            self.embedding_space.as_deref().unwrap_or("-"),
            self.quantization.as_deref().unwrap_or("-"),
        )
    }
}

impl fmt::Display for ModelClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.pool_key())
    }
}

/// The reuse ceiling between two model classes.
///
/// This is the rule that answers "shared KV store between models of the same
/// type": *type* is [`ModelClass`], and how same they are decides what may
/// cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Compatibility {
    /// Different embedding space and different family: nothing is reusable.
    Incompatible,
    /// Same vector space, different model family. Embeddings and retrieval
    /// artifacts may be reused; generated text should be treated as advisory.
    SameEmbeddingSpace,
    /// Same provider + family + vector space, different revision or size.
    /// Text-level artifacts (summaries, tool results, RAG chunks) are reusable;
    /// raw KV/tensor blocks are **not**.
    SameFamily,
    /// Byte-identical serving class. Everything is reusable, including raw
    /// prompt-prefix KV blocks.
    Identical,
}

// ---------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------

/// Identifies one *admission* — one running instance of an agent — uniquely.
///
/// Distinct from [`Principal::agent`] (the readable role name, e.g.
/// `"search"`, reused every time that role is admitted) and
/// [`Principal::harness`] (the routing address). Minted fresh by
/// [`super::kernel::Kernel::admit`], so a role spawned, terminated, and
/// spawned again never collides with its earlier self in the agent registry
/// or the audit trail, even though its name and harness id are identical
/// both times.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentUid(pub Uuid);

impl AgentUid {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// First 8 hex characters — the form [`super::audit::AuditEvent::summarize`]
    /// prints, the same convention as a git short hash. The full value is
    /// always available via `.0` (or [`fmt::Display`]) when precision matters.
    pub fn short(&self) -> String {
        self.0.simple().to_string()[..8].to_string()
    }
}

impl Default for AgentUid {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentUid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The subject of an access check: a running harness plus its credentials.
///
/// Built by the kernel at spawn time (see [`super::kernel`]); never constructed
/// by an agent or a tool, because a principal that can name itself can lie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// Unique to this admission — see [`AgentUid`].
    pub uid: AgentUid,
    /// Which process this is. Matches the bus routing address.
    pub harness: crate::types::HarnessId,
    /// The agent role running inside it — what rules usually match on.
    pub agent: String,
    /// The model behind the agent. Decides cache-pool membership.
    pub model_class: ModelClass,
    /// Isolation domain.
    pub tenant: TenantId,
    /// Privilege band.
    pub trust: TrustTier,
    /// Who spawned this principal, if anyone. Grants may only ever be a subset
    /// of the parent's — see [`super::policy::GrantSet::attenuate`].
    pub parent: Option<crate::types::HarnessId>,
    /// The spawning principal's own [`AgentUid`], if any — the precise,
    /// collision-proof edge [`super::kernel::GuardedServices::list_agents`]
    /// walks to decide descendant visibility. `parent` above is kept
    /// alongside this for display/routing; this field is what structural
    /// checks actually use.
    pub parent_uid: Option<AgentUid>,
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}@{})", self.agent, self.harness, self.tenant)
    }
}
