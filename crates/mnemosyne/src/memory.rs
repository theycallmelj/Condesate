//! Wires `condesate::faraday` — Diary, IdeaBook, SlipIndex — into real tools.
//!
//! This is the first live consumer of `faraday`: the module's own docs note
//! nothing in it was yet wired into `GuardedServices`. The wiring chosen here
//! is tool-level, not a new `Resource`/`Action` variant in the core policy
//! engine — every tool below still goes through the exact same
//! `Action::Invoke` / `Resource::Tool` gate as any other tool in this
//! workspace (see `condesate::agent::mcp`'s module docs for why a second,
//! bespoke gate is never the right move). A `Resource::Memory`-style
//! first-class integration is the documented "natural next step" for
//! `faraday` itself, not something this one demo app should invent on its
//! own.
//!
//! Two roles split the API cleanly:
//! - **mnemosyne** (the main agent) only ever *reads*: [`ListMemoryTopics`],
//!   [`RecallMemory`].
//! - **morpheus** (the background curator) only ever *writes*:
//!   [`RecordDiaryEntry`], [`TagSlip`], [`JotIdea`], [`ReviseIdea`],
//!   [`StrikeIdea`], plus one read tool of its own ([`ListLiveIdeas`]) so a
//!   later curation pass can revise or strike a speculation from an earlier
//!   one instead of only ever jotting new ones.
//!
//! Nothing here decides *when* curation happens — see `main.rs`'s
//! turn-counting loop for the "every N messages" policy the user asked for.

use anyhow::{anyhow, Result};
use condesate::{
    Clock, Diary, DiaryEntry, EntryKind, GuardedServices, HarnessId, IdeaBook, InMemoryDiary,
    InMemoryIdeaBook, InMemorySlipIndex, Menu, NewDiaryEntry, SheetComposer, Slip, SlipIndex,
    StandardComposer, Tool, ToolSpec,
};
use std::sync::Arc;

/// How many neighboring Diary entries [`RecallMemory`] pulls in on each side
/// of a hit — see `faraday::diary`'s module docs for why a bare entry, with
/// no neighborhood, isn't the same information the record actually held.
/// Not exposed as an env var: unlike the curation cadence, this doesn't need
/// runtime tuning to be useful, and one more knob here would be one more
/// thing to explain instead of just doing the sensible thing.
const RECALL_WINDOW_RADIUS: u32 = 2;

/// The shared Faraday store, held once and reached by both agents' tools —
/// the same shape as `leader-search`'s shared `Arc<dyn Storage>`, just for
/// `faraday` instead of `swarm::storage`.
#[derive(Clone)]
pub struct MemoryBank {
    pub diary: Arc<InMemoryDiary>,
    pub ideas: Arc<InMemoryIdeaBook>,
    pub slips: Arc<InMemorySlipIndex>,
}

impl MemoryBank {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { diary: InMemoryDiary::new(clock), ideas: InMemoryIdeaBook::new(), slips: InMemorySlipIndex::new() }
    }
}

fn parse_kind(s: &str) -> Result<EntryKind> {
    match s.to_lowercase().as_str() {
        "observation" => Ok(EntryKind::Observation),
        "decision" => Ok(EntryKind::Decision),
        "tool_result" | "toolresult" => Ok(EntryKind::ToolResult),
        "retrieved" => Ok(EntryKind::Retrieved),
        "reflection" => Ok(EntryKind::Reflection),
        other => Err(anyhow!(
            "unknown kind '{other}' — must be one of: observation, decision, tool_result, retrieved, reflection"
        )),
    }
}

fn format_entries(entries: &[DiaryEntry]) -> String {
    entries
        .iter()
        .map(|e| format!("#{} [{:?}] {}", e.seq, e.kind, e.content))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Read tools — shared by both agents
// ---------------------------------------------------------------------------

/// Browse what topics exist before a deep retrieval — the `Menu` half of
/// `faraday::index`.
pub struct ListMemoryTopics {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for ListMemoryTopics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_memory_topics".into(),
            description: "List every topic currently tracked in long-term memory. No arguments."
                .into(),
        }
    }

    async fn call(&self, _args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let topics = Menu::topics(self.bank.slips.as_ref()).await?;
        if topics.is_empty() {
            return Ok("no topics recorded yet".into());
        }
        Ok(topics.join(", "))
    }
}

/// Resolve a topic into a composed, windowed retrieval sheet — the actual
/// "supplement its memory" path for mnemosyne. Falls back to listing known
/// topics on a miss rather than just erroring, so the model can retry with a
/// real one instead of giving up.
pub struct RecallMemory {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for RecallMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall_memory".into(),
            description: "Retrieve what long-term memory holds on a topic, with surrounding \
                context, not just a bare fact. Args: {\"topic\": <string>}. If the topic isn't \
                known, returns the list of topics that are."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let topic = args.get("topic").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'topic'"))?;
        let slips = self.bank.slips.by_topic(topic).await?;
        if slips.is_empty() {
            let known = Menu::topics(self.bank.slips.as_ref()).await?;
            return Ok(if known.is_empty() {
                "nothing recorded on that topic — memory is empty so far".to_string()
            } else {
                format!("nothing recorded under '{topic}' — known topics: {}", known.join(", "))
            });
        }
        let sheet = StandardComposer
            .compose(topic, &slips, self.bank.diary.as_ref() as &dyn Diary, RECALL_WINDOW_RADIUS)
            .await?;
        Ok(format_entries(&sheet.entries))
    }
}

// ---------------------------------------------------------------------------
// Write tools — morpheus only
// ---------------------------------------------------------------------------

/// Commit a permanent, addressed record — the *capture* step.
pub struct RecordDiaryEntry {
    pub bank: MemoryBank,
    pub harness: HarnessId,
}

#[async_trait::async_trait]
impl Tool for RecordDiaryEntry {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "record_diary_entry".into(),
            description: "Permanently record one fact from the conversation. Args: \
                {\"kind\": \"observation\"|\"decision\"|\"tool_result\"|\"retrieved\"|\"reflection\", \
                \"content\": <string>, \"refs\": [<uint>, ...] (optional, other diary addresses \
                this relates to)}. Returns the permanent address (seq) — use it in tag_slip."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, services: &GuardedServices) -> Result<String> {
        let kind = parse_kind(args.get("kind").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'kind'"))?)?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'content'"))?;
        let refs = args
            .get("refs")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_default();
        let seq = self
            .bank
            .diary
            .record(NewDiaryEntry {
                harness: self.harness.clone(),
                activation: services.principal().uid.to_string(),
                kind,
                content: content.to_string(),
                refs,
            })
            .await?;
        Ok(format!("recorded at address {seq}"))
    }
}

/// Tag a retrieval cue against one or more Diary addresses — the *encode*
/// step. What [`RecallMemory`] actually searches by topic.
pub struct TagSlip {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for TagSlip {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "tag_slip".into(),
            description: "Make a diary entry findable by topic. Args: {\"descriptor\": <string, \
                one-line summary>, \"topic\": <string>, \"refs\": [<uint>, ...] (diary addresses \
                from record_diary_entry)}."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let descriptor = args.get("descriptor").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'descriptor'"))?;
        let topic = args.get("topic").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'topic'"))?;
        let refs: Vec<u64> = args
            .get("refs")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_default();
        if refs.is_empty() {
            return Err(anyhow!("refs must name at least one diary address"));
        }
        self.bank.slips.tag(Slip { descriptor: descriptor.to_string(), topic: topic.to_string(), refs }).await?;
        Ok(format!("tagged under '{topic}'"))
    }
}

/// Read the live (unstruck) speculations under a topic — lets a later
/// curation pass revise or strike a speculation from an earlier one, instead
/// of only ever jotting new ones with no memory of what it already thought.
pub struct ListLiveIdeas {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for ListLiveIdeas {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_live_ideas".into(),
            description: "List still-live (not struck or superseded) speculations under a \
                topic, with their ids. Args: {\"topic\": <string>}."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let topic = args.get("topic").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'topic'"))?;
        let live = self.bank.ideas.live(topic).await?;
        if live.is_empty() {
            return Ok(format!("no live ideas under '{topic}'"));
        }
        Ok(live.iter().map(|s| format!("#{} {}", s.id, s.content)).collect::<Vec<_>>().join("\n"))
    }
}

/// Jot a new, unstruck speculation — in-flight thinking, not yet promoted to
/// the Diary.
pub struct JotIdea {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for JotIdea {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "jot_idea".into(),
            description: "Note a speculative, not-yet-settled idea. Args: {\"topic\": <string>, \
                \"content\": <string>}. Returns its id."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let topic = args.get("topic").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'topic'"))?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'content'"))?;
        let id = self.bank.ideas.jot(topic, content).await?;
        Ok(format!("jotted as id {id}"))
    }
}

/// Strike an existing speculation and jot its replacement, linked back to it
/// — a revision chain, not a silent overwrite.
pub struct ReviseIdea {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for ReviseIdea {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "revise_idea".into(),
            description: "Replace an existing speculation with an updated one, keeping the old \
                one visible in history. Args: {\"id\": <uint>, \"content\": <string>}. Returns \
                the new id."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| anyhow!("missing uint arg 'id'"))?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'content'"))?;
        let new_id = self.bank.ideas.revise(id, content).await?;
        Ok(format!("revised into id {new_id}"))
    }
}

/// Strike a speculation that didn't pan out, without replacing it.
pub struct StrikeIdea {
    pub bank: MemoryBank,
}

#[async_trait::async_trait]
impl Tool for StrikeIdea {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "strike_idea".into(),
            description: "Mark a speculation as no longer live — it turned out to be wrong or \
                irrelevant. Args: {\"id\": <uint>}."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, _services: &GuardedServices) -> Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| anyhow!("missing uint arg 'id'"))?;
        self.bank.ideas.strike(id).await?;
        Ok(format!("struck id {id}"))
    }
}
