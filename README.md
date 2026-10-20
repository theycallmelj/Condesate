# condesate workspace

A Cargo workspace with four crates:

- **`crates/condesate`** — the trait-driven agent-harness + swarm library.
- **`crates/chat-app`** — a small REPL chat app that *imports* the library and
  can talk to OpenAI or Anthropic.
- **`crates/evals`** — an evaluation harness: a native suite against the real
  `condesate` agent loop (table/JSON/CSV/dashboard output), plus real
  integrations with four external eval tools — `harness-evals`, `agentevals`,
  `strands-agents-evals`, and `iris-eval/mcp-server`.
- **`crates/leader-search`** — a two-agent demo: a leader that dynamically
  spawns and terminates a web-search agent (real MCP server, no API key),
  both driven by the same live model.

```
condesate-ws/
├── Cargo.toml            # workspace root
├── rust-toolchain.toml
├── crates/
│   ├── condesate/        # library (+ `demo` bin, tests)
│   ├── chat-app/         # consumer app
│   ├── evals/            # eval harness: native suite + real harness-evals HTTP integration
│   └── leader-search/    # leader + dynamically-spawned search agent, real MCP web search
```

## Quick start

```bash
# run the swarm demo (offline, deterministic mock models)
cargo run -p condesate --bin demo

# run the chat app offline (echo provider)
cargo run -p chat-app

# run the chat app against a real model — reads PROVIDER/ANTHROPIC_API_KEY/OPENAI_API_KEY/MODEL
# from your shell env, or automatically from a `.env` file (cp .env.example .env and fill it in)
cargo run -p chat-app --features remote

# run all tests (offline)
cargo test

# set up + run every eval path in one shot (venv, real pip installs, all 5 binaries)
# NOTE: uses a real, billed model if PROVIDER + an API key are set (incl. via .env) — see crates/evals/README.md
./crates/evals/run-all.sh

# ...or run pieces individually:

# run the native eval suite — prints a table, and writes evals-out/{report.json,report.csv,dashboard.html}
cargo run -p evals

# run the REAL harness-evals CLI against condesate over HTTP — real model if .env is set, else offline script
# (needs: pip install harness-evals httpx)
cargo run -p evals --bin harness_evals_run

# run the REAL agentevals trajectory matcher against real (incl. regressed) condesate runs — always offline/scripted
# (needs: pip install agentevals)
cargo run -p evals --bin agentevals_run

# run the REAL strands-agents-evals Experiment against condesate over HTTP — real model if .env is set, else offline script
# (needs: pip install strands-agents-evals)
cargo run -p evals --bin strands_evals_run

# run the REAL iris-eval MCP server via condesate's own MCP client (needs: Node.js 20+)
cargo run -p evals --bin iris_eval_run

# run the leader + web-search agent demo (needs PROVIDER + API key, e.g. in .env; Node.js 20+)
cargo run -p leader-search
```

## Using the library

### Importing the library
`crates/chat-app` depends on `condesate` by path and uses its public API
(`ModelProvider`, `BasicAgent`, `AgentLoop`/`SingleShot`, `Message`, ...) to run
a persistent chat loop. That's the template for any consumer crate: add
`condesate = { path = "…" }` (or a git/version dep) and build on the traits.

### Connecting to OpenAI or Anthropic
`crates/condesate/src/agent/remote.rs` provides `OpenAiModel` and `AnthropicModel`,
both implementing `ModelProvider`, behind the `remote` feature (so the offline
core stays dependency-light). The request/response mapping lives in
`src/agent/wire.rs` as pure functions and is unit-tested without any network.

> Toolchain note: the `remote` feature pulls `reqwest`, whose current dependency
> tree requires a recent stable Rust (edition 2024). The offline core, the demo,
> the chat app's default build, and the whole test suite build on older
> toolchains too.

### Tests
```bash
cargo test                                   # unit + integration, all offline
cargo test -p condesate --features remote    # + reqwest client build (needs current stable)
cargo test -p condesate --features mcp       # + MCP tool integration (in-process test server)
```

Covered: storage round-trip & prefix listing; bus direct-send / broadcast /
unknown-route error; tools (word_count, remember, send_message); the ReAct and
SingleShot loops incl. the step-budget cap; provider request-building and
response-parsing for both vendors; a full end-to-end swarm run asserting the
verdict written to shared storage; the permission boundary (admission,
attenuation, revocation, audit, and dynamic child spawning via
`GuardedServices::spawn_child`); memory/context engineering (`faraday`); and
MCP tool discovery/calling, including proof that a denied call never reaches
the server.

See `crates/condesate/README.md` for the architecture and per-trait extension
guide, `crates/evals/README.md` for the eval suite, `crates/leader-search/README.md`
for the two-agent demo, and `docs/references.md` for the source material
behind the non-obvious design choices.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE) for attribution.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall be
licensed as above, without any additional terms or conditions.
