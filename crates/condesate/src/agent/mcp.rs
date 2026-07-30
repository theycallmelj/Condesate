//! Tools backed by real MCP servers, via the official [rmcp](https://github.com/modelcontextprotocol/rust-sdk)
//! SDK.
//!
//! An [`McpTool`] is an ordinary [`Tool`] — nothing downstream of the
//! toolbelt knows or cares that calling it sends a `tools/call` JSON-RPC
//! request over a child process's stdio instead of running local Rust code.
//! That is deliberate: it means the *existing* call-time gate in
//! `crate::agent::loops::execute_tools` (`GuardedServices::authorize_tool`,
//! checked before any `Tool::call`) already stops a denied MCP call before
//! `McpTool::call` — and therefore the JSON-RPC request itself — ever runs.
//! There is no second, MCP-specific permission check here, and there should
//! not be one: a parallel gate is a gate that can drift out of sync with the
//! real one. The boundary is authoritative because it is the *only* one.
//!
//! Tool names are namespaced `mcp:<server>:<tool>` on the way into the
//! toolbelt, so an operator can write ordinary [`crate::security::policy`]
//! rules that scope a whole server (`Tool(Pattern::parse("mcp:git:*"))`) or
//! one specific tool (`Tool(Pattern::Exact("mcp:git:git_status".into()))`)
//! with no new policy primitives.

use super::tool::Tool;
use crate::security::kernel::GuardedServices;
use crate::types::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use std::sync::Arc;

/// A live connection to one MCP server, reached by spawning it as a child
/// process and speaking MCP over its stdin/stdout.
pub struct McpConnection {
    server_name: String,
    client: RunningService<RoleClient, ()>,
}

impl McpConnection {
    /// Spawn `command` and complete the MCP initialize handshake with it.
    /// `server_name` is the namespace every tool it exposes is registered
    /// under (see the module docs).
    pub async fn connect_stdio(
        server_name: impl Into<String>,
        command: tokio::process::Command,
    ) -> Result<Arc<Self>> {
        let transport = TokioChildProcess::new(command)?;
        let client = ().serve(transport).await?;
        Ok(Arc::new(Self { server_name: server_name.into(), client }))
    }

    /// Ask the server what it offers and wrap each tool for the toolbelt.
    /// Zero tools is a normal answer, not an error.
    pub async fn tools(self: &Arc<Self>) -> Result<Vec<Arc<dyn Tool>>> {
        let listed = self.client.list_tools(None).await?;
        Ok(listed
            .tools
            .into_iter()
            .map(|t| {
                Arc::new(McpTool {
                    connection: self.clone(),
                    namespaced_name: format!("mcp:{}:{}", self.server_name, t.name),
                    remote_name: t.name.to_string(),
                    description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                }) as Arc<dyn Tool>
            })
            .collect())
    }

    /// Close the connection. Best-effort — a server that has already exited
    /// (e.g. crashed) is not an error worth surfacing here.
    pub async fn close(self: Arc<Self>) {
        if let Ok(inner) = Arc::try_unwrap(self) {
            let _ = inner.client.cancel().await;
        }
    }
}

/// One MCP-server-provided tool, proxied through the ordinary [`Tool`] trait.
pub struct McpTool {
    connection: Arc<McpConnection>,
    namespaced_name: String,
    remote_name: String,
    description: String,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.namespaced_name.clone(), description: self.description.clone() }
    }

    /// Send `tools/call` to the server and flatten the response into the
    /// plain-text observation the rest of the loop expects.
    ///
    /// No permission check happens here — see the module docs for why that
    /// is correct rather than an oversight: by the time this runs, the call
    /// has already cleared `GuardedServices::authorize_tool`.
    async fn call(&self, args: serde_json::Value, _svc: &GuardedServices) -> Result<String> {
        let arguments = args.as_object().cloned().unwrap_or_default();
        let result = self
            .connection
            .client
            .call_tool(CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments))
            .await?;

        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error == Some(true) {
            return Err(anyhow!("mcp tool '{}' returned an error: {text}", self.namespaced_name));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::core::AgentContext;
    use crate::agent::loops::{AgentLoop, ReActLoop};
    use crate::agent::model::ModelProvider;
    use crate::agent::core::BasicAgent;
    use crate::security::audit::{FixedClock, MemoryAudit};
    use crate::security::identity::{ModelClass, TenantId, TrustTier};
    use crate::security::kernel::{AgentManifest, Kernel};
    use crate::security::policy::{
        Action, GrantSet, Pattern, ResourcePattern, Rule, RuleSetPolicy, SubjectMatch,
    };
    use crate::swarm::bus::Bus;
    use crate::swarm::storage::InMemoryStorage;
    use crate::types::{CompletionRequest, CompletionResponse, HarnessId, Role, ToolCall};
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{
        CallToolResult, ErrorData, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerInfo, Tool as RmcpTool,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A minimal in-process MCP server: one tool, `echo`, that returns
    /// whatever `text` it was given. `calls` counts how many times
    /// `call_tool` actually ran — the thing a denied request must never move.
    #[derive(Clone, Default)]
    struct EchoServer {
        calls: Arc<AtomicUsize>,
    }

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult::with_all_items(vec![RmcpTool::new(
                "echo",
                "Echoes back {text}.",
                Arc::new(serde_json::Map::new()),
            )]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<rmcp::model::CallToolResponse, ErrorData> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = request
                .arguments
                .as_ref()
                .and_then(|a| a.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(rmcp::model::CallToolResponse::Complete(CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(text),
            ])))
        }
    }

    /// Wires an [`EchoServer`] and an [`McpConnection`] together over an
    /// in-memory duplex pipe — no child process, no real subprocess I/O.
    async fn connected_echo() -> (Arc<McpConnection>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let server = EchoServer { calls: calls.clone() };

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            // `.serve()` only awaits the handshake and returns a handle; the
            // handle itself must stay alive for the connection to stay open,
            // so wait on it here rather than letting it drop when this task
            // function returns.
            let running = server.serve(server_io).await.expect("server handshake");
            let _ = running.waiting().await;
        });

        let client = ().serve(client_io).await.expect("client handshake");
        (Arc::new(McpConnection { server_name: "echo".into(), client }), calls)
    }

    #[tokio::test]
    async fn discovers_and_calls_the_remote_tool() {
        let (conn, calls) = connected_echo().await;
        let tools = conn.tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "mcp:echo:echo");

        let (svc, _inboxes) = guarded_services("worker", full_access("worker")).await;
        let out = tools[0].call(json!({ "text": "hello" }), &svc).await.unwrap();
        assert_eq!(out, "hello");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_denied_mcp_tool_call_never_reaches_the_server() {
        // No Invoke grant at all: authorize_tool must refuse before McpTool::call
        // ever runs, so the JSON-RPC request never crosses the duplex pipe.
        let (conn, calls) = connected_echo().await;
        let tools = conn.tools().await.unwrap();

        let (svc, _inboxes) = guarded_services("worker", vec![]).await;
        let a = BasicAgent::new("worker", "s", Arc::new(DeniedToolCaller), tools.clone());
        let mut ctx = AgentContext::new(svc);
        let outcome = ReActLoop::default().run(&a, &mut ctx).await.unwrap();
        assert_eq!(outcome.steps, 2);

        let obs = ctx
            .transcript
            .iter()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .unwrap();
        assert!(obs.contains("denied"), "{obs}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the server must never have been called");
    }

    /// Always tries to call `mcp:echo:echo` once, then stops.
    struct DeniedToolCaller;

    #[async_trait]
    impl ModelProvider for DeniedToolCaller {
        fn name(&self) -> &str {
            "denied-tool-caller"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
            let turns = req.messages.iter().filter(|m| m.role == Role::Assistant).count();
            Ok(if turns == 0 {
                CompletionResponse {
                    content: "trying the mcp tool".into(),
                    tool_calls: vec![ToolCall {
                        name: "mcp:echo:echo".into(),
                        args: json!({ "text": "should never run" }),
                    }],
                }
            } else {
                CompletionResponse { content: "done".into(), tool_calls: vec![] }
            })
        }
    }

    fn full_access(agent: &str) -> Vec<Rule> {
        vec![Rule::allow(
            "tools",
            SubjectMatch::agent(agent),
            &[Action::Invoke],
            ResourcePattern::Tool(Pattern::Any),
        )]
    }

    async fn guarded_services(
        me: &str,
        requested: Vec<Rule>,
    ) -> (Arc<GuardedServices>, ()) {
        let raw = crate::swarm::service::ServiceHandle {
            me: HarnessId::new(me),
            roster: Arc::new(vec![HarnessId::new(me)]),
            storage: InMemoryStorage::new(),
            bus: Bus::new(std::collections::HashMap::new()),
        };
        let kernel = Kernel::new(
            GrantSet::new(requested.clone()),
            Arc::new(RuleSetPolicy::new()),
            MemoryAudit::new(),
            Arc::new(FixedClock(0)),
        );
        let manifest = AgentManifest {
            harness: HarnessId::new(me),
            agent: me.to_string(),
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
        (Arc::new(svc), ())
    }
}
