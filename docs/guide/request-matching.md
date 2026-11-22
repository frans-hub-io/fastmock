# Request matching

A stub answers a request when every condition in its `request` object holds. Conditions that are not set do not
restrict anything: `{"request": {}}` matches every request. This page lists every condition, the string and body
matchers they use, how FastMock picks one stub when several match, and the limits that keep matching cheap.

- [A complete example](#a-complete-example)
- [Method and URL](#method-and-url)
- [Query parameters, headers and cookies](#query-parameters-headers-and-cookies)
- [Host, port, scheme and basic authentication](#host-port-scheme-and-basic-authentication)
- [Form parameters](#form-parameters)
- [Multipart parts](#multipart-parts)
- [String matchers](#string-matchers)
- [Body matchers](#body-matchers)
- [GraphQL](#graphql)
- [Which stub wins](#which-stub-wins)
- [Near misses](#near-misses)
- [Limits](#limits)

## A complete example

```json
{
  "name": "create invoice",
  "priority": 3,
  "request": {
    "method": "POST",
    "urlPathTemplate": "/api/{version}/invoices",
    "pathParams": {"version": {"matches": "v[12]"}},
    "query": {"tenant": {"equalTo": "acme"}, "debug": {"absent": true}},
    "headers": {"content-type": {"contains": "json"}, "x-trace-id": {}},
    "cookies": {"session": {"matches": "s-[0-9]+"}},
    "host": {"equalTo": "billing.example.com"},
    "scheme": "https",
    "body": [
      {"jsonPath": "$.amount", "value": {"matches": "[0-9]+(\\.[0-9]{2})?"}},
      {"equalToJson": {"currency": "EUR", "lines": "${json-unit.ignore}"}, "ignoreExtraElements": true}
    ]
  },
  "response": {"status": 201, "jsonBody": {"id": "{{uuid}}"}}
}
```

```sh
curl -X POST http://localhost:9090/__admin/stubs \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d @invoice-stub.json
```

## Method and URL

| Field | Matches |
|---|---|
| `method` | The HTTP method, case-insensitive. `ANY` or no method matches every method. |
| `url` | Path **and** query string exactly, as sent: `/search?q=a%20b&page=2`. Parameter order matters. |
| `urlPattern` | An anchored regular expression over path and query string: `/search\\?q=.*`. |
| `urlPath` | The path exactly; the query string is ignored. |
| `urlPathPattern` | An anchored regular expression over the path. |
| `urlPathTemplate` | A path template: `/users/{id}`, `/report.{format}`, `/v1/{name}:cancel`, `/files/{rest...}`. |
| `pathParams` | String matchers for the template parameters; needs `urlPathTemplate`. |

Use at most one of `url`, `urlPattern`, `urlPath`, `urlPathPattern` and `urlPathTemplate`. Paths are
percent-decoded before matching, except `%2F`, which stays encoded so it cannot create a new path segment. For `url`
and `urlPattern` the query string is used exactly as the client sent it (not decoded, not reordered).

Regular expressions are anchored: `"urlPathPattern": "/a/\\d+"` matches `/a/12` and not `/x/a/12`. They use the
Rust `regex` syntax (no look-around, no back-references), which guarantees linear-time matching.

## Query parameters, headers and cookies

`query`, `headers` and `cookies` map a name to a [string matcher](#string-matchers).

```json
{
  "request": {
    "query": {"id": {"hasExactly": [{"equalTo": "1"}, {"equalTo": "2"}]}, "sort": {"equalTo": "asc"}},
    "headers": {"accept": {"contains": "json"}, "authorization": {"absent": true}},
    "cookies": {"session": {"matches": "s-[0-9]+"}, "theme": {"or": [{"absent": true}, {"equalTo": "dark"}]}}
  }
}
```

- Header names are case-insensitive; query parameter and cookie names are case-sensitive.
- A name can appear several times (`?id=1&id=2`, two `accept` lines, two cookies with the same name). A single-value
  matcher passes when **any** value passes; `absent` passes when there is no value at all; the multi-value
  matchers `hasExactly` and `includes` see all values (see [Multi-value matchers](#multi-value-matchers)).
- Cookies are read from every `Cookie` header (`a=1; b=2`); surrounding quotes of a value are removed.
- An empty matcher `{}` means "present with any value".

## Host, port, scheme and basic authentication

```json
{
  "request": {
    "host": {"equalTo": "api.example.com"},
    "port": 8443,
    "scheme": "https",
    "basicAuth": {"username": "partner", "password": "s3cret"}
  }
}
```

| Field | Matches |
|---|---|
| `host` | A string matcher on the host name from the `Host` header (HTTP/1.1) or `:authority` (HTTP/2), without the port, compared case-insensitively. |
| `port` | The port from `Host`/`:authority`; when the client sent none, the data port FastMock listens on (`FASTMOCK_LISTEN`), else 80 or 443 by scheme. |
| `scheme` | `http` or `https`. The data port is `https` when `FASTMOCK_TLS_CERT_FILE` and `FASTMOCK_TLS_KEY_FILE` are set. |
| `basicAuth` | `Authorization: Basic ...` with exactly this user name and password. |

`basicAuth` stores the password in the stub. Viewers see stubs too, so use a test-only credential.

## Form parameters

`formParameters` matches fields of an `application/x-www-form-urlencoded` body. Bodies with any other content type
have no form parameters, so every non-`absent` matcher fails.

```json
{
  "request": {
    "method": "POST",
    "urlPath": "/oauth/token",
    "formParameters": {
      "grant_type": {"equalTo": "client_credentials"},
      "scope": {"includes": [{"equalTo": "read"}, {"equalTo": "write"}]}
    }
  },
  "response": {"jsonBody": {"access_token": "{{randomValue length=32}}", "token_type": "Bearer"}}
}
```

```sh
curl -X POST localhost:8080/oauth/token -d 'grant_type=client_credentials&scope=read&scope=write'
```

Names and values are URL-decoded (`+` is a space). At most 10,000 fields are read from one body.

## Multipart parts

`multipart` matches the parts of a `multipart/*` body (usually `multipart/form-data`).

```json
{
  "request": {
    "method": "POST",
    "urlPath": "/documents",
    "multipartMatchingType": "ANY",
    "multipart": [
      {"name": "meta", "body": [{"jsonPath": "$.kind", "value": {"equalTo": "invoice"}}]},
      {"filename": {"matches": ".*\\.pdf"}, "headers": {"content-type": {"equalTo": "application/pdf"}}, "body": [{"contains": "%PDF"}]}
    ]
  },
  "response": {"status": 201}
}
```

| Field | Meaning |
|---|---|
| `name` | The `name` of the part's `Content-Disposition`, exactly. |
| `filename` | String matcher on the `filename` of `Content-Disposition`. |
| `headers` | String matchers on the part's own headers (names case-insensitive). |
| `body` | [Body matchers](#body-matchers) applied to the part's content. |

`multipartMatchingType` decides how patterns and parts combine:

- `ANY` (default): every pattern must match at least one part.
- `ALL`: every part must match every pattern (and the body must have at least one part).

A request that is not multipart, has no boundary, is malformed or has more than 64 parts matches no multipart
pattern. A stub may have at most 16 multipart patterns.

```sh
curl -X POST localhost:8080/documents \
  -F 'meta={"kind":"invoice"};type=application/json' -F 'file=@invoice.pdf;type=application/pdf'
```

## String matchers

A string matcher is an object; every field that is set must hold.

| Field | Meaning |
|---|---|
| `equalTo` | The value equals the text. |
| `contains` | The value contains the text. |
| `matches` | The value matches the anchored regular expression. |
| `doesNotMatch` | The value does not match the anchored regular expression. |
| `caseInsensitive` | `true` makes `equalTo`, `contains`, `matches` and `doesNotMatch` ignore case. |
| `absent` | `true`: the value must be missing. `false`: it must be present. |
| `and` | A list of string matchers that must all hold. |
| `or` | A list of string matchers of which at least one must hold. |
| `not` | A string matcher that must not hold. |
| `before`, `after`, `equalToDateTime` | Date-time comparisons, see below. |
| `actualFormat`, `truncateExpected`, `truncateActual` | Options of the date-time comparisons. |
| `hasExactly`, `includes`, `allValues` | Multi-value matchers, see below. |

`{}` means "present". A missing value only satisfies `absent: true`, or a matcher made only of `and`/`or`/`not`
whose logic accepts a missing value: `{"or": [{"absent": true}, {"equalTo": "1"}]}` means "missing or 1", and
`{"not": {"equalTo": "x"}}` also accepts a missing value.

### Combining matchers

```json
{
  "headers": {
    "x-client": {
      "or": [
        {"equalTo": "mobile"},
        {"and": [{"matches": "web-.*"}, {"not": {"contains": "beta"}}]}
      ]
    }
  }
}
```

Nesting is limited to 8 levels, and a stub may contain at most 256 matcher nodes in total (every string matcher,
nested matcher, body matcher and multipart pattern counts as one node).

### Date-time matchers

`before`, `after` and `equalToDateTime` compare the value as a point in time with an expected time. The expected
time is either an ISO-8601 date-time, `now`, or one of them followed by an offset:

| Expected | Meaning |
|---|---|
| `2026-06-01T00:00:00Z` | A fixed instant (a time without zone is UTC; a date alone is midnight UTC). |
| `now` | The time the request is matched. |
| `now +3 days`, `now -1 hours`, `now +30 minutes` | Offset from now. Units: `seconds`, `minutes`, `hours`, `days`, `weeks`, `months`, `years` (singular too; also `ms`, `s`, `h`, `d`). |
| `2026-06-01T00:00:00Z +2 hours` | Offset from a fixed instant. |

```json
{
  "query": {"from": {"after": "now -30 days", "before": "now"}},
  "headers": {"if-modified-since": {"before": "2026-01-01T00:00:00Z"}},
  "body": [{"jsonPath": "$.expires", "value": {"after": "now +1 days", "truncateActual": "first day of month"}}]
}
```

Without `actualFormat` the value may be RFC 3339 / ISO-8601 (with or without zone, with `T` or a space), an HTTP
date (`Tue, 15 Nov 1994 08:12:31 GMT`) or a plain date. `actualFormat` parses a different format:

| `actualFormat` | Value |
|---|---|
| `unix` or `epoch` | Seconds since 1970-01-01 UTC. |
| `epochMillis` | Milliseconds since 1970-01-01 UTC. |
| A Java pattern: `dd/MM/yyyy`, `yyyy-MM-dd'T'HH:mm:ssXXX` | Letters `y M d D H h k m s S E a Z X x z w e c`; text in `'...'`. |
| A strftime pattern (contains `%`): `%d.%m.%Y %H:%M` | As in `chrono`. |

`truncateExpected` and `truncateActual` round the expected or the actual time down before comparing: `first second
of minute`, `first minute of hour`, `first hour of day`, `first day of month`, `first day of next month`, `last day of
month`, `first day of year`, `first day of next year`, `last day of year` (the result is midnight UTC of that day;
the `FIRST_DAY_OF_MONTH` spelling is accepted too). Example: "issued this month" is
`{"equalToDateTime": "now", "truncateExpected": "first day of month", "truncateActual": "first day of month"}`.

A value that cannot be parsed never matches. `actualFormat`/`truncate*` without a date-time comparison, an unknown
unit and an unparsable expected time are rejected when the stub is saved.

### Multi-value matchers

`hasExactly`, `includes` and `allValues` see every value of a repeated query parameter, header, cookie or form
field (and every result of a `jsonPath` or `xpath` body matcher):

- `includes: [m1, m2]`: each matcher matches at least one value; other values are allowed.
- `hasExactly: [m1, m2]`: there are exactly as many values as matchers and each value is matched by a different
  matcher (any order).
- `allValues: [m1, m2]`: there is at least one value and every value is matched by at least one of the matchers
  (MockServer `keyMatchStyle: MATCHING_KEY`): `{"allValues": [{"matches": "[0-9]"}]}` accepts `?id=1&id=2` and
  rejects `?id=1&id=x`.

```json
{"query": {"tag": {"includes": [{"equalTo": "red"}]}, "id": {"hasExactly": [{"equalTo": "1"}, {"matches": "[0-9]+"}]}}}
```

`?tag=blue&tag=red&id=7&id=1` matches; `?tag=red&id=1&id=2&id=3` does not (three ids). A multi-value matcher cannot be
combined with other fields in the same object, and its list must not be empty.

## Body matchers

`body` is a list; every entry must match. Each entry sets exactly one of the fields below, plus the options that
belong to it and `negate` (`true` inverts the result).

| Field | Matches |
|---|---|
| `equalTo` | The body text equals the string. |
| `contains` | The body text contains the string. |
| `matches` | The body text matches the anchored regular expression (use `(?s)` to let `.` cross lines). |
| `equalToJson` | The JSON body equals this JSON value; options `ignoreExtraElements`, `ignoreExtraArrayItems`, `ignoreArrayOrder`, `enablePlaceholders`. |
| `jsonPath` | The JSONPath selects something; with `value`, a string matcher must accept one of the results; with `valueBody`, a body matcher must accept one of them. Option `jsonPathDialect`. |
| `jsonSchema` | The JSON body is valid against this JSON Schema (formats are checked). |
| `xpath` | The XPath selects something; with `value` a string matcher (or with `valueBody` a body matcher) must accept one of the results; option `xpathNamespaces`. |
| `equalToXml` | The XML body is semantically equal; options `exemptedComparisons`, `enablePlaceholders`. |
| `binaryEqualTo` | The raw body bytes equal this base64 value. |
| `absent` | `true`: the body is empty. |

### equalToJson

Numbers compare by value (`6` equals `6.0`; big integers compare exactly). By default objects must have the same
keys and arrays the same elements in the same order.

- `ignoreExtraElements: true`: the request may have more object keys than expected (arrays keep their length).
- `ignoreExtraArrayItems: true`: an array of the request may have more elements than expected. In order, the
  expected elements must be a prefix of the actual ones; with `ignoreArrayOrder`, every expected element needs its
  own matching actual element. The WireMock importer sets it together with `ignoreExtraElements`, as WireMock does;
  MockServer's default `ONLY_MATCHING_FIELDS` sets both and `ignoreArrayOrder`.
- `ignoreArrayOrder: true`: array elements may come in any order (lengths must match unless
  `ignoreExtraArrayItems` is set).
- Placeholders (on unless `enablePlaceholders: false`) stand for values in the expected JSON:

| Placeholder | Accepts |
|---|---|
| `${json-unit.any-string}` | any string |
| `${json-unit.any-number}` | any number |
| `${json-unit.any-boolean}` | `true` or `false` |
| `${json-unit.ignore}` | any value, but the key must be present |
| `${json-unit.ignore-element}` | any value, and the key may be missing |
| `${json-unit.regex}[A-Z]{2}\\d+` | a string (or number/boolean as text) matching the anchored regex |

```json
{
  "body": [{
    "equalToJson": {"id": "${json-unit.any-number}", "code": "${json-unit.regex}[A-Z]{3}", "tags": ["b", "a"], "meta": "${json-unit.ignore}"},
    "ignoreArrayOrder": true
  }]
}
```

`equalToJson` and `jsonSchema` need a JSON body of at most 200,000 JSON nodes; larger or invalid bodies do not match.

### jsonPath

The expression follows RFC 9535 (`$.items[?@.qty > 1].sku`, `length(@.tags) == 2`, `match(@.name, 'Req.*')`). With
`value`, a string matcher must accept one of the selected values (objects and arrays as JSON text); with `value`
`{"absent": true}`, the path must select nothing. With `valueBody`, a body matcher (`equalToJson`, `jsonSchema`,
`equalToXml`, ...) must accept one of the selected values; with several results, the array of all of them is tried
too:

```json
{"body": [{"jsonPath": "$.items[0]", "valueBody": {"equalToJson": {"sku": "A"}, "ignoreExtraElements": true}}]}
```

`jsonPathDialect: "jayway"` evaluates the expression the way WireMock and MockServer (Jayway JsonPath) do; their
importers set it. It accepts `[?(@.name =~ /Req.*/i)]`, `[?(@.status in ['A','B'])]`, `nin`, `size`,
`empty true|false`, `@.items.length()` and `.size()` inside filters, the trailing functions `length()`, `size()`,
`min()`, `max()`, `avg()`, `sum()`, `stddev()`, `keys()`, `first()`, `last()` (`$.items.length()` with
`value: {"equalTo": "2"}`), member names with `-`, and a filter applied to an object tests the object itself
(`$.order[?(@.qty > 1)]` matches `{"order": {"qty": 2}}`). `contains`, `subsetof`, `anyof` and `noneof` are not
supported: the stub is rejected, and an import skips that mapping with a warning that names it.

### xpath

XPath 1.0 location paths: `/a/b`, `//b`, `*`, `@attr`, `text()`, predicates `[n]`, `[@a='v']`, `[child='v']`,
`[@a]`. Without `xpathNamespaces` names are compared by local name, so prefixes in the expression and the document
do not matter. With `xpathNamespaces` a prefixed step only matches elements in that namespace:

```json
{
  "body": [{
    "xpath": "//b:msisdn",
    "xpathNamespaces": {"b": "urn:balance"},
    "value": {"matches": "380[0-9]{9}"}
  }]
}
```

An unknown prefix is rejected when the stub is saved. Documents with a DTD are refused.

### equalToXml

The request body is compared with the expected document by structure, not by text:

- whitespace around text and between elements, comments, processing instructions and the XML declaration are ignored;
- attribute order and namespace prefixes do not matter (namespace URIs do);
- child elements are compared in order.

```json
{
  "body": [{
    "equalToXml": "<o:order xmlns:o=\"urn:orders\" id=\"${xmlunit.ignore}\"><o:item>${xmlunit.matchesRegex([A-Z]{3}-[0-9]+)}</o:item></o:order>",
    "exemptedComparisons": ["CHILD_NODELIST_SEQUENCE"]
  }]
}
```

Placeholders (on unless `enablePlaceholders: false`) can be a whole attribute value or a whole text node:
`${xmlunit.ignore}`, `${xmlunit.isNumber}`, `${xmlunit.isDateTime}`, `${xmlunit.matchesRegex(<regex>)}`.

`exemptedComparisons` switches comparisons off:

| Value | Effect |
|---|---|
| `CHILD_NODELIST_SEQUENCE` | Child elements may come in any order. |
| `CHILD_NODELIST_LENGTH`, `CHILD_LOOKUP` | The request may have extra child nodes. |
| `ELEMENT_TAG_NAME` | Element names are not compared. |
| `NAMESPACE_URI` | Namespace URIs of elements and attributes are not compared. |
| `ATTR_VALUE` | Attribute values are not compared. |
| `ATTR_NAME_LOOKUP`, `ELEMENT_NUM_ATTRIBUTES` | The request may have extra or missing attributes. |
| `TEXT_VALUE` | Text content is not compared. |
| `SCHEMA_LOCATION`, `NO_NAMESPACE_SCHEMA_LOCATION` | `xsi:schemaLocation` / `xsi:noNamespaceSchemaLocation` are ignored. |
| `NAMESPACE_PREFIX`, `XML_VERSION`, `XML_STANDALONE`, `XML_ENCODING`, `HAS_DOCTYPE_DECLARATION`, `DOCTYPE_NAME`, `DOCTYPE_PUBLIC_ID`, `DOCTYPE_SYSTEM_ID`, `COMMENT_VALUE`, `PROCESSING_INSTRUCTION_TARGET`, `PROCESSING_INSTRUCTION_DATA`, `NODE_TYPE` | Accepted; these are never compared anyway. |

Unknown names, invalid expected XML and more than 64 regex placeholders are rejected when the stub is saved. Both
documents are limited to 100,000 nodes and 256 levels; deeper or bigger request bodies do not match. Unordered
comparison (`CHILD_NODELIST_SEQUENCE`) is used for up to 10,000 element pairs per level and falls back to ordered
comparison beyond that; one comparison visits at most 500,000 elements, a bigger comparison does not match.

### binaryEqualTo and absent

```json
{"body": [{"binaryEqualTo": "AJ+Slg=="}]}
{"method": "GET", "body": [{"absent": true}]}
```

## GraphQL

`graphql` matches GraphQL-over-HTTP requests with a JSON body `{"query": ..., "operationName": ...}`:

| Field | Matches |
|---|---|
| `operationName` | String matcher on `operationName`. |
| `query` | String matcher on the query text. |
| `rootField` | String matcher on the root fields of the selected operation (any root field may match; `includes`/`hasExactly` see all of them). |

## gRPC messages

A gRPC call is matched as `POST /<package>.<Service>/<Method>` with its request message as a JSON body; for
client-streaming and bidirectional calls the body is the JSON array of the messages. `grpc` adds matchers over the
messages: `messageCount` (`equalTo`, `min`, `max`), `everyMessage`, `anyMessage` and `lastMessage` (body matchers
applied to one message):

```json
{"urlPath": "/shop.v1.OrderService/UploadItems",
 "grpc": {"messageCount": {"min": 1}, "lastMessage": [{"jsonPath": "$.sku", "value": {"equalTo": "END"}}]}}
```

See [gRPC](grpc.md#4-client-streaming).

## MCP calls

`mcp` matches Model Context Protocol calls (JSON-RPC over streamable HTTP) and turns the stub's path into a mock
MCP server: `{"tool": "get_forecast", "arguments": [{"jsonPath": "$.city", "value": {"equalTo": "Kyiv"}}]}`
matches `tools/call` of that tool with those arguments, `resource` matches `resources/read`, `method` any other
JSON-RPC method. See [Mock MCP servers](mock-mcp-servers.md).

## Which stub wins

1. Stubs that are disabled, expired (`expiresAt`, `ttlSeconds`) or used up (`times`) are skipped; see
   [Stub lifecycle](stub-lifecycle.md).
2. Lower `priority` first (default 5).
3. At equal priority, stubs with an `order` come first, lower `order` first, and their URL condition is not compared.
   The WireMock and MockServer importers set `order` to reproduce those tools
   ([Stub lifecycle](stub-lifecycle.md#identity-labels-and-ordering)); you can set it on any stub.
4. For stubs without `order`, the more specific URL condition first: `urlPath` and `url`; then `urlPathTemplate` with
   fewer parameters; then templates with `{rest...}`; then `urlPathPattern` and `urlPattern`; then no URL condition.
5. Then the most recently changed stub.

Scenario state (`requiredState`) is checked after the request matched; a stub in the wrong state lets the next stub
try. The full order is evaluated for every request.

## Near misses

Unmatched requests get `404 {"error": "no stub matched", ...}`; with `FASTMOCK_UNMATCHED_DETAILS=true` the body
includes the three closest stubs. The admin API explains any request on demand:

```sh
curl -X POST http://localhost:9090/__admin/requests/near-misses \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"method": "POST", "path": "/login", "headers": {"cookie": "session=x"}, "body": "user=ann"}'
```

```json
{
  "nearMisses": [{
    "stubId": "0193f3a4-6a43-7c1b-9d0e-9c4f6b1d2e11",
    "name": "login",
    "score": 0.6,
    "mismatches": ["cookie `session` does not match", "form parameter `user` does not match"]
  }]
}
```

The score is the share of conditions that hold: method counts 1, the URL 2, and every scheme, port, host, basic
authentication, path parameter, query parameter, header, cookie, form parameter, multipart block, body matcher and
GraphQL root field 1.

## Limits

| Limit | Value | When exceeded |
|---|---|---|
| Matchers per stub (query, headers, cookies, path params, form fields, body matchers, multipart pattern contents, host, basic auth, GraphQL root field) | 64 | stub rejected |
| Matcher nodes per stub, including nested `and`/`or`/`not`/`hasExactly`/`includes` | 256 | stub rejected |
| Nesting depth of string matchers | 8 | stub rejected |
| Multipart patterns per stub | 16 | stub rejected |
| Compiled size of one regular expression | 256 KiB | stub rejected |
| Regex placeholders per `equalToJson` / `equalToXml` | 64 | stub rejected |
| `xpathNamespaces` entries | 32 | stub rejected |
| JSON nodes parsed from a request body | 200,000 | JSON matchers do not match |
| XML nodes / depth | 100,000 / 256 | XML matchers do not match |
| Text collected by one XPath expression | 2 MiB over all results | the rest of the text is not collected |
| Element comparisons in one `equalToJson` with `ignoreArrayOrder` | 2,000,000 | the body does not match |
| Element comparisons in one `equalToXml` | 500,000 | the body does not match |
| Parts read from a multipart body | 64 | multipart patterns do not match |
| Cookies read from a request | 256 | the rest is ignored |
| Form fields read from a body | 10,000 | the rest is ignored |

Stubs that break a rule are rejected by `POST /__admin/stubs`, `PUT /__admin/stubs/{id}` and
`POST /__admin/stubs/validate` with `400 {"error": "..."}` naming the problem.
