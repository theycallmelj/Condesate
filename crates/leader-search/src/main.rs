//! A two-agent demo built on `condesate`: a **leader** that can dynamically
//! spawn and terminate a **search** agent, which does real web searches
//! through a real MCP server (`mcp-duckduckgo`, no API key needed). Both
//! agents share the same live model, picked from `.env` (see `model.rs`).
//!
//! The interesting part isn't the chat loop — it's that spawning the search
//! agent goes through `condesate::GuardedServices::spawn_child`: a real,
//! policy-checked admission that gives the child an *attenuated* copy of
//! the leader's own authority (here: exactly one MCP tool, nothing else —
//! not `Action::Spawn`, so the search agent can't spawn its own children;
//! not the leader's other tools). See `search_agent.rs` for the tools that
//! drive it and `docs/` (workspace root) for more.
//!
//! Run: `cargo run -p leader-search` (needs PROVIDER + an API key, e.g. in
//! `.env`, and Node.js 20+ for the search agent's MCP server).

mod model;
mod search_agent;

use anyhow::Result;
use condesate::{
    Action, AgentContext, AgentLoop, AgentManifest, BasicAgent, Bus, GrantSet, GuardedServices,
    HarnessId, InMemoryStorage, Kernel, MemoryAudit, Message, ModelClass, Pattern, ReActLoop,
    ResourcePattern, Rule, RuleSetPolicy, ServiceHandle, Storage, SubjectMatch, SystemClock,
    TenantId, Tool, ToolSpec, TrustTier,
};
use search_agent::{AskSearchAgent, Registry, SpawnSearchAgent, TerminateSearchAgent};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

const LEADER_ID: &str = "leader";
const SEARCH_ID: &str = "search";

/// The leader's own root authority: enough to invoke its three lifecycle
/// tools, to spawn exactly one kind of child ("search"), to control that
/// specific child once spawned, and to read/write the `search/*` memory
/// namespace the two hand results off through (see `search_agent.rs`).
/// `SubjectMatch::default()` on the tool-invoke and memory rules (not scoped
/// to "leader") is deliberate — it's what lets these same grants cover the
/// *search* agent's much narrower, differently-named requests when
/// `spawn_child` attenuates from them.
fn root_grants() -> Vec<Rule> {
    vec![
        Rule::allow(
            "leader-tools",
            SubjectMatch::default(),
            &[Action::Invoke],
            ResourcePattern::Tool(Pattern::Any),
        ),
        Rule::allow(
            "spawn-search",
            SubjectMatch::agent(LEADER_ID),
            &[Action::Spawn],
            ResourcePattern::Spawn(Pattern::parse(SEARCH_ID)),
        ),
        Rule::allow(
            "control-search",
            SubjectMatch::agent(LEADER_ID),
            &[Action::Control],
            ResourcePattern::Peer(Pattern::parse(SEARCH_ID)),
        ),
        Rule::allow(
            "search-result-mem",
            SubjectMatch::default(),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("search/*")),
        ),
    ]
}

fn build_leader_services(storage: Arc<dyn Storage>) -> Arc<GuardedServices> {
    let grants = root_grants();
    let kernel = Kernel::new(
        GrantSet::new(grants.clone()),
        Arc::new(RuleSetPolicy::new()),
        MemoryAudit::new(),
        Arc::new(SystemClock),
    );
    let raw = ServiceHandle {
        me: HarnessId::new(LEADER_ID),
        roster: Arc::new(vec![HarnessId::new(LEADER_ID), HarnessId::new(SEARCH_ID)]),
        storage,
        bus: Bus::new(HashMap::new()),
    };
    let manifest = AgentManifest {
        harness: HarnessId::new(LEADER_ID),
        agent: LEADER_ID.to_string(),
        model_class: ModelClass {
            provider: "leader-search".into(),
            family: "leader-search".into(),
            revision: "leader-search".into(),
            embedding_space: None,
            quantization: None,
        },
        tenant: TenantId::new("local"),
        requested_trust: TrustTier::Privileged,
        requested: grants,
        cache_classes: vec![],
    };
    let admission = kernel.admit(&manifest, None);
    let services = kernel.attach(&admission, raw);
    services.begin_activation(LEADER_ID, u64::MAX);
    Arc::new(services)
}

fn leader_system_prompt(tool_specs: &[ToolSpec]) -> String {
    format!(
        "You are the leader of a small two-agent system. You have no web search of your own — a \
         separate search agent does, over a real connection to a search server. Rules:\n\
         - If the user asks something that needs current or external information, call \
           spawn_search_agent (skip this if it's already running), then call ask_search_agent \
           with one focused query, then answer the user using what it found.\n\
         - When you're done needing web search for this conversation (e.g. the user says thanks, \
           changes topic to something you can answer yourself, or says goodbye), call \
           terminate_search_agent.\n\
         - For anything you can just answer (simple facts, conversation, math), answer directly — \
           don't spawn the search agent needlessly.\n\n{}",
        condesate::tool_instructions(tool_specs)
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let (model, label) = model::choose_model()?;
    eprintln!("[leader] model: {label}");

    // One storage backend shared by both principals, reached through two
    // separately-permissioned guards — see `search_agent.rs` for why.
    let storage: Arc<dyn Storage> = InMemoryStorage::new();

    let registry: Registry = Arc::new(tokio::sync::Mutex::new(None));
    let terminate_tool = Arc::new(TerminateSearchAgent { registry: registry.clone() });
    let leader_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SpawnSearchAgent { registry: registry.clone(), storage: storage.clone() }),
        Arc::new(AskSearchAgent { registry: registry.clone() }),
        terminate_tool.clone(),
    ];
    let specs: Vec<ToolSpec> = leader_tools.iter().map(|t| t.spec()).collect();
    let leader_agent = BasicAgent::new(LEADER_ID, leader_system_prompt(&specs), model, leader_tools);
    let leader_services = build_leader_services(storage);

    println!("leader-search ready — ask a question, or 'exit' to quit.\n");
    let stdin = std::io::stdin();
    let mut transcript: Vec<Message> = Vec::new();
    loop {
        print!("you> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }

        let mut ctx = AgentContext::new(leader_services.clone());
        ctx.transcript = transcript.clone();
        ctx.transcript.push(Message::user(line.to_string()));

        match (ReActLoop { max_steps: 8 }).run(&leader_agent as &dyn condesate::Agent, &mut ctx).await {
            Ok(outcome) => {
                println!("leader> {}\n", outcome.final_text);
                transcript.push(Message::user(line.to_string()));
                transcript.push(Message::assistant(outcome.final_text));
            }
            Err(e) => eprintln!("leader> error: {e}\n"),
        }
    }

    if registry.lock().await.is_some() {
        eprintln!("[leader] cleaning up: terminating the search agent");
        // The exact same tool call the leader itself would make — same
        // permission check, same effect, not a special-cased shutdown path.
        match terminate_tool.call(serde_json::json!({}), &leader_services).await {
            Ok(msg) => eprintln!("[leader] {msg}"),
            Err(e) => eprintln!("[leader] cleanup error: {e}"),
        }
    }

    println!("bye.");
    Ok(())
}
