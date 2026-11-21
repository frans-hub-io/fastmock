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
- [Viewing the access configuration](#viewing-the-access-configuration)
- [In the web UI](#in-the-web-ui)
- [Errors](#errors)
- [Examples](#examples)
- [Upgrade notes](#upgrade-notes)
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
| `contracts:secrets` | `secretHeaders` in `POST /contract-tests` | global (the secrets of `FASTMOCK_CONTRACT_SECRETS` are server-wide); a custom role that lists it by name cannot be bound in spaces, see [Bindings](#bindings) |
| `info:read` | `GET /info`, `/version`, `/plugins`, `/spaces` | global, implied by any binding |
| `audit:read` | `GET /audit` | global |
| `access:read` | `GET /access` (the [access configuration](#viewing-the-access-configuration): spaces, roles and bindings) | global |
| `system:reset` | `POST /reset` | global |
| `tokens:read`, `tokens:write` | `GET /tokens`, `/tokens/{id}`; `POST /tokens`, `DELETE /tokens/{id}` ([managed tokens](#managed-tokens)) | space (the scopes of the token) |
| `mail:read` | captured mail: `GET /mail/messages*`, `/mail/stream`, `/mail/mailboxes`, `/mail/chaos`, `/mail/webhooks`; `POST /mail/messages/wait`, `/mail/verify` | space |
| `mail:write` | `POST /mail/messages` (inject), `PUT /mail/messages/{id}/labels`, mailbox passwords (together with `mail:read`), mail chaos rules and webhooks, `POST /mail/inbound` | space |
| `mail:delete` | `DELETE /mail/messages`, `/mail/messages/{id}` | space |

Mail permissions also apply outside the admin API ([Mail: spaces](../guide/mail.md#spaces)): a managed token with
`mail:read` is accepted as an IMAP or POP3 password for the mailboxes of its space (changing the mailbox needs
`mail:write`, expunging `mail:write` or `mail:delete` as well), and one with `mail:write` as an SMTP AUTH password
when `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH=true`, for recipients routed to spaces where it holds `mail:write`. Messages
of the global area need the permission globally.

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
| `viewer` | every `*:read` permission except `audit:read`, `tokens:read` and `access:read` |
| `editor` | `viewer` plus `stubs:write`, `imports:run`, `journal:delete`, `files:write`, `recordings:write`, `callbacks:delete`, `state:write`, `chaos:write`, `grpc:write`, `contracts:details`, `contracts:write`, `mail:write`, `mail:delete` |
| `admin` | every permission |

The built-in roles cannot be redefined.

## The access file

`FASTMOCK_ACCESS_FILE` names a YAML file with custom roles, spaces and bindings. It is read once at start-up,
like the token file; change it and restart (with Helm, `helm upgrade` rolls the pods). A file with an error stops
the server with a one-line message that names the file and the entry, for example
``access file /etc/fastmock/access/access.yaml: roles[0] (stub-author): unknown permission `stubs:wrte` `` or
`access file /etc/fastmock/access/access.yaml: bindings[3] (token:nosuch): no token named nosuch in FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE`
for a binding of an unknown token.

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
    mailDomains: [billing.example.test]
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

Spaces, custom roles and bindings are defined only in the access file: the file named by `FASTMOCK_ACCESS_FILE`,
the `access` values of the [Helm chart](helm.md#access-control), or the ConfigMap named by
`spec.access.configMapRef` with the [operator](operator.md#access-file). There is no API that creates or changes
them. To add a space, add it to the file and restart (with Helm and the operator the pods roll on their own). The
UI's [Access page](../guide/ui.md#access) and `GET /__admin/access` show the configuration read-only
([Viewing the access configuration](#viewing-the-access-configuration)).

## Custom roles

| Field | Meaning |
|---|---|
| `name` | `[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`, not `viewer`, `editor` or `admin`. |
| `permissions` | Permission names; `resource:*` grants every permission of a resource (`stubs:*`, `contracts:*`), and `"*"` (quoted) every permission. Unknown names are start-up errors. |
| `description` | Optional text. |

A custom role can be used in a binding and as the role of a token entry: `ci-billing:stub-author:<sha256>` in
`FASTMOCK_ADMIN_TOKENS` is a global binding of `stub-author`.

## Bindings

A binding gives a role to a subject, either everywhere (`global: true`), in a list of spaces (`spaces`) or in every
space (`spaces: ["*"]`, see [All spaces](#all-spaces)); exactly one of `global` and `spaces`.

| Subject | Matches |
|---|---|
| `token:<name>` | the static token entry with that name in `FASTMOCK_ADMIN_TOKENS` or `FASTMOCK_ADMIN_TOKENS_FILE` (a binding for an unknown token name is a start-up error) |
| `user:<username>` | an [OIDC user](#oidc-sign-in-and-bearer-jwts) by the value of the username claim (use a stable claim such as `sub`, see [choosing the username claim](#choosing-the-username-claim)) |
| `group:<group>` | an [OIDC user](#oidc-sign-in-and-bearer-jwts) who has this group in the groups claim |

A token entry with a role is a global binding of that role, as before. To give a token only space bindings,
leave its role empty: `ci-billing::<sha256>`. Such an entry needs at least one `token:ci-billing` binding.

A principal is allowed a permission in a space when one of its bindings grants it globally, or grants it in that
space and the permission is a space permission. A binding of `admin` in a space therefore grants every space
permission there but none of the global ones (`stubs:purge`, `system:reset`, `audit:read`, `contracts:secrets`
and so on).
`info:read` comes with any binding.

`contracts:secrets` is granted only by global bindings. A space binding of `admin`, or of a custom role that gets
the permission through a wildcard (`contracts:*`), is accepted and simply does not grant it; `GET /__admin/info`
shows the permissions the caller actually holds. A space binding of a custom role that lists `contracts:secrets`
by name stops the start:

```text
access file /etc/fastmock/access/access.yaml: bindings[1] (group:qa): role `secret-runner` lists contracts:secrets, a global permission that a binding limited to spaces cannot grant (contract secrets are server-wide); bind the role with global: true or remove contracts:secrets from it
```

Bind such a role with `global: true`, or remove `contracts:secrets` from it.

### All spaces

`spaces: ["*"]` binds the role in every space of the access file, including spaces added to the file later: once
the server restarts with a new space, the binding covers it without a change of its own. Quote the `*`; a bare
`*` is not valid YAML there.

```yaml
bindings:
  - subject: group:qa
    role: editor
    spaces: ["*"]
```

In each space the binding grants what a binding naming that space would grant, and nothing more:

- it does not grant global data: stubs, journal entries, files, scenario states and key-value entries of the
  global area, and requests that no stub matched, still need a global binding;
- it does not grant global permissions (`stubs:purge`, `system:reset`, `audit:read`, `access:read`,
  `contracts:secrets`, `recordings:*`, `chaos:*`, `grpc:*`, see [Permissions](#permissions));
- a principal with such a binding counts as bound to several spaces, so writes without `space` answer `400`
  ([Selecting a space](#selecting-a-space-in-the-api)), unless the file defines exactly one space.

`*` must be the only entry, and the file must define at least one space; otherwise the server does not start:

```text
access file /etc/fastmock/access/access.yaml: bindings[2] (group:qa): "*" must be the only entry in spaces
access file /etc/fastmock/access/access.yaml: bindings[2] (group:qa): spaces: ["*"] needs at least one space in the file
```

A custom role that lists `contracts:secrets` by name is refused with `spaces: ["*"]` exactly as in any other space
binding (see above).

### A developer who works in several spaces

There are three ways to give one person or team access to several spaces:

- **List the spaces** in the binding: `spaces: [payments, billing]`. The binding covers exactly these spaces; a
  new space needs a change of the file.
- **Use groups.** Bind each team's group in its space and add the person to the groups at the identity provider.
  Group changes take effect at the next sign-in, without a change of the file.
- **Bind all spaces** with `spaces: ["*"]`, for someone who works with every team, such as a QA lead or an
  engineer on support duty, and should keep working in spaces added later.

Use `global: true` instead when the person also needs global data (global stubs, unmatched requests, global files
and state) or global permissions such as recordings, chaos rules, gRPC descriptors, the audit log or the access
configuration. Bindings combine: a global `viewer` binding together with an `editor` binding in `["*"]` lets a
person read everything and change the stubs and test data of every space, but nothing in the global area.

```yaml
spaces:
  - name: payments
  - name: billing
  - name: orders
bindings:
  - subject: group:payments-dev
    role: editor
    spaces: [payments, billing]
  - subject: group:qa
    role: editor
    spaces: ["*"]
  - subject: group:qa
    role: viewer
    global: true
  - subject: group:platform
    role: admin
    global: true
```

## Managed tokens

Besides the static entries of `FASTMOCK_ADMIN_TOKENS`, tokens can be created and revoked through the admin API
without a restart. A managed token has a name, one role (built-in or custom) and a scope: global or a list of
spaces. Its expiry is mandatory. Managed tokens need authentication: while `FASTMOCK_AUTH_DISABLED=true`, they
cannot be created or revoked, and `POST /__admin/tokens` answers
`409 {"error":"managed tokens cannot be created while authentication is disabled (FASTMOCK_AUTH_DISABLED=true)","details":[]}`
(`revoked` for `DELETE /__admin/tokens/{id}`).

```sh
curl -s -X POST localhost:9090/__admin/tokens -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name": "ci-billing-nightly", "role": "editor", "spaces": ["billing"], "expiresInDays": 30, "description": "CI of the billing team"}'
```

```json
{"token": "fm_3f9c...", "id": "019a8f1e-...", "name": "ci-billing-nightly", "role": "editor", "spaces": ["billing"],
 "global": false, "description": "CI of the billing team", "createdBy": "ops", "createdAt": "2026-11-20T09:00:00Z",
 "expiresAt": "2026-12-20T09:00:00Z", "lastUsedAt": null, "revokedAt": null, "status": "active"}
```

The value (`fm_` and 64 hex digits, the same format as `fastmock hash-token --generate`) is returned only in this
response; FastMock stores its SHA-256 hash. Store it in your secret manager right away.

| Field | Meaning |
|---|---|
| `name` | `[A-Za-z0-9._@-]`, at most 128 characters. Unique among active tokens and different from every static token name (`409` otherwise); a name becomes free again when its token is revoked or expired. It appears as the principal name in `/info`, logs and the audit log. |
| `role` | `viewer`, `editor`, `admin` or a custom role of the access file. |
| `spaces` or `global: true` | Exactly one of them. A token in spaces gets the role's space permissions in each of them (like a space binding). `spaces: ["*"]` gives them in every space of the access file, including spaces added later, like a binding in [all spaces](#all-spaces); `*` must be the only entry (`400` with `"*" must be the only entry in spaces` otherwise), and the access file must define at least one space (`400` with `spaces: ["*"] needs at least one space in the access file` otherwise). The token keeps `["*"]` as stored, and the spaces are resolved on every request. |
| `expiresInDays` | 1 to `FASTMOCK_TOKENS_MAX_DAYS` (default 365). When omitted: 90 days, or `FASTMOCK_TOKENS_MAX_DAYS` when lower. A token created by a managed token never outlives it (see below). |
| `description` | Optional, at most 1024 bytes. |

| Endpoint | Permission |
|---|---|
| `POST /tokens` | the escalation rule below |
| `GET /tokens` | `tokens:read`; lists the tokens for which the caller holds `tokens:read` in every scope (global tokens need it globally). Filters: `?space=` (a token in all spaces belongs to every space), `?status=active\|expired\|revoked`. |
| `GET /tokens/{id}` | `tokens:read` in every scope of the token; others answer 404 |
| `DELETE /tokens/{id}` | `tokens:write` in every scope of the token, or the token itself (self-revocation) |
| `GET /tokens/self` | the managed token itself (any static token gets 404) |
| `GET /tokens/options` | `tokens:write` somewhere; the roles with the scopes where the caller may grant them under the escalation rule (`global`, `spaces`, and `allSpaces` when the caller may create tokens in all spaces with the role), and `maxDays`/`defaultDays` (used by the UI's create form) |

**Escalation rule.** The caller can create a token only if, in each requested scope, it holds `tokens:write` and
every permission the role grants there: for a global token, globally; for a token in space `s`, the space
permissions of the role in `s`; for a token in all spaces (`spaces: ["*"]`), each of these either globally or
through the caller's own binding or token in all spaces. Being bound to every current space one by one is not
enough, because a token in all spaces also covers spaces added later. A token therefore never holds more than its
creator, and a caller bound only to spaces cannot create a global token. A denial names the first missing
permission:
`403 {"error":"forbidden","details":["requires permission files:write in space billing to grant role editor there"],"required":"files:write"}`.
The built-in `editor` role lacks `tokens:write`; to let a team manage its own tokens, bind a custom role with
`tokens:read` and `tokens:write` in its space:

```yaml
roles:
  - name: team-lead
    permissions: [stubs:*, imports:run, journal:*, files:*, callbacks:*, state:*, contracts:read,
      contracts:details, contracts:write, mail:*, tokens:read, tokens:write]
bindings:
  - subject: token:billing-lead
    role: team-lead
    spaces: [billing]
```

**Lifecycle.**

- *Use*: static tokens are checked first, in constant time; a value in the managed format is then looked up by
  its hash. Each replica caches the result of a lookup (including "unknown") for at most 10 seconds. While the
  store holds no managed token at all, a replica remembers that for up to 10 seconds and answers unknown values
  without a lookup; so the first token created on one replica may be rejected (`401`) on the others for up to
  10 seconds. Later tokens are found at once.
- *Revocation* (`DELETE /tokens/{id}`) takes effect at once on the replica that handled it and within 10 seconds
  on every other replica of the same storage. Revoking again answers `200` and changes nothing.
- *Expiry* is checked on every request against the stored time, so an expired token stops working at once
  everywhere (`401 {"error":"unauthorized","details":["token expired"]}`).
- `lastUsedAt` is written in the background at most once per minute per token; it never delays a request.
- A token created by another managed token cannot outlive its creator: its `expiresAt` is capped at the
  creator's, and the metadata of the new token carries the creator's id as `parentId`. Revoking a token also
  revokes the tokens it created, and a managed token whose own creator token is no longer active cannot create
  tokens (`401`).
- Tokens created by an SSO user keep working after the user loses its groups at the provider or leaves. The token
  list shows the creator (`createdBy`); revoke a person's tokens as part of off-boarding.
- Revoked and expired tokens stay listed with `status` `revoked` or `expired` for 90 days, then they are deleted.
- If the role of a token disappears from the access file, the token is rejected until the role is back.
- A token in all spaces grants nothing in spaces while the access file defines none; requests are not refused for
  that reason, and the token covers the spaces as soon as the file defines them.
- A token in all spaces may create tokens in all spaces or in any list of spaces, within the escalation rule; the
  expiry cap and the revocation of the tokens it created apply as for any managed token.
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

### Choosing the username claim

`user:` bindings match the value of `FASTMOCK_OIDC_USERNAME_CLAIM`. Bind users by a claim that is unique per
user, never reassigned and not editable by the user: `sub`, or `oid` with Microsoft Entra ID. Display names,
`preferred_username` and `email` can change or be reused at many providers, so use them only where your provider
guarantees otherwise. When the username claim is `email`, sign-in requires `email_verified: true` in the token.
The claim value is also the principal name in `/__admin/info`, logs and the audit log. Binding by
[group](#bindings) avoids per-user entries altogether.

### Sessions

| Property | Value |
|---|---|
| Cookie | `__Host-fastmock_session`, `Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`, `Max-Age` = `FASTMOCK_OIDC_SESSION_TTL` |
| Value | 256 random bits (43 base64url characters) |
| Server side | in the hot store under the SHA-256 of the value: user name, group names, creation and last-seen time, and the ID token of the sign-in (for the logout hint) |
| Lifetime | absolute `FASTMOCK_OIDC_SESSION_TTL` (default `8h`) and idle `FASTMOCK_OIDC_SESSION_IDLE` (default `1h`); the groups captured at sign-in are kept until one of them expires |
| Replicas | every replica and every cluster sharing the Valkey sees the same sessions; in memory mode they live in the process |

The last-seen time is written back at most once a minute (and at least four times per idle period), so the idle
timeout can be exceeded by up to a quarter of it. The bindings are computed from the access file on every request,
so a changed access file applies to existing sessions after the restart. The groups are captured at sign-in and
kept until the idle or absolute expiry of the session; set `FASTMOCK_OIDC_SESSION_IDLE` and
`FASTMOCK_OIDC_SESSION_TTL` to how quickly group changes at the provider must take effect.

An expired or unknown session answers `401 {"error":"unauthorized","details":["session expired or unknown: sign in again"]}`
and clears the cookie.

`POST /__admin/auth/logout` deletes the session and clears the cookie. When the provider publishes an
`end_session_endpoint`, the response carries `logoutUrl` so the UI can end the provider session too. The URL has
`client_id`, `post_logout_redirect_uri` set to the admin URL's `/ui/` (which shows the sign-in page once the
session is gone) and `id_token_hint` with the ID token of the sign-in, so the provider ends its session without
asking the user to confirm:

```json
{"loggedOut": true, "logoutUrl": "https://sso.example.com/realms/mocks/protocol/openid-connect/logout?client_id=fastmock&post_logout_redirect_uri=https%3A%2F%2Fmock-admin.example.com%2Fui%2F&id_token_hint=eyJhbGciOi..."}
```

The ID token is kept only in the session record in the hot store and leaves the server only in this URL, which
goes to the browser that signed in. It is never put in a cookie or logged. ID tokens up to 64 KiB are accepted at
sign-in, so users with many groups can sign in; ID tokens larger than 16 KiB are not kept in the session, and the
logout then goes without the hint and the provider may ask for confirmation. Sessions created by an earlier
version have no ID token either.

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

With `FASTMOCK_OIDC_API_AUDIENCE` set, a bearer value that is not a static token and has three dot-separated
parts (whatever their content and length) is treated as a JWT and validated like the ID token, but against that audience, without `nonce` and without `azp` (the token may have been issued to
another client, for example a CI pipeline using client credentials or a CLI using the device flow). `iat` is
optional. The username claim falls back to `sub` when absent; the groups claim is read from the token (userinfo is
not called). Tokens that carry a `nonce` claim (ID tokens) are rejected, and so are tokens larger than 64 KiB. The
audience must differ from the client id: `FASTMOCK_OIDC_API_AUDIENCE` set to an empty value, or to the value of
`FASTMOCK_OIDC_CLIENT_ID`, stops the start. An empty value stops it with
`FASTMOCK_OIDC_API_AUDIENCE is set but empty: set the audience of the API tokens, or remove the variable to turn bearer JWTs off`.
When the variable is not set at all, bearer JWTs are off.

Precedence: a request with an `Authorization` header is decided by that header alone, in this order: static
tokens, managed `fm_` tokens, JWTs. The session cookie is used only when there is no `Authorization` header. A
JWT for a user without bindings answers `403` with
`"details":["user alice has no access: the access file binds neither user:alice nor any of the user's groups"]`.
An invalid JWT answers `401 {"error":"unauthorized","details":["invalid bearer token"]}` with
`WWW-Authenticate: Bearer error="invalid_token"`.

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `FASTMOCK_OIDC_ISSUER` | none | Issuer URL; OIDC is off without it. https (plain http only for `localhost`, `127.0.0.1` and `::1`, for tests). Must equal the `issuer` of the discovery document exactly. |
| `FASTMOCK_OIDC_CLIENT_ID` | none | Client id (required). |
| `FASTMOCK_OIDC_CLIENT_SECRET` / `_SECRET_FILE` | none | Client secret, one of the two (required). |
| `FASTMOCK_OIDC_REDIRECT_URL` | none | Public admin URL followed by `/__admin/auth/callback` (required, https except on localhost). Register it at the provider. |
| `FASTMOCK_OIDC_SCOPES` | `openid profile email` | Space- or comma-separated; must include `openid`. |
| `FASTMOCK_OIDC_USERNAME_CLAIM` | `preferred_username` | Claim (or dotted path) with the user name for `user:` bindings; `sub` (or `oid` with Entra ID) is recommended ([choosing the username claim](#choosing-the-username-claim)). With `email`, sign-in requires `email_verified: true`. |
| `FASTMOCK_OIDC_GROUPS_CLAIM` | `groups` | Claim (or dotted path such as `realm_access.roles`) with the groups for `group:` bindings: an array of strings or one string. |
| `FASTMOCK_OIDC_API_AUDIENCE` | none | Enables bearer JWTs on the admin API for this audience; unset, bearer JWTs are off. Set but empty, or equal to the client id, stops the start. |
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
allowed hosts, redirects only to those hosts (at most three; the call to the token endpoint follows no redirects,
so the discovered `token_endpoint` must answer directly), 5 seconds to connect, 10 seconds per request,
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
   confidential client with *Service accounts* on (client credentials). Add its service account user to a group
   (for example `ci-payments`) and bind `group:ci-payments`; with `FASTMOCK_OIDC_USERNAME_CLAIM=sub` you can
   instead bind `user:<id of the service account user>`.
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
   `FASTMOCK_OIDC_CLIENT_ID=<application id>`, `FASTMOCK_OIDC_USERNAME_CLAIM=oid` (the user's object id, which
   never changes or moves to another user), so `user:` bindings look like
   `user:3d0f6c1e-0000-4000-8000-000000000002`. Microsoft advises against authorizing by `preferred_username`
   (the UPN) or `email`; prefer group bindings for teams.
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
5. The `token_endpoint` of the discovery document must answer the code exchange itself: FastMock follows no
   redirects on that call. Behind a proxy or load balancer that redirects, publish the final URL in discovery.

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
  - subject: group:ci-payments
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
reason, `auth.logout`); changes made by a user record `principalKind: user`. Codes, tokens (the ID token
included), session ids, the client secret and the PKCE verifier are never logged.

## Spaces

A space is a named partition, named like a Kubernetes namespace (`[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`).

| Field | Meaning |
|---|---|
| `name` | The space name. |
| `description` | Optional text, shown by `GET /__admin/spaces`. |
| `pathPrefixes` | Optional data-plane path prefixes that the space's stubs must stay under ([ownership](#ownership-of-the-data-plane)). Each must be a plain path starting with `/`: no query, fragment, `.` or `..` segment, encoded slash or backslash (`%2F`, `%5C`) or backslash. |
| `hosts` | Optional `Host` values the space's stubs must match. |
| `mailDomains` | Optional mail domains whose captured messages go to the space (lowercase; `*.example.com` covers the subdomains of `example.com`, not `example.com` itself). A domain belongs to one space: overlaps between spaces stop the start, like overlapping path prefixes. The IMAP and POP3 mailboxes of these domains live in the space. See [Mail: spaces](../guide/mail.md#spaces). |

What belongs to a space:

- **Stubs** carry a `space` field. It is set on create and never changes: `PUT` and `PATCH` keep the stored
  space and reject a different one (move a stub by deleting and creating it, both authorised).
- **Journal entries** get the space of the stub that matched. Requests that no stub matched, and requests
  matched by global stubs, belong to no space and are visible only with global `journal:read`.
- **Callback jobs** get the space of the stub that scheduled them. Callbacks scheduled by a plugin get the space
  of the stub being answered or, on a plugin route, the space that owns the request's path or host (when exactly
  one space does).
- **Scenario states, key-value entries and body files** of a space are stored apart from global data, in a key
  namespace of their own. Two spaces can use the same scenario name, key or file name independently, and a global
  name such as `payments/x` is always global data, even when a space `payments` exists. Entries written by a
  principal bound to a space go to its space; global principals see and write global data as before, so
  existing state and files need no migration.
- **Data used by a stub** comes from the stub's space: its scenario states and key-value entries are those of its
  space, and `bodyFileName` is resolved among the files of its space first, then among the global files. A stub
  never resolves a file, scenario or key of another space, also when the name is built by a template. Global
  body files are therefore shared with every space: keep files that belong to one team in that team's space, and
  keep only files meant for everyone in the global area.
- **Contract test runs** carry the space they were started in.
- **Captured mail** goes to the space that claims the recipients' domains in `mailDomains`, else to the space named
  like the SMTP AUTH user, else to the global area; mail chaos rules, provider webhooks and mailbox passwords are
  set per space ([Mail](../guide/mail.md#spaces)).

Messages published to Kafka, gRPC descriptors, chaos rules, global settings, recordings, async mocks and the
service catalog are not scoped. On the data plane every enabled stub competes for a request, except inside the
paths and hosts that a space owns ([ownership](#ownership-of-the-data-plane)).

A request naming a space that the access file does not define, or a space the caller cannot see (it holds no
space permission globally, `info:read` aside, and has no binding there), answers 404.

## Selecting a space in the API

Collection endpoints take a `space` query parameter; stubs also take a `space` field.

| Caller | Reads without `space` | Writes without `space` |
|---|---|---|
| holds the permission globally | everything (global area and every space) | the global area |
| bound only to spaces | the union of the spaces where it holds the permission | its space when it has exactly one; otherwise 400 |

With `space`, reads and writes apply to that space, and the caller must hold the permission there: a space the
caller cannot see answers 404 like an unknown one, a visible space without the permission 403. Deletions that select existing items (`DELETE /requests`, `DELETE /callbacks`, `DELETE /scenarios`,
`DELETE /stubs?tag=`, `POST /stubs/remove-by-metadata`, `POST /requests/remove`, `DELETE /stubs/counters`) select
like reads: without `space` they act on everything the caller may change. Single items (`/stubs/{id}`,
`/requests/{id}`, `/contract-tests/{id}`) are found by id; one in a space the caller cannot read answers 404.
`/kv/{key}`, `/scenarios/{name}` and `/files/{name}` address one entry and resolve the space like writes. Stub ids
are unique across all spaces: `POST /stubs` or an import with an id that belongs to a stub in another space or in
the global area answers `409` with a generic conflict message.

| Endpoint | How the space is given |
|---|---|
| `POST /stubs` | `space` field of the stub or `space` query parameter (they must agree) |
| `POST /stubs/import`, `POST /import`, `/import/openapi`, `/import/wiremock`, `/import/postman`, `/import/har`, `/import/mockserver` | `space` query parameter; every imported stub goes to that space, and `replaceTag`/`replace` removes only that space's stubs with the tag. For a space that owns `hosts`, the `host` query parameter sets `request.host` on every imported stub; it defaults to the space's only host and is required when the space owns several |
| other collection endpoints | `space` query parameter |

Imports into a space are not recorded in the [service catalog](../guide/service-catalog.md), which is global.

## Ownership of the data plane

A space with `pathPrefixes` or `hosts` owns that part of the data plane.

**Stubs of the space stay inside it.** Every stub in the space must match only inside its prefixes and hosts,
checked on create, update and import (`400` with the reason):

- with `pathPrefixes`, the stub's URL matcher must be `url`, `urlPath` or `urlPathTemplate` lying under one of
  the prefixes (`/payments/` and `/payments` both allow `/payments/x`, neither allows `/paymentsx`). Regular
  expression matchers (`urlPattern`, `urlPathPattern`) and stubs without a URL matcher are rejected, and so are
  paths with a `.` or `..` segment, plain or percent-encoded (`/payments/../billing/x`, `/payments/%2E%2E/x`),
  and paths with an encoded slash or backslash in any case or a raw backslash (`/payments/..%2Fbilling/x`,
  `/payments/a%5Cb`, `/payments\x`), which a proxy or server that decodes them would read as other segments;
- with `hosts`, the stub must have a `request.host` matcher with `equalTo` one of the hosts and nothing else.

A space with both `pathPrefixes` and `hosts` needs both: the stub's path must lie under one of the prefixes **and**
its `request.host` must be one of the hosts. Importers set `request.host` from the `host` query parameter
([selecting a space](#selecting-a-space-in-the-api)).

The rules apply when a stub is written. Stubs stored before the access file gave the space prefixes or hosts, or
before it dropped the space, stay stored and keep their space. Each replica logs a warning at start-up for every
such stub (`stored stub breaks the rules of its space`, with `stub_id`, `space` and `reason`), and
`GET /__admin/info` lists them in `warnings` to principals that can read stubs in that space. Fix or delete them.

**Owned prefixes and hosts do not overlap.** Two spaces may not own overlapping path prefixes (such as
`/payments/` and `/payments/refunds/`) or the same host; a host without a port overlaps the same host with any
port. An access file where they do stops the server at start-up, and the error names both spaces and the
conflicting values:
``space payments and space refunds overlap: path prefix `/payments/` of space payments and `/payments/refunds/` of space refunds; each path belongs to one space``.

**Requests inside a space go to its stubs.** A data-plane request whose path lies under a prefix owned by a space,
or whose `Host` is a host owned by a space, is answered only by stubs of that space or by global stubs; stubs of
other spaces are skipped for it. Requests outside every owned prefix and host are matched against all enabled
stubs as usual.

The data plane does not normalise request paths: the owning space is decided on the path as received, the same
path the stubs are matched against. A request is therefore answered only by stubs of the space that owns its
path as sent. Clients and proxies normally remove dot segments before sending; a request that keeps them, such as
`/payments/../billing/x` sent with `curl --path-as-is`, belongs to `/payments/` and never reaches stubs of the
space that owns `/billing/`.

Stubs in the global area are not restricted; only principals with global `stubs:write` can create them. When
several teams share an instance, give each team's space the `pathPrefixes` or `hosts` of the traffic it mocks, so
that requests there are answered only by that team's stubs and global ones.

## What is scoped and what is global

| Data | Scoped by space |
|---|---|
| stubs, journal entries, callback jobs, scenario states, key-value entries, body files, contract test runs, stub counters | yes |
| captured mail, IMAP/POP3 mailboxes and their passwords, mail chaos rules, provider webhook settings | yes |
| recordings, chaos rules, global settings, gRPC descriptors, async mocks, service catalog, Kafka messages, audit log | no (global permissions) |

## Inspecting your permissions

`GET /__admin/info` returns the caller's principal:

```json
{"principal": {"name": "ci-billing", "kind": "token", "role": "viewer",
  "permissions": {"global": ["info:read"],
                  "spaces": {"billing": ["stubs:read", "stubs:write", "imports:run", "journal:read", "state:read"]},
                  "allSpaces": []}}}
```

`permissions.global` holds what is granted everywhere; `permissions.spaces` what is granted only in single spaces
(the effective permissions in a space are both lists together). `permissions.allSpaces` lists the permissions
granted through a binding or token in [all spaces](#all-spaces) (empty when there is none); these permissions also
appear under every space of `permissions.spaces`, so clients that read only `spaces` keep working. `kind` is `token` for static tokens, `apiToken`
for [managed tokens](#managed-tokens), `user` for [OIDC users](#oidc-sign-in-and-bearer-jwts) (`anonymous` when
authentication is disabled). Users also get `groups`, the group names from the provider. `role` is kept for older clients: the highest built-in role whose permissions
the principal holds everywhere, `viewer` when it holds less. `stubsLoaded` in the same response counts only the
stubs the caller can see.

`GET /__admin/spaces` lists the spaces the caller can see (every space for a principal that holds a space
permission globally or is bound in all spaces; `info:read`, which every principal has, does not count; otherwise its bound spaces) with the caller's permissions in each:

```json
{"spaces": [{"name": "billing", "pathPrefixes": [], "hosts": [],
             "permissions": ["stubs:read", "stubs:write", "imports:run", "journal:read", "state:read"]}]}
```

## Viewing the access configuration

`GET /__admin/access` returns the configuration that the server loaded: spaces, roles and bindings, including the
global roles of static tokens. It needs the
global permission `access:read`, which the built-in `admin` role has; a custom role gets it by listing
`access:read`, `access:*` or `"*"` and being bound with `global: true`. Without an access file and OIDC it needs `admin`,
like every admin-only route.

```json
{"configured": true, "oidc": true,
 "spaces": [{"name": "billing", "pathPrefixes": ["/billing/"], "hosts": ["billing.example.com"]},
            {"name": "payments", "pathPrefixes": ["/payments/"], "hosts": []}],
 "roles": [{"name": "viewer", "builtin": true, "permissions": ["stubs:read", "journal:read", "..."]},
           {"name": "editor", "builtin": true, "permissions": ["..."]},
           {"name": "admin", "builtin": true, "permissions": ["..."]},
           {"name": "ci", "builtin": false, "permissions": ["stubs:read", "stubs:write", "..."]}],
 "bindings": [{"subject": "token:ci-payments", "role": "ci", "global": false, "spaces": ["payments"], "source": "accessFile"},
              {"subject": "group:qa", "role": "editor", "global": false, "spaces": ["*"], "source": "accessFile"},
              {"subject": "group:platform", "role": "admin", "global": true, "spaces": [], "source": "accessFile"},
              {"subject": "token:ops", "role": "admin", "global": true, "spaces": [], "source": "adminTokens"}]}
```

| Field | Meaning |
|---|---|
| `configured` | an access file is loaded |
| `oidc` | OIDC sign-in or bearer JWTs are enabled |
| `spaces` | the spaces of the access file, sorted by name, with `name`, `pathPrefixes` and `hosts` |
| `roles` | the built-in roles first (`viewer`, `editor`, `admin`), then the custom roles sorted by name; `permissions` lists the permission names with wildcards expanded |
| `bindings` | the bindings of the access file in file order, then one global binding for each static token written `name:role:hash` in `FASTMOCK_ADMIN_TOKENS` or `FASTMOCK_ADMIN_TOKENS_FILE`, in entry order: `subject` (as written; `token:<name>` for a static token), `role`, `global`, `spaces` (`["*"]` for [all spaces](#all-spaces), empty for a global binding) and `source` |
| `bindings[].source` | `accessFile` for a binding of the access file, `adminTokens` for the role of a static token. A static token written `name::hash` (no role) has only its access-file bindings and adds no entry |

The response contains no token values or hashes, no file paths and no OIDC client settings. Without an access
file, `configured` is `false`, `spaces` is empty, `roles` holds the built-in roles and `bindings` holds only the
roles of static tokens (`source: adminTokens`). The response is read-only: spaces, roles and bindings change only
in the [access file](#the-access-file) and the static token list, after a restart.

## In the web UI

The [web UI](../guide/ui.md#signing-in-permissions-and-signing-out) follows the same model; it has no privileges
of its own and the server checks every call.

- **Sign-in.** With OIDC configured the sign-in page offers **Sign in with SSO** next to the token form. The UI
  then uses the session cookie and sends `X-FastMock-Request: 1` on every admin call; the account menu shows the
  user name, kind (`token`, `managed token` or `user`), groups and bound spaces, and **Sign out** ends the session
  (and the provider session when it supports RP-initiated logout). The messages for each `loginError` and for an
  expired session appear on the sign-in page.
- **Space selector.** When the access file defines spaces, the header has a selector with the spaces the principal
  can see, plus **All spaces** for principals with a global permission or several spaces. Lists (stubs, journal,
  scenarios, callbacks, files, contract tests, tokens) send the selected space as `?space=` and show a Space
  column; create and import forms preselect it and offer only the areas where the principal holds the needed
  permission. The choice is remembered in the browser. Without spaces the selector and the column are hidden.
- **Actions** follow `principal.permissions` from `GET /__admin/info`: controls the principal cannot use are hidden
  or disabled, and a disabled control's tooltip names the missing permission (for example "Requires the
  stubs:write permission in space billing").
- **Admin tokens** (visible with `tokens:read` when the server lists `tokens` in the `features` of
  `GET /__admin/version`) lists managed tokens with role, scope, creator, creation, expiry,
  last use and status, creates tokens limited to the roles and spaces of `GET /__admin/tokens/options` and to
  `FASTMOCK_TOKENS_MAX_DAYS`, shows the new value once with a copy button, and revokes after a confirmation.
  **All spaces** is offered as a scope where `allSpaces` of the chosen role is `true`, and tokens in all spaces
  show **All spaces** in the list.
- **Access** shows every signed-in user **Your access**: the global permissions and the effective permissions in
  each space (**All spaces** for permissions granted in all spaces). With `access:read` the page also shows the
  spaces, roles and bindings of `GET /__admin/access` as read-only tables. Changes are made in the access file.

## Errors

With an access file or OIDC configured, every `403` of the admin API has the same shape,
`{"error":"forbidden","details":[...],"required":...}`: `details` says what is missing and `required` names the
missing role or permission where there is one. With static tokens only, `403` bodies are those of earlier versions.

| Response | Meaning |
|---|---|
| `403 {"error":"forbidden","details":["requires role editor"],"required":"editor"}` | a principal holding only global built-in roles lacks the permission (as before) |
| `403 {"error":"forbidden","details":["requires permission files:read"],"required":"files:read"}` | any other principal lacks the permission |
| `403 {"error":"forbidden","details":["requires permission stubs:write in space billing"],"required":"stubs:write"}` | the permission is missing in the selected space |
| `400 {"error":"several spaces are bound (billing, payments); set space",...}` | a write without `space` by a principal bound to several spaces |
| `404 {"error":"space nope not found",...}` | unknown space, or a space the caller cannot see |
| `409` with a generic conflict message | a stub id that belongs to a stub in another area |
| `400 {"error":"space payments owns /payments/, /psp/: ...",...}` | a stub outside the space's ownership |
| `403 {"error":"forbidden","details":["requires permission ... to grant role ... there"],"required":...}` | a token request that would exceed the caller's permissions ([escalation rule](#managed-tokens)) |
| `401 {"error":"unauthorized","details":["token expired"]}` | an expired managed token |
| `401 {"error":"unauthorized","details":["session expired or unknown: sign in again"]}` | an expired, logged-out or unknown session cookie |
| `401 {"error":"unauthorized","details":["invalid bearer token"]}` | a JWT that fails validation |
| `403 {"error":"forbidden","details":["missing X-FastMock-Request: 1 header"]}` | a cookie-authenticated change without the CSRF header |
| `403 {"error":"forbidden","details":["user bob has no access: ..."]}` | an OIDC user without any binding |

Denials count in `fastmock_admin_auth_failures_total` with `reason="role"` for principals holding only global
built-in roles and `reason="permission"` otherwise (including OIDC users without any binding); missing, unknown
and revoked tokens count as `reason="token"`, expired managed tokens as `reason="expired"`, rejected sessions as
`reason="session"`, invalid JWTs as `reason="jwt"`, missing CSRF headers as `reason="csrf"` and refused browser
origins as `reason="origin"`. Audit events record the principal kind (`principalKind`) and the space of the
change (`space`).

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

## Upgrade notes

These notes apply when upgrading from a release without access control. Without an access file and without OIDC,
every endpoint keeps its role, response bodies (including every `403`) are unchanged, and existing stubs, journal
entries, state and files stay in the global area. Check the points below before rolling out.

**403 bodies.** Once an access file or OIDC is configured, every `403` of the admin API reads
`{"error":"forbidden","details":[...],"required":...}`, also on routes whose `403` had another `error` text before
([Errors](#errors)). Clients and scripts that match the `error` text of a `403` should read `details` and `required`
instead.

**Database.** The migrations are additive:

| Change | Tables |
|---|---|
| new nullable `space` column | `fm_stubs`, `fm_journal`, `fm_outbox` |
| new nullable `principal_kind` and `space` columns | `fm_audit` |
| new table for [managed tokens](#managed-tokens) | `fm_api_tokens` |

The migration that adds the `space` columns only adds them, in a short transaction. The three indexes on these
columns are built by separate migrations, one index each, with `CREATE INDEX CONCURRENTLY IF NOT EXISTS` outside a
transaction, so journal, callback and stub writes of running replicas are not blocked while a large journal is
indexed. Allow for the index build time in `FASTMOCK_DATABASE_MIGRATION_TIMEOUT` and the startup probe, or
[run migrations separately](database.md#running-migrations-separately). If you run migrations separately and grant the server user rights per table,
grant them on the new table too, for example
`GRANT SELECT, INSERT, UPDATE, DELETE ON fm_api_tokens TO fastmock;`. Without it the server starts, but managed
tokens cannot be created or used (a managed token answers `503 {"error":"token store unavailable",...}`).

**Mail permissions.** The release that adds [mail](../guide/mail.md) adds `mail:read`, `mail:write` and
`mail:delete`. `viewer` gains `mail:read`, `editor` gains all three. Because a token can only be created by a
caller that holds every permission of its role, a custom role that copies the permission list of `editor` (for
example to add `tokens:write`) can no longer create `editor` tokens until it also lists `mail:*` (or the three
mail permissions). Add them to such roles before upgrading:

```yaml
roles:
  - name: team-lead
    permissions: [stubs:*, imports:run, journal:*, files:*, callbacks:*, state:*, contracts:read,
      contracts:details, contracts:write, mail:*, tokens:read, tokens:write]
```

Mail stays off until `FASTMOCK_MAIL_ENABLED=true`; while it is off the mail routes answer `404`. With PostgreSQL
storage, mail adds four tables and a sequence ([Database](database.md#what-is-stored-where)) and needs Valkey
publish/subscribe: a Valkey ACL user needs the channel rule `&<prefix>mail:events:*` (for example
`&fastmock:mail:events:*`) and the commands `publish` and `psubscribe`
([Security: storage connections](security.md#storage-connections)). If you grant table rights one by one, grant
them on `fm_mail_messages`, `fm_mail_mailboxes`, `fm_mail_folders` and `fm_mail_entries`, and `USAGE` on the
sequence `fm_mail_uid_validity`.

**Logs.** Audit log lines (target `audit`) change:

- `role=Admin` becomes `role=admin` (lowercase, the role names of the principal's bindings);
- the lines carry a new `kind` field (`token`, `apiToken`, `user`, `anonymous`) and, for changes inside a space,
  `space`;
- the rejection line `admin request rejected: role too low` now reads `admin request rejected: permission missing`
  and carries the missing `permission`.

Update log-based alerts and dashboards that match these strings.

**Metrics.** Denials of admin-only routes (`GET /audit`, `POST /reset`, `DELETE /stubs` without `tag`) by a
non-admin are now counted in `fastmock_admin_auth_failures_total{reason="role"}` and write an audit warning line;
before, only the response was sent. `fastmock_admin_auth_failures_total` has new `reason` values, and
`fastmock_store_errors_total` new `store` values, see [Observability](observability.md#admin-api-and-contract-tests). New metrics:
`fastmock_api_tokens_active`, `fastmock_oidc_logins_total`, `fastmock_oidc_ready`.

**New routes.** `GET /__admin/auth/config`, `GET /__admin/auth/login`, `GET /__admin/auth/callback` and
`POST /__admin/auth/logout` are reached without an admin token (the sign-in flow needs them). If a WAF, a Gateway
or a route allow-list sits in front of the admin port, allow them when you use OIDC sign-in; without OIDC they
answer `404`, except `/auth/config`. The token endpoints `/__admin/tokens*`, `GET /__admin/spaces` and
`GET /__admin/access` need authentication like every other admin route; `GET /__admin/access` needs the global
permission `access:read` (`admin`). Custom roles bound with `global: true` that should see the
[access configuration](#viewing-the-access-configuration) need `access:read` added.

**Unlisted admin paths.** With an access file or OIDC configured, an admin route without an explicit permission,
including an unknown path under `/__admin`, needs `admin`: a non-admin gets `403` there where it got `404` before.
Without them nothing changes: an unknown path answers `404` to a caller whose role allows the method. Plugin routes under
`/__admin/plugins/<name>/` keep `viewer` for `GET` and `editor` for other methods.

**Rolling upgrade and rollback.** Upgrade every replica before you add an access file, create spaces or let
teams create stubs in spaces; while old and new versions run side by side, the old ones do not know spaces. A
previous version that starts against the new schema ignores the new columns and tables. Replicas of earlier
versions that are already running when the first new pod adds the columns may log
`cached plan must not change result type` and fail to claim callbacks or read the journal until they are
replaced, usually for a few seconds; due callbacks wait for a new replica and are then sent. From this version on
the queries name their columns, so later additive migrations do not cause this. After a rollback, stubs
in spaces behave as global stubs, the files, scenario states and key-value entries of spaces are not visible,
and managed tokens and SSO sign-in are not available. Before rolling back, remove the access file and the OIDC
settings, and give every entry of `FASTMOCK_ADMIN_TOKENS` / `FASTMOCK_ADMIN_TOKENS_FILE` a built-in role again: the
previous version refuses to start with an empty or custom role in a token entry.

## Migrating an existing installation

1. Upgrade as described in the [upgrade notes](#upgrade-notes): without an access file the behaviour is
   unchanged, and existing rows stay in the global area.
2. Add spaces and bindings in the access file. Existing global tokens keep their global role, so they keep
   seeing and changing everything.
3. Give teams new tokens with an empty role and space bindings (`name::<hash>` plus `token:name` bindings),
   create [managed tokens](#managed-tokens) for them, or bind their [OIDC groups](#oidc-sign-in-and-bearer-jwts).
   Move their stubs into their space by exporting them
   (`GET /__admin/export?tag=...`), importing them with `?space=<space>` and deleting the global copies.
4. Clients that read `principal.role` from `/__admin/info` keep working; use `principal.permissions` for new
   decisions. `403` responses carry `error`, `details` and `required`; with an access file or OIDC, `error` is
   always `forbidden` ([Errors](#errors)).
