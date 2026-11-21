//! Mailbox login for IMAP: the user name is the mailbox address, the password a managed FastMock token
//! allowed `mail:read` in the mailbox's area, or the mailbox password. Kept small and self-contained so
//! it can be swapped for the login module shared with POP3.

use std::sync::OnceLock;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::address::{domain_of, mailbox_of, split};
use crate::service::MailService;

/// Checks a managed FastMock token given as a mailbox password; the server implements it.
#[async_trait]
pub trait ReadTokenCheck: Send + Sync {
    /// Whether `token` is a valid managed token allowed `mail:read` in the area (`None`: the global
    /// area, which needs a global grant).
    async fn allows_read(&self, space: Option<&str>, token: &str) -> bool;
}

/// Compares a stored mailbox password hash with a password ([`verify_password`] by default).
pub type PasswordVerify = fn(stored_hash: &str, password: &[u8]) -> bool;

/// Iterations of [`hash_password`].
pub const PBKDF2_ITERATIONS: u32 = 100_000;
/// Longest password accepted.
pub const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_ITERATIONS: u32 = 10_000_000;
const PREFIX: &str = "pbkdf2-sha256";

fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let Ok(key) = Hmac::<Sha256>::new_from_slice(password) else {
        return;
    };
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let mut mac = key.clone();
        mac.update(salt);
        mac.update(&(i as u32 + 1).to_be_bytes());
        let mut u = mac.finalize().into_bytes();
        let mut t = u;
        for _ in 1..iterations {
            let mut mac = key.clone();
            mac.update(&u);
            u = mac.finalize().into_bytes();
            for (a, b) in t.iter_mut().zip(u.iter()) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

/// `pbkdf2-sha256$<iterations>$<salt>$<hash>` (base64 without padding, 16-byte salt, 32-byte hash).
pub fn hash_password_with(password: &[u8], iterations: u32) -> String {
    let salt: [u8; 16] = rand::random();
    let mut out = [0u8; 32];
    pbkdf2(password, &salt, iterations, &mut out);
    format!(
        "{PREFIX}${iterations}${}${}",
        B64.encode(salt),
        B64.encode(out)
    )
}

/// [`hash_password_with`] at [`PBKDF2_ITERATIONS`].
pub fn hash_password(password: &[u8]) -> String {
    hash_password_with(password, PBKDF2_ITERATIONS)
}

/// Checks a password against a `pbkdf2-sha256$...` hash in constant time; malformed hashes and
/// passwords over [`MAX_PASSWORD_BYTES`] never match.
pub fn verify_password(stored_hash: &str, password: &[u8]) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let mut parts = stored_hash.trim().split('$');
    let (Some(PREFIX), Some(iter), Some(salt), Some(hash), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let Ok(iterations) = iter.parse::<u32>() else {
        return false;
    };
    let (Ok(salt), Ok(want)) = (B64.decode(salt), B64.decode(hash)) else {
        return false;
    };
    if !(1..=MAX_ITERATIONS).contains(&iterations)
        || salt.len() > 64
        || !(16..=64).contains(&want.len())
    {
        return false;
    }
    let mut got = vec![0u8; want.len()];
    pbkdf2(password, &salt, iterations, &mut got);
    bool::from(got.ct_eq(&want))
}

/// A hash checked for mailboxes without a password, so the answer time does not reveal them.
fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash_password(b"fastmock-no-mailbox-password"))
}

/// A logged-in mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxUser {
    /// The area of the mailbox: the space owning the address's domain (`mailDomains`), else global.
    pub space: Option<String>,
    /// Lowercase mailbox address without `+tag`.
    pub address: String,
}

/// The mailbox a login name refers to; `None` when it is not an address of at most 320 bytes.
pub fn mailbox_user(service: &MailService, user: &str) -> Option<MailboxUser> {
    let user = user.trim();
    if user.is_empty() || user.len() > 320 || user.chars().any(char::is_control) {
        return None;
    }
    split(user)?;
    let address = mailbox_of(user);
    let space = domain_of(&address).and_then(|d| service.domains.owner(&d).map(String::from));
    Some(MailboxUser { space, address })
}

/// Checks a login: a managed token first, then the mailbox password (hashing runs on a blocking
/// thread). Credentials are never logged.
pub async fn check(
    service: &MailService,
    tokens: Option<&dyn ReadTokenCheck>,
    verify: PasswordVerify,
    user: &[u8],
    password: &[u8],
) -> Option<MailboxUser> {
    let mb = std::str::from_utf8(user)
        .ok()
        .and_then(|u| mailbox_user(service, u));
    let password_ok = !password.is_empty() && password.len() <= MAX_PASSWORD_BYTES;
    if let (Some(mb), Some(t), Ok(p), true) =
        (&mb, tokens, std::str::from_utf8(password), password_ok)
        && t.allows_read(mb.space.as_deref(), p).await
    {
        return Some(mb.clone());
    }
    let stored = match &mb {
        Some(mb) => match service
            .store
            .mailbox_password(mb.space.as_deref(), &mb.address)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "reading a mailbox password failed");
                None
            }
        },
        None => None,
    };
    let real = stored.is_some();
    let hash = stored.unwrap_or_else(|| dummy_hash().to_string());
    let given = if password_ok {
        password.to_vec()
    } else {
        vec![]
    };
    let matched = tokio::task::spawn_blocking(move || verify(&hash, &given))
        .await
        .unwrap_or(false);
    (real && matched && password_ok).then_some(mb).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MailConfig;
    use crate::routing::MailDomains;
    use fastmock_core::mail::{LocalMailEvents, MemoryMailStore};
    use std::sync::Arc;

    struct Tokens;

    #[async_trait]
    impl ReadTokenCheck for Tokens {
        async fn allows_read(&self, space: Option<&str>, token: &str) -> bool {
            token == format!("tok-{}", space.unwrap_or("global"))
        }
    }

    fn service() -> MailService {
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        domains.add("payments", "payments.example.test");
        MailService::new(
            Arc::new(MailConfig::default()),
            Arc::new(MemoryMailStore::new(10)),
            Arc::new(LocalMailEvents::new(4)),
            domains,
            ["billing", "payments"].map(String::from).into(),
        )
    }

    #[test]
    fn pbkdf2_matches_the_rfc_7914_vector() {
        let mut out = [0u8; 64];
        pbkdf2(b"passwd", b"salt", 1, &mut out);
        assert_eq!(
            hex::encode(out),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
    }

    #[test]
    fn password_hashes() {
        let h = hash_password_with(b"secret", 1000);
        assert!(h.starts_with("pbkdf2-sha256$1000$"), "{h}");
        assert_eq!(h.split('$').count(), 4);
        assert!(!h.contains('='));
        assert!(verify_password(&h, b"secret"));
        assert!(!verify_password(&h, b"Secret"));
        assert_ne!(h, hash_password_with(b"secret", 1000), "salted");
        let salt = B64.encode(b"0123456789abcdef");
        let mut out = [0u8; 32];
        pbkdf2(b"pw", b"0123456789abcdef", 7, &mut out);
        let fixed = format!("pbkdf2-sha256$7${salt}${}", B64.encode(out));
        assert!(verify_password(&fixed, b"pw"));
        for bad in [
            "",
            "pbkdf2-sha256$7$x",
            &fixed.replace("sha256", "sha1"),
            &fixed.replace("$7$", "$0$"),
            &fixed.replace("$7$", "$x$"),
            &format!("{fixed}$more"),
            "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b",
        ] {
            assert!(!verify_password(bad, b"pw"), "{bad}");
        }
        let long = vec![b'a'; MAX_PASSWORD_BYTES + 1];
        let h = hash_password_with(&long, 1);
        assert!(!verify_password(&h, &long));
    }

    #[test]
    fn user_names_map_to_mailboxes() {
        let s = service();
        assert_eq!(
            mailbox_user(&s, "Alice+News@Billing.Example.Test"),
            Some(MailboxUser {
                space: Some("billing".into()),
                address: "alice@billing.example.test".into()
            })
        );
        assert_eq!(mailbox_user(&s, "x@other.example").unwrap().space, None);
        assert_eq!(mailbox_user(&s, "nobody"), None);
        assert_eq!(mailbox_user(&s, "a\n@b.example"), None);
        let long = format!("{}@b.example", "a".repeat(320));
        assert_eq!(mailbox_user(&s, &long), None);
    }

    #[tokio::test]
    async fn tokens_and_passwords() {
        let s = service();
        let t: &dyn ReadTokenCheck = &Tokens;
        let v = verify_password;
        let ok = check(&s, Some(t), v, b"a@billing.example.test", b"tok-billing").await;
        assert_eq!(ok.unwrap().space.as_deref(), Some("billing"));
        assert!(
            check(&s, Some(t), v, b"a@payments.example.test", b"tok-billing")
                .await
                .is_none(),
            "a billing token cannot open a payments mailbox"
        );
        assert!(
            check(&s, Some(t), v, b"a@x.example", b"tok-global")
                .await
                .is_some()
        );
        assert!(
            check(&s, None, v, b"a@billing.example.test", b"tok-billing")
                .await
                .is_none()
        );
        s.store
            .set_mailbox_password(
                Some("payments"),
                "p@payments.example.test",
                Some(&hash_password_with(b"secret", 1000)),
            )
            .await
            .unwrap();
        for (user, pw, ok) in [
            (&b"P+x@payments.example.test"[..], &b"secret"[..], true),
            (b"p@payments.example.test", b"wrong", false),
            (b"p@payments.example.test", b"", false),
            (b"q@payments.example.test", b"secret", false),
            (b"not-an-address", b"secret", false),
        ] {
            assert_eq!(
                check(&s, Some(t), v, user, pw).await.is_some(),
                ok,
                "{}",
                String::from_utf8_lossy(user)
            );
        }
    }
}
