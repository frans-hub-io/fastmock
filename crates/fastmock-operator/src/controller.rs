use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service, ServiceAccount};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, DeleteParams, DynamicObject, Patch, PatchParams, Preconditions};
use kube::core::{ApiResource, GroupVersionKind, PartialObjectMeta};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::reflector::ObjectRef;
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
    #[error("admin api: {0}")]
    AdminNotFound(String),
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
    pub space_from_namespace: bool,
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

fn oidc_problem(spec: &FastMockSpec) -> Option<&'static str> {
    let o = spec.oidc.as_ref()?;
    if !o.issuer.starts_with("https://") {
        Some("oidc.issuer must be an https URL")
    } else if o.client_id.trim().is_empty() {
        Some("oidc.clientId is required")
    } else if !o.redirect_url.starts_with("https://")
        || !o.redirect_url.ends_with("/__admin/auth/callback")
    {
        Some(
            "oidc.redirectUrl must be the public https admin URL followed by /__admin/auth/callback",
        )
    } else {
        None
    }
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

/// The CA ConfigMap of `spec.oidc`: present, labelled for mounting, with the key.
async fn check_oidc_ca(
    cms: &Api<ConfigMap>,
    spec: &FastMockSpec,
) -> Result<Option<(&'static str, String)>, Error> {
    let Some(r) = spec
        .oidc
        .as_ref()
        .and_then(|o| o.ca_config_map_ref.as_ref())
    else {
        return Ok(None);
    };
    Ok(match cms.get_opt(&r.name).await? {
        None => Some((
            "ConfigMapNotFound",
            format!(
                "referenced ConfigMap not found: oidc.caConfigMapRef={}",
                r.name
            ),
        )),
        Some(cm) if !secret_allowed(cm.metadata.labels.as_ref()) => Some((
            "ConfigMapNotAllowed",
            format!(
                "referenced ConfigMaps must carry the label {ALLOW_MOUNT_LABEL}=true: oidc.caConfigMapRef={}",
                r.name
            ),
        )),
        Some(cm) if !cm.data.as_ref().is_some_and(|d| d.contains_key(&r.key)) => Some((
            "ConfigMapNotFound",
            format!("ConfigMap {} has no key {}", r.name, r.key),
        )),
        Some(_) => None,
    })
}

/// The OIDC client secret of a `FastMock`: `Ok(None)` without `spec.oidc`, otherwise a digest of the
/// configured key's value (it goes into the pod checksum, so a rotated secret rolls the pods).
async fn oidc_client_secret(
    secrets: &Api<Secret>,
    spec: &FastMockSpec,
) -> Result<Result<Option<String>, (&'static str, String)>, Error> {
    let Some(r) = spec.oidc.as_ref().map(|o| &o.client_secret_ref) else {
        return Ok(Ok(None));
    };
    let secret = secrets.get_opt(&r.name).await?;
    Ok(oidc_secret_digest(r, secret.as_ref()).map(Some))
}

fn oidc_secret_digest(
    r: &crate::crd::OidcSecretRef,
    secret: Option<&Secret>,
) -> Result<String, (&'static str, String)> {
    let Some(secret) = secret else {
        return Err((
            "SecretNotFound",
            format!(
                "referenced Secrets not found: oidc.clientSecretRef={}",
                r.name
            ),
        ));
    };
    match secret.data.as_ref().and_then(|d| d.get(&r.key)) {
        Some(v) if !String::from_utf8_lossy(&v.0).trim().is_empty() => {
            Ok(manifests::sha256_hex_bytes(&v.0))
        }
        Some(_) => Err((
            "SecretKeyNotFound",
            format!(
                "Secret {} key {} is empty (oidc.clientSecretRef)",
                r.name, r.key
            ),
        )),
        None => Err((
            "SecretKeyNotFound",
            format!(
                "Secret {} has no key {} (oidc.clientSecretRef)",
                r.name, r.key
            ),
        )),
    }
}

/// The access file of a `FastMock`: `Ok(None)` without `spec.access`, `Err((reason, message))` when the
/// ConfigMap is missing, lacks the key or is not labelled for mounting.
async fn access_file(
    cms: &Api<ConfigMap>,
    spec: &FastMockSpec,
) -> Result<Result<Option<String>, (&'static str, String)>, Error> {
    let Some(a) = &spec.access else {
        return Ok(Ok(None));
    };
    let r = &a.config_map_ref;
    Ok(match cms.get_opt(&r.name).await? {
        None => Err((
            "ConfigMapNotFound",
            format!(
                "referenced ConfigMap not found: access.configMapRef={}",
                r.name
            ),
        )),
        Some(cm) if !secret_allowed(cm.metadata.labels.as_ref()) => Err((
            "ConfigMapNotAllowed",
            format!(
                "referenced ConfigMaps must carry the label {ALLOW_MOUNT_LABEL}=true: access.configMapRef={}",
                r.name
            ),
        )),
        Some(cm) => match cm.data.and_then(|d| d.get(&r.key).cloned()) {
            Some(doc) => Ok(Some(doc)),
            None => Err((
                "ConfigMapNotFound",
                format!("ConfigMap {} has no key {}", r.name, r.key),
            )),
        },
    })
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
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), &ns);
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
    let mut access_doc = None;
    let mut oidc_digest = None;
    let refusal: Option<(&str, String, Duration)> =
        if let Some(msg) = storage_problem(spec).or_else(|| oidc_problem(spec)) {
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
        } else if let Some((r, m)) = check_secrets(&secrets, spec).await? {
            Some((r, m, Duration::from_secs(60)))
        } else if let Some((r, m)) = check_oidc_ca(&cms, spec).await? {
            Some((r, m, Duration::from_secs(60)))
        } else {
            match oidc_client_secret(&secrets, spec).await? {
                Err((r, m)) => Some((r, m, Duration::from_secs(60))),
                Ok(d) => {
                    oidc_digest = d;
                    match access_file(&cms, spec).await? {
                        Ok(doc) => {
                            access_doc = doc;
                            None
                        }
                        Err((r, m)) => Some((r, m, Duration::from_secs(60))),
                    }
                }
            }
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
    apply(
        &cms,
        &format!("{name}-config"),
        &manifests::config_map(&name, &ns, &owner, &env),
    )
    .await?;
    let sas: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), &ns);
    apply(&sas, &name, &manifests::service_account(&name, &ns, &owner)).await?;
    let checksum =
        manifests::config_checksum(&env, &tokens, access_doc.as_deref(), oidc_digest.as_deref());
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
    pub space: Option<String>,
}

/// The space a MockApi's stubs go to: `spec.space`, or the namespace when the operator forces
/// spaces from namespaces (a different `spec.space` is then an error).
pub fn effective_space(
    from_namespace: bool,
    ns: &str,
    requested: Option<&str>,
) -> Result<Option<String>, Error> {
    match (from_namespace, requested) {
        (true, Some(s)) if s != ns => Err(Error::Spec(format!(
            "spec.space must be empty or {ns}: the operator puts the stubs of a namespace into the space of the same name"
        ))),
        (true, _) => Ok(Some(ns.to_string())),
        (false, s) => Ok(s.map(String::from)),
    }
}

fn space_param(space: Option<&str>) -> String {
    space.map(|s| format!("space={s}")).unwrap_or_default()
}

async fn resolve_inputs(ctx: &Ctx, ns: &str, spec: &MockApiSpec) -> Result<ApiInputs, Error> {
    let space = effective_space(ctx.space_from_namespace, ns, spec.space.as_deref())?;
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
    Ok(ApiInputs {
        openapi,
        wiremock,
        space,
    })
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
    let mut v = v;
    if let Some(space) = &inputs.space {
        v["space"] = json!(space);
    }
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
    http: &'a reqwest::Client,
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
            .http
            .request(method, format!("{}/__admin/{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.map_err(|e| Error::Admin(e.to_string()))?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::AdminNotFound(format!("{status}: {v}")));
        }
        if !status.is_success() {
            return Err(Error::Admin(format!("{status}: {v}")));
        }
        Ok(v)
    }

    /// Deletes the stubs with this tag in every space (the operator token is a global admin and the
    /// tag belongs to one MockApi); 404 counts as done.
    async fn delete_tag(&self, tag: &str) -> Result<(), Error> {
        let q = tag.replace('/', "%2F").replace(':', "%3A");
        match self
            .call(reqwest::Method::DELETE, &format!("stubs?tag={q}"), None)
            .await
        {
            Ok(_) | Err(Error::AdminNotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn delete_api_stubs(&self, tag: &str) -> Result<(), Error> {
        for t in api_tags(tag) {
            self.delete_tag(&t).await?;
        }
        Ok(())
    }

    /// Whether the server defines the space; a server without `/spaces` has none.
    async fn space_exists(&self, space: &str) -> Result<bool, Error> {
        match self.call(reqwest::Method::GET, "spaces", None).await {
            Ok(v) => Ok(v["spaces"]
                .as_array()
                .is_some_and(|l| l.iter().any(|s| s["name"] == space))),
            Err(Error::AdminNotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The instance id of the server process answering; `None` for servers that do not report one.
    async fn instance(&self) -> Result<Option<String>, Error> {
        match self.call(reqwest::Method::GET, "info", None).await {
            Ok(v) => Ok(v["instanceId"].as_str().map(String::from)),
            Err(Error::AdminNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn count_tagged(&self, tags: &[String], space: Option<&str>) -> Result<i64, Error> {
        let s = space_param(space);
        let path = if s.is_empty() {
            "stubs".to_string()
        } else {
            format!("stubs?{s}")
        };
        let v = self.call(reqwest::Method::GET, &path, None).await?;
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
    Ok(Some(Admin {
        http: &ctx.http,
        base,
        token,
    }))
}

/// Whether a Deployment has finished rolling out: every pod runs the current template and is ready, and
/// no pod of an earlier template is left. Only then does the admin Service reach the one in-memory server
/// that data-plane requests reach too.
pub fn deployment_settled(d: &Deployment) -> bool {
    let want = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let Some(st) = &d.status else {
        return false;
    };
    st.observed_generation.unwrap_or(0) >= d.metadata.generation.unwrap_or(0)
        && st.replicas.unwrap_or(0) == want
        && st.updated_replicas.unwrap_or(0) == want
        && st.ready_replicas.unwrap_or(0) == want
        && st.unavailable_replicas.unwrap_or(0) == 0
}

async fn rollout_settled(ctx: &Ctx, ns: &str, server: &str) -> Result<bool, Error> {
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);
    Ok(deployments
        .get_opt(server)
        .await?
        .is_some_and(|d| deployment_settled(&d)))
}

/// How soon an import into a newly started in-memory server is checked again. A rollout can finish while
/// the Service still routes to the old pod for a moment; the check catches stubs that went there.
const VERIFY_AFTER_IMPORT: Duration = Duration::from_secs(5);

fn space_not_found_message(space: &str, server: &str) -> String {
    format!(
        "space {space} is not defined on FastMock {server}: add it to the server's access file or choose another space; the stubs were left unchanged"
    )
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
    let space = inputs.space.as_deref();
    let q = match space_param(space) {
        s if s.is_empty() => s,
        s => format!("?{s}"),
    };
    if spec.stubs.is_empty() {
        admin.delete_tag(&stub_tag).await?;
    } else {
        let r = admin
            .call(
                reqwest::Method::POST,
                &format!("stubs/import{q}"),
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
                    &format!("import/openapi{q}"),
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
                    &format!("import/wiremock{q}"),
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
        space: base.space.clone(),
        server_instance: base.server_instance.clone(),
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
    let in_memory = server.spec.storage.mode == "memory";
    let mut instance = None;
    if in_memory {
        if !rollout_settled(ctx, &ns, server_name).await? {
            let st = make(
                base.imported_stubs,
                base.warnings.clone(),
                base.applied_hash.clone(),
                base.last_sync_time.clone(),
                false,
                "ServerRestarting",
                &format!(
                    "FastMock {server_name} is rolling out; its in-memory stubs are imported again once the new pod is the only one"
                ),
            );
            write_status(&apis, &name, prev, &st).await?;
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
        instance = admin.instance().await?;
    }
    let new_instance = instance
        .as_ref()
        .is_some_and(|i| *i != base.server_instance);
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
    let mut space_found = true;
    if base.applied_hash == hash && base.observed_generation == generation && synced(&base) {
        if new_instance {
            tracing::info!(mockapi = %name, namespace = %ns, server = %server_name, "in-memory server restarted; re-importing")
        } else {
            match admin
                .count_tagged(&api_tags(&tag), inputs.space.as_deref())
                .await
            {
                Ok(present) if present >= base.imported_stubs => {
                    return Ok(Action::requeue(RESYNC));
                }
                Ok(present) => {
                    tracing::info!(mockapi = %name, namespace = %ns, present, expected = base.imported_stubs, "stubs missing on the server; re-importing")
                }
                Err(Error::AdminNotFound(_)) if inputs.space.is_some() => space_found = false,
                Err(e) => return Err(e),
            }
        }
    }
    if let Some(space) = &inputs.space
        && !(space_found && admin.space_exists(space).await?)
    {
        tracing::warn!(mockapi = %name, namespace = %ns, %space, "space not defined on the server; stubs left unchanged");
        let st = MockApiStatus {
            applied_hash: base.applied_hash.clone(),
            ..make(
                base.imported_stubs,
                base.warnings.clone(),
                String::new(),
                base.last_sync_time.clone(),
                false,
                "SpaceNotFound",
                &space_not_found_message(space, server_name),
            )
        };
        write_status(&apis, &name, prev, &st).await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }
    let mut imported = 0i64;
    let mut warnings: Vec<String> = vec![];
    if !base.applied_hash.is_empty() && base.space != inputs.space {
        admin.delete_api_stubs(&tag).await?;
    }
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
        Ok(()) => MockApiStatus {
            space: inputs.space.clone(),
            server_instance: instance.clone().unwrap_or_default(),
            ..make(
                imported,
                warnings,
                hash,
                chrono::Utc::now().to_rfc3339(),
                true,
                "Synced",
                &match &inputs.space {
                    Some(s) => format!("{imported} stubs applied with tag {tag} in space {s}"),
                    None => format!("{imported} stubs applied with tag {tag}"),
                },
            )
        },
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
        Ok(()) if new_instance => Ok(Action::requeue(VERIFY_AFTER_IMPORT)),
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
    admin.delete_api_stubs(&tag_of(&api)).await?;
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

/// The FastMocks whose pods depend on this opted-in ConfigMap, so that its edits apply without
/// waiting for the resync. Secrets are not watched: a rotated Secret is picked up by the next
/// reconcile (the resync, or any change of the FastMock).
pub fn servers_using(servers: &[Arc<FastMock>], ns: &str, name: &str) -> Vec<ObjectRef<FastMock>> {
    servers
        .iter()
        .filter(|s| s.namespace().as_deref() == Some(ns))
        .filter(|s| {
            let spec = &s.spec;
            spec.access
                .as_ref()
                .is_some_and(|a| a.config_map_ref.name == name)
                || spec
                    .oidc
                    .as_ref()
                    .and_then(|o| o.ca_config_map_ref.as_ref())
                    .is_some_and(|c| c.name == name)
        })
        .map(|s| ObjectRef::from_obj(s.as_ref()))
        .collect()
}

/// The MockApis of the FastMock whose Deployment changed. Pod restarts and rollouts show up there, so an
/// in-memory server gets its stubs back within seconds instead of at the resync.
pub fn apis_using(apis: &[Arc<MockApi>], ns: &str, server: &str) -> Vec<ObjectRef<MockApi>> {
    apis.iter()
        .filter(|a| a.namespace().as_deref() == Some(ns) && a.spec.server_ref.name == server)
        .map(|a| ObjectRef::from_obj(a.as_ref()))
        .collect()
}

fn api_in<K>(client: &Client, namespace: &Option<String>) -> Api<K>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>,
    <K as Resource>::DynamicType: Default,
{
    match namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

pub async fn run(ctx: Arc<Ctx>, namespace: Option<String>) {
    let client = ctx.client.clone();
    let servers: Api<FastMock> = api_in(&client, &namespace);
    let apis: Api<MockApi> = api_in(&client, &namespace);
    let deps: Api<Deployment> = api_in(&client, &namespace);
    let managed =
        watcher::Config::default().labels("app.kubernetes.io/managed-by=fastmock-operator");
    let server_deployments = watcher(
        api_in::<PartialObjectMeta<Deployment>>(&client, &namespace),
        managed.clone(),
    )
    .default_backoff()
    .touched_objects();
    let mounted = watcher::Config::default().labels(&format!("{ALLOW_MOUNT_LABEL}=true"));
    let config_maps = watcher(
        api_in::<PartialObjectMeta<ConfigMap>>(&client, &namespace),
        mounted,
    )
    .default_backoff()
    .touched_objects();
    let wc = watcher::Config::default();
    let (server_reader, server_writer) = reflector::store();
    let server_events = watcher(servers, wc.clone())
        .default_backoff()
        .reflect(server_writer)
        .applied_objects()
        .predicate_filter(trigger_hash::<FastMock>, Default::default());
    let cm_refs = server_reader.clone();
    let server_ctrl = Controller::for_stream(server_events, server_reader)
        .owns(deps, managed)
        .watches_stream(config_maps, move |cm: PartialObjectMeta<ConfigMap>| {
            servers_using(
                &cm_refs.state(),
                &cm.namespace().unwrap_or_default(),
                &cm.name_any(),
            )
        })
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
    let api_refs = api_reader.clone();
    let api_ctrl = Controller::for_stream(api_events, api_reader)
        .watches_stream(
            server_deployments,
            move |d: PartialObjectMeta<Deployment>| {
                apis_using(
                    &api_refs.state(),
                    &d.namespace().unwrap_or_default(),
                    &d.name_any(),
                )
            },
        )
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
            space: None,
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
            space: None,
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
    fn space_comes_from_the_spec_or_the_namespace() {
        assert_eq!(effective_space(false, "team-a", None).unwrap(), None);
        assert_eq!(
            effective_space(false, "team-a", Some("payments")).unwrap(),
            Some("payments".into())
        );
        assert_eq!(
            effective_space(true, "team-a", None).unwrap(),
            Some("team-a".into())
        );
        assert_eq!(
            effective_space(true, "team-a", Some("team-a")).unwrap(),
            Some("team-a".into())
        );
        assert!(matches!(
            effective_space(true, "team-a", Some("payments")),
            Err(Error::Spec(_))
        ));
        let stubs = [json!({"request": {"method": "GET"}})];
        let global = inputs_hash(&stubs, "t", "uid", &ApiInputs::default());
        let spaced = ApiInputs {
            space: Some("team-a".into()),
            ..Default::default()
        };
        assert_ne!(global, inputs_hash(&stubs, "t", "uid", &spaced));
        assert_eq!(space_param(Some("team-a")), "space=team-a");
        assert_eq!(space_param(None), "");
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

    type Responder = Arc<dyn Fn(&str, &Value) -> (u16, Value) + Send + Sync>;
    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    const NS: &str = "team";
    const SERVER: &str = "srv";

    async fn mock(respond: Responder) -> (String, Calls) {
        let calls: Calls = Arc::new(Mutex::new(vec![]));
        let log = calls.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let respond = respond.clone();
            let log = log.clone();
            async move {
                let call = format!(
                    "{} {}",
                    req.method(),
                    req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
                );
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let (status, v) = respond(&call, &body);
                log.lock().unwrap().push((call, body));
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(v),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), calls)
    }

    fn test_ctx(base: &str) -> Ctx {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::try_from(kube::Config::new(base.parse().unwrap())).unwrap();
        Ctx {
            client,
            http: reqwest::Client::new(),
            admin_url_template: base.to_string(),
            images: ImagePolicy::new("fastmock:1", ""),
            space_from_namespace: false,
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn not_found() -> (u16, Value) {
        (
            404,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "message": "not found", "reason": "NotFound", "code": 404}),
        )
    }

    fn server_json() -> Value {
        json!({"apiVersion": "fastmock.franshub.io/v1alpha1", "kind": "FastMock",
            "metadata": {"name": SERVER, "namespace": NS, "uid": "srv-uid"},
            "spec": {}, "status": {"readyReplicas": 1}})
    }

    fn mockapi_json() -> Value {
        json!({"apiVersion": "fastmock.franshub.io/v1alpha1", "kind": "MockApi",
            "metadata": {"name": "a", "namespace": NS}, "spec": {"serverRef": {"name": SERVER}}})
    }

    fn deployment_json(replicas: i32, updated: i32, ready: i32) -> Value {
        json!({"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": SERVER, "namespace": NS, "generation": 2},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": SERVER}}, "template": {}},
            "status": {"observedGeneration": 2, "replicas": replicas, "updatedReplicas": updated,
                "readyReplicas": ready, "availableReplicas": ready}})
    }

    fn kube_objects(call: &str) -> Option<(u16, Value)> {
        let fm = format!("/apis/fastmock.franshub.io/v1alpha1/namespaces/{NS}");
        if call == format!("GET /apis/apps/v1/namespaces/{NS}/deployments/{SERVER}") {
            Some((200, deployment_json(1, 1, 1)))
        } else if call == format!("GET {fm}/fastmocks/{SERVER}") {
            Some((200, server_json()))
        } else if call == format!("GET /api/v1/namespaces/{NS}/secrets/{SERVER}-operator-token") {
            Some((
                200,
                json!({"apiVersion": "v1", "kind": "Secret",
                    "metadata": {"name": format!("{SERVER}-operator-token"), "namespace": NS},
                    "data": {"token": "dG9r"}}),
            ))
        } else if call.starts_with(&format!("PATCH {fm}/mockapis/a/status")) {
            Some((200, mockapi_json()))
        } else if call.starts_with(&format!("PATCH {fm}/fastmocks/{SERVER}/status")) {
            Some((200, server_json()))
        } else {
            None
        }
    }

    fn mockapi(space: Option<&str>, status: MockApiStatus) -> MockApi {
        let mut api = MockApi::new(
            "a",
            MockApiSpec {
                server_ref: crate::crd::ServerRef {
                    name: SERVER.into(),
                },
                stubs: vec![
                    json!({"request": {"method": "GET", "urlPath": "/x"}, "response": {"status": 200}}),
                ],
                space: space.map(String::from),
                ..Default::default()
            },
        );
        api.metadata.namespace = Some(NS.into());
        api.metadata.generation = Some(1);
        api.status = Some(status);
        api
    }

    fn synced_status(hash: &str, space: Option<&str>) -> MockApiStatus {
        MockApiStatus {
            imported_stubs: 1,
            observed_generation: 1,
            last_sync_time: "2026-01-01T00:00:00Z".into(),
            applied_hash: hash.into(),
            space: space.map(String::from),
            conditions: vec![condition("Synced", true, "Synced", "1 stubs applied")],
            ..Default::default()
        }
    }

    fn admin_calls(calls: &Calls) -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| c.clone())
            .filter(|c| c.contains("/__admin/"))
            .collect()
    }

    fn written_status(calls: &Calls, kind: &str) -> Value {
        calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(c, _)| {
                c.starts_with("PATCH") && c.contains(&format!("/{kind}/")) && c.contains("/status")
            })
            .map(|(_, b)| b["status"].clone())
            .unwrap_or(Value::Null)
    }

    fn condition_of(status: &Value, type_: &str) -> Value {
        status["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["type"] == type_)
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn cleanup_releases_a_mockapi_whose_space_is_gone() {
        let (base, calls) = mock(Arc::new(|call: &str, _: &Value| {
            kube_objects(call).unwrap_or_else(|| {
                if call.starts_with("DELETE /__admin/stubs") && call.contains("space=") {
                    (404, json!({"error": "space gone not found"}))
                } else if call.starts_with("DELETE /__admin/stubs") {
                    (200, json!({"deleted": 1}))
                } else {
                    not_found()
                }
            })
        }))
        .await;
        let ctx = test_ctx(&base);
        let api = mockapi(Some("gone"), synced_status("h", Some("gone")));
        cleanup_api(Arc::new(api), &ctx).await.unwrap();
        let deletes = admin_calls(&calls);
        assert_eq!(deletes.len(), 3, "{deletes:?}");
        assert!(
            deletes
                .iter()
                .all(|c| c.starts_with("DELETE") && !c.contains("space=")),
            "{deletes:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_treats_404_from_the_admin_api_as_done() {
        let (base, _) = mock(Arc::new(|call: &str, _: &Value| {
            kube_objects(call).unwrap_or_else(|| (404, json!({"error": "not found"})))
        }))
        .await;
        let ctx = test_ctx(&base);
        let api = mockapi(None, synced_status("h", None));
        cleanup_api(Arc::new(api), &ctx).await.unwrap();
    }

    #[tokio::test]
    async fn space_change_to_a_missing_space_keeps_the_old_stubs() {
        let (base, calls) = mock(Arc::new(|call: &str, _: &Value| {
            kube_objects(call).unwrap_or_else(|| {
                if call == "GET /__admin/spaces" {
                    (200, json!({"spaces": [{"name": "a"}]}))
                } else if call.contains("space=b") {
                    (404, json!({"error": "space b not found"}))
                } else if call.starts_with("DELETE /__admin/stubs") {
                    (200, json!({"deleted": 1}))
                } else {
                    not_found()
                }
            })
        }))
        .await;
        let ctx = test_ctx(&base);
        let api = mockapi(Some("b"), synced_status("old", Some("a")));
        sync_api(Arc::new(api), &ctx).await.unwrap();
        let admin = admin_calls(&calls);
        assert!(
            admin
                .iter()
                .all(|c| !c.starts_with("DELETE") && !c.starts_with("POST")),
            "{admin:?}"
        );
        let st = written_status(&calls, "mockapis");
        let synced = condition_of(&st, "Synced");
        assert_eq!(synced["status"], "False", "{st}");
        assert_eq!(synced["reason"], "SpaceNotFound", "{st}");
        assert!(
            synced["message"].as_str().unwrap().contains("space b"),
            "{st}"
        );
        assert_eq!(st["appliedHash"], "old", "{st}");
        assert_eq!(st["space"], "a", "{st}");
    }

    #[tokio::test]
    async fn space_change_deletes_old_stubs_even_when_the_old_space_is_gone() {
        let (base, calls) = mock(Arc::new(|call: &str, _: &Value| {
            kube_objects(call).unwrap_or_else(|| {
                if call == "GET /__admin/spaces" {
                    (200, json!({"spaces": [{"name": "b"}]}))
                } else if call.contains("space=a") {
                    (404, json!({"error": "space a not found"}))
                } else if call.starts_with("DELETE /__admin/stubs") {
                    (200, json!({"deleted": 1}))
                } else if call.starts_with("POST /__admin/stubs/import?space=b") {
                    (200, json!({"imported": 1}))
                } else {
                    not_found()
                }
            })
        }))
        .await;
        let ctx = test_ctx(&base);
        let api = mockapi(Some("b"), synced_status("old", Some("a")));
        sync_api(Arc::new(api), &ctx).await.unwrap();
        let admin = admin_calls(&calls);
        let deletes: Vec<&String> = admin.iter().filter(|c| c.starts_with("DELETE")).collect();
        assert!(deletes.len() >= 3, "{admin:?}");
        assert!(deletes.iter().all(|c| !c.contains("space=")), "{admin:?}");
        let import = admin
            .iter()
            .position(|c| c.starts_with("POST /__admin/stubs/import?space=b"))
            .expect("import into the new space");
        let first_delete = admin.iter().position(|c| c.starts_with("DELETE")).unwrap();
        assert!(first_delete < import, "{admin:?}");
        let st = written_status(&calls, "mockapis");
        assert_eq!(condition_of(&st, "Synced")["status"], "True", "{st}");
        assert_eq!(st["space"], "b", "{st}");
    }

    #[tokio::test]
    async fn removed_space_is_not_left_synced() {
        let (base, calls) = mock(Arc::new(|call: &str, _: &Value| {
            kube_objects(call).unwrap_or_else(|| {
                if call == "GET /__admin/spaces" {
                    (200, json!({"spaces": []}))
                } else if call.contains("space=a") {
                    (404, json!({"error": "space a not found"}))
                } else {
                    not_found()
                }
            })
        }))
        .await;
        let ctx = test_ctx(&base);
        let spec_api = mockapi(Some("a"), MockApiStatus::default());
        let inputs = ApiInputs {
            space: Some("a".into()),
            ..Default::default()
        };
        let hash = inputs_hash(&spec_api.spec.stubs, &tag_of(&spec_api), "srv-uid", &inputs);
        let api = mockapi(Some("a"), synced_status(&hash, Some("a")));
        sync_api(Arc::new(api), &ctx).await.unwrap();
        let st = written_status(&calls, "mockapis");
        let synced = condition_of(&st, "Synced");
        assert_eq!(synced["status"], "False", "{st}");
        assert_eq!(synced["reason"], "SpaceNotFound", "{st}");
        assert!(
            admin_calls(&calls)
                .iter()
                .all(|c| !c.starts_with("DELETE") && !c.starts_with("POST"))
        );
    }

    fn unchanged_hash() -> String {
        let api = mockapi(None, MockApiStatus::default());
        inputs_hash(
            &api.spec.stubs,
            &tag_of(&api),
            "srv-uid",
            &ApiInputs::default(),
        )
    }

    fn listing_with_the_stub() -> (u16, Value) {
        (200, json!({"stubs": [{"id": "s1", "tags": ["team.a"]}]}))
    }

    #[test]
    fn deployment_changes_map_to_the_mockapis_of_their_server() {
        let mine = Arc::new(mockapi(None, MockApiStatus::default()));
        let mut other_server = mockapi(None, MockApiStatus::default());
        other_server.metadata.name = Some("b".into());
        other_server.spec.server_ref.name = "other".into();
        let mut other_ns = mockapi(None, MockApiStatus::default());
        other_ns.metadata.name = Some("c".into());
        other_ns.metadata.namespace = Some("elsewhere".into());
        let apis = vec![mine.clone(), Arc::new(other_server), Arc::new(other_ns)];
        assert_eq!(
            apis_using(&apis, NS, SERVER),
            vec![ObjectRef::from_obj(mine.as_ref())]
        );
        assert!(apis_using(&apis, NS, "nobody").is_empty());
    }

    #[test]
    fn a_rollout_settles_when_only_ready_current_pods_are_left() {
        let d = |v: Value| serde_json::from_value::<Deployment>(v).unwrap();
        assert!(deployment_settled(&d(deployment_json(1, 1, 1))));
        assert!(
            !deployment_settled(&d(deployment_json(2, 1, 2))),
            "old pod left"
        );
        assert!(
            !deployment_settled(&d(deployment_json(1, 0, 1))),
            "old template"
        );
        assert!(
            !deployment_settled(&d(deployment_json(1, 1, 0))),
            "not ready"
        );
        let mut stale = deployment_json(1, 1, 1);
        stale["status"]["observedGeneration"] = json!(1);
        assert!(!deployment_settled(&d(stale)), "new spec not observed yet");
        let mut none = deployment_json(1, 1, 1);
        none["status"] = Value::Null;
        assert!(!deployment_settled(&d(none)));
    }

    #[tokio::test]
    async fn in_memory_server_mid_rollout_is_not_reported_synced() {
        let (base, calls) = mock(Arc::new(|call: &str, _: &Value| {
            if call == format!("GET /apis/apps/v1/namespaces/{NS}/deployments/{SERVER}") {
                return (200, deployment_json(2, 1, 2));
            }
            kube_objects(call).unwrap_or_else(|| {
                if call == "GET /__admin/stubs" {
                    listing_with_the_stub()
                } else {
                    not_found()
                }
            })
        }))
        .await;
        let ctx = test_ctx(&base);
        let hash = unchanged_hash();
        let mut status = synced_status(&hash, None);
        status.server_instance = "old".into();
        let action = sync_api(Arc::new(mockapi(None, status)), &ctx)
            .await
            .unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(60)));
        let st = written_status(&calls, "mockapis");
        let synced = condition_of(&st, "Synced");
        assert_eq!(synced["status"], "False", "{st}");
        assert_eq!(synced["reason"], "ServerRestarting", "{st}");
        assert_eq!(st["appliedHash"], json!(hash), "{st}");
        assert!(
            admin_calls(&calls)
                .iter()
                .all(|c| !c.starts_with("POST") && !c.starts_with("DELETE")),
            "nothing is imported while two pods answer"
        );
    }

    #[tokio::test]
    async fn restarted_in_memory_server_gets_its_stubs_again() {
        for (reported, reimport) in [("new", true), ("same", false)] {
            let (base, calls) = mock(Arc::new(move |call: &str, _: &Value| {
                kube_objects(call).unwrap_or_else(|| match call {
                    "GET /__admin/info" => (200, json!({"instanceId": reported})),
                    "GET /__admin/stubs" => listing_with_the_stub(),
                    c if c.starts_with("POST /__admin/stubs/import") => {
                        (200, json!({"imported": 1}))
                    }
                    c if c.starts_with("DELETE /__admin/stubs") => (200, json!({"deleted": 0})),
                    _ => not_found(),
                })
            }))
            .await;
            let ctx = test_ctx(&base);
            let mut status = synced_status(&unchanged_hash(), None);
            status.server_instance = "same".into();
            let action = sync_api(Arc::new(mockapi(None, status)), &ctx)
                .await
                .unwrap();
            let imported = admin_calls(&calls)
                .iter()
                .any(|c| c.starts_with("POST /__admin/stubs/import"));
            assert_eq!(imported, reimport, "{reported}");
            if reimport {
                assert_eq!(action, Action::requeue(VERIFY_AFTER_IMPORT));
                let st = written_status(&calls, "mockapis");
                assert_eq!(condition_of(&st, "Synced")["status"], "True", "{st}");
                assert_eq!(st["serverInstance"], "new", "{st}");
            } else {
                assert_eq!(action, Action::requeue(RESYNC));
            }
        }
    }

    #[tokio::test]
    async fn missing_stubs_on_the_same_instance_are_imported_without_a_fast_retry() {
        let (base, calls) = mock(Arc::new(|call: &str, _: &Value| {
            kube_objects(call).unwrap_or_else(|| match call {
                "GET /__admin/info" => (200, json!({"instanceId": "same"})),
                "GET /__admin/stubs" => (200, json!({"stubs": []})),
                c if c.starts_with("POST /__admin/stubs/import") => (200, json!({"imported": 1})),
                c if c.starts_with("DELETE /__admin/stubs") => (200, json!({"deleted": 0})),
                _ => not_found(),
            })
        }))
        .await;
        let ctx = test_ctx(&base);
        let mut status = synced_status(&unchanged_hash(), None);
        status.server_instance = "same".into();
        let action = sync_api(Arc::new(mockapi(None, status)), &ctx)
            .await
            .unwrap();
        assert!(
            admin_calls(&calls)
                .iter()
                .any(|c| c.starts_with("POST /__admin/stubs/import"))
        );
        assert_eq!(action, Action::requeue(RESYNC));
    }

    fn oidc_server(secret_key: &str) -> FastMock {
        let mut fm = FastMock::new(
            SERVER,
            FastMockSpec {
                replicas: 1,
                storage: crate::crd::StorageSpec {
                    mode: "memory".into(),
                    ..Default::default()
                },
                oidc: Some(crate::crd::OidcSpec {
                    issuer: "https://sso.example.com/realms/mocks".into(),
                    client_id: "fastmock".into(),
                    client_secret_ref: crate::crd::OidcSecretRef {
                        name: "sso".into(),
                        key: secret_key.into(),
                    },
                    redirect_url: "https://mock-admin.example.com/__admin/auth/callback".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        fm.metadata.namespace = Some(NS.into());
        fm.metadata.uid = Some("srv-uid".into());
        fm.metadata.generation = Some(1);
        fm
    }

    fn sso_secret(data: Value) -> Value {
        json!({"apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": "sso", "namespace": NS, "labels": {ALLOW_MOUNT_LABEL: "true"}},
            "data": data})
    }

    fn echo_kube(call: &str, body: &Value, sso: &Value) -> (u16, Value) {
        if call == format!("GET /api/v1/namespaces/{NS}/secrets/sso") {
            return (200, sso.clone());
        }
        if let Some(r) = kube_objects(call) {
            return r;
        }
        if call.starts_with("PATCH") {
            (200, body.clone())
        } else {
            not_found()
        }
    }

    #[tokio::test]
    async fn oidc_secret_without_the_key_is_refused_with_a_clear_status() {
        let sso = sso_secret(json!({"other": "eA=="}));
        let (base, calls) = mock(Arc::new(move |call: &str, body: &Value| {
            echo_kube(call, body, &sso)
        }))
        .await;
        let ctx = test_ctx(&base);
        reconcile_server_inner(&oidc_server("client-secret"), &ctx)
            .await
            .unwrap();
        let st = written_status(&calls, "fastmocks");
        let ready = condition_of(&st, "Ready");
        assert_eq!(ready["status"], "False", "{st}");
        assert_eq!(ready["reason"], "SecretKeyNotFound", "{st}");
        assert!(
            ready["message"].as_str().unwrap().contains("client-secret"),
            "{st}"
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|(c, _)| !c.contains("/deployments/")),
            "no Deployment may be applied"
        );
    }

    async fn applied_checksum(secret_value_b64: &str) -> String {
        let sso = sso_secret(json!({"client-secret": secret_value_b64}));
        let (base, calls) = mock(Arc::new(move |call: &str, body: &Value| {
            echo_kube(call, body, &sso)
        }))
        .await;
        let ctx = test_ctx(&base);
        reconcile_server_inner(&oidc_server("client-secret"), &ctx)
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        let (_, d) = calls
            .iter()
            .find(|(c, _)| c.starts_with("PATCH") && c.contains("/deployments/"))
            .expect("Deployment applied");
        d["spec"]["template"]["metadata"]["annotations"]["fastmock.franshub.io/config-checksum"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn rotating_the_oidc_client_secret_rolls_the_pods() {
        let a = applied_checksum("b25l").await;
        let b = applied_checksum("dHdv").await;
        assert_eq!(a, applied_checksum("b25l").await);
        assert_ne!(a, b);
    }

    #[test]
    fn mounted_config_maps_map_to_their_servers() {
        let mut with_access = oidc_server("client-secret");
        with_access.spec.access = Some(crate::crd::AccessSpec {
            config_map_ref: crate::crd::AccessConfigMapRef {
                name: "team-access".into(),
                key: "access.yaml".into(),
            },
        });
        let mut other_ns = with_access.clone();
        other_ns.metadata.namespace = Some("elsewhere".into());
        let mut plain = FastMock::new("plain", FastMockSpec::default());
        plain.metadata.namespace = Some(NS.into());
        let all = [Arc::new(with_access), Arc::new(other_ns), Arc::new(plain)];
        let names = |name: &str| -> Vec<(String, Option<String>)> {
            servers_using(&all, NS, name)
                .into_iter()
                .map(|r| (r.name, r.namespace))
                .collect()
        };
        let srv = vec![(SERVER.to_string(), Some(NS.to_string()))];
        assert_eq!(names("team-access"), srv);
        assert!(names("sso").is_empty());
        assert!(names("unrelated").is_empty());
    }

    #[test]
    fn empty_oidc_client_secret_is_refused() {
        let r = crate::crd::OidcSecretRef {
            name: "sso".into(),
            key: "client-secret".into(),
        };
        let secret: Secret =
            serde_json::from_value(sso_secret(json!({"client-secret": "IA=="}))).unwrap();
        assert_eq!(
            oidc_secret_digest(&r, Some(&secret)).unwrap_err().0,
            "SecretKeyNotFound"
        );
        assert_eq!(
            oidc_secret_digest(&r, None).unwrap_err().0,
            "SecretNotFound"
        );
    }

    #[test]
    fn oidc_rules() {
        let ok = crate::crd::OidcSpec {
            issuer: "https://sso.example.com/realms/mocks".into(),
            client_id: "fastmock".into(),
            redirect_url: "https://mock-admin.example.com/__admin/auth/callback".into(),
            ..Default::default()
        };
        let with = |o: crate::crd::OidcSpec| FastMockSpec {
            oidc: Some(o),
            ..Default::default()
        };
        assert!(oidc_problem(&FastMockSpec::default()).is_none());
        assert!(oidc_problem(&with(ok.clone())).is_none());
        for bad in [
            crate::crd::OidcSpec {
                issuer: "http://sso.example.com".into(),
                ..ok.clone()
            },
            crate::crd::OidcSpec {
                client_id: " ".into(),
                ..ok.clone()
            },
            crate::crd::OidcSpec {
                redirect_url: "https://mock-admin.example.com/".into(),
                ..ok.clone()
            },
        ] {
            assert!(oidc_problem(&with(bad)).is_some());
        }
    }
}
