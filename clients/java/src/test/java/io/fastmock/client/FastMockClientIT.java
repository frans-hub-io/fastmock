package io.fastmock.client;

import io.fastmock.client.Stub.Callback;
import io.fastmock.client.Stub.Match;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
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
}
