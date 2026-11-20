# fastmock-mail

Mail capture for FastMock: SMTP (and later IMAP/POP3) listeners, the message model, parsing and
matching. The crate has no HTTP dependency; `fastmock-server` wires it in. FastMock never delivers mail.

## Layout

| Where | What |
|---|---|
| `fastmock-core/src/mail/` | Stored model (`MailMessage`, `Envelope`, `MailSummary`, `MailEntry`), search filter (`MailFilter`, `MailTerm`), paging (`MailQuery`, `MailCursor`), the `MailStore` and `MailEvents` traits, `MemoryMailStore`, `LocalMailEvents`/`MailEventHub`, and `conformance` (checks every store must pass). Re-exported here. |
| `src/service.rs` | `MailService::accept(Incoming)`: parse once, route, label, store with mailbox deliveries, publish `message.received`. `sweep` and `publish_deleted` for retention and deletes. |
| `src/parse.rs` | `ParsedMail` (mail-parser, depth 16, 500 parts, raw size cap): `summary`, `detail` (`MailDetail`), `text`, `html`, `part`, `html_with_inline_parts` (`cid:` to `data:`), `header_values`, `recipients`. |
| `src/routing.rs` | `mailDomains` checks (`check_domain`, `domains_overlap`), `MailDomains`, `route` (M3). |
| `src/address.rs` | `domain_of`, `mailbox_of` (lowercase, plus tag removed), `plus_tag`. |
| `src/labels.rs` | Label rules: plus addresses, `X-FastMock-Labels`, `normalize_labels` for the API. |
| `src/matching.rs` | `MailMatch` (stub string matchers), `CompiledMailMatch::failures` for wait/verify and near misses, `MatchView`. |
| `src/query.rs` | `parse_query`: `from: to: cc: subject: has:attachment label: after: before:`, free text, quotes. |
| `src/extract.rs` | `links` (`/links`) and `CodePattern` (`/codes`), size-bounded. |
| `src/compose.rs` | `ComposeRequest` + `compose` (mail-builder) for JSON injection and provider payloads. |
| `src/config.rs` | `MailConfig` (all `FASTMOCK_MAIL_*` values with defaults and `validate`); the server reads the environment in `fastmock-server/src/config.rs` (`mail_settings`). |
| `src/tls.rs` | `server_config(&MailTlsFiles)`: rustls config for SMTPS/IMAPS/POP3S and STARTTLS/STLS. |
| `src/smtp/` | SMTP listener (B1). |

Server side: `AppState::mail: Option<Arc<MailService>>` (`None` unless `FASTMOCK_MAIL_ENABLED=true`),
built in `fastmock-server/src/mail_setup.rs`; the maintenance loop calls `MailService::sweep` every minute.
Permissions `mail:read`, `mail:write`, `mail:delete` (space-scoped) and the route rules for
`/__admin/mail/...` are in `access.rs`; spaces carry `mailDomains`.

## Model decisions

- A message routed to several spaces is stored once per space (own UUIDv7 each), `envelope.rcptTo` listing
  only that space's recipients. Recipients of no space go together to the AUTH user's space (when a space
  has that name) or the global area.
- `received_at` has microsecond precision (`micros`), so cursors `(received_at, id)` compare alike in
  memory and PostgreSQL. Lists are newest first unless `oldest_first`.
- Mailboxes belong to an area (space or global) and a mailbox address (`mailbox_of`: lowercase, without
  `+tag`); folders hold entries with UIDs. `INBOX` is created on first delivery; `UIDVALIDITY` is set when a
  folder is created and grows if a folder is deleted and created again; `UIDNEXT` only grows.
- Expunging an entry (IMAP EXPUNGE, POP3 DELE) removes it from that folder only; the message stays in the
  admin API. Deleting a message (admin API, retention) removes all its folder entries.
- Labels are lowercase, sorted, at most 32; the summary is immutable, labels are not.

## Seams for the next phases

- **B1 SMTP** (`src/smtp/`): build `Incoming::new(MailSource::Smtp, envelope, raw)` with the masked
  transcript and call `MailService::accept`; `AcceptError::TooLarge` maps to 552. Limits, timeouts, AUTH
  users and hostname come from `MailConfig`; TLS from `tls::server_config`. Add `smtp-proto =0.2.5` and
  `tokio-rustls` to this crate's manifest. Chaos hooks: rules per space read through a trait the server
  implements (no HTTP types here). Start the listeners from `fastmock-server/src/lib.rs` `serve`.
- **B2 admin API** (`fastmock-server/src/mail_api.rs`, already merged into the admin router): use
  `spaces::select`/`target` with `Permission::Mail*`; `parse_query` for `query`, `MailCursor::encode/decode`
  for `cursor`; `ParsedMail::detail` for `GET /{id}`; `html_with_inline_parts` for `/html`;
  `extract::links`/`CodePattern` for `/links`, `/codes`; `compose` + `Destination::Area` for inject;
  `normalize_labels` + `MailStore::set_labels`; `MailService::publish_deleted` after deletes;
  `events.subscribe(SpaceFilter)` for SSE and `wait` (on `Lagged`, re-list from the store);
  `CompiledMailMatch` + `MatchView` for wait/verify. Per-space chaos and webhook settings can live in
  `Stores::data`. Regenerate `docs/api/openapi.json` (`UPDATE_OPENAPI=1 cargo test -p fastmock-server
  openapi_spec_is_current`) and `ui/src/api/schema.d.ts` (`npm run gen:api`).
- **B3 PostgreSQL + Valkey** (`fastmock-store`): implement `MailStore` (table `mail_messages`, mailbox and
  folder tables with `uid_validity`/`uid_next` per folder, entries `(folder, uid, message_id, flags)`,
  password hash per mailbox; `sweep` under an advisory lock) and pass
  `fastmock_core::mail::conformance::{messages, mailboxes, retention}`. Implement `MailEvents` with Valkey
  pub/sub, a channel per area (`<prefix>mail:events:<space>`, `_global` for the global area; space names
  cannot contain `_`): `publish` sends JSON of `MailEvent`, one listener task per process `PSUBSCRIBE`s and
  calls `MailEventHub::deliver`, `subscribe` reads from that hub. Select them in
  `fastmock-server/src/mail_setup.rs` `backends` (the `Postgres` arm).
- **C1 IMAP / C2 POP3**: mailbox state through `MailStore::mailbox_*` (sequence numbers are positions in UID
  order); IDLE through `events.subscribe` (`MailEvent::mailboxes` lists the INBOX addresses that got a
  message); passwords through `mailbox_password`/`set_mailbox_password` (hash chosen by C2); APPEND stores
  with `accept` (`MailSource::Inject`, `Destination::Area`, `mailboxes` override).
- **C3 providers**: `compose` for JSON payloads, `Incoming { source: Provider, provider, provider_extras }`,
  `Destination::Area` for `@<space>` URLs, else `Destination::Route`.
- **C4 UI**: `features` of `/__admin/version` lists `mail`, `mail.imap`, `mail.pop3`, `mail.providers`;
  `/__admin/spaces` lists `mailDomains`; permission names are in `ui/src/auth/permissions.ts`.
