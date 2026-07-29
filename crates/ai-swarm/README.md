# ai-swarm

A trait-driven AI agent **harness** in Rust, with a **swarm** layer on top that
behaves like a tiny operating system: concurrent harnesses ("processes"),
message passing between them ("IPC"), and shared storage ("shared memory").

Every layer is a trait, so you can replace any one piece — the model call, the
loop strategy, the harness runtime, the storage backend, the transport — without
touching the others. The included implementations are deliberately simple (and
the models are deterministic offline mocks) so the whole thing compiles and runs
with `cargo run` and gives you clean seams to grow into.

## Layering

```
  ModelProvider   how an agent is *called*        LocalModel / CloudModel
       |
  Agent           model + prompt + tools          BasicAgent
       |
  AgentLoop       how the loop *works*            SingleShot / ReActLoop
       |
  Harness         one running "process"           StandardHarness
       |
  Swarm           OS: scheduling, IPC, storage    Swarm
```

Cross-cutting services reach agents/tools through a guarded handle
(`GuardedServices`, wrapping identity, roster, `Storage`, `Bus`) — the
"syscall surface". There is no raw `ServiceHandle` reachable from tool or loop
code; every real crossing is checked against policy and audited first.

## Directory layout

```
src/
  lib.rs, main.rs, types.rs   crate root + shared plain data types
  agent/    how one agent is called and thinks: ModelProvider, Agent, AgentLoop, Tool
  swarm/    the OS layer: Harness, Swarm, IPC (Bus), ServiceHandle, Storage
  security/ the permission boundary: identity, policy, kernel, audit
  cache/    shared KV store per ModelClass (scaffolding)
  faraday/  memory & context engineering
```

Each folder's `mod.rs` re-exports its public types, so the crate's flat public
API (`ai_swarm::Agent`, `ai_swarm::Kernel`, ...) is unaffected by which folder
a module physically lives in — only code *inside* the crate needs to know the
internal path.

## Run it

```
cargo run --bin demo
```

You'll see the planner (cloud model) delegate a word count to the worker (local
model), the worker reply over the bus, the planner store a verdict and broadcast
shutdown, then a dump of shared storage.

## Where each trait lives

| Trait | File | Swap it to... |
|-------|------|---------------|
| `ModelProvider` | `src/agent/model.rs` | wire real inference: Ollama/llama.cpp locally, an HTTP API in the cloud (marked `>>> REAL ... <<<`) |
| `Tool` | `src/agent/tool.rs` | add capabilities; tools get a `&GuardedServices`, so every storage/bus/cache touch is checked and audited |
| `Agent` | `src/agent/core.rs` | change how a turn is produced (planning, memory injection, retrieval) |
| `AgentLoop` | `src/agent/loops.rs` | change control flow: tree search, debate, human-in-the-loop, budget-limited |
| `Storage` | `src/swarm/storage.rs` | Redis, sqlite, a vector DB — same interface |
| `Bus` / `Payload` | `src/swarm/bus.rs` | new message kinds, or a network transport instead of in-process mpsc |
| `Harness` | `src/swarm/harness.rs` | different runtime: batch, cron, streaming, REPL |
| `Swarm` | `src/swarm/core.rs` | scheduling/lifecycle policy for the whole fleet |
| `PolicyEngine` | `src/security/policy.rs` | where allow/deny comes from: static rules, a remote PDP, a human |
| `AuditSink` | `src/security/audit.rs` | where the trail goes: memory, file, storage, an SIEM |
| `CachePool` / `CacheRegistry` | `src/cache/pool.rs` | the shared KV store behind it: map, sqlite, Redis, a vector index |
| `PullPlanner` / `PeerTransport` | `src/cache/federation.rs` | how (and whether) cache values move between nodes |
| `Diary` / `IdeaBook` | `src/faraday/` | memory and context engineering — swap the in-memory versions for a durable store |

## The cross-cutting planes

Alongside that stack sit two more things every layer can touch. Full write-ups
in [`docs/boundaries-and-shared-cache.md`](../../docs/boundaries-and-shared-cache.md)
and [`docs/faraday-context-engineering.md`](../../docs/faraday-context-engineering.md).

```
  identity ─► policy ─► kernel ─► audit        the permission boundary (live)
                           │
                           ▼
                         cache                 the shared KV store (scaffolding)

  faraday: diary + ideabook + index             memory & context engineering
                                                 (diary/ideabook/index are real;
                                                  not yet wired into the loop)
```

**Permission boundary — live, not just designed.** `Principal` (harness +
agent + model class + tenant + trust tier) × `Action` × `Resource` →
`Decision`, evaluated by a `PolicyEngine` against the principal's own admitted
`GrantSet` — never a shared global set matched only by pattern. Default deny,
deny-wins, and delegation may only ever attenuate (checked on both the
resource *and* the subject, so a child can't silently drop a parent's trust
floor). The `Kernel` admits an `AgentManifest` into that `GrantSet`, wraps
`ServiceHandle` in a `GuardedServices`, and `Swarm`/`Harness`/`Tool` all run
through it: every real crossing goes check → audit → effect, and an effect
that could not be audited does not run.

**Shared KV store — scaffolding.** One `CachePool` per `ModelClass`, so agents
running the same model share work. How far a value crosses between classes is
governed by `Compatibility` and each `ValueClass`'s floor: raw KV blocks need
byte-identical serving classes, summaries stay in one family, RAG chunks only
need a common embedding space. A lookup returns `Candidate`s, not hits —
geometry and model compatibility are the cache's job, authorization is the
policy layer's. No `CachePool` implementation exists yet.

**Memory and context engineering — named for Michael Faraday.** A permanent,
sequentially-addressed `Diary` (episodic memory), a revisable `IdeaBook`
(in-loop working memory, Level 2 in the "three levels of the agent loop"
sense), and `Slip`/`RetrievalSheet` composition that resolves a reference
through a *window* of surrounding entries, not the bare entry alone — directly
reproducing a finding from the historical record this module is modeled on:
retrieval that drops surrounding context stops being meaningful. See the doc
for the full mapping and the primary source.

## Design notes

- **Async trait objects.** Traits use `#[async_trait]` so they stay
  object-safe (`dyn ModelProvider`, `Box<dyn Harness>`, ...). That dynamic
  dispatch is what lets you mix implementations in one swarm and add more later.
- **Actor model.** Each harness owns one inbox (`mpsc`) and runs its loop once
  per message. Peers address each other by `HarnessId`; broadcast is supported.
- **Concurrency.** Harnesses are spawned as independent Tokio tasks and joined
  by the swarm. Swap the runtime or use OS threads if you prefer.
- **Determinism.** The two model mocks are small state machines keyed on the
  transcript so the demo is reproducible. Replace `complete()` with a real call
  and nothing above it changes.

## Suggested next steps

1. Implement a real `ModelProvider` (start with one HTTP call in `CloudModel`).
2. Wire `faraday::Diary` into `StandardHarness`/`loops::execute_tools` so an
   activation actually records observations, decisions, and tool results —
   durable memory, not just a transcript that dies with the activation. See
   the gaps list at the end of `docs/faraday-context-engineering.md`.
3. Add a `Scheduler` trait to `Swarm` for priorities / fairness across harnesses.
4. Give `Bus` a network backend so a swarm can span machines.
5. Implement a real `CachePool` (in-memory first) so the cache read/write path
   in `security::kernel::GuardedServices` can be exercised end to end — see
   the gaps list at the end of `docs/boundaries-and-shared-cache.md`.
