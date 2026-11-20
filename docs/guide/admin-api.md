# Admin API

Everything FastMock does is controlled through the admin API: stubs, the request journal, verification, body
files, imports, recording, scenarios, chaos rules and gRPC descriptors. The UI uses the same API.

- [Where it lives](#where-it-lives)
- [OpenAPI document and Swagger UI](#openapi-document-and-swagger-ui)
- [Authentication](#authentication)
- [Roles per endpoint](#roles-per-endpoint)
- [Managed tokens](#managed-tokens)
- [Errors](#errors)
- [Pagination and filtering](#pagination-and-filtering)
- [Stubs](#stubs)
- [Partial updates (PATCH)](#partial-updates-patch)
- [Metadata search (WireMock compatible)](#metadata-search-wiremock-compatible)
- [Unused stubs](#unused-stubs)
- [Body files](#body-files)
- [Audit log](#audit-log)
- [Version and info](#version-and-info)
- [Limits](#limits)
- [Metrics](#metrics)

The examples use `FASTMOCK_TOKEN` for an editor token (an admin token where noted), the admin API on
`localhost:9090` and the data plane on `localhost:8080`.

## Where it lives

All routes are under `/__admin` on the **admin listener** (`FASTMOCK_ADMIN_LISTEN`, default `0.0.0.0:9090`), which
is separate from the data plane that serves stubs. The admin listener also serves, without authentication:

| Path | Content |
|---|---|
| `/openapi.json` | OpenAPI 3.1 description of the admin API (no data) |
| `/ui`, `/ui/api` | web UI and the Swagger UI for the admin API |
| `/healthz`, `/readyz` | liveness and readiness |
| `/metrics` | Prometheus metrics |

## OpenAPI document and Swagger UI

`GET /openapi.json` returns the complete OpenAPI 3.1 document of the admin API: every route, request and response
schema (the stub model included), query parameters, error responses and examples. It is served with
`Cache-Control: no-cache` and the same security headers as the rest of the admin listener. The same document is
kept in the repository at `docs/api/openapi.json`.

```sh
curl -s http://localhost:9090/openapi.json | jq '.paths | keys | length'
```

Open `http://localhost:9090/ui/api` for Swagger UI: click **Authorize**, paste a token, and try requests from the
browser. Use the document to generate clients:

```sh
npx openapi-typescript http://localhost:9090/openapi.json -o fastmock-admin.d.ts
```

## Authentication

Every `/__admin` request needs `Authorization: Bearer <token>`. Tokens are configured as
`name:role:sha256hex` entries in `FASTMOCK_ADMIN_TOKENS` or `FASTMOCK_ADMIN_TOKENS_FILE`; the server stores only
SHA-256 hashes and compares them in constant time. Create a token and its entry with:

```sh
fastmock hash-token --generate          # prints a new random token and its hash
fastmock hash-token "$FASTMOCK_TOKEN"   # prints the hash of an existing token
# FASTMOCK_ADMIN_TOKENS=ci:editor:<hash>,dash:viewer:<hash>
```

Tokens can also be created, listed and revoked through the API, with an expiry and optionally limited to
spaces: see [Managed tokens](#managed-tokens).

With OIDC configured, two more credentials are accepted ([Access control: OIDC](../operations/access-control.md#oidc-sign-in-and-bearer-jwts)):

- a JWT from the identity provider as the bearer token, when `FASTMOCK_OIDC_API_AUDIENCE` is set and the token's
  `aud` contains it;
- the UI's session cookie (`__Host-fastmock_session`) when there is no `Authorization` header. Requests other than
  `GET` and `HEAD` then need the header `X-FastMock-Request: 1`.

An `Authorization` header always wins: static tokens are checked first, then managed `fm_` tokens, then JWTs.

Browser requests carrying an `Origin` header are accepted only from `FASTMOCK_ADMIN_ALLOWED_ORIGINS` or the
admin listener's own origin.

| Response | Meaning |
|---|---|
| `401 {"error":"unauthorized","details":[]}` + `WWW-Authenticate: Bearer` | token missing, unknown or revoked |
| `401 {"error":"unauthorized","details":["token expired"]}` | managed token past its expiry |
| `503 {"error":"token store unavailable","details":[]}` | a managed token could not be checked because the database is unreachable |
| `401 {"error":"unauthorized","details":["invalid bearer token"]}` + `WWW-Authenticate: Bearer error="invalid_token"` | a JWT with a bad signature, issuer, audience or lifetime |
| `401 {"error":"unauthorized","details":["session expired or unknown: sign in again"]}` | session cookie expired, logged out or unknown; the cookie is cleared |
| `403 {"error":"forbidden","details":["missing X-FastMock-Request: 1 header"]}` | a change authenticated by the session cookie without the CSRF header |
| `403 {"error":"forbidden","details":["user bob has no access: ..."],"required":"..."}` | an OIDC user without any binding |
| `503 {"error":"session store unavailable","details":[]}` | the hot store holding sessions is unreachable |
| `403 {"error":"forbidden","details":["requires role editor"],"required":"editor"}` | token valid, role too low |
| `403 {"error":"forbidden","details":["requires permission files:read"],"required":"files:read"}` | a token with a custom role or space bindings lacks the permission ([Access control](../operations/access-control.md#errors)) |
| `403 {"error":"origin not allowed","details":[]}` | browser request from another origin |

## Roles per endpoint

Every route needs a permission (`stubs:read`, `journal:delete`, ...). The three built-in roles are sets of
permissions, each including the one before: **viewer** reads, **editor** changes test state, **admin** can
additionally wipe everything and read the audit log. With the built-in roles the rule is: `GET`/`HEAD` and
read-only `POST`s need viewer, every other method needs editor; the routes marked *admin* need admin. Custom roles,
spaces and space bindings come from the access file; the permission of every route and how the `space` parameter
selects data are listed in [Access control](../operations/access-control.md#permissions).

| Endpoint | Role |
|---|---|
| `GET /stubs`, `/stubs/{id}`, `/stubs/unused`, `/export` | viewer (credentials masked, see [Security: redaction](../operations/security.md#redaction)) |
| `POST /stubs/find-by-metadata`, `POST /stubs/validate` | viewer |
| `POST /stubs`, `PUT/PATCH/DELETE /stubs/{id}`, `DELETE /stubs?tag=`, `POST /stubs/import`, `POST /stubs/remove-by-metadata` | editor |
| `DELETE /stubs` (no tag: all stubs) | admin |
| `GET /requests`, `/requests/{id}`, `/requests/unmatched`, `/requests/count` | viewer |
| `POST /requests/count`, `/requests/near-misses`, `/requests/find`, `/verify`, `/verify/sequence` | viewer (bounded scans, see [Verification: limits](verification.md#limits-and-caveats)) |
| `DELETE /requests`, `POST /requests/remove` | editor |
| `GET /files`, `/files/{name}` | viewer |
| `PUT/DELETE /files/{name}` | editor |
| `POST /import`, `/import/openapi`, `/import/wiremock`, `/import/postman`, `/import/har` (and the other `/import/*`) | editor |
| `GET /recordings`; `POST /recordings/start`, `/recordings/stop`, `/recordings/snapshot` | viewer; editor |
| `GET /callbacks`, `/scenarios`, `/kv/{key}`, `/chaos`, `/grpc/descriptors` | viewer |
| `DELETE /callbacks`, `DELETE /scenarios`, `PUT /scenarios/{name}`, `PUT/DELETE /kv/{key}`, `PUT/DELETE /chaos`, `PUT/DELETE /grpc/descriptors/{name}` | editor |
| `GET /info`, `/version`, `/plugins`, `/spaces` | viewer (any token) |
| `GET /settings`, `/stubs/counters`; `PUT /settings`, `DELETE /stubs/counters` | viewer; editor |
| `GET /contract-tests`, `/contract-tests/{id}` | viewer (verdicts only; details need editor) |
| `POST /contract-tests`, `DELETE /contract-tests/{id}` | editor; `POST` with `secretHeaders` needs admin |
| `POST /reset`, `GET /audit` | admin |
| `GET /tokens`, `/tokens/{id}`; `POST /tokens`, `GET /tokens/options`, `DELETE /tokens/{id}` | admin (`tokens:read`, `tokens:write`; see [Managed tokens](#managed-tokens)) |
| `GET /tokens/self`; `DELETE /tokens/{id}` of the calling token itself | the managed token itself |

## Sign-in endpoints

These routes serve the UI's SSO sign-in and need no token ([Access control: OIDC](../operations/access-control.md#oidc-sign-in-and-bearer-jwts)).
Without OIDC configuration they answer `404`, except `/auth/config`.

| Endpoint | Response |
|---|---|
| `GET /auth/config` | `{"oidc": true, "tokenLogin": true}`: which sign-in methods the login page offers |
| `GET /auth/login?return=/ui/...` | `302` to the identity provider; `400` for a `return` outside `/ui/`; `503` until the provider is discovered; `429` when too many logins start on this replica |
| `GET /auth/callback` | `302` to the `return` path with the session cookie, or to `/ui/?loginError=<code>` (`no_access`, `invalid_state`, `idp_error`, `failed`, `unavailable`) |
| `POST /auth/logout` | needs `X-FastMock-Request: 1`; deletes the session, clears the cookie, `{"loggedOut": true, "logoutUrl": "..."}` (`logoutUrl` only when the provider supports RP-initiated logout) |

## Managed tokens

```sh
# create (the value is shown only once)
curl -s -X POST localhost:9090/__admin/tokens -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name": "ci-billing", "role": "editor", "spaces": ["billing"], "expiresInDays": 30}'
# list, read one, read your own, revoke
curl -s localhost:9090/__admin/tokens?status=active -H "Authorization: Bearer $ADMIN_TOKEN"
curl -s localhost:9090/__admin/tokens/$ID -H "Authorization: Bearer $ADMIN_TOKEN"
curl -s localhost:9090/__admin/tokens/self -H "Authorization: Bearer $CI_TOKEN"
curl -s -X DELETE localhost:9090/__admin/tokens/$ID -H "Authorization: Bearer $ADMIN_TOKEN"
```

| Endpoint | Body or parameters | Response |
|---|---|---|
| `POST /tokens` | `{name, role, spaces?, global?, expiresInDays?, description?}`; exactly one of `spaces` and `global: true`; `expiresInDays` 1 to `FASTMOCK_TOKENS_MAX_DAYS`, default 90 | `201` with `token` (the value, once) and the metadata; `409` when the name is taken by an active or a static token |
| `GET /tokens` | `?space=`, `?status=active\|expired\|revoked` | `{"tokens": [...]}`: metadata of the tokens the caller may manage, newest first |
| `GET /tokens/{id}`, `GET /tokens/self` | | metadata: `id`, `name`, `role`, `spaces`, `global`, `description`, `createdBy`, `createdAt`, `expiresAt`, `lastUsedAt`, `revokedAt`, `status` |
| `GET /tokens/options` | | what the caller may choose when creating a token: `maxDays`, `defaultDays` and every role with its `permissions`, `builtin`, and where the caller may grant it (`global`, `spaces`); needs `tokens:write` somewhere |
| `DELETE /tokens/{id}` | | the metadata with `status: revoked`; every replica rejects the token within 10 seconds |

Who may create, see and revoke which tokens (the escalation rule), expiry, caching and memory-mode behaviour are
described in [Access control: managed tokens](../operations/access-control.md#managed-tokens).

## Errors

Every error from the admin API has the same JSON shape:

```json
{"error": "stub not found", "details": []}
```

`error` is a single human-readable sentence; `details` lists individual problems when there are several (for
example the mismatches of a failed verification). Some responses add fields: `403` adds `required`, `422` from
`/verify` adds the actual `count` and `nearMisses`. Errors never contain stack traces, internal paths or database
messages; internal failures answer `500 {"error":"internal error","details":[]}` and are logged with details on the
server.

| Status | When |
|---|---|
| 400 | invalid parameter, invalid stub, pattern, tag, file name or document; a request body that is not valid JSON or does not fit the schema (unknown field, wrong type); an import that yields no stub |
| 401 / 403 | see [Authentication](#authentication) |
| 404 | the stub, request, file, key or route does not exist |
| 405 | method not allowed on this route |
| 409 | conflict: stub id exists on `POST /stubs`, incompatible gRPC descriptor, too many files |
| 413 | body larger than the route allows |
| 415 | `POST /import` cannot detect or convert the format; missing JSON content type |
| 422 | a verification that failed (`/verify`, `/verify/sequence`), or a journal scan over its work budget |

## Pagination and filtering

List endpoints take `limit` (1..10000) and `offset` (default 0) and return the total before pagination:

```sh
curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  "http://localhost:9090/__admin/stubs?tag=billing&q=invoice&enabled=true&limit=20&offset=40"
```

```json
{"total": 57, "offset": 40, "limit": 20, "stubs": [ ... ]}
```

| Endpoint | Filters | Default page |
|---|---|---|
| `GET /stubs` | `tag`, `q` (case-insensitive substring of the name, `urlPath`, `urlPathPattern` or `urlPathTemplate`), `enabled` | all stubs, newest first |
| `GET /requests` | `source`, `method`, `pathPrefix`, `correlation`, `bodyContains`, `matched`, `stubId`, `since` | 100 entries, newest first |
| `GET /requests/unmatched` | `source`, `since` | 100 entries |
| `GET /audit` | `action` (exact, or prefix with a trailing `*`), `principal`, `since` | 100 events |

`GET /requests` answers `{"requests": [...], "total": n, "offset": o, "limit": l}`.

Collection endpoints also take `space` to limit them to one [space](../operations/access-control.md#spaces); for a
token bound to spaces they cover its spaces by default, and `total` and the pages count only those entries.

## Stubs

```sh
curl -s -X POST http://localhost:9090/__admin/stubs \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"name": "invoice", "request": {"method": "GET", "urlPathTemplate": "/invoices/{id}"},
       "response": {"jsonBody": {"id": "{{request.pathParams.id}}"}},
       "tags": ["billing"], "metadata": {"service": "billing", "owner": {"team": "payments"}}}'
```

`POST` answers `201` with the saved stub (`409` when the id exists), `PUT /stubs/{id}` creates or replaces,
`DELETE /stubs/{id}` answers `204`. `POST /stubs/validate` checks a stub without saving it and answers
`{"valid": false, "error": "..."}` for an invalid one. `GET /export?tag=` returns stubs in the format
`POST /stubs/import` accepts.

## Partial updates (PATCH)

`PATCH /stubs/{id}` applies an [RFC 7396](https://www.rfc-editor.org/rfc/rfc7396) JSON merge patch to the stored
stub: objects merge, `null` removes a field, arrays and scalars replace. The id cannot change. The result is
validated like a `PUT`.

```sh
# disable a stub
curl -s -X PATCH http://localhost:9090/__admin/stubs/$ID \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/merge-patch+json' \
  -d '{"enabled": false}'

# switch it to an error and drop its response headers
curl -s -X PATCH http://localhost:9090/__admin/stubs/$ID \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"enabled": true, "response": {"status": 503, "headers": null}}'
```

`application/json` and `application/merge-patch+json` are both accepted. The response is the patched stub; `404`
when the stub does not exist, `400` when the patch produces an invalid stub.

## Metadata search (WireMock compatible)

Stubs carry free-form `metadata` (any JSON object). Find or delete stubs by it, with the same body WireMock uses:

```sh
curl -s -X POST http://localhost:9090/__admin/stubs/find-by-metadata \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"matchesJsonPath": "$.service", "equalTo": "billing"}'

curl -s -X POST http://localhost:9090/__admin/stubs/remove-by-metadata \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"matchesJsonPath": {"expression": "$.owner.team", "equalTo": "payments"}}'
```

| Field | Meaning |
|---|---|
| `matchesJsonPath` | JSONPath (RFC 9535) that must select at least one node; a string, or `{"expression", "equalTo"\|"contains"\|"matches"}` as in WireMock |
| `equalTo` | a selected node equals this value (strings compare as text, other JSON values structurally) |
| `contains` / `matches` | a selected node's text contains the string / matches the whole regular expression |
| `equalToJson` | the metadata equals this JSON; with `ignoreExtraElements: true` it only has to contain it |

`find-by-metadata` answers like `GET /stubs` (`{"total", "offset", "stubs"}`) and needs only viewer;
`remove-by-metadata` answers `{"deleted": 2, "ids": [...]}` and needs editor. Stubs without metadata never match.

## Unused stubs

`GET /stubs/unused?since=2026-11-01T00:00:00Z` lists stubs that no journal entry matched (since the given
instant). The answer is computed from the request journal, so it covers only the journal retention window
(`FASTMOCK_JOURNAL_RETENTION`) and only requests made while the journal was enabled.

## Body files

Body files are stored by the server (in PostgreSQL with the default storage) and used by stubs through
`response.bodyFileName` instead of inline bodies.

```sh
# upload or replace: raw body, the Content-Type is stored with the file
curl -s -X PUT http://localhost:9090/__admin/files/invoices/42.json \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  --data-binary @invoice-42.json
# {"name":"invoices/42.json","contentType":"application/json","size":812,"updatedAt":"2026-11-19T10:00:00.000Z"}

curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" http://localhost:9090/__admin/files
# {"total":1,"files":[{"name":"invoices/42.json","contentType":"application/json","size":812,"updatedAt":"..."}]}

curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" -o out.json http://localhost:9090/__admin/files/invoices/42.json
curl -s -X DELETE -H "Authorization: Bearer $FASTMOCK_TOKEN" http://localhost:9090/__admin/files/invoices/42.json
```

`PUT` answers `201` for a new file and `200` for a replaced one. Downloads come back with the stored content type,
`Content-Disposition: attachment` and a sandboxing `Content-Security-Policy`, so an uploaded HTML file is never
rendered on the admin origin.

| Rule | Value |
|---|---|
| name | `[A-Za-z0-9._-/]`, 1..512 bytes, relative, no `..`, no empty or `.` segments |
| size | at most 10 MiB per file (`413` above) |
| count | at most 10000 files (`409` above) |
| content type | the request's `Content-Type`, default `application/octet-stream`, at most 255 characters |

Every upload and delete is recorded in the audit log (`file.put`, `file.delete`).

## Audit log

Every change through the admin API is recorded with who, what and when. Admins read it with filters:

```sh
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
  "http://localhost:9090/__admin/audit?action=stub.*&principal=ci&since=2026-11-19T00:00:00Z&limit=50"
```

```json
{"events": [{
  "id": 912, "at": "2026-11-19T10:02:11.120Z", "principal": "ci", "role": "editor",
  "action": "stub.update",
  "detail": {"id": "4f0c…", "name": "invoice", "changesTruncated": false, "changes": [
    {"path": "/response/status", "before": 200, "after": 503},
    {"path": "/enabled", "before": true, "after": false}
  ]}
}]}
```

Stub updates (`PUT`, `PATCH`) store the changed fields as JSON Pointer paths with their old and new values (at most
100 changes, values larger than 16 KiB are replaced by `{"truncated": true, "bytes": n}`); `stub.delete` stores
the deleted stub in `before`. Sensitive header values and JSON fields named like secrets are masked (`***`) before
they are written. Other actions: `stub.create`, `stub.delete_many`, `stub.delete_by_metadata`, `stub.import`,
`import.<format>`, `journal.clear`, `journal.remove`, `file.put`, `file.delete`, `recording.start`,
`recording.stop`, `recording.snapshot`, `scenario.set`, `scenario.reset`, `kv.set`, `kv.delete`, `chaos.set`,
`chaos.clear`, `grpc.descriptor.put`, `grpc.descriptor.delete`, `callbacks.clear`, `reset`, `token.create`,
`token.revoke` (id, name, kind, role, scopes and expiry of the managed token, never its value). Audit entries are also
logged with the `audit` tracing target.

## Version and info

```sh
curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" http://localhost:9090/__admin/version
# {"version":"0.1.0","gitSha":"3f2c1ab","features":["grpc","kafka","sse","chaos","recording","verification","files","import-openapi","import-wiremock","import-postman","import-har"]}
```

`gitSha` is set when the binary was built with the `FASTMOCK_GIT_SHA` environment variable, otherwise `null`.
`GET /info` returns the version, storage kind, uptime, number of loaded stubs, plugins and the caller's identity:
`principal` with `name`, `kind`, `role` and `permissions` (`global` and per-space lists). `GET /spaces` lists the
spaces the caller can see with its permissions in each ([Access control](../operations/access-control.md#inspecting-your-permissions)).

## Limits

| Limit | Value |
|---|---|
| request body on the admin API | 32 MiB (64 MiB hard limit on the listener) |
| body file | 10 MiB, 10000 files |
| stubs per import | 10000 |
| `limit` on list endpoints | 1..10000 |
| JSONPath / regular expression in metadata search | 1024 bytes / 256 KiB compiled |
| `GET /audit` filters | `action` and `principal` at most 128 bytes |

## Metrics

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_admin_changes_total` | `action` | audited admin changes |
| `fastmock_admin_auth_failures_total` | `reason` (`token`, `role`, `permission`, `origin`) | rejected admin requests (`role` for tokens holding only global built-in roles, `permission` otherwise) |
| `fastmock_admin_errors_total` | | internal errors in admin handlers |
| `fastmock_files_written_total` | | body file uploads |
| `fastmock_file_size_bytes` | | histogram of uploaded file sizes |
| `fastmock_imports_total` | `format`, `dry_run` | imports |
| `fastmock_admin_verifications_total` | `kind` (`count`, `sequence`), `result` (`pass`, `fail`) | verifications, see [Verification](verification.md) |
| `fastmock_snapshot_stubs_total` | | stubs generated by snapshots, see [Recording](recording.md) |

See also: [Verification](verification.md), [Recording](recording.md), [Importing](importing.md).
