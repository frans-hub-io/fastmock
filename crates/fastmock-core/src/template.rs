use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{SecondsFormat, Utc};
use handlebars::{
    Context, Handlebars, Helper, HelperDef, HelperResult, JsonRender, Output, RenderContext,
    RenderError, RenderErrorReason, ScopedJson,
};
use hmac::{Hmac, Mac};
use serde_json::Value;
use serde_json_path::JsonPath;
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;

use crate::matcher::value_text;

pub const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INTERMEDIATE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RENDER_STEPS: usize = 1_000_000;
pub const MAX_RENDER_TIME: std::time::Duration = std::time::Duration::from_secs(2);
pub const MAX_BLOCK_NESTING: usize = 64;
pub const MAX_SUBEXPRESSION_NESTING: usize = 32;
const TEMPLATE_CACHE_ENTRIES: usize = 4096;
const DEADLINE_CHECK_EVERY: usize = 256;

thread_local! {
    static BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_INTERMEDIATE_BYTES) };
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_RENDER_STEPS) };
    static DEADLINE: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

fn reset_limits() {
    BUDGET.with(|b| b.set(MAX_INTERMEDIATE_BYTES));
    STEPS.with(|s| s.set(MAX_RENDER_STEPS));
    DEADLINE.with(|d| d.set(Some(std::time::Instant::now() + MAX_RENDER_TIME)));
}

fn tick(n: usize) -> Result<(), RenderError> {
    let left = STEPS.with(|s| {
        let left = s.get().saturating_sub(n);
        let crossed = s.get() / DEADLINE_CHECK_EVERY != left / DEADLINE_CHECK_EVERY;
        s.set(left);
        (left, crossed)
    });
    if left.0 == 0 {
        return Err(other("template exceeds the step limit"));
    }
    if left.1
        && DEADLINE
            .with(|d| d.get())
            .is_some_and(|t| std::time::Instant::now() > t)
    {
        STEPS.with(|s| s.set(0));
        return Err(other("template exceeds the time limit"));
    }
    Ok(())
}

fn budget_left() -> usize {
    BUDGET.with(|b| b.get())
}

fn ensure_fits(n: usize) -> Result<(), RenderError> {
    if n > budget_left() {
        BUDGET.with(|b| b.set(0));
        return Err(other(
            "template intermediate values exceed the memory budget",
        ));
    }
    Ok(())
}

struct CappedWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.cap {
            return Err(std::io::Error::other("budget"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CountingWriter {
    n: usize,
    cap: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.n += data.len();
        if self.n > self.cap {
            return Err(std::io::Error::other("budget"));
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_string(v: &Value, pretty: bool) -> Result<String, RenderError> {
    let mut w = CappedWriter {
        buf: Vec::new(),
        cap: budget_left(),
    };
    let r = if pretty {
        serde_json::to_writer_pretty(&mut w, v)
    } else {
        serde_json::to_writer(&mut w, v)
    };
    if r.is_err() {
        return Err(ensure_fits(usize::MAX).unwrap_err());
    }
    String::from_utf8(w.buf).map_err(|e| other(e.to_string()))
}

fn text_len(v: &Value, cap: usize) -> usize {
    match v {
        Value::String(s) => s.len(),
        other => {
            let mut w = CountingWriter { n: 0, cap };
            let _ = serde_json::to_writer(&mut w, other);
            w.n
        }
    }
}

fn bounded_text(v: &Value) -> Result<String, RenderError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => json_string(other, false),
    }
}

fn parse_json_text(s: &str) -> Result<Option<Value>, RenderError> {
    match crate::request::parse_json_bounded(s.as_bytes()) {
        Some(v) => {
            charge(&v)?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

fn check_nesting(tpl: &str) -> Result<(), TemplateError> {
    let mut blocks = 0usize;
    let mut rest = tpl;
    while let Some(start) = rest.find("{{") {
        let tag = &rest[start + 2..];
        let end = tag.find("}}").unwrap_or(tag.len());
        let body = tag[..end].trim_start_matches(['{', '~']).trim_start();
        if body.starts_with('!') {
            let close = if body.starts_with("!--") {
                "--}}"
            } else {
                "}}"
            };
            let skip = tag
                .find(close)
                .map(|i| i + close.len())
                .unwrap_or(tag.len());
            rest = &tag[skip..];
            continue;
        }
        match body.chars().next() {
            Some('#') | Some('^') if body.len() > 1 && !body[1..].trim_start().starts_with('}') => {
                blocks += 1;
            }
            Some('/') => blocks = blocks.saturating_sub(1),
            _ => {}
        }
        if blocks > MAX_BLOCK_NESTING {
            return Err(TemplateError::Render(format!(
                "blocks are nested deeper than {MAX_BLOCK_NESTING} levels"
            )));
        }
        let mut parens = 0usize;
        for c in body[..body.len().min(end)].chars() {
            match c {
                '(' => {
                    parens += 1;
                    if parens > MAX_SUBEXPRESSION_NESTING {
                        return Err(TemplateError::Render(format!(
                            "subexpressions are nested deeper than {MAX_SUBEXPRESSION_NESTING} levels"
                        )));
                    }
                }
                ')' => parens = parens.saturating_sub(1),
                _ => {}
            }
        }
        rest = &tag[end.min(tag.len())..];
    }
    Ok(())
}

fn approx_size(v: &Value, cap: usize) -> usize {
    match v {
        Value::String(s) => s.len() + 8,
        Value::Array(a) => {
            let mut n = 16;
            for x in a {
                n += approx_size(x, cap);
                if n > cap {
                    break;
                }
            }
            n
        }
        Value::Object(o) => {
            let mut n = 16;
            for (k, x) in o {
                n += k.len() + approx_size(x, cap);
                if n > cap {
                    break;
                }
            }
            n
        }
        _ => 8,
    }
}

fn charge(v: &Value) -> Result<(), RenderError> {
    BUDGET.with(|b| {
        let left = b.get();
        let size = approx_size(v, left + 1);
        if size > left {
            b.set(0);
            Err(RenderErrorReason::Other(
                "template intermediate values exceed the memory budget".into(),
            )
            .into())
        } else {
            b.set(left - size);
            Ok(())
        }
    })
}

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("template too large ({0} bytes)")]
    TooLarge(usize),
    #[error("rendered output too large")]
    OutputTooLarge,
    #[error("template error: {0}")]
    Render(String),
}

pub struct Templates {
    hb: Handlebars<'static>,
    cache:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<handlebars::Template>>>,
}

pub use handlebars::Context as TemplateContext;

fn reject_partials(tpl: &str) -> Result<(), TemplateError> {
    static PARTIAL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\{\{\{?~?\s*(?:>|#\s*>|#\s*\*)").unwrap());
    if PARTIAL.is_match(tpl) {
        return Err(TemplateError::Render(
            "partials and decorators are not supported".into(),
        ));
    }
    Ok(())
}

pub fn context(v: Value) -> TemplateContext {
    TemplateContext::from(v)
}

impl Default for Templates {
    fn default() -> Self {
        Self::new()
    }
}

fn param_str(h: &Helper, i: usize) -> Option<String> {
    h.param(i).map(|p| value_text(p.value()))
}

fn req_str(h: &Helper, i: usize, name: &'static str) -> Result<String, RenderError> {
    param_str(h, i).ok_or_else(|| RenderErrorReason::ParamNotFoundForIndex(name, i).into())
}

fn hash_str(h: &Helper, key: &str) -> Option<String> {
    h.hash_get(key).map(|v| value_text(v.value()))
}

macro_rules! value_helper {
    ($name:ident, |$h:ident| $body:block) => {
        struct $name;
        impl HelperDef for $name {
            fn call_inner<'reg: 'rc, 'rc>(
                &self,
                $h: &Helper<'rc>,
                _: &'reg Handlebars<'reg>,
                _: &'rc Context,
                _: &mut RenderContext<'reg, 'rc>,
            ) -> Result<ScopedJson<'rc>, RenderError> {
                tick(1)?;
                let v: Value = $body;
                charge(&v)?;
                Ok(ScopedJson::Derived(v))
            }
        }
    };
}

value_helper!(JsonPathHelper, |h| {
    let path = req_str(h, 1, "jsonPath")?;
    let jp = JsonPath::parse(&path)
        .map_err(|e| RenderErrorReason::Other(format!("invalid JSONPath {path}: {e}")))?;
    let first = |data: &Value| jp.query(data).first().cloned().unwrap_or(Value::Null);
    match h.param(0).map(|p| p.value()) {
        Some(Value::String(s)) => match parse_json_text(s)? {
            Some(parsed) => first(&parsed),
            None => first(&Value::String(s.clone())),
        },
        Some(data) => first(data),
        None => first(&Value::Null),
    }
});

/// Writes objects and arrays as JSON text when the helper is rendered directly (WireMock's `jsonPath`);
/// sub-expressions and block helpers still receive the JSON value.
struct JsonText<H>(H);

impl<H: HelperDef> HelperDef for JsonText<H> {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
    ) -> Result<ScopedJson<'rc>, RenderError> {
        self.0.call_inner(h, r, ctx, rc)
    }

    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        let v = self.0.call_inner(h, r, ctx, rc)?;
        let text = match v.as_json() {
            j @ (Value::Object(_) | Value::Array(_)) => j.to_string(),
            j => j.render(),
        };
        out.write(&text)?;
        Ok(())
    }
}

value_helper!(UuidHelper, |_h| {
    Value::String(uuid::Uuid::new_v4().to_string())
});

value_helper!(NowHelper, |h| {
    let now = Utc::now();
    let offset = hash_str(h, "offsetSeconds")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let t = chrono::TimeDelta::try_seconds(offset)
        .and_then(|d| now.checked_add_signed(d))
        .ok_or_else(|| {
            RenderErrorReason::Other(format!("now: offsetSeconds {offset} is out of range"))
        })?;
    shifted(h, t, "now")?
});

value_helper!(RandomIntHelper, |h| {
    let lo = param_str(h, 0)
        .or_else(|| hash_str(h, "lower"))
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let hi = param_str(h, 1)
        .or_else(|| hash_str(h, "upper"))
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(i32::MAX as i64);
    if hi < lo {
        return Err(RenderErrorReason::Other("randomInt: upper < lower".into()).into());
    }
    Value::from(rand::random_range(lo..=hi))
});

value_helper!(RandomDigitsHelper, |h| {
    let n = param_str(h, 0)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8)
        .min(256);
    Value::String(
        (0..n)
            .map(|_| char::from(b'0' + rand::random_range(0..10u8)))
            .collect(),
    )
});

value_helper!(PickRandomHelper, |h| {
    let n = h.params().len();
    if let (1, Some(Value::Array(a))) = (n, h.param(0).map(|p| p.value())) {
        if a.is_empty() {
            Value::Null
        } else {
            a[rand::random_range(0..a.len())].clone()
        }
    } else if n == 0 {
        Value::Null
    } else {
        h.params()[rand::random_range(0..n)].value().clone()
    }
});

value_helper!(Base64Helper, |h| {
    let s = req_str(h, 0, "base64")?;
    ensure_fits((s.len() / 3 + 1).saturating_mul(4).saturating_add(8))?;
    if hash_bool(h, "decode") {
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(s.trim().trim_end_matches('='))
            .map_err(|e| RenderErrorReason::Other(format!("base64: {e}")))?;
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    } else if h.hash_get("padding").is_some_and(|v| !truthy(v.value())) {
        Value::String(base64::engine::general_purpose::STANDARD_NO_PAD.encode(s))
    } else {
        Value::String(B64.encode(s))
    }
});

value_helper!(Base64DecodeHelper, |h| {
    let s = req_str(h, 0, "base64Decode")?;
    let bytes = B64
        .decode(s.trim())
        .map_err(|e| RenderErrorReason::Other(format!("base64Decode: {e}")))?;
    Value::String(String::from_utf8_lossy(&bytes).into_owned())
});

value_helper!(Sha256Helper, |h| {
    Value::String(hex::encode(Sha256::digest(
        req_str(h, 0, "sha256")?.as_bytes(),
    )))
});

value_helper!(HmacHelper, |h| {
    let algo = req_str(h, 0, "hmac")?;
    let key = req_str(h, 1, "hmac")?;
    let data = req_str(h, 2, "hmac")?;
    let enc = hash_str(h, "encoding").unwrap_or_else(|| "hex".into());
    let raw = hmac_bytes(&algo, key.as_bytes(), data.as_bytes())
        .map_err(|e| RenderErrorReason::Other(e.to_string()))?;
    Value::String(if enc == "base64" {
        B64.encode(raw)
    } else {
        hex::encode(raw)
    })
});

value_helper!(UpperHelper, |h| {
    Value::String(param_str(h, 0).unwrap_or_default().to_uppercase())
});
value_helper!(LowerHelper, |h| {
    Value::String(param_str(h, 0).unwrap_or_default().to_lowercase())
});

value_helper!(DefaultHelper, |h| {
    let v = h.param(0).map(|p| p.value().clone()).unwrap_or(Value::Null);
    let empty = matches!(&v, Value::Null) || matches!(&v, Value::String(s) if s.is_empty());
    if empty {
        h.param(1).map(|p| p.value().clone()).unwrap_or(Value::Null)
    } else {
        v
    }
});

value_helper!(ConcatHelper, |h| {
    let left = budget_left();
    let mut total = 0usize;
    for p in h.params() {
        total = total.saturating_add(text_len(p.value(), left.saturating_add(1)));
        ensure_fits(total)?;
    }
    let mut out = String::with_capacity(total);
    for p in h.params() {
        out.push_str(&bounded_text(p.value())?);
    }
    Value::String(out)
});

value_helper!(SubstringHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let len = s.chars().count();
    let start = param_str(h, 1)
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(0)
        .min(len);
    let end = param_str(h, 2)
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(len)
        .clamp(start, len);
    Value::String(s.chars().skip(start).take(end - start).collect())
});

value_helper!(RegexExtractHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let pat = req_str(h, 1, "regexExtract")?;
    let group = param_str(h, 2)
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(0);
    let re =
        cached_regex(&pat).map_err(|e| RenderErrorReason::Other(format!("regexExtract: {e}")))?;
    re.captures(&s)
        .and_then(|c| c.get(group))
        .map(|m| Value::String(m.as_str().to_string()))
        .unwrap_or(Value::Null)
});

value_helper!(ToJsonHelper, |h| {
    Value::String(match h.param(0) {
        Some(p) => json_string(p.value(), false)?,
        None => "null".into(),
    })
});

value_helper!(ParseJsonHelper, |h| {
    match h.param(0).map(|p| p.value()) {
        Some(Value::String(s)) => parse_json_text(s)?.unwrap_or(Value::Null),
        Some(other) => {
            let s = bounded_text(other)?;
            parse_json_text(&s)?.unwrap_or(Value::Null)
        }
        None => Value::Null,
    }
});

value_helper!(SizeHelper, |h| {
    match h.param(0).map(|p| p.value()) {
        Some(Value::Array(a)) => Value::from(a.len()),
        Some(Value::Object(o)) => Value::from(o.len()),
        Some(Value::String(s)) => Value::from(s.chars().count()),
        _ => Value::from(0),
    }
});

value_helper!(MathHelper, |h| {
    let a = param_str(h, 0)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let op = req_str(h, 1, "math")?;
    let b = param_str(h, 2)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let r = match op.as_str() {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" if b != 0.0 => a / b,
        "%" if b != 0.0 => a % b,
        "^" | "pow" => a.powf(b),
        "min" => a.min(b),
        "max" => a.max(b),
        "//" if b != 0.0 => (a / b).floor(),
        _ => return Err(RenderErrorReason::Other(format!("math: unsupported `{op}`")).into()),
    };
    if !r.is_finite() {
        return Err(RenderErrorReason::Other("math: result is not a finite number".into()).into());
    }
    number_value(r)
});

const MAX_HELPER_ITEMS: usize = 10_000;
const MAX_RANDOM_LENGTH: usize = 4096;
const MAX_FORMAT_DEPTH: usize = 256;

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn hash_bool(h: &Helper, key: &str) -> bool {
    h.hash_get(key).is_some_and(|v| truthy(v.value()))
}

fn hash_num(h: &Helper, key: &str) -> Option<f64> {
    h.hash_get(key).and_then(|v| as_f64(v.value()))
}

fn json_arg(v: Option<&Value>) -> Result<Value, RenderError> {
    Ok(match v {
        Some(Value::String(s)) => match parse_json_text(s)? {
            Some(parsed) => parsed,
            None => Value::String(s.clone()),
        },
        Some(other) => {
            charge(other)?;
            other.clone()
        }
        None => Value::Null,
    })
}

fn other(msg: impl Into<String>) -> RenderError {
    RenderErrorReason::Other(msg.into()).into()
}

fn number_value(r: f64) -> Value {
    if r.fract() == 0.0 && r.abs() < 9.0e15 {
        Value::from(r as i64)
    } else {
        Value::from(r)
    }
}

fn to_time(v: &Value) -> Option<chrono::DateTime<Utc>> {
    use chrono::TimeZone;
    match v {
        Value::Number(n) => {
            let x = n.as_i64()?;
            if x.abs() > 100_000_000_000 {
                Utc.timestamp_millis_opt(x).single()
            } else {
                Utc.timestamp_opt(x, 0).single()
            }
        }
        Value::String(s) => crate::datetime::parse_datetime(s),
        _ => None,
    }
}

fn shifted(h: &Helper, mut t: chrono::DateTime<Utc>, name: &str) -> Result<Value, RenderError> {
    if let Some(off) = hash_str(h, "offset") {
        let (a, u) =
            crate::datetime::parse_offset(&off).map_err(|e| other(format!("{name}: {e}")))?;
        t = crate::datetime::apply_offset(t, a, u)
            .ok_or_else(|| other(format!("{name}: offset `{off}` is out of range")))?;
    }
    let tz = match hash_str(h, "timezone") {
        Some(z) => Some(crate::datetime::parse_timezone(&z).ok_or_else(|| {
            other(format!(
                "{name}: timezone `{z}` is not supported; use UTC or a fixed offset such as +02:00"
            ))
        })?),
        None => None,
    };
    let format = hash_str(h, "format");
    Ok(match format.as_deref() {
        Some("epoch") | Some("unix") => Value::from(t.timestamp()),
        Some("epochMillis") => Value::from(t.timestamp_millis()),
        f => Value::String(
            crate::datetime::format(t, f, tz).map_err(|e| other(format!("{name}: {e}")))?,
        ),
    })
}

value_helper!(DateHelper, |h| {
    let v = h.param(0).map(|p| p.value().clone()).unwrap_or(Value::Null);
    let t = to_time(&v).ok_or_else(|| other("date: the first argument is not a date"))?;
    shifted(h, t, "date")?
});

value_helper!(ParseDateHelper, |h| {
    let s = req_str(h, 0, "parseDate")?;
    let t = match hash_str(h, "format") {
        Some(f) => crate::datetime::parse_with_format(&s, &f),
        None => crate::datetime::parse_datetime(&s),
    };
    match t {
        Some(t) => Value::String(t.to_rfc3339_opts(SecondsFormat::Millis, true)),
        None => Value::Null,
    }
});

value_helper!(DateFormatHelper, |h| {
    let v = h.param(0).map(|p| p.value().clone()).unwrap_or(Value::Null);
    let t = to_time(&v).ok_or_else(|| other("dateFormat: the first argument is not a date"))?;
    let f = param_str(h, 1)
        .or_else(|| hash_str(h, "format"))
        .unwrap_or_else(|| "iso".into());
    let f = match f.as_str() {
        "full" => "EEEE, MMMM d, yyyy".to_string(),
        "long" => "MMMM d, yyyy".to_string(),
        "medium" => "MMM d, yyyy".to_string(),
        "short" => "M/d/yy".to_string(),
        _ => f,
    };
    let tz = hash_str(h, "timezone").and_then(|z| crate::datetime::parse_timezone(&z));
    Value::String(
        crate::datetime::format(t, Some(&f), tz).map_err(|e| other(format!("dateFormat: {e}")))?,
    )
});

value_helper!(TrimHelper, |h| {
    Value::String(param_str(h, 0).unwrap_or_default().trim().to_string())
});

value_helper!(SplitHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let sep = param_str(h, 1).unwrap_or_else(|| ",".into());
    if sep.is_empty() {
        return Err(other("split: the separator must not be empty"));
    }
    let estimate = s
        .split(sep.as_str())
        .take(MAX_HELPER_ITEMS)
        .fold(16usize, |n, x| n.saturating_add(x.len() + 8));
    ensure_fits(estimate)?;
    Value::Array(
        s.split(sep.as_str())
            .take(MAX_HELPER_ITEMS)
            .map(|x| Value::String(x.to_string()))
            .collect(),
    )
});

value_helper!(JoinHelper, |h| {
    let sep = param_str(h, 0).unwrap_or_default();
    let items: Vec<&Value> = match h.params().get(1).map(|p| p.value()) {
        Some(Value::Array(a)) if h.params().len() == 2 => a.iter().take(MAX_HELPER_ITEMS).collect(),
        _ => h.params().iter().skip(1).map(|p| p.value()).collect(),
    };
    let prefix = hash_str(h, "prefix").unwrap_or_default();
    let suffix = hash_str(h, "suffix").unwrap_or_default();
    let left = budget_left().saturating_add(1);
    let mut total = prefix.len() + suffix.len();
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            total = total.saturating_add(sep.len());
        }
        total = total.saturating_add(text_len(v, left));
        ensure_fits(total)?;
    }
    tick(items.len())?;
    let mut out = String::with_capacity(total);
    out.push_str(&prefix);
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        out.push_str(&bounded_text(v)?);
    }
    out.push_str(&suffix);
    Value::String(out)
});

value_helper!(ReplaceHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let from = req_str(h, 1, "replace")?;
    let to = param_str(h, 2).unwrap_or_default();
    if from.is_empty() {
        Value::String(s)
    } else {
        let hits = s.matches(from.as_str()).count();
        let size = (s.len() - hits * from.len()).saturating_add(hits.saturating_mul(to.len()));
        ensure_fits(size.saturating_add(8))?;
        Value::String(s.replace(from.as_str(), &to))
    }
});

value_helper!(CapitalizeHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let fully = hash_bool(h, "fully");
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_whitespace() {
            start = true;
            out.push(c);
        } else if start {
            out.extend(c.to_uppercase());
            start = false;
        } else if fully {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    Value::String(out)
});

value_helper!(CapitalizeFirstHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let mut c = s.chars();
    Value::String(match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    })
});

value_helper!(AbbreviateHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let n = param_str(h, 1)
        .and_then(|x| x.parse::<usize>().ok())
        .ok_or_else(|| other("abbreviate: the width must be a number"))?;
    if n < 4 {
        return Err(other("abbreviate: the width must be at least 4"));
    }
    Value::String(if s.chars().count() <= n {
        s
    } else {
        format!("{}...", s.chars().take(n - 3).collect::<String>())
    })
});

value_helper!(UrlEncodeHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    Value::String(if hash_bool(h, "decode") {
        crate::request::parse_query(&format!("v={s}"))
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or_default()
    } else {
        ensure_fits(s.len().saturating_mul(3).saturating_add(8))?;
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b' ' => out.push('+'),
                b if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'*') => {
                    out.push(b as char)
                }
                b => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    })
});

value_helper!(ArrayHelper, |h| {
    let left = budget_left().saturating_add(1);
    let mut total = 16usize;
    for p in h.params() {
        total = total.saturating_add(approx_size(p.value(), left));
        ensure_fits(total)?;
    }
    Value::Array(h.params().iter().map(|p| p.value().clone()).collect())
});

value_helper!(ValHelper, |h| {
    let v = h.param(0).map(|p| p.value().clone()).unwrap_or(Value::Null);
    if v.is_null() {
        h.hash_get("default")
            .or_else(|| h.hash_get("or"))
            .map(|x| x.value().clone())
            .unwrap_or(Value::Null)
    } else {
        v
    }
});

value_helper!(RandomValueHelper, |h| {
    let kind = hash_str(h, "type")
        .unwrap_or_else(|| "ALPHANUMERIC".into())
        .to_ascii_uppercase();
    let len = hash_num(h, "length").unwrap_or(36.0).max(0.0) as usize;
    if len > MAX_RANDOM_LENGTH {
        return Err(other(format!(
            "randomValue: length is limited to {MAX_RANDOM_LENGTH}"
        )));
    }
    let alphabet: &[u8] = match kind.as_str() {
        "UUID" => return Ok(ScopedJson::Derived(Value::String(uuid::Uuid::new_v4().to_string()))),
        "ALPHANUMERIC" => b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
        "ALPHABETIC" => b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        "NUMERIC" => b"0123456789",
        "HEXADECIMAL" => b"0123456789abcdef",
        "ALPHANUMERIC_AND_SYMBOLS" => {
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&()*+,-./:;<=>?@[]^_{|}~"
        }
        other_kind => return Err(other(format!("randomValue: unsupported type `{other_kind}`"))),
    };
    let s: String = (0..len)
        .map(|_| char::from(alphabet[rand::random_range(0..alphabet.len())]))
        .collect();
    Value::String(if hash_bool(h, "uppercase") {
        s.to_uppercase()
    } else {
        s
    })
});

value_helper!(RandomDecimalHelper, |h| {
    let lo = hash_num(h, "lower").unwrap_or(0.0);
    let hi = hash_num(h, "upper").unwrap_or(1.0);
    if hi < lo || !lo.is_finite() || !hi.is_finite() {
        return Err(other("randomDecimal: upper must not be less than lower"));
    }
    Value::from(if hi == lo {
        lo
    } else {
        rand::random_range(lo..hi)
    })
});

fn deep_merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in y {
                match x.get_mut(&k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        x.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn strip_nulls(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.retain(|_, x| !x.is_null());
            o.values_mut().for_each(strip_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

value_helper!(JsonMergeHelper, |h| {
    let mut a = json_arg(h.param(0).map(|p| p.value()))?;
    let b = json_arg(h.param(1).map(|p| p.value()))?;
    if !a.is_object() || !b.is_object() {
        return Err(other("jsonMerge: both arguments must be JSON objects"));
    }
    deep_merge(&mut a, b);
    if hash_bool(h, "removeNulls") {
        strip_nulls(&mut a);
    }
    Value::String(json_string(&a, false)?)
});

fn pointer_tokens(p: &str) -> Vec<String> {
    p.split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn remove_at(root: &mut Value, tokens: &[String]) {
    let Some((last, parent)) = tokens.split_last() else {
        return;
    };
    let mut cur = root;
    for t in parent {
        cur = match cur {
            Value::Object(o) => match o.get_mut(t) {
                Some(x) => x,
                None => return,
            },
            Value::Array(a) => match t.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                Some(x) => x,
                None => return,
            },
            _ => return,
        };
    }
    match cur {
        Value::Object(o) => {
            o.shift_remove(last);
        }
        Value::Array(a) => {
            if let Some(i) = last.parse::<usize>().ok().filter(|i| *i < a.len()) {
                a.remove(i);
            }
        }
        _ => {}
    }
}

value_helper!(JsonRemoveHelper, |h| {
    let mut doc = json_arg(h.param(0).map(|p| p.value()))?;
    let path = req_str(h, 1, "jsonRemove")?;
    let jp = JsonPath::parse(&path)
        .map_err(|e| other(format!("jsonRemove: invalid JSONPath {path}: {e}")))?;
    let mut targets: Vec<Vec<String>> = jp
        .query_located(&doc)
        .locations()
        .map(|l| pointer_tokens(&l.to_json_pointer()))
        .filter(|t| !t.is_empty())
        .collect();
    targets.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b) {
            let o = match (x.parse::<usize>(), y.parse::<usize>()) {
                (Ok(i), Ok(j)) => i.cmp(&j),
                _ => x.cmp(y),
            };
            if o != std::cmp::Ordering::Equal {
                return o.reverse();
            }
        }
        b.len().cmp(&a.len())
    });
    for t in &targets {
        remove_at(&mut doc, t);
    }
    Value::String(json_string(&doc, false)?)
});

value_helper!(JsonArrayAddHelper, |h| {
    let arr = json_arg(h.param(0).map(|p| p.value()))?;
    let item = json_arg(h.param(1).map(|p| p.value()))?;
    let Value::Array(mut a) = arr else {
        return Err(other(
            "jsonArrayAdd: the first argument must be a JSON array",
        ));
    };
    match item {
        Value::Array(items) if hash_bool(h, "flatten") => a.extend(items),
        x => a.push(x),
    }
    if let Some(max) = hash_num(h, "maxItems").filter(|m| *m >= 0.0) {
        let max = max as usize;
        if a.len() > max {
            a.drain(..a.len() - max);
        }
    }
    if a.len() > MAX_HELPER_ITEMS {
        return Err(other("jsonArrayAdd: too many items"));
    }
    Value::String(json_string(&Value::Array(a), false)?)
});

value_helper!(FormatJsonHelper, |h| {
    let v = json_arg(h.param(0).map(|p| p.value()))?;
    let compact = hash_str(h, "format").as_deref() == Some("compact");
    Value::String(json_string(&v, !compact)?)
});

fn xml_escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn qname(n: roxmltree::Node, ns: Option<&str>, local: &str) -> String {
    match ns.and_then(|u| n.lookup_prefix(u)) {
        Some(p) if !p.is_empty() => format!("{p}:{local}"),
        _ => local.to_string(),
    }
}

fn write_xml(
    n: roxmltree::Node,
    out: &mut String,
    depth: usize,
    pretty: bool,
    cap: usize,
) -> Result<(), String> {
    if depth > MAX_FORMAT_DEPTH {
        return Err("XML is nested too deeply".into());
    }
    if out.len() > cap {
        return Err("formatted XML is too large".into());
    }
    let indent = |out: &mut String, d: usize| {
        if pretty {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&"  ".repeat(d));
        }
    };
    if n.is_text() {
        let t = n.text().unwrap_or("").trim();
        if !t.is_empty() {
            out.push_str(&xml_escape(t, false));
        }
        return Ok(());
    }
    if n.is_comment() {
        indent(out, depth);
        out.push_str(&format!("<!--{}-->", n.text().unwrap_or("")));
        return Ok(());
    }
    if !n.is_element() {
        return Ok(());
    }
    indent(out, depth);
    let name = qname(n, n.tag_name().namespace(), n.tag_name().name());
    out.push('<');
    out.push_str(&name);
    let parent_ns: Vec<(Option<&str>, &str)> = n
        .parent_element()
        .map(|p| p.namespaces().map(|x| (x.name(), x.uri())).collect())
        .unwrap_or_default();
    for ns in n.namespaces() {
        if parent_ns.contains(&(ns.name(), ns.uri())) || ns.name() == Some("xml") {
            continue;
        }
        match ns.name() {
            Some(p) => out.push_str(&format!(" xmlns:{p}=\"{}\"", xml_escape(ns.uri(), true))),
            None => out.push_str(&format!(" xmlns=\"{}\"", xml_escape(ns.uri(), true))),
        }
    }
    for a in n.attributes() {
        let an = match a.namespace() {
            Some(u) => qname(n, Some(u), a.name()),
            None => a.name().to_string(),
        };
        out.push_str(&format!(" {an}=\"{}\"", xml_escape(a.value(), true)));
    }
    let children: Vec<roxmltree::Node> = n
        .children()
        .filter(|c| {
            c.is_element()
                || c.is_comment()
                || (c.is_text() && !c.text().unwrap_or("").trim().is_empty())
        })
        .collect();
    if children.is_empty() {
        out.push_str("/>");
        return Ok(());
    }
    out.push('>');
    let only_text = children.iter().all(|c| c.is_text());
    for c in &children {
        write_xml(*c, out, depth + 1, pretty && !only_text, cap)?;
    }
    if !only_text {
        indent(out, depth);
    }
    out.push_str(&format!("</{name}>"));
    Ok(())
}

pub fn format_xml(xml: &str, pretty: bool) -> Result<String, String> {
    format_xml_capped(xml, pretty, MAX_OUTPUT_BYTES)
}

fn format_xml_capped(xml: &str, pretty: bool, cap: usize) -> Result<String, String> {
    let opts = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: crate::xml::MAX_XML_NODES,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, opts).map_err(|e| e.to_string())?;
    let mut out = String::new();
    write_xml(doc.root_element(), &mut out, 0, pretty, cap)?;
    if out.len() > cap {
        return Err("formatted XML is too large".into());
    }
    Ok(out)
}

value_helper!(FormatXmlHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let compact = hash_str(h, "format").as_deref() == Some("compact");
    let cap = budget_left().min(MAX_OUTPUT_BYTES);
    Value::String(
        format_xml_capped(&s, !compact, cap).map_err(|e| other(format!("formatXml: {e}")))?,
    )
});

value_helper!(XPathHelper, |h| {
    let s = param_str(h, 0).unwrap_or_default();
    let expr = req_str(h, 1, "xPath")?;
    let xp =
        crate::xml::parse(&expr).map_err(|e| other(format!("xPath: invalid XPath {expr}: {e}")))?;
    match xp.select(&s) {
        None => Value::Null,
        Some(mut v) if v.len() <= 1 => v.pop().map(Value::String).unwrap_or(Value::Null),
        Some(v) => Value::Array(v.into_iter().map(Value::String).collect()),
    }
});

fn form_value(h: &Helper) -> Value {
    let s = param_str(h, 0).unwrap_or_default();
    let decode = hash_bool(h, "urlDecode");
    let mut m = serde_json::Map::new();
    for pair in s
        .split('&')
        .filter(|p| !p.is_empty())
        .take(MAX_HELPER_ITEMS)
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = if decode {
            crate::request::parse_query(pair)
                .into_iter()
                .next()
                .unwrap_or_default()
        } else {
            (k.to_string(), v.to_string())
        };
        m.entry(k).or_insert(Value::String(v));
    }
    Value::Object(m)
}

fn valid_variable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(name, "request" | "originalRequest" | "state")
}

struct FormDataHelper;

impl HelperDef for FormDataHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
    ) -> Result<ScopedJson<'rc>, RenderError> {
        tick(1)?;
        let v = form_value(h);
        charge(&v)?;
        Ok(ScopedJson::Derived(v))
    }

    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        _: &mut dyn Output,
    ) -> HelperResult {
        tick(1)?;
        let name = req_str(h, 1, "formData")?;
        if !valid_variable(&name) {
            return Err(other(format!(
                "formData: `{name}` is not a valid variable name"
            )));
        }
        let v = form_value(h);
        charge(&v)?;
        let mut c = rc.context().as_deref().unwrap_or(ctx).clone();
        charge(c.data())?;
        if let Some(m) = c.data_mut().as_object_mut() {
            m.insert(name, v);
        }
        rc.set_context(c);
        Ok(())
    }
}

struct FakeHelper(&'static str);

impl HelperDef for FakeHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
    ) -> Result<ScopedJson<'rc>, RenderError> {
        tick(1)?;
        let seed = hash_str(h, "seed");
        let mut rng = crate::fakedata::rng(seed.as_deref(), self.0);
        let opts = crate::fakedata::Options {
            length: (hash_num(h, "length").unwrap_or(10.0).max(0.0) as usize)
                .min(MAX_RANDOM_LENGTH),
            days: hash_num(h, "days").unwrap_or(365.0) as i64,
            ..Default::default()
        };
        let v = crate::fakedata::generate(self.0, &mut rng, &opts).unwrap_or(Value::Null);
        charge(&v)?;
        Ok(ScopedJson::Derived(v))
    }
}

fn compare(h: &Helper, op: &str) -> bool {
    let a = h.param(0).map(|p| p.value()).unwrap_or(&Value::Null);
    let b = h.param(1).map(|p| p.value()).unwrap_or(&Value::Null);
    let ord = match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => match (a, b) {
            (
                Value::String(_) | Value::Number(_) | Value::Bool(_),
                Value::String(_) | Value::Number(_) | Value::Bool(_),
            ) => Some(value_text(a).cmp(&value_text(b))),
            _ => {
                return match op {
                    "eq" => a == b,
                    "ne" => a != b,
                    _ => false,
                };
            }
        },
    };
    let Some(o) = ord else {
        return op == "ne";
    };
    match op {
        "eq" => o.is_eq(),
        "ne" => o.is_ne(),
        "gt" => o.is_gt(),
        "gte" => o.is_ge(),
        "lt" => o.is_lt(),
        "lte" => o.is_le(),
        _ => false,
    }
}

fn contains(h: &Helper) -> bool {
    let hay = h.param(0).map(|p| p.value()).unwrap_or(&Value::Null);
    let needle = h.param(1).map(|p| p.value()).unwrap_or(&Value::Null);
    match hay {
        Value::Array(a) => a
            .iter()
            .any(|x| x == needle || value_text(x) == value_text(needle)),
        Value::Object(o) => o.contains_key(&value_text(needle)),
        Value::Null => false,
        other => value_text(other).contains(value_text(needle).as_str()),
    }
}

fn regex_matches(h: &Helper) -> Result<bool, RenderError> {
    let s = param_str(h, 0).unwrap_or_default();
    let pat = req_str(h, 1, "matches")?;
    let re = cached_regex(&format!("^(?:{pat})$")).map_err(|e| other(format!("matches: {e}")))?;
    Ok(re.is_match(&s))
}

const REGEX_CACHE_ENTRIES: usize = 64;

thread_local! {
    static REGEX_CACHE: std::cell::RefCell<std::collections::HashMap<String, regex::Regex>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn cached_regex(pattern: &str) -> Result<regex::Regex, String> {
    if let Some(r) = REGEX_CACHE.with(|c| c.borrow().get(pattern).cloned()) {
        return Ok(r);
    }
    let re = regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| e.to_string())?;
    if pattern.len() > 4096 {
        return Ok(re);
    }
    REGEX_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= REGEX_CACHE_ENTRIES {
            c.clear();
        }
        c.insert(pattern.to_string(), re.clone());
    });
    Ok(re)
}

struct BoolHelper(&'static str);

impl BoolHelper {
    fn eval(&self, h: &Helper) -> Result<bool, RenderError> {
        tick(1)?;
        Ok(match self.0 {
            "eq" | "ne" | "gt" | "gte" | "lt" | "lte" => compare(h, self.0),
            "and" => !h.params().is_empty() && h.params().iter().all(|p| truthy(p.value())),
            "or" => h.params().iter().any(|p| truthy(p.value())),
            "not" => !h.param(0).is_some_and(|p| truthy(p.value())),
            "contains" => contains(h),
            "matches" => regex_matches(h)?,
            _ => false,
        })
    }
}

impl HelperDef for BoolHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
    ) -> Result<ScopedJson<'rc>, RenderError> {
        Ok(ScopedJson::Derived(Value::Bool(self.eval(h)?)))
    }

    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        use handlebars::Renderable;
        let v = self.eval(h)?;
        match h.template() {
            Some(t) => {
                if v {
                    t.render(r, ctx, rc, out)
                } else if let Some(inv) = h.inverse() {
                    inv.render(r, ctx, rc, out)
                } else {
                    Ok(())
                }
            }
            None => out
                .write(if v { "true" } else { "false" })
                .map_err(RenderError::from),
        }
    }
}

struct EachHelper;

impl EachHelper {
    fn iteration<'rc>(
        rc: &mut RenderContext<'_, 'rc>,
        h: &Helper<'rc>,
        base: Option<&Vec<String>>,
        (i, len): (usize, usize),
        key: Option<&str>,
        v: &Value,
    ) -> Result<(), RenderError> {
        let Some(block) = rc.block_mut() else {
            return Ok(());
        };
        let first = i == 0;
        let relative = key.map(String::from).unwrap_or_else(|| i.to_string());
        let k = match key {
            Some(k) => Value::String(k.to_string()),
            None => Value::from(i),
        };
        block.set_local_var("first", Value::Bool(first));
        block.set_local_var("last", Value::Bool(i + 1 == len));
        block.set_local_var("index", Value::from(i));
        if key.is_some() {
            block.set_local_var("key", k.clone());
        }
        match base {
            Some(p) => {
                if first {
                    let mut path = p.clone();
                    path.push(relative);
                    *block.base_path_mut() = path;
                } else if let Some(last) = block.base_path_mut().last_mut() {
                    *last = relative;
                }
            }
            None => block.set_base_value(v.clone()),
        }
        let mut params = handlebars::BlockParams::new();
        if let Some(name) = h.block_param() {
            match base {
                Some(_) => params.add_path(name, Vec::new())?,
                None => params.add_value(name, v.clone())?,
            }
            block.set_block_params(params);
        } else if let Some((name, key_name)) = h.block_param_pair() {
            match base {
                Some(_) => params.add_path(name, Vec::new())?,
                None => params.add_value(name, v.clone())?,
            }
            params.add_value(key_name, k)?;
            block.set_block_params(params);
        }
        Ok(())
    }
}

impl HelperDef for EachHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        use handlebars::Renderable;
        let value = h
            .param(0)
            .ok_or(RenderErrorReason::ParamNotFoundForIndex("each", 0))?;
        let Some(t) = h.template() else {
            return Ok(());
        };
        let new_block = || {
            let mut block = handlebars::BlockContext::new();
            match value.context_path() {
                Some(p) => block.base_path_mut().clone_from(p),
                None => block.set_base_value(value.value().clone()),
            }
            block
        };
        let base = value.context_path();
        match value.value() {
            Value::Array(list) if !list.is_empty() || h.inverse().is_none() => {
                tick(1 + if base.is_none() { list.len() } else { 0 })?;
                rc.push_block(new_block());
                let len = list.len();
                for (i, v) in list.iter().enumerate() {
                    tick(1)?;
                    Self::iteration(rc, h, base, (i, len), None, v)?;
                    t.render(r, ctx, rc, out)?;
                }
                rc.pop_block();
                Ok(())
            }
            Value::Object(obj) if !obj.is_empty() || h.inverse().is_none() => {
                tick(1 + if base.is_none() { obj.len() } else { 0 })?;
                rc.push_block(new_block());
                let len = obj.len();
                for (i, (k, v)) in obj.iter().enumerate() {
                    tick(1)?;
                    Self::iteration(rc, h, base, (i, len), Some(k), v)?;
                    t.render(r, ctx, rc, out)?;
                }
                rc.pop_block();
                Ok(())
            }
            _ => match h.inverse() {
                Some(inv) => inv.render(r, ctx, rc, out),
                None => Ok(()),
            },
        }
    }
}

pub fn hmac_bytes(algo: &str, key: &[u8], data: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(match algo.to_ascii_lowercase().as_str() {
        "sha256" => {
            let mut m = Hmac::<Sha256>::new_from_slice(key)?;
            m.update(data);
            m.finalize().into_bytes().to_vec()
        }
        "sha512" => {
            let mut m = Hmac::<Sha512>::new_from_slice(key)?;
            m.update(data);
            m.finalize().into_bytes().to_vec()
        }
        other => anyhow::bail!("unsupported hmac algorithm {other}"),
    })
}

struct LimitedOutput {
    buf: String,
}

impl Output for LimitedOutput {
    fn write(&mut self, seg: &str) -> std::io::Result<()> {
        if self.buf.len() + seg.len() > MAX_OUTPUT_BYTES {
            return Err(std::io::Error::other("output limit"));
        }
        tick(1).map_err(|e| std::io::Error::other(e.to_string()))?;
        self.buf.push_str(seg);
        Ok(())
    }
}

impl Templates {
    pub fn new() -> Self {
        let mut hb = Handlebars::new();
        hb.register_escape_fn(handlebars::no_escape);
        hb.set_strict_mode(false);
        hb.set_prevent_indent(true);
        hb.register_helper("each", Box::new(EachHelper));
        hb.register_helper("jsonPath", Box::new(JsonText(JsonPathHelper)));
        hb.register_helper("uuid", Box::new(UuidHelper));
        hb.register_helper("now", Box::new(NowHelper));
        hb.register_helper("randomInt", Box::new(RandomIntHelper));
        hb.register_helper("randomDigits", Box::new(RandomDigitsHelper));
        hb.register_helper("pickRandom", Box::new(PickRandomHelper));
        hb.register_helper("base64", Box::new(Base64Helper));
        hb.register_helper("base64Decode", Box::new(Base64DecodeHelper));
        hb.register_helper("sha256", Box::new(Sha256Helper));
        hb.register_helper("hmac", Box::new(HmacHelper));
        hb.register_helper("upper", Box::new(UpperHelper));
        hb.register_helper("lower", Box::new(LowerHelper));
        hb.register_helper("default", Box::new(DefaultHelper));
        hb.register_helper("concat", Box::new(ConcatHelper));
        hb.register_helper("substring", Box::new(SubstringHelper));
        hb.register_helper("regexExtract", Box::new(RegexExtractHelper));
        hb.register_helper("toJson", Box::new(ToJsonHelper));
        hb.register_helper("parseJson", Box::new(ParseJsonHelper));
        hb.register_helper("size", Box::new(SizeHelper));
        hb.register_helper("math", Box::new(MathHelper));
        hb.register_helper("date", Box::new(DateHelper));
        hb.register_helper("parseDate", Box::new(ParseDateHelper));
        hb.register_helper("dateFormat", Box::new(DateFormatHelper));
        hb.register_helper("trim", Box::new(TrimHelper));
        hb.register_helper("split", Box::new(SplitHelper));
        hb.register_helper("join", Box::new(JoinHelper));
        hb.register_helper("replace", Box::new(ReplaceHelper));
        hb.register_helper("capitalize", Box::new(CapitalizeHelper));
        hb.register_helper("capitalizeFirst", Box::new(CapitalizeFirstHelper));
        hb.register_helper("abbreviate", Box::new(AbbreviateHelper));
        hb.register_helper("urlEncode", Box::new(UrlEncodeHelper));
        hb.register_helper("array", Box::new(ArrayHelper));
        hb.register_helper("val", Box::new(ValHelper));
        hb.register_helper("randomValue", Box::new(RandomValueHelper));
        hb.register_helper("randomDecimal", Box::new(RandomDecimalHelper));
        hb.register_helper("jsonMerge", Box::new(JsonMergeHelper));
        hb.register_helper("jsonRemove", Box::new(JsonRemoveHelper));
        hb.register_helper("jsonArrayAdd", Box::new(JsonArrayAddHelper));
        hb.register_helper("formatJson", Box::new(FormatJsonHelper));
        hb.register_helper("formatXml", Box::new(FormatXmlHelper));
        hb.register_helper("xPath", Box::new(XPathHelper));
        hb.register_helper("formData", Box::new(FormDataHelper));
        for name in [
            "eq", "ne", "gt", "gte", "lt", "lte", "and", "or", "not", "contains", "matches",
        ] {
            hb.register_helper(name, Box::new(BoolHelper(name)));
        }
        for kind in crate::fakedata::KINDS {
            hb.register_helper(kind, Box::new(FakeHelper(kind)));
        }
        Self {
            hb,
            cache: Default::default(),
        }
    }

    pub fn validate(&self, tpl: &str) -> Result<(), TemplateError> {
        if tpl.len() > MAX_TEMPLATE_BYTES {
            return Err(TemplateError::TooLarge(tpl.len()));
        }
        reject_partials(tpl)?;
        check_nesting(tpl)?;
        handlebars::Template::compile(tpl)
            .map(|_| ())
            .map_err(|e| TemplateError::Render(e.to_string()))
    }

    pub fn render(&self, tpl: &str, ctx: &Value) -> Result<String, TemplateError> {
        if !tpl.contains("{{") {
            if tpl.len() > MAX_TEMPLATE_BYTES {
                return Err(TemplateError::TooLarge(tpl.len()));
            }
            return Ok(tpl.to_string());
        }
        self.render_in(tpl, &context(ctx.clone()))
    }

    fn compiled(&self, tpl: &str) -> Result<std::sync::Arc<handlebars::Template>, TemplateError> {
        if let Some(t) = self.cache.lock().unwrap().get(tpl) {
            return Ok(t.clone());
        }
        check_nesting(tpl)?;
        let t = std::sync::Arc::new(
            handlebars::Template::compile(tpl).map_err(|e| TemplateError::Render(e.to_string()))?,
        );
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= TEMPLATE_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(tpl.to_string(), t.clone());
        Ok(t)
    }

    pub fn render_in(&self, tpl: &str, context: &TemplateContext) -> Result<String, TemplateError> {
        if tpl.len() > MAX_TEMPLATE_BYTES {
            return Err(TemplateError::TooLarge(tpl.len()));
        }
        if !tpl.contains("{{") {
            return Ok(tpl.to_string());
        }
        reject_partials(tpl)?;
        let template = self.compiled(tpl)?;
        reset_limits();
        let mut rc = RenderContext::new(None);
        let mut out = LimitedOutput { buf: String::new() };
        use handlebars::Renderable;
        template
            .render(&self.hb, context, &mut rc, &mut out)
            .map_err(|e| {
                if out.buf.len() >= MAX_OUTPUT_BYTES / 2 {
                    TemplateError::OutputTooLarge
                } else {
                    TemplateError::Render(e.to_string())
                }
            })?;
        Ok(out.buf)
    }

    pub fn render_json_in(&self, v: &Value, ctx: &TemplateContext) -> Result<Value, TemplateError> {
        Ok(match v {
            Value::String(s) => {
                if let Some(expr) = s.strip_prefix("{{json ").and_then(|x| x.strip_suffix("}}")) {
                    let rendered = self.render_in(&format!("{{{{toJson {expr}}}}}"), ctx)?;
                    serde_json::from_str(&rendered).unwrap_or(Value::String(rendered))
                } else {
                    Value::String(self.render_in(s, ctx)?)
                }
            }
            Value::Array(a) => Value::Array(
                a.iter()
                    .map(|x| self.render_json_in(x, ctx))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, x) in o {
                    m.insert(self.render_in(k, ctx)?, self.render_json_in(x, ctx)?);
                }
                Value::Object(m)
            }
            other => other.clone(),
        })
    }

    pub fn render_json(&self, v: &Value, ctx: &Value) -> Result<Value, TemplateError> {
        self.render_json_in(v, &context(ctx.clone()))
    }

    pub fn validate_json(&self, v: &Value) -> Result<(), TemplateError> {
        match v {
            Value::String(s) => self.validate(s),
            Value::Array(a) => a.iter().try_for_each(|x| self.validate_json(x)),
            Value::Object(o) => o.iter().try_for_each(|(k, x)| {
                self.validate(k)?;
                self.validate_json(x)
            }),
            _ => Ok(()),
        }
    }
}

#[allow(dead_code)]
fn _json_render_marker(v: &Value) -> String {
    v.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({"request": {"path": "/v1/usr/42", "pathParams": {"code": "42"}, "query": {"q": "x"}, "headers": {"x-id": "abc"},
            "body": "{\"dest\":\"380501234567\",\"seq\":7}", "json": {"dest": "380501234567", "seq": 7}}})
    }

    #[test]
    fn renders_request_values_and_helpers() {
        let t = Templates::new();
        assert_eq!(
            t.render(
                "{{request.pathParams.code}}-{{request.query.q}}-{{request.headers.x-id}}",
                &ctx()
            )
            .unwrap(),
            "42-x-abc"
        );
        assert_eq!(
            t.render("{{jsonPath request.json '$.seq'}}", &ctx())
                .unwrap(),
            "7"
        );
        assert_eq!(
            t.render("{{jsonPath request.body '$.dest'}}", &ctx())
                .unwrap(),
            "380501234567"
        );
        assert_eq!(
            t.render("{{upper (concat 'a' 'b')}}", &ctx()).unwrap(),
            "AB"
        );
        assert_eq!(
            t.render("{{substring '380501234567' 6 7}}", &ctx())
                .unwrap(),
            "2"
        );
        assert_eq!(t.render("{{math 2 '*' 21}}", &ctx()).unwrap(), "42");
        assert_eq!(
            t.render(
                "{{#if (eq request.query.q 'x')}}yes{{else}}no{{/if}}",
                &ctx()
            )
            .unwrap(),
            "yes"
        );
        assert_eq!(
            t.render("{{default request.query.missing 'fallback'}}", &ctx())
                .unwrap(),
            "fallback"
        );
        assert_eq!(t.render("<{{request.query.q}}>&", &ctx()).unwrap(), "<x>&");
        assert_eq!(
            t.render("{{base64Decode (base64 'hi')}}", &ctx()).unwrap(),
            "hi"
        );
        assert_eq!(
            t.render(
                "{{regexExtract '380501234567' '380([0-9][0-9])' 1}}",
                &ctx()
            )
            .unwrap(),
            "50"
        );
        assert_eq!(t.render("{{randomDigits 5}}", &ctx()).unwrap().len(), 5);
        assert_eq!(t.render("{{uuid}}", &ctx()).unwrap().len(), 36);
    }

    #[test]
    fn partials_are_refused() {
        let t = Templates::new();
        for tpl in [
            "{{#*inline \"p\"}}{{> p}}{{/inline}}{{> p}}",
            "{{~> x}}",
            "{{#> layout}}x{{/layout}}",
            "{{ > p }}",
        ] {
            assert!(t.validate(tpl).is_err(), "{tpl}");
            assert!(t.render(tpl, &json!({})).is_err(), "{tpl}");
        }
        assert_eq!(
            t.render("{{#if (eq 1 1)}}a > b{{/if}}", &json!({}))
                .unwrap(),
            "a > b"
        );
    }

    #[test]
    fn doubling_intermediate_strings_hit_the_budget() {
        let t = Templates::new();
        let mut tpl = String::from("{{#with 'aaaaaaaaaaaaaaaa'}}");
        for _ in 0..24 {
            tpl.push_str("{{#with (concat this this)}}");
        }
        tpl.push_str("{{size this}}");
        for _ in 0..25 {
            tpl.push_str("{{/with}}");
        }
        let err = t.render(&tpl, &json!({})).unwrap_err().to_string();
        assert!(err.contains("budget"), "{err}");
        assert_eq!(
            t.render("{{size (concat 'ab' 'cd')}}", &json!({})).unwrap(),
            "4"
        );
    }

    #[test]
    fn hostile_now_arguments_are_errors_not_panics() {
        let t = Templates::new();
        assert!(
            t.render(
                "{{now offsetSeconds=request.query.off}}",
                &json!({"request": {"query": {"off": "9223372036854775"}}})
            )
            .is_err()
        );
        assert!(t.render("{{now format='%Q'}}", &json!({})).is_err());
        assert_eq!(
            t.render("{{now format='%Y'}}", &json!({})).unwrap().len(),
            4
        );
    }

    fn r(tpl: &str) -> String {
        Templates::new().render(tpl, &ctx()).unwrap()
    }

    #[test]
    fn wiremock_string_helpers() {
        assert_eq!(r("{{trim '  a b  '}}"), "a b");
        assert_eq!(
            r("{{#each (split 'a,b,c' ',')}}[{{this}}]{{/each}}"),
            "[a][b][c]"
        );
        assert_eq!(r("{{join ',' (array 'a' 'b' 'c')}}"), "a,b,c");
        assert_eq!(r("{{join '-' 'x' 'y' prefix='<' suffix='>'}}"), "<x-y>");
        assert_eq!(r("{{replace 'a-b-c' '-' '+'}}"), "a+b+c");
        assert_eq!(r("{{capitalize 'hello wORLD'}}"), "Hello WORLD");
        assert_eq!(r("{{capitalize 'hello wORLD' fully=true}}"), "Hello World");
        assert_eq!(r("{{capitalizeFirst 'abc def'}}"), "Abc def");
        assert_eq!(r("{{abbreviate 'Hello world' 8}}"), "Hello...");
        assert_eq!(r("{{abbreviate 'Hi' 8}}"), "Hi");
        assert_eq!(r("{{urlEncode 'a b&c=d/é'}}"), "a+b%26c%3Dd%2F%C3%A9");
        assert_eq!(r("{{urlEncode 'a+b%26c' decode=true}}"), "a b&c");
        assert_eq!(r("{{base64 'aGk' decode=true}}"), "hi");
        assert_eq!(r("{{base64 'hi' padding=false}}"), "aGk");
        assert_eq!(r("{{size 'абв'}}-{{size (array 1 2)}}"), "3-2");
        assert_eq!(
            r("{{val request.query.none default='d'}}|{{val request.query.q or='d'}}"),
            "d|x"
        );
        assert_eq!(
            r("{{math 2 'pow' 10}} {{math 3 'max' 7}} {{math 7 '//' 2}}"),
            "1024 7 3"
        );
    }

    #[test]
    fn conditional_helpers_work_inline_and_as_blocks() {
        assert_eq!(r("{{#if (contains 'abcde' 'bcd')}}y{{else}}n{{/if}}"), "y");
        assert_eq!(
            r("{{#contains (array 'a' 'b') 'b'}}y{{else}}n{{/contains}}"),
            "y"
        );
        assert_eq!(
            r("{{#matches '123' '[0-9]+'}}num{{else}}text{{/matches}}"),
            "num"
        );
        assert_eq!(r("{{matches 'abc' '[0-9]+'}}"), "false");
        assert_eq!(
            r("{{#eq request.query.q 'x'}}is x{{else}}not x{{/eq}}"),
            "is x"
        );
        assert_eq!(r("{{#if (gt request.json.seq 5)}}big{{/if}}"), "big");
        assert_eq!(r("{{#if (lt '10' '9')}}str{{else}}num{{/if}}"), "num");
        assert_eq!(
            r("{{#if (and (eq 1 1) (ne 1 2) (gte 2 2) (lte 1 2))}}ok{{/if}}"),
            "ok"
        );
        assert_eq!(r("{{#if (or false (not ''))}}ok{{/if}}"), "ok");
    }

    #[test]
    fn date_helpers() {
        assert_eq!(
            r("{{date (parseDate '10/11/2021' format='dd/MM/yyyy') format='yyyy-MM-dd'}}"),
            "2021-11-10"
        );
        assert_eq!(
            r("{{date '2021-01-31T10:00:00Z' offset='1 months' format='iso'}}"),
            "2021-02-28T10:00:00.000Z"
        );
        assert_eq!(
            r("{{date '2021-01-31T10:00:00Z' timezone='+02:00' format='HH:mm'}}"),
            "12:00"
        );
        assert_eq!(
            r("{{date '2021-01-31T10:00:00Z' format='epoch'}}"),
            "1612087200"
        );
        assert_eq!(
            r("{{dateFormat '2021-01-31T10:00:00Z' 'long'}}"),
            "January 31, 2021"
        );
        assert_eq!(
            r("{{parseDate '1612087200' format='unix'}}"),
            "2021-01-31T10:00:00.000Z"
        );
        let year: i32 = r("{{now offset='1 years' format='yyyy'}}").parse().unwrap();
        assert_eq!(year, chrono::Datelike::year(&Utc::now()) + 1);
        assert!(
            Templates::new()
                .render("{{now timezone='Mars/Base'}}", &json!({}))
                .is_err()
        );
        assert!(
            Templates::new()
                .render("{{now offset='3 fortnights'}}", &json!({}))
                .is_err()
        );
    }

    #[test]
    fn random_helpers() {
        let v = r("{{randomValue length=12 type='HEXADECIMAL' uppercase=true}}");
        assert_eq!(v.len(), 12);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
        assert_eq!(r("{{randomValue type='UUID'}}").len(), 36);
        assert!(
            r("{{randomValue length=5 type='NUMERIC'}}")
                .parse::<u64>()
                .is_ok()
        );
        assert!(
            Templates::new()
                .render("{{randomValue length=100000}}", &json!({}))
                .is_err()
        );
        let d: f64 = r("{{randomDecimal lower=1.5 upper=2.5}}").parse().unwrap();
        assert!((1.5..2.5).contains(&d));
        let i: i64 = r("{{randomInt lower=3 upper=4}}").parse().unwrap();
        assert!((3..=4).contains(&i));
        assert!(["a", "b"].contains(&r("{{pickRandom (array 'a' 'b')}}").as_str()));
        let a = r("{{randomFullName seed='42'}} {{randomEmail seed=7}} {{randomCity seed='x'}}");
        assert_eq!(
            a,
            r("{{randomFullName seed='42'}} {{randomEmail seed=7}} {{randomCity seed='x'}}")
        );
        assert_eq!(r("{{randomAlphaNumeric length=6}}").len(), 6);
        assert!(r("{{randomIP}}").parse::<std::net::Ipv4Addr>().is_ok());
        assert!(matches!(r("{{randomBoolean}}").as_str(), "true" | "false"));
        assert!(chrono::DateTime::parse_from_rfc3339(&r("{{randomDateTime days=3}}")).is_ok());
    }

    #[test]
    fn json_and_xml_helpers() {
        assert_eq!(
            r(r#"{{jsonMerge '{"a":1,"b":{"c":2}}' '{"b":{"d":3},"e":null}' removeNulls=true}}"#),
            r#"{"a":1,"b":{"c":2,"d":3}}"#
        );
        assert_eq!(
            r(r#"{{jsonRemove '{"a":1,"b":[1,2,3],"c":{"d":1}}' '$.b[0,2]'}}"#),
            r#"{"a":1,"b":[2],"c":{"d":1}}"#
        );
        assert_eq!(
            r(r#"{{jsonRemove '{"a":1,"c":{"d":1}}' '$.c.d'}}"#),
            r#"{"a":1,"c":{}}"#
        );
        assert_eq!(
            r(r#"{{jsonArrayAdd '[1,2]' '[3,4]' flatten=true maxItems=3}}"#),
            "[2,3,4]"
        );
        assert_eq!(r(r#"{{jsonArrayAdd '[1]' '{"x":1}'}}"#), r#"[1,{"x":1}]"#);
        assert_eq!(
            r(r#"{{formatJson '{"a":[1]}' format='compact'}}"#),
            r#"{"a":[1]}"#
        );
        assert_eq!(r(r#"{{formatJson '{"a":1}'}}"#), "{\n  \"a\": 1\n}");
        assert_eq!(
            r(r#"{{formatXml '<a xmlns:p="urn:p"><p:b x="1">t</p:b><c/></a>'}}"#),
            "<a xmlns:p=\"urn:p\">\n  <p:b x=\"1\">t</p:b>\n  <c/>\n</a>"
        );
        assert_eq!(
            Templates::new()
                .render(
                    "{{formatXml x format='compact'}}",
                    &json!({"x": "<a>\n  <b>t</b>\n</a>"})
                )
                .unwrap(),
            "<a><b>t</b></a>"
        );
        assert_eq!(r("{{xPath '<r><t>hi</t></r>' '/r/t'}}"), "hi");
        assert_eq!(
            r("{{#each (xPath '<r><t>1</t><t>2</t></r>' '//t')}}{{this}};{{/each}}"),
            "1;2;"
        );
        assert!(
            Templates::new()
                .render("{{jsonMerge '[1]' '{}'}}", &json!({}))
                .is_err()
        );
    }

    #[test]
    fn form_data_assigns_a_variable() {
        let t = Templates::new();
        let c = json!({"request": {"body": "a=1&b=x%20y&a=2"}});
        assert_eq!(
            t.render(
                "{{formData request.body 'form' urlDecode=true}}{{form.a}}-{{form.b}}",
                &c
            )
            .unwrap(),
            "1-x y"
        );
        assert_eq!(
            t.render("{{formData request.body 'f'}}{{f.b}}", &c)
                .unwrap(),
            "x%20y"
        );
        assert_eq!(
            t.render("{{lookup (formData request.body) 'a'}}", &c)
                .unwrap(),
            "1"
        );
        assert!(t.render("{{formData request.body 'request'}}", &c).is_err());
    }

    #[test]
    fn request_context_has_origin_cookies_and_form() {
        let req = crate::request::CapturedRequest::new(
            "POST",
            "/p",
            Some("a=1"),
            vec![
                ("Host".into(), "api.example.com:8443".into()),
                ("Cookie".into(), "sid=abc; theme=dark".into()),
                (
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
            ],
            bytes::Bytes::from_static(b"name=Ann+Lee&age=7"),
        )
        .with_origin("https", None, None);
        let c = req.template_context(&Default::default(), false);
        let t = Templates::new();
        assert_eq!(
            t.render(
                "{{request.scheme}}://{{request.host}}:{{request.port}}{{request.url}} {{request.cookies.sid}} {{request.formParams.name}} {{request.baseUrl}}",
                &c
            )
            .unwrap(),
            "https://api.example.com:8443/p?a=1 abc Ann Lee https://api.example.com:8443"
        );
    }

    #[test]
    fn hmac_known_vector() {
        let t = Templates::new();
        let out = t
            .render(
                "{{hmac 'sha256' 'key' 'The quick brown fox jumps over the lazy dog'}}",
                &json!({}),
            )
            .unwrap();
        assert_eq!(
            out,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn render_json_keeps_types_with_json_marker() {
        let t = Templates::new();
        let v = t.render_json(&json!({"seq": "{{json request.json.seq}}", "msg": "{{request.pathParams.code}}", "n": 1}), &ctx()).unwrap();
        assert_eq!(v, json!({"seq": 7, "msg": "42", "n": 1}));
    }

    #[test]
    fn rejects_bad_templates_and_limits() {
        let t = Templates::new();
        assert!(t.validate("{{#if}}").is_err());
        assert!(
            t.render(&"x".repeat(MAX_TEMPLATE_BYTES + 1), &json!({}))
                .is_err()
        );
        let loops = format!("{{{{#each items}}}}{}{{{{/each}}}}", "y".repeat(200 * 1024));
        let many: Vec<u32> = (0..64).collect();
        assert!(t.render(&loops, &json!({"items": many})).is_err());
    }

    #[test]
    fn each_matches_builtin_semantics() {
        let ours = Templates::new();
        let mut stock = Handlebars::new();
        stock.register_escape_fn(handlebars::no_escape);
        let data = json!({"list": [{"n": "a", "v": [1, 2]}, {"n": "b", "v": []}], "map": {"x": 1, "y": {"z": 2}}, "top": "T", "empty": []});
        for tpl in [
            "{{#each list}}{{@index}}{{@first}}{{@last}}{{n}}{{../top}}{{#each v}}[{{this}}{{@../index}}]{{/each}};{{/each}}",
            "{{#each map}}{{@key}}={{@index}}{{@first}}{{@last}}{{this}},{{/each}}",
            "{{#each list as |item idx|}}{{idx}}:{{item.n}} {{/each}}",
            "{{#each list as |it|}}{{it.n}} {{/each}}",
            "{{#each map as |vv|}}{{vv}} {{/each}}",
            "{{#each map}}{{this}} {{/each}}",
            "{{#each map as |vv kk|}}{{kk}}-{{vv}} {{/each}}",
            "{{#each empty}}x{{else}}none{{/each}}",
            "{{#each missing}}x{{else}}none{{/each}}",
            "{{#each list}}{{#each ../map}}{{@key}}{{/each}}{{/each}}",
            "{{#with list as |l|}}{{#each l}}{{n}}{{@index}}{{/each}}{{/with}}",
            "{{#each list}}{{lookup ../map 'x'}}{{/each}}",
        ] {
            let expected = stock.render_template(tpl, &data).unwrap();
            assert_eq!(ours.render(tpl, &data).unwrap(), expected, "{tpl}");
        }
    }

    #[test]
    fn json_helpers_parse_with_the_node_limit() {
        let t = Templates::new();
        let big = format!("[{}0]", "0,".repeat(crate::request::MAX_JSON_NODES + 10));
        let ctx = json!({"request": {"body": big}});
        assert_eq!(
            t.render("{{jsonPath request.body '$[0]'}}", &ctx).unwrap(),
            ""
        );
        assert_eq!(t.render("{{parseJson request.body}}", &ctx).unwrap(), "");
        let small = json!({"request": {"body": "{\"id\":7,\"a\":[1,2]}"}});
        assert_eq!(
            t.render("{{jsonPath request.body '$.id'}}", &small)
                .unwrap(),
            "7"
        );
        assert_eq!(
            t.render("{{jsonMerge request.body '{\"b\":1}'}}", &small)
                .unwrap(),
            r#"{"id":7,"a":[1,2],"b":1}"#
        );
        let deep = format!(
            "{}{}{}",
            "[".repeat(120),
            vec!["0"; 150_000].join(","),
            "]".repeat(120)
        );
        let started = std::time::Instant::now();
        let err = t
            .render(
                "{{formatJson request.body}}",
                &json!({"request": {"body": deep}}),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("budget"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn capped_json_writer_stops_at_the_budget() {
        BUDGET.with(|b| b.set(1_000));
        let v = json!([[[[[[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]]]]]]);
        let compact = json_string(&v, false).unwrap();
        assert!(compact.len() < 100);
        let nested = json!(vec![vec![vec![1; 20]; 10]; 2]);
        assert!(json_string(&nested, false).is_ok());
        BUDGET.with(|b| b.set(1_000));
        assert!(json_string(&nested, true).is_err());
        BUDGET.with(|b| b.set(MAX_INTERMEDIATE_BYTES));
    }

    #[test]
    fn amplifying_helpers_are_charged_before_allocation() {
        let t = Templates::new();
        let body = "a".repeat(23_000);
        let ctx = json!({"request": {"body": body}});
        for tpl in [
            "{{#with (replace request.body 'a' request.body)}}{{size this}}{{/with}}",
            "{{size (join request.body (split request.body 'a'))}}",
            "{{size (concat request.body request.body (replace request.body 'a' request.body))}}",
        ] {
            let started = std::time::Instant::now();
            let err = t.render(tpl, &ctx).unwrap_err().to_string();
            assert!(err.contains("budget"), "{tpl}: {err}");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "{tpl}"
            );
        }
        let ok = json!({"request": {"body": "a".repeat(2_000)}});
        assert_eq!(
            t.render(
                "{{#with (replace request.body 'a' request.body)}}{{size this}}{{/with}}",
                &ok
            )
            .unwrap(),
            "4000000"
        );
        assert_eq!(
            t.render("{{join '-' (split 'a,b,c' ',')}}", &json!({}))
                .unwrap(),
            "a-b-c"
        );
    }

    #[test]
    fn nested_loops_hit_the_step_limit() {
        let t = Templates::new();
        let body = vec!["x"; 1_000].join(",");
        let ctx = json!({"request": {"body": body}});
        let started = std::time::Instant::now();
        let err = t
            .render(
                "{{#with (split request.body ',') as |a|}}{{#each a}}{{#each a}}{{#each a}}{{/each}}{{/each}}{{/each}}{{/with}}done",
                &ctx,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(
            t.render(
                "{{#with (split request.body ',') as |a|}}{{#each a}}{{/each}}{{/with}}done",
                &ctx
            )
            .unwrap(),
            "done"
        );
    }

    #[test]
    fn deep_nesting_is_rejected_before_rendering() {
        let t = Templates::new();
        let blocks = format!("{}x{}", "{{#if 1}}".repeat(100), "{{/if}}".repeat(100));
        assert!(t.validate(&blocks).is_err());
        assert!(t.render(&blocks, &json!({})).is_err());
        let sub = format!(
            "{{{{upper {}'x'{}}}}}",
            "(upper ".repeat(300),
            ")".repeat(300)
        );
        assert!(t.validate(&sub).is_err());
        assert!(t.render(&sub, &json!({})).is_err());
        let fine = format!("{}x{}", "{{#if 1}}".repeat(20), "{{/if}}".repeat(20));
        assert_eq!(t.render(&fine, &json!({})).unwrap(), "x");
        assert_eq!(
            t.render("{{!-- {{#if}} --}}{{#if 1}}a{{else}}b{{/if}}", &json!({}))
                .unwrap(),
            "a"
        );
    }
}
