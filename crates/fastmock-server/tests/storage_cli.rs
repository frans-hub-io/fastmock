use std::process::{Command, Output};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn migrate(env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fastmock"));
    cmd.arg("migrate");
    for (k, _) in std::env::vars() {
        if k.starts_with("FASTMOCK_") || k.starts_with("PG") || k == "RUST_LOG" {
            cmd.env_remove(k);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn migrate_validates_settings_before_connecting() {
    let o = migrate(&[
        ("FASTMOCK_DATABASE_URL", "postgres://fm@db.example.com/fm"),
        ("FASTMOCK_DATABASE_SSL_MODE", "verify-everything"),
    ]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("FASTMOCK_DATABASE_SSL_MODE"),
        "{}",
        text(&o)
    );
    let o = migrate(&[
        ("FASTMOCK_DATABASE_URL", "postgres://fm@db.example.com/fm"),
        ("FASTMOCK_DATABASE_SSL_CERT", "/nonexistent/client.pem"),
    ]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("FASTMOCK_DATABASE_SSL_KEY"),
        "{}",
        text(&o)
    );
    let o = migrate(&[
        ("FASTMOCK_DATABASE_URL", "postgres://fm@db.example.com/fm"),
        ("FASTMOCK_DATABASE_SSL_MODE", "verify-full"),
        ("FASTMOCK_DATABASE_SSL_ROOT_CERT", "/nonexistent/ca.pem"),
    ]);
    assert!(!o.status.success());
    assert!(text(&o).contains("cannot read"), "{}", text(&o));
}

#[test]
fn migrate_uses_tls_settings_and_password() {
    let (Some(url), Some(ca)) = (
        var("FASTMOCK_TEST_DATABASE_TLS_URL"),
        var("FASTMOCK_TEST_DATABASE_TLS_CA"),
    ) else {
        eprintln!("FASTMOCK_TEST_DATABASE_TLS_URL/FASTMOCK_TEST_DATABASE_TLS_CA not set: skipped");
        return;
    };
    let mut u = reqwest::Url::parse(&url).unwrap();
    let password = u.password().map(|p| {
        let bytes: Vec<u8> = p
            .split('%')
            .enumerate()
            .flat_map(|(i, part)| {
                if i == 0 {
                    part.as_bytes().to_vec()
                } else {
                    let mut v = vec![u8::from_str_radix(&part[..2], 16).unwrap()];
                    v.extend_from_slice(&part.as_bytes()[2..]);
                    v
                }
            })
            .collect();
        String::from_utf8(bytes).unwrap()
    });
    u.set_password(None).unwrap();
    let mut env = vec![
        ("FASTMOCK_DATABASE_URL", u.to_string()),
        ("FASTMOCK_DATABASE_SSL_MODE", "verify-full".to_string()),
        ("FASTMOCK_DATABASE_SSL_ROOT_CERT", ca),
    ];
    if let Some(p) = password {
        env.push(("FASTMOCK_DATABASE_PASSWORD", p));
    }
    let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let o = migrate(&refs);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("migrations applied"), "{}", text(&o));

    let without_ca: Vec<(&str, &str)> = refs
        .iter()
        .copied()
        .filter(|(k, _)| *k != "FASTMOCK_DATABASE_SSL_ROOT_CERT")
        .collect();
    let o = migrate(&without_ca);
    assert!(!o.status.success());
    let out = text(&o);
    assert!(out.contains("UnknownIssuer"), "{out}");
    assert!(out.contains("FASTMOCK_DATABASE_SSL_ROOT_CERT"), "{out}");
    if let Some(p) = refs
        .iter()
        .find(|(k, _)| *k == "FASTMOCK_DATABASE_PASSWORD")
        .map(|(_, v)| *v)
    {
        assert!(!out.contains(p), "the password must not be printed");
    }
}

fn migration_versions() -> Vec<i64> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fastmock-store/migrations");
    let mut v: Vec<i64> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.split('_').next()?.parse().ok()
        })
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn migrate_logs_each_applied_migration_and_a_summary() {
    use sqlx::Connection;
    let Some(admin_url) = var("FASTMOCK_TEST_DATABASE_URL") else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL not set: skipped");
        return;
    };
    let db = format!("fm_migrate_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    let url = format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db);
    let versions = migration_versions();
    let latest = *versions.last().unwrap();

    let o = migrate(&[("FASTMOCK_DATABASE_URL", &url)]);
    assert!(o.status.success(), "{}", text(&o));
    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
    let applied: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|l| l["message"] == "applied database migration")
        .collect();
    assert_eq!(
        applied
            .iter()
            .map(|l| l["version"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        versions,
        "{stdout}"
    );
    assert!(
        applied
            .iter()
            .all(|l| l["name"].as_str().is_some_and(|n| !n.is_empty()) && l["duration_ms"].is_u64()),
        "{stdout}"
    );
    let summary = stdout.lines().last().unwrap();
    assert!(
        summary.starts_with(&format!(
            "migrations applied (schema version {latest}): {} applied ({}-{latest}) in ",
            versions.len(),
            versions[0]
        )),
        "{stdout}"
    );

    let o = migrate(&[
        ("FASTMOCK_DATABASE_URL", &url),
        ("FASTMOCK_LOG_FORMAT", "text"),
    ]);
    assert!(o.status.success(), "{}", text(&o));
    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(!stdout.contains("applied database migration"), "{stdout}");
    assert!(stdout.contains("no migration applied"), "{stdout}");
    assert_eq!(
        stdout.lines().last().unwrap(),
        format!("migrations applied (schema version {latest}): none, the schema was up to date"),
        "{stdout}"
    );
    drop(c);
}
