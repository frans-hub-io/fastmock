# AsyncAPI and async mocks

FastMock imports AsyncAPI 2.x (2.0 to 2.6) and 3.0 documents, in JSON or YAML, and turns every operation that the
described application **publishes** into a mock that keeps publishing example messages:

- Kafka, MQTT and AMQP operations become **async mocks**: stored definitions that a background publisher sends to
  a Kafka topic, an MQTT topic or an AMQP exchange at a fixed frequency, round robin over their examples (Kafka
  values as JSON or Confluent Avro).
- WebSocket operations become **WebSocket stubs** (ordinary stubs with `response.websocket`) that stream the examples
  to every client that connects. See [WebSocket mocks](websocket.md).

This is the event-driven counterpart of the [OpenAPI import](importing.md#openapi-3) and covers the "async API mocking" feature
of Microcks. To check that a real producer respects the same contract, see [Contract testing](contract-testing.md).

- [Importing a document](#importing-a-document)
- [What is imported](#what-is-imported)
- [Examples and generated payloads](#examples-and-generated-payloads)
- [Avro](#avro)
- [The async mock document](#the-async-mock-document)
- [Managing async mocks](#managing-async-mocks)
- [Publishing and leader election](#publishing-and-leader-election)
- [Allow-lists](#allow-lists)
- [Metrics, traces and logs](#metrics-traces-and-logs)
- [Limits](#limits)
- [Errors](#errors)

The examples use `FASTMOCK_TOKEN` for an editor or admin token and the admin API on `localhost:9090`. Kafka
publishing needs `FASTMOCK_KAFKA_BOOTSTRAP` (and `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` for Avro), see
[Kafka messages](kafka.md); TLS and SASL are described in [Secure connections](kafka.md#secure-connections). MQTT
and AMQP publishing need `FASTMOCK_MQTT_URL` and `FASTMOCK_AMQP_URL`, see [MQTT and AMQP messages](mqtt-amqp.md).

## Importing a document

```sh
curl -X POST localhost:9090/__admin/import/asyncapi \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile spec streetlights.yaml '{spec: $spec, tag: "streetlights", frequencyMs: 5000}')"
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `spec` | string or object | required | The AsyncAPI document: a JSON object, or a JSON or YAML string. |
| `tag` | string | `asyncapi-<title slug>` | Tag put on every async mock and WebSocket stub created by the import. Re-importing with the same tag **replaces** the async mocks and the stubs that carry it. 1..128 characters. |
| `dryRun` | boolean | `false` | Parse and convert, return what would be created, store nothing. |
| `frequencyMs` | integer | `10000` | Publishing interval for every created mock, 1000..86400000. |
| `pathPrefix` | string | none | Prefix for the paths of WebSocket stubs, for example `/ws`. Must start with `/`. |
| `topicNaming` | string | `channel` | Destination names: `channel` (below) or `microcks` (`<service>-<version>-<channel>`, the Kafka topic, MQTT topic or AMQP exchange Microcks uses; see [Microcks topic names](#microcks-topic-names)). |

The response is `201 Created` (or `200 OK` for a dry run):

```json
{
  "dryRun": false,
  "tag": "streetlights",
  "asyncMocks": [
    {
      "id": "0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e01",
      "name": "sendLightMeasured",
      "protocol": "kafka",
      "channel": "smartylighting.streetlights.1.0.event.s1.lighting.measured",
      "operation": "sendLightMeasured",
      "service": "Streetlights Kafka API:1.0.0",
      "messages": [
        {"name": "example-1", "key": "s1", "payload": {"lumens": 7, "sentAt": "{{now}}"}}
      ],
      "frequencyMs": 5000,
      "enabled": true,
      "templated": true,
      "contentType": "application/json",
      "payloadSchema": {"type": "object", "properties": {"lumens": {"type": "integer", "minimum": 0}, "sentAt": {"type": "string", "format": "date-time"}}},
      "tags": ["streetlights"],
      "updatedAt": "2026-11-19T10:00:00Z"
    }
  ],
  "websocketStubs": [],
  "warnings": [
    "channel `smartylighting.streetlights.1.0.event.{streetlightId}.lighting.measured` was mapped to Kafka topic `smartylighting.streetlights.1.0.event.s1.lighting.measured`",
    "turnOn: `receive` (the application consumes); nothing to mock"
  ]
}
```

Warnings never fail the import; read them. A document that yields no async mock and no WebSocket stub (for example
only `publish` operations) is answered `400` and the mocks of the tag are left as they were. They explain skipped operations, renamed topics, topics outside the
allowlist, examples that are not valid templates (they are then sent verbatim, `templated: false`) and a missing
Kafka configuration.

## What is imported

FastMock mocks the side of the document that **sends** messages, because that is what a consumer under test needs:

| AsyncAPI | Mocked operation |
|---|---|
| 2.x | channels with a `subscribe` operation (the application publishes and clients subscribe) |
| 3.0 | operations with `"action": "send"` |

`publish` (2.x) and `receive` (3.0) operations are reported as warnings and skipped.

**Protocol.** An operation is mocked for Kafka when its operation, channel or message has a `kafka` binding, for
MQTT with an `mqtt` (or `mqtt5`) binding, for AMQP with an `amqp` binding and for WebSocket with a `ws` binding.
Without bindings, the protocols of the servers decide: the servers listed by the channel (`servers` in 2.2+ or 3.0),
otherwise all servers of the document. `kafka` and `kafka-secure` mean Kafka, `mqtt`, `mqtts`, `secure-mqtt` and
`mqtt5` mean MQTT, `amqp` and `amqps` mean AMQP 0-9-1, `ws` and `wss` mean WebSocket. Operations with none of them
(for example JMS, NATS, AMQP 1.0 or SQS only) are skipped with a warning
(`<operation>: no Kafka, MQTT, AMQP or WebSocket binding or server; skipped`). An operation that matches several
protocols produces one mock for each.

**Messages.** Each message of an operation becomes its own mock, so a 2.x `message.oneOf` with three messages, or a
3.0 operation with three `messages`, gives three mocks named `<operation> <message name>`. An operation with a single
message is named after the operation (`operationId` in 2.x, the operation key in 3.0). A 3.0 operation without
`messages` uses every message of its channel.

**Kafka topic.** `bindings.kafka.topic` of the channel wins. Otherwise the channel name (2.x) or `address` (3.0) is
used; channel parameters such as `{streetlightId}` are replaced with the parameter's first `examples` value, its
`default`, its first `enum` value, an example generated from its `schema`, or else its name. Characters that Kafka
does not allow in topic names (anything but `a-z A-Z 0-9 . _ -`) become `-`, and the result is cut to 249
characters, so `user/signedup` becomes `user-signedup`. Each such change is reported as a warning.

### Microcks topic names

Microcks publishes to `<service name without spaces and dashes>-<version without spaces>-<operation part>`, where the
operation part is the channel address with its parameters replaced and `/` turned into `-` (AsyncAPI 2.x and
parameterized 3.0 channels), or the operation key (other 3.0 operations); `bindings.kafka.topic` is not used.
For MQTT the same name is the topic, keeping the `/` of the channel (`StreetlightsAPI-1.0.0-lights/l7/measured`).
For AMQP it is the exchange, which Microcks declares non-durable with the binding's `exchange.type` (default `topic`):
the async mock gets `amqp.exchange` set to that name, `durable: false` and the type, and FastMock declares the
exchange when `FASTMOCK_AMQP_DECLARE_EXCHANGES=true` (the import warns when it is not set). The routing key stays the
channel address. Consumers configured for Microcks keep working when the import sets `"topicNaming": "microcks"`:

```sh
curl -X POST localhost:9090/__admin/import/asyncapi \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile spec streetlights.yaml '{spec: $spec, tag: "streetlights", topicNaming: "microcks"}')"
```

For the document above the topic becomes
`StreetlightsKafkaAPI-1.0.0-smartylighting.streetlights.1.0.event.s1.lighting.measured`. Parameters take the value
described under **Kafka topic** (Microcks takes it from each example message, so use one example value per
parameter). Characters Kafka does not allow become `-`, with a warning.

**MQTT topic.** The channel name (2.x) or `address` (3.0) with its parameters replaced as for Kafka topics; other
characters are kept, so `lights/{lightId}/measured` becomes `lights/l7/measured`. The operation binding gives the
options: `bindings.mqtt.qos` (default 0), `retain` and `messageExpiryInterval` (MQTT 5 expiry in seconds).

**AMQP exchange and routing key.** For a channel binding `bindings.amqp` with `is: routingKey` (the default), the
mock publishes to `exchange.name` with the channel address (parameters replaced) as routing key; `exchange.type` and
`exchange.durable` are used when FastMock declares a missing exchange (`FASTMOCK_AMQP_DECLARE_EXCHANGES=true`).
Without an exchange name the default exchange is used, with a warning. With `is: queue` the mock publishes through
the default exchange with `queue.name` (or the address) as routing key, so the message lands in that queue. The
operation binding gives `deliveryMode` (2 is persistent), `expiration` and `priority`; the message binding gives
`messageType` (the AMQP `type` property). Example headers become AMQP headers, and the content type the
`content_type` property.

**WebSocket path.** `pathPrefix` + the channel address. Parameters stay as path template parameters, so
`/live/{city}` matches `/live/kyiv` and `{{request.pathParams.city}}` is available in the examples.

**Key, headers and content type.** The Kafka key is an example of `bindings.kafka.key` of the message (MQTT and AMQP
messages have no key). Headers come from each example's `headers`, or from an example generated from the message
`headers` schema; they are Kafka headers, MQTT 5 user properties or AMQP headers. The content type is the message
`contentType` or the document `defaultContentType`; when set it is sent as the Kafka header `content-type` unless the
example sets that header itself, as the MQTT 5 content type, or as the AMQP `content_type` property.

**`$ref`** pointers inside the document (`#/components/...`, `#/channels/...`, `#/servers/...`) are resolved with
the same budgets as the OpenAPI import: recursion is cut, at most 200,000 nodes are expanded per operation, and the
resolved document may not exceed 64 MiB (counted while references are expanded). References to other files or URLs are not fetched; they resolve to an
empty schema.

## Examples and generated payloads

For every message, FastMock takes the first of these that exists:

1. The message `examples` (each item's `payload`, `headers` and `name`); up to 100 per message.
2. The payload schema's `examples` array, or its `example`.
3. A payload generated from the schema, as in the OpenAPI import: `enum`, `const`, `default` and `minimum` values are
   used, formats get typical values (`date-time` gives `2026-01-01T00:00:00Z`, `uuid` a fixed UUID, `email`
   `user@example.com`), objects get all their properties, arrays one item. Avro schemas get a value per field (see
   [Avro](#avro)).

Example payloads are **templates**: every string is rendered with the template engine before each publication, with
no request (async mocks run on a timer, not in reply to a request). The context is:

| Variable | Value |
|---|---|
| `mock.id`, `mock.name`, `mock.channel` | the async mock |
| `sequence` | a counter per mock, shared by all replicas, starting at 1 |
| `example` | the name of the example being sent |

All helpers work (`uuid`, `now`, `randomInt`, `randomDigits`, `pickRandom`, ...; see [Templating](templating.md)).
A string that is exactly `"{{json expr}}"` keeps the JSON type of the value:

```json
{"id": "{{uuid}}", "sentAt": "{{now}}", "seq": "{{json sequence}}", "lumens": "{{json (randomInt 0 1000)}}"}
```

## Avro

A message whose `schemaFormat` (2.x message, or 3.0 multi-format payload `{"schemaFormat": ..., "schema": ...}`)
starts with `application/vnd.apache.avro` (for example `application/vnd.apache.avro;version=1.9.0` or
`application/vnd.apache.avro+json;version=1.9.0`) is published as Confluent-framed Avro, exactly like a stub
[message with `avroSchema`](kafka.md#avro): the schema is registered under the subject `<topic>-value` in
`FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL`, and each JSON example is converted to Avro with the same rules (unions, logical
types, defaults, named types). The schema may be inline JSON or a JSON string.

The same holds for `mqtt` and `amqp` channels (see [Avro payloads](mqtt-amqp.md#avro-payloads)): the import keeps
the schema, and the subject is `<channel>-value` for MQTT and `<exchange>.<channel>-value` for AMQP (`<channel>-value`
on the default exchange), with characters such as `/` percent-encoded in the registry request.

Generated Avro examples use the field `default` when there is one, `0` for numbers, `"string"` for strings, the
first enum symbol, one item for arrays, `{"key": ...}` for maps, the first non-null branch of unions,
`"2026-01-01"` for `date`, `"2026-01-01T00:00:00Z"` for timestamps and `"0"` for decimals. A named type that
refers to itself is expanded once; the inner reference becomes `null`. One generated example has at most 10,000
values and 1 MiB.

## The async mock document

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | UUID | generated | Taken from the URL on `PUT`. |
| `name` | string | required | 1..256 characters. Used in logs and traces. |
| `protocol` | `kafka`, `mqtt`, `amqp` | `kafka` | The broker. WebSocket mocks are stubs, not async mocks. |
| `channel` | string | required | Kafka topic (`a-z A-Z 0-9 . _ -`, at most 249 characters), MQTT topic (no `+` or `#`), or AMQP routing key (at most 255 bytes, may be empty). |
| `mqtt` | object | none | MQTT only: `qos` (0, 1, 2; default 0), `retain`, `messageExpirySeconds`. |
| `amqp` | object | none | AMQP only: `exchange` (empty or `amq.default`: the default exchange), `exchangeType` and `durable` (used to declare a missing exchange), `persistent`, `expiration` (ms, digits), `priority`, `messageType`. |
| `messages` | array | required | 1..100 examples: `{"name", "key", "headers", "payload"}`. `payload` is any JSON; a string is sent as raw text, other values as JSON. |
| `frequencyMs` | integer | `10000` | Interval between publications, 1000..86400000. One example is sent per interval, round robin. |
| `enabled` | boolean | `true` | Disabled mocks are kept but not published. |
| `templated` | boolean | `true` | Render `payload`, `key` and header values as templates. |
| `contentType` | string | none | Sent as the `content-type` header unless an example sets it. |
| `avroSchema` | object | none | Publish as Confluent Avro with this schema (Kafka, MQTT and AMQP, see [Avro](#avro)). |
| `payloadSchema` | object | none | JSON Schema of the payload, kept for [contract testing](contract-testing.md). |
| `operation`, `service` | string | none | Where the mock came from. |
| `tags` | array | none | At most 32 tags of up to 128 characters. |

Validation runs on import and on `PUT`: limits above, every template must compile, payloads at most 256 KiB, at most
64 headers per example (names up to 256 bytes, values up to 8 KiB), keys up to 4 KiB.

## Managing async mocks

All routes are under `/__admin` and need a token: `GET` needs `viewer`, everything else `editor`. Changes are written
to the audit log.

| Route | Purpose |
|---|---|
| `POST /import/asyncapi` | import a document (above) |
| `GET /async-mocks?tag=` | list async mocks, optionally by tag |
| `GET /async-mocks/{id}` | one async mock |
| `PUT /async-mocks/{id}` | create or replace an async mock |
| `DELETE /async-mocks/{id}` | delete it (`204`, or `404`) |
| `POST /async-mocks/{id}/trigger` | publish the next example **now**, on the replica that receives the call |

Create one by hand:

```sh
curl -X PUT localhost:9090/__admin/async-mocks/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e01 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "name": "order events",
  "channel": "orders.v1",
  "frequencyMs": 2000,
  "contentType": "application/json",
  "messages": [
    {"name": "created", "key": "order-{{sequence}}", "headers": {"x-event": "created"},
     "payload": {"orderId": "order-{{sequence}}", "status": "CREATED", "at": "{{now}}"}},
    {"name": "paid", "key": "order-{{sequence}}", "headers": {"x-event": "paid"},
     "payload": {"orderId": "order-{{sequence}}", "status": "PAID", "at": "{{now}}"}}
  ]
}'
```

Pause it, then publish one message on demand:

```sh
curl -X PUT localhost:9090/__admin/async-mocks/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e01 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(curl -s localhost:9090/__admin/async-mocks/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e01 \
        -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq '.enabled = false')"

curl -X POST localhost:9090/__admin/async-mocks/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e01/trigger \
  -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{"id": "0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e01", "protocol": "kafka", "topic": "orders.v1", "partition": 2, "offset": 118}
```

An MQTT or AMQP mock publishes the same way; `trigger` answers without `partition` and `offset`, and with `exchange`
for AMQP:

```sh
curl -X PUT localhost:9090/__admin/async-mocks/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e02 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "name": "lamp state", "protocol": "mqtt", "channel": "devices/lamp-1/state", "frequencyMs": 5000,
  "contentType": "application/json", "mqtt": {"qos": 1, "retain": true},
  "messages": [{"headers": {"seq": "{{sequence}}"}, "payload": {"on": true, "lumens": "{{json (randomInt 0 800)}}"}}]
}'

curl -X PUT localhost:9090/__admin/async-mocks/0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e03 \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "name": "invoices", "protocol": "amqp", "channel": "invoice.created", "frequencyMs": 10000,
  "amqp": {"exchange": "billing", "persistent": true, "messageType": "invoice.created"},
  "messages": [{"payload": {"invoiceId": "inv-{{sequence}}", "amount": "{{json (randomInt 10 500)}}"}}]
}'
```

```json
{"id": "0193a5c2-8f4e-7c1a-9d4e-2f6b1a7c9e03", "protocol": "amqp", "topic": "invoice.created", "exchange": "billing"}
```

`trigger` works for disabled mocks too, and continues the same round robin as the background publisher.

## Publishing and leader election

Every replica runs the publisher loop, but **exactly one replica publishes** at a time, so the number of replicas
never multiplies the message rate. The replicas elect the publisher with a lease in Valkey:

- The lease is the key `asyncmock:leader` (under `FASTMOCK_VALKEY_PREFIX`) holding the id of the leading replica,
  with a 15 second time-to-live.
- A replica becomes leader by creating the key only if it does not exist (`SET NX PX` semantics, through the same
  atomic compare-and-set script that scenarios use). The leader renews it every 5 seconds by an atomic
  compare-and-set that succeeds only while the key still holds its own id; followers try to take it every second.
- The leader publishes only while its **local** view of the lease is valid: it counts the 15 seconds from the moment
  before its last successful renewal, minus a 2 second safety margin. A leader that cannot reach Valkey therefore
  stops publishing before another replica can acquire the lease.
- On shutdown the leader releases the lease (it lets the key expire at once), so another replica takes over within
  about a second. If a replica dies without releasing, the lease expires within 15 seconds.
- The leader reloads the async mocks every 5 seconds; changes made through the admin API on any replica are
  picked up within that time. Each enabled mock is published once per `frequencyMs`, the first time right after
  the leader loads it.
- The round-robin position is the `sequence` counter in Valkey (`asyncmock:seq:<id>`, kept 30 days), so a new leader
  continues where the previous one stopped.

With `FASTMOCK_STORAGE=memory` the single instance is always the leader. The gauge `fastmock_async_leader` is `1` on
the leading replica and `0` elsewhere; alert if the sum over all replicas stays `0`.

Publishing is at-most-once per interval: a failed publication is logged and counted, not retried, and the next
example follows at the next interval. Up to 32 publications run at once per leader; when they are all busy (for
example Kafka is slow), due messages are skipped and counted as `dropped`.

## Allow-lists

Async mocks publish through the same clients as stub `messages`, so `FASTMOCK_KAFKA_ALLOWED_TOPICS`,
`FASTMOCK_MQTT_ALLOWED_TOPICS` and `FASTMOCK_AMQP_ALLOWED_EXCHANGES` apply (`name` or `prefix*`, comma-separated;
unset allows everything). On import, destinations outside the lists produce a warning; at run time publishing to
them fails and is counted as `failed`, and `trigger` answers `400`. Kafka topics must exist unless
`FASTMOCK_KAFKA_CREATE_TOPICS=true`; AMQP exchanges must exist unless `FASTMOCK_AMQP_DECLARE_EXCHANGES=true`. A mock
whose broker is not configured is stored but not published (logged once per protocol).

```sh
FASTMOCK_KAFKA_BOOTSTRAP=kafka-0.example.com:9092 \
FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL=http://schema-registry.example.com:8081 \
FASTMOCK_KAFKA_ALLOWED_TOPICS='orders.v1,smartylighting.*' \
fastmock serve
```

## Metrics, traces and logs

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_async_messages_total` | counter | `mock` (async mock id), `result` = `sent`, `failed`, `dropped` | Publications by async mocks, scheduled or triggered. Ids of stored mocks only, at most 1000. |
| `fastmock_async_leader` | gauge | none | `1` on the replica that holds the publisher lease. |
| `fastmock_async_mocks_active` | gauge | none | Enabled async mocks (Kafka, MQTT and AMQP) loaded by the leader. |
| `fastmock_store_errors_total` | counter | `store` = `asyncmocks`, `asyncmock_lease` | Storage errors while loading mocks or renewing the lease. |

Every publication has a span `fastmock.async_publish` (`otel.kind=producer`, `messaging.system` = `kafka`, `mqtt` or `rabbitmq`,
`messaging.destination.name`, `fastmock.async_mock`). Leadership changes are logged at `info`, failed publications
at `warn` with the topic and the error.

## Limits

| Limit | Value |
|---|---|
| Stored async mocks | 1000 |
| Examples per mock | 100 |
| Payload or Avro schema size | 256 KiB |
| Headers per example | 64 |
| `frequencyMs` | 1000 .. 86,400,000 |
| Operations or channels per document | 2000 |
| `$ref` expansion | 200,000 nodes per operation, 64 MiB in total |
| Generated Avro example | 10,000 values, 1 MiB |
| Concurrent publications per leader | 32 |

## Errors

| Status | When |
|---|---|
| `400` | The document is not AsyncAPI 2.x or 3.x, has no `channels` (2.x) or `operations` (3.0), exceeds a limit, is neither JSON nor YAML; invalid `frequencyMs`, `tag` or `pathPrefix`; an invalid async mock on `PUT`; storing would exceed 1000 mocks; `trigger` to a destination outside an allow-list or without the broker setting (`FASTMOCK_KAFKA_BOOTSTRAP`, `FASTMOCK_MQTT_URL`, `FASTMOCK_AMQP_URL`). The body is `{"error": "..."}`. |
| `401` / `403` | Missing token, or a viewer token on a route that changes something. |
| `404` | Unknown async mock id. |
| `502` | `trigger` could not publish (broker unreachable, unknown topic or exchange, schema registry error); the error names the cause, for example `publishing to AMQP failed: ... NOT_FOUND - no exchange 'billing' in vhost '/'`. |
