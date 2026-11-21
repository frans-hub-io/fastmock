//! SASL for SMTP AUTH: PLAIN (RFC 4616), LOGIN and CRAM-MD5 (RFC 2195). CRAM-MD5 gets a challenge, and
//! its digest is not checked (it is offered only while any credentials are accepted).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// Longest accepted user name and password.
pub const MAX_CREDENTIAL_LEN: usize = 1024;

/// What the client proved.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    /// `None` for CRAM-MD5, whose digest cannot be checked against a stored hash.
    pub password: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaslError {
    /// The client sent `*`.
    Cancelled,
    /// Not base64, not UTF-8, or not the mechanism's format.
    Malformed,
}

/// Decodes one base64 response line (`=` is an empty initial response).
pub fn decode(line: &str) -> Result<Vec<u8>, SaslError> {
    let t = line.trim();
    if t == "*" {
        return Err(SaslError::Cancelled);
    }
    if t == "=" {
        return Ok(Vec::new());
    }
    if t.len() > MAX_CREDENTIAL_LEN * 3 {
        return Err(SaslError::Malformed);
    }
    B64.decode(t).map_err(|_| SaslError::Malformed)
}

pub fn encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn text(bytes: Vec<u8>) -> Result<String, SaslError> {
    let s = String::from_utf8(bytes).map_err(|_| SaslError::Malformed)?;
    if s.len() > MAX_CREDENTIAL_LEN || s.chars().any(|c| c.is_control()) {
        return Err(SaslError::Malformed);
    }
    Ok(s)
}

/// `authzid NUL authcid NUL passwd`; the user is the authentication id.
pub fn plain(msg: &[u8]) -> Result<Credentials, SaslError> {
    let mut parts = msg.split(|b| *b == 0);
    let (Some(_authz), Some(user), Some(pass), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(SaslError::Malformed);
    };
    let user = text(user.to_vec())?;
    if user.is_empty() {
        return Err(SaslError::Malformed);
    }
    Ok(Credentials {
        user,
        password: Some(text(pass.to_vec())?),
    })
}

/// A LOGIN field (user name or password).
pub fn login_field(bytes: Vec<u8>) -> Result<String, SaslError> {
    text(bytes)
}

/// The CRAM-MD5 challenge: `<random.timestamp@hostname>`.
pub fn cram_challenge(hostname: &str) -> String {
    format!(
        "<{}.{}@{hostname}>",
        rand::random::<u32>(),
        chrono::Utc::now().timestamp()
    )
}

/// `user SP digest`; the digest must be 32 hex digits but is not checked.
pub fn cram_response(msg: Vec<u8>) -> Result<Credentials, SaslError> {
    let s = text(msg)?;
    let (user, digest) = s.rsplit_once(' ').ok_or(SaslError::Malformed)?;
    if user.is_empty() || digest.len() != 32 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SaslError::Malformed);
    }
    Ok(Credentials {
        user: user.to_string(),
        password: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_login_cram() {
        let c = plain(b"\0app\0secret").unwrap();
        assert_eq!(c.user, "app");
        assert_eq!(c.password.as_deref(), Some("secret"));
        assert_eq!(
            plain(b"admin\0app\0").unwrap().password.as_deref(),
            Some("")
        );
        assert_eq!(plain(b"app\0secret"), Err(SaslError::Malformed));
        assert_eq!(plain(b"\0\0secret"), Err(SaslError::Malformed));
        assert_eq!(plain(b"\0a\0b\0c"), Err(SaslError::Malformed));
        assert_eq!(plain(b"\0a\x01\0b"), Err(SaslError::Malformed));
        assert_eq!(decode("*"), Err(SaslError::Cancelled));
        assert_eq!(decode("="), Ok(vec![]));
        assert_eq!(decode("!!"), Err(SaslError::Malformed));
        assert_eq!(decode(&encode(b"x")), Ok(b"x".to_vec()));
        assert!(!format!("{c:?}").contains("secret"));
        let ch = cram_challenge("mail.example.test");
        assert!(ch.starts_with('<') && ch.ends_with("@mail.example.test>"));
        let c = cram_response(b"bob 0123456789abcdef0123456789abcdef".to_vec()).unwrap();
        assert_eq!(c.user, "bob");
        assert_eq!(c.password, None);
        assert_eq!(
            cram_response(b"bob xyz".to_vec()),
            Err(SaslError::Malformed)
        );
        assert_eq!(login_field(vec![0xff, 0xfe]), Err(SaslError::Malformed));
    }
}
