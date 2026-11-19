use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::matcher::value_text;
use crate::model::{
    BodyMatcher, Delay, Fault, ProxyDef, RequestPattern, ResponseDef, StringMatcher, Stub,
};
pub use crate::wiremock::Converted;

pub const MAX_EXPECTATIONS: usize = 10_000;
const REGEX_META: &[char] = &[
    '*', '+', '?', '[', ']', '(', ')', '{', '}', '^', '$', '|', '\\',
];

fn literal_or_regex(s: &str) -> StringMatcher {
    if s.contains(REGEX_META) && regex::Regex::new(s).is_ok() {
        StringMatcher {
            or: vec![
                StringMatcher::equal(s),
                StringMatcher {
                    matches: Some(s.to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    } else {
        StringMatcher::equal(s)
    }
}

fn negated(m: StringMatcher, not: bool) -> StringMatcher {
    if not {
        StringMatcher {
            not: Some(Box::new(m)),
            ..Default::default()
        }
    } else {
        m
    }
}

fn schema_matcher(schema: &Value, ctx: &str, w: &mut Vec<String>) -> StringMatcher {
    if let Some(p) = schema.get("pattern").and_then(Value::as_str) {
        return StringMatcher {
            matches: Some(p.to_string()),
            ..Default::default()
        };
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        return StringMatcher {
            or: values
                .iter()
                .map(|v| StringMatcher::equal(value_text(v)))
                .collect(),
            ..Default::default()
        };
    }
    let re = match (
        schema.get("type").and_then(Value::as_str),
        schema.get("format").and_then(Value::as_str),
    ) {
        (_, Some("uuid")) => {
            Some("[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
        }
        (_, Some("date")) => Some(r"\d{4}-\d{2}-\d{2}"),
        (Some("integer"), _) => Some(r"-?\d+"),
        (Some("number"), _) => Some(r"-?\d+(\.\d+)?([eE][+-]?\d+)?"),
        (Some("boolean"), _) => Some("true|false"),
        _ => None,
    };
    match re {
        Some(r) => StringMatcher {
            matches: Some(r.to_string()),
            ..Default::default()
        },
        None => {
            w.push(format!(
                "{ctx}: schema {schema} is reduced to a presence check"
            ));
            StringMatcher::default()
        }
    }
}

fn value_matcher(v: &Value, ctx: &str, w: &mut Vec<String>) -> StringMatcher {
    match v {
        Value::String(s) => match s.strip_prefix('!') {
            Some(rest) => negated(literal_or_regex(rest), true),
            None => literal_or_regex(s),
        },
        Value::Object(o) => {
            let not = o.get("not").and_then(Value::as_bool).unwrap_or(false);
            let inner = match (o.get("schema"), o.get("value")) {
                (Some(s), _) => schema_matcher(s, ctx, w),
                (None, Some(x)) => literal_or_regex(&value_text(x)),
                _ => StringMatcher::default(),
            };
            negated(inner, not)
        }
        other => StringMatcher::equal(value_text(other)),
    }
}

fn entries(v: Option<&Value>) -> Vec<(String, Vec<Value>)> {
    let one = |x: &Value| match x {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    match v {
        Some(Value::Object(o)) => o
            .iter()
            .filter(|(k, _)| k.as_str() != "keyMatchStyle")
            .map(|(k, x)| (k.clone(), one(x)))
            .collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|e| {
                let name = e.get("name").map(value_text)?;
                let values = e
                    .get("values")
                    .or_else(|| e.get("value"))
                    .map(one)
                    .unwrap_or_default();
                Some((name, values))
            })
            .collect(),
        _ => vec![],
    }
}

fn multi_matchers(
    v: Option<&Value>,
    lower: bool,
    ctx: &str,
    w: &mut Vec<String>,
) -> BTreeMap<String, StringMatcher> {
    let mut out = BTreeMap::new();
    let matching_key = v
        .and_then(|x| x.get("keyMatchStyle"))
        .and_then(Value::as_str)
        == Some("MATCHING_KEY");
    for (raw_name, values) in entries(v) {
        let (name, optional) = match raw_name.strip_prefix('?') {
            Some(n) => (n.to_string(), true),
            None => (raw_name.clone(), false),
        };
        let (name, not_key) = match name.strip_prefix('!') {
            Some(n) => (n.to_string(), true),
            None => (name, false),
        };
        let name = if lower {
            name.to_ascii_lowercase()
        } else {
            name
        };
        let mut matchers: Vec<StringMatcher> =
            values.iter().map(|x| value_matcher(x, ctx, w)).collect();
        let m = if not_key {
            StringMatcher {
                absent: Some(true),
                ..Default::default()
            }
        } else if matching_key && !matchers.is_empty() {
            if optional {
                w.push(format!(
                    "{ctx}: optional `{name}` with keyMatchStyle MATCHING_KEY is matched as required"
                ));
            }
            StringMatcher {
                all_values: Some(matchers),
                ..Default::default()
            }
        } else if matchers.len() > 1 {
            if optional {
                w.push(format!(
                    "{ctx}: optional `{name}` with several values is matched as required"
                ));
            }
            StringMatcher {
                includes: Some(matchers),
                ..Default::default()
            }
        } else {
            let m = matchers.pop().unwrap_or_default();
            if optional {
                StringMatcher {
                    or: vec![
                        StringMatcher {
                            absent: Some(true),
                            ..Default::default()
                        },
                        m,
                    ],
                    ..Default::default()
                }
            } else {
                m
            }
        };
        out.insert(name, m);
    }
    out
}

fn has_regex_meta(path: &str) -> bool {
    path.chars().any(|c| {
        matches!(
            c,
            '*' | '+' | '?' | '[' | ']' | '(' | ')' | '^' | '$' | '|' | '\\'
        )
    })
}

fn json_value(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
        other => other.clone(),
    }
}

fn body_matchers(
    b: &Value,
    req: &mut RequestPattern,
    ctx: &str,
    w: &mut Vec<String>,
) -> Result<(), String> {
    let o = match b {
        Value::String(s) => {
            req.body.push(BodyMatcher {
                equal_to: Some(s.clone()),
                ..Default::default()
            });
            return Ok(());
        }
        Value::Object(o) => o,
        Value::Null => return Ok(()),
        other => {
            req.body.push(BodyMatcher {
                equal_to_json: Some(other.clone()),
                ignore_extra_elements: true,
                ignore_extra_array_items: true,
                ignore_array_order: true,
                ..Default::default()
            });
            return Ok(());
        }
    };
    let not = o.get("not").and_then(Value::as_bool).unwrap_or(false);
    let ty = o
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_ascii_uppercase);
    let s = |k: &str| o.get(k).map(value_text);
    let m = match ty.as_deref() {
        None => BodyMatcher {
            equal_to_json: Some(b.clone()),
            ignore_extra_elements: true,
            ignore_extra_array_items: true,
            ignore_array_order: true,
            ..Default::default()
        },
        Some("STRING") => {
            let text = s("string").unwrap_or_default();
            if o.get("subString").and_then(Value::as_bool) == Some(true) {
                BodyMatcher {
                    contains: Some(text),
                    ..Default::default()
                }
            } else {
                BodyMatcher {
                    equal_to: Some(text),
                    ..Default::default()
                }
            }
        }
        Some("REGEX") => BodyMatcher {
            matches: s("regex"),
            ..Default::default()
        },
        Some("JSON") => {
            let strict = o.get("matchType").and_then(Value::as_str) == Some("STRICT");
            BodyMatcher {
                equal_to_json: Some(json_value(o.get("json").unwrap_or(&Value::Null))),
                ignore_extra_elements: !strict,
                ignore_extra_array_items: !strict,
                ignore_array_order: !strict,
                ..Default::default()
            }
        }
        Some("JSON_SCHEMA") => BodyMatcher {
            json_schema: Some(json_value(o.get("jsonSchema").unwrap_or(&Value::Null))),
            ..Default::default()
        },
        Some("JSON_PATH") => BodyMatcher {
            json_path: s("jsonPath"),
            json_path_dialect: Some(crate::model::JsonPathDialect::Jayway),
            ..Default::default()
        },
        Some("XML") => BodyMatcher {
            equal_to_xml: s("xml"),
            exempted_comparisons: vec!["CHILD_NODELIST_SEQUENCE".into()],
            ..Default::default()
        },
        Some("XPATH") => BodyMatcher {
            xpath: s("xpath"),
            xpath_namespaces: o
                .get("namespacePrefixes")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), value_text(v))).collect())
                .unwrap_or_default(),
            ..Default::default()
        },
        Some("BINARY") => BodyMatcher {
            binary_equal_to: s("base64Bytes"),
            ..Default::default()
        },
        Some("PARAMETERS") => {
            if not {
                return Err("a negated PARAMETERS body cannot be expressed".into());
            }
            req.form_parameters
                .extend(multi_matchers(o.get("parameters"), false, ctx, w));
            return Ok(());
        }
        Some("XML_SCHEMA") => {
            w.push(format!(
                "{ctx}: XML_SCHEMA body matching is not supported; the body is not validated"
            ));
            return Ok(());
        }
        Some(other) => return Err(format!("body type {other} is not supported")),
    };
    req.body.push(BodyMatcher { negate: not, ..m });
    Ok(())
}

fn request_pattern(r: &Value, ctx: &str, w: &mut Vec<String>) -> Result<RequestPattern, String> {
    let mut req = RequestPattern::default();
    if let Some(m) = r.get("method") {
        let m = match m {
            Value::Object(o) => {
                if o.get("not").and_then(Value::as_bool) == Some(true) {
                    return Err("a negated method cannot be expressed".into());
                }
                o.get("value").map(value_text).unwrap_or_default()
            }
            other => value_text(other),
        };
        if m.starts_with('!') {
            return Err("a negated method cannot be expressed".into());
        }
        if !m.is_empty() {
            req.method = Some(m.to_ascii_uppercase());
        }
    }
    if let Some(p) = r.get("path") {
        let p = match p {
            Value::Object(o) => {
                if o.get("not").and_then(Value::as_bool) == Some(true) {
                    return Err("a negated path cannot be expressed".into());
                }
                o.get("value").map(value_text).unwrap_or_default()
            }
            other => value_text(other),
        };
        if p.starts_with('!') {
            return Err("a negated path cannot be expressed".into());
        }
        let has_params = r.get("pathParameters").is_some();
        if has_params || (p.contains('{') && !has_regex_meta(&p)) {
            req.url_path_template = Some(p);
        } else if has_regex_meta(&p) {
            req.url_path_pattern = Some(p);
        } else if !p.is_empty() {
            req.url_path = Some(p);
        }
    }
    req.path_params = multi_matchers(r.get("pathParameters"), false, ctx, w);
    req.query = multi_matchers(r.get("queryStringParameters"), false, ctx, w);
    req.headers = multi_matchers(r.get("headers"), true, ctx, w);
    req.cookies = multi_matchers(r.get("cookies"), false, ctx, w);
    if let Some(s) = r.get("secure").and_then(Value::as_bool) {
        req.scheme = Some(if s { "https" } else { "http" }.into());
    }
    if let Some(k) = r.get("keepAlive").and_then(Value::as_bool) {
        let close = StringMatcher {
            matches: Some("(?i).*close.*".into()),
            ..Default::default()
        };
        req.headers.insert(
            "connection".into(),
            if k {
                StringMatcher {
                    not: Some(Box::new(close)),
                    ..Default::default()
                }
            } else {
                close
            },
        );
    }
    if let Some(b) = r.get("body") {
        body_matchers(b, &mut req, ctx, w)?;
    }
    for k in [
        "socketAddress",
        "protocol",
        "localAddress",
        "remoteAddress",
        "clientCertificateChain",
    ] {
        if r.get(k).is_some() {
            w.push(format!("{ctx}: httpRequest.{k} is not matched"));
        }
    }
    Ok(req)
}

fn header_map(v: Option<&Value>) -> BTreeMap<String, String> {
    entries(v)
        .into_iter()
        .map(|(k, vals)| {
            (
                k,
                vals.iter().map(value_text).collect::<Vec<_>>().join(", "),
            )
        })
        .collect()
}

fn response_headers(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (k, vals) in entries(v) {
        let joined = crate::model::header_values::join(vals.iter().map(value_text));
        match out.keys().find(|x| x.eq_ignore_ascii_case(&k)).cloned() {
            Some(existing) => {
                let prev = out.remove(&existing).unwrap_or_default();
                out.insert(existing, crate::model::header_values::join([prev, joined]));
            }
            None => {
                out.insert(k, joined);
            }
        }
    }
    out
}

fn delay_ms(d: &Value) -> Option<u64> {
    let v = d.get("value").and_then(Value::as_u64)?;
    let unit = d
        .get("timeUnit")
        .and_then(Value::as_str)
        .unwrap_or("MILLISECONDS");
    Some(match unit {
        "NANOSECONDS" => v / 1_000_000,
        "MICROSECONDS" => v / 1000,
        "SECONDS" => v.saturating_mul(1000),
        "MINUTES" => v.saturating_mul(60_000),
        "HOURS" => v.saturating_mul(3_600_000),
        "DAYS" => v.saturating_mul(86_400_000),
        _ => v,
    })
}

fn seconds(amount: u64, unit: &str) -> u64 {
    match unit {
        "NANOSECONDS" | "MICROSECONDS" => 1,
        "MILLISECONDS" => (amount / 1000).max(1),
        "MINUTES" => amount.saturating_mul(60),
        "HOURS" => amount.saturating_mul(3600),
        "DAYS" => amount.saturating_mul(86_400),
        _ => amount,
    }
}

fn response_body(b: &Value, resp: &mut ResponseDef) {
    match b {
        Value::String(s) => resp.body = Some(s.clone()),
        Value::Object(o) => {
            if let Some(ct) = o.get("contentType").and_then(Value::as_str) {
                resp.headers
                    .entry("content-type".into())
                    .or_insert_with(|| ct.to_string());
            }
            match o.get("type").and_then(Value::as_str) {
                Some("JSON") => {
                    resp.json_body = Some(json_value(o.get("json").unwrap_or(&Value::Null)))
                }
                Some("STRING") => resp.body = o.get("string").map(value_text),
                Some("XML") => {
                    resp.headers
                        .entry("content-type".into())
                        .or_insert_with(|| "application/xml".into());
                    resp.body = o.get("xml").map(value_text);
                }
                Some("BINARY") => resp.base64_body = o.get("base64Bytes").map(value_text),
                Some(_) => resp.body = Some(Value::Object(o.clone()).to_string()),
                None => resp.json_body = Some(b.clone()),
            }
        }
        Value::Null => {}
        other => resp.json_body = Some(other.clone()),
    }
}

fn http_response(r: &Value, ctx: &str, w: &mut Vec<String>, templated: bool) -> ResponseDef {
    let mut resp = ResponseDef {
        status: r.get("statusCode").and_then(Value::as_u64).unwrap_or(200) as u16,
        status_message: r
            .get("reasonPhrase")
            .and_then(Value::as_str)
            .map(String::from),
        headers: response_headers(r.get("headers")),
        templated,
        ..Default::default()
    };
    let cookies: Vec<String> = entries(r.get("cookies"))
        .into_iter()
        .map(|(name, values)| {
            let v = values.first().map(value_text).unwrap_or_default();
            format!("{name}={v}")
        })
        .collect();
    if !cookies.is_empty() {
        let key = resp
            .headers
            .keys()
            .find(|k| k.eq_ignore_ascii_case("set-cookie"))
            .cloned()
            .unwrap_or_else(|| "set-cookie".into());
        let mut lines: Vec<String> = resp
            .headers
            .remove(&key)
            .map(|v| {
                v.split(crate::model::header_values::SEPARATOR)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        lines.extend(cookies);
        resp.headers
            .insert(key, crate::model::header_values::join(lines));
    }
    if let Some(b) = r.get("body") {
        response_body(b, &mut resp);
    }
    if let Some(d) = r.get("delay").and_then(delay_ms) {
        resp.delay = Some(Delay {
            fixed_ms: Some(d),
            ..Default::default()
        });
    }
    if r.get("connectionOptions").is_some() {
        w.push(format!(
            "{ctx}: httpResponse.connectionOptions are not supported"
        ));
    }
    resp
}

static MUSTACHE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\{?\s*([^}]*?)\s*\}?\}\}").expect("static regex"));
static INDEXED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^request\.(pathParameters|queryStringParameters|headers)\.([A-Za-z0-9_\-]+)(?:\.0)?$",
    )
    .expect("static regex")
});

fn mustache_expr(e: &str) -> Option<String> {
    if e.starts_with(['#', '^', '/', '!', '>', '&']) {
        return None;
    }
    Some(match e {
        "request.method" | "request.path" | "request.body" => e.to_string(),
        "uuid" => "uuid".into(),
        "now_iso_8601" => "now".into(),
        "now_epoch" => "now format='epoch'".into(),
        "rand_int_10" => "randomInt 0 9".into(),
        "rand_int_100" => "randomInt 0 99".into(),
        _ => {
            if let Some(c) = INDEXED.captures(e) {
                let name = &c[2];
                match &c[1] {
                    "pathParameters" => format!("request.pathParams.[{name}]"),
                    "queryStringParameters" => format!("request.query.[{name}]"),
                    _ => format!("request.headers.[{}]", name.to_ascii_lowercase()),
                }
            } else {
                format!("request.cookies.[{}]", e.strip_prefix("request.cookies.")?)
            }
        }
    })
}

fn convert_mustache(template: &str) -> Option<String> {
    let mut ok = true;
    let out =
        MUSTACHE_TAG.replace_all(template, |c: &regex::Captures| match mustache_expr(&c[1]) {
            Some(x) => format!("{{{{{x}}}}}"),
            None => {
                ok = false;
                String::new()
            }
        });
    ok.then(|| out.into_owned())
}

fn template_response(t: &Value, ctx: &str, w: &mut Vec<String>) -> Option<ResponseDef> {
    let kind = t
        .get("templateType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    if kind != "MUSTACHE" {
        w.push(format!(
            "{ctx}: {kind} response templates are not executed; the expectation was skipped"
        ));
        return None;
    }
    let text = t.get("template").map(value_text).unwrap_or_default();
    let converted = convert_mustache(&text).and_then(|c| serde_json::from_str::<Value>(&c).ok());
    match converted {
        Some(v @ Value::Object(_)) => Some(http_response(&v, ctx, w, true)),
        _ => {
            w.push(format!(
                "{ctx}: the mustache template uses sections or unsupported values; the expectation was skipped"
            ));
            None
        }
    }
}

fn socket_url(sa: &Value) -> Option<String> {
    let host = sa.get("host").and_then(Value::as_str)?;
    let scheme = sa
        .get("scheme")
        .and_then(Value::as_str)
        .unwrap_or("HTTP")
        .to_ascii_lowercase();
    Some(match sa.get("port").and_then(Value::as_u64) {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    })
}

fn forward(e: &Map<String, Value>, ctx: &str, w: &mut Vec<String>) -> Option<ProxyDef> {
    if let Some(f) = e.get("httpForward") {
        return socket_url(f).map(|base_url| ProxyDef {
            base_url,
            additional_headers: BTreeMap::new(),
            remove_headers: vec![],
            strip_prefix: None,
        });
    }
    let o = e.get("httpOverrideForwardedRequest")?;
    let r = o
        .get("httpRequest")
        .or_else(|| o.get("requestOverride"))
        .cloned()
        .unwrap_or(json!({}));
    let mut headers = header_map(r.get("headers"));
    let host = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.clone());
    headers.retain(|k, _| !k.eq_ignore_ascii_case("host"));
    let dropped: Vec<String> = headers
        .keys()
        .filter(|k| crate::outbound::blocked_outbound_header(k))
        .cloned()
        .collect();
    for k in dropped {
        headers.remove(&k);
        w.push(format!(
            "{ctx}: httpOverrideForwardedRequest header {k} was dropped: it selects the target host or changes message framing"
        ));
    }
    let base = r.get("socketAddress").and_then(socket_url).or_else(|| {
        host.map(|h| {
            let secure = r.get("secure").and_then(Value::as_bool).unwrap_or(false);
            format!("{}://{h}", if secure { "https" } else { "http" })
        })
    });
    let Some(base_url) = base else {
        w.push(format!(
            "{ctx}: httpOverrideForwardedRequest has no target host; the expectation was skipped"
        ));
        return None;
    };
    for k in ["path", "queryStringParameters", "body", "method"] {
        if r.get(k).is_some() {
            w.push(format!(
                "{ctx}: httpOverrideForwardedRequest.{k} is not applied; the request is forwarded as received"
            ));
        }
    }
    if o.get("httpResponse").is_some() || o.get("responseOverride").is_some() {
        w.push(format!(
            "{ctx}: overriding the forwarded response is not supported"
        ));
    }
    Some(ProxyDef {
        base_url,
        additional_headers: headers,
        remove_headers: vec![],
        strip_prefix: None,
    })
}

fn http_error(e: &Value) -> ResponseDef {
    let drop = e
        .get("dropConnection")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let bytes = e.get("responseBytes").is_some();
    ResponseDef {
        fault: Some(match (drop, bytes) {
            (_, true) => Fault::RandomDataThenClose,
            (true, false) => Fault::ConnectionReset,
            (false, false) => Fault::EmptyResponse,
        }),
        ..Default::default()
    }
}

pub fn convert_expectation(e: &Value, index: usize) -> (Option<Stub>, Vec<String>) {
    let mut w = Vec::new();
    let id_text = e.get("id").and_then(Value::as_str).map(String::from);
    let ctx = id_text
        .clone()
        .unwrap_or_else(|| format!("expectation #{index}"));
    let Some(o) = e.as_object() else {
        return (None, vec![format!("{ctx}: not a JSON object")]);
    };
    let empty = json!({});
    let req_v = o.get("httpRequest").unwrap_or(&empty);
    let request = match request_pattern(req_v, &ctx, &mut w) {
        Ok(r) => r,
        Err(why) => {
            w.push(format!("{ctx}: {why}; the expectation was skipped"));
            return (None, w);
        }
    };
    for k in [
        "httpResponseClassCallback",
        "httpResponseObjectCallback",
        "httpForwardClassCallback",
        "httpForwardObjectCallback",
        "httpForwardTemplate",
    ] {
        if o.contains_key(k) {
            w.push(format!(
                "{ctx}: {k} runs code and is not supported; the expectation was skipped"
            ));
            return (None, w);
        }
    }
    let response = if let Some(r) = o.get("httpResponse") {
        http_response(r, &ctx, &mut w, false)
    } else if let Some(t) = o.get("httpResponseTemplate") {
        match template_response(t, &ctx, &mut w) {
            Some(r) => r,
            None => return (None, w),
        }
    } else if let Some(p) = forward(o, &ctx, &mut w) {
        ResponseDef {
            proxy: Some(p),
            templated: false,
            ..Default::default()
        }
    } else if let Some(err) = o.get("httpError") {
        http_error(err)
    } else {
        if !o.contains_key("httpOverrideForwardedRequest") {
            w.push(format!(
                "{ctx}: no supported action (httpResponse, httpResponseTemplate, httpForward, httpOverrideForwardedRequest, httpError); the expectation was skipped"
            ));
        }
        return (None, w);
    };
    let times = o.get("times").and_then(|t| {
        if t.get("unlimited").and_then(Value::as_bool) == Some(true) {
            None
        } else {
            t.get("remainingTimes")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
        }
    });
    let ttl_seconds = o.get("timeToLive").and_then(|t| {
        if t.get("unlimited").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }
        let amount = t.get("timeToLive").and_then(Value::as_u64)?;
        let unit = t
            .get("timeUnit")
            .and_then(Value::as_str)
            .unwrap_or("SECONDS");
        Some(seconds(amount, unit).clamp(1, crate::engine::MAX_TTL_SECS))
    });
    let ms_priority = o.get("priority").and_then(Value::as_i64).unwrap_or(0);
    let priority = (crate::model::DEFAULT_PRIORITY as i64 - ms_priority)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let id = id_text
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::now_v7);
    let name = match (&id_text, Uuid::parse_str(id_text.as_deref().unwrap_or(""))) {
        (Some(t), Err(_)) => Some(t.clone()),
        _ => Some(format!(
            "{} {}",
            request.method.clone().unwrap_or_else(|| "ANY".into()),
            request
                .url_path
                .clone()
                .or(request.url_path_template.clone())
                .or(request.url_path_pattern.clone())
                .unwrap_or_else(|| "*".into())
        )),
    };
    let stub = Stub {
        id,
        name,
        priority,
        request,
        response,
        times,
        ttl_seconds,
        metadata: Some(json!({"source": "mockserver"})),
        ..Default::default()
    };
    (Some(stub), w)
}

/// At equal priority MockServer serves the expectation created first: `order` grows with the position in the
/// document and with the time of the import.
pub fn convert(doc: &Value) -> Converted {
    let mut out = Converted::default();
    let list: Vec<Value> = match doc {
        Value::Array(a) => a.clone(),
        Value::Object(o) if o.contains_key("expectations") => match &o["expectations"] {
            Value::Array(a) => a.clone(),
            one @ Value::Object(_) => vec![one.clone()],
            _ => {
                out.warnings
                    .push("`expectations` must be an expectation or an array of them".into());
                return out;
            }
        },
        one @ Value::Object(_) => vec![one.clone()],
        _ => {
            out.warnings
                .push("the document must be an expectation or an array of them".into());
            return out;
        }
    };
    let base = crate::wiremock::order_base();
    if list.len() > MAX_EXPECTATIONS {
        out.warnings.push(format!(
            "at most {MAX_EXPECTATIONS} expectations are imported; {} were ignored",
            list.len() - MAX_EXPECTATIONS
        ));
    }
    for (i, e) in list.iter().take(MAX_EXPECTATIONS).enumerate() {
        let (s, w) = convert_expectation(e, i);
        out.warnings.extend(w);
        if let Some(mut s) = s {
            s.order = Some(base + i as i64);
            out.stubs.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::CompiledStub;
    use crate::request::CapturedRequest;
    use bytes::Bytes;

    fn req(
        method: &str,
        path: &str,
        q: Option<&str>,
        h: &[(&str, &str)],
        body: &str,
    ) -> CapturedRequest {
        CapturedRequest::new(
            method,
            path,
            q,
            h.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            Bytes::from(body.to_string()),
        )
    }

    #[test]
    fn converts_requests_and_responses() {
        let doc = json!([{
            "id": "orders-get",
            "priority": 10,
            "httpRequest": {
                "method": "GET", "path": "/orders/{id}",
                "pathParameters": {"id": ["[0-9]+"]},
                "queryStringParameters": {"expand": ["items", "customer"], "?debug": ["true"], "!secret": [".*"]},
                "headers": {"Accept": ["application/json"]},
                "cookies": {"session": "s-[0-9]+"},
                "secure": false
            },
            "httpResponse": {
                "statusCode": 200, "reasonPhrase": "Found",
                "headers": [{"name": "X-Trace", "values": ["a", "b"]}],
                "cookies": {"seen": "1"},
                "body": {"type": "JSON", "json": "{\"id\": 1}"},
                "delay": {"timeUnit": "SECONDS", "value": 1}
            },
            "times": {"remainingTimes": 2, "unlimited": false},
            "timeToLive": {"timeUnit": "MINUTES", "timeToLive": 5, "unlimited": false}
        }]);
        let c = convert(&doc);
        assert_eq!(c.stubs.len(), 1, "{:?}", c.warnings);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let s = &c.stubs[0];
        assert_eq!(s.name.as_deref(), Some("orders-get"));
        assert_eq!(s.priority, -5);
        assert_eq!(s.times, Some(2));
        assert_eq!(s.ttl_seconds, Some(300));
        assert_eq!(s.response.status_message.as_deref(), Some("Found"));
        assert_eq!(
            s.response.headers["X-Trace"], "a\nb",
            "one header line per value"
        );
        assert_eq!(s.response.headers["set-cookie"], "seen=1");
        assert_eq!(s.response.json_body, Some(json!({"id": 1})));
        assert_eq!(s.response.delay.as_ref().unwrap().fixed_ms, Some(1000));
        let cs = CompiledStub::compile(s).unwrap();
        let ok = req(
            "GET",
            "/orders/7",
            Some("expand=customer&expand=items"),
            &[("accept", "application/json"), ("cookie", "session=s-1")],
            "",
        );
        assert!(cs.match_request(&ok, None).is_some());
        let debug_bad = req(
            "GET",
            "/orders/7",
            Some("expand=customer&expand=items&debug=no"),
            &[("accept", "application/json"), ("cookie", "session=s-1")],
            "",
        );
        assert!(cs.match_request(&debug_bad, None).is_none());
        let secret = req(
            "GET",
            "/orders/7",
            Some("expand=customer&expand=items&secret=1"),
            &[("accept", "application/json"), ("cookie", "session=s-1")],
            "",
        );
        assert!(cs.match_request(&secret, None).is_none());
        assert!(
            cs.match_request(
                &req(
                    "GET",
                    "/orders/x",
                    Some("expand=customer&expand=items"),
                    &[("accept", "application/json"), ("cookie", "session=s-1")],
                    ""
                ),
                None
            )
            .is_none()
        );
    }

    #[test]
    fn body_types() {
        let cases = [
            (json!("exact"), "exact", "other"),
            (
                json!({"type": "STRING", "string": "mid", "subString": true}),
                "a mid b",
                "nope",
            ),
            (
                json!({"type": "REGEX", "regex": "id=\\d+"}),
                "id=12",
                "id=x",
            ),
            (
                json!({"type": "JSON", "json": {"a": [1, 2]}}),
                "{\"a\":[2,1],\"b\":1}",
                "{\"a\":[1]}",
            ),
            (
                json!({"type": "JSON", "json": {"a": 1}, "matchType": "STRICT"}),
                "{\"a\":1}",
                "{\"a\":1,\"b\":2}",
            ),
            (
                json!({"type": "JSON_SCHEMA", "jsonSchema": {"type": "object", "required": ["a"]}}),
                "{\"a\":1}",
                "{}",
            ),
            (
                json!({"type": "JSON_PATH", "jsonPath": "$.a"}),
                "{\"a\":1}",
                "{}",
            ),
            (
                json!({"type": "XML", "xml": "<a><b>1</b></a>"}),
                "<a> <b>1</b> </a>",
                "<a/>",
            ),
            (
                json!({"type": "XPATH", "xpath": "/a/b"}),
                "<a><b/></a>",
                "<a/>",
            ),
            (json!({"type": "BINARY", "base64Bytes": "aGk="}), "hi", "ho"),
            (
                json!({"type": "STRING", "string": "x", "not": true}),
                "y",
                "x",
            ),
            (json!({"a": 1}), "{\"a\":1,\"b\":2}", "{\"a\":2}"),
        ];
        for (body, good, bad) in cases {
            let c = convert(&json!({"httpRequest": {"body": body.clone()}, "httpResponse": {}}));
            assert_eq!(c.stubs.len(), 1, "{body}: {:?}", c.warnings);
            let cs = CompiledStub::compile(&c.stubs[0]).unwrap_or_else(|e| panic!("{body}: {e}"));
            let g = req("POST", "/", None, &[], good);
            let b = req("POST", "/", None, &[], bad);
            assert!(
                cs.match_request(&g, g.json()).is_some(),
                "{body} should match {good}"
            );
            assert!(
                cs.match_request(&b, b.json()).is_none(),
                "{body} should not match {bad}"
            );
        }
        let form = convert(
            &json!({"httpRequest": {"body": {"type": "PARAMETERS", "parameters": {"a": ["1"]}}}, "httpResponse": {}}),
        );
        assert_eq!(
            form.stubs[0].request.form_parameters["a"]
                .equal_to
                .as_deref(),
            Some("1")
        );
        let xsd = convert(
            &json!({"httpRequest": {"body": {"type": "XML_SCHEMA", "xmlSchema": "<x/>"}}, "httpResponse": {}}),
        );
        assert!(xsd.warnings[0].contains("XML_SCHEMA"));
    }

    #[test]
    fn actions_templates_and_skips() {
        let fwd = convert(
            &json!({"httpRequest": {"path": "/p"}, "httpForward": {"host": "upstream.example.com", "port": 8080, "scheme": "HTTPS"}}),
        );
        assert_eq!(
            fwd.stubs[0].response.proxy.as_ref().unwrap().base_url,
            "https://upstream.example.com:8080"
        );
        let ovr = convert(
            &json!({"httpRequest": {"path": "/p"}, "httpOverrideForwardedRequest": {"httpRequest": {"headers": {"Host": ["api.example.com"], "X-A": ["1"]}}}}),
        );
        let p = ovr.stubs[0].response.proxy.as_ref().unwrap();
        assert_eq!(p.base_url, "http://api.example.com");
        assert_eq!(p.additional_headers["X-A"], "1");
        let err = convert(
            &json!([{"httpError": {"dropConnection": true}}, {"httpError": {"responseBytes": "AAE="}}, {"httpError": {}}]),
        );
        let faults: Vec<_> = err.stubs.iter().map(|s| s.response.fault).collect();
        assert_eq!(
            faults,
            [
                Some(Fault::ConnectionReset),
                Some(Fault::RandomDataThenClose),
                Some(Fault::EmptyResponse)
            ]
        );
        let tpl = convert(
            &json!({"httpRequest": {"path": "/u/{id}", "pathParameters": {"id": [".+"]}}, "httpResponseTemplate": {"templateType": "MUSTACHE",
            "template": "{\"statusCode\": 201, \"headers\": {\"x-id\": [\"{{ request.pathParameters.id.0 }}\"]}, \"body\": \"{{ request.method }} {{ request.headers.X-Key.0 }} {{ uuid }}\"}"}}),
        );
        assert!(tpl.warnings.is_empty(), "{:?}", tpl.warnings);
        let r = &tpl.stubs[0].response;
        assert!(r.templated);
        assert_eq!(r.status, 201);
        assert_eq!(r.headers["x-id"], "{{request.pathParams.[id]}}");
        assert_eq!(
            r.body.as_deref(),
            Some("{{request.method}} {{request.headers.[x-key]}} {{uuid}}")
        );
        for skipped in [
            json!({"httpResponseTemplate": {"templateType": "VELOCITY", "template": "{}"}}),
            json!({"httpResponseTemplate": {"templateType": "JAVASCRIPT", "template": "return {}"}}),
            json!({"httpResponseTemplate": {"templateType": "MUSTACHE", "template": "{{#each x}}{{/each}}"}}),
            json!({"httpResponseClassCallback": {"callbackClass": "a.B"}}),
            json!({"httpRequest": {"method": "!GET"}, "httpResponse": {}}),
            json!({"httpRequest": {"path": "!/x"}, "httpResponse": {}}),
            json!({"httpRequest": {"path": "/x"}}),
        ] {
            let c = convert(&skipped);
            assert!(c.stubs.is_empty(), "{skipped}");
            assert!(!c.warnings.is_empty(), "{skipped}");
        }
    }
}
