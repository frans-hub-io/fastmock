use std::sync::OnceLock;

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use utoipa::OpenApi;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};

use crate::{admin, files, imports, journal_api, snapshot};

struct BearerScheme;

impl utoipa::Modify for BearerScheme {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Static admin token from FASTMOCK_ADMIN_TOKENS, a managed token from POST /__admin/tokens or, with FASTMOCK_OIDC_API_AUDIENCE, a JWT from the OIDC provider. The UI may instead use the SSO session cookie (see the `session` scheme). Every route needs a permission (`resource:action`); the built-in roles viewer, editor and admin are permission sets, custom roles and space bindings come from FASTMOCK_ACCESS_FILE. Without an access file: GET/HEAD and read-only POSTs need viewer, other changes editor; deleting all stubs, reset, the audit log and managing other tokens (`/__admin/tokens*`, `tokens:read`/`tokens:write`) and reading the access configuration (`/__admin/access`, `access:read`) need admin. Routes without a rule need admin.",
                    ))
                    .build(),
            ),
        );
        let mut cookie = ApiKeyValue::new("__Host-fastmock_session");
        cookie.description = Some("SSO session cookie set by GET /__admin/auth/callback; accepted by every /__admin route instead of a bearer token when no Authorization header is sent. Requests other than GET and HEAD need the `X-FastMock-Request: 1` header.".into());
        components.add_security_scheme("session", SecurityScheme::ApiKey(ApiKey::Cookie(cookie)));
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "FastMock Admin API",
        description = "Manage stubs, inspect and verify the request journal, import API descriptions, record traffic and control chaos rules. All routes live on the admin listener under `/__admin` and need `Authorization: Bearer <token>`. Errors use `{\"error\": \"...\", \"details\": [...]}`.",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    paths(
        serve_spec,
        admin::list_stubs,
        admin::create_stub,
        admin::delete_stubs,
        admin::get_stub,
        admin::put_stub,
        admin::patch_stub,
        admin::delete_stub,
        admin::import_stubs,
        admin::validate_stub,
        admin::find_by_metadata,
        admin::remove_by_metadata,
        admin::unused_stubs,
        admin::export_stubs,
        admin::list_requests,
        admin::clear_requests,
        admin::count_requests,
        admin::count_requests_post,
        admin::near_misses,
        admin::list_callbacks,
        admin::clear_callbacks,
        admin::list_scenarios,
        admin::reset_scenarios,
        admin::set_scenario,
        admin::kv_get,
        admin::kv_put,
        admin::kv_delete,
        admin::reset,
        admin::info,
        admin::list_spaces,
        admin::version,
        admin::plugins,
        admin::list_audit,
        admin::import_openapi,
        admin::import_wiremock,
        admin::recording_status,
        admin::recording_start,
        admin::recording_stop,
        admin::chaos_get,
        admin::chaos_put,
        admin::chaos_delete,
        admin::grpc_list,
        admin::grpc_put,
        admin::grpc_delete,
    ),
    components(schemas(admin::ErrorBody, admin::NearMissDoc)),
    modifiers(&BearerScheme),
    tags(
        (name = "stubs", description = "Create, change, search and export stubs"),
        (name = "requests", description = "Request journal"),
        (name = "verification", description = "Verify that requests were (not) received"),
        (name = "callbacks", description = "Outgoing callback deliveries"),
        (name = "scenarios", description = "Stateful scenarios"),
        (name = "kv", description = "Shared key-value store"),
        (name = "files", description = "Body files referenced by `response.bodyFileName`"),
        (name = "imports", description = "Generate stubs from API descriptions and traffic captures"),
        (name = "recordings", description = "Record and play back real traffic"),
        (name = "chaos", description = "Probabilistic faults and delays"),
        (name = "grpc", description = "gRPC descriptor sets"),
        (name = "audit", description = "Audit log"),
        (name = "access", description = "Spaces, roles and bindings of the access file (read-only)"),
        (name = "auth", description = "SSO sign-in for the UI (OIDC, backend for frontend)"),
        (name = "system", description = "Server information")
    )
)]
pub struct AdminApi;

pub fn spec() -> utoipa::openapi::OpenApi {
    let mut doc = AdminApi::openapi();
    doc.merge(journal_api::JournalApi::openapi());
    doc.merge(files::FilesApi::openapi());
    doc.merge(snapshot::SnapshotApi::openapi());
    doc.merge(imports::ImportsApi::openapi());
    doc.merge(crate::asyncmock::AsyncMockApi::openapi());
    doc.merge(crate::contract::ContractApi::openapi());
    doc.merge(crate::services::ServicesApi::openapi());
    doc.merge(crate::admin_core::CoreApi::openapi());
    doc.merge(crate::tokens::TokensApi::openapi());
    doc.merge(crate::mail_chaos_api::MailChaosApi::openapi());
    doc.merge(crate::capture::api::CaptureApi::openapi());
    doc.merge(crate::mail_webhooks_api::MailWebhooksApi::openapi());
    doc.merge(crate::oidc::routes::AuthApi::openapi());
    doc.merge(crate::mail_api::MailApi::openapi());
    normalize_tags(&mut doc);
    standard_responses(&mut doc);
    doc
}

/// Every route behind the admin authentication can answer `403` (a missing permission, or a browser
/// `Origin` that is not allowed), and every route with a `space` parameter `404` (an unknown space, or
/// one the caller cannot see). Operations that do not document them get the generic description.
fn standard_responses(doc: &mut utoipa::openapi::OpenApi) {
    use utoipa::openapi::path::ParameterIn;
    use utoipa::openapi::{ContentBuilder, Ref, RefOr, ResponseBuilder};
    let error = |description: &str| -> RefOr<utoipa::openapi::Response> {
        RefOr::T(
            ResponseBuilder::new()
                .description(description)
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .schema(Some(Ref::from_schema_name("ErrorBody")))
                        .build(),
                )
                .build(),
        )
    };
    for (path, item) in doc.paths.paths.iter_mut() {
        if path == "/openapi.json" || path.starts_with("/__admin/auth/") {
            continue;
        }
        for op in [
            &mut item.get,
            &mut item.put,
            &mut item.post,
            &mut item.delete,
            &mut item.patch,
            &mut item.head,
        ]
        .into_iter()
        .flatten()
        {
            let responses = &mut op.responses.responses;
            responses.entry("403".into()).or_insert_with(|| {
                error("Forbidden: the caller lacks the permission named in `required`, or the browser origin is not allowed")
            });
            let has_space = op
                .parameters
                .iter()
                .flatten()
                .any(|p| p.name == "space" && matches!(p.parameter_in, ParameterIn::Query));
            if has_space {
                responses
                    .entry("404".into())
                    .or_insert_with(|| error("Unknown space, or a space the caller cannot see"));
            }
        }
    }
}

fn normalize_tags(doc: &mut utoipa::openapi::OpenApi) {
    let mut tags = doc.tags.take().unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    tags.retain(|t| seen.insert(t.name.clone()));
    for item in doc.paths.paths.values() {
        for op in [
            &item.get,
            &item.put,
            &item.post,
            &item.delete,
            &item.patch,
            &item.head,
        ]
        .into_iter()
        .flatten()
        {
            for t in op.tags.iter().flatten() {
                if seen.insert(t.clone()) {
                    tags.push(utoipa::openapi::tag::Tag::new(t.clone()));
                }
            }
        }
    }
    doc.tags = Some(tags);
}

pub fn spec_json() -> &'static str {
    static SPEC: OnceLock<String> = OnceLock::new();
    SPEC.get_or_init(|| {
        let mut s = spec().to_pretty_json().unwrap_or_else(|_| "{}".to_string());
        s.push('\n');
        s
    })
}

#[utoipa::path(
    get, path = "/openapi.json", tag = "system", operation_id = "getOpenApiSpec",
    summary = "This OpenAPI document",
    description = "Served on the admin listener without authentication; it contains no data.",
    responses((status = 200, description = "OpenAPI 3.1 document", content_type = "application/json", body = Object)),
    security(())
)]
pub async fn serve_spec() -> Response {
    let mut r = ([(header::CONTENT_TYPE, "application/json")], spec_json()).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    use super::*;

    fn spec_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/openapi.json")
    }

    #[test]
    fn openapi_spec_is_current() {
        let generated = spec_json();
        let path = spec_path();
        if std::env::var("UPDATE_OPENAPI").as_deref() == Ok("1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, generated).unwrap();
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == generated,
            "docs/api/openapi.json is out of date; run `UPDATE_OPENAPI=1 cargo test -p fastmock-server openapi_spec_is_current`"
        );
    }

    fn collect_refs(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if k == "$ref"
                        && let Value::String(r) = x
                    {
                        out.push(r.clone());
                    }
                    collect_refs(x, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
            _ => {}
        }
    }

    #[test]
    fn importers_document_space_and_host() {
        let v: Value = serde_json::from_str(spec_json()).unwrap();
        for path in [
            "/__admin/import",
            "/__admin/import/openapi",
            "/__admin/import/wiremock",
            "/__admin/import/postman",
            "/__admin/import/har",
            "/__admin/import/mockserver",
            "/__admin/stubs/import",
        ] {
            let op = &v["paths"][path]["post"];
            let names: Vec<&str> = op["parameters"]
                .as_array()
                .unwrap_or_else(|| panic!("{path}: no parameters"))
                .iter()
                .filter(|p| p["in"] == "query")
                .filter_map(|p| p["name"].as_str())
                .collect();
            for want in ["space", "host"] {
                assert!(names.contains(&want), "{path}: no {want} parameter");
            }
            for status in ["403", "404"] {
                assert!(op["responses"].get(status).is_some(), "{path}: no {status}");
            }
        }
        let days =
            v["components"]["schemas"]["CreateToken"]["properties"]["expiresInDays"]["description"]
                .as_str()
                .unwrap();
        assert!(days.contains("FASTMOCK_TOKENS_MAX_DAYS"), "{days}");
        assert!(days.contains("lower"), "{days}");
    }

    #[test]
    fn openapi_spec_is_well_formed() {
        let v: Value = serde_json::from_str(spec_json()).unwrap();
        assert!(v["openapi"].as_str().unwrap().starts_with("3.1"));
        assert_eq!(v["info"]["title"], "FastMock Admin API");
        assert!(
            v["components"]["securitySchemes"]["bearer"]["scheme"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
        );
        let declared_tags: HashSet<&str> = v["tags"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        let mut ids = HashSet::new();
        let paths = v["paths"].as_object().unwrap();
        assert!(paths.len() > 40, "only {} paths", paths.len());
        for (path, item) in paths {
            assert!(
                path == "/openapi.json" || path.starts_with("/__admin/"),
                "{path} is outside /__admin"
            );
            for (method, op) in item.as_object().unwrap() {
                if !["get", "put", "post", "delete", "patch", "head"].contains(&method.as_str()) {
                    continue;
                }
                let where_ = format!("{method} {path}");
                let id = op["operationId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{where_}: no operationId"));
                assert!(
                    ids.insert(id.to_string()),
                    "{where_}: duplicate operationId {id}"
                );
                let tags = op["tags"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{where_}: no tags"));
                assert!(!tags.is_empty(), "{where_}: no tags");
                for t in tags {
                    assert!(
                        declared_tags.contains(t.as_str().unwrap()),
                        "{where_}: tag {t} is not declared"
                    );
                }
                assert!(
                    op["responses"].as_object().is_some_and(|r| !r.is_empty()),
                    "{where_}: no responses"
                );
                let security = op["security"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{where_}: no security"));
                if path == "/openapi.json" || path.starts_with("/__admin/auth/") {
                    assert!(
                        security.iter().all(|s| s
                            .as_object()
                            .is_some_and(|o| o.is_empty() || o.contains_key("session"))),
                        "{where_}: unexpected security"
                    );
                } else {
                    assert!(
                        security.iter().any(|s| s.get("bearer").is_some()),
                        "{where_}: not protected by the bearer scheme"
                    );
                    assert!(
                        op["responses"].get("401").is_some(),
                        "{where_}: 401 not documented"
                    );
                    assert!(
                        op["responses"].get("403").is_some(),
                        "{where_}: 403 not documented"
                    );
                    let takes_space = op["parameters"].as_array().is_some_and(|ps| {
                        ps.iter()
                            .any(|p| p["name"] == "space" && p["in"] == "query")
                    });
                    if takes_space {
                        assert!(
                            op["responses"].get("404").is_some(),
                            "{where_}: takes space but 404 not documented"
                        );
                    }
                }
                if let Some(params) = op["parameters"].as_array() {
                    for p in params {
                        if p["in"] == "path" {
                            let name = p["name"].as_str().unwrap();
                            assert!(
                                path.contains(&format!("{{{name}}}")),
                                "{where_}: path parameter {name} not in path"
                            );
                        }
                    }
                }
            }
        }
        let mut refs = Vec::new();
        collect_refs(&v, &mut refs);
        assert!(!refs.is_empty());
        for r in refs {
            let name = r
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected $ref {r}"));
            assert!(
                v["components"]["schemas"].get(name).is_some(),
                "$ref {r} does not resolve"
            );
        }
    }
}
