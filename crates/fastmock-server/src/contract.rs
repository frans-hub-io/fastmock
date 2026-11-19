use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fastmock_core::SecretProvider;
use fastmock_core::asyncapi::{self, AsyncImportOptions, AsyncProtocol, schema_errors};
use fastmock_core::openapi::{ResolveBudget, example, resolve_budgeted, to_json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event as audit, bad, require};
use crate::asyncmock::parse_spec;
use crate::auth::{Principal, Role};
use crate::state::AppState;
use fastmock_core::outbound::{blocked_outbound_header, blocked_reason};

pub const NAMESPACE: &str = "contract-runs";
pub const SPECS_NAMESPACE: &str = "specs";
pub const MAX_RUNS_KEPT: usize = 200;
pub const RUN_RETENTION: Duration = Duration::from_secs(7 * 86_400);
pub const MAX_OPERATIONS_PER_RUN: usize = 200;
pub const MAX_EXAMPLES_PER_OPERATION: usize = 10;
pub const MAX_CONCURRENT_RUNS: usize = 4;
pub const REQUEST_CONCURRENCY: usize = 4;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_EXCERPT_BYTES: usize = 4096;
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_TIMEOUT_MS: u64 = 60_000;
pub const MAX_RUN_DURATION: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_WAIT_MS: u64 = 10_000;
pub const MAX_WAIT_MS: u64 = 120_000;
pub const MAX_ASYNC_MESSAGES: usize = 100;
const MAX_RUN_CLIENT_TIMEOUT: Duration = Duration::from_secs(MAX_TIMEOUT_MS / 1000);
const MAX_ERRORS: usize = 20;
const MAX_HEADERS: usize = 64;
const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

static RUNS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS)));

#[derive(Clone, Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContractTestRequest {
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub spec: Option<Value>,
    #[serde(default)]
    pub spec_ref: Option<String>,
    #[serde(default)]
    pub target_url: Option<String>,
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub secret_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub message_count: Option<usize>,
    #[serde(default)]
    pub wait_ms: Option<u64>,
    #[serde(default)]
    pub from_beginning: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Passed,
    Failed,
    Error,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Passed,
    Failed,
    Error,
}

impl Verdict {
    fn label(&self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
            Verdict::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestRecord {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResponseRecord {
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub body_excerpt: String,
    pub body_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    pub partition: i32,
    pub offset: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub excerpt: String,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageRecord>,
    pub errors: Vec<String>,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContractRun {
    pub id: Uuid,
    pub kind: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_url: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub started_by: String,
    pub summary: RunSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub results: Vec<OperationResult>,
    /// True when request/response excerpts, message excerpts and error texts were removed because
    /// the caller is a viewer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub details_hidden: bool,
}

#[derive(Clone, Debug)]
pub struct HttpCase {
    pub operation: String,
    pub example: Option<String>,
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<(String, Vec<u8>)>,
    pub responses: Value,
}

#[derive(Clone, Debug)]
pub struct AsyncCase {
    pub operation: String,
    pub topic: String,
    pub payload_schema: Option<Value>,
    pub avro_schema: Option<Value>,
}

pub enum Plan {
    Http(Vec<HttpCase>),
    Kafka(Vec<AsyncCase>),
}

fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn text(v: &Value) -> String {
    fastmock_core::matcher::value_text(v)
}

fn param_example(p: &Value, name: Option<&str>) -> Option<Value> {
    if let Some(n) = name
        && let Some(v) = p.pointer(&format!(
            "/examples/{}/value",
            n.replace('~', "~0").replace('/', "~1")
        ))
    {
        return Some(v.clone());
    }
    if let Some(v) = p.get("example") {
        return Some(v.clone());
    }
    if let Some(Value::Object(ex)) = p.get("examples")
        && let Some((_, first)) = ex.iter().next()
        && let Some(v) = first.get("value")
    {
        return Some(v.clone());
    }
    let s = p.get("schema")?;
    let e = example(s, 0);
    (!e.is_null()).then_some(e)
}

fn json_media(content: Option<&Value>) -> Option<(&String, &Value)> {
    let c = content?.as_object()?;
    c.iter()
        .find(|(k, _)| k.starts_with("application/json") || k.contains("+json"))
        .or_else(|| c.iter().next())
}

fn media_example(m: &Value, name: Option<&str>) -> Value {
    if let Some(n) = name
        && let Some(Value::Object(ex)) = m.get("examples")
        && let Some(v) = ex.get(n).and_then(|e| e.get("value"))
    {
        return v.clone();
    }
    if let Some(e) = m.get("example") {
        return e.clone();
    }
    if let Some(Value::Object(ex)) = m.get("examples")
        && let Some((_, first)) = ex.iter().next()
        && let Some(v) = first.get("value")
    {
        return v.clone();
    }
    m.get("schema")
        .map(|s| example(s, 0))
        .unwrap_or(Value::Null)
}

fn operation_selected(filter: &[String], op_id: Option<&str>, method: &str, path: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let label = format!("{} {path}", method.to_uppercase());
    filter
        .iter()
        .any(|f| Some(f.as_str()) == op_id || f.eq_ignore_ascii_case(&label))
}

pub fn plan_openapi(doc: &Value, filter: &[String]) -> anyhow::Result<Vec<HttpCase>> {
    let version = doc.get("openapi").and_then(|v| v.as_str()).unwrap_or("");
    if !version.starts_with('3') {
        anyhow::bail!("only OpenAPI 3.x documents can be contract tested");
    }
    let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) else {
        anyhow::bail!("the document has no paths");
    };
    let mut cases = Vec::new();
    let mut budget = ResolveBudget::default();
    for (path, raw_item) in paths {
        let item = resolve_budgeted(doc, raw_item, 0, &mut budget)?;
        let path_params: Vec<Value> = item
            .get("parameters")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            let op_id = op.get("operationId").and_then(|v| v.as_str());
            if !operation_selected(filter, op_id, method, path) {
                continue;
            }
            let label = op_id
                .map(String::from)
                .unwrap_or_else(|| format!("{} {path}", method.to_uppercase()));
            let mut params: Vec<Value> = Vec::new();
            let op_params: Vec<Value> = op
                .get("parameters")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            for p in path_params.iter().chain(op_params.iter()) {
                let key = (p.get("name").cloned(), p.get("in").cloned());
                params.retain(|x| (x.get("name").cloned(), x.get("in").cloned()) != key);
                params.push(p.clone());
            }
            let media = op
                .get("requestBody")
                .and_then(|rb| json_media(rb.get("content")));
            let mut names: Vec<String> = Vec::new();
            if let Some((_, m)) = media
                && let Some(Value::Object(ex)) = m.get("examples")
            {
                names.extend(ex.keys().cloned());
            }
            for p in &params {
                if let Some(Value::Object(ex)) = p.get("examples") {
                    for k in ex.keys() {
                        if !names.contains(k) {
                            names.push(k.clone());
                        }
                    }
                }
            }
            names.truncate(MAX_EXAMPLES_PER_OPERATION);
            let variants: Vec<Option<String>> = if names.is_empty() {
                vec![None]
            } else {
                names.into_iter().map(Some).collect()
            };
            for name in variants {
                let n = name.as_deref();
                let mut filled = path.clone();
                let mut query = Vec::new();
                let mut headers = Vec::new();
                for p in &params {
                    let (Some(pname), Some(loc)) = (
                        p.get("name").and_then(|v| v.as_str()),
                        p.get("in").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    let required = p
                        .get("required")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(loc == "path");
                    let value = param_example(p, n);
                    match loc {
                        "path" => {
                            let v = value.map(|v| text(&v)).unwrap_or_else(|| "1".into());
                            filled = filled.replace(&format!("{{{pname}}}"), &encode_component(&v));
                        }
                        "query" if required || value.is_some() => {
                            if let Some(v) = value.or_else(|| Some(json!("1"))) {
                                match v {
                                    Value::Array(a) => {
                                        for x in a {
                                            query.push((pname.to_string(), text(&x)));
                                        }
                                    }
                                    other => query.push((pname.to_string(), text(&other))),
                                }
                            }
                        }
                        "header" if required => {
                            let v = value.map(|v| text(&v)).unwrap_or_else(|| "1".into());
                            headers.push((pname.to_string(), v));
                        }
                        _ => {}
                    }
                }
                let body = media.map(|(ct, m)| {
                    let ex = media_example(m, n);
                    let bytes = if ct.contains("json") {
                        serde_json::to_vec(&ex).unwrap_or_default()
                    } else {
                        text(&ex).into_bytes()
                    };
                    (ct.clone(), bytes)
                });
                cases.push(HttpCase {
                    operation: label.clone(),
                    example: name.clone(),
                    method: method.to_uppercase(),
                    path: filled,
                    query,
                    headers,
                    body,
                    responses: op.get("responses").cloned().unwrap_or(Value::Null),
                });
                if cases.len() > MAX_OPERATIONS_PER_RUN {
                    anyhow::bail!(
                        "the run would send more than {MAX_OPERATIONS_PER_RUN} requests; narrow it with `operations`"
                    );
                }
            }
        }
    }
    if cases.is_empty() {
        anyhow::bail!("no operations selected");
    }
    Ok(cases)
}

pub fn plan_asyncapi(doc: &Value, filter: &[String]) -> anyhow::Result<Vec<AsyncCase>> {
    let r = asyncapi::import(doc, &AsyncImportOptions::default())?;
    let cases: Vec<AsyncCase> = r
        .mocks
        .into_iter()
        .filter(|m| m.protocol == AsyncProtocol::Kafka)
        .filter(|m| {
            filter.is_empty()
                || filter
                    .iter()
                    .any(|f| *f == m.name || Some(f) == m.operation.as_ref() || *f == m.channel)
        })
        .map(|m| AsyncCase {
            operation: m.name,
            topic: m.channel,
            payload_schema: m.payload_schema,
            avro_schema: m.avro_schema,
        })
        .collect();
    if cases.is_empty() {
        anyhow::bail!("no Kafka operations selected");
    }
    if cases.len() > MAX_OPERATIONS_PER_RUN {
        anyhow::bail!("the run would test more than {MAX_OPERATIONS_PER_RUN} operations");
    }
    Ok(cases)
}

fn declared_response(responses: &Value, status: u16) -> Option<&Value> {
    let o = responses.as_object()?;
    let exact = status.to_string();
    if let Some(r) = o.get(&exact) {
        return Some(r);
    }
    let class = format!("{}XX", status / 100);
    if let Some((_, r)) = o.iter().find(|(k, _)| k.eq_ignore_ascii_case(&class)) {
        return Some(r);
    }
    o.get("default")
}

fn media_matches(declared: &str, actual: &str) -> bool {
    let d = declared
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if d == "*/*" || d == actual {
        return true;
    }
    match d.strip_suffix("/*") {
        Some(t) => actual.split('/').next() == Some(t),
        None => false,
    }
}

pub fn validate_response(
    responses: &Value,
    status: u16,
    content_type: Option<&str>,
    body: &[u8],
    truncated: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(def) = declared_response(responses, status) else {
        let declared: Vec<String> = responses
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        errors.push(format!(
            "status {status} is not declared (declared: {})",
            declared.join(", ")
        ));
        return errors;
    };
    let Some(content) = def.get("content").and_then(|c| c.as_object()) else {
        return errors;
    };
    if content.is_empty() {
        return errors;
    }
    let actual = content_type
        .map(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if body.is_empty() && actual.is_empty() {
        if content.values().any(|m| m.get("schema").is_some()) {
            errors.push("the response declares content but the body is empty".into());
        }
        return errors;
    }
    let Some((media_type, media)) = content.iter().find(|(k, _)| media_matches(k, &actual)) else {
        errors.push(format!(
            "content type `{}` is not declared (declared: {})",
            if actual.is_empty() { "none" } else { &actual },
            content.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
        return errors;
    };
    let Some(schema) = media.get("schema") else {
        return errors;
    };
    let json_like = actual.contains("json") || media_type.contains("json");
    if !json_like {
        return errors;
    }
    if truncated {
        errors.push(format!(
            "the response exceeds {MAX_RESPONSE_BYTES} bytes; the body was not validated"
        ));
        return errors;
    }
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("the body is not valid JSON: {e}"));
            return errors;
        }
    };
    match schema_errors(&to_json_schema(schema), &value, MAX_ERRORS) {
        Ok(list) => errors.extend(list.into_iter().map(|e| format!("body {e}"))),
        Err(e) => errors.push(format!("the response schema cannot be compiled: {e}")),
    }
    errors
}

fn header_ok(name: &str, value: &str) -> bool {
    axum::http::HeaderName::from_bytes(name.as_bytes()).is_ok()
        && axum::http::HeaderValue::from_str(value).is_ok()
}

struct RunInput {
    target: Option<reqwest::Url>,
    client: reqwest::Client,
    headers: Vec<(String, String)>,
    secret_headers: Vec<(String, String)>,
    timeout: Duration,
    message_count: usize,
    wait: Duration,
    from_beginning: bool,
}

async fn run_http_case(app: &AppState, input: &RunInput, case: &HttpCase) -> OperationResult {
    let start = Instant::now();
    let mut result = OperationResult {
        operation: case.operation.clone(),
        example: case.example.clone(),
        verdict: Verdict::Error,
        request: None,
        response: None,
        topic: None,
        messages: vec![],
        errors: vec![],
        duration_ms: 0,
    };
    let Some(base) = &input.target else {
        result.errors.push("targetUrl is required".into());
        return result;
    };
    let mut url = base.clone();
    let base_path = base.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{base_path}{}", case.path));
    url.set_query(None);
    if !case.query.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in &case.query {
            q.append_pair(k, v);
        }
    }
    let mut headers: Vec<(String, String)> = case.headers.clone();
    for (k, v) in &input.headers {
        headers.retain(|(x, _)| !x.eq_ignore_ascii_case(k));
        headers.push((k.clone(), v.clone()));
    }
    if let Some((ct, _)) = &case.body
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("content-type".into(), ct.clone()));
    }
    headers.retain(|(k, _)| !blocked_outbound_header(k));
    let mut recorded = app
        .redactor
        .headers(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    for (k, _) in &input.secret_headers {
        recorded.insert(k.to_ascii_lowercase(), "***".into());
    }
    let mut shown = url.clone();
    shown.set_query(url.query().map(|q| app.redactor.query(q)).as_deref());
    result.request = Some(RequestRecord {
        method: case.method.clone(),
        url: shown.to_string(),
        headers: recorded,
        body: case
            .body
            .as_ref()
            .map(|(ct, b)| app.redactor.body(b, Some(ct.as_str()), MAX_EXCERPT_BYTES)),
    });
    if let Err(e) = app.policy.check_url(&url) {
        result
            .errors
            .push(format!("the target is not allowed: {e}"));
        result.duration_ms = start.elapsed().as_millis() as u64;
        return result;
    }
    let span = tracing::info_span!(
        "fastmock.contract_test",
        otel.name = %format!("{} contract test", case.method),
        otel.kind = "client",
        http.request.method = %case.method,
        url.full = %url,
        fastmock.operation = %case.operation,
        http.response.status_code = tracing::field::Empty,
    );
    let outcome = async {
        let method = reqwest::Method::from_bytes(case.method.as_bytes())
            .map_err(|_| "invalid method".to_string())?;
        let mut rb = input
            .client
            .request(method, url.clone())
            .timeout(input.timeout);
        for (k, v) in headers.iter().chain(input.secret_headers.iter()) {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some((_, b)) = &case.body {
            rb = rb.body(b.clone());
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| format!("request failed: {}", describe(&e)))?;
        let status = resp.status().as_u16();
        tracing::Span::current().record("http.response.status_code", status);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let mut body = Vec::new();
        let mut truncated = false;
        let mut resp = resp;
        let read = tokio::time::timeout(input.timeout, async {
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| format!("reading the response failed: {}", describe(&e)))?
            {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Ok::<(), String>(())
        })
        .await;
        match read {
            Ok(r) => r?,
            Err(_) => return Err("reading the response timed out".to_string()),
        }
        Ok((status, ct, body, truncated))
    }
    .instrument(span)
    .await;
    match outcome {
        Err(e) => result.errors.push(app.redactor.scrub(&e).into_owned()),
        Ok((status, ct, body, truncated)) => {
            result.errors =
                validate_response(&case.responses, status, ct.as_deref(), &body, truncated)
                    .into_iter()
                    .map(|e| app.redactor.scrub(&e).into_owned())
                    .collect();
            result.verdict = if result.errors.is_empty() {
                Verdict::Passed
            } else {
                Verdict::Failed
            };
            result.response = Some(ResponseRecord {
                status,
                body_excerpt: app.redactor.body(&body, ct.as_deref(), MAX_EXCERPT_BYTES),
                content_type: ct,
                body_bytes: body.len(),
            });
        }
    }
    result.duration_ms = start.elapsed().as_millis() as u64;
    metrics::counter!("fastmock_contract_test_requests_total", "verdict" => result.verdict.label())
        .increment(1);
    result
}

fn describe(e: &reqwest::Error) -> String {
    let mut s = if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else if e.is_body() || e.is_decode() {
        "invalid response body".to_string()
    } else {
        "request error".to_string()
    };
    let mut src = std::error::Error::source(e);
    let mut depth = 0;
    while let Some(x) = src {
        if depth >= 4 {
            break;
        }
        s.push_str(": ");
        s.push_str(&x.to_string());
        src = x.source();
        depth += 1;
    }
    s
}

async fn run_async_case(app: &AppState, input: &RunInput, case: &AsyncCase) -> OperationResult {
    let start = Instant::now();
    let mut result = OperationResult {
        operation: case.operation.clone(),
        example: None,
        verdict: Verdict::Error,
        request: None,
        response: None,
        topic: Some(case.topic.clone()),
        messages: vec![],
        errors: vec![],
        duration_ms: 0,
    };
    let span = tracing::info_span!(
        "fastmock.contract_test",
        otel.name = %format!("consume {}", case.topic),
        otel.kind = "consumer",
        messaging.system = "kafka",
        messaging.destination.name = %case.topic,
        fastmock.operation = %case.operation,
    );
    let outcome: anyhow::Result<Vec<fastmock_kafka::ConsumedRecord>> = async {
        if !contract_topic_allowed(&app.cfg.kafka_contract_topics, &case.topic) {
            anyhow::bail!("topic is not in FASTMOCK_KAFKA_CONTRACT_TOPICS");
        }
        let bootstrap = app
            .cfg
            .kafka_bootstrap
            .clone()
            .ok_or_else(|| anyhow::anyhow!("FASTMOCK_KAFKA_BOOTSTRAP is not configured"))?;
        let client = tokio::time::timeout(
            Duration::from_secs(10),
            fastmock_kafka::KafkaPublisher::connect(&fastmock_kafka::KafkaSettings {
                bootstrap,
                client_id: "fastmock-contract".into(),
                security: app.cfg.kafka_security.clone(),
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connecting to Kafka timed out"))??;
        let from = if input.from_beginning {
            fastmock_kafka::StartFrom::Earliest
        } else {
            fastmock_kafka::StartFrom::Latest
        };
        let mut cursor = client.cursor(&case.topic, from).await?;
        client
            .consume(
                &mut cursor,
                input.message_count,
                MAX_RESPONSE_BYTES as i32,
                input.wait,
            )
            .await
    }
    .instrument(span)
    .await;
    let records = match outcome {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(topic = %case.topic, error = %format!("{e:#}"), "contract test consumer failed");
            let msg = e.to_string();
            result.errors.push(
                if msg.contains("CONTRACT_TOPICS")
                    || msg.contains("not configured")
                    || msg.contains("does not exist")
                {
                    msg
                } else {
                    "consuming from Kafka failed; see the server log".into()
                },
            );
            result.duration_ms = start.elapsed().as_millis() as u64;
            return result;
        }
    };
    let avro = match &case.avro_schema {
        Some(s) => match apache_avro::Schema::parse(s) {
            Ok(x) => Some(x),
            Err(e) => {
                result
                    .errors
                    .push(format!("the Avro schema is invalid: {e}"));
                result.duration_ms = start.elapsed().as_millis() as u64;
                return result;
            }
        },
        None => None,
    };
    let json_schema = case.payload_schema.clone();
    let mut any_invalid = false;
    for r in &records {
        let bytes = r.value.clone().unwrap_or_default();
        let mut errors = Vec::new();
        let decoded: Option<Value> = match &avro {
            Some(s) => match fastmock_kafka::decode_avro(s, &bytes) {
                Ok(v) => Some(v),
                Err(e) => {
                    errors.push(format!("{e:#}"));
                    None
                }
            },
            None => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => Some(v),
                Err(_)
                    if json_schema
                        .as_ref()
                        .is_some_and(|s| s.get("type") == Some(&json!("string"))) =>
                {
                    Some(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                }
                Err(e) => {
                    errors.push(format!("the message is not valid JSON: {e}"));
                    None
                }
            },
        };
        if let (Some(v), Some(schema)) = (&decoded, &json_schema) {
            match schema_errors(schema, v, MAX_ERRORS) {
                Ok(list) => errors.extend(list),
                Err(e) => errors.push(format!("the message schema cannot be compiled: {e}")),
            }
        }
        if !errors.is_empty() {
            any_invalid = true;
        }
        let excerpt = match &decoded {
            Some(v) if avro.is_some() => {
                let raw = serde_json::to_vec(v).unwrap_or_default();
                app.redactor
                    .body(&raw, Some("application/json"), MAX_EXCERPT_BYTES)
            }
            _ => app.redactor.body(&bytes, None, MAX_EXCERPT_BYTES),
        };
        result.messages.push(MessageRecord {
            partition: r.partition,
            offset: r.offset,
            key: r
                .key
                .as_ref()
                .map(|k| crate::redact::truncate(String::from_utf8_lossy(k).into_owned(), 256)),
            excerpt,
            errors,
        });
    }
    if records.is_empty() {
        result.verdict = Verdict::Failed;
        result.errors.push(format!(
            "no message arrived on {} within {} ms",
            case.topic,
            input.wait.as_millis()
        ));
    } else if any_invalid {
        result.verdict = Verdict::Failed;
        result.errors.push(format!(
            "{} of {} messages do not match the contract",
            result
                .messages
                .iter()
                .filter(|m| !m.errors.is_empty())
                .count(),
            records.len()
        ));
    } else {
        result.verdict = Verdict::Passed;
    }
    result.duration_ms = start.elapsed().as_millis() as u64;
    metrics::counter!("fastmock_contract_test_requests_total", "verdict" => result.verdict.label())
        .increment(1);
    result
}

async fn save(app: &AppState, run: &ContractRun) -> anyhow::Result<()> {
    app.stores
        .data
        .put(NAMESPACE, &run.id.to_string(), &serde_json::to_value(run)?)
        .await
}

async fn execute(app: AppState, mut run: ContractRun, plan: Plan, input: RunInput) {
    let start = Instant::now();
    let input = Arc::new(input);
    let gate = Arc::new(Semaphore::new(REQUEST_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    let count = match plan {
        Plan::Http(cases) => {
            let n = cases.len();
            for (i, c) in cases.into_iter().enumerate() {
                let (app, input, gate) = (app.clone(), input.clone(), gate.clone());
                set.spawn(
                    async move {
                        let _slot = gate.acquire_owned().await;
                        (i, run_http_case(&app, &input, &c).await)
                    }
                    .in_current_span(),
                );
            }
            n
        }
        Plan::Kafka(cases) => {
            let n = cases.len();
            for (i, c) in cases.into_iter().enumerate() {
                let (app, input, gate) = (app.clone(), input.clone(), gate.clone());
                set.spawn(
                    async move {
                        let _slot = gate.acquire_owned().await;
                        (i, run_async_case(&app, &input, &c).await)
                    }
                    .in_current_span(),
                );
            }
            n
        }
    };
    let work = async {
        let mut indexed: Vec<(usize, OperationResult)> = Vec::with_capacity(count);
        while let Some(r) = set.join_next().await {
            match r {
                Ok(x) => indexed.push(x),
                Err(e) => tracing::error!(error = %e, "contract test task failed"),
            }
        }
        indexed.sort_by_key(|(i, _)| *i);
        indexed.into_iter().map(|(_, r)| r).collect::<Vec<_>>()
    };
    match tokio::time::timeout(MAX_RUN_DURATION, work).await {
        Ok(results) => {
            let mut s = RunSummary {
                total: results.len(),
                ..Default::default()
            };
            for r in &results {
                match r.verdict {
                    Verdict::Passed => s.passed += 1,
                    Verdict::Failed => s.failed += 1,
                    Verdict::Error => s.errors += 1,
                }
            }
            run.status = if s.failed == 0 && s.errors == 0 {
                RunStatus::Passed
            } else if s.failed > 0 {
                RunStatus::Failed
            } else {
                RunStatus::Error
            };
            run.summary = s;
            run.results = results;
        }
        Err(_) => {
            run.status = RunStatus::Error;
            run.error = Some(format!(
                "the run exceeded {} seconds",
                MAX_RUN_DURATION.as_secs()
            ));
        }
    }
    let elapsed = start.elapsed();
    run.finished_at = Some(Utc::now());
    run.duration_ms = Some(elapsed.as_millis() as u64);
    let label = match run.status {
        RunStatus::Passed => "passed",
        RunStatus::Failed => "failed",
        _ => "error",
    };
    metrics::counter!("fastmock_contract_tests_total", "result" => label).increment(1);
    metrics::histogram!("fastmock_contract_test_duration_seconds").record(elapsed.as_secs_f64());
    tracing::info!(run_id = %run.id, status = label, total = run.summary.total, failed = run.summary.failed, "contract test finished");
    if let Err(e) = save(&app, &run).await {
        metrics::counter!("fastmock_store_errors_total", "store" => "contract_runs").increment(1);
        tracing::error!(error = %e, run_id = %run.id, "storing the contract test result failed");
    }
}

pub async fn purge(app: &AppState) -> anyhow::Result<u64> {
    let mut rows = app.stores.data.list(NAMESPACE, 100_000).await?;
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let cutoff = Utc::now() - chrono::Duration::from_std(RUN_RETENTION).unwrap_or_default();
    let stale = Utc::now() - chrono::Duration::from_std(MAX_RUN_DURATION * 2).unwrap_or_default();
    let mut deleted = 0;
    for (i, (k, v)) in rows.iter().enumerate() {
        let run: Option<ContractRun> = serde_json::from_value(v.clone()).ok();
        let old = run.as_ref().is_none_or(|r| r.started_at < cutoff);
        if i >= MAX_RUNS_KEPT || old {
            if app.stores.data.delete(NAMESPACE, k).await? {
                deleted += 1;
            }
            continue;
        }
        if let Some(mut r) = run
            && r.status == RunStatus::Running
            && r.started_at < stale
        {
            r.status = RunStatus::Error;
            r.error = Some("the run was interrupted (the replica running it stopped)".into());
            save(app, &r).await?;
        }
    }
    Ok(deleted)
}

/// Topics contract tests may read. An empty list disables reading; `prefix*` patterns never match
/// internal topics (starting with `_`) unless the prefix itself starts with `_`.
pub fn contract_topic_allowed(allowed: &[String], topic: &str) -> bool {
    allowed.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => {
            topic.starts_with(prefix) && (!topic.starts_with('_') || prefix.starts_with('_'))
        }
        None => topic == p,
    })
}

pub fn bound_host_matches(pattern: &str, host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let pattern = pattern.trim_start_matches('[').trim_end_matches(']');
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => host == pattern,
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// True when `addr` reaches one of this process's listeners: the data or admin port on a loopback,
/// unspecified or local interface address.
pub fn is_own_listener(cfg: &crate::config::Config, addr: SocketAddr) -> bool {
    let own: Vec<SocketAddr> = vec![cfg.listen, cfg.admin_listen];
    if !own.iter().any(|o| o.port() == addr.port() && o.port() != 0) {
        return false;
    }
    let ip = canonical(addr.ip());
    ip.is_loopback()
        || ip.is_unspecified()
        || own.iter().any(|o| canonical(o.ip()) == ip)
        || std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok()
}

async fn pinned_client(app: &AppState, url: &reqwest::Url) -> ApiResult<reqwest::Client> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| bad("targetUrl has no port"))?;
    let host = url.host_str().ok_or_else(|| bad("targetUrl has no host"))?;
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    let (domain, addrs): (Option<String>, Vec<SocketAddr>) = match literal.parse::<IpAddr>() {
        Ok(ip) => (None, vec![SocketAddr::new(ip, port)]),
        Err(_) => {
            let found = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| bad(format!("targetUrl: resolving {host} timed out")))?
            .map_err(|_| bad(format!("targetUrl: {host} cannot be resolved")))?
            .collect::<Vec<_>>();
            (Some(host.to_string()), found)
        }
    };
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| app.policy.ip_allowed(a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(bad(
            "targetUrl: all addresses of the host are denied by FASTMOCK_CALLBACK_DENIED_CIDRS",
        ));
    }
    if allowed.iter().any(|a| is_own_listener(&app.cfg, *a)) {
        metrics::counter!("fastmock_contract_tests_rejected_total", "reason" => "self_target")
            .increment(1);
        return Err(bad(
            "targetUrl points at this FastMock instance (its data or admin port); contract tests must target the service under test",
        ));
    }
    let roots = crate::outbox::load_outbound_ca(app.cfg.outbound_ca_file.as_deref())?;
    let mut builder =
        crate::outbox::client_builder(app.policy.clone(), MAX_RUN_CLIENT_TIMEOUT, &roots);
    if let Some(d) = domain {
        builder = builder.resolve_to_addrs(&d, &allowed);
    }
    Ok(builder
        .build()
        .map_err(|e| anyhow::anyhow!("building the contract test client failed: {e}"))?)
}

fn secret_headers(
    app: &AppState,
    req: &ContractTestRequest,
    target: Option<&reqwest::Url>,
) -> ApiResult<Vec<(String, String)>> {
    if req.secret_headers.is_empty() {
        return Ok(Vec::new());
    }
    let Some(target) = target else {
        return Err(bad("secretHeaders need an HTTP targetUrl"));
    };
    let host = target.host_str().unwrap_or("");
    let store = crate::state::Secrets::new(app.cfg.secrets_dir.clone());
    let mut out = Vec::new();
    for (k, name) in &req.secret_headers {
        if blocked_outbound_header(k) {
            return Err(bad(blocked_reason(k)));
        }
        let unknown = || {
            bad(format!(
                "secret `{name}` is not configured or not listed in FASTMOCK_CONTRACT_SECRETS"
            ))
        };
        let binding = app
            .cfg
            .contract_secrets
            .iter()
            .find(|c| &c.name == name)
            .ok_or_else(unknown)?;
        if !binding.hosts.iter().any(|h| bound_host_matches(h, host)) {
            metrics::counter!("fastmock_contract_tests_rejected_total", "reason" => "secret_host")
                .increment(1);
            return Err(bad(format!(
                "secret `{name}` is not bound to host {host} in FASTMOCK_CONTRACT_SECRETS"
            )));
        }
        let v = store.get(name).ok_or_else(unknown)?;
        if !header_ok(k, &v) {
            return Err(bad(format!("invalid secret header {k}")));
        }
        app.redactor
            .register_literal(&v)
            .map_err(|e| bad(format!("secret `{name}`: {e}")))?;
        out.push((k.clone(), v));
    }
    Ok(out)
}

/// The view of a run for viewers: verdicts, statuses and counts only.
pub fn viewer_view(run: &mut ContractRun) {
    run.details_hidden = true;
    for r in &mut run.results {
        r.request = None;
        r.errors.clear();
        if let Some(resp) = &mut r.response {
            resp.body_excerpt.clear();
            resp.content_type = None;
        }
        for m in &mut r.messages {
            m.key = None;
            m.excerpt.clear();
            m.errors.clear();
        }
    }
}

async fn load_spec_ref(app: &AppState, id: &str) -> ApiResult<Value> {
    if id.is_empty() || id.len() > 256 {
        return Err(bad("specRef must be 1..256 characters"));
    }
    let v = app
        .stores
        .data
        .get(SPECS_NAMESPACE, id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("spec {id} not found")))?;
    let doc = v
        .get("document")
        .ok_or_else(|| bad(format!("spec {id} has no document")))?;
    parse_spec(doc)
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContractTestStarted {
    pub id: Uuid,
    pub status: RunStatus,
    pub kind: String,
    pub operations: usize,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContractRunList {
    pub runs: Vec<ContractRun>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RunListQuery {
    pub limit: Option<usize>,
}

#[utoipa::path(
    post,
    path = "/__admin/contract-tests",
    tag = "contract-tests",
    operation_id = "startContractTest",
    summary = "Start a contract test run",
    request_body = ContractTestRequest,
    responses(
        (status = 202, description = "The run started in the background", body = ContractTestStarted),
        (status = 400, description = "Invalid request or document", body = ErrorBody),
        (status = 404, description = "specRef not found", body = ErrorBody),
        (status = 429, description = "Too many runs in progress", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The token lacks the editor role, or the admin role when secretHeaders is set", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn start(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(req): Json<ContractTestRequest>,
) -> ApiResult<Response> {
    if !req.secret_headers.is_empty() {
        require(&p, Role::Admin).map_err(|_| {
            ApiError(
                StatusCode::FORBIDDEN,
                "secretHeaders require the admin role".into(),
            )
        })?;
    }
    let doc = match (&req.spec, &req.spec_ref) {
        (Some(s), None) => parse_spec(s)?,
        (None, Some(r)) => load_spec_ref(&app, r).await?,
        _ => return Err(bad("set exactly one of spec and specRef")),
    };
    if req.operations.len() > MAX_OPERATIONS_PER_RUN {
        return Err(bad(format!(
            "at most {MAX_OPERATIONS_PER_RUN} operations can be selected"
        )));
    }
    if req.headers.len() + req.secret_headers.len() > MAX_HEADERS {
        return Err(bad(format!("at most {MAX_HEADERS} headers")));
    }
    for (k, v) in &req.headers {
        if blocked_outbound_header(k) {
            return Err(bad(blocked_reason(k)));
        }
        if !header_ok(k, v) {
            return Err(bad(format!("invalid header {k}")));
        }
    }
    let timeout_ms = req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(bad(format!(
            "timeoutMs must be between 1 and {MAX_TIMEOUT_MS}"
        )));
    }
    let message_count = req.message_count.unwrap_or(1);
    if message_count == 0 || message_count > MAX_ASYNC_MESSAGES {
        return Err(bad(format!(
            "messageCount must be between 1 and {MAX_ASYNC_MESSAGES}"
        )));
    }
    let wait_ms = req.wait_ms.unwrap_or(DEFAULT_WAIT_MS);
    if wait_ms == 0 || wait_ms > MAX_WAIT_MS {
        return Err(bad(format!("waitMs must be between 1 and {MAX_WAIT_MS}")));
    }
    let is_async = doc.get("asyncapi").is_some();
    let target = match &req.target_url {
        Some(t) => {
            let u = reqwest::Url::parse(t).map_err(|e| bad(format!("targetUrl: {e}")))?;
            app.policy
                .check_url(&u)
                .map_err(|e| bad(format!("targetUrl: {e}")))?;
            Some(u)
        }
        None if is_async => None,
        None => return Err(bad("targetUrl is required for OpenAPI contract tests")),
    };
    let client = match &target {
        Some(u) => pinned_client(&app, u).await?,
        None => app.http.clone(),
    };
    let secret_headers = secret_headers(&app, &req, target.as_ref())?;
    let filter = req.operations.clone();
    let service = format!(
        "{}:{}",
        doc.pointer("/info/title")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        doc.pointer("/info/version")
            .map(fastmock_core::matcher::value_text)
            .unwrap_or_default()
    );
    let plan = tokio::task::spawn_blocking(move || -> anyhow::Result<Plan> {
        if is_async {
            Ok(Plan::Kafka(plan_asyncapi(&doc, &filter)?))
        } else {
            Ok(Plan::Http(plan_openapi(&doc, &filter)?))
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("planning failed: {e}"))?
    .map_err(|e| bad(format!("{e:#}")))?;
    let operations = match &plan {
        Plan::Http(c) => c.len(),
        Plan::Kafka(c) => c.len(),
    };
    if let Plan::Kafka(cases) = &plan
        && let Some(c) = cases
            .iter()
            .find(|c| !contract_topic_allowed(&app.cfg.kafka_contract_topics, &c.topic))
    {
        return Err(bad(format!(
            "topic {} is not in FASTMOCK_KAFKA_CONTRACT_TOPICS; contract tests may read only the topics listed there",
            c.topic
        )));
    }
    let Ok(permit) = RUNS.clone().try_acquire_owned() else {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            format!("at most {MAX_CONCURRENT_RUNS} contract test runs can run at once"),
        ));
    };
    if let Err(e) = purge(&app).await {
        tracing::warn!(error = %e, "contract test retention failed");
    }
    let kind = if is_async { "asyncapi" } else { "openapi" }.to_string();
    let run = ContractRun {
        id: Uuid::now_v7(),
        kind: kind.clone(),
        status: RunStatus::Running,
        service: Some(service),
        spec_ref: req.spec_ref.clone(),
        target_url: target.as_ref().map(|u| {
            let mut shown = u.clone();
            shown.set_query(u.query().map(|q| app.redactor.query(q)).as_deref());
            shown.to_string()
        }),
        started_at: Utc::now(),
        finished_at: None,
        duration_ms: None,
        started_by: p.name.clone(),
        summary: RunSummary {
            total: operations,
            ..Default::default()
        },
        error: None,
        results: vec![],
        details_hidden: false,
    };
    save(&app, &run).await?;
    audit(
        &app,
        &p,
        "contract_test.start",
        json!({"id": run.id, "kind": kind, "targetUrl": run.target_url, "operations": operations,
            "secretHeaders": secret_headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect::<Vec<_>>()}),
    )
    .await;
    let input = RunInput {
        target,
        client,
        headers: req.headers.clone().into_iter().collect(),
        secret_headers,
        timeout: Duration::from_millis(timeout_ms),
        message_count,
        wait: Duration::from_millis(wait_ms),
        from_beginning: req.from_beginning,
    };
    let started = ContractTestStarted {
        id: run.id,
        status: RunStatus::Running,
        kind,
        operations,
    };
    let span = tracing::info_span!("fastmock.contract_run", fastmock.run_id = %run.id);
    let task_app = app.clone();
    tokio::spawn(
        async move {
            let _permit = permit;
            execute(task_app, run, plan, input).await;
        }
        .instrument(span),
    );
    Ok((StatusCode::ACCEPTED, Json(started)).into_response())
}

#[utoipa::path(
    get,
    path = "/__admin/contract-tests",
    tag = "contract-tests",
    operation_id = "listContractTests",
    summary = "List contract test runs",
    params(RunListQuery),
    responses(
        (status = 200, description = "Runs, newest first, without per-operation results", body = ContractRunList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list(
    State(app): State<AppState>,
    Query(q): Query<RunListQuery>,
) -> ApiResult<Json<ContractRunList>> {
    let mut rows = app.stores.data.list(NAMESPACE, 100_000).await?;
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let limit = q.limit.unwrap_or(50).clamp(1, MAX_RUNS_KEPT);
    let runs = rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<ContractRun>(v).ok())
        .take(limit)
        .map(|mut r| {
            r.results.clear();
            r
        })
        .collect();
    Ok(Json(ContractRunList { runs }))
}

#[utoipa::path(
    get,
    path = "/__admin/contract-tests/{id}",
    tag = "contract-tests",
    operation_id = "getContractTest",
    summary = "One contract test run with results",
    params(("id" = Uuid, Path, description = "Run id")),
    responses(
        (status = 200, description = "The run with per-operation results. Viewers get verdicts, statuses and counts only (`detailsHidden: true`): request and response excerpts, message excerpts and error texts are reserved to editors.", body = ContractRun),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_run(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let v = app
        .stores
        .data
        .get(NAMESPACE, &id.to_string())
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "contract test run not found".into()))?;
    if p.role >= Role::Editor {
        return Ok(Json(v));
    }
    let mut run: ContractRun = serde_json::from_value(v)
        .map_err(|e| anyhow::anyhow!("stored contract run is invalid: {e}"))?;
    viewer_view(&mut run);
    Ok(Json(
        serde_json::to_value(run).map_err(anyhow::Error::from)?,
    ))
}

#[utoipa::path(
    delete,
    path = "/__admin/contract-tests/{id}",
    tag = "contract-tests",
    operation_id = "deleteContractTest",
    summary = "Delete a contract test run",
    params(("id" = Uuid, Path, description = "Run id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The token lacks the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_run(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !app.stores.data.delete(NAMESPACE, &id.to_string()).await? {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "contract test run not found".into(),
        ));
    }
    audit(&app, &p, "contract_test.delete", json!({"id": id})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/contract-tests", post(start).get(list))
        .route("/contract-tests/{id}", get(get_run).delete(delete_run))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    tags((name = "contract-tests", description = "Contract tests of real services against OpenAPI and AsyncAPI documents")),
    paths(start, list, get_run, delete_run),
    components(schemas(
        ContractTestRequest,
        ContractTestStarted,
        ContractRunList,
        ContractRun,
        RunStatus,
        RunSummary,
        OperationResult,
        Verdict,
        RequestRecord,
        ResponseRecord,
        MessageRecord,
    ))
)]
pub struct ContractApi;

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r##"
openapi: 3.0.3
info: {title: Pets, version: "1"}
paths:
  /pets/{id}:
    get:
      operationId: getPet
      parameters:
        - {name: id, in: path, required: true, schema: {type: integer}, example: 7}
        - {name: x-tenant, in: header, required: true, schema: {type: string, example: acme}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Pet"}
        "4XX": {description: client error}
  /pets:
    post:
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Pet"}
            examples:
              rex: {value: {id: 1, name: rex}}
              tom: {value: {id: 2, name: tom}}
      responses:
        "201": {description: created}
components:
  schemas:
    Pet:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
"##;

    #[test]
    fn plans_one_request_per_operation_and_example() {
        let doc = fastmock_core::openapi::parse_document(SPEC).unwrap();
        let cases = plan_openapi(&doc, &[]).unwrap();
        assert_eq!(cases.len(), 3);
        let get = &cases[0];
        assert_eq!(get.operation, "getPet");
        assert_eq!(get.path, "/pets/7");
        assert_eq!(
            get.headers,
            vec![("x-tenant".to_string(), "acme".to_string())]
        );
        let post: Vec<&HttpCase> = cases.iter().filter(|c| c.method == "POST").collect();
        assert_eq!(post.len(), 2);
        assert_eq!(post[0].operation, "POST /pets");
        assert_eq!(post[0].example.as_deref(), Some("rex"));
        assert_eq!(
            post[1].body.as_ref().unwrap().1,
            br#"{"id":2,"name":"tom"}"#.to_vec()
        );
        let only = plan_openapi(&doc, &["post /pets".to_string()]).unwrap();
        assert_eq!(only.len(), 2);
        assert!(plan_openapi(&doc, &["nothing".to_string()]).is_err());
    }

    #[test]
    fn validates_status_content_type_and_body() {
        let doc = fastmock_core::openapi::parse_document(SPEC).unwrap();
        let cases = plan_openapi(&doc, &["getPet".to_string()]).unwrap();
        let r = &cases[0].responses;
        assert!(
            validate_response(
                r,
                200,
                Some("application/json"),
                br#"{"id":1,"name":"a"}"#,
                false
            )
            .is_empty()
        );
        let e = validate_response(r, 200, Some("application/json"), br#"{"id":"1"}"#, false);
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(
            validate_response(r, 404, None, b"", false).is_empty(),
            "4XX range"
        );
        let e = validate_response(r, 500, None, b"", false);
        assert!(e[0].contains("not declared"), "{e:?}");
        let e = validate_response(r, 200, Some("text/plain"), b"hi", false);
        assert!(e[0].contains("content type"), "{e:?}");
        let e = validate_response(r, 200, Some("application/json"), b"{", false);
        assert!(e[0].contains("not valid JSON"), "{e:?}");
        let e = validate_response(r, 200, Some("application/json; charset=utf-8"), b"{}", true);
        assert!(e[0].contains("not validated"), "{e:?}");
        assert!(media_matches("application/*", "application/json"));
        assert!(!media_matches("text/*", "application/json"));
    }

    #[test]
    fn contract_topics_are_an_explicit_allow_list() {
        assert!(!contract_topic_allowed(&[], "orders"));
        let l = vec!["orders-*".to_string(), "payments".to_string()];
        assert!(contract_topic_allowed(&l, "orders-eu"));
        assert!(contract_topic_allowed(&l, "payments"));
        assert!(!contract_topic_allowed(&l, "payments-dlq"));
        let all = vec!["*".to_string()];
        assert!(contract_topic_allowed(&all, "anything"));
        assert!(!contract_topic_allowed(&all, "__consumer_offsets"));
        assert!(!contract_topic_allowed(&all, "_schemas"));
        assert!(contract_topic_allowed(
            &["_schemas*".to_string()],
            "_schemas"
        ));
    }

    #[test]
    fn secrets_bind_to_exact_hosts_or_subdomains() {
        assert!(bound_host_matches("api.example.com", "API.example.com."));
        assert!(!bound_host_matches(
            "api.example.com",
            "api.example.com.evil.net"
        ));
        assert!(bound_host_matches("*.example.com", "a.b.example.com"));
        assert!(!bound_host_matches("*.example.com", "example.com"));
        assert!(bound_host_matches("[::1]", "[::1]"));
        assert!(bound_host_matches("::1", "[::1]"));
    }

    #[test]
    fn own_listeners_are_recognised() {
        let mut cfg = crate::config::Config::for_tests(crate::config::StorageKind::Memory, vec![]);
        cfg.listen = "0.0.0.0:18080".parse().unwrap();
        cfg.admin_listen = "127.0.0.1:18090".parse().unwrap();
        let own = |a: &str| is_own_listener(&cfg, a.parse().unwrap());
        assert!(own("127.0.0.1:18080"));
        assert!(own("127.8.9.10:18090"));
        assert!(own("[::ffff:127.0.0.1]:18080"));
        assert!(own("[::1]:18080"));
        assert!(own("0.0.0.0:18080"));
        assert!(!own("127.0.0.1:18081"));
        assert!(!own("198.51.100.7:18080"), "not a local address");
        if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
            && s.connect("198.51.100.1:9").is_ok()
            && let Ok(local) = s.local_addr()
            && !local.ip().is_unspecified()
        {
            assert!(
                own(&format!("{}:18080", local.ip())),
                "an interface address of this host"
            );
        }
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        assert!(!is_own_listener(&cfg, "127.0.0.1:0".parse().unwrap()));
    }

    #[test]
    fn viewers_get_verdicts_only() {
        let mut run: ContractRun = serde_json::from_value(json!({
            "id": Uuid::now_v7(), "kind": "openapi", "status": "failed",
            "startedAt": Utc::now(), "startedBy": "ci",
            "summary": {"total": 1, "passed": 0, "failed": 1, "errors": 0},
            "results": [{"operation": "getPet", "verdict": "failed", "durationMs": 3,
                "errors": ["body /card: \"4111111111111111\" is not of type integer"],
                "request": {"method": "GET", "url": "http://a.example.com/p", "headers": {}},
                "response": {"status": 200, "contentType": "application/json", "bodyExcerpt": "{\"card\":\"4111\"}", "bodyBytes": 20},
                "topic": "t", "messages": [{"partition": 0, "offset": 1, "key": "k", "excerpt": "secret", "errors": ["x"]}]}]
        }))
        .unwrap();
        viewer_view(&mut run);
        let v = serde_json::to_value(&run).unwrap();
        assert_eq!(v["detailsHidden"], true);
        let r = &v["results"][0];
        assert_eq!(r["verdict"], "failed");
        assert_eq!(r["response"]["status"], 200);
        assert_eq!(r["response"]["bodyExcerpt"], "");
        assert!(r.get("request").is_none());
        assert_eq!(r["errors"], json!([]));
        assert_eq!(r["messages"][0]["excerpt"], "");
        assert!(!v.to_string().contains("4111"));
    }

    #[test]
    fn encodes_path_components() {
        assert_eq!(encode_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_component("ok-1.2_~"), "ok-1.2_~");
    }
}
