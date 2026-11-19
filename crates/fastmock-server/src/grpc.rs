use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::{BufMut, Bytes, BytesMut};
use http_body_util::StreamBody;
use hyper::body::Frame;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::state::AppState;

pub const NAMESPACE: &str = "grpc-descriptors";
pub const REVISION_KEY: &str = "grpc:revision";

#[derive(Clone, Debug)]
pub struct DecodedBody(pub String);

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

pub fn decode_request(method: &MethodDescriptor, body: &[u8]) -> Result<Value, (u32, String)> {
    if body.len() < 5 {
        return Err((13, "malformed gRPC frame".into()));
    }
    if body[0] != 0 {
        return Err((12, "compressed gRPC messages are not supported".into()));
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if body.len() < 5 + len {
        return Err((13, "truncated gRPC frame".into()));
    }
    let msg = DynamicMessage::decode(method.input(), &body[5..5 + len]).map_err(|e| {
        (
            13,
            format!("cannot decode {}: {e}", method.input().full_name()),
        )
    })?;
    serde_json::to_value(&msg).map_err(|e| (13, e.to_string()))
}

pub fn encode_response(method: &MethodDescriptor, json: &Value) -> Result<Bytes, String> {
    let msg = DynamicMessage::deserialize(method.output(), json.clone()).map_err(|e| {
        format!(
            "stub body does not fit {}: {e}",
            method.output().full_name()
        )
    })?;
    let payload = msg.encode_to_vec();
    let mut b = BytesMut::with_capacity(payload.len() + 5);
    b.put_u8(0);
    b.put_u32(payload.len() as u32);
    b.put_slice(&payload);
    Ok(b.freeze())
}

fn percent(s: &str) -> String {
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

pub fn response(message: Option<Bytes>, status: u32, msg: &str) -> Response {
    let mut trailers = HeaderMap::new();
    trailers.insert(
        "grpc-status",
        HeaderValue::from_str(&status.to_string()).unwrap(),
    );
    if !msg.is_empty()
        && let Ok(v) = HeaderValue::from_str(&percent(msg))
    {
        trailers.insert("grpc-message", v);
    }
    let mut frames: Vec<Result<Frame<Bytes>, std::io::Error>> = Vec::new();
    if let Some(m) = message {
        frames.push(Ok(Frame::data(m)));
    }
    frames.push(Ok(Frame::trailers(trailers)));
    let mut r = Response::new(Body::new(StreamBody::new(futures::stream::iter(frames))));
    *r.status_mut() = StatusCode::OK;
    r.headers_mut()
        .insert("content-type", HeaderValue::from_static("application/grpc"));
    r.extensions_mut().insert(crate::extras::Streaming);
    r
}

pub type Shared = Arc<Registry>;
