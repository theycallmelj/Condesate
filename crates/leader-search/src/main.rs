//! A two-agent demo built on `condesate`: a **leader** that can dynamically
//! spawn and terminate a **search** agent, which does real web searches
//! through a real MCP server (`mcp-duckduckgo`, no API key needed). Both
//! agents share the same live model, picked from `.env` (see `model.rs`).
//!
//! The interesting part isn't the chat loop — it's that spawning the search
//! agent goes through `condesate::GuardedServices::spawn_child`: a real,
//! policy-checked admission that gives the child an *attenuated* copy of
//! the leader's own authority (here: three specific MCP tools and nothing
//! else — not `Action::Spawn`, so the search agent can't spawn its own
//! children; not the leader's other tools). See `search_agent.rs` for the
//! tools that drive it (and why one real mcp-duckduckgo tool is
//! deliberately excluded) and `docs/` (workspace root) for more. stdout is
//! pure chat by default; set `VERBOSE=1` to see every check either agent
//! makes live on stderr, and/or `AUDIT_LOG_PATH` to append them to a file —
//! see the README's "Where's the audit log" section.
//!
//! Run: `cargo run -p leader-search` (needs PROVIDER + an API key, e.g. in
//! `.env`, and Node.js 20+ for the search agent's MCP server).

mod model;
mod search_agent;

use anyhow::Result;
use condesate::{
    run_repl, Action, AgentManifest, AuditSink, BasicAgent, Bus, FileAudit, GrantSet,
    GuardedServices, HarnessId, InMemoryStorage, Kernel, MemoryAudit, ModelClass, Pattern,
    ReActLoop, ReplOnError, ReplOptions, ResourcePattern, Rule, RuleSetPolicy, ServiceHandle,
    Storage, SubjectMatch, SystemClock, TenantId, Tool, ToolSpec, TracingAudit, TrustTier,
};
use search_agent::{AskSearchAgent, ListAgents, Registry, SpawnSearchAgent, TerminateSearchAgent};
use std::collections::HashMap;
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

/// Builds the leader's `GuardedServices` and returns the concrete
/// `MemoryAudit` sink alongside it. The `Kernel` (and so the audit trail) is
/// shared with every child `spawn_child` admits from this handle — the
/// search agent's own checks land in the *same* sink, not a separate one —
/// so keeping this one handle is enough to see both principals' history.
///
/// Every record is captured in `audit` (queryable via `.events()`) no matter
/// what. Two more sinks are opt-in on top of that: `verbose` wraps it in
/// `TracingAudit` (live stderr echo, one line per check as it happens); an
/// `AUDIT_LOG_PATH` env var wraps it in `FileAudit` (durable across process
/// restarts, unlike stderr or the in-memory sink). Neither is on by default —
/// a normal run's stderr carries only a handful of one-line diagnostics.
async fn build_leader_services(
    storage: Arc<dyn Storage>,
    verbose: bool,
) -> Result<(Arc<GuardedServices>, Arc<MemoryAudit>)> {
    let grants = root_grants();
    let audit = MemoryAudit::new();
    let mut live_audit: Arc<dyn AuditSink> = audit.clone();
    if verbose {
        live_audit = Arc::new(TracingAudit { inner: live_audit });
    }
    if let Ok(path) = std::env::var("AUDIT_LOG_PATH") {
        eprintln!("[leader] audit trail: also appending to {path}");
        live_audit = Arc::new(FileAudit::open(&path, live_audit).await?);
    }
    let kernel = Kernel::new(GrantSet::new(grants.clone()), Arc::new(RuleSetPolicy::new()), live_audit, Arc::new(SystemClock));
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
    Ok((Arc::new(services), audit))
}

fn leader_system_prompt(tool_specs: &[ToolSpec]) -> String {
    format!(
        "You are the leader of a small two-agent system. You have no web search of your own — a \
         separate search agent does, over a real connection to a search server. Rules:\n\
         - If the user asks something that needs current or external information, call \
           spawn_search_agent (skip this if it's already running), then call ask_search_agent \
           with one focused query, then answer the user using what it found. The search agent \
           remembers your conversation with it — if its answer is thin or you need more detail \
           on something it mentioned, call ask_search_agent again with a follow-up instead of \
           re-asking the same question from scratch.\n\
         - When you're done needing web search for this conversation (e.g. the user says thanks, \
           changes topic to something you can answer yourself, or says goodbye), call \
           terminate_search_agent.\n\
         - For anything you can just answer (simple facts, conversation, math), answer directly — \
           don't spawn the search agent needlessly.\n\
         - If asked what agents are running, call list_agents.\n\n{}",
        condesate::tool_instructions(tool_specs)
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    // Quiet by default: stdout is pure chat, stderr carries only a few
    // one-line diagnostics. Set VERBOSE=1 to also see each agent's think
    // steps and tool calls, the live per-check audit trail, and the search
    // MCP server's own startup log — all on stderr, none of it on stdout.
    let verbose = std::env::var("VERBOSE").is_ok();

    let (model, label) = model::choose_model()?;
    eprintln!("[leader] model: {label}");
    if !verbose {
        eprintln!("[leader] quiet mode — set VERBOSE=1 to see reasoning + live audit on stderr");
    }

    // One storage backend shared by both principals, reached through two
    // separately-permissioned guards — see `search_agent.rs` for why.
    let storage: Arc<dyn Storage> = InMemoryStorage::new();

    let registry: Registry = Arc::new(tokio::sync::Mutex::new(None));
    let terminate_tool = Arc::new(TerminateSearchAgent { registry: registry.clone() });
    let leader_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SpawnSearchAgent { registry: registry.clone(), storage: storage.clone(), verbose }),
        Arc::new(AskSearchAgent { registry: registry.clone() }),
        terminate_tool.clone(),
        Arc::new(ListAgents),
    ];
    let specs: Vec<ToolSpec> = leader_tools.iter().map(|t| t.spec()).collect();
    let leader_agent = BasicAgent::new(LEADER_ID, leader_system_prompt(&specs), model, leader_tools);
    let (leader_services, audit) = build_leader_services(storage, verbose).await?;
    if verbose {
        eprintln!("[leader] audit trail: live on stderr as `audit ...` lines below, from here on\n");
    }

    run_repl(
        &leader_agent as &dyn condesate::Agent,
        &ReActLoop { max_steps: 8 },
        leader_services.clone(),
        std::io::stdin().lock(),
        ReplOptions {
            greeting: "leader-search ready — ask a question, or 'exit' to quit.".into(),
            reply_prefix: "leader> ".into(),
            // A bad turn (a model hiccup, a denied call) shouldn't kill the
            // whole session while the search agent is still running under
            // it — print it and let the user try again, same as before.
            on_error: ReplOnError::Continue,
            trace: verbose,
        },
    )
    .await?;

    if registry.lock().await.is_some() {
        eprintln!("[leader] cleaning up: terminating the search agent");
        // The exact same tool call the leader itself would make — same
        // permission check, same effect, not a special-cased shutdown path.
        match terminate_tool.call(serde_json::json!({}), &leader_services).await {
            Ok(msg) => eprintln!("[leader] {msg}"),
            Err(e) => eprintln!("[leader] cleanup error: {e}"),
        }
    }

    let events = audit.events().await;
    let denials = audit.refusals().await;
    // stderr, with the rest of the audit trail — stdout stays pure chat
    // (greeting, `you>`/`leader>` lines, `bye.`), nothing else.
    eprintln!("[audit] {} boundary crossing(s) checked this run, {} denied", events.len(), denials.len());

    println!("bye.");
    Ok(())
}
