use std::sync::Arc;

use anyhow::Context;
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

/// Runs the `fastmock` command line with the plugins registered through
/// [`register_plugins`](crate::register_plugins).
pub async fn run() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Migrate => {
            let url = std::env::var("FASTMOCK_DATABASE_URL")
                .context("FASTMOCK_DATABASE_URL is required")?;
            let pg = fastmock_store::PgStore::connect(&url, 2).await?;
            pg.migrate().await?;
            println!("migrations applied");
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
