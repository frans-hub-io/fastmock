use apache_avro::Schema;
use fastmock_kafka::{decode_avro, limit_decode_allocation};

fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

#[test]
fn tiny_messages_that_declare_huge_values_are_refused() {
    assert_eq!(limit_decode_allocation(1024 * 1024), 1024 * 1024);
    assert_eq!(
        limit_decode_allocation(16),
        1024 * 1024,
        "the first call wins"
    );

    for (schema, what) in [
        (r#"{"type":"string"}"#, "string"),
        (r#"{"type":"bytes"}"#, "bytes"),
        (r#"{"type":"array","items":"long"}"#, "array"),
        (r#"{"type":"map","values":"long"}"#, "map"),
    ] {
        let schema = Schema::parse_str(schema).unwrap();
        let mut body = vec![0, 0, 0, 0, 1];
        body.extend(varint(zigzag(100_000_000)));
        body.push(0);
        assert!(body.len() < 20);
        let e = format!("{:#}", decode_avro(&schema, &body).unwrap_err());
        assert!(e.contains("maximum allowed: 1048576"), "{what}: {e}");
    }

    let schema = Schema::parse_str(r#"{"type":"string"}"#).unwrap();
    let mut ok = vec![0, 0, 0, 0, 1];
    ok.extend(varint(zigzag(5)));
    ok.extend(b"hello");
    assert_eq!(
        decode_avro(&schema, &ok).unwrap(),
        serde_json::json!("hello")
    );
}
