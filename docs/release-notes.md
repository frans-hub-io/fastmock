# Release notes

## 0.1.0-dev15

Published as `franshub/fastmock:0.1.0-dev15` and `franshub/fastmock-operator:0.1.0-dev15` for `linux/amd64` and
`linux/arm64`. Helm charts: `fastmock` 0.3.0 and `fastmock-operator` 0.2.0. This is a development release.

### New

- **Per-space capture allow-lists.** A space in the access file can narrow what its message capture sources may
  read with `capture.kafkaTopics`, `capture.mqttTopics` and `capture.amqpExchanges`, so teams that share one broker
  login can each read only their own destinations. See
  [Message capture](guide/message-capture.md#allow-lists-per-space).
- **gRPC proxy and record.** A stub can forward gRPC calls (unary and streaming) to a real service, with optional
  client certificates for upstreams that require mTLS, and a recording can turn gRPC calls into stubs. See
  [gRPC](guide/grpc.md#proxying-and-recording) and [Recording](guide/recording.md#grpc-calls).
- **AMQP SASL EXTERNAL.** FastMock can authenticate to RabbitMQ with its client certificate instead of a password
  (`FASTMOCK_AMQP_SASL_MECHANISM=EXTERNAL`). See [MQTT and AMQP](guide/mqtt-amqp.md).
- **Java client capture.** The Java client lists and manages capture sources and finds, waits for and verifies
  captured messages. See [Java client](guide/java-client.md).
- **Avro over MQTT and AMQP.** Messages published to MQTT and AMQP can carry Avro payloads with a Schema Registry
  schema, as for Kafka, and capture decodes Avro bodies from all three protocols.
- **MQTT shared subscriptions.** The documentation explains how to test applications that read through
  `$share/...` subscriptions and why capture does not join a shared group.

### Hardening

- Proxied gRPC calls are bounded in number, memory and duration, and loops through FastMock proxies are stopped.
- The gRPC client certificate is presented only to the upstreams you list and only for proxy stubs of the global area.
- Capture ids are issued by the server, and capture sources are checked against the allow-lists of their space.
- Templated Avro destinations need an explicit subject, the schema cache is bounded and decoding limits the memory a
  message can request.
- The Java client no longer includes message payloads in error messages.

### Upgrading

See the [Helm upgrade notes](operations/helm.md#upgrade-notes). The release adds no database migration.
