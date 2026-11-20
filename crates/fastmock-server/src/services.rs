use std::collections::BTreeMap;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fastmock_core::Stub;
use fastmock_core::microcks::{self, ApiKind, Artifact, ExampleTarget, OperationMeta};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use crate::admin::{
    ApiError, ApiResult, ErrorBody, ImportResult, ImportTarget, audit_event, bad, finish_import,
    refresh, valid_tag,
};
use crate::auth::Principal;
use crate::state::AppState;

pub const NAMESPACE: &str = "services";
pub const SPECS_NAMESPACE: &str = "specs";
pub const MAX_SPEC_BYTES: usize = 8 * 1024 * 1024;
const MAX_SERVICES: i64 = 1000;
const EXAMPLES_SUFFIX: &str = ":examples";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceOperation {
    #[schema(example = "GET /pastry/{name}")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub stub_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceOverrides {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, OperationMeta>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionArtifact {
    #[schema(example = "APIExamples")]
    pub kind: String,
    pub imported_at: DateTime<Utc>,
    pub imported_by: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    #[schema(example = "api-pastry-2.0:2.0.0")]
    pub id: String,
    #[schema(example = "API Pastry - 2.0")]
    pub name: String,
    #[schema(example = "2.0.0")]
    pub version: String,
    #[serde(rename = "type")]
    pub kind: ApiKind,
    #[schema(example = "openapi")]
    pub source_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub operations: Vec<ServiceOperation>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub imported_at: DateTime<Utc>,
    pub imported_by: String,
    #[serde(default)]
    pub has_spec: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub import_options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ServiceOverrides>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<CompanionArtifact>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ServiceList {
    pub services: Vec<Service>,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct StoredSpec {
    pub name: String,
    pub version: String,
    #[serde(rename = "type")]
    pub kind: ApiKind,
    #[schema(value_type = Object)]
    pub document: Value,
}

#[derive(Clone, Debug)]
pub struct ServiceInfo {
    pub name: String,
    pub version: String,
    pub kind: ApiKind,
    pub format: String,
    pub tag: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub document: Option<Value>,
    pub import_options: Option<Value>,
    pub operations: Option<Vec<ServiceOperation>>,
}

impl ServiceInfo {
    pub fn new(name: &str, version: &str, kind: ApiKind, format: &str, tag: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            kind,
            format: format.to_string(),
            tag: tag.map(String::from),
            labels: BTreeMap::new(),
            document: None,
            import_options: None,
            operations: None,
        }
    }

    pub fn openapi(doc: &Value, opts: &fastmock_core::openapi::ImportOptions) -> Self {
        let info = fastmock_core::openapi::service_info(doc);
        let mut s = Self::new(
            if info.name.is_empty() {
                &opts.tag
            } else {
                &info.name
            },
            if info.version.is_empty() {
                "1.0"
            } else {
                &info.version
            },
            ApiKind::Rest,
            "openapi",
            Some(&opts.tag),
        );
        s.labels = info.labels;
        s.document = Some(doc.clone());
        s.import_options = Some(json!({
            "validateRequests": opts.validate_requests,
            "pathPrefix": opts.path_prefix,
        }));
        s
    }

    pub fn asyncapi(doc: &Value, tag: &str) -> Self {
        let text = |k: &str| {
            doc.get("info")
                .and_then(|i| i.get(k))
                .map(fastmock_core::matcher::value_text)
                .filter(|s| !s.is_empty())
        };
        let mut s = Self::new(
            &text("title").unwrap_or_else(|| tag.to_string()),
            &text("version").unwrap_or_else(|| "1.0".into()),
            ApiKind::Event,
            "asyncapi",
            Some(tag),
        );
        s.document = Some(doc.clone());
        s.operations = Some(asyncapi_operations(doc));
        s
    }

    pub fn generic(format: &str, tag: &str) -> Self {
        Self::new(tag, "1.0", ApiKind::Generic, format, Some(tag))
    }

    pub fn grpc(name: &str, descriptor_set: &[u8]) -> Self {
        let mut s = Self::new(name, "1.0", ApiKind::Grpc, "grpc-descriptor", None);
        let mut pool = prost_reflect::DescriptorPool::new();
        if pool.decode_file_descriptor_set(descriptor_set).is_ok() {
            s.operations = Some(
                pool.services()
                    .flat_map(|svc| {
                        svc.methods()
                            .map(|m| ServiceOperation {
                                name: format!("{}/{}", svc.full_name(), m.name()),
                                method: Some("POST".into()),
                                path: Some(format!("/{}/{}", svc.full_name(), m.name())),
                                stub_ids: vec![],
                            })
                            .collect::<Vec<_>>()
                    })
                    .take(10_000)
                    .collect(),
            );
        }
        s
    }

    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels.extend(labels);
        self
    }
}

fn asyncapi_operations(doc: &Value) -> Vec<ServiceOperation> {
    let mut ops = Vec::new();
    let channels = doc.get("channels").and_then(|c| c.as_object());
    if let Some(operations) = doc.get("operations").and_then(|o| o.as_object()) {
        for (id, op) in operations.iter().take(10_000) {
            let action = op.get("action").and_then(|a| a.as_str()).unwrap_or("send");
            let channel = op
                .get("channel")
                .and_then(|c| c.get("$ref"))
                .and_then(|r| r.as_str())
                .and_then(|r| r.rsplit('/').next())
                .map(|key| {
                    channels
                        .and_then(|c| c.get(key))
                        .and_then(|c| c.get("address"))
                        .and_then(|a| a.as_str())
                        .unwrap_or(key)
                        .to_string()
                })
                .unwrap_or_else(|| id.clone());
            ops.push(ServiceOperation {
                name: format!("{} {channel}", action.to_ascii_uppercase()),
                method: None,
                path: Some(channel),
                stub_ids: vec![],
            });
        }
    } else if let Some(channels) = channels {
        for (name, ch) in channels.iter().take(10_000) {
            for action in ["publish", "subscribe"] {
                if ch.get(action).is_some() {
                    ops.push(ServiceOperation {
                        name: format!("{} {name}", action.to_ascii_uppercase()),
                        method: None,
                        path: Some(name.clone()),
                        stub_ids: vec![],
                    });
                }
            }
        }
    }
    ops
}

pub fn service_id(name: &str, version: &str) -> String {
    let slug = |s: &str| {
        let mut out = String::new();
        for c in s.trim().to_lowercase().chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                out.push(c);
            } else if !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches('-').to_string()
    };
    let (n, v) = (slug(name), slug(version));
    let mut id = format!(
        "{}:{}",
        if n.is_empty() { "service" } else { &n },
        if v.is_empty() { "0" } else { &v }
    );
    if id.len() > 100 {
        let digest = hex::encode(sha2::Sha256::digest(
            format!("{name}\n{version}").as_bytes(),
        ));
        id.truncate(88);
        while id.ends_with('-') {
            id.pop();
        }
        id.push('-');
        id.push_str(&digest[..10]);
    }
    id
}

fn examples_tag(tag: &str) -> String {
    format!("{tag}{EXAMPLES_SUFFIX}")
}

fn group_operations(stubs: &[Stub], tag: &str) -> Vec<ServiceOperation> {
    let ex_tag = examples_tag(tag);
    let mut ops: Vec<ServiceOperation> = Vec::new();
    for s in stubs
        .iter()
        .filter(|s| s.tags.iter().any(|t| *t == tag || *t == ex_tag))
    {
        let name = microcks::stub_operation(s);
        let r = &s.request;
        let path = r
            .url_path
            .clone()
            .or(r.url_path_template.clone())
            .or(r.url_path_pattern.clone());
        match ops.iter_mut().find(|o| o.name == name) {
            Some(o) => o.stub_ids.push(s.id),
            None => ops.push(ServiceOperation {
                name,
                method: r.method.clone(),
                path,
                stub_ids: vec![s.id],
            }),
        }
    }
    ops
}

pub async fn load(app: &AppState, id: &str) -> anyhow::Result<Option<Service>> {
    Ok(app
        .stores
        .data
        .get(NAMESPACE, id)
        .await?
        .and_then(|v| serde_json::from_value(v).ok()))
}

async fn save(app: &AppState, s: &Service) -> anyhow::Result<()> {
    app.stores
        .data
        .put(NAMESPACE, &s.id, &serde_json::to_value(s)?)
        .await
}

async fn reimport_with_overrides(
    app: &AppState,
    info: &ServiceInfo,
    overrides: &ServiceOverrides,
) -> anyhow::Result<()> {
    let (Some(doc), Some(tag), Some(opts)) = (&info.document, &info.tag, &info.import_options)
    else {
        return Ok(());
    };
    if overrides.operations.is_empty() || info.format != "openapi" {
        return Ok(());
    }
    let r = fastmock_core::openapi::import_with(
        doc,
        &fastmock_core::openapi::ImportOptions {
            tag: tag.clone(),
            validate_requests: opts
                .get("validateRequests")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            path_prefix: opts
                .get("pathPrefix")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        &fastmock_core::openapi::Overrides {
            operations: overrides.operations.clone(),
        },
    )?;
    for s in &r.stubs {
        app.engine
            .validate(s)
            .map_err(|e| anyhow::anyhow!("re-applying APIMetadata: {e}"))?;
    }
    app.stores
        .stubs
        .replace_tag(Some(tag), &fastmock_core::SpaceFilter::Global, &r.stubs)
        .await?;
    app.engine.reload().await?;
    Ok(())
}

pub async fn record_service(
    app: &AppState,
    imported_by: &str,
    info: ServiceInfo,
) -> Option<Service> {
    match record_inner(app, imported_by, info).await {
        Ok(s) => Some(s),
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "services").increment(1);
            tracing::error!(error = %e, "recording the imported service failed");
            None
        }
    }
}

#[tracing::instrument(name = "services.record", skip_all, fields(format = %info.format))]
async fn record_inner(
    app: &AppState,
    imported_by: &str,
    info: ServiceInfo,
) -> anyhow::Result<Service> {
    let id = service_id(&info.name, &info.version);
    let old = load(app, &id).await?;
    if let (Some(old_tag), Some(new_tag)) = (old.as_ref().and_then(|o| o.tag.clone()), &info.tag)
        && old_tag != *new_tag
    {
        app.stores
            .stubs
            .delete_by_tag(&old_tag, &fastmock_core::SpaceFilter::Global)
            .await?;
        app.stores
            .stubs
            .delete_by_tag(&examples_tag(&old_tag), &fastmock_core::SpaceFilter::Global)
            .await?;
        app.engine.reload().await?;
    }
    let overrides = old.as_ref().and_then(|o| o.overrides.clone());
    if let Some(ov) = &overrides {
        reimport_with_overrides(app, &info, ov).await?;
    }
    let operations = match (&info.operations, &info.tag) {
        (Some(ops), _) => ops.clone(),
        (None, Some(tag)) => group_operations(&app.stores.stubs.list().await?, tag),
        (None, None) => vec![],
    };
    let mut has_spec = false;
    if let Some(doc) = &info.document {
        let size = serde_json::to_vec(doc)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size <= MAX_SPEC_BYTES {
            let spec = StoredSpec {
                name: info.name.clone(),
                version: info.version.clone(),
                kind: info.kind,
                document: doc.clone(),
            };
            app.stores
                .data
                .put(SPECS_NAMESPACE, &id, &serde_json::to_value(&spec)?)
                .await?;
            has_spec = true;
        } else {
            tracing::warn!(service = %id, size, "spec not stored: larger than the limit");
        }
    }
    if !has_spec && old.as_ref().is_some_and(|o| o.has_spec) {
        app.stores.data.delete(SPECS_NAMESPACE, &id).await?;
    }
    let mut labels = info.labels.clone();
    if let Some(ov) = &overrides {
        labels.extend(ov.labels.clone());
    }
    let svc = Service {
        id,
        name: info.name.clone(),
        version: info.version.clone(),
        kind: info.kind,
        source_format: info.format.clone(),
        tag: info.tag.clone(),
        operations,
        labels,
        imported_at: Utc::now(),
        imported_by: imported_by.to_string(),
        has_spec,
        import_options: info.import_options.clone(),
        overrides,
        artifacts: old.map(|o| o.artifacts).unwrap_or_default(),
    };
    save(app, &svc).await?;
    metrics::counter!("fastmock_services_recorded_total", "type" => info.kind.as_str())
        .increment(1);
    Ok(svc)
}

async fn regroup(app: &AppState, svc: &mut Service) -> anyhow::Result<()> {
    if let Some(tag) = &svc.tag {
        let fresh = group_operations(&app.stores.stubs.list().await?, tag);
        for op in &mut svc.operations {
            op.stub_ids.clear();
        }
        for f in fresh {
            match svc.operations.iter_mut().find(|o| o.name == f.name) {
                Some(o) => o.stub_ids = f.stub_ids,
                None => svc.operations.push(f),
            }
        }
    }
    save(app, svc).await
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ServiceQuery {
    /// Only services of this type (REST, SOAP, GRAPHQL, EVENT, GRPC, GENERIC).
    #[serde(rename = "type")]
    #[param(rename = "type")]
    kind: Option<String>,
    /// Only services carrying this label, as `key=value` or `key`.
    label: Option<String>,
    /// Case-insensitive substring of the service name.
    q: Option<String>,
}

#[utoipa::path(
    get, path = "/__admin/services", tag = "services", operation_id = "listServices",
    summary = "List imported APIs and services",
    params(ServiceQuery),
    responses(
        (status = 200, description = "Services, most recently imported first", body = ServiceList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_services(
    State(app): State<AppState>,
    Query(q): Query<ServiceQuery>,
) -> ApiResult<Json<ServiceList>> {
    let items = app.stores.data.list(NAMESPACE, MAX_SERVICES).await?;
    let mut services: Vec<Service> = items
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value(v).ok())
        .collect();
    if let Some(k) = &q.kind {
        services.retain(|s| s.kind.as_str().eq_ignore_ascii_case(k));
    }
    if let Some(l) = &q.label {
        let (k, v) = match l.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (l.as_str(), None),
        };
        services.retain(|s| {
            s.labels
                .get(k)
                .is_some_and(|x| v.is_none_or(|v| x.as_str() == v))
        });
    }
    if let Some(n) = &q.q {
        let n = n.to_lowercase();
        services.retain(|s| s.name.to_lowercase().contains(&n));
    }
    services.sort_by_key(|s| std::cmp::Reverse(s.imported_at));
    Ok(Json(ServiceList { services }))
}

#[utoipa::path(
    get, path = "/__admin/services/{id}", tag = "services", operation_id = "getService",
    summary = "One service with its operations and stub ids",
    params(("id" = String, Path, description = "Service id (`<name>:<version>` slug)")),
    responses(
        (status = 200, description = "The service", body = Service),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Unknown service", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_service(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Service>> {
    load(&app, &id)
        .await?
        .map(Json)
        .ok_or_else(|| crate::admin::not_found("service"))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeletedService {
    pub id: String,
    pub deleted_stubs: u64,
}

#[utoipa::path(
    delete, path = "/__admin/services/{id}", tag = "services", operation_id = "deleteService",
    summary = "Delete a service, its stubs and its stored document",
    params(("id" = String, Path, description = "Service id")),
    responses(
        (status = 200, description = "Deleted", body = DeletedService),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "Unknown service", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_service(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<DeletedService>> {
    let svc = load(&app, &id)
        .await?
        .ok_or_else(|| crate::admin::not_found("service"))?;
    let mut deleted = 0;
    if let Some(tag) = &svc.tag {
        deleted += app
            .stores
            .stubs
            .delete_by_tag(tag, &fastmock_core::SpaceFilter::Global)
            .await?;
        deleted += app
            .stores
            .stubs
            .delete_by_tag(&examples_tag(tag), &fastmock_core::SpaceFilter::Global)
            .await?;
    }
    app.stores.data.delete(SPECS_NAMESPACE, &id).await?;
    app.stores.data.delete(NAMESPACE, &id).await?;
    refresh(&app).await?;
    audit_event(
        &app,
        &p,
        "service.delete",
        json!({"id": id, "deletedStubs": deleted}),
    )
    .await;
    Ok(Json(DeletedService {
        id,
        deleted_stubs: deleted,
    }))
}

#[utoipa::path(
    get, path = "/__admin/services/{id}/spec", tag = "services", operation_id = "getServiceSpec",
    summary = "The OpenAPI or AsyncAPI document the service was imported from",
    params(("id" = String, Path, description = "Service id")),
    responses(
        (status = 200, description = "Stored document", body = StoredSpec),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Unknown service or no stored document", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_spec(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    app.stores
        .data
        .get(SPECS_NAMESPACE, &id)
        .await?
        .map(Json)
        .ok_or_else(|| crate::admin::not_found("spec"))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactImportResult {
    #[serde(flatten)]
    pub result: ImportResult,
    /// Catalog id of the imported or updated service (absent on a dry run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
}

fn retag(stubs: &mut [Stub], tag: &str) {
    for s in stubs {
        s.tags.retain(|t| !t.is_empty());
        if !s.tags.iter().any(|t| t == tag) {
            s.tags.push(tag.to_string());
        }
    }
}

struct Converted {
    name: String,
    version: String,
    kind: ApiKind,
    stubs: Vec<Stub>,
    warnings: Vec<String>,
}

async fn store_converted(
    app: &AppState,
    p: &Principal,
    format: &str,
    tag: Option<&str>,
    replace: bool,
    dry_run: bool,
    mut c: Converted,
) -> ApiResult<Json<ArtifactImportResult>> {
    let id = service_id(&c.name, &c.version);
    let tag = tag.map(String::from).unwrap_or_else(|| id.clone());
    valid_tag(&tag)?;
    retag(&mut c.stubs, &tag);
    let res = finish_import(
        app,
        p,
        ImportTarget {
            format,
            tag: &tag,
            replace,
            dry_run,
            space: None,
        },
        c.stubs,
        c.warnings,
    )
    .await?;
    if dry_run {
        return Ok(Json(ArtifactImportResult {
            result: res.0,
            service_id: None,
        }));
    }
    let svc = record_service(
        app,
        &p.name,
        ServiceInfo::new(&c.name, &c.version, c.kind, format, Some(&tag)),
    )
    .await;
    Ok(Json(ArtifactImportResult {
        result: res.0,
        service_id: svc.map(|s| s.id),
    }))
}

fn count(format: &'static str, ok: bool) {
    metrics::counter!("fastmock_artifact_imports_total", "format" => format, "outcome" => if ok { "ok" } else { "error" })
        .increment(1);
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphQlImport {
    /// GraphQL SDL. A `# microcksId: <name> : <version>` comment names the service.
    pub schema: String,
    /// Optional Microcks APIExamples (YAML/JSON text or object) with examples per query or mutation.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub examples: Option<Value>,
    /// Tag for the generated stubs (default: the service id).
    #[serde(default)]
    pub tag: Option<String>,
    /// Endpoint path (default `/graphql`).
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default = "crate::admin::yes")]
    pub replace: bool,
    #[serde(default)]
    pub dry_run: bool,
}

fn parse_examples(v: &Value) -> ApiResult<microcks::ApiExamples> {
    let art = match v {
        Value::String(s) => microcks::parse_artifact(s),
        other => microcks::parse_artifact_value(other),
    }
    .map_err(|e| bad(format!("examples: {e}")))?;
    match art {
        Artifact::Examples(e) => Ok(e),
        Artifact::Metadata(_) => Err(bad("examples must be an APIExamples document")),
    }
}

#[utoipa::path(
    post, path = "/__admin/import/graphql", tag = "imports", operation_id = "importGraphQl",
    summary = "Generate stubs from a GraphQL schema (SDL)",
    description = "One stub per Query and Mutation field, matching requests whose root field is that field and answering `{\"data\": {<field>: <example>}}`. Examples from a Microcks APIExamples document become higher-priority stubs matching the request variables.",
    request_body(content = GraphQlImport, example = json!({"schema": "# microcksId: Movie Graph API : 1.0\ntype Query { film(id: ID!): Film }\ntype Film { id: ID! title: String }", "path": "/graphql"})),
    responses(
        (status = 200, description = "Import result", body = ArtifactImportResult),
        (status = 400, description = "Invalid schema, examples or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_graphql(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<GraphQlImport>,
) -> ApiResult<Json<ArtifactImportResult>> {
    let examples = b.examples.as_ref().map(parse_examples).transpose()?;
    run_graphql(&app, &p, &b.schema, examples.as_ref(), &b).await
}

#[tracing::instrument(name = "import.graphql", skip_all)]
async fn run_graphql(
    app: &AppState,
    p: &Principal,
    schema: &str,
    examples: Option<&microcks::ApiExamples>,
    b: &GraphQlImport,
) -> ApiResult<Json<ArtifactImportResult>> {
    if let Some(path) = &b.path
        && !path.starts_with('/')
    {
        return Err(bad("path must start with /"));
    }
    let r = fastmock_core::graphql::import(
        schema,
        &fastmock_core::graphql::ImportOptions {
            tag: String::new(),
            path: b.path.clone().unwrap_or_default(),
            name: b.name.clone(),
            version: b.version.clone(),
        },
        examples,
    )
    .map_err(|e| {
        count("graphql", false);
        bad(e.to_string())
    })?;
    count("graphql", true);
    store_converted(
        app,
        p,
        "graphql",
        b.tag.as_deref(),
        b.replace,
        b.dry_run,
        Converted {
            name: r.name,
            version: r.version,
            kind: ApiKind::GraphQl,
            stubs: r.stubs,
            warnings: r.warnings,
        },
    )
    .await
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SoapImport {
    /// The SoapUI project XML (`/import/soapui`) or the WSDL 1.1 document (`/import/wsdl`).
    pub document: String,
    /// Tag for the generated stubs (default: the service id).
    #[serde(default)]
    pub tag: Option<String>,
    /// Endpoint path; default: the mock service path (SoapUI) or the port address path (WSDL).
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// SoapUI only: which mock service to import when the project has several.
    #[serde(default)]
    pub mock_service: Option<String>,
    #[serde(default = "crate::admin::yes")]
    pub replace: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[tracing::instrument(name = "import.soap", skip_all, fields(format = format))]
async fn run_soap(
    app: &AppState,
    p: &Principal,
    format: &'static str,
    b: &SoapImport,
) -> ApiResult<Json<ArtifactImportResult>> {
    if let Some(path) = &b.path
        && !path.starts_with('/')
    {
        return Err(bad("path must start with /"));
    }
    let opts = fastmock_core::soap::ImportOptions {
        tag: String::new(),
        path: b.path.clone(),
        name: b.name.clone(),
        version: b.version.clone(),
        mock_service: b.mock_service.clone(),
    };
    let text = b.document.clone();
    let r = tokio::task::spawn_blocking(move || {
        if format == "soapui" {
            fastmock_core::soap::import_soapui(&text, &opts)
        } else {
            fastmock_core::soap::import_wsdl(&text, &opts)
        }
    })
    .await
    .map_err(|e| ApiError::from(anyhow::Error::from(e)))?
    .map_err(|e| {
        count(format, false);
        bad(e.to_string())
    })?;
    count(format, true);
    store_converted(
        app,
        p,
        format,
        b.tag.as_deref(),
        b.replace,
        b.dry_run,
        Converted {
            name: r.name,
            version: r.version,
            kind: ApiKind::Soap,
            stubs: r.stubs,
            warnings: r.warnings,
        },
    )
    .await
}

#[utoipa::path(
    post, path = "/__admin/import/soapui", tag = "imports", operation_id = "importSoapUi",
    summary = "Generate stubs from a SoapUI project (Microcks' SOAP format)",
    description = "Mock operations and responses become stubs matched on SOAPAction (SOAP 1.1) or the content-type action (SOAP 1.2) and the body root element. Dispatch styles: SEQUENCE and RANDOM become response lists, XPATH and QUERY_MATCH become XPath body matchers; SCRIPT is never executed (warning, default response).",
    request_body(content = SoapImport, example = json!({"document": "<con:soapui-project xmlns:con=\"http://eviware.com/soapui/config\" name=\"Hello\">...</con:soapui-project>"})),
    responses(
        (status = 200, description = "Import result", body = ArtifactImportResult),
        (status = 400, description = "Invalid project or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_soapui(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<SoapImport>,
) -> ApiResult<Json<ArtifactImportResult>> {
    run_soap(&app, &p, "soapui", &b).await
}

#[utoipa::path(
    post, path = "/__admin/import/wsdl", tag = "imports", operation_id = "importWsdl",
    summary = "Generate stubs from a WSDL 1.1 document",
    description = "One stub per operation and SOAP binding (1.1 and 1.2) answering an envelope generated from the XSD of the output message. Faults become disabled alternative stubs.",
    request_body(content = SoapImport, example = json!({"document": "<wsdl:definitions xmlns:wsdl=\"http://schemas.xmlsoap.org/wsdl/\">...</wsdl:definitions>", "version": "1.0"})),
    responses(
        (status = 200, description = "Import result", body = ArtifactImportResult),
        (status = 400, description = "Invalid WSDL or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_wsdl(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<SoapImport>,
) -> ApiResult<Json<ArtifactImportResult>> {
    run_soap(&app, &p, "wsdl", &b).await
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MicrocksImport {
    /// An APIExamples or APIMetadata document (YAML/JSON text or object).
    #[schema(value_type = Object)]
    pub document: Value,
    #[serde(default)]
    pub dry_run: bool,
}

fn find_operation<'a>(svc: &'a Service, key: &str) -> Option<&'a ServiceOperation> {
    svc.operations
        .iter()
        .find(|o| o.name == key)
        .or_else(|| {
            svc.operations
                .iter()
                .find(|o| o.name.eq_ignore_ascii_case(key))
        })
        .or_else(|| {
            svc.operations
                .iter()
                .find(|o| svc.kind == ApiKind::Grpc && o.name.rsplit('/').next() == Some(key))
        })
        .or_else(|| {
            let (m, path) = key.split_once(' ')?;
            svc.operations.iter().find(|o| {
                o.method
                    .as_deref()
                    .is_some_and(|x| x.eq_ignore_ascii_case(m))
                    && o.path.as_deref().is_some_and(|p| p.ends_with(path.trim()))
            })
        })
}

async fn apply_examples(
    app: &AppState,
    p: &Principal,
    mut svc: Service,
    ex: &microcks::ApiExamples,
    dry_run: bool,
) -> ApiResult<Json<ArtifactImportResult>> {
    let tag = svc.tag.clone().unwrap_or_else(|| svc.id.clone());
    let ex_tag = examples_tag(&tag);
    valid_tag(&ex_tag)?;
    let mut warnings = Vec::new();
    let mut stubs = Vec::new();
    let delays = svc
        .overrides
        .as_ref()
        .map(|o| o.operations.clone())
        .unwrap_or_default();
    for (key, examples) in &ex.operations {
        let Some(op) = find_operation(&svc, key) else {
            warnings.push(format!(
                "operation `{key}` is not part of the service; its examples were skipped"
            ));
            continue;
        };
        let mut base: Option<Stub> = None;
        for id in &op.stub_ids {
            if let Some(s) = app.stores.stubs.get(*id).await? {
                let better = base.as_ref().is_none_or(|b| s.priority > b.priority);
                if better {
                    base = Some(s);
                }
            }
        }
        let (request, content_type) = match &base {
            Some(b) => (
                microcks::operation_identity(b),
                b.response.headers.get("content-type").cloned(),
            ),
            None => match (&op.method, &op.path) {
                (Some(m), Some(path)) => (
                    fastmock_core::RequestPattern {
                        method: Some(m.clone()),
                        url_path: Some(path.clone()),
                        ..Default::default()
                    },
                    None,
                ),
                _ => {
                    warnings.push(format!(
                        "operation `{key}` has no stubs to attach examples to"
                    ));
                    continue;
                }
            },
        };
        if svc.kind == ApiKind::Event {
            warnings.push(format!(
                "`{key}`: event examples are not imported here; use the AsyncAPI import"
            ));
            continue;
        }
        let target = ExampleTarget {
            kind: svc.kind,
            operation: &op.name,
            request,
            content_type,
            priority: 3,
            tags: vec![ex_tag.clone()],
            delay_ms: delays
                .get(&op.name)
                .and_then(|m| m.delay)
                .map(|d| d.min(600_000)),
        };
        for e in examples {
            if let Some(s) = microcks::example_stub(&target, e, &mut warnings) {
                stubs.push(s);
            }
        }
    }
    let res = finish_import(
        app,
        p,
        ImportTarget {
            format: "microcks",
            tag: &ex_tag,
            replace: true,
            dry_run,
            space: None,
        },
        stubs,
        warnings,
    )
    .await?;
    if !dry_run {
        svc.artifacts.retain(|a| a.kind != "APIExamples");
        svc.artifacts.push(CompanionArtifact {
            kind: "APIExamples".into(),
            imported_at: Utc::now(),
            imported_by: p.name.clone(),
        });
        regroup(app, &mut svc).await?;
    }
    Ok(Json(ArtifactImportResult {
        result: res.0,
        service_id: (!dry_run).then_some(svc.id),
    }))
}

async fn apply_metadata(
    app: &AppState,
    p: &Principal,
    mut svc: Service,
    md: &microcks::ApiMetadata,
    dry_run: bool,
) -> ApiResult<Json<ArtifactImportResult>> {
    let mut warnings = Vec::new();
    let mut ov = svc.overrides.clone().unwrap_or_default();
    ov.labels.extend(md.labels.clone());
    for (key, meta) in &md.operations {
        match find_operation(&svc, key) {
            Some(op) => {
                ov.operations.insert(op.name.clone(), meta.clone());
            }
            None => warnings.push(format!(
                "operation `{key}` is not part of the service; ignored"
            )),
        }
    }
    let spec = app.stores.data.get(SPECS_NAMESPACE, &svc.id).await?;
    let mut result = ImportResult {
        imported: 0,
        removed: 0,
        warnings: vec![],
        dry_run,
        stubs: None,
    };
    let openapi_doc = spec
        .as_ref()
        .filter(|_| svc.source_format == "openapi")
        .and_then(|s| s.get("document").cloned());
    if let (Some(doc), Some(tag)) = (openapi_doc, svc.tag.clone()) {
        let opts = svc.import_options.clone().unwrap_or(json!({}));
        let r = fastmock_core::openapi::import_with(
            &doc,
            &fastmock_core::openapi::ImportOptions {
                tag: tag.clone(),
                validate_requests: opts
                    .get("validateRequests")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                path_prefix: opts
                    .get("pathPrefix")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
            &fastmock_core::openapi::Overrides {
                operations: ov.operations.clone(),
            },
        )
        .map_err(|e| bad(e.to_string()))?;
        warnings.extend(r.warnings);
        result = finish_import(
            app,
            p,
            ImportTarget {
                format: "microcks",
                tag: &tag,
                replace: true,
                dry_run,
                space: None,
            },
            r.stubs,
            warnings,
        )
        .await?
        .0;
    } else {
        let mut updated = Vec::new();
        for (op_name, meta) in &ov.operations {
            if meta.dispatcher.is_some() {
                warnings.push(format!(
                    "`{op_name}`: dispatcher overrides are applied to services imported from OpenAPI only"
                ));
            }
            let Some(ms) = meta.delay else { continue };
            let Some(op) = svc.operations.iter().find(|o| &o.name == op_name) else {
                continue;
            };
            for id in &op.stub_ids {
                if let Some(mut s) = app.stores.stubs.get(*id).await? {
                    s.response.delay = Some(fastmock_core::Delay {
                        fixed_ms: Some(ms.min(600_000)),
                        ..Default::default()
                    });
                    for r in &mut s.responses {
                        r.delay = s.response.delay.clone();
                    }
                    s.updated_at = None;
                    updated.push(s);
                }
            }
        }
        result.imported = updated.len();
        if dry_run {
            result.stubs = Some(updated);
        } else {
            for s in &updated {
                app.engine.validate(s).map_err(bad)?;
                app.stores.stubs.upsert(s).await?;
            }
            refresh(app).await?;
        }
        result.warnings = warnings;
    }
    if !dry_run {
        svc.labels.extend(ov.labels.clone());
        svc.overrides = Some(ov);
        svc.artifacts.retain(|a| a.kind != "APIMetadata");
        svc.artifacts.push(CompanionArtifact {
            kind: "APIMetadata".into(),
            imported_at: Utc::now(),
            imported_by: p.name.clone(),
        });
        regroup(app, &mut svc).await?;
        audit_event(app, p, "service.metadata", json!({"id": svc.id})).await;
    }
    Ok(Json(ArtifactImportResult {
        result,
        service_id: (!dry_run).then_some(svc.id),
    }))
}

#[tracing::instrument(name = "import.microcks", skip_all)]
async fn run_microcks(
    app: &AppState,
    p: &Principal,
    doc: &Value,
    dry_run: bool,
) -> ApiResult<Json<ArtifactImportResult>> {
    let art = match doc {
        Value::String(s) => microcks::parse_artifact(s),
        other => microcks::parse_artifact_value(other),
    }
    .map_err(|e| {
        count("microcks", false);
        bad(e.to_string())
    })?;
    let (name, version) = art.name();
    let id = service_id(name, version);
    let svc = load(app, &id).await?.ok_or_else(|| {
        count("microcks", false);
        ApiError(
            StatusCode::NOT_FOUND,
            format!("service `{name}` version `{version}` is not imported; import its main artifact first"),
        )
    })?;
    count("microcks", true);
    match &art {
        Artifact::Examples(ex) => apply_examples(app, p, svc, ex, dry_run).await,
        Artifact::Metadata(md) => apply_metadata(app, p, svc, md, dry_run).await,
    }
}

#[utoipa::path(
    post, path = "/__admin/import/microcks", tag = "imports", operation_id = "importMicrocks",
    summary = "Apply a Microcks APIExamples or APIMetadata document to an imported service",
    description = "The service is found by `metadata.name` and `metadata.version`. APIExamples add example stubs (tag `<service tag>:examples`, replaced on re-import). APIMetadata adds labels and per-operation delay and dispatcher overrides; for OpenAPI services the stubs are regenerated with the overrides, which are kept across re-imports.",
    request_body(content = MicrocksImport, example = json!({"document": "apiVersion: mocks.microcks.io/v1alpha1\nkind: APIMetadata\nmetadata:\n  name: API Pastry - 2.0\n  version: 2.0.0\n  labels: {team: pastry}\noperations:\n  'GET /pastry/{name}':\n    delay: 100\n"})),
    responses(
        (status = 200, description = "Import result", body = ArtifactImportResult),
        (status = 400, description = "Invalid document", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "The service named by the document is not imported", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_microcks(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<MicrocksImport>,
) -> ApiResult<Json<ArtifactImportResult>> {
    run_microcks(&app, &p, &b.document, b.dry_run).await
}

pub async fn import_text(
    app: &AppState,
    p: &Principal,
    format: &str,
    text: &str,
    tag: Option<&str>,
    replace: bool,
    dry_run: bool,
) -> ApiResult<Json<ImportResult>> {
    let r = match format {
        "graphql" => {
            let b = GraphQlImport {
                schema: String::new(),
                examples: None,
                tag: tag.map(String::from),
                path: None,
                name: None,
                version: None,
                replace,
                dry_run,
            };
            run_graphql(app, p, text, None, &b).await?
        }
        "soapui" | "wsdl" => {
            let b = SoapImport {
                document: text.to_string(),
                tag: tag.map(String::from),
                path: None,
                name: None,
                version: None,
                mock_service: None,
                replace,
                dry_run,
            };
            run_soap(
                app,
                p,
                if format == "soapui" { "soapui" } else { "wsdl" },
                &b,
            )
            .await?
        }
        "microcks" => run_microcks(app, p, &Value::String(text.to_string()), dry_run).await?,
        other => {
            return Err(bad(format!(
                "format `{other}` is not handled by the artifact importers"
            )));
        }
    };
    Ok(Json(r.0.result))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/services", get(list_services))
        .route("/services/{id}", get(get_service).delete(delete_service))
        .route("/services/{id}/spec", get(get_spec))
        .route("/import/graphql", post(import_graphql))
        .route("/import/soapui", post(import_soapui))
        .route("/import/wsdl", post(import_wsdl))
        .route("/import/microcks", post(import_microcks))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        list_services,
        get_service,
        delete_service,
        get_spec,
        import_graphql,
        import_soapui,
        import_wsdl,
        import_microcks
    ),
    components(schemas(
        Service,
        ServiceOperation,
        ServiceOverrides,
        CompanionArtifact,
        ServiceList,
        StoredSpec,
        DeletedService,
        ArtifactImportResult,
        GraphQlImport,
        SoapImport,
        MicrocksImport,
        ApiKind,
        OperationMeta
    ))
)]
pub struct ServicesApi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_ids_are_valid_tags() {
        assert_eq!(
            service_id("API Pastry - 2.0", "2.0.0"),
            "api-pastry-2.0:2.0.0"
        );
        assert_eq!(service_id("  ", ""), "service:0");
        let long = service_id(&"x".repeat(300), "1");
        assert!(long.len() <= 100);
        assert_ne!(long, service_id(&"x".repeat(301), "1"));
        for id in [
            service_id("Hello Service Mock", "0.9"),
            long,
            service_id("Ünïcode / API", "v1 beta"),
        ] {
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')),
                "{id}"
            );
        }
    }

    #[test]
    fn asyncapi_operations_v2_and_v3() {
        let v2 = json!({"asyncapi": "2.6.0", "info": {"title": "User signed-up API", "version": "0.1.0"},
            "channels": {"user/signedup": {"subscribe": {"message": {}}}, "user/deleted": {"publish": {}}}});
        let info = ServiceInfo::asyncapi(&v2, "users");
        assert_eq!(
            service_id(&info.name, &info.version),
            "user-signed-up-api:0.1.0"
        );
        let names: Vec<String> = info
            .operations
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect();
        assert_eq!(
            names,
            vec!["SUBSCRIBE user/signedup", "PUBLISH user/deleted"]
        );
        let v3 = json!({"asyncapi": "3.0.0", "info": {"title": "T", "version": "1"},
            "channels": {"signedup": {"address": "user.signedup"}},
            "operations": {"onSignup": {"action": "receive", "channel": {"$ref": "#/channels/signedup"}}}});
        let ops = ServiceInfo::asyncapi(&v3, "t").operations.unwrap();
        assert_eq!(ops[0].name, "RECEIVE user.signedup");
        assert_eq!(ServiceInfo::asyncapi(&v3, "t").kind, ApiKind::Event);
    }
}
