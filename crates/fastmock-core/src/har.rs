use std::collections::HashSet;

use serde_json::Value;
use uuid::Uuid;

use crate::model::{BodyMatcher, RequestPattern, ResponseDef, StringMatcher, Stub};

pub const MAX_ENTRIES: usize = 10_000;
pub const MAX_BODY: usize = 10 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = crate::postman::MAX_TOTAL_BYTES;

#[derive(Debug, Clone, Default)]
pub struct HarOptions {
    pub tag: String,
    /// Keep only entries whose response MIME type starts with one of these (empty = all).
    pub mime_types: Vec<String>,
    /// Keep only entries for these hosts (empty = all).
    pub hosts: Vec<String>,
    /// Response headers to keep besides content-type.
    pub capture_headers: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Converted {
    pub stubs: Vec<Stub>,
    pub warnings: Vec<String>,
}

pub fn is_har(doc: &Value) -> bool {
    doc["log"]["entries"].is_array()
}

pub struct SplitUrl {
    pub host: String,
    pub path: String,
    pub query: String,
}

pub fn split_url(url: &str) -> SplitUrl {
    let url = url.split('#').next().unwrap_or(url);
    let (host, rest) = match url.find("://") {
        Some(i) => {
            let after = &url[i + 3..];
            match after.find(['/', '?']) {
                Some(j) => (&after[..j], &after[j..]),
                None => (after, ""),
            }
        }
        None => ("", url),
    };
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host.split(':').next().unwrap_or(host));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    SplitUrl {
        host: host.to_ascii_lowercase(),
        path: if path.is_empty() {
            "/".into()
        } else {
            path.to_string()
        },
        query: query.to_string(),
    }
}

fn mime_of(v: &Value) -> String {
    v.as_str()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub fn convert(doc: &Value, opts: &HarOptions) -> Result<Converted, String> {
    let entries = doc["log"]["entries"]
        .as_array()
        .ok_or("not a HAR document: `log.entries` array missing")?;
    if entries.len() > MAX_ENTRIES {
        return Err(format!(
            "HAR has {} entries; at most {MAX_ENTRIES}",
            entries.len()
        ));
    }
    let hosts: Vec<String> = opts.hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
    let mimes: Vec<String> = opts
        .mime_types
        .iter()
        .map(|m| m.to_ascii_lowercase())
        .collect();
    let keep: Vec<String> = opts
        .capture_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .chain(["content-type".to_string()])
        .collect();
    let mut out = Converted::default();
    let mut seen = HashSet::new();
    let (mut skipped_host, mut skipped_mime, mut duplicates) = (0, 0, 0);
    let mut bytes = 0usize;
    for (i, e) in entries.iter().enumerate() {
        let req = &e["request"];
        let resp = &e["response"];
        let method = req["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
        let Some(url) = req["url"].as_str() else {
            out.warnings
                .push(format!("entry {i}: request.url missing; skipped"));
            continue;
        };
        let u = split_url(url);
        if !hosts.is_empty() && !hosts.contains(&u.host) {
            skipped_host += 1;
            continue;
        }
        let status = resp["status"].as_u64().unwrap_or(0);
        if !(100..=599).contains(&status) {
            out.warnings.push(format!(
                "entry {i} ({method} {}): no response status (blocked or failed request); skipped",
                u.path
            ));
            continue;
        }
        let content = &resp["content"];
        let mime = mime_of(&content["mimeType"]);
        if !mimes.is_empty() && !mimes.iter().any(|m| mime.starts_with(m.as_str())) {
            skipped_mime += 1;
            continue;
        }
        let body_text = req["postData"]["text"].as_str().unwrap_or("");
        let sig = format!("{method} {}?{}\n{body_text}", u.path, u.query);
        if !seen.insert(sig) {
            duplicates += 1;
            continue;
        }
        let mut pattern = RequestPattern {
            method: Some(method.clone()),
            url_path: Some(crate::request::decode_path(&u.path)),
            ..Default::default()
        };
        let query: Vec<(String, String)> = match req["queryString"].as_array() {
            Some(q) if !q.is_empty() => q
                .iter()
                .filter_map(|x| {
                    Some((
                        x["name"].as_str()?.to_string(),
                        x["value"].as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect(),
            _ => crate::request::parse_query(&u.query),
        };
        for (k, v) in query {
            pattern.query.insert(
                k,
                StringMatcher {
                    equal_to: Some(v),
                    ..Default::default()
                },
            );
        }
        if !body_text.is_empty() {
            if body_text.len() > MAX_BODY {
                out.warnings.push(format!(
                    "entry {i} ({method} {}): request body larger than 10 MiB is not matched",
                    u.path
                ));
            } else {
                let req_mime = mime_of(&req["postData"]["mimeType"]);
                match serde_json::from_str::<Value>(body_text) {
                    Ok(v) if req_mime.contains("json") || v.is_object() || v.is_array() => {
                        pattern.body.push(BodyMatcher {
                            equal_to_json: Some(v),
                            ..Default::default()
                        })
                    }
                    _ => pattern.body.push(BodyMatcher {
                        equal_to: Some(body_text.to_string()),
                        ..Default::default()
                    }),
                }
            }
        }
        let mut response = ResponseDef {
            status: status as u16,
            templated: false,
            ..Default::default()
        };
        for h in resp["headers"].as_array().into_iter().flatten() {
            if let (Some(k), Some(v)) = (h["name"].as_str(), h["value"].as_str())
                && keep.contains(&k.to_ascii_lowercase())
                && !response.headers.keys().any(|x| x.eq_ignore_ascii_case(k))
            {
                response
                    .headers
                    .insert(k.to_ascii_lowercase(), v.to_string());
            }
        }
        if !mime.is_empty() && !response.headers.contains_key("content-type") {
            response.headers.insert(
                "content-type".into(),
                content["mimeType"].as_str().unwrap_or("").to_string(),
            );
        }
        if let Some(text) = content["text"].as_str().filter(|t| !t.is_empty()) {
            if text.len() > MAX_BODY * 4 / 3 + 4 {
                out.warnings.push(format!(
                    "entry {i} ({method} {}): response body larger than 10 MiB; skipped",
                    u.path
                ));
                continue;
            }
            if content["encoding"].as_str() == Some("base64") {
                response.base64_body = Some(text.to_string());
            } else {
                match serde_json::from_str::<Value>(text) {
                    Ok(v) if mime.contains("json") => response.json_body = Some(v),
                    _ => response.body = Some(text.to_string()),
                }
            }
        }
        let stub = Stub {
            id: Uuid::new_v4(),
            name: Some(format!("har {method} {}", u.path)),
            request: pattern,
            response,
            tags: vec![opts.tag.clone(), "har".into()],
            enabled: true,
            ..Default::default()
        };
        bytes += crate::postman::stub_size(&stub);
        if bytes > MAX_TOTAL_BYTES {
            return Err(format!(
                "HAR expands to more than {} MiB of stubs",
                MAX_TOTAL_BYTES >> 20
            ));
        }
        if out.stubs.len() >= crate::postman::MAX_STUBS {
            return Err(format!(
                "HAR produces more than {} stubs",
                crate::postman::MAX_STUBS
            ));
        }
        out.stubs.push(stub);
    }
    if skipped_host > 0 {
        out.warnings.push(format!(
            "{skipped_host} entries skipped by the hosts filter"
        ));
    }
    if skipped_mime > 0 {
        out.warnings.push(format!(
            "{skipped_mime} entries skipped by the mimeTypes filter"
        ));
    }
    if duplicates > 0 {
        out.warnings.push(format!(
            "{duplicates} repeated requests skipped; the first response was kept"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn har() -> Value {
        json!({"log": {"version": "1.2", "creator": {"name": "browser", "version": "1"}, "entries": [
            {"request": {"method": "GET", "url": "https://api.example.com/users/7?expand=1", "headers": [{"name": "Authorization", "value": "Bearer secret"}], "queryString": [{"name": "expand", "value": "1"}]},
             "response": {"status": 200, "headers": [{"name": "Content-Type", "value": "application/json; charset=utf-8"}, {"name": "Set-Cookie", "value": "sid=abc"}, {"name": "X-Trace", "value": "t"}], "content": {"mimeType": "application/json; charset=utf-8", "text": "{\"id\":7}"}}},
            {"request": {"method": "GET", "url": "https://api.example.com/users/7?expand=1"},
             "response": {"status": 500, "headers": [], "content": {"mimeType": "application/json", "text": "{}"}}},
            {"request": {"method": "POST", "url": "https://api.example.com/orders", "postData": {"mimeType": "application/json", "text": "{\"sku\":\"A\"}"}},
             "response": {"status": 201, "headers": [], "content": {"mimeType": "text/plain", "text": "created"}}},
            {"request": {"method": "GET", "url": "https://cdn.example.com/logo.png"},
             "response": {"status": 200, "headers": [], "content": {"mimeType": "image/png", "text": "iVBORw0KGgo=", "encoding": "base64"}}},
            {"request": {"method": "GET", "url": "https://api.example.com/blocked"},
             "response": {"status": 0, "headers": [], "content": {}}}
        ]}})
    }

    #[test]
    fn converts_entries() {
        let doc = har();
        assert!(is_har(&doc));
        let c = convert(
            &doc,
            &HarOptions {
                tag: "h".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(c.stubs.len(), 3, "{:?}", c.warnings);
        let s = &c.stubs[0];
        assert_eq!(s.request.url_path.as_deref(), Some("/users/7"));
        assert_eq!(s.request.query["expand"].equal_to.as_deref(), Some("1"));
        assert!(s.request.headers.is_empty());
        assert_eq!(s.response.json_body, Some(json!({"id": 7})));
        assert_eq!(s.response.headers.len(), 1);
        assert!(s.response.headers["content-type"].starts_with("application/json"));
        assert!(!s.response.templated);
        assert_eq!(
            c.stubs[1].request.body[0].equal_to_json,
            Some(json!({"sku": "A"}))
        );
        assert_eq!(c.stubs[1].response.body.as_deref(), Some("created"));
        assert_eq!(
            c.stubs[2].response.base64_body.as_deref(),
            Some("iVBORw0KGgo=")
        );
        assert!(c.warnings.iter().any(|w| w.contains("repeated")));
        assert!(c.warnings.iter().any(|w| w.contains("blocked")));
    }

    #[test]
    fn filters() {
        let c = convert(
            &har(),
            &HarOptions {
                tag: "h".into(),
                mime_types: vec!["application/json".into()],
                hosts: vec!["api.example.com".into()],
                capture_headers: vec!["x-trace".into()],
            },
        )
        .unwrap();
        assert_eq!(c.stubs.len(), 1, "{:?}", c.warnings);
        assert_eq!(
            c.stubs[0]
                .response
                .headers
                .get("x-trace")
                .map(String::as_str),
            Some("t")
        );
        assert!(c.warnings.iter().any(|w| w.contains("mimeTypes")));
        assert!(c.warnings.iter().any(|w| w.contains("hosts")));
    }

    #[test]
    fn urls() {
        let u = split_url("https://user@Example.com:8443/a/b?x=1#frag");
        assert_eq!(
            (u.host.as_str(), u.path.as_str(), u.query.as_str()),
            ("example.com", "/a/b", "x=1")
        );
        assert_eq!(split_url("http://[::1]:80").path, "/");
        assert_eq!(split_url("http://[::1]:80").host, "::1");
        assert!(convert(&json!({"log": {}}), &HarOptions::default()).is_err());
    }
}
