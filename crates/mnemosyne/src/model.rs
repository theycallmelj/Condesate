//! Picks a live model from `PROVIDER`/`ANTHROPIC_API_KEY`/`OPENAI_API_KEY` — a
//! `.env` file at the workspace root is loaded automatically (see `main.rs`).
//! Unlike `leader-search`'s single shared model, mnemosyne and morpheus each
//! need their *own* model name (Opus vs. Sonnet), so this takes the name as
//! an argument instead of reading one fixed `MODEL` env var.
//!
//! No offline fallback, for the same reason as `leader-search`: an agent that
//! can't actually think isn't a meaningful demo of either the memory hand-off
//! or the context-reset behavior.

use anyhow::{bail, Result};
use condesate::{AnthropicModel, ModelProvider, OpenAiModel, PromptedToolModel};
use std::sync::Arc;

pub fn choose_model(model_name: &str) -> Result<(Arc<dyn ModelProvider>, String)> {
    let provider = std::env::var("PROVIDER").unwrap_or_default().to_lowercase();

    match provider.as_str() {
        "anthropic" => {
            let m = AnthropicModel::from_env(model_name.to_string())?;
            Ok((Arc::new(PromptedToolModel::new(Arc::new(m))), format!("anthropic ({model_name})")))
        }
        "openai" => {
            let m = OpenAiModel::from_env(model_name.to_string())?;
            Ok((Arc::new(PromptedToolModel::new(Arc::new(m))), format!("openai ({model_name})")))
        }
        _ => bail!(
            "no live model configured — set PROVIDER=anthropic|openai and the matching API key \
             (ANTHROPIC_API_KEY / OPENAI_API_KEY), e.g. in a .env file at the workspace root \
             (see .env.example)"
        ),
    }
}
