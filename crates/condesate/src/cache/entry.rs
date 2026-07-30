//! What a cached value is made of.
//!
//! A cache entry is a value plus a [`Sidecar`] — the metadata that makes reuse
//! decidable. Everything a planner, a validator, or the policy engine needs to
//! reason about a value lives in the sidecar, so none of them ever has to look
//! at the value itself.

use crate::security::identity::{GovernanceLabel, ModelClass};

// ---------------------------------------------------------------------------
// Value classes
// ---------------------------------------------------------------------------

/// What *kind* of artifact a value is. Reuse rules differ sharply by class, so
/// this is a first-class field rather than a naming convention.
///
/// The compatibility floor for each class is enforced by policy conditions
/// (`Condition::MinCompatibility`), not by this enum — see
/// [`ValueClass::min_compatibility`] for the recommended defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ValueClass {
    /// Raw transformer KV blocks for a prompt prefix. Only ever reusable
    /// between byte-identical serving classes.
    KvBlock,
    /// A retrieved document chunk.
    RagChunk,
    /// A tool's output, keyed by tool + args.
    ToolResult,
    /// Model-written summary of a file, module, or transcript.
    Summary,
    /// A plan or task decomposition.
    Plan,
    /// A static analysis artifact: symbol table, call graph slice, test map.
    CodeFact,
    /// A bare embedding vector.
    Embedding,
    /// Anything the application defines.
    Other,
}

impl ValueClass {
    /// The weakest model-class relationship at which reuse of this class is
    /// still sound. Wire this into a `Condition::MinCompatibility` when you
    /// build the grant set.
    pub fn min_compatibility(&self) -> crate::security::identity::Compatibility {
        use crate::security::identity::Compatibility::*;
        match self {
            // Tensor layout is revision- and quantization-specific.
            ValueClass::KvBlock => Identical,
            // Generated text carries the writing model's judgment; keep it
            // inside one family.
            ValueClass::Summary | ValueClass::Plan => SameFamily,
            // Vector-space artifacts only need a common embedding space.
            ValueClass::RagChunk | ValueClass::Embedding => SameEmbeddingSpace,
            // Deterministic facts about the repo are model-independent, but we
            // still require a shared vector space to look them up.
            ValueClass::CodeFact | ValueClass::ToolResult => SameEmbeddingSpace,
            ValueClass::Other => Identical,
        }
    }
}

// ---------------------------------------------------------------------------
// Keys and vectors
// ---------------------------------------------------------------------------

/// Exact-identity key. Two entries with the same key in the same pool are the
/// same value.
///
/// The key is a *hash* of the identifying inputs (prompt prefix, tool + args,
/// file + revision), never the inputs themselves — pools are shared, and the
/// key surface is the widest thing a peer can see.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey(pub String);

impl CacheKey {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A normalized vector in a named embedding space.
///
/// The space name must match the owning pool's `ModelClass::embedding_space`;
/// mixing spaces silently is the single easiest way to produce confident
/// nonsense, so the check belongs in the validator, not in a comment.
#[derive(Clone, Debug, PartialEq)]
pub struct Embedding {
    pub space: String,
    pub dim: u16,
    pub values: Vec<f32>,
}

impl Embedding {
    pub fn is_normalized(&self, tol: f32) -> bool {
        let norm: f32 = self.values.iter().map(|v| v * v).sum::<f32>().sqrt();
        (norm - 1.0).abs() <= tol
    }
}

// ---------------------------------------------------------------------------
// Sidecar and entry
// ---------------------------------------------------------------------------

/// Metadata stored beside a value; the only thing planners and peers ever see.
#[derive(Clone, Debug)]
pub struct Sidecar {
    pub key: CacheKey,
    /// Pool the value belongs to — its owning model class.
    pub class: ModelClass,
    pub value_class: ValueClass,
    /// Present in semantic mode, absent in blind mode.
    pub embedding: Option<Embedding>,
    /// Classification that travels with the value. Checked by policy *after*
    /// the geometric match, never instead of it.
    pub label: GovernanceLabel,
    pub bytes: u64,
    pub written_at_ms: u64,
    pub ttl_ms: Option<u64>,
    /// Reuse counter. Drives weighting when ranges are summarized, and eviction
    /// priority when space runs out.
    pub hits: u32,
    /// Digest of the value bytes. A receiver recomputes it before admitting.
    pub digest: String,
}

impl Sidecar {
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.written_at_ms)
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.ttl_ms.is_some_and(|ttl| self.age_ms(now_ms) > ttl)
    }
}

/// A value plus its sidecar. The unit that moves between pools.
#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub sidecar: Sidecar,
    /// Opaque bytes. The cache layer never interprets these.
    pub value: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------

/// A read request against a pool.
///
/// Carries the *requesting principal's* model class and label context, because
/// a lookup is simultaneously a geometry question and an authorization
/// question, and the pool must not answer the first without the second being
/// answerable.
#[derive(Clone, Debug)]
pub struct Demand {
    /// The class of the model asking. Compared against the pool's class to
    /// derive a `Compatibility`.
    pub asker: ModelClass,
    pub value_class: Option<ValueClass>,
    /// Exact key, when the caller knows it.
    pub key: Option<CacheKey>,
    /// Query vector, for near-neighbour reads.
    pub embedding: Option<Embedding>,
    /// Maximum acceptable cosine distance.
    pub max_distance: f32,
    /// How many candidates to consider.
    pub limit: usize,
    /// How hot this demand is; feeds the planner's utility score.
    pub hotness: u32,
}

/// A near-neighbour result, before authorization.
///
/// Deliberately named "candidate", not "hit": geometry produced it, policy has
/// not yet cleared it.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub entry: CacheEntry,
    /// Cosine distance from the demand vector, `0.0` for an exact-key match.
    pub distance: f32,
    /// Reuse ceiling between the asker and the owning pool.
    pub compatibility: crate::security::identity::Compatibility,
}

/// Why a write was or was not admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admitted {
    /// Stored.
    Stored,
    /// Stored, displacing `evicted` bytes.
    StoredWithEviction { evicted_bytes: u64 },
    /// Refused: no space after the reserve, and nothing worth evicting.
    RejectedNoSpace,
    /// Refused: failed validation.
    RejectedInvalid(RejectReason),
    /// Refused by policy.
    RejectedByPolicy(String),
}

/// Why an inbound value was refused. These are the failure modes worth counting
/// per peer — a peer that produces them repeatedly should stop being asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Value bytes do not hash to the advertised digest.
    DigestMismatch,
    /// Embedding dimension or space does not match the pool.
    EmbeddingMismatch,
    /// The owning model class is not reusable by the asker.
    IncompatibleModelClass,
    /// TTL already elapsed.
    Expired,
    /// Already present.
    Duplicate,
    /// Response exceeded the requested bounds.
    OverBudget,
    /// Missing or malformed governance label.
    UnlabelledValue,
    /// No validator could run. Fail closed: an unchecked value is not admitted.
    ValidatorUnavailable,
}
