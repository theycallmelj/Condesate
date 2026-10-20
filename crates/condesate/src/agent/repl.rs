//! A minimal turn-by-turn console driver.
//!
//! `chat-app` and `leader-search` had each independently grown the exact
//! same ~25-line stdin loop: read a line, thread it through a persistent
//! transcript, run one [`AgentLoop`] turn, print the reply, repeat until
//! `exit`/`quit`/EOF. [`run_repl`] is that loop, factored out once the
//! duplication was real rather than hypothetical.
//!
//! What varies between consumers stays with them — this owns only the loop
//! itself, not what happens before it (building `services`, choosing a
//! model) or after (cleanup, an audit summary, ...). [`ReplOptions`] covers
//! the two things that actually differ today: the greeting/reply labels, and
//! whether a turn's own error aborts the whole REPL or gets printed and
//! skipped (`leader-search` wants the latter, so one bad turn doesn't kill a
//! session with a live child agent still running).

use super::core::{Agent, AgentContext};
use super::loops::AgentLoop;
use crate::security::kernel::GuardedServices;
use crate::types::Message;
use anyhow::Result;
use std::io::{BufRead, Write};
use std::sync::Arc;

/// What to do when a turn itself errors (model/provider failure — a denied
/// or failing tool call already surfaces as an observation inside the
/// transcript, not as an `Err` here).
pub enum ReplOnError {
    /// Return the error immediately, ending the REPL without printing a
    /// reply for that turn.
    Abort,
    /// Print `{reply_prefix}error: ...` and keep going. The failed turn is
    /// not added to the transcript, so the next turn doesn't see it.
    Continue,
}

pub struct ReplOptions {
    /// Printed once, before the first prompt.
    pub greeting: String,
    /// Printed before each reply, e.g. `"bot> "` or `"leader> "`.
    pub reply_prefix: String,
    pub on_error: ReplOnError,
    /// Forwarded to each turn's [`AgentContext::trace`] — off by default
    /// upstream, so leave this `false` unless the caller wants the
    /// underlying `AgentLoop`'s internal reasoning echoed to stderr.
    pub trace: bool,
}

/// Runs the loop until `input` hits EOF or the user types `exit`/`quit`.
/// Blank lines are ignored. Each accepted line becomes one turn: a fresh
/// [`AgentContext`] seeded with the running transcript, one `loop_strategy`
/// run, then — on success — both the user line and the reply appended back
/// to the transcript for the next turn.
///
/// `input` is generic over `BufRead` (not hardcoded to `stdin()`) so this is
/// exercisable in tests against an in-memory buffer; real callers pass
/// `std::io::stdin().lock()`.
pub async fn run_repl<R: BufRead>(
    agent: &dyn Agent,
    loop_strategy: &dyn AgentLoop,
    services: Arc<GuardedServices>,
    mut input: R,
    opts: ReplOptions,
) -> Result<()> {
    println!("{}\n", opts.greeting);
    let mut transcript: Vec<Message> = Vec::new();

    loop {
        print!("you> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }

        let mut ctx = AgentContext::new(services.clone());
        ctx.trace = opts.trace;
        ctx.transcript = transcript.clone();
        ctx.transcript.push(Message::user(line.to_string()));

        match loop_strategy.run(agent, &mut ctx).await {
            Ok(outcome) => {
                println!("{}{}\n", opts.reply_prefix, outcome.final_text);
                transcript.push(Message::user(line.to_string()));
                transcript.push(Message::assistant(outcome.final_text));
            }
            Err(e) => match opts.on_error {
                ReplOnError::Abort => return Err(e),
                ReplOnError::Continue => eprintln!("{}error: {e}\n", opts.reply_prefix),
            },
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::core::BasicAgent;
    use super::super::model::ModelProvider;
    use crate::security::audit::{FixedClock, MemoryAudit};
    use crate::security::identity::{ModelClass, TenantId, TrustTier};
    use crate::security::kernel::{AgentManifest, Kernel};
    use crate::security::policy::GrantSet;
    use crate::swarm::bus::Bus;
    use crate::swarm::service::ServiceHandle;
    use crate::swarm::storage::InMemoryStorage;
    use crate::types::{CompletionRequest, CompletionResponse, HarnessId, Role};
    use async_trait::async_trait;
    use std::io::Cursor;

    /// Echoes the last user message back — enough to prove a turn round-trips
    /// through the transcript without needing a real or scripted tool-caller.
    struct EchoModel;

    #[async_trait]
    impl ModelProvider for EchoModel {
        fn name(&self) -> &str {
            "echo"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
            let last = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(CompletionResponse { content: format!("echo: {last}"), tool_calls: vec![] })
        }
    }

    /// Always errors — for exercising `ReplOnError`.
    struct FailingLoop;

    #[async_trait]
    impl AgentLoop for FailingLoop {
        async fn run(&self, _agent: &dyn Agent, _ctx: &mut AgentContext) -> Result<super::super::loops::LoopOutcome> {
            Err(anyhow::anyhow!("simulated turn failure"))
        }
    }

    fn services() -> Arc<GuardedServices> {
        let raw = ServiceHandle {
            me: HarnessId::new("t"),
            roster: Arc::new(vec![HarnessId::new("t")]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(std::collections::HashMap::new()),
        };
        let kernel = Kernel::new(
            GrantSet::default(),
            Arc::new(crate::security::policy::RuleSetPolicy::new()),
            MemoryAudit::new(),
            Arc::new(FixedClock(0)),
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
            requested_trust: TrustTier::Standard,
            requested: vec![],
            cache_classes: vec![],
        };
        let admission = kernel.admit(&manifest, None);
        let svc = kernel.attach(&admission, raw);
        svc.begin_activation("test", u64::MAX);
        Arc::new(svc)
    }

    fn opts(on_error: ReplOnError) -> ReplOptions {
        ReplOptions { greeting: "hi".into(), reply_prefix: "bot> ".into(), on_error, trace: false }
    }

    #[tokio::test]
    async fn immediate_eof_ends_cleanly() {
        let agent = BasicAgent::new("t", "s", Arc::new(EchoModel), vec![]);
        let out = run_repl(
            &agent,
            &crate::agent::loops::SingleShot,
            services(),
            Cursor::new(&b""[..]),
            opts(ReplOnError::Abort),
        )
        .await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn blank_lines_are_skipped_and_exit_stops_the_loop() {
        let agent = BasicAgent::new("t", "s", Arc::new(EchoModel), vec![]);
        // A blank line, then a real turn, then `exit` — anything after exit
        // must never be read.
        let input = Cursor::new(&b"\nhello\nexit\nshould not be reached\n"[..]);
        let out =
            run_repl(&agent, &crate::agent::loops::SingleShot, services(), input, opts(ReplOnError::Abort))
                .await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn quit_is_also_accepted_as_an_exit_word() {
        let agent = BasicAgent::new("t", "s", Arc::new(EchoModel), vec![]);
        let input = Cursor::new(&b"quit\n"[..]);
        let out =
            run_repl(&agent, &crate::agent::loops::SingleShot, services(), input, opts(ReplOnError::Abort))
                .await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn abort_policy_returns_the_turn_error() {
        let agent = BasicAgent::new("t", "s", Arc::new(EchoModel), vec![]);
        let input = Cursor::new(&b"hello\nexit\n"[..]);
        let out = run_repl(&agent, &FailingLoop, services(), input, opts(ReplOnError::Abort)).await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn continue_policy_survives_a_turn_error_and_still_reaches_exit() {
        let agent = BasicAgent::new("t", "s", Arc::new(EchoModel), vec![]);
        // Every turn fails, but with Continue the loop should still consume
        // both lines and end cleanly at `exit` rather than propagating.
        let input = Cursor::new(&b"hello\nexit\n"[..]);
        let out = run_repl(&agent, &FailingLoop, services(), input, opts(ReplOnError::Continue)).await;
        assert!(out.is_ok());
    }
}
