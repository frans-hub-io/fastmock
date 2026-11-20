//! Data-plane path prefixes and hosts owned by spaces: a request under one of them is answered only by
//! stubs of the owning space or by global stubs.

use crate::request::split_host_port;

/// True when `path` is `prefix` or lies below it; `/pay/` and `/pay` both own `/pay` and `/pay/x`.
pub fn under(path: &str, prefix: &str) -> bool {
    let p = prefix.trim_end_matches('/');
    p.is_empty() || path == p || path.starts_with(&format!("{p}/"))
}

/// Whether a path has a `.` or `..` segment, plain or percent-encoded in any case (`%2e`, `.%2E`).
/// Such a path names a different location than its text says, so ownership checks refuse it.
pub fn has_dot_segment(path: &str) -> bool {
    path.split('/').any(|seg| {
        let s = seg.to_ascii_lowercase().replace("%2e", ".");
        s == "." || s == ".."
    })
}

/// Two prefixes overlap when a path can lie under both.
pub fn prefixes_overlap(a: &str, b: &str) -> bool {
    under(a.trim_end_matches('/'), b) || under(b.trim_end_matches('/'), a)
}

/// Two hosts overlap when a request can match both: same name, and the same port or no port on one side.
pub fn hosts_overlap(a: &str, b: &str) -> bool {
    let (ha, pa) = split_host_port(a);
    let (hb, pb) = split_host_port(b);
    ha == hb && (pa.is_none() || pb.is_none() || pa == pb)
}

#[derive(Clone, Debug, Default)]
pub struct Ownership {
    prefixes: Vec<(String, String)>,
    hosts: Vec<(String, Option<u16>, String)>,
}

impl Ownership {
    pub fn add_prefix(&mut self, space: &str, prefix: &str) {
        self.prefixes.push((prefix.to_string(), space.to_string()));
    }

    pub fn add_host(&mut self, space: &str, host: &str) {
        let (h, p) = split_host_port(host);
        self.hosts
            .push((h.trim_end_matches('.').to_string(), p, space.to_string()));
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty() && self.hosts.is_empty()
    }

    /// The spaces owning a request: the owner of its path and the owner of its host, when any.
    pub fn owners(&self, path: &str, host: Option<&str>, port: Option<u16>) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        if let Some((_, s)) = self.prefixes.iter().find(|(p, _)| under(path, p)) {
            out.push(s);
        }
        if let Some(h) = host {
            let h = h.trim_end_matches('.').to_ascii_lowercase();
            if let Some((_, _, s)) = self
                .hosts
                .iter()
                .find(|(oh, op, _)| *oh == h && op.is_none_or(|p| Some(p) == port))
            {
                out.push(s);
            }
        }
        out
    }

    /// Whether a stub of `space` may answer a request owned by `owners`.
    pub fn admits(owners: &[&str], space: Option<&str>) -> bool {
        match space {
            None => true,
            Some(s) => owners.iter().all(|o| *o == s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlaps_and_owners() {
        assert!(prefixes_overlap("/pay/", "/pay/x"));
        assert!(prefixes_overlap("/pay", "/pay/"));
        assert!(prefixes_overlap("/", "/d/"));
        assert!(!prefixes_overlap("/pay/", "/payments/"));
        assert!(hosts_overlap("psp.example.com", "psp.example.com:8443"));
        assert!(!hosts_overlap("psp.example.com:80", "psp.example.com:8443"));
        assert!(!hosts_overlap("a.example.com", "b.example.com"));
        let mut o = Ownership::default();
        o.add_prefix("pay", "/pay/");
        o.add_host("bill", "bill.example.com");
        o.add_host("api", "api.example.com:8443");
        assert_eq!(o.owners("/pay/x", None, None), ["pay"]);
        assert!(o.owners("/payx", None, None).is_empty());
        assert_eq!(
            o.owners("/x", Some("Bill.example.com."), Some(80)),
            ["bill"]
        );
        assert_eq!(
            o.owners("/pay", Some("bill.example.com"), None),
            ["pay", "bill"]
        );
        assert!(o.owners("/x", Some("api.example.com"), Some(80)).is_empty());
        assert_eq!(o.owners("/x", Some("api.example.com"), Some(8443)), ["api"]);
        assert!(Ownership::admits(&["pay"], None));
        assert!(Ownership::admits(&["pay"], Some("pay")));
        assert!(!Ownership::admits(&["pay"], Some("free")));
        assert!(!Ownership::admits(&["pay", "bill"], Some("pay")));
        assert!(Ownership::admits(&[], Some("free")));
    }
}
