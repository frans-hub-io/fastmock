# MQTT and AMQP messages

A stub can publish MQTT messages and AMQP 0-9-1 messages (RabbitMQ, LavinMQ and other AMQP 0-9-1 brokers) when it
matches, in the same way as [Kafka messages](kafka.md): the event a real service would emit after an HTTP call.
[Async mocks](asyncapi.md) publish to MQTT and AMQP too, on a timer, and the [AsyncAPI import](asyncapi.md#importing-a-document)
creates them from `mqtt` and `amqp` bindings. To check what *your application* publishes to a broker, see
[Message capture](message-capture.md).

- [Server settings](#server-settings)
- [Publishing from stubs](#publishing-from-stubs)
- [MQTT messages](#mqtt-messages)
- [AMQP messages](#amqp-messages)
- [Connections, reconnects and readiness](#connections-reconnects-and-readiness)
- [Allow-lists](#allow-lists)
- [TLS and authentication](#tls-and-authentication)
- [Observability](#observability)
- [Troubleshooting](#troubleshooting)

## Server settings

MQTT and AMQP are off until their broker URL is set. Each protocol has one broker; FastMock never needs
credentials in the URL.

| Variable | Default | Meaning |
|---|---|---|
| `FASTMOCK_MQTT_URL` | none | `mqtt://host:1883` or `mqtts://host:8883` (also `tcp://`, `ssl://`, `tls://`). |
| `FASTMOCK_MQTT_VERSION` | `5` | `5` or `3.1.1`. With 3.1.1, user properties, content type, correlation data, response topic and expiry are not sent. |
| `FASTMOCK_MQTT_CLIENT_ID_PREFIX` | `fastmock` | Client ids are `<prefix>-pub-<8 hex digits>` for publishing and `<prefix>-cap-<8 hex digits>` for capture, new for every connection, so replicas never take each other's session. Match it in broker ACLs that restrict client ids. |
| `FASTMOCK_MQTT_USERNAME`, `FASTMOCK_MQTT_PASSWORD` | none | Credentials (a user name alone is allowed for MQTT). |
| `FASTMOCK_MQTT_TLS_CA_FILE`, `_TLS_CERT_FILE`, `_TLS_KEY_FILE` | none | See [TLS and authentication](#tls-and-authentication); only with `mqtts://`. |
| `FASTMOCK_MQTT_KEEP_ALIVE` | `30s` | MQTT keep-alive (at least 5 s). |
| `FASTMOCK_MQTT_ALLOWED_TOPICS` | none (any) | Topics stubs and async mocks may publish to: exact names or prefixes ending in `*` (`devices/*`). |
| `FASTMOCK_AMQP_URL` | none | `amqp://host:5672/vhost` or `amqps://host:5671/vhost`. The virtual host is the URL-encoded path (`/%2f` or no path for `/`, `/orders` for `orders`). |
| `FASTMOCK_AMQP_USERNAME`, `FASTMOCK_AMQP_PASSWORD` | none (`guest`) | Credentials for SASL `PLAIN`. Set both or neither. |
| `FASTMOCK_AMQP_SASL_MECHANISM` | `PLAIN` | `PLAIN` or `EXTERNAL` (the broker takes the identity from the client certificate; needs `amqps://` and a client certificate, no password). |
| `FASTMOCK_AMQP_TLS_CA_FILE`, `_TLS_CERT_FILE`, `_TLS_KEY_FILE` | none | Only with `amqps://`. |
| `FASTMOCK_AMQP_ALLOWED_EXCHANGES` | none (any) | Exchanges stubs and async mocks may publish to: names or `prefix*`. The default exchange routes to the queue named by the routing key, so it is allowed through `amq.default/<queue>` entries (`amq.default/orders.*`); `amq.default` alone allows no queue. |
| `FASTMOCK_AMQP_DECLARE_EXCHANGES` | `false` | Declare a missing exchange before the first message to it, with the message's `exchangeType` (default `topic`), durable. Only exchange names written in the stub are declared, never names produced by a template. Existing exchanges are never redeclared. The user needs the `configure` permission. |

Settings are checked at start-up: a URL with credentials, a query or an unknown scheme, a password without a user
name (AMQP), a certificate without its key, TLS files with a plain `mqtt://` or `amqp://` URL, `EXTERNAL` without
a client certificate, or `FASTMOCK_MQTT_*`/`FASTMOCK_AMQP_*` settings without the URL stop the server with a message
naming the variable. Passwords never appear in logs, errors or `/__admin/info`. In Helm these are the `mqtt.*` and
`amqp.*` values ([Helm](../operations/helm.md#values)).

```bash
FASTMOCK_MQTT_URL=mqtts://mqtt.example.com:8883
FASTMOCK_MQTT_USERNAME=fastmock
FASTMOCK_MQTT_PASSWORD=<from a Secret>
FASTMOCK_MQTT_ALLOWED_TOPICS='devices/*,alerts/*'
FASTMOCK_AMQP_URL=amqps://rabbit.example.com/orders
FASTMOCK_AMQP_USERNAME=fastmock
FASTMOCK_AMQP_PASSWORD=<from a Secret>
FASTMOCK_AMQP_TLS_CA_FILE=/etc/fastmock/amqp/ca.crt
FASTMOCK_AMQP_ALLOWED_EXCHANGES='orders,audit.*'
```

## Publishing from stubs

The `messages` of a stub take `protocol`: `kafka` (the default, see [Kafka messages](kafka.md)), `mqtt` or `amqp`.
One stub can publish to several brokers. The template context is the same as for the response.

```json
{
  "name": "ship order",
  "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/ship"},
  "response": {"status": 202, "jsonBody": {"id": "{{request.pathParams.id}}", "status": "SHIPPING"}},
  "messages": [
    {
      "protocol": "mqtt",
      "topic": "orders/{{request.pathParams.id}}/status",
      "qos": 1,
      "retain": true,
      "contentType": "application/json",
      "correlationData": "{{request.headers.x-request-id}}",
      "headers": {"event-type": "OrderShipped"},
      "jsonValue": {"orderId": "{{request.pathParams.id}}", "status": "SHIPPED", "at": "{{now}}"}
    },
    {
      "protocol": "amqp",
      "exchange": "orders",
      "routingKey": "order.shipped",
      "contentType": "application/json",
      "messageId": "{{uuid}}",
      "correlationId": "{{request.headers.x-request-id}}",
      "persistent": true,
      "headers": {"event-type": "OrderShipped"},
      "jsonValue": {"orderId": "{{request.pathParams.id}}", "carrier": "{{request.json.carrier}}"},
      "delayMs": 500
    }
  ]
}
```

Fields for every protocol:

| Field | Type | Meaning |
|---|---|---|
| `protocol` | `kafka`, `mqtt`, `amqp` | The broker; `kafka` when absent. |
| `jsonValue` | JSON with templates | JSON body. A string that is exactly `"{{json expr}}"` keeps the JSON type of the value. |
| `value` | template | Text body, used when `jsonValue` is not set. Without both, the body is empty. |
| `headers` | map of templates | Kafka headers, MQTT 5 user properties, AMQP headers (long strings). At most 100 per message. |
| `contentType` | template | MQTT 5 content type property, AMQP `content_type`; for Kafka the `content-type` header unless `headers` sets it. |
| `delayMs` | integer | Publish this long after the match, at most `FASTMOCK_MAX_DELAY` and one hour. Messages of a stub are sent in order of their delay; each delay counts from the match. |

A stub has at most 100 messages. All templated fields of all messages of one response render together within one
budget: 16 MiB of output and the time, step and memory limits of one template. A field a protocol limits stops
rendering as soon as it is too long and fails the message: AMQP exchange, routing key, `messageId`,
`correlationId`, `replyTo`, `expiration`, `messageType`, `appId` and `contentType` at 255 bytes, a Kafka topic at
249 bytes, MQTT topics, properties and user properties at 65535 bytes (`messages[0].routingKey: the rendered value
exceeds 255 bytes`). The response itself is not affected.

The HTTP response is not delayed by publishing and does not fail when publishing fails: publishing runs in the
background, the outcome is logged and counted. Messages are published at most once, never retried; a message whose
broker is unreachable is lost and counted as `failed`. Each replica publishes at most 256 messages at a time;
messages due now beyond that are dropped and counted as `dropped`. Delayed messages wait in a separate queue and
take a publication slot only when they are due: at most 4096 messages and 64 MiB wait per replica, at most 1024 of
them from one stub; messages beyond that are dropped and counted as `dropped`, and a due message that finds no free
slot within 10 seconds too. Messages that do not fit the protocol (an MQTT `key`, an AMQP
`topic`, `qos` for Kafka, `avroSchema` outside Kafka) are rejected when the stub is saved:
`messages[0]: key not used with protocol mqtt`.

## MQTT messages

| Field | Type | Meaning |
|---|---|---|
| `topic` | template, required | Topic name; no `+` or `#`, no control characters. |
| `qos` | `0`, `1`, `2` | Quality of service, default `0`. |
| `retain` | boolean | The broker keeps the message as the topic's retained message. |
| `correlationData` | template | MQTT 5 correlation data (UTF-8 bytes). |
| `responseTopic` | template | MQTT 5 response topic: a topic name like `topic` (no `+` or `#`). |
| `messageExpirySeconds` | integer | MQTT 5 message expiry interval. |

Before a message is handed to the connection, its topic, response topic, content type and user properties are
checked against what brokers accept (no NUL or other control characters, no non-characters, at most 65535 bytes;
correlation data at most 65535 bytes): a bad value fails that message only instead of making the broker close the
connection that every publication shares. A publication counts as sent when the broker acknowledged it: on the write for QoS 0, on `PUBACK` for QoS 1, on
`PUBCOMP` for QoS 2. An MQTT 5 acknowledgement with an error reason (`NotAuthorized`, `TopicNameInvalid`, ...) fails
the message; `NoMatchingSubscribers` counts as sent. A publication waits at most 10 seconds for its acknowledgement.

Example with Mosquitto (`mosquitto_sub -V mqttv5 -t 'orders/#' -F '%t %p %P'` shows the topic, payload and user
properties):

```sh
curl -s -X POST localhost:8080/orders/A-1/ship -H 'content-type: application/json' \
  -H 'x-request-id: r-17' -d '{"carrier":"DHL"}'
```

```text
orders/A-1/status {"at":"2026-11-22T02:09:16.401Z","orderId":"A-1","status":"SHIPPED"} event-type:OrderShipped
```

## AMQP messages

| Field | Type | Meaning |
|---|---|---|
| `exchange` | template | Exchange; empty or `amq.default` is the default exchange, which routes to the queue named by the routing key. |
| `routingKey` | template | Routing key. A message needs `exchange`, `routingKey` or both. |
| `exchangeType` | `topic`, `direct`, `fanout`, `headers` | Type used when FastMock declares a missing exchange (`FASTMOCK_AMQP_DECLARE_EXCHANGES=true`); default `topic`. |
| `messageId`, `correlationId`, `replyTo`, `messageType`, `appId` | template | The AMQP basic properties `message_id`, `correlation_id`, `reply_to`, `type`, `app_id`. |
| `expiration` | template | Per-message TTL in milliseconds, digits only (`"60000"`). |
| `persistent` | boolean | Delivery mode 2 (persistent) instead of 1. |
| `priority` | 0..255 | Message priority. |

Every message also carries `timestamp` (seconds). FastMock publishes on one channel in confirm mode: a message
counts as sent when the broker confirms it, and fails on a negative confirm, when the exchange does not exist
(`exchange orders does not exist (create it, or set FASTMOCK_AMQP_DECLARE_EXCHANGES=true ...)`) or when no confirm
arrives within 10 seconds. Before the first message to an exchange FastMock checks on a separate channel that it
exists, so a missing exchange fails only the messages to it, never the confirms of other messages on the shared
channel. It remembers up to 256 exchanges as existing. A message routed to no queue is confirmed and dropped by the
broker, as for any AMQP producer.

## Connections, reconnects and readiness

Each replica opens one MQTT connection and one AMQP connection for publishing, on the first message, and keeps
them. An unset or unreachable broker never affects readiness (`/readyz`): stubs answer, their messages fail.

- **MQTT**: the connection reconnects by itself after an error, with a pause that doubles from 200 ms to 30 s and
  resets only after a connection that lasted at least 10 seconds, so a broker that accepts and then closes every
  connection is not retried at the shortest pause. It uses a clean session; publications in flight when the
  connection breaks fail and are not resent.
- **AMQP**: after a failure the connection is rebuilt by the next message, at most once per pause (200 ms doubling
  to 30 s); messages in between fail at once with
  `the AMQP broker is unavailable (...); the next connection attempt is in 1.6s`. A closed channel (for example
  after `NOT_FOUND`) is reopened on the next message.

TLS files are read again on every new connection; restart the pods after rotating a client certificate, as the
publishing connections stay open.

## Allow-lists

`FASTMOCK_MQTT_ALLOWED_TOPICS` and `FASTMOCK_AMQP_ALLOWED_EXCHANGES` work like `FASTMOCK_KAFKA_ALLOWED_TOPICS`:
unset allows everything; otherwise a message outside the list is not published and fails with
`topic is not in FASTMOCK_MQTT_ALLOWED_TOPICS` or `exchange audit is not in FASTMOCK_AMQP_ALLOWED_EXCHANGES`. The
stub still answers. For the AMQP default exchange (`amq.default` or an empty exchange) the routing key is the
destination queue, so the list needs `amq.default/<queue>` entries: `amq.default/replies.*` allows routing keys
starting with `replies.`; a plain `amq.default` entry allows none. The AsyncAPI import warns about async mocks outside the lists, and
`POST /__admin/async-mocks/{id}/trigger` answers `400`. Use them in shared brokers.

## TLS and authentication

TLS uses rustls (TLS 1.2 and 1.3) with the ring provider. The broker certificate is verified against the public
Mozilla roots compiled into the binary plus the certificates of the `*_TLS_CA_FILE` bundle; verification cannot be
switched off. The certificate must name the host of the URL. `*_TLS_CERT_FILE` (leaf first, then intermediates)
and `*_TLS_KEY_FILE` (unencrypted PEM) present a client certificate for mTLS; set both or neither. For `mqtts://`
the MQTT client talks to a Unix socket of the process and FastMock wraps that connection in TLS towards the broker.
On Linux the socket has a random name in the abstract namespace, so no file is written and a read-only root
filesystem needs no writable `/tmp`; the bridge accepts connections from the FastMock process only. On other Unix
systems the socket is a file in a private temporary directory (mode `0700`, removed with the connection), so
`TMPDIR` (else `/tmp`) must be writable there.

| Broker setup | Settings |
|---|---|
| MQTT with user and password | `FASTMOCK_MQTT_USERNAME`, `FASTMOCK_MQTT_PASSWORD` (use `mqtts://` outside test networks) |
| MQTT with a private CA | `mqtts://...`, `FASTMOCK_MQTT_TLS_CA_FILE` |
| MQTT with client certificates | plus `FASTMOCK_MQTT_TLS_CERT_FILE`, `FASTMOCK_MQTT_TLS_KEY_FILE` |
| RabbitMQ with user and password | `FASTMOCK_AMQP_USERNAME`, `FASTMOCK_AMQP_PASSWORD` |
| RabbitMQ over TLS | `amqps://...`, `FASTMOCK_AMQP_TLS_CA_FILE` |
| RabbitMQ with `rabbitmq_auth_mechanism_ssl` | `amqps://...`, the client certificate and `FASTMOCK_AMQP_SASL_MECHANISM=EXTERNAL` |

The same settings are used by [message capture](message-capture.md).

### SASL EXTERNAL with RabbitMQ

With `EXTERNAL`, RabbitMQ takes FastMock's user name from its client certificate, so no password is stored
anywhere. Publishing and capture both work this way (CI tests both against RabbitMQ 4.3). On the broker:

```ini
listeners.ssl.default = 5671
ssl_options.cacertfile = /etc/rabbitmq/tls/ca.pem
ssl_options.certfile = /etc/rabbitmq/tls/server.pem
ssl_options.keyfile = /etc/rabbitmq/tls/server.key
ssl_options.verify = verify_peer
auth_mechanisms.1 = PLAIN
auth_mechanisms.2 = EXTERNAL
ssl_cert_login_from = common_name
```

Enable the plugin (`rabbitmq-plugins enable rabbitmq_auth_mechanism_ssl`) and create a user named like the
certificate's common name, with the permissions of [capture](message-capture.md#how-each-broker-is-read) and
publishing; its password is never used. FastMock:

```sh
FASTMOCK_AMQP_URL=amqps://rabbitmq.example.com:5671/%2f
FASTMOCK_AMQP_SASL_MECHANISM=EXTERNAL
FASTMOCK_AMQP_TLS_CA_FILE=/etc/fastmock/amqp/ca.pem
FASTMOCK_AMQP_TLS_CERT_FILE=/etc/fastmock/amqp/client.pem
FASTMOCK_AMQP_TLS_KEY_FILE=/etc/fastmock/amqp/client.key
```

A certificate RabbitMQ does not trust shows `TLS handshake failed: the broker refused FastMock's client certificate`
in the `lastError` of capture sources; a certificate whose name is not a RabbitMQ user shows
`the broker refused the credentials`. With Helm, set `messaging.amqp.sasl.mechanism: EXTERNAL` and
`messaging.amqp.tls.clientCertSecret`.

## Observability

| Signal | Content |
|---|---|
| `fastmock_messages_total{protocol, result}` | Stub messages: `protocol` = `kafka`, `mqtt`, `amqp`; `result` = `sent`, `failed` (allow-list, broker or encoding errors), `dropped` (more than 256 publications of a replica in flight). |
| `fastmock_async_messages_total{mock, result}` | Async mock publications of every protocol. |
| log `message published` | `protocol`, `topic` (`exchange/routing key` for AMQP), for Kafka `partition` and `offset`. |
| log `message publishing failed` | `protocol`, `topic`, `error`. |
| logs `MQTT connected`, `MQTT connection lost; reconnecting`, `AMQP connected`, `AMQP connection failed` | Connection state, with the broker URL (never credentials). |

Errors returned through the admin API (`POST /__admin/async-mocks/{id}/trigger`, the `lastError` of capture sources)
name no broker address, virtual host, certificate detail or broker response: they say what failed, for example
`publishing to AMQP failed: broker connection refused (details in the server log)`, `TLS handshake failed` or
`schema registry returned 404`. Errors FastMock raises itself (allow-lists, invalid values, missing settings or
exchanges) keep their text. The server log has the full error.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `FASTMOCK_MQTT_URL is not configured`, `FASTMOCK_AMQP_URL is not configured` | Set the broker URL. |
| `Connection refused, return code: NotAuthorized` (MQTT) | Wrong user or password, or the broker requires them. |
| `invalid peer certificate: UnknownIssuer` | The broker's CA is missing from `*_TLS_CA_FILE`. |
| `the MQTT broker did not acknowledge the message within 10s` | The broker is overloaded or dropped the connection; QoS 0 avoids waiting for acknowledgements. |
| `the broker refused the message: NotAuthorized` | The broker's ACL denies the topic to FastMock's user. |
| `exchange orders does not exist` | Create the exchange, or set `FASTMOCK_AMQP_DECLARE_EXCHANGES=true` (not for exchange names produced by a template). |
| `the default exchange with this routing key is not in FASTMOCK_AMQP_ALLOWED_EXCHANGES` | Add `amq.default/<queue>` (or a `prefix*`) for the queues stubs may publish to. |
| `responseTopic ... is not a valid MQTT topic name` | The rendered response topic contains `+`, `#` or a control character. |
| `ACCESS_REFUSED` (AMQP) | The user lacks `write` on the exchange (or `configure` for declarations), or the virtual host is wrong (`/%2f` is `/`). |
| MQTT 5 properties are missing at the consumer | `FASTMOCK_MQTT_VERSION=3.1.1`, or the consumer uses MQTT 3.1.1. |
