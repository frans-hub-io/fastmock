use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service, ServiceAccount};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, DeleteParams, DynamicObject, Patch, PatchParams, Preconditions};
use kube::core::{ApiResource, GroupVersionKind};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::{Controller, WatchStreamExt, reflector, watcher};
use kube::{Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;

use crate::crd::{
    Condition, DocumentSource, FastMock, FastMockSpec, FastMockStatus, MockApi, MockApiSpec,
    MockApiStatus, condition, merge_conditions, status_changed,
};
use crate::manifests::{self, MANAGER};
use crate::policy::{
    ALLOW_MOUNT_LABEL, ImagePolicy, denied_env_keys, referenced_secrets, secret_allowed,
};

pub const FINALIZER: &str = "fastmock.franshub.io/stubs";

const RESYNC: Duration = Duration::from_secs(300);

#[derive(Debug, Error)]
pub enum Error {
    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),
    #[error("admin api: {0}")]
    Admin(String),
    #[error("invalid spec: {0}")]
    Spec(String),
    #[error("finalizer: {0}")]
    Finalizer(String),
}

pub struct Ctx {
    pub client: Client,
    pub http: reqwest::Client,
    pub admin_url_template: String,
    pub images: ImagePolicy,
    pub failures: Mutex<HashMap<String, u32>>,
}

fn secret_value(s: &Secret, key: &str) -> Option<String> {
    s.data
        .as_ref()?
        .get(key)
        .map(|b| String::from_utf8_lossy(&b.0).into_owned())
}

async fn apply<K>(api: &Api<K>, name: &str, v: &Value) -> Result<(), Error>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
{
    api.patch(name, &PatchParams::apply(MANAGER).force(), &Patch::Apply(v))
        .await?;
    Ok(())
}

async fn delete_owned<K>(api: &Api<K>, name: &str, owner_uid: &str) -> Result<(), Error>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
{
    let Some(obj) = api.get_opt(name).await? else {
        return Ok(());
    };
    let owned = obj
        .meta()
        .owner_references
        .as_ref()
        .is_some_and(|refs| refs.iter().any(|r| r.uid == owner_uid));
    if !owned {
        return Ok(());
    }
    let dp = DeleteParams {
        preconditions: Some(Preconditions {
            uid: obj.meta().uid.clone(),
            resource_version: None,
        }),
        ..Default::default()
    };
    match api.delete(name, &dp).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.is_not_found() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn write_status<K, S>(
    api: &Api<K>,
    name: &str,
    prev: Option<&S>,
    next: &S,
) -> Result<(), Error>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
    S: Serialize + PartialEq,
{
    if !status_changed(prev, next) {
        return Ok(());
    }
    api.patch_status(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({"status": next})),
    )
    .await?;
    Ok(())
}

fn generate_token() -> String {
    let b: [u8; 32] = rand::random();
    format!("fm_op_{}", hex::encode(b))
}

fn observe(kind: &str, start: Instant, result: &Result<Action, Error>) {
    let outcome = if result.is_ok() { "ok" } else { "error" };
    metrics::counter!("fastmock_operator_reconciles_total", "kind" => kind.to_string(), "result" => outcome).increment(1);
    metrics::histogram!("fastmock_operator_reconcile_duration_seconds", "kind" => kind.to_string())
        .record(start.elapsed().as_secs_f64());
}

fn object_key<K: Resource<DynamicType = ()>>(obj: &K) -> String {
    format!(
        "{}/{}/{}",
        K::kind(&()),
        obj.meta().namespace.as_deref().unwrap_or_default(),
        obj.meta().name.as_deref().unwrap_or_default()
    )
}

pub fn backoff_delay(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(16);
    Duration::from_secs(5u64.saturating_mul(1u64 << exp).min(300))
}

fn reset_failures<K: Resource<DynamicType = ()>>(ctx: &Ctx, obj: &K, r: &Result<Action, Error>) {
    if r.is_ok()
        && let Ok(mut f) = ctx.failures.lock()
    {
        f.remove(&object_key(obj));
    }
}

pub fn trigger_hash<K: Resource>(obj: &K) -> Option<u64> {
    let m = obj.meta();
    let mut h = DefaultHasher::new();
    m.generation.hash(&mut h);
    m.finalizers.hash(&mut h);
    m.deletion_timestamp.is_some().hash(&mut h);
    Some(h.finish())
}

pub async fn reconcile_server(ms: Arc<FastMock>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let start = Instant::now();
    let r = reconcile_server_inner(&ms, &ctx).await;
    observe("FastMock", start, &r);
    reset_failures(&ctx, ms.as_ref(), &r);
    r
}

fn storage_problem(spec: &FastMockSpec) -> Option<&'static str> {
    let invalid = match spec.storage.mode.as_str() {
        "memory" => None,
        "postgres" if spec.storage.database.is_none() || spec.storage.valkey.is_none() => {
            Some("storage.mode postgres requires storage.database and storage.valkey")
        }
        "postgres" => None,
        _ => Some("storage.mode must be memory or postgres"),
    };
    invalid.or_else(|| {
        (spec.replicas > 1 && spec.storage.mode == "memory")
            .then_some("memory storage cannot be shared: use replicas 1 or postgres storage")
    })
}

async fn check_secrets(
    secrets: &Api<Secret>,
    spec: &FastMockSpec,
) -> Result<Option<(&'static str, String)>, Error> {
    let mut missing = vec![];
    let mut denied = vec![];
    for (field, name) in referenced_secrets(spec) {
        match secrets.get_opt(&name).await? {
            None => missing.push(format!("{field}={name}")),
            Some(s) if !secret_allowed(s.metadata.labels.as_ref()) => {
                denied.push(format!("{field}={name}"))
            }
            Some(_) => {}
        }
    }
    if !denied.is_empty() {
        return Ok(Some((
            "SecretNotAllowed",
            format!(
                "referenced Secrets must carry the label {ALLOW_MOUNT_LABEL}=true: {}",
                denied.join(", ")
            ),
        )));
    }
    if !missing.is_empty() {
        return Ok(Some((
            "SecretNotFound",
            format!("referenced Secrets not found: {}", missing.join(", ")),
        )));
    }
    Ok(None)
}

async fn reconcile_server_inner(ms: &FastMock, ctx: &Ctx) -> Result<Action, Error> {
    let ns = ms
        .namespace()
        .ok_or_else(|| Error::Spec("FastMock must be namespaced".into()))?;
    let name = ms.name_any();
    let owner = ms
        .controller_owner_ref(&())
        .ok_or_else(|| Error::Spec("missing uid".into()))?;
    let spec = &ms.spec;
    let generation = ms.metadata.generation.unwrap_or(0);
    let prev = ms.status.as_ref();
    let servers: Api<FastMock> = Api::namespaced(ctx.client.clone(), &ns);
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    let blocked = |reason: &str, msg: &str| {
        let base = prev.cloned().unwrap_or_default();
        FastMockStatus {
            observed_generation: generation,
            conditions: merge_conditions(
                &base.conditions,
                vec![condition("Ready", false, reason, msg)],
            ),
            ..base
        }
    };
    let image = ctx.images.resolve(&spec.image);
    let denied_env = denied_env_keys(&spec.env);
    let refusal: Option<(&str, String, Duration)> = if let Some(msg) = storage_problem(spec) {
        Some(("InvalidSpec", msg.to_string(), RESYNC))
    } else if let Err(msg) = &image {
        Some(("ImageNotAllowed", msg.clone(), RESYNC))
    } else if !denied_env.is_empty() {
        Some((
            "EnvNotAllowed",
            format!(
                "spec.env must not set {}; use the dedicated spec fields",
                denied_env.join(", ")
            ),
            RESYNC,
        ))
    } else {
        check_secrets(&secrets, spec)
            .await?
            .map(|(r, m)| (r, m, Duration::from_secs(60)))
    };
    if let Some((reason, msg, requeue)) = refusal {
        tracing::warn!(fastmock = %name, namespace = %ns, %reason, %msg, "FastMock refused");
        write_status(&servers, &name, prev, &blocked(reason, &msg)).await?;
        return Ok(Action::requeue(requeue));
    }
    let image = image.unwrap_or_default();
    let owner_uid = owner.uid.clone();
    let token_name = format!("{name}-operator-token");
    let token = match secrets
        .get_opt(&token_name)
        .await?
        .and_then(|s| secret_value(&s, "token"))
    {
        Some(t) => t,
        None => {
            let t = generate_token();
            apply(
                &secrets,
                &token_name,
                &manifests::operator_token_secret(&name, &ns, &owner, &t),
            )
            .await?;
            t
        }
    };
    let user_lines = match &spec.admin_tokens_secret {
        Some(r) => secrets
            .get_opt(&r.name)
            .await?
            .and_then(|s| secret_value(&s, &r.key))
            .unwrap_or_default(),
        None => String::new(),
    };
    let tokens = manifests::tokens_file(&user_lines, &token);
    apply(
        &secrets,
        &format!("{name}-admin-tokens"),
        &manifests::tokens_secret(&name, &ns, &owner, &tokens),
    )
    .await?;
    let env = manifests::env_map(&name, spec);
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), &ns);
    apply(
        &cms,
        &format!("{name}-config"),
        &manifests::config_map(&name, &ns, &owner, &env),
    )
    .await?;
    let sas: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), &ns);
    apply(&sas, &name, &manifests::service_account(&name, &ns, &owner)).await?;
    let checksum = manifests::sha256_hex(&format!(
        "{}|{}",
        serde_json::to_string(&env).unwrap_or_default(),
        tokens
    ));
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    apply(
        &deployments,
        &name,
        &manifests::deployment(&name, &ns, &owner, spec, &image, &checksum),
    )
    .await?;
    let svcs: Api<Service> = Api::namespaced(ctx.client.clone(), &ns);
    let [data, admin] = manifests::services(&name, &ns, &owner);
    apply(&svcs, &name, &data).await?;
    apply(&svcs, &format!("{name}-admin"), &admin).await?;
    let pdbs: Api<PodDisruptionBudget> = Api::namespaced(ctx.client.clone(), &ns);
    if spec.replicas > 1 {
        apply(&pdbs, &name, &manifests::pdb(&name, &ns, &owner)).await?;
    } else {
        delete_owned(&pdbs, &name, &owner_uid).await?;
    }
    let route_condition = reconcile_route(ctx, ms, &ns, &name, &owner).await;
    let ready = deployments
        .get_opt(&name)
        .await?
        .and_then(|d| d.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let ok = ready >= 1;
    let route_ok = route_condition.as_ref().is_none_or(|c| c.status == "True");
    let mut conditions = vec![condition(
        "Ready",
        ok,
        if ok { "Available" } else { "Progressing" },
        &format!("{ready}/{} replicas ready", spec.replicas),
    )];
    conditions.extend(route_condition);
    let st = FastMockStatus {
        ready_replicas: ready,
        observed_generation: generation,
        admin_url: format!("http://{name}-admin.{ns}.svc:9090"),
        data_url: format!("http://{name}.{ns}.svc:8080"),
        conditions: merge_conditions(
            prev.map(|p| p.conditions.as_slice()).unwrap_or_default(),
            conditions,
        ),
    };
    write_status(&servers, &name, prev, &st).await?;
    Ok(Action::requeue(if !ok {
        Duration::from_secs(10)
    } else if !route_ok {
        Duration::from_secs(60)
    } else {
        RESYNC
    }))
}

async fn reconcile_route(
    ctx: &Ctx,
    ms: &FastMock,
    ns: &str,
    name: &str,
    owner: &k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Option<Condition> {
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "gateway.networking.k8s.io",
        "v1",
        "HTTPRoute",
    ));
    let routes: Api<DynamicObject> = Api::namespaced_with(ctx.client.clone(), ns, &ar);
    let Some(route) = &ms.spec.route else {
        if let Err(e) = delete_owned(&routes, name, &owner.uid).await {
            tracing::warn!(fastmock = %name, namespace = %ns, error = %e, "removing HTTPRoute failed");
        }
        return None;
    };
    let manifest = manifests::http_route(name, ns, owner, &route.hostnames, &route.parent_refs);
    Some(
        match routes
            .patch(
                name,
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(manifest),
            )
            .await
        {
            Ok(_) => condition("RouteReady", true, "Applied", "HTTPRoute applied"),
            Err(e) => {
                tracing::warn!(fastmock = %name, namespace = %ns, error = %e, "applying HTTPRoute failed");
                condition(
                    "RouteReady",
                    false,
                    "ApplyFailed",
                    &format!("applying HTTPRoute failed (is Gateway API installed?): {e}"),
                )
            }
        },
    )
}

fn tag_of(api: &MockApi) -> String {
    api.spec
        .tag
        .clone()
        .unwrap_or_else(|| format!("{}.{}", api.namespace().unwrap_or_default(), api.name_any()))
}

fn api_tags(tag: &str) -> [String; 3] {
    [
        tag.to_string(),
        format!("{tag}:openapi"),
        format!("{tag}:wiremock"),
    ]
}

async fn document(ctx: &Ctx, ns: &str, src: &DocumentSource) -> Result<String, Error> {
    if let Some(i) = &src.inline {
        return Ok(i.clone());
    }
    let r = src
        .config_map_ref
        .as_ref()
        .ok_or_else(|| Error::Spec("set inline or configMapRef".into()))?;
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    let cm = cms.get(&r.name).await?;
    cm.data
        .and_then(|d| d.get(&r.key).cloned())
        .ok_or_else(|| Error::Spec(format!("ConfigMap {} has no key {}", r.name, r.key)))
}

#[derive(Debug, Default)]
pub struct OpenApiInput {
    pub doc: String,
    pub path_prefix: String,
    pub validate_requests: bool,
}

#[derive(Debug, Default)]
pub struct ApiInputs {
    pub openapi: Option<OpenApiInput>,
    pub wiremock: Option<Value>,
}

async fn resolve_inputs(ctx: &Ctx, ns: &str, spec: &MockApiSpec) -> Result<ApiInputs, Error> {
    let openapi = match &spec.openapi {
        Some(o) => Some(OpenApiInput {
            doc: document(ctx, ns, &o.source).await?,
            path_prefix: o.path_prefix.clone(),
            validate_requests: o.validate_requests,
        }),
        None => None,
    };
    let wiremock = match &spec.wiremock {
        Some(w) => {
            let doc = document(ctx, ns, w).await?;
            Some(if doc.trim_start().starts_with(['{', '[']) {
                serde_json::from_str(&doc).map_err(|e| Error::Spec(format!("wiremock: {e}")))?
            } else {
                serde_yaml_ng::from_str(&doc).map_err(|e| Error::Spec(format!("wiremock: {e}")))?
            })
        }
        None => None,
    };
    Ok(ApiInputs { openapi, wiremock })
}

fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical_json(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn inputs_hash(stubs: &[Value], tag: &str, server_uid: &str, inputs: &ApiInputs) -> String {
    let v = json!({
        "version": 1,
        "tag": tag,
        "server": server_uid,
        "stubs": stubs,
        "openapi": inputs.openapi.as_ref().map(|o| json!({
            "doc": o.doc,
            "pathPrefix": o.path_prefix,
            "validateRequests": o.validate_requests,
        })),
        "wiremock": inputs.wiremock,
    });
    let mut s = String::new();
    canonical_json(&v, &mut s);
    manifests::sha256_hex(&s)
}

pub fn count_tagged(listing: &Value, tags: &[String]) -> i64 {
    listing["stubs"]
        .as_array()
        .map(|stubs| {
            stubs
                .iter()
                .filter(|s| {
                    s["tags"].as_array().is_some_and(|t| {
                        t.iter()
                            .any(|x| x.as_str().is_some_and(|x| tags.iter().any(|w| w == x)))
                    })
                })
                .count() as i64
        })
        .unwrap_or(0)
}

struct Admin<'a> {
    ctx: &'a Ctx,
    base: String,
    token: String,
}

impl Admin<'_> {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, Error> {
        let mut rb = self
            .ctx
            .http
            .request(method, format!("{}/__admin/{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.map_err(|e| Error::Admin(e.to_string()))?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::Admin(format!("{status}: {v}")));
        }
        Ok(v)
    }

    async fn delete_tag(&self, tag: &str) -> Result<(), Error> {
        let q = tag.replace('/', "%2F").replace(':', "%3A");
        self.call(reqwest::Method::DELETE, &format!("stubs?tag={q}"), None)
            .await
            .map(|_| ())
    }

    async fn count_tagged(&self, tags: &[String]) -> Result<i64, Error> {
        let v = self.call(reqwest::Method::GET, "stubs", None).await?;
        Ok(count_tagged(&v, tags))
    }
}

async fn admin_for<'a>(ctx: &'a Ctx, ns: &str, server: &str) -> Result<Option<Admin<'a>>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let Some(token) = secrets
        .get_opt(&format!("{server}-operator-token"))
        .await?
        .and_then(|s| secret_value(&s, "token"))
    else {
        return Ok(None);
    };
    let base = ctx
        .admin_url_template
        .replace("{name}", server)
        .replace("{namespace}", ns);
    Ok(Some(Admin { ctx, base, token }))
}

fn synced(st: &MockApiStatus) -> bool {
    st.conditions
        .iter()
        .any(|c| c.type_ == "Synced" && c.status == "True")
}

fn warnings_of(r: &Value) -> impl Iterator<Item = String> + '_ {
    r["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w.as_str().map(String::from))
}

async fn import_all(
    admin: &Admin<'_>,
    spec: &MockApiSpec,
    tag: &str,
    inputs: &ApiInputs,
    imported: &mut i64,
    warnings: &mut Vec<String>,
) -> Result<(), Error> {
    let [stub_tag, openapi_tag, wm_tag] = api_tags(tag);
    if spec.stubs.is_empty() {
        admin.delete_tag(&stub_tag).await?;
    } else {
        let r = admin
            .call(
                reqwest::Method::POST,
                "stubs/import",
                Some(json!({"stubs": spec.stubs, "replaceTag": stub_tag})),
            )
            .await?;
        *imported += r["imported"].as_i64().unwrap_or(0);
    }
    match &inputs.openapi {
        Some(o) => {
            let r = admin
                .call(
                    reqwest::Method::POST,
                    "import/openapi",
                    Some(json!({"spec": o.doc, "tag": openapi_tag, "pathPrefix": o.path_prefix, "validateRequests": o.validate_requests})),
                )
                .await?;
            *imported += r["imported"].as_i64().unwrap_or(0);
            warnings.extend(warnings_of(&r));
        }
        None => admin.delete_tag(&openapi_tag).await?,
    }
    match &inputs.wiremock {
        Some(parsed) => {
            let r = admin
                .call(
                    reqwest::Method::POST,
                    "import/wiremock",
                    Some(json!({"mappings": parsed, "tag": wm_tag})),
                )
                .await?;
            *imported += r["imported"].as_i64().unwrap_or(0);
            warnings.extend(warnings_of(&r));
        }
        None => admin.delete_tag(&wm_tag).await?,
    }
    Ok(())
}

async fn sync_api(api: Arc<MockApi>, ctx: &Ctx) -> Result<Action, Error> {
    let ns = api.namespace().unwrap_or_default();
    let name = api.name_any();
    let apis: Api<MockApi> = Api::namespaced(ctx.client.clone(), &ns);
    let servers: Api<FastMock> = Api::namespaced(ctx.client.clone(), &ns);
    let generation = api.metadata.generation.unwrap_or(0);
    let prev = api.status.as_ref();
    let base = prev.cloned().unwrap_or_default();
    let make = |imported: i64,
                warnings: Vec<String>,
                hash: String,
                synced_at: String,
                ok: bool,
                reason: &str,
                msg: &str| MockApiStatus {
        imported_stubs: imported,
        warnings,
        observed_generation: generation,
        last_sync_time: synced_at,
        applied_hash: hash,
        conditions: merge_conditions(&base.conditions, vec![condition("Synced", ok, reason, msg)]),
    };
    let server_name = &api.spec.server_ref.name;
    let server = servers.get_opt(server_name).await?.filter(|s| {
        s.status.as_ref().is_some_and(|st| st.ready_replicas >= 1)
            && s.metadata.deletion_timestamp.is_none()
    });
    let admin = match &server {
        Some(_) => admin_for(ctx, &ns, server_name).await?,
        None => None,
    };
    let (Some(server), Some(admin)) = (server, admin) else {
        let st = make(
            0,
            vec![],
            String::new(),
            base.last_sync_time.clone(),
            false,
            "ServerNotReady",
            &format!("FastMock {server_name} is missing or not ready"),
        );
        write_status(&apis, &name, prev, &st).await?;
        return Ok(Action::requeue(Duration::from_secs(15)));
    };
    let tag = tag_of(&api);
    let inputs = match resolve_inputs(ctx, &ns, &api.spec).await {
        Ok(i) => i,
        Err(e) => {
            let st = make(
                base.imported_stubs,
                base.warnings.clone(),
                String::new(),
                base.last_sync_time.clone(),
                false,
                "SyncFailed",
                &e.to_string(),
            );
            write_status(&apis, &name, prev, &st).await?;
            return match e {
                Error::Spec(_) => Ok(Action::requeue(RESYNC)),
                e => Err(e),
            };
        }
    };
    let hash = inputs_hash(
        &api.spec.stubs,
        &tag,
        server.metadata.uid.as_deref().unwrap_or_default(),
        &inputs,
    );
    if base.applied_hash == hash && base.observed_generation == generation && synced(&base) {
        let present = admin.count_tagged(&api_tags(&tag)).await?;
        if present >= base.imported_stubs {
            return Ok(Action::requeue(RESYNC));
        }
        tracing::info!(mockapi = %name, namespace = %ns, present, expected = base.imported_stubs, "stubs missing on the server; re-importing");
    }
    let mut imported = 0i64;
    let mut warnings: Vec<String> = vec![];
    let outcome = import_all(
        &admin,
        &api.spec,
        &tag,
        &inputs,
        &mut imported,
        &mut warnings,
    )
    .await;
    let st = match &outcome {
        Ok(()) => make(
            imported,
            warnings,
            hash,
            chrono::Utc::now().to_rfc3339(),
            true,
            "Synced",
            &format!("{imported} stubs applied with tag {tag}"),
        ),
        Err(e) => make(
            imported,
            warnings,
            String::new(),
            base.last_sync_time.clone(),
            false,
            "SyncFailed",
            &e.to_string(),
        ),
    };
    write_status(&apis, &name, prev, &st).await?;
    match outcome {
        Ok(()) => Ok(Action::requeue(RESYNC)),
        Err(Error::Spec(_)) => Ok(Action::requeue(RESYNC)),
        Err(e) => Err(e),
    }
}

async fn cleanup_api(api: Arc<MockApi>, ctx: &Ctx) -> Result<Action, Error> {
    let ns = api.namespace().unwrap_or_default();
    let name = api.name_any();
    let server_name = &api.spec.server_ref.name;
    let servers: Api<FastMock> = Api::namespaced(ctx.client.clone(), &ns);
    match servers.get_opt(server_name).await? {
        Some(s) if s.metadata.deletion_timestamp.is_none() => {}
        _ => {
            tracing::info!(mockapi = %name, namespace = %ns, server = %server_name, "FastMock is gone; releasing MockApi without stub cleanup");
            return Ok(Action::await_change());
        }
    }
    let Some(admin) = admin_for(ctx, &ns, server_name).await? else {
        tracing::info!(mockapi = %name, namespace = %ns, server = %server_name, "operator token is gone; releasing MockApi without stub cleanup");
        return Ok(Action::await_change());
    };
    for t in api_tags(&tag_of(&api)) {
        admin.delete_tag(&t).await?;
    }
    Ok(Action::await_change())
}

pub async fn reconcile_api(api: Arc<MockApi>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let start = Instant::now();
    let ns = api.namespace().unwrap_or_default();
    let apis: Api<MockApi> = Api::namespaced(ctx.client.clone(), &ns);
    let c = ctx.clone();
    let obj = api.clone();
    let r = finalizer(&apis, FINALIZER, api, |event| async move {
        match event {
            Event::Apply(a) => sync_api(a, &c).await,
            Event::Cleanup(a) => cleanup_api(a, &c).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(e.to_string()));
    observe("MockApi", start, &r);
    reset_failures(&ctx, obj.as_ref(), &r);
    r
}

fn error_policy<K: Resource<DynamicType = ()>>(obj: Arc<K>, err: &Error, ctx: Arc<Ctx>) -> Action {
    let key = object_key(obj.as_ref());
    let failures = match ctx.failures.lock() {
        Ok(mut f) => {
            let n = f.entry(key.clone()).or_insert(0);
            *n = n.saturating_add(1);
            *n
        }
        Err(_) => 1,
    };
    let delay = backoff_delay(failures);
    tracing::warn!(object = %key, error = %err, failures, retry_in_secs = delay.as_secs(), "reconcile failed; retrying");
    Action::requeue(delay)
}

pub async fn run(ctx: Arc<Ctx>, namespace: Option<String>) {
    let client = ctx.client.clone();
    let (servers, apis, deps): (Api<FastMock>, Api<MockApi>, Api<Deployment>) = match &namespace {
        Some(ns) => (
            Api::namespaced(client.clone(), ns),
            Api::namespaced(client.clone(), ns),
            Api::namespaced(client.clone(), ns),
        ),
        None => (
            Api::all(client.clone()),
            Api::all(client.clone()),
            Api::all(client.clone()),
        ),
    };
    let wc = watcher::Config::default();
    let (server_reader, server_writer) = reflector::store();
    let server_events = watcher(servers, wc.clone())
        .default_backoff()
        .reflect(server_writer)
        .applied_objects()
        .predicate_filter(trigger_hash::<FastMock>, Default::default());
    let server_ctrl = Controller::for_stream(server_events, server_reader)
        .owns(
            deps,
            watcher::Config::default().labels("app.kubernetes.io/managed-by=fastmock-operator"),
        )
        .shutdown_on_signal()
        .run(reconcile_server, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::debug!(error = %e, "FastMock controller event");
            }
        });
    let (api_reader, api_writer) = reflector::store();
    let api_events = watcher(apis, wc)
        .default_backoff()
        .reflect(api_writer)
        .applied_objects()
        .predicate_filter(trigger_hash::<MockApi>, Default::default());
    let api_ctrl = Controller::for_stream(api_events, api_reader)
        .shutdown_on_signal()
        .run(reconcile_api, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::debug!(error = %e, "MockApi controller event");
            }
        });
    tokio::join!(server_ctrl, api_ctrl);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::MockApiSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(0), Duration::from_secs(5));
        assert_eq!(backoff_delay(1), Duration::from_secs(5));
        assert_eq!(backoff_delay(2), Duration::from_secs(10));
        assert_eq!(backoff_delay(4), Duration::from_secs(40));
        assert_eq!(backoff_delay(7), Duration::from_secs(300));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(300));
    }

    #[test]
    fn hash_is_stable_and_order_independent() {
        let mut a = serde_json::Map::new();
        a.insert("request".into(), json!({"method": "GET", "urlPath": "/x"}));
        a.insert("response".into(), json!({"status": 200, "body": "ok"}));
        let mut b = serde_json::Map::new();
        b.insert("response".into(), json!({"body": "ok", "status": 200}));
        b.insert("request".into(), json!({"urlPath": "/x", "method": "GET"}));
        let inputs = ApiInputs {
            openapi: Some(OpenApiInput {
                doc: "openapi: 3.0.0".into(),
                path_prefix: "/p".into(),
                validate_requests: true,
            }),
            wiremock: None,
        };
        let h1 = inputs_hash(&[Value::Object(a)], "t", "uid", &inputs);
        let h2 = inputs_hash(&[Value::Object(b)], "t", "uid", &inputs);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        let stubs = [json!({"request": {"method": "GET"}})];
        let base = inputs_hash(&stubs, "t", "uid", &inputs);
        assert_eq!(base, inputs_hash(&stubs, "t", "uid", &inputs));
        assert_ne!(base, inputs_hash(&stubs, "t2", "uid", &inputs));
        assert_ne!(base, inputs_hash(&stubs, "t", "uid2", &inputs));
        let flipped = ApiInputs {
            openapi: Some(OpenApiInput {
                validate_requests: false,
                ..OpenApiInput {
                    doc: "openapi: 3.0.0".into(),
                    path_prefix: "/p".into(),
                    validate_requests: true,
                }
            }),
            wiremock: None,
        };
        assert_ne!(base, inputs_hash(&stubs, "t", "uid", &flipped));
        let wm = ApiInputs {
            wiremock: Some(json!({"mappings": []})),
            ..Default::default()
        };
        assert_ne!(
            inputs_hash(&stubs, "t", "uid", &ApiInputs::default()),
            inputs_hash(&stubs, "t", "uid", &wm)
        );
    }

    #[test]
    fn counts_stubs_with_any_api_tag() {
        let listing = json!({"total": 4, "stubs": [
            {"id": "1", "tags": ["ns.a"]},
            {"id": "2", "tags": ["x", "ns.a:openapi"]},
            {"id": "3", "tags": ["ns.b"]},
            {"id": "4"}
        ]});
        assert_eq!(count_tagged(&listing, &api_tags("ns.a")), 2);
        assert_eq!(count_tagged(&Value::Null, &api_tags("ns.a")), 0);
    }

    #[test]
    fn trigger_ignores_status_but_sees_finalizers_and_deletion() {
        let mut api = MockApi::new("a", MockApiSpec::default());
        api.metadata.generation = Some(1);
        let h0 = trigger_hash(&api);
        api.status = Some(MockApiStatus {
            imported_stubs: 5,
            ..Default::default()
        });
        api.metadata.resource_version = Some("2".into());
        assert_eq!(trigger_hash(&api), h0);
        api.metadata.finalizers = Some(vec![FINALIZER.into()]);
        let h1 = trigger_hash(&api);
        assert_ne!(h1, h0);
        api.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        assert_ne!(trigger_hash(&api), h1);
        let h2 = trigger_hash(&api);
        api.metadata.generation = Some(2);
        assert_ne!(trigger_hash(&api), h2);
    }

    #[test]
    fn storage_rules() {
        let mut spec = FastMockSpec {
            replicas: 1,
            storage: crate::crd::StorageSpec {
                mode: "memory".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(storage_problem(&spec).is_none());
        spec.replicas = 2;
        assert!(storage_problem(&spec).is_some());
        spec.storage.mode = "postgres".into();
        assert!(storage_problem(&spec).is_some());
    }
}
