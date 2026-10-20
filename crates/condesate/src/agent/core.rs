//! The agent: a model + a system prompt + a toolbelt.
//!
//! An `Agent` is deliberately *passive*: given the running transcript it
//! produces one assistant turn (text + optional tool calls). It does **not**
//! decide how many times to run — that's the `AgentLoop`'s job. This separation
//! is what lets you keep the same agent but change its control flow.

use super::model::ModelProvider;
use super::tool::Tool;
use crate::security::kernel::GuardedServices;
use crate::types::{CompletionRequest, CompletionResponse, Message};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// Mutable working state threaded through a single "activation" of a harness.
pub struct AgentContext {
    /// Growing conversation transcript for this activation.
    pub transcript: Vec<Message>,
    /// The guarded syscall surface available to tools — storage, bus,
    /// identity, all subject to whatever this principal was admitted with.
    pub services: Arc<GuardedServices>,
    /// When true, `AgentLoop` impls in `loops.rs` echo their internal
    /// reasoning (each think-step, each tool call and observation) to
    /// stderr. Off by default: that trace is a debugging aid, not part of
    /// an agent's actual reply, and a caller driving a user-facing chat
    /// (e.g. `run_repl`) should not have to see one agent's internals just
    /// because it happens to be calling another agent through a tool.
    pub trace: bool,
}

impl AgentContext {
    pub fn new(services: Arc<GuardedServices>) -> Self {
        Self { transcript: Vec::new(), services, trace: false }
    }
}

#[async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    /// Tools this agent is allowed to call.
    fn tools(&self) -> &[Arc<dyn Tool>];
    /// Produce the next assistant turn given the current context.
    async fn think(&self, ctx: &AgentContext) -> Result<CompletionResponse>;
}

/// Default agent: prepends a system prompt and forwards the transcript to its
/// model provider. Swap `provider` to move between local and cloud inference.
pub struct BasicAgent {
    name: String,
    system_prompt: String,
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
}

impl BasicAgent {
    pub fn new(
        name: impl Into<String>,
        system_prompt: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            name: name.into(),
            system_prompt: system_prompt.into(),
            provider,
            tools,
        }
    }
}

#[async_trait]
impl Agent for BasicAgent {
    fn name(&self) -> &str {
        &self.name
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    async fn think(&self, ctx: &AgentContext) -> Result<CompletionResponse> {
        let mut messages = Vec::with_capacity(ctx.transcript.len() + 1);
        messages.push(Message::system(self.system_prompt.clone()));
        messages.extend(ctx.transcript.iter().cloned());

        // Advertise-time gating: the model is only told about tools this
        // principal may actually invoke. Narrower than the call-time check in
        // `loops::execute_tools`, which remains authoritative regardless.
        let tools = self
            .tools
            .iter()
            .filter(|t| ctx.services.may_advertise(&t.spec().name))
            .map(|t| t.spec())
            .collect();

        let req = CompletionRequest { messages, tools, temperature: 0.0 };
        self.provider.complete(req).await
    }
}
