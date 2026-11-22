use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, STANDARD_NO_PAD as B64_NO_PAD};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use prost_reflect::prost_types::field_descriptor_proto::{Label, Type};
use prost_reflect::prost_types::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, MessageOptions,
};
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::state::AppState;

pub const NAMESPACE: &str = "grpc-descriptors";
pub const REVISION_KEY: &str = "grpc:revision";
/// The value of `grpc-accept-encoding` FastMock sends.
pub const ACCEPT_ENCODING: &str = "gzip, identity";

#[derive(Default)]
pub struct Registry {
    pool: RwLock<Option<(String, DescriptorPool)>>,
}

impl Registry {
    pub async fn reload(&self, app: &AppState) -> anyhow::Result<usize> {
        let revision = app.stores.hot.get(REVISION_KEY).await?.unwrap_or_default();
        let items = app.stores.data.list(NAMESPACE, 1000).await?;
        let mut pool = DescriptorPool::new();
        let mut n = 0;
        for (name, v) in &items {
            let Some(b64) = v.get("fileDescriptorSet").and_then(|x| x.as_str()) else {
                continue;
            };
            match B64
                .decode(b64)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    pool.decode_file_descriptor_set(&bytes[..])
                        .map_err(anyhow::Error::from)
                }) {
                Ok(()) => n += 1,
                Err(e) => {
                    tracing::error!(descriptor = %name, error = %e, "invalid gRPC descriptor set skipped")
                }
            }
        }
        add_builtin_files(&mut pool);
        *self.pool.write().await = Some((revision, pool));
        Ok(n)
    }

    pub async fn refresh_if_changed(&self, app: &AppState) -> anyhow::Result<()> {
        let revision = app.stores.hot.get(REVISION_KEY).await?.unwrap_or_default();
        let current = self.pool.read().await.as_ref().map(|p| p.0.clone());
        if current.as_deref() != Some(revision.as_str()) {
            self.reload(app).await?;
        }
        Ok(())
    }

    pub async fn method(&self, path: &str) -> Option<MethodDescriptor> {
        let p = path.strip_prefix('/')?;
        let (service, method) = p.split_once('/')?;
        let guard = self.pool.read().await;
        let pool = &guard.as_ref()?.1;
        pool.get_service_by_name(service)?
            .methods()
            .find(|m| m.name() == method)
    }

    /// The loaded descriptors together with the built-in `google.rpc` status and error detail types.
    pub async fn pool(&self) -> DescriptorPool {
        match self.pool.read().await.as_ref() {
            Some((_, p)) => p.clone(),
            None => {
                let mut p = DescriptorPool::new();
                add_builtin_files(&mut p);
                p
            }
        }
    }

    pub async fn services(&self) -> Vec<String> {
        self.pool
            .read()
            .await
            .as_ref()
            .map(|p| p.1.services().map(|s| s.full_name().to_string()).collect())
            .unwrap_or_default()
    }
}

pub async fn check_compatible(app: &AppState, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let mut pool = DescriptorPool::new();
    for (other, v) in app.stores.data.list(NAMESPACE, 1000).await? {
        if other == name {
            continue;
        }
        if let Some(b) = v.get("fileDescriptorSet").and_then(|x| x.as_str())
            && let Ok(raw) = B64.decode(b)
        {
            let _ = pool.decode_file_descriptor_set(&raw[..]);
        }
    }
    pool.decode_file_descriptor_set(bytes)
        .map_err(|e| anyhow::anyhow!("conflicts with descriptors already loaded: {e}"))?;
    Ok(())
}

pub fn validate_descriptor_set(bytes: &[u8]) -> anyhow::Result<Vec<String>> {
    let mut pool = DescriptorPool::new();
    pool.decode_file_descriptor_set(bytes)?;
    Ok(pool.services().map(|s| s.full_name().to_string()).collect())
}

pub fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/grpc"))
}

/// The wire protocol of a gRPC request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `application/grpc`: HTTP/2 with trailers.
    Grpc,
    /// `application/grpc-web`: the trailers are the last frame of the body.
    Web,
    /// `application/grpc-web-text`: gRPC-Web, base64-encoded.
    WebText,
}

impl Protocol {
    pub fn detect(headers: &HeaderMap) -> Protocol {
        let ct = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ct.starts_with("application/grpc-web-text") {
            Protocol::WebText
        } else if ct.starts_with("application/grpc-web") {
            Protocol::Web
        } else {
            Protocol::Grpc
        }
    }

    pub fn journal_name(self) -> Option<&'static str> {
        match self {
            Protocol::Grpc => None,
            Protocol::Web => Some("grpc-web"),
            Protocol::WebText => Some("grpc-web-text"),
        }
    }

    pub fn content_type(self, request: Option<&str>) -> &'static str {
        let proto = request.is_some_and(|c| c.contains("+proto"));
        match (self, proto) {
            (Protocol::Grpc, _) => "application/grpc",
            (Protocol::Web, true) => "application/grpc-web+proto",
            (Protocol::Web, false) => "application/grpc-web",
            (Protocol::WebText, true) => "application/grpc-web-text+proto",
            (Protocol::WebText, false) => "application/grpc-web-text",
        }
    }
}

/// Message compression (`grpc-encoding`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Gzip,
}

impl Encoding {
    /// `Ok(None)` without the header, `Err` for an encoding FastMock does not support.
    pub fn from_header(headers: &HeaderMap) -> Result<Option<Encoding>, String> {
        let Some(v) = headers.get("grpc-encoding") else {
            return Ok(None);
        };
        match v
            .to_str()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "identity" => Ok(Some(Encoding::Identity)),
            "gzip" => Ok(Some(Encoding::Gzip)),
            other => Err(other.to_string()),
        }
    }

    pub fn accepted_by_client(headers: &HeaderMap, e: Encoding) -> bool {
        match e {
            Encoding::Identity => true,
            Encoding::Gzip => headers.get_all("grpc-accept-encoding").iter().any(|v| {
                v.to_str()
                    .is_ok_and(|s| s.split(',').any(|p| p.trim().eq_ignore_ascii_case("gzip")))
            }),
        }
    }
}

/// CRC-32 (IEEE) for gzip trailers.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

/// A length-prefixed message frame; compressed with gzip when `gzip` is set.
pub fn frame(payload: &[u8], gzip_it: bool) -> Bytes {
    let compressed;
    let (flag, body) = if gzip_it {
        compressed = gzip(payload);
        (1u8, &compressed[..])
    } else {
        (0u8, payload)
    };
    let mut b = BytesMut::with_capacity(body.len() + 5);
    b.put_u8(flag);
    b.put_u32(body.len() as u32);
    b.put_slice(body);
    b.freeze()
}

pub fn encode_message(method: &MethodDescriptor, json: &Value) -> Result<Vec<u8>, String> {
    let msg = DynamicMessage::deserialize(method.output(), json.clone()).map_err(|e| {
        format!(
            "stub message does not fit {}: {e}",
            method.output().full_name()
        )
    })?;
    Ok(msg.encode_to_vec())
}

pub fn decode_message(method: &MethodDescriptor, payload: &[u8]) -> Result<Value, String> {
    let msg = DynamicMessage::decode(method.input(), payload)
        .map_err(|e| format!("cannot decode {}: {e}", method.input().full_name()))?;
    serde_json::to_value(&msg).map_err(|e| e.to_string())
}

pub fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if (0x20..0x7f).contains(&b) && b != b'%' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The final status of a call with its trailing metadata.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub code: u32,
    pub message: String,
    /// `grpc-status-details-bin`, base64 without padding.
    pub details: Option<String>,
    pub trailers: Vec<(String, String)>,
}

impl Outcome {
    pub fn status(code: u32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            ..Default::default()
        }
    }

    /// The trailers as HTTP/2 trailers.
    pub fn header_map(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("grpc-status", HeaderValue::from(self.code));
        if !self.message.is_empty()
            && let Ok(v) = HeaderValue::from_str(&percent(&self.message))
        {
            h.insert("grpc-message", v);
        }
        if let Some(d) = &self.details
            && let Ok(v) = HeaderValue::from_str(d)
        {
            h.insert("grpc-status-details-bin", v);
        }
        for (k, v) in &self.trailers {
            if let (Ok(n), Ok(v)) = (
                axum::http::HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes()),
                HeaderValue::from_str(v),
            ) {
                h.append(n, v);
            }
        }
        h
    }

    /// The trailers as the gRPC-Web trailer frame (flag `0x80`).
    pub fn web_frame(&self) -> Bytes {
        let mut text = String::new();
        for (k, v) in &self.header_map() {
            text.push_str(k.as_str());
            text.push_str(": ");
            text.push_str(&String::from_utf8_lossy(v.as_bytes()));
            text.push_str("\r\n");
        }
        let mut b = BytesMut::with_capacity(text.len() + 5);
        b.put_u8(0x80);
        b.put_u32(text.len() as u32);
        b.put_slice(text.as_bytes());
        b.freeze()
    }
}

/// Encodes a `google.rpc.Status` with `details` (JSON objects with `@type`) for `grpc-status-details-bin`.
pub fn status_details(
    pool: &DescriptorPool,
    code: u32,
    message: &str,
    details: &[Value],
) -> Result<String, String> {
    let desc = pool
        .get_message_by_name("google.rpc.Status")
        .ok_or_else(|| "google.rpc.Status is not available".to_string())?;
    let v = json!({"code": code, "message": message, "details": details});
    let msg = DynamicMessage::deserialize(desc, v)
        .map_err(|e| format!("status details cannot be encoded: {e}"))?;
    Ok(B64_NO_PAD.encode(msg.encode_to_vec()))
}

/// Decodes `grpc-status-details-bin` back to JSON (tests and tools).
pub fn decode_status_details(pool: &DescriptorPool, b64: &str) -> Option<Value> {
    let raw = B64_NO_PAD
        .decode(b64.trim_end_matches('='))
        .ok()
        .or_else(|| B64.decode(b64).ok())?;
    let desc = pool.get_message_by_name("google.rpc.Status")?;
    let msg = DynamicMessage::decode(desc, &raw[..]).ok()?;
    serde_json::to_value(&msg).ok()
}

fn field(
    name: &str,
    number: i32,
    ty: Type,
    label: Label,
    type_name: Option<&str>,
) -> FieldDescriptorProto {
    let mut json_name = String::new();
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            json_name.extend(c.to_uppercase());
            upper = false;
        } else {
            json_name.push(c);
        }
    }
    FieldDescriptorProto {
        name: Some(name.into()),
        number: Some(number),
        label: Some(label as i32),
        r#type: Some(ty as i32),
        type_name: type_name.map(String::from),
        json_name: Some(json_name),
        ..Default::default()
    }
}

fn string(name: &str, number: i32) -> FieldDescriptorProto {
    field(name, number, Type::String, Label::Optional, None)
}

fn message(
    name: &str,
    fields: Vec<FieldDescriptorProto>,
    nested: Vec<DescriptorProto>,
) -> DescriptorProto {
    DescriptorProto {
        name: Some(name.into()),
        field: fields,
        nested_type: nested,
        ..Default::default()
    }
}

fn string_map_entry(name: &str) -> DescriptorProto {
    DescriptorProto {
        name: Some(name.into()),
        field: vec![string("key", 1), string("value", 2)],
        options: Some(MessageOptions {
            map_entry: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `google/rpc/status.proto` and the common types of `google/rpc/error_details.proto`.
pub fn builtin_files() -> Vec<FileDescriptorProto> {
    use Label::{Optional as One, Repeated as Many};
    let status = FileDescriptorProto {
        name: Some("google/rpc/status.proto".into()),
        package: Some("google.rpc".into()),
        dependency: vec!["google/protobuf/any.proto".into()],
        syntax: Some("proto3".into()),
        message_type: vec![message(
            "Status",
            vec![
                field("code", 1, Type::Int32, One, None),
                string("message", 2),
                field(
                    "details",
                    3,
                    Type::Message,
                    Many,
                    Some(".google.protobuf.Any"),
                ),
            ],
            vec![],
        )],
        ..Default::default()
    };
    let details = FileDescriptorProto {
        name: Some("google/rpc/error_details.proto".into()),
        package: Some("google.rpc".into()),
        dependency: vec!["google/protobuf/duration.proto".into()],
        syntax: Some("proto3".into()),
        message_type: vec![
            message(
                "ErrorInfo",
                vec![
                    string("reason", 1),
                    string("domain", 2),
                    field(
                        "metadata",
                        3,
                        Type::Message,
                        Many,
                        Some(".google.rpc.ErrorInfo.MetadataEntry"),
                    ),
                ],
                vec![string_map_entry("MetadataEntry")],
            ),
            message(
                "RetryInfo",
                vec![field(
                    "retry_delay",
                    1,
                    Type::Message,
                    One,
                    Some(".google.protobuf.Duration"),
                )],
                vec![],
            ),
            message(
                "DebugInfo",
                vec![
                    field("stack_entries", 1, Type::String, Many, None),
                    string("detail", 2),
                ],
                vec![],
            ),
            message(
                "QuotaFailure",
                vec![field(
                    "violations",
                    1,
                    Type::Message,
                    Many,
                    Some(".google.rpc.QuotaFailure.Violation"),
                )],
                vec![message(
                    "Violation",
                    vec![string("subject", 1), string("description", 2)],
                    vec![],
                )],
            ),
            message(
                "PreconditionFailure",
                vec![field(
                    "violations",
                    1,
                    Type::Message,
                    Many,
                    Some(".google.rpc.PreconditionFailure.Violation"),
                )],
                vec![message(
                    "Violation",
                    vec![
                        string("type", 1),
                        string("subject", 2),
                        string("description", 3),
                    ],
                    vec![],
                )],
            ),
            message(
                "BadRequest",
                vec![field(
                    "field_violations",
                    1,
                    Type::Message,
                    Many,
                    Some(".google.rpc.BadRequest.FieldViolation"),
                )],
                vec![message(
                    "FieldViolation",
                    vec![
                        string("field", 1),
                        string("description", 2),
                        string("reason", 3),
                        field(
                            "localized_message",
                            4,
                            Type::Message,
                            One,
                            Some(".google.rpc.LocalizedMessage"),
                        ),
                    ],
                    vec![],
                )],
            ),
            message(
                "RequestInfo",
                vec![string("request_id", 1), string("serving_data", 2)],
                vec![],
            ),
            message(
                "ResourceInfo",
                vec![
                    string("resource_type", 1),
                    string("resource_name", 2),
                    string("owner", 3),
                    string("description", 4),
                ],
                vec![],
            ),
            message(
                "Help",
                vec![field(
                    "links",
                    1,
                    Type::Message,
                    Many,
                    Some(".google.rpc.Help.Link"),
                )],
                vec![message(
                    "Link",
                    vec![string("description", 1), string("url", 2)],
                    vec![],
                )],
            ),
            message(
                "LocalizedMessage",
                vec![string("locale", 1), string("message", 2)],
                vec![],
            ),
        ],
        ..Default::default()
    };
    vec![status, details]
}

/// Adds the well-known types the built-in files need and the built-in files, skipping files the uploaded
/// descriptor sets already contain.
fn add_builtin_files(pool: &mut DescriptorPool) {
    let global = DescriptorPool::global();
    for name in [
        "google/protobuf/any.proto",
        "google/protobuf/duration.proto",
    ] {
        if pool.get_file_by_name(name).is_none()
            && let Some(f) = global.get_file_by_name(name)
            && let Err(e) = pool.add_file_descriptor_proto(f.file_descriptor_proto().clone())
        {
            tracing::warn!(file = name, error = %e, "adding a well-known type failed");
        }
    }
    for f in builtin_files() {
        let name = f.name().to_string();
        if pool.get_file_by_name(&name).is_some() {
            continue;
        }
        if let Err(e) = pool.add_file_descriptor_proto(f) {
            tracing::debug!(file = %name, error = %e, "built-in google.rpc types not added");
        }
    }
}

pub type Shared = Arc<Registry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_details_round_trip() {
        let mut pool = DescriptorPool::new();
        add_builtin_files(&mut pool);
        let details = vec![
            json!({"@type": "type.googleapis.com/google.rpc.BadRequest",
                   "fieldViolations": [{"field": "id", "description": "must not be empty"}]}),
            json!({"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "1.500s"}),
            json!({"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "STOCK",
                   "domain": "shop.example.com", "metadata": {"sku": "A-1"}}),
        ];
        let b64 = status_details(&pool, 3, "bad", &details).unwrap();
        let back = decode_status_details(&pool, &b64).unwrap();
        assert_eq!(back["code"], 3);
        assert_eq!(back["details"][0]["fieldViolations"][0]["field"], "id");
        assert_eq!(back["details"][1]["retryDelay"], "1.500s");
        assert_eq!(back["details"][2]["metadata"]["sku"], "A-1");
        assert!(
            status_details(
                &pool,
                3,
                "x",
                &[json!({"@type": "type.googleapis.com/x.Unknown"})]
            )
            .is_err()
        );
    }

    #[test]
    fn frames_and_trailers() {
        let f = frame(b"abc", false);
        assert_eq!(&f[..], &[0, 0, 0, 0, 3, b'a', b'b', b'c']);
        let z = frame(&b"x".repeat(1000), true);
        assert_eq!(z[0], 1);
        let out = crate::mail_providers::gunzip(&z[5..], 2000).unwrap();
        assert_eq!(out.len(), 1000);
        let o = Outcome {
            code: 5,
            message: "not found: é".into(),
            details: None,
            trailers: vec![("x-shard".into(), "7".into())],
        };
        let web = o.web_frame();
        assert_eq!(web[0], 0x80);
        let text = String::from_utf8(web[5..].to_vec()).unwrap();
        assert!(text.contains("grpc-status: 5\r\n"), "{text}");
        assert!(
            text.contains("grpc-message: not found: %C3%A9\r\n"),
            "{text}"
        );
        assert!(text.contains("x-shard: 7\r\n"), "{text}");
    }
}
