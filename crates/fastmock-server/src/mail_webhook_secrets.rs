//! Mail webhook secrets at rest: signing keys, URL passwords and the space's SendGrid key are stored
//! sealed (AES-256-GCM) in the data store, and the basic authentication of a queued webhook request is
//! kept sealed in the outbox until the request is sent.
//!
//! The key comes from the secret [`KEY_SECRET`], read like other FastMock secrets
//! (`FASTMOCK_SECRET_MAIL_WEBHOOK_KEY` or the file `mail-webhook-key` in `FASTMOCK_SECRETS_DIR`). With
//! memory storage and no such secret a random key of this process is used (the data does not outlive
//! it); with PostgreSQL storage and no secret, webhook secrets cannot be stored.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fastmock_core::SecretProvider;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};

use crate::config::{Config, StorageKind};

/// The secret holding the key.
pub const KEY_SECRET: &str = "mail-webhook-key";
/// Prefix of sealed values.
pub const SEALED: &str = "sealed:v1:";
/// The outbox header that carries a sealed `Authorization` value of a mail webhook; the outbox opens
/// it when it sends the request and never sends the header itself.
pub const SEALED_AUTH_HEADER: &str = "x-fastmock-sealed-authorization";

/// Seals and opens webhook secrets.
pub struct Sealer {
    key: Option<LessSafeKey>,
}

impl Sealer {
    /// The key of [`KEY_SECRET`], a random key with memory storage, or none.
    pub fn from_config(cfg: &Config) -> Self {
        let secret = crate::state::Secrets::new(cfg.secrets_dir.clone()).get(KEY_SECRET);
        let material: Option<[u8; 32]> = match secret {
            Some(s) if !s.trim().is_empty() => Some(Sha256::digest(s.trim().as_bytes()).into()),
            _ if matches!(cfg.storage, StorageKind::Memory) => {
                let mut k = [0u8; 32];
                SystemRandom::new()
                    .fill(&mut k)
                    .expect("the system random generator works");
                Some(k)
            }
            _ => None,
        };
        Self::with_key(material)
    }

    /// A sealer with this key (`None`: secrets cannot be stored).
    pub fn with_key(key: Option<[u8; 32]>) -> Self {
        Self {
            key: key.map(|k| {
                LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &k).expect("a 256-bit key"))
            }),
        }
    }

    /// Whether secrets can be sealed.
    pub fn available(&self) -> bool {
        self.key.is_some()
    }

    /// `sealed:v1:<base64 of nonce, ciphertext and tag>`.
    pub fn seal(&self, plain: &str) -> Result<String, String> {
        let key = self.key.as_ref().ok_or_else(|| {
            format!(
                "storing webhook signing keys and URL passwords needs the secret `{KEY_SECRET}` (FASTMOCK_SECRET_MAIL_WEBHOOK_KEY or the file {KEY_SECRET} in FASTMOCK_SECRETS_DIR)"
            )
        })?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "no random nonce".to_string())?;
        let mut data = plain.as_bytes().to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data)
            .map_err(|_| "sealing failed".to_string())?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&data);
        Ok(format!("{SEALED}{}", B64.encode(out)))
    }

    /// The plain value of a sealed one; a value without the prefix is returned as it is (stored before
    /// sealing existed). `None` when it cannot be opened (another key, damaged).
    pub fn open(&self, v: &str) -> Option<String> {
        let Some(b64) = v.strip_prefix(SEALED) else {
            return Some(v.to_string());
        };
        let key = self.key.as_ref()?;
        let raw = B64.decode(b64).ok()?;
        if raw.len() < NONCE_LEN {
            return None;
        }
        let (nonce, rest) = raw.split_at(NONCE_LEN);
        let mut data = rest.to_vec();
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let plain = key.open_in_place(nonce, Aad::empty(), &mut data).ok()?;
        String::from_utf8(plain.to_vec()).ok()
    }

    /// Seals a value unless it is sealed already.
    pub fn seal_once(&self, v: &str) -> Result<String, String> {
        if v.starts_with(SEALED) {
            return Ok(v.to_string());
        }
        self.seal(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens() {
        let s = Sealer::with_key(Some([7; 32]));
        let a = s.seal("key-1").unwrap();
        let b = s.seal("key-1").unwrap();
        assert!(a.starts_with(SEALED) && a != b && !a.contains("key-1"));
        assert_eq!(s.open(&a).as_deref(), Some("key-1"));
        assert_eq!(s.open("plain").as_deref(), Some("plain"));
        assert_eq!(s.seal_once(&a).unwrap(), a);
        assert!(Sealer::with_key(Some([8; 32])).open(&a).is_none());
        let mut broken = a.clone();
        broken.pop();
        broken.push('A');
        assert!(s.open(&broken).is_none() || s.open(&broken).as_deref() != Some("key-1"));
        let none = Sealer::with_key(None);
        assert!(!none.available());
        assert!(none.seal("x").unwrap_err().contains(KEY_SECRET));
        assert!(none.open(&a).is_none());
    }
}
