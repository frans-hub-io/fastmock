# Server-Sent Events

A stub can answer with a stream of [Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
instead of a body: progress updates, order status streams, notification feeds. Each event can wait before it is
sent, so a test sees events arrive over time.

## Defining an event stream

```json
{
  "name": "order status stream",
  "request": {"method": "GET", "urlPathTemplate": "/orders/{id}/events"},
  "response": {
    "status": 200,
    "sse": [
      {"event": "status", "id": "1", "data": "{\"orderId\": \"{{request.pathParams.id}}\", \"status\": \"PACKED\"}"},
      {"event": "status", "id": "2", "data": "{\"orderId\": \"{{request.pathParams.id}}\", \"status\": \"SHIPPED\"}", "delayMs": 500},
      {"event": "done", "data": "line one\nline two", "delayMs": 200}
    ]
  }
}
```

| Field | Type | Meaning |
|---|---|---|
| `event` | template | Event type (`event:` line). Omitted when not set. |
| `id` | template | Event id (`id:` line). Omitted when not set. |
| `data` | template | Event data. Each line of the rendered text becomes its own `data:` line. |
| `delayMs` | integer | Wait before this event, capped at `FASTMOCK_MAX_DELAY`. |

All templates are rendered when the request matches, with the usual context (see [Templating](templating.md)).
`response.delay`, if set, applies before the stream starts.

Rules checked when the stub is saved:

- `sse` cannot be combined with `body`, `jsonBody`, `base64Body` or `proxy`;
- at most 10,000 events per stub.

## What the client receives

```sh
curl -sN -i localhost:8080/orders/A-1/events
```

```text
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
x-fastmock-trace-id: b8c0fcbecdb64a34f064701acbc8ec80
transfer-encoding: chunked

id: 1
event: status
data: {"orderId": "A-1", "status": "PACKED"}

id: 2
event: status
data: {"orderId": "A-1", "status": "SHIPPED"}

event: done
data: line one
data: line two

```

The stream took 0.7 s (500 ms + 200 ms of delays) and the connection ends after the last event. The response
status comes from `response.status`; the response always has `content-type: text/event-stream` and
`cache-control: no-cache`. New lines inside `event` and `id` are replaced by spaces so a template cannot inject
extra fields.

## Combining with other features

- **Scenarios:** a stream stub can require and change scenario state like any other stub, for example to stream
  different events after an order was paid ([Scenarios](scenarios-and-state.md)).
- **Callbacks and Kafka messages** defined on the stub are scheduled and published when the stream starts
  ([Callbacks](callbacks.md), [Kafka](kafka.md)).
- **Journal:** the request is journaled; the streamed response body is not stored.
- **Long streams:** each event delay is capped by `FASTMOCK_MAX_DELAY` (15 minutes by default). When FastMock
  shuts down, open streams get up to 20 seconds to finish.

## Testing an SSE client

A JavaScript `EventSource`, Spring `WebClient` or any SSE library can consume the stream. To test reconnection
logic, combine a stream with a [scenario](scenarios-and-state.md): the first stub (state `Started`) sends two
events and moves the scenario to `reconnected`; the second stub (state `reconnected`) checks the
`Last-Event-ID` header the client sends when it reconnects:

```json
{"request": {"method": "GET", "urlPath": "/feed", "headers": {"last-event-id": {"equalTo": "2"}}},
 "scenario": {"name": "feed", "requiredState": "reconnected"},
 "response": {"sse": [{"id": "3", "event": "status", "data": "resumed"}]}}
```

Request header matching is described in [Request matching](request-matching.md).
