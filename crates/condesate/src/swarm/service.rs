//! The bundle of swarm-provided services handed to every harness at spawn time.
//!
//! Think of this as the "syscall surface" the OS (swarm) exposes to a process
//! (harness): who am I, who else exists, how do I message them, where's shared
//! storage. Agents and tools reach the outside world only through this handle,
//! which keeps them decoupled from the swarm's internals.

use super::bus::{Bus, Envelope, Payload, Recipient};
use super::storage::Storage;
use crate::types::HarnessId;
use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServiceHandle {
    /// This harness's own id.
    pub me: HarnessId,
    /// Every harness id in the swarm (the "process table").
    pub roster: Arc<Vec<HarnessId>>,
    /// Shared key/value storage.
    pub storage: Arc<dyn Storage>,
    /// Message bus for talking to peers.
    pub bus: Bus,
}

impl ServiceHandle {
    /// Convenience: send a task to a specific peer.
    pub fn send_task(&self, to: impl Into<String>, text: impl Into<String>) -> Result<()> {
        self.bus.dispatch(Envelope {
            from: self.me.clone(),
            to: Recipient::Harness(HarnessId::new(to)),
            payload: Payload::Task(text.into()),
        })
    }

    /// Convenience: reply to a specific peer.
    pub fn send_reply(&self, to: impl Into<String>, text: impl Into<String>) -> Result<()> {
        self.bus.dispatch(Envelope {
            from: self.me.clone(),
            to: Recipient::Harness(HarnessId::new(to)),
            payload: Payload::Reply(text.into()),
        })
    }

    /// Convenience: tell the whole swarm to wind down.
    pub fn broadcast_shutdown(&self) -> Result<()> {
        self.bus.dispatch(Envelope {
            from: self.me.clone(),
            to: Recipient::Broadcast,
            payload: Payload::Shutdown,
        })
    }
}
