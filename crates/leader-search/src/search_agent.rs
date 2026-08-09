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
//! There is no background task or bus messaging here: "spawning" means
//! admitting the child principal and connecting the MCP server; "asking" it
//! something is a direct, in-process `ReActLoop::run` call against its own
//! agent and its own guarded services — but a *conversation*, not a fresh
//! one-shot each time: [`SearchAgentHandle`] keeps its own running
//! transcript, so a second `ask_search_agent` call is a follow-up with the
//! first call's findings still in context, the same way `chat-app` keeps a
//! transcript across turns. The *answer* itself does not come back as a bare
//! function return, either — the search agent writes it to shared storage
//! under its own grant, and the leader reads it back under a separate grant
//! of its own (see [`AskSearchAgent::call`]), so the hand-off is two
//! independently-checked crossings of the permission boundary, not one.
//! "Terminating" drops the handle and closes the MCP connection. What
//! `condesate` actually enforces throughout is the boundary itself — every
//! one of the search agent's own tool calls is checked against the grant it
//! was admitted with, not the leader's broader one, and it can't spawn a
//! grandchild because its own manifest never asked for `Action::Spawn`.

use anyhow::{anyhow, Context, Result};
use condesate::{
    tool_instructions, Action, Agent, AgentContext, AgentLoop, AgentManifest, BasicAgent, Bus,
    GuardedServices, HarnessId, McpConnection, Message, ModelClass, Pattern, ReActLoop, Resource,
    ResourcePattern, Rule, ServiceHandle, Storage, SubjectMatch, TenantId, Tool, ToolSpec, TrustTier,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::model::choose_model;

const SEARCH_AGENT_ID: &str = "search";
/// The three mcp-duckduckgo tools this agent actually gets — `research` is
/// excluded; see the module docs for why.
const WEB_TOOLS: [&str; 3] =
    ["mcp:duckduckgo:search", "mcp:duckduckgo:search_and_crawl", "mcp:duckduckgo:fetch"];
/// Shared-memory key the answer is handed off through — see the module docs.
const RESULT_KEY: &str = "search/result";

pub struct SearchAgentHandle {
    services: Arc<GuardedServices>,
    connection: Arc<McpConnection>,
    agent: BasicAgent,
    /// The running conversation with this agent, carried across calls so a
    /// follow-up question still has the earlier findings (and fetched page
    /// content) in context.
    transcript: Vec<Message>,
    /// Mirrors `VERBOSE` at spawn time — whether this agent's own think
    /// steps and tool calls get echoed to stderr (see `AgentContext::trace`)
    /// when `ask_search_agent` drives it.
    trace: bool,
}

impl SearchAgentHandle {
    async fn close(self) {
        self.connection.close().await;
    }
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
        ],
        cache_classes: vec![],
    }
}

async fn spawn(leader: &GuardedServices, storage: Arc<dyn Storage>, verbose: bool) -> Result<SearchAgentHandle> {
    let raw = ServiceHandle {
        me: HarnessId::new(SEARCH_AGENT_ID),
        roster: Arc::new(vec![HarnessId::new("leader"), HarnessId::new(SEARCH_AGENT_ID)]),
        storage,
        bus: Bus::new(HashMap::new()),
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

    Ok(SearchAgentHandle { services, connection, agent, transcript: Vec::new(), trace: verbose })
}

pub struct SpawnSearchAgent {
    pub registry: Registry,
    /// Shared with the leader's own `ServiceHandle` — the same storage
    /// backend, reached through two separately-permissioned guards. Without
    /// this, the search agent's `storage_set` and the leader's `storage_get`
    /// would be writing to and reading from two different stores.
    pub storage: Arc<dyn Storage>,
    /// `VERBOSE` at startup — see [`SearchAgentHandle::trace`].
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
        *reg = Some(spawn(leader, self.storage.clone(), self.verbose).await?);
        Ok("search agent spawned and ready".into())
    }
}

pub struct AskSearchAgent {
    pub registry: Registry,
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

        let mut reg = self.registry.lock().await;
        let handle =
            reg.as_mut().ok_or_else(|| anyhow!("no search agent running — call spawn_search_agent first"))?;

        // Continue the running conversation rather than starting fresh —
        // this is what lets a follow-up question reference a page the
        // agent already fetched.
        let mut ctx = AgentContext::new(handle.services.clone());
        ctx.trace = handle.trace;
        ctx.transcript = handle.transcript.clone();
        ctx.transcript.push(Message::user(query.to_string()));
        let outcome = ReActLoop { max_steps: 8 }.run(&handle.agent as &dyn Agent, &mut ctx).await?;
        handle.transcript = ctx.transcript;

        // Hand-off through shared storage, not a bare return value: the
        // search agent writes under its own grant (`write-result` above),
        // the leader reads under its own (`root_grants` in main.rs) — two
        // real, separately-audited crossings of the permission boundary.
        handle.services.storage_set(RESULT_KEY, &outcome.final_text).await?;
        leader
            .storage_get(RESULT_KEY)
            .await?
            .ok_or_else(|| anyhow!("search agent reported done but wrote no result"))
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
        // A second, independent check beyond the ordinary tool-invoke gate —
        // the same defense-in-depth pattern `ShutdownSwarm` uses internally
        // for `broadcast_shutdown`: being allowed to *call this tool* and
        // being allowed to *control this specific peer* are checked
        // separately, against separately-requested grants.
        leader
            .check(Action::Control, &Resource::Peer { id: HarnessId::new(SEARCH_AGENT_ID) })
            .await
            .map_err(|refusal| anyhow!("terminating the search agent was denied: {refusal}"))?;

        let mut reg = self.registry.lock().await;
        match reg.take() {
            Some(handle) => {
                handle.close().await;
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
