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
relay the answer, then tear the search agent back down. Ask it something it
can just answer (simple facts, conversation) and it won't bother spawning
anything.

## What's actually real here

- **`condesate::GuardedServices::spawn_child`** — a real library feature
  added for this app (`crates/condesate/src/security/kernel.rs`), not
  scaffolding. It admits the search agent as a genuinely attenuated child
  principal: its `AgentManifest` requests exactly one rule (invoke the
  `mcp:duckduckgo:search` tool), so that's exactly what it gets — not the
  leader's other tools, and critically not `Action::Spawn`, so the search
  agent cannot spawn a grandchild. Gated on `Action::Spawn` against
  `Resource::Spawn { agent }`, the same check → audit → effect path every
  other syscall in `condesate` goes through. See
  `security::kernel::tests::spawn_child_*` for the proof.
- **Real web search** — `mcp-duckduckgo` reached through `condesate`'s own
  MCP client (`condesate::McpConnection`, feature `mcp`), the same
  integration `crates/evals`' iris-eval path uses. No mocked results.
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
