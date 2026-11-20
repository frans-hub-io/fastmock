//! Labels: set through the admin API, from plus addresses (`user+tag@`) and from the
//! `X-FastMock-Labels` header.

use crate::address::plus_tag;

pub const MAX_LABELS: usize = 32;
pub const MAX_LABEL_LEN: usize = 64;
/// The header whose comma-separated values become labels.
pub const LABELS_HEADER: &str = "X-FastMock-Labels";

/// A label in canonical form: trimmed, lowercase, 1 to 64 characters out of letters, digits and
/// `- _ . : /`.
pub fn normalize_label(raw: &str) -> Option<String> {
    let l = raw.trim().to_lowercase();
    let ok = !l.is_empty()
        && l.chars().count() <= MAX_LABEL_LEN
        && l.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'));
    ok.then_some(l)
}

/// Labels given explicitly (admin API): every one must be valid; sorted, without duplicates.
pub fn normalize_labels<S: AsRef<str>>(raw: &[S]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let r = r.as_ref();
        out.push(normalize_label(r).ok_or_else(|| {
            format!("invalid label `{r}`: use 1 to {MAX_LABEL_LEN} letters, digits or - _ . : /")
        })?);
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_LABELS {
        return Err(format!("at most {MAX_LABELS} labels"));
    }
    Ok(out)
}

/// Labels a message gets on arrival: the tags of plus-addressed recipients, the values of
/// `X-FastMock-Labels` headers and `extra`. Invalid ones are skipped; sorted, without duplicates, at
/// most [`MAX_LABELS`].
pub fn auto_labels<R: AsRef<str>, H: AsRef<str>>(
    rcpt_to: &[R],
    header_values: &[H],
    extra: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = rcpt_to
        .iter()
        .filter_map(|r| plus_tag(r.as_ref()))
        .chain(header_values.iter().flat_map(|h| h.as_ref().split(',')))
        .chain(extra.iter().map(String::as_str))
        .filter_map(normalize_label)
        .collect();
    out.sort();
    out.dedup();
    out.truncate(MAX_LABELS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_from_addresses_and_headers() {
        assert_eq!(normalize_label(" Welcome "), Some("welcome".into()));
        assert_eq!(normalize_label("team/otp:v2"), Some("team/otp:v2".into()));
        assert_eq!(normalize_label("two words"), None);
        assert_eq!(normalize_label(""), None);
        assert_eq!(normalize_label(&"x".repeat(65)), None);
        assert_eq!(
            auto_labels(
                &[
                    "alice+Signup@example.test",
                    "bob@example.test",
                    "c+bad tag@example.test"
                ],
                &["otp, welcome", " ,signup"],
                &["inject".to_string()],
            ),
            vec!["inject", "otp", "signup", "welcome"]
        );
        let many: Vec<String> = (0..40).map(|i| format!("l{i:02}")).collect();
        assert_eq!(
            auto_labels::<&str, String>(&[], &many, &[]).len(),
            MAX_LABELS
        );
        assert_eq!(
            normalize_labels(&["B", "a", "b"]).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(
            normalize_labels(&["ok", "not ok"])
                .unwrap_err()
                .contains("not ok")
        );
        assert!(normalize_labels(&many).is_err());
    }
}
