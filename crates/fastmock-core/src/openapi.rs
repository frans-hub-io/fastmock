use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::microcks::{
    Dispatcher, Flavor, OperationMeta, convert_json, convert_text, json_body_cases,
    parse_dispatcher, set_body,
};
use crate::model::{BodyMatcher, Delay, RequestPattern, ResponseDef, StringMatcher, Stub};

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];
const MAX_DEPTH: usize = 24;

#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub tag: String,
    pub validate_requests: bool,
    pub path_prefix: String,
}

#[derive(Debug, Default)]
pub struct ImportResult {
    pub stubs: Vec<Stub>,
    pub warnings: Vec<String>,
}

pub fn parse_document(text: &str) -> anyhow::Result<Value> {
    let t = text.trim_start();
    if t.starts_with('{') {
        return Ok(serde_json::from_str(text)?);
    }
    Ok(serde_yaml_ng::from_str::<Value>(text)?)
}

fn pointer<'a>(doc: &'a Value, r: &str) -> Option<&'a Value> {
    let p = r.strip_prefix('#')?;
    doc.pointer(p)
}

pub const MAX_RESOLVED_NODES: usize = 200_000;
pub const MAX_RESOLVED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ResolveBudget {
    bytes: usize,
}

impl Default for ResolveBudget {
    fn default() -> Self {
        Self::new(MAX_RESOLVED_BYTES)
    }
}

impl ResolveBudget {
    pub fn new(bytes: usize) -> Self {
        Self { bytes }
    }

    pub fn remaining(&self) -> usize {
        self.bytes
    }
}

struct Limits<'b> {
    nodes: usize,
    bytes: &'b mut usize,
    exceeded: Option<&'static str>,
}

impl Limits<'_> {
    fn charge(&mut self, bytes: usize) -> bool {
        if self.exceeded.is_some() {
            return false;
        }
        if self.nodes == 0 {
            self.exceeded = Some("nodes");
            return false;
        }
        if bytes > *self.bytes {
            *self.bytes = 0;
            self.exceeded = Some("bytes");
            return false;
        }
        self.nodes -= 1;
        *self.bytes -= bytes;
        true
    }
}

pub fn resolve(doc: &Value, v: &Value, depth: usize) -> anyhow::Result<Value> {
    resolve_budgeted(doc, v, depth, &mut ResolveBudget::default())
}

pub fn resolve_budgeted(
    doc: &Value,
    v: &Value,
    depth: usize,
    budget: &mut ResolveBudget,
) -> anyhow::Result<Value> {
    let mut limits = Limits {
        nodes: MAX_RESOLVED_NODES,
        bytes: &mut budget.bytes,
        exceeded: None,
    };
    let out = resolve_with(doc, v, depth, &mut Vec::new(), &mut limits);
    match limits.exceeded {
        Some("nodes") => anyhow::bail!(
            "$ref expansion exceeds {MAX_RESOLVED_NODES} nodes; the document references too many schemas"
        ),
        Some(_) => anyhow::bail!("the resolved document exceeds 64 MiB"),
        None => Ok(out),
    }
}

fn resolve_with(
    doc: &Value,
    v: &Value,
    depth: usize,
    stack: &mut Vec<String>,
    limits: &mut Limits,
) -> Value {
    let size = match v {
        Value::String(s) => s.len() + 2,
        _ => 8,
    };
    if depth > MAX_DEPTH * 4 || !limits.charge(size) {
        return json!({});
    }
    match v {
        Value::Object(o) => {
            if let Some(Value::String(r)) = o.get("$ref") {
                if stack.contains(r) {
                    return json!({});
                }
                return match pointer(doc, r) {
                    Some(target) => {
                        stack.push(r.clone());
                        let out = resolve_with(doc, target, depth + 1, stack, limits);
                        stack.pop();
                        out
                    }
                    None => json!({}),
                };
            }
            let mut m = Map::new();
            for (k, x) in o {
                if !limits.charge(k.len() + 4) {
                    break;
                }
                m.insert(k.clone(), resolve_with(doc, x, depth + 1, stack, limits));
            }
            Value::Object(m)
        }
        Value::Array(a) => {
            let mut out = Vec::new();
            for x in a {
                if limits.exceeded.is_some() {
                    break;
                }
                out.push(resolve_with(doc, x, depth + 1, stack, limits));
            }
            Value::Array(out)
        }
        other => other.clone(),
    }
}

pub fn to_json_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, v) in o {
                match k.as_str() {
                    "nullable" | "discriminator" | "xml" | "externalDocs" | "example" => {}
                    _ => {
                        m.insert(k.clone(), to_json_schema(v));
                    }
                }
            }
            if o.get("nullable").and_then(|n| n.as_bool()) == Some(true) {
                match m.get("type").cloned() {
                    Some(Value::String(t)) => {
                        m.insert("type".into(), json!([t, "null"]));
                    }
                    Some(Value::Array(mut a)) => {
                        if !a.contains(&json!("null")) {
                            a.push(json!("null"));
                        }
                        m.insert("type".into(), Value::Array(a));
                    }
                    _ => {}
                }
                if let Some(Value::Array(e)) = m.get_mut("enum")
                    && !e.contains(&Value::Null)
                {
                    e.push(Value::Null);
                }
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(to_json_schema).collect()),
        other => other.clone(),
    }
}

pub fn example(schema: &Value, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    let Some(o) = schema.as_object() else {
        return Value::Null;
    };
    if let Some(e) = o.get("example") {
        return e.clone();
    }
    if let Some(Value::Array(e)) = o.get("examples")
        && let Some(first) = e.first()
    {
        return first.clone();
    }
    if let Some(d) = o.get("default") {
        return d.clone();
    }
    if let Some(Value::Array(e)) = o.get("enum")
        && let Some(first) = e.first()
    {
        return first.clone();
    }
    if let Some(Value::String(c)) = o.get("const") {
        return Value::String(c.clone());
    }
    if let Some(Value::Array(all)) = o.get("allOf") {
        let mut merged = Map::new();
        for s in all {
            if let Value::Object(x) = example(s, depth + 1) {
                merged.extend(x);
            }
        }
        return Value::Object(merged);
    }
    for k in ["oneOf", "anyOf"] {
        if let Some(Value::Array(a)) = o.get(k)
            && let Some(first) = a.first()
        {
            return example(first, depth + 1);
        }
    }
    let ty = match o.get("type") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .find(|t| *t != "null")
            .unwrap_or("null")
            .to_string(),
        _ if o.contains_key("properties") => "object".into(),
        _ if o.contains_key("items") => "array".into(),
        _ => return Value::Null,
    };
    match ty.as_str() {
        "object" => {
            let mut m = Map::new();
            if let Some(Value::Object(props)) = o.get("properties") {
                for (k, s) in props {
                    if s.get("writeOnly").and_then(|x| x.as_bool()) != Some(true) {
                        m.insert(k.clone(), example(s, depth + 1));
                    }
                }
            }
            Value::Object(m)
        }
        "array" => match o.get("items") {
            Some(i) => Value::Array(vec![example(i, depth + 1)]),
            None => json!([]),
        },
        "string" => Value::String(
            match o.get("format").and_then(|f| f.as_str()) {
                Some("date-time") => "2026-01-01T00:00:00Z",
                Some("date") => "2026-01-01",
                Some("time") => "00:00:00",
                Some("uuid") => "3fa85f64-5717-4562-b3fc-2c963f66afa6",
                Some("email") => "user@example.com",
                Some("uri") | Some("url") => "https://example.com",
                Some("hostname") => "example.com",
                Some("ipv4") => "192.0.2.1",
                Some("ipv6") => "2001:db8::1",
                Some("byte") => "c3RyaW5n",
                _ => "string",
            }
            .to_string(),
        ),
        "integer" => o
            .get("minimum")
            .cloned()
            .filter(|v| v.is_i64() || v.is_u64())
            .unwrap_or(json!(0)),
        "number" => o.get("minimum").cloned().unwrap_or(json!(0.0)),
        "boolean" => json!(true),
        _ => Value::Null,
    }
}

fn param_matcher(schema: Option<&Value>) -> StringMatcher {
    let mut m = StringMatcher::default();
    let Some(s) = schema else {
        return m;
    };
    if let Some(Value::Array(e)) = s.get("enum") {
        let alts: Vec<String> = e
            .iter()
            .map(|x| regex::escape(&crate::matcher::value_text(x)))
            .collect();
        if !alts.is_empty() {
            m.matches = Some(alts.join("|"));
            return m;
        }
    }
    match s.get("type").and_then(|t| t.as_str()) {
        Some("integer") => m.matches = Some("-?[0-9]+".into()),
        Some("number") => m.matches = Some("-?[0-9]+(\\.[0-9]+)?([eE][-+]?[0-9]+)?".into()),
        Some("boolean") => m.matches = Some("true|false".into()),
        _ => {
            if let Some(Value::String(p)) = s.get("pattern") {
                let p = p.trim_start_matches('^').trim_end_matches('$');
                if regex::Regex::new(p).is_ok() {
                    m.matches = Some(format!(".*(?:{p}).*"));
                }
            }
        }
    }
    m
}

fn pick_response(op: &Value) -> Option<(u16, Value)> {
    let responses = op.get("responses")?.as_object()?;
    let mut codes: Vec<(&String, &Value)> = responses.iter().collect();
    codes.sort_by_key(|(c, _)| c.as_str().to_string());
    if let Some((c, r)) = codes.iter().find(|(c, _)| c.starts_with('2')) {
        return Some((
            c.parse().ok().or_else(|| {
                c.chars()
                    .next()
                    .and_then(|x| x.to_digit(10))
                    .map(|d| d as u16 * 100)
            })?,
            (*r).clone(),
        ));
    }
    responses.get("default").map(|r| (200, r.clone()))
}

fn media(content: Option<&Value>) -> Option<(String, Value)> {
    let c = content?.as_object()?;
    let preferred = c
        .iter()
        .find(|(k, _)| k.starts_with("application/json") || k.ends_with("+json"))
        .or_else(|| c.iter().next())?;
    Some((preferred.0.clone(), preferred.1.clone()))
}

fn media_example(m: &Value) -> Value {
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

#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub operations: BTreeMap<String, OperationMeta>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServiceInfo {
    pub name: String,
    pub version: String,
    pub labels: BTreeMap<String, String>,
}

pub fn service_info(doc: &Value) -> ServiceInfo {
    let info = doc.get("info");
    let text = |k: &str| {
        info.and_then(|i| i.get(k))
            .map(crate::matcher::value_text)
            .unwrap_or_default()
    };
    let labels = info
        .and_then(|i| i.get("x-microcks"))
        .and_then(|x| x.get("labels"))
        .and_then(|l| l.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), crate::matcher::value_text(v)))
                .collect()
        })
        .unwrap_or_default();
    ServiceInfo {
        name: text("title"),
        version: text("version"),
        labels,
    }
}

pub fn detect(text: &str) -> bool {
    let head: String = text.chars().take(4096).collect();
    head.contains("openapi") && (head.contains("\"openapi\"") || head.contains("openapi:"))
}

fn status_of(code: &str, has_2xx: bool) -> u16 {
    if let Ok(c) = code.parse::<u16>()
        && (100..=599).contains(&c)
    {
        return c;
    }
    if code == "default" {
        return if has_2xx { 500 } else { 200 };
    }
    code.chars()
        .next()
        .and_then(|x| x.to_digit(10))
        .map(|d| d as u16 * 100)
        .filter(|c| (100..=599).contains(c))
        .unwrap_or(200)
}

#[derive(Clone, Debug, Default)]
struct RespExample {
    status: u16,
    media: Option<String>,
    value: Option<Value>,
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
struct ReqExample {
    path: BTreeMap<String, String>,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Option<Value>,
}

fn example_value(ex: &Value, ctx: &str, warnings: &mut Vec<String>) -> Option<Value> {
    if let Some(v) = ex.get("value") {
        return Some(v.clone());
    }
    if ex.get("externalValue").is_some() {
        warnings.push(format!(
            "{ctx}: externalValue examples are never fetched; example skipped"
        ));
    }
    None
}

fn response_examples(
    op: &Value,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> Vec<(String, RespExample)> {
    let mut out: Vec<(String, RespExample)> = Vec::new();
    let Some(responses) = op.get("responses").and_then(|r| r.as_object()) else {
        return out;
    };
    let has_2xx = responses.keys().any(|c| c.starts_with('2'));
    let mut codes: Vec<(&String, &Value)> = responses.iter().collect();
    codes.sort_by_key(|(c, _)| c.as_str().to_string());
    for (code, r) in codes {
        let status = status_of(code, has_2xx);
        let first_media = r
            .get("content")
            .and_then(|c| c.as_object())
            .and_then(|c| c.keys().next().cloned());
        for (mt, m) in r
            .get("content")
            .and_then(|c| c.as_object())
            .into_iter()
            .flatten()
        {
            for (name, ex) in m
                .get("examples")
                .and_then(|e| e.as_object())
                .into_iter()
                .flatten()
            {
                if out.iter().any(|(n, _)| n == name) {
                    continue;
                }
                let Some(value) = example_value(ex, &format!("{ctx} / {name}"), warnings) else {
                    continue;
                };
                out.push((
                    name.clone(),
                    RespExample {
                        status,
                        media: Some(mt.clone()),
                        value: Some(value),
                        headers: BTreeMap::new(),
                    },
                ));
            }
        }
        for name in r
            .get("x-microcks-refs")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str())
        {
            if !out.iter().any(|(n, _)| n == name) {
                out.push((
                    name.to_string(),
                    RespExample {
                        status,
                        media: first_media.clone(),
                        value: None,
                        headers: BTreeMap::new(),
                    },
                ));
            }
        }
        for (h, hdef) in r
            .get("headers")
            .and_then(|h| h.as_object())
            .into_iter()
            .flatten()
        {
            for (name, ex) in hdef
                .get("examples")
                .and_then(|e| e.as_object())
                .into_iter()
                .flatten()
            {
                if let Some((_, rx)) = out
                    .iter_mut()
                    .find(|(n, x)| n == name && x.status == status)
                    && let Some(v) = ex.get("value")
                {
                    rx.headers
                        .insert(h.to_ascii_lowercase(), crate::matcher::value_text(v));
                }
            }
        }
    }
    out
}

fn request_examples(
    params: &[Value],
    op: &Value,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, ReqExample> {
    let mut out: BTreeMap<String, ReqExample> = BTreeMap::new();
    for p in params {
        let (Some(pname), Some(loc)) = (
            p.get("name").and_then(|v| v.as_str()),
            p.get("in").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        for (name, ex) in p
            .get("examples")
            .and_then(|e| e.as_object())
            .into_iter()
            .flatten()
        {
            let Some(v) = example_value(ex, &format!("{ctx} / {name}"), warnings) else {
                continue;
            };
            let v = crate::matcher::value_text(&v);
            let e = out.entry(name.clone()).or_default();
            match loc {
                "path" => e.path.insert(pname.to_string(), v),
                "query" => e.query.insert(pname.to_string(), v),
                "header" => e.headers.insert(pname.to_ascii_lowercase(), v),
                _ => None,
            };
        }
    }
    if let Some(content) = op
        .get("requestBody")
        .and_then(|b| b.get("content"))
        .and_then(|c| c.as_object())
    {
        for (_, m) in content {
            for (name, ex) in m
                .get("examples")
                .and_then(|e| e.as_object())
                .into_iter()
                .flatten()
            {
                let e = out.entry(name.clone()).or_default();
                if e.body.is_none() {
                    e.body = example_value(ex, &format!("{ctx} / {name}"), warnings);
                }
            }
        }
    }
    out
}

fn equal(v: &str) -> StringMatcher {
    StringMatcher {
        equal_to: Some(v.to_string()),
        ..Default::default()
    }
}

fn json_of(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(|x| x.is_object() || x.is_array()),
        Value::Object(_) | Value::Array(_) => Some(v.clone()),
        _ => None,
    }
}

fn build_response(
    rx: &RespExample,
    delay: Option<u64>,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> ResponseDef {
    let mut resp = ResponseDef {
        status: rx.status,
        templated: false,
        ..Default::default()
    };
    resp.headers.extend(rx.headers.clone());
    if let Some(mt) = &rx.media
        && rx.value.is_some()
    {
        resp.headers.insert("content-type".into(), mt.clone());
    }
    if let Some(v) = &rx.value {
        let flavor = if rx.media.as_deref().is_some_and(|m| m.contains("xml")) {
            Flavor::Xml
        } else {
            Flavor::Json
        };
        set_body(&mut resp, rx.media.as_deref(), v, flavor, ctx, warnings);
    }
    if let Some(ms) = delay {
        resp.delay = Some(Delay {
            fixed_ms: Some(ms),
            ..Default::default()
        });
    }
    resp
}

fn new_stub(
    name: String,
    priority: i32,
    request: RequestPattern,
    response: ResponseDef,
    tag: &str,
    operation: &str,
) -> Stub {
    Stub {
        id: Uuid::now_v7(),
        name: Some(name),
        priority,
        request,
        response,
        tags: vec![tag.to_string()],
        metadata: Some(json!({"operation": operation})),
        enabled: true,
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn microcks_stubs(
    out: &mut ImportResult,
    opts: &ImportOptions,
    name: &str,
    operation: &str,
    base: &RequestPattern,
    meta: &OperationMeta,
    resp_ex: &[(String, RespExample)],
    req_ex: &BTreeMap<String, ReqExample>,
) {
    let ctx = name.to_string();
    let w = &mut out.warnings;
    let mut dispatcher = parse_dispatcher(
        meta.dispatcher.as_deref().unwrap_or(""),
        meta.dispatcher_rules.as_deref().unwrap_or(""),
        &ctx,
        w,
    );
    let mut fallback_name: Option<String> = None;
    if let Dispatcher::Fallback(inner, fb) = dispatcher {
        dispatcher = *inner;
        if !fb.is_empty() {
            fallback_name = Some(fb);
        }
    }
    if let Dispatcher::QueryMatch(_) = dispatcher {
        w.push(format!(
            "{ctx}: QUERY_MATCH applies to XML bodies; examples are dispatched by their request values"
        ));
        dispatcher = Dispatcher::Inferred;
    }
    let mut body_equality = false;
    if dispatcher == Dispatcher::Inferred {
        let path: Vec<String> = req_ex
            .values()
            .flat_map(|e| e.path.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let query: Vec<String> = req_ex
            .values()
            .flat_map(|e| e.query.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        dispatcher = match (path.is_empty(), query.is_empty()) {
            (false, false) => Dispatcher::UriElements(path, query),
            (false, true) => Dispatcher::UriParts(path),
            (true, false) => Dispatcher::UriParams(query),
            (true, true) => {
                body_equality = req_ex.values().any(|e| e.body.is_some());
                Dispatcher::None
            }
        };
    }
    let lookup = |n: &str| resp_ex.iter().find(|(x, _)| x == n).map(|(_, r)| r);
    let default_name = fallback_name
        .clone()
        .filter(|n| {
            let found = lookup(n).is_some();
            if !found {
                w.push(format!(
                    "{ctx}: fallback example `{n}` does not exist; using the first example"
                ));
            }
            found
        })
        .or_else(|| {
            resp_ex
                .iter()
                .find(|(_, r)| (200..300).contains(&r.status))
                .or(resp_ex.first())
                .map(|(n, _)| n.clone())
        });
    let mut stubs = Vec::new();
    let tag = opts.tag.as_str();
    match &dispatcher {
        Dispatcher::JsonBody(rules) => {
            let (cases, dflt) = json_body_cases(rules, &ctx, w);
            for (m, ex_name) in cases {
                let Some(rx) = lookup(&ex_name) else {
                    w.push(format!(
                        "{ctx}: JSON_BODY case refers to unknown example `{ex_name}`"
                    ));
                    continue;
                };
                let mut req = base.clone();
                req.body.push(m);
                let resp = build_response(rx, meta.delay, &format!("{ctx} / {ex_name}"), w);
                stubs.push(new_stub(
                    format!("{name} [{ex_name}]"),
                    4,
                    req,
                    resp,
                    tag,
                    operation,
                ));
            }
            if let Some(d) = dflt
                && lookup(&d).is_some()
                && fallback_name.is_none()
            {
                fallback_name = Some(d);
            }
        }
        Dispatcher::Random => {
            let all: Vec<ResponseDef> = resp_ex
                .iter()
                .map(|(n, rx)| build_response(rx, meta.delay, &format!("{ctx} / {n}"), w))
                .collect();
            if let Some(first) = all.first().cloned() {
                let mut s = new_stub(
                    format!("{name} [random]"),
                    4,
                    base.clone(),
                    first,
                    tag,
                    operation,
                );
                s.response = ResponseDef::default();
                s.responses = all;
                s.response_mode = Some(crate::model::ResponseMode::Random);
                stubs.push(s);
            }
        }
        Dispatcher::None if !body_equality => {}
        _ => {
            for (ex_name, rx) in resp_ex {
                let Some(rq) = req_ex.get(ex_name) else {
                    if !req_ex.is_empty() && Some(ex_name) != default_name.as_ref() {
                        w.push(format!(
                            "{ctx}: response example `{ex_name}` has no request example with the same name; it is not dispatched"
                        ));
                    }
                    continue;
                };
                let mut req = base.clone();
                let mut distinguishing = 0;
                let (path_names, query_names, header_names): (&[String], &[String], &[String]) =
                    match &dispatcher {
                        Dispatcher::UriParts(p) => (p, &[], &[]),
                        Dispatcher::UriParams(q) | Dispatcher::QueryArgs(q) => (&[], q, &[]),
                        Dispatcher::UriElements(p, q) => (p, q, &[]),
                        Dispatcher::QueryHeader(h) => (&[], &[], h),
                        _ => (&[], &[], &[]),
                    };
                for p in path_names {
                    if let Some(v) = rq.path.get(p) {
                        req.path_params.insert(p.clone(), equal(v));
                        distinguishing += 1;
                    }
                }
                for q in query_names {
                    if let Some(v) = rq.query.get(q) {
                        req.query.insert(q.clone(), equal(v));
                        distinguishing += 1;
                    }
                }
                for h in header_names {
                    if let Some(v) = rq.headers.get(&h.to_ascii_lowercase()) {
                        req.headers.insert(h.to_ascii_lowercase(), equal(v));
                        distinguishing += 1;
                    }
                }
                if body_equality && let Some(b) = &rq.body {
                    match json_of(b) {
                        Some(j) => req.body.push(BodyMatcher {
                            equal_to_json: Some(j),
                            ignore_extra_elements: true,
                            ..Default::default()
                        }),
                        None => req.body.push(BodyMatcher {
                            equal_to: Some(crate::matcher::value_text(b)),
                            ..Default::default()
                        }),
                    }
                    distinguishing += 1;
                }
                if distinguishing == 0 {
                    if Some(ex_name) != default_name.as_ref() {
                        w.push(format!(
                            "{ctx}: example `{ex_name}` has no values for the dispatcher; it is not dispatched"
                        ));
                    }
                    continue;
                }
                let resp = build_response(rx, meta.delay, &format!("{ctx} / {ex_name}"), w);
                stubs.push(new_stub(
                    format!("{name} [{ex_name}]"),
                    4,
                    req,
                    resp,
                    tag,
                    operation,
                ));
            }
        }
    }
    let chosen = fallback_name.or(default_name);
    if let Some(n) = chosen
        && let Some(rx) = lookup(&n)
    {
        let resp = build_response(rx, meta.delay, &format!("{ctx} / {n}"), w);
        stubs.push(new_stub(
            name.to_string(),
            5,
            base.clone(),
            resp,
            tag,
            operation,
        ));
    }
    out.stubs.extend(stubs);
}

pub fn import(raw: &Value, opts: &ImportOptions) -> anyhow::Result<ImportResult> {
    import_with(raw, opts, &Overrides::default())
}

pub fn import_with(
    raw: &Value,
    opts: &ImportOptions,
    overrides: &Overrides,
) -> anyhow::Result<ImportResult> {
    let mut r = import_documents(raw, opts, overrides)?;
    let prefix_segments = opts
        .path_prefix
        .split('/')
        .filter(|s| !s.is_empty())
        .count();
    for s in &mut r.stubs {
        crate::microcks::shift_path_segments(s, prefix_segments);
    }
    Ok(r)
}

fn import_documents(
    raw: &Value,
    opts: &ImportOptions,
    overrides: &Overrides,
) -> anyhow::Result<ImportResult> {
    let version = raw.get("openapi").and_then(|v| v.as_str()).unwrap_or("");
    if !version.starts_with('3') {
        anyhow::bail!(
            "only OpenAPI 3.x documents are supported (got `{}`)",
            if version.is_empty() {
                "no openapi field"
            } else {
                version
            }
        );
    }
    let doc = raw;
    let mut out = ImportResult::default();
    let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) else {
        anyhow::bail!("the document has no paths");
    };
    let prefix = opts.path_prefix.trim_end_matches('/');
    if paths.len() > 10_000 {
        anyhow::bail!(
            "the document has {} paths; at most 10000 are imported at once",
            paths.len()
        );
    }
    let mut budget = ResolveBudget::default();
    for (path, item) in paths {
        let item = resolve_budgeted(doc, item, 0, &mut budget)?;
        let path_level: Vec<Value> = item
            .get("parameters")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            let operation = format!("{} {path}", method.to_uppercase());
            let operation_id = op.get("operationId").and_then(|v| v.as_str());
            let name = operation_id
                .map(String::from)
                .unwrap_or_else(|| operation.clone());
            let mut params: Vec<Value> = Vec::new();
            for p in path_level.iter().chain(
                op.get("parameters")
                    .and_then(|p| p.as_array())
                    .into_iter()
                    .flatten(),
            ) {
                let key = (p.get("name").cloned(), p.get("in").cloned());
                params.retain(|q| (q.get("name").cloned(), q.get("in").cloned()) != key);
                params.push(p.clone());
            }
            let mut meta: OperationMeta = op
                .get("x-microcks-operation")
                .map(|x| OperationMeta {
                    delay: x.get("delay").and_then(|d| {
                        d.as_u64()
                            .or_else(|| crate::matcher::value_text(d).trim().parse().ok())
                    }),
                    dispatcher: x
                        .get("dispatcher")
                        .map(crate::matcher::value_text)
                        .filter(|s| !s.is_empty()),
                    dispatcher_rules: x.get("dispatcherRules").map(crate::matcher::value_text),
                })
                .unwrap_or_default();
            if let Some(o) = overrides
                .operations
                .get(&operation)
                .or_else(|| operation_id.and_then(|id| overrides.operations.get(id)))
            {
                if o.delay.is_some() {
                    meta.delay = o.delay;
                }
                if o.dispatcher.is_some() {
                    meta.dispatcher = o.dispatcher.clone();
                    meta.dispatcher_rules = o.dispatcher_rules.clone();
                }
            }
            if meta.delay.is_some_and(|d| d > 600_000) {
                out.warnings
                    .push(format!("{name}: delay is capped at 600000 ms"));
                meta.delay = Some(600_000);
            }
            let mut req = RequestPattern {
                method: Some(method.to_uppercase()),
                url_path_template: Some(format!("{prefix}{path}")),
                ..Default::default()
            };
            let mut validating = false;
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
                let schema = p.get("schema");
                match loc {
                    "path" => {
                        let m = param_matcher(schema);
                        if m.matches.is_some() && opts.validate_requests {
                            req.path_params.insert(pname.to_string(), m);
                            validating = true;
                        }
                    }
                    "query" if required && opts.validate_requests => {
                        req.query.insert(pname.to_string(), param_matcher(schema));
                        validating = true;
                    }
                    "header" if required && opts.validate_requests => {
                        req.headers
                            .insert(pname.to_ascii_lowercase(), StringMatcher::default());
                        validating = true;
                    }
                    _ => {}
                }
            }
            if opts.validate_requests
                && let Some(rb) = op.get("requestBody")
                && let Some((ct, m)) = media(rb.get("content"))
                && (ct.contains("json"))
                && let Some(schema) = m.get("schema")
            {
                let js = to_json_schema(schema);
                if crate::matcher::compile_schema(&js).is_ok() {
                    req.body.push(BodyMatcher {
                        json_schema: Some(js),
                        ..Default::default()
                    });
                    validating = true;
                } else {
                    out.warnings.push(format!(
                        "{name}: request schema is not a valid JSON schema; body is not validated"
                    ));
                }
            }
            let resp_ex = response_examples(op, &name, &mut out.warnings);
            if resp_ex.is_empty() {
                let mut resp = ResponseDef {
                    templated: false,
                    ..Default::default()
                };
                match pick_response(op) {
                    Some((status, r)) => {
                        resp.status = status;
                        if let Some((ct, m)) = media(r.get("content")) {
                            resp.headers.insert("content-type".into(), ct.clone());
                            let ex = media_example(&m);
                            let flavor = if ct.contains("xml") {
                                Flavor::Xml
                            } else {
                                Flavor::Json
                            };
                            if ct.contains("json") && !ex.is_string() {
                                let (b, t) = convert_json(&ex, &name, &mut out.warnings);
                                resp.json_body = Some(b);
                                resp.templated = t;
                            } else if ct.contains("json") {
                                set_body(
                                    &mut resp,
                                    Some(&ct),
                                    &ex,
                                    flavor,
                                    &name,
                                    &mut out.warnings,
                                );
                            } else {
                                let (b, t) = convert_text(
                                    &crate::matcher::value_text(&ex),
                                    flavor,
                                    &name,
                                    &mut out.warnings,
                                );
                                resp.body = Some(b);
                                resp.templated = t;
                            }
                        }
                    }
                    None => out.warnings.push(format!(
                        "{name}: no 2xx or default response; answering 200 with an empty body"
                    )),
                }
                if let Some(ms) = meta.delay {
                    resp.delay = Some(Delay {
                        fixed_ms: Some(ms),
                        ..Default::default()
                    });
                }
                out.stubs.push(new_stub(
                    name.clone(),
                    5,
                    req.clone(),
                    resp,
                    &opts.tag,
                    &operation,
                ));
            } else {
                let req_ex = request_examples(&params, op, &name, &mut out.warnings);
                microcks_stubs(
                    &mut out, opts, &name, &operation, &req, &meta, &resp_ex, &req_ex,
                );
            }
            if validating {
                let loose = RequestPattern {
                    method: req.method.clone(),
                    url_path_template: req.url_path_template.clone(),
                    ..Default::default()
                };
                let mut bad = ResponseDef {
                    status: 400,
                    templated: false,
                    ..Default::default()
                };
                bad.json_body = Some(
                    json!({"error": "request does not match the OpenAPI contract", "operation": name}),
                );
                out.stubs.push(new_stub(
                    format!("{name} (contract violation)"),
                    6,
                    loose,
                    bad,
                    &opts.tag,
                    &operation,
                ));
            }
        }
    }
    if out.stubs.is_empty() {
        out.warnings.push("no operations found".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diamond_refs_are_rejected_not_expanded() {
        let mut schemas = Map::new();
        for i in 0..30 {
            schemas.insert(
                format!("S{i}"),
                json!({"type": "object", "properties": {
                "a": {"$ref": format!("#/components/schemas/S{}", i + 1)},
                "b": {"$ref": format!("#/components/schemas/S{}", i + 1)}}}),
            );
        }
        schemas.insert("S30".into(), json!({"type": "string"}));
        let doc = json!({"openapi": "3.0.3", "info": {"title": "x", "version": "1"},
            "components": {"schemas": schemas},
            "paths": {"/x": {"get": {"responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/S0"}}}}}}}}});
        let err = import(
            &doc,
            &ImportOptions {
                tag: "t".into(),
                validate_requests: false,
                path_prefix: String::new(),
            },
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("$ref expansion"), "{err}");
    }

    #[test]
    fn ref_expansion_is_limited_by_bytes() {
        let refs: Vec<Value> = (0..1100)
            .map(|_| json!({"$ref": "#/components/x"}))
            .collect();
        let doc = json!({"openapi": "3.0.3", "info": {"title": "x", "version": "1"},
            "components": {"x": "a".repeat(64 * 1024)},
            "paths": {"/x": {"x-r": refs, "get": {"responses": {"200": {"description": "ok"}}}}}});
        let started = std::time::Instant::now();
        let err = import(
            &doc,
            &ImportOptions {
                tag: "t".into(),
                validate_requests: false,
                path_prefix: String::new(),
            },
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("64 MiB"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let mut budget = ResolveBudget::new(10_000);
        let one = json!({"$ref": "#/components/x"});
        assert!(resolve_budgeted(&doc, &one, 0, &mut budget).is_err());
        let mut budget = ResolveBudget::new(200_000);
        assert!(resolve_budgeted(&doc, &one, 0, &mut budget).is_ok());
        assert!(resolve_budgeted(&doc, &one, 0, &mut budget).is_ok());
        assert!(resolve_budgeted(&doc, &one, 0, &mut budget).is_ok());
        assert!(resolve_budgeted(&doc, &one, 0, &mut budget).is_err());
    }

    #[test]
    fn literal_paths_win_over_templates_and_mixed_segments_import() {
        let doc = json!({"openapi": "3.0.3", "info": {"title": "x", "version": "1"}, "paths": {
            "/users/me": {"get": {"responses": {"200": {"description": "me", "content": {"application/json": {"example": {"who": "me"}}}}}}},
            "/users/{id}": {"get": {"parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
                "responses": {"200": {"description": "id", "content": {"application/json": {"example": {"who": "id"}}}}}}},
            "/report.{format}": {"get": {"parameters": [{"name": "format", "in": "path", "required": true, "schema": {"type": "string"}}],
                "responses": {"200": {"description": "r"}}}}}});
        let r = import(
            &doc,
            &ImportOptions {
                tag: "t".into(),
                validate_requests: false,
                path_prefix: String::new(),
            },
        )
        .unwrap();
        let mut compiled: Vec<CompiledStub> = r
            .stubs
            .iter()
            .map(|s| CompiledStub::compile(s).unwrap())
            .collect();
        compiled.reverse();
        compiled.sort_by_key(|c| (c.stub.priority, c.specificity()));
        let req = CapturedRequest::new("GET", "/users/me", None, vec![], bytes::Bytes::new());
        let hit = compiled
            .iter()
            .find(|c| c.match_request(&req, None).is_some())
            .unwrap();
        assert_eq!(hit.stub.response.json_body.as_ref().unwrap()["who"], "me");
        let req = CapturedRequest::new("GET", "/report.csv", None, vec![], bytes::Bytes::new());
        assert!(
            compiled
                .iter()
                .any(|c| c.match_request(&req, None).is_some())
        );
    }
    use crate::matcher::CompiledStub;
    use crate::request::CapturedRequest;
    use bytes::Bytes;

    const SPEC: &str = r##"
openapi: 3.0.3
info: {title: Pets, version: "1"}
paths:
  /pets/{petId}:
    parameters:
      - {name: petId, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getPet
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Pet"}
        "404": {description: missing}
  /pets:
    post:
      operationId: createPet
      parameters:
        - {name: tenant, in: query, required: true, schema: {type: string, enum: [a, b]}}
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/NewPet"}
      responses:
        "201":
          description: created
          content:
            application/json:
              example: {id: 7, name: rex}
components:
  schemas:
    Pet:
      type: object
      properties:
        id: {type: integer, minimum: 1}
        name: {type: string}
        tag: {type: string, nullable: true}
        born: {type: string, format: date-time}
        owner: {$ref: "#/components/schemas/Pet"}
    NewPet:
      type: object
      required: [name]
      properties:
        name: {type: string, minLength: 1}
        tag: {type: string, nullable: true}
"##;

    fn opts() -> ImportOptions {
        ImportOptions {
            tag: "pets".into(),
            validate_requests: true,
            path_prefix: "/api".into(),
        }
    }

    #[test]
    fn imports_operations_with_examples_and_validation() {
        let doc = parse_document(SPEC).unwrap();
        let r = import(&doc, &opts()).unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let get = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("getPet"))
            .unwrap();
        assert_eq!(
            get.request.url_path_template.as_deref(),
            Some("/api/pets/{petId}")
        );
        let body = get.response.json_body.as_ref().unwrap();
        assert_eq!(body["id"], 1);
        assert_eq!(body["born"], "2026-01-01T00:00:00Z");
        assert!(
            body["owner"].is_null(),
            "recursive refs are cut instead of looping"
        );
        let create = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("createPet"))
            .unwrap();
        assert_eq!(create.response.status, 201);
        assert_eq!(
            create.response.json_body,
            Some(json!({"id": 7, "name": "rex"}))
        );
        let cs = CompiledStub::compile(create).unwrap();
        let ok = CapturedRequest::new(
            "POST",
            "/api/pets",
            Some("tenant=a"),
            vec![],
            Bytes::from(r#"{"name":"rex","tag":null}"#),
        );
        assert!(cs.match_request(&ok, ok.json()).is_some());
        let bad = CapturedRequest::new(
            "POST",
            "/api/pets",
            Some("tenant=a"),
            vec![],
            Bytes::from(r#"{"name":""}"#),
        );
        assert!(cs.match_request(&bad, bad.json()).is_none());
        let wrong_enum = CapturedRequest::new(
            "POST",
            "/api/pets",
            Some("tenant=z"),
            vec![],
            Bytes::from(r#"{"name":"x"}"#),
        );
        assert!(cs.match_request(&wrong_enum, wrong_enum.json()).is_none());
        let fallback = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("createPet (contract violation)"))
            .unwrap();
        assert_eq!(fallback.response.status, 400);
        assert_eq!(fallback.priority, 6);
        let g = CompiledStub::compile(get).unwrap();
        let req = CapturedRequest::new("GET", "/api/pets/abc", None, vec![], Bytes::new());
        assert!(g.match_request(&req, None).is_none(), "integer path param");
    }

    #[test]
    fn rejects_swagger2_and_handles_nullable() {
        assert!(import(&json!({"swagger": "2.0", "paths": {}}), &opts()).is_err());
        let s = to_json_schema(&json!({"type": "string", "nullable": true, "enum": ["a"]}));
        assert_eq!(s, json!({"type": ["string", "null"], "enum": ["a", null]}));
    }

    fn pastry(validate: bool, overrides: &Overrides) -> Vec<CompiledStub> {
        let doc = parse_document(include_str!("../tests/fixtures/pastry-openapi.yaml")).unwrap();
        let r = import_with(
            &doc,
            &ImportOptions {
                tag: "pastry".into(),
                validate_requests: validate,
                path_prefix: String::new(),
            },
            overrides,
        )
        .unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let mut compiled: Vec<CompiledStub> = r
            .stubs
            .iter()
            .map(|s| CompiledStub::compile(s).unwrap())
            .collect();
        compiled.sort_by_key(|c| (c.stub.priority, c.specificity()));
        compiled
    }

    fn hit<'a>(
        c: &'a [CompiledStub],
        method: &str,
        path: &str,
        query: Option<&str>,
        body: &str,
    ) -> &'a Stub {
        let req = CapturedRequest::new(method, path, query, vec![], Bytes::from(body.to_string()));
        &c.iter()
            .find(|s| s.match_request(&req, req.json()).is_some())
            .unwrap_or_else(|| panic!("{method} {path} {query:?} matched nothing"))
            .stub
    }

    #[test]
    fn microcks_named_examples_dispatch() {
        for validate in [false, true] {
            let c = pastry(validate, &Overrides::default());
            let s = hit(&c, "GET", "/pastry/Millefeuille", None, "");
            assert_eq!(s.response.json_body.as_ref().unwrap()["size"], "L");
            assert_eq!(s.response.delay.as_ref().unwrap().fixed_ms, Some(15));
            assert_eq!(
                s.metadata.as_ref().unwrap()["operation"],
                "GET /pastry/{name}"
            );
            let s = hit(&c, "GET", "/pastry/Eclair%20Chocolat", None, "");
            assert_eq!(s.response.status, 200, "{s:?}");
            assert!(s.response.templated);
            assert_eq!(
                s.response.json_body.as_ref().unwrap()["bakedAt"],
                "{{now format=\"%Y-%m-%d\"}}"
            );
            let s = hit(&c, "GET", "/pastry/Croissant", None, "");
            assert_eq!(
                s.response.status, 404,
                "FALLBACK returns the Unknown example"
            );
            assert_eq!(
                s.response.json_body.as_ref().unwrap()["error"],
                "pastry {{request.pathSegments.[1]}} not found"
            );
            let s = hit(&c, "GET", "/pastry", Some("size=M"), "");
            assert_eq!(
                s.response.json_body.as_ref().unwrap()[0]["name"],
                "Eclair Cafe"
            );
            let s = hit(&c, "GET", "/pastry", Some("size=XL"), "");
            assert_eq!(
                s.response.json_body.as_ref().unwrap()[0]["name"],
                "Tartelette Fraise",
                "default example"
            );
            let s = hit(
                &c,
                "POST",
                "/pastry",
                None,
                r#"{"name": "Big", "size": "L"}"#,
            );
            assert_eq!(s.response.status, 201);
            assert!(s.response.json_body.as_ref().unwrap().get("note").is_some());
            let s = hit(
                &c,
                "POST",
                "/pastry",
                None,
                r#"{"name": "Small", "size": "S"}"#,
            );
            assert!(s.response.json_body.as_ref().unwrap().get("note").is_none());
            let s = hit(&c, "DELETE", "/pastry/Eclair%20Chocolat", None, "");
            assert_eq!(s.response.status, 204);
            assert!(s.response.json_body.is_none() && s.response.body.is_none());
        }
        let overrides = Overrides {
            operations: BTreeMap::from([(
                "GET /pastry/{name}".to_string(),
                OperationMeta {
                    delay: Some(40),
                    dispatcher: Some("URI_PARTS".into()),
                    dispatcher_rules: Some("name".into()),
                },
            )]),
        };
        let c = pastry(false, &overrides);
        let s = hit(&c, "GET", "/pastry/Croissant", None, "");
        assert_eq!(
            s.response.status, 200,
            "without FALLBACK the default example answers"
        );
        assert_eq!(s.response.delay.as_ref().unwrap().fixed_ms, Some(40));
        let info = service_info(
            &parse_document(include_str!("../tests/fixtures/pastry-openapi.yaml")).unwrap(),
        );
        assert_eq!(info.name, "API Pastry - 2.0");
        assert_eq!(info.labels["domain"], "pastry");
    }

    #[test]
    fn microcks_style_prefix_keeps_client_urls() {
        let doc = parse_document(include_str!("../tests/fixtures/pastry-openapi.yaml")).unwrap();
        let r = import(
            &doc,
            &ImportOptions {
                tag: "pastry".into(),
                validate_requests: false,
                path_prefix: "/rest/API+Pastry+-+2.0/2.0.0".into(),
            },
        )
        .unwrap();
        let mut c: Vec<CompiledStub> = r
            .stubs
            .iter()
            .map(|s| CompiledStub::compile(s).unwrap())
            .collect();
        c.sort_by_key(|x| (x.stub.priority, x.specificity()));
        let s = hit(
            &c,
            "GET",
            "/rest/API+Pastry+-+2.0/2.0.0/pastry/Millefeuille",
            None,
            "",
        );
        assert_eq!(s.response.json_body.as_ref().unwrap()["size"], "L");
    }

    #[test]
    fn random_dispatcher_uses_response_list() {
        let doc = json!({"openapi": "3.0.3", "info": {"title": "r", "version": "1"}, "paths": {"/coin": {"get": {
            "x-microcks-operation": {"dispatcher": "RANDOM"},
            "responses": {"200": {"description": "ok", "content": {"application/json": {"examples": {
                "heads": {"value": {"side": "heads"}}, "tails": {"value": {"side": "tails"}}}}}}}}}}});
        let r = import(&doc, &opts()).unwrap();
        let s = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("GET /coin [random]"))
            .unwrap();
        assert_eq!(s.responses.len(), 2);
        assert_eq!(s.response_mode, Some(crate::model::ResponseMode::Random));
        assert_eq!(s.response, ResponseDef::default());
    }
}
