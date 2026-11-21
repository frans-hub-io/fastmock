//! Provider webhooks (M8): per-space configuration (webhooks, expected provider API keys, the SendGrid
//! signing key pair), delivery events after a provider message is stored and inbound payloads, all sent
//! through the durable callbacks outbox and signed the way each provider signs.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, SecondsFormat, Utc};
use fastmock_core::mail::MailEntry;
use fastmock_core::{CallbackJob, Stores};
use fastmock_mail::ParsedMail;
use fastmock_mail::chaos::area_key;
use fastmock_mail::providers::Provider;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::outbox::{BODY_BASE64_HEADER, EgressPolicy};

/// Data-store namespace of the configuration; the key is the space name or `_global`.
pub const NAMESPACE: &str = "mail-webhooks";
/// `source` of the callback jobs (journal, metrics, `/__admin/callbacks`).
pub const SOURCE: &str = "mail";
pub const MAX_WEBHOOKS: usize = 20;
pub const MAX_KEYS: usize = 20;
/// Delivery attempts of a webhook call (retries on 5xx, 408, 429 and connection errors).
pub const MAX_ATTEMPTS: u32 = 8;
const MAX_URL: usize = 2048;
/// The topic of SES notifications when the webhook names none.
pub const DEFAULT_TOPIC_ARN: &str = "arn:aws:sns:us-east-1:123456789012:fastmock-ses";
/// What `GET` shows instead of a password in a webhook URL; a `PUT` with it keeps the stored one.
pub const MASK: &str = "***";
/// Providers that have webhooks.
pub const WEBHOOK_PROVIDERS: [Provider; 4] = [
    Provider::Ses,
    Provider::Sendgrid,
    Provider::Mailgun,
    Provider::Postmark,
];

/// DER prefix of a P-256 `SubjectPublicKeyInfo` before the 65-byte point.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WebhookKind {
    /// Delivery events after a message is sent through the provider API.
    Events,
    /// Inbound mail posted by `POST /__admin/mail/inbound`.
    Inbound,
}

/// A delivery event. FastMock emits `delivered` unless the recipient's local part (without `+tag`) is
/// `bounce`, `complaint`, `deferred` or `dropped`; `open` and `click` are accepted in the configuration
/// but not emitted.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum MailEventName {
    Delivered,
    Bounce,
    Complaint,
    Deferred,
    Dropped,
    Open,
    Click,
}

impl MailEventName {
    /// The event for a recipient address.
    pub fn for_recipient(addr: &str) -> Self {
        let mailbox = fastmock_mail::address::mailbox_of(addr);
        let local = mailbox.split('@').next().unwrap_or_default();
        match local {
            "bounce" => Self::Bounce,
            "complaint" => Self::Complaint,
            "deferred" => Self::Deferred,
            "dropped" => Self::Dropped,
            _ => Self::Delivered,
        }
    }
}

/// A webhook as `PUT /__admin/mail/webhooks` takes it.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookInput {
    /// Letters, digits, `.`, `_`, `-` (at most 64); `webhook-<n>` when left out. A webhook keeps its
    /// stored `signingKey` and URL password when `PUT` again with the same id without them.
    #[serde(default)]
    #[schema(example = "sendgrid-events")]
    pub id: Option<String>,
    /// `ses`, `sendgrid`, `mailgun` or `postmark`.
    #[schema(example = "sendgrid")]
    pub provider: String,
    pub kind: WebhookKind,
    /// `http(s)://` URL of the application; user info (`https://user:pass@host/`) is sent as basic
    /// authentication (Postmark style). Checked against the callback egress policy.
    #[schema(example = "https://app.example.com/hooks/sendgrid")]
    pub url: String,
    /// Write-only. SendGrid: a P-256 private key (PKCS#8, PEM or base64 DER) to sign with instead of the
    /// space's generated key. Mailgun: the HTTP webhook signing key for the HMAC. `""` removes a stored
    /// key.
    #[serde(default)]
    pub signing_key: Option<String>,
    /// Events to send (`kind: events`); all when empty.
    #[serde(default)]
    pub events: Vec<MailEventName>,
    /// SendGrid inbound: post the full MIME message (`email` field) instead of parsed fields.
    #[serde(default)]
    pub raw: bool,
    /// SES: the SNS topic ARN in notifications (default `arn:aws:sns:us-east-1:123456789012:fastmock-ses`).
    #[serde(default)]
    pub topic_arn: Option<String>,
}

/// The stored form; never returned as it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredWebhook {
    pub id: String,
    pub provider: String,
    pub kind: WebhookKind,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    #[serde(default)]
    pub events: Vec<MailEventName>,
    #[serde(default)]
    pub raw: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_arn: Option<String>,
}

impl StoredWebhook {
    fn wants(&self, e: MailEventName) -> bool {
        self.events.is_empty() || self.events.contains(&e)
    }

    fn provider(&self) -> Option<Provider> {
        Provider::parse(&self.provider)
    }
}

/// The configuration of an area.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMailHooks {
    #[serde(default)]
    pub webhooks: Vec<StoredWebhook>,
    /// Provider name to SHA-256 hex digests of the accepted API keys.
    #[serde(default)]
    pub api_keys: BTreeMap<String, Vec<String>>,
    /// The space's SendGrid signing key (PKCS#8, base64), generated on the first `PUT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sendgrid_key: Option<String>,
}

/// Body of `PUT /__admin/mail/webhooks`.
#[derive(Clone, Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailWebhooksBody {
    /// At most 20.
    #[serde(default)]
    pub webhooks: Vec<WebhookInput>,
    /// Expected API keys per provider (`ses`, `sendgrid`, `mailgun`, `postmark`, `resend`, `brevo`) as
    /// lowercase SHA-256 hex digests, at most 20 each. A provider listed here accepts only these keys in
    /// this space (SES: the access key id); others accept any key.
    #[serde(default)]
    #[schema(example = json!({"sendgrid": ["9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]}))]
    pub api_keys: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookView {
    pub id: String,
    pub provider: String,
    pub kind: WebhookKind,
    /// The URL with its password replaced by `***`.
    pub url: String,
    pub events: Vec<MailEventName>,
    /// A signing key is stored (never shown).
    pub has_signing_key: bool,
    /// SendGrid: the public key that verifies this webhook's signatures (base64 DER).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub raw: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic_arn: Option<String>,
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailWebhooksView {
    /// The space; absent for the global area.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "billing")]
    pub space: Option<String>,
    pub webhooks: Vec<WebhookView>,
    pub api_keys: BTreeMap<String, Vec<String>>,
    /// The space's SendGrid verification key (base64 DER `SubjectPublicKeyInfo`), for the application's
    /// Event Webhook and Inbound Parse verification settings. Absent until webhooks are first stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sendgrid_public_key: Option<String>,
}

/// Reads the configuration of an area.
pub async fn load(stores: &Stores, area: Option<&str>) -> anyhow::Result<StoredMailHooks> {
    match stores.data.get(NAMESPACE, area_key(area)).await? {
        Some(v) => Ok(serde_json::from_value(v)?),
        None => Ok(StoredMailHooks::default()),
    }
}

pub async fn save(stores: &Stores, area: Option<&str>, h: &StoredMailHooks) -> anyhow::Result<()> {
    stores
        .data
        .put(NAMESPACE, area_key(area), &serde_json::to_value(h)?)
        .await
}

pub async fn remove(stores: &Stores, area: Option<&str>) -> anyhow::Result<bool> {
    stores.data.delete(NAMESPACE, area_key(area)).await
}

/// SHA-256 hex of an API key.
pub fn key_digest(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// Whether `key` passes the expected keys of `provider` in every area that lists some.
pub async fn key_allowed(
    stores: &Stores,
    areas: &[Option<String>],
    provider: Provider,
    key: &str,
) -> anyhow::Result<bool> {
    if fastmock_mail::providers::always_accepted(provider, key) {
        return Ok(true);
    }
    let digest = key_digest(key);
    for a in areas {
        let h = load(stores, a.as_deref()).await?;
        if let Some(list) = h.api_keys.get(provider.name())
            && !list.is_empty()
            && !list
                .iter()
                .any(|d| subtle::ConstantTimeEq::ct_eq(d.as_bytes(), digest.as_bytes()).into())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A P-256 key pair from PKCS#8 (PEM or base64 DER).
fn pkcs8_from_text(s: &str) -> Result<Vec<u8>, String> {
    let body: String = if s.contains("-----BEGIN") {
        if !s.contains("BEGIN PRIVATE KEY") {
            return Err(
                "signingKey must be a PKCS#8 key (BEGIN PRIVATE KEY); convert SEC1 keys with `openssl pkcs8 -topk8 -nocrypt`"
                    .into(),
            );
        }
        s.lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("")
    } else {
        s.to_string()
    };
    let der = B64
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| "signingKey is not valid PEM or base64".to_string())?;
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &der, &SystemRandom::new())
        .map_err(|_| "signingKey must be a P-256 (prime256v1) private key in PKCS#8".to_string())?;
    Ok(der)
}

fn key_pair(pkcs8: &[u8]) -> Option<EcdsaKeyPair> {
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &SystemRandom::new()).ok()
}

/// A new P-256 key pair (PKCS#8, base64).
pub fn generate_sendgrid_key() -> String {
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
        .expect("generating a P-256 key");
    B64.encode(doc.as_ref())
}

/// The base64 DER `SubjectPublicKeyInfo` of a PKCS#8 P-256 key, as SendGrid shows verification keys.
pub fn public_key_of(pkcs8: &[u8]) -> Option<String> {
    let kp = key_pair(pkcs8)?;
    let mut der = P256_SPKI_PREFIX.to_vec();
    der.extend_from_slice(kp.public_key().as_ref());
    Some(B64.encode(der))
}

/// The SendGrid signature: base64 of the ASN.1 ECDSA P-256 SHA-256 signature of `timestamp + body`.
pub fn sendgrid_signature(pkcs8: &[u8], timestamp: &str, body: &[u8]) -> Option<String> {
    let kp = key_pair(pkcs8)?;
    let mut msg = timestamp.as_bytes().to_vec();
    msg.extend_from_slice(body);
    kp.sign(&SystemRandom::new(), &msg)
        .ok()
        .map(|s| B64.encode(s.as_ref()))
}

/// The Mailgun signature: hex HMAC-SHA256 of `timestamp + token` with the signing key.
pub fn mailgun_signature(key: &str, timestamp: &str, token: &str) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key.as_bytes());
    let mut ctx = ring::hmac::Context::with_key(&k);
    ctx.update(timestamp.as_bytes());
    ctx.update(token.as_bytes());
    hex::encode(ctx.sign().as_ref())
}

fn masked(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some(MASK));
            u.to_string()
        }
        _ => url.to_string(),
    }
}

/// The view `GET` returns: no signing keys, URL passwords masked.
pub fn view(area: Option<String>, h: &StoredMailHooks) -> MailWebhooksView {
    let space_key = h
        .sendgrid_key
        .as_deref()
        .and_then(|k| B64.decode(k).ok())
        .and_then(|k| public_key_of(&k));
    MailWebhooksView {
        space: area,
        webhooks: h
            .webhooks
            .iter()
            .map(|w| WebhookView {
                id: w.id.clone(),
                provider: w.provider.clone(),
                kind: w.kind,
                url: masked(&w.url),
                events: w.events.clone(),
                has_signing_key: w.signing_key.is_some(),
                public_key: (w.provider == "sendgrid")
                    .then(|| {
                        w.signing_key
                            .as_deref()
                            .and_then(|k| B64.decode(k).ok())
                            .and_then(|k| public_key_of(&k))
                            .or_else(|| space_key.clone())
                    })
                    .flatten(),
                raw: w.raw,
                topic_arn: w.topic_arn.clone(),
            })
            .collect(),
        api_keys: h.api_keys.clone(),
        sendgrid_public_key: space_key,
    }
}

/// Validates a new configuration against the stored one: ids, URLs (egress policy), keys; keeps
/// stored signing keys and URL passwords where the input omits them; keeps (or creates) the space's
/// SendGrid key pair.
pub fn merge(
    old: &StoredMailHooks,
    body: MailWebhooksBody,
    policy: &EgressPolicy,
) -> Result<StoredMailHooks, String> {
    if body.webhooks.len() > MAX_WEBHOOKS {
        return Err(format!("at most {MAX_WEBHOOKS} webhooks per space"));
    }
    let mut api_keys = BTreeMap::new();
    for (p, list) in body.api_keys {
        let provider =
            Provider::parse(&p).ok_or_else(|| format!("apiKeys: unknown provider `{p}`"))?;
        if list.len() > MAX_KEYS {
            return Err(format!("apiKeys.{p}: at most {MAX_KEYS} keys"));
        }
        let mut out: Vec<String> = Vec::with_capacity(list.len());
        for d in list {
            let d = d.trim().to_ascii_lowercase();
            if d.len() != 64 || !d.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "apiKeys.{p}: each entry must be the SHA-256 hex digest of a key (64 hex digits)"
                ));
            }
            if !out.contains(&d) {
                out.push(d);
            }
        }
        if !out.is_empty() {
            api_keys.insert(provider.name().to_string(), out);
        }
    }
    let mut ids: Vec<String> = body.webhooks.iter().filter_map(|w| w.id.clone()).collect();
    let mut webhooks = Vec::with_capacity(body.webhooks.len());
    for (i, w) in body.webhooks.into_iter().enumerate() {
        let at = |m: String| format!("webhooks[{i}]: {m}");
        let id = match w.id.clone() {
            Some(id) => {
                if !valid_id(&id) {
                    return Err(at(format!(
                        "id `{id}` must be 1-64 letters, digits, `.`, `_`, `-`"
                    )));
                }
                if webhooks.iter().any(|x: &StoredWebhook| x.id == id) {
                    return Err(at(format!("id `{id}` is used twice")));
                }
                id
            }
            None => {
                let mut n = i + 1;
                while ids.contains(&format!("webhook-{n}")) {
                    n += 1;
                }
                let id = format!("webhook-{n}");
                ids.push(id.clone());
                id
            }
        };
        let provider = Provider::parse(&w.provider)
            .filter(|p| WEBHOOK_PROVIDERS.contains(p))
            .ok_or_else(|| {
                at(format!(
                    "provider `{}` has no webhooks; use ses, sendgrid, mailgun or postmark",
                    w.provider
                ))
            })?;
        let previous = old.webhooks.iter().find(|x| x.id == id);
        if w.url.len() > MAX_URL {
            return Err(at(format!("url is longer than {MAX_URL} characters")));
        }
        let mut url = reqwest::Url::parse(w.url.trim()).map_err(|e| at(format!("url: {e}")))?;
        if url.password() == Some(MASK) || url.password() == Some("%2A%2A%2A") {
            let old_password = previous
                .and_then(|p| reqwest::Url::parse(&p.url).ok())
                .and_then(|u| u.password().map(String::from));
            match old_password {
                Some(pw) => {
                    let _ = url.set_password(Some(&pw));
                }
                None => return Err(at("url: no stored password to keep".into())),
            }
        }
        let mut bare = url.clone();
        let _ = bare.set_username("");
        let _ = bare.set_password(None);
        policy
            .check_url(&bare)
            .map_err(|e| at(format!("url: {e}")))?;
        let signing_key = match w.signing_key.as_deref() {
            Some("") => None,
            Some(k) => match provider {
                Provider::Sendgrid => Some(B64.encode(pkcs8_from_text(k).map_err(at)?)),
                Provider::Mailgun => {
                    if k.len() > 256 || k.chars().any(char::is_control) {
                        return Err(at(
                            "signingKey must be at most 256 printable characters".into()
                        ));
                    }
                    Some(k.to_string())
                }
                _ => {
                    return Err(at(
                        "signingKey applies to sendgrid and mailgun webhooks only".into(),
                    ));
                }
            },
            None => previous
                .filter(|p| p.provider == provider.name())
                .and_then(|p| p.signing_key.clone()),
        };
        if w.kind == WebhookKind::Inbound && !w.events.is_empty() {
            return Err(at("events apply to kind events only".into()));
        }
        if w.raw && !(provider == Provider::Sendgrid && w.kind == WebhookKind::Inbound) {
            return Err(at("raw applies to sendgrid inbound webhooks only".into()));
        }
        if let Some(t) = &w.topic_arn
            && (provider != Provider::Ses
                || !t.starts_with("arn:")
                || t.len() > 256
                || t.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            return Err(at(
                "topicArn applies to ses webhooks and must be an ARN (arn:...)".into(),
            ));
        }
        let mut events = w.events;
        events.sort();
        events.dedup();
        webhooks.push(StoredWebhook {
            id,
            provider: provider.name().to_string(),
            kind: w.kind,
            url: url.to_string(),
            signing_key,
            events,
            raw: w.raw,
            topic_arn: w.topic_arn,
        });
    }
    Ok(StoredMailHooks {
        webhooks,
        api_keys,
        sendgrid_key: old
            .sendgrid_key
            .clone()
            .or_else(|| Some(generate_sendgrid_key())),
    })
}

/// A request to queue.
#[derive(Clone, Debug, PartialEq)]
pub struct Outgoing {
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Outgoing {
    fn json(v: &Value, content_type: &str) -> Self {
        Self {
            headers: BTreeMap::from([("content-type".to_string(), content_type.to_string())]),
            body: serde_json::to_vec(v).unwrap_or_default(),
        }
    }
}

/// The URL without user info, and the basic authentication header the user info stands for.
fn target(url: &str) -> Option<(String, Option<String>)> {
    let mut u = reqwest::Url::parse(url).ok()?;
    let auth = (!u.username().is_empty() || u.password().is_some()).then(|| {
        let user = percent_decode(u.username());
        let pass = percent_decode(u.password().unwrap_or(""));
        format!("Basic {}", B64.encode(format!("{user}:{pass}")))
    });
    let _ = u.set_username("");
    let _ = u.set_password(None);
    Some((u.to_string(), auth))
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Queues a request for a webhook through the callbacks outbox.
pub async fn enqueue(
    stores: &Stores,
    area: Option<&str>,
    hook: &StoredWebhook,
    out: Outgoing,
    correlation: &str,
) -> anyhow::Result<uuid::Uuid> {
    let (url, auth) =
        target(&hook.url).ok_or_else(|| anyhow::anyhow!("webhook {} has no valid url", hook.id))?;
    let mut headers = out.headers;
    if let Some(a) = auth {
        headers.insert("authorization".into(), a);
    }
    let body = match String::from_utf8(out.body) {
        Ok(s) => s,
        Err(e) => {
            headers.insert(BODY_BASE64_HEADER.into(), "1".into());
            B64.encode(e.into_bytes())
        }
    };
    let mut job = CallbackJob::new(
        SOURCE,
        url,
        "POST".into(),
        headers,
        body,
        Utc::now(),
        MAX_ATTEMPTS,
    )
    .with_correlation(correlation);
    job.space = area.map(String::from);
    stores.outbox.enqueue(&job).await?;
    metrics::counter!("fastmock_mail_webhooks_total", "provider" => hook.provider.clone(), "kind" => match hook.kind { WebhookKind::Events => "events", WebhookKind::Inbound => "inbound" })
        .increment(1);
    Ok(job.id)
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        &uuid::Uuid::new_v4().simple().to_string()[..18]
    )
}

fn short_id() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes())
}

/// What a provider message looks like to the webhook builders.
#[derive(Clone, Debug)]
pub struct Sent<'a> {
    pub entry: &'a MailEntry,
    pub provider_id: String,
    /// `Message-ID` without brackets.
    pub message_id: String,
}

impl<'a> Sent<'a> {
    pub fn new(entry: &'a MailEntry) -> Self {
        let extras = entry.provider_extras.clone().unwrap_or_default();
        let message_id = entry
            .summary
            .message_id
            .clone()
            .unwrap_or_else(|| format!("{}@fastmock", entry.id.simple()));
        Self {
            entry,
            provider_id: extras
                .provider_message_id
                .unwrap_or_else(|| entry.id.to_string()),
            message_id,
        }
    }

    fn extras(&self) -> fastmock_core::mail::ProviderExtras {
        self.entry.provider_extras.clone().unwrap_or_default()
    }
}

/// SendGrid Event Webhook: one JSON array with an event per recipient.
pub fn sendgrid_events(
    sent: &Sent<'_>,
    events: &[(String, MailEventName)],
    at: DateTime<Utc>,
) -> Vec<Value> {
    let ex = sent.extras();
    events
        .iter()
        .enumerate()
        .map(|(n, (rcpt, e))| {
            let mut v = Map::new();
            for (k, val) in &ex.metadata {
                v.insert(k.clone(), val.clone());
            }
            v.insert("email".into(), json!(rcpt));
            v.insert("timestamp".into(), json!(at.timestamp()));
            v.insert("smtp-id".into(), json!(format!("<{}>", sent.message_id)));
            v.insert("sg_event_id".into(), json!(short_id()));
            v.insert(
                "sg_message_id".into(),
                json!(format!(
                    "{}.fastmock-{n}.{}",
                    sent.provider_id,
                    sent.entry.id.simple()
                )),
            );
            match ex.tags.len() {
                0 => {}
                1 => {
                    v.insert("category".into(), json!(ex.tags[0]));
                }
                _ => {
                    v.insert("category".into(), json!(ex.tags));
                }
            }
            match e {
                MailEventName::Bounce => {
                    v.insert("event".into(), json!("bounce"));
                    v.insert("type".into(), json!("bounce"));
                    v.insert("status".into(), json!("5.1.1"));
                    v.insert(
                        "reason".into(),
                        json!(
                            "550 5.1.1 The email account that you tried to reach does not exist."
                        ),
                    );
                    v.insert("bounce_classification".into(), json!("Invalid Address"));
                }
                MailEventName::Complaint => {
                    v.insert("event".into(), json!("spamreport"));
                }
                MailEventName::Deferred => {
                    v.insert("event".into(), json!("deferred"));
                    v.insert("response".into(), json!("451 4.3.0 Try again later"));
                    v.insert("attempt".into(), json!("1"));
                }
                MailEventName::Dropped => {
                    v.insert("event".into(), json!("dropped"));
                    v.insert("reason".into(), json!("Bounced Address"));
                    v.insert("status".into(), json!("5.0.0"));
                }
                _ => {
                    v.insert("event".into(), json!("delivered"));
                    v.insert("response".into(), json!("250 2.0.0 OK"));
                }
            }
            Value::Object(v)
        })
        .collect()
}

/// Mailgun event webhook: `{"signature": {...}, "event-data": {...}}` for one recipient.
pub fn mailgun_event(
    sent: &Sent<'_>,
    rcpt: &str,
    e: MailEventName,
    key: &str,
    at: DateTime<Utc>,
) -> Value {
    let ex = sent.extras();
    let ts = at.timestamp().to_string();
    let tok = token();
    let s = &sent.entry.summary;
    let (event, severity, code, message, level) = match e {
        MailEventName::Bounce => (
            "failed",
            Some("permanent"),
            550,
            "5.1.1 The email account that you tried to reach does not exist.",
            "error",
        ),
        MailEventName::Dropped => (
            "failed",
            Some("permanent"),
            605,
            "Not delivering to previously bounced address",
            "error",
        ),
        MailEventName::Deferred => (
            "failed",
            Some("temporary"),
            451,
            "4.3.0 Try again later",
            "warn",
        ),
        MailEventName::Complaint => ("complained", None, 0, "", "warn"),
        _ => ("delivered", None, 250, "OK", "info"),
    };
    let mut data = json!({
        "id": short_id(),
        "timestamp": at.timestamp_millis() as f64 / 1000.0,
        "log-level": level,
        "event": event,
        "recipient": rcpt,
        "recipient-domain": fastmock_mail::address::domain_of(rcpt).unwrap_or_default(),
        "tags": ex.tags,
        "user-variables": ex.metadata,
        "envelope": {"sender": sent.entry.envelope.mail_from, "targets": rcpt, "transport": "smtp"},
        "message": {
            "headers": {
                "to": s.to.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "),
                "message-id": sent.provider_id.trim_matches(['<', '>']),
                "from": s.from.first().map(|a| a.to_string()).unwrap_or_default(),
                "subject": s.subject.clone().unwrap_or_default(),
            },
            "attachments": [],
            "size": s.size,
        },
        "flags": {"is-test-mode": ex.test_mode, "is-authenticated": true},
    });
    if event != "complained" {
        data["delivery-status"] = json!({
            "code": code, "message": message, "description": if code >= 400 { message } else { "" },
            "attempt-no": 1, "tls": true,
        });
    }
    if let Some(sev) = severity {
        data["severity"] = json!(sev);
        data["reason"] = json!(if e == MailEventName::Dropped {
            "suppress-bounce"
        } else {
            "generic"
        });
    }
    json!({
        "signature": {"timestamp": ts, "token": tok, "signature": mailgun_signature(key, &ts, &tok)},
        "event-data": data,
    })
}

/// Postmark Delivery, Bounce or SpamComplaint webhook for one recipient.
pub fn postmark_event(sent: &Sent<'_>, rcpt: &str, e: MailEventName, at: DateTime<Utc>) -> Value {
    let ex = sent.extras();
    let tag = ex.tags.first().cloned().unwrap_or_default();
    let s = &sent.entry.summary;
    if e == MailEventName::Delivered {
        return json!({
            "RecordType": "Delivery", "ServerID": 1, "MessageStream": "outbound",
            "MessageID": sent.provider_id, "Recipient": rcpt, "Tag": tag,
            "DeliveredAt": iso(at), "Details": "Test delivery webhook details", "Metadata": ex.metadata,
        });
    }
    let (record, kind, code, name, description, inactive) = match e {
        MailEventName::Complaint => (
            "SpamComplaint",
            "SpamComplaint",
            100001,
            "Spam complaint",
            "The subscriber explicitly marked this message as spam.",
            true,
        ),
        MailEventName::Deferred => (
            "Bounce",
            "SoftBounce",
            4096,
            "Soft bounce",
            "Unable to temporarily deliver this message.",
            false,
        ),
        MailEventName::Dropped => (
            "Bounce",
            "ManuallyDeactivated",
            100002,
            "Manually deactivated",
            "The email was manually deactivated.",
            true,
        ),
        _ => (
            "Bounce",
            "HardBounce",
            1,
            "Hard bounce",
            "The server was unable to deliver your message (ex: unknown user, mailbox not found).",
            true,
        ),
    };
    json!({
        "RecordType": record, "ID": at.timestamp_millis(), "Type": kind, "TypeCode": code, "Name": name,
        "Tag": tag, "MessageID": sent.provider_id, "Metadata": ex.metadata, "ServerID": 1,
        "MessageStream": "outbound", "Description": description,
        "Details": "smtp;550 5.1.1 The email account that you tried to reach does not exist.",
        "Email": rcpt, "From": sent.entry.envelope.mail_from, "BouncedAt": iso(at),
        "DumpAvailable": false, "Inactive": inactive, "CanActivate": true,
        "Subject": s.subject.clone().unwrap_or_default(), "Content": null,
    })
}

fn ses_mail(sent: &Sent<'_>, at: DateTime<Utc>, headers: Option<Vec<Value>>) -> Value {
    let s = &sent.entry.summary;
    let ex = sent.extras();
    let tags: Map<String, Value> = ex
        .metadata
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                json!([v
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())]),
            )
        })
        .collect();
    let mut m = json!({
        "timestamp": iso(at),
        "messageId": sent.provider_id,
        "source": sent.entry.envelope.mail_from,
        "sendingAccountId": "123456789012",
        "destination": sent.entry.envelope.rcpt_to,
        "headersTruncated": false,
        "commonHeaders": {
            "from": s.from.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "to": s.to.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "messageId": sent.message_id,
            "subject": s.subject.clone().unwrap_or_default(),
        },
        "tags": tags,
    });
    if let Some(h) = headers {
        m["headers"] = Value::Array(h);
    }
    m
}

/// SES notifications for one message: one per event type with its recipients.
pub fn ses_notifications(
    sent: &Sent<'_>,
    events: &[(String, MailEventName)],
    at: DateTime<Utc>,
) -> Vec<Value> {
    let mut by: BTreeMap<MailEventName, Vec<String>> = BTreeMap::new();
    for (r, e) in events {
        by.entry(*e).or_default().push(r.clone());
    }
    let fid = format!("{}-000000", uuid::Uuid::new_v4());
    by.into_iter()
        .filter(|(e, _)| !matches!(e, MailEventName::Open | MailEventName::Click))
        .map(|(e, rcpts)| {
            let mail = ses_mail(sent, at, None);
            match e {
                MailEventName::Delivered => json!({
                    "notificationType": "Delivery", "mail": mail,
                    "delivery": {"timestamp": iso(at), "processingTimeMillis": 500, "recipients": rcpts,
                                 "smtpResponse": "250 2.6.0 Message received"},
                }),
                MailEventName::Complaint => json!({
                    "notificationType": "Complaint", "mail": mail,
                    "complaint": {"complainedRecipients": rcpts.iter().map(|r| json!({"emailAddress": r})).collect::<Vec<_>>(),
                                  "timestamp": iso(at), "feedbackId": fid, "complaintFeedbackType": "abuse"},
                }),
                other => {
                    let (kind, sub, action, status) = match other {
                        MailEventName::Deferred => ("Transient", "General", "delayed", "4.3.0"),
                        MailEventName::Dropped => ("Permanent", "Suppressed", "failed", "5.1.1"),
                        _ => ("Permanent", "General", "failed", "5.1.1"),
                    };
                    json!({
                        "notificationType": "Bounce", "mail": mail,
                        "bounce": {"bounceType": kind, "bounceSubType": sub, "timestamp": iso(at), "feedbackId": fid,
                                   "bouncedRecipients": rcpts.iter().map(|r| json!({"emailAddress": r, "action": action, "status": status, "diagnosticCode": format!("smtp; {} {status} recipient rejected", if action == "delayed" { 451 } else { 550 })})).collect::<Vec<_>>()},
                    })
                }
            }
        })
        .collect()
}

/// An SNS `Notification` carrying `message` (unsigned: FastMock cannot sign for AWS).
pub fn sns_envelope(topic: &str, message: &Value, at: DateTime<Utc>) -> Outgoing {
    let id = uuid::Uuid::new_v4().to_string();
    let subscription = format!("{topic}:{}", uuid::Uuid::new_v4());
    let region = topic.split(':').nth(3).unwrap_or("us-east-1");
    let body = json!({
        "Type": "Notification",
        "MessageId": id,
        "TopicArn": topic,
        "Message": message.to_string(),
        "Timestamp": iso(at),
        "SignatureVersion": "1",
        "Signature": "FASTMOCK-UNSIGNED",
        "SigningCertURL": format!("https://sns.{region}.amazonaws.com/SimpleNotificationService-fastmock-unsigned.pem"),
        "UnsubscribeURL": format!("https://sns.{region}.amazonaws.com/?Action=Unsubscribe&SubscriptionArn={subscription}"),
    });
    let mut out = Outgoing::json(&body, "text/plain; charset=UTF-8");
    out.headers
        .insert("x-amz-sns-message-type".into(), "Notification".into());
    out.headers.insert("x-amz-sns-message-id".into(), id);
    out.headers
        .insert("x-amz-sns-topic-arn".into(), topic.to_string());
    out.headers
        .insert("x-amz-sns-subscription-arn".into(), subscription);
    out.headers.insert(
        "user-agent".into(),
        "Amazon Simple Notification Service Agent".into(),
    );
    out
}

fn sign_sendgrid(
    out: &mut Outgoing,
    hook: &StoredWebhook,
    space_key: Option<&str>,
    at: DateTime<Utc>,
) {
    let key = hook
        .signing_key
        .as_deref()
        .or(space_key)
        .and_then(|k| B64.decode(k).ok());
    let ts = at.timestamp().to_string();
    if let Some(sig) = key.and_then(|k| sendgrid_signature(&k, &ts, &out.body)) {
        out.headers
            .insert("x-twilio-email-event-webhook-signature".into(), sig);
        out.headers
            .insert("x-twilio-email-event-webhook-timestamp".into(), ts);
    }
}

/// The requests the delivery events of a stored provider message make for one webhook.
pub fn event_requests(
    hook: &StoredWebhook,
    space_key: Option<&str>,
    sent: &Sent<'_>,
    at: DateTime<Utc>,
) -> Vec<Outgoing> {
    let events: Vec<(String, MailEventName)> = sent
        .entry
        .envelope
        .rcpt_to
        .iter()
        .map(|r| (r.clone(), MailEventName::for_recipient(r)))
        .filter(|(_, e)| hook.wants(*e))
        .collect();
    if events.is_empty() {
        return vec![];
    }
    match hook.provider() {
        Some(Provider::Sendgrid) => {
            let mut out = Outgoing::json(
                &Value::Array(sendgrid_events(sent, &events, at)),
                "application/json",
            );
            sign_sendgrid(&mut out, hook, space_key, at);
            vec![out]
        }
        Some(Provider::Mailgun) => events
            .iter()
            .map(|(r, e)| {
                Outgoing::json(
                    &mailgun_event(sent, r, *e, hook.signing_key.as_deref().unwrap_or(""), at),
                    "application/json",
                )
            })
            .collect(),
        Some(Provider::Postmark) => events
            .iter()
            .map(|(r, e)| Outgoing::json(&postmark_event(sent, r, *e, at), "application/json"))
            .collect(),
        Some(Provider::Ses) => {
            let topic = hook.topic_arn.as_deref().unwrap_or(DEFAULT_TOPIC_ARN);
            ses_notifications(sent, &events, at)
                .iter()
                .map(|n| sns_envelope(topic, n, at))
                .collect()
        }
        _ => vec![],
    }
}

/// Queues the delivery events of a stored provider message for the events webhooks of its space.
/// Messages sent in test mode get none. Failures are logged; they never fail the API call.
pub async fn emit_events(stores: &Stores, entry: &MailEntry) {
    let Some(provider) = entry.provider.as_deref() else {
        return;
    };
    if entry.provider_extras.as_ref().is_some_and(|e| e.test_mode) {
        return;
    }
    let hooks = match load(stores, entry.space.as_deref()).await {
        Ok(h) => h,
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "mail_webhooks")
                .increment(1);
            tracing::warn!(error = %e, "reading mail webhooks failed");
            return;
        }
    };
    let sent = Sent::new(entry);
    let now = Utc::now();
    for hook in hooks
        .webhooks
        .iter()
        .filter(|h| h.kind == WebhookKind::Events && h.provider == provider)
    {
        for out in event_requests(hook, hooks.sendgrid_key.as_deref(), &sent, now) {
            if let Err(e) = enqueue(
                stores,
                entry.space.as_deref(),
                hook,
                out,
                &entry.id.to_string(),
            )
            .await
            {
                metrics::counter!("fastmock_store_errors_total", "store" => "outbox").increment(1);
                tracing::warn!(error = %e, webhook = %hook.id, "queueing a mail webhook failed");
            }
        }
    }
}

/// A multipart/form-data body.
#[derive(Default)]
pub struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

fn quote(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace('"', "%22")
}

impl Multipart {
    pub fn new() -> Self {
        Self {
            boundary: format!("xYzZY{}", uuid::Uuid::new_v4().simple()),
            body: Vec::new(),
        }
    }

    pub fn text(&mut self, name: &str, value: &str) {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                self.boundary,
                quote(name)
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(value.as_bytes());
        self.body.extend_from_slice(b"\r\n");
    }

    pub fn file(&mut self, name: &str, filename: &str, content_type: &str, data: &[u8]) {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                self.boundary,
                quote(name),
                quote(filename),
                quote(content_type)
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(data);
        self.body.extend_from_slice(b"\r\n");
    }

    pub fn finish(mut self) -> Outgoing {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        Outgoing {
            headers: BTreeMap::from([(
                "content-type".to_string(),
                format!("multipart/form-data; boundary={}", self.boundary),
            )]),
            body: self.body,
        }
    }
}

/// An inbound message: the raw MIME and its envelope.
#[derive(Clone, Debug)]
pub struct InboundMail {
    pub raw: Vec<u8>,
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
}

struct InboundParts {
    headers_block: String,
    headers: Vec<(String, String)>,
    from: String,
    from_full: Option<fastmock_core::mail::MailAddress>,
    to: String,
    to_full: Vec<fastmock_core::mail::MailAddress>,
    cc: String,
    cc_full: Vec<fastmock_core::mail::MailAddress>,
    subject: String,
    text: Option<String>,
    html: Option<String>,
    date: String,
    message_id: String,
    reply_to: String,
    /// (name, content type, content id, bytes)
    attachments: Vec<(String, String, Option<String>, Vec<u8>)>,
}

fn inbound_parts(raw: &[u8], max_bytes: usize) -> InboundParts {
    let limits = fastmock_mail::ParseLimits {
        max_bytes,
        ..Default::default()
    };
    let p = ParsedMail::parse(raw, &limits);
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 2)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| i + 1))
        .unwrap_or(raw.len());
    let detail = p.detail();
    let join = |v: &[fastmock_core::mail::MailAddress]| {
        v.iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let attachments = p
        .attachments()
        .into_iter()
        .filter_map(|a| {
            let part = p.part(&a.part_id)?;
            Some((
                a.name
                    .clone()
                    .unwrap_or_else(|| format!("attachment-{}", a.part_id)),
                a.content_type.clone(),
                a.content_id.clone(),
                part.bytes,
            ))
        })
        .collect();
    InboundParts {
        headers_block: String::from_utf8_lossy(&raw[..end]).into_owned(),
        headers: detail
            .headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect(),
        from: join(&detail.from),
        from_full: detail.from.first().cloned(),
        to: join(&detail.to),
        to_full: detail.to.clone(),
        cc: join(&detail.cc),
        cc_full: detail.cc.clone(),
        subject: detail.subject.clone().unwrap_or_default(),
        text: p.text(),
        html: p.html(),
        date: p
            .header_values("Date")
            .into_iter()
            .next()
            .unwrap_or_default(),
        message_id: detail.message_id.clone().unwrap_or_default(),
        reply_to: join(&detail.reply_to),
        attachments,
    }
}

/// SendGrid Inbound Parse (multipart; `raw` webhooks post the MIME as `email`), signed like events.
pub fn sendgrid_inbound(
    mail: &InboundMail,
    hook: &StoredWebhook,
    space_key: Option<&str>,
    max_bytes: usize,
    at: DateTime<Utc>,
) -> Outgoing {
    let p = inbound_parts(&mail.raw, max_bytes);
    let mut m = Multipart::new();
    let domain = fastmock_mail::address::domain_of(&mail.mail_from).unwrap_or_default();
    m.text("dkim", &format!("{{@{domain} : pass}}"));
    m.text("to", &p.to);
    if !p.cc.is_empty() {
        m.text("cc", &p.cc);
    }
    m.text("from", &p.from);
    m.text("subject", &p.subject);
    m.text("sender_ip", "198.51.100.25");
    m.text("spam_report", "");
    m.text("spam_score", "0.0");
    m.text("SPF", "pass");
    m.text(
        "envelope",
        &json!({"to": mail.rcpt_to, "from": mail.mail_from}).to_string(),
    );
    m.text(
        "charsets",
        &json!({"to": "UTF-8", "from": "UTF-8", "subject": "UTF-8", "text": "UTF-8", "html": "UTF-8"}).to_string(),
    );
    if hook.raw {
        m.text("email", &String::from_utf8_lossy(&mail.raw));
    } else {
        m.text("headers", &p.headers_block);
        if let Some(t) = &p.text {
            m.text("text", t);
        }
        if let Some(h) = &p.html {
            m.text("html", h);
        }
        m.text("attachments", &p.attachments.len().to_string());
        let mut info = Map::new();
        let mut cids = Map::new();
        for (i, (name, ct, cid, data)) in p.attachments.iter().enumerate() {
            let key = format!("attachment{}", i + 1);
            let mut a = json!({"filename": name, "name": name, "type": ct});
            if let Some(c) = cid {
                a["content-id"] = json!(c);
                cids.insert(c.clone(), json!(key));
            }
            info.insert(key.clone(), a);
            m.file(&key, name, ct, data);
        }
        if !info.is_empty() {
            m.text("attachment-info", &Value::Object(info).to_string());
        }
        if !cids.is_empty() {
            m.text("content-ids", &Value::Object(cids).to_string());
        }
    }
    let mut out = m.finish();
    sign_sendgrid(&mut out, hook, space_key, at);
    out
}

/// Mailgun route `forward()`: urlencoded (multipart with attachments); a URL ending in `mime` gets
/// `body-mime`, one ending in `json` a JSON body with the signature in `X-Mailgun-*` headers.
pub fn mailgun_inbound(
    mail: &InboundMail,
    hook: &StoredWebhook,
    max_bytes: usize,
    at: DateTime<Utc>,
) -> Outgoing {
    let p = inbound_parts(&mail.raw, max_bytes);
    let ts = at.timestamp().to_string();
    let tok = token();
    let sig = mailgun_signature(hook.signing_key.as_deref().unwrap_or(""), &ts, &tok);
    let path = reqwest::Url::parse(&hook.url)
        .map(|u| u.path().trim_end_matches('/').to_ascii_lowercase())
        .unwrap_or_default();
    let mut fields: Vec<(String, String)> = vec![
        ("recipient".into(), mail.rcpt_to.join(", ")),
        ("sender".into(), mail.mail_from.clone()),
        ("from".into(), p.from.clone()),
        ("subject".into(), p.subject.clone()),
    ];
    let headers_json =
        Value::Array(p.headers.iter().map(|(k, v)| json!([k, v])).collect()).to_string();
    if path.ends_with("mime") {
        let mut m = Multipart::new();
        for (k, v) in &fields {
            m.text(k, v);
        }
        m.text("body-mime", &String::from_utf8_lossy(&mail.raw));
        m.text("timestamp", &ts);
        m.text("token", &tok);
        m.text("signature", &sig);
        return m.finish();
    }
    let text = p.text.clone().unwrap_or_default();
    let html = p.html.clone().unwrap_or_default();
    fields.extend([
        ("body-plain".into(), text.clone()),
        ("body-html".into(), html.clone()),
        ("stripped-text".into(), text),
        ("stripped-signature".into(), String::new()),
        ("stripped-html".into(), html),
        ("message-headers".into(), headers_json),
        ("attachment-count".into(), p.attachments.len().to_string()),
    ]);
    let mut cid_map = Map::new();
    for (i, (_, _, cid, _)) in p.attachments.iter().enumerate() {
        if let Some(c) = cid {
            cid_map.insert(format!("<{c}>"), json!(format!("attachment-{}", i + 1)));
        }
    }
    if !cid_map.is_empty() {
        fields.push(("content-id-map".into(), Value::Object(cid_map).to_string()));
    }
    if path.ends_with("json") {
        let mut v: Map<String, Value> = fields.into_iter().map(|(k, v)| (k, json!(v))).collect();
        v.insert(
            "attachments".into(),
            Value::Array(
                p.attachments
                    .iter()
                    .map(|(n, ct, _, d)| json!({"filename": n, "content-type": ct, "size": d.len(), "content": B64.encode(d)}))
                    .collect(),
            ),
        );
        v.insert("timestamp".into(), json!(ts));
        v.insert("token".into(), json!(tok));
        let mut out = Outgoing::json(&Value::Object(v), "application/json");
        out.headers.insert("x-mailgun-signature".into(), sig);
        out.headers.insert("x-mailgun-timestamp".into(), ts);
        out.headers.insert("x-mailgun-token".into(), tok);
        return out;
    }
    fields.extend([
        ("timestamp".into(), ts),
        ("token".into(), tok),
        ("signature".into(), sig),
    ]);
    if p.attachments.is_empty() {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter())
            .finish();
        return Outgoing {
            headers: BTreeMap::from([(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )]),
            body: body.into_bytes(),
        };
    }
    let mut m = Multipart::new();
    for (k, v) in &fields {
        m.text(k, v);
    }
    for (i, (n, ct, _, d)) in p.attachments.iter().enumerate() {
        m.file(&format!("attachment-{}", i + 1), n, ct, d);
    }
    m.finish()
}

fn full(a: &fastmock_core::mail::MailAddress) -> Value {
    json!({
        "Email": a.address,
        "Name": a.name.clone().unwrap_or_default(),
        "MailboxHash": fastmock_mail::address::plus_tag(&a.address).unwrap_or_default(),
    })
}

/// Postmark inbound JSON.
pub fn postmark_inbound(mail: &InboundMail, max_bytes: usize) -> Outgoing {
    let p = inbound_parts(&mail.raw, max_bytes);
    let first = mail.rcpt_to.first().cloned().unwrap_or_default();
    let from = p.from_full.clone().unwrap_or_default();
    let v = json!({
        "FromName": from.name.clone().unwrap_or_default(),
        "MessageStream": "inbound",
        "From": from.address,
        "FromFull": full(&from),
        "To": p.to,
        "ToFull": p.to_full.iter().map(full).collect::<Vec<_>>(),
        "Cc": p.cc,
        "CcFull": p.cc_full.iter().map(full).collect::<Vec<_>>(),
        "Bcc": "",
        "BccFull": [],
        "OriginalRecipient": first,
        "Subject": p.subject,
        "MessageID": uuid::Uuid::new_v4().to_string(),
        "ReplyTo": p.reply_to,
        "MailboxHash": fastmock_mail::address::plus_tag(&first).unwrap_or_default(),
        "Date": p.date,
        "TextBody": p.text.unwrap_or_default(),
        "HtmlBody": p.html.unwrap_or_default(),
        "StrippedTextReply": "",
        "Tag": "",
        "Headers": p.headers.iter().map(|(k, v)| json!({"Name": k, "Value": v})).collect::<Vec<_>>(),
        "Attachments": p.attachments.iter().map(|(n, ct, cid, d)| json!({
            "Name": n, "Content": B64.encode(d), "ContentType": ct, "ContentLength": d.len(),
            "ContentID": cid.clone().unwrap_or_default(),
        })).collect::<Vec<_>>(),
    });
    Outgoing::json(&v, "application/json")
}

/// SES receipt (`notificationType: Received`, SNS action) in an SNS envelope.
pub fn ses_inbound(
    mail: &InboundMail,
    hook: &StoredWebhook,
    max_bytes: usize,
    at: DateTime<Utc>,
) -> Outgoing {
    let p = inbound_parts(&mail.raw, max_bytes);
    let topic = hook.topic_arn.as_deref().unwrap_or(DEFAULT_TOPIC_ARN);
    let (encoding, content) = match std::str::from_utf8(&mail.raw) {
        Ok(s) => ("UTF8", s.to_string()),
        Err(_) => ("BASE64", B64.encode(&mail.raw)),
    };
    let pass = json!({"status": "PASS"});
    let message = json!({
        "notificationType": "Received",
        "receipt": {
            "timestamp": iso(at), "processingTimeMillis": 200, "recipients": mail.rcpt_to,
            "spamVerdict": pass, "virusVerdict": pass, "spfVerdict": pass, "dkimVerdict": pass, "dmarcVerdict": pass,
            "action": {"type": "SNS", "topicArn": topic, "encoding": encoding},
        },
        "mail": {
            "timestamp": iso(at),
            "source": mail.mail_from,
            "messageId": uuid::Uuid::new_v4().simple().to_string(),
            "destination": mail.rcpt_to,
            "headersTruncated": false,
            "headers": p.headers.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
            "commonHeaders": {
                "returnPath": mail.mail_from,
                "from": [p.from], "to": [p.to], "date": p.date,
                "messageId": p.message_id, "subject": p.subject,
            },
        },
        "content": content,
    });
    sns_envelope(topic, &message, at)
}

/// The inbound request for a webhook.
pub fn inbound_request(
    mail: &InboundMail,
    hook: &StoredWebhook,
    space_key: Option<&str>,
    max_bytes: usize,
    at: DateTime<Utc>,
) -> Option<Outgoing> {
    Some(match hook.provider()? {
        Provider::Sendgrid => sendgrid_inbound(mail, hook, space_key, max_bytes, at),
        Provider::Mailgun => mailgun_inbound(mail, hook, max_bytes, at),
        Provider::Postmark => postmark_inbound(mail, max_bytes),
        Provider::Ses => ses_inbound(mail, hook, max_bytes, at),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> EgressPolicy {
        EgressPolicy {
            allowed_hosts: vec![],
            denied: vec!["169.254.0.0/16".parse().unwrap()],
        }
    }

    fn input(v: Value) -> MailWebhooksBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn merge_validates_and_keeps_secrets() {
        let first = merge(
            &StoredMailHooks::default(),
            input(json!({"webhooks": [
                {"id": "mg", "provider": "mailgun", "kind": "events", "url": "https://hooks.example.com/mg", "signingKey": "key-1", "events": ["bounce", "delivered", "bounce"]},
                {"provider": "postmark", "kind": "events", "url": "https://user:secret@hooks.example.com/pm"}
            ], "apiKeys": {"SendGrid": [key_digest("SG.x").to_uppercase()]}})),
            &policy(),
        )
        .unwrap();
        assert_eq!(
            first.webhooks[0].events,
            vec![MailEventName::Delivered, MailEventName::Bounce]
        );
        assert_eq!(first.webhooks[1].id, "webhook-2");
        assert_eq!(first.api_keys["sendgrid"], vec![key_digest("SG.x")]);
        assert!(first.sendgrid_key.is_some());
        let v = serde_json::to_string(&view(None, &first)).unwrap();
        assert!(!v.contains("key-1") && !v.contains("secret"), "{v}");
        assert!(v.contains("user:***@hooks.example.com"));
        let second = merge(
            &first,
            input(json!({"webhooks": [
                {"id": "mg", "provider": "mailgun", "kind": "events", "url": "https://hooks.example.com/mg"},
                {"id": "webhook-2", "provider": "postmark", "kind": "events", "url": "https://user:***@hooks.example.com/pm"}
            ]})),
            &policy(),
        )
        .unwrap();
        assert_eq!(second.webhooks[0].signing_key.as_deref(), Some("key-1"));
        assert!(second.webhooks[1].url.contains("user:secret@"));
        assert_eq!(second.sendgrid_key, first.sendgrid_key);
        let bad = |v: Value| merge(&StoredMailHooks::default(), input(v), &policy()).unwrap_err();
        assert!(bad(json!({"webhooks": [{"provider": "resend", "kind": "events", "url": "https://x.example.com/"}]})).contains("no webhooks"));
        assert!(bad(json!({"webhooks": [{"provider": "ses", "kind": "events", "url": "http://169.254.169.254/"}]})).contains("url"));
        assert!(bad(json!({"webhooks": [{"provider": "ses", "kind": "events", "url": "https://x.example.com/", "signingKey": "k"}]})).contains("signingKey"));
        assert!(bad(json!({"webhooks": [{"provider": "sendgrid", "kind": "events", "url": "https://x.example.com/", "signingKey": "bm90IGEga2V5"}]})).contains("P-256"));
        assert!(bad(json!({"apiKeys": {"sendgrid": ["abc"]}})).contains("SHA-256"));
        assert!(bad(json!({"webhooks": [{"provider": "sendgrid", "kind": "inbound", "url": "https://x.example.com/", "events": ["bounce"]}]})).contains("events"));
        assert!(bad(json!({"webhooks": [{"provider": "postmark", "kind": "events", "url": "https://u:***@x.example.com/"}]})).contains("password"));
    }

    #[test]
    fn events_follow_the_local_part() {
        assert_eq!(
            MailEventName::for_recipient("Bounce+x@example.test"),
            MailEventName::Bounce
        );
        assert_eq!(
            MailEventName::for_recipient("complaint@example.test"),
            MailEventName::Complaint
        );
        assert_eq!(
            MailEventName::for_recipient("deferred@example.test"),
            MailEventName::Deferred
        );
        assert_eq!(
            MailEventName::for_recipient("dropped@example.test"),
            MailEventName::Dropped
        );
        assert_eq!(
            MailEventName::for_recipient("bouncer@example.test"),
            MailEventName::Delivered
        );
    }

    #[test]
    fn user_info_becomes_basic_auth() {
        let (url, auth) = target("https://us%40r:p%3Ass@hooks.example.com/x?a=1").unwrap();
        assert_eq!(url, "https://hooks.example.com/x?a=1");
        assert_eq!(auth.unwrap(), format!("Basic {}", B64.encode("us@r:p:ss")));
        assert_eq!(target("https://hooks.example.com/").unwrap().1, None);
    }

    #[test]
    fn sendgrid_keys_sign_and_show_spki() {
        let k = B64.decode(generate_sendgrid_key()).unwrap();
        let spki = B64.decode(public_key_of(&k).unwrap()).unwrap();
        assert_eq!(spki.len(), 91);
        assert_eq!(&spki[..26], &P256_SPKI_PREFIX);
        let sig = B64
            .decode(sendgrid_signature(&k, "1700000000", b"[]").unwrap())
            .unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_ASN1,
            &spki[26..],
        )
        .verify(b"1700000000[]", &sig)
        .unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            B64.encode(&k)
        );
        assert_eq!(pkcs8_from_text(&pem).unwrap(), k);
        assert!(
            pkcs8_from_text("-----BEGIN EC PRIVATE KEY-----\nAA==\n-----END EC PRIVATE KEY-----")
                .is_err()
        );
    }
}
