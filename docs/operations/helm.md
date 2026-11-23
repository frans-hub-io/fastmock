# Helm chart

`deploy/helm/fastmock` installs one FastMock instance: a Deployment, separate Services for the data and admin
ports (and for the mail ports when [mail](#mail) is on), and optionally PostgreSQL, Valkey and Kafka from maintained dependency charts, Gateway API routes, a
PodDisruptionBudget, an HPA, NetworkPolicies, a ServiceMonitor and a PrometheusRule. It needs Kubernetes 1.30 or
newer.

| Dependency | Chart | Enabled by | Use it for |
|---|---|---|---|
| PostgreSQL, single instance | [`postgres`](https://github.com/cloudpirates-io/helm-charts/tree/main/charts/postgres) from CloudPirates, official `postgres` image | `postgresql.enabled` | test environments without a managed database |
| PostgreSQL cluster | [`cluster`](https://github.com/cloudnative-pg/charts/tree/main/charts/cluster) from CloudNativePG | `cnpg.enabled` | replicas, failover and backups through the CloudNativePG operator |
| Valkey | [`valkey`](https://github.com/valkey-io/valkey-helm) from the Valkey project | `valkey.enabled` | a Valkey next to the release |
| Apache Kafka, single broker | [`kafka`](https://github.com/cloudpirates-io/helm-charts/tree/main/charts/kafka) from CloudPirates, official `apache/kafka` image, KRaft, SASL/PLAIN | `kafka.enabled` | testing stubs that publish to Kafka without a shared cluster |

With none of them enabled, FastMock connects to an external PostgreSQL, Valkey and Kafka, over TLS and with
authentication when configured (see [Secure connections](#secure-connections)). The values are validated against
`values.schema.json`: unknown keys, wrong types and unsupported modes fail `helm install` with the offending key.
When installing from a checkout, fetch the dependencies first:

```sh
helm repo add cnpg https://cloudnative-pg.github.io/charts
helm repo add valkey https://valkey.io/valkey-helm/
helm dependency build deploy/helm/fastmock
```

For many instances managed declaratively, see the [operator](operator.md) instead.

- [Install](#install)
- [Upgrade and uninstall](#upgrade-and-uninstall)
- [Values](#values)
- [Production checklist](#production-checklist)
- [Exposing the admin UI with Gateway API](#exposing-the-admin-ui-with-gateway-api)
- [Secure connections](#secure-connections)
- [TLS and mTLS](#tls-and-mtls)
- [NetworkPolicy](#networkpolicy)
- [Mail](#mail)
- [Upgrade notes](#upgrade-notes)

## Install

The chart refuses to render without an admin token Secret, and in `postgres` mode without database and Valkey
connection settings.

**1. Admin tokens.** One `name:role:sha256hex` entry per line, under the key `tokens`:

```sh
fastmock hash-token --generate          # or: docker run --rm fastmock:0.1.0 hash-token --generate
CI_HASH=$(fastmock hash-token "$CI_TOKEN")   # an existing token: prints only the hex hash
kubectl -n mocks create secret generic fastmock-admin-tokens \
  --from-literal=tokens="$(printf 'ci:editor:%s\nops:admin:%s\ngrafana:viewer:%s\n' "$CI_HASH" "$OPS_HASH" "$VIEW_HASH")"
```

**2a. External PostgreSQL and Valkey (recommended).** Put the connection URLs into a Secret:

```sh
kubectl -n mocks create secret generic fastmock-storage \
  --from-literal=database-url='postgres://fastmock:<password>@db.example.com:5432/fastmock?sslmode=verify-full' \
  --from-literal=valkey-url='redis://:<password>@valkey.example.com:6379/'
```

```sh
helm upgrade --install fastmock deploy/helm/fastmock -n mocks \
  --set image.repository=registry.example.com/platform/fastmock --set image.tag=0.1.0 \
  --set admin.tokensSecret.name=fastmock-admin-tokens \
  --set storage.database.existingSecret=fastmock-storage \
  --set storage.valkey.existingSecret=fastmock-storage
```

Percent-encode special characters in passwords inside URLs.

**2b. PostgreSQL 18 and Valkey 9 with the release** (single instances, for test environments that do not have
managed databases). One Secret holds the superuser password (`postgres-password`) and the password of the
`fastmock` user (`password`), another the Valkey password:

```sh
kubectl -n mocks create secret generic fastmock-postgresql \
  --from-literal=postgres-password="$(openssl rand -hex 24)" --from-literal=password="$(openssl rand -hex 24)"
kubectl -n mocks create secret generic fastmock-valkey --from-literal=password="$(openssl rand -hex 24)"
helm upgrade --install fastmock deploy/helm/fastmock -n mocks \
  --set admin.tokensSecret.name=fastmock-admin-tokens \
  --set postgresql.enabled=true \
  --set postgresql.auth.existingSecret=fastmock-postgresql \
  --set postgresql.customUser.existingSecret=fastmock-postgresql \
  --set valkey.enabled=true --set valkey.auth.usersExistingSecret=fastmock-valkey
```

The passwords reach FastMock through environment variables from the Secrets (`FASTMOCK_DATABASE_PASSWORD`,
`FASTMOCK_VALKEY_PASSWORD`), never through command lines or ConfigMaps. With `networkPolicy.storage: true`
(default) only FastMock pods may connect to them.

**2c. PostgreSQL through CloudNativePG** (replicas, automatic failover, backups). Install the
[CloudNativePG operator](https://cloudnative-pg.io/documentation/current/installation_upgrade/) in the cluster
once; the chart refuses `cnpg.enabled` without it. The chart then creates a `Cluster` named `<release>-cnpg` with
a `fastmock` database, and FastMock reads the connection URL from the `<release>-cnpg-app` Secret that the
operator generates:

```sh
helm upgrade --install fastmock deploy/helm/fastmock -n mocks \
  --set admin.tokensSecret.name=fastmock-admin-tokens \
  --set cnpg.enabled=true --set cnpg.cluster.instances=3 \
  --set valkey.enabled=true --set valkey.auth.usersExistingSecret=fastmock-valkey
```

Everything under `cnpg` is passed to the CloudNativePG `cluster` chart: backups to object storage
(`cnpg.backups`), storage class, PostgreSQL parameters, monitoring. See its
[values](https://github.com/cloudnative-pg/charts/blob/main/charts/cluster/values.yaml).

**2d. Memory mode** (one replica, nothing persisted):

```sh
helm upgrade --install fastmock deploy/helm/fastmock -n mocks \
  --set admin.tokensSecret.name=fastmock-admin-tokens \
  --set storage.mode=memory --set replicaCount=1 --set pdb.enabled=false
```

**3. Check it:**

```sh
kubectl -n mocks rollout status deploy/fastmock
kubectl -n mocks port-forward svc/fastmock-admin 9090:9090 &
curl -s localhost:9090/readyz
```

Resource names use the release name when it contains `fastmock`, otherwise `<release>-fastmock`; below this is
called `<fullname>`. The Services are `<fullname>` (data, port `service.port`) and `<fullname>-admin` (admin, port
`service.adminPort`); with the release name `fastmock` they are `fastmock` and `fastmock-admin`. Inside the
cluster tests use `http://fastmock.mocks.svc:8080` and `http://fastmock-admin.mocks.svc:9090`.

## Upgrade and uninstall

```sh
helm upgrade fastmock deploy/helm/fastmock -n mocks -f values.yaml
helm uninstall fastmock -n mocks
```

- Rolling updates use `maxUnavailable: 0`, `maxSurge: 1`. A pod that is being stopped withdraws readiness at
  once, keeps serving for `shutdownDelay` (5 s) while endpoints are updated, then drains open connections;
  `terminationGracePeriodSeconds` (45 s) must stay larger than `shutdownDelay` plus 20 s of draining.
- Configuration changes roll the pods automatically (the ConfigMap checksum is a pod annotation). Changing only
  a Secret (for example the admin tokens) does not: run `kubectl -n mocks rollout restart deploy/fastmock`.
- Database migrations run at start-up and are additive; the first new pod migrates, old pods keep working. See
  [Database](database.md#how-migrations-run), also for running them as a separate Job.
- `helm uninstall` keeps the PostgreSQL PersistentVolumeClaim (`data-<release>-postgresql-0`) and the
  CloudNativePG volumes until you delete them. The Valkey PersistentVolumeClaim (`<release>-valkey`) is part of the
  release and is deleted with it; Valkey holds only counters and caches that FastMock rebuilds.

## Values

### Image and pods

| Key | Default | Meaning |
|---|---|---|
| `image.repository` | `fastmock` | Server image repository. |
| `image.tag` | `""` (chart `appVersion`) | Image tag. Pin a version or digest in production. |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | List of `{name: ...}` for the FastMock pods. |
| `replicaCount` | `2` | Replicas when autoscaling is off. Use 1 with `storage.mode: memory`. |
| `startupProbe.periodSeconds`, `.failureThreshold` | `2`, `60` | Start-up allowance (120 s); raise it with longer `auto` migrations. |
| `resources` | requests `100m`/`64Mi`, limits `2`/`512Mi` | Server container resources. |
| `podAnnotations`, `podLabels` | `{}` | Extra pod metadata. |
| `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints` | empty | Scheduling. |
| `podSecurityContext` | non-root 65532, `fsGroup` 65532, `RuntimeDefault` seccomp | Pod security context. |
| `securityContext` | non-root, no privilege escalation, read-only root FS, all capabilities dropped | Container security context. |
| `serviceAccount.create` | `true` | Create a dedicated ServiceAccount (its token is not mounted). |
| `serviceAccount.name` | `""` | Name to create or use; required when `create` is false. |
| `terminationGracePeriodSeconds` | `45` | |
| `shutdownDelay` | `5s` | `FASTMOCK_SHUTDOWN_DELAY`. |
| `logFormat` | `json` | `FASTMOCK_LOG_FORMAT` (`json` or `text`). |
| `extraEnv` | `[]` | Extra container `env` entries (any [setting](configuration.md)), for example `FASTMOCK_CALLBACK_RETENTION`. |

### Admin API

| Key | Default | Meaning |
|---|---|---|
| `admin.tokensSecret.name` | `""` (required unless `oidc.issuer` is set) | Secret with the admin token entries. Mounted read-only at `/etc/fastmock/admin/tokens` (`FASTMOCK_ADMIN_TOKENS_FILE`). |
| `admin.tokensSecret.key` | `tokens` | Key inside the Secret. |
| `admin.allowedOrigins` | `[]` | `FASTMOCK_ADMIN_ALLOWED_ORIGINS`: browser origins allowed besides the same origin. |
| `admin.uiDataPlaneUrls` | `[]` | `FASTMOCK_UI_DATA_PLANE_URL`: exact public origins of the data plane that the web UI may call (its `connect-src`). |
| `admin.mcp.enabled` | `true` | `FASTMOCK_MCP_ENABLED`: the [MCP endpoint](../guide/mcp.md) `/mcp` on the admin port for coding agents. |
| `access.roles`, `access.spaces`, `access.bindings` | `[]` | Custom roles, spaces and bindings, see [Access control](#access-control). |
| `oidc.*` | off | SSO sign-in and bearer JWTs, see [OIDC sign-in](#oidc-sign-in). |

### Storage

| Key | Default | Meaning |
|---|---|---|
| `storage.mode` | `postgres` | `postgres` or `memory`. |
| `storage.database.existingSecret` | `""` | Secret with the PostgreSQL URL; required in `postgres` mode unless `postgresql.enabled` or `cnpg.enabled`. |
| `storage.database.secretKey` | `database-url` | Key of the URL. |
| `storage.database.maxConnections` | `10` | `FASTMOCK_DB_MAX_CONNECTIONS` per pod. |
| `storage.database.migrations.mode` | `auto` | `auto`: every pod applies pending migrations at start-up. `job`: a pre-install/pre-upgrade Job runs `fastmock migrate` first and pods start with `FASTMOCK_DATABASE_MIGRATE=skip`; external databases only. See [Database](database.md). |
| `storage.database.migrations.timeout` | `120s` | `FASTMOCK_DATABASE_MIGRATION_TIMEOUT`, including the wait for the migration lock. In `auto` mode keep it below `startupProbe.periodSeconds × startupProbe.failureThreshold`. |
| `storage.database.migrations.job.existingSecret`, `.secretKey` | runtime Secret, `database-url` | URL for the Job; give it the schema owner so that the runtime user needs no `CREATE` privilege. |
| `storage.database.migrations.job.backoffLimit`, `.activeDeadlineSeconds`, `.resources` | `2`, `900`, small | Job settings. |
| `storage.valkey.existingSecret` | `""` | Secret with the Valkey URL; required in `postgres` mode unless `valkey.enabled`. |
| `storage.valkey.secretKey` | `valkey-url` | Key of the URL. |
| `storage.valkey.prefix` | `fastmock:` | `FASTMOCK_VALKEY_PREFIX`; give each release that shares a Valkey its own prefix. |
| `postgresql.enabled` | `false` | Install the CloudPirates `postgres` chart with the release. |
| `postgresql.auth.existingSecret` | `""` (required when enabled) | Secret with the superuser password under `postgresql.auth.secretKeys.adminPasswordKey` (`postgres-password`). |
| `postgresql.customUser.name`, `.database` | `fastmock`, `fastmock` | User and database FastMock uses. |
| `postgresql.customUser.existingSecret` | `""` (required when enabled) | Secret with that user's password under `postgresql.customUser.secretKeys.password` (`password`). |
| `postgresql.image.tag` | `18.7-trixie` | Official `postgres` image. |
| `postgresql.persistence.size`, `.storageClass` | `10Gi`, `""` | |
| `postgresql.resources` | requests `100m`/`256Mi`, limits `1`/`1Gi` | Other keys: the [chart's values](https://github.com/cloudpirates-io/helm-charts/blob/main/charts/postgres/values.yaml). |
| `cnpg.enabled` | `false` | Install the CloudNativePG `cluster` chart; needs the operator. |
| `cnpg.cluster.instances` | `1` | Use `3` for failover. |
| `cnpg.cluster.imageName` | `ghcr.io/cloudnative-pg/postgresql:18.7-standard-trixie` | |
| `cnpg.cluster.storage.size`, `.storageClass` | `10Gi`, `""` | |
| `cnpg.cluster.initdb` | database and owner `fastmock` | |
| `cnpg.backups` | `enabled: false` | Object storage backups; see the [chart's values](https://github.com/cloudnative-pg/charts/blob/main/charts/cluster/values.yaml). |
| `valkey.enabled` | `false` | Install the official `valkey` chart with the release. |
| `valkey.auth.usersExistingSecret` | `""` (required when enabled) | Secret with the password of the `default` user under `valkey.auth.aclUsers.default.passwordKey` (`password`). |
| `valkey.image.tag` | `9.1.4-trixie` | |
| `valkey.valkeyConfig` | `appendonly yes`, `maxmemory 256mb`, `maxmemory-policy volatile-lru` | Keep persistence on and eviction limited to keys with a TTL: counters and scenario state have none. |
| `valkey.dataStorage.requestedSize`, `.className` | `2Gi`, `""` | Persistent volume; the pod uses the `Recreate` strategy so the volume is never attached twice. |
| `valkey.resources` | requests `50m`/`64Mi`, limits `500m`/`384Mi` | Other keys: the [chart's values](https://github.com/valkey-io/valkey-helm/blob/main/valkey/values.yaml). |

### Mock behaviour and limits

| Key | Default | Variable |
|---|---|---|
| `callbacks.allowedHosts` | `[]` | `FASTMOCK_CALLBACK_ALLOWED_HOSTS` |
| `callbacks.deniedCidrs` | `""` (server default) | `FASTMOCK_CALLBACK_DENIED_CIDRS` (a comma-separated string) |
| `callbacks.workers` | `32` | `FASTMOCK_CALLBACK_WORKERS` |
| `callbacks.timeout` | `10s` | `FASTMOCK_CALLBACK_TIMEOUT` |
| `secrets.existingSecret` | `""` | Secret mounted at `/etc/fastmock/secrets` (`FASTMOCK_SECRETS_DIR`); each key is a secret for HMAC signatures and plugins. |
| `secrets.stubSecrets` | `[]` | `FASTMOCK_STUB_SECRETS`: names from that Secret that stubs may use. |
| `secrets.contractSecrets` | `{}` | `FASTMOCK_CONTRACT_SECRETS`: map `secret name → [hosts]`; names from that Secret that contract tests may send, and the only hosts they may be sent to. A name must not also be in `stubSecrets`. |
| `outboundCA.configMap` | `""` | ConfigMap with a PEM bundle of extra CA certificates for outgoing HTTPS; mounted read-only and passed as `FASTMOCK_OUTBOUND_CA_FILE`. |
| `outboundCA.key` | `ca.crt` | Key inside that ConfigMap. |
| `journal.retention` | `24h` | `FASTMOCK_JOURNAL_RETENTION` |
| `journal.maxBodyBytes` | `65536` | `FASTMOCK_JOURNAL_MAX_BODY_BYTES` |
| `journal.queue` | `2048` | `FASTMOCK_JOURNAL_QUEUE` |
| `journal.scanTimeout` | `10s` | `FASTMOCK_JOURNAL_SCAN_TIMEOUT` |
| `journal.scanMaxWork` | `4294967296` | `FASTMOCK_JOURNAL_SCAN_MAX_WORK` |
| `journal.scanConcurrency` | `2` | `FASTMOCK_JOURNAL_SCAN_CONCURRENCY` |
| `limits.maxBodyBytes` | `10485760` | `FASTMOCK_MAX_BODY_BYTES` |
| `limits.maxInflightBodyBytes` | `134217728` | `FASTMOCK_MAX_INFLIGHT_BODY_BYTES` (128 MiB, sized for the 512Mi memory limit) |
| `limits.maxDelay` | `15m` | `FASTMOCK_MAX_DELAY` |
| `limits.recordingMaxStubs` | `1000` | `FASTMOCK_RECORDING_MAX_STUBS` |
| `limits.headerReadTimeout` | `30s` | `FASTMOCK_HEADER_READ_TIMEOUT` |
| `messaging.kafka.bootstrap` | `[]` | `FASTMOCK_KAFKA_BOOTSTRAP` of an external cluster (`host:port`). Not with `kafka.enabled`. |
| `messaging.kafka.securityProtocol` | `""` (plaintext) | `plaintext`, `ssl`, `sasl_plaintext` or `sasl_ssl`. |
| `messaging.kafka.sasl.mechanism`, `.username` | `""` | `PLAIN`, `SCRAM-SHA-256` or `SCRAM-SHA-512`, and the user. |
| `messaging.kafka.sasl.existingSecret`, `.passwordKey` | `""`, `password` | Secret with the SASL password; required with a mechanism. |
| `messaging.kafka.tls.caSecret`, `.caKey` | `""`, `ca.crt` | Secret with the CA bundle of the brokers, added to the public roots. |
| `messaging.kafka.tls.clientCertSecret` | `""` | `kubernetes.io/tls` Secret with a client certificate (mTLS). |
| `messaging.kafka.schemaRegistry.url` | `""` | `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL`. |
| `messaging.kafka.schemaRegistry.username`, `.existingSecret`, `.passwordKey` | `""`, `""`, `password` | Basic authentication for the registry. |
| `messaging.kafka.allowedTopics` | `[]` | `FASTMOCK_KAFKA_ALLOWED_TOPICS` |
| `messaging.kafka.createTopics.enabled`, `.partitions`, `.replication` | `false`, `1`, `-1` | Create a missing topic on its first message (`FASTMOCK_KAFKA_CREATE_TOPICS`); only topics allowed by `allowedTopics`. `-1` uses the broker's default replication. Useful with `kafka.enabled`. |
| `messaging.kafka.contractTopics` | `[]` (reading disabled) | `FASTMOCK_KAFKA_CONTRACT_TOPICS`: topics AsyncAPI contract tests may read. |
| `messaging.mqtt.url` | `""` | `FASTMOCK_MQTT_URL` (`mqtt://host:1883` or `mqtts://host:8883`, no credentials). Nothing MQTT renders without it. See [MQTT and AMQP messages](../guide/mqtt-amqp.md). |
| `messaging.mqtt.version`, `.clientIdPrefix`, `.keepAlive` | `"5"`, `fastmock`, `30s` | `FASTMOCK_MQTT_VERSION` (`5` or `3.1.1`), `_CLIENT_ID_PREFIX`, `_KEEP_ALIVE`. |
| `messaging.mqtt.username`, `.existingSecret`, `.passwordKey` | `""`, `""`, `password` | User and the Secret holding the password. |
| `messaging.mqtt.tls.caSecret`, `.caKey`, `.clientCertSecret` | `""`, `ca.crt`, `""` | CA bundle and `kubernetes.io/tls` client certificate, mounted as files; needs `mqtts://`. |
| `messaging.mqtt.allowedTopics` | `[]` | `FASTMOCK_MQTT_ALLOWED_TOPICS` |
| `messaging.amqp.url` | `""` | `FASTMOCK_AMQP_URL` (`amqp://host:5672/vhost` or `amqps://`, vhost URL-encoded, no credentials). |
| `messaging.amqp.sasl.mechanism`, `.username`, `.existingSecret`, `.passwordKey` | `""` (`PLAIN`), `""`, `""`, `password` | `FASTMOCK_AMQP_SASL_MECHANISM` (`PLAIN` or `EXTERNAL`, which needs `amqps://` and a client certificate), user and password Secret. |
| `messaging.amqp.tls.caSecret`, `.caKey`, `.clientCertSecret` | `""`, `ca.crt`, `""` | As for MQTT; needs `amqps://`. |
| `messaging.amqp.allowedExchanges`, `.declareExchanges` | `[]`, `false` | `FASTMOCK_AMQP_ALLOWED_EXCHANGES`, `FASTMOCK_AMQP_DECLARE_EXCHANGES`. |
| `messaging.capture.enabled` | `false` | `FASTMOCK_CAPTURE_ENABLED`: [message capture](../guide/message-capture.md). |
| `messaging.capture.sources` | `[]` | Sources from the configuration (`name`, `protocol`, `space`, `topics`, `exchange`, `bindingKeys`, `qos`, `startFrom`, `maxMessages`, `enabled`, `description`), rendered into the ConfigMap `<fullname>-capture` and read through `FASTMOCK_CAPTURE_SOURCES_FILE`; pods restart when they change. |
| `messaging.capture.limits.maxSources`, `.maxMessages`, `.maxMessageBytes`, `.maxBytes`, `.maxAge` | `50`, `1000`, `1048576`, `268435456`, `24h` | `FASTMOCK_CAPTURE_MAX_*`. |
| `messaging.capture.limits.maxSpaceBytes` | unset (a quarter of `maxBytes`) | `FASTMOCK_CAPTURE_MAX_SPACE_BYTES`: stored bytes per space. |
| `messaging.capture.allowedKafkaTopics`, `.allowedMqttTopics`, `.allowedAmqpExchanges` | `[]` | `FASTMOCK_CAPTURE_KAFKA_TOPICS`, `_MQTT_TOPICS`, `_AMQP_EXCHANGES`: what capture sources may read (names, `prefix*` or `*`). Empty: only the sources in `messaging.capture.sources`; sources created through the API need the list of their protocol. Spaces narrow them with `access.spaces[].capture` ([Access control](#access-control)). |
| `kafka.enabled` | `false` | Install the CloudPirates `kafka` chart (one broker, KRaft) and connect FastMock to it with SASL/PLAIN. |
| `kafka.auth.existingSecret` | `""` (the Kafka chart generates `<release>-kafka`) | Secret with `client-password`, `inter-broker-password` and `controller-password`. Without it the Kafka chart generates random passwords once, keeps them across upgrades and FastMock reads `client-password` from that Secret. |
| `kafka.auth.clientUser` | `fastmock` | SASL user FastMock connects as. |
| `kafka.image.registry`, `.repository`, `.tag` | chart default: `docker.io`, `apache/kafka`, `4.4.0` pinned by digest | Official `apache/kafka` image; override all three for a mirror. |
| `kafka.serviceAccount.create`, `.name`, `.annotations` | `true`, `""` (`<release>-kafka`), `{}` | Dedicated ServiceAccount for the broker; set `create: false` and `name` to use an existing one. |
| `kafka.serviceAccount.automountServiceAccountToken` | `false` | The broker does not call the Kubernetes API. |
| `kafka.persistence.size`, `kafka.resources` | `8Gi`, requests `250m`/`768Mi`, limits `1`/`1Gi` | Other keys: the [chart's values](https://github.com/cloudpirates-io/helm-charts/blob/main/charts/kafka/values.yaml) (version 0.3.0). |
| `plugins` | `[]` (all) | `FASTMOCK_PLUGINS` |
| `pluginSettings` | `{}` | Map `plugin → {key: value}`, rendered as `FASTMOCK_PLUGIN_<PLUGIN>_<KEY>`. |

Keep `limits.maxInflightBodyBytes` well below the memory limit: request bodies are held in memory while a
request is processed.

### Mail

| Key | Default | Meaning |
|---|---|---|
| `mail.enabled` | `false` | `FASTMOCK_MAIL_ENABLED`; see [Mail](#mail). |
| `mail.ports.smtp` | `2525` | SMTP port of the pod and of the `<fullname>-mail` Service; `""` turns SMTP off. |
| `mail.ports.smtps`, `.imap`, `.imaps`, `.pop3`, `.pop3s` | `""` (off) | Ports of the other listeners, for example `4650`, `1143`, `1993`, `1110`, `1995`. The implicit-TLS ones need `mail.tls.existingSecret`. |
| `mail.hostname` | `""` (`localhost`) | `FASTMOCK_MAIL_HOSTNAME`. |
| `mail.tls.existingSecret` | `""` | Secret with the certificate and key for STARTTLS, STLS and the implicit-TLS ports, mounted read-only at `/etc/fastmock/tls/mail/`. |
| `mail.tls.certKey`, `.keyKey` | `tls.crt`, `tls.key` | Keys inside that Secret (a `kubernetes.io/tls` Secret from cert-manager works as is). |
| `mail.requireAuth` | `false` | `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH`. |
| `mail.smtpUsers.existingSecret`, `.key` | `""`, `users` | Secret with the `user:hash` entries of `FASTMOCK_MAIL_SMTP_USERS` (`pbkdf2-sha256$...` recommended, hex SHA-256 accepted). |
| `mail.allowPlaintextAuth` | `false` | `FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH`. |
| `mail.limits.maxMessageBytes`, `.maxRecipients`, `.maxMessages`, `.maxAge` | `10485760`, `100`, `1000`, `7d` | `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`, `_MAX_RECIPIENTS`, `_MAX_MESSAGES`, `_MAX_AGE`. |
| `mail.limits.maxBytes`, `.maxMailboxes` | `268435456`, `10000` | `FASTMOCK_MAIL_MAX_BYTES` (raw bytes kept per space, at least `maxMessageBytes`), `_MAX_MAILBOXES` (mailboxes per space). |
| `mail.limits.maxConnections`, `.maxConnectionsPerIp`, `.maxAuthFailures` | `256`, `32`, `3` | `FASTMOCK_MAIL_MAX_CONNECTIONS`, `_MAX_CONNECTIONS_PER_IP`, `_MAX_AUTH_FAILURES`. |
| `mail.limits.loginFailureLimit` | `5` | `FASTMOCK_MAIL_LOGIN_FAILURE_LIMIT`: failed logins per client address and per user name within 5 minutes before logins are refused for a backoff; `0` turns it off. |
| `mail.limits.idleTimeout`, `.dataTimeout`, `.sessionTimeout`, `.imapIdleTimeout` | `30s`, `60s`, `10m`, `30m` | `FASTMOCK_MAIL_IDLE_TIMEOUT`, `_DATA_TIMEOUT`, `_SESSION_TIMEOUT`, `_IMAP_IDLE_TIMEOUT`. |
| `mail.service.type`, `.annotations` | `ClusterIP`, `{}` | The `<fullname>-mail` Service. |
| `networkPolicy.mailFrom` | `[]` (any) | `from` peers allowed on the mail ports. |

### Telemetry

| Key | Default | Meaning |
|---|---|---|
| `otel.endpoint` | `""` | `OTEL_EXPORTER_OTLP_ENDPOINT`; traces and logs are exported only when set. |
| `otel.protocol` | `http/protobuf` | `OTEL_EXPORTER_OTLP_PROTOCOL` |
| `otel.serviceName` | `fastmock` | `OTEL_SERVICE_NAME` |
| `otel.resourceAttributes` | `""` | `OTEL_RESOURCE_ATTRIBUTES`, for example `deployment.environment=staging` |
| `metrics.serviceMonitor.enabled` | `false` | Create a ServiceMonitor that scrapes `/metrics` on the admin Service. |
| `metrics.serviceMonitor.interval` | `30s` | |
| `metrics.serviceMonitor.labels` | `{}` | Labels your Prometheus selects ServiceMonitors by. |
| `metrics.prometheusRule.enabled` | `false` | Create the [alert rules](observability.md#alerts). |
| `metrics.prometheusRule.labels` | `{}` | |
| `metrics.prometheusRule.unmatchedRatio` | `0.2` | Threshold of `FastMockUnmatchedRequests`. |
| `metrics.prometheusRule.callbackBacklog` | `1000` | Threshold of `FastMockCallbackBacklog`. |

### Networking

| Key | Default | Meaning |
|---|---|---|
| `service.type` | `ClusterIP` | Type of the data Service (the admin Service is always `ClusterIP`). |
| `service.port`, `service.adminPort` | `8080`, `9090` | Service ports. |
| `service.annotations` | `{}` | Annotations of the data Service. |
| `route.enabled` | `false` | HTTPRoute `<fullname>` for the data port (path prefix `/`). |
| `route.hostnames`, `route.parentRefs` | `[]` | Hostnames and Gateway references. |
| `route.admin.enabled` | `false` | HTTPRoute `<fullname>-admin` for `/__admin` (prefix) and `/readyz`. |
| `route.admin.hostnames` | `[]` | |
| `route.admin.parentRefs` | `[]` (falls back to `route.parentRefs`) | |
| `networkPolicy.enabled` | `false` | Ingress (and optionally egress) policy for FastMock pods. |
| `networkPolicy.dataFrom` | `[]` (any) | `from` peers allowed on the data port. |
| `networkPolicy.adminFrom` | `[]` (any) | `from` peers allowed on the admin port (tests, Prometheus, the Gateway, the operator). |
| `networkPolicy.storage` | `true` | With `postgresql.enabled` or `valkey.enabled`, allow connections to them only from FastMock pods. CloudNativePG clusters are left to the operator's own policies. |
| `networkPolicy.egress.enabled` | `false` | Also restrict egress: DNS and the databases installed with the release are always allowed, plus `rules`. |
| `networkPolicy.egress.rules` | `[]` | Extra egress rules (external databases, Kafka, callback receivers, proxy targets). |
| `pdb.enabled`, `pdb.minAvailable` | `true`, `1` | PodDisruptionBudget. Disable for a single replica. |
| `autoscaling.enabled` | `false` | HorizontalPodAutoscaler on CPU (`postgres` mode only). |
| `autoscaling.minReplicas`, `maxReplicas`, `targetCPUUtilizationPercentage` | `2`, `6`, `70` | |

## Production checklist

A values file that covers the list below (all of it renders with `helm template` and passes `helm lint`):

```yaml
image:
  repository: registry.example.com/platform/fastmock
  tag: 0.1.0
replicaCount: 3
admin:
  tokensSecret:
    name: fastmock-admin-tokens
  allowedOrigins:
    - https://fastmock-admin.example.com
storage:
  database:
    existingSecret: fastmock-storage
    secretKey: database-url
    maxConnections: 20
  valkey:
    existingSecret: fastmock-storage
    secretKey: valkey-url
secrets:
  existingSecret: fastmock-hook-secrets
  stubSecrets:
    - delivery-hook
callbacks:
  allowedHosts:
    - "*.svc.cluster.local"
    - hooks.example.com
messaging:
  kafka:
    bootstrap:
      - kafka-0.kafka.example.svc:9093
    securityProtocol: sasl_ssl
    sasl:
      mechanism: SCRAM-SHA-512
      username: fastmock
      existingSecret: fastmock-kafka-user
    tls:
      caSecret: kafka-cluster-ca
    schemaRegistry:
      url: https://schema-registry.kafka.example.svc:8081
    allowedTopics:
      - "orders.*"
otel:
  endpoint: http://otel-collector.observability.svc:4318
metrics:
  serviceMonitor:
    enabled: true
  prometheusRule:
    enabled: true
networkPolicy:
  enabled: true
  adminFrom:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: monitoring
    - podSelector:
        matchLabels:
          app.kubernetes.io/name: integration-tests
  egress:
    enabled: true
    rules:
      - to:
          - ipBlock:
              cidr: 198.51.100.0/24
        ports:
          - port: 5432
          - port: 6379
      - to:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: payments
autoscaling:
  enabled: true
  minReplicas: 3
  maxReplicas: 8
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
    labelSelector:
      matchLabels:
        app.kubernetes.io/name: fastmock
        app.kubernetes.io/component: server
```

- [ ] **External PostgreSQL and Valkey** with backups, TLS to PostgreSQL (`sslmode=verify-full`), and a
      dedicated database user that owns only the FastMock database. Valkey must persist data and must not evict
      keys without TTL (`maxmemory-policy volatile-lru` or `noeviction`).
- [ ] **Secrets** come from your secret manager (External Secrets, Sealed Secrets, CSI driver) into the three
      Secrets: admin tokens, storage URLs, HMAC keys. Nothing secret goes into values files.
- [ ] **Tokens per client** (`ci:editor`, `ops:admin`, `grafana:viewer`); no shared tokens. See
      [token rotation](security.md#rotating-tokens).
- [ ] **NetworkPolicy** on: the admin port only from tests, Prometheus and the Gateway; egress only to the
      databases, Kafka and the callback/proxy targets.
- [ ] **Egress allow-list** `callbacks.allowedHosts` matching the egress NetworkPolicy.
- [ ] **Stub secrets** listed explicitly in `secrets.stubSecrets`; contract-test secrets in
      `secrets.contractSecrets`, each bound only to the host of the service it belongs to.
- [ ] **Kafka reads** for contract tests limited to the topics in `messaging.kafka.contractTopics` (empty disables them).
- [ ] **Encrypted, authenticated connections** to PostgreSQL (`verify-full`), Valkey (`rediss://` with an ACL user) and Kafka (`sasl_ssl`), see [Secure connections](#secure-connections).
- [ ] **Replicas, PDB, spread**: at least 2 replicas, `pdb.enabled`, topology spread across nodes; HPA if load
      varies.
- [ ] **Resources** sized: memory limit above `limits.maxInflightBodyBytes` plus stubs and caches;
      `storage.database.maxConnections × replicas` below the PostgreSQL connection limit.
- [ ] **Monitoring**: ServiceMonitor, PrometheusRule, the [Grafana dashboard](observability.md#grafana-dashboard),
      OTLP endpoint.
- [ ] **Image pinned** to a tag or digest from your registry.
- [ ] **Admin UI** behind a Gateway with TLS and `admin.allowedOrigins` set to its URL (next section);
      `admin.uiDataPlaneUrls` set to the public data-plane URL when the UI's test panel should reach it.

## Exposing the admin UI with Gateway API

The chart's `route.admin` publishes `/__admin` and `/readyz`. To publish the web UI and the API description as
well, create a route like the following (TLS terminates at the Gateway; adjust the names):

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: fastmock-admin-ui
  namespace: mocks
spec:
  parentRefs:
    - name: internal-gateway
      namespace: gateway-system
      sectionName: https
  hostnames:
    - fastmock-admin.example.com
  rules:
    - matches:
        - path:
            type: PathPrefix
            value: /__admin
        - path:
            type: PathPrefix
            value: /ui
        - path:
            type: Exact
            value: /openapi.json
      backendRefs:
        - name: fastmock-admin
          port: 9090
```

and set `admin.allowedOrigins: [https://fastmock-admin.example.com]` (requests from the UI at that origin are
also accepted as same-origin when the Gateway forwards the original `Host`). Do not route `/metrics` publicly;
Prometheus scrapes it inside the cluster. Allow the Gateway's pods in `networkPolicy.adminFrom`.

## Secure connections

FastMock verifies the server certificate of every database and broker it connects to and can present a client
certificate. Certificates come from Secrets; the chart mounts them read-only under `/etc/fastmock/tls/` and sets
the matching variables ([Configuration](configuration.md)).

| Service | Values | Result |
|---|---|---|
| PostgreSQL | `storage.database.tls.mode` (`verify-full` recommended), `.caSecret`, `.clientCertSecret` | `FASTMOCK_DATABASE_SSL_MODE`, `_SSL_ROOT_CERT`, `_SSL_CERT`, `_SSL_KEY`; settings in the URL (`sslmode=...`) also work |
| PostgreSQL with `cnpg.enabled` | nothing | `verify-full` against the CA Secret the operator creates (`<release>-cnpg-ca`) |
| Valkey | a `rediss://` URL in `storage.valkey.existingSecret`, `storage.valkey.username`, `.tls.caSecret`, `.tls.clientCertSecret` | `FASTMOCK_VALKEY_USERNAME`, `_TLS_CA_FILE`, `_TLS_CERT_FILE`, `_TLS_KEY_FILE` |
| Kafka | `messaging.kafka.securityProtocol`, `.sasl.*`, `.tls.*` | `FASTMOCK_KAFKA_SECURITY_PROTOCOL`, `_SASL_*`, `_TLS_*` |
| Schema Registry | `messaging.kafka.schemaRegistry.*` with an `https://` URL | basic authentication, trusts `messaging.kafka.tls.caSecret` |
| MQTT | an `mqtts://` URL in `messaging.mqtt.url`, `.username`, `.existingSecret`, `.tls.*` | `FASTMOCK_MQTT_USERNAME`, `_PASSWORD`, `_TLS_*` |
| AMQP | an `amqps://` URL in `messaging.amqp.url`, `.sasl.*`, `.tls.*` | `FASTMOCK_AMQP_USERNAME`, `_PASSWORD`, `_SASL_MECHANISM`, `_TLS_*` |

CA Secrets need the key named in `caKey` (default `ca.crt`); client certificates are standard `kubernetes.io/tls`
Secrets (`tls.crt`, `tls.key`), for example issued by cert-manager.

## TLS and mTLS

The **data port** can serve HTTPS itself, which is needed to mock services that clients reach with mTLS. The
chart has no values for certificate volumes: either let the Gateway terminate TLS and keep the pods on HTTP, or
add a Secret volume with a Helm post-renderer (for example kustomize) and point the variables at it with
`extraEnv`. Server settings: `FASTMOCK_TLS_CERT_FILE`, `FASTMOCK_TLS_KEY_FILE`,
`FASTMOCK_TLS_CLIENT_CA_FILE`, `FASTMOCK_TLS_CLIENT_AUTH` ([Configuration](configuration.md#data-plane-tls-and-mtls)).
Stubs can match the client certificate fingerprint through the `x-fastmock-client-cert-sha256` header.

The **admin port** is plain HTTP inside the cluster; publish it only through a Gateway or ingress with TLS.

## NetworkPolicy

With `networkPolicy.enabled: true` the chart creates one policy for FastMock pods:

- **ingress** on the data port from `dataFrom` (or anywhere when empty), on the admin port from `adminFrom` (or
  anywhere when empty), and with [mail](#mail) on the mail ports from `mailFrom` (or anywhere when empty). Set
  `adminFrom`.
- **egress** (with `egress.enabled`): DNS to `kube-dns`, the databases installed with the release (including the CloudNativePG cluster on port 5432), and your `egress.rules`. Callback
  receivers, proxy targets, Kafka, external PostgreSQL/Valkey and the OTLP collector must be covered by rules.

With `postgresql.enabled` or `valkey.enabled`, `networkPolicy.storage: true` adds a second policy that lets only FastMock pods of the
release reach them.

## MQTT, AMQP and message capture

Stub messages and async mocks publish to MQTT and AMQP brokers once their URL is set
([MQTT and AMQP messages](../guide/mqtt-amqp.md)); `messaging.capture` turns on
[message capture](../guide/message-capture.md). Passwords come from Secrets, CA bundles and client certificates are
mounted as files, and capture sources are rendered into a ConfigMap:

```yaml
messaging:
  mqtt:
    url: mqtts://mqtt.example.com:8883
    username: fastmock
    existingSecret: fastmock-mqtt
    tls:
      caSecret: fastmock-brokers-ca
    allowedTopics: ["devices/*"]
  amqp:
    url: amqps://rabbitmq.example.com:5671/orders
    sasl:
      username: fastmock
      existingSecret: fastmock-amqp
    tls:
      caSecret: fastmock-brokers-ca
    allowedExchanges: [orders]
  capture:
    enabled: true
    sources:
      - name: orders
        protocol: amqp
        exchange: orders
        bindingKeys: ["order.*"]
      - name: telemetry
        protocol: mqtt
        topics: ["devices/+/telemetry"]
    allowedMqttTopics: ["devices/*"]
    allowedAmqpExchanges: [orders]
```

The install fails with a clear message for settings the server would refuse: TLS settings with a plain URL, a
user without its password Secret, `EXTERNAL` without `amqps://` and a client certificate, sources while capture is
off, duplicate source names in a space, or more sources than `limits.maxSources`. With a NetworkPolicy egress
allow-list, allow the broker ports (1883/8883, 5672/5671). The MQTT TLS bridge uses an abstract Unix socket, so
`mqtts://` needs no writable `/tmp` on the read-only root filesystem. Captured messages are kept in memory with
`storage.mode: memory`: size the memory limit for `limits.maxBytes` plus the rest of the server.

## Mail

With `mail.enabled: true` the chart sets the `FASTMOCK_MAIL_*` variables, adds the mail ports to the container and
creates a Service `<fullname>-mail` (`ClusterIP` by default) with one port per enabled listener, so applications
in the cluster send to `<fullname>-mail:2525`. The provider APIs use the data Service (`/__mail/...`) and the mail
admin API the admin Service; neither needs extra values. See [Mail](../guide/mail.md).

```yaml
mail:
  enabled: true
  ports:
    smtp: 2525
    imap: 1143
  tls:
    existingSecret: fastmock-mail-tls
  limits:
    maxAge: 1d
access:
  spaces:
    - name: billing
      mailDomains: [billing.example.test]
networkPolicy:
  enabled: true
  mailFrom:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: billing
```

The chart refuses to render when a mail port equals the data or admin port, when two mail listeners share a
port, or when an implicit-TLS port is set without `mail.tls.existingSecret`. Messages are stored in PostgreSQL in
`postgres` mode, so every replica answers waits and IMAP logins for messages accepted by another one; with a
restricted Valkey ACL user, add the mail event channel and the failed-login keys
([Database](database.md#valkey-and-redis)). Keep the mail Service `ClusterIP` unless systems outside the cluster
must send to it, and limit `networkPolicy.mailFrom` to the namespaces of the applications under test.

Provider webhooks with a signing key or a URL password need the secret `mail-webhook-key` in `postgres` mode: add
a long random value under that key to the Secret of `secrets.existingSecret` (mounted as `FASTMOCK_SECRETS_DIR`),
for example with `kubectl create secret generic fastmock-secrets --from-literal=mail-webhook-key="$(openssl rand -base64 32)"`.
Every replica must use the same value.

## Access control

The `access` values become the access file ([Access control](access-control.md)): the chart renders them into the
ConfigMap `<fullname>-access`, mounts it read-only at `/etc/fastmock/access/access.yaml` and sets
`FASTMOCK_ACCESS_FILE`. Nothing is rendered while all three lists are empty. The pods carry a `checksum/access`
annotation, so `helm upgrade` with changed values rolls them.

```yaml
access:
  roles:
    - name: stub-author
      permissions: [stubs:read, stubs:write, imports:run, journal:read, state:read]
  spaces:
    - name: payments
      pathPrefixes: [/payments/]
      hosts: [psp.example.com]
      capture:
        kafkaTopics: ["payments.*"]
    - name: billing
  bindings:
    - subject: token:ci-billing
      role: stub-author
      spaces: [billing]
    - subject: group:platform
      role: admin
      global: true
```

`access.spaces[].capture` (`kafkaTopics`, `mqttTopics`, `amqpExchanges`) narrows the capture allow-lists of
`messaging.capture.allowedKafkaTopics`, `.allowedMqttTopics` and `.allowedAmqpExchanges` for the sources of that
space; a space can never read more than the server-wide lists allow
([Message capture: allow-lists per space](../guide/message-capture.md#allow-lists-per-space)).

The values schema checks names, permission and subject formats, and that capture list entries are names, `prefix*`
or `*`; the server checks the rest at start-up (unknown
permissions, roles and spaces, `token:` bindings without a matching entry in the tokens Secret). A token that
should only get space bindings has an empty role in the Secret: `ci-billing::<sha256>`.

## OIDC sign-in

The `oidc` values configure [OIDC sign-in](access-control.md#oidc-sign-in-and-bearer-jwts). Bind the provider's
groups in `access.bindings`; without a binding a user cannot sign in.

| Key | Default | Meaning |
|---|---|---|
| `oidc.issuer` | `""` | `FASTMOCK_OIDC_ISSUER`; an https URL. Empty: OIDC off. |
| `oidc.clientId` | `""` | `FASTMOCK_OIDC_CLIENT_ID` (required with the issuer). |
| `oidc.clientSecret.existingSecret` | `""` | Secret with the client secret (required with the issuer), mounted read-only at `/etc/fastmock/oidc/client-secret` (`FASTMOCK_OIDC_CLIENT_SECRET_FILE`). |
| `oidc.clientSecret.key` | `client-secret` | Key inside the Secret. |
| `oidc.redirectUrl` | `""` | `FASTMOCK_OIDC_REDIRECT_URL`: the public admin URL plus `/__admin/auth/callback` (required with the issuer). |
| `oidc.scopes` | `[]` | `FASTMOCK_OIDC_SCOPES`; empty means `openid profile email`. |
| `oidc.claims.username`, `oidc.claims.groups` | `""` | `FASTMOCK_OIDC_USERNAME_CLAIM`, `FASTMOCK_OIDC_GROUPS_CLAIM`; empty means `preferred_username` and `groups`. For `user:` bindings set `oidc.claims.username: sub` (or `oid` with Entra ID), see [choosing the username claim](access-control.md#choosing-the-username-claim). |
| `oidc.apiAudience` | `""` | `FASTMOCK_OIDC_API_AUDIENCE`: accept the provider's JWTs with this audience on the admin API; must differ from `oidc.clientId`. |
| `oidc.trustOutboundCA` | `false` | Trust the CA bundle of `outboundCA.configMap` for the provider connections (`FASTMOCK_OIDC_CA_FILE`). |
| `oidc.session.ttl`, `oidc.session.idle` | `8h`, `1h` | `FASTMOCK_OIDC_SESSION_TTL`, `FASTMOCK_OIDC_SESSION_IDLE`. |
| `oidc.allowedHosts` | `[]` | `FASTMOCK_OIDC_ALLOWED_HOSTS`: hosts besides the issuer's that discovery endpoints may use. |

```yaml
admin:
  allowedOrigins: [https://mock-admin.example.com]
oidc:
  issuer: https://sso.example.com/realms/mocks
  clientId: fastmock
  clientSecret:
    existingSecret: fastmock-oidc
  redirectUrl: https://mock-admin.example.com/__admin/auth/callback
  apiAudience: fastmock-api
access:
  spaces:
    - name: payments
  bindings:
    - subject: group:payments-dev
      role: editor
      spaces: [payments]
    - subject: group:platform
      role: admin
      global: true
route:
  admin:
    enabled: true
    hostnames: [mock-admin.example.com]
```

```sh
kubectl create secret generic fastmock-oidc --from-literal=client-secret="$CLIENT_SECRET"
```

Sessions live in Valkey with `storage.mode=postgres`, so every replica accepts them; with `storage.mode=memory`
they live in the single pod and end with it. The admin token Secret becomes optional with OIDC, but keep one
admin token for automation and as a fallback when the provider is unavailable. With
`networkPolicy.egress.enabled`, add a rule that lets the pods reach the provider. The client secret is read at
start-up: after rotating it, `kubectl rollout restart deploy/<fullname>`.

## Upgrade notes

- **Access control** (spaces, managed tokens, OIDC sign-in): with no `access` and `oidc` values the release
  behaves as before, but read the [access control upgrade notes](access-control.md#upgrade-notes) first: a new
  table needs grants when migrations run separately with per-table rights, audit log lines and auth failure
  metrics changed, and new `/__admin/auth/*` routes must pass a WAF or route allow-list when OIDC is used.
  Upgrade every replica before setting `access` or `oidc`.
- **Mail**: the release that adds [mail](../guide/mail.md) adds migrations 11 to 14 (four tables, a
  sequence and three indexes; [Database](database.md#mail-migrations)) and the `mail:*` permissions: `editor` now
  includes `mail:write` and `mail:delete`, so custom roles that copy editor's permission list need `mail:*` to keep
  creating editor tokens ([access control upgrade notes](access-control.md#upgrade-notes)). With `mail.enabled`
  in `postgres` mode and a restricted Valkey ACL user, add the channel rule `&<prefix>mail:events:*` and
  `+publish +psubscribe`; otherwise the pods stop at start-up. With per-table grants, grant the new tables before
  upgrading.
- **0.3.0** (FastMock 0.1.0-dev15): per-space capture allow-lists (`access.spaces[].capture`), gRPC proxy and
  recording settings, and AMQP SASL EXTERNAL; no migration. Capture sources created through the API now belong to a
  space: a space with a `capture` list can read only what both its list and the server-wide list allow. Review
  `messaging.capture.allowed*` before upgrading if sources of several spaces share one broker login.
- **0.2.0**: MQTT and AMQP publishing (`messaging.mqtt`, `messaging.amqp`) and [message capture](../guide/message-capture.md)
  (`messaging.capture`), all off by default. The release adds migration 16 (table `fm_captured_messages`) and the
  `messages:*` permissions: `editor` now includes `messages:write` and `messages:delete`, so custom roles that copy
  editor's permission list need `messages:*` to keep creating editor tokens. The metric `fastmock_messages_total`
  gains the label `protocol`.
- **0.1.0**: first release. `FASTMOCK_STUB_SECRETS` (`secrets.stubSecrets`) is required for stub HMAC
  signatures; stubs that reference other secrets produce failed callbacks with
  `secret ... is not configured`.
- The chart never deletes PersistentVolumeClaims or database content on upgrade.
- When the operator manages FastMock, upgrade the operator chart instead ([Operator](operator.md#upgrading)).
