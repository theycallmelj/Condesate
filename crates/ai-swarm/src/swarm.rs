//! The swarm: an OS-like layer over a set of harnesses.
//!
//! Responsibilities, in OS terms:
//!   * process table   — the roster of registered harnesses
//!   * IPC setup       — build the bus routing table (one inbox per harness)
//!   * shared memory   — hand every harness the same `Arc<dyn Storage>`
//!   * scheduling      — spawn each harness on the async runtime and join them
//!
//! You register `Box<dyn Harness>` values, so any custom harness runtime slots
//! in. `run()` blocks until every harness has exited.

use crate::bus::{Bus, Inbox};
use crate::harness::Harness;
use crate::service::ServiceHandle;
use crate::storage::Storage;
use crate::types::HarnessId;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct Swarm {
    storage: Arc<dyn Storage>,
    harnesses: Vec<Box<dyn Harness>>,
    inboxes: HashMap<HarnessId, Inbox>,
    routes: HashMap<HarnessId, mpsc::UnboundedSender<crate::bus::Envelope>>,
}

impl Swarm {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            harnesses: Vec::new(),
            inboxes: HashMap::new(),
            routes: HashMap::new(),
        }
    }

    /// Register a harness. The swarm creates its inbox channel here so the
    /// routing table is complete before anything is spawned.
    pub fn register(&mut self, harness: Box<dyn Harness>) {
        let id = harness.id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.insert(id.clone(), tx);
        self.inboxes.insert(id, rx);
        self.harnesses.push(harness);
    }

    /// Wire up services and run every harness to completion.
    pub async fn run(mut self) -> Result<()> {
        let roster: Arc<Vec<HarnessId>> =
            Arc::new(self.harnesses.iter().map(|h| h.id()).collect());
        let bus = Bus::new(self.routes.clone());

        println!("swarm: booting {} harness(es)\n", self.harnesses.len());

        let mut tasks = Vec::new();
        for mut harness in std::mem::take(&mut self.harnesses) {
            let id = harness.id();
            let inbox = self
                .inboxes
                .remove(&id)
                .expect("every registered harness has an inbox");
            harness.install_inbox(inbox);

            let services = ServiceHandle {
                me: id.clone(),
                roster: roster.clone(),
                storage: self.storage.clone(),
                bus: bus.clone(),
            };

            // Each harness is its own scheduled "process".
            tasks.push(tokio::spawn(async move {
                if let Err(e) = harness.run(services).await {
                    eprintln!("harness '{id}' crashed: {e:?}");
                }
            }));
        }

        // Drop our own senders so no dangling references keep inboxes open.
        drop(bus);
        self.routes.clear();

        for t in tasks {
            let _ = t.await;
        }
        println!("\nswarm: all harnesses exited");
        Ok(())
    }

    /// Access to shared storage (e.g. to read results after the swarm halts).
    pub fn storage(&self) -> Arc<dyn Storage> {
        self.storage.clone()
    }
}
