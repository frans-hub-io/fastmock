const BLOCKED: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "http2-settings",
    "forwarded",
    "via",
    "x-real-ip",
    "x-original-url",
    "x-original-host",
    "x-rewrite-url",
    "x-host",
    "x-http-host-override",
    "x-fastmock-body-base64",
    "x-fastmock-sealed-authorization",
];

const BLOCKED_PREFIXES: &[&str] = &["proxy-", "x-forwarded-", "x-envoy-", ":"];

/// Headers that FastMock never sets on outbound requests (callbacks, proxied requests, contract
/// tests) from user input: they select the virtual host or upstream, change message framing, or are
/// hop-by-hop.
pub fn blocked_outbound_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    BLOCKED.contains(&n.as_str()) || BLOCKED_PREFIXES.iter().any(|p| n.starts_with(p))
}

pub fn blocked_reason(name: &str) -> String {
    format!(
        "header {name} cannot be set on outbound requests: it selects the target host or changes message framing"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_and_framing_headers_are_blocked() {
        for h in [
            "Host",
            " host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "TE",
            "Trailer",
            "Upgrade",
            "Proxy-Authorization",
            "proxy-connection",
            "X-Forwarded-Host",
            "x-forwarded-for",
            "Forwarded",
            "X-Original-URL",
            "x-envoy-upstream-alt-stat-name",
            "X-FastMock-Body-Base64",
            "x-fastmock-sealed-authorization",
        ] {
            assert!(blocked_outbound_header(h), "{h}");
        }
        for h in [
            "authorization",
            "content-type",
            "x-request-id",
            "x-hostname",
            "accept",
        ] {
            assert!(!blocked_outbound_header(h), "{h}");
        }
    }
}
