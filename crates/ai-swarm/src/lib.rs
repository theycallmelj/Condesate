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

pub mod agent;
pub mod bus;
pub mod harness;
pub mod loops;
pub mod model;
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
pub use bus::{Bus, Envelope, Inbox, Payload, Recipient};
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
