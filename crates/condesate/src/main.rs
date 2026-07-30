//! Demo: a two-harness swarm.
//!
//! Story it acts out (fully offline, deterministic mock models):
//!   1. `planner-cloud` (CloudModel) is seeded with a task.
//!   2. It delegates a word-count to `worker-local` over the bus.
//!   3. `worker-local` (LocalModel) runs a tool, replies with the count.
//!   4. `planner-cloud` records a verdict in shared storage, then broadcasts
//!      a swarm-wide shutdown.
//!   5. `main` reads the verdict back out of shared storage.
//!
//! This touches every abstraction: local vs cloud model, the ReAct loop, tools
//! (compute / storage / messaging), inter-harness IPC, and OS-like scheduling.

use std::sync::Arc;

use condesate::{
    Action, Agent, AgentManifest, BasicAgent, CloudModel, GrantSet, HarnessId, Kernel, LocalModel,
    MemoryAudit, ModelClass, Pattern, ReActLoop, Remember, ResourcePattern, Rule, RuleSetPolicy,
    SendMessage, ShutdownSwarm, StandardHarness, Storage, SubjectMatch, Swarm, SystemClock,
    TenantId, Tool, TrustTier, WordCount,
};

fn boxed_agent(a: BasicAgent) -> Box<dyn Agent> {
    Box::new(a)
}

/// A stand-in model class: the demo's mocks aren't a real serving revision, so
/// this is just what every harness here declares itself as for cache-pool and
/// audit purposes.
fn mock_class() -> ModelClass {
    ModelClass {
        provider: "mock".into(),
        family: "mock".into(),
        revision: "mock".into(),
        embedding_space: None,
        quantization: None,
    }
}

/// The swarm operator's root authority: everything a harness in this demo may
/// ever hold, before any manifest narrows it further. Memory is scoped per
/// harness (`scratch/<id>/*`) plus a shared results namespace both may write;
/// messaging and tool invocation are open; shutdown requires `Privileged`.
fn root_grants() -> GrantSet {
    GrantSet::new(vec![
        Rule::allow(
            "mem-scratch",
            SubjectMatch::default(),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("scratch/*")),
        ),
        Rule::allow(
            "mem-shared",
            SubjectMatch::default(),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("slogan/*")),
        ),
        Rule::allow(
            "talk",
            SubjectMatch::default(),
            &[Action::Send],
            ResourcePattern::Peer(Pattern::Any),
        ),
        Rule::allow(
            "tools",
            SubjectMatch::default(),
            &[Action::Invoke],
            ResourcePattern::Tool(Pattern::Any),
        ),
        Rule::allow(
            "shutdown",
            SubjectMatch { min_trust: Some(TrustTier::Privileged), ..Default::default() },
            &[Action::Control],
            ResourcePattern::Swarm,
        ),
    ])
}

/// A manifest requesting the subset of `root_grants()` this demo's harnesses
/// need, scoped to `agent`. Every manifest must declare rules that are
/// actually covered by the authority above it — see `AgentManifest`'s docs —
/// there is no implicit "top-level agents get everything" path.
fn manifest(harness: &str, agent: &str, trust: TrustTier, needs_control: bool) -> AgentManifest {
    // Requested rules must be covered *exactly enough* by root_grants(): a
    // request for `Memory(Any)` would not be contained by root's narrower
    // `scratch/*` / `slogan/*` prefixes, so it mirrors their shape instead of
    // asking for the world and hoping.
    let mut requested = vec![
        Rule::allow(
            "mem-scratch",
            SubjectMatch::agent(agent),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("scratch/*")),
        ),
        Rule::allow(
            "mem-shared",
            SubjectMatch::agent(agent),
            &[Action::Read, Action::Write],
            ResourcePattern::Memory(Pattern::parse("slogan/*")),
        ),
        Rule::allow(
            "talk",
            SubjectMatch::agent(agent),
            &[Action::Send],
            ResourcePattern::Peer(Pattern::Any),
        ),
        Rule::allow(
            "tools",
            SubjectMatch::agent(agent),
            &[Action::Invoke],
            ResourcePattern::Tool(Pattern::Any),
        ),
    ];
    if needs_control {
        requested.push(Rule::allow(
            "control",
            SubjectMatch {
                agent: Pattern::parse(agent),
                min_trust: Some(TrustTier::Privileged),
                ..Default::default()
            },
            &[Action::Control],
            ResourcePattern::Swarm,
        ));
    }
    AgentManifest {
        harness: HarnessId::new(harness),
        agent: agent.to_string(),
        model_class: mock_class(),
        tenant: TenantId::new("demo"),
        requested_trust: trust,
        requested,
        cache_classes: vec![],
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- shared services --------------------------------------------------
    let storage = condesate::InMemoryStorage::new();
    let kernel = Kernel::new(
        root_grants(),
        Arc::new(RuleSetPolicy::new()),
        MemoryAudit::new(),
        Arc::new(SystemClock),
    );
    let mut swarm = Swarm::new(storage.clone(), kernel);

    // --- worker: LOCAL model ---------------------------------------------
    let worker_tools: Vec<Arc<dyn Tool>> =
        vec![Arc::new(WordCount), Arc::new(SendMessage)];
    let worker_agent = BasicAgent::new(
        "worker",
        "You are a local worker. Use tools to answer, then report back.",
        Arc::new(LocalModel::new("local-llama-mock")),
        worker_tools,
    );
    let worker = StandardHarness::new(
        "worker-local",
        boxed_agent(worker_agent),
        Box::new(ReActLoop::default()),
    );

    // --- planner: CLOUD model --------------------------------------------
    let planner_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SendMessage),
        Arc::new(Remember),
        Arc::new(ShutdownSwarm),
    ];
    let planner_agent = BasicAgent::new(
        "planner",
        "You are a cloud planner. Delegate work, record verdicts, then stop.",
        Arc::new(CloudModel::new("cloud-sonnet-mock")),
        planner_tools,
    );
    let planner = StandardHarness::new(
        "planner-cloud",
        boxed_agent(planner_agent),
        Box::new(ReActLoop::default()),
    )
    .with_seed("Analyze the slogan 'ship fast stay safe' and record a verdict.");

    // Registration order doesn't matter; the bus is fully wired before boot.
    swarm.register(Box::new(worker), manifest("worker-local", "worker", TrustTier::Standard, false));
    swarm.register(
        Box::new(planner),
        manifest("planner-cloud", "planner", TrustTier::Privileged, true),
    );

    // --- run to completion ------------------------------------------------
    swarm.run().await?;

    // --- inspect shared storage afterwards -------------------------------
    println!("\n─── shared storage dump ───");
    for key in storage.keys("").await? {
        let val = storage.get(&key).await?.unwrap_or_default();
        println!("  {key} = {val}");
    }

    Ok(())
}
