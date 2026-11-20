use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use chrono::Utc;
use fastmock_core::ApiToken;
use fastmock_server::auth::{generate_token, hash_token};
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_all_spaces_tests_0123456789";
const EDITOR: &str = "fm_editor_token_for_all_spaces_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_all_spaces_tests_0123456789";
const ALLV: &str = "fm_allv_token_for_all_spaces_tests_0123456789";
const ALLE: &str = "fm_alle_token_for_all_spaces_tests_0123456789";
const ALLA: &str = "fm_alla_token_for_all_spaces_tests_0123456789";
const MIXED: &str = "fm_mixed_token_for_all_spaces_tests_0123456789";
const LEAD: &str = "fm_lead_token_for_all_spaces_tests_0123456789";
const PERSPACE: &str = "fm_perspace_token_for_all_spaces_tests_0123456789";
const MINTER: &str = "fm_minter_token_for_all_spaces_tests_0123456789";
const SPADMIN: &str = "fm_spadmin_token_for_all_spaces_tests_0123456789";
const AUDITOR: &str = "fm_auditor_token_for_all_spaces_tests_0123456789";

const ACCESS: &str = r#"
roles:
  - name: team-lead
    permissions: [stubs:read, stubs:write, imports:run, journal:read, journal:delete, files:read, files:write,
      callbacks:read, callbacks:delete, state:read, state:write, contracts:read, contracts:details,
      contracts:write, tokens:read, tokens:write]
  - name: minter
    permissions: [stubs:read, tokens:write]
  - name: auditor
    permissions: ["access:*", "info:read"]
spaces:
  - name: beta
    hosts: [beta.example.com]
  - name: alpha
    pathPrefixes: [/alpha/]
bindings:
  - subject: token:allv
    role: viewer
    spaces: ["*"]
  - subject: token:alle
    role: editor
    spaces: ["*"]
  - subject: token:alla
    role: admin
    spaces: ["*"]
  - subject: token:mixed
    role: viewer
    spaces: ["*"]
  - subject: token:mixed
    role: editor
    spaces: [alpha]
  - subject: token:lead
    role: team-lead
    spaces: ["*"]
  - subject: token:perspace
    role: team-lead
    spaces: [alpha, beta]
  - subject: token:minter
    role: minter
    spaces: ["*"]
  - subject: token:spadmin
    role: admin
    spaces: [alpha]
  - subject: group:auditors
    role: auditor
    global: true
  - subject: token:auditor
    role: auditor
    global: true
"#;

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn legacy_tokens() -> Vec<String> {
    vec![
        format!("ops:admin:{}", hash_token(ADMIN)),
        format!("ci:editor:{}", hash_token(EDITOR)),
        format!("dash:viewer:{}", hash_token(VIEWER)),
    ]
}

fn tokens() -> Vec<String> {
    let mut v = legacy_tokens();
    for (name, value) in [
        ("allv", ALLV),
        ("alle", ALLE),
        ("alla", ALLA),
        ("mixed", MIXED),
        ("lead", LEAD),
        ("perspace", PERSPACE),
        ("minter", MINTER),
        ("spadmin", SPADMIN),
        ("auditor", AUDITOR),
    ] {
        v.push(format!("{name}::{}", hash_token(value)));
    }
    v
}

async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_all_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("all:{}:", uuid::Uuid::new_v4().simple()),
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

fn access_file(content: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fm-all-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("access.yaml");
    std::fs::write(&f, content).unwrap();
    f
}

#[allow(dead_code)]
struct H {
    admin: String,
    http: reqwest::Client,
    app: AppState,
    file: Option<std::path::PathBuf>,
    _shutdown: watch::Sender<bool>,
}

impl H {
    async fn start(storage: StorageKind, tokens: Vec<String>, access: Option<&str>) -> H {
        let mut cfg = Config::for_tests(storage, tokens);
        cfg.plugins = Some(vec![]);
        cfg.access_file = access.map(access_file);
        let file = cfg.access_file.clone();
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
            file,
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

    async fn create_token(&self, by: &str, body: Value) -> (String, Value) {
        let (st, v) = self.call(Method::POST, "/tokens", by, Some(body)).await;
        assert_eq!(st, 201, "{v}");
        (v["token"].as_str().unwrap().to_string(), v)
    }

    async fn stub(&self, token: &str, query: &str, path: &str) -> (u16, Value) {
        let mut request = json!({"method": "GET", "urlPath": path});
        if query.contains("space=beta") {
            request["host"] = json!({"equalTo": "beta.example.com"});
        }
        self.call(
            Method::POST,
            &format!("/stubs{query}"),
            token,
            Some(json!({"request": request, "response": {"status": 200}})),
        )
        .await
    }
}

fn strings(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

fn stub_spaces(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v["stubs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["space"].as_str().unwrap_or("<global>").to_string())
        .collect();
    out.sort();
    out
}

fn viewer_space_permissions() -> Vec<String> {
    let mut v: Vec<String> = [
        "stubs:read",
        "journal:read",
        "files:read",
        "callbacks:read",
        "state:read",
        "contracts:read",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    v.sort();
    v
}

#[tokio::test]
async fn all_spaces_bindings_grant_every_space_but_no_global_data() {
    for (name, storage) in storages().await {
        let h = H::start(storage, tokens(), Some(ACCESS)).await;
        let info = h.ok(Method::GET, "/info", ALLV, None).await;
        let perms = &info["principal"]["permissions"];
        assert_eq!(perms["global"], json!(["info:read"]), "{name}: {info}");
        assert_eq!(
            strings(&perms["allSpaces"]),
            viewer_space_permissions(),
            "{name}"
        );
        assert_eq!(
            perms["spaces"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"],
            "{name}: every configured space is listed"
        );
        assert_eq!(perms["spaces"]["beta"], perms["allSpaces"], "{name}");
        let admin_info = h.ok(Method::GET, "/info", ADMIN, None).await;
        assert_eq!(
            admin_info["principal"]["permissions"]["allSpaces"],
            json!([]),
            "{name}"
        );

        let spaces = h.ok(Method::GET, "/spaces", ALLV, None).await;
        let listed: Vec<&str> = spaces["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(listed, vec!["alpha", "beta"], "{name}");

        let (st, v) = h.stub(ALLE, "", "/alpha/x").await;
        assert_eq!(st, 400, "{name}: several spaces are bound: {v}");
        assert!(
            v["error"].as_str().unwrap().contains("set space"),
            "{name}: {v}"
        );
        let (st, v) = h.stub(ALLE, "?space=alpha", "/alpha/x").await;
        assert_eq!((st, v["space"].clone()), (201, json!("alpha")), "{name}");
        let (st, v) = h.stub(ALLE, "?space=beta", "/beta/x").await;
        assert_eq!(st, 201, "{name}: {v}");
        let (st, _) = h.stub(ADMIN, "", "/global").await;
        assert_eq!(st, 201, "{name}");

        let v = h.ok(Method::GET, "/stubs", ALLV, None).await;
        assert_eq!(
            stub_spaces(&v),
            vec!["alpha", "beta"],
            "{name}: data without a space is not visible"
        );
        let v = h.ok(Method::GET, "/stubs?space=beta", ALLV, None).await;
        assert_eq!(stub_spaces(&v), vec!["beta"], "{name}");
        let v = h.ok(Method::GET, "/stubs", ADMIN, None).await;
        assert_eq!(stub_spaces(&v), vec!["<global>", "alpha", "beta"], "{name}");
        let (st, _) = h.stub(ALLV, "?space=alpha", "/alpha/y").await;
        assert_eq!(st, 403, "{name}: viewer through *");
        let (st, _) = h
            .call(Method::PUT, "/kv/k", ALLE, Some(json!({"value": "global"})))
            .await;
        assert_eq!(st, 400, "{name}: several spaces are bound");
        let (st, _) = h.call(Method::GET, "/requests/unmatched", ALLV, None).await;
        assert_eq!(st, 403, "{name}: global journal routes are not granted");

        for (m, path, required) in [
            (Method::GET, "/audit", "audit:read"),
            (Method::POST, "/reset", "system:reset"),
            (Method::DELETE, "/stubs", "stubs:purge"),
            (Method::GET, "/access", "access:read"),
            (Method::GET, "/chaos", "chaos:read"),
        ] {
            let (st, v) = h.call(m.clone(), path, ALLA, None).await;
            assert_eq!(
                (st, v["required"].clone()),
                (403, json!(required)),
                "{name}: {m} {path} with admin through *: {v}"
            );
        }
        let (st, v) = h.stub(ALLA, "?space=beta", "/beta/z").await;
        assert_eq!(st, 201, "{name}: {v}");

        let (st, v) = h.stub(MIXED, "", "/alpha/m").await;
        assert_eq!(
            (st, v["space"].clone()),
            (201, json!("alpha")),
            "{name}: the only space where the caller writes"
        );
        let (st, _) = h.stub(MIXED, "?space=beta", "/beta/m").await;
        assert_eq!(st, 403, "{name}");
        let info = h.ok(Method::GET, "/info", MIXED, None).await;
        let perms = &info["principal"]["permissions"];
        assert_eq!(
            strings(&perms["allSpaces"]),
            viewer_space_permissions(),
            "{name}"
        );
        assert!(
            perms["spaces"]["alpha"]
                .as_array()
                .unwrap()
                .contains(&json!("stubs:write")),
            "{name}"
        );
        assert!(
            !perms["spaces"]["beta"]
                .as_array()
                .unwrap()
                .contains(&json!("stubs:write")),
            "{name}"
        );
    }
}

#[tokio::test]
async fn all_spaces_with_a_single_space_needs_no_space_parameter() {
    let h = H::start(
        StorageKind::Memory,
        vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("alle::{}", hash_token(ALLE)),
        ],
        Some("spaces: [{name: only}]\nbindings: [{subject: 'token:alle', role: editor, spaces: ['*']}]"),
    )
    .await;
    let (st, v) = h.stub(ALLE, "", "/x").await;
    assert_eq!((st, v["space"].clone()), (201, json!("only")), "{v}");
}

#[tokio::test]
async fn bad_all_spaces_bindings_fail_at_start_up() {
    for (doc, want) in [
        (
            "spaces: [{name: alpha}]\nbindings: [{subject: 'user:a', role: viewer, spaces: ['*', alpha]}]",
            "bindings[0] (user:a): \"*\" must be the only entry in spaces",
        ),
        (
            "bindings: [{subject: 'user:a', role: viewer, spaces: ['*']}]",
            "bindings[0] (user:a): spaces: [\"*\"] needs at least one space in the file",
        ),
        (
            "roles: [{name: secret-runner, permissions: [contracts:write, contracts:secrets]}]\nspaces: [{name: alpha}]\nbindings: [{subject: 'group:qa', role: secret-runner, spaces: ['*']}]",
            "bindings[0] (group:qa): role `secret-runner` lists contracts:secrets, a global permission that a binding limited to spaces cannot grant",
        ),
    ] {
        let mut cfg = Config::for_tests(StorageKind::Memory, legacy_tokens());
        cfg.plugins = Some(vec![]);
        let f = access_file(doc);
        cfg.access_file = Some(f.clone());
        let e = match fastmock_server::build(cfg, prom()).await {
            Ok(_) => panic!("{doc} was accepted"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            e.contains(&format!("access file {}: {want}", f.display())),
            "{doc}: {e}"
        );
    }
}

fn grantable(v: &Value) -> Vec<(String, bool)> {
    v["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["allSpaces"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn all_spaces_tokens_follow_the_escalation_rule() {
    for (name, storage) in storages().await {
        let h = H::start(storage, tokens(), Some(ACCESS)).await;
        let (value, created) = h
            .create_token(
                LEAD,
                json!({"name": "all-ci", "role": "editor", "spaces": ["*"]}),
            )
            .await;
        assert_eq!(
            (created["spaces"].clone(), created["global"].clone()),
            (json!(["*"]), json!(false)),
            "{name}: stored literally"
        );
        let info = h.ok(Method::GET, "/info", &value, None).await;
        assert_eq!(
            info["principal"]["permissions"]["spaces"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"],
            "{name}: {info}"
        );
        let (st, _) = h.stub(&value, "?space=beta", "/b").await;
        assert_eq!(st, 201, "{name}");
        let (st, _) = h.stub(&value, "", "/g").await;
        assert_eq!(st, 400, "{name}: several spaces, set space");
        let id = created["id"].as_str().unwrap();
        let got = h
            .ok(Method::GET, &format!("/tokens/{id}"), ADMIN, None)
            .await;
        assert_eq!(got["spaces"], json!(["*"]), "{name}");

        let (_, by_admin) = h
            .create_token(
                ADMIN,
                json!({"name": "all-admin", "role": "admin", "spaces": ["*"]}),
            )
            .await;
        assert_eq!(by_admin["spaces"], json!(["*"]), "{name}");

        for (by, role, required) in [
            (PERSPACE, "viewer", "tokens:write"),
            (ALLE, "viewer", "tokens:write"),
            (MINTER, "editor", "stubs:write"),
            (SPADMIN, "viewer", "tokens:write"),
        ] {
            let (st, v) = h
                .call(
                    Method::POST,
                    "/tokens",
                    by,
                    Some(json!({"name": "refused", "role": role, "spaces": ["*"]})),
                )
                .await;
            assert_eq!(
                (st, v["required"].clone()),
                (403, json!(required)),
                "{name}: {role}: {v}"
            );
        }
        let (st, v) = h
            .call(
                Method::POST,
                "/tokens",
                PERSPACE,
                Some(json!({"name": "listed", "role": "viewer", "spaces": ["alpha", "beta"]})),
            )
            .await;
        assert_eq!(
            st, 201,
            "{name}: the caller may still list every current space: {v}"
        );
        h.create_token(
            MINTER,
            json!({"name": "minted", "role": "minter", "spaces": ["*"]}),
        )
        .await;
        for spaces in [
            json!(["*", "alpha"]),
            json!(["alpha", "*"]),
            json!(["*", "*"]),
        ] {
            let (st, v) = h
                .call(
                    Method::POST,
                    "/tokens",
                    ADMIN,
                    Some(json!({"name": "bad", "role": "viewer", "spaces": spaces})),
                )
                .await;
            assert_eq!(
                (st, v["error"].clone()),
                (400, json!("\"*\" must be the only entry in spaces")),
                "{name}: {spaces}"
            );
        }

        let lead = h.ok(Method::GET, "/tokens/options", LEAD, None).await;
        assert_eq!(
            grantable(&lead),
            vec![
                ("viewer".to_string(), true),
                ("editor".to_string(), true),
                ("admin".to_string(), true),
                ("auditor".to_string(), true),
                ("minter".to_string(), true),
                ("team-lead".to_string(), true),
            ],
            "{name}: {lead}"
        );
        let admin = h.ok(Method::GET, "/tokens/options", ADMIN, None).await;
        assert!(
            grantable(&admin).iter().all(|(_, all)| *all),
            "{name}: {admin}"
        );
        let per = h.ok(Method::GET, "/tokens/options", PERSPACE, None).await;
        assert!(grantable(&per).iter().all(|(_, all)| !all), "{name}: {per}");
        assert_eq!(
            per["roles"][0]["spaces"],
            json!(["alpha", "beta"]),
            "{name}"
        );
        let minter = h.ok(Method::GET, "/tokens/options", MINTER, None).await;
        assert_eq!(
            grantable(&minter)[..3],
            [
                ("viewer".to_string(), false),
                ("editor".to_string(), false),
                ("admin".to_string(), false),
            ],
            "{name}: viewer needs files:read and more through *"
        );
        assert!(
            grantable(&minter).contains(&("minter".to_string(), true)),
            "{name}: {minter}"
        );

        let listed = h.ok(Method::GET, "/tokens?space=alpha", ADMIN, None).await;
        let names: Vec<&str> = listed["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for n in ["all-ci", "all-admin", "listed", "minted"] {
            assert!(names.contains(&n), "{name}: {n} in {names:?}");
        }
        let by_lead = h.ok(Method::GET, "/tokens", LEAD, None).await;
        assert!(
            by_lead["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "all-ci"),
            "{name}: tokens:read through * covers * tokens"
        );
        let by_per = h.ok(Method::GET, "/tokens", PERSPACE, None).await;
        let per_names: Vec<&str> = by_per["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            per_names,
            vec!["listed"],
            "{name}: * tokens need tokens:read in all spaces"
        );
        let (st, _) = h
            .call(Method::GET, &format!("/tokens/{id}"), PERSPACE, None)
            .await;
        assert_eq!(st, 404, "{name}");
        let (st, _) = h
            .call(Method::DELETE, &format!("/tokens/{id}"), PERSPACE, None)
            .await;
        assert_eq!(st, 404, "{name}");
        let minted = h.ok(Method::GET, "/tokens", ADMIN, None).await["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "minted")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (st, v) = h
            .call(Method::DELETE, &format!("/tokens/{minted}"), MINTER, None)
            .await;
        assert_eq!(st, 200, "{name}: tokens:write through *: {v}");
        let (st, v) = h
            .call(Method::DELETE, &format!("/tokens/{id}"), LEAD, None)
            .await;
        assert_eq!(
            (st, v["status"].clone()),
            (200, json!("revoked")),
            "{name}: {v}"
        );
    }
}

#[tokio::test]
async fn children_of_all_spaces_tokens() {
    for (name, storage) in storages().await {
        let h = H::start(storage, tokens(), Some(ACCESS)).await;
        let (parent, p) = h
            .create_token(
                ADMIN,
                json!({"name": "parent", "role": "team-lead", "spaces": ["*"], "expiresInDays": 5}),
            )
            .await;
        let (all_child, c1) = h
            .create_token(
                &parent,
                json!({"name": "child-all", "role": "viewer", "spaces": ["*"], "expiresInDays": 30}),
            )
            .await;
        let (one_child, c2) = h
            .create_token(
                &parent,
                json!({"name": "child-alpha", "role": "editor", "spaces": ["alpha"]}),
            )
            .await;
        for c in [&c1, &c2] {
            assert_eq!(c["parentId"], p["id"], "{name}");
            assert_eq!(
                c["expiresAt"], p["expiresAt"],
                "{name}: capped by the parent"
            );
        }
        assert_eq!(c1["spaces"], json!(["*"]), "{name}");
        let (st, v) = h
            .call(
                Method::POST,
                "/tokens",
                &parent,
                Some(json!({"name": "child-global", "role": "viewer", "global": true})),
            )
            .await;
        assert_eq!(st, 403, "{name}: {v}");
        for t in [&all_child, &one_child] {
            assert_eq!(h.call(Method::GET, "/info", t, None).await.0, 200, "{name}");
        }
        let pid = p["id"].as_str().unwrap();
        h.ok(Method::DELETE, &format!("/tokens/{pid}"), ADMIN, None)
            .await;
        for id in [&c1["id"], &c2["id"]] {
            let t = h
                .ok(
                    Method::GET,
                    &format!("/tokens/{}", id.as_str().unwrap()),
                    ADMIN,
                    None,
                )
                .await;
            assert_eq!(t["status"], "revoked", "{name}: revoked with the parent");
        }
        for t in [&all_child, &one_child] {
            assert_eq!(h.call(Method::GET, "/info", t, None).await.0, 401, "{name}");
        }
    }
}

fn stored_all_spaces(value: &str, role: &str) -> ApiToken {
    let now = Utc::now();
    ApiToken {
        id: uuid::Uuid::now_v7(),
        name: format!("stored-{}", uuid::Uuid::new_v4().simple()),
        sha256: hash_token(value),
        role: role.into(),
        spaces: vec!["*".into()],
        global: false,
        description: None,
        created_by: "test".into(),
        created_at: now,
        expires_at: now + chrono::Duration::days(1),
        last_used_at: None,
        revoked_at: None,
        revoked: false,
        parent_id: None,
    }
}

const BEFORE: &str = "spaces: [{name: alpha}]\n";
const AFTER: &str = "spaces: [{name: alpha}, {name: gamma}]\n";

#[tokio::test]
async fn all_spaces_tokens_cover_spaces_added_later() {
    for (name, storage) in storages().await {
        let only_admin = vec![format!("ops:admin:{}", hash_token(ADMIN))];
        let first = H::start(storage.clone(), only_admin.clone(), Some(BEFORE)).await;
        let value = if name == "memory" {
            let v = generate_token();
            let after = H::start(storage.clone(), only_admin.clone(), Some(AFTER)).await;
            after
                .app
                .tokens
                .store()
                .create(&stored_all_spaces(&v, "editor"), Utc::now())
                .await
                .unwrap();
            after.app.tokens.note_created();
            let (st, s) = after.stub(&v, "?space=gamma", "/g").await;
            assert_eq!((st, s["space"].clone()), (201, json!("gamma")), "{name}");
            continue;
        } else {
            first
                .create_token(
                    ADMIN,
                    json!({"name": "early", "role": "editor", "spaces": ["*"]}),
                )
                .await
                .0
        };
        let (st, s) = first.stub(&value, "", "/a").await;
        assert_eq!((st, s["space"].clone()), (201, json!("alpha")), "{name}");
        let (st, _) = first.stub(&value, "?space=gamma", "/g").await;
        assert_eq!(st, 404, "{name}: gamma does not exist yet");
        let after = H::start(storage.clone(), only_admin.clone(), Some(AFTER)).await;
        let (st, s) = after.stub(&value, "?space=gamma", "/g").await;
        assert_eq!(
            (st, s["space"].clone()),
            (201, json!("gamma")),
            "{name}: {s}"
        );
        let (st, _) = after.stub(&value, "", "/x").await;
        assert_eq!(st, 400, "{name}: now bound to two spaces");
        let none = H::start(storage.clone(), only_admin.clone(), Some("roles: []\n")).await;
        let info = none.ok(Method::GET, "/info", &value, None).await;
        let perms = &info["principal"]["permissions"];
        assert_eq!(
            (perms["allSpaces"].clone(), perms["spaces"].clone()),
            (json!([]), json!({})),
            "{name}: without spaces * grants nothing in space scope: {info}"
        );
        let (st, _) = none.call(Method::GET, "/stubs", &value, None).await;
        assert_eq!(st, 403, "{name}");
        let listed = none.ok(Method::GET, "/tokens", ADMIN, None).await;
        assert_eq!(listed["tokens"][0]["spaces"], json!(["*"]), "{name}");
        let (st, v) = none
            .call(
                Method::POST,
                "/tokens",
                ADMIN,
                Some(json!({"name": "late", "role": "viewer", "spaces": ["*"]})),
            )
            .await;
        assert_eq!(st, 400, "{name}: {v}");
        let options = none.ok(Method::GET, "/tokens/options", ADMIN, None).await;
        assert!(
            grantable(&options).iter().all(|(_, all)| !all),
            "{name}: {options}"
        );
    }
}

#[tokio::test]
async fn access_view_lists_the_access_file_without_secrets() {
    let h = H::start(StorageKind::Memory, tokens(), Some(ACCESS)).await;
    let (text_status, text) = h.raw(Method::GET, "/access", ADMIN, None).await;
    assert_eq!(text_status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["configured"], true);
    assert_eq!(v["oidc"], false);
    assert_eq!(
        v["spaces"],
        json!([
            {"name": "alpha", "pathPrefixes": ["/alpha/"], "hosts": []},
            {"name": "beta", "pathPrefixes": [], "hosts": ["beta.example.com"]},
        ])
    );
    let roles: Vec<(&str, bool)> = v["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["builtin"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        roles,
        vec![
            ("viewer", true),
            ("editor", true),
            ("admin", true),
            ("auditor", false),
            ("minter", false),
            ("team-lead", false),
        ]
    );
    assert_eq!(
        v["roles"][3]["permissions"],
        json!(["info:read", "access:read"])
    );
    assert!(
        v["roles"][2]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("access:read"))
    );
    assert!(
        !v["roles"][1]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("access:read"))
    );
    let bindings = v["bindings"].as_array().unwrap();
    assert_eq!(bindings.len(), 14);
    assert_eq!(
        bindings[0],
        json!({"subject": "token:allv", "role": "viewer", "global": false, "spaces": ["*"], "source": "accessFile"})
    );
    assert_eq!(
        bindings[6],
        json!({"subject": "token:perspace", "role": "team-lead", "global": false, "spaces": ["alpha", "beta"], "source": "accessFile"})
    );
    assert_eq!(
        bindings[9],
        json!({"subject": "group:auditors", "role": "auditor", "global": true, "spaces": [], "source": "accessFile"})
    );
    assert_eq!(
        bindings[11..],
        [
            json!({"subject": "token:ops", "role": "admin", "global": true, "spaces": [], "source": "adminTokens"}),
            json!({"subject": "token:ci", "role": "editor", "global": true, "spaces": [], "source": "adminTokens"}),
            json!({"subject": "token:dash", "role": "viewer", "global": true, "spaces": [], "source": "adminTokens"}),
        ]
    );
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["bindings", "configured", "oidc", "roles", "spaces"]
    );
    let file = h.file.as_ref().unwrap();
    for secret in [
        ADMIN.to_string(),
        LEAD.to_string(),
        hash_token(ADMIN),
        hash_token(EDITOR),
        hash_token(VIEWER),
        hash_token(LEAD),
        file.display().to_string(),
        file.parent().unwrap().display().to_string(),
        "access.yaml".to_string(),
        "sha256".to_string(),
    ] {
        assert!(!text.contains(&secret), "{secret} leaked: {text}");
    }
    let auditor = h.ok(Method::GET, "/access", AUDITOR, None).await;
    assert_eq!(auditor, v, "access:* through a custom role");

    for token in [SPADMIN, ALLA, ALLV] {
        let (st, body) = h.call(Method::GET, "/access", token, None).await;
        assert_eq!(st, 403, "{body}");
        assert_eq!(
            body,
            json!({"error": "forbidden", "details": ["requires permission access:read"], "required": "access:read"}),
            "{token}"
        );
    }
    for token in [EDITOR, VIEWER] {
        let (st, body) = h.call(Method::GET, "/access", token, None).await;
        assert_eq!(
            (st, body),
            (
                403,
                json!({"error": "forbidden", "details": ["requires role admin"], "required": "admin"})
            ),
            "{token}: global built-in roles name the role, as before"
        );
    }
}

#[tokio::test]
async fn access_view_lists_the_roles_of_static_tokens_without_hashes() {
    let h = H::start(
        StorageKind::Memory,
        vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("lead::{}", hash_token(LEAD)),
        ],
        Some(
            r#"
spaces:
  - name: alpha
    pathPrefixes: [/alpha/]
bindings:
  - subject: token:lead
    role: editor
    spaces: [alpha]
"#,
        ),
    )
    .await;
    let (st, text) = h.raw(Method::GET, "/access", ADMIN, None).await;
    assert_eq!(st, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["bindings"],
        json!([
            {"subject": "token:lead", "role": "editor", "global": false, "spaces": ["alpha"], "source": "accessFile"},
            {"subject": "token:ops", "role": "admin", "global": true, "spaces": [], "source": "adminTokens"},
        ]),
        "name:role:hash adds a global binding, name::hash adds none"
    );
    for secret in [
        ADMIN.to_string(),
        LEAD.to_string(),
        hash_token(ADMIN),
        hash_token(LEAD),
        hash_token(ADMIN).to_uppercase(),
        hash_token(LEAD).to_uppercase(),
        "sha256".to_string(),
    ] {
        assert!(!text.contains(&secret), "{secret} leaked: {text}");
    }
}

#[tokio::test]
async fn access_view_without_an_access_file_is_admin_only() {
    let h = H::start(StorageKind::Memory, legacy_tokens(), None).await;
    let v = h.ok(Method::GET, "/access", ADMIN, None).await;
    assert_eq!(
        v,
        json!({
            "configured": false,
            "oidc": false,
            "spaces": [],
            "roles": [
                {"name": "viewer", "builtin": true, "permissions": v["roles"][0]["permissions"].clone()},
                {"name": "editor", "builtin": true, "permissions": v["roles"][1]["permissions"].clone()},
                {"name": "admin", "builtin": true, "permissions": v["roles"][2]["permissions"].clone()},
            ],
            "bindings": [
                {"subject": "token:ops", "role": "admin", "global": true, "spaces": [], "source": "adminTokens"},
                {"subject": "token:ci", "role": "editor", "global": true, "spaces": [], "source": "adminTokens"},
                {"subject": "token:dash", "role": "viewer", "global": true, "spaces": [], "source": "adminTokens"},
            ],
        })
    );
    assert_eq!(v["roles"][2]["permissions"].as_array().unwrap().len(), 28);
    for token in [VIEWER, EDITOR] {
        let (st, body) = h.call(Method::GET, "/access", token, None).await;
        assert_eq!(
            (st, body),
            (
                403,
                json!({"error": "forbidden", "details": ["requires role admin"], "required": "admin"})
            )
        );
    }
    let info = h.ok(Method::GET, "/info", ADMIN, None).await;
    assert_eq!(info["principal"]["permissions"]["allSpaces"], json!([]));
    assert_eq!(info["principal"]["permissions"]["spaces"], json!({}));
}
