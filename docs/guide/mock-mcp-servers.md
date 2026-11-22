# Mock MCP servers

Test your AI agent against a fake [MCP](https://modelcontextprotocol.io/) server the way you test an application
against a fake HTTP API. Stubs with an `mcp` matcher turn their path on the data port into an MCP server
(streamable HTTP): FastMock answers `initialize`, `tools/list` and `resources/list` from those stubs, and each
`tools/call` or `resources/read` is matched like any request, so templates, delays, faults, sequences,
scenarios, the journal, verification and near misses all work.

To let a coding agent manage FastMock itself, see [MCP for coding agents](mcp.md).

## A tool

```json
{
  "name": "forecast for Kyiv",
  "request": {
    "urlPath": "/mcp",
    "mcp": {
      "tool": "get_forecast",
      "arguments": [{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]
    }
  },
  "mcp": {
    "description": "Weather forecast for a city",
    "inputSchema": {
      "type": "object",
      "properties": {"city": {"type": "string"}, "days": {"type": "integer"}},
      "required": ["city"]
    },
    "annotations": {"readOnlyHint": true}
  },
  "response": {
    "jsonBody": {"city": "{{jsonPath request.body '$.params.arguments.city'}}", "temperature": 21}
  }
}
```

Point the agent's MCP client at `https://mock.example.com/mcp` (the data port). It sees the tool
`get_forecast` with the description and schema above; calling it with `{"city": "Kyiv"}` returns the rendered
body as the tool result.

### `request.mcp`

| Field | Meaning |
|---|---|
| `tool` | Matches `tools/call` of this tool (`params.name`). The stub is listed by `tools/list`. |
| `resource` | Matches `resources/read` of this URI (`params.uri`). The stub is listed by `resources/list`. |
| `method` | Matches another JSON-RPC method, for example `prompts/list`; with `tool` or `resource` it overrides `tools/call` / `resources/read`. |
| `arguments` | [Body matchers](request-matching.md#body-matchers) on the call's `params.arguments`. A `jsonPath` starts at the arguments object (`$.city`, `$.items[0].sku`); other matchers (`equalToJson`, `jsonSchema`, `matches`, ...) apply to the whole arguments object. |

Give `tool`, `resource` or `method`. `request.mcp` implies `POST`; the URL fields (`urlPath`, `urlPathPattern`,
`urlPathTemplate`, `url`), `host`, `headers`, `query` and the other request matchers work as usual, so several
mocked servers can live on different paths or hosts, and a header matcher can require the agent's token.

Several stubs may serve the same tool: the usual [matching order](request-matching.md#which-stub-wins) picks one (priority,
then the most specific URL), so a stub with argument matchers and a lower `priority` number answers specific
calls and a catch-all stub answers the rest.

### The stub's `mcp`

| Field | Meaning |
|---|---|
| `description`, `title` | Shown in `tools/list` / `resources/list` (the description defaults to the stub name). |
| `inputSchema` | JSON Schema of the arguments (default `{"type": "object"}`). Matching does not enforce it; add `{"jsonSchema": ...}` to `arguments` to answer only valid calls. |
| `outputSchema` | JSON Schema of the tool's `structuredContent`. |
| `annotations` | Tool annotations such as `{"readOnlyHint": true}`. |
| `name`, `mimeType` | A resource's name and MIME type (default `text/plain`). |
| `error` | `{"code", "message", "data"}`: answer with this JSON-RPC error instead of a result. |

When several stubs serve one tool, `tools/list` shows the first one (in matching order) that has an `mcp` field.

## What the agent receives

The rendered response body becomes the JSON-RPC result:

| Body | Result |
|---|---|
| A JSON object with `content`, `structuredContent` or `isError` | Used as the tool result as is (`{"isError": true, "content": [...]}` is a tool-level failure the agent sees) |
| Any other JSON object | `structuredContent`, with the JSON text as a text content block |
| Text | One text content block |
| A JSON-RPC message (an object with `jsonrpc`) | Sent as is; its `id` is filled in when missing |
| `resources/read`: text, or an object with `contents` | `{"contents": [{"uri", "mimeType", "text"}]}`, or the object as is |
| Other methods | The JSON body (or text) as the result |

Other answers:

- `mcp.error` answers with that JSON-RPC error, for example `{"code": -32000, "message": "quota exceeded"}`.
- A response `status` outside 2xx answers with that HTTP status and the body as is, to test transport errors
  (`401`, `429`, `503`).
- `fault` (`CONNECTION_RESET`, `EMPTY_RESPONSE`, ...) and `delay` work as for HTTP stubs, and so do
  [chaos rules](proxy-and-chaos.md) on the path.
- `responses` with `responseMode`, `times`, `scenario` and `callbacks` work as for HTTP stubs, for example a
  tool that fails on every second call:

```json
{
  "request": {"urlPath": "/mcp", "mcp": {"tool": "search"}},
  "mcp": {"description": "Search the catalogue"},
  "responseMode": "cycle",
  "responses": [
    {"jsonBody": {"content": [{"type": "text", "text": "3 results"}]}},
    {"jsonBody": {"isError": true, "content": [{"type": "text", "text": "upstream timeout"}]}}
  ]
}
```

A resource:

```json
{
  "request": {"urlPath": "/mcp", "mcp": {"resource": "file:///docs/readme.md"}},
  "mcp": {"description": "Project readme", "mimeType": "text/markdown"},
  "response": {"body": "# Readme\nHello"}
}
```

Any other method, here a prompt list:

```json
{
  "request": {"urlPath": "/mcp", "mcp": {"method": "prompts/list"}},
  "response": {"jsonBody": {"prompts": [{"name": "summarize"}]}}
}
```

## Protocol details

- FastMock answers `initialize` (protocol versions `2024-11-05` to `2025-11-25`; the client's version when
  it is one of them, otherwise `2025-11-25`), `ping`, `tools/list`, `resources/list` and
  `resources/templates/list` itself, unless a stub with `mcp.method` for that method matches first.
- Notifications (messages without an `id`) are accepted with `202`. Batches are refused with `400`, `GET`
  and other methods with `405`; there is no server-sent stream and no session id.
- A `tools/call` that no stub matches gets JSON-RPC error `-32602` (`unknown tool: x`, or
  `no stub of tool x matches these arguments`, with near misses in `data` when `FASTMOCK_UNMATCHED_DETAILS` is
  on); `resources/read` gets `-32002`; other unknown methods `-32601`.
- Paths without MCP stubs are not affected; while no MCP stub is loaded, the data plane checks one flag per
  request and nothing else.

## Verify what the agent did

Every MCP message is a journal entry. `request.mcp` works in verification patterns, `requests/find` and the
[`verify` tool](mcp.md):

```sh
curl -s -X POST https://fastmock-admin.example.com/__admin/verify \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "request": {"urlPath": "/mcp", "mcp": {"tool": "get_forecast",
    "arguments": [{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]}},
  "count": 1
}'
```

Calls that matched no stub are journaled as unmatched, and near misses name the MCP part that differed, for
example `MCP tool is not get_forecast` or `MCP argument matcher #0 ($.city) does not match`.

## Metrics

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_mcp_mock_requests_total` | `method` (`initialize`, `ping`, `tools/list`, `tools/call`, `resources/list`, `resources/read`, `resources/templates/list`, `prompts/list`, `prompts/get`, `other`), `outcome` (`stub`, `builtin`, `unmatched`, `notification`, `invalid`, `method_not_allowed`, `error`) | Messages to mocked MCP servers |

Each message is a `fastmock.mcp.mock` span with `rpc.method`, `mcp.tool` and `mcp.outcome` inside the usual
`fastmock.request` span. The usual data-plane metrics count the requests too.
