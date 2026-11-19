use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use apache_avro::Schema;
use apache_avro::types::Value as AvroValue;
use rskafka::client::partition::{Compression, OffsetAt, PartitionClient, UnknownTopicHandling};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;
use serde_json::Value;
use tokio::sync::Mutex;

mod security;

pub use security::{
    BasicAuth, ClientCertFiles, KafkaSecurity, RegistryOptions, SaslMechanism, SaslSettings,
    SecurityProtocol, TlsFiles, client_tls_config, read_ca_bundle, read_client_cert,
};

pub fn murmur2(data: &[u8]) -> i32 {
    let seed: u32 = 0x9747_b28c;
    let m: u32 = 0x5bd1_e995;
    let r = 24;
    let len = data.len();
    let mut h: u32 = seed ^ (len as u32);
    let chunks = len / 4;
    for i in 0..chunks {
        let o = i * 4;
        let mut k = u32::from(data[o])
            | (u32::from(data[o + 1]) << 8)
            | (u32::from(data[o + 2]) << 16)
            | (u32::from(data[o + 3]) << 24);
        k = k.wrapping_mul(m);
        k ^= k >> r;
        k = k.wrapping_mul(m);
        h = h.wrapping_mul(m);
        h ^= k;
    }
    let tail = chunks * 4;
    let rem = len % 4;
    if rem == 3 {
        h ^= u32::from(data[tail + 2]) << 16;
    }
    if rem >= 2 {
        h ^= u32::from(data[tail + 1]) << 8;
    }
    if rem >= 1 {
        h ^= u32::from(data[tail]);
        h = h.wrapping_mul(m);
    }
    h ^= h >> 13;
    h = h.wrapping_mul(m);
    h ^= h >> 15;
    h as i32
}

pub fn partition_for(key: Option<&[u8]>, partitions: i32) -> i32 {
    if partitions <= 1 {
        return 0;
    }
    match key {
        Some(k) => (murmur2(k) & 0x7fff_ffff) % partitions,
        None => rand::random_range(0..partitions),
    }
}

pub fn confluent_frame(schema_id: i32, avro: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(avro.len() + 5);
    v.push(0u8);
    v.extend_from_slice(&schema_id.to_be_bytes());
    v.extend_from_slice(avro);
    v
}

pub fn with_java_strings(schema: &Value) -> Value {
    match schema {
        Value::String(s) if s == "string" => {
            serde_json::json!({"type": "string", "avro.java.string": "String"})
        }
        Value::Array(a) => Value::Array(a.iter().map(with_java_strings).collect()),
        Value::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, v) in o {
                if k == "type" || k == "items" || k == "values" || k == "fields" {
                    m.insert(k.clone(), with_java_strings(v));
                } else {
                    m.insert(k.clone(), v.clone());
                }
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

type Names<'a> = std::collections::HashMap<apache_avro::schema::Name, &'a Schema>;

pub fn json_to_avro(schema: &Schema, v: &Value) -> anyhow::Result<AvroValue> {
    let resolved = apache_avro::schema::ResolvedSchema::try_from(schema)?;
    json_to_avro_in(schema, v, &mut Vec::new(), resolved.get_names())
}

fn unscaled(v: &Value, scale: usize) -> anyhow::Result<i128> {
    let text = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        other => anyhow::bail!("{other} is not a decimal"),
    };
    let (neg, digits) = match text.strip_prefix('-') {
        Some(d) => (true, d.to_string()),
        None => (false, text.trim_start_matches('+').to_string()),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((&digits, ""));
    if frac.len() > scale || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        anyhow::bail!("`{text}` does not fit decimal scale {scale}");
    }
    let s = format!("{int}{frac}{}", "0".repeat(scale - frac.len()));
    let n: i128 = s.trim_start_matches('0').parse().unwrap_or(0);
    Ok(if neg { -n } else { n })
}

fn twos_complement(n: i128, size: Option<usize>) -> anyhow::Result<Vec<u8>> {
    let full = n.to_be_bytes();
    let mut start = 0;
    while start < 15
        && ((full[start] == 0 && full[start + 1] & 0x80 == 0)
            || (full[start] == 0xff && full[start + 1] & 0x80 != 0))
    {
        start += 1;
    }
    let minimal = full[start..].to_vec();
    match size {
        None => Ok(minimal),
        Some(sz) if sz >= minimal.len() => {
            let pad = if n < 0 { 0xff } else { 0 };
            let mut out = vec![pad; sz - minimal.len()];
            out.extend(minimal);
            Ok(out)
        }
        Some(sz) => anyhow::bail!(
            "decimal needs {} bytes but the fixed size is {sz}",
            minimal.len()
        ),
    }
}

fn epoch(v: &Value, unit: i64) -> anyhow::Result<i64> {
    match v {
        Value::Number(n) => n.as_i64().context("not an integer timestamp"),
        Value::String(s) => {
            let t = chrono::DateTime::parse_from_rfc3339(s)
                .with_context(|| format!("`{s}` is not an RFC 3339 timestamp"))?;
            Ok(match unit {
                1_000 => t.timestamp_millis(),
                1_000_000 => t.timestamp_micros(),
                _ => t.timestamp_nanos_opt().context("timestamp out of range")?,
            })
        }
        other => anyhow::bail!("{other} is not a timestamp"),
    }
}

fn json_to_avro_in(
    schema: &Schema,
    v: &Value,
    path: &mut Vec<String>,
    names: &Names,
) -> anyhow::Result<AvroValue> {
    fn here(path: &[String]) -> String {
        if path.is_empty() {
            "$".to_string()
        } else {
            format!("$.{}", path.join("."))
        }
    }
    Ok(match (schema, v) {
        (Schema::Null, Value::Null) => AvroValue::Null,
        (Schema::Boolean, Value::Bool(b)) => AvroValue::Boolean(*b),
        (Schema::Int, Value::Number(n)) => AvroValue::Int(
            n.as_i64()
                .and_then(|x| i32::try_from(x).ok())
                .with_context(|| format!("{}: not an int", here(path)))?,
        ),
        (Schema::Long, Value::Number(n)) => AvroValue::Long(
            n.as_i64()
                .with_context(|| format!("{}: not a long", here(path)))?,
        ),
        (Schema::Float, Value::Number(n)) => {
            AvroValue::Float(n.as_f64().unwrap_or_default() as f32)
        }
        (Schema::Double, Value::Number(n)) => AvroValue::Double(n.as_f64().unwrap_or_default()),
        (Schema::String, Value::String(s)) => AvroValue::String(s.clone()),
        (Schema::Bytes, Value::String(s)) => AvroValue::Bytes(s.clone().into_bytes()),
        (Schema::Uuid(_), Value::String(s)) => AvroValue::String(s.clone()),
        (Schema::Enum(e), Value::String(s)) => {
            let i = e
                .symbols
                .iter()
                .position(|x| x == s)
                .with_context(|| format!("{}: `{s}` is not an enum symbol", here(path)))?;
            AvroValue::Enum(i as u32, s.clone())
        }
        (Schema::Array(a), Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, x) in items.iter().enumerate() {
                path.push(i.to_string());
                out.push(json_to_avro_in(&a.items, x, path, names)?);
                path.pop();
            }
            AvroValue::Array(out)
        }
        (Schema::Map(m), Value::Object(o)) => {
            let mut out = std::collections::HashMap::new();
            for (k, x) in o {
                path.push(k.clone());
                out.insert(k.clone(), json_to_avro_in(&m.types, x, path, names)?);
                path.pop();
            }
            AvroValue::Map(out)
        }
        (Schema::Record(r), Value::Object(o)) => {
            let mut fields = Vec::with_capacity(r.fields.len());
            for f in &r.fields {
                path.push(f.name.clone());
                let val = match o.get(&f.name) {
                    Some(x) => json_to_avro_in(&f.schema, x, path, names)?,
                    None => match &f.default {
                        Some(d) => json_to_avro_in(&f.schema, d, path, names)?,
                        None => anyhow::bail!("{}: required field is missing", here(path)),
                    },
                };
                path.pop();
                fields.push((f.name.clone(), val));
            }
            AvroValue::Record(fields)
        }
        (Schema::Union(u), x) => {
            for (i, variant) in u.variants().iter().enumerate() {
                if let Ok(val) = json_to_avro_in(variant, x, path, names) {
                    return Ok(AvroValue::Union(i as u32, Box::new(val)));
                }
            }
            anyhow::bail!("{}: value {x} matches no branch of the union", here(path))
        }
        (Schema::Ref { name }, x) => {
            let target = names
                .get(name)
                .with_context(|| format!("{}: unknown named type {name}", here(path)))?;
            json_to_avro_in(target, x, path, names)?
        }
        (Schema::Fixed(f), Value::String(s)) => {
            if s.len() != f.size {
                anyhow::bail!(
                    "{}: fixed {} needs {} bytes, got {}",
                    here(path),
                    f.name,
                    f.size,
                    s.len()
                );
            }
            AvroValue::Fixed(f.size, s.clone().into_bytes())
        }
        (Schema::Decimal(d), x) => {
            let n = unscaled(x, d.scale).with_context(|| here(path))?;
            let size = match &d.inner {
                apache_avro::schema::InnerDecimalSchema::Fixed(f) => Some(f.size),
                _ => {
                    let needed = ((d.precision as f64 * 10f64.log2() + 1.0) / 8.0).ceil() as usize;
                    Some(needed.max(twos_complement(n, None)?.len()))
                }
            };
            AvroValue::Decimal(apache_avro::Decimal::from(
                twos_complement(n, size).with_context(|| here(path))?,
            ))
        }
        (Schema::Date, Value::String(s)) => {
            let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .with_context(|| format!("{}: `{s}` is not a date", here(path)))?;
            AvroValue::Date(
                (d - chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days() as i32,
            )
        }
        (Schema::Date, Value::Number(n)) => AvroValue::Date(
            n.as_i64()
                .and_then(|x| i32::try_from(x).ok())
                .with_context(|| here(path))?,
        ),
        (Schema::TimeMillis, Value::Number(n)) => AvroValue::TimeMillis(
            n.as_i64()
                .and_then(|x| i32::try_from(x).ok())
                .with_context(|| here(path))?,
        ),
        (Schema::TimeMicros, Value::Number(n)) => {
            AvroValue::TimeMicros(n.as_i64().with_context(|| here(path))?)
        }
        (Schema::TimestampMillis, x) => {
            AvroValue::TimestampMillis(epoch(x, 1_000).with_context(|| here(path))?)
        }
        (Schema::TimestampMicros, x) => {
            AvroValue::TimestampMicros(epoch(x, 1_000_000).with_context(|| here(path))?)
        }
        (Schema::TimestampNanos, x) => {
            AvroValue::TimestampNanos(epoch(x, 1_000_000_000).with_context(|| here(path))?)
        }
        (Schema::LocalTimestampMillis, x) => {
            AvroValue::LocalTimestampMillis(epoch(x, 1_000).with_context(|| here(path))?)
        }
        (Schema::LocalTimestampMicros, x) => {
            AvroValue::LocalTimestampMicros(epoch(x, 1_000_000).with_context(|| here(path))?)
        }
        (Schema::LocalTimestampNanos, x) => {
            AvroValue::LocalTimestampNanos(epoch(x, 1_000_000_000).with_context(|| here(path))?)
        }
        (s, x) => anyhow::bail!("{}: {x} does not fit Avro type {s:?}", here(path)),
    })
}

pub fn encode(schema: &Schema, value: AvroValue) -> anyhow::Result<Vec<u8>> {
    let resolved = value
        .resolve(schema)
        .context("value does not match the Avro schema")?;
    #[allow(deprecated)]
    let bytes = apache_avro::to_avro_datum(schema, resolved)?;
    Ok(bytes)
}

pub struct SchemaRegistry {
    base: String,
    http: reqwest::Client,
    auth: Option<BasicAuth>,
    cache: Mutex<HashMap<(String, String), i32>>,
}

impl SchemaRegistry {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        Self::with_options(base, &RegistryOptions::default())
    }

    pub fn with_options(base: &str, opts: &RegistryOptions) -> anyhow::Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            anyhow::bail!("schema registry URL must start with http:// or https://");
        }
        let mut http = reqwest::Client::builder().timeout(Duration::from_secs(10));
        if let Some(ca) = &opts.ca_file {
            for c in read_ca_bundle(ca)? {
                http = http.add_root_certificate(reqwest::Certificate::from_der(c.as_ref())?);
            }
        }
        Ok(Self {
            base,
            http: http.build()?,
            auth: opts.auth.clone(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn register(&self, subject: &str, schema: &str) -> anyhow::Result<i32> {
        let key = (subject.to_string(), schema.to_string());
        if let Some(id) = self.cache.lock().await.get(&key) {
            return Ok(*id);
        }
        let url = format!("{}/subjects/{}/versions", self.base, subject);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/vnd.schemaregistry.v1+json")
            .json(&serde_json::json!({"schema": schema}));
        if let Some(a) = &self.auth {
            req = req.basic_auth(&a.username, Some(&a.password));
        }
        let resp = req.send().await.context("schema registry is unreachable")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            anyhow::bail!("schema registry returned {status} for subject {subject}: {body}");
        }
        let id = body
            .get("id")
            .and_then(|v| v.as_i64())
            .context("schema registry response has no id")? as i32;
        self.cache.lock().await.insert(key, id);
        Ok(id)
    }
}

pub fn decode_avro(schema: &Schema, bytes: &[u8]) -> anyhow::Result<Value> {
    let body = if bytes.len() > 5 && bytes[0] == 0 {
        &bytes[5..]
    } else {
        bytes
    };
    #[allow(deprecated)]
    let v = apache_avro::from_avro_datum(schema, &mut &body[..], None)
        .context("the value is not valid Avro for the schema")?;
    apache_avro::from_value::<Value>(&v).context("the Avro value cannot be represented as JSON")
}

#[derive(Clone, Debug)]
pub struct ConsumedRecord {
    pub partition: i32,
    pub offset: i64,
    pub key: Option<Vec<u8>>,
    pub value: Option<Vec<u8>>,
    pub headers: BTreeMap<String, Vec<u8>>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFrom {
    Latest,
    Earliest,
}

pub struct TopicCursor {
    topic: String,
    positions: Vec<(Arc<PartitionClient>, i64)>,
}

#[derive(Clone, Debug, Default)]
pub struct KafkaSettings {
    pub bootstrap: Vec<String>,
    pub client_id: String,
    pub security: KafkaSecurity,
}

pub struct KafkaPublisher {
    client: Client,
    partitions: Mutex<HashMap<String, i32>>,
    clients: Mutex<HashMap<(String, i32), Arc<PartitionClient>>>,
}

impl KafkaPublisher {
    pub async fn connect(s: &KafkaSettings) -> anyhow::Result<Self> {
        let b = s
            .security
            .apply(ClientBuilder::new(s.bootstrap.clone()).client_id(s.client_id.clone()))?;
        let client = tokio::time::timeout(Duration::from_secs(15), b.build())
            .await
            .context("connecting to Kafka timed out")??;
        Ok(Self {
            client,
            partitions: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
        })
    }

    async fn partition_count(&self, topic: &str) -> anyhow::Result<i32> {
        if let Some(n) = self.partitions.lock().await.get(topic) {
            return Ok(*n);
        }
        let topics = self.client.list_topics().await?;
        let n = topics
            .iter()
            .find(|t| t.name == topic)
            .map(|t| t.partitions.len() as i32)
            .with_context(|| format!("Kafka topic {topic} does not exist"))?;
        self.partitions.lock().await.insert(topic.to_string(), n);
        Ok(n)
    }

    async fn partition_client(
        &self,
        topic: &str,
        partition: i32,
    ) -> anyhow::Result<Arc<PartitionClient>> {
        let key = (topic.to_string(), partition);
        if let Some(c) = self.clients.lock().await.get(&key) {
            return Ok(c.clone());
        }
        let c = Arc::new(
            self.client
                .partition_client(topic.to_string(), partition, UnknownTopicHandling::Retry)
                .await?,
        );
        self.clients.lock().await.insert(key, c.clone());
        Ok(c)
    }

    pub async fn publish(
        &self,
        topic: &str,
        key: Option<Vec<u8>>,
        value: Vec<u8>,
        headers: BTreeMap<String, Vec<u8>>,
    ) -> anyhow::Result<(i32, i64)> {
        let n = self.partition_count(topic).await?;
        let p = partition_for(key.as_deref(), n);
        let pc = self.partition_client(topic, p).await?;
        let record = Record {
            key,
            value: Some(value),
            headers,
            timestamp: chrono::Utc::now(),
        };
        let res = pc.produce(vec![record], Compression::NoCompression).await?;
        Ok((p, res.first().copied().unwrap_or(-1)))
    }

    pub async fn cursor(&self, topic: &str, from: StartFrom) -> anyhow::Result<TopicCursor> {
        let n = self.partition_count(topic).await?;
        let mut positions = Vec::with_capacity(n as usize);
        for p in 0..n {
            let pc = self.partition_client(topic, p).await?;
            let at = match from {
                StartFrom::Latest => OffsetAt::Latest,
                StartFrom::Earliest => OffsetAt::Earliest,
            };
            let offset = pc.get_offset(at).await?;
            positions.push((pc, offset));
        }
        Ok(TopicCursor {
            topic: topic.to_string(),
            positions,
        })
    }

    pub async fn consume(
        &self,
        cursor: &mut TopicCursor,
        max_records: usize,
        max_record_bytes: i32,
        wait: Duration,
    ) -> anyhow::Result<Vec<ConsumedRecord>> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut out = Vec::new();
        let per_poll =
            (wait.as_millis() as i32 / cursor.positions.len().max(1) as i32).clamp(10, 500);
        while out.len() < max_records && tokio::time::Instant::now() < deadline {
            for (pc, offset) in cursor.positions.iter_mut() {
                if out.len() >= max_records {
                    break;
                }
                let (records, _hwm) = pc
                    .fetch_records(*offset, 1..max_record_bytes.max(1024), per_poll)
                    .await
                    .with_context(|| format!("fetching from {}", cursor.topic))?;
                for r in records {
                    if r.offset < *offset {
                        continue;
                    }
                    *offset = r.offset + 1;
                    if out.len() < max_records {
                        out.push(ConsumedRecord {
                            partition: pc.partition(),
                            offset: r.offset,
                            key: r.record.key,
                            value: r.record.value,
                            headers: r.record.headers,
                            timestamp: r.record.timestamp,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn ensure_topics(
        &self,
        topics: &[String],
        partitions: i32,
        replication: i16,
    ) -> anyhow::Result<Vec<String>> {
        let existing: Vec<String> = self
            .client
            .list_topics()
            .await?
            .into_iter()
            .map(|t| t.name)
            .collect();
        let controller = self.client.controller_client()?;
        let mut created = vec![];
        for t in topics {
            if existing.contains(t) {
                continue;
            }
            match controller
                .create_topic(t.clone(), partitions, replication, 30_000)
                .await
            {
                Ok(()) => created.push(t.clone()),
                Err(e) => tracing::warn!(topic = %t, error = %e, "creating Kafka topic failed"),
            }
        }
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_types_and_named_references() {
        let schema = Schema::parse_str(r#"{"type":"record","name":"Order","namespace":"x","fields":[
            {"name":"at","type":{"type":"long","logicalType":"timestamp-millis"}},
            {"name":"day","type":{"type":"int","logicalType":"date"}},
            {"name":"amount","type":{"type":"bytes","logicalType":"decimal","precision":10,"scale":2}},
            {"name":"code","type":{"type":"fixed","name":"Code","size":3}},
            {"name":"again","type":"Code"}]}"#).unwrap();
        let v = json_to_avro(
            &schema,
            &serde_json::json!({"at": "2026-11-19T00:00:00Z", "day": "2026-11-19",
            "amount": "-12.5", "code": "abc", "again": "xyz"}),
        )
        .unwrap();
        let bytes = encode(&schema, v).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(twos_complement(-1250, None).unwrap(), vec![0xfb, 0x1e]);
        assert_eq!(twos_complement(128, Some(4)).unwrap(), vec![0, 0, 0, 0x80]);
    }

    #[test]
    #[allow(deprecated)]
    fn murmur2_matches_kafka() {
        assert_eq!(murmur2(b"21"), -973932308);
        assert_eq!(murmur2(b"foobar"), -790332482);
        assert_eq!(murmur2(b"a-little-bit-long-string"), -985981536);
        assert_eq!(murmur2(b"a-little-bit-longer-string"), -1486304829);
        assert_eq!(
            murmur2(b"lkjh234lh9fiuh90y23oiuhsafujhadof229phr9h19h89h8"),
            -58897971
        );
        assert_eq!(murmur2(b"abc"), 479470107);
    }

    #[test]
    #[allow(deprecated)]
    fn frames_and_schemas() {
        assert_eq!(confluent_frame(7, &[1, 2]), vec![0, 0, 0, 0, 7, 1, 2]);
        let s = serde_json::json!({"type": "record", "name": "Hexagon", "fields": [{"name": "h3Cell", "type": ["null", "string"], "default": null}]});
        let j = with_java_strings(&s);
        assert_eq!(
            j["fields"][0]["type"][1],
            serde_json::json!({"type": "string", "avro.java.string": "String"})
        );
        let schema = Schema::parse(&j).unwrap();
        let v = AvroValue::Record(vec![(
            "h3Cell".into(),
            AvroValue::Union(1, Box::new(AvroValue::String("8a1f".into()))),
        )]);
        let bytes = encode(&schema, v).unwrap();
        assert_eq!(bytes, vec![2, 8, b'8', b'a', b'1', b'f']);
        let back = apache_avro::from_avro_datum(&schema, &mut &bytes[..], None).unwrap();
        assert!(matches!(back, AvroValue::Record(_)));
    }

    #[test]
    #[allow(deprecated)]
    fn json_values_follow_the_schema() {
        let schema = Schema::parse(
            &serde_json::json!({"type": "record", "name": "E", "fields": [
            {"name": "msisdn", "type": "string"},
            {"name": "amount", "type": ["null", "long"], "default": null},
            {"name": "kind", "type": {"type": "enum", "name": "K", "symbols": ["A", "B"]}},
            {"name": "tags", "type": {"type": "array", "items": "string"}},
            {"name": "ratio", "type": "double"}]}),
        )
        .unwrap();
        let v = json_to_avro(&schema, &serde_json::json!({"msisdn": "380", "amount": 15, "kind": "B", "tags": ["x"], "ratio": 0.5})).unwrap();
        let bytes = encode(&schema, v).unwrap();
        let back = apache_avro::from_avro_datum(&schema, &mut &bytes[..], None).unwrap();
        assert_eq!(
            apache_avro::from_value::<serde_json::Value>(&back).unwrap(),
            serde_json::json!({"msisdn": "380", "amount": 15, "kind": "B", "tags": ["x"], "ratio": 0.5})
        );
        assert!(
            json_to_avro(&schema, &serde_json::json!({"amount": 1}))
                .unwrap_err()
                .to_string()
                .contains("$.msisdn")
        );
        let defaults = json_to_avro(
            &schema,
            &serde_json::json!({"msisdn": "1", "kind": "A", "tags": [], "ratio": 1}),
        )
        .unwrap();
        assert!(
            matches!(defaults, AvroValue::Record(ref f) if f[1].1 == AvroValue::Union(0, Box::new(AvroValue::Null)))
        );
    }

    #[test]
    fn decodes_framed_and_raw_avro() {
        let schema = Schema::parse(&serde_json::json!({"type": "record", "name": "E", "fields": [
            {"name": "id", "type": "long"}, {"name": "note", "type": ["null", "string"], "default": null}]}))
        .unwrap();
        let v = json_to_avro(&schema, &serde_json::json!({"id": 7, "note": "x"})).unwrap();
        let raw = encode(&schema, v).unwrap();
        let framed = confluent_frame(3, &raw);
        let expected = serde_json::json!({"id": 7, "note": "x"});
        assert_eq!(decode_avro(&schema, &framed).unwrap(), expected);
        assert_eq!(decode_avro(&schema, &raw).unwrap(), expected);
        assert!(decode_avro(&schema, &[0, 0, 0, 0, 1, 0xff]).is_err());
    }

    #[test]
    fn partitions() {
        assert_eq!(partition_for(Some(b"21"), 1), 0);
        let p = partition_for(Some(b"21"), 12);
        assert_eq!(p, ((-973932308i32) & 0x7fffffff) % 12);
        for _ in 0..50 {
            assert!((0..3).contains(&partition_for(None, 3)));
        }
    }
}
