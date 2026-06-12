//! MCP server (Streamable HTTP, JSON-RPC 2.0) exposing the ATS tool surface to
//! Claude Code sessions. The orchestrator — itself a Claude Code session —
//! drives the daemon through this; with the server registered at user scope
//! (`claude mcp add --scope user`), every session can reach it.
//!
//! Bound to loopback only: fleet control must never be reachable off-machine.
//! Tool declarations and execution are reused verbatim from `crate::agent`, so
//! there is exactly one implementation of each tool.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};

use crate::agent;
use crate::server::Daemon;

/// MCP protocol version we implement; we echo the client's when it sends one.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The agent's tools, with Anthropic's `input_schema` renamed to MCP's
/// `inputSchema`. Same names, descriptions, and schemas — one source of truth.
fn mcp_tools() -> Value {
    let mut tools = agent::tools();
    if let Some(arr) = tools.as_array_mut() {
        for t in arr {
            let schema = t.get("input_schema").cloned();
            if let (Some(schema), Some(obj)) = (schema, t.as_object_mut()) {
                obj.remove("input_schema");
                obj.insert("inputSchema".into(), schema);
            }
        }
    }
    tools
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error })
}

/// Dispatch one JSON-RPC message. Returns `None` for notifications (no `id`),
/// which get no response per JSON-RPC.
async fn handle_rpc(daemon: &Arc<Daemon>, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => {
            let proto = req
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            Ok(json!({
                "protocolVersion": proto,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ats", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "tools/list" => Ok(json!({ "tools": mcp_tools() })),
        "tools/call" => {
            let name = req.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            let args = req.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));
            tracing::info!(tool = name, "mcp tools/call");
            if let Some(block) = agent::guardrail_block(daemon, name, &args) {
                Ok(tool_result(block, true))
            } else {
                match agent::execute_tool(daemon, name, &args).await {
                    Ok(text) => Ok(tool_result(text, false)),
                    Err(e) => Ok(tool_result(format!("error: {e:#}"), true)),
                }
            }
        }
        "ping" => Ok(json!({})),
        m if m.starts_with("notifications/") => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    // notification (no id): acknowledge with no body
    id.as_ref()?;

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

async fn mcp_endpoint(State(daemon): State<Arc<Daemon>>, body: Json<Value>) -> impl IntoResponse {
    // single request, or a JSON-RPC batch
    let resp = if let Some(arr) = body.0.as_array() {
        let mut out = Vec::new();
        for req in arr {
            if let Some(r) = handle_rpc(&daemon, req).await {
                out.push(r);
            }
        }
        if out.is_empty() { Value::Null } else { Value::Array(out) }
    } else {
        handle_rpc(&daemon, &body.0).await.unwrap_or(Value::Null)
    };
    Json(resp)
}

/// Serve the MCP endpoint on `127.0.0.1:<port>/mcp` until the process exits.
pub async fn serve(daemon: Arc<Daemon>, port: u16) -> Result<()> {
    let app = Router::new().route("/mcp", post(mcp_endpoint)).with_state(daemon);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding MCP server on {addr}"))?;
    tracing::info!(%addr, "ats MCP server listening at http://{addr}/mcp");
    axum::serve(listener, app).await.context("MCP server")?;
    Ok(())
}
