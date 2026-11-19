use std::sync::OnceLock;

use bytes::{BufMut, Bytes, BytesMut};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, Value as PValue};
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    MethodDescriptorProto, ServiceDescriptorProto,
};
use serde_json::{Value, json};

const ADMIN: &str = "fm_admin_token_for_grpc_tests_0123456789";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn field(name: &str, n: i32) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.into()),
        number: Some(n),
        label: Some(Label::Optional as i32),
        r#type: Some(Type::String as i32),
        json_name: Some(name.into()),
        ..Default::default()
    }
}

fn descriptor_set() -> Vec<u8> {
    let file = FileDescriptorProto {
        name: Some("demo.proto".into()),
        package: Some("demo".into()),
        syntax: Some("proto3".into()),
        message_type: vec![
            DescriptorProto {
                name: Some("HelloRequest".into()),
                field: vec![field("name", 1)],
                ..Default::default()
            },
            DescriptorProto {
                name: Some("HelloReply".into()),
                field: vec![field("message", 1)],
                ..Default::default()
            },
        ],
        service: vec![ServiceDescriptorProto {
            name: Some("Greeter".into()),
            method: vec![MethodDescriptorProto {
                name: Some("SayHello".into()),
                input_type: Some(".demo.HelloRequest".into()),
                output_type: Some(".demo.HelloReply".into()),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    FileDescriptorSet { file: vec![file] }.encode_to_vec()
}

fn frame(payload: &[u8]) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(0);
    b.put_u32(payload.len() as u32);
    b.put_slice(payload);
    b.freeze()
}

#[tokio::test]
async fn unary_grpc_with_dynamic_descriptors() {
    let cfg = Config::for_tests(
        StorageKind::Memory,
        vec![format!("ops:admin:{}", hash_token(ADMIN))],
    );
    let (_app, data, admin, _p) = fastmock_server::build(cfg, prom()).await.unwrap();
    let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (daddr, aaddr) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
    tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
    let http = reqwest::Client::new();
    let fds = descriptor_set();
    let r: Value = http
        .put(format!("http://{aaddr}/__admin/grpc/descriptors/demo"))
        .bearer_auth(ADMIN)
        .body(fds.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["services"], json!(["demo.Greeter"]));
    assert_eq!(
        http.put(format!("http://{aaddr}/__admin/grpc/descriptors/bad"))
            .bearer_auth(ADMIN)
            .body("garbage")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    for stub in [
        json!({"priority": 1, "request": {"urlPath": "/demo.Greeter/SayHello", "body": [{"jsonPath": "$.name", "value": {"equalTo": "missing"}}]}, "response": {"headers": {"grpc-status": "5", "grpc-message": "користувача не знайдено"}}}),
        json!({"request": {"urlPath": "/demo.Greeter/SayHello"}, "response": {"jsonBody": {"message": "hi {{request.json.name}}"}}}),
    ] {
        assert_eq!(
            http.post(format!("http://{aaddr}/__admin/stubs"))
                .bearer_auth(ADMIN)
                .json(&stub)
                .send()
                .await
                .unwrap()
                .status(),
            201
        );
    }
    let pool = DescriptorPool::decode(&fds[..]).unwrap();
    let req_desc = pool.get_message_by_name("demo.HelloRequest").unwrap();
    let reply_desc = pool.get_message_by_name("demo.HelloReply").unwrap();
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http();
    let call = |name: &str, path: &str| {
        let mut m = DynamicMessage::new(req_desc.clone());
        m.set_field_by_name("name", PValue::String(name.into()));
        let req = hyper::Request::post(format!("http://{daddr}{path}"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(Full::new(frame(&m.encode_to_vec())))
            .unwrap();
        let c = client.clone();
        async move {
            let resp = c.request(req).await.unwrap();
            assert_eq!(resp.headers()["content-type"], "application/grpc");
            let collected = resp.into_body().collect().await.unwrap();
            let trailers = collected.trailers().cloned().unwrap();
            (collected.to_bytes(), trailers)
        }
    };
    let (body, trailers) = call("Ann", "/demo.Greeter/SayHello").await;
    assert_eq!(trailers["grpc-status"], "0");
    let reply = DynamicMessage::decode(reply_desc.clone(), &body[5..]).unwrap();
    assert_eq!(
        reply.get_field_by_name("message").unwrap().as_str(),
        Some("hi Ann")
    );
    let (body, trailers) = call("missing", "/demo.Greeter/SayHello").await;
    assert!(body.is_empty());
    assert_eq!(trailers["grpc-status"], "5");
    assert!(
        trailers["grpc-message"]
            .to_str()
            .unwrap()
            .starts_with("%D0%BA"),
        "non-ASCII grpc-message is percent-encoded"
    );
    let (_, trailers) = call("x", "/demo.Greeter/Unknown").await;
    assert_eq!(trailers["grpc-status"], "12");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let j: Value = http
        .get(format!(
            "http://{aaddr}/__admin/requests?pathPrefix=/demo.Greeter/SayHello"
        ))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bodies: Vec<&str> = j["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["body"].as_str().unwrap())
        .collect();
    assert!(
        bodies.contains(&"{\"name\":\"Ann\"}"),
        "journal stores the decoded protobuf message: {bodies:?}"
    );
}
