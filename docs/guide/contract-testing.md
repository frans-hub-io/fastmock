# Contract testing

FastMock can check a **real** implementation against the same contract it mocks: it reads an OpenAPI 3.x or AsyncAPI
2.x/3.0 document, calls every operation of a running service (or consumes the messages it publishes to Kafka), and
validates what comes back against the document. This is the "tests" feature of Microcks. Runs execute in the
background on the replica that received the request and are stored, so a CI pipeline starts a run, polls it and fails
when the service breaks its contract.

- [Starting a run](#starting-a-run)
- [What an OpenAPI run does](#what-an-openapi-run-does)
- [What an AsyncAPI run does](#what-an-asyncapi-run-does)
- [Reading results](#reading-results)
- [Using a stored specification](#using-a-stored-specification)
- [Secrets and redaction](#secrets-and-redaction)
- [CI usage](#ci-usage)
- [Metrics, traces and logs](#metrics-traces-and-logs)
- [Limits and retention](#limits-and-retention)
- [Errors](#errors)

The examples use `FASTMOCK_TOKEN` for an editor or admin token and the admin API on `localhost:9090`. Listing runs
and reading their verdicts needs `viewer`; request and response details need `editor`; starting and deleting runs
needs `editor`, and starting a run with `secretHeaders` needs `admin`.

## Starting a run

```sh
curl -X POST localhost:9090/__admin/contract-tests \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile spec petstore.yaml '{
        spec: $spec,
        targetUrl: "https://petstore.example.com/api/v3",
        operations: ["getPetById", "POST /pet"],
        headers: {"x-test-run": "ci-1234"},
        secretHeaders: {"Authorization": "petstore-token"},
        timeoutMs: 5000}')"
```

```json
{"id": "0193a5d0-1c2e-7b44-8a10-5d3e9c7f0a11", "status": "running", "kind": "openapi", "operations": 2}
```

The answer is `202 Accepted`; the run continues in the background.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `spec` | string or object | – | The document, as a JSON object or a JSON or YAML string. Set exactly one of `spec` and `specRef`. |
| `specRef` | string | – | Id of a stored specification, see [below](#using-a-stored-specification). |
| `targetUrl` | URL | required for OpenAPI | Base URL of the service under test. Operation paths are appended to its path: `https://h/api/v3` + `/pet/{petId}`. |
| `operations` | array of strings | all | Only these operations: OpenAPI `operationId` or `"METHOD /path"` (case-insensitive method); AsyncAPI operation id, mock name or topic. At most 200. |
| `headers` | map | none | Headers added to every request (they replace generated header parameters of the same name). Routing and framing headers are refused, see [below](#headers-that-are-never-sent). |
| `secretHeaders` | map header -> secret name | none | Headers whose values are server-side secrets bound to the target host; needs the `admin` role, see [Secrets](#secrets-and-redaction). |
| `timeoutMs` | integer | `5000` | Per request, for connecting, sending and reading the response, 1..60000. |
| `messageCount` | integer | `1` | AsyncAPI: messages to consume per operation, 1..100. |
| `waitMs` | integer | `10000` | AsyncAPI: how long to wait for those messages, 1..120000. |
| `fromBeginning` | boolean | `false` | AsyncAPI: read from the earliest retained offset instead of only new messages. |

The document is parsed and the requests are planned before the answer, so a broken document, an empty operation
selection or a forbidden target fails at once with `400` instead of producing a failed run.

`targetUrl` must be the service under test, not FastMock itself: a target whose host resolves to a loopback address,
to an address of the machine FastMock runs on, or to the address FastMock listens on, combined with FastMock's data
or admin port, is refused with `400`. The addresses are resolved once when the run starts and the run connects only
to them, so DNS changes during the run cannot redirect it.

### Headers that are never sent

FastMock never sets these headers from user input on contract-test requests, proxied requests or callbacks, because
they select the virtual host or upstream behind a shared ingress, change message framing, or are hop-by-hop:
`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Keep-Alive`, `TE`, `Trailer`, `Upgrade`,
`HTTP2-Settings`, `Forwarded`, `Via`, `X-Real-IP`, `X-Original-URL`, `X-Original-Host`, `X-Rewrite-URL`, `X-Host`,
`X-HTTP-Host-Override`, and every header starting with `Proxy-`, `X-Forwarded-` or `X-Envoy-`. In `headers` and
`secretHeaders` they answer `400`; header parameters of the document with these names are skipped.

## What an OpenAPI run does

For every selected operation FastMock sends one request per **example**, and one request when the operation has no
named examples:

- Example names are the keys of the request body `examples` (JSON media type) and of the parameters' `examples`; at
  most 10 per operation. Each request uses the values of the example with that name where it exists.
- Path parameters: the named example, the parameter `example`, its first `examples` value, an example generated from
  its schema, or `1`. Values are percent-encoded.
- Query parameters: required ones, and optional ones that have an example. Array values become repeated parameters.
- Header parameters: required ones.
- Body: the named example, `example`, the first of `examples`, or a body generated from the schema; JSON media types
  are preferred, and `content-type` is set to the media type.

The response is then checked:

1. **Status code** must be declared: the exact code, a range such as `2XX`, or `default`.
2. **Content type**: when the declared response has `content`, the response `content-type` (without parameters) must
   match one of its media types; `application/*` and `*/*` match by type. A response that declares content with a
   schema but comes back with no body and no content type fails.
3. **Body**: for JSON media types with a `schema`, the body must parse as JSON and validate against the schema (OpenAPI
   `nullable` is honoured, `$ref`s are resolved). Up to 20 schema errors are reported, each with the JSON pointer of
   the failing value, for example `body /id: "seven" is not of type "integer"`. Bodies over 1 MiB are not validated
   and fail the check.

A request that cannot be sent (refused connection, timeout, DNS error, a target outside the egress policy) gets the
verdict `error`; a response that violates the contract gets `failed`; otherwise `passed`.

Requests are sent with the same HTTP client as callbacks and proxying: the **egress policy**
(`FASTMOCK_CALLBACK_ALLOWED_HOSTS`, `FASTMOCK_CALLBACK_DENIED_CIDRS`) applies to `targetUrl` and to every address its
host resolves to, redirects are not followed, and no proxy from the environment is used. Up to 4 requests of a run are
in flight at a time.

## What an AsyncAPI run does

For every Kafka operation that the document says the application **publishes** (2.x `subscribe`, 3.0 `send`; the same
selection and topic mapping as the [AsyncAPI import](asyncapi.md#what-is-imported)), FastMock connects to
`FASTMOCK_KAFKA_BOOTSTRAP`, positions itself at the end of every partition of the topic (or at the earliest offset with
`fromBeginning`), and waits up to `waitMs` for `messageCount` messages. Each message is checked:

- JSON messages must parse and validate against the message payload schema (a payload schema of type `string`
  accepts any text).
- Avro messages (`schemaFormat` `application/vnd.apache.avro...`) must decode with the schema of the document, with
  or without the 5-byte Confluent header.

The operation passes when at least one message arrived and all received messages are valid; it fails when a message
is invalid or none arrived in time.

Reading from Kafka is **off by default**. Contract tests read only the topics listed in
`FASTMOCK_KAFKA_CONTRACT_TOPICS` (comma-separated; an entry ending in `*` is a prefix). This list is separate from
`FASTMOCK_KAFKA_ALLOWED_TOPICS`, which only controls where mocks may publish. A `prefix*` entry, including `*`, never
matches internal topics (names starting with `_`, such as `__consumer_offsets` or `_schemas`) unless the prefix
itself starts with `_`. A run whose document selects a topic outside the list answers `400`:

```sh
FASTMOCK_KAFKA_CONTRACT_TOPICS=streetlights.*,orders-events
```

```json
{"error": "topic payments is not in FASTMOCK_KAFKA_CONTRACT_TOPICS; contract tests may read only the topics listed there", "details": []}
```

WebSocket operations are not tested. Typical use: start the run, then make the service produce, for example by
calling its API or by running the consumer test that triggers it.

```sh
curl -X POST localhost:9090/__admin/contract-tests \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile spec streetlights.yaml '{spec: $spec, operations: ["sendLightMeasured"], messageCount: 3, waitMs: 30000}')"
```

## Reading results

```sh
curl localhost:9090/__admin/contract-tests/0193a5d0-1c2e-7b44-8a10-5d3e9c7f0a11 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{
  "id": "0193a5d0-1c2e-7b44-8a10-5d3e9c7f0a11",
  "kind": "openapi",
  "status": "failed",
  "service": "Swagger Petstore:1.0.19",
  "targetUrl": "https://petstore.example.com/api/v3",
  "startedAt": "2026-11-19T10:00:00.120Z",
  "finishedAt": "2026-11-19T10:00:00.874Z",
  "durationMs": 754,
  "startedBy": "ci",
  "summary": {"total": 2, "passed": 1, "failed": 1, "errors": 0},
  "results": [
    {
      "operation": "getPetById",
      "verdict": "failed",
      "request": {
        "method": "GET",
        "url": "https://petstore.example.com/api/v3/pet/10",
        "headers": {"authorization": "***", "x-test-run": "ci-1234"}
      },
      "response": {
        "status": 200,
        "contentType": "application/json",
        "bodyExcerpt": "{\"id\":\"ten\",\"name\":\"doggie\",\"photoUrls\":[]}",
        "bodyBytes": 41
      },
      "errors": ["body /id: \"ten\" is not of type \"integer\""],
      "durationMs": 212
    },
    {
      "operation": "POST /pet",
      "example": "dog",
      "verdict": "passed",
      "request": {"method": "POST", "url": "https://petstore.example.com/api/v3/pet", "headers": {"authorization": "***", "content-type": "application/json", "x-test-run": "ci-1234"}, "body": "{\"id\":10,\"name\":\"doggie\",\"photoUrls\":[]}"},
      "response": {"status": 200, "contentType": "application/json", "bodyExcerpt": "{\"id\":10,\"name\":\"doggie\",\"photoUrls\":[]}", "bodyBytes": 41},
      "errors": [],
      "durationMs": 230
    }
  ]
}
```

Run `status`: `running`, then `passed` (every operation passed), `failed` (at least one failed), or `error` (no
failures but at least one operation could not be tested, or the run exceeded 15 minutes). AsyncAPI results have
`topic` and `messages` (`partition`, `offset`, `key`, `excerpt`, `errors`) instead of `request` and `response`.

Viewers get verdicts only. For a `viewer` token the run carries `"detailsHidden": true`, `request` is omitted,
`errors` is empty, `response` keeps `status` and `bodyBytes` with an empty `bodyExcerpt`, and every message keeps
`partition` and `offset` with an empty `excerpt`:

```json
{
  "id": "0193a5d0-1c2e-7b44-8a10-5d3e9c7f0a11",
  "status": "failed",
  "summary": {"total": 2, "passed": 1, "failed": 1, "errors": 0},
  "results": [
    {"operation": "getPetById", "verdict": "failed", "response": {"status": 200, "bodyExcerpt": "", "bodyBytes": 41}, "errors": [], "durationMs": 212},
    {"operation": "POST /pet", "example": "dog", "verdict": "passed", "response": {"status": 200, "bodyExcerpt": "", "bodyBytes": 41}, "errors": [], "durationMs": 230}
  ],
  "detailsHidden": true
}
```

| Route | Purpose |
|---|---|
| `POST /contract-tests` | start a run (`202`) |
| `GET /contract-tests?limit=50` | runs, newest first, without `results` (`limit` 1..200) |
| `GET /contract-tests/{id}` | one run with all results (verdicts only for viewers) |
| `DELETE /contract-tests/{id}` | delete a run (`204`) |

## Using a stored specification

`specRef` names a specification stored by FastMock's artifact import (the `specs` store, key = service id, value
`{"name", "version", "type", "document"}`, where `document` is the JSON object or the JSON/YAML text). This lets CI
test against exactly the version that the mocks were generated from:

```sh
curl -X POST localhost:9090/__admin/contract-tests -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"specRef": "petstore:1.0.19", "targetUrl": "https://petstore.example.com/api/v3"}'
```

An unknown `specRef` answers `404`.

## Secrets and redaction

Credentials for the service under test should not be sent in the request body of the run. Put them in a server-side
secret, **bind** the secret to the hosts that may receive it, and reference it by name in `secretHeaders`:

```sh
FASTMOCK_SECRET_PETSTORE_TOKEN='Bearer <token>'      # or a file petstore-token in FASTMOCK_SECRETS_DIR
FASTMOCK_CONTRACT_SECRETS='petstore-token=petstore.example.com|*.petstore.example.com'
```

```json
{"targetUrl": "https://petstore.example.com/api/v3", "secretHeaders": {"Authorization": "petstore-token"}}
```

Rules:

- `FASTMOCK_CONTRACT_SECRETS` is a comma-separated list of `name=host1|host2` bindings. A host is an exact name or
  IP address (`198.51.100.7`, `[2001:db8::7]`), or `*.example.com` for any subdomain (not `example.com` itself).
  Only these names can be used in `secretHeaders`, and only when the host of `targetUrl` matches one of the hosts
  bound to the name. Unknown names and names without a value answer `400` with the same message; a host that is not
  bound answers `400`.
- Contract secrets are separate from stub secrets. A name in `FASTMOCK_STUB_SECRETS` (the HMAC keys of callbacks)
  can never be used in a contract test, and FastMock refuses to start when a name appears in both lists.
- Starting a run with `secretHeaders` needs the `admin` role. Editors can still run contract tests without them.
- A secret value must be at least 8 bytes long. Secret header values are never stored: they show as `***` in the run.
  Once a run has used a secret, its exact value is also masked wherever it appears in stored data of this replica:
  response excerpts, error texts, and the request journal (headers, query strings, bodies). A service that echoes
  the credential back therefore does not reveal it.
- Anyone who can start a run with the secret can send it to every bound host. Bind a secret only to the service it
  belongs to, never to a host that routes back to FastMock or to a host that an editor controls, and keep
  `secretHeaders` for admins.

Other stored data goes through the same redaction as the request journal (`FASTMOCK_REDACT_HEADERS`,
`FASTMOCK_REDACT_JSON_KEYS`): sensitive headers, query parameters and JSON fields in request bodies, response
excerpts and message excerpts are masked, and so are sensitive query parameters of the stored `targetUrl`. Excerpts
are cut to 4 KiB and are visible to editors only.

## CI usage

Start a run, poll it until it is no longer `running`, print the failures and fail the job unless it passed:

```sh
#!/usr/bin/env bash
set -euo pipefail
FASTMOCK=${FASTMOCK:-http://fastmock-admin.example.com:9090}
auth=(-H "Authorization: Bearer $FASTMOCK_TOKEN")

id=$(curl -sf "${auth[@]}" -H 'content-type: application/json' \
  -X POST "$FASTMOCK/__admin/contract-tests" \
  -d "$(jq -n --rawfile spec api/openapi.yaml --arg url "$SERVICE_URL" \
        '{spec: $spec, targetUrl: $url, secretHeaders: {Authorization: "ci-service-token"}}')" | jq -r .id)
echo "contract test run $id"

for _ in $(seq 1 180); do
  run=$(curl -sf "${auth[@]}" "$FASTMOCK/__admin/contract-tests/$id")
  status=$(jq -r .status <<<"$run")
  [ "$status" != "running" ] && break
  sleep 2
done

jq -r '"\(.status): \(.summary.passed)/\(.summary.total) passed"' <<<"$run"
jq -r '.results[] | select(.verdict != "passed")
       | "\(.verdict | ascii_upcase) \(.operation)\(if .example then " [" + .example + "]" else "" end): \(.errors | join("; "))"' <<<"$run"
[ "$status" = "passed" ]
```

The same in a GitLab CI job:

```yaml
contract-test:
  stage: test
  image: alpine:3.22
  before_script:
    - apk add --no-cache bash curl jq
  script:
    - bash ci/contract-test.sh
  variables:
    SERVICE_URL: https://orders.test.example.com
```

The script exits non-zero when the run is `failed` or `error`, or when it is still running after 6 minutes, so the
pipeline fails.

## Metrics, traces and logs

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_contract_tests_total` | counter | `result` = `passed`, `failed`, `error` | Finished runs. |
| `fastmock_contract_test_duration_seconds` | histogram | none | Run duration. |
| `fastmock_contract_test_requests_total` | counter | `verdict` = `passed`, `failed`, `error` | Tested requests and consumed operations. |
| `fastmock_contract_tests_rejected_total` | counter | `reason` = `self_target`, `secret_host` | Runs refused because the target is FastMock itself or a secret is not bound to the target host. |

Each run has a span `fastmock.contract_run` (`fastmock.run_id`); each request a child span `fastmock.contract_test`
(`otel.kind=client`, `http.request.method`, `url.full`, `http.response.status_code`, `fastmock.operation`), each
AsyncAPI operation one with `otel.kind=consumer` and `messaging.destination.name`. Starting and deleting runs is
written to the audit log (`contract_test.start` with the names of the secret headers, never their values;
`contract_test.delete`); finished runs are logged at `info`.

## Limits and retention

| Limit | Value |
|---|---|
| Requests (or AsyncAPI operations) per run | 200 |
| Examples per operation | 10 |
| Concurrent runs per replica | 4 (more answer `429`) |
| Concurrent requests per run | 4 |
| Run duration | 15 minutes, then `error` |
| Response body read and validated | 1 MiB |
| Stored excerpts | 4 KiB each |
| Schema errors reported per request or message | 20 |
| Headers per run | 64 |
| Stored runs | the newest 200, none older than 7 days |

Retention is applied when a new run starts: older runs are removed, and a run that is still `running` 30 minutes
after it started (its replica stopped before it finished) is marked `error`.

## Errors

| Status | When |
|---|---|
| `202` | The run started. |
| `400` | Both or neither of `spec` and `specRef`; the document is not OpenAPI 3.x or AsyncAPI 2.x/3.0; no operation selected; more than 200 requests; `targetUrl` missing (OpenAPI), invalid, denied by the egress policy or pointing at FastMock itself; a secret that is not in `FASTMOCK_CONTRACT_SECRETS`, has no value, is shorter than 8 bytes or is not bound to the target host; `secretHeaders` without an HTTP `targetUrl`; an invalid header or a [header that is never sent](#headers-that-are-never-sent); a Kafka topic outside `FASTMOCK_KAFKA_CONTRACT_TOPICS`; `timeoutMs`, `messageCount` or `waitMs` out of range. |
| `401` / `403` | Missing token, a viewer token on `POST` or `DELETE`, or a non-admin token with `secretHeaders`. |
| `404` | Unknown `specRef` or run id. |
| `429` | 4 runs are already running on this replica. |
