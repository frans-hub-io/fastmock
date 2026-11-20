use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn preserve_unknown_array(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "items": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}
    })
}

fn preserve_unknown_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "object", "x-kubernetes-preserve-unknown-fields": true})
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    pub name: String,
    #[serde(default = "default_key")]
    pub key: String,
}

fn default_key() -> String {
    "url".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokensSecretRef {
    pub name: String,
    #[serde(default = "default_tokens_key")]
    pub key: String,
}

fn default_tokens_key() -> String {
    "tokens".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccessConfigMapRef {
    /// ConfigMap in the namespace of the FastMock; it must carry the label `fastmock.franshub.io/allow-mount: "true"`.
    pub name: String,
    /// Key holding the access file.
    #[serde(default = "default_access_key")]
    pub key: String,
}

fn default_access_key() -> String {
    "access.yaml".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccessSpec {
    /// The access file (custom roles, spaces, bindings), mounted read-only and passed as `FASTMOCK_ACCESS_FILE`.
    pub config_map_ref: AccessConfigMapRef,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OidcSecretRef {
    /// Secret in the namespace of the FastMock; it must carry the label `fastmock.franshub.io/allow-mount: "true"`.
    pub name: String,
    #[serde(default = "default_client_secret_key")]
    pub key: String,
}

fn default_client_secret_key() -> String {
    "client-secret".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CaConfigMapRef {
    /// ConfigMap in the namespace of the FastMock; it must carry the label `fastmock.franshub.io/allow-mount: "true"`.
    pub name: String,
    #[serde(default = "default_ca_key")]
    pub key: String,
}

fn default_ca_key() -> String {
    "ca.crt".into()
}

/// OIDC sign-in for the UI and OIDC bearer JWTs for the admin API (`FASTMOCK_OIDC_*`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OidcSpec {
    /// Issuer URL (https); discovery from `/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    /// The client secret, mounted read-only and passed as `FASTMOCK_OIDC_CLIENT_SECRET_FILE`.
    pub client_secret_ref: OidcSecretRef,
    /// Public admin URL followed by `/__admin/auth/callback`.
    pub redirect_url: String,
    /// Default `openid profile email`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Default `preferred_username`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_claim: Option<String>,
    /// Default `groups`; dotted paths such as `realm_access.roles` work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_claim: Option<String>,
    /// Enables bearer JWTs on the admin API for this audience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_audience: Option<String>,
    /// Absolute session lifetime, default `8h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ttl: Option<String>,
    /// Idle timeout, default `1h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_idle: Option<String>,
    /// Hosts besides the issuer's that the discovery document may point to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    /// PEM bundle of extra CAs for the IdP connections, passed as `FASTMOCK_OIDC_CA_FILE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_config_map_ref: Option<CaConfigMapRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<SecretKeyRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valkey: Option<SecretKeyRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valkey_prefix: Option<String>,
}

fn default_mode() -> String {
    "memory".into()
}

impl Default for StorageSpec {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            database: None,
            valkey: None,
            valkey_prefix: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouteSpec {
    #[serde(default)]
    pub hostnames: Vec<String>,
    #[serde(default)]
    #[schemars(schema_with = "preserve_unknown_array")]
    pub parent_refs: Vec<Value>,
}

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "fastmock.franshub.io",
    version = "v1alpha1",
    kind = "FastMock",
    namespaced,
    status = "FastMockStatus",
    shortname = "fm",
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Storage","type":"string","jsonPath":".spec.storage.mode"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FastMockSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default = "one")]
    pub replicas: i32,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
    #[serde(default)]
    pub storage: StorageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_tokens_secret: Option<TokensSecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_secret: Option<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub plugin_settings: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "preserve_unknown_object")]
    pub resources: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteSpec>,
    /// Access control for the server: a ConfigMap with its access file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessSpec>,
    /// SSO sign-in (OIDC) for the UI and bearer JWTs for the admin API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcSpec>,
}

fn one() -> i32 {
    1
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition_time: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FastMockStatus {
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub admin_url: String,
    #[serde(default)]
    pub data_url: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerRef {
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapKeyRef {
    pub name: String,
    pub key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_ref: Option<ConfigMapKeyRef>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpenApiSource {
    #[serde(flatten)]
    pub source: DocumentSource,
    #[serde(default)]
    pub path_prefix: String,
    #[serde(default = "yes")]
    pub validate_requests: bool,
}

fn yes() -> bool {
    true
}

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "fastmock.franshub.io",
    version = "v1alpha1",
    kind = "MockApi",
    namespaced,
    status = "MockApiStatus",
    shortname = "fmapi",
    printcolumn = r#"{"name":"Server","type":"string","jsonPath":".spec.serverRef.name"}"#,
    printcolumn = r#"{"name":"Stubs","type":"integer","jsonPath":".status.importedStubs"}"#,
    printcolumn = r#"{"name":"Synced","type":"string","jsonPath":".status.conditions[?(@.type==\"Synced\")].status"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MockApiSpec {
    pub server_ref: ServerRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "preserve_unknown_array")]
    pub stubs: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openapi: Option<OpenApiSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiremock: Option<DocumentSource>,
    /// Space of the FastMock server the stubs go to; it must be defined in the server's access file. With the operator policy `spaceFromNamespace` the space is the MockApi namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$"))]
    pub space: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MockApiStatus {
    #[serde(default)]
    pub imported_stubs: i64,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub last_sync_time: String,
    #[serde(default)]
    pub applied_hash: String,
    /// Space the stubs were applied to; absent for the global area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Instance id of the in-memory server process the stubs were imported into. A server that reports
    /// another id has restarted and lost them, so they are imported again.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server_instance: String,
    /// Instance id of the in-memory server process a restart replaces. While it is set, `Synced` stays
    /// `False` with reason `ServerRestarting` until the stubs are confirmed on the new instance.
    #[serde(default)]
    pub replaced_instance: Option<String>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

pub fn condition(type_: &str, ok: bool, reason: &str, message: &str) -> Condition {
    Condition {
        type_: type_.into(),
        status: if ok { "True" } else { "False" }.into(),
        reason: reason.into(),
        message: message.chars().take(1000).collect(),
        last_transition_time: chrono::Utc::now().to_rfc3339(),
    }
}

pub fn merge_conditions(prev: &[Condition], next: Vec<Condition>) -> Vec<Condition> {
    next.into_iter()
        .map(|mut c| {
            if let Some(p) = prev
                .iter()
                .find(|p| p.type_ == c.type_ && p.status == c.status)
            {
                c.last_transition_time = p.last_transition_time.clone();
            }
            c
        })
        .collect()
}

pub fn status_changed<S: PartialEq>(prev: Option<&S>, next: &S) -> bool {
    prev != Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mut c: Condition, t: &str) -> Condition {
        c.last_transition_time = t.into();
        c
    }

    #[test]
    fn transition_time_kept_while_status_unchanged() {
        let prev = vec![
            at(condition("Ready", true, "Available", "1/1"), "t0"),
            at(condition("RouteReady", false, "ApplyFailed", "x"), "t0"),
        ];
        let next = merge_conditions(
            &prev,
            vec![
                condition("Ready", true, "Available", "2/2"),
                condition("RouteReady", true, "Applied", "ok"),
            ],
        );
        assert_eq!(next[0].last_transition_time, "t0");
        assert_eq!(next[0].message, "2/2");
        assert_ne!(next[1].last_transition_time, "t0");
    }

    #[test]
    fn access_key_defaults_to_access_yaml() {
        let spec: FastMockSpec = serde_json::from_value(serde_json::json!({
            "access": {"configMapRef": {"name": "team-access"}}
        }))
        .unwrap();
        let r = spec.access.unwrap().config_map_ref;
        assert_eq!(
            (r.name.as_str(), r.key.as_str()),
            ("team-access", "access.yaml")
        );
        let none: FastMockSpec = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(none.access.is_none());
        assert!(serde_json::to_value(&none).unwrap().get("access").is_none());
    }

    #[test]
    fn identical_status_is_not_rewritten() {
        let prev = MockApiStatus {
            imported_stubs: 3,
            observed_generation: 2,
            last_sync_time: "t0".into(),
            applied_hash: "h".into(),
            conditions: vec![at(condition("Synced", true, "Synced", "3 stubs"), "t0")],
            ..Default::default()
        };
        let next = MockApiStatus {
            conditions: merge_conditions(
                &prev.conditions,
                vec![condition("Synced", true, "Synced", "3 stubs")],
            ),
            ..prev.clone()
        };
        assert!(!status_changed(Some(&prev), &next));
        let failed = MockApiStatus {
            conditions: merge_conditions(
                &prev.conditions,
                vec![condition("Synced", false, "SyncFailed", "boom")],
            ),
            ..prev.clone()
        };
        assert!(status_changed(Some(&prev), &failed));
        assert!(status_changed(None, &prev));
    }
}
