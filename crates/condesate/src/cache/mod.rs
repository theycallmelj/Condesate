//! Shared KV cache for models of the same type.
//!
//! Two layers, usable independently:
//!
//! * [`pool`] — the local store. One pool per [`ModelClass`], exact-key and
//!   near-neighbour reads, capacity accounting, fail-closed validation. This
//!   alone gives you a cache shared by every agent running the same model.
//! * [`federation`] — optional. Nodes advertise bounded ownership sketches and
//!   pull values from each other only when a planner expects reuse. Adopt this
//!   when one shared pool stops being reachable from every node.
//!
//! ## Start here
//!
//! ```text
//!   write   agent ──► put(entry) ──► Validator ──► pool[ModelClass]
//!                                       │
//!                                       └─ fail closed on digest / dim /
//!                                          compatibility / TTL / label
//!
//!   read    agent ──► lookup(demand)
//!                        │
//!                        ├─ 1. geometry     near enough?         (pool)
//!                        ├─ 2. compatible   reusable class?      (pool)
//!                        └─ 3. authorized   label clears?        (policy)
//!                                              │
//!                                    all three ▼ → value reaches the model
//! ```
//!
//! Gate 3 is not the cache's decision. A pool returns [`Candidate`]s, not hits;
//! the kernel turns candidates into hits by asking [`crate::security::policy`].
//! Keeping the naming honest here is what stops "the embeddings matched" from
//! quietly becoming "the agent was allowed to see it".
//!
//! [`ModelClass`]: crate::security::identity::ModelClass

pub mod entry;
pub mod federation;
pub mod pool;

pub use entry::{
    Admitted, CacheEntry, CacheKey, Candidate, Demand, Embedding, RejectReason, Sidecar, ValueClass,
};
pub use federation::{
    Ellipsoid, PeerQuality, PeerTransport, PlanInputs, PullOutcome, PullPlan, PullPlanner,
    PullRequest, PullResponse, Range, RangeSketch, SketchBudget, SketchDirectory, SketchPublisher,
};
pub use pool::{CachePool, CacheRegistry, PoolUsage, StandardValidator, Validator};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::identity::{Compatibility, ModelClass};

    fn class(family: &str, revision: &str, space: Option<&str>) -> ModelClass {
        ModelClass {
            provider: "anthropic".into(),
            family: family.into(),
            revision: revision.into(),
            embedding_space: space.map(str::to_string),
            quantization: None,
        }
    }

    #[test]
    fn identical_classes_share_one_pool() {
        let a = class("claude-opus", "claude-opus-5", Some("emb-v1"));
        let b = class("claude-opus", "claude-opus-5", Some("emb-v1"));
        assert_eq!(a.pool_key(), b.pool_key());
        assert_eq!(a.compatibility(&b), Compatibility::Identical);
    }

    #[test]
    fn quantization_splits_the_pool() {
        let a = class("claude-opus", "claude-opus-5", Some("emb-v1"));
        let mut b = a.clone();
        b.quantization = Some("int8".into());
        assert_ne!(a.pool_key(), b.pool_key());
        // Same family and space, so text artifacts still cross...
        assert_eq!(a.compatibility(&b), Compatibility::SameFamily);
        // ...but KV blocks do not.
        assert!(a.compatibility(&b) < ValueClass::KvBlock.min_compatibility());
    }

    #[test]
    fn kv_blocks_never_cross_revisions_but_summaries_do() {
        let opus = class("claude-opus", "claude-opus-5", Some("emb-v1"));
        let sonnet = class("claude-sonnet", "claude-sonnet-5", Some("emb-v1"));
        let compat = opus.compatibility(&sonnet);

        assert_eq!(compat, Compatibility::SameEmbeddingSpace);
        assert!(compat < ValueClass::KvBlock.min_compatibility());
        assert!(compat < ValueClass::Summary.min_compatibility());
        assert!(compat >= ValueClass::RagChunk.min_compatibility());
    }

    #[test]
    fn different_embedding_space_isolates_completely() {
        let a = class("claude-opus", "claude-opus-5", Some("emb-v1"));
        let b = class("claude-opus", "claude-opus-5", Some("emb-v2"));
        assert_eq!(a.compatibility(&b), Compatibility::Incompatible);
    }

    #[test]
    fn round_budget_respects_reserve_and_hard_cap() {
        let usage = PoolUsage {
            capacity_bytes: 1_000,
            used_bytes: 600,
            reserve_bytes: 100,
            eviction_budget_bytes: 50,
            entries: 12,
        };
        assert_eq!(usage.free_after_reserve(), 300);
        assert_eq!(usage.round_budget(1_000), 350);
        assert_eq!(usage.round_budget(200), 200, "hard cap wins");

        let full = PoolUsage { used_bytes: 1_000, ..usage };
        assert_eq!(full.free_after_reserve(), 0);
        assert_eq!(full.round_budget(1_000), 50, "only the eviction budget");
    }
}
