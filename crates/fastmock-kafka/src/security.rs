use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, bail};
use rskafka::client::{ClientBuilder, Credentials, SaslConfig};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

const REDACTED: &str = "<redacted>";

/// Kafka `security.protocol`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecurityProtocol {
    #[default]
    Plaintext,
    Ssl,
    SaslPlaintext,
    SaslSsl,
}

impl SecurityProtocol {
    pub fn uses_tls(self) -> bool {
        matches!(self, Self::Ssl | Self::SaslSsl)
    }

    pub fn uses_sasl(self) -> bool {
        matches!(self, Self::SaslPlaintext | Self::SaslSsl)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Ssl => "ssl",
            Self::SaslPlaintext => "sasl_plaintext",
            Self::SaslSsl => "sasl_ssl",
        }
    }
}

impl fmt::Display for SecurityProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SecurityProtocol {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "plaintext" => Self::Plaintext,
            "ssl" => Self::Ssl,
            "sasl_plaintext" => Self::SaslPlaintext,
            "sasl_ssl" => Self::SaslSsl,
            _ => bail!(
                "unknown security protocol `{}`; expected plaintext, ssl, sasl_plaintext or sasl_ssl",
                s.trim()
            ),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaslMechanism {
    Plain,
    ScramSha256,
    ScramSha512,
}

impl SaslMechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "PLAIN",
            Self::ScramSha256 => "SCRAM-SHA-256",
            Self::ScramSha512 => "SCRAM-SHA-512",
        }
    }
}

impl fmt::Display for SaslMechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SaslMechanism {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_ascii_uppercase().as_str() {
            "PLAIN" => Self::Plain,
            "SCRAM-SHA-256" => Self::ScramSha256,
            "SCRAM-SHA-512" => Self::ScramSha512,
            _ => bail!(
                "unknown SASL mechanism `{}`; expected PLAIN, SCRAM-SHA-256 or SCRAM-SHA-512",
                s.trim()
            ),
        })
    }
}

/// SASL credentials. `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct SaslSettings {
    pub mechanism: SaslMechanism,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for SaslSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaslSettings")
            .field("mechanism", &self.mechanism)
            .field("username", &self.username)
            .field("password", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCertFiles {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

/// PEM files for TLS. Server certificates are verified against the Mozilla roots
/// (`webpki-roots`) plus `ca_file`; `client_cert` enables mTLS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsFiles {
    pub ca_file: Option<PathBuf>,
    pub client_cert: Option<ClientCertFiles>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KafkaSecurity {
    pub protocol: SecurityProtocol,
    pub sasl: Option<SaslSettings>,
    pub tls: TlsFiles,
}

impl KafkaSecurity {
    /// Checks that the settings fit the protocol and that the PEM files load.
    pub fn validate(&self) -> anyhow::Result<()> {
        let p = self.protocol;
        match (&self.sasl, p.uses_sasl()) {
            (None, true) => bail!("security protocol {p} needs SASL credentials"),
            (Some(_), false) => bail!("SASL credentials are set but the security protocol is {p}"),
            (Some(s), true) => {
                if s.username.is_empty() {
                    bail!("SASL user name is empty");
                }
                if s.password.is_empty() {
                    bail!("SASL password is empty");
                }
            }
            (None, false) => {}
        }
        if !p.uses_tls() && self.tls != TlsFiles::default() {
            bail!("TLS files are set but the security protocol is {p}");
        }
        self.tls_config()?;
        Ok(())
    }

    /// The rustls configuration for `ssl` and `sasl_ssl`, read from the files on every call.
    pub fn tls_config(&self) -> anyhow::Result<Option<Arc<rustls::ClientConfig>>> {
        if !self.protocol.uses_tls() {
            return Ok(None);
        }
        client_tls_config(&self.tls).map(Some)
    }

    pub fn sasl_config(&self) -> anyhow::Result<Option<SaslConfig>> {
        if !self.protocol.uses_sasl() {
            return Ok(None);
        }
        let s = self.sasl.as_ref().with_context(|| {
            format!("security protocol {} needs SASL credentials", self.protocol)
        })?;
        let c = Credentials::new(s.username.clone(), s.password.clone());
        Ok(Some(match s.mechanism {
            SaslMechanism::Plain => SaslConfig::Plain(c),
            SaslMechanism::ScramSha256 => SaslConfig::ScramSha256(c),
            SaslMechanism::ScramSha512 => SaslConfig::ScramSha512(c),
        }))
    }

    pub fn apply(&self, mut b: ClientBuilder) -> anyhow::Result<ClientBuilder> {
        if let Some(tls) = self.tls_config()? {
            b = b.tls_config(tls);
        }
        if let Some(sasl) = self.sasl_config()? {
            b = b.sasl_config(sasl);
        }
        Ok(b)
    }
}

pub fn read_ca_bundle(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading {}", path.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", path.display()))?;
    if certs.is_empty() {
        bail!("{} contains no PEM certificates", path.display());
    }
    Ok(certs)
}

pub fn read_client_cert(
    files: &ClientCertFiles,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs = read_ca_bundle(&files.cert_file)?;
    let key = PrivateKeyDer::from_pem_file(&files.key_file).map_err(|e| {
        anyhow::anyhow!(
            "{} contains no usable PEM private key ({e})",
            files.key_file.display()
        )
    })?;
    Ok((certs, key))
}

pub fn client_tls_config(tls: &TlsFiles) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(ca) = &tls.ca_file {
        for c in read_ca_bundle(ca)? {
            roots
                .add(c)
                .with_context(|| format!("{} contains an invalid CA certificate", ca.display()))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots);
    let cfg = match &tls.client_cert {
        Some(files) => {
            let (certs, key) = read_client_cert(files)?;
            builder.with_client_auth_cert(certs, key).with_context(|| {
                format!(
                    "client certificate {} does not fit key {}",
                    files.cert_file.display(),
                    files.key_file.display()
                )
            })?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(cfg))
}

/// HTTP basic authentication for the Schema Registry. `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasicAuth")
            .field("username", &self.username)
            .field("password", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryOptions {
    pub ca_file: Option<PathBuf>,
    pub auth: Option<BasicAuth>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Pki {
        dir: PathBuf,
    }

    impl Pki {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fastmock-kafka-{name}-{}-{}",
                std::process::id(),
                rand::random::<u32>()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let mut ca = rcgen::CertificateParams::new(vec![]).unwrap();
            ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let ca_cert = ca.self_signed(&ca_key).unwrap();
            let client_key = rcgen::KeyPair::generate().unwrap();
            let issuer = rcgen::Issuer::new(ca, ca_key);
            let client = rcgen::CertificateParams::new(vec!["client".into()])
                .unwrap()
                .signed_by(&client_key, &issuer)
                .unwrap();
            let other_key = rcgen::KeyPair::generate().unwrap();
            std::fs::write(dir.join("ca.pem"), ca_cert.pem()).unwrap();
            std::fs::write(dir.join("client.pem"), client.pem()).unwrap();
            std::fs::write(dir.join("client.key"), client_key.serialize_pem()).unwrap();
            std::fs::write(dir.join("other.key"), other_key.serialize_pem()).unwrap();
            std::fs::write(dir.join("empty.pem"), "").unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for Pki {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sasl() -> SaslSettings {
        SaslSettings {
            mechanism: SaslMechanism::ScramSha512,
            username: "fastmock".into(),
            password: "s3cr3t-pa55".into(),
        }
    }

    #[test]
    fn names_parse_case_insensitively() {
        assert_eq!(
            "SASL_SSL".parse::<SecurityProtocol>().unwrap(),
            SecurityProtocol::SaslSsl
        );
        assert_eq!(
            "sasl_plaintext".parse::<SecurityProtocol>().unwrap(),
            SecurityProtocol::SaslPlaintext
        );
        assert_eq!(
            " Ssl ".parse::<SecurityProtocol>().unwrap(),
            SecurityProtocol::Ssl
        );
        assert!("tls".parse::<SecurityProtocol>().is_err());
        assert_eq!(
            "scram-sha-256".parse::<SaslMechanism>().unwrap(),
            SaslMechanism::ScramSha256
        );
        assert_eq!(
            "plain".parse::<SaslMechanism>().unwrap(),
            SaslMechanism::Plain
        );
        assert!("GSSAPI".parse::<SaslMechanism>().is_err());
    }

    #[test]
    fn debug_redacts_passwords() {
        let s = KafkaSecurity {
            protocol: SecurityProtocol::SaslSsl,
            sasl: Some(sasl()),
            tls: TlsFiles::default(),
        };
        let d = format!("{s:?} {:?}", s.sasl_config().unwrap());
        let mine = format!("{s:?}");
        assert!(!mine.contains("s3cr3t"), "{mine}");
        assert!(mine.contains("<redacted>") && mine.contains("fastmock"));
        assert!(d.contains("ScramSha512"));
        let b = format!(
            "{:?}",
            RegistryOptions {
                ca_file: None,
                auth: Some(BasicAuth {
                    username: "sr".into(),
                    password: "hunter2-hunter2".into()
                })
            }
        );
        assert!(!b.contains("hunter2") && b.contains("<redacted>"), "{b}");
    }

    #[test]
    fn settings_must_fit_the_protocol() {
        let pki = Pki::new("fit");
        let plain = KafkaSecurity::default();
        assert!(plain.validate().is_ok());
        assert!(plain.tls_config().unwrap().is_none());
        assert!(plain.sasl_config().unwrap().is_none());

        let e = KafkaSecurity {
            protocol: SecurityProtocol::SaslPlaintext,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(e.to_string().contains("needs SASL"), "{e}");

        let e = KafkaSecurity {
            sasl: Some(sasl()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(e.to_string().contains("plaintext"), "{e}");
        assert!(!format!("{e:#}").contains("s3cr3t"));

        let e = KafkaSecurity {
            protocol: SecurityProtocol::SaslPlaintext,
            tls: TlsFiles {
                ca_file: Some(pki.path("ca.pem")),
                client_cert: None,
            },
            sasl: Some(sasl()),
        }
        .validate()
        .unwrap_err();
        assert!(e.to_string().contains("TLS files"), "{e}");

        let mut empty_pw = sasl();
        empty_pw.password.clear();
        assert!(
            KafkaSecurity {
                protocol: SecurityProtocol::SaslPlaintext,
                sasl: Some(empty_pw),
                ..Default::default()
            }
            .validate()
            .is_err()
        );

        let ok = KafkaSecurity {
            protocol: SecurityProtocol::SaslSsl,
            sasl: Some(sasl()),
            tls: TlsFiles {
                ca_file: Some(pki.path("ca.pem")),
                client_cert: Some(ClientCertFiles {
                    cert_file: pki.path("client.pem"),
                    key_file: pki.path("client.key"),
                }),
            },
        };
        ok.validate().unwrap();
        assert!(
            ok.tls_config()
                .unwrap()
                .unwrap()
                .client_auth_cert_resolver
                .has_certs()
        );
        assert!(ok.sasl_config().unwrap().is_some());
        assert!(
            ok.apply(ClientBuilder::new(vec!["198.51.100.1:9093".into()]))
                .is_ok()
        );
        let webpki_only = KafkaSecurity {
            protocol: SecurityProtocol::Ssl,
            ..Default::default()
        };
        webpki_only.validate().unwrap();
    }

    #[test]
    fn pem_files_are_checked() {
        let pki = Pki::new("pem");
        let with = |ca: Option<&str>, cert: Option<(&str, &str)>| {
            KafkaSecurity {
                protocol: SecurityProtocol::Ssl,
                sasl: None,
                tls: TlsFiles {
                    ca_file: ca.map(|c| pki.path(c)),
                    client_cert: cert.map(|(c, k)| ClientCertFiles {
                        cert_file: pki.path(c),
                        key_file: pki.path(k),
                    }),
                },
            }
            .validate()
            .map_err(|e| format!("{e:#}"))
        };
        assert!(with(Some("ca.pem"), None).is_ok());
        let e = with(Some("missing.pem"), None).unwrap_err();
        assert!(e.contains("missing.pem"), "{e}");
        let e = with(Some("empty.pem"), None).unwrap_err();
        assert!(e.contains("no PEM certificates"), "{e}");
        let e = with(Some("client.key"), None).unwrap_err();
        assert!(e.contains("no PEM certificates"), "{e}");
        let e = with(None, Some(("client.pem", "empty.pem"))).unwrap_err();
        assert!(e.contains("no usable PEM private key"), "{e}");
        let e = with(None, Some(("empty.pem", "client.key"))).unwrap_err();
        assert!(e.contains("no PEM certificates"), "{e}");
        let e = with(None, Some(("client.pem", "other.key"))).unwrap_err();
        assert!(e.contains("does not fit"), "{e}");
        assert!(with(None, Some(("client.pem", "client.key"))).is_ok());
    }
}
