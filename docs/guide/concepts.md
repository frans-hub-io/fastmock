# Concepts

This page explains how FastMock is put together and what happens to a request. Feature pages go into detail;
this one gives the model they share.

- [Data plane and control plane](#data-plane-and-control-plane)
- [Stubs](#stubs)
- [Matching order](#matching-order)
- [Templates](#templates)
- [Scenarios and key-value state](#scenarios-and-key-value-state)
- [Request journal](#request-journal)
- [Callbacks outbox](#callbacks-outbox)
- [Kafka messages](#kafka-messages)
- [Plugins](#plugins)
- [Storage modes](#storage-modes)
- [Running several replicas](#running-several-replicas)
- [Life of a request](#life-of-a-request)

## Data plane and control plane

FastMock listens on two ports.

| | Data plane | Control plane |
|---|---|---|
| Default address | `0.0.0.0:8080` (`FASTMOCK_LISTEN`) | `0.0.0.0:9090` (`FASTMOCK_ADMIN_LISTEN`) |
| Serves | mocks: stubs, plugins, gRPC, SSE, WebSocket | `/__admin/*` API, `/ui`, OpenAPI document and Swagger UI, `/metrics`, `/healthz`, `/readyz` |
| Clients | the system under test | tests, CI pipelines, the operator, people, Prometheus |
| Authentication | none (it imitates other services) | bearer token with a role on every `/__admin` call |
| TLS | optional, with mTLS (`FASTMOCK_TLS_*`) | terminate at the Gateway or ingress |

The two can never be the same port, and no admin route exists on the data port: a system under test cannot
change its own mocks. Expose them separately (in Kubernetes the chart creates two Services) and restrict the
control plane to the clients that need it ([Security](../operations/security.md)).

## Stubs

A **stub** is a JSON document: a request pattern and what to do when a request matches it.

```json
{
  "id": "7f1c4c1e-8a0a-4a43-9d1b-0d6c1d4a9f10",
  "name": "send sms",
  "priority": 5,
  "tags": ["billing"],
  "metadata": {"owner": "team-messaging"},
  "enabled": true,
  "request": {"method": "POST", "urlPath": "/sms"},
  "response": {"status": 202, "jsonBody": {"id": "{{uuid}}"}},
  "scenario": {"name": "sms-{{request.json.to}}", "newState": "SENT"},
  "callbacks": [{"url": "http://app.example.svc/hooks/sms", "jsonBody": {"status": "DELIVERED"}}],
  "messages": [{"topic": "sms.events", "jsonValue": {"to": "{{request.json.to}}"}}]
}
```

| Part | Purpose | Reference |
|---|---|---|
| `id` | UUID; generated when absent. `PUT /__admin/stubs/{id}` creates or replaces a stub with a known id. | [Stub lifecycle](stub-lifecycle.md) |
| `name`, `tags`, `metadata` | Labels for people, cleanup (`DELETE /__admin/stubs?tag=`), import sets and metrics. | [Stub lifecycle](stub-lifecycle.md) |
| `priority` | Lower numbers are tried first; default 5. | [Request matching](request-matching.md) |
| `enabled` | Disabled stubs stay stored but never match. | [Stub lifecycle](stub-lifecycle.md) |
| `request` | Method, path (exact, template, regex), query, headers, body matchers, GraphQL. | [Request matching](request-matching.md) |
| `response` | Status, headers, body, delay, fault, proxy, SSE. | [Responses](responses.md) |
| `scenario` | State machine condition and transition. | [Scenarios and state](scenarios-and-state.md) |
| `callbacks` | Durable HTTP requests sent after the response. | [Callbacks](callbacks.md) |
| `messages` | Kafka records published when the stub matches. | [Kafka](kafka.md) |

Stub JSON is strict: unknown fields are rejected with 400, so typos do not silently change behaviour. Stubs can
be validated without saving them (`POST /__admin/stubs/validate`, `fastmock validate file.json`), imported in
bulk, generated from OpenAPI, WireMock, MockServer, Postman, HAR and other formats ([Importing](importing.md)),
or recorded from a real service ([Recording](recording.md)).

## Matching order

All enabled stubs are kept in memory, compiled (regular expressions, JSONPath, schemas) and sorted once when they
change. For each request FastMock walks the list and the first stub whose every condition holds answers:

1. **`priority`**: lower first.
2. **`order`** at equal priority: stubs that carry an `order` (set by the WireMock and MockServer importers to
   reproduce their tie-breaking) come first, lower `order` first.
3. **Specificity** of the path for stubs without `order`: an exact path before a path template, templates with fewer
   parameters before templates with more, a trailing `{rest...}` template after those, then regular
   expressions, then stubs without a path condition.
4. **Recency**: at equal priority and specificity, the most recently changed stub wins.

A stub with a scenario whose `requiredState` does not hold is skipped and the walk continues. When nothing
matches, the data plane answers `404 {"error":"no stub matched","method":"GET","path":"/x"}` and journals the
request with `matched=false`; `POST /__admin/requests/near-misses` explains which stubs came closest and why
([Verification](verification.md)).

## Templates

Response bodies, headers, callback and message fields and scenario names are
[Handlebars](https://handlebarsjs.com/) templates rendered with the request as context: `request.pathParams.id`,
`request.json.orderId`, `request.headers.x-client-id`, `state.current`, and helpers such as `uuid`, `now`,
`jsonPath`, `randomInt`, `hmac`. Templates are a closed language: they cannot read files, environment
variables, secrets or the network, and their size and output are bounded. Set `"templated": false` on a
response to send text with `{{` literally. See [Templating](templating.md).

## Scenarios and key-value state

A scenario is a named state machine. Stubs require a state (`requiredState`) and move it (`newState`), so the
same request can get different answers over time. Names are templates, so one set of stubs drives a separate
machine per order or user. States start at `Started`, expire after `ttlSeconds` (7 days by default), and change
atomically across replicas. A separate key-value store keeps strings with optional expiry. See
[Scenarios and state](scenarios-and-state.md).

## Request journal

Every data-plane request is recorded after the response is sent: method, path, query, headers, body, status,
response body, matched stub, correlation id, duration and trace id. Secrets are masked before anything is stored
([redaction](../operations/security.md#redaction)). Tests query the journal to verify calls
(`GET /__admin/requests`, `/requests/count`) and wait for them with the Java client's `awaitRequests`.

Because the journal is written asynchronously, an entry can appear a few milliseconds after the response; poll
instead of reading once. When the write queue (`FASTMOCK_JOURNAL_QUEUE`) is full, entries are dropped and counted
in `fastmock_journal_dropped_total` rather than slowing down mocks. Entries are kept for
`FASTMOCK_JOURNAL_RETENTION` (24 h). See [Verification](verification.md).

## Callbacks outbox

Callbacks are written to an **outbox** before the response is returned, then delivered by whichever replica
claims them when they are due. Delivery is **at-least-once** with exponential backoff for network errors, 408,
429 and 5xx, up to `maxAttempts`. Every attempt carries the same `x-fastmock-delivery-id` (the idempotency key
for receivers), the attempt number, a `traceparent` that continues the trace of the original request, and
optionally an HMAC signature made with a server-side secret that stubs may use only if it is listed in
`FASTMOCK_STUB_SECRETS`. Outgoing traffic passes the [egress policy](../operations/security.md#egress-policy).
See [Callbacks](callbacks.md).

## Kafka messages

Stubs can publish Kafka records (text, JSON, or Avro registered in a Schema Registry) when they match.
Publishing runs in the background and does not delay or fail the response. See [Kafka](kafka.md).

## Plugins

Some providers need more than declarative stubs: generated identifiers that later requests refer to, delivery
reports that depend on earlier messages, provider-specific signatures. Such providers are implemented as
**plugins** compiled into the binary. A plugin is mounted on the data port under `/<name>`, can add admin routes
under `/__admin/plugins/<name>/`, uses the same stores, templates, callbacks outbox, journal and metrics (its
traffic is labelled `source="<name>"` instead of `source="stubs"`), and reads settings from
`FASTMOCK_PLUGIN_<NAME>_<KEY>`. `GET /__admin/plugins` and `fastmock plugins` list them; `FASTMOCK_PLUGINS`
selects which are active. The release binary contains no plugins; [Writing plugins](plugins.md) shows how to build
one that does.

## Storage modes

| What | `memory` | `postgres` (PostgreSQL + Valkey) |
|---|---|---|
| stubs | process | PostgreSQL `fm_stubs`, change notifications via `LISTEN/NOTIFY` |
| request journal | process, latest 100,000 entries | PostgreSQL `fm_journal` |
| callbacks outbox | process | PostgreSQL `fm_outbox` |
| audit log | process | PostgreSQL `fm_audit` (kept 90 days) |
| imported documents, gRPC descriptors, plugin data | process | PostgreSQL `fm_data` |
| scenario states, key-value entries, chaos rules, recording state, plugin counters | process | Valkey, keys under `FASTMOCK_VALKEY_PREFIX` |
| survives a restart | no | yes |
| replicas | exactly one | any number |

`memory` needs nothing else and starts in milliseconds: use it on a laptop and in CI jobs that start their own
FastMock. `postgres` is the default and the mode for shared environments. Migrations run at start-up (or
separately with `fastmock migrate`). PostgreSQL 18 and Valkey 9 are the tested versions.

## Running several replicas

In `postgres` mode replicas share everything, so you can scale horizontally and roll out without losing state:

- **Stubs** change on all replicas within milliseconds (PostgreSQL notifications, with polling every
  `FASTMOCK_STUB_POLL` as a fallback).
- **Scenario transitions** use compare-and-set in Valkey: concurrent requests on different replicas move a
  scenario exactly once.
- **Callbacks** are claimed with row locks; any replica delivers, and a callback whose replica died is retried by
  another after its lease expires.
- **The journal** is one table: a test can call replica A and verify through replica B.
- **Chaos rules** are cached for up to 2 seconds per replica.
- **Shutdown** is graceful: on SIGTERM a replica withdraws readiness, waits `FASTMOCK_SHUTDOWN_DELAY`, then
  finishes open requests (up to 20 s) and in-flight callbacks.

Several independent FastMock instances can share one PostgreSQL server if each uses its own database, and one
Valkey if each uses its own `FASTMOCK_VALKEY_PREFIX`.

## Life of a request

```text
client ──► data port
            │ size and memory budget checks (413 / 503)
            │ server span, trace id (continues an incoming traceparent)
            │ chaos rules (may delay, fail or reset)
            │ plugin route?  ── yes ──► plugin handler
            │ gRPC?          ── yes ──► decode with descriptors, then match as JSON
            │ match stubs (priority, specificity, recency, scenario state)
            │   ├─ no match ─► 404, or proxy to the recording target while recording
            │   └─ match ───► scenario transition (compare-and-set)
            │                 render response, callbacks → outbox, messages → Kafka (background)
            │                 delay, then response / fault / proxy / SSE stream
            ▼
        response + x-fastmock-trace-id
            │ metrics (requests, latency, stub hits)
            └─► journal queue ─► background writer (redacted)
```
