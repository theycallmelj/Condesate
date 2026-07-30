//! The OS layer: `Harness -> Swarm`, plus IPC and shared services.
//!
//! | Module | Holds |
//! |---|---|
//! | [`harness`] | `Harness` — one running "process"; `StandardHarness` |
//! | [`core`] | `Swarm` — process table, scheduling, admission |
//! | [`bus`] | `Bus` / `Payload` — inter-harness messaging |
//! | [`service`] | `ServiceHandle` — the raw syscall surface a `Kernel` guards |
//! | [`storage`] | `Storage` — shared key/value memory |

pub mod bus;
pub mod core;
pub mod harness;
pub mod service;
pub mod storage;

pub use bus::{Bus, Envelope, Inbox, Payload, Recipient};
pub use core::Swarm;
pub use harness::{Harness, StandardHarness};
pub use service::ServiceHandle;
pub use storage::{InMemoryStorage, Storage};
