use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::crd::{FastMockSpec, MailSpec};

pub const MANAGER: &str = "fastmock-operator";
pub const OPERATOR_PRINCIPAL: &str = "fastmock-operator";
pub const ACCESS_DIR: &str = "/etc/fastmock/access";
pub const ACCESS_FILE: &str = "access.yaml";
pub const OIDC_DIR: &str = "/etc/fastmock/oidc";
pub const OIDC_CA_DIR: &str = "/etc/fastmock/oidc-ca";
pub const MAIL_TLS_DIR: &str = "/etc/fastmock/tls/mail";

pub fn labels(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), "fastmock".to_string()),
        ("app.kubernetes.io/instance".to_string(), name.to_string()),
        (
            "app.kubernetes.io/managed-by".to_string(),
            MANAGER.to_string(),
        ),
    ])
}

fn selector(name: &str) -> Value {
    json!({"app.kubernetes.io/name": "fastmock", "app.kubernetes.io/instance": name, "app.kubernetes.io/component": "server"})
}

fn meta(name: &str, ns: &str, owner: &OwnerReference, extra: Option<Value>) -> Value {
    let mut l = serde_json::to_value(labels(name)).unwrap();
    if let Some(Value::Object(e)) = extra {
        l.as_object_mut().unwrap().extend(e);
    }
    json!({"name": name, "namespace": ns, "labels": l, "ownerReferences": [owner]})
}

pub fn sha256_hex(s: &str) -> String {
    sha256_hex_bytes(s.as_bytes())
}

pub fn sha256_hex_bytes(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

pub fn tokens_file(user_lines: &str, operator_token: &str) -> String {
    let mut out: Vec<String> = user_lines
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with('#')
                && !l.starts_with(&format!("{OPERATOR_PRINCIPAL}:"))
        })
        .map(String::from)
        .collect();
    out.push(format!(
        "{OPERATOR_PRINCIPAL}:admin:{}",
        sha256_hex(operator_token)
    ));
    out.join("\n") + "\n"
}

pub fn env_map(name: &str, spec: &FastMockSpec) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("FASTMOCK_LISTEN".into(), "0.0.0.0:8080".into());
    m.insert("FASTMOCK_ADMIN_LISTEN".into(), "0.0.0.0:9090".into());
    m.insert("FASTMOCK_STORAGE".into(), spec.storage.mode.clone());
    m.insert(
        "FASTMOCK_ADMIN_TOKENS_FILE".into(),
        "/etc/fastmock/admin/tokens".into(),
    );
    m.insert("FASTMOCK_LOG_FORMAT".into(), "json".into());
    if let Some(p) = &spec.storage.valkey_prefix {
        m.insert("FASTMOCK_VALKEY_PREFIX".into(), p.clone());
    } else {
        m.insert("FASTMOCK_VALKEY_PREFIX".into(), format!("fastmock:{name}:"));
    }
    if spec.access.is_some() {
        m.insert(
            "FASTMOCK_ACCESS_FILE".into(),
            format!("{ACCESS_DIR}/{ACCESS_FILE}"),
        );
    }
    if let Some(o) = &spec.oidc {
        let mut put = |k: &str, v: &str| m.insert(format!("FASTMOCK_OIDC_{k}"), v.to_string());
        put("ISSUER", &o.issuer);
        put("CLIENT_ID", &o.client_id);
        put("CLIENT_SECRET_FILE", &format!("{OIDC_DIR}/client-secret"));
        put("REDIRECT_URL", &o.redirect_url);
        if !o.scopes.is_empty() {
            put("SCOPES", &o.scopes.join(" "));
        }
        if !o.allowed_hosts.is_empty() {
            put("ALLOWED_HOSTS", &o.allowed_hosts.join(","));
        }
        if o.ca_config_map_ref.is_some() {
            put("CA_FILE", &format!("{OIDC_CA_DIR}/ca.crt"));
        }
        for (k, v) in [
            ("USERNAME_CLAIM", &o.username_claim),
            ("GROUPS_CLAIM", &o.groups_claim),
            ("API_AUDIENCE", &o.api_audience),
            ("SESSION_TTL", &o.session_ttl),
            ("SESSION_IDLE", &o.session_idle),
        ] {
            if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                put(k, v);
            }
        }
    }
    if let Some(mail) = spec.mail_on() {
        mail_env(&mut m, mail);
    }
    if let Some(mcp) = &spec.mcp {
        m.insert("FASTMOCK_MCP_ENABLED".into(), mcp.enabled.to_string());
    }
    if spec.secrets_secret.is_some() {
        m.insert(
            "FASTMOCK_SECRETS_DIR".into(),
            "/etc/fastmock/secrets".into(),
        );
    }
    if !spec.plugins.is_empty() {
        m.insert("FASTMOCK_PLUGINS".into(), spec.plugins.join(","));
    }
    for (plugin, settings) in &spec.plugin_settings {
        for (k, v) in settings {
            let key = format!(
                "FASTMOCK_PLUGIN_{}_{}",
                plugin.to_ascii_uppercase().replace('-', "_"),
                k.to_ascii_uppercase().replace(['-', '.'], "_")
            );
            m.insert(key, v.clone());
        }
    }
    if let Some(o) = &spec.otel_endpoint {
        m.insert("OTEL_EXPORTER_OTLP_ENDPOINT".into(), o.clone());
        m.insert("OTEL_SERVICE_NAME".into(), format!("fastmock-{name}"));
    }
    for (k, v) in &spec.env {
        m.insert(k.clone(), v.clone());
    }
    m
}

fn mail_env(m: &mut BTreeMap<String, String>, mail: &MailSpec) {
    m.insert("FASTMOCK_MAIL_ENABLED".into(), "true".into());
    let ports = mail.ports.enabled();
    if !ports.iter().any(|(n, ..)| *n == "smtp") {
        m.insert("FASTMOCK_MAIL_SMTP_LISTEN".into(), String::new());
    }
    for (_, port, var, _) in ports {
        m.insert(var.into(), format!("0.0.0.0:{port}"));
    }
    if let Some(h) = mail
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        m.insert("FASTMOCK_MAIL_HOSTNAME".into(), h.into());
    }
    if mail.tls_secret_ref.is_some() {
        m.insert(
            "FASTMOCK_MAIL_TLS_CERT_FILE".into(),
            format!("{MAIL_TLS_DIR}/tls.crt"),
        );
        m.insert(
            "FASTMOCK_MAIL_TLS_KEY_FILE".into(),
            format!("{MAIL_TLS_DIR}/tls.key"),
        );
    }
    m.insert(
        "FASTMOCK_MAIL_SMTP_REQUIRE_AUTH".into(),
        mail.require_auth.to_string(),
    );
    m.insert(
        "FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH".into(),
        mail.allow_plaintext_auth.to_string(),
    );
    if let Some(l) = &mail.limits {
        for (k, v) in [
            ("MAX_MESSAGE_BYTES", l.max_message_bytes),
            ("MAX_MESSAGES", l.max_messages),
            ("MAX_BYTES", l.max_bytes),
            ("MAX_MAILBOXES", l.max_mailboxes),
            ("LOGIN_FAILURE_LIMIT", l.login_failure_limit),
            ("MAX_CONNECTIONS", l.max_connections),
            ("MAX_CONNECTIONS_PER_IP", l.max_connections_per_ip),
        ] {
            if let Some(v) = v {
                m.insert(format!("FASTMOCK_MAIL_{k}"), v.to_string());
            }
        }
        for (k, v) in [
            ("MAX_AGE", &l.max_age),
            ("IDLE_TIMEOUT", &l.idle_timeout),
            ("SESSION_TIMEOUT", &l.session_timeout),
            ("IMAP_IDLE_TIMEOUT", &l.imap_idle_timeout),
        ] {
            if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                m.insert(format!("FASTMOCK_MAIL_{k}"), v.into());
            }
        }
    }
}

/// Pod template checksum: the pods roll when the environment, the token file, the access file, the
/// OIDC client secret or the mail certificate and SMTP users (given as digests) change. Without an access
/// file, OIDC and mail it is the same as before they existed, so upgrades do not roll pods.
pub fn config_checksum(
    env: &BTreeMap<String, String>,
    tokens: &str,
    access_file: Option<&str>,
    oidc_secret_digest: Option<&str>,
    mail_secrets_digest: Option<&str>,
) -> String {
    let mut basis = format!(
        "{}|{}",
        serde_json::to_string(env).unwrap_or_default(),
        tokens
    );
    if let Some(a) = access_file {
        basis.push('|');
        basis.push_str(a);
    }
    if let Some(d) = oidc_secret_digest {
        basis.push_str("|oidc-client-secret:");
        basis.push_str(d);
    }
    if let Some(d) = mail_secrets_digest {
        basis.push_str("|mail-secrets:");
        basis.push_str(d);
    }
    sha256_hex(&basis)
}

pub fn config_map(
    name: &str,
    ns: &str,
    owner: &OwnerReference,
    env: &BTreeMap<String, String>,
) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": meta(&format!("{name}-config"), ns, owner, None), "data": env})
}

pub fn tokens_secret(name: &str, ns: &str, owner: &OwnerReference, tokens: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Secret", "type": "Opaque", "metadata": meta(&format!("{name}-admin-tokens"), ns, owner, None), "stringData": {"tokens": tokens}})
}

pub fn operator_token_secret(name: &str, ns: &str, owner: &OwnerReference, token: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Secret", "type": "Opaque", "metadata": meta(&format!("{name}-operator-token"), ns, owner, None), "stringData": {"token": token}})
}

pub fn service_account(name: &str, ns: &str, owner: &OwnerReference) -> Value {
    json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": meta(name, ns, owner, None), "automountServiceAccountToken": false})
}

pub fn services(name: &str, ns: &str, owner: &OwnerReference) -> [Value; 2] {
    [
        json!({"apiVersion": "v1", "kind": "Service", "metadata": meta(name, ns, owner, None),
            "spec": {"selector": selector(name), "ports": [{"name": "http", "port": 8080, "targetPort": "http"}]}}),
        json!({"apiVersion": "v1", "kind": "Service", "metadata": meta(&format!("{name}-admin"), ns, owner, Some(json!({"app.kubernetes.io/component": "admin"}))),
            "spec": {"selector": selector(name), "ports": [{"name": "admin", "port": 9090, "targetPort": "admin"}]}}),
    ]
}

/// The `<name>-mail` Service with the enabled mail ports; `None` when mail is off or has no listener.
pub fn mail_service(
    name: &str,
    ns: &str,
    owner: &OwnerReference,
    spec: &FastMockSpec,
) -> Option<Value> {
    let mail = spec.mail_on()?;
    let ports: Vec<Value> = mail
        .ports
        .enabled()
        .into_iter()
        .map(|(n, p, _, _)| json!({"name": n, "port": p, "targetPort": n, "protocol": "TCP"}))
        .collect();
    if ports.is_empty() {
        return None;
    }
    let svc = mail.service.clone().unwrap_or_default();
    let mut metadata = meta(
        &format!("{name}-mail"),
        ns,
        owner,
        Some(json!({"app.kubernetes.io/component": "mail"})),
    );
    if !svc.annotations.is_empty() {
        metadata["annotations"] = json!(svc.annotations);
    }
    Some(
        json!({"apiVersion": "v1", "kind": "Service", "metadata": metadata,
        "spec": {"type": svc.type_.as_deref().unwrap_or("ClusterIP"), "selector": selector(name), "ports": ports}}),
    )
}

pub fn pdb(name: &str, ns: &str, owner: &OwnerReference) -> Value {
    json!({"apiVersion": "policy/v1", "kind": "PodDisruptionBudget", "metadata": meta(name, ns, owner, None),
        "spec": {"minAvailable": 1, "selector": {"matchLabels": selector(name)}}})
}

pub fn http_route(
    name: &str,
    ns: &str,
    owner: &OwnerReference,
    hostnames: &[String],
    parent_refs: &[Value],
) -> Value {
    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": meta(name, ns, owner, None),
        "spec": {"parentRefs": parent_refs, "hostnames": hostnames,
            "rules": [{"matches": [{"path": {"type": "PathPrefix", "value": "/"}}], "backendRefs": [{"name": name, "port": 8080}]}]}})
}

fn secret_env(env: &str, r: &crate::crd::SecretKeyRef) -> Value {
    json!({"name": env, "valueFrom": {"secretKeyRef": {"name": r.name, "key": r.key}}})
}

pub fn deployment(
    name: &str,
    ns: &str,
    owner: &OwnerReference,
    spec: &FastMockSpec,
    image: &str,
    checksum: &str,
) -> Value {
    let mut env = vec![];
    if spec.storage.mode == "postgres" {
        if let Some(d) = &spec.storage.database {
            env.push(secret_env("FASTMOCK_DATABASE_URL", d));
        }
        if let Some(v) = &spec.storage.valkey {
            env.push(secret_env("FASTMOCK_VALKEY_URL", v));
        }
    }
    let mail = spec.mail_on();
    if let Some(r) = mail.and_then(|m| m.users_secret_ref.as_ref()) {
        env.push(json!({"name": "FASTMOCK_MAIL_SMTP_USERS", "valueFrom": {"secretKeyRef": {"name": r.name, "key": r.key}}}));
    }
    let mut ports = vec![
        json!({"name": "http", "containerPort": 8080}),
        json!({"name": "admin", "containerPort": 9090}),
    ];
    for (n, p, _, _) in mail.map(|m| m.ports.enabled()).unwrap_or_default() {
        ports.push(json!({"name": n, "containerPort": p, "protocol": "TCP"}));
    }
    let mut mounts =
        vec![json!({"name": "admin-tokens", "mountPath": "/etc/fastmock/admin", "readOnly": true})];
    let mut volumes = vec![
        json!({"name": "admin-tokens", "secret": {"secretName": format!("{name}-admin-tokens"), "defaultMode": 0o440}}),
    ];
    if let Some(s) = &spec.secrets_secret {
        mounts.push(
            json!({"name": "secrets", "mountPath": "/etc/fastmock/secrets", "readOnly": true}),
        );
        volumes.push(json!({"name": "secrets", "secret": {"secretName": s, "defaultMode": 0o440}}));
    }
    if let Some(a) = &spec.access {
        let r = &a.config_map_ref;
        mounts.push(json!({"name": "access", "mountPath": ACCESS_DIR, "readOnly": true}));
        volumes.push(json!({"name": "access", "configMap": {"name": r.name, "items": [{"key": r.key, "path": ACCESS_FILE}], "defaultMode": 0o440}}));
    }
    if let Some(o) = &spec.oidc {
        let r = &o.client_secret_ref;
        mounts.push(json!({"name": "oidc", "mountPath": OIDC_DIR, "readOnly": true}));
        volumes.push(json!({"name": "oidc", "secret": {"secretName": r.name, "items": [{"key": r.key, "path": "client-secret"}], "defaultMode": 0o440}}));
        if let Some(c) = &o.ca_config_map_ref {
            mounts.push(json!({"name": "oidc-ca", "mountPath": OIDC_CA_DIR, "readOnly": true}));
            volumes.push(json!({"name": "oidc-ca", "configMap": {"name": c.name, "items": [{"key": c.key, "path": "ca.crt"}], "defaultMode": 0o440}}));
        }
    }
    if let Some(r) = mail.and_then(|m| m.tls_secret_ref.as_ref()) {
        mounts.push(json!({"name": "tls-mail", "mountPath": MAIL_TLS_DIR, "readOnly": true}));
        volumes.push(json!({"name": "tls-mail", "secret": {"secretName": r.name, "items": [{"key": r.cert_key, "path": "tls.crt"}, {"key": r.key_key, "path": "tls.key"}], "defaultMode": 0o440}}));
    }
    let mqtt_tls = spec.env.get("FASTMOCK_MQTT_URL").is_some_and(|u| {
        let u = u.trim().to_ascii_lowercase();
        ["mqtts://", "ssl://", "tls://"]
            .iter()
            .any(|p| u.starts_with(p))
    });
    if mqtt_tls {
        mounts.push(json!({"name": "tmp", "mountPath": "/tmp"}));
        volumes.push(json!({"name": "tmp", "emptyDir": {"medium": "Memory", "sizeLimit": "1Mi"}}));
    }
    let resources = spec.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"cpu": "2", "memory": "512Mi"}}));
    let pull: Vec<Value> = spec
        .image_pull_secrets
        .iter()
        .map(|s| json!({"name": s}))
        .collect();
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": meta(name, ns, owner, Some(json!({"app.kubernetes.io/component": "server"}))),
        "spec": {
            "replicas": spec.replicas,
            "selector": {"matchLabels": selector(name)},
            "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 0, "maxSurge": 1}},
            "template": {
                "metadata": {"labels": selector(name), "annotations": {"fastmock.franshub.io/config-checksum": checksum}},
                "spec": {
                    "serviceAccountName": name,
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "imagePullSecrets": pull,
                    "securityContext": {"runAsNonRoot": true, "runAsUser": 65532, "runAsGroup": 65532, "fsGroup": 65532, "seccompProfile": {"type": "RuntimeDefault"}},
                    "containers": [{
                        "name": "fastmock",
                        "image": image,
                        "args": ["serve"],
                        "envFrom": [{"configMapRef": {"name": format!("{name}-config")}}],
                        "env": env,
                        "ports": ports,
                        "securityContext": {"runAsNonRoot": true, "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": {"drop": ["ALL"]}},
                        "startupProbe": {"httpGet": {"path": "/healthz", "port": "admin"}, "periodSeconds": 2, "failureThreshold": 60},
                        "readinessProbe": {"httpGet": {"path": "/readyz", "port": "admin"}, "periodSeconds": 5, "timeoutSeconds": 3},
                        "livenessProbe": {"httpGet": {"path": "/healthz", "port": "admin"}, "periodSeconds": 10, "timeoutSeconds": 3, "failureThreshold": 6},
                        "resources": resources,
                        "volumeMounts": mounts
                    }],
                    "volumes": volumes
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{SecretKeyRef, StorageSpec};

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "fastmock.franshub.io/v1alpha1".into(),
            kind: "FastMock".into(),
            name: "m".into(),
            uid: "u".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn tokens_file_replaces_operator_line() {
        let f = tokens_file("# c\nops:admin:aa\nfastmock-operator:admin:old\n", "t");
        assert_eq!(
            f,
            format!(
                "ops:admin:aa\nfastmock-operator:admin:{}\n",
                sha256_hex("t")
            )
        );
    }

    #[test]
    fn deployment_is_hardened_and_wires_storage() {
        let spec = FastMockSpec {
            image: "img:1".into(),
            replicas: 2,
            storage: StorageSpec {
                mode: "postgres".into(),
                database: Some(SecretKeyRef {
                    name: "db".into(),
                    key: "url".into(),
                }),
                valkey: Some(SecretKeyRef {
                    name: "vk".into(),
                    key: "url".into(),
                }),
                valkey_prefix: None,
            },
            plugin_settings: BTreeMap::from([(
                "example".into(),
                BTreeMap::from([("callback_url".into(), "http://x".into())]),
            )]),
            ..Default::default()
        };
        let d = deployment("m", "ns", &owner(), &spec, "img:2", "abc");
        let c = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["image"], "img:2");
        assert_eq!(c["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(c["env"][0]["valueFrom"]["secretKeyRef"]["name"], "db");
        assert_eq!(
            d["spec"]["template"]["spec"]["automountServiceAccountToken"],
            false
        );
        assert_eq!(d["metadata"]["ownerReferences"][0]["kind"], "FastMock");
        let env = env_map("m", &spec);
        assert_eq!(env["FASTMOCK_PLUGIN_EXAMPLE_CALLBACK_URL"], "http://x");
        assert_eq!(env["FASTMOCK_VALKEY_PREFIX"], "fastmock:m:");
        assert!(!env.contains_key("FASTMOCK_ACCESS_FILE"));
        let vols = d["spec"]["template"]["spec"]["volumes"].as_array().unwrap();
        assert!(vols.iter().all(|v| v["name"] != "access"));
        assert!(vols.iter().all(|v| v["name"] != "tmp"));
    }

    #[test]
    fn mqtt_over_tls_gets_a_writable_tmp() {
        let spec = FastMockSpec {
            env: BTreeMap::from([(
                "FASTMOCK_MQTT_URL".into(),
                "mqtts://mqtt.example.com:8883".into(),
            )]),
            ..Default::default()
        };
        let d = deployment("m", "ns", &owner(), &spec, "img:1", "abc");
        let pod = &d["spec"]["template"]["spec"];
        assert!(
            pod["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "tmp" && v["emptyDir"]["medium"] == "Memory")
        );
        assert!(
            pod["containers"][0]["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "tmp" && m["mountPath"] == "/tmp")
        );
    }

    #[test]
    fn access_file_is_mounted_read_only_and_rolls_the_pods() {
        let spec = FastMockSpec {
            access: Some(crate::crd::AccessSpec {
                config_map_ref: crate::crd::AccessConfigMapRef {
                    name: "team-access".into(),
                    key: "roles.yaml".into(),
                },
            }),
            ..Default::default()
        };
        let env = env_map("m", &spec);
        assert_eq!(
            env["FASTMOCK_ACCESS_FILE"],
            "/etc/fastmock/access/access.yaml"
        );
        let d = deployment("m", "ns", &owner(), &spec, "img:1", "abc");
        let pod = &d["spec"]["template"]["spec"];
        let vol = pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "access")
            .unwrap();
        assert_eq!(
            vol["configMap"],
            json!({"name": "team-access", "items": [{"key": "roles.yaml", "path": "access.yaml"}], "defaultMode": 0o440})
        );
        let mount = pod["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "access")
            .unwrap();
        assert_eq!(mount["mountPath"], "/etc/fastmock/access");
        assert_eq!(mount["readOnly"], true);
        let base = config_checksum(&env, "t", None, None, None);
        assert_eq!(
            base,
            sha256_hex(&format!("{}|t", serde_json::to_string(&env).unwrap()))
        );
        let a = config_checksum(&env, "t", Some("spaces: [{name: a}]"), None, None);
        let b = config_checksum(&env, "t", Some("spaces: [{name: b}]"), None, None);
        assert!(a != b && a != base);
        let s1 = config_checksum(&env, "t", None, Some("d1"), None);
        let s2 = config_checksum(&env, "t", None, Some("d2"), None);
        assert!(s1 != s2 && s1 != base);
    }

    #[test]
    fn oidc_secret_and_ca_are_mounted_and_env_is_set() {
        let spec = FastMockSpec {
            oidc: Some(crate::crd::OidcSpec {
                issuer: "https://sso.example.com/realms/mocks".into(),
                client_id: "fastmock".into(),
                client_secret_ref: crate::crd::OidcSecretRef {
                    name: "sso".into(),
                    key: "secret".into(),
                },
                redirect_url: "https://mock-admin.example.com/__admin/auth/callback".into(),
                scopes: vec!["openid".into(), "groups".into()],
                groups_claim: Some("realm_access.roles".into()),
                api_audience: Some("fastmock-api".into()),
                allowed_hosts: vec!["login.example.com".into()],
                ca_config_map_ref: Some(crate::crd::CaConfigMapRef {
                    name: "corp-ca".into(),
                    key: "bundle.pem".into(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let env = env_map("m", &spec);
        assert_eq!(
            env["FASTMOCK_OIDC_ISSUER"],
            "https://sso.example.com/realms/mocks"
        );
        assert_eq!(
            env["FASTMOCK_OIDC_CLIENT_SECRET_FILE"],
            "/etc/fastmock/oidc/client-secret"
        );
        assert_eq!(env["FASTMOCK_OIDC_SCOPES"], "openid groups");
        assert_eq!(env["FASTMOCK_OIDC_GROUPS_CLAIM"], "realm_access.roles");
        assert_eq!(env["FASTMOCK_OIDC_API_AUDIENCE"], "fastmock-api");
        assert_eq!(env["FASTMOCK_OIDC_ALLOWED_HOSTS"], "login.example.com");
        assert_eq!(env["FASTMOCK_OIDC_CA_FILE"], "/etc/fastmock/oidc-ca/ca.crt");
        assert!(!env.contains_key("FASTMOCK_OIDC_USERNAME_CLAIM"));
        assert!(!env.contains_key("FASTMOCK_OIDC_CLIENT_SECRET"));
        let d = deployment("m", "ns", &owner(), &spec, "img:1", "abc");
        let pod = &d["spec"]["template"]["spec"];
        let vol = |n: &str| {
            pod["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == n)
                .cloned()
                .unwrap()
        };
        assert_eq!(
            vol("oidc")["secret"],
            json!({"secretName": "sso", "items": [{"key": "secret", "path": "client-secret"}], "defaultMode": 0o440})
        );
        assert_eq!(vol("oidc-ca")["configMap"]["items"][0]["key"], "bundle.pem");
        let mounts = pod["containers"][0]["volumeMounts"].as_array().unwrap();
        assert!(mounts.iter().any(|m| m["name"] == "oidc"
            && m["mountPath"] == "/etc/fastmock/oidc"
            && m["readOnly"] == true));
        let plain = env_map("m", &FastMockSpec::default());
        assert!(plain.keys().all(|k| !k.starts_with("FASTMOCK_OIDC_")));
    }

    #[test]
    fn empty_optional_oidc_values_are_not_put_into_the_env() {
        let spec = FastMockSpec {
            oidc: Some(crate::crd::OidcSpec {
                issuer: "https://sso.example.com/realms/mocks".into(),
                client_id: "fastmock".into(),
                redirect_url: "https://mock-admin.example.com/__admin/auth/callback".into(),
                api_audience: Some(String::new()),
                username_claim: Some(" ".into()),
                session_ttl: Some(String::new()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let env = env_map("m", &spec);
        for k in [
            "FASTMOCK_OIDC_API_AUDIENCE",
            "FASTMOCK_OIDC_USERNAME_CLAIM",
            "FASTMOCK_OIDC_SESSION_TTL",
        ] {
            assert!(!env.contains_key(k), "{k}");
        }
        assert_eq!(
            env["FASTMOCK_OIDC_ISSUER"],
            "https://sso.example.com/realms/mocks"
        );
    }

    #[test]
    fn mcp_can_be_switched_off() {
        let plain = env_map("m", &FastMockSpec::default());
        assert!(!plain.contains_key("FASTMOCK_MCP_ENABLED"));
        let off: FastMockSpec = serde_json::from_value(json!({"mcp": {"enabled": false}})).unwrap();
        assert_eq!(env_map("m", &off)["FASTMOCK_MCP_ENABLED"], "false");
        let on: FastMockSpec = serde_json::from_value(json!({"mcp": {}})).unwrap();
        assert_eq!(env_map("m", &on)["FASTMOCK_MCP_ENABLED"], "true");
    }

    fn mail_spec() -> FastMockSpec {
        serde_json::from_value(json!({
            "mail": {
                "enabled": true,
                "hostname": "mail.example.test",
                "ports": {"imap": 1143, "smtps": 4650},
                "tlsSecretRef": {"name": "mail-tls"},
                "requireAuth": true,
                "usersSecretRef": {"name": "mail-users", "key": "list"},
                "limits": {"maxMessages": 50, "maxBytes": 1048576000, "maxMailboxes": 300, "loginFailureLimit": 0, "maxAge": "2d", "idleTimeout": " "},
                "service": {"type": "LoadBalancer", "annotations": {"example.com/a": "b"}}
            }
        }))
        .unwrap()
    }

    #[test]
    fn mail_sets_env_ports_mounts_and_a_service() {
        let spec = mail_spec();
        let env = env_map("m", &spec);
        assert_eq!(env["FASTMOCK_MAIL_ENABLED"], "true");
        assert_eq!(env["FASTMOCK_MAIL_SMTP_LISTEN"], "0.0.0.0:2525");
        assert_eq!(env["FASTMOCK_MAIL_SMTPS_LISTEN"], "0.0.0.0:4650");
        assert_eq!(env["FASTMOCK_MAIL_IMAP_LISTEN"], "0.0.0.0:1143");
        assert!(!env.contains_key("FASTMOCK_MAIL_POP3_LISTEN"));
        assert_eq!(env["FASTMOCK_MAIL_HOSTNAME"], "mail.example.test");
        assert_eq!(
            env["FASTMOCK_MAIL_TLS_CERT_FILE"],
            "/etc/fastmock/tls/mail/tls.crt"
        );
        assert_eq!(env["FASTMOCK_MAIL_SMTP_REQUIRE_AUTH"], "true");
        assert_eq!(env["FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH"], "false");
        assert_eq!(env["FASTMOCK_MAIL_MAX_MESSAGES"], "50");
        assert_eq!(env["FASTMOCK_MAIL_MAX_BYTES"], "1048576000");
        assert_eq!(env["FASTMOCK_MAIL_MAX_MAILBOXES"], "300");
        assert_eq!(env["FASTMOCK_MAIL_LOGIN_FAILURE_LIMIT"], "0");
        assert_eq!(env["FASTMOCK_MAIL_MAX_AGE"], "2d");
        assert!(!env.contains_key("FASTMOCK_MAIL_IDLE_TIMEOUT"));
        assert!(!env.contains_key("FASTMOCK_MAIL_SMTP_USERS"));
        let d = deployment("m", "ns", &owner(), &spec, "img:1", "abc");
        let pod = &d["spec"]["template"]["spec"];
        let c = &pod["containers"][0];
        let names: Vec<&str> = c["ports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["http", "admin", "smtp", "smtps", "imap"]);
        assert!(c["env"].as_array().unwrap().iter().any(|e| e
            == &json!({"name": "FASTMOCK_MAIL_SMTP_USERS", "valueFrom": {"secretKeyRef": {"name": "mail-users", "key": "list"}}})));
        let vol = pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "tls-mail")
            .unwrap();
        assert_eq!(
            vol["secret"],
            json!({"secretName": "mail-tls", "items": [{"key": "tls.crt", "path": "tls.crt"}, {"key": "tls.key", "path": "tls.key"}], "defaultMode": 0o440})
        );
        assert!(
            c["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"] == "tls-mail"
                    && m["mountPath"] == "/etc/fastmock/tls/mail"
                    && m["readOnly"] == true)
        );
        let svc = mail_service("m", "ns", &owner(), &spec).unwrap();
        assert_eq!(svc["metadata"]["name"], "m-mail");
        assert_eq!(svc["metadata"]["annotations"]["example.com/a"], "b");
        assert_eq!(
            svc["metadata"]["labels"]["app.kubernetes.io/component"],
            "mail"
        );
        assert_eq!(svc["spec"]["type"], "LoadBalancer");
        assert_eq!(svc["spec"]["selector"], selector("m"));
        assert_eq!(
            svc["spec"]["ports"][2],
            json!({"name": "imap", "port": 1143, "targetPort": "imap", "protocol": "TCP"})
        );
        let base = config_checksum(&env, "t", None, None, None);
        assert_ne!(base, config_checksum(&env, "t", None, None, Some("d")));
    }

    #[test]
    fn mail_off_or_without_listeners_changes_nothing() {
        let plain: FastMockSpec = serde_json::from_value(json!({})).unwrap();
        let mut off = mail_spec();
        off.mail.as_mut().unwrap().enabled = false;
        assert_eq!(env_map("m", &plain), env_map("m", &off));
        assert_eq!(
            deployment("m", "ns", &owner(), &plain, "i", "c"),
            deployment("m", "ns", &owner(), &off, "i", "c")
        );
        assert!(mail_service("m", "ns", &owner(), &off).is_none());
        let none: FastMockSpec = serde_json::from_value(json!({
            "mail": {"enabled": true, "ports": {"smtp": 0}}
        }))
        .unwrap();
        let env = env_map("m", &none);
        assert_eq!(env["FASTMOCK_MAIL_SMTP_LISTEN"], "");
        assert!(mail_service("m", "ns", &owner(), &none).is_none());
        let svc = mail_service(
            "m",
            "ns",
            &owner(),
            &serde_json::from_value(json!({"mail": {"enabled": true}})).unwrap(),
        )
        .unwrap();
        assert_eq!(svc["spec"]["type"], "ClusterIP");
        assert!(svc["metadata"].get("annotations").is_none());
        assert_eq!(svc["spec"]["ports"].as_array().unwrap().len(), 1);
    }
}
