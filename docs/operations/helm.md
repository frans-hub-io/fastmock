# Helm chart

`deploy/helm/fastmock` installs one FastMock instance: a Deployment, separate Services for the data and admin
ports, and optionally bundled PostgreSQL and Valkey, Gateway API routes, a PodDisruptionBudget, an HPA,
NetworkPolicies, a ServiceMonitor and a PrometheusRule. It needs Kubernetes 1.30 or newer.

For many instances managed declaratively, see the [operator](operator.md) instead.

- [Install](#install)
- [Upgrade and uninstall](#upgrade-and-uninstall)
- [Values](#values)
- [Production checklist](#production-checklist)
- [Exposing the admin UI with Gateway API](#exposing-the-admin-ui-with-gateway-api)
- [TLS and mTLS](#tls-and-mtls)
- [NetworkPolicy](#networkpolicy)
- [Upgrade notes](#upgrade-notes)

## Install

The chart refuses to render without an admin token Secret, and in `postgres` mode without database and Valkey
connection settings.

**1. Admin tokens.** One `name:role:sha256hex` entry per line, under the key `tokens`:

```sh
fastmock hash-token --generate          # or: docker run --rm fastmock:0.1.0 hash-token --generate
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

**2b. Bundled PostgreSQL 18 and Valkey 9** (single-instance StatefulSets, for test environments that do not have
managed databases):

```sh
kubectl -n mocks create secret generic fastmock-db --from-literal=password="$(openssl rand -hex 24)"
kubectl -n mocks create secret generic fastmock-valkey --from-literal=password="$(openssl rand -hex 24)"
helm upgrade --install fastmock deploy/helm/fastmock -n mocks \
  --set admin.tokensSecret.name=fastmock-admin-tokens \
  --set postgresql.enabled=true --set postgresql.auth.existingSecret=fastmock-db \
  --set valkey.enabled=true --set valkey.auth.existingSecret=fastmock-valkey
```

The passwords reach PostgreSQL, Valkey and FastMock through environment variables from the Secrets
(`FASTMOCK_DATABASE_PASSWORD`, `FASTMOCK_VALKEY_PASSWORD`), never through command lines or ConfigMaps. With
`networkPolicy.storage: true` (default) only FastMock pods may connect to the bundled databases.

**2c. Memory mode** (one replica, nothing persisted):

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
- Database migrations run at start-up and are additive; the first new pod migrates, old pods keep working.
- `helm uninstall` keeps the PersistentVolumeClaims of the bundled databases (`data-<fullname>-postgresql-0`,
  `data-<fullname>-valkey-0`); delete them to remove the data.

## Values

### Image and pods

| Key | Default | Meaning |
|---|---|---|
| `image.repository` | `fastmock` | Server image repository. |
| `image.tag` | `""` (chart `appVersion`) | Image tag. Pin a version or digest in production. |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | List of `{name: ...}`; also used by the bundled databases. |
| `replicaCount` | `2` | Replicas when autoscaling is off. Use 1 with `storage.mode: memory`. |
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
| `admin.tokensSecret.name` | `""` (required) | Secret with the admin token entries. Mounted read-only at `/etc/fastmock/admin/tokens` (`FASTMOCK_ADMIN_TOKENS_FILE`). |
| `admin.tokensSecret.key` | `tokens` | Key inside the Secret. |
| `admin.allowedOrigins` | `[]` | `FASTMOCK_ADMIN_ALLOWED_ORIGINS`: browser origins allowed besides the same origin. |
| `admin.uiDataPlaneUrls` | `[]` | `FASTMOCK_UI_DATA_PLANE_URL`: exact public origins of the data plane that the web UI may call (its `connect-src`). |

### Storage

| Key | Default | Meaning |
|---|---|---|
| `storage.mode` | `postgres` | `postgres` or `memory`. |
| `storage.database.existingSecret` | `""` | Secret with the PostgreSQL URL; required in `postgres` mode unless `postgresql.enabled`. |
| `storage.database.secretKey` | `database-url` | Key of the URL. |
| `storage.database.maxConnections` | `10` | `FASTMOCK_DB_MAX_CONNECTIONS` per pod. |
| `storage.valkey.existingSecret` | `""` | Secret with the Valkey URL; required in `postgres` mode unless `valkey.enabled`. |
| `storage.valkey.secretKey` | `valkey-url` | Key of the URL. |
| `storage.valkey.prefix` | `fastmock:` | `FASTMOCK_VALKEY_PREFIX`; give each release that shares a Valkey its own prefix. |
| `postgresql.enabled` | `false` | Deploy a PostgreSQL StatefulSet with the release. |
| `postgresql.image` | `docker.io/library/postgres:18.7-trixie` | |
| `postgresql.database`, `postgresql.username` | `fastmock`, `fastmock` | |
| `postgresql.auth.existingSecret` | `""` (required when enabled) | Secret with the password. |
| `postgresql.auth.passwordKey` | `password` | |
| `postgresql.storage.size`, `postgresql.storage.storageClassName` | `10Gi`, `""` | PVC size and class. |
| `postgresql.resources` | requests `100m`/`256Mi`, limits `1`/`1Gi` | |
| `valkey.enabled` | `false` | Deploy a Valkey StatefulSet (append-only persistence, `volatile-lru`). |
| `valkey.image` | `docker.io/valkey/valkey:9.1.4-trixie` | |
| `valkey.auth.existingSecret` | `""` (required when enabled) | Secret with the password. |
| `valkey.auth.passwordKey` | `password` | |
| `valkey.maxMemory` | `256mb` | Valkey `maxmemory`. |
| `valkey.storage.size`, `valkey.storage.storageClassName` | `2Gi`, `""` | |
| `valkey.resources` | requests `50m`/`64Mi`, limits `500m`/`384Mi` | |

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
| `kafka.bootstrap` | `[]` | `FASTMOCK_KAFKA_BOOTSTRAP` |
| `kafka.schemaRegistryUrl` | `""` | `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` |
| `kafka.allowedTopics` | `[]` | `FASTMOCK_KAFKA_ALLOWED_TOPICS` |
| `kafka.contractTopics` | `[]` (reading disabled) | `FASTMOCK_KAFKA_CONTRACT_TOPICS`: topics AsyncAPI contract tests may read. |
| `plugins` | `[]` (all) | `FASTMOCK_PLUGINS` |
| `pluginSettings` | `{}` | Map `plugin → {key: value}`, rendered as `FASTMOCK_PLUGIN_<PLUGIN>_<KEY>`. |

Keep `limits.maxInflightBodyBytes` well below the memory limit: request bodies are held in memory while a
request is processed.

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
| `networkPolicy.storage` | `true` | With bundled databases, allow connections to them only from FastMock pods. |
| `networkPolicy.egress.enabled` | `false` | Also restrict egress: DNS and the bundled databases are always allowed, plus `rules`. |
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
kafka:
  bootstrap:
    - kafka-0.kafka.example.svc:9092
  schemaRegistryUrl: http://schema-registry.kafka.example.svc:8081
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
- [ ] **Kafka reads** for contract tests limited to the topics in `kafka.contractTopics` (empty disables them).
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
  anywhere when empty). Set `adminFrom`.
- **egress** (with `egress.enabled`): DNS to `kube-dns`, the bundled databases, and your `egress.rules`. Callback
  receivers, proxy targets, Kafka, external PostgreSQL/Valkey and the OTLP collector must be covered by rules.

With bundled databases, `networkPolicy.storage: true` adds a second policy that lets only FastMock pods of the
release reach them.

## Upgrade notes

- **0.1.0**: first release. `FASTMOCK_STUB_SECRETS` (`secrets.stubSecrets`) is required for stub HMAC
  signatures; stubs that reference other secrets produce failed callbacks with
  `secret ... is not configured`.
- The chart never deletes PersistentVolumeClaims or database content on upgrade.
- When the operator manages FastMock, upgrade the operator chart instead ([Operator](operator.md#upgrading)).
