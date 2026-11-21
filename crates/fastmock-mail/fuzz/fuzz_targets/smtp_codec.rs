#![no_main]

use fastmock_mail::smtp::codec::{ProtoCodec, SmtpCodec};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let codec = ProtoCodec;
    for line in data.split_inclusive(|b| *b == b'\n') {
        let _ = codec.command(line);
    }
    let mut dec = codec.body_decoder();
    let mut out = Vec::new();
    let mut rest = data;
    let mut step = 1usize;
    while !rest.is_empty() {
        let take = step.min(rest.len());
        let (n, done) = dec.feed(&rest[..take], &mut out);
        assert!(n <= take);
        if done {
            break;
        }
        assert_eq!(n, take);
        rest = &rest[take..];
        step = step % 7 + 1;
    }
    assert!(out.len() <= data.len());
});
