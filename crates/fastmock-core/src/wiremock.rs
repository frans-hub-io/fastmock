use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::matcher::value_text;
use crate::model::{
    BasicAuth, BodyMatcher, CallbackDef, ChunkedDribbleDelay, Delay, Fault, LogNormalDelay,
    MultipartMatcher, MultipartMatchingType, ProxyDef, RequestPattern, ResponseDef, ScenarioRule,
    StringMatcher, Stub,
};

#[derive(Debug, Default)]
pub struct Converted {
    pub stubs: Vec<Stub>,
    pub warnings: Vec<String>,
}

const MAX_NESTING: usize = 8;

fn date_expected(o: &serde_json::Map<String, Value>, base: &Value) -> String {
    let base = value_text(base);
    match (
        o.get("expectedOffset").and_then(Value::as_i64),
        o.get("expectedOffsetUnit").and_then(Value::as_str),
    ) {
        (Some(n), Some(u)) if n != 0 => format!("{base} {n:+} {}", u.to_ascii_lowercase()),
        _ => base,
    }
}

fn string_matcher(v: &Value, ctx: &str, w: &mut Vec<String>) -> StringMatcher {
    string_matcher_at(v, ctx, w, 0)
}

fn string_list(v: &Value, ctx: &str, w: &mut Vec<String>, depth: usize) -> Vec<StringMatcher> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| string_matcher_at(x, ctx, w, depth + 1))
                .collect()
        })
        .unwrap_or_default()
}

fn string_matcher_at(v: &Value, ctx: &str, w: &mut Vec<String>, depth: usize) -> StringMatcher {
    let mut m = StringMatcher {
        case_insensitive: v
            .get("caseInsensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ..Default::default()
    };
    let Some(o) = v.as_object() else {
        m.equal_to = Some(value_text(v));
        return m;
    };
    if depth > MAX_NESTING {
        w.push(format!(
            "{ctx}: matcher nesting is too deep; the inner part was ignored"
        ));
        return m;
    }
    for (k, x) in o {
        let s = || value_text(x);
        match k.as_str() {
            "equalTo" => m.equal_to = Some(s()),
            "contains" => m.contains = Some(s()),
            "doesNotContain" => {
                m.not = Some(Box::new(StringMatcher {
                    contains: Some(s()),
                    case_insensitive: m.case_insensitive,
                    ..Default::default()
                }))
            }
            "matches" => m.matches = Some(s()),
            "doesNotMatch" => m.does_not_match = Some(s()),
            "absent" => m.absent = x.as_bool(),
            "and" => m.and = string_list(x, ctx, w, depth),
            "or" => m.or = string_list(x, ctx, w, depth),
            "not" => m.not = Some(Box::new(string_matcher_at(x, ctx, w, depth + 1))),
            "hasExactly" => m.has_exactly = Some(string_list(x, ctx, w, depth)),
            "includes" => m.includes = Some(string_list(x, ctx, w, depth)),
            "before" => m.before = Some(date_expected(o, x)),
            "after" => m.after = Some(date_expected(o, x)),
            "equalToDateTime" => m.equal_to_date_time = Some(date_expected(o, x)),
            "actualFormat" => {
                m.actual_format = Some(match s().as_str() {
                    "epoch" => "epochMillis".to_string(),
                    other => other.to_string(),
                })
            }
            "truncateExpected" | "truncateActual" => {
                match serde_json::from_value(Value::String(s().to_ascii_lowercase())) {
                    Ok(t) if k == "truncateExpected" => m.truncate_expected = Some(t),
                    Ok(t) => m.truncate_actual = Some(t),
                    Err(_) => w.push(format!("{ctx}: {k} `{}` is not supported", s())),
                }
            }
            "caseInsensitive" | "expectedOffset" | "expectedOffsetUnit" => {}
            other => w.push(format!(
                "{ctx}: matcher `{other}` is not supported and was ignored"
            )),
        }
    }
    m
}

fn parse_json_text(x: &Value) -> Value {
    match x {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
        other => other.clone(),
    }
}

fn namespaces(o: &serde_json::Map<String, Value>) -> BTreeMap<String, String> {
    o.get("xPathNamespaces")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), value_text(v))).collect())
        .unwrap_or_default()
}

const BODY_SUB_MATCHERS: &[&str] = &[
    "equalToJson",
    "matchesJsonSchema",
    "equalToXml",
    "matchesJsonPath",
    "matchesXPath",
    "binaryEqualTo",
];

struct Expression {
    expr: String,
    value: Option<StringMatcher>,
    value_body: Option<Box<BodyMatcher>>,
    namespaces: BTreeMap<String, String>,
}

fn expression_matcher(x: &Value, ctx: &str, w: &mut Vec<String>) -> Result<Expression, String> {
    match x {
        Value::String(p) => Ok(Expression {
            expr: p.clone(),
            value: None,
            value_body: None,
            namespaces: BTreeMap::new(),
        }),
        Value::Object(e) => {
            let mut rest = e.clone();
            let expr = rest
                .remove("expression")
                .and_then(|v| v.as_str().map(String::from))
                .ok_or("an expression matcher has no `expression`")?;
            let namespaces = namespaces(&rest);
            rest.remove("xPathNamespaces");
            let mut out = Expression {
                expr,
                value: None,
                value_body: None,
                namespaces,
            };
            if rest.keys().any(|k| BODY_SUB_MATCHERS.contains(&k.as_str())) {
                let mut sub_warnings = Vec::new();
                match body_matcher_of(&rest, ctx, &mut sub_warnings)? {
                    Some(b) if sub_warnings.is_empty() => out.value_body = Some(Box::new(b)),
                    _ => {
                        return Err(format!(
                            "the sub-matcher {} of `{}` is not supported",
                            Value::Object(rest),
                            out.expr
                        ));
                    }
                }
            } else if !rest.is_empty() {
                out.value = Some(string_matcher(&Value::Object(rest), ctx, w));
            }
            Ok(out)
        }
        _ => Err("an expression matcher must be a string or an object".into()),
    }
}

fn body_matcher(v: &Value, ctx: &str, w: &mut Vec<String>) -> Result<Option<BodyMatcher>, String> {
    let Some(o) = v.as_object() else {
        return Ok(None);
    };
    body_matcher_of(o, ctx, w)
}

fn body_matcher_of(
    o: &serde_json::Map<String, Value>,
    ctx: &str,
    w: &mut Vec<String>,
) -> Result<Option<BodyMatcher>, String> {
    let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
    let placeholders = o.get("enablePlaceholders").and_then(Value::as_bool);
    if o.contains_key("placeholderOpeningDelimiterRegex")
        || o.contains_key("placeholderClosingDelimiterRegex")
    {
        w.push(format!(
            "{ctx}: custom placeholder delimiters are not supported; ${{...}} is used"
        ));
    }
    if let Some(x) = o.get("equalToJson") {
        return Ok(Some(BodyMatcher {
            equal_to_json: Some(parse_json_text(x)),
            ignore_extra_elements: flag("ignoreExtraElements"),
            ignore_extra_array_items: flag("ignoreExtraElements"),
            ignore_array_order: flag("ignoreArrayOrder"),
            enable_placeholders: placeholders,
            ..Default::default()
        }));
    }
    if let Some(x) = o.get("matchesJsonPath") {
        let e = expression_matcher(x, ctx, w)?;
        return Ok(Some(BodyMatcher {
            json_path: Some(e.expr),
            json_path_dialect: Some(crate::model::JsonPathDialect::Jayway),
            value: e.value,
            value_body: e.value_body,
            ..Default::default()
        }));
    }
    if let Some(x) = o.get("matchesJsonSchema") {
        let schema = match x {
            Value::String(s) => serde_json::from_str(s)
                .map_err(|e| format!("matchesJsonSchema is not valid JSON: {e}"))?,
            other => other.clone(),
        };
        if o.contains_key("schemaVersion") {
            w.push(format!(
                "{ctx}: schemaVersion is ignored; the draft is taken from $schema"
            ));
        }
        return Ok(Some(BodyMatcher {
            json_schema: Some(schema),
            ..Default::default()
        }));
    }
    if let Some(x) = o.get("equalToXml") {
        return Ok(Some(BodyMatcher {
            equal_to_xml: Some(value_text(x)),
            exempted_comparisons: o
                .get("exemptedComparisons")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(value_text).collect())
                .unwrap_or_default(),
            enable_placeholders: Some(placeholders.unwrap_or(false)),
            ..Default::default()
        }));
    }
    if let Some(x) = o.get("matchesXPath") {
        let mut e = expression_matcher(x, ctx, w)?;
        e.namespaces.extend(namespaces(o));
        return Ok(Some(BodyMatcher {
            xpath: Some(e.expr),
            value: e.value,
            value_body: e.value_body,
            xpath_namespaces: e.namespaces,
            ..Default::default()
        }));
    }
    if let Some(x) = o.get("binaryEqualTo") {
        return Ok(Some(BodyMatcher {
            binary_equal_to: Some(value_text(x)),
            ..Default::default()
        }));
    }
    if o.get("absent").and_then(Value::as_bool) == Some(true) {
        return Ok(Some(BodyMatcher {
            absent: true,
            ..Default::default()
        }));
    }
    let text = |k: &str| o.get(k).map(value_text);
    if let Some(s) = text("equalTo") {
        if flag("caseInsensitive") {
            return Ok(Some(BodyMatcher {
                matches: Some(format!("(?is){}", regex::escape(&s))),
                ..Default::default()
            }));
        }
        return Ok(Some(BodyMatcher {
            equal_to: Some(s),
            ..Default::default()
        }));
    }
    if let Some(s) = text("contains") {
        return Ok(Some(BodyMatcher {
            contains: Some(s),
            ..Default::default()
        }));
    }
    if let Some(s) = text("doesNotContain") {
        return Ok(Some(BodyMatcher {
            contains: Some(s),
            negate: true,
            ..Default::default()
        }));
    }
    if let Some(s) = text("matches") {
        return Ok(Some(BodyMatcher {
            matches: Some(s),
            ..Default::default()
        }));
    }
    if let Some(s) = text("doesNotMatch") {
        return Ok(Some(BodyMatcher {
            matches: Some(s),
            negate: true,
            ..Default::default()
        }));
    }
    w.push(format!(
        "{ctx}: body pattern {} is not supported and was ignored",
        Value::Object(o.clone())
    ));
    Ok(None)
}

static PATH_PART: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(originalRequest|request)\.path\.(?:\[([^\]]+)\]|([A-Za-z0-9_\-]+))")
        .expect("static regex")
});
static QUERY_INDEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(originalRequest|request)\.query\.([A-Za-z0-9_\-]+)\.\[?(\d+)\]?")
        .expect("static regex")
});
static QUERY_LIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(#each|\bsize|\bjoin)(\s+)(originalRequest|request)\.query\.")
        .expect("static regex")
});
static HEADER_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(#each\s+|\bsize\s+|\bjoin\s+)?\b(originalRequest|request)\.headers\.(?:\[([^\]]+)\]|([A-Za-z0-9_\-]+))(\.\[?(\d+)\]?)?",
    )
    .expect("static regex")
});
static NOW_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*now(\s[^{}]*)?\}\}").expect("static regex"));
static RANDOM_INT_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*randomInt(\s[^{}]*)?\}\}").expect("static regex"));
static UPPER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bupper=(?:(-?\d+)|'(-?\d+)'|"(-?\d+)")"#).expect("static regex")
});
static EPOCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"format=(['"])epoch(['"])"#).expect("static regex"));

pub fn convert_templates(s: &str) -> String {
    if !s.contains("{{") {
        return s.to_string();
    }
    let s = s
        .replace("request.requestLine.pathSegments", "request.pathSegments")
        .replace("request.requestLine.path", "request.path")
        .replace("request.requestLine.method", "request.method")
        .replace("request.requestLine.scheme", "request.scheme")
        .replace("request.requestLine.host", "request.host")
        .replace("request.requestLine.port", "request.port")
        .replace("request.requestLine.baseUrl", "request.baseUrl")
        .replace("request.requestLine.query", "request.query");
    let s = PATH_PART.replace_all(&s, |c: &regex::Captures| {
        let name = c.get(2).or_else(|| c.get(3)).map_or("", |m| m.as_str());
        if name.chars().all(|ch| ch.is_ascii_digit()) {
            format!("{}.pathSegments.[{name}]", &c[1])
        } else {
            format!("{}.pathParams.[{name}]", &c[1])
        }
    });
    let s = QUERY_INDEX.replace_all(&s, "$1.queryAll.$2.[$3]");
    let s = QUERY_LIST.replace_all(&s, "$1$2$3.queryAll.");
    let s = HEADER_NAME.replace_all(&s, |c: &regex::Captures| {
        let list = c.get(1).map_or("", |m| m.as_str());
        let name = c.get(3).or_else(|| c.get(4)).map_or("", |m| m.as_str());
        let lower = name.to_ascii_lowercase();
        let key = if lower
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            lower
        } else {
            format!("[{lower}]")
        };
        match (c.get(6), list.is_empty()) {
            (Some(i), _) => format!("{list}{}.headersAll.{key}.[{}]", &c[2], i.as_str()),
            (None, false) => format!("{list}{}.headersAll.{key}", &c[2]),
            (None, true) => format!("{}.headers.{key}", &c[2]),
        }
    });
    let s = NOW_TAG.replace_all(&s, |c: &regex::Captures| {
        let args = c.get(1).map_or("", |m| m.as_str());
        if args.contains("format=") {
            c[0].to_string()
        } else {
            format!("{{{{now{} format='isoSeconds'}}}}", args.trim_end())
        }
    });
    let s = RANDOM_INT_TAG.replace_all(&s, |c: &regex::Captures| {
        let args = c.get(1).map_or("", |m| m.as_str());
        let mut args = UPPER
            .replace(args, |u: &regex::Captures| {
                let n: i64 = u
                    .get(1)
                    .or_else(|| u.get(2))
                    .or_else(|| u.get(3))
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(i32::MAX as i64);
                format!("upper={}", n.saturating_sub(1))
            })
            .trim_end()
            .to_string();
        if !args.contains("upper=") {
            args.push_str(&format!(" upper={}", i32::MAX - 1));
        }
        if !args.contains("lower=") {
            args.push_str(&format!(" lower={}", i32::MIN));
        }
        format!("{{{{randomInt{args}}}}}")
    });
    EPOCH
        .replace_all(&s, "format=${1}epochMillis${2}")
        .into_owned()
}

fn convert_json_templates(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(convert_templates(s)),
        Value::Array(a) => Value::Array(a.iter().map(convert_json_templates).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (convert_templates(k), convert_json_templates(x)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn warn_unsafe_helpers(text: &str, ctx: &str, w: &mut Vec<String>) {
    for (needle, what) in [
        (
            "systemValue",
            "systemValue (environment and system properties)",
        ),
        ("hostname", "hostname"),
        ("parameters.", "transformerParameters"),
        ("#assign", "assign"),
    ] {
        if text.contains(needle) {
            w.push(format!(
                "{ctx}: template helper {what} is not available; it renders empty"
            ));
        }
    }
}

fn delay_of(d: &Value, ctx: &str, w: &mut Vec<String>) -> Option<Delay> {
    match d.get("type").and_then(Value::as_str) {
        Some("uniform") => Some(Delay {
            min_ms: d.get("lower").and_then(Value::as_u64),
            max_ms: d.get("upper").and_then(Value::as_u64),
            ..Default::default()
        }),
        Some("fixed") => Some(Delay {
            fixed_ms: d.get("milliseconds").and_then(Value::as_u64),
            ..Default::default()
        }),
        Some("lognormal") => Some(Delay {
            lognormal: Some(LogNormalDelay {
                median_ms: d
                    .get("median")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .max(0.0) as u64,
                sigma: d.get("sigma").and_then(Value::as_f64).unwrap_or(0.0),
            }),
            ..Default::default()
        }),
        other => {
            w.push(format!(
                "{ctx}: delay distribution {other:?} is not supported and was dropped"
            ));
            None
        }
    }
}

fn string_map(
    r: &Value,
    key: &str,
    lower: bool,
    ctx: &str,
    w: &mut Vec<String>,
) -> BTreeMap<String, StringMatcher> {
    r.get(key)
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    (
                        if lower {
                            k.to_ascii_lowercase()
                        } else {
                            k.clone()
                        },
                        string_matcher(v, ctx, w),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn request_pattern(r: &Value, ctx: &str, w: &mut Vec<String>) -> Result<RequestPattern, String> {
    let s = |k: &str| r.get(k).and_then(Value::as_str).map(String::from);
    let mut req = RequestPattern {
        method: s("method"),
        url: s("url"),
        url_pattern: s("urlPattern"),
        url_path: s("urlPath"),
        url_path_pattern: s("urlPathPattern"),
        url_path_template: s("urlPathTemplate"),
        scheme: s("scheme"),
        port: r
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok()),
        host: r.get("host").map(|h| string_matcher(h, ctx, w)),
        query: string_map(r, "queryParameters", false, ctx, w),
        headers: string_map(r, "headers", true, ctx, w),
        path_params: string_map(r, "pathParameters", false, ctx, w),
        cookies: string_map(r, "cookies", false, ctx, w),
        form_parameters: string_map(r, "formParameters", false, ctx, w),
        ..Default::default()
    };
    if let Some(b) = r.get("basicAuthCredentials") {
        req.basic_auth = Some(BasicAuth {
            username: b.get("username").map(value_text).unwrap_or_default(),
            password: b.get("password").map(value_text).unwrap_or_default(),
        });
    }
    if let Some(Value::Array(parts)) = r.get("multipartPatterns") {
        let mut types = std::collections::BTreeSet::new();
        for p in parts {
            types.insert(
                p.get("matchingType")
                    .and_then(Value::as_str)
                    .unwrap_or("ANY")
                    .to_ascii_uppercase(),
            );
            let mut headers = string_map(p, "headers", true, ctx, w);
            let mut name = p.get("name").and_then(Value::as_str).map(String::from);
            if name.is_none()
                && let Some(cd) = headers.get("content-disposition")
                && let Some(eq) = &cd.contains
                && let Some(n) = eq.split(';').find_map(|x| x.trim().strip_prefix("name="))
            {
                name = Some(n.trim_matches('"').to_string());
                headers.remove("content-disposition");
            }
            req.multipart.push(MultipartMatcher {
                name,
                filename: p.get("filename").map(|f| string_matcher(f, ctx, w)),
                headers,
                body: p
                    .get("bodyPatterns")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| body_matcher(b, ctx, w).transpose())
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
            });
        }
        if types.contains("ALL") {
            req.multipart_matching_type = Some(MultipartMatchingType::All);
            if types.len() > 1 {
                w.push(format!(
                    "{ctx}: multipart patterns mix ANY and ALL; ALL is applied to every pattern"
                ));
            }
        }
    }
    if r.get("customMatcher").is_some() {
        w.push(format!(
            "{ctx}: customMatcher (Java extension) is not supported and was ignored"
        ));
    }
    if let Some(Value::Array(bp)) = r.get("bodyPatterns") {
        for b in bp {
            if let Some(m) = body_matcher(b, ctx, w)? {
                req.body.push(m);
            }
        }
    }
    Ok(req)
}

fn response_def(resp_v: &Value, ctx: &str, w: &mut Vec<String>) -> ResponseDef {
    let transformers: Vec<String> = resp_v
        .get("transformers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(value_text).collect())
        .unwrap_or_default();
    let templated = transformers.iter().any(|x| x == "response-template");
    for t in transformers.iter().filter(|x| *x != "response-template") {
        w.push(format!(
            "{ctx}: response transformer `{t}` is not supported"
        ));
    }
    if resp_v.get("transformerParameters").is_some() {
        w.push(format!(
            "{ctx}: transformerParameters are not supported and were ignored"
        ));
    }
    let tpl = |s: &str, w: &mut Vec<String>| {
        if templated {
            warn_unsafe_helpers(s, ctx, w);
            convert_templates(s)
        } else {
            s.to_string()
        }
    };
    let mut resp = ResponseDef {
        status: resp_v.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        status_message: resp_v
            .get("statusMessage")
            .and_then(Value::as_str)
            .map(String::from),
        templated,
        ..Default::default()
    };
    if let Some(Value::Object(h)) = resp_v.get("headers") {
        for (k, v) in h {
            let val = match v {
                Value::Array(a) => {
                    crate::model::header_values::join(a.iter().map(|x| tpl(&value_text(x), w)))
                }
                other => tpl(&value_text(other), w),
            };
            resp.headers.insert(k.clone(), val);
        }
    }
    if let Some(b) = resp_v.get("body").and_then(Value::as_str) {
        resp.body = Some(tpl(b, w));
    } else if let Some(j) = resp_v.get("jsonBody") {
        resp.json_body = Some(if templated {
            warn_unsafe_helpers(&j.to_string(), ctx, w);
            convert_json_templates(j)
        } else {
            j.clone()
        });
    } else if let Some(b) = resp_v.get("base64Body").and_then(Value::as_str) {
        resp.base64_body = Some(b.to_string());
    } else if let Some(f) = resp_v.get("bodyFileName").and_then(Value::as_str) {
        resp.body_file_name = Some(tpl(f, w));
    }
    if let Some(d) = resp_v.get("fixedDelayMilliseconds").and_then(Value::as_u64) {
        resp.delay = Some(Delay {
            fixed_ms: Some(d),
            ..Default::default()
        });
    } else if let Some(d) = resp_v.get("delayDistribution") {
        resp.delay = delay_of(d, ctx, w);
    }
    if let Some(d) = resp_v.get("chunkedDribbleDelay") {
        resp.chunked_dribble_delay = Some(ChunkedDribbleDelay {
            number_of_chunks: d
                .get("numberOfChunks")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, u32::MAX as u64) as u32,
            total_duration_ms: d.get("totalDuration").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    if let Some(f) = resp_v.get("fault").and_then(Value::as_str) {
        resp.fault = match f {
            "CONNECTION_RESET_BY_PEER" => Some(Fault::ConnectionReset),
            "RANDOM_DATA_THEN_CLOSE" => Some(Fault::RandomDataThenClose),
            "EMPTY_RESPONSE" => Some(Fault::EmptyResponse),
            "MALFORMED_RESPONSE_CHUNK" => Some(Fault::MalformedResponseChunk),
            other => {
                w.push(format!("{ctx}: fault {other} is not supported"));
                None
            }
        };
    }
    if let Some(p) = resp_v.get("proxyBaseUrl").and_then(Value::as_str) {
        resp.proxy = Some(ProxyDef {
            base_url: p.to_string(),
            additional_headers: resp_v
                .get("additionalProxyRequestHeaders")
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| {
                            let blocked = crate::outbound::blocked_outbound_header(k);
                            if blocked {
                                w.push(format!(
                                    "{ctx}: additionalProxyRequestHeaders.{k} was dropped: it selects the target host or changes message framing"
                                ));
                            }
                            !blocked
                        })
                        .map(|(k, v)| (k.clone(), value_text(v)))
                        .collect()
                })
                .unwrap_or_default(),
            remove_headers: resp_v
                .get("removeProxyRequestHeaders")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(value_text).collect())
                .unwrap_or_default(),
            strip_prefix: resp_v
                .get("proxyUrlPrefixToRemove")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    resp
}

fn webhook(l: &Value, ctx: &str, w: &mut Vec<String>) -> Option<CallbackDef> {
    if l.get("name").and_then(Value::as_str) != Some("webhook") {
        w.push(format!(
            "{ctx}: serve event listener {} is not supported",
            l.get("name").unwrap_or(&Value::Null)
        ));
        return None;
    }
    let p = l.get("parameters").cloned().unwrap_or(json!({}));
    let mut headers = BTreeMap::new();
    if let Some(Value::Object(h)) = p.get("headers") {
        for (k, v) in h {
            let val = match v {
                Value::Array(a) => a.iter().map(value_text).collect::<Vec<_>>().join(", "),
                other => value_text(other),
            };
            headers.insert(k.clone(), convert_templates(&val));
        }
    }
    let body = match (p.get("body"), p.get("base64Body")) {
        (Some(b), _) => Some(convert_templates(&value_text(b))),
        (None, Some(_)) => {
            w.push(format!(
                "{ctx}: webhook base64Body is not supported; the callback is sent without a body"
            ));
            None
        }
        _ => None,
    };
    Some(CallbackDef {
        url: convert_templates(p.get("url").and_then(Value::as_str).unwrap_or("")),
        method: p
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("POST")
            .to_ascii_uppercase(),
        headers,
        body,
        json_body: None,
        delay: p.get("delay").and_then(|d| delay_of(d, ctx, w)),
        max_attempts: 3,
        correlation: None,
        hmac: None,
    })
}

pub fn convert_mapping(m: &Value, index: usize) -> (Option<Stub>, Vec<String>) {
    let mut w = Vec::new();
    let ctx = m
        .get("name")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("mapping #{index}"));
    let Some(r) = m.get("request").filter(|r| r.is_object()) else {
        return (
            None,
            vec![format!("{ctx}: no request; the mapping was skipped")],
        );
    };
    let request = match request_pattern(r, &ctx, &mut w) {
        Ok(r) => r,
        Err(e) => {
            w.push(format!("{ctx}: {e}; the mapping was skipped"));
            return (None, w);
        }
    };
    let response = response_def(
        &m.get("response").cloned().unwrap_or(json!({})),
        &ctx,
        &mut w,
    );
    let scenario = m
        .get("scenarioName")
        .and_then(Value::as_str)
        .map(|name| ScenarioRule {
            name: name.to_string(),
            required_state: m
                .get("requiredScenarioState")
                .and_then(Value::as_str)
                .map(String::from),
            new_state: m
                .get("newScenarioState")
                .and_then(Value::as_str)
                .map(String::from),
            ttl_seconds: None,
        });
    let listeners = m
        .get("serveEventListeners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            m.get("postServeActions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    let callbacks = listeners.filter_map(|l| webhook(l, &ctx, &mut w)).collect();
    if m.get("persistent").and_then(Value::as_bool) == Some(true) {
        w.push(format!(
            "{ctx}: persistent is implied; every FastMock stub is stored durably"
        ));
    }
    let id = m
        .get("id")
        .or_else(|| m.get("uuid"))
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::now_v7);
    let stub = Stub {
        id,
        name: m.get("name").and_then(Value::as_str).map(String::from),
        priority: m.get("priority").and_then(Value::as_i64).unwrap_or(5) as i32,
        request,
        response,
        scenario,
        callbacks,
        metadata: m.get("metadata").cloned().filter(|v| !v.is_null()),
        ..Default::default()
    };
    if let Err(e) = crate::matcher::CompiledStub::compile(&stub) {
        w.push(format!("{ctx}: {e}; the mapping was skipped"));
        return (None, w);
    }
    (Some(stub), w)
}

/// Base of the `order` values given to one import: later imports get larger bases.
pub fn order_base() -> i64 {
    chrono::Utc::now().timestamp_millis().saturating_mul(10_000)
}

/// Converts a document like WireMock's `POST /__admin/mappings/import`: mappings are added last to first,
/// so at equal priority the first mapping of the document wins, and a later import wins over an earlier one.
pub fn convert(doc: &Value) -> Converted {
    let mut out = Converted::default();
    let list: Vec<Value> = match doc {
        Value::Array(a) => a.clone(),
        Value::Object(o) if o.contains_key("mappings") => match &o["mappings"] {
            Value::Array(a) => a.clone(),
            _ => {
                out.warnings
                    .push("`mappings` must be an array of stub mappings".into());
                return out;
            }
        },
        Value::Object(o) if o.contains_key("request") => vec![doc.clone()],
        Value::Object(_) => {
            out.warnings.push(
                "the document is neither a stub mapping (with `request`), an array of mappings nor {\"mappings\": [...]}".into(),
            );
            return out;
        }
        _ => {
            out.warnings
                .push("the document must be a JSON object or array".into());
            return out;
        }
    };
    let base = order_base();
    let n = list.len() as i64;
    for (i, m) in list.iter().enumerate() {
        let (s, w) = convert_mapping(m, i);
        out.warnings.extend(w);
        if let Some(mut s) = s {
            s.order = Some(-(base + (n - 1 - i as i64)));
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

    #[test]
    fn converts_common_mappings() {
        let doc = json!({"mappings": [
            {"id": "8c5db8b0-2db4-4ad7-a99f-38c9b00da3f7", "name": "send", "priority": 2,
             "request": {"method": "POST", "urlPath": "/send", "headers": {"X-Key": {"equalTo": "k"}},
                         "bodyPatterns": [{"matchesJsonPath": "$.to"}, {"matchesJsonPath": {"expression": "$.type", "equalTo": "sms"}}, {"equalToJson": "{\"a\":1}", "ignoreExtraElements": true}]},
             "response": {"status": 202, "jsonBody": {"ok": true}, "fixedDelayMilliseconds": 50, "transformers": ["response-template"]},
             "serveEventListeners": [{"name": "webhook", "parameters": {"method": "POST", "url": "http://cb.example.com/{{jsonPath originalRequest.body '$.to'}}", "body": "x", "delay": {"type": "fixed", "milliseconds": 1000}}}],
             "scenarioName": "s1", "requiredScenarioState": "Started", "newScenarioState": "done"},
            {"request": {"url": "/q?x=1&y=2"}, "response": {"status": 200, "body": "q", "fault": "CONNECTION_RESET_BY_PEER"}},
            {"request": {"urlPattern": "/p/.*", "cookies": {"a": {"equalTo": "b"}}}, "response": {"bodyFileName": "x.json"}}
        ]});
        let c = convert(&doc);
        assert_eq!(c.stubs.len(), 3);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let s = &c.stubs[0];
        assert_eq!(s.id.to_string(), "8c5db8b0-2db4-4ad7-a99f-38c9b00da3f7");
        assert_eq!(s.priority, 2);
        assert_eq!(s.request.body.len(), 3);
        assert_eq!(s.response.delay.as_ref().unwrap().fixed_ms, Some(50));
        assert_eq!(s.callbacks[0].delay.as_ref().unwrap().fixed_ms, Some(1000));
        assert_eq!(
            s.scenario.as_ref().unwrap().new_state.as_deref(),
            Some("done")
        );
        let cs = CompiledStub::compile(s).unwrap();
        let body = json!({"to": "380", "type": "sms", "a": 1, "b": 2});
        let req = CapturedRequest::new(
            "POST",
            "/send",
            None,
            vec![("x-key".into(), "k".into())],
            Bytes::from(body.to_string()),
        );
        assert!(cs.match_request(&req, Some(&body)).is_some());
        let q = CompiledStub::compile(&c.stubs[1]).unwrap();
        assert!(
            q.match_request(
                &CapturedRequest::new("GET", "/q", Some("x=1&y=2"), vec![], Bytes::new()),
                None
            )
            .is_some()
        );
        assert!(
            q.match_request(
                &CapturedRequest::new("GET", "/q", Some("y=2&x=1"), vec![], Bytes::new()),
                None
            )
            .is_none(),
            "url is an exact match of path and query, as in WireMock"
        );
        assert_eq!(c.stubs[1].response.fault, Some(Fault::ConnectionReset));
        assert_eq!(c.stubs[2].request.url_pattern.as_deref(), Some("/p/.*"));
        assert_eq!(
            c.stubs[2].request.cookies["a"].equal_to.as_deref(),
            Some("b")
        );
        assert_eq!(
            c.stubs[2].response.body_file_name.as_deref(),
            Some("x.json")
        );
    }

    #[test]
    fn converts_advanced_matchers_and_responses() {
        let m = json!({
            "request": {
                "method": "POST", "urlPath": "/upload", "scheme": "https", "host": {"equalTo": "api.example.com"}, "port": 8443,
                "basicAuthCredentials": {"username": "u", "password": "p"},
                "formParameters": {"a": {"hasExactly": [{"equalTo": "1"}, {"equalTo": "2"}]}},
                "queryParameters": {"q": {"or": [{"absent": true}, {"and": [{"contains": "x"}, {"not": {"equalTo": "xy"}}]}]},
                                    "since": {"after": "now", "expectedOffset": -3, "expectedOffsetUnit": "days", "actualFormat": "epoch", "truncateActual": "first day of month"}},
                "headers": {"X-Ids": {"includes": [{"equalTo": "7"}]}},
                "multipartPatterns": [{"matchingType": "ALL", "headers": {"Content-Disposition": {"contains": "name=\"file\""}}, "bodyPatterns": [{"binaryEqualTo": "AQID"}]}],
                "bodyPatterns": [
                    {"equalToXml": "<a>${xmlunit.ignore}</a>", "enablePlaceholders": true, "exemptedComparisons": ["NAMESPACE_URI"]},
                    {"matchesXPath": {"expression": "//p:id", "equalTo": "1"}, "xPathNamespaces": {"p": "urn:p"}},
                    {"equalToJson": {"a": "${json-unit.any-number}"}, "ignoreArrayOrder": true},
                    {"doesNotContain": "secret"}
                ]
            },
            "response": {
                "status": 200, "statusMessage": "Okay", "bodyFileName": "files/{{request.path.[0]}}.json",
                "transformers": ["response-template"],
                "delayDistribution": {"type": "lognormal", "median": 80, "sigma": 0.4},
                "chunkedDribbleDelay": {"numberOfChunks": 5, "totalDuration": 1000}
            },
            "metadata": {"team": "billing"},
            "persistent": true
        });
        let (s, w) = convert_mapping(&m, 0);
        let s = s.unwrap();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("persistent"));
        let r = &s.request;
        assert_eq!(r.scheme.as_deref(), Some("https"));
        assert_eq!(r.port, Some(8443));
        assert_eq!(r.basic_auth.as_ref().unwrap().password, "p");
        assert_eq!(
            r.form_parameters["a"].has_exactly.as_ref().unwrap().len(),
            2
        );
        assert_eq!(r.query["q"].or.len(), 2);
        assert_eq!(r.query["since"].after.as_deref(), Some("now -3 days"));
        assert_eq!(
            r.query["since"].actual_format.as_deref(),
            Some("epochMillis")
        );
        assert!(r.query["since"].truncate_actual.is_some());
        assert_eq!(r.headers["x-ids"].includes.as_ref().unwrap().len(), 1);
        assert_eq!(r.multipart[0].name.as_deref(), Some("file"));
        assert_eq!(r.multipart_matching_type, Some(MultipartMatchingType::All));
        assert_eq!(r.body[1].xpath_namespaces["p"], "urn:p");
        assert!(r.body[2].ignore_array_order);
        assert!(r.body[3].negate);
        let resp = &s.response;
        assert_eq!(resp.status_message.as_deref(), Some("Okay"));
        assert_eq!(
            resp.body_file_name.as_deref(),
            Some("files/{{request.pathSegments.[0]}}.json")
        );
        assert_eq!(
            resp.delay
                .as_ref()
                .unwrap()
                .lognormal
                .as_ref()
                .unwrap()
                .median_ms,
            80
        );
        assert_eq!(
            resp.chunked_dribble_delay
                .as_ref()
                .unwrap()
                .number_of_chunks,
            5
        );
        assert_eq!(s.metadata.as_ref().unwrap()["team"], "billing");
        CompiledStub::compile(&s).unwrap();
    }

    #[test]
    fn faults_templates_and_unsupported_parts() {
        for (f, expected) in [
            ("RANDOM_DATA_THEN_CLOSE", Fault::RandomDataThenClose),
            ("MALFORMED_RESPONSE_CHUNK", Fault::MalformedResponseChunk),
            ("EMPTY_RESPONSE", Fault::EmptyResponse),
        ] {
            let (s, w) = convert_mapping(&json!({"request": {}, "response": {"fault": f}}), 0);
            assert!(w.is_empty());
            assert_eq!(s.unwrap().response.fault, Some(expected));
        }
        assert_eq!(
            convert_templates(
                "{{request.headers.X-Request-Id}} {{request.headers.[Content-Type]}} {{request.query.ids.[1]}} {{upper request.path.[0]}} {{now format='epoch'}} {{request.requestLine.path}}"
            ),
            "{{request.headers.[x-request-id]}} {{request.headers.[content-type]}} {{request.queryAll.ids.[1]}} {{upper request.pathSegments.[0]}} {{now format='epochMillis'}} {{request.path}}"
        );
        let (_, w) = convert_mapping(
            &json!({"request": {"customMatcher": {"name": "x"}}, "response": {"body": "{{systemValue key='HOME'}}", "transformers": ["response-template", "custom"]}}),
            0,
        );
        assert!(w.iter().any(|x| x.contains("customMatcher")));
        assert!(w.iter().any(|x| x.contains("systemValue")));
        assert!(w.iter().any(|x| x.contains("custom")));
    }
}
