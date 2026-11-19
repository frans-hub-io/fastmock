//! JSONPath evaluation in two dialects: RFC 9535 (native) and the Jayway dialect used by WireMock and
//! MockServer. Jayway expressions are translated into RFC 9535 pieces; the parts RFC 9535 cannot express
//! (filters applied to an object itself, trailing functions such as `length()`) are evaluated here.

use std::borrow::Cow;

use serde_json::{Value, json};
use serde_json_path::JsonPath;

use crate::model::JsonPathDialect;

const MAX_EXPRESSION_BYTES: usize = 4096;
const MAX_STEPS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Length,
    Min,
    Max,
    Avg,
    Sum,
    StdDev,
    Keys,
    First,
    Last,
}

#[derive(Debug, Clone)]
enum Step {
    Path(JsonPath),
    Filter(JsonPath),
}

#[derive(Debug, Clone)]
pub struct CompiledJsonPath(Inner);

#[derive(Debug, Clone)]
enum Inner {
    Rfc(JsonPath),
    Jayway {
        steps: Vec<Step>,
        func: Option<Func>,
    },
}

impl CompiledJsonPath {
    pub fn compile(expr: &str, dialect: Option<JsonPathDialect>) -> Result<Self, String> {
        match dialect.unwrap_or_default() {
            JsonPathDialect::Rfc9535 => JsonPath::parse(expr)
                .map(|p| CompiledJsonPath(Inner::Rfc(p)))
                .map_err(|e| e.to_string()),
            JsonPathDialect::Jayway => compile_jayway(expr),
        }
    }

    pub fn query<'a>(&self, root: &'a Value) -> Vec<Cow<'a, Value>> {
        match &self.0 {
            Inner::Rfc(p) => p.query(root).all().into_iter().map(Cow::Borrowed).collect(),
            Inner::Jayway { steps, func } => {
                let mut nodes: Vec<&'a Value> = vec![root];
                for step in steps {
                    let mut next = Vec::new();
                    for n in nodes {
                        match step {
                            Step::Path(p) => next.extend(p.query(n).all()),
                            Step::Filter(p) => match n {
                                Value::Array(_) => next.extend(p.query(n).all()),
                                Value::Object(_) if !p.query(&json!([n])).is_empty() => {
                                    next.push(n)
                                }
                                _ => {}
                            },
                        }
                    }
                    nodes = next;
                }
                match func {
                    None => nodes.into_iter().map(Cow::Borrowed).collect(),
                    Some(f) => nodes
                        .into_iter()
                        .filter_map(|n| apply(*f, n))
                        .map(Cow::Owned)
                        .collect(),
                }
            }
        }
    }
}

fn numbers(v: &Value) -> Option<Vec<f64>> {
    let a = v.as_array()?;
    let n: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
    (!n.is_empty()).then_some(n)
}

fn number(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn apply(f: Func, v: &Value) -> Option<Value> {
    Some(match f {
        Func::Length => match v {
            Value::Array(a) => Value::from(a.len()),
            Value::Object(o) => Value::from(o.len()),
            Value::String(s) => Value::from(s.chars().count()),
            _ => return None,
        },
        Func::Keys => Value::Array(v.as_object()?.keys().cloned().map(Value::String).collect()),
        Func::First => v.as_array()?.first()?.clone(),
        Func::Last => v.as_array()?.last()?.clone(),
        Func::Min => number(numbers(v)?.into_iter().fold(f64::INFINITY, f64::min)),
        Func::Max => number(numbers(v)?.into_iter().fold(f64::NEG_INFINITY, f64::max)),
        Func::Sum => number(numbers(v)?.into_iter().sum()),
        Func::Avg => {
            let n = numbers(v)?;
            number(n.iter().sum::<f64>() / n.len() as f64)
        }
        Func::StdDev => {
            let n = numbers(v)?;
            let mean = n.iter().sum::<f64>() / n.len() as f64;
            number((n.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n.len() as f64).sqrt())
        }
    })
}

fn func_of(name: &str) -> Option<Func> {
    Some(match name {
        "length" | "size" => Func::Length,
        "min" => Func::Min,
        "max" => Func::Max,
        "avg" => Func::Avg,
        "sum" => Func::Sum,
        "stddev" => Func::StdDev,
        "keys" => Func::Keys,
        "first" => Func::First,
        "last" => Func::Last,
        _ => return None,
    })
}

fn unsupported(what: &str) -> String {
    format!("unsupported Jayway JSONPath construct: {what}")
}

fn is_shorthand(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii())
}

fn quote(name: &str) -> String {
    let mut out = String::from("['");
    for c in name.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push_str("']");
    out
}

fn member(name: &str) -> String {
    if name == "*" || is_shorthand(name) {
        format!(".{name}")
    } else {
        quote(name)
    }
}

/// Index of the byte that closes the bracket opened at `open`, skipping quoted strings and regex literals.
fn closing(s: &[u8], open: usize) -> Option<usize> {
    let (o, c) = match s[open] {
        b'[' => (b'[', b']'),
        _ => (b'(', b')'),
    };
    let mut depth = 0usize;
    let mut i = open;
    while i < s.len() {
        match s[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < s.len() && s[i] != q {
                    if s[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if i > 0 && s[..i].trim_ascii_end().ends_with(b"=~") => {
                i += 1;
                while i < s.len() && s[i] != b'/' {
                    if s[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            x if x == o => depth += 1,
            x if x == c => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits a path (after its `$` or `@`) into RFC 9535 segment text, filters and a trailing function.
struct Parsed {
    pieces: Vec<Piece>,
    func: Option<Func>,
}

enum Piece {
    Segments(String),
    Filter { descendant: bool, cond: String },
}

fn parse_path(src: &str) -> Result<Parsed, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut pieces = Vec::new();
    let mut seg = String::new();
    let mut func = None;
    while i < b.len() {
        if func.is_some() {
            return Err(unsupported("a function must be the last part of the path"));
        }
        let descendant = b[i..].starts_with(b"..");
        if b[i] == b'.' {
            i += if descendant { 2 } else { 1 };
            if i < b.len() && b[i] == b'[' {
                if descendant {
                    seg.push_str("..");
                }
                continue;
            }
            let start = i;
            while i < b.len() && !matches!(b[i], b'.' | b'[' | b'(') {
                i += 1;
            }
            let name = &src[start..i];
            if name.is_empty() {
                return Err(unsupported("an empty member name"));
            }
            if i < b.len() && b[i] == b'(' {
                if !src[i..].starts_with("()") {
                    return Err(unsupported("function arguments"));
                }
                func = Some(func_of(name).ok_or_else(|| unsupported(&format!("{name}()")))?);
                i += 2;
                continue;
            }
            if descendant {
                seg.push_str("..");
                seg.push_str(&member(name)[if member(name).starts_with('.') { 1 } else { 0 }..]);
            } else {
                seg.push_str(&member(name));
            }
            continue;
        }
        if b[i] == b'[' {
            let end = closing(b, i).ok_or_else(|| unsupported("an unbalanced bracket"))?;
            let inner = src[i + 1..end].trim();
            if let Some(cond) = inner.strip_prefix('?') {
                let descendant = seg.ends_with("..");
                if descendant {
                    seg.truncate(seg.len() - 2);
                }
                if !seg.is_empty() {
                    pieces.push(Piece::Segments(std::mem::take(&mut seg)));
                }
                pieces.push(Piece::Filter {
                    descendant,
                    cond: translate_filter(cond.trim())?,
                });
            } else {
                seg.push('[');
                seg.push_str(inner);
                seg.push(']');
            }
            i = end + 1;
            continue;
        }
        return Err(unsupported(&format!("`{}`", &src[i..])));
    }
    if !seg.is_empty() {
        pieces.push(Piece::Segments(seg));
    }
    Ok(Parsed { pieces, func })
}

fn compile_jayway(expr: &str) -> Result<CompiledJsonPath, String> {
    let expr = expr.trim();
    if expr.len() > MAX_EXPRESSION_BYTES {
        return Err("the JSONPath expression is too long".into());
    }
    let rest = expr
        .strip_prefix('$')
        .ok_or_else(|| unsupported("a path that does not start with $"))?;
    let parsed = parse_path(rest)?;
    if parsed.pieces.len() > MAX_STEPS {
        return Err("the JSONPath expression has too many parts".into());
    }
    let mut steps = Vec::with_capacity(parsed.pieces.len());
    let parse = |text: String| {
        JsonPath::parse(&text).map_err(|e| format!("invalid JSONPath `{expr}` ({text}): {e}"))
    };
    for p in parsed.pieces {
        steps.push(match p {
            Piece::Segments(s) => Step::Path(parse(format!("${s}"))?),
            Piece::Filter {
                descendant: true,
                cond,
            } => Step::Path(parse(format!("$..[?{cond}]"))?),
            Piece::Filter {
                descendant: false,
                cond,
            } => Step::Filter(parse(format!("$[?{cond}]"))?),
        });
    }
    if steps.is_empty() {
        steps.push(Step::Path(parse("$".into())?));
    }
    Ok(CompiledJsonPath(Inner::Jayway {
        steps,
        func: parsed.func,
    }))
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Operand(String),
    Op(String),
    Regex(String),
    List(Vec<String>),
    Open,
    Close,
}

fn rfc_string(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn query_operand(src: &str) -> Result<String, String> {
    let root = &src[..1];
    let parsed = parse_path(&src[1..])?;
    let mut text = String::from(root);
    for p in parsed.pieces {
        match p {
            Piece::Segments(s) => text.push_str(&s),
            Piece::Filter { descendant, cond } => {
                if descendant {
                    text.push_str("..");
                }
                text.push_str(&format!("[?{cond}]"));
            }
        }
    }
    match parsed.func {
        None => Ok(text),
        Some(Func::Length) => Ok(format!("length({text})")),
        Some(_) => Err(unsupported("functions other than length() inside a filter")),
    }
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'(' => {
                out.push(Tok::Open);
                i += 1;
            }
            b')' => {
                out.push(Tok::Close);
                i += 1;
            }
            b'\'' | b'"' => {
                let mut j = i + 1;
                let mut s = String::new();
                while j < b.len() && b[j] != c {
                    if b[j] == b'\\' && j + 1 < b.len() {
                        j += 1;
                    }
                    let ch = src[j..].chars().next().unwrap_or_default();
                    s.push(ch);
                    j += ch.len_utf8();
                }
                if j >= b.len() {
                    return Err(unsupported("an unterminated string"));
                }
                out.push(Tok::Operand(rfc_string(&s)));
                i = j + 1;
            }
            b'/' if matches!(out.last(), Some(Tok::Op(o)) if o == "=~") => {
                let mut j = i + 1;
                let mut re = String::new();
                while j < b.len() && b[j] != b'/' {
                    if b[j] == b'\\' && j + 1 < b.len() && b[j + 1] == b'/' {
                        j += 1;
                    }
                    let ch = src[j..].chars().next().unwrap_or_default();
                    re.push(ch);
                    j += ch.len_utf8();
                }
                if j >= b.len() {
                    return Err(unsupported("an unterminated regex"));
                }
                j += 1;
                let mut flags = String::new();
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    flags.push(b[j] as char);
                    j += 1;
                }
                if flags.chars().any(|f| f != 'i') {
                    return Err(unsupported(&format!("regex flags `{flags}`")));
                }
                if flags.contains('i') {
                    re = format!("(?i){re}");
                }
                out.push(Tok::Regex(re));
                i = j;
            }
            b'@' | b'$' => {
                let mut j = i + 1;
                while j < b.len() {
                    match b[j] {
                        b'[' => {
                            j = closing(b, j).ok_or_else(|| unsupported("an unbalanced bracket"))?
                                + 1
                        }
                        b'(' if src[j..].starts_with("()") => j += 2,
                        x if x.is_ascii_alphanumeric()
                            || matches!(x, b'.' | b'_' | b'-' | b'*')
                            || !x.is_ascii() =>
                        {
                            j += 1
                        }
                        _ => break,
                    }
                }
                out.push(Tok::Operand(query_operand(&src[i..j])?));
                i = j;
            }
            b'[' => {
                let end = closing(b, i).ok_or_else(|| unsupported("an unbalanced list"))?;
                let list: Value = serde_json::from_str(&src[i..end + 1].replace('\'', "\""))
                    .map_err(|_| unsupported(&format!("the list `{}`", &src[i..end + 1])))?;
                let items = list
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|v| match v {
                                Value::String(s) => rfc_string(s),
                                other => other.to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Tok::List(items));
                i = end + 1;
            }
            b'=' | b'!' | b'<' | b'>' | b'&' | b'|' => {
                let two = src.get(i..i + 2).unwrap_or("");
                let op = match two {
                    "==" | "!=" | "<=" | ">=" | "=~" | "&&" | "||" => two,
                    _ => &src[i..i + 1],
                };
                if op == "=" || op == "&" || op == "|" {
                    return Err(unsupported(&format!("the operator `{op}`")));
                }
                out.push(Tok::Op(op.to_string()));
                i += op.len();
            }
            _ => {
                let mut j = i;
                while j < b.len()
                    && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'.' | b'-' | b'+' | b'_'))
                {
                    j += 1;
                }
                if j == i {
                    return Err(unsupported(&format!("`{}`", &src[i..])));
                }
                let word = &src[i..j];
                match word {
                    "in" | "nin" | "size" | "empty" | "subsetof" | "anyof" | "noneof"
                    | "contains" => out.push(Tok::Op(word.to_string())),
                    _ => out.push(Tok::Operand(word.to_string())),
                }
                i = j;
            }
        }
    }
    Ok(out)
}

fn translate_filter(cond: &str) -> Result<String, String> {
    let cond = cond.trim();
    let cond = match cond.strip_prefix('(').and_then(|c| c.strip_suffix(')')) {
        Some(inner) if closing(cond.as_bytes(), 0) == Some(cond.len() - 1) => inner,
        _ => cond,
    };
    let toks = tokenize(cond)?;
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let next_op = match toks.get(i + 1) {
            Some(Tok::Op(o)) => Some(o.as_str()),
            _ => None,
        };
        match (&toks[i], next_op) {
            (Tok::Operand(lhs), Some(op @ ("=~" | "in" | "nin" | "size" | "empty"))) => {
                let rhs = toks.get(i + 2).ok_or_else(|| unsupported(op))?;
                let text = match (op, rhs) {
                    ("=~", Tok::Regex(re)) => format!("match({lhs}, {})", rfc_string(re)),
                    ("in", Tok::List(items)) if !items.is_empty() => format!(
                        "({})",
                        items
                            .iter()
                            .map(|x| format!("{lhs} == {x}"))
                            .collect::<Vec<_>>()
                            .join(" || ")
                    ),
                    ("in", Tok::List(_)) => "false".into(),
                    ("nin", Tok::List(items)) if !items.is_empty() => format!(
                        "({})",
                        items
                            .iter()
                            .map(|x| format!("{lhs} != {x}"))
                            .collect::<Vec<_>>()
                            .join(" && ")
                    ),
                    ("nin", Tok::List(_)) => "true".into(),
                    ("size", Tok::Operand(n)) => format!("length({lhs}) == {n}"),
                    ("empty", Tok::Operand(t)) if t == "true" => format!("length({lhs}) == 0"),
                    ("empty", Tok::Operand(t)) if t == "false" => format!("length({lhs}) > 0"),
                    _ => return Err(unsupported(&format!("`{op}` with this operand"))),
                };
                out.push(text);
                i += 3;
            }
            (_, Some(op @ ("subsetof" | "anyof" | "noneof" | "contains"))) => {
                return Err(unsupported(&format!("the `{op}` operator")));
            }
            (Tok::Operand(x), _) => {
                out.push(x.clone());
                i += 1;
            }
            (Tok::Op(o), _) => {
                if matches!(o.as_str(), "=~" | "in" | "nin" | "size" | "empty") {
                    return Err(unsupported(&format!("`{o}` without a left operand")));
                }
                out.push(o.clone());
                i += 1;
            }
            (Tok::Open, _) => {
                out.push("(".into());
                i += 1;
            }
            (Tok::Close, _) => {
                out.push(")".into());
                i += 1;
            }
            (Tok::Regex(_), _) | (Tok::List(_), _) => {
                return Err(unsupported("a regex or list outside a comparison"));
            }
        }
    }
    let mut text = String::new();
    for (k, part) in out.iter().enumerate() {
        if k > 0 && part != ")" && !text.ends_with('(') && !text.ends_with('!') {
            text.push(' ');
        }
        text.push_str(part);
    }
    Ok(format!("({text})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(expr: &str, doc: Value) -> Vec<Value> {
        CompiledJsonPath::compile(expr, Some(JsonPathDialect::Jayway))
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
            .query(&doc)
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    }

    #[test]
    fn jayway_constructs_from_the_review() {
        let things = json!({"things": [{"name": "Required"}, {"name": "Other"}]});
        assert_eq!(q("$.things[?(@.name =~ /Req.*/)]", things.clone()).len(), 1);
        assert_eq!(
            q("$.things[?(@.name =~ /req.*/i)]", things.clone()).len(),
            1
        );
        assert!(q("$.things[?(@.name =~ /Req/)]", things).is_empty());
        let items = json!({"items": [{"status": "A"}, {"status": "B"}, {"status": "C"}]});
        assert_eq!(
            q("$.items[?(@.status in [\"A\",\"B\"])]", items.clone()).len(),
            2
        );
        assert_eq!(
            q("$.items[?(@.status nin ['A','B'])]", items.clone()).len(),
            1
        );
        assert_eq!(q("$.items.length()", items.clone()), vec![json!(3)]);
        assert_eq!(q("$.items.size()", items.clone()), vec![json!(3)]);
        assert_eq!(q("$[?(@.items.size() == 3)]", items.clone()).len(), 1);
        assert_eq!(q("$[?(@.items.length() == 2)]", items.clone()).len(), 0);
        assert_eq!(q("$[?(@.items size 3)]", items.clone()).len(), 1);
        assert_eq!(q("$[?(@.items empty false)]", items).len(), 1);
    }

    #[test]
    fn filters_apply_to_objects_themselves() {
        assert_eq!(
            q("$[?(@.qty > 1)]", json!({"qty": 2})),
            vec![json!({"qty": 2})]
        );
        assert!(q("$[?(@.qty > 1)]", json!({"qty": 1})).is_empty());
        assert_eq!(
            q(
                "$.order[?(@.qty > 1)].sku",
                json!({"order": {"qty": 2, "sku": "A"}})
            ),
            vec![json!("A")]
        );
        assert_eq!(
            q(
                "$.order[?(@.qty > 1)].sku",
                json!({"order": [{"qty": 2, "sku": "A"}, {"qty": 0, "sku": "B"}]})
            ),
            vec![json!("A")]
        );
        assert_eq!(
            q(
                "$..book[?(@.price < 10)].t",
                json!({"s": {"book": [{"price": 5, "t": "x"}, {"price": 50, "t": "y"}]}})
            ),
            vec![json!("x")]
        );
    }

    #[test]
    fn plain_paths_and_names() {
        let doc = json!({"a-b": {"c": [1, 2, 3]}, "x": {"1y": true}});
        assert_eq!(q("$.a-b.c[1]", doc.clone()), vec![json!(2)]);
        assert_eq!(q("$['a-b'].c[-1:]", doc.clone()), vec![json!(3)]);
        assert_eq!(q("$.x.1y", doc.clone()), vec![json!(true)]);
        assert_eq!(q("$.a-b.c.max()", doc.clone()), vec![json!(3.0)]);
        assert_eq!(q("$.a-b.c.avg()", doc.clone()), vec![json!(2.0)]);
        assert_eq!(q("$", doc.clone()), vec![doc]);
    }

    #[test]
    fn unsupported_constructs_are_errors() {
        for e in [
            "$.items[?(@.tags contains 'x')]",
            "$.items[?(@.tags subsetof ['x'])]",
            "$.items.foo()",
            "items",
            "$.items[?(@.a =~ /x/g)]",
        ] {
            assert!(
                CompiledJsonPath::compile(e, Some(JsonPathDialect::Jayway)).is_err(),
                "{e}"
            );
        }
        assert!(CompiledJsonPath::compile("$.a.length()", None).is_err());
    }
}
