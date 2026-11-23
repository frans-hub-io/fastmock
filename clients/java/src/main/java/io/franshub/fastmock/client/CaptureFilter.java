package io.franshub.fastmock.client;

import java.time.Instant;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.stream.Collectors;

/** Filter for listing captured messages. All criteria are optional and combined with AND. */
public final class CaptureFilter {

    private final Map<String, String> params = new LinkedHashMap<>();

    public static CaptureFilter all() {
        return new CaptureFilter();
    }

    public static CaptureFilter source(String nameOrId) {
        return new CaptureFilter().withSource(nameOrId);
    }

    public CaptureFilter withSource(String nameOrId) {
        return and("source", nameOrId);
    }

    public CaptureFilter withProtocol(String protocol) {
        return and("protocol", protocol);
    }

    public CaptureFilter withTopic(String topicOrRoutingKey) {
        return and("topic", topicOrRoutingKey);
    }

    public CaptureFilter since(Instant v) {
        return and("since", v.toString());
    }

    public CaptureFilter until(Instant v) {
        return and("until", v.toString());
    }

    /** Page size, 1 to 500. */
    public CaptureFilter limit(int v) {
        return and("limit", Integer.toString(v));
    }

    /** Oldest first instead of newest first. */
    public CaptureFilter oldestFirst() {
        return and("order", "oldest");
    }

    private CaptureFilter and(String k, String v) {
        params.put(k, v);
        return this;
    }

    Map<String, String> params() {
        return params;
    }

    String toQuery() {
        return params.entrySet().stream().map(e -> e.getKey() + "=" + FastMock.q(e.getValue())).collect(Collectors.joining("&"));
    }

    @Override
    public String toString() {
        return toQuery();
    }
}
