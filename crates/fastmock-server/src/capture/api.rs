//! `/__admin/capture/...`: capture sources, captured messages, find, wait and verify. Every route answers
//! 404 while `FASTMOCK_CAPTURE_ENABLED` is off. Permissions: `messages:read` (sources, messages, find,
//! wait, verify), `messages:write` (sources), `messages:delete` (messages), all grantable per space.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fastmock_core::capture::{
    BodyView, CaptureQuery, CaptureSource, CapturedMessage, CompiledMessageMatch, MAX_PAGE,
    MessageMatch,
};
use fastmock_core::{MessageProtocol, SpaceFilter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::{
    CaptureService, DecodedBody, NAMESPACE, SourceStatus, decode, decode_with, load_sources,
    resolve_schema, statuses,
};
use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad, not_found};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

pub const MAX_WAIT_MS: u64 = 60_000;
pub const DEFAULT_WAIT_MS: u64 = 10_000;
pub const MAX_WAIT_COUNT: usize = 500;
/// Stored messages one find, wait or verify looks at, at most.
pub const MAX_SCAN: usize = 10_000;
/// Body bytes one find, wait or verify loads and matches, at most; only the bodies of messages that meet
/// the other conditions count.
pub const MAX_SCAN_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// Body bytes loaded at once.
const BODY_BATCH_BYTES: u64 = 8 * 1024 * 1024;
/// Schema registry requests one find, wait or verify makes; cached answers are free.
const MAX_SCHEMA_LOOKUPS: usize = 16;
const PAGE: usize = 200;
const DEFAULT_PAGE: usize = 50;
const MAX_NEAR_MISSES: usize = 5;
/// How far back a wait reads again on each scan (late commits, clock differences between replicas).
const RESCAN_OVERLAP_SECS: i64 = 10;
const SCAN_QUEUE_WAIT: Duration = Duration::from_secs(5);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/capture/sources", get(list_sources))
        .route(
            "/capture/sources/{id}",
            get(get_source).put(put_source).delete(delete_source),
        )
        .route(
            "/capture/messages",
            get(list_messages).delete(delete_messages),
        )
        .route("/capture/messages/find", post(find_messages))
        .route("/capture/messages/wait", post(wait))
        .route("/capture/verify", post(verify))
        .route(
            "/capture/messages/{id}",
            get(get_message).delete(delete_message),
        )
        .route("/capture/messages/{id}/body", get(body))
}

/// The capture module, or 404 while it is off.
pub fn service(app: &AppState) -> ApiResult<Arc<CaptureService>> {
    app.capture.clone().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "message capture is not enabled; set FASTMOCK_CAPTURE_ENABLED=true".into(),
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------------------

/// A source with what the consumer reports.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    #[serde(flatten)]
    pub source: CaptureSource,
    pub status: SourceStatus,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceList {
    pub sources: Vec<SourceView>,
    /// `FASTMOCK_CAPTURE_MAX_SOURCES`.
    pub max_sources: usize,
    /// `FASTMOCK_CAPTURE_MAX_MESSAGES`: messages kept per source at most.
    pub max_messages: usize,
}

async fn views(app: &AppState, svc: &CaptureService, list: Vec<CaptureSource>) -> Vec<SourceView> {
    let st = statuses(app).await;
    let counts = svc.store.counts().await.unwrap_or_default();
    list.into_iter()
        .map(|s| {
            let mut status = st.get(&s.id).cloned().unwrap_or_else(|| SourceStatus {
                state: if s.enabled { "stopped" } else { "disabled" }.into(),
                ..Default::default()
            });
            if !s.enabled {
                status.state = "disabled".into();
            }
            status.stored = counts.get(&s.id).copied().unwrap_or(0);
            SourceView { source: s, status }
        })
        .collect()
}

#[utoipa::path(
    get, path = "/__admin/capture/sources", tag = "capture", operation_id = "listCaptureSources",
    summary = "List capture sources",
    description = "The sources of the areas the caller may read (`messages:read`), from the configuration and the API, with the state of their consumer and the number of stored messages.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Sources", body = SourceList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, or an unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_sources(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<SourceList>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MessagesRead, q.space.as_deref())?;
    let list: Vec<CaptureSource> = load_sources(&app)
        .await?
        .into_iter()
        .filter(|s| spaces.matches(s.space.as_deref()))
        .collect();
    Ok(Json(SourceList {
        sources: views(&app, &svc, list).await,
        max_sources: svc.config.max_sources,
        max_messages: svc.config.max_messages,
    }))
}

async fn find_source(
    app: &AppState,
    p: &Principal,
    id: Uuid,
    perm: Permission,
) -> ApiResult<CaptureSource> {
    let s = load_sources(app)
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| not_found("capture source"))?;
    spaces::item(
        p,
        Permission::MessagesRead,
        perm,
        s.space.as_deref(),
        "capture source",
    )?;
    Ok(s)
}

#[utoipa::path(
    get, path = "/__admin/capture/sources/{id}", tag = "capture", operation_id = "getCaptureSource",
    summary = "One capture source",
    params(("id" = Uuid, Path, description = "Source id")),
    responses(
        (status = 200, description = "The source", body = SourceView),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Not found, or capture is not enabled", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_source(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SourceView>> {
    let svc = service(&app)?;
    let s = find_source(&app, &p, id, Permission::MessagesRead).await?;
    Ok(Json(views(&app, &svc, vec![s]).await.remove(0)))
}

#[utoipa::path(
    put, path = "/__admin/capture/sources/{id}", tag = "capture", operation_id = "putCaptureSource",
    summary = "Create or replace a capture source",
    description = "Stores a source in `space` (or the caller's only space); the leading replica subscribes within a few seconds. Kafka sources read the partitions of their `topics` directly, without a consumer group; MQTT sources subscribe to the topic filters in `topics`; AMQP sources bind a server-named, exclusive, auto-delete queue of FastMock's own to `exchange` with `bindingKeys`, so the application's queues never lose a message. Sources from the configuration cannot be changed (409). Audited as `capture.source.put`.",
    params(("id" = Uuid, Path, description = "Source id"), SpaceQuery),
    request_body(content = CaptureSource, example = json!({"name": "orders", "protocol": "kafka", "topics": ["orders.events"], "startFrom": "latest"})),
    responses(
        (status = 200, description = "Stored", body = SourceView),
        (status = 400, description = "Invalid source, broker not configured, not allowed, name taken or too many sources", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller lacks messages:write", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, or an unknown space", body = ErrorBody),
        (status = 409, description = "The source comes from the configuration", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn put_source(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<SourceView>> {
    let svc = service(&app)?;
    let mut s: CaptureSource =
        serde_json::from_value(body).map_err(|e| bad(format!("invalid capture source: {e}")))?;
    let space = spaces::target(&app, &p, Permission::MessagesWrite, q.space.as_deref())?;
    s.id = id;
    s.space = space.clone();
    s.from_config = false;
    s.validate().map_err(bad)?;
    if let Some(setting) = super::missing_setting(&app.cfg, s.protocol) {
        return Err(bad(format!("{setting} is not configured")));
    }
    super::allowed(&svc.config, &s).map_err(bad)?;
    if let Some(m) = s.max_messages
        && m > svc.config.max_messages
    {
        return Err(bad(format!(
            "maxMessages is at most {} (FASTMOCK_CAPTURE_MAX_MESSAGES)",
            svc.config.max_messages
        )));
    }
    let all = load_sources(&app).await?;
    if let Some(existing) = all.iter().find(|x| x.id == id) {
        if existing.from_config {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "this source is defined in the server configuration and cannot be changed".into(),
            ));
        }
        if existing.space != space {
            spaces::item(
                &p,
                Permission::MessagesRead,
                Permission::MessagesWrite,
                existing.space.as_deref(),
                "capture source",
            )?;
            return Err(bad(
                "the space of a source cannot change; delete it and create it again",
            ));
        }
    } else if all.len() >= svc.config.max_sources {
        return Err(bad(format!(
            "at most {} capture sources (FASTMOCK_CAPTURE_MAX_SOURCES)",
            svc.config.max_sources
        )));
    }
    if all
        .iter()
        .any(|x| x.id != id && x.space == space && x.name == s.name)
    {
        return Err(bad(format!(
            "a capture source named {} exists in this space",
            s.name
        )));
    }
    s.updated_at = Some(Utc::now());
    app.stores
        .data
        .put(
            NAMESPACE,
            &id.to_string(),
            &serde_json::to_value(&s).map_err(anyhow::Error::from)?,
        )
        .await?;
    audit_event_in(
        &app,
        &p,
        space.as_deref(),
        "capture.source.put",
        json!({"id": id, "name": s.name, "protocol": s.protocol, "destinations": s.destinations(), "enabled": s.enabled}),
    )
    .await;
    Ok(Json(views(&app, &svc, vec![s]).await.remove(0)))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteSourceQuery {
    /// Delete the source's stored messages too.
    pub purge: Option<bool>,
}

#[utoipa::path(
    delete, path = "/__admin/capture/sources/{id}", tag = "capture", operation_id = "deleteCaptureSource",
    summary = "Delete a capture source",
    description = "Stops its consumer within a few seconds. Stored messages stay until retention removes them (age, or the storage budgets, oldest first), unless `purge=true`, which also needs `messages:delete` and is checked before anything is deleted. Audited as `capture.source.delete`.",
    params(("id" = Uuid, Path, description = "Source id"), DeleteSourceQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller lacks messages:write, or messages:delete with purge=true", body = ErrorBody),
        (status = 404, description = "Not found, or capture is not enabled", body = ErrorBody),
        (status = 409, description = "The source comes from the configuration", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_source(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<DeleteSourceQuery>,
) -> ApiResult<StatusCode> {
    let svc = service(&app)?;
    let s = find_source(&app, &p, id, Permission::MessagesWrite).await?;
    if s.from_config {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this source is defined in the server configuration and cannot be deleted".into(),
        ));
    }
    let purge = q.purge == Some(true);
    if purge {
        crate::admin::require(&p, Permission::MessagesDelete, s.space.as_deref())?;
    }
    app.stores.data.delete(NAMESPACE, &id.to_string()).await?;
    let _ = app
        .stores
        .hot
        .delete(&format!("{}{id}", super::STATUS_PREFIX))
        .await;
    let mut purged = 0;
    if purge {
        purged = svc
            .store
            .delete_matching(&SpaceFilter::area(s.space.as_deref()), Some(id))
            .await?;
    }
    audit_event_in(
        &app,
        &p,
        s.space.as_deref(),
        "capture.source.delete",
        json!({"id": id, "name": s.name, "purged": purged}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// A stored message without its body.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub source_id: Uuid,
    /// The name of the source.
    pub source: String,
    pub protocol: MessageProtocol,
    /// Kafka or MQTT topic; AMQP routing key.
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub headers: std::collections::BTreeMap<String, String>,
    #[schema(value_type = Object)]
    pub properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub size: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub received_at: DateTime<Utc>,
}

impl From<&CapturedMessage> for MessageSummary {
    fn from(m: &CapturedMessage) -> Self {
        Self {
            id: m.id,
            space: m.space.clone(),
            source_id: m.source_id,
            source: m.source.clone(),
            protocol: m.protocol,
            topic: m.topic.clone(),
            exchange: m.exchange.clone(),
            key: m.key.clone(),
            headers: m.headers.clone(),
            properties: m.properties.clone(),
            content_type: m.content_type.clone(),
            size: m.size,
            truncated: m.truncated,
            preview: m.preview.clone(),
            received_at: m.received_at,
        }
    }
}

/// A message with its decoded body.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageDetail {
    #[serde(flatten)]
    pub message: MessageSummary,
    pub body: DecodedBody,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageList {
    pub messages: Vec<MessageSummary>,
    /// Pass as `cursor` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Space to list; without it every area the caller may read.
    pub space: Option<String>,
    /// Source name or id.
    pub source: Option<String>,
    /// `kafka`, `mqtt` or `amqp`.
    pub protocol: Option<String>,
    /// Exact topic or AMQP routing key.
    pub topic: Option<String>,
    /// Received at or after (RFC 3339).
    pub since: Option<DateTime<Utc>>,
    /// Received before (RFC 3339).
    pub until: Option<DateTime<Utc>>,
    /// 1 to 500 (default 50).
    pub limit: Option<usize>,
    /// `nextCursor` of the previous page.
    pub cursor: Option<Uuid>,
    /// `newest` (default) or `oldest` first.
    pub order: Option<String>,
}

/// The source ids a `source` filter (name or id) selects among the readable sources.
async fn source_ids(app: &AppState, spaces: &SpaceFilter, source: &str) -> ApiResult<Vec<Uuid>> {
    let all = load_sources(app).await?;
    let ids: Vec<Uuid> = all
        .iter()
        .filter(|s| spaces.matches(s.space.as_deref()))
        .filter(|s| s.name == source || s.id.to_string() == source)
        .map(|s| s.id)
        .collect();
    if ids.is_empty() {
        if let Ok(id) = Uuid::parse_str(source) {
            return Ok(vec![id]);
        }
        return Err(not_found("capture source"));
    }
    Ok(ids)
}

fn parse_protocol(p: Option<&str>) -> ApiResult<Option<MessageProtocol>> {
    p.filter(|x| !x.is_empty())
        .map(|x| {
            MessageProtocol::parse(x).ok_or_else(|| bad("protocol must be kafka, mqtt or amqp"))
        })
        .transpose()
}

fn parse_order(o: Option<&str>) -> ApiResult<bool> {
    match o.map(str::trim) {
        None | Some("") | Some("newest") => Ok(false),
        Some("oldest") => Ok(true),
        Some(_) => Err(bad("order must be newest or oldest")),
    }
}

#[utoipa::path(
    get, path = "/__admin/capture/messages", tag = "capture", operation_id = "listCapturedMessages",
    summary = "List captured messages",
    description = "Summaries (no bodies) of the stored messages of the areas the caller may read, newest first unless `order=oldest`, filtered by source, protocol, topic and time, paged with `cursor`. For conditions on keys, headers, properties and bodies use `POST /capture/messages/find`.",
    params(ListQuery),
    responses(
        (status = 200, description = "A page of messages", body = MessageList),
        (status = 400, description = "Invalid filter, limit, order or cursor", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, an unknown space or source", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_messages(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<MessageList>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MessagesRead, q.space.as_deref())?;
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(bad(format!("limit must be between 1 and {MAX_PAGE}")));
    }
    let oldest_first = parse_order(q.order.as_deref())?;
    let protocol = parse_protocol(q.protocol.as_deref())?;
    let ids = match q.source.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(source_ids(&app, &spaces, s).await?),
        None => None,
    };
    let mut query = CaptureQuery {
        spaces,
        protocol,
        topic: q.topic.clone().filter(|t| !t.is_empty()),
        since: q.since,
        until: q.until,
        limit,
        cursor: q.cursor,
        oldest_first,
        with_body: false,
        source_id: None,
    };
    let page = match ids.as_deref() {
        None => svc.store.list(&query).await?,
        Some([one]) => {
            query.source_id = Some(*one);
            svc.store.list(&query).await?
        }
        Some(many) => {
            let mut items = Vec::new();
            for id in many {
                query.source_id = Some(*id);
                items.extend(svc.store.list(&query).await?.items);
            }
            items.sort_by_key(|m| m.id);
            if !oldest_first {
                items.reverse();
            }
            let more = items.len() > limit;
            items.truncate(limit);
            fastmock_core::capture::CapturePage {
                next: if more {
                    items.last().map(|m| m.id)
                } else {
                    None
                },
                items,
            }
        }
    };
    Ok(Json(MessageList {
        messages: page.items.iter().map(MessageSummary::from).collect(),
        next_cursor: page.next.map(|c| c.to_string()),
    }))
}

/// One stored message the caller may act on with `perm`.
async fn find_one(
    app: &AppState,
    svc: &CaptureService,
    p: &Principal,
    id: Uuid,
    space: Option<&str>,
    perm: Permission,
) -> ApiResult<CapturedMessage> {
    let filter = match space {
        Some(s) => {
            spaces::known(app, p, s)?;
            SpaceFilter::In(vec![s.to_string()])
        }
        None => SpaceFilter::Any,
    };
    let m = svc
        .store
        .get(id, &filter)
        .await?
        .ok_or_else(|| not_found("message"))?;
    spaces::item(
        p,
        Permission::MessagesRead,
        perm,
        m.space.as_deref(),
        "message",
    )?;
    Ok(m)
}

#[utoipa::path(
    get, path = "/__admin/capture/messages/{id}", tag = "capture", operation_id = "getCapturedMessage",
    summary = "One captured message with its decoded body",
    description = "The body is decoded as `json`, `text`, `avro` (Confluent-framed Avro, with the schema from `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL`) or `base64` for other binary data. `/body` returns the bytes.",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "The message", body = MessageDetail),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Not found, or capture is not enabled", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_message(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MessageDetail>> {
    let svc = service(&app)?;
    let m = find_one(
        &app,
        &svc,
        &p,
        id,
        q.space.as_deref(),
        Permission::MessagesRead,
    )
    .await?;
    Ok(Json(MessageDetail {
        body: decode(&app, &m).await,
        message: MessageSummary::from(&m),
    }))
}

#[utoipa::path(
    get, path = "/__admin/capture/messages/{id}/body", tag = "capture", operation_id = "getCapturedMessageBody",
    summary = "The body of a captured message as stored",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "The bytes, with the message's content type (application/octet-stream otherwise)", content_type = "application/octet-stream"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Not found, or capture is not enabled", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn body(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let m = find_one(
        &app,
        &svc,
        &p,
        id,
        q.space.as_deref(),
        Permission::MessagesRead,
    )
    .await?;
    let ct = m
        .content_type
        .as_deref()
        .and_then(|c| HeaderValue::from_str(c).ok())
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let mut r = Response::new(Body::from(m.body.clone()));
    r.headers_mut().insert(header::CONTENT_TYPE, ct);
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    r.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{id}.bin\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    Ok(r)
}

#[utoipa::path(
    delete, path = "/__admin/capture/messages/{id}", tag = "capture", operation_id = "deleteCapturedMessage",
    summary = "Delete one captured message",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller lacks messages:delete", body = ErrorBody),
        (status = 404, description = "Not found, or capture is not enabled", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_message(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    let svc = service(&app)?;
    let m = find_one(
        &app,
        &svc,
        &p,
        id,
        q.space.as_deref(),
        Permission::MessagesDelete,
    )
    .await?;
    svc.store
        .delete(id, &SpaceFilter::area(m.space.as_deref()))
        .await?;
    audit_event_in(
        &app,
        &p,
        m.space.as_deref(),
        "capture.message.delete",
        json!({"id": id}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteQuery {
    pub space: Option<String>,
    /// Only the messages of this source (name or id).
    pub source: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Deleted {
    pub deleted: u64,
}

#[utoipa::path(
    delete, path = "/__admin/capture/messages", tag = "capture", operation_id = "deleteCapturedMessages",
    summary = "Delete captured messages",
    description = "Deletes the stored messages of `space` (or of every area where the caller holds `messages:delete`), of one source when `source` is given. Use it to reset between tests. Audited as `capture.messages.delete`.",
    params(DeleteQuery),
    responses(
        (status = 200, description = "Deleted", body = Deleted),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller lacks messages:delete", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, an unknown space or source", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_messages(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<Json<Deleted>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MessagesDelete, q.space.as_deref())?;
    let mut deleted = 0;
    match q.source.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => {
            for id in source_ids(&app, &spaces, s).await? {
                deleted += svc.store.delete_matching(&spaces, Some(id)).await?;
            }
        }
        None => deleted = svc.store.delete_matching(&spaces, None).await?,
    }
    audit_event_in(
        &app,
        &p,
        q.space.as_deref(),
        "capture.messages.delete",
        json!({"source": q.source, "deleted": deleted}),
    )
    .await;
    Ok(Json(Deleted { deleted }))
}

// ---------------------------------------------------------------------------------------------
// Find, wait, verify
// ---------------------------------------------------------------------------------------------

struct Matcher {
    compiled: CompiledMessageMatch,
    body: bool,
    /// A body matcher reads JSON: parse it, and decode Avro for it.
    json: bool,
}

impl Matcher {
    fn new(m: &MessageMatch) -> ApiResult<Arc<Self>> {
        Ok(Arc::new(Self {
            compiled: CompiledMessageMatch::compile(m)
                .map_err(|e| bad(format!("invalid match: {e}")))?,
            body: m.needs_body(),
            json: m.needs_json(),
        }))
    }

    /// The failed body conditions of a message with its body (CPU work: called off the async workers).
    fn body_failures(
        &self,
        m: &CapturedMessage,
        schema: Option<Result<Arc<apache_avro::Schema>, String>>,
    ) -> Vec<String> {
        let decoded = self.json.then(|| decode_with(m, schema));
        let text = m.text();
        self.compiled.body_failures(&BodyView {
            text: &text,
            raw: &m.body,
            json: decoded.as_ref().and_then(|d| d.json()),
        })
    }
}

/// Global limit of concurrent finds, waits and verifications, and the share one space (or, across
/// spaces, one caller) may take.
const SCANS_TOTAL: usize = 8;
const SCANS_PER_KEY: usize = 2;

struct ScanLimiter {
    total: Arc<Semaphore>,
    per_key_limit: usize,
    per_key: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
}

static LIMITER: std::sync::LazyLock<ScanLimiter> =
    std::sync::LazyLock::new(|| ScanLimiter::new(SCANS_TOTAL, SCANS_PER_KEY));

/// Held while a scan reads and matches stored messages.
pub struct ScanPermit {
    _key: tokio::sync::OwnedSemaphorePermit,
    _total: tokio::sync::OwnedSemaphorePermit,
}

/// Who a scan counts against: the one space it reads, otherwise the caller.
fn scan_key(p: &Principal, spaces: &SpaceFilter) -> String {
    match spaces {
        SpaceFilter::In(l) if l.len() == 1 => format!("space:{}", l[0]),
        SpaceFilter::Global => "global".into(),
        _ => format!("principal:{}", p.name),
    }
}

impl ScanLimiter {
    fn new(total: usize, per_key: usize) -> Self {
        Self {
            total: Arc::new(Semaphore::new(total)),
            per_key_limit: per_key,
            per_key: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// A permit of `key`'s share and of the total, waiting at most `wait` for both.
    async fn acquire(&self, key: String, wait: Duration) -> ApiResult<ScanPermit> {
        let busy = || {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many message searches are reading stored messages; retry in a moment".into(),
            )
        };
        let sem = {
            let mut m = self.per_key.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() >= 1024 {
                m.retain(|_, s| Arc::strong_count(s) > 1);
            }
            m.entry(key)
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_key_limit)))
                .clone()
        };
        let deadline = tokio::time::Instant::now() + wait;
        let key = match tokio::time::timeout_at(deadline, sem.acquire_owned()).await {
            Ok(Ok(p)) => p,
            _ => return Err(busy()),
        };
        let total =
            match tokio::time::timeout_at(deadline, self.total.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                _ => return Err(busy()),
            };
        Ok(ScanPermit {
            _key: key,
            _total: total,
        })
    }
}

async fn scan_permit(key: String) -> ApiResult<ScanPermit> {
    LIMITER.acquire(key, SCAN_QUEUE_WAIT).await
}

/// A message a scan looked at: without its body, with the failed conditions (empty: it matches).
struct Seen {
    message: CapturedMessage,
    failures: Vec<String>,
    /// The body conditions were checked (otherwise other conditions failed first, or there are none).
    body_checked: bool,
}

fn too_much(what: String) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, what)
}

/// Reads the stored messages of `spaces` received at or after `since`, oldest first, a page at a time and
/// without keeping them: checks the conditions other than the body, then loads the bodies of the messages
/// that meet them in batches of at most [`BODY_BATCH_BYTES`] and checks the body conditions off the async
/// workers. Messages for which `skip` holds are passed over. `visit` gets each message looked at and
/// returns `false` to stop. Fails with 422 beyond [`MAX_SCAN`] messages or [`MAX_SCAN_BODY_BYTES`] of
/// bodies.
async fn scan(
    app: &AppState,
    svc: &CaptureService,
    spaces: &SpaceFilter,
    since: Option<DateTime<Utc>>,
    matcher: &Arc<Matcher>,
    skip: &(dyn Fn(&Uuid) -> bool + Sync),
    visit: &mut (dyn FnMut(Seen) -> bool + Send),
) -> ApiResult<()> {
    let mut cursor = None;
    let mut looked = 0usize;
    let mut body_bytes = 0u64;
    let mut lookups = MAX_SCHEMA_LOOKUPS;
    loop {
        let page = svc
            .store
            .list(&CaptureQuery {
                spaces: spaces.clone(),
                since,
                limit: PAGE,
                cursor,
                oldest_first: true,
                with_body: false,
                ..Default::default()
            })
            .await?;
        looked += page.items.len();
        if looked > MAX_SCAN {
            return Err(too_much(format!(
                "more than {MAX_SCAN} stored messages to check; narrow it with space or since"
            )));
        }
        let items: Vec<CapturedMessage> = page.items.into_iter().filter(|m| !skip(&m.id)).collect();
        let m2 = matcher.clone();
        let checked: Vec<(CapturedMessage, Vec<String>)> = tokio::task::spawn_blocking(move || {
            items
                .into_iter()
                .map(|m| {
                    let f = m2.compiled.meta_failures(&m);
                    (m, f)
                })
                .collect()
        })
        .await
        .map_err(anyhow::Error::from)?;
        let mut pending: Vec<CapturedMessage> = Vec::new();
        for (m, failures) in checked {
            if failures.is_empty() && matcher.body {
                pending.push(m);
                continue;
            }
            let body_checked = !matcher.body;
            if !visit(Seen {
                message: m,
                failures,
                body_checked,
            }) {
                return Ok(());
            }
        }
        let mut rest = pending.into_iter().peekable();
        while rest.peek().is_some() {
            let mut batch = Vec::new();
            let mut batch_bytes = 0u64;
            while let Some(m) = rest.peek() {
                let n = if m.truncated {
                    m.size.min(svc.config.max_message_bytes as u64)
                } else {
                    m.size
                };
                if !batch.is_empty() && batch_bytes + n > BODY_BATCH_BYTES {
                    break;
                }
                batch_bytes += n;
                batch.push(rest.next().unwrap_or_else(|| unreachable!()));
            }
            body_bytes += batch_bytes;
            if body_bytes > MAX_SCAN_BODY_BYTES {
                return Err(too_much(format!(
                    "the messages to check hold more than {} MiB of bodies; narrow it with space, since or conditions on source, topic or headers",
                    MAX_SCAN_BODY_BYTES / (1024 * 1024)
                )));
            }
            let ids: Vec<Uuid> = batch.iter().map(|m| m.id).collect();
            let mut bodies = svc.store.bodies(&ids).await?;
            let mut items = Vec::with_capacity(batch.len());
            for mut m in batch {
                let Some(b) = bodies.remove(&m.id) else {
                    continue;
                };
                m.body = b;
                let schema = if matcher.json {
                    resolve_schema(app, &m, &mut lookups).await
                } else {
                    None
                };
                items.push((m, schema));
            }
            let m2 = matcher.clone();
            let results: Vec<(CapturedMessage, Vec<String>)> =
                tokio::task::spawn_blocking(move || {
                    items
                        .into_iter()
                        .map(|(m, schema)| {
                            let f = m2.body_failures(&m, schema);
                            (m.without_body(), f)
                        })
                        .collect()
                })
                .await
                .map_err(anyhow::Error::from)?;
            for (message, failures) in results {
                if !visit(Seen {
                    message,
                    failures,
                    body_checked: true,
                }) {
                    return Ok(());
                }
            }
        }
        match page.next {
            Some(c) => cursor = Some(c),
            None => return Ok(()),
        }
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FindBody {
    /// Conditions on the message; all given ones must hold. Empty: any message.
    #[serde(default, rename = "match")]
    pub matcher: MessageMatch,
    /// Only messages received at or after this time.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Matching messages to return, newest first, 1 to 500 (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct FindResult {
    /// Matching messages, newest first.
    pub messages: Vec<MessageSummary>,
    /// All matching messages, also those beyond `limit`.
    pub count: usize,
}

#[utoipa::path(
    post, path = "/__admin/capture/messages/find", tag = "capture", operation_id = "findCapturedMessages",
    summary = "Find captured messages",
    description = "The stored messages of `space` (or of the areas the caller may read) matching `match`: source, protocol, topic, exchange, key, headers, properties, content type (string matchers) and `body` (the body matchers of stubs; JSON matchers see Avro bodies decoded). Looks at up to 10000 stored messages and loads at most 64 MiB of bodies: only those of messages that meet the other conditions.",
    params(SpaceQuery),
    request_body(content = FindBody, example = json!({"match": {"topic": {"equalTo": "orders.events"}, "headers": {"event-type": {"equalTo": "OrderShipped"}}, "body": [{"jsonPath": "$.orderId", "value": {"equalTo": "A-1"}}]}, "limit": 10})),
    responses(
        (status = 200, description = "Matching messages", body = FindResult),
        (status = 400, description = "Invalid match or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, or an unknown space", body = ErrorBody),
        (status = 422, description = "More than 10000 stored messages or 64 MiB of bodies to check", body = ErrorBody),
        (status = 503, description = "Too many searches are reading stored messages", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn find_messages(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<FindBody>,
) -> ApiResult<Json<FindResult>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MessagesRead, q.space.as_deref())?;
    let limit = b.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(bad(format!("limit must be between 1 and {MAX_PAGE}")));
    }
    let matcher = Matcher::new(&b.matcher)?;
    let mut newest: VecDeque<MessageSummary> = VecDeque::new();
    let mut count = 0usize;
    {
        let _permit = scan_permit(scan_key(&p, &spaces)).await?;
        scan(
            &app,
            &svc,
            &spaces,
            b.since,
            &matcher,
            &|_| false,
            &mut |seen| {
                if seen.failures.is_empty() {
                    count += 1;
                    newest.push_back(MessageSummary::from(&seen.message));
                    if newest.len() > limit {
                        newest.pop_front();
                    }
                }
                true
            },
        )
        .await?;
    }
    let messages: Vec<MessageSummary> = newest.into_iter().rev().collect();
    Ok(Json(FindResult { messages, count }))
}

fn default_count() -> usize {
    1
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitBody {
    #[serde(default, rename = "match")]
    pub matcher: MessageMatch,
    /// Matching messages to wait for, 1 to 500 (default 1).
    #[serde(default = "default_count")]
    pub count: usize,
    /// 0 to 60000 ms (default 10000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Count matching messages already stored, whenever they arrived.
    #[serde(default)]
    pub include_existing: bool,
    /// Count stored messages received at or after this time too.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Without `since`: count stored messages received up to this many milliseconds before the request.
    #[serde(default)]
    pub lookback_ms: Option<u64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WaitResult {
    /// The first `count` matching messages, oldest first.
    pub messages: Vec<MessageSummary>,
    pub count: usize,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WaitTimeout {
    #[schema(example = "timed out after 10000 ms: 0 of 1 matching messages received")]
    pub error: String,
    pub details: Vec<String>,
    pub count: usize,
    pub expected: usize,
    pub messages: Vec<MessageSummary>,
}

#[utoipa::path(
    post, path = "/__admin/capture/messages/wait", tag = "capture", operation_id = "waitForCapturedMessages",
    summary = "Wait for captured messages",
    description = "Long-polls until `count` messages matching `match` were captured in `space` (or the areas the caller may read) and answers 200 with them, oldest first, or 408 with what was seen when `timeoutMs` (at most 60 s) runs out. Messages stored before the request count only with `includeExisting`, `since` or `lookbackMs`. Works on any replica; replicas that do not run the consumers re-read the store every 500 ms. Stops when the client disconnects; 503 while the server shuts down.",
    params(SpaceQuery),
    request_body(content = WaitBody, example = json!({"match": {"source": {"equalTo": "orders"}, "body": [{"jsonPath": "$.status", "value": {"equalTo": "SHIPPED"}}]}, "count": 1, "timeoutMs": 10000})),
    responses(
        (status = 200, description = "The matching messages arrived", body = WaitResult),
        (status = 400, description = "Invalid match, count or timeout", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, or an unknown space", body = ErrorBody),
        (status = 408, description = "Timed out", body = WaitTimeout),
        (status = 422, description = "More than 10000 stored messages or 64 MiB of bodies to check", body = ErrorBody),
        (status = 503, description = "Too many waits are reading stored messages, or the server is shutting down", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn wait(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<WaitBody>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MessagesRead, q.space.as_deref())?;
    if !(1..=MAX_WAIT_COUNT).contains(&b.count) {
        return Err(bad(format!("count must be between 1 and {MAX_WAIT_COUNT}")));
    }
    let timeout_ms = b.timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
    if timeout_ms > MAX_WAIT_MS {
        return Err(bad(format!(
            "timeoutMs must be between 0 and {MAX_WAIT_MS}"
        )));
    }
    let lookback = b.lookback_ms.unwrap_or(0);
    if lookback > 7 * 86_400_000 {
        return Err(bad("lookbackMs must be at most 7 days"));
    }
    let matcher = Matcher::new(&b.matcher)?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let started = Utc::now();
    let since = if b.include_existing {
        None
    } else {
        Some(
            b.since
                .unwrap_or(started - chrono::Duration::milliseconds(lookback as i64)),
        )
    };
    metrics::gauge!("fastmock_capture_waits_active").increment(1.0);
    struct Active;
    impl Drop for Active {
        fn drop(&mut self) {
            metrics::gauge!("fastmock_capture_waits_active").decrement(1.0);
        }
    }
    let _active = Active;
    let mut changes = svc.stored.subscribe();
    let mut found: Vec<MessageSummary> = Vec::new();
    let mut checked: HashSet<Uuid> = HashSet::new();
    let mut scan_from = since;
    let poll = svc.poll.unwrap_or(Duration::from_secs(5));
    loop {
        changes.borrow_and_update();
        let scanned_at = Utc::now();
        {
            let _permit = scan_permit(scan_key(&p, &spaces)).await?;
            let already = checked.clone();
            scan(
                &app,
                &svc,
                &spaces,
                scan_from,
                &matcher,
                &|id| already.contains(id),
                &mut |seen| {
                    checked.insert(seen.message.id);
                    if seen.failures.is_empty() {
                        found.push(MessageSummary::from(&seen.message));
                    }
                    found.len() < b.count
                },
            )
            .await?;
        }
        // Later scans read the last seconds again: a batch may commit after messages received later.
        let overlap = scanned_at - chrono::Duration::seconds(RESCAN_OVERLAP_SECS);
        scan_from = Some(since.map_or(overlap, |s| s.max(overlap)));
        if found.len() >= b.count {
            found.truncate(b.count);
            metrics::counter!("fastmock_capture_waits_total", "result" => "found").increment(1);
            return Ok(Json(WaitResult {
                count: found.len(),
                messages: found,
            })
            .into_response());
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                metrics::counter!("fastmock_capture_waits_total", "result" => "timeout").increment(1);
                return Ok((
                    StatusCode::REQUEST_TIMEOUT,
                    Json(WaitTimeout {
                        error: format!(
                            "timed out after {timeout_ms} ms: {} of {} matching messages received",
                            found.len(),
                            b.count
                        ),
                        details: vec![],
                        count: found.len(),
                        expected: b.count,
                        messages: found,
                    }),
                )
                    .into_response());
            }
            _ = changes.changed() => {}
            _ = tokio::time::sleep(poll) => {}
            _ = crate::mail_api::shutting_down(app.ready.clone()) => {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the server is shutting down; retry on another replica".into(),
                ));
            }
        }
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifyBody {
    #[serde(default, rename = "match")]
    pub matcher: MessageMatch,
    /// Exactly this many matching messages.
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub at_least: Option<u64>,
    #[serde(default)]
    pub at_most: Option<u64>,
    /// Only messages received at or after this time.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
}

/// `(lowest, highest, text)` of the expectation; without any of the three, at least 1.
pub fn expectation(b: &VerifyBody) -> Result<(u64, u64, String), String> {
    match (b.count, b.at_least, b.at_most) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            Err("use either count or atLeast/atMost".into())
        }
        (Some(n), None, None) => Ok((n, n, format!("exactly {n}"))),
        (None, lo, hi) => {
            let lo_v = lo.unwrap_or(if hi.is_some() { 0 } else { 1 });
            let hi_v = hi.unwrap_or(u64::MAX);
            if lo_v > hi_v {
                return Err("atLeast must not exceed atMost".into());
            }
            let text = match (lo, hi) {
                (Some(l), Some(h)) => format!("between {l} and {h}"),
                (None, Some(h)) => format!("at most {h}"),
                _ => format!("at least {lo_v}"),
            };
            Ok((lo_v, hi_v, text))
        }
    }
}

/// A stored message that fails some conditions.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageNearMiss {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub source: String,
    pub topic: String,
    pub received_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Share of the conditions that held, 0..1.
    pub score: f64,
    /// The failed conditions: `topic`, `key`, `headers.event-type`, `properties.correlationId`, `body[0]`, ...
    #[schema(example = json!(["body[0]"]))]
    pub mismatches: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub verified: bool,
    pub count: u64,
    pub expected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub details: Vec<String>,
    /// The closest non-matching messages, best first (only on failure, at most 5).
    pub near_misses: Vec<MessageNearMiss>,
}

#[utoipa::path(
    post, path = "/__admin/capture/verify", tag = "capture", operation_id = "verifyCapturedMessages",
    summary = "Verify how many messages were captured",
    description = "Counts the stored messages of `space` (or of the areas the caller may read) matching `match`. Give `count` for an exact number, or `atLeast` and/or `atMost` (default: at least 1). 200 when it holds; 422 with the count and the closest non-matching messages and their failed conditions when it does not. Body conditions are checked only on messages that meet the other conditions; a near miss that fails those lists only them.",
    params(SpaceQuery),
    request_body(content = VerifyBody, example = json!({"match": {"topic": {"equalTo": "orders.events"}}, "count": 1})),
    responses(
        (status = 200, description = "Expectation met", body = VerifyResult),
        (status = 400, description = "Invalid match or expectation", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Capture is not enabled, or an unknown space", body = ErrorBody),
        (status = 422, description = "Expectation not met (`VerifyResult`), or more than 10000 stored messages or 64 MiB of bodies to check (`ErrorBody`)", body = VerifyResult),
        (status = 503, description = "Too many verifications are reading stored messages", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn verify(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<VerifyBody>,
) -> ApiResult<Response> {
    let (lo, hi, expected) = expectation(&b).map_err(bad)?;
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MessagesRead, q.space.as_deref())?;
    let matcher = Matcher::new(&b.matcher)?;
    let total = b.matcher.count().max(1) as f64;
    let body_conditions = b.matcher.body.len();
    let mut count = 0u64;
    let mut misses: Vec<MessageNearMiss> = Vec::new();
    {
        let _permit = scan_permit(scan_key(&p, &spaces)).await?;
        scan(
            &app,
            &svc,
            &spaces,
            b.since,
            &matcher,
            &|_| false,
            &mut |seen| {
                if seen.failures.is_empty() {
                    count += 1;
                    return true;
                }
                let m = &seen.message;
                let unchecked = if seen.body_checked {
                    0
                } else {
                    body_conditions
                };
                misses.push(MessageNearMiss {
                    id: m.id,
                    space: m.space.clone(),
                    source: m.source.clone(),
                    topic: m.topic.clone(),
                    received_at: m.received_at,
                    preview: m.preview.clone(),
                    score: (total - (seen.failures.len() + unchecked) as f64).max(0.0) / total,
                    mismatches: seen.failures,
                });
                if misses.len() > 4 * MAX_NEAR_MISSES {
                    sort_misses(&mut misses);
                    misses.truncate(MAX_NEAR_MISSES);
                }
                true
            },
        )
        .await?;
    }
    let verified = (lo..=hi).contains(&count);
    metrics::counter!("fastmock_admin_verifications_total", "kind" => "capture", "result" => if verified { "pass" } else { "fail" }).increment(1);
    if verified {
        return Ok(Json(VerifyResult {
            verified,
            count,
            expected,
            error: None,
            details: vec![],
            near_misses: vec![],
        })
        .into_response());
    }
    sort_misses(&mut misses);
    misses.truncate(MAX_NEAR_MISSES);
    let details = misses
        .iter()
        .map(|m| {
            format!(
                "message {} ({} {}): {}",
                m.id,
                m.source,
                m.topic,
                m.mismatches.join(", ")
            )
        })
        .collect();
    Ok((
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(VerifyResult {
            verified,
            count,
            error: Some(format!(
                "expected {expected} matching messages, received {count}"
            )),
            expected,
            details,
            near_misses: misses,
        }),
    )
        .into_response())
}

fn sort_misses(misses: &mut [MessageNearMiss]) {
    misses.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.received_at.cmp(&a.received_at))
            .then(b.id.cmp(&a.id))
    });
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        list_sources,
        get_source,
        put_source,
        delete_source,
        list_messages,
        get_message,
        body,
        delete_message,
        delete_messages,
        find_messages,
        wait,
        verify,
    ),
    components(schemas(
        CaptureSource,
        fastmock_core::capture::StartFrom,
        fastmock_core::MessageProtocol,
        MessageMatch,
        SourceStatus,
    )),
    tags((name = "capture", description = "Message capture: sources that subscribe to Kafka topics, MQTT topic filters and AMQP exchanges; list, find, wait for and verify the captured messages"))
)]
pub struct CaptureApi;

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> VerifyBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn expectations() {
        assert_eq!(
            expectation(&body(json!({}))).unwrap(),
            (1, u64::MAX, "at least 1".into())
        );
        assert_eq!(
            expectation(&body(json!({"count": 0}))).unwrap().2,
            "exactly 0"
        );
        assert_eq!(
            expectation(&body(json!({"atMost": 2}))).unwrap(),
            (0, 2, "at most 2".into())
        );
        assert!(expectation(&body(json!({"count": 1, "atMost": 2}))).is_err());
        assert!(expectation(&body(json!({"atLeast": 3, "atMost": 2}))).is_err());
        assert!(serde_json::from_value::<VerifyBody>(json!({"exactly": 1})).is_err());
    }

    #[tokio::test]
    async fn one_space_gets_a_share_of_the_search_slots() {
        let l = ScanLimiter::new(4, 2);
        let wait = Duration::from_millis(50);
        let a1 = l.acquire("space:a".into(), wait).await.unwrap();
        let _a2 = l.acquire("space:a".into(), wait).await.unwrap();
        let e = l.acquire("space:a".into(), wait).await.err().unwrap();
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        let _b1 = l.acquire("space:b".into(), wait).await.unwrap();
        let _c1 = l.acquire("space:c".into(), wait).await.unwrap();
        assert!(
            l.acquire("space:d".into(), wait).await.is_err(),
            "the total is a bound too"
        );
        drop(a1);
        assert!(l.acquire("space:d".into(), wait).await.is_ok());
    }

    #[test]
    fn orders_and_protocols() {
        assert!(!parse_order(None).unwrap());
        assert!(parse_order(Some("oldest")).unwrap());
        assert!(parse_order(Some("x")).is_err());
        assert_eq!(
            parse_protocol(Some("MQTT")).unwrap(),
            Some(MessageProtocol::Mqtt)
        );
        assert!(parse_protocol(Some("nats")).is_err());
    }
}
