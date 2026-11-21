//! Mailbox logins shared by IMAP and POP3: the user name is the mailbox address (lowercase, plus tag
//! removed), its area is the space whose `mailDomains` own the domain, else the global area. An address
//! no space's `mailDomains` own can name its area explicitly: `<space>/<address>`, or
//! `_global/<address>` for the global area. The password is a managed FastMock token allowed `mail:read`
//! in that area (or globally), or the mailbox password set through the admin API, stored as a salted
//! PBKDF2-HMAC-SHA256 hash in the form `pbkdf2-sha256$<iterations>$<salt>$<hash>` (base64 without
//! padding). The admin API writes these hashes with [`hash_password`]. A token session may change the
//! mailbox only with `mail:write` (and expunge with `mail:write` or `mail:delete`); a mailbox password
//! opens the mailbox with full access. Failed logins are counted by [`crate::auth_guard::AuthGuard`].

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};

use crate::address::{domain_of, mailbox_of, split};
use crate::service::MailService;

/// The user-name prefix of the global area (`_global/<address>`).
pub const GLOBAL_PREFIX: &str = "_global";

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

/// What a mailbox session may change besides reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxAccess {
    /// `mail:write`: APPEND, flags, COPY/MOVE, folders, expunge, POP3 DELE.
    pub write: bool,
    /// `mail:delete`: expunge and POP3 DELE.
    pub delete: bool,
}

impl MailboxAccess {
    pub const FULL: Self = Self {
        write: true,
        delete: true,
    };
    pub const READ_ONLY: Self = Self {
        write: false,
        delete: false,
    };

    /// Whether entries may be expunged (IMAP EXPUNGE, POP3 DELE).
    pub fn can_expunge(&self) -> bool {
        self.write || self.delete
    }
}

/// Checks a managed FastMock token given as an IMAP/POP3 password; the server implements it.
#[async_trait]
pub trait MailboxTokenCheck: Send + Sync {
    /// `Some` when `token` is a valid managed token allowed `mail:read` in `area` (a space; `None` is
    /// the global area), with what it may change there. A grant in the global area covers every space.
    async fn access(&self, area: Option<&str>, token: &str) -> Option<MailboxAccess>;
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

/// Whether `stored` is a well-formed `pbkdf2-sha256$<iterations>$<salt>$<hash>`.
pub fn hash_is_valid(stored: &str) -> bool {
    let mut it = stored.split('$');
    let (Some(SCHEME), Some(n), Some(salt), Some(want), None) =
        (it.next(), it.next(), it.next(), it.next(), it.next())
    else {
        return false;
    };
    n.parse::<u32>()
        .is_ok_and(|n| (1..=MAX_ITERATIONS).contains(&n))
        && B64.decode(salt).is_ok_and(|s| !s.is_empty())
        && B64.decode(want).is_ok_and(|w| w.len() == HASH_BYTES)
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
    /// What the session may change.
    pub access: MailboxAccess,
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

    /// The area and mailbox address of a login name: `<address>` (area from `mailDomains`), or
    /// `<space>/<address>` and `_global/<address>` for an address no space's `mailDomains` own. `None`
    /// for anything else (unknown space, or a prefix that differs from the owning space).
    pub fn resolve(&self, user: &str) -> Option<(Option<String>, String)> {
        let u = user.trim();
        let (prefix, rest) = match u.split_once('/') {
            Some((p, r)) => (Some(p), r),
            None => (None, u),
        };
        let address = Self::address_of(rest)?;
        let owner = self.area_of(&address);
        match prefix {
            None => Some((owner, address)),
            Some(_) if owner.is_some() => None,
            Some(GLOBAL_PREFIX) => Some((None, address)),
            Some(p) if self.service.spaces.contains(p) => Some((Some(p.to_string()), address)),
            Some(_) => None,
        }
    }

    /// The mailbox address of a login name: an address with a local part and a domain, printable,
    /// without spaces, at most [`MAX_USER_BYTES`]; `None` otherwise.
    pub fn address_of(user: &str) -> Option<String> {
        let u = user.trim();
        let ok = !u.is_empty()
            && u.len() <= MAX_USER_BYTES
            && !u
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | ',' | '/'))
            && split(u).is_some();
        ok.then(|| mailbox_of(u))
    }

    /// Checks a login from `peer`; `None` for every kind of failure (unknown user, wrong password,
    /// blocked client or user, store error), so callers cannot tell them apart. Blocked clients and
    /// users are refused without checking the password; failures and successes are counted by
    /// [`MailService::auth`].
    pub async fn check(&self, peer: IpAddr, user: &str, password: &str) -> Option<LoginOk> {
        let guard = &self.service.auth;
        if guard.blocked(peer, user).await {
            return None;
        }
        let ok = self.verify(user, password).await;
        match &ok {
            Some(_) => guard.succeeded(user).await,
            None => guard.failed(peer, user).await,
        }
        ok
    }

    async fn verify(&self, user: &str, password: &str) -> Option<LoginOk> {
        if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
            return None;
        }
        let (area, address) = self.resolve(user)?;
        if let Some(t) = &self.tokens
            && let Some(access) = t.access(area.as_deref(), password).await
        {
            return Some(LoginOk {
                area,
                address,
                by_token: true,
                access,
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
                self.service
                    .auth
                    .hash(|| verify_password(dummy_hash(), "-"))
                    .await;
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading a mailbox password failed");
                return None;
            }
        };
        let pw = password.to_string();
        let ok = self
            .service
            .auth
            .hash(move || verify_password(&stored, &pw))
            .await
            .unwrap_or(false);
        ok.then_some(LoginOk {
            area,
            address,
            by_token: false,
            access: MailboxAccess::FULL,
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
        async fn access(&self, area: Option<&str>, token: &str) -> Option<MailboxAccess> {
            (token == format!("tok-{}", area.unwrap_or("global"))).then_some(MailboxAccess::FULL)
        }
    }

    const PEER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7));

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
            .check(PEER, "a@billing.example.test", "tok-billing")
            .await
            .unwrap();
        assert_eq!(ok.area.as_deref(), Some("billing"));
        assert!(ok.by_token);
        assert!(
            l.check(PEER, "a@payments.example.test", "tok-billing")
                .await
                .is_none(),
            "a billing token cannot open a payments mailbox"
        );
        assert!(l.check(PEER, "a@x.example", "tok-global").await.is_some());
        assert!(
            login(false)
                .check(PEER, "a@billing.example.test", "tok-billing")
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
            let got = l.check(PEER, user, pw).await;
            assert_eq!(got.is_some(), ok, "{user}");
            if let Some(got) = got {
                assert_eq!(got.address, "p@payments.example.test");
                assert!(!got.by_token);
            }
        }
    }

    #[tokio::test]
    async fn repeated_failures_block_the_client_and_the_mailbox() {
        let l = login(true);
        l.service
            .store
            .set_mailbox_password(
                Some("billing"),
                "p@billing.example.test",
                Some(&hash_password_with("secret", 1000)),
            )
            .await
            .unwrap();
        for _ in 0..5 {
            assert!(
                l.check(PEER, "p@billing.example.test", "wrong")
                    .await
                    .is_none()
            );
        }
        assert!(
            l.check(PEER, "p@billing.example.test", "secret")
                .await
                .is_none(),
            "blocked: even the right password is refused"
        );
        let other: IpAddr = "198.51.100.9".parse().unwrap();
        assert!(
            l.check(other, "p@billing.example.test", "secret")
                .await
                .is_none(),
            "the mailbox is blocked for every client"
        );
        assert!(
            l.check(other, "q@billing.example.test", "tok-billing")
                .await
                .is_some(),
            "other clients and mailboxes are not"
        );
    }

    #[tokio::test]
    async fn unowned_addresses_name_their_area() {
        let l = login(true);
        assert_eq!(
            l.resolve("billing/x@other.example"),
            Some((Some("billing".into()), "x@other.example".into()))
        );
        assert_eq!(
            l.resolve("_global/x@other.example"),
            Some((None, "x@other.example".into()))
        );
        assert_eq!(
            l.resolve("x@other.example"),
            Some((None, "x@other.example".into()))
        );
        assert_eq!(l.resolve("payments/a@billing.example.test"), None);
        assert_eq!(l.resolve("nosuch/x@other.example"), None);
        l.service
            .store
            .set_mailbox_password(
                Some("billing"),
                "x@other.example",
                Some(&hash_password_with("pw", 1000)),
            )
            .await
            .unwrap();
        let ok = l
            .check(PEER, "billing/X@other.example", "pw")
            .await
            .unwrap();
        assert_eq!(ok.area.as_deref(), Some("billing"));
        assert_eq!(ok.address, "x@other.example");
        assert_eq!(ok.access, MailboxAccess::FULL);
        assert!(
            l.check(PEER, "billing/x@other.example", "tok-billing")
                .await
                .is_some()
        );
        assert!(l.check(PEER, "x@other.example", "pw").await.is_none());
    }
}
