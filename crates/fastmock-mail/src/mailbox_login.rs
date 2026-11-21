//! Mailbox logins shared by IMAP and POP3: the user name is the mailbox address (lowercase, plus tag
//! removed), its area is the space whose `mailDomains` own the domain, else the global area. The password
//! is a managed FastMock token allowed `mail:read` in that area (or globally), or the mailbox password set
//! through the admin API, stored as a salted PBKDF2-HMAC-SHA256 hash in the form
//! `pbkdf2-sha256$<iterations>$<salt>$<hash>` (base64 without padding). The admin API writes these hashes
//! with [`hash_password`].

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};

use crate::address::{domain_of, mailbox_of, split};
use crate::service::MailService;

/// The hash scheme of mailbox passwords.
pub const SCHEME: &str = "pbkdf2-sha256";
/// Iterations of new hashes.
pub const ITERATIONS: u32 = 100_000;
/// Longest accepted password (and token).
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// Longest accepted user name.
pub const MAX_USER_BYTES: usize = 320;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const MAX_ITERATIONS: u32 = 10_000_000;

/// Checks a managed FastMock token given as an IMAP/POP3 password; the server implements it.
#[async_trait]
pub trait MailboxTokenCheck: Send + Sync {
    /// Whether `token` is a valid managed token allowed `mail:read` in `area` (a space; `None` is the
    /// global area). A grant in the global area covers every space.
    async fn can_read(&self, area: Option<&str>, token: &str) -> bool;
}

/// A new salted hash of `password`.
pub fn hash_password(password: &str) -> String {
    hash_password_with(password, ITERATIONS)
}

/// A salted hash with a chosen number of iterations (tests use few).
pub fn hash_password_with(password: &str, iterations: u32) -> String {
    let mut salt = [0u8; SALT_BYTES];
    SystemRandom::new()
        .fill(&mut salt)
        .expect("the system random generator works");
    let mut out = [0u8; HASH_BYTES];
    let n = NonZeroU32::new(iterations).unwrap_or(NonZeroU32::MIN);
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        n,
        &salt,
        password.as_bytes(),
        &mut out,
    );
    format!("{SCHEME}${}${}${}", n, B64.encode(salt), B64.encode(out))
}

/// Whether `password` matches a stored hash (compared in constant time). Malformed hashes and passwords
/// above [`MAX_PASSWORD_BYTES`] never match.
pub fn verify_password(stored: &str, password: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let mut it = stored.split('$');
    let (Some(SCHEME), Some(n), Some(salt), Some(want), None) =
        (it.next(), it.next(), it.next(), it.next(), it.next())
    else {
        return false;
    };
    let Some(n) = n
        .parse::<u32>()
        .ok()
        .filter(|n| *n <= MAX_ITERATIONS)
        .and_then(NonZeroU32::new)
    else {
        return false;
    };
    let (Ok(salt), Ok(want)) = (B64.decode(salt), B64.decode(want)) else {
        return false;
    };
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        n,
        &salt,
        password.as_bytes(),
        &want,
    )
    .is_ok()
}

/// A hash checked when a mailbox has no password, so that unknown mailboxes take as long to refuse.
fn dummy_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| hash_password(""))
}

/// A mailbox a login opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginOk {
    /// The space owning the address's domain, `None` for the global area.
    pub area: Option<String>,
    /// The mailbox address (lowercase, without plus tag).
    pub address: String,
    /// The password was a managed token (else the mailbox password).
    pub by_token: bool,
}

/// Checks mailbox logins against managed tokens and mailbox passwords.
#[derive(Clone)]
pub struct MailboxLogin {
    pub service: Arc<MailService>,
    pub tokens: Option<Arc<dyn MailboxTokenCheck>>,
}

impl MailboxLogin {
    pub fn new(service: Arc<MailService>, tokens: Option<Arc<dyn MailboxTokenCheck>>) -> Self {
        Self { service, tokens }
    }

    /// The area of a mailbox address: the space owning its domain, else the global area.
    pub fn area_of(&self, address: &str) -> Option<String> {
        domain_of(address).and_then(|d| self.service.domains.owner(&d).map(String::from))
    }

    /// The mailbox address of a login name: an address with a local part and a domain, printable,
    /// without spaces, at most [`MAX_USER_BYTES`]; `None` otherwise.
    pub fn address_of(user: &str) -> Option<String> {
        let u = user.trim();
        let ok = !u.is_empty()
            && u.len() <= MAX_USER_BYTES
            && !u
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | ','))
            && split(u).is_some();
        ok.then(|| mailbox_of(u))
    }

    /// Checks a login; `None` for every kind of failure (unknown user, wrong password, store error), so
    /// callers cannot tell them apart.
    pub async fn check(&self, user: &str, password: &str) -> Option<LoginOk> {
        let address = Self::address_of(user)?;
        if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
            return None;
        }
        let area = self.area_of(&address);
        if let Some(t) = &self.tokens
            && t.can_read(area.as_deref(), password).await
        {
            return Some(LoginOk {
                area,
                address,
                by_token: true,
            });
        }
        let stored = match self
            .service
            .store
            .mailbox_password(area.as_deref(), &address)
            .await
        {
            Ok(Some(h)) => h,
            Ok(None) => {
                tokio::task::spawn_blocking(|| verify_password(dummy_hash(), "-"))
                    .await
                    .ok();
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading a mailbox password failed");
                return None;
            }
        };
        let pw = password.to_string();
        let ok = tokio::task::spawn_blocking(move || verify_password(&stored, &pw))
            .await
            .unwrap_or(false);
        ok.then_some(LoginOk {
            area,
            address,
            by_token: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_differ() {
        let a = hash_password_with("s3cret", 1000);
        let b = hash_password_with("s3cret", 1000);
        assert_ne!(a, b);
        assert!(a.starts_with("pbkdf2-sha256$1000$"));
        assert!(verify_password(&a, "s3cret"));
        assert!(!verify_password(&a, "s3cret "));
        assert!(!verify_password(&a, ""));
        assert!(!verify_password("plain", "plain"));
        assert!(!verify_password("pbkdf2-sha256$0$AA$AA", "x"));
        assert!(!verify_password("pbkdf2-sha256$99999999999$AA$AA", "x"));
        assert!(!verify_password("pbkdf2-sha256$1000$!!$AA", "x"));
        assert!(!verify_password(&format!("{a}$extra"), "s3cret"));
        assert!(!verify_password(&a, &"x".repeat(MAX_PASSWORD_BYTES + 1)));
        let full = hash_password("pw");
        assert!(full.starts_with(&format!("pbkdf2-sha256${ITERATIONS}$")));
        assert!(verify_password(&full, "pw"));
    }

    #[test]
    fn login_names() {
        assert_eq!(
            MailboxLogin::address_of(" Alice+News@Billing.Example.TEST ").as_deref(),
            Some("alice@billing.example.test")
        );
        assert_eq!(MailboxLogin::address_of("alice"), None);
        assert_eq!(MailboxLogin::address_of("@example.test"), None);
        assert_eq!(MailboxLogin::address_of("a b@example.test"), None);
        assert_eq!(MailboxLogin::address_of("<a@example.test>"), None);
        assert_eq!(MailboxLogin::address_of("a\u{7}@example.test"), None);
        assert_eq!(
            MailboxLogin::address_of(&format!("{}@example.test", "a".repeat(400))),
            None
        );
    }

    #[test]
    fn hashes_follow_the_rfc_7914_vector() {
        let want = hex::decode("55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc")
            .unwrap();
        let stored = format!(
            "pbkdf2-sha256$1${}${}",
            B64.encode(b"salt"),
            B64.encode(want)
        );
        assert!(verify_password(&stored, "passwd"));
        assert!(!verify_password(&stored, "Passwd"));
    }

    struct Tokens;

    #[async_trait]
    impl MailboxTokenCheck for Tokens {
        async fn can_read(&self, area: Option<&str>, token: &str) -> bool {
            token == format!("tok-{}", area.unwrap_or("global"))
        }
    }

    fn login(tokens: bool) -> MailboxLogin {
        use crate::config::MailConfig;
        use crate::routing::MailDomains;
        use fastmock_core::mail::{LocalMailEvents, MemoryMailStore};
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        domains.add("payments", "payments.example.test");
        let service = MailService::new(
            Arc::new(MailConfig::default()),
            Arc::new(MemoryMailStore::new(10)),
            Arc::new(LocalMailEvents::new(4)),
            domains,
            ["billing", "payments"].map(String::from).into(),
        );
        let t: Option<Arc<dyn MailboxTokenCheck>> = tokens.then(|| Arc::new(Tokens) as _);
        MailboxLogin::new(Arc::new(service), t)
    }

    #[test]
    fn areas_follow_mail_domains() {
        let l = login(false);
        assert_eq!(
            l.area_of("alice@billing.example.test").as_deref(),
            Some("billing")
        );
        assert_eq!(l.area_of("x@other.example"), None);
    }

    #[tokio::test]
    async fn tokens_and_passwords() {
        let l = login(true);
        let ok = l
            .check("a@billing.example.test", "tok-billing")
            .await
            .unwrap();
        assert_eq!(ok.area.as_deref(), Some("billing"));
        assert!(ok.by_token);
        assert!(
            l.check("a@payments.example.test", "tok-billing")
                .await
                .is_none(),
            "a billing token cannot open a payments mailbox"
        );
        assert!(l.check("a@x.example", "tok-global").await.is_some());
        assert!(
            login(false)
                .check("a@billing.example.test", "tok-billing")
                .await
                .is_none()
        );
        l.service
            .store
            .set_mailbox_password(
                Some("payments"),
                "p@payments.example.test",
                Some(&hash_password_with("secret", 1000)),
            )
            .await
            .unwrap();
        for (user, pw, ok) in [
            ("P+x@payments.example.test", "secret", true),
            ("p@payments.example.test", "wrong", false),
            ("p@payments.example.test", "", false),
            ("q@payments.example.test", "secret", false),
            ("not-an-address", "secret", false),
        ] {
            let got = l.check(user, pw).await;
            assert_eq!(got.is_some(), ok, "{user}");
            if let Some(got) = got {
                assert_eq!(got.address, "p@payments.example.test");
                assert!(!got.by_token);
            }
        }
    }
}
