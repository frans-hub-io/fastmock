//! gRPC streaming, reflection, compression, rich errors, gRPC-Web, limits and the journal, driven by real
//! gRPC clients (tonic) and raw HTTP/2 or HTTP/1.1 framing.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use futures::StreamExt;
use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    MethodDescriptorProto, ServiceDescriptorProto,
};
use serde_json::{Value, json};
use tonic::codec::CompressionEncoding;
use tonic::transport::Channel;
use tonic_prost::ProstCodec;

const ADMIN: &str = "fm_admin_token_for_grpc_stream_tests_01234";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

#[derive(Clone, PartialEq, Message)]
struct GetOrderRequest {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
struct Order {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    status: String,
    #[prost(int32, tag = "3")]
    items: i32,
}

#[derive(Clone, PartialEq, Message)]
struct OrderEvent {
    #[prost(string, tag = "1")]
    order_id: String,
    #[prost(string, tag = "2")]
    status: String,
    #[prost(int32, tag = "3")]
    seq: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Item {
    #[prost(string, tag = "1")]
    sku: String,
    #[prost(int32, tag = "2")]
    qty: i32,
}

#[derive(Clone, PartialEq, Message)]
struct UploadSummary {
    #[prost(int32, tag = "1")]
    count: i32,
    #[prost(string, tag = "2")]
    first_sku: String,
    #[prost(string, tag = "3")]
    last_sku: String,
}

#[derive(Clone, PartialEq, Message)]
struct ChatMessage {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "2")]
    user: String,
}

#[derive(Clone, PartialEq, Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}

#[derive(Clone, PartialEq, Message)]
struct BadRequest {
    #[prost(message, repeated, tag = "1")]
    field_violations: Vec<FieldViolation>,
}

#[derive(Clone, PartialEq, Message)]
struct FieldViolation {
    #[prost(string, tag = "1")]
    field: String,
    #[prost(string, tag = "2")]
    description: String,
}

#[derive(Clone, PartialEq, Message)]
struct RetryInfo {
    #[prost(message, optional, tag = "1")]
    retry_delay: Option<prost_types::Duration>,
}

fn field(name: &str, n: i32, ty: Type) -> FieldDescriptorProto {
    let json: String = name
        .split('_')
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                p.to_string()
            } else {
                p[..1].to_uppercase() + &p[1..]
            }
        })
        .collect();
    FieldDescriptorProto {
        name: Some(name.into()),
        number: Some(n),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        json_name: Some(json),
        ..Default::default()
    }
}

fn msg(name: &str, fields: Vec<FieldDescriptorProto>) -> DescriptorProto {
    DescriptorProto {
        name: Some(name.into()),
        field: fields,
        ..Default::default()
    }
}

fn method(name: &str, input: &str, output: &str, cs: bool, ss: bool) -> MethodDescriptorProto {
    MethodDescriptorProto {
        name: Some(name.into()),
        input_type: Some(format!(".shop.v1.{input}")),
        output_type: Some(format!(".shop.v1.{output}")),
        client_streaming: Some(cs),
        server_streaming: Some(ss),
        ..Default::default()
    }
}

fn descriptor_set() -> Vec<u8> {
    use Type::{Int32, String as S};
    let file = FileDescriptorProto {
        name: Some("shop/v1/orders.proto".into()),
        package: Some("shop.v1".into()),
        syntax: Some("proto3".into()),
        message_type: vec![
            msg("GetOrderRequest", vec![field("id", 1, S)]),
            msg(
                "Order",
                vec![
                    field("id", 1, S),
                    field("status", 2, S),
                    field("items", 3, Int32),
                ],
            ),
            msg(
                "OrderEvent",
                vec![
                    field("order_id", 1, S),
                    field("status", 2, S),
                    field("seq", 3, Int32),
                ],
            ),
            msg("Item", vec![field("sku", 1, S), field("qty", 2, Int32)]),
            msg(
                "UploadSummary",
                vec![
                    field("count", 1, Int32),
                    field("first_sku", 2, S),
                    field("last_sku", 3, S),
                ],
            ),
            msg(
                "ChatMessage",
                vec![field("text", 1, S), field("user", 2, S)],
            ),
        ],
        service: vec![ServiceDescriptorProto {
            name: Some("OrderService".into()),
            method: vec![
                method("GetOrder", "GetOrderRequest", "Order", false, false),
                method("WatchOrder", "GetOrderRequest", "OrderEvent", false, true),
                method("UploadItems", "Item", "UploadSummary", true, false),
                method("Chat", "ChatMessage", "ChatMessage", true, true),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    FileDescriptorSet { file: vec![file] }.encode_to_vec()
}

struct Srv {
    data: SocketAddr,
    admin: SocketAddr,
    http: reqwest::Client,
    _stop: tokio::sync::watch::Sender<bool>,
}

impl Srv {
    async fn start(tune: impl FnOnce(&mut Config)) -> Srv {
        let mut cfg = Config::for_tests(
            StorageKind::Memory,
            vec![format!("ops:admin:{}", hash_token(ADMIN))],
        );
        tune(&mut cfg);
        let streams = cfg.http2_max_concurrent_streams;
        let (_app, data, admin, _p) = fastmock_server::build(cfg, prom()).await.unwrap();
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (daddr, aaddr) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        let (stop, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(fastmock_server::tls::serve_with_streams(
            dl,
            data,
            None,
            rx,
            Duration::from_secs(30),
            Duration::from_secs(1),
            streams,
        ));
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        let srv = Srv {
            data: daddr,
            admin: aaddr,
            http: reqwest::Client::new(),
            _stop: stop,
        };
        let r = srv
            .http
            .put(format!("http://{aaddr}/__admin/grpc/descriptors/shop"))
            .bearer_auth(ADMIN)
            .body(descriptor_set())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        srv
    }

    async fn stub(&self, stub: Value) {
        let r = self
            .http
            .post(format!("http://{}/__admin/stubs", self.admin))
            .bearer_auth(ADMIN)
            .json(&stub)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body = r.text().await.unwrap();
        assert_eq!(status, 201, "{body}");
    }

    async fn admin_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut b = self
            .http
            .request(method, format!("http://{}/__admin{path}", self.admin))
            .bearer_auth(ADMIN);
        if let Some(j) = body {
            b = b.json(&j);
        }
        let r = b.send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn channel(&self) -> tonic::client::Grpc<Channel> {
        let ch = Channel::from_shared(format!("http://{}", self.data))
            .unwrap()
            .connect()
            .await
            .unwrap();
        tonic::client::Grpc::new(ch)
    }

    /// The journal entries of a path, newest first, once the calls are written.
    async fn journal(&self, path: &str, n: usize) -> Vec<Value> {
        for _ in 0..50 {
            let (_, j) = self
                .admin_json(
                    reqwest::Method::GET,
                    &format!("/requests?pathPrefix={path}"),
                    None,
                )
                .await;
            let list = j["requests"].as_array().cloned().unwrap_or_default();
            if list.len() >= n {
                return list;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("journal has fewer than {n} entries for {path}");
    }
}

fn path(m: &str) -> http::uri::PathAndQuery {
    http::uri::PathAndQuery::try_from(format!("/shop.v1.OrderService/{m}")).unwrap()
}

async fn watch(
    grpc: &mut tonic::client::Grpc<Channel>,
    id: &str,
) -> (
    Vec<OrderEvent>,
    Result<Option<tonic::metadata::MetadataMap>, tonic::Status>,
    tonic::metadata::MetadataMap,
) {
    grpc.ready().await.unwrap();
    let resp = grpc
        .server_streaming(
            tonic::Request::new(GetOrderRequest { id: id.into() }),
            path("WatchOrder"),
            ProstCodec::<GetOrderRequest, OrderEvent>::default(),
        )
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(s) => return (vec![], Err(s), Default::default()),
    };
    let headers = resp.metadata().clone();
    let mut stream = resp.into_inner();
    let mut events = Vec::new();
    loop {
        match stream.message().await {
            Ok(Some(e)) => events.push(e),
            Ok(None) => break,
            Err(s) => return (events, Err(s), headers),
        }
    }
    let trailers = stream.trailers().await;
    (events, trailers, headers)
}

#[tokio::test]
async fn server_streaming_messages_status_trailers_and_faults() {
    let srv = Srv::start(|_| {}).await;
    srv.stub(json!({
        "name": "watch ok",
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder"},
        "response": {
            "headers": {"x-region": "eu-1"},
            "grpc": {
                "messages": [
                    {"jsonBody": {"orderId": "{{request.json.id}}", "status": "PACKED", "seq": "{{stream.index}}"}},
                    {"jsonBody": {"orderId": "{{request.json.id}}", "status": "SHIPPED", "seq": "{{stream.index}}"}, "delayMs": 150},
                    {"jsonBody": {"orderId": "{{request.json.id}}", "status": "DELIVERED", "seq": 2}}
                ],
                "trailers": {"x-shard": "7"}
            }
        }
    }))
    .await;
    srv.stub(json!({
        "priority": 1,
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder",
                    "body": [{"jsonPath": "$.id", "value": {"equalTo": "E-1"}}]},
        "response": {"grpc": {
            "messages": [{"jsonBody": {"orderId": "E-1", "status": "PACKED"}},
                         {"jsonBody": {"orderId": "E-1", "status": "SHIPPED"}}],
            "status": {"code": "UNAVAILABLE", "message": "tracking for {{request.json.id}} is down"}
        }}
    }))
    .await;
    srv.stub(json!({
        "priority": 1,
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder",
                    "body": [{"jsonPath": "$.id", "value": {"equalTo": "TICK"}}]},
        "response": {"grpc": {
            "messages": [{"jsonBody": {"orderId": "TICK", "seq": "{{stream.index}}"}, "delayMs": 5}],
            "repeat": true, "maxMessages": 5,
            "status": {"code": 8, "message": "quota used up"}
        }}
    }))
    .await;
    srv.stub(json!({
        "priority": 1,
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder",
                    "body": [{"jsonPath": "$.id", "value": {"equalTo": "RESET"}}]},
        "response": {"fault": "CONNECTION_RESET", "grpc": {
            "messages": [{"jsonBody": {"orderId": "RESET", "status": "PACKED"}}]
        }}
    }))
    .await;
    srv.stub(json!({
        "priority": 1,
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder",
                    "body": [{"jsonPath": "$.id", "value": {"equalTo": "ONE"}}]},
        "response": {"jsonBody": {"orderId": "ONE", "status": "PAID"}}
    }))
    .await;
    srv.stub(json!({
        "priority": 1,
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder",
                    "body": [{"jsonPath": "$.id", "value": {"equalTo": "OLD"}}]},
        "response": {"headers": {"grpc-status": "5", "grpc-message": "no order OLD"}}
    }))
    .await;
    let mut grpc = srv.channel().await;

    let t = Instant::now();
    let (events, trailers, headers) = watch(&mut grpc, "A-1").await;
    assert!(t.elapsed() >= Duration::from_millis(150));
    assert_eq!(
        events.iter().map(|e| e.status.as_str()).collect::<Vec<_>>(),
        ["PACKED", "SHIPPED", "DELIVERED"]
    );
    assert!(events.iter().all(|e| e.order_id == "A-1"));
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(headers.get("x-region").unwrap(), "eu-1");
    assert_eq!(trailers.unwrap().unwrap().get("x-shard").unwrap(), "7");

    let (events, status, _) = watch(&mut grpc, "E-1").await;
    assert_eq!(events.len(), 2, "messages arrive before the error");
    let s = status.unwrap_err();
    assert_eq!(s.code(), tonic::Code::Unavailable);
    assert_eq!(s.message(), "tracking for E-1 is down");

    let (events, status, _) = watch(&mut grpc, "TICK").await;
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4]
    );
    assert_eq!(status.unwrap_err().code(), tonic::Code::ResourceExhausted);

    let (events, status, _) = watch(&mut grpc, "RESET").await;
    assert_eq!(
        events.len(),
        1,
        "the fault happens after the scripted messages"
    );
    assert_ne!(status.unwrap_err().code(), tonic::Code::Ok);

    let (events, status, _) = watch(&mut grpc, "ONE").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, "PAID");
    assert!(status.is_ok());

    let (events, status, _) = watch(&mut grpc, "OLD").await;
    assert!(events.is_empty());
    let s = status.unwrap_err();
    assert_eq!(
        (s.code(), s.message()),
        (tonic::Code::NotFound, "no order OLD")
    );

    let list = srv.journal("/shop.v1.OrderService/WatchOrder", 6).await;
    let ok = list
        .iter()
        .find(|e| e["body"] == r#"{"id":"A-1"}"#)
        .expect("journal entry of A-1");
    assert_eq!(ok["grpc"]["kind"], "serverStreaming");
    assert_eq!(ok["grpc"]["status"], 0);
    assert_eq!(ok["grpc"]["responseMessages"], 3);
    let sent: Value = serde_json::from_str(ok["responseBody"].as_str().unwrap()).unwrap();
    assert_eq!(sent[1]["status"], "SHIPPED");
    assert!(ok["durationMs"].as_u64().unwrap() >= 150);
    let err = list
        .iter()
        .find(|e| e["body"] == r#"{"id":"E-1"}"#)
        .unwrap();
    assert_eq!(err["grpc"]["status"], 14);
    assert_eq!(err["grpc"]["message"], "tracking for E-1 is down");
    let metrics = prom().render();
    assert!(
        metrics.contains(
            "fastmock_grpc_stream_messages_total{direction=\"out\",kind=\"server_streaming\"}"
        ),
        "{metrics}"
    );
}

#[tokio::test]
async fn client_streaming_matching_and_rich_errors() {
    let srv = Srv::start(|_| {}).await;
    srv.stub(json!({
        "priority": 1,
        "request": {
            "urlPath": "/shop.v1.OrderService/UploadItems",
            "body": [{"jsonPath": "$[0].sku", "value": {"equalTo": "FIRST"}}],
            "grpc": {
                "messageCount": {"min": 3},
                "everyMessage": [{"jsonPath": "$.qty", "value": {"matches": "[1-9][0-9]*"}}],
                "lastMessage": [{"jsonPath": "$.sku", "value": {"equalTo": "LAST"}}]
            }
        },
        "response": {"jsonBody": {"count": "{{size request.json}}", "firstSku": "{{request.json.[0].sku}}",
                                  "lastSku": "{{jsonPath request.body '$[-1].sku'}}"}}
    }))
    .await;
    srv.stub(json!({
        "priority": 2,
        "request": {"urlPath": "/shop.v1.OrderService/UploadItems",
                    "grpc": {"anyMessage": [{"jsonPath": "$.sku", "value": {"equalTo": "BROKEN"}}]}},
        "response": {"grpc": {"status": {
            "code": "INVALID_ARGUMENT",
            "message": "{{size request.json}} items rejected",
            "details": [
                {"@type": "type.googleapis.com/google.rpc.BadRequest",
                 "fieldViolations": [{"field": "items[0].sku", "description": "unknown sku"}]},
                {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "2s"}
            ]
        }, "trailers": {"x-request-id": "r-1"}}}
    }))
    .await;
    srv.stub(json!({
        "priority": 3,
        "request": {"urlPath": "/shop.v1.OrderService/UploadItems", "grpc": {"messageCount": {"equalTo": 0}}},
        "response": {"grpc": {"messages": [{"jsonBody": {"count": 0}}]}}
    }))
    .await;
    let grpc = srv.channel().await;
    let upload = |items: Vec<Item>| {
        let mut g = grpc.clone();
        async move {
            g.ready().await.unwrap();
            g.client_streaming(
                tonic::Request::new(futures::stream::iter(items)),
                path("UploadItems"),
                ProstCodec::<Item, UploadSummary>::default(),
            )
            .await
        }
    };
    let item = |sku: &str, qty: i32| Item {
        sku: sku.into(),
        qty,
    };
    let s = upload(vec![item("FIRST", 1), item("MID", 2), item("LAST", 3)])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        s,
        UploadSummary {
            count: 3,
            first_sku: "FIRST".into(),
            last_sku: "LAST".into()
        }
    );
    let s = upload(vec![]).await.unwrap().into_inner();
    assert_eq!(s.count, 0);
    let err = upload(vec![item("FIRST", 1), item("MID", 0), item("LAST", 3)])
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::Unimplemented,
        "qty 0 fails everyMessage"
    );
    let err = upload(vec![item("A", 1), item("BROKEN", 1)])
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(err.message(), "2 items rejected");
    assert_eq!(err.metadata().get("x-request-id").unwrap(), "r-1");
    let st = RpcStatus::decode(err.details()).unwrap();
    assert_eq!(st.code, 3);
    assert_eq!(st.details.len(), 2);
    assert_eq!(
        st.details[0].type_url,
        "type.googleapis.com/google.rpc.BadRequest"
    );
    let br = BadRequest::decode(&st.details[0].value[..]).unwrap();
    assert_eq!(br.field_violations[0].field, "items[0].sku");
    let ri = RetryInfo::decode(&st.details[1].value[..]).unwrap();
    assert_eq!(ri.retry_delay.unwrap().seconds, 2);

    let list = srv.journal("/shop.v1.OrderService/UploadItems", 4).await;
    let ok = list
        .iter()
        .find(|e| e["grpc"]["status"] == 0 && e["grpc"]["requestMessages"] == 3)
        .unwrap();
    assert_eq!(ok["grpc"]["kind"], "clientStreaming");
    let body: Value = serde_json::from_str(ok["body"].as_str().unwrap()).unwrap();
    assert_eq!(body[2], json!({"sku": "LAST", "qty": 3}));
    let resp: Value = serde_json::from_str(ok["responseBody"].as_str().unwrap()).unwrap();
    assert_eq!(resp["firstSku"], "FIRST");

    let verify = |pattern: Value, count: u64| {
        let srv = &srv;
        async move {
            srv.admin_json(
                reqwest::Method::POST,
                "/verify",
                Some(json!({"request": pattern, "count": count})),
            )
            .await
        }
    };
    let (s, v) = verify(
        json!({"urlPath": "/shop.v1.OrderService/UploadItems",
               "grpc": {"messageCount": {"equalTo": 3}, "lastMessage": [{"jsonPath": "$.sku", "value": {"equalTo": "LAST"}}]}}),
        2,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = verify(
        json!({"urlPath": "/shop.v1.OrderService/UploadItems",
               "body": [{"jsonPath": "$[1].sku", "value": {"equalTo": "BROKEN"}}]}),
        1,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = verify(
        json!({"urlPath": "/shop.v1.OrderService/UploadItems", "grpc": {"messageCount": {"equalTo": 7}}}),
        1,
    )
    .await;
    assert_eq!(s, 422);
    assert!(
        v["nearMisses"]
            .to_string()
            .contains("gRPC message count does not match"),
        "{v}"
    );
}

#[tokio::test]
async fn bidirectional_streaming_replies_in_order() {
    let srv = Srv::start(|_| {}).await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/Chat"},
        "response": {
            "jsonBody": {"text": "echo {{message.json.text}}", "user": "bot"},
            "grpc": {
                "messages": [{"jsonBody": {"text": "welcome {{request.headers.x-user}}", "user": "bot"}}],
                "onMessage": [
                    {"match": [{"jsonPath": "$.text", "value": {"equalTo": "bye"}}],
                     "messages": [{"jsonBody": {"text": "bye {{message.json.user}}"}}],
                     "status": {"code": 0}},
                    {"match": [{"jsonPath": "$.text", "value": {"contains": "?"}}],
                     "messages": [{"jsonBody": {"text": "answer #{{message.index}}"}, "delayMs": 20},
                                  {"jsonBody": {"text": "anything else?"}}]},
                    {"match": [{"jsonPath": "$.text", "value": {"equalTo": "boom"}}],
                     "status": {"code": "ABORTED", "message": "boom at #{{message.index}}"}}
                ]
            }
        }
    }))
    .await;
    let grpc = srv.channel().await;
    let chat = |texts: Vec<&'static str>| {
        let mut g = grpc.clone();
        async move {
            g.ready().await.unwrap();
            let msgs = texts.into_iter().map(|t| ChatMessage {
                text: t.into(),
                user: "ann".into(),
            });
            let mut req = tonic::Request::new(futures::stream::iter(msgs));
            req.metadata_mut().insert("x-user", "ann".parse().unwrap());
            let resp = g
                .streaming(
                    req,
                    path("Chat"),
                    ProstCodec::<ChatMessage, ChatMessage>::default(),
                )
                .await
                .unwrap();
            let mut s = resp.into_inner();
            let mut out = Vec::new();
            loop {
                match s.message().await {
                    Ok(Some(m)) => out.push(m.text),
                    Ok(None) => return (out, None),
                    Err(e) => return (out, Some(e)),
                }
            }
        }
    };
    let (texts, err) = chat(vec!["hi", "what?", "bye", "ignored"]).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(
        texts,
        [
            "welcome ann",
            "echo hi",
            "answer #1",
            "anything else?",
            "bye ann"
        ]
    );
    let (texts, err) = chat(vec!["hello"]).await;
    assert_eq!(texts, ["welcome ann", "echo hello"]);
    assert!(err.is_none(), "the call ends when the client half-closes");
    let (_, err) = chat(vec!["a", "boom"]).await;
    let err = err.unwrap();
    assert_eq!(
        (err.code(), err.message()),
        (tonic::Code::Aborted, "boom at #1")
    );

    let list = srv.journal("/shop.v1.OrderService/Chat", 3).await;
    let first = list
        .iter()
        .find(|e| e["grpc"]["requestMessages"] == 3)
        .unwrap();
    assert_eq!(first["grpc"]["kind"], "bidiStreaming");
    assert_eq!(first["grpc"]["responseMessages"], 5);
    let body: Value = serde_json::from_str(first["body"].as_str().unwrap()).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn server_reflection_v1_and_v1alpha() {
    use tonic_reflection::pb::v1 as r1;
    use tonic_reflection::pb::v1alpha as r1a;
    let srv = Srv::start(|_| {}).await;
    let ch = Channel::from_shared(format!("http://{}", srv.data))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = r1::server_reflection_client::ServerReflectionClient::new(ch.clone());
    let reqs = vec![
        r1::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(r1::server_reflection_request::MessageRequest::ListServices(
                String::new(),
            )),
        },
        r1::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(
                r1::server_reflection_request::MessageRequest::FileContainingSymbol(
                    "shop.v1.OrderService.WatchOrder".into(),
                ),
            ),
        },
        r1::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(
                r1::server_reflection_request::MessageRequest::FileByFilename(
                    "missing.proto".into(),
                ),
            ),
        },
    ];
    let mut s = client
        .server_reflection_info(futures::stream::iter(reqs))
        .await
        .unwrap()
        .into_inner();
    let mut answers = Vec::new();
    while let Some(a) = s.message().await.unwrap() {
        answers.push(a.message_response.unwrap());
    }
    assert_eq!(answers.len(), 3);
    match &answers[0] {
        r1::server_reflection_response::MessageResponse::ListServicesResponse(l) => {
            assert_eq!(
                l.service
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
                ["shop.v1.OrderService"]
            )
        }
        other => panic!("{other:?}"),
    }
    match &answers[1] {
        r1::server_reflection_response::MessageResponse::FileDescriptorResponse(f) => {
            let file = FileDescriptorProto::decode(&f.file_descriptor_proto[0][..]).unwrap();
            assert_eq!(file.name(), "shop/v1/orders.proto");
            assert_eq!(file.service[0].method.len(), 4);
            assert!(file.service[0].method[1].server_streaming());
        }
        other => panic!("{other:?}"),
    }
    match &answers[2] {
        r1::server_reflection_response::MessageResponse::ErrorResponse(e) => {
            assert_eq!(e.error_code, 5)
        }
        other => panic!("{other:?}"),
    }
    let mut alpha = r1a::server_reflection_client::ServerReflectionClient::new(ch);
    let mut s = alpha
        .server_reflection_info(futures::stream::iter(vec![r1a::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(
                r1a::server_reflection_request::MessageRequest::ListServices(String::new()),
            ),
        }]))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        s.message().await.unwrap().unwrap().message_response,
        Some(r1a::server_reflection_response::MessageResponse::ListServicesResponse(_))
    ));

    let off = Srv::start(|c| c.grpc_reflection = false).await;
    let ch = Channel::from_shared(format!("http://{}", off.data))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = r1::server_reflection_client::ServerReflectionClient::new(ch);
    let err = match client
        .server_reflection_info(futures::stream::iter(vec![
            r1::ServerReflectionRequest::default(),
        ]))
        .await
    {
        Err(e) => e,
        Ok(r) => r.into_inner().message().await.unwrap_err(),
    };
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

#[tokio::test]
async fn gzip_requests_and_responses() {
    let srv = Srv::start(|_| {}).await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/GetOrder"},
        "response": {"jsonBody": {"id": "{{request.json.id}}", "status": "PAID", "items": 3}}
    }))
    .await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder"},
        "response": {"grpc": {"compression": "gzip", "messages": [
            {"jsonBody": {"orderId": "{{request.json.id}}", "status": "x"}},
            {"jsonBody": {"orderId": "{{request.json.id}}", "status": "y"}}
        ]}}
    }))
    .await;
    let mut grpc = srv
        .channel()
        .await
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip);
    grpc.ready().await.unwrap();
    let long = "A".repeat(5000);
    let resp = grpc
        .unary(
            tonic::Request::new(GetOrderRequest { id: long.clone() }),
            path("GetOrder"),
            ProstCodec::<GetOrderRequest, Order>::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp.metadata().get("grpc-encoding").unwrap(), "gzip");
    assert_eq!(resp.into_inner().id, long);
    let (events, status, headers) = watch(&mut grpc, "Z").await;
    assert_eq!(events.len(), 2);
    assert!(status.is_ok());
    assert_eq!(headers.get("grpc-encoding").unwrap(), "gzip");
    let mut plain = srv.channel().await;
    let (events, _, headers) = watch(&mut plain, "Z").await;
    assert_eq!(events.len(), 2);
    assert!(
        headers.get("grpc-encoding").is_none(),
        "no gzip for a client that does not accept it"
    );

    let client: hyper_util::client::legacy::Client<_, http_body_util::Full<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .http2_only(true)
            .build_http();
    let req = hyper::Request::post(format!("http://{}/shop.v1.OrderService/GetOrder", srv.data))
        .header("content-type", "application/grpc")
        .header("grpc-encoding", "snappy")
        .body(http_body_util::Full::new(frame(
            0,
            &GetOrderRequest { id: "x".into() }.encode_to_vec(),
        )))
        .unwrap();
    let (_, trailers) = collect(client.request(req).await.unwrap()).await;
    assert_eq!(trailers["grpc-status"], "12");
}

fn frame(flag: u8, payload: &[u8]) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(flag);
    b.put_u32(payload.len() as u32);
    b.put_slice(payload);
    b.freeze()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    fastmock_server::grpc::gzip(data)
}

async fn collect(resp: hyper::Response<hyper::body::Incoming>) -> (Bytes, http::HeaderMap) {
    use http_body_util::BodyExt;
    let c = resp.into_body().collect().await.unwrap();
    let t = c.trailers().cloned().unwrap_or_default();
    (c.to_bytes(), t)
}

#[tokio::test]
async fn limits_on_streams_and_messages() {
    let srv = Srv::start(|c| {
        c.grpc_max_stream_messages = 20;
        c.grpc_stream_idle_timeout = Duration::from_millis(300);
        c.http2_max_concurrent_streams = 3;
    })
    .await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/UploadItems"},
        "response": {"jsonBody": {"count": "{{size request.json}}"}}
    }))
    .await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/GetOrder"},
        "response": {"jsonBody": {"id": "x"}}
    }))
    .await;
    let grpc = srv.channel().await;
    let upload = |items: futures::stream::BoxStream<'static, Item>| {
        let mut g = grpc.clone();
        async move {
            g.ready().await.unwrap();
            g.client_streaming(
                tonic::Request::new(items),
                path("UploadItems"),
                ProstCodec::<Item, UploadSummary>::default(),
            )
            .await
        }
    };
    let item = Item {
        sku: "S".into(),
        qty: 1,
    };
    let ok = upload(futures::stream::iter(vec![item.clone(); 20]).boxed())
        .await
        .unwrap();
    assert_eq!(ok.into_inner().count, 20);

    let t = Instant::now();
    let forever = futures::stream::repeat(item.clone()).boxed();
    let err = tokio::time::timeout(Duration::from_secs(10), upload(forever))
        .await
        .expect("a client that streams forever is stopped")
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");
    assert!(
        err.message().contains("more than 20 request messages"),
        "{}",
        err.message()
    );
    assert!(t.elapsed() < Duration::from_secs(5));

    let stalled = futures::stream::iter(vec![item.clone()])
        .chain(futures::stream::pending())
        .boxed();
    let t = Instant::now();
    let err = upload(stalled).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DeadlineExceeded, "{err:?}");
    assert!(t.elapsed() >= Duration::from_millis(300));

    let client: hyper_util::client::legacy::Client<_, http_body_util::Full<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .http2_only(true)
            .build_http();
    let send = |body: Bytes, gz: bool| {
        let mut b =
            hyper::Request::post(format!("http://{}/shop.v1.OrderService/GetOrder", srv.data))
                .header("content-type", "application/grpc")
                .header("te", "trailers");
        if gz {
            b = b.header("grpc-encoding", "gzip");
        }
        let req = b.body(http_body_util::Full::new(body)).unwrap();
        let c = client.clone();
        async move { collect(c.request(req).await.unwrap()).await }
    };
    let bomb = gzip(&vec![0u8; 8 * 1024 * 1024]);
    assert!(bomb.len() < 64 * 1024, "{}", bomb.len());
    let t = Instant::now();
    let (_, trailers) = send(frame(1, &bomb), true).await;
    assert_eq!(trailers["grpc-status"], "8");
    assert!(
        trailers["grpc-message"]
            .to_str()
            .unwrap()
            .contains("decompressed"),
        "{trailers:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(5));
    let (_, trailers) = send(frame(1, b"not gzip at all"), true).await;
    assert_eq!(trailers["grpc-status"], "13");
    let (_, trailers) = send(frame(1, &gzip(b"\x0a\x01x")), false).await;
    assert_eq!(
        trailers["grpc-status"], "13",
        "compressed flag needs grpc-encoding"
    );
    let mut huge = BytesMut::new();
    huge.put_u8(0);
    huge.put_u32(2 * 1024 * 1024);
    huge.put_slice(&[0u8; 64]);
    let (_, trailers) = send(huge.freeze(), false).await;
    assert_eq!(trailers["grpc-status"], "8");
    let (body, trailers) = send(
        frame(
            1,
            &gzip(&GetOrderRequest { id: "z".into() }.encode_to_vec()),
        ),
        true,
    )
    .await;
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(Order::decode(&body[5..]).unwrap().id, "x");

    let tcp = tokio::net::TcpStream::connect(srv.data).await.unwrap();
    let (h2c, conn) = h2::client::handshake(tcp).await.unwrap();
    tokio::spawn(conn);
    let mut h2c = h2c.ready().await.unwrap();
    let req = http::Request::post("/shop.v1.OrderService/GetOrder")
        .header("content-type", "application/grpc")
        .body(())
        .unwrap();
    let (resp, mut stream) = h2c.send_request(req, false).unwrap();
    stream
        .send_data(
            frame(0, &GetOrderRequest { id: "a".into() }.encode_to_vec()),
            true,
        )
        .unwrap();
    resp.await.unwrap();
    assert_eq!(h2c.current_max_send_streams(), 3);

    let short = Srv::start(|c| c.grpc_max_stream_duration = Duration::from_millis(700)).await;
    short
        .stub(json!({
            "request": {"urlPath": "/shop.v1.OrderService/UploadItems"},
            "response": {"jsonBody": {"count": 1}}
        }))
        .await;
    let mut g = short.channel().await;
    g.ready().await.unwrap();
    let slow = futures::stream::unfold((), |_| async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Some((
            Item {
                sku: "S".into(),
                qty: 1,
            },
            (),
        ))
    });
    let t = Instant::now();
    let err = g
        .client_streaming(
            tonic::Request::new(slow),
            path("UploadItems"),
            ProstCodec::<Item, UploadSummary>::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
    assert!(
        err.message().contains("maximum stream duration"),
        "{}",
        err.message()
    );
    assert!(t.elapsed() >= Duration::from_millis(700) && t.elapsed() < Duration::from_secs(5));
    let metrics = prom().render();
    assert!(metrics.contains("fastmock_grpc_stream_limits_total{limit=\"messages\"}"));
    assert!(metrics.contains("fastmock_grpc_stream_limits_total{limit=\"idle\"}"));
}

/// Splits a gRPC-Web body into message payloads and the trailer block.
fn web_frames(mut body: &[u8]) -> (Vec<Vec<u8>>, String) {
    let mut msgs = Vec::new();
    let mut trailers = String::new();
    while body.len() >= 5 {
        let flag = body[0];
        let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        let payload = &body[5..5 + len];
        if flag & 0x80 != 0 {
            trailers = String::from_utf8(payload.to_vec()).unwrap();
        } else {
            msgs.push(payload.to_vec());
        }
        body = &body[5 + len..];
    }
    (msgs, trailers)
}

#[tokio::test]
async fn grpc_web_unary_streaming_text_and_cors() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    let srv = Srv::start(|_| {}).await;
    srv.stub(json!({
        "priority": 1,
        "request": {"method": "OPTIONS", "urlPathPattern": "/shop\\.v1\\..*"},
        "response": {"status": 204, "headers": {
            "access-control-allow-origin": "https://shop.example.com",
            "access-control-allow-methods": "POST",
            "access-control-allow-headers": "content-type,x-grpc-web,x-user-agent",
            "access-control-max-age": "600"
        }}
    }))
    .await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/GetOrder"},
        "response": {
            "headers": {"access-control-allow-origin": "https://shop.example.com",
                        "access-control-expose-headers": "grpc-status,grpc-message"},
            "jsonBody": {"id": "{{request.json.id}}", "status": "PAID", "items": 2}
        }
    }))
    .await;
    srv.stub(json!({
        "priority": 1,
        "request": {"urlPath": "/shop.v1.OrderService/GetOrder", "body": [{"jsonPath": "$.id", "value": {"equalTo": "NONE"}}]},
        "response": {"grpc": {"status": {"code": "NOT_FOUND", "message": "order NONE not found"}}}
    }))
    .await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder"},
        "response": {"grpc": {"messages": [
            {"jsonBody": {"orderId": "{{request.json.id}}", "status": "PACKED"}},
            {"jsonBody": {"orderId": "{{request.json.id}}", "status": "SHIPPED"}, "delayMs": 20}
        ]}}
    }))
    .await;
    let h1 = reqwest::Client::builder().http1_only().build().unwrap();
    let pre = h1
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{}/shop.v1.OrderService/GetOrder", srv.data),
        )
        .header("origin", "https://shop.example.com")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(pre.status(), 204);
    assert_eq!(
        pre.headers()["access-control-allow-origin"],
        "https://shop.example.com"
    );
    let call = |client: &reqwest::Client, m: &str, ct: &str, body: Vec<u8>| {
        client
            .post(format!("http://{}/shop.v1.OrderService/{m}", srv.data))
            .header("content-type", ct)
            .header("x-grpc-web", "1")
            .body(body)
            .send()
    };
    let req = frame(0, &GetOrderRequest { id: "W-1".into() }.encode_to_vec()).to_vec();
    let r = call(&h1, "GetOrder", "application/grpc-web+proto", req.clone())
        .await
        .unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_11);
    assert_eq!(r.headers()["content-type"], "application/grpc-web+proto");
    assert_eq!(
        r.headers()["access-control-allow-origin"],
        "https://shop.example.com"
    );
    let (msgs, trailers) = web_frames(&r.bytes().await.unwrap());
    assert_eq!(Order::decode(&msgs[0][..]).unwrap().id, "W-1");
    assert!(trailers.contains("grpc-status: 0\r\n"), "{trailers}");

    let none = frame(0, &GetOrderRequest { id: "NONE".into() }.encode_to_vec()).to_vec();
    let r = call(&h1, "GetOrder", "application/grpc-web", none)
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "application/grpc-web");
    let (msgs, trailers) = web_frames(&r.bytes().await.unwrap());
    assert!(msgs.is_empty());
    assert!(trailers.contains("grpc-status: 5\r\n"), "{trailers}");
    assert!(
        trailers.contains("grpc-message: order NONE not found\r\n"),
        "{trailers}"
    );

    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let w = frame(0, &GetOrderRequest { id: "S-1".into() }.encode_to_vec()).to_vec();
    let r = call(&h2, "WatchOrder", "application/grpc-web+proto", w.clone())
        .await
        .unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_2);
    let (msgs, trailers) = web_frames(&r.bytes().await.unwrap());
    let statuses: Vec<String> = msgs
        .iter()
        .map(|m| OrderEvent::decode(&m[..]).unwrap().status)
        .collect();
    assert_eq!(statuses, ["PACKED", "SHIPPED"]);
    assert!(trailers.contains("grpc-status: 0"));

    let r = call(
        &h1,
        "WatchOrder",
        "application/grpc-web-text",
        B64.encode(&w).into_bytes(),
    )
    .await
    .unwrap();
    assert_eq!(r.headers()["content-type"], "application/grpc-web-text");
    let text = r.text().await.unwrap();
    let mut raw = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let end = rest.find('=').map(|i| {
            let mut j = i;
            while rest[j..].starts_with('=') {
                j += 1;
            }
            j
        });
        let (chunk, tail) = rest.split_at(end.unwrap_or(rest.len()));
        raw.extend(B64.decode(chunk).unwrap());
        rest = tail;
    }
    let (msgs, trailers) = web_frames(&raw);
    assert_eq!(msgs.len(), 2);
    assert!(trailers.contains("grpc-status: 0"));

    let list = srv.journal("/shop.v1.OrderService/WatchOrder", 2).await;
    assert!(
        list.iter()
            .any(|e| e["grpc"]["protocol"] == "grpc-web-text")
    );
    assert!(list.iter().any(|e| e["grpc"]["protocol"] == "grpc-web"));
}

#[tokio::test]
async fn journal_keeps_bounded_streams_with_a_truncation_marker() {
    let srv = Srv::start(|c| c.journal_max_body = 300).await;
    srv.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/UploadItems"},
        "response": {"jsonBody": {"count": "{{size request.json}}"}}
    }))
    .await;
    let mut g = srv.channel().await;
    g.ready().await.unwrap();
    let items: Vec<Item> = (0..40)
        .map(|i| Item {
            sku: format!("SKU-{i:03}"),
            qty: i + 1,
        })
        .collect();
    let r = g
        .client_streaming(
            tonic::Request::new(futures::stream::iter(items)),
            path("UploadItems"),
            ProstCodec::<Item, UploadSummary>::default(),
        )
        .await
        .unwrap();
    assert_eq!(r.into_inner().count, 40, "matching sees every message");
    let list = srv.journal("/shop.v1.OrderService/UploadItems", 1).await;
    let e = &list[0];
    assert_eq!(e["grpc"]["requestMessages"], 40);
    assert_eq!(e["grpc"]["requestTruncated"], true);
    let body: Value = serde_json::from_str(e["body"].as_str().unwrap()).unwrap();
    let kept = body.as_array().unwrap().len();
    assert!(kept > 0 && kept < 40, "{kept}");
    let (s, v) = srv
        .admin_json(
            reqwest::Method::POST,
            "/verify",
            Some(
                json!({"request": {"urlPath": "/shop.v1.OrderService/UploadItems",
                                    "grpc": {"messageCount": {"equalTo": 40}}}, "count": 1}),
            ),
        )
        .await;
    assert_eq!(
        s, 200,
        "the count comes from the entry, not the kept messages: {v}"
    );
}

#[tokio::test]
async fn microcks_examples_for_streaming_methods() {
    let srv = Srv::start(|_| {}).await;
    let (s, r) = srv
        .admin_json(
            reqwest::Method::POST,
            "/import/microcks",
            Some(json!({"document": {
                "apiVersion": "mocks.microcks.io/v1alpha1",
                "kind": "APIExamples",
                "metadata": {"name": "shop", "version": "1.0"},
                "operations": {
                    "shop.v1.OrderService/WatchOrder": {
                        "shipped": {"request": {"body": {"id": "M-1"}},
                                    "response": {"body": {"orderId": "M-1", "status": "SHIPPED"}}}
                    },
                    "shop.v1.OrderService/UploadItems": {
                        "one": {"request": {"body": {"sku": "A"}}, "response": {"body": {"count": 1}}}
                    }
                }
            }})),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["imported"], 1, "{r}");
    assert!(
        r["warnings"].to_string().contains("client-streaming"),
        "{r}"
    );
    let mut grpc = srv.channel().await;
    let (events, status, _) = watch(&mut grpc, "M-1").await;
    assert!(status.is_ok());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, "SHIPPED");
}
