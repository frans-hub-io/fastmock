# Message capture

FastMock can subscribe to the Kafka topics, MQTT topic filters and AMQP exchanges your application publishes to,
store what arrives, and let tests wait for a message, read it and check how many arrived. It works like
[mail](mail.md) for email: the application publishes as usual, the test asks FastMock.

```text
POST /orders/A-1/ship ──► application ──► Kafka "orders.events" ──► other services
                                                │
                                                └──► FastMock capture source "orders" ──► wait / find / verify
```

- [Enabling capture](#enabling-capture)
- [Capture sources](#capture-sources)
- [How each broker is read](#how-each-broker-is-read)
- [MQTT shared subscriptions](#mqtt-shared-subscriptions)
- [Reading captured messages](#reading-captured-messages)
- [Waiting and verifying in tests](#waiting-and-verifying-in-tests)
- [Matching messages](#matching-messages)
- [Spaces and permissions](#spaces-and-permissions)
- [Replicas and storage](#replicas-and-storage)
- [Limits and retention](#limits-and-retention)
- [Metrics](#metrics)
- [Troubleshooting](#troubleshooting)

## Enabling capture

Capture is off by default. `FASTMOCK_CAPTURE_ENABLED=true` turns it on; the brokers are the ones FastMock publishes
to: `FASTMOCK_KAFKA_*` ([Kafka](kafka.md#server-settings)), `FASTMOCK_MQTT_*` and `FASTMOCK_AMQP_*`
([MQTT and AMQP](mqtt-amqp.md#server-settings)), with the same TLS and credentials. While capture is off,
`/__admin/capture/*` answers `404 message capture is not enabled; set FASTMOCK_CAPTURE_ENABLED=true`. When it is on,
`GET /__admin/version` lists the feature `capture`.

| Variable | Default | Meaning |
|---|---|---|
| `FASTMOCK_CAPTURE_ENABLED` | `false` | Turns capture on. |
| `FASTMOCK_CAPTURE_SOURCES` | none | Sources defined in the configuration: a JSON or YAML list (or `{"sources": [...]}`). |
| `FASTMOCK_CAPTURE_SOURCES_FILE` | none | The same, read from a file. Both may be used; names must be unique per space. |
| `FASTMOCK_CAPTURE_MAX_SOURCES` | `50` | Sources in total (configuration and API), 1..1000. |
| `FASTMOCK_CAPTURE_MAX_MESSAGES` | `1000` | Messages kept per source, oldest dropped first, 1..100000. A source may ask for fewer (`maxMessages`). |
| `FASTMOCK_CAPTURE_MAX_MESSAGE_BYTES` | `1048576` | Body bytes kept per message; longer bodies are cut and marked `truncated` (`size` keeps the full length). At most 16 MiB. |
| `FASTMOCK_CAPTURE_MAX_BYTES` | `268435456` (256 MiB) | Bytes of all stored messages (bodies, topics, keys, headers and properties); beyond it the oldest messages are removed. 1 MiB to 1 TiB. |
| `FASTMOCK_CAPTURE_MAX_SPACE_BYTES` | a quarter of `FASTMOCK_CAPTURE_MAX_BYTES` | Bytes of the stored messages of one space, and of the global area; beyond it the oldest messages of that space are removed. At least 64 KiB, at most `FASTMOCK_CAPTURE_MAX_BYTES`. |
| `FASTMOCK_CAPTURE_MAX_AGE` | `24h` | Messages older than this are removed once a minute. |
| `FASTMOCK_CAPTURE_KAFKA_TOPICS` | none | Allow-list of topics sources may read: names, `prefix*`, or `*` for every topic. |
| `FASTMOCK_CAPTURE_MQTT_TOPICS` | none | Allow-list of MQTT topic filters sources may use (`devices/*` allows `devices/+/state`; `#` needs `#` or `*`). |
| `FASTMOCK_CAPTURE_AMQP_EXCHANGES` | none | Allow-list of exchanges sources may bind to. |

The allow-lists limit what a token with `messages:write` can read through FastMock's broker credentials. Sources
created through the API need the allow-list of their protocol: without `FASTMOCK_CAPTURE_KAFKA_TOPICS`, for example,
`PUT /capture/sources/...` with a Kafka source answers `400` naming the variable. Set it to the topics your tests
read, or to `*` on a broker that only test applications use. Sources from the configuration (`FASTMOCK_CAPTURE_SOURCES`,
`FASTMOCK_CAPTURE_SOURCES_FILE`) need no allow-list, but must match one when it is set. The allow-lists apply to
every space alike: a token with `messages:write` in any space may read every allowed destination, so keep them as
narrow as your tests allow.

Whatever the allow-lists say, sources never read broker internals: MQTT filters starting with `$` (`$SYS/#` and shared
subscriptions, see [MQTT shared subscriptions](#mqtt-shared-subscriptions)), Kafka topics
starting with `__` (`__consumer_offsets`, `__transaction_state`) and AMQP exchanges `amq.rabbitmq.*` are refused.

A start with an invalid source, a source whose broker is not configured, a source outside an allow-list or a space
the access file does not define stops with an error naming it.

## Capture sources

A source subscribes to one broker and stores what it receives in its space:

| Field | Protocols | Meaning |
|---|---|---|
| `name` | all | 1..64 characters of `a-z 0-9 . _ -`, unique in the space. Filters and matchers use it. |
| `protocol` | all | `kafka`, `mqtt` or `amqp`. |
| `topics` | Kafka, MQTT | Kafka topics, or MQTT topic filters with `+` and `#`; at most 20. Not Kafka internal topics (`__*`) nor MQTT filters starting with `$`. |
| `startFrom` | Kafka | `latest` (default): only messages published after the source started; `earliest`: the whole topic first. |
| `qos` | MQTT | Subscription QoS, default `1`. A message is delivered with the lower of its QoS and this one. |
| `exchange` | AMQP | A named exchange (not the default exchange, which cannot be bound, nor `amq.rabbitmq.*`). |
| `bindingKeys` | AMQP | Binding keys of FastMock's queue, default `["#"]` (everything on a topic exchange; ignored by fanout exchanges). |
| `maxMessages` | all | Messages kept, at most `FASTMOCK_CAPTURE_MAX_MESSAGES`. |
| `enabled` | all | `false` keeps the source and its messages but stops the subscription. |
| `description` | all | Free text. |

Through the admin API (`?space=` selects the space, see [Spaces and permissions](#spaces-and-permissions)):

```sh
curl -X PUT localhost:9090/__admin/capture/sources/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e10 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"name": "orders", "protocol": "kafka", "topics": ["orders.events"]}'

curl -X PUT localhost:9090/__admin/capture/sources/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e11 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"name": "devices", "protocol": "mqtt", "topics": ["devices/+/state"], "qos": 1}'

curl -X PUT localhost:9090/__admin/capture/sources/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e12 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"name": "billing-events", "protocol": "amqp", "exchange": "billing", "bindingKeys": ["invoice.*"]}'
```

Or in the configuration, for static setups (the ids are derived from the space and name, so they stay the same
across restarts):

```yaml
# FASTMOCK_CAPTURE_SOURCES_FILE=/etc/fastmock/capture-sources.yaml
- name: orders
  protocol: kafka
  topics: [orders.events]
- name: devices
  space: iot
  protocol: mqtt
  topics: ["devices/+/state"]
```

| Route | Permission | Purpose |
|---|---|---|
| `GET /capture/sources` | `messages:read` | Sources with their state, message counts and the limits. |
| `GET /capture/sources/{id}` | `messages:read` | One source. |
| `PUT /capture/sources/{id}` | `messages:write` | Create or replace (audit `capture.source.put`). Sources from the configuration answer `409`. |
| `DELETE /capture/sources/{id}` | `messages:write` | Delete (audit `capture.source.delete`); `?purge=true` also deletes its messages and needs `messages:delete`, checked before anything is deleted. Without `purge`, the messages stay until the age limit or the byte budgets remove them. |

Every source in a list has a `status`:

```json
{
  "id": "0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e11", "name": "devices", "protocol": "mqtt",
  "topics": ["devices/+/state"], "qos": 1, "enabled": true, "updatedAt": "2026-11-22T02:09:15.002Z",
  "status": {"state": "running", "received": 12, "dropped": 0, "lastMessageAt": "2026-11-22T02:10:01.552Z",
             "since": "2026-11-22T02:09:15.226Z", "stored": 12, "connects": 1}
}
```

`state` is `running` (subscribed), `connecting`, `error` (with `lastError`, for example
`exchange billing does not exist`, `FASTMOCK_MQTT_URL is not configured`, `broker connection refused` or
`TLS handshake failed`), `disabled`, or `stopped` (no replica runs it yet, for example in the first seconds after a
start). `lastError` is a short description that names no broker address, certificate detail or broker response;
the server log of the leading replica has the full error. `connects` above 1 means the consumer reconnected.
Changes take effect within about 5 seconds.

## How each broker is read

FastMock never takes messages away from the application's own consumers:

- **Kafka**: FastMock reads the partitions of the topics directly, without joining a consumer group and without
  committing offsets, so it is invisible to the application's consumer groups. It starts at the latest offsets (or
  the earliest with `startFrom: earliest`); after a broken connection it continues from the latest offsets, so
  messages published while it was disconnected are not captured. New partitions are picked up when the source
  restarts. The topics must exist.
- **MQTT**: each source has its own connection (clean session, client id `<prefix>-cap-<random>`) subscribed to the
  filters. Every MQTT subscriber gets its own copy, so the application's subscribers are not affected. Retained
  messages are delivered on subscription and stored with `"retain": true`.
- **AMQP**: FastMock declares its own server-named queue (`exclusive`, `auto-delete`, not durable, `x-max-length`
  10000), binds it to the exchange with the binding keys and consumes from it without acknowledgements. The broker
  delivers a copy of every routed message to each bound queue, so the application's queues receive the same
  messages as before. The queue belongs to FastMock's connection and disappears with it. The user needs the
  `configure` permission on server-named queues (`amq.gen-*`), `write` on them and `read` on the exchange.

## MQTT shared subscriptions

A capture source cannot subscribe to `$share/<group>/<filter>` (nor to any filter starting with `$`); saving it
answers `400` with `starts with $`. This is deliberate. A shared subscription does not give every subscriber a
copy: the broker hands each message to one member of the group. If FastMock joined the group of the application
under test, it would take part of the messages away from the application, and the test would fail or pass for
the wrong reason. A normal subscription gets its own copy of every message, so capture subscribes to the plain
filter (`orders/#`) next to the application's shared subscription and sees every message the application's group
gets, without taking any of them.

So to check what a group of consumers receives, capture the plain topic (the messages are the same ones the
group gets) and assert on those. To feed a group, a stub or async mock publishes to the plain topic as usual;
see [Topics consumed through shared subscriptions](mqtt-amqp.md#topics-consumed-through-shared-subscriptions).

## Reading captured messages

| Route | Permission | Returns |
|---|---|---|
| `GET /capture/messages` | `messages:read` | Summaries, newest first. Filters: `source` (name or id), `protocol`, `topic` (exact topic or routing key), `since`, `until`; `limit` (1..500, default 50), `cursor` (the `nextCursor` of the previous page), `order` (`newest` or `oldest`). |
| `POST /capture/messages/find` | `messages:read` | Messages matching a [match](#matching-messages), newest first: `{"match": {...}, "since": ..., "limit": 50}` → `{messages, count}`. |
| `GET /capture/messages/{id}` | `messages:read` | One message with its decoded body. |
| `GET /capture/messages/{id}/body` | `messages:read` | The stored bytes, as a download. |
| `DELETE /capture/messages/{id}` | `messages:delete` | One message (audit `capture.message.delete`). |
| `DELETE /capture/messages?source=` | `messages:delete` | The messages of the space, or of one source: `{deleted}` (audit `capture.messages.delete`). Use it to reset between tests. |

A summary:

```json
{
  "id": "019aa79c-2b0e-7e51-8f6e-1c3f0a9d2b44", "sourceId": "0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e10",
  "source": "orders", "protocol": "kafka", "topic": "orders.events", "key": "A-1",
  "headers": {"event-type": "OrderShipped"},
  "properties": {"partition": 0, "offset": 118, "timestamp": "2026-11-22T02:10:01.548+00:00"},
  "contentType": "application/json", "size": 63,
  "preview": "{\"orderId\":\"A-1\",\"carrier\":\"DHL\",\"status\":\"SHIPPED\"}",
  "receivedAt": "2026-11-22T02:10:01.552Z"
}
```

`topic` is the Kafka or MQTT topic, or the AMQP routing key, and AMQP messages also have `exchange`. `headers` are
Kafka headers, MQTT 5 user properties or AMQP headers (values as text; a name present twice is joined with `, `).
`properties` holds what else the protocol carries:

| Protocol | Properties |
|---|---|
| Kafka | `partition`, `offset`, `timestamp` |
| MQTT | `qos`, `retain`, and for MQTT 5 `contentType`, `correlationData`, `responseTopic`, `messageExpiryInterval`, `payloadFormatIndicator` |
| AMQP | `contentType`, `contentEncoding`, `messageId`, `correlationId`, `replyTo`, `expiration`, `deliveryMode`, `priority`, `type`, `appId`, `userId`, `timestamp`, `redelivered` |

`GET /capture/messages/{id}` adds the decoded `body`:

| `encoding` | When | `value` |
|---|---|---|
| `json` | The body is JSON. | The JSON value. |
| `avro` | The body is Confluent-framed Avro (byte 0, then the schema id; Kafka, MQTT or AMQP) and `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` knows the id. | The record as JSON, with `schemaId`. |
| `text` | Other UTF-8 text (also a truncated JSON body). | The text. |
| `base64` | Binary data, or Avro that cannot be decoded (then `error` says why, for example `schema registry returned 404 Not Found for schema id 7`). | The base64 of the bytes. |
| `empty` | No body. | `""` |

```json
{"id": "019aa79c-2b0e-7e51-8f6e-1c3f0a9d2b44", "source": "orders", "topic": "orders.events", "...": "...",
 "body": {"encoding": "json", "value": {"orderId": "A-1", "carrier": "DHL", "status": "SHIPPED"}}}
```

## Waiting and verifying in tests

`POST /__admin/capture/messages/wait` blocks until enough matching messages were captured, up to 60 seconds, with
the same fields as [mail](mail.md#waiting-for-a-message):

| Field | Default | Meaning |
|---|---|---|
| `match` | any message | A [match](#matching-messages). |
| `count` | `1` | Messages to wait for, 1 to 500. |
| `timeoutMs` | `10000` | 0 to 60000. |
| `includeExisting` | `false` | Also count matching messages already stored. |
| `since` | none | Also count messages received at or after this time (RFC 3339). |
| `lookbackMs` | `0` | Without `since`: also count messages received up to this long before the request (at most 7 days). |

By default only messages arriving after the request count. A test that triggers the message first and waits
afterwards takes a timestamp before the action and passes it as `since`.

```sh
curl -s localhost:9090/__admin/capture/messages/wait -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
    "match": {"source": {"equalTo": "orders"}, "key": {"equalTo": "A-1"},
              "body": [{"jsonPath": "$.status", "value": {"equalTo": "SHIPPED"}}]},
    "since": "2026-11-22T02:10:00Z", "timeoutMs": 10000}'
```

`200` returns `{"messages": [...], "count": 1}` (summaries, oldest first); `408` reports what arrived in time:

```json
{"error": "timed out after 10000 ms: 0 of 1 matching messages received", "details": [], "count": 0, "expected": 1, "messages": []}
```

`POST /__admin/capture/verify` counts stored messages without waiting. Give `count` for an exact number, or
`atLeast` and/or `atMost` (default: at least 1), and optionally `since`. `200 {"verified": true, ...}` when it holds;
otherwise `422` with the closest messages and the conditions they failed:

```json
{
  "verified": false, "count": 0, "expected": "exactly 1",
  "error": "expected exactly 1 matching messages, received 0",
  "details": ["message 019aa79c-2b0e-7e51-8f6e-1c3f0a9d2b44 (orders orders.events): body[0]"],
  "nearMisses": [{"id": "019aa79c-2b0e-7e51-8f6e-1c3f0a9d2b44", "source": "orders", "topic": "orders.events",
                  "receivedAt": "2026-11-22T02:10:01.552Z", "preview": "{\"orderId\":\"A-1\",...}",
                  "score": 0.5, "mismatches": ["body[0]"]}]
}
```

In test code (Python with `requests`; the same requests work from any language):

```python
import os
from datetime import datetime, timezone

import requests

ADMIN = "http://localhost:9090"
AUTH = {"Authorization": f"Bearer {os.environ['FASTMOCK_TOKEN']}"}

def test_shipping_publishes_an_event():
    since = datetime.now(timezone.utc).isoformat()
    ship_order("A-1")

    r = requests.post(f"{ADMIN}/__admin/capture/messages/wait", headers=AUTH, timeout=20, json={
        "match": {"source": {"equalTo": "orders"}, "key": {"equalTo": "A-1"}},
        "since": since,
        "timeoutMs": 15000,
    })
    assert r.status_code == 200, r.text
    message = requests.get(f"{ADMIN}/__admin/capture/messages/{r.json()['messages'][0]['id']}", headers=AUTH).json()
    assert message["body"]["value"]["status"] == "SHIPPED"

    r = requests.post(f"{ADMIN}/__admin/capture/verify", headers=AUTH, json={
        "match": {"source": {"equalTo": "orders"}}, "count": 1, "since": since,
    })
    assert r.status_code == 200, r.json()["details"]
```

AI assistants get the same through the [MCP tools](mcp.md) `find_messages`, `wait_for_message` and
`verify_messages`.

## Matching messages

A match is an object of optional conditions; all given ones must hold. String conditions are
[string matchers](request-matching.md) as in stubs (`equalTo`, `contains`, `matches`, `caseInsensitive`,
`and`/`or`/`not`, `absent`, ...).

| Field | Matches |
|---|---|
| `source` | the source name |
| `protocol` | `kafka`, `mqtt` or `amqp` (a plain value) |
| `topic` | the topic, or the AMQP routing key |
| `exchange` | the AMQP exchange |
| `key` | the Kafka key |
| `headers` | a map of header name (case-insensitive) to matcher |
| `properties` | a map of property name (`correlationId`, `messageId`, `qos`, `retain`, `partition`, ...) to matcher; numbers and booleans compare as text |
| `contentType` | the content type |
| `body` | a list of [body matchers](request-matching.md) as in stub requests: `equalTo`, `contains`, `matches`, `equalToJson` (with `ignoreExtraElements`), `jsonPath` with `value`, `jsonSchema`, `binaryEqualTo`, ... JSON matchers see Avro bodies decoded. |

```json
{"match": {
  "topic": {"matches": "devices/.+/state"},
  "properties": {"retain": {"equalTo": "true"}},
  "body": [{"equalToJson": {"on": true}, "ignoreExtraElements": true}]
}}
```

## Spaces and permissions

Capture is scoped by [space](../operations/access-control.md#spaces): a source belongs to the space it was created
in (`?space=`, or the caller's only space), and its messages belong to that space too. Reads without `space` cover
every area the caller may read; the global area only with the permission granted globally.

| Permission | Allows |
|---|---|
| `messages:read` | listing sources and messages, reading them, find, wait and verify |
| `messages:write` | creating, changing and deleting sources |
| `messages:delete` | deleting captured messages |

All three can be granted per space. `viewer` has `messages:read`; `editor` has all three. A message or source the
caller cannot read answers `404`.

## Replicas and storage

Every replica runs the capture loop, but only the replica holding the lease `capture:leader` in Valkey subscribes,
the same election as the [async mock publisher](asyncapi.md#publishing-and-leader-election), so each message is
stored once whatever the number of replicas. When the leader stops, another replica takes over within 15 seconds
and subscribes again; messages published in between are not captured (Kafka sources resume from the latest
offsets). The gauge `fastmock_capture_leader` is `1` on the leader.

With PostgreSQL storage, messages are stored in PostgreSQL (table `fm_captured_messages`), so any replica answers
lists, finds, waits and verifications. A wait on the leader wakes up as soon as a message is stored; on other
replicas it reads the store again every 500 ms. With `FASTMOCK_STORAGE=memory` they stay in the process.

## Limits and retention

| Limit | Value |
|---|---|
| sources | 50 (`FASTMOCK_CAPTURE_MAX_SOURCES`) |
| messages per source | 1000, oldest dropped first (`FASTMOCK_CAPTURE_MAX_MESSAGES`, or the source's `maxMessages`) |
| body bytes per message | 1 MiB, longer bodies cut and marked `truncated` (`FASTMOCK_CAPTURE_MAX_MESSAGE_BYTES`) |
| age | 24 hours (`FASTMOCK_CAPTURE_MAX_AGE`), removed once a minute |
| stored bytes | 256 MiB in all (`FASTMOCK_CAPTURE_MAX_BYTES`), 64 MiB per space and for the global area (`FASTMOCK_CAPTURE_MAX_SPACE_BYTES`); the oldest messages are removed first, also those of deleted sources |
| topic, key, headers and properties per message | 64 KiB together; topics and keys up to 4 KiB, at most 100 headers, header and property values up to 8 KiB; what does not fit is dropped and the message marked `truncated` |
| topics, filters or binding keys per source | 20 |
| buffered messages per source | 4096 messages and 16 MiB (at least two messages of the largest size) between the broker and the store, counted after cutting; beyond, messages are dropped and counted in `dropped` |
| MQTT packets of capture connections | `FASTMOCK_CAPTURE_MAX_MESSAGE_BYTES` + 1 MiB, at most 16 MiB; MQTT 5 brokers do not send larger messages to FastMock, an MQTT 3.1.1 connection that receives one reconnects |
| AMQP capture queue | `x-max-length` 10000 at the broker |
| list page | 1 to 500 |
| find, wait, verify | 10,000 stored messages looked at and 64 MiB of bodies loaded, counting only the bodies of messages that meet the other conditions (`422` beyond; narrow with `space`, `since`, or conditions on source, topic or headers) |
| concurrent find, wait, verify | 8 per replica, at most 2 of them per space (or, for reads across spaces, per caller); a request waits up to 5 s for a slot, then `503` |
| Avro bodies | decoded only for JSON body matchers (`equalToJson`, `jsonPath`, `jsonSchema`); at most 16 schema registry requests per find, wait or verify; a failed lookup is answered from memory for 30 s; a value that declares more than the message size limit (at least 64 KiB) is not decoded |
| wait | 1 to 500 messages, 60 s |

Verification reports near misses from the messages it looked at. Body conditions are checked only on messages that
meet the other conditions, so a near miss that fails, say, `topic` lists `topic` alone, and its score counts its
body conditions as failed.

## Metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_capture_messages_total` | counter | `protocol`, `result` = `stored`, `truncated`, `dropped`, `evicted` | Captured messages (`truncated` also counts as `stored`; `dropped`: the buffer was full or storing failed; `evicted`: removed to stay within the byte budgets or the per-source count). |
| `fastmock_capture_leader` | gauge | | `1` on the replica that runs the consumers. |
| `fastmock_capture_sources_active` | gauge | | Consumers running on the leader. |
| `fastmock_capture_waits_total` | counter | `result` = `found`, `timeout` | Finished waits. |
| `fastmock_capture_waits_active` | gauge | | Waits in progress. |
| `fastmock_admin_verifications_total` | counter | `kind="capture"`, `result` | Verifications. |
| `fastmock_store_errors_total` | counter | `store` = `capture`, `capture_sources`, `capture_lease` | Storage errors. |

Logs: `capture consumer started`, `Kafka capture subscribed`, `AMQP capture subscribed`, `MQTT connected`, and
warnings when a consumer is interrupted (`... capture interrupted`, with `retry_in`).

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `state: stopped` for a long time | No replica holds the capture lease: check Valkey, and `fastmock_capture_leader`. |
| `state: error`, `FASTMOCK_KAFKA_BOOTSTRAP is not configured` | Configure the broker the source reads. |
| `state: error`, `exchange billing does not exist` | Create the exchange; FastMock never declares exchanges for capture. |
| `state: error`, `the broker refused the subscription to ...` | The MQTT broker's ACL denies the filter to FastMock's user. |
| `state: error`, `Kafka topic ... does not exist` | Create the topic. |
| `wait` answers `408` although the message is listed | It arrived before the wait started: pass `since` or `includeExisting: true`. |
| `PUT /capture/sources/...` answers `400 capture sources created through the API need FASTMOCK_CAPTURE_..._TOPICS` | Set the allow-list of the protocol (`*` allows every name). |
| find, wait or verify answers `422 the messages to check hold more than 64 MiB of bodies` | Add conditions on source, topic or headers, or `since`: only the bodies of messages that meet them are read. |
| Old messages disappear before `FASTMOCK_CAPTURE_MAX_AGE` | The byte budgets are reached; raise `FASTMOCK_CAPTURE_MAX_BYTES` or `FASTMOCK_CAPTURE_MAX_SPACE_BYTES`, or delete messages between tests. |
| A Kafka message published right after creating the source is missing | The source starts at the latest offsets once it runs (`state: running`, within about 5 s); wait for it, or use `startFrom: earliest`. |
| `body.encoding` is `base64` with `error` for Avro | Set `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL`, or the body was truncated. |
| `dropped` grows | The broker delivers faster than the store writes; reduce the topics or raise the database capacity. |
