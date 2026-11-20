use std::sync::Arc;

use clap::{Parser, Subcommand};
use fastmock_core::memory::memory_stores;
use fastmock_core::template::Templates;
use fastmock_core::{Engine, Stub};

use crate::{all_plugins, auth, serve};

#[derive(Parser)]
#[command(
    name = "fastmock",
    version,
    about = "FastMock: API mock server with durable callbacks, shared state and full observability"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Migrate,
    HashToken {
        #[arg(long)]
        generate: bool,
        token: Option<String>,
    },
    Validate {
        file: std::path::PathBuf,
    },
    Plugins,
}

async fn migrate(db: &crate::config::DatabaseSettings) -> anyhow::Result<String> {
    fastmock_store::pg_connect_options(&db.url, &db.tls)?;
    let pg = fastmock_store::connect_postgres(&db.url, &db.tls, 2).await?;
    let applied = fastmock_store::migrate(&pg, db.migration_timeout).await?;
    Ok(migrate_summary(
        &applied,
        fastmock_store::PgStore::expected_schema_version(),
    ))
}

/// The line `fastmock migrate` ends with.
fn migrate_summary(applied: &[fastmock_store::AppliedMigration], version: i64) -> String {
    match (applied.first(), applied.last()) {
        (Some(first), Some(last)) => {
            let ms: u128 = applied.iter().map(|m| m.duration.as_millis()).sum();
            let range = if first.version == last.version {
                first.version.to_string()
            } else {
                format!("{}-{}", first.version, last.version)
            };
            format!(
                "migrations applied (schema version {version}): {} applied ({range}) in {ms} ms",
                applied.len()
            )
        }
        _ => format!(
            "migrations applied (schema version {version}): none, the schema was up to date"
        ),
    }
}

/// Runs the `fastmock` command line with the plugins registered through
/// [`register_plugins`](crate::register_plugins).
pub async fn run() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Migrate => {
            let db = crate::config::database_from_env()?;
            let telemetry = crate::telemetry::init(crate::config::log_format_from_env()?)?;
            let result = migrate(&db).await;
            telemetry.shutdown();
            println!("{}", result?);
            Ok(())
        }
        Command::HashToken { generate, token } => {
            let token = match (generate, token) {
                (true, _) => auth::generate_token(),
                (false, Some(t)) => t,
                (false, None) => {
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s)?;
                    s.trim().to_string()
                }
            };
            if generate {
                println!("token: {token}");
            }
            println!("hash:  {}", auth::hash_token(&token));
            Ok(())
        }
        Command::Validate { file } => {
            let text = std::fs::read_to_string(&file)?;
            let v: serde_json::Value = serde_json::from_str(&text)?;
            let list = match v {
                serde_json::Value::Array(a) => a,
                serde_json::Value::Object(ref o) if o.contains_key("stubs") => {
                    o["stubs"].as_array().cloned().unwrap_or_default()
                }
                other => vec![other],
            };
            let engine = Engine::new(
                memory_stores(1),
                Arc::new(Templates::new()),
                Arc::new(fastmock_core::NoSecrets),
            );
            let mut errors = 0;
            for (i, s) in list.into_iter().enumerate() {
                let r = serde_json::from_value::<Stub>(s)
                    .map_err(|e| e.to_string())
                    .and_then(|s| engine.validate(&s));
                if let Err(e) = r {
                    errors += 1;
                    eprintln!("stub #{i}: {e}");
                }
            }
            if errors > 0 {
                anyhow::bail!("{errors} invalid stub(s)");
            }
            println!("all stubs are valid");
            Ok(())
        }
        Command::Plugins => {
            for p in all_plugins() {
                println!("{:<14} {}", p.name(), p.description());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use fastmock_store::AppliedMigration;

    use super::migrate_summary;

    fn applied(version: i64, ms: u64) -> AppliedMigration {
        AppliedMigration {
            version,
            name: format!("m{version}"),
            duration: Duration::from_millis(ms),
        }
    }

    #[test]
    fn migrate_summary_names_what_was_applied() {
        assert_eq!(
            migrate_summary(&[applied(1, 20), applied(2, 5), applied(10, 1)], 10),
            "migrations applied (schema version 10): 3 applied (1-10) in 26 ms"
        );
        assert_eq!(
            migrate_summary(&[applied(10, 3)], 10),
            "migrations applied (schema version 10): 1 applied (10) in 3 ms"
        );
        assert_eq!(
            migrate_summary(&[], 10),
            "migrations applied (schema version 10): none, the schema was up to date"
        );
    }
}
