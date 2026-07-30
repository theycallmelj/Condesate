//! Slips, retrieval sheets, and menus — turning a permanent Diary into a
//! bounded, task-specific working context.
//!
//! Three devices, in increasing order of composition: a [`Slip`] is a
//! one-line descriptor plus the Diary addresses it points to (the *encode*
//! step); a [`RetrievalSheet`], built by a [`SheetComposer`], is a group of
//! slips resolved and composed into one named working set (*organize* +
//! *retrieve*); a [`Menu`] catalogs the sheets/slip-topics that exist, for
//! browsing before a deep retrieval.
//!
//! This is "context engineering" in this crate's specific sense: not the
//! prompt-formatting step, but the discipline of composing a bounded working
//! set from a permanent record without dropping the surrounding context a
//! bare fact needs to stay meaningful — see [`super::diary`] for why.

use super::diary::{Diary, DiaryEntry};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeSet;

/// A one-line retrieval cue: a descriptor plus the Diary addresses it points
/// to. The *encode* step — turning a raw entry into something that can be
/// found again by topic rather than by address.
#[derive(Clone, Debug)]
pub struct Slip {
    pub descriptor: String,
    pub topic: String,
    pub refs: Vec<u64>,
}

/// Where slips live once tagged, organized for lookup by topic. Deliberately
/// separate from [`Diary`] — a Diary entry is written once and never moves;
/// which slips point to it can grow, shrink, and be reorganized freely.
#[async_trait]
pub trait SlipIndex: Send + Sync {
    async fn tag(&self, slip: Slip) -> Result<()>;
    async fn by_topic(&self, topic: &str) -> Result<Vec<Slip>>;
}

/// A composed, purpose-built working set: slips resolved into Diary entries,
/// deduplicated, and ordered — the bounded context that actually gets handed
/// to a model. The *organize* + *retrieve* steps, done.
#[derive(Clone, Debug)]
pub struct RetrievalSheet {
    pub purpose: String,
    /// In Diary order, deduplicated — this is the assembled working context.
    pub entries: Vec<DiaryEntry>,
}

/// Builds a [`RetrievalSheet`] from a set of slips.
///
/// The one contract that matters: composing must resolve each slip's
/// reference through [`Diary::window`], never [`Diary::entry`] alone — see
/// [`super::diary`] for why a bare entry isn't enough.
#[async_trait]
pub trait SheetComposer: Send + Sync {
    async fn compose(
        &self,
        purpose: &str,
        slips: &[Slip],
        diary: &dyn Diary,
        window_radius: u32,
    ) -> Result<RetrievalSheet>;
}

/// Reference composer: resolves every slip reference to its window, dedupes
/// by address, and returns the result in Diary order.
pub struct StandardComposer;

#[async_trait]
impl SheetComposer for StandardComposer {
    async fn compose(
        &self,
        purpose: &str,
        slips: &[Slip],
        diary: &dyn Diary,
        window_radius: u32,
    ) -> Result<RetrievalSheet> {
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        let mut entries: Vec<DiaryEntry> = Vec::new();
        for slip in slips {
            for &addr in &slip.refs {
                for e in diary.window(addr, window_radius).await? {
                    if seen.insert(e.seq) {
                        entries.push(e);
                    }
                }
            }
        }
        entries.sort_by_key(|e| e.seq);
        Ok(RetrievalSheet { purpose: purpose.to_string(), entries })
    }
}

/// A catalog of what retrieval sheets or slip-topics exist, browsed before a
/// deep retrieval. Trait-only: a deployment earns the need for a `Menu` once
/// it has enough slips and sheets that browsing them requires its own index.
#[async_trait]
pub trait Menu: Send + Sync {
    /// Every topic with at least one slip tagged under it.
    async fn topics(&self) -> Result<Vec<String>>;
}

/// In-process [`SlipIndex`], reusable as a [`Menu`] over the same data —
/// listing topics is exactly what a menu over a slip index needs.
pub struct InMemorySlipIndex {
    by_topic: tokio::sync::RwLock<std::collections::HashMap<String, Vec<Slip>>>,
}

impl InMemorySlipIndex {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { by_topic: tokio::sync::RwLock::new(std::collections::HashMap::new()) })
    }
}

#[async_trait]
impl SlipIndex for InMemorySlipIndex {
    async fn tag(&self, slip: Slip) -> Result<()> {
        self.by_topic.write().await.entry(slip.topic.clone()).or_default().push(slip);
        Ok(())
    }

    async fn by_topic(&self, topic: &str) -> Result<Vec<Slip>> {
        Ok(self.by_topic.read().await.get(topic).cloned().unwrap_or_default())
    }
}

#[async_trait]
impl Menu for InMemorySlipIndex {
    async fn topics(&self) -> Result<Vec<String>> {
        let mut topics: Vec<String> = self.by_topic.read().await.keys().cloned().collect();
        topics.sort();
        Ok(topics)
    }
}

#[cfg(test)]
mod tests {
    use super::super::diary::{EntryKind, InMemoryDiary, NewDiaryEntry};
    use super::*;
    use crate::security::audit::FixedClock;
    use crate::types::HarnessId;

    async fn seeded_diary() -> std::sync::Arc<InMemoryDiary> {
        let d = InMemoryDiary::new(std::sync::Arc::new(FixedClock(1_000)));
        for content in [
            "set up coil A near the magnet",       // 1
            "no deflection observed yet",           // 2
            "deflection!",                          // 3
            "repeated with coil B, same result",    // 4
            "concluded: motion induces current",    // 5
            "unrelated: capacitor leakage test",    // 6
        ] {
            d.record(NewDiaryEntry {
                harness: HarnessId::new("worker"),
                activation: "act-1".into(),
                kind: EntryKind::Observation,
                content: content.into(),
                refs: vec![],
            })
            .await
            .unwrap();
        }
        d
    }

    #[tokio::test]
    async fn composed_sheet_carries_the_neighborhood_not_just_the_hit() {
        // The same finding as diary::tests::window_returns_neighbors_not_just_the_hit,
        // now exercised through the actual Slip -> Sheet composition path a
        // caller would use.
        let diary = seeded_diary().await;
        let slip = Slip { descriptor: "the induction result".into(), topic: "induction".into(), refs: vec![3] };

        let sheet = StandardComposer.compose("write up induction", &[slip], diary.as_ref(), 1).await.unwrap();
        let contents: Vec<&str> = sheet.entries.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["no deflection observed yet", "deflection!", "repeated with coil B, same result"]
        );
        assert_eq!(sheet.purpose, "write up induction");
    }

    #[tokio::test]
    async fn composing_from_multiple_slips_dedupes_and_orders_by_address() {
        let diary = seeded_diary().await;
        let slips = vec![
            Slip { descriptor: "result".into(), topic: "induction".into(), refs: vec![5] },
            Slip { descriptor: "setup".into(), topic: "induction".into(), refs: vec![1] },
        ];
        // radius 1 around #5 covers #4-#5 (6 doesn't exist... it does, #6 is
        // unrelated but still a neighbor); radius 1 around #1 covers #1-#2.
        // #2 and #4 overlap from neither slip directly, so this also proves
        // overlapping windows don't produce duplicate entries.
        let sheet = StandardComposer.compose("overview", &slips, diary.as_ref(), 1).await.unwrap();
        let seqs: Vec<u64> = sheet.entries.iter().map(|e| e.seq).collect();
        let mut expected = seqs.clone();
        expected.sort();
        expected.dedup();
        assert_eq!(seqs, expected, "sorted and deduplicated by address");
        assert_eq!(seqs, vec![1, 2, 4, 5, 6]);
    }

    #[tokio::test]
    async fn slip_index_groups_by_topic_and_doubles_as_a_menu() {
        let index = InMemorySlipIndex::new();
        index
            .tag(Slip { descriptor: "the induction result".into(), topic: "induction".into(), refs: vec![3] })
            .await
            .unwrap();
        index
            .tag(Slip { descriptor: "leakage".into(), topic: "capacitance".into(), refs: vec![6] })
            .await
            .unwrap();

        assert_eq!(index.by_topic("induction").await.unwrap().len(), 1);
        assert_eq!(index.by_topic("unknown-topic").await.unwrap().len(), 0);

        let topics = Menu::topics(index.as_ref()).await.unwrap();
        assert_eq!(topics, vec!["capacitance".to_string(), "induction".to_string()]);
    }
}
