# leader-search

A two-agent demo built on `condesate`: a **leader** that can dynamically
spawn and terminate a **search** agent, which does real web searches
through a real MCP server. Both agents share the same live model, picked
from `.env`.

```bash
cargo run -p leader-search
```

Needs:
- `PROVIDER=anthropic|openai` + the matching API key (`ANTHROPIC_API_KEY` /
  `OPENAI_API_KEY`), e.g. in a `.env` file at the workspace root (see
  `.env.example`). There's no offline fallback here — see `src/model.rs`
  for why.
- Node.js 20+ (the search agent spawns
  [`mcp-duckduckgo`](https://github.com/Cooperiano/duckduckgo-mcp) via
  `npx` — free, no API key, no signup).

Ask it something that needs current information ("what's the latest stable
Rust release?") and watch it: spawn the search agent, delegate the query,
relay an answer built from real fetched page content (not just a link),
then — if you ask a follow-up — go back to the *same* search agent
conversation rather than starting cold. Say goodbye or change topics and it
tears the search agent back down. Ask it something it can just answer
(simple facts, conversation) and it won't bother spawning anything.

By default, stdout carries only the chat itself (greeting, `you>`/`leader>`
lines, `bye.`) and stderr carries a few one-line diagnostics — nothing about
how the leader got its answer. Set `VERBOSE=1` to also see, live on stderr:
each agent's think-steps and tool calls (including the search agent's raw
fetched page content), the MCP server's own startup log, and every
permission check as `audit #N ...` with a summary count at the end — see
"Where's the audit log" below.

## What's actually real here

- **`condesate::GuardedServices::spawn_child`** — a real library feature
  added for this app (`crates/condesate/src/security/kernel.rs`), not
  scaffolding. It admits the search agent as a genuinely attenuated child
  principal: its `AgentManifest` requests exactly the tools it needs (see
  below), so that's exactly what it gets — not the leader's other tools,
  and critically not `Action::Spawn`, so the search agent cannot spawn a
  grandchild. Gated on `Action::Spawn` against `Resource::Spawn { agent }`,
  the same check → audit → effect path every other syscall in `condesate`
  goes through. See `security::kernel::tests::spawn_child_*` for the proof.
- **A real, permissioned process table — `list_agents`.** Every admitted
  principal gets a fresh `AgentUid` (a real UUID; `Principal::agent`, e.g.
  `"search"`, is the readable name, reused across respawns — the uid is what
  tells two instances of it apart) and a live registry entry from
  `Kernel::attach` until its `GuardedServices` handle drops. `list_agents`
  is permission-bound like everything else — `Action::Read` against
  `Resource::Agent { agent }`, checked and audited per entry — but self- and
  spawned-descendant visibility is *structural*, not a grant: the leader
  sees itself and the search agent it spawned without either being
  requested in a manifest, the same way a child can never outrank its
  parent. Anything else needs an explicit rule. Try it: ask the leader to
  spawn the search agent and then list the agents it can see, before
  terminating — you'll get both, with the search agent's `parent` uid
  matching the leader's own. See `security::kernel::tests::list_agents_*`.
- **Real web search and page fetching, three tools, one deliberately
  withheld.** The search agent is granted `search`, `search_and_crawl`, and
  `fetch` on the real, no-API-key
  [`mcp-duckduckgo`](https://github.com/Cooperiano/duckduckgo-mcp) MCP
  server, reached through `condesate`'s own MCP client — the same
  integration `crates/evals`' iris-eval path uses. `fetch`/`search_and_crawl`
  are what let it read actual page content instead of returning a search
  snippet's link. The fourth tool, `research`, is *not* granted: live
  testing found a real bug in mcp-duckduckgo's own Go implementation — one
  call panics (`interface conversion: interface {} is nil, not string`) and
  kills the whole server process, poisoning every other tool call on that
  connection for the rest of the session. Upstream, compiled, not fixable
  here — so it's just not granted. See `search_agent.rs`'s module docs.
- **A real, stateful conversation with the search agent**, not one-shot
  Q&A. `SearchAgentHandle` keeps its own running transcript across
  `ask_search_agent` calls, so a follow-up question ("what were the key
  highlights of that release specifically?") still has the earlier
  findings — including fetched page content — in context, the same way
  `chat-app` carries a transcript across turns.
- **Real live model, both agents** — `condesate::PromptedToolModel` wraps
  `AnthropicModel`/`OpenAiModel` (feature `remote`) with a prompted
  (ReAct-style) tool-calling convention, since `condesate::agent::wire`
  doesn't implement either vendor's native tool-use wire format yet. See
  `condesate::PromptedToolModel`'s doc comment for the tool-result framing
  bug this had to work around (a bare tool result like `"4"` is
  indistinguishable from a new user message unless explicitly marked).
- **Defense in depth on termination** — `terminate_search_agent` checks
  `Action::Control` on `Resource::Peer { id: "search" }` *in addition to*
  the ordinary tool-invoke gate every tool call already goes through — the
  same two-layer pattern `ShutdownSwarm` uses internally for
  `broadcast_shutdown`.
- **The answer is handed off through shared storage, not a bare return
  value.** The search agent's `ReActLoop::run` result is written with its
  own `storage_set("search/result", ...)`, under a grant scoped to
  `search/*` and nothing else; the leader then reads the same key back with
  its own `storage_get`, under a separately-requested grant. Two
  independently-checked, independently-audited crossings of the permission
  boundary, not one direct function-call return threading the answer
  straight through — see `AskSearchAgent::call` in `search_agent.rs`.

## Where's the audit log

Every `GuardedServices::check` call — for *both* principals, since
`spawn_child` shares the leader's own `Kernel` rather than starting a
second one — is recorded in a `condesate::MemoryAudit` sink regardless of
any of the below, and its final `[audit] N boundary crossing(s) checked this
run, M denied` summary always prints on exit. Two more sinks are opt-in on
top of that:

- **`VERBOSE=1`** wraps it in `condesate::TracingAudit`, which mirrors each
  event to stderr the instant it happens (`audit #4 ... search/819fa5aa
  Write mem:search/result ALLOW[write-result] (Completed)`) — that
  `search/819fa5aa` is the acting principal's name and the first 8
  characters of its `AgentUid`, so two same-named instances (the search
  agent spawned, terminated, and spawned again) never look like the same
  line twice. `TracingAudit` existed in `condesate` before this app but was
  never re-exported from the crate's public API — promoted alongside
  `spawn_child` since this was the first real consumer that needed to see it.
- **`AUDIT_LOG_PATH`** — already present in `.env.example` (and picked up
  automatically from `.env`, since `main.rs` calls `dotenvy::dotenv()`
  before reading it) — wraps it in `condesate::FileAudit`, which appends one
  `summarize()` line per event to that file in append mode:

  ```bash
  AUDIT_LOG_PATH=./audit.log cargo run -p leader-search   # or just set it in .env
  ```

Both wrap the same underlying `MemoryAudit`, so with both set every event
lands in all three places (file, stderr, and the in-memory sink) from the
same `append()` call. Neither is on by default — a plain `cargo run -p
leader-search` writes nothing but the chat to stdout and a few one-line
diagnostics to stderr.

## What isn't real (known simplifications)

- **No bus/inbox messaging between the two agents.** The leader doesn't run
  the search agent as a `Harness` with its own inbox loop; asking it
  something is still a direct, in-process `ReActLoop::run` *call* against the
  search agent's own `BasicAgent` and its own `GuardedServices` (only the
  *answer* travels back through storage, as above). `condesate::swarm::Bus`
  (the actual IPC layer `Swarm` uses) isn't touched at all here — there was
  no need to make its routing table mutable at runtime for this app, since
  nothing addresses the search agent by `HarnessId` over the bus.
- **"Terminate" doesn't revoke.** `condesate::Kernel::revoke_all()` bumps
  one epoch shared by the *whole* kernel — there's no way to invalidate just
  the search agent's `GuardedServices` without also staling out the
  leader's own. Terminating here just drops the handle (and closes the MCP
  child process) rather than cryptographically revoking anything. A
  per-principal revoke would be a real, separate feature to add if that
  distinction ever matters.
- **One search agent at a time.** `search_agent::Registry` holds at most
  one `SearchAgentHandle`; calling `spawn_search_agent` again while one is
  already running is a no-op, not a second instance.
