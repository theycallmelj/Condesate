# evals

Four independent ways to evaluate `condesate`, all in this crate. See
`docs/references.md` for the source material.

## Run everything

```bash
./crates/evals/run-all.sh
```

Sets up and runs all five eval paths below in one shot: creates (or reuses)
a Python venv at `crates/evals/.venv`, installs `harness-evals`, `httpx`,
`agentevals`, and `strands-agents-evals` into it, checks for `npx` (skips
the iris-eval integration with a warning if Node.js isn't available rather
than failing the whole run), builds the crate, then runs the native suite
and all four real integrations in sequence, printing a pass/fail summary at
the end and exiting non-zero if anything failed. Safe to re-run — the venv
is only created once.

Each piece below can also be run and installed individually if you'd rather
not set up all four external tools at once.

1. **Native suite** — a self-authored suite (borrowing `harness-evals`'
   vocabulary) that drives the agent loop in-process, with full access to
   tool traces.
2. **Real `harness-evals` integration** — the actual, pip-installed
   [`harness-evals`](https://github.com/harness/harness-evals) CLI, run for
   real against a real HTTP endpoint in front of `condesate`.
3. **Real `agentevals` integration** — LangChain's real, pip-installed
   [`agentevals`](https://github.com/langchain-ai/agentevals) trajectory
   matcher, grading real (including a genuinely regressed) `condesate` runs.
4. **Real `strands-agents-evals` integration** — AWS's real, pip-installed
   [`strands-agents-evals`](https://github.com/strands-agents/evals),
   grading the same HTTP endpoint as #2 with its own deterministic
   evaluators.
5. **Real `iris-eval/mcp-server` integration** — a real, published MCP
   server, called through `condesate`'s own MCP *client*
   (`condesate::McpConnection`) — no new server or protocol code.

None of 2–5 reimplement the tool they integrate — each shells out to (or, for
#5, speaks the real wire protocol to) the genuine, independently-maintained
project, and its own real pass/fail verdict is what gets printed and gates
the exit code.

## The model behind #2 and #4

`harness_evals_run` and `strands_evals_run` both hit the same HTTP target
(`server.rs`). That target uses a **real** model — not a script — whenever
`PROVIDER`/`ANTHROPIC_API_KEY` or `OPENAI_API_KEY` are set (a `.env` file at
the workspace root, see `.env.example`, is loaded automatically). With no
key configured it falls back to a deterministic scripted model instead, so
these paths still work fully offline. **This means #2 and #4 make real,
billed API calls whenever a key is configured** — including via
`run-all.sh`. If you don't want that, unset `PROVIDER` or don't put a key
in `.env` before running them.

This is a deliberate fix to an earlier version of this crate, where every
eval path used a scripted model that already knew the "right" answer —
meaning nothing here could ever fail the way real agent evals are supposed
to: catching a model that picks the wrong tool, hallucinates, or otherwise
does something the harness didn't script for. With a live model, `contains`/
`Equals`/`ToolCalled` etc. are checking a real, non-deterministic decision.

`condesate::agent::wire` doesn't implement either vendor's native
tool-calling wire format yet (it's marked with `>>> TODO <<<` in the
source) — `server.rs`'s `PromptedToolModel` bridges that with an ordinary
prompted/ReAct-style convention (`TOOL_CALL: <name> <json>` on its own
line) instead of a vendor tool_use block. One real bug surfaced building
this: tool results were arriving as plain, unmarked "user" turns
(`condesate`'s wire mapping folds every non-system role into `user` for
these text APIs), so the model couldn't tell a tool's own result apart from
a new instruction and would call the same tool repeatedly until the loop's
step cap kicked in. Fixed by explicitly prefixing tool-result turns with
`[TOOL RESULT]` before they reach the model, and telling it what that means
in the system prompt.

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

## 3. Real `agentevals` integration

```bash
pip install agentevals
cargo run -p evals --bin agentevals_run
```

`agentevals_run` runs **two real `condesate` `ReActLoop` activations** —
a correct one (count words, then report the count to a peer) and a
genuinely regressed one (counts, then silently never reports back) — and
dumps both, plus a hand-authored reference plan, as OpenAI-message-shaped
JSON to `agent_evals/trajectory.json`. It then shells out to
`python3 grade_trajectory.py`, which grades both real runs against the
reference with the real
[`agentevals.trajectory.match.create_trajectory_match_evaluator`](https://github.com/langchain-ai/agentevals)
across all four of its match modes (`strict`/`unordered`/`subset`/`superset`).

The point isn't that the correct run passes (trivial) — it's that the
regressed run is caught by `strict` matching and correctly identified as a
`subset` (fewer tool calls, not an unrelated failure) of the intended plan.
That distinction — dropped-a-step vs. did-something-else-entirely — is real
signal a CI gate would want, and it comes from `agentevals`' real matching
logic, not this crate's.

Env vars: `AGENTEVALS_SCRIPT` (default `grade_trajectory.py`, resolved from
`agent_evals/`).

## 4. Real `strands-agents-evals` integration

```bash
pip install strands-agents-evals
cargo run -p evals --bin strands_evals_run
```

Starts the same `eval_target_server` as #2, then shells out to
`python3 strands_eval.py`, which builds four real
[`strands_evals.Experiment`](https://github.com/strands-agents/evals)s
against it — a task function that POSTs to `/run` and reports back
`{"output": ..., "trajectory": [...]}}`, where `trajectory` is the server's
own record of which tools it actually called (not a guess — see above),
graded by Strands' real deterministic evaluators: `Contains` (correctness —
the answer contains the right count, or the right phrase; not `Equals`,
since a live model's exact phrasing isn't guaranteed) and `ToolCalled`
(trajectory — the right tool was actually attempted). No LLM judge;
`strands-agents-evals` also ships LLM-based evaluators
(`HelpfulnessEvaluator`, etc.) that aren't used here.

Env vars: `EVAL_TARGET_ADDR` (default `127.0.0.1:8080`), `STRANDS_EVALS_SCRIPT`
(default `strands_eval.py`, resolved from `strands_evals/`).

## 5. Real `iris-eval/mcp-server` integration

```bash
cargo run -p evals --bin iris_eval_run   # needs Node.js 20+; npx fetches the package on first run
```

Unlike #2 and #4, this needs **no new server**: `condesate` already has a
real MCP client (`condesate::McpConnection`/`McpTool`, feature `mcp`, see
`crates/condesate/src/agent/mcp.rs`) that speaks MCP to any server over
stdio. `iris_eval_run` spawns the real, published
[`@iris-eval/mcp-server`](https://github.com/iris-eval/mcp-server) via
`npx`, discovers its tools, and calls its real `evaluate_output` tool
(heuristic, deterministic, offline — no API key) on three real `condesate`
outputs through the *exact same* `SingleShot` + `authorize_tool` tool-calling
path every other tool call in this codebase goes through — nothing
MCP-specific or eval-specific about the wiring.

Three real outputs, three different `eval_type` bundles: the word-count
answer (`completeness`), the permission boundary's own denial text
(`safety` — proving the boundary's plain-language refusals don't themselves
leak PII or trip injection/hallucination heuristics), and the echo answer
(`relevance`). The exit code reflects whether every call *ran*, not whether
iris-eval's own opinionated thresholds all came back `passed: true` — its
`completeness` rule wants ≥50 characters and ≥2 sentences, which a terse
numeric answer like `"4"` will never satisfy, and that's a real, useful
verdict worth printing honestly rather than tuning the inputs to please it.
