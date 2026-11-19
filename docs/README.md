# FastMock documentation

FastMock is an API mock server for test environments: HTTP, HTTPS with mTLS, gRPC, GraphQL, SOAP, WebSocket,
Server-Sent Events, Kafka and AsyncAPI, with durable signed callbacks, shared state for any number of replicas,
security by default and full observability. Start with [Getting started](guide/getting-started.md), then pick the
section for what you do.

## Getting started

| Page | What you learn |
|---|---|
| [Getting started](guide/getting-started.md) | Run FastMock with Docker or Docker Compose, create a token, first stub, journal, UI, callback and scenario in ten minutes. |
| [Concepts](guide/concepts.md) | Data plane and control plane, stubs, matching order, templates, state, journal, callbacks outbox, plugins, storage modes, replicas. |
| [Web UI](guide/ui.md) | The admin UI at `/ui` and the Swagger UI for the admin API. |

## Writing mocks

| Page | What you learn |
|---|---|
| [Request matching](guide/request-matching.md) | Every request condition: method, URL and path templates, query, headers, cookies, body matchers (JSON, XML, schema, multipart, form), logical operators, priorities, limits. |
| [Responses](guide/responses.md) | Status, headers, bodies and body files, delays, dribbling, faults, response sequences, proxying. |
| [Templating](guide/templating.md) | Handlebars context and every helper. |
| [Stub lifecycle](guide/stub-lifecycle.md) | Ids, tags, metadata, enabling, `times`, expiry, counters, export. |
| [Scenarios and state](guide/scenarios-and-state.md) | State machines, templated scenario names, TTLs, the key-value store. |
| [Callbacks](guide/callbacks.md) | Webhooks after a response: delays, retries, HMAC signatures, correlation, egress policy. |
| [Proxying and chaos rules](guide/proxy-and-chaos.md) | Forward to real services; inject delays, errors and faults at random. |
| [Server-Sent Events](guide/sse.md) | Event streams with per-event delays. |
| [WebSocket](guide/websocket.md) | Scripted WebSocket sessions. |
| [gRPC](guide/grpc.md) | Unary gRPC from uploaded descriptor sets. |
| [GraphQL](guide/graphql.md) | Matching GraphQL operations, schema import. |
| [SOAP](guide/soap.md) | SOAP 1.1/1.2 stubs, WSDL and SoapUI import. |
| [Kafka messages](guide/kafka.md) | Publishing JSON or Avro records from stubs, Schema Registry, topic allow-list. |
| [AsyncAPI and async mocks](guide/asyncapi.md) | Mocks that keep publishing messages from AsyncAPI documents. |

## Importing and migrating

| Page | What you learn |
|---|---|
| [Importing](guide/importing.md) | OpenAPI, Postman, HAR and auto-detection; common import options. |
| [WireMock and MockServer import](guide/import-wiremock-mockserver.md) | Field-by-field conversion of WireMock mappings and MockServer expectations. |
| [Microcks artifacts](guide/microcks-artifacts.md) | OpenAPI with Microcks examples, APIExamples/APIMetadata, SoapUI, Postman, GraphQL, gRPC artifacts. |
| [Service catalog](guide/service-catalog.md) | The catalog of imported services and their operations. |
| [Recording](guide/recording.md) | Recording real traffic and snapshots of the journal into stubs. |
| [Migrating from WireMock](guide/migrating-from-wiremock.md) | Import, behaviour differences, test code changes. |
| [Migrating from MockServer](guide/migrating-from-mockserver.md) | Import, behaviour differences, test code changes. |
| [Migrating from Microcks](guide/migrating-from-microcks.md) | Moving a Microcks installation's mocks to FastMock. |

## Testing with FastMock

| Page | What you learn |
|---|---|
| [Verification](guide/verification.md) | Query the journal, verify counts and sequences, near misses. |
| [Java client](guide/java-client.md) | The Java 25 client: every method, JUnit 5 patterns, Testcontainers. |
| [Contract testing](guide/contract-testing.md) | Testing a real service against its OpenAPI or AsyncAPI contract. |
| [Admin API](guide/admin-api.md) | Authentication, roles per endpoint, errors, pagination, every endpoint group. |
| [Admin API reference](api/swagger/index.html) | Interactive Swagger UI for the admin API. |
| [OpenAPI description](api/openapi.json) | Machine-readable description of the admin API (also served at `/openapi.json`). |

## Operating FastMock

| Page | What you learn |
|---|---|
| [Configuration](operations/configuration.md) | Every environment variable with type, default and effect. |
| [Helm chart](operations/helm.md) | Install, upgrade, every value, production checklist, Gateway API, NetworkPolicy. |
| [Operator](operations/operator.md) | `MockServer` and `MockApi` resources, status conditions, policies, RBAC, CRD upgrades. |
| [Security](operations/security.md) | Tokens and roles, origins, TLS, egress, secrets, redaction, limits, audit, hardening checklist. |
| [Observability](operations/observability.md) | Every metric, PromQL examples, alerts, Grafana dashboard, tracing, logs, health checks. |
| [Troubleshooting](operations/troubleshooting.md) | Symptom, cause and fix for the common problems. |
| [Docker Compose example](examples/docker-compose.yaml) | FastMock with PostgreSQL 18 and Valkey 9. |

## Reference

| Page | What you learn |
|---|---|
| [Writing plugins](guide/plugins.md) | Implement a provider in Rust and build a binary that includes it. |
| [Security policy](https://github.com/frans-hub-io/fastmock/blob/main/SECURITY.md) | How to report a vulnerability. |

## Conventions in these pages

- The data plane is `http://localhost:8080` and the admin API `http://localhost:9090`; in Kubernetes they are the
  `<release>` and `<release>-admin` Services.
- `FASTMOCK_TOKEN` holds an admin token (`fastmock hash-token --generate`); calls that change something need the
  `editor` role, a few need `admin`.
- Example host names use `example.com`, example addresses `198.51.100.0/24` and `203.0.113.0/24`.
- Outputs shown on these pages come from real runs of FastMock 0.1.0, shortened where marked.
