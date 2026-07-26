//! # ai-swarm
//!
//! A trait-driven AI agent harness with a swarm layer that behaves like a tiny
//! operating system.
//!
//! ## The layering (bottom to top)
//!
//! ```text
//!   ModelProvider   how an agent is *called*        (LocalModel / CloudModel)
//!        |
//!   Agent           model + prompt + tools          (BasicAgent)
//!        |
//!   AgentLoop       how the loop *works*            (SingleShot / ReActLoop)
//!        |
//!   Harness         one running "process"           (StandardHarness)
//!        |
//!   Swarm           OS: scheduling, IPC, storage    (Swarm)
//! ```
//!
//! Every boundary is a trait, so any layer can be replaced independently.
//! Cross-cutting services (`Storage`, `Bus`) are handed to harnesses via a
//! `ServiceHandle`, keeping agents and tools decoupled from swarm internals.
//!
//! ## The two cross-cutting planes
//!
//! Alongside that stack sit two things every layer touches. Both are currently
//! trait-and-type scaffolding — the shapes are settled, the implementations are
//! deliberately not written yet.
//!
//! ```text
//!   identity ─► policy ─► kernel ─► audit        the permission boundary
//!                            │
//!                            ▼
//!                          cache                 the shared KV store
//! ```
//!
//! * [`identity`] — who is acting (`Principal`) and what model they are
//!   (`ModelClass`), plus the labels that travel with data.
//! * [`policy`] — capabilities, allow/deny rules, and the deny-wins evaluator.
//! * [`kernel`] — where the boundary is actually enforced: manifests are
//!   admitted, the syscall surface is wrapped, every crossing is audited.
//! * [`audit`] — the append-only record of every decision, denials included.
//! * [`cache`] — a KV pool per `ModelClass`, so agents running the same model
//!   share work, with an optional peer-federation layer on top.

pub mod agent;
pub mod audit;
pub mod bus;
pub mod cache;
pub mod harness;
pub mod identity;
pub mod kernel;
pub mod loops;
pub mod model;
pub mod policy;
#[cfg(feature = "remote")]
pub mod remote;
pub mod service;
/// Pure provider request/response mapping (always compiled + tested).
pub mod wire;
pub mod storage;
pub mod swarm;
pub mod tool;
pub mod types;

// Convenient flat re-exports.
pub use agent::{Agent, AgentContext, BasicAgent};
pub use audit::{AuditEvent, AuditSink, Clock, MemoryAudit, Outcome, SystemClock};
pub use bus::{Bus, Envelope, Inbox, Payload, Recipient};
pub use cache::{
    CacheEntry, CacheKey, CachePool, CacheRegistry, Candidate, Demand, Sidecar, ValueClass,
};
pub use identity::{Compatibility, GovernanceLabel, ModelClass, Principal, TenantId, TrustTier};
pub use kernel::{AgentManifest, Admission, GuardedServices, Kernel, Refusal};
pub use policy::{
    AccessRequest, Action, Decision, Effect, GrantSet, Obligation, Pattern, PolicyEngine, Resource,
    ResourcePattern, Rule, RuleSetPolicy, SubjectMatch, ToolBroker,
};
pub use harness::{Harness, StandardHarness};
pub use loops::{AgentLoop, LoopOutcome, ReActLoop, SingleShot};
pub use model::{CloudModel, LocalModel, ModelProvider};
#[cfg(feature = "remote")]
pub use remote::{AnthropicModel, OpenAiModel};
pub use service::ServiceHandle;
pub use storage::{InMemoryStorage, Storage};
pub use swarm::Swarm;
pub use tool::{Remember, SendMessage, ShutdownSwarm, Tool, WordCount};
pub use types::{
    CompletionRequest, CompletionResponse, HarnessId, Message, Role, ToolCall, ToolSpec,
};
