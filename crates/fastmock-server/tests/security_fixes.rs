use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, ContractSecret, StorageKind};
use serde_json::{Value, json};
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_security_tests_01";
const EDITOR: &str = "fm_editor_token_for_security_tests_0";
const VIEWER: &str = "fm_viewer_token_for_security_tests_0";
const SECRET: &str = "Bearer s3cr3t-contract-value";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn config() -> Config {
    Config::for_tests(
        StorageKind::Memory,
        vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("ci:editor:{}", hash_token(EDITOR)),
            format!("dash:viewer:{}", hash_token(VIEWER)),
        ],
    )
}

struct Harness {
    data: String,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl Harness {
    async fn start(mut cfg: Config) -> Harness {
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        cfg.listen = da;
        cfg.admin_listen = aa;
        let (app, data, admin, policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        tokio::spawn(fastmock_server::outbox::run(
            app.clone(),
            policy,
            rx.clone(),
        ));
        app.ready.store(true, Ordering::SeqCst);
        Harness {
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            _shutdown: tx,
        }
    }

    async fn call(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut rb = self
            .http
            .request(method, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (st, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn wait_run(&self, id: &str) -> Value {
        for _ in 0..200 {
            let (st, v) = self
                .call(
                    ADMIN,
                    reqwest::Method::GET,
                    &format!("/contract-tests/{id}"),
                    None,
                )
                .await;
            assert_eq!(st, 200, "{v}");
            if v["status"] != "running" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("contract test run {id} did not finish");
    }

    async fn journal_count(&self) -> i64 {
        let (_, v) = self
            .call(VIEWER, reqwest::Method::GET, "/requests/count", None)
            .await;
        v["count"].as_i64().unwrap_or(0)
    }
}

async fn echo(headers: HeaderMap) -> impl IntoResponse {
    let h: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                json!(v.to_str().unwrap_or_default()),
            )
        })
        .collect();
    let probe = headers
        .get("x-probe")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (
        [("content-type", "text/plain")],
        format!("got={probe} host={host} all={}", Value::Object(h)),
    )
}

async fn echo_server() -> String {
    let app = Router::new()
        .route("/echo", get(echo))
        .route("/{*rest}", any(echo));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

const ECHO_SPEC: &str = r##"
openapi: 3.0.3
info: {title: Echo, version: "1"}
paths:
  /echo:
    get:
      operationId: echo
      parameters:
        - {name: Host, in: header, required: true, schema: {type: string}, example: internal-admin.example.com}
      responses:
        "200":
          description: ok
          content:
            text/plain:
              schema: {type: string}
"##;

#[tokio::test]
async fn contract_secrets_are_bound_to_hosts_and_never_reach_fastmock() {
    let dir = std::env::temp_dir().join(format!("fm-sec-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pets-token"), format!("{SECRET}\n")).unwrap();
    std::fs::write(dir.join("other-token"), "another-long-secret-value").unwrap();
    std::fs::write(dir.join("short-token"), "abc").unwrap();
    std::fs::write(dir.join("hook-key"), "hmac-signing-key-value").unwrap();
    let mut cfg = config();
    cfg.secrets_dir = Some(dir.clone());
    cfg.stub_secrets = vec!["hook-key".into()];
    cfg.contract_secrets = vec![
        ContractSecret {
            name: "pets-token".into(),
            hosts: vec!["127.0.0.1".into()],
        },
        ContractSecret {
            name: "other-token".into(),
            hosts: vec!["api.example.com".into()],
        },
        ContractSecret {
            name: "short-token".into(),
            hosts: vec!["127.0.0.1".into()],
        },
    ];
    let h = Harness::start(cfg).await;
    let target = echo_server().await;
    let start = |target: &str, secret: &str| json!({"spec": ECHO_SPEC, "targetUrl": target, "secretHeaders": {"X-Probe": secret}});

    let (st, v) = h
        .call(
            EDITOR,
            reqwest::Method::POST,
            "/contract-tests",
            Some(start(&target, "pets-token")),
        )
        .await;
    assert_eq!(st, 403, "editors cannot send secrets: {v}");

    for (body, needle) in [
        (start(&h.data, "pets-token"), "this FastMock instance"),
        (start(&h.admin, "pets-token"), "this FastMock instance"),
        (
            json!({"spec": ECHO_SPEC, "targetUrl": h.data}),
            "this FastMock instance",
        ),
        (start(&target, "other-token"), "not bound to host 127.0.0.1"),
        (start(&target, "hook-key"), "not configured"),
        (start(&target, "missing"), "not configured"),
        (start(&target, "short-token"), "shorter than"),
        (
            json!({"spec": ECHO_SPEC, "targetUrl": target, "secretHeaders": {"Host": "pets-token"}}),
            "cannot be set",
        ),
    ] {
        let (st, v) = h
            .call(ADMIN, reqwest::Method::POST, "/contract-tests", Some(body))
            .await;
        assert_eq!(st, 400, "{needle}: {v}");
        assert!(
            v["error"].as_str().unwrap().contains(needle),
            "{needle}: {v}"
        );
    }

    let (st, v) = h
        .call(
            ADMIN,
            reqwest::Method::POST,
            "/contract-tests",
            Some(start(
                &format!("{target}/?access_token=qs-secret-1"),
                "pets-token",
            )),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let run = h.wait_run(&id).await;
    assert_eq!(run["status"], "passed", "{run}");
    let text = run.to_string();
    assert!(!text.contains("s3cr3t"), "{text}");
    assert!(!text.contains("qs-secret-1"), "{text}");
    let r = &run["results"][0];
    assert_eq!(r["request"]["headers"]["x-probe"], "***");
    let excerpt = r["response"]["bodyExcerpt"].as_str().unwrap();
    assert!(excerpt.starts_with("got=*** "), "{excerpt}");
    let port = target.rsplit(':').next().unwrap();
    assert!(
        excerpt.contains(&format!("host=127.0.0.1:{port} ")),
        "the spec's Host parameter is never sent: {excerpt}"
    );

    let (st, viewer) = h
        .call(
            VIEWER,
            reqwest::Method::GET,
            &format!("/contract-tests/{id}"),
            None,
        )
        .await;
    assert_eq!(st, 200);
    assert_eq!(viewer["detailsHidden"], true);
    let vr = &viewer["results"][0];
    assert_eq!(vr["verdict"], "passed");
    assert_eq!(vr["response"]["status"], 200);
    assert_eq!(vr["response"]["bodyExcerpt"], "");
    assert!(vr.get("request").is_none(), "{vr}");
    assert!(!viewer.to_string().contains("got="), "{viewer}");

    let r = h
        .http
        .get(format!("{}/anything", h.data))
        .header("x-note", format!("echo {SECRET}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    for _ in 0..50 {
        if h.journal_count().await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, journal) = h
        .call(VIEWER, reqwest::Method::GET, "/requests?limit=5", None)
        .await;
    let j = journal.to_string();
    assert!(j.contains("x-note"), "{j}");
    assert!(
        !j.contains("s3cr3t"),
        "the journal masks contract secrets: {j}"
    );

    for headers in [
        json!({"Host": "internal.example.com"}),
        json!({"X-Forwarded-Host": "x"}),
        json!({"Transfer-Encoding": "chunked"}),
    ] {
        let (st, v) = h
            .call(
                EDITOR,
                reqwest::Method::POST,
                "/contract-tests",
                Some(json!({"spec": ECHO_SPEC, "targetUrl": target, "headers": headers})),
            )
            .await;
        assert_eq!(st, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("cannot be set"),
            "{v}"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn outbound_header_filter_covers_proxies_and_callbacks() {
    let h = Harness::start(config()).await;
    let upstream = echo_server().await;
    for (stub, needle) in [
        (
            json!({"request": {"urlPathPattern": "/px/.*"}, "response": {"proxy": {"baseUrl": upstream, "additionalHeaders": {"Host": "internal-admin.example.com"}}}}),
            "proxy.additionalHeaders",
        ),
        (
            json!({"request": {"urlPath": "/cb"}, "response": {"status": 200}, "callbacks": [{"url": "https://hooks.example.com/", "headers": {"X-Forwarded-Host": "x"}}]}),
            "callbacks[0].headers",
        ),
    ] {
        let (st, v) = h
            .call(EDITOR, reqwest::Method::POST, "/stubs", Some(stub))
            .await;
        assert_eq!(st, 400, "{v}");
        let e = v["error"].as_str().unwrap();
        assert!(e.contains(needle) && e.contains("cannot be set"), "{e}");
    }
    let (st, v) = h
        .call(
            EDITOR,
            reqwest::Method::POST,
            "/stubs",
            Some(json!({"request": {"urlPathPattern": "/px/.*"}, "response": {"proxy": {"baseUrl": upstream, "stripPrefix": "/px", "additionalHeaders": {"X-Extra": "1"}}}})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let body = h
        .http
        .get(format!("{}/px/whoami", h.data))
        .header("x-forwarded-host", "evil.example.com")
        .header("forwarded", "host=evil.example.com")
        .header("x-original-url", "/admin")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let port = upstream.rsplit(':').next().unwrap();
    assert!(body.contains(&format!("host=127.0.0.1:{port} ")), "{body}");
    assert!(body.contains("\"x-extra\":\"1\""), "{body}");
    assert!(!body.contains("evil.example.com"), "{body}");
    assert!(!body.contains("x-original-url"), "{body}");
}

#[tokio::test]
async fn contract_tests_read_only_listed_kafka_topics() {
    let spec = |topic: &str| {
        json!({"spec": {"asyncapi": "2.6.0", "info": {"title": "t", "version": "1"},
            "servers": {"k": {"url": "kafka.example.com:9092", "protocol": "kafka"}},
            "channels": {topic: {"subscribe": {"message": {"payload": {"type": "object"}}}}}},
            "waitMs": 100})
    };
    let h = Harness::start(config()).await;
    let (st, v) = h
        .call(
            EDITOR,
            reqwest::Method::POST,
            "/contract-tests",
            Some(spec("orders")),
        )
        .await;
    assert_eq!(st, 400, "reading is disabled by default: {v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("FASTMOCK_KAFKA_CONTRACT_TOPICS"),
        "{v}"
    );
    let mut cfg = config();
    cfg.kafka_contract_topics = vec!["orders-*".into(), "*".into()];
    let h = Harness::start(cfg).await;
    for topic in ["__consumer_offsets", "_schemas"] {
        let (st, v) = h
            .call(
                EDITOR,
                reqwest::Method::POST,
                "/contract-tests",
                Some(spec(topic)),
            )
            .await;
        assert_eq!(st, 400, "{topic}: {v}");
    }
    let (st, v) = h
        .call(
            EDITOR,
            reqwest::Method::POST,
            "/contract-tests",
            Some(spec("orders-eu")),
        )
        .await;
    assert_eq!(st, 202, "{v}");
}

#[tokio::test]
async fn viewers_never_see_credentials_in_stubs() {
    let h = Harness::start(config()).await;
    let stub = json!({
        "name": "masktest",
        "request": {
            "url": "/m?api_key=URLSECRET0&page=2",
            "query": {"access_token": {"equalTo": "QSECRET1"}, "page": {"equalTo": "visible-page"}},
            "cookies": {"session": {"equalTo": "CSECRET2"}},
            "headers": {"X-Api-Key": {"equalTo": "HSECRET3"}, "Accept": {"equalTo": "visible/accept"}},
            "basicAuth": {"username": "visible-user", "password": "BSECRET4"},
            "body": [
                {"equalToJson": {"password": "JSECRET5", "user": "visible-json"}},
                {"contains": "{\"token\": \"TSECRET6\"}"},
                {"jsonPath": "$.password", "value": {"equalTo": "PSECRET7"}},
                {"jsonPath": "$[?(@.secret == 'FILTERSECRET')]"}
            ],
            "formParameters": {"client_secret": {"equalTo": "FSECRET8"}},
            "multipart": [{"name": "password", "body": [{"equalTo": "MSECRET9"}]}]
        },
        "response": {
            "status": 200,
            "headers": {"Set-Cookie": "sid=RSECRET11", "X-Visible": "visible-header"},
            "jsonBody": {"access_token": "RSECRET10", "ok": true}
        },
        "callbacks": [{
            "url": "https://hooks.example.com/cb?token=CBSECRET13&x=1",
            "headers": {"X-Signature-Token": "CBSECRET14"},
            "jsonBody": {"secret": "CBSECRET15"}
        }],
        "messages": [{"topic": "t", "jsonValue": {"password": "MGSECRET16"}, "headers": {"x-auth-token": "MGSECRET17"}}],
        "metadata": {"apiKey": "MDSECRET18", "team": "visible-team"}
    });
    let (st, v) = h
        .call(EDITOR, reqwest::Method::POST, "/stubs", Some(stub))
        .await;
    assert_eq!(st, 201, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let (st, v) = h
        .call(
            EDITOR,
            reqwest::Method::POST,
            "/stubs",
            Some(json!({"name": "maskproxy", "request": {"urlPathPattern": "/mp/.*"},
                "response": {"proxy": {"baseUrl": "https://upstream.example.com", "additionalHeaders": {"Authorization": "Bearer PXSECRET12"}}},
                "responses": []})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let (st, v) = h
        .call(
            EDITOR,
            reqwest::Method::POST,
            "/stubs",
            Some(json!({"name": "maskseq", "request": {"urlPath": "/seq"},
                "responses": [{"status": 200, "body": "{\"refresh_token\": \"SQSECRET19\"}"}]})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let secrets = [
        "URLSECRET0",
        "QSECRET1",
        "CSECRET2",
        "HSECRET3",
        "BSECRET4",
        "JSECRET5",
        "TSECRET6",
        "PSECRET7",
        "FILTERSECRET",
        "FSECRET8",
        "MSECRET9",
        "RSECRET10",
        "RSECRET11",
        "PXSECRET12",
        "CBSECRET13",
        "CBSECRET14",
        "CBSECRET15",
        "MGSECRET16",
        "MGSECRET17",
        "MDSECRET18",
        "SQSECRET19",
    ];
    let visible = [
        "visible-page",
        "visible/accept",
        "visible-user",
        "visible-json",
        "visible-header",
        "visible-team",
        "page=2",
        "x=1",
    ];
    let (_, editor_view) = h.call(EDITOR, reqwest::Method::GET, "/stubs", None).await;
    let e = editor_view.to_string();
    for s in secrets {
        assert!(e.contains(s), "editors see {s}");
    }
    for path in [
        "/stubs".to_string(),
        "/stubs?q=mask".to_string(),
        format!("/stubs/{id}"),
        "/export".to_string(),
        "/stubs/unused".to_string(),
    ] {
        let (st, v) = h.call(VIEWER, reqwest::Method::GET, &path, None).await;
        assert_eq!(st, 200, "{path}: {v}");
        let text = v.to_string();
        for s in secrets {
            assert!(!text.contains(s), "{path} leaks {s}: {text}");
        }
        if path != "/stubs/unused" {
            for s in visible {
                assert!(text.contains(s), "{path} still shows {s}: {text}");
            }
        }
    }
    let (st, v) = h
        .call(
            VIEWER,
            reqwest::Method::POST,
            "/stubs/find-by-metadata",
            Some(json!({"matchesJsonPath": "$.team"})),
        )
        .await;
    if st == 200 {
        let text = v.to_string();
        for s in secrets {
            assert!(!text.contains(s), "find-by-metadata leaks {s}: {text}");
        }
    }
}

fn heavy_pattern() -> Value {
    let mut body: Vec<Value> = (1..64)
        .map(|n| json!({"matches": format!("(?s)[a-p\\n]*[a-p]{{{n}}}[a-p\\n]*")}))
        .collect();
    body.push(json!({"matches": "(?s).*q.*"}));
    json!({"method": "POST", "body": body})
}

async fn fill_journal(h: &Harness, n: usize) {
    let line: String = "abcdefghijklmnop".repeat(4);
    let body: String = std::iter::repeat_n(line, 900)
        .collect::<Vec<_>>()
        .join("\n");
    let mut tasks = Vec::new();
    for _ in 0..n {
        let rb = h
            .http
            .post(format!("{}/unmatched", h.data))
            .body(body.clone());
        tasks.push(tokio::spawn(
            async move { rb.send().await.unwrap().status() },
        ));
    }
    for t in tasks {
        t.await.unwrap();
    }
    for _ in 0..200 {
        if h.journal_count().await >= n as i64 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the journal did not fill");
}

#[tokio::test]
async fn journal_scans_have_a_deadline_and_a_work_budget() {
    let mut cfg = config();
    cfg.journal_scan_timeout = Duration::from_millis(300);
    let h = Harness::start(cfg).await;
    fill_journal(&h, 120).await;
    for (path, body) in [
        ("/requests/find", heavy_pattern()),
        ("/verify", json!({"request": heavy_pattern(), "atLeast": 1})),
        (
            "/verify/sequence",
            json!({"requests": [heavy_pattern(), {"urlPath": "/x"}]}),
        ),
    ] {
        let t = Instant::now();
        let (st, v) = h
            .call(VIEWER, reqwest::Method::POST, path, Some(body))
            .await;
        assert_eq!(st, 503, "{path}: {v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("FASTMOCK_JOURNAL_SCAN_TIMEOUT"),
            "{v}"
        );
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{path}: {:?}",
            t.elapsed()
        );
    }
    let (st, v) = h
        .call(
            VIEWER,
            reqwest::Method::POST,
            "/requests/find",
            Some(json!({"urlPath": "/unmatched"})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["total"], 120);
    let (st, v) = h
        .call(
            VIEWER,
            reqwest::Method::POST,
            "/requests/near-misses",
            Some(json!({"method": "GET", "path": "/x", "body": "x".repeat(1024 * 1024 + 1)})),
        )
        .await;
    assert_eq!(st, 422, "{v}");
    let (st, _) = h
        .call(
            VIEWER,
            reqwest::Method::POST,
            "/requests/count",
            Some(json!({"method": "POST"})),
        )
        .await;
    assert_eq!(st, 200);

    let mut cfg = config();
    cfg.journal_scan_max_work = 10_000;
    let small = Harness::start(cfg).await;
    fill_journal(&small, 5).await;
    let (st, v) = small
        .call(
            VIEWER,
            reqwest::Method::POST,
            "/requests/find",
            Some(json!({"method": "POST", "body": [{"contains": "zzz"}]})),
        )
        .await;
    assert_eq!(st, 422, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("FASTMOCK_JOURNAL_SCAN_MAX_WORK"),
        "{v}"
    );
}

#[tokio::test]
async fn journal_scans_stop_when_the_client_disconnects() {
    let mut cfg = config();
    cfg.journal_scan_timeout = Duration::from_secs(60);
    cfg.journal_scan_concurrency = 1;
    let h = Harness::start(cfg).await;
    fill_journal(&h, 300).await;
    let r = h
        .http
        .post(format!("{}/__admin/requests/find", h.admin))
        .bearer_auth(VIEWER)
        .json(&heavy_pattern())
        .timeout(Duration::from_millis(500))
        .send()
        .await;
    assert!(
        r.is_err(),
        "the heavy scan is still running when the client gives up"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    let (st, v) = h
        .call(
            VIEWER,
            reqwest::Method::POST,
            "/requests/find",
            Some(json!({"urlPath": "/nothing"})),
        )
        .await;
    assert_eq!(st, 200, "the only scan slot was released: {v}");
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
}
