# Configuration reference

FastMock is configured only through environment variables. They are read once at start-up; nothing that affects
security (tokens, origins, egress policy, secrets, storage, listeners) can be changed through the admin API.

The Helm chart and the operator set most of these variables for you; see [Helm chart](helm.md) and
[Operator](operator.md) for the mapping.

## Value formats

| Type | Format | Examples |
|---|---|---|
| string | as is; an empty or whitespace-only value counts as unset, except `FASTMOCK_OIDC_API_AUDIENCE`, where a set but empty value stops the start | `fastmock:` |
| list | items separated by commas or new lines; blanks are ignored | `a,b`, `"a\nb"` |
| boolean | `1`, `true`, `yes`, `on` / `0`, `false`, `no`, `off` (any case) | `true` |
| duration | an integer followed by `ms`, `s`, `m`, `h` or `d`; no unit means seconds | `250ms`, `30s`, `15m`, `24h`, `7d`, `2` |
| bytes | an integer number of bytes | `10485760` |
| address | `host:port` socket address (an IP, not a host name) | `0.0.0.0:8080`, `[::]:8080` |

An invalid value stops the server at start-up with a message that names the variable, for example
`FASTMOCK_CALLBACK_TIMEOUT: invalid duration unit `y` in `5y``.

## Listeners and storage

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_LISTEN` | address | `0.0.0.0:8080` | Data plane: the port that serves mocks (HTTP/1.1, h2c, HTTPS with HTTP/2 when TLS is configured, gRPC, SSE, plugins). |
| `FASTMOCK_ADMIN_LISTEN` | address | `0.0.0.0:9090` | Control plane: admin API, UI, `/metrics`, `/healthz`, `/readyz`. Must differ from `FASTMOCK_LISTEN`; the server refuses to start otherwise. |
| `FASTMOCK_STORAGE` | `postgres` \| `memory` | `postgres` | `memory` keeps everything in the process (one instance, development and CI). `postgres` stores stubs, journal, callbacks, audit and data in PostgreSQL and hot state in Valkey, so replicas share everything. See [Concepts: storage](../guide/concepts.md#storage-modes). |
| `FASTMOCK_DATABASE_URL` | URL | none, required for `postgres` | PostgreSQL connection URL, for example `postgres://fastmock@db.example.com:5432/fastmock?sslmode=verify-full`. The URL parameters `sslmode`, `sslrootcert`, `sslcert` and `sslkey` work; the `FASTMOCK_DATABASE_SSL_*` variables below override them. |
| `FASTMOCK_DATABASE_PASSWORD` | string | none | Password added to `FASTMOCK_DATABASE_URL` with correct URL encoding. Use it to keep the password out of the URL (for example from a separate Secret key). |
| `FASTMOCK_DATABASE_SSL_MODE` | `disable` \| `allow` \| `prefer` \| `require` \| `verify-ca` \| `verify-full` | `sslmode` from the URL, else `prefer` | How the PostgreSQL connection uses TLS; see [Storage TLS](#storage-tls). Use `verify-full` in production. |
| `FASTMOCK_DATABASE_SSL_ROOT_CERT` | path | none | PEM file with the CA certificate(s) that issued the PostgreSQL server certificate. They are trusted in addition to the public CAs compiled into the binary. |
| `FASTMOCK_DATABASE_SSL_CERT` | path | none | PEM client certificate (chain) for PostgreSQL `cert` authentication. Set together with `FASTMOCK_DATABASE_SSL_KEY`. |
| `FASTMOCK_DATABASE_SSL_KEY` | path | none | PEM private key (PKCS#8, PKCS#1 or SEC1) of the client certificate. |
| `FASTMOCK_DATABASE_MIGRATE` | `auto` \| `skip` | `auto` | `auto` applies pending migrations at start-up. `skip` applies nothing and only checks that the schema is at least the version this binary needs; run `fastmock migrate` separately. See [Database](database.md#running-migrations-separately). |
| `FASTMOCK_DATABASE_MIGRATION_TIMEOUT` | duration | `120s` | Upper limit for migrations at start-up and in `fastmock migrate`, including the wait for another replica that holds the migration lock. |
| `FASTMOCK_DB_MAX_CONNECTIONS` | integer | `10` | Size of the PostgreSQL connection pool per replica. |
| `FASTMOCK_VALKEY_URL` | URL | none, required for `postgres` | Valkey (or any Redis-protocol server) URL: `redis://valkey.example.com:6379/` for plain TCP, `rediss://valkey.example.com:6380/` (or `valkeys://`) for TLS. A database number goes in the path (`/2`). |
| `FASTMOCK_VALKEY_USERNAME` | string | none | ACL user name. Its password is `FASTMOCK_VALKEY_PASSWORD` (or the password in the URL); a user name without a password is refused. Without a user name the password authenticates the `default` user. |
| `FASTMOCK_VALKEY_PASSWORD` | string | none | Password added to `FASTMOCK_VALKEY_URL` with correct URL encoding. |
| `FASTMOCK_VALKEY_TLS_CA_FILE` | path | none | PEM file with the CA certificate(s) of the Valkey server. When set, only these CAs are trusted for Valkey (the public CAs are not). Needs a `rediss://` URL. |
| `FASTMOCK_VALKEY_TLS_CERT_FILE` | path | none | PEM client certificate (chain) for Valkey servers that require client certificates (`tls-auth-clients yes`). Set together with `FASTMOCK_VALKEY_TLS_KEY_FILE`; needs a `rediss://` URL. |
| `FASTMOCK_VALKEY_TLS_KEY_FILE` | path | none | PEM private key of the Valkey client certificate. |
| `FASTMOCK_VALKEY_PREFIX` | string | `fastmock:` | Prefix of every Valkey key. Give each FastMock instance that shares a Valkey database its own prefix (the operator uses `fastmock:<name>:`). |
| `FASTMOCK_STUB_POLL` | duration | `5s` | Interval of the maintenance loop: reloads stubs when their revision changed (PostgreSQL `LISTEN/NOTIFY` normally reloads them at once, polling is the fallback), refreshes gRPC descriptors, updates the `fastmock_stubs_loaded` and `fastmock_callbacks_pending` gauges, and once a minute applies retention. |

`fastmock migrate` applies the database migrations and exits. It reads only the `FASTMOCK_DATABASE_*` variables
(URL, password, TLS and migration timeout) and the logging settings (`FASTMOCK_LOG_FORMAT`, `RUST_LOG`, `OTEL_*`),
so a migration Job needs no Valkey settings.

### Storage TLS

PostgreSQL modes, as implemented by the client library FastMock uses (they differ from libpq in two places):

| Mode | Encrypted | Server certificate checked |
|---|---|---|
| `disable`, `allow` | no (`allow` never tries TLS) | no |
| `prefer` | if the server offers TLS | no |
| `require` | yes | no, even when a CA file is set |
| `verify-ca` | yes | chain to a trusted CA |
| `verify-full` | yes | chain to a trusted CA and the host name in the URL |

The trusted CAs are the public ones compiled into the binary (`webpki-roots`) plus `FASTMOCK_DATABASE_SSL_ROOT_CERT`.
The standard `PGSSLMODE`, `PGSSLROOTCERT`, `PGSSLCERT` and `PGSSLKEY` variables are honoured too; the URL
parameters override them and the `FASTMOCK_DATABASE_SSL_*` variables override both.

Valkey uses TLS when the URL scheme is `rediss://` or `valkeys://`. The server certificate is always verified,
including the host name: against the public CAs, or only against `FASTMOCK_VALKEY_TLS_CA_FILE` when it is set.
The `#insecure` URL fragment, which would switch verification off, is refused.

Checked at start-up (the server and `fastmock migrate` stop with a message that names the setting):

- an unknown `FASTMOCK_DATABASE_SSL_MODE` or `FASTMOCK_DATABASE_MIGRATE` value, a zero or invalid timeout;
- a client certificate without its key or the other way round, from the variables or the URL;
- a CA, certificate or key file that cannot be read or holds no PEM certificate or key;
- `FASTMOCK_VALKEY_TLS_*` with a `redis://` URL (an error, not ignored), `FASTMOCK_VALKEY_USERNAME` without a password.

Warnings are logged, without stopping, for `verify-ca` or `verify-full` without a CA file (only public CAs are
trusted, which fails for a private CA with `UnknownIssuer`) and for a CA file with a mode that does not use it.
Connection errors caused by certificates say which variable to set. Passwords are never logged; the start-up log
shows host, port, database, SSL mode, Valkey address and ACL user only.

## Admin authentication

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_ADMIN_TOKENS` | list | none | Admin tokens as `name:role:sha256hex` entries. `name` is letters, digits, `-`, `_`, `.`, `@`; `role` is `viewer`, `editor`, `admin` or a custom role of the access file (a global binding), or empty for a token that gets only the bindings of the access file; the hash is the hex SHA-256 of the token (`fastmock hash-token`). |
| `FASTMOCK_ADMIN_TOKENS_FILE` | path | none | File with one `name:role:sha256hex` entry per line; blank lines and lines starting with `#` are ignored. Entries from the file and from `FASTMOCK_ADMIN_TOKENS` are combined. |
| `FASTMOCK_ACCESS_FILE` | path | none | YAML file with custom roles, spaces and bindings, read once at start-up; an invalid file stops the server with a message naming the entry. See [Access control](access-control.md). |
| `FASTMOCK_TOKENS_MAX_DAYS` | integer | `365` | Longest lifetime of a [managed token](access-control.md#managed-tokens) in days, 1 to 3650. `expiresInDays` above it is rejected; tokens without `expiresInDays` get 90 days (or this value when it is lower). |
| `FASTMOCK_AUTH_DISABLED` | boolean | `false` | Turns admin authentication off: every caller becomes `anonymous` with the `admin` role and the same-origin exception for browsers is disabled. For a developer laptop only; the server logs a warning at start-up. [Managed tokens](access-control.md#managed-tokens) cannot be created or revoked in this mode. The operator refuses this variable in `MockServer.spec.env`. |
| `FASTMOCK_ADMIN_ALLOWED_ORIGINS` | list | none | Browser origins (exact `scheme://host[:port]`) allowed to call the admin port, for example `https://fastmock-admin.example.com`. Requests without an `Origin` header (curl, test clients) are not affected. A request whose `Origin` equals `http(s)://` plus its own `Host` header is also accepted while authentication is enabled. Anything else gets `403 {"error":"forbidden","details":["origin not allowed"]}` with an access file or OIDC, `403 {"error":"origin not allowed","details":[]}` with static tokens only. |

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_UI_ENABLED` | boolean | `true` | `false`, `0`, `no` or `off` switches the web UI off: `/ui` and everything below it answer 404. The admin API, `/openapi.json` and `/metrics` are not affected. See [Web UI](../guide/ui.md). |
| `FASTMOCK_MCP_ENABLED` | boolean | `true` | Serves the [MCP endpoint](../guide/mcp.md) `/mcp` on the admin port for coding agents. It uses the admin API's authentication, origin check, permissions and audit log, so it grants nothing a token could not do through the admin API; `false` removes it (`/mcp` answers 404). [Mock MCP servers](../guide/mock-mcp-servers.md) on the data port are stubs and need no setting. |
| `FASTMOCK_UI_DATA_PLANE_URL` | list | none | Exact origins (`scheme://host[:port]`, no path, no wildcard) of the data plane as browsers reach it, for example `https://mock.example.com`. The UI's `Content-Security-Policy` allows `connect-src` to `'self'` and these origins only, and the first one is the default target of the test-request panel. Without it, `connect-src` allows `'self'` and the data port on the host name the browser used for the UI (`http(s)://<ui host>:<FASTMOCK_LISTEN port>`). An invalid entry is a start-up error. |

The server does not start without at least one token unless OIDC sign-in is configured or
`FASTMOCK_AUTH_DISABLED=true`. Roles and token handling are described in
[Security](security.md#admin-tokens-and-roles), permissions, custom roles and spaces in
[Access control](access-control.md).

### OIDC sign-in

OIDC is off unless `FASTMOCK_OIDC_ISSUER` is set; setting any other `FASTMOCK_OIDC_*` variable without it is a
start-up error, and so is combining it with `FASTMOCK_AUTH_DISABLED=true`. See
[Access control: OIDC](access-control.md#oidc-sign-in-and-bearer-jwts) for the flow and provider setup.

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_OIDC_ISSUER` | URL | none | Issuer; must equal the `issuer` of `<issuer>/.well-known/openid-configuration` exactly. https; plain http only for `localhost`, `127.0.0.1` and `::1` (tests and local development). |
| `FASTMOCK_OIDC_CLIENT_ID` | string | none | Client id of the confidential client (required with the issuer). |
| `FASTMOCK_OIDC_CLIENT_SECRET` | string | none | Client secret. Set this or `FASTMOCK_OIDC_CLIENT_SECRET_FILE`. |
| `FASTMOCK_OIDC_CLIENT_SECRET_FILE` | path | none | File with the client secret (trailing newline ignored). |
| `FASTMOCK_OIDC_REDIRECT_URL` | URL | none | Public admin URL plus `/__admin/auth/callback`, no query (required). https except on localhost. |
| `FASTMOCK_OIDC_SCOPES` | list | `openid profile email` | Space- or comma-separated scopes; must include `openid`. |
| `FASTMOCK_OIDC_USERNAME_CLAIM` | string | `preferred_username` | Claim with the user name for `user:` bindings; a dotted path reaches into objects. API JWTs without it use `sub`. `sub` (or `oid` with Entra ID) is recommended ([choosing the username claim](access-control.md#choosing-the-username-claim)); with `email`, sign-in requires `email_verified: true`. |
| `FASTMOCK_OIDC_GROUPS_CLAIM` | string | `groups` | Claim with the groups for `group:` bindings (array of strings or one string); a dotted path such as `realm_access.roles` reaches into objects. Falls back to userinfo at sign-in when the ID token lacks it. |
| `FASTMOCK_OIDC_API_AUDIENCE` | string | none | Accept JWTs of the provider with this `aud` as bearer tokens on the admin API; unset, bearer JWTs are off. Set but empty (unlike other strings, see [Value formats](#value-formats)) or equal to the client id, it stops the start; tokens with a `nonce` claim (ID tokens) and tokens larger than 64 KiB are rejected. |
| `FASTMOCK_OIDC_CA_FILE` | path | none | PEM bundle of extra trusted roots for the provider connections (discovery, JWKS, token, userinfo). |
| `FASTMOCK_OIDC_SESSION_TTL` | duration | `8h` | Absolute lifetime of a UI session. A session keeps the groups captured at sign-in until it ends. |
| `FASTMOCK_OIDC_SESSION_IDLE` | duration | `1h` | A session unused for this long ends. |
| `FASTMOCK_OIDC_ALLOWED_HOSTS` | list | none | Host names besides the issuer's that discovery endpoints (token, JWKS, userinfo, authorization, end session) may use; others stop the start. |
| `FASTMOCK_OIDC_INSECURE_COOKIE` | boolean | `false` | Session and login cookies without `Secure` and without the `__Host-` prefix, for local development over plain http. Accepted only with a `localhost`, `127.0.0.1` or `::1` redirect URL. |

## Data-plane TLS and mTLS

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_TLS_CERT_FILE` | path | none | PEM certificate chain for the data port. Set together with `FASTMOCK_TLS_KEY_FILE`; setting only one of them is a start-up error. With TLS the data port negotiates HTTP/2 or HTTP/1.1 through ALPN. |
| `FASTMOCK_TLS_KEY_FILE` | path | none | PEM private key. |
| `FASTMOCK_TLS_CLIENT_CA_FILE` | path | none | PEM bundle of CAs that client certificates must chain to. Enables mTLS. |
| `FASTMOCK_TLS_CLIENT_AUTH` | `required` \| `optional` | `required` | With a client CA: `required` rejects handshakes without a valid client certificate, `optional` accepts them but still verifies any certificate that is presented. |

When a client certificate was verified, the data plane adds the request header `x-fastmock-client-cert-sha256`
(lower-case hex SHA-256 of the DER certificate) so stubs can match on the caller's identity; a header with that name
sent by the client is always removed first. The admin port is plain HTTP; terminate TLS for it at the Gateway or
ingress (see [Helm: TLS](helm.md#tls-and-mtls)).

## Request limits and timeouts

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_MAX_BODY_BYTES` | bytes | `10485760` (10 MiB) | Largest request body on the data port. Larger requests get `413 {"error":"request body too large"}` (checked against `Content-Length` before reading, and while reading chunked bodies). |
| `FASTMOCK_MAX_INFLIGHT_BODY_BYTES` | bytes | `268435456` (256 MiB) | Memory budget for request bodies being processed at the same time. Each request reserves three times its `Content-Length` (or three times `FASTMOCK_MAX_BODY_BYTES` when the length is unknown). A request that cannot get its share within 10 seconds gets `503 {"error":"server busy: too many large requests in flight"}` with `Retry-After: 1`. Must be at least `FASTMOCK_MAX_BODY_BYTES`. |
| `FASTMOCK_MAX_DELAY` | duration | `15m` | Upper bound for response `delay`, chaos delays and SSE event delays. Callback delays have their own limit of 7 days. |
| `FASTMOCK_HEADER_READ_TIMEOUT` | duration | `30s` | Time an HTTP/1.1 client has to send the complete request headers on either port. |
| `FASTMOCK_HTTP2_MAX_CONCURRENT_STREAMS` | integer | `200` | Concurrent HTTP/2 streams (requests, gRPC calls) per client connection on the data port, 1 to 10,000. |
| `FASTMOCK_SHUTDOWN_DELAY` | duration | `5s` | After SIGTERM or Ctrl-C: `/readyz` starts returning 503 at once, the listeners stay open for this long so load balancers can stop sending traffic, then open connections get up to 20 seconds to finish. |
| `FASTMOCK_RECORDING_MAX_STUBS` | integer | `1000` | Number of stubs one recording session may create; further new exchanges are proxied but not stored (`fastmock_recording_skipped_total{reason="limit"}`). |
| `FASTMOCK_JOURNAL_SCAN_TIMEOUT` | duration | `10s` | Deadline of one journal scan (`POST /__admin/requests/find`, `/requests/remove`, `/verify`, `/verify/sequence`, `/requests/count`, `/requests/near-misses`). A scan that runs longer is stopped and answers `503`. Scans run on the blocking thread pool, never on the request-handling threads, and stop as soon as the client disconnects. |
| `FASTMOCK_JOURNAL_SCAN_MAX_WORK` | integer | `4294967296` | Work budget of one scan: the sum, over scanned journal entries, of the entry size in bytes (body, path, query and headers plus 64) times the weight of the pattern (number of matchers plus 1 per KiB of pattern JSON). A scan that would exceed it stops and answers `422`. |
| `FASTMOCK_JOURNAL_SCAN_CONCURRENCY` | integer | `2` | Scans that run at the same time per replica (minimum 1). A scan that does not get a slot within 5 seconds answers `503`. |
| `FASTMOCK_UNMATCHED_DETAILS` | boolean | `false` | Adds `nearMisses` (the three closest stubs and why they did not match) to the 404 body of unmatched data-plane requests. Off by default because it reveals stub definitions to callers of the data port; the same information is always available to viewers through `POST /__admin/requests/near-misses`. |

## gRPC

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_GRPC_REFLECTION` | boolean | `true` | Serves [gRPC server reflection](../guide/grpc.md#server-reflection) (`grpc.reflection.v1` and `v1alpha`) on the data port from the uploaded descriptor sets, so grpcurl, Postman and Kreya work without `.proto` files. `false` answers the reflection methods with `UNIMPLEMENTED`. |
| `FASTMOCK_GRPC_MAX_STREAM_MESSAGES` | integer | `10000` | Request messages one gRPC call may send; the next one ends the call with `RESOURCE_EXHAUSTED`. |
| `FASTMOCK_GRPC_MAX_STREAM_BYTES` | bytes | `67108864` (64 MiB) | Decompressed request bytes one gRPC call may send; more end the call with `RESOURCE_EXHAUSTED`. Each message is also limited to `FASTMOCK_MAX_BODY_BYTES`, compressed and decompressed. |
| `FASTMOCK_GRPC_STREAM_IDLE_TIMEOUT` | duration | `60s` | Longest wait for the next request message (or the end of the request stream); then the call ends with `DEADLINE_EXCEEDED`. |
| `FASTMOCK_GRPC_MAX_STREAM_DURATION` | duration | `1h` | Longest duration of a gRPC call, reading and sending; also caps `grpc.closeAfterMs` of stubs. Reading stops with `DEADLINE_EXCEEDED`; a stream that is still sending ends with the stub's status. |
| `FASTMOCK_GRPC_PROXY_MAX_CALLS` | integer | `256` | [Proxied gRPC calls](../guide/grpc.md#proxying-and-recording) in flight on one replica, 1..10000; more end with `RESOURCE_EXHAUSTED`. |
| `FASTMOCK_GRPC_PROXY_CLIENT_CERT_FILE`, `FASTMOCK_GRPC_PROXY_CLIENT_KEY_FILE` | path | unset | PEM client certificate and key presented to gRPC upstreams over TLS (mTLS), only to the hosts in `FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS`. Both or neither; startup fails if they cannot be read or do not fit. |
| `FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS` | list | unset | Upstream hosts (`name` or `*.domain`) that get the client certificate; required with it. Other upstreams get no certificate. |
| `FASTMOCK_GRPC_RECORD_MAX_MESSAGES` | integer | `100` | Messages per direction a recorded gRPC call may have, 1..1000; longer calls are proxied but not recorded (`fastmock_recording_skipped_total{reason="too_large"}`). |
| `FASTMOCK_GRPC_RECORD_MAX_BYTES` | bytes | `1048576` (1 MiB) | JSON bytes of all messages of a recorded gRPC call, 1 KiB..16 MiB; each message also at most 1 MiB. Larger calls are proxied but not recorded. |

## Request journal

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_JOURNAL_ENABLED` | boolean | `true` | Record data-plane requests and responses. |
| `FASTMOCK_JOURNAL_RETENTION` | duration | `24h` | Entries older than this are deleted (checked once a minute). In memory mode the journal also keeps at most the latest 100,000 entries. |
| `FASTMOCK_JOURNAL_MAX_BODY_BYTES` | bytes | `65536` | Request and response bodies are stored up to this size and marked `…[truncated N bytes]` beyond it. Also limits the bodies shown by `GET /__admin/callbacks`. |
| `FASTMOCK_JOURNAL_QUEUE` | integer | `2048` | Entries waiting to be written. The journal is written in the background after the response; when the queue is full, entries are dropped and counted in `fastmock_journal_dropped_total`. |
| `FASTMOCK_REDACT_HEADERS` | list | see below | Header names (case-insensitive) whose values are masked as `***` (an `Authorization`-style scheme such as `Bearer` is kept). Replaces the default list. |
| `FASTMOCK_REDACT_JSON_KEYS` | list | see below | Field names masked in JSON, form, XML and multipart bodies and in query strings. Matching ignores case, `-` and `_`. Replaces the default list. |

Default `FASTMOCK_REDACT_HEADERS`: `authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`,
`apikey`, `api-key`, `x-auth-token`, `x-callback-token`, `x-goog-signature`, `x-signature`, `x-hub-signature`,
`x-hub-signature-256`, `sha2-signature`, `ocp-apim-subscription-key`, `x-amz-security-token`,
`x-fastmock-signature`. In addition, any header whose name contains one of the words `sig`, `signature`, `hmac`,
`token`, `secret`, `password`, `apikey`, `key`, `credential(s)` (split on `-` and `_`), or ends with `signature` or
`token`, is always masked.

Default `FASTMOCK_REDACT_JSON_KEYS`: `password`, `passwd`, `secret`, `token`, `access_token`, `refresh_token`,
`id_token`, `client_secret`, `otp`, `apikey`, `api_key`, `authorization`, `auth`, `pin`, `cvv`, `sig`,
`signature`, `key`, `private_key`, `session`.

Redaction applies to what FastMock stores and shows (journal, callbacks API, logs, UI). Matching and templates
always see the original request. See [Security: redaction](security.md#redaction).

## Callbacks and outgoing traffic

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_CALLBACK_WORKERS` | integer | `32` | Callbacks delivered at the same time per replica (minimum 1). |
| `FASTMOCK_CALLBACK_BATCH` | integer | `50` | Most callbacks claimed from the outbox in one poll (minimum 1). |
| `FASTMOCK_CALLBACK_POLL` | duration | `200ms` | How often each replica looks for due callbacks. |
| `FASTMOCK_CALLBACK_TIMEOUT` | duration | `10s` | Timeout of one delivery attempt (connect timeout is the smaller of this and 5 s). A claimed callback is leased for twice this value plus 5 s; if a replica stops during delivery, another replica retries after the lease expires. Proxy stubs and recording use the same client with a 30-second request timeout. |
| `FASTMOCK_CALLBACK_RETENTION` | duration | `24h` | Delivered and failed callbacks older than this are deleted. Pending callbacks are kept. |
| `FASTMOCK_CALLBACK_ALLOWED_HOSTS` | list | none (any host) | Host allow-list for callbacks, proxy stubs and recording: exact names (`hooks.example.com`) or suffixes (`*.svc.cluster.local`, which matches `a.svc.cluster.local` but not `svc.cluster.local`). |
| `FASTMOCK_CALLBACK_DENIED_CIDRS` | list | see below | Address ranges that callbacks, proxy stubs and recording never connect to. Checked on literal IPs in the URL and on every address the host name resolves to. Replaces the default list. |
| `FASTMOCK_OUTBOUND_CA_FILE` | path | unset | PEM bundle of extra CA certificates trusted for outgoing HTTPS (callbacks, proxy stubs, recording, contract tests, gRPC upstreams), in addition to the built-in public roots. Startup fails if the file is missing or holds no certificate. |
| `FASTMOCK_STUB_SECRETS` | list | none | Names of secrets that stubs may reference in `callbacks[].hmac.secretRef`. Any other name fails the callback with `secret ... is not configured`. Plugins have their own secrets and are not limited by this list. These secrets can never be sent by contract tests. |
| `FASTMOCK_CONTRACT_SECRETS` | list | none | Secrets that contract tests may send in `secretHeaders`, each bound to the hosts that may receive it: `name=host1\|host2`, for example `petstore-token=petstore.example.com\|*.petstore.example.com`. Hosts are exact names or IP addresses, or `*.suffix` for subdomains. A name may not also appear in `FASTMOCK_STUB_SECRETS`; a malformed entry is a start-up error. Starting a run with `secretHeaders` needs the global permission `contracts:secrets` (the `admin` role bound globally). See [Contract testing: secrets](../guide/contract-testing.md#secrets-and-redaction). |

Default `FASTMOCK_CALLBACK_DENIED_CIDRS`: `127.0.0.0/8`, `::1/128`, `::/128`, `::ffff:127.0.0.0/104`,
`::ffff:169.254.0.0/112`, `::ffff:0.0.0.0/104`, `169.254.0.0/16`, `fe80::/10`, `fd00:ec2::254/128`,
`100.100.100.200/32`, `0.0.0.0/8`, `224.0.0.0/4`, `ff00::/8` (loopback, link-local including cloud metadata
endpoints, unspecified and multicast). Unspecified and multicast addresses are refused even if you replace the
list, and IPv4 addresses embedded in IPv6 (IPv4-mapped, NAT64 `64:ff9b::/96`, 6to4 `2002::/16`, IPv4-compatible)
are checked as IPv4. Private ranges such as `10.0.0.0/8` are allowed by default because callbacks usually go to
services in the same cluster; use `FASTMOCK_CALLBACK_ALLOWED_HOSTS` to narrow them.

Outgoing requests never follow redirects, never use an HTTP proxy from the environment, and refuse URLs with
credentials or schemes other than `http` and `https`. Callbacks, proxy stubs and contract tests never send `Host`,
hop-by-hop or forwarding headers taken from stubs, from the proxied client request or from a run request (the list
is in [Contract testing](../guide/contract-testing.md#headers-that-are-never-sent)); stubs that set them in
`proxy.additionalHeaders` or `callbacks[].headers` are rejected with `400`. Details: [Callbacks](../guide/callbacks.md),
[Security: egress](security.md#egress-policy).

## Secrets

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_SECRETS_DIR` | path | none | Directory with one file per secret (file name = secret name, content = value; trailing new lines are removed). Typically a mounted Kubernetes Secret. |
| `FASTMOCK_SECRET_<NAME>` | string | none | A secret given directly. The name is upper-cased with `-` and `.` replaced by `_`: secret `delivery-hook` is `FASTMOCK_SECRET_DELIVERY_HOOK`. Takes precedence over the file. |

Secret names are 1 to 128 characters of letters, digits, `-`, `_` and `.` and do not start with `.`. Secrets are
used for HMAC signatures (`FASTMOCK_STUB_SECRETS`), by contract tests (`FASTMOCK_CONTRACT_SECRETS`) and by plugins;
templates cannot read them.

## Kafka

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_KAFKA_BOOTSTRAP` | list | none | Kafka bootstrap servers (`host:port`) for stub `messages`. Without it, stubs with `messages` still respond, but publishing fails and is counted as `fastmock_messages_total{result="failed"}`. |
| `FASTMOCK_KAFKA_SECURITY_PROTOCOL` | enum | `plaintext` | `plaintext`, `ssl`, `sasl_plaintext` or `sasl_ssl` (Kafka `security.protocol`, case-insensitive). Applies to stub messages, async mocks and AsyncAPI contract tests. |
| `FASTMOCK_KAFKA_SASL_MECHANISM` | enum | none | `PLAIN`, `SCRAM-SHA-256` or `SCRAM-SHA-512`. Required for `sasl_*`, refused otherwise. |
| `FASTMOCK_KAFKA_SASL_USERNAME` | string | none | SASL user. Required for `sasl_*`, refused otherwise. |
| `FASTMOCK_KAFKA_SASL_PASSWORD` | string | none | SASL password. Required for `sasl_*`, refused otherwise. Never logged. |
| `FASTMOCK_KAFKA_TLS_CA_FILE` | path | none | PEM bundle trusted in addition to the public roots compiled into the binary, for `ssl` and `sasl_ssl`. Also trusted by an `https://` Schema Registry. With `plaintext` or `sasl_plaintext` it is refused unless the registry URL is `https://`. |
| `FASTMOCK_KAFKA_TLS_CERT_FILE` | path | none | Client certificate (PEM, leaf first, optionally followed by intermediates) for mTLS. Set together with `FASTMOCK_KAFKA_TLS_KEY_FILE`; only for `ssl` and `sasl_ssl`. |
| `FASTMOCK_KAFKA_TLS_KEY_FILE` | path | none | Private key of the client certificate (PEM: PKCS#8, PKCS#1 or SEC1, unencrypted). |
| `FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL` | URL | none | Confluent-compatible Schema Registry (`http://` or `https://`), required for messages with `avroSchema`. |
| `FASTMOCK_KAFKA_SCHEMA_REGISTRY_USERNAME` | string | none | HTTP basic authentication for the registry. Set together with the password. |
| `FASTMOCK_KAFKA_SCHEMA_REGISTRY_PASSWORD` | string | none | Registry password. Never logged. |
| `FASTMOCK_KAFKA_ALLOWED_TOPICS` | list | none (any topic) | Topics stub messages may publish to: exact names or prefixes ending in `*` (`orders.*`). It does not grant reading. |
| `FASTMOCK_KAFKA_CREATE_TOPICS` | boolean | `false` | Create a missing topic on its first message (only topics allowed by `FASTMOCK_KAFKA_ALLOWED_TOPICS`). |
| `FASTMOCK_KAFKA_TOPIC_PARTITIONS` | integer | `1` | Partitions of created topics; needs `FASTMOCK_KAFKA_CREATE_TOPICS=true`. |
| `FASTMOCK_KAFKA_TOPIC_REPLICATION` | integer | `-1` | Replication factor of created topics, `-1` for the broker default; needs `FASTMOCK_KAFKA_CREATE_TOPICS=true`. |
| `FASTMOCK_KAFKA_CONTRACT_TOPICS` | list | none (reading disabled) | Topics AsyncAPI contract tests may read: exact names or prefixes ending in `*`. A prefix entry never matches internal topics starting with `_` unless the prefix starts with `_`. Without it, AsyncAPI contract tests answer `400`. |

The security settings are checked at start-up: an unknown protocol or mechanism, missing credentials, a
certificate without its key, an unreadable or empty PEM file, or SASL or TLS settings that the protocol does not
use stop the server with an error naming the variable. See [Kafka messages](../guide/kafka.md) and
[Secure connections](../guide/kafka.md#secure-connections).

## MQTT

Publishing to MQTT from stubs and async mocks, and MQTT [capture sources](../guide/message-capture.md). See
[MQTT and AMQP messages](../guide/mqtt-amqp.md).

| Variable | Type | Default | Meaning |
|---|---|---|---|
| `FASTMOCK_MQTT_URL` | URL | none | `mqtt://host:port` (default port 1883) or `mqtts://host:port` (8883); `tcp://`, `ssl://`, `tls://` also work. No credentials, path or query. Without it MQTT messages fail and MQTT capture sources are refused. |
| `FASTMOCK_MQTT_VERSION` | enum | `5` | `5` or `3.1.1`. |
| `FASTMOCK_MQTT_CLIENT_ID_PREFIX` | string | `fastmock` | Client ids are `<prefix>-pub-<random>` and `<prefix>-cap-<random>`; 1..40 characters. |
| `FASTMOCK_MQTT_USERNAME` | string | none | User name. |
| `FASTMOCK_MQTT_PASSWORD` | string | none | Password; needs the user name. Never logged. |
| `FASTMOCK_MQTT_TLS_CA_FILE` | path | none | PEM bundle trusted in addition to the public roots; `mqtts://` only. |
| `FASTMOCK_MQTT_TLS_CERT_FILE`, `FASTMOCK_MQTT_TLS_KEY_FILE` | path | none | Client certificate and key (PEM) for mTLS; set both. |
| `FASTMOCK_MQTT_KEEP_ALIVE` | duration | `30s` | Keep-alive interval (at least 5 s). |
| `FASTMOCK_MQTT_ALLOWED_TOPICS` | list | none (any) | Topics stubs and async mocks may publish to: names or `prefix*`. |

## AMQP

Publishing to AMQP 0-9-1 brokers (RabbitMQ, LavinMQ, ...) and AMQP capture sources.

| Variable | Type | Default | Meaning |
|---|---|---|---|
| `FASTMOCK_AMQP_URL` | URL | none | `amqp://host:port/vhost` (default port 5672) or `amqps://host:port/vhost` (5671). The virtual host is the URL-encoded path; no path or `/%2f` is `/`. No credentials or query. |
| `FASTMOCK_AMQP_USERNAME`, `FASTMOCK_AMQP_PASSWORD` | string | none (`guest`) | SASL `PLAIN` credentials; set both or neither. The password is never logged. |
| `FASTMOCK_AMQP_SASL_MECHANISM` | enum | `PLAIN` | `PLAIN` or `EXTERNAL` (needs `amqps://` and a client certificate, and no password). |
| `FASTMOCK_AMQP_TLS_CA_FILE` | path | none | PEM bundle trusted in addition to the public roots; `amqps://` only. |
| `FASTMOCK_AMQP_TLS_CERT_FILE`, `FASTMOCK_AMQP_TLS_KEY_FILE` | path | none | Client certificate and key (PEM) for mTLS and `EXTERNAL`; set both. |
| `FASTMOCK_AMQP_ALLOWED_EXCHANGES` | list | none (any) | Exchanges stubs and async mocks may publish to: names or `prefix*`. The default exchange only through `amq.default/<queue>` entries (exact or `prefix*`), since its routing key names the queue. |
| `FASTMOCK_AMQP_DECLARE_EXCHANGES` | boolean | `false` | Declare missing exchanges before publishing (type from the message, default `topic`); exchange names produced by a template are never declared. |

## Message capture

See [Message capture](../guide/message-capture.md).

| Variable | Type | Default | Meaning |
|---|---|---|---|
| `FASTMOCK_CAPTURE_ENABLED` | boolean | `false` | Turns capture on; `/__admin/capture/*` answers `404` otherwise. |
| `FASTMOCK_CAPTURE_SOURCES` | JSON or YAML | none | Sources defined in the configuration (a list, or `{"sources": [...]}`); read-only in the API. |
| `FASTMOCK_CAPTURE_SOURCES_FILE` | path | none | The same in a file. |
| `FASTMOCK_CAPTURE_MAX_SOURCES` | integer | `50` | Sources in total, 1..1000. |
| `FASTMOCK_CAPTURE_MAX_MESSAGES` | integer | `1000` | Messages kept per source, 1..100000. |
| `FASTMOCK_CAPTURE_MAX_MESSAGE_BYTES` | integer | `1048576` | Body bytes kept per message, at most 16 MiB; longer bodies are cut. |
| `FASTMOCK_CAPTURE_MAX_BYTES` | integer | `268435456` | Bytes of all stored messages, 1 MiB to 1 TiB; the oldest are removed beyond it. |
| `FASTMOCK_CAPTURE_MAX_SPACE_BYTES` | integer | a quarter of `FASTMOCK_CAPTURE_MAX_BYTES` | Bytes of the stored messages of one space (and of the global area), at least 64 KiB and at most `FASTMOCK_CAPTURE_MAX_BYTES`; the oldest of that space are removed beyond it. |
| `FASTMOCK_CAPTURE_MAX_AGE` | duration | `24h` | Retention by age. |
| `FASTMOCK_CAPTURE_KAFKA_TOPICS` | list | none | Topics capture sources may read: names, `prefix*` or `*`. Without it, only sources from the configuration read Kafka; sources created through the API are refused. Internal topics (`__*`) are always refused. |
| `FASTMOCK_CAPTURE_MQTT_TOPICS` | list | none | MQTT topic filters capture sources may use: names, `prefix*` or `*`; required for sources created through the API. Filters starting with `$` (`$SYS`, `$share`) are always refused. |
| `FASTMOCK_CAPTURE_AMQP_EXCHANGES` | list | none | Exchanges capture sources may bind to: names, `prefix*` or `*`; required for sources created through the API. `amq.rabbitmq.*` is always refused. |

## Mail

Mail capture is off by default. While `FASTMOCK_MAIL_ENABLED` is not `true`, every other `FASTMOCK_MAIL_*`
variable is ignored. See [Mail](../guide/mail.md).

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_MAIL_ENABLED` | boolean | `false` | Turns on the SMTP, IMAP and POP3 listeners, the provider APIs under `/__mail/` on the data port and the `/__admin/mail/*` routes. |
| `FASTMOCK_MAIL_SMTP_LISTEN` | address | `0.0.0.0:2525` | SMTP with STARTTLS when a certificate is configured. An empty value turns it off. |
| `FASTMOCK_MAIL_SMTPS_LISTEN` | address | off | SMTP over implicit TLS, for example `0.0.0.0:4650`. Needs a certificate. |
| `FASTMOCK_MAIL_IMAP_LISTEN` | address | off | IMAP with STARTTLS when a certificate is configured, for example `0.0.0.0:1143`. |
| `FASTMOCK_MAIL_IMAPS_LISTEN` | address | off | IMAP over implicit TLS, for example `0.0.0.0:1993`. Needs a certificate. |
| `FASTMOCK_MAIL_POP3_LISTEN` | address | off | POP3 with STLS when a certificate is configured, for example `0.0.0.0:1110`. |
| `FASTMOCK_MAIL_POP3S_LISTEN` | address | off | POP3 over implicit TLS, for example `0.0.0.0:1995`. Needs a certificate. |
| `FASTMOCK_MAIL_TLS_CERT_FILE` | path | `FASTMOCK_TLS_CERT_FILE` | PEM certificate chain for STARTTLS, STLS and the implicit-TLS listeners. Set together with `FASTMOCK_MAIL_TLS_KEY_FILE`; when neither is set, the data-plane certificate is used if there is one. |
| `FASTMOCK_MAIL_TLS_KEY_FILE` | path | `FASTMOCK_TLS_KEY_FILE` | PEM private key of that certificate. |
| `FASTMOCK_MAIL_HOSTNAME` | string | `localhost` | Host name in the SMTP greeting, the `EHLO` reply and the POP3 greeting. |
| `FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH` | boolean | `false` | Offer SMTP AUTH and accept IMAP and POP3 logins before TLS even when a certificate is configured. Without a certificate, plaintext logins are always possible. |
| `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH` | boolean | `false` | SMTP clients must authenticate with a user of `FASTMOCK_MAIL_SMTP_USERS` or a managed token holding `mail:write`; otherwise any credentials, and none, are accepted. |
| `FASTMOCK_MAIL_SMTP_USERS` | list | none | SMTP users as `user:hash` entries. Use a salted `pbkdf2-sha256$<iterations>$<salt>$<hash>` value (recommended; the format of mailbox passwords); the hex SHA-256 of the password (`fastmock hash-token`) is still accepted. A malformed or duplicate entry is a start-up error. |
| `FASTMOCK_MAIL_MAX_MESSAGE_BYTES` | bytes | `10485760` (10 MiB) | Largest message, advertised as SMTP `SIZE`; also the limit of injected and provider messages and of IMAP `APPEND`. At most 512 MiB. |
| `FASTMOCK_MAIL_MAX_RECIPIENTS` | integer | `100` | Recipients per SMTP message. |
| `FASTMOCK_MAIL_MAX_LINE_BYTES` | integer | `4096` | Longest command line (SMTP, IMAP, POP3); at least 512. |
| `FASTMOCK_MAIL_IDLE_TIMEOUT` | duration | `30s` | Wait for the next command, including the first one after the greeting. |
| `FASTMOCK_MAIL_DATA_TIMEOUT` | duration | `60s` | Wait for the next chunk of SMTP `DATA` or `BDAT`. |
| `FASTMOCK_MAIL_SESSION_TIMEOUT` | duration | `10m` | Longest SMTP, IMAP or POP3 session. Time spent in IMAP `IDLE` does not count. Mail clients reconnect when it ends. |
| `FASTMOCK_MAIL_IMAP_IDLE_TIMEOUT` | duration | `30m` | Longest IMAP `IDLE` command. |
| `FASTMOCK_MAIL_MAX_CONNECTIONS` | integer | `256` | Open connections over all mail listeners per replica. |
| `FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP` | integer | `32` | Open mail connections per client address; at most `FASTMOCK_MAIL_MAX_CONNECTIONS`. |
| `FASTMOCK_MAIL_MAX_AUTH_FAILURES` | integer | `3` | Failed logins per connection before it is closed. |
| `FASTMOCK_MAIL_LOGIN_FAILURE_LIMIT` | integer | `5` | Failed SMTP AUTH, IMAP and POP3 logins per client address and per user name within 5 minutes; after that, logins from that address or for that user are refused without checking the password for 30 seconds, doubling with each further failure up to 15 minutes. Counted across connections, and across replicas through Valkey with `FASTMOCK_STORAGE=postgres`. `0` turns this off. |
| `FASTMOCK_MAIL_MAX_MESSAGES` | integer | `1000` | Messages kept per space (and in the global area); the oldest are removed first. |
| `FASTMOCK_MAIL_MAX_BYTES` | bytes | `268435456` (256 MiB) | Raw message bytes kept per space (and in the global area); the oldest messages are removed first, the newest one always stays. At least `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`. |
| `FASTMOCK_MAIL_MAX_MAILBOXES` | integer | `10000` | IMAP/POP3 mailboxes per space (and in the global area). Messages to further addresses are still stored and shown in the admin API, but get no mailbox; the first refusal per space is logged. Mailboxes without messages and without a password are removed by the retention sweep. |
| `FASTMOCK_MAIL_MAX_AGE` | duration | `7d` | Messages older than this are removed (checked once a minute). |

Checked at start-up: mail listeners must use ports of their own (not each other's, `FASTMOCK_LISTEN` or
`FASTMOCK_ADMIN_LISTEN`); implicit-TLS listeners need a certificate; the certificate and key must be readable;
limits and timeouts must be greater than zero. The error names the variable, for example
`FASTMOCK_MAIL_SMTPS_LISTEN needs a certificate: set FASTMOCK_MAIL_TLS_CERT_FILE and FASTMOCK_MAIL_TLS_KEY_FILE (or FASTMOCK_TLS_CERT_FILE and FASTMOCK_TLS_KEY_FILE)`.
With `FASTMOCK_STORAGE=postgres`, messages are stored in PostgreSQL and mail events use Valkey publish/subscribe
([Database](database.md#valkey-and-redis)).

Provider webhook signing keys and URL passwords are stored encrypted with the secret `mail-webhook-key`
(`FASTMOCK_SECRET_MAIL_WEBHOOK_KEY`, or the file `mail-webhook-key` in `FASTMOCK_SECRETS_DIR`): use a long random
value, for example the output of `openssl rand -base64 32`, and the same value on every replica. With
`FASTMOCK_STORAGE=postgres` it is required for webhooks that have a signing key or a URL password; with memory
storage a random key of the process is used when it is not set. Changing the key makes stored webhook secrets
unreadable: set them again with `PUT /__admin/mail/webhooks`.

## Plugins

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_PLUGINS` | list | all compiled-in plugins | Plugins to enable; `all` enables every plugin. `fastmock plugins` lists what the binary contains. |
| `FASTMOCK_PLUGIN_<PLUGIN>_<KEY>` | string | none | Setting `<key>` of plugin `<plugin>` (both upper-cased, `-` becomes `_`); the plugin sees the key in lower case. |

Each plugin is mounted on the data port under `/<name>` and may add admin routes under
`/__admin/plugins/<name>/`. Plugin-specific settings are documented with the plugins; see
[Writing plugins](../guide/plugins.md).

## Logging and telemetry

| Variable | Type | Default | Effect |
|---|---|---|---|
| `FASTMOCK_LOG_FORMAT` | `json` \| `text` | `json` | Log format on stdout. |
| `RUST_LOG` | filter | `info,sqlx=warn,hyper=warn,h2=warn,reqwest=warn,opentelemetry=warn` | Log levels per module, for example `info,fastmock_server::outbox=debug`. Audit records use the target `audit`. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | URL | none | Enables OpenTelemetry export of traces and logs over OTLP/HTTP (protobuf), for example `http://otel-collector:4318`. Without an endpoint nothing is exported. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | URL | none | Per-signal endpoints; setting either also enables export. |
| `OTEL_SERVICE_NAME` | string | `fastmock` | `service.name` resource attribute. |
| `OTEL_SDK_DISABLED` | boolean | `false` | `true` disables export even when an endpoint is set. |

Other standard OpenTelemetry SDK variables (such as `OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_RESOURCE_ATTRIBUTES`)
are read by the SDK. Prometheus metrics are always available on `/metrics`. See
[Observability](observability.md).

## Command line

```text
fastmock serve            run the server (the default command)
fastmock migrate          apply PostgreSQL migrations and exit (uses the FASTMOCK_DATABASE_* variables)
fastmock hash-token       print only the hex SHA-256 of a token read from the argument or stdin
fastmock hash-token --generate   generate a new random token and print it with its hash
fastmock validate FILE    validate a stub, a list of stubs or {"stubs": [...]} without a server
fastmock plugins          list compiled-in plugins
fastmock --version
```

Build-time variable: `FASTMOCK_GIT_SHA`, when set during `cargo build`, is embedded into the binary and reported
by `GET /__admin/version` as `gitSha`.

## Operator settings

The operator (`fastmock-operator`) reads these variables; the operator chart sets them.

| Variable | Default | Effect |
|---|---|---|
| `FASTMOCK_OPERATOR_NAMESPACE` | empty (all namespaces) | Watch only this namespace. |
| `FASTMOCK_OPERATOR_SERVER_IMAGE` | `fastmock:<operator version>` | Image used when a `FastMock` does not set `spec.image`. |
| `FASTMOCK_OPERATOR_ALLOWED_IMAGE_PREFIXES` | the repository of the default image | Comma-separated image prefixes a `FastMock` may use. |
| `FASTMOCK_OPERATOR_SPACE_FROM_NAMESPACE` | `false` | `true` puts the stubs of every `MockApi` into the space named like its namespace; a different `spec.space` is refused. |
| `FASTMOCK_OPERATOR_ADMIN_URL_TEMPLATE` | `http://{name}-admin.{namespace}.svc:9090` | How the operator reaches a server's admin API. |
| `FASTMOCK_OPERATOR_LISTEN` | `0.0.0.0:8081` | Operator `/healthz`, `/readyz` and `/metrics`. |
| `RUST_LOG` | `info,kube=warn,hyper=warn` | Operator log levels. |
