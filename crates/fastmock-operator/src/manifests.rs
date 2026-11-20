use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::crd::FastMockSpec;

pub const MANAGER: &str = "fastmock-operator";
pub const OPERATOR_PRINCIPAL: &str = "fastmock-operator";
pub const ACCESS_DIR: &str = "/etc/fastmock/access";
pub const ACCESS_FILE: &str = "access.yaml";

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
    hex::encode(Sha256::digest(s.as_bytes()))
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

/// Pod template checksum: the pods roll when the environment, the token file or the access file change.
/// Without an access file it is the same as before access files existed, so upgrades do not roll pods.
pub fn config_checksum(
    env: &BTreeMap<String, String>,
    tokens: &str,
    access_file: Option<&str>,
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
                        "ports": [{"name": "http", "containerPort": 8080}, {"name": "admin", "containerPort": 9090}],
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
        let base = config_checksum(&env, "t", None);
        assert_eq!(
            base,
            sha256_hex(&format!("{}|t", serde_json::to_string(&env).unwrap()))
        );
        let a = config_checksum(&env, "t", Some("spaces: [{name: a}]"));
        let b = config_checksum(&env, "t", Some("spaces: [{name: b}]"));
        assert!(a != b && a != base);
    }
}
