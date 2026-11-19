# Several clusters, one FastMock

FastMock keeps no state that matters in its pods. Stubs, files, settings, the request journal, the callbacks
outbox and the audit log live in PostgreSQL; counters (`times`, response sequences), scenario states, the
key-value store and the async-mock leader lease live in Valkey. Replicas coordinate only through these two
services, never through Kubernetes. Any number of Deployments in any number of clusters that reach the same
PostgreSQL database and the same Valkey (with the same `FASTMOCK_VALKEY_PREFIX`) therefore behave as one
FastMock instance.

- [Typical layout: a FastMock in every cluster](#typical-layout-a-fastmock-in-every-cluster)
- [What is shared and how](#what-is-shared-and-how)
- [Requirements](#requirements)
- [Limits](#limits)
- [Separate instances on shared servers](#separate-instances-on-shared-servers)
- [Verified behaviour](#verified-behaviour)

## Typical layout: a FastMock in every cluster

When the system under test runs in several clusters, install the chart in every cluster with the same values, so
that each cluster has its own FastMock Service under the same name. Applications keep calling a cluster-local
address such as `http://fastmock.mocks.svc:8080` and never leave their cluster for a mock; only FastMock talks to
the shared PostgreSQL and Valkey. A message sent through the mock in one cluster can then be followed up (a
delivery report, a status poll, a callback) through the mock in the other cluster.

Give callbacks targets that are valid in every cluster, for example the cluster-local name of the receiving
service, which exists in both clusters when the same releases run there.

## What is shared and how

| Concern | Mechanism | Effect across clusters |
|---|---|---|
| Stub changes | PostgreSQL `LISTEN/NOTIFY` on every change, plus a revision check every `FASTMOCK_STUB_POLL` (5 s) | A stub saved through any admin API is served everywhere at once, at the latest after one poll interval |
| `times`, response sequences | atomic counters in Valkey | A stub with `times: 3` answers three times in total, whichever pods receive the calls |
| Scenarios, key-value store | Valkey | A state change in one cluster is seen by the next request in another |
| Callbacks | outbox rows claimed with `FOR UPDATE SKIP LOCKED`, idempotency key per delivery | Each callback is attempted by exactly one pod at a time; retries continue elsewhere if a pod or cluster disappears |
| Async mocks publishing to Kafka | leader lease in Valkey (15 s TTL) | One pod in all clusters publishes; another takes over when the lease expires |
| WebSocket mocks | stubs with `response.websocket`, served by the pod that holds the connection | Work in every cluster; each session is local to its pod |
| Journal, verification, audit | PostgreSQL | `verify` and request searches see the traffic of every cluster |
| Migrations | PostgreSQL advisory lock | Pods starting at the same time in different clusters migrate once |

## Requirements

- **Same database and prefix.** Every cluster points at the same PostgreSQL database and Valkey, and uses the
  same `storage.valkey.prefix`.
- **Same secrets.** Admin token hashes, `FASTMOCK_SECRETS_DIR` contents (HMAC keys used by callbacks), stub and
  contract secret allow-lists must be identical; otherwise a stub that signs callbacks works in one cluster and
  fails in another.
- **Same version.** Run the same FastMock version in all clusters, except during a rolling upgrade (migrations
  are additive, so old pods keep working while new ones start).
- **Connections to PostgreSQL.** Every pod connects directly with its own pool (`storage.database.maxConnections`,
  default 10) plus one connection for `LISTEN`; no pooler is needed. Keep `clusters × replicas ×
  (maxConnections + 1)` well below the server's `max_connections`. If a pooler such as PgBouncer is put in between,
  it must run in session mode: in transaction mode notifications are lost and changes arrive only by polling.
- **Reachability and latency.** Every request that touches counters or scenarios makes a Valkey round trip, so
  keep the clusters close to the databases. Use encrypted, authenticated connections across networks (see
  [Helm chart: Secure connections](helm.md#secure-connections)).
- **Egress per cluster.** Callback targets are resolved and checked by the egress policy of the pod that delivers
  them; any cluster may deliver any callback, so every cluster must be allowed to reach the targets.

The databases installed by the chart (`postgresql.enabled`, `valkey.enabled`, `cnpg.enabled`) are only reachable
inside their own cluster and, with `networkPolicy.storage`, only from FastMock pods of the same release. For
several clusters use external PostgreSQL and Valkey and set `storage.database.existingSecret` and
`storage.valkey.existingSecret` in every release.

## Limits

- **One PostgreSQL and one Valkey for all clusters.** FastMock does not replicate between clusters by itself.
  When the databases are unreachable, every cluster stops serving (readiness fails). High availability comes from
  the databases: a CloudNativePG cluster with replicas, a managed PostgreSQL, Valkey with replicas or Sentinel.
- **Latency on stateful requests.** Matching uses the stubs held in memory and costs nothing extra, and the journal
  is written in the background. Requests to stubs with `times`, response sequences, scenarios or the key-value
  store wait for one Valkey round trip, and stubs with callbacks for one PostgreSQL insert. A cluster far from the
  databases adds that distance to these requests.
- **Stub changes are not synchronous.** A stub saved in one cluster reaches the others through `LISTEN/NOTIFY`
  within milliseconds, or after up to `FASTMOCK_STUB_POLL` without notifications. A test that creates stubs
  through one cluster and calls another right away can race; call the same cluster or wait for the stub.
- **Shared journal.** `verify`, request searches and unmatched requests include the traffic of every cluster.
  Tests running in parallel in different clusters must filter by their own data (a header, an id) or use separate
  instances.
- **Callbacks leave from any cluster.** Receivers see different source addresses and must be reachable, and
  allowed by the egress policy, from every cluster. Delivery is at least once; use the `x-fastmock-delivery-id`
  header to drop duplicates after a pod or cluster fails mid-delivery.
- **Kafka async mocks publish from one pod.** That pod can be in any cluster, so the Kafka brokers must be
  reachable from all of them.
- **Clocks.** Stub expiry (`expiresAt`, `ttlSeconds`) and callback due times use the clock of the pod that
  evaluates them. Keep the nodes in sync (NTP); skew shifts these times by the same amount.
- **One writer for declarative stubs.** If the operator runs in several clusters with the same `MockApi`, each
  copy imports the same tag into the shared database. Manage stubs from one place (one cluster's operator, a CI
  job or the admin API), not from every cluster.
- **Configuration is not shared.** Tokens, HMAC secrets, egress rules, limits and enabled plugins come from each
  release's values; keep them identical, for example from one values file.
- **Memory mode cannot be shared.** `storage.mode: memory` keeps everything in the pod.

## Separate instances on shared servers

To run independent FastMock instances on the same servers instead, give each its own PostgreSQL database (or
server) and its own `storage.valkey.prefix`. Instances that share a database also share stubs and journal, a
shared prefix makes them share counters and scenarios.

## Verified behaviour

Two releases with separate Deployments and Services, two replicas each, connected to one PostgreSQL and one
Valkey:

| Check | Result |
|---|---|
| Stub created through the admin API of release A, requested through release B | served at once |
| Stub with `times: 3`, six requests alternating between A and B | `200 200 200`, then no match |
| Scenario moved to a new state through A, read through B | B sees the new state |
| Callback scheduled by a request to B | delivered once, although four pods poll the outbox |
