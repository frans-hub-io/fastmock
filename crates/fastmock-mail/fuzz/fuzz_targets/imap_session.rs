#![no_main]

use std::sync::Arc;
use std::time::Duration;

use fastmock_core::mail::{Envelope, LocalMailEvents, MailSource, MemoryMailStore};
use fastmock_mail::imap::ImapServer;
use fastmock_mail::{Incoming, MailConfig, MailDomains, MailService};
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MESSAGE: &[u8] = b"From: Shop <shop@example.com>\r\nTo: a@billing.example.test\r\nSubject: =?utf-8?q?Gr=C3=BC=C3=9Fe?=\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhello\r\n--b\r\nContent-Type: message/rfc822\r\n\r\nSubject: inner\r\n\r\ninner body\r\n--b--\r\n";

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
        let cfg = MailConfig {
            max_message_bytes: 4096,
            max_line_bytes: 512,
            idle_timeout: Duration::from_secs(5),
            data_timeout: Duration::from_secs(5),
            session_timeout: Duration::from_secs(60),
            imap_idle_timeout: Duration::from_secs(10),
            allow_plaintext_auth: true,
            ..Default::default()
        };
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        let svc = Arc::new(MailService::new(
            Arc::new(cfg),
            Arc::new(MemoryMailStore::new(16)),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing".to_string()].into(),
        ));
        svc.store
            .set_mailbox_password(
                Some("billing"),
                "a@billing.example.test",
                Some(&fastmock_mail::mailbox_login::hash_password_with("secret", 1)),
            )
            .await
            .unwrap();
        for _ in 0..(flags & 3) {
            let _ = svc
                .accept(Incoming::new(
                    MailSource::Smtp,
                    Envelope {
                        rcpt_to: vec!["a@billing.example.test".into()],
                        ..Default::default()
                    },
                    MESSAGE.to_vec(),
                ))
                .await;
        }
        let srv = ImapServer::new(svc, None, None);
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let mut script = Vec::new();
        if flags & 4 != 0 {
            script.extend_from_slice(b"x LOGIN a@billing.example.test secret\r\n");
            if flags & 8 != 0 {
                script.extend_from_slice(b"y SELECT INBOX\r\n");
            }
        }
        script.extend_from_slice(input);
        let feeder = async move {
            for chunk in script.chunks(97) {
                if client.write_all(chunk).await.is_err() {
                    return;
                }
            }
            let _ = client.shutdown().await;
            let mut sink = Vec::new();
            let _ = client.read_to_end(&mut sink).await;
        };
        let session = srv.serve(server, "198.51.100.1:1143".parse().unwrap(), false, None);
        tokio::join!(feeder, session);
    });
});
