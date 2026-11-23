<div class="fm-hero" markdown>

<p class="fm-hero__eyebrow">FastMock by FransHub</p>

# FastMock documentation

FastMock is an API mock server for test environments: HTTP, HTTPS with mTLS, gRPC, GraphQL, SOAP, WebSocket,
Server-Sent Events, Kafka and AsyncAPI, plus mail capture over SMTP and provider APIs, with durable signed
callbacks, shared state for any number of replicas, security by default and full observability. Start with [Getting started](guide/getting-started.md), then pick the
section for what you do.

<div class="fm-hero__actions" markdown>
[Get started :material-arrow-right:](guide/getting-started.md){ .md-button .md-button--primary }
[Concepts](guide/concepts.md){ .md-button }
[Admin API reference](api/swagger/index.html){ .md-button }
</div>

</div>

## Getting started

<div class="grid cards fm-cards" markdown>

-   :material-rocket-launch-outline:{ .fm-card__icon } **[Getting started](guide/getting-started.md)**

    Run FastMock with Docker, Docker Compose, Helm or the operator; create a token, first stub, journal, UI,
    callback and scenario in ten minutes.

-   :material-lightbulb-on-outline:{ .fm-card__icon } **[Concepts](guide/concepts.md)**

    Data plane and control plane, stubs, matching order, templates, state, journal, callbacks outbox, plugins,
    storage modes, replicas.

-   :material-monitor-dashboard:{ .fm-card__icon } **[Web UI](guide/ui.md)**

    The admin UI at `/ui` and the Swagger UI for the admin API.

</div>

## Writing mocks

<div class="grid cards fm-cards" markdown>

-   :material-target:{ .fm-card__icon } **[Request matching](guide/request-matching.md)**

    Every request condition: method, URL and path templates, query, headers, cookies, body matchers (JSON, XML,
    schema, multipart, form), logical operators, priorities, limits.

-   :material-reply-outline:{ .fm-card__icon } **[Responses](guide/responses.md)**

    Status, headers, bodies and body files, delays, dribbling, faults, response sequences, proxying.

-   :material-code-braces:{ .fm-card__icon } **[Templating](guide/templating.md)**

    Handlebars context and every helper.

-   :material-history:{ .fm-card__icon } **[Stub lifecycle](guide/stub-lifecycle.md)**

    Ids, tags, metadata, enabling, `times`, expiry, counters, export.

-   :material-state-machine:{ .fm-card__icon } **[Scenarios and state](guide/scenarios-and-state.md)**

    State machines, templated scenario names, TTLs, the key-value store.

-   :material-webhook:{ .fm-card__icon } **[Callbacks](guide/callbacks.md)**

    Webhooks after a response: delays, retries, HMAC signatures, correlation, egress policy.

-   :material-shuffle-variant:{ .fm-card__icon } **[Proxying and chaos rules](guide/proxy-and-chaos.md)**

    Forward to real services; inject delays, errors and faults at random.

-   :material-broadcast:{ .fm-card__icon } **[Server-Sent Events](guide/sse.md)**

    Event streams with per-event delays.

-   :material-swap-horizontal:{ .fm-card__icon } **[WebSocket](guide/websocket.md)**

    Scripted WebSocket sessions.

-   :material-lan-connect:{ .fm-card__icon } **[gRPC](guide/grpc.md)**

    Unary and streaming gRPC and gRPC-Web from uploaded descriptor sets, with server reflection.

-   :material-graphql:{ .fm-card__icon } **[GraphQL](guide/graphql.md)**

    Matching GraphQL operations, schema import.

-   :material-xml:{ .fm-card__icon } **[SOAP](guide/soap.md)**

    SOAP 1.1/1.2 stubs, WSDL and SoapUI import.

-   :material-message-processing-outline:{ .fm-card__icon } **[Kafka messages](guide/kafka.md)**

    Publishing JSON or Avro records from stubs, Schema Registry, topic allow-list.

-   :material-access-point-network:{ .fm-card__icon } **[MQTT and AMQP messages](guide/mqtt-amqp.md)**

    Publishing to MQTT 3.1.1/5 topics and AMQP 0-9-1 exchanges from stubs and async mocks, TLS, allow-lists.

-   :material-message-arrow-right-outline:{ .fm-card__icon } **[AsyncAPI and async mocks](guide/asyncapi.md)**

    Mocks that keep publishing messages from AsyncAPI documents.

-   :material-robot-outline:{ .fm-card__icon } **[Mock MCP servers](guide/mock-mcp-servers.md)**

    Fake MCP tools and resources for testing AI agents, with argument matching, errors, delays and verification.

</div>

## Importing and migrating

<div class="grid cards fm-cards" markdown>

-   :material-import:{ .fm-card__icon } **[Importing](guide/importing.md)**

    OpenAPI, Postman, HAR and auto-detection; common import options.

-   :material-swap-horizontal-bold:{ .fm-card__icon } **[WireMock and MockServer import](guide/import-wiremock-mockserver.md)**

    Field-by-field conversion of WireMock mappings and MockServer expectations.

-   :material-package-variant-closed:{ .fm-card__icon } **[Microcks artifacts](guide/microcks-artifacts.md)**

    OpenAPI with Microcks examples, APIExamples/APIMetadata, SoapUI, Postman, GraphQL, gRPC artifacts.

-   :material-book-open-variant:{ .fm-card__icon } **[Service catalog](guide/service-catalog.md)**

    The catalog of imported services and their operations.

-   :material-record-rec:{ .fm-card__icon } **[Recording](guide/recording.md)**

    Recording real traffic and snapshots of the journal into stubs.

-   :material-transfer-right:{ .fm-card__icon } **[Migrating from WireMock](guide/migrating-from-wiremock.md)**

    Import, behaviour differences, test code changes.

-   :material-transfer-right:{ .fm-card__icon } **[Migrating from MockServer](guide/migrating-from-mockserver.md)**

    Import, behaviour differences, test code changes.

-   :material-transfer-right:{ .fm-card__icon } **[Migrating from Microcks](guide/migrating-from-microcks.md)**

    Moving a Microcks installation's mocks to FastMock.

</div>

## Testing with FastMock

<div class="grid cards fm-cards" markdown>

-   :material-check-decagram-outline:{ .fm-card__icon } **[Verification](guide/verification.md)**

    Query the journal, verify counts and sequences, near misses.

-   :material-email-outline:{ .fm-card__icon } **[Mail](guide/mail.md)**

    Capture email over SMTP and provider APIs (SES, SendGrid, Mailgun, Postmark, Resend, Brevo), wait for and verify it in tests, read links and codes, IMAP and POP3 mailboxes, mail chaos, provider webhooks.

-   :material-inbox-arrow-down-outline:{ .fm-card__icon } **[Message capture](guide/message-capture.md)**

    Store what the application publishes to Kafka, MQTT and AMQP, then find, wait for and verify it in tests.

-   :material-code-tags:{ .fm-card__icon } **[Java client](guide/java-client.md)**

    The Java 25 client: every method, JUnit 5 patterns, Testcontainers.

-   :material-file-document-check-outline:{ .fm-card__icon } **[Contract testing](guide/contract-testing.md)**

    Testing a real service against its OpenAPI or AsyncAPI contract.

-   :material-api:{ .fm-card__icon } **[Admin API](guide/admin-api.md)**

    Authentication, roles per endpoint, errors, pagination, every endpoint group.

-   :material-robot-outline:{ .fm-card__icon } **[MCP for coding agents](guide/mcp.md)**

    The `/mcp` endpoint: GitHub Copilot, Claude Code and Cursor create stubs, search the journal and verify calls
    with your token's permissions.

-   :material-application-brackets-outline:{ .fm-card__icon } **[Admin API reference](api/swagger/index.html)**

    Interactive Swagger UI for the admin API.

-   :material-code-json:{ .fm-card__icon } **[OpenAPI description](api/openapi.json)**

    Machine-readable description of the admin API (also served at `/openapi.json`).

</div>

## Operating FastMock

<div class="grid cards fm-cards" markdown>

-   :material-tune-variant:{ .fm-card__icon } **[Configuration](operations/configuration.md)**

    Every environment variable with type, default and effect.

-   :material-ship-wheel:{ .fm-card__icon } **[Helm chart](operations/helm.md)**

    Install, upgrade, every value, production checklist, Gateway API, NetworkPolicy.

-   :material-cog-sync-outline:{ .fm-card__icon } **[Operator](operations/operator.md)**

    `FastMock` and `MockApi` resources, status conditions, policies, RBAC, CRD upgrades.

-   :material-shield-lock-outline:{ .fm-card__icon } **[Security](operations/security.md)**

    Tokens and roles, origins, TLS, storage connections, egress, secrets, redaction, limits, audit, hardening
    checklist.

-   :material-account-key-outline:{ .fm-card__icon } **[Access control](operations/access-control.md)**

    Permissions, built-in and custom roles, spaces for several teams, bindings, managed tokens, OIDC sign-in.

-   :material-earth:{ .fm-card__icon } **[Several clusters](operations/multi-cluster.md)**

    Running FastMock in several clusters on shared PostgreSQL and Valkey: what is shared, requirements, limits.

-   :material-database-outline:{ .fm-card__icon } **[Database](operations/database.md)**

    How migrations run, running them as a separate Job, migration policy, supported PostgreSQL and Valkey
    versions, PostgreSQL major upgrades.

-   :material-chart-timeline-variant:{ .fm-card__icon } **[Observability](operations/observability.md)**

    Every metric, PromQL examples, alerts, Grafana dashboard, tracing, logs, health checks.

-   :material-stethoscope:{ .fm-card__icon } **[Troubleshooting](operations/troubleshooting.md)**

    Symptom, cause and fix for the common problems.

-   :material-file-code-outline:{ .fm-card__icon } **[Docker Compose example](examples/docker-compose.yaml)**

    FastMock with PostgreSQL 18 and Valkey 9.

</div>

## Reference

<div class="grid cards fm-cards" markdown>

-   :material-puzzle-outline:{ .fm-card__icon } **[Writing plugins](guide/plugins.md)**

    Implement a provider in Rust and build a binary that includes it.

-   :material-shield-bug-outline:{ .fm-card__icon } **[Security policy](https://github.com/frans-hub-io/fastmock/blob/main/SECURITY.md)**

    How to report a vulnerability.

</div>

## Conventions in these pages

- The data plane is `http://localhost:8080` and the admin API `http://localhost:9090`; in Kubernetes they are the
  `<release>` and `<release>-admin` Services.
- `FASTMOCK_TOKEN` holds an admin token (`fastmock hash-token --generate`); calls that change something need the
  `editor` role, a few need `admin`.
- Example host names use `example.com`, example addresses `198.51.100.0/24` and `203.0.113.0/24`.
- Outputs shown on these pages come from real runs of FastMock 0.1.0-dev15, shortened where marked.
