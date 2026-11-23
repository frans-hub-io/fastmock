# gRPC

FastMock answers gRPC calls on the data port with ordinary stubs: unary calls, server streaming, client streaming
and bidirectional streaming, over gRPC (HTTP/2) and gRPC-Web (HTTP/1.1 or HTTP/2). You upload the service
descriptors once; FastMock then decodes each request message to JSON, matches it with the usual matchers, and
encodes the JSON messages of the stub into the protobuf output type. No code generation and no `.proto`
compilation inside FastMock.

- [1. Upload the descriptors](#1-upload-the-descriptors)
- [2. Unary calls](#2-unary-calls)
- [3. Server streaming](#3-server-streaming)
- [4. Client streaming](#4-client-streaming)
- [5. Bidirectional streaming](#5-bidirectional-streaming)
- [Status, rich error details, metadata and trailers](#status-rich-error-details-metadata-and-trailers)
- [Network faults](#network-faults)
- [Compression](#compression)
- [Server reflection](#server-reflection)
- [gRPC-Web](#grpc-web)
- [Templates](#templates)
- [Field reference](#field-reference)
- [Limits](#limits)
- [Status codes FastMock returns itself](#status-codes-fastmock-returns-itself)
- [Journal, verification and metrics](#journal-verification-and-metrics)
- [Proxying and recording](#proxying-and-recording)
- [Other features](#other-features)

The examples use `FASTMOCK_TOKEN` for an editor or admin token, the admin API on `localhost:9090` and the data
plane on `localhost:8080`.

## 1. Upload the descriptors

Compile your `.proto` files into a binary `FileDescriptorSet` that includes imports:

```sh
protoc --include_imports --descriptor_set_out=orders.pb -I proto proto/shop/v1/orders.proto
```

with, for example:

```proto
syntax = "proto3";
package shop.v1;

service OrderService {
  rpc GetOrder (GetOrderRequest) returns (Order);
  rpc WatchOrder (GetOrderRequest) returns (stream OrderEvent);
  rpc UploadItems (stream Item) returns (UploadSummary);
  rpc Chat (stream ChatMessage) returns (stream ChatMessage);
}

message GetOrderRequest { string id = 1; }
message Order { string id = 1; string status = 2; int32 items = 3; }
message OrderEvent { string order_id = 1; string status = 2; int32 seq = 3; }
message Item { string sku = 1; int32 qty = 2; }
message UploadSummary { int32 count = 1; string first_sku = 2; string last_sku = 3; }
message ChatMessage { string text = 1; string user = 2; }
```

Upload it under a name of your choice (`orders`):

```sh
curl -s -X PUT localhost:9090/__admin/grpc/descriptors/orders \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/octet-stream' \
  --data-binary @orders.pb
```

```json
{"name":"orders","services":["shop.v1.OrderService"]}
```

| Call | Role | Effect |
|---|---|---|
| `PUT /__admin/grpc/descriptors/{name}` | editor | Stores a `FileDescriptorSet` (binary body, up to 16 MiB). Replaces an earlier set with the same name. A set that conflicts with descriptors already loaded under other names (same file or type defined differently) is refused with 409. |
| `GET /__admin/grpc/descriptors` | viewer | `{"descriptors":[{"name":"orders","services":["shop.v1.OrderService"]}],"loadedServices":["shop.v1.OrderService"]}` |
| `DELETE /__admin/grpc/descriptors/{name}` | editor | Removes a set; 404 if it did not exist. |

Descriptor sets are stored with the other data (PostgreSQL in shared mode) and every replica picks up changes
within `FASTMOCK_STUB_POLL` (5 s). They survive restarts. Descriptor sets are global, not part of a
[space](../operations/access-control.md).

The kind of a call (unary, server streaming, client streaming, bidirectional) comes from the method in the
descriptor; a stub does not declare it. The same stub fields serve all kinds, as described below.

## 2. Unary calls

A gRPC call is matched as `POST /<package>.<Service>/<Method>` with the request message as a JSON body:

```sh
curl -s -X POST localhost:9090/__admin/stubs -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
  "name": "GetOrder ok",
  "request": {"method": "POST", "urlPath": "/shop.v1.OrderService/GetOrder",
              "body": [{"jsonPath": "$.id", "value": {"matches": "A-.*"}}]},
  "response": {"jsonBody": {"id": "{{request.json.id}}", "status": "PAID", "items": 3}}
}'

curl -s -X POST localhost:9090/__admin/stubs -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
  "name": "GetOrder not found",
  "priority": 6,
  "request": {"method": "POST", "urlPath": "/shop.v1.OrderService/GetOrder"},
  "response": {"grpc": {"status": {"code": "NOT_FOUND", "message": "order {{request.json.id}} not found"}}}
}'
```

- **Request:** the input message is decoded with the uploaded descriptors and converted with the proto3 JSON
  mapping (JSON field names in lowerCamelCase, 64-bit integers as strings, enums as names, fields with default
  values omitted). All body matchers and `request.json` in templates work on that JSON. Request metadata is
  available as headers.
- **Response:** `jsonBody` (with templates) is the output message in the same JSON mapping. `response.grpc.messages`
  can hold it instead; a unary call sends the first entry. A field that does not exist in the output type, or a
  value of the wrong type, ends the call with `INTERNAL` (13) and a message that names the problem. A stub
  without either answers with an empty message.
- **Errors:** `response.grpc.status` sets the final status ([details below](#status-rich-error-details-metadata-and-trailers)).
  With a non-OK status a unary call sends no message. The older form, the response headers `grpc-status` (a code
  from 1 to 16; anything larger becomes 2) and `grpc-message`, still works.

Call it with any gRPC client. Plain-text HTTP/2 (h2c) works on the data port, and HTTP/2 over TLS when the data
port has TLS ([Configuration: TLS](../operations/configuration.md#data-plane-tls-and-mtls)). With
[grpcurl](https://github.com/fullstorydev/grpcurl) and [server reflection](#server-reflection) no `.proto` file is
needed on the client:

```sh
grpcurl -plaintext -d '{"id":"A-1"}' localhost:8080 shop.v1.OrderService/GetOrder
```

```json
{
  "id": "A-1",
  "status": "PAID",
  "items": 3
}
```

```sh
grpcurl -plaintext -d '{"id":"Z-9"}' localhost:8080 shop.v1.OrderService/GetOrder
```

```text
ERROR:
  Code: NotFound
  Message: order Z-9 not found
```

On the wire, the first call returns the frame `00 00 00 00 0d 0a 03 41 2d 31 12 04 50 41 49 44 18 03` and the
trailer `grpc-status: 0`; the second returns no frame and the trailers `grpc-status: 5`,
`grpc-message: order Z-9 not found`. Responses also carry `x-fastmock-trace-id`.

## 3. Server streaming

`response.grpc.messages` is the list of messages to stream. Each one is a template and can wait `delayMs`
before it is sent; then the call ends with `response.grpc.status` (OK when absent) and the trailers:

```json
{
  "name": "order status stream",
  "request": {"method": "POST", "urlPath": "/shop.v1.OrderService/WatchOrder"},
  "response": {
    "headers": {"x-region": "eu-1"},
    "grpc": {
      "messages": [
        {"jsonBody": {"orderId": "{{request.json.id}}", "status": "PACKED", "seq": "{{stream.index}}"}},
        {"jsonBody": {"orderId": "{{request.json.id}}", "status": "SHIPPED", "seq": "{{stream.index}}"}, "delayMs": 500},
        {"jsonBody": {"orderId": "{{request.json.id}}", "status": "DELIVERED", "seq": "{{stream.index}}"}, "delayMs": 500}
      ],
      "trailers": {"x-shard": "7"}
    }
  }
}
```

```sh
grpcurl -plaintext -d '{"id":"A-1"}' localhost:8080 shop.v1.OrderService/WatchOrder
```

The three events arrive over one second, followed by `grpc-status: 0` and the trailer `x-shard: 7`.

- **End with an error after N messages:** put the error in `status`. The messages are sent first, then the status:

  ```json
  {"grpc": {
    "messages": [{"jsonBody": {"orderId": "E-1", "status": "PACKED"}},
                 {"jsonBody": {"orderId": "E-1", "status": "SHIPPED"}}],
    "status": {"code": "UNAVAILABLE", "message": "tracking for {{request.json.id}} is down"}
  }}
  ```

- **Repeat:** `"repeat": true` starts `messages` again after the last one, as for [WebSocket](websocket.md) scripts.
  It needs a message with `delayMs > 0`, and the call ends with `status` after `maxMessages` messages (default
  10,000) or after `closeAfterMs`:

  ```json
  {"grpc": {
    "messages": [{"jsonBody": {"orderId": "T-1", "seq": "{{stream.index}}", "at": "{{now}}"}, "delayMs": 1000}],
    "repeat": true, "maxMessages": 60,
    "status": {"code": "RESOURCE_EXHAUSTED", "message": "stream quota used up"}
  }}
  ```

- **No `grpc.messages`:** a stub with only `jsonBody` streams that one message, so stubs written for unary
  methods (and examples imported from Microcks) also answer server-streaming methods.

`response.delay` delays the response headers (the start of the stream); each `delayMs` is capped by
`FASTMOCK_MAX_DELAY`. When the client cancels, FastMock stops sending.

## 4. Client streaming

FastMock reads all request messages, up to the [limits](#limits), and then matches the stubs. For a
client-streaming call the request body is the **JSON array of the messages**, so:

- body matchers see the array: `$[0].sku` is the first message, `$[-1].sku` the last one, `$[*].sku` any message;
- `request.grpc` adds matchers over the messages: `messageCount`, `everyMessage`, `anyMessage` and
  `lastMessage` (each a list of body matchers applied to one message);
- `request.json` in templates is the array.

```json
{
  "name": "upload with a final marker",
  "priority": 1,
  "request": {
    "method": "POST",
    "urlPath": "/shop.v1.OrderService/UploadItems",
    "body": [{"jsonPath": "$[0].sku", "value": {"matches": "A-.*"}}],
    "grpc": {
      "messageCount": {"min": 2, "max": 100},
      "everyMessage": [{"jsonPath": "$.qty", "value": {"matches": "[1-9][0-9]*"}}],
      "lastMessage": [{"jsonPath": "$.sku", "value": {"equalTo": "END"}}]
    }
  },
  "response": {"jsonBody": {
    "count": "{{size request.json}}",
    "firstSku": "{{request.json.[0].sku}}",
    "lastSku": "{{jsonPath request.body '$[-1].sku'}}"
  }}
}
```

A client-streaming method answers with one message: `jsonBody`, or the first entry of `grpc.messages`. An empty
stream is a valid call: `{"messageCount": {"equalTo": 0}}` matches it, and the body is `[]`.

## 5. Bidirectional streaming

A bidirectional call is matched when it starts, from the path and the metadata (headers); at that moment no
message has arrived, so the body is `[]`. The stub then drives the call:

1. `grpc.messages` is an initial burst, sent from the start of the call with its `delayMs` (and `repeat`), as in
   server streaming.
2. Every incoming message is matched against `grpc.onMessage` in order; the first rule whose `match` (body
   matchers on the message, all must match; an empty list accepts every message) accepts it sends the rule's
   `messages`, and a rule with a `status` ends the call with that status.
3. A message that no rule accepts is answered with `jsonBody` when the stub has one, and otherwise ignored.
4. When the client half-closes, the call ends with `grpc.status` (OK when absent) once the initial burst is done
   (at once with `repeat`). `maxMessages` and `closeAfterMs` end it earlier.

Incoming messages are processed one at a time, in order: replies to one message (with their `delayMs`) are sent
before the next message is read. Reply templates see the incoming message as `message.json` and its position as
`message.index`.

```json
{
  "name": "chat",
  "request": {"method": "POST", "urlPath": "/shop.v1.OrderService/Chat"},
  "response": {
    "jsonBody": {"text": "echo {{message.json.text}}", "user": "bot"},
    "grpc": {
      "messages": [{"jsonBody": {"text": "welcome {{request.headers.x-user}}", "user": "bot"}}],
      "onMessage": [
        {"match": [{"jsonPath": "$.text", "value": {"equalTo": "bye"}}],
         "messages": [{"jsonBody": {"text": "bye {{message.json.user}}"}}],
         "status": {"code": "OK"}},
        {"match": [{"jsonPath": "$.text", "value": {"contains": "?"}}],
         "messages": [{"jsonBody": {"text": "answer #{{message.index}}"}, "delayMs": 200}]},
        {"match": [{"jsonPath": "$.text", "value": {"equalTo": "boom"}}],
         "status": {"code": "ABORTED", "message": "boom at #{{message.index}}"}}
      ]
    }
  }
}
```

The client sends `hi`, `what?`, `bye` and receives `welcome ann`, `echo hi`, `answer #1`, `bye ann`, then
`grpc-status: 0`.

## Status, rich error details, metadata and trailers

`response.grpc.status` is the final status of the call:

| Field | Meaning |
|---|---|
| `code` | A number from 0 to 16 or a canonical name: `OK`, `CANCELLED`, `UNKNOWN`, `INVALID_ARGUMENT`, `DEADLINE_EXCEEDED`, `NOT_FOUND`, `ALREADY_EXISTS`, `PERMISSION_DENIED`, `RESOURCE_EXHAUSTED`, `FAILED_PRECONDITION`, `ABORTED`, `OUT_OF_RANGE`, `UNIMPLEMENTED`, `INTERNAL`, `UNAVAILABLE`, `DATA_LOSS`, `UNAUTHENTICATED` (case does not matter). |
| `message` | Template; sent percent-encoded in `grpc-message`. |
| `details` | `google.rpc.Status` details: JSON objects with `@type`, each a template. They are encoded as `google.protobuf.Any` into a `google.rpc.Status` sent in the `grpc-status-details-bin` trailer, which gRPC client libraries expose as rich error details. Needs a non-OK code. |

```json
{"grpc": {
  "status": {
    "code": "INVALID_ARGUMENT",
    "message": "{{size request.json}} items rejected",
    "details": [
      {"@type": "type.googleapis.com/google.rpc.BadRequest",
       "fieldViolations": [{"field": "items[0].sku", "description": "unknown sku"}]},
      {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "2s"},
      {"@type": "type.googleapis.com/google.rpc.ErrorInfo",
       "reason": "OUT_OF_STOCK", "domain": "shop.example.com", "metadata": {"sku": "A-1"}}
    ]
  },
  "trailers": {"x-request-id": "{{uuid}}"}
}}
```

The detail types of `google/rpc/error_details.proto` are built in: `ErrorInfo`, `RetryInfo`, `DebugInfo`,
`QuotaFailure`, `PreconditionFailure`, `BadRequest`, `RequestInfo`, `ResourceInfo`, `Help`, `LocalizedMessage`
(the built-in `QuotaFailure.Violation` has `subject` and `description`). Any message type from your uploaded
descriptor sets works too, and your own copy of `google/rpc/*.proto` takes precedence over the built-in one. A
detail that cannot be encoded (unknown `@type`, wrong field) ends the call with `INTERNAL` and a message that
names the problem.

**Metadata.** The stub's response `headers` are sent as response metadata (initial headers), for example
`"headers": {"x-region": "eu-1"}`. `content-type`, `grpc-*` names and hop-by-hop headers are not copied.
**Trailers.** `grpc.trailers` are sent with the status (values are templates; names ending in `-bin` carry
base64 values, as gRPC requires). `grpc-*` names are reserved for the status.

## Network faults

`response.fault` works on gRPC stubs. A unary or client-streaming call is reset before its message; a server
stream sends its `grpc.messages` first and is then reset instead of ending with a status, which tests how a
client handles a connection that breaks mid-stream; a bidirectional call is reset when it would end.
`RANDOM_DATA_THEN_CLOSE`, `MALFORMED_RESPONSE_CHUNK` and `MALFORMED_JSON` first send a frame of random bytes.
On HTTP/2 the reset is an `RST_STREAM`; clients usually report `INTERNAL` or `UNAVAILABLE`.

```json
{"fault": "CONNECTION_RESET", "grpc": {"messages": [
  {"jsonBody": {"orderId": "R-1", "status": "PACKED"}},
  {"jsonBody": {"orderId": "R-1", "status": "SHIPPED"}, "delayMs": 300}
]}}
```

The journal records such a call with status 14 and the message `fault injected: ConnectionReset`.

## Compression

FastMock accepts gzip-compressed request messages (`grpc-encoding: gzip`, compressed flag set per message;
messages without the flag are read as they are) and sends `grpc-accept-encoding: gzip, identity` on every
response. Another `grpc-encoding` gets `UNIMPLEMENTED`. A compressed message is decompressed up to
`FASTMOCK_MAX_BODY_BYTES`, so a decompression bomb ends with `RESOURCE_EXHAUSTED` after at most that many bytes.

Responses are compressed with gzip when the request was gzip-compressed and the client accepts gzip
(`grpc-accept-encoding`). `"compression": "gzip"` in `response.grpc` compresses whenever the client accepts gzip;
`"compression": "identity"` never compresses.

## Server reflection

FastMock implements `grpc.reflection.v1.ServerReflection` and `grpc.reflection.v1alpha.ServerReflection` from the
uploaded descriptor sets, so grpcurl, Postman, Kreya and other clients list the services and call methods without
`.proto` files:

```sh
grpcurl -plaintext localhost:8080 list
grpcurl -plaintext localhost:8080 describe shop.v1.OrderService
grpcurl -plaintext -d '{"id":"A-1"}' localhost:8080 shop.v1.OrderService/WatchOrder
```

```text
shop.v1.OrderService
```

`ListServices` returns the services of all uploaded descriptor sets (the reflection services themselves are not
listed); `FileByFilename`, `FileContainingSymbol` (services, methods, messages, fields, enums and their values,
extensions), `FileContainingExtension` and `AllExtensionNumbersOfType` return the file with its imports. Mocks are
test infrastructure, so reflection is on by default; `FASTMOCK_GRPC_REFLECTION=false` turns it off, and the
reflection paths then answer `UNIMPLEMENTED` like any unknown method. Reflection calls are not journaled and do not
use stubs. Descriptor sets are global, so reflection shows every uploaded service whatever the
[space](../operations/access-control.md) of the stubs.

## gRPC-Web

Browser apps can call mocks with gRPC-Web: `application/grpc-web`, `application/grpc-web+proto`,
`application/grpc-web-text` and `application/grpc-web-text+proto`, over HTTP/1.1 or HTTP/2. The stubs are the
same as for gRPC; the status and trailers arrive as the last frame of the response body (flag `0x80`), and the
text variants are base64-encoded. Unary and server-streaming calls work from browsers; client-streaming and
bidirectional calls need a client that can stream a request body, which browsers cannot.

CORS is handled as for HTTP stubs: answer the preflight with a stub and add the CORS headers to the gRPC stubs
(they are sent as response metadata). Browsers only let scripts read `grpc-status` and `grpc-message` from
headers listed in `access-control-expose-headers`:

```json
{"name": "gRPC-Web preflight", "priority": 1,
 "request": {"method": "OPTIONS", "urlPathPattern": "/shop\\.v1\\..*"},
 "response": {"status": 204, "headers": {
   "access-control-allow-origin": "https://shop.example.com",
   "access-control-allow-methods": "POST",
   "access-control-allow-headers": "content-type,x-grpc-web,x-user-agent,grpc-timeout",
   "access-control-max-age": "600"}}}
```

```json
{"request": {"method": "POST", "urlPath": "/shop.v1.OrderService/GetOrder"},
 "response": {
   "headers": {"access-control-allow-origin": "https://shop.example.com",
               "access-control-expose-headers": "grpc-status,grpc-message"},
   "jsonBody": {"id": "{{request.json.id}}", "status": "PAID"}}}
```

Give the preflight stub a lower `priority` number than the gRPC stubs, or set `"method": "POST"` on them, so the
`OPTIONS` request does not match a gRPC stub with an exact path.

## Templates

All texts in `grpc` are Handlebars templates unless the response has `"templated": false`
([Templating](templating.md)). Besides the usual `request.*` variables:

| Variable | Value |
|---|---|
| `request.json` | The request message (unary, server streaming); the array of messages (client streaming); `[]` (bidirectional) |
| `stream.index` | Position of the message being sent among the messages sent so far, from 0 (`grpc.messages`) |
| `message.json` | The incoming message (bidirectional replies, `jsonBody` fallback, rule status) |
| `message.index` | Position of the incoming message, from 0 |
| `state.*` | The scenario state, as for HTTP stubs |

Use `"{{json ...}}"` to insert a value of another type, and quote numbers as strings (`"seq": "{{stream.index}}"`):
the proto3 JSON mapping accepts strings for numeric fields.

## Field reference

`response.grpc`:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `messages` | array | none | `{"jsonBody": object, "delayMs": n}`: messages in order; `delayMs` (0..3,600,000) is the pause before the message. At most 1,000, each at most 1 MiB. |
| `repeat` | boolean | `false` | Start `messages` again after the last one (streams). Needs a message with `delayMs > 0`. |
| `maxMessages` | integer | 10,000 | End the call with `status` after this many messages, 1..100,000. |
| `closeAfterMs` | integer | stream duration | End the call with `status` after this time, 1..86,400,000; capped by `FASTMOCK_GRPC_MAX_STREAM_DURATION`. |
| `status` | object | OK | `{"code", "message", "details"}`, see [above](#status-rich-error-details-metadata-and-trailers). |
| `trailers` | object | none | Trailing metadata, at most 64 entries. |
| `onMessage` | array | none | Bidirectional reply rules `{"match": [body matchers], "messages": [...], "status": {...}}`; a rule needs messages, a status or both. At most 64 rules with 100 messages and 32 matchers each. |
| `compression` | string | follows the request | `gzip` or `identity`. |

`request.grpc`:

| Field | Type | Meaning |
|---|---|---|
| `messageCount` | object | `{"equalTo": n}`, `{"min": n}`, `{"max": n}` or `min` and `max` together. |
| `everyMessage` | array | Body matchers every message must match (an empty stream matches). |
| `anyMessage` | array | Body matchers at least one message must match. |
| `lastMessage` | array | Body matchers the last message must match. |

At most 32 matchers in `request.grpc`; they count towards the 64 matchers of a stub. For unary and
server-streaming calls the list of messages is the one request message.

`response.grpc` cannot be combined with `proxy` ([proxying](#proxying-and-recording)), `sse` or `websocket`. `response.status` (the HTTP status) does
not apply to gRPC calls, which always answer HTTP 200.

## Limits

| Limit | Default | Setting |
|---|---|---|
| Size of one request message, compressed and decompressed | 10 MiB | `FASTMOCK_MAX_BODY_BYTES` |
| Request messages per call | 10,000 | `FASTMOCK_GRPC_MAX_STREAM_MESSAGES` |
| Decompressed request bytes per call | 64 MiB | `FASTMOCK_GRPC_MAX_STREAM_BYTES` |
| Wait for the next request message | 60 s | `FASTMOCK_GRPC_STREAM_IDLE_TIMEOUT` |
| Duration of a call | 1 hour | `FASTMOCK_GRPC_MAX_STREAM_DURATION` |
| Concurrent HTTP/2 streams per connection | 200 | `FASTMOCK_HTTP2_MAX_CONCURRENT_STREAMS` |
| Response messages per call | 10,000 (stub: up to 100,000) | `grpc.maxMessages` |
| Pause before a message | 15 min | `FASTMOCK_MAX_DELAY` |

A call that breaks a request limit ends with `RESOURCE_EXHAUSTED` (8) (size and count) or `DEADLINE_EXCEEDED` (4)
(idle timeout, duration), counted in `fastmock_grpc_stream_limits_total`. A client that streams forever is
stopped at the message, byte or duration limit, whichever comes first. The messages of a client stream are kept in
memory until the call is matched and count against `FASTMOCK_MAX_INFLIGHT_BODY_BYTES`; when that budget is used up
the call ends with `RESOURCE_EXHAUSTED`.

## Status codes FastMock returns itself

| Situation | Status |
|---|---|
| no descriptor contains the method | `UNIMPLEMENTED` (12), `unknown gRPC method /x.Y/Z: upload its descriptor set first` |
| no stub matched | `UNIMPLEMENTED` (12), `no stub matched /x.Y/Z` |
| unsupported `grpc-encoding` | `UNIMPLEMENTED` (12) |
| malformed or truncated frame, message that does not decode, compressed flag without `grpc-encoding: gzip`, invalid gzip | `INTERNAL` (13) |
| unary or server-streaming call without exactly one request message | `INTERNAL` (13) |
| message, byte or memory limit reached; decoded request larger than twice `FASTMOCK_MAX_BODY_BYTES` | `RESOURCE_EXHAUSTED` (8) |
| idle timeout or maximum duration reached | `DEADLINE_EXCEEDED` (4) |
| a stub message does not fit the output type, a template or detail fails, stub evaluation failed | `INTERNAL` (13) |

Requests are recognised as gRPC by `content-type: application/grpc` (including `application/grpc+proto` and the
gRPC-Web types).

## Journal, verification and metrics

Each call, whatever its kind, is one journal entry, written when the call ends:

- `body` is the request message as JSON (`{"id":"Z-9"}`), or the JSON array of the messages for client-streaming
  and bidirectional calls; `responseBody` is the response message, or the array of the messages sent for
  server-streaming and bidirectional calls;
- `grpc` holds `kind` (`unary`, `serverStreaming`, `clientStreaming`, `bidiStreaming`), the final `status` and
  `message`, `requestMessages` and `responseMessages` (the full counts), `protocol` (`grpc-web` or
  `grpc-web-text` for gRPC-Web), and `requestTruncated` / `responseTruncated` when the journal kept only the first
  messages (the arrays stay valid JSON and stop at `FASTMOCK_JOURNAL_MAX_BODY_BYTES`);
- `durationMs` is the duration of the whole call.

```json
{"method": "POST", "path": "/shop.v1.OrderService/UploadItems", "status": 200,
 "body": "[{\"sku\":\"A-1\",\"qty\":2},{\"sku\":\"END\",\"qty\":1}]",
 "responseBody": "{\"count\":2,\"firstSku\":\"A-1\",\"lastSku\":\"END\"}",
 "grpc": {"kind": "clientStreaming", "status": 0, "requestMessages": 2, "responseMessages": 1}}
```

[Verification](verification.md), `POST /__admin/requests/find` and near misses use the same patterns as stubs,
including `request.grpc`; `messageCount` uses the full count even when the journal kept fewer messages:

```sh
curl -s -X POST localhost:9090/__admin/verify -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
  "request": {"urlPath": "/shop.v1.OrderService/UploadItems",
              "grpc": {"messageCount": {"equalTo": 2},
                       "lastMessage": [{"jsonPath": "$.sku", "value": {"equalTo": "END"}}]}},
  "count": 1
}'
```

A near miss names the failing part, for example `gRPC message count does not match` or `not every gRPC message
matches request.grpc.everyMessage`. The UI shows gRPC journal entries as numbered message lists with the status,
and the stub editor summarises `request.grpc` and `response.grpc` (edit them in the JSON tab).

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_grpc_calls_total` | counter | `method` | Calls per method of the uploaded descriptors. |
| `fastmock_grpc_stream_messages_total` | counter | `direction` (`in`, `out`), `kind` (`unary`, `server_streaming`, `client_streaming`, `bidi_streaming`) | Messages received and sent. |
| `fastmock_grpc_streams_active` | gauge | none | Calls sending their response on this replica. |
| `fastmock_grpc_stream_limits_total` | counter | `limit` (`message_bytes`, `messages`, `stream_bytes`, `idle`, `duration`) | Calls ended by a limit. |
| `fastmock_grpc_reflection_calls_total` | counter | none | Server reflection streams. |

The usual request metrics (`fastmock_requests_total`, `fastmock_stub_hits_total`, ...) count gRPC calls too.

## Proxying and recording

A stub with `response.proxy` forwards the gRPC calls it matches to a real gRPC server, and while a
[recording](recording.md#grpc-calls) runs, calls that match no stub go to its `grpcTargetBaseUrl` and become stubs.
A low-priority catch-all proxy stub sends everything that no specific stub answers to the real service (partial
mocking); put it in a [space](../operations/access-control.md#spaces) to give that space its own upstream:

```json
{"name": "orders upstream", "priority": 100,
 "request": {"method": "POST", "urlPathPattern": "/shop\\.v1\\.OrderService/.*"},
 "response": {"proxy": {"baseUrl": "https://orders.example.com", "additionalHeaders": {"x-env": "test"}}}}
```

| Aspect | Behaviour |
|---|---|
| Upstream | `baseUrl` + the method path (`/shop.v1.OrderService/GetOrder`, minus `stripPrefix` when set). `http://` speaks HTTP/2 without TLS (h2c), `https://` HTTP/2 over TLS, verified against the public roots and `FASTMOCK_OUTBOUND_CA_FILE`; the upstream must negotiate ALPN `h2`. FastMock sends only the HTTP/2 connection preface until the upstream has answered with a SETTINGS frame, so a server that does not speak HTTP/2 receives no request metadata or messages. |
| mTLS | `FASTMOCK_GRPC_PROXY_CLIENT_CERT_FILE` and `_KEY_FILE` are presented only over TLS, only to an upstream whose `host:port` is in `FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS`, and only for proxy stubs of the global area and recordings. A stub of a space whose `proxy.baseUrl` is such an upstream is refused with `400` when it is saved, and a call through one that is stored anyway gets no certificate. A proxy stub to a certificate host lets every client that reaches the data plane call that upstream with FastMock's identity, for the methods of the uploaded descriptors. |
| Egress | The [egress policy](../operations/security.md#egress-policy): the host must be in `FASTMOCK_CALLBACK_ALLOWED_HOSTS` (when set), and the upstream is reached only on addresses outside `FASTMOCK_CALLBACK_DENIED_CIDRS`. |
| Calls | Unary, server-streaming, client-streaming and bidirectional calls, from gRPC and gRPC-Web clients (the upstream is always spoken to with gRPC). Messages are passed on as they are, in both directions; the request messages read for matching are sent first, then the rest of a bidirectional stream as it arrives. |
| Metadata | Request metadata is forwarded except `content-type`, `grpc-encoding`, hop-by-hop and routing headers (as for [HTTP proxying](proxy-and-chaos.md#proxy-stubs)) and `removeHeaders`; `additionalHeaders` are added. Response metadata, the status with its message and rich details, and trailers come back unchanged. |
| Limits | The upstream's messages have the [limits](#limits) of request messages (size, count, bytes, idle time, call duration); `FASTMOCK_GRPC_PROXY_MAX_CALLS` (default 256) proxied calls run at once per replica, more end with `RESOURCE_EXHAUSTED`. Messages in flight, in both directions, count against `FASTMOCK_MAX_INFLIGHT_BODY_BYTES` (twice their size, for the frame and the decoded copy); when it is used up the call ends with `RESOURCE_EXHAUSTED` and `server busy`. A client or upstream that takes nothing for `FASTMOCK_GRPC_STREAM_IDLE_TIMEOUT` ends the call. Messages larger than the journal keeps (`FASTMOCK_JOURNAL_MAX_BODY_BYTES`, or 1 MiB while recording) are passed on without being decoded. Each call has its own upstream connection. |
| Loops | FastMock adds `x-fastmock-proxy-hops` (the number of FastMock proxies the call went through) to proxied calls and does not proxy a call that already went through 3, nor to its own listeners; such calls end with `UNAVAILABLE`. HTTP proxy stubs and recordings use the same header and answer `502` with `proxy loop`. |
| Failure | An upstream that cannot be reached, is blocked or sends no response headers within `FASTMOCK_GRPC_STREAM_IDLE_TIMEOUT` ends the call with `UNAVAILABLE` and `proxy failed: upstream unavailable` (or `blocked by egress policy`); the log has the reason, `fastmock_proxy_errors_total` counts it. A non-200 HTTP answer becomes the status gRPC clients derive from it (`404` is `UNIMPLEMENTED`, `503` is `UNAVAILABLE`, ...). |
| Switch | `proxyPassThrough: false` in the [settings](responses.md#proxying) answers proxied gRPC calls with `UNIMPLEMENTED` and `proxy pass-through is disabled` instead. |
| Journal | One entry per call with the messages in both directions decoded with the descriptors, the status and the proxy stub's id. |

The descriptor set of the service must be uploaded, also for proxying: FastMock decodes the messages for matching
and the journal. `fastmock_grpc_proxied_calls_total{target="stub"|"recording"}` counts proxied calls.

## Other features

- `response.delay`, scenarios, [callbacks](callbacks.md) and [Kafka messages](kafka.md) work on gRPC stubs; they
  run once per call, when the call is matched.
- gRPC stubs are ordinary stubs: tags, import/export and the [Java client](java-client.md) handle them like any
  other stub. In the request headers seen by matchers and templates, `content-type` is `application/json`.
- [Microcks APIExamples](microcks-artifacts.md#apiexamples) for gRPC services become stubs for unary and
  server-streaming methods (the example response is the one message of the stream). Examples of client-streaming
  and bidirectional methods are skipped with a warning; write those stubs with `request.grpc` and
  `response.grpc`.
- `proxy` stubs and [recording](recording.md) work for gRPC calls of every kind, see
  [Proxying and recording](#proxying-and-recording).
