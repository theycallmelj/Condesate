//! Real network model providers (compiled only with `--features remote`).
//!
//! `AnthropicModel` and `OpenAiModel` implement `ModelProvider` by POSTing to
//! each vendor's API. All the request/response *mapping* lives in `crate::wire`
//! (pure, always compiled, unit-tested); this file is just the HTTP plumbing.
//!
//! NOTE: this module pulls in `reqwest`. Modern crates in that tree require a
//! recent Rust toolchain (edition 2024). It builds fine on current stable; it
//! will not build on very old toolchains. The offline parts of the project do
//! not depend on it.

use crate::model::ModelProvider;
use crate::types::{CompletionRequest, CompletionResponse};
use crate::wire;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

// --- Anthropic --------------------------------------------------------------

pub struct AnthropicModel {
    model: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl AnthropicModel {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            max_tokens: 1024,
            client: reqwest::Client::new(),
        }
    }

    /// Read the key from `ANTHROPIC_API_KEY`.
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        Ok(Self::new(model, key))
    }
}

#[async_trait]
impl ModelProvider for AnthropicModel {
    fn name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let body = wire::build_anthropic_body(&self.model, self.max_tokens, &req);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("anthropic request failed")?;
        let value: Value = resp.json().await.context("anthropic bad json")?;
        wire::parse_anthropic_response(&value)
    }
}

// --- OpenAI -----------------------------------------------------------------

pub struct OpenAiModel {
    model: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiModel {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            base_url: "https://api.openai.com".into(),
            client: reqwest::Client::new(),
        }
    }

    /// Read the key from `OPENAI_API_KEY`.
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
        Ok(Self::new(model, key))
    }
}

#[async_trait]
impl ModelProvider for OpenAiModel {
    fn name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let body = wire::build_openai_body(&self.model, &req);
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("openai request failed")?;
        let value: Value = resp.json().await.context("openai bad json")?;
        wire::parse_openai_response(&value)
    }
}
