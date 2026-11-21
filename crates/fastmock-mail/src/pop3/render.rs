//! Multi-line POP3 responses (RFC 1939 3): every line ends with CRLF (bare LF and bare CR in stored
//! messages become CRLF), lines starting with `.` get another `.`, and `.` CRLF ends the response.

/// The lines of a message without their line breaks; `\r\n`, `\n` and a lone `\r` each end a line. A
/// final line break does not start another line.
pub fn lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = raw;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
            None => {
                let line = rest;
                rest = &[];
                Some(line)
            }
            Some(i) => {
                let line = &rest[..i];
                let skip = if rest[i] == b'\r' && rest.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                rest = &rest[i + skip..];
                Some(line)
            }
        }
    })
}

fn push_line(out: &mut Vec<u8>, line: &[u8]) {
    if line.first() == Some(&b'.') {
        out.push(b'.');
    }
    out.extend_from_slice(line);
    out.extend_from_slice(b"\r\n");
}

/// Appends the message as the body of a multi-line response, with the terminating `.` line. With
/// `top`, only the header, the empty line after it and that many body lines (RFC 1939 TOP).
pub fn message(raw: &[u8], top: Option<u32>, out: &mut Vec<u8>) {
    out.reserve(raw.len() + raw.len() / 32 + 8);
    let mut header = true;
    let mut body_lines: u32 = 0;
    for line in lines(raw) {
        if header {
            push_line(out, line);
            if line.is_empty() {
                header = false;
            }
            continue;
        }
        if let Some(max) = top {
            if body_lines >= max {
                break;
            }
            body_lines += 1;
        }
        push_line(out, line);
    }
    out.extend_from_slice(b".\r\n");
}

/// The size in octets of the message sent with CRLF line breaks (without dot-stuffing).
pub fn crlf_size(raw: &[u8]) -> u64 {
    lines(raw).map(|l| l.len() as u64 + 2).sum()
}

/// Reverses [`message`]: the lines up to the `.` line, unstuffed, joined with CRLF. `None` without the
/// final `.` line. For tests and fuzzing.
pub fn unstuff(resp: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(resp.len());
    let mut rest = resp;
    loop {
        let i = rest.windows(2).position(|w| w == b"\r\n")?;
        let line = &rest[..i];
        rest = &rest[i + 2..];
        if line == b"." {
            return rest.is_empty().then_some(out);
        }
        out.extend_from_slice(line.strip_prefix(b".").unwrap_or(line));
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(raw: &[u8], top: Option<u32>) -> Vec<u8> {
        let mut out = vec![];
        message(raw, top, &mut out);
        out
    }

    #[test]
    fn splits_lines_on_any_break() {
        let v: Vec<&[u8]> = lines(b"a\r\nb\nc\rd\r\n\r\ne").collect();
        assert_eq!(v, vec![&b"a"[..], b"b", b"c", b"d", b"", b"e"]);
        assert_eq!(lines(b"a\r\n").count(), 1);
        assert_eq!(lines(b"").count(), 0);
        assert_eq!(lines(b"\r\n\r\n").count(), 2);
    }

    #[test]
    fn dot_stuffing_and_crlf() {
        assert_eq!(
            render(b"Subject: x\n\n.\n..two\nend", None),
            b"Subject: x\r\n\r\n..\r\n...two\r\nend\r\n.\r\n".to_vec()
        );
        assert_eq!(render(b"", None), b".\r\n".to_vec());
        assert_eq!(render(b".hdr\r\n", None), b"..hdr\r\n.\r\n".to_vec());
        assert_eq!(crlf_size(b"a\nb\r\nc"), 9);
        assert_eq!(crlf_size(b"a\r\n"), 3);
    }

    #[test]
    fn round_trip() {
        let cases: [&[u8]; 6] = [
            b"Subject: a\r\n\r\nbody\r\n.\r\n..\r\n. x\r\n",
            b"Subject: a\r\n\r\n.\r\n",
            b"no header break",
            b"A: b\r\n\r\nlast line without break",
            b"\r\n.\r\n",
            b"",
        ];
        for raw in cases {
            let back = unstuff(&render(raw, None)).unwrap();
            let mut want = vec![];
            for l in lines(raw) {
                want.extend_from_slice(l);
                want.extend_from_slice(b"\r\n");
            }
            assert_eq!(back, want, "{:?}", String::from_utf8_lossy(raw));
            assert_eq!(back.len() as u64, crlf_size(raw));
        }
        assert_eq!(unstuff(b"a\r\n"), None);
        assert_eq!(unstuff(b".\r\nextra"), None);
    }

    #[test]
    fn random_messages_round_trip() {
        const ALPHABET: &[u8] = b"a.\r\n :";
        for _ in 0..2000 {
            let len = rand::random_range(0..40);
            let raw: Vec<u8> = (0..len)
                .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())])
                .collect();
            let full = render(&raw, None);
            let back = unstuff(&full).unwrap();
            assert_eq!(back.len() as u64, crlf_size(&raw), "{raw:?}");
            assert!(!back.windows(3).any(|w| w == b"\r\r\n"), "{raw:?}");
            let top = unstuff(&render(&raw, Some(rand::random_range(0..3)))).unwrap();
            assert!(back.starts_with(&top), "{raw:?}");
        }
    }

    #[test]
    fn top_lines() {
        let raw = b"From: a@example.test\r\nSubject: t\r\n\r\none\r\n.two\r\nthree\r\n";
        assert_eq!(
            render(raw, Some(0)),
            b"From: a@example.test\r\nSubject: t\r\n\r\n.\r\n".to_vec()
        );
        assert_eq!(
            render(raw, Some(2)),
            b"From: a@example.test\r\nSubject: t\r\n\r\none\r\n..two\r\n.\r\n".to_vec()
        );
        assert_eq!(render(raw, Some(99)), render(raw, None));
        assert_eq!(
            render(b"Subject: only\r\n", Some(0)),
            b"Subject: only\r\n.\r\n".to_vec()
        );
    }
}
