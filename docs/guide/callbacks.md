# Callbacks (webhooks)

A stub can send one or more HTTP requests after it has answered: delivery reports, payment notifications,
status webhooks. FastMock calls these **callbacks**. They are durable (stored before the response is returned),
delivered by any replica, retried with backoff, optionally signed with HMAC, traced, and visible through the admin
API so tests can wait for them.

- [Defining a callback](#defining-a-callback)
- [How delivery works](#how-delivery-works)
- [Retries and failure](#retries-and-failure)
- [What the receiver gets](#what-the-receiver-gets)
- [HMAC signatures](#hmac-signatures)
- [Correlation and waiting in tests](#correlation-and-waiting-in-tests)
- [Listing and clearing callbacks](#listing-and-clearing-callbacks)
- [Egress policy](#egress-policy)
- [Metrics, traces and logs](#metrics-traces-and-logs)
- [Limits](#limits)

The examples use `FASTMOCK_TOKEN` for an editor or admin token, the admin API on `localhost:9090` and the data
plane on `localhost:8080` (see [Getting started](getting-started.md)).

## Defining a callback

```json
{
  "name": "create payment",
  "request": {"method": "POST", "urlPath": "/payments"},
  "response": {"status": 202, "jsonBody": {"id": "{{request.json.orderId}}", "status": "PENDING"}},
  "callbacks": [{
    "url": "http://shop.example.com/hooks/payments",
    "method": "POST",
    "headers": {"X-Event-Id": "{{uuid}}"},
    "jsonBody": {"orderId": "{{request.json.orderId}}", "status": "PAID", "amount": "{{json request.json.amount}}"},
    "delay": {"minMs": 500, "maxMs": 2000},
    "maxAttempts": 5,
    "correlation": "{{request.json.orderId}}",
    "hmac": {"header": "X-Signature", "secretRef": "delivery-hook", "algorithm": "sha256", "prefix": "sha256="}
  }]
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `url` | template | required | Target URL, `http` or `https`. Rendered per request, so it can come from the request: `"{{request.json.callbackUrl}}"`. Subject to the [egress policy](#egress-policy). |
| `method` | string | `POST` | Any HTTP method. |
| `headers` | map of templates | none | Extra request headers. `traceparent` is set by FastMock and ignored here. `Host`, `Content-Length`, hop-by-hop headers and routing headers (`Forwarded`, `X-Forwarded-*`, `Proxy-*` and the others listed in [Contract testing](contract-testing.md#headers-that-are-never-sent)) are rejected with `400` when the stub is saved. |
| `body` | template | none | Text body. |
| `jsonBody` | JSON with templates | none | JSON body; sets `content-type: application/json` unless `headers` sets a content type. A string that is exactly `"{{json expr}}"` keeps the JSON type of the value (number, object, array). |
| `delay` | object | none (send at once) | `{"fixedMs": n}` or `{"minMs": a, "maxMs": b}` (uniformly random). At most 7 days. |
| `maxAttempts` | integer | `3` | Total delivery attempts, 1 to 50. |
| `correlation` | template | none | Correlation id for finding the callback and the request that caused it. See [Correlation](#correlation-and-waiting-in-tests). |
| `hmac` | object | none | Signs the body. See [HMAC signatures](#hmac-signatures). |

Every template has the same context as the response: `request.method`, `path`, `pathParams`, `query`, `headers`,
`body`, `json`, the scenario `state`, and all helpers (see [Templating](templating.md)). A stub may have any number
of callbacks; each becomes its own delivery with its own id, delay and retries.

Callbacks are validated when the stub is saved (`POST /__admin/stubs/validate` or `fastmock validate` check them
too): templates must compile, `maxAttempts` must be 1..50, the delay at most 7 days and the method valid.

## How delivery works

1. A request matches the stub. FastMock renders each callback and writes it to the **outbox** (table `fm_outbox`
   in PostgreSQL, or process memory in memory mode) with status `pending` and a due time of now plus `delay`.
   Only then is the response returned, so a callback is never lost because a replica stopped right after
   answering.
2. Every replica polls the outbox (`FASTMOCK_CALLBACK_POLL`, 200 ms) and claims due callbacks with a lease, up to
   `FASTMOCK_CALLBACK_BATCH` at a time and `FASTMOCK_CALLBACK_WORKERS` in parallel. Claiming uses row locks, so
   two replicas never deliver the same attempt at once.
3. The attempt is sent through the shared outgoing HTTP client with the timeout `FASTMOCK_CALLBACK_TIMEOUT`
   (10 s), no redirects and the egress policy.
4. The result is written back: `sent` on any 2xx, a retry or `failed` otherwise.

Delivery is **at-least-once**. If a replica stops in the middle of an attempt, or a receiver answers after the
timeout, the lease (twice the timeout plus 5 s) expires and another attempt is made. Receivers should
deduplicate on the `x-fastmock-delivery-id` header, which is the same for every attempt of one callback. A late
result of an attempt whose lease expired never overwrites the result of the attempt that replaced it (counted in
`fastmock_callbacks_stale_results_total`), and `maxAttempts` is never exceeded.

Callbacks that are due while all replicas are down are delivered when a replica starts again. Order between
different callbacks is not guaranteed; use `delay` to order them in time.

## Retries and failure

| Outcome of an attempt | Retried? | Result |
|---|---|---|
| 2xx | no | `sent`, `lastStatus` set |
| connection error, DNS failure, timeout | yes | `pending` with a later due time, `lastError` set |
| 408, 429, any 5xx | yes | `pending` with a later due time, `lastStatus` and `lastError` set |
| any other status (3xx, other 4xx) | no | `failed`, `lastError: "unexpected status 400"` |
| blocked by the egress policy (host not in the allow-list, literal IP in a denied range), invalid URL or method | no | `failed`, `lastError` explains why |
| the callback could not be rendered (template error, missing HMAC secret) | never sent | `failed` with `attempts: 0` and `lastError: "callback could not be built: ..."` |

The delay before attempt *n + 1* is `500 ms × 2^(n−1)`, capped at 60 s: 0.5 s, 1 s, 2 s, 4 s, 8 s, 16 s, 32 s,
60 s, 60 s... When the last allowed attempt fails, the callback becomes `failed`.

Results from a local run (receivers that answer 204, three times 500 then 204, 400, a missing secret, a metadata
address and a closed port):

```text
correlation  status  attempts  maxAttempts  lastStatus  lastError
o-ok         sent    1         3            204
o-flaky      sent    3         5            204
o-bad        failed  1         5            400         unexpected status 400
o-nosecret   failed  0         3                        callback could not be built: secret `not-allowed` is not configured
o-meta       failed  1         3                        blocked by egress policy: address 169.254.169.254 is denied by FASTMOCK_CALLBACK_DENIED_CIDRS
o-down       failed  2         2                        request failed: error sending request for url (<url>): client error (Connect): tcp connect error: Connection refused (os error 111)
```

URLs are never included in `lastError`, so tokens in query strings do not leak through the API.

## What the receiver gets

Headers sent with every attempt (captured from a real delivery):

```text
content-type: application/json
x-event-id: aa1d6e34-5000-4b2d-9f8e-ff502a03ba63
x-signature: sha256=bf624d066ae48b9e672f4034a05effecd6d8ff1788388b1654b5658815de5486
x-fastmock-delivery-id: 01a1f259-a1a8-770c-8b05-767ed2d58716
x-fastmock-delivery-attempt: 1
traceparent: 00-e93b518fa79f551734789b4ed451a69a-162351e1c6282586-01
user-agent: FastMock/0.1.0-dev14
```

| Header | Meaning |
|---|---|
| `x-fastmock-delivery-id` | Id of the callback (UUIDv7), identical on every attempt. Use it to deduplicate. |
| `x-fastmock-delivery-attempt` | Attempt number, starting at 1. |
| `traceparent` | W3C trace context. The trace id is the trace of the request that caused the callback, so a tracing backend shows the request and its callbacks in one trace. |
| `user-agent` | `FastMock/<version>` |

The body is sent exactly as rendered; HTTP/1.1 header names arrive in lower case.

## HMAC signatures

`hmac` signs the exact body bytes with a server-side secret and puts the signature in a header:

| Field | Default | Meaning |
|---|---|---|
| `header` | required | Header that carries the signature, for example `X-Signature` or `X-Hub-Signature-256`. |
| `secretRef` | required | Name of a server-side secret. The stub never contains the key. |
| `algorithm` | `sha256` | `sha256` (HMAC-SHA256) or `sha512` (HMAC-SHA512). |
| `encoding` | `hex` | `hex` (lower case) or `base64` (standard alphabet with padding). |
| `prefix` | none | Text before the signature, for example `sha256=`. |

The secret must be configured on the server **and** allowed for stubs:

```sh
FASTMOCK_SECRET_DELIVERY_HOOK='a-long-random-key'   # or a file named delivery-hook in FASTMOCK_SECRETS_DIR
FASTMOCK_STUB_SECRETS=delivery-hook                  # names that stubs may reference
```

`FASTMOCK_STUB_SECRETS` keeps editors from making FastMock sign arbitrary content with keys that are meant for
plugins or other integrations. With Helm, put the key into the Secret named by `secrets.existingSecret` and list
its name in `secrets.stubSecrets` ([Helm values](../operations/helm.md#values)).

The signature header is masked (`***`) in the callbacks API, the journal and logs.

### Verifying on the receiver

Shell, with the raw body in `$BODY` and the header value in `$SIG`:

```sh
SECRET='a-long-random-key'
expected="sha256=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | awk '{print $NF}')"
[ "$expected" = "$SIG" ] && echo "signature OK" || echo "signature mismatch"
```

Java 25, comparing in constant time:

```java
import java.security.MessageDigest;
import java.util.HexFormat;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

public final class WebhookSignature {

    private WebhookSignature() {
    }

    public static boolean verify(byte[] rawBody, String headerValue, byte[] secret) throws Exception {
        if (headerValue == null || !headerValue.startsWith("sha256=")) {
            return false;
        }
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(secret, "HmacSHA256"));
        byte[] expected = mac.doFinal(rawBody);
        byte[] presented;
        try {
            presented = HexFormat.of().parseHex(headerValue.substring("sha256=".length()));
        } catch (IllegalArgumentException e) {
            return false;
        }
        return MessageDigest.isEqual(expected, presented);
    }
}
```

Both snippets were checked against a signature produced by FastMock. Always verify the raw bytes as received,
not a re-serialized JSON object.

## Correlation and waiting in tests

`correlation` is rendered from the request (an order id, a message id, a phone number). FastMock stores it on the
callback, and the correlation of the first callback that has one is also stored on the journal entry of the
request. Tests can then ask for exactly "the callbacks of this order":

```sh
curl -s "localhost:9090/__admin/callbacks?correlation=ord-1001" -H "Authorization: Bearer $FASTMOCK_TOKEN"
curl -s "localhost:9090/__admin/requests?correlation=ord-1001" -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

The [Java client](java-client.md) wraps this as `awaitCallbacks(correlation, n, timeout)`, which waits until `n`
callbacks exist and none of them is still `pending`.

## Listing and clearing callbacks

`GET /__admin/callbacks` (viewer) returns the newest callbacks first.

| Query parameter | Meaning |
|---|---|
| `correlation` | exact correlation id |
| `status` | `pending`, `sent` or `failed` |
| `source` | `stubs` for stub callbacks, or a plugin name for callbacks that a plugin scheduled |
| `limit` | 1 to 10000, default 100 |

```sh
curl -s "localhost:9090/__admin/callbacks?correlation=ord-1001" -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{"callbacks": [{
  "id": "01a1f259-4b87-761a-9c67-253719bb1cb6",
  "source": "stubs",
  "correlation": "ord-1001",
  "url": "http://fastmock:8080/hooks/payments",
  "method": "POST",
  "headers": {"content-type": "application/json", "x-signature": "***"},
  "body": "{\"orderId\":\"ord-1001\",\"status\":\"PAID\"}",
  "dueAt": "2026-11-19T05:48:21.243575093Z",
  "attempts": 1,
  "maxAttempts": 3,
  "status": "sent",
  "lastStatus": 204,
  "createdAt": "2026-11-19T05:48:20.743583177Z",
  "sentAt": "2026-11-19T05:48:21.275466677Z",
  "traceparent": "00-28ce92460df8eb5bd2f930e6c00f5586-93357723223cfba6-01"
}]}
```

Headers and the body are shown with the same [redaction](../operations/security.md#redaction) as the journal and
truncated to `FASTMOCK_JOURNAL_MAX_BODY_BYTES`.

| Call | Role | Effect |
|---|---|---|
| `DELETE /__admin/callbacks` | editor | Deletes every callback, including pending ones (they will not be sent). Returns `{"deleted": n}`. |
| `POST /__admin/reset` | admin | Clears the journal, callbacks and scenario states. |

Delivered and failed callbacks are deleted automatically after `FASTMOCK_CALLBACK_RETENTION` (24 h).

## Egress policy

Callbacks, [proxy stubs](proxy-and-chaos.md) and [recording](recording.md) share one policy:

- only `http` and `https`, no user name or password in the URL, no redirects, no proxy from the environment;
- `FASTMOCK_CALLBACK_ALLOWED_HOSTS`, when set, lists the only host names allowed (`hooks.example.com`,
  `*.svc.cluster.local`);
- `FASTMOCK_CALLBACK_DENIED_CIDRS` lists address ranges that are never contacted. The default covers loopback,
  link-local (including cloud metadata endpoints), unspecified and multicast addresses. The check runs on the
  addresses the name actually resolves to, inside the resolver, so a name cannot point somewhere else between the
  check and the connection.

A callback whose host is not allowed, or whose URL contains a literal IP in a denied range, fails at once (no
retries) with `lastError: "blocked by egress policy: ..."` and is counted as
`fastmock_callbacks_total{result="blocked"}`. A host name whose addresses are all denied fails at connection
time like an unreachable host, so it is retried up to `maxAttempts`, with
`lastError: "request failed: ... dns error: all addresses of localhost are denied by FASTMOCK_CALLBACK_DENIED_CIDRS"`.

Because loopback is denied by default, a callback to `http://localhost:8080/...` from FastMock to itself is
blocked. Address the server by its service or host name instead (`http://fastmock:8080/...` in Docker Compose or
Kubernetes). On a developer machine you can narrow the denied list, for example
`FASTMOCK_CALLBACK_DENIED_CIDRS=169.254.0.0/16`.

In Kubernetes, enforce the same boundary at the network level with `networkPolicy.egress`
([Helm](../operations/helm.md#networkpolicy)).

## Metrics, traces and logs

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_callbacks_scheduled_total` | `source` | Callbacks written to the outbox by stubs. |
| `fastmock_callbacks_total` | `source`, `result` (`sent`, `retry`, `failed`, `blocked`) | Attempt outcomes. |
| `fastmock_callback_duration_seconds` | `source` | Histogram of attempt duration. |
| `fastmock_callbacks_pending` | none | Callbacks waiting in the outbox (all replicas), updated every `FASTMOCK_STUB_POLL`. |
| `fastmock_callbacks_stale_results_total` | none | Results ignored because a newer attempt owned the callback. |

Each attempt is a client span `fastmock.callback` (`otel.name` = `<source> callback`) with the attributes
`http.request.method`, `url.full` (without the query string), `fastmock.source`, `fastmock.callback_id`,
`fastmock.attempt` and `http.response.status_code`. Its parent is the server span of the request that created the
callback, even when the attempt runs minutes later on another replica.

Log lines (JSON):

```json
{"level":"WARN","message":"callback attempt failed","callback_id":"01a1f259-a1b1-76f3-a75b-3bb920a85dad","attempt":2,"error":"unexpected status 500","retry":true,"target":"fastmock_server::outbox"}
{"level":"INFO","message":"callback delivered","callback_id":"01a1f259-a1b1-76f3-a75b-3bb920a85dad","status":204,"target":"fastmock_server::outbox"}
```

The chart's alerts `FastMockCallbacksFailing` and `FastMockCallbackBacklog` watch these metrics
([Observability](../operations/observability.md#alerts)). For stuck or failing callbacks see
[Troubleshooting](../operations/troubleshooting.md#callbacks-stay-pending-or-fail).

## Limits

| Limit | Value |
|---|---|
| `delay` | up to 7 days |
| `maxAttempts` | 1 to 50 |
| backoff | 0.5 s doubling, at most 60 s |
| one attempt | `FASTMOCK_CALLBACK_TIMEOUT` (10 s) |
| parallel deliveries per replica | `FASTMOCK_CALLBACK_WORKERS` (32) |
| stored after completion | `FASTMOCK_CALLBACK_RETENTION` (24 h) |
