# Verification

Tests usually need to assert what the system under test sent: "the order service called `POST /payments`
exactly once", "nobody called `DELETE /users`", "login happened before checkout". FastMock answers these questions
from its request journal, with the same request patterns stubs use, so anything a stub can match on (method,
path, query, headers, JSON/XML body matchers, JSONPath, JSON Schema, GraphQL) can be verified.

- [The request journal](#the-request-journal)
- [Find requests](#find-requests)
- [Verify a count](#verify-a-count)
- [Verify an order (sequence)](#verify-an-order-sequence)
- [Single requests, unmatched requests, near misses](#single-requests-unmatched-requests-near-misses)
- [Remove requests](#remove-requests)
- [Coming from WireMock or MockServer](#coming-from-wiremock-or-mockserver)
- [Limits and caveats](#limits-and-caveats)
- [Metrics](#metrics)

The examples use `FASTMOCK_TOKEN` (viewer is enough for everything except removing requests), the admin API on
`localhost:9090` and the data plane on `localhost:8080`.

## The request journal

Every request the data plane receives is stored with its method, path, query, headers, body, the response status
and body, the matched stub id, a correlation id and the trace id. Before it is stored, sensitive headers, query
parameters and JSON fields are masked (`***`) and bodies are cut at `FASTMOCK_JOURNAL_MAX_BODY_BYTES` (default
64 KiB). Entries are kept for `FASTMOCK_JOURNAL_RETENTION` (default 24h). Writes are batched in the background, so
an entry can appear a few milliseconds after the response; poll `/verify` or `/requests/count` instead of asserting
right after the call when timing matters.

Verification works on that stored, redacted copy: matchers on a masked value see `***`, and a body matcher
cannot match content beyond the stored size.

## Find requests

`POST /__admin/requests/find` takes a request pattern (the `request` part of a stub) and returns the matching
journal entries, newest first:

```sh
curl -s -X POST http://localhost:9090/__admin/requests/find \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"method": "POST", "urlPath": "/orders",
       "body": [{"jsonPath": "$.amount", "value": {"matches": "[0-9]+"}}]}'
```

```json
{"total": 2, "requests": [
  {"id": 87, "at": "2026-11-19T10:00:01.311Z", "source": "stubs", "method": "POST", "path": "/orders",
   "query": "", "headers": {"content-type": "application/json", "authorization": "Bearer ***"},
   "body": "{\"amount\":7}", "status": 201, "responseBody": "{\"id\":\"o-7\"}",
   "stubId": "4f0c…", "matched": true, "durationMs": 2}
]}
```

Query parameters: `source` (`stubs` or a plugin name), `since` (RFC 3339), `limit` (entries returned, default 100;
`total` counts all matches).

## Verify a count

`POST /__admin/verify` counts the journal entries matching `request` and compares the count with an expectation:

| Field | Expectation |
|---|---|
| `count` or `exactly` (only one of them) | exactly n |
| `atLeast` | at least n |
| `atMost` | at most n (`atMost: 0` = never received) |
| `atLeast` + `atMost` | between, inclusive |
| none | at least 1 |
| `source`, `since` | restrict the journal entries considered |

A body that is not valid JSON, has an unknown field (for example `atleast`) or a field of the wrong type, or
combines `count`/`exactly` with `atLeast`/`atMost`, is answered `400 {"error": ...}`. `422` always comes with a
result: an expectation that does not hold (a `verified: false` body), or the scan budget described below. A CI
script can therefore treat `400` as a broken check and `422` as a failed one.

The answer is `200` when the expectation holds:

```sh
curl -s -X POST http://localhost:9090/__admin/verify \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"request": {"method": "POST", "urlPath": "/payments"}, "exactly": 1}'
```

```json
{"verified": true, "count": 1, "expected": "exactly 1", "details": [], "nearMisses": []}
```

and `422` when it does not, with the actual count and the closest non-matching requests (at most five, best
first), each with the matchers that failed:

```json
{
  "verified": false,
  "count": 0,
  "expected": "at least 1",
  "error": "expected at least 1 matching requests, received 0",
  "details": ["request 88 POST /payments: body matcher #0 does not match"],
  "nearMisses": [
    {"requestId": 88, "at": "2026-11-19T10:00:02.004Z", "method": "POST", "path": "/payments",
     "score": 0.75, "mismatches": ["body matcher #0 does not match"]}
  ]
}
```

`score` is the share of the pattern's matchers (method, path, each query, header and body matcher) that matched.
Combining `count` with `atLeast`/`atMost`, or `atLeast` greater than `atMost`, is a `400`.

A test helper only has to check the status code:

```sh
curl -sf -X POST http://localhost:9090/__admin/verify \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"request": {"method": "DELETE", "urlPathPattern": "/users/.*"}, "atMost": 0}' >/dev/null \
  || echo "unexpected DELETE /users/*"
```

## Verify an order (sequence)

`POST /__admin/verify/sequence` checks that requests matching the patterns arrived in the given order. Other
requests may come in between; each pattern must be matched by a request received after the one that matched the
previous pattern.

```sh
curl -s -X POST http://localhost:9090/__admin/verify/sequence \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"requests": [
        {"method": "POST", "urlPath": "/login"},
        {"method": "GET",  "urlPath": "/cart"},
        {"method": "POST", "urlPath": "/checkout"}
      ]}'
```

```json
{"verified": true, "matched": 3, "requestIds": [101, 104, 109], "details": []}
```

On failure the answer is `422` and says where the sequence broke:

```json
{"verified": false, "matched": 1, "requestIds": [101],
 "error": "sequence broken at requests[1]: 1 of 3 patterns matched in order",
 "details": ["no request matching requests[1] (GET /cart) after request 101"]}
```

`requests` holds 1..50 patterns; `source` and `since` restrict the entries as for `/verify`.

## Single requests, unmatched requests, near misses

```sh
# one entry by id
curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" http://localhost:9090/__admin/requests/87

# requests no stub matched (newest first, paginated)
curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" "http://localhost:9090/__admin/requests/unmatched?limit=20"

# why does a request not match any stub?
curl -s -X POST http://localhost:9090/__admin/requests/near-misses \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"method": "GET", "path": "/users/7", "headers": {"accept": "application/json"}}'
```

`GET /requests` lists the journal with filters and pagination, `GET|POST /requests/count` counts with the same
filters (see [Admin API](admin-api.md#pagination-and-filtering)).

## Remove requests

`POST /__admin/requests/remove` (editor) deletes the entries matching a pattern, for example noise from health
checks, and answers with the deleted ids. `DELETE /__admin/requests` clears the whole journal.

```sh
curl -s -X POST http://localhost:9090/__admin/requests/remove \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"urlPathPattern": "/health.*"}'
# {"deleted": 42, "ids": [3, 5, ...]}
```

Removals are recorded in the audit log as `journal.remove`.

## Coming from WireMock or MockServer

| WireMock | MockServer | FastMock |
|---|---|---|
| `POST /__admin/requests/find` | `PUT /mockserver/retrieve?type=REQUESTS` | `POST /__admin/requests/find` |
| `POST /__admin/requests/count` | | `POST /__admin/verify` (answers the count either way) |
| `verify(exactly(1), postRequestedFor(...))` | `PUT /mockserver/verify` `{"times": {"atLeast": 1, "atMost": 1}}` | `POST /__admin/verify` `{"exactly": 1}` / `{"atLeast": 1, "atMost": 1}` |
| | `PUT /mockserver/verifySequence` | `POST /__admin/verify/sequence` |
| `POST /__admin/requests/remove` | `PUT /mockserver/clear?type=LOG` | `POST /__admin/requests/remove` |
| `GET /__admin/requests/unmatched` | | `GET /__admin/requests/unmatched` |
| `GET /__admin/requests/{id}` | | `GET /__admin/requests/{id}` |

Request patterns use FastMock's stub syntax (`urlPath`, `urlPathPattern`, `urlPathTemplate`, `query`, `headers`,
`body` matchers). Converting a WireMock or MockServer pattern is mechanical; the importers in
[Importing](importing.md) show the mapping.

## Limits and caveats

| Limit | Value |
|---|---|
| journal entries scanned per call | the newest 10000 that pass `source`, `since` and the pattern's method; narrow with `since` on busy servers |
| patterns per sequence | 50 |
| near misses returned | 5 |
| matchers per pattern | 64 (same as stubs) |
| time per scan | `FASTMOCK_JOURNAL_SCAN_TIMEOUT`, default 10 s |
| work per scan | `FASTMOCK_JOURNAL_SCAN_MAX_WORK`, default 4 GiB of entry bytes × pattern weight |
| scans running at once per replica | `FASTMOCK_JOURNAL_SCAN_CONCURRENCY`, default 2 |
| request body of `/requests/near-misses` | 1 MiB (body, query and headers) |

The scan window is the reason to pass `since` (for example the test's start time): it keeps verification exact on
servers with long journals and makes it independent of other tests' traffic.

`/requests/find`, `/requests/remove`, `/verify`, `/verify/sequence`, `/requests/count` and
`/requests/near-misses` run on a separate thread pool under these limits. The work of a scan is the sum over the
scanned entries of the entry size (body, path, query and headers, plus 64 bytes) times the pattern weight (number
of matchers plus one per KiB of pattern JSON). When a client disconnects, its scan stops at the next entry.

| Status | Body | When |
|---|---|---|
| `503` | `{"error": "too many journal scans are running; retry in a moment", "details": []}` | No scan slot became free within 5 seconds. Retry with backoff. |
| `503` | `{"error": "the scan exceeded 10000 ms (FASTMOCK_JOURNAL_SCAN_TIMEOUT); narrow it with since, source, method or simpler matchers", "details": []}` | The deadline passed. |
| `422` | `{"error": "the scan exceeds the journal scan work budget (FASTMOCK_JOURNAL_SCAN_MAX_WORK); narrow it with since, source, method or fewer matchers", "details": []}` | The work budget ran out. On `/verify` and `/verify/sequence` this body has `error` but no `verified` field, unlike a failed expectation. |

## Metrics

| Metric | Labels |
|---|---|
| `fastmock_admin_verifications_total` | `kind` = `count` \| `sequence`, `result` = `pass` \| `fail` |
| `fastmock_journal_scans_total` | `kind` = `find` \| `remove` \| `verify` \| `sequence` \| `count` \| `near_misses`, `result` = `ok` \| `busy` \| `deadline` \| `budget` \| `cancelled` |
| `fastmock_journal_scan_duration_seconds` (histogram) | `kind` |

See also: [Admin API](admin-api.md), [Recording](recording.md).
