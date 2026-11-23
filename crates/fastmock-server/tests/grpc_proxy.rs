//! gRPC proxying and recording: proxy stubs forward every kind of call to an upstream gRPC server (here a
//! second FastMock) over h2c, TLS and mTLS through the egress policy; recording turns unmatched calls into
//! stubs that replay them, within the recording limits.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind, TlsSettings};
use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    MethodDescriptorProto, ServiceDescriptorProto,
};
use serde_json::{Value, json};
use tonic::transport::Channel;
use tonic_prost::ProstCodec;

const ADMIN: &str = "fm_admin_token_for_grpc_proxy_tests_012345";

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
        cfg.plugins = Some(vec![]);
        tune(&mut cfg);
        let tls = cfg
            .tls
            .as_ref()
            .map(|t| fastmock_server::tls::server_config(t).unwrap());
        let streams = cfg.http2_max_concurrent_streams;
        let (_app, data, admin, _p) = fastmock_server::build(cfg, prom()).await.unwrap();
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (daddr, aaddr) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        let (stop, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(fastmock_server::tls::serve_with_streams(
            dl,
            data,
            tls,
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

    async fn admin(
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

    async fn stub(&self, stub: Value) -> Value {
        let (st, v) = self
            .admin(reqwest::Method::POST, "/stubs", Some(stub))
            .await;
        assert_eq!(st, 201, "{v}");
        v
    }

    async fn channel(&self) -> tonic::client::Grpc<Channel> {
        let ch = Channel::from_shared(format!("http://{}", self.data))
            .unwrap()
            .connect()
            .await
            .unwrap();
        tonic::client::Grpc::new(ch)
    }

    async fn journal(&self, path: &str, n: usize) -> Vec<Value> {
        for _ in 0..100 {
            let (_, j) = self
                .admin(
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

/// The upstream: stubs for every kind of call.
async fn upstream(tune: impl FnOnce(&mut Config)) -> Srv {
    let up = Srv::start(tune).await;
    up.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/GetOrder",
                    "body": [{"jsonPath": "$.id", "value": {"matches": "A-.*"}}]},
        "response": {"headers": {"x-region": "eu-1", "x-upstream-id": "{{request.headers.x-call}}"},
                     "jsonBody": {"id": "{{request.json.id}}", "status": "PAID", "items": 3}}
    }))
    .await;
    up.stub(json!({
        "priority": 6,
        "request": {"urlPath": "/shop.v1.OrderService/GetOrder"},
        "response": {"grpc": {"status": {
            "code": "NOT_FOUND", "message": "order {{request.json.id}} not found",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "NO_ORDER", "domain": "shop.example.com"}]
        }}}
    }))
    .await;
    up.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/WatchOrder"},
        "response": {"grpc": {
            "messages": [
                {"jsonBody": {"orderId": "{{request.json.id}}", "status": "PACKED", "seq": "{{stream.index}}"}},
                {"jsonBody": {"orderId": "{{request.json.id}}", "status": "SHIPPED", "seq": "{{stream.index}}"}},
                {"jsonBody": {"orderId": "{{request.json.id}}", "status": "DELIVERED", "seq": "{{stream.index}}"}}
            ],
            "trailers": {"x-shard": "7"}
        }}
    }))
    .await;
    up.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/UploadItems"},
        "response": {"jsonBody": {"count": "{{size request.json}}", "firstSku": "{{request.json.[0].sku}}",
                                  "lastSku": "{{jsonPath request.body '$[-1].sku'}}"}}
    }))
    .await;
    up.stub(json!({
        "request": {"urlPath": "/shop.v1.OrderService/Chat"},
        "response": {
            "jsonBody": {"text": "echo {{message.json.text}}", "user": "bot"},
            "grpc": {
                "messages": [{"jsonBody": {"text": "welcome", "user": "bot"}}],
                "onMessage": [{"match": [{"jsonPath": "$.text", "value": {"equalTo": "bye"}}],
                               "messages": [{"jsonBody": {"text": "bye {{message.json.user}}"}}],
                               "status": {"code": "OK"}}]
            }
        }
    }))
    .await;
    up
}

async fn get_order(
    grpc: &mut tonic::client::Grpc<Channel>,
    id: &str,
) -> Result<tonic::Response<Order>, tonic::Status> {
    grpc.ready().await.unwrap();
    let mut req = tonic::Request::new(GetOrderRequest { id: id.into() });
    req.metadata_mut().insert("x-call", "c-1".parse().unwrap());
    grpc.unary(
        req,
        path("GetOrder"),
        ProstCodec::<GetOrderRequest, Order>::default(),
    )
    .await
}

async fn watch(
    grpc: &mut tonic::client::Grpc<Channel>,
    id: &str,
) -> (Vec<OrderEvent>, Option<tonic::metadata::MetadataMap>) {
    grpc.ready().await.unwrap();
    let resp = grpc
        .server_streaming(
            tonic::Request::new(GetOrderRequest { id: id.into() }),
            path("WatchOrder"),
            ProstCodec::<GetOrderRequest, OrderEvent>::default(),
        )
        .await
        .unwrap();
    let mut s = resp.into_inner();
    let mut out = Vec::new();
    while let Some(e) = s.message().await.unwrap() {
        out.push(e);
    }
    (out, s.trailers().await.unwrap())
}

async fn upload(grpc: &mut tonic::client::Grpc<Channel>, skus: &[&str]) -> UploadSummary {
    grpc.ready().await.unwrap();
    let items: Vec<Item> = skus
        .iter()
        .map(|s| Item {
            sku: s.to_string(),
            qty: 1,
        })
        .collect();
    grpc.client_streaming(
        tonic::Request::new(futures::stream::iter(items)),
        path("UploadItems"),
        ProstCodec::<Item, UploadSummary>::default(),
    )
    .await
    .unwrap()
    .into_inner()
}

/// A chat that sends each message after the reply to the previous one arrived.
async fn chat(grpc: &mut tonic::client::Grpc<Channel>, texts: &[&str]) -> Vec<String> {
    grpc.ready().await.unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel::<ChatMessage>(4);
    let stream =
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) });
    let resp = grpc
        .streaming(
            tonic::Request::new(stream),
            path("Chat"),
            ProstCodec::<ChatMessage, ChatMessage>::default(),
        )
        .await
        .unwrap();
    let mut s = resp.into_inner();
    let mut out = vec![s.message().await.unwrap().unwrap().text];
    for t in texts {
        tx.send(ChatMessage {
            text: t.to_string(),
            user: "ann".into(),
        })
        .await
        .unwrap();
        match s.message().await.unwrap() {
            Some(m) => out.push(m.text),
            None => break,
        }
    }
    drop(tx);
    while let Some(m) = s.message().await.unwrap() {
        out.push(m.text);
    }
    out
}

fn details_reason(status: &tonic::Status) -> String {
    let st = RpcStatus::decode(status.details()).unwrap();
    String::from_utf8_lossy(&st.details[0].value).into_owned()
}

#[tokio::test]
async fn proxy_stubs_forward_every_kind_of_call() {
    let up = upstream(|_| {}).await;
    let front = Srv::start(|_| {}).await;
    let proxy = front
        .stub(json!({
            "name": "to the real order service",
            "priority": 100,
            "request": {"urlPathPattern": "/shop\\.v1\\.OrderService/.*"},
            "response": {"proxy": {"baseUrl": format!("http://{}", up.data),
                                   "additionalHeaders": {"x-env": "test"}}}
        }))
        .await;
    front
        .stub(json!({
            "priority": 1,
            "request": {"urlPath": "/shop.v1.OrderService/GetOrder",
                        "body": [{"jsonPath": "$.id", "value": {"equalTo": "A-LOCAL"}}]},
            "response": {"jsonBody": {"id": "A-LOCAL", "status": "MOCKED"}}
        }))
        .await;
    let mut g = front.channel().await;

    let r = get_order(&mut g, "A-1").await.unwrap();
    assert_eq!(r.metadata().get("x-region").unwrap(), "eu-1");
    assert_eq!(
        r.metadata().get("x-upstream-id").unwrap(),
        "c-1",
        "client metadata reaches the upstream"
    );
    let o = r.into_inner();
    assert_eq!(
        (o.id.as_str(), o.status.as_str(), o.items),
        ("A-1", "PAID", 3)
    );
    let local = get_order(&mut g, "A-LOCAL").await.unwrap().into_inner();
    assert_eq!(local.status, "MOCKED", "specific stubs win over the proxy");
    let e = get_order(&mut g, "Z-9").await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::NotFound);
    assert_eq!(e.message(), "order Z-9 not found");
    assert!(
        details_reason(&e).contains("NO_ORDER"),
        "rich details pass through"
    );

    let (events, trailers) = watch(&mut g, "A-7").await;
    assert_eq!(
        events.iter().map(|e| e.status.as_str()).collect::<Vec<_>>(),
        ["PACKED", "SHIPPED", "DELIVERED"]
    );
    assert_eq!(events[2].seq, 2);
    assert_eq!(trailers.unwrap().get("x-shard").unwrap(), "7");

    let s = upload(&mut g, &["S-1", "S-2", "S-3"]).await;
    assert_eq!(
        (s.count, s.first_sku.as_str(), s.last_sku.as_str()),
        (3, "S-1", "S-3")
    );

    let texts = chat(&mut g, &["hi", "how?", "bye"]).await;
    assert_eq!(texts, ["welcome", "echo hi", "echo how?", "bye ann"]);

    let list = front.journal("/shop.v1.OrderService/", 6).await;
    let watch_entry = list
        .iter()
        .find(|e| e["path"] == "/shop.v1.OrderService/WatchOrder")
        .unwrap();
    assert_eq!(watch_entry["grpc"]["responseMessages"], 3);
    assert_eq!(watch_entry["stubId"], proxy["id"], "{watch_entry}");
    let chat_entry = list
        .iter()
        .find(|e| e["path"] == "/shop.v1.OrderService/Chat")
        .unwrap();
    assert_eq!(chat_entry["grpc"]["requestMessages"], 3);
    assert_eq!(chat_entry["grpc"]["responseMessages"], 4);
    let sent: Value = serde_json::from_str(chat_entry["body"].as_str().unwrap()).unwrap();
    assert_eq!(sent[1]["text"], "how?");

    let ups = up.journal("/shop.v1.OrderService/GetOrder", 2).await;
    assert!(
        ups.iter().all(|e| e["headers"]["x-env"] == "test"),
        "{ups:?}"
    );
}

#[tokio::test]
async fn the_egress_policy_and_the_pass_through_switch_apply() {
    let up = upstream(|_| {}).await;
    let blocked = Srv::start(|c| {
        c.callback_denied_cidrs = vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()];
    })
    .await;
    let other = Srv::start(|c| {
        c.callback_allowed_hosts = vec!["grpc.example.com".into()];
    })
    .await;
    for front in [&blocked, &other] {
        front
            .stub(json!({
                "request": {"urlPathPattern": "/shop\\.v1\\..*"},
                "response": {"proxy": {"baseUrl": format!("http://localhost:{}", up.data.port())}}
            }))
            .await;
        let e = get_order(&mut front.channel().await, "A-1")
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unavailable);
        assert_eq!(e.message(), "proxy failed: blocked by egress policy");
    }
    let (st, _) = up
        .admin(reqwest::Method::GET, "/requests?pathPrefix=/shop.v1", None)
        .await;
    assert_eq!(st, 200);
    assert!(
        up.journal("/shop.v1.OrderService/GetOrder", 0)
            .await
            .is_empty(),
        "nothing reached the upstream"
    );

    let front = Srv::start(|_| {}).await;
    front
        .stub(json!({
            "request": {"urlPathPattern": "/shop\\.v1\\..*"},
            "response": {"proxy": {"baseUrl": format!("http://{}", up.data)}}
        }))
        .await;
    let (st, v) = front
        .admin(
            reqwest::Method::PUT,
            "/settings",
            Some(json!({"proxyPassThrough": false})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let e = get_order(&mut front.channel().await, "A-1")
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unimplemented);
    assert!(
        e.message().contains("pass-through is disabled"),
        "{}",
        e.message()
    );

    let dead = Srv::start(|_| {}).await;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    dead.stub(json!({
        "request": {"urlPathPattern": "/shop\\.v1\\..*"},
        "response": {"proxy": {"baseUrl": format!("http://127.0.0.1:{port}")}}
    }))
    .await;
    let e = get_order(&mut dead.channel().await, "A-1")
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unavailable);
    assert_eq!(e.message(), "proxy failed: upstream unavailable");
}

struct Pki {
    dir: std::path::PathBuf,
}

fn pki() -> Pki {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };
    let dir = std::env::temp_dir().join(format!("fm-grpc-proxy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "fastmock grpc proxy test ca");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let sk = KeyPair::generate().unwrap();
    let mut sp = CertificateParams::new(vec!["localhost".into()]).unwrap();
    sp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server = sp.signed_by(&sk, &issuer).unwrap();
    let ck = KeyPair::generate().unwrap();
    let mut cp = CertificateParams::new(vec![]).unwrap();
    cp.distinguished_name.push(DnType::CommonName, "fastmock");
    cp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = cp.signed_by(&ck, &issuer).unwrap();
    for (name, text) in [
        ("ca.pem", ca.pem()),
        ("server.pem", server.pem()),
        ("server.key", sk.serialize_pem()),
        ("client.pem", client.pem()),
        ("client.key", ck.serialize_pem()),
    ] {
        std::fs::write(dir.join(name), text).unwrap();
    }
    Pki { dir }
}

#[tokio::test]
async fn upstreams_over_tls_and_mtls() {
    let p = pki();
    let up = upstream(|c| {
        c.tls = Some(TlsSettings {
            cert_file: p.dir.join("server.pem"),
            key_file: p.dir.join("server.key"),
            client_ca_file: Some(p.dir.join("ca.pem")),
            client_auth_required: true,
        });
    })
    .await;
    let base = format!("https://localhost:{}", up.data.port());
    let with_cert = |hosts: Vec<String>| {
        let dir = p.dir.clone();
        move |c: &mut Config| {
            c.outbound_ca_file = Some(dir.join("ca.pem"));
            c.grpc_proxy.client_cert = Some(fastmock_kafka::ClientCertFiles {
                cert_file: dir.join("client.pem"),
                key_file: dir.join("client.key"),
            });
            c.grpc_proxy.client_cert_hosts = hosts;
        }
    };
    let front = Srv::start(with_cert(vec!["localhost".into()])).await;
    front
        .stub(json!({
            "request": {"urlPathPattern": "/shop\\.v1\\..*"},
            "response": {"proxy": {"baseUrl": base}}
        }))
        .await;
    let mut g = front.channel().await;
    let o = get_order(&mut g, "A-1").await.unwrap().into_inner();
    assert_eq!(o.status, "PAID");
    let (events, _) = watch(&mut g, "A-2").await;
    assert_eq!(events.len(), 3);

    let elsewhere = Srv::start(with_cert(vec!["grpc.example.com".into()])).await;
    elsewhere
        .stub(json!({
            "request": {"urlPathPattern": "/shop\\.v1\\..*"},
            "response": {"proxy": {"baseUrl": base}}
        }))
        .await;
    let e = get_order(&mut elsewhere.channel().await, "A-1")
        .await
        .unwrap_err();
    assert_eq!(
        e.code(),
        tonic::Code::Unavailable,
        "the certificate goes only to the configured hosts: {e:?}"
    );

    let untrusted = Srv::start(|_| {}).await;
    untrusted
        .stub(json!({
            "request": {"urlPathPattern": "/shop\\.v1\\..*"},
            "response": {"proxy": {"baseUrl": base}}
        }))
        .await;
    let e = get_order(&mut untrusted.channel().await, "A-1")
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unavailable, "{e:?}");
}

#[tokio::test]
async fn recording_turns_unmatched_calls_into_stubs() {
    let up = upstream(|_| {}).await;
    let front = Srv::start(|_| {}).await;
    let (st, v) = front
        .admin(
            reqwest::Method::POST,
            "/recordings/start",
            Some(json!({"targetBaseUrl": "https://api.example.com",
                        "grpcTargetBaseUrl": format!("http://{}", up.data),
                        "tag": "rec-grpc", "captureHeaders": ["x-region", "x-shard"]})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let mut g = front.channel().await;
    assert_eq!(
        get_order(&mut g, "A-1").await.unwrap().into_inner().status,
        "PAID"
    );
    assert_eq!(
        get_order(&mut g, "A-1").await.unwrap().into_inner().status,
        "PAID"
    );
    assert_eq!(
        get_order(&mut g, "Z-9").await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    assert_eq!(watch(&mut g, "A-7").await.0.len(), 3);
    assert_eq!(upload(&mut g, &["S-1", "S-2"]).await.count, 2);
    assert_eq!(
        chat(&mut g, &["hi", "bye"]).await,
        ["welcome", "echo hi", "bye ann"]
    );

    let mut stubs = Vec::new();
    for _ in 0..100 {
        let (_, v) = front
            .admin(reqwest::Method::GET, "/stubs?tag=rec-grpc", None)
            .await;
        stubs = v["stubs"].as_array().cloned().unwrap_or_default();
        if stubs.len() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(stubs.len(), 5, "one stub per distinct call: {stubs:#?}");
    let by = |p: &str, f: &dyn Fn(&Value) -> bool| -> Value {
        stubs
            .iter()
            .find(|s| s["request"]["urlPath"] == format!("/shop.v1.OrderService/{p}") && f(s))
            .cloned()
            .unwrap_or_else(|| panic!("no recorded stub for {p}: {stubs:#?}"))
    };
    let ok = by("GetOrder", &|s| s["response"]["jsonBody"].is_object());
    assert_eq!(
        ok["request"]["body"][0]["equalToJson"],
        json!({"id": "A-1"})
    );
    assert_eq!(ok["response"]["jsonBody"]["status"], "PAID");
    assert_eq!(ok["response"]["headers"]["x-region"], "eu-1");
    assert_eq!(ok["response"]["templated"], false);
    let nf = by("GetOrder", &|s| s["response"]["grpc"]["status"].is_object());
    assert_eq!(nf["response"]["grpc"]["status"]["code"], "NOT_FOUND");
    assert_eq!(
        nf["response"]["grpc"]["status"]["details"][0]["reason"],
        "NO_ORDER"
    );
    let w = by("WatchOrder", &|_| true);
    assert_eq!(
        w["response"]["grpc"]["messages"].as_array().unwrap().len(),
        3
    );
    assert_eq!(w["response"]["grpc"]["trailers"]["x-shard"], "7");
    let c = by("Chat", &|_| true);
    assert_eq!(
        c["response"]["grpc"]["messages"][0]["jsonBody"]["text"],
        "welcome"
    );
    assert_eq!(
        c["response"]["grpc"]["onMessage"].as_array().unwrap().len(),
        2,
        "{c}"
    );

    let (st, v) = front
        .admin(reqwest::Method::POST, "/recordings/stop", None)
        .await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = up.admin(reqwest::Method::DELETE, "/stubs", None).await;
    assert!(st == 200 || st == 204, "{st} {v}");

    let mut g = front.channel().await;
    let r = get_order(&mut g, "A-1").await.unwrap();
    assert_eq!(r.metadata().get("x-region").unwrap(), "eu-1");
    assert_eq!(r.into_inner().items, 3);
    let e = get_order(&mut g, "Z-9").await.unwrap_err();
    assert_eq!(
        (e.code(), e.message()),
        (tonic::Code::NotFound, "order Z-9 not found")
    );
    assert!(details_reason(&e).contains("NO_ORDER"));
    let (events, trailers) = watch(&mut g, "A-7").await;
    assert_eq!(events[1].status, "SHIPPED");
    assert_eq!(trailers.unwrap().get("x-shard").unwrap(), "7");
    let s = upload(&mut g, &["S-1", "S-2"]).await;
    assert_eq!((s.count, s.last_sku.as_str()), (2, "S-2"));
    assert_eq!(
        chat(&mut g, &["hi", "bye"]).await,
        ["welcome", "echo hi", "bye ann"],
        "the recorded stub replays the conversation"
    );
    let e = get_order(&mut g, "B-1").await.unwrap_err();
    assert_eq!(
        e.code(),
        tonic::Code::Unimplemented,
        "nothing was recorded for B-1"
    );
}

#[tokio::test]
async fn recording_limits_skip_large_calls_but_still_proxy_them() {
    let up = upstream(|_| {}).await;
    let front = Srv::start(|c| {
        c.grpc_proxy.record_max_messages = 2;
    })
    .await;
    let (st, v) = front
        .admin(
            reqwest::Method::POST,
            "/recordings/start",
            Some(json!({"targetBaseUrl": format!("http://{}", up.data), "tag": "rec-small"})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let mut g = front.channel().await;
    let (events, _) = watch(&mut g, "A-1").await;
    assert_eq!(events.len(), 3, "the call is proxied in full");
    assert_eq!(
        get_order(&mut g, "A-2").await.unwrap().into_inner().status,
        "PAID"
    );
    let mut stubs = Vec::new();
    for _ in 0..100 {
        let (_, v) = front
            .admin(reqwest::Method::GET, "/stubs?tag=rec-small", None)
            .await;
        stubs = v["stubs"].as_array().cloned().unwrap_or_default();
        if !stubs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, v) = front
        .admin(reqwest::Method::GET, "/stubs?tag=rec-small", None)
        .await;
    let stubs_now = v["stubs"].as_array().cloned().unwrap_or_default();
    assert_eq!(stubs_now.len(), 1, "{stubs:#?}");
    assert_eq!(
        stubs_now[0]["request"]["urlPath"],
        "/shop.v1.OrderService/GetOrder"
    );
    let metrics = prom().render();
    assert!(
        metrics.contains("fastmock_recording_skipped_total{reason=\"too_large\"}"),
        "{metrics}"
    );
}
