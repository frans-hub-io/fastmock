use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const DEFAULT_PRIORITY: i32 = 5;
pub const SCENARIO_STARTED: &str = "Started";

fn default_priority() -> i32 {
    DEFAULT_PRIORITY
}

fn yes() -> bool {
    true
}

fn default_status() -> u16 {
    200
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_attempts() -> u32 {
    3
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Stub {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// Tie-breaker between stubs of equal priority: the lower value is tried first. A stub with an `order`
    /// is placed before stubs without one, and URL specificity is not used for it. Set by the WireMock
    /// (newest wins) and MockServer (oldest wins) importers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    pub request: RequestPattern,
    #[serde(default)]
    pub response: ResponseDef,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responses: Vec<ResponseDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mode: Option<ResponseMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<ScenarioRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callbacks: Vec<CallbackDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Default for Stub {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: None,
            priority: DEFAULT_PRIORITY,
            order: None,
            request: RequestPattern::default(),
            response: ResponseDef::default(),
            responses: Vec::new(),
            response_mode: None,
            times: None,
            expires_at: None,
            ttl_seconds: None,
            scenario: None,
            callbacks: Vec::new(),
            messages: Vec::new(),
            tags: Vec::new(),
            metadata: None,
            enabled: true,
            updated_at: None,
        }
    }
}

impl Stub {
    pub fn expiry(&self) -> Option<DateTime<Utc>> {
        let by_ttl = self.ttl_seconds.and_then(|t| {
            let base = self.updated_at?;
            base.checked_add_signed(chrono::TimeDelta::try_seconds(
                t.min(i64::MAX as u64) as i64
            )?)
        });
        match (self.expires_at, by_ttl) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expiry().is_some_and(|e| e <= now)
    }

    pub fn response_count(&self) -> usize {
        if self.responses.is_empty() {
            1
        } else {
            self.responses.len()
        }
    }

    pub fn response_at(&self, index: usize) -> &ResponseDef {
        self.responses.get(index).unwrap_or(&self.response)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseMode {
    Sequential,
    Cycle,
    Random,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestPattern {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_path_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_path_template: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub path_params: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<BodyMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql: Option<GraphQlMatcher>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cookies: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<BasicAuth>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub form_parameters: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub multipart: Vec<MultipartMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multipart_matching_type: Option<MultipartMatchingType>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MultipartMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<BodyMatcher>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MultipartMatchingType {
    #[default]
    Any,
    All,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub enum DateTruncation {
    #[serde(rename = "first second of minute", alias = "FIRST_SECOND_OF_MINUTE")]
    FirstSecondOfMinute,
    #[serde(rename = "first minute of hour", alias = "FIRST_MINUTE_OF_HOUR")]
    FirstMinuteOfHour,
    #[serde(rename = "first hour of day", alias = "FIRST_HOUR_OF_DAY")]
    FirstHourOfDay,
    #[serde(rename = "first day of month", alias = "FIRST_DAY_OF_MONTH")]
    FirstDayOfMonth,
    #[serde(rename = "first day of next month", alias = "FIRST_DAY_OF_NEXT_MONTH")]
    FirstDayOfNextMonth,
    #[serde(rename = "last day of month", alias = "LAST_DAY_OF_MONTH")]
    LastDayOfMonth,
    #[serde(rename = "first day of year", alias = "FIRST_DAY_OF_YEAR")]
    FirstDayOfYear,
    #[serde(rename = "first day of next year", alias = "FIRST_DAY_OF_NEXT_YEAR")]
    FirstDayOfNextYear,
    #[serde(rename = "last day of year", alias = "LAST_DAY_OF_YEAR")]
    LastDayOfYear,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphQlMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_field: Option<StringMatcher>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StringMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equal_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub does_not_match: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub case_insensitive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(no_recursion)]
    pub and: Vec<StringMatcher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(no_recursion)]
    pub or: Vec<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub not: Option<Box<StringMatcher>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equal_to_date_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncate_expected: Option<DateTruncation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncate_actual: Option<DateTruncation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub has_exactly: Option<Vec<StringMatcher>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub includes: Option<Vec<StringMatcher>>,
    /// Every value of a multi-valued parameter must match at least one of these matchers, and at least one
    /// value must be present (MockServer `keyMatchStyle: MATCHING_KEY`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub all_values: Option<Vec<StringMatcher>>,
}

impl StringMatcher {
    pub fn equal(v: impl Into<String>) -> Self {
        Self {
            equal_to: Some(v.into()),
            ..Default::default()
        }
    }

    pub fn is_multi_value(&self) -> bool {
        self.has_exactly.is_some() || self.includes.is_some() || self.all_values.is_some()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equal_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equal_to_json: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_extra_elements: bool,
    /// equalToJson: an actual array may hold more items than the expected one (WireMock
    /// `ignoreExtraElements`, MockServer `ONLY_MATCHING_FIELDS`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_extra_array_items: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_path: Option<String>,
    /// JSONPath dialect: `rfc9535` (default) or `jayway` (WireMock: `=~`, `in`, `nin`, `length()`,
    /// filters applied to objects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_path_dialect: Option<JsonPathDialect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<StringMatcher>,
    /// A body matcher applied to each node selected by jsonPath or xpath; objects and arrays are matched as
    /// their JSON text (WireMock `matchesJsonPath` with `equalToJson`, `matchesJsonSchema`, `equalToXml`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub value_body: Option<Box<BodyMatcher>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xpath: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub xpath_namespaces: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equal_to_xml: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempted_comparisons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_placeholders: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_array_order: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_equal_to: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub negate: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum JsonPathDialect {
    #[default]
    Rfc9535,
    Jayway,
}

/// One header value, or an array of values sent as separate header lines (for example `Set-Cookie`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(untagged)]
pub enum HeaderValues {
    One(String),
    Many(Vec<String>),
}

/// Response header values are kept as one string; several values are joined with a line feed, which can
/// never be part of a header value, and are sent as separate header lines.
pub mod header_values {
    use std::collections::BTreeMap;

    use serde::de::Error as _;
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serializer};

    pub const SEPARATOR: char = '\n';

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    pub fn join<I: IntoIterator<Item = S>, S: AsRef<str>>(values: I) -> String {
        let mut out = String::new();
        for (i, v) in values.into_iter().enumerate() {
            if i > 0 {
                out.push(SEPARATOR);
            }
            out.push_str(v.as_ref());
        }
        out
    }

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, String>, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(m.len()))?;
        for (k, v) in m {
            if v.contains(SEPARATOR) {
                map.serialize_entry(k, &v.split(SEPARATOR).collect::<Vec<_>>())?;
            } else {
                map.serialize_entry(k, v)?;
            }
        }
        map.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, String>, D::Error> {
        let raw = BTreeMap::<String, OneOrMany>::deserialize(d)?;
        raw.into_iter()
            .map(|(k, v)| match v {
                OneOrMany::One(s) => Ok((k, s)),
                OneOrMany::Many(list) if list.is_empty() => Err(D::Error::custom(format!(
                    "header {k}: an array of values must not be empty"
                ))),
                OneOrMany::Many(list) => Ok((k, join(list))),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseDef {
    #[serde(default = "default_status")]
    pub status: u16,
    /// Header values: a string, or an array of strings sent as separate header lines.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "header_values"
    )]
    #[schema(value_type = BTreeMap<String, HeaderValues>)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64_body: Option<String>,
    #[serde(default = "yes")]
    pub templated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Delay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<Fault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse: Option<Vec<SseEvent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket: Option<crate::ws_model::WebSocketDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunked_dribble_delay: Option<ChunkedDribbleDelay>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChunkedDribbleDelay {
    pub number_of_chunks: u32,
    pub total_duration_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SseEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub delay_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageDef {
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_value: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avro_schema: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChaosRule {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub probability: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Delay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<Fault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl ChaosRule {
    pub fn applies(&self, source: &str, method: &str, path: &str, now: DateTime<Utc>) -> bool {
        self.expires_at.is_none_or(|e| e > now)
            && self.source.as_ref().is_none_or(|s| s == source)
            && self
                .method
                .as_ref()
                .is_none_or(|m| m.eq_ignore_ascii_case(method))
            && self
                .path_prefix
                .as_ref()
                .is_none_or(|p| path.starts_with(p.as_str()))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > 64 {
            return Err("chaos rule id must be 1..64 characters".into());
        }
        if !(0.0..=1.0).contains(&self.probability) {
            return Err(format!(
                "chaos rule {}: probability must be within 0..1",
                self.id
            ));
        }
        if self.status.is_some_and(|s| !(100..=599).contains(&s)) {
            return Err(format!("chaos rule {}: invalid status", self.id));
        }
        if self.delay.is_none() && self.status.is_none() && self.fault.is_none() {
            return Err(format!(
                "chaos rule {}: set delay, status or fault",
                self.id
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyDef {
    pub base_url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_prefix: Option<String>,
}

impl Default for ResponseDef {
    fn default() -> Self {
        Self {
            status: 200,
            headers: BTreeMap::new(),
            body: None,
            json_body: None,
            base64_body: None,
            templated: true,
            delay: None,
            fault: None,
            proxy: None,
            sse: None,
            websocket: None,
            body_file_name: None,
            status_message: None,
            chunked_dribble_delay: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Delay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lognormal: Option<LogNormalDelay>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogNormalDelay {
    pub median_ms: u64,
    pub sigma: f64,
}

pub const MAX_DELAY_SPEC_MS: u64 = 24 * 3600 * 1000;
const MAX_LOGNORMAL_SIGMA: f64 = 10.0;

impl LogNormalDelay {
    pub fn sample_ms(&self) -> u64 {
        let u1: f64 = rand::random::<f64>().max(f64::MIN_POSITIVE);
        let u2: f64 = rand::random::<f64>();
        let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
        let v = self.median_ms as f64 * (self.sigma * z).exp();
        if v.is_finite() {
            v.clamp(0.0, MAX_DELAY_SPEC_MS as f64).round() as u64
        } else {
            MAX_DELAY_SPEC_MS
        }
    }
}

impl Delay {
    pub fn validate(&self) -> Result<(), String> {
        if [self.fixed_ms, self.min_ms, self.max_ms]
            .iter()
            .flatten()
            .any(|v| *v > MAX_DELAY_SPEC_MS)
        {
            return Err(format!(
                "delay values are limited to {MAX_DELAY_SPEC_MS} ms"
            ));
        }
        if let Some(l) = &self.lognormal {
            if l.median_ms > MAX_DELAY_SPEC_MS {
                return Err(format!(
                    "delay.lognormal.medianMs is limited to {MAX_DELAY_SPEC_MS} ms"
                ));
            }
            if !(l.sigma.is_finite() && (0.0..=MAX_LOGNORMAL_SIGMA).contains(&l.sigma)) {
                return Err("delay.lognormal.sigma must be within 0..10".into());
            }
        }
        let kinds = [
            self.fixed_ms.is_some(),
            self.lognormal.is_some(),
            self.min_ms.is_some() || self.max_ms.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if kinds > 1 {
            return Err("delay: use only one of fixedMs, lognormal, minMs/maxMs".into());
        }
        Ok(())
    }

    pub fn sample_ms(&self) -> u64 {
        if let Some(f) = self.fixed_ms {
            return f;
        }
        if let Some(l) = &self.lognormal {
            return l.sample_ms();
        }
        match (self.min_ms, self.max_ms) {
            (Some(lo), Some(hi)) if hi > lo => rand::random_range(lo..=hi),
            (Some(lo), _) => lo,
            (None, Some(hi)) => rand::random_range(0..=hi),
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Fault {
    EmptyResponse,
    MalformedJson,
    ConnectionReset,
    RandomDataThenClose,
    MalformedResponseChunk,
}

fn default_pass_through() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Delay>,
    #[serde(default = "default_pass_through")]
    pub proxy_pass_through: bool,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            fixed_delay_ms: None,
            delay: None,
            proxy_pass_through: true,
        }
    }
}

impl GlobalSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.fixed_delay_ms.is_some_and(|v| v > MAX_DELAY_SPEC_MS) {
            return Err(format!("fixedDelayMs is limited to {MAX_DELAY_SPEC_MS} ms"));
        }
        if let Some(d) = &self.delay {
            d.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScenarioRule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallbackDef {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Delay>,
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac: Option<HmacSignature>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HmacSignature {
    pub header: String,
    pub secret_ref: String,
    #[serde(default = "default_hmac_algorithm")]
    pub algorithm: HmacAlgorithm,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default)]
    pub encoding: SignatureEncoding,
}

fn default_hmac_algorithm() -> HmacAlgorithm {
    HmacAlgorithm::Sha256
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SignatureEncoding {
    #[default]
    Hex,
    Base64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HmacAlgorithm {
    Sha256,
    Sha512,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    #[serde(default)]
    pub id: i64,
    pub at: DateTime<Utc>,
    pub source: String,
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub status: u16,
    pub response_body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stub_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    pub matched: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalFilter {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub correlation: Option<String>,
    #[serde(default)]
    pub body_contains: Option<String>,
    #[serde(default)]
    pub matched: Option<bool>,
    #[serde(default)]
    pub stub_id: Option<Uuid>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<i64>,
}

impl JournalFilter {
    pub fn accepts(&self, e: &JournalEntry) -> bool {
        self.source.as_ref().is_none_or(|s| *s == e.source)
            && self
                .method
                .as_ref()
                .is_none_or(|m| m.eq_ignore_ascii_case(&e.method))
            && self
                .path_prefix
                .as_ref()
                .is_none_or(|p| e.path.starts_with(p.as_str()))
            && self
                .correlation
                .as_ref()
                .is_none_or(|c| e.correlation.as_deref() == Some(c.as_str()))
            && self
                .body_contains
                .as_ref()
                .is_none_or(|b| e.body.contains(b.as_str()))
            && self.matched.is_none_or(|m| m == e.matched)
            && self.stub_id.is_none_or(|id| e.stub_id == Some(id))
            && self.since.is_none_or(|s| e.at >= s)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CallbackStatus {
    Pending,
    Sent,
    Failed,
}

impl CallbackStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallbackStatus::Pending => "pending",
            CallbackStatus::Sent => "sent",
            CallbackStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallbackJob {
    pub id: Uuid,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    pub url: String,
    pub method: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub due_at: DateTime<Utc>,
    pub attempts: u32,
    pub max_attempts: u32,
    pub status: CallbackStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<u16>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

impl CallbackJob {
    pub fn new(
        source: &str,
        url: String,
        method: String,
        headers: BTreeMap<String, String>,
        body: String,
        due_at: DateTime<Utc>,
        max_attempts: u32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7(),
            source: source.to_string(),
            correlation: None,
            url,
            method,
            headers,
            body,
            due_at,
            attempts: 0,
            max_attempts: max_attempts.max(1),
            status: CallbackStatus::Pending,
            last_error: None,
            last_status: None,
            created_at: now,
            sent_at: None,
            traceparent: None,
        }
    }

    pub fn with_correlation(mut self, c: impl Into<String>) -> Self {
        self.correlation = Some(c.into());
        self
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallbackFilter {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub correlation: Option<String>,
    #[serde(default)]
    pub status: Option<CallbackStatus>,
    #[serde(default)]
    pub limit: Option<i64>,
}

impl CallbackFilter {
    pub fn accepts(&self, j: &CallbackJob) -> bool {
        self.source.as_ref().is_none_or(|s| *s == j.source)
            && self
                .correlation
                .as_ref()
                .is_none_or(|c| j.correlation.as_deref() == Some(c.as_str()))
            && self.status.is_none_or(|s| s == j.status)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    #[serde(default)]
    pub id: i64,
    pub at: DateTime<Utc>,
    pub principal: String,
    pub role: String,
    pub action: String,
    pub detail: Value,
}
