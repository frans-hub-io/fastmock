use std::collections::BTreeMap;

use bytes::Bytes;
use serde_json::{Map, Value, json};

pub const MAX_JSON_NODES: usize = 200_000;
pub const MAX_FORM_PARAMS: usize = 10_000;
pub const MAX_MULTIPART_PARTS: usize = 64;
pub const MAX_COOKIES: usize = 256;

#[derive(Clone, Debug)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub scheme: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// The number of messages of a gRPC call when the body holds fewer (a journaled stream cut short).
    pub grpc_message_count: Option<u64>,
    /// A gRPC call: a match also returns the raw gRPC plan (messages are rendered while they are sent).
    pub grpc_call: bool,
    query_string: String,
    parsed: std::sync::OnceLock<Option<Value>>,
    form: std::sync::OnceLock<Vec<(String, String)>>,
    parts: std::sync::OnceLock<Option<Vec<MultipartPart>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultipartPart {
    pub name: Option<String>,
    pub filename: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl MultipartPart {
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

pub fn split_host_port(authority: &str) -> (String, Option<u16>) {
    let a = authority.trim();
    let a = a.rsplit_once('@').map(|(_, h)| h).unwrap_or(a);
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (h.to_ascii_lowercase(), port);
        }
        return (a.to_ascii_lowercase(), None);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_ascii_lowercase(), p.parse().ok()),
        _ => (a.to_ascii_lowercase(), None),
    }
}

fn header_param(value: &str, name: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(name) {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .unwrap_or(v);
            return Some(v.to_string());
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

pub fn parse_multipart(content_type: &str, body: &Bytes) -> Option<Vec<MultipartPart>> {
    if !content_type
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("multipart/")
    {
        return None;
    }
    let boundary = header_param(content_type, "boundary")?;
    if boundary.is_empty() || boundary.len() > 200 {
        return None;
    }
    let delim = format!("--{boundary}").into_bytes();
    let next_delim = format!("\r\n--{boundary}").into_bytes();
    let mut pos = find(body, &delim, 0)? + delim.len();
    let mut parts = Vec::new();
    loop {
        if body.get(pos..pos + 2) == Some(b"--") {
            return Some(parts);
        }
        if body.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        } else if body.get(pos..pos + 1) == Some(b"\n") {
            pos += 1;
        }
        if parts.len() >= MAX_MULTIPART_PARTS {
            return None;
        }
        let (head_end, body_start) = match find(body, b"\r\n\r\n", pos) {
            Some(i) => (i, i + 4),
            None => {
                let i = find(body, b"\n\n", pos)?;
                (i, i + 2)
            }
        };
        let end = find(body, &next_delim, body_start)?;
        let head = String::from_utf8_lossy(&body[pos..head_end]);
        let headers: Vec<(String, String)> = head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let disposition = headers
            .iter()
            .find(|(k, _)| k == "content-disposition")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        parts.push(MultipartPart {
            name: header_param(disposition, "name"),
            filename: header_param(disposition, "filename"),
            headers,
            body: body.slice(body_start..end),
        });
        pos = end + next_delim.len();
    }
}

pub fn json_node_estimate(body: &[u8]) -> usize {
    let mut n = 1;
    let mut in_str = false;
    let mut esc = false;
    for &b in body {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b',' | b'[' | b'{' | b':' => n += 1,
            _ => {}
        }
    }
    n
}

pub fn parse_json_bounded(body: &[u8]) -> Option<Value> {
    let start = body.iter().position(|b| !b.is_ascii_whitespace())?;
    if !matches!(
        body[start],
        b'{' | b'[' | b'"' | b't' | b'f' | b'n' | b'-' | b'0'..=b'9'
    ) {
        return None;
    }
    if json_node_estimate(body) > MAX_JSON_NODES {
        return None;
    }
    serde_json::from_slice(body).ok()
}

impl CapturedRequest {
    pub fn new(
        method: &str,
        path: &str,
        raw_query: Option<&str>,
        headers: Vec<(String, String)>,
        body: Bytes,
    ) -> Self {
        let query = raw_query.map(parse_query).unwrap_or_default();
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let (host, port) = match headers.iter().find(|(k, _)| k == "host") {
            Some((_, h)) => {
                let (h, p) = split_host_port(h);
                (Some(h), p)
            }
            None => (None, None),
        };
        Self {
            method: method.to_ascii_uppercase(),
            path: decode_path(path),
            query,
            headers,
            body,
            scheme: "http".into(),
            host,
            port,
            grpc_message_count: None,
            grpc_call: false,
            query_string: raw_query.unwrap_or("").to_string(),
            parsed: std::sync::OnceLock::new(),
            form: std::sync::OnceLock::new(),
            parts: std::sync::OnceLock::new(),
        }
    }

    pub fn with_origin(
        mut self,
        scheme: &str,
        authority: Option<&str>,
        local_port: Option<u16>,
    ) -> Self {
        self.scheme = scheme.to_ascii_lowercase();
        if let Some(a) = authority.filter(|a| !a.is_empty()) {
            let (h, p) = split_host_port(a);
            self.host = Some(h);
            self.port = p.or(self.port);
        }
        if self.port.is_none() {
            self.port = local_port;
        }
        self
    }

    pub fn effective_port(&self) -> Option<u16> {
        self.port.or(match self.scheme.as_str() {
            "https" => Some(443),
            "http" => Some(80),
            _ => None,
        })
    }

    pub fn url(&self) -> String {
        if self.query_string.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query_string)
        }
    }

    pub fn header_all(&self, name: &str) -> Vec<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .filter(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn cookies(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for raw in self.header_all("cookie") {
            for c in raw.split(';') {
                if out.len() >= MAX_COOKIES {
                    return out;
                }
                let c = c.trim();
                if c.is_empty() {
                    continue;
                }
                let (k, v) = c.split_once('=').unwrap_or((c, ""));
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .unwrap_or(v);
                out.push((k.trim().to_string(), v.to_string()));
            }
        }
        out
    }

    pub fn cookie_all(&self, name: &str) -> Vec<String> {
        self.cookies()
            .into_iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v)
            .collect()
    }

    pub fn basic_auth(&self) -> Option<(String, String)> {
        use base64::Engine as _;
        let h = self.header("authorization")?.trim();
        let (scheme, value) = h.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let raw = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .ok()?;
        let text = String::from_utf8(raw).ok()?;
        let (u, p) = text.split_once(':')?;
        Some((u.to_string(), p.to_string()))
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn form_params(&self) -> &[(String, String)] {
        self.form.get_or_init(|| {
            let is_form = self.content_type().is_some_and(|c| {
                c.to_ascii_lowercase()
                    .contains("application/x-www-form-urlencoded")
            });
            if !is_form {
                return Vec::new();
            }
            let text = String::from_utf8_lossy(&self.body);
            let mut v = parse_query(text.trim());
            v.truncate(MAX_FORM_PARAMS);
            v
        })
    }

    pub fn form_all(&self, name: &str) -> Vec<&str> {
        self.form_params()
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn multipart_parts(&self) -> Option<&[MultipartPart]> {
        self.parts
            .get_or_init(|| parse_multipart(self.content_type()?, &self.body))
            .as_deref()
    }

    pub fn query_first(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query_all(&self, name: &str) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Option<&Value> {
        self.parsed
            .get_or_init(|| parse_json_bounded(&self.body))
            .as_ref()
    }

    pub fn raw_query(&self) -> String {
        self.query
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    pub fn template_context(
        &self,
        path_params: &BTreeMap<String, String>,
        with_original: bool,
    ) -> Value {
        let mut query = Map::new();
        let mut query_all: Map<String, Value> = Map::new();
        for (k, v) in &self.query {
            query
                .entry(k.clone())
                .or_insert_with(|| Value::String(v.clone()));
            query_all
                .entry(k.clone())
                .or_insert_with(|| Value::Array(vec![]))
                .as_array_mut()
                .unwrap()
                .push(Value::String(v.clone()));
        }
        let mut headers = Map::new();
        let mut headers_all: Map<String, Value> = Map::new();
        for (k, v) in &self.headers {
            headers
                .entry(k.clone())
                .or_insert_with(|| Value::String(v.clone()));
            if let Value::Array(a) = headers_all
                .entry(k.clone())
                .or_insert_with(|| Value::Array(vec![]))
            {
                a.push(Value::String(v.clone()));
            }
        }
        let segments: Vec<&str> = self
            .path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let mut cookies = Map::new();
        for (k, v) in self.cookies() {
            cookies.entry(k).or_insert_with(|| Value::String(v));
        }
        let mut form = Map::new();
        for (k, v) in self.form_params() {
            form.entry(k.clone())
                .or_insert_with(|| Value::String(v.clone()));
        }
        let request = json!({
            "method": self.method,
            "url": self.url(),
            "scheme": self.scheme,
            "host": self.host,
            "port": self.effective_port(),
            "baseUrl": match (&self.host, self.port) {
                (Some(h), Some(p)) => format!("{}://{h}:{p}", self.scheme),
                (Some(h), None) => format!("{}://{h}", self.scheme),
                _ => String::new(),
            },
            "cookies": cookies,
            "formParams": form,
            "path": self.path,
            "pathSegments": segments,
            "pathParams": path_params,
            "query": query,
            "queryAll": query_all,
            "headers": headers,
            "headersAll": headers_all,
            "body": self.body_text(),
            "json": self.json().cloned().unwrap_or(Value::Null),
        });
        if with_original {
            json!({"request": request.clone(), "originalRequest": request})
        } else {
            json!({"request": request})
        }
    }
}

pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

pub fn decode_path(p: &str) -> String {
    if !p.contains('%') {
        return p.to_string();
    }
    let bytes = p.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            && b != b'/'
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| p.to_string())
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 3 <= bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_decodes_query() {
        let q = parse_query("a=1&b=hello+world&c=%2B380&d&a=2");
        assert_eq!(
            q,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "hello world".into()),
                ("c".into(), "+380".into()),
                ("d".into(), "".into()),
                ("a".into(), "2".into()),
            ]
        );
    }

    #[test]
    fn json_is_parsed_once_and_bounded() {
        let r = CapturedRequest::new(
            "POST",
            "/",
            None,
            vec![],
            Bytes::from_static(br#" {"a":[1,2,{"b":"x,y"}]}"#),
        );
        assert_eq!(r.json().unwrap()["a"][2]["b"], "x,y");
        assert!(std::ptr::eq(r.json().unwrap(), r.json().unwrap()));
        let big = format!("[{}0]", "0,".repeat(MAX_JSON_NODES));
        let r = CapturedRequest::new("POST", "/", None, vec![], Bytes::from(big));
        assert!(r.json().is_none());
        assert!(
            CapturedRequest::new("POST", "/", None, vec![], Bytes::from_static(b"plain text"))
                .json()
                .is_none()
        );
    }

    #[test]
    fn paths_are_percent_decoded_except_slashes() {
        assert_eq!(decode_path("/%D0%BF%D1%80/a%20b/x%2Fy"), "/пр/a b/x%2Fy");
        assert_eq!(decode_path("/%ff"), "/%ff");
    }

    #[test]
    fn keeps_broken_percent_sequences() {
        assert_eq!(
            parse_query("a=%zz&b=%4"),
            vec![("a".into(), "%zz".into()), ("b".into(), "%4".into())]
        );
    }
}
