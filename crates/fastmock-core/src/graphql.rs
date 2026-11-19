use std::collections::{BTreeMap, HashSet};

use graphql_parser::schema::{Definition, Type, TypeDefinition, TypeExtension};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::microcks::{ApiExamples, ApiKind, ExampleTarget, example_stub};
use crate::model::{GraphQlMatcher, RequestPattern, ResponseDef, StringMatcher, Stub};

pub const MAX_SCHEMA_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SCHEMA_NESTING: usize = 64;
pub const MAX_QUERY_BYTES: usize = 1024 * 1024;
const MAX_TOKENS: usize = 200_000;
const MAX_ROOT_FIELDS: usize = 64;
const MAX_FRAGMENT_DEPTH: usize = 4;
const MAX_SAMPLE_DEPTH: usize = 5;
const MAX_SAMPLE_NODES: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok<'a> {
    Name(&'a str),
    P(u8),
    Spread,
}

fn tokens(q: &str) -> Option<Vec<Tok<'_>>> {
    if q.len() > MAX_QUERY_BYTES {
        return None;
    }
    let b = q.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            0xEF if b[i..].starts_with(&[0xEF, 0xBB, 0xBF]) => i += 3,
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                if b[i..].starts_with(b"\"\"\"") {
                    i += 3;
                    loop {
                        if i >= b.len() {
                            return None;
                        }
                        if b[i] == b'\\' && b[i..].starts_with(b"\\\"\"\"") {
                            i += 4;
                        } else if b[i..].starts_with(b"\"\"\"") {
                            i += 3;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    i += 1;
                    loop {
                        if i >= b.len() || b[i] == b'\n' {
                            return None;
                        }
                        if b[i] == b'\\' {
                            i += 2;
                        } else if b[i] == b'"' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                }
            }
            b'.' if b[i..].starts_with(b"...") => {
                out.push(Tok::Spread);
                i += 3;
            }
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b':' | b'@' | b'$' | b'=' | b'!' | b'|'
            | b'&' => {
                out.push(Tok::P(c));
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Tok::Name(&q[start..i]));
            }
            c if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' => {
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'.' | b'-' | b'+'))
                {
                    i += 1;
                }
            }
            _ => return None,
        }
        if out.len() > MAX_TOKENS {
            return None;
        }
    }
    Some(out)
}

#[derive(Default, Debug)]
struct Selection {
    fields: Vec<String>,
    spreads: Vec<String>,
}

fn collect(t: &[Tok], open: usize) -> (Selection, usize) {
    let mut sel = Selection::default();
    let mut stack: Vec<bool> = Vec::new();
    let mut parens = 0usize;
    let mut pending_transparent = false;
    let mut skip_names = 0usize;
    let mut eff = 0usize;
    let mut i = open;
    while i < t.len() {
        let tok = t[i];
        if parens > 0 {
            match tok {
                Tok::P(b'(') => parens += 1,
                Tok::P(b')') => parens -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        match tok {
            Tok::P(b'{') => {
                stack.push(pending_transparent);
                if !pending_transparent {
                    eff += 1;
                }
                pending_transparent = false;
                skip_names = 0;
            }
            Tok::P(b'}') => {
                if stack.pop() == Some(false) {
                    eff -= 1;
                }
                if stack.is_empty() {
                    return (sel, i + 1);
                }
            }
            Tok::P(b'(') => parens += 1,
            Tok::P(b'@') => skip_names = 1,
            Tok::Spread if eff == 1 => match t.get(i + 1) {
                Some(Tok::Name("on")) => {
                    pending_transparent = true;
                    i += 2;
                }
                Some(Tok::Name(n)) => {
                    sel.spreads.push((*n).to_string());
                    i += 1;
                }
                _ => pending_transparent = true,
            },
            Tok::Spread => {
                if matches!(t.get(i + 1), Some(Tok::Name("on"))) {
                    i += 2;
                } else if matches!(t.get(i + 1), Some(Tok::Name(_))) {
                    i += 1;
                }
            }
            Tok::Name(_) if skip_names > 0 => skip_names -= 1,
            Tok::Name(n) if eff == 1 => {
                if matches!(t.get(i + 1), Some(Tok::P(b':'))) {
                    if let Some(Tok::Name(f)) = t.get(i + 2) {
                        if sel.fields.len() < MAX_ROOT_FIELDS {
                            sel.fields.push((*f).to_string());
                        }
                        i += 2;
                    }
                } else if sel.fields.len() < MAX_ROOT_FIELDS {
                    sel.fields.push(n.to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    (sel, t.len())
}

fn skip_to_brace(t: &[Tok], mut i: usize) -> Option<usize> {
    let mut parens = 0usize;
    while i < t.len() {
        match t[i] {
            Tok::P(b'(') => parens += 1,
            Tok::P(b')') => parens = parens.saturating_sub(1),
            Tok::P(b'{') if parens == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn root_fields(query: &str, operation_name: Option<&str>) -> Vec<String> {
    let Some(t) = tokens(query) else {
        return vec![];
    };
    let mut ops: Vec<(Option<String>, Selection)> = Vec::new();
    let mut fragments: BTreeMap<String, Selection> = BTreeMap::new();
    let mut i = 0;
    while i < t.len() {
        match t[i] {
            Tok::P(b'{') => {
                let (sel, next) = collect(&t, i);
                ops.push((None, sel));
                i = next;
            }
            Tok::Name("query" | "mutation" | "subscription") => {
                let name = match t.get(i + 1) {
                    Some(Tok::Name(n)) => Some((*n).to_string()),
                    _ => None,
                };
                let Some(open) = skip_to_brace(&t, i + 1) else {
                    break;
                };
                let (sel, next) = collect(&t, open);
                ops.push((name, sel));
                i = next;
            }
            Tok::Name("fragment") => {
                let name = match t.get(i + 1) {
                    Some(Tok::Name(n)) => (*n).to_string(),
                    _ => break,
                };
                let Some(open) = skip_to_brace(&t, i + 1) else {
                    break;
                };
                let (sel, next) = collect(&t, open);
                fragments.insert(name, sel);
                i = next;
            }
            _ => i += 1,
        }
    }
    let chosen = match operation_name.filter(|n| !n.is_empty()) {
        Some(n) => ops.iter().find(|(name, _)| name.as_deref() == Some(n)),
        None => ops.first(),
    };
    let Some((_, sel)) = chosen else {
        return vec![];
    };
    let mut out = sel.fields.clone();
    let mut queue: Vec<(String, usize)> = sel.spreads.iter().map(|s| (s.clone(), 1)).collect();
    let mut seen = HashSet::new();
    while let Some((name, depth)) = queue.pop() {
        if depth > MAX_FRAGMENT_DEPTH || !seen.insert(name.clone()) {
            continue;
        }
        if let Some(f) = fragments.get(&name) {
            out.extend(f.fields.iter().cloned());
            queue.extend(f.spreads.iter().map(|s| (s.clone(), depth + 1)));
        }
    }
    out.retain(|f| f != "__typename");
    out.truncate(MAX_ROOT_FIELDS);
    out
}

pub fn request_root_fields(body: &str, json: Option<&Value>) -> Vec<String> {
    match json {
        Some(j) => match j.get("query").and_then(|q| q.as_str()) {
            Some(q) => root_fields(q, j.get("operationName").and_then(|o| o.as_str())),
            None => vec![],
        },
        None => root_fields(body, None),
    }
}

fn nesting_ok(sdl: &str) -> bool {
    let mut depth = 0usize;
    let mut in_comment = false;
    let mut in_string = false;
    let mut prev = '\0';
    for c in sdl.chars() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if c == '"' && prev != '\\' {
                in_string = false;
            }
            prev = c;
            continue;
        }
        match c {
            '#' => in_comment = true,
            '"' => in_string = true,
            '{' | '[' | '(' => {
                depth += 1;
                if depth > MAX_SCHEMA_NESTING {
                    return false;
                }
            }
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        prev = c;
    }
    true
}

pub fn detect(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with('{') || t.starts_with('<') || t.starts_with("openapi") {
        return false;
    }
    let head: String = t.chars().take(8192).collect();
    head.contains("microcksId")
        || head.contains("type Query")
        || head.contains("type Mutation")
        || head.contains("schema {")
        || head.contains("schema{")
}

#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    pub tag: String,
    pub path: String,
    pub name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Default)]
pub struct Imported {
    pub name: String,
    pub version: String,
    pub stubs: Vec<Stub>,
    pub warnings: Vec<String>,
    pub operations: Vec<String>,
}

type Fields = Vec<(String, Type<'static, String>)>;

enum Kind {
    Object(Fields),
    Enum(Vec<String>),
    Union(Vec<String>),
    Scalar,
    Input,
}

struct Sampler<'a> {
    types: &'a BTreeMap<String, Kind>,
    budget: usize,
}

impl Sampler<'_> {
    fn sample(
        &mut self,
        ty: &Type<'static, String>,
        depth: usize,
        stack: &mut Vec<String>,
    ) -> Value {
        if self.budget == 0 {
            return Value::Null;
        }
        self.budget -= 1;
        match ty {
            Type::NonNullType(inner) => self.sample(inner, depth, stack),
            Type::ListType(inner) => {
                if depth >= MAX_SAMPLE_DEPTH {
                    json!([])
                } else {
                    let v = self.sample(inner, depth, stack);
                    if v.is_null() { json!([]) } else { json!([v]) }
                }
            }
            Type::NamedType(n) => self.named(n, depth, stack),
        }
    }

    fn named(&mut self, n: &str, depth: usize, stack: &mut Vec<String>) -> Value {
        match n {
            "ID" => return json!("1"),
            "String" => return json!("string"),
            "Int" => return json!(1),
            "Float" => return json!(1.5),
            "Boolean" => return json!(true),
            _ => {}
        }
        let types = self.types;
        match types.get(n) {
            Some(Kind::Enum(values)) => values.first().map(|v| json!(v)).unwrap_or(Value::Null),
            Some(Kind::Scalar) => json!("string"),
            Some(Kind::Union(members)) => match members.first() {
                Some(m) => self.named(m, depth, stack),
                None => Value::Null,
            },
            Some(Kind::Object(fields)) => {
                if depth >= MAX_SAMPLE_DEPTH || stack.iter().any(|s| s == n) {
                    return Value::Null;
                }
                stack.push(n.to_string());
                let mut m = Map::new();
                for (f, t) in fields {
                    m.insert(f.clone(), self.sample(t, depth + 1, stack));
                }
                stack.pop();
                Value::Object(m)
            }
            Some(Kind::Input) | None => Value::Null,
        }
    }
}

fn microcks_id(sdl: &str) -> Option<(String, String)> {
    for line in sdl.lines().take(50) {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix('#')
            && let Some(id) = rest.trim().strip_prefix("microcksId:")
        {
            let (n, v) = id.rsplit_once(':')?;
            let (n, v) = (n.trim(), v.trim());
            if !n.is_empty() && !v.is_empty() {
                return Some((n.to_string(), v.to_string()));
            }
        }
    }
    None
}

pub fn import(
    sdl: &str,
    opts: &ImportOptions,
    examples: Option<&ApiExamples>,
) -> anyhow::Result<Imported> {
    if sdl.len() > MAX_SCHEMA_BYTES {
        anyhow::bail!("the schema exceeds {MAX_SCHEMA_BYTES} bytes");
    }
    if !nesting_ok(sdl) {
        anyhow::bail!("the schema nests deeper than {MAX_SCHEMA_NESTING} levels");
    }
    let doc = graphql_parser::schema::parse_schema::<String>(sdl)
        .map_err(|e| anyhow::anyhow!("invalid GraphQL schema: {e}"))?
        .into_static();
    let mut roots: BTreeMap<&str, String> = BTreeMap::from([
        ("query", "Query".to_string()),
        ("mutation", "Mutation".to_string()),
        ("subscription", "Subscription".to_string()),
    ]);
    let mut types: BTreeMap<String, Kind> = BTreeMap::new();
    let mut extensions: Vec<(String, Fields)> = Vec::new();
    for d in &doc.definitions {
        match d {
            Definition::SchemaDefinition(s) => {
                if let Some(q) = &s.query {
                    roots.insert("query", q.clone());
                }
                if let Some(m) = &s.mutation {
                    roots.insert("mutation", m.clone());
                }
                if let Some(x) = &s.subscription {
                    roots.insert("subscription", x.clone());
                }
            }
            Definition::TypeDefinition(t) => match t {
                TypeDefinition::Object(o) => {
                    types.insert(
                        o.name.clone(),
                        Kind::Object(
                            o.fields
                                .iter()
                                .map(|f| (f.name.clone(), f.field_type.clone()))
                                .collect(),
                        ),
                    );
                }
                TypeDefinition::Interface(o) => {
                    types.insert(
                        o.name.clone(),
                        Kind::Object(
                            o.fields
                                .iter()
                                .map(|f| (f.name.clone(), f.field_type.clone()))
                                .collect(),
                        ),
                    );
                }
                TypeDefinition::Enum(e) => {
                    types.insert(
                        e.name.clone(),
                        Kind::Enum(e.values.iter().map(|v| v.name.clone()).collect()),
                    );
                }
                TypeDefinition::Union(u) => {
                    types.insert(u.name.clone(), Kind::Union(u.types.clone()));
                }
                TypeDefinition::Scalar(s) => {
                    types.insert(s.name.clone(), Kind::Scalar);
                }
                TypeDefinition::InputObject(i) => {
                    types.insert(i.name.clone(), Kind::Input);
                }
            },
            Definition::TypeExtension(TypeExtension::Object(o)) => extensions.push((
                o.name.clone(),
                o.fields
                    .iter()
                    .map(|f| (f.name.clone(), f.field_type.clone()))
                    .collect(),
            )),
            _ => {}
        }
    }
    for (name, fields) in extensions {
        match types.get_mut(&name) {
            Some(Kind::Object(existing)) => existing.extend(fields),
            _ => {
                types.insert(name, Kind::Object(fields));
            }
        }
    }
    let (id_name, id_version) = microcks_id(sdl).unzip();
    let mut out = Imported {
        name: opts
            .name
            .clone()
            .or(id_name)
            .unwrap_or_else(|| "GraphQL API".into()),
        version: opts
            .version
            .clone()
            .or(id_version)
            .unwrap_or_else(|| "1.0".into()),
        ..Default::default()
    };
    let path = if opts.path.is_empty() {
        "/graphql".to_string()
    } else {
        opts.path.clone()
    };
    let mut sampler = Sampler {
        types: &types,
        budget: MAX_SAMPLE_NODES * 64,
    };
    let mut bases: BTreeMap<String, RequestPattern> = BTreeMap::new();
    for kind in ["query", "mutation", "subscription"] {
        let type_name = &roots[kind];
        let Some(Kind::Object(fields)) = types.get(type_name) else {
            continue;
        };
        if kind == "subscription" {
            out.warnings.push(format!(
                "{type_name}: subscriptions are not mocked over HTTP; {} field(s) skipped",
                fields.len()
            ));
            continue;
        }
        for (field, ty) in fields {
            sampler.budget = MAX_SAMPLE_NODES;
            let sample = sampler.sample(ty, 0, &mut Vec::new());
            let request = RequestPattern {
                method: Some("POST".into()),
                url_path: Some(path.clone()),
                graphql: Some(GraphQlMatcher {
                    root_field: Some(StringMatcher {
                        equal_to: Some(field.clone()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mut response = ResponseDef {
                templated: false,
                json_body: Some(json!({"data": {field.as_str(): sample}})),
                ..Default::default()
            };
            response
                .headers
                .insert("content-type".into(), "application/json".into());
            out.stubs.push(Stub {
                id: Uuid::now_v7(),
                name: Some(format!("{type_name}.{field}")),
                priority: 5,
                request: request.clone(),
                response,
                tags: vec![opts.tag.clone()],
                metadata: Some(json!({"operation": field, "operationType": kind})),
                enabled: true,
                ..Default::default()
            });
            out.operations.push(field.clone());
            bases.insert(field.clone(), request);
        }
    }
    if let Some(ex) = examples {
        for (op, list) in &ex.operations {
            let Some(base) = bases.get(op) else {
                out.warnings.push(format!(
                    "examples for `{op}` ignored: the schema has no such query or mutation"
                ));
                continue;
            };
            let target = ExampleTarget {
                kind: ApiKind::GraphQl,
                operation: op,
                request: base.clone(),
                content_type: Some("application/json".into()),
                priority: 4,
                tags: vec![opts.tag.clone()],
                delay_ms: None,
            };
            for e in list {
                if let Some(s) = example_stub(&target, e, &mut out.warnings) {
                    out.stubs.push(s);
                }
            }
        }
    }
    if out.stubs.is_empty() {
        out.warnings
            .push("the schema has no query or mutation fields".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::CompiledStub;
    use crate::request::CapturedRequest;

    #[test]
    fn root_fields_of_queries() {
        assert_eq!(root_fields("{ film(id: 1) { title } }", None), vec!["film"]);
        assert_eq!(
            root_fields(
                "query Q($id: ID!) { f: film(id: $id) @include(if: true) { title } allFilms { totalCount } }",
                None
            ),
            vec!["film", "allFilms"]
        );
        assert_eq!(
            root_fields(
                "query A { a } mutation B { addStar(filmId: \"x\") { id } }",
                Some("B")
            ),
            vec!["addStar"]
        );
        assert_eq!(
            root_fields(
                "query { ...Root ... on Query { inline } __typename } fragment Root on Query { viaFragment(x: \"{ not a field }\") { id } }",
                None
            ),
            vec!["inline", "viaFragment"]
        );
        assert_eq!(
            root_fields("# comment { x }\nquery { \"\"\"doc\"\"\" a }", None),
            Vec::<String>::new()
                .into_iter()
                .chain(["a".to_string()])
                .collect::<Vec<_>>()
        );
        assert!(root_fields("{ unterminated(\"x) }", None).is_empty());
        assert_eq!(
            root_fields("{ a { b @skip(if: true) } c }", None),
            vec!["a", "c"]
        );
        let deep = format!("{}{}", "{ a ".repeat(50_000), "}".repeat(50_000));
        let started = std::time::Instant::now();
        assert_eq!(root_fields(&deep, None), vec!["a"]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn imports_schema_with_examples() {
        let ex = match crate::microcks::parse_artifact(include_str!(
            "../tests/fixtures/films-examples.yaml"
        ))
        .unwrap()
        {
            crate::microcks::Artifact::Examples(e) => e,
            _ => panic!(),
        };
        let r = import(
            include_str!("../tests/fixtures/films.graphql"),
            &ImportOptions {
                tag: "films".into(),
                ..Default::default()
            },
            Some(&ex),
        )
        .unwrap();
        assert_eq!(
            (r.name.as_str(), r.version.as_str()),
            ("Movie Graph API", "1.0")
        );
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(
            r.operations,
            vec!["allFilms", "film", "addStar", "addReview"]
        );
        let film = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("Query.film"))
            .unwrap();
        let body = film.response.json_body.as_ref().unwrap();
        assert_eq!(body["data"]["film"]["genre"], "SCIFI");
        assert_eq!(body["data"]["film"]["reviews"][0]["film"], Value::Null);
        let mut compiled: Vec<CompiledStub> = r
            .stubs
            .iter()
            .map(|s| CompiledStub::compile(s).unwrap())
            .collect();
        compiled.sort_by_key(|c| c.stub.priority);
        let call = |body: Value| {
            let req = CapturedRequest::new(
                "POST",
                "/graphql",
                None,
                vec![],
                bytes::Bytes::from(body.to_string()),
            );
            compiled
                .iter()
                .find(|c| c.match_request(&req, req.json()).is_some())
                .map(|c| c.stub.response.json_body.clone().unwrap())
        };
        let hit = call(json!({"query": "query film($id: ID!) { film(id: $id) { title } }", "variables": {"id": "ZmlsbXM6Mg=="}})).unwrap();
        assert_eq!(hit["data"]["film"]["title"], "The Empire Strikes Back");
        let hit = call(json!({"query": "query { film(id: \"other\") { title } }"})).unwrap();
        assert_eq!(hit["data"]["film"]["title"], "string");
        let hit = call(json!({"query": "mutation { addStar(filmId: \"1\") { id } }"})).unwrap();
        assert!(hit["data"]["addStar"].is_object());
        assert!(call(json!({"query": "{ unknownField }"})).is_none());
    }

    #[test]
    fn rejects_hostile_schemas() {
        let deep = format!(
            "type Query {{ a: {}String{} }}",
            "[".repeat(100),
            "]".repeat(100)
        );
        assert!(import(&deep, &ImportOptions::default(), None).is_err());
        assert!(import("type Query {", &ImportOptions::default(), None).is_err());
        let recursive =
            "type Query { node: Node } type Node { id: ID! next: Node children: [Node!]! }";
        let r = import(recursive, &ImportOptions::default(), None).unwrap();
        assert_eq!(r.stubs.len(), 1);
        assert!(detect(recursive));
        assert!(!detect("{\"openapi\": \"3.0.0\"}"));
    }
}
