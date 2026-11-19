package io.fastmock.client;

/** Raised when the FastMock admin API answers with an error or cannot be reached. */
public final class FastMockException extends RuntimeException {

    private static final long serialVersionUID = 1L;

    private final int status;
    private final transient String body;

    FastMockException(int status, String body) {
        super("FastMock admin API answered " + status + ": " + body);
        this.status = status;
        this.body = body;
    }

    FastMockException(String message, Throwable cause) {
        super(message, cause);
        this.status = -1;
        this.body = null;
    }

    public int status() {
        return status;
    }

    public String body() {
        return body;
    }
}
