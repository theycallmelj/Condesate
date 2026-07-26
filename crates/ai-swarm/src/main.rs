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

use ai_swarm::{
    Agent, BasicAgent, CloudModel, LocalModel, ReActLoop, Remember, SendMessage, ShutdownSwarm,
    StandardHarness, Storage, Swarm, Tool, WordCount,
};

fn boxed_agent(a: BasicAgent) -> Box<dyn Agent> {
    Box::new(a)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- shared services --------------------------------------------------
    let storage = ai_swarm::InMemoryStorage::new();
    let mut swarm = Swarm::new(storage.clone());

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
    swarm.register(Box::new(worker));
    swarm.register(Box::new(planner));

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
