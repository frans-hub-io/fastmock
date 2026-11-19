use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions};

#[derive(Debug, Clone, PartialEq)]
enum Axis {
    Child,
    Descendant,
}

#[derive(Debug, Clone, PartialEq)]
enum Predicate {
    Index(usize),
    AttrEq(String, String),
    ChildEq(String, String),
    HasAttr(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Test {
    Name(String, Option<String>),
    Any,
    Attr(String),
    Text,
}

#[derive(Debug, Clone, PartialEq)]
struct Step {
    axis: Axis,
    test: Test,
    predicates: Vec<Predicate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XPath {
    steps: Vec<Step>,
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    let q = s.chars().next()?;
    if (q == '\'' || q == '"') && s.ends_with(q) && s.len() >= 2 {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

fn parse_predicate(p: &str) -> Result<Predicate, String> {
    let p = p.trim();
    if let Ok(n) = p.parse::<usize>() {
        if n == 0 {
            return Err("XPath positions start at 1".into());
        }
        return Ok(Predicate::Index(n));
    }
    if p.contains("!=") || p.contains('<') || p.contains('>') {
        return Err(format!(
            "unsupported predicate [{p}]: only = comparisons are supported"
        ));
    }
    if let Some((l, r)) = p.split_once('=') {
        let v = unquote(r).ok_or_else(|| format!("predicate value must be quoted: [{p}]"))?;
        let l = l.trim();
        return Ok(match l.strip_prefix('@') {
            Some(a) => Predicate::AttrEq(local(a).to_string(), v),
            None => Predicate::ChildEq(local(l).to_string(), v),
        });
    }
    if let Some(a) = p.strip_prefix('@') {
        return Ok(Predicate::HasAttr(local(a).to_string()));
    }
    Err(format!("unsupported predicate [{p}]"))
}

pub fn parse(expr: &str) -> Result<XPath, String> {
    parse_with_namespaces(expr, &BTreeMap::new())
}

fn qualified(n: &str, namespaces: &BTreeMap<String, String>) -> Result<Test, String> {
    let ns = match n.split_once(':') {
        Some((prefix, _)) if !namespaces.is_empty() => Some(
            namespaces
                .get(prefix)
                .cloned()
                .ok_or_else(|| format!("unknown namespace prefix `{prefix}`"))?,
        ),
        _ => None,
    };
    Ok(Test::Name(local(n).to_string(), ns))
}

pub fn parse_with_namespaces(
    expr: &str,
    namespaces: &BTreeMap<String, String>,
) -> Result<XPath, String> {
    let e = expr.trim();
    if !e.starts_with('/') {
        return Err("XPath must be absolute (start with / or //)".into());
    }
    let mut steps = Vec::new();
    let mut rest = e;
    while !rest.is_empty() {
        let axis = if let Some(r) = rest.strip_prefix("//") {
            rest = r;
            Axis::Descendant
        } else if let Some(r) = rest.strip_prefix('/') {
            rest = r;
            Axis::Child
        } else {
            return Err(format!("unexpected `{rest}`"));
        };
        let mut depth = 0;
        let end = rest
            .char_indices()
            .find(|(_, c)| {
                match c {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    '/' if depth == 0 => return true,
                    _ => {}
                }
                false
            })
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let token = &rest[..end];
        rest = &rest[end..];
        let (name, preds) = match token.find('[') {
            Some(i) => (&token[..i], &token[i..]),
            None => (token, ""),
        };
        let mut predicates = Vec::new();
        let mut p = preds;
        while let Some(stripped) = p.strip_prefix('[') {
            let close = stripped.find(']').ok_or("unclosed predicate")?;
            predicates.push(parse_predicate(&stripped[..close])?);
            p = &stripped[close + 1..];
        }
        if !p.is_empty() {
            return Err(format!("unexpected `{p}` after predicate"));
        }
        let test = match name {
            "" => return Err("empty step".into()),
            "*" => Test::Any,
            "text()" => Test::Text,
            n if n.starts_with('@') => Test::Attr(local(&n[1..]).to_string()),
            n if n
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')) =>
            {
                qualified(n, namespaces)?
            }
            n => return Err(format!("unsupported step `{n}`")),
        };
        if matches!(test, Test::Attr(_) | Test::Text) && !rest.is_empty() {
            return Err("@attribute and text() must be the last step".into());
        }
        steps.push(Step {
            axis,
            test,
            predicates,
        });
    }
    if steps.is_empty() {
        return Err("empty XPath".into());
    }
    Ok(XPath { steps })
}

pub const MAX_XML_NODES: u32 = 100_000;
const MAX_RESULTS: usize = 1_000;
const MAX_TEXT: usize = 1 << 20;
pub const MAX_SELECT_TEXT: usize = 2 << 20;

fn text_of(n: Node, budget: &mut usize) -> String {
    let mut out = String::new();
    for d in n.descendants().filter(|d| d.is_text()) {
        if let Some(t) = d.text() {
            if out.len() + t.len() > MAX_TEXT || t.len() > *budget {
                break;
            }
            *budget -= t.len();
            out.push_str(t);
        }
    }
    out
}

fn direct_text(n: Node) -> String {
    n.children()
        .filter(|c| c.is_text())
        .filter_map(|c| c.text())
        .collect()
}

fn keep(n: Node, p: &Predicate, budget: &mut usize) -> bool {
    match p {
        Predicate::Index(_) => true,
        Predicate::AttrEq(a, v) => n.attributes().any(|x| x.name() == a && x.value() == v),
        Predicate::HasAttr(a) => n.attributes().any(|x| x.name() == a),
        Predicate::ChildEq(c, v) => n.children().any(|ch| {
            ch.is_element() && ch.tag_name().name() == c && text_of(ch, budget).trim() == v
        }),
    }
}

fn descendant_or_self<'a, 'i>(current: &[Node<'a, 'i>]) -> Vec<Node<'a, 'i>> {
    let mut roots: Vec<Node> = current.to_vec();
    roots.sort_by_key(|n| n.id().get());
    let mut out = Vec::new();
    let mut covered_end = 0usize;
    for n in roots {
        let r = n.range();
        if r.end <= covered_end {
            continue;
        }
        covered_end = covered_end.max(r.end);
        out.extend(n.descendants().filter(|d| d.is_element() || d.is_root()));
    }
    out.sort_by_key(|n| n.id().get());
    out.dedup_by_key(|n| n.id().get());
    out
}

impl XPath {
    pub fn select(&self, xml: &str) -> Option<Vec<String>> {
        let opts = ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
            ..Default::default()
        };
        let doc = Document::parse_with_options(xml, opts).ok()?;
        let mut current: Vec<Node> = vec![doc.root()];
        let mut budget = MAX_SELECT_TEXT;
        let last = self.steps.len() - 1;
        for (i, step) in self.steps.iter().enumerate() {
            let context = if step.axis == Axis::Descendant {
                descendant_or_self(&current)
            } else {
                current.clone()
            };
            match &step.test {
                Test::Attr(a) => {
                    return Some(
                        context
                            .iter()
                            .filter_map(|e| {
                                e.attributes()
                                    .find(|x| x.name() == a)
                                    .map(|x| x.value().to_string())
                            })
                            .take(MAX_RESULTS)
                            .collect(),
                    );
                }
                Test::Text => {
                    return Some(
                        context
                            .iter()
                            .map(|n| direct_text(*n))
                            .take(MAX_RESULTS)
                            .collect(),
                    );
                }
                Test::Name(..) | Test::Any => {
                    let mut next = Vec::new();
                    for n in &context {
                        let mut matched: Vec<Node> = n
                            .children()
                            .filter(|c| c.is_element())
                            .filter(|c| match &step.test {
                                Test::Name(name, ns) => {
                                    c.tag_name().name() == name
                                        && ns
                                            .as_deref()
                                            .is_none_or(|u| c.tag_name().namespace() == Some(u))
                                }
                                _ => true,
                            })
                            .filter(|c| step.predicates.iter().all(|p| keep(*c, p, &mut budget)))
                            .collect();
                        if let Some(Predicate::Index(k)) = step
                            .predicates
                            .iter()
                            .find(|p| matches!(p, Predicate::Index(_)))
                        {
                            matched = matched.get(k - 1).map(|x| vec![*x]).unwrap_or_default();
                        }
                        next.extend(matched);
                    }
                    current = next;
                    if i == last {
                        return Some(
                            current
                                .iter()
                                .take(MAX_RESULTS)
                                .map(|n| text_of(*n, &mut budget))
                                .collect(),
                        );
                    }
                }
            }
        }
        Some(vec![])
    }
}

pub const MAX_XML_DEPTH: usize = 256;
const MAX_UNORDERED_PAIRS: usize = 10_000;
pub const MAX_XML_COMPARE_STEPS: usize = 500_000;
const MAX_XML_PLACEHOLDER_REGEXES: usize = 64;

const EXEMPTIONS: &[&str] = &[
    "NAMESPACE_URI",
    "NAMESPACE_PREFIX",
    "ELEMENT_TAG_NAME",
    "ATTR_VALUE",
    "ATTR_NAME_LOOKUP",
    "ELEMENT_NUM_ATTRIBUTES",
    "TEXT_VALUE",
    "CHILD_NODELIST_SEQUENCE",
    "CHILD_NODELIST_LENGTH",
    "CHILD_LOOKUP",
    "SCHEMA_LOCATION",
    "NO_NAMESPACE_SCHEMA_LOCATION",
    "XML_VERSION",
    "XML_STANDALONE",
    "XML_ENCODING",
    "HAS_DOCTYPE_DECLARATION",
    "DOCTYPE_NAME",
    "DOCTYPE_PUBLIC_ID",
    "DOCTYPE_SYSTEM_ID",
    "COMMENT_VALUE",
    "PROCESSING_INSTRUCTION_TARGET",
    "PROCESSING_INSTRUCTION_DATA",
    "NODE_TYPE",
];

const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";

#[derive(Debug, Clone, PartialEq)]
struct XmlElem {
    ns: Option<String>,
    name: String,
    attrs: Vec<(Option<String>, String, String)>,
    children: Vec<XmlChild>,
}

#[derive(Debug, Clone, PartialEq)]
enum XmlChild {
    Elem(XmlElem),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct ExpectedXml {
    root: XmlElem,
    exempt: HashSet<String>,
    placeholders: bool,
    regexes: HashMap<String, Regex>,
}

fn parse_doc(xml: &str) -> Result<Document<'_>, String> {
    let opts = ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_XML_NODES,
        ..Default::default()
    };
    Document::parse_with_options(xml, opts).map_err(|e| e.to_string())
}

fn significant_children<'a, 'i>(n: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    n.children()
        .filter(|c| {
            c.is_element() || (c.is_text() && c.text().is_some_and(|t| !t.trim().is_empty()))
        })
        .collect()
}

fn owned(n: Node, depth: usize) -> Result<XmlElem, String> {
    if depth > MAX_XML_DEPTH {
        return Err(format!("XML is nested deeper than {MAX_XML_DEPTH} levels"));
    }
    let mut children = Vec::new();
    for c in significant_children(n) {
        if c.is_element() {
            children.push(XmlChild::Elem(owned(c, depth + 1)?));
        } else {
            children.push(XmlChild::Text(c.text().unwrap_or("").trim().to_string()));
        }
    }
    Ok(XmlElem {
        ns: n.tag_name().namespace().map(String::from),
        name: n.tag_name().name().to_string(),
        attrs: n
            .attributes()
            .map(|a| {
                (
                    a.namespace().map(String::from),
                    a.name().to_string(),
                    a.value().to_string(),
                )
            })
            .collect(),
        children,
    })
}

fn placeholder_regex(text: &str) -> Option<&str> {
    text.strip_prefix("${xmlunit.matchesRegex(")
        .and_then(|r| r.strip_suffix(")}"))
}

fn collect_regexes(e: &XmlElem, out: &mut HashMap<String, Regex>) -> Result<(), String> {
    let texts = e
        .attrs
        .iter()
        .map(|a| a.2.as_str())
        .chain(e.children.iter().filter_map(|c| match c {
            XmlChild::Text(t) => Some(t.as_str()),
            XmlChild::Elem(_) => None,
        }));
    for t in texts {
        if let Some(re) = placeholder_regex(t) {
            if out.len() >= MAX_XML_PLACEHOLDER_REGEXES {
                return Err(format!(
                    "at most {MAX_XML_PLACEHOLDER_REGEXES} regex placeholders per XML matcher"
                ));
            }
            let compiled = regex::RegexBuilder::new(&format!("^(?:{re})$"))
                .size_limit(256 * 1024)
                .build()
                .map_err(|e| format!("invalid placeholder regex `{re}`: {e}"))?;
            out.insert(re.to_string(), compiled);
        }
    }
    for c in &e.children {
        if let XmlChild::Elem(x) = c {
            collect_regexes(x, out)?;
        }
    }
    Ok(())
}

pub fn compile_expected(
    xml: &str,
    exempted: &[String],
    placeholders: bool,
) -> Result<ExpectedXml, String> {
    let mut exempt = HashSet::new();
    for e in exempted {
        let up = e.trim().to_ascii_uppercase();
        if !EXEMPTIONS.contains(&up.as_str()) {
            return Err(format!("unknown exempted comparison `{e}`"));
        }
        exempt.insert(up);
    }
    let doc = parse_doc(xml).map_err(|e| format!("equalToXml is not valid XML: {e}"))?;
    let root = owned(doc.root_element(), 0)?;
    let mut regexes = HashMap::new();
    if placeholders {
        collect_regexes(&root, &mut regexes)?;
    }
    Ok(ExpectedXml {
        root,
        exempt,
        placeholders,
        regexes,
    })
}

impl ExpectedXml {
    fn ex(&self, name: &str) -> bool {
        self.exempt.contains(name)
    }

    fn value_eq(&self, expected: &str, actual: &str) -> bool {
        if self.placeholders && expected.starts_with("${xmlunit.") {
            match expected {
                "${xmlunit.ignore}" => return true,
                "${xmlunit.isNumber}" => return actual.trim().parse::<f64>().is_ok(),
                "${xmlunit.isDateTime}" => {
                    return crate::datetime::parse_datetime(actual).is_some();
                }
                _ => {
                    if let Some(re) = placeholder_regex(expected).and_then(|r| self.regexes.get(r))
                    {
                        return re.is_match(actual.trim());
                    }
                }
            }
        }
        expected == actual
    }

    pub fn matches(&self, actual: &str) -> bool {
        let Ok(doc) = parse_doc(actual) else {
            return false;
        };
        self.elem(&self.root, doc.root_element(), 0, &mut 0)
    }

    fn skip_attr(&self, ns: Option<&str>, name: &str) -> bool {
        ns == Some(XSI)
            && ((name == "schemaLocation" && self.ex("SCHEMA_LOCATION"))
                || (name == "noNamespaceSchemaLocation" && self.ex("NO_NAMESPACE_SCHEMA_LOCATION")))
    }

    fn elem(&self, e: &XmlElem, a: Node, depth: usize, work: &mut usize) -> bool {
        *work += 1;
        if depth > MAX_XML_DEPTH || *work > MAX_XML_COMPARE_STEPS {
            return false;
        }
        if !self.ex("ELEMENT_TAG_NAME") && e.name != a.tag_name().name() {
            return false;
        }
        if !self.ex("NAMESPACE_URI") && e.ns.as_deref() != a.tag_name().namespace() {
            return false;
        }
        let actual_attrs: Vec<_> = a
            .attributes()
            .filter(|x| !self.skip_attr(x.namespace(), x.name()))
            .collect();
        let expected_attrs: Vec<_> = e
            .attrs
            .iter()
            .filter(|x| !self.skip_attr(x.0.as_deref(), &x.1))
            .collect();
        let lookup_exempt = self.ex("ATTR_NAME_LOOKUP");
        for (ns, name, value) in &expected_attrs {
            let found = actual_attrs.iter().find(|x| {
                x.name() == name && (self.ex("NAMESPACE_URI") || x.namespace() == ns.as_deref())
            });
            match found {
                None if lookup_exempt => {}
                None => return false,
                Some(x) => {
                    if !self.ex("ATTR_VALUE") && !self.value_eq(value, x.value()) {
                        return false;
                    }
                }
            }
        }
        if expected_attrs.len() != actual_attrs.len()
            && !lookup_exempt
            && !self.ex("ELEMENT_NUM_ATTRIBUTES")
        {
            return false;
        }
        let actual_children = significant_children(a);
        let subset = self.ex("CHILD_NODELIST_LENGTH") || self.ex("CHILD_LOOKUP");
        if !subset && actual_children.len() != e.children.len() {
            return false;
        }
        if e.children.len() > actual_children.len() {
            return false;
        }
        let mut child = |x: &XmlChild, n: Node| -> bool {
            match x {
                XmlChild::Elem(ce) => n.is_element() && self.elem(ce, n, depth + 1, work),
                XmlChild::Text(t) => {
                    n.is_text()
                        && (self.ex("TEXT_VALUE")
                            || self.value_eq(t, n.text().unwrap_or("").trim()))
                }
            }
        };
        let unordered = self.ex("CHILD_NODELIST_SEQUENCE")
            && e.children.len() * actual_children.len() <= MAX_UNORDERED_PAIRS;
        if unordered || (subset && e.children.len() != actual_children.len()) {
            if e.children.len() == actual_children.len()
                && e.children
                    .iter()
                    .zip(actual_children.iter())
                    .all(|(x, n)| child(x, *n))
            {
                return true;
            }
            if e.children.len() * actual_children.len() > MAX_UNORDERED_PAIRS {
                return false;
            }
            return bipartite(e.children.len(), actual_children.len(), |i, j| {
                child(&e.children[i], actual_children[j])
            });
        }
        e.children
            .iter()
            .zip(actual_children.iter())
            .all(|(x, n)| child(x, *n))
    }
}

pub fn bipartite(n: usize, m: usize, mut ok: impl FnMut(usize, usize) -> bool) -> bool {
    if n > m {
        return false;
    }
    let mut adj = vec![Vec::new(); n];
    for (i, row) in adj.iter_mut().enumerate() {
        for j in 0..m {
            if ok(i, j) {
                row.push(j);
            }
        }
        if row.is_empty() {
            return false;
        }
    }
    let mut owner: Vec<Option<usize>> = vec![None; m];
    fn augment(
        i: usize,
        adj: &[Vec<usize>],
        owner: &mut [Option<usize>],
        seen: &mut [bool],
    ) -> bool {
        for &j in &adj[i] {
            if seen[j] {
                continue;
            }
            seen[j] = true;
            if owner[j].is_none_or(|k| augment(k, adj, owner, seen)) {
                owner[j] = Some(i);
                return true;
            }
        }
        false
    }
    for i in 0..n {
        let mut seen = vec![false; m];
        if !augment(i, &adj, &mut owner, &mut seen) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_to_xml_is_semantic() {
        let exp = compile_expected(
            r#"<a:order xmlns:a="urn:o" id="1" kind="x"><a:item>  one </a:item><a:item>two</a:item></a:order>"#,
            &[],
            true,
        )
        .unwrap();
        assert!(exp.matches(
            "<?xml version=\"1.0\"?>\n<b:order kind=\"x\" id=\"1\" xmlns:b=\"urn:o\">\n  <b:item>one</b:item>\n  <!-- c --><b:item>two</b:item>\n</b:order>"
        ));
        assert!(!exp.matches(r#"<order kind="x" id="1"><item>one</item><item>two</item></order>"#));
        assert!(!exp.matches(
            r#"<b:order xmlns:b="urn:o" id="1" kind="x"><b:item>two</b:item><b:item>one</b:item></b:order>"#
        ));
        let unordered = compile_expected(
            r#"<o><i>1</i><i>2</i></o>"#,
            &["CHILD_NODELIST_SEQUENCE".into()],
            true,
        )
        .unwrap();
        assert!(unordered.matches("<o><i>2</i><i>1</i></o>"));
        assert!(!unordered.matches("<o><i>2</i><i>2</i></o>"));
        assert!(compile_expected("<o/>", &["NOPE".into()], true).is_err());
        assert!(compile_expected("<o>", &[], true).is_err());
    }

    #[test]
    fn xml_placeholders() {
        let exp = compile_expected(
            "<r id=\"${xmlunit.ignore}\"><n>${xmlunit.isNumber}</n><c>${xmlunit.matchesRegex([A-Z]{3})}</c><t>${xmlunit.isDateTime}</t></r>",
            &[],
            true,
        )
        .unwrap();
        assert!(exp.matches("<r id=\"77\"><n>4.5</n><c>UAH</c><t>2024-01-01T00:00:00Z</t></r>"));
        assert!(!exp.matches("<r id=\"77\"><n>x</n><c>UAH</c><t>2024-01-01T00:00:00Z</t></r>"));
        assert!(!exp.matches("<r id=\"77\"><n>1</n><c>uah</c><t>2024-01-01T00:00:00Z</t></r>"));
        let literal = compile_expected("<r>${xmlunit.ignore}</r>", &[], false).unwrap();
        assert!(!literal.matches("<r>x</r>"));
    }

    #[test]
    fn deep_xml_is_bounded() {
        let deep = format!("{}{}", "<a>".repeat(400), "</a>".repeat(400));
        assert!(compile_expected(&deep, &[], true).is_err());
        let exp = compile_expected("<a><a/></a>", &[], true).unwrap();
        assert!(!exp.matches(&deep));
    }

    #[test]
    fn xpath_with_namespaces() {
        let ns = BTreeMap::from([("b".to_string(), "urn:balance".to_string())]);
        let x = parse_with_namespaces("//b:msisdn", &ns).unwrap();
        assert_eq!(x.select(SOAP).unwrap(), vec!["380501234567"]);
        let other = BTreeMap::from([("b".to_string(), "urn:other".to_string())]);
        assert!(
            parse_with_namespaces("//b:msisdn", &other)
                .unwrap()
                .select(SOAP)
                .unwrap()
                .is_empty()
        );
        assert!(parse_with_namespaces("//x:msisdn", &ns).is_err());
    }

    #[test]
    fn xpath_one_semantics_and_bounded_cost() {
        let x = parse("//i[1]").unwrap();
        assert_eq!(
            x.select("<r><a><i>a1</i><i>a2</i></a><b><i>b1</i></b></r>")
                .unwrap(),
            vec!["a1", "b1"]
        );
        let t = parse("/r/text()").unwrap();
        assert_eq!(t.select("<r>top<c>child</c></r>").unwrap(), vec!["top"]);
        assert!(parse("/r[a!='x']").is_err());
        let deep = format!("{}{}", "<a>".repeat(3000), "</a>".repeat(3000));
        let started = std::time::Instant::now();
        let n = parse("//a//a").unwrap().select(&deep).unwrap().len();
        assert_eq!(n, MAX_RESULTS);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn nested_unordered_children_are_bounded() {
        let n = 24;
        let leaf = |order: &mut dyn Iterator<Item = usize>| -> String {
            format!(
                "<c>{}</c>",
                order.map(|i| format!("<d>{i}</d>")).collect::<String>()
            )
        };
        let block = |inner: &str| format!("<b>{}</b>", inner.repeat(n));
        let expected = format!("<a>{}</a>", block(&leaf(&mut (0..n))).repeat(n));
        let x = compile_expected(&expected, &["CHILD_NODELIST_SEQUENCE".into()], true).unwrap();
        let shuffled = format!("<a>{}</a>", block(&leaf(&mut (0..n).rev())).repeat(n));
        assert!(x.matches(&shuffled));
        let bad_leaf = leaf(&mut (0..n).rev()).replacen("<d>23</d>", "<d>99</d>", 1);
        let bad = format!(
            "<a>{}<b>{}{bad_leaf}</b></a>",
            block(&leaf(&mut (0..n).rev())).repeat(n - 1),
            leaf(&mut (0..n).rev()).repeat(n - 1)
        );
        let started = std::time::Instant::now();
        assert!(!x.matches(&bad));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn xpath_text_budget_spans_all_results() {
        let text = "x".repeat(100 * 1024);
        let doc = format!("{}{text}{}", "<a>".repeat(1000), "</a>".repeat(1000));
        let out = parse("//a").unwrap().select(&doc).unwrap();
        assert_eq!(out.len(), MAX_RESULTS);
        let total: usize = out.iter().map(|s| s.len()).sum();
        assert!(total <= MAX_SELECT_TEXT, "{total}");
        assert_eq!(out[0].len(), text.len());
        let pred = format!("{}{text}{}", "<a><b>".repeat(500), "</b></a>".repeat(500));
        let out = parse("//a[b='nomatch']").unwrap().select(&pred).unwrap();
        assert!(out.is_empty());
    }

    const SOAP: &str = r#"<?xml version="1.0"?>
<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:bal="urn:balance">
  <soapenv:Header/>
  <soapenv:Body>
    <bal:getBalance version="2">
      <bal:msisdn>380501234567</bal:msisdn>
      <bal:items><bal:item code="a">1</bal:item><bal:item code="b">2</bal:item></bal:items>
    </bal:getBalance>
  </soapenv:Body>
</soapenv:Envelope>"#;

    fn sel(x: &str) -> Vec<String> {
        parse(x).unwrap().select(SOAP).unwrap()
    }

    #[test]
    fn namespace_agnostic_paths() {
        assert_eq!(
            sel("/Envelope/Body/getBalance/msisdn"),
            vec!["380501234567"]
        );
        assert_eq!(sel("//msisdn"), vec!["380501234567"]);
        assert_eq!(sel("//soapenv:Body/bal:getBalance/@version"), vec!["2"]);
        assert_eq!(sel("//item[@code='b']"), vec!["2"]);
        assert_eq!(sel("//items/item[2]/@code"), vec!["b"]);
        assert_eq!(
            sel("//getBalance[msisdn='380501234567']/@version"),
            vec!["2"]
        );
        assert_eq!(sel("//item/@code"), vec!["a", "b"]);
        assert!(sel("//nothing").is_empty());
        assert_eq!(
            sel("/Envelope/*/getBalance/msisdn/text()"),
            vec!["380501234567"]
        );
    }

    #[test]
    fn rejects_unsupported_and_unsafe() {
        assert!(parse("msisdn").is_err());
        assert!(parse("//a[position()>1]").is_err());
        assert!(parse("//@a/b").is_err());
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;">]><a>&lol2;</a>"#;
        assert!(
            parse("//a").unwrap().select(bomb).is_none(),
            "DTDs are refused"
        );
    }
}
