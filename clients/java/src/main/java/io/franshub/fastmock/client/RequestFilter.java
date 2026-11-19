package io.franshub.fastmock.client;

import java.time.Instant;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.stream.Collectors;

/** Filter for the request journal. All criteria are optional and combined with AND. */
public final class RequestFilter {

    private final Map<String, String> params = new LinkedHashMap<>();

    public static RequestFilter all() {
        return new RequestFilter();
    }

    public static RequestFilter source(String source) {
        return new RequestFilter().and("source", source);
    }

    public static RequestFilter correlation(String correlation) {
        return new RequestFilter().and("correlation", correlation);
    }

    public static RequestFilter path(String prefix) {
        return new RequestFilter().and("pathPrefix", prefix);
    }

    public RequestFilter withSource(String v) {
        return and("source", v);
    }

    public RequestFilter withMethod(String v) {
        return and("method", v);
    }

    public RequestFilter withPathPrefix(String v) {
        return and("pathPrefix", v);
    }

    public RequestFilter withCorrelation(String v) {
        return and("correlation", v);
    }

    public RequestFilter withBodyContaining(String v) {
        return and("bodyContains", v);
    }

    public RequestFilter matched(boolean v) {
        return and("matched", Boolean.toString(v));
    }

    public RequestFilter since(Instant v) {
        return and("since", v.toString());
    }

    public RequestFilter limit(int v) {
        return and("limit", Integer.toString(v));
    }

    private RequestFilter and(String k, String v) {
        params.put(k, v);
        return this;
    }

    String toQuery() {
        return params.entrySet().stream().map(e -> e.getKey() + "=" + FastMock.q(e.getValue())).collect(Collectors.joining("&"));
    }

    @Override
    public String toString() {
        return toQuery();
    }
}
