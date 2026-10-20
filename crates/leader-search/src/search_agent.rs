//! The search agent: spawned on demand by the leader, admitted as an
//! attenuated child principal (see `condesate::GuardedServices::spawn_child`),
//! and given three of the four tools on the real, no-API-key
//! [`mcp-duckduckgo`](https://github.com/Cooperiano/duckduckgo-mcp) MCP
//! server — `search`, `search_and_crawl`, `fetch` — reached through
//! `condesate`'s own MCP client the same way `crates/evals`'s iris-eval
//! integration does. `fetch`/`search_and_crawl` are what let it actually
//! read a page's content rather than just return a search snippet's link.
//!
//! The fourth tool, `research`, is deliberately *not* granted: live testing
//! found a real bug in mcp-duckduckgo's own Go implementation — calling it
//! panics (`interface conversion: interface {} is nil, not string`) and
//! kills the whole server process, poisoning every other tool on the same
//! connection for the rest of that session. Not something this crate can
//! fix (it's upstream, compiled), so the fix here is simply not to grant
//! the broken tool — narrower than "all of them" was always an option, and
//! this is why it's the right one for `research` specifically.
//!
//! **This is real agent-to-agent messaging, not a function call.** Spawning
//! the search agent starts [`run_search_actor`] as its own tokio task,
//! owning its own `Inbox` — a genuine actor, the same shape `StandardHarness`
//! runs inside a `Swarm`, just registered dynamically (via
//! `condesate::Bus::insert_route`) since the search agent doesn't exist yet
//! when the leader boots. `ask_search_agent` sends the query as a real
//! `Payload::Task` over the shared `Bus` (`GuardedServices::send_task`,
//! checked against the leader's own `Action::Send` grant) and blocks on the
//! leader's *own* inbox for the search agent's `Payload::Reply` — two
//! separately-checked, separately-audited crossings of the permission
//! boundary, not one direct `ReActLoop::run` call reaching into the search
//! agent's private state. The search agent keeps its own running transcript
//! inside its actor loop, across `Payload::Task` messages, so a follow-up
//! question still has the earlier findings in context, the same way
//! `chat-app` keeps a transcript across turns — that continuity now lives on
//! the actor side of the bus instead of in the tool that calls it.
//! "Terminating" sends a `Payload::Shutdown` to the search agent's inbox
//! (`GuardedServices::send_shutdown`, gated on `Action::Control` against
//! that peer) and joins its task, so the actor drains its inbox and closes
//! the MCP connection before a respawn can race it. What `condesate` enforces
//! throughout is the boundary itself — every one of the search agent's own
//! tool calls and bus sends is checked against the grant it was admitted
//! with, not the leader's broader one, and it can't spawn a grandchild
//! because its own manifest never asked for `Action::Spawn`.

use anyhow::{anyhow, Context, Result};
use condesate::{
    tool_instructions, Action, Agent, AgentContext, AgentLoop, AgentManifest, BasicAgent, Bus,
    Envelope, GuardedServices, HarnessId, Inbox, McpConnection, Message, ModelClass, Pattern,
    Payload, ReActLoop, ResourcePattern, Rule, ServiceHandle, Storage, SubjectMatch, TenantId,
    Tool, ToolSpec, TrustTier,
};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::model::choose_model;

const LEADER_ID: &str = "leader";
const SEARCH_AGENT_ID: &str = "search";
/// The three mcp-duckduckgo tools this agent actually gets — `research` is
/// excluded; see the module docs for why.
const WEB_TOOLS: [&str; 3] =
    ["mcp:duckduckgo:search", "mcp:duckduckgo:search_and_crawl", "mcp:duckduckgo:fetch"];
/// Shared-memory key the answer is *also* recorded under — see the module
/// docs on why the answer's primary path back is a bus `Payload::Reply` now,
/// with this as a second, independently-audited write under the search
/// agent's own grant, not the way the leader actually gets its answer.
const RESULT_KEY: &str = "search/result";

/// A handle to the running search agent: just its background actor task now
/// — the agent, its transcript, its MCP connection, and its `GuardedServices`
/// all live inside [`run_search_actor`], reached only through the bus from
/// here on, never through a direct reference back into that task's state.
pub struct SearchAgentHandle {
    task: JoinHandle<()>,
}

pub type Registry = Arc<Mutex<Option<SearchAgentHandle>>>;

fn search_agent_manifest() -> AgentManifest {
    AgentManifest {
        harness: HarnessId::new(SEARCH_AGENT_ID),
        agent: SEARCH_AGENT_ID.to_string(),
        model_class: ModelClass {
            provider: "leader-search".into(),
            family: "leader-search".into(),
            revision: "leader-search".into(),
            embedding_space: None,
            quantization: None,
        },
        tenant: TenantId::new("local"),
        requested_trust: TrustTier::Standard,
        requested: vec![
            Rule::allow(
                "web-search",
                SubjectMatch::agent(SEARCH_AGENT_ID),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact(WEB_TOOLS[0].to_string())),
            ),
            Rule::allow(
                "web-crawl",
                SubjectMatch::agent(SEARCH_AGENT_ID),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact(WEB_TOOLS[1].to_string())),
            ),
            Rule::allow(
                "web-fetch",
                SubjectMatch::agent(SEARCH_AGENT_ID),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact(WEB_TOOLS[2].to_string())),
            ),
            // Write-only, and only under its own namespace — it hands the
            // leader an answer, it can't read anything the leader put there.
            Rule::allow(
                "write-result",
                SubjectMatch::agent(SEARCH_AGENT_ID),
                &[Action::Write],
                ResourcePattern::Memory(Pattern::parse("search/*")),
            ),
            // Lets the search agent reply to the leader over the bus —
            // attenuated from the leader's own `bus-messaging` root grant
            // (see `main.rs`), narrowed to exactly this one direction.
            Rule::allow(
                "reply-to-leader",
                SubjectMatch::agent(SEARCH_AGENT_ID),
                &[Action::Send],
                ResourcePattern::Peer(Pattern::parse(LEADER_ID)),
            ),
        ],
        cache_classes: vec![],
    }
}

async fn spawn(leader: &GuardedServices, storage: Arc<dyn Storage>, bus: Bus, verbose: bool) -> Result<SearchAgentHandle> {
    // The search agent doesn't exist when the leader (and its Bus) boots, so
    // its route is added to the *same* bus at spawn time rather than being
    // in the routing table from the start — see `Bus::insert_route`.
    let (tx, inbox) = mpsc::unbounded_channel();
    bus.insert_route(HarnessId::new(SEARCH_AGENT_ID), tx);

    let raw = ServiceHandle {
        me: HarnessId::new(SEARCH_AGENT_ID),
        roster: Arc::new(vec![HarnessId::new(LEADER_ID), HarnessId::new(SEARCH_AGENT_ID)]),
        storage,
        bus,
    };
    let services = leader
        .spawn_child(&search_agent_manifest(), raw)
        .await
        .map_err(|refusal| anyhow!("spawning the search agent was denied: {refusal}"))?;
    let services = Arc::new(services);

    let mut cmd = tokio::process::Command::new("npx");
    cmd.arg("--yes").arg("mcp-duckduckgo");
    // mcp-duckduckgo logs its own startup banner (and, if it ever panics
    // again, a stack trace — see the module docs) to stderr, independent of
    // our own trace/audit output. Inherited only in verbose mode, so quiet
    // runs stay quiet regardless of what the child process feels like
    // printing.
    let connection = McpConnection::connect_stdio("duckduckgo", cmd, verbose)
        .await
        .context("failed to launch mcp-duckduckgo — is Node.js 20+ / npx available?")?;

    let tools: Vec<Arc<dyn Tool>> =
        connection.tools().await?.into_iter().filter(|t| WEB_TOOLS.contains(&t.spec().name.as_str())).collect();
    if tools.is_empty() {
        return Err(anyhow!("mcp-duckduckgo did not advertise any of the expected tools ({WEB_TOOLS:?})"));
    }

    let (model, label) = choose_model()?;
    eprintln!("[search agent] model: {label}");
    let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();
    let system_prompt = format!(
        "You are a web-research assistant working for a leader agent, not for an end user \
         directly. Use search to find candidates, then use fetch (or search_and_crawl) to \
         actually read the pages that look relevant — don't stop at snippets.\n\
         \n\
         When you report back, share the substantive content you found — the actual facts, \
         numbers, quotes, explanations — not just a link or a citation. The leader can't click \
         links; it can only see what you write. It may come back with follow-up questions about \
         what you already found, or ask you to look closer at a specific source — you'll still \
         have this conversation's earlier findings in context, so build on them rather than \
         starting over.\n\n{}",
        tool_instructions(&specs)
    );
    let agent = BasicAgent::new(SEARCH_AGENT_ID, system_prompt, model, tools);

    // The actor itself: a real inbox consumer running on its own task, not a
    // function this crate calls into. See the module docs and
    // `run_search_actor`.
    let task = tokio::spawn(run_search_actor(services, connection, agent, inbox, verbose));

    Ok(SearchAgentHandle { task })
}

/// The search agent's own message loop. Each inbound `Payload::Task` is
/// answered with a `ReActLoop::run` over a transcript that persists across
/// messages (so a follow-up still has earlier findings, including fetched
/// page content, in context) and the answer travels back as a
/// `Payload::Reply` addressed to whoever sent the task — over the same `Bus`
/// `ask_search_agent` used to send it, not a bare function return.
/// `Payload::Shutdown` drains the loop and closes the MCP connection.
async fn run_search_actor(
    services: Arc<GuardedServices>,
    connection: Arc<McpConnection>,
    agent: BasicAgent,
    mut inbox: Inbox,
    trace: bool,
) {
    let mut transcript: Vec<Message> = Vec::new();
    while let Some(Envelope { from, payload, .. }) = inbox.recv().await {
        match payload {
            Payload::Shutdown => break,
            Payload::Task(query) => {
                let mut ctx = AgentContext::new(services.clone());
                ctx.trace = trace;
                ctx.transcript = std::mem::take(&mut transcript);
                ctx.transcript.push(Message::user(query));

                let reply = match (ReActLoop { max_steps: 8 }).run(&agent as &dyn Agent, &mut ctx).await {
                    Ok(outcome) => {
                        transcript = ctx.transcript;
                        // A second, independently-audited crossing of the
                        // permission boundary for the same answer — under
                        // this agent's own write-only grant — even though
                        // the leader now gets its copy over the bus reply
                        // below, not by reading this key back.
                        if let Err(e) = services.storage_set(RESULT_KEY, &outcome.final_text).await {
                            eprintln!("[search agent] failed to record result in storage: {e}");
                        }
                        outcome.final_text
                    }
                    Err(e) => {
                        transcript = ctx.transcript;
                        format!("search agent error: {e}")
                    }
                };

                if let Err(e) = services.send_reply(&from.to_string(), &reply).await {
                    eprintln!("[search agent] failed to reply to {from}: {e}");
                }
            }
            other => {
                eprintln!("[search agent] ignoring unexpected message from {from}: {other:?}");
            }
        }
    }
    connection.close().await;
}

pub struct SpawnSearchAgent {
    pub registry: Registry,
    /// Shared with the leader's own `ServiceHandle` — the same storage
    /// backend, reached through two separately-permissioned guards. Without
    /// this, the search agent's `storage_set` and the leader's `storage_get`
    /// would be writing to and reading from two different stores.
    pub storage: Arc<dyn Storage>,
    /// The same `Bus` the leader's own `ServiceHandle` was built with — the
    /// search agent's inbox is registered onto it at spawn time (see
    /// `spawn`), so both principals end up addressing each other over one
    /// shared routing table.
    pub bus: Bus,
    /// `VERBOSE` at startup — mirrored into the actor task so its think
    /// steps, tool calls, and the MCP server's own log line up with the
    /// leader's own tracing.
    pub verbose: bool,
}

#[async_trait::async_trait]
impl Tool for SpawnSearchAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_search_agent".into(),
            description: "Start the web-search agent. Call this before ask_search_agent if it \
                isn't already running. No arguments."
                .into(),
        }
    }

    async fn call(&self, _args: serde_json::Value, leader: &GuardedServices) -> Result<String> {
        let mut reg = self.registry.lock().await;
        if reg.is_some() {
            return Ok("search agent already running".into());
        }
        *reg = Some(spawn(leader, self.storage.clone(), self.bus.clone(), self.verbose).await?);
        Ok("search agent spawned and ready".into())
    }
}

pub struct AskSearchAgent {
    pub registry: Registry,
    /// The leader's own inbox. Held here rather than drained by a background
    /// loop — the leader's real driver is the REPL, not the swarm harness
    /// loop — so a call can send the query as a genuine `Payload::Task` over
    /// the bus and then block on this same inbox for the search agent's
    /// `Payload::Reply`, exactly the way two independent harnesses talk.
    pub leader_inbox: Arc<Mutex<Inbox>>,
}

#[async_trait::async_trait]
impl Tool for AskSearchAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_search_agent".into(),
            description: "Ask the running search agent something that needs real web research — \
                an initial question, or a follow-up on what it already found (it remembers the \
                conversation). Requires spawn_search_agent first. Args: {\"query\": <string>}."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, leader: &GuardedServices) -> Result<String> {
        let query =
            args.get("query").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'query'"))?;

        {
            let reg = self.registry.lock().await;
            if reg.is_none() {
                return Err(anyhow!("no search agent running — call spawn_search_agent first"));
            }
        }

        // A real cross-principal send — checked against the leader's own
        // `Action::Send` grant, delivered onto the search agent's actor
        // inbox — not a direct call into its loop.
        leader.send_task(SEARCH_AGENT_ID, query).await?;

        let mut inbox = self.leader_inbox.lock().await;
        loop {
            let env = inbox
                .recv()
                .await
                .ok_or_else(|| anyhow!("leader inbox closed while waiting for the search agent's reply"))?;
            if env.from != HarnessId::new(SEARCH_AGENT_ID) {
                continue;
            }
            match env.payload {
                Payload::Reply(text) => return Ok(text),
                other => eprintln!("[leader] ignoring unexpected message from search agent: {other:?}"),
            }
        }
    }
}

pub struct TerminateSearchAgent {
    pub registry: Registry,
}

#[async_trait::async_trait]
impl Tool for TerminateSearchAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "terminate_search_agent".into(),
            description: "Stop the running search agent and close its web-search connection. No arguments."
                .into(),
        }
    }

    async fn call(&self, _args: serde_json::Value, leader: &GuardedServices) -> Result<String> {
        let mut reg = self.registry.lock().await;
        match reg.take() {
            Some(handle) => {
                // `send_shutdown` is its own independent check beyond the
                // ordinary tool-invoke gate every tool call already goes
                // through — `Action::Control` against this specific peer,
                // the same defense-in-depth pattern `ShutdownSwarm` uses
                // internally for `broadcast_shutdown`.
                leader
                    .send_shutdown(SEARCH_AGENT_ID)
                    .await
                    .map_err(|e| anyhow!("terminating the search agent was denied: {e}"))?;
                // Join the actor rather than dropping the handle — it still
                // needs to drain to the shutdown message and close the MCP
                // connection, and a respawn right after this call must not
                // race that teardown.
                if let Err(e) = handle.task.await {
                    eprintln!("[leader] search agent task panicked: {e}");
                }
                Ok("search agent terminated".into())
            }
            None => Ok("no search agent was running".into()),
        }
    }
}

/// The process table: every currently-running agent this principal may see.
/// For the leader that's always itself, plus the search agent once spawned
/// — visible because `spawn_child` made the leader its spawner, not because
/// of any rule granted here. See `GuardedServices::list_agents`'s doc
/// comment for the structural-vs-granted split this relies on.
pub struct ListAgents;

#[async_trait::async_trait]
impl Tool for ListAgents {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_agents".into(),
            description: "List every agent you can currently see running: yourself, and any \
                agent you've spawned (e.g. the search agent, once started). No arguments."
                .into(),
        }
    }

    async fn call(&self, _args: serde_json::Value, leader: &GuardedServices) -> Result<String> {
        let mut agents = leader.list_agents().await?;
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        let lines: Vec<String> = agents
            .iter()
            .map(|a| {
                let parent = a.parent.map(|p| p.short()).unwrap_or_else(|| "-".into());
                format!("{} uid={} trust={:?} parent={}", a.name, a.uid.short(), a.trust, parent)
            })
            .collect();
        Ok(lines.join("\n"))
    }
}
