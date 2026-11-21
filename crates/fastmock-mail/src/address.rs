//! Envelope addresses: domains for routing, mailbox names for IMAP/POP3 and plus-address tags.

/// The address without surrounding spaces or angle brackets.
pub fn bare(addr: &str) -> &str {
    let a = addr.trim();
    a.strip_prefix('<')
        .and_then(|x| x.strip_suffix('>'))
        .unwrap_or(a)
        .trim()
}

/// Local part and domain, split at the last `@`; `None` without one or with an empty side.
pub fn split(addr: &str) -> Option<(&str, &str)> {
    let (local, domain) = bare(addr).rsplit_once('@')?;
    (!local.is_empty() && !domain.is_empty()).then_some((local, domain))
}

/// The lowercase domain without a trailing dot.
pub fn domain_of(addr: &str) -> Option<String> {
    split(addr).map(|(_, d)| d.trim_end_matches('.').to_lowercase())
}

/// The tag of a plus address (`user+tag@example.com` gives `tag`); quoted local parts have none.
pub fn plus_tag(addr: &str) -> Option<&str> {
    let (local, _) = split(addr)?;
    if local.starts_with('"') {
        return None;
    }
    local
        .split_once('+')
        .map(|(_, tag)| tag)
        .filter(|t| !t.is_empty())
}

/// The mailbox an address delivers to: lowercase, without a plus tag (`User+news@Example.com` and
/// `user@example.com` share `user@example.com`). Addresses without a domain are only lowercased.
pub fn mailbox_of(addr: &str) -> String {
    match split(addr) {
        Some((local, domain)) => {
            let base = if local.starts_with('"') {
                local
            } else {
                local.split_once('+').map_or(local, |(b, _)| b)
            };
            let base = if base.is_empty() { local } else { base };
            format!(
                "{}@{}",
                base.to_lowercase(),
                domain.trim_end_matches('.').to_lowercase()
            )
        }
        None => bare(addr).to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(bare(" <a@example.com> "), "a@example.com");
        assert_eq!(split("a@b@example.com"), Some(("a@b", "example.com")));
        assert_eq!(split("@example.com"), None);
        assert_eq!(split("nobody"), None);
        assert_eq!(
            domain_of("x@Billing.Example.TEST.").as_deref(),
            Some("billing.example.test")
        );
        assert_eq!(plus_tag("user+news@example.com"), Some("news"));
        assert_eq!(plus_tag("user+a+b@example.com"), Some("a+b"));
        assert_eq!(plus_tag("user+@example.com"), None);
        assert_eq!(plus_tag("\"a+b\"@example.com"), None);
        assert_eq!(mailbox_of("User+News@Example.COM"), "user@example.com");
        assert_eq!(mailbox_of("+tag@example.com"), "+tag@example.com");
        assert_eq!(mailbox_of("\"A+b\"@example.com"), "\"a+b\"@example.com");
        assert_eq!(mailbox_of(" Postmaster "), "postmaster");
    }
}
