use std::path::Path;

use anyhow::{Context, anyhow, bail};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub(crate) fn read_certs(what: &str, path: &Path) -> anyhow::Result<Vec<u8>> {
    let data =
        std::fs::read(path).with_context(|| format!("{what} {}: cannot read", path.display()))?;
    let certs = CertificateDer::pem_slice_iter(&data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{what} {}: invalid PEM: {e}", path.display()))?;
    if certs.is_empty() {
        bail!("{what} {}: no PEM certificate found", path.display());
    }
    Ok(data)
}

pub(crate) fn read_key(what: &str, path: &Path) -> anyhow::Result<Vec<u8>> {
    let data =
        std::fs::read(path).with_context(|| format!("{what} {}: cannot read", path.display()))?;
    PrivateKeyDer::from_pem_slice(&data).map_err(|e| {
        anyhow!(
            "{what} {}: no usable private key (expected PKCS#8, PKCS#1 or SEC1 PEM): {e}",
            path.display()
        )
    })?;
    Ok(data)
}

pub(crate) fn tls_hint(message: &str, ca_setting: &str) -> Option<String> {
    if message.contains("UnknownIssuer") {
        Some(format!(
            "the server certificate is not signed by a trusted CA: set {ca_setting} to the CA that issued it"
        ))
    } else if message.contains("NotValidForName") {
        Some(
            "the server certificate does not name the host in the URL: connect using a name listed in the certificate"
                .into(),
        )
    } else if message.contains("Expired") {
        Some("the server certificate has expired".into())
    } else if message.contains("CertificateRequired") || message.contains("certificate required") {
        Some("the server requires a client certificate".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_name_the_setting() {
        let h = tls_hint("invalid peer certificate: UnknownIssuer", "X_CA").unwrap();
        assert!(h.contains("X_CA"));
        assert!(tls_hint("connection refused", "X_CA").is_none());
    }

    #[test]
    fn unreadable_and_empty_files_are_rejected() {
        let dir = std::env::temp_dir().join(format!("fm-pem-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.pem");
        let e = read_certs("CA", &missing).unwrap_err();
        assert!(format!("{e:#}").contains("cannot read"));
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "not a pem").unwrap();
        assert!(
            read_certs("CA", &empty)
                .unwrap_err()
                .to_string()
                .contains("no PEM certificate")
        );
        assert!(
            read_key("key", &empty)
                .unwrap_err()
                .to_string()
                .contains("no usable private key")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
