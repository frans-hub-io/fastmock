# Access control

FastMock checks every admin API call against a permission. The three roles `viewer`, `editor` and `admin` are
built-in sets of permissions; custom roles, spaces and bindings come from an optional access file. Spaces let
several teams share one FastMock instance: a team bound to a space sees and changes only the stubs, journal
entries, state, files, callbacks and contract tests of that space, and can be limited to its own part of the
data-plane URL space.

- [Without an access file](#without-an-access-file)
- [Permissions](#permissions)
- [Built-in roles](#built-in-roles)
- [The access file](#the-access-file)
- [Custom roles](#custom-roles)
- [Bindings](#bindings)
- [Managed tokens](#managed-tokens)
- [OIDC sign-in and bearer JWTs](#oidc-sign-in-and-bearer-jwts)
- [Spaces](#spaces)
- [Selecting a space in the API](#selecting-a-space-in-the-api)
- [Ownership of the data plane](#ownership-of-the-data-plane)
- [What is scoped and what is global](#what-is-scoped-and-what-is-global)
- [Inspecting your permissions](#inspecting-your-permissions)
- [Errors](#errors)
- [Examples](#examples)
- [Migrating an existing installation](#migrating-an-existing-installation)

## Without an access file

Nothing changes: every entry of `FASTMOCK_ADMIN_TOKENS` / `FASTMOCK_ADMIN_TOKENS_FILE` (`name:role:sha256hex`) is a
global binding of a built-in role, and every endpoint needs the same role as before
([Admin API: roles per endpoint](../guide/admin-api.md#roles-per-endpoint)). Existing stubs, journal entries and
state belong to the global area.

## Permissions

A permission is `resource:action`. *Space* permissions can be granted in single spaces; *global* permissions only
everywhere.

| Permission | Endpoints | Scope |
|---|---|---|
| `stubs:read` | `GET /stubs`, `/stubs/{id}`, `/stubs/unused`, `/export`; `POST /stubs/find-by-metadata`, `/stubs/validate` | space |
| `stubs:write` | `POST /stubs`, `PUT`/`PATCH`/`DELETE /stubs/{id}`, `DELETE /stubs?tag=`, `POST /stubs/remove-by-metadata` | space |
| `stubs:purge` | `DELETE /stubs` without `tag` | global |
| `imports:run` | `POST /stubs/import`, `/import`, `/import/*` | space (the target space of the imported stubs) |
| `journal:read` | `GET /requests`, `/requests/{id}`, `/requests/count`; `POST /requests/count`, `/requests/near-misses`, `/requests/find`, `/verify`, `/verify/sequence` | space |
| `journal:delete` | `DELETE /requests`, `POST /requests/remove` | space |
| `files:read`, `files:write` | `GET /files*`; `PUT`/`DELETE /files/{name}` | space |
| `recordings:read`, `recordings:write` | `GET /recordings`; `POST /recordings/start`, `/stop`, `/snapshot` | global |
| `callbacks:read`, `callbacks:delete` | `GET /callbacks`; `DELETE /callbacks` | space |
| `state:read`, `state:write` | `GET /scenarios`, `/kv/{key}`, `/stubs/counters`; `PUT /scenarios/{name}`, `DELETE /scenarios`, `PUT`/`DELETE /kv/{key}`, `DELETE /stubs/counters` | space |
| `chaos:read`, `chaos:write` | `/chaos`, `/settings` | global |
| `grpc:read`, `grpc:write` | `/grpc/descriptors*` | global |
| `contracts:read` | `GET /contract-tests`, `/contract-tests/{id}` (verdicts) | space |
| `contracts:details` | request and response excerpts and error texts of contract test results | space |
| `contracts:write` | `POST /contract-tests`, `DELETE /contract-tests/{id}` | space |
| `contracts:secrets` | `secretHeaders` in `POST /contract-tests` | space |
| `info:read` | `GET /info`, `/version`, `/plugins`, `/spaces` | global, implied by any binding |
| `audit:read` | `GET /audit` | global |
| `system:reset` | `POST /reset` | global |
| `tokens:read`, `tokens:write` | `GET /tokens`, `/tokens/{id}`; `POST /tokens`, `DELETE /tokens/{id}` ([managed tokens](#managed-tokens)) | space (the scopes of the token) |

Endpoints without a space of their own need the permission globally:

| Endpoints | Permission (global) |
|---|---|
| `GET /requests/unmatched` | `journal:read` (unmatched requests belong to no space) |
| `GET /services*`, `GET /async-mocks*` | `stubs:read` |
| `DELETE /services/{id}`, `PUT`/`DELETE /async-mocks/{id}`, `POST /async-mocks/{id}/trigger` | `stubs:write` |
| `POST /import/asyncapi`, `/import/graphql`, `/import/soapui`, `/import/wsdl`, `/import/microcks` (and `POST /import` detecting one of these formats) | `imports:run` |
| plugin admin routes `/plugins/{name}/...` | `stubs:read` for `GET`, `stubs:write` otherwise |

Credentials inside stubs are shown only to principals with `stubs:write` in the stub's space; others get them
masked ([Security: redaction](security.md#redaction)).

## Built-in roles

| Role | Permissions |
|---|---|
| `viewer` | every `*:read` permission except `audit:read` and `tokens:read` |
| `editor` | `viewer` plus `stubs:write`, `imports:run`, `journal:delete`, `files:write`, `recordings:write`, `callbacks:delete`, `state:write`, `chaos:write`, `grpc:write`, `contracts:details`, `contracts:write` |
| `admin` | every permission |

The built-in roles cannot be redefined.

## The access file

`FASTMOCK_ACCESS_FILE` names a YAML file with custom roles, spaces and bindings. It is read once at start-up,
like the token file; change it and restart (with Helm, `helm upgrade` rolls the pods). A file with an error stops
the server, and the message names the entry, for example
``access file /etc/fastmock/access/access.yaml: roles[0] (stub-author): unknown permission `stubs:wrte` ``.

```yaml
roles:
  - name: stub-author
    permissions: [stubs:read, stubs:write, imports:run, journal:read, state:read]
spaces:
  - name: payments
    description: Payment provider mocks
    pathPrefixes: [/payments/, /psp/]
    hosts: [psp.example.com]
  - name: billing
bindings:
  - subject: group:payments-dev
    role: editor
    spaces: [payments]
  - subject: group:platform
    role: admin
    global: true
  - subject: token:ci-billing
    role: stub-author
    spaces: [billing]
```

All three lists are optional. Unknown fields are errors.

## Custom roles

| Field | Meaning |
|---|---|
| `name` | `[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`, not `viewer`, `editor` or `admin`. |
| `permissions` | Permission names; `resource:*` grants every permission of a resource (`stubs:*`, `contracts:*`). Unknown names are start-up errors. |
| `description` | Optional text. |

A custom role can be used in a binding and as the role of a token entry: `ci-billing:stub-author:<sha256>` in
`FASTMOCK_ADMIN_TOKENS` is a global binding of `stub-author`.

## Bindings

A binding gives a role to a subject, either everywhere (`global: true`) or in a list of spaces (`spaces`, exactly
one of the two).

| Subject | Matches |
|---|---|
| `token:<name>` | the static token entry with that name in `FASTMOCK_ADMIN_TOKENS` or `FASTMOCK_ADMIN_TOKENS_FILE` (a binding for an unknown token name is a start-up error) |
| `user:<username>` | an [OIDC user](#oidc-sign-in-and-bearer-jwts) by the value of the username claim |
| `group:<group>` | an [OIDC user](#oidc-sign-in-and-bearer-jwts) who has this group in the groups claim |

A token entry with a role is a global binding of that role, as before. To give a token only space bindings,
leave its role empty: `ci-billing::<sha256>`. Such an entry needs at least one `token:ci-billing` binding.

A principal is allowed a permission in a space when one of its bindings grants it globally, or grants it in that
space and the permission is a space permission. A binding of `admin` in a space therefore grants every space
permission there but none of the global ones (`stubs:purge`, `system:reset`, `audit:read` and so on).
`info:read` comes with any binding.

## Managed tokens

Besides the static entries of `FASTMOCK_ADMIN_TOKENS`, tokens can be created and revoked through the admin API
without a restart. A managed token has a name, one role (built-in or custom) and a scope: global or a list of
spaces. Its expiry is mandatory.

```sh
curl -s -X POST localhost:9090/__admin/tokens -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name": "ci-billing", "role": "editor", "spaces": ["billing"], "expiresInDays": 30, "description": "CI of the billing team"}'
```

```json
{"token": "fm_3f9c...", "id": "019a8f1e-...", "name": "ci-billing", "role": "editor", "spaces": ["billing"],
 "global": false, "description": "CI of the billing team", "createdBy": "ops", "createdAt": "2026-11-20T09:00:00Z",
 "expiresAt": "2026-12-20T09:00:00Z", "lastUsedAt": null, "revokedAt": null, "status": "active"}
```

The value (`fm_` and 64 hex digits, the same format as `fastmock hash-token --generate`) is returned only in this
response; FastMock stores its SHA-256 hash. Store it in your secret manager right away.

| Field | Meaning |
|---|---|
| `name` | `[A-Za-z0-9._@-]`, at most 128 characters. Unique among active tokens and different from every static token name (`409` otherwise); a name becomes free again when its token is revoked or expired. It appears as the principal name in `/info`, logs and the audit log. |
| `role` | `viewer`, `editor`, `admin` or a custom role of the access file. |
| `spaces` or `global: true` | Exactly one of them. A token in spaces gets the role's space permissions in each of them (like a space binding). |
| `expiresInDays` | 1 to `FASTMOCK_TOKENS_MAX_DAYS` (default 365); 90 when omitted. |
| `description` | Optional, at most 1024 bytes. |

| Endpoint | Permission |
|---|---|
| `POST /tokens` | the escalation rule below |
| `GET /tokens` | `tokens:read`; lists the tokens for which the caller holds `tokens:read` in every scope (global tokens need it globally). Filters: `?space=`, `?status=active\|expired\|revoked`. |
| `GET /tokens/{id}` | `tokens:read` in every scope of the token; others answer 404 |
| `DELETE /tokens/{id}` | `tokens:write` in every scope of the token, or the token itself (self-revocation) |
| `GET /tokens/self` | the managed token itself (any static token gets 404) |

**Escalation rule.** The caller can create a token only if, in each requested scope, it holds `tokens:write` and
every permission the role grants there: for a global token, globally; for a token in space `s`, the space
permissions of the role in `s`. A token therefore never holds more than its creator, and a caller bound only to
spaces cannot create a global token. A denial names the first missing permission:
`403 {"error":"requires permission contracts:secrets in space billing to grant role admin there",...,"required":"contracts:secrets"}`.
The built-in `editor` role lacks `tokens:write`; to let a team manage its own tokens, bind a custom role with
`tokens:read` and `tokens:write` in its space:

```yaml
roles:
  - name: team-lead
    permissions: [stubs:*, imports:run, journal:*, files:*, callbacks:*, state:*, contracts:read,
      contracts:details, contracts:write, tokens:read, tokens:write]
bindings:
  - subject: token:billing-lead
    role: team-lead
    spaces: [billing]
```

**Lifecycle.**

- *Use*: static tokens are checked first, in constant time; a value in the managed format is then looked up by
  its hash. Each replica caches the result of a lookup (including "unknown") for at most 10 seconds.
- *Revocation* (`DELETE /tokens/{id}`) takes effect at once on the replica that handled it and within 10 seconds
  on every other replica of the same storage. Revoking again answers `200` and changes nothing.
- *Expiry* is checked on every request against the stored time, so an expired token stops working at once
  everywhere (`401 {"error":"unauthorized","details":["token expired"]}`).
- `lastUsedAt` is written in the background at most once per minute per token; it never delays a request.
- Revoked and expired tokens stay listed with `status` `revoked` or `expired` for 90 days, then they are deleted.
- If the role of a token disappears from the access file, the token is rejected until the role is back.
- In memory mode (`FASTMOCK_STORAGE=memory`) managed tokens live in the process and are lost on restart. With
  PostgreSQL they are stored in `fm_api_tokens` and shared by all replicas.

The token's own principal has kind `apiToken` (`GET /__admin/info`), and the audit log records `token.create` and
`token.revoke` with the token's id, name, kind, role, scopes and expiry; the value never appears in logs, the audit
log or later responses.

## OIDC sign-in and bearer JWTs

FastMock can sign users in through an OpenID Connect provider (Keycloak, Microsoft Entra ID, Okta, Auth0, ADFS,
Dex, Authentik and others) and accept the provider's JWTs as bearer tokens on the admin API. FastMock implements
only OIDC: LDAP, Active Directory and SAML users come through the provider (for Keycloak: user federation and
identity brokering, see [below](#keycloak)). Static and managed tokens keep working next to it, and the UI's token
login stays available.

### How it works

The UI signs in through the admin port (backend for frontend): the browser never sees an OIDC token.

1. `GET /__admin/auth/login?return=/ui/stubs` answers `302` to the provider's authorization endpoint with
   `response_type=code`, `state`, `nonce` and a PKCE `code_challenge` (S256). The pending login (nonce, PKCE
   verifier, return path) is kept for 10 minutes in the hot store (Valkey, or process memory in memory mode) under
   the hash of `state`. A short-lived cookie `__Host-fastmock_login` ties the callback to the browser that started
   the login, so a callback URL sent to someone else does not sign them in.
2. The provider redirects to `GET /__admin/auth/callback?code=...&state=...`. FastMock takes the pending login
   (each `state` works once), checks the login cookie, exchanges the code (with the client secret and the PKCE
   verifier) and validates the ID token: signature against the provider's JWKS, `iss`, `aud`, `azp` when
   present, `exp`, `nbf`, `iat` (60 seconds of clock skew) and `nonce`. Accepted algorithms: RS256, PS256, ES256
   and EdDSA (Ed25519); `none` and HMAC algorithms are always rejected. A token signed with an unknown key id
   makes FastMock fetch the JWKS again, at most once a minute.
3. The username and groups come from the ID token (`FASTMOCK_OIDC_USERNAME_CLAIM`, default
   `preferred_username`; `FASTMOCK_OIDC_GROUPS_CLAIM`, default `groups`). When the ID token has no groups claim
   (or no username claim), FastMock asks the userinfo endpoint. The groups claim may be a name or a dotted path
   (`realm_access.roles`); a claim whose name itself contains dots (`https://example.com/groups`) is found by its
   full name first.
4. FastMock computes the user's bindings (`user:<username>` and `group:<group>` subjects). **Without any binding no
   session is created**: the browser goes to `/ui/?loginError=no_access` and the login is audited as failed.
   Otherwise FastMock creates a session and redirects to the `return` path with the session cookie.

`return` must be a relative path under `/ui/`. Values starting with `//`, containing a backslash, a scheme,
`..` segments, control characters or the same tricks percent-encoded answer `400`.

| `loginError` | Meaning |
|---|---|
| `no_access` | the user signed in at the provider but no binding matches the user or its groups |
| `invalid_state` | unknown, expired (10 minutes) or already used `state`, or the login cookie is missing or different |
| `idp_error` | the provider answered with an error (for example the user cancelled) |
| `failed` | code exchange or ID token validation failed, or the username claim is missing; the log says why |
| `unavailable` | the provider has not been discovered yet or the session store is unavailable |

### Sessions

| Property | Value |
|---|---|
| Cookie | `__Host-fastmock_session`, `Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`, `Max-Age` = `FASTMOCK_OIDC_SESSION_TTL` |
| Value | 256 random bits (43 base64url characters) |
| Server side | in the hot store under the SHA-256 of the value: user name, group names, creation and last-seen time |
| Lifetime | absolute `FASTMOCK_OIDC_SESSION_TTL` (default `8h`) and idle `FASTMOCK_OIDC_SESSION_IDLE` (default `1h`) |
| Replicas | every replica and every cluster sharing the Valkey sees the same sessions; in memory mode they live in the process |

The last-seen time is written back at most once a minute (and at least four times per idle period), so the idle
timeout can be exceeded by up to a quarter of it. The bindings are computed from the access file on every request,
so a changed access file applies to existing sessions after the restart. The groups are those of the sign-in; a
user removed from a group at the provider keeps it until the session ends.

An expired or unknown session answers `401 {"error":"unauthorized","details":["session expired or unknown: sign in again"]}`
and clears the cookie.

`POST /__admin/auth/logout` deletes the session and clears the cookie. When the provider publishes an
`end_session_endpoint`, the response carries `logoutUrl` (with `client_id` and `post_logout_redirect_uri` set to
the admin URL's `/ui/`) so the UI can end the provider session too:

```json
{"loggedOut": true, "logoutUrl": "https://sso.example.com/realms/mocks/protocol/openid-connect/logout?client_id=fastmock&post_logout_redirect_uri=https%3A%2F%2Fmock-admin.example.com%2Fui%2F"}
```

`GET /__admin/auth/config` (no authentication) tells the login page what is available:
`{"oidc": true, "tokenLogin": true}`.

**Local development over http.** Browsers keep `Secure` cookies only on https, except on `localhost` where most
browsers accept them over plain http too. For other local setups set `FASTMOCK_OIDC_INSECURE_COOKIE=true`: the
cookies are then called `fastmock_session` and `fastmock_login` and lose `Secure` (the `__Host-` prefix requires
it). The switch is accepted only when `FASTMOCK_OIDC_REDIRECT_URL` points to `localhost`, `127.0.0.1` or `::1`.

### CSRF protection

Requests authenticated by the session cookie that are not `GET` or `HEAD` must carry the header
`X-FastMock-Request: 1`; otherwise they answer `403 {"error":"forbidden","details":["missing X-FastMock-Request: 1 header"]}`.
A cross-origin page cannot add the header without a CORS preflight, which the admin API never grants. The header
comes on top of the `Origin` check that applies to every admin request. `POST /__admin/auth/logout` needs it too.
Requests with an `Authorization` header are not affected.

### Bearer JWTs on the admin API

With `FASTMOCK_OIDC_API_AUDIENCE` set, a bearer value that looks like a JWT (three base64url parts) is validated
like the ID token, but against that audience, without `nonce` and without `azp` (the token may have been issued to
another client, for example a CI pipeline using client credentials or a CLI using the device flow). `iat` is
optional. The username claim falls back to `sub` when absent; the groups claim is read from the token (userinfo is
not called). Use an audience different from the client id, so ID tokens of the UI client are not accepted as API
tokens.

Precedence: a request with an `Authorization` header is decided by that header alone, in this order: static
tokens, managed `fm_` tokens, JWTs. The session cookie is used only when there is no `Authorization` header. A
JWT for a user without bindings answers `403` with
`"details":["user alice has no access: the access file binds neither user:alice nor any of the user's groups"]`.
An invalid JWT answers `401` with `WWW-Authenticate: Bearer error="invalid_token"`.

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `FASTMOCK_OIDC_ISSUER` | none | Issuer URL; OIDC is off without it. https (plain http only for `localhost`, `127.0.0.1` and `::1`, for tests). Must equal the `issuer` of the discovery document exactly. |
| `FASTMOCK_OIDC_CLIENT_ID` | none | Client id (required). |
| `FASTMOCK_OIDC_CLIENT_SECRET` / `_SECRET_FILE` | none | Client secret, one of the two (required). |
| `FASTMOCK_OIDC_REDIRECT_URL` | none | Public admin URL followed by `/__admin/auth/callback` (required, https except on localhost). Register it at the provider. |
| `FASTMOCK_OIDC_SCOPES` | `openid profile email` | Space- or comma-separated; must include `openid`. |
| `FASTMOCK_OIDC_USERNAME_CLAIM` | `preferred_username` | Claim (or dotted path) with the user name for `user:` bindings. |
| `FASTMOCK_OIDC_GROUPS_CLAIM` | `groups` | Claim (or dotted path such as `realm_access.roles`) with the groups for `group:` bindings: an array of strings or one string. |
| `FASTMOCK_OIDC_API_AUDIENCE` | none | Enables bearer JWTs on the admin API for this audience. |
| `FASTMOCK_OIDC_CA_FILE` | none | PEM bundle added to the trusted roots for the provider connections. |
| `FASTMOCK_OIDC_SESSION_TTL` | `8h` | Absolute session lifetime. |
| `FASTMOCK_OIDC_SESSION_IDLE` | `1h` | Idle timeout. |
| `FASTMOCK_OIDC_ALLOWED_HOSTS` | none | Hosts besides the issuer's that discovery endpoints may use (comma-separated). |
| `FASTMOCK_OIDC_INSECURE_COOKIE` | `false` | Cookies without `Secure` and `__Host-` for local http development (localhost redirect URL only). |

Everything is checked at start-up; errors name the variable. With OIDC configured, `FASTMOCK_ADMIN_TOKENS` may be
empty (sign-in only), and `FASTMOCK_AUTH_DISABLED=true` is refused.

**Discovery and egress.** At start-up FastMock reads `<issuer>/.well-known/openid-configuration` and the JWKS. If
the provider cannot be reached within 5 seconds the server starts anyway (the data plane and token
authentication work) and retries in the background with backoff up to one minute; until then
`/__admin/auth/login` answers `503` and the gauge `fastmock_oidc_ready` is 0. A discovery document that FastMock
must not use stops the start with a clear error: a different `issuer`, an endpoint on a host other than the
issuer's and not in `FASTMOCK_OIDC_ALLOWED_HOSTS`, plain http outside loopback, or no PKCE S256 support. All
provider calls (discovery, JWKS, token, userinfo) use a dedicated HTTP client: only to the issuer host and the
allowed hosts, redirects only to those hosts (at most three), 5 seconds to connect, 10 seconds per request,
responses up to 1 MiB, the system roots plus `FASTMOCK_OIDC_CA_FILE`. The client honours `HTTPS_PROXY` and
`NO_PROXY`. These calls are not subject to the [egress policy](security.md#egress-policy) for stub callbacks, so
the provider may live on a private network. The client authenticates at the token endpoint with
`client_secret_basic`, or `client_secret_post` when the provider supports only that. The JWKS is refreshed every
hour and on an unknown key id.

Starting logins is limited to a burst of 100 and 10 per second per replica (`429` above), so anonymous callers
cannot fill the hot store with pending logins.

### Keycloak

1. Clients → *Create client*: type *OpenID Connect*, client id `fastmock`. *Client authentication* on (a
   confidential client), *Standard flow* on, *Direct access grants* off. PKCE: in *Advanced* set *Proof Key for
   Code Exchange Code Challenge Method* to `S256`.
2. *Valid redirect URIs*: `https://mock-admin.example.com/__admin/auth/callback`. *Valid post logout redirect
   URIs*: `https://mock-admin.example.com/ui/`. *Web origins*: leave empty (the browser never calls Keycloak with
   JavaScript).
3. *Credentials*: copy the client secret into a Secret (`FASTMOCK_OIDC_CLIENT_SECRET_FILE`).
4. Groups: *Client scopes* → `fastmock-dedicated` → *Add mapper* → *By configuration* → *Group Membership*,
   token claim name `groups`, *Full group path* **off** (otherwise the values are `/payments-dev`), added to the
   ID token, the access token and userinfo. To bind realm roles instead, set
   `FASTMOCK_OIDC_GROUPS_CLAIM=realm_access.roles`.
5. For API JWTs, add an *Audience* mapper with *Included custom audience* `fastmock-api` to the client scope that
   CI clients request, and set `FASTMOCK_OIDC_API_AUDIENCE=fastmock-api`. A CI pipeline then uses a separate
   confidential client with *Service accounts* on (client credentials); its user name is
   `service-account-<client id>`, so bind `user:service-account-ci-payments`, or give the service account groups.
6. `FASTMOCK_OIDC_ISSUER=https://sso.example.com/realms/mocks`.

**LDAP and Active Directory users.** Configure *User federation* → *LDAP* in the realm (vendor *Active Directory*
or *Other*), and add a *group-ldap-mapper* (mode `READ_ONLY`) that imports the LDAP groups. The users sign in
through Keycloak with their directory password; their directory groups arrive in the `groups` claim and match
`group:` bindings by name. FastMock never talks to LDAP.

**SAML.** Add the SAML identity provider under *Identity providers* (identity brokering) and map its group or role
attribute to Keycloak groups with an *Advanced attribute to group* or *Attribute importer* mapper. Users choose the
SAML provider on the Keycloak login page; FastMock still sees an OIDC sign-in.

### Microsoft Entra ID

1. *App registrations* → *New registration*; *Redirect URI* type *Web*:
   `https://mock-admin.example.com/__admin/auth/callback`. Add `https://mock-admin.example.com/ui/` as a second
   *Web* redirect URI, so that the provider returns the browser to the UI after logout
   (`post_logout_redirect_uri`).
2. *Certificates & secrets* → *New client secret*; store it in a Secret.
3. *Token configuration* → *Add groups claim*: *Security groups* (or *Groups assigned to the application*, which
   avoids the 200-group limit of the token). The values are group object ids, so bindings look like
   `group:5f1c3f6e-0000-4000-8000-000000000001`. For groups synchronised from Active Directory you can emit
   `sAMAccountName` instead and bind by name.
4. Settings: `FASTMOCK_OIDC_ISSUER=https://login.microsoftonline.com/<tenant id>/v2.0`,
   `FASTMOCK_OIDC_CLIENT_ID=<application id>`, `FASTMOCK_OIDC_USERNAME_CLAIM=preferred_username` (the UPN) or `email`.
5. For API JWTs, *Expose an API* with an application id URI such as `api://fastmock` and set
   `FASTMOCK_OIDC_API_AUDIENCE` to the `aud` value of the access tokens (the application id for v2 tokens). App
   roles assigned to service principals arrive in the `roles` claim; set `FASTMOCK_OIDC_GROUPS_CLAIM=roles` if you
   bind by app role.

Users with more groups than fit in the token (more than 200) get an overage claim instead of `groups`; FastMock
then sees no groups. Use *Groups assigned to the application* to avoid it.

### Other providers

Any provider with OIDC discovery, the authorization code flow with PKCE S256, a confidential client and RS256,
PS256, ES256 or EdDSA signatures works:

1. Create a confidential web client with the redirect URI `https://<admin host>/__admin/auth/callback`.
2. Make the provider put the user's groups into the ID token (or userinfo) as an array of strings, and note the
   claim name (`groups`, `roles`, a namespaced claim such as `https://example.com/groups`, or a nested path).
3. Check the issuer: `curl https://<issuer>/.well-known/openid-configuration | jq .issuer` must print exactly
   the value of `FASTMOCK_OIDC_ISSUER`, including a trailing slash if there is one.
4. If the token or JWKS endpoint lives on another host than the issuer, list it in `FASTMOCK_OIDC_ALLOWED_HOSTS`.

ADFS works as an OIDC provider (*Server application* plus *Web API* in an application group); its groups come
from a claim issuance rule.

### Example

```yaml
spaces:
  - name: payments
    pathPrefixes: [/payments/]
bindings:
  - subject: group:payments-dev
    role: editor
    spaces: [payments]
  - subject: group:platform
    role: admin
    global: true
  - subject: user:service-account-ci-payments
    role: editor
    spaces: [payments]
```

```sh
FASTMOCK_OIDC_ISSUER=https://sso.example.com/realms/mocks
FASTMOCK_OIDC_CLIENT_ID=fastmock
FASTMOCK_OIDC_CLIENT_SECRET_FILE=/etc/fastmock/oidc/client-secret
FASTMOCK_OIDC_REDIRECT_URL=https://mock-admin.example.com/__admin/auth/callback
FASTMOCK_OIDC_API_AUDIENCE=fastmock-api
```

```sh
# a CI job with a client-credentials token from the provider
TOKEN=$(curl -s -d grant_type=client_credentials -u "ci-payments:$CI_SECRET" \
  https://sso.example.com/realms/mocks/protocol/openid-connect/token | jq -r .access_token)
curl -s https://mock-admin.example.com/__admin/stubs?space=payments -H "Authorization: Bearer $TOKEN"
```

Login, failed login and logout are audit events (`auth.login` with the group names, `auth.login_failed` with the
reason, `auth.logout`); changes made by a user record `principalKind: user`. Codes, tokens, session ids, the
client secret and the PKCE verifier are never logged.

## Spaces

A space is a named partition, named like a Kubernetes namespace (`[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`).

| Field | Meaning |
|---|---|
| `name` | The space name. |
| `description` | Optional text, shown by `GET /__admin/spaces`. |
| `pathPrefixes` | Optional data-plane path prefixes that the space's stubs must stay under ([ownership](#ownership-of-the-data-plane)). |
| `hosts` | Optional `Host` values the space's stubs must match. |

What belongs to a space:

- **Stubs** carry a `space` field. It is set on create and never changes: `PUT` and `PATCH` keep the stored
  space and reject a different one (move a stub by deleting and creating it, both authorised).
- **Journal entries** get the space of the stub that matched. Requests that no stub matched, and requests
  matched by global stubs, belong to no space and are visible only with global `journal:read`.
- **Callback jobs** get the space of the stub that scheduled them.
- **Scenario states** of stubs in a space are kept under `scenario:<space>/<name>` and **key-value entries**
  written for a space under `kv:<space>/<key>` (in Valkey: after `FASTMOCK_VALKEY_PREFIX`). Two spaces can use the
  same scenario name or key independently. Templates of a stub in a space see that space's scenario states.
  Global keys stay `scenario:<name>` and `kv:<key>`, so existing state needs no migration.
- **Body files** of a space are stored under `<space>/`. A stub in space `s` resolves `bodyFileName` in `s/`
  first, then among the global files. Files written by a principal bound to a space go to its space.
- **Contract test runs** carry the space they were started in.

Messages published to Kafka, gRPC descriptors, chaos rules, global settings, recordings, async mocks and the
service catalog are not scoped. The data plane does not know spaces: every enabled stub competes for every
request as before; [ownership](#ownership-of-the-data-plane) keeps teams apart.

A request naming a space that the access file does not define answers 404.

Global scenario names, key-value keys and file names that start with `<space>/` share the storage key of that
space's entries. Avoid such names in the global area once spaces exist.

## Selecting a space in the API

Collection endpoints take a `space` query parameter; stubs also take a `space` field.

| Caller | Reads without `space` | Writes without `space` |
|---|---|---|
| holds the permission globally | everything (global area and every space) | the global area |
| bound only to spaces | the union of the spaces where it holds the permission | its space when it has exactly one; otherwise 400 |

With `space`, reads and writes apply to that space, and the caller must hold the permission there (403
otherwise). Deletions that select existing items (`DELETE /requests`, `DELETE /callbacks`, `DELETE /scenarios`,
`DELETE /stubs?tag=`, `POST /stubs/remove-by-metadata`, `POST /requests/remove`, `DELETE /stubs/counters`) select
like reads: without `space` they act on everything the caller may change. Single items (`/stubs/{id}`,
`/requests/{id}`, `/contract-tests/{id}`) are found by id; one in a space the caller cannot read answers 404.
`/kv/{key}`, `/scenarios/{name}` and `/files/{name}` address one entry and resolve the space like writes.

| Endpoint | How the space is given |
|---|---|
| `POST /stubs` | `space` field of the stub or `space` query parameter (they must agree) |
| `POST /stubs/import`, `POST /import`, `/import/openapi`, `/import/wiremock`, `/import/postman`, `/import/har`, `/import/mockserver` | `space` query parameter; every imported stub goes to that space, and `replaceTag`/`replace` removes only that space's stubs with the tag |
| other collection endpoints | `space` query parameter |

Imports into a space are not recorded in the [service catalog](../guide/service-catalog.md), which is global.

## Ownership of the data plane

A space with `pathPrefixes` or `hosts` owns that part of the data plane. Every stub in it must match only
inside it, checked on create, update and import (`400` with the reason):

- with `pathPrefixes`, the stub's URL matcher must be `url`, `urlPath` or `urlPathTemplate` lying under one of
  the prefixes (`/payments/` and `/payments` both allow `/payments/x`, neither allows `/paymentsx`). Regular
  expression matchers (`urlPattern`, `urlPathPattern`) and stubs without a URL matcher are rejected;
- with `hosts`, the stub must have a `request.host` matcher with `equalTo` one of the hosts and nothing else.

So one team cannot answer another team's traffic. Stubs in the global area are not restricted; only principals
with global `stubs:write` can create them.

## What is scoped and what is global

| Data | Scoped by space |
|---|---|
| stubs, journal entries, callback jobs, scenario states, key-value entries, body files, contract test runs, stub counters | yes |
| recordings, chaos rules, global settings, gRPC descriptors, async mocks, service catalog, Kafka messages, audit log | no (global permissions) |

## Inspecting your permissions

`GET /__admin/info` returns the caller's principal:

```json
{"principal": {"name": "ci-billing", "kind": "token", "role": "viewer",
  "permissions": {"global": ["info:read"],
                  "spaces": {"billing": ["stubs:read", "stubs:write", "imports:run", "journal:read", "state:read"]}}}}
```

`permissions.global` holds what is granted everywhere; `permissions.spaces` what is granted only in single spaces
(the effective permissions in a space are both lists together). `kind` is `token` for static tokens, `apiToken`
for [managed tokens](#managed-tokens), `user` for [OIDC users](#oidc-sign-in-and-bearer-jwts) (`anonymous` when
authentication is disabled). Users also get `groups`, the group names from the provider. `role` is kept for older clients: the highest built-in role whose permissions
the principal holds everywhere, `viewer` when it holds less.

`GET /__admin/spaces` lists the spaces the caller can see (every space for a principal with a global permission,
otherwise its bound spaces) with the caller's permissions in each:

```json
{"spaces": [{"name": "billing", "pathPrefixes": [], "hosts": [],
             "permissions": ["stubs:read", "stubs:write", "imports:run", "journal:read", "state:read"]}]}
```

## Errors

| Response | Meaning |
|---|---|
| `403 {"error":"forbidden","details":["requires role editor"],"required":"editor"}` | a principal holding only global built-in roles lacks the permission (as before) |
| `403 {"error":"forbidden","details":["requires permission files:read"],"required":"files:read"}` | any other principal lacks the permission |
| `403 {"error":"requires permission stubs:write in space billing","details":[],"required":"stubs:write"}` | the permission is missing in the selected space |
| `400 {"error":"several spaces are bound (billing, payments); set space",...}` | a write without `space` by a principal bound to several spaces |
| `404 {"error":"space nope not found",...}` | unknown space |
| `400 {"error":"space payments owns /payments/, /psp/: ...",...}` | a stub outside the space's ownership |
| `403 {"error":"requires permission ... to grant role ... there",...}` | a token request that would exceed the caller's permissions ([escalation rule](#managed-tokens)) |
| `401 {"error":"unauthorized","details":["token expired"]}` | an expired managed token |
| `401 {"error":"unauthorized","details":["session expired or unknown: sign in again"]}` | an expired, logged-out or unknown session cookie |
| `401 {"error":"unauthorized","details":["invalid bearer token"]}` | a JWT that fails validation |
| `403 {"error":"forbidden","details":["missing X-FastMock-Request: 1 header"]}` | a cookie-authenticated change without the CSRF header |
| `403 {"error":"forbidden","details":["user bob has no access: ..."]}` | an OIDC user without any binding |

Denials count in `fastmock_admin_auth_failures_total` with `reason="role"` for principals holding only global
built-in roles and `reason="permission"` otherwise; expired managed tokens count as `reason="expired"`, revoked
ones as `reason="token"`, rejected sessions as `reason="session"`, invalid JWTs as `reason="jwt"` and missing CSRF
headers as `reason="csrf"`. Audit events record the principal kind (`principalKind`) and
the space of the change (`space`).

## Examples

A shared instance for two teams and a platform group, with the CI token of each team limited to its space:

```yaml
roles:
  - name: ci
    permissions: [stubs:*, imports:run, journal:*, state:*, callbacks:*, files:*]
spaces:
  - name: payments
    pathPrefixes: [/payments/]
  - name: billing
    pathPrefixes: [/billing/]
    hosts: [billing.example.com]
bindings:
  - subject: token:ci-payments
    role: ci
    spaces: [payments]
  - subject: token:ci-billing
    role: ci
    spaces: [billing]
  - subject: token:dashboards
    role: viewer
    spaces: [payments, billing]
```

```sh
# FASTMOCK_ADMIN_TOKENS=ci-payments::<hash>,ci-billing::<hash>,dashboards::<hash>,ops:admin:<hash>
curl -s -X POST localhost:9090/__admin/stubs -H "Authorization: Bearer $CI_PAYMENTS_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"request": {"method": "GET", "urlPath": "/payments/health"}, "response": {"status": 200}}'
# the stub lands in space payments, the only space of the token
curl -s "localhost:9090/__admin/requests?space=payments" -H "Authorization: Bearer $DASHBOARDS_TOKEN"
```

With Helm, put the same document under the `access` value ([Helm: access control](helm.md#access-control)).

## Migrating an existing installation

1. Nothing is needed to upgrade: without an access file the behaviour is unchanged. The database migration only
   adds nullable `space` columns and indexes; existing rows stay in the global area.
2. Add spaces and bindings in the access file. Existing global tokens keep their global role, so they keep
   seeing and changing everything.
3. Give teams new tokens with an empty role and space bindings (`name::<hash>` plus `token:name` bindings),
   create [managed tokens](#managed-tokens) for them, or bind their [OIDC groups](#oidc-sign-in-and-bearer-jwts).
   The migration that adds the `fm_api_tokens` table is additive as well. Move their stubs into their space by exporting them
   (`GET /__admin/export?tag=...`), importing them with `?space=<space>` and deleting the global copies.
4. Clients that read `principal.role` from `/__admin/info` keep working; use `principal.permissions` for new
   decisions. `403` responses keep `error`, `details` and `required`.
