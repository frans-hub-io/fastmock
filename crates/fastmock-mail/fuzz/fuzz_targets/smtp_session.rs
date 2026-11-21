#![no_main]

use std::sync::Arc;
use std::time::Duration;

use fastmock_core::mail::{LocalMailEvents, MemoryMailStore};
use fastmock_mail::chaos::{MailChaos, MemoryChaosBackend};
use fastmock_mail::smtp::SmtpServer;
use fastmock_mail::{MailConfig, MailDomains, MailService};
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fuzz_target!(|data: &[u8]| {
    let Some((&flags, input)) = data.split_first() else {
        return;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut cfg = MailConfig {
            max_message_bytes: 4096,
            max_recipients: 4,
            max_line_bytes: 512,
            smtp_require_auth: flags & 1 != 0,
            idle_timeout: Duration::from_secs(5),
            data_timeout: Duration::from_secs(5),
            session_timeout: Duration::from_secs(60),
            ..Default::default()
        };
        if flags & 2 != 0 {
            cfg.smtp_users = fastmock_mail::config::parse_smtp_users(
                "app:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            )
            .unwrap();
        }
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        let svc = Arc::new(MailService::new(
            Arc::new(cfg),
            Arc::new(MemoryMailStore::new(16)),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing".to_string()].into(),
        ));
        let chaos = (flags & 4 != 0).then(|| {
            Arc::new(MailChaos::with_cache_ttl(
                Arc::new(MemoryChaosBackend::default()),
                Duration::ZERO,
            ))
        });
        let srv = SmtpServer::new(svc, None, None, chaos);
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let input = input.to_vec();
        let feeder = async move {
            for chunk in input.chunks(97) {
                if client.write_all(chunk).await.is_err() {
                    return;
                }
            }
            let _ = client.shutdown().await;
            let mut sink = Vec::new();
            let _ = client.read_to_end(&mut sink).await;
        };
        let session = srv.serve(server, "198.51.100.1:2525".parse().unwrap(), false, None);
        tokio::join!(feeder, session);
    });
});
