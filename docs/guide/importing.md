# Importing

FastMock generates stubs from the API descriptions and traffic captures teams already have, so a mock does not
have to be written by hand. Every importer turns the document into ordinary stubs (plus async mocks for
AsyncAPI), tags them, and can preview the result without saving anything.

- [Formats and endpoints](#formats-and-endpoints)
- [Common options and response](#common-options-and-response)
- [Auto-detection: POST /import](#auto-detection-post-import)
- [OpenAPI 3](#openapi-3)
- [Postman collections](#postman-collections)
- [HAR captures](#har-captures)
- [Spaces](#spaces)
- [Secrets](#secrets)
- [Limits](#limits)
- [Metrics and audit](#metrics-and-audit)

The examples use `FASTMOCK_TOKEN` for an editor token, the admin API on `localhost:9090` and the data plane on
`localhost:8080`.

## Formats and endpoints

| Format | Endpoint | Details |
|---|---|---|
| OpenAPI 3.0 / 3.1 (JSON or YAML) | `POST /__admin/import/openapi` | [below](#openapi-3), [Microcks artifacts](microcks-artifacts.md) for examples and dispatchers |
| WireMock mappings | `POST /__admin/import/wiremock` | [WireMock and MockServer](import-wiremock-mockserver.md) |
| MockServer expectations | `POST /__admin/import` (auto-detected) | [WireMock and MockServer](import-wiremock-mockserver.md) |
| Postman Collection v2.1 / v2.0 | `POST /__admin/import/postman` | [below](#postman-collections) |
| HAR 1.2 | `POST /__admin/import/har` | [below](#har-captures) |
| AsyncAPI 2.x / 3.x | `POST /__admin/import/asyncapi` | [AsyncAPI](asyncapi.md) |
| GraphQL SDL, WSDL, SoapUI projects, Microcks APIExamples | `POST /__admin/import/graphql`, `/import/wsdl`, `/import/soapui`, `/import/microcks` | [Microcks artifacts](microcks-artifacts.md) |
| any of the above | `POST /__admin/import` | [auto-detection](#auto-detection-post-import) |

Exporting goes the other way: `GET /__admin/export?tag=` returns stubs in the format `POST /__admin/stubs/import`
accepts.

## Common options and response

| Option | Default | Meaning |
|---|---|---|
| `tag` | format name (`postman`, `har`, `wiremock`, …) | added to every generated stub; it identifies the import |
| `replace` | `true` | delete the stubs carrying `tag` first, in the same transaction, so re-importing a changed document leaves no stale stubs |
| `dryRun` | `false` | validate and return the generated stubs without saving anything |

```json
{"imported": 12, "removed": 11, "warnings": ["Get user / not found: same request as an earlier example; ..."]}
```

A dry run answers `{"imported": 0, "removed": 0, "dryRun": true, "stubs": [...], "warnings": [...]}`. Warnings
report what could not be converted or was skipped; the import still succeeds. A generated stub that fails
validation is skipped with a warning naming it (its `name`, or its id). A document that is not of the expected
format, or that yields no valid stub at all, is a `400` and nothing is saved, deleted or replaced: the stubs of the
tag stay as they were. The error lists the first warnings:

```json
{"error": "the document produced no stubs; nothing was changed; mapping #0: no request; the mapping was skipped", "details": []}
```

Tags are 1..128 characters of `[A-Za-z0-9-_.:/]`. A request body that is not valid JSON, or has a field of the wrong
type, is a `400` too.

## Auto-detection: POST /import

Send the document itself, as it is on disk, and let FastMock pick the importer. Options go into the query string.

```sh
curl -s -X POST "http://localhost:9090/__admin/import?tag=petstore&dryRun=true" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/yaml' \
  --data-binary @petstore.yaml
```

| Detected as | When the document |
|---|---|
| OpenAPI 3 | has `openapi: 3.x` |
| AsyncAPI | has `asyncapi` |
| Microcks APIExamples | has `apiVersion: mocks.microcks.io/...` or `kind: APIExamples` |
| Postman | has an `item` array and `info.schema` on `schema.getpostman.com` (or `info._postman_id`) |
| HAR | has `log.entries` |
| WireMock | is `{"mappings": [...]}`, a single mapping or an array of mappings (`request` + `response` objects) |
| MockServer | is an expectation or an array of expectations (`httpRequest` with `httpResponse`, `httpForward`, `httpError`, …) |
| SoapUI | is XML with a `soapui-project` root |
| WSDL | is XML with WSDL 1.1 `definitions` or WSDL 2.0 `description` |
| GraphQL SDL | is text with `type Query`, `type Mutation`, `type Subscription`, `schema {` or `extend type` |

| Query parameter | Meaning |
|---|---|
| `tag` | as above; default is the format name, for GraphQL, WSDL, SoapUI and Microcks the service id |
| `replace`, `dryRun` | as above |
| `format` | skip detection: `openapi`, `wiremock`, `mockserver`, `postman`, `har`, `asyncapi`, `graphql`, `wsdl`, `soapui`, `microcks` |

The response is the importer's response (AsyncAPI answers like `POST /import/asyncapi`). An unrecognised
document, or Swagger 2.0, answers `415`:

```json
{"error": "could not detect the document format; supported: OpenAPI 3, WireMock, MockServer, Postman v2, HAR 1.2, AsyncAPI, GraphQL SDL, WSDL, SoapUI, Microcks APIExamples", "details": []}
```

## OpenAPI 3

```sh
curl -s -X POST http://localhost:9090/__admin/import/openapi \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d "$(jq -n --rawfile spec petstore.yaml '{spec: $spec, tag: "petstore", pathPrefix: "/v1", validateRequests: true}')"
```

| Field | Default | Meaning |
|---|---|---|
| `spec` | required | the document as a JSON object, or JSON/YAML text |
| `tag` | required | tag of the generated stubs |
| `pathPrefix` | `""` | prefix added to every path |
| `validateRequests` | `true` | requests that violate the operation's parameters or body schema get a `400` with the violations |
| `replace`, `dryRun` | `true`, `false` | see above |

Each operation becomes stubs answering its documented examples (or examples generated from the schemas);
`$ref`s are resolved within the document. How examples are selected and dispatched is described in
[Microcks artifacts](microcks-artifacts.md).

## Postman collections

Export the collection from Postman as **Collection v2.1** and send it as `collection` (a JSON object or the file's
text):

```sh
curl -s -X POST http://localhost:9090/__admin/import/postman \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d "$(jq -n --slurpfile c shop.postman_collection.json '{collection: $c[0], tag: "shop"}')"
```

How the collection maps to stubs:

| Postman | Stub |
|---|---|
| saved example response | one stub; requests without saved examples are skipped with a warning |
| example's `originalRequest` (else the item's request) | method, path, query |
| `:id` path segment, unresolved `{{var}}` segment | path parameter: `urlPathTemplate: /users/{id}` |
| `{{baseUrl}}`, host, scheme | resolved from collection and folder variables, then dropped: stubs match on the path |
| enabled query parameters | `equalTo` matchers (values with unresolved `{{var}}` are not matched) |
| raw request body, when the request has several examples | `equalToJson` (`ignoreExtraElements`) or `equalTo`, so the examples can be told apart |
| example `code`, headers, body | response status, headers (except `Content-Length`, `Set-Cookie`, `Date` and hop-by-hop headers), `jsonBody` for JSON, else `body` |
| folders | one tag per folder level (name sanitised to the tag alphabet) |

Example:

```json
{"item": [{"name": "Users", "item": [{
  "name": "Get user",
  "request": {"method": "GET", "url": "{{baseUrl}}/users/:id"},
  "response": [
    {"name": "found", "code": 200, "header": [{"key": "Content-Type", "value": "application/json"}], "body": "{\"id\": 7}"},
    {"name": "missing", "code": 404, "body": ""}
  ]}]}],
 "variable": [{"key": "baseUrl", "value": "https://api.example.com"}]}
```

generates `GET /users/{id}` answering `200 {"id": 7}` and a second, identical request pattern for `404`. Two
examples with the same request cannot both answer: the later one gets a lower priority (higher number) and a
warning; give the examples different query parameters or bodies to make both reachable, or use scenarios.

Variables are resolved once (a variable's value is not resolved again), dynamic variables like `{{$guid}}` are left
as text, and generated stubs have `templated: false`, so `{{...}}` left in a response body is returned literally
and never executed. Postman v1 collections are rejected with a hint to export as v2.1.

## HAR captures

Save a HAR from the browser's developer tools (Network > Export HAR) or a proxy, and send it as `har`:

```sh
curl -s -X POST http://localhost:9090/__admin/import/har \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d "$(jq -n --slurpfile h session.har '{har: $h[0], tag: "checkout-session",
        mimeTypes: ["application/json"], hosts: ["api.example.com"], captureHeaders: ["x-request-id"]}')"
```

| Field | Default | Meaning |
|---|---|---|
| `har` | required | the HAR document as a JSON object or JSON text |
| `mimeTypes` | all | keep only entries whose response MIME type starts with one of these (at most 32) |
| `hosts` | all | keep only entries for these hosts (exact host name, port ignored; at most 256) |
| `captureHeaders` | `[]` | response headers to keep besides `content-type` (at most 32) |
| `tag`, `replace`, `dryRun` | `har`, `true`, `false` | see above |

Each distinct request (method, path, query, body) becomes one stub replaying the first recorded response;
repeats are counted in a warning. The request's path and query become exact matchers, a JSON body
`equalToJson`, another body `equalTo`; request headers are not matched. Responses keep their status, the
`content-type` header, and the body (`jsonBody` for JSON, `base64Body` for base64-encoded content such as images,
else `body`). Entries without a response (blocked or failed requests) are skipped with a warning, as are
responses over 10 MiB.

## Spaces

With an [access file](../operations/access-control.md#spaces) that defines spaces, the OpenAPI, WireMock,
MockServer, Postman and HAR importers (and `POST /__admin/stubs/import`) take a `space` query parameter: every
imported stub goes to that space, and `replace` removes only that space's stubs with the tag. A principal bound to
exactly one space may leave it out. AsyncAPI, GraphQL, WSDL, SoapUI and Microcks imports are global and need
`imports:run` globally.

The imported stubs must stay inside the space's
[ownership](../operations/access-control.md#ownership-of-the-data-plane): their paths under its `pathPrefixes` and,
when the space owns `hosts`, a `request.host` matcher with one of them (both rules apply when the space has both).
The importers set `request.host` for you:

| Space owns | `host` query parameter |
|---|---|
| no hosts | not needed |
| one host | optional; defaults to that host |
| several hosts | required; one of the space's hosts |

```sh
curl -s -X POST "http://localhost:9090/__admin/import/openapi?space=payments&host=psp.example.com" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'Content-Type: application/json' \
  -d "$(jq -n --rawfile spec psp.yaml '{spec: $spec, tag: "psp", pathPrefix: "/payments"}')"
```

Imports into a space are not recorded in the [service catalog](service-catalog.md), which is global.

## Secrets

Postman collections and HAR files often contain real credentials. Before the stubs are stored or returned:

- query parameter matchers whose name is a sensitive key (`FASTMOCK_REDACT_JSON_KEYS`, e.g. `api_key`, `token`)
  become `matches: ".*"`;
- sensitive JSON fields are removed from `equalToJson` matchers, which then use `ignoreExtraElements: true`;
- text body matchers containing a secret (JSON, XML, multipart or form fields with sensitive names) are dropped;
- response headers matching `FASTMOCK_REDACT_HEADERS` or sensitive name patterns (`*token*`, `*secret*`,
  `*key*`, …) are dropped; HAR imports keep only `content-type` and `captureHeaders` to begin with.

The warnings say how many stubs were changed. Request headers (`Authorization`, `Cookie`) are never turned into
matchers by these importers.

## Limits

| Limit | Value |
|---|---|
| request body | 32 MiB |
| stubs per import | 10000 (every saved Postman example is one stub) |
| Postman items (requests and folders) | 10000, folders nested at most 32 deep |
| HAR entries | 10000; bodies over 10 MiB are skipped |
| generated stubs (Postman, HAR) | 64 MiB in total, after variables are substituted |
| parsing | no code is executed while importing; generated stubs are not templated (Postman, HAR) |

## Metrics and audit

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_imports_total` | `format`, `dry_run` | imports per format |
| `fastmock_import_detections_total` | `format` | formats detected by `POST /import` |

Every saved import is audited as `import.<format>` with the tag and the number of imported and removed stubs, and
the imported API is recorded in the service catalog (`GET /__admin/services`), except for imports into a
space.

See also: [Admin API](admin-api.md), [Recording](recording.md), [WireMock and MockServer](import-wiremock-mockserver.md),
[Microcks artifacts](microcks-artifacts.md), [AsyncAPI](asyncapi.md).
