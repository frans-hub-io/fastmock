# Observability

FastMock is meant to be watched: every request, match, callback, message and admin change shows up in metrics,
traces and logs, and the Helm chart ships alert rules and a Grafana dashboard.

- [Health and readiness](#health-and-readiness)
- [Metrics](#metrics)
- [Example queries](#example-queries)
- [Alerts](#alerts)
- [Grafana dashboard](#grafana-dashboard)
- [Tracing](#tracing)
- [Logs](#logs)

## Health and readiness

Both endpoints are on the admin port and need no token.

| Endpoint | Meaning | Response |
|---|---|---|
| `GET /healthz` | The process runs and serves HTTP. Use for liveness and startup probes. | `200 {"status":"ok"}` |
| `GET /readyz` | The instance can serve mocks: it has started, is not shutting down, and PostgreSQL and Valkey answer a ping. Use for readiness. | `200 {"status":"ready","valkey":"ok","database":"ok"}`; `503 {"status":"starting"}` while starting or after SIGTERM; `503 {"status":"degraded","valkey":"unavailable","database":"ok"}` when a store fails |

In memory mode both stores always report `ok`. The Helm chart and the operator use `/healthz` for the startup
(2 s period, up to 120 s) and liveness probes and `/readyz` for readiness (5 s period). A storage outage makes
pods unready but does not restart them.

On SIGTERM a replica answers `/readyz` with 503 at once, keeps serving for `FASTMOCK_SHUTDOWN_DELAY` (5 s) while
load balancers remove it, then stops accepting connections and gives open requests up to 20 s and running
callback deliveries up to 15 s to finish.

## Metrics

Prometheus text format on `GET /metrics` (admin port, no token; restrict it with NetworkPolicy). The chart's
ServiceMonitor scrapes it from the `<fullname>-admin` Service. Histograms whose name ends in `_seconds` use the
buckets 1 ms, 2.5 ms, 5 ms, 10 ms, 25 ms, 50 ms, 100 ms, 250 ms, 500 ms, 1 s, 2.5 s, 5 s, 10 s, 30 s, 60 s and
300 s; the two other distributions (`fastmock_journal_batch_size`, `fastmock_file_size_bytes`) are exported as
summaries. A series appears after its first event.

Label cardinality is bounded: `source` is `stubs` or a plugin name, `stub` is the stub name (or id when it has
none), `rule` a chaos rule id, `method` a gRPC method from uploaded descriptors. Give stubs stable names, and
avoid generating thousands of uniquely named stubs per run.

### Data plane

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_requests_total` | counter | `source`, `matched` (`true`/`false`), `status_class` (`1xx`..`5xx`) | Requests answered on the data port, including plugin and gRPC traffic. |
| `fastmock_request_duration_seconds` | histogram | `source` | Time from receiving the request to the response, including configured delays. |
| `fastmock_unmatched_requests_total` | counter | `source` | Requests no stub matched (404, or gRPC `UNIMPLEMENTED`). |
| `fastmock_stub_hits_total` | counter | `stub` | Matches per stub. |
| `fastmock_stub_exhausted_total` | counter | `stub` | A stub was skipped because its `times` were used up. |
| `fastmock_stubs_loaded` | gauge | | Stubs in this replica's matching cache. |
| `fastmock_requests_rejected_total` | counter | `reason` (`body_too_large`, `busy`) | Requests refused with 413 or 503 before matching. |
| `fastmock_stub_errors_total` | counter | | Stub evaluation failures (500). |
| `fastmock_invalid_response_headers_total` | counter | | Rendered response headers that were not valid and were dropped. |
| `fastmock_body_file_missing_total` | counter | | Responses whose `bodyFileName` was not found. |
| `fastmock_faults_total` | counter | `fault` | Faults returned by stubs (`ConnectionReset`, `EmptyResponse`, `MalformedJson`, `RandomDataThenClose`, `MalformedResponseChunk`). |
| `fastmock_chaos_injections_total` | counter | `rule` | Chaos rule firings. |
| `fastmock_proxy_errors_total` | counter | | Proxy stubs and recording that could not reach the upstream (502). |
| `fastmock_proxy_pass_through_blocked_total` | counter | | Proxy stubs not forwarded because `proxyPassThrough` is off. |
| `fastmock_grpc_calls_total` | counter | `method` | gRPC calls per method. |
| `fastmock_ws_connections` | gauge | | Open WebSocket sessions. |
| `fastmock_ws_sessions_total` | counter | `stub` | WebSocket sessions started. |
| `fastmock_ws_messages_total` | counter | `direction` (`in`, `out`), `result` (`received`, `sent`, `template_error`) | WebSocket messages. |
| `fastmock_ws_rejected_total` | counter | `reason` (`handshake`, `connections`, `invalid`, `upgrade`) | Refused WebSocket upgrades. |
| `fastmock_tls_handshake_failures_total` | counter | | Failed or timed-out TLS handshakes on the data port (including rejected client certificates). |
| `fastmock_accept_errors_total` | counter | | Failed `accept()` calls (for example file descriptor exhaustion). |
| `fastmock_panics_total` | counter | | Request handlers that panicked and were answered with 500. |

### Callbacks and messages

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_callbacks_scheduled_total` | counter | `source` | Callbacks written to the outbox by stubs. |
| `fastmock_callbacks_total` | counter | `source`, `result` (`sent`, `retry`, `failed`, `blocked`) | Delivery attempt outcomes. |
| `fastmock_callback_duration_seconds` | histogram | `source` | Duration of a delivery attempt. |
| `fastmock_callbacks_pending` | gauge | | Pending callbacks in the outbox (all replicas), refreshed every `FASTMOCK_STUB_POLL`. |
| `fastmock_callbacks_stale_results_total` | counter | | Attempt results ignored because the lease had expired and another attempt owned the callback. |
| `fastmock_messages_total` | counter | `result` (`sent`, `failed`, `dropped`) | Kafka messages from stubs. |
| `fastmock_async_messages_total` | counter | `mock`, `result` (`sent`, `failed`, `dropped`) | Messages published by AsyncAPI async mocks. |
| `fastmock_async_mocks_active` | gauge | | Active async mocks on the publishing replica. |
| `fastmock_async_leader` | gauge | | 1 on the replica that currently publishes async mock messages, 0 elsewhere. |

### Journal, storage, imports

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_journal_dropped_total` | counter | | Journal entries dropped because the write queue was full. |
| `fastmock_journal_write_errors_total` | counter | | Journal entries that could not be written. |
| `fastmock_journal_batch_size` | summary | | Entries per journal write. |
| `fastmock_store_errors_total` | counter | `store` (`stubs`, `outbox`, `audit`, `chaos`, `settings`, `services`, `asyncmocks`, `asyncmock_lease`, `contract_runs`) | Storage operations that failed in background work. |
| `fastmock_imports_total` | counter | `format`, `dry_run` | Stub imports (WireMock, MockServer, OpenAPI, Postman, HAR and others). |
| `fastmock_import_rejections_total` | counter | `format` | Imports rejected with `400` because the document produced no valid stub; nothing was changed. |
| `fastmock_import_detections_total` | counter | `format` | Formats detected by `POST /__admin/import`. |
| `fastmock_artifact_imports_total` | counter | `format`, `outcome` (`ok`, `error`) | Microcks artifact imports. |
| `fastmock_services_recorded_total` | counter | `type` | Services written to the service catalog. |
| `fastmock_recorded_stubs_total` | counter | | Stubs created by live recording. |
| `fastmock_recording_skipped_total` | counter | `reason` (`limit`, `invalid`) | Recorded exchanges not stored. |
| `fastmock_snapshot_stubs_total` | counter | | Stubs created from journal snapshots. |
| `fastmock_files_written_total` | counter | | Body files uploaded. |
| `fastmock_file_size_bytes` | summary | | Size of uploaded body files. |

### Admin API and contract tests

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_admin_changes_total` | counter | `action` | Audited changes (`stub.create`, `stub.import`, `scenario.set`, `reset`, ...). |
| `fastmock_admin_auth_failures_total` | counter | `reason` (`token`, `role`, `permission`, `origin`) | Rejected admin calls. |
| `fastmock_admin_errors_total` | counter | | Admin calls that failed with 500. |
| `fastmock_admin_verifications_total` | counter | `kind` (`count`, `sequence`), `result` (`pass`, `fail`) | Verification requests. |
| `fastmock_contract_tests_total` | counter | `result` (`passed`, `failed`, `error`) | Contract test runs. |
| `fastmock_contract_test_requests_total` | counter | `verdict` (`passed`, `failed`, `error`) | Checked operations in contract test runs. |
| `fastmock_contract_test_duration_seconds` | histogram | | Contract test run duration. |
| `fastmock_ui_requests_total` | counter | `kind` (`index`, `asset`, `missing`) | Web UI requests. |
| `fastmock_build_info` | gauge | `version` | Always 1; the version of the binary. |

### Operator

Exported by `fastmock-operator` on port 8081:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_operator_reconciles_total` | counter | `kind` (`MockServer`, `MockApi`), `result` (`ok`, `error`) | Reconciliations. |
| `fastmock_operator_reconcile_duration_seconds` | summary | `kind` | Reconciliation time. |

## Example queries

```promql
# requests per second by source and match result
sum by (source, matched) (rate(fastmock_requests_total[5m]))

# share of unmatched requests (what the FastMockUnmatchedRequests alert uses)
sum(rate(fastmock_unmatched_requests_total[10m])) / clamp_min(sum(rate(fastmock_requests_total[10m])), 1e-9)

# p95 latency per source, excluding nothing (configured delays are included)
histogram_quantile(0.95, sum by (le, source) (rate(fastmock_request_duration_seconds_bucket[5m])))

# stubs that are never hit in the last day (candidates for cleanup)
sum by (stub) (increase(fastmock_stub_hits_total[1d])) == 0

# callback success ratio per source
sum by (source) (rate(fastmock_callbacks_total{result="sent"}[15m]))
  / sum by (source) (rate(fastmock_callbacks_total[15m]))

# callback backlog
max(fastmock_callbacks_pending)

# Kafka publishing failures
sum by (result) (rate(fastmock_messages_total{result!="sent"}[5m]))

# replicas that disagree on the number of loaded stubs
max(fastmock_stubs_loaded) - min(fastmock_stubs_loaded)

# who changes what
sum by (action) (increase(fastmock_admin_changes_total[1h]))
```

## Alerts

`metrics.prometheusRule.enabled: true` creates these rules (namespace-scoped to the release namespace):

| Alert | Expression (simplified) | For | Severity |
|---|---|---|---|
| `FastMockDown` | `absent(up{service="<fullname>-admin"} == 1)` | 5m | critical |
| `FastMockUnmatchedRequests` | unmatched / all requests over 10 min > `metrics.prometheusRule.unmatchedRatio` (0.2) | 15m | warning |
| `FastMockCallbacksFailing` | `sum by (source) (rate(fastmock_callbacks_total{result=~"failed\|blocked"}[15m])) > 0` | 15m | warning |
| `FastMockCallbackBacklog` | `max(fastmock_callbacks_pending) > metrics.prometheusRule.callbackBacklog` (1000) | 10m | warning |
| `FastMockStorageErrors` | `rate(fastmock_store_errors_total[5m]) + rate(fastmock_journal_write_errors_total[5m]) > 0` | 10m | critical |
| `FastMockAdminAuthFailures` | `rate(fastmock_admin_auth_failures_total[5m]) > 1` | 10m | warning |

`FastMockUnmatchedRequests` usually means that a client calls endpoints nobody mocked (or a stub set was not
imported); `FastMockCallbacksFailing` that a receiver rejects callbacks or the egress policy blocks them. The
[troubleshooting guide](troubleshooting.md) has the next steps.

## Grafana dashboard

`deploy/grafana/fastmock-dashboard.json` (uid `fastmock`) has a datasource and a namespace variable and these
panels:

| Panel | Shows |
|---|---|
| Requests/s, Unmatched/s, p95 latency, Callbacks pending, Stubs loaded, Auth failures/s | Stat row |
| Requests by source and match | `fastmock_requests_total` by `source`, `matched` |
| Status classes | `fastmock_requests_total` by `status_class` |
| Latency by source (p50, p95, p99) | `fastmock_request_duration_seconds` |
| Top stubs | `topk(10, ...)` of `fastmock_stub_hits_total` |
| Callbacks by result | `fastmock_callbacks_total` by `source`, `result` |
| Callback delivery time p95 | `fastmock_callback_duration_seconds` |
| Messages, gRPC, proxy, recording | `fastmock_messages_total`, `fastmock_grpc_calls_total`, `fastmock_proxy_errors_total`, `fastmock_recorded_stubs_total` |
| Chaos and faults | `fastmock_chaos_injections_total` by `rule`, `fastmock_faults_total` by `fault` |
| Errors | store, journal, stub evaluation and TLS handshake errors |
| Admin changes and auth failures | `fastmock_admin_changes_total` by `action`, `fastmock_admin_auth_failures_total` by `reason` |
| Operator reconciles | `fastmock_operator_reconciles_total` by `kind`, `result` |

Import it in Grafana (Dashboards, New, Import) or provision it, for example with the Grafana sidecar:

```sh
kubectl -n monitoring create configmap fastmock-dashboard --from-file=deploy/grafana/fastmock-dashboard.json
kubectl -n monitoring label configmap fastmock-dashboard grafana_dashboard=1
```

## Tracing

FastMock creates OpenTelemetry spans and exports them, together with its logs, over OTLP/HTTP when
`OTEL_EXPORTER_OTLP_ENDPOINT` (or a per-signal endpoint) is set; nothing is exported otherwise. `OTEL_SERVICE_NAME`
(default `fastmock`), `OTEL_RESOURCE_ATTRIBUTES` and `OTEL_EXPORTER_OTLP_HEADERS` work as in other OpenTelemetry
SDKs. In Helm: `otel.endpoint`, `otel.serviceName`, `otel.resourceAttributes`; in the operator:
`spec.otelEndpoint`.

| Span | Kind | Attributes |
|---|---|---|
| `fastmock.request` (named `<METHOD> <source>`, for example `POST stubs`) | server | `http.request.method`, `url.path`, `fastmock.source`, `http.response.status_code`, `fastmock.stub_id`, `trace_id` |
| `fastmock.callback` (named `<source> callback`) | client | `http.request.method`, `url.full` (without query), `fastmock.source`, `fastmock.callback_id`, `fastmock.attempt`, `http.response.status_code` |

Propagation:

1. An incoming W3C `traceparent` header is continued: the request span becomes its child, and the response header
   `x-fastmock-trace-id` carries the caller's trace id. Without a `traceparent`, FastMock starts a new trace.
2. The trace id is stored with the journal entry (`traceId`) and in every log line of the request (`trace_id`).
3. When a stub schedules a callback, the request's trace context is stored with it. The delivery span, minutes
   later and possibly on another replica, is a child of the request span, and the callback carries a
   `traceparent` of that trace to the receiver. A receiver that continues the trace therefore appears in the same
   trace as the original call:

```text
client  POST /payments ───────────────────────────────────────────────── trace 0af76519...
  └─ fastmock.request  POST stubs                       (replica A)
       └─ fastmock.callback  stubs callback  attempt 1  (replica B, 500 ms later)
            └─ receiver  POST /hooks/payments           (traceparent from FastMock)
```

Even without an OTLP endpoint, trace ids are propagated: `x-fastmock-trace-id`, the journal and the logs use the
caller's trace id, and callbacks carry a `traceparent` in the same trace. For example, a request sent with
`traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01` is answered with
`x-fastmock-trace-id: 0af7651916cd43dd8448eb211c80319c`.

## Logs

Logs go to stdout, one JSON object per line by default (`FASTMOCK_LOG_FORMAT=text` for people). Levels come from
`RUST_LOG` (default `info`, with noisy libraries at `warn`).

| Field | Meaning |
|---|---|
| `timestamp`, `level`, `message`, `target` | Standard fields; `target` is the module (`fastmock_server::outbox`) or `audit`. |
| `span` | The current span with its attributes: for request-related lines `trace_id`, `url.path`, `fastmock.stub_id`, `http.response.status_code`; for callbacks `fastmock.callback_id`, `fastmock.attempt`, `url.full`. |
| event fields | For example `callback_id`, `attempt`, `error`, `retry`, `topic`, `partition`, `offset`. |

Examples (shortened):

```json
{"timestamp":"2026-11-19T05:48:43.474008Z","level":"WARN","message":"callback attempt failed","callback_id":"01a1f259-a1b1-76f3-a75b-3bb920a85dad","attempt":2,"error":"unexpected status 500","retry":true,"target":"fastmock_server::outbox","span":{"fastmock.attempt":2,"fastmock.source":"stubs","otel.kind":"client","url.full":"http://shop.example.com/hooks/payments","name":"fastmock.callback"}}
{"timestamp":"2026-11-19T05:52:09.411191Z","level":"INFO","message":"message published","topic":"orders.events","partition":0,"offset":0,"target":"fastmock_server::extras","span":{"fastmock.stub_id":"20e363c3-ab06-4c2e-b1d5-5d39a3739653","trace_id":"74cf475f875a2a5ac559937ca032c427","url.path":"/orders/A-1/ship","name":"fastmock.request"}}
{"timestamp":"2026-11-19T05:49:27.026678Z","level":"INFO","message":"admin change","principal":"me","role":"Admin","action":"reset","detail":"{\"requests\":19,\"callbacks\":7,\"scenarios\":0}","target":"audit"}
```

Useful messages:

| Message | Level | Meaning |
|---|---|---|
| `FastMock started` | info | Listen addresses, plugins, whether OTLP export is on. |
| `stubs reloaded` | info | The stub cache changed (with the new count). |
| `stub evaluation failed` | error | A request got 500; see `error` and the trace id. |
| `callback delivered`, `callback attempt failed` | info, warn | Callback outcomes. |
| `delivery result ignored: the lease expired ...` | warn | A late attempt result (at-least-once). |
| `message published`, `message publishing failed` | info, error | Kafka. |
| `journal write failed` | error | Journal entries lost. |
| `readiness check failed` | warn | A store does not answer. |
| `admin change` (target `audit`) | info | Every change through the admin API. |
| `admin request rejected: ...` (target `audit`) | warn | Rejected admin calls. |
| `shutdown requested; readiness withdrawn, draining` | info | SIGTERM received. |

Route `target="audit"` lines to long-term storage; they complement the audit table
([Security: audit log](security.md#audit-log)).
