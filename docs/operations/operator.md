# Kubernetes operator

The operator manages FastMock declaratively with two custom resources:

- **`FastMock`** describes a FastMock instance. The operator creates its Deployment, Services, configuration,
  admin tokens, PodDisruptionBudget and optional HTTPRoute.
- **`MockApi`** describes mocks for a `FastMock`: inline stubs, an OpenAPI document or WireMock mappings. The
  operator imports them through the admin API and keeps them in sync.

Teams can then keep their mocks next to their application manifests (GitOps), and platform owners keep control
over images, secrets and security settings through operator policies.

- [Install](#install)
- [FastMock](#fastmock)
- [MockApi](#mockapi)
- [Status and conditions](#status-and-conditions)
- [Policies](#policies)
- [RBAC](#rbac)
- [Monitoring the operator](#monitoring-the-operator)
- [Upgrading](#upgrading)

## Install

```sh
helm upgrade --install fastmock-operator deploy/helm/fastmock-operator -n fastmock-system --create-namespace \
  --set image.repository=registry.example.com/platform/fastmock-operator --set image.tag=0.1.0 \
  --set server.image.repository=registry.example.com/platform/fastmock --set server.image.tag=0.1.0
```

Helm installs the CRDs (`fastmocks.fastmock.franshub.io`, `mockapis.fastmock.franshub.io`, version `v1alpha1`) on first install.

| Value | Default | Meaning |
|---|---|---|
| `image.repository`, `image.tag`, `image.pullPolicy` | `fastmock-operator`, chart `appVersion`, `IfNotPresent` | Operator image. |
| `imagePullSecrets` | `[]` | For the operator pod. |
| `server.image.repository`, `server.image.tag` | `fastmock`, chart `appVersion` | Default FastMock image for `FastMock`s without `spec.image` (`FASTMOCK_OPERATOR_SERVER_IMAGE`). |
| `server.allowedImagePrefixes` | `[]` (the repository of the default image) | Image prefixes a `FastMock` may use (`FASTMOCK_OPERATOR_ALLOWED_IMAGE_PREFIXES`). |
| `watchNamespace` | `""` (all namespaces) | Watch only one namespace; the operator then gets a Role instead of a ClusterRole. |
| `logLevel` | `info` | Operator log level. |
| `resources` | requests `50m`/`64Mi`, limits `500m`/`256Mi` | |
| `metrics.serviceMonitor.enabled`, `.interval` | `false`, `30s` | ServiceMonitor for the operator's `/metrics`. |
| `podSecurityContext`, `securityContext` | non-root 65532, read-only root FS, no capabilities | |
| `nodeSelector`, `tolerations`, `affinity` | empty | |

The operator runs one replica (`Recreate` strategy) and serves `/healthz`, `/readyz` and `/metrics` on port 8081.

## FastMock

```yaml
apiVersion: fastmock.franshub.io/v1alpha1
kind: FastMock
metadata:
  name: payments
  namespace: team-payments
spec:
  replicas: 2
  storage:
    mode: postgres
    database: {name: payments-mock-storage, key: database-url}
    valkey: {name: payments-mock-storage, key: valkey-url}
  adminTokensSecret: {name: payments-mock-tokens, key: tokens}
  secretsSecret: payments-mock-hook-secrets
  env:
    FASTMOCK_STUB_SECRETS: delivery-hook
    FASTMOCK_JOURNAL_RETENTION: 6h
  otelEndpoint: http://otel-collector.observability.svc:4318
  resources:
    requests: {cpu: 100m, memory: 128Mi}
    limits: {cpu: "1", memory: 512Mi}
  route:
    hostnames: [payments-mock.example.com]
    parentRefs:
      - {name: internal-gateway, namespace: gateway-system}
```

Every Secret it references must carry the label `fastmock.franshub.io/allow-mount: "true"` (see [Policies](#policies)):

```sh
kubectl -n team-payments create secret generic payments-mock-storage \
  --from-literal=database-url='postgres://fastmock:<password>@db.example.com:5432/payments_mock?sslmode=verify-full' \
  --from-literal=valkey-url='redis://:<password>@valkey.example.com:6379/'
kubectl -n team-payments label secret payments-mock-storage fastmock.franshub.io/allow-mount=true
```

### Fields

| Field | Default | Meaning |
|---|---|---|
| `image` | operator default image | FastMock image; must match an allowed prefix. |
| `replicas` | `1` | Replicas. More than 1 requires `storage.mode: postgres`. A PodDisruptionBudget (`minAvailable: 1`) is created when `replicas > 1`. |
| `imagePullSecrets` | `[]` | Names of pull Secrets (each must be labelled). |
| `storage.mode` | `memory` | `memory` or `postgres`. Omitting the whole `storage` block means `memory`. |
| `storage.database` | none | `{name, key}` of a Secret holding the PostgreSQL URL (`key` defaults to `url`). Required for `postgres`. |
| `storage.valkey` | none | `{name, key}` of a Secret holding the Valkey URL (`key` defaults to `url`). Required for `postgres`. |
| `storage.valkeyPrefix` | `fastmock:<name>:` | Valkey key prefix, so several servers can share one Valkey. |
| `adminTokensSecret` | none | `{name, key}` of a Secret with your `name:role:sha256hex` token lines. `key` defaults to `tokens`. |
| `secretsSecret` | none | Secret mounted at `/etc/fastmock/secrets` (HMAC keys, plugin secrets). |
| `plugins` | `[]` (all) | Plugins to enable. |
| `pluginSettings` | `{}` | `plugin → {key: value}`, rendered as `FASTMOCK_PLUGIN_<PLUGIN>_<KEY>`. |
| `env` | `{}` | Extra [settings](configuration.md); security-relevant variables are refused (see [Policies](#policies)). |
| `otelEndpoint` | none | OTLP endpoint; sets `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME=fastmock-<name>`. |
| `resources` | requests `100m`/`64Mi`, limits `2`/`512Mi` | Container resources. |
| `route` | none | `{hostnames, parentRefs}`: creates an HTTPRoute `<name>` for the **data** port. Removing the field deletes the route. |

### What the operator creates

All objects are named after the `FastMock`, owned by it (deleted with it) and labelled
`app.kubernetes.io/managed-by: fastmock-operator`.

| Object | Name | Content |
|---|---|---|
| Secret | `<name>-operator-token` | The operator's own admin token (generated once). |
| Secret | `<name>-admin-tokens` | Your token lines plus `fastmock-operator:admin:<hash>`, mounted as `FASTMOCK_ADMIN_TOKENS_FILE`. |
| ConfigMap | `<name>-config` | Environment: listeners, storage mode, log format, Valkey prefix, plugins, OTLP, `env`. |
| ServiceAccount | `<name>` | Without token automount. |
| Deployment | `<name>` | Hardened pod (non-root 65532, read-only root FS, no capabilities, seccomp), probes on `/healthz` and `/readyz`, rolling update with `maxUnavailable: 0`. A checksum of configuration and tokens rolls the pods when they change. |
| Service | `<name>` (port 8080), `<name>-admin` (port 9090) | Data and admin ports. |
| PodDisruptionBudget | `<name>` | When `replicas > 1`. |
| HTTPRoute | `<name>` | When `route` is set (needs Gateway API CRDs). |

The status shows the in-cluster URLs: `status.adminUrl` = `http://<name>-admin.<namespace>.svc:9090`,
`status.dataUrl` = `http://<name>.<namespace>.svc:8080`.

```sh
kubectl -n team-payments get fastmocks
```

```text
NAME       READY   STORAGE    AGE
payments   2       postgres   5m
```

## MockApi

```yaml
apiVersion: fastmock.franshub.io/v1alpha1
kind: MockApi
metadata:
  name: billing
  namespace: team-payments
spec:
  serverRef: {name: payments}
  openapi:
    configMapRef: {name: billing-openapi, key: openapi.yaml}
    pathPrefix: /billing
    validateRequests: true
  wiremock:
    configMapRef: {name: billing-wiremock, key: mappings.json}
  stubs:
    - name: billing health
      request: {method: GET, urlPath: /billing/health}
      response: {status: 200, body: ok}
    - name: invoice paid webhook
      request: {method: POST, urlPathTemplate: "/billing/invoices/{id}/pay"}
      response: {status: 202}
      callbacks:
        - url: http://billing-app.team-payments.svc:8080/hooks/invoice
          jsonBody: {invoiceId: "{{request.pathParams.id}}", status: PAID}
          correlation: "{{request.pathParams.id}}"
```

| Field | Default | Meaning |
|---|---|---|
| `serverRef.name` | required | `FastMock` in the same namespace. |
| `tag` | `<namespace>.<name>` | Tag of the imported stubs. |
| `stubs` | `[]` | Stub objects exactly as in the admin API ([Concepts: stubs](../guide/concepts.md#stubs)). |
| `openapi.inline` or `openapi.configMapRef` | none | OpenAPI 3 document (JSON or YAML) inline or from a ConfigMap key. |
| `openapi.pathPrefix` | `""` | Prefix added to every generated path. |
| `openapi.validateRequests` | `true` | Generated stubs reject requests that violate the document. |
| `wiremock.inline` or `wiremock.configMapRef` | none | WireMock mappings, JSON or YAML. |

How it is applied:

- Inline stubs are imported with tag `<tag>`, the OpenAPI stubs with `<tag>:openapi`, the WireMock stubs with
  `<tag>:wiremock`. Each import replaces the previous stubs of its tag, so removing a stub from the resource
  removes it from the server; removing a whole section deletes its tag. Details of the conversions:
  [Importing](../guide/importing.md), [WireMock and MockServer import](../guide/import-wiremock-mockserver.md).
- The operator imports only when the effective input changed: the spec, the content of the referenced
  ConfigMaps, or the server (a re-created `FastMock` gets a new UID). Every 5 minutes it re-reads the
  ConfigMaps and checks that the server still has the expected number of stubs (for example after a memory-mode
  server restarted); missing stubs are re-imported.
- Deleting a `MockApi` deletes its stubs. The finalizer `fastmock.franshub.io/stubs` keeps the resource until the removal
  succeeded, as long as the `FastMock` exists; if the server is gone, the finalizer is released without
  cleanup.

```sh
kubectl -n team-payments get mockapis
```

```text
NAME      SERVER     STUBS   SYNCED
billing   payments   14      True
```

## Status and conditions

### FastMock

| Field | Meaning |
|---|---|
| `status.readyReplicas` | Ready pods of the Deployment. |
| `status.observedGeneration` | Generation the status refers to. |
| `status.adminUrl`, `status.dataUrl` | In-cluster URLs. |
| `status.conditions` | See below. |

| Condition | Status | Reason | Meaning |
|---|---|---|---|
| `Ready` | True | `Available` | At least one replica is ready (`message`: `2/2 replicas ready`). |
| `Ready` | False | `Progressing` | Applied, no replica ready yet. |
| `Ready` | False | `InvalidSpec` | `storage.mode` is not `memory`/`postgres`, `postgres` without `database` and `valkey`, or more than one replica with memory storage. |
| `Ready` | False | `ImageNotAllowed` | The image does not match an allowed prefix. |
| `Ready` | False | `EnvNotAllowed` | `spec.env` sets a variable that is reserved for dedicated fields. |
| `Ready` | False | `SecretNotAllowed` | A referenced Secret lacks the label `fastmock.franshub.io/allow-mount: "true"`. |
| `Ready` | False | `SecretNotFound` | A referenced Secret does not exist. |
| `RouteReady` | True / False | `Applied` / `ApplyFailed` | Only when `route` is set; `ApplyFailed` usually means Gateway API is not installed. |

When a resource is refused (`InvalidSpec`, `ImageNotAllowed`, `EnvNotAllowed`, `SecretNotAllowed`,
`SecretNotFound`) **nothing is applied** and an existing Deployment keeps running unchanged. Secret problems are
re-checked every minute, the others when the resource changes and every 5 minutes.

```sh
kubectl -n team-payments get fastmock payments -o jsonpath='{range .status.conditions[*]}{.type}={.status} {.reason}: {.message}{"\n"}{end}'
```

```text
Ready=False SecretNotAllowed: referenced Secrets must carry the label fastmock.franshub.io/allow-mount=true: storage.database=payments-mock-storage
```

### MockApi

| Field | Meaning |
|---|---|
| `status.importedStubs` | Stubs imported in the last successful sync. |
| `status.warnings` | Conversion warnings of the OpenAPI/WireMock importers. |
| `status.lastSyncTime` | Time of the last successful import. |
| `status.appliedHash` | Hash of the applied input (spec, ConfigMap contents, server UID). |
| `status.observedGeneration` | |

| Condition | Status | Reason | Meaning |
|---|---|---|---|
| `Synced` | True | `Synced` | `14 stubs applied with tag team-payments.billing` |
| `Synced` | False | `ServerNotReady` | The `FastMock` is missing, being deleted or has no ready replica; retried every 15 s. |
| `Synced` | False | `SyncFailed` | The import failed: invalid stub, unparsable document, missing ConfigMap or key, admin API error. The message tells which. |

## Policies

`FastMock` resources are written by application teams, while the operator runs with broad permissions. To keep
the operator from being used to run arbitrary images, read arbitrary Secrets or weaken FastMock's security
settings, it enforces three policies:

1. **Image allow-list.** `spec.image` (or the default image) must start with one of
   `server.allowedImagePrefixes`; the prefix must end at a path, tag or digest boundary, so
   `registry.example.com/fastmock` allows `registry.example.com/fastmock:1.2` and
   `registry.example.com/fastmock@sha256:...` but not `registry.example.com/fastmock-other`. Without explicit
   prefixes only the repository of the default image is allowed. Otherwise: `ImageNotAllowed`.
2. **Secret opt-in.** Every Secret named in `storage.database`, `storage.valkey`, `adminTokensSecret`,
   `secretsSecret` and `imagePullSecrets` must have the label `fastmock.franshub.io/allow-mount: "true"`. A Secret owner
   thereby agrees that FastMock pods may mount it. Otherwise: `SecretNotAllowed`.
3. **Reserved environment.** `spec.env` must not set `FASTMOCK_AUTH_DISABLED`, `FASTMOCK_ADMIN_TOKENS`,
   `FASTMOCK_ADMIN_TOKENS_FILE`, `FASTMOCK_ADMIN_ALLOWED_ORIGINS`, `FASTMOCK_SECRETS_DIR`, `FASTMOCK_SECRET_*`,
   `FASTMOCK_CALLBACK_DENIED_CIDRS`, `FASTMOCK_CALLBACK_ALLOWED_HOSTS`, `FASTMOCK_DATABASE_URL`,
   `FASTMOCK_VALKEY_URL`, `FASTMOCK_VALKEY_PREFIX`, `FASTMOCK_STORAGE`, `FASTMOCK_LISTEN`,
   `FASTMOCK_ADMIN_LISTEN` or any `LD_*` variable (names are compared case-insensitively). Use the dedicated
   fields instead. Otherwise: `EnvNotAllowed`.

Combine these with a namespace-level policy engine (Kyverno, Gatekeeper) or `watchNamespace` if only some
namespaces should get mock servers.

## RBAC

The chart grants the operator (as a ClusterRole, or a Role in `watchNamespace`):

| API group | Resources | Verbs |
|---|---|---|
| `fastmock.franshub.io` | `fastmocks` | get, list, watch |
| `fastmock.franshub.io` | `mockapis` | list, watch, patch |
| `fastmock.franshub.io` | `fastmocks/status`, `mockapis/status` | patch |
| `fastmock.franshub.io` | `fastmocks/finalizers` | update |
| core | `secrets`, `configmaps` | get, create, patch |
| core | `services`, `serviceaccounts` | create, patch |
| `apps` | `deployments` | get, list, watch, create, patch |
| `policy` | `poddisruptionbudgets` | get, create, patch, delete |
| `gateway.networking.k8s.io` | `httproutes` | get, create, patch, delete |

It cannot list Secrets, only read those it is pointed at, and the Secret-label policy limits which of those it
mounts. Users need RBAC on `fastmocks` and `mockapis` in their namespaces; reading the generated
`<name>-operator-token` Secret is equivalent to admin access to that FastMock, so do not grant `get secrets` more
widely than needed.

## Monitoring the operator

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_operator_reconciles_total` | `kind` (`FastMock`, `MockApi`), `result` (`ok`, `error`) | Reconciliations. |
| `fastmock_operator_reconcile_duration_seconds` | `kind` | Reconciliation time. |

Logs are JSON; a refused resource logs `FastMock refused` with `reason` and `msg`. Failed reconciliations are
retried with backoff from 5 s up to 5 minutes. The [Grafana dashboard](observability.md#grafana-dashboard) has an
operator panel.

## Upgrading

Helm installs CRDs only on the first install and never upgrades them. After upgrading the chart, apply the CRDs
yourself:

```sh
helm upgrade fastmock-operator deploy/helm/fastmock-operator -n fastmock-system -f values.yaml
kubectl apply --server-side -f deploy/helm/fastmock-operator/crds/fastmock.franshub.io.yaml
```

The CRD file is generated from the operator code (`fastmock-operator crd`), so it always matches the operator
version of the same chart. Existing `FastMock`s are re-applied with the new operator's manifests; their pods
roll only if the generated Deployment changed. Changing the default server image (`server.image.tag`) rolls
every `FastMock` that does not pin `spec.image`.

Uninstalling the chart leaves the CRDs and therefore all `FastMock`/`MockApi` resources and their objects in
place. Delete the resources first if you want everything removed; deleting the CRDs deletes all resources of
those kinds.
