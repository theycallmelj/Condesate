//! The control loop: "how the loop works", made swappable.
//!
//! An `AgentLoop` drives an `Agent` to completion for one activation. Two impls
//! are provided:
//!   * `SingleShot` — think once, run any tools once, stop.
//!   * `ReActLoop`  — think / act / observe until the agent stops calling tools
//!                    (or a step budget is hit).
//!
//! Because this is a trait, you can drop in tree-search, a debate loop, a
//! human-in-the-loop gate, etc., without touching agents or harnesses.

use crate::agent::{Agent, AgentContext};
use crate::tool::Tool;
use crate::types::{Message, ToolCall};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// What a loop reports back to its harness.
#[derive(Clone, Debug, Default)]
pub struct LoopOutcome {
    /// Final assistant text.
    pub final_text: String,
    /// How many think-steps were taken.
    pub steps: usize,
}

#[async_trait]
pub trait AgentLoop: Send + Sync {
    async fn run(&self, agent: &dyn Agent, ctx: &mut AgentContext) -> Result<LoopOutcome>;
}

/// Execute every tool call in one assistant turn, appending observations.
async fn execute_tools(
    calls: &[ToolCall],
    tools: &[Arc<dyn Tool>],
    ctx: &mut AgentContext,
) -> Result<()> {
    for call in calls {
        let observation = match tools.iter().find(|t| t.spec().name == call.name) {
            Some(tool) => match tool.call(call.args.clone(), &ctx.services).await {
                Ok(out) => out,
                Err(e) => format!("tool '{}' error: {e}", call.name),
            },
            None => format!("no such tool: '{}'", call.name),
        };
        println!(
            "      ↳ tool {}({}) -> {observation}",
            call.name, call.args
        );
        ctx.transcript.push(Message::tool(observation));
    }
    Ok(())
}

/// One think, one round of tools, done.
pub struct SingleShot;

#[async_trait]
impl AgentLoop for SingleShot {
    async fn run(&self, agent: &dyn Agent, ctx: &mut AgentContext) -> Result<LoopOutcome> {
        let resp = agent.think(ctx).await?;
        ctx.transcript.push(Message::assistant(resp.content.clone()));
        execute_tools(&resp.tool_calls, agent.tools(), ctx).await?;
        Ok(LoopOutcome { final_text: resp.content, steps: 1 })
    }
}

/// Think -> act -> observe until the agent emits no tool calls.
pub struct ReActLoop {
    pub max_steps: usize,
}

impl Default for ReActLoop {
    fn default() -> Self {
        Self { max_steps: 8 }
    }
}

#[async_trait]
impl AgentLoop for ReActLoop {
    async fn run(&self, agent: &dyn Agent, ctx: &mut AgentContext) -> Result<LoopOutcome> {
        let mut last_text = String::new();
        let mut steps = 0;

        while steps < self.max_steps {
            steps += 1;
            let resp = agent.think(ctx).await?;
            last_text = resp.content.clone();
            println!("      · step {steps}: {}", resp.content);
            ctx.transcript.push(Message::assistant(resp.content));

            if resp.tool_calls.is_empty() {
                break; // agent is satisfied
            }
            execute_tools(&resp.tool_calls, agent.tools(), ctx).await?;
        }

        Ok(LoopOutcome { final_text: last_text, steps })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentContext, BasicAgent};
    use crate::bus::Bus;
    use crate::model::ModelProvider;
    use crate::service::ServiceHandle;
    use crate::storage::InMemoryStorage;
    use crate::tool::WordCount;
    use crate::types::{CompletionRequest, CompletionResponse, HarnessId, Role, ToolCall};
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;

    /// Calls `word_count` once, then finishes — enough to exercise a loop.
    struct ScriptedModel;

    #[async_trait]
    impl ModelProvider for ScriptedModel {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
            let turns = req.messages.iter().filter(|m| m.role == Role::Assistant).count();
            Ok(if turns == 0 {
                CompletionResponse {
                    content: "counting".into(),
                    tool_calls: vec![ToolCall {
                        name: "word_count".into(),
                        args: json!({ "text": "a b c" }),
                    }],
                }
            } else {
                CompletionResponse { content: "done".into(), tool_calls: vec![] }
            })
        }
    }

    fn ctx() -> AgentContext {
        let svc = ServiceHandle {
            me: HarnessId::new("t"),
            roster: std::sync::Arc::new(vec![HarnessId::new("t")]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(HashMap::new()),
        };
        AgentContext::new(svc)
    }

    fn agent() -> BasicAgent {
        BasicAgent::new(
            "t",
            "system",
            std::sync::Arc::new(ScriptedModel),
            vec![std::sync::Arc::new(WordCount)],
        )
    }

    #[tokio::test]
    async fn react_loop_runs_tool_then_stops() {
        let a = agent();
        let mut c = ctx();
        let out = ReActLoop::default().run(&a, &mut c).await.unwrap();
        assert_eq!(out.steps, 2, "one tool step, one finishing step");
        assert_eq!(out.final_text, "done");
        // transcript should contain a Tool observation equal to "3"
        let obs = c
            .transcript
            .iter()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.content.clone());
        assert_eq!(obs.as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn single_shot_runs_exactly_one_think() {
        let a = agent();
        let mut c = ctx();
        let out = SingleShot.run(&a, &mut c).await.unwrap();
        assert_eq!(out.steps, 1);
        // it executed the tool once but did not think again
        assert_eq!(c.transcript.iter().filter(|m| m.role == Role::Assistant).count(), 1);
    }

    #[tokio::test]
    async fn react_loop_respects_step_budget() {
        // A model that never stops calling a (missing) tool should hit the cap.
        struct Loopy;
        #[async_trait]
        impl ModelProvider for Loopy {
            fn name(&self) -> &str { "loopy" }
            async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
                Ok(CompletionResponse {
                    content: "again".into(),
                    tool_calls: vec![ToolCall { name: "nope".into(), args: json!({}) }],
                })
            }
        }
        let a = BasicAgent::new("t", "s", std::sync::Arc::new(Loopy), vec![]);
        let mut c = ctx();
        let out = ReActLoop { max_steps: 3 }.run(&a, &mut c).await.unwrap();
        assert_eq!(out.steps, 3);
    }
}
