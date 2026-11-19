# Proxying and chaos rules

Two ways to make a mock environment behave less ideally than a set of perfect stubs: **proxy stubs** pass some
traffic to a real service, and **chaos rules** inject delays, errors and broken connections at random into any
traffic of the data plane, without touching the stubs.

- [Proxy stubs](#proxy-stubs)
- [Chaos rules](#chaos-rules)
- [Recipes](#recipes)

## Proxy stubs

A stub whose response has `proxy` forwards the matched request to another service and returns its answer. The
fields (`baseUrl`, `stripPrefix`, `additionalHeaders`, `removeHeaders`) and the global `proxyPassThrough` switch
are described in [Responses: proxying](responses.md#proxying). Operational behaviour:

| Aspect | Behaviour |
|---|---|
| Target URL | `baseUrl` + request path (minus `stripPrefix` when the path starts with it at a segment boundary) + original query string. |
| Method and body | Forwarded unchanged. |
| Request headers | Forwarded, except the names in `removeHeaders` and the headers FastMock never sends from user input: `host`, `content-length`, hop-by-hop headers (`connection`, `keep-alive`, `te`, `trailer`, `transfer-encoding`, `upgrade`, every `proxy-*`) and routing headers (`forwarded`, `via`, `x-real-ip`, every `x-forwarded-*` and `x-envoy-*`, `x-original-url`, `x-original-host`, `x-rewrite-url`, `x-host`, `x-http-host-override`); `additionalHeaders` are added. A stub whose `additionalHeaders` names one of these headers is rejected with `400` (`proxy.additionalHeaders: header Host cannot be set on outbound requests ...`); the WireMock and MockServer importers drop such headers with a warning. |
| Response | Status, headers (without hop-by-hop headers) and body of the upstream, body up to 16 MiB. |
| Timeouts | 30 s per request, connect timeout at most 5 s. |
| Egress | The [egress policy](../operations/security.md#egress-policy): allowed hosts, denied address ranges, no redirects. |
| Failure | `502 {"error":"proxy failed","detail":"upstream unavailable"}` (or `blocked by egress policy`, `upstream body too large`), `fastmock_proxy_errors_total`, warning log `proxying failed`. |
| Journal | The request, the upstream's status and body (redacted) and the proxy stub's id. |

Typical uses:

- **Partial mocking:** a low-priority catch-all proxy (`"priority": 100`, `urlPathPattern: "/.*"`) sends
  everything that no specific stub answers to the real service, while specific stubs override single endpoints.
- **Recording:** [recording](recording.md) uses the same forwarding to create stubs from real traffic.
- **Safety switch:** set `proxyPassThrough: false` to make every proxy stub answer 404 instead of reaching a real
  service, for example in a CI run that must be hermetic.

## Chaos rules

Chaos rules apply to requests on the data plane before stub matching. Each rule has a probability; when it fires
it delays the request, answers with an error status, or breaks the response. They are global for the instance,
shared by all replicas (stored in Valkey, cached up to 2 seconds per replica) and changed at run time through
the admin API.

```sh
curl -s -X PUT localhost:9090/__admin/chaos \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "rules": [
    {"id": "slow-payments", "pathPrefix": "/payments", "probability": 0.3, "delay": {"minMs": 500, "maxMs": 3000}},
    {"id": "flaky-sms", "pathPrefix": "/sms", "method": "POST", "probability": 0.1,
     "status": 503, "body": "{\"error\":\"temporarily unavailable\"}",
     "expiresAt": "2026-12-01T00:00:00Z"},
    {"id": "resets", "source": "stubs", "probability": 0.01, "fault": "CONNECTION_RESET"}
  ]
}'
```

```json
{"rules": 3}
```

### Rule fields

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | 1 to 64 characters; used in metrics and the response header `x-fastmock-chaos`. |
| `probability` | yes | 0 to 1; evaluated independently for each request. |
| `source` | no | Only requests to this source: `stubs` for stubs, or a plugin name (its traffic under `/<plugin>`). |
| `pathPrefix` | no | Only paths that start with this prefix. |
| `method` | no | Only this HTTP method (case-insensitive). |
| `delay` | one of `delay`, `status`, `fault` | `{"fixedMs": n}`, `{"minMs": a, "maxMs": b}` or a log-normal distribution as in [Responses: delays](responses.md#delays); capped at `FASTMOCK_MAX_DELAY`. |
| `status` | | Answer with this status (100 to 599) instead of calling the stub. |
| `body` | | Body for `status` (text, sent as is). |
| `fault` | | `CONNECTION_RESET`, `EMPTY_RESPONSE`, `MALFORMED_JSON`, `RANDOM_DATA_THEN_CLOSE` or `MALFORMED_RESPONSE_CHUNK` (see [Responses: faults](responses.md#faults)). |
| `expiresAt` | no | RFC 3339 time after which the rule is ignored. |

How a rule acts when it fires:

- only `delay`: the request waits, then is handled normally (stub, plugin, proxy);
- `status` (with optional `delay` and `body`): the request is answered with that status; no stub is evaluated
  (so no scenario transition, callback or message happens);
- `fault` (with optional `delay`): the response is broken as described; `status` is not used with a fault.

Rules are checked in order, and the first rule that applies and fires wins. A response affected by a rule carries
the header `x-fastmock-chaos: <id>`, and the journal records the outcome as the client saw it.

### Managing rules

| Call | Role | Effect |
|---|---|---|
| `GET /__admin/chaos` | viewer | `{"rules": [...]}` |
| `PUT /__admin/chaos` | editor | Replaces all rules (at most 100, validated); returns `{"rules": n}`. Invalid rules are refused with 400, for example `chaos rule flaky: probability must be within 0..1`. |
| `DELETE /__admin/chaos` | editor | Removes all rules; returns 204. |

Changes are audited (`chaos.set`, `chaos.clear`).

### Observability

| Signal | Content |
|---|---|
| `fastmock_chaos_injections_total{rule}` | Rule firings per rule id. |
| `fastmock_requests_total{status_class}` | Includes the injected statuses. |
| Response header `x-fastmock-chaos` | Id of the rule that fired. |
| Grafana dashboard | Panel "Chaos and faults". |

## Recipes

**Test client timeouts:** a delay longer than the client's timeout on one endpoint.

```json
{"rules": [{"id": "timeout-orders", "pathPrefix": "/orders", "probability": 1, "delay": {"fixedMs": 35000}}]}
```

**Test retries with backoff:** fail half of the calls with 503; the client must eventually succeed.

```json
{"rules": [{"id": "retry-503", "pathPrefix": "/payments", "method": "POST", "probability": 0.5, "status": 503}]}
```

**Game day with automatic end:** set `expiresAt` an hour ahead so forgotten rules switch themselves off.

**Deterministic failures** belong in stubs, not chaos rules: use [scenarios](scenarios-and-state.md) for "fail
twice, then succeed", response sequences ([Responses](responses.md#response-sequences)) for fixed patterns, and
stub faults for a specific request.
