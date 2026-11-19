# Java client

`clients/java` is a small client for the FastMock admin API, written for Java 25 with one dependency (Jackson
databind). It creates stubs with a fluent builder or raw JSON, waits for requests and callbacks, verifies the
journal, and manages scenarios, files and imports. All examples on this page were run against FastMock 0.1.0.

- [Setup](#setup)
- [Connecting](#connecting)
- [Stub builder](#stub-builder)
- [Method reference](#method-reference)
- [JUnit 5 patterns](#junit-5-patterns)
- [Testcontainers](#testcontainers)
- [Errors](#errors)

## Setup

Build and publish the client to your Maven repository (or to the local one):

```sh
cd clients/java
gradle publishToMavenLocal        # io.franshub:fastmock-client:0.1.0
```

```kotlin
// build.gradle.kts
dependencies {
    testImplementation("io.franshub:fastmock-client:0.1.0")
}
```

```xml
<!-- pom.xml -->
<dependency>
  <groupId>io.franshub</groupId>
  <artifactId>fastmock-client</artifactId>
  <version>0.1.0</version>
  <scope>test</scope>
</dependency>
```

The client uses `java.net.http.HttpClient` (no redirects) and Jackson 2.

## Connecting

```java
FastMock fastmock = FastMock.connect("http://fastmock-admin.mocks.svc:9090", System.getenv("FASTMOCK_TOKEN"));
FastMock slow = FastMock.connect(adminUrl, token, Duration.ofSeconds(30));   // connect and request timeout
```

The URL is the **admin** port. The token needs the `editor` role for everything tests usually do and `admin` for
`reset()`. A `FastMock` instance is thread-safe; create one per test class or suite.

## Stub builder

`Stub` builds the JSON of a stub. Paths are **path templates** (`/orders/{id}`, a trailing `{rest...}` matches
the rest of the path):

```java
fastmock.createStub(Stub.post("/api/{version}/messages")
        .name("send sms")
        .priority(5)
        .pathParam("version", Match.matches("v[12]"))
        .query("tenant", Match.equalTo("a"))
        .header("X-Api-Key", Match.present())
        .bodyJsonPath("$.to", Match.matches("380\\d{9}"))
        .bodyJsonSchema(Map.of("type", "object", "required", List.of("to", "text")))
        .respond(202)
        .responseHeader("x-request-id", "{{uuid}}")
        .jsonBody(Map.of("id", "{{uuid}}", "to", "{{request.json.to}}"))
        .randomDelay(Duration.ofMillis(50), Duration.ofMillis(200))
        .callback(Callback.post("http://app.example.svc/hooks/sms")
                .jsonBody(Map.of("to", "{{request.json.to}}", "status", "DELIVERED"))
                .header("X-Event", "delivered")
                .delay(Duration.ofSeconds(2))
                .maxAttempts(5)
                .correlation("{{request.json.to}}")
                .hmac("X-Signature", "delivery-hook", "sha256", "sha256="))
        .tag("sms-test"));
```

| Builder method | Stub JSON |
|---|---|
| `Stub.get/post/put/delete(path)`, `Stub.any(path)` | `request.method`, `request.urlPathTemplate` |
| `name`, `priority`, `tag` | `name`, `priority`, `tags[]` |
| `query(name, m)`, `header(name, m)`, `pathParam(name, m)` | `request.query`, `request.headers` (name lower-cased), `request.pathParams` |
| `bodyJsonPath(path, m)` (`m` may be null for "exists"), `bodyEqualToJson(obj, ignoreExtra)`, `bodyContains(s)`, `bodyJsonSchema(obj)` | `request.body[]` |
| `respond(status)`, `responseHeader`, `body`, `jsonBody`, `literal()` | `response.status`, `headers`, `body`, `jsonBody`, `templated: false` |
| `delay(d)`, `randomDelay(min, max)` | `response.delay` |
| `fault("CONNECTION_RESET")` | `response.fault` |
| `proxyTo(baseUrl)` | `response.proxy.baseUrl` |
| `scenario(name, requiredState, newState)` | `scenario` (nulls are omitted) |
| `callback(Callback...)` | `callbacks[]` |
| `toJson()` | the JSON as a Jackson tree, to extend or send yourself |

`Match`: `equalTo`, `contains`, `matches` (regex), `absent()`, `present()`, `.ignoringCase()`.

`Callback`: `Callback.post(url)`, `Callback.to(method, url)`, `header`, `body`, `jsonBody`, `delay`,
`maxAttempts`, `correlation`, `hmac(header, secretRef, algorithm, prefix)`.

For any field the builder does not cover (`times`, `responses`, `websocket`, `messages`, GraphQL, XML
matchers...), pass raw JSON:

```java
ObjectMapper json = new ObjectMapper();
fastmock.createStub(json.readTree("""
        {"tags": ["orders-test"], "times": 1,
         "request": {"method": "POST", "urlPath": "/orders"},
         "response": {"status": 201, "jsonBody": {"status": "CREATED"}}}
        """));
```

## Method reference

### Stubs

| Method | Admin call | Returns |
|---|---|---|
| `UUID createStub(Stub)`, `UUID createStub(JsonNode)` | `POST /stubs` | the new stub id |
| `void updateStub(UUID, Stub)` | `PUT /stubs/{id}` | |
| `JsonNode patchStub(UUID, JsonNode mergePatch)` | `PATCH /stubs/{id}` (JSON merge patch) | the patched stub |
| `void deleteStub(UUID)` | `DELETE /stubs/{id}` | |
| `long deleteStubsByTag(String)` | `DELETE /stubs?tag=` | number deleted |
| `JsonNode listStubs()` | `GET /stubs` | the `stubs` array |
| `JsonNode findStubsByMetadata(JsonNode)`, `long removeStubsByMetadata(JsonNode)` | `POST /stubs/find-by-metadata`, `/remove-by-metadata` | stubs / number deleted |
| `ImportResult importOpenApi(spec, tag, pathPrefix, validateRequests)` | `POST /import/openapi` | `imported`, `removed`, `warnings` |
| `ImportResult importWireMock(JsonNode mappings, tag)` | `POST /import/wiremock` | `imported`, `removed`, `warnings` |
| `void putFile(name, bytes, contentType)`, `void deleteFile(name)`, `JsonNode listFiles()` | `PUT/DELETE /files/{name}`, `GET /files` | body files for `bodyFileName` |

### Journal and verification

| Method | Admin call | Behaviour |
|---|---|---|
| `List<RecordedRequest> requests(RequestFilter)` | `GET /requests` | Matching entries, newest first. |
| `long countRequests(RequestFilter)` | `GET /requests/count` | |
| `List<RecordedRequest> awaitRequests(RequestFilter, n, Duration)` | polls `GET /requests` every 100 ms | Returns when at least `n` entries match; throws `AssertionError` with the last result on timeout. |
| `List<RecordedRequest> findRequests(JsonNode pattern)` | `POST /requests/find` | Entries matching a stub `request` pattern. |
| `VerifyResult verify(JsonNode body)` | `POST /verify` | Never throws on a failed expectation; check `verified()`. |
| `VerifyResult verifyExactly(pattern, n)`, `verifyAtLeast`, `verifyAtMost`, `verifyNever(pattern)` | `POST /verify` | Throw `AssertionError` with the reason and near misses when the expectation fails. |
| `void verifySequence(List<JsonNode> patterns)` | `POST /verify/sequence` | Throws `AssertionError` when the order is broken. |
| `JsonNode snapshot(JsonNode body)` | `POST /recordings/snapshot` | Stubs from journal entries ([Recording](recording.md)). |
| `void clearRequests()` | `DELETE /requests` | |

`RequestFilter` builds the query of `/requests`: `RequestFilter.all()`, `.path(prefix)`, `.source(s)`,
`.correlation(c)`, and `.withMethod`, `.withPathPrefix`, `.withCorrelation`, `.withBodyContaining`,
`.withSource`, `.matched(boolean)`, `.since(Instant)`, `.limit(int)`.

`RecordedRequest` is a record with `id`, `at`, `source`, `method`, `path`, `query`, `headers`, `body`, `status`,
`responseBody`, `stubId`, `correlation`, `matched`, `durationMs`, `traceId`, and `json()` to parse the body.

A failed `verifyExactly` reads like this:

```text
java.lang.AssertionError: expected exactly 1 matching requests, received 2; request 6 POST /hooks/payments: path: `/hooks/payments` does not match; ...
```

### Callbacks

| Method | Admin call | Behaviour |
|---|---|---|
| `List<CallbackRecord> callbacks(String correlation)` | `GET /callbacks?correlation=` | Newest first. |
| `List<CallbackRecord> awaitCallbacks(correlation, n, Duration)` | polls every 100 ms | Returns when at least `n` callbacks exist and none is `pending`; throws `AssertionError` on timeout. |

`CallbackRecord` has `id`, `source`, `correlation`, `url`, `method`, `headers` (redacted), `body`, `dueAt`,
`attempts`, `maxAttempts`, `status` (`pending`, `sent`, `failed`), `lastError`, `lastStatus`, `createdAt`,
`sentAt`, `traceparent`, and `json()`. A real record:

```text
CallbackRecord[id=01a1f297-23ff-750f-b120-5e83231d7dbb, source=stubs, correlation=ord-7a9ee5b9-..., url=http://fastmock:8080/hooks/payments,
  method=POST, headers={content-type=application/json, x-signature=***}, body={"status":"PAID","orderId":"ord-7a9ee5b9-..."},
  attempts=1, maxAttempts=5, status=sent, lastError=null, lastStatus=204, ...]
```

### Scenarios, key-value, reset, plugins

| Method | Admin call |
|---|---|
| `void setScenarioState(name, state)` | `PUT /scenarios/{name}` (name URL-encoded) |
| `void resetScenarios()` | `DELETE /scenarios` |
| `void putValue(key, value, Duration ttl)` | `PUT /kv/{key}` (`ttl` may be null) |
| `void reset()` | `POST /reset` (admin role): clears journal, callbacks, scenario states; stubs stay |
| `JsonNode plugin(plugin, method, path, body)` | any call under `/plugins/{plugin}/` |

## JUnit 5 patterns

The journal, callbacks and scenarios are **shared** by everything that talks to the same FastMock: other test
classes, parallel test runs, other pipelines on a shared environment. Make every test independent:

1. **Tag** the stubs of a test with a unique tag and delete them in `@AfterEach`.
2. Use **unique values** (order ids, correlation ids, scenario names) per test, and filter the journal by them
   (`withCorrelation`, `withBodyContaining`) or by time (`since(start)`), not only by path.
3. **Wait** for asynchronous effects (`awaitRequests`, `awaitCallbacks`); the journal and callbacks are written
   after the response.
4. Avoid `reset()` in shared environments; it affects everyone.

```java
class PaymentFlowTest {

    static FastMock fastmock;
    static String dataUrl;
    static final HttpClient http = HttpClient.newHttpClient();

    String tag;

    @BeforeAll
    static void connect() {
        fastmock = FastMock.connect(System.getenv("FASTMOCK_ADMIN_URL"), System.getenv("FASTMOCK_TOKEN"));
        dataUrl = System.getenv("FASTMOCK_DATA_URL");
    }

    @BeforeEach
    void newTag() {
        tag = "test-" + UUID.randomUUID();
    }

    @AfterEach
    void cleanUp() {
        fastmock.deleteStubsByTag(tag);
    }

    @Test
    void paymentIsConfirmedByWebhook() throws Exception {
        String orderId = "ord-" + UUID.randomUUID();
        fastmock.createStub(Stub.post("/hooks/payments").respond(204).tag(tag));
        fastmock.createStub(Stub.post("/payments")
                .bodyJsonPath("$.amount", Match.matches("[0-9]+"))
                .respond(202)
                .jsonBody(Map.of("id", "{{request.json.orderId}}", "status", "PENDING"))
                .callback(Callback.post(dataUrl + "/hooks/payments")
                        .jsonBody(Map.of("orderId", "{{request.json.orderId}}", "status", "PAID"))
                        .delay(Duration.ofMillis(200))
                        .maxAttempts(5)
                        .correlation("{{request.json.orderId}}")
                        .hmac("X-Signature", "delivery-hook", "sha256", "sha256="))
                .tag(tag));

        // in a real test the system under test makes this call
        var response = http.send(HttpRequest.newBuilder(URI.create(dataUrl + "/payments"))
                .header("content-type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofString("{\"orderId\":\"" + orderId + "\",\"amount\":250}"))
                .build(), HttpResponse.BodyHandlers.ofString());
        assertEquals(202, response.statusCode());

        var callbacks = fastmock.awaitCallbacks(orderId, 1, Duration.ofSeconds(10));
        assertEquals("sent", callbacks.getFirst().status());
        assertEquals("PAID", callbacks.getFirst().json().get("status").asText());

        var requests = fastmock.awaitRequests(
                RequestFilter.path("/payments").withCorrelation(orderId), 1, Duration.ofSeconds(5));
        assertEquals(250, requests.getFirst().json().get("amount").asInt());
    }

    @Test
    void secondPaymentIsRefused() throws Exception {
        String scenario = "order-" + UUID.randomUUID();
        fastmock.createStub(Stub.post("/orders/{id}/pay").scenario(scenario, "Started", "PAID")
                .respond(200).body("paid").tag(tag));
        fastmock.createStub(Stub.post("/orders/{id}/pay").scenario(scenario, "PAID", null)
                .respond(409).body("already paid").tag(tag));
        // ... exercise the system under test ...
        fastmock.setScenarioState(scenario, "Started");    // start over without new stubs
    }
}
```

This class ran green against a FastMock started with `FASTMOCK_SECRET_DELIVERY_HOOK` and
`FASTMOCK_STUB_SECRETS=delivery-hook`. The callback goes to the data URL of FastMock itself, which works when that
URL is not a loopback address (see [Callbacks: egress policy](callbacks.md#egress-policy)); in real tests it
points to the system under test.

## Testcontainers

For tests that start their own FastMock, run the image in memory mode with a generated token:

```java
@Testcontainers
class WithOwnFastMockTest {

    static final String TOKEN = "fm_test_" + UUID.randomUUID().toString().replace("-", "");

    @Container
    static final GenericContainer<?> FASTMOCK = new GenericContainer<>("registry.example.com/platform/fastmock:0.1.0")
            .withEnv("FASTMOCK_STORAGE", "memory")
            .withEnv("FASTMOCK_ADMIN_TOKENS", "tests:admin:" + sha256Hex(TOKEN))
            .withEnv("FASTMOCK_LOG_FORMAT", "text")
            .withExposedPorts(8080, 9090)
            .waitingFor(Wait.forHttp("/readyz").forPort(9090));

    static FastMock fastmock;
    static String dataUrl;

    @BeforeAll
    static void connect() {
        fastmock = FastMock.connect("http://" + FASTMOCK.getHost() + ":" + FASTMOCK.getMappedPort(9090), TOKEN);
        dataUrl = "http://" + FASTMOCK.getHost() + ":" + FASTMOCK.getMappedPort(8080);
    }

    static String sha256Hex(String s) {
        try {
            return HexFormat.of().formatHex(
                    MessageDigest.getInstance("SHA-256").digest(s.getBytes(StandardCharsets.UTF_8)));
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException(e);
        }
    }
}
```

- Point the system under test (or its configuration property) to `dataUrl`.
- To receive callbacks in the test JVM, expose a host port with `Testcontainers.exposeHostPorts(port)` and use
  `http://host.testcontainers.internal:<port>/...` as the callback URL.
- For HMAC callbacks add `.withEnv("FASTMOCK_SECRET_DELIVERY_HOOK", key)` and
  `.withEnv("FASTMOCK_STUB_SECRETS", "delivery-hook")`.
- A container per test class keeps tests isolated without tags; a shared container (static in a base class) is
  faster and needs the patterns above.

## Errors

Admin API errors throw `FastMockException` (unchecked) with `status()` and `body()`; connection problems throw it
with `status() == -1` and the cause.

```java
var error = assertThrows(FastMockException.class, () -> fastmock.createStub(Stub.get("/broken").respond(42)));
assertEquals(400, error.status());
// error.body(): {"error":"invalid status 42","details":[]}
```

Failed expectations (`awaitRequests`, `awaitCallbacks`, `verifyExactly` and the other `verify*` helpers except
`verify(JsonNode)`) throw `AssertionError`, so test frameworks report them as failures, not errors.
