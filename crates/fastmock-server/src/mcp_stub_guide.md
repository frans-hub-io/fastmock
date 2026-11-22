# Writing FastMock stubs

A stub is a JSON object with a `request` pattern and a `response`. When a request on the data plane
matches the pattern, FastMock answers with the response. The full JSON Schema is the resource
`fastmock://schema/stub`.

```json
{
  "name": "get user",
  "request": {"method": "GET", "urlPathTemplate": "/users/{id}"},
  "response": {
    "status": 200,
    "headers": {"content-type": "application/json"},
    "jsonBody": {"id": "{{request.pathParams.id}}", "name": "Alice"}
  }
}
```

## Request matching

- URL: exactly one of `urlPath` (exact path), `urlPathPattern` (regular expression),
  `urlPathTemplate` (`/users/{id}`, read the value as `request.pathParams.id`), `url` or `urlPattern`
  (path with query).
- `method`: `GET`, `POST`, ... or `ANY`.
- `query`, `headers`, `cookies`, `formParameters`: maps of string matchers.
- String matchers: `equalTo`, `contains`, `matches` (regex), `doesNotMatch`, `absent: true`,
  `caseInsensitive: true`, and `and` / `or` / `not` combinations.
- `body`: a list of body matchers, all of which must match: `equalToJson` (with `ignoreExtraElements`,
  `ignoreArrayOrder`), `jsonPath` with an optional `value` string matcher
  (`{"jsonPath": "$.amount", "value": {"matches": "[0-9]+"}}`), `matches`, `contains`, `equalTo`,
  `jsonSchema`, `xpath`, `equalToXml`.
- `priority` (lower wins, default 5) decides between several matching stubs.

## Responses

- `status`, `headers`, and one of `body` (text), `jsonBody` (JSON), `base64Body` or `bodyFileName`.
- Templates (Handlebars) in bodies and headers: `{{request.path}}`, `{{request.query.page}}`,
  `{{request.headers.x-id}}`, `{{jsonPath request.body '$.id'}}`, `{{randomValue type='UUID'}}`,
  `{{now}}`. Set `"templated": false` to send text as is.
- `delay`: `{"fixedMs": 200}` or `{"minMs": 100, "maxMs": 900}`.
- `fault`: `CONNECTION_RESET`, `EMPTY_RESPONSE`, `MALFORMED_RESPONSE_CHUNK`, `RANDOM_DATA_THEN_CLOSE`,
  `MALFORMED_JSON`.
- `responses` (a list) with `responseMode` `sequential`, `cycle` or `random` answers differently per call.
- `times: 1` lets a stub answer only once.

## State and callbacks

- Scenarios: `"scenario": {"name": "order", "requiredState": "Started", "newState": "Paid"}`. Read and set
  states with the tools `get_scenarios` and `set_scenario`.
- Callbacks (webhooks after the response): `"callbacks": [{"url": "https://hooks.example.com/x",
  "method": "POST", "jsonBody": {"id": "{{jsonPath request.body '$.id'}}"}}]`. The tool `list_callbacks`
  shows their delivery status.

## Mocking an MCP server

A stub can be a tool (or resource) of a mocked MCP server on the data plane, so that an AI agent can be
tested against it. The `request.mcp` matcher matches JSON-RPC calls posted to the stub's path; the stub's
`mcp` field describes the tool for `tools/list`:

```json
{
  "request": {
    "urlPath": "/mcp",
    "mcp": {"tool": "get_forecast", "arguments": [{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]}
  },
  "mcp": {
    "description": "Weather forecast for a city",
    "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
  },
  "response": {"jsonBody": {"content": [{"type": "text", "text": "Sunny in {{jsonPath request.body '$.params.arguments.city'}}"}]}}
}
```

- `request.mcp.tool` matches `tools/call` of that tool; `arguments` are body matchers whose `jsonPath`
  starts at the tool arguments. `request.mcp.resource` matches `resources/read` of that URI, and
  `request.mcp.method` any other JSON-RPC method (for example `prompts/list`).
- FastMock answers `initialize`, `ping`, `tools/list` and `resources/list` itself from the stubs on
  that path.
- The response body becomes the JSON-RPC result: a JSON object with `content`, `structuredContent` or
  `isError` is used as the tool result; other JSON becomes `structuredContent` with a text copy; text
  becomes a text content block. `"mcp": {"error": {"code": -32000, "message": "quota exceeded"}}`
  answers with a JSON-RPC error instead, and a non-2xx `status` answers with that HTTP status.
- Delays, faults, sequences, scenarios and `times` work as for HTTP stubs; every call is in the journal.

## Checking what happened

- `find_requests` searches the journal; `near_misses` explains why a request matched no stub.
- `verify` checks counts:
  `{"request": {"method": "POST", "urlPath": "/orders"}, "count": 1}`, or for a mocked MCP tool
  `{"request": {"urlPath": "/mcp", "mcp": {"tool": "get_forecast"}}, "atLeast": 1}`.
