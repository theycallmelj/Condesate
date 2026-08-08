//! Real integration with [iris-eval/mcp-server](https://github.com/iris-eval/mcp-server)
//! — a real, published MCP server (`npx @iris-eval/mcp-server`) that scores
//! agent output for completeness/relevance/safety/cost using deterministic,
//! offline heuristic rules (no LLM, no API key: `evaluate_output`).
//!
//! Unlike the other two integrations in this crate (which stand up a new
//! HTTP endpoint), this one needs zero new protocol code: `condesate`
//! already has a real MCP *client* (`condesate::McpConnection`/`McpTool`,
//! feature `mcp`) that speaks MCP to any server over stdio. Real outputs
//! from real `condesate` scenarios are handed to `evaluate_output` through
//! the *exact same* tool-calling path — `SingleShot`, a scripted
//! `ToolCall`, `loops::execute_tools`' `authorize_tool` gate — that every
//! other tool call in this codebase goes through. Nothing here is
//! MCP-specific or eval-specific; it's the ordinary tool-call machinery
//! pointed at a real external MCP server.
//!
//! Requires Node.js 20+ (`npx @iris-eval/mcp-server` spawns on first call —
//! slower the first time while npm resolves the package); see
//! `crates/evals/README.md`.

use anyhow::{Context, Result};
use condesate::{
    Agent, AgentContext, AgentLoop, BasicAgent, CompletionResponse, McpConnection, Role, SingleShot,
    ToolCall,
};
use evals::harness::{build_services, invoke_exact_tool, PlaybackModel};
use serde_json::{json, Value};
use std::sync::Arc;

const SERVER_NAME: &str = "iris-eval";
const NAMESPACED_TOOL: &str = "mcp:iris-eval:evaluate_output";

struct EvalCase {
    name: &'static str,
    output: &'static str,
    eval_type: &'static str,
    expected: Option<&'static str>,
    input: Option<&'static str>,
}

/// Runs one real `condesate` `SingleShot` activation whose only tool call is
/// `evaluate_output` on the real, connected iris-eval server, and parses its
/// real JSON response.
async fn evaluate(
    connection: &Arc<McpConnection>,
    output: &str,
    eval_type: &str,
    expected: Option<&str>,
    input: Option<&str>,
) -> Result<Value> {
    let tools = connection.tools().await?;
    let grants = vec![invoke_exact_tool("evaluator", NAMESPACED_TOOL)];
    let (svc, _inboxes) = build_services("evaluator", &["evaluator"], grants);

    let mut args = json!({ "output": output, "eval_type": eval_type });
    if let Some(exp) = expected {
        args["expected"] = json!(exp);
    }
    if let Some(inp) = input {
        args["input"] = json!(inp);
    }

    let agent = BasicAgent::new(
        "evaluator",
        "system",
        Arc::new(PlaybackModel::new(vec![CompletionResponse {
            content: "evaluating".into(),
            tool_calls: vec![ToolCall { name: NAMESPACED_TOOL.into(), args }],
        }])),
        tools,
    );
    let mut ctx = AgentContext::new(svc);
    SingleShot.run(&agent as &dyn Agent, &mut ctx).await?;

    let obs = ctx
        .transcript
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .context("no tool observation recorded")?;
    serde_json::from_str(&obs).with_context(|| format!("evaluate_output did not return JSON: {obs}"))
}

fn print_result(name: &str, eval_type: &str, result: &Value) {
    let score = result.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let passed = result.get("passed").and_then(|v| v.as_bool()).unwrap_or(false);
    let insufficient = result.get("insufficient_data").and_then(|v| v.as_bool()).unwrap_or(false);
    println!(
        "[{}] {name} (eval_type={eval_type}): score={score:.2}{}",
        if passed { "PASS" } else { "FAIL" },
        if insufficient { " (insufficient_data)" } else { "" }
    );
    if let Some(rules) = result.get("rule_results").and_then(|v| v.as_array()) {
        for r in rules {
            let rule_name = r.get("ruleName").and_then(|v| v.as_str()).unwrap_or("?");
            let rule_passed = r.get("passed").and_then(|v| v.as_bool()).unwrap_or(false);
            let message = r.get("message").and_then(|v| v.as_str()).unwrap_or("");
            println!("    [{}] {rule_name}: {message}", if rule_passed { "ok" } else { "!!" });
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cmd = tokio::process::Command::new("npx");
    cmd.arg("--yes").arg("@iris-eval/mcp-server");
    println!("launching: npx --yes @iris-eval/mcp-server (stdio)\n");

    let connection = McpConnection::connect_stdio(SERVER_NAME, cmd)
        .await
        .context("failed to launch iris-eval mcp-server — is Node.js 20+ / npx available?")?;

    // Three real condesate outputs, each scored against a different
    // eval_type bundle:
    //   - the word-count answer, for completeness (length/structure)
    //   - the permission boundary's own denial text, for safety (PII /
    //     prompt-injection / hallucination-marker scanning) — proving the
    //     boundary's plain-language refusals don't themselves leak anything
    //   - the echo answer, for relevance (keyword overlap vs `expected`)
    let cases = [
        EvalCase { name: "word_count_answer", output: "4", eval_type: "completeness", expected: None, input: None },
        EvalCase {
            name: "shutdown_denial_text",
            output: "tool 'shutdown_swarm' denied: denied: http-eval(http-eval@evals) may not Invoke tool:shutdown_swarm (NoMatchingRule)",
            eval_type: "safety",
            expected: None,
            input: None,
        },
        EvalCase {
            name: "echo_answer",
            output: "echo this back to me",
            eval_type: "relevance",
            expected: Some("echo this back to me"),
            input: Some("echo this back to me"),
        },
    ];

    // The exit code reflects whether every call to the real server *ran*
    // successfully, not whether iris-eval's own opinionated thresholds all
    // came back `passed: true`. Its `completeness` rule wants >=50 chars and
    // >=2 sentences, which is a prose expectation a terse numeric tool
    // answer like `"4"` will never satisfy — that's a real, useful verdict
    // from a real tool, not a bug to tune away by picking friendlier inputs.
    let mut ran_ok = true;
    for case in cases {
        match evaluate(&connection, case.output, case.eval_type, case.expected, case.input).await {
            Ok(result) => {
                print_result(case.name, case.eval_type, &result);
            }
            Err(e) => {
                println!("[ERROR] {}: {e}", case.name);
                ran_ok = false;
            }
        }
    }
    println!(
        "\n{} — iris-eval's own per-case verdicts are printed above as real \
         (sometimes surprising) output, not forced to pass.",
        if ran_ok { "all calls completed" } else { "one or more calls failed to run" }
    );

    connection.close().await;
    std::process::exit(if ran_ok { 0 } else { 1 });
}
