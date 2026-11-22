//! Mocked MCP servers on the data plane. A path with stubs that match MCP calls (`request.mcp`) serves
//! the streamable HTTP transport statelessly: JSON-RPC requests are answered by the matching stub (its
//! rendered body becomes the result, delays and faults apply as for HTTP), and otherwise by FastMock
//! itself for `initialize`, `ping`, `tools/list` and `resources/list`. Every call goes through the
//! data-plane middleware, so it is journaled and counts for verification and near misses.

use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use fastmock_core::mcp::{self as core_mcp, McpDef};
use fastmock_core::{CapturedRequest, EngineResponse, SpaceFilter, Stub};
use serde_json::{Map, Value, json};
use tracing::Instrument;

use crate::dataplane::{Unmatched, stub_response};
use crate::state::AppState;

/// Protocol revisions with an `initialize` handshake that the mock speaks, oldest first.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Methods counted under their own name in metrics; others count as `other`.
const METHODS: [&str; 9] = [
    "initialize",
    "ping",
    "tools/list",
    "tools/call",
    "resources/list",
    "resources/read",
    "resources/templates/list",
    "prompts/list",
    "prompts/get",
];

fn method_label(m: &str) -> &'static str {
    METHODS
        .iter()
        .find(|x| **x == m)
        .copied()
        .unwrap_or("other")
}

fn count(method: &'static str, outcome: &'static str) {
    metrics::counter!("fastmock_mcp_mock_requests_total", "method" => method, "outcome" => outcome)
        .increment(1);
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    let mut r = (status, serde_json::to_vec(v).unwrap_or_default()).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

fn rpc_error(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": e})
}

fn unmatched(mut r: Response) -> Response {
    r.extensions_mut().insert(Unmatched);
    r
}

/// Answers the request when its path serves a mocked MCP server; `None` otherwise.
pub async fn handle(app: &AppState, method: &Method, cr: &CapturedRequest) -> Option<Response> {
    let stubs = app.engine.mcp_endpoint(cr)?;
    let span = tracing::info_span!(
        "fastmock.mcp.mock",
        rpc.method = tracing::field::Empty,
        mcp.tool = tracing::field::Empty,
        mcp.outcome = tracing::field::Empty,
    );
    Some(serve(app, method, cr, stubs).instrument(span).await)
}

async fn serve(
    app: &AppState,
    method: &Method,
    cr: &CapturedRequest,
    stubs: Vec<Stub>,
) -> Response {
    let span = tracing::Span::current();
    if method != Method::POST {
        count("other", "method_not_allowed");
        let mut r = StatusCode::METHOD_NOT_ALLOWED.into_response();
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return r;
    }
    let msg: Value = match serde_json::from_slice(&cr.body) {
        Ok(v) => v,
        Err(_) => {
            count("other", "invalid");
            return unmatched(json_response(
                StatusCode::BAD_REQUEST,
                &rpc_error(&Value::Null, -32700, "parse error", None),
            ));
        }
    };
    let Some(obj) = msg.as_object() else {
        count("other", "invalid");
        return unmatched(json_response(
            StatusCode::BAD_REQUEST,
            &rpc_error(
                &Value::Null,
                -32600,
                "invalid request: send one JSON-RPC message per request (batches are not supported)",
                None,
            ),
        ));
    };
    let rpc_method = obj.get("method").and_then(Value::as_str);
    let id = obj.get("id").cloned().filter(|v| !v.is_null());
    let (Some(rpc_method), Some(id)) = (rpc_method, id) else {
        if rpc_method.is_some() || obj.contains_key("result") || obj.contains_key("error") {
            count("other", "notification");
            return StatusCode::ACCEPTED.into_response();
        }
        count("other", "invalid");
        return unmatched(json_response(
            StatusCode::BAD_REQUEST,
            &rpc_error(&Value::Null, -32600, "invalid request", None),
        ));
    };
    let label = method_label(rpc_method);
    span.record("rpc.method", label);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if let Some(t) = params.get("name").and_then(Value::as_str)
        && rpc_method == core_mcp::TOOLS_CALL
        && t.len() <= 256
    {
        span.record("mcp.tool", t);
    }
    match app
        .engine
        .handle_where(cr, |cs| cs.stub.request.mcp.is_some())
        .await
    {
        Ok(Some(r)) => {
            let def = stubs
                .iter()
                .find(|s| s.id == r.stub_id)
                .and_then(|s| s.mcp.clone());
            span.record("mcp.outcome", "stub");
            count(label, "stub");
            let r = wrap(r, &id, rpc_method, &params, def.as_ref());
            return stub_response(app, r).await;
        }
        Ok(None) => {}
        Err(e) => {
            metrics::counter!("fastmock_stub_errors_total").increment(1);
            tracing::error!(error = %e, path = %cr.path, "MCP stub evaluation failed");
            count(label, "error");
            return json_response(
                StatusCode::OK,
                &rpc_error(
                    &id,
                    -32603,
                    "stub evaluation failed; see the server log and the request journal for this trace id",
                    None,
                ),
            );
        }
    }
    let (body, matched) = builtin(app, cr, rpc_method, &id, &params, &stubs);
    span.record("mcp.outcome", if matched { "builtin" } else { "unmatched" });
    count(label, if matched { "builtin" } else { "unmatched" });
    let r = json_response(StatusCode::OK, &body);
    if matched { r } else { unmatched(r) }
}

/// Turns a stub's rendered answer into a JSON-RPC response; faults and non-2xx statuses stay as they are.
fn wrap(
    mut r: EngineResponse,
    id: &Value,
    method: &str,
    params: &Value,
    def: Option<&McpDef>,
) -> EngineResponse {
    if r.fault.is_some() || !(200..300).contains(&r.status) {
        return r;
    }
    let envelope = match def.and_then(|d| d.error.as_ref()) {
        Some(e) => rpc_error(id, e.code, &e.message, e.data.clone()),
        None => match serde_json::from_slice::<Value>(&r.body) {
            Ok(Value::Object(mut o)) if o.contains_key("jsonrpc") => {
                o.entry("id").or_insert_with(|| id.clone());
                Value::Object(o)
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": core_mcp::result_of(method, params, &r.body, def),
            }),
        },
    };
    r.status = 200;
    r.body = Bytes::from(serde_json::to_vec(&envelope).unwrap_or_default());
    r.headers.retain(|(k, _)| {
        !k.eq_ignore_ascii_case("content-type") && !k.eq_ignore_ascii_case("content-length")
    });
    r.headers
        .push(("content-type".into(), "application/json".into()));
    r
}

fn negotiate(params: &Value) -> &'static str {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    PROTOCOL_VERSIONS
        .iter()
        .find(|v| Some(**v) == asked)
        .copied()
        .unwrap_or(PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1])
}

fn tools(stubs: &[Stub]) -> Vec<Value> {
    let mut out: Vec<(Value, bool)> = Vec::new();
    for s in stubs {
        let Some(t) = core_mcp::tool_entry(s) else {
            continue;
        };
        let described = s.mcp.is_some();
        match out.iter_mut().find(|(x, _)| x["name"] == t["name"]) {
            Some(slot) if !slot.1 && described => *slot = (t, true),
            Some(_) => {}
            None => out.push((t, described)),
        }
    }
    out.into_iter().map(|(t, _)| t).collect()
}

fn resources(stubs: &[Stub]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for r in stubs.iter().filter_map(core_mcp::resource_entry) {
        if !out.iter().any(|x| x["uri"] == r["uri"]) {
            out.push(r);
        }
    }
    out
}

/// FastMock's own answer when no stub matched; the flag is false for calls that should have matched a
/// stub (they are journaled as unmatched).
fn builtin(
    app: &AppState,
    cr: &CapturedRequest,
    method: &str,
    id: &Value,
    params: &Value,
    stubs: &[Stub],
) -> (Value, bool) {
    let ok = |result: Value| (json!({"jsonrpc": "2.0", "id": id, "result": result}), true);
    match method {
        "initialize" => {
            let mut caps = Map::new();
            caps.insert("tools".into(), json!({"listChanged": false}));
            if stubs
                .iter()
                .any(|s| s.request.mcp.as_ref().is_some_and(|m| m.resource.is_some()))
            {
                caps.insert(
                    "resources".into(),
                    json!({"listChanged": false, "subscribe": false}),
                );
            }
            ok(json!({
                "protocolVersion": negotiate(params),
                "capabilities": caps,
                "serverInfo": {"name": "fastmock", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": tools(stubs)})),
        "resources/list" => ok(json!({"resources": resources(stubs)})),
        "resources/templates/list" => ok(json!({"resourceTemplates": []})),
        core_mcp::TOOLS_CALL => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let known = tools(stubs).iter().any(|t| t["name"] == name);
            let data = (known && app.cfg.unmatched_details)
                .then(|| json!({"nearMisses": app.engine.near_misses(cr, 3, &SpaceFilter::Any)}));
            let message = if known {
                format!("no stub of tool {name} matches these arguments")
            } else {
                format!("unknown tool: {name}")
            };
            (rpc_error(id, -32602, &message, data), false)
        }
        core_mcp::RESOURCES_READ => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            (
                rpc_error(id, -32002, "resource not found", Some(json!({"uri": uri}))),
                false,
            )
        }
        other => (
            rpc_error(id, -32601, &format!("method not found: {other}"), None),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(v: Value) -> Stub {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn tools_are_listed_once_preferring_described_stubs() {
        let a = stub(
            json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "t", "arguments": [{"jsonPath": "$.x", "value": {"equalTo": "1"}}]}}}),
        );
        let b = stub(
            json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "t"}}, "mcp": {"description": "does t", "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}}}}}),
        );
        let c = stub(
            json!({"request": {"urlPath": "/mcp", "mcp": {"resource": "file:///r"}}, "mcp": {"mimeType": "text/markdown"}}),
        );
        let l = tools(&[a, b, c.clone()]);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["description"], "does t");
        assert_eq!(l[0]["inputSchema"]["properties"]["x"]["type"], "string");
        let r = resources(&[c]);
        assert_eq!(r[0]["mimeType"], "text/markdown");
    }

    #[test]
    fn versions_are_negotiated() {
        assert_eq!(
            negotiate(&json!({"protocolVersion": "2025-03-26"})),
            "2025-03-26"
        );
        assert_eq!(
            negotiate(&json!({"protocolVersion": "2099-01-01"})),
            "2025-11-25"
        );
        assert_eq!(negotiate(&Value::Null), "2025-11-25");
    }
}
