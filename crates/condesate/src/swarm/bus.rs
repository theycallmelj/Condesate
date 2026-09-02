
//! Inter-harness messaging — the swarm's "IPC".
//!
//! Each harness owns one `Inbox` (an mpsc receiver). The `Bus` is a cloneable
//! routing table mapping `HarnessId -> Sender`, so any harness can address any
//! other by id, or broadcast to everyone. This is the actor-model plumbing that
//! lets harnesses "talk" the way the swarm-as-OS is supposed to.
//!
//! The routing table is mutable behind a `Mutex`, not fixed at construction:
//! `Swarm::run` still wires up the whole roster before anything starts, but a
//! principal that dynamically spawns a child (`GuardedServices::spawn_child`,
//! outside `Swarm` entirely) needs to add that child's inbox to the *same*
//! bus its own `ServiceHandle` already carries, after the fact — see
//! [`Bus::insert_route`].

use crate::types::HarnessId;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A message travelling between harnesses.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub from: HarnessId,
    pub to: Recipient,
    pub payload: Payload,
}

#[derive(Clone, Debug)]
pub enum Recipient {
    Harness(HarnessId),
    Broadcast,
}

/// The kinds of things harnesses send each other. Extend freely.
#[derive(Clone, Debug)]
pub enum Payload {
    /// "Please do this for me."
    Task(String),
    /// "Here is the result you asked for."
    Reply(String),
    /// Fire-and-forget note.
    Note(String),
    /// Cooperative shutdown signal.
    Shutdown,
    /// Escape hatch for structured payloads.
    Custom(Value),
}

pub type Inbox = mpsc::UnboundedReceiver<Envelope>;
type Outbox = mpsc::UnboundedSender<Envelope>;

/// Cloneable handle used by harnesses to send messages. Built by the swarm once
/// all harnesses are registered, so the routing table is complete — or, for a
/// dynamically-spawned principal outside `Swarm`, extended at runtime via
/// [`Bus::insert_route`].
#[derive(Clone)]
pub struct Bus {
    routes: Arc<Mutex<HashMap<HarnessId, Outbox>>>,
}

impl Bus {
    pub fn new(routes: HashMap<HarnessId, Outbox>) -> Self {
        Self { routes: Arc::new(Mutex::new(routes)) }
    }

    /// Register (or replace) a route at runtime. Every clone of this `Bus`
    /// shares the same table (it's an `Arc` underneath), so a route added
    /// here is immediately reachable from every principal already holding a
    /// clone — in particular, the spawner that just admitted the child whose
    /// inbox this is.
    pub fn insert_route(&self, id: HarnessId, tx: Outbox) {
        self.routes.lock().expect("bus routing table poisoned").insert(id, tx);
    }

    /// Drop a route, e.g. once a dynamically-spawned principal has torn down
    /// its inbox — so a later stray send fails loudly (`no route to harness`)
    /// instead of silently reaching a channel nobody is receiving on anymore.
    pub fn remove_route(&self, id: &HarnessId) {
        self.routes.lock().expect("bus routing table poisoned").remove(id);
    }

    /// Deliver an envelope according to its `to` field.
    pub fn dispatch(&self, env: Envelope) -> Result<()> {
        match env.to.clone() {
            Recipient::Harness(id) => self.send_to(&id, env),
            Recipient::Broadcast => {
                let routes = self.routes.lock().expect("bus routing table poisoned");
                for (id, tx) in routes.iter() {
                    // Ignore individual closed receivers during broadcast.
                    let mut e = env.clone();
                    e.to = Recipient::Harness(id.clone());
                    let _ = tx.send(e);
                }
                Ok(())
            }
        }
    }

    fn send_to(&self, id: &HarnessId, env: Envelope) -> Result<()> {
        let routes = self.routes.lock().expect("bus routing table poisoned");
        let tx = routes.get(id).ok_or_else(|| anyhow!("no route to harness '{id}'"))?;
        tx.send(env).map_err(|_| anyhow!("harness '{id}' inbox is closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(ids: &[&str]) -> (Bus, HashMap<HarnessId, Inbox>) {
        let mut routes = HashMap::new();
        let mut inboxes = HashMap::new();
        for id in ids {
            let (tx, rx) = mpsc::unbounded_channel();
            let hid = HarnessId::new(*id);
            routes.insert(hid.clone(), tx);
            inboxes.insert(hid, rx);
        }
        (Bus::new(routes), inboxes)
    }

    #[tokio::test]
    async fn direct_dispatch_reaches_target() {
        let (bus, mut inboxes) = wire(&["a", "b"]);
        bus.dispatch(Envelope {
            from: HarnessId::new("a"),
            to: Recipient::Harness(HarnessId::new("b")),
            payload: Payload::Task("hi".into()),
        })
        .unwrap();
        let got = inboxes.get_mut(&HarnessId::new("b")).unwrap().recv().await.unwrap();
        assert_eq!(got.from, HarnessId::new("a"));
        matches!(got.payload, Payload::Task(t) if t == "hi");
    }

    #[tokio::test]
    async fn broadcast_reaches_everyone() {
        let (bus, mut inboxes) = wire(&["a", "b", "c"]);
        bus.dispatch(Envelope {
            from: HarnessId::new("a"),
            to: Recipient::Broadcast,
            payload: Payload::Shutdown,
        })
        .unwrap();
        for id in ["a", "b", "c"] {
            let got = inboxes.get_mut(&HarnessId::new(id)).unwrap().recv().await.unwrap();
            assert!(matches!(got.payload, Payload::Shutdown));
        }
    }

    #[test]
    fn dispatch_to_unknown_is_error() {
        let (bus, _inboxes) = wire(&["a"]);
        let err = bus.dispatch(Envelope {
            from: HarnessId::new("a"),
            to: Recipient::Harness(HarnessId::new("ghost")),
            payload: Payload::Note("x".into()),
        });
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn insert_route_makes_a_dynamically_spawned_peer_reachable() {
        let (bus, _inboxes) = wire(&["a"]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        bus.insert_route(HarnessId::new("child"), tx);

        bus.dispatch(Envelope {
            from: HarnessId::new("a"),
            to: Recipient::Harness(HarnessId::new("child")),
            payload: Payload::Task("hi".into()),
        })
        .unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.from, HarnessId::new("a"));
    }

    #[test]
    fn remove_route_makes_further_sends_fail_loudly() {
        let (bus, _inboxes) = wire(&["a"]);
        bus.remove_route(&HarnessId::new("a"));
        let err = bus.dispatch(Envelope {
            from: HarnessId::new("a"),
            to: Recipient::Harness(HarnessId::new("a")),
            payload: Payload::Note("x".into()),
        });
        assert!(err.is_err());
    }
}
