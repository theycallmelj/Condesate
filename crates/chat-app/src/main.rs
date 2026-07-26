//! A minimal terminal chat app built on the `ai-swarm` library.
//!
//! Demonstrates the consumer pattern: pick a `ModelProvider`, wrap it in a
//! `BasicAgent`, and drive a `SingleShot` loop turn-by-turn while keeping a
//! persistent transcript.
//!
//! Offline by default (an echo provider) so it always runs. Build with the
//! `remote` feature for real OpenAI / Anthropic connectivity:
//!
//!   cargo run -p chat-app --features remote
//!     with e.g. PROVIDER=anthropic ANTHROPIC_API_KEY=sk-... MODEL=claude-sonnet-4-6
//!          or   PROVIDER=openai    OPENAI_API_KEY=sk-...    MODEL=gpt-4o
//!
//! Type a message and press enter; `exit` / `quit` / Ctrl-D leaves.

use std::io::Write;
use std::sync::Arc;

use ai_swarm::{
    Agent, AgentContext, AgentLoop, BasicAgent, Bus, CompletionRequest, CompletionResponse,
    HarnessId, Message, ModelProvider, Role, ServiceHandle, SingleShot, Storage,
};
use anyhow::Result;
use async_trait::async_trait;

/// Offline fallback provider so the app runs without any API key or the
/// `remote` feature.
struct EchoModel;

#[async_trait]
impl ModelProvider for EchoModel {
    fn name(&self) -> &str {
        "echo-offline"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let last = req
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User))
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(CompletionResponse {
            content: format!("(offline echo) you said: {last}"),
            tool_calls: vec![],
        })
    }
}

/// A no-op storage so we can build a `ServiceHandle` for the loop. Plain chat
/// needs no shared storage, but the loop's tool context requires the handle.
struct NullStorage;

#[async_trait]
impl Storage for NullStorage {
    async fn get(&self, _k: &str) -> Result<Option<String>> {
        Ok(None)
    }
    async fn set(&self, _k: &str, _v: &str) -> Result<()> {
        Ok(())
    }
    async fn keys(&self, _p: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

/// Provider selection when the `remote` feature is ON.
#[cfg(feature = "remote")]
fn choose_provider() -> Arc<dyn ModelProvider> {
    use ai_swarm::{AnthropicModel, OpenAiModel};
    let provider = std::env::var("PROVIDER").unwrap_or_default().to_lowercase();
    let model = std::env::var("MODEL").ok();

    match provider.as_str() {
        "anthropic" => {
            match AnthropicModel::from_env(model.unwrap_or_else(|| "claude-sonnet-4-6".into())) {
                Ok(m) => {
                    eprintln!("[using Anthropic]");
                    Arc::new(m)
                }
                Err(e) => {
                    eprintln!("[anthropic unavailable: {e}; using offline echo]");
                    Arc::new(EchoModel)
                }
            }
        }
        "openai" => match OpenAiModel::from_env(model.unwrap_or_else(|| "gpt-4o".into())) {
            Ok(m) => {
                eprintln!("[using OpenAI]");
                Arc::new(m)
            }
            Err(e) => {
                eprintln!("[openai unavailable: {e}; using offline echo]");
                Arc::new(EchoModel)
            }
        },
        _ => {
            eprintln!("[set PROVIDER=anthropic|openai to go live; using offline echo]");
            Arc::new(EchoModel)
        }
    }
}

/// Provider selection when the `remote` feature is OFF: always offline.
#[cfg(not(feature = "remote"))]
fn choose_provider() -> Arc<dyn ModelProvider> {
    eprintln!("[offline echo build; rebuild with `--features remote` for OpenAI/Anthropic]");
    Arc::new(EchoModel)
}

/// Build a minimal `ServiceHandle` (chat needs no bus peers or storage).
fn solo_services() -> ServiceHandle {
    ServiceHandle {
        me: HarnessId::new("chat"),
        roster: Arc::new(vec![HarnessId::new("chat")]),
        storage: Arc::new(NullStorage),
        bus: Bus::new(std::collections::HashMap::new()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let provider = choose_provider();
    let agent = BasicAgent::new(
        "assistant",
        "You are a concise, friendly assistant.",
        provider,
        vec![], // no tools in the basic chat flow
    );
    let loop_strategy = SingleShot;
    let services = solo_services();

    // Persistent transcript across turns — the "chat flow" state.
    let mut transcript: Vec<Message> = Vec::new();

    println!("chat ready — type a message, or 'exit' to quit.\n");
    let stdin = std::io::stdin();
    loop {
        print!("you> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        let n = stdin.read_line(&mut line)?;
        if n == 0 {
            break; // EOF (Ctrl-D)
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }

        // One turn: fresh context seeded with the running transcript + the new
        // user message; the loop produces a reply; then we persist both.
        let mut ctx = AgentContext::new(services.clone());
        ctx.transcript = transcript.clone();
        ctx.transcript.push(Message::user(line.to_string()));

        let outcome = loop_strategy.run(&agent as &dyn Agent, &mut ctx).await?;
        println!("bot> {}\n", outcome.final_text);

        transcript.push(Message::user(line.to_string()));
        transcript.push(Message::assistant(outcome.final_text));
    }

    println!("bye.");
    Ok(())
}
