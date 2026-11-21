//! Mailbox passwords for IMAP/POP3 logins, stored as PBKDF2-HMAC-SHA256 hashes in the form
//! `pbkdf2-sha256$<iterations>$<salt, base64 without padding>$<hash, base64 without padding>`.
//! The admin API writes them ([`hash`]); the IMAP and POP3 servers check logins with [`verify`].

use std::num::NonZeroU32;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};

pub const SCHEME: &str = "pbkdf2-sha256";
pub const ITERATIONS: u32 = 100_000;
pub const MAX_PASSWORD_BYTES: usize = 1024;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const MAX_ITERATIONS: u32 = 10_000_000;

/// A new salted hash of `password`.
pub fn hash(password: &str) -> String {
    hash_with(password, ITERATIONS)
}

fn hash_with(password: &str, iterations: u32) -> String {
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

/// Whether `password` matches a hash written by [`hash`] (compared in constant time). Malformed hashes
/// and passwords above [`MAX_PASSWORD_BYTES`] never match.
pub fn verify(stored: &str, password: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_differ() {
        let a = hash_with("s3cret", 1000);
        let b = hash_with("s3cret", 1000);
        assert_ne!(a, b);
        assert!(a.starts_with("pbkdf2-sha256$1000$"));
        assert!(verify(&a, "s3cret"));
        assert!(!verify(&a, "s3cret "));
        assert!(!verify(&a, ""));
        assert!(!verify("plain", "plain"));
        assert!(!verify("pbkdf2-sha256$0$AA$AA", "x"));
        assert!(!verify("pbkdf2-sha256$99999999999$AA$AA", "x"));
        assert!(!verify(&format!("{a}$extra"), "s3cret"));
        assert!(!verify(&a, &"x".repeat(MAX_PASSWORD_BYTES + 1)));
        let full = hash("pw");
        assert!(full.starts_with(&format!("pbkdf2-sha256${ITERATIONS}$")));
        assert!(verify(&full, "pw"));
    }
}
