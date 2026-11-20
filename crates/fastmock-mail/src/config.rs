//! Mail settings (`FASTMOCK_MAIL_*`). The server crate reads them from the environment; this module holds
//! the values, their defaults and the checks that need no other setting.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use fastmock_core::mail::MailRetention;

/// Mail listeners; `None` is off (an empty variable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailListeners {
    /// `FASTMOCK_MAIL_SMTP_LISTEN`, default `0.0.0.0:2525`.
    pub smtp: Option<SocketAddr>,
    /// `FASTMOCK_MAIL_SMTPS_LISTEN` (implicit TLS), default off; `0.0.0.0:4650` is suggested.
    pub smtps: Option<SocketAddr>,
    /// `FASTMOCK_MAIL_IMAP_LISTEN`, default off; `0.0.0.0:1143` is suggested.
    pub imap: Option<SocketAddr>,
    /// `FASTMOCK_MAIL_IMAPS_LISTEN` (implicit TLS), default off; `0.0.0.0:1993` is suggested.
    pub imaps: Option<SocketAddr>,
    /// `FASTMOCK_MAIL_POP3_LISTEN`, default off; `0.0.0.0:1110` is suggested.
    pub pop3: Option<SocketAddr>,
    /// `FASTMOCK_MAIL_POP3S_LISTEN` (implicit TLS), default off; `0.0.0.0:1995` is suggested.
    pub pop3s: Option<SocketAddr>,
}

impl Default for MailListeners {
    fn default() -> Self {
        Self {
            smtp: Some(SocketAddr::from(([0, 0, 0, 0], 2525))),
            smtps: None,
            imap: None,
            imaps: None,
            pop3: None,
            pop3s: None,
        }
    }
}

impl MailListeners {
    /// The listeners that are on, with the name of their variable and whether they use implicit TLS.
    pub fn enabled(&self) -> Vec<(&'static str, SocketAddr, bool)> {
        [
            ("FASTMOCK_MAIL_SMTP_LISTEN", self.smtp, false),
            ("FASTMOCK_MAIL_SMTPS_LISTEN", self.smtps, true),
            ("FASTMOCK_MAIL_IMAP_LISTEN", self.imap, false),
            ("FASTMOCK_MAIL_IMAPS_LISTEN", self.imaps, true),
            ("FASTMOCK_MAIL_POP3_LISTEN", self.pop3, false),
            ("FASTMOCK_MAIL_POP3S_LISTEN", self.pop3s, true),
        ]
        .into_iter()
        .filter_map(|(n, a, tls)| a.map(|a| (n, a, tls)))
        .collect()
    }
}

/// Whether two listeners would compete for a port: same port, and the same address or an unspecified
/// address on one side. Port 0 (any free port) never conflicts.
pub fn addr_conflict(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0
        && a.port() == b.port()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// Certificate and key for SMTPS/IMAPS/POP3S and STARTTLS/STLS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailTlsFiles {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// Taken from `FASTMOCK_TLS_CERT_FILE`/`_KEY_FILE` because the mail variables are not set.
    pub from_server: bool,
}

/// A user of `FASTMOCK_MAIL_SMTP_USERS` (`user:sha256hex`).
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpUser {
    pub name: String,
    /// Lowercase hex SHA-256 of the password, hashed like admin tokens.
    pub sha256: String,
}

impl std::fmt::Debug for SmtpUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpUser")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Parses `FASTMOCK_MAIL_SMTP_USERS`: `user:sha256hex` entries separated by commas or new lines.
pub fn parse_smtp_users(raw: &str) -> Result<Vec<SmtpUser>, String> {
    let mut out: Vec<SmtpUser> = Vec::new();
    for item in raw
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let Some((name, hash)) = item.rsplit_once(':') else {
            return Err(format!(
                "FASTMOCK_MAIL_SMTP_USERS: an entry must look like user:sha256hex, got `{}`",
                item.split(':').next().unwrap_or_default()
            ));
        };
        let name = name.trim();
        let hash = hash.trim().to_ascii_lowercase();
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            return Err(
                "FASTMOCK_MAIL_SMTP_USERS: user names must be 1..256 printable characters".into(),
            );
        }
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "FASTMOCK_MAIL_SMTP_USERS: the password of `{name}` must be a SHA-256 in hex (64 characters)"
            ));
        }
        if out.iter().any(|u| u.name == name) {
            return Err(format!(
                "FASTMOCK_MAIL_SMTP_USERS: `{name}` is listed twice"
            ));
        }
        out.push(SmtpUser {
            name: name.to_string(),
            sha256: hash,
        });
    }
    Ok(out)
}

/// The mail module's settings; present in the server configuration only when
/// `FASTMOCK_MAIL_ENABLED=true`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailConfig {
    pub listeners: MailListeners,
    /// `FASTMOCK_MAIL_TLS_CERT_FILE` and `FASTMOCK_MAIL_TLS_KEY_FILE`, else the server's TLS files. With
    /// a certificate, STARTTLS (SMTP, IMAP) and STLS (POP3) are offered.
    pub tls: Option<MailTlsFiles>,
    /// `FASTMOCK_MAIL_HOSTNAME`: the name in the SMTP greeting and EHLO reply, default `localhost`.
    pub hostname: String,
    /// `FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH`: offer AUTH/LOGIN before TLS even when TLS is available.
    pub allow_plaintext_auth: bool,
    /// `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH`: SMTP clients must authenticate with a user of `smtp_users` or a
    /// managed token holding `mail:write`. Off: any credentials are accepted.
    pub smtp_require_auth: bool,
    /// `FASTMOCK_MAIL_SMTP_USERS`.
    pub smtp_users: Vec<SmtpUser>,
    /// `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`, default 10 MiB: advertised as SIZE and enforced while
    /// receiving; also the cap of injected and provider messages.
    pub max_message_bytes: usize,
    /// `FASTMOCK_MAIL_MAX_RECIPIENTS`, default 100 per message.
    pub max_recipients: usize,
    /// `FASTMOCK_MAIL_MAX_LINE_BYTES`, default 4096: longest command line (at least 512).
    pub max_line_bytes: usize,
    /// `FASTMOCK_MAIL_IDLE_TIMEOUT`, default 30s: wait for the next command (and for the first one
    /// after the greeting).
    pub idle_timeout: Duration,
    /// `FASTMOCK_MAIL_DATA_TIMEOUT`, default 60s: wait for the next chunk of DATA or BDAT.
    pub data_timeout: Duration,
    /// `FASTMOCK_MAIL_SESSION_TIMEOUT`, default 10m: longest SMTP, IMAP or POP3 session.
    pub session_timeout: Duration,
    /// `FASTMOCK_MAIL_MAX_CONNECTIONS`, default 256 over all mail listeners.
    pub max_connections: usize,
    /// `FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP`, default 32.
    pub max_connections_per_ip: usize,
    /// `FASTMOCK_MAIL_MAX_AUTH_FAILURES`, default 3; then the connection is closed (SMTP 421).
    pub max_auth_failures: u32,
    /// `FASTMOCK_MAIL_IMAP_IDLE_TIMEOUT`, default 30m: longest IMAP IDLE.
    pub imap_idle_timeout: Duration,
    /// `FASTMOCK_MAIL_MAX_MESSAGES` (default 1000 per space) and `FASTMOCK_MAIL_MAX_AGE` (default 7d).
    pub retention: MailRetention,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            listeners: MailListeners::default(),
            tls: None,
            hostname: "localhost".into(),
            allow_plaintext_auth: false,
            smtp_require_auth: false,
            smtp_users: vec![],
            max_message_bytes: 10 * 1024 * 1024,
            max_recipients: 100,
            max_line_bytes: 4096,
            idle_timeout: Duration::from_secs(30),
            data_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(600),
            max_connections: 256,
            max_connections_per_ip: 32,
            max_auth_failures: 3,
            imap_idle_timeout: Duration::from_secs(1800),
            retention: MailRetention {
                max_messages: 1000,
                max_age: Duration::from_secs(7 * 86_400),
            },
        }
    }
}

/// Largest accepted `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`.
pub const MAX_MESSAGE_BYTES_LIMIT: usize = 512 * 1024 * 1024;

impl MailConfig {
    /// Checks the values among themselves; the server checks them against its own listeners and reads
    /// the TLS files.
    pub fn validate(&self) -> Result<(), String> {
        let positive = [
            ("FASTMOCK_MAIL_MAX_MESSAGE_BYTES", self.max_message_bytes),
            ("FASTMOCK_MAIL_MAX_RECIPIENTS", self.max_recipients),
            ("FASTMOCK_MAIL_MAX_CONNECTIONS", self.max_connections),
            (
                "FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP",
                self.max_connections_per_ip,
            ),
            (
                "FASTMOCK_MAIL_MAX_AUTH_FAILURES",
                self.max_auth_failures as usize,
            ),
            ("FASTMOCK_MAIL_MAX_MESSAGES", self.retention.max_messages),
        ];
        for (name, v) in positive {
            if v == 0 {
                return Err(format!("{name} must be greater than zero"));
            }
        }
        for (name, d) in [
            ("FASTMOCK_MAIL_IDLE_TIMEOUT", self.idle_timeout),
            ("FASTMOCK_MAIL_DATA_TIMEOUT", self.data_timeout),
            ("FASTMOCK_MAIL_SESSION_TIMEOUT", self.session_timeout),
            ("FASTMOCK_MAIL_IMAP_IDLE_TIMEOUT", self.imap_idle_timeout),
            ("FASTMOCK_MAIL_MAX_AGE", self.retention.max_age),
        ] {
            if d.is_zero() {
                return Err(format!("{name} must be greater than zero"));
            }
        }
        if self.max_message_bytes > MAX_MESSAGE_BYTES_LIMIT {
            return Err(format!(
                "FASTMOCK_MAIL_MAX_MESSAGE_BYTES must be at most {MAX_MESSAGE_BYTES_LIMIT}"
            ));
        }
        if self.max_line_bytes < 512 {
            return Err("FASTMOCK_MAIL_MAX_LINE_BYTES must be at least 512 (RFC 5321)".into());
        }
        if self.max_connections_per_ip > self.max_connections {
            return Err(
                "FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP must not exceed FASTMOCK_MAIL_MAX_CONNECTIONS"
                    .into(),
            );
        }
        let host_ok = !self.hostname.is_empty()
            && self.hostname.len() <= 253
            && self
                .hostname
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
        if !host_ok {
            return Err("FASTMOCK_MAIL_HOSTNAME must be a host name".into());
        }
        let listeners = self.listeners.enabled();
        for (i, (na, a, tls)) in listeners.iter().enumerate() {
            if *tls && self.tls.is_none() {
                return Err(format!(
                    "{na} needs a certificate: set FASTMOCK_MAIL_TLS_CERT_FILE and FASTMOCK_MAIL_TLS_KEY_FILE (or FASTMOCK_TLS_CERT_FILE and FASTMOCK_TLS_KEY_FILE)"
                ));
            }
            if let Some((nb, b, _)) = listeners[i + 1..]
                .iter()
                .find(|(_, b, _)| addr_conflict(*a, *b))
            {
                return Err(format!("{na} ({a}) and {nb} ({b}) use the same port"));
            }
        }
        Ok(())
    }

    /// Entries for `features` of `/__admin/version`: `mail`, plus `mail.imap`, `mail.pop3` when their
    /// listeners are on, and `mail.providers`.
    pub fn features(&self) -> Vec<&'static str> {
        let l = &self.listeners;
        let mut v = vec!["mail"];
        if l.imap.is_some() || l.imaps.is_some() {
            v.push("mail.imap");
        }
        if l.pop3.is_some() || l.pop3s.is_some() {
            v.push("mail.pop3");
        }
        v.push("mail.providers");
        v
    }

    pub fn parse_limits(&self) -> crate::parse::ParseLimits {
        crate::parse::ParseLimits {
            max_bytes: self.max_message_bytes,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_design() {
        let c = MailConfig::default();
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            c.listeners.enabled(),
            vec![(
                "FASTMOCK_MAIL_SMTP_LISTEN",
                "0.0.0.0:2525".parse().unwrap(),
                false
            )]
        );
        assert_eq!(c.max_message_bytes, 10 * 1024 * 1024);
        assert_eq!(
            (
                c.max_recipients,
                c.max_connections,
                c.max_connections_per_ip
            ),
            (100, 256, 32)
        );
        assert_eq!(c.retention.max_messages, 1000);
        assert_eq!(c.retention.max_age, Duration::from_secs(604_800));
        assert_eq!(c.features(), vec!["mail", "mail.providers"]);
    }

    #[test]
    fn validation_names_the_variable() {
        let bad = |f: &dyn Fn(&mut MailConfig)| {
            let mut c = MailConfig::default();
            f(&mut c);
            c.validate().unwrap_err()
        };
        assert!(bad(&|c| c.max_recipients = 0).contains("FASTMOCK_MAIL_MAX_RECIPIENTS"));
        assert!(bad(&|c| c.retention.max_age = Duration::ZERO).contains("FASTMOCK_MAIL_MAX_AGE"));
        assert!(bad(&|c| c.max_line_bytes = 100).contains("FASTMOCK_MAIL_MAX_LINE_BYTES"));
        assert!(bad(&|c| c.max_message_bytes = usize::MAX).contains("at most"));
        assert!(bad(&|c| c.max_connections_per_ip = 1000).contains("PER_IP"));
        assert!(bad(&|c| c.hostname = "bad host".into()).contains("HOSTNAME"));
        assert!(
            bad(&|c| c.listeners.smtps = Some("0.0.0.0:4650".parse().unwrap()))
                .contains("FASTMOCK_MAIL_SMTPS_LISTEN needs a certificate")
        );
        assert!(
            bad(&|c| c.listeners.imap = Some("127.0.0.1:2525".parse().unwrap()))
                .contains("same port")
        );
        let mut c = MailConfig::default();
        c.listeners.imap = Some("127.0.0.1:1143".parse().unwrap());
        c.listeners.pop3s = Some("127.0.0.1:1995".parse().unwrap());
        c.tls = Some(MailTlsFiles {
            cert_file: "c.pem".into(),
            key_file: "k.pem".into(),
            from_server: false,
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            c.features(),
            vec!["mail", "mail.imap", "mail.pop3", "mail.providers"]
        );
    }

    #[test]
    fn conflicts_and_users() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert!(addr_conflict(a("0.0.0.0:25"), a("127.0.0.1:25")));
        assert!(addr_conflict(a("[::]:25"), a("127.0.0.1:25")));
        assert!(!addr_conflict(a("127.0.0.1:25"), a("127.0.0.2:25")));
        assert!(!addr_conflict(a("127.0.0.1:0"), a("127.0.0.1:0")));
        let hash = "a".repeat(64);
        let users = parse_smtp_users(&format!(
            "app:{hash},\n ops@example.com : {}",
            "B".repeat(64)
        ))
        .unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[1].name, "ops@example.com");
        assert_eq!(users[1].sha256, "b".repeat(64));
        assert!(!format!("{users:?}").contains(&hash));
        assert!(
            parse_smtp_users("app")
                .unwrap_err()
                .contains("user:sha256hex")
        );
        assert!(parse_smtp_users("app:xyz").unwrap_err().contains("SHA-256"));
        assert!(
            parse_smtp_users(&format!("app:{hash},app:{hash}"))
                .unwrap_err()
                .contains("twice")
        );
        assert!(parse_smtp_users("").unwrap().is_empty());
    }
}
