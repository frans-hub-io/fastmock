# MCP for coding agents

FastMock serves the [Model Context Protocol](https://modelcontextprotocol.io/) at `/mcp` on the admin port, so
a coding agent in your editor (GitHub Copilot in VS Code, Claude Code, Cursor or any other MCP client) can create
and change stubs, search the request journal, explain why a request did not match, verify calls, set scenario
states, read callback deliveries, import API descriptions and read captured mail. The agent runs on your machine
with your token; FastMock itself calls no AI service.

To test your own AI agent against a fake MCP server instead, see [Mock MCP servers](mock-mcp-servers.md).

## The endpoint

| | |
|---|---|
| URL | `https://<admin host>/mcp`, for example `https://fastmock-admin.example.com/mcp` or `http://localhost:9090/mcp` |
| Transport | Streamable HTTP, stateless (no session id), JSON responses |
| Authentication | The admin API's: static tokens, managed `fm_` tokens, OIDC bearer JWTs, or the SSO session cookie with `X-FastMock-Request: 1` |
| Origin check | The admin API's (`FASTMOCK_ADMIN_ALLOWED_ORIGINS`) |
| Switch | `FASTMOCK_MCP_ENABLED` (default `true`), Helm `admin.mcp.enabled`, operator `spec.mcp.enabled` |

The endpoint is on by default: it grants nothing beyond what the same token can do through the admin API, and
it lives on the admin port, which already requires a token. Set `FASTMOCK_MCP_ENABLED=false` to remove it; `/mcp`
then answers 404.

## Connect a client

Create a token for the agent first, with the smallest role that fits (`viewer` to investigate, `editor` to
change stubs), ideally a [managed token](../operations/access-control.md#managed-tokens) with an expiry:

```sh
curl -s -X POST https://fastmock-admin.example.com/__admin/tokens \
  -H "Authorization: Bearer $FASTMOCK_ADMIN_TOKEN" -H 'content-type: application/json' \
  -d '{"name": "copilot-alice", "role": "editor", "expiresInDays": 30}'
```

### VS Code and GitHub Copilot

`.vscode/mcp.json` in your repository; VS Code asks for the token once and keeps it in its secret storage:

```json
{
  "inputs": [
    {
      "type": "promptString",
      "id": "fastmock-token",
      "description": "FastMock token (viewer to read, editor to change stubs)",
      "password": true
    }
  ],
  "servers": {
    "fastmock": {
      "type": "http",
      "url": "https://fastmock-admin.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${input:fastmock-token}"
      }
    }
  }
}
```

### Claude Code

```sh
claude mcp add --transport http fastmock https://fastmock-admin.example.com/mcp \
  --header "Authorization: Bearer $FASTMOCK_TOKEN"
```

or, shared with the team in `.mcp.json` at the repository root (the variable is read from each user's
environment):

```json
{
  "mcpServers": {
    "fastmock": {
      "type": "http",
      "url": "https://fastmock-admin.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${FASTMOCK_TOKEN}"
      }
    }
  }
}
```

### Cursor

`.cursor/mcp.json` in the project (or `~/.cursor/mcp.json` for all projects):

```json
{
  "mcpServers": {
    "fastmock": {
      "url": "https://fastmock-admin.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${env:FASTMOCK_TOKEN}"
      }
    }
  }
}
```

### Any other client

Any client that speaks streamable HTTP and can send an `Authorization` header works. A quick check with curl:

```sh
curl -s https://fastmock-admin.example.com/mcp \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}'
```

## Tools

Every tool runs the admin API route in the last column with the caller's credentials, so it needs exactly that
route's permission, in the same [spaces](../operations/access-control.md#spaces), and answers with the same
masking. `tools/list` shows only the tools whose permission the token holds somewhere; the mail tools appear
only when [mail capture](mail.md) is on.

| Tool | Does | Permission | Admin API |
|---|---|---|---|
| `list_stubs` | Compact stub summaries, filtered by space, tag, enabled or search text, paged | `stubs:read` | `GET /__admin/stubs` |
| `get_stub` | One complete stub | `stubs:read` | `GET /__admin/stubs/{id}` |
| `create_stub` | Create a stub | `stubs:write` | `POST /__admin/stubs` |
| `update_stub` | Replace a stub (`stub`) or merge-patch it (`patch`) | `stubs:write` | `PUT` / `PATCH /__admin/stubs/{id}` |
| `delete_stub` | Delete a stub | `stubs:write` | `DELETE /__admin/stubs/{id}` |
| `validate_stub` | Check a stub without saving it | `stubs:read` | `POST /__admin/stubs/validate` |
| `find_requests` | Search the journal by filters or a request pattern | `journal:read` | `GET /__admin/requests`, `POST /__admin/requests/find` |
| `get_request` | One journal entry | `journal:read` | `GET /__admin/requests/{id}` |
| `near_misses` | The closest stubs to a journal entry or a request, with what differed | `journal:read` | `POST /__admin/requests/near-misses` |
| `verify` | Check a request count (`count`, `atLeast`, `atMost`) | `journal:read` | `POST /__admin/verify` |
| `reset_journal` | Clear the journal of a space, or of everything the token may delete | `journal:delete` | `DELETE /__admin/requests` |
| `get_scenarios` | Current scenario states | `state:read` | `GET /__admin/scenarios` |
| `set_scenario` | Move a scenario to a state | `state:write` | `PUT /__admin/scenarios/{name}` |
| `list_callbacks` | Callback deliveries with status, attempts and errors | `callbacks:read` | `GET /__admin/callbacks` |
| `import` | Generate stubs from OpenAPI, WireMock, MockServer, Postman, HAR, AsyncAPI, GraphQL, WSDL, SoapUI or Microcks | `imports:run` | `POST /__admin/import` |
| `get_chaos` | Active chaos rules | `chaos:read` (global) | `GET /__admin/chaos` |
| `wait_for_mail` | Wait for emails matching a matcher (at most 30 s) | `mail:read` | `POST /__admin/mail/messages/wait` |
| `read_mail` | One email with its links and one-time codes | `mail:read` | `GET /__admin/mail/messages/{id}`, `/links`, `/codes` |
| `find_messages` | Captured Kafka, MQTT and AMQP messages matching a matcher, newest first, optionally with decoded bodies | `messages:read` | `POST /__admin/capture/messages/find`, `GET /__admin/capture/messages/{id}` |
| `wait_for_message` | Wait for captured broker messages (at most 30 s), with their decoded bodies | `messages:read` | `POST /__admin/capture/messages/wait` |
| `verify_messages` | Check a count of captured broker messages, with near misses | `messages:read` | `POST /__admin/capture/verify` |

Every tool has a description written for the model and JSON Schemas for its arguments and its result. Two
behaviours differ from the REST defaults on purpose: `import` keeps existing stubs with the same tag unless
`replace: true` is given, and `wait_for_mail` and `wait_for_message` wait at most 30 seconds, because MCP clients often stop waiting
for a tool sooner than that.

### Resources

| URI | Content |
|---|---|
| `fastmock://guide/stubs` | How to write a stub: matching, responses, templates, faults, scenarios, callbacks, MCP tool stubs and verification, with examples (Markdown) |
| `fastmock://schema/stub` | The JSON Schema of a stub, as accepted by `create_stub` |

### Results

Tool results are bounded so that they fit into a model's context:

- page sizes are limited (`limit` up to 100), and `list_stubs` returns summaries; `get_stub` returns one stub;
- request and response bodies from the journal are cut to `maxBodyBytes` (default 2048 bytes in lists, 16 KiB
  for one entry, at most 64 KiB) with the journal's `…[truncated N bytes]` marker; the journal itself keeps
  bodies up to `FASTMOCK_JOURNAL_MAX_BODY_BYTES`;
- any string longer than 16 KiB is cut the same way, and a result larger than 256 KiB drops items from its list
  and says so with `"truncated": true` and `"omitted"`;
- headers, query parameters and bodies come from the journal, which stores them redacted; stubs and callbacks are
  masked exactly as the admin API masks them for the token.

A refused call is a tool error with the admin API's status and message, for example
`HTTP 403: forbidden; requires permission stubs:write in space payments (required: stubs:write)`.

## Auditing

Changes made through MCP are written to the [audit log](../operations/security.md#audit-log) like REST changes,
under the token's name and space, with `"via": "mcp"` and the tool name (`"mcpTool": "create_stub"`) added to the
detail:

```json
{"principal": "copilot-alice", "space": "payments", "action": "stub.create",
 "detail": {"id": "8d0c…", "name": "order created", "via": "mcp", "mcpTool": "create_stub"}}
```

## Example session

Ask the agent, for example: "Mock `POST /orders` so that SKU `A-1` gets `201` with a generated id, then check
that my test called it once." A typical agent reads `fastmock://guide/stubs`, calls `validate_stub` and
`create_stub`, lets you run the test, then calls `verify` with
`{"request": {"method": "POST", "urlPath": "/orders"}, "count": 1}`; when the count is wrong it calls
`find_requests` with `matched: false` and `near_misses` to explain what the test sent instead.

## Observability

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_mcp_tool_calls_total` | `tool`, `outcome` (`ok`, `invalid`, `denied`, `rejected`, `error`, `unknown_tool`) | Tool calls |
| `fastmock_mcp_tool_duration_seconds` | `tool` | Tool call duration |

Each tool call is a `fastmock.mcp.tool` span with `mcp.tool` and `mcp.outcome`; the admin API work it does is
logged and audited as usual.

## Good practice

- Give each agent its own token with the smallest role and an expiry, and revoke it when it is no longer needed.
- Keep tokens out of repositories: use the client's secret prompt or an environment variable as above.
- Bind tokens to spaces when agents work on one team's mocks only; the tools then see and change only those
  spaces.
- Treat what the agent reads from the journal as data from the systems under test, and review proposed changes
  before you let the agent apply them.
- Expose `/mcp` only where the admin API is exposed.
