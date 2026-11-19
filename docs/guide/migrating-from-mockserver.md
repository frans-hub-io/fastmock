# Migrating from MockServer

FastMock imports MockServer 5.x expectations (the JSON of `PUT /mockserver/expectation` and of initialization
files), converts matchers, responses, forwards and simple Mustache templates, and reports what it cannot convert.
This page describes the migration process; the field-by-field conversion is on
[WireMock and MockServer import](import-wiremock-mockserver.md#mockserver).

- [1. Export and import the expectations](#1-export-and-import-the-expectations)
- [2. What behaves differently](#2-what-behaves-differently)
- [3. Change the test code](#3-change-the-test-code)
- [4. Replace the MockServer deployment](#4-replace-the-mockserver-deployment)
- [Checklist](#checklist)

## 1. Export and import the expectations

From a running MockServer:

```sh
curl -s -X PUT "http://mockserver.example.com:1080/mockserver/retrieve?type=ACTIVE_EXPECTATIONS&format=JSON" \
  > expectations.json
```

or use the initialization file (`mockserver.initializationJsonPath`) directly. Convert without saving first:

```sh
curl -s -X POST "http://localhost:9090/__admin/import/mockserver?tag=legacy&dryRun=true" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d @expectations.json | jq '{stubs: (.stubs | length), warnings}'
```

```json
{
  "stubs": 41,
  "warnings": ["expectation #2: JAVASCRIPT response templates are not executed; the expectation was skipped"]
}
```

Review the warnings, rewrite skipped expectations as FastMock stubs (below), then import for real by dropping
`dryRun=true`. The import replaces earlier stubs with the same tag, so it can be repeated. Every imported stub
carries `metadata.source = "mockserver"`, which makes the converted set easy to find later
(`POST /__admin/stubs/find-by-metadata`).

## 2. What behaves differently

| Topic | MockServer | FastMock | What to do |
|---|---|---|---|
| Ports | One port (1080) for mocks and `/mockserver/*` control | Mocks on 8080, admin API on 9090 | Point control clients to the admin port. |
| Control API authentication | Optional (mTLS or JWT) | Always on, bearer tokens with roles | Create tokens ([Security](../operations/security.md#admin-tokens-and-roles)). |
| Priority | Higher number first | Lower number first | Converted on import (`5 - priority`); keep it in mind for new stubs. |
| Equal priority | The expectation created first wins | Native stubs: the more specific URL, then the newest | Imported expectations keep MockServer's rule through an `order` field: the first of the document, and earlier imports, win ([Stub lifecycle](stub-lifecycle.md#equal-priorities-in-imported-stubs)). |
| `keyMatchStyle` | `SUB_SET` (default) or `MATCHING_KEY` | `includes` or `allValues` | Converted. |
| JSON and XML bodies | `ONLY_MATCHING_FIELDS` ignores extra fields, extra array elements and array order; XML ignores child order | Same through `ignoreExtraElements`, `ignoreExtraArrayItems`, `ignoreArrayOrder` and `CHILD_NODELIST_SEQUENCE` | Converted. |
| Several header values, several cookies | One header line per value | Same (header values can be arrays) | Converted. |
| Empty imports | | A document that yields no stub (only Velocity templates, wrong structure) answers `400` and changes nothing | Read the error; fix or rewrite the expectations. |
| `times` | Per MockServer instance | Counted across all replicas | Nothing; converted on import. |
| `timeToLive` | Expectation expires | Stub `ttlSeconds` | Converted. |
| Templates | Mustache, Velocity, JavaScript | Handlebars only, no code execution | Simple Mustache is converted; rewrite the rest with [templates](templating.md), [scenarios](scenarios-and-state.md) and response sequences ([Responses](responses.md#response-sequences)). |
| Class and object callbacks | Response computed by code in the test JVM | Not supported | Model the behaviour declaratively: matchers per case, templates for echoing request data, scenarios for state. |
| Asynchronous side effects | Not built in | Durable [callbacks](callbacks.md) and [Kafka messages](kafka.md) | New capability; callbacks to `localhost` are blocked by default. |
| `httpForward`, `httpOverrideForwardedRequest` | Forward with optional overrides | `response.proxy` (forward as received, add or remove headers) | Converted; overrides of path, method or body are dropped with a warning. |
| Proxy and recording mode | Proxy with recorded expectations | [Recording](recording.md) and snapshots | |
| Verification | `PUT /mockserver/verify`, `/verifySequence`, `/retrieve` | `POST /__admin/verify`, `/verify/sequence`, `/requests/find` | See the [mapping table](verification.md#coming-from-wiremock-or-mockserver). |
| Clearing | `PUT /mockserver/clear`, `/reset` | `DELETE /__admin/stubs?tag=`, `POST /__admin/requests/remove`, `POST /__admin/reset` | Prefer per-test tags. |
| TLS | Generates a CA and certificates on the fly | PEM files (`FASTMOCK_TLS_*`), optional mTLS with fingerprint matching | [Configuration](../operations/configuration.md#data-plane-tls-and-mtls). |
| Request log | Synchronous | Asynchronous, a few milliseconds after the response | Wait (`awaitRequests`) instead of reading once. |
| State | In memory, per instance | PostgreSQL and Valkey, shared by replicas | Tag stubs per test. |

## 3. Change the test code

### Java

```java
// MockServer
MockServerClient client = new MockServerClient("localhost", 1080);
client.when(request().withMethod("POST").withPath("/orders/.*")
                .withBody(json("{\"amount\": 250}", MatchType.ONLY_MATCHING_FIELDS)),
            Times.exactly(1))
      .respond(response().withStatusCode(201).withBody("{\"status\":\"CREATED\"}"));
// ...
client.verify(request().withMethod("POST").withPath("/orders/.*"), VerificationTimes.exactly(1));
client.reset();
```

```java
// FastMock
FastMock fastmock = FastMock.connect("http://localhost:9090", System.getenv("FASTMOCK_TOKEN"));
ObjectMapper json = new ObjectMapper();
String tag = "orders-test-" + UUID.randomUUID();
fastmock.createStub(json.readTree("""
        {"tags": ["%s"],
         "times": 1,
         "request": {"method": "POST", "urlPathPattern": "/orders/.*",
                     "body": [{"equalToJson": {"amount": 250}, "ignoreExtraElements": true, "ignoreArrayOrder": true}]},
         "response": {"status": 201, "jsonBody": {"status": "CREATED"}}}
        """.formatted(tag)));
// ...
fastmock.verifyExactly(json.readTree("""
        {"method": "POST", "urlPathPattern": "/orders/.*"}"""), 1);
fastmock.deleteStubsByTag(tag);
```

The fluent [`Stub` builder](java-client.md#stub-builder) covers common cases; raw JSON reaches every stub field.

### Expectations you keep as JSON

If your tests send MockServer expectation JSON, send the same JSON to `POST /__admin/import/mockserver?tag=<test
tag>` instead of `PUT /mockserver/expectation`. The conversion is the same as for the bulk import.

### Raw HTTP

| MockServer call | FastMock call |
|---|---|
| `PUT /mockserver/expectation` | `POST /__admin/import/mockserver` (MockServer JSON) or `POST /__admin/stubs` (FastMock JSON) |
| `PUT /mockserver/retrieve?type=REQUESTS` | `POST /__admin/requests/find` or `GET /__admin/requests` |
| `PUT /mockserver/retrieve?type=ACTIVE_EXPECTATIONS` | `GET /__admin/stubs`, `GET /__admin/export` |
| `PUT /mockserver/verify` | `POST /__admin/verify` |
| `PUT /mockserver/verifySequence` | `POST /__admin/verify/sequence` |
| `PUT /mockserver/clear?type=LOG` | `POST /__admin/requests/remove` |
| `PUT /mockserver/clear?type=EXPECTATIONS` | `DELETE /__admin/stubs?tag=...` |
| `PUT /mockserver/reset` | `POST /__admin/reset` plus deleting the test's stubs |

## 4. Replace the MockServer deployment

- **Tests with Testcontainers:** replace `MockServerContainer` with a `GenericContainer` running FastMock in
  memory mode ([Java client: Testcontainers](java-client.md#testcontainers)).
- **Shared environments:** install the [Helm chart](../operations/helm.md) or create a `MockServer` resource with
  the [operator](../operations/operator.md), and turn initialization files into `MockApi` resources with inline
  stubs, or import them in a deployment job.
- **Callers:** point the system under test to the FastMock data Service (port 8080) instead of port 1080.

## Checklist

- [ ] Expectations imported with `dryRun`, every warning reviewed.
- [ ] Code callbacks and Velocity/JavaScript templates rewritten as declarative stubs.
- [ ] Priorities of hand-written stubs checked (lower wins).
- [ ] Control clients use the admin port and a token.
- [ ] Tests tag their stubs and wait for asynchronous effects.
- [ ] TLS certificates provided as PEM files where HTTPS is needed.
