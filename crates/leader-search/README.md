# leader-search

A multi-agent demo built on `condesate`: a **leader** that can dynamically
spawn and terminate a **search** agent, which does real web searches through
a real MCP server, and can also talk directly to **external agents hosted on
other sites** over the real
[A2A ("Agent2Agent")](https://a2a-protocol.org/) protocol. Every agent
involved shares the same live model, picked from `.env`.

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
- Network access to whatever site you point `discover_a2a_agent` at, if you
  use it — it's a plain outbound HTTPS client, nothing to install.

Ask it something that needs current information ("what's the latest stable
Rust release?") and watch it: spawn the search agent, delegate the query,
relay an answer built from real fetched page content (not just a link),
then — if you ask a follow-up — go back to the *same* search agent
conversation rather than starting cold. Say goodbye or change topics and it
tears the search agent back down. Ask it something it can just answer
(simple facts, conversation) and it won't bother spawning anything.

Point it at a site that hosts its own agent ("there's an agent at
https://example.com — ask it what it can do") and it takes a different path:
`discover_a2a_agent` fetches that site's agent card (name, description,
skills, the actual endpoint to message), then `send_a2a_message` sends it one
message over real JSON-RPC and relays the reply — no spawning, no child
principal, just an outbound call under the leader's own tool grant. See
[`crates/condesate/src/agent/a2a.rs`](../condesate/src/agent/a2a.rs) for the
client and [`src/main.rs`](src/main.rs)'s system prompt for how the leader
decides between this and the search agent.

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
- **Defense in depth on termination** — `terminate_search_agent` calls
  `GuardedServices::send_shutdown`, which checks `Action::Control` on
  `Resource::Peer { id: "search" }` *in addition to* the ordinary
  tool-invoke gate every tool call already goes through — the same
  two-layer pattern `ShutdownSwarm` uses internally for
  `broadcast_shutdown`.
- **Real agent-to-agent messaging over `condesate::swarm::Bus`, not an
  in-process function call.** Spawning the search agent starts it as its own
  tokio task running [`run_search_actor`](src/search_agent.rs) — a genuine
  inbox consumer, the same shape a `StandardHarness` runs inside a `Swarm`,
  just registered onto the bus dynamically (`Bus::insert_route`) since the
  search agent doesn't exist yet when the leader boots and so can't be in a
  `Swarm`'s fixed roster from the start. `ask_search_agent` sends the query
  as a real `Payload::Task` (`GuardedServices::send_task`, checked against
  the leader's own `Action::Send` grant) and blocks on the leader's *own*
  inbox for the search agent's `Payload::Reply` — two independently-checked,
  independently-audited crossings of the permission boundary, not one direct
  `ReActLoop::run` call reaching into the search agent's private state. The
  search agent's transcript now lives inside its actor loop, carried across
  `Payload::Task` messages, so a follow-up question still has earlier
  findings in context. The answer is *also* written to shared storage
  (`storage_set("search/result", ...)`, under the search agent's own
  `search/*`-scoped grant) as a second, independently-audited record of the
  same answer — see `run_search_actor` and `AskSearchAgent::call` in
  `search_agent.rs`.
- **Real A2A protocol client — agent cards and JSON-RPC, not a mock.**
  `discover_a2a_agent` fetches a site's agent card (trying both the current
  `/.well-known/agent-card.json` and the earlier draft's
  `/.well-known/agent.json`, since both are seen on real, live agents) and
  `send_a2a_message` sends a `message/send` JSON-RPC 2.0 request to the
  endpoint the card names, over a real `reqwest` HTTP client. Both go through
  the *ordinary* `Action::Invoke`/`Resource::Tool` gate every tool call
  already goes through — no A2A-specific permission check, the same reasoning
  `condesate::agent::mcp` uses for MCP tools: a parallel gate is a gate that
  can drift out of sync with the real one. Unlike the search agent, there's
  no spawned child principal here at all — the leader talks to the remote
  agent directly, the same way it'd call any other tool. See
  `condesate::agent::a2a` for the client (client-only: it reaches agents
  other people host, it doesn't serve one) and its own module docs for what's
  deliberately out of scope (streaming, push notifications, task
  cancellation, auth beyond plain HTTPS).

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
