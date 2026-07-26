//! End-to-end test: boot a real two-harness swarm with the built-in mock
//! models and assert the observable outcome (shared storage) after it halts.
//!
//! This exercises every layer together: ModelProvider -> Agent -> AgentLoop ->
//! Harness -> Swarm, plus the Bus (delegation + reply + shutdown) and Storage.

use std::sync::Arc;

use ai_swarm::{
    Agent, BasicAgent, CloudModel, InMemoryStorage, LocalModel, ReActLoop, Remember, SendMessage,
    ShutdownSwarm, StandardHarness, Storage, Swarm, Tool, WordCount,
};

fn build_swarm(storage: Arc<InMemoryStorage>) -> Swarm {
    let mut swarm = Swarm::new(storage);

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

    swarm.register(Box::new(worker));
    swarm.register(Box::new(planner));
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
