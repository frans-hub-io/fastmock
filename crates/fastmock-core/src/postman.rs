use std::collections::BTreeMap;

use serde_json::Value;
use uuid::Uuid;

use crate::model::{BodyMatcher, RequestPattern, ResponseDef, StringMatcher, Stub};

pub const MAX_ITEMS: usize = 10_000;
pub const MAX_STUBS: usize = 10_000;
pub const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DEPTH: usize = 32;
const MAX_RESOLVED: usize = 1024 * 1024;
const SKIPPED_RESPONSE_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "set-cookie",
    "date",
];

#[derive(Debug, Clone, Default)]
pub struct PostmanOptions {
    pub tag: String,
}

#[derive(Debug, Default)]
pub struct Converted {
    pub stubs: Vec<Stub>,
    pub warnings: Vec<String>,
}

pub fn is_collection(doc: &Value) -> bool {
    let info = &doc["info"];
    doc.get("item").is_some_and(Value::is_array)
        && (info["schema"]
            .as_str()
            .is_some_and(|s| s.contains("getpostman.com"))
            || info.get("_postman_id").is_some())
}

struct Ctx<'a> {
    vars: BTreeMap<String, String>,
    opts: &'a PostmanOptions,
    out: Converted,
    items: usize,
    seen: std::collections::HashMap<String, usize>,
    bytes: usize,
}

struct CountingSink(usize);

impl std::io::Write for CountingSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0 += data.len();
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn stub_size(stub: &Stub) -> usize {
    let mut w = CountingSink(0);
    let _ = serde_json::to_writer(&mut w, stub);
    w.0
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn variables(v: &Value) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for x in v.as_array().into_iter().flatten() {
        if x["disabled"].as_bool() == Some(true) {
            continue;
        }
        if let Some(k) = x["key"].as_str().or_else(|| x["id"].as_str()) {
            m.insert(k.to_string(), scalar(&x["value"]));
        }
    }
    m
}

fn resolve(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match vars.get(name) {
                    Some(v) if out.len() + v.len() <= MAX_RESOLVED => out.push_str(v),
                    _ => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn sanitize_tag(name: &str) -> Option<String> {
    let t: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let t = t.trim_matches('-');
    if t.is_empty() {
        return None;
    }
    let mut t = t.to_string();
    t.truncate(128);
    Some(t)
}

pub fn path_template(raw: &str) -> (String, bool) {
    let mut templated = false;
    let segs: Vec<String> = raw
        .split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix(':').filter(|n| !n.is_empty()) {
                templated = true;
                return format!("{{{name}}}");
            }
            if seg.starts_with("{{") && seg.ends_with("}}") && seg.len() > 4 {
                templated = true;
                return format!("{{{}}}", seg[2..seg.len() - 2].trim());
            }
            seg.to_string()
        })
        .collect();
    let mut p = segs.join("/");
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    (p, templated)
}

fn strip_origin(url: &str) -> &str {
    let url = url.split('#').next().unwrap_or(url);
    if let Some(i) = url.find("://") {
        let after = &url[i + 3..];
        return match after.find(['/', '?']) {
            Some(j) => &after[j..],
            None => "",
        };
    }
    if url.starts_with("{{")
        && let Some(end) = url.find("}}")
    {
        return &url[end + 2..];
    }
    url
}

struct ParsedUrl {
    path: String,
    templated: bool,
    query: Vec<(String, String)>,
}

fn parse_url(url: &Value, vars: &BTreeMap<String, String>) -> ParsedUrl {
    let mut query = Vec::new();
    let path_raw = match url {
        Value::String(s) => {
            let resolved = resolve(s, vars);
            let rel = strip_origin(&resolved).to_string();
            let (p, q) = rel.split_once('?').unwrap_or((&rel, ""));
            for pair in q.split('&').filter(|x| !x.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                query.push((k.to_string(), v.to_string()));
            }
            p.to_string()
        }
        Value::Object(o) => {
            for q in o
                .get("query")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if q["disabled"].as_bool() == Some(true) {
                    continue;
                }
                if let Some(k) = q["key"].as_str() {
                    query.push((resolve(k, vars), resolve(&scalar(&q["value"]), vars)));
                }
            }
            match o.get("path") {
                Some(Value::Array(segs)) => segs
                    .iter()
                    .map(|s| match s {
                        Value::Object(m) => scalar(m.get("value").unwrap_or(&Value::Null)),
                        other => scalar(other),
                    })
                    .map(|s| resolve(&s, vars))
                    .collect::<Vec<_>>()
                    .join("/"),
                Some(Value::String(s)) => resolve(s, vars),
                _ => {
                    let raw = resolve(o.get("raw").and_then(Value::as_str).unwrap_or(""), vars);
                    let rel = strip_origin(&raw).to_string();
                    rel.split('?').next().unwrap_or("").to_string()
                }
            }
        }
        _ => String::new(),
    };
    let (path, templated) = path_template(path_raw.trim_start_matches('/'));
    ParsedUrl {
        path,
        templated,
        query,
    }
}

fn header_list(v: &Value, vars: &BTreeMap<String, String>) -> Vec<(String, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["disabled"].as_bool() != Some(true))
        .filter_map(|h| {
            Some((
                h["key"].as_str()?.to_string(),
                resolve(&scalar(&h["value"]), vars),
            ))
        })
        .collect()
}

fn raw_body(req: &Value, vars: &BTreeMap<String, String>) -> Option<String> {
    let b = req.get("body")?;
    if b["mode"].as_str() != Some("raw") {
        return None;
    }
    let raw = b["raw"].as_str()?;
    (!raw.trim().is_empty()).then(|| resolve(raw, vars))
}

fn build(
    ctx: &mut Ctx<'_>,
    item: &Value,
    example: Option<&Value>,
    tags: &[String],
    with_body: bool,
) {
    let item_name = item["name"].as_str().unwrap_or("request");
    let req = example
        .and_then(|e| e.get("originalRequest"))
        .filter(|r| r.is_object())
        .unwrap_or(&item["request"]);
    let req = match req {
        Value::String(url) => &serde_json::json!({"method": "GET", "url": url}),
        other => other,
    };
    let method = req["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
    let url = parse_url(req.get("url").unwrap_or(&Value::Null), &ctx.vars);
    let mut pattern = RequestPattern {
        method: Some(method.clone()),
        ..Default::default()
    };
    if url.templated {
        pattern.url_path_template = Some(url.path.clone());
    } else {
        pattern.url_path = Some(url.path.clone());
    }
    for (k, v) in &url.query {
        if v.contains("{{") {
            continue;
        }
        pattern.query.insert(
            k.clone(),
            StringMatcher {
                equal_to: Some(v.clone()),
                ..Default::default()
            },
        );
    }
    if with_body && let Some(body) = raw_body(req, &ctx.vars) {
        match serde_json::from_str::<Value>(&body) {
            Ok(v) if !body.contains("{{") => pattern.body.push(BodyMatcher {
                equal_to_json: Some(v),
                ignore_extra_elements: true,
                ..Default::default()
            }),
            Err(_) if !body.contains("{{") => pattern.body.push(BodyMatcher {
                equal_to: Some(body),
                ..Default::default()
            }),
            _ => {}
        }
    }
    let mut response = ResponseDef {
        status: 200,
        templated: false,
        ..Default::default()
    };
    let example_name = match example {
        Some(e) => {
            response.status = e["code"]
                .as_u64()
                .filter(|c| (100..=599).contains(c))
                .unwrap_or(200) as u16;
            let mut is_json = false;
            for (k, v) in header_list(&e["header"], &ctx.vars) {
                let lk = k.to_ascii_lowercase();
                if SKIPPED_RESPONSE_HEADERS.contains(&lk.as_str()) {
                    continue;
                }
                if lk == "content-type" && v.contains("json") {
                    is_json = true;
                }
                response.headers.insert(k, v);
            }
            if let Some(body) = e["body"].as_str().filter(|b| !b.is_empty()) {
                let body = resolve(body, &ctx.vars);
                let lang_json = e["_postman_previewlanguage"].as_str() == Some("json");
                match serde_json::from_str::<Value>(&body) {
                    Ok(v) if is_json || lang_json => {
                        if !response
                            .headers
                            .keys()
                            .any(|k| k.eq_ignore_ascii_case("content-type"))
                        {
                            response
                                .headers
                                .insert("Content-Type".into(), "application/json".into());
                        }
                        response.json_body = Some(v)
                    }
                    _ => response.body = Some(body),
                }
            }
            e["name"].as_str().unwrap_or("example").to_string()
        }
        None => "request".to_string(),
    };
    let key = serde_json::to_string(&pattern).unwrap_or_default();
    let mut priority = crate::model::DEFAULT_PRIORITY;
    let shadowed = ctx.seen.get(&key).copied().unwrap_or(0);
    if shadowed > 0 {
        priority += shadowed as i32;
        ctx.out.warnings.push(format!(
            "{item_name} / {example_name}: same request as an earlier example; it only answers when that one is removed (priority {priority})"
        ));
    }
    *ctx.seen.entry(key).or_insert(0) += 1;
    let mut stub_tags = vec![ctx.opts.tag.clone()];
    for t in tags {
        if !stub_tags.contains(t) {
            stub_tags.push(t.clone());
        }
    }
    let mut name = format!("{item_name} / {example_name}");
    if name.len() > 256 {
        let mut cut = 256;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
    }
    ctx.out.stubs.push(Stub {
        id: Uuid::new_v4(),
        name: Some(name),
        priority,
        request: pattern,
        response,
        tags: stub_tags,
        enabled: true,
        ..Default::default()
    });
}

fn walk(ctx: &mut Ctx<'_>, items: &Value, tags: &[String], depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("folders are nested deeper than {MAX_DEPTH} levels"));
    }
    for item in items.as_array().into_iter().flatten() {
        ctx.items += 1;
        if ctx.items > MAX_ITEMS {
            return Err(format!("collection has more than {MAX_ITEMS} items"));
        }
        let name = item["name"].as_str().unwrap_or("item");
        if let Some(children) = item.get("item") {
            let saved = ctx.vars.clone();
            ctx.vars.extend(variables(&item["variable"]));
            let mut t = tags.to_vec();
            if let Some(tag) = sanitize_tag(name) {
                t.push(tag);
            }
            walk(ctx, children, &t, depth + 1)?;
            ctx.vars = saved;
            continue;
        }
        if item.get("request").is_none() {
            ctx.out
                .warnings
                .push(format!("{name}: neither a folder nor a request; skipped"));
            continue;
        }
        let examples: Vec<&Value> = item["response"].as_array().into_iter().flatten().collect();
        if examples.is_empty() {
            ctx.out.warnings.push(format!(
                "{name}: no saved example response; skipped (save an example in Postman to mock it)"
            ));
            continue;
        }
        if ctx.out.stubs.len() + examples.len() > MAX_STUBS {
            return Err(format!(
                "collection produces more than {MAX_STUBS} stubs (one per saved example)"
            ));
        }
        let with_body = examples.len() > 1;
        for e in examples {
            build(ctx, item, Some(e), tags, with_body);
            ctx.bytes += ctx.out.stubs.last().map(stub_size).unwrap_or(0);
            if ctx.bytes > MAX_TOTAL_BYTES {
                return Err(format!(
                    "collection expands to more than {} MiB of stubs",
                    MAX_TOTAL_BYTES >> 20
                ));
            }
        }
    }
    Ok(())
}

pub fn convert(doc: &Value, opts: &PostmanOptions) -> Result<Converted, String> {
    if doc.get("requests").is_some() && doc.get("item").is_none() {
        return Err(
            "Postman v1 collections are not supported; export the collection as v2.1".into(),
        );
    }
    if !doc.get("item").is_some_and(Value::is_array) {
        return Err("not a Postman collection: `item` array missing".into());
    }
    if let Some(schema) = doc["info"]["schema"].as_str()
        && !(schema.contains("v2.1") || schema.contains("v2.0"))
    {
        return Err(format!(
            "unsupported Postman schema {schema}; export as v2.1"
        ));
    }
    let mut ctx = Ctx {
        vars: variables(&doc["variable"]),
        opts,
        out: Converted::default(),
        items: 0,
        seen: Default::default(),
        bytes: 0,
    };
    walk(&mut ctx, &doc["item"], &[], 0)?;
    Ok(ctx.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collection() -> Value {
        json!({
            "info": {"name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "variable": [{"key": "baseUrl", "value": "https://api.example.com"}, {"key": "ver", "value": "v2"}],
            "item": [
                {"name": "Users & Auth", "item": [
                    {"name": "Get user", "request": {"method": "GET", "url": {"raw": "{{baseUrl}}/{{ver}}/users/:id?expand=orders", "host": ["{{baseUrl}}"], "path": ["{{ver}}", "users", ":id"], "query": [{"key": "expand", "value": "orders"}, {"key": "debug", "value": "1", "disabled": true}]}},
                     "response": [{"name": "found", "code": 200, "header": [{"key": "Content-Type", "value": "application/json"}, {"key": "Content-Length", "value": "10"}], "body": "{\"id\": 7, \"ver\": \"{{ver}}\"}"}]}
                ]},
                {"name": "Create order", "request": {"method": "POST", "url": "{{baseUrl}}/orders", "body": {"mode": "raw", "raw": "{\"sku\": \"A\"}"}},
                 "response": [
                    {"name": "created", "originalRequest": {"method": "POST", "url": "{{baseUrl}}/orders", "body": {"mode": "raw", "raw": "{\"sku\": \"A\"}"}}, "code": 201, "header": [], "body": "ok"},
                    {"name": "rejected", "originalRequest": {"method": "POST", "url": "{{baseUrl}}/orders", "body": {"mode": "raw", "raw": "{\"sku\": \"X\"}"}}, "code": 422, "body": "{{unknown}}"}
                 ]},
                {"name": "No examples", "request": {"method": "GET", "url": "{{baseUrl}}/nothing"}}
            ]
        })
    }

    #[test]
    fn converts_examples_variables_and_folders() {
        let doc = collection();
        assert!(is_collection(&doc));
        let c = convert(&doc, &PostmanOptions { tag: "shop".into() }).unwrap();
        assert_eq!(c.stubs.len(), 3, "{:?}", c.warnings);
        let get = &c.stubs[0];
        assert_eq!(get.request.method.as_deref(), Some("GET"));
        assert_eq!(
            get.request.url_path_template.as_deref(),
            Some("/v2/users/{id}")
        );
        assert_eq!(
            get.request.query["expand"].equal_to.as_deref(),
            Some("orders")
        );
        assert!(!get.request.query.contains_key("debug"));
        assert_eq!(get.response.json_body, Some(json!({"id": 7, "ver": "v2"})));
        assert!(!get.response.templated);
        assert!(!get.response.headers.contains_key("Content-Length"));
        assert_eq!(
            get.tags,
            vec!["shop".to_string(), "Users---Auth".to_string()]
        );
        let created = &c.stubs[1];
        assert_eq!(created.request.url_path.as_deref(), Some("/orders"));
        assert_eq!(created.response.status, 201);
        assert_eq!(
            created.request.body[0].equal_to_json,
            Some(json!({"sku": "A"}))
        );
        let rejected = &c.stubs[2];
        assert_eq!(rejected.response.status, 422);
        assert_eq!(rejected.response.body.as_deref(), Some("{{unknown}}"));
        assert!(c.warnings.iter().any(|w| w.contains("No examples")));
    }

    #[test]
    fn rejects_other_documents_and_limits() {
        assert!(convert(&json!({"requests": []}), &PostmanOptions::default()).is_err());
        assert!(convert(&json!({"openapi": "3.0.0"}), &PostmanOptions::default()).is_err());
        let mut deep = json!({"name": "leaf", "request": {"url": "/x"}});
        for _ in 0..40 {
            deep = json!({"name": "f", "item": [deep]});
        }
        assert!(convert(&json!({"item": [deep]}), &PostmanOptions::default()).is_err());
    }

    #[test]
    fn duplicate_examples_are_shadowed_with_warning() {
        let doc = json!({"item": [
            {"name": "a", "request": {"method": "GET", "url": "/x"}, "response": [{"name": "one", "code": 200, "body": "1"}]},
            {"name": "b", "request": {"method": "GET", "url": "/x"}, "response": [{"name": "two", "code": 200, "body": "2"}]}
        ]});
        let c = convert(&doc, &PostmanOptions { tag: "t".into() }).unwrap();
        assert_eq!(c.stubs[0].priority, 5);
        assert_eq!(c.stubs[1].priority, 6);
        assert_eq!(c.warnings.len(), 1);
    }

    #[test]
    fn resolves_only_known_variables() {
        let vars = BTreeMap::from([("a".to_string(), "1".to_string())]);
        assert_eq!(resolve("x{{a}}y{{b}}z{{", &vars), "x1y{{b}}z{{");
        assert_eq!(
            path_template("users/:id/{{other}}").0,
            "/users/{id}/{other}"
        );
    }

    #[test]
    fn examples_count_toward_stub_and_byte_limits() {
        let examples: Vec<Value> = (0..1_200)
            .map(|i| json!({"name": format!("e{i}"), "code": 200, "body": "{{v}}"}))
            .collect();
        let doc = json!({
            "info": {"name": "x", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "variable": [{"key": "v", "value": "a".repeat(64 * 1024)}],
            "item": [{"name": "r", "request": {"method": "GET", "url": "https://api.example.com/r"}, "response": examples}]
        });
        let started = std::time::Instant::now();
        let err = convert(&doc, &PostmanOptions { tag: "t".into() }).unwrap_err();
        assert!(err.contains("MiB"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let many: Vec<Value> = (0..MAX_STUBS + 1)
            .map(|i| json!({"name": format!("e{i}"), "code": 200, "body": "x"}))
            .collect();
        let doc = json!({
            "info": {"name": "x", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{"name": "r", "request": {"method": "GET", "url": "https://api.example.com/r"}, "response": many}]
        });
        let err = convert(&doc, &PostmanOptions { tag: "t".into() }).unwrap_err();
        assert!(err.contains("stubs"), "{err}");
    }
}
