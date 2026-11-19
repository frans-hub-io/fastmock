# Kafka messages

A stub can publish Kafka records when it matches: the event a real service would emit after an HTTP call
(`OrderShipped` after `POST /orders/{id}/ship`). Values can be plain text, JSON, or Avro in the Confluent wire
format with automatic schema registration.

For mocking event-driven APIs described by AsyncAPI documents, see [AsyncAPI](asyncapi.md).

## Server settings

| Variable | Meaning |
|---|---|
| `FASTMOCK_KAFKA_BOOTSTRAP` | Bootstrap servers, comma-separated `host:port`. Required for publishing. |
| `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` | Confluent-compatible Schema Registry, required for Avro messages. |
| `FASTMOCK_KAFKA_ALLOWED_TOPICS` | Optional allow-list: exact topic names or prefixes ending in `*` (`orders.*`). Recommended in shared clusters. It controls publishing only. |
| `FASTMOCK_KAFKA_CONTRACT_TOPICS` | Topics that AsyncAPI [contract tests](contract-testing.md#what-an-asyncapi-run-does) may read; empty (the default) disables reading. |

The producer connects over plain TCP without SASL, on first use, and caches the partition count of each topic.
Topics must exist; FastMock does not create them. In Helm these are the `kafka.*` values
([Helm](../operations/helm.md#values)).

## Defining messages

```json
{
  "name": "ship order",
  "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/ship"},
  "response": {"status": 202, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "SHIPPING"}},
  "messages": [
    {
      "topic": "orders.events",
      "key": "{{request.pathParams.id}}",
      "headers": {"event-type": "OrderShipped"},
      "jsonValue": {"orderId": "{{request.pathParams.id}}", "carrier": "{{request.json.carrier}}", "at": "{{now}}"}
    }
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `topic` | template, required | Topic name. |
| `key` | template | Record key (UTF-8). Records with a key go to the partition chosen by the Java client's default partitioner (murmur2 of the key), so ordering per key matches a Java producer. Without a key the partition is random. |
| `jsonValue` | JSON with templates | JSON value. A string that is exactly `"{{json expr}}"` keeps the JSON type of the value. |
| `value` | template | Text value, used when `jsonValue` is not set. Without both, the record has an empty value. |
| `headers` | map of templates | Record headers (UTF-8 values). |
| `avroSchema` | JSON | Avro schema; the value (from `jsonValue`) is encoded as Avro. See [Avro](#avro). |

A stub may publish several messages, to different topics. The template context is the same as for the response,
including scenario `state` (see [Templating](templating.md)).

Publishing runs in the background after the stub matched: the HTTP response is not delayed by Kafka and does not
fail when publishing fails. The outcome is logged and counted in `fastmock_messages_total`. Messages from SSE and
gRPC stubs are published the same way. Unlike [callbacks](callbacks.md), messages are not stored in an outbox and
are not retried: when the broker is unreachable the message is lost and counted as `failed`.

Example run (real output of `kafka-console-consumer.sh --property print.key=true --property print.headers=true`):

```sh
curl -s -X POST localhost:8080/orders/A-1/ship -H 'content-type: application/json' -d '{"carrier":"DHL"}'
```

```text
{"id":"A-1","status":"SHIPPING"}
```

```text
event-type:OrderShipped	A-1	{"at":"2026-11-19T05:52:09.394Z","carrier":"DHL","orderId":"A-1"}
```

## Avro

With `avroSchema`, FastMock:

1. registers the schema in the Schema Registry under the subject `<topic>-value` (the topic name strategy) and
   caches the returned id;
2. converts `jsonValue` to Avro according to the schema;
3. writes the Confluent wire format: byte `0`, the 4-byte schema id, then the Avro binary encoding.

Consumers that use the Confluent `KafkaAvroDeserializer` read these records without changes.

```json
{
  "topic": "orders.avro",
  "key": "{{request.pathParams.id}}",
  "avroSchema": {
    "type": "record", "name": "OrderShipped", "namespace": "com.example.orders",
    "fields": [
      {"name": "orderId", "type": "string"},
      {"name": "carrier", "type": "string"},
      {"name": "weightKg", "type": ["null", "double"], "default": null}
    ]
  },
  "jsonValue": {
    "orderId": "{{request.pathParams.id}}",
    "carrier": "{{request.json.carrier}}",
    "weightKg": "{{json request.json.weightKg}}"
  }
}
```

For `{"carrier":"DHL","weightKg":1.5}` this produced the record bytes
`00 00 00 00 06 06 41 2d 31 06 44 48 4c 02 00 00 00 00 00 00 f8 3f` (schema id 6, then `"A-1"`, `"DHL"`, union
branch 1, double 1.5).

### JSON to Avro conversion

| Avro type | Accepted JSON |
|---|---|
| `null`, `boolean`, `string` | the same JSON type |
| `int`, `long`, `float`, `double` | number |
| `bytes`, `fixed` | string (its UTF-8 bytes; for `fixed` the length must match) |
| `enum` | string that is one of the symbols |
| `array`, `map` | array, object |
| `record` | object; missing fields take the schema default, a missing field without default is an error |
| union | the plain value (not the `{"type": value}` wrapping); the first branch the value fits is used |
| `uuid` | string |
| `decimal` | number or string, at most `scale` fractional digits (`"12.50"` is safer than `12.5` for money) |
| `date` | `"YYYY-MM-DD"` or days since 1970-01-01 |
| `time-millis`, `time-micros` | number |
| `timestamp-*`, `local-timestamp-*` | epoch number in the unit of the type, or an RFC 3339 string such as `"{{now}}"` |

A value that does not fit the schema fails the message with a path to the field, for example
`$.weightKg: value "heavy" matches no branch of the union`, in the log line `message publishing failed`.

## Topic allow-list

With `FASTMOCK_KAFKA_ALLOWED_TOPICS=orders.*`, a message to `audit.events` is not published:

```json
{"level":"ERROR","message":"message publishing failed","topic":"audit.not-allowed","error":"topic is not in FASTMOCK_KAFKA_ALLOWED_TOPICS"}
```

The stub still answers; only the message is refused.

## Observability

| Signal | Content |
|---|---|
| `fastmock_messages_total{result}` | `sent`, `failed` (encoding, registry, broker or allow-list errors), `dropped` (more than 256 publications of a replica in flight) |
| log `message published` | `topic`, `partition`, `offset`, inside the span of the request (with its `trace_id`) |
| log `message publishing failed` | `topic`, `error` |

Example:

```json
{"level":"INFO","message":"message published","topic":"orders.events","partition":0,"offset":0,"span":{"fastmock.stub_id":"20e363c3-ab06-4c2e-b1d5-5d39a3739653","trace_id":"74cf475f875a2a5ac559937ca032c427","url.path":"/orders/A-1/ship"}}
```

## Troubleshooting

| Symptom | Cause |
|---|---|
| `FASTMOCK_KAFKA_BOOTSTRAP is not configured` | Set the bootstrap servers. |
| `Kafka topic X does not exist` | Create the topic; FastMock does not create topics. |
| `avroSchema needs FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` | Configure the registry. |
| `schema registry returned 409 ...` | The schema is not compatible with the latest version of `<topic>-value` under the registry's compatibility rules. |
| `topic is not in FASTMOCK_KAFKA_ALLOWED_TOPICS` | Add the topic or a matching prefix. |

More in [Troubleshooting](../operations/troubleshooting.md#kafka-messages-are-not-published).
