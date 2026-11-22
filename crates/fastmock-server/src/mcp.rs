//! The control-plane MCP endpoint: `/mcp` on the admin listener (streamable HTTP, stateless, JSON
//! responses). It authenticates like the admin API (bearer tokens, managed tokens, OIDC JWTs and SSO
//! sessions, with the same origin rule) and every tool runs as one or more admin API requests with the
//! caller's credentials, so each tool needs exactly the permission of its REST route, in the same spaces,
//! and its changes are audited like REST changes (with `via: mcp`).

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use tracing::Instrument;
use uuid::Uuid;

use crate::access::Permission;
use crate::auth::{Principal, admin_auth};
use crate::state::AppState;

/// Largest serialized tool result.
pub const MAX_OUTPUT: usize = 256 * 1024;
/// Longest string kept in a tool result; longer ones are cut with a marker.
pub const MAX_STRING: usize = 16 * 1024;
/// Largest MCP request body (an imported document travels inside it).
pub const MAX_BODY: usize = 8 * 1024 * 1024;
/// Largest admin API answer a tool reads.
const MAX_REST_RESPONSE: usize = 32 * 1024 * 1024;
/// Default and largest number of journal bodies' bytes per field in request results.
const DEFAULT_BODY_BYTES: usize = 2048;
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Longest mail wait through MCP (the admin API allows 60 s; MCP clients often time out sooner).
const MAX_MAIL_WAIT_MS: u64 = 30_000;

pub const GUIDE_URI: &str = "fastmock://guide/stubs";
pub const SCHEMA_URI: &str = "fastmock://schema/stub";
const GUIDE: &str = include_str!("mcp_stub_guide.md");

/// Request headers passed on to the admin API: credentials, the CSRF marker of cookie sessions, and the
/// origin information the admin API checks.
const FORWARDED: [header::HeaderName; 6] = [
    header::AUTHORIZATION,
    header::COOKIE,
    header::ORIGIN,
    header::HOST,
    header::USER_AGENT,
    header::HeaderName::from_static("x-fastmock-request"),
];

tokio::task_local! {
    static TOOL: &'static str;
}

/// Audit detail of a change made by an MCP tool: the detail with `via: mcp` and the tool name; other
/// changes keep their detail.
pub fn audit_detail(detail: Value) -> Value {
    let Ok(tool) = TOOL.try_with(|t| *t) else {
        return detail;
    };
    match detail {
        Value::Object(mut m) => {
            m.insert("via".into(), json!("mcp"));
            m.insert("mcpTool".into(), json!(tool));
            Value::Object(m)
        }
        other => json!({"via": "mcp", "mcpTool": tool, "detail": other}),
    }
}

// ---------------------------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpaceArgs {
    /// Space to act on. Without it: everything the token may access (reads), or the global area or the
    /// token's only space (writes).
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListStubsArgs {
    /// Only stubs of this space.
    space: Option<String>,
    /// Only stubs carrying this tag.
    tag: Option<String>,
    /// Case-insensitive search in the stub name and URL path fields.
    query: Option<String>,
    /// Only enabled (true) or disabled (false) stubs.
    enabled: Option<bool>,
    /// Page size, 1 to 100 (default 25).
    limit: Option<u32>,
    /// Stubs to skip (default 0).
    offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StubIdArgs {
    /// Stub id (UUID).
    id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateStubArgs {
    /// The stub: `{request, response}` plus optional fields (see the resource fastmock://schema/stub). An
    /// `id` is generated when missing.
    stub: Map<String, Value>,
    /// Space for the stub when the stub has no `space` field.
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateStubArgs {
    /// Stub id (UUID).
    id: Uuid,
    /// The complete new stub (replaces the stored one). Give either `stub` or `patch`.
    stub: Option<Map<String, Value>>,
    /// JSON merge patch (RFC 7396) applied to the stored stub: objects merge, `null` removes a field,
    /// arrays and scalars replace. Example: `{"response": {"status": 503}}`.
    patch: Option<Map<String, Value>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ValidateStubArgs {
    /// The stub to check without saving it.
    stub: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FindRequestsArgs {
    /// Only requests of this space.
    space: Option<String>,
    /// A stub request pattern (`{method, urlPath, headers, body, mcp, ...}`); when given, the newest
    /// journal entries are matched against it like a stub, and only `space`, `source`, `since`, `limit`
    /// and `maxBodyBytes` apply.
    pattern: Option<Map<String, Value>>,
    /// HTTP method, e.g. `POST`.
    method: Option<String>,
    /// Path prefix, e.g. `/orders`.
    path_prefix: Option<String>,
    /// true: only requests a stub answered; false: only unmatched requests.
    matched: Option<bool>,
    /// Only requests answered by this stub.
    stub_id: Option<Uuid>,
    /// Only requests whose body contains this text.
    body_contains: Option<String>,
    /// Only requests with this correlation id.
    correlation: Option<String>,
    /// `stubs` for stub traffic, or a plugin name.
    source: Option<String>,
    /// Only requests at or after this instant (RFC 3339, e.g. `2026-01-31T12:00:00Z`).
    since: Option<String>,
    /// Page size, 1 to 100 (default 20). Newest first.
    limit: Option<u32>,
    /// Entries to skip (filters only, not with `pattern`).
    offset: Option<u32>,
    /// Bytes kept of each request and response body, 0 to 65536 (default 2048).
    max_body_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetRequestArgs {
    /// Journal entry id.
    id: i64,
    /// Bytes kept of the request and response body, 0 to 65536 (default 16384).
    max_body_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NearMissRequest {
    /// HTTP method.
    method: String,
    /// Path, e.g. `/users/7`.
    path: String,
    /// Raw query string without `?`.
    query: Option<String>,
    /// Request headers.
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    /// Request body text.
    body: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NearMissesArgs {
    /// Explain this journal entry (as recorded, so with secrets redacted). Give `requestId` or `request`.
    request_id: Option<i64>,
    /// Explain this request.
    request: Option<NearMissRequest>,
    /// Only compare with stubs of this space.
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifyArgs {
    /// Stub request pattern the requests must match, e.g. `{"method": "POST", "urlPath": "/orders"}`.
    request: Map<String, Value>,
    /// Exactly this many matching requests.
    count: Option<u64>,
    /// At least this many (default 1 when no count is given).
    at_least: Option<u64>,
    /// At most this many.
    at_most: Option<u64>,
    /// Only requests at or after this instant (RFC 3339).
    since: Option<String>,
    /// `stubs` or a plugin name.
    source: Option<String>,
    /// Only requests of this space.
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetScenarioArgs {
    /// Scenario name.
    name: String,
    /// New state, e.g. `Started` or `Paid`.
    state: String,
    /// Space of the scenario.
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListCallbacksArgs {
    /// `pending`, `sent` or `failed`.
    status: Option<String>,
    /// `stubs` or a plugin name.
    source: Option<String>,
    /// Correlation id of the request that scheduled the callback.
    correlation: Option<String>,
    /// Only callbacks of this space.
    space: Option<String>,
    /// 1 to 100 (default 20). Newest first.
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportArgs {
    /// The document text: OpenAPI 3, WireMock mappings, MockServer expectations, Postman, HAR, AsyncAPI,
    /// GraphQL SDL, WSDL, SoapUI or Microcks (JSON, YAML, XML or SDL).
    document: String,
    /// Force a format instead of detecting it, e.g. `openapi`, `wiremock`, `mockserver`.
    format: Option<String>,
    /// Tag for the generated stubs (default: the format name or service id).
    tag: Option<String>,
    /// Delete the stubs carrying the tag first (default false here; the admin API defaults to true).
    replace: Option<bool>,
    /// Only return the generated stubs without saving them.
    dry_run: Option<bool>,
    /// Space for the stubs (OpenAPI, WireMock, MockServer, Postman and HAR only).
    space: Option<String>,
    /// `request.host` for imported stubs without one.
    host: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitForMailArgs {
    /// Mail matcher: `{from, to, cc, subject, text, html, headers, hasAttachment, attachmentName, label,
    /// source, provider}`, each a string matcher such as `{"contains": "alice@"}`.
    #[serde(rename = "match")]
    matcher: Map<String, Value>,
    /// Messages to wait for, 1 to 500 (default 1).
    count: Option<usize>,
    /// Longest wait in milliseconds, 0 to 30000 (default 10000).
    timeout_ms: Option<u64>,
    /// Also count messages stored before the call.
    include_existing: Option<bool>,
    /// Also count messages stored up to this many milliseconds before the call.
    lookback_ms: Option<u64>,
    /// Only messages of this space.
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadMailArgs {
    /// Message id (UUID), e.g. from `wait_for_mail`.
    id: Uuid,
    /// Space of the message.
    space: Option<String>,
    /// Regular expression for one-time codes; with a capture group the first group is the code (default:
    /// 4 to 8 digits).
    code_pattern: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FindMessagesArgs {
    /// Message matcher: `{source, protocol, topic, exchange, key, headers, properties, contentType, body}`;
    /// string conditions are string matchers such as `{"equalTo": "orders.events"}`, `body` is a list of
    /// body matchers such as `{"jsonPath": "$.status", "value": {"equalTo": "SHIPPED"}}`.
    #[serde(rename = "match", default)]
    matcher: Option<Map<String, Value>>,
    /// Only messages received at or after this time (RFC 3339).
    since: Option<String>,
    /// Messages to return, newest first, 1 to 100 (default 20).
    limit: Option<u32>,
    /// Only messages of this space.
    space: Option<String>,
    /// Include the decoded body of each returned message (at most 20 messages).
    with_bodies: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitForMessageArgs {
    /// Message matcher, as in find_messages.
    #[serde(rename = "match", default)]
    matcher: Option<Map<String, Value>>,
    /// Messages to wait for, 1 to 500 (default 1).
    count: Option<usize>,
    /// Longest wait in milliseconds, 0 to 30000 (default 10000).
    timeout_ms: Option<u64>,
    /// Also count messages captured before the call.
    include_existing: Option<bool>,
    /// Also count messages captured up to this many milliseconds before the call.
    lookback_ms: Option<u64>,
    /// Only messages of this space.
    space: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifyMessagesArgs {
    /// Message matcher, as in find_messages.
    #[serde(rename = "match", default)]
    matcher: Option<Map<String, Value>>,
    /// Exactly this many matching messages.
    count: Option<u64>,
    at_least: Option<u64>,
    at_most: Option<u64>,
    /// Only messages received at or after this time (RFC 3339).
    since: Option<String>,
    space: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Tool table
// ---------------------------------------------------------------------------------------------

struct ToolSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// The permission of the REST route the tool calls (for listing; the admin API enforces it).
    permission: Permission,
    input: fn() -> Value,
    output: fn() -> Value,
    read_only: bool,
    destructive: bool,
    module: Module,
}

/// The module a tool needs: tools of a module that is off are not listed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Module {
    Core,
    Mail,
    Capture,
}

impl Module {
    fn on(self, mail: bool, capture: bool) -> bool {
        match self {
            Module::Core => true,
            Module::Mail => mail,
            Module::Capture => capture,
        }
    }
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type": "object"}))
}

fn out_object(props: Value) -> Value {
    json!({"type": "object", "properties": props})
}

fn out_list(key: &str) -> Value {
    out_object(json!({
        key: {"type": "array", "items": {"type": "object"}},
        "total": {"type": "integer"},
        "truncated": {"type": "boolean", "description": "Items were left out to keep the answer small"},
    }))
}

fn out_any() -> Value {
    json!({"type": "object"})
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "list_stubs",
        title: "List stubs",
        description: "List the mock stubs the token can read, newest first, as compact summaries (id, name, space, enabled, priority, tags, method, URL, MCP tool or resource, response status). Filter by space, tag, enabled or a search text; page with limit/offset. Use get_stub for a complete stub. Credentials inside stubs are masked unless the token may change the stub. REST: GET /__admin/stubs.",
        permission: Permission::StubsRead,
        input: schema::<ListStubsArgs>,
        output: || {
            out_object(json!({
                "total": {"type": "integer"}, "offset": {"type": "integer"},
                "stubs": {"type": "array", "items": {"type": "object"}},
                "truncated": {"type": "boolean"},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "get_stub",
        title: "Get a stub",
        description: "One stub with its complete request pattern, responses, delays, faults, scenario, callbacks and metadata. Credentials are masked unless the token may change the stub. REST: GET /__admin/stubs/{id}.",
        permission: Permission::StubsRead,
        input: schema::<StubIdArgs>,
        output: out_any,
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "create_stub",
        title: "Create a stub",
        description: "Create a mock stub. Minimal stub: {\"request\": {\"method\": \"GET\", \"urlPath\": \"/users/1\"}, \"response\": {\"status\": 200, \"jsonBody\": {\"id\": 1}}}. Matching: urlPath, urlPathPattern (regex), urlPathTemplate (/users/{id}), query, headers, body matchers (equalToJson, jsonPath, matches, contains), mcp (tool calls). Responses support Handlebars templates such as {{request.pathParams.id}}, delays, faults and sequences. Read fastmock://guide/stubs for more. Validate first with validate_stub when unsure. Audited. REST: POST /__admin/stubs.",
        permission: Permission::StubsWrite,
        input: schema::<CreateStubArgs>,
        output: out_any,
        read_only: false,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "update_stub",
        title: "Update a stub",
        description: "Change a stub: `stub` replaces it completely (REST PUT /__admin/stubs/{id}), `patch` applies a JSON merge patch to the stored stub (REST PATCH), e.g. {\"enabled\": false} or {\"response\": {\"status\": 503}}. The space of a stub never changes. Audited with the changed fields.",
        permission: Permission::StubsWrite,
        input: schema::<UpdateStubArgs>,
        output: out_any,
        read_only: false,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "delete_stub",
        title: "Delete a stub",
        description: "Delete one stub by id. Audited. REST: DELETE /__admin/stubs/{id}.",
        permission: Permission::StubsWrite,
        input: schema::<StubIdArgs>,
        output: || out_object(json!({"deleted": {"type": "boolean"}, "id": {"type": "string"}})),
        read_only: false,
        destructive: true,
        module: Module::Core,
    },
    ToolSpec {
        name: "validate_stub",
        title: "Validate a stub",
        description: "Check a stub without saving it: JSON structure, matchers, regular expressions, templates and limits. Answers {valid, error}. REST: POST /__admin/stubs/validate.",
        permission: Permission::StubsRead,
        input: schema::<ValidateStubArgs>,
        output: || out_object(json!({"valid": {"type": "boolean"}, "error": {"type": "string"}})),
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "find_requests",
        title: "Find requests in the journal",
        description: "Search the request journal (requests the mock received, newest first) by method, path prefix, matched/unmatched, stub id, body text, correlation, source and time, or with a stub request `pattern`. Headers, query parameters and bodies are recorded with secrets redacted; bodies are cut to maxBodyBytes. REST: GET /__admin/requests, or POST /__admin/requests/find with a pattern.",
        permission: Permission::JournalRead,
        input: schema::<FindRequestsArgs>,
        output: || out_list("requests"),
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "get_request",
        title: "Get one journal entry",
        description: "One recorded request with its headers, query, body, response status and body, matched stub, space and trace id (secrets redacted, bodies cut to maxBodyBytes). REST: GET /__admin/requests/{id}.",
        permission: Permission::JournalRead,
        input: schema::<GetRequestArgs>,
        output: out_any,
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "near_misses",
        title: "Explain a non-matching request",
        description: "Why does a request not match any stub? Returns the five closest stubs with a score and the matchers that failed. Give a journal `requestId` (e.g. an unmatched entry from find_requests with matched=false) or a `request` {method, path, query, headers, body}. REST: POST /__admin/requests/near-misses.",
        permission: Permission::JournalRead,
        input: schema::<NearMissesArgs>,
        output: || {
            out_object(json!({"nearMisses": {"type": "array", "items": {"type": "object"}}}))
        },
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "verify",
        title: "Verify received requests",
        description: "Check how often requests matching a stub request pattern were received: exactly `count`, or between `atLeast` and `atMost` (default: at least 1). Answers {verified, count, expected} and, when the expectation fails, the closest non-matching requests with what differed. REST: POST /__admin/verify.",
        permission: Permission::JournalRead,
        input: schema::<VerifyArgs>,
        output: || {
            out_object(json!({
                "verified": {"type": "boolean"}, "count": {"type": "integer"},
                "expected": {"type": "string"}, "error": {"type": "string"},
                "details": {"type": "array", "items": {"type": "string"}},
                "nearMisses": {"type": "array", "items": {"type": "object"}},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "reset_journal",
        title: "Clear the request journal",
        description: "Delete the recorded requests of `space`, or of every area the token may delete from. Use before a test run to start from an empty journal. Audited. REST: DELETE /__admin/requests.",
        permission: Permission::JournalDelete,
        input: schema::<SpaceArgs>,
        output: || out_object(json!({"deleted": {"type": "integer"}})),
        read_only: false,
        destructive: true,
        module: Module::Core,
    },
    ToolSpec {
        name: "get_scenarios",
        title: "Read scenario states",
        description: "Current state of every scenario (stateful stub sequences: stubs with scenario.requiredState and scenario.newState). REST: GET /__admin/scenarios.",
        permission: Permission::StateRead,
        input: schema::<SpaceArgs>,
        output: || out_list("scenarios"),
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "set_scenario",
        title: "Set a scenario state",
        description: "Move a scenario to a state, e.g. back to `Started` or ahead to `Paid`, so that the stubs requiring that state answer next. Audited. REST: PUT /__admin/scenarios/{name}.",
        permission: Permission::StateWrite,
        input: schema::<SetScenarioArgs>,
        output: || out_object(json!({"name": {"type": "string"}, "state": {"type": "string"}})),
        read_only: false,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "list_callbacks",
        title: "List callback deliveries",
        description: "Outgoing callbacks (webhooks) the mock scheduled, newest first, with status pending/sent/failed, attempts, last error and response. Sensitive headers and body fields are masked. REST: GET /__admin/callbacks.",
        permission: Permission::CallbacksRead,
        input: schema::<ListCallbacksArgs>,
        output: || out_list("callbacks"),
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "import",
        title: "Import an API description",
        description: "Generate stubs from an API document: OpenAPI 3, WireMock mappings, MockServer expectations, Postman, HAR, AsyncAPI, GraphQL SDL, WSDL, SoapUI or Microcks; the format is detected. Use dryRun to preview the stubs. Existing stubs with the same tag are kept unless replace is true. Audited. REST: POST /__admin/import.",
        permission: Permission::ImportsRun,
        input: schema::<ImportArgs>,
        output: out_any,
        read_only: false,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "get_chaos",
        title: "Read chaos rules",
        description: "Active chaos rules: injected delays, error statuses and faults for a share of matching traffic. REST: GET /__admin/chaos.",
        permission: Permission::ChaosRead,
        input: schema::<NoArgs>,
        output: || out_list("rules"),
        read_only: true,
        destructive: false,
        module: Module::Core,
    },
    ToolSpec {
        name: "wait_for_mail",
        title: "Wait for an email",
        description: "Wait until `count` emails matching `match` were captured (SMTP or provider APIs) and return their summaries (id, from, to, subject, labels). Times out after timeoutMs with {timedOut: true} and what was seen. Then read one with read_mail. REST: POST /__admin/mail/messages/wait.",
        permission: Permission::MailRead,
        input: schema::<WaitForMailArgs>,
        output: || {
            out_object(json!({
                "messages": {"type": "array", "items": {"type": "object"}},
                "count": {"type": "integer"}, "timedOut": {"type": "boolean"},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Mail,
    },
    ToolSpec {
        name: "read_mail",
        title: "Read an email",
        description: "One captured email: envelope, headers, text body, attachments, the http(s) links in it and one-time codes found in the subject and text. REST: GET /__admin/mail/messages/{id}, /links and /codes.",
        permission: Permission::MailRead,
        input: schema::<ReadMailArgs>,
        output: || {
            out_object(json!({
                "message": {"type": "object"},
                "links": {"type": "array", "items": {"type": "object"}},
                "codes": {"type": "array", "items": {"type": "string"}},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Mail,
    },
    ToolSpec {
        name: "find_messages",
        title: "Find captured broker messages",
        description: "Messages FastMock captured from the Kafka topics, MQTT topic filters and AMQP exchanges of its capture sources (what the application under test published), newest first, filtered by `match`: source, protocol, topic (AMQP: routing key), exchange, key, headers, properties (correlationId, messageId, qos, partition, ...), contentType and body matchers. withBodies adds the decoded bodies (JSON, text, Avro or base64). REST: POST /__admin/capture/messages/find and GET /__admin/capture/messages/{id}.",
        permission: Permission::MessagesRead,
        input: schema::<FindMessagesArgs>,
        output: || {
            out_object(json!({
                "messages": {"type": "array", "items": {"type": "object"}},
                "count": {"type": "integer"},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Capture,
    },
    ToolSpec {
        name: "wait_for_message",
        title: "Wait for a broker message",
        description: "Wait until `count` captured broker messages matching `match` arrive (Kafka, MQTT or AMQP, see find_messages for the matcher) and return them with their decoded bodies. By default only messages arriving after the call count; use includeExisting or lookbackMs when the application may already have published. Times out after timeoutMs with {timedOut: true} and what was seen. REST: POST /__admin/capture/messages/wait.",
        permission: Permission::MessagesRead,
        input: schema::<WaitForMessageArgs>,
        output: || {
            out_object(json!({
                "messages": {"type": "array", "items": {"type": "object"}},
                "count": {"type": "integer"}, "timedOut": {"type": "boolean"},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Capture,
    },
    ToolSpec {
        name: "verify_messages",
        title: "Verify captured broker messages",
        description: "Check how many captured broker messages match `match`: exactly `count`, or between `atLeast` and `atMost` (default: at least 1). Answers {verified, count, expected} and, when the expectation fails, the closest messages with the conditions they failed (topic, headers.x, body[0], ...). REST: POST /__admin/capture/verify.",
        permission: Permission::MessagesRead,
        input: schema::<VerifyMessagesArgs>,
        output: || {
            out_object(json!({
                "verified": {"type": "boolean"}, "count": {"type": "integer"},
                "expected": {"type": "string"}, "error": {"type": "string"},
                "details": {"type": "array", "items": {"type": "string"}},
                "nearMisses": {"type": "array", "items": {"type": "object"}},
            }))
        },
        read_only: true,
        destructive: false,
        module: Module::Capture,
    },
];

/// The tools a principal may use: those whose REST permission it holds somewhere.
pub fn tools_for(p: Option<&Principal>, mail: bool, capture: bool) -> Vec<Tool> {
    TOOLS
        .iter()
        .filter(|t| t.module.on(mail, capture))
        .filter(|t| p.is_none_or(|p| p.permissions.anywhere(t.permission)))
        .map(tool_def)
        .collect()
}

fn as_object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn tool_def(t: &ToolSpec) -> Tool {
    let mut tool = Tool::new(t.name, t.description, Arc::new(as_object((t.input)())));
    tool.title = Some(t.title.to_string());
    tool.output_schema = Some(Arc::new(as_object((t.output)())));
    let mut a = ToolAnnotations::new()
        .read_only(t.read_only)
        .open_world(false);
    if !t.read_only {
        a = a.destructive(t.destructive);
    }
    tool.annotations = Some(a);
    tool
}

// ---------------------------------------------------------------------------------------------
// Calling the admin API
// ---------------------------------------------------------------------------------------------

/// A failed tool call.
#[derive(Debug)]
enum ToolError {
    /// The arguments are not valid for the tool.
    Invalid(String),
    /// The admin API refused the request.
    Rest(StatusCode, Value),
    /// The call could not be made.
    Internal(String),
}

impl ToolError {
    fn outcome(&self) -> &'static str {
        match self {
            ToolError::Invalid(_) => "invalid",
            ToolError::Rest(s, _)
                if *s == StatusCode::UNAUTHORIZED || *s == StatusCode::FORBIDDEN =>
            {
                "denied"
            }
            ToolError::Rest(s, _) if s.is_client_error() => "rejected",
            ToolError::Rest(..) | ToolError::Internal(_) => "error",
        }
    }

    fn text(&self) -> String {
        match self {
            ToolError::Invalid(m) => format!("invalid arguments: {m}"),
            ToolError::Internal(m) => m.clone(),
            ToolError::Rest(status, body) => {
                let mut msg = format!("HTTP {}", status.as_u16());
                if let Some(e) = body.get("error").and_then(Value::as_str) {
                    msg.push_str(": ");
                    msg.push_str(e);
                }
                if let Some(d) = body.get("details").and_then(Value::as_array) {
                    for x in d.iter().filter_map(Value::as_str) {
                        msg.push_str("; ");
                        msg.push_str(x);
                    }
                }
                if let Some(r) = body.get("required").and_then(Value::as_str) {
                    msg.push_str(&format!(" (required: {r})"));
                }
                if body.get("error").is_none() && !body.is_null() {
                    msg.push_str(": ");
                    msg.push_str(&clip(&body.to_string(), 2048));
                }
                msg
            }
        }
    }
}

type ToolResult = Result<Value, ToolError>;

enum RestBody {
    None,
    Json(Value),
    Text(String),
}

fn args<T: DeserializeOwned>(v: &Value) -> Result<T, ToolError> {
    serde_json::from_value(v.clone()).map_err(|e| ToolError::Invalid(e.to_string()))
}

fn limit(v: Option<u32>, default: u32, max: u32) -> Result<u32, ToolError> {
    match v {
        None => Ok(default),
        Some(n) if (1..=max).contains(&n) => Ok(n),
        Some(_) => Err(ToolError::Invalid(format!("limit must be 1 to {max}"))),
    }
}

fn body_bytes(v: Option<usize>, default: usize) -> Result<usize, ToolError> {
    match v {
        None => Ok(default),
        Some(n) if n <= MAX_BODY_BYTES => Ok(n),
        Some(_) => Err(ToolError::Invalid(format!(
            "maxBodyBytes must be 0 to {MAX_BODY_BYTES}"
        ))),
    }
}

/// Cuts a string at a character boundary with the journal's truncation marker.
fn clip(s: &str, max: usize) -> String {
    crate::redact::truncate(s.to_string(), max)
}

fn path_segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

struct Query(Vec<(&'static str, String)>);

impl Query {
    fn new() -> Self {
        Query(Vec::new())
    }

    fn opt(mut self, k: &'static str, v: Option<impl ToString>) -> Self {
        if let Some(v) = v {
            self.0.push((k, v.to_string()));
        }
        self
    }

    fn with(self, k: &'static str, v: impl ToString) -> Self {
        self.opt(k, Some(v))
    }

    fn to(&self, path: &str) -> String {
        if self.0.is_empty() {
            return path.to_string();
        }
        let mut s = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &self.0 {
            s.append_pair(k, v);
        }
        format!("{path}?{}", s.finish())
    }
}

/// The MCP server of one HTTP request.
#[derive(Clone)]
pub struct McpServer {
    app: AppState,
    api: Router,
}

struct Call {
    headers: HeaderMap,
}

impl McpServer {
    async fn rest(
        &self,
        call: &Call,
        method: Method,
        uri: String,
        body: RestBody,
    ) -> Result<(StatusCode, Value), ToolError> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(h) = b.headers_mut() {
            for name in &FORWARDED {
                for v in call.headers.get_all(name) {
                    h.append(name.clone(), v.clone());
                }
            }
        }
        let body = match body {
            RestBody::None => Body::empty(),
            RestBody::Json(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap_or_default())
            }
            RestBody::Text(t) => {
                b = b.header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
                Body::from(t)
            }
        };
        let req = b
            .body(body)
            .map_err(|e| ToolError::Internal(format!("building the request failed: {e}")))?;
        let resp = match self.api.clone().oneshot(req).await {
            Ok(r) => r,
            Err(e) => match e {},
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), MAX_REST_RESPONSE)
            .await
            .map_err(|_| {
                ToolError::Internal(
                    "the answer is too large; narrow the request (space, filters, limit)".into(),
                )
            })?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Ok((status, value))
    }

    /// A REST call whose 2xx answer is the result.
    async fn ok(&self, call: &Call, method: Method, uri: String, body: RestBody) -> ToolResult {
        let (status, v) = self.rest(call, method, uri, body).await?;
        if status.is_success() {
            Ok(v)
        } else {
            Err(ToolError::Rest(status, v))
        }
    }

    async fn run(&self, name: &str, a: &Value, call: &Call) -> ToolResult {
        match name {
            "list_stubs" => {
                let x: ListStubsArgs = args(a)?;
                let limit = limit(x.limit, 25, 100)?;
                let q = Query::new()
                    .opt("space", x.space)
                    .opt("tag", x.tag)
                    .opt("q", x.query)
                    .opt("enabled", x.enabled)
                    .with("limit", limit)
                    .opt("offset", x.offset);
                let v = self
                    .ok(call, Method::GET, q.to("/stubs"), RestBody::None)
                    .await?;
                let stubs: Vec<Value> = v
                    .get("stubs")
                    .and_then(Value::as_array)
                    .map(|l| l.iter().map(stub_summary).collect())
                    .unwrap_or_default();
                Ok(json!({"total": v["total"], "offset": v["offset"], "stubs": stubs}))
            }
            "get_stub" => {
                let x: StubIdArgs = args(a)?;
                self.ok(
                    call,
                    Method::GET,
                    format!("/stubs/{}", x.id),
                    RestBody::None,
                )
                .await
            }
            "create_stub" => {
                let x: CreateStubArgs = args(a)?;
                let q = Query::new().opt("space", x.space);
                self.ok(
                    call,
                    Method::POST,
                    q.to("/stubs"),
                    RestBody::Json(Value::Object(x.stub)),
                )
                .await
            }
            "update_stub" => {
                let x: UpdateStubArgs = args(a)?;
                let (method, body) = match (x.stub, x.patch) {
                    (Some(s), None) => (Method::PUT, s),
                    (None, Some(p)) => (Method::PATCH, p),
                    _ => return Err(ToolError::Invalid("give either stub or patch".into())),
                };
                self.ok(
                    call,
                    method,
                    format!("/stubs/{}", x.id),
                    RestBody::Json(Value::Object(body)),
                )
                .await
            }
            "delete_stub" => {
                let x: StubIdArgs = args(a)?;
                self.ok(
                    call,
                    Method::DELETE,
                    format!("/stubs/{}", x.id),
                    RestBody::None,
                )
                .await?;
                Ok(json!({"deleted": true, "id": x.id}))
            }
            "validate_stub" => {
                let x: ValidateStubArgs = args(a)?;
                let (status, v) = self
                    .rest(
                        call,
                        Method::POST,
                        "/stubs/validate".into(),
                        RestBody::Json(Value::Object(x.stub)),
                    )
                    .await?;
                match status {
                    s if s.is_success() => Ok(v),
                    StatusCode::BAD_REQUEST => Ok(json!({
                        "valid": false,
                        "error": v.get("error").cloned().unwrap_or(Value::Null),
                    })),
                    s => Err(ToolError::Rest(s, v)),
                }
            }
            "find_requests" => {
                let x: FindRequestsArgs = args(a)?;
                let limit = limit(x.limit, 20, 100)?;
                let max = body_bytes(x.max_body_bytes, DEFAULT_BODY_BYTES)?;
                let mut v = match x.pattern {
                    Some(p) => {
                        if x.method.is_some()
                            || x.path_prefix.is_some()
                            || x.matched.is_some()
                            || x.stub_id.is_some()
                            || x.body_contains.is_some()
                            || x.correlation.is_some()
                            || x.offset.is_some()
                        {
                            return Err(ToolError::Invalid(
                                "with pattern, only space, source, since, limit and maxBodyBytes apply".into(),
                            ));
                        }
                        let q = Query::new()
                            .opt("space", x.space)
                            .opt("source", x.source)
                            .opt("since", x.since)
                            .with("limit", limit);
                        self.ok(
                            call,
                            Method::POST,
                            q.to("/requests/find"),
                            RestBody::Json(Value::Object(p)),
                        )
                        .await?
                    }
                    None => {
                        let q = Query::new()
                            .opt("space", x.space)
                            .opt("method", x.method)
                            .opt("pathPrefix", x.path_prefix)
                            .opt("matched", x.matched)
                            .opt("stubId", x.stub_id)
                            .opt("bodyContains", x.body_contains)
                            .opt("correlation", x.correlation)
                            .opt("source", x.source)
                            .opt("since", x.since)
                            .with("limit", limit)
                            .opt("offset", x.offset);
                        self.ok(call, Method::GET, q.to("/requests"), RestBody::None)
                            .await?
                    }
                };
                if let Some(list) = v.get_mut("requests").and_then(Value::as_array_mut) {
                    for e in list {
                        clip_entry(e, max);
                    }
                }
                Ok(v)
            }
            "get_request" => {
                let x: GetRequestArgs = args(a)?;
                let max = body_bytes(x.max_body_bytes, MAX_STRING)?;
                let mut v = self
                    .ok(
                        call,
                        Method::GET,
                        format!("/requests/{}", x.id),
                        RestBody::None,
                    )
                    .await?;
                clip_entry(&mut v, max);
                Ok(v)
            }
            "near_misses" => {
                let x: NearMissesArgs = args(a)?;
                let req = match (x.request_id, x.request) {
                    (Some(id), None) => {
                        let e = self
                            .ok(call, Method::GET, format!("/requests/{id}"), RestBody::None)
                            .await?;
                        json!({
                            "method": e["method"], "path": e["path"],
                            "query": e["query"].as_str().filter(|q| !q.is_empty()),
                            "headers": e["headers"], "body": e["body"],
                        })
                    }
                    (None, Some(r)) => json!({
                        "method": r.method, "path": r.path, "query": r.query,
                        "headers": r.headers, "body": r.body,
                    }),
                    _ => {
                        return Err(ToolError::Invalid(
                            "give either requestId or request".into(),
                        ));
                    }
                };
                let q = Query::new().opt("space", x.space);
                self.ok(
                    call,
                    Method::POST,
                    q.to("/requests/near-misses"),
                    RestBody::Json(req),
                )
                .await
            }
            "verify" => {
                let x: VerifyArgs = args(a)?;
                let mut body = json!({"request": Value::Object(x.request)});
                for (k, v) in [
                    ("count", x.count),
                    ("atLeast", x.at_least),
                    ("atMost", x.at_most),
                ] {
                    if let Some(v) = v {
                        body[k] = json!(v);
                    }
                }
                if let Some(s) = x.since {
                    body["since"] = json!(s);
                }
                if let Some(s) = x.source {
                    body["source"] = json!(s);
                }
                let q = Query::new().opt("space", x.space);
                let (status, v) = self
                    .rest(call, Method::POST, q.to("/verify"), RestBody::Json(body))
                    .await?;
                if status.is_success() || v.get("verified").is_some() {
                    Ok(v)
                } else {
                    Err(ToolError::Rest(status, v))
                }
            }
            "reset_journal" => {
                let x: SpaceArgs = args(a)?;
                let q = Query::new().opt("space", x.space);
                self.ok(call, Method::DELETE, q.to("/requests"), RestBody::None)
                    .await
            }
            "get_scenarios" => {
                let x: SpaceArgs = args(a)?;
                let q = Query::new().opt("space", x.space);
                self.ok(call, Method::GET, q.to("/scenarios"), RestBody::None)
                    .await
            }
            "set_scenario" => {
                let x: SetScenarioArgs = args(a)?;
                let q = Query::new().opt("space", x.space);
                self.ok(
                    call,
                    Method::PUT,
                    q.to(&format!("/scenarios/{}", path_segment(&x.name))),
                    RestBody::Json(json!({"state": x.state})),
                )
                .await?;
                Ok(json!({"name": x.name, "state": x.state}))
            }
            "list_callbacks" => {
                let x: ListCallbacksArgs = args(a)?;
                let limit = limit(x.limit, 20, 100)?;
                let q = Query::new()
                    .opt("status", x.status)
                    .opt("source", x.source)
                    .opt("correlation", x.correlation)
                    .opt("space", x.space)
                    .with("limit", limit);
                self.ok(call, Method::GET, q.to("/callbacks"), RestBody::None)
                    .await
            }
            "import" => {
                let x: ImportArgs = args(a)?;
                let q = Query::new()
                    .opt("format", x.format)
                    .opt("tag", x.tag)
                    .with("replace", x.replace.unwrap_or(false))
                    .opt("dryRun", x.dry_run)
                    .opt("space", x.space)
                    .opt("host", x.host);
                self.ok(
                    call,
                    Method::POST,
                    q.to("/import"),
                    RestBody::Text(x.document),
                )
                .await
            }
            "get_chaos" => {
                let _: NoArgs = args(a)?;
                self.ok(call, Method::GET, "/chaos".into(), RestBody::None)
                    .await
            }
            "wait_for_mail" => {
                let x: WaitForMailArgs = args(a)?;
                let timeout = x.timeout_ms.unwrap_or(10_000);
                if timeout > MAX_MAIL_WAIT_MS {
                    return Err(ToolError::Invalid(format!(
                        "timeoutMs must be 0 to {MAX_MAIL_WAIT_MS}"
                    )));
                }
                let mut body = json!({
                    "match": Value::Object(x.matcher),
                    "count": x.count.unwrap_or(1),
                    "timeoutMs": timeout,
                });
                if let Some(v) = x.include_existing {
                    body["includeExisting"] = json!(v);
                }
                if let Some(v) = x.lookback_ms {
                    body["lookbackMs"] = json!(v);
                }
                let q = Query::new().opt("space", x.space);
                let (status, mut v) = self
                    .rest(
                        call,
                        Method::POST,
                        q.to("/mail/messages/wait"),
                        RestBody::Json(body),
                    )
                    .await?;
                match status {
                    s if s.is_success() => {
                        v["timedOut"] = json!(false);
                        Ok(v)
                    }
                    StatusCode::REQUEST_TIMEOUT => {
                        v["timedOut"] = json!(true);
                        Ok(v)
                    }
                    s => Err(ToolError::Rest(s, v)),
                }
            }
            "read_mail" => {
                let x: ReadMailArgs = args(a)?;
                let q = Query::new().opt("space", x.space.clone());
                let base = format!("/mail/messages/{}", x.id);
                let message = self
                    .ok(call, Method::GET, q.to(&base), RestBody::None)
                    .await?;
                let links = self
                    .ok(
                        call,
                        Method::GET,
                        q.to(&format!("{base}/links")),
                        RestBody::None,
                    )
                    .await?;
                let codes = self
                    .ok(
                        call,
                        Method::GET,
                        Query::new()
                            .opt("space", x.space)
                            .opt("pattern", x.code_pattern)
                            .to(&format!("{base}/codes")),
                        RestBody::None,
                    )
                    .await?;
                Ok(json!({
                    "message": message,
                    "links": links.get("links").cloned().unwrap_or(json!([])),
                    "codes": codes.get("codes").cloned().unwrap_or(json!([])),
                }))
            }
            "find_messages" => {
                let x: FindMessagesArgs = args(a)?;
                let limit = limit(x.limit, 20, 100)?;
                let mut body = json!({
                    "match": Value::Object(x.matcher.unwrap_or_default()),
                    "limit": limit,
                });
                if let Some(s) = x.since {
                    body["since"] = json!(s);
                }
                let q = Query::new().opt("space", x.space.clone());
                let mut v = self
                    .ok(
                        call,
                        Method::POST,
                        q.to("/capture/messages/find"),
                        RestBody::Json(body),
                    )
                    .await?;
                if x.with_bodies == Some(true) {
                    self.add_bodies(call, &mut v, x.space).await?;
                }
                Ok(v)
            }
            "wait_for_message" => {
                let x: WaitForMessageArgs = args(a)?;
                let timeout = x.timeout_ms.unwrap_or(10_000);
                if timeout > MAX_MAIL_WAIT_MS {
                    return Err(ToolError::Invalid(format!(
                        "timeoutMs must be 0 to {MAX_MAIL_WAIT_MS}"
                    )));
                }
                let mut body = json!({
                    "match": Value::Object(x.matcher.unwrap_or_default()),
                    "count": x.count.unwrap_or(1),
                    "timeoutMs": timeout,
                });
                if let Some(v) = x.include_existing {
                    body["includeExisting"] = json!(v);
                }
                if let Some(v) = x.lookback_ms {
                    body["lookbackMs"] = json!(v);
                }
                let q = Query::new().opt("space", x.space.clone());
                let (status, mut v) = self
                    .rest(
                        call,
                        Method::POST,
                        q.to("/capture/messages/wait"),
                        RestBody::Json(body),
                    )
                    .await?;
                match status {
                    s if s.is_success() => {
                        self.add_bodies(call, &mut v, x.space).await?;
                        v["timedOut"] = json!(false);
                        Ok(v)
                    }
                    StatusCode::REQUEST_TIMEOUT => {
                        v["timedOut"] = json!(true);
                        Ok(v)
                    }
                    s => Err(ToolError::Rest(s, v)),
                }
            }
            "verify_messages" => {
                let x: VerifyMessagesArgs = args(a)?;
                let mut body = json!({"match": Value::Object(x.matcher.unwrap_or_default())});
                for (k, v) in [
                    ("count", x.count),
                    ("atLeast", x.at_least),
                    ("atMost", x.at_most),
                ] {
                    if let Some(v) = v {
                        body[k] = json!(v);
                    }
                }
                if let Some(s) = x.since {
                    body["since"] = json!(s);
                }
                let q = Query::new().opt("space", x.space);
                let (status, v) = self
                    .rest(
                        call,
                        Method::POST,
                        q.to("/capture/verify"),
                        RestBody::Json(body),
                    )
                    .await?;
                if status.is_success() || v.get("verified").is_some() {
                    Ok(v)
                } else {
                    Err(ToolError::Rest(status, v))
                }
            }
            other => Err(ToolError::Invalid(format!("unknown tool {other:?}"))),
        }
    }

    /// Adds the decoded body to the first 20 messages of a result.
    async fn add_bodies(
        &self,
        call: &Call,
        v: &mut Value,
        space: Option<String>,
    ) -> Result<(), ToolError> {
        let Some(list) = v.get_mut("messages").and_then(Value::as_array_mut) else {
            return Ok(());
        };
        for m in list.iter_mut().take(20) {
            let Some(id) = m.get("id").and_then(Value::as_str).map(String::from) else {
                continue;
            };
            let q = Query::new().opt("space", space.clone());
            let detail = self
                .ok(
                    call,
                    Method::GET,
                    q.to(&format!("/capture/messages/{id}")),
                    RestBody::None,
                )
                .await?;
            if let Some(b) = detail.get("body") {
                let mut b = b.clone();
                if let Some(s) = b.get("value").and_then(Value::as_str)
                    && s.len() > MAX_STRING
                {
                    b["value"] = json!(clip(s, MAX_STRING));
                }
                m["body"] = b;
            }
        }
        Ok(())
    }
}

/// A short view of a stub for lists.
fn stub_summary(s: &Value) -> Value {
    let r = &s["request"];
    let mut out =
        json!({"id": s["id"], "enabled": s.get("enabled").cloned().unwrap_or(json!(true))});
    for k in ["name", "space", "priority", "tags"] {
        if let Some(v) = s.get(k) {
            out[k] = v.clone();
        }
    }
    let mut req = json!({});
    for k in [
        "method",
        "url",
        "urlPattern",
        "urlPath",
        "urlPathPattern",
        "urlPathTemplate",
        "mcp",
    ] {
        if let Some(v) = r.get(k) {
            req[k] = v.clone();
        }
    }
    out["request"] = req;
    match s.get("responses").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => {
            out["responses"] = json!(list.len());
            out["status"] = list[0].get("status").cloned().unwrap_or(json!(200));
        }
        _ => out["status"] = s["response"].get("status").cloned().unwrap_or(json!(200)),
    }
    out
}

/// Cuts the request and response bodies of a journal entry.
fn clip_entry(e: &mut Value, max: usize) {
    for k in ["body", "responseBody"] {
        if let Some(Value::String(s)) = e.get_mut(k)
            && s.len() > max
        {
            *s = clip(s, max);
        }
    }
}

/// Bounds a tool result: long strings are cut, then items of the largest top-level list are left out
/// until the result fits [`MAX_OUTPUT`].
pub fn bound(mut v: Value) -> Value {
    fn strings(v: &mut Value) {
        match v {
            Value::String(s) if s.len() > MAX_STRING => *s = clip(s, MAX_STRING),
            Value::Array(a) => a.iter_mut().for_each(strings),
            Value::Object(m) => m.values_mut().for_each(strings),
            _ => {}
        }
    }
    strings(&mut v);
    let mut size = serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(0);
    if size <= MAX_OUTPUT {
        return v;
    }
    if let Value::Object(m) = &mut v {
        let key = m
            .iter()
            .filter_map(|(k, x)| x.as_array().map(|a| (k.clone(), a.len())))
            .max_by_key(|(_, n)| *n)
            .map(|(k, _)| k);
        if let Some(key) = key {
            let mut omitted = 0usize;
            while size > MAX_OUTPUT {
                let Some(Value::Array(a)) = m.get_mut(&key) else {
                    break;
                };
                let Some(last) = a.pop() else {
                    break;
                };
                omitted += 1;
                size = size
                    .saturating_sub(serde_json::to_vec(&last).map(|b| b.len() + 1).unwrap_or(1));
            }
            m.insert("truncated".into(), json!(true));
            m.insert("omitted".into(), json!(omitted));
            if size <= MAX_OUTPUT {
                return v;
            }
        }
    }
    json!({
        "truncated": true,
        "error": format!("the result is larger than {MAX_OUTPUT} bytes; narrow it with filters or a smaller limit"),
    })
}

fn stub_schema() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let spec = serde_json::to_value(crate::openapi_doc::spec()).unwrap_or_default();
        let all = spec["components"]["schemas"].clone();
        let mut keep = BTreeSet::new();
        let mut todo = vec!["Stub".to_string()];
        while let Some(n) = todo.pop() {
            if !keep.insert(n.clone()) {
                continue;
            }
            let mut refs = Vec::new();
            collect_refs(&all[&n], &mut refs);
            todo.extend(refs);
        }
        let defs: Map<String, Value> = keep
            .into_iter()
            .filter_map(|k| all.get(&k).cloned().map(|v| (k, v)))
            .collect();
        let text = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "FastMock stub",
            "$ref": "#/$defs/Stub",
            "$defs": defs,
        })
        .to_string();
        text.replace("#/components/schemas/", "#/$defs/")
    })
}

fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(r) = m.get("$ref").and_then(Value::as_str)
                && let Some(n) = r.strip_prefix("#/components/schemas/")
            {
                out.push(n.to_string());
            }
            m.values().for_each(|x| collect_refs(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

impl McpServer {
    fn call(ctx: &RequestContext<RoleServer>) -> Call {
        Call {
            headers: ctx
                .extensions
                .get::<Parts>()
                .map(|p| p.headers.clone())
                .unwrap_or_default(),
        }
    }

    fn principal(ctx: &RequestContext<RoleServer>) -> Option<Principal> {
        ctx.extensions
            .get::<Parts>()
            .and_then(|p| p.extensions.get::<Principal>())
            .cloned()
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("fastmock", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "FastMock is an API mock server. Use these tools to create and change mock stubs, search the request journal, explain why a request did not match (near_misses), verify that requests were received, manage scenario states, read callback deliveries, import API descriptions and, when mail capture is on, wait for and read emails. Read the resource fastmock://guide/stubs before writing stubs. Every tool acts with your token's permissions; changes are audited.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let p = Self::principal(&context);
        Ok(ListToolsResult::with_all_items(tools_for(
            p.as_ref(),
            self.app.mail.is_some(),
            self.app.capture.is_some(),
        )))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(spec) = TOOLS.iter().find(|t| {
            t.name == request.name
                && t.module
                    .on(self.app.mail.is_some(), self.app.capture.is_some())
        }) else {
            metrics::counter!("fastmock_mcp_tool_calls_total", "tool" => "unknown", "outcome" => "unknown_tool").increment(1);
            return Err(McpError::invalid_params(
                format!("unknown tool {:?}", request.name),
                None,
            ));
        };
        let call = Self::call(&context);
        let a = Value::Object(request.arguments.clone().unwrap_or_default());
        let span = tracing::info_span!(
            "fastmock.mcp.tool",
            otel.name = %format!("mcp {}", spec.name),
            mcp.tool = spec.name,
            mcp.outcome = tracing::field::Empty,
        );
        let started = Instant::now();
        let result = TOOL
            .scope(spec.name, self.run(spec.name, &a, &call))
            .instrument(span.clone())
            .await;
        let outcome = match &result {
            Ok(_) => "ok",
            Err(e) => e.outcome(),
        };
        span.record("mcp.outcome", outcome);
        metrics::counter!("fastmock_mcp_tool_calls_total", "tool" => spec.name, "outcome" => outcome)
            .increment(1);
        metrics::histogram!("fastmock_mcp_tool_duration_seconds", "tool" => spec.name)
            .record(started.elapsed().as_secs_f64());
        let r = match result {
            Ok(v) => CallToolResult::structured(bound(match v {
                Value::Object(_) => v,
                other => json!({"result": other}),
            })),
            Err(e) => {
                if let ToolError::Internal(m) = &e {
                    tracing::warn!(tool = spec.name, error = %m, "MCP tool failed");
                }
                CallToolResult::error(vec![ContentBlock::text(e.text())])
            }
        };
        Ok(r.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(GUIDE_URI, "stub-guide")
                    .with_title("How to write a FastMock stub")
                    .with_description(
                        "Stub structure, request matching, responses, templates, faults, scenarios, MCP tool stubs and verification, with examples.",
                    )
                    .with_mime_type("text/markdown"),
            Resource::new(SCHEMA_URI, "stub-schema")
                    .with_title("FastMock stub JSON Schema")
                    .with_description("JSON Schema of a stub as accepted by create_stub.")
                    .with_mime_type("application/schema+json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let (text, mime) = match request.uri.as_str() {
            GUIDE_URI => (GUIDE.to_string(), "text/markdown"),
            SCHEMA_URI => (stub_schema().to_string(), "application/schema+json"),
            other => {
                return Err(McpError::resource_not_found(
                    format!("unknown resource {other}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri.clone()).with_mime_type(mime),
        ])
        .into())
    }
}

/// `/mcp` behind the admin API's authentication and origin check; `api` is the admin API router the
/// tools call.
pub fn router(app: AppState, api: Router) -> Router<AppState> {
    let cfg = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .disable_allowed_hosts()
        .with_max_request_body_bytes(MAX_BODY);
    let st = app.clone();
    let svc = StreamableHttpService::new(
        move || {
            Ok(McpServer {
                app: st.clone(),
                api: api.clone(),
            })
        },
        Arc::new(NeverSessionManager::default()),
        cfg,
    );
    Router::new()
        .route_service("/mcp", svc)
        .layer(middleware::from_fn_with_state(app, admin_auth))
        .layer(middleware::map_response(no_store))
}

async fn no_store(mut r: axum::response::Response) -> axum::response::Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_cuts_strings_and_lists() {
        let v = bound(json!({"a": "x".repeat(MAX_STRING + 10)}));
        assert!(v["a"].as_str().unwrap().contains("truncated"));
        let items: Vec<Value> = (0..400)
            .map(|i| json!({"i": i, "s": "y".repeat(4000)}))
            .collect();
        let v = bound(json!({"requests": items, "total": 400}));
        assert_eq!(v["truncated"], true);
        assert!(v["omitted"].as_u64().unwrap() > 0);
        assert!(serde_json::to_vec(&v).unwrap().len() <= MAX_OUTPUT);
        assert_eq!(v["total"], 400);
    }

    #[test]
    fn audit_detail_marks_mcp_changes() {
        assert_eq!(audit_detail(json!({"id": 1})), json!({"id": 1}));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let v = rt.block_on(TOOL.scope("create_stub", async { audit_detail(json!({"id": 1})) }));
        assert_eq!(v, json!({"id": 1, "via": "mcp", "mcpTool": "create_stub"}));
    }

    #[test]
    fn every_tool_has_schemas_and_a_description() {
        let mut names = BTreeSet::new();
        for t in TOOLS {
            assert!(names.insert(t.name), "{}", t.name);
            assert!(t.description.len() > 40, "{}", t.name);
            assert_eq!((t.input)()["type"], "object", "{}", t.name);
            assert_eq!((t.output)()["type"], "object", "{}", t.name);
        }
        assert_eq!(tools_for(None, false, false).len(), TOOLS.len() - 5);
        assert_eq!(tools_for(None, true, true).len(), TOOLS.len());
    }

    #[test]
    fn stub_schema_is_self_contained() {
        let s: Value = serde_json::from_str(stub_schema()).unwrap();
        assert!(s["$defs"]["Stub"].is_object());
        assert!(s["$defs"]["RequestPattern"].is_object());
        assert!(!stub_schema().contains("#/components/"));
    }

    #[test]
    fn summaries_keep_the_essentials() {
        let s = summary_of(
            json!({"id": "x", "name": "n", "request": {"method": "GET", "urlPath": "/a", "headers": {}}, "response": {"status": 201, "body": "long"}}),
        );
        assert_eq!(s["request"], json!({"method": "GET", "urlPath": "/a"}));
        assert_eq!(s["status"], 201);
        assert!(s.get("response").is_none());
    }

    fn summary_of(v: Value) -> Value {
        stub_summary(&v)
    }
}
