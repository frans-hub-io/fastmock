# WebSocket mocks

A stub whose response has a `websocket` section accepts WebSocket connections: when a matching request asks to
upgrade (`Connection: Upgrade`, `Upgrade: websocket`), FastMock completes the handshake and runs a scripted session:
it sends scripted messages, answers incoming messages by rules, echoes, and closes after a time or a number of
messages. All texts are templates that see the handshake request and, for replies, the incoming message.

WebSocket stubs are also what the [AsyncAPI import](asyncapi.md) creates for operations with `ws` bindings or
servers: they stream the examples of the document to every client, one per `frequencyMs`.

- [Defining a WebSocket stub](#defining-a-websocket-stub)
- [Session behaviour](#session-behaviour)
- [Templates](#templates)
- [Plain HTTP requests to the same path](#plain-http-requests-to-the-same-path)
- [Trying it](#trying-it)
- [Journal, metrics and traces](#journal-metrics-and-traces)
- [Limits](#limits)
- [Errors](#errors)

The examples use `FASTMOCK_TOKEN` for an editor or admin token, the admin API on `localhost:9090` and the data
plane on `localhost:8080`.

## Defining a WebSocket stub

```json
{
  "name": "chat room",
  "request": {"method": "GET", "urlPathTemplate": "/rooms/{room}"},
  "response": {
    "status": 426,
    "body": "this endpoint speaks WebSocket",
    "websocket": {
      "messages": [
        {"data": "{\"type\": \"welcome\", \"room\": \"{{request.pathParams.room}}\", \"user\": \"{{request.query.user}}\"}"},
        {"data": "{\"type\": \"ping\", \"at\": \"{{now}}\"}", "delayMs": 30000}
      ],
      "onMessage": [
        {"match": {"contains": "\"type\":\"bye\""}, "reply": "{\"type\": \"bye\"}", "close": true},
        {"match": {"matches": "^\\{.*\"type\":\"join\".*"}, "reply": "{\"type\": \"joined\", \"id\": \"{{message.json.id}}\", \"session\": \"{{uuid}}\"}"}
      ],
      "echo": true,
      "repeat": false,
      "closeAfterMs": 600000,
      "maxMessages": 1000
    }
  }
}
```

The request pattern is an ordinary one: method, path, query, headers (for example a token in `Sec-WebSocket-Protocol`
or `Authorization`) all work, and priorities decide between stubs. Use `"method": "GET"`; WebSocket handshakes are
`GET` requests over HTTP/1.1.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `messages` | array | none | Scripted messages sent in order: `{"data": template, "delayMs": n}`. `delayMs` is the pause before this message (counted from the handshake for the first one, from the previous scripted message otherwise), 0..3,600,000. |
| `repeat` | boolean | `false` | Start `messages` again from the first after the last one. Needs at least one message with `delayMs > 0`. |
| `onMessage` | array | none | Rules for incoming messages: `{"match": StringMatcher, "reply": template, "close": bool}`. The first rule whose `match` accepts the message applies; a rule without `match` accepts everything. A rule needs `reply`, `close` or both. |
| `echo` | boolean | `false` | Send every incoming message that no rule matched back unchanged (text as text, binary as binary). |
| `closeAfterMs` | integer | 3,600,000 (1 hour) | Close the session with code `1000` after this time, 1..86,400,000. |
| `maxMessages` | integer | 10,000 | Close the session with code `1000` after FastMock has sent this many messages (scripted, replies and echoes together), 1..100,000. |

`match` is the usual string matcher: `equalTo`, `contains`, `matches`, `doesNotMatch`, `caseInsensitive`. It is applied
to the text of the message; binary messages are matched as UTF-8 (invalid bytes become U+FFFD).

`response.status`, `headers` and `body` are used only for requests that do not ask for an upgrade (see
[below](#plain-http-requests-to-the-same-path)). `response.delay` delays the handshake. For upgrade requests
`fault`, `proxy` and `sse` are ignored, so do not combine them with `websocket`. Only the stub's `response` can
carry `websocket`, not the entries of `responses`.

## Session behaviour

- Scripted messages, replies and echoes are sent as **text** frames (echoes keep the frame type of the incoming
  message).
- Scripted messages run on their own timer; incoming messages are answered immediately and do not shift the script.
- Without `repeat`, the session stays open after the last scripted message until the client closes it,
  `closeAfterMs` passes, `maxMessages` is reached, or a rule with `"close": true` matches.
- FastMock closes with code `1000` and a short reason (`closeAfterMs reached`, `maxMessages reached`,
  `closed by onMessage rule`) and with `1008` when a client sends more than 10,000 messages.
- Pings are answered with pongs automatically and do not count as messages.
- If the client asks for subprotocols (`Sec-WebSocket-Protocol`), the first one is accepted.
- A message whose template fails to render is skipped (logged and counted), the session continues.

## Templates

Every `data` and `reply` is a Handlebars template (unless the response has `"templated": false`). The context is the
**handshake request**, the same as for HTTP responses:

| Variable | Value |
|---|---|
| `request.method`, `request.path`, `request.pathSegments`, `request.pathParams`, `request.query`, `request.queryAll`, `request.headers` | the upgrade request |
| `message.text` | the incoming message as text (replies only) |
| `message.json` | the incoming message parsed as JSON, or `null` (replies only) |

All helpers are available: `uuid`, `now`, `randomInt`, `pickRandom`, `jsonPath`, ... (see [Templating](templating.md)).
Templates cannot read files or the environment and cannot execute code.

```json
{"match": {"matches": "^\\{.*"}, "reply": "{\"echoedId\": \"{{message.json.id}}\", \"items\": {{size message.json.items}}}"}
```

## Plain HTTP requests to the same path

A request that matches the stub but is not an upgrade request gets the stub's normal response (`status`, `headers`,
`body`). Stubs created by the AsyncAPI import answer such requests with `426 Upgrade Required` and a JSON error.

## Trying it

```sh
curl -X POST localhost:9090/__admin/stubs -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
  "request": {"method": "GET", "urlPath": "/ticker"},
  "response": {"websocket": {
    "messages": [{"data": "{\"price\": {{randomInt 90 110}}}", "delayMs": 1000}],
    "repeat": true, "maxMessages": 5,
    "onMessage": [{"match": {"equalTo": "stop"}, "reply": "bye", "close": true}]
  }}
}'

websocat ws://localhost:8080/ticker
```

```text
{"price": 97}
{"price": 104}
{"price": 91}
{"price": 110}
{"price": 99}
```

From a test, any WebSocket client works; for example with Node.js 22:

```js
const ws = new WebSocket("ws://localhost:8080/rooms/r1?user=ann");
ws.onmessage = (e) => console.log(e.data);
ws.onopen = () => ws.send(JSON.stringify({type: "join", id: 7}));
```

## Journal, metrics and traces

The handshake is journaled like any request, with status `101` and the matched stub id; the messages of the session
are not journaled.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_ws_connections` | gauge | none | Open WebSocket sessions on this replica. |
| `fastmock_ws_sessions_total` | counter | `stub` (stub name or id) | Sessions started. |
| `fastmock_ws_messages_total` | counter | `direction` = `in`, `out`; `result` = `received`, `sent`, `template_error` | Messages received and sent. |
| `fastmock_ws_rejected_total` | counter | `reason` = `connections`, `handshake`, `upgrade`, `invalid` | Upgrade requests that did not become a session. |

Each session runs in a span `fastmock.websocket` (`url.path`, `fastmock.stub_id`, and at the end
`fastmock.ws.sent` and `fastmock.ws.received`), a child of the request span of the handshake.

## Limits

| Limit | Value |
|---|---|
| Open sessions per replica | 1024 (more get `503` with `retry-after: 1`) |
| Frame and message size, both directions | 1 MiB (larger incoming messages close the connection) |
| Session duration | `closeAfterMs`, default 1 hour, at most 24 hours |
| Messages sent per session | `maxMessages`, default 10,000, at most 100,000 |
| Messages received per session | 10,000 (then close with `1008`) |
| Scripted messages | 1000 per stub, each at most 1 MiB, `delayMs` at most 1 hour |
| `onMessage` rules | 64 per stub, replies at most 1 MiB |

## Errors

| Status | When |
|---|---|
| `101` | The session started. |
| `400` / `426` | The upgrade request is malformed (for example a missing `Sec-WebSocket-Key` or version), as answered by the WebSocket handshake. |
| `500` | The stored `websocket` definition is invalid (out of range values, broken templates or matchers); fix the stub. |
| `503` | 1024 sessions are already open on this replica. |
