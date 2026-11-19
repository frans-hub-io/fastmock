use std::path::PathBuf;
use std::time::Duration;

use fastmock_core::HotStore;
use fastmock_store::{
    PgStore, PgTls, SslMode, ValkeyStore, ValkeyTls, connect_postgres, valkey_client,
};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn path(name: &str) -> Option<PathBuf> {
    var(name).map(PathBuf::from)
}

struct WrongCa {
    dir: PathBuf,
}

impl WrongCa {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("fm-wrong-ca-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
        Self { dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }
}

impl Drop for WrongCa {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pg_tls_env() -> Option<(String, PathBuf)> {
    match (
        var("FASTMOCK_TEST_DATABASE_TLS_URL"),
        path("FASTMOCK_TEST_DATABASE_TLS_CA"),
    ) {
        (Some(u), Some(ca)) => Some((u, ca)),
        _ => {
            eprintln!(
                "FASTMOCK_TEST_DATABASE_TLS_URL/FASTMOCK_TEST_DATABASE_TLS_CA not set: skipped"
            );
            None
        }
    }
}

fn verify_full(ca: Option<PathBuf>) -> PgTls {
    PgTls {
        mode: Some(SslMode::VerifyFull),
        root_cert: ca,
        ..Default::default()
    }
}

#[tokio::test]
async fn postgres_verify_full_with_private_ca() {
    let Some((url, ca)) = pg_tls_env() else {
        return;
    };
    let pg = connect_postgres(&url, &verify_full(Some(ca)), 2)
        .await
        .expect("verify-full with the private CA");
    let tls: bool = sqlx::query_scalar("select ssl from pg_stat_ssl where pid = pg_backend_pid()")
        .fetch_one(pg.pool())
        .await
        .unwrap();
    assert!(tls, "the session must be encrypted");
    fastmock_store::migrate(&pg, Duration::from_secs(60))
        .await
        .unwrap();
    pg.check_schema().await.unwrap();
}

#[tokio::test]
async fn postgres_url_parameters_select_tls() {
    let Some((url, ca)) = pg_tls_env() else {
        return;
    };
    let sep = if url.contains('?') { '&' } else { '?' };
    let with_params = format!("{url}{sep}sslmode=verify-full&sslrootcert={}", ca.display());
    let pg = connect_postgres(&with_params, &PgTls::default(), 2)
        .await
        .expect("sslmode and sslrootcert from the URL");
    pg.ping().await.unwrap();
}

#[tokio::test]
async fn postgres_wrong_or_missing_ca_is_rejected() {
    let Some((url, _)) = pg_tls_env() else {
        return;
    };
    let wrong = WrongCa::new();
    for tls in [verify_full(Some(wrong.file())), verify_full(None)] {
        let e = match connect_postgres(&url, &tls, 2).await {
            Ok(_) => panic!("connected with an untrusted server certificate"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains("UnknownIssuer"), "{e}");
        assert!(e.contains("FASTMOCK_DATABASE_SSL_ROOT_CERT"), "{e}");
    }
    let wrong_ca = PgTls {
        mode: Some(SslMode::VerifyCa),
        root_cert: Some(wrong.file()),
        ..Default::default()
    };
    assert!(connect_postgres(&url, &wrong_ca, 2).await.is_err());
}

#[tokio::test]
async fn postgres_client_certificate() {
    let Some((_, ca)) = pg_tls_env() else {
        return;
    };
    let (Some(url), Some(cert), Some(key)) = (
        var("FASTMOCK_TEST_DATABASE_TLS_MTLS_URL"),
        path("FASTMOCK_TEST_DATABASE_TLS_CERT"),
        path("FASTMOCK_TEST_DATABASE_TLS_KEY"),
    ) else {
        eprintln!("FASTMOCK_TEST_DATABASE_TLS_MTLS_URL/_CERT/_KEY not set: skipped");
        return;
    };
    let tls = PgTls {
        mode: Some(SslMode::VerifyFull),
        root_cert: Some(ca.clone()),
        client_cert: Some(cert),
        client_key: Some(key),
    };
    let pg = connect_postgres(&url, &tls, 2)
        .await
        .expect("certificate authentication");
    let user: String = sqlx::query_scalar("select current_user::text")
        .fetch_one(pg.pool())
        .await
        .unwrap();
    assert!(!user.is_empty());
    fastmock_store::migrate(&pg, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        connect_postgres(&url, &verify_full(Some(ca)), 2)
            .await
            .is_err(),
        "the server must refuse the certificate user without a client certificate"
    );
}

struct ValkeyEnv {
    url: String,
    tls: ValkeyTls,
    username: Option<String>,
    password: Option<String>,
}

fn valkey_tls_env() -> Option<ValkeyEnv> {
    let Some(url) = var("FASTMOCK_TEST_VALKEY_TLS_URL") else {
        eprintln!("FASTMOCK_TEST_VALKEY_TLS_URL not set: skipped");
        return None;
    };
    Some(ValkeyEnv {
        url,
        tls: ValkeyTls {
            ca_file: path("FASTMOCK_TEST_VALKEY_TLS_CA"),
            cert_file: path("FASTMOCK_TEST_VALKEY_TLS_CERT"),
            key_file: path("FASTMOCK_TEST_VALKEY_TLS_KEY"),
        },
        username: var("FASTMOCK_TEST_VALKEY_TLS_USERNAME"),
        password: var("FASTMOCK_TEST_VALKEY_TLS_PASSWORD"),
    })
}

fn with_password(url: &str, password: Option<&str>) -> String {
    match password {
        None => url.to_string(),
        Some(p) => {
            let (scheme, rest) = url.split_once("://").unwrap();
            let p: String = p
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("{scheme}://:{p}@{rest}")
        }
    }
}

async fn valkey_store(
    env: &ValkeyEnv,
    tls: &ValkeyTls,
    password: Option<&str>,
) -> anyhow::Result<ValkeyStore> {
    let client = valkey_client(
        &with_password(&env.url, password),
        env.username.as_deref(),
        tls,
    )?;
    let prefix = format!("fmtest:{}:", uuid::Uuid::new_v4().simple());
    tokio::time::timeout(
        Duration::from_secs(15),
        ValkeyStore::connect_with(client, &prefix),
    )
    .await?
}

#[tokio::test]
async fn valkey_tls_acl_user_runs_every_command() {
    let Some(env) = valkey_tls_env() else {
        return;
    };
    let h = valkey_store(&env, &env.tls, env.password.as_deref())
        .await
        .expect("TLS connection with the configured CA, client certificate and ACL user");
    h.ping().await.unwrap();
    h.set("k", "v", None).await.unwrap();
    h.set("t", "v", Some(Duration::from_secs(5))).await.unwrap();
    assert_eq!(h.get("k").await.unwrap().as_deref(), Some("v"));
    assert!(h.compare_and_set("k", Some("v"), "w", None).await.unwrap());
    assert!(
        h.compare_and_set("n", None, "x", Some(Duration::from_secs(5)))
            .await
            .unwrap()
    );
    assert_eq!(
        h.incr_by("c", 2, Some(Duration::from_secs(5)))
            .await
            .unwrap(),
        2
    );
    assert_eq!(h.scan("", 100).await.unwrap().len(), 4);
    h.zadd("q", 1.0, "a").await.unwrap();
    h.zadd("q", 2.0, "b").await.unwrap();
    assert_eq!(h.zpop_until("q", 10.0, 10).await.unwrap(), vec!["a", "b"]);
    assert!(h.delete("k").await.unwrap());
    assert_eq!(h.delete_prefix("").await.unwrap(), 3);
}

#[tokio::test]
async fn valkey_tls_rejects_wrong_ca_and_missing_credentials() {
    let Some(env) = valkey_tls_env() else {
        return;
    };
    let wrong = WrongCa::new();
    let tls = ValkeyTls {
        ca_file: Some(wrong.file()),
        ..env.tls.clone()
    };
    let e = format!(
        "{:#}",
        valkey_store(&env, &tls, env.password.as_deref())
            .await
            .err()
            .expect("an untrusted server certificate must fail")
    );
    assert!(e.contains("UnknownIssuer"), "{e}");
    assert!(e.contains("FASTMOCK_VALKEY_TLS_CA_FILE"), "{e}");
    if env.tls.cert_file.is_some() {
        let tls = ValkeyTls {
            cert_file: None,
            key_file: None,
            ..env.tls.clone()
        };
        assert!(
            valkey_store(&env, &tls, env.password.as_deref())
                .await
                .is_err(),
            "the server requires a client certificate"
        );
    }
    if env.username.is_some() {
        assert!(
            valkey_store(&env, &env.tls, Some("wrong-password"))
                .await
                .is_err(),
            "a wrong ACL password must fail"
        );
    }
}

#[tokio::test]
async fn plain_urls_reject_tls_settings() {
    let tls = ValkeyTls {
        ca_file: Some("/nonexistent/ca.pem".into()),
        ..Default::default()
    };
    let e = valkey_client("redis://127.0.0.1:6379/", None, &tls)
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("rediss://"), "{e}");
    let _ = PgStore::expected_schema_version();
}
