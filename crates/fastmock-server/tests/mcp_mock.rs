//! Mocked MCP servers on the data plane: an MCP client lists and calls tools and resources served from
//! stubs, with argument matching, templates, JSON-RPC errors, delays and faults, and the calls are
//! journaled for verification and near misses. Runs on memory storage and, with
//! FASTMOCK_TEST_DATABASE_URL and FASTMOCK_TEST_VALKEY_URL, on PostgreSQL and Valkey.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
use rmcp::service::ServiceError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_mcp_mock_tests_0123456789";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn storages() -> Vec<(&'static str, StorageKind)> {
    let mut v = vec![("memory", StorageKind::Memory)];
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: PostgreSQL+Valkey part skipped"
        );
        return v;
    };
    let db = format!("fm_mcpm_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    v.push((
        "postgres+valkey",
        StorageKind::Postgres {
            database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
            valkey_url,
            valkey_prefix: format!("mcpm:{}:", uuid::Uuid::new_v4().simple()),
            max_connections: 5,
            options: Default::default(),
        },
    ));
    v
}

struct H {
    data: String,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl H {
    async fn start(storage: StorageKind) -> H {
        let mut cfg = Config::for_tests(storage, vec![format!("ops:admin:{}", hash_token(ADMIN))]);
        cfg.plugins = Some(vec![]);
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        tokio::spawn(fastmock_server::listen_notifications(app.clone(), rx));
        app.ready.store(true, Ordering::SeqCst);
        H {
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            _shutdown: tx,
        }
    }

    async fn admin(&self, m: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .http
            .request(m, format!("{}/__admin{path}", self.admin))
            .bearer_auth(ADMIN);
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

    async fn stub(&self, v: Value) {
        let (st, b) = self.admin(reqwest::Method::POST, "/stubs", Some(v)).await;
        assert_eq!(st, 201, "{b}");
    }

    async fn rpc(&self, body: &str) -> (u16, String) {
        let r = self
            .http
            .post(format!("{}/mcp", self.data))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn journal(&self, n: usize) -> Vec<Value> {
        for _ in 0..100 {
            let (_, v) = self
                .admin(reqwest::Method::GET, "/requests?limit=200", None)
                .await;
            let l = v["requests"].as_array().cloned().unwrap_or_default();
            if l.len() >= n {
                return l;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("journal never reached {n} entries");
    }
}

async fn weather_server(h: &H) {
    h.stub(json!({
        "name": "forecast for Kyiv",
        "priority": 1,
        "request": {"urlPath": "/mcp", "mcp": {"tool": "get_forecast",
            "arguments": [{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]}},
        "mcp": {
            "description": "Weather forecast for a city",
            "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}, "days": {"type": "integer"}}, "required": ["city"]},
            "annotations": {"readOnlyHint": true}
        },
        "response": {"jsonBody": {"city": "{{jsonPath request.body '$.params.arguments.city'}}", "temperature": 21, "sky": "sunny"}}
    }))
    .await;
    h.stub(json!({
        "name": "forecast for anything else",
        "priority": 5,
        "request": {"urlPath": "/mcp", "mcp": {"tool": "get_forecast"}},
        "response": {"body": "No forecast for {{jsonPath request.body '$.params.arguments.city'}}"}
    }))
    .await;
    h.stub(json!({
        "request": {"urlPath": "/mcp", "mcp": {"tool": "book_trip"}},
        "mcp": {"description": "Book a trip", "error": {"code": -32000, "message": "booking quota exceeded", "data": {"retryAfter": 60}}},
        "response": {}
    }))
    .await;
    h.stub(json!({
        "request": {"urlPath": "/mcp", "mcp": {"tool": "slow_search"}},
        "mcp": {"description": "A slow search"},
        "response": {"delay": {"fixedMs": 300}, "jsonBody": {"content": [{"type": "text", "text": "found 3"}]}}
    }))
    .await;
    h.stub(json!({
        "request": {"urlPath": "/mcp", "mcp": {"tool": "flaky"}},
        "mcp": {"description": "Fails half of the time"},
        "responses": [
            {"jsonBody": {"content": [{"type": "text", "text": "ok"}]}},
            {"jsonBody": {"isError": true, "content": [{"type": "text", "text": "upstream timeout"}]}}
        ],
        "responseMode": "cycle"
    }))
    .await;
    h.stub(json!({
        "request": {"urlPath": "/mcp", "mcp": {"tool": "broken"}},
        "mcp": {"description": "Drops the connection"},
        "response": {"fault": "CONNECTION_RESET"}
    }))
    .await;
    h.stub(json!({
        "name": "readme",
        "request": {"urlPath": "/mcp", "mcp": {"resource": "file:///docs/readme.md"}},
        "mcp": {"description": "Project readme", "mimeType": "text/markdown"},
        "response": {"body": "# Readme\nHello"}
    }))
    .await;
    h.stub(json!({
        "request": {"method": "GET", "urlPath": "/plain"},
        "response": {"status": 200, "body": "plain http"}
    }))
    .await;
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[tokio::test]
async fn an_mcp_client_uses_a_mocked_mcp_server() {
    for (label, storage) in storages().await {
        let h = H::start(storage).await;
        weather_server(&h).await;
        let client = ()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", h.data)),
            ))
            .await
            .unwrap();
        let info = client.peer_info().unwrap();
        assert_eq!(
            info.server_info.as_ref().unwrap().name,
            "fastmock",
            "{label}"
        );
        let mut tools = client.list_all_tools().await.unwrap();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                "book_trip",
                "broken",
                "flaky",
                "get_forecast",
                "slow_search"
            ],
            "{label}"
        );
        let f = tools.iter().find(|t| t.name == "get_forecast").unwrap();
        assert_eq!(
            f.description.as_deref(),
            Some("Weather forecast for a city")
        );
        assert_eq!(f.input_schema["required"], json!(["city"]));
        assert_eq!(f.annotations.as_ref().unwrap().read_only_hint, Some(true));

        let r = client
            .call_tool(
                CallToolRequestParams::new("get_forecast")
                    .with_arguments(args(json!({"city": "Kyiv", "days": 2}))),
            )
            .await
            .unwrap();
        let s = r.structured_content.clone().unwrap();
        assert_eq!(s["city"], "Kyiv", "{label}: {r:?}");
        assert_eq!(s["temperature"], 21);
        assert_ne!(r.is_error, Some(true));

        let r = client
            .call_tool(
                CallToolRequestParams::new("get_forecast")
                    .with_arguments(args(json!({"city": "Lviv"}))),
            )
            .await
            .unwrap();
        assert!(
            format!("{:?}", r.content).contains("No forecast for Lviv"),
            "{label}: {r:?}"
        );

        let e = client
            .call_tool(CallToolRequestParams::new("book_trip"))
            .await
            .unwrap_err();
        match e {
            ServiceError::McpError(d) => {
                assert_eq!(d.code.0, -32000, "{label}");
                assert_eq!(d.message, "booking quota exceeded");
                assert_eq!(d.data.unwrap()["retryAfter"], 60);
            }
            other => panic!("{label}: expected a JSON-RPC error, got {other:?}"),
        }

        let e = client
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, ServiceError::McpError(d) if d.code.0 == -32602 && d.message.contains("unknown tool")),
            "{label}: {e:?}"
        );

        let started = Instant::now();
        let r = client
            .call_tool(CallToolRequestParams::new("slow_search"))
            .await
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(280),
            "{label}: the delay applies"
        );
        assert!(format!("{:?}", r.content).contains("found 3"));

        let first = client
            .call_tool(CallToolRequestParams::new("flaky"))
            .await
            .unwrap();
        let second = client
            .call_tool(CallToolRequestParams::new("flaky"))
            .await
            .unwrap();
        assert_ne!(first.is_error, Some(true), "{label}");
        assert_eq!(second.is_error, Some(true), "{label}: {second:?}");

        let res = client.list_all_resources().await.unwrap();
        assert_eq!(res.len(), 1, "{label}");
        assert_eq!(res[0].uri, "file:///docs/readme.md");
        assert_eq!(res[0].mime_type.as_deref(), Some("text/markdown"));
        let doc = client
            .read_resource(ReadResourceRequestParams::new("file:///docs/readme.md"))
            .await
            .unwrap();
        assert!(
            format!("{:?}", doc.contents).contains("# Readme"),
            "{doc:?}"
        );
        client.cancel().await.unwrap();

        let broken = match h
            .http
            .post(format!("{}/mcp", h.data))
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"broken"}}"#)
            .send()
            .await
        {
            Err(_) => true,
            Ok(r) => r.text().await.is_err(),
        };
        assert!(broken, "{label}: the fault drops the answer");

        let r = h
            .http
            .get(format!("{}/plain", h.data))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.text().await.unwrap(),
            "plain http",
            "{label}: HTTP stubs are untouched"
        );

        let (st, b) = h
            .admin(
                reqwest::Method::POST,
                "/verify",
                Some(json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "get_forecast",
                    "arguments": [{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]}}, "count": 1})),
            )
            .await;
        assert_eq!(st, 200, "{label}: {b}");
        let (st, b) = h
            .admin(
                reqwest::Method::POST,
                "/verify",
                Some(json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "get_forecast"}}, "count": 2})),
            )
            .await;
        assert_eq!(st, 200, "{label}: {b}");
        let entries = h.journal(10).await;
        let methods: Vec<String> = entries
            .iter()
            .filter(|e| e["path"] == "/mcp")
            .filter_map(|e| {
                serde_json::from_str::<Value>(e["body"].as_str()?)
                    .ok()
                    .and_then(|b| b["method"].as_str().map(String::from))
            })
            .collect();
        for m in ["initialize", "tools/list", "tools/call", "resources/read"] {
            assert!(
                methods.iter().any(|x| x == m),
                "{label}: {m} journaled: {methods:?}"
            );
        }
        let unknown = entries
            .iter()
            .find(|e| {
                e["body"]
                    .as_str()
                    .is_some_and(|b| b.contains("no_such_tool"))
            })
            .unwrap();
        assert_eq!(unknown["matched"], false, "{label}: {unknown}");
        let (st, near) = h
            .admin(
                reqwest::Method::POST,
                "/requests/near-misses",
                Some(json!({"method": "POST", "path": "/mcp",
                    "body": r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_forecast","arguments":{"town":"Kyiv"}}}"#})),
            )
            .await;
        assert_eq!(st, 200, "{near}");
        assert!(
            near.to_string()
                .contains("MCP argument matcher #0 ($.city) does not match"),
            "{label}: {near}"
        );
    }
}

#[tokio::test]
async fn the_transport_follows_streamable_http() {
    let h = H::start(StorageKind::Memory).await;
    weather_server(&h).await;
    let r = h.http.get(format!("{}/mcp", h.data)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 405);
    assert_eq!(r.headers()["allow"], "POST");
    let (st, b) = h
        .rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .await;
    assert_eq!((st, b.as_str()), (202, ""));
    let (st, b) = h.rpc(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
    assert_eq!(st, 400, "{b}");
    let (st, b) = h.rpc("not json").await;
    assert_eq!(st, 400);
    assert!(b.contains("-32700"), "{b}");
    let (st, b) = h
        .rpc(r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#)
        .await;
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["id"], "a");
    assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
    assert!(v["result"]["capabilities"]["resources"].is_object(), "{v}");
    let (_, b) = h
        .rpc(r#"{"jsonrpc":"2.0","id":2,"method":"server/discover","params":{}}"#)
        .await;
    assert!(b.contains("-32601"), "{b}");
    let (_, b) = h.rpc(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#).await;
    assert_eq!(
        serde_json::from_str::<Value>(&b).unwrap()["result"],
        json!({})
    );
}

#[tokio::test]
async fn mcp_stubs_are_validated() {
    let h = H::start(StorageKind::Memory).await;
    for (bad, why) in [
        (
            json!({"request": {"method": "GET", "urlPath": "/mcp", "mcp": {"tool": "t"}}, "response": {}}),
            "POST",
        ),
        (
            json!({"request": {"urlPath": "/mcp"}, "mcp": {"description": "x"}, "response": {}}),
            "request.mcp",
        ),
        (
            json!({"request": {"urlPath": "/mcp", "mcp": {}}, "response": {}}),
            "tool, resource or method",
        ),
        (
            json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "t"}}, "mcp": {"inputSchema": {"type": 7}}, "response": {}}),
            "schema",
        ),
        (
            json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "t", "arguments": [{"jsonPath": "city"}]}}, "response": {}}),
            "must start with $",
        ),
        (
            json!({"request": {"urlPath": "/mcp", "mcp": {"tool": "t"}}, "response": {"proxy": {"baseUrl": "https://api.example.com"}}}),
            "proxy",
        ),
    ] {
        let (st, b) = h
            .admin(reqwest::Method::POST, "/stubs", Some(bad.clone()))
            .await;
        assert_eq!(st, 400, "{bad}: {b}");
        assert!(b.to_string().contains(why), "{bad}: {b}");
    }
    let (st, b) = h
        .admin(
            reqwest::Method::POST,
            "/stubs",
            Some(
                json!({"request": {"urlPath": "/mcp", "mcp": {"method": "prompts/list"}},
                "response": {"jsonBody": {"prompts": [{"name": "summarize"}]}}}),
            ),
        )
        .await;
    assert_eq!(st, 201, "{b}");
    let (_, b) = h
        .rpc(r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#)
        .await;
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["result"]["prompts"][0]["name"], "summarize", "{v}");
}
