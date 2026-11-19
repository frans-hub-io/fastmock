# Responses

The `response` object of a stub describes what FastMock sends back: status, reason phrase, headers, a body (inline,
JSON, base64 or a stored file), a delay, a slow "dribbled" body, a network fault, a proxy to a real service or a
stream of Server-Sent Events. A stub can also return a different response on every call with `responses`.

- [Status, reason phrase and headers](#status-reason-phrase-and-headers)
- [Bodies](#bodies)
- [Body files](#body-files)
- [Delays](#delays)
- [Dribbling the body](#dribbling-the-body)
- [Faults](#faults)
- [Response sequences](#response-sequences)
- [Proxying](#proxying)
- [Server-Sent Events](#server-sent-events)
- [Errors and metrics](#errors-and-metrics)

## Status, reason phrase and headers

```json
{
  "request": {"method": "GET", "urlPath": "/legacy/status"},
  "response": {
    "status": 299,
    "statusMessage": "Mostly Fine",
    "headers": {"content-type": "text/plain", "x-request-id": "{{uuid}}"},
    "body": "partial"
  }
}
```

| Field | Default | Meaning |
|---|---|---|
| `status` | 200 | Any code from 200 to 599. |
| `statusMessage` | standard phrase | Custom reason phrase on HTTP/1.1 (`HTTP/1.1 299 Mostly Fine`). HTTP/2 has no reason phrase, so it is ignored there. At most 256 bytes, no control characters. |
| `headers` | none | Name to value, or to an array of values sent as separate header lines (`"set-cookie": ["a=1", "b=2"]`; the lines are never folded into one); values are templates. A rendered value that is not a valid header value is dropped and counted in `fastmock_invalid_response_headers_total`. |
| `templated` | `true` | Render header values, bodies and `bodyFileName` as Handlebars templates (see [Templating](templating.md)). |

```sh
curl -si localhost:8080/legacy/status | head -1
# HTTP/1.1 299 Mostly Fine
```

## Bodies

Use at most one of these fields:

| Field | Meaning |
|---|---|
| `body` | Text; a template when `templated` is true. |
| `jsonBody` | A JSON value; every string inside is a template, and `"{{json expr}}"` keeps the JSON type of the value. Adds `content-type: application/json` unless a header sets it. |
| `base64Body` | Binary content, base64-encoded; never templated. |
| `bodyFileName` | The name of a stored file, see below. |

## Body files

Large or binary bodies, shared fixtures and bodies maintained outside the stub live in the file store and are
referenced by name. Upload them through the admin API (the request body is the file, `Content-Type` is stored with
it, files are limited to 10 MiB):

```sh
curl -X PUT http://localhost:9090/__admin/files/users/7.json \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  --data-binary '{"id": 7, "name": "Ann", "requestedPath": "{{request.path}}"}'
```

```json
{
  "request": {"method": "GET", "urlPathTemplate": "/users/{id}"},
  "response": {"status": 200, "bodyFileName": "users/{{request.pathParams.id}}.json"}
}
```

```sh
curl -s localhost:8080/users/7
# {"id": 7, "name": "Ann", "requestedPath": "/users/7"}
```

- `bodyFileName` is a template when `templated` is true, so one stub can serve many files.
- The stored content type becomes the `content-type` header unless the stub sets one.
- With `templated: true` the file content is rendered as a template when it is text (`text/*`, JSON, XML, YAML,
  CSV, HTML, JavaScript, form data, or valid UTF-8 without a content type) and contains `{{`. Templates are limited
  to 256 KiB: set `"templated": false` for larger text files that only need to be served. Binary files are served
  byte for byte.
- Files are stored in PostgreSQL (DataStore namespace `files`), so every replica serves the same content. Each
  replica caches a file for up to 2 seconds (at most 256 files and 64 MiB; files over 16 MiB are not cached).
- File names use `[A-Za-z0-9._-/]`, at most 512 bytes; `GET /__admin/files` lists them.

When the file does not exist (or the rendered name is empty or longer than 512 bytes) the client gets
`500 {"error": "response body file is not available", ...}` without the file name, the name is logged on the
server, and `fastmock_body_file_missing_total` is incremented.

## Delays

```json
{"response": {"status": 200, "body": "slow", "delay": {"fixedMs": 1500}}}
{"response": {"status": 200, "body": "jittery", "delay": {"minMs": 100, "maxMs": 900}}}
{"response": {"status": 200, "body": "realistic", "delay": {"lognormal": {"medianMs": 120, "sigma": 0.4}}}}
```

| `delay` field | Meaning |
|---|---|
| `fixedMs` | Always this many milliseconds. |
| `minMs` / `maxMs` | Uniformly random in the range (either bound alone is allowed). |
| `lognormal` | `{"medianMs", "sigma"}`: a log-normal sample, `medianMs * e^(sigma * N(0,1))`. Half of the delays are below the median; a larger `sigma` gives a longer tail. Sigma 0.1 is tight, 0.5 is a typical network tail, 1 or more is very bursty. |

Use one kind per delay. Values are limited to 24 hours and `sigma` to 0..10 when the stub is saved, and every delay
is capped at `FASTMOCK_MAX_DELAY` (default 15 minutes) when it is applied.

The [global settings](stub-lifecycle.md#global-settings) add server-wide delays: `fixedDelayMs` is used for stubs
without their own `delay`, and `delay` (any kind above) is added to every stub response. Unmatched requests are not
delayed. The delay is applied before the response starts, also for proxy and SSE responses.

## Dribbling the body

`chunkedDribbleDelay` sends the body in pieces spread over a duration, to test read timeouts and progress handling:

```json
{
  "request": {"urlPath": "/download/report.csv"},
  "response": {
    "headers": {"content-type": "text/csv"},
    "body": "id,amount\n1,10\n2,20\n3,30\n",
    "chunkedDribbleDelay": {"numberOfChunks": 5, "totalDurationMs": 2000}
  }
}
```

The status and headers go out at once; the body is cut into `numberOfChunks` pieces of equal size (fewer when the
body is shorter than that) and each piece is sent after `totalDurationMs / numberOfChunks`. The response uses chunked
transfer encoding. `numberOfChunks` is 1 to 1000; the total duration is capped at `FASTMOCK_MAX_DELAY`. It cannot be
combined with `sse`, `proxy` or `fault`, and dribbled response bodies are not stored in the request journal.

## Faults

`fault` breaks the response on purpose:

| Fault | What the client sees |
|---|---|
| `EMPTY_RESPONSE` | Nothing: the connection closes without a single byte (curl: `Empty reply from server`, Java clients: `NoHttpResponseException`). Over HTTP/2 the stream is reset before any frame of the response. As in WireMock. For a status line with an empty body, use a normal response without a body and `connection: close`. |
| `MALFORMED_JSON` | The status, `content-type: application/json` and a truncated JSON document. |
| `CONNECTION_RESET` | The connection closes before a complete response arrives. |
| `RANDOM_DATA_THEN_CLOSE` | 128 random bytes instead of an HTTP response, then the connection closes. |
| `MALFORMED_RESPONSE_CHUNK` | `HTTP/1.1 200 OK`, `Transfer-Encoding: chunked`, then bytes that are not a valid chunk, then the connection closes. |

```json
{"request": {"urlPath": "/flaky"}, "response": {"fault": "MALFORMED_RESPONSE_CHUNK"}}
```

```sh
curl -sv localhost:8080/flaky
# < HTTP/1.1 200 OK
# < Transfer-Encoding: chunked
# * Illegal or missing hexadecimal sequence in chunked-encoding   (the wording differs between curl versions)
# curl: (56) ...
```

`RANDOM_DATA_THEN_CLOSE` and `MALFORMED_RESPONSE_CHUNK` write raw bytes on HTTP/1.x connections. On HTTP/2 the
connection carries other requests too, so FastMock sends a 200 response whose body stream is reset after the
garbage bytes. Faults can also be injected at random with chaos rules (`PUT /__admin/chaos`). Every injected fault
counts in `fastmock_faults_total{fault}`.

## Response sequences

`responses` replaces `response` with a list that is served in turn, for retries, polling and state that changes over
time without scenarios:

```json
{
  "name": "payment status polling",
  "request": {"method": "GET", "urlPathTemplate": "/payments/{id}"},
  "responseMode": "sequential",
  "responses": [
    {"status": 202, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "PENDING"}},
    {"status": 202, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "PROCESSING"}},
    {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "PAID"}}
  ]
}
```

| `responseMode` | Order |
|---|---|
| `sequential` (default) | 1st, 2nd, 3rd, then the last one forever. |
| `cycle` | 1st, 2nd, 3rd, 1st, 2nd, ... |
| `random` | A random entry on every call. |

```sh
for i in 1 2 3 4; do curl -s localhost:8080/payments/p-1; echo; done
# {"id":"p-1","status":"PENDING"}
# {"id":"p-1","status":"PROCESSING"}
# {"id":"p-1","status":"PAID"}
# {"id":"p-1","status":"PAID"}
```

- Every entry is a full response object (status, headers, bodies, delay, fault, ...).
- Use `response` or `responses`, not both; at most 100 entries; `responseMode` needs `responses`.
- The position is a counter in Valkey shared by all replicas, counted per stub (not per path parameter or client).
  It restarts when the stub is changed (`PUT`, `PATCH`, re-import) and on `DELETE /__admin/stubs/counters`; see
  [Stub lifecycle](stub-lifecycle.md#counters).
- Callbacks and Kafka messages of the stub are sent with every entry.

## Proxying

```json
{
  "request": {"urlPathPattern": "/legacy/.*"},
  "response": {
    "proxy": {
      "baseUrl": "https://legacy.example.com",
      "stripPrefix": "/legacy",
      "additionalHeaders": {"x-forwarded-by": "fastmock"},
      "removeHeaders": ["authorization"]
    }
  }
}
```

The request is forwarded to `baseUrl` + path (minus `stripPrefix`) + query, through the egress policy
(`FASTMOCK_CALLBACK_ALLOWED_HOSTS`, `FASTMOCK_CALLBACK_DENIED_CIDRS`). Upstream bodies are limited to 16 MiB; failures answer
`502`. A proxy response cannot have a body. With the global setting `"proxyPassThrough": false` proxy stubs do not
forward and answer `404 {"error": "proxy pass-through is disabled"}` (counted in
`fastmock_proxy_pass_through_blocked_total`), which keeps a test run from reaching real services.

## Server-Sent Events

`sse` streams a list of events instead of a body; see [Server-Sent Events](sse.md).

## Errors and metrics

| Situation | Client response | Metric / log |
|---|---|---|
| A template fails while rendering | `500 {"error": "stub evaluation failed"}` | `fastmock_stub_errors_total`, error log with the trace id |
| A body file is missing | `500 {"error": "response body file is not available"}` | `fastmock_body_file_missing_total`, error log with the file name |
| A rendered header value is invalid | the header is dropped | `fastmock_invalid_response_headers_total` |
| A fault is injected | see [Faults](#faults) | `fastmock_faults_total{fault}` |
| A proxy stub is blocked by `proxyPassThrough: false` | `404` | `fastmock_proxy_pass_through_blocked_total` |
| A `times` stub is used up | the next stub answers | `fastmock_stub_exhausted_total{stub}` |

Every response carries `x-fastmock-trace-id`, which is also the trace id of the request in the journal and in
OpenTelemetry.
