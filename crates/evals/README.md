# evals

Two independent ways to evaluate `condesate`, both in this crate. See
`docs/references.md` for the source material.

1. **Native suite** — a self-authored suite (borrowing `harness-evals`'
   vocabulary) that drives the agent loop in-process, with full access to
   tool traces.
2. **Real `harness-evals` integration** — the actual, pip-installed
   [`harness-evals`](https://github.com/harness/harness-evals) CLI, run for
   real against a real HTTP endpoint in front of `condesate`. Not a
   reimplementation — this shells out to the genuine tool.

## 1. Native suite

```bash
cargo run -p evals
```

Prints a table to stdout, then writes (default directory `evals-out/`,
override with `EVALS_OUT_DIR`):

- `report.json` — the full structured report.
- `report.csv` — one row per case.
- `dashboard.html` — a static, self-contained page: overall pass rate, a bar
  per dimension, and the full case table.

Exits non-zero if any case fails, so it can gate CI.

Every case drives the **real** `condesate` agent loop — `BasicAgent`,
`SingleShot`/`ReActLoop`, real `Tool` impls, a real admitted
`GuardedServices` behind a real `Kernel` — not a stub. The only mock is the
model itself: `harness::PlaybackModel` plays back a scripted list of turns,
because `condesate` ships no real inference (see its own `LocalModel`/
`CloudModel` doc comments).

Vocabulary borrowed from `harness-evals`:

- **`Golden`** (`golden.rs`) — what you author: an id, description, and an
  `Expectation`.
- **`CaseOutcome`** (`outcome.rs`) — a `Golden` enriched with what actually
  happened: final text, tool observations, denied tools, elapsed time, steps.
- **`Metric`** (`metrics.rs`) — a scoring function, `measure()`, that turns a
  `CaseOutcome` into a normalized `Score` (0.0–1.0, plus a threshold and a
  computed `passed`). Each metric declares its own `Dimension` — a case's
  dimension is whichever metric graded it, not something the case author
  picks.
- **Five dimensions** (`golden::Dimension`) — Correctness, Trajectory,
  Safety, Performance, Groundedness. `harness-evals` also has a
  Groundedness dimension for RAG; here it's repurposed for `condesate`'s own
  memory system (`faraday`) — a `Diary`/`SheetComposer` case proves a
  composed retrieval window keeps the context around a fact, not just the
  bare fact.

### Adding a case

Write an `async fn` in `suite.rs` that builds a `condesate::GuardedServices`
via `harness::build_services`, drives an agent through a loop (or, for
`faraday` cases, calls `Diary`/`SheetComposer` directly), and returns a
`CaseOutcome`. Add a `Golden` to `suite()` naming an `Expectation` — if none
of the six existing `Metric`s fit, add one in `metrics.rs` and register it in
`all_metrics()`.

## 2. Real `harness-evals` integration

```bash
pip install harness-evals httpx   # httpx: HttpTarget's async client
cargo run -p evals --bin harness_evals_run
```

This starts `server.rs` (a hand-rolled HTTP server — no framework, one
route) on `127.0.0.1:8080`, exposing the real `condesate` agent loop as a
[`harness-evals` `HttpTarget`](https://github.com/harness/harness-evals):
`POST /run {"input": "..."}` → `{"output": "..."}`. Once the server answers,
it shells out to the real `harness-evals` CLI —

```bash
harness-evals run harness_evals/condesate.eval.yaml
```

— which prints **its own** table (dimension bars included) and writes
`harness_evals/results.jsonl`. `condesate.eval.yaml` uses two of
`harness-evals`' real, non-LLM metrics (`harness-evals list-metrics` for the
full catalog — most of it needs an LLM judge and an API key, these two
don't):

- `contains` (correctness) — checks the response contains the expected text.
- `latency` (performance) — checks the real HTTP round-trip against a budget.

`harness_evals/goldens.jsonl` has four cases, including one
(`"input": "shutdown"`) where the server's `GuardedServices` was admitted
with **no** grant for `shutdown_swarm` — so `harness-evals`' own `contains`
metric is verifying, over real HTTP, the same permission-boundary guarantee
`suite::safety_denied_tool_never_executes` verifies in-process.

Env vars: `EVAL_TARGET_ADDR` (default `127.0.0.1:8080`), `HARNESS_EVALS_CONFIG`
(default `condesate.eval.yaml`, resolved from `harness_evals/`).

You can also run the two halves separately — start
`cargo run -p evals --bin eval_target_server` in one terminal, then
`harness-evals run condesate.eval.yaml` from `harness_evals/` in another.
