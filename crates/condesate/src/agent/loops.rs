//! The control loop: "how the loop works", made swappable.
//!
//! An `AgentLoop` drives an `Agent` to completion for one activation. Two impls
//! are provided:
//!   * `SingleShot` — think once, run any tools once, stop.
//!   * `ReActLoop`  — think / act / observe until the agent stops calling
//!     tools (or a step budget is hit).
//!
//! Because this is a trait, you can drop in tree-search, a debate loop, a
//! human-in-the-loop gate, etc., without touching agents or harnesses.

use super::core::{Agent, AgentContext};
use super::tool::Tool;
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
///
/// Each call is authorized before it runs — `authorize_tool` is the call-time
/// gate described in `crate::security::policy`, and it is checked here regardless of
/// whether the model was ever shown the tool at advertise time. A denial does
/// not error the loop out; it becomes a tool observation, same as any other
/// tool failure, so the agent can see *why* and adapt.
async fn execute_tools(
    calls: &[ToolCall],
    tools: &[Arc<dyn Tool>],
    ctx: &mut AgentContext,
) -> Result<()> {
    for call in calls {
        let observation = match tools.iter().find(|t| t.spec().name == call.name) {
            Some(tool) => match ctx.services.authorize_tool(&call.name).await {
                Ok(_obligations) => match tool.call(call.args.clone(), &ctx.services).await {
                    Ok(out) => out,
                    Err(e) => format!("tool '{}' error: {e}", call.name),
                },
                Err(refusal) => format!("tool '{}' denied: {refusal}", call.name),
            },
            None => format!("no such tool: '{}'", call.name),
        };
        // Gated on `ctx.trace` (default off) and, when shown, on stderr —
        // this is internal reasoning trace, not the agent's own reply, so it
        // must never land where a caller's user-facing output goes (e.g. a
        // REPL's `you>` / `leader>` lines on stdout), the same reason
        // `TracingAudit` mirrors to stderr instead of stdout.
        if ctx.trace {
            eprintln!(
                "      ↳ tool {}({}) -> {observation}",
                call.name, call.args
            );
        }
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
            if ctx.trace {
                eprintln!("      · step {steps}: {}", resp.content); // see the trace note in execute_tools
            }
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
    use super::super::core::{AgentContext, BasicAgent};
    use super::super::model::ModelProvider;
    use super::super::tool::WordCount;
    use super::*;
    use crate::security::audit::{FixedClock, MemoryAudit};
    use crate::security::identity::{ModelClass, TenantId, TrustTier};
    use crate::security::kernel::{AgentManifest, Kernel};
    use crate::security::policy::{
        Action, GrantSet, Pattern, ResourcePattern, Rule, RuleSetPolicy, SubjectMatch,
    };
    use crate::swarm::bus::Bus;
    use crate::swarm::service::ServiceHandle;
    use crate::swarm::storage::InMemoryStorage;
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

    /// A guarded context granting broad tool invocation, enough to exercise
    /// loop control-flow without itself being a policy test.
    fn ctx() -> AgentContext {
        let raw = ServiceHandle {
            me: HarnessId::new("t"),
            roster: std::sync::Arc::new(vec![HarnessId::new("t")]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(HashMap::new()),
        };
        let requested = vec![Rule::allow(
            "tools",
            SubjectMatch::agent("t"),
            &[Action::Invoke],
            ResourcePattern::Tool(Pattern::Any),
        )];
        let kernel = Kernel::new(
            GrantSet::new(requested.clone()),
            std::sync::Arc::new(RuleSetPolicy::new()),
            MemoryAudit::new(),
            std::sync::Arc::new(FixedClock(0)),
        );
        let manifest = AgentManifest {
            harness: HarnessId::new("t"),
            agent: "t".into(),
            model_class: ModelClass {
                provider: "test".into(),
                family: "test".into(),
                revision: "test".into(),
                embedding_space: None,
                quantization: None,
            },
            tenant: TenantId::new("test"),
            requested_trust: TrustTier::Privileged,
            requested,
            cache_classes: vec![],
        };
        let admission = kernel.admit(&manifest, None);
        let svc = kernel.attach(&admission, raw);
        svc.begin_activation("test", u64::MAX);
        AgentContext::new(std::sync::Arc::new(svc))
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

    #[tokio::test]
    async fn an_unauthorized_tool_call_becomes_a_denied_observation_not_a_crash() {
        // Same agent/tool as react_loop_runs_tool_then_stops, but the context
        // holds no Invoke grant at all — the call-time gate in
        // `execute_tools` should refuse the call and let the loop continue,
        // not error the whole activation out.
        let raw = ServiceHandle {
            me: HarnessId::new("t"),
            roster: std::sync::Arc::new(vec![HarnessId::new("t")]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(HashMap::new()),
        };
        let kernel = Kernel::new(
            GrantSet::default(),
            std::sync::Arc::new(RuleSetPolicy::new()),
            MemoryAudit::new(),
            std::sync::Arc::new(FixedClock(0)),
        );
        let manifest = AgentManifest {
            harness: HarnessId::new("t"),
            agent: "t".into(),
            model_class: ModelClass {
                provider: "test".into(),
                family: "test".into(),
                revision: "test".into(),
                embedding_space: None,
                quantization: None,
            },
            tenant: TenantId::new("test"),
            requested_trust: TrustTier::Privileged,
            requested: vec![],
            cache_classes: vec![],
        };
        let admission = kernel.admit(&manifest, None);
        let svc = kernel.attach(&admission, raw);
        svc.begin_activation("test", u64::MAX);
        let mut c = AgentContext::new(std::sync::Arc::new(svc));

        let a = agent();
        let out = ReActLoop::default().run(&a, &mut c).await.unwrap();
        assert_eq!(out.steps, 2, "the loop still completes rather than aborting");

        let obs = c
            .transcript
            .iter()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .unwrap();
        assert!(obs.contains("denied"), "{obs}");
    }
}
