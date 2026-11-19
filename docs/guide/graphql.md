# GraphQL mocks

FastMock mocks GraphQL over HTTP POST. Stubs match a GraphQL request by its operation name, its query text or the
**root field** it selects, and answer with any JSON. A GraphQL schema (SDL) can be imported to generate one stub
per query and mutation field, optionally with examples from a Microcks `APIExamples` document.

- [Matching GraphQL requests](#matching-graphql-requests)
- [Importing a schema](#importing-a-schema)
- [Examples](#examples)
- [Variables in templates](#variables-in-templates)
- [Metrics](#metrics)
- [Limits](#limits)
- [Errors](#errors)

The examples use `FASTMOCK_TOKEN` for an editor token, the admin API on `localhost:9090` and the data plane on
`localhost:8080`.

## Matching GraphQL requests

`request.graphql` adds conditions on a JSON GraphQL request body (`{"query": ..., "operationName": ...,
"variables": ...}`). A body sent as `application/graphql` (the query as plain text) is also understood by
`rootField`.

| Field | Matches |
|---|---|
| `operationName` | string matcher on `operationName` |
| `query` | string matcher on the query text |
| `rootField` | string matcher on the root fields of the executed operation |

Root fields are the fields selected directly by the operation: in `query Q($id: ID!) { f: film(id: $id) { title }
allFilms { totalCount } }` they are `film` and `allFilms` (aliases are resolved, `__typename` is ignored). When
the document holds several operations, the one named by `operationName` is used, otherwise the first one. Fields
selected through root-level fragment spreads (`...RootFields`) and inline fragments (`... on Query { ... }`) count
too. `rootField` matches when **any** root field matches.

```sh
curl -s -X POST localhost:9090/__admin/stubs \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "request": {"method": "POST", "urlPath": "/graphql", "graphql": {"rootField": {"equalTo": "film"}}},
  "response": {"jsonBody": {"data": {"film": {"id": "{{request.json.variables.id}}", "title": "A New Hope"}}}}
}'
curl -s localhost:8080/graphql -H 'content-type: application/json' \
  -d '{"query": "query film($id: ID!) { film(id: $id) { id title } }", "variables": {"id": "ZmlsbXM6MQ=="}}'
# {"data":{"film":{"id":"ZmlsbXM6MQ==","title":"A New Hope"}}}
```

Combine `rootField` with body matchers on `$.variables` to dispatch on arguments:

```json
{
  "request": {
    "method": "POST", "urlPath": "/graphql",
    "graphql": {"rootField": {"equalTo": "film"}},
    "body": [{"jsonPath": "$.variables.id", "value": {"equalTo": "ZmlsbXM6Mg=="}}]
  },
  "response": {"jsonBody": {"data": {"film": {"title": "The Empire Strikes Back"}}}}
}
```

The response is returned as written: FastMock does not apply the field selection of the query to it.

## Importing a schema

```sh
cat > films.graphql <<'EOF'
# microcksId: Movie Graph API : 1.0
type Film { id: ID! title: String! episodeID: Int genre: Genre reviews: [Review!]! }
type Review { id: ID! stars: Int film: Film }
enum Genre { SCIFI DRAMA }
type FilmsConnection { totalCount: Int films: [Film] }
type Query { allFilms: FilmsConnection film(id: ID!): Film }
type Mutation { addStar(filmId: ID!): Film }
EOF
curl -s -X POST localhost:9090/__admin/import/graphql \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile s films.graphql '{schema: $s, path: "/graphql"}')"
```

```json
{"imported": 3, "removed": 0, "warnings": [], "serviceId": "movie-graph-api:1.0"}
```

Request fields:

| Field | Default | Meaning |
|---|---|---|
| `schema` | required | GraphQL SDL text |
| `examples` | none | Microcks `APIExamples` (YAML/JSON text or object), see [Examples](#examples) |
| `path` | `/graphql` | endpoint path of the generated stubs |
| `name`, `version` | from `# microcksId: <name> : <version>`, else `GraphQL API` / `1.0` | service name and version in the [catalog](service-catalog.md) |
| `tag` | the service id | tag of the generated stubs |
| `replace` | `true` | replace the stubs that carry the tag |
| `dryRun` | `false` | return the stubs without saving them |

For every field of the query type and the mutation type (`schema { query: ... mutation: ... }` or the default
names `Query` and `Mutation`, including `extend type` blocks) FastMock creates:

```json
{
  "name": "Query.film",
  "priority": 5,
  "request": {"method": "POST", "urlPath": "/graphql", "graphql": {"rootField": {"equalTo": "film"}}},
  "response": {
    "status": 200,
    "headers": {"content-type": "application/json"},
    "jsonBody": {"data": {"film": {"id": "1", "title": "string", "episodeID": 1, "genre": "SCIFI",
      "reviews": [{"id": "1", "stars": 1, "film": null}]}}},
    "templated": false
  },
  "tags": ["movie-graph-api:1.0"],
  "metadata": {"operation": "film", "operationType": "query"}
}
```

Example values come from the return type: `ID` -> `"1"`, `String` and custom scalars -> `"string"`, `Int` -> `1`,
`Float` -> `1.5`, `Boolean` -> `true`, enums -> the first value, unions -> the first member, lists -> one
element. A type that is already being expanded higher up becomes `null` (or `[]` inside a list), and nesting stops
after 5 object levels. Subscription fields are skipped with a warning.

## Examples

Microcks attaches GraphQL examples in an `APIExamples` document whose operations are the field names. Pass it as
`examples` in the import, or import it later with `POST /__admin/import/microcks` (see
[Microcks artifacts](microcks-artifacts.md#apiexamples)).

```yaml
apiVersion: mocks.microcks.io/v1alpha1
kind: APIExamples
metadata: {name: Movie Graph API, version: '1.0'}
operations:
  film:
    film 1:
      request:
        body:
          query: 'query film($id: ID!) { film(id: $id) { id title } }'
          variables: {id: ZmlsbXM6MQ==}
      response:
        body:
          data:
            film: {id: ZmlsbXM6MQ==, title: A New Hope}
```

Each example becomes a stub (priority 4 inline, 3 through `/import/microcks`) with the same `rootField` and a
body matcher `{"equalToJson": {"variables": {"id": "ZmlsbXM6MQ=="}}, "ignoreExtraElements": true}`, so it answers
when the request variables contain the example variables. A response body without `data` or `errors` is wrapped
as `{"data": {<field>: body}}`. Requests with other variables get the generated stub.

## Variables in templates

The request body is parsed as JSON, so variables, the operation name and the query are available in response
templates:

```json
{"jsonBody": {"data": {"addStar": {"id": "{{request.json.variables.filmId}}", "op": "{{request.json.operationName}}"}}}}
```

Microcks expressions such as `{{ request.body/variables/id }}` in imported examples are converted to
`{{request.json.[variables].[id]}}`.

## Metrics

Requests to GraphQL stubs are ordinary stub requests (`fastmock_requests_total`, journal, traces). Imports count in
`fastmock_imports_total{format="graphql"}` and `fastmock_artifact_imports_total{format="graphql",outcome}`.

## Limits

| Input | Limit |
|---|---|
| Schema | 2 MiB, nesting of `{`, `[`, `(` at most 64 levels |
| Query text inspected by `rootField` | 1 MiB, 200000 tokens; larger queries match no root field |
| Root fields per request | 64 |
| Fragment spreads followed | 4 levels |
| Generated example | 5 object levels, 2000 values per field |

## Errors

| Status | When |
|---|---|
| 400 | the schema does not parse (`invalid GraphQL schema: ...`), is too large or too deep, `path` does not start with `/`, `examples` is not an `APIExamples` document, the tag is invalid |
| 403 | the token is not an editor |
