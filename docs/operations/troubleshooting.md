# Troubleshooting

Symptom, cause, fix. Most investigations start with three tools:

- the **trace id**: every data-plane response has `x-fastmock-trace-id`; search the journal
  (`GET /__admin/requests`), the logs (`"trace_id"`) and your tracing backend for it;
- the **journal and near misses**: `GET /__admin/requests?matched=false`, `POST /__admin/requests/near-misses`;
- **`/readyz`, `/metrics` and the logs** of the admin port.

Contents:

- [Admin API: 401 and 403](#admin-api-401-and-403)
- [The server does not start](#the-server-does-not-start)
- [No stub matched (404)](#no-stub-matched-404)
- [500 stub evaluation failed](#500-stub-evaluation-failed)
- [413 and 503 on the data port](#413-and-503-on-the-data-port)
- [Requests are missing from the journal](#requests-are-missing-from-the-journal)
- [Callbacks stay pending or fail](#callbacks-stay-pending-or-fail)
- [Scenarios do not change state](#scenarios-do-not-change-state)
- [Kafka messages are not published](#kafka-messages-are-not-published)
- [gRPC calls return UNIMPLEMENTED](#grpc-calls-return-unimplemented)
- [Readiness fails](#readiness-fails)
- [Replicas behave differently](#replicas-behave-differently)
- [Operator conditions](#operator-conditions)

## Admin API: 401 and 403

| Response | Cause | Fix |
|---|---|---|
| `401 {"error":"unauthorized","details":[]}` | No `Authorization: Bearer <token>` header, unknown token, or a static or managed token longer than 512 characters. | Send the header. Compare `fastmock hash-token '<token>'` with the hash in the token entry. A hash made with `echo token \| sha256sum` includes a new line and is wrong; use `printf '%s' token \| sha256sum` or `fastmock hash-token`. |
| `401` after a token change | Token files are read at start-up. | Roll the pods ([Rotating tokens](security.md#rotating-tokens)). |
| `403 {"error":"origin not allowed","details":[]}` (static tokens only) or `403 {"error":"forbidden","details":["origin not allowed"]}` (access file or OIDC) | The request has an `Origin` header (a browser, or a tool that adds it) that is neither in `FASTMOCK_ADMIN_ALLOWED_ORIGINS` nor the admin server's own origin. | Add the exact origin (`https://fastmock-admin.example.com`, scheme and port included) to `FASTMOCK_ADMIN_ALLOWED_ORIGINS`. Behind a Gateway, make sure it forwards the original `Host`. Scripts should not send `Origin`. |
| `403 {"error":"forbidden","details":["requires role editor"],"required":"editor"}` | The token's role is too low for the call. | Use a token with the role named in `required` ([roles](security.md#admin-tokens-and-roles)). |

`fastmock_admin_auth_failures_total{reason}` (`token`, `expired`, `role`, `permission`, `origin`, `session`, `jwt`, `csrf`) and
log lines with `"target":"audit"` show who is rejected and why.

### SSO sign-in

| Symptom | Cause | Fix |
|---|---|---|
| `/__admin/auth/login` answers `503` | The provider has not been discovered (unreachable, TLS not trusted, wrong issuer URL); `fastmock_oidc_ready` is 0 and the log says why. | Check `curl <issuer>/.well-known/openid-configuration` from the pod; add the provider CA with `FASTMOCK_OIDC_CA_FILE`. |
| Back at `/ui/?loginError=no_access` | The user signed in, but no `user:` or `group:` binding matches. The audit log has `auth.login_failed` with the user name. | Check the groups the provider sends (`GET /__admin/info` of a user who can sign in shows them): Keycloak sends `/team` with *Full group path* on, Entra ID sends object ids. |
| Back at `/ui/?loginError=invalid_state` | The login took more than 10 minutes, the callback was opened twice or in another browser, or the login cookie was dropped (the UI was opened under another host name than `FASTMOCK_OIDC_REDIRECT_URL`, http without `FASTMOCK_OIDC_INSECURE_COOKIE` outside localhost, or a proxy that strips cookies). | Open the UI under the host of the redirect URL, over https, and sign in again. |
| Back at `/ui/?loginError=failed` | Code exchange or ID token validation failed. The log line `code exchange failed` or `ID token rejected` names the cause (`invalid_client`, `claim check failed: aud`, `unknown signing key`, clock skew above 60 seconds). | Fix the client secret, the client id, or the clock. |
| `403 ... missing X-FastMock-Request: 1 header` | A change authenticated by the session cookie without the CSRF header. | Send `X-FastMock-Request: 1` (the UI does), or use a bearer token. |
| `401 ... invalid bearer token` with a JWT | Wrong audience (`FASTMOCK_OIDC_API_AUDIENCE`), issuer, expiry or signing key. | Decode the token's payload and compare `iss` and `aud` with the settings. |

## The server does not start

The process exits with one line that names the problem:

| Message | Fix |
|---|---|
| `no admin tokens configured: set FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE ...` | Configure at least one token or OIDC sign-in. |
| `OIDC discovery for ... failed: ... points to ..., which is neither the issuer host nor listed in FASTMOCK_OIDC_ALLOWED_HOSTS` | The provider's discovery document names an endpoint on another host: add the host to `FASTMOCK_OIDC_ALLOWED_HOSTS` if you trust it. |
| `the discovery document names issuer "...", not FASTMOCK_OIDC_ISSUER "..."` | The issuer setting differs from the provider's (often a trailing slash or the realm path): copy the `issuer` value of the discovery document. |
| `FASTMOCK_OIDC_... ` errors | See [OIDC configuration](configuration.md#oidc-sign-in). `FASTMOCK_OIDC_API_AUDIENCE`, when set, must not be empty (`FASTMOCK_OIDC_API_AUDIENCE is set but empty: set the audience of the API tokens, or remove the variable to turn bearer JWTs off`) and must differ from `FASTMOCK_OIDC_CLIENT_ID`; remove the variable to turn bearer JWTs off. |
| ``space a and space b overlap: path prefix `/pay/` of space a and `/pay/x` of space b; each path belongs to one space`` (or ``... host `x` of space a and `y` of space b; each host belongs to one space``) | Two spaces own overlapping path prefixes or the same host; a host without a port overlaps the same host with any port ([ownership](access-control.md#ownership-of-the-data-plane)). Give each space its own prefixes and hosts. |
| `admin token entry #2: expected name:role:sha256hex` / `unknown role` / `hash must be a SHA-256 (64 hex chars)` | Fix that entry (counted from 1, file lines after variable entries). |
| `FASTMOCK_LISTEN and FASTMOCK_ADMIN_LISTEN must differ` | Use two ports. |
| `FASTMOCK_DATABASE_URL is required for postgres storage` | Set the URLs or `FASTMOCK_STORAGE=memory`. |
| `connecting to storage` with a connection error | PostgreSQL or Valkey unreachable, wrong credentials, or the database does not exist. |
| `set both FASTMOCK_TLS_CERT_FILE and FASTMOCK_TLS_KEY_FILE` | TLS needs both files. |
| `FASTMOCK_MAX_INFLIGHT_BODY_BYTES must be at least FASTMOCK_MAX_BODY_BYTES` | Raise the in-flight budget or lower the body limit. |
| `FASTMOCK_X: invalid duration unit ...`, `not a boolean` | See [value formats](configuration.md#value-formats). |
| `binding 0.0.0.0:8080` / `Address already in use` | Another process uses the port. |

## No stub matched (404)

The data plane answered `404 {"error":"no stub matched","method":"POST","path":"/sms"}` and the journal entry
has `matched: false`. Ask why:

```sh
curl -s -X POST localhost:9090/__admin/requests/near-misses \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"method": "POST", "path": "/orders/A-1/pay2"}'
```

```json
{"nearMisses":[{"stubId":"25c5bacf-600a-4b26-85e1-81efa815c5f6","name":"create order","score":0.33,"mismatches":["path: `/orders/A-1/pay2` does not match"]}]}
```

The [verification page](../guide/verification.md) shows how to run near misses for a journaled request. In a development
environment, `FASTMOCK_UNMATCHED_DETAILS=true` puts the three closest stubs into the 404 body.

Common causes:

| Cause | Check |
|---|---|
| Different method, or a trailing slash (`/orders/` vs `/orders`) | `mismatches` names the method or path. |
| Query string in `urlPath` | `urlPath` matches the path only; use `query` matchers (or WireMock-style `url`). See [Request matching](../guide/request-matching.md). |
| Header name case | Header names in stubs are matched case-insensitively; values are case-sensitive unless `caseInsensitive`. |
| Body matcher on JSON, but the client sends another content type or invalid JSON | The journal shows the body as received. |
| A scenario stub whose `requiredState` does not hold | `GET /__admin/scenarios`; states expire after `ttlSeconds`. |
| The stub is disabled, used up (`times`) or expired (`ttlSeconds` of the stub) | `GET /__admin/stubs/{id}`; `fastmock_stub_exhausted_total`. |
| A higher-priority stub answers instead | The journal's `stubId` shows which stub answered. Lower numbers win. |
| The request went to a plugin mount (`/<plugin>/...`) | Plugin paths are handled by the plugin, not by stubs; `source` in the journal shows it. |

## 500 stub evaluation failed

`500 {"error":"stub evaluation failed", ...}`: a template failed while rendering (an unknown helper argument,
output over 8 MiB, a rendered scenario name over 512 bytes), or a scenario changed concurrently more than 8
times. The error log line has the details and the trace id:

```sh
kubectl logs deploy/fastmock | grep '"stub evaluation failed"'
```

`fastmock_stub_errors_total` counts these. Test templates before saving with `POST /__admin/stubs/validate`.
A missing body file answers `500 {"error":"response body file is not available"}`
(`fastmock_body_file_missing_total`).

## 413 and 503 on the data port

| Response | Cause | Fix |
|---|---|---|
| `413 {"error":"request body too large"}` | Body larger than `FASTMOCK_MAX_BODY_BYTES` (10 MiB). | Raise the limit if the mocked API really accepts such bodies. Counted as `fastmock_requests_rejected_total{reason="body_too_large"}`. |
| `503 {"error":"server busy: too many large requests in flight"}` with `Retry-After: 1` | The in-flight body budget (`FASTMOCK_MAX_INFLIGHT_BODY_BYTES`) was used up for 10 seconds; each request reserves three times its size. | Add replicas, raise the budget together with the memory limit, or reduce parallel large uploads. Counted as `fastmock_requests_rejected_total{reason="busy"}`. |
| `502 {"error":"proxy failed", ...}` | A proxy stub could not reach its upstream. | See [Proxy stubs](../guide/proxy-and-chaos.md#proxy-stubs). |
| Unexpected `5xx` or broken connections with `x-fastmock-chaos` header | A chaos rule fired. | `GET /__admin/chaos`; `DELETE /__admin/chaos` to remove all rules. |

## Requests are missing from the journal

| Cause | Check / fix |
|---|---|
| The journal is written after the response, asynchronously. | Poll (`/requests/count`, the Java client's `awaitRequests`) instead of reading once right after the call. |
| The write queue was full and entries were dropped. | `fastmock_journal_dropped_total` > 0: raise `FASTMOCK_JOURNAL_QUEUE`, check PostgreSQL latency. |
| PostgreSQL write errors. | `fastmock_journal_write_errors_total`, error log `journal write failed`; alert `FastMockStorageErrors`. |
| Retention removed them. | `FASTMOCK_JOURNAL_RETENTION` (24 h). In memory mode at most 100,000 entries are kept. |
| Journaling is off. | `FASTMOCK_JOURNAL_ENABLED`. |
| Filters do not match. | `pathPrefix` is a prefix of the path without query; `correlation` must be exact; `since` is RFC 3339. |
| Bodies end with `…[truncated N bytes]` or show `***`. | `FASTMOCK_JOURNAL_MAX_BODY_BYTES`, [redaction](security.md#redaction). |

## Callbacks stay pending or fail

Look at the callback first:

```sh
curl -s "localhost:9090/__admin/callbacks?correlation=ord-1001" -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  | jq '.callbacks[] | {status, attempts, maxAttempts, dueAt, lastStatus, lastError}'
```

| What you see | Cause | Fix |
|---|---|---|
| No callback at all | The request did not match the stub with the callback (check the journal's `stubId`), or the stub has no `callbacks`. | |
| `pending`, `attempts: 0`, `dueAt` in the future | The `delay` has not passed. | Wait or shorten `delay`. |
| `pending`, `dueAt` in the past, `fastmock_callbacks_pending` growing | No replica is delivering: workers are busy with slow receivers, or the outbox cannot be read. | `fastmock_callback_duration_seconds`, `fastmock_store_errors_total{store="outbox"}`, error log `claiming callbacks failed`. Raise `FASTMOCK_CALLBACK_WORKERS`, fix slow receivers. Alert `FastMockCallbackBacklog`. |
| `pending` with `lastError` and `attempts` < `maxAttempts` | A retryable failure (connection error, timeout, 408, 429, 5xx); the next attempt is scheduled with backoff. | Fix the receiver; see `lastError`. |
| `failed`, `attempts: 0`, `callback could not be built: secret ... is not configured` | The HMAC `secretRef` is not configured or not in `FASTMOCK_STUB_SECRETS`. | Configure the secret and list its name ([HMAC](../guide/callbacks.md#hmac-signatures)). |
| `failed`, `attempts: 0`, `callback could not be built: ...` (other) | A callback template failed. | Fix the template; validate the stub. |
| `failed`, `blocked by egress policy: host X is not in FASTMOCK_CALLBACK_ALLOWED_HOSTS` | Allow-list. | Add the host or a `*.suffix`. |
| `failed`, `blocked by egress policy: address A is denied by FASTMOCK_CALLBACK_DENIED_CIDRS` | Literal IP in a denied range. | Use a host name of an allowed address. |
| `lastError` contains `all addresses of X are denied by FASTMOCK_CALLBACK_DENIED_CIDRS` | The host resolves to a denied address, typically `localhost`. | Use the service name (`http://fastmock:8080/...`) instead of `localhost`. |
| `failed`, `unexpected status 400` (or another non-retryable status) | The receiver rejected the callback. | Check body, headers, signature on the receiver side. |
| `failed` after `maxAttempts` with timeouts | The receiver is slower than `FASTMOCK_CALLBACK_TIMEOUT`. | Raise the timeout or speed up the receiver. |
| The receiver gets the same callback twice | At-least-once delivery after a timeout or a replica restart. | Deduplicate on `x-fastmock-delivery-id`. |
| Callbacks disappear | `DELETE /__admin/callbacks`, `POST /__admin/reset`, or retention (`FASTMOCK_CALLBACK_RETENTION`). | |

## Scenarios do not change state

| Cause | Fix |
|---|---|
| The rendered name differs between stubs (`order-{{request.json.id}}` vs `order-{{request.pathParams.id}}` with different values) | Compare with `GET /__admin/scenarios`. |
| The state expired (`ttlSeconds`, default 7 days) and is back to `Started` | Use a longer TTL. |
| A stub without `scenario` and with a lower priority number answers first | Check `stubId` in the journal; adjust priorities. |
| Expected a state change from a stub without `newState` | Only stubs with `newState` move the scenario. |

## Kafka messages are not published

The stub answers normally; publishing errors only appear in logs and metrics
(`fastmock_messages_total{result="failed"}`, log `message publishing failed` with `topic` and `error`).

| `error` | Fix |
|---|---|
| `topic is not in FASTMOCK_KAFKA_ALLOWED_TOPICS` | Add the topic or a `prefix*` entry. |
| `FASTMOCK_KAFKA_BOOTSTRAP is not configured` | Configure Kafka. |
| `Kafka topic X does not exist` | Create the topic. |
| `connecting to Kafka timed out` | Network or NetworkPolicy egress to the brokers, or a TLS or SASL error: the warnings `Failed to connect to broker` before it name the cause ([Secure connections](../guide/kafka.md#secure-connections)). |
| `avroSchema needs FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL`, `schema registry returned 409 ...` | Registry configuration or schema compatibility. |
| `$.field: ... does not fit Avro type` | The rendered `jsonValue` does not match the schema ([conversion rules](../guide/kafka.md#json-to-avro-conversion)). |

`result="dropped"` means more than 256 publications were in flight on one replica; Kafka is too slow or
unreachable.

## gRPC calls return UNIMPLEMENTED

| `grpc-message` | Fix |
|---|---|
| `unknown gRPC method /x.Y/Z: upload its descriptor set first` | Upload the descriptor set (`PUT /__admin/grpc/descriptors/{name}`); other replicas pick it up within `FASTMOCK_STUB_POLL`. |
| `no stub matched /x.Y/Z` | Create a stub for `POST /x.Y/Z`; near misses work as for HTTP. |
| `compressed gRPC messages are not supported` | Disable compression in the client. |

## Readiness fails

`GET /readyz` on the admin port:

| Response | Meaning |
|---|---|
| `200 {"status":"ready","valkey":"ok","database":"ok"}` | Ready. |
| `503 {"status":"starting"}` | Still starting, or shutting down (readiness is withdrawn on SIGTERM). |
| `503 {"status":"degraded","valkey":"unavailable","database":"ok"}` | A store does not answer; the warning log `readiness check failed` has the error. |

`/healthz` only reports that the process runs; Kubernetes restarts the pod only when it fails. Storage outages
make pods unready (no traffic) but do not restart them.

## Replicas behave differently

| Cause | Fix |
|---|---|
| Memory mode with more than one replica | Each replica has its own stubs and journal. Use `postgres` mode (the operator refuses this combination). |
| Two instances share one Valkey with the same prefix | Give each its own `FASTMOCK_VALKEY_PREFIX`. |
| A stub was changed a moment ago | Changes reach all replicas within milliseconds through PostgreSQL notifications, at worst within `FASTMOCK_STUB_POLL`; `fastmock_stubs_loaded` should be equal on all replicas. |
| Chaos rules just changed | Cached for up to 2 seconds per replica. |

## Operator conditions

`kubectl get fastmock <name> -o yaml` and `kubectl get mockapi <name> -o yaml` show `status.conditions`; the
operator log has a line per refusal (`FastMock refused`).

| Condition | Fix |
|---|---|
| `Ready=False ImageNotAllowed` | Use an image under an allowed prefix, or extend `server.allowedImagePrefixes` in the operator chart. |
| `Ready=False SecretNotAllowed` | Label the Secret named in the message: `kubectl label secret <name> fastmock.franshub.io/allow-mount=true`. |
| `Ready=False SecretNotFound` | Create the Secret, or fix the name. |
| `Ready=False SecretKeyNotFound` | The OIDC client Secret has no value under the key of `spec.oidc.clientSecretRef` (default `client-secret`): add it or fix the key. |
| `Ready=False ConfigMapNotAllowed` | Label the access ConfigMap: `kubectl label configmap <name> fastmock.franshub.io/allow-mount=true`. |
| `Ready=False ConfigMapNotFound` | Create the ConfigMap named in `spec.access.configMapRef`, or add the key (default `access.yaml`). |
| `Ready=False EnvNotAllowed` | Remove the listed variables from `spec.env` and use the dedicated fields (`storage`, `adminTokensSecret`, `secretsSecret`, `access`). |
| `Ready=False InvalidSpec` | Read the condition's `message`. Set `spec.storage.mode` (`memory` or `postgres`); `postgres` needs `database` and `valkey`; memory allows one replica. For `oidc.apiAudience must not be empty: remove it to turn bearer JWTs off` or `oidc.apiAudience must differ from oidc.clientId`, set the audience of the API tokens or remove `apiAudience`. |
| `Ready=False Progressing` | Pods are not ready: `kubectl describe deploy <name>`, pod logs, [Readiness fails](#readiness-fails). |
| `RouteReady=False ApplyFailed` | Gateway API CRDs are missing or the route is invalid; the message has the API error. |
| `Synced=False ServerNotReady` | The `FastMock` is missing or not ready yet; the `MockApi` retries every 15 s. |
| `Synced=False SyncFailed` | The message names the problem: invalid stub JSON, unparsable OpenAPI/WireMock document, missing ConfigMap or key, admin API error. |
| `Synced=False SpaceNotFound` | The server's access file does not define the `MockApi`'s space; the stubs applied before stay. Add the space to the access file ([Access file](operator.md#access-file)) or fix `spec.space`. |

While a `FastMock` is refused, nothing is changed: the previous Deployment keeps serving.
