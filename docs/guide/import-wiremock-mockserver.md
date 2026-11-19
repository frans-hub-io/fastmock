# Importing WireMock mappings and MockServer expectations

FastMock converts WireMock 3.x stub mappings and MockServer 5.x expectations into native stubs. The result is an
ordinary stub you can read, edit and export; nothing of the source format is kept at run time. Parts that have no
safe equivalent are dropped and reported as warnings, never silently.

- [WireMock](#wiremock)
- [MockServer](#mockserver)
- [Common behaviour](#common-behaviour)

## WireMock

```sh
curl -s -X POST http://localhost:9090/__admin/import/wiremock \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "{\"mappings\": $(cat wiremock/mappings/orders.json), \"tag\": \"orders\", \"dryRun\": false}"
```

`mappings` is a WireMock document `{"mappings": [...]}`, an array of mappings or a single mapping. `tag` (default
`wiremock`) is added to every stub; `replace` (default `true`) deletes the stubs that already carry the tag first;
`dryRun: true` converts and validates without saving and returns the stubs. A mapping without `request`, or one
whose conversion would match more than the original (an unsupported sub-matcher of `matchesJsonPath`, an
unsupported JSONPath construct), is skipped with a warning that names it; the other mappings are imported. A
document that yields no stub at all is answered `400` and nothing is deleted or replaced (see
[Common behaviour](#common-behaviour)).

```json
{"imported": 12, "removed": 12, "warnings": ["mapping #4: customMatcher (Java extension) is not supported and was ignored"]}
```

Files referenced by `bodyFileName` are not part of the mapping: upload the `__files` directory with
`PUT /__admin/files/{name}` (see [Body files](responses.md#body-files)) using the same relative names.

### Mapping level

| WireMock | FastMock | Notes |
|---|---|---|
| `id` / `uuid` | `id` | A new UUID when missing or not a UUID. |
| `name` | `name` | |
| `priority` | `priority` | Same direction (lower first). Default 5 in both. |
| position in the document | `order` | At equal priority the first mapping of a document wins and a later import wins over an earlier one, as with WireMock's `POST /__admin/mappings/import`; URL specificity is not used ([Stub lifecycle](stub-lifecycle.md#equal-priorities-in-imported-stubs)). |
| `scenarioName`, `requiredScenarioState`, `newScenarioState` | `scenario.name`, `requiredState`, `newState` | Initial state `Started` in both. |
| `metadata` | `metadata` | |
| `serveEventListeners` / `postServeActions` of type `webhook` | `callbacks` | `method`, `url`, `headers`, `body`, `delay` (`fixed`, `uniform`, `lognormal`); templates converted; delivered through the durable outbox with 3 attempts. |
| `persistent` | (implied) | Every FastMock stub is stored durably; a warning says so. |

### Request

| WireMock | FastMock |
|---|---|
| `method` | `method` |
| `url` | `request.url` (path and query, exact) |
| `urlPattern` | `request.urlPattern` |
| `urlPath`, `urlPathPattern`, `urlPathTemplate` | same names |
| `pathParameters` | `pathParams` |
| `queryParameters`, `headers`, `cookies` | `query`, `headers` (lower-cased names), `cookies` |
| `host`, `port`, `scheme` | `host`, `port`, `scheme` |
| `basicAuthCredentials` | `basicAuth` |
| `formParameters` | `formParameters` |
| `multipartPatterns` (`name`, `headers`, `bodyPatterns`, `matchingType`) | `multipart` + `multipartMatchingType`; `name="x"` in a `Content-Disposition` header matcher becomes `name`; mixed `ANY`/`ALL` becomes `ALL` with a warning |
| `bodyPatterns` | `body` (below) |

### String matchers

| WireMock | FastMock |
|---|---|
| `equalTo`, `contains`, `matches`, `doesNotMatch`, `absent`, `caseInsensitive` | same names |
| `doesNotContain` | `not: {contains}` |
| `and`, `or`, `not` | `and`, `or`, `not` |
| `hasExactly`, `includes` | `hasExactly`, `includes` |
| `before`, `after`, `equalToDateTime` | same names; `expectedOffset` + `expectedOffsetUnit` become `now -3 days` style expressions |
| `actualFormat` | `actualFormat` (`epoch`, which is milliseconds in WireMock, becomes `epochMillis`) |
| `truncateExpected`, `truncateActual` | same values |

### Body patterns

| WireMock | FastMock |
|---|---|
| `equalTo` (`caseInsensitive`) | `equalTo` (case-insensitive becomes an escaped `(?is)` regex) |
| `contains`, `doesNotContain` | `contains`, `contains` + `negate` |
| `matches`, `doesNotMatch` | `matches`, `matches` + `negate` |
| `equalToJson` with `ignoreExtraElements`, `ignoreArrayOrder`, `${json-unit.*}` placeholders | `equalToJson` with the same options and placeholders; `ignoreExtraElements` also sets `ignoreExtraArrayItems` (extra array elements are allowed, as in WireMock) |
| `matchesJsonPath` (string or `{expression, <matcher>}`) | `jsonPath` + `jsonPathDialect: "jayway"` + `value`; a sub-matcher `equalToJson`, `matchesJsonSchema`, `equalToXml`, `binaryEqualTo` becomes `valueBody` |
| `matchesJsonSchema` | `jsonSchema` (the draft comes from `$schema`; `schemaVersion` is ignored with a warning) |
| `equalToXml` with `enablePlaceholders`, `exemptedComparisons` | `equalToXml` with the same options (placeholders stay off unless enabled, as in WireMock) |
| `matchesXPath` (string or `{expression, <matcher>}`) + `xPathNamespaces` | `xpath` + `value` (or `valueBody`) + `xpathNamespaces` |
| `binaryEqualTo` | `binaryEqualTo` |
| `absent` | `absent` |

### Response

| WireMock | FastMock |
|---|---|
| `status`, `statusMessage` | `status`, `statusMessage` |
| `headers` (an array sends one header line per value, for example two `Set-Cookie` lines) | `headers` (same arrays) |
| `body`, `jsonBody`, `base64Body`, `bodyFileName` | same names |
| `transformers: ["response-template"]` | `templated: true` (otherwise `false`) |
| `fixedDelayMilliseconds` | `delay.fixedMs` |
| `delayDistribution` `uniform` / `fixed` / `lognormal` | `delay.minMs`+`maxMs` / `delay.fixedMs` / `delay.lognormal` (`median`, `sigma`) |
| `chunkedDribbleDelay` (`numberOfChunks`, `totalDuration`) | `chunkedDribbleDelay` (`numberOfChunks`, `totalDurationMs`) |
| `fault` `CONNECTION_RESET_BY_PEER`, `EMPTY_RESPONSE`, `MALFORMED_RESPONSE_CHUNK`, `RANDOM_DATA_THEN_CLOSE` | `CONNECTION_RESET`, `EMPTY_RESPONSE`, `MALFORMED_RESPONSE_CHUNK`, `RANDOM_DATA_THEN_CLOSE` |
| `proxyBaseUrl`, `additionalProxyRequestHeaders`, `removeProxyRequestHeaders`, `proxyUrlPrefixToRemove` | `proxy.baseUrl`, `additionalHeaders`, `removeHeaders`, `stripPrefix` |

### Templates

Response templates are converted to FastMock's context (see [Templating](templating.md)); WireMock helpers such as
`jsonPath`, `xPath`, `formData`, `date`/`now`/`parseDate`, `randomValue`, `randomInt`, `pickRandom`, `math`,
`base64`, `urlEncode`, `trim`, `split`, `join`, `replace`, `capitalize`, `abbreviate`, `contains`, `matches`,
`eq`/`ne`/`gt`/`lt`/`and`/`or`/`not`, `array`, `val`, `size`, `jsonMerge`, `jsonRemove`, `jsonArrayAdd`,
`formatJson`, `formatXml` keep their names and arguments.

| WireMock | FastMock |
|---|---|
| `request.path.[0]`, `request.path.0` | `request.pathSegments.[0]` |
| `request.path.contactId`, `request.path.[contactId]` (variables of `urlPathTemplate`) | `request.pathParams.[contactId]` |
| `originalRequest.*` in webhooks | converted like `request.*` |
| `request.requestLine.path`, `.method`, `.scheme`, `.host`, `.port`, `.baseUrl`, `.query.x` | `request.path`, `request.method`, ... `request.query.x` |
| `request.query.ids.[1]`; `#each`, `size`, `join` over `request.query.ids` | `request.queryAll.ids.[1]`; the same over `request.queryAll.ids` |
| `request.headers.X-Request-Id`, `request.headers.[Content-Type]` | `request.headers.[x-request-id]`, `request.headers.[content-type]` |
| `request.headers.X-Multi.[1]`; `#each`, `size`, `join` over `request.headers.X-Multi` | `request.headersAll.[x-multi].[1]`; the same over `request.headersAll.[x-multi]` |
| `{{now}}` without `format` (ISO 8601 without milliseconds) | `{{now format='isoSeconds'}}` |
| `{{randomInt lower=1 upper=10}}` (upper bound exclusive; `lower` defaults to -2147483648) | `{{randomInt lower=1 upper=9}}` (FastMock's bounds are inclusive); a missing `lower` or `upper` is written out |
| `request.url`, `request.cookies.x`, `request.body` | same |
| `format='epoch'` (milliseconds) | `format='epochMillis'` |

### Not converted

| WireMock feature | Why | What happens |
|---|---|---|
| `customMatcher`, custom transformers, `transformerParameters`, non-webhook serve event listeners | Java extensions; FastMock runs no user code | warning; the part is dropped |
| `systemValue`, `hostname`, `{{#assign}}`, `parameters.*` in templates | No access to the environment or host; no extensions | warning; renders empty |
| `placeholderOpeningDelimiterRegex` / `ClosingDelimiterRegex` | Only `${...}` placeholders exist | warning |
| Webhook `base64Body` | Callbacks send text bodies | warning; sent without a body |
| `persistent` | Always durable | warning (informational) |

## MockServer

```sh
curl -s -X POST "http://localhost:9090/__admin/import/mockserver?tag=legacy&dryRun=true" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d @expectations.json
```

The body is one expectation, an array of expectations (the format of MockServer's `PUT /mockserver/expectation`
and of its initialization JSON), or a wrapper `{"expectations": [...], "tag": "legacy", "replace": true,
"dryRun": false}`. Query parameters `tag` (default `mockserver`), `replace` (default `true`) and `dryRun` (default
`false`) override the wrapper. The response has the same shape as the WireMock import:

```json
{
  "imported": 0,
  "removed": 0,
  "dryRun": true,
  "warnings": ["expectation #2: JAVASCRIPT response templates are not executed; the expectation was skipped"],
  "stubs": [{"name": "GET /ping", "priority": 5, "times": 1, "request": {"method": "GET", "urlPath": "/ping"}, "response": {"status": 200, "body": "pong"}}]
}
```

Requires the editor role; at most 10,000 expectations per request. Converted stubs that do not validate (for example
an invalid regular expression) are skipped with a warning; the rest are imported.

### Expectation

| MockServer | FastMock | Notes |
|---|---|---|
| `id` | `id` (when a UUID), else `name` | |
| `priority` | `priority = 5 - priority` | MockServer runs higher priorities first; FastMock lower. `10` becomes `-5`, `0` becomes `5`. |
| `times.remainingTimes` / `unlimited` | `times` / none | Counted across replicas. |
| `timeToLive` (`timeUnit`, `timeToLive`, `unlimited`) | `ttlSeconds` | Converted to seconds (at least 1). `unlimited` defaults to `false`, as in MockServer. |
| position in the document | `order` | At equal priority the expectation created first wins: the first of the document, and an earlier import over a later one ([Stub lifecycle](stub-lifecycle.md#equal-priorities-in-imported-stubs)). |
| `httpResponse` | `response` | |
| `httpResponseTemplate` `MUSTACHE` | `response` with `templated: true` | Only when the template is a JSON response with plain `{{ value }}` tags (below). |
| `httpForward` (`host`, `port`, `scheme`) | `response.proxy.baseUrl` | |
| `httpOverrideForwardedRequest` | `response.proxy` | Target from `socketAddress` or the `Host` header; other headers become `additionalHeaders`. |
| `httpError` | `response.fault` | `responseBytes` → `RANDOM_DATA_THEN_CLOSE`; `dropConnection` → `CONNECTION_RESET`; neither → `EMPTY_RESPONSE`. |

Every imported stub gets `metadata: {"source": "mockserver"}`.

### httpRequest

| MockServer | FastMock |
|---|---|
| `method` | `method` |
| `path` without regex characters | `urlPath` |
| `path` with `{name}` or with `pathParameters` | `urlPathTemplate` + `pathParams` |
| `path` with regex characters (`* + ? [ ] ( ) ^ $ \| \`) | `urlPathPattern` |
| `queryStringParameters`, `headers`, `cookies` (object or `[{name, values}]` form) | `query`, `headers`, `cookies` |
| `secure: true/false` | `scheme: https/http` |
| `keepAlive: true/false` | `connection` header must not / must contain `close` |
| `body` | `body`, see below |

Values: a value with regex characters matches either literally or as a regular expression (MockServer's rule); other
values match exactly. `"!value"` and `{"not": true, "value": ...}` become `not`. `{"schema": {...}}` uses `pattern`,
`enum`, or `type` `integer`/`number`/`boolean` and `format` `uuid`/`date`; other schemas become a presence check with
a warning. Several values for one name become `includes` (all must be present, others allowed). With
`"keyMatchStyle": "MATCHING_KEY"` the values become `allValues` (every value of the key must match one of them). An
optional key `"?name"` becomes `or: [absent, value]`; a negated key `"!name"` becomes `absent: true`.

| `body` | FastMock body matcher |
|---|---|
| a string | `equalTo` |
| a JSON object without `type` | `equalToJson` with `ignoreExtraElements`, `ignoreExtraArrayItems` and `ignoreArrayOrder` |
| `STRING` (`subString`) | `equalTo` (`contains`) |
| `REGEX` | `matches` |
| `JSON` `ONLY_MATCHING_FIELDS` (default) / `STRICT` | `equalToJson` lenient (extra fields, extra array elements, any array order) / exact |
| `JSON_SCHEMA` | `jsonSchema` |
| `JSON_PATH` | `jsonPath` with `jsonPathDialect: "jayway"` |
| `XML` | `equalToXml` with `exemptedComparisons: ["CHILD_NODELIST_SEQUENCE"]` (child order does not matter, as in MockServer) |
| `XPATH` (`namespacePrefixes`) | `xpath` (`xpathNamespaces`) |
| `BINARY` (`base64Bytes`) | `binaryEqualTo` |
| `PARAMETERS` | `formParameters` |
| `"not": true` | `negate: true` |

### httpResponse

| MockServer | FastMock |
|---|---|
| `statusCode`, `reasonPhrase` | `status`, `statusMessage` |
| `headers` (several values: one header line each) | `headers` (arrays) |
| `cookies` | one `set-cookie` line per cookie, added to a `Set-Cookie` header of `headers` if there is one |
| `body` string / `STRING` / `JSON` / `XML` / `BINARY` / object | `body` / `body` / `jsonBody` / `body` + `application/xml` / `base64Body` / `jsonBody`; `contentType` becomes `content-type` |
| `delay` (`timeUnit`, `value`) | `delay.fixedMs` |

Imported MockServer responses are not templated (`templated: false`), so `{{` in a body is sent as is.

### Mustache templates

A `MUSTACHE` template is converted when it is a JSON response object and every tag is one of:

| MockServer | FastMock |
|---|---|
| `{{ request.method }}`, `{{ request.path }}`, `{{ request.body }}` | same |
| `{{ request.pathParameters.id.0 }}` | `{{request.pathParams.[id]}}` |
| `{{ request.queryStringParameters.q.0 }}` | `{{request.query.[q]}}` |
| `{{ request.headers.X-Key.0 }}` | `{{request.headers.[x-key]}}` |
| `{{ request.cookies.name }}` | `{{request.cookies.[name]}}` |
| `{{ uuid }}`, `{{ now_iso_8601 }}`, `{{ now_epoch }}`, `{{ rand_int_10 }}`, `{{ rand_int_100 }}` | `{{uuid}}`, `{{now}}`, `{{now format='epoch'}}`, `{{randomInt 0 9}}`, `{{randomInt 0 99}}` |

Templates with sections (`{{#...}}`, `{{^...}}`), other values, or tags outside JSON strings are skipped with a
warning; rewrite them as a FastMock stub with [Handlebars helpers](templating.md).

### Not converted

| MockServer feature | Why | What happens |
|---|---|---|
| `VELOCITY` and `JAVASCRIPT` templates | FastMock never executes code from stubs | expectation skipped, warning |
| `httpResponseClassCallback`, `httpResponseObjectCallback`, `httpForwardClassCallback`, `httpForwardObjectCallback`, `httpForwardTemplate` | Code callbacks | expectation skipped, warning |
| Negated `method` or `path` (`!GET`, `{"not": true}`) | Not expressible without matching everything else | expectation skipped, warning |
| `XML_SCHEMA` body | No XSD validator | the body condition is dropped, warning |
| `socketAddress`, `protocol`, `localAddress`, `remoteAddress`, `clientCertificateChain` in `httpRequest` | Connection properties are not matched | dropped, warning |
| `connectionOptions` in `httpResponse` | Socket-level control | dropped, warning |
| Overriding `path`, `method`, query or body of a forwarded request; overriding the forwarded response | The proxy forwards the request as received | dropped, warning |
| Optional key with several values | | matched as required, warning |

## Common behaviour

- Imports are validated exactly like stubs created through `POST /__admin/stubs`; templates are compiled, regular
  expressions are size-limited and XML/JSON limits apply (see [Limits](request-matching.md#limits)). A converted
  stub that fails validation is skipped with a warning naming it (its `name`, or its id); the rest are imported.
- A document that yields no valid stub (wrong structure such as `{"mappings": 5}`, a misspelled key, only skipped
  mappings or expectations) is answered `400 {"error": "the document produced no stubs; nothing was changed; ..."}`
  with the first warnings, and the stubs of the tag stay as they were. Counted in
  `fastmock_import_rejections_total{format}`.
- `dryRun` returns the converted stubs without storing them, so you can review or adjust them and send them to
  `POST /__admin/stubs/import` yourself.
- Every import is audited (`import.wiremock`, `import.mockserver`) and counted in
  `fastmock_imports_total{format, dry_run}`.
- `POST /__admin/import` detects the format of a document automatically and calls the same converters.
