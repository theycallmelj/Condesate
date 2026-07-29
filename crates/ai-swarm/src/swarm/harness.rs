//! The harness: one running "process" in the swarm.
//!
//! A harness pairs an `Agent` with an `AgentLoop` and an `Inbox`. It behaves
//! like an actor:
//!   1. optionally process a seed task,
//!   2. then block on its inbox, running the loop once per incoming message,
//!   3. exit on `Shutdown` (or when the inbox closes).
//!
//! `Harness` is a trait so you can implement radically different runtimes
//! (batch, cron-driven, streaming, a REPL, ...) and still register them in the
//! same swarm. Every implementation runs against a `GuardedServices`, not a
//! raw `ServiceHandle` — the swarm admits each harness through a `Kernel`
//! before spawning it, so there is no runtime path that reaches a tool or a
//! peer without going through the permission boundary.

use super::bus::{Envelope, Inbox, Payload};
use crate::agent::{Agent, AgentContext, AgentLoop};
use crate::security::kernel::GuardedServices;
use crate::types::{HarnessId, Message};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    /// The swarm injects this harness's inbox before spawning it.
    fn install_inbox(&mut self, inbox: Inbox);
    /// Run until shutdown. Consumes the injected inbox internally.
    async fn run(&mut self, services: Arc<GuardedServices>) -> Result<()>;
}

/// Default harness implementation.
pub struct StandardHarness {
    id: HarnessId,
    agent: Box<dyn Agent>,
    agent_loop: Box<dyn AgentLoop>,
    seed: Option<String>,
    inbox: Option<Inbox>,
    /// Incremented per activation, so every audit correlation id is unique
    /// even across many messages to the same harness.
    activation_seq: u64,
    /// Byte budget handed to `GuardedServices::begin_activation` each round —
    /// the planning-round cap a cache-pull policy condition can check against.
    byte_budget: u64,
}

impl StandardHarness {
    pub fn new(
        id: impl Into<String>,
        agent: Box<dyn Agent>,
        agent_loop: Box<dyn AgentLoop>,
    ) -> Self {
        Self {
            id: HarnessId::new(id),
            agent,
            agent_loop,
            seed: None,
            inbox: None,
            activation_seq: 0,
            byte_budget: 64 * 1024 * 1024,
        }
    }

    /// Give this harness an initial task to run before it starts listening.
    pub fn with_seed(mut self, task: impl Into<String>) -> Self {
        self.seed = Some(task.into());
        self
    }

    /// Override the default per-activation byte budget.
    pub fn with_byte_budget(mut self, bytes: u64) -> Self {
        self.byte_budget = bytes;
        self
    }

    /// Run the agent loop once over a single inbound text, fresh transcript.
    async fn activate(&mut self, text: &str, services: &Arc<GuardedServices>) -> Result<()> {
        self.activation_seq += 1;
        services.begin_activation(format!("{}-{}", self.id, self.activation_seq), self.byte_budget);

        println!("  ┌─ {} activated: {text}", self.id);
        let mut ctx = AgentContext::new(services.clone());
        ctx.transcript.push(Message::user(text.to_string()));
        let outcome = self.agent_loop.run(self.agent.as_ref(), &mut ctx).await?;
        println!(
            "  └─ {} done in {} step(s): {}",
            self.id, outcome.steps, outcome.final_text
        );
        Ok(())
    }
}

#[async_trait]
impl Harness for StandardHarness {
    fn id(&self) -> HarnessId {
        self.id.clone()
    }

    fn install_inbox(&mut self, inbox: Inbox) {
        self.inbox = Some(inbox);
    }

    async fn run(&mut self, services: Arc<GuardedServices>) -> Result<()> {
        let mut inbox = self
            .inbox
            .take()
            .expect("swarm must install an inbox before run()");

        // 1. Seed task, if any.
        if let Some(seed) = self.seed.take() {
            self.activate(&seed, &services).await?;
        }

        // 2. Serve the inbox until shutdown.
        while let Some(Envelope { from, payload, .. }) = inbox.recv().await {
            match payload {
                Payload::Shutdown => {
                    println!("  ✗ {} received shutdown", self.id);
                    break;
                }
                Payload::Task(t) | Payload::Reply(t) | Payload::Note(t) => {
                    self.activate(&format!("[from {from}] {t}"), &services).await?;
                }
                Payload::Custom(v) => {
                    self.activate(&format!("[from {from}] {v}"), &services).await?;
                }
            }
        }
        Ok(())
    }
}
