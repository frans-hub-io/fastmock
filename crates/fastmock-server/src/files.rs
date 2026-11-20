use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{Value, json};

use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad, not_found};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

pub const NAMESPACE: &str = "files";
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FILES: usize = 10_000;
const MAX_NAME: usize = 512;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/files", get(list_files))
        .route(
            "/files/{*name}",
            get(get_file).put(put_file).delete(delete_file),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_FILE_BYTES + 1024))
}

pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(format!("file name must be 1..{MAX_NAME} bytes"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err("file name may only contain [A-Za-z0-9._-/]".into());
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err("file name must be a relative path without empty segments".into());
    }
    if name.split('/').any(|seg| seg == "." || seg == "..") || name.contains("..") {
        return Err("file name must not contain `..` or `.` segments".into());
    }
    Ok(())
}

fn checked(name: &str) -> ApiResult<()> {
    valid_name(name).map_err(bad)
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    #[schema(example = "users/42.json")]
    pub name: String,
    #[schema(example = "application/json")]
    pub content_type: String,
    pub size: u64,
    pub updated_at: Option<String>,
    /// Space of the file; absent in the global area. `name` is relative to the space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
}

fn info_of(name: String, v: &Value) -> FileInfo {
    FileInfo {
        space: None,
        name,
        content_type: v
            .get("contentType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string(),
        size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
        updated_at: v.get("updatedAt").and_then(Value::as_str).map(String::from),
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct FileList {
    pub total: usize,
    pub files: Vec<FileInfo>,
}

#[utoipa::path(
    get, path = "/__admin/files", tag = "files", operation_id = "listFiles",
    summary = "List stored body files",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Files, sorted by name", body = FileList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_files(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<FileList>> {
    let sel = spaces::select(&app, &p, Permission::FilesRead, q.space.as_deref())?;
    let items = app
        .stores
        .data
        .list_without(NAMESPACE, MAX_FILES as i64, "base64")
        .await?;
    let files: Vec<FileInfo> = items
        .into_iter()
        .filter_map(|(k, v)| {
            let (space, name) = spaces::split_key(&app, &k);
            if !sel.matches(space) {
                return None;
            }
            let mut info = info_of(name.to_string(), &v);
            info.space = space.map(String::from);
            Some(info)
        })
        .collect();
    Ok(Json(FileList {
        total: files.len(),
        files,
    }))
}

#[utoipa::path(
    get, path = "/__admin/files/{name}", tag = "files", operation_id = "getFile",
    summary = "Download a body file",
    description = "Returns the raw content with the stored content type, as an attachment.",
    params(("name" = String, Path, description = "File name, may contain `/`"), SpaceQuery),
    responses(
        (status = 200, description = "Raw file content", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 400, description = "Invalid name", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "No such file", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_file(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Response> {
    let space = spaces::target(&app, &p, Permission::FilesRead, q.space.as_deref())?;
    checked(&name)?;
    let v = app
        .stores
        .data
        .get(NAMESPACE, &spaces::join_key(space.as_deref(), &name))
        .await?
        .ok_or_else(|| not_found("file"))?;
    let bytes = v
        .get("base64")
        .and_then(Value::as_str)
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("stored file {name} is corrupt")))?;
    let info = info_of(name.clone(), &v);
    let mut r = Response::new(axum::body::Body::from(bytes));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&info.content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    let file_name = name.rsplit('/').next().unwrap_or("file");
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{file_name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox; frame-ancestors 'none'"),
    );
    Ok(r)
}

fn content_type(headers: &HeaderMap) -> ApiResult<String> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().map_err(|_| bad("invalid content-type header")))
        .transpose()?
        .unwrap_or("application/octet-stream")
        .trim();
    if ct.is_empty() || ct.len() > 255 || ct.chars().any(|c| c.is_control()) {
        return Err(bad("content-type must be 1..255 printable characters"));
    }
    Ok(ct.to_string())
}

#[utoipa::path(
    put, path = "/__admin/files/{name}", tag = "files", operation_id = "putFile",
    summary = "Upload or replace a body file",
    description = "The raw request body is the file content (at most 10 MiB); `Content-Type` is stored with it and becomes the default response content type for stubs using `response.bodyFileName`.",
    params(("name" = String, Path, description = "File name: [A-Za-z0-9._-/], at most 512 bytes, no `..`"), SpaceQuery),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Replaced", body = FileInfo),
        (status = 201, description = "Created", body = FileInfo),
        (status = 400, description = "Invalid name or content type", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 409, description = "Too many files", body = ErrorBody),
        (status = 413, description = "File larger than 10 MiB"),
    ),
    security(("bearer" = []))
)]
pub async fn put_file(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    Query(q): Query<SpaceQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let space = spaces::target(&app, &p, Permission::FilesWrite, q.space.as_deref())?;
    checked(&name)?;
    let key = spaces::join_key(space.as_deref(), &name);
    if body.len() > MAX_FILE_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "files are limited to 10 MiB".into(),
        ));
    }
    let ct = content_type(&headers)?;
    let existed = app.stores.data.get(NAMESPACE, &key).await?.is_some();
    if !existed {
        let n = app
            .stores
            .data
            .list_without(NAMESPACE, MAX_FILES as i64 + 1, "base64")
            .await?
            .len();
        if n >= MAX_FILES {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("at most {MAX_FILES} files; delete some first"),
            ));
        }
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let v = json!({
        "contentType": ct,
        "base64": base64::engine::general_purpose::STANDARD.encode(&body),
        "size": body.len(),
        "updatedAt": now,
    });
    app.stores.data.put(NAMESPACE, &key, &v).await?;
    app.engine.invalidate_files();
    metrics::counter!("fastmock_files_written_total").increment(1);
    metrics::histogram!("fastmock_file_size_bytes").record(body.len() as f64);
    audit_event_in(
        &app,
        &p,
        space.as_deref(),
        "file.put",
        json!({"name": name, "contentType": ct, "size": body.len(), "replaced": existed}),
    )
    .await;
    let mut info = info_of(name, &v);
    info.space = space;
    Ok((
        if existed {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(info),
    )
        .into_response())
}

#[utoipa::path(
    delete, path = "/__admin/files/{name}", tag = "files", operation_id = "deleteFile",
    summary = "Delete a body file",
    params(("name" = String, Path, description = "File name"), SpaceQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "Invalid name", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 404, description = "No such file", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_file(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    let space = spaces::target(&app, &p, Permission::FilesWrite, q.space.as_deref())?;
    checked(&name)?;
    if !app
        .stores
        .data
        .delete(NAMESPACE, &spaces::join_key(space.as_deref(), &name))
        .await?
    {
        return Err(not_found("file"));
    }
    app.engine.invalidate_files();
    audit_event_in(
        &app,
        &p,
        space.as_deref(),
        "file.delete",
        json!({"name": name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(list_files, get_file, put_file, delete_file))]
pub struct FilesApi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        for ok in ["a.json", "users/42.json", "x_y-z.v1.bin", "deep/er/file"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/abs",
            "a//b",
            "../x",
            "a/../b",
            "a/./b",
            "x..y",
            "dir/",
            "sp ace",
            "q?x",
            "back\\slash",
            "ü",
        ] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        assert!(valid_name(&"a".repeat(512)).is_ok());
        assert!(valid_name(&"a".repeat(513)).is_err());
    }
}
