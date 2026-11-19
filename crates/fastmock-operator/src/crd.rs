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
    group = "fastmock.io",
    version = "v1alpha1",
    kind = "MockServer",
    namespaced,
    status = "MockServerStatus",
    shortname = "fms",
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Storage","type":"string","jsonPath":".spec.storage.mode"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MockServerSpec {
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
pub struct MockServerStatus {
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
    group = "fastmock.io",
    version = "v1alpha1",
    kind = "MockApi",
    namespaced,
    status = "MockApiStatus",
    shortname = "fma",
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
