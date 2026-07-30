//! The shared KV store, scoped by model class.
//!
//! ## The sharing rule
//!
//! One [`CachePool`] per [`ModelClass`]. Agents share a pool when their model
//! classes are the same; they may *reach into* another pool only as far as
//! [`Compatibility`] allows, and only as far as policy allows. Concretely:
//!
//! ```text
//!   agent A (claude-opus-5) ─┐
//!   agent B (claude-opus-5) ─┼─► pool "anthropic/claude-opus/claude-opus-5/emb-v1/-"
//!   agent C (claude-opus-5) ─┘        everything shared, including KV blocks
//!
//!   agent D (claude-sonnet-5) ─► pool "anthropic/claude-sonnet/..."
//!        SameFamily with the pool above: may read summaries, plans, RAG chunks
//!        MAY NOT read KvBlock values — different tensor layout
//!
//!   agent E (llama-3, no shared embedding space) ─► its own pool, isolated
//! ```
//!
//! ## Three gates, in this order
//!
//! A value reaches a model only after passing all three. They are separate
//! because each can be wrong in a different way, and because collapsing them is
//! the classic mistake — semantic similarity is not an authorization decision:
//!
//! 1. **Geometry** — is the value near the demand? ([`CachePool::lookup`])
//! 2. **Compatibility** — may this model class reuse this value class?
//!    ([`Compatibility`] vs [`ValueClass::min_compatibility`])
//! 3. **Authorization** — may *this principal* see *this label*?
//!    ([`crate::security::policy::PolicyEngine`], applied by [`crate::security::kernel`])
//!
//! [`Compatibility`]: crate::security::identity::Compatibility
//! [`ValueClass::min_compatibility`]: crate::cache::ValueClass::min_compatibility

use super::entry::{Admitted, CacheEntry, CacheKey, Candidate, Demand, RejectReason, Sidecar};
use crate::security::identity::ModelClass;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// A single-model-class key/value pool with exact and near-neighbour reads.
///
/// Implementations are free to be anything with these operations: an in-process
/// map, a sqlite file, Redis, a vector index. The pool is responsible for
/// geometry and capacity; it is **not** responsible for authorization.
#[async_trait]
pub trait CachePool: Send + Sync {
    /// The model class this pool serves. Its `pool_key()` is the namespace.
    fn class(&self) -> &ModelClass;

    /// Exact-key read. The blind-mode path: works with no embeddings at all.
    async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>>;

    /// Near-neighbour read. Returns candidates ordered by distance, already
    /// filtered by value class and TTL, **not** filtered by authorization.
    async fn lookup(&self, demand: &Demand) -> Result<Vec<Candidate>>;

    /// Write a value. The pool applies capacity pressure and may evict; it
    /// reports what it did via [`Admitted`].
    async fn put(&self, entry: CacheEntry) -> Result<Admitted>;

    /// Remove one entry. Used by revocation and by label-change invalidation.
    async fn invalidate(&self, key: &CacheKey) -> Result<bool>;

    /// Live sidecars, for the sketch builder. Never returns values.
    async fn sidecars(&self) -> Result<Vec<Sidecar>>;

    /// Current occupancy, for the planner's capacity multiplier.
    async fn usage(&self) -> Result<PoolUsage>;
}

/// Capacity accounting for one pool. Mirrors the byte-budget terms a pull
/// planner needs: capacity, used, reserve, and how much may be displaced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolUsage {
    pub capacity_bytes: u64,
    pub used_bytes: u64,
    /// Headroom never spent on remote pulls, kept for local writes.
    pub reserve_bytes: u64,
    /// How many bytes this round may displace.
    pub eviction_budget_bytes: u64,
    pub entries: u64,
}

impl PoolUsage {
    /// Free space after the reserve.
    pub fn free_after_reserve(&self) -> u64 {
        self.capacity_bytes
            .saturating_sub(self.used_bytes)
            .saturating_sub(self.reserve_bytes)
    }

    /// The most a single planning round may move into this pool.
    pub fn round_budget(&self, hard_cap: u64) -> u64 {
        hard_cap.min(self.free_after_reserve() + self.eviction_budget_bytes)
    }
}

/// Routes a model class to its pool. The object the swarm actually holds.
///
/// This is where "models of the same type share a store" is *implemented*: two
/// principals calling `pool_for` with equal classes get the same `Arc`.
#[async_trait]
pub trait CacheRegistry: Send + Sync {
    /// The pool for this class, created on first use.
    async fn pool_for(&self, class: &ModelClass) -> Result<Arc<dyn CachePool>>;

    /// Every pool currently resident. Used when a demand may be satisfied from
    /// a compatible-but-not-identical pool.
    async fn pools(&self) -> Result<Vec<Arc<dyn CachePool>>>;

    /// Pools whose class is reusable by `asker` at or above `min`, ordered
    /// most-compatible first. The read path for cross-class reuse.
    async fn compatible_pools(
        &self,
        asker: &ModelClass,
        min: crate::security::identity::Compatibility,
    ) -> Result<Vec<Arc<dyn CachePool>>> {
        let mut out: Vec<(crate::security::identity::Compatibility, Arc<dyn CachePool>)> = self
            .pools()
            .await?
            .into_iter()
            .map(|p| (asker.compatibility(p.class()), p))
            .filter(|(c, _)| *c >= min)
            .collect();
        out.sort_by_key(|(compat, _)| std::cmp::Reverse(*compat));
        Ok(out.into_iter().map(|(_, p)| p).collect())
    }
}

/// Checks an inbound entry before it is admitted. Fails closed.
///
/// Runs on every write, local or remote, so a compromised peer and a buggy
/// local tool hit the same wall. Rejections are peer-quality evidence — see
/// [`crate::cache::federation::PeerQuality`].
pub trait Validator: Send + Sync {
    /// `Ok(())` means admissible. Never returns `Ok` on doubt.
    fn validate(
        &self,
        entry: &CacheEntry,
        pool_class: &ModelClass,
        now_ms: u64,
    ) -> Result<(), RejectReason>;
}

/// Placeholder for the real validator. Refuses everything until implemented,
/// which is the correct behaviour for a missing safety check.
///
/// The checks it owes, spelled out so the implementation has something to be
/// measured against:
///
/// * value bytes hash to `sidecar.digest`
/// * embedding space and dimension match the pool
/// * embedding is unit-normalized
/// * `sidecar.class` is compatible with `pool_class` at or above
///   `value_class.min_compatibility()`
/// * TTL has not elapsed
/// * the entry carries a governance label
/// * declared `bytes` matches the actual value length
pub struct StandardValidator;

impl Validator for StandardValidator {
    fn validate(
        &self,
        _entry: &CacheEntry,
        _pool_class: &ModelClass,
        _now_ms: u64,
    ) -> Result<(), RejectReason> {
        Err(RejectReason::ValidatorUnavailable)
    }
}
