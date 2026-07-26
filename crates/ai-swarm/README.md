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

Cross-cutting services reach agents/tools through a single `ServiceHandle`
(identity, roster, `Storage`, `Bus`) — the "syscall surface".

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
| `ModelProvider` | `src/model.rs` | wire real inference: Ollama/llama.cpp locally, an HTTP API in the cloud (marked `>>> REAL ... <<<`) |
| `Tool` | `src/tool.rs` | add capabilities; tools get the `ServiceHandle`, so they can touch storage/bus |
| `Agent` | `src/agent.rs` | change how a turn is produced (planning, memory injection, retrieval) |
| `AgentLoop` | `src/loops.rs` | change control flow: tree search, debate, human-in-the-loop, budget-limited |
| `Storage` | `src/storage.rs` | Redis, sqlite, a vector DB — same interface |
| `Bus` / `Payload` | `src/bus.rs` | new message kinds, or a network transport instead of in-process mpsc |
| `Harness` | `src/harness.rs` | different runtime: batch, cron, streaming, REPL |
| `Swarm` | `src/swarm.rs` | scheduling/lifecycle policy for the whole fleet |
| `PolicyEngine` | `src/policy.rs` | where allow/deny comes from: static rules, a remote PDP, a human |
| `AuditSink` | `src/audit.rs` | where the trail goes: memory, file, storage, an SIEM |
| `CachePool` / `CacheRegistry` | `src/cache/pool.rs` | the shared KV store behind it: map, sqlite, Redis, a vector index |
| `PullPlanner` / `PeerTransport` | `src/cache/federation.rs` | how (and whether) cache values move between nodes |

## The two cross-cutting planes

Alongside that stack sit two things every layer touches. Both are currently
trait-and-type scaffolding — the shapes are settled, most implementations are
deliberately not written yet. Full write-up in
[`docs/boundaries-and-shared-cache.md`](../../docs/boundaries-and-shared-cache.md).

```
  identity ─► policy ─► kernel ─► audit        the permission boundary
                           │
                           ▼
                         cache                 the shared KV store
```

**Permission boundary.** `Principal` (harness + agent + model class + tenant +
trust tier) × `Action` × `Resource` → `Decision`, evaluated by a `PolicyEngine`.
Default deny, deny-wins, and delegation may only ever attenuate. The `Kernel`
admits an `AgentManifest` into a `GrantSet`, wraps `ServiceHandle` in a
`GuardedServices`, and every crossing goes check → audit → effect. An effect
that could not be audited does not run.

**Shared KV store.** One `CachePool` per `ModelClass`, so agents running the
same model share work. How far a value crosses between classes is governed by
`Compatibility` and each `ValueClass`'s floor: raw KV blocks need byte-identical
serving classes, summaries stay in one family, RAG chunks only need a common
embedding space. A lookup returns `Candidate`s, not hits — geometry and model
compatibility are the cache's job, authorization is the policy layer's.

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
2. Add a `Transcript` persisted to `Storage` per activation for durable memory.
3. Add a `Scheduler` trait to `Swarm` for priorities / fairness across harnesses.
4. Give `Bus` a network backend so a swarm can span machines.
5. Close the boundary: make `Tool::call` take a `&GuardedServices` instead of a
   `&ServiceHandle`, and have `Swarm` admit each harness through a `Kernel`.
   Until then the guard is an *additional* gate, not the only one — see the
   gaps list at the end of `docs/boundaries-and-shared-cache.md`.
```
