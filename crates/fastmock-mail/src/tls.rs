//! The rustls server configuration of the mail listeners (SMTPS, IMAPS, POP3S, STARTTLS, STLS).

use std::sync::Arc;

use anyhow::Context;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::MailTlsFiles;

/// Reads the certificate chain and key; fails with the file name when one is unreadable or invalid.
pub fn server_config(files: &MailTlsFiles) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&files.cert_file)
        .with_context(|| format!("reading {}", files.cert_file.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", files.cert_file.display()))?;
    if certs.is_empty() {
        anyhow::bail!("{} contains no certificates", files.cert_file.display());
    }
    let key = PrivateKeyDer::from_pem_file(&files.key_file)
        .with_context(|| format!("reading {}", files.key_file.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the mail certificate and key do not match")?;
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_pair_and_reports_bad_files() {
        let dir = std::env::temp_dir().join(format!("fm-mail-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["mail.example.test".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        std::fs::write(dir.join("c.pem"), cert.pem()).unwrap();
        std::fs::write(dir.join("k.pem"), key.serialize_pem()).unwrap();
        std::fs::write(dir.join("empty.pem"), "\n").unwrap();
        let files = |c: &str, k: &str| MailTlsFiles {
            cert_file: dir.join(c),
            key_file: dir.join(k),
            from_server: false,
        };
        assert!(server_config(&files("c.pem", "k.pem")).is_ok());
        let e = format!(
            "{:#}",
            server_config(&files("missing.pem", "k.pem")).unwrap_err()
        );
        assert!(e.contains("missing.pem"), "{e}");
        let e = format!(
            "{:#}",
            server_config(&files("empty.pem", "k.pem")).unwrap_err()
        );
        assert!(e.contains("no certificates"), "{e}");
        let e = format!(
            "{:#}",
            server_config(&files("c.pem", "empty.pem")).unwrap_err()
        );
        assert!(e.contains("empty.pem"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
