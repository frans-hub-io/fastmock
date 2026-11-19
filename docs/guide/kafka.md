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
| `FASTMOCK_KAFKA_SECURITY_PROTOCOL` and the `FASTMOCK_KAFKA_SASL_*`, `FASTMOCK_KAFKA_TLS_*` variables | TLS and SASL, see [Secure connections](#secure-connections). |

The producer connects on first use and caches the partition count of each topic. Topics must exist; FastMock
does not create them. In Helm these are the `kafka.*` values ([Helm](../operations/helm.md#values)).

## Secure connections

By default FastMock connects over plain TCP without authentication. `FASTMOCK_KAFKA_SECURITY_PROTOCOL` takes the
Kafka names of the four combinations:

| Protocol | TLS | SASL | Also needs |
|---|---|---|---|
| `plaintext` (default) | no | no | nothing |
| `ssl` | yes | no | optionally `FASTMOCK_KAFKA_TLS_CA_FILE`, a client certificate for mTLS |
| `sasl_plaintext` | no | yes | `FASTMOCK_KAFKA_SASL_MECHANISM`, `_USERNAME`, `_PASSWORD` |
| `sasl_ssl` | yes | yes | both of the above |

The same settings are used for stub messages, [async mocks](asyncapi.md) and AsyncAPI
[contract tests](contract-testing.md#what-an-asyncapi-run-does).

- **TLS** (rustls, TLS 1.2 and 1.3): the broker certificate is verified against the public Mozilla roots compiled
  into the binary plus the certificates in `FASTMOCK_KAFKA_TLS_CA_FILE`; verification cannot be switched off. The
  certificate must name the host of each broker address as Kafka advertises it (DNS name or IP address).
- **mTLS**: `FASTMOCK_KAFKA_TLS_CERT_FILE` (leaf first, then intermediates) and `FASTMOCK_KAFKA_TLS_KEY_FILE`
  (unencrypted PEM key) present a client certificate. Set both or neither.
- **SASL**: `PLAIN`, `SCRAM-SHA-256` or `SCRAM-SHA-512`. Use `PLAIN` only over `sasl_ssl`; it sends the password
  as is. OAUTHBEARER and GSSAPI (Kerberos) are not supported.
- **Schema Registry**: `FASTMOCK_KAFKA_SCHEMA_REGISTRY_USERNAME` and `_PASSWORD` enable HTTP basic
  authentication; an `https://` registry also trusts `FASTMOCK_KAFKA_TLS_CA_FILE`. Use `https://` when the
  registry needs a password.

Settings are checked at start-up: an unknown protocol or mechanism, missing credentials, a certificate without
its key, an unreadable or empty PEM file, or SASL or TLS settings that the protocol does not use stop the server
with an error naming the variable. The one exception is `FASTMOCK_KAFKA_TLS_CA_FILE` with `plaintext` or
`sasl_plaintext`: it is accepted when the registry URL is `https://` and then used for the registry only.
Passwords never appear in logs, errors or `/__admin/info`. The certificate files are read again on every new
connection; the stub publisher keeps its connection, so restart the pods after rotating a client certificate.

SASL_SSL with SCRAM, passwords from a Kubernetes Secret:

```bash
FASTMOCK_KAFKA_BOOTSTRAP=kafka-0.example.com:9093,kafka-1.example.com:9093
FASTMOCK_KAFKA_SECURITY_PROTOCOL=SASL_SSL
FASTMOCK_KAFKA_SASL_MECHANISM=SCRAM-SHA-512
FASTMOCK_KAFKA_SASL_USERNAME=fastmock
FASTMOCK_KAFKA_SASL_PASSWORD=<from a Secret>
FASTMOCK_KAFKA_TLS_CA_FILE=/etc/fastmock/kafka/ca.crt
FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL=https://registry.example.com
FASTMOCK_KAFKA_SCHEMA_REGISTRY_USERNAME=fastmock
FASTMOCK_KAFKA_SCHEMA_REGISTRY_PASSWORD=<from a Secret>
```

mTLS without SASL (for example Strimzi's `tls` listener with `authentication: tls`):

```bash
FASTMOCK_KAFKA_BOOTSTRAP=kafka-bootstrap.example.com:9093
FASTMOCK_KAFKA_SECURITY_PROTOCOL=ssl
FASTMOCK_KAFKA_TLS_CA_FILE=/etc/fastmock/kafka/ca.crt
FASTMOCK_KAFKA_TLS_CERT_FILE=/etc/fastmock/kafka/user.crt
FASTMOCK_KAFKA_TLS_KEY_FILE=/etc/fastmock/kafka/user.key
```

A broker that rejects the connection does not produce an error right away: the client retries until the
connect timeout and the publication fails with `connecting to Kafka timed out`. The cause is in the warnings
`Failed to connect to broker` logged before it, for example `invalid peer certificate: UnknownIssuer` (the CA is
missing from `FASTMOCK_KAFKA_TLS_CA_FILE`), `received fatal alert: CertificateRequired` (the listener needs a
client certificate) or `SaslAuthenticationFailed` (wrong user, password or mechanism).

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
| `connecting to Kafka timed out` | Network, or a TLS or SASL error; see the preceding `Failed to connect to broker` warnings ([Secure connections](#secure-connections)). |
| `Kafka topic X does not exist` | Create the topic; FastMock does not create topics. |
| `avroSchema needs FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` | Configure the registry. |
| `schema registry returned 409 ...` | The schema is not compatible with the latest version of `<topic>-value` under the registry's compatibility rules. |
| `topic is not in FASTMOCK_KAFKA_ALLOWED_TOPICS` | Add the topic or a matching prefix. |

More in [Troubleshooting](../operations/troubleshooting.md#kafka-messages-are-not-published).
