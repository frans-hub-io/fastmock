# Migrating from WireMock

FastMock reads WireMock 3.x stub mappings, keeps their matching semantics where they exist in both tools, and
reports what it cannot convert. A migration usually takes three steps: import the mappings, adapt the test code,
and adjust the few behaviours that differ on purpose.

The field-by-field conversion table is on [WireMock and MockServer import](import-wiremock-mockserver.md#wiremock);
this page is about the process.

- [1. Import the mappings](#1-import-the-mappings)
- [2. What behaves differently](#2-what-behaves-differently)
- [3. Change the test code](#3-change-the-test-code)
- [4. Replace the WireMock container or server](#4-replace-the-wiremock-container-or-server)
- [Checklist](#checklist)

## 1. Import the mappings

Mappings come from a WireMock project's `mappings/` directory, or from a running WireMock with
`GET /__admin/mappings`. Body files from `__files/` are uploaded separately with the same relative names.

```sh
export FASTMOCK=http://localhost:9090
# body files referenced by bodyFileName
(cd wiremock/__files && find . -type f | sed 's|^\./||' | while read -r f; do
  curl -sf -X PUT "$FASTMOCK/__admin/files/$f" -H "Authorization: Bearer $FASTMOCK_TOKEN" \
    -H 'content-type: application/octet-stream' --data-binary @"$f" >/dev/null || echo "failed: $f"
done)

# one mapping file at a time, all under one tag; check with dryRun first
for f in wiremock/mappings/*.json; do
  curl -s -X POST "$FASTMOCK/__admin/import/wiremock" \
    -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
    -d "{\"mappings\": $(cat "$f"), \"tag\": \"wm-$(basename "$f" .json)\", \"dryRun\": true}" \
    | jq -c '{file: "'"$f"'", stubs: (.stubs | length), warnings}'
done
```

Read every warning. They name the mapping and the part that was dropped, for example:

```json
{"imported": 12, "removed": 12, "warnings": ["mapping #4: customMatcher (Java extension) is not supported and was ignored"]}
```

Then run the same loop without `dryRun`. Each import replaces the stubs of its tag, so the loop is safe to
re-run. `POST /__admin/import` detects the format automatically if you have mixed sources
([Importing](importing.md)).

For Kubernetes, put the mappings into a ConfigMap and let the [operator](../operations/operator.md#mockapi) import
them (`MockApi.spec.wiremock`); every change of the ConfigMap is re-imported.

## 2. What behaves differently

| Topic | WireMock | FastMock | What to do |
|---|---|---|---|
| Ports | Mocks and `/__admin` on one port | Mocks on 8080, admin API on 9090 | Point admin clients to the admin port. |
| Admin authentication | Off by default | Always on, bearer tokens with roles | Create tokens ([Security](../operations/security.md#admin-tokens-and-roles)). |
| Admin API | `/__admin/mappings`, `/__admin/requests`... | `/__admin/stubs`, `/__admin/requests`, `/__admin/verify`... | See [Admin API](admin-api.md); the [verification table](verification.md#coming-from-wiremock-or-mockserver) maps the calls. |
| Extensions | Java extensions, custom matchers and transformers | No user code; plugins are compiled into FastMock | Express the logic with matchers, templates and scenarios, or ask for a plugin. |
| Response templating | Opt-in per stub (`response-template`) or global | On by default for stub responses; `"templated": false` turns it off | Imported stubs keep their WireMock setting. |
| Template data | `request.path.[0]`, `request.path.id`, `request.headers.X-Id`, multi-valued `request.query.x` | `request.pathSegments.[0]`, `request.pathParams.id`, `request.headers.[x-id]` (lower case), `request.queryAll.x` / `request.headersAll.[x-id]` | Converted on import (also inside webhooks' `originalRequest`); new stubs use the FastMock names ([Templating](templating.md)). |
| `now` and `randomInt` | `{{now}}` has no milliseconds; `randomInt` excludes `upper` and starts at -2147483648 without `lower` | `{{now}}` has milliseconds; `randomInt` includes `upper` and starts at 0 | Imported templates are rewritten to WireMock's results (`format='isoSeconds'`, `upper` minus one, explicit bounds). |
| Equal priority | The newest stub wins; `POST /__admin/mappings/import` adds a document from last to first, so its first mapping wins; URL specificity is not used | Native stubs: the more specific URL, then the newest | Imported mappings keep WireMock's rule through an `order` field ([Stub lifecycle](stub-lifecycle.md#equal-priorities-in-imported-stubs)). WireMock loading a `mappings/` file with several mappings lets the **last** one win instead; if a file relies on that, give its mappings explicit priorities. |
| JSONPath | Jayway: `=~`, `in`, `nin`, `length()`, filters on objects | RFC 9535 for native stubs; imported matchers use `jsonPathDialect: "jayway"` | `contains`, `subsetof`, `anyof`, `noneof` are not supported: those mappings are skipped with a warning naming them ([jsonPath](request-matching.md#jsonpath)). |
| `matchesJsonPath` with `equalToJson`, `matchesJsonSchema`, `equalToXml` | Applied to the selected node | `jsonPath` + `valueBody` | Converted; a sub-matcher that cannot be converted skips the mapping rather than matching more. |
| `ignoreExtraElements` | Also allows extra array elements | Same (`ignoreExtraArrayItems`) | Converted. |
| `fault: EMPTY_RESPONSE` | Closes the connection without a byte | Same | Converted. |
| Header arrays | One header line per value (two `Set-Cookie` lines) | Same | Converted. |
| Empty imports | | A document that yields no stub (`{"mappings": 5}`, a misspelled `mappings` key) answers `400` and changes nothing | Read the error. |
| Webhooks | Fire-and-forget from the JVM | Durable callbacks with retries, HMAC, correlation, egress policy | Callbacks to `localhost` are blocked by default: use service names ([Callbacks](callbacks.md#egress-policy)). |
| Persistence | In memory unless `persistent` | Every stub is stored; shared by all replicas in `postgres` mode | Tag stubs per test and delete by tag in teardown. |
| Request journal | Synchronous | Written right after the response, asynchronously | Wait for requests (`awaitRequests`, `/requests/count`) instead of reading once. |
| Unmatched requests | 404 with a diff report in the body | `404 {"error":"no stub matched",...}`; near misses through the admin API, or in the body with `FASTMOCK_UNMATCHED_DETAILS=true` | [Verification](verification.md). |
| Proxy and recording | `proxyBaseUrl`, recorder | `response.proxy`, `/__admin/recordings/*`, snapshots | [Responses](responses.md#proxying), [Recording](recording.md). Proxy targets go through the egress policy. |
| Chunked dribble, faults, delays | Supported | Supported (`chunkedDribbleDelay`, faults, `fixed`/`uniform`/`lognormal` delays) | Converted on import. |
| Scenarios | `Started` initial state, global names | Same, plus templated names, TTLs and atomic transitions across replicas | [Scenarios](scenarios-and-state.md). |
| HTTPS | Keystore with generated certificate | PEM files (`FASTMOCK_TLS_*`), optional mTLS | [Configuration](../operations/configuration.md#data-plane-tls-and-mtls). |

## 3. Change the test code

### Java

WireMock's DSL talks to `/__admin/mappings`. Replace it with the [FastMock Java client](java-client.md), or keep
your existing JSON mappings and send them through `importWireMock`.

```java
// WireMock
WireMock.configureFor("localhost", 8080);
stubFor(post(urlPathEqualTo("/sms"))
        .withRequestBody(matchingJsonPath("$.to", matching("380\\d{9}")))
        .willReturn(aResponse().withStatus(202).withHeader("Content-Type", "application/json")
                .withBody("{\"id\":\"{{randomValue type='UUID'}}\"}")
                .withTransformers("response-template")));
// ...
verify(1, postRequestedFor(urlPathEqualTo("/sms")));
WireMock.reset();
```

```java
// FastMock
FastMock fastmock = FastMock.connect("http://localhost:9090", System.getenv("FASTMOCK_TOKEN"));
String tag = "sms-test-" + UUID.randomUUID();
fastmock.createStub(Stub.post("/sms")
        .bodyJsonPath("$.to", Match.matches("380\\d{9}"))
        .respond(202)
        .jsonBody(Map.of("id", "{{uuid}}"))
        .tag(tag));
// ...
fastmock.verifyExactly(new ObjectMapper().readTree("""
        {"method": "POST", "urlPath": "/sms"}"""), 1);
fastmock.deleteStubsByTag(tag);
```

Differences to keep in mind:

- `Stub.post(path)` uses a **path template** (`/orders/{id}` works); query strings are matched with `.query(...)`.
- Tag every stub a test creates and delete the tag in `@AfterEach`; this keeps parallel tests and shared
  environments independent, which a global `reset()` would not.
- Use `awaitRequests` / `awaitCallbacks` for anything asynchronous.

### Raw HTTP

| WireMock call | FastMock call |
|---|---|
| `POST /__admin/mappings` | `POST /__admin/stubs` (FastMock stub JSON) or `POST /__admin/import/wiremock` (WireMock JSON) |
| `POST /__admin/mappings/import` | `POST /__admin/import/wiremock` |
| `DELETE /__admin/mappings` | `DELETE /__admin/stubs?tag=...` (by tag), or delete all as admin |
| `POST /__admin/mappings/find-by-metadata`, `remove-by-metadata` | `POST /__admin/stubs/find-by-metadata`, `/stubs/remove-by-metadata` |
| `GET /__admin/requests`, `POST /__admin/requests/count`, `/requests/find` | same paths |
| `GET /__admin/requests/unmatched`, `/near-misses/...` | `GET /__admin/requests/unmatched`, `POST /__admin/requests/near-misses` |
| `POST /__admin/scenarios/reset` | `DELETE /__admin/scenarios` |
| `PUT /__admin/scenarios/{name}/state` | `PUT /__admin/scenarios/{name}` |
| `POST /__admin/reset` | `POST /__admin/reset` (journal, callbacks, scenarios; stubs stay) |
| `POST /__admin/recordings/start`, `stop`, `snapshot` | same paths ([Recording](recording.md)) |

## 4. Replace the WireMock container or server

With Testcontainers, the FastMock image runs as a `GenericContainer`:

```java
static final String TOKEN = "fm_test_" + UUID.randomUUID().toString().replace("-", "");

@Container
static final GenericContainer<?> FASTMOCK = new GenericContainer<>("registry.example.com/platform/fastmock:0.1.0")
        .withEnv("FASTMOCK_STORAGE", "memory")
        .withEnv("FASTMOCK_ADMIN_TOKENS", "tests:admin:" + sha256Hex(TOKEN))
        .withExposedPorts(8080, 9090)
        .waitingFor(Wait.forHttp("/readyz").forPort(9090));
```

Details and the `sha256Hex` helper are in the [Java client guide](java-client.md#testcontainers).

In shared environments, replace the WireMock Deployment with the [Helm chart](../operations/helm.md) or a
`MockServer` resource, and the mapping ConfigMaps with `MockApi` resources.

## Checklist

- [ ] Mappings imported with `dryRun`, every warning reviewed.
- [ ] Body files uploaded.
- [ ] Java extensions replaced by matchers, templates, scenarios or a plugin.
- [ ] Admin clients use the admin port and a token.
- [ ] Webhook targets reachable under the egress policy (no `localhost`).
- [ ] Tests tag their stubs and wait for asynchronous effects.
- [ ] Unmatched-request diagnostics switched to near misses.
