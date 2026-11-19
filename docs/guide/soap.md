# SOAP mocks

FastMock serves SOAP 1.1 and SOAP 1.2 mocks as ordinary stubs that match the SOAP action and the XML body. Two
imports generate them: **SoapUI projects** (the main SOAP format of Microcks, with its mock responses and dispatch
rules) and **WSDL 1.1** documents (one stub per operation with a response envelope generated from the XSD).

- [How SOAP requests are matched](#how-soap-requests-are-matched)
- [Importing a SoapUI project](#importing-a-soapui-project)
- [Dispatch styles](#dispatch-styles)
- [Importing a WSDL](#importing-a-wsdl)
- [Faults](#faults)
- [Templates in SOAP responses](#templates-in-soap-responses)
- [XML safety](#xml-safety)
- [Metrics](#metrics)
- [Limits](#limits)
- [Errors](#errors)

The examples use `FASTMOCK_TOKEN` for an editor token, the admin API on `localhost:9090` and the data plane on
`localhost:8080`.

## How SOAP requests are matched

Each generated stub matches `POST` on the endpoint path and:

| SOAP version | Action | Body |
|---|---|---|
| 1.1 (`text/xml`) | header `SOAPAction`, with or without quotes, when the operation has an action | XPath `/Envelope/Body/<request element>` exists |
| 1.2 (`application/soap+xml`) | `action="..."` parameter of `Content-Type`, when the operation has an action | same |

XPath steps compare local names, so namespace prefixes in the request do not matter. A stub written by hand looks
the same:

```json
{
  "name": "sayHello",
  "request": {
    "method": "POST",
    "urlPath": "/HelloService",
    "headers": {"soapaction": {"matches": "\"?http://www\\.example\\.com/hello/sayHello\"?"}},
    "body": [
      {"xpath": "/Envelope/Body/sayHello"},
      {"xpath": "//sayHello/name", "value": {"equalTo": "Andrew"}}
    ]
  },
  "response": {
    "headers": {"content-type": "text/xml; charset=utf-8"},
    "body": "<soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\"><soapenv:Body><sayHelloResponse><sayHello>Hello Andrew !</sayHello></sayHelloResponse></soapenv:Body></soapenv:Envelope>"
  }
}
```

## Importing a SoapUI project

```sh
curl -s -X POST localhost:9090/__admin/import/soapui \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile doc HelloService-soapui-project.xml '{document: $doc}')"
```

```json
{"imported": 8, "removed": 0, "warnings": ["audit: SCRIPT dispatch is never executed; every request gets the default response `ok`"], "serviceId": "helloservice-mock:0.9"}
```

Request fields (the same for `/import/wsdl`):

| Field | Default | Meaning |
|---|---|---|
| `document` | required | the project XML (or the WSDL) as text |
| `path` | the `path` of the mock service (SoapUI), the address path of the port (WSDL) | endpoint path of the stubs |
| `name` | the mock service name (SoapUI), the `wsdl:service` name (WSDL) | service name in the [catalog](service-catalog.md) |
| `version` | the mock service property `version` (SoapUI), `1.0` | service version |
| `mockService` | the first mock service | SoapUI only: which mock service to import |
| `tag` | the service id | tag of the generated stubs |
| `replace` | `true` | replace the stubs that carry the tag |
| `dryRun` | `false` | return the stubs without saving them |

What is read from the project:

- `con:interface`: `soapVersion` (`1_1` or `1_2`) and, per `con:operation`, the `action` (SOAP action).
- The request element of each operation: from the WSDL cached in `con:definitionCache`, else from the body of the
  first saved request (`con:call/con:request`), else the operation name.
- `con:mockService`: `name`, `path` and the `version` property. Microcks uses the same name and version to
  identify the service.
- `con:mockOperation`: `dispatchStyle`, `dispatchPath`, `defaultResponse`, `con:dispatchConfig` queries and every
  `con:response` with its `httpResponseStatus`, `responseContent` and `con:header` entries (name and value).

Mock services of other kinds (`restMockService`) are not imported; import their OpenAPI or Postman definition.

## Dispatch styles

| `dispatchStyle` | Generated stubs |
|---|---|
| `SEQUENCE` (default) | one stub whose `responses` are all mock responses with `responseMode: cycle` |
| `RANDOM` | one stub with `responseMode: random` |
| `XPATH` | one stub per response (priority 4) matching `dispatchPath` = the response **name**, plus a default stub (priority 5) |
| `QUERY_MATCH` | one stub per query (priority 4) matching its XPath = `match` and answering its `response`, plus a default stub |
| `SCRIPT` | not executed: a warning and one stub answering the default response |

`declare namespace p='...';` prologues are removed and `*[local-name()='x']` steps are rewritten to `x`. XPath
expressions must stay inside the supported subset (absolute paths, `//`, `*`, `@attr`, `text()`, `[n]`,
`[@a='v']`, `[child='v']`); dispatch on an expression outside it (for example `concat(...)`) is skipped with a
warning and every request gets the default response. The default response is `defaultResponse`, or the first
response when it is not set.

Example: with this `QUERY_MATCH` configuration,

```xml
<con:dispatchConfig xsi:type="con:MockOperationQueryMatchDispatch">
  <con:query>
    <con:name>Andrew</con:name>
    <con:query>declare namespace hel='http://www.example.com/hello'; //hel:sayHello/name</con:query>
    <con:match>Andrew</con:match>
    <con:response>Andrew Response</con:response>
  </con:query>
</con:dispatchConfig>
```

```sh
curl -s localhost:8080/HelloService -H 'content-type: text/xml' \
  -H 'SOAPAction: "http://www.example.com/hello/sayHello"' \
  -d '<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:hel="http://www.example.com/hello"><soapenv:Body><hel:sayHello><name>Andrew</name></hel:sayHello></soapenv:Body></soapenv:Envelope>'
```

returns the `Andrew Response` content, while any other name gets the default response with its own status
(for example a 500 SOAP fault).

## Importing a WSDL

```sh
curl -s -X POST localhost:9090/__admin/import/wsdl \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --rawfile doc HelloService.wsdl '{document: $doc, version: "1.0"}')"
```

For every `wsdl:binding` with a SOAP 1.1 or SOAP 1.2 binding and every operation in it, FastMock creates one stub:

- path: `path`, else the path of the `soap:address` / `soap12:address` of the port that uses the binding, else
  `/<service name>`;
- matching as described in [How SOAP requests are matched](#how-soap-requests-are-matched); the request element is
  the element of the first part of the input message (`document` style) or the operation name (`rpc` style);
- response: `200`, `content-type` `text/xml; charset=utf-8` (1.1) or `application/soap+xml; charset=utf-8` (1.2),
  and an envelope with a sample of the output element.

When the WSDL has both a 1.1 and a 1.2 binding, the stub names end with `(SOAP 1.1)` / `(SOAP 1.2)` and the 1.1
stubs also require a `Content-Type` that is not `application/soap+xml`.

The sample is generated from the XSD in `wsdl:types`: `element` with `type`, `ref` or an inline `complexType` /
`simpleType`; `sequence`, `all`, `choice` (first branch), `complexContent` / `simpleContent` `extension` and
`restriction`; attributes; enumerations (first value). Values by type: `string` -> `string`, integers -> `0`
(`positiveInteger` -> `1`), `decimal`/`float`/`double` -> `0.0`, `boolean` -> `true`, `date` -> `2026-01-01`,
`dateTime` -> `2026-01-01T00:00:00Z`, `anyURI` -> `https://example.com`. Elements are qualified with the target
namespace according to `elementFormDefault` / `form`; every schema namespace is declared on the envelope as
`ns0`, `ns1`, ... A recursive type is expanded once.

```xml
<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns0="http://www.example.com/hello">
  <soapenv:Header/>
  <soapenv:Body>
    <ns0:getBalanceResponse version="0"><amount>0.0</amount><currency>UAH</currency>
      <updatedAt>2026-01-01T00:00:00Z</updatedAt><history><at>2026-01-01</at><delta>0</delta></history>
    </ns0:getBalanceResponse>
  </soapenv:Body>
</soapenv:Envelope>
```

(shown indented; the generated body has no whitespace between elements). Edit the generated stubs or add
higher-priority stubs with XPath body matchers to return specific data.

## Faults

Each `wsdl:fault` of an operation becomes an extra stub that is **disabled** (`"enabled": false`), priority 4,
status 500, with a SOAP fault envelope whose `detail` (1.1) or `Detail` (1.2) holds a sample of the fault
element. Enable it to make the operation fail:

```sh
curl -s localhost:9090/__admin/stubs -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  | jq -r '.stubs[] | select(.name | startswith("sayHello fault")) | .id'
curl -s -X PATCH localhost:9090/__admin/stubs/<id> -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{"enabled": true}'
```

## Templates in SOAP responses

Mock response content may use Microcks templates and SoapUI `${...}` expressions with the same functions, for
example `${ uuid() }` or `{{ now(yyyy-MM-dd) }}`; they are converted as listed in
[Microcks artifacts](microcks-artifacts.md#template-expressions). XPath forms such as `{{ request.body//name }}`
and Groovy expansions (`${= ...}`) stay literal with a warning; to echo request values use the FastMock `xPath`
helper by hand:

```xml
<sayHello>Hello {{xPath request.body "/Envelope/Body/sayHello/name"}} !</sayHello>
```

## XML safety

Projects and WSDL documents are parsed with DTDs disabled (documents with a `DOCTYPE` are rejected, so entity
expansion is impossible), at most 1,000,000 nodes and 32 MiB. External schemas (`xsd:import` / `xsd:include` with
`schemaLocation`) and remote WSDL parts are never fetched; their types are left empty with a warning. Request
bodies are matched with the same parser and limits as any XPath body matcher.

## Metrics

| Metric | Labels |
|---|---|
| `fastmock_imports_total` | `format` (`soapui`, `wsdl`), `dry_run` |
| `fastmock_artifact_imports_total` | `format`, `outcome` (`ok`, `error`) |

Requests to SOAP stubs are counted, journaled and traced like any other stub request.

## Limits

| Input | Limit |
|---|---|
| Document | 32 MiB, 1,000,000 XML nodes, elements nested at most 256 levels |
| Operations per import | 2000 |
| Generated envelope | 500 elements, 12 levels |
| Schema walk per envelope | 64 nested particles or references, 20,000 steps; a recursive `ref` or type ends as an empty element |
| Stubs per import | 10000 |

## Errors

| Status | When |
|---|---|
| 400 | not well-formed XML or a `DOCTYPE` (`invalid XML: ...`), not a SoapUI project or a WSDL 1.1 document, no SOAP binding, no SOAP mock service, unknown `mockService`, `path` without a leading `/`, invalid tag |
| 403 | the token is not an editor |
