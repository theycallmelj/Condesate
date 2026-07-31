# condesate workspace

A Cargo workspace with three crates:

- **`crates/condesate`** — the trait-driven agent-harness + swarm library.
- **`crates/chat-app`** — a small REPL chat app that *imports* the library and
  can talk to OpenAI or Anthropic.
- **`crates/evals`** — an evaluation harness: a native suite against the real
  `condesate` agent loop (table/JSON/CSV/dashboard output), plus a real
  integration with the [`harness-evals`](https://github.com/harness/harness-evals)
  CLI over HTTP.

```
condesate-ws/
├── Cargo.toml            # workspace root
├── rust-toolchain.toml
├── crates/
│   ├── condesate/        # library (+ `demo` bin, tests)
│   ├── chat-app/         # consumer app
│   └── evals/            # eval harness: native suite + real harness-evals HTTP integration
```

## Quick start

```bash
# run the swarm demo (offline, deterministic mock models)
cargo run -p condesate --bin demo

# run the chat app offline (echo provider)
cargo run -p chat-app

# run the chat app against a real model
cargo run -p chat-app --features remote
#   PROVIDER=anthropic ANTHROPIC_API_KEY=sk-... MODEL=claude-sonnet-4-6
#   PROVIDER=openai    OPENAI_API_KEY=sk-...    MODEL=gpt-4o

# run all tests (offline)
cargo test

# run the native eval suite — prints a table, and writes evals-out/{report.json,report.csv,dashboard.html}
cargo run -p evals

# run the REAL harness-evals CLI against condesate over HTTP (needs: pip install harness-evals httpx)
cargo run -p evals --bin harness_evals_run
```

## The four pieces you asked for

### 1. Make it a git repo
From the workspace root:

```bash
git init
git add .
git commit -m "condesate: agent harness + swarm OS, chat app, providers, tests"

# then point it at a remote and push (create the empty repo on the host first):
git branch -M main
git remote add origin git@github.com:<you>/condesate.git   # or the https URL
git push -u origin main
```

> This workspace's own remote is still `git@github.com:theycallmelj/ai-swarm.git`
> — the crate/package was renamed locally to `condesate`, but the GitHub repo
> itself was not, since that's an external, harder-to-reverse action. Rename it
> on GitHub (or via `gh repo rename`) and update the remote with
> `git remote set-url origin <new-url>` if you want the two to match.

`.gitignore` already excludes `/target` and `.env*` (keep API keys out of git).

### 2. A project that imports the library
`crates/chat-app` depends on `condesate` by path and uses its public API
(`ModelProvider`, `BasicAgent`, `AgentLoop`/`SingleShot`, `Message`, ...) to run
a persistent chat loop. That's the template for any consumer crate: add
`condesate = { path = "…" }` (or a git/version dep) and build on the traits.

### 3. Connect to OpenAI or Anthropic
`crates/condesate/src/agent/remote.rs` provides `OpenAiModel` and `AnthropicModel`,
both implementing `ModelProvider`, behind the `remote` feature (so the offline
core stays dependency-light). The request/response mapping lives in
`src/agent/wire.rs` as pure functions and is unit-tested without any network.

> Toolchain note: the `remote` feature pulls `reqwest`, whose current dependency
> tree requires a recent stable Rust (edition 2024). The offline core, the demo,
> the chat app's default build, and the whole test suite build on older
> toolchains too.

### 4. Tests
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
attenuation, revocation, audit); memory/context engineering (`faraday`); and
MCP tool discovery/calling, including proof that a denied call never reaches
the server.

See `crates/condesate/README.md` for the architecture and per-trait extension
guide, `crates/evals/README.md` for the eval suite, and `docs/references.md`
for the source material behind the non-obvious design choices.
