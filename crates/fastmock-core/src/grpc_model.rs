//! Stub fields for gRPC calls: `response.grpc` (streams, status with details, trailers, replies to incoming
//! messages) and `request.grpc` (matchers over the messages of a client stream).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::BodyMatcher;

pub const MAX_GRPC_MESSAGES: usize = 1_000;
pub const MAX_GRPC_RULES: usize = 64;
pub const MAX_GRPC_RULE_MESSAGES: usize = 100;
pub const MAX_GRPC_DETAILS: usize = 16;
pub const MAX_GRPC_TRAILERS: usize = 64;
pub const MAX_GRPC_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_GRPC_DELAY_MS: u64 = 3_600_000;
pub const MAX_GRPC_DURATION_MS: u64 = 24 * 3_600_000;
pub const MAX_GRPC_SENT: u32 = 100_000;
pub const DEFAULT_GRPC_SENT: u32 = 10_000;
pub const MAX_GRPC_MATCHERS: usize = 32;

/// Canonical gRPC status codes, by number.
pub const GRPC_CODES: [&str; 17] = [
    "OK",
    "CANCELLED",
    "UNKNOWN",
    "INVALID_ARGUMENT",
    "DEADLINE_EXCEEDED",
    "NOT_FOUND",
    "ALREADY_EXISTS",
    "PERMISSION_DENIED",
    "RESOURCE_EXHAUSTED",
    "FAILED_PRECONDITION",
    "ABORTED",
    "OUT_OF_RANGE",
    "UNIMPLEMENTED",
    "INTERNAL",
    "UNAVAILABLE",
    "DATA_LOSS",
    "UNAUTHENTICATED",
];

/// The gRPC part of a stub response: messages of a server stream, the final status (with
/// `google.rpc.Status` details), trailers, and replies to the messages of a bidirectional stream.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcResponseDef {
    /// Response messages in order. Server streaming sends all of them; unary and client-streaming calls send
    /// the first one; bidirectional calls send them as an initial burst while incoming messages are answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<GrpcMessage>,
    /// Start `messages` again from the first after the last one (streams only). Needs a message with
    /// `delayMs > 0`; the call ends at `maxMessages` or `closeAfterMs`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeat: bool,
    /// End the call (with `status`) after this many messages were sent, 1..100000 (default 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<u32>,
    /// End the call (with `status`) after this time, 1..86400000 ms; capped by the server's stream duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_after_ms: Option<u64>,
    /// The status that ends the call; `OK` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GrpcStatus>,
    /// Trailing metadata sent with the status (values are templates; keys ending in `-bin` take base64).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trailers: BTreeMap<String, String>,
    /// Replies to the incoming messages of a bidirectional stream: the first rule whose `match` accepts
    /// a message applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_message: Vec<GrpcReplyRule>,
    /// `gzip` or `identity`: compression of the response messages. By default the response uses the
    /// request's `grpc-encoding` when the client accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<GrpcCompression>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GrpcCompression {
    Gzip,
    Identity,
}

/// One response message: the output message in the proto3 JSON mapping (a template) and the pause before it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcMessage {
    #[serde(default)]
    pub json_body: Value,
    /// Pause before this message, 0..3600000 ms, capped by `FASTMOCK_MAX_DELAY`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ms: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// A gRPC status: `code` as a number (0..16) or a name (`NOT_FOUND`), a templated `message`, and
/// `google.rpc.Status` details as JSON objects with `@type` (sent in `grpc-status-details-bin`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcStatus {
    #[serde(default)]
    pub code: GrpcCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<Value>,
}

/// A status code as a number or a canonical name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(untagged)]
pub enum GrpcCode {
    Number(u32),
    Name(String),
}

impl Default for GrpcCode {
    fn default() -> Self {
        GrpcCode::Number(0)
    }
}

impl GrpcCode {
    pub fn value(&self) -> Option<u32> {
        match self {
            GrpcCode::Number(n) if *n <= 16 => Some(*n),
            GrpcCode::Number(_) => None,
            GrpcCode::Name(s) => code_by_name(s),
        }
    }
}

pub fn code_by_name(s: &str) -> Option<u32> {
    let up = s.trim().to_ascii_uppercase().replace(['-', ' '], "_");
    GRPC_CODES.iter().position(|c| *c == up).map(|i| i as u32)
}

pub fn code_name(code: u32) -> &'static str {
    GRPC_CODES.get(code as usize).copied().unwrap_or("UNKNOWN")
}

/// A reply rule of a bidirectional stream.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcReplyRule {
    /// Body matchers applied to the incoming message as JSON; all must match. An empty list accepts
    /// every message.
    #[serde(default, rename = "match", skip_serializing_if = "Vec::is_empty")]
    pub matcher: Vec<BodyMatcher>,
    /// Messages sent in answer, in order (templates that see `message.json` and `message.index`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<GrpcMessage>,
    /// End the call with this status after the messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GrpcStatus>,
}

/// Matchers over the messages of a gRPC call. The request body of a client-streaming or bidirectional
/// call is the JSON array of its messages; for unary and server-streaming calls it is the one message.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcMatcher {
    /// The number of request messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<CountMatcher>,
    /// Every request message must match all of these body matchers (an empty stream matches).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub every_message: Vec<BodyMatcher>,
    /// At least one request message must match all of these body matchers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_message: Vec<BodyMatcher>,
    /// The last request message must match all of these body matchers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_message: Vec<BodyMatcher>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CountMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equal_to: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

impl CountMatcher {
    pub fn matches(&self, n: u64) -> bool {
        self.equal_to.is_none_or(|e| e == n)
            && self.min.is_none_or(|m| n >= m)
            && self.max.is_none_or(|m| n <= m)
    }
}

impl GrpcMatcher {
    pub fn matcher_count(&self) -> usize {
        usize::from(self.message_count.is_some())
            + self.every_message.len()
            + self.any_message.len()
            + self.last_message.len()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.matcher_count() > MAX_GRPC_MATCHERS {
            return Err(format!(
                "request.grpc: at most {MAX_GRPC_MATCHERS} matchers"
            ));
        }
        if let Some(c) = &self.message_count {
            if c.equal_to.is_none() && c.min.is_none() && c.max.is_none() {
                return Err("request.grpc.messageCount needs equalTo, min or max".into());
            }
            if let (Some(lo), Some(hi)) = (c.min, c.max)
                && lo > hi
            {
                return Err("request.grpc.messageCount: min is larger than max".into());
            }
        }
        Ok(())
    }
}

fn validate_status(
    s: &GrpcStatus,
    at: &str,
    template: &dyn Fn(&str) -> Result<(), String>,
    json_template: &dyn Fn(&Value) -> Result<(), String>,
) -> Result<(), String> {
    if s.code.value().is_none() {
        return Err(format!(
            "{at}.code must be 0..16 or a status name such as NOT_FOUND"
        ));
    }
    if let Some(m) = &s.message {
        if m.len() > 16 * 1024 {
            return Err(format!("{at}.message exceeds 16384 bytes"));
        }
        template(m).map_err(|e| format!("{at}.message: {e}"))?;
    }
    if s.details.len() > MAX_GRPC_DETAILS {
        return Err(format!("{at}.details: at most {MAX_GRPC_DETAILS} entries"));
    }
    if !s.details.is_empty() && s.code.value() == Some(0) {
        return Err(format!("{at}.details need a non-OK code"));
    }
    for (i, d) in s.details.iter().enumerate() {
        if !d.get("@type").is_some_and(|t| t.is_string()) {
            return Err(format!(
                "{at}.details[{i}] must be an object with \"@type\", for example \"type.googleapis.com/google.rpc.BadRequest\""
            ));
        }
        json_template(d).map_err(|e| format!("{at}.details[{i}]: {e}"))?;
    }
    Ok(())
}

fn validate_messages(
    list: &[GrpcMessage],
    at: &str,
    json_template: &dyn Fn(&Value) -> Result<(), String>,
) -> Result<(), String> {
    for (i, m) in list.iter().enumerate() {
        if !m.json_body.is_object() {
            return Err(format!("{at}[{i}].jsonBody must be a JSON object"));
        }
        if serde_json::to_vec(&m.json_body).map_or(0, |v| v.len()) > MAX_GRPC_MESSAGE_BYTES {
            return Err(format!(
                "{at}[{i}].jsonBody exceeds {MAX_GRPC_MESSAGE_BYTES} bytes"
            ));
        }
        if m.delay_ms > MAX_GRPC_DELAY_MS {
            return Err(format!(
                "{at}[{i}].delayMs must be at most {MAX_GRPC_DELAY_MS}"
            ));
        }
        json_template(&m.json_body).map_err(|e| format!("{at}[{i}].jsonBody: {e}"))?;
    }
    Ok(())
}

impl GrpcResponseDef {
    /// Checks limits and templates; `template` and `json_template` validate text and JSON templates.
    pub fn validate(
        &self,
        template: &dyn Fn(&str) -> Result<(), String>,
        json_template: &dyn Fn(&Value) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.messages.len() > MAX_GRPC_MESSAGES {
            return Err(format!(
                "grpc.messages: at most {MAX_GRPC_MESSAGES} messages"
            ));
        }
        validate_messages(&self.messages, "grpc.messages", json_template)?;
        if self.repeat && self.messages.iter().all(|m| m.delay_ms == 0) {
            return Err("grpc.repeat needs at least one message with delayMs > 0".into());
        }
        if self
            .max_messages
            .is_some_and(|n| n == 0 || n > MAX_GRPC_SENT)
        {
            return Err(format!(
                "grpc.maxMessages must be between 1 and {MAX_GRPC_SENT}"
            ));
        }
        if self
            .close_after_ms
            .is_some_and(|d| d == 0 || d > MAX_GRPC_DURATION_MS)
        {
            return Err(format!(
                "grpc.closeAfterMs must be between 1 and {MAX_GRPC_DURATION_MS}"
            ));
        }
        if let Some(s) = &self.status {
            validate_status(s, "grpc.status", template, json_template)?;
        }
        if self.trailers.len() > MAX_GRPC_TRAILERS {
            return Err(format!(
                "grpc.trailers: at most {MAX_GRPC_TRAILERS} entries"
            ));
        }
        for (k, v) in &self.trailers {
            let lower = k.to_ascii_lowercase();
            if http::HeaderName::from_bytes(lower.as_bytes()).is_err() {
                return Err(format!("grpc.trailers: invalid name {k}"));
            }
            if lower.starts_with("grpc-") || lower.starts_with(':') || lower == "content-type" {
                return Err(format!(
                    "grpc.trailers: {k} is reserved; use grpc.status for the status"
                ));
            }
            template(v).map_err(|e| format!("grpc.trailers.{k}: {e}"))?;
        }
        if self.on_message.len() > MAX_GRPC_RULES {
            return Err(format!("grpc.onMessage: at most {MAX_GRPC_RULES} rules"));
        }
        for (i, r) in self.on_message.iter().enumerate() {
            let at = format!("grpc.onMessage[{i}]");
            if r.matcher.len() > MAX_GRPC_MATCHERS {
                return Err(format!("{at}.match: at most {MAX_GRPC_MATCHERS} matchers"));
            }
            if r.messages.len() > MAX_GRPC_RULE_MESSAGES {
                return Err(format!(
                    "{at}.messages: at most {MAX_GRPC_RULE_MESSAGES} messages"
                ));
            }
            crate::matcher::CompiledBodies::compile(&r.matcher)
                .map_err(|e| format!("{at}.match: {e}"))?;
            if r.messages.is_empty() && r.status.is_none() {
                return Err(format!("{at} needs messages, a status, or both"));
            }
            validate_messages(&r.messages, &format!("{at}.messages"), json_template)?;
            if let Some(s) = &r.status {
                validate_status(s, &format!("{at}.status"), template, json_template)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(_: &str) -> Result<(), String> {
        Ok(())
    }

    fn ok_json(_: &Value) -> Result<(), String> {
        Ok(())
    }

    fn check(v: Value) -> Result<(), String> {
        serde_json::from_value::<GrpcResponseDef>(v)
            .map_err(|e| e.to_string())?
            .validate(&ok, &ok_json)
    }

    #[test]
    fn parses_codes_by_number_and_name() {
        let s: GrpcStatus =
            serde_json::from_value(json!({"code": "not_found", "message": "x"})).unwrap();
        assert_eq!(s.code.value(), Some(5));
        let s: GrpcStatus = serde_json::from_value(json!({"code": 14})).unwrap();
        assert_eq!(s.code.value(), Some(14));
        assert_eq!(code_name(8), "RESOURCE_EXHAUSTED");
        assert_eq!(GrpcCode::Number(17).value(), None);
        assert_eq!(code_by_name("deadline-exceeded"), Some(4));
    }

    #[test]
    fn validates_definitions() {
        assert!(
            check(json!({
                "messages": [{"jsonBody": {"id": "1"}}, {"jsonBody": {"id": "2"}, "delayMs": 10}],
                "status": {"code": "INVALID_ARGUMENT", "message": "bad", "details": [
                    {"@type": "type.googleapis.com/google.rpc.BadRequest", "fieldViolations": []}
                ]},
                "trailers": {"x-request-id": "{{uuid}}"},
                "onMessage": [{"match": [{"jsonPath": "$.id"}], "messages": [{"jsonBody": {}}]}]
            }))
            .is_ok()
        );
        for bad in [
            json!({"messages": [{"jsonBody": "text"}]}),
            json!({"repeat": true, "messages": [{"jsonBody": {}}]}),
            json!({"maxMessages": 0}),
            json!({"closeAfterMs": 0}),
            json!({"status": {"code": 99}}),
            json!({"status": {"code": "NOPE"}}),
            json!({"status": {"code": 3, "details": [{"no": "type"}]}}),
            json!({"status": {"code": 0, "details": [{"@type": "x"}]}}),
            json!({"trailers": {"grpc-status": "1"}}),
            json!({"onMessage": [{"match": []}]}),
            json!({"messages": [{"jsonBody": {}, "delayMs": 3_600_001}]}),
        ] {
            assert!(check(bad.clone()).is_err(), "{bad}");
        }
        assert!(serde_json::from_value::<GrpcResponseDef>(json!({"unknown": 1})).is_err());
    }

    #[test]
    fn count_matcher() {
        let c = CountMatcher {
            min: Some(2),
            max: Some(3),
            equal_to: None,
        };
        assert!(!c.matches(1) && c.matches(2) && c.matches(3) && !c.matches(4));
        let m = GrpcMatcher {
            message_count: Some(CountMatcher::default()),
            ..Default::default()
        };
        assert!(m.validate().is_err());
    }
}
