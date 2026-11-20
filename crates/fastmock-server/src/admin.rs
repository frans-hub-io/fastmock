use std::time::Duration;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Router, middleware};
use fastmock_core::{CallbackFilter, CapturedRequest, JournalFilter, SpaceFilter, Stub};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::access::Permission;
use crate::auth::{Principal, admin_auth};
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

#[derive(Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    #[schema(example = "stub not found")]
    pub error: String,
    pub details: Vec<String>,
    /// On 403: the built-in role (for principals holding only global built-in roles) or the permission the request needs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<String>,
}

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

fn required_of(status: StatusCode, msg: &str) -> Option<String> {
    if status != StatusCode::FORBIDDEN {
        return None;
    }
    msg.strip_prefix("requires role ")
        .or_else(|| msg.strip_prefix("requires permission "))
        .and_then(|r| r.split_whitespace().next())
        .map(String::from)
}

/// A `403` answered by a handler. Its body is `{"error": <message>, "details": []}` as in earlier
/// versions; with an access file or OIDC, [`admin_auth`] rewrites it to [`forbidden`].
#[derive(Clone, Debug)]
pub struct Denial {
    pub message: String,
    pub required: Option<String>,
}

/// The `403` body used whenever an access file or OIDC is configured.
pub fn forbidden(message: String, required: Option<String>) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody {
            error: "forbidden".into(),
            details: vec![message],
            required,
        }),
    )
        .into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let denial = (self.0 == StatusCode::FORBIDDEN).then(|| Denial {
            required: required_of(self.0, &self.1),
            message: self.1.clone(),
        });
        let mut r = (
            self.0,
            Json(ErrorBody {
                error: self.1,
                details: vec![],
                required: None,
            }),
        )
            .into_response();
        if let Some(d) = denial {
            r.extensions_mut().insert(d);
        }
        r
    }
}

#[derive(Debug)]
pub struct DetailedError(pub StatusCode, pub String, pub Vec<String>);

impl IntoResponse for DetailedError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(ErrorBody {
                error: self.1,
                details: self.2,
                required: None,
            }),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "admin operation failed");
        metrics::counter!("fastmock_admin_errors_total").increment(1);
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

pub fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

pub fn bad(msg: impl Into<String>) -> ApiError {
    let mut m: String = msg.into();
    if m.len() > 2048 {
        let mut cut = 2048;
        while !m.is_char_boundary(cut) {
            cut -= 1;
        }
        m.truncate(cut);
        m.push('…');
    }
    ApiError(StatusCode::BAD_REQUEST, m)
}

/// Handler-level permission check (`allowed(permission, space)`); answers 403 when it fails.
pub fn require(p: &Principal, perm: Permission, space: Option<&str>) -> ApiResult<()> {
    if p.allowed(perm, space) {
        return Ok(());
    }
    p.count_denial();
    tracing::warn!(target: "audit", principal = %p.name, kind = p.kind.as_str(), permission = %perm, space, "admin request rejected: permission missing");
    Err(ApiError(StatusCode::FORBIDDEN, p.denial(perm, space).0))
}

/// Handler check of the admin-only `/audit` and `/reset`. Without an access file and OIDC the
/// middleware lets viewers (`/audit`) and editors (`/reset`) through, as earlier versions did, and
/// this check answers with the earlier denial body.
fn admin_route_denial(p: &Principal, perm: Permission) -> Option<Response> {
    require(p, perm, None)
        .err()
        .map(IntoResponse::into_response)
}

pub async fn audit_event(app: &AppState, p: &Principal, action: &str, detail: Value) {
    audit_event_in(app, p, None, action, detail).await
}

pub async fn audit_event_in(
    app: &AppState,
    p: &Principal,
    space: Option<&str>,
    action: &str,
    detail: Value,
) {
    metrics::counter!("fastmock_admin_changes_total", "action" => action.to_string()).increment(1);
    tracing::info!(target: "audit", principal = %p.name, kind = p.kind.as_str(), role = %p.role_label, space, action, detail = %detail, "admin change");
    let e = fastmock_core::AuditEvent {
        id: 0,
        at: chrono::Utc::now(),
        principal: p.name.clone(),
        role: p.role_label.clone(),
        principal_kind: Some(p.kind.as_str().to_string()),
        space: space.map(String::from),
        action: action.to_string(),
        detail,
    };
    if let Err(err) = app.stores.audit.record(&e).await {
        metrics::counter!("fastmock_store_errors_total", "store" => "audit").increment(1);
        tracing::error!(error = %err, "audit write failed");
    }
}

pub fn parse_stub(v: Value) -> ApiResult<Stub> {
    serde_json::from_value(v).map_err(|e| bad(format!("invalid stub: {e}")))
}

pub async fn refresh(app: &AppState) -> ApiResult<()> {
    app.engine.reload().await?;
    Ok(())
}

fn masked_matcher() -> fastmock_core::model::StringMatcher {
    fastmock_core::model::StringMatcher {
        equal_to: Some(crate::redact::MASK.to_string()),
        ..Default::default()
    }
}

fn mentions_sensitive_key(r: &crate::redact::Redactor, expr: &str) -> bool {
    expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .any(|t| !t.is_empty() && r.sensitive_key(t))
}

fn mask_quoted_literals(expr: &str) -> String {
    static QUOTED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"'[^']*'|"[^"]*""#).unwrap());
    QUOTED
        .replace_all(expr, |c: &regex::Captures| {
            let q = &c[0][..1];
            format!("{q}{}{q}", crate::redact::MASK)
        })
        .into_owned()
}

fn mask_text_opt(r: &crate::redact::Redactor, v: &mut Option<String>) {
    if let Some(t) = v {
        *t = r.mask_text(t);
    }
}

fn mask_json_opt(r: &crate::redact::Redactor, v: &mut Option<Value>) {
    if let Some(j) = v {
        r.json(j);
    }
}

fn mask_named_matchers(
    r: &crate::redact::Redactor,
    m: &mut std::collections::BTreeMap<String, fastmock_core::model::StringMatcher>,
    sensitive: impl Fn(&str) -> bool,
) {
    for (k, v) in m.iter_mut() {
        if sensitive(k) {
            *v = masked_matcher();
        } else {
            mask_string_matcher(r, v);
        }
    }
}

fn mask_string_matcher(r: &crate::redact::Redactor, m: &mut fastmock_core::model::StringMatcher) {
    for t in [&mut m.equal_to, &mut m.contains] {
        mask_text_opt(r, t);
    }
    for x in m.and.iter_mut().chain(m.or.iter_mut()) {
        mask_string_matcher(r, x);
    }
    if let Some(n) = &mut m.not {
        mask_string_matcher(r, n);
    }
    for list in [&mut m.has_exactly, &mut m.includes].into_iter().flatten() {
        for x in list.iter_mut() {
            mask_string_matcher(r, x);
        }
    }
}

fn mask_body_matchers(r: &crate::redact::Redactor, body: &mut [fastmock_core::BodyMatcher]) {
    for b in body {
        mask_text_opt(r, &mut b.equal_to);
        mask_text_opt(r, &mut b.contains);
        mask_text_opt(r, &mut b.matches);
        mask_text_opt(r, &mut b.equal_to_xml);
        mask_json_opt(r, &mut b.equal_to_json);
        mask_json_opt(r, &mut b.json_schema);
        for path in [&mut b.json_path, &mut b.xpath].into_iter().flatten() {
            if mentions_sensitive_key(r, path) {
                *path = mask_quoted_literals(path);
                if b.value.is_some() {
                    b.value = Some(masked_matcher());
                }
            }
        }
        if let Some(v) = &mut b.value {
            mask_string_matcher(r, v);
        }
    }
}

fn mask_url(r: &crate::redact::Redactor, url: &mut String) {
    if let Some((base, query)) = url.split_once('?') {
        let (query, fragment) = match query.split_once('#') {
            Some((q, f)) => (q, Some(f)),
            None => (query, None),
        };
        let mut out = format!("{base}?{}", r.query(query));
        if let Some(f) = fragment {
            out.push('#');
            out.push_str(f);
        }
        *url = out;
    }
}

fn mask_response(r: &crate::redact::Redactor, resp: &mut fastmock_core::ResponseDef) {
    for (k, v) in resp.headers.iter_mut() {
        if r.sensitive_header(k) {
            *v = crate::redact::MASK.to_string();
        } else {
            *v = r.mask_text(v);
        }
    }
    mask_text_opt(r, &mut resp.body);
    mask_json_opt(r, &mut resp.json_body);
    if let Some(px) = &mut resp.proxy {
        mask_header_values(r, &mut px.additional_headers);
    }
    if let Some(events) = &mut resp.sse {
        for e in events {
            e.data = r.mask_text(&e.data);
        }
    }
    if let Some(ws) = &mut resp.websocket {
        for m in &mut ws.messages {
            m.data = r.mask_text(&m.data);
        }
        for reply in &mut ws.on_message {
            mask_text_opt(r, &mut reply.reply);
        }
    }
}

fn mask_header_values(
    r: &crate::redact::Redactor,
    m: &mut std::collections::BTreeMap<String, String>,
) {
    for (k, v) in m.iter_mut() {
        if r.sensitive_header(k) {
            *v = crate::redact::MASK.to_string();
        }
    }
}

/// Masks credentials in a stub for principals that may only read: the same rules as the request
/// journal (sensitive header names, sensitive query, form and JSON key names, all cookies, basic
/// auth passwords, secrets inside bodies) applied to matchers, responses, proxies, callbacks and
/// messages.
pub fn mask_stub_secrets(r: &crate::redact::Redactor, s: &mut Stub) {
    let req = &mut s.request;
    if let Some(b) = &mut req.basic_auth {
        b.password = crate::redact::MASK.to_string();
    }
    mask_named_matchers(r, &mut req.headers, |k| r.sensitive_header(k));
    mask_named_matchers(r, &mut req.query, |k| r.sensitive_key(k));
    mask_named_matchers(r, &mut req.form_parameters, |k| r.sensitive_key(k));
    mask_named_matchers(r, &mut req.cookies, |_| true);
    if let Some(u) = &mut req.url {
        mask_url(r, u);
    }
    mask_body_matchers(r, &mut req.body);
    for part in &mut req.multipart {
        let secret_part = part.name.as_deref().is_some_and(|n| r.sensitive_key(n));
        mask_named_matchers(r, &mut part.headers, |k| r.sensitive_header(k));
        if secret_part {
            part.body = vec![fastmock_core::BodyMatcher {
                equal_to: Some(crate::redact::MASK.to_string()),
                ..Default::default()
            }];
        } else {
            mask_body_matchers(r, &mut part.body);
        }
    }
    mask_response(r, &mut s.response);
    for resp in &mut s.responses {
        mask_response(r, resp);
    }
    for c in &mut s.callbacks {
        mask_url(r, &mut c.url);
        mask_header_values(r, &mut c.headers);
        mask_text_opt(r, &mut c.body);
        mask_json_opt(r, &mut c.json_body);
    }
    for m in &mut s.messages {
        mask_header_values(r, &mut m.headers);
        mask_text_opt(r, &mut m.value);
        mask_json_opt(r, &mut m.json_value);
    }
    mask_json_opt(r, &mut s.metadata);
}

/// Masks credentials unless the principal may change the stub (`stubs:write` in its space).
pub fn mask_for(app: &AppState, p: &Principal, stubs: &mut [Stub]) {
    for s in stubs {
        if !p.allowed(Permission::StubsWrite, s.space.as_deref()) {
            mask_stub_secrets(&app.redactor, s);
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StubList {
    pub total: usize,
    pub offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    pub stubs: Vec<Stub>,
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct StubsQuery {
    tag: Option<String>,
    /// Only stubs of this space.
    space: Option<String>,
    q: Option<String>,
    enabled: Option<bool>,
    #[param(minimum = 1, maximum = 10000)]
    limit: Option<usize>,
    #[param(minimum = 0)]
    offset: Option<usize>,
}

fn stub_search_hit(s: &Stub, needle: &str) -> bool {
    let n = needle.to_lowercase();
    let r = &s.request;
    [
        s.name.as_deref(),
        r.url_path.as_deref(),
        r.url_path_pattern.as_deref(),
        r.url_path_template.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|f| f.to_lowercase().contains(&n))
}

fn paginate<T>(items: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> ApiResult<Vec<T>> {
    if limit.is_some_and(|l| l == 0 || l > 10_000) {
        return Err(bad("limit must be between 1 and 10000"));
    }
    let offset = offset.unwrap_or(0);
    Ok(items
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect())
}

#[utoipa::path(
    get, path = "/__admin/stubs", tag = "stubs", operation_id = "listStubs",
    summary = "List stubs",
    description = "Newest first. Filters combine with AND; `q` searches the name and the URL path fields case-insensitively. Viewers see credentials masked: sensitive headers, query, form and JSON keys, cookies, basic auth passwords and secrets inside bodies, callbacks, proxies and messages.",
    params(StubsQuery),
    responses(
        (status = 200, description = "Stubs", body = StubList),
        (status = 400, description = "Invalid parameters", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_stubs(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<StubsQuery>,
) -> ApiResult<Json<StubList>> {
    let sel = spaces::select(&app, &p, Permission::StubsRead, q.space.as_deref())?;
    let mut stubs = app.stores.stubs.list().await?;
    stubs.retain(|s| {
        sel.matches(s.space.as_deref())
            && q.tag.as_ref().is_none_or(|t| s.tags.contains(t))
            && q.enabled.is_none_or(|e| s.enabled == e)
            && q.q.as_deref().is_none_or(|n| stub_search_hit(s, n))
    });
    let total = stubs.len();
    let mut stubs = paginate(stubs, q.offset, q.limit)?;
    mask_for(&app, &p, &mut stubs);
    Ok(Json(StubList {
        total,
        offset: q.offset.unwrap_or(0),
        limit: q.limit,
        stubs,
    }))
}

#[utoipa::path(
    get, path = "/__admin/stubs/{id}", tag = "stubs", operation_id = "getStub",
    summary = "Get one stub",
    params(("id" = Uuid, Path, description = "Stub id")),
    responses(
        (status = 200, description = "The stub", body = Stub),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "No such stub", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_stub(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Stub>> {
    let stub = app
        .stores
        .stubs
        .get(id)
        .await?
        .ok_or_else(|| not_found("stub"))?;
    spaces::item(
        &p,
        Permission::StubsRead,
        Permission::StubsRead,
        stub.space.as_deref(),
        "stub",
    )?;
    let mut one = [stub];
    mask_for(&app, &p, &mut one);
    let [stub] = one;
    Ok(Json(stub))
}

#[utoipa::path(
    post, path = "/__admin/stubs", tag = "stubs", operation_id = "createStub",
    summary = "Create a stub",
    description = "The stub goes to the space in its `space` field or the `space` parameter; without either, to the global area (or the caller's only space). Stubs in a space with ownership rules must stay inside them.",
    params(SpaceQuery),
    request_body(content = Stub, example = json!({"name": "get user", "request": {"method": "GET", "urlPathTemplate": "/users/{id}"}, "response": {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}"}}})),
    responses(
        (status = 201, description = "Created", body = Stub),
        (status = 400, description = "Invalid stub", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 409, description = "A stub with this id exists", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn create_stub(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(v): Json<Value>,
) -> ApiResult<Response> {
    let mut stub = parse_stub(v)?;
    stub.updated_at = None;
    let requested = requested_space(stub.space.as_deref(), q.space.as_deref())?;
    stub.space = spaces::target(&app, &p, Permission::StubsWrite, requested.as_deref())?;
    spaces::check_stub(&app, &stub).map_err(bad)?;
    if !app.stores.stubs.insert(&stub).await? {
        let existing = app.stores.stubs.get(stub.id).await?;
        let msg = match existing {
            Some(e)
                if e.space == stub.space
                    && p.allowed(Permission::StubsRead, e.space.as_deref()) =>
            {
                "stub with this id exists; use PUT"
            }
            _ => "id is already in use",
        };
        return Err(ApiError(StatusCode::CONFLICT, msg.into()));
    }
    refresh(&app).await?;
    audit_event_in(
        &app,
        &p,
        stub.space.as_deref(),
        "stub.create",
        json!({"id": stub.id, "name": stub.name}),
    )
    .await;
    let saved = app.stores.stubs.get(stub.id).await?;
    Ok((StatusCode::CREATED, Json(saved)).into_response())
}

const SPACE_FIXED: &str =
    "the space of a stub cannot change; delete it and create it in the other space";

fn requested_space(field: Option<&str>, param: Option<&str>) -> ApiResult<Option<String>> {
    match (field, param) {
        (Some(a), Some(b)) if a != b => Err(bad(
            "the space field of the stub and the space parameter differ",
        )),
        (a, b) => Ok(a.or(b).map(String::from)),
    }
}

const AUDIT_MAX_CHANGES: usize = 100;
const AUDIT_MAX_VALUE: usize = 16 * 1024;

pub fn audit_view(app: &AppState, stub: &Stub) -> Value {
    let mut s = stub.clone();
    s.updated_at = None;
    mask_stub_secrets(&app.redactor, &mut s);
    let mut v = serde_json::to_value(&s).unwrap_or(Value::Null);
    app.redactor.json(&mut v);
    v
}

fn capped(v: &Value) -> Value {
    let text = v.to_string();
    if text.len() > AUDIT_MAX_VALUE {
        json!({"truncated": true, "bytes": text.len()})
    } else {
        v.clone()
    }
}

pub fn json_diff(path: &str, before: &Value, after: &Value, out: &mut Vec<Value>) {
    if out.len() > AUDIT_MAX_CHANGES || before == after {
        return;
    }
    match (before, after) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                let p = format!("{path}/{}", k.replace('~', "~0").replace('/', "~1"));
                json_diff(
                    &p,
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ => out.push(json!({"path": if path.is_empty() { "/" } else { path }, "before": capped(before), "after": capped(after)})),
    }
}

pub fn stub_change_detail(app: &AppState, id: Uuid, before: Option<&Stub>, after: &Stub) -> Value {
    let after_v = audit_view(app, after);
    match before {
        None => json!({"id": id, "name": after.name, "created": true, "after": capped(&after_v)}),
        Some(b) => {
            let before_v = audit_view(app, b);
            let mut changes = Vec::new();
            json_diff("", &before_v, &after_v, &mut changes);
            let truncated = changes.len() > AUDIT_MAX_CHANGES;
            changes.truncate(AUDIT_MAX_CHANGES);
            json!({"id": id, "name": after.name, "changes": changes, "changesTruncated": truncated})
        }
    }
}

async fn save_stub_update(
    app: &AppState,
    p: &Principal,
    id: Uuid,
    before: Option<Stub>,
    stub: Stub,
) -> ApiResult<Json<Option<Stub>>> {
    spaces::check_stub(app, &stub).map_err(bad)?;
    app.stores.stubs.upsert(&stub).await?;
    refresh(app).await?;
    audit_event_in(
        app,
        p,
        stub.space.as_deref(),
        "stub.update",
        stub_change_detail(app, id, before.as_ref(), &stub),
    )
    .await;
    Ok(Json(app.stores.stubs.get(id).await?))
}

#[utoipa::path(
    put, path = "/__admin/stubs/{id}", tag = "stubs", operation_id = "putStub",
    summary = "Create or replace a stub",
    description = "The id in the path wins over an id in the body. The audit log records the changed fields. An existing stub keeps its space; a new one goes to the space given like for `POST /stubs`.",
    params(("id" = Uuid, Path, description = "Stub id"), SpaceQuery),
    request_body = Stub,
    responses(
        (status = 200, description = "Saved stub", body = Stub),
        (status = 400, description = "Invalid stub", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn put_stub(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
    Json(v): Json<Value>,
) -> ApiResult<Json<Option<Stub>>> {
    let mut stub = parse_stub(v)?;
    stub.id = id;
    stub.updated_at = None;
    let requested = requested_space(stub.space.as_deref(), q.space.as_deref())?;
    let before = app.stores.stubs.get(id).await?;
    match &before {
        Some(b) => {
            spaces::item(
                &p,
                Permission::StubsRead,
                Permission::StubsWrite,
                b.space.as_deref(),
                "stub",
            )?;
            if requested.is_some() && requested != b.space {
                return Err(bad(SPACE_FIXED));
            }
            stub.space = b.space.clone();
        }
        None => {
            stub.space = spaces::target(&app, &p, Permission::StubsWrite, requested.as_deref())?;
        }
    }
    save_stub_update(&app, &p, id, before, stub).await
}

pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(serde_json::Map::new());
            }
            if let Value::Object(t) = target {
                for (k, v) in p {
                    if v.is_null() {
                        t.remove(k);
                    } else {
                        merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                    }
                }
            }
        }
        other => *target = other.clone(),
    }
}

#[utoipa::path(
    patch, path = "/__admin/stubs/{id}", tag = "stubs", operation_id = "patchStub",
    summary = "Partially update a stub (JSON merge patch)",
    description = "RFC 7396 JSON merge patch applied to the stored stub: objects merge, `null` removes a field, arrays and scalars replace. The id cannot change.",
    params(("id" = Uuid, Path, description = "Stub id")),
    request_body(content = Object, content_type = "application/merge-patch+json", example = json!({"enabled": false, "response": {"status": 503}})),
    responses(
        (status = 200, description = "Patched stub", body = Stub),
        (status = 400, description = "Patch produces an invalid stub", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "No such stub", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn patch_stub(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(patch): Json<Value>,
) -> ApiResult<Json<Option<Stub>>> {
    if !patch.is_object() {
        return Err(bad("merge patch must be a JSON object"));
    }
    let before = app
        .stores
        .stubs
        .get(id)
        .await?
        .ok_or_else(|| not_found("stub"))?;
    spaces::item(
        &p,
        Permission::StubsRead,
        Permission::StubsWrite,
        before.space.as_deref(),
        "stub",
    )?;
    let mut base = before.clone();
    base.updated_at = None;
    let mut v = serde_json::to_value(&base).map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    merge_patch(&mut v, &patch);
    let mut stub = parse_stub(v)?;
    stub.id = id;
    stub.updated_at = None;
    if stub.space != before.space {
        return Err(bad(SPACE_FIXED));
    }
    save_stub_update(&app, &p, id, Some(before), stub).await
}

#[utoipa::path(
    delete, path = "/__admin/stubs/{id}", tag = "stubs", operation_id = "deleteStub",
    summary = "Delete a stub",
    params(("id" = Uuid, Path, description = "Stub id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "No such stub", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_stub(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let before = app.stores.stubs.get(id).await?;
    if let Some(b) = &before {
        spaces::item(
            &p,
            Permission::StubsRead,
            Permission::StubsWrite,
            b.space.as_deref(),
            "stub",
        )?;
    }
    if !app.stores.stubs.delete(id).await? {
        return Err(not_found("stub"));
    }
    refresh(&app).await?;
    let detail = match &before {
        Some(b) => json!({"id": id, "name": b.name, "before": capped(&audit_view(&app, b))}),
        None => json!({"id": id}),
    };
    let space = before.as_ref().and_then(|b| b.space.as_deref());
    audit_event_in(&app, &p, space, "stub.delete", detail).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TagQuery {
    tag: Option<String>,
    /// Only stubs of this space.
    space: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct DeletedCount {
    pub deleted: u64,
}

#[utoipa::path(
    delete, path = "/__admin/stubs", tag = "stubs", operation_id = "deleteStubs",
    summary = "Delete stubs by tag, or all stubs",
    description = "Without `tag` every stub (of `space`, when given) is deleted, which needs `stubs:purge` (admin role). With `tag`, the tagged stubs of every space the caller may change, or of `space`, are deleted.",
    params(TagQuery),
    responses(
        (status = 200, description = "Number of deleted stubs", body = DeletedCount),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role, or admin without a tag", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_stubs(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<TagQuery>,
) -> ApiResult<Json<DeletedCount>> {
    let n = match &q.tag {
        Some(t) => {
            let sel = spaces::select(&app, &p, Permission::StubsWrite, q.space.as_deref())?;
            app.stores.stubs.delete_by_tag(t, &sel).await?
        }
        None => {
            require(&p, Permission::StubsPurge, None)?;
            let sel = match &q.space {
                Some(s) => {
                    spaces::known(&app, &p, s)?;
                    SpaceFilter::In(vec![s.clone()])
                }
                None => SpaceFilter::Any,
            };
            app.stores.stubs.clear(&sel).await?
        }
    };
    refresh(&app).await?;
    audit_event_in(
        &app,
        &p,
        q.space.as_deref(),
        "stub.delete_many",
        json!({"tag": q.tag, "deleted": n}),
    )
    .await;
    Ok(Json(DeletedCount { deleted: n }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetadataPattern {
    #[serde(default)]
    #[schema(value_type = Option<Object>, example = "$.service")]
    matches_json_path: Option<Value>,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    equal_to: Option<Value>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    matches: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    equal_to_json: Option<Value>,
    #[serde(default)]
    ignore_extra_elements: bool,
}

pub struct CompiledMetadata {
    path: Option<serde_json_path::JsonPath>,
    equal_to: Option<Value>,
    contains: Option<String>,
    matches: Option<regex::Regex>,
    equal_to_json: Option<Value>,
    ignore_extra: bool,
}

impl MetadataPattern {
    pub fn compile(self) -> ApiResult<CompiledMetadata> {
        let mut equal_to = self.equal_to;
        let mut contains = self.contains;
        let mut matches = self.matches;
        let expr = match self.matches_json_path {
            None => None,
            Some(Value::String(s)) => Some(s),
            Some(Value::Object(mut o)) => {
                let e = match o.remove("expression") {
                    Some(Value::String(s)) => s,
                    _ => return Err(bad("matchesJsonPath.expression must be a string")),
                };
                equal_to = equal_to.or(o.remove("equalTo"));
                if let Some(Value::String(c)) = o.remove("contains") {
                    contains = contains.or(Some(c));
                }
                if let Some(Value::String(m)) = o.remove("matches") {
                    matches = matches.or(Some(m));
                }
                Some(e)
            }
            Some(_) => return Err(bad("matchesJsonPath must be a string or an object")),
        };
        if expr.is_none() && self.equal_to_json.is_none() {
            return Err(bad("set matchesJsonPath or equalToJson"));
        }
        let path = expr
            .map(|e| {
                if e.len() > 1024 {
                    return Err(bad("matchesJsonPath is limited to 1024 bytes"));
                }
                serde_json_path::JsonPath::parse(&e)
                    .map_err(|err| bad(format!("invalid JSONPath `{e}`: {err}")))
            })
            .transpose()?;
        let matches = matches
            .map(|m| {
                regex::RegexBuilder::new(&format!("^(?:{m})$"))
                    .size_limit(256 * 1024)
                    .build()
                    .map_err(|e| bad(format!("invalid regex: {e}")))
            })
            .transpose()?;
        Ok(CompiledMetadata {
            path,
            equal_to,
            contains,
            matches,
            equal_to_json: self.equal_to_json,
            ignore_extra: self.ignore_extra_elements,
        })
    }
}

impl CompiledMetadata {
    pub fn matches(&self, metadata: Option<&Value>) -> bool {
        let Some(md) = metadata else {
            return false;
        };
        if let Some(expected) = &self.equal_to_json {
            let ok = if self.ignore_extra {
                fastmock_core::matcher::json_subset(expected, md)
            } else {
                expected == md
            };
            if !ok {
                return false;
            }
        }
        let Some(path) = &self.path else {
            return true;
        };
        let nodes = path.query(md).all();
        if nodes.is_empty() {
            return false;
        }
        if self.equal_to.is_none() && self.contains.is_none() && self.matches.is_none() {
            return true;
        }
        nodes.iter().any(|n| {
            let text = fastmock_core::matcher::value_text(n);
            self.equal_to.as_ref().is_none_or(|e| match e {
                Value::String(s) => *s == text,
                other => *other == **n,
            }) && self
                .contains
                .as_ref()
                .is_none_or(|c| text.contains(c.as_str()))
                && self.matches.as_ref().is_none_or(|re| re.is_match(&text))
        })
    }
}

#[utoipa::path(
    post, path = "/__admin/stubs/find-by-metadata", tag = "stubs", operation_id = "findStubsByMetadata",
    summary = "Find stubs by metadata (WireMock compatible)",
    params(SpaceQuery),
    request_body(content = MetadataPattern, example = json!({"matchesJsonPath": "$.service", "equalTo": "billing"})),
    responses(
        (status = 200, description = "Matching stubs", body = StubList),
        (status = 400, description = "Invalid pattern", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn find_by_metadata(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(pattern): Json<MetadataPattern>,
) -> ApiResult<Json<StubList>> {
    let sel = spaces::select(&app, &p, Permission::StubsRead, q.space.as_deref())?;
    let m = pattern.compile()?;
    let mut stubs = app.stores.stubs.list().await?;
    stubs.retain(|s| sel.matches(s.space.as_deref()) && m.matches(s.metadata.as_ref()));
    mask_for(&app, &p, &mut stubs);
    Ok(Json(StubList {
        total: stubs.len(),
        offset: 0,
        limit: None,
        stubs,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct DeletedIds {
    pub deleted: u64,
    pub ids: Vec<Uuid>,
}

#[utoipa::path(
    post, path = "/__admin/stubs/remove-by-metadata", tag = "stubs", operation_id = "removeStubsByMetadata",
    summary = "Delete stubs by metadata (WireMock compatible)",
    params(SpaceQuery),
    request_body(content = MetadataPattern, example = json!({"matchesJsonPath": "$.service", "equalTo": "billing"})),
    responses(
        (status = 200, description = "Deleted stubs", body = DeletedIds),
        (status = 400, description = "Invalid pattern", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn remove_by_metadata(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(pattern): Json<MetadataPattern>,
) -> ApiResult<Json<DeletedIds>> {
    let sel = spaces::select(&app, &p, Permission::StubsWrite, q.space.as_deref())?;
    let m = pattern.compile()?;
    let stubs = app.stores.stubs.list().await?;
    let mut ids = Vec::new();
    for s in stubs
        .iter()
        .filter(|s| sel.matches(s.space.as_deref()) && m.matches(s.metadata.as_ref()))
    {
        if app.stores.stubs.delete(s.id).await? {
            ids.push(s.id);
        }
    }
    if !ids.is_empty() {
        refresh(&app).await?;
    }
    audit_event(
        &app,
        &p,
        "stub.delete_by_metadata",
        json!({"deleted": ids.len(), "ids": ids.iter().take(1000).collect::<Vec<_>>()}),
    )
    .await;
    Ok(Json(DeletedIds {
        deleted: ids.len() as u64,
        ids,
    }))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SinceQuery {
    /// Only journal entries at or after this instant (RFC 3339) count as hits.
    since: Option<chrono::DateTime<chrono::Utc>>,
    /// Only stubs of this space.
    space: Option<String>,
}

#[utoipa::path(
    get, path = "/__admin/stubs/unused", tag = "stubs", operation_id = "listUnusedStubs",
    summary = "Stubs that no journal entry matched",
    description = "Computed from the request journal, so the answer only covers the journal retention window.",
    params(SinceQuery),
    responses(
        (status = 200, description = "Unused stubs", body = StubList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn unused_stubs(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SinceQuery>,
) -> ApiResult<Json<StubList>> {
    let hits: std::collections::HashSet<Uuid> = app
        .stores
        .journal
        .stub_hits(q.since)
        .await?
        .into_iter()
        .collect();
    let sel = spaces::select(&app, &p, Permission::StubsRead, q.space.as_deref())?;
    let mut stubs = app.stores.stubs.list().await?;
    stubs.retain(|s| sel.matches(s.space.as_deref()) && !hits.contains(&s.id));
    mask_for(&app, &p, &mut stubs);
    Ok(Json(StubList {
        total: stubs.len(),
        offset: 0,
        limit: None,
        stubs,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportBody {
    #[schema(value_type = Vec<Stub>)]
    stubs: Vec<Value>,
    #[serde(default)]
    replace_tag: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct BulkImportResult {
    pub imported: usize,
    pub removed: u64,
}

#[utoipa::path(
    post, path = "/__admin/stubs/import", tag = "stubs", operation_id = "importStubs",
    summary = "Bulk import FastMock stubs",
    description = "With `replaceTag`, stubs carrying that tag (in the target space) are deleted first and the tag is added to every imported stub, in one transaction. At most 10000 stubs. Every stub goes to the target space (`space` parameter; without it the global area or the caller's only space).",
    params(spaces::ImportQuery),
    request_body = ImportBody,
    responses(
        (status = 200, description = "Imported", body = BulkImportResult),
        (status = 400, description = "Invalid stub", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_stubs(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<spaces::ImportQuery>,
    Json(body): Json<ImportBody>,
) -> ApiResult<Json<BulkImportResult>> {
    let target = spaces::target(&app, &p, Permission::ImportsRun, q.space.as_deref())?;
    let host = spaces::import_host(&app, target.as_deref(), q.host.as_deref())?;
    if body.stubs.len() > 10_000 {
        return Err(bad("at most 10000 stubs per import"));
    }
    let mut parsed = Vec::with_capacity(body.stubs.len());
    for (i, v) in body.stubs.into_iter().enumerate() {
        let mut s = parse_stub(v).map_err(|e| bad(format!("stubs[{i}]: {}", e.1)))?;
        s.updated_at = None;
        if s.space.is_some() && s.space != target {
            return Err(bad(format!(
                "stubs[{i}]: the space of the stub differs from the target space of the import"
            )));
        }
        s.space = target.clone();
        spaces::set_import_host(&mut s, host.as_deref());
        spaces::check_stub(&app, &s).map_err(|e| bad(format!("stubs[{i}]: {e}")))?;
        if let Some(tag) = &body.replace_tag
            && !s.tags.contains(tag)
        {
            s.tags.push(tag.clone());
        }
        parsed.push(s);
    }
    spaces::check_ids(&app, &parsed, target.as_deref()).await?;
    let removed = app
        .stores
        .stubs
        .replace_tag(
            body.replace_tag.as_deref(),
            &SpaceFilter::area(target.as_deref()),
            &parsed,
        )
        .await?;
    refresh(&app).await?;
    audit_event_in(
        &app,
        &p,
        target.as_deref(),
        "stub.import",
        json!({"imported": parsed.len(), "replaced": removed, "tag": body.replace_tag}),
    )
    .await;
    Ok(Json(BulkImportResult {
        imported: parsed.len(),
        removed,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ValidationResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[utoipa::path(
    post, path = "/__admin/stubs/validate", tag = "stubs", operation_id = "validateStub",
    summary = "Validate a stub without saving it",
    request_body = Stub,
    responses(
        (status = 200, description = "Validation result", body = ValidationResult),
        (status = 400, description = "Not a stub document", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn validate_stub(
    State(app): State<AppState>,
    Json(v): Json<Value>,
) -> ApiResult<Json<ValidationResult>> {
    let stub = parse_stub(v)?;
    Ok(Json(match app.engine.validate(&stub) {
        Ok(()) => ValidationResult {
            valid: true,
            error: None,
        },
        Err(e) => ValidationResult {
            valid: false,
            error: Some(e),
        },
    }))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
pub struct RequestsQuery {
    /// `stubs` for the generic engine, otherwise a plugin name.
    source: Option<String>,
    method: Option<String>,
    path_prefix: Option<String>,
    correlation: Option<String>,
    body_contains: Option<String>,
    matched: Option<bool>,
    stub_id: Option<Uuid>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    #[param(minimum = 1, maximum = 10000)]
    limit: Option<i64>,
    #[param(minimum = 0)]
    offset: Option<i64>,
    /// Only entries of this space (matched by its stubs).
    space: Option<String>,
}

impl From<RequestsQuery> for JournalFilter {
    fn from(q: RequestsQuery) -> Self {
        JournalFilter {
            source: q.source,
            method: q.method,
            path_prefix: q.path_prefix,
            correlation: q.correlation,
            body_contains: q.body_contains,
            matched: q.matched,
            stub_id: q.stub_id,
            since: q.since,
            limit: q.limit,
            spaces: SpaceFilter::Any,
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct RequestList {
    pub requests: Vec<fastmock_core::JournalEntry>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

pub async fn journal_page(
    app: &AppState,
    mut f: JournalFilter,
    offset: Option<i64>,
) -> ApiResult<Json<RequestList>> {
    let offset = offset.unwrap_or(0);
    if offset < 0 {
        return Err(bad("offset must not be negative"));
    }
    let limit = f.limit.unwrap_or(100).clamp(1, 10_000);
    f.limit = Some(limit);
    let requests = app.stores.journal.query_page(&f, offset).await?;
    let total = app.stores.journal.count(&f).await?;
    Ok(Json(RequestList {
        requests,
        total,
        offset,
        limit,
    }))
}

#[utoipa::path(
    get, path = "/__admin/requests", tag = "requests", operation_id = "listRequests",
    summary = "Query the request journal",
    description = "Newest first. Headers, query parameters and bodies are stored redacted.",
    params(RequestsQuery),
    responses(
        (status = 200, description = "Journal entries", body = RequestList),
        (status = 400, description = "Invalid parameters", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_requests(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(mut q): Query<RequestsQuery>,
) -> ApiResult<Json<RequestList>> {
    let offset = q.offset;
    let sel = spaces::select(&app, &p, Permission::JournalRead, q.space.take().as_deref())?;
    let mut f: JournalFilter = q.into();
    f.spaces = sel;
    journal_page(&app, f, offset).await
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CountResult {
    pub count: i64,
}

#[utoipa::path(
    get, path = "/__admin/requests/count", tag = "requests", operation_id = "countRequests",
    summary = "Count journal entries",
    params(RequestsQuery),
    responses(
        (status = 200, description = "Count", body = CountResult),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn count_requests(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(mut q): Query<RequestsQuery>,
) -> ApiResult<Json<CountResult>> {
    let sel = spaces::select(&app, &p, Permission::JournalRead, q.space.take().as_deref())?;
    let mut f: JournalFilter = q.into();
    f.spaces = sel;
    count_bounded(&app, f).await
}

async fn count_bounded(app: &AppState, f: JournalFilter) -> ApiResult<Json<CountResult>> {
    let store = app.stores.journal.clone();
    let rt = tokio::runtime::Handle::current();
    let count = crate::journal_api::run_bounded(app, "count", move |b| {
        b.charge(0)?;
        Ok(rt.block_on(store.count(&f)))
    })
    .await??;
    Ok(Json(CountResult { count }))
}

#[utoipa::path(
    post, path = "/__admin/requests/count", tag = "requests", operation_id = "countRequestsByFilter",
    summary = "Count journal entries (filter in the body)",
    params(SpaceQuery),
    request_body = JournalFilter,
    responses(
        (status = 200, description = "Count", body = CountResult),
        (status = 400, description = "Invalid filter", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn count_requests_post(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(mut f): Json<JournalFilter>,
) -> ApiResult<Json<CountResult>> {
    f.spaces = spaces::select(&app, &p, Permission::JournalRead, q.space.as_deref())?;
    count_bounded(&app, f).await
}

#[utoipa::path(
    delete, path = "/__admin/requests", tag = "requests", operation_id = "clearRequests",
    summary = "Clear the request journal",
    description = "Deletes the entries of `space`, or of everything the caller may delete.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Number of deleted entries", body = DeletedCount),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn clear_requests(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<DeletedCount>> {
    let sel = spaces::select(&app, &p, Permission::JournalDelete, q.space.as_deref())?;
    let n = app.stores.journal.clear(&sel).await?;
    audit_event_in(
        &app,
        &p,
        q.space.as_deref(),
        "journal.clear",
        json!({"deleted": n}),
    )
    .await;
    Ok(Json(DeletedCount { deleted: n }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NearMissBody {
    method: String,
    path: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NearMissDoc {
    pub stub_id: Uuid,
    pub name: Option<String>,
    /// Share of matchers that matched, 0..1.
    pub score: f64,
    pub mismatches: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NearMissList {
    #[schema(value_type = Vec<NearMissDoc>)]
    pub near_misses: Vec<fastmock_core::matcher::NearMiss>,
}

#[utoipa::path(
    post, path = "/__admin/requests/near-misses", tag = "requests", operation_id = "nearMisses",
    summary = "Explain why a request does not match",
    description = "Returns the five closest stubs (of `space`, or of the spaces the caller may read) with the matchers that failed.",
    params(SpaceQuery),
    request_body(content = NearMissBody, example = json!({"method": "GET", "path": "/users/7", "headers": {"accept": "application/json"}})),
    responses(
        (status = 200, description = "Closest stubs", body = NearMissList),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 422, description = "The request to explain exceeds 1 MiB", body = ErrorBody),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn near_misses(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<NearMissBody>,
) -> ApiResult<Json<NearMissList>> {
    let sel = spaces::select(&app, &p, Permission::JournalRead, q.space.as_deref())?;
    let size = b.body.as_ref().map_or(0, String::len)
        + b.query.as_ref().map_or(0, String::len)
        + b.headers
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>();
    if size > MAX_NEAR_MISS_REQUEST_BYTES {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the request to explain exceeds {MAX_NEAR_MISS_REQUEST_BYTES} bytes"),
        ));
    }
    let engine = app.engine.clone();
    let near_misses = crate::journal_api::run_bounded(&app, "near_misses", move |budget| {
        budget.charge(size as u64)?;
        let req = CapturedRequest::new(
            &b.method,
            &b.path,
            b.query.as_deref(),
            b.headers.into_iter().collect(),
            b.body.unwrap_or_default().into(),
        );
        Ok(engine.near_misses(&req, 5, &sel))
    })
    .await?;
    Ok(Json(NearMissList { near_misses }))
}

const MAX_NEAR_MISS_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallbacksQuery {
    source: Option<String>,
    correlation: Option<String>,
    /// pending, sent or failed
    status: Option<String>,
    #[param(minimum = 1, maximum = 10000)]
    limit: Option<i64>,
    /// Only jobs of this space.
    space: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CallbackList {
    pub callbacks: Vec<fastmock_core::CallbackJob>,
}

#[utoipa::path(
    get, path = "/__admin/callbacks", tag = "callbacks", operation_id = "listCallbacks",
    summary = "List callback deliveries",
    description = "Newest first. Sensitive headers and body fields are masked.",
    params(CallbacksQuery),
    responses(
        (status = 200, description = "Callback jobs", body = CallbackList),
        (status = 400, description = "Invalid status", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_callbacks(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<CallbacksQuery>,
) -> ApiResult<Json<CallbackList>> {
    let sel = spaces::select(&app, &p, Permission::CallbacksRead, q.space.as_deref())?;
    let status = match q.status.as_deref() {
        None => None,
        Some(s) => Some(
            fastmock_core::CallbackStatus::parse(s)
                .ok_or_else(|| bad("status must be pending, sent or failed"))?,
        ),
    };
    let f = CallbackFilter {
        source: q.source,
        correlation: q.correlation,
        status,
        limit: q.limit,
        spaces: sel,
    };
    let mut jobs = app.stores.outbox.list(&f).await?;
    for j in &mut jobs {
        let redacted = app
            .redactor
            .headers(j.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        j.headers = redacted;
        j.body = app.redactor.body(
            j.body.as_bytes(),
            j.headers.get("content-type").map(String::as_str),
            app.cfg.journal_max_body,
        );
    }
    Ok(Json(CallbackList { callbacks: jobs }))
}

#[utoipa::path(
    delete, path = "/__admin/callbacks", tag = "callbacks", operation_id = "clearCallbacks",
    summary = "Delete all callback jobs, including pending ones",
    description = "Deletes the jobs of `space`, or of everything the caller may delete.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Number of deleted jobs", body = DeletedCount),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn clear_callbacks(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<DeletedCount>> {
    let sel = spaces::select(&app, &p, Permission::CallbacksDelete, q.space.as_deref())?;
    let n = app.stores.outbox.clear(&sel).await?;
    audit_event_in(
        &app,
        &p,
        q.space.as_deref(),
        "callbacks.clear",
        json!({"deleted": n}),
    )
    .await;
    Ok(Json(DeletedCount { deleted: n }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ScenarioState {
    pub name: String,
    pub state: String,
    /// Space of the scenario; absent in the global area.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ScenarioList {
    pub scenarios: Vec<ScenarioState>,
}

#[utoipa::path(
    get, path = "/__admin/scenarios", tag = "scenarios", operation_id = "listScenarios",
    summary = "Current scenario states",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Scenarios", body = ScenarioList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_scenarios(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<ScenarioList>> {
    let sel = spaces::select(&app, &p, Permission::StateRead, q.space.as_deref())?;
    let states = app.engine.scenario_states().await?;
    Ok(Json(ScenarioList {
        scenarios: states
            .into_iter()
            .filter_map(|(space, name, state)| {
                sel.matches(space.as_deref())
                    .then_some(ScenarioState { name, state, space })
            })
            .collect(),
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct StateBody {
    #[schema(example = "Paid")]
    state: String,
}

#[utoipa::path(
    put, path = "/__admin/scenarios/{name}", tag = "scenarios", operation_id = "setScenarioState",
    summary = "Set a scenario state",
    params(("name" = String, Path, description = "Scenario name"), SpaceQuery),
    request_body = StateBody,
    responses(
        (status = 204, description = "State set"),
        (status = 400, description = "Name or state too long", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn set_scenario(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<StateBody>,
) -> ApiResult<StatusCode> {
    let space = spaces::target(&app, &p, Permission::StateWrite, q.space.as_deref())?;
    if name.len() > 512 || b.state.len() > 512 {
        return Err(bad("name and state are limited to 512 bytes"));
    }
    app.engine
        .set_scenario_state(space.as_deref(), &name, &b.state)
        .await?;
    audit_event_in(
        &app,
        &p,
        space.as_deref(),
        "scenario.set",
        json!({"name": name, "state": b.state}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ResetCount {
    pub reset: u64,
}

#[utoipa::path(
    delete, path = "/__admin/scenarios", tag = "scenarios", operation_id = "resetScenarios",
    summary = "Reset every scenario to Started",
    description = "Resets the scenarios of `space`, or of everything the caller may change.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Number of reset scenarios", body = ResetCount),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn reset_scenarios(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<ResetCount>> {
    let n = match spaces::select(&app, &p, Permission::StateWrite, q.space.as_deref())? {
        SpaceFilter::In(list) => app.engine.reset_scenarios_in(&list).await?,
        _ => app.engine.reset_scenarios().await?,
    };
    audit_event_in(
        &app,
        &p,
        q.space.as_deref(),
        "scenario.reset",
        json!({"reset": n}),
    )
    .await;
    Ok(Json(ResetCount { reset: n }))
}

/// Hot-store key of a key-value entry: `kv:{key}` globally, `kv@{space}:{key}` in a space, so that no
/// global key can name space data.
fn kv_key(space: Option<&str>, key: &str) -> ApiResult<String> {
    if key.is_empty() || key.len() > 512 {
        return Err(bad("key must be 1..512 bytes"));
    }
    Ok(match space {
        Some(s) => format!("kv@{s}:{key}"),
        None => format!("kv:{key}"),
    })
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct KvBody {
    #[schema(example = "42")]
    value: String,
    #[serde(default)]
    #[schema(minimum = 1, maximum = 31536000)]
    ttl_seconds: Option<u64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct KvValue {
    pub key: String,
    pub value: String,
}

#[utoipa::path(
    get, path = "/__admin/kv/{key}", tag = "kv", operation_id = "getKv",
    summary = "Read a key from the shared key-value store",
    params(("key" = String, Path, description = "Key, 1..512 bytes"), SpaceQuery),
    responses(
        (status = 200, description = "Value", body = KvValue),
        (status = 400, description = "Invalid key", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "No such key", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn kv_get(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(key): Path<String>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<KvValue>> {
    let space = spaces::target(&app, &p, Permission::StateRead, q.space.as_deref())?;
    match app.stores.hot.get(&kv_key(space.as_deref(), &key)?).await? {
        Some(value) => Ok(Json(KvValue { key, value })),
        None => Err(not_found("key")),
    }
}

#[utoipa::path(
    put, path = "/__admin/kv/{key}", tag = "kv", operation_id = "putKv",
    summary = "Write a key",
    params(("key" = String, Path, description = "Key, 1..512 bytes"), SpaceQuery),
    request_body = KvBody,
    responses(
        (status = 204, description = "Stored"),
        (status = 400, description = "Invalid key, value or TTL", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn kv_put(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(key): Path<String>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<KvBody>,
) -> ApiResult<StatusCode> {
    let space = spaces::target(&app, &p, Permission::StateWrite, q.space.as_deref())?;
    if b.value.len() > 1024 * 1024 {
        return Err(bad("value is limited to 1 MiB"));
    }
    if b.ttl_seconds
        .is_some_and(|t| t == 0 || t > fastmock_core::engine::MAX_TTL_SECS)
    {
        return Err(bad("ttlSeconds must be between 1 and 31536000"));
    }
    app.stores
        .hot
        .set(
            &kv_key(space.as_deref(), &key)?,
            &b.value,
            b.ttl_seconds.map(Duration::from_secs),
        )
        .await?;
    audit_event_in(&app, &p, space.as_deref(), "kv.set", json!({"key": key})).await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete, path = "/__admin/kv/{key}", tag = "kv", operation_id = "deleteKv",
    summary = "Delete a key",
    params(("key" = String, Path, description = "Key, 1..512 bytes"), SpaceQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "Invalid key", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "No such key"),
    ),
    security(("bearer" = []))
)]
pub async fn kv_delete(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(key): Path<String>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    let space = spaces::target(&app, &p, Permission::StateWrite, q.space.as_deref())?;
    let found = app
        .stores
        .hot
        .delete(&kv_key(space.as_deref(), &key)?)
        .await?;
    audit_event_in(&app, &p, space.as_deref(), "kv.delete", json!({"key": key})).await;
    Ok(if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ResetResult {
    requests: u64,
    callbacks: u64,
    scenarios: u64,
}

#[utoipa::path(
    post, path = "/__admin/reset", tag = "system", operation_id = "reset",
    summary = "Clear the journal, callbacks and scenario states",
    description = "Stubs are kept. Needs the admin role.",
    responses(
        (status = 200, description = "What was cleared", body = ResetResult),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the admin role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn reset(State(app): State<AppState>, Extension(p): Extension<Principal>) -> Response {
    if let Some(denied) = admin_route_denial(&p, Permission::SystemReset) {
        return denied;
    }
    reset_all(&app, &p).await.into_response()
}

async fn reset_all(app: &AppState, p: &Principal) -> ApiResult<Json<ResetResult>> {
    let r = ResetResult {
        requests: app.stores.journal.clear(&SpaceFilter::Any).await?,
        callbacks: app.stores.outbox.clear(&SpaceFilter::Any).await?,
        scenarios: app.engine.reset_scenarios().await?,
    };
    app.engine.reset_counters().await?;
    audit_event(
        app,
        p,
        "reset",
        json!({"requests": r.requests, "callbacks": r.callbacks, "scenarios": r.scenarios}),
    )
    .await;
    Ok(Json(r))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InfoResponse {
    pub name: String,
    pub version: String,
    /// `memory` or `postgres+valkey`
    pub storage: String,
    pub uptime_seconds: u64,
    /// Loaded stubs the caller may read.
    pub stubs_loaded: usize,
    pub plugins: Vec<crate::state::PluginInfo>,
    pub principal: Principal,
    pub auth_disabled: bool,
    /// Random id of the answering server process, new on every start. A change tells clients such as the
    /// operator that an in-memory server lost its stubs.
    #[schema(example = "6f1c0d3a9b8e4f27a1c5d2e3f4a5b6c7")]
    pub instance_id: String,
    /// Stored stubs the caller can read that break the rules of their space (outside its path prefixes or
    /// hosts, or in a space the access file no longer defines), typically after the access file changed.
    /// They stay stored; fix or delete them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[utoipa::path(
    get, path = "/__admin/info", tag = "system", operation_id = "info",
    summary = "Server information and the caller's identity",
    responses(
        (status = 200, description = "Info", body = InfoResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn info(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Json<InfoResponse> {
    Json(InfoResponse {
        name: "FastMock".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        storage: match app.cfg.storage {
            crate::config::StorageKind::Memory => "memory",
            _ => "postgres+valkey",
        }
        .into(),
        uptime_seconds: app.started.elapsed().as_secs(),
        stubs_loaded: app
            .engine
            .snapshot()
            .iter()
            .filter(|c| p.allowed(Permission::StubsRead, c.stub.space.as_deref()))
            .count(),
        plugins: (*app.plugins).clone(),
        warnings: crate::spaces::ownership_problems(
            app.auth.access(),
            app.engine.snapshot().iter().map(|c| &c.stub),
        )
        .into_iter()
        .filter(|w| p.allowed(Permission::StubsRead, Some(&w.space)))
        .map(|w| w.to_string())
        .collect(),
        principal: p,
        auth_disabled: app.auth.disabled(),
        instance_id: app.instance_id.to_string(),
    })
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpaceInfo {
    #[schema(example = "payments")]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Data-plane path prefixes the space's stubs must stay under (empty: no restriction).
    pub path_prefixes: Vec<String>,
    /// Hosts the space's stubs must match (empty: no restriction).
    pub hosts: Vec<String>,
    /// The caller's effective permissions in the space.
    pub permissions: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SpaceList {
    pub spaces: Vec<SpaceInfo>,
}

#[utoipa::path(
    get, path = "/__admin/spaces", tag = "system", operation_id = "listSpaces",
    summary = "Spaces the caller can see",
    description = "Spaces come from the access file (`FASTMOCK_ACCESS_FILE`). A principal holding a permission globally sees every space; a principal bound to spaces sees only those.",
    responses(
        (status = 200, description = "Spaces with the caller's permissions in each", body = SpaceList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_spaces(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Json<SpaceList> {
    let all = p.permissions.sees_all_spaces();
    let spaces = app
        .auth
        .access()
        .spaces()
        .filter_map(|s| {
            let perms = p.permissions.in_space(&s.name);
            (all || p.permissions.bound_spaces().any(|(n, _)| *n == s.name)).then(|| SpaceInfo {
                name: s.name.clone(),
                description: s.description.clone(),
                path_prefixes: s.path_prefixes.clone(),
                hosts: s.hosts.clone(),
                permissions: perms.names().into_iter().map(String::from).collect(),
            })
        })
        .collect();
    Json(SpaceList { spaces })
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VersionResponse {
    #[schema(example = "0.1.0")]
    pub version: String,
    /// Commit the binary was built from, when the build set `FASTMOCK_GIT_SHA`.
    pub git_sha: Option<String>,
    /// Compiled-in optional features and capabilities.
    pub features: Vec<String>,
}

pub fn version_info() -> VersionResponse {
    let mut features: Vec<String> = Vec::new();
    features.extend(
        [
            "grpc",
            "kafka",
            "sse",
            "chaos",
            "recording",
            "verification",
            "files",
            "tokens",
            "import-openapi",
            "import-wiremock",
            "import-postman",
            "import-har",
        ]
        .map(String::from),
    );
    VersionResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        git_sha: option_env!("FASTMOCK_GIT_SHA")
            .filter(|s| !s.is_empty())
            .map(String::from),
        features,
    }
}

#[utoipa::path(
    get, path = "/__admin/version", tag = "system", operation_id = "version",
    summary = "Build version",
    responses(
        (status = 200, description = "Version", body = VersionResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn version() -> Json<VersionResponse> {
    Json(version_info())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    /// Exact action, or a prefix ending in `*` (for example `stub.*`).
    action: Option<String>,
    principal: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    #[param(minimum = 1, maximum = 10000)]
    limit: Option<i64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AuditList {
    pub events: Vec<fastmock_core::AuditEvent>,
}

#[utoipa::path(
    get, path = "/__admin/audit", tag = "audit", operation_id = "listAudit",
    summary = "Audit log of admin changes",
    description = "Newest first. Stub updates carry the changed fields (`changes`), deletes the removed stub (`before`); secrets are masked. Needs the admin role.",
    params(AuditQuery),
    responses(
        (status = 200, description = "Audit events", body = AuditList),
        (status = 400, description = "Invalid parameters", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the admin role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_audit(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<AuditQuery>,
) -> Response {
    if let Some(denied) = admin_route_denial(&p, Permission::AuditRead) {
        return denied;
    }
    audit_events(&app, q).await.into_response()
}

async fn audit_events(app: &AppState, q: AuditQuery) -> ApiResult<Json<AuditList>> {
    if q.action.as_ref().is_some_and(|a| a.len() > 128)
        || q.principal.as_ref().is_some_and(|a| a.len() > 128)
    {
        return Err(bad("action and principal are limited to 128 bytes"));
    }
    let f = fastmock_core::AuditFilter {
        action: q.action,
        principal: q.principal,
        since: q.since,
        limit: Some(q.limit.unwrap_or(100)),
    };
    Ok(Json(AuditList {
        events: app.stores.audit.query(&f).await?,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct PluginList {
    pub plugins: Vec<crate::state::PluginInfo>,
}

#[utoipa::path(
    get, path = "/__admin/plugins", tag = "system", operation_id = "listPlugins",
    summary = "Enabled provider plugins",
    responses(
        (status = 200, description = "Plugins", body = PluginList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn plugins(State(app): State<AppState>) -> Json<PluginList> {
    Json(PluginList {
        plugins: (*app.plugins).clone(),
    })
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    /// Stubs saved (0 on a dry run).
    pub imported: usize,
    /// Stubs deleted because they carried the replaced tag.
    pub removed: u64,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// The generated stubs, only on a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stubs: Option<Vec<Stub>>,
}

pub struct ImportTarget<'a> {
    pub format: &'a str,
    pub tag: &'a str,
    pub replace: bool,
    pub dry_run: bool,
    /// Space the stubs go to; `None` for the global area.
    pub space: Option<&'a str>,
    /// `request.host` for stubs without one ([`spaces::import_host`]).
    pub host: Option<&'a str>,
}

pub async fn finish_import(
    app: &AppState,
    p: &Principal,
    t: ImportTarget<'_>,
    stubs: Vec<Stub>,
    warnings: Vec<String>,
) -> ApiResult<Json<ImportResult>> {
    let ImportTarget {
        format,
        tag,
        replace,
        dry_run,
        space,
        host,
    } = t;
    if stubs.len() > 10_000 {
        return Err(bad(format!(
            "the document produces {} stubs; at most 10000 per import",
            stubs.len()
        )));
    }
    metrics::counter!("fastmock_imports_total", "format" => format.to_string(), "dry_run" => dry_run.to_string()).increment(1);
    let mut warnings = warnings;
    let mut valid = Vec::with_capacity(stubs.len());
    for mut s in stubs {
        s.space = space.map(String::from);
        spaces::set_import_host(&mut s, host);
        match app.engine.validate(&s) {
            Ok(()) => {
                if let Err(e) = spaces::check_stub(app, &s) {
                    return Err(bad(format!(
                        "{}: {e}",
                        s.name.clone().unwrap_or_else(|| s.id.to_string())
                    )));
                }
                valid.push(s)
            }
            Err(e) => warnings.push(format!(
                "{}: the generated stub is invalid ({e}); it was skipped",
                s.name.clone().unwrap_or_else(|| s.id.to_string())
            )),
        }
    }
    let stubs = valid;
    if stubs.is_empty() {
        metrics::counter!("fastmock_import_rejections_total", "format" => format.to_string())
            .increment(1);
        let mut msg = String::from("the document produced no stubs; nothing was changed");
        for w in warnings.iter().take(5) {
            msg.push_str("; ");
            msg.push_str(w);
        }
        if warnings.len() > 5 {
            msg.push_str(&format!("; and {} more warnings", warnings.len() - 5));
        }
        if msg.len() > 2048 {
            let mut cut = 2048;
            while !msg.is_char_boundary(cut) {
                cut -= 1;
            }
            msg.truncate(cut);
            msg.push('…');
        }
        return Err(bad(msg));
    }
    if dry_run {
        return Ok(Json(ImportResult {
            imported: 0,
            removed: 0,
            warnings,
            dry_run: true,
            stubs: Some(stubs),
        }));
    }
    let (n, removed) = store_imported(app, stubs, tag, replace, space).await?;
    audit_event_in(
        app,
        p,
        space,
        &format!("import.{format}"),
        json!({"tag": tag, "imported": n, "removed": removed}),
    )
    .await;
    Ok(Json(ImportResult {
        imported: n,
        removed,
        warnings,
        dry_run: false,
        stubs: None,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenApiImport {
    /// OpenAPI 3 document as a JSON object or as JSON/YAML text.
    #[schema(value_type = Object)]
    spec: Value,
    #[schema(example = "petstore")]
    tag: String,
    #[serde(default = "yes")]
    validate_requests: bool,
    #[serde(default)]
    path_prefix: String,
    #[serde(default = "yes")]
    replace: bool,
    #[serde(default)]
    dry_run: bool,
}

pub fn yes() -> bool {
    true
}

pub async fn store_imported(
    app: &AppState,
    stubs: Vec<Stub>,
    tag: &str,
    replace: bool,
    space: Option<&str>,
) -> ApiResult<(usize, u64)> {
    for (i, s) in stubs.iter().enumerate() {
        app.engine.validate(s).map_err(|e| {
            bad(format!(
                "generated stub #{i} ({}) is invalid: {e}",
                s.name.clone().unwrap_or_default()
            ))
        })?;
    }
    spaces::check_ids(app, &stubs, space).await?;
    let removed = app
        .stores
        .stubs
        .replace_tag(replace.then_some(tag), &SpaceFilter::area(space), &stubs)
        .await?;
    refresh(app).await?;
    Ok((stubs.len(), removed))
}

pub fn valid_tag(tag: &str) -> ApiResult<()> {
    if tag.is_empty()
        || tag.len() > 128
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
    {
        return Err(bad("tag must be 1..128 characters of [A-Za-z0-9-_.:/]"));
    }
    Ok(())
}

#[utoipa::path(
    post, path = "/__admin/import/openapi", tag = "imports", operation_id = "importOpenApi",
    summary = "Generate stubs from an OpenAPI 3 document",
    description = "The stubs go to the target space (`space` parameter); only imports into the global area are recorded in the service catalog.",
    params(spaces::ImportQuery),
    request_body(content = OpenApiImport, example = json!({"spec": "openapi: 3.0.3\ninfo: {title: Pets, version: '1'}\npaths: {}", "tag": "petstore"})),
    responses(
        (status = 200, description = "Import result", body = ImportResult),
        (status = 400, description = "Invalid document or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_openapi(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<spaces::ImportQuery>,
    Json(b): Json<OpenApiImport>,
) -> ApiResult<Json<ImportResult>> {
    let space = spaces::target(&app, &p, Permission::ImportsRun, q.space.as_deref())?;
    let host = spaces::import_host(&app, space.as_deref(), q.host.as_deref())?;
    valid_tag(&b.tag)?;
    let doc = match &b.spec {
        Value::String(text) => fastmock_core::openapi::parse_document(text)
            .map_err(|e| bad(format!("spec is neither JSON nor YAML: {e}")))?,
        other => other.clone(),
    };
    let opts = fastmock_core::openapi::ImportOptions {
        tag: b.tag.clone(),
        validate_requests: b.validate_requests,
        path_prefix: b.path_prefix.clone(),
    };
    let r = fastmock_core::openapi::import(&doc, &opts).map_err(|e| bad(e.to_string()))?;
    let res = finish_import(
        &app,
        &p,
        ImportTarget {
            format: "openapi",
            tag: &b.tag,
            replace: b.replace,
            dry_run: b.dry_run,
            space: space.as_deref(),
            host: host.as_deref(),
        },
        r.stubs,
        r.warnings,
    )
    .await?;
    if !b.dry_run && space.is_none() {
        crate::services::record_service(
            &app,
            &p.name,
            crate::services::ServiceInfo::openapi(&doc, &opts),
        )
        .await;
    }
    Ok(res)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WireMockImport {
    /// `{"mappings": [...]}`, an array of mappings or a single mapping.
    #[schema(value_type = Object)]
    mappings: Value,
    #[serde(default = "wiremock_tag")]
    tag: String,
    #[serde(default = "yes")]
    replace: bool,
    #[serde(default)]
    dry_run: bool,
}

fn wiremock_tag() -> String {
    "wiremock".into()
}

#[utoipa::path(
    post, path = "/__admin/import/wiremock", tag = "imports", operation_id = "importWireMock",
    summary = "Convert WireMock mappings into stubs",
    description = "Unsupported parts come back as warnings. The stubs go to the target space (`space` parameter).",
    params(spaces::ImportQuery),
    request_body(content = WireMockImport, example = json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/ping"}, "response": {"status": 200, "body": "pong"}}]}, "tag": "legacy"})),
    responses(
        (status = 200, description = "Import result", body = ImportResult),
        (status = 400, description = "Invalid mappings or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_wiremock(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<spaces::ImportQuery>,
    Json(b): Json<WireMockImport>,
) -> ApiResult<Json<ImportResult>> {
    let space = spaces::target(&app, &p, Permission::ImportsRun, q.space.as_deref())?;
    let host = spaces::import_host(&app, space.as_deref(), q.host.as_deref())?;
    valid_tag(&b.tag)?;
    let mut c = fastmock_core::wiremock::convert(&b.mappings);
    for s in &mut c.stubs {
        s.tags.push(b.tag.clone());
    }
    let res = finish_import(
        &app,
        &p,
        ImportTarget {
            format: "wiremock",
            tag: &b.tag,
            replace: b.replace,
            dry_run: b.dry_run,
            space: space.as_deref(),
            host: host.as_deref(),
        },
        c.stubs,
        c.warnings,
    )
    .await?;
    if !b.dry_run && space.is_none() {
        crate::services::record_service(
            &app,
            &p.name,
            crate::services::ServiceInfo::generic("wiremock", &b.tag),
        )
        .await;
    }
    Ok(res)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ExportResult {
    pub stubs: Vec<Stub>,
}

#[utoipa::path(
    get, path = "/__admin/export", tag = "stubs", operation_id = "exportStubs",
    summary = "Export stubs in import format",
    params(TagQuery),
    responses(
        (status = 200, description = "Stubs without server-managed fields", body = ExportResult),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn export_stubs(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<TagQuery>,
) -> ApiResult<Json<ExportResult>> {
    let sel = spaces::select(&app, &p, Permission::StubsRead, q.space.as_deref())?;
    let mut stubs = app.stores.stubs.list().await?;
    stubs.retain(|s| sel.matches(s.space.as_deref()));
    mask_for(&app, &p, &mut stubs);
    if let Some(t) = &q.tag {
        stubs.retain(|s| s.tags.contains(t));
    }
    for s in &mut stubs {
        s.updated_at = None;
    }
    Ok(Json(ExportResult { stubs }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStart {
    #[schema(example = "https://api.example.com")]
    target_base_url: String,
    #[schema(example = "recorded-api")]
    tag: String,
    /// Response headers to keep in recorded stubs (content-type is always kept).
    #[serde(default)]
    capture_headers: Vec<String>,
    /// Request headers to add as matchers.
    #[serde(default)]
    request_headers: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct RecordingStatus {
    pub recording: Option<crate::proxy::Recording>,
}

#[utoipa::path(
    post, path = "/__admin/recordings/start", tag = "recordings", operation_id = "startRecording",
    summary = "Proxy unmatched requests to a target and record them as stubs",
    request_body = RecordingStart,
    responses(
        (status = 200, description = "Recording started", body = RecordingStatus),
        (status = 400, description = "Invalid or blocked target URL, invalid tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn recording_start(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<RecordingStart>,
) -> ApiResult<Json<RecordingStatus>> {
    valid_tag(&b.tag)?;
    let url = reqwest::Url::parse(&b.target_base_url)
        .map_err(|e| bad(format!("invalid targetBaseUrl: {e}")))?;
    app.policy
        .check_url(&url)
        .map_err(|e| bad(format!("targetBaseUrl is not allowed: {e}")))?;
    let rec = crate::proxy::Recording {
        target_base_url: b.target_base_url,
        tag: b.tag,
        capture_headers: b.capture_headers,
        request_headers: b.request_headers,
        started_by: p.name.clone(),
        started_at: Some(chrono::Utc::now()),
    };
    app.stores.hot.delete("recording:count").await?;
    app.stores
        .hot
        .set(
            crate::proxy::RECORDING_KEY,
            &serde_json::to_string(&rec).map_err(|e| ApiError::from(anyhow::Error::from(e)))?,
            None,
        )
        .await?;
    audit_event(
        &app,
        &p,
        "recording.start",
        json!({"target": rec.target_base_url, "tag": rec.tag}),
    )
    .await;
    Ok(Json(RecordingStatus {
        recording: Some(rec),
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStopResult {
    pub stopped: bool,
    pub recorded_stubs: usize,
}

#[utoipa::path(
    post, path = "/__admin/recordings/stop", tag = "recordings", operation_id = "stopRecording",
    summary = "Stop recording",
    responses(
        (status = 200, description = "Recording stopped", body = RecordingStopResult),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn recording_stop(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> ApiResult<Json<RecordingStopResult>> {
    let rec = crate::proxy::active_recording(&app).await;
    app.stores.hot.delete(crate::proxy::RECORDING_KEY).await?;
    app.stores.hot.delete_prefix("recording:seen:").await?;
    let recorded = match &rec {
        Some(r) => app
            .stores
            .stubs
            .list()
            .await?
            .into_iter()
            .filter(|s| s.tags.contains(&r.tag) && s.tags.iter().any(|t| t == "recorded"))
            .count(),
        None => 0,
    };
    audit_event(
        &app,
        &p,
        "recording.stop",
        json!({"tag": rec.as_ref().map(|r| r.tag.clone()), "stubs": recorded}),
    )
    .await;
    Ok(Json(RecordingStopResult {
        stopped: rec.is_some(),
        recorded_stubs: recorded,
    }))
}

#[utoipa::path(
    get, path = "/__admin/recordings", tag = "recordings", operation_id = "recordingStatus",
    summary = "Active recording, if any",
    responses(
        (status = 200, description = "Status", body = RecordingStatus),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn recording_status(State(app): State<AppState>) -> Json<RecordingStatus> {
    Json(RecordingStatus {
        recording: crate::proxy::active_recording(&app).await,
    })
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct ChaosBody {
    rules: Vec<fastmock_core::ChaosRule>,
}

#[utoipa::path(
    get, path = "/__admin/chaos", tag = "chaos", operation_id = "getChaos",
    summary = "Active chaos rules",
    responses(
        (status = 200, description = "Rules", body = ChaosBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn chaos_get(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let raw = app.stores.hot.get(crate::extras::CHAOS_KEY).await?;
    let rules: Value = raw
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| json!([]));
    Ok(Json(json!({"rules": rules})))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ChaosSetResult {
    /// Number of active rules.
    pub rules: usize,
}

#[utoipa::path(
    put, path = "/__admin/chaos", tag = "chaos", operation_id = "putChaos",
    summary = "Replace the chaos rules",
    request_body(content = ChaosBody, example = json!({"rules": [{"id": "slow-orders", "pathPrefix": "/orders", "probability": 0.2, "delay": {"fixedMs": 1500}}]})),
    responses(
        (status = 200, description = "Rules stored", body = ChaosSetResult),
        (status = 400, description = "Invalid rule", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn chaos_put(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<ChaosBody>,
) -> ApiResult<Json<ChaosSetResult>> {
    if b.rules.len() > 100 {
        return Err(bad("at most 100 chaos rules"));
    }
    for r in &b.rules {
        r.validate().map_err(bad)?;
    }
    let raw =
        serde_json::to_string(&b.rules).map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    app.stores
        .hot
        .set(crate::extras::CHAOS_KEY, &raw, None)
        .await?;
    app.chaos.invalidate().await;
    audit_event(
        &app,
        &p,
        "chaos.set",
        json!({"rules": b.rules.iter().map(|r| &r.id).collect::<Vec<_>>()}),
    )
    .await;
    Ok(Json(ChaosSetResult {
        rules: b.rules.len(),
    }))
}

#[utoipa::path(
    delete, path = "/__admin/chaos", tag = "chaos", operation_id = "clearChaos",
    summary = "Remove every chaos rule",
    responses(
        (status = 204, description = "Cleared"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn chaos_delete(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> ApiResult<StatusCode> {
    app.stores.hot.delete(crate::extras::CHAOS_KEY).await?;
    app.chaos.invalidate().await;
    audit_event(&app, &p, "chaos.clear", json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct GrpcDescriptor {
    pub name: String,
    /// Fully qualified service names.
    pub services: Vec<String>,
}

#[utoipa::path(
    put, path = "/__admin/grpc/descriptors/{name}", tag = "grpc", operation_id = "putGrpcDescriptor",
    summary = "Upload a binary FileDescriptorSet",
    description = "Produce it with `protoc --include_imports --descriptor_set_out=set.pb`. At most 16 MiB. A set that redefines services of another set with incompatible messages is rejected with 409.",
    params(("name" = String, Path, description = "Descriptor set name, [A-Za-z0-9-_.:/]{1,128}")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Stored", body = GrpcDescriptor),
        (status = 400, description = "Not a valid FileDescriptorSet or invalid name", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 409, description = "Conflicts with another descriptor set", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn grpc_put(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<GrpcDescriptor>> {
    valid_tag(&name)?;
    if body.len() > 16 * 1024 * 1024 {
        return Err(bad("descriptor set is limited to 16 MiB"));
    }
    let services = crate::grpc::validate_descriptor_set(&body)
        .map_err(|e| bad(format!("not a valid FileDescriptorSet: {e}")))?;
    crate::grpc::check_compatible(&app, &name, &body)
        .await
        .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
    use base64::Engine as _;
    let v = json!({"fileDescriptorSet": base64::engine::general_purpose::STANDARD.encode(&body), "services": services});
    app.stores
        .data
        .put(crate::grpc::NAMESPACE, &name, &v)
        .await?;
    app.stores.hot.incr(crate::grpc::REVISION_KEY, None).await?;
    app.grpc.reload(&app).await?;
    crate::services::record_service(
        &app,
        &p.name,
        crate::services::ServiceInfo::grpc(&name, &body),
    )
    .await;
    audit_event(
        &app,
        &p,
        "grpc.descriptor.put",
        json!({"name": name, "services": services}),
    )
    .await;
    Ok(Json(GrpcDescriptor { name, services }))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GrpcDescriptorList {
    pub descriptors: Vec<GrpcDescriptor>,
    pub loaded_services: Vec<String>,
}

#[utoipa::path(
    get, path = "/__admin/grpc/descriptors", tag = "grpc", operation_id = "listGrpcDescriptors",
    summary = "Uploaded descriptor sets and the services they serve",
    responses(
        (status = 200, description = "Descriptor sets", body = GrpcDescriptorList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn grpc_list(State(app): State<AppState>) -> ApiResult<Json<GrpcDescriptorList>> {
    let items = app
        .stores
        .data
        .list_without(crate::grpc::NAMESPACE, 1000, "fileDescriptorSet")
        .await?;
    let descriptors = items
        .into_iter()
        .map(|(name, v)| GrpcDescriptor {
            name,
            services: v
                .get("services")
                .cloned()
                .and_then(|s| serde_json::from_value(s).ok())
                .unwrap_or_default(),
        })
        .collect();
    Ok(Json(GrpcDescriptorList {
        descriptors,
        loaded_services: app.grpc.services().await,
    }))
}

#[utoipa::path(
    delete, path = "/__admin/grpc/descriptors/{name}", tag = "grpc", operation_id = "deleteGrpcDescriptor",
    summary = "Delete a descriptor set",
    params(("name" = String, Path, description = "Descriptor set name")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "No such descriptor set"),
    ),
    security(("bearer" = []))
)]
pub async fn grpc_delete(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let found = app
        .stores
        .data
        .delete(crate::grpc::NAMESPACE, &name)
        .await?;
    app.stores.hot.incr(crate::grpc::REVISION_KEY, None).await?;
    app.grpc.reload(&app).await?;
    audit_event(&app, &p, "grpc.descriptor.delete", json!({"name": name})).await;
    Ok(if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

pub async fn json_errors(req: axum::extract::Request, next: middleware::Next) -> Response {
    let resp = next.run(req).await;
    let mut status = resp.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    let is_json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if is_json {
        return resp;
    }
    if status == StatusCode::UNPROCESSABLE_ENTITY {
        status = StatusCode::BAD_REQUEST;
    }
    let (parts, body) = resp.into_parts();
    let text = match axum::body::to_bytes(body, 4096).await {
        Ok(b) => String::from_utf8_lossy(&b).trim().to_string(),
        Err(_) => String::new(),
    };
    let msg = if text.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("error")
            .to_ascii_lowercase()
    } else {
        text
    };
    let mut out = (
        status,
        Json(ErrorBody {
            error: msg,
            details: vec![],
            required: None,
        }),
    )
        .into_response();
    for (k, v) in parts.headers.iter() {
        if k != header::CONTENT_TYPE && k != header::CONTENT_LENGTH {
            out.headers_mut().insert(k.clone(), v.clone());
        }
    }
    out
}

async fn security_headers(req: axum::extract::Request, next: middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        );
    }
    resp
}

pub fn router(app: AppState, plugin_admin: Vec<(String, Router)>) -> Router {
    let mut api = Router::new()
        .route(
            "/stubs",
            get(list_stubs).post(create_stub).delete(delete_stubs),
        )
        .route("/stubs/import", post(import_stubs))
        .route("/stubs/validate", post(validate_stub))
        .route(
            "/stubs/{id}",
            get(get_stub)
                .put(put_stub)
                .patch(patch_stub)
                .delete(delete_stub),
        )
        .route("/stubs/find-by-metadata", post(find_by_metadata))
        .route("/stubs/remove-by-metadata", post(remove_by_metadata))
        .route("/stubs/unused", get(unused_stubs))
        .route("/requests", get(list_requests).delete(clear_requests))
        .route(
            "/requests/count",
            get(count_requests).post(count_requests_post),
        )
        .route("/requests/near-misses", post(near_misses))
        .route("/callbacks", get(list_callbacks).delete(clear_callbacks))
        .route("/scenarios", get(list_scenarios).delete(reset_scenarios))
        .route("/scenarios/{name}", put(set_scenario))
        .route("/kv/{key}", get(kv_get).put(kv_put).delete(kv_delete))
        .route("/reset", post(reset))
        .route("/info", get(info))
        .route("/spaces", get(list_spaces))
        .route("/version", get(version))
        .route("/plugins", get(plugins))
        .route("/audit", get(list_audit))
        .route("/import/openapi", post(import_openapi))
        .route("/import/wiremock", post(import_wiremock))
        .route("/export", get(export_stubs))
        .route("/chaos", get(chaos_get).put(chaos_put).delete(chaos_delete))
        .route("/grpc/descriptors", get(grpc_list))
        .route(
            "/grpc/descriptors/{name}",
            put(grpc_put).delete(grpc_delete),
        )
        .route("/recordings", get(recording_status))
        .route("/recordings/start", post(recording_start))
        .route("/recordings/stop", post(recording_stop))
        .merge(crate::journal_api::routes())
        .merge(crate::files::routes())
        .merge(crate::snapshot::routes())
        .merge(crate::imports::routes())
        .merge(crate::asyncmock::routes())
        .merge(crate::contract::routes())
        .merge(crate::services::routes())
        .merge(crate::admin_core::routes())
        .merge(crate::tokens::routes())
        .with_state(app.clone());
    for (name, r) in plugin_admin {
        api = api.nest(&format!("/plugins/{name}"), r);
    }
    Router::<AppState>::new()
        .nest_service(
            "/__admin",
            api.layer(middleware::from_fn_with_state(app.clone(), admin_auth))
                .layer(middleware::from_fn(json_errors))
                .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .merge(crate::ui::router())
        .merge(crate::oidc::routes::routes())
        .route("/healthz", get(crate::dataplane::health))
        .route("/readyz", get(crate::dataplane::ready))
        .route("/metrics", get(metrics_handler))
        .route("/openapi.json", get(crate::openapi_doc::serve_spec))
        .with_state(app)
        .layer(middleware::from_fn(security_headers))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            64 * 1024 * 1024,
        ))
}

async fn metrics_handler(
    Extension(handle): Extension<metrics_exporter_prometheus::PrometheusHandle>,
) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        handle.render(),
    )
}
