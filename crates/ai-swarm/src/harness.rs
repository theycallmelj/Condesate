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
//! same swarm.

use crate::agent::{Agent, AgentContext};
use crate::bus::{Envelope, Inbox, Payload};
use crate::loops::AgentLoop;
use crate::service::ServiceHandle;
use crate::types::{HarnessId, Message};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    /// The swarm injects this harness's inbox before spawning it.
    fn install_inbox(&mut self, inbox: Inbox);
    /// Run until shutdown. Consumes the injected inbox internally.
    async fn run(&mut self, services: ServiceHandle) -> Result<()>;
}

/// Default harness implementation.
pub struct StandardHarness {
    id: HarnessId,
    agent: Box<dyn Agent>,
    agent_loop: Box<dyn AgentLoop>,
    seed: Option<String>,
    inbox: Option<Inbox>,
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
        }
    }

    /// Give this harness an initial task to run before it starts listening.
    pub fn with_seed(mut self, task: impl Into<String>) -> Self {
        self.seed = Some(task.into());
        self
    }

    /// Run the agent loop once over a single inbound text, fresh transcript.
    async fn activate(&self, text: &str, services: &ServiceHandle) -> Result<()> {
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

    async fn run(&mut self, services: ServiceHandle) -> Result<()> {
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
