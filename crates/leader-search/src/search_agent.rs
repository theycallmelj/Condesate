//! The search agent: spawned on demand by the leader, admitted as an
//! attenuated child principal (see `condesate::GuardedServices::spawn_child`),
//! and given exactly one capability — the real, no-API-key
//! [`mcp-duckduckgo`](https://github.com/Cooperiano/duckduckgo-mcp) MCP
//! server's `search` tool, reached through `condesate`'s own MCP client the
//! same way `crates/evals`'s iris-eval integration does.
//!
//! There is no background task or bus messaging here: "spawning" means
//! admitting the child principal and connecting the MCP server; "asking" it
//! something is a direct, in-process `ReActLoop::run` call against its own
//! agent and its own guarded services. The *answer* itself, though, does not
//! come back as a bare function return — the search agent writes it to
//! shared storage under its own grant, and the leader reads it back under a
//! separate grant of its own (see [`AskSearchAgent::call`]), so the hand-off
//! is two independently-checked crossings of the permission boundary, not
//! one. "Terminating" drops the handle and closes the MCP connection. What
//! `condesate` actually enforces throughout is the boundary itself — every
//! one of the search agent's own tool calls is checked against the narrow
//! grant it was admitted with, not the leader's broader one, and it can't
//! spawn a grandchild because its own manifest never asked for
//! `Action::Spawn`.

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
const SEARCH_TOOL: &str = "mcp:duckduckgo:search";
/// Shared-memory key the answer is handed off through — see the module docs.
const RESULT_KEY: &str = "search/result";

pub struct SearchAgentHandle {
    services: Arc<GuardedServices>,
    connection: Arc<McpConnection>,
    agent: BasicAgent,
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
            // Exactly one tool, exactly the one it needs — not "every
            // mcp-duckduckgo tool" (search_and_crawl/research/fetch also
            // exist on that server).
            Rule::allow(
                "web-search",
                SubjectMatch::agent(SEARCH_AGENT_ID),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact(SEARCH_TOOL.to_string())),
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

async fn spawn(leader: &GuardedServices, storage: Arc<dyn Storage>) -> Result<SearchAgentHandle> {
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
    let connection = McpConnection::connect_stdio("duckduckgo", cmd)
        .await
        .context("failed to launch mcp-duckduckgo — is Node.js 20+ / npx available?")?;

    let tools: Vec<Arc<dyn Tool>> =
        connection.tools().await?.into_iter().filter(|t| t.spec().name == SEARCH_TOOL).collect();
    if tools.is_empty() {
        return Err(anyhow!("mcp-duckduckgo did not advertise the expected '{SEARCH_TOOL}' tool"));
    }

    let (model, label) = choose_model()?;
    eprintln!("[search agent] model: {label}");
    let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();
    let system_prompt = format!(
        "You are a web-search assistant. Use your search tool to find real, current information, \
         then answer the question directly and concisely, citing what you found. Don't speculate \
         when you can search instead.\n\n{}",
        tool_instructions(&specs)
    );
    let agent = BasicAgent::new(SEARCH_AGENT_ID, system_prompt, model, tools);

    Ok(SearchAgentHandle { services, connection, agent })
}

pub struct SpawnSearchAgent {
    pub registry: Registry,
    /// Shared with the leader's own `ServiceHandle` — the same storage
    /// backend, reached through two separately-permissioned guards. Without
    /// this, the search agent's `storage_set` and the leader's `storage_get`
    /// would be writing to and reading from two different stores.
    pub storage: Arc<dyn Storage>,
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
        *reg = Some(spawn(leader, self.storage.clone()).await?);
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
            description: "Ask the running search agent something that needs a real web search. \
                Requires spawn_search_agent first. Args: {\"query\": <string>}."
                .into(),
        }
    }

    async fn call(&self, args: serde_json::Value, leader: &GuardedServices) -> Result<String> {
        let query =
            args.get("query").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg 'query'"))?;

        let mut reg = self.registry.lock().await;
        let handle =
            reg.as_mut().ok_or_else(|| anyhow!("no search agent running — call spawn_search_agent first"))?;

        let mut ctx = AgentContext::new(handle.services.clone());
        ctx.transcript.push(Message::user(query.to_string()));
        let outcome = ReActLoop { max_steps: 4 }.run(&handle.agent as &dyn Agent, &mut ctx).await?;

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
