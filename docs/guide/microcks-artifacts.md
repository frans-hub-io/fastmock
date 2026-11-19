# Microcks artifacts

FastMock reads the artifacts that teams already maintain for [Microcks](https://microcks.io): OpenAPI documents with
named examples and `x-microcks-*` extensions, the companion `APIExamples` and `APIMetadata` files, GraphQL schemas,
SoapUI projects and WSDL documents. Every import is recorded in the [service catalog](service-catalog.md), the
counterpart of the Microcks "APIs & Services" page.

- [What is supported](#what-is-supported)
- [OpenAPI with named examples](#openapi-with-named-examples)
- [Dispatchers](#dispatchers)
- [x-microcks extensions](#x-microcks-extensions)
- [APIExamples](#apiexamples)
- [APIMetadata](#apimetadata)
- [Template expressions](#template-expressions)
- [What is never executed](#what-is-never-executed)
- [Metrics and audit](#metrics-and-audit)
- [Limits](#limits)
- [Errors](#errors)

The examples use `FASTMOCK_TOKEN` for an editor token, the admin API on `localhost:9090` and the data plane on
`localhost:8080`. A step-by-step migration is in [Migrating from Microcks](migrating-from-microcks.md).

## What is supported

| Microcks artifact | FastMock endpoint | Page |
|---|---|---|
| OpenAPI 3.x (primary) | `POST /__admin/import/openapi` | this page |
| AsyncAPI 2.x / 3.0 (primary) | `POST /__admin/import/asyncapi` | [AsyncAPI](asyncapi.md) |
| GraphQL SDL (primary) | `POST /__admin/import/graphql` | [GraphQL](graphql.md) |
| SoapUI project (primary) | `POST /__admin/import/soapui` | [SOAP](soap.md) |
| WSDL 1.1 | `POST /__admin/import/wsdl` | [SOAP](soap.md) |
| gRPC descriptor set | `PUT /__admin/grpc/descriptors/{name}` | [gRPC](grpc.md) |
| Postman collection | `POST /__admin/import/postman` | [Admin API](admin-api.md) |
| `APIExamples` (secondary) | `POST /__admin/import/microcks` | this page |
| `APIMetadata` (secondary) | `POST /__admin/import/microcks` | this page |

`POST /__admin/import` detects the format of a document sent as the request body and forwards it to the right
converter.

## OpenAPI with named examples

Microcks pairs request and response fragments by **example name**: an example called `Millefeuille` in a path
parameter, in a query parameter, in a header parameter or in `requestBody.content.<media type>.examples`, and a
response example with the same name under `responses.<code>.content.<media type>.examples`, form one request /
response pair. The status code is the key of the response that holds the example.

FastMock creates, per operation:

- one stub per pair (priority 4) whose request pattern matches the parts that tell the examples apart (see
  [Dispatchers](#dispatchers)) and that answers the example with its status code, media type and the response
  header examples of the same name;
- one fallback stub (priority 5) that answers the **default example**: the example named by a `FALLBACK`
  dispatcher, otherwise the first example with a 2xx status, otherwise the first example;
- with `validateRequests` (the default), the contract-violation stub (priority 6, status 400) as for any OpenAPI
  import.

An operation without named response examples is imported exactly as before: one stub answering the `example`, the
first example or an example generated from the schema.

Response examples of a response without content are attached with `x-microcks-refs`:

```yaml
responses:
  "204":
    description: deleted
    x-microcks-refs: [Eclair Chocolat]
```

Example: a document with three named examples on `GET /pastry/{name}`.

```yaml
openapi: 3.0.3
info: {title: API Pastry - 2.0, version: 2.0.0}
paths:
  /pastry/{name}:
    parameters:
      - name: name
        in: path
        required: true
        schema: {type: string}
        examples:
          Eclair Chocolat: {value: Eclair Chocolat}
          Millefeuille: {value: Millefeuille}
          Unknown: {value: Unknown}
    get:
      operationId: getPastry
      x-microcks-operation:
        delay: 15
        dispatcher: FALLBACK
        dispatcherRules: |-
          {"dispatcher": "URI_PARTS", "dispatcherRules": "name", "fallback": "Unknown"}
      responses:
        "200":
          description: a pastry
          content:
            application/json:
              examples:
                Eclair Chocolat:
                  value: {name: Eclair Chocolat, size: M, price: 2.5, bakedAt: "{{ now(yyyy-MM-dd) }}"}
                Millefeuille:
                  value: {name: Millefeuille, size: L, price: 4.4}
        "404":
          description: unknown pastry
          content:
            application/json:
              examples:
                Unknown:
                  value: {error: "pastry {{ request.path[1] }} not found"}
```

```sh
curl -s -X POST localhost:9090/__admin/import/openapi \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile spec pastry.yaml '{spec: $spec, tag: "pastry", validateRequests: false}')"
```

```json
{"imported": 4, "removed": 0, "warnings": []}
```

```sh
curl -s localhost:8080/pastry/Millefeuille     # {"name":"Millefeuille","size":"L","price":4.4}
curl -s localhost:8080/pastry/Eclair%20Chocolat # {"name":"Eclair Chocolat",...,"bakedAt":"2026-11-19"}
curl -s -i localhost:8080/pastry/Croissant      # HTTP/1.1 404 ... {"error":"pastry Croissant not found"}
```

Each generated stub carries `metadata.operation` with the Microcks operation name (`GET /pastry/{name}`); the
catalog groups stubs by it.

## Dispatchers

The dispatcher decides which parts of a request select an example. FastMock turns each dispatcher into request
matchers at import time, so dispatching costs nothing at request time and the generated stubs are ordinary stubs
that can be inspected, exported and edited.

When `x-microcks-operation.dispatcher` is not set, the dispatcher is inferred like Microcks does: path parameters
with examples give `URI_PARTS`, query parameters give `URI_PARAMS`, both give `URI_ELEMENTS`. An operation with
only request body examples is dispatched on the body: each example stub matches a request whose JSON body
**contains** the example body (`equalToJson` with `ignoreExtraElements`; non-JSON bodies use `equalTo`).

| Microcks dispatcher | `dispatcherRules` | Generated matchers |
|---|---|---|
| `URI_PARTS` | `name && other` | `pathParams.<name>.equalTo` = example value |
| `URI_PARAMS` | `page && size` | `query.<name>.equalTo` = example value |
| `URI_ELEMENTS` | `petId ?? user_key` (path `??` query) | both of the above |
| `QUERY_ARGS` (REST) | `a && b` | `query.<name>.equalTo` |
| `QUERY_HEADER` | `x-tenant && x-api-key` | `headers.<name>.equalTo` (header parameter examples) |
| `JSON_BODY` | `{"exp": "/country", "operator": "equals", "cases": {...}}` | one stub per case with `jsonPath` on the pointer |
| `FALLBACK` | `{"dispatcher": "...", "dispatcherRules": "...", "fallback": "Unknown"}` | the inner dispatcher, and the fallback stub answers the `fallback` example |
| `RANDOM` | none | one stub with every example in `responses` and `responseMode: random` |
| `SCRIPT`, `GROOVY`, `JS` | a script | not executed: warning, the inferred dispatcher is used |
| `PROXY`, `PROXY_FALLBACK` | a URL | not imported: warning, the inferred dispatcher is used; use a stub with `response.proxy` |
| `QUERY_MATCH` | an XPath | warning for REST; used by the [SoapUI import](soap.md) |

`JSON_BODY` operators:

| Operator | Case keys | Matcher |
|---|---|---|
| `equals` | the value | `{"jsonPath": "$['country']", "value": {"equalTo": "Belgium"}}` |
| `regexp` | a regular expression (whole value) | `{"jsonPath": "...", "value": {"matches": "..."}}` |
| `presence` | `found` | `{"jsonPath": "..."}` (the field exists) |
| `range`, `size` | `[min;max]` | not supported: warning, the case is skipped |

The `default` case becomes the fallback stub. Example:

```yaml
post:
  x-microcks-operation:
    dispatcher: JSON_BODY
    dispatcherRules: |-
      {"exp": "/size", "operator": "equals", "cases": {"L": "large", "default": "regular"}}
```

Generated stub for the `L` case:

```json
{
  "name": "createPastry [large]",
  "priority": 4,
  "request": {
    "method": "POST",
    "urlPathTemplate": "/pastry",
    "body": [{"jsonPath": "$['size']", "value": {"equalTo": "L"}}]
  },
  "response": {"status": 201, "headers": {"content-type": "application/json"},
    "jsonBody": {"id": "{{uuid}}", "name": "{{request.json.[name]}}", "note": "large pastries take longer"}},
  "metadata": {"operation": "POST /pastry"}
}
```

An example that has no value for the dispatcher (for instance a response example without a request example of the
same name) is reachable only as the default example and is reported in `warnings`.

## x-microcks extensions

| Extension | Where | Effect |
|---|---|---|
| `x-microcks.labels` | `info` | service labels in the [catalog](service-catalog.md) |
| `x-microcks-operation.delay` | operation | `response.delay.fixedMs` on every stub of the operation (at most 600000 ms) |
| `x-microcks-operation.dispatcher`, `dispatcherRules` | operation | see [Dispatchers](#dispatchers) |
| `x-microcks-refs` | response | attaches the response to the named request examples |

`parameterConstraints` and `triggers` are not imported. Use `validateRequests` for contract checks and
[callbacks](callbacks.md) or [async mocks](asyncapi.md) for triggered messages.

## APIExamples

An `APIExamples` document adds examples to a service that is already imported (the main artifact). The service is
found by `metadata.name` and `metadata.version`, so they must match the title and version of the imported
document (the catalog id is `<name>:<version>` in lower case with spaces as dashes).

```yaml
apiVersion: mocks.microcks.io/v1alpha1
kind: APIExamples
metadata:
  name: API Pastry - 2.0
  version: '2.0.0'
operations:
  'GET /pastry/{name}':
    Croissant:
      request:
        parameters:
          name: Croissant
      response:
        mediaType: application/json
        body:
          name: Croissant
          size: S
          price: 1.2
          servedBy: '{{ randomFirstName() }}'
```

```sh
curl -s -X POST localhost:9090/__admin/import/microcks \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile doc pastry-examples.yaml '{document: $doc}')"
```

```json
{"imported": 1, "removed": 0, "warnings": [], "serviceId": "api-pastry-2.0:2.0.0"}
```

Operations that the imported service does not have are skipped with a warning such as
`operation `GET /pastry` is not part of the service; its examples were skipped`. When no example at all can be
used, the answer is `400` and the examples imported earlier stay in place:

```json
{"error": "the document produced no stubs; nothing was changed; operation `GET /pastry` is not part of the service; its examples were skipped", "details": []}
```

How an example becomes a stub (priority 3, so it wins over the examples of the main artifact):

| Service type | Operation key | Request matching | Response |
|---|---|---|---|
| REST | `'GET /pastry/{name}'` | `parameters` that appear as `{name}` in the path become `pathParams`, the others `query`; `headers` become header matchers (except `accept`, `content-type`, `user-agent`); `body` becomes `equalToJson` with `ignoreExtraElements` | `status` (or `code`), `headers`, `mediaType`, `body` (YAML objects and arrays become `jsonBody`) |
| GRAPHQL | the query or mutation field, for example `film` | `request.body.variables` must be contained in the request variables | `response.body`; a body without `data` is wrapped as `{"data": {<field>: body}}` |
| SOAP | the operation name, for example `sayHello` | up to 16 leaf elements of the request body become XPath matchers (`/Envelope/Body/sayHello/name` = `Andrew`) | `response.body` as XML |
| GRPC | `<package.Service>/<Method>` | `request.body` contained in the decoded request | `response.body` |

The base request pattern (method, path, SOAP action, GraphQL root field) is taken from the stubs of the operation in
the main import. Example stubs carry the tag `<service tag>:examples`; importing the same `APIExamples` again
replaces them, importing the main artifact again keeps them, and deleting the service deletes them. Event
(`eventMessage`) examples are reported as warnings; import them with the [AsyncAPI import](asyncapi.md).

## APIMetadata

```yaml
apiVersion: mocks.microcks.io/v1alpha1
kind: APIMetadata
metadata:
  name: API Pastry - 2.0
  version: 2.0.0
  labels:
    team: pastry
    status: GA
operations:
  'GET /pastry/{name}':
    delay: 25
    dispatcher: FALLBACK
    dispatcherRules: |-
      {"dispatcher": "URI_PARTS", "dispatcherRules": "name", "fallback": "Unknown"}
```

```sh
curl -s -X POST localhost:9090/__admin/import/microcks \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile doc pastry-metadata.yaml '{document: $doc}')"
```

- `labels` are added to the service labels.
- For a service imported from OpenAPI, `delay`, `dispatcher` and `dispatcherRules` override the
  `x-microcks-operation` of the operation and the stubs of the main import are regenerated from the stored
  document. The overrides are stored with the service (`overrides` in the catalog) and are applied again every time
  the OpenAPI document is re-imported, as Microcks keeps them.
- For other services, `delay` is set on the existing stubs of the operation; a `dispatcher` is reported as a
  warning.

Both companion imports accept `"dryRun": true`, which returns the stubs that would be written.

## Template expressions

Microcks templates in examples (`{{ ... }}`, and `${ ... }` in SoapUI responses) are converted into FastMock
Handlebars templates at import time. The stub is marked `templated` only when at least one expression was
converted. An expression that cannot be converted is reported in `warnings` and kept literally: the body is
served as written when nothing in it was converted, and the expression is escaped as `\{{` when other
expressions of the same body were converted, so it is still sent literally.

| Microcks | FastMock |
|---|---|
| `{{ request.body }}` | `{{request.body}}` |
| `{{ request.body/books/1/author }}` (JSON pointer) | `{{request.json.[books].[1].[author]}}` |
| `{{ request.path[1] }}` | `{{request.pathSegments.[1]}}` |
| `{{ request.params[locale] }}` | `{{request.query.[locale]}}` |
| `{{ request.headers[X-Trace] }}` | `{{request.headers.[x-trace]}}` |
| `{{ now() }}`, `{{ timestamp() }}` | `{{now format="epochMillis"}}` |
| `{{ now(dd/MM/yyyy HH:mm:ss) }}` | `{{now format="%d/%m/%Y %H:%M:%S"}}` |
| `{{ now(yyyy-MM-dd, 2d) }}` | `{{now format="%Y-%m-%d" offsetSeconds="172800"}}` |
| `{{ uuid() }}`, `{{ guid() }}`, `{{ randomUUID() }}` | `{{uuid}}` |
| `{{ randomInt() }}` | `{{randomInt -65635 65635}}` |
| `{{ randomInt(10) }}` | `{{randomInt 0 10}}` |
| `{{ randomInt(5, 10) }}` | `{{randomInt 5 10}}` |
| `{{ randomString() }}`, `{{ randomString(64) }}` | `{{randomAlphaNumeric length=32}}`, `{{randomAlphaNumeric length=64}}` |
| `{{ randomValue(a, b, c) }}` | `{{pickRandom "a" "b" "c"}}` |
| `{{ randomBoolean() }}` | `{{pickRandom true false}}` |
| `{{ randomFirstName() }}`, `randomLastName`, `randomFullName`, `randomEmail`, `randomPhoneNumber`, `randomCity`, `randomCountry`, `randomCountryCode`, `randomStreetAddress`, `randomCompanyName` | `{{randomFirstName}}` and so on |
| `{{ request.body/name \|\| 'John' }}` | `{{default request.json.[name] "John"}}` |
| `{{ a \|\| b \|\| randomFirstName() }}` | `{{default a (default b (randomFirstName))}}` |
| Postman variables `{{$randomUUID}}`, `{{$timestamp}}`, `{{$randomInt}}`, `{{$randomEmail}}` ... | `{{uuid}}`, `{{now format="epoch"}}`, `{{randomInt 0 1000}}`, `{{randomEmail}}` ... |

Date patterns use Java `SimpleDateFormat` letters: `yyyy yy MMMM MMM MM M dd d D HH H hh h mm m ss s SSS a EEEE
EEE u z Z X XXX` and quoted literals (`'T'`). Other letters are reported as warnings. Date deltas accept `s`, `m`,
`H`/`h`, `d`, `w`, and `M` (30 days) and `y` (365 days), with a warning for the two approximations.

Kept literally, with a warning:

- XPath forms of `request.body` (`request.body//name`, `request.body/a/b[1]`) and any `request.body/...` in a SOAP
  response; use the FastMock `xPath` helper by hand: `{{xPath request.body "//name"}}`.
- Context functions (`uuid() > put(id)`, `{{ id }}`, `get(...)`).
- Faker functions without a FastMock equivalent (`randomNamePrefix`, `randomNameSuffix`, `randomStreetName`,
  `randomLatitude`, `randomLongitude`) and any unknown function.

## What is never executed

FastMock never runs code from an imported artifact:

- `SCRIPT` dispatchers (Groovy or JavaScript) in `x-microcks-operation`, `APIMetadata` and SoapUI mock operations
  are reported as warnings. The operation is dispatched by the request values of its examples instead (REST), or
  answers its default response (SoapUI). Rewrite the logic as request matchers, a [scenario](scenarios-and-state.md)
  or several stubs with different priorities.
- SoapUI `${= ...}` Groovy property expansions and template expressions that call unknown functions stay literal.
- `externalValue` examples and external XSD schemas (`xsd:import` with `schemaLocation`) are never fetched.
- `PROXY` dispatchers are not turned into outbound calls; create a proxy stub explicitly so that the egress policy
  applies.

## Metrics and audit

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_imports_total` | `format`, `dry_run` | imports that reached the store step (`openapi`, `graphql`, `soapui`, `wsdl`, `microcks`, ...) |
| `fastmock_artifact_imports_total` | `format`, `outcome` (`ok`, `error`) | GraphQL, SoapUI, WSDL and Microcks documents parsed |
| `fastmock_services_recorded_total` | `type` | catalog entries written |
| `fastmock_store_errors_total` | `store="services"` | catalog writes that failed (the import itself succeeded) |

Every import that writes stubs is in the audit log as `import.<format>`; `APIMetadata` also writes
`service.metadata`.

## Limits

| Input | Limit |
|---|---|
| `APIExamples` / `APIMetadata` document | 8 MiB, 5000 examples |
| Examples converted per SOAP request body | 16 leaf elements |
| Template expression | 4 KiB |
| Delay from `delay` | 600000 ms |
| Stubs per import | 10000 |
| OpenAPI document | 10000 paths, 64 MiB after `$ref` resolution, 200000 resolved nodes |

## Errors

| Status | When |
|---|---|
| 400 | the document is not YAML/JSON, `apiVersion` is not `mocks.microcks.io/*`, `kind` is not `APIExamples` or `APIMetadata`, `metadata.name` or `metadata.version` is missing, a generated stub is invalid |
| 403 | the token is not an editor |
| 404 | the service named by `metadata.name` and `metadata.version` is not imported (`"service `API Pastry - 2.0` version `2.0.0` is not imported; import its main artifact first"`) |
