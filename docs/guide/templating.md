# Templating

Response headers and bodies, `bodyFileName`, SSE events, callbacks, Kafka messages and scenario names are
[Handlebars](https://handlebarsjs.com/guide/) templates. Templates can read the request and call helpers; they
cannot read files, environment variables or the network, cannot run code, and every render is bounded in size.

- [The template context](#the-template-context)
- [Typed JSON values](#typed-json-values)
- [Conditions and loops](#conditions-and-loops)
- [Helper reference](#helper-reference)
- [Fake data](#fake-data)
- [Limits and errors](#limits-and-errors)

Set `"templated": false` on a response to send `{{` literally.

## The template context

| Path | Value |
|---|---|
| `request.method` | `GET`, `POST`, ... |
| `request.url` | Path and query string as sent: `/search?q=a%20b` |
| `request.path` | Percent-decoded path: `/users/7` |
| `request.pathSegments.[0]` | Path segments: `users` |
| `request.pathParams.id` | Parameters of `urlPathTemplate` |
| `request.query.q` | First value of a query parameter |
| `request.queryAll.id.[1]` | All values of a query parameter |
| `request.headers.x-request-id` | First value of a header; names are lower-case (`request.headers.[content-type]`) |
| `request.headersAll.[x-multi].[1]` | All values of a header (lower-case names), for `#each`, `size`, `join` |
| `request.cookies.session` | First value of a cookie |
| `request.formParams.user` | First value of a form field (`application/x-www-form-urlencoded` bodies only) |
| `request.body` | Body as text |
| `request.json.items.[0].sku` | Body parsed as JSON (up to 200,000 nodes; `null` otherwise) |
| `request.scheme`, `request.host`, `request.port` | `https`, `api.example.com`, `8443` (port defaults to 80/443 by scheme) |
| `request.baseUrl` | `https://api.example.com:8443` |
| `state.scenario`, `state.current` | Rendered scenario name and its state before this request (stubs with a `scenario`) |
| `originalRequest` | The same object as `request` (WireMock webhook compatibility); added only to stubs that use it |

```json
{
  "request": {"method": "POST", "urlPathTemplate": "/carts/{cartId}/items"},
  "response": {
    "status": 201,
    "headers": {"location": "{{request.baseUrl}}/carts/{{request.pathParams.cartId}}/items/{{uuid}}"},
    "jsonBody": {
      "cart": "{{request.pathParams.cartId}}",
      "sku": "{{request.json.sku}}",
      "quantity": "{{json request.json.quantity}}",
      "client": "{{default request.headers.x-client 'unknown'}}"
    }
  }
}
```

## Typed JSON values

In `jsonBody` (and callback and Kafka `jsonBody`/`jsonValue`) every string is rendered as text. A string that is
exactly `"{{json <expression>}}"` keeps the JSON type of the value: numbers stay numbers, objects stay objects.

```json
{"jsonBody": {"count": "{{json request.json.count}}", "items": "{{json request.json.items}}", "label": "{{request.json.count}}"}}
```

With the body `{"count": 3, "items": [1, 2]}` this renders `{"count": 3, "items": [1, 2], "label": "3"}`.

## Conditions and loops

The Handlebars built-ins `if`, `unless`, `each`, `with`, `lookup` and `len` are available. The comparison helpers
below work both as sub-expressions and as blocks:

```handlebars
{{#if (eq request.query.lang 'uk')}}Привіт{{else}}Hello{{/if}}
{{#eq request.method 'POST'}}created{{else}}read{{/eq}}
{{#each request.queryAll.id}}{{this}}{{#unless @last}},{{/unless}}{{/each}}
{{#contains request.headers.accept 'xml'}}<ok/>{{else}}{"ok": true}{{/contains}}
```

Partials (`{{> name}}`) and decorators (`{{#* inline}}`) are rejected.

## Helper reference

Arguments are positional unless shown as `name=value`. Strings are quoted with `'` or `"`; there are no escape
sequences inside literals, so put text with backslashes or line breaks into the request or into a body file.

### Request data

| Helper | Example | Result |
|---|---|---|
| `jsonPath value path` | `{{jsonPath request.body '$.items[0].sku'}}` | First JSONPath result; `value` may be a JSON string. Objects and arrays render as JSON text (`{"k":1}`), as in WireMock; inside `#each` or a sub-expression they stay JSON values |
| `xPath xml path` | `{{xPath request.body '/order/id'}}` | Text of the selected node; an array when several match, null when none |
| `formData text [var] urlDecode=true` | `{{formData request.body 'form' urlDecode=true}}{{form.user}}` | Parses `a=1&b=2`; with a variable name the object is stored in that variable, otherwise it is returned: `{{lookup (formData request.body) 'user'}}` |
| `val value default=x` / `or=x` | `{{val request.query.page default='1'}}` | The value, or the default when it is missing (null) |
| `default value fallback` | `{{default request.query.page '1'}}` | The fallback when the value is missing or an empty string |
| `size value` | `{{size request.queryAll.id}}` | Length of a string (characters), array or object |

`formData` cannot overwrite `request`, `originalRequest` or `state`; variable names are `[A-Za-z0-9_]`, at most 64
characters.

### Strings

| Helper | Example | Result |
|---|---|---|
| `upper` / `lower` | `{{upper 'abc'}}` | `ABC` |
| `capitalize s [fully=true]` | `{{capitalize 'hello wORLD'}}` / `fully=true` | `Hello WORLD` / `Hello World` |
| `capitalizeFirst s` | `{{capitalizeFirst 'abc def'}}` | `Abc def` |
| `trim s` | `{{trim '  a b  '}}` | `a b` |
| `abbreviate s width` | `{{abbreviate 'Hello world' 8}}` | `Hello...` (width at least 4) |
| `replace s from to` | `{{replace 'a-b-c' '-' '+'}}` | `a+b+c` |
| `concat a b ...` | `{{concat 'order-' request.pathParams.id}}` | `order-7` |
| `substring s start [end]` | `{{substring '380501234567' 0 3}}` | `380` (character positions) |
| `split s separator` | `{{#each (split 'a,b,c' ',')}}[{{this}}]{{/each}}` | `[a][b][c]` |
| `join separator array` / `join separator a b ...` | `{{join ', ' (array 'a' 'b') prefix='[' suffix=']'}}` | `[a, b]` |
| `regexExtract s regex [group]` | `{{regexExtract request.path '/users/([0-9]+)' 1}}` | `7` |
| `urlEncode s [decode=true]` | `{{urlEncode 'a b&c'}}` / `{{urlEncode 'a+b%26c' decode=true}}` | `a+b%26c` / `a b&c` |
| `base64 s [decode=true] [padding=false]` | `{{base64 'hi'}}` / `{{base64 'aGk=' decode=true}}` | `aGk=` / `hi` |
| `base64Decode s` | `{{base64Decode 'aGk='}}` | `hi` |
| `sha256 s` | `{{sha256 request.body}}` | hex digest |
| `hmac algorithm key data [encoding='base64']` | `{{hmac 'sha256' 'key' request.body}}` | hex (or base64) HMAC; `sha256` or `sha512` |

### Comparisons and logic

All return `true`/`false` and can be used as blocks (`{{#eq a b}}...{{else}}...{{/eq}}`).

| Helper | Example | Notes |
|---|---|---|
| `eq a b`, `ne a b` | `{{#if (eq request.method 'GET')}}` | Values that both look like numbers compare as numbers (`'10'` equals `10.0`); otherwise as text |
| `gt`, `gte`, `lt`, `lte` | `{{#if (gt request.json.amount 1000)}}` | Numeric when both are numbers or numeric strings, else text order |
| `and a b ...`, `or a b ...`, `not a` | `{{#if (and (eq a 1) (not b))}}` | Falsy: null, false, 0, empty string, empty array |
| `contains haystack needle` | `{{#if (contains request.headers.accept 'json')}}` | Substring of a string, element of an array, or key of an object |
| `matches s regex` | `{{#matches request.query.id '[0-9]+'}}num{{/matches}}` | Anchored regular expression |

### Numbers and random values

| Helper | Example | Result |
|---|---|---|
| `math a op b` | `{{math request.json.qty '*' 2}}` | `+ - * / %`, `//` (integer division), `^` or `pow`, `min`, `max`; division by zero is an error |
| `randomInt lower upper` | `{{randomInt 1 6}}` or `{{randomInt lower=1 upper=6}}` | Integer, both bounds inclusive |
| `randomDecimal lower=x upper=y` | `{{randomDecimal lower=0.5 upper=2.5}}` | Float in `[lower, upper)` |
| `randomDigits n` | `{{randomDigits 6}}` | `048213` (at most 256 digits) |
| `randomValue length=n type=T [uppercase=true]` | `{{randomValue length=12 type='HEXADECIMAL' uppercase=true}}` | Types `ALPHANUMERIC` (default), `ALPHABETIC`, `NUMERIC`, `HEXADECIMAL`, `ALPHANUMERIC_AND_SYMBOLS`, `UUID`; length at most 4096 (default 36) |
| `uuid` | `{{uuid}}` | Random UUID v4 |
| `pickRandom a b c` / `pickRandom array` | `{{pickRandom 'red' 'green'}}` | One of the values |
| `array a b ...` | `{{array 1 'two' true}}` | A JSON array, for `join`, `each`, `jsonArrayAdd`, ... |

### Dates and times

Dates are passed between helpers as ISO-8601 strings. `format` accepts `iso` (default, milliseconds, UTC `Z`),
`isoSeconds` (no fraction of a second, as WireMock's default), `isoNanos`, `epoch` or `unix` (seconds, a number), `epochMillis` (a number), a Java pattern such as `yyyy-MM-dd'T'HH:mm:ss`
(`y M d D H h k m s S E a Z X x z w e c`, text in `'...'`), or a strftime pattern containing `%` such as `%d.%m.%Y`.
`offset` is `<amount> <unit>` (`3 days`, `-24 seconds`, `1 months`; units as in
[date-time matchers](request-matching.md#date-time-matchers)). `timezone` is `UTC`, `GMT`, `Z` or a fixed offset
(`+02:00`, `-0530`, `UTC+3`); region names such as `Europe/Kyiv` are not supported because their rules change over time.

| Helper | Example | Result |
|---|---|---|
| `now [offset=] [offsetSeconds=] [format=] [timezone=]` | `{{now offset='1 days' format='yyyy-MM-dd'}}` | Tomorrow's date |
| `date value [offset=] [format=] [timezone=]` | `{{date request.json.created offset='-1 hours' timezone='+02:00' format='HH:mm'}}` | Shifted and formatted; `value` is an ISO string or epoch seconds (milliseconds when larger than 10^11) |
| `parseDate text [format=]` | `{{parseDate '10/11/2021' format='dd/MM/yyyy'}}` | `2021-11-10T00:00:00.000Z`; null when it does not parse |
| `dateFormat value [format]` | `{{dateFormat (parseDate request.query.d) 'long'}}` | Named styles `full`, `long`, `medium`, `short`, or any format above |

```handlebars
{{date (parseDate request.headers.[if-modified-since]) offset='1 days' format='EEE, dd MMM yyyy HH:mm:ss' }} GMT
```

### JSON and XML

The JSON helpers accept JSON values or JSON text and return compact JSON text, so they nest:
`{{jsonPath (jsonMerge a b) '$.x'}}`. In a `jsonBody`, use `"{{json (parseJson (jsonMerge a b))}}"` to insert the
result as an object.

| Helper | Example | Result |
|---|---|---|
| `toJson value` | `{{toJson request.json.items}}` | JSON text |
| `parseJson text` | `{{json (parseJson request.query.filter)}}` | JSON value (null when invalid) |
| `jsonMerge a b [removeNulls=true]` | `{{jsonMerge '{"a":1,"b":{"c":2}}' '{"b":{"d":3}}'}}` | `{"a":1,"b":{"c":2,"d":3}}` (deep merge of objects; `removeNulls` drops keys whose value is null) |
| `jsonRemove json path` | `{{jsonRemove request.body '$.password'}}` | The document without the JSONPath matches |
| `jsonArrayAdd array item [flatten=true] [maxItems=n]` | `{{jsonArrayAdd '[1,2]' '[3,4]' flatten=true maxItems=3}}` | `[2,3,4]` (`maxItems` keeps the last n) |
| `formatJson json [format='compact']` | `{{formatJson request.body}}` | Pretty-printed (two spaces) or compact |
| `formatXml xml [format='compact']` | `{{formatXml request.body}}` | Pretty-printed (two spaces) or compact XML; no DTDs |

## Fake data

Postman/Microcks-style generators for realistic test data. Without `seed` every call is random; with
`seed=<any value>` the result is the same every time for that seed (and differs between helpers), which makes
responses reproducible: `{{randomFullName seed=request.pathParams.id}}` always gives the same name for the same id.

| Helper | Example result |
|---|---|
| `randomFirstName`, `randomLastName`, `randomFullName` | `Olivia`, `Garcia`, `Liam Walker` |
| `randomUserName` | `olivia512` |
| `randomEmail` | `liam.walker@example.org` |
| `randomPhoneNumber` | `+1-415-555-0172` |
| `randomCity`, `randomCountry`, `randomCountryCode` | `Riverside`, `Poland`, `PL` |
| `randomStreetAddress` | `742 Maple Avenue` |
| `randomCompanyName`, `randomJobTitle` | `Globex Labs`, `Architect` |
| `randomWord`, `randomColor` | `tango`, `teal` |
| `randomBoolean` | `true` |
| `randomAlphaNumeric [length=10]` | `aZ3k9QpL0x` |
| `randomIP`, `randomIPv6` | `83.14.201.7`, `2a1:9f:...` |
| `randomUUID` | `5b1c...` (UUID v4 layout) |
| `randomUrl` | `https://example.com/delta` |
| `randomDateTime [days=365]` | `2026-03-02T11:20:45Z` (within ±`days` of now) |

Email and URL domains are always `example.com`, `example.org` or `example.net`.

```json
{
  "request": {"method": "GET", "urlPathTemplate": "/customers/{id}"},
  "response": {
    "jsonBody": {
      "id": "{{request.pathParams.id}}",
      "name": "{{randomFullName seed=request.pathParams.id}}",
      "email": "{{randomEmail seed=request.pathParams.id}}",
      "address": {"street": "{{randomStreetAddress seed=request.pathParams.id}}", "city": "{{randomCity seed=request.pathParams.id}}"},
      "active": "{{json (randomBoolean)}}"
    }
  }
}
```

## Limits and errors

| Limit | Value |
|---|---|
| Template size | 256 KiB |
| Rendered output | 8 MiB |
| Values produced by helpers during one render | 32 MiB (each helper result, each JSON text parsed by a helper and each `formData` variable counts; sizes are checked before a result is built) |
| Request JSON parsed for `request.json` | 200,000 nodes |
| JSON text parsed by `jsonPath`, `parseJson`, `formatJson`, `jsonMerge`, `jsonRemove`, `jsonArrayAdd` | 200,000 nodes, 128 levels; larger text is treated as a plain string (`parseJson` returns `null`) |
| Steps per render (helper calls, `#each` iterations, output writes) | 1,000,000 |
| Render time | 2 seconds |
| Nesting of blocks / subexpressions | 64 / 32 levels (checked when the stub is saved) |
| Text returned by one `xPath` call | 2 MiB in total over all results |
| Generated lengths (`randomValue`, `randomAlphaNumeric`) | 4096 characters |
| Items produced by `split`, `formData`, `jsonArrayAdd` | 10,000 |
| Regular expressions in `regexExtract` and `matches` | 1 MiB compiled |
| `formatXml` input | 100,000 nodes, 256 levels |

Templates are checked when the stub is saved (syntax, size, no partials). Errors that depend on the request (an
invalid date, a bad regular expression in a request value, an exceeded budget, step limit or render time) answer
`500 {"error": "stub evaluation failed"}` and are counted in `fastmock_stub_errors_total`; the server log has the
message under the request's trace id.
