//! The agent: a model + a system prompt + a toolbelt.
//!
//! An `Agent` is deliberately *passive*: given the running transcript it
//! produces one assistant turn (text + optional tool calls). It does **not**
//! decide how many times to run — that's the `AgentLoop`'s job. This separation
//! is what lets you keep the same agent but change its control flow.

use crate::model::ModelProvider;
use crate::service::ServiceHandle;
use crate::tool::Tool;
use crate::types::{CompletionRequest, CompletionResponse, Message};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// Mutable working state threaded through a single "activation" of a harness.
pub struct AgentContext {
    /// Growing conversation transcript for this activation.
    pub transcript: Vec<Message>,
    /// Swarm services (storage, bus, identity) available to tools.
    pub services: ServiceHandle,
}

impl AgentContext {
    pub fn new(services: ServiceHandle) -> Self {
        Self { transcript: Vec::new(), services }
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

        let req = CompletionRequest {
            messages,
            tools: self.tools.iter().map(|t| t.spec()).collect(),
            temperature: 0.0,
        };
        self.provider.complete(req).await
    }
}
