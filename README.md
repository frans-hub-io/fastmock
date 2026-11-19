# FastMock

FastMock is an API mock server for test environments. It mocks HTTP/1.1, HTTP/2 and HTTPS with mTLS, gRPC,
GraphQL, SOAP, WebSocket and Server-Sent Events, publishes Kafka messages (JSON or Confluent Avro), runs AsyncAPI
mocks, and sends durable, signed callbacks with retries. State lives in PostgreSQL and Valkey, so any number of
replicas share stubs, scenarios, the request journal and the callback queue. It runs as a single binary or
container, as a Helm release, or through a Kubernetes operator.

The control plane (admin API, web UI, OpenAPI description, metrics, health) listens on its own port and always
requires a token.

**Documentation: [docs/README.md](docs/README.md)**

## Highlights

| | |
|---|---|
| **Durable callbacks** | Webhooks are written to an outbox before the response, delivered by any replica after a delay, retried with backoff on network errors, 408, 429 and 5xx, signed with HMAC from a server-side secret, idempotent by `x-fastmock-delivery-id`, and visible per delivery in the API. [Callbacks](docs/guide/callbacks.md) |
| **Shared state, any number of replicas** | Stubs, journal, audit and outbox in PostgreSQL; scenarios, key-value entries and counters in Valkey with atomic transitions. A test can call one replica and verify through another. [Concepts](docs/guide/concepts.md#running-several-replicas) |
| **Security by default** | Separate admin port, no start without tokens, viewer/editor/admin roles, origin checks, security settings only at start-up, templates without code execution, one egress policy against SSRF, secrets masked in journal and logs, bounded inputs, audit log. [Security](docs/operations/security.md) |
| **Observability** | Prometheus metrics with bounded labels, OpenTelemetry traces that continue through callbacks, JSON logs with trace ids, a Grafana dashboard and alert rules in the chart. [Observability](docs/operations/observability.md) |
| **Bring your existing mocks** | Imports WireMock mappings, MockServer expectations, OpenAPI, AsyncAPI, Postman, HAR, WSDL/SoapUI, GraphQL schemas and Microcks artifacts; records real traffic. [Importing](docs/guide/importing.md) |
| **Kubernetes native** | Helm chart with NetworkPolicy, PDB, HPA, ServiceMonitor, PrometheusRule, Gateway API; operator with `FastMock` and `MockApi` resources and policies for images, Secrets and settings. [Helm](docs/operations/helm.md), [Operator](docs/operations/operator.md) |

FastMock covers the features that test suites typically use in WireMock 3, MockServer 5 and Microcks 1:

| Coming from | FastMock equivalent |
|---|---|
| WireMock mappings, response templating, scenarios, webhooks, faults, dribble delays, proxying, recording, verification and near misses | Native stubs, [import](docs/guide/import-wiremock-mockserver.md#wiremock), [migration guide](docs/guide/migrating-from-wiremock.md) |
| MockServer expectations, `times`, `timeToLive`, forwards, verification and sequences | Native stubs, [import](docs/guide/import-wiremock-mockserver.md#mockserver), [migration guide](docs/guide/migrating-from-mockserver.md) |
| Microcks artifacts (OpenAPI examples, SoapUI, Postman, GraphQL, gRPC, AsyncAPI), service catalog, contract tests | [Microcks artifacts](docs/guide/microcks-artifacts.md), [Service catalog](docs/guide/service-catalog.md), [Contract testing](docs/guide/contract-testing.md), [migration guide](docs/guide/migrating-from-microcks.md) |

## Quick start

```sh
docker build -t fastmock:0.1.0 .
docker run --rm fastmock:0.1.0 hash-token --generate      # prints a token and its SHA-256
export FASTMOCK_TOKEN=<token>

docker run --rm -p 127.0.0.1:8080:8080 -p 127.0.0.1:9090:9090 \
  -e FASTMOCK_STORAGE=memory -e FASTMOCK_ADMIN_TOKENS="me:admin:<sha256>" fastmock:0.1.0

curl -s -X POST localhost:9090/__admin/stubs -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
  "request":  {"method": "GET", "urlPathTemplate": "/hello/{name}"},
  "response": {"status": 200, "jsonBody": {"hello": "{{request.pathParams.name}}"}}
}'
curl -s localhost:8080/hello/world        # {"hello":"world"}
```

Mocks are served on port 8080; the admin API, the UI (`/ui`), the Swagger UI (`/ui/api`), the OpenAPI document
(`/openapi.json`), `/metrics`, `/healthz` and `/readyz` on port 9090. Without Docker:
`cargo build --release -p fastmock-server` builds `target/release/fastmock`.

For shared environments run PostgreSQL 18 and Valkey 9 and set `FASTMOCK_STORAGE=postgres`,
`FASTMOCK_DATABASE_URL` and `FASTMOCK_VALKEY_URL`; [docs/examples/docker-compose.yaml](docs/examples/docker-compose.yaml)
starts all three. The [Getting started](docs/guide/getting-started.md) tour continues with the journal, the UI,
callbacks and scenarios.

## A stub

```json
{
  "name": "send sms",
  "priority": 5,
  "tags": ["billing"],
  "request": {
    "method": "POST",
    "urlPathTemplate": "/api/{version}/messages",
    "pathParams": {"version": {"matches": "v[12]"}},
    "headers": {"x-api-key": {}},
    "body": [
      {"jsonPath": "$.to", "value": {"matches": "380\\d{9}"}},
      {"jsonSchema": {"type": "object", "required": ["to", "text"]}}
    ]
  },
  "response": {
    "status": 202,
    "jsonBody": {"id": "{{uuid}}", "to": "{{request.json.to}}"},
    "delay": {"minMs": 50, "maxMs": 200}
  },
  "scenario": {"name": "sms-{{request.json.to}}", "newState": "SENT", "ttlSeconds": 3600},
  "callbacks": [{
    "url": "http://app.example.svc/callbacks/delivery",
    "jsonBody": {"to": "{{request.json.to}}", "status": "DELIVERED"},
    "delay": {"fixedMs": 2000},
    "maxAttempts": 5,
    "correlation": "{{request.json.to}}",
    "hmac": {"header": "X-Signature", "secretRef": "delivery-hook", "prefix": "sha256="}
  }],
  "messages": [{"topic": "sms-events", "key": "{{request.json.to}}", "jsonValue": {"to": "{{request.json.to}}"}}]
}
```

[Request matching](docs/guide/request-matching.md), [Responses](docs/guide/responses.md),
[Templating](docs/guide/templating.md), [Scenarios](docs/guide/scenarios-and-state.md),
[Callbacks](docs/guide/callbacks.md), [Kafka](docs/guide/kafka.md).

## Testing with Java

`clients/java` is a client for Java 25 that depends only on Jackson:

```java
var fastmock = FastMock.connect("http://fastmock-admin:9090", System.getenv("FASTMOCK_TOKEN"));
fastmock.createStub(Stub.post("/api/{v}/send").bodyJsonPath("$.phone", Match.matches("380\\d{9}"))
        .respond(202).jsonBody(Map.of("id", "{{uuid}}"))
        .callback(Callback.post("http://app:8080/cb").jsonBody(Map.of("status", "DELIVERED")).correlation("{{request.json.id}}"))
        .tag("my-test"));
var callbacks = fastmock.awaitCallbacks(messageId, 1, Duration.ofSeconds(10));
var requests = fastmock.awaitRequests(RequestFilter.path("/api").withCorrelation(messageId), 1, Duration.ofSeconds(5));
fastmock.deleteStubsByTag("my-test");
```

See the [Java client guide](docs/guide/java-client.md) for every method, JUnit 5 patterns and Testcontainers.

## Kubernetes

```sh
helm upgrade --install fastmock deploy/helm/fastmock -n mocks \
  --set admin.tokensSecret.name=fastmock-admin-tokens \
  --set storage.database.existingSecret=fastmock-storage \
  --set storage.valkey.existingSecret=fastmock-storage
```

```yaml
apiVersion: fastmock.franshub.io/v1alpha1
kind: MockApi
metadata: {name: billing}
spec:
  serverRef: {name: payments}
  openapi:
    configMapRef: {name: billing-openapi, key: openapi.yaml}
    pathPrefix: /billing
  stubs:
    - request: {method: GET, urlPath: /billing/health}
      response: {status: 200, body: ok}
```

[Helm chart](docs/operations/helm.md), [Operator](docs/operations/operator.md),
[Configuration](docs/operations/configuration.md), [Troubleshooting](docs/operations/troubleshooting.md).

## Plugins

Providers whose behaviour is more than a stub can express are implemented in Rust as plugins mounted under
`/<name>`. A plugin lives in its own crate and is compiled into a custom binary that reuses the whole FastMock
command line; see [Writing plugins](docs/guide/plugins.md).

## Development

```sh
cargo test --workspace
FASTMOCK_TEST_DATABASE_URL=postgres://user@localhost:5432/postgres \
FASTMOCK_TEST_VALKEY_URL=redis://localhost:6379/ \
FASTMOCK_TEST_KAFKA=localhost:9092 FASTMOCK_TEST_SCHEMA_REGISTRY=http://localhost:8081 \
cargo test --workspace        # also runs the storage contract, replica and Kafka tests
cargo run -p fastmock-operator -- crd > deploy/helm/fastmock-operator/crds/fastmock.franshub.io.yaml
```

The web UI lives in `ui/` (React, TypeScript, Vite); `npm ci && npm run build` there before `cargo build` embeds
it into the binary.

Workspace: `fastmock-core` (model, matching, templates, engine, importers), `fastmock-store` (PostgreSQL and
Valkey), `fastmock-kafka`, `fastmock-server` (binary `fastmock`),
`fastmock-operator`.

Security issues: see [SECURITY.md](SECURITY.md).

## License

Apache License 2.0, see [LICENSE](LICENSE).
