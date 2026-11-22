package io.franshub.fastmock.client;

import io.franshub.fastmock.client.Stub.Callback;
import io.franshub.fastmock.client.Stub.Match;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.time.Instant;
import java.util.Map;
import java.util.UUID;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

class FastMockClientIT {

    static FastMock fm;
    static String data;
    static final HttpClient http = HttpClient.newHttpClient();

    @BeforeAll
    static void connect() {
        String admin = System.getenv("FASTMOCK_ADMIN_URL");
        data = System.getenv("FASTMOCK_DATA_URL");
        assumeTrue(admin != null && data != null, "FASTMOCK_ADMIN_URL and FASTMOCK_DATA_URL are not set");
        fm = FastMock.connect(admin, System.getenv("FASTMOCK_TOKEN"));
    }

    static HttpResponse<String> send(String method, String path, String json) throws Exception {
        var b = HttpRequest.newBuilder(URI.create(data + path)).header("content-type", "application/json");
        b.method(method, json == null ? HttpRequest.BodyPublishers.noBody() : HttpRequest.BodyPublishers.ofString(json));
        return http.send(b.build(), HttpResponse.BodyHandlers.ofString());
    }

    @Test
    void stubJournalAndCallbacks() throws Exception {
        String tag = "it-" + UUID.randomUUID();
        String cid = "msg-" + UUID.randomUUID();
        fm.createStub(Stub.post("/it/{ver}/send")
                .bodyJsonPath("$.phone", Match.matches("380\\d{9}"))
                .respond(202)
                .jsonBody(Map.of("ver", "{{request.pathParams.ver}}", "id", "{{request.json.id}}"))
                .callback(Callback.post(data + "/it/callback-sink").jsonBody(Map.of("status", "DELIVERED")).correlation("{{request.json.id}}"))
                .tag(tag));
        fm.createStub(Stub.post("/it/callback-sink").respond(204).tag(tag));

        var r = send("POST", "/it/v3/send", "{\"phone\":\"380501234567\",\"id\":\"" + cid + "\"}");
        assertEquals(202, r.statusCode());
        assertTrue(r.body().contains("\"ver\":\"v3\""), r.body());

        var cbs = fm.awaitCallbacks(cid, 1, Duration.ofSeconds(10));
        assertEquals("sent", cbs.getFirst().status());
        assertEquals("DELIVERED", cbs.getFirst().json().get("status").asText());

        var reqs = fm.awaitRequests(RequestFilter.path("/it/v3/send").withCorrelation(cid), 1, Duration.ofSeconds(5));
        assertEquals("380501234567", reqs.getFirst().json().get("phone").asText());
        fm.awaitRequests(RequestFilter.path("/it/callback-sink"), 1, Duration.ofSeconds(5));

        assertEquals(404, send("POST", "/it/v3/send", "{\"phone\":\"1\"}").statusCode());
        assertTrue(fm.countRequests(RequestFilter.path("/it/v3/send").matched(false)) >= 1);
        assertEquals(2, fm.deleteStubsByTag(tag));
    }

    @Test
    void scenariosAndErrors() throws Exception {
        String tag = "it-" + UUID.randomUUID();
        String sc = "order-" + UUID.randomUUID();
        fm.createStub(Stub.get("/it/order").scenario(sc, "Started", "paid").body("first").tag(tag));
        fm.createStub(Stub.get("/it/order").scenario(sc, "paid", null).body("again").tag(tag));
        assertEquals("first", send("GET", "/it/order", null).body());
        assertEquals("again", send("GET", "/it/order", null).body());
        fm.setScenarioState(sc, "Started");
        assertEquals("first", send("GET", "/it/order", null).body());
        var e = assertThrows(FastMockException.class, () -> fm.createStub(Stub.get("/x").respond(42)));
        assertEquals(400, e.status());
        fm.deleteStubsByTag(tag);
    }

    @Test
    void openApiImport() throws Exception {
        String spec = """
                openapi: 3.0.3
                info: {title: it, version: "1"}
                paths:
                  /it-api/items/{id}:
                    get:
                      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
                      responses:
                        "200": {description: ok, content: {application/json: {example: {id: 1, name: thing}}}}
                """;
        var res = fm.importOpenApi(spec, "it-openapi", "", true);
        assertEquals(2, res.imported());
        assertEquals(200, send("GET", "/it-api/items/5", null).statusCode());
        assertEquals(400, send("GET", "/it-api/items/x", null).statusCode());
        fm.deleteStubsByTag("it-openapi");
    }

    @Test
    void verificationFilesAndSnapshot() throws Exception {
        String tag = "it-verify-" + UUID.randomUUID();
        String path = "/it-verify/" + UUID.randomUUID();
        fm.createStub(Stub.post(path).respond(201).body("ok").tag(tag));
        assertEquals(201, send("POST", path, "{\"n\":1}").statusCode());
        var pattern = FastMock.JSON.createObjectNode().put("method", "POST").put("urlPath", path);
        fm.awaitRequests(RequestFilter.all().withPathPrefix(path), 1, Duration.ofSeconds(10));
        assertEquals(1, fm.verifyExactly(pattern, 1).count());
        fm.verifyNever(FastMock.JSON.createObjectNode().put("method", "DELETE").put("urlPath", path));
        assertThrows(AssertionError.class, () -> fm.verifyAtLeast(pattern, 2));
        assertEquals(1, fm.findRequests(pattern).size());
        fm.verifySequence(java.util.List.of(pattern));

        fm.putFile("it/" + tag + ".json", "{\"a\":1}".getBytes(), "application/json");
        assertTrue(fm.listFiles().toString().contains(tag));
        fm.deleteFile("it/" + tag + ".json");

        var snap = fm.snapshot(FastMock.JSON.readTree("{\"filters\": {\"request\": {\"urlPath\": \"" + path + "\"}}, \"persist\": false}"));
        assertEquals(1, snap.path("stubs").size());
        fm.deleteStubsByTag(tag);
    }

    @Test
    void messageCapture() throws Exception {
        assumeTrue(System.getenv("FASTMOCK_TEST_MQTT") != null, "FASTMOCK_TEST_MQTT is not set");
        assumeTrue(fm.call("GET", "version", null).path("features").toString().contains("\"capture\""),
                "message capture is not enabled on this FastMock");
        String id = UUID.randomUUID().toString().substring(0, 8);
        String source = "it-" + id;
        String tag = "it-capture-" + id;
        var created = fm.createCaptureSource(CaptureSource.mqtt(source, "it/" + id + "/#").qos(1).description("Java client test"));
        assertEquals(source, created.name());
        var uuid = UUID.fromString(created.id());
        try {
            assertEquals("running", fm.awaitCaptureSourceRunning(uuid, Duration.ofSeconds(20)).status().state());
            assertTrue(fm.captureSources().stream().anyMatch(s -> s.name().equals(source)));

            fm.createStub(Stub.post("/it-capture/" + id + "/{order}").respond(202).body("ok").tag(tag));
            var raw = FastMock.JSON.readTree("""
                    {"tags": ["%s"], "request": {"method": "POST", "urlPathTemplate": "/it-capture/%s/ship/{order}"},
                     "response": {"status": 202},
                     "messages": [{"protocol": "mqtt", "topic": "it/%s/status", "qos": 1,
                       "headers": {"event-type": "OrderShipped"},
                       "jsonValue": {"orderId": "{{request.pathParams.order}}", "status": "SHIPPED"}}]}
                    """.formatted(tag, id, id));
            fm.createStub(raw);

            Instant before = Instant.now().minusSeconds(1);
            assertEquals(202, send("POST", "/it-capture/" + id + "/ship/A-1", "{}").statusCode());
            assertEquals(202, send("POST", "/it-capture/" + id + "/ship/A-2", "{}").statusCode());

            var match = MessageMatch.source(source).bodyJsonPath("$.orderId", "A-1");
            var found = fm.waitForMessages(match, Duration.ofSeconds(15), before);
            assertEquals(1, found.size());
            assertEquals("it/" + id + "/status", found.getFirst().topic());
            assertEquals("OrderShipped", found.getFirst().headers().get("event-type"));

            var detail = fm.capturedMessage(UUID.fromString(found.getFirst().id()));
            assertEquals("json", detail.body().encoding());
            assertEquals("SHIPPED", detail.body().value().get("status").asText());

            assertEquals(2, fm.waitForMessages(MessageMatch.source(source), 2, Duration.ofSeconds(15), before).size());
            assertEquals(2, fm.capturedMessages(CaptureFilter.source(source)).size());
            assertEquals(1, fm.findMessages(match, before, 10).size());
            assertEquals(1, fm.verifyMessageCount(match, 1, before).count());
            fm.verifyMessagesAtLeast(MessageMatch.source(source), 2, before);
            fm.verifyNoMessages(MessageMatch.source(source).key("nobody"), before);

            var late = assertThrows(AssertionError.class,
                    () -> fm.waitForMessages(MessageMatch.source(source).bodyJsonPath("$.orderId", "nope"), Duration.ofMillis(300), before));
            assertTrue(late.getMessage().contains("timed out"), late.getMessage());
            var wrong = assertThrows(AssertionError.class, () -> fm.verifyMessageCount(match, 2, before));
            assertTrue(wrong.getMessage().contains("exactly 2"), wrong.getMessage());

            assertEquals(2, fm.clearCapturedMessages(source));
            fm.verifyNoMessages(MessageMatch.source(source), null);
        } finally {
            fm.deleteCaptureSource(uuid, true);
            fm.deleteStubsByTag(tag);
        }
        assertTrue(fm.captureSources().stream().noneMatch(s -> s.name().equals(source)));
    }
}
