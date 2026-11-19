# Service catalog

Every import records a **service** in the catalog: its name, version, type, the format it came from, its
operations with the ids of the stubs that implement them, labels, and who imported it when. It is the FastMock
counterpart of the Microcks "APIs & Services" page and is shown in the UI. The OpenAPI and AsyncAPI documents
themselves are stored too, for [contract testing](contract-testing.md) and the UI.

- [What is recorded](#what-is-recorded)
- [The service document](#the-service-document)
- [Listing and reading services](#listing-and-reading-services)
- [Stored specifications](#stored-specifications)
- [Re-imports](#re-imports)
- [Deleting a service](#deleting-a-service)
- [Roles](#roles)
- [Storage, metrics and limits](#storage-metrics-and-limits)

The examples use `FASTMOCK_TOKEN` for a token with the needed role and the admin API on `localhost:9090`.

## What is recorded

| Import | Type | Name and version | Tag | Document stored |
|---|---|---|---|---|
| `POST /import/openapi` | `REST` | `info.title`, `info.version` | `tag` | yes |
| `POST /import/asyncapi` | `EVENT` | `info.title`, `info.version` | the import tag | yes |
| `POST /import/graphql` | `GRAPHQL` | `# microcksId`, `name`/`version` | `tag` or the service id | no |
| `POST /import/soapui` | `SOAP` | mock service name, `version` property | `tag` or the service id | no |
| `POST /import/wsdl` | `SOAP` | `wsdl:service` name, `version` | `tag` or the service id | no |
| `PUT /grpc/descriptors/{name}` | `GRPC` | `{name}`, `1.0` | none | no |
| `POST /import/wiremock`, `/import/postman`, `/import/har` | `GENERIC` | the tag, `1.0` | `tag` | no |

Dry runs record nothing. A catalog write that fails is logged and counted but never fails the import, because the
stubs are already saved.

The service **id** is `<name>:<version>` in lower case, with every run of characters other than letters, digits,
`.` and `_` replaced by one `-` (ids longer than 100 characters are shortened and end with a hash). It is a valid
tag, and the imports that default their tag use it: `API Pastry - 2.0` version `2.0.0` has the id
`api-pastry-2.0:2.0.0`.

## The service document

```json
{
  "id": "api-pastry-2.0:2.0.0",
  "name": "API Pastry - 2.0",
  "version": "2.0.0",
  "type": "REST",
  "sourceFormat": "openapi",
  "tag": "pastry",
  "operations": [
    {"name": "GET /pastry", "method": "GET", "path": "/pastry",
     "stubIds": ["019a9b5e-0c1f-7c51-9a6e-2f4f4a2b9c01", "019a9b5e-0c1f-7c51-9a6e-2f4f4a2b9c02"]},
    {"name": "GET /pastry/{name}", "method": "GET", "path": "/pastry/{name}",
     "stubIds": ["019a9b5e-0c1f-7c51-9a6e-2f4f4a2b9c03"]}
  ],
  "labels": {"domain": "pastry", "status": "GA", "team": "pastry"},
  "importedAt": "2026-11-19T10:15:02.114Z",
  "importedBy": "ci",
  "hasSpec": true,
  "importOptions": {"validateRequests": false, "pathPrefix": ""},
  "overrides": {
    "labels": {"team": "pastry"},
    "operations": {"GET /pastry/{name}": {"delay": 25}}
  },
  "artifacts": [
    {"kind": "APIExamples", "importedAt": "2026-11-19T10:16:40.502Z", "importedBy": "ci"},
    {"kind": "APIMetadata", "importedAt": "2026-11-19T10:17:03.881Z", "importedBy": "ci"}
  ]
}
```

| Field | Meaning |
|---|---|
| `type` | `REST`, `SOAP`, `GRAPHQL`, `EVENT`, `GRPC` or `GENERIC` |
| `sourceFormat` | `openapi`, `asyncapi`, `graphql`, `soapui`, `wsdl`, `grpc-descriptor`, `wiremock`, `postman`, `har` |
| `operations[].name` | Microcks operation name: `GET /path` for REST, the field for GraphQL, the operation for SOAP, `package.Service/Method` for gRPC, `SUBSCRIBE channel` / `SEND channel` for AsyncAPI; for other formats `METHOD path` of the stubs |
| `operations[].stubIds` | stubs that implement the operation, including `APIExamples` stubs |
| `labels` | from `info.x-microcks.labels` and `APIMetadata` |
| `overrides` | `APIMetadata` labels and operation settings, applied again on every re-import |
| `artifacts` | companion `APIExamples` / `APIMetadata` imports |

Operations of imported stubs are grouped by `metadata.operation` of the stubs (set by every importer), otherwise
by method and path.

## Listing and reading services

```sh
curl -s localhost:9090/__admin/services -H "Authorization: Bearer $FASTMOCK_TOKEN"
curl -s 'localhost:9090/__admin/services?type=SOAP' -H "Authorization: Bearer $FASTMOCK_TOKEN"
curl -s 'localhost:9090/__admin/services?label=team=pastry&q=pastry' -H "Authorization: Bearer $FASTMOCK_TOKEN"
curl -s localhost:9090/__admin/services/api-pastry-2.0:2.0.0 -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

`GET /__admin/services` returns `{"services": [...]}` with the most recently imported first. Filters:

| Query | Meaning |
|---|---|
| `type` | one of the types, case-insensitive |
| `label` | `key=value`, or `key` for "has the label" |
| `q` | case-insensitive substring of the name |

`GET /__admin/services/{id}` returns one service or 404 `{"error": "service not found", "details": []}`. To see
the stubs of an operation, read them by id (`GET /__admin/stubs/{id}`) or by tag (`GET /__admin/stubs?tag=<tag>`).

## Stored specifications

OpenAPI and AsyncAPI documents are stored, after parsing, in the data store namespace `specs` under the service id:

```sh
curl -s localhost:9090/__admin/services/api-pastry-2.0:2.0.0/spec -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq '.name, .type, .document.info'
```

```json
{"name": "API Pastry - 2.0", "version": "2.0.0", "type": "REST", "document": {"openapi": "3.0.3", "info": {"...": "..."}, "paths": {"...": "..."}}}
```

Contract tests refer to it with `specRef` (the service id). The document is not stored when it is larger than
8 MiB serialized (logged as a warning); the service then has `"hasSpec": false` and `/spec` answers 404. Services
from other formats have no stored document.

## Re-imports

Importing a service with the same name and version replaces the previous one:

- the stubs carrying the import tag are replaced in one transaction of the stub store, so every replica sees
  either the old or the new set;
- when the previous import used a different tag, the stubs of the old tag (and its `:examples` tag) are deleted;
- `APIExamples` stubs (tag `<tag>:examples`) are kept, as are labels and operation overrides from `APIMetadata`;
  for OpenAPI services the overrides are applied to the new stubs right away;
- `importedAt` and `importedBy` are updated and the stored document is replaced.

A different version is a different service, so versions can be mocked side by side when their paths differ (for
example with `pathPrefix` on the OpenAPI import).

## Deleting a service

```sh
curl -s -X DELETE localhost:9090/__admin/services/api-pastry-2.0:2.0.0 -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{"id": "api-pastry-2.0:2.0.0", "deletedStubs": 13}
```

The stubs carrying the service tag and `<tag>:examples` are deleted, then the stored document and the catalog
entry. The engine reloads immediately, and other replicas pick the change up as for any stub change. Deleting a
gRPC service removes only the catalog entry; remove the descriptor set with
`DELETE /__admin/grpc/descriptors/{name}`. The deletion is audited as `service.delete`.

Use one tag per service: deleting a service deletes every stub with its tag, including stubs added by hand with
the same tag.

## Roles

| Endpoint | Role |
|---|---|
| `GET /__admin/services`, `GET /__admin/services/{id}`, `GET /__admin/services/{id}/spec` | viewer |
| `DELETE /__admin/services/{id}` and every import | editor |

## Storage, metrics and limits

Services live in the data store namespace `services` (PostgreSQL in shared mode, memory otherwise), so every
replica sees the same catalog. At most 1000 services are listed.

| Metric | Labels | Meaning |
|---|---|---|
| `fastmock_services_recorded_total` | `type` | catalog entries written |
| `fastmock_store_errors_total` | `store="services"` | failed catalog writes |
| `fastmock_admin_changes_total` | `action="service.delete"`, `action="service.metadata"` | catalog changes |
