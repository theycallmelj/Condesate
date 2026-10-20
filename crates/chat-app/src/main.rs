//! A minimal terminal chat app built on the `condesate` library.
//!
//! Demonstrates the consumer pattern: pick a `ModelProvider`, wrap it in a
//! `BasicAgent`, and drive it turn-by-turn with `condesate::run_repl`, which
//! owns the stdin loop and the persistent transcript.
//!
//! Offline by default (an echo provider) so it always runs. Build with the
//! `remote` feature for real OpenAI / Anthropic connectivity:
//!
//!   cargo run -p chat-app --features remote
//!     with e.g. PROVIDER=anthropic ANTHROPIC_API_KEY=sk-... MODEL=claude-sonnet-4-6
//!          or   PROVIDER=openai    OPENAI_API_KEY=sk-...    MODEL=gpt-4o
//!
//! Type a message and press enter; `exit` / `quit` / Ctrl-D leaves.

use std::sync::Arc;

use condesate::{
    run_repl, Agent, AgentManifest, BasicAgent, Bus, CompletionRequest, CompletionResponse,
    GrantSet, HarnessId, Kernel, MemoryAudit, ModelClass, ModelProvider, ReplOnError, ReplOptions,
    Role, RuleSetPolicy, ServiceHandle, SingleShot, Storage, SystemClock, TenantId, TrustTier,
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
    use condesate::{AnthropicModel, OpenAiModel};
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

/// Build a minimal guarded handle for the solo chat harness. The basic chat
/// flow calls no tools and touches no shared storage, so the manifest asks for
/// nothing beyond existing — every real syscall would still be denied and
/// audited, same as any other principal in the swarm.
fn solo_services() -> Arc<condesate::GuardedServices> {
    let raw = ServiceHandle {
        me: HarnessId::new("chat"),
        roster: Arc::new(vec![HarnessId::new("chat")]),
        storage: Arc::new(NullStorage),
        bus: Bus::new(std::collections::HashMap::new()),
    };
    let kernel = Kernel::new(
        GrantSet::default(),
        Arc::new(RuleSetPolicy::new()),
        MemoryAudit::new(),
        Arc::new(SystemClock),
    );
    let manifest = AgentManifest {
        harness: HarnessId::new("chat"),
        agent: "assistant".into(),
        model_class: ModelClass {
            provider: "chat-app".into(),
            family: "chat-app".into(),
            revision: "chat-app".into(),
            embedding_space: None,
            quantization: None,
        },
        tenant: TenantId::new("local"),
        requested_trust: TrustTier::Standard,
        requested: vec![],
        cache_classes: vec![],
    };
    let admission = kernel.admit(&manifest, None);
    let services = kernel.attach(&admission, raw);
    services.begin_activation("chat", 0);
    Arc::new(services)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Loads `.env` from the current or an ancestor directory if present;
    // a no-op (not an error) when there isn't one, so this works whether
    // you export PROVIDER/ANTHROPIC_API_KEY/OPENAI_API_KEY/MODEL yourself
    // or keep them in `.env` (see `.env.example`).
    dotenvy::dotenv().ok();

    let provider = choose_provider();
    let agent = BasicAgent::new(
        "assistant",
        "You are a concise, friendly assistant.",
        provider,
        vec![], // no tools in the basic chat flow
    );
    let services = solo_services();

    run_repl(
        &agent as &dyn Agent,
        &SingleShot,
        services,
        std::io::stdin().lock(),
        ReplOptions {
            greeting: "chat ready — type a message, or 'exit' to quit.".into(),
            reply_prefix: "bot> ".into(),
            on_error: ReplOnError::Abort,
            // No tools in the basic chat flow, so there'd be nothing to
            // trace anyway.
            trace: false,
        },
    )
    .await?;

    println!("bye.");
    Ok(())
}
