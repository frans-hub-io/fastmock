//! Mailbox passwords for IMAP/POP3 logins. The hash format and the check live in
//! [`fastmock_mail::mailbox_login`] (PBKDF2-HMAC-SHA256, `pbkdf2-sha256$<iterations>$<salt>$<hash>`), which
//! the IMAP and POP3 servers use; the admin API writes hashes with [`hash`] and [`verify`] checks them.

pub use fastmock_mail::mailbox_login::{MAX_PASSWORD_BYTES, verify_password as verify};

/// A new salted hash of `password`.
pub fn hash(password: &str) -> String {
    fastmock_mail::mailbox_login::hash_password(password)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmock_mail::mailbox_login::ITERATIONS;

    #[test]
    fn hashes_are_mailbox_login_hashes() {
        let a = hash("s3cret");
        assert!(a.starts_with(&format!("pbkdf2-sha256${ITERATIONS}$")));
        assert_ne!(a, hash("s3cret"));
        assert!(verify(&a, "s3cret"));
        assert!(!verify(&a, "s3cret "));
    }
}
