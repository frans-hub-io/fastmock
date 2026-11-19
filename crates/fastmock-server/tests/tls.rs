use std::sync::OnceLock;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind, TlsSettings};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use serde_json::{Value, json};
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_tls_tests_0123456789";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

struct Pki {
    ca_pem: String,
    server_cert: String,
    server_key: String,
    client_pem: String,
}

fn pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "fastmock test ca");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let sk = KeyPair::generate().unwrap();
    let mut sp = CertificateParams::new(vec!["localhost".into()]).unwrap();
    sp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server = sp.signed_by(&sk, &issuer).unwrap();
    let ck = KeyPair::generate().unwrap();
    let mut cp = CertificateParams::new(vec![]).unwrap();
    cp.distinguished_name
        .push(DnType::CommonName, "test-client");
    cp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = cp.signed_by(&ck, &issuer).unwrap();
    Pki {
        ca_pem: ca.pem(),
        server_cert: server.pem(),
        server_key: sk.serialize_pem(),
        client_pem: format!("{}{}", client.pem(), ck.serialize_pem()),
    }
}

#[tokio::test]
async fn https_mtls_http2_and_client_cert_matching() {
    let p = pki();
    let dir = std::env::temp_dir().join(format!("fm-tls-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.crt"), &p.server_cert).unwrap();
    std::fs::write(dir.join("server.key"), &p.server_key).unwrap();
    std::fs::write(dir.join("ca.crt"), &p.ca_pem).unwrap();
    let mut cfg = Config::for_tests(
        StorageKind::Memory,
        vec![format!("ops:admin:{}", hash_token(ADMIN))],
    );
    cfg.tls = Some(TlsSettings {
        cert_file: dir.join("server.crt"),
        key_file: dir.join("server.key"),
        client_ca_file: Some(dir.join("ca.crt")),
        client_auth_required: true,
    });
    let tls_cfg = fastmock_server::tls::server_config(cfg.tls.as_ref().unwrap()).unwrap();
    let (_app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
    let (_tx, rx) = watch::channel(false);
    let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (dport, aaddr) = (dl.local_addr().unwrap().port(), al.local_addr().unwrap());
    tokio::spawn(fastmock_server::tls::serve(
        dl,
        data,
        Some(tls_cfg),
        rx,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(1),
    ));
    tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
    let plain = reqwest::Client::new();
    let fp = {
        let pem = p
            .client_pem
            .split("-----BEGIN PRIVATE KEY-----")
            .next()
            .unwrap()
            .to_string();
        use rustls::pki_types::pem::PemObject;
        let der = rustls::pki_types::CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(der.as_ref()))
    };
    let r = plain
        .post(format!("http://{aaddr}/__admin/stubs"))
        .bearer_auth(ADMIN)
        .json(&json!({"request": {"urlPath": "/secure", "headers": {"x-fastmock-client-cert-sha256": {"equalTo": fp}}}, "response": {"body": "hello trusted client"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let ca = reqwest::Certificate::from_pem(p.ca_pem.as_bytes()).unwrap();
    let anon = reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(ca.clone())
        .build()
        .unwrap();
    assert!(
        anon.get(format!("https://localhost:{dport}/secure"))
            .send()
            .await
            .is_err(),
        "client certificate is required"
    );
    let id = reqwest::Identity::from_pem(p.client_pem.as_bytes()).unwrap();
    let client = reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(ca)
        .identity(id)
        .build()
        .unwrap();
    let r = client
        .get(format!("https://localhost:{dport}/secure"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.version(),
        reqwest::Version::HTTP_2,
        "ALPN negotiates HTTP/2"
    );
    assert_eq!(r.text().await.unwrap(), "hello trusted client");
    let spoof = client
        .get(format!("https://localhost:{dport}/secure"))
        .header("x-fastmock-client-cert-sha256", "0000")
        .send()
        .await
        .unwrap();
    assert_eq!(
        spoof.status(),
        200,
        "a spoofed header is replaced by the real fingerprint"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let j: Value = plain
        .get(format!(
            "http://{aaddr}/__admin/requests?pathPrefix=/secure"
        ))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        j["requests"][0]["headers"]["x-fastmock-client-cert-sha256"],
        fp.as_str()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn plaintext_port_speaks_h2c_and_strips_spoofed_cert_header() {
    let cfg = Config::for_tests(
        StorageKind::Memory,
        vec![format!("ops:admin:{}", hash_token(ADMIN))],
    );
    let (_app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
    let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (daddr, aaddr) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
    tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
    let c = reqwest::Client::new();
    c.post(format!("http://{aaddr}/__admin/stubs"))
        .bearer_auth(ADMIN)
        .json(&json!({"request": {"urlPath": "/s", "headers": {"x-fastmock-client-cert-sha256": {"equalTo": "forged"}}}, "response": {"body": "should never match"}}))
        .send()
        .await
        .unwrap();
    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let r = h2
        .get(format!("http://{daddr}/s"))
        .header("x-fastmock-client-cert-sha256", "forged")
        .send()
        .await
        .unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_2);
    assert_eq!(
        r.status(),
        404,
        "the fingerprint header can only come from a verified TLS client certificate"
    );
}

#[tokio::test]
async fn outbound_client_trusts_extra_ca_file() {
    let p = pki();
    let dir = std::env::temp_dir().join(format!("fm-ca-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.crt"), &p.server_cert).unwrap();
    std::fs::write(dir.join("server.key"), &p.server_key).unwrap();
    std::fs::write(dir.join("ca.crt"), &p.ca_pem).unwrap();
    std::fs::write(dir.join("empty.crt"), "").unwrap();
    let mut cfg = Config::for_tests(
        StorageKind::Memory,
        vec![format!("ops:admin:{}", hash_token(ADMIN))],
    );
    cfg.tls = Some(TlsSettings {
        cert_file: dir.join("server.crt"),
        key_file: dir.join("server.key"),
        client_ca_file: None,
        client_auth_required: false,
    });
    let tls_cfg = fastmock_server::tls::server_config(cfg.tls.as_ref().unwrap()).unwrap();
    let (_app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
    let (_tx, rx) = watch::channel(false);
    let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (dport, aaddr) = (dl.local_addr().unwrap().port(), al.local_addr().unwrap());
    tokio::spawn(fastmock_server::tls::serve(
        dl,
        data,
        Some(tls_cfg),
        rx,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(1),
    ));
    tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
    let r = reqwest::Client::new()
        .post(format!("http://{aaddr}/__admin/stubs"))
        .bearer_auth(ADMIN)
        .json(&json!({"request": {"urlPath": "/ca"}, "response": {"body": "trusted"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let policy = std::sync::Arc::new(fastmock_server::outbox::EgressPolicy {
        allowed_hosts: vec![],
        denied: vec![],
    });
    let url = format!("https://localhost:{dport}/ca");
    let timeout = std::time::Duration::from_secs(5);
    let default_client =
        fastmock_server::outbox::http_client(policy.clone(), timeout, &[]).unwrap();
    assert!(default_client.get(&url).send().await.is_err());
    let roots = fastmock_server::outbox::load_outbound_ca(Some(&dir.join("ca.crt"))).unwrap();
    let client = fastmock_server::outbox::http_client(policy, timeout, &roots).unwrap();
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "trusted");
    assert!(fastmock_server::outbox::load_outbound_ca(Some(&dir.join("empty.crt"))).is_err());
    assert!(fastmock_server::outbox::load_outbound_ca(Some(&dir.join("missing.crt"))).is_err());
}
