# Migrating from Microcks

This guide moves the mocks of a Microcks installation to FastMock: collect the artifacts, import them, verify the
mocks, and switch the clients. The two can run side by side until the switch is done.

- [1. Inventory the services](#1-inventory-the-services)
- [2. Collect the artifacts](#2-collect-the-artifacts)
- [3. Import](#3-import)
- [4. Verify](#4-verify)
- [5. Switch the endpoints](#5-switch-the-endpoints)
- [6. Contract tests and async APIs](#6-contract-tests-and-async-apis)
- [Differences to plan for](#differences-to-plan-for)

The examples use `FASTMOCK_TOKEN` for an editor token, the admin API on `localhost:9090` and the data plane on
`localhost:8080`. `jq` is used to build JSON bodies.

## 1. Inventory the services

List what Microcks serves today: the "APIs & Services" page shows every service with its name, version and type
(REST, SOAP, GraphQL, gRPC, Event). For each one write down:

- the **primary artifact** (OpenAPI, AsyncAPI, SoapUI project, GraphQL schema, gRPC `.proto`, Postman collection)
  and the **secondary artifacts** (`APIExamples`, `APIMetadata`, a Postman collection with GraphQL examples);
- dispatchers and delays changed in the Microcks UI (they are not in the artifacts; add them to an `APIMetadata`
  file, see below);
- operations that use a `SCRIPT` dispatcher (Groovy or JavaScript): FastMock does not run scripts, so these need
  explicit stubs;
- the URLs clients use (`/rest/<service>/<version>/...`, `/soap/<service>/<version>`,
  `/graphql/<service>/<version>`, gRPC host and port).

## 2. Collect the artifacts

Use the files from the source repository that the Microcks importer jobs or CI pipelines read (the usual setup:
`microcks import` or the Microcks GitHub Action pointing at files in git). If a service was uploaded by hand, open
it in the Microcks UI and download its contract files from the service page.

Put them in one directory, for example:

```text
mocks/
  pastry-openapi.yaml          primary   (OpenAPI 3)
  pastry-examples.yaml         secondary (APIExamples)
  pastry-metadata.yaml         secondary (APIMetadata)
  films.graphql                primary   (GraphQL SDL with "# microcksId: Movie Graph API : 1.0")
  films-examples.yaml          secondary (APIExamples)
  HelloService-soapui-project.xml primary (SoapUI)
  user-signedup-asyncapi.yaml  primary   (AsyncAPI)
  greeter.pb                   primary   (gRPC descriptor set)
```

gRPC: FastMock needs a compiled descriptor set instead of the `.proto` file:

```sh
protoc --include_imports --descriptor_set_out=greeter.pb -I protos protos/greeter.proto
```

Overrides made in the Microcks UI become an `APIMetadata` file, which also keeps them under version control:

```yaml
apiVersion: mocks.microcks.io/v1alpha1
kind: APIMetadata
metadata:
  name: API Pastry - 2.0
  version: 2.0.0
operations:
  'GET /pastry/{name}':
    delay: 100
    dispatcher: URI_PARTS
    dispatcherRules: name
```

## 3. Import

Import the primary artifacts first, then the secondary ones (they are applied to the service with the same
`metadata.name` and `metadata.version`). Each import returns `warnings`; keep them for step 4.

```sh
H=(-H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json')
A=localhost:9090/__admin

curl -s -X POST $A/import/openapi "${H[@]}" \
  -d "$(jq -n --rawfile s mocks/pastry-openapi.yaml '{spec: $s, tag: "pastry"}')" | jq
curl -s -X POST $A/import/graphql "${H[@]}" \
  -d "$(jq -n --rawfile s mocks/films.graphql --rawfile e mocks/films-examples.yaml '{schema: $s, examples: $e}')" | jq
curl -s -X POST $A/import/soapui "${H[@]}" \
  -d "$(jq -n --rawfile d mocks/HelloService-soapui-project.xml '{document: $d}')" | jq
curl -s -X POST $A/import/asyncapi "${H[@]}" \
  -d "$(jq -n --rawfile s mocks/user-signedup-asyncapi.yaml '{spec: $s}')" | jq
curl -s -X PUT $A/grpc/descriptors/greeter -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  --data-binary @mocks/greeter.pb | jq

for f in mocks/pastry-examples.yaml mocks/pastry-metadata.yaml; do
  curl -s -X POST $A/import/microcks "${H[@]}" -d "$(jq -n --rawfile d "$f" '{document: $d}')" | jq
done
```

`POST /__admin/import` accepts any of these documents as the raw request body and detects the format, which is
convenient in a loop over a directory:

```sh
for f in mocks/*.yaml mocks/*.graphql mocks/*.xml; do
  curl -s -X POST "$A/import" -H "Authorization: Bearer $FASTMOCK_TOKEN" --data-binary @"$f" \
    | jq -c '{file: "'"$f"'", imported: (.imported // ((.asyncMocks // []) + (.websocketStubs // []) | length)), warnings: (.warnings // .error)}'
done
```

AsyncAPI documents answer with `asyncMocks` and `websocketStubs` instead of `imported`, hence the fallback in the
filter; a document that yields nothing answers `400 {"error": ...}`, shown in place of the warnings.

Run the imports with `"dryRun": true` (or `?dryRun=true` on `/import`) first to read the generated stubs and
warnings without changing anything. Re-running an import replaces the previous stubs of the same service, so the
script can run on every change of the repository, from CI, exactly like the Microcks importer.

## 4. Verify

1. **Catalog.** Every service should be listed with the same name, version, type and operations as in Microcks:

   ```sh
   curl -s localhost:9090/__admin/services -H "Authorization: Bearer $FASTMOCK_TOKEN" \
     | jq -r '.services[] | "\(.type)\t\(.name)\t\(.version)\t\(.operations | length) operations"'
   ```

2. **Warnings.** Read the warnings of every import. The usual ones:

   | Warning | What to do |
   |---|---|
   | `dispatcher SCRIPT runs a script; scripts are never executed` | write stubs with request matchers that reproduce the script's decision |
   | `template ... kept literally` | replace the expression with a FastMock helper (see [template conversion](microcks-artifacts.md#template-expressions)) |
   | `response example X has no request example with the same name` | add the missing request example, or accept that X is served only as the default |
   | `JSON_BODY operator range is not supported` | split the case into `regexp` cases or write stubs by hand |
   | `external schema ... is not fetched` | inline the XSD into the WSDL `types` |

3. **Call the mocks** with the requests your tests send and compare with Microcks:

   ```sh
   diff <(curl -s http://microcks.example.com/rest/API+Pastry+-+2.0/2.0.0/pastry/Millefeuille | jq -S .) \
        <(curl -s localhost:8080/pastry/Millefeuille | jq -S .)
   ```

   Fields produced by templates (dates, ids, random values) differ by design.

4. **Unmatched requests.** Run the client test suite against FastMock and look for requests no stub answered:

   ```sh
   curl -s 'localhost:9090/__admin/requests?matched=false&limit=50' -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq '.requests[] | {method, path}'
   curl -s -X POST localhost:9090/__admin/requests/near-misses -H "Authorization: Bearer $FASTMOCK_TOKEN" \
     -H 'content-type: application/json' -d '{"method": "GET", "path": "/pastry/Croissant"}' | jq
   ```

## 5. Switch the endpoints

Microcks serves each service under a prefix; FastMock serves the paths of the artifact. Either change the base URL
of the clients, or keep their URLs by importing with the Microcks prefix:

| Type | Microcks URL | FastMock |
|---|---|---|
| REST | `http://microcks.example.com/rest/API+Pastry+-+2.0/2.0.0/pastry/x` | `http://fastmock:8080/pastry/x`, or import with `"pathPrefix": "/rest/API+Pastry+-+2.0/2.0.0"` |
| SOAP | `http://microcks.example.com/soap/HelloService+Mock/0.9` | import with `"path": "/soap/HelloService+Mock/0.9"` |
| GraphQL | `http://microcks.example.com/graphql/Movie+Graph+API/1.0` | import with `"path": "/graphql/Movie+Graph+API/1.0"` |
| gRPC | `microcks.example.com:9090` | the FastMock data plane port (HTTP/2) |
| Async | the Kafka topic `<service name without spaces and dashes>-<version>-<channel>` | the channel of the AsyncAPI document, or Microcks' topic names with `"topicNaming": "microcks"` on the import (see [AsyncAPI](asyncapi.md#microcks-topic-names)) |

`request.path[n]` in Microcks templates counts from the start of the operation path, also under a `pathPrefix`:
`{{ request.path[1] }}` is `Croissant` for `/rest/API+Pastry+-+2.0/2.0.0/pastry/Croissant`.

Paths are matched after percent-decoding, and `+` in a path is a plus sign, not a space. A prefix with `+` matches
clients that send `+` (`/rest/API+Pastry+-+2.0/...`); a client that sends `%20` (`/rest/API%20Pastry%20-%202.0/...`)
needs the prefix with spaces, `"pathPrefix": "/rest/API Pastry - 2.0/2.0.0"`. If both kinds of clients exist,
change the base URL of one of them. Switch one consumer at a time,
keep Microcks running until the client test suites pass against FastMock, and then remove it.

## 6. Contract tests and async APIs

- Microcks **tests** (checking a real implementation against its contract) map to FastMock
  [contract tests](contract-testing.md); they read the OpenAPI or AsyncAPI document stored with the service
  (`specRef` = service id, see [service catalog](service-catalog.md#stored-specifications)).
- AsyncAPI services become async mocks that publish to Kafka or WebSocket stubs; see [AsyncAPI](asyncapi.md).

## Differences to plan for

- **No code execution.** Groovy and JavaScript dispatchers, SoapUI Groovy expansions and context functions
  (`put`, `get`) are not run. Everything they did must be expressed with request matchers, priorities,
  [scenarios](scenarios-and-state.md) or templates.
- **Dispatching is static.** Examples become stubs at import time. Change dispatching by editing the artifact or
  an `APIMetadata` file and re-importing, or edit the generated stubs.
- **GraphQL field selection** is not applied: a response returns every field of the example.
- **Unmatched values** get the default example (FastMock adds it as a fallback stub for every operation with
  named examples) where Microcks answers with an error; add a `FALLBACK` dispatcher to choose the example, or
  delete the fallback stub to get a 404.
- **Labels** come from `info.x-microcks.labels` and `APIMetadata`; labels set only in the Microcks UI must be added
  to an `APIMetadata` file.
- **Security.** Every admin call needs a token with the right role, and outbound calls (callbacks, proxies) obey
  the egress policy; see [Admin API](admin-api.md).
