# Stub lifecycle

A stub lives from the moment it is saved until it is deleted, but it can stop answering earlier: after a number of
matches (`times`), at a point in time (`expiresAt`, `ttlSeconds`), when it is disabled, or while its scenario is in
another state. This page covers identity and ordering, these limits, the shared counters behind them and response
sequences, and the global settings that apply to every stub.

- [Identity, labels and ordering](#identity-labels-and-ordering)
- [Limiting the number of matches: times](#limiting-the-number-of-matches-times)
- [Expiry: expiresAt and ttlSeconds](#expiry-expiresat-and-ttlseconds)
- [Enabling and disabling](#enabling-and-disabling)
- [Scenarios](#scenarios)
- [Counters](#counters)
- [Global settings](#global-settings)
- [Observability](#observability)

## Identity, labels and ordering

| Field | Meaning |
|---|---|
| `id` | UUID; generated when absent. `PUT /__admin/stubs/{id}` creates or replaces the stub with that id. |
| `name` | Free text; used in logs, near misses and the `stub` label of metrics. |
| `tags` | Labels for cleanup (`DELETE /__admin/stubs?tag=`), export and imports (`replace` deletes the stubs of a tag first). |
| `metadata` | Any JSON object; searchable with `POST /__admin/stubs/find-by-metadata`. |
| `priority` | Lower numbers are tried first; default 5. Equal priorities are ordered by `order`, then by how specific the URL condition is, then by the latest change (see [Which stub wins](request-matching.md#which-stub-wins)). |
| `order` | Optional integer tie-breaker at equal priority: stubs with an `order` come before stubs without one, lower first, and their URL specificity is not used. The importers set it so that imported stubs keep the precedence of their source tool (below). |
| `enabled` | `false` keeps the stub stored but never matches it. |
| `updatedAt` | Set by the server on every save. |

### Equal priorities in imported stubs

WireMock and MockServer break ties differently from FastMock and from each other; neither looks at how specific
the URL is. The importers write an `order` so that each set of stubs behaves as it did in its source tool:

| Source | At equal priority | `order` written by the import |
|---|---|---|
| WireMock (`/__admin/import/wiremock`, `/__admin/import` with WireMock mappings) | The newest stub wins. Like WireMock's `POST /__admin/mappings/import`, the mappings of one document are added from last to first, so the **first** mapping of the document wins, and a later import wins over an earlier one. | Negative, decreasing with each import: `-(importMillis * 10000 + (n - 1 - index))`. |
| MockServer (`/__admin/import/mockserver`, `/__admin/import` with expectations) | The expectation created first wins: the first one of the document, and an earlier import over a later one. | Positive, increasing with each import: `importMillis * 10000 + index`. |
| FastMock stubs, OpenAPI, Postman, HAR, Microcks and the other formats | Unchanged: URL specificity, then the latest change. | none |

Stubs with an `order` come before stubs without one at the same priority. Re-importing a tag gives its stubs a
new `order`, as re-adding mappings does in WireMock. Use `priority` when stubs from different sources overlap.

Example: these two WireMock mappings, imported in this order, answer `GET /spec/x` with `pattern-first`
(WireMock 3 does the same; FastMock's own ordering would pick the exact path):

```json
{"mappings": [
  {"request": {"urlPathPattern": "/spec/.*"}, "response": {"body": "pattern-first"}},
  {"request": {"urlPath": "/spec/x"}, "response": {"body": "exact-second"}}
]}
```

Every change (`POST`, `PUT`, `PATCH`, import) is visible to all replicas within `FASTMOCK_STUB_POLL` (PostgreSQL
notifications make it immediate in practice) and is written to the audit log.

## Limiting the number of matches: times

```json
{
  "name": "first two calls fail",
  "priority": 1,
  "times": 2,
  "request": {"method": "POST", "urlPath": "/payments"},
  "response": {"status": 503, "jsonBody": {"error": "try again"}}
}
```

```json
{
  "name": "then succeed",
  "priority": 5,
  "request": {"method": "POST", "urlPath": "/payments"},
  "response": {"status": 201, "jsonBody": {"id": "{{uuid}}"}}
}
```

```sh
for i in 1 2 3; do curl -s -o /dev/null -w '%{http_code}\n' -X POST localhost:8080/payments; done
# 503
# 503
# 201
```

- A stub with `times: n` answers at most `n` requests; afterwards it is skipped and the next matching stub answers
  (or the request is unmatched). This is MockServer's `Times` and the usual "fail twice, then succeed" pattern.
- The count is kept in Valkey with an atomic increment, so it holds across all replicas: with three replicas and
  `times: 2`, exactly two requests in total get this stub.
- Only requests that match the whole stub (including the scenario's `requiredState`) count. When a scenario
  transition loses a race with a concurrent request, the used slot is given back.
- The count restarts when the stub is changed in any way and on `DELETE /__admin/stubs/counters` or
  `POST /__admin/reset`. Deleting the counter key is the only way to "refill" a stub without changing it.
- `times` must be at least 1. Each request that hits a used-up stub increments
  `fastmock_stub_exhausted_total{stub}`.

## Expiry: expiresAt and ttlSeconds

```json
{"name": "maintenance window", "expiresAt": "2026-12-01T06:00:00Z", "priority": 0,
 "request": {"urlPathPattern": "/api/.*"}, "response": {"status": 503, "headers": {"retry-after": "600"}}}
```

```json
{"name": "short-lived override", "ttlSeconds": 300, "priority": 0,
 "request": {"urlPath": "/feature-flags"}, "response": {"jsonBody": {"newCheckout": true}}}
```

- `expiresAt` (RFC 3339) is an absolute time; `ttlSeconds` (1 to 31,536,000) counts from the stub's last save
  (`updatedAt`), so saving the stub again starts a new period. With both, the earlier time wins.
- An expired stub is skipped like a disabled one. It stays stored and listed by `GET /__admin/stubs`, so it can be
  inspected, re-enabled with a new `expiresAt`, or deleted (`DELETE /__admin/stubs/{id}`, or by tag).
- Expiry is checked on every request; there is no background job and no delay.

## Enabling and disabling

```sh
curl -X PATCH http://localhost:9090/__admin/stubs/$STUB_ID \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/merge-patch+json' \
  -d '{"enabled": false}'
```

Disabling is a change, so it also restarts the stub's `times` and sequence counters.

## Scenarios

A `scenario` makes a stub depend on, and move, a named state machine shared by all replicas:

```json
{"scenario": {"name": "order-{{request.json.id}}", "requiredState": "Started", "newState": "paid", "ttlSeconds": 3600}}
```

The name is a template, so each order or user can have its own state. See [Scenarios and state](scenarios-and-state.md)
for transitions, expiry, concurrency and the admin endpoints. Scenarios, `times` and response sequences combine: a
stub is considered only in its required state, counts against `times` only when it answers, and advances its
sequence on every answer.

## Counters

`times` and [response sequences](responses.md#response-sequences) use counters in Valkey under the prefix `stub:`
(`stub:times:<id>:<updatedAt>` and `stub:seq:<id>:<updatedAt>`). The key includes the time of the last save, so any
change of the stub starts from zero, and old keys expire on their own (30 days, or at the stub's expiry).

```sh
curl -s http://localhost:9090/__admin/stubs/counters -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{
  "total": 2,
  "counters": [
    {"kind": "times", "stubId": "0193f3a4-6a43-7c1b-9d0e-9c4f6b1d2e11", "count": 2},
    {"kind": "seq", "stubId": "0193f3a4-7d10-7a2e-8c55-1b2d3e4f5a6b", "count": 5}
  ]
}
```

```sh
curl -s -X DELETE http://localhost:9090/__admin/stubs/counters -H "Authorization: Bearer $FASTMOCK_TOKEN"
# {"reset": 2}
```

| Endpoint | Role | Purpose |
|---|---|---|
| `GET /__admin/stubs/counters` | viewer | List counters (at most 10,000). `count` of `times` can exceed the limit: requests that found the stub used up are counted too. |
| `DELETE /__admin/stubs/counters` | editor | Restart every `times` and sequence counter; audited as `stub.counters.reset`. |
| `POST /__admin/reset` | admin | Clears the journal, callbacks, scenarios and these counters. |

## Global settings

Server-wide settings apply to every stub response on every replica.

```sh
curl -s -X PUT http://localhost:9090/__admin/settings \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"fixedDelayMs": 200, "delay": {"lognormal": {"medianMs": 50, "sigma": 0.5}}, "proxyPassThrough": true}'
```

```json
{"fixedDelayMs": 200, "delay": {"lognormal": {"medianMs": 50, "sigma": 0.5}}, "proxyPassThrough": true}
```

| Field | Default | Meaning |
|---|---|---|
| `fixedDelayMs` | none | Delay for stubs that have no `delay` of their own (a stub delay replaces it). |
| `delay` | none | A [delay](responses.md#delays) (`fixedMs`, `minMs`/`maxMs` or `lognormal`) added to every stub response, including stubs with their own delay. |
| `proxyPassThrough` | `true` | `false` stops stubs with a `proxy` response from forwarding; they answer `404 {"error": "proxy pass-through is disabled"}`. Recording mode is not affected. |

- `GET /__admin/settings` (viewer) returns the stored settings; `PUT` (editor) replaces them as a whole, so send every
  field you want to keep. `PUT` with `{}` restores the defaults.
- Values are validated like stub delays (at most 24 hours, `sigma` 0..10, one kind per delay); invalid settings
  answer `400`. Every applied delay is still capped at `FASTMOCK_MAX_DELAY`.
- The settings are stored in PostgreSQL (DataStore namespace `settings`, key `global`). The replica that receives the
  `PUT` applies them at once; the others within 2 seconds. Changes are audited as `settings.update`.
- Unmatched requests, the admin API and plugin endpoints are not delayed.

## Observability

| Signal | Meaning |
|---|---|
| `fastmock_stub_hits_total{stub}` | Responses per stub. |
| `fastmock_stub_exhausted_total{stub}` | Requests that matched a used-up `times` stub and fell through. |
| `fastmock_body_file_missing_total` | Responses whose `bodyFileName` did not exist. |
| `fastmock_proxy_pass_through_blocked_total` | Proxy stubs blocked by `proxyPassThrough: false`. |
| `fastmock_store_errors_total{store="settings"}` | Settings could not be read; the previous settings stay in use. |
| Audit log (`GET /__admin/audit`) | `settings.update`, `stub.counters.reset`, `reset` and every stub change with the principal. |
| Request journal | Every request with `stubId`, so `times` and sequences can be verified with `/__admin/requests/count`. |
