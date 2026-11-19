use serde::{Deserialize, Serialize};

use crate::matcher::CompiledString;
use crate::model::StringMatcher;

pub const MAX_WS_SCRIPTED: usize = 1_000;
pub const MAX_WS_RULES: usize = 64;
pub const MAX_WS_DATA_BYTES: usize = 1024 * 1024;
pub const MAX_WS_DELAY_MS: u64 = 3_600_000;
pub const MAX_WS_DURATION_MS: u64 = 24 * 3_600_000;
pub const MAX_WS_MESSAGES: u32 = 100_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSocketDef {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<WsMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_message: Vec<WsReply>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub echo: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeat: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WsMessage {
    pub data: String,
    #[serde(default)]
    pub delay_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WsReply {
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub matcher: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
}

impl WebSocketDef {
    pub fn validate(&self, template: impl Fn(&str) -> Result<(), String>) -> Result<(), String> {
        if self.messages.len() > MAX_WS_SCRIPTED {
            return Err(format!(
                "websocket.messages: at most {MAX_WS_SCRIPTED} messages"
            ));
        }
        if self.on_message.len() > MAX_WS_RULES {
            return Err(format!("websocket.onMessage: at most {MAX_WS_RULES} rules"));
        }
        if self.repeat && self.messages.iter().all(|m| m.delay_ms == 0) {
            return Err("websocket.repeat needs at least one message with delayMs > 0".to_string());
        }
        for (i, m) in self.messages.iter().enumerate() {
            if m.data.len() > MAX_WS_DATA_BYTES {
                return Err(format!(
                    "websocket.messages[{i}].data exceeds {MAX_WS_DATA_BYTES} bytes"
                ));
            }
            if m.delay_ms > MAX_WS_DELAY_MS {
                return Err(format!(
                    "websocket.messages[{i}].delayMs must be at most {MAX_WS_DELAY_MS}"
                ));
            }
            template(&m.data).map_err(|e| format!("websocket.messages[{i}].data: {e}"))?;
        }
        for (i, r) in self.on_message.iter().enumerate() {
            if let Some(m) = &r.matcher {
                CompiledString::compile(m)
                    .map_err(|e| format!("websocket.onMessage[{i}].match: {e}"))?;
            }
            if let Some(reply) = &r.reply {
                if reply.len() > MAX_WS_DATA_BYTES {
                    return Err(format!(
                        "websocket.onMessage[{i}].reply exceeds {MAX_WS_DATA_BYTES} bytes"
                    ));
                }
                template(reply).map_err(|e| format!("websocket.onMessage[{i}].reply: {e}"))?;
            }
            if r.reply.is_none() && !r.close {
                return Err(format!(
                    "websocket.onMessage[{i}] needs a reply, close, or both"
                ));
            }
        }
        if self
            .close_after_ms
            .is_some_and(|d| d == 0 || d > MAX_WS_DURATION_MS)
        {
            return Err(format!(
                "websocket.closeAfterMs must be between 1 and {MAX_WS_DURATION_MS}"
            ));
        }
        if self
            .max_messages
            .is_some_and(|n| n == 0 || n > MAX_WS_MESSAGES)
        {
            return Err(format!(
                "websocket.maxMessages must be between 1 and {MAX_WS_MESSAGES}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(_: &str) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn parses_and_validates() {
        let d: WebSocketDef = serde_json::from_value(serde_json::json!({
            "messages": [{"data": "hello {{request.path}}", "delayMs": 10}],
            "onMessage": [{"match": {"contains": "bye"}, "reply": "ciao", "close": true}],
            "echo": true, "closeAfterMs": 1000, "maxMessages": 5
        }))
        .unwrap();
        assert!(d.validate(ok).is_ok());
        assert_eq!(
            d.on_message[0]
                .matcher
                .as_ref()
                .unwrap()
                .contains
                .as_deref(),
            Some("bye")
        );
        let back = serde_json::to_value(&d).unwrap();
        assert_eq!(back["onMessage"][0]["match"]["contains"], "bye");
        assert!(serde_json::from_value::<WebSocketDef>(serde_json::json!({"unknown": 1})).is_err());
    }

    #[test]
    fn rejects_out_of_range() {
        let bad = |v: serde_json::Value| {
            serde_json::from_value::<WebSocketDef>(v)
                .unwrap()
                .validate(ok)
                .is_err()
        };
        assert!(bad(serde_json::json!({"closeAfterMs": 0})));
        assert!(bad(serde_json::json!({"maxMessages": 1000000})));
        assert!(bad(
            serde_json::json!({"onMessage": [{"match": {"matches": "("}, "reply": "x"}]})
        ));
        assert!(bad(
            serde_json::json!({"onMessage": [{"match": {"contains": "x"}}]})
        ));
        assert!(bad(
            serde_json::json!({"repeat": true, "messages": [{"data": "x"}]})
        ));
        assert!(bad(
            serde_json::json!({"messages": [{"data": "x", "delayMs": 99999999}]})
        ));
        let failing = |_: &str| Err("broken".to_string());
        let d: WebSocketDef =
            serde_json::from_value(serde_json::json!({"messages": [{"data": "{{"}]})).unwrap();
        assert!(d.validate(failing).unwrap_err().contains("broken"));
    }
}
