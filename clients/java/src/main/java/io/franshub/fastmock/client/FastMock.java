package io.franshub.fastmock.client;

import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.io.IOException;
import java.net.URI;
import java.net.URLEncoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.UUID;
import java.util.function.Predicate;

/**
 * Client for the FastMock control plane (admin port). Thread-safe; create one per test suite.
 */
public final class FastMock {

    static final ObjectMapper JSON = new ObjectMapper().configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

    private final URI admin;
    private final String token;
    private final HttpClient http;
    private final Duration timeout;
    private final String space;
    private final boolean payloadsInErrors;

    private FastMock(URI admin, String token, Duration timeout) {
        this(admin, token, timeout, null, null, false);
    }

    private FastMock(URI admin, String token, Duration timeout, String space, HttpClient shared, boolean payloadsInErrors) {
        this.space = space;
        this.payloadsInErrors = payloadsInErrors;
        this.admin = URI.create(admin.toString().replaceAll("/+$", "") + "/");
        this.token = Objects.requireNonNull(token, "token");
        this.timeout = timeout;
        this.http = shared != null ? shared
                : HttpClient.newBuilder().connectTimeout(timeout).followRedirects(HttpClient.Redirect.NEVER).build();
    }

    /**
     * A client whose message capture calls ({@code createCaptureSource}, {@code waitForMessages}, ...) work in
     * the given space; the other calls are unchanged. Without it the caller's only space applies.
     */
    public FastMock inSpace(String space) {
        return new FastMock(admin, token, timeout, Objects.requireNonNull(space, "space"), http, payloadsInErrors);
    }

    /**
     * A client whose {@link #waitForMessages} timeout errors list the received messages with their payload
     * previews. Off by default: captured payloads can hold secrets, and assertion messages end up in CI logs and
     * test reports. The default error names only how many messages arrived, with their sources and topics.
     */
    public FastMock withPayloadsInErrors() {
        return new FastMock(admin, token, timeout, space, http, true);
    }

    /** Connects to the admin endpoint, for example {@code http://fastmock-admin:9090}, with a bearer token. */
    public static FastMock connect(String adminUrl, String token) {
        return new FastMock(URI.create(adminUrl), token, Duration.ofSeconds(10));
    }

    public static FastMock connect(String adminUrl, String token, Duration timeout) {
        return new FastMock(URI.create(adminUrl), token, timeout);
    }

    JsonNode call(String method, String path, Object body) {
        return call(method, path, body, 0);
    }

    JsonNode call(String method, String path, Object body, int acceptedErrorStatus) {
        try {
            HttpRequest.BodyPublisher publisher = body == null
                    ? HttpRequest.BodyPublishers.noBody()
                    : HttpRequest.BodyPublishers.ofString(JSON.writeValueAsString(body));
            return send(method, path, body == null ? null : "application/json", publisher, acceptedErrorStatus);
        } catch (IOException e) {
            throw new FastMockException("FastMock admin call failed: " + method + " " + path, e);
        }
    }

    JsonNode send(String method, String path, String contentType, HttpRequest.BodyPublisher publisher, int acceptedErrorStatus) {
        try {
            var b = HttpRequest.newBuilder(admin.resolve("__admin/" + path.replaceFirst("^/", "")))
                    .timeout(timeout)
                    .header("Authorization", "Bearer " + token)
                    .header("Accept", "application/json");
            if (contentType != null) {
                b.header("Content-Type", contentType);
            }
            b.method(method, publisher);
            HttpResponse<String> r = http.send(b.build(), HttpResponse.BodyHandlers.ofString());
            if (r.statusCode() >= 400 && r.statusCode() != acceptedErrorStatus) {
                throw new FastMockException(r.statusCode(), r.body());
            }
            return r.body() == null || r.body().isBlank() ? JSON.createObjectNode() : JSON.readTree(r.body());
        } catch (IOException e) {
            throw new FastMockException("FastMock admin call failed: " + method + " " + path, e);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new FastMockException("interrupted", e);
        }
    }

    static String q(String s) {
        return URLEncoder.encode(s, StandardCharsets.UTF_8);
    }

    // ---- stubs

    /** Creates a stub and returns its id. */
    public UUID createStub(Stub stub) {
        return UUID.fromString(call("POST", "stubs", stub.toJson()).get("id").asText());
    }

    public UUID createStub(JsonNode raw) {
        return UUID.fromString(call("POST", "stubs", raw).get("id").asText());
    }

    public void updateStub(UUID id, Stub stub) {
        call("PUT", "stubs/" + id, stub.toJson());
    }

    public void deleteStub(UUID id) {
        call("DELETE", "stubs/" + id, null);
    }

    /** Deletes all stubs with the tag; returns how many were removed. */
    public long deleteStubsByTag(String tag) {
        return call("DELETE", "stubs?tag=" + q(tag), null).path("deleted").asLong();
    }

    public JsonNode listStubs() {
        return call("GET", "stubs", null).path("stubs");
    }

    /** Applies a JSON merge patch (RFC 7396), for example {@code {"enabled": false}}; returns the patched stub. */
    public JsonNode patchStub(UUID id, JsonNode mergePatch) {
        return call("PATCH", "stubs/" + id, mergePatch);
    }

    /** Stubs whose metadata matches a WireMock-style pattern, e.g. {@code {"matchesJsonPath": "$.service", "equalTo": "billing"}}. */
    public JsonNode findStubsByMetadata(JsonNode pattern) {
        return call("POST", "stubs/find-by-metadata", pattern).path("stubs");
    }

    /** Deletes stubs whose metadata matches; returns how many were removed. */
    public long removeStubsByMetadata(JsonNode pattern) {
        return call("POST", "stubs/remove-by-metadata", pattern).path("deleted").asLong();
    }

    // ---- body files

    /** Uploads or replaces a body file referenced by {@code response.bodyFileName}. */
    public void putFile(String name, byte[] content, String contentType) {
        send("PUT", "files/" + name, contentType == null ? "application/octet-stream" : contentType,
                HttpRequest.BodyPublishers.ofByteArray(content), 0);
    }

    public void deleteFile(String name) {
        call("DELETE", "files/" + name, null);
    }

    public JsonNode listFiles() {
        return call("GET", "files", null).path("files");
    }

    public ImportResult importOpenApi(String spec, String tag, String pathPrefix, boolean validateRequests) {
        ObjectNode b = JSON.createObjectNode().put("spec", spec).put("tag", tag).put("pathPrefix", pathPrefix == null ? "" : pathPrefix).put("validateRequests", validateRequests);
        return JSON.convertValue(call("POST", "import/openapi", b), ImportResult.class);
    }

    public ImportResult importWireMock(JsonNode mappings, String tag) {
        ObjectNode b = JSON.createObjectNode().put("tag", tag);
        b.set("mappings", mappings);
        return JSON.convertValue(call("POST", "import/wiremock", b), ImportResult.class);
    }

    // ---- request journal

    public List<RecordedRequest> requests(RequestFilter filter) {
        JsonNode r = call("GET", "requests?" + filter.toQuery(), null).path("requests");
        List<RecordedRequest> out = new ArrayList<>();
        r.forEach(n -> out.add(JSON.convertValue(n, RecordedRequest.class)));
        return out;
    }

    public long countRequests(RequestFilter filter) {
        return call("GET", "requests/count?" + filter.toQuery(), null).path("count").asLong();
    }

    /** Polls the journal until at least {@code n} requests match or the timeout passes; returns the matching requests. */
    public List<RecordedRequest> awaitRequests(RequestFilter filter, int n, Duration within) {
        return await(() -> requests(filter), l -> l.size() >= n, within, "expected at least " + n + " requests matching " + filter.toQuery());
    }

    /** Journal entries matching a stub request pattern, newest first. */
    public List<RecordedRequest> findRequests(JsonNode requestPattern) {
        JsonNode r = call("POST", "requests/find?limit=10000", requestPattern).path("requests");
        List<RecordedRequest> out = new ArrayList<>();
        r.forEach(n -> out.add(JSON.convertValue(n, RecordedRequest.class)));
        return out;
    }

    /**
     * Verifies the journal with a raw body, e.g. {@code {"request": {...}, "atLeast": 1}}; never throws on a
     * failed expectation, check {@link VerifyResult#verified()}.
     */
    public VerifyResult verify(JsonNode body) {
        return JSON.convertValue(call("POST", "verify", body, 422), VerifyResult.class);
    }

    /** Asserts that exactly {@code times} requests matched the pattern; the AssertionError lists near misses. */
    public VerifyResult verifyExactly(JsonNode requestPattern, long times) {
        return expect(JSON.createObjectNode().put("exactly", times), requestPattern);
    }

    public VerifyResult verifyAtLeast(JsonNode requestPattern, long times) {
        return expect(JSON.createObjectNode().put("atLeast", times), requestPattern);
    }

    public VerifyResult verifyAtMost(JsonNode requestPattern, long times) {
        return expect(JSON.createObjectNode().put("atMost", times), requestPattern);
    }

    public VerifyResult verifyNever(JsonNode requestPattern) {
        return verifyAtMost(requestPattern, 0);
    }

    private VerifyResult expect(ObjectNode body, JsonNode requestPattern) {
        body.set("request", requestPattern);
        VerifyResult r = verify(body);
        if (!r.verified()) {
            throw new AssertionError(r.error() + (r.details() == null || r.details().isEmpty() ? "" : "; " + String.join("; ", r.details())));
        }
        return r;
    }

    /** Asserts that requests matching the patterns arrived in this order (other requests may come in between). */
    public void verifySequence(List<JsonNode> requestPatterns) {
        ObjectNode body = JSON.createObjectNode();
        body.set("requests", JSON.valueToTree(requestPatterns));
        JsonNode r = call("POST", "verify/sequence", body, 422);
        if (!r.path("verified").asBoolean()) {
            throw new AssertionError(r.path("error").asText() + "; " + r.path("details"));
        }
    }

    /** Converts journal entries into stubs (WireMock snapshot), e.g. {@code {"filters": {...}, "persist": true}}. */
    public JsonNode snapshot(JsonNode body) {
        return call("POST", "recordings/snapshot", body);
    }

    public void clearRequests() {
        call("DELETE", "requests", null);
    }

    // ---- message capture

    private String capturePath(String path, String query) {
        StringBuilder b = new StringBuilder("capture/").append(path);
        String sep = "?";
        if (space != null) {
            b.append(sep).append("space=").append(q(space));
            sep = "&";
        }
        if (query != null && !query.isEmpty()) {
            b.append(sep).append(query);
        }
        return b.toString();
    }

    /** Creates a capture source (needs {@code messages:write}) and returns it with its state. */
    public CaptureSourceView createCaptureSource(CaptureSource source) {
        UUID id = source.idOrNew();
        return JSON.convertValue(call("PUT", capturePath("sources/" + id, null), source.toJson()), CaptureSourceView.class);
    }

    public List<CaptureSourceView> captureSources() {
        List<CaptureSourceView> out = new ArrayList<>();
        call("GET", capturePath("sources", null), null).path("sources")
                .forEach(n -> out.add(JSON.convertValue(n, CaptureSourceView.class)));
        return out;
    }

    /** Waits until the source consumes ({@code state} {@code running}), so messages published next are captured. */
    public CaptureSourceView awaitCaptureSourceRunning(UUID id, Duration within) {
        return await(() -> JSON.convertValue(call("GET", capturePath("sources/" + id, null), null), CaptureSourceView.class),
                v -> v.status() != null && "running".equals(v.status().state()), within,
                "capture source " + id + " is not running");
    }

    /** Deletes a source; with {@code purge} also its messages (needs {@code messages:delete}). */
    public void deleteCaptureSource(UUID id, boolean purge) {
        call("DELETE", capturePath("sources/" + id, purge ? "purge=true" : null), null);
    }

    /** Lists captured messages, newest first (oldest first with {@link CaptureFilter#oldestFirst()}). */
    public List<CapturedMessage> capturedMessages(CaptureFilter filter) {
        List<CapturedMessage> out = new ArrayList<>();
        call("GET", capturePath("messages", filter.toQuery()), null).path("messages")
                .forEach(n -> out.add(JSON.convertValue(n, CapturedMessage.class)));
        return out;
    }

    /** Messages matching the conditions, newest first, at most {@code limit} (1 to 500) of them. */
    public List<CapturedMessage> findMessages(MessageMatch match, Instant since, int limit) {
        ObjectNode b = JSON.createObjectNode().put("limit", limit);
        b.set("match", match.toJson());
        if (since != null) {
            b.put("since", since.toString());
        }
        List<CapturedMessage> out = new ArrayList<>();
        call("POST", capturePath("messages/find", null), b).path("messages")
                .forEach(n -> out.add(JSON.convertValue(n, CapturedMessage.class)));
        return out;
    }

    /** One message with its decoded body. */
    public CapturedMessageDetail capturedMessage(UUID id) {
        return JSON.convertValue(call("GET", capturePath("messages/" + id, null), null), CapturedMessageDetail.class);
    }

    /**
     * Blocks until {@code count} messages matching {@code match} were captured, at most {@code timeout} (up to
     * 60 s), and returns them oldest first. Only messages received at or after {@code since} (or arriving
     * during the wait, when {@code since} is null) count. Throws {@link AssertionError} with what arrived on
     * timeout.
     */
    public List<CapturedMessage> waitForMessages(MessageMatch match, int count, Duration timeout, Instant since) {
        ObjectNode b = JSON.createObjectNode().put("count", count).put("timeoutMs", timeout.toMillis());
        b.set("match", match.toJson());
        if (since != null) {
            b.put("since", since.toString());
        }
        JsonNode r = call("POST", capturePath("messages/wait", null), b, 408);
        if (r.has("error")) {
            List<String> details = new ArrayList<>();
            r.path("details").forEach(n -> details.add(n.asText()));
            throw new AssertionError(r.path("error").asText() + (details.isEmpty() ? "" : "; " + String.join("; ", details))
                    + "; received so far: " + describeReceived(r.path("messages")));
        }
        List<CapturedMessage> out = new ArrayList<>();
        r.path("messages").forEach(n -> out.add(JSON.convertValue(n, CapturedMessage.class)));
        return out;
    }

    public List<CapturedMessage> waitForMessages(MessageMatch match, Duration timeout, Instant since) {
        return waitForMessages(match, 1, timeout, since);
    }

    private String describeReceived(JsonNode messages) {
        if (payloadsInErrors) {
            return messages.toString();
        }
        List<String> out = new ArrayList<>();
        messages.forEach(n -> out.add(n.path("source").asText() + " " + n.path("topic").asText()));
        return out.size() + (out.isEmpty() ? "" : " " + out) + " (payloads left out; see FastMock.withPayloadsInErrors())";
    }

    /** Counts stored messages without waiting; never throws on a failed expectation, check {@link VerifyResult#verified()}. */
    public VerifyResult verifyMessages(JsonNode body) {
        return JSON.convertValue(call("POST", capturePath("verify", null), body, 422), VerifyResult.class);
    }

    /** Asserts that exactly {@code times} stored messages (received at or after {@code since}, if given) match. */
    public VerifyResult verifyMessageCount(MessageMatch match, long times, Instant since) {
        return expectMessages(match, since, JSON.createObjectNode().put("count", times));
    }

    public VerifyResult verifyMessagesAtLeast(MessageMatch match, long times, Instant since) {
        return expectMessages(match, since, JSON.createObjectNode().put("atLeast", times));
    }

    public VerifyResult verifyMessagesAtMost(MessageMatch match, long times, Instant since) {
        return expectMessages(match, since, JSON.createObjectNode().put("atMost", times));
    }

    public VerifyResult verifyNoMessages(MessageMatch match, Instant since) {
        return verifyMessagesAtMost(match, 0, since);
    }

    private VerifyResult expectMessages(MessageMatch match, Instant since, ObjectNode body) {
        body.set("match", match.toJson());
        if (since != null) {
            body.put("since", since.toString());
        }
        VerifyResult r = verifyMessages(body);
        if (!r.verified()) {
            throw new AssertionError(r.error() + (r.details() == null || r.details().isEmpty() ? "" : "; " + String.join("; ", r.details())));
        }
        return r;
    }

    /** Deletes the messages of the space, or of one source (name or id); returns how many were removed. */
    public long clearCapturedMessages(String source) {
        String query = source == null ? null : "source=" + q(source);
        return call("DELETE", capturePath("messages", query), null).path("deleted").asLong();
    }

    // ---- callbacks

    public List<CallbackRecord> callbacks(String correlation) {
        JsonNode r = call("GET", "callbacks?correlation=" + q(correlation), null).path("callbacks");
        List<CallbackRecord> out = new ArrayList<>();
        r.forEach(n -> out.add(JSON.convertValue(n, CallbackRecord.class)));
        return out;
    }

    /** Waits until every callback with this correlation id is delivered (or failed) and returns them. */
    public List<CallbackRecord> awaitCallbacks(String correlation, int n, Duration within) {
        return await(() -> callbacks(correlation),
                l -> l.size() >= n && l.stream().noneMatch(c -> "pending".equals(c.status())),
                within, "expected " + n + " finished callbacks for correlation " + correlation);
    }

    // ---- scenarios, key-value, reset

    public void setScenarioState(String scenario, String state) {
        call("PUT", "scenarios/" + q(scenario), Map.of("state", state));
    }

    public void resetScenarios() {
        call("DELETE", "scenarios", null);
    }

    public void putValue(String key, String value, Duration ttl) {
        ObjectNode b = JSON.createObjectNode().put("value", value);
        if (ttl != null) {
            b.put("ttlSeconds", ttl.toSeconds());
        }
        call("PUT", "kv/" + q(key), b);
    }

    /** Clears the journal, callbacks and scenario states (requires the admin role). Stubs stay. */
    public void reset() {
        call("POST", "reset", null);
    }

    /** Escape hatch for plugin admin APIs, for example {@code plugin("registry", "PUT", "companies/123", body)}. */
    public JsonNode plugin(String plugin, String method, String path, Object body) {
        return call(method, "plugins/" + plugin + "/" + path.replaceFirst("^/", ""), body);
    }

    static <T> T await(java.util.function.Supplier<T> poll, Predicate<T> done, Duration within, String message) {
        Instant deadline = Instant.now().plus(within);
        T last = poll.get();
        while (!done.test(last)) {
            if (Instant.now().isAfter(deadline)) {
                throw new AssertionError(message + "; last result: " + last);
            }
            try {
                Thread.sleep(100);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new FastMockException("interrupted", e);
            }
            last = poll.get();
        }
        return last;
    }

    public record ImportResult(long imported, long removed, List<String> warnings) {
    }

    public record VerifyResult(boolean verified, long count, String expected, String error, List<String> details,
                               JsonNode nearMisses) {
    }

    public record RecordedRequest(long id, String at, String source, String method, String path, String query,
                                  Map<String, String> headers, String body, int status, String responseBody,
                                  String stubId, String correlation, boolean matched, long durationMs, String traceId) {
        public JsonNode json() {
            try {
                return JSON.readTree(body);
            } catch (IOException e) {
                throw new FastMockException("request body is not JSON", e);
            }
        }
    }

    public record CaptureSourceView(String id, String name, String space, String protocol, List<String> topics,
                                    String exchange, List<String> bindingKeys, Integer qos, String startFrom,
                                    Integer maxMessages, boolean enabled, String description, boolean fromConfig,
                                    String updatedAt, CaptureStatus status) {
    }

    public record CaptureStatus(String state, long received, long dropped, String lastMessageAt, String since,
                                long stored, long connects, String lastError) {
    }

    public record CapturedMessage(String id, String space, String sourceId, String source, String protocol,
                                  String topic, String exchange, String key, Map<String, String> headers,
                                  JsonNode properties, String contentType, long size, boolean truncated,
                                  String preview, String receivedAt) {
    }

    /** A message with its decoded body: {@code encoding} is json, avro, text, base64 or empty. */
    public record CapturedMessageDetail(String id, String space, String sourceId, String source, String protocol,
                                        String topic, String exchange, String key, Map<String, String> headers,
                                        JsonNode properties, String contentType, long size, boolean truncated,
                                        String preview, String receivedAt, DecodedBody body) {
    }

    public record DecodedBody(String encoding, JsonNode value, Long schemaId, String error) {
    }

    public record CallbackRecord(String id, String source, String correlation, String url, String method,
                                 Map<String, String> headers, String body, String dueAt, int attempts,
                                 int maxAttempts, String status, String lastError, Integer lastStatus,
                                 String createdAt, String sentAt, String traceparent) {
        public JsonNode json() {
            try {
                return JSON.readTree(body);
            } catch (IOException e) {
                throw new FastMockException("callback body is not JSON", e);
            }
        }
    }
}
