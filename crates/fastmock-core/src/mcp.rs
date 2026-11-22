//! Mocked MCP servers: stubs that answer Model Context Protocol calls (JSON-RPC over streamable HTTP)
//! posted to their path. `request.mcp` matches a call; the stub's `mcp` field describes how the stub
//! appears in `tools/list` or `resources/list` and may turn the answer into a JSON-RPC error.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::model::{BodyMatcher, StringMatcher, Stub};

/// Matches an MCP call (a JSON-RPC request) in the request body.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpMatcher {
    /// JSON-RPC method, e.g. `prompts/get`. Defaults to `tools/call` with `tool` and to `resources/read`
    /// with `resource`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tools/call")]
    pub method: Option<String>,
    /// Tool name (`params.name` of `tools/call`); the stub is listed by `tools/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "get_forecast")]
    pub tool: Option<String>,
    /// Resource URI (`params.uri` of `resources/read`); the stub is listed by `resources/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "file:///docs/readme.md")]
    pub resource: Option<String>,
    /// Body matchers on `params.arguments`: a `jsonPath` starts at the arguments object (`$.city`), any
    /// other matcher (`equalToJson`, `jsonSchema`, ...) applies to the whole arguments object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<BodyMatcher>,
}

/// How a stub appears in a mocked MCP server.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpDef {
    /// Display title of the tool or resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Description shown to the agent in `tools/list` or `resources/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the tool arguments (default `{"type": "object"}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Value>,
    /// JSON Schema of the tool's `structuredContent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output_schema: Option<Value>,
    /// Tool annotations, e.g. `{"readOnlyHint": true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub annotations: Option<Value>,
    /// Resource name in `resources/list` (default: the stub name, else the URI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// MIME type of a resource (default `text/plain`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Answer with this JSON-RPC error instead of a result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<McpErrorDef>,
}

/// A JSON-RPC error answer.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpErrorDef {
    /// JSON-RPC error code, e.g. -32602 (invalid params) or -32000 (server error).
    #[schema(example = -32000)]
    pub code: i64,
    #[schema(example = "quota exceeded")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub data: Option<Value>,
}

pub const TOOLS_CALL: &str = "tools/call";
pub const RESOURCES_READ: &str = "resources/read";
const ARGUMENTS: &str = "$.params.arguments";
const MAX_NAME: usize = 256;

/// The JSON-RPC method a matcher stands for.
pub fn method_of(m: &McpMatcher) -> Option<&str> {
    m.method.as_deref().or(if m.tool.is_some() {
        Some(TOOLS_CALL)
    } else if m.resource.is_some() {
        Some(RESOURCES_READ)
    } else {
        None
    })
}

fn equal(path: &str, v: &str) -> BodyMatcher {
    BodyMatcher {
        json_path: Some(path.into()),
        value: Some(StringMatcher::equal(v)),
        ..Default::default()
    }
}

/// The body matchers a matcher stands for, with a label for near misses.
pub fn body_matchers(m: &McpMatcher) -> Result<Vec<(String, BodyMatcher)>, String> {
    let Some(method) = method_of(m) else {
        return Err("mcp: set tool, resource or method".into());
    };
    if m.tool.is_some() && m.resource.is_some() {
        return Err("mcp: use either tool or resource".into());
    }
    if method.is_empty() || method.len() > MAX_NAME {
        return Err("mcp.method must have 1 to 256 bytes".into());
    }
    let mut out = vec![(
        format!("MCP method is not {method}"),
        equal("$.method", method),
    )];
    if let Some(t) = &m.tool {
        if t.is_empty() || t.len() > MAX_NAME {
            return Err("mcp.tool must have 1 to 256 bytes".into());
        }
        out.push((format!("MCP tool is not {t}"), equal("$.params.name", t)));
    }
    if let Some(r) = &m.resource {
        if r.is_empty() || r.len() > 2048 {
            return Err("mcp.resource must have 1 to 2048 bytes".into());
        }
        out.push((format!("MCP resource is not {r}"), equal("$.params.uri", r)));
    }
    for (i, a) in m.arguments.iter().enumerate() {
        let b = match &a.json_path {
            Some(p) => {
                let rest = p.strip_prefix('$').ok_or_else(|| {
                    format!("mcp.arguments[{i}].jsonPath must start with $ (the arguments object)")
                })?;
                let mut b = a.clone();
                b.json_path = Some(format!("{ARGUMENTS}{rest}"));
                b
            }
            None if a.absent => {
                return Err(format!(
                    "mcp.arguments[{i}]: use absent with a jsonPath, e.g. {{\"jsonPath\": \"$.dryRun\", \"absent\": true}}"
                ));
            }
            None => BodyMatcher {
                json_path: Some(ARGUMENTS.into()),
                value_body: Some(Box::new(a.clone())),
                ..Default::default()
            },
        };
        let what = a.json_path.as_deref().unwrap_or("arguments");
        out.push((
            format!("MCP argument matcher #{i} ({what}) does not match"),
            b,
        ));
    }
    Ok(out)
}

/// Checks the MCP parts of a stub that matching does not cover.
pub fn validate(stub: &Stub) -> Result<(), String> {
    let Some(m) = &stub.request.mcp else {
        if stub.mcp.is_some() {
            return Err("mcp needs request.mcp (the call the stub answers)".into());
        }
        return Ok(());
    };
    if stub
        .request
        .method
        .as_deref()
        .is_some_and(|x| !x.eq_ignore_ascii_case("POST") && !x.eq_ignore_ascii_case("ANY"))
    {
        return Err("request.mcp: MCP calls are POST requests; remove request.method".into());
    }
    for (i, r) in std::iter::once(&stub.response)
        .chain(stub.responses.iter())
        .enumerate()
    {
        if r.proxy.is_some() || r.sse.is_some() || r.websocket.is_some() {
            return Err(format!(
                "response {i}: MCP stubs answer with a body; proxy, sse and websocket are not supported"
            ));
        }
    }
    if let Some(d) = &stub.mcp {
        for (k, v) in [
            ("inputSchema", &d.input_schema),
            ("outputSchema", &d.output_schema),
            ("annotations", &d.annotations),
        ] {
            if v.as_ref().is_some_and(|v| !v.is_object()) {
                return Err(format!("mcp.{k} must be a JSON object"));
            }
        }
        if d.input_schema.is_some() || d.output_schema.is_some() {
            if m.tool.is_none() {
                return Err("mcp.inputSchema and mcp.outputSchema need request.mcp.tool".into());
            }
            for s in [&d.input_schema, &d.output_schema].into_iter().flatten() {
                crate::matcher::compile_schema(s).map_err(|e| format!("mcp: {e}"))?;
            }
        }
        if let Some(e) = &d.error
            && (e.message.is_empty() || e.message.len() > 4096)
        {
            return Err("mcp.error.message must have 1 to 4096 bytes".into());
        }
    }
    Ok(())
}

/// The `tools/list` entry of a tool stub.
pub fn tool_entry(stub: &Stub) -> Option<Value> {
    let name = stub.request.mcp.as_ref()?.tool.as_ref()?;
    let d = stub.mcp.clone().unwrap_or_default();
    let mut t = Map::new();
    t.insert("name".into(), json!(name));
    if let Some(x) = d.title {
        t.insert("title".into(), json!(x));
    }
    t.insert(
        "description".into(),
        json!(d.description.or(stub.name.clone()).unwrap_or_default()),
    );
    t.insert(
        "inputSchema".into(),
        d.input_schema.unwrap_or_else(|| json!({"type": "object"})),
    );
    if let Some(x) = d.output_schema {
        t.insert("outputSchema".into(), x);
    }
    if let Some(x) = d.annotations {
        t.insert("annotations".into(), x);
    }
    Some(Value::Object(t))
}

/// The `resources/list` entry of a resource stub.
pub fn resource_entry(stub: &Stub) -> Option<Value> {
    let uri = stub.request.mcp.as_ref()?.resource.as_ref()?;
    let d = stub.mcp.clone().unwrap_or_default();
    let mut r = Map::new();
    r.insert("uri".into(), json!(uri));
    r.insert(
        "name".into(),
        json!(d.name.or(stub.name.clone()).unwrap_or_else(|| uri.clone())),
    );
    if let Some(x) = d.title {
        r.insert("title".into(), json!(x));
    }
    if let Some(x) = d.description {
        r.insert("description".into(), json!(x));
    }
    r.insert(
        "mimeType".into(),
        json!(d.mime_type.unwrap_or_else(|| "text/plain".into())),
    );
    Some(Value::Object(r))
}

/// The JSON-RPC `result` of a stub's rendered body for a call of `method`.
pub fn result_of(method: &str, params: &Value, body: &[u8], def: Option<&McpDef>) -> Value {
    let text = String::from_utf8_lossy(body);
    let parsed: Option<Value> = if text.trim().is_empty() {
        None
    } else {
        serde_json::from_str(&text).ok()
    };
    match method {
        TOOLS_CALL => match parsed {
            Some(Value::Object(o))
                if o.contains_key("content")
                    || o.contains_key("structuredContent")
                    || o.contains_key("isError") =>
            {
                let mut o = o;
                o.entry("content").or_insert_with(|| json!([]));
                Value::Object(o)
            }
            Some(Value::Object(o)) => json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": Value::Object(o),
            }),
            _ if text.is_empty() => json!({"content": []}),
            _ => json!({"content": [{"type": "text", "text": text}]}),
        },
        RESOURCES_READ => match parsed {
            Some(Value::Object(o)) if o.contains_key("contents") => Value::Object(o),
            _ => json!({"contents": [{
                "uri": params.get("uri").cloned().unwrap_or(Value::Null),
                "mimeType": def.and_then(|d| d.mime_type.clone()).unwrap_or_else(|| "text/plain".into()),
                "text": text,
            }]}),
        },
        _ => match parsed {
            Some(v) => v,
            None if text.is_empty() => json!({}),
            None => json!(text),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: Value) -> McpMatcher {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn matchers_point_into_the_call() {
        let b = body_matchers(&m(json!({"tool": "t", "arguments": [
            {"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}},
            {"equalToJson": {"city": "Kyiv"}, "ignoreExtraElements": true}
        ]})))
        .unwrap();
        assert_eq!(b[0].1.json_path.as_deref(), Some("$.method"));
        assert_eq!(b[1].1.json_path.as_deref(), Some("$.params.name"));
        assert_eq!(b[2].1.json_path.as_deref(), Some("$.params.arguments.city"));
        assert_eq!(b[3].1.json_path.as_deref(), Some("$.params.arguments"));
        assert!(b[3].1.value_body.is_some());
        assert!(body_matchers(&m(json!({}))).is_err());
        assert!(body_matchers(&m(json!({"tool": "a", "resource": "b"}))).is_err());
        assert!(
            body_matchers(&m(
                json!({"tool": "a", "arguments": [{"jsonPath": "city"}]})
            ))
            .is_err()
        );
        assert_eq!(
            body_matchers(&m(json!({"resource": "file:///a"}))).unwrap()[0]
                .1
                .value,
            Some(StringMatcher::equal("resources/read"))
        );
    }

    fn call(args: Value) -> crate::CapturedRequest {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "t", "arguments": args}});
        crate::CapturedRequest::new(
            "POST",
            "/mcp",
            None,
            vec![("content-type".into(), "application/json".into())],
            bytes::Bytes::from(body.to_string()),
        )
    }

    #[test]
    fn argument_matchers_match_calls() {
        let stub = |arguments: Value| {
            let s: Stub = serde_json::from_value(json!({"request": {"urlPath": "/mcp",
                "mcp": {"tool": "t", "arguments": arguments}}}))
            .unwrap();
            crate::matcher::CompiledStub::compile(&s).unwrap()
        };
        let hits = |c: &crate::matcher::CompiledStub, args: Value| {
            let r = call(args);
            c.match_request(&r, r.json()).is_some()
        };
        let by_path = stub(json!([{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]));
        assert!(hits(&by_path, json!({"city": "Kyiv", "days": 2})));
        assert!(!hits(&by_path, json!({"city": "Lviv"})));
        let whole = stub(json!([{"equalToJson": {"city": "Kyiv"}, "ignoreExtraElements": true}]));
        assert!(hits(&whole, json!({"city": "Kyiv", "days": 2})));
        assert!(!hits(&whole, json!({"town": "Kyiv"})));
        let schema = stub(
            json!([{"jsonSchema": {"type": "object", "required": ["city"],
            "properties": {"city": {"type": "string"}}}}]),
        );
        assert!(hits(&schema, json!({"city": "Kyiv"})));
        assert!(!hits(&schema, json!({"city": 7})));
        let other = crate::CapturedRequest::new(
            "POST",
            "/mcp",
            None,
            vec![],
            bytes::Bytes::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"u","arguments":{"city":"Kyiv"}}}"#,
            ),
        );
        assert!(by_path.match_request(&other, other.json()).is_none());
        let nm = by_path.near_miss(&other, other.json());
        assert_eq!(nm.mismatches, vec!["MCP tool is not t".to_string()]);
    }

    #[test]
    fn bodies_become_results() {
        let p = json!({"uri": "file:///a"});
        assert_eq!(
            result_of(TOOLS_CALL, &p, b"hello", None),
            json!({"content": [{"type": "text", "text": "hello"}]})
        );
        let r = result_of(TOOLS_CALL, &p, br#"{"temp": 21}"#, None);
        assert_eq!(r["structuredContent"]["temp"], 21);
        assert_eq!(r["content"][0]["type"], "text");
        let r = result_of(TOOLS_CALL, &p, br#"{"isError": true, "content": []}"#, None);
        assert_eq!(r["isError"], true);
        let r = result_of(RESOURCES_READ, &p, b"# Readme", None);
        assert_eq!(r["contents"][0]["uri"], "file:///a");
        assert_eq!(r["contents"][0]["text"], "# Readme");
        assert_eq!(
            result_of("prompts/list", &p, br#"{"prompts": []}"#, None),
            json!({"prompts": []})
        );
        assert_eq!(result_of("x", &p, b"", None), json!({}));
    }
}
