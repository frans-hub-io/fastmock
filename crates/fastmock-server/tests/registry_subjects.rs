use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use fastmock_kafka::SchemaRegistry;

type Paths = Arc<Mutex<Vec<String>>>;

async fn registry() -> (String, Paths) {
    let paths: Paths = Arc::default();
    async fn record(State(p): State<Paths>, uri: Uri) -> axum::Json<serde_json::Value> {
        let mut g = p.lock().unwrap();
        g.push(uri.path().to_string());
        axum::Json(serde_json::json!({"id": g.len()}))
    }
    let router = Router::new().fallback(record).with_state(paths.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/prefix", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    (url, paths)
}

const SCHEMA: &str = r#"{"type":"string"}"#;

#[tokio::test]
async fn registrations_go_to_the_subject_path_and_nothing_else() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (url, paths) = registry().await;
    let r = SchemaRegistry::new(&url).unwrap();
    r.register("orders-value", SCHEMA).await.unwrap();
    r.register("devices/d1/state-value", SCHEMA).await.unwrap();
    r.register("a/../b?x=1#y%2F-value", SCHEMA).await.unwrap();
    assert_eq!(
        *paths.lock().unwrap(),
        vec![
            "/prefix/subjects/orders-value/versions",
            "/prefix/subjects/devices%2Fd1%2Fstate-value/versions",
            "/prefix/subjects/a%2F..%2Fb%3Fx%3D1%23y%252F-value/versions",
        ]
    );
    for bad in [".", "..", ""] {
        let e = r.register(bad, SCHEMA).await.unwrap_err().to_string();
        assert!(e.contains("not valid"), "{bad}: {e}");
    }
    assert_eq!(
        paths.lock().unwrap().len(),
        3,
        "no request for a bad subject"
    );
}

#[tokio::test]
async fn the_registration_cache_is_bounded_and_keyed_by_the_schema() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (url, paths) = registry().await;
    let r = SchemaRegistry::new(&url).unwrap();
    let big = format!(r#"{{"type":"string","doc":"{}"}}"#, "x".repeat(10_000));
    r.register("same-value", &big).await.unwrap();
    r.register("same-value", &big).await.unwrap();
    assert_eq!(
        paths.lock().unwrap().len(),
        1,
        "the second call is answered from memory"
    );
    r.register("same-value", SCHEMA).await.unwrap();
    assert_eq!(
        paths.lock().unwrap().len(),
        2,
        "another schema is another entry"
    );
    for i in 0..1500 {
        r.register(&format!("s{i}-value"), SCHEMA).await.unwrap();
    }
    let n = r.registered_entries().await;
    assert!(n <= 1024, "{n} entries");
}
