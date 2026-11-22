package io.franshub.fastmock.client;

import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.UUID;

/**
 * A message capture source: the Kafka topics, MQTT topic filters or AMQP exchange FastMock subscribes to.
 * Example: {@code CaptureSource.mqtt("devices", "devices/+/state").qos(1)}.
 */
public final class CaptureSource {

    private final ObjectNode json = FastMock.JSON.createObjectNode();
    private UUID id;

    private CaptureSource(String name, String protocol) {
        json.put("name", name);
        json.put("protocol", protocol);
    }

    public static CaptureSource kafka(String name, String... topics) {
        return new CaptureSource(name, "kafka").topics(topics);
    }

    public static CaptureSource mqtt(String name, String... topicFilters) {
        return new CaptureSource(name, "mqtt").topics(topicFilters);
    }

    public static CaptureSource amqp(String name, String exchange, String... bindingKeys) {
        CaptureSource s = new CaptureSource(name, "amqp");
        s.json.put("exchange", exchange);
        if (bindingKeys.length > 0) {
            ArrayNode a = s.json.putArray("bindingKeys");
            for (String k : bindingKeys) {
                a.add(k);
            }
        }
        return s;
    }

    private CaptureSource topics(String[] topics) {
        ArrayNode a = json.putArray("topics");
        for (String t : topics) {
            a.add(t);
        }
        return this;
    }

    /** Fixes the source id; by default a random one is generated when the source is created. */
    public CaptureSource id(UUID id) {
        this.id = id;
        return this;
    }

    /** MQTT subscription QoS (0, 1 or 2). */
    public CaptureSource qos(int qos) {
        json.put("qos", qos);
        return this;
    }

    /** Kafka: read the whole topic first ({@code "earliest"}) instead of only new messages ({@code "latest"}). */
    public CaptureSource startFrom(String startFrom) {
        json.put("startFrom", startFrom);
        return this;
    }

    public CaptureSource maxMessages(int n) {
        json.put("maxMessages", n);
        return this;
    }

    public CaptureSource enabled(boolean enabled) {
        json.put("enabled", enabled);
        return this;
    }

    public CaptureSource description(String text) {
        json.put("description", text);
        return this;
    }

    UUID idOrNew() {
        if (id == null) {
            id = UUID.randomUUID();
        }
        return id;
    }

    public ObjectNode toJson() {
        return json.deepCopy();
    }
}
