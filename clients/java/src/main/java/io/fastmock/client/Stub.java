package io.fastmock.client;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.time.Duration;

/**
 * Fluent stub definition. Example:
 * <pre>{@code
 * Stub.post("/api/{ver}/send")
 *     .bodyJsonPath("$.phone", Match.matches("380\\d{9}"))
 *     .respond(202).jsonBody(Map.of("id", "{{uuid}}"))
 *     .callback(Callback.post("http://app/cb").jsonBody(Map.of("status", "DELIVERED")).delay(Duration.ofSeconds(1)))
 *     .tag("my-test");
 * }</pre>
 */
public final class Stub {

    private final ObjectNode root = FastMock.JSON.createObjectNode();
    private final ObjectNode request = root.putObject("request");
    private final ObjectNode response = root.putObject("response");

    private Stub(String method, String pathTemplate) {
        if (method != null) {
            request.put("method", method);
        }
        request.put("urlPathTemplate", pathTemplate);
    }

    public static Stub get(String path) {
        return new Stub("GET", path);
    }

    public static Stub post(String path) {
        return new Stub("POST", path);
    }

    public static Stub put(String path) {
        return new Stub("PUT", path);
    }

    public static Stub delete(String path) {
        return new Stub("DELETE", path);
    }

    public static Stub any(String path) {
        return new Stub(null, path);
    }

    public Stub name(String name) {
        root.put("name", name);
        return this;
    }

    /** Lower numbers win; the default is 5. */
    public Stub priority(int p) {
        root.put("priority", p);
        return this;
    }

    public Stub tag(String tag) {
        (root.has("tags") ? (ArrayNode) root.get("tags") : root.putArray("tags")).add(tag);
        return this;
    }

    public Stub query(String name, Match m) {
        obj(request, "query").set(name, m.node());
        return this;
    }

    public Stub header(String name, Match m) {
        obj(request, "headers").set(name.toLowerCase(), m.node());
        return this;
    }

    public Stub pathParam(String name, Match m) {
        obj(request, "pathParams").set(name, m.node());
        return this;
    }

    public Stub bodyJsonPath(String jsonPath, Match m) {
        ObjectNode b = arr(request, "body").addObject().put("jsonPath", jsonPath);
        if (m != null) {
            b.set("value", m.node());
        }
        return this;
    }

    public Stub bodyEqualToJson(Object json, boolean ignoreExtraElements) {
        ObjectNode b = arr(request, "body").addObject();
        b.set("equalToJson", FastMock.JSON.valueToTree(json));
        b.put("ignoreExtraElements", ignoreExtraElements);
        return this;
    }

    public Stub bodyContains(String s) {
        arr(request, "body").addObject().put("contains", s);
        return this;
    }

    public Stub bodyJsonSchema(Object schema) {
        arr(request, "body").addObject().set("jsonSchema", FastMock.JSON.valueToTree(schema));
        return this;
    }

    public Stub respond(int status) {
        response.put("status", status);
        return this;
    }

    public Stub responseHeader(String name, String value) {
        obj(response, "headers").put(name, value);
        return this;
    }

    /** Text body; Handlebars templates such as {@code {{request.pathParams.id}}} are rendered unless {@link #literal()} is set. */
    public Stub body(String body) {
        response.put("body", body);
        return this;
    }

    public Stub jsonBody(Object json) {
        response.set("jsonBody", FastMock.JSON.valueToTree(json));
        return this;
    }

    public Stub literal() {
        response.put("templated", false);
        return this;
    }

    public Stub delay(Duration d) {
        response.putObject("delay").put("fixedMs", d.toMillis());
        return this;
    }

    public Stub randomDelay(Duration min, Duration max) {
        response.putObject("delay").put("minMs", min.toMillis()).put("maxMs", max.toMillis());
        return this;
    }

    public Stub fault(String fault) {
        response.put("fault", fault);
        return this;
    }

    public Stub proxyTo(String baseUrl) {
        response.putObject("proxy").put("baseUrl", baseUrl);
        return this;
    }

    public Stub scenario(String name, String requiredState, String newState) {
        ObjectNode s = root.putObject("scenario").put("name", name);
        if (requiredState != null) {
            s.put("requiredState", requiredState);
        }
        if (newState != null) {
            s.put("newState", newState);
        }
        return this;
    }

    public Stub callback(Callback c) {
        arr(root, "callbacks").add(c.node());
        return this;
    }

    public JsonNode toJson() {
        return root.deepCopy();
    }

    private static ObjectNode obj(ObjectNode parent, String name) {
        return parent.has(name) ? (ObjectNode) parent.get(name) : parent.putObject(name);
    }

    private static ArrayNode arr(ObjectNode parent, String name) {
        return parent.has(name) ? (ArrayNode) parent.get(name) : parent.putArray(name);
    }

    /** String matchers for query parameters, headers, path parameters and JSON path values. */
    public record Match(ObjectNode node) {
        public static Match equalTo(String v) {
            return new Match(FastMock.JSON.createObjectNode().put("equalTo", v));
        }

        public static Match contains(String v) {
            return new Match(FastMock.JSON.createObjectNode().put("contains", v));
        }

        public static Match matches(String regex) {
            return new Match(FastMock.JSON.createObjectNode().put("matches", regex));
        }

        public static Match absent() {
            return new Match(FastMock.JSON.createObjectNode().put("absent", true));
        }

        public static Match present() {
            return new Match(FastMock.JSON.createObjectNode());
        }

        public Match ignoringCase() {
            return new Match(node.deepCopy().put("caseInsensitive", true));
        }
    }

    /** Durable callback (webhook) sent by FastMock after the response, with retries. */
    public static final class Callback {
        private final ObjectNode node = FastMock.JSON.createObjectNode();

        private Callback(String method, String url) {
            node.put("method", method).put("url", url);
        }

        public static Callback post(String url) {
            return new Callback("POST", url);
        }

        public static Callback to(String method, String url) {
            return new Callback(method, url);
        }

        public Callback header(String name, String value) {
            (node.has("headers") ? (ObjectNode) node.get("headers") : node.putObject("headers")).put(name, value);
            return this;
        }

        public Callback body(String body) {
            node.put("body", body);
            return this;
        }

        public Callback jsonBody(Object json) {
            node.set("jsonBody", FastMock.JSON.valueToTree(json));
            return this;
        }

        public Callback delay(Duration d) {
            node.putObject("delay").put("fixedMs", d.toMillis());
            return this;
        }

        public Callback maxAttempts(int n) {
            node.put("maxAttempts", n);
            return this;
        }

        /** Correlation id template, e.g. {@code {{request.json.messageId}}}; use it with {@link FastMock#awaitCallbacks}. */
        public Callback correlation(String template) {
            node.put("correlation", template);
            return this;
        }

        /** Signs the body with HMAC; the key is a server-side secret referenced by name, never sent by the client. */
        public Callback hmac(String header, String secretRef, String algorithm, String prefix) {
            ObjectNode h = node.putObject("hmac").put("header", header).put("secretRef", secretRef).put("algorithm", algorithm);
            if (prefix != null) {
                h.put("prefix", prefix);
            }
            return this;
        }

        ObjectNode node() {
            return node;
        }
    }
}
