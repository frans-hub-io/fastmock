# Security

FastMock runs in test environments next to real services, receives traffic that may contain real credentials,
and sends requests into internal networks. It is built to be safe by default there. This page describes the
security model and how to harden a deployment. To report a vulnerability, see [SECURITY.md](https://github.com/frans-hub-io/fastmock/blob/main/SECURITY.md).

- [Model](#model)
- [Admin tokens and roles](#admin-tokens-and-roles)
- [Managed tokens](#managed-tokens)
- [Rotating tokens](#rotating-tokens)
- [Browsers, origins and the UI](#browsers-origins-and-the-ui)
- [TLS and mTLS](#tls-and-mtls)
- [Storage connections](#storage-connections)
- [Egress policy](#egress-policy)
- [Secrets](#secrets)
- [Redaction](#redaction)
- [Template sandbox](#template-sandbox)
- [Resource limits](#resource-limits)
- [Audit log](#audit-log)
- [Containers and Kubernetes](#containers-and-kubernetes)
- [Supply chain](#supply-chain)
- [Hardening checklist](#hardening-checklist)

## Model

| Principle | How |
|---|---|
| The system under test cannot change its mocks | Mocks and the control plane are on different ports; no admin route exists on the data port. |
| Every control-plane call is authenticated | Bearer tokens bound to roles (sets of permissions), compared as SHA-256 hashes in constant time. There is no anonymous mode except an explicit local-development switch. |
| Security settings are not runtime state | Static tokens, roles, spaces, bindings, origins, egress policy, secrets, storage and listeners come from the environment at start-up and cannot be changed through the API or the UI. Managed tokens are the exception: they can be created through the API, but never with more permissions than their creator, and they always expire. |
| Mock definitions are data, never code | Templates are a closed language without file, environment, secret or network access; imports never execute anything. |
| Outgoing traffic follows one policy | Callbacks, proxy stubs and recording use one HTTP client with one egress policy. |
| What is stored is masked | Secrets in headers, query strings and bodies are masked before they reach the journal, the callbacks API, the UI or logs. |
| Every input is bounded | Sizes, counts, nesting depth, regex size, template output and time are limited. |
| Every change is audited | Admin changes are recorded with principal, principal kind, role, space, action and details. |

The data plane is intentionally unauthenticated: it imitates other services, and those services' credentials
are part of what the mocks match on. Restrict who can reach it with network policies.

## Admin tokens and roles

Tokens are random strings; FastMock stores only their SHA-256 hash:

```sh
fastmock hash-token --generate      # new token: prints "token: fm_..." and "hash:  ..."
fastmock hash-token 'existing-token'  # prints only the hex hash, ready for a token entry
```

Configure entries as `name:role:sha256hex` in `FASTMOCK_ADMIN_TOKENS` or, preferably, one per line in the file
named by `FASTMOCK_ADMIN_TOKENS_FILE` (the Helm chart and the operator mount it from a Secret). The `name`
appears in the audit log, logs and `GET /__admin/info`, so give every client its own token: `ci-payments`,
`ops-alice`, `grafana`.

| Role | May |
|---|---|
| `viewer` | Read everything: stubs (with credentials masked, see [Redaction](#redaction)), journal, callbacks, scenarios, key-value entries, imports, recordings, plugins, info, contract-test verdicts; run read-only queries such as request counts and near-miss analysis. |
| `editor` | Everything a viewer may, plus every change a test makes: stubs, imports, journal and callback cleanup, scenario states, key-value entries, chaos rules, recordings, gRPC descriptors, plugin admin calls. |
| `admin` | Everything, plus deleting all stubs at once, `POST /__admin/reset`, reading the audit log and starting contract tests that send server-side secrets (`secretHeaders`, the global permission `contracts:secrets`; an `admin` bound only in spaces does not get it). |

The roles are sets of permissions; custom roles, spaces and space-limited bindings come from the access file
([Access control](access-control.md)). Without it, `GET` and `HEAD` need `viewer`, other methods `editor`, unless an endpoint says otherwise (the
[Admin API](../guide/admin-api.md) lists the role of each endpoint). Responses: `401 {"error":"unauthorized","details":[]}`
with `WWW-Authenticate: Bearer` for a missing or wrong token, `403 {"error":"forbidden","details":["requires role editor"],"required":"editor"}`
for a role that is too low (principals with custom roles or space bindings get the missing permission in `required`
instead, for example `"required":"files:read"`). With an access file or OIDC configured, every `403` has this
`forbidden` shape ([Access control: errors](access-control.md#errors)). Static and managed tokens longer than 512
characters are rejected without hashing; bearer JWTs follow their own limits
([Access control: bearer JWTs](access-control.md#bearer-jwts-on-the-admin-api)).

Editors can change everything that tests change, including which callbacks go where (within the egress policy).
Give `editor` to automated test clients and the people who maintain mocks, `viewer` to dashboards and readers,
`admin` to operators. Teams that share one instance and must not affect each other get [spaces](access-control.md#spaces):
tokens bound to a space see and change only that space's stubs, journal, state, files, callbacks and contract
tests. Give each space the `pathPrefixes` or `hosts` of its traffic ([ownership](access-control.md#ownership-of-the-data-plane)):
its stubs then stay inside them, and requests there are answered only by its own and global stubs. Teams that need separate
recordings, chaos rules, gRPC descriptors or Kafka settings still need separate FastMock instances (the
[operator](operator.md) makes that cheap). Body files in the global area are shared with every space, because
stubs fall back to them ([Spaces](access-control.md#spaces)): keep a team's files in the team's space, and keep
only files meant for every team in the global area.

`FASTMOCK_AUTH_DISABLED=true` turns authentication off for a laptop. The server logs a warning, and the operator
refuses the variable. [Managed tokens](#managed-tokens) cannot be created or revoked in this mode.

## Managed tokens

[Managed tokens](access-control.md#managed-tokens) are created through `POST /__admin/tokens` and need no restart.
Their life cycle:

| Stage | What happens |
|---|---|
| Creation | Only a caller holding `tokens:write` and every permission of the requested role in each requested scope can create one; a caller bound to spaces cannot create a global token. The value (`fm_` and 64 random hex digits) is returned once; only its SHA-256 hash is stored. Name, role, scopes and expiry are audited (`token.create`), the value never. |
| Use | Static tokens are checked first; a value in the managed format is then looked up by hash. Each replica caches lookups for at most 10 seconds. The last use is recorded at most once per minute. |
| Expiry | Mandatory: by default 90 days, or `FASTMOCK_TOKENS_MAX_DAYS` when lower; at most `FASTMOCK_TOKENS_MAX_DAYS` (365 by default). A token created by a managed token never outlives its creator. Checked on every request; expired tokens get `401` with `"details":["token expired"]` and count as `fastmock_admin_auth_failures_total{reason="expired"}`. |
| Revocation | `DELETE /__admin/tokens/{id}` by a caller with `tokens:write` in every scope of the token, or by the token itself. Effective at once on the replica that handled it and within 10 seconds on all others. Revoking a token also revokes the tokens it created. Audited as `token.revoke`; later uses count as `reason="token"`. |
| Clean-up | Revoked and expired tokens stay visible for 90 days, then they are deleted. |

Prefer managed tokens with short lifetimes for CI pipelines and people, scoped to the spaces they work in; keep
static tokens for the few clients that must survive a database loss (the operator, monitoring). In memory mode
managed tokens are lost on restart. Tokens created by an SSO user keep working after the user leaves or loses
its groups; the token list shows the creator, so revoke a person's tokens as part of off-boarding. Watch
`fastmock_api_tokens_active` (a cluster-wide count, read it with `max`) for tokens piling up.

## OIDC sign-in

With [OIDC sign-in](access-control.md#oidc-sign-in-and-bearer-jwts) people use the UI with their identity provider
account, and their groups decide what they may do. Design points:

- **No tokens in the browser.** The admin port runs the authorization code flow itself (backend for frontend)
  with `state`, `nonce` and PKCE S256 and a confidential client. The browser holds only an opaque session cookie
  (`__Host-fastmock_session`: `Secure`, `HttpOnly`, `SameSite=Lax`, host-only). The session record in the hot
  store is keyed by the SHA-256 of the cookie value, so a copy of Valkey does not contain usable cookies.
- **Login integrity.** Each `state` is single-use and expires after 10 minutes; a login cookie ties the callback
  to the browser that started the login (no login CSRF); `return` accepts only relative paths under `/ui/` (no
  open redirect). Starting logins is rate-limited per replica.
- **Token validation.** Signatures are checked against the provider's JWKS with RS256, PS256, ES256 or EdDSA only;
  `none`, HMAC and `crit` headers are rejected, so a public key can never be used as an HMAC secret. `iss`, `aud`,
  `azp`, `exp`, `nbf`, `iat` and `nonce` are checked with 60 seconds of skew. Unknown key ids trigger at most one
  JWKS refresh per minute. Bearer JWTs on the admin API need their own audience (`FASTMOCK_OIDC_API_AUDIENCE`,
  different from the client id), and tokens carrying a `nonce` (ID tokens) are not accepted there.
- **Stable user identifiers.** Bind users by `sub` (or `oid` with Entra ID) rather than by `preferred_username`
  or `email`; with `email`, sign-in requires `email_verified: true`
  ([choosing the username claim](access-control.md#choosing-the-username-claim)).
- **CSRF.** Cookie-authenticated changes need `X-FastMock-Request: 1`, which a cross-site page cannot send without
  a CORS preflight that the admin port never grants, on top of the `Origin` check.
- **No implicit access.** A user without a binding gets no session (and `403` with a bearer JWT); there is no
  default viewer role.
- **Separate host names.** Serve the admin UI on its own host name, not on the host name of the mock port:
  browsers do not separate cookies by port. FastMock also removes its session and login cookies from data-plane
  requests before matching.
- **Egress.** Provider calls go only to the issuer host and `FASTMOCK_OIDC_ALLOWED_HOSTS`, through a dedicated
  client with timeouts, a response size limit, no redirects elsewhere and none at all on the token endpoint call. A discovery document pointing elsewhere
  stops the start.
- **Logs and audit.** `auth.login`, `auth.login_failed` (with the reason) and `auth.logout` are audit events.
  Authorization codes, tokens, session ids, the client secret and the PKCE verifier are never logged.
- **Session lifetime.** Logging out deletes the session on every replica at once. A session keeps the groups
  captured at sign-in until its idle or absolute expiry, so set `FASTMOCK_OIDC_SESSION_IDLE` and
  `FASTMOCK_OIDC_SESSION_TTL` to how quickly group changes at the provider must take effect. Bearer JWTs are
  accepted until their `exp`; keep the provider's access token lifetime short for clients that call the admin API.

## Rotating tokens

Managed tokens rotate without a restart: create the new token, switch the client, revoke the old one. Static
token files are read at start-up. To rotate without downtime:

1. Add an entry for the new token next to the old one (`ci-payments-2026q4:editor:<new hash>`).
2. Roll the pods. With Helm: update the Secret, then `kubectl rollout restart deploy/<fullname>` (pods are
   replaced one by one, the old token keeps working). With the operator: update the Secret referenced by
   `adminTokensSecret`; the operator notices the change and rolls the Deployment.
3. Switch the clients to the new token.
4. Remove the old entry and roll again.

`fastmock_admin_auth_failures_total{reason="token"}` and `audit` log lines
(`admin request rejected: missing or invalid token`) show clients that still use a removed token. The operator's
own token lives in `<name>-operator-token`; delete that Secret to make the operator generate a new one.

## Browsers, origins and the UI

To keep web pages that a browser visits from calling an admin port the browser can reach (including through DNS
rebinding), FastMock refuses every admin request with an `Origin` header unless:

- the origin is listed in `FASTMOCK_ADMIN_ALLOWED_ORIGINS` (exact `scheme://host[:port]`), or
- the origin is the admin server itself (`Origin` equals `http(s)://` plus the request's `Host`), which is how
  the built-in UI calls the API.

Refusals answer `403 {"error":"forbidden","details":["origin not allowed"]}` with an access file or OIDC
configured, `403 {"error":"origin not allowed","details":[]}` with static tokens only, and count as
`fastmock_admin_auth_failures_total{reason="origin"}`. Non-browser clients do not send `Origin` and are not
affected.

The UI is a static application served from the admin port. With token login it authenticates every call with
the bearer token, which it keeps only in the tab's session storage. With [OIDC sign-in](#oidc-sign-in) it uses the
session cookie and sends the `X-FastMock-Request` header on changes. It is delivered
with a strict Content-Security-Policy (`default-src 'none'`, `script-src 'self'`, `frame-ancestors 'none'`, no
inline or third-party scripts) and renders data as text, never as HTML. Publish it only over TLS (see [Helm: Gateway API](helm.md#exposing-the-admin-ui-with-gateway-api)) and set
`FASTMOCK_ADMIN_ALLOWED_ORIGINS` to its public URL.

## TLS and mTLS

- **Data port:** `FASTMOCK_TLS_CERT_FILE` and `FASTMOCK_TLS_KEY_FILE` enable HTTPS with HTTP/2 and HTTP/1.1
  (rustls, TLS 1.2 and 1.3 only). `FASTMOCK_TLS_CLIENT_CA_FILE` enables client certificate verification;
  `FASTMOCK_TLS_CLIENT_AUTH=required` (default) rejects clients without a valid certificate. The SHA-256
  fingerprint of a verified client certificate is passed to stubs as `x-fastmock-client-cert-sha256`; a header
  of that name sent by a client is removed first, so it cannot be forged. TLS handshake failures are counted in
  `fastmock_tls_handshake_failures_total`.
- **Admin port:** plain HTTP; terminate TLS at the Gateway or ingress and keep the port reachable only from
  inside the cluster.
- **PostgreSQL and Valkey:** see [Storage connections](#storage-connections).
- **Outgoing:** callbacks, proxy stubs and recording to `https://` URLs verify the server certificate against
  the public Mozilla root certificates compiled into the binary (`webpki-roots`), plus the certificates in
  `FASTMOCK_OUTBOUND_CA_FILE` when it is set (Helm: `outboundCA.configMap`). Use it for receivers whose
  certificates are issued by your organisation's CA; certificate verification is never switched off.
- **Kafka and Schema Registry:** `FASTMOCK_KAFKA_SECURITY_PROTOCOL` selects TLS, SASL or both (`ssl`,
  `sasl_plaintext`, `sasl_ssl`) for publishing and for contract tests. Broker certificates are verified against
  the same public roots plus `FASTMOCK_KAFKA_TLS_CA_FILE` (also trusted by an `https://` registry); a client
  certificate can be presented for mTLS. SASL supports `PLAIN`, `SCRAM-SHA-256` and `SCRAM-SHA-512`; the
  registry supports HTTP basic authentication. Inconsistent settings stop the server at start-up
  ([Secure connections](../guide/kafka.md#secure-connections)). Broker and registry addresses come only from
  the configuration, never from stubs, so the egress policy below does not apply to them; scope them with the
  egress NetworkPolicy.

## Storage connections

- **PostgreSQL:** set `FASTMOCK_DATABASE_SSL_MODE=verify-full` and, for a private CA,
  `FASTMOCK_DATABASE_SSL_ROOT_CERT`. `prefer` and `require` encrypt but do not check the server certificate, so
  use `verify-full` wherever the network path is not fully trusted. For certificate authentication add
  `FASTMOCK_DATABASE_SSL_CERT` and `FASTMOCK_DATABASE_SSL_KEY` (the certificate's common name is the database
  user, as with PostgreSQL `cert` authentication).
- **Valkey:** use a `rediss://` URL; the server certificate and host name are always verified. Set
  `FASTMOCK_VALKEY_TLS_CA_FILE` for a private CA and `FASTMOCK_VALKEY_TLS_CERT_FILE` with
  `FASTMOCK_VALKEY_TLS_KEY_FILE` when the server requires client certificates.
- **Least privilege:** give FastMock its own database and user. With the default `FASTMOCK_DATABASE_MIGRATE=auto`
  the server user needs `CREATE` on the schema, even when no migration is pending. With `skip` it needs only
  `USAGE` on the schema and `SELECT`, `INSERT`, `UPDATE` and `DELETE` on its tables (including
  `_sqlx_migrations`); the schema owner is used only by the `fastmock migrate` Job
  ([Database](database.md#running-migrations-separately)). In Valkey use an ACL user limited to FastMock's key
  prefix and commands, with `FASTMOCK_VALKEY_USERNAME` and `FASTMOCK_VALKEY_PASSWORD`:

  ```text
  ACL SETUSER fastmock on >CHANGE-ME ~fastmock:* -@all +get +set +psetex +del +incrby +pexpire +mget +unlink +scan +zadd +zrangebyscore +zrem +evalsha +script|load +ping
  ```

  Add `+select` when the URL selects a database other than `0`. The key pattern must match
  `FASTMOCK_VALKEY_PREFIX`.
- Mount CA files, client certificates and keys from Secrets; FastMock reads them once at start-up, so a rotated
  certificate needs a restart.

## Egress policy

Everything FastMock sends on behalf of stubs (callbacks, proxy stubs, recording) goes through one HTTP client:

- schemes `http` and `https` only; user names and passwords in URLs are refused;
- redirects are never followed; proxy settings from the environment are ignored;
- `FASTMOCK_CALLBACK_ALLOWED_HOSTS`: when set, only these host names (`hooks.example.com`) or suffixes
  (`*.svc.cluster.local`) are contacted;
- `FASTMOCK_CALLBACK_DENIED_CIDRS`: address ranges that are never contacted. The default covers loopback,
  link-local (cloud metadata endpoints), unspecified and multicast addresses; IPv4 addresses embedded in IPv6 are
  checked as IPv4. The check runs on every resolved address inside the DNS resolver, so the address that is
  checked is the address that is connected to.

Recommended: set `FASTMOCK_CALLBACK_ALLOWED_HOSTS` to the services your mocks call back, and add a Kubernetes
egress NetworkPolicy with the same scope ([Helm: NetworkPolicy](helm.md#networkpolicy)). Add internal ranges
such as the cluster's service and pod CIDRs or `10.0.0.0/8` to the denied list when callbacks only go to
external receivers.

## Secrets

- Secrets come from files in `FASTMOCK_SECRETS_DIR` (a mounted Secret) or `FASTMOCK_SECRET_<NAME>` variables.
  Names are validated, so a secret reference cannot read other files.
- Stubs can use a secret only for HMAC signatures of callbacks, only by name, and only if the name is listed in
  `FASTMOCK_STUB_SECRETS`. Templates cannot read secrets at all.
- Contract tests can send a secret as a header value only if it is listed in `FASTMOCK_CONTRACT_SECRETS`, only to
  the hosts bound to it there, never to FastMock's own listeners, and only in runs started by a principal that
holds `contracts:secrets` globally (the built-in `admin` role bound globally, or a custom role in a global
binding). A name
  cannot be both a stub secret and a contract secret, so HMAC keys never leave the server. Values of contract
  secrets are masked wherever they appear in stored run results and in the journal
  ([Contract testing](../guide/contract-testing.md#secrets-and-redaction)).
- Plugins read their own secrets (for example a provider's signing key); they are not exposed to stubs.
- Database and Valkey passwords can be passed separately (`FASTMOCK_DATABASE_PASSWORD`,
  `FASTMOCK_VALKEY_PASSWORD`) so they never appear in URLs, ConfigMaps or command lines. Kafka SASL and Schema
  Registry passwords (`FASTMOCK_KAFKA_SASL_PASSWORD`, `FASTMOCK_KAFKA_SCHEMA_REGISTRY_PASSWORD`) are separate
  variables as well; take them from a Secret.
- `GET /__admin/info`, API errors and logs do not include secret values or connection URLs.

## Redaction

Before a request, a response or a callback is stored or shown, FastMock masks secrets as `***`:

| Where | What is masked |
|---|---|
| Headers | Names in `FASTMOCK_REDACT_HEADERS` (authorization, cookies, API keys, signatures and more) and every header whose name contains `token`, `secret`, `password`, `key`, `signature`, `sig`, `hmac` or `credential`. `Authorization: Bearer x` becomes `Bearer ***`, keeping the scheme. |
| Query strings | Parameters named in `FASTMOCK_REDACT_JSON_KEYS` (`?access_token=***`). |
| JSON bodies | Values of keys in `FASTMOCK_REDACT_JSON_KEYS` at any depth (`password`, `token`, `client_secret`, `otp`, `pin`, `cvv`, ...). Key matching ignores case, `-` and `_`. |
| Form, multipart and XML bodies | The same keys as form fields, multipart part names, XML elements and attributes. |
| Logs | Callback URLs are logged without query strings; delivery errors never contain the URL. |

Redaction is applied to the journal, `GET /__admin/callbacks`, the UI, near-miss reports and logs. Matching and
templates always work on the original request.

Viewers read stubs (`GET /stubs`, `/stubs/{id}`, `/export`, `/stubs/unused`, `POST /stubs/find-by-metadata`) with
the same rules applied to the stub definition: matchers on sensitive headers, sensitive query and form parameter
names and all cookies show `{"equalTo": "***"}`; basic-auth passwords, sensitive keys in `equalToJson`,
`jsonBody`, callback and message bodies, metadata and text bodies, sensitive callback URL parameters and sensitive
proxy, callback and message headers show `***`; `jsonPath` and `xpath` matchers that name a sensitive key have
their expected value and quoted literals masked; multipart parts with a sensitive name have their content masked.
Editors see stubs unchanged. Contract-test request and response excerpts are shown to editors only. Recording replaces sensitive request headers and body fields with
"must be present" conditions instead of storing their values.

Extend the lists for your domain (for example `FASTMOCK_REDACT_JSON_KEYS` with `iban,cardNumber`). Setting
either variable replaces the default list, so include the defaults ([Configuration](configuration.md#request-journal)).

## Template sandbox

Templates are Handlebars with a fixed set of helpers:

- no access to files, environment variables, secrets, the network or the clock beyond `now`;
- no partials, no custom helpers from stubs, no code;
- template source is limited to 256 KiB, rendered output to 8 MiB and intermediate values to 32 MiB per render;
- the parsed request JSON is built once per request and only for bodies up to 200,000 JSON nodes;
- regular expressions in matchers and helpers use a linear-time engine with a compiled-size limit (256 KiB), so
  no input can cause catastrophic backtracking;
- XML is parsed without DTDs or external entities, with node and depth limits.

## Resource limits

| Limit | Default | Setting |
|---|---|---|
| request body | 10 MiB | `FASTMOCK_MAX_BODY_BYTES` |
| bodies in flight | 256 MiB (128 MiB in the chart) | `FASTMOCK_MAX_INFLIGHT_BODY_BYTES` |
| time to send headers | 30 s | `FASTMOCK_HEADER_READ_TIMEOUT` |
| response and chaos delay | 15 min | `FASTMOCK_MAX_DELAY` |
| journal write queue | 2,048 entries | `FASTMOCK_JOURNAL_QUEUE` |
| matchers per stub | 64 | fixed |
| JSON nodes parsed per request | 200,000 | fixed |
| form parameters / multipart parts / cookies | 10,000 / 64 / 256 | fixed |
| callback delay / attempts | 7 days / 50 | fixed |
| Kafka publications in flight | 256 per replica | fixed |
| proxied response body | 16 MiB | fixed |
| recorded stubs per session | 1,000 | `FASTMOCK_RECORDING_MAX_STUBS` |

Importers (OpenAPI, AsyncAPI, WireMock, MockServer, Postman, HAR, WSDL, GraphQL schemas, Microcks artifacts) have
their own size, count and depth limits, listed on their pages. Panics in request handlers are caught and turned
into `500 {"error":"internal error"}` (`fastmock_panics_total`); errors returned to clients never contain stack
traces, file paths or SQL.

## Audit log

Every change through the admin API is recorded:

- in the store (PostgreSQL table `fm_audit`, kept 90 days), readable by admins with `GET /__admin/audit?limit=100`;
- as a log line with target `audit` (`"message":"admin change"`), including rejected calls
  (`admin request rejected: missing or invalid token`, `... permission missing`), ready to be shipped to your log
  system.

Events carry `principalKind` (`token`, `apiToken`, `user`, `anonymous`) and, for changes inside a space, `space`; `role` lists the role
names of the principal's bindings.

```json
{"events":[{"id":15,"at":"2026-11-19T05:49:27.026723055Z","principal":"me","role":"admin","principalKind":"token","action":"reset","detail":{"requests":19,"callbacks":7,"scenarios":0}}]}
```

```json
{"level":"WARN","message":"admin request rejected: permission missing","principal":"ci","kind":"token","path":"/__admin/kv/x","permission":"state:write","target":"audit"}
```

`fastmock_admin_changes_total{action}` counts changes per action, `fastmock_admin_auth_failures_total{reason}`
rejected calls (alert `FastMockAdminAuthFailures`).

## Containers and Kubernetes

The image is distroless (no shell, no package manager) and runs as user 65532. The Helm chart and the operator
run pods with:

- `runAsNonRoot`, read-only root file system, no privilege escalation, all capabilities dropped,
  `RuntimeDefault` seccomp;
- no service account token mounted and no service links;
- the admin token file and secrets mounted read-only with mode `0440`;
- separate Services for data and admin ports, so they can be exposed and restricted separately;
- optional NetworkPolicies for ingress, egress and the bundled databases.

FastMock sends no telemetry anywhere unless you configure an OTLP endpoint.

## Supply chain

- Builds are reproducible from `Cargo.lock` (`cargo build --locked`) and pinned base images.
- Dependencies are checked against the RustSec advisory database (`cargo audit`) and kept current.
- In your pipeline, build the image from source, scan it, generate an SBOM, sign it (for example with cosign),
  push it to your registry, and allow only that registry: in Kubernetes with an admission policy, and for the
  operator with `server.allowedImagePrefixes`.
- Pin the image by digest in production values.

## Hardening checklist

- [ ] One token per client, minimal role, stored in a secret manager; rotation documented.
- [ ] People sign in through OIDC with group bindings instead of shared tokens; session TTL and idle timeout
      match your off-boarding needs; `user:` bindings use `sub` (or `oid`); the admin UI on its own host name.
- [ ] On a shared instance every team's space has `pathPrefixes` or `hosts`; with the operator,
      `policy.spaceFromNamespace` is on or RBAC on `mockapis` matches who may write into which space.
- [ ] Managed tokens of people who leave are revoked (the token list shows the creator).
- [ ] Admin port reachable only from tests, Prometheus and the Gateway (NetworkPolicy `adminFrom`).
- [ ] UI only over TLS, `FASTMOCK_ADMIN_ALLOWED_ORIGINS` set to its URL; `/metrics` not published.
- [ ] `FASTMOCK_CALLBACK_ALLOWED_HOSTS` set; egress NetworkPolicy with the same scope.
- [ ] `FASTMOCK_STUB_SECRETS` lists only the keys stubs need; `FASTMOCK_CONTRACT_SECRETS` binds each contract
      secret to the host of its service only.
- [ ] Redaction lists extended for your domain's sensitive fields.
- [ ] Kafka over `sasl_ssl` or `ssl` with a dedicated user that may write only the mocked topics and read only
      the contract topics; `PLAIN` only over TLS.
- [ ] `FASTMOCK_KAFKA_ALLOWED_TOPICS` set when Kafka is shared; `FASTMOCK_KAFKA_CONTRACT_TOPICS` lists only the
      topics contract tests must read (empty disables reading).
- [ ] `FASTMOCK_UI_DATA_PLANE_URL` set to the public data-plane origin when the UI is exposed.
- [ ] Journal and callback retention as short as your tests allow.
- [ ] PostgreSQL with `FASTMOCK_DATABASE_SSL_MODE=verify-full`, dedicated database and user; the server user
      without DDL rights when migrations run as a separate Job.
- [ ] Valkey over `rediss://` with an ACL user limited to FastMock's prefix and commands.
- [ ] Image from your registry, pinned by digest; operator `server.allowedImagePrefixes` set.
- [ ] Alerts from the chart's PrometheusRule routed to the owning team; audit log shipped.
