# mnemosyne dashboard

A small React + TypeScript (Vite) single-page app that polls mnemosyne's
diagnostic HTTP API (`crates/mnemosyne/src/api.rs`) every 3 seconds and shows,
live, while you chat with mnemosyne in the terminal:

- **Agents** — mnemosyne and morpheus, with uid, trust, and parent (same data
  `GuardedServices::list_agents` returns, over HTTP).
- **Diary** — every permanent fact morpheus has recorded, newest first.
- **Topics & Slips** — how the Diary is organized for retrieval.
- **Ideas** — the IdeaBook's in-flight speculation, struck entries shown
  visibly struck-through rather than hidden.
- **Audit** — every permission check either agent has made, denials
  highlighted.

Not bundled into mnemosyne's binary or the Cargo workspace — it's a plain,
separate npm project that happens to live next to the Rust app it talks to.

## Run it

```bash
# 1. In one terminal: start mnemosyne (the API defaults to :4477)
cargo run -p mnemosyne

# 2. In another: install once, then run the dashboard's dev server
cd crates/mnemosyne/dashboard
npm install
npm run dev
```

Open the printed `http://localhost:5183` URL. The "API base" field in the
header defaults to `http://127.0.0.1:4477` (mnemosyne's default `API_PORT`)
and is saved in `localStorage` — change it there if you ran mnemosyne with a
different `API_PORT`.

`npm run build` produces a static `dist/` you could serve from anywhere; it
still just needs network access to wherever mnemosyne's API is running.

## Why hand-rolled HTTP on the Rust side, not a framework

`crates/mnemosyne/src/api.rs` follows `crates/evals/src/server.rs`'s existing
precedent in this workspace: a handful of fixed `GET` routes with no request
body doesn't need a framework, so it's a raw `tokio::net::TcpListener` loop
instead of pulling in one. `Access-Control-Allow-Origin: *` is sent on every
response so this dev server (a different origin/port) can call it directly —
fine for a local diagnostic tool, not something to expose beyond that as-is.
