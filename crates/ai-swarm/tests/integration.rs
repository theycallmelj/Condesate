//! End-to-end test: boot a real two-harness swarm with the built-in mock
//! models and assert the observable outcome (shared storage) after it halts.
//!
//! This exercises every layer together: ModelProvider -> Agent -> AgentLoop ->
//! Harness -> Swarm, plus the Bus (delegation + reply + shutdown) and Storage.

use std::sync::Arc;

use ai_swarm::{
    Action, Agent, AgentManifest, BasicAgent, CloudModel, GrantSet, HarnessId, InMemoryStorage,
    Kernel, LocalModel, MemoryAudit, ModelClass, Pattern, ReActLoop, Remember, ResourcePattern,
    Rule, RuleSetPolicy, SendMessage, ShutdownSwarm, StandardHarness, Storage, SubjectMatch,
    Swarm, SystemClock, TenantId, Tool, TrustTier, WordCount,
};

fn mock_class() -> ModelClass {
    ModelClass {
        provider: "mock".into(),
        family: "mock".into(),
        revision: "mock".into(),
        embedding_space: None,
        quantization: None,
    }
}

fn root_grants() -> GrantSet {
    GrantSet::new(vec![
        Rule::allow(
            "mem",
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

fn manifest(harness: &str, agent: &str, trust: TrustTier, needs_control: bool) -> AgentManifest {
    let mut requested = vec![
        Rule::allow(
            "mem",
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
        tenant: TenantId::new("test"),
        requested_trust: trust,
        requested,
        cache_classes: vec![],
    }
}

fn build_swarm(storage: Arc<InMemoryStorage>) -> Swarm {
    let kernel = Kernel::new(
        root_grants(),
        Arc::new(RuleSetPolicy::new()),
        MemoryAudit::new(),
        Arc::new(SystemClock),
    );
    let mut swarm = Swarm::new(storage, kernel);

    let worker_tools: Vec<Arc<dyn Tool>> = vec![Arc::new(WordCount), Arc::new(SendMessage)];
    let worker_agent: Box<dyn Agent> = Box::new(BasicAgent::new(
        "worker",
        "local worker",
        Arc::new(LocalModel::new("local-mock")),
        worker_tools,
    ));
    let worker = StandardHarness::new("worker-local", worker_agent, Box::new(ReActLoop::default()));

    let planner_tools: Vec<Arc<dyn Tool>> =
        vec![Arc::new(SendMessage), Arc::new(Remember), Arc::new(ShutdownSwarm)];
    let planner_agent: Box<dyn Agent> = Box::new(BasicAgent::new(
        "planner",
        "cloud planner",
        Arc::new(CloudModel::new("cloud-mock")),
        planner_tools,
    ));
    let planner = StandardHarness::new(
        "planner-cloud",
        planner_agent,
        Box::new(ReActLoop::default()),
    )
    .with_seed("Analyze the slogan 'ship fast stay safe' and record a verdict.");

    swarm.register(Box::new(worker), manifest("worker-local", "worker", TrustTier::Standard, false));
    swarm.register(
        Box::new(planner),
        manifest("planner-cloud", "planner", TrustTier::Privileged, true),
    );
    swarm
}

#[tokio::test]
async fn swarm_delegates_counts_and_records_verdict() {
    let storage = InMemoryStorage::new();
    let swarm = build_swarm(storage.clone());

    swarm.run().await.expect("swarm should run to completion");

    // The planner should have written exactly one verdict, derived from the
    // worker's word count of the 4-word slogan.
    let keys = storage.keys("").await.unwrap();
    assert_eq!(keys, vec!["slogan/verdict".to_string()]);

    let verdict = storage.get("slogan/verdict").await.unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("4 words — concise enough to ship")
    );
}

#[tokio::test]
async fn swarm_terminates_without_hanging() {
    // If shutdown propagation were broken this test would hang; the test
    // harness timeout would then flag it. Reaching the assert means all tasks
    // joined cleanly.
    let storage = InMemoryStorage::new();
    let swarm = build_swarm(storage.clone());
    swarm.run().await.unwrap();
    assert!(storage.get("slogan/verdict").await.unwrap().is_some());
}
