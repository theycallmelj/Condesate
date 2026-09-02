//! # condesate
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
//! Cross-cutting services (`Storage`, `Bus`) reach agents and tools only
//! through a guarded handle, never a raw one — see `security::kernel`.
//!
//! ## Module tree
//!
//! ```text
//!   agent/     how one agent is called and thinks   (live)
//!   swarm/     the OS layer: harness, IPC, storage   (live)
//!   security/  the permission boundary               (live: gates every
//!                                                      tool call, memory
//!                                                      access, peer message)
//!   cache/     shared KV store per ModelClass         (scaffolding: traits
//!                                                      settled, no CachePool
//!                                                      impl yet)
//!   faraday/   memory & context engineering           (diary/ideabook/index
//!                                                      are real; not yet
//!                                                      wired into the loop)
//!   types.rs   plain data types shared by everything above
//! ```
//!
//! * [`agent`] — `ModelProvider -> Agent -> AgentLoop`, plus `Tool`.
//! * [`swarm`] — `Harness -> Swarm`, the bus, storage, and the raw
//!   `ServiceHandle` that `security::kernel` wraps.
//! * [`security`] — `Principal` × `Action` × `Resource` → `Decision`, admitted
//!   into an attenuated `GrantSet`, checked and audited on every crossing.
//!   `Tool::call` and `Harness::run` both take a guarded handle — there is no
//!   raw `ServiceHandle` reachable from tool or loop code.
//! * [`cache`] — a KV pool per `ModelClass`, so agents running the same model
//!   share work, with an optional peer-federation layer on top.
//! * [`faraday`] — memory and context engineering, named for Michael
//!   Faraday's own notebooks: a permanent, addressable `Diary` (episodic
//!   memory), a revisable `IdeaBook` (in-loop working memory), and
//!   `Slip`/`RetrievalSheet` composition that preserves surrounding context
//!   on retrieval rather than returning bare facts.

pub mod agent;
pub mod cache;
pub mod faraday;
pub mod security;
pub mod swarm;
pub mod types;

// Convenient flat re-exports — the public API shape is unaffected by which
// folder a module physically lives in.
pub use agent::{
    run_repl, tool_instructions, Agent, AgentContext, AgentLoop, BasicAgent, CloudModel,
    LocalModel, LoopOutcome, ModelProvider, PromptedToolModel, ReActLoop, Remember, ReplOnError,
    ReplOptions, SendMessage, ShutdownSwarm, SingleShot, Tool, WordCount,
};
#[cfg(feature = "remote")]
pub use agent::{AnthropicModel, OpenAiModel};
#[cfg(feature = "mcp")]
pub use agent::{McpConnection, McpTool};
#[cfg(feature = "a2a")]
pub use agent::{A2aClient, AgentCard, AgentSkill, DiscoverA2aAgent, SendA2aMessage};
pub use cache::{
    CacheEntry, CacheKey, CachePool, CacheRegistry, Candidate, Demand, Sidecar, ValueClass,
};
pub use faraday::{
    Diary, DiaryEntry, EntryKind, IdeaBook, InMemoryDiary, InMemoryIdeaBook, InMemorySlipIndex,
    Menu, NewDiaryEntry, RetrievalSheet, SheetComposer, Slip, SlipIndex, Speculation,
    StandardComposer,
};
pub use security::{
    AccessRequest, Action, AgentInfo, AgentManifest, AgentUid, Admission, AuditEvent, AuditSink,
    Clock, Compatibility, Condition, Decision, Effect, FileAudit, GovernanceLabel, GrantSet,
    GuardedServices, Kernel, MemoryAudit, ModelClass, Obligation, Outcome, Pattern, PolicyEngine,
    Principal, Refusal, Resource, ResourcePattern, Rule, RuleSetPolicy, SubjectMatch, SystemClock,
    TenantId, ToolBroker, TracingAudit, TrustTier,
};
pub use swarm::{
    Bus, Envelope, Harness, Inbox, InMemoryStorage, Payload, Recipient, ServiceHandle,
    StandardHarness, Storage, Swarm,
};
pub use types::{
    CompletionRequest, CompletionResponse, HarnessId, Message, Role, ToolCall, ToolSpec,
};
