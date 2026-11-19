# Getting started

A ten-minute tour: start FastMock, create a token, add a stub, call it, look at the journal, open the UI, send a
signed callback and build a small stateful flow. At the end you run the same thing on PostgreSQL and Valkey with
Docker Compose.

You need Docker (or a Rust toolchain), `curl`, and optionally `jq`.

- [1. Build the image](#1-build-the-image)
- [2. Create an admin token](#2-create-an-admin-token)
- [3. Start FastMock in memory mode](#3-start-fastmock-in-memory-mode)
- [4. Your first stub](#4-your-first-stub)
- [5. Look at the journal](#5-look-at-the-journal)
- [6. Open the UI and the API description](#6-open-the-ui-and-the-api-description)
- [7. Your first callback](#7-your-first-callback)
- [8. Your first scenario](#8-your-first-scenario)
- [9. Shared storage with Docker Compose](#9-shared-storage-with-docker-compose)
- [Next steps](#next-steps)

## 1. Build the image

From the repository root:

```sh
docker build -t fastmock:0.1.0 .
```

The image is distroless, runs as user 65532 and starts `fastmock serve`. Without Docker, build the binary
instead: `cargo build --release -p fastmock-server` (the binary is `target/release/fastmock`; build the UI first
with `npm ci && npm run build` in `ui/` if you want it embedded).

## 2. Create an admin token

The admin API always needs a token. FastMock stores only the SHA-256 of each token:

```sh
docker run --rm fastmock:0.1.0 hash-token --generate
```

```text
token: fm_3c1f...e9a0
hash:  6b0d...41c2
```

Keep the token for your requests and give the server the hash, together with a name and a role
(`viewer`, `editor` or `admin`):

```sh
export FASTMOCK_TOKEN=fm_3c1f...e9a0
export FASTMOCK_TOKEN_HASH=6b0d...41c2
```

## 3. Start FastMock in memory mode

Memory mode keeps everything in the process: ideal for a laptop or a CI job.

```sh
docker run --rm --name fastmock --hostname fastmock \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:9090:9090 \
  -e FASTMOCK_STORAGE=memory \
  -e FASTMOCK_ADMIN_TOKENS="me:admin:$FASTMOCK_TOKEN_HASH" \
  -e FASTMOCK_SECRET_DELIVERY_HOOK=change-me-to-a-long-random-key \
  -e FASTMOCK_STUB_SECRETS=delivery-hook \
  -e FASTMOCK_LOG_FORMAT=text \
  fastmock:0.1.0
```

Port 8080 is the **data plane** (the mocks), port 9090 the **control plane** (admin API, UI, metrics). The two
`SECRET` variables prepare step 7. Check that it is up:

```sh
curl -s localhost:9090/readyz
curl -s localhost:9090/__admin/info -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq '{version, storage, principal}'
```

```json
{"status":"ready","valkey":"ok","database":"ok"}
```

```json
{"version": "0.1.0", "storage": "memory", "principal": {"name": "me", "role": "admin"}}
```

Without a valid token the admin API answers `401 {"error":"unauthorized","details":[]}`.

## 4. Your first stub

```sh
curl -s -X POST localhost:9090/__admin/stubs \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "name": "hello",
  "request":  {"method": "GET", "urlPathTemplate": "/hello/{name}"},
  "response": {"status": 200, "jsonBody": {"hello": "{{request.pathParams.name}}", "id": "{{uuid}}"}}
}'
```

The answer is the stored stub with its generated `id` (HTTP 201):

```json
{"id":"101cb2ea-eead-441a-b122-cb561dfcab0c","name":"hello","priority":5,"request":{"method":"GET","urlPathTemplate":"/hello/{name}"},"response":{"status":200,"jsonBody":{"hello":"{{request.pathParams.name}}","id":"{{uuid}}"},"templated":true},"enabled":true,"updatedAt":"2026-11-19T05:48:12.144164047Z"}
```

Call it on the data plane:

```sh
curl -si localhost:8080/hello/world
```

```text
HTTP/1.1 200 OK
content-type: application/json
x-fastmock-trace-id: ae1797334b4072c3c626b263e727b99a
content-length: 61

{"hello":"world","id":"14923b23-f1b8-40d5-8877-33b9a099df74"}
```

The body is a template: `{{request.pathParams.name}}` comes from the path and `{{uuid}}` is new on every call.
Every response carries `x-fastmock-trace-id`, which you can look up in the journal, the logs and your tracing
backend. A request that matches nothing gets a 404:

```sh
curl -s localhost:8080/nothing-here
```

```json
{"error":"no stub matched","method":"GET","path":"/nothing-here"}
```

## 5. Look at the journal

Every data-plane request is recorded, with secrets masked:

```sh
curl -s "localhost:9090/__admin/requests?limit=2" -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq '.requests[] | {path, status, matched, stubId, traceId}'
```

```json
{"path": "/nothing-here", "status": 404, "matched": false, "stubId": null, "traceId": "19687de22f6687bd367b1630b809e89f"}
{"path": "/hello/world", "status": 200, "matched": true, "stubId": "101cb2ea-eead-441a-b122-cb561dfcab0c", "traceId": "ae1797334b4072c3c626b263e727b99a"}
```

Ask why a request did not match:

```sh
curl -s -X POST localhost:9090/__admin/requests/near-misses \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"method": "GET", "path": "/hello"}'
```

The answer lists the closest stubs with a score and the conditions that failed. Filters, counting and waiting are
described in [Verification](verification.md).

## 6. Open the UI and the API description

| URL | What |
|---|---|
| <http://localhost:9090/ui> | Web UI: stubs, services, scenarios, request journal, near misses, verification, callbacks, recordings, chaos, key-value store, audit log. Sign in with your token. |
| <http://localhost:9090/ui/api> | Swagger UI for the admin API, inside the web UI. |
| <http://localhost:9090/openapi.json> | OpenAPI 3.1 description of the admin API, for code generation and tooling. |

The UI uses the same token and roles as the API; see [Web UI](ui.md) and [Admin API](admin-api.md).

## 7. Your first callback

A payment provider answers "pending" and later calls your webhook. Here FastMock plays both sides: one stub is
the provider, another receives the webhook so you can see it arrive.

```sh
curl -s -X POST localhost:9090/__admin/stubs \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "name": "webhook receiver",
  "request":  {"method": "POST", "urlPath": "/hooks/payments"},
  "response": {"status": 204}
}'

curl -s -X POST localhost:9090/__admin/stubs \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "name": "create payment",
  "request":  {"method": "POST", "urlPath": "/payments"},
  "response": {"status": 202, "jsonBody": {"id": "{{request.json.orderId}}", "status": "PENDING"}},
  "callbacks": [{
    "url": "http://fastmock:8080/hooks/payments",
    "jsonBody": {"orderId": "{{request.json.orderId}}", "status": "PAID"},
    "delay": {"fixedMs": 1000},
    "correlation": "{{request.json.orderId}}",
    "hmac": {"header": "X-Signature", "secretRef": "delivery-hook", "prefix": "sha256="}
  }]
}'

curl -s -X POST localhost:8080/payments -H 'content-type: application/json' -d '{"orderId": "ord-1001", "amount": 250}'
```

```json
{"id":"ord-1001","status":"PENDING"}
```

The callback URL uses the container's host name `fastmock` (set with `--hostname`), because FastMock never sends
callbacks to loopback addresses such as `localhost`. One second later the callback has been delivered:

```sh
curl -s "localhost:9090/__admin/callbacks?correlation=ord-1001" -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq '.callbacks[0] | {status, attempts, lastStatus, headers, body}'
```

```json
{
  "status": "sent",
  "attempts": 1,
  "lastStatus": 204,
  "headers": {"content-type": "application/json", "x-signature": "***"},
  "body": "{\"orderId\":\"ord-1001\",\"status\":\"PAID\"}"
}
```

The receiver got the body, an HMAC-SHA256 signature in `X-Signature` (masked in the API), the delivery id and a
`traceparent` that continues the trace of the original request. Failed deliveries are retried with backoff. See
[Callbacks](callbacks.md) for retries, signatures and receiver-side verification.

## 8. Your first scenario

Scenarios make a mock stateful. Here an order can be paid once; the second attempt is refused:

```sh
curl -s -X POST localhost:9090/__admin/stubs/import \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
"replaceTag": "tour",
"stubs": [
  {"name": "pay order", "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/pay"},
   "scenario": {"name": "order-{{request.pathParams.id}}", "requiredState": "Started", "newState": "PAID"},
   "response": {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "PAID"}}},
  {"name": "already paid", "priority": 6, "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/pay"},
   "response": {"status": 409, "jsonBody": {"error": "order {{request.pathParams.id}} cannot be paid"}}}
]}'

curl -s -X POST localhost:8080/orders/A-1/pay
curl -s -X POST localhost:8080/orders/A-1/pay
curl -s -X POST localhost:8080/orders/B-2/pay
curl -s localhost:9090/__admin/scenarios -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```text
{"imported":2,"removed":0}
{"id":"A-1","status":"PAID"}
{"error":"order A-1 cannot be paid"}
{"id":"B-2","status":"PAID"}
{"scenarios":[{"name":"order-A-1","state":"PAID"},{"name":"order-B-2","state":"PAID"}]}
```

Each order has its own state because the scenario name is a template. Reset all states with
`curl -X DELETE localhost:9090/__admin/scenarios -H "Authorization: Bearer $FASTMOCK_TOKEN"`. More in
[Scenarios and state](scenarios-and-state.md).

Stop the container with Ctrl-C; in memory mode everything is gone.

## 9. Shared storage with Docker Compose

For a shared test environment FastMock keeps its data in PostgreSQL 18 and Valkey 9, so it survives restarts and
any number of replicas share stubs, scenarios, the journal and the callback queue.
[`docs/examples/docker-compose.yaml`](../examples/docker-compose.yaml) starts all three. Create a `.env` file next
to it:

```sh
cd docs/examples
cat > .env <<EOF
POSTGRES_PASSWORD=$(openssl rand -hex 24)
VALKEY_PASSWORD=$(openssl rand -hex 24)
DELIVERY_HOOK_SECRET=$(openssl rand -hex 32)
FASTMOCK_ADMIN_TOKENS=me:admin:$FASTMOCK_TOKEN_HASH
EOF
chmod 600 .env
docker compose up -d
docker compose logs fastmock | grep -E 'migrations|storage ready|FastMock started'
```

```text
fastmock-1  | 2026-11-19T06:40:41.867428Z  INFO fastmock_store: applying database migrations
fastmock-1  | 2026-11-19T06:40:41.887368Z  INFO fastmock_store: storage ready
fastmock-1  | 2026-11-19T06:40:41.901674Z  INFO fastmock_server: FastMock started listen=0.0.0.0:8080 admin_listen=0.0.0.0:9090 plugins=[...] otel=false
```

```sh
curl -s localhost:9090/readyz
curl -s localhost:9090/__admin/info -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq .storage
```

```json
{"status":"ready","valkey":"ok","database":"ok"}
```

```text
"postgres+valkey"
```

Steps 4 to 8 work unchanged (in Compose the service name `fastmock` resolves to the FastMock container, so the
callback URL stays the same). Now stubs and scenario states survive `docker compose restart fastmock`, and you
could run more FastMock containers against the same databases. The ports are bound to `127.0.0.1` only; the
databases are not published at all.

Stop and remove everything, including the data, with `docker compose down -v`.

## Next steps

- Learn the model: [Concepts](concepts.md).
- Write mocks: [Request matching](request-matching.md), [Responses](responses.md), [Templating](templating.md).
- Bring existing mocks: [Importing](importing.md), [Migrating from WireMock](migrating-from-wiremock.md),
  [Migrating from MockServer](migrating-from-mockserver.md), [Migrating from Microcks](migrating-from-microcks.md).
- Test with Java: [Java client](java-client.md).
- Run it in Kubernetes: [Helm chart](../operations/helm.md) or [Operator](../operations/operator.md).
- Everything else: the [documentation index](../README.md).
