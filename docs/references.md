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

## Evaluation harness — `crates/evals/`

**Harness. `harness-evals`.** <https://github.com/harness/harness-evals>
Used two ways in `crates/evals/`. First, as vocabulary: `golden.rs`,
`outcome.rs`, and `metrics.rs` reuse its `Golden` (authored input +
expectation) → `EvalCase`-equivalent (`CaseOutcome` here) → `Metric` →
normalized `Score` (0.0–1.0, threshold, computed `passed`) shape, and its
"Five Dimensions" framing (Correctness, Groundedness, Safety, Trajectory,
Performance), each dimension set by the metric that measures it rather than
chosen per case. Second, as a literal dependency: `harness_evals_run.rs` and
`server.rs` are a real integration — the real, pip-installed `harness-evals`
CLI is run against a real HTTP endpoint (`server.rs`, matching its
`HttpTarget`'s `{"input"} -> {"output"}` contract exactly) in front of the
real `condesate` agent loop, using its own real `contains`/`latency`
metrics — not a reimplementation. See `crates/evals/README.md` §2.

**Daniel Rosehill (@danielrosehill). "Awesome AI Evaluations & Benchmarks."**
<https://github.com/danielrosehill/Awesome-AI-Evaluations-Tools>
A curated survey of open-source eval frameworks and benchmark suites, used
two ways: as the broader map of what "an evals app" typically reports (a
scored table, machine-readable exports, a dashboard) when scoping `evals`'
output formats — stdout table, `report.json`, `report.csv`,
`dashboard.html`; and as the source of the "Agentic & Tool Use Evals"
section evaluated for integration below (thirteen projects surveyed for
real integrability; three were real and easy enough to build for real, most
of the rest were either heavier academic benchmarks needing their own
environments, or hard-wired to specific named agent frameworks with no
external-system hook — one (`HarnessLab`) turned out not to be a real,
installable artifact at all: `pip install harnesslab` 404s on PyPI).

**LangChain. `agentevals`.** <https://github.com/langchain-ai/agentevals>
`agentevals_run.rs` (`bin/`) is a real integration: two genuine
`condesate` `ReActLoop` runs (one correct, one with a real, silently
dropped step) are dumped as OpenAI-message-shaped JSON and graded against a
hand-authored reference by the real, pip-installed
`agentevals.trajectory.match.create_trajectory_match_evaluator`, across all
four of its real match modes. See `crates/evals/README.md` §3.

**AWS / Strands Agents. `strands-agents-evals`.** <https://github.com/strands-agents/evals>
`strands_evals_run.rs` is a real integration: the same HTTP target server
used for `harness-evals` (`server.rs`), graded by real
`strands_evals.Experiment`s using the package's real deterministic
evaluators (`Equals`, `Contains`, `ToolCalled` — no LLM judge). See
`crates/evals/README.md` §4.

**iris-eval. `@iris-eval/mcp-server`.** <https://github.com/iris-eval/mcp-server>
`iris_eval_run.rs` is a real integration, and the only one of the three that
needed no new server: it spawns the real, published MCP server via `npx`
and calls its real `evaluate_output` tool through `condesate`'s own,
already-real MCP client (`crate::agent::mcp`, see the MCP SDK entry above)
— the same `SingleShot` + `authorize_tool` path every other tool call in
this codebase goes through. See `crates/evals/README.md` §5.

## Two-agent demo — `crates/leader-search/`

**Cooperiano. `mcp-duckduckgo`.** <https://github.com/Cooperiano/duckduckgo-mcp>
(npm package `mcp-duckduckgo`) A real, free, no-API-key web-search MCP
server (`search`, `search_and_crawl`, `research`, `fetch`). `search_agent.rs`
spawns it via `npx` and reaches its `search` tool through the same
`condesate::McpConnection` client used above — the search agent's *only*
granted tool. Chosen after checking several npm candidates for the same
"is this real and actually installable" bar applied to the eval
integrations; `@modelcontextprotocol/server-brave-search` (the official
alternative) is deprecated and needs an API key besides.
