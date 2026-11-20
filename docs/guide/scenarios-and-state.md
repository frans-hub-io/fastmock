# Scenarios and state

Stubs are stateless by default: the same request gets the same answer. **Scenarios** add state machines, so a
mock can behave like the real service over a sequence of calls: an order that is created, paid and shipped, a
provider that fails twice and then succeeds, a token that can be used once. The **key-value store** holds values
that tests set and plugins use.

- [Scenario rules](#scenario-rules)
- [Templated scenario names](#templated-scenario-names)
- [Example: order lifecycle](#example-order-lifecycle)
- [Example: fail twice, then succeed](#example-fail-twice-then-succeed)
- [Expiry](#expiry)
- [Concurrency and replicas](#concurrency-and-replicas)
- [Inspecting, setting and resetting state](#inspecting-setting-and-resetting-state)
- [Key-value store](#key-value-store)
- [Limits](#limits)

## Scenario rules

A stub joins a scenario with the `scenario` block:

```json
{
  "scenario": {"name": "checkout", "requiredState": "Started", "newState": "PAID", "ttlSeconds": 3600},
  "request": {"method": "POST", "urlPath": "/checkout"},
  "response": {"status": 200, "jsonBody": {"status": "PAID"}}
}
```

| Field | Default | Meaning |
|---|---|---|
| `name` | required | Scenario name; a [template](templating.md), so it can depend on the request. |
| `requiredState` | none | The stub matches only while the scenario is in this state. Without it the stub matches in every state. |
| `newState` | none | After the stub matched, the scenario moves to this state. Without it the state does not change. |
| `ttlSeconds` | `604800` (7 days) | How long the state set by this stub is kept, 1 to 31536000 seconds. Only used with `newState`. |

Every scenario starts in the state **`Started`**. A scenario that was never touched, was reset or has expired is
in `Started`.

Matching works as usual (see [Request matching](request-matching.md)): stubs are tried in priority order; a stub
whose `requiredState` differs from the current state is skipped and the next candidate is tried. So give a
"fallback" stub without `scenario` a higher `priority` number if it should answer when no state-specific stub
applies.

Templates in the response, headers, callbacks and messages see the scenario as `state`:

| Variable | Value |
|---|---|
| `state.scenario` | the rendered scenario name, for example `order-A-1` |
| `state.current` | the state **before** this request's transition |

## Templated scenario names

Because `name` is a template, one set of stubs drives a separate state machine per order, user or message:

```json
{"scenario": {"name": "order-{{request.pathParams.id}}", "requiredState": "CREATED", "newState": "PAID"}}
```

Order `A-1` and order `B-2` then have independent states (`order-A-1`, `order-B-2`). Anything in the template
context can be used: `{{request.json.orderId}}`, `{{request.headers.x-client-id}}`, `{{request.query.session}}`.
The rendered name may be up to 512 bytes; a longer name makes the request fail with 500
(`stub evaluation failed`).

## Example: order lifecycle

Six stubs, imported together under the tag `orders` (`replaceTag` replaces earlier versions of the same set):

```sh
curl -s -X POST localhost:9090/__admin/stubs/import \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
"replaceTag": "orders",
"stubs": [
  {"name": "create order", "request": {"method": "POST", "urlPath": "/orders"},
   "scenario": {"name": "order-{{request.json.id}}", "requiredState": "Started", "newState": "CREATED", "ttlSeconds": 3600},
   "response": {"status": 201, "jsonBody": {"id": "{{request.json.id}}", "status": "CREATED"}}},
  {"name": "order is created", "request": {"method": "GET", "urlPathTemplate": "/orders/{id}"},
   "scenario": {"name": "order-{{request.pathParams.id}}", "requiredState": "CREATED"},
   "response": {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "CREATED"}}},
  {"name": "pay order", "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/pay"},
   "scenario": {"name": "order-{{request.pathParams.id}}", "requiredState": "CREATED", "newState": "PAID", "ttlSeconds": 3600},
   "response": {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "PAID", "previous": "{{state.current}}"}}},
  {"name": "order is paid", "request": {"method": "GET", "urlPathTemplate": "/orders/{id}"},
   "scenario": {"name": "order-{{request.pathParams.id}}", "requiredState": "PAID"},
   "response": {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "PAID"}}},
  {"name": "pay twice", "priority": 6, "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/pay"},
   "response": {"status": 409, "jsonBody": {"error": "order {{request.pathParams.id}} cannot be paid"}}},
  {"name": "unknown order", "priority": 9, "request": {"method": "GET", "urlPathTemplate": "/orders/{id}"},
   "response": {"status": 404, "jsonBody": {"error": "order {{request.pathParams.id}} not found"}}}
]}'
```

```text
{"imported":6,"removed":0}
```

The flow (real responses):

```text
GET  /orders/A-1          -> {"error":"order A-1 not found"}
POST /orders {"id":"A-1"} -> {"id":"A-1","status":"CREATED"}
GET  /orders/A-1          -> {"id":"A-1","status":"CREATED"}
POST /orders/A-1/pay      -> {"id":"A-1","status":"PAID","previous":"CREATED"}
POST /orders/A-1/pay      -> {"error":"order A-1 cannot be paid"}
GET  /orders/A-1          -> {"id":"A-1","status":"PAID"}
GET  /orders/B-2          -> {"error":"order B-2 not found"}
```

The `pay twice` and `unknown order` stubs have no scenario and a higher priority number, so they answer only when
no state-specific stub matched. Add [callbacks](callbacks.md) to `pay order` to send the payment webhook, or
[Kafka messages](kafka.md) to publish an `OrderPaid` event.

## Example: fail twice, then succeed

Useful for testing client retries. The scenario is per client, taken from a header:

```json
{"replaceTag": "flaky-sms", "stubs": [
  {"name": "first attempt fails", "request": {"method": "POST", "urlPath": "/sms"},
   "scenario": {"name": "sms-{{request.headers.x-client-id}}", "requiredState": "Started", "newState": "failed-once", "ttlSeconds": 60},
   "response": {"status": 503, "jsonBody": {"error": "try again"}}},
  {"name": "second attempt fails", "request": {"method": "POST", "urlPath": "/sms"},
   "scenario": {"name": "sms-{{request.headers.x-client-id}}", "requiredState": "failed-once", "newState": "failed-twice", "ttlSeconds": 60},
   "response": {"status": 503, "jsonBody": {"error": "try again"}}},
  {"name": "then it works", "request": {"method": "POST", "urlPath": "/sms"},
   "scenario": {"name": "sms-{{request.headers.x-client-id}}", "requiredState": "failed-twice"},
   "response": {"status": 202, "jsonBody": {"accepted": true, "scenario": "{{state.scenario}}"}}}
]}
```

```text
POST /sms (x-client-id: c1) -> 503 {"error":"try again"}
POST /sms (x-client-id: c1) -> 503 {"error":"try again"}
POST /sms (x-client-id: c1) -> 202 {"accepted":true,"scenario":"sms-c1"}
POST /sms (x-client-id: c1) -> 202 {"accepted":true,"scenario":"sms-c1"}
POST /sms (x-client-id: c2) -> 503 {"error":"try again"}
```

Sixty seconds after the last transition the state of `sms-c1` expires and the cycle starts again.

## Expiry

Each transition stores the new state with the `ttlSeconds` of the stub that made it (default 7 days). When the
time passes, the state disappears and the scenario is back in `Started`. A stub without `newState` does not
extend the expiry. States set through `PUT /__admin/scenarios/{name}` do not expire.

Use short TTLs for per-request scenarios (one per order or message) so state does not accumulate, and the default
or longer for long-running test environments.

## Concurrency and replicas

Scenario states live in Valkey (in memory mode: in the process), so every replica sees the same state.
Transitions use compare-and-set: of two concurrent requests that both see `CREATED` and want to move to `PAID`,
exactly one wins. The other is matched again against the new state (and then gets, for example, the `pay twice`
answer). If the state keeps changing under a request, FastMock gives up after 8 rounds and answers 500.

## Inspecting, setting and resetting state

| Call | Role | Effect |
|---|---|---|
| `GET /__admin/scenarios` | viewer | Lists scenarios that have a stored state (scenarios never touched are implicitly `Started` and not listed): `{"scenarios":[{"name":"order-A-1","state":"PAID"}]}` (up to 10,000). |
| `PUT /__admin/scenarios/{name}` with `{"state": "..."}` | editor | Sets a state (no expiry). Name and state are limited to 512 bytes; percent-encode special characters in the name. Returns 204. |
| `DELETE /__admin/scenarios` | editor | Resets every scenario to `Started`. Returns `{"reset": n}`. |
| `POST /__admin/reset` | admin | Clears the journal, callbacks and all scenario states. |

```sh
curl -s localhost:9090/__admin/scenarios -H "Authorization: Bearer $FASTMOCK_TOKEN"
curl -s -X PUT localhost:9090/__admin/scenarios/order-A-1 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{"state":"CREATED"}'
curl -s -X DELETE localhost:9090/__admin/scenarios -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```text
{"scenarios":[{"name":"order-A-1","state":"PAID"}]}
{"reset":1}
```

Setting a state is the fastest way to start a test in the middle of a flow ("an order that is already paid")
without replaying the requests that lead there. With the Java client: `setScenarioState(name, state)` and
`resetScenarios()` ([Java client](java-client.md)).

## Key-value store

A shared string store with optional expiry, kept next to the scenario states. Tests use it to hand data to
plugins or to keep fixtures that several test processes share.

| Call | Role | Effect |
|---|---|---|
| `PUT /__admin/kv/{key}` with `{"value": "...", "ttlSeconds": 600}` | editor | Stores a string value; `ttlSeconds` (1 to 31536000) is optional. Returns 204. |
| `GET /__admin/kv/{key}` | viewer | `{"key": "...", "value": "..."}`, or 404 `{"error":"key not found","details":[]}`. |
| `DELETE /__admin/kv/{key}` | editor | 204, or 404 when the key did not exist. |

```sh
curl -s -X PUT localhost:9090/__admin/kv/feature-flags -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{"value": "{\"newCheckout\": true}", "ttlSeconds": 600}'
curl -s localhost:9090/__admin/kv/feature-flags -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```text
{"key":"feature-flags","value":"{\"newCheckout\": true}"}
```

Values are strings; store JSON as a string as above. Keys are 1 to 512 bytes and values up to 1 MiB.

With [spaces](../operations/access-control.md#spaces), scenario states and key-value entries belong to a space.
`/kv/{key}` and `PUT /scenarios/{name}` address one entry and pick the space like writes, also for reads: a
principal bound to one space uses that space, a principal with the permission globally uses the global area, and
a principal bound to several spaces must pass `?space=` (otherwise `400`), for example
`GET /__admin/kv/feature-flags?space=billing`.

## Where state lives

| Data | Memory mode | PostgreSQL + Valkey mode |
|---|---|---|
| scenario states | process memory | Valkey key `<FASTMOCK_VALKEY_PREFIX>scenario:<name>` |
| key-value entries | process memory | Valkey key `<FASTMOCK_VALKEY_PREFIX>kv:<key>` |

Valkey should therefore keep its data across restarts and must not evict keys without a TTL; the Valkey that the
Helm chart installs with `valkey.enabled` uses append-only persistence and `maxmemory-policy volatile-lru`. Scenario and
key-value changes are recorded in the [audit log](../operations/security.md#audit-log).

## Limits

| Limit | Value |
|---|---|
| rendered scenario name | 512 bytes |
| state set through the admin API | 512 bytes |
| `ttlSeconds` | 1 to 31536000 |
| concurrent transition retries | 8 |
| key-value key / value | 512 bytes / 1 MiB |
| scenarios listed | 10,000 |
