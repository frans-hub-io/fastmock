//! Mail domains owned by spaces (`mailDomains` in the access file) and the routing of a message to
//! spaces.

use std::collections::BTreeMap;

use crate::address::domain_of;

/// A domain of `mailDomains` in canonical form: lowercase, without a trailing dot, either a host name
/// (`billing.example.test`) or a wildcard for its subdomains (`*.billing.example.test`, which does not
/// cover `billing.example.test` itself).
pub fn check_domain(raw: &str) -> Result<String, String> {
    let d = raw.trim().trim_end_matches('.').to_lowercase();
    let host = d.strip_prefix("*.").unwrap_or(&d);
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_alphanumeric() || c == '-')
    };
    if host.is_empty() || host.len() > 253 || !host.split('.').all(label_ok) {
        return Err(format!(
            "mail domain `{raw}` must be a domain name such as billing.example.test, or *.billing.example.test for its subdomains"
        ));
    }
    Ok(d)
}

fn covers(pattern: &str, domain: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(base) => domain
            .strip_suffix(base)
            .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
        None => pattern == domain,
    }
}

/// Whether some domain is covered by both (canonical) patterns.
pub fn domains_overlap(a: &str, b: &str) -> bool {
    match (a.strip_prefix("*."), b.strip_prefix("*.")) {
        (None, None) => a == b,
        (Some(_), None) => covers(a, b),
        (None, Some(_)) => covers(b, a),
        (Some(x), Some(y)) => x == y || covers(a, y) || covers(b, x),
    }
}

/// Which space owns which mail domains. Overlaps between spaces are refused when the access file is
/// read, so a domain has at most one owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailDomains {
    entries: Vec<(String, String)>,
}

impl MailDomains {
    /// Adds a canonical domain (see [`check_domain`]) owned by `space`.
    pub fn add(&mut self, space: &str, domain: &str) {
        self.entries.push((domain.to_string(), space.to_string()));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The space owning a domain (any case, trailing dot allowed).
    pub fn owner(&self, domain: &str) -> Option<&str> {
        let d = domain.trim_end_matches('.').to_lowercase();
        self.entries
            .iter()
            .find(|(p, _)| covers(p, &d))
            .map(|(_, s)| s.as_str())
    }

    /// The first pair of overlapping domains of different spaces: `(space, domain, space, domain)`.
    pub fn first_overlap(&self) -> Option<(&str, &str, &str, &str)> {
        for (i, (da, sa)) in self.entries.iter().enumerate() {
            for (db, sb) in &self.entries[i + 1..] {
                if sa != sb && domains_overlap(da, db) {
                    return Some((sa, da, sb, db));
                }
            }
        }
        None
    }
}

/// One stored copy of a message: its area and the recipients it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub space: Option<String>,
    pub rcpt_to: Vec<String>,
}

/// Routes a message (M3). Recipients whose domain a space owns go to that space; a message whose
/// recipients belong to several spaces is stored once per space, each copy listing only its own
/// recipients. Recipients of no space go, together, to the space named like the AUTH user when such a
/// space exists (`is_space`), else to the global area; when that space also owns some recipients, the
/// two groups make one copy. Copies are ordered by space name, the global area first; recipients keep
/// their order. A message without recipients goes to the AUTH user's space or the global area.
pub fn route(
    rcpt_to: &[String],
    auth_user: Option<&str>,
    domains: &MailDomains,
    is_space: &dyn Fn(&str) -> bool,
) -> Vec<Route> {
    let fallback = auth_user.filter(|u| is_space(u)).map(String::from);
    let mut groups: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    for r in rcpt_to {
        let owner = domain_of(r)
            .and_then(|d| domains.owner(&d).map(String::from))
            .or_else(|| fallback.clone());
        groups.entry(owner).or_default().push(r.clone());
    }
    if groups.is_empty() {
        groups.insert(fallback, vec![]);
    }
    groups
        .into_iter()
        .map(|(space, rcpt_to)| Route { space, rcpt_to })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domains() -> MailDomains {
        let mut d = MailDomains::default();
        d.add("billing", "billing.example.test");
        d.add("billing", "*.invoices.example.test");
        d.add("crm", "crm.example.test");
        d
    }

    fn r(space: Option<&str>, rcpt: &[&str]) -> Route {
        Route {
            space: space.map(String::from),
            rcpt_to: rcpt.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn rcpts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn domain_checks_and_overlaps() {
        assert_eq!(
            check_domain(" Billing.Example.TEST. ").unwrap(),
            "billing.example.test"
        );
        assert_eq!(check_domain("*.example.test").unwrap(), "*.example.test");
        assert_eq!(check_domain("bücher.example").unwrap(), "bücher.example");
        for bad in [
            "",
            "*.",
            "a..b",
            "-a.example",
            "a_b.example",
            "*.*.example",
            "a@b",
            "a b",
        ] {
            assert!(check_domain(bad).is_err(), "{bad}");
        }
        assert!(domains_overlap("a.example", "a.example"));
        assert!(!domains_overlap("a.example", "b.example"));
        assert!(domains_overlap("*.example", "a.example"));
        assert!(domains_overlap("x.a.example", "*.example"));
        assert!(!domains_overlap("*.example", "example"));
        assert!(!domains_overlap("*.a.example", "a.example"));
        assert!(domains_overlap("*.example", "*.a.example"));
        assert!(domains_overlap("*.example", "*.example"));
        assert!(!domains_overlap("*.a.example", "*.b.example"));
        assert!(!domains_overlap("*.ample", "ex.example"));
        let d = domains();
        assert_eq!(d.owner("BILLING.example.test."), Some("billing"));
        assert_eq!(d.owner("eu.invoices.example.test"), Some("billing"));
        assert_eq!(d.owner("invoices.example.test"), None);
        assert_eq!(d.first_overlap(), None);
        let mut o = d.clone();
        o.add("crm", "x.invoices.example.test");
        assert_eq!(
            o.first_overlap(),
            Some((
                "billing",
                "*.invoices.example.test",
                "crm",
                "x.invoices.example.test"
            ))
        );
    }

    #[test]
    fn routing_to_spaces() {
        let d = domains();
        let spaces = |s: &str| ["billing", "crm", "ops"].contains(&s);
        let route = |rcpt: &[&str], auth: Option<&str>| route(&rcpts(rcpt), auth, &d, &spaces);
        assert_eq!(
            route(&["a@billing.example.test", "B@Billing.Example.Test"], None),
            vec![r(
                Some("billing"),
                &["a@billing.example.test", "B@Billing.Example.Test"]
            )]
        );
        assert_eq!(
            route(
                &["a@crm.example.test", "b@eu.invoices.example.test"],
                Some("ops")
            ),
            vec![
                r(Some("billing"), &["b@eu.invoices.example.test"]),
                r(Some("crm"), &["a@crm.example.test"]),
            ]
        );
        assert_eq!(
            route(&["a@crm.example.test", "x@elsewhere.example"], None),
            vec![
                r(None, &["x@elsewhere.example"]),
                r(Some("crm"), &["a@crm.example.test"]),
            ]
        );
        assert_eq!(
            route(&["x@elsewhere.example", "y@other.example"], Some("ops")),
            vec![r(Some("ops"), &["x@elsewhere.example", "y@other.example"])]
        );
        assert_eq!(
            route(&["a@crm.example.test", "x@elsewhere.example"], Some("crm")),
            vec![r(
                Some("crm"),
                &["a@crm.example.test", "x@elsewhere.example"]
            )]
        );
        assert_eq!(
            route(&["x@elsewhere.example"], Some("unknown")),
            vec![r(None, &["x@elsewhere.example"])]
        );
        assert_eq!(route(&["postmaster"], None), vec![r(None, &["postmaster"])]);
        assert_eq!(route(&[], Some("ops")), vec![r(Some("ops"), &[])]);
        assert_eq!(route(&[], None), vec![r(None, &[])]);
    }
}
