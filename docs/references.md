# References

Source material behind the non-obvious design choices in this workspace. Each
entry names what it's cited for and where in the code that shows up.

## Distributed caching — `crates/condesate/src/cache/`

**Devon Tietjen (@d-tietjen). "Pluribus: Predictive Distributed Caching for
LLM Coding Agents." July 6, 2026.**
Provided directly, not a public URL. The control-plane/data-plane split in
`cache/federation.rs` — bounded ownership sketches gossiped freely, whole
values pulled only when a local planner expects reuse — follows Pluribus's
protocol: range sketches, ellipsoid descriptors, peer quality tracking, and
the metadata-blind fallback when no embeddings are available.

## Memory & context engineering — `crates/condesate/src/faraday/`

**Ryan D. Tweney. "Faraday's notebooks: the active organization of creative
science." *Physics Education* 26, 1991, 301–306.**
<https://gwern.net/doc/cs/linkrot/archiving/1991-tweney.pdf>
The primary source. Describes the five categories of Faraday's records (the
Diary, Idea Books, loose slips, retrieval sheets, "indexes of indexes") that
`diary.rs`, `ideabook.rs`, and `index.rs` are directly modeled on, and reports
the retrieval experiment `Diary::window` and `SheetComposer` are built
around: pasting together only the *referenced facts* from a retrieval sheet
"made little sense," while reading them in Diary order, with their
neighbors, "made much more sense."

**Ryan D. Tweney & Christopher D. Ayala. "Memory and the construction of
scientific meaning: Michael Faraday's use of notebooks and records."
*Memory Studies* 8(4), 2015, 422–439. DOI: 10.1177/1750698015587149.**
<https://gwern.net/doc/cs/linkrot/archiving/2015-tweney.pdf>
Companion piece — reframes the same archival record through a "distributed
cognition" lens (external artifacts as part of the cognitive system, not
just records of it). Background for the module's "the record is cognitive
scaffolding, not a chore after the fact" framing.

**Oracle. "The Agent Loop Decoded: Three Levels Every Agent Engineer Must
Know." Oracle Developers Blog, June 11, 2026.**
<https://blogs.oracle.com/developers/the-agent-loop-decoded-three-levels-every-agent-engineer-must-know>
Source of the Level 1 (bare loop, no memory) / Level 2 (in-loop lifecycle
and memory) / Level 3 (memory and state external to the loop) framing used
in `faraday/mod.rs` to place `ideabook` (Level 2) against `diary`/`index`
(Level 3) relative to `crate::agent::loops` (Level 1).

**Oracle. "Introduction to Context Engineering and Agent Memory with Oracle
AI Database." Webinar page, scheduled January 29, 2026.**
<https://go.oracle.com/LP=151579>
Source of the memory-type taxonomy (working / episodic / semantic /
procedural / entity / workflow) and the capture → encode → store → organize
→ retrieve pipeline that `faraday/mod.rs` maps its own vocabulary onto
(Diary = episodic/*capture*, Slip = semantic/*encode*, IdeaBook = procedural,
SheetComposer = *organize* + *retrieve*).

## Tool protocol — `crates/condesate/src/agent/mcp.rs`

**Model Context Protocol. Official Rust SDK.**
<https://github.com/modelcontextprotocol/rust-sdk> (crate `rmcp`, v3.0.0)
The protocol `McpTool`/`McpConnection` speak: tools exposed by an external
MCP server (spawned as a child process, `tools/list` + `tools/call` over
stdio) are proxied through the same `Tool` trait as native tools, so they
pass through the *same* call-time permission gate in
`crate::agent::loops::execute_tools` before a request is ever sent — see
`mcp.rs`'s module docs for why no second, MCP-specific check exists.
