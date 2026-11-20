use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use fastmock_core::{StringMatcher, Stub};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::access::Permission;
use crate::admin::require;
use crate::admin::{
    ApiError, ApiResult, ErrorBody, ImportResult, ImportTarget, bad, finish_import, valid_tag, yes,
};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/import", post(import_auto))
        .route("/import/postman", post(import_postman))
        .route("/import/har", post(import_har))
}

fn document(v: Value, what: &str) -> ApiResult<Value> {
    match v {
        Value::String(text) => fastmock_core::openapi::parse_document(&text)
            .map_err(|e| bad(format!("{what} is neither JSON nor YAML: {e}"))),
        other => Ok(other),
    }
}

pub fn redact_imported(r: &crate::redact::Redactor, stubs: &mut [Stub]) -> usize {
    let mut changed = 0;
    for s in stubs {
        let mut touched = false;
        for (k, m) in s.request.query.iter_mut() {
            if r.sensitive_key(k) && m.equal_to.is_some() {
                *m = StringMatcher {
                    matches: Some(".*".into()),
                    ..Default::default()
                };
                touched = true;
            }
        }
        s.request.body.retain_mut(|b| {
            if let Some(v) = &mut b.equal_to_json
                && r.strip_sensitive(v)
            {
                b.ignore_extra_elements = true;
                touched = true;
            }
            if let Some(t) = &b.equal_to
                && (r.mask_text(t) != *t || r.query(t) != *t)
            {
                touched = true;
                return false;
            }
            true
        });
        let before = s.response.headers.len();
        s.response.headers.retain(|k, _| !r.sensitive_header(k));
        touched |= before != s.response.headers.len();
        if touched {
            changed += 1;
        }
    }
    changed
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostmanImport {
    /// Postman Collection v2.1 (or v2.0) as a JSON object or JSON text.
    #[schema(value_type = Object)]
    pub collection: Value,
    #[serde(default = "postman_tag")]
    pub tag: String,
    /// Delete stubs carrying `tag` first (default true).
    #[serde(default = "yes")]
    pub replace: bool,
    /// Only return the generated stubs.
    #[serde(default)]
    pub dry_run: bool,
}

fn postman_tag() -> String {
    "postman".into()
}

async fn run_postman(
    app: &AppState,
    p: &Principal,
    doc: &Value,
    t: ImportTarget<'_>,
) -> ApiResult<Json<ImportResult>> {
    let (tag, dry_run, space) = (t.tag, t.dry_run, t.space);
    valid_tag(tag)?;
    let mut c = fastmock_core::postman::convert(
        doc,
        &fastmock_core::postman::PostmanOptions { tag: tag.into() },
    )
    .map_err(bad)?;
    let n = redact_imported(&app.redactor, &mut c.stubs);
    if n > 0 {
        c.warnings.push(format!(
            "{n} stubs had secrets (sensitive query parameters, body fields or headers) removed from matchers"
        ));
    }
    let out = finish_import(app, p, t, c.stubs, c.warnings).await?;
    if !dry_run && space.is_none() {
        crate::services::record_service(
            app,
            &p.name,
            crate::services::ServiceInfo::generic("postman", tag),
        )
        .await;
    }
    Ok(out)
}

#[utoipa::path(
    post, path = "/__admin/import/postman", tag = "imports", operation_id = "importPostman",
    summary = "Generate stubs from a Postman collection",
    description = "Every saved example response becomes a stub: method, path (`:id` and unresolved `{{var}}` segments become path parameters), enabled query parameters and, when a request has several examples, its raw body. `{{var}}` is resolved from collection and folder variables; folders become tags. Requests without saved examples are skipped with a warning. Response bodies are not templated.",
    request_body(content = PostmanImport, example = json!({"collection": {"info": {"name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"}, "variable": [{"key": "baseUrl", "value": "https://api.example.com"}], "item": [{"name": "Get user", "request": {"method": "GET", "url": "{{baseUrl}}/users/:id"}, "response": [{"name": "ok", "code": 200, "header": [{"key": "Content-Type", "value": "application/json"}], "body": "{\"id\": 1}"}]}]}, "tag": "shop"})),
    responses(
        (status = 200, description = "Import result", body = ImportResult),
        (status = 400, description = "Not a Postman v2 collection, invalid tag or invalid stub", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_postman(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<PostmanImport>,
) -> ApiResult<Json<ImportResult>> {
    let space = spaces::target(&app, &p, Permission::ImportsRun, q.space.as_deref())?;
    let doc = document(b.collection, "collection")?;
    run_postman(
        &app,
        &p,
        &doc,
        ImportTarget {
            format: "postman",
            tag: &b.tag,
            replace: b.replace,
            dry_run: b.dry_run,
            space: space.as_deref(),
        },
    )
    .await
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HarImport {
    /// HAR 1.2 document as a JSON object or JSON text.
    #[schema(value_type = Object)]
    pub har: Value,
    #[serde(default = "har_tag")]
    pub tag: String,
    #[serde(default = "yes")]
    pub replace: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Keep only entries whose response MIME type starts with one of these, e.g. `application/json`.
    #[serde(default)]
    pub mime_types: Vec<String>,
    /// Keep only entries for these hosts.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Response headers to keep besides content-type (sensitive ones are always dropped).
    #[serde(default)]
    pub capture_headers: Vec<String>,
}

fn har_tag() -> String {
    "har".into()
}

#[derive(Default)]
struct HarFilters {
    mime_types: Vec<String>,
    hosts: Vec<String>,
    capture_headers: Vec<String>,
}

async fn run_har(
    app: &AppState,
    p: &Principal,
    doc: &Value,
    t: ImportTarget<'_>,
    f: HarFilters,
) -> ApiResult<Json<ImportResult>> {
    valid_tag(t.tag)?;
    if f.mime_types.len() > 32 || f.hosts.len() > 256 || f.capture_headers.len() > 32 {
        return Err(bad("at most 32 mimeTypes, 256 hosts and 32 captureHeaders"));
    }
    let opts = fastmock_core::har::HarOptions {
        tag: t.tag.into(),
        mime_types: f.mime_types,
        hosts: f.hosts,
        capture_headers: f.capture_headers,
    };
    let mut c = fastmock_core::har::convert(doc, &opts).map_err(bad)?;
    let n = redact_imported(&app.redactor, &mut c.stubs);
    if n > 0 {
        c.warnings.push(format!(
            "{n} stubs had secrets (sensitive query parameters, body fields or headers) removed from matchers"
        ));
    }
    let (tag, dry_run, space) = (t.tag.to_string(), t.dry_run, t.space);
    let out = finish_import(app, p, t, c.stubs, c.warnings).await?;
    if !dry_run && space.is_none() {
        crate::services::record_service(
            app,
            &p.name,
            crate::services::ServiceInfo::generic("har", &tag),
        )
        .await;
    }
    Ok(out)
}

#[utoipa::path(
    post, path = "/__admin/import/har", tag = "imports", operation_id = "importHar",
    summary = "Generate stubs from a HAR 1.2 capture",
    description = "Each distinct request (method, path, query, body) becomes a stub replaying the first recorded response. Failed entries are skipped; sensitive headers, query parameters and body fields never reach the stubs.",
    request_body(content = HarImport, example = json!({"har": {"log": {"version": "1.2", "entries": [{"request": {"method": "GET", "url": "https://api.example.com/users/7"}, "response": {"status": 200, "headers": [], "content": {"mimeType": "application/json", "text": "{\"id\":7}"}}}]}}, "tag": "browser", "mimeTypes": ["application/json"]})),
    responses(
        (status = 200, description = "Import result", body = ImportResult),
        (status = 400, description = "Not a HAR document, invalid filters or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_har(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<HarImport>,
) -> ApiResult<Json<ImportResult>> {
    let space = spaces::target(&app, &p, Permission::ImportsRun, q.space.as_deref())?;
    let doc = document(b.har, "har")?;
    run_har(
        &app,
        &p,
        &doc,
        ImportTarget {
            format: "har",
            tag: &b.tag,
            replace: b.replace,
            dry_run: b.dry_run,
            space: space.as_deref(),
        },
        HarFilters {
            mime_types: b.mime_types,
            hosts: b.hosts,
            capture_headers: b.capture_headers,
        },
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    OpenApi,
    Swagger,
    WireMock,
    MockServer,
    Postman,
    Har,
    AsyncApi,
    GraphQl,
    Wsdl,
    SoapUi,
    Microcks,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::OpenApi => "openapi",
            Format::Swagger => "swagger",
            Format::WireMock => "wiremock",
            Format::MockServer => "mockserver",
            Format::Postman => "postman",
            Format::Har => "har",
            Format::AsyncApi => "asyncapi",
            Format::GraphQl => "graphql",
            Format::Wsdl => "wsdl",
            Format::SoapUi => "soapui",
            Format::Microcks => "microcks",
        }
    }
}

pub enum Parsed {
    Doc(Value),
    Text(String),
}

fn looks_like_wiremock_mapping(v: &Value) -> bool {
    v.get("request").is_some_and(Value::is_object)
        && v.get("response").is_some_and(Value::is_object)
}

fn looks_like_mockserver(v: &Value) -> bool {
    v.get("httpRequest").is_some()
        && (v.get("httpResponse").is_some()
            || v.get("httpForward").is_some()
            || v.get("httpOverrideForwardedRequest").is_some()
            || v.get("httpError").is_some()
            || v.get("httpResponseTemplate").is_some()
            || v.get("httpForwardTemplate").is_some())
}

pub fn detect_doc(v: &Value) -> Option<Format> {
    if let Some(o) = v.as_object() {
        if o.get("openapi")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with('3'))
        {
            return Some(Format::OpenApi);
        }
        if o.contains_key("swagger") {
            return Some(Format::Swagger);
        }
        if o.contains_key("asyncapi") {
            return Some(Format::AsyncApi);
        }
        if o.get("apiVersion")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("mocks.microcks.io"))
            || o.get("kind").and_then(Value::as_str) == Some("APIExamples")
        {
            return Some(Format::Microcks);
        }
        if fastmock_core::postman::is_collection(v) {
            return Some(Format::Postman);
        }
        if fastmock_core::har::is_har(v) {
            return Some(Format::Har);
        }
        if o.get("mappings")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().all(looks_like_wiremock_mapping))
        {
            return Some(Format::WireMock);
        }
        if looks_like_mockserver(v)
            || o.get("expectations")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty() && a.iter().all(looks_like_mockserver))
        {
            return Some(Format::MockServer);
        }
        if looks_like_wiremock_mapping(v) {
            return Some(Format::WireMock);
        }
    }
    if let Some(a) = v.as_array().filter(|a| !a.is_empty()) {
        if a.iter().all(looks_like_mockserver) {
            return Some(Format::MockServer);
        }
        if a.iter().all(looks_like_wiremock_mapping) {
            return Some(Format::WireMock);
        }
    }
    None
}

pub fn detect_text(text: &str) -> Option<Format> {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('<') {
        let head: String = t.chars().take(4096).collect();
        if head.contains("soapui-project") {
            return Some(Format::SoapUi);
        }
        if head.contains("schemas.xmlsoap.org/wsdl")
            || head.contains("www.w3.org/ns/wsdl")
            || head.contains(":definitions")
            || head.contains("<definitions")
        {
            return Some(Format::Wsdl);
        }
        return None;
    }
    let sdl_markers = [
        "type Query",
        "type Mutation",
        "schema {",
        "schema{",
        "type Subscription",
        "extend type ",
    ];
    if sdl_markers.iter().any(|m| t.contains(m)) && !t.starts_with('{') {
        return Some(Format::GraphQl);
    }
    None
}

pub fn parse_body(body: &[u8]) -> ApiResult<(Parsed, Option<Format>)> {
    let text = std::str::from_utf8(body).map_err(|_| bad("the document must be UTF-8 text"))?;
    if text.trim().is_empty() {
        return Err(bad("empty document"));
    }
    if let Some(f) = detect_text(text) {
        return Ok((Parsed::Text(text.to_string()), Some(f)));
    }
    match fastmock_core::openapi::parse_document(text) {
        Ok(v) if v.is_object() || v.is_array() => {
            let f = detect_doc(&v);
            Ok((Parsed::Doc(v), f))
        }
        _ => Ok((Parsed::Text(text.to_string()), None)),
    }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
pub struct AutoImportQuery {
    /// Tag for the generated stubs (default: the detected format name; for GraphQL, WSDL, SoapUI and Microcks the service id).
    tag: Option<String>,
    /// Delete stubs carrying the tag first (default true).
    replace: Option<bool>,
    /// Only return the generated stubs.
    dry_run: Option<bool>,
    /// Force a format instead of detecting it.
    format: Option<String>,
    /// Space the stubs go to (OpenAPI, WireMock, MockServer, Postman and HAR only).
    space: Option<String>,
}

fn unsupported(format: Format, hint: &str) -> ApiError {
    ApiError(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        format!(
            "detected a {} document, which this server cannot import here{hint}",
            format.name()
        ),
    )
}

#[utoipa::path(
    post, path = "/__admin/import", tag = "imports", operation_id = "importAuto",
    summary = "Import any supported document, detecting its format",
    description = "Send the document itself as the body (JSON, YAML, XML or GraphQL SDL). Detected: OpenAPI 3, WireMock mappings, MockServer expectations, Postman collections, HAR, AsyncAPI, GraphQL SDL, WSDL, SoapUI projects and Microcks APIExamples. A format without a converter in this build answers 415. Options go into the query string.",
    params(AutoImportQuery),
    request_body(description = "The document itself: JSON, YAML, XML or GraphQL SDL. The content type is not needed for detection.",
        content(
            (String = "application/json"),
            (String = "application/yaml"),
            (String = "application/xml"),
            (String = "text/xml"),
            (String = "application/graphql"),
            (String = "text/plain")
        )
    ),
    responses(
        (status = 200, description = "Import result; AsyncAPI documents answer like `POST /import/asyncapi`", body = AutoImportResult),
        (status = 400, description = "Invalid document, tag or option", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
        (status = 415, description = "Unknown format, or no converter for the detected format", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_auto(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<AutoImportQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    let (parsed, detected) = parse_body(&body)?;
    let format = match q.format.as_deref() {
        Some(f) => [
            Format::OpenApi,
            Format::WireMock,
            Format::MockServer,
            Format::Postman,
            Format::Har,
            Format::AsyncApi,
            Format::GraphQl,
            Format::Wsdl,
            Format::SoapUi,
            Format::Microcks,
        ]
        .into_iter()
        .find(|x| x.name() == f)
        .ok_or_else(|| bad(format!("unknown format `{f}`")))?,
        None => detected.ok_or_else(|| {
            ApiError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "could not detect the document format; supported: OpenAPI 3, WireMock, MockServer, Postman v2, HAR 1.2, AsyncAPI, GraphQL SDL, WSDL, SoapUI, Microcks APIExamples".into(),
            )
        })?,
    };
    metrics::counter!("fastmock_import_detections_total", "format" => format.name()).increment(1);
    let replace = q.replace.unwrap_or(true);
    let dry_run = q.dry_run.unwrap_or(false);
    if let Some(t) = &q.tag {
        valid_tag(t)?;
    }
    let global_only = matches!(
        format,
        Format::GraphQl | Format::Wsdl | Format::SoapUi | Format::Microcks | Format::AsyncApi
    );
    let space = if global_only {
        if q.space.is_some() {
            return Err(bad(format!(
                "{} documents import into the global area only; remove space",
                format.name()
            )));
        }
        require(&p, Permission::ImportsRun, None)?;
        None
    } else {
        spaces::target(&app, &p, Permission::ImportsRun, q.space.as_deref())?
    };
    if matches!(
        format,
        Format::GraphQl | Format::Wsdl | Format::SoapUi | Format::Microcks
    ) {
        let text =
            std::str::from_utf8(&body).map_err(|_| bad("the document must be UTF-8 text"))?;
        let r = crate::services::import_text(
            &app,
            &p,
            format.name(),
            text,
            q.tag.as_deref(),
            replace,
            dry_run,
        )
        .await?;
        return Ok(r.into_response());
    }
    let tag = q.tag.clone().unwrap_or_else(|| format.name().to_string());
    let target = ImportTarget {
        format: format.name(),
        tag: &tag,
        replace,
        dry_run,
        space: space.as_deref(),
    };
    let doc = match &parsed {
        Parsed::Doc(v) => Some(v),
        Parsed::Text(_) => None,
    };
    let need_doc =
        || doc.ok_or_else(|| bad(format!("a {} document must be JSON or YAML", format.name())));
    let record = space.is_none();
    let generic = |app: AppState, p: Principal, tag: String, format: &'static str| async move {
        if !dry_run && record {
            crate::services::record_service(
                &app,
                &p.name,
                crate::services::ServiceInfo::generic(format, &tag),
            )
            .await;
        }
    };
    let r = match format {
        Format::OpenApi => {
            let doc = need_doc()?;
            let opts = fastmock_core::openapi::ImportOptions {
                tag: tag.clone(),
                validate_requests: true,
                path_prefix: String::new(),
            };
            let r = fastmock_core::openapi::import(doc, &opts).map_err(|e| bad(e.to_string()))?;
            let out = finish_import(&app, &p, target, r.stubs, r.warnings).await?;
            if !dry_run && record {
                crate::services::record_service(
                    &app,
                    &p.name,
                    crate::services::ServiceInfo::openapi(doc, &opts),
                )
                .await;
            }
            out
        }
        Format::WireMock | Format::MockServer => {
            let mut c = if format == Format::WireMock {
                fastmock_core::wiremock::convert(need_doc()?)
            } else {
                fastmock_core::mockserver::convert(need_doc()?)
            };
            for s in &mut c.stubs {
                if !s.tags.contains(&tag) {
                    s.tags.push(tag.clone());
                }
            }
            if format == Format::MockServer {
                let mut kept = Vec::with_capacity(c.stubs.len());
                for s in c.stubs {
                    match app.engine.validate(&s) {
                        Ok(()) => kept.push(s),
                        Err(e) => c.warnings.push(format!(
                            "expectation {} skipped: {e}",
                            s.name.clone().unwrap_or_else(|| s.id.to_string())
                        )),
                    }
                }
                c.stubs = kept;
            }
            let out = finish_import(&app, &p, target, c.stubs, c.warnings).await?;
            generic(app.clone(), p.clone(), tag.clone(), format.name()).await;
            out
        }
        Format::Postman => run_postman(&app, &p, need_doc()?, target).await?,
        Format::Har => run_har(&app, &p, need_doc()?, target, HarFilters::default()).await?,
        Format::AsyncApi => {
            let doc = need_doc()?.clone();
            let req = crate::asyncmock::ImportAsyncApiRequest {
                tag: q.tag.clone(),
                dry_run,
                ..Default::default()
            };
            let r = crate::asyncmock::import_document(&app, &p, doc.clone(), &req).await?;
            if !dry_run {
                crate::services::record_service(
                    &app,
                    &p.name,
                    crate::services::ServiceInfo::asyncapi(&doc, &r.tag),
                )
                .await;
            }
            return Ok(Json(r).into_response());
        }
        Format::Swagger => {
            return Err(unsupported(
                format,
                "; convert Swagger 2.0 to OpenAPI 3 first",
            ));
        }
        Format::GraphQl | Format::Wsdl | Format::SoapUi | Format::Microcks => {
            return Err(unsupported(format, ""));
        }
    };
    Ok(r.into_response())
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum AutoImportResult {
    Stubs(ImportResult),
    AsyncApi(crate::asyncmock::ImportAsyncApiResponse),
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(import_auto, import_postman, import_har))]
pub struct ImportsApi;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_formats() {
        let d = |v: Value| detect_doc(&v);
        assert_eq!(
            d(json!({"openapi": "3.0.3", "paths": {}})),
            Some(Format::OpenApi)
        );
        assert_eq!(d(json!({"swagger": "2.0"})), Some(Format::Swagger));
        assert_eq!(d(json!({"asyncapi": "2.6.0"})), Some(Format::AsyncApi));
        assert_eq!(
            d(json!({"apiVersion": "mocks.microcks.io/v1alpha1", "kind": "APIExamples"})),
            Some(Format::Microcks)
        );
        assert_eq!(
            d(
                json!({"info": {"schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"}, "item": []})
            ),
            Some(Format::Postman)
        );
        assert_eq!(d(json!({"log": {"entries": []}})), Some(Format::Har));
        assert_eq!(
            d(json!({"mappings": [{"request": {"url": "/x"}, "response": {"status": 200}}]})),
            Some(Format::WireMock)
        );
        assert_eq!(
            d(json!({"request": {"url": "/x"}, "response": {}})),
            Some(Format::WireMock)
        );
        assert_eq!(
            d(json!([{"httpRequest": {"path": "/x"}, "httpResponse": {"statusCode": 200}}])),
            Some(Format::MockServer)
        );
        assert_eq!(
            d(json!({"httpRequest": {"path": "/x"}, "httpResponse": {}})),
            Some(Format::MockServer)
        );
        assert_eq!(d(json!({"hello": "world"})), None);
        assert_eq!(detect_text("type Query { a: Int }"), Some(Format::GraphQl));
        assert_eq!(
            detect_text(
                "<?xml version=\"1.0\"?><con:soapui-project xmlns:con=\"http://eviware.com/soapui/config\"/>"
            ),
            Some(Format::SoapUi)
        );
        assert_eq!(
            detect_text("<definitions xmlns=\"http://schemas.xmlsoap.org/wsdl/\"/>"),
            Some(Format::Wsdl)
        );
        assert_eq!(detect_text("{\"a\": 1}"), None);
        let (p, f) =
            parse_body(b"openapi: 3.1.0\ninfo: {title: x, version: '1'}\npaths: {}\n").unwrap();
        assert!(matches!(p, Parsed::Doc(_)));
        assert_eq!(f, Some(Format::OpenApi));
        assert!(parse_body(b"   ").is_err());
        assert!(parse_body(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn redaction_of_imported_stubs() {
        let r = crate::redact::Redactor::new(
            &["set-cookie".to_string(), "authorization".to_string()],
            &["password".to_string(), "api_key".to_string()],
        );
        let mut stubs: Vec<Stub> = vec![serde_json::from_value(json!({
            "request": {"urlPath": "/x", "query": {"api_key": {"equalTo": "s3cret"}, "page": {"equalTo": "1"}},
                        "body": [{"equalToJson": {"user": "a", "password": "p"}}, {"equalTo": "password=p"}]},
            "response": {"headers": {"Set-Cookie": "sid=1", "Content-Type": "text/plain"}}
        }))
        .unwrap()];
        assert_eq!(redact_imported(&r, &mut stubs), 1);
        let s = &stubs[0];
        assert_eq!(s.request.query["api_key"].matches.as_deref(), Some(".*"));
        assert_eq!(s.request.query["page"].equal_to.as_deref(), Some("1"));
        assert_eq!(s.request.body.len(), 1);
        assert_eq!(s.request.body[0].equal_to_json, Some(json!({"user": "a"})));
        assert!(s.request.body[0].ignore_extra_elements);
        assert_eq!(s.response.headers.len(), 1);
    }
}
