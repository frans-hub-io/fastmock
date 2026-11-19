use std::collections::{BTreeMap, BTreeSet};

use crate::crd::FastMockSpec;

pub const ALLOW_MOUNT_LABEL: &str = "fastmock.franshub.io/allow-mount";

const DENIED_ENV: &[&str] = &[
    "FASTMOCK_AUTH_DISABLED",
    "FASTMOCK_ADMIN_TOKENS",
    "FASTMOCK_ADMIN_TOKENS_FILE",
    "FASTMOCK_SECRETS_DIR",
    "FASTMOCK_CALLBACK_DENIED_CIDRS",
    "FASTMOCK_CALLBACK_ALLOWED_HOSTS",
    "FASTMOCK_ADMIN_ALLOWED_ORIGINS",
    "FASTMOCK_DATABASE_URL",
    "FASTMOCK_VALKEY_URL",
    "FASTMOCK_VALKEY_PREFIX",
    "FASTMOCK_STORAGE",
    "FASTMOCK_LISTEN",
    "FASTMOCK_ADMIN_LISTEN",
];

const DENIED_ENV_PREFIXES: &[&str] = &["FASTMOCK_SECRET_", "LD_"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePolicy {
    pub default_image: String,
    pub allowed_prefixes: Vec<String>,
}

impl ImagePolicy {
    pub fn new(default_image: &str, allowed_prefixes: &str) -> Self {
        let default_image = default_image.trim().to_string();
        let mut allowed: Vec<String> = allowed_prefixes
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        if allowed.is_empty() && !default_image.is_empty() {
            allowed.push(repository(&default_image).to_string());
        }
        Self {
            default_image,
            allowed_prefixes: allowed,
        }
    }

    pub fn resolve(&self, requested: &str) -> Result<String, String> {
        let image = requested.trim();
        let image = if image.is_empty() {
            self.default_image.as_str()
        } else {
            image
        };
        if image.is_empty() {
            return Err("spec.image is empty and the operator has no default server image".into());
        }
        if self
            .allowed_prefixes
            .iter()
            .any(|p| image_matches(image, p))
        {
            Ok(image.to_string())
        } else {
            Err(format!(
                "image {image} is not allowed; allowed prefixes: {}",
                self.allowed_prefixes.join(", ")
            ))
        }
    }
}

pub fn repository(image: &str) -> &str {
    let no_digest = image.split('@').next().unwrap_or(image);
    let last_slash = no_digest.rfind('/').map_or(0, |i| i + 1);
    match no_digest[last_slash..].rfind(':') {
        Some(i) => &no_digest[..last_slash + i],
        None => no_digest,
    }
}

pub fn image_matches(image: &str, prefix: &str) -> bool {
    if prefix.is_empty() || image.chars().any(char::is_whitespace) {
        return false;
    }
    if image == prefix {
        return true;
    }
    let Some(rest) = image.strip_prefix(prefix) else {
        return false;
    };
    prefix.ends_with(['/', ':', '@']) || rest.starts_with([':', '@', '/'])
}

pub fn denied_env_keys(env: &BTreeMap<String, String>) -> Vec<String> {
    env.keys()
        .filter(|k| {
            let u = k.trim().to_ascii_uppercase();
            DENIED_ENV.contains(&u.as_str()) || DENIED_ENV_PREFIXES.iter().any(|p| u.starts_with(p))
        })
        .cloned()
        .collect()
}

pub fn referenced_secrets(spec: &FastMockSpec) -> Vec<(String, String)> {
    let mut out = vec![];
    if let Some(s) = &spec.secrets_secret {
        out.push(("secretsSecret".to_string(), s.clone()));
    }
    if let Some(r) = &spec.storage.database {
        out.push(("storage.database".to_string(), r.name.clone()));
    }
    if let Some(r) = &spec.storage.valkey {
        out.push(("storage.valkey".to_string(), r.name.clone()));
    }
    if let Some(r) = &spec.admin_tokens_secret {
        out.push(("adminTokensSecret".to_string(), r.name.clone()));
    }
    for s in &spec.image_pull_secrets {
        out.push(("imagePullSecrets".to_string(), s.clone()));
    }
    let mut seen = BTreeSet::new();
    out.retain(|(_, name)| seen.insert(name.clone()));
    out
}

pub fn secret_allowed(labels: Option<&BTreeMap<String, String>>) -> bool {
    labels
        .and_then(|l| l.get(ALLOW_MOUNT_LABEL))
        .is_some_and(|v| v == "true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{SecretKeyRef, StorageSpec};

    #[test]
    fn repository_strips_tag_and_digest() {
        assert_eq!(repository("fastmock:0.1.0"), "fastmock");
        assert_eq!(
            repository("registry.example.com:5000/team/fastmock:1"),
            "registry.example.com:5000/team/fastmock"
        );
        assert_eq!(
            repository("registry.example.com/fastmock@sha256:abc"),
            "registry.example.com/fastmock"
        );
        assert_eq!(
            repository("registry.example.com:5000/fastmock"),
            "registry.example.com:5000/fastmock"
        );
    }

    #[test]
    fn image_prefix_respects_boundaries() {
        let p = "registry.example.com/fastmock";
        assert!(image_matches("registry.example.com/fastmock:1.0", p));
        assert!(image_matches("registry.example.com/fastmock@sha256:ab", p));
        assert!(image_matches("registry.example.com/fastmock", p));
        assert!(!image_matches("registry.example.com/fastmock-evil:1", p));
        assert!(!image_matches("registry.example.com/fastmockx", p));
        assert!(!image_matches("evil.example.com/fastmock:1", p));
        assert!(image_matches(
            "registry.example.com/any:1",
            "registry.example.com/"
        ));
        assert!(!image_matches("anything:1", ""));
    }

    #[test]
    fn policy_defaults_to_repository_of_default_image() {
        let p = ImagePolicy::new("registry.example.com/fastmock:0.2.0", " ");
        assert_eq!(p.allowed_prefixes, vec!["registry.example.com/fastmock"]);
        assert_eq!(
            p.resolve("").unwrap(),
            "registry.example.com/fastmock:0.2.0"
        );
        assert_eq!(
            p.resolve("registry.example.com/fastmock:0.3.0").unwrap(),
            "registry.example.com/fastmock:0.3.0"
        );
        assert!(p.resolve("docker.io/library/busybox:1").is_err());
        let p = ImagePolicy::new(
            "fastmock:1",
            "registry.example.com/a, registry.example.com/b/,",
        );
        assert_eq!(p.allowed_prefixes.len(), 2);
        assert!(p.resolve("registry.example.com/b/x:1").is_ok());
        assert!(p.resolve("fastmock:1").is_err());
        assert!(ImagePolicy::new("", "").resolve("").is_err());
    }

    #[test]
    fn env_filter_rejects_security_relevant_keys() {
        let env = BTreeMap::from([
            ("FASTMOCK_AUTH_DISABLED".to_string(), "true".to_string()),
            ("fastmock_admin_tokens".to_string(), "x".to_string()),
            ("FASTMOCK_SECRET_DB".to_string(), "x".to_string()),
            ("FASTMOCK_VALKEY_PREFIX".to_string(), "other:".to_string()),
            ("LD_PRELOAD".to_string(), "/x.so".to_string()),
            ("FASTMOCK_MAX_DELAY".to_string(), "5s".to_string()),
            ("FASTMOCK_PLUGIN_RCS_URL".to_string(), "x".to_string()),
            ("RUST_LOG".to_string(), "debug".to_string()),
        ]);
        let denied = denied_env_keys(&env);
        assert_eq!(
            denied,
            vec![
                "FASTMOCK_AUTH_DISABLED",
                "FASTMOCK_SECRET_DB",
                "FASTMOCK_VALKEY_PREFIX",
                "LD_PRELOAD",
                "fastmock_admin_tokens"
            ]
        );
    }

    #[test]
    fn referenced_secrets_are_collected_once() {
        let spec = FastMockSpec {
            secrets_secret: Some("payments-secrets".into()),
            storage: StorageSpec {
                mode: "postgres".into(),
                database: Some(SecretKeyRef {
                    name: "db".into(),
                    key: "url".into(),
                }),
                valkey: Some(SecretKeyRef {
                    name: "db".into(),
                    key: "valkey".into(),
                }),
                valkey_prefix: None,
            },
            admin_tokens_secret: Some(crate::crd::TokensSecretRef {
                name: "tokens".into(),
                key: "tokens".into(),
            }),
            image_pull_secrets: vec!["pull".into()],
            ..Default::default()
        };
        let names: Vec<String> = referenced_secrets(&spec)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert_eq!(names, vec!["payments-secrets", "db", "tokens", "pull"]);
    }

    #[test]
    fn secret_label_opt_in() {
        assert!(!secret_allowed(None));
        let mut l = BTreeMap::new();
        assert!(!secret_allowed(Some(&l)));
        l.insert(ALLOW_MOUNT_LABEL.to_string(), "false".to_string());
        assert!(!secret_allowed(Some(&l)));
        l.insert(ALLOW_MOUNT_LABEL.to_string(), "true".to_string());
        assert!(secret_allowed(Some(&l)));
    }
}
