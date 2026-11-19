use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::matcher::value_text;
use crate::model::{BodyMatcher, RequestPattern, ResponseDef, StringMatcher, Stub};

pub const API_VERSION: &str = "mocks.microcks.io/v1alpha1";
pub const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EXAMPLES: usize = 5_000;
const MAX_EXPRESSION_BYTES: usize = 4 * 1024;
const MAX_XML_LEAF_MATCHERS: usize = 16;

pub const FAKE_DATA_HELPERS: &[&str] = crate::fakedata::KINDS;

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApiKind {
    Rest,
    Soap,
    #[serde(rename = "GRAPHQL")]
    GraphQl,
    Event,
    Grpc,
    Generic,
}

impl ApiKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiKind::Rest => "REST",
            ApiKind::Soap => "SOAP",
            ApiKind::GraphQl => "GRAPHQL",
            ApiKind::Event => "EVENT",
            ApiKind::Grpc => "GRPC",
            ApiKind::Generic => "GENERIC",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Json,
    Xml,
}

fn find_close(s: &str, from: usize) -> Option<usize> {
    s[from..].find("}}").map(|i| i + from)
}

fn hbs_string(s: &str) -> Result<String, String> {
    if s.contains('"') || s.contains('\\') {
        return Err(format!("argument `{s}` contains a quote or backslash"));
    }
    Ok(format!("\"{s}\""))
}

fn split_top(s: &str, sep: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    let mut i = 0;
    let b = s.as_bytes();
    while i < s.len() {
        let c = s[i..].chars().next().unwrap_or(' ');
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
        } else if depth == 0 && b[i..].starts_with(sep.as_bytes()) {
            out.push(std::mem::take(&mut cur));
            i += sep.len();
            continue;
        }
        cur.push(c);
        i += c.len_utf8();
    }
    out.push(cur);
    out
}

pub fn java_date_format(pattern: &str) -> Result<String, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            let mut j = i + 1;
            if j < chars.len() && chars[j] == '\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            while j < chars.len() && chars[j] != '\'' {
                if chars[j] == '%' {
                    out.push_str("%%");
                } else {
                    out.push(chars[j]);
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            if c == '%' {
                out.push_str("%%");
            } else {
                out.push(c);
            }
            i += 1;
            continue;
        }
        let mut n = 1;
        while i + n < chars.len() && chars[i + n] == c {
            n += 1;
        }
        let spec = match (c, n) {
            ('y', 2) => "%y",
            ('y', _) | ('u', 4) => "%Y",
            ('M', 1) => "%-m",
            ('M', 2) => "%m",
            ('M', 3) => "%b",
            ('M', _) => "%B",
            ('d', 1) => "%-d",
            ('d', _) => "%d",
            ('D', _) => "%j",
            ('H', 1) => "%-H",
            ('H', _) => "%H",
            ('h', 1) => "%-I",
            ('h', _) => "%I",
            ('m', 1) => "%-M",
            ('m', _) => "%M",
            ('s', 1) => "%-S",
            ('s', _) => "%S",
            ('S', 1..=3) => "%3f",
            ('S', 4..=6) => "%6f",
            ('S', _) => "%9f",
            ('a', _) => "%p",
            ('E', 1..=3) => "%a",
            ('E', _) => "%A",
            ('u', _) => "%u",
            ('z', _) => "%Z",
            ('Z', _) => "%z",
            ('X', 1) | ('X', 2) => "%z",
            ('X', _) => "%:z",
            _ => {
                return Err(format!(
                    "date pattern letter `{}` is not supported",
                    std::iter::repeat_n(c, n).collect::<String>()
                ));
            }
        };
        out.push_str(spec);
        i += n;
    }
    Ok(out)
}

fn delta_seconds(delta: &str, warnings: &mut Vec<String>, ctx: &str) -> Result<i64, String> {
    let d = delta.trim();
    let split = d
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '+'))
        .ok_or_else(|| format!("date delta `{d}` has no unit"))?;
    let (num, unit) = d.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| format!("date delta `{d}` is not a number"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "H" | "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "M" => {
            warnings.push(format!(
                "{ctx}: date delta `{d}` uses months; converted to 30-day months"
            ));
            30 * 86_400
        }
        "y" => {
            warnings.push(format!(
                "{ctx}: date delta `{d}` uses years; converted to 365-day years"
            ));
            365 * 86_400
        }
        _ => return Err(format!("date delta unit `{unit}` is not supported")),
    };
    n.checked_mul(mult)
        .filter(|v| v.abs() <= 400 * 365 * 86_400)
        .ok_or_else(|| format!("date delta `{d}` is out of range"))
}

fn json_pointer_path(rest: &str) -> Result<String, String> {
    let mut out = String::from("request.json");
    for seg in rest.trim_start_matches('/').split('/') {
        let seg = seg.replace("~1", "/").replace("~0", "~");
        if seg.is_empty() || seg.contains(['[', ']', ' ', '}', '{', '"', '\'']) {
            return Err(format!("JSON pointer segment `{seg}` is not supported"));
        }
        out.push_str(".[");
        out.push_str(&seg);
        out.push(']');
    }
    Ok(out)
}

fn bracket_arg(rest: &str) -> Result<String, String> {
    let inner = rest
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .ok_or_else(|| format!("expected [name] after request field, got `{rest}`"))?
        .trim()
        .trim_matches(['\'', '"']);
    if inner.is_empty() || inner.contains(['[', ']', ' ', '}', '{', '"', '\'']) {
        return Err(format!("unsupported index `{inner}`"));
    }
    Ok(inner.to_string())
}

fn call_args(args: &str) -> Vec<String> {
    if args.trim().is_empty() {
        return vec![];
    }
    split_top(args, ",")
        .into_iter()
        .map(|a| a.trim().trim_matches(['\'', '"']).to_string())
        .collect()
}

fn function(
    name: &str,
    args: &[String],
    warnings: &mut Vec<String>,
    ctx: &str,
) -> Result<String, String> {
    let int = |s: &str| {
        s.parse::<i64>()
            .map_err(|_| format!("{name}: `{s}` is not an integer"))
    };
    Ok(match (name, args.len()) {
        ("now" | "timestamp", 0) => "(now format=\"epochMillis\")".into(),
        ("now", 1) => format!("(now format={})", hbs_string(&java_date_format(&args[0])?)?),
        ("now", 2) => format!(
            "(now format={} offsetSeconds={})",
            hbs_string(&java_date_format(&args[0])?)?,
            hbs_string(&delta_seconds(&args[1], warnings, ctx)?.to_string())?
        ),
        ("uuid" | "guid" | "randomUUID", 0) => "(uuid)".into(),
        ("randomInt", 0) => "(randomInt -65635 65635)".into(),
        ("randomInt", 1) => format!("(randomInt 0 {})", int(&args[0])?),
        ("randomInt", 2) => format!("(randomInt {} {})", int(&args[0])?, int(&args[1])?),
        ("randomBoolean", 0) => "(pickRandom true false)".into(),
        ("randomValue", n) if n > 0 => {
            let mut s = String::from("(pickRandom");
            for a in args {
                s.push(' ');
                s.push_str(&hbs_string(a)?);
            }
            s.push(')');
            s
        }
        ("randomString", 0) => "(randomAlphaNumeric length=32)".into(),
        ("randomString", 1) => format!(
            "(randomAlphaNumeric length={})",
            int(&args[0])?.clamp(0, 4096)
        ),
        (n, 0) if FAKE_DATA_HELPERS.contains(&n) => format!("({n})"),
        ("put" | "get", _) => return Err("context functions put/get are not supported".into()),
        (n, k) => {
            return Err(format!(
                "function `{n}` with {k} argument(s) is not supported"
            ));
        }
    })
}

fn postman_variable(name: &str) -> Option<&'static str> {
    Some(match name {
        "$guid" | "$randomUUID" => "(uuid)",
        "$timestamp" => "(now format=\"epoch\")",
        "$isoTimestamp" => "(now)",
        "$randomInt" => "(randomInt 0 1000)",
        "$randomBoolean" => "(pickRandom true false)",
        "$randomFirstName" => "(randomFirstName)",
        "$randomLastName" => "(randomLastName)",
        "$randomFullName" => "(randomFullName)",
        "$randomEmail" => "(randomEmail)",
        "$randomCity" => "(randomCity)",
        "$randomCountry" => "(randomCountry)",
        "$randomPhoneNumber" => "(randomPhoneNumber)",
        "$randomStreetAddress" => "(randomStreetAddress)",
        "$randomCompanyName" => "(randomCompanyName)",
        _ => return None,
    })
}

fn term(t: &str, flavor: Flavor, warnings: &mut Vec<String>, ctx: &str) -> Result<String, String> {
    let t = t.trim();
    if t.is_empty() {
        return Err("empty expression".into());
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return hbs_string(&t[1..t.len() - 1]);
    }
    if t.contains('>') {
        return Err("context functions (`> put(...)`) are not supported".into());
    }
    if let Some(rest) = t.strip_prefix("request.body") {
        if rest.is_empty() {
            return Ok("request.body".into());
        }
        if !rest.starts_with('/') {
            return Err(format!("unsupported request field `{t}`"));
        }
        if flavor == Flavor::Xml || rest.starts_with("//") || rest.contains(['[', '@', '(', ':']) {
            return Err("XPath expressions on request.body are not supported in templates".into());
        }
        return json_pointer_path(rest);
    }
    if let Some(rest) = t.strip_prefix("request.path") {
        if rest.is_empty() {
            return Ok("request.path".into());
        }
        let idx = bracket_arg(rest)?;
        let n: usize = idx
            .parse()
            .map_err(|_| format!("request.path index `{idx}` is not a number"))?;
        return Ok(format!("request.pathSegments.[{n}]"));
    }
    if let Some(rest) = t.strip_prefix("request.params") {
        return Ok(format!("request.query.[{}]", bracket_arg(rest)?));
    }
    if let Some(rest) = t.strip_prefix("request.headers") {
        return Ok(format!(
            "request.headers.[{}]",
            bracket_arg(rest)?.to_ascii_lowercase()
        ));
    }
    if let Some(p) = postman_variable(t) {
        return Ok(p.into());
    }
    if let Some(open) = t.find('(') {
        let name = t[..open].trim();
        let Some(args) = t[open + 1..].strip_suffix(')') else {
            return Err(format!("unbalanced parentheses in `{t}`"));
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("`{t}` is not a function call"));
        }
        return function(name, &call_args(args), warnings, ctx);
    }
    Err(format!(
        "`{t}` is not a supported expression (context variables are not supported)"
    ))
}

pub fn convert_expression(
    expr: &str,
    flavor: Flavor,
    warnings: &mut Vec<String>,
    ctx: &str,
) -> Result<String, String> {
    let expr = expr.trim();
    if expr.len() > MAX_EXPRESSION_BYTES {
        return Err("expression is too long".into());
    }
    let alts = split_top(expr, "||");
    let mut parts = Vec::with_capacity(alts.len());
    for a in &alts {
        parts.push(term(a, flavor, warnings, ctx)?);
    }
    let mut acc = parts.pop().unwrap_or_default();
    while let Some(p) = parts.pop() {
        acc = format!("(default {p} {acc})");
    }
    let inner = match acc.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        Some(x) => x.to_string(),
        None => acc,
    };
    if inner.starts_with('"') {
        return Ok(inner.trim_matches('"').to_string());
    }
    Ok(format!("{{{{{inner}}}}}"))
}

fn convert_with(
    s: &str,
    flavor: Flavor,
    ctx: &str,
    warnings: &mut Vec<String>,
    converted: &mut usize,
) -> String {
    let soapui = flavor == Flavor::Xml && s.contains("${");
    if !s.contains("{{") && !soapui {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 16);
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let next_hbs = rest.find("{{");
        let next_soapui = if soapui { rest.find("${") } else { None };
        let (start, opener) = match (next_hbs, next_soapui) {
            (Some(a), Some(b)) if b < a => (b, "${"),
            (Some(a), _) => (a, "{{"),
            (None, Some(b)) => (b, "${"),
            (None, None) => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..start]);
        let body_from = i + start + opener.len();
        let close = if opener == "{{" {
            find_close(s, body_from)
        } else {
            s[body_from..].find('}').map(|x| x + body_from)
        };
        let Some(close) = close else {
            if opener == "{{" {
                out.push('\\');
            }
            out.push_str(&s[i + start..]);
            break;
        };
        let expr = &s[body_from..close];
        let end = close + if opener == "{{" { 2 } else { 1 };
        match convert_expression(expr, flavor, warnings, ctx) {
            Ok(hbs) => {
                *converted += 1;
                out.push_str(&hbs);
            }
            Err(reason) => {
                warnings.push(format!(
                    "{ctx}: template `{}` kept literally: {reason}",
                    &s[i + start..end]
                ));
                if opener == "{{" {
                    out.push('\\');
                }
                out.push_str(&s[i + start..end]);
            }
        }
        i = end;
    }
    out
}

pub fn convert_text(
    s: &str,
    flavor: Flavor,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> (String, bool) {
    let mut n = 0;
    let out = convert_with(s, flavor, ctx, warnings, &mut n);
    if n == 0 {
        (s.to_string(), false)
    } else {
        (out, true)
    }
}

fn convert_json_with(v: &Value, ctx: &str, warnings: &mut Vec<String>, n: &mut usize) -> Value {
    match v {
        Value::String(s) => Value::String(convert_with(s, Flavor::Json, ctx, warnings, n)),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| convert_json_with(x, ctx, warnings, n))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let key = if k.contains("{{") {
                        format!("\\{k}")
                    } else {
                        k.clone()
                    };
                    (key, convert_json_with(x, ctx, warnings, n))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn convert_json(v: &Value, ctx: &str, warnings: &mut Vec<String>) -> (Value, bool) {
    let mut n = 0;
    let out = convert_json_with(v, ctx, warnings, &mut n);
    if n == 0 {
        (v.clone(), false)
    } else {
        (out, true)
    }
}

pub fn set_body(
    resp: &mut ResponseDef,
    media_type: Option<&str>,
    value: &Value,
    flavor: Flavor,
    ctx: &str,
    warnings: &mut Vec<String>,
) {
    let json_media = media_type.is_none_or(|m| m.contains("json"));
    match value {
        Value::Null => {}
        Value::String(s) if json_media && flavor == Flavor::Json => {
            match serde_json::from_str::<Value>(s) {
                Ok(parsed) if parsed.is_object() || parsed.is_array() => {
                    let (b, t) = convert_json(&parsed, ctx, warnings);
                    resp.json_body = Some(b);
                    resp.templated = t;
                }
                _ => {
                    let (b, t) = convert_text(s, flavor, ctx, warnings);
                    resp.body = Some(b);
                    resp.templated = t;
                }
            }
        }
        Value::String(s) => {
            let (b, t) = convert_text(s, flavor, ctx, warnings);
            resp.body = Some(b);
            resp.templated = t;
        }
        other if json_media => {
            let (b, t) = convert_json(other, ctx, warnings);
            resp.json_body = Some(b);
            resp.templated = t;
        }
        other => {
            let (b, t) = convert_text(&value_text(other), flavor, ctx, warnings);
            resp.body = Some(b);
            resp.templated = t;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonBodyRules {
    pub exp: String,
    pub operator: String,
    pub cases: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Dispatcher {
    Inferred,
    None,
    UriParts(Vec<String>),
    UriParams(Vec<String>),
    UriElements(Vec<String>, Vec<String>),
    QueryArgs(Vec<String>),
    QueryHeader(Vec<String>),
    JsonBody(JsonBodyRules),
    QueryMatch(String),
    Random,
    Fallback(Box<Dispatcher>, String),
}

fn names(rules: &str) -> Vec<String> {
    rules
        .split("&&")
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

pub fn parse_dispatcher(
    name: &str,
    rules: &str,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> Dispatcher {
    match name.trim().to_ascii_uppercase().as_str() {
        "" => Dispatcher::Inferred,
        "URI_PARTS" => Dispatcher::UriParts(names(rules)),
        "URI_PARAMS" => Dispatcher::UriParams(names(rules)),
        "URI_ELEMENTS" => {
            let (p, q) = rules.split_once("??").unwrap_or((rules, ""));
            Dispatcher::UriElements(names(p), names(q))
        }
        "QUERY_ARGS" => Dispatcher::QueryArgs(names(rules)),
        "QUERY_HEADER" => Dispatcher::QueryHeader(names(rules)),
        "QUERY_MATCH" | "XPATH" => Dispatcher::QueryMatch(rules.trim().to_string()),
        "RANDOM" => Dispatcher::Random,
        "JSON_BODY" => match serde_json::from_str::<Value>(rules) {
            Ok(v) => {
                let cases = v
                    .get("cases")
                    .and_then(|c| c.as_object())
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), value_text(v))).collect())
                    .unwrap_or_default();
                Dispatcher::JsonBody(JsonBodyRules {
                    exp: v
                        .get("exp")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    operator: v
                        .get("operator")
                        .and_then(|x| x.as_str())
                        .unwrap_or("equals")
                        .to_string(),
                    cases,
                })
            }
            Err(e) => {
                warnings.push(format!(
                    "{ctx}: JSON_BODY dispatcherRules are not valid JSON ({e}); dispatcher ignored"
                ));
                Dispatcher::Inferred
            }
        },
        "FALLBACK" => match serde_json::from_str::<Value>(rules) {
            Ok(v) => {
                let inner = parse_dispatcher(
                    v.get("dispatcher").and_then(|x| x.as_str()).unwrap_or(""),
                    v.get("dispatcherRules")
                        .and_then(|x| x.as_str())
                        .unwrap_or(""),
                    ctx,
                    warnings,
                );
                let fb = v
                    .get("fallback")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                Dispatcher::Fallback(Box::new(inner), fb)
            }
            Err(e) => {
                warnings.push(format!(
                    "{ctx}: FALLBACK dispatcherRules are not valid JSON ({e}); dispatcher ignored"
                ));
                Dispatcher::Inferred
            }
        },
        "SCRIPT" | "GROOVY" | "JS" => {
            warnings.push(format!(
                "{ctx}: dispatcher {name} runs a script; scripts are never executed, examples are dispatched by their request values instead"
            ));
            Dispatcher::Inferred
        }
        "PROXY" | "PROXY_FALLBACK" => {
            warnings.push(format!(
                "{ctx}: dispatcher {name} is not imported; use a stub with a proxy response for pass-through"
            ));
            Dispatcher::Inferred
        }
        other => {
            warnings.push(format!(
                "{ctx}: dispatcher {other} is not supported; examples are dispatched by their request values"
            ));
            Dispatcher::Inferred
        }
    }
}

pub fn pointer_to_json_path(exp: &str) -> String {
    let mut out = String::from("$");
    for seg in exp.trim().trim_start_matches('/').split('/') {
        if seg.is_empty() {
            continue;
        }
        let seg = seg.replace("~1", "/").replace("~0", "~");
        if seg.chars().all(|c| c.is_ascii_digit()) && seg.len() < 10 {
            out.push_str(&format!("[{seg}]"));
        } else {
            out.push_str(&format!(
                "['{}']",
                seg.replace('\\', "\\\\").replace('\'', "\\'")
            ));
        }
    }
    out
}

pub fn json_body_cases(
    rules: &JsonBodyRules,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> (Vec<(BodyMatcher, String)>, Option<String>) {
    let path = pointer_to_json_path(&rules.exp);
    let mut out = Vec::new();
    let mut default = None;
    for (case, response) in &rules.cases {
        if case == "default" {
            default = Some(response.clone());
            continue;
        }
        let value = match rules.operator.as_str() {
            "equals" => Some(StringMatcher {
                equal_to: Some(case.clone()),
                ..Default::default()
            }),
            "regexp" => Some(StringMatcher {
                matches: Some(case.clone()),
                ..Default::default()
            }),
            "presence" if case == "found" => None,
            "presence" => {
                warnings.push(format!(
                    "{ctx}: JSON_BODY presence case `{case}` ignored; only `found` and `default` exist"
                ));
                continue;
            }
            op => {
                warnings.push(format!(
                    "{ctx}: JSON_BODY operator `{op}` is not supported; case `{case}` ignored"
                ));
                continue;
            }
        };
        out.push((
            BodyMatcher {
                json_path: Some(path.clone()),
                value,
                ..Default::default()
            },
            response.clone(),
        ));
    }
    (out, default)
}

pub fn strip_xpath_declarations(expr: &str) -> String {
    let mut rest = expr.trim();
    while rest.starts_with("declare namespace") {
        match rest.find(';') {
            Some(i) => rest = rest[i + 1..].trim_start(),
            None => break,
        }
    }
    let re = regex::Regex::new(r#"\*\[\s*local-name\(\)\s*=\s*['"]([A-Za-z0-9_.-]+)['"]\s*\]"#)
        .expect("static regex");
    re.replace_all(rest, "$1").trim().to_string()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExampleRequest {
    pub parameters: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExampleResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub media_type: Option<String>,
    pub body: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Example {
    pub name: String,
    pub request: ExampleRequest,
    pub response: Option<ExampleResponse>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiExamples {
    pub name: String,
    pub version: String,
    pub operations: Vec<(String, Vec<Example>)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher_rules: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiMetadata {
    pub name: String,
    pub version: String,
    pub labels: BTreeMap<String, String>,
    pub operations: BTreeMap<String, OperationMeta>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Artifact {
    Examples(ApiExamples),
    Metadata(ApiMetadata),
}

impl Artifact {
    pub fn name(&self) -> (&str, &str) {
        match self {
            Artifact::Examples(e) => (&e.name, &e.version),
            Artifact::Metadata(m) => (&m.name, &m.version),
        }
    }
}

fn parse_value(text: &str) -> anyhow::Result<Value> {
    if text.len() > MAX_DOCUMENT_BYTES {
        anyhow::bail!("the document exceeds {MAX_DOCUMENT_BYTES} bytes");
    }
    if text.trim_start().starts_with('{') {
        return Ok(serde_json::from_str(text)?);
    }
    Ok(serde_yaml_ng::from_str(text)?)
}

pub fn detect(text: &str) -> Option<&'static str> {
    let head: String = text.chars().take(4096).collect();
    if !head.contains("mocks.microcks.io/") {
        return None;
    }
    if head.contains("APIExamples") {
        Some("APIExamples")
    } else if head.contains("APIMetadata") {
        Some("APIMetadata")
    } else {
        None
    }
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(|x| x.as_object())
        .map(|o| o.iter().map(|(k, v)| (k.clone(), value_text(v))).collect())
        .unwrap_or_default()
}

fn parse_status(v: Option<&Value>) -> Option<u16> {
    let s = value_text(v?);
    s.trim()
        .parse::<u16>()
        .ok()
        .filter(|c| (100..=599).contains(c))
}

pub fn parse_artifact_value(doc: &Value) -> anyhow::Result<Artifact> {
    let api_version = doc.get("apiVersion").and_then(|v| v.as_str()).unwrap_or("");
    if !api_version.starts_with("mocks.microcks.io/") {
        anyhow::bail!("apiVersion must be {API_VERSION} (got `{api_version}`)");
    }
    let meta = doc
        .get("metadata")
        .ok_or_else(|| anyhow::anyhow!("metadata is required"))?;
    let name = meta
        .get("name")
        .map(value_text)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("metadata.name is required"))?;
    let version = meta
        .get("version")
        .map(value_text)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("metadata.version is required"))?;
    let empty = Map::new();
    let ops = doc
        .get("operations")
        .and_then(|o| o.as_object())
        .unwrap_or(&empty);
    match doc.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "APIExamples" => {
            let mut operations = Vec::new();
            let mut total = 0usize;
            for (op, examples) in ops {
                let mut list = Vec::new();
                for (ex_name, ex) in examples.as_object().into_iter().flatten() {
                    total += 1;
                    if total > MAX_EXAMPLES {
                        anyhow::bail!("at most {MAX_EXAMPLES} examples per document");
                    }
                    let req = ex.get("request");
                    let request = ExampleRequest {
                        parameters: string_map(req.and_then(|r| r.get("parameters"))),
                        headers: string_map(req.and_then(|r| r.get("headers"))),
                        body: req.and_then(|r| r.get("body")).cloned(),
                    };
                    let response = ex.get("response").map(|r| ExampleResponse {
                        status: parse_status(r.get("status"))
                            .or_else(|| parse_status(r.get("code")))
                            .unwrap_or(200),
                        headers: string_map(r.get("headers")),
                        media_type: r.get("mediaType").map(value_text),
                        body: r.get("body").cloned(),
                    });
                    list.push(Example {
                        name: ex_name.clone(),
                        request,
                        response,
                    });
                }
                operations.push((op.clone(), list));
            }
            Ok(Artifact::Examples(ApiExamples {
                name,
                version,
                operations,
            }))
        }
        "APIMetadata" => {
            let mut operations = BTreeMap::new();
            for (op, m) in ops {
                operations.insert(
                    op.clone(),
                    OperationMeta {
                        delay: m
                            .get("delay")
                            .and_then(|d| d.as_u64().or_else(|| value_text(d).trim().parse().ok())),
                        dispatcher: m.get("dispatcher").map(value_text),
                        dispatcher_rules: m.get("dispatcherRules").map(value_text),
                    },
                );
            }
            Ok(Artifact::Metadata(ApiMetadata {
                name,
                version,
                labels: string_map(meta.get("labels")),
                operations,
            }))
        }
        other => anyhow::bail!("kind must be APIExamples or APIMetadata (got `{other}`)"),
    }
}

pub fn parse_artifact(text: &str) -> anyhow::Result<Artifact> {
    parse_artifact_value(&parse_value(text)?)
}

pub fn operation_identity(stub: &Stub) -> RequestPattern {
    let r = &stub.request;
    let mut p = RequestPattern {
        method: r.method.clone(),
        url_path: r.url_path.clone(),
        url_path_pattern: r.url_path_pattern.clone(),
        url_path_template: r.url_path_template.clone(),
        graphql: r.graphql.clone(),
        ..Default::default()
    };
    for (k, v) in &r.headers {
        if k == "soapaction" || k == "content-type" {
            p.headers.insert(k.clone(), v.clone());
        }
    }
    p.body = r
        .body
        .iter()
        .filter(|b| b.xpath.is_some() && b.value.is_none() && !b.negate)
        .cloned()
        .collect();
    p
}

fn xml_leaves(xml: &str, out: &mut Vec<(String, String)>) -> bool {
    let opts = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: crate::xml::MAX_XML_NODES,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(xml, opts) else {
        return false;
    };
    let root = doc.root_element();
    let start = if root.tag_name().name() == "Envelope" {
        root.children()
            .find(|c| c.is_element() && c.tag_name().name() == "Body")
            .and_then(|b| b.children().find(|c| c.is_element()))
    } else {
        Some(root)
    };
    let Some(start) = start else {
        return false;
    };
    for n in start.descendants().filter(|n| n.is_element()) {
        if n.children().any(|c| c.is_element()) {
            continue;
        }
        let text = n.text().unwrap_or("").trim();
        if text.is_empty() || text.contains("${") || text.contains("{{") {
            continue;
        }
        let mut path: Vec<&str> = n
            .ancestors()
            .filter(|a| a.is_element())
            .map(|a| a.tag_name().name())
            .collect();
        path.reverse();
        let mut xp = String::new();
        for seg in &path {
            xp.push('/');
            xp.push_str(seg);
        }
        if crate::xml::parse(&xp).is_ok() {
            out.push((xp, text.to_string()));
        }
        if out.len() >= MAX_XML_LEAF_MATCHERS {
            break;
        }
    }
    true
}

pub struct ExampleTarget<'a> {
    pub kind: ApiKind,
    pub operation: &'a str,
    pub request: RequestPattern,
    pub content_type: Option<String>,
    pub priority: i32,
    pub tags: Vec<String>,
    pub delay_ms: Option<u64>,
}

fn json_from(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => Some(other.clone()),
    }
}

pub fn example_stub(
    target: &ExampleTarget,
    ex: &Example,
    warnings: &mut Vec<String>,
) -> Option<Stub> {
    let ctx = format!("{} / {}", target.operation, ex.name);
    let Some(r) = &ex.response else {
        warnings.push(format!("{ctx}: example has no response; skipped"));
        return None;
    };
    let mut req = target.request.clone();
    let template = req.url_path_template.clone().unwrap_or_default();
    for (k, v) in &ex.request.parameters {
        let m = StringMatcher {
            equal_to: Some(v.clone()),
            ..Default::default()
        };
        if template.contains(&format!("{{{k}}}")) {
            req.path_params.insert(k.clone(), m);
        } else if target.kind == ApiKind::Rest {
            req.query.insert(k.clone(), m);
        } else {
            warnings.push(format!("{ctx}: parameter `{k}` ignored for this API type"));
        }
    }
    for (k, v) in &ex.request.headers {
        let lk = k.to_ascii_lowercase();
        if matches!(lk.as_str(), "accept" | "content-type" | "user-agent") {
            continue;
        }
        req.headers.insert(
            lk,
            StringMatcher {
                equal_to: Some(v.clone()),
                ..Default::default()
            },
        );
    }
    if let Some(body) = &ex.request.body {
        match target.kind {
            ApiKind::GraphQl => match json_from(body) {
                Some(b) => {
                    if let Some(vars) = b
                        .get("variables")
                        .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
                    {
                        req.body.push(BodyMatcher {
                            equal_to_json: Some(json!({"variables": vars})),
                            ignore_extra_elements: true,
                            ..Default::default()
                        });
                    }
                }
                None => warnings.push(format!("{ctx}: GraphQL request body is not JSON; ignored")),
            },
            ApiKind::Soap => {
                let mut leaves = Vec::new();
                if xml_leaves(&value_text(body), &mut leaves) {
                    for (xp, v) in leaves {
                        req.body.push(BodyMatcher {
                            xpath: Some(xp),
                            value: Some(StringMatcher {
                                equal_to: Some(v),
                                ..Default::default()
                            }),
                            ..Default::default()
                        });
                    }
                } else {
                    warnings.push(format!(
                        "{ctx}: request body is not well-formed XML; ignored"
                    ));
                }
            }
            _ => match json_from(body) {
                Some(b) if b.is_object() || b.is_array() => req.body.push(BodyMatcher {
                    equal_to_json: Some(b),
                    ignore_extra_elements: true,
                    ..Default::default()
                }),
                _ => req.body.push(BodyMatcher {
                    equal_to: Some(value_text(body)),
                    ..Default::default()
                }),
            },
        }
    }
    let flavor = if target.kind == ApiKind::Soap {
        Flavor::Xml
    } else {
        Flavor::Json
    };
    let mut resp = ResponseDef {
        status: r.status,
        templated: false,
        ..Default::default()
    };
    let media = r.media_type.clone().or_else(|| {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
    });
    let media = media.or_else(|| target.content_type.clone()).or_else(|| {
        (flavor == Flavor::Json && r.body.as_ref().is_some_and(|b| !b.is_string()))
            .then(|| "application/json".to_string())
    });
    for (k, v) in &r.headers {
        resp.headers.insert(k.to_ascii_lowercase(), v.clone());
    }
    if let Some(m) = &media {
        resp.headers
            .entry("content-type".into())
            .or_insert_with(|| m.clone());
    }
    if let Some(body) = &r.body {
        let body = if target.kind == ApiKind::GraphQl {
            match json_from(body) {
                Some(b) if b.get("data").is_some() || b.get("errors").is_some() => b,
                Some(b) => json!({"data": {target.operation: b}}),
                None => body.clone(),
            }
        } else {
            body.clone()
        };
        set_body(&mut resp, media.as_deref(), &body, flavor, &ctx, warnings);
    }
    if let Some(ms) = target.delay_ms {
        resp.delay = Some(crate::model::Delay {
            fixed_ms: Some(ms),
            ..Default::default()
        });
    }
    Some(Stub {
        id: Uuid::now_v7(),
        name: Some(format!("{} [{}]", target.operation, ex.name)),
        priority: target.priority,
        request: req,
        response: resp,
        tags: target.tags.clone(),
        metadata: Some(json!({"operation": target.operation, "example": ex.name})),
        enabled: true,
        ..Default::default()
    })
}

static PATH_SEGMENT_INDEX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\brequest\.pathSegments\.\[(\d+)\]").expect("static regex")
});

fn shift_text(s: &str, by: usize) -> String {
    if !s.contains("request.pathSegments.") {
        return s.to_string();
    }
    PATH_SEGMENT_INDEX
        .replace_all(s, |c: &regex::Captures| {
            let n = c[1].parse::<usize>().unwrap_or(0).saturating_add(by);
            format!("request.pathSegments.[{n}]")
        })
        .into_owned()
}

fn shift_json(v: &mut Value, by: usize) {
    match v {
        Value::String(s) => *s = shift_text(s, by),
        Value::Array(a) => a.iter_mut().for_each(|x| shift_json(x, by)),
        Value::Object(o) => o.values_mut().for_each(|x| shift_json(x, by)),
        _ => {}
    }
}

/// Microcks numbers `request.path[n]` from the start of the operation path; with a `pathPrefix` the converted
/// `request.pathSegments.[n]` indices are moved past the prefix segments.
pub fn shift_path_segments(stub: &mut Stub, by: usize) {
    if by == 0 {
        return;
    }
    for r in std::iter::once(&mut stub.response).chain(stub.responses.iter_mut()) {
        if !r.templated {
            continue;
        }
        if let Some(b) = &mut r.body {
            *b = shift_text(b, by);
        }
        if let Some(f) = &mut r.body_file_name {
            *f = shift_text(f, by);
        }
        if let Some(j) = &mut r.json_body {
            shift_json(j, by);
        }
        for v in r.headers.values_mut() {
            *v = shift_text(v, by);
        }
    }
}

pub fn stub_operation(stub: &Stub) -> String {
    if let Some(op) = stub
        .metadata
        .as_ref()
        .and_then(|m| m.get("operation"))
        .and_then(|o| o.as_str())
    {
        return op.to_string();
    }
    let r = &stub.request;
    let path = r
        .url_path
        .as_ref()
        .or(r.url_path_template.as_ref())
        .or(r.url_path_pattern.as_ref())
        .or(r.url.as_ref())
        .or(r.url_pattern.as_ref())
        .cloned()
        .unwrap_or_else(|| "/*".into());
    format!(
        "{} {path}",
        r.method.clone().unwrap_or_else(|| "ANY".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(s: &str) -> (String, bool, Vec<String>) {
        let mut w = Vec::new();
        let (o, t) = convert_text(s, Flavor::Json, "t", &mut w);
        (o, t, w)
    }

    #[test]
    fn converts_microcks_expressions() {
        let cases = [
            ("{{ request.body/name }}", "{{request.json.[name]}}"),
            (
                "{{request.body/books/1/author}}",
                "{{request.json.[books].[1].[author]}}",
            ),
            ("{{ request.path[1] }}", "{{request.pathSegments.[1]}}"),
            ("{{ request.params[locale] }}", "{{request.query.[locale]}}"),
            (
                "{{ request.headers[X-Trace] }}",
                "{{request.headers.[x-trace]}}",
            ),
            ("{{ now() }}", "{{now format=\"epochMillis\"}}"),
            (
                "{{ now(dd/MM/yyyy HH:mm:ss) }}",
                "{{now format=\"%d/%m/%Y %H:%M:%S\"}}",
            ),
            (
                "{{ now(yyyy-MM-dd'T'HH:mm, 2d) }}",
                "{{now format=\"%Y-%m-%dT%H:%M\" offsetSeconds=\"172800\"}}",
            ),
            ("{{ uuid() }}", "{{uuid}}"),
            ("{{ guid() }}", "{{uuid}}"),
            ("{{ randomInt(5, 10) }}", "{{randomInt 5 10}}"),
            ("{{ randomInt(10) }}", "{{randomInt 0 10}}"),
            ("{{ randomValue(a, b) }}", "{{pickRandom \"a\" \"b\"}}"),
            ("{{ randomFirstName() }}", "{{randomFirstName}}"),
            (
                "{{ request.body/name || 'John' }}",
                "{{default request.json.[name] \"John\"}}",
            ),
            (
                "{{ request.body/a || request.body/b || randomFirstName() }}",
                "{{default request.json.[a] (default request.json.[b] (randomFirstName))}}",
            ),
            ("{{ request.body }}", "{{request.body}}"),
        ];
        for (input, expected) in cases {
            let (out, templated, w) = conv(input);
            assert_eq!(out, expected, "{input}: {w:?}");
            assert!(templated);
            assert!(w.is_empty(), "{input}: {w:?}");
        }
    }

    #[test]
    fn unknown_functions_stay_literal_and_escaped() {
        let (out, t, w) = conv("Hello {{ myFunc() }}");
        assert_eq!(out, "Hello {{ myFunc() }}");
        assert!(!t);
        assert_eq!(w.len(), 1);
        let (out, t, w) = conv("{{ uuid() }} {{ x > put(y) }}");
        assert_eq!(out, "{{uuid}} \\{{ x > put(y) }}");
        assert!(t);
        assert_eq!(w.len(), 1);
        let mut w = Vec::new();
        let (o, t) = convert_text("<a>{{ request.body//name }}</a>", Flavor::Xml, "t", &mut w);
        assert!(!t && o.contains("{{ request.body//name }}") && w.len() == 1);
        let (o, t) = convert_text("<a>${ uuid() }</a>", Flavor::Xml, "t", &mut w);
        assert!(t);
        assert_eq!(o, "<a>{{uuid}}</a>");
    }

    #[test]
    fn converted_templates_render() {
        let tpl = crate::template::Templates::new();
        let ctx = json!({"request": {"json": {"name": "Ann", "books": [{"a": 1}, {"a": 2}]},
            "pathSegments": ["hello", "microcks"], "query": {"locale": "fr"}, "headers": {"x-trace": "t1"}, "body": "{}"}});
        let (v, t) = convert_json(
            &json!({"n": "{{ request.body/name }}", "b": "{{ request.body/books/1/a }}", "p": "{{ request.path[1] }}",
                "q": "{{ request.params[locale] }}", "h": "{{ request.headers[X-Trace] }}", "f": "{{ request.body/missing || 'dflt' }}",
                "d": "{{ now(yyyy) }}", "rs": "{{ randomString(8) }}", "fn": "{{ randomFirstName() }}", "lat": "{{ randomLatitude() }}", "lit": "a {{ nope() }} b"}),
            "t",
            &mut Vec::new(),
        );
        assert!(t);
        let out = tpl.render_json(&v, &ctx).unwrap();
        assert_eq!(out["n"], "Ann");
        assert_eq!(out["b"], "2");
        assert_eq!(out["p"], "microcks");
        assert_eq!(out["q"], "fr");
        assert_eq!(out["h"], "t1");
        assert_eq!(out["f"], "dflt");
        assert_eq!(out["d"].as_str().unwrap().len(), 4);
        assert_eq!(out["lit"], "a {{ nope() }} b");
        assert_eq!(out["rs"].as_str().unwrap().len(), 8);
        assert!(!out["fn"].as_str().unwrap().is_empty());
        assert_eq!(out["lat"], "{{ randomLatitude() }}");
    }

    #[test]
    fn java_date_patterns() {
        assert_eq!(java_date_format("yyyy-MM-dd").unwrap(), "%Y-%m-%d");
        assert_eq!(
            java_date_format("EEE, d MMM yyyy HH:mm:ss Z").unwrap(),
            "%a, %-d %b %Y %H:%M:%S %z"
        );
        assert_eq!(java_date_format("HH:mm:ss.SSS").unwrap(), "%H:%M:%S.%3f");
        assert_eq!(java_date_format("'at' h a").unwrap(), "at %-I %p");
        assert!(java_date_format("GGG").is_err());
    }

    #[test]
    fn dispatchers_parse() {
        let mut w = Vec::new();
        assert_eq!(
            parse_dispatcher("URI_ELEMENTS", "petId ?? user_key && x", "t", &mut w),
            Dispatcher::UriElements(vec!["petId".into()], vec!["user_key".into(), "x".into()])
        );
        let fb = parse_dispatcher(
            "FALLBACK",
            r#"{"dispatcher": "URI_PARTS", "dispatcherRules": "region", "fallback": "Unknown"}"#,
            "t",
            &mut w,
        );
        assert_eq!(
            fb,
            Dispatcher::Fallback(
                Box::new(Dispatcher::UriParts(vec!["region".into()])),
                "Unknown".into()
            )
        );
        let jb = parse_dispatcher(
            "JSON_BODY",
            r#"{"exp": "/country", "operator": "equals", "cases": {"Belgium": "be", "default": "other"}}"#,
            "t",
            &mut w,
        );
        let Dispatcher::JsonBody(rules) = jb else {
            panic!()
        };
        let (cases, dflt) = json_body_cases(&rules, "t", &mut w);
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].0.json_path.as_deref(), Some("$['country']"));
        assert_eq!(dflt.as_deref(), Some("other"));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            parse_dispatcher("SCRIPT", "return 'x'", "t", &mut w),
            Dispatcher::Inferred
        );
        assert_eq!(w.len(), 1);
        assert_eq!(
            strip_xpath_declarations(
                "declare namespace ser='http://x'; //ser:sayHello/*[local-name() = 'name']"
            ),
            "//ser:sayHello/name"
        );
    }

    #[test]
    fn parses_companion_artifacts() {
        let ex = parse_artifact(include_str!("../tests/fixtures/pastry-examples.yaml")).unwrap();
        let Artifact::Examples(ex) = ex else { panic!() };
        assert_eq!(ex.name, "API Pastry - 2.0");
        assert_eq!(ex.version, "2.0.0");
        let (op, list) = &ex.operations[0];
        assert_eq!(op, "GET /pastry/{name}");
        assert_eq!(list[0].request.parameters["name"], "Croissant");
        let md = parse_artifact(include_str!("../tests/fixtures/pastry-metadata.yaml")).unwrap();
        let Artifact::Metadata(md) = md else { panic!() };
        assert_eq!(md.labels["team"], "pastry");
        assert_eq!(md.operations["GET /pastry/{name}"].delay, Some(25));
        assert!(detect("apiVersion: mocks.microcks.io/v1alpha1\nkind: APIMetadata").is_some());
        assert!(parse_artifact("apiVersion: v1\nkind: APIExamples").is_err());
    }

    #[test]
    fn example_stubs_for_each_kind() {
        let mut w = Vec::new();
        let target = ExampleTarget {
            kind: ApiKind::Rest,
            operation: "GET /pastry/{name}",
            request: RequestPattern {
                method: Some("GET".into()),
                url_path_template: Some("/pastry/{name}".into()),
                ..Default::default()
            },
            content_type: None,
            priority: 3,
            tags: vec!["t".into()],
            delay_ms: Some(5),
        };
        let ex = Example {
            name: "e".into(),
            request: ExampleRequest {
                parameters: BTreeMap::from([
                    ("name".into(), "Eclair".into()),
                    ("lang".into(), "fr".into()),
                ]),
                ..Default::default()
            },
            response: Some(ExampleResponse {
                status: 200,
                headers: BTreeMap::new(),
                media_type: None,
                body: Some(json!({"name": "{{ request.path[1] }}"})),
            }),
        };
        let s = example_stub(&target, &ex, &mut w).unwrap();
        assert_eq!(
            s.request.path_params["name"].equal_to.as_deref(),
            Some("Eclair")
        );
        assert_eq!(s.request.query["lang"].equal_to.as_deref(), Some("fr"));
        assert!(s.response.templated);
        assert_eq!(s.response.headers["content-type"], "application/json");
        assert_eq!(s.response.delay.as_ref().unwrap().fixed_ms, Some(5));
        let soap = ExampleTarget {
            kind: ApiKind::Soap,
            operation: "sayHello",
            request: RequestPattern::default(),
            content_type: Some("text/xml".into()),
            priority: 3,
            tags: vec![],
            delay_ms: None,
        };
        let ex = Example {
            name: "Andrew".into(),
            request: ExampleRequest {
                body: Some(json!(
                    "<soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:h=\"urn:h\"><soapenv:Body><h:sayHello><name>Andrew</name></h:sayHello></soapenv:Body></soapenv:Envelope>"
                )),
                ..Default::default()
            },
            response: Some(ExampleResponse {
                status: 200,
                headers: BTreeMap::new(),
                media_type: None,
                body: Some(json!("<r/>")),
            }),
        };
        let s = example_stub(&soap, &ex, &mut w).unwrap();
        assert_eq!(
            s.request.body[0].xpath.as_deref(),
            Some("/Envelope/Body/sayHello/name")
        );
        assert_eq!(s.response.body.as_deref(), Some("<r/>"));
        assert!(w.is_empty(), "{w:?}");
    }
}
