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
| `tokens:read`, `tokens:write` | reserved for token management through the API | space |

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
| `user:<username>` | a user signed in through OIDC, by user name (reserved for OIDC sign-in) |
| `group:<group>` | a user signed in through OIDC, by group (reserved for OIDC sign-in) |

A token entry with a role is a global binding of that role, as before. To give a token only space bindings,
leave its role empty: `ci-billing::<sha256>`. Such an entry needs at least one `token:ci-billing` binding.

A principal is allowed a permission in a space when one of its bindings grants it globally, or grants it in that
space and the permission is a space permission. A binding of `admin` in a space therefore grants every space
permission there but none of the global ones (`stubs:purge`, `system:reset`, `audit:read` and so on).
`info:read` comes with any binding.

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
(the effective permissions in a space are both lists together). `kind` is `token` for static tokens (`anonymous`
when authentication is disabled). `role` is kept for older clients: the highest built-in role whose permissions
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

Denials count in `fastmock_admin_auth_failures_total` with `reason="role"` for principals holding only global
built-in roles and `reason="permission"` otherwise. Audit events record the principal kind (`principalKind`) and
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
3. Give teams new tokens with an empty role and space bindings (`name::<hash>` plus `token:name` bindings), or
   bind their OIDC groups once OIDC sign-in is available. Move their stubs into their space by exporting them
   (`GET /__admin/export?tag=...`), importing them with `?space=<space>` and deleting the global copies.
4. Clients that read `principal.role` from `/__admin/info` keep working; use `principal.permissions` for new
   decisions. `403` responses keep `error`, `details` and `required`.
