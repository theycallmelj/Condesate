//! The swarm: an OS-like layer over a set of harnesses.
//!
//! Responsibilities, in OS terms:
//!   * process table   — the roster of registered harnesses
//!   * IPC setup       — build the bus routing table (one inbox per harness)
//!   * shared memory   — hand every harness the same `Arc<dyn Storage>`
//!   * scheduling      — spawn each harness on the async runtime and join them
//!   * admission       — turn each harness's `AgentManifest` into a
//!     `GuardedServices` handle via the swarm's `Kernel`, before it is ever
//!     handed to the harness
//!
//! You register `Box<dyn Harness>` values, so any custom harness runtime slots
//! in. `run()` blocks until every harness has exited. There is no path from
//! `register` to a running harness that skips the kernel: every harness gets
//! exactly the authority its manifest was admitted for, nothing more.

use super::bus::{Bus, Inbox};
use super::harness::Harness;
use super::service::ServiceHandle;
use super::storage::Storage;
use crate::security::kernel::{AgentManifest, Kernel};
use crate::types::HarnessId;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct Swarm {
    storage: Arc<dyn Storage>,
    kernel: Arc<Kernel>,
    harnesses: Vec<(Box<dyn Harness>, AgentManifest)>,
    inboxes: HashMap<HarnessId, Inbox>,
    routes: HashMap<HarnessId, mpsc::UnboundedSender<super::bus::Envelope>>,
}

impl Swarm {
    pub fn new(storage: Arc<dyn Storage>, kernel: Arc<Kernel>) -> Self {
        Self {
            storage,
            kernel,
            harnesses: Vec::new(),
            inboxes: HashMap::new(),
            routes: HashMap::new(),
        }
    }

    /// Register a harness together with the manifest declaring what it needs.
    /// The swarm creates its inbox channel here so the routing table is
    /// complete before anything is spawned; admission itself happens in
    /// `run()`, once the full roster (and thus the bus) is known.
    pub fn register(&mut self, harness: Box<dyn Harness>, manifest: AgentManifest) {
        let id = harness.id();
        assert_eq!(
            id, manifest.harness,
            "manifest's harness id must match the registered harness's id"
        );
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.insert(id.clone(), tx);
        self.inboxes.insert(id, rx);
        self.harnesses.push((harness, manifest));
    }

    /// Wire up services, admit each harness through the kernel, and run every
    /// harness to completion.
    pub async fn run(mut self) -> Result<()> {
        let roster: Arc<Vec<HarnessId>> =
            Arc::new(self.harnesses.iter().map(|(h, _)| h.id()).collect());
        let bus = Bus::new(self.routes.clone());

        println!("swarm: booting {} harness(es)\n", self.harnesses.len());

        let mut tasks = Vec::new();
        for (mut harness, manifest) in std::mem::take(&mut self.harnesses) {
            let id = harness.id();
            let inbox = self
                .inboxes
                .remove(&id)
                .expect("every registered harness has an inbox");
            harness.install_inbox(inbox);

            let raw = ServiceHandle {
                me: id.clone(),
                roster: roster.clone(),
                storage: self.storage.clone(),
                bus: bus.clone(),
            };
            // Every harness in this swarm is top-level from the kernel's
            // perspective (no parent admission) — a harness that spawns its
            // own children would instead call `Kernel::admit` with
            // `Some(&its_own_admission)`.
            let admission = self.kernel.admit(&manifest, None);
            let guarded = Arc::new(self.kernel.attach(&admission, raw));

            // Each harness is its own scheduled "process".
            tasks.push(tokio::spawn(async move {
                if let Err(e) = harness.run(guarded).await {
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

    /// Access to the swarm's kernel (e.g. to inspect the audit trail, or to
    /// call `revoke_all` from outside the running swarm).
    pub fn kernel(&self) -> Arc<Kernel> {
        self.kernel.clone()
    }
}
