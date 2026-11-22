use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::datetime::{ExpectedTime, parse_datetime, parse_with_format, truncate, validate_format};
use crate::model::{
    BodyMatcher, DateTruncation, MultipartMatcher, MultipartMatchingType, RequestPattern,
    StringMatcher, Stub,
};
use crate::request::{CapturedRequest, MultipartPart};

const REGEX_SIZE_LIMIT: usize = 256 * 1024;
pub const MAX_MATCHERS_PER_STUB: usize = 64;
pub const MAX_MATCHER_DEPTH: usize = 8;
pub const MAX_MATCHER_NODES: usize = 256;
pub const MAX_MULTIPART_PATTERNS: usize = 16;
const MAX_JSON_PLACEHOLDER_REGEXES: usize = 64;
const MAX_UNORDERED_PAIRS: usize = 65_536;
pub const MAX_JSON_COMPARE_STEPS: usize = 2_000_000;

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("invalid regex `{0}`: {1}")]
    Regex(String, String),
    #[error("invalid JSONPath `{0}`: {1}")]
    JsonPath(String, String),
    #[error("invalid path template `{0}`")]
    PathTemplate(String),
    #[error("{0}")]
    Invalid(String),
}

fn regex(pattern: &str, case_insensitive: bool) -> Result<Regex, CompileError> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| CompileError::Regex(pattern.to_string(), e.to_string()))?;
    RegexBuilder::new(&format!("^(?:{pattern})$"))
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| CompileError::Regex(pattern.to_string(), e.to_string()))
}

struct Budget {
    nodes: usize,
}

impl Budget {
    fn new() -> Self {
        Self { nodes: 0 }
    }

    fn take(&mut self, depth: usize) -> Result<(), CompileError> {
        if depth > MAX_MATCHER_DEPTH {
            return Err(CompileError::Invalid(format!(
                "matchers may be nested at most {MAX_MATCHER_DEPTH} levels deep"
            )));
        }
        self.nodes += 1;
        if self.nodes > MAX_MATCHER_NODES {
            return Err(CompileError::Invalid(format!(
                "a stub may have at most {MAX_MATCHER_NODES} matcher nodes including nested ones"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DateKind {
    Before,
    After,
    Equal,
}

#[derive(Debug, Clone)]
struct DateCheck {
    kind: DateKind,
    expected: ExpectedTime,
}

#[derive(Debug, Clone, Default)]
pub struct CompiledString {
    equal_to: Option<String>,
    contains: Option<String>,
    absent: Option<bool>,
    case_insensitive: bool,
    re: Option<Regex>,
    not_re: Option<Regex>,
    dates: Vec<DateCheck>,
    actual_format: Option<String>,
    truncate_expected: Option<DateTruncation>,
    truncate_actual: Option<DateTruncation>,
    and: Vec<CompiledString>,
    or: Vec<CompiledString>,
    not: Option<Box<CompiledString>>,
    has_exactly: Option<Vec<CompiledString>>,
    includes: Option<Vec<CompiledString>>,
    all_values: Option<Vec<CompiledString>>,
    leaf: bool,
}

impl CompiledString {
    pub fn compile(spec: &StringMatcher) -> Result<Self, CompileError> {
        Self::compile_with(spec, 0, &mut Budget::new())
    }

    fn compile_list(
        list: &[StringMatcher],
        depth: usize,
        budget: &mut Budget,
    ) -> Result<Vec<CompiledString>, CompileError> {
        list.iter()
            .map(|m| Self::compile_with(m, depth + 1, budget))
            .collect()
    }

    fn compile_with(
        spec: &StringMatcher,
        depth: usize,
        budget: &mut Budget,
    ) -> Result<Self, CompileError> {
        budget.take(depth)?;
        let re = spec
            .matches
            .as_deref()
            .map(|p| regex(p, spec.case_insensitive))
            .transpose()?;
        let not_re = spec
            .does_not_match
            .as_deref()
            .map(|p| regex(p, spec.case_insensitive))
            .transpose()?;
        let mut dates = Vec::new();
        for (kind, v) in [
            (DateKind::Before, &spec.before),
            (DateKind::After, &spec.after),
            (DateKind::Equal, &spec.equal_to_date_time),
        ] {
            if let Some(v) = v {
                dates.push(DateCheck {
                    kind,
                    expected: ExpectedTime::parse(v).map_err(CompileError::Invalid)?,
                });
            }
        }
        if let Some(f) = &spec.actual_format {
            validate_format(f).map_err(CompileError::Invalid)?;
        }
        if dates.is_empty()
            && (spec.actual_format.is_some()
                || spec.truncate_actual.is_some()
                || spec.truncate_expected.is_some())
        {
            return Err(CompileError::Invalid(
                "actualFormat, truncateExpected and truncateActual need before, after or equalToDateTime".into(),
            ));
        }
        let leaf = spec.equal_to.is_some()
            || spec.contains.is_some()
            || re.is_some()
            || not_re.is_some()
            || !dates.is_empty();
        let multi =
            spec.has_exactly.is_some() || spec.includes.is_some() || spec.all_values.is_some();
        if multi
            && (leaf
                || spec.absent.is_some()
                || !spec.and.is_empty()
                || !spec.or.is_empty()
                || spec.not.is_some())
        {
            return Err(CompileError::Invalid(
                "hasExactly and includes cannot be combined with other conditions in the same matcher".into(),
            ));
        }
        if [
            spec.has_exactly.is_some(),
            spec.includes.is_some(),
            spec.all_values.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count()
            > 1
        {
            return Err(CompileError::Invalid(
                "use only one of hasExactly, includes and allValues".into(),
            ));
        }
        if spec.has_exactly.as_ref().is_some_and(|v| v.is_empty())
            || spec.includes.as_ref().is_some_and(|v| v.is_empty())
            || spec.all_values.as_ref().is_some_and(|v| v.is_empty())
        {
            return Err(CompileError::Invalid(
                "hasExactly and includes need at least one matcher".into(),
            ));
        }
        Ok(Self {
            equal_to: spec.equal_to.clone(),
            contains: spec.contains.as_ref().map(|c| {
                if spec.case_insensitive {
                    c.to_lowercase()
                } else {
                    c.clone()
                }
            }),
            absent: spec.absent,
            case_insensitive: spec.case_insensitive,
            re,
            not_re,
            dates,
            actual_format: spec.actual_format.clone(),
            truncate_expected: spec.truncate_expected,
            truncate_actual: spec.truncate_actual,
            and: Self::compile_list(&spec.and, depth, budget)?,
            or: Self::compile_list(&spec.or, depth, budget)?,
            not: spec
                .not
                .as_deref()
                .map(|n| Self::compile_with(n, depth + 1, budget).map(Box::new))
                .transpose()?,
            has_exactly: spec
                .has_exactly
                .as_deref()
                .map(|l| Self::compile_list(l, depth, budget))
                .transpose()?,
            includes: spec
                .includes
                .as_deref()
                .map(|l| Self::compile_list(l, depth, budget))
                .transpose()?,
            all_values: spec
                .all_values
                .as_deref()
                .map(|l| Self::compile_list(l, depth, budget))
                .transpose()?,
            leaf,
        })
    }

    fn date_ok(&self, v: &str, now: DateTime<Utc>) -> bool {
        let actual = match &self.actual_format {
            Some(f) => parse_with_format(v, f),
            None => parse_datetime(v),
        };
        let Some(mut actual) = actual else {
            return false;
        };
        if let Some(t) = self.truncate_actual {
            actual = truncate(actual, t);
        }
        self.dates.iter().all(|d| {
            let Some(mut expected) = d.expected.resolve(now) else {
                return false;
            };
            if let Some(t) = self.truncate_expected {
                expected = truncate(expected, t);
            }
            match d.kind {
                DateKind::Before => actual < expected,
                DateKind::After => actual > expected,
                DateKind::Equal => actual == expected,
            }
        })
    }

    pub fn matches(&self, value: Option<&str>) -> bool {
        if self.absent == Some(true) {
            return value.is_none();
        }
        if let Some(l) = self
            .has_exactly
            .as_ref()
            .or(self.includes.as_ref())
            .or(self.all_values.as_ref())
            && !l.is_empty()
        {
            return self.matches_values(value.as_slice());
        }
        let Some(v) = value else {
            if self.leaf || self.absent == Some(false) {
                return false;
            }
            if self.and.is_empty() && self.or.is_empty() && self.not.is_none() {
                return false;
            }
            return self.and.iter().all(|m| m.matches(None))
                && (self.or.is_empty() || self.or.iter().any(|m| m.matches(None)))
                && self.not.as_ref().is_none_or(|n| !n.matches(None));
        };
        if let Some(e) = &self.equal_to {
            let ok = if self.case_insensitive {
                v.to_lowercase() == e.to_lowercase()
            } else {
                v == e
            };
            if !ok {
                return false;
            }
        }
        if let Some(c) = &self.contains {
            let ok = if self.case_insensitive {
                v.to_lowercase().contains(c.as_str())
            } else {
                v.contains(c.as_str())
            };
            if !ok {
                return false;
            }
        }
        if let Some(re) = &self.re
            && !re.is_match(v)
        {
            return false;
        }
        if let Some(re) = &self.not_re
            && re.is_match(v)
        {
            return false;
        }
        if !self.dates.is_empty() && !self.date_ok(v, Utc::now()) {
            return false;
        }
        self.and.iter().all(|m| m.matches(Some(v)))
            && (self.or.is_empty() || self.or.iter().any(|m| m.matches(Some(v))))
            && self.not.as_ref().is_none_or(|n| !n.matches(Some(v)))
    }

    pub fn matches_values<S: AsRef<str>>(&self, values: &[S]) -> bool {
        if let Some(list) = &self.has_exactly {
            return values.len() == list.len()
                && crate::xml::bipartite(list.len(), values.len(), |i, j| {
                    list[i].matches(Some(values[j].as_ref()))
                });
        }
        if let Some(list) = &self.includes {
            return !values.is_empty()
                && list
                    .iter()
                    .all(|m| values.iter().any(|v| m.matches(Some(v.as_ref()))));
        }
        if let Some(list) = &self.all_values {
            return !values.is_empty()
                && values
                    .iter()
                    .all(|v| list.iter().any(|m| m.matches(Some(v.as_ref()))));
        }
        if values.is_empty() {
            self.matches(None)
        } else {
            values.iter().any(|v| self.matches(Some(v.as_ref())))
        }
    }
}

#[derive(Debug, Clone)]
enum PathMatch {
    Any,
    Exact(String),
    Regex(Regex),
    Template(Vec<Segment>),
    Url(String),
    UrlRegex(Regex),
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(String),
    Param(String),
    Pattern(Regex, Vec<String>),
    Rest(String),
}

fn compile_mixed_segment(p: &str, t: &str) -> Result<Segment, CompileError> {
    let mut re = String::from("^");
    let mut names = Vec::new();
    let mut rest = p;
    while let Some(open) = rest.find('{') {
        re.push_str(&regex::escape(&rest[..open]));
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| CompileError::PathTemplate(t.to_string()))?
            + open;
        let name = &rest[open + 1..close];
        if name.is_empty() || name.contains(['{', '}']) || name.ends_with("...") {
            return Err(CompileError::PathTemplate(t.to_string()));
        }
        names.push(name.to_string());
        re.push_str("([^/]+?)");
        rest = &rest[close + 1..];
    }
    if rest.contains('}') {
        return Err(CompileError::PathTemplate(t.to_string()));
    }
    re.push_str(&regex::escape(rest));
    re.push('$');
    Ok(Segment::Pattern(
        Regex::new(&re).map_err(|e| CompileError::Regex(re.clone(), e.to_string()))?,
        names,
    ))
}

fn compile_template(t: &str) -> Result<Vec<Segment>, CompileError> {
    let mut segs = Vec::new();
    let parts: Vec<&str> = t.trim_start_matches('/').split('/').collect();
    for (i, p) in parts.iter().enumerate() {
        if let Some(inner) = p.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
            if let Some(name) = inner.strip_suffix("...") {
                if i != parts.len() - 1 || name.is_empty() {
                    return Err(CompileError::PathTemplate(t.to_string()));
                }
                segs.push(Segment::Rest(name.to_string()));
            } else {
                if inner.is_empty() || inner.contains(['{', '}']) {
                    return Err(CompileError::PathTemplate(t.to_string()));
                }
                segs.push(Segment::Param(inner.to_string()));
            }
        } else if p.contains(['{', '}']) {
            segs.push(compile_mixed_segment(p, t)?);
        } else {
            segs.push(Segment::Literal((*p).to_string()));
        }
    }
    Ok(segs)
}

fn match_template(segs: &[Segment], path: &str) -> Option<BTreeMap<String, String>> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut params = BTreeMap::new();
    for (i, seg) in segs.iter().enumerate() {
        match seg {
            Segment::Rest(name) => {
                if i > parts.len() {
                    return None;
                }
                params.insert(name.clone(), parts[i..].join("/"));
                return Some(params);
            }
            Segment::Literal(l) => {
                if parts.get(i) != Some(&l.as_str()) {
                    return None;
                }
            }
            Segment::Param(name) => {
                let v = parts.get(i)?;
                if v.is_empty() {
                    return None;
                }
                params.insert(name.clone(), (*v).to_string());
            }
            Segment::Pattern(re, names) => {
                let caps = re.captures(parts.get(i)?)?;
                for (k, name) in names.iter().enumerate() {
                    params.insert(name.clone(), caps.get(k + 1)?.as_str().to_string());
                }
            }
        }
    }
    (parts.len() == segs.len()).then_some(params)
}

#[derive(Clone)]
pub struct SchemaValidator(pub std::sync::Arc<jsonschema::Validator>);

impl std::fmt::Debug for SchemaValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SchemaValidator")
    }
}

#[derive(Debug, Clone)]
struct JsonExpect {
    value: Value,
    lenient: bool,
    extra_items: bool,
    ignore_order: bool,
    placeholders: bool,
    regexes: Arc<HashMap<String, Regex>>,
}

#[derive(Debug, Clone)]
enum BodyCheck {
    EqualTo(String),
    EqualToJson(Box<JsonExpect>),
    Contains(String),
    Matches(Regex),
    JsonPath(
        crate::jsonpath::CompiledJsonPath,
        Option<CompiledString>,
        Option<Box<CompiledBody>>,
    ),
    JsonSchema(SchemaValidator),
    XPath(
        crate::xml::XPath,
        Option<CompiledString>,
        Option<Box<CompiledBody>>,
    ),
    EqualToXml(Arc<crate::xml::ExpectedXml>),
    Binary(Vec<u8>),
    Absent,
    GraphQlRootField(CompiledString),
}

#[derive(Debug, Clone)]
struct CompiledBody {
    check: BodyCheck,
    negate: bool,
}

pub fn compile_schema(schema: &Value) -> Result<SchemaValidator, CompileError> {
    let v = jsonschema::options()
        .should_validate_formats(true)
        .with_pattern_options(
            jsonschema::PatternOptions::regex()
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_SIZE_LIMIT),
        )
        .build(schema)
        .map_err(|e| CompileError::Invalid(format!("invalid JSON schema: {e}")))?;
    Ok(SchemaValidator(std::sync::Arc::new(v)))
}

const JSON_REGEX_PLACEHOLDER: &str = "${json-unit.regex}";

fn collect_json_regexes(v: &Value, out: &mut HashMap<String, Regex>) -> Result<(), CompileError> {
    match v {
        Value::String(s) => {
            if let Some(re) = s.strip_prefix(JSON_REGEX_PLACEHOLDER) {
                if out.len() >= MAX_JSON_PLACEHOLDER_REGEXES {
                    return Err(CompileError::Invalid(format!(
                        "at most {MAX_JSON_PLACEHOLDER_REGEXES} regex placeholders per equalToJson"
                    )));
                }
                out.insert(re.to_string(), regex(re, false)?);
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_json_regexes(x, out)?;
            }
        }
        Value::Object(o) => {
            for x in o.values() {
                collect_json_regexes(x, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn compile_body(
    b: &BodyMatcher,
    depth: usize,
    budget: &mut Budget,
) -> Result<CompiledBody, CompileError> {
    budget.take(depth)?;
    let set = [
        b.equal_to.is_some(),
        b.equal_to_json.is_some(),
        b.contains.is_some(),
        b.matches.is_some(),
        b.json_path.is_some(),
        b.json_schema.is_some(),
        b.xpath.is_some(),
        b.equal_to_xml.is_some(),
        b.binary_equal_to.is_some(),
        b.absent,
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if set != 1 {
        return Err(CompileError::Invalid("a body matcher must set exactly one of equalTo, equalToJson, equalToXml, binaryEqualTo, contains, matches, jsonPath, jsonSchema, xpath, absent".into()));
    }
    if b.ignore_extra_array_items && b.equal_to_json.is_none() {
        return Err(CompileError::Invalid(
            "ignoreExtraArrayItems applies only to equalToJson".into(),
        ));
    }
    if b.json_path_dialect.is_some() && b.json_path.is_none() {
        return Err(CompileError::Invalid(
            "jsonPathDialect applies only to jsonPath".into(),
        ));
    }
    if b.value_body.is_some() && b.json_path.is_none() && b.xpath.is_none() {
        return Err(CompileError::Invalid(
            "valueBody applies only to jsonPath and xpath".into(),
        ));
    }
    if b.value_body.is_some() && b.value.is_some() {
        return Err(CompileError::Invalid(
            "use only one of value and valueBody".into(),
        ));
    }
    let value_body = |budget: &mut Budget| {
        b.value_body
            .as_deref()
            .map(|v| compile_body(v, depth + 1, budget).map(Box::new))
            .transpose()
    };
    if b.ignore_array_order && b.equal_to_json.is_none() {
        return Err(CompileError::Invalid(
            "ignoreArrayOrder applies only to equalToJson".into(),
        ));
    }
    if !b.xpath_namespaces.is_empty() && b.xpath.is_none() {
        return Err(CompileError::Invalid(
            "xpathNamespaces applies only to xpath".into(),
        ));
    }
    if !b.exempted_comparisons.is_empty() && b.equal_to_xml.is_none() {
        return Err(CompileError::Invalid(
            "exemptedComparisons applies only to equalToXml".into(),
        ));
    }
    let placeholders = b.enable_placeholders != Some(false);
    let value = |budget: &mut Budget| {
        b.value
            .as_ref()
            .map(|v| CompiledString::compile_with(v, depth + 1, budget))
            .transpose()
    };
    let check = if let Some(v) = &b.equal_to {
        BodyCheck::EqualTo(v.clone())
    } else if let Some(v) = &b.equal_to_json {
        let mut regexes = HashMap::new();
        if placeholders {
            collect_json_regexes(v, &mut regexes)?;
        }
        BodyCheck::EqualToJson(Box::new(JsonExpect {
            value: v.clone(),
            lenient: b.ignore_extra_elements,
            extra_items: b.ignore_extra_array_items,
            ignore_order: b.ignore_array_order,
            placeholders,
            regexes: Arc::new(regexes),
        }))
    } else if let Some(v) = &b.contains {
        BodyCheck::Contains(v.clone())
    } else if let Some(v) = &b.matches {
        BodyCheck::Matches(regex(v, false)?)
    } else if let Some(v) = &b.json_schema {
        BodyCheck::JsonSchema(compile_schema(v)?)
    } else if let Some(x) = &b.xpath {
        if b.xpath_namespaces.len() > 32 {
            return Err(CompileError::Invalid("at most 32 xpathNamespaces".into()));
        }
        let xp = crate::xml::parse_with_namespaces(x, &b.xpath_namespaces)
            .map_err(|e| CompileError::Invalid(format!("invalid XPath `{x}`: {e}")))?;
        BodyCheck::XPath(xp, value(budget)?, value_body(budget)?)
    } else if let Some(x) = &b.equal_to_xml {
        BodyCheck::EqualToXml(Arc::new(
            crate::xml::compile_expected(x, &b.exempted_comparisons, placeholders)
                .map_err(CompileError::Invalid)?,
        ))
    } else if let Some(x) = &b.binary_equal_to {
        BodyCheck::Binary(
            B64.decode(x.trim())
                .map_err(|e| CompileError::Invalid(format!("binaryEqualTo is not base64: {e}")))?,
        )
    } else if b.absent {
        BodyCheck::Absent
    } else {
        let p = b.json_path.as_deref().unwrap_or_default();
        let path = crate::jsonpath::CompiledJsonPath::compile(p, b.json_path_dialect)
            .map_err(|e| CompileError::JsonPath(p.to_string(), e))?;
        BodyCheck::JsonPath(path, value(budget)?, value_body(budget)?)
    };
    if b.value.is_some() && !matches!(check, BodyCheck::XPath(..) | BodyCheck::JsonPath(..)) {
        return Err(CompileError::Invalid(
            "value applies only to jsonPath and xpath".into(),
        ));
    }
    Ok(CompiledBody {
        check,
        negate: b.negate,
    })
}

fn numbers_equal(e: &serde_json::Number, a: &serde_json::Number) -> bool {
    match (e.as_i64(), a.as_i64(), e.as_u64(), a.as_u64()) {
        (Some(x), Some(y), _, _) => x == y,
        (_, _, Some(x), Some(y)) => x == y,
        _ if e.is_f64() || a.is_f64() => e.as_f64() == a.as_f64(),
        _ => false,
    }
}

pub fn json_subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => e
            .iter()
            .all(|(k, ev)| a.get(k).is_some_and(|av| json_subset(ev, av))),
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(x, y)| json_subset(x, y))
        }
        (Value::Number(e), Value::Number(a)) => numbers_equal(e, a),
        _ => expected == actual,
    }
}

impl JsonExpect {
    fn placeholder(&self, p: &str, a: Option<&Value>) -> Option<bool> {
        if !self.placeholders || !p.starts_with("${json-unit.") {
            return None;
        }
        Some(match p {
            "${json-unit.ignore-element}" => true,
            "${json-unit.ignore}" => a.is_some(),
            "${json-unit.any-string}" => a.is_some_and(Value::is_string),
            "${json-unit.any-number}" => a.is_some_and(Value::is_number),
            "${json-unit.any-boolean}" => a.is_some_and(Value::is_boolean),
            _ => {
                let re = p.strip_prefix(JSON_REGEX_PLACEHOLDER)?;
                match (self.regexes.get(re), a) {
                    (Some(r), Some(Value::String(s))) => r.is_match(s),
                    (Some(r), Some(v @ (Value::Number(_) | Value::Bool(_)))) => {
                        r.is_match(&v.to_string())
                    }
                    _ => false,
                }
            }
        })
    }

    fn check(&self, e: &Value, a: &Value, work: &mut usize) -> bool {
        *work += 1;
        if *work > MAX_JSON_COMPARE_STEPS {
            return false;
        }
        if let Value::String(s) = e
            && let Some(r) = self.placeholder(s, Some(a))
        {
            return r;
        }
        match (e, a) {
            (Value::Object(eo), Value::Object(ao)) => {
                for (k, ev) in eo {
                    match ao.get(k) {
                        Some(av) => {
                            if !self.check(ev, av, work) {
                                return false;
                            }
                        }
                        None => {
                            let ok = matches!(ev, Value::String(s) if self.placeholder(s, None) == Some(true));
                            if !ok {
                                return false;
                            }
                        }
                    }
                }
                self.lenient || ao.keys().all(|k| eo.contains_key(k))
            }
            (Value::Array(ea), Value::Array(aa)) => {
                if ea.len() != aa.len() && !(self.extra_items && ea.len() < aa.len()) {
                    return false;
                }
                if self.ignore_order {
                    if ea.iter().zip(aa).all(|(x, y)| self.check(x, y, work)) {
                        return true;
                    }
                    if ea.len() * aa.len() <= MAX_UNORDERED_PAIRS {
                        crate::xml::bipartite(ea.len(), aa.len(), |i, j| {
                            self.check(&ea[i], &aa[j], work)
                        })
                    } else {
                        let mut used = vec![false; aa.len()];
                        ea.iter().all(|x| {
                            match (0..aa.len()).find(|j| !used[*j] && self.check(x, &aa[*j], work))
                            {
                                Some(j) => {
                                    used[j] = true;
                                    true
                                }
                                None => false,
                            }
                        })
                    }
                } else {
                    ea.iter().zip(aa).all(|(x, y)| self.check(x, y, work))
                }
            }
            (Value::Number(x), Value::Number(y)) => numbers_equal(x, y),
            _ => e == a,
        }
    }
}

pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl CompiledBody {
    fn matches(&self, text: &str, raw: &[u8], json: Option<&Value>) -> bool {
        self.check.matches(text, raw, json) != self.negate
    }
}

fn node_matches(sub: &CompiledBody, node: &Value) -> bool {
    let text = value_text(node);
    let parsed = match node {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        _ => None,
    };
    let json = match node {
        Value::String(_) => parsed.as_ref(),
        other => Some(other),
    };
    sub.matches(&text, text.as_bytes(), json)
}

impl BodyCheck {
    fn matches(&self, text: &str, raw: &[u8], json: Option<&Value>) -> bool {
        match self {
            BodyCheck::EqualTo(v) => text == v,
            BodyCheck::Contains(v) => text.contains(v.as_str()),
            BodyCheck::Matches(re) => re.is_match(text),
            BodyCheck::JsonSchema(v) => json.is_some_and(|j| v.0.is_valid(j)),
            BodyCheck::XPath(xp, value, sub) => match xp.select(text) {
                None => false,
                Some(found) if sub.is_some() => found.iter().any(|t| {
                    let parsed = serde_json::from_str::<Value>(t).ok();
                    sub.as_ref()
                        .is_some_and(|b| b.matches(t, t.as_bytes(), parsed.as_ref()))
                }),
                Some(found) => match value {
                    None => !found.is_empty(),
                    Some(m) if found.is_empty() => m.absent == Some(true),
                    Some(m)
                        if m.has_exactly.is_some()
                            || m.includes.is_some()
                            || m.all_values.is_some() =>
                    {
                        m.matches_values(&found)
                    }
                    Some(m) => found.iter().any(|f| m.matches(Some(f))),
                },
            },
            BodyCheck::EqualToJson(x) => json.is_some_and(|j| x.check(&x.value, j, &mut 0)),
            BodyCheck::JsonPath(path, value, sub) => {
                let Some(j) = json else {
                    return false;
                };
                let found = path.query(j);
                if let Some(sub) = sub {
                    return found.iter().any(|n| node_matches(sub, n))
                        || (found.len() > 1
                            && node_matches(
                                sub,
                                &Value::Array(found.iter().map(|n| n.as_ref().clone()).collect()),
                            ));
                }
                match value {
                    None => !found.is_empty(),
                    Some(m) if found.is_empty() => m.absent == Some(true),
                    Some(m)
                        if m.has_exactly.is_some()
                            || m.includes.is_some()
                            || m.all_values.is_some() =>
                    {
                        let texts: Vec<String> = found.iter().map(|v| value_text(v)).collect();
                        m.matches_values(&texts)
                    }
                    Some(m) => found.iter().any(|v| m.matches(Some(&value_text(v)))),
                }
            }
            BodyCheck::EqualToXml(x) => x.matches(text),
            BodyCheck::Binary(b) => raw == b.as_slice(),
            BodyCheck::Absent => raw.is_empty(),
            BodyCheck::GraphQlRootField(m) => crate::graphql::request_root_fields(text, json)
                .iter()
                .any(|f| m.matches(Some(f))),
        }
    }
}

#[derive(Debug, Clone)]
struct CompiledPart {
    name: Option<String>,
    filename: Option<CompiledString>,
    headers: Vec<(String, CompiledString)>,
    body: Vec<CompiledBody>,
}

impl CompiledPart {
    fn matches(&self, p: &MultipartPart) -> bool {
        if let Some(n) = &self.name
            && p.name.as_deref() != Some(n.as_str())
        {
            return false;
        }
        if let Some(f) = &self.filename
            && !f.matches(p.filename.as_deref())
        {
            return false;
        }
        if !self
            .headers
            .iter()
            .all(|(k, m)| m.matches_values(&p.header_all(k)))
        {
            return false;
        }
        if self.body.is_empty() {
            return true;
        }
        let text = String::from_utf8_lossy(&p.body);
        let json = crate::request::parse_json_bounded(&p.body);
        self.body
            .iter()
            .all(|b| b.matches(&text, &p.body, json.as_ref()))
    }
}

#[derive(Debug, Clone)]
pub struct CompiledStub {
    pub stub: Stub,
    pub uses_original_request: bool,
    method: Option<String>,
    path: PathMatch,
    path_params: Vec<(String, CompiledString)>,
    query: Vec<(String, CompiledString)>,
    headers: Vec<(String, CompiledString)>,
    cookies: Vec<(String, CompiledString)>,
    host: Option<CompiledString>,
    port: Option<u16>,
    scheme: Option<String>,
    basic_auth: Option<(String, String)>,
    form: Vec<(String, CompiledString)>,
    multipart: Vec<CompiledPart>,
    multipart_all: bool,
    body: Vec<CompiledBody>,
    graphql_root: Option<CompiledString>,
    /// Body matchers of `request.mcp`, with the text near misses show.
    mcp: Vec<(String, CompiledBody)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NearMiss {
    pub stub_id: uuid::Uuid,
    pub name: Option<String>,
    pub score: f64,
    pub mismatches: Vec<String>,
}

fn compile_map(
    m: &BTreeMap<String, StringMatcher>,
    lower: bool,
    budget: &mut Budget,
) -> Result<Vec<(String, CompiledString)>, CompileError> {
    m.iter()
        .map(|(k, v)| {
            Ok((
                if lower {
                    k.to_ascii_lowercase()
                } else {
                    k.clone()
                },
                CompiledString::compile_with(v, 0, budget)?,
            ))
        })
        .collect()
}

fn compile_part(p: &MultipartMatcher, budget: &mut Budget) -> Result<CompiledPart, CompileError> {
    budget.take(0)?;
    Ok(CompiledPart {
        name: p.name.clone(),
        filename: p
            .filename
            .as_ref()
            .map(|f| CompiledString::compile_with(f, 1, budget))
            .transpose()?,
        headers: compile_map(&p.headers, true, budget)?,
        body: p
            .body
            .iter()
            .map(|b| compile_body(b, 1, budget))
            .collect::<Result<_, _>>()?,
    })
}

fn mentions_original(v: &impl Serialize) -> bool {
    serde_json::to_string(v)
        .map(|x| x.contains("originalRequest"))
        .unwrap_or(true)
}

impl CompiledStub {
    pub fn compile(stub: &Stub) -> Result<Self, CompileError> {
        let p: &RequestPattern = &stub.request;
        let set = [
            p.url_path.is_some(),
            p.url_path_pattern.is_some(),
            p.url_path_template.is_some(),
            p.url.is_some(),
            p.url_pattern.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if set > 1 {
            return Err(CompileError::Invalid(
                "use only one of url, urlPattern, urlPath, urlPathPattern, urlPathTemplate".into(),
            ));
        }
        let path = if let Some(x) = &p.url_path {
            PathMatch::Exact(x.clone())
        } else if let Some(x) = &p.url_path_pattern {
            PathMatch::Regex(regex(x, false)?)
        } else if let Some(x) = &p.url_path_template {
            PathMatch::Template(compile_template(x)?)
        } else if let Some(x) = &p.url {
            PathMatch::Url(x.clone())
        } else if let Some(x) = &p.url_pattern {
            PathMatch::UrlRegex(regex(x, false)?)
        } else {
            PathMatch::Any
        };
        let part_matchers: usize = p
            .multipart
            .iter()
            .map(|m| 1 + m.headers.len() + m.body.len() + usize::from(m.filename.is_some()))
            .sum();
        let mcp_matchers = match &p.mcp {
            Some(m) => crate::mcp::body_matchers(m).map_err(CompileError::Invalid)?,
            None => Vec::new(),
        };
        let matcher_count = mcp_matchers.len()
            + p.path_params.len()
            + p.query.len()
            + p.headers.len()
            + p.body.len()
            + p.cookies.len()
            + p.form_parameters.len()
            + part_matchers
            + usize::from(p.host.is_some())
            + usize::from(p.graphql.as_ref().is_some_and(|g| g.root_field.is_some()))
            + usize::from(p.basic_auth.is_some());
        if matcher_count > MAX_MATCHERS_PER_STUB {
            return Err(CompileError::Invalid(format!(
                "a stub may have at most {MAX_MATCHERS_PER_STUB} matchers, found {matcher_count}"
            )));
        }
        if p.multipart.len() > MAX_MULTIPART_PATTERNS {
            return Err(CompileError::Invalid(format!(
                "at most {MAX_MULTIPART_PATTERNS} multipart patterns"
            )));
        }
        if p.multipart_matching_type.is_some() && p.multipart.is_empty() {
            return Err(CompileError::Invalid(
                "multipartMatchingType needs multipart patterns".into(),
            ));
        }
        if !p.path_params.is_empty() && !matches!(path, PathMatch::Template(_)) {
            return Err(CompileError::Invalid(
                "pathParams require urlPathTemplate".into(),
            ));
        }
        let scheme = p.scheme.as_ref().map(|s| s.to_ascii_lowercase());
        if scheme
            .as_deref()
            .is_some_and(|s| s != "http" && s != "https")
        {
            return Err(CompileError::Invalid("scheme must be http or https".into()));
        }
        let method = p
            .method
            .as_ref()
            .map(|m| m.to_ascii_uppercase())
            .or_else(|| p.mcp.as_ref().map(|_| "POST".to_string()))
            .filter(|m| m != "ANY");
        let uses_original_request = mentions_original(&stub.response)
            || mentions_original(&stub.responses)
            || mentions_original(&stub.callbacks)
            || mentions_original(&stub.messages)
            || stub
                .scenario
                .as_ref()
                .is_some_and(|x| x.name.contains("originalRequest"));
        let mut budget = Budget::new();
        let mut body: Vec<CompiledBody> = p
            .body
            .iter()
            .map(|b| compile_body(b, 0, &mut budget))
            .collect::<Result<_, _>>()?;
        if let Some(g) = &p.graphql {
            for (path, m) in [
                ("$.operationName", &g.operation_name),
                ("$.query", &g.query),
            ] {
                if let Some(m) = m {
                    body.push(compile_body(
                        &BodyMatcher {
                            json_path: Some(path.into()),
                            value: Some(m.clone()),
                            ..Default::default()
                        },
                        0,
                        &mut budget,
                    )?);
                }
            }
            if let Some(m) = &g.root_field {
                body.push(CompiledBody {
                    check: BodyCheck::GraphQlRootField(CompiledString::compile(m)?),
                    negate: false,
                });
            }
        }
        let mcp = mcp_matchers
            .into_iter()
            .map(|(label, b)| Ok((label, compile_body(&b, 0, &mut budget)?)))
            .collect::<Result<Vec<_>, CompileError>>()?;
        Ok(Self {
            stub: stub.clone(),
            uses_original_request,
            mcp,
            method,
            path,
            path_params: compile_map(&p.path_params, false, &mut budget)?,
            query: compile_map(&p.query, false, &mut budget)?,
            headers: compile_map(&p.headers, true, &mut budget)?,
            cookies: compile_map(&p.cookies, false, &mut budget)?,
            host: p
                .host
                .as_ref()
                .map(|h| {
                    let mut h = h.clone();
                    h.case_insensitive = true;
                    CompiledString::compile_with(&h, 0, &mut budget)
                })
                .transpose()?,
            port: p.port,
            scheme,
            basic_auth: p
                .basic_auth
                .as_ref()
                .map(|b| (b.username.clone(), b.password.clone())),
            form: compile_map(&p.form_parameters, false, &mut budget)?,
            multipart: p
                .multipart
                .iter()
                .map(|m| compile_part(m, &mut budget))
                .collect::<Result<_, _>>()?,
            multipart_all: p.multipart_matching_type == Some(MultipartMatchingType::All),
            graphql_root: p
                .graphql
                .as_ref()
                .and_then(|g| g.root_field.as_ref())
                .map(|m| CompiledString::compile_with(m, 0, &mut budget))
                .transpose()?,
            body,
        })
    }

    fn match_path(&self, req: &CapturedRequest) -> Option<BTreeMap<String, String>> {
        let path = req.path.as_str();
        match &self.path {
            PathMatch::Any => Some(BTreeMap::new()),
            PathMatch::Exact(p) => (p == path).then(BTreeMap::new),
            PathMatch::Regex(re) => re.is_match(path).then(BTreeMap::new),
            PathMatch::Template(segs) => match_template(segs, path),
            PathMatch::Url(u) => (*u == req.url()).then(BTreeMap::new),
            PathMatch::UrlRegex(re) => re.is_match(&req.url()).then(BTreeMap::new),
        }
    }

    /// Whether the request reaches this stub's URL, host, port and scheme (method and body aside).
    pub fn endpoint_matches(&self, req: &CapturedRequest) -> bool {
        self.match_path(req).is_some() && !self.origin_ok(req)[..3].contains(&Some(false))
    }

    fn multipart_ok(&self, req: &CapturedRequest) -> bool {
        if self.multipart.is_empty() {
            return true;
        }
        let Some(parts) = req.multipart_parts() else {
            return false;
        };
        if self.multipart_all {
            !parts.is_empty()
                && self
                    .multipart
                    .iter()
                    .all(|m| parts.iter().all(|p| m.matches(p)))
        } else {
            self.multipart
                .iter()
                .all(|m| parts.iter().any(|p| m.matches(p)))
        }
    }

    fn origin_ok(&self, req: &CapturedRequest) -> [Option<bool>; 4] {
        [
            self.scheme.as_ref().map(|s| *s == req.scheme),
            self.port.map(|p| req.effective_port() == Some(p)),
            self.host.as_ref().map(|h| h.matches(req.host.as_deref())),
            self.basic_auth.as_ref().map(|(u, p)| {
                req.basic_auth()
                    .is_some_and(|(au, ap)| au == *u && ap == *p)
            }),
        ]
    }

    pub fn match_request(
        &self,
        req: &CapturedRequest,
        json: Option<&Value>,
    ) -> Option<BTreeMap<String, String>> {
        if let Some(m) = &self.method
            && *m != req.method
        {
            return None;
        }
        let params = self.match_path(req)?;
        if self.origin_ok(req).contains(&Some(false)) {
            return None;
        }
        for (k, m) in &self.path_params {
            if !m.matches(params.get(k).map(String::as_str)) {
                return None;
            }
        }
        for (k, m) in &self.query {
            if !m.matches_values(&req.query_all(k)) {
                return None;
            }
        }
        for (k, m) in &self.headers {
            if !m.matches_values(&req.header_all(k)) {
                return None;
            }
        }
        if !self.cookies.is_empty() {
            for (k, m) in &self.cookies {
                if !m.matches_values(&req.cookie_all(k)) {
                    return None;
                }
            }
        }
        for (k, m) in &self.form {
            if !m.matches_values(&req.form_all(k)) {
                return None;
            }
        }
        if !self.multipart_ok(req) {
            return None;
        }
        if !self.body.is_empty() || self.graphql_root.is_some() || !self.mcp.is_empty() {
            let text = req.body_text();
            if !self.body.iter().all(|b| b.matches(&text, &req.body, json)) {
                return None;
            }
            if !self
                .mcp
                .iter()
                .all(|(_, b)| b.matches(&text, &req.body, json))
            {
                return None;
            }
            if let Some(m) = &self.graphql_root
                && !m.matches_values(&crate::graphql::request_root_fields(&text, json))
            {
                return None;
            }
        }
        Some(params)
    }

    pub fn specificity(&self) -> (u8, usize) {
        match &self.path {
            PathMatch::Exact(_) | PathMatch::Url(_) => (0, 0),
            PathMatch::Template(segs) => {
                let dynamic = segs
                    .iter()
                    .filter(|s| !matches!(s, Segment::Literal(_)))
                    .count();
                let rest = segs.iter().any(|s| matches!(s, Segment::Rest(_)));
                (if rest { 2 } else { 1 }, dynamic)
            }
            PathMatch::Regex(_) | PathMatch::UrlRegex(_) => (3, 0),
            PathMatch::Any => (4, 0),
        }
    }

    pub fn near_miss(&self, req: &CapturedRequest, json: Option<&Value>) -> NearMiss {
        let mut mismatches = Vec::new();
        let mut total = 0.0;
        let mut ok = 0.0;
        let mut score = |hit: bool, weight: f64, why: &dyn Fn() -> String, m: &mut Vec<String>| {
            total += weight;
            if hit {
                ok += weight;
            } else {
                m.push(why());
            }
        };
        score(
            self.method.as_ref().is_none_or(|m| *m == req.method),
            1.0,
            &|| {
                format!(
                    "method: expected {}, got {}",
                    self.method.clone().unwrap_or_default(),
                    req.method
                )
            },
            &mut mismatches,
        );
        let params = self.match_path(req);
        score(
            params.is_some(),
            2.0,
            &|| match &self.path {
                PathMatch::Url(_) | PathMatch::UrlRegex(_) => {
                    format!("url: `{}` does not match", req.url())
                }
                _ => format!("path: `{}` does not match", req.path),
            },
            &mut mismatches,
        );
        let names = ["scheme", "port", "host", "basic auth credentials"];
        for (i, r) in self.origin_ok(req).iter().enumerate() {
            if let Some(hit) = r {
                score(
                    *hit,
                    1.0,
                    &|| format!("{} does not match", names[i]),
                    &mut mismatches,
                );
            }
        }
        if let Some(p) = &params {
            for (k, m) in &self.path_params {
                score(
                    m.matches(p.get(k).map(String::as_str)),
                    1.0,
                    &|| format!("path parameter `{k}` does not match"),
                    &mut mismatches,
                );
            }
        }
        for (k, m) in &self.query {
            score(
                m.matches_values(&req.query_all(k)),
                1.0,
                &|| format!("query `{k}` does not match"),
                &mut mismatches,
            );
        }
        for (k, m) in &self.headers {
            score(
                m.matches_values(&req.header_all(k)),
                1.0,
                &|| format!("header `{k}` does not match"),
                &mut mismatches,
            );
        }
        for (k, m) in &self.cookies {
            score(
                m.matches_values(&req.cookie_all(k)),
                1.0,
                &|| format!("cookie `{k}` does not match"),
                &mut mismatches,
            );
        }
        for (k, m) in &self.form {
            score(
                m.matches_values(&req.form_all(k)),
                1.0,
                &|| format!("form parameter `{k}` does not match"),
                &mut mismatches,
            );
        }
        if !self.multipart.is_empty() {
            score(
                self.multipart_ok(req),
                1.0,
                &|| "multipart parts do not match".to_string(),
                &mut mismatches,
            );
        }
        if !self.body.is_empty() {
            let text = req.body_text();
            for (i, b) in self.body.iter().enumerate() {
                score(
                    b.matches(&text, &req.body, json),
                    1.0,
                    &|| format!("body matcher #{i} does not match"),
                    &mut mismatches,
                );
            }
        }
        if !self.mcp.is_empty() {
            let text = req.body_text();
            for (label, b) in &self.mcp {
                score(
                    b.matches(&text, &req.body, json),
                    1.0,
                    &|| label.clone(),
                    &mut mismatches,
                );
            }
        }
        if let Some(m) = &self.graphql_root {
            let fields = crate::graphql::request_root_fields(&req.body_text(), json);
            score(
                m.matches_values(&fields),
                1.0,
                &|| "GraphQL root field does not match".to_string(),
                &mut mismatches,
            );
        }
        NearMiss {
            stub_id: self.stub.id,
            name: self.stub.name.clone(),
            score: if total > 0.0 { ok / total } else { 0.0 },
            mismatches,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ResponseDef, Stub};
    use bytes::Bytes;
    use chrono::Utc;
    use serde_json::json;

    fn stub(req: serde_json::Value) -> CompiledStub {
        let s: Stub =
            serde_json::from_value(json!({"request": req, "response": {"status": 200}})).unwrap();
        CompiledStub::compile(&s).unwrap()
    }

    fn get(path: &str, query: Option<&str>) -> CapturedRequest {
        CapturedRequest::new(
            "GET",
            path,
            query,
            vec![("X-Env".into(), "stg".into())],
            Bytes::new(),
        )
    }

    #[test]
    fn template_extracts_params() {
        let s = stub(
            json!({"method": "GET", "urlPathTemplate": "/v1/usr/{code}", "pathParams": {"code": {"matches": "\\d+"}}}),
        );
        let p = s.match_request(&get("/v1/usr/123", None), None).unwrap();
        assert_eq!(p["code"], "123");
        assert!(s.match_request(&get("/v1/usr/abc", None), None).is_none());
        assert!(s.match_request(&get("/v1/usr/1/x", None), None).is_none());
        assert!(s.match_request(&get("/v1/usr/", None), None).is_none());
    }

    #[test]
    fn params_inside_segments_and_specificity() {
        let s = stub(json!({"urlPathTemplate": "/report.{format}"}));
        assert_eq!(
            s.match_request(&get("/report.csv", None), None).unwrap()["format"],
            "csv"
        );
        let g = stub(json!({"urlPathTemplate": "/v1/{name}:cancel"}));
        assert_eq!(
            g.match_request(&get("/v1/op-7:cancel", None), None)
                .unwrap()["name"],
            "op-7"
        );
        assert!(g.match_request(&get("/v1/op-7", None), None).is_none());
        let lit = stub(json!({"urlPath": "/users/me"}));
        let tpl = stub(json!({"urlPathTemplate": "/users/{id}"}));
        assert!(lit.specificity() < tpl.specificity());
    }

    #[test]
    fn patterns_cannot_escape_the_anchors() {
        assert!(CompiledStub::compile(&serde_json::from_value(json!({"request": {"urlPathPattern": "/only)|(.*"}, "response": {"status": 200}})).unwrap()).is_err());
    }

    #[test]
    fn big_integers_compare_exactly() {
        assert!(!json_subset(
            &json!(9007199254740993u64),
            &json!(9007199254740992u64)
        ));
        assert!(json_subset(&json!(6), &json!(6.0)));
    }

    #[test]
    fn rest_segment() {
        let s = stub(json!({"urlPathTemplate": "/files/{rest...}"}));
        assert_eq!(
            s.match_request(&get("/files/a/b/c", None), None).unwrap()["rest"],
            "a/b/c"
        );
    }

    #[test]
    fn regex_is_anchored() {
        let s = stub(json!({"urlPathPattern": "/a/\\d+"}));
        assert!(s.match_request(&get("/a/12", None), None).is_some());
        assert!(s.match_request(&get("/x/a/12", None), None).is_none());
    }

    #[test]
    fn query_header_and_absent() {
        let s = stub(
            json!({"query": {"contractorCode": {"equalTo": "1"}, "debug": {"absent": true}}, "headers": {"x-env": {"equalTo": "STG", "caseInsensitive": true}}}),
        );
        assert!(
            s.match_request(&get("/s", Some("contractorCode=1")), None)
                .is_some()
        );
        assert!(
            s.match_request(&get("/s", Some("contractorCode=1&debug=1")), None)
                .is_none()
        );
        assert!(
            s.match_request(&get("/s", Some("contractorCode=2")), None)
                .is_none()
        );
    }

    #[test]
    fn body_json_matchers() {
        let s = stub(json!({"method": "POST", "body": [
            {"jsonPath": "$.dest", "value": {"matches": "380\\d{9}"}},
            {"equalToJson": {"type": 6}, "ignoreExtraElements": true}
        ]}));
        let body = json!({"dest": "380501234567", "type": 6.0, "x": 1});
        let req = CapturedRequest::new("POST", "/", None, vec![], Bytes::from(body.to_string()));
        assert!(s.match_request(&req, Some(&body)).is_some());
        let bad = json!({"dest": "123", "type": 6});
        assert!(s.match_request(&req, Some(&bad)).is_none());
    }

    #[test]
    fn rejects_ambiguous_definitions() {
        let s: Stub = serde_json::from_value(
            json!({"request": {"urlPath": "/a", "urlPathPattern": "/a"}, "response": {}}),
        )
        .unwrap();
        assert!(CompiledStub::compile(&s).is_err());
        let s: Stub = serde_json::from_value(
            json!({"request": {"body": [{"contains": "a", "matches": "b"}]}, "response": {}}),
        )
        .unwrap();
        assert!(CompiledStub::compile(&s).is_err());
        let s: Stub =
            serde_json::from_value(json!({"request": {"urlPathPattern": "("}, "response": {}}))
                .unwrap();
        assert!(CompiledStub::compile(&s).is_err());
        let _ = ResponseDef::default();
    }

    fn req(
        method: &str,
        path: &str,
        query: Option<&str>,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> CapturedRequest {
        CapturedRequest::new(
            method,
            path,
            query,
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            Bytes::copy_from_slice(body),
        )
    }

    fn sm(v: serde_json::Value) -> CompiledString {
        CompiledString::compile(&serde_json::from_value(v).unwrap()).unwrap()
    }

    #[test]
    fn combinators_and_depth_limit() {
        let m = sm(
            json!({"or": [{"equalTo": "a"}, {"and": [{"contains": "x"}, {"not": {"contains": "y"}}]}]}),
        );
        assert!(m.matches(Some("a")));
        assert!(m.matches(Some("1x2")));
        assert!(!m.matches(Some("xy")));
        assert!(!m.matches(None));
        let maybe = sm(json!({"or": [{"absent": true}, {"equalTo": "1"}]}));
        assert!(maybe.matches(None));
        assert!(maybe.matches(Some("1")));
        assert!(!maybe.matches(Some("2")));
        let mut deep = json!({"equalTo": "a"});
        for _ in 0..12 {
            deep = json!({"not": deep});
        }
        let spec: StringMatcher = serde_json::from_value(deep).unwrap();
        assert!(CompiledString::compile(&spec).is_err());
        let wide: StringMatcher = serde_json::from_value(
            json!({"or": (0..300).map(|i| json!({"equalTo": i.to_string()})).collect::<Vec<_>>()}),
        )
        .unwrap();
        assert!(CompiledString::compile(&wide).is_err());
    }

    #[test]
    fn date_time_matchers() {
        let past = sm(json!({"before": "now"}));
        assert!(past.matches(Some("2001-01-01T00:00:00Z")));
        assert!(!past.matches(Some("2999-01-01T00:00:00Z")));
        assert!(!past.matches(Some("not a date")));
        let future = sm(json!({"after": "now +3 days"}));
        let in_week = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
        assert!(future.matches(Some(&in_week)));
        assert!(!future.matches(Some(&Utc::now().to_rfc3339())));
        let fmt =
            sm(json!({"equalToDateTime": "2021-06-14T00:00:00Z", "actualFormat": "dd/MM/yyyy"}));
        assert!(fmt.matches(Some("14/06/2021")));
        assert!(!fmt.matches(Some("2021-06-14")));
        let month = sm(
            json!({"equalToDateTime": "2021-06-01T00:00:00Z", "truncateActual": "first day of month"}),
        );
        assert!(month.matches(Some("2021-06-17T13:14:15Z")));
        let http = sm(json!({"before": "2020-01-01T00:00:00Z"}));
        assert!(http.matches(Some("Tue, 15 Nov 1994 08:12:31 GMT")));
        assert!(
            CompiledString::compile(&serde_json::from_value(json!({"before": "someday"})).unwrap())
                .is_err()
        );
        assert!(
            CompiledString::compile(
                &serde_json::from_value(json!({"actualFormat": "unix"})).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn multi_value_query_and_headers() {
        let s = stub(json!({
            "query": {"id": {"hasExactly": [{"equalTo": "1"}, {"equalTo": "2"}]}, "tag": {"includes": [{"equalTo": "a"}]}},
            "headers": {"x-v": {"includes": [{"matches": "v\\d"}]}}
        }));
        let ok = req(
            "GET",
            "/",
            Some("id=2&id=1&tag=b&tag=a"),
            &[("x-v", "a"), ("x-v", "v2")],
            b"",
        );
        assert!(s.match_request(&ok, None).is_some());
        let extra = req(
            "GET",
            "/",
            Some("id=2&id=1&id=3&tag=a"),
            &[("x-v", "v2")],
            b"",
        );
        assert!(s.match_request(&extra, None).is_none());
        let missing = req("GET", "/", Some("id=2&id=1"), &[("x-v", "v2")], b"");
        assert!(s.match_request(&missing, None).is_none());
        let second_header = stub(json!({"headers": {"accept": {"equalTo": "text/plain"}}}));
        assert!(
            second_header
                .match_request(
                    &req(
                        "GET",
                        "/",
                        None,
                        &[("accept", "a/b"), ("accept", "text/plain")],
                        b""
                    ),
                    None
                )
                .is_some()
        );
    }

    #[test]
    fn cookies_origin_and_basic_auth() {
        let s = stub(json!({
            "cookies": {"session": {"matches": "s-\\d+"}},
            "host": {"equalTo": "API.example.com"},
            "port": 8443,
            "scheme": "https",
            "basicAuth": {"username": "u", "password": "p:w"}
        }));
        let auth = format!("Basic {}", B64.encode("u:p:w"));
        let r = req(
            "GET",
            "/",
            None,
            &[
                ("cookie", "a=1; session=s-42"),
                ("host", "api.example.com:8443"),
                ("authorization", &auth),
            ],
            b"",
        )
        .with_origin("https", None, None);
        assert!(s.match_request(&r, None).is_some());
        let plain = r.clone().with_origin("http", None, None);
        assert!(s.match_request(&plain, None).is_none());
        let other_port = req(
            "GET",
            "/",
            None,
            &[
                ("cookie", "session=s-1"),
                ("host", "api.example.com"),
                ("authorization", &auth),
            ],
            b"",
        )
        .with_origin("https", None, None);
        assert!(s.match_request(&other_port, None).is_none());
        let nm = s.near_miss(&plain, None);
        assert!(
            nm.mismatches.iter().any(|m| m.contains("scheme")),
            "{:?}",
            nm.mismatches
        );
        let default_port = stub(json!({"port": 443}));
        assert!(
            default_port
                .match_request(
                    &req("GET", "/", None, &[("host", "x")], b"").with_origin("https", None, None),
                    None
                )
                .is_some()
        );
    }

    #[test]
    fn form_parameters() {
        let s = stub(
            json!({"formParameters": {"grant_type": {"equalTo": "client credentials"}, "scope": {"includes": [{"equalTo": "a"}, {"equalTo": "b"}]}}}),
        );
        let r = req(
            "POST",
            "/token",
            None,
            &[(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )],
            b"grant_type=client+credentials&scope=a&scope=b",
        );
        assert!(s.match_request(&r, None).is_some());
        let json = req(
            "POST",
            "/token",
            None,
            &[("content-type", "application/json")],
            b"grant_type=client+credentials&scope=a&scope=b",
        );
        assert!(s.match_request(&json, None).is_none());
    }

    #[test]
    fn multipart_any_and_all() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"meta\"\r\nContent-Type: application/json\r\n\r\n{\"kind\":\"invoice\"}\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4\r\n--XyZ--\r\n";
        let r = req(
            "POST",
            "/upload",
            None,
            &[("content-type", "multipart/form-data; boundary=XyZ")],
            body,
        );
        let any = stub(json!({"multipart": [
            {"name": "meta", "body": [{"jsonPath": "$.kind", "value": {"equalTo": "invoice"}}]},
            {"filename": {"matches": ".*\\.pdf"}, "headers": {"Content-Type": {"equalTo": "application/pdf"}}, "body": [{"contains": "%PDF"}]}
        ]}));
        assert!(any.match_request(&r, None).is_some());
        let all = stub(
            json!({"multipartMatchingType": "ALL", "multipart": [{"headers": {"content-disposition": {"contains": "form-data"}}}]}),
        );
        assert!(all.match_request(&r, None).is_some());
        let all_pdf = stub(
            json!({"multipartMatchingType": "ALL", "multipart": [{"filename": {"matches": ".*"}}]}),
        );
        assert!(all_pdf.match_request(&r, None).is_none());
        let not_multipart = req(
            "POST",
            "/upload",
            None,
            &[("content-type", "text/plain")],
            body,
        );
        assert!(any.match_request(&not_multipart, None).is_none());
        let many = format!(
            "{}--XyZ--",
            "--XyZ\r\nContent-Disposition: form-data; name=\"p\"\r\n\r\nx\r\n".repeat(100)
        );
        let big = req(
            "POST",
            "/upload",
            None,
            &[("content-type", "multipart/form-data; boundary=XyZ")],
            many.as_bytes(),
        );
        assert!(big.multipart_parts().is_none());
    }

    #[test]
    fn url_and_url_pattern() {
        let exact = stub(json!({"url": "/search?q=a%20b&page=2"}));
        assert!(
            exact
                .match_request(&get("/search", Some("q=a%20b&page=2")), None)
                .is_some()
        );
        assert!(
            exact
                .match_request(&get("/search", Some("page=2&q=a%20b")), None)
                .is_none()
        );
        let pat = stub(json!({"urlPattern": "/search\\?q=.*"}));
        assert!(
            pat.match_request(&get("/search", Some("q=1")), None)
                .is_some()
        );
        assert!(pat.match_request(&get("/search", None), None).is_none());
        assert_eq!(exact.specificity(), (0, 0));
        assert_eq!(pat.specificity(), (3, 0));
    }

    #[test]
    fn nested_unordered_arrays_are_bounded() {
        let n = 24;
        let leaf: Vec<i64> = (0..n).collect();
        let expected = vec![vec![leaf.clone(); n as usize]; n as usize];
        let s = stub(json!({"body": [{"equalToJson": expected, "ignoreArrayOrder": true}]}));
        let mut rev = leaf.clone();
        rev.reverse();
        let shuffled = json!(vec![vec![rev.clone(); n as usize]; n as usize]);
        let r = req("POST", "/", None, &[], shuffled.to_string().as_bytes());
        assert!(s.match_request(&r, Some(&shuffled)).is_some());
        let mut bad = vec![vec![rev.clone(); n as usize]; n as usize];
        bad[n as usize - 1][n as usize - 1][0] = -1;
        let bad = json!(bad);
        let started = std::time::Instant::now();
        assert!(s.match_request(&r, Some(&bad)).is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn json_placeholders_and_array_order() {
        let s = stub(json!({"body": [{"equalToJson": {
            "id": "${json-unit.any-number}", "name": "${json-unit.any-string}", "ok": "${json-unit.any-boolean}",
            "code": "${json-unit.regex}[A-Z]{2}\\d+", "meta": "${json-unit.ignore}", "opt": "${json-unit.ignore-element}",
            "tags": ["b", "a"]
        }, "ignoreArrayOrder": true}]}));
        let body = json!({"id": 7, "name": "x", "ok": false, "code": "AB12", "meta": {"a": 1}, "tags": ["a", "b"]});
        let r = req("POST", "/", None, &[], body.to_string().as_bytes());
        assert!(s.match_request(&r, Some(&body)).is_some());
        let bad = json!({"id": "7", "name": "x", "ok": false, "code": "AB12", "meta": 1, "tags": ["a", "b"]});
        assert!(s.match_request(&r, Some(&bad)).is_none());
        let missing_meta =
            json!({"id": 7, "name": "x", "ok": false, "code": "AB12", "tags": ["a", "b"]});
        assert!(s.match_request(&r, Some(&missing_meta)).is_none());
        let ordered = stub(json!({"body": [{"equalToJson": [1, 2]}]}));
        assert!(ordered.match_request(&r, Some(&json!([2, 1]))).is_none());
        let literal = stub(
            json!({"body": [{"equalToJson": {"a": "${json-unit.any-string}"}, "enablePlaceholders": false}]}),
        );
        assert!(
            literal
                .match_request(&r, Some(&json!({"a": "x"})))
                .is_none()
        );
    }

    #[test]
    fn xml_binary_and_absent_bodies() {
        let s =
            stub(json!({"body": [{"equalToXml": "<a><b>1</b><c x=\"${xmlunit.ignore}\"/></a>"}]}));
        let r = req(
            "POST",
            "/",
            None,
            &[],
            b"<a>\n <b>1</b>\n <c x=\"zz\"></c>\n</a>",
        );
        assert!(s.match_request(&r, None).is_some());
        let bin = stub(json!({"body": [{"binaryEqualTo": B64.encode([0u8, 159, 146, 150])}]}));
        assert!(
            bin.match_request(&req("POST", "/", None, &[], &[0, 159, 146, 150]), None)
                .is_some()
        );
        assert!(
            bin.match_request(&req("POST", "/", None, &[], &[0, 159]), None)
                .is_none()
        );
        let empty = stub(json!({"body": [{"absent": true}]}));
        assert!(
            empty
                .match_request(&req("GET", "/", None, &[], b""), None)
                .is_some()
        );
        assert!(
            empty
                .match_request(&req("GET", "/", None, &[], b"x"), None)
                .is_none()
        );
        let ns = stub(
            json!({"body": [{"xpath": "//p:id", "xpathNamespaces": {"p": "urn:p"}, "value": {"equalTo": "9"}}]}),
        );
        assert!(
            ns.match_request(
                &req(
                    "POST",
                    "/",
                    None,
                    &[],
                    b"<r xmlns:q=\"urn:p\"><q:id>9</q:id><id>8</id></r>"
                ),
                None
            )
            .is_some()
        );
        assert!(
            ns.match_request(&req("POST", "/", None, &[], b"<r><id>9</id></r>"), None)
                .is_none()
        );
        for bad in [
            json!({"body": [{"absent": true, "contains": "x"}]}),
            json!({"body": [{"contains": "x", "ignoreArrayOrder": true}]}),
            json!({"body": [{"binaryEqualTo": "***"}]}),
            json!({"body": [{"equalToXml": "<a>"}]}),
            json!({"url": "/a", "urlPath": "/a"}),
            json!({"scheme": "ftp"}),
            json!({"multipartMatchingType": "ALL"}),
        ] {
            let s: Stub = serde_json::from_value(json!({"request": bad, "response": {}})).unwrap();
            assert!(CompiledStub::compile(&s).is_err(), "{bad}");
        }
    }

    #[test]
    fn near_miss_reports_reasons() {
        let s = stub(json!({"method": "POST", "urlPath": "/a"}));
        let nm = s.near_miss(&get("/a", None), None);
        assert!(nm.score > 0.5 && nm.score < 1.0);
        assert_eq!(nm.mismatches.len(), 1);
    }
}
