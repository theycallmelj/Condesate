//! Picks the live model both agents share, from `PROVIDER`/`ANTHROPIC_API_KEY`/
//! `OPENAI_API_KEY`/`MODEL` — a `.env` file at the workspace root is loaded
//! automatically (see `main.rs`).
//!
//! Unlike `crates/evals`, there's no scripted offline fallback here: a
//! search agent that can't actually decide what to search for or read the
//! results isn't a meaningful demo, so this errors clearly instead of
//! silently degrading to a script that would never call the search tool for
//! a real reason.

use anyhow::{bail, Result};
use condesate::{AnthropicModel, ModelProvider, OpenAiModel, PromptedToolModel};
use std::sync::Arc;

pub fn choose_model() -> Result<(Arc<dyn ModelProvider>, String)> {
    let provider = std::env::var("PROVIDER").unwrap_or_default().to_lowercase();
    let model_name = std::env::var("MODEL").ok();

    match provider.as_str() {
        "anthropic" => {
            let name = model_name.unwrap_or_else(|| "claude-sonnet-4-6".to_string());
            let m = AnthropicModel::from_env(name.clone())?;
            Ok((Arc::new(PromptedToolModel::new(Arc::new(m))), format!("anthropic ({name})")))
        }
        "openai" => {
            let name = model_name.unwrap_or_else(|| "gpt-4o".to_string());
            let m = OpenAiModel::from_env(name.clone())?;
            Ok((Arc::new(PromptedToolModel::new(Arc::new(m))), format!("openai ({name})")))
        }
        _ => bail!(
            "no live model configured — set PROVIDER=anthropic|openai and the matching API key \
             (ANTHROPIC_API_KEY / OPENAI_API_KEY), e.g. in a .env file at the workspace root \
             (see .env.example)"
        ),
    }
}
