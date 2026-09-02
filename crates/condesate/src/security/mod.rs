//! The permission boundary: who is acting, what they may do, and the record
//! of every time that was checked. Full design notes:
//! `docs/boundaries-and-shared-cache.md`.
//!
//! | Module | Holds |
//! |---|---|
//! | [`identity`] | `Principal`, `ModelClass`, `GovernanceLabel` |
//! | [`policy`] | `Rule`, `GrantSet`, `PolicyEngine` — the deny-wins evaluator |
//! | [`kernel`] | `Kernel`, `GuardedServices` — where checks are enforced |
//! | [`audit`] | `AuditSink` — the append-only decision trail |

pub mod audit;
pub mod identity;
pub mod kernel;
pub mod policy;

pub use audit::{AuditEvent, AuditSink, Clock, FileAudit, MemoryAudit, Outcome, SystemClock, TracingAudit};
pub use identity::{AgentUid, Compatibility, GovernanceLabel, ModelClass, Principal, TenantId, TrustTier};
pub use kernel::{AgentInfo, AgentManifest, Admission, GuardedServices, Kernel, Refusal};
pub use policy::{
    AccessRequest, Action, Condition, Decision, Effect, GrantSet, Obligation, Pattern, PolicyEngine,
    Resource, ResourcePattern, Rule, RuleSetPolicy, SubjectMatch, ToolBroker,
};
