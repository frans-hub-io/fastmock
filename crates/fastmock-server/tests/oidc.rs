//! OIDC sign-in, sessions and bearer JWTs against a fake identity provider running in this process.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use reqwest::Method;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_oidc_tests_0123456789";
const CLIENT_ID: &str = "fastmock";
const CLIENT_SECRET: &str = "client-secret-for-oidc-tests";
const API_AUDIENCE: &str = "fastmock-api";

const ACCESS: &str = r#"
spaces:
  - name: payments
  - name: billing
bindings:
  - subject: group:payments-dev
    role: editor
    spaces: [payments]
  - subject: user:carol
    role: viewer
    global: true
"#;

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

impl LogBuf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

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

async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_oidc_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("oidc:{}:", uuid::Uuid::new_v4().simple()),
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

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn b64(v: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

struct Key {
    kid: String,
    kp: EcdsaKeyPair,
}

impl Key {
    fn new(kid: &str) -> Arc<Self> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        Arc::new(Key {
            kid: kid.into(),
            kp: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                .unwrap(),
        })
    }

    fn jwk(&self) -> Value {
        let p = self.kp.public_key().as_ref();
        json!({"kty": "EC", "crv": "P-256", "kid": self.kid, "use": "sig", "alg": "ES256",
            "x": URL_SAFE_NO_PAD.encode(&p[1..33]), "y": URL_SAFE_NO_PAD.encode(&p[33..65])})
    }

    fn sign(&self, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            b64(&json!({"alg": "ES256", "typ": "JWT", "kid": self.kid})),
            b64(claims)
        );
        let sig = self
            .kp
            .sign(&SystemRandom::new(), input.as_bytes())
            .unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }
}

#[derive(Clone)]
struct User {
    sub: String,
    name: String,
    /// `None`: no groups claim in the ID token (userinfo has them).
    groups: Option<Vec<String>>,
    userinfo_groups: Vec<String>,
}

fn user(name: &str, groups: &[&str]) -> User {
    User {
        sub: format!("sub-{name}"),
        name: name.into(),
        groups: Some(groups.iter().map(|g| g.to_string()).collect()),
        userinfo_groups: vec![],
    }
}

struct Code {
    challenge: String,
    nonce: String,
    redirect_uri: String,
    user: User,
}

#[derive(Default)]
struct IdpState {
    next_user: Option<User>,
    codes: HashMap<String, Code>,
    authorize_params: Vec<HashMap<String, String>>,
    jwks_fetches: u32,
    published: Vec<Arc<Key>>,
    signing: Option<Arc<Key>>,
    wrong_nonce: bool,
    jwks_uri_override: Option<String>,
    issuer_override: Option<String>,
    token_requests: u32,
}

#[derive(Clone)]
struct Idp {
    url: String,
    st: Arc<Mutex<IdpState>>,
}

impl Idp {
    async fn start() -> Idp {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::serve(l).await
    }

    async fn serve(l: tokio::net::TcpListener) -> Idp {
        let url = format!("http://{}", l.local_addr().unwrap());
        let key = Key::new("k1");
        let st = Arc::new(Mutex::new(IdpState {
            published: vec![key.clone()],
            signing: Some(key),
            ..Default::default()
        }));
        let idp = Idp { url, st };
        let app = axum::Router::new()
            .route(
                "/realms/test/.well-known/openid-configuration",
                get(discovery),
            )
            .route("/realms/test/authorize", get(authorize))
            .route("/realms/test/token", post(token))
            .route("/realms/test/userinfo", get(userinfo))
            .route("/realms/test/jwks", get(jwks))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        idp
    }

    fn issuer(&self) -> String {
        format!("{}/realms/test", self.url)
    }

    fn with<R>(&self, f: impl FnOnce(&mut IdpState) -> R) -> R {
        f(&mut self.st.lock().unwrap())
    }

    fn jwt(&self, claims: &Value) -> String {
        self.with(|s| s.signing.clone().unwrap()).sign(claims)
    }

    fn api_token(&self, name: &str, groups: &[&str], aud: &str, exp_in: i64) -> String {
        self.jwt(&json!({
            "iss": self.issuer(), "aud": aud, "azp": "some-cli", "sub": format!("sub-{name}"),
            "preferred_username": name, "groups": groups, "iat": now(), "exp": now() + exp_in,
            "realm_access": {"roles": ["offline_access", "uma_authorization", "default-roles-test"]},
            "scope": "openid profile email", "email": format!("{name}@example.com"), "sid": "a".repeat(200)
        }))
    }
}

async fn discovery(State(idp): State<Idp>) -> Json {
    let base = idp.issuer();
    let (jwks, iss) = idp.with(|s| (s.jwks_uri_override.clone(), s.issuer_override.clone()));
    J(json!({
        "issuer": iss.unwrap_or_else(|| base.clone()),
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "userinfo_endpoint": format!("{base}/userinfo"),
        "jwks_uri": jwks.unwrap_or_else(|| format!("{base}/jwks")),
        "end_session_endpoint": format!("{base}/logout"),
        "code_challenge_methods_supported": ["plain", "S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "id_token_signing_alg_values_supported": ["ES256"]
    }))
}

type Json = axum::Json<Value>;
use axum::Json as J;

async fn authorize(State(idp): State<Idp>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut s = idp.st.lock().unwrap();
    s.authorize_params.push(q.clone());
    let ok = q.get("response_type").map(String::as_str) == Some("code")
        && q.get("client_id").map(String::as_str) == Some(CLIENT_ID)
        && q.get("code_challenge_method").map(String::as_str) == Some("S256")
        && q.get("code_challenge").is_some_and(|c| c.len() == 43)
        && q.contains_key("state")
        && q.contains_key("nonce");
    let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
    let state = q.get("state").cloned().unwrap_or_default();
    if !ok {
        return (StatusCode::BAD_REQUEST, "bad authorization request").into_response();
    }
    let Some(user) = s.next_user.clone() else {
        let loc = format!("{redirect}?error=access_denied&state={state}");
        return (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response();
    };
    let code = format!("code-{}", uuid::Uuid::new_v4().simple());
    s.codes.insert(
        code.clone(),
        Code {
            challenge: q["code_challenge"].clone(),
            nonce: q["nonce"].clone(),
            redirect_uri: redirect.clone(),
            user,
        },
    );
    let loc = format!("{redirect}?code={code}&state={state}&session_state=x");
    (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response()
}

async fn token(
    State(idp): State<Idp>,
    headers: HeaderMap,
    Form(f): Form<HashMap<String, String>>,
) -> Response {
    let bad = |e: &str| (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(basic.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let (code, wrong_nonce) = {
        let mut s = idp.st.lock().unwrap();
        s.token_requests += 1;
        let Some(code) = f.get("code").and_then(|c| s.codes.remove(c)) else {
            return bad("invalid_grant");
        };
        (code, s.wrong_nonce)
    };
    if f.get("grant_type").map(String::as_str) != Some("authorization_code")
        || f.get("redirect_uri") != Some(&code.redirect_uri)
    {
        return bad("invalid_grant");
    }
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != code.challenge {
        return bad("invalid_grant");
    }
    let mut claims = json!({
        "iss": idp.issuer(), "aud": CLIENT_ID, "azp": CLIENT_ID, "sub": code.user.sub,
        "preferred_username": code.user.name, "iat": now(), "exp": now() + 300,
        "nonce": if wrong_nonce { "not-the-nonce".to_string() } else { code.nonce.clone() },
    });
    if let Some(g) = &code.user.groups {
        claims["groups"] = json!(g);
    }
    let id_token = idp.jwt(&claims);
    let access = format!(
        "at.{}.{}",
        URL_SAFE_NO_PAD.encode(code.user.sub.as_bytes()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&code.user.userinfo_groups).unwrap())
    );
    axum::Json(json!({"access_token": access, "token_type": "Bearer", "expires_in": 300, "id_token": id_token}))
        .into_response()
}

async fn userinfo(headers: HeaderMap) -> Response {
    let Some(at) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer at."))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let (sub, groups) = at.split_once('.').unwrap();
    let sub = String::from_utf8(URL_SAFE_NO_PAD.decode(sub).unwrap()).unwrap();
    let groups: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(groups).unwrap()).unwrap();
    axum::Json(json!({"sub": sub, "groups": groups})).into_response()
}

async fn jwks(State(idp): State<Idp>) -> Json {
    let mut s = idp.st.lock().unwrap();
    s.jwks_fetches += 1;
    J(json!({"keys": s.published.iter().map(|k| k.jwk()).collect::<Vec<_>>()}))
}

struct Opts {
    ttl: &'static str,
    idle: &'static str,
    api: bool,
    insecure: bool,
    /// Public admin URL of all replicas; the replica's own address when `None`.
    public: Option<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            ttl: "8h",
            idle: "1h",
            api: true,
            insecure: false,
            public: None,
        }
    }
}

fn settings(
    idp: &Idp,
    admin: &str,
    o: &Opts,
) -> anyhow::Result<Option<fastmock_server::oidc::OidcSettings>> {
    let mut vars: HashMap<&str, String> = HashMap::from([
        ("OIDC_ISSUER", idp.issuer()),
        ("OIDC_CLIENT_ID", CLIENT_ID.to_string()),
        ("OIDC_CLIENT_SECRET", CLIENT_SECRET.to_string()),
        (
            "OIDC_REDIRECT_URL",
            format!("{admin}/__admin/auth/callback"),
        ),
        ("OIDC_SESSION_TTL", o.ttl.to_string()),
        ("OIDC_SESSION_IDLE", o.idle.to_string()),
    ]);
    if o.api {
        vars.insert("OIDC_API_AUDIENCE", API_AUDIENCE.into());
    }
    if o.insecure {
        vars.insert("OIDC_INSECURE_COOKIE", "true".into());
    }
    fastmock_server::oidc::settings::from_env(&|k| vars.get(k).cloned())
}

#[allow(dead_code)]
struct H {
    admin: String,
    data: String,
    http: reqwest::Client,
    app: AppState,
    _shutdown: watch::Sender<bool>,
}

struct LoginResult {
    status: u16,
    location: String,
    cookies: Vec<String>,
    session: Option<String>,
    callback_query: String,
    login_cookie: String,
}

fn cookie_named<'a>(cookies: &'a [String], name: &str) -> Option<&'a String> {
    cookies.iter().find(|c| c.starts_with(&format!("{name}=")))
}

fn cookie_val(c: &str) -> String {
    c.split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string()
}

fn set_cookies(r: &reqwest::Response) -> Vec<String> {
    r.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

impl H {
    async fn try_start(storage: StorageKind, idp: &Idp, o: &Opts) -> anyhow::Result<H> {
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin = format!("http://{}", al.local_addr().unwrap());
        let data = format!("http://{}", dl.local_addr().unwrap());
        let mut cfg = Config::for_tests(storage, vec![format!("ops:admin:{}", hash_token(ADMIN))]);
        cfg.plugins = Some(vec![]);
        let dir = std::env::temp_dir().join(format!("fm-oidc-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("access.yaml");
        std::fs::write(&f, ACCESS).unwrap();
        cfg.access_file = Some(f);
        cfg.oidc = settings(idp, o.public.as_deref().unwrap_or(&admin), o)?;
        let (app, data_router, admin_router, _policy) = fastmock_server::build(cfg, prom()).await?;
        let (tx, _rx) = watch::channel(false);
        tokio::spawn(async move { axum::serve(al, admin_router).await.unwrap() });
        tokio::spawn(async move { axum::serve(dl, data_router).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        Ok(H {
            admin,
            data,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            app,
            _shutdown: tx,
        })
    }

    async fn start(storage: StorageKind, idp: &Idp, o: &Opts) -> H {
        Self::try_start(storage, idp, o).await.unwrap()
    }

    fn session_cookie(&self) -> &'static str {
        self.app.oidc.as_ref().unwrap().session_cookie_name()
    }

    /// The login redirect: returns the IdP URL and the login cookie.
    async fn begin(&self, ret: &str) -> (u16, String, Vec<String>) {
        let r = self
            .http
            .get(format!("{}/__admin/auth/login", self.admin))
            .query(&[("return", ret)])
            .send()
            .await
            .unwrap();
        let loc = r
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (r.status().as_u16(), loc, set_cookies(&r))
    }

    async fn callback(
        &self,
        query: &str,
        login_cookie: Option<&str>,
    ) -> (u16, String, Vec<String>) {
        let mut rb = self
            .http
            .get(format!("{}/__admin/auth/callback?{query}", self.admin));
        if let Some(c) = login_cookie {
            rb = rb.header(header::COOKIE, c);
        }
        let r = rb.send().await.unwrap();
        let loc = r
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (r.status().as_u16(), loc, set_cookies(&r))
    }

    /// The whole code flow for `u`; the callback may run on another replica.
    async fn login_via(&self, other: &H, idp: &Idp, u: Option<User>, ret: &str) -> LoginResult {
        idp.with(|s| s.next_user = u);
        let (st, idp_url, cookies) = self.begin(ret).await;
        assert_eq!(st, 302, "{idp_url}");
        let login_name = self.app.oidc.as_ref().unwrap().login_cookie_name();
        let login_cookie = cookie_named(&cookies, login_name).unwrap().clone();
        let r = self.http.get(&idp_url).send().await.unwrap();
        assert_eq!(r.status(), 302);
        let back = r.headers()[header::LOCATION].to_str().unwrap().to_string();
        let query = back.split_once('?').unwrap().1.to_string();
        let cookie = format!("{login_name}={}", cookie_val(&login_cookie));
        let (status, location, cookies) = other.callback(&query, Some(&cookie)).await;
        let session = cookie_named(&cookies, self.session_cookie())
            .map(|c| cookie_val(c))
            .filter(|v| !v.is_empty());
        LoginResult {
            status,
            location,
            cookies,
            session,
            callback_query: query,
            login_cookie: cookie,
        }
    }

    async fn login(&self, idp: &Idp, u: Option<User>, ret: &str) -> LoginResult {
        self.login_via(self, idp, u, ret).await
    }

    async fn with_session(
        &self,
        m: Method,
        path: &str,
        session: &str,
        csrf: bool,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut rb = self
            .http
            .request(m, format!("{}/__admin{path}", self.admin))
            .header(
                header::COOKIE,
                format!("{}={session}", self.session_cookie()),
            );
        if csrf {
            rb = rb.header("X-FastMock-Request", "1");
        }
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn bearer(
        &self,
        m: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut rb = self
            .http
            .request(m, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn logout(&self, session: &str, csrf: bool, origin: Option<&str>) -> reqwest::Response {
        let mut rb = self
            .http
            .post(format!("{}/__admin/auth/logout", self.admin))
            .header(
                header::COOKIE,
                format!("{}={session}", self.session_cookie()),
            );
        if csrf {
            rb = rb.header("X-FastMock-Request", "1");
        }
        if let Some(o) = origin {
            rb = rb.header(header::ORIGIN, o);
        }
        rb.send().await.unwrap()
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

#[tokio::test]
async fn code_flow_session_csrf_and_logout() {
    let idp = Idp::start().await;
    for (name, storage) in storages().await {
        let h = H::start(storage, &idp, &Opts::default()).await;
        let r = h
            .http
            .get(format!("{}/__admin/auth/config", h.admin))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.json::<Value>().await.unwrap(),
            json!({"oidc": true, "tokenLogin": true})
        );

        let ret = "/ui/stubs?space=payments";
        let res = h
            .login(&idp, Some(user("alice", &["payments-dev", "other"])), ret)
            .await;
        assert_eq!(res.status, 302, "{name}");
        assert_eq!(res.location, ret, "{name}");
        let params = idp.with(|s| s.authorize_params.last().cloned().unwrap());
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["scope"], "openid profile email");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(
            params["redirect_uri"],
            format!("{}/__admin/auth/callback", h.admin)
        );
        assert_eq!(params["state"].len(), 43);
        assert_eq!(params["nonce"].len(), 43);
        assert_ne!(params["state"], params["nonce"]);

        let sc = cookie_named(&res.cookies, "__Host-fastmock_session").unwrap();
        for attr in [
            "Path=/",
            "HttpOnly",
            "SameSite=Lax",
            "Secure",
            "Max-Age=28800",
        ] {
            assert!(
                sc.split("; ").any(|a| a == attr),
                "{name}: {sc} lacks {attr}"
            );
        }
        assert!(!sc.contains("Domain"), "{sc}");
        let session = res.session.clone().unwrap();
        assert_eq!(session.len(), 43);
        let lc = cookie_named(&res.cookies, "__Host-fastmock_login").unwrap();
        assert!(lc.contains("Max-Age=0"), "{lc}");

        let (st, info) = h
            .with_session(Method::GET, "/info", &session, false, None)
            .await;
        assert_eq!(st, 200, "{name}: {info}");
        let p = &info["principal"];
        assert_eq!(p["kind"], "user");
        assert_eq!(p["name"], "alice");
        assert_eq!(p["groups"], json!(["payments-dev", "other"]));
        assert!(
            p["permissions"]["spaces"]["payments"]
                .as_array()
                .unwrap()
                .contains(&json!("stubs:write")),
            "{p}"
        );

        let stub = json!({"request": {"urlPath": "/payments/x"}, "response": {"status": 200}, "space": "payments"});
        let (st, v) = h
            .with_session(Method::POST, "/stubs", &session, false, Some(stub.clone()))
            .await;
        assert_eq!(st, 403, "{name}: {v}");
        assert_eq!(v["details"][0], "missing X-FastMock-Request: 1 header");
        let (st, v) = h
            .with_session(Method::POST, "/stubs", &session, true, Some(stub.clone()))
            .await;
        assert_eq!(st, 201, "{name}: {v}");
        assert_eq!(v["space"], "payments");
        let mut billing = stub.clone();
        billing["space"] = json!("billing");
        let (st, _) = h
            .with_session(Method::POST, "/stubs", &session, true, Some(billing))
            .await;
        assert_eq!(st, 403, "{name}");
        let (st, _) = h
            .with_session(
                Method::DELETE,
                "/stubs?space=payments",
                &session,
                false,
                None,
            )
            .await;
        assert_eq!(st, 403, "{name}: DELETE needs the header too");

        let r = h
            .http
            .get(format!("{}/__admin/info", h.admin))
            .bearer_auth(ADMIN)
            .header(header::COOKIE, format!("__Host-fastmock_session={session}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.json::<Value>().await.unwrap()["principal"]["name"], "ops");
        let r = h
            .http
            .get(format!("{}/__admin/info", h.admin))
            .bearer_auth("not-a-token")
            .header(header::COOKIE, format!("__Host-fastmock_session={session}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "an Authorization header decides alone");

        let (st, l, c) = h
            .callback(&res.callback_query, Some(&res.login_cookie))
            .await;
        assert_eq!(st, 302);
        assert_eq!(l, "/ui/?loginError=invalid_state", "{name}: replayed state");
        assert!(cookie_named(&c, "__Host-fastmock_session").is_none());

        assert_eq!(h.logout(&session, false, None).await.status(), 403);
        assert_eq!(
            h.logout(&session, true, Some("https://evil.example.com"))
                .await
                .status(),
            403
        );
        let r = h.logout(&session, true, None).await;
        assert_eq!(r.status(), 200);
        let cleared = set_cookies(&r);
        assert!(
            cleared[0].starts_with("__Host-fastmock_session=;"),
            "{cleared:?}"
        );
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["loggedOut"], true);
        let lu = v["logoutUrl"].as_str().unwrap();
        assert!(
            lu.starts_with(&format!(
                "{}/logout?client_id=fastmock&post_logout_redirect_uri=",
                idp.issuer()
            )),
            "{lu}"
        );
        let (st, v) = h
            .with_session(Method::GET, "/info", &session, false, None)
            .await;
        assert_eq!(st, 401, "{name}: {v}");
        assert_eq!(
            h.logout(&session, true, None)
                .await
                .json::<Value>()
                .await
                .unwrap()["loggedOut"],
            false
        );

        let (st, audit) = h.bearer(Method::GET, "/audit?limit=50", ADMIN, None).await;
        assert_eq!(st, 200);
        let events = audit["events"].as_array().cloned().unwrap();
        let actions: Vec<(String, String, String)> = events
            .iter()
            .map(|e| {
                (
                    e["action"].as_str().unwrap_or_default().to_string(),
                    e["principal"].as_str().unwrap_or_default().to_string(),
                    e["principalKind"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        for want in ["auth.login", "auth.logout", "stub.create"] {
            assert!(
                actions
                    .iter()
                    .any(|(a, p, k)| a == want && p == "alice" && k == "user"),
                "{name}: {want} missing in {actions:?}"
            );
        }
        let m = h.metrics().await;
        assert!(
            m.contains("fastmock_oidc_logins_total{result=\"success\"}"),
            "{m}"
        );
        assert!(m.contains("fastmock_oidc_logins_total{result=\"invalid_state\"}"));
        assert!(m.contains("fastmock_admin_auth_failures_total{reason=\"csrf\"}"));
        assert!(m.contains("fastmock_admin_auth_failures_total{reason=\"session\"}"));

        let logged = logs().text();
        for secret in [
            session.as_str(),
            CLIENT_SECRET,
            res.callback_query
                .split('&')
                .next()
                .unwrap()
                .trim_start_matches("code="),
            params["state"].as_str(),
        ] {
            assert!(
                !logged.contains(secret),
                "{name}: a secret leaked into the logs"
            );
        }
    }
}

#[tokio::test]
async fn login_rejections() {
    let idp = Idp::start().await;
    let h = H::start(StorageKind::Memory, &idp, &Opts::default()).await;
    for bad in [
        "//evil.example.com",
        "https://evil.example.com/ui/",
        "/ui//evil.example.com",
        "/\\evil.example.com",
        "/ui/%2F%2Fevil.example.com",
        "/ui/%5C%5Cevil.example.com",
        "/ui/../__admin/stubs",
        "/__admin/stubs",
        "javascript:alert(1)",
    ] {
        let (st, loc, cookies) = h.begin(bad).await;
        assert_eq!(st, 400, "{bad} -> {loc}");
        assert!(cookies.is_empty());
    }
    let res = h
        .login(&idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await;
    assert_eq!(res.location, "/ui/");

    idp.with(|s| s.wrong_nonce = true);
    let res = h
        .login(&idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await;
    idp.with(|s| s.wrong_nonce = false);
    assert_eq!(res.location, "/ui/?loginError=failed");
    assert!(res.session.is_none());

    let res = h.login(&idp, None, "/ui/").await;
    assert_eq!(res.location, "/ui/?loginError=idp_error");
    assert!(res.session.is_none());

    idp.with(|s| s.next_user = Some(user("alice", &["payments-dev"])));
    let (_, idp_url, _) = h.begin("/ui/").await;
    let r = h.http.get(&idp_url).send().await.unwrap();
    let back = r.headers()[header::LOCATION].to_str().unwrap().to_string();
    let query = back.split_once('?').unwrap().1;
    let (st, loc, cookies) = h.callback(query, None).await;
    assert_eq!(
        (st, loc.as_str()),
        (302, "/ui/?loginError=invalid_state"),
        "no login cookie"
    );
    assert!(cookie_named(&cookies, "__Host-fastmock_session").is_none());
    let (_, loc, _) = h
        .callback(
            query,
            Some("__Host-fastmock_login=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        )
        .await;
    assert_eq!(loc, "/ui/?loginError=invalid_state", "state was consumed");
    let (_, loc, _) = h.callback("code=x&state=unknown", None).await;
    assert_eq!(loc, "/ui/?loginError=invalid_state");
    let (_, loc, _) = h.callback("code=x", None).await;
    assert_eq!(loc, "/ui/?loginError=invalid_state");

    let audit = h
        .bearer(Method::GET, "/audit?limit=50", ADMIN, None)
        .await
        .1;
    let text = audit.to_string();
    assert!(text.contains("auth.login_failed"), "{text}");
    assert!(text.contains("ID token rejected"), "{text}");
    assert!(!text.contains("not-the-nonce"));
}

#[tokio::test]
async fn groups_and_users_map_to_bindings() {
    let idp = Idp::start().await;
    for (name, storage) in storages().await {
        let h = H::start(storage, &idp, &Opts::default()).await;
        let res = h.login(&idp, Some(user("bob", &["nobody"])), "/ui/").await;
        assert_eq!(res.location, "/ui/?loginError=no_access", "{name}");
        assert!(res.session.is_none());

        let res = h.login(&idp, Some(user("carol", &[])), "/ui/").await;
        let s = res.session.unwrap();
        let (st, v) = h.with_session(Method::GET, "/stubs", &s, false, None).await;
        assert_eq!(st, 200, "{name}: {v}");
        let (st, _) = h
            .with_session(
                Method::POST,
                "/stubs",
                &s,
                true,
                Some(json!({"request": {"urlPath": "/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 403, "{name}: viewer");

        let mut dave = user("dave", &[]);
        dave.groups = None;
        dave.userinfo_groups = vec!["payments-dev".into()];
        let res = h.login(&idp, Some(dave), "/ui/").await;
        let s = res.session.expect("groups from userinfo");
        let (_, info) = h.with_session(Method::GET, "/info", &s, false, None).await;
        assert_eq!(
            info["principal"]["groups"],
            json!(["payments-dev"]),
            "{name}"
        );
        let (st, v) = h
            .with_session(
                Method::POST,
                "/stubs",
                &s,
                true,
                Some(json!({"request": {"urlPath": "/p"}, "response": {"status": 200}, "space": "payments"})),
            )
            .await;
        assert_eq!(st, 201, "{name}: {v}");
        let m = h.metrics().await;
        assert!(m.contains("fastmock_oidc_logins_total{result=\"no_binding\"}"));
    }
}

#[tokio::test]
async fn sessions_expire_when_idle_and_after_the_absolute_ttl() {
    let idp = Idp::start().await;
    let h = H::start(
        StorageKind::Memory,
        &idp,
        &Opts {
            ttl: "2500ms",
            idle: "1s",
            ..Default::default()
        },
    )
    .await;
    let s = h
        .login(&idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await
        .session
        .unwrap();
    let start = Instant::now();
    let mut last_ok = Duration::ZERO;
    loop {
        let (st, _) = h.with_session(Method::GET, "/info", &s, false, None).await;
        if st != 200 {
            assert_eq!(st, 401);
            break;
        }
        last_ok = start.elapsed();
        assert!(
            last_ok < Duration::from_millis(2600),
            "outlived the absolute TTL"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        last_ok > Duration::from_millis(2000),
        "ended early at {last_ok:?}"
    );

    let s = h
        .login(&idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await
        .session
        .unwrap();
    assert_eq!(
        h.with_session(Method::GET, "/info", &s, false, None)
            .await
            .0,
        200
    );
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let r = h
        .http
        .get(format!("{}/__admin/info", h.admin))
        .header(header::COOKIE, format!("__Host-fastmock_session={s}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "idle timeout");
    assert!(set_cookies(&r)[0].contains("Max-Age=0"));
}

#[tokio::test]
async fn sessions_and_pending_logins_are_shared_by_replicas() {
    let Some(storage) = pg_storage().await else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: skipped");
        return;
    };
    let idp = Idp::start().await;
    let a = H::start(storage.clone(), &idp, &Opts::default()).await;
    let b = H::start(
        storage,
        &idp,
        &Opts {
            public: Some(a.admin.clone()),
            ..Default::default()
        },
    )
    .await;
    let res = a
        .login_via(&b, &idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await;
    assert_eq!(res.location, "/ui/", "callback on the other replica");
    let s = res.session.unwrap();
    assert_eq!(
        a.with_session(Method::GET, "/info", &s, false, None)
            .await
            .0,
        200
    );
    assert_eq!(
        b.with_session(Method::GET, "/info", &s, false, None)
            .await
            .0,
        200
    );
    let r = b
        .http
        .post(format!("{}/__admin/auth/logout", b.admin))
        .header(header::COOKIE, format!("__Host-fastmock_session={s}"))
        .header("X-FastMock-Request", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        a.with_session(Method::GET, "/info", &s, false, None)
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn bearer_jwts_next_to_static_and_managed_tokens() {
    let idp = Idp::start().await;
    for (name, storage) in storages().await {
        let h = H::start(storage, &idp, &Opts::default()).await;
        let t = idp.api_token("alice", &["payments-dev"], API_AUDIENCE, 300);
        assert!(t.len() > 512);
        let (st, v) = h.bearer(Method::GET, "/info", &t, None).await;
        assert_eq!(st, 200, "{name}: {v}");
        assert_eq!(v["principal"]["kind"], "user");
        assert_eq!(v["principal"]["name"], "alice");
        let (st, _) = h
            .bearer(
                Method::POST,
                "/stubs",
                &t,
                Some(json!({"request": {"urlPath": "/j"}, "response": {"status": 200}, "space": "payments"})),
            )
            .await;
        assert_eq!(st, 201, "{name}: no CSRF header needed with a bearer token");

        for bad in [
            idp.api_token("alice", &["payments-dev"], CLIENT_ID, 300),
            idp.api_token("alice", &["payments-dev"], API_AUDIENCE, -120),
            idp.jwt(&json!({"iss": "https://evil.example.com", "aud": API_AUDIENCE, "sub": "x", "exp": now() + 60})),
            format!("{}x", &t[..t.len() - 1]),
        ] {
            let (st, v) = h.bearer(Method::GET, "/info", &bad, None).await;
            assert_eq!(st, 401, "{name}: {v}");
        }
        let (st, v) = h
            .bearer(
                Method::GET,
                "/info",
                &idp.api_token("bob", &["nobody"], API_AUDIENCE, 300),
                None,
            )
            .await;
        assert_eq!(st, 403, "{name}: {v}");
        assert!(
            v["details"][0]
                .as_str()
                .unwrap()
                .contains("user bob has no access")
        );

        let (st, _) = h.bearer(Method::GET, "/info", ADMIN, None).await;
        assert_eq!(st, 200);
        let (st, created) = h
            .bearer(
                Method::POST,
                "/tokens",
                ADMIN,
                Some(json!({"name": "ci", "role": "viewer", "global": true})),
            )
            .await;
        assert_eq!(st, 201);
        let (st, v) = h
            .bearer(
                Method::GET,
                "/info",
                created["token"].as_str().unwrap(),
                None,
            )
            .await;
        assert_eq!(
            (st, v["principal"]["kind"].as_str()),
            (200, Some("apiToken"))
        );

        let before = idp.with(|s| s.jwks_fetches);
        let stranger = Key::new("unknown-kid");
        let forged = stranger.sign(&json!({"iss": idp.issuer(), "aud": API_AUDIENCE, "sub": "x", "preferred_username": "alice", "groups": ["payments-dev"], "exp": now() + 60}));
        assert_eq!(h.bearer(Method::GET, "/info", &forged, None).await.0, 401);
        assert_eq!(h.bearer(Method::GET, "/info", &forged, None).await.0, 401);
        assert_eq!(
            idp.with(|s| s.jwks_fetches) - before,
            1,
            "{name}: one rate-limited refresh for unknown keys"
        );
        let m = h.metrics().await;
        assert!(m.contains("fastmock_admin_auth_failures_total{reason=\"jwt\"}"));
    }

    let rotated = Key::new("k2");
    idp.with(|s| {
        s.published.push(rotated.clone());
        s.signing = Some(rotated.clone());
    });
    let h = H::start(StorageKind::Memory, &idp, &Opts::default()).await;
    idp.with(|s| s.published.retain(|k| k.kid == "k1"));
    let fresh = Key::new("k3");
    idp.with(|s| {
        s.published.push(fresh.clone());
        s.signing = Some(fresh.clone());
    });
    let t = idp.api_token("alice", &["payments-dev"], API_AUDIENCE, 300);
    assert_eq!(
        h.bearer(Method::GET, "/info", &t, None).await.0,
        200,
        "key rotation"
    );

    let no_api = H::start(
        StorageKind::Memory,
        &idp,
        &Opts {
            api: false,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(no_api.bearer(Method::GET, "/info", &t, None).await.0, 401);
}

#[tokio::test]
async fn data_plane_never_sees_admin_cookies() {
    let idp = Idp::start().await;
    let h = H::start(StorageKind::Memory, &idp, &Opts::default()).await;
    let (st, v) = h
        .bearer(
            Method::POST,
            "/stubs",
            ADMIN,
            Some(json!({"request": {"urlPath": "/echo"}, "response": {"status": 200, "body": "c={{request.headers.[cookie]}}"}})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    h.app.engine.reload().await.unwrap();
    let body = h
        .http
        .get(format!("{}/echo", h.data))
        .header(
            header::COOKIE,
            "a=1; __Host-fastmock_session=secret-value; b=2",
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "c=a=1; b=2");
}

#[tokio::test]
async fn provider_down_at_start_and_bad_discovery_documents() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let placeholder = Idp {
        url: format!("http://{addr}"),
        st: Arc::new(Mutex::new(IdpState::default())),
    };
    let h = H::start(StorageKind::Memory, &placeholder, &Opts::default()).await;
    let (st, _, _) = h.begin("/ui/").await;
    assert_eq!(st, 503, "SSO waits for discovery");
    let r = h
        .http
        .get(format!("{}/nothing", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "the data plane serves meanwhile");
    assert_eq!(h.bearer(Method::GET, "/info", ADMIN, None).await.0, 200);
    let l = tokio::net::TcpListener::bind(addr).await.unwrap();
    let idp = Idp::serve(l).await;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (st, _, _) = h.begin("/ui/").await;
        if st == 302 {
            break;
        }
        assert!(Instant::now() < deadline, "discovery was not retried");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let res = h
        .login(&idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await;
    assert!(res.session.is_some());

    let idp = Idp::start().await;
    idp.with(|s| s.jwks_uri_override = Some("https://keys.example.com/jwks".into()));
    let e = H::try_start(StorageKind::Memory, &idp, &Opts::default())
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap();
    assert!(
        e.contains("jwks_uri") && e.contains("FASTMOCK_OIDC_ALLOWED_HOSTS"),
        "{e}"
    );
    idp.with(|s| {
        s.jwks_uri_override = None;
        s.issuer_override = Some("https://other.example.com".into());
    });
    let e = H::try_start(StorageKind::Memory, &idp, &Opts::default())
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap();
    assert!(e.contains("not FASTMOCK_OIDC_ISSUER"), "{e}");
}

#[tokio::test]
async fn insecure_cookie_for_local_http() {
    let idp = Idp::start().await;
    let h = H::start(
        StorageKind::Memory,
        &idp,
        &Opts {
            insecure: true,
            ..Default::default()
        },
    )
    .await;
    let res = h
        .login(&idp, Some(user("alice", &["payments-dev"])), "/ui/")
        .await;
    let c = cookie_named(&res.cookies, "fastmock_session").unwrap();
    assert!(!c.contains("Secure"), "{c}");
    let s = res.session.unwrap();
    assert_eq!(
        h.with_session(Method::GET, "/info", &s, false, None)
            .await
            .0,
        200
    );
}
