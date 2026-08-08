//! How one agent is called and thinks: `ModelProvider -> Agent -> AgentLoop`,
//! plus the tools a loop can invoke.
//!
//! | Module | Holds |
//! |---|---|
//! | [`model`] | `ModelProvider` — `LocalModel` / `CloudModel` mocks |
//! | [`wire`] | pure request/response mapping for real providers |
//! | [`remote`] (feature `remote`) | `AnthropicModel` / `OpenAiModel` |
//! | [`core`] | `Agent`, `AgentContext`, `BasicAgent` |
//! | [`loops`] | `AgentLoop` — `SingleShot` / `ReActLoop` |
//! | [`tool`] | `Tool` and the built-in tools |
//! | [`mcp`] (feature `mcp`) | `McpConnection` / `McpTool` — tools proxied over the MCP protocol |
//! | [`prompted_tools`] | `PromptedToolModel` — prompted tool-calling for providers with no native tool-use wire format yet |

pub mod core;
pub mod loops;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod model;
pub mod prompted_tools;
#[cfg(feature = "remote")]
pub mod remote;
pub mod tool;
/// Pure provider request/response mapping (always compiled + tested).
pub mod wire;

pub use core::{Agent, AgentContext, BasicAgent};
pub use loops::{AgentLoop, LoopOutcome, ReActLoop, SingleShot};
#[cfg(feature = "mcp")]
pub use mcp::{McpConnection, McpTool};
pub use model::{CloudModel, LocalModel, ModelProvider};
pub use prompted_tools::{tool_instructions, PromptedToolModel};
#[cfg(feature = "remote")]
pub use remote::{AnthropicModel, OpenAiModel};
pub use tool::{Remember, SendMessage, ShutdownSwarm, Tool, WordCount};
