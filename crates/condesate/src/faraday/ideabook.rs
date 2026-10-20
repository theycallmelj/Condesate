//! The Idea Book: mutable, revisable working memory for speculation in flight.
//!
//! Where the Diary is permanent — an address, once assigned, never changes
//! what it points to — an Idea Book entry can be revised or struck as a
//! hypothesis firms up or turns out wrong. This is Level 2 memory in the
//! "agent loop, three levels" framing (see [`super`]): state that lives
//! *inside* an activation, not yet worth committing to the Diary. A
//! `ReActLoop` step that reconsiders its own plan should be revising an
//! `IdeaBook` entry, not silently overwriting a fact in the transcript.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

/// One speculative entry. `struck` and `supersedes` exist to keep a
/// superseded entry visible rather than erase it — the history of how a
/// thought changed is worth keeping.
#[derive(Clone, Debug)]
pub struct Speculation {
    pub id: u64,
    pub topic: String,
    pub content: String,
    /// True once struck — no longer live, but still present in the record.
    pub struck: bool,
    /// The entry this one revises, if any, forming a chain of revisions
    /// rather than a single mutable slot.
    pub supersedes: Option<u64>,
}

/// Mutable, topic-organized scratch space. Unlike [`super::diary::Diary`],
/// nothing here promises permanence — that promise belongs to the Diary, and
/// an `IdeaBook` entry graduates into one deliberately, not by default.
#[async_trait]
pub trait IdeaBook: Send + Sync {
    /// Add a new, unstruck speculation under `topic`.
    async fn jot(&self, topic: &str, content: &str) -> Result<u64>;

    /// Strike `id` and jot a replacement under the same topic, linked back to
    /// it via `supersedes`. Returns the new entry's id.
    async fn revise(&self, id: u64, content: &str) -> Result<u64>;

    /// Strike an entry without replacing it — the idea didn't pan out.
    async fn strike(&self, id: u64) -> Result<()>;

    /// Every entry ever jotted under `topic`, in id order, struck entries
    /// included — the full history, not just the live tip.
    async fn by_topic(&self, topic: &str) -> Result<Vec<Speculation>>;

    /// Only the live (unstruck) entries under `topic`, latest revision only —
    /// what a caller usually wants when assembling working context.
    async fn live(&self, topic: &str) -> Result<Vec<Speculation>> {
        let all = self.by_topic(topic).await?;
        let superseded: std::collections::HashSet<u64> =
            all.iter().filter_map(|s| s.supersedes).collect();
        Ok(all.into_iter().filter(|s| !s.struck && !superseded.contains(&s.id)).collect())
    }

    /// Every topic with at least one speculation jotted under it — the
    /// `IdeaBook` counterpart of `super::index::Menu::topics`, so a caller
    /// (a diagnostic dump, a browsing UI) can enumerate everything without
    /// already knowing what topic names exist.
    async fn topics(&self) -> Result<Vec<String>>;
}

/// In-process Idea Book.
pub struct InMemoryIdeaBook {
    next_id: std::sync::atomic::AtomicU64,
    entries: RwLock<HashMap<u64, Speculation>>,
    /// Insertion order per topic, so `by_topic` reads in the order things
    /// were actually jotted rather than hash-map order.
    order: RwLock<HashMap<String, Vec<u64>>>,
}

impl InMemoryIdeaBook {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            next_id: std::sync::atomic::AtomicU64::new(1),
            entries: RwLock::new(HashMap::new()),
            order: RwLock::new(HashMap::new()),
        })
    }

    async fn insert(&self, topic: &str, content: &str, supersedes: Option<u64>) -> u64 {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entries.write().await.insert(
            id,
            Speculation { id, topic: topic.to_string(), content: content.to_string(), struck: false, supersedes },
        );
        self.order.write().await.entry(topic.to_string()).or_default().push(id);
        id
    }
}

#[async_trait]
impl IdeaBook for InMemoryIdeaBook {
    async fn jot(&self, topic: &str, content: &str) -> Result<u64> {
        Ok(self.insert(topic, content, None).await)
    }

    async fn revise(&self, id: u64, content: &str) -> Result<u64> {
        let topic = {
            let entries = self.entries.read().await;
            entries.get(&id).ok_or_else(|| anyhow!("no such speculation: {id}"))?.topic.clone()
        };
        self.strike(id).await?;
        Ok(self.insert(&topic, content, Some(id)).await)
    }

    async fn strike(&self, id: u64) -> Result<()> {
        let mut entries = self.entries.write().await;
        let s = entries.get_mut(&id).ok_or_else(|| anyhow!("no such speculation: {id}"))?;
        s.struck = true;
        Ok(())
    }

    async fn by_topic(&self, topic: &str) -> Result<Vec<Speculation>> {
        let order = self.order.read().await;
        let entries = self.entries.read().await;
        Ok(order
            .get(topic)
            .into_iter()
            .flatten()
            .filter_map(|id| entries.get(id).cloned())
            .collect())
    }

    async fn topics(&self) -> Result<Vec<String>> {
        let mut topics: Vec<String> = self.order.read().await.keys().cloned().collect();
        topics.sort();
        Ok(topics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revision_strikes_the_old_entry_but_keeps_it_visible() {
        let book = InMemoryIdeaBook::new();
        let first = book.jot("induction", "maybe it's the wire moving").await.unwrap();
        let second = book.revise(first, "no — it's the flux changing").await.unwrap();

        let all = book.by_topic("induction").await.unwrap();
        assert_eq!(all.len(), 2, "the struck entry stays in the record");
        assert!(all.iter().find(|s| s.id == first).unwrap().struck);
        assert!(!all.iter().find(|s| s.id == second).unwrap().struck);
        assert_eq!(all.iter().find(|s| s.id == second).unwrap().supersedes, Some(first));
    }

    #[tokio::test]
    async fn live_returns_only_the_current_tip_of_each_chain() {
        let book = InMemoryIdeaBook::new();
        let a = book.jot("plan", "try approach A").await.unwrap();
        let b = book.revise(a, "approach A failed, try B").await.unwrap();
        let _c = book.revise(b, "B works, proceed").await.unwrap();
        book.jot("plan", "separately: also check edge cases").await.unwrap();

        let live = book.live("plan").await.unwrap();
        let contents: Vec<&str> = live.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(contents, vec!["B works, proceed", "separately: also check edge cases"]);
    }

    #[tokio::test]
    async fn a_bare_strike_removes_an_idea_without_replacing_it() {
        let book = InMemoryIdeaBook::new();
        let dead_end = book.jot("plan", "try approach C").await.unwrap();
        book.strike(dead_end).await.unwrap();

        assert!(book.live("plan").await.unwrap().is_empty());
        assert_eq!(book.by_topic("plan").await.unwrap().len(), 1, "still in the history");
    }

    #[tokio::test]
    async fn topics_stay_independent() {
        let book = InMemoryIdeaBook::new();
        book.jot("induction", "idea A").await.unwrap();
        book.jot("capacitance", "idea B").await.unwrap();
        assert_eq!(book.by_topic("induction").await.unwrap().len(), 1);
        assert_eq!(book.by_topic("capacitance").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn topics_lists_every_topic_jotted_under_exactly_once() {
        let book = InMemoryIdeaBook::new();
        book.jot("induction", "idea A").await.unwrap();
        book.jot("induction", "idea B").await.unwrap(); // same topic again
        book.jot("capacitance", "idea C").await.unwrap();
        assert_eq!(book.topics().await.unwrap(), vec!["capacitance".to_string(), "induction".to_string()]);
    }
}
