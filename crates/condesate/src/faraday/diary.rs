//! The Diary: a permanent, sequentially-addressed episodic record.
//!
//! [`DiaryEntry::seq`] is assigned once, in order, and never reassigned or
//! reused — a stable address any index into the Diary (this crate's
//! [`super::index::Slip`] included) can hold indefinitely and trust still
//! resolves to the same content.

use crate::security::audit::Clock;
use crate::types::HarnessId;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::RwLock;

/// What kind of thing a Diary entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Something noticed — an input, a fact, a signal from the environment.
    Observation,
    /// A choice made and (ideally) the reason for it.
    Decision,
    /// The result of a tool call worth remembering past this activation.
    ToolResult,
    /// Something pulled back out of memory and re-surfaced — recording a
    /// retrieval keeps the Diary honest about what informed a later decision.
    Retrieved,
    /// A reflection on the record itself, committed because it turned out to
    /// matter permanently (compare [`super::ideabook::IdeaBook`], which holds
    /// this kind of content while it's still in flight).
    Reflection,
}

/// A Diary entry not yet assigned an address. What a caller hands to
/// [`Diary::record`].
#[derive(Clone, Debug)]
pub struct NewDiaryEntry {
    pub harness: HarnessId,
    /// Correlation id, matching [`crate::security::kernel::GuardedServices::begin_activation`].
    pub activation: String,
    pub kind: EntryKind,
    pub content: String,
    /// Other Diary addresses this entry relates to.
    pub refs: Vec<u64>,
}

/// A committed Diary entry: everything in [`NewDiaryEntry`] plus its
/// permanent address and timestamp.
#[derive(Clone, Debug)]
pub struct DiaryEntry {
    /// Permanent, monotonically-assigned address. Never reused.
    pub seq: u64,
    pub at_ms: u64,
    pub harness: HarnessId,
    pub activation: String,
    pub kind: EntryKind,
    pub content: String,
    pub refs: Vec<u64>,
}

/// An append-only episodic log with permanent addresses and context-preserving
/// retrieval.
///
/// [`Diary::entry`] exists for completeness, but [`Diary::window`] is what a
/// real caller should reach for — see [`super`] for why.
#[async_trait]
pub trait Diary: Send + Sync {
    /// Append an entry and return its permanent address.
    async fn record(&self, entry: NewDiaryEntry) -> Result<u64>;

    /// The entry at exactly this address, if it exists.
    async fn entry(&self, seq: u64) -> Result<Option<DiaryEntry>>;

    /// The entry at `seq`, plus up to `radius` entries immediately before and
    /// after it, in Diary order. Missing neighbors (near the start of the log)
    /// are simply omitted, not padded. Any caller assembling context for a
    /// model should call this, not `entry`.
    async fn window(&self, seq: u64, radius: u32) -> Result<Vec<DiaryEntry>>;

    /// How many entries have been recorded. The next address `record` will
    /// assign is always `len() + 1`.
    async fn len(&self) -> Result<u64>;

    async fn is_empty(&self) -> Result<bool> {
        Ok(self.len().await? == 0)
    }
}

/// In-process Diary. Good enough to demonstrate — and test — the shape;
/// swap for anything durable (sqlite, an append-only file, object storage)
/// by writing a new impl of [`Diary`], the same way [`crate::swarm::storage::InMemoryStorage`]
/// stands in for a real [`crate::swarm::storage::Storage`] backend.
pub struct InMemoryDiary {
    clock: std::sync::Arc<dyn Clock>,
    entries: RwLock<Vec<DiaryEntry>>,
}

impl InMemoryDiary {
    pub fn new(clock: std::sync::Arc<dyn Clock>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { clock, entries: RwLock::new(Vec::new()) })
    }
}

#[async_trait]
impl Diary for InMemoryDiary {
    async fn record(&self, entry: NewDiaryEntry) -> Result<u64> {
        let mut entries = self.entries.write().await;
        let seq = entries.len() as u64 + 1;
        entries.push(DiaryEntry {
            seq,
            at_ms: self.clock.now_ms(),
            harness: entry.harness,
            activation: entry.activation,
            kind: entry.kind,
            content: entry.content,
            refs: entry.refs,
        });
        Ok(seq)
    }

    async fn entry(&self, seq: u64) -> Result<Option<DiaryEntry>> {
        if seq == 0 {
            return Ok(None);
        }
        Ok(self.entries.read().await.get((seq - 1) as usize).cloned())
    }

    async fn window(&self, seq: u64, radius: u32) -> Result<Vec<DiaryEntry>> {
        if seq == 0 {
            return Ok(Vec::new());
        }
        let entries = self.entries.read().await;
        let center = (seq - 1) as usize;
        if center >= entries.len() {
            return Ok(Vec::new());
        }
        let lo = center.saturating_sub(radius as usize);
        let hi = (center + radius as usize).min(entries.len().saturating_sub(1));
        Ok(entries[lo..=hi].to_vec())
    }

    async fn len(&self) -> Result<u64> {
        Ok(self.entries.read().await.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::audit::FixedClock;

    fn diary() -> std::sync::Arc<InMemoryDiary> {
        InMemoryDiary::new(std::sync::Arc::new(FixedClock(1_000)))
    }

    fn entry(content: &str) -> NewDiaryEntry {
        NewDiaryEntry {
            harness: HarnessId::new("worker"),
            activation: "act-1".into(),
            kind: EntryKind::Observation,
            content: content.into(),
            refs: vec![],
        }
    }

    #[tokio::test]
    async fn addresses_are_permanent_and_monotonic() {
        let d = diary();
        let a = d.record(entry("first")).await.unwrap();
        let b = d.record(entry("second")).await.unwrap();
        let c = d.record(entry("third")).await.unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(d.len().await.unwrap(), 3);

        // The address still resolves to the same content it was given,
        // regardless of what has been recorded since.
        assert_eq!(d.entry(a).await.unwrap().unwrap().content, "first");
    }

    #[tokio::test]
    async fn missing_address_is_none_not_an_error() {
        let d = diary();
        d.record(entry("only entry")).await.unwrap();
        assert!(d.entry(0).await.unwrap().is_none());
        assert!(d.entry(99).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn window_returns_neighbors_not_just_the_hit() {
        // Entry 3 alone is uninterpretable; radius 1 recovers the setup and
        // result around it.
        let d = diary();
        for content in [
            "set up coil A near the magnet",
            "no deflection observed yet",
            "deflection!",
            "repeated with coil B, same result",
            "concluded: motion of magnet induces current",
        ] {
            d.record(entry(content)).await.unwrap();
        }

        let bare = d.entry(3).await.unwrap().unwrap();
        assert_eq!(bare.content, "deflection!");

        let windowed = d.window(3, 1).await.unwrap();
        let contents: Vec<&str> = windowed.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "no deflection observed yet",
                "deflection!",
                "repeated with coil B, same result",
            ]
        );
    }

    #[tokio::test]
    async fn window_near_the_edges_omits_missing_neighbors_rather_than_padding() {
        let d = diary();
        for content in ["a", "b", "c"] {
            d.record(entry(content)).await.unwrap();
        }
        let start = d.window(1, 2).await.unwrap();
        assert_eq!(start.len(), 3, "clipped to the start, not padded with empties");

        let end = d.window(3, 5).await.unwrap();
        assert_eq!(end.len(), 3, "clipped to the end");
    }

    #[tokio::test]
    async fn window_of_an_unknown_address_is_empty() {
        let d = diary();
        d.record(entry("only entry")).await.unwrap();
        assert!(d.window(50, 2).await.unwrap().is_empty());
    }
}
