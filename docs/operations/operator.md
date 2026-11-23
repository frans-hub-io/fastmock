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
| `policy.spaceFromNamespace` | `false` | `FASTMOCK_OPERATOR_SPACE_FROM_NAMESPACE`: put the stubs of every `MockApi` into the space named like its namespace; a `MockApi` with another `spec.space` fails with `SyncFailed`. |
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
  access:
    configMapRef: {name: payments-mock-access}
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

Every Secret it references, and the access ConfigMap, must carry the label `fastmock.franshub.io/allow-mount: "true"`
(see [Policies](#policies)):

```sh
kubectl -n team-payments create secret generic payments-mock-storage \
  --from-literal=database-url='postgres://fastmock:<password>@db.example.com:5432/payments_mock?sslmode=verify-full' \
  --from-literal=valkey-url='redis://:<password>@valkey.example.com:6379/'
kubectl -n team-payments label secret payments-mock-storage fastmock.franshub.io/allow-mount=true
```

### Access file

`spec.access.configMapRef` names a ConfigMap in the same namespace that holds the server's
[access file](access-control.md#the-access-file) (custom roles, spaces, bindings) under `key` (default
`access.yaml`). The operator mounts it read-only at `/etc/fastmock/access/access.yaml` and sets
`FASTMOCK_ACCESS_FILE`; that is what makes `MockApi.spec.space` and space-scoped tokens usable on
operator-managed servers.

```sh
kubectl -n team-payments create configmap payments-mock-access --from-file=access.yaml
kubectl -n team-payments label configmap payments-mock-access fastmock.franshub.io/allow-mount=true
```

```yaml
spaces:
  - name: team-payments
    pathPrefixes: [/payments/]
    capture:
      kafkaTopics: ["payments.*"]
  - name: team-billing
    pathPrefixes: [/billing/]
    capture:
      kafkaTopics: ["billing.*"]
      amqpExchanges: [billing]
bindings:
  - subject: token:ci-billing
    role: editor
    spaces: [team-billing]
```

Per-space capture allow-lists are part of the same file: `spaces[].capture` (`kafkaTopics`, `mqttTopics`,
`amqpExchanges`) narrows the server-wide `FASTMOCK_CAPTURE_*` lists (set through `env`) for the
[capture sources](../guide/message-capture.md#allow-lists-per-space) of that space, so each team can read only its
own topics through the shared broker credentials.

The content is part of the pod template checksum, so editing the ConfigMap rolls the pods. The operator watches
labelled ConfigMaps, so an edit applies right away, without waiting for the periodic resync. An invalid access file stops the new pods at
start-up with a message naming the entry (`Ready=False Progressing`, see the pod log), while the old pods keep
running. `token:` bindings must name entries of `adminTokensSecret`; the operator's own token
(`fastmock-operator`) is always a global admin.

### OIDC sign-in

`spec.oidc` turns on [OIDC sign-in](access-control.md#oidc-sign-in-and-bearer-jwts) for the server's UI and,
with `apiAudience`, bearer JWTs on its admin API. The client secret comes from a Secret in the same namespace
(labelled like every other referenced Secret); the operator mounts it read-only at
`/etc/fastmock/oidc/client-secret`. Bind the provider's groups in the [access file](#access-file).

```yaml
spec:
  access:
    configMapRef: {name: payments-mock-access}
  oidc:
    issuer: https://sso.example.com/realms/mocks
    clientId: payments-mock
    clientSecretRef: {name: payments-mock-oidc, key: client-secret}
    redirectUrl: https://payments-mock-admin.example.com/__admin/auth/callback
    groupsClaim: groups
    apiAudience: fastmock-api
    sessionIdle: 30m
    caConfigMapRef: {name: corporate-ca, key: ca.crt}
```

```sh
kubectl -n team-payments create secret generic payments-mock-oidc --from-literal=client-secret="$CLIENT_SECRET"
kubectl -n team-payments label secret payments-mock-oidc fastmock.franshub.io/allow-mount=true
```

| Field | Default | Meaning |
|---|---|---|
| `issuer` | required | `FASTMOCK_OIDC_ISSUER`; must start with `https://`. |
| `clientId` | required | `FASTMOCK_OIDC_CLIENT_ID`. |
| `clientSecretRef` | required | `{name, key}` of the Secret with the client secret; `key` defaults to `client-secret`. |
| `redirectUrl` | required | Public https admin URL followed by `/__admin/auth/callback`. |
| `scopes` | `openid profile email` | `FASTMOCK_OIDC_SCOPES`. |
| `usernameClaim`, `groupsClaim` | `preferred_username`, `groups` | Claims for `user:` and `group:` bindings; `sub` (or `oid` with Entra ID) is recommended for `user:` bindings ([choosing the username claim](access-control.md#choosing-the-username-claim)). |
| `apiAudience` | none | Accept the provider's JWTs with this audience on the admin API; must not be empty and must differ from `clientId`. Leave it out to turn bearer JWTs off. |
| `sessionTtl`, `sessionIdle` | `8h`, `1h` | Session lifetimes. |
| `allowedHosts` | `[]` | Hosts besides the issuer's that discovery endpoints may use. |
| `caConfigMapRef` | none | `{name, key}` of a labelled ConfigMap with extra CA certificates for the provider (`key` defaults to `ca.crt`), mounted at `/etc/fastmock/oidc-ca`. |

An `issuer` or `redirectUrl` that is not https, a missing `clientId`, a redirect URL not ending in
`/__admin/auth/callback`, or an `apiAudience` that is empty or equal to `clientId` refuse the spec (`InvalidSpec`,
with the message `oidc.apiAudience must not be empty: remove it to turn bearer JWTs off` or
`oidc.apiAudience must differ from oidc.clientId` for the audience); a missing or unlabelled Secret or CA ConfigMap gives
`SecretNotFound`, `SecretNotAllowed`, `ConfigMapNotFound` or `ConfigMapNotAllowed`, and a client secret Secret
without the configured key, or with an empty value, gives `SecretKeyNotFound`. The server checks the rest at
start-up. `FASTMOCK_OIDC_*` cannot be set through `spec.env`. The admin port is not published by `spec.route`,
which serves the data port; expose the admin Service with your own HTTPRoute on the host of `redirectUrl`. The
client secret is part of the pod template checksum: updating it in the Secret rolls the pods at the next
reconcile, that is within the 5-minute resync or at once when the `FastMock` changes. With `storage.mode: memory` sessions live in the single pod.

### Mail

`spec.mail` turns on [mail capture](../guide/mail.md): SMTP, IMAP and POP3 listeners, the provider APIs and the
mail admin API. The operator sets the `FASTMOCK_MAIL_*` variables, adds the container ports and creates the
Service `<name>-mail`.

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
  mail:
    enabled: true
    ports:
      smtp: 2525
      imap: 1143
    tlsSecretRef: {name: payments-mock-mail-tls}
    limits:
      maxAge: 1d
```

| Field | Default | Meaning |
|---|---|---|
| `mail.enabled` | `false` | `FASTMOCK_MAIL_ENABLED`. |
| `mail.ports.smtp` | `2525` | SMTP port; `0` turns it off. |
| `mail.ports.smtps`, `.imap`, `.imaps`, `.pop3`, `.pop3s` | `0` (off) | The other listeners; the implicit-TLS ones need `tlsSecretRef`. |
| `mail.hostname` | `localhost` | `FASTMOCK_MAIL_HOSTNAME`. |
| `mail.tlsSecretRef` | none | `{name, certKey, keyKey}` of a Secret with the certificate and key (`tls.crt`, `tls.key` by default), mounted read-only. |
| `mail.requireAuth` | `false` | `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH`. |
| `mail.usersSecretRef` | none | `{name, key}` of a Secret with the `user:hash` entries of `FASTMOCK_MAIL_SMTP_USERS` (`pbkdf2-sha256$...` recommended, hex SHA-256 accepted; `key` defaults to `users`). |
| `mail.allowPlaintextAuth` | `false` | `FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH`. |
| `mail.limits` | server defaults | `maxMessageBytes`, `maxMessages`, `maxBytes`, `maxMailboxes`, `maxAge`, `maxConnections`, `maxConnectionsPerIp`, `loginFailureLimit`, `idleTimeout`, `sessionTimeout`, `imapIdleTimeout`, mapped to the `FASTMOCK_MAIL_*` variables of the same names (`loginFailureLimit` is `FASTMOCK_MAIL_LOGIN_FAILURE_LIMIT`, `0` turns it off; `maxBytes` must be at least `maxMessageBytes`). Other mail settings go through `env`. |
| `mail.service` | `ClusterIP` | `{type, annotations}` of the `<name>-mail` Service (`ClusterIP`, `NodePort` or `LoadBalancer`). |

Both Secrets need the label `fastmock.franshub.io/allow-mount: "true"` like the others ([Policies](#policies)), and
their content is part of the pod checksum, so a renewed certificate or a changed user list rolls the pods. Mail
settings the server would refuse (a mail port used twice or equal to 8080 or 9090, an implicit-TLS port without
`tlsSecretRef`, invalid limits) set `Ready=False` with reason `InvalidSpec` and a message naming the field, for
example `mail.ports.smtps needs a certificate: set mail.tlsSecretRef`. Mail domains of spaces go into the access
file (`mailDomains`, [Access file](#access-file)). With `storage.mode: postgres`, provider webhooks with a signing
key or a URL password need the secret `mail-webhook-key` (a long random value, the same for every replica) in the
Secret of `secretsSecret`.

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
| `access.configMapRef` | none | `{name, key}` of a ConfigMap holding the [access file](#access-file); `key` defaults to `access.yaml`. |
| `oidc` | none | [OIDC sign-in](#oidc-sign-in) for the UI and bearer JWTs for the admin API. |
| `plugins` | `[]` (all) | Plugins to enable. |
| `pluginSettings` | `{}` | `plugin → {key: value}`, rendered as `FASTMOCK_PLUGIN_<PLUGIN>_<KEY>`. |
| `env` | `{}` | Extra [settings](configuration.md); security-relevant variables are refused (see [Policies](#policies)). |
| `otelEndpoint` | none | OTLP endpoint; sets `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME=fastmock-<name>`. |
| `resources` | requests `100m`/`64Mi`, limits `2`/`512Mi` | Container resources. |
| `route` | none | `{hostnames, parentRefs}`: creates an HTTPRoute `<name>` for the **data** port. Removing the field deletes the route. |
| `mail` | off | [Mail capture](#mail): listeners, certificate, SMTP users, limits and the mail Service. |
| `mcp.enabled` | `true` | `FASTMOCK_MCP_ENABLED`: the [MCP endpoint](../guide/mcp.md) `/mcp` on the admin port. Without the `mcp` field the server default (on) applies. |

### What the operator creates

All objects are named after the `FastMock`, owned by it (deleted with it) and labelled
`app.kubernetes.io/managed-by: fastmock-operator`.

| Object | Name | Content |
|---|---|---|
| Secret | `<name>-operator-token` | The operator's own admin token (generated once). |
| Secret | `<name>-admin-tokens` | Your token lines plus `fastmock-operator:admin:<hash>`, mounted as `FASTMOCK_ADMIN_TOKENS_FILE`. |
| ConfigMap | `<name>-config` | Environment: listeners, storage mode, log format, Valkey prefix, plugins, OTLP, `env`. |
| ServiceAccount | `<name>` | Without token automount. |
| Deployment | `<name>` | Hardened pod (non-root 65532, read-only root FS, no capabilities, seccomp), probes on `/healthz` and `/readyz`, rolling update with `maxUnavailable: 0`. A checksum of configuration, tokens and the access file rolls the pods when they change. With `access`, the ConfigMap is mounted read-only at `/etc/fastmock/access`. |
| Service | `<name>` (port 8080), `<name>-admin` (port 9090) | Data and admin ports. |
| Service | `<name>-mail` | The enabled mail ports, when `spec.mail.enabled` is true. |
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
| `space` | none | [Space](access-control.md#spaces) of the server that the stubs go to; the server's [access file](#access-file) must define it. With the operator policy `spaceFromNamespace` the space is always the namespace of the `MockApi`. |

How it is applied:

- Inline stubs are imported with tag `<tag>`, the OpenAPI stubs with `<tag>:openapi`, the WireMock stubs with
  `<tag>:wiremock`. Each import replaces the previous stubs of its tag, so removing a stub from the resource
  removes it from the server; removing a whole section deletes its tag. Details of the conversions:
  [Importing](../guide/importing.md), [WireMock and MockServer import](../guide/import-wiremock-mockserver.md).
- The operator imports only when the effective input changed: the spec, the content of the referenced
  ConfigMaps, or the server (a re-created `FastMock` gets a new UID). Every 5 minutes it re-reads the
  ConfigMaps and checks that the server still has the expected number of stubs; missing stubs are re-imported.
- With `storage.mode: memory` the stubs live only in the pod, so a new pod (an access ConfigMap edit, a rotated
  Secret, an image change, a crash) starts empty. The operator follows the server's Deployment and recognises a
  server process by the `instanceId` it reports in `GET /__admin/info`. From the start of a rollout or restart
  until the stubs are confirmed on the new process, `Synced` is `False` with reason `ServerRestarting`. While the
  Deployment rolls out, the operator waits and records the instance being replaced in `status.replacedInstance`.
  Once only the new, ready pod is left, it keeps waiting, checking every 5 seconds, as long as the replaced pod
  still answers the admin Service while it shuts down. When a new instance answers, the operator imports the
  stubs into it, records it in `status.serverInstance` and checks again 5 seconds later. `Synced` becomes `True`
  (and `status.replacedInstance` is cleared) only when the answering instance is the one in
  `status.serverInstance` and it has the expected number of stubs; otherwise the operator imports again. This
  usually takes a few seconds after the new pod is ready. With `postgres` storage the stubs survive restarts and
  nothing is re-imported.
- With a space, every import goes to that space (`?space=` on the import calls) and replaces only that space's
  stubs of the tag; the space's [ownership rules](access-control.md#ownership-of-the-data-plane) apply. Changing
  the space moves the stubs to the new space. `status.space` records where the stubs were applied.
- A `MockApi` whose space is not defined in the server's access file is not synced: `Synced` is `False` with reason
  `SpaceNotFound`, and the stubs applied before stay on the server until the space exists or
  `spec.space` is corrected.
- Deleting a `MockApi` deletes its stubs by tag, in whatever space they are. The finalizer `fastmock.franshub.io/stubs` keeps the resource until the removal
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
| `Ready` | False | `InvalidSpec` | `storage.mode` is not `memory`/`postgres`, `postgres` without `database` and `valkey`, more than one replica with memory storage, or invalid `oidc` fields ([OIDC sign-in](#oidc-sign-in)), for example an empty `oidc.apiAudience`. |
| `Ready` | False | `ImageNotAllowed` | The image does not match an allowed prefix. |
| `Ready` | False | `EnvNotAllowed` | `spec.env` sets a variable that is reserved for dedicated fields. |
| `Ready` | False | `SecretNotAllowed` | A referenced Secret lacks the label `fastmock.franshub.io/allow-mount: "true"`. |
| `Ready` | False | `SecretNotFound` | A referenced Secret does not exist. |
| `Ready` | False | `SecretKeyNotFound` | The OIDC client Secret lacks the key named in `oidc.clientSecretRef`, or its value is empty. |
| `Ready` | False | `ConfigMapNotAllowed` | The access ConfigMap lacks the label `fastmock.franshub.io/allow-mount: "true"`. |
| `Ready` | False | `ConfigMapNotFound` | The access ConfigMap or its key does not exist. |
| `RouteReady` | True / False | `Applied` / `ApplyFailed` | Only when `route` is set; `ApplyFailed` usually means Gateway API is not installed. |

When a resource is refused (`InvalidSpec`, `ImageNotAllowed`, `EnvNotAllowed`, `SecretNotAllowed`,
`SecretNotFound`, `SecretKeyNotFound`, `ConfigMapNotAllowed`, `ConfigMapNotFound`) **nothing is applied** and an existing Deployment
keeps running unchanged. Secret and ConfigMap problems are re-checked every minute, the others when the resource
changes and every 5 minutes.

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
| `status.appliedHash` | Hash of the applied input (spec, ConfigMap contents, server UID, space). |
| `status.space` | Space the stubs were applied to; absent for the global area. |
| `status.serverInstance` | Memory storage only: instance id of the server process the stubs were imported into. |
| `status.replacedInstance` | Memory storage only, during a restart: instance id of the server process being replaced. |
| `status.observedGeneration` | |

| Condition | Status | Reason | Meaning |
|---|---|---|---|
| `Synced` | True | `Synced` | `14 stubs applied with tag team-payments.billing` |
| `Synced` | False | `ServerNotReady` | The `FastMock` is missing, being deleted or has no ready replica; retried every 15 s. |
| `Synced` | False | `ServerRestarting` | Memory storage: the server is rolling out or restarted, so the new process may not have the stubs yet. It stays until the stubs are imported into the new process and confirmed there. |
| `Synced` | False | `SyncFailed` | The import failed: invalid stub, unparsable document, missing ConfigMap or key, admin API error. The message tells which. |
| `Synced` | False | `SpaceNotFound` | `spec.space` (or the namespace with `spaceFromNamespace`) is not defined in the server's access file; the stubs applied before are left unchanged. |

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
   `secretsSecret`, `oidc.clientSecretRef`, `mail.tlsSecretRef`, `mail.usersSecretRef` and `imagePullSecrets` must have the label `fastmock.franshub.io/allow-mount: "true"`. A Secret owner
   thereby agrees that FastMock pods may mount it. Otherwise: `SecretNotAllowed`. The same label is required on
   the ConfigMaps named in `access.configMapRef` and `oidc.caConfigMapRef` (otherwise `ConfigMapNotAllowed`),
   since their content decides who may do what on the server and which identity provider it trusts.
3. **Reserved environment.** `spec.env` must not set `FASTMOCK_AUTH_DISABLED`, `FASTMOCK_ADMIN_TOKENS`,
   `FASTMOCK_ADMIN_TOKENS_FILE`, `FASTMOCK_ACCESS_FILE`, `FASTMOCK_ADMIN_ALLOWED_ORIGINS`, `FASTMOCK_SECRETS_DIR`, `FASTMOCK_SECRET_*`,
   `FASTMOCK_CALLBACK_DENIED_CIDRS`, `FASTMOCK_CALLBACK_ALLOWED_HOSTS`, `FASTMOCK_DATABASE_URL`,
   `FASTMOCK_VALKEY_URL`, `FASTMOCK_VALKEY_PREFIX`, `FASTMOCK_STORAGE`, `FASTMOCK_LISTEN`,
   `FASTMOCK_ADMIN_LISTEN`, `FASTMOCK_MAIL_ENABLED`, the `FASTMOCK_MAIL_*_LISTEN` variables,
   `FASTMOCK_MAIL_TLS_CERT_FILE`, `FASTMOCK_MAIL_TLS_KEY_FILE`, `FASTMOCK_MAIL_SMTP_USERS`, any `FASTMOCK_OIDC_*`
   or any `LD_*` variable (names are compared case-insensitively). Use the dedicated
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
| core | `secrets` | get, create, patch |
| core | `configmaps` | get, list, watch, create, patch |
| core | `services`, `serviceaccounts` | create, patch |
| `apps` | `deployments` | get, list, watch, create, patch |
| `policy` | `poddisruptionbudgets` | get, create, patch, delete |
| `gateway.networking.k8s.io` | `httproutes` | get, create, patch, delete |

It lists and watches ConfigMaps carrying the label `fastmock.franshub.io/allow-mount=true`, reading only their
metadata, so that changes to them reach the servers that use them at once. It watches only the Deployments it
manages (label `app.kubernetes.io/managed-by=fastmock-operator`), for the `FastMock` status and to re-import the
stubs of memory-mode servers after a restart. Secrets are only read by name: the
content of those a `FastMock` or `MockApi` points at, and the label policy limits which of those it mounts. Users need RBAC on `fastmocks` and `mockapis` in their namespaces; reading the generated
`<name>-operator-token` Secret is equivalent to admin access to that FastMock, so do not grant `get secrets` more
widely than needed.

**Spaces and RBAC.** The operator writes stubs with its own global admin token. With `policy.spaceFromNamespace`
off (the default) it writes into whatever space `spec.space` names, so Kubernetes RBAC on `mockapis` decides who
can write into which space of a server: anyone who may create or update a `MockApi` in the server's namespace can
target every space of that server. Grant `create`, `update` and `patch` on `mockapis` in that namespace only to
people and pipelines that may change all of its spaces, or turn on `policy.spaceFromNamespace` to pin every
`MockApi` to the space named like its namespace. Teams that need their own spaces on a shared server without
access to each other's can use the admin API with [space-bound tokens](access-control.md#bindings) instead of
`MockApi` resources.

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
every `FastMock` that does not pin `spec.image`. When the new server image adds access control, read its
[upgrade notes](access-control.md#upgrade-notes), and set `spec.access` or `spec.oidc` only after every replica
runs the new image. The same applies to `spec.mail`: apply the new CRD first (older CRDs drop the field), let every
replica run an image with mail, then enable it; see the [mail upgrade notes](helm.md#upgrade-notes) for the
`editor` role and the Valkey ACL.

Uninstalling the chart leaves the CRDs and therefore all `FastMock`/`MockApi` resources and their objects in
place. Delete the resources first if you want everything removed; deleting the CRDs deletes all resources of
those kinds.
