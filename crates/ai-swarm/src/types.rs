//! Plain data types shared across every layer of the framework.
//!
//! These deliberately hold *no behavior*. Behavior lives in the traits
//! (`ModelProvider`, `Agent`, `AgentLoop`, `Tool`, `Storage`, `Harness`) so
//! that any piece can be swapped without touching the wire format.

use serde_json::Value;

/// Who authored a message in a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    /// The observation returned by executing a tool.
    Tool,
}

/// A single turn in a conversation transcript.
#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(c: impl Into<String>) -> Self {
        Self { role: Role::System, content: c.into() }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self { role: Role::User, content: c.into() }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: c.into() }
    }
    pub fn tool(c: impl Into<String>) -> Self {
        Self { role: Role::Tool, content: c.into() }
    }
}

/// What we hand a model provider to get the next assistant turn.
#[derive(Clone, Debug)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: f32,
}

/// What a model provider returns: some text, plus zero or more tool calls.
#[derive(Clone, Debug, Default)]
pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

/// A request from the model to invoke a named tool with JSON arguments.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

/// The advertised interface of a tool (what the model is told it can call).
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
}

/// Stable identifier for a harness inside a swarm. Used as a routing address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HarnessId(pub String);

impl HarnessId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for HarnessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
