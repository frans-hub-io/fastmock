# Recording

FastMock turns real traffic into stubs in two ways:

- **Live recording** proxies requests that no stub matches to a real backend and saves each new exchange as a
  stub, so the next identical request is answered by the mock.
- **Snapshots** (WireMock's `snapshot`) convert requests already in the request journal into stubs, with filters,
  de-duplication and optional scenarios for repeated requests.

Traffic captures from browsers and API clients (HAR files, Postman collections) are covered in
[Importing](importing.md). gRPC calls (unary and streaming) are not proxied or recorded yet; write gRPC stubs
from the [descriptor sets](grpc.md).

- [Live recording](#live-recording)
- [Snapshots](#snapshots)
- [What a generated stub looks like](#what-a-generated-stub-looks-like)
- [Repeated requests as scenarios](#repeated-requests-as-scenarios)
- [Secrets](#secrets)
- [Limits](#limits)
- [Metrics](#metrics)

The examples use `FASTMOCK_TOKEN` for an editor token, the admin API on `localhost:9090` and the data plane on
`localhost:8080`.

## Live recording

```sh
curl -s -X POST http://localhost:9090/__admin/recordings/start \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"targetBaseUrl": "https://api.example.com", "tag": "rec-payments",
       "captureHeaders": ["x-request-id"], "requestHeaders": ["accept"]}'

# exercise the system under test against http://localhost:8080 ...

curl -s -X POST -H "Authorization: Bearer $FASTMOCK_TOKEN" http://localhost:9090/__admin/recordings/stop
# {"stopped": true, "recordedStubs": 14}
curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" http://localhost:9090/__admin/recordings
# {"recording": null}
```

| Field | Meaning |
|---|---|
| `targetBaseUrl` | backend to proxy unmatched requests to; must pass the egress policy (`FASTMOCK_CALLBACK_ALLOWED_HOSTS`, `FASTMOCK_CALLBACK_DENIED_CIDRS`) |
| `tag` | added to every recorded stub, next to `recorded` |
| `captureHeaders` | response headers to keep (content-type is always kept) |
| `requestHeaders` | request headers to add as matchers |

While recording is active, a request that matches no stub is forwarded to the target; the response is returned to
the client and stored as a stub unless an identical request (method, path, query, body) was recorded already.
Requests that match an existing stub, including stubs recorded earlier, are answered by the mock. At most
`FASTMOCK_RECORDING_MAX_STUBS` (default 1000) stubs are recorded per day. Recording state is shared by all
replicas.

Delete a recording with `DELETE /__admin/stubs?tag=rec-payments`.

## Snapshots

`POST /__admin/recordings/snapshot` converts journal entries into stubs. Nothing has to be started beforehand:
any traffic the data plane served (through proxy stubs, plugins or ordinary stubs) can become a snapshot.

```sh
curl -s -X POST http://localhost:9090/__admin/recordings/snapshot \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{
        "filters": {"request": {"urlPathPattern": "/api/.*"}, "since": "2026-11-19T09:00:00Z"},
        "captureHeaders": ["accept"],
        "repeatsAsScenarios": true,
        "persist": true,
        "outputFormat": "IDS",
        "tag": "snap-checkout"
      }'
```

```json
{"persisted": true, "ids": ["0193…", "0193…"], "warnings": []}
```

| Field | Default | Meaning |
|---|---|---|
| `filters.request` | all | request pattern (stub syntax) the entries must match |
| `filters.ids` | all | only these journal ids (at most 10000) |
| `filters.since` | all | only entries at or after this instant |
| `filters.source` | all | `stubs` or a plugin name |
| `filters.matched` | all | only matched (`true`) or unmatched (`false`) entries |
| `captureHeaders` | `[]` | request headers to add as `equalTo` matchers (at most 32) |
| `repeatsAsScenarios` | `false` | see [below](#repeated-requests-as-scenarios) |
| `persist` | `true` | save the stubs; `false` only returns them |
| `outputFormat` | `FULL` | `FULL` returns the stubs, `IDS` only their ids |
| `tag` | `snapshot` | tag for the generated stubs (they also get `recorded`) |

Entries are processed oldest first. Identical requests (method, path, query and body) produce one stub; without
`repeatsAsScenarios` the first response wins and a warning reports the other responses. Use `persist: false` to
review the stubs, then save them with `POST /__admin/stubs/import`.

## What a generated stub looks like

```json
{
  "id": "0193b0c4-…",
  "name": "snapshot POST /api/orders",
  "priority": 5,
  "request": {
    "method": "POST",
    "urlPath": "/api/orders",
    "query": {"dryRun": {"equalTo": "false"}},
    "headers": {"accept": {"equalTo": "application/json"}},
    "body": [{"equalToJson": {"sku": "A-1", "qty": 2}}]
  },
  "response": {
    "status": 201,
    "headers": {"content-type": "application/json"},
    "jsonBody": {"id": "o-42", "status": "NEW"},
    "templated": false
  },
  "tags": ["snap-checkout", "recorded"]
}
```

- Path and query parameters become exact matchers; a JSON body becomes `equalToJson`, another text body
  `equalTo`.
- The response has the recorded status and body; JSON bodies become `jsonBody` with `content-type:
  application/json`. Response headers are not stored in the journal, so snapshots do not reproduce them.
- `templated` is `false`: recorded text is replayed literally, `{{...}}` in it is never executed.

## Repeated requests as scenarios

With `repeatsAsScenarios: true`, identical requests that received different responses become a scenario with
one state per recorded call, in the recorded order, as in WireMock. Four calls answered `RUNNING`, `RUNNING`,
`DONE`, `DONE` replay the same way; a trailing run of identical responses is folded into one stub because the last
state repeats anyway:

```json
[
  {"request": {"method": "GET", "urlPath": "/api/job/7"}, "response": {"jsonBody": {"state": "RUNNING"}},
   "scenario": {"name": "snap-checkout GET /api/job/7 #3", "requiredState": "Started", "newState": "call-2"}},
  {"request": {"method": "GET", "urlPath": "/api/job/7"}, "response": {"jsonBody": {"state": "RUNNING"}},
   "scenario": {"name": "snap-checkout GET /api/job/7 #3", "requiredState": "call-2", "newState": "call-3"}},
  {"request": {"method": "GET", "urlPath": "/api/job/7"}, "response": {"jsonBody": {"state": "DONE"}},
   "scenario": {"name": "snap-checkout GET /api/job/7 #3", "requiredState": "call-3"}}
]
```

The last response repeats once the scenario reaches its state. Reset with `DELETE /__admin/scenarios` (see
[Scenarios and state](scenarios-and-state.md)).

## Secrets

Generated stubs never contain secrets:

- the journal already masks sensitive headers, query parameters and JSON fields (`FASTMOCK_REDACT_HEADERS`,
  `FASTMOCK_REDACT_JSON_KEYS`) before anything is stored;
- sensitive request headers named in `captureHeaders`/`requestHeaders` become `matches: ".+"` instead of the
  value;
- sensitive JSON fields are removed from `equalToJson` matchers, which then use `ignoreExtraElements: true`; a
  text body that contains a secret is not used as a matcher at all.

## Limits

| Limit | Value |
|---|---|
| journal entries considered per snapshot | the newest 10000 passing `since`, `source` and `matched` |
| `filters.ids` | 10000 |
| `captureHeaders` | 32 |
| bodies | as stored in the journal: `FASTMOCK_JOURNAL_MAX_BODY_BYTES` (default 64 KiB); truncated or binary bodies are reported as warnings |
| live recording | `FASTMOCK_RECORDING_MAX_STUBS` per day (default 1000), upstream bodies up to 16 MiB |

## Metrics

| Metric | Meaning |
|---|---|
| `fastmock_recorded_stubs_total` | stubs saved by live recording |
| `fastmock_recording_skipped_total{reason}` | exchanges not saved (`limit`, `invalid`) |
| `fastmock_snapshot_stubs_total` | stubs generated by snapshots |
| `fastmock_proxy_errors_total` | failed upstream calls |

Snapshots are audited as `recording.snapshot`, live recordings as `recording.start` / `recording.stop`.

See also: [Admin API](admin-api.md), [Verification](verification.md), [Importing](importing.md).
