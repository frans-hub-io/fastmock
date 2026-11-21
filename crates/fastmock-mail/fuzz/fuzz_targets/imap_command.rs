#![no_main]

use fastmock_mail::ParseLimits;
use fastmock_mail::imap::codec;
use fastmock_mail::imap::command::{Section, SectionText};
use fastmock_mail::imap::mime::MessageView;
use fastmock_mail::imap::search::SearchDoc;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = codec::parse(data);
    let limits = ParseLimits {
        max_bytes: 64 * 1024,
        max_depth: 8,
        max_parts: 64,
    };
    let view = MessageView::parse(data, &limits);
    let mut out = Vec::new();
    view.envelope(&mut out, data.first().is_some_and(|b| b & 1 == 1));
    view.body_structure(&mut out, true, false);
    view.body_structure(&mut out, false, true);
    for part in [vec![], vec![1], vec![1, 1], vec![2, 1, 1]] {
        for text in [
            SectionText::Full,
            SectionText::Header,
            SectionText::HeaderFields(vec!["SUBJECT".into()]),
            SectionText::HeaderFieldsNot(vec!["FROM".into()]),
            SectionText::Text,
            SectionText::Mime,
        ] {
            let _ = view.section(&Section {
                part: part.clone(),
                text,
            });
        }
    }
    let _ = SearchDoc::build(data, &limits);
});
