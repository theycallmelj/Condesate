# Faraday: memory and context engineering

Design notes for `crates/ai-swarm/src/faraday/` — a memory subsystem named for
Michael Faraday, built directly from the historical record of how he actually
organized his own notebooks.

## Why Faraday

Faraday left the largest documentary archive of any scientist in history:
roughly 30,000 experiments, plus idea books, indexes, and thousands of
hand-written retrieval slips, almost all organized by Faraday himself. He did
this because he distrusted his own memory and treated recording as part of
thinking, not paperwork after it. Two historical sources ground this module:

- Ryan D. Tweney, *"Faraday's notebooks: the active organization of creative
  science,"* Physics Education 26 (1991) — the primary source; describes the
  five kinds of record Faraday kept and reports a direct experiment on how
  retrieval either preserves or destroys meaning.
- Ryan D. Tweney, *"Faraday's discovery of induction: a cognitive approach"*
  (2015 reprint) — companion material on the cognitive role of the notebooks.

That record is a field-tested answer to the exact problem an agent harness
has: keep a permanent, honest account of what happened, and still be able to
pull the *right* bounded slice of it back out when a task needs it.

## Where this sits in the loop

Oracle's *"The Agent Loop Decoded: Three Levels Every Agent Engineer Must
Know"* names three levels of agent engineering:

- **Level 1** — the minimal loop: an LLM, tools, a response. No memory, no
  external state.
- **Level 2** — a loop with a *lifecycle*: memory operations turn a stateless
  process into a reasoning engine with state.
- **Level 3** — a *system*-level loop: operations happen both inside and
  outside the loop, and the harness itself becomes the durable thing.

`ai-swarm`'s [`crate::agent::loops`] (`SingleShot`, `ReActLoop`) is Level 1 on
its own. `faraday` is what moves a harness up:

| Level | Faraday device | This module |
|---|---|---|
| 2 — in-loop, revisable | Idea Books | [`ideabook::IdeaBook`] |
| 3 — permanent, cross-activation | The Diary | [`diary::Diary`] |
| 3 — retrieval, composed context | Loose slips, retrieval sheets, "indexes of indexes" | [`index::Slip`], [`index::SheetComposer`], [`index::Menu`] |

This also lines up with the memory taxonomy a structured-memory system
typically names (working / episodic / semantic / procedural memory, through a
capture → encode → store → organize → retrieve pipeline):

```
  capture   →   encode   →   store         →   organize        →   retrieve
  Diary::record   Slip{}       Diary/SlipIndex    SheetComposer       RetrievalSheet
  (episodic)      (semantic    (persisted the      groups slips        (working memory:
                   tag)         same way            into a purpose-      the bounded set
                                Storage is)          built set            actually handed
                                                                          to a model)
```

Idea Books hold the in-progress procedural content — hypotheses, plans — that
has not yet settled into something worth keeping permanently.

## The five devices, and what they became

Tweney's "bird's-eye view" names five categories in Faraday's archive. Four
map directly onto this module; the fifth (Work sheets — raw calculation
scratch) has no analogue here because it is closer to a tool's own scratch
space than to memory worth indexing.

### The Diary → `diary::Diary`

Faraday's Diary ran from entry #1 (25 August 1832) to #16041 (6 March 1860) —
one unbroken numbering, never restarted, never renumbered. Tweney is explicit
about why that mattered: *"it is only with such a fixed and unvarying address
scheme that he could have used the wide variety of retrieval aids... that we
know he used from the 1830s to the end."*

```rust
pub trait Diary: Send + Sync {
    async fn record(&self, entry: NewDiaryEntry) -> Result<u64>;   // returns the permanent address
    async fn entry(&self, seq: u64) -> Result<Option<DiaryEntry>>;
    async fn window(&self, seq: u64, radius: u32) -> Result<Vec<DiaryEntry>>;
    async fn len(&self) -> Result<u64>;
}
```

`seq` is assigned once, in order, never reassigned or reused — the same
property that let every slip and cross-reference Faraday ever wrote keep
meaning what it meant for decades. `InMemoryDiary` is a real, tested
implementation; swap it for anything durable (sqlite, an append-only file,
object storage) the same way `InMemoryStorage` stands in for a real `Storage`
backend.

### Idea Books → `ideabook::IdeaBook`

Where the Diary is permanent, Idea Books were the opposite by design. Tweney:
*"Idea books were not kept in chronological order, entries were not dated...
there is evidence that Faraday altered some entries after, perhaps long after,
they were written. In a number of cases, Faraday crossed out particular
entries — this was rarely done in the diaries."*

```rust
pub trait IdeaBook: Send + Sync {
    async fn jot(&self, topic: &str, content: &str) -> Result<u64>;
    async fn revise(&self, id: u64, content: &str) -> Result<u64>;  // strikes the old entry, links to it
    async fn strike(&self, id: u64) -> Result<()>;
    async fn by_topic(&self, topic: &str) -> Result<Vec<Speculation>>;  // full history, struck entries included
    async fn live(&self, topic: &str) -> Result<Vec<Speculation>>;      // only the current tip of each chain
}
```

A struck entry stays in the record — Faraday's practice was specifically to
keep the crossed-out entry visible, not erase it. The history of *how a
thought changed* was itself worth keeping. `revise` encodes this: it strikes
the old entry and jots a new one linked via `supersedes`, forming a chain
rather than a mutable slot. A `ReActLoop` step that reconsiders its own plan
should be revising an `IdeaBook` entry, not silently overwriting a fact in the
transcript.

### Loose slips → `index::Slip`

*"Loose slips are generally small (about 12 by 100 mm, on average) and almost
always contain only one line of writing... a brief descriptor followed by one
or more references to Diary numbers."*

```rust
pub struct Slip {
    pub descriptor: String,
    pub topic: String,
    pub refs: Vec<u64>,   // Diary addresses
}
```

This is the *encode* step: turning a raw Diary entry into something findable
by topic rather than by address. `SlipIndex` is where slips live once tagged
— deliberately separate from the Diary, because which slips point to an entry
can be freely reorganized, the same way Faraday *"would sort and re-sort the
slips on a particular topic."*

### Retrieval sheets → `index::RetrievalSheet` / `SheetComposer`

*"[Retrieval sheets] generally have multiple lines of entries... using them to
organize the writing of his scientific papers."*

```rust
pub trait SheetComposer: Send + Sync {
    async fn compose(&self, purpose: &str, slips: &[Slip], diary: &dyn Diary, window_radius: u32)
        -> Result<RetrievalSheet>;
}
```

This is the *organize* + *retrieve* step, and it is where the module's central
design argument lives — see below. `StandardComposer` is a real implementation:
it resolves every slip's references through `Diary::window` (never `entry`
alone), deduplicates by address, and returns the result in Diary order.

### "Indexes of indexes" → `index::Menu`

*"Some [retrieval sheets] stand out because they don't refer directly to the
Diary itself. Instead, they seem to be indexes of indexes, or 'menus'... these
are striking because they suggest that Faraday used so many retrieval devices
that he needed to organize these as well."*

`Menu` is trait-only. Faraday only needed this once his own retrieval devices
had multiplied past what he could hold in his head; a deployment earns the
need for a concrete `Menu` the same way, once it has enough slips and sheets
that browsing them requires its own index. `InMemorySlipIndex` implements
`Menu` over its own topics as a starting point.

## The one finding the design is built around

Tweney ran an experiment directly on Faraday's own retrieval sheets:

> *"I took one of the sheets and photocopied the relevant sections referred to
> in the Diary, pasted these up on long sheets, and tried to read the result...
> Unfortunately, the result made little sense, even when it dealt with a part
> of the Diary that I was fairly familiar with. The reason is clear: Faraday
> didn't just need references to the particular facts recorded in the Diary;
> he needed cues to the entire context of his memories about the incidents in
> question. In fact, when I abandoned the long paste-ups... and simply read
> the Diary in the order in which references were made, the whole made much
> more sense."*

A bare fact, retrieved in isolation, is not the same information the record
actually held. This is the whole design argument for `Diary::window` and for
why `SheetComposer` resolves a slip's reference into a *window*, not a single
entry — a composer that used `entry` alone would reproduce exactly the failure
Tweney found.

`faraday::diary::tests::window_returns_neighbors_not_just_the_hit` and
`faraday::index::tests::composed_sheet_carries_the_neighborhood_not_just_the_hit`
reproduce this directly: five diary entries describing an unfolding
experiment, where entry 3 alone ("deflection!") is uninterpretable and entry 3
with radius 1 recovers the setup and the result around it.

**Context engineering, in this module's specific sense**, is the discipline of
never breaking that link: retrieving enough surrounding record to keep a fact
meaningful, while keeping the total window bounded enough to fit a prompt.
That is a narrower claim than "context engineering" is sometimes used to mean
(prompt formatting, token budgeting, tool-result summarization) — this module
is specifically about the retrieval step, not the rest of that surface.

## Status

**Real and tested (12 tests):** `InMemoryDiary`, `InMemoryIdeaBook`,
`InMemorySlipIndex`, `StandardComposer` — including the two tests above that
reproduce Tweney's finding directly, and tests for permanent/monotonic Diary
addresses, revision chains that keep struck entries visible, and
deduplication/ordering when composing from overlapping slip windows.

**Trait-only:** `Menu` beyond the `InMemorySlipIndex` starting point — a real
deployment earns this once slips and sheets have multiplied enough to need
their own index, the same way Faraday's did.

**Not done — the natural next steps, in order:**

1. **Nothing writes to a `Diary` during a real activation.** `StandardHarness`
   and `loops::execute_tools` do not yet record observations, decisions, or
   tool results anywhere. Wiring this in is the equivalent of what
   `docs/boundaries-and-shared-cache.md` calls out for the cache layer:
   the types are real, the harness does not use them yet.
2. **No governance labels on Diary entries.** A `DiaryEntry` is exactly the
   kind of thing `crate::security::identity::GovernanceLabel` exists to scope — shared
   memory across harnesses needs the same tenant/scope checks the cache
   layer already has designed in.
3. **No connection from `GuardedServices` to a `Diary`/`IdeaBook`.** The
   permission boundary in `docs/boundaries-and-shared-cache.md` should
   eventually gate memory reads and writes here the same way it gates
   `storage_get`/`storage_set` today.
