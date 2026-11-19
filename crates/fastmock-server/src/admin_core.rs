use axum::extract::{Extension, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use fastmock_core::GlobalSettings;
use fastmock_core::engine::{COUNTER_PREFIX, SETTINGS_KEY, SETTINGS_NAMESPACE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::admin::{
    ApiResult, ErrorBody, ImportResult, ImportTarget, audit_event, bad, finish_import, valid_tag,
};
use crate::auth::Principal;
use crate::state::AppState;

const MAX_COUNTERS: usize = 10_000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(get_settings).put(put_settings))
        .route("/stubs/counters", get(list_counters).delete(reset_counters))
        .route("/import/mockserver", post(import_mockserver))
}

#[utoipa::path(
    get, path = "/__admin/settings", tag = "system", operation_id = "getSettings",
    summary = "Read the global response settings",
    description = "Global delays applied to every stub response and the proxy pass-through switch.",
    responses(
        (status = 200, description = "Current settings", body = GlobalSettings),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_settings(State(app): State<AppState>) -> ApiResult<Json<GlobalSettings>> {
    let stored = app
        .stores
        .data
        .get(SETTINGS_NAMESPACE, SETTINGS_KEY)
        .await?;
    let settings = match stored {
        Some(v) => serde_json::from_value(v).unwrap_or_default(),
        None => GlobalSettings::default(),
    };
    Ok(Json(settings))
}

#[utoipa::path(
    put, path = "/__admin/settings", tag = "system", operation_id = "putSettings",
    summary = "Replace the global response settings",
    description = "`fixedDelayMs` is used when a stub has no delay of its own; `delay` is added to every stub response; `proxyPassThrough: false` stops proxy stubs from forwarding (they answer 404). Every replica applies the change within 2 seconds.",
    request_body(content = GlobalSettings, example = json!({"fixedDelayMs": 100, "delay": {"lognormal": {"medianMs": 80, "sigma": 0.4}}, "proxyPassThrough": true})),
    responses(
        (status = 200, description = "Stored settings", body = GlobalSettings),
        (status = 400, description = "Invalid settings", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn put_settings(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(v): Json<Value>,
) -> ApiResult<Json<GlobalSettings>> {
    let settings: GlobalSettings =
        serde_json::from_value(v).map_err(|e| bad(format!("invalid settings: {e}")))?;
    settings.validate().map_err(bad)?;
    let stored = serde_json::to_value(&settings).map_err(anyhow::Error::from)?;
    app.stores
        .data
        .put(SETTINGS_NAMESPACE, SETTINGS_KEY, &stored)
        .await?;
    app.engine.cache_settings(settings.clone());
    audit_event(&app, &p, "settings.update", stored).await;
    Ok(Json(settings))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StubCounter {
    /// `times` (matches counted against `times`) or `seq` (position in `responses`).
    pub kind: String,
    pub stub_id: String,
    pub count: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StubCounterList {
    pub total: usize,
    pub counters: Vec<StubCounter>,
}

#[utoipa::path(
    get, path = "/__admin/stubs/counters", tag = "stubs", operation_id = "listStubCounters",
    summary = "List the shared stub counters",
    description = "Counters behind `times` and response sequences, shared by all replicas.",
    responses(
        (status = 200, description = "Counters", body = StubCounterList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_counters(State(app): State<AppState>) -> ApiResult<Json<StubCounterList>> {
    let raw = app.stores.hot.scan(COUNTER_PREFIX, MAX_COUNTERS).await?;
    let counters: Vec<StubCounter> = raw
        .into_iter()
        .filter_map(|(k, v)| {
            let rest = k.strip_prefix(COUNTER_PREFIX)?;
            let mut it = rest.splitn(3, ':');
            let kind = it.next()?.to_string();
            let stub_id = it.next()?.to_string();
            Some(StubCounter {
                kind,
                stub_id,
                count: v.parse().unwrap_or(0),
            })
        })
        .collect();
    Ok(Json(StubCounterList {
        total: counters.len(),
        counters,
    }))
}

#[utoipa::path(
    delete, path = "/__admin/stubs/counters", tag = "stubs", operation_id = "resetStubCounters",
    summary = "Reset `times` and response sequence counters",
    responses(
        (status = 200, description = "Number of counters removed", body = Object, example = json!({"reset": 3})),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn reset_counters(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    let n = app.engine.reset_counters().await?;
    audit_event(&app, &p, "stub.counters.reset", json!({"reset": n})).await;
    Ok(Json(json!({"reset": n})))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct MockServerImportQuery {
    /// Tag added to every stub (default `mockserver`).
    pub tag: Option<String>,
    /// Delete the stubs that already carry the tag first (default true).
    pub replace: Option<bool>,
    /// Convert and validate only; nothing is stored.
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MockServerImport {
    /// One expectation or an array of expectations.
    #[schema(value_type = Object)]
    pub expectations: Value,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub replace: Option<bool>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// One expectation, an array of expectations, or the wrapper with `expectations`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum MockServerImportBody {
    Wrapper(MockServerImport),
    Expectations(Vec<std::collections::HashMap<String, Value>>),
    Expectation(std::collections::HashMap<String, Value>),
}

#[utoipa::path(
    post, path = "/__admin/import/mockserver", tag = "imports", operation_id = "importMockServer",
    summary = "Convert MockServer expectations into stubs",
    description = "Accepts one expectation, an array of expectations, or `{\"expectations\": ..., \"tag\": ..., \"replace\": ..., \"dryRun\": ...}`. Query parameters override the wrapper fields. Velocity and JavaScript templates and class callbacks are never executed: such expectations are skipped with a warning.",
    params(MockServerImportQuery),
    request_body(content = MockServerImportBody, example = json!({"expectations": [{"httpRequest": {"method": "GET", "path": "/ping"}, "httpResponse": {"statusCode": 200, "body": "pong"}, "times": {"remainingTimes": 1}}], "tag": "legacy"})),
    responses(
        (status = 200, description = "Import result", body = ImportResult),
        (status = 400, description = "Invalid document or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_mockserver(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<MockServerImportQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<ImportResult>> {
    let (doc, w_tag, w_replace, w_dry) = match body {
        Value::Object(mut o)
            if o.contains_key("expectations") && !o.contains_key("httpRequest") =>
        {
            let field = |o: &mut serde_json::Map<String, Value>, k: &str| o.remove(k);
            let tag = field(&mut o, "tag").and_then(|v| v.as_str().map(String::from));
            let replace = field(&mut o, "replace").and_then(|v| v.as_bool());
            let dry = field(&mut o, "dryRun").and_then(|v| v.as_bool());
            (
                o.remove("expectations").unwrap_or(Value::Null),
                tag,
                replace,
                dry,
            )
        }
        other => (other, None, None, None),
    };
    if !(doc.is_array() || doc.is_object()) {
        return Err(bad("expectations must be an object or an array"));
    }
    let tag = q.tag.or(w_tag).unwrap_or_else(|| "mockserver".into());
    valid_tag(&tag)?;
    let replace = q.replace.or(w_replace).unwrap_or(true);
    let dry_run = q.dry_run.or(w_dry).unwrap_or(false);
    let converted = fastmock_core::mockserver::convert(&doc);
    let mut warnings = converted.warnings;
    let mut stubs = Vec::with_capacity(converted.stubs.len());
    for mut s in converted.stubs {
        if let Err(e) = app.engine.validate(&s) {
            warnings.push(format!(
                "{}: the converted stub is invalid ({e}); it was skipped",
                s.name.clone().unwrap_or_else(|| s.id.to_string())
            ));
            continue;
        }
        s.tags.push(tag.clone());
        stubs.push(s);
    }
    finish_import(
        &app,
        &p,
        ImportTarget {
            format: "mockserver",
            tag: &tag,
            replace,
            dry_run,
        },
        stubs,
        warnings,
    )
    .await
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        get_settings,
        put_settings,
        list_counters,
        reset_counters,
        import_mockserver
    ),
    components(schemas(
        GlobalSettings,
        fastmock_core::Delay,
        fastmock_core::LogNormalDelay,
        StubCounter,
        StubCounterList,
        MockServerImportBody,
        MockServerImport
    ))
)]
pub struct CoreApi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_doc_builds() {
        let doc = <CoreApi as utoipa::OpenApi>::openapi();
        let paths: Vec<&String> = doc.paths.paths.keys().collect();
        assert!(paths.iter().any(|p| p.as_str() == "/__admin/settings"));
        assert!(
            paths
                .iter()
                .any(|p| p.as_str() == "/__admin/import/mockserver")
        );
        assert!(
            paths
                .iter()
                .any(|p| p.as_str() == "/__admin/stubs/counters")
        );
    }
}
