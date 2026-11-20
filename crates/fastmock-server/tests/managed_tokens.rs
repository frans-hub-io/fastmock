use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use fastmock_core::ApiToken;
use fastmock_server::auth::{generate_token, hash_token};
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_token_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_token_tests_0123456789";
const LEAD: &str = "fm_lead_token_for_token_tests_0123456789";
const ED: &str = "fm_ed_token_for_token_tests_0123456789";

const ACCESS: &str = r#"
roles:
  - name: team-lead
    permissions: [stubs:read, stubs:write, imports:run, journal:read, journal:delete, files:read, files:write,
      callbacks:read, callbacks:delete, state:read, state:write, contracts:read, contracts:details,
      contracts:write, tokens:read, tokens:write]
spaces:
  - name: alpha
  - name: beta
bindings:
  - subject: token:lead
    role: team-lead
    spaces: [alpha]
  - subject: token:ed
    role: editor
    spaces: [alpha]
"#;

/// Everything the tests in this binary log, at debug level. A process-wide subscriber, because
/// callsite interest cached by other test threads would hide events from a scoped one.
fn logs() -> &'static LogBuf {
    static L: OnceLock<LogBuf> = OnceLock::new();
    L.get_or_init(|| {
        let buf = LogBuf::default();
        let w = buf.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || w.clone())
            .init();
        buf
    })
}

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    logs();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn static_tokens() -> Vec<String> {
    vec![
        format!("ops:admin:{}", hash_token(ADMIN)),
        format!("dash:viewer:{}", hash_token(VIEWER)),
        format!("lead::{}", hash_token(LEAD)),
        format!("ed::{}", hash_token(ED)),
    ]
}

async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_tok_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("tok:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn storages() -> Vec<(&'static str, StorageKind)> {
    let mut v = vec![("memory", StorageKind::Memory)];
    match pg_storage().await {
        Some(p) => v.push(("postgres+valkey", p)),
        None => eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: PostgreSQL+Valkey part skipped"
        ),
    }
    v
}

#[allow(dead_code)]
struct H {
    admin: String,
    http: reqwest::Client,
    app: AppState,
    _dir: std::path::PathBuf,
    _shutdown: watch::Sender<bool>,
}

impl H {
    async fn start(storage: StorageKind, ttl: Option<Duration>) -> H {
        let mut cfg = Config::for_tests(storage, static_tokens());
        cfg.plugins = Some(vec![]);
        if let Some(t) = ttl {
            cfg.token_cache_ttl = t;
        }
        let dir = std::env::temp_dir().join(format!("fm-tok-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("access.yaml");
        std::fs::write(&f, ACCESS).unwrap();
        cfg.access_file = Some(f);
        let (app, _data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, _rx) = watch::channel(false);
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let aa = al.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        H {
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            app,
            _dir: dir,
            _shutdown: tx,
        }
    }

    async fn raw(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, String) {
        let mut rb = self
            .http
            .request(m, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn call(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let (st, text) = self.raw(m, path, token, body).await;
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn ok(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> Value {
        let (st, v) = self.call(m.clone(), path, token, body).await;
        assert!((200..300).contains(&st), "{m} {path}: {st} {v}");
        v
    }

    /// Creates a token and returns its value and id.
    async fn create(&self, by: &str, body: Value) -> (String, String) {
        let (st, v) = self.call(Method::POST, "/tokens", by, Some(body)).await;
        assert_eq!(st, 201, "{v}");
        (
            v["token"].as_str().unwrap().to_string(),
            v["id"].as_str().unwrap().to_string(),
        )
    }

    async fn status_of(&self, token: &str) -> u16 {
        self.call(Method::GET, "/info", token, None).await.0
    }

    async fn metrics(&self) -> String {
        self.http
            .get(format!("{}/metrics", self.admin))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }
}

fn stored(name: &str, value: &str, expires_in: chrono::Duration) -> ApiToken {
    let now = Utc::now();
    ApiToken {
        id: uuid::Uuid::now_v7(),
        name: name.into(),
        sha256: hash_token(value),
        role: "viewer".into(),
        spaces: vec![],
        global: true,
        description: None,
        created_by: "test".into(),
        created_at: now,
        expires_at: now + expires_in,
        last_used_at: None,
        revoked_at: None,
        revoked: false,
        parent_id: None,
    }
}

fn names(v: &Value) -> Vec<String> {
    let mut n: Vec<String> = v["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    n.sort();
    n
}

#[tokio::test]
async fn create_use_list_self_expire_and_revoke() {
    for (name, storage) in storages().await {
        let h = H::start(storage, None).await;
        let v = h
            .ok(
                Method::POST,
                "/tokens",
                ADMIN,
                Some(json!({"name": "ci-alpha", "role": "editor", "spaces": ["alpha"], "expiresInDays": 30, "description": "CI of team alpha"})),
            )
            .await;
        let value = v["token"].as_str().unwrap().to_string();
        let id = v["id"].as_str().unwrap().to_string();
        assert!(
            value.starts_with("fm_") && value.len() == 67,
            "{name}: {value}"
        );
        assert_eq!(v["status"], "active", "{name}");
        assert_eq!(v["createdBy"], "ops", "{name}");
        assert_eq!(v["spaces"], json!(["alpha"]), "{name}");
        assert_eq!(v["global"], false, "{name}");
        assert_eq!(v["lastUsedAt"], Value::Null, "{name}");
        let expires: chrono::DateTime<Utc> = v["expiresAt"].as_str().unwrap().parse().unwrap();
        let days = (expires - Utc::now()).num_hours() as f64 / 24.0;
        assert!((29.9..=30.0).contains(&days), "{name}: {days}");

        let info = h.ok(Method::GET, "/info", &value, None).await;
        assert_eq!(info["principal"]["name"], "ci-alpha", "{name}");
        assert_eq!(info["principal"]["kind"], "apiToken", "{name}");
        assert!(
            info["principal"]["permissions"]["spaces"]["alpha"]
                .as_array()
                .unwrap()
                .contains(&json!("stubs:write")),
            "{name}: {info}"
        );
        let stub = h
            .ok(
                Method::POST,
                "/stubs",
                &value,
                Some(json!({"request": {"urlPath": "/alpha/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(stub["space"], "alpha", "{name}");
        let (st, _) = h
            .call(
                Method::POST,
                "/stubs?space=beta",
                &value,
                Some(json!({"request": {"urlPath": "/b"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 404, "{name}");
        let own = h.ok(Method::GET, "/tokens/self", &value, None).await;
        assert_eq!(own["id"], json!(id), "{name}");
        assert!(own.get("token").is_none(), "{name}");
        let (st, _) = h.call(Method::GET, "/tokens/self", ADMIN, None).await;
        assert_eq!(st, 404, "{name}: static tokens have no managed metadata");

        let list = h.ok(Method::GET, "/tokens", ADMIN, None).await;
        assert_eq!(names(&list), vec!["ci-alpha"], "{name}");
        let one = h
            .ok(Method::GET, &format!("/tokens/{id}"), ADMIN, None)
            .await;
        assert_eq!(one["description"], "CI of team alpha", "{name}");
        let (st, v) = h.call(Method::GET, "/tokens", VIEWER, None).await;
        assert_eq!((st, v["required"].clone()), (403, json!("admin")), "{name}");
        let used = async {
            for _ in 0..50 {
                let v = h
                    .ok(Method::GET, &format!("/tokens/{id}"), ADMIN, None)
                    .await;
                if v["lastUsedAt"].is_string() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        };
        assert!(used.await, "{name}: lastUsedAt is recorded");

        for (body, status, msg) in [
            (
                json!({"name": "ci-alpha", "role": "viewer", "global": true}),
                409,
                "an active token named ci-alpha exists",
            ),
            (
                json!({"name": "ops", "role": "viewer", "global": true}),
                409,
                "static token",
            ),
            (
                json!({"name": "x", "role": "viewer", "global": true, "expiresInDays": 366}),
                400,
                "between 1 and 365",
            ),
            (
                json!({"name": "x", "role": "viewer", "global": true, "expiresInDays": 0}),
                400,
                "between 1 and 365",
            ),
            (json!({"name": "x", "role": "viewer"}), 400, "set spaces"),
            (
                json!({"name": "x", "role": "viewer", "global": true, "spaces": ["alpha"]}),
                400,
                "not both",
            ),
            (
                json!({"name": "x", "role": "root", "global": true}),
                400,
                "unknown role",
            ),
            (
                json!({"name": "x", "role": "viewer", "spaces": ["nope"]}),
                404,
                "space nope not found",
            ),
            (
                json!({"name": "bad name", "role": "viewer", "global": true}),
                400,
                "name must be",
            ),
        ] {
            let (st, v) = h
                .call(Method::POST, "/tokens", ADMIN, Some(body.clone()))
                .await;
            assert_eq!(st, status, "{name}: {body} {v}");
            assert!(
                v["error"].as_str().unwrap_or_default().contains(msg),
                "{name}: {body} {v}"
            );
        }
        let (_, id90) = h
            .create(
                ADMIN,
                json!({"name": "default-expiry", "role": "viewer", "global": true}),
            )
            .await;
        let v = h
            .ok(Method::GET, &format!("/tokens/{id90}"), ADMIN, None)
            .await;
        let expires: chrono::DateTime<Utc> = v["expiresAt"].as_str().unwrap().parse().unwrap();
        assert_eq!((expires - Utc::now()).num_hours() / 24, 89, "{name}");

        let v = h
            .ok(Method::DELETE, &format!("/tokens/{id}"), ADMIN, None)
            .await;
        assert_eq!(v["status"], "revoked", "{name}");
        assert!(v["revokedAt"].is_string(), "{name}");
        let (st, v) = h.call(Method::GET, "/info", &value, None).await;
        assert_eq!(
            st, 401,
            "{name}: revoked tokens stop working at once on this replica"
        );
        assert_eq!(v["details"], json!([]), "{name}");
        let v = h
            .ok(Method::DELETE, &format!("/tokens/{id}"), ADMIN, None)
            .await;
        assert_eq!(
            v["status"], "revoked",
            "{name}: revoking again changes nothing"
        );
        let (again, _) = h
            .create(
                ADMIN,
                json!({"name": "ci-alpha", "role": "viewer", "global": true}),
            )
            .await;
        assert_eq!(
            h.status_of(&again).await,
            200,
            "{name}: name reusable after revocation"
        );
        let list = h
            .ok(Method::GET, "/tokens?status=revoked", ADMIN, None)
            .await;
        assert_eq!(names(&list), vec!["ci-alpha"], "{name}");
        let list = h
            .ok(Method::GET, "/tokens?status=active", ADMIN, None)
            .await;
        assert_eq!(names(&list), vec!["ci-alpha", "default-expiry"], "{name}");

        let (selfish, self_id) = h
            .create(
                ADMIN,
                json!({"name": "selfish", "role": "viewer", "global": true}),
            )
            .await;
        let v = h
            .ok(
                Method::DELETE,
                &format!("/tokens/{self_id}"),
                &selfish,
                None,
            )
            .await;
        assert_eq!(v["status"], "revoked", "{name}");
        assert_eq!(h.status_of(&selfish).await, 401, "{name}");

        let soon = generate_token();
        assert!(
            h.app
                .tokens
                .store()
                .create(
                    &stored("short-lived", &soon, chrono::Duration::milliseconds(1500)),
                    Utc::now()
                )
                .await
                .unwrap()
        );
        assert_eq!(h.status_of(&soon).await, 200, "{name}");
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let (st, v) = h.call(Method::GET, "/info", &soon, None).await;
        assert_eq!(st, 401, "{name}: expiry is exact even with a cached token");
        assert_eq!(v["details"], json!(["token expired"]), "{name}");
        let old = generate_token();
        h.app
            .tokens
            .store()
            .create(&stored("old", &old, chrono::Duration::days(-1)), Utc::now())
            .await
            .unwrap();
        assert_eq!(h.status_of(&old).await, 401, "{name}");
        let list = h
            .ok(Method::GET, "/tokens?status=expired", ADMIN, None)
            .await;
        assert_eq!(names(&list), vec!["old", "short-lived"], "{name}");
        let (_, renewed) = h
            .create(
                ADMIN,
                json!({"name": "old", "role": "viewer", "global": true}),
            )
            .await;
        assert!(!renewed.is_empty(), "{name}: expired names are reusable");

        let audit = h
            .ok(Method::GET, "/audit?action=token.*", ADMIN, None)
            .await;
        let events = audit["events"].as_array().unwrap();
        let created = events
            .iter()
            .find(|e| e["action"] == "token.create" && e["detail"]["id"] == json!(id))
            .unwrap_or_else(|| panic!("{name}: {audit}"));
        assert_eq!(created["principal"], "ops", "{name}");
        assert_eq!(created["principalKind"], "token", "{name}");
        assert_eq!(created["space"], "alpha", "{name}");
        assert_eq!(created["detail"]["name"], "ci-alpha", "{name}");
        assert_eq!(created["detail"]["kind"], "apiToken", "{name}");
        assert_eq!(created["detail"]["role"], "editor", "{name}");
        assert_eq!(created["detail"]["spaces"], json!(["alpha"]), "{name}");
        let revoked = events
            .iter()
            .filter(|e| e["action"] == "token.revoke")
            .collect::<Vec<_>>();
        assert_eq!(revoked.len(), 2, "{name}: revoking twice is audited once");
        let by_self = revoked
            .iter()
            .find(|e| e["detail"]["self"] == true)
            .unwrap();
        assert_eq!(by_self["principal"], "selfish", "{name}");
        assert_eq!(by_self["principalKind"], "apiToken", "{name}");

        let m = h.metrics().await;
        assert!(
            m.contains("fastmock_admin_auth_failures_total{reason=\"expired\"}"),
            "{name}"
        );
        assert!(m.contains("fastmock_api_tokens_active"), "{name}");
    }
}

#[tokio::test]
async fn no_escalation_through_tokens() {
    for (name, storage) in storages().await {
        let h = H::start(storage, None).await;
        let (st, v) = h
            .call(
                Method::POST,
                "/tokens",
                ED,
                Some(json!({"name": "e1", "role": "viewer", "spaces": ["alpha"]})),
            )
            .await;
        assert_eq!(
            (st, v["required"].clone()),
            (403, json!("tokens:write")),
            "{name}: {v}"
        );

        let (lead_editor, lead_editor_id) = h
            .create(
                LEAD,
                json!({"name": "alpha-ci", "role": "editor", "spaces": ["alpha"]}),
            )
            .await;
        assert_eq!(h.status_of(&lead_editor).await, 200, "{name}");
        for (body, required) in [
            (
                json!({"name": "a1", "role": "admin", "spaces": ["alpha"]}),
                "contracts:secrets",
            ),
            (
                json!({"name": "a4", "role": "viewer", "global": true}),
                "tokens:write",
            ),
        ] {
            let (st, v) = h
                .call(Method::POST, "/tokens", LEAD, Some(body.clone()))
                .await;
            assert_eq!(st, 403, "{name}: {body} {v}");
            assert_eq!(v["required"], required, "{name}: {body} {v}");
        }
        for spaces in [json!(["beta"]), json!(["alpha", "beta"])] {
            let (st, v) = h
                .call(
                    Method::POST,
                    "/tokens",
                    LEAD,
                    Some(json!({"name": "a2", "role": "viewer", "spaces": spaces})),
                )
                .await;
            assert_eq!(st, 404, "{name}: a space the caller cannot see: {v}");
        }
        let (st, v) = h
            .call(
                Method::POST,
                "/tokens",
                &lead_editor,
                Some(json!({"name": "e2", "role": "viewer", "spaces": ["alpha"]})),
            )
            .await;
        assert_eq!(st, 403, "{name}: an editor token cannot create tokens: {v}");
        let (deputy, _) = h
            .create(
                LEAD,
                json!({"name": "deputy", "role": "team-lead", "spaces": ["alpha"]}),
            )
            .await;
        let (st, v) = h
            .call(
                Method::POST,
                "/tokens",
                &deputy,
                Some(json!({"name": "d1", "role": "admin", "spaces": ["alpha"]})),
            )
            .await;
        assert_eq!(st, 403, "{name}: {v}");
        h.create(
            &deputy,
            json!({"name": "d2", "role": "viewer", "spaces": ["alpha"]}),
        )
        .await;

        let (_, global_id) = h
            .create(
                ADMIN,
                json!({"name": "root", "role": "admin", "global": true}),
            )
            .await;
        let (_, beta_id) = h
            .create(
                ADMIN,
                json!({"name": "beta-admin", "role": "admin", "spaces": ["beta"]}),
            )
            .await;
        let (_, both_id) = h
            .create(
                ADMIN,
                json!({"name": "both", "role": "viewer", "spaces": ["alpha", "beta"]}),
            )
            .await;

        let mine = h.ok(Method::GET, "/tokens", LEAD, None).await;
        assert_eq!(names(&mine), vec!["alpha-ci", "d2", "deputy"], "{name}");
        let all = h.ok(Method::GET, "/tokens", ADMIN, None).await;
        assert_eq!(
            names(&all),
            vec!["alpha-ci", "beta-admin", "both", "d2", "deputy", "root"],
            "{name}"
        );
        let beta = h.ok(Method::GET, "/tokens?space=beta", ADMIN, None).await;
        assert_eq!(names(&beta), vec!["beta-admin", "both"], "{name}");
        let (st, _) = h.call(Method::GET, "/tokens?space=beta", LEAD, None).await;
        assert_eq!(st, 404, "{name}");
        for id in [&global_id, &beta_id, &both_id] {
            let (st, _) = h
                .call(Method::GET, &format!("/tokens/{id}"), LEAD, None)
                .await;
            assert_eq!(st, 404, "{name}");
            let (st, _) = h
                .call(Method::DELETE, &format!("/tokens/{id}"), LEAD, None)
                .await;
            assert_eq!(st, 404, "{name}");
        }
        let (st, v) = h
            .call(
                Method::DELETE,
                &format!("/tokens/{global_id}"),
                &lead_editor,
                None,
            )
            .await;
        assert_eq!(st, 403, "{name}: {v}");
        let (st, _) = h
            .call(Method::DELETE, &format!("/tokens/{global_id}"), ED, None)
            .await;
        assert_eq!(st, 403, "{name}");
        h.ok(
            Method::DELETE,
            &format!("/tokens/{lead_editor_id}"),
            LEAD,
            None,
        )
        .await;
        assert_eq!(h.status_of(&lead_editor).await, 401, "{name}");
        h.ok(Method::DELETE, &format!("/tokens/{both_id}"), ADMIN, None)
            .await;
        let (beta_admin, _) = h
            .create(
                ADMIN,
                json!({"name": "beta-admin-2", "role": "admin", "spaces": ["beta"]}),
            )
            .await;
        let (st, v) = h
            .call(
                Method::POST,
                "/tokens",
                &beta_admin,
                Some(json!({"name": "b1", "role": "admin", "global": true})),
            )
            .await;
        assert_eq!(
            st, 403,
            "{name}: a space admin cannot create a global token: {v}"
        );
        h.create(
            &beta_admin,
            json!({"name": "b2", "role": "admin", "spaces": ["beta"]}),
        )
        .await;
    }
}

fn grantable(v: &Value) -> Vec<(String, bool, Vec<String>)> {
    v["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["global"].as_bool().unwrap(),
                r["spaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect()
}

#[tokio::test]
async fn token_options_follow_the_escalation_rule() {
    let h = H::start(StorageKind::Memory, None).await;
    let all = vec!["alpha".to_string(), "beta".to_string()];
    let admin = h.ok(Method::GET, "/tokens/options", ADMIN, None).await;
    assert_eq!(admin["maxDays"], 365);
    assert_eq!(admin["defaultDays"], 90);
    assert_eq!(
        grantable(&admin),
        vec![
            ("viewer".to_string(), true, all.clone()),
            ("editor".to_string(), true, all.clone()),
            ("admin".to_string(), true, all.clone()),
            ("team-lead".to_string(), true, all.clone()),
        ]
    );
    let roles = admin["roles"].as_array().unwrap();
    assert_eq!(roles[0]["builtin"], true);
    assert_eq!(roles[3]["builtin"], false);
    assert!(
        roles[3]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("tokens:write"))
    );

    let lead = h.ok(Method::GET, "/tokens/options", LEAD, None).await;
    let alpha = vec!["alpha".to_string()];
    assert_eq!(
        grantable(&lead),
        vec![
            ("viewer".to_string(), false, alpha.clone()),
            ("editor".to_string(), false, alpha.clone()),
            ("admin".to_string(), false, vec![]),
            ("team-lead".to_string(), false, alpha.clone()),
        ]
    );
    for (role, space) in [("viewer", "alpha"), ("team-lead", "alpha")] {
        h.create(
            LEAD,
            json!({"name": format!("opt-{role}"), "role": role, "spaces": [space]}),
        )
        .await;
    }
    let (st, _) = h
        .call(
            Method::POST,
            "/tokens",
            LEAD,
            Some(json!({"name": "opt-admin", "role": "admin", "spaces": ["alpha"]})),
        )
        .await;
    assert_eq!(st, 403);

    let (st, v) = h.call(Method::GET, "/tokens/options", ED, None).await;
    assert_eq!(
        (st, v["required"].clone()),
        (403, json!("tokens:write")),
        "{v}"
    );
    let (st, _) = h.call(Method::GET, "/tokens/options", VIEWER, None).await;
    assert_eq!(st, 403);
}

#[tokio::test]
async fn revocation_reaches_other_replicas_within_the_cache_ttl() {
    let Some(storage) = pg_storage().await else {
        eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: two-replica test skipped"
        );
        return;
    };
    assert_eq!(
        Config::for_tests(StorageKind::Memory, vec![]).token_cache_ttl,
        Duration::from_secs(10)
    );
    let ttl = Duration::from_secs(2);
    let a = H::start(storage.clone(), Some(ttl)).await;
    let b = H::start(storage, Some(ttl)).await;
    let (value, id) = a
        .create(
            ADMIN,
            json!({"name": "shared", "role": "editor", "global": true}),
        )
        .await;
    assert_eq!(b.status_of(&value).await, 200);
    let v = b
        .ok(Method::GET, &format!("/tokens/{id}"), ADMIN, None)
        .await;
    assert_eq!(v["name"], "shared");
    let (_, v) = b
        .call(
            Method::POST,
            "/tokens",
            ADMIN,
            Some(json!({"name": "shared", "role": "viewer", "global": true})),
        )
        .await;
    assert!(v["error"].as_str().unwrap().contains("active token"), "{v}");

    a.ok(Method::DELETE, &format!("/tokens/{id}"), ADMIN, None)
        .await;
    let revoked_at = Instant::now();
    assert_eq!(a.status_of(&value).await, 401);
    loop {
        if b.status_of(&value).await == 401 {
            break;
        }
        assert!(
            revoked_at.elapsed() < ttl + Duration::from_secs(1),
            "replica b still accepts the revoked token after {:?}",
            revoked_at.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for _ in 0..5 {
        assert_eq!(b.status_of(&value).await, 401);
    }

    let soon = generate_token();
    a.app
        .tokens
        .store()
        .create(
            &stored("brief", &soon, chrono::Duration::milliseconds(1200)),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(a.status_of(&soon).await, 200);
    assert_eq!(b.status_of(&soon).await, 200);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(a.status_of(&soon).await, 401);
    assert_eq!(b.status_of(&soon).await, 401);

    let unknown = generate_token();
    assert_eq!(b.status_of(&unknown).await, 401);
    a.app
        .tokens
        .store()
        .create(
            &stored("late", &unknown, chrono::Duration::days(1)),
            Utc::now(),
        )
        .await
        .unwrap();
    let created = Instant::now();
    while b.status_of(&unknown).await != 200 {
        assert!(
            created.elapsed() < ttl + Duration::from_secs(1),
            "negative cache entries expire"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn token_values_never_appear_in_logs_audit_or_later_responses() {
    for (name, storage) in storages().await {
        let h = H::start(storage, None).await;
        let (st, created) = h
            .raw(
                Method::POST,
                "/tokens",
                ADMIN,
                Some(json!({"name": "secret-keeper", "role": "editor", "spaces": ["alpha"]})),
            )
            .await;
        assert_eq!(st, 201, "{name}: {created}");
        let v: Value = serde_json::from_str(&created).unwrap();
        let value = v["token"].as_str().unwrap().to_string();
        let id = v["id"].as_str().unwrap().to_string();
        let hash = hash_token(&value);
        let mut later = Vec::new();
        for (m, path, token) in [
            (Method::GET, "/info".to_string(), value.as_str()),
            (Method::GET, "/tokens/self".to_string(), value.as_str()),
            (Method::GET, "/audit".to_string(), value.as_str()),
            (Method::GET, "/stubs?space=beta".to_string(), value.as_str()),
            (Method::GET, "/tokens".to_string(), ADMIN),
            (Method::GET, format!("/tokens/{id}"), ADMIN),
            (Method::DELETE, format!("/tokens/{id}"), ADMIN),
            (Method::GET, "/info".to_string(), value.as_str()),
            (Method::GET, "/tokens?status=revoked".to_string(), ADMIN),
            (Method::GET, "/audit".to_string(), ADMIN),
        ] {
            later.push(h.raw(m, &path, token, None).await);
        }
        assert_eq!(later[3].0, 404, "{name}");
        assert_eq!(later[7].0, 401, "{name}");
        assert!(later[9].1.contains("token.revoke"), "{name}");
        for (st, body) in &later {
            assert!(!body.contains(&value), "{name}: {st} {body}");
            assert!(!body.contains(&hash), "{name}: {st} {body}");
            assert!(!body.contains(&value[3..]), "{name}: {st} {body}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let logs = String::from_utf8_lossy(&logs().0.lock().unwrap()).to_string();
        assert!(logs.contains("token.create"), "{name}: logs captured");
        assert!(logs.contains("secret-keeper"), "{name}: logs captured");
        assert!(
            logs.contains("token revoked") || logs.contains("missing or invalid token"),
            "{name}"
        );
        assert!(!logs.contains(&value[3..]), "{name}: token value logged");
    }
}

#[tokio::test]
async fn tokens_created_by_tokens_never_outlive_or_survive_their_parent() {
    for (name, storage) in storages().await {
        let h = H::start(storage, None).await;
        let (parent, pid) = h
            .create(
                ADMIN,
                json!({"name": "parent", "role": "admin", "global": true, "expiresInDays": 1}),
            )
            .await;
        let parent_exp: chrono::DateTime<Utc> = serde_json::from_value(
            h.ok(Method::GET, &format!("/tokens/{pid}"), ADMIN, None)
                .await["expiresAt"]
                .clone(),
        )
        .unwrap();
        let (st, child) = h
            .call(
                Method::POST,
                "/tokens",
                &parent,
                Some(
                    json!({"name": "child", "role": "admin", "global": true, "expiresInDays": 365}),
                ),
            )
            .await;
        assert_eq!(st, 201, "{name}: {child}");
        let child_exp: chrono::DateTime<Utc> =
            serde_json::from_value(child["expiresAt"].clone()).unwrap();
        assert!(
            child_exp <= parent_exp,
            "{name}: child expires {child_exp}, parent {parent_exp}"
        );
        let child = child["token"].as_str().unwrap().to_string();
        let (grandchild, _) = h
            .create(
                &child,
                json!({"name": "grandchild", "role": "viewer", "global": true}),
            )
            .await;
        let (other, _) = h
            .create(
                ADMIN,
                json!({"name": "other", "role": "viewer", "global": true}),
            )
            .await;
        for t in [&parent, &child, &grandchild, &other] {
            assert_eq!(h.status_of(t).await, 200, "{name}");
        }
        h.ok(Method::DELETE, &format!("/tokens/{pid}"), ADMIN, None)
            .await;
        for t in [&parent, &child, &grandchild] {
            assert_eq!(h.status_of(t).await, 401, "{name}: revoked with the parent");
        }
        assert_eq!(h.status_of(&other).await, 200, "{name}");
        let v = h
            .ok(Method::GET, "/tokens?status=revoked", ADMIN, None)
            .await;
        assert_eq!(names(&v), ["child", "grandchild", "parent"], "{name}");
    }
}

/// While the store holds no tokens, other replicas answer unknown values without a lookup for up to
/// the cache TTL; the creating replica accepts its token at once, the others within the TTL.
#[tokio::test]
async fn a_first_token_reaches_other_replicas_within_the_cache_ttl() {
    let Some(storage) = pg_storage().await else {
        eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: two-replica test skipped"
        );
        return;
    };
    let ttl = Duration::from_secs(2);
    let a = H::start(storage.clone(), Some(ttl)).await;
    let b = H::start(storage, Some(ttl)).await;
    assert_eq!(b.status_of(&generate_token()).await, 401);
    assert_eq!(a.status_of(&generate_token()).await, 401);
    let (value, _) = a
        .create(
            ADMIN,
            json!({"name": "first", "role": "viewer", "global": true}),
        )
        .await;
    let created = Instant::now();
    assert_eq!(
        a.status_of(&value).await,
        200,
        "the creating replica at once"
    );
    while b.status_of(&value).await != 200 {
        assert!(
            created.elapsed() < ttl + Duration::from_secs(1),
            "replica b still rejects the new token after {:?}",
            created.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
