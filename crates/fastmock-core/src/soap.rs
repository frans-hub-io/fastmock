use std::collections::{BTreeMap, HashMap};

use roxmltree::{Document, Node, ParsingOptions};
use serde_json::json;
use uuid::Uuid;

use crate::microcks::{Flavor, convert_text, strip_xpath_declarations};
use crate::model::{BodyMatcher, RequestPattern, ResponseDef, ResponseMode, StringMatcher, Stub};

pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_NODES: u32 = 1_000_000;
pub const MAX_OPERATIONS: usize = 2_000;
const MAX_SKELETON_DEPTH: usize = 12;
const MAX_SKELETON_ELEMENTS: usize = 500;
const MAX_SKELETON_NESTING: usize = 64;
const MAX_SKELETON_STEPS: usize = 20_000;
pub const MAX_XML_DEPTH: usize = 256;

pub const SOAP11_ENV: &str = "http://schemas.xmlsoap.org/soap/envelope/";
pub const SOAP12_ENV: &str = "http://www.w3.org/2003/05/soap-envelope";
const WSDL_NS: &str = "http://schemas.xmlsoap.org/wsdl/";
const WSDL_SOAP11: &str = "http://schemas.xmlsoap.org/wsdl/soap/";
const WSDL_SOAP12: &str = "http://schemas.xmlsoap.org/wsdl/soap12/";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
const SOAPUI_NS: &str = "http://eviware.com/soapui/config";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoapVersion {
    V11,
    V12,
}

impl SoapVersion {
    pub fn content_type(&self) -> &'static str {
        match self {
            SoapVersion::V11 => "text/xml; charset=utf-8",
            SoapVersion::V12 => "application/soap+xml; charset=utf-8",
        }
    }

    fn envelope_ns(&self) -> &'static str {
        match self {
            SoapVersion::V11 => SOAP11_ENV,
            SoapVersion::V12 => SOAP12_ENV,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            SoapVersion::V11 => "SOAP 1.1",
            SoapVersion::V12 => "SOAP 1.2",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    pub tag: String,
    pub path: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub mock_service: Option<String>,
}

#[derive(Debug, Default)]
pub struct Imported {
    pub name: String,
    pub version: String,
    pub stubs: Vec<Stub>,
    pub warnings: Vec<String>,
    pub operations: Vec<String>,
}

fn parse(text: &str) -> anyhow::Result<Document<'_>> {
    if text.len() > MAX_DOCUMENT_BYTES {
        anyhow::bail!("the document exceeds {MAX_DOCUMENT_BYTES} bytes");
    }
    let opts = ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_NODES,
        ..Default::default()
    };
    let doc = Document::parse_with_options(text, opts)
        .map_err(|e| anyhow::anyhow!("invalid XML: {e}"))?;
    check_depth(&doc)?;
    Ok(doc)
}

fn check_depth(doc: &Document) -> anyhow::Result<()> {
    let mut depths: Vec<u32> = vec![0; doc.descendants().count() + 1];
    for n in doc.descendants().filter(|n| n.is_element()) {
        let parent = n.parent().map(|p| p.id().get_usize()).unwrap_or(0);
        let d = depths.get(parent).copied().unwrap_or(0) + 1;
        if d as usize > MAX_XML_DEPTH {
            anyhow::bail!("the document nests elements deeper than {MAX_XML_DEPTH} levels");
        }
        if let Some(slot) = depths.get_mut(n.id().get_usize()) {
            *slot = d;
        }
    }
    Ok(())
}

pub fn detect_wsdl(text: &str) -> bool {
    let head: String = text.chars().take(8192).collect();
    head.contains(WSDL_NS) && head.contains("definitions") && !head.contains(SOAPUI_NS)
}

pub fn detect_soapui(text: &str) -> bool {
    let head: String = text.chars().take(8192).collect();
    head.contains(SOAPUI_NS) && head.contains("soapui-project")
}

fn local(q: &str) -> &str {
    q.rsplit(':').next().unwrap_or(q)
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a, 'i>(n: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    n.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text_of(n: Node) -> String {
    n.children().filter_map(|c| c.text()).collect::<String>()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn builtin_sample(t: &str) -> Option<&'static str> {
    Some(match t {
        "string" | "normalizedString" | "token" | "Name" | "NCName" | "NMTOKEN" | "ID"
        | "IDREF" | "language" | "anySimpleType" => "string",
        "anyURI" => "https://example.com",
        "int" | "integer" | "long" | "short" | "byte" | "nonNegativeInteger"
        | "nonPositiveInteger" | "negativeInteger" | "unsignedLong" | "unsignedInt"
        | "unsignedShort" | "unsignedByte" => "0",
        "positiveInteger" => "1",
        "decimal" | "float" | "double" => "0.0",
        "boolean" => "true",
        "date" => "2026-01-01",
        "dateTime" => "2026-01-01T00:00:00Z",
        "time" => "00:00:00",
        "duration" => "PT0S",
        "gYear" => "2026",
        "base64Binary" | "hexBinary" | "QName" | "anyType" => "",
        _ => return None,
    })
}

struct Schemas<'a, 'i> {
    elements: HashMap<&'a str, Node<'a, 'i>>,
    complex: HashMap<&'a str, Node<'a, 'i>>,
    simple: HashMap<&'a str, Node<'a, 'i>>,
    prefixes: BTreeMap<String, String>,
}

impl<'a, 'i> Schemas<'a, 'i> {
    fn index(root: Node<'a, 'i>, warnings: &mut Vec<String>) -> Self {
        let mut s = Schemas {
            elements: HashMap::new(),
            complex: HashMap::new(),
            simple: HashMap::new(),
            prefixes: BTreeMap::new(),
        };
        for schema in root.descendants().filter(|n| {
            n.is_element()
                && n.tag_name().name() == "schema"
                && n.tag_name().namespace() == Some(XSD_NS)
        }) {
            let tns = schema
                .attribute("targetNamespace")
                .unwrap_or("")
                .to_string();
            let next = format!("ns{}", s.prefixes.len());
            s.prefixes.entry(tns).or_insert(next);
            for c in schema.children().filter(|c| c.is_element()) {
                let name = c.attribute("name");
                match (c.tag_name().name(), name) {
                    ("element", Some(n)) => {
                        s.elements.entry(n).or_insert(c);
                    }
                    ("complexType", Some(n)) => {
                        s.complex.entry(n).or_insert(c);
                    }
                    ("simpleType", Some(n)) => {
                        s.simple.entry(n).or_insert(c);
                    }
                    ("import" | "include", _) if c.attribute("schemaLocation").is_some() => {
                        warnings.push(format!(
                            "external schema `{}` is not fetched; types from it are left empty",
                            c.attribute("schemaLocation").unwrap_or("")
                        ));
                    }
                    _ => {}
                }
            }
        }
        s
    }

    fn schema_of(n: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
        n.ancestors()
            .find(|a| a.is_element() && a.tag_name().name() == "schema")
    }

    fn ns_decls(&self) -> String {
        self.prefixes
            .iter()
            .filter(|(ns, _)| !ns.is_empty())
            .map(|(ns, p)| format!(" xmlns:{p}=\"{}\"", xml_escape(ns)))
            .collect()
    }
}

struct Skeleton<'s, 'a, 'i> {
    schemas: &'s Schemas<'a, 'i>,
    out: String,
    count: usize,
    stack: Vec<String>,
    refs: Vec<&'a str>,
    nesting: usize,
    steps: usize,
}

impl<'s, 'a, 'i> Skeleton<'s, 'a, 'i> {
    fn new(schemas: &'s Schemas<'a, 'i>) -> Self {
        Skeleton {
            schemas,
            out: String::new(),
            count: 0,
            stack: Vec::new(),
            refs: Vec::new(),
            nesting: 0,
            steps: 0,
        }
    }

    fn enter(&mut self) -> bool {
        self.steps += 1;
        if self.steps > MAX_SKELETON_STEPS || self.nesting >= MAX_SKELETON_NESTING {
            return false;
        }
        self.nesting += 1;
        true
    }

    fn leave(&mut self) {
        self.nesting -= 1;
    }

    fn simple_sample(&self, st: Node<'a, 'i>, depth: usize) -> String {
        if depth > 8 {
            return String::new();
        }
        if let Some(r) = child(st, "restriction") {
            if let Some(e) = child(r, "enumeration").and_then(|e| e.attribute("value")) {
                return e.to_string();
            }
            if let Some(base) = r.attribute("base") {
                return self.type_sample(local(base), depth + 1);
            }
        }
        "string".into()
    }

    fn type_sample(&self, t: &str, depth: usize) -> String {
        if let Some(st) = self.schemas.simple.get(t) {
            return self.simple_sample(*st, depth);
        }
        builtin_sample(t).unwrap_or("?").to_string()
    }

    fn qualified_name(&self, el: Node<'a, 'i>, global: bool) -> String {
        let name = el.attribute("name").unwrap_or("element");
        let schema = Schemas::schema_of(el);
        let qualified = global
            || el.attribute("form") == Some("qualified")
            || (el.attribute("form").is_none()
                && schema.and_then(|s| s.attribute("elementFormDefault")) == Some("qualified"));
        let tns = schema
            .and_then(|s| s.attribute("targetNamespace"))
            .unwrap_or("");
        match self.schemas.prefixes.get(tns) {
            Some(p) if qualified && !tns.is_empty() => format!("{p}:{name}"),
            _ => name.to_string(),
        }
    }

    fn element(&mut self, el: Node<'a, 'i>, depth: usize, global: bool) {
        if self.count >= MAX_SKELETON_ELEMENTS || depth > MAX_SKELETON_DEPTH {
            return;
        }
        if !self.enter() {
            return;
        }
        self.element_in(el, depth, global);
        self.leave();
    }

    fn element_in(&mut self, el: Node<'a, 'i>, depth: usize, global: bool) {
        if let Some(r) = el.attribute("ref") {
            let name = local(r);
            if self.refs.contains(&name) {
                return;
            }
            if let Some(g) = self.schemas.elements.get(name).copied() {
                self.refs.push(name);
                self.element(g, depth, true);
                self.refs.pop();
            }
            return;
        }
        let optional = el.attribute("minOccurs") == Some("0");
        let ty = el.attribute("type").map(local);
        if let Some(t) = ty
            && self.schemas.complex.contains_key(t)
            && self.stack.iter().any(|s| s == t)
        {
            if !optional {
                let qn = self.qualified_name(el, global);
                self.out.push_str(&format!("<{qn}/>"));
            }
            return;
        }
        self.count += 1;
        let qn = self.qualified_name(el, global);
        let complex = match ty {
            Some(t) => self.schemas.complex.get(t).copied(),
            None => child(el, "complexType"),
        };
        if let Some(ct) = complex {
            if let Some(t) = ty {
                self.stack.push(t.to_string());
            }
            let mut attrs = String::new();
            let mut body = String::new();
            std::mem::swap(&mut body, &mut self.out);
            self.complex(ct, depth + 1, &mut attrs);
            std::mem::swap(&mut body, &mut self.out);
            if ty.is_some() {
                self.stack.pop();
            }
            self.out.push_str(&format!("<{qn}{attrs}>{body}</{qn}>"));
            return;
        }
        let sample = match (ty, child(el, "simpleType")) {
            (Some(t), _) => self.type_sample(t, 0),
            (None, Some(st)) => self.simple_sample(st, 0),
            (None, None) => String::new(),
        };
        self.out
            .push_str(&format!("<{qn}>{}</{qn}>", xml_escape(&sample)));
    }

    fn attributes(&self, n: Node<'a, 'i>, attrs: &mut String) {
        for a in children(n, "attribute") {
            if let Some(name) = a.attribute("name") {
                let v = a
                    .attribute("type")
                    .map(|t| self.type_sample(local(t), 0))
                    .or_else(|| a.attribute("fixed").map(String::from))
                    .unwrap_or_else(|| "string".into());
                attrs.push_str(&format!(" {name}=\"{}\"", xml_escape(&v)));
            }
        }
    }

    fn complex(&mut self, ct: Node<'a, 'i>, depth: usize, attrs: &mut String) {
        if !self.enter() {
            return;
        }
        self.complex_in(ct, depth, attrs);
        self.leave();
    }

    fn complex_in(&mut self, ct: Node<'a, 'i>, depth: usize, attrs: &mut String) {
        self.attributes(ct, attrs);
        for c in ct.children().filter(|c| c.is_element()) {
            match c.tag_name().name() {
                "sequence" | "all" => self.particles(c, depth),
                "choice" => {
                    if let Some(first) = c.children().find(|x| x.is_element()) {
                        self.particle(first, depth);
                    }
                }
                "complexContent" => {
                    if let Some(ext) = child(c, "extension").or_else(|| child(c, "restriction")) {
                        if let Some(base) = ext.attribute("base").map(local)
                            && let Some(b) = self.schemas.complex.get(base).copied()
                            && !self.stack.iter().any(|s| s == base)
                        {
                            self.stack.push(base.to_string());
                            self.complex(b, depth, attrs);
                            self.stack.pop();
                        }
                        self.complex(ext, depth, attrs);
                    }
                }
                "simpleContent" => {
                    if let Some(ext) = child(c, "extension").or_else(|| child(c, "restriction")) {
                        self.attributes(ext, attrs);
                        let base = ext.attribute("base").map(local).unwrap_or("string");
                        let sample = self.type_sample(base, 0);
                        self.out.push_str(&xml_escape(&sample));
                    }
                }
                _ => {}
            }
        }
    }

    fn particles(&mut self, group: Node<'a, 'i>, depth: usize) {
        for p in group.children().filter(|c| c.is_element()) {
            self.particle(p, depth);
        }
    }

    fn particle(&mut self, p: Node<'a, 'i>, depth: usize) {
        if !self.enter() {
            return;
        }
        self.particle_in(p, depth);
        self.leave();
    }

    fn particle_in(&mut self, p: Node<'a, 'i>, depth: usize) {
        match p.tag_name().name() {
            "element" => self.element(p, depth, false),
            "sequence" | "all" => self.particles(p, depth),
            "choice" => {
                if let Some(first) = p.children().find(|x| x.is_element()) {
                    self.particle(first, depth);
                }
            }
            _ => {}
        }
    }
}

fn envelope(version: SoapVersion, ns_decls: &str, body: &str) -> String {
    format!(
        "<soapenv:Envelope xmlns:soapenv=\"{}\"{ns_decls}><soapenv:Header/><soapenv:Body>{body}</soapenv:Body></soapenv:Envelope>",
        version.envelope_ns()
    )
}

fn fault_envelope(version: SoapVersion, ns_decls: &str, reason: &str, detail: &str) -> String {
    let reason = xml_escape(reason);
    let fault = match version {
        SoapVersion::V11 => format!(
            "<soapenv:Fault><faultcode>soapenv:Server</faultcode><faultstring>{reason}</faultstring><detail>{detail}</detail></soapenv:Fault>"
        ),
        SoapVersion::V12 => format!(
            "<soapenv:Fault><soapenv:Code><soapenv:Value>soapenv:Receiver</soapenv:Value></soapenv:Code><soapenv:Reason><soapenv:Text xml:lang=\"en\">{reason}</soapenv:Text></soapenv:Reason><soapenv:Detail>{detail}</soapenv:Detail></soapenv:Fault>"
        ),
    };
    envelope(version, ns_decls, &fault)
}

pub fn request_pattern(
    path: &str,
    version: SoapVersion,
    action: &str,
    root_element: Option<&str>,
    exclude_soap12: bool,
) -> RequestPattern {
    let mut req = RequestPattern {
        method: Some("POST".into()),
        url_path: Some(path.to_string()),
        ..Default::default()
    };
    let action = action.trim().trim_matches('"');
    match version {
        SoapVersion::V11 => {
            if !action.is_empty() {
                req.headers.insert(
                    "soapaction".into(),
                    StringMatcher {
                        matches: Some(format!("\"?{}\"?", regex::escape(action))),
                        ..Default::default()
                    },
                );
            }
            if exclude_soap12 {
                req.headers.insert(
                    "content-type".into(),
                    StringMatcher {
                        does_not_match: Some(".*soap\\+xml.*".into()),
                        case_insensitive: true,
                        ..Default::default()
                    },
                );
            }
        }
        SoapVersion::V12 => {
            req.headers.insert(
                "content-type".into(),
                StringMatcher {
                    matches: Some(if action.is_empty() {
                        ".*application/soap\\+xml.*".to_string()
                    } else {
                        format!(
                            ".*application/soap\\+xml.*action=\"?{}\"?.*",
                            regex::escape(action)
                        )
                    }),
                    case_insensitive: true,
                    ..Default::default()
                },
            );
        }
    }
    if let Some(root) = root_element.filter(|r| !r.is_empty())
        && crate::xml::parse(&format!("/Envelope/Body/{root}")).is_ok()
    {
        req.body.push(BodyMatcher {
            xpath: Some(format!("/Envelope/Body/{root}")),
            ..Default::default()
        });
    }
    req
}

fn path_of(location: &str) -> String {
    let after_scheme = location.split_once("://").map(|x| x.1).unwrap_or(location);
    let path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None => "/",
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    if path.is_empty() {
        "/".into()
    } else {
        path.to_string()
    }
}

fn stub(
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

type Parts = Vec<(String, Option<String>, Option<String>)>;
type InterfaceOps = HashMap<String, (String, Option<String>)>;

struct WsdlOperation {
    name: String,
    input_root: Option<String>,
    output_body: String,
    faults: Vec<(String, String)>,
}

struct WsdlBinding {
    version: SoapVersion,
    path: Option<String>,
    operations: Vec<(WsdlOperation, String)>,
}

struct Wsdl {
    service: Option<String>,
    definitions_name: Option<String>,
    ns_decls: String,
    bindings: Vec<WsdlBinding>,
}

fn analyse_wsdl(doc: &Document, warnings: &mut Vec<String>) -> anyhow::Result<Wsdl> {
    let root = doc.root_element();
    if root.tag_name().name() != "definitions" || root.tag_name().namespace() != Some(WSDL_NS) {
        anyhow::bail!("not a WSDL 1.1 document (root must be wsdl:definitions)");
    }
    let schemas = Schemas::index(root, warnings);
    let ns_decls = schemas.ns_decls();
    let tns = root.attribute("targetNamespace").unwrap_or("");
    let mut messages: HashMap<&str, Parts> = HashMap::new();
    for m in children(root, "message") {
        let Some(name) = m.attribute("name") else {
            continue;
        };
        let parts = children(m, "part")
            .map(|p| {
                (
                    p.attribute("name").unwrap_or("part").to_string(),
                    p.attribute("element").map(|e| local(e).to_string()),
                    p.attribute("type").map(|t| local(t).to_string()),
                )
            })
            .collect();
        messages.insert(name, parts);
    }
    let mut port_types: HashMap<&str, Node> = HashMap::new();
    for p in children(root, "portType") {
        if let Some(n) = p.attribute("name") {
            port_types.insert(n, p);
        }
    }
    let mut addresses: HashMap<String, String> = HashMap::new();
    let mut service_name = None;
    for s in children(root, "service") {
        service_name = service_name.or(s.attribute("name").map(String::from));
        for port in children(s, "port") {
            if let (Some(b), Some(addr)) = (
                port.attribute("binding"),
                child(port, "address").and_then(|a| a.attribute("location")),
            ) {
                addresses.insert(local(b).to_string(), path_of(addr));
            }
        }
    }
    let skeleton_of = |element: &str| -> Option<String> {
        let el = schemas.elements.get(element).copied()?;
        let mut sk = Skeleton::new(&schemas);
        sk.element(el, 0, true);
        Some(sk.out)
    };
    let tns_prefix = schemas.prefixes.get(tns).cloned();
    let mut bindings = Vec::new();
    let mut op_count = 0usize;
    for b in children(root, "binding") {
        let Some(sb) = b
            .children()
            .find(|c| c.is_element() && c.tag_name().name() == "binding")
        else {
            continue;
        };
        let version = match sb.tag_name().namespace() {
            Some(WSDL_SOAP11) => SoapVersion::V11,
            Some(WSDL_SOAP12) => SoapVersion::V12,
            _ => continue,
        };
        let default_style = sb.attribute("style").unwrap_or("document");
        let bname = b.attribute("name").unwrap_or("");
        let Some(pt) = b
            .attribute("type")
            .and_then(|t| port_types.get(local(t)))
            .copied()
        else {
            warnings.push(format!("binding {bname}: portType not found; skipped"));
            continue;
        };
        let mut ops = Vec::new();
        for bop in children(b, "operation") {
            op_count += 1;
            if op_count > MAX_OPERATIONS {
                anyhow::bail!("at most {MAX_OPERATIONS} operations are imported");
            }
            let Some(op_name) = bop.attribute("name") else {
                continue;
            };
            let soap_op = bop
                .children()
                .find(|c| c.is_element() && c.tag_name().name() == "operation");
            let action = soap_op
                .and_then(|o| o.attribute("soapAction"))
                .unwrap_or("")
                .to_string();
            let style = soap_op
                .and_then(|o| o.attribute("style"))
                .unwrap_or(default_style);
            let Some(pop) =
                children(pt, "operation").find(|o| o.attribute("name") == Some(op_name))
            else {
                warnings.push(format!("{op_name}: not declared in the portType; skipped"));
                continue;
            };
            let msg = |io: &str| {
                child(pop, io)
                    .and_then(|i| i.attribute("message"))
                    .and_then(|m| messages.get(local(m)))
            };
            let (input_root, output_body) = if style == "rpc" {
                let prefix = tns_prefix.clone().unwrap_or_default();
                let q = |n: &str| {
                    if prefix.is_empty() {
                        n.to_string()
                    } else {
                        format!("{prefix}:{n}")
                    }
                };
                let mut body = format!("<{}>", q(&format!("{op_name}Response")));
                for (pname, element, ty) in msg("output").into_iter().flatten() {
                    match (element, ty) {
                        (Some(e), _) => body.push_str(&skeleton_of(e).unwrap_or_default()),
                        (None, Some(t)) => {
                            let sk = Skeleton::new(&schemas);
                            body.push_str(&format!(
                                "<{pname}>{}</{pname}>",
                                xml_escape(&sk.type_sample(t, 0))
                            ));
                        }
                        _ => {}
                    }
                }
                body.push_str(&format!("</{}>", q(&format!("{op_name}Response"))));
                (Some(op_name.to_string()), body)
            } else {
                let input_root = msg("input")
                    .and_then(|parts| parts.first())
                    .map(|(pname, element, _)| element.clone().unwrap_or_else(|| pname.clone()));
                let output = msg("output")
                    .and_then(|parts| parts.first())
                    .and_then(|(_, element, _)| element.as_deref())
                    .map(|e| {
                        skeleton_of(e).unwrap_or_else(|| {
                            warnings.push(format!(
                                "{op_name}: output element {e} not found in the schemas"
                            ));
                            String::new()
                        })
                    })
                    .unwrap_or_default();
                (input_root, output)
            };
            let faults = children(pop, "fault")
                .map(|f| {
                    let fname = f.attribute("name").unwrap_or("fault").to_string();
                    let detail = f
                        .attribute("message")
                        .and_then(|m| messages.get(local(m)))
                        .and_then(|parts| parts.first())
                        .and_then(|(_, e, _)| e.as_deref())
                        .and_then(skeleton_of)
                        .unwrap_or_default();
                    (fname, detail)
                })
                .collect();
            ops.push((
                WsdlOperation {
                    name: op_name.to_string(),
                    input_root,
                    output_body,
                    faults,
                },
                action,
            ));
        }
        bindings.push(WsdlBinding {
            version,
            path: addresses.get(bname).cloned(),
            operations: ops,
        });
    }
    Ok(Wsdl {
        service: service_name,
        definitions_name: root.attribute("name").map(String::from),
        ns_decls,
        bindings,
    })
}

pub fn import_wsdl(text: &str, opts: &ImportOptions) -> anyhow::Result<Imported> {
    let doc = parse(text)?;
    let mut warnings = Vec::new();
    let wsdl = analyse_wsdl(&doc, &mut warnings)?;
    if wsdl.bindings.is_empty() {
        anyhow::bail!("the WSDL has no SOAP 1.1 or SOAP 1.2 binding");
    }
    let name = opts
        .name
        .clone()
        .or(wsdl.service.clone())
        .or(wsdl.definitions_name.clone())
        .unwrap_or_else(|| "SOAP service".into());
    let mut out = Imported {
        version: opts.version.clone().unwrap_or_else(|| "1.0".into()),
        name,
        warnings,
        ..Default::default()
    };
    let both = wsdl.bindings.iter().any(|b| b.version == SoapVersion::V11)
        && wsdl.bindings.iter().any(|b| b.version == SoapVersion::V12);
    let mut seen_paths: Vec<(SoapVersion, String)> = Vec::new();
    for b in &wsdl.bindings {
        let path = opts
            .path
            .clone()
            .or(b.path.clone())
            .unwrap_or_else(|| format!("/{}", out.name.replace(' ', "")));
        if seen_paths.contains(&(b.version, path.clone())) {
            out.warnings.push(format!(
                "a second {} binding on {path} was skipped",
                b.version.label()
            ));
            continue;
        }
        seen_paths.push((b.version, path.clone()));
        for (op, action) in &b.operations {
            let request = request_pattern(&path, b.version, action, op.input_root.as_deref(), both);
            let suffix = if both {
                format!(" ({})", b.version.label())
            } else {
                String::new()
            };
            let mut response = ResponseDef {
                templated: false,
                body: Some(envelope(b.version, &wsdl.ns_decls, &op.output_body)),
                ..Default::default()
            };
            response
                .headers
                .insert("content-type".into(), b.version.content_type().into());
            out.stubs.push(stub(
                format!("{}{suffix}", op.name),
                5,
                request.clone(),
                response,
                &opts.tag,
                &op.name,
            ));
            for (fname, detail) in &op.faults {
                let mut response = ResponseDef {
                    status: 500,
                    templated: false,
                    body: Some(fault_envelope(b.version, &wsdl.ns_decls, fname, detail)),
                    ..Default::default()
                };
                response
                    .headers
                    .insert("content-type".into(), b.version.content_type().into());
                let mut s = stub(
                    format!("{} fault {fname}{suffix}", op.name),
                    4,
                    request.clone(),
                    response,
                    &opts.tag,
                    &op.name,
                );
                s.enabled = false;
                out.stubs.push(s);
            }
            if !out.operations.contains(&op.name) {
                out.operations.push(op.name.clone());
            }
        }
    }
    Ok(out)
}

struct SoapUiResponse {
    name: String,
    status: u16,
    content: String,
    headers: BTreeMap<String, String>,
}

fn body_root(xml: &str) -> Option<String> {
    let doc = parse(xml).ok()?;
    let env = doc.root_element();
    let body = child(env, "Body")?;
    body.children()
        .find(|c| c.is_element())
        .map(|c| c.tag_name().name().to_string())
}

pub fn import_soapui(text: &str, opts: &ImportOptions) -> anyhow::Result<Imported> {
    let doc = parse(text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "soapui-project" {
        anyhow::bail!("not a SoapUI project (root must be con:soapui-project)");
    }
    let mut warnings = Vec::new();
    let mut ifaces: HashMap<String, (SoapVersion, InterfaceOps)> = HashMap::new();
    for iface in children(root, "interface") {
        let Some(iname) = iface.attribute("name") else {
            continue;
        };
        let version = if iface.attribute("soapVersion") == Some("1_2") {
            SoapVersion::V12
        } else {
            SoapVersion::V11
        };
        let mut wsdl_roots: HashMap<String, String> = HashMap::new();
        if let Some(cache) = child(iface, "definitionCache") {
            for part in children(cache, "part") {
                let content = child(part, "content").map(text_of).unwrap_or_default();
                if content.contains(WSDL_NS)
                    && let Ok(d) = parse(&content)
                    && let Ok(w) = analyse_wsdl(&d, &mut Vec::new())
                {
                    for b in w.bindings {
                        for (op, _) in b.operations {
                            if let Some(r) = op.input_root {
                                wsdl_roots.entry(op.name).or_insert(r);
                            }
                        }
                    }
                }
            }
        }
        let mut ops = HashMap::new();
        for op in children(iface, "operation") {
            let Some(oname) = op.attribute("name") else {
                continue;
            };
            let action = op.attribute("action").unwrap_or("").to_string();
            let root_el = wsdl_roots.get(oname).cloned().or_else(|| {
                children(op, "call")
                    .filter_map(|c| child(c, "request").map(text_of))
                    .find_map(|r| body_root(&r))
            });
            ops.insert(oname.to_string(), (action, root_el));
        }
        ifaces.insert(iname.to_string(), (version, ops));
    }
    let services: Vec<Node> = children(root, "mockService").collect();
    if services.is_empty() {
        if children(root, "restMockService").next().is_some() {
            anyhow::bail!(
                "the project has only REST mock services; import their OpenAPI or Postman definition instead"
            );
        }
        anyhow::bail!("the project has no SOAP mock service");
    }
    let ms = match &opts.mock_service {
        Some(n) => services
            .iter()
            .find(|s| s.attribute("name") == Some(n.as_str()))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("mock service `{n}` not found"))?,
        None => {
            if services.len() > 1 {
                warnings.push(format!(
                    "the project has {} mock services; only `{}` was imported (choose another with mockService)",
                    services.len(),
                    services[0].attribute("name").unwrap_or("")
                ));
            }
            services[0]
        }
    };
    let ms_name = ms.attribute("name").unwrap_or("SoapUI mock").to_string();
    let prop_version = child(ms, "properties").and_then(|p| {
        children(p, "property")
            .find(|x| child(*x, "name").map(text_of).as_deref() == Some("version"))
            .and_then(|x| child(x, "value").map(text_of))
    });
    let path = opts
        .path
        .clone()
        .or(ms.attribute("path").map(String::from))
        .unwrap_or_else(|| "/".into());
    let mut out = Imported {
        name: opts.name.clone().unwrap_or(ms_name),
        version: opts
            .version
            .clone()
            .or(prop_version)
            .unwrap_or_else(|| "1.0".into()),
        warnings,
        ..Default::default()
    };
    let tag = opts.tag.as_str();
    let mut count = 0usize;
    for mo in children(ms, "mockOperation") {
        count += 1;
        if count > MAX_OPERATIONS {
            anyhow::bail!("at most {MAX_OPERATIONS} operations are imported");
        }
        let op_name = mo
            .attribute("operation")
            .or(mo.attribute("name"))
            .unwrap_or("operation")
            .to_string();
        let (version, action, root_el) = match mo.attribute("interface").and_then(|i| ifaces.get(i))
        {
            Some((v, ops)) => match ops.get(&op_name) {
                Some((a, r)) => (*v, a.clone(), r.clone()),
                None => (*v, String::new(), None),
            },
            None => {
                out.warnings.push(format!(
                    "{op_name}: interface not found in the project; matching on the body only"
                ));
                (SoapVersion::V11, String::new(), None)
            }
        };
        let root_el = root_el.or_else(|| Some(op_name.clone()));
        let base = request_pattern(&path, version, &action, root_el.as_deref(), false);
        let ctx = op_name.clone();
        let responses: Vec<SoapUiResponse> = children(mo, "response")
            .map(|r| SoapUiResponse {
                name: r.attribute("name").unwrap_or("").to_string(),
                status: r
                    .attribute("httpResponseStatus")
                    .and_then(|s| s.trim().parse().ok())
                    .filter(|s| (100..=599).contains(s))
                    .unwrap_or(200),
                content: child(r, "responseContent").map(text_of).unwrap_or_default(),
                headers: children(r, "header")
                    .filter_map(|h| {
                        let n = child(h, "name").map(text_of)?;
                        let v = child(h, "value").map(text_of).unwrap_or_default();
                        Some((n.to_ascii_lowercase(), v))
                    })
                    .collect(),
            })
            .collect();
        if responses.is_empty() {
            out.warnings
                .push(format!("{ctx}: no mock responses; skipped"));
            continue;
        }
        let to_def = |r: &SoapUiResponse, warnings: &mut Vec<String>| {
            let (body, templated) = convert_text(
                &r.content,
                Flavor::Xml,
                &format!("{ctx} / {}", r.name),
                warnings,
            );
            let mut d = ResponseDef {
                status: r.status,
                templated,
                body: Some(body),
                ..Default::default()
            };
            d.headers.extend(r.headers.clone());
            d.headers
                .entry("content-type".into())
                .or_insert_with(|| version.content_type().into());
            d
        };
        let default_name = child(mo, "defaultResponse")
            .map(text_of)
            .filter(|s| !s.is_empty());
        let default = default_name
            .as_ref()
            .and_then(|n| responses.iter().find(|r| &r.name == n))
            .unwrap_or(&responses[0]);
        let style = child(mo, "dispatchStyle")
            .map(text_of)
            .unwrap_or_else(|| "SEQUENCE".into())
            .trim()
            .to_ascii_uppercase();
        let dispatch_path = child(mo, "dispatchPath").map(text_of).unwrap_or_default();
        let mut dispatched: Vec<(String, String, String)> = Vec::new();
        match style.as_str() {
            "SEQUENCE" | "RANDOM" => {
                let defs: Vec<ResponseDef> = responses.iter().map(|r| to_def(r, &mut out.warnings)).collect();
                let mut s = stub(op_name.clone(), 5, base.clone(), defs[0].clone(), tag, &op_name);
                if defs.len() > 1 {
                    s.response = ResponseDef::default();
                    s.responses = defs;
                    s.response_mode = Some(if style == "RANDOM" {
                        ResponseMode::Random
                    } else {
                        ResponseMode::Cycle
                    });
                }
                out.stubs.push(s);
                out.operations.push(op_name.clone());
                continue;
            }
            "XPATH" => {
                let xp = strip_xpath_declarations(&dispatch_path);
                for r in &responses {
                    dispatched.push((xp.clone(), r.name.clone(), r.name.clone()));
                }
            }
            "QUERY_MATCH" => {
                if let Some(cfg) = child(mo, "dispatchConfig") {
                    for q in children(cfg, "query") {
                        let xp = child(q, "query").map(text_of).unwrap_or_default();
                        let m = child(q, "match").map(text_of).unwrap_or_default();
                        let resp = child(q, "response").map(text_of).unwrap_or_default();
                        dispatched.push((strip_xpath_declarations(&xp), m, resp));
                    }
                }
            }
            "SCRIPT" => out.warnings.push(format!(
                "{ctx}: SCRIPT dispatch is never executed; every request gets the default response `{}`",
                default.name
            )),
            other => out.warnings.push(format!(
                "{ctx}: dispatch style {other} is not supported; every request gets the default response"
            )),
        }
        for (xp, value, resp_name) in dispatched {
            if crate::xml::parse(&xp).is_err() {
                out.warnings.push(format!(
                    "{ctx}: XPath `{xp}` is outside the supported subset; dispatch on it was skipped"
                ));
                break;
            }
            let Some(r) = responses.iter().find(|r| r.name == resp_name) else {
                out.warnings.push(format!(
                    "{ctx}: rule refers to unknown response `{resp_name}`"
                ));
                continue;
            };
            let mut req = base.clone();
            req.body.push(BodyMatcher {
                xpath: Some(xp.clone()),
                value: Some(StringMatcher {
                    equal_to: Some(value.trim().to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let def = to_def(r, &mut out.warnings);
            out.stubs.push(stub(
                format!("{op_name} [{}]", r.name),
                4,
                req,
                def,
                tag,
                &op_name,
            ));
        }
        let def = to_def(default, &mut out.warnings);
        out.stubs
            .push(stub(op_name.clone(), 5, base, def, tag, &op_name));
        out.operations.push(op_name);
    }
    if out.stubs.is_empty() {
        out.warnings.push("no mock operations found".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::CompiledStub;
    use crate::request::CapturedRequest;

    fn compiled(stubs: &[Stub]) -> Vec<CompiledStub> {
        let mut c: Vec<CompiledStub> = stubs
            .iter()
            .filter(|s| s.enabled)
            .map(|s| CompiledStub::compile(s).unwrap())
            .collect();
        c.sort_by_key(|x| x.stub.priority);
        c
    }

    fn call<'a>(
        c: &'a [CompiledStub],
        path: &str,
        headers: Vec<(&str, &str)>,
        body: &str,
    ) -> Option<&'a CompiledStub> {
        let req = CapturedRequest::new(
            "POST",
            path,
            None,
            headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            bytes::Bytes::from(body.to_string()),
        );
        c.iter()
            .find(|s| s.match_request(&req, req.json()).is_some())
    }

    fn env(inner: &str) -> String {
        format!(
            "<soapenv:Envelope xmlns:soapenv=\"{SOAP11_ENV}\" xmlns:hel=\"http://www.example.com/hello\"><soapenv:Body>{inner}</soapenv:Body></soapenv:Envelope>"
        )
    }

    #[test]
    fn wsdl_operations_skeletons_and_faults() {
        let r = import_wsdl(
            include_str!("../tests/fixtures/hello.wsdl"),
            &ImportOptions {
                tag: "hello".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.name, "HelloService");
        assert_eq!(r.operations, vec!["sayHello", "getBalance"]);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let say11 = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("sayHello (SOAP 1.1)"))
            .unwrap();
        let body = say11.response.body.as_deref().unwrap();
        assert!(
            body.contains(
                "<ns0:sayHelloResponse><sayHello>string</sayHello></ns0:sayHelloResponse>"
            ),
            "{body}"
        );
        assert!(roxmltree::Document::parse(body).is_ok());
        let bal = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("getBalance (SOAP 1.2)"))
            .unwrap();
        let body = bal.response.body.as_deref().unwrap();
        assert!(body.contains("version=\"0\""), "{body}");
        assert!(body.contains("<currency>UAH</currency>"), "{body}");
        assert!(
            body.contains("<history><at>2026-01-01</at><delta>0</delta></history>"),
            "{body}"
        );
        assert!(body.contains(SOAP12_ENV));
        let fault = r
            .stubs
            .iter()
            .find(|s| s.name.as_deref() == Some("sayHello fault HelloFault (SOAP 1.1)"))
            .unwrap();
        assert!(!fault.enabled);
        assert_eq!(fault.response.status, 500);
        assert!(
            fault
                .response
                .body
                .as_deref()
                .unwrap()
                .contains("<ns0:helloFault><code>0</code>")
        );
        let c = compiled(&r.stubs);
        let hit = call(
            &c,
            "/services/HelloService",
            vec![
                ("soapaction", "\"http://www.example.com/hello/sayHello\""),
                ("content-type", "text/xml"),
            ],
            &env("<hel:sayHello><name>x</name></hel:sayHello>"),
        )
        .unwrap();
        assert_eq!(hit.stub.name.as_deref(), Some("sayHello (SOAP 1.1)"));
        let hit = call(&c, "/services/HelloService12", vec![("content-type", "application/soap+xml; charset=utf-8; action=\"http://www.example.com/hello/sayHello\"")], &env("<hel:sayHello/>")).unwrap();
        assert_eq!(hit.stub.name.as_deref(), Some("sayHello (SOAP 1.2)"));
        assert!(
            call(
                &c,
                "/services/HelloService",
                vec![("soapaction", "urn:other"), ("content-type", "text/xml")],
                &env("<hel:sayHello/>")
            )
            .is_none()
        );
        let hit = call(
            &c,
            "/services/HelloService",
            vec![("content-type", "text/xml")],
            &env("<hel:getBalance><msisdn>1</msisdn></hel:getBalance>"),
        )
        .unwrap();
        assert_eq!(hit.stub.name.as_deref(), Some("getBalance (SOAP 1.1)"));
    }

    #[test]
    fn soapui_dispatch_styles() {
        let r = import_soapui(
            include_str!("../tests/fixtures/hello-soapui-project.xml"),
            &ImportOptions {
                tag: "soapui".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            (r.name.as_str(), r.version.as_str()),
            ("HelloService Mock", "0.9")
        );
        assert_eq!(
            r.operations,
            vec!["sayHello", "getBalance", "ping", "audit"]
        );
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("SCRIPT"));
        let c = compiled(&r.stubs);
        let say = |name: &str| {
            call(
                &c,
                "/HelloService",
                vec![("soapaction", "http://www.example.com/hello/sayHello")],
                &env(&format!("<hel:sayHello><name>{name}</name></hel:sayHello>")),
            )
            .map(|s| s.stub.clone())
        };
        let andrew = say("Andrew").unwrap();
        assert!(
            andrew
                .response
                .body
                .as_deref()
                .unwrap()
                .contains("Hello Andrew !")
        );
        assert!(andrew.response.templated);
        assert!(
            andrew
                .response
                .body
                .as_deref()
                .unwrap()
                .contains("<id>{{uuid}}</id>")
        );
        assert_eq!(andrew.response.headers["x-greeting"], "andrew");
        assert!(
            say("Karla")
                .unwrap()
                .response
                .body
                .as_deref()
                .unwrap()
                .contains("Hello Karla !")
        );
        let other = say("Bob").unwrap();
        assert_eq!(other.response.status, 500);
        let bal = |m: &str| {
            call(
                &c,
                "/HelloService",
                vec![],
                &env(&format!(
                    "<hel:getBalance><msisdn>{m}</msisdn></hel:getBalance>"
                )),
            )
            .map(|s| s.stub.response.body.clone().unwrap())
            .unwrap()
        };
        assert!(bal("380501234567").contains("10.50"));
        assert!(bal("1").contains("1000000"));
        let ping = call(
            &c,
            "/HelloService",
            vec![("soapaction", "urn:ping")],
            &env("<ping/>"),
        )
        .unwrap();
        assert_eq!(ping.stub.responses.len(), 2);
        assert_eq!(ping.stub.response_mode, Some(ResponseMode::Cycle));
        assert!(
            call(
                &c,
                "/HelloService",
                vec![("soapaction", "urn:audit")],
                &env("<audit/>")
            )
            .is_some()
        );
    }

    #[test]
    fn hostile_xml_is_refused() {
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE d [<!ENTITY a "aaaa">]><wsdl:definitions xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/">&a;</wsdl:definitions>"#;
        assert!(import_wsdl(bomb, &ImportOptions::default()).is_err());
        assert!(import_soapui("<a/>", &ImportOptions::default()).is_err());
        assert!(detect_wsdl(include_str!("../tests/fixtures/hello.wsdl")));
        assert!(detect_soapui(include_str!(
            "../tests/fixtures/hello-soapui-project.xml"
        )));
        assert!(!detect_wsdl(include_str!(
            "../tests/fixtures/hello-soapui-project.xml"
        )));
        let recursive = r#"<wsdl:definitions xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/" xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t">
<wsdl:types><xsd:schema targetNamespace="urn:t"><xsd:element name="r" type="t:N"/><xsd:complexType name="N"><xsd:sequence><xsd:element name="n" type="t:N"/><xsd:element name="m" type="t:N"/></xsd:sequence></xsd:complexType></xsd:schema></wsdl:types>
<wsdl:message name="m"><wsdl:part name="p" element="t:r"/></wsdl:message>
<wsdl:portType name="P"><wsdl:operation name="o"><wsdl:input message="t:m"/><wsdl:output message="t:m"/></wsdl:operation></wsdl:portType>
<wsdl:binding name="B" type="t:P"><soap:binding style="document"/><wsdl:operation name="o"><soap:operation soapAction="o"/></wsdl:operation></wsdl:binding></wsdl:definitions>"#;
        let r = import_wsdl(recursive, &ImportOptions::default()).unwrap();
        let body = r.stubs[0].response.body.clone().unwrap();
        assert!(body.len() < 2_000, "{body}");
        assert_eq!(r.stubs[0].request.url_path.as_deref(), Some("/SOAPservice"));
    }

    fn wsdl_with_schema(schema: &str) -> String {
        format!(
            r#"<wsdl:definitions xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/" xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t">
<wsdl:types><xsd:schema targetNamespace="urn:t">{schema}</xsd:schema></wsdl:types>
<wsdl:message name="m"><wsdl:part name="p" element="t:r"/></wsdl:message>
<wsdl:portType name="P"><wsdl:operation name="o"><wsdl:input message="t:m"/><wsdl:output message="t:m"/></wsdl:operation></wsdl:portType>
<wsdl:binding name="B" type="t:P"><soap:binding style="document"/><wsdl:operation name="o"><soap:operation soapAction="o"/></wsdl:operation></wsdl:binding></wsdl:definitions>"#
        )
    }

    #[test]
    fn element_ref_cycles_terminate() {
        for schema in [
            r#"<xsd:element name="r" ref="t:r"/>"#,
            r#"<xsd:element name="r" ref="t:a"/><xsd:element name="a" ref="t:r"/>"#,
            r#"<xsd:element name="r"><xsd:complexType><xsd:sequence><xsd:element ref="t:r"/><xsd:element ref="t:r"/></xsd:sequence></xsd:complexType></xsd:element>"#,
            r#"<xsd:complexType name="A"><xsd:complexContent><xsd:extension base="t:B"/></xsd:complexContent></xsd:complexType><xsd:complexType name="B"><xsd:complexContent><xsd:extension base="t:A"/></xsd:complexContent></xsd:complexType><xsd:element name="r" type="t:A"/>"#,
        ] {
            let r = import_wsdl(&wsdl_with_schema(schema), &ImportOptions::default()).unwrap();
            let body = r.stubs[0].response.body.clone().unwrap_or_default();
            assert!(body.len() < 50_000, "{}", body.len());
        }
    }

    #[test]
    fn deeply_nested_particles_are_bounded() {
        let nested = |tag: &str, n: usize| {
            format!(
                r#"<xsd:element name="r"><xsd:complexType>{}<xsd:element name="x" type="xsd:string"/>{}</xsd:complexType></xsd:element>"#,
                format!("<xsd:{tag}>").repeat(n),
                format!("</xsd:{tag}>").repeat(n)
            )
        };
        for tag in ["sequence", "choice"] {
            let r = import_wsdl(
                &wsdl_with_schema(&nested(tag, 200)),
                &ImportOptions::default(),
            );
            assert!(r.is_ok());
            let err = import_wsdl(
                &wsdl_with_schema(&nested(tag, 50_000)),
                &ImportOptions::default(),
            )
            .unwrap_err();
            assert!(err.to_string().contains("deeper"), "{err}");
        }
        let soapui = format!(
            r#"<con:soapui-project xmlns:con="http://eviware.com/soapui/config">{}{}</con:soapui-project>"#,
            "<con:x>".repeat(50_000),
            "</con:x>".repeat(50_000)
        );
        assert!(import_soapui(&soapui, &ImportOptions::default()).is_err());
    }
}
