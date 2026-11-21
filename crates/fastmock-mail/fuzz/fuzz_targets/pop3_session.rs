#![no_main]

use std::sync::Arc;
use std::time::Duration;

use fastmock_core::mail::{Envelope, LocalMailEvents, MailSource, MailStore, MemoryMailStore};
use fastmock_mail::mailbox_login::hash_password_with;
use fastmock_mail::pop3::{Pop3Server, command, render};
use fastmock_mail::{Incoming, MailConfig, MailDomains, MailService};
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MESSAGES: [&str; 3] = [
    "Subject: one\r\n\r\n.\r\n..two\r\nend\r\n",
    "Subject: bare\n\nlf only\nlast",
    "no header break",
];

fuzz_target!(|data: &[u8]| {
    let Some((&flags, input)) = data.split_first() else {
        return;
    };
    for line in input.split(|b| *b == b'\n') {
        let _ = command::parse(line);
    }
    let mut out = Vec::new();
    render::message(input, None, &mut out);
    let back = render::unstuff(&out).expect("rendered messages end with the dot line");
    assert_eq!(back.len() as u64, render::crlf_size(input));
    out.clear();
    render::message(input, Some(u32::from(flags >> 4)), &mut out);
    assert!(render::unstuff(&out).is_some_and(|top| top.len() <= back.len()));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    rt.block_on(async move {
        let cfg = MailConfig {
            max_line_bytes: 512,
            allow_plaintext_auth: flags & 1 != 0,
            idle_timeout: Duration::from_secs(5),
            session_timeout: Duration::from_secs(60),
            ..Default::default()
        };
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        let store = Arc::new(MemoryMailStore::new(16));
        let svc = Arc::new(MailService::new(
            Arc::new(cfg),
            store.clone(),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing".to_string()].into(),
        ));
        for raw in MESSAGES {
            svc.accept(Incoming::new(
                MailSource::Smtp,
                Envelope {
                    mail_from: "shop@example.com".into(),
                    rcpt_to: vec!["a@billing.example.test".into()],
                    ..Default::default()
                },
                raw.as_bytes().to_vec(),
            ))
            .await
            .unwrap();
        }
        store
            .set_mailbox_password(
                Some("billing"),
                "a@billing.example.test",
                Some(&hash_password_with("pw", 1)),
            )
            .await
            .unwrap();
        let mut srv = Pop3Server::new(svc, None, None);
        srv.failure_delay = Duration::from_millis(1);
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let mut input = input.to_vec();
        if flags & 2 != 0 {
            input.splice(
                0..0,
                b"USER a@billing.example.test\r\nPASS pw\r\n"
                    .iter()
                    .copied(),
            );
        }
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
        let session = srv.serve(server, "198.51.100.1:1110".parse().unwrap(), false, None);
        tokio::join!(feeder, session);
    });
});
