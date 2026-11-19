# Database

With `FASTMOCK_STORAGE=postgres` (the default) FastMock keeps its durable data in PostgreSQL and short-lived shared
state in Valkey. This page covers how the schema is migrated, which server versions are supported and how to
upgrade PostgreSQL to a new major version. Connection settings and TLS are in
[Configuration](configuration.md#listeners-and-storage) and [Security](security.md#storage-connections).

- [What is stored where](#what-is-stored-where)
- [How migrations run](#how-migrations-run)
- [Running migrations separately](#running-migrations-separately)
- [Writing migrations](#writing-migrations)
- [Supported versions](#supported-versions)
- [PostgreSQL major upgrades](#postgresql-major-upgrades)

## What is stored where

| Store | Table or keys | Content |
|---|---|---|
| PostgreSQL | `fm_stubs`, `fm_revision` | Stubs and the revision counter that tells replicas to reload them. |
| PostgreSQL | `fm_data` | Body files, settings, gRPC descriptors, async mocks, the service catalog, specifications and contract-test runs. |
| PostgreSQL | `fm_journal` | Request journal (kept for `FASTMOCK_JOURNAL_RETENTION`). |
| PostgreSQL | `fm_outbox` | Callbacks waiting for delivery and their results. |
| PostgreSQL | `fm_audit` | Audit log of admin actions. |
| PostgreSQL | `_sqlx_migrations` | Applied migrations: version, description, checksum, success. |
| Valkey | keys under `FASTMOCK_VALKEY_PREFIX` | Scenario states, counters, delayed deliveries and caches. |

## How migrations run

Migrations are SQL files compiled into the binary (`crates/fastmock-store/migrations`). With
`FASTMOCK_DATABASE_MIGRATE=auto` (the default) every replica applies them at start-up, before it opens its ports:

1. It takes a PostgreSQL session advisory lock (`pg_advisory_lock`, the key is derived from the database name),
   so when several replicas start together only one migrates; the others wait for the lock and then find
   nothing to do.
2. It reads `_sqlx_migrations`. A migration recorded as failed stops start-up; so does an applied migration whose
   checksum differs from the file in the binary (an edited migration).
3. It applies each pending migration in its own transaction together with its `_sqlx_migrations` row, so a failed
   migration leaves nothing behind.
4. It releases the lock. If the process dies, PostgreSQL releases the lock with the session.

`FASTMOCK_DATABASE_MIGRATION_TIMEOUT` (default `120s`) limits the whole step, including the wait for the lock. A
replica that runs out of time exits and is restarted; the replica holding the lock carries on. With the Helm
chart the startup probe allows about two minutes in total, so run long migrations as a separate Job.

Applied migrations that the binary does not know (written by a newer FastMock) are accepted. During a rolling
upgrade the first new pod migrates and the old pods keep working; an old pod that restarts, or a rollback to the
previous image, starts against the newer schema. This relies on the [migration policy](#writing-migrations).

In `auto` mode the database user needs `CREATE` on the schema even when nothing is pending, because the
migration table is created with `create table if not exists`.

## Running migrations separately

To keep DDL rights away from the server, or to migrate once per release instead of in every pod:

1. Run `fastmock migrate` with the new image and a user that owns the schema. It reads only the
   `FASTMOCK_DATABASE_*` variables (URL, password, TLS settings, `FASTMOCK_DATABASE_MIGRATION_TIMEOUT`) and exits
   with status 0 once the schema is current.
2. Start the server with `FASTMOCK_DATABASE_MIGRATE=skip`. It applies nothing and checks at start-up that every
   migration it needs is applied and unchanged; otherwise it stops with a message such as
   `the database schema is older than this FastMock version expects (version 4): migration(s) 4 (admin queries)
   not applied; run fastmock migrate from this version before starting it with FASTMOCK_DATABASE_MIGRATE=skip`.
   The server user then needs only `USAGE` on the schema and `SELECT`, `INSERT`, `UPDATE`, `DELETE` on its tables.

A Kubernetes Job for step 1, run before the Deployment is updated (for example as a pipeline step or a Helm
`pre-upgrade` hook):

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: fastmock-migrate-1-2-3
spec:
  backoffLimit: 2
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: migrate
          image: registry.example.com/fastmock:1.2.3
          args: ["migrate"]
          env:
            - name: FASTMOCK_DATABASE_URL
              value: postgres://fastmock_owner@db.example.com:5432/fastmock
            - name: FASTMOCK_DATABASE_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: fastmock-db-owner
                  key: password
            - name: FASTMOCK_DATABASE_SSL_MODE
              value: verify-full
            - name: FASTMOCK_DATABASE_SSL_ROOT_CERT
              value: /etc/fastmock/db-ca/ca.crt
          volumeMounts:
            - name: db-ca
              mountPath: /etc/fastmock/db-ca
              readOnly: true
      volumes:
        - name: db-ca
          secret:
            secretName: fastmock-db-ca
```

With the Helm chart, add `FASTMOCK_DATABASE_MIGRATE=skip` through `extraEnv`; the chart does not create the Job.

## Writing migrations

FastMock runs old and new versions side by side during a rolling upgrade, so every migration must work with the
previous release still running:

- **Never edit, rename or delete a migration that has been released.** The checksum check stops every replica
  (and `fastmock migrate`) against a database where it was applied. Fix mistakes with a new migration.
- **Additive changes only** in one release: new tables, new indexes, new nullable columns or columns with a
  constant default. Code must tolerate rows written by the previous release.
- **Expand and contract** for anything destructive. To rename or drop a column, or to tighten a constraint: add
  the new structure and write both (release N), switch reads (release N+1), then drop the old structure in a
  later release, once no supported version reads it.
- **Large indexes:** `create index concurrently` cannot run inside a transaction. Put it alone in its own file
  whose first line is `-- no-transaction`; sqlx then runs that file outside a transaction. Such a migration is
  not atomic, so use `if not exists`, and if it fails, drop the invalid index before running it again.
- Files are named `<version>_<description>.sql` with a new, higher version; gaps in the numbering are allowed.
- Use only features of the oldest [supported PostgreSQL version](#supported-versions).

## Supported versions

FastMock is tested with PostgreSQL 18 and Valkey 9.

### PostgreSQL

The schema and queries need **PostgreSQL 11 or later**; use a major version that the PostgreSQL project still
supports (see its [versioning policy](https://www.postgresql.org/support/versioning/)). The newest feature in use:

| Feature | Where | Since |
|---|---|---|
| `starts_with(text, text)` | journal path filter, audit action prefix filter | 11 |
| `generated always as identity` | `fm_journal.id`, `fm_audit.id` | 10 |
| `insert ... on conflict`, `for update skip locked`, `jsonb - text`, `create index if not exists` | stubs and data upserts, callback claiming, listing data without a field, migration `0004` | 9.5 |
| `LISTEN`/`NOTIFY`, `gin` index on `text[]`, partial indexes, `jsonb` | stub reload, tag queries, journal and outbox indexes | 9.4 |

`starts_with` is the only reason for 11; every other construct works on 10. No extension is required.

### Valkey and Redis

FastMock connects to one endpoint: a standalone server, or a managed endpoint that always points to the
primary. Valkey Cluster mode is not supported. Commands used:

| Command | Purpose | Since (Redis) |
|---|---|---|
| `GET`, `SET`, `MGET`, `DEL`, `PING`, `SELECT` | key-value state, health check, database in the URL | 1.0 |
| `PSETEX`, `SET ... PX` (inside a script) | values with a time to live | 2.6 / 2.6.12 |
| `INCRBY`, `PEXPIRE` (inside a script) | counters with a time to live | 1.0 / 2.6 |
| `ZADD`, `ZRANGEBYSCORE ... LIMIT`, `ZREM` with several members | delayed-delivery queues | 2.4 |
| `EVALSHA`, `SCRIPT LOAD` | atomic compare-and-set, counter and queue pop (Lua) | 2.6 |
| `SCAN ... MATCH ... COUNT` | listing keys by prefix | 2.8 |
| `UNLINK` | deleting keys by prefix | 4.0 |
| `AUTH <user> <password>` | ACL user (`FASTMOCK_VALKEY_USERNAME`) | 6.0 |
| TLS (`rediss://`) | encrypted connections | 6.0 |

`GETEX`, `HEXPIRE`, `SET ... GET`, Redis Functions and modules are not used. `CLIENT SETINFO` (7.2) is sent by
the client library at connect time; errors from it are ignored, so it needs no ACL permission.

Minimum versions: **Redis 4.0** without TLS and ACL users, **Redis 6.0** with TLS or an ACL user (ACL rules
also apply to the commands the Lua scripts call). **Every Valkey release** (7.2 and later) supports everything
FastMock uses.

## PostgreSQL major upgrades

FastMock needs nothing special for a major upgrade: no extensions, and the schema is the same on every supported
version. After the upgrade FastMock starts as usual; its migrations find nothing to do.

What to carry over: stubs, body files and settings matter; the journal, callbacks and audit log are history that
you may accept to lose. Stubs and body files can also be moved through the admin API, independently of the
database: `GET /__admin/export` (with an editor or admin token, so credentials are not masked) and
`POST /__admin/stubs/import` for stubs, `GET` and `PUT /__admin/files/{name}` for body files. Settings, gRPC
descriptors, async mocks, the service catalog, the journal, callbacks and the audit log are not moved that way.

**CloudNativePG** (1.26 and later) upgrades a cluster in place, offline: change the major version in
`imageName` (or the image catalog), using an image of the same operating system family. The operator stops all
instances, runs `pg_upgrade` in a Job, then clones the replicas again; the database is unavailable for the whole
operation, so scale FastMock to zero first or accept failing requests. Take a backup before and a new base backup
after: the upgraded cluster has a new system identifier and point-in-time recovery does not cross the upgrade.
If the upgrade fails, setting the old image back restarts the cluster on the old version. Run `ANALYZE` afterwards,
because `pg_upgrade` does not carry planner statistics over. See the
[CloudNativePG documentation](https://cloudnative-pg.io/documentation/current/postgres_upgrades/).

**A single PostgreSQL instance** (for example the one the Helm chart can install) cannot be upgraded in place
by changing the image: the data directory of one major version does not start on another. Dump and restore:

```sh
kubectl -n mocks scale deploy/fastmock --replicas=0
pg_dump -Fc -h db-old.example.com -U fastmock -d fastmock -f fastmock.dump
pg_restore -h db-new.example.com -U fastmock -d fastmock --no-owner fastmock.dump
kubectl -n mocks scale deploy/fastmock --replicas=2
```

Point `FASTMOCK_DATABASE_URL` at the new server before scaling up. Use `pg_dump` from the newer major version.
For test environments where history does not matter, exporting stubs and body files, starting on an empty new
database and importing them is the shorter path.
