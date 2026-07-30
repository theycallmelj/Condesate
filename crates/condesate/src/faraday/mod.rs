//! Memory and context engineering for the swarm, named for Michael Faraday —
//! whose notebooks are the closest thing available to a field-tested design
//! for keeping a permanent, honest record and still pulling the right bounded
//! slice of it back out on demand. Full background: `docs/faraday-context-engineering.md`.
//!
//! | Submodule | Holds |
//! |---|---|
//! | [`diary`] | permanent, sequentially-addressed record — never reordered, never renumbered |
//! | [`ideabook`] | speculation in flight: revisable, entries can be struck out |
//! | [`index::Slip`] | a one-line descriptor plus a pointer back into the Diary (*encode*) |
//! | [`index::RetrievalSheet`] / [`index::SheetComposer`] | slips resolved into one bounded, task-specific working set (*organize* + *retrieve*) |
//! | [`index::Menu`] | a catalog of what retrieval sheets exist |
//!
//! ## Where this sits in the loop
//!
//! In the "three levels of the agent loop" framing — Level 1: a bare loop, no
//! memory; Level 2: a loop with in-process lifecycle/memory; Level 3: memory
//! and state external to the loop — this crate's [`crate::agent::loops`] is
//! Level 1 on its own. [`ideabook`] is Level 2: memory that lives inside one
//! activation, revisable turn to turn. [`diary`] and [`index`] are Level 3:
//! memory that outlives any one activation or harness, persisted the same way
//! [`crate::swarm::storage::Storage`] is, and (in a real deployment) subject
//! to the same governance labels as [`crate::cache`].
//!
//! ## The design argument
//!
//! [`diary::Diary::window`] and [`index::SheetComposer`] both resolve a
//! reference into a *window* of surrounding entries, never a single bare one.
//! A retrieved fact without its neighborhood is not the same information the
//! record actually held. Context engineering, in this module's specific
//! sense, is the discipline of never breaking that link — retrieving enough
//! surrounding record to keep a fact meaningful, while keeping the total
//! window bounded enough to fit a prompt.
//!
//! ## Status
//!
//! [`diary::InMemoryDiary`], [`ideabook::InMemoryIdeaBook`], [`index::Slip`],
//! and [`index::StandardComposer`] are real and tested, including a test that
//! directly proves the window-vs-bare-entry claim above. [`index::Menu`] is
//! trait-only — a deployment earns a concrete one once it has enough slips
//! and sheets that browsing them needs its own index. Nothing here is yet
//! wired into [`crate::security::kernel::GuardedServices`] or
//! [`crate::swarm::harness::StandardHarness`]; doing so is the natural next
//! step, the same way `cache` is designed to be wired in.

pub mod diary;
pub mod ideabook;
pub mod index;

pub use diary::{Diary, DiaryEntry, EntryKind, InMemoryDiary, NewDiaryEntry};
pub use ideabook::{IdeaBook, InMemoryIdeaBook, Speculation};
pub use index::{Menu, RetrievalSheet, SheetComposer, Slip, SlipIndex, StandardComposer};
