use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

const EDITOR: &str = "fm_parity2_editor_token_0123456789abcd";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

struct Harness {
    data: String,
    data_addr: std::net::SocketAddr,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl Harness {
    async fn start() -> Harness {
        let tokens = vec![format!("ci:editor:{}", hash_token(EDITOR))];
        let cfg = Config::for_tests(StorageKind::Memory, tokens);
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(fastmock_server::tls::serve(
            dl,
            data,
            None,
            rx.clone(),
            Duration::from_secs(5),
            Duration::from_secs(1),
        ));
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        Harness {
            data: format!("http://{da}"),
            data_addr: da,
            admin: format!("http://{aa}/__admin"),
            http: reqwest::Client::new(),
            _shutdown: tx,
        }
    }

    async fn admin_raw(
        &self,
        method: reqwest::Method,
        path: &str,
        ct: &str,
        body: String,
    ) -> (u16, Value) {
        let r = self
            .http
            .request(method, format!("{}{path}", self.admin))
            .bearer_auth(EDITOR)
            .header("content-type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.admin_raw(
            reqwest::Method::POST,
            path,
            "application/json",
            body.to_string(),
        )
        .await
    }

    async fn get(&self, path: &str) -> (u16, String) {
        let r = self
            .http
            .get(format!("{}{path}", self.data))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn raw(&self, request: &str) -> Vec<u8> {
        let mut s = tokio::net::TcpStream::connect(self.data_addr)
            .await
            .unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        out
    }

    async fn wait_requests(&self, n: u64) {
        for _ in 0..100 {
            let r = self
                .http
                .get(format!("{}/requests/count", self.admin))
                .bearer_auth(EDITOR)
                .send()
                .await
                .unwrap();
            let v: Value = r.json().await.unwrap();
            if v["count"].as_u64().unwrap_or(0) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the journal did not reach {n} requests");
    }
}

#[tokio::test]
async fn d2_empty_or_broken_imports_change_nothing() {
    let h = Harness::start().await;
    let (st, v) = h
        .post("/import/wiremock", json!({"tag": "keepme", "mappings": {"mappings": [{"request": {"urlPath": "/keep"}, "response": {"body": "kept"}}]}}))
        .await;
    assert_eq!(st, 200, "{v}");
    for doc in [
        json!(5),
        json!({"mapings": [{"request": {"urlPath": "/keep"}}]}),
        json!({"mappings": []}),
    ] {
        let (st, v) = h
            .post(
                "/import/wiremock",
                json!({"tag": "keepme", "mappings": doc}),
            )
            .await;
        assert_eq!(st, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("nothing was changed"),
            "{v}"
        );
    }
    let (st, v) = h
        .admin_raw(
            reqwest::Method::POST,
            "/import?tag=keepme&format=wiremock",
            "application/json",
            r#"{"mappings": 5}"#.into(),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = h
        .post(
            "/import/mockserver?tag=keepme",
            json!([{"httpRequest": {"path": "/keep"},
            "httpResponseTemplate": {"templateType": "VELOCITY", "template": "{}"}}]),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("VELOCITY"), "{v}");
    let (st, v) = h
        .post("/import/openapi", json!({"tag": "keepme", "spec": {"openapi": "3.0.3", "info": {"title": "t", "version": "1"}, "paths": {}}}))
        .await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = h
        .post("/import/asyncapi", json!({"tag": "keepme", "spec": {"asyncapi": "2.6.0", "info": {"title": "t", "version": "1"},
            "channels": {"a": {"publish": {"message": {"payload": {"type": "string"}}}}}}}))
        .await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(h.get("/keep").await, (200, "kept".to_string()));

    let (st, v) = h
        .post("/import/wiremock", json!({"tag": "keepme", "mappings": [{"request": {"urlPath": "/new"}, "response": {"body": "new"}}, {"response": {}}]}))
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["imported"], 1);
    assert!(
        v["warnings"][0].as_str().unwrap().contains("no request"),
        "{v}"
    );
    assert_eq!(h.get("/keep").await.0, 404);
}

#[tokio::test]
async fn d3_empty_response_sends_nothing() {
    let h = Harness::start().await;
    let (st, v) = h
        .post("/import/wiremock", json!({"tag": "f", "mappings": {"request": {"urlPath": "/empty"}, "response": {"fault": "EMPTY_RESPONSE"}}}))
        .await;
    assert_eq!(st, 200, "{v}");
    let raw = h.raw("GET /empty HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert!(raw.is_empty(), "{:?}", String::from_utf8_lossy(&raw));
    assert!(
        h.http
            .get(format!("{}/empty", h.data))
            .send()
            .await
            .is_err()
    );
    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let r = h2.get(format!("{}/empty", h.data)).send().await;
    assert!(
        r.is_err(),
        "HTTP/2 stream must be reset before any response: {r:?}"
    );
    assert_eq!(h.get("/__nothing").await.0, 404, "the server keeps serving");
}

#[tokio::test]
async fn d10_imported_tie_breaks_over_http() {
    let h = Harness::start().await;
    let (st, v) = h
        .post(
            "/import/wiremock",
            json!({"tag": "wm", "mappings": {"mappings": [
            {"request": {"urlPathPattern": "/spec/.*"}, "response": {"body": "pattern-first"}},
            {"request": {"urlPath": "/spec/x"}, "response": {"body": "exact-second"}}]}}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(h.get("/spec/x").await.1, "pattern-first");
    let (st, v) = h
        .post(
            "/import/mockserver?tag=ms",
            json!([
            {"httpRequest": {"path": "/ms/same"}, "httpResponse": {"body": "first"}},
            {"httpRequest": {"path": "/ms/same"}, "httpResponse": {"body": "second"}}]),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(h.get("/ms/same").await.1, "first");
}

#[tokio::test]
async fn d13_snapshot_keeps_the_call_sequence() {
    let h = Harness::start().await;
    let (st, v) = h
        .post("/stubs", json!({"request": {"urlPath": "/api/job/7"}, "responseMode": "sequential", "responses": [
            {"jsonBody": {"state": "RUNNING"}}, {"jsonBody": {"state": "RUNNING"}},
            {"jsonBody": {"state": "DONE"}}, {"jsonBody": {"state": "DONE"}}]}))
        .await;
    assert_eq!(st, 201, "{v}");
    for _ in 0..4 {
        assert_eq!(h.get("/api/job/7").await.0, 200);
    }
    h.wait_requests(4).await;
    let (st, v) = h
        .post(
            "/recordings/snapshot",
            json!({"repeatsAsScenarios": true, "persist": false, "tag": "snap"}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let states: Vec<String> = v["stubs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            s["response"]["jsonBody"]["state"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(states, ["RUNNING", "RUNNING", "DONE"], "{v}");
    let sc: Vec<(Value, Value)> = v["stubs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["scenario"]["requiredState"].clone(),
                s["scenario"]["newState"].clone(),
            )
        })
        .collect();
    assert_eq!(
        sc,
        vec![
            (json!("Started"), json!("call-2")),
            (json!("call-2"), json!("call-3")),
            (json!("call-3"), Value::Null)
        ]
    );
}

#[tokio::test]
async fn d14_set_cookie_lines_are_not_folded() {
    let h = Harness::start().await;
    let (st, v) = h
        .post("/import/wiremock", json!({"tag": "c", "mappings": {"request": {"urlPath": "/cookies"},
            "response": {"headers": {"Set-Cookie": ["a=1; Expires=Wed, 21 Oct 2027 07:28:00 GMT", "b=2"]}}}}))
        .await;
    assert_eq!(st, 200, "{v}");
    let r = h
        .http
        .get(format!("{}/cookies", h.data))
        .send()
        .await
        .unwrap();
    let cookies: Vec<&str> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(
        cookies,
        ["a=1; Expires=Wed, 21 Oct 2027 07:28:00 GMT", "b=2"]
    );
    let (st, v) = h
        .post("/stubs", json!({"request": {"urlPath": "/native"}, "response": {"headers": {"set-cookie": ["x=1", "y={{request.query.v}}"]}}}))
        .await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(
        v["response"]["headers"]["set-cookie"],
        json!(["x=1", "y={{request.query.v}}"])
    );
    let r = h
        .http
        .get(format!("{}/native?v=2", h.data))
        .send()
        .await
        .unwrap();
    let cookies: Vec<&str> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(cookies, ["x=1", "y=2"]);
}

#[tokio::test]
async fn d19_microcks_topic_naming() {
    let h = Harness::start().await;
    let spec = json!({"asyncapi": "2.6.0", "info": {"title": "Streetlights Kafka API", "version": "1.0.0"},
        "servers": {"k": {"url": "kafka.example.com:9092", "protocol": "kafka"}},
        "channels": {"smartylighting/streetlights/1/0/event/{streetlightId}/lighting/measured": {
            "parameters": {"streetlightId": {"schema": {"type": "string"}, "examples": ["s1"]}},
            "subscribe": {"operationId": "receiveLightMeasurement", "message": {"payload": {"type": "object"}, "examples": [{"payload": {"lumens": 1}}]}}}}});
    let (st, v) = h
        .post("/import/asyncapi", json!({"spec": spec, "dryRun": true}))
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["asyncMocks"][0]["channel"],
        "smartylighting-streetlights-1-0-event-s1-lighting-measured"
    );
    let (st, v) = h
        .post(
            "/import/asyncapi",
            json!({"spec": spec, "dryRun": true, "topicNaming": "microcks"}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["asyncMocks"][0]["channel"],
        "StreetlightsKafkaAPI-1.0.0-smartylighting-streetlights-1-0-event-s1-lighting-measured"
    );
}

#[tokio::test]
async fn d20_body_errors_are_400_and_unmet_expectations_422() {
    let h = Harness::start().await;
    let (st, v) = h
        .post(
            "/stubs/find-by-metadata",
            json!({"matchesJsonPath": "$.a", "unknown": 1}),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = h
        .post(
            "/verify",
            json!({"request": {"urlPath": "/v"}, "atleast": 1}),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = h
        .post(
            "/verify",
            json!({"request": {"urlPath": "/v"}, "exactly": 1}),
        )
        .await;
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["expected"], "exactly 1");
    let (st, v) = h
        .post(
            "/verify",
            json!({"request": {"urlPath": "/v"}, "exactly": 0}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = h
        .post(
            "/verify",
            json!({"request": {"urlPath": "/v"}, "exactly": 0, "count": 0}),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = h
        .admin_raw(
            reqwest::Method::POST,
            "/import/mockserver",
            "application/json",
            "{not json".into(),
        )
        .await;
    assert_eq!(st, 400, "{v}");
}
