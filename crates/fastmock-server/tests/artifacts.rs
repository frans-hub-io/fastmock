use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const EDITOR: &str = "fm_editor_token_for_artifact_tests_01";
const VIEWER: &str = "fm_viewer_token_for_artifact_tests_01";

const PASTRY: &str = include_str!("../../fastmock-core/tests/fixtures/pastry-openapi.yaml");
const PASTRY_EXAMPLES: &str =
    include_str!("../../fastmock-core/tests/fixtures/pastry-examples.yaml");
const PASTRY_METADATA: &str =
    include_str!("../../fastmock-core/tests/fixtures/pastry-metadata.yaml");
const FILMS: &str = include_str!("../../fastmock-core/tests/fixtures/films.graphql");
const FILMS_EXAMPLES: &str = include_str!("../../fastmock-core/tests/fixtures/films-examples.yaml");
const SOAPUI: &str = include_str!("../../fastmock-core/tests/fixtures/hello-soapui-project.xml");
const WSDL: &str = include_str!("../../fastmock-core/tests/fixtures/hello.wsdl");
const PASTRY_ID: &str = "api-pastry-2.0:2.0.0";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

struct Harness {
    data: String,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl Harness {
    async fn start(storage: StorageKind) -> Harness {
        let cfg = Config::for_tests(
            storage,
            vec![
                format!("ci:editor:{}", hash_token(EDITOR)),
                format!("dash:viewer:{}", hash_token(VIEWER)),
            ],
        );
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, _rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        Harness {
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            _shutdown: tx,
        }
    }

    async fn admin(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = self
            .http
            .request(method, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn ok(&self, method: Method, path: &str, body: Value) -> Value {
        let (s, v) = self.admin(method, path, EDITOR, Some(body)).await;
        assert_eq!(s, 200, "{path}: {v}");
        v
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<String>,
    ) -> (u16, reqwest::header::HeaderMap, String) {
        let mut r = self.http.request(method, format!("{}{path}", self.data));
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        if let Some(b) = body {
            r = r.body(b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        (status, headers, resp.text().await.unwrap())
    }

    async fn json(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let (s, _, t) = self
            .call(
                method,
                path,
                &[("content-type", "application/json")],
                body.map(|b| b.to_string()),
            )
            .await;
        (s, serde_json::from_str(&t).unwrap_or(Value::String(t)))
    }
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    if let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) {
        let db = format!("fm_art_{}", uuid::Uuid::new_v4().simple());
        let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
        sqlx::query(&format!("create database {db}"))
            .execute(&mut c)
            .await
            .unwrap();
        v.push(StorageKind::Postgres {
            database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
            valkey_url,
            valkey_prefix: format!("art:{}:", uuid::Uuid::new_v4().simple()),
            max_connections: 5,
        });
    }
    v
}

#[tokio::test]
async fn openapi_microcks_dispatch_companions_and_catalog() {
    for storage in storages().await {
        let h = Harness::start(storage).await;
        let r = h
            .ok(
                Method::POST,
                "/import/openapi",
                json!({"spec": PASTRY, "tag": "pastry", "validateRequests": false}),
            )
            .await;
        assert!(r["warnings"].as_array().unwrap().is_empty(), "{r}");

        let (s, v) = h.json(Method::GET, "/pastry/Millefeuille", None).await;
        assert_eq!((s, v["size"].as_str()), (200, Some("L")));
        let (s, v) = h.json(Method::GET, "/pastry/Eclair%20Chocolat", None).await;
        assert_eq!(s, 200);
        assert_eq!(v["bakedAt"].as_str().unwrap().len(), 10, "{v}");
        let (s, v) = h.json(Method::GET, "/pastry/Croissant", None).await;
        assert_eq!(s, 404, "FALLBACK answers with the Unknown example");
        assert_eq!(v["error"], "pastry Croissant not found");
        let (_, v) = h.json(Method::GET, "/pastry?size=M", None).await;
        assert_eq!(v[0]["name"], "Eclair Cafe");
        let (s, v) = h
            .json(
                Method::POST,
                "/pastry",
                Some(json!({"name": "Big", "size": "L"})),
            )
            .await;
        assert_eq!(s, 201);
        assert_eq!(v["name"], "Big");
        assert!(v["note"].is_string());
        assert_eq!(v["id"].as_str().unwrap().len(), 36);

        let (s, list) = h.admin(Method::GET, "/services", VIEWER, None).await;
        assert_eq!(s, 200);
        let svc = list["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == PASTRY_ID)
            .unwrap()
            .clone();
        assert_eq!(svc["type"], "REST");
        assert_eq!(svc["sourceFormat"], "openapi");
        assert_eq!(svc["labels"]["domain"], "pastry");
        assert_eq!(svc["importedBy"], "ci");
        let op = svc["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == "GET /pastry/{name}")
            .unwrap();
        assert_eq!(op["stubIds"].as_array().unwrap().len(), 4, "{op}");
        let (s, spec) = h
            .admin(
                Method::GET,
                &format!("/services/{PASTRY_ID}/spec"),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(s, 200);
        assert_eq!(spec["document"]["info"]["title"], "API Pastry - 2.0");
        assert_eq!(spec["type"], "REST");

        let r = h
            .ok(
                Method::POST,
                "/import/microcks",
                json!({"document": PASTRY_EXAMPLES}),
            )
            .await;
        assert_eq!(r["imported"], 2, "{r}");
        assert_eq!(r["serviceId"], PASTRY_ID);
        let (s, v) = h.json(Method::GET, "/pastry/Croissant", None).await;
        assert_eq!((s, v["price"].as_f64()), (200, Some(1.2)), "{v}");
        let (_, v) = h.json(Method::GET, "/pastry?size=L", None).await;
        assert_eq!(v[0]["name"], "Paris-Brest");

        let r = h
            .ok(
                Method::POST,
                "/import/microcks",
                json!({"document": PASTRY_METADATA}),
            )
            .await;
        assert!(r["warnings"].as_array().unwrap().is_empty(), "{r}");
        let (_, svc) = h
            .admin(Method::GET, &format!("/services/{PASTRY_ID}"), VIEWER, None)
            .await;
        assert_eq!(svc["labels"]["team"], "pastry");
        assert_eq!(
            svc["overrides"]["operations"]["GET /pastry/{name}"]["delay"],
            25
        );
        assert_eq!(svc["artifacts"].as_array().unwrap().len(), 2);
        let started = std::time::Instant::now();
        let (s, _) = h.json(Method::GET, "/pastry/Millefeuille", None).await;
        assert_eq!(s, 200);
        assert!(started.elapsed() >= std::time::Duration::from_millis(25));

        h.ok(
            Method::POST,
            "/import/openapi",
            json!({"spec": PASTRY, "tag": "pastry", "validateRequests": false}),
        )
        .await;
        let (_, stubs) = h.admin(Method::GET, "/stubs", VIEWER, None).await;
        let mf = stubs["stubs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "getPastry [Millefeuille]")
            .unwrap();
        assert_eq!(
            mf["response"]["delay"]["fixedMs"], 25,
            "APIMetadata overrides survive a re-import"
        );
        let (s, _) = h.json(Method::GET, "/pastry/Croissant", None).await;
        assert_eq!(
            s, 200,
            "APIExamples stubs survive a re-import of the main artifact"
        );

        let (s, _) = h
            .admin(
                Method::DELETE,
                &format!("/services/{PASTRY_ID}"),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(s, 403);
        let (s, d) = h
            .admin(
                Method::DELETE,
                &format!("/services/{PASTRY_ID}"),
                EDITOR,
                None,
            )
            .await;
        assert_eq!(s, 200, "{d}");
        assert!(d["deletedStubs"].as_u64().unwrap() >= 10, "{d}");
        let (s, _) = h.json(Method::GET, "/pastry/Millefeuille", None).await;
        assert_eq!(s, 404);
        let (s, _) = h
            .admin(Method::GET, &format!("/services/{PASTRY_ID}"), VIEWER, None)
            .await;
        assert_eq!(s, 404);
        let (s, _) = h
            .admin(
                Method::GET,
                &format!("/services/{PASTRY_ID}/spec"),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(s, 404);
        let (s, v) = h
            .admin(
                Method::POST,
                "/import/microcks",
                EDITOR,
                Some(json!({"document": PASTRY_EXAMPLES})),
            )
            .await;
        assert_eq!(s, 404, "{v}");
    }
}

#[tokio::test]
async fn graphql_soapui_and_wsdl_mocks() {
    let h = Harness::start(StorageKind::Memory).await;
    let (s, _) = h
        .admin(
            Method::POST,
            "/import/graphql",
            VIEWER,
            Some(json!({"schema": FILMS})),
        )
        .await;
    assert_eq!(s, 403);
    let dry = h
        .ok(
            Method::POST,
            "/import/graphql",
            json!({"schema": FILMS, "dryRun": true}),
        )
        .await;
    assert_eq!(dry["dryRun"], true);
    assert_eq!(dry["stubs"].as_array().unwrap().len(), 4);
    let (_, list) = h.admin(Method::GET, "/services", VIEWER, None).await;
    assert!(
        list["services"].as_array().unwrap().is_empty(),
        "a dry run records nothing"
    );

    let r = h
        .ok(
            Method::POST,
            "/import/graphql",
            json!({"schema": FILMS, "examples": FILMS_EXAMPLES}),
        )
        .await;
    assert_eq!(r["imported"], 6, "{r}");
    assert_eq!(r["serviceId"], "movie-graph-api:1.0");
    let q = |query: &str, vars: Value| json!({"query": query, "variables": vars});
    let (s, v) = h
        .json(
            Method::POST,
            "/graphql",
            Some(q(
                "query film($id: ID!) { film(id: $id) { title } }",
                json!({"id": "ZmlsbXM6MQ=="}),
            )),
        )
        .await;
    assert_eq!(
        (s, v["data"]["film"]["title"].as_str()),
        (200, Some("A New Hope"))
    );
    let (_, v) = h
        .json(
            Method::POST,
            "/graphql",
            Some(q("{ allFilms { totalCount } }", json!({}))),
        )
        .await;
    assert_eq!(v["data"]["allFilms"]["totalCount"], 1);
    let (_, v) = h
        .json(
            Method::POST,
            "/graphql",
            Some(q("mutation { addStar(filmId: \"x\") { id } }", json!({}))),
        )
        .await;
    assert!(v["data"]["addStar"].is_object());
    let (s, _) = h
        .json(Method::POST, "/graphql", Some(q("{ nothing }", json!({}))))
        .await;
    assert_eq!(s, 404);

    let r = h
        .ok(Method::POST, "/import/soapui", json!({"document": SOAPUI}))
        .await;
    assert_eq!(r["serviceId"], "helloservice-mock:0.9");
    assert_eq!(r["warnings"].as_array().unwrap().len(), 1, "{r}");
    let env = |inner: &str| {
        format!(
            "<soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:hel=\"http://www.example.com/hello\"><soapenv:Body>{inner}</soapenv:Body></soapenv:Envelope>"
        )
    };
    let (s, hdr, body) = h
        .call(
            Method::POST,
            "/HelloService",
            &[
                ("content-type", "text/xml"),
                ("soapaction", "\"http://www.example.com/hello/sayHello\""),
            ],
            Some(env("<hel:sayHello><name>Karla</name></hel:sayHello>")),
        )
        .await;
    assert_eq!(s, 200);
    assert!(body.contains("Hello Karla !"), "{body}");
    assert!(
        hdr["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/xml")
    );
    let (_, hdr, body) = h
        .call(
            Method::POST,
            "/HelloService",
            &[
                ("content-type", "text/xml"),
                ("soapaction", "http://www.example.com/hello/sayHello"),
            ],
            Some(env("<hel:sayHello><name>Andrew</name></hel:sayHello>")),
        )
        .await;
    assert!(body.contains("Hello Andrew !"));
    assert!(
        !body.contains("{{"),
        "SoapUI ${{}} expression rendered: {body}"
    );
    assert_eq!(hdr["x-greeting"], "andrew");
    let (s, _, _) = h
        .call(
            Method::POST,
            "/HelloService",
            &[
                ("content-type", "text/xml"),
                ("soapaction", "http://www.example.com/hello/sayHello"),
            ],
            Some(env("<hel:sayHello><name>Bob</name></hel:sayHello>")),
        )
        .await;
    assert_eq!(s, 500, "default response of QUERY_MATCH");
    let (_, _, body) = h
        .call(
            Method::POST,
            "/HelloService",
            &[("content-type", "text/xml")],
            Some(env(
                "<hel:getBalance><msisdn>380501234567</msisdn></hel:getBalance>",
            )),
        )
        .await;
    assert!(body.contains("10.50"), "{body}");
    let mut pongs = Vec::new();
    for _ in 0..3 {
        let (_, _, body) = h
            .call(
                Method::POST,
                "/HelloService",
                &[("content-type", "text/xml"), ("soapaction", "urn:ping")],
                Some(env("<ping/>")),
            )
            .await;
        pongs.push(body.contains("<pong>1</pong>"));
    }
    assert_eq!(
        pongs,
        vec![true, false, true],
        "SEQUENCE dispatch cycles through the responses"
    );

    let r = h
        .ok(
            Method::POST,
            "/import/wsdl",
            json!({"document": WSDL, "version": "2.1"}),
        )
        .await;
    assert_eq!(r["serviceId"], "helloservice:2.1");
    let (s, hdr, body) = h
        .call(
            Method::POST,
            "/services/HelloService12",
            &[("content-type", "application/soap+xml; charset=utf-8; action=\"http://www.example.com/hello/sayHello\"")],
            Some(env("<hel:sayHello><name>x</name></hel:sayHello>")),
        )
        .await;
    assert_eq!(s, 200);
    assert!(
        hdr["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/soap+xml")
    );
    assert!(body.contains("sayHelloResponse"), "{body}");

    let (_, list) = h
        .admin(Method::GET, "/services?type=SOAP", VIEWER, None)
        .await;
    assert_eq!(list["services"].as_array().unwrap().len(), 2);
    let (_, list) = h
        .admin(Method::GET, "/services?q=movie", VIEWER, None)
        .await;
    let gql = &list["services"][0];
    assert_eq!(gql["type"], "GRAPHQL");
    assert_eq!(gql["operations"].as_array().unwrap().len(), 4);
    let (s, _) = h
        .admin(
            Method::GET,
            "/services/movie-graph-api:1.0/spec",
            VIEWER,
            None,
        )
        .await;
    assert_eq!(s, 404);

    let r = h.ok(Method::POST, "/import/wiremock", json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/wm"}, "response": {"status": 200, "body": "wm"}}]}, "tag": "legacy"})).await;
    assert_eq!(r["imported"], 1);
    let (_, svc) = h
        .admin(Method::GET, "/services/legacy:1.0", VIEWER, None)
        .await;
    assert_eq!(svc["type"], "GENERIC");
    assert_eq!(svc["operations"][0]["name"], "GET /wm");

    let (s, v) = h
        .admin(
            Method::POST,
            "/import/graphql",
            EDITOR,
            Some(json!({"schema": "type Query {"})),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = h
        .admin(
            Method::POST,
            "/import/wsdl",
            EDITOR,
            Some(json!({"document": "<!DOCTYPE x [<!ENTITY a \"b\">]><x>&a;</x>"})),
        )
        .await;
    assert_eq!(s, 400, "{v}");
}

#[tokio::test]
async fn auto_import_detects_artifacts_and_asyncapi_is_cataloged() {
    let h = Harness::start(StorageKind::Memory).await;
    for (doc, id) in [
        (FILMS, "movie-graph-api:1.0"),
        (SOAPUI, "helloservice-mock:0.9"),
        (WSDL, "helloservice:1.0"),
    ] {
        let resp = h
            .http
            .post(format!("{}/__admin/import", h.admin))
            .bearer_auth(EDITOR)
            .body(doc)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap();
        assert_eq!(status, 200, "{v}");
        assert!(v["imported"].as_u64().unwrap() > 0, "{v}");
        let (s, _) = h
            .admin(Method::GET, &format!("/services/{id}"), VIEWER, None)
            .await;
        assert_eq!(s, 200, "{id}");
    }
    let asyncapi = json!({"asyncapi": "2.6.0", "info": {"title": "User signed-up API", "version": "0.1.0"},
        "servers": {"k": {"url": "kafka.example.com:9092", "protocol": "kafka"}},
        "channels": {"user/signedup": {"subscribe": {"message": {"payload": {"type": "object"}, "examples": [{"payload": {"name": "x"}}]}}}}});
    let (s, v) = h
        .admin(
            Method::POST,
            "/import/asyncapi",
            EDITOR,
            Some(json!({"spec": asyncapi, "tag": "users"})),
        )
        .await;
    assert!(s == 200 || s == 201, "{v}");
    let (s, svc) = h
        .admin(
            Method::GET,
            "/services/user-signed-up-api:0.1.0",
            VIEWER,
            None,
        )
        .await;
    assert_eq!(s, 200, "{svc}");
    assert_eq!(svc["type"], "EVENT");
    assert_eq!(svc["operations"][0]["name"], "SUBSCRIBE user/signedup");
    let (s, spec) = h
        .admin(
            Method::GET,
            "/services/user-signed-up-api:0.1.0/spec",
            VIEWER,
            None,
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(spec["document"]["asyncapi"], "2.6.0");
}
