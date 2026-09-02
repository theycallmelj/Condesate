# mnemosyne

A two-agent demo built on `condesate`: **mnemosyne**, an Opus-driven main
agent whose own conversation transcript is periodically wiped, backed by
**morpheus**, a Sonnet-driven background curator that turns the raw
conversation into `condesate::faraday`'s permanent memory structures (Diary,
IdeaBook, SlipIndex) before each wipe. mnemosyne keeps working from a bounded
context; nothing it was told is actually forgotten, because morpheus already
built it into long-term memory before the wipe, and mnemosyne can pull it
back with `recall_memory`.

```bash
cargo run -p mnemosyne
```

Needs:
- `PROVIDER=anthropic|openai` + the matching API key, e.g. in a `.env` file
  at the workspace root (see `.env.example`). No offline fallback, same
  reasoning as `leader-search`.
- `MAIN_MODEL` (default `claude-opus-4-6`) and `MORPHEUS_MODEL` (default
  `claude-sonnet-4-6`) — the two agents run different models on purpose;
  override either independently if your account doesn't have access to one
  of these exact ids.

By default, stdout is pure chat (greeting, `you>`/`mnemosyne>`, `bye.`) and
stderr carries a handful of one-line diagnostics plus the curation
announcements (`[morpheus] curating N message(s)...`). Set `VERBOSE=1` to
also see every think-step, tool call, and permission check live on stderr,
and/or `AUDIT_LOG_PATH` to append the audit trail to a file — same knobs as
`leader-search`, see its README for the reasoning.

## Watching it from outside: the dashboard

A read-only HTTP API (`src/api.rs`) starts alongside the chat loop —
`http://127.0.0.1:4477` by default, override with `API_PORT`. It exposes
every agent, everything currently in Faraday memory (Diary, topics, Slips,
IdeaBook), and the full audit trail as JSON. `dashboard/` is a small React +
TypeScript app that polls it every few seconds:

```bash
cd crates/mnemosyne/dashboard
npm install
npm run dev   # http://localhost:5183
```

Or skip the manual steps: set `DEBUG=1` and mnemosyne starts the dashboard's
dev server itself (running `npm install` first if `dashboard/node_modules`
doesn't exist yet) and opens it in your default browser, pre-pointed at the
right `API_PORT` via a `?api=` query param — no manual reconfiguration even
if you've changed it from the default. `DASHBOARD_PORT` overrides the
dashboard's own port (default `5183`). `DEBUG` is independent of
`VERBOSE` — one controls whether the dashboard auto-launches, the other
controls stderr trace detail; combine them freely.

```bash
DEBUG=1 cargo run -p mnemosyne
```

The dashboard is torn down when mnemosyne exits (best-effort — see
`dashboard.rs`'s `shutdown` doc comment for the one edge case that isn't
fully covered).

See `dashboard/README.md` for the endpoint list and why the Rust side is
hand-rolled HTTP rather than a framework (same reasoning `crates/evals`'
target server already established in this workspace).

## The cadence: `CURATE_EVERY` / `RESET_EVERY`

Two env vars, both counted in **turns** — one exchange (your message plus
mnemosyne's reply), not raw `Message` struct count:

- `CURATE_EVERY` (default `10`) — every this many turns, morpheus is handed
  everything since its last pass and builds it into Faraday.
- `RESET_EVERY` (default `20`) — every this many turns, mnemosyne's own
  transcript is cleared. A curation pass always runs immediately before a
  reset, *regardless* of `CURATE_EVERY`'s own cadence, covering whatever's
  accumulated since the last pass — nothing raw is ever dropped without a
  chance to be remembered first. (If `CURATE_EVERY >= RESET_EVERY`,
  mnemosyne logs a heads-up at startup: `CURATE_EVERY`'s own cadence would
  never fire on its own, since the reset always forces a pass first.)

A final curation pass also runs on `exit`/EOF/Ctrl-D, so nothing said right
before quitting is lost either.

```bash
CURATE_EVERY=5 RESET_EVERY=15 cargo run -p mnemosyne
```

## What's actually real here

- **The first real consumer of `condesate::faraday`.** The module's own docs
  note nothing in it was yet wired into `GuardedServices` — everything in
  `memory.rs` is that wiring, done at the tool level (`Action::Invoke` /
  `Resource::Tool`, the same gate every other tool in this workspace goes
  through) rather than inventing a new `Resource`/`Action` variant in the
  core policy engine. A first-class `faraday` integration is still the
  documented "natural next step" for the library itself, not something one
  demo app should decide unilaterally.
- **morpheus is a genuinely attenuated child**, spawned once at startup via
  `GuardedServices::spawn_child` (the same real feature `leader-search` uses)
  and reused for the process's whole life — curation is a recurring
  background job, not an on-demand tool. It holds exactly six narrowly-named
  tool grants (`record_diary_entry`, `tag_slip`, `list_live_ideas`,
  `jot_idea`, `revise_idea`, `strike_idea`) and nothing else — not
  `recall_memory` (that's mnemosyne's), not `Action::Spawn` (it can't spawn
  its own children).
- **A real runaway-loop backstop, and a real reason it's needed.** Each of
  morpheus's grants carries `Condition::StepsUnder { max_steps: 8 }` —
  `condesate::security::policy`'s own documented primitive for exactly this.
  It matters here: under live testing, morpheus (via `PromptedToolModel`'s
  prompted tool-calling convention) sometimes re-issues `record_diary_entry`
  for the same fact several times on a small, repetitive batch, worded
  slightly differently each time, before stopping — even with an explicit
  system-prompt instruction not to. This is a real, observed model
  reliability limit, not something a stronger prompt fully eliminated (see
  "What isn't real" below) — the backstop bounds the wasted calls when it
  happens rather than letting it run to `ReActLoop`'s full step budget with
  no explanation on the transcript. `curate()` calls `begin_activation`
  before every pass specifically so this counter resets each time, instead
  of accumulating across passes until every grant permanently denies.
- **The context reset is real, and recall across it is the actual point.**
  `main.rs`'s loop clears `transcript: Vec<Message>` outright every
  `RESET_EVERY` turns. The only way mnemosyne can still answer "what did I
  tell you earlier" after that is by actually calling `list_memory_topics` /
  `recall_memory` and getting a real answer back from `faraday` — there is
  no hidden fallback transcript. This was live-verified end to end: told a
  fact, several turns and a context reset later, asked about it again with
  no trace of it left in mnemosyne's own transcript, and it correctly
  recalled it from memory.
- **Retrieval pulls a window, not a bare fact.** `RecallMemory` calls
  `StandardComposer::compose`, which resolves each slip through
  `Diary::window` — the same "a fact without its surrounding record isn't
  the same information" argument `faraday`'s own module docs make.

## What isn't real (known simplifications)

- **Curation quality depends on the underlying model, and isn't perfectly
  reliable.** Beyond the duplicate-call behavior the `StepsUnder` backstop
  bounds (see above), on at least one live run a duplicate `record_diary_entry`
  call consumed a diary address that a later `tag_slip` call then
  (incorrectly) pointed a *different* fact's tag at — a real mistagging, not
  hypothetical. There's no cross-check here (no "does this address's content
  actually match what you're tagging" verification) — that would be a
  legitimate next step (a native-tool-calling model, a verification tool, or
  algorithmic content matching) but is beyond what this demo does today.
  Treat morpheus's output as "usually right, occasionally noisy," not as a
  guaranteed-correct index.
- **One curator, spawned once, never restarted.** If morpheus's process
  somehow got into a bad state there's no supervision here to detect or
  restart it — unlike `leader-search`'s on-demand search agent, there's also
  no `terminate`/re-`spawn` tool for it; its lifecycle is exactly mnemosyne's
  process lifetime.
- **No bus/inbox messaging between the two agents**, same simplification
  `leader-search` documents — curation is a direct, in-process `ReActLoop::run`
  call against morpheus's own agent and its own `GuardedServices`, not a
  message sent over `condesate::swarm::Bus`.
- **In-memory only.** `InMemoryDiary` / `InMemoryIdeaBook` / `InMemorySlipIndex`
  — nothing here is durable across a restart. Swapping in a real backend is a
  new `impl` of each trait, the same way `InMemoryStorage` stands in for a
  real `Storage`.
