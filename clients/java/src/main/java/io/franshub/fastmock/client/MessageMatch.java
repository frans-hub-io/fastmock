package io.franshub.fastmock.client;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

/**
 * Conditions on captured messages; all given ones must hold. String conditions are exact matches unless the
 * {@code *Matching} variants are used. Example:
 * <pre>{@code
 * MessageMatch.source("orders").key("A-1").bodyJsonPath("$.status", "SHIPPED")
 * }</pre>
 */
public final class MessageMatch {

    private final ObjectNode json = FastMock.JSON.createObjectNode();

    public static MessageMatch any() {
        return new MessageMatch();
    }

    public static MessageMatch source(String name) {
        return new MessageMatch().sourceName(name);
    }

    public MessageMatch sourceName(String name) {
        return equalTo("source", name);
    }

    public MessageMatch protocol(String protocol) {
        json.put("protocol", protocol);
        return this;
    }

    public MessageMatch topic(String topic) {
        return equalTo("topic", topic);
    }

    public MessageMatch topicMatching(String regex) {
        return matcher("topic", "matches", regex);
    }

    public MessageMatch exchange(String exchange) {
        return equalTo("exchange", exchange);
    }

    public MessageMatch key(String key) {
        return equalTo("key", key);
    }

    public MessageMatch contentType(String contentType) {
        return equalTo("contentType", contentType);
    }

    public MessageMatch header(String name, String value) {
        return nested("headers", name, "equalTo", value);
    }

    public MessageMatch property(String name, String value) {
        return nested("properties", name, "equalTo", value);
    }

    public MessageMatch bodyContains(String text) {
        body().add(FastMock.JSON.createObjectNode().put("contains", text));
        return this;
    }

    public MessageMatch bodyEqualsJson(JsonNode expected, boolean ignoreExtraElements) {
        ObjectNode c = FastMock.JSON.createObjectNode();
        c.set("equalToJson", expected);
        c.put("ignoreExtraElements", ignoreExtraElements);
        body().add(c);
        return this;
    }

    /** The JSON path must evaluate to a value equal to {@code value} (as text); Avro bodies are decoded first. */
    public MessageMatch bodyJsonPath(String path, String value) {
        ObjectNode c = FastMock.JSON.createObjectNode().put("jsonPath", path);
        c.putObject("value").put("equalTo", value);
        body().add(c);
        return this;
    }

    public ObjectNode toJson() {
        return json.deepCopy();
    }

    private MessageMatch equalTo(String field, String value) {
        return matcher(field, "equalTo", value);
    }

    private MessageMatch matcher(String field, String op, String value) {
        json.putObject(field).put(op, value);
        return this;
    }

    private MessageMatch nested(String field, String name, String op, String value) {
        ObjectNode m = json.has(field) ? (ObjectNode) json.get(field) : json.putObject(field);
        m.putObject(name).put(op, value);
        return this;
    }

    private ArrayNode body() {
        return json.has("body") ? (ArrayNode) json.get("body") : json.putArray("body");
    }
}
