# gRPC

FastMock answers unary gRPC calls on the data port with ordinary stubs. You upload the service descriptors once;
FastMock then decodes each request message to JSON, matches it with the usual matchers, and encodes the stub's
JSON response into the protobuf output message. No code generation and no `.proto` compilation inside FastMock.

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
}

message GetOrderRequest { string id = 1; }
message Order { string id = 1; string status = 2; int32 items = 3; }
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
within `FASTMOCK_STUB_POLL` (5 s). They survive restarts.

## 2. Create stubs

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
  "response": {"headers": {"grpc-status": "5", "grpc-message": "order {{request.json.id}} not found"}}
}'
```

- **Request:** the input message is decoded with the uploaded descriptors and converted with the proto3 JSON
  mapping (JSON field names in lowerCamelCase, 64-bit integers as strings, enums as names, fields with default
  values omitted). All body matchers and `request.json` in templates work on that JSON. Request metadata is
  available as headers.
- **Response:** `jsonBody` (with templates) is the output message in the same JSON mapping. A field that does not
  exist in the output type, or a value of the wrong type, ends the call with `INTERNAL` (13) and a message that
  names the problem.
- **Errors:** set the response headers `grpc-status` (a code from 1 to 16; anything larger becomes 2) and
  optionally `grpc-message`. With a non-zero status no message is sent.

## 3. Call it

Plain-text HTTP/2 (h2c) works on the data port, and HTTP/2 over TLS when the data port has TLS
([Configuration: TLS](../operations/configuration.md#data-plane-tls-and-mtls)). With
[grpcurl](https://github.com/fullstorydev/grpcurl):

```sh
grpcurl -plaintext -protoset orders.pb -d '{"id":"A-1"}' localhost:8080 shop.v1.OrderService/GetOrder
```

```json
{
  "id": "A-1",
  "status": "PAID",
  "items": 3
}
```

```sh
grpcurl -plaintext -protoset orders.pb -d '{"id":"Z-9"}' localhost:8080 shop.v1.OrderService/GetOrder
```

```text
ERROR:
  Code: NotFound
  Message: order Z-9 not found
```

On the wire (captured with `curl --http2-prior-knowledge`), the first call returns the frame
`00 00 00 00 0d 0a 03 41 2d 31 12 04 50 41 49 44 18 03` and the trailer `grpc-status: 0`; the second returns no
frame and the trailers `grpc-status: 5`, `grpc-message: order Z-9 not found`. Responses also carry
`x-fastmock-trace-id`.

## Status codes FastMock returns itself

| Situation | Status |
|---|---|
| no descriptor contains the method | `UNIMPLEMENTED` (12), `unknown gRPC method /x.Y/Z: upload its descriptor set first` |
| no stub matched | `UNIMPLEMENTED` (12), `no stub matched /x.Y/Z` |
| compressed request message | `UNIMPLEMENTED` (12) |
| malformed or truncated frame, message that does not decode | `INTERNAL` (13) |
| decoded request larger than twice `FASTMOCK_MAX_BODY_BYTES` | `RESOURCE_EXHAUSTED` (8) |
| stub `jsonBody` does not fit the output type, or stub evaluation failed | `INTERNAL` (13) |

Requests are recognised as gRPC by `content-type: application/grpc` (including `application/grpc+proto`).
Only unary calls are supported: one request message, one response message.

## Journal, metrics, other features

- The journal stores the decoded request as JSON (`"body": "{\"id\":\"Z-9\"}"`), so verification and
  near-miss analysis work as for HTTP.
- `fastmock_grpc_calls_total{method="/shop.v1.OrderService/GetOrder"}` counts calls per method, in addition to
  the usual request metrics.
- `response.delay`, scenarios, [callbacks](callbacks.md) and [Kafka messages](kafka.md) work on gRPC stubs.
- gRPC stubs are ordinary stubs: tags, import/export and the [Java client](java-client.md) handle them like
  any other stub. In the request headers seen by matchers and templates, `content-type` is
  `application/json`.
