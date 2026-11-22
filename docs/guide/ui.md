# Web UI

FastMock ships a web UI inside the `fastmock` binary. It runs in the browser, talks only to the admin API, and
covers everything the admin API offers: stubs, the request journal, verification, scenarios, callbacks,
recording, chaos rules, files, services, contract tests, async mocks, settings, managed admin tokens, the access
configuration, the audit log and the API reference.

- [Opening the UI](#opening-the-ui)
- [Signing in, permissions and signing out](#signing-in-permissions-and-signing-out)
- [Spaces](#spaces)
- [Screens](#screens)
- [API docs (Swagger UI)](#api-docs-swagger-ui)
- [Security](#security)
- [Building, embedding and disabling the UI](#building-embedding-and-disabling-the-ui)
- [Metrics](#metrics)

## Opening the UI

The UI is served by the **admin listener** (`FASTMOCK_ADMIN_LISTEN`, default `0.0.0.0:9090`), never by the data
plane:

```sh
# hash-token prints only the hex SHA-256 of the token
FASTMOCK_STORAGE=memory \
FASTMOCK_ADMIN_TOKENS="me:admin:$(fastmock hash-token "$FASTMOCK_TOKEN")" \
fastmock serve
# open http://localhost:9090/ui
```

| Path | Content |
|---|---|
| `/ui`, `/ui/` | the application |
| `/ui/<route>` | client-side routes, for example `/ui/stubs/<id>` or `/ui/requests?matched=false`; bookmarkable |
| `/ui/api` | Swagger UI for the admin API |
| `/ui/config.json` | data-plane port, scheme and the configured data-plane URL (`FASTMOCK_UI_DATA_PLANE_URL`), used to build "test this stub" URLs (no secrets) |
| `/ui/assets/*` | hashed JavaScript and CSS bundles |

The UI follows the operating system's light or dark preference; the sun/moon button in the header switches it and
the choice is remembered in the browser. The layout adapts to narrow screens (the navigation moves into a menu
behind the burger button) and every action is reachable with the keyboard; "Skip to content" is the first
focusable element.

## Signing in, permissions and signing out

The sign-in page asks for an admin token, the same bearer token the admin API takes (see
[Admin API](admin-api.md#authentication)): a static token or a [managed token](#admin-tokens). When the server
has OIDC configured ([Access control: OIDC](../operations/access-control.md#oidc-sign-in-and-bearer-jwts)), the
page also offers **Sign in with SSO**, which sends the browser to the identity provider and back to the page you
were opening. The UI checks the sign-in with `GET /__admin/info` and shows the principal in the header: its name,
its kind (`token`, `managed token` or `user`) and a short summary of its permissions, derived from them: the role
held globally and the role held in all spaces or in a single space, for example `viewer · editor in all spaces`
or `editor in payments` (`custom` when the permissions match no built-in role). The account menu lists the same
per scope (global, all spaces and each space with more permissions) and the groups of an SSO user.

- A token is kept in memory and in the tab's `sessionStorage`, so it survives a reload of the tab but not
  closing it, and it is never written to `localStorage` or cookies. It is sent only to the admin listener, as
  `Authorization: Bearer <token>`.
- After SSO sign-in the browser holds only the server's `HttpOnly` session cookie; the UI never sees an OIDC token.
  Every admin request then carries the `X-FastMock-Request: 1` header that the server requires from cookie
  sessions. A failed SSO sign-in comes back to the sign-in page with the reason (for example "your account has no
  access to this FastMock server" when no binding matches the user or its groups).
- **Sign out** (account menu in the header) forgets the token and clears all cached data; for an SSO session it
  also ends the session on the server and, when the provider supports it, at the provider.
- When the server answers `401` (token removed, rotated, revoked or expired, or the SSO session expired), the UI
  returns to the sign-in page and says why.
- When the server runs with `FASTMOCK_AUTH_DISABLED=true` (local development only), the UI opens without a token
  and shows "Authentication is disabled on this server" in the account menu.

The principal's permissions (`principal.permissions` of `/__admin/info`, see
[Access control: permissions](../operations/access-control.md#permissions)) decide what the UI offers. Controls
for actions the principal may not take are hidden or disabled; a disabled control explains in its tooltip which
permission is missing, for example "Requires the stubs:write permission in space billing". The server enforces
the same rules, so hiding a button is a convenience, not the protection. With the built-in roles:

| Role | In the UI |
|---|---|
| `viewer` | every screen read-only: no create, edit, delete, import, clear, trigger or upload controls; sensitive stub header values are masked by the server; the audit log and admin tokens are hidden, and the Access page shows only your own access |
| `editor` | everything that changes test state: stubs, imports, journal, callbacks, scenarios, chaos, KV, files, recordings, snapshots, contract tests, async mocks, settings, gRPC descriptors |
| `admin` | everything an editor can do, plus the audit log, admin tokens, the spaces, roles and bindings on the Access page, secret headers of contract tests and deleting all stubs |

Custom roles of the access file get exactly the controls their permissions allow.

## Spaces

When the access file defines [spaces](../operations/access-control.md#spaces), the header shows a space selector
with the spaces you can see. **All spaces** is offered when you hold a permission globally or are bound to several
spaces; a principal bound to a single space always works in it. A binding or token in all spaces
(`spaces: ["*"]`) lists every space and counts as several, unless the server has only one space. The choice is
remembered in this browser.

- Lists send the selected space to the server (`?space=`): stubs, the request journal, near misses, verification,
  scenarios, callbacks, files, contract tests, admin tokens and the dashboard counters. With **All spaces** they
  show everything you may read, with a **Space** column (a dash marks the global area).
- Create and import forms (new stub, import, file upload, scenario state, KV, contract test runs) have a **Space**
  field that starts at the selected space and offers only the areas where you hold the permission: the global
  area (for global principals) and your spaces. A stub's space cannot be changed later.
- Clearing actions (journal, callbacks, scenarios, deleting a tag) act on the selected space; with **All spaces**
  they act on everything you may change.

Without spaces (no access file, or one without `spaces`), the selector and the Space columns are not shown.

## Screens

Every list shows a loading state, an empty state and an error state with a retry button; every change shows a
notification, and every destructive action (delete, clear, reset, remove all) asks for confirmation first.

### Dashboard

Server name, version (`GET /__admin/version`), storage, uptime, the signed-in principal, and four counters that
link to the matching screen: stubs loaded, journal entries, unmatched requests and pending callbacks (refreshed
every 5 seconds). Below them: the latest unmatched requests (click one to open it in the journal), the enabled
provider plugins with their mount paths, and a filterable view of the Prometheus metrics from `/metrics` with a
link to the raw text for Grafana or `curl`.

### Stubs

A paginated table (25 per page) of all stubs with search by name or path, a tag filter and an
all/enabled/disabled switch; the filters are kept in the URL. Each row has an on/off switch (a JSON merge patch
`{"enabled": false}`), and a menu to edit, duplicate, export the stub as a JSON file or delete it.

- **Export** downloads `GET /__admin/export` (only the selected tag when a tag filter is set) as a file that
  **Import** accepts again.
- **Import** takes a dropped or chosen file (up to 32 MiB). A FastMock export is sent to
  `POST /__admin/stubs/import`; anything else goes to `POST /__admin/import`, which detects OpenAPI, WireMock,
  MockServer, Postman, HAR, AsyncAPI, GraphQL SDL, WSDL, SoapUI and Microcks documents. **Preview** runs a dry run
  and shows how many stubs would be created and the converter warnings; **Import** saves them under the given tag,
  replacing stubs that already carry it unless "Replace" is cleared.
- **Delete tag** (with a tag filter) removes every stub carrying the tag.

### Quick mock

A guided page for a first mock (`/ui/quick-mock`, shown to principals with `stubs:write`; an empty stubs page links
to it). Templates prefill the form; step 1 sets method, path, status, body and headers, step 2 switches on
behaviours that map to stub features: request matching, credentials (with a second stub that answers 401), delay,
fault, random failures (a `random` response list), templating, a response sequence, a scenario, a callback, CORS
headers (with an OPTIONS stub when the browser needs a preflight) and `times`/`ttlSeconds`. The side panel shows
each stub as `POST /__admin/stubs` takes it and the result of `POST /__admin/stubs/validate`. **Create mock**
saves the stubs (tagged `quick-mock`) in the chosen space, **Update mock** replaces them, and **Try it** calls the
data plane from the browser and shows the call as cURL, JavaScript, Python and Java code. **Open in full editor**
continues in the stub editor.

### Stub editor

Opened from the stubs table (`/ui/stubs/<id>`), from **New stub**, or from "Create stub from this request" in the
journal. It has three tabs:

- **Form** for the common fields: name, priority, tags, enabled; method and URL matcher (path equals, path
  template, path regex, path and query equals, path and query regex, any path); query, header and path-parameter
  conditions (equals, contains, matches regex, does not match, present, absent, case-insensitive); body matchers as
  a JSON array; status, fault, response headers, body as JSON, text or Base64, Handlebars templating on/off; a fixed
  or random delay; and a scenario (name, required state, new state). Fields the form does not show (callbacks,
  Kafka messages, response sequences, proxying and the rest) are kept unchanged.
- **JSON** for the whole stub. The editor validates the document against the `Stub` schema from `/openapi.json`
  while you type (unknown fields, wrong types, missing `request`, invalid enum values are underlined with a
  message), completes property names and enum values with `Ctrl+Space`, and shows a field's type and description
  when you hover over a key.
- **Test this stub** builds a sample request from the matchers (path template parameters, `equalTo` query and
  header values, `equalToJson` or `equalTo` bodies) and lets you edit and send it to the data plane. The base URL
  defaults to the first `FASTMOCK_UI_DATA_PLANE_URL` origin, or to the browser's host with the data-plane port
  from `/ui/config.json`, and can be changed (it is remembered in the browser; the browser only lets the panel
  reach the origins allowed by the Content-Security-Policy below). The panel shows status, timing, headers and the formatted body, and always offers
  the equivalent `curl` command.

Independently of the tab, the editor sends the current draft to `POST /__admin/stubs/validate` (debounced) and
shows whether the server accepts it, with the server's message when it does not. **Save** creates
(`POST /__admin/stubs`) or replaces (`PUT /__admin/stubs/{id}`) the stub.

The data plane is a different origin than the UI, so a browser can call it only when the response carries CORS
headers. Add them to stubs you want to test from the UI:

```json
{
  "name": "Get user",
  "request": {"method": "GET", "urlPathTemplate": "/api/users/{id}"},
  "response": {
    "status": 200,
    "headers": {"content-type": "application/json", "access-control-allow-origin": "*"},
    "jsonBody": {"id": "{{request.pathParams.id}}"}
  }
}
```

Without them the panel reports that the browser blocked the request and you use the `curl` command instead.

The Content-Security-Policy of the UI lets the browser connect to exact origins only: the UI's own origin and the
data plane. When the data plane is exposed under its own host name or port (for example behind an ingress), list
its public origins in `FASTMOCK_UI_DATA_PLANE_URL`:

```sh
FASTMOCK_UI_DATA_PLANE_URL=https://mock.example.com,https://mock-eu.example.com:8443
```

Without the setting, the allowed data-plane origin is the host name the browser used to open the UI with the
data-plane port, for example `http://fastmock.example.com:8080` for a UI opened at
`http://fastmock.example.com:9090/ui`. Any other target can only be tested with `curl`.

### Request journal

The newest requests first, refreshed every 2 seconds; **Pause** stops the refresh. Filters, all kept in the URL:
matched/unmatched, method, path prefix, source (`stubs` or a plugin), body contains, correlation, since and a
limit (up to 10,000). Clicking a row (or `Enter` on it) opens a drawer with the request (method, path, query,
headers, body pretty-printed as JSON or XML), the response status and body, the matched stub (linked), the trace
id, the correlation and the duration, and these actions:

- **Create stub from this request**: opens the stub editor with a stub that matches this method and path (and the
  JSON body with `ignoreExtraElements`), answering with the recorded status and body for matched requests.
- **Near misses** for an unmatched request.
- **Copy as curl**.

Editors can **Clear** the journal. Bodies and headers are shown exactly as the server stored them, that is already
redacted (`***`) and size-limited.

### Near misses

Explains why a request did not match. Pick one of the recent unmatched requests, or describe one (method, path,
query, headers, body) and press **Explain**. For each of the closest stubs (`POST /__admin/requests/near-misses`)
the screen shows the share of matched conditions, the conditions that failed, and a line diff between the stub's
request pattern ("stub expects") and a pattern built from the request ("request had").

### Verify

Build a request pattern with the form (method, URL matcher) or the JSON editor (validated against the
`RequestPattern` schema) and run one of:

- **Count**: `POST /__admin/requests/find`, shows how many journal entries match and lists them.
- **Verify**: `POST /__admin/verify` with exactly / at least / at most N; shows "Verified" or "Not verified" with
  the expectation, the actual count and the closest requests.
- **Sequence**: `POST /__admin/verify/sequence` with up to 50 ordered patterns; shows how many steps were found in
  order and the journal ids.

An optional source limits all three to one source.

### Scenarios

Current state of every scenario that has moved (a scenario without a stored state is `Started`), with a filter.
Editors set any scenario to a state, reset one to `Started`, or reset all.

### KV store

Look up a key of the shared key/value store that templates use, see its value, and (editors) set a value with an
optional TTL or delete the key. When the server lists keys, the key list with a prefix filter is shown next to it.

### Chaos

The chaos rules in a table (id, method, path prefix, source, probability, effect, expiry) and a form to add one:
probability slider, status, fault and delay. A **JSON** tab edits the whole rule list with schema validation.
Changes are local until **Save rules** (`PUT /__admin/chaos`); **Remove all** deletes every rule.

### Callbacks

The callback outbox with status (pending, sent, failed), attempts, the last status or error, due time and
correlation, filtered by status, correlation and source and refreshed every 3 seconds. A row opens the delivery
with its delivery id, timestamps, `traceparent`, headers (signatures masked) and body. Editors can clear the
outbox.

### Recordings

Start recording against a target base URL with a tag, the response headers to keep and the request headers to
match on; the screen shows who started the recording and when, and **Stop recording** reports how many stubs were
recorded. **Snapshot the journal** turns journal entries (filtered by path regex, since and matched/unmatched)
into stubs with a tag, optionally keeping request headers and turning repeated requests into scenarios.

### Files

Body files for `response.bodyFileName`: name, content type, size, last change. Editors upload by dropping files
(optionally into a folder such as `payments/v1`, at most 10 MiB each), and everyone can download them. With
spaces, a file belongs to a space and stubs of that space find it there first.

### Settings

The global response settings (`/__admin/settings`): a default delay for stubs without their own, a delay added
to every response (fixed, uniform or log-normal), and whether proxy stubs forward requests.

### gRPC

The uploaded protobuf descriptor sets and the services they define, plus the services currently loaded. Editors
upload a `FileDescriptorSet` (`protoc --include_imports --descriptor_set_out=api.pb ...`, up to 16 MiB) under a
name, or remove one. In the journal a gRPC call shows its kind, final status and the request and response messages
as numbered lists (with a note when the journal kept only the first ones); the stub editor summarises the stream
messages, status, trailers and reply rules of a gRPC stub, which are edited in the JSON tab.

### Async mocks

Kafka and WebSocket mocks imported from AsyncAPI: name, protocol, channel, publish interval and messages. Editors
enable or disable a mock, publish one message now (**Trigger**, shows topic, partition and offset), edit the JSON
or delete it.

### Services

The service catalog, Microcks style: one card per imported API with type (REST, SOAP, GRAPHQL, EVENT, GRPC,
GENERIC), version, number of operations and source format, filtered by name, type and label. A service shows its
operations with method, path and links to the stubs that serve each operation, the stored specification and
companion artifacts. Editors can delete a service together with its stubs.

### Contract tests

Start a run against an imported service or a pasted OpenAPI/AsyncAPI document, with the target URL, the
operations to test (all by default), headers, secret headers (only with the global `contracts:secrets` permission,
which admins bound globally have; header name to secret name, see
[Contract testing: secrets](contract-testing.md#secrets-and-redaction)) and a per-call timeout. The history lists
runs with status and passed/total; a run opens per-operation results with the request, the response excerpt, the
received messages for event APIs, and every validation error. Viewers see the verdict, status code and size of
each result only, with a note that the details are reserved to editors.

### Admin tokens

Shown when the server supports managed tokens (`tokens` in the `features` of `GET /__admin/version`) and you
hold the `tokens:read` permission (admins, or a custom role such as a team lead). The list shows the
managed tokens you may manage, filtered by status (active by default) and by the selected space: name and
description, role (hover for its permissions), scope (global or spaces), who created it, creation, expiry, last
use and status. Static tokens from `FASTMOCK_ADMIN_TOKENS` are not listed.

- **Create token** (needs `tokens:write`) asks for a name, an optional description, a role, the scope and the
  lifetime in days. Only roles you may grant are offered, and only in the spaces where you may grant them: you
  must hold every permission of the role there ([escalation rule](../operations/access-control.md#managed-tokens)),
  so a space-bound lead cannot create global tokens. **All spaces** is offered as a scope when you hold the
  role's permissions globally or in all spaces; the token then works in every space, including spaces added later.
  Tokens in all spaces show **All spaces** in the scope column. Under the role the form lists the permissions
  the token will hold in the chosen scope: a token limited to spaces or all spaces does not get the role's
  permissions that act on the whole server (recordings, chaos, gRPC, audit and the like), and the form says how
  many are left out. The lifetime is limited to the server maximum
  (`FASTMOCK_TOKENS_MAX_DAYS`) and defaults to 90 days, or `FASTMOCK_TOKENS_MAX_DAYS` when lower. A token
  created with a managed token expires no later than that token.
- After creation the value is shown **once**, with a copy button and a warning; it is kept only in the dialog and
  dropped when the dialog closes. The dialog is the confirmation; no separate notification is shown. The UI never stores or logs it, and the server never returns it again.
- **Revoke** (after a confirmation; needs `tokens:write` in the token's spaces, and for a token in all spaces
  `tokens:write` in all spaces, globally or through `spaces: ["*"]`) stops the token, and the tokens created with it, on every replica within 10
  seconds; revoked and expired tokens stay visible under their status filters. Tokens created by an SSO user keep
  working after that user leaves; the creator column shows whose tokens to revoke.

### Access

The **Access** page (`/ui/access`, next to Admin tokens in the navigation) shows who may do what. It is
read-only.

- **Your access**, shown to everyone: your global permissions and, for each space, your effective permissions there
  (from `GET /__admin/info`). Permissions you hold in all spaces are shown under **All spaces**.
- With the `access:read` permission (admins, or a custom role that lists it, bound globally) the page also shows
  three tables from `GET /__admin/access`: **Spaces** (name, path prefixes, hosts), **Roles** (name, a badge for
  built-in roles, permissions) and **Bindings** (subject, role, scope: **Global**, **All spaces** or the list of
  spaces). Bindings include the global role of every static token written `name:role:hash` in
  `FASTMOCK_ADMIN_TOKENS` (field `source: adminTokens`), marked with an **admin tokens** badge next to the subject,
  so every admin is listed; without an access file the Bindings table shows only these.

Spaces, roles and bindings cannot be created or changed in the UI. They come from the
[access file](../operations/access-control.md#the-access-file) (`FASTMOCK_ACCESS_FILE`, the `access` values of the
Helm chart or the operator's `spec.access.configMapRef`) and from the static token list (`FASTMOCK_ADMIN_TOKENS`).
A change takes effect after a restart; with Helm and the operator the pods roll on their own.

### Audit log

Needs the `audit:read` permission (admins). Every change made through the admin API with time, principal, role and action, filtered by
principal, action and detail text. An entry opens its detail: stub updates as a table of changed fields with the
values before and after, deletions as a diff of the removed stub, everything else as JSON.

## API docs (Swagger UI)

`/ui/api` renders Swagger UI for `/openapi.json`. The document lists every admin route with its parameters,
schemas, examples and error responses. "Try it out" is enabled, and every request that Swagger UI sends to the
admin listener carries the token you signed in with (or, after SSO sign-in, the session cookie and the
`X-FastMock-Request` header), so calls run with your permissions (a viewer gets `403` for changes). The token is attached only when the request URL, resolved against the page, has exactly the UI's origin
(scheme, host and port); requests to any other origin are sent without it. There is no need to use the **Authorize** dialog. Swagger UI is bundled into the binary and loaded only
when this page is opened; it does not contact any external service.

## Security

- **Content-Security-Policy.** Every UI response carries

  ```text
  default-src 'none'; script-src 'self'; style-src 'self' 'nonce-<random>'; img-src 'self' data:;
  font-src 'self' data:; connect-src 'self' <data-plane origins>;
  manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'
  ```

  `<data-plane origins>` are the exact origins from `FASTMOCK_UI_DATA_PLANE_URL`, or, without that setting,
  `http(s)://<host the UI was opened with>:<data-plane port>`; never a wildcard.

  Scripts come only from the binary: no inline scripts, no `eval`, no CDN. The style nonce is new for every load
  of the page and lets the editor and dialog components add their style sheets; inline `style` attributes stay
  blocked. Responses also carry `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` and
  `Referrer-Policy: no-referrer`.
- **No HTML from data.** Request bodies, stub names, headers and every other value are rendered as text.
- **Token handling** as described in [Signing in](#signing-in-permissions-and-signing-out); the token never
  appears in URLs. SSO sessions use the server's `HttpOnly` cookie and the `X-FastMock-Request` header; cookies
  are never sent with token-authenticated requests.
- **Managed token values** appear only in the dialog right after creation.
- **Same rules as the API.** The UI has no privileges of its own; it calls the documented admin endpoints with
  your token.

## Building, embedding and disabling the UI

The UI source lives in `ui/` (see `ui/README.md` for development). `npm ci && npm run build` writes `ui/dist`,
and `cargo build` embeds that directory into the binary, so a release binary needs no files at runtime:

```sh
cd ui && npm ci && npm run build && cd ..
cargo build --release -p fastmock-server
```

The Docker image does both: a pinned Node stage builds the UI, and the Rust stage embeds it.

- **Binary built without `ui/dist`.** The server works normally; `/ui` answers `404` with a short page that says
  the UI was not built into this binary.
- **Switching the UI off.** Set `FASTMOCK_UI_ENABLED=false` (also `0`, `no` or `off`); `/ui` and everything below
  it then answer `404`. The admin API, `/openapi.json` and `/metrics` are unaffected.

```sh
FASTMOCK_UI_ENABLED=false fastmock serve
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:9090/ui     # 404
```

Caching: files under `/ui/assets/` have content hashes in their names and are served with
`Cache-Control: public, max-age=31536000, immutable`; `index.html` and `/ui/config.json` with `no-cache`, so a new
server version is picked up on the next page load.

## Metrics

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_ui_requests_total` | `kind` = `index`, `asset`, `missing` | UI page loads, asset downloads and requests for asset files that do not exist |

All data the UI shows comes from admin endpoints, so their metrics (`fastmock_admin_*`) and the audit log cover
what users do in the UI.
