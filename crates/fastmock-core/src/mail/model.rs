use bytes::Bytes;
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// `received_at` with the microsecond precision PostgreSQL keeps, so that cursors compare alike in
/// every store.
pub fn micros(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(6)
}

/// How a message reached FastMock.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum MailSource {
    /// Received by the SMTP listener.
    Smtp,
    /// Sent through a provider HTTP API (`/__mail/<provider>/...`).
    Provider,
    /// Put in through the admin API (`POST /__admin/mail/messages`) or IMAP APPEND.
    Inject,
}

impl MailSource {
    pub fn as_str(self) -> &'static str {
        match self {
            MailSource::Smtp => "smtp",
            MailSource::Provider => "provider",
            MailSource::Inject => "inject",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "smtp" => Some(MailSource::Smtp),
            "provider" => Some(MailSource::Provider),
            "inject" => Some(MailSource::Inject),
            _ => None,
        }
    }
}

/// The SMTP envelope, or what stands for it for messages that did not come over SMTP.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    /// Reverse path of `MAIL FROM`; empty for the null sender `<>`.
    #[schema(example = "noreply@example.com")]
    pub mail_from: String,
    /// Recipients of this stored copy. A message routed to several spaces lists here only the recipients
    /// of the copy's space.
    #[schema(example = json!(["alice@billing.example.com"]))]
    pub rcpt_to: Vec<String>,
    /// EHLO or HELO name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helo: Option<String>,
    /// Address of the SMTP or HTTP client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "198.51.100.7")]
    pub client_ip: Option<String>,
    /// The session used TLS (SMTPS, or STARTTLS before the message).
    #[serde(default)]
    pub tls: bool,
    /// User name given with SMTP AUTH; never the password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_user: Option<String>,
}

/// A mailbox of an address header.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailAddress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Billing")]
    pub name: Option<String>,
    #[schema(example = "billing@example.com")]
    pub address: String,
}

impl std::fmt::Display for MailAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(n) if !n.is_empty() => write!(f, "{n} <{}>", self.address),
            _ => f.write_str(&self.address),
        }
    }
}

/// What lists and searches show of a message, parsed once when it is stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Your invoice")]
    pub subject: Option<String>,
    #[serde(default)]
    pub from: Vec<MailAddress>,
    #[serde(default)]
    pub to: Vec<MailAddress>,
    #[serde(default)]
    pub cc: Vec<MailAddress>,
    /// The `Date` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
    /// The `Message-ID` header, without angle brackets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Size of the raw message in bytes.
    pub size: u64,
    #[serde(default)]
    pub has_attachments: bool,
    /// File names of the attachments, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_names: Vec<String>,
    #[serde(default)]
    pub has_text: bool,
    #[serde(default)]
    pub has_html: bool,
    /// The start of the text (or of the HTML converted to text), at most 200 characters.
    #[serde(default)]
    pub preview: String,
    /// The message could not be parsed as MIME (too large, or no header block); it is kept raw.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unparsed: bool,
}

/// Who wrote a transcript line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptDirection {
    Client,
    Server,
}

/// One line of an SMTP session: a command (credentials masked, DATA replaced by its size) or a reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptLine {
    pub at: DateTime<Utc>,
    pub direction: TranscriptDirection,
    #[schema(example = "MAIL FROM:<noreply@example.com> SIZE=1024")]
    pub text: String,
}

/// Fields of a message sent through a provider API.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderExtras {
    /// Id the provider API answered with (SES `MessageId`, SendGrid `X-Message-Id`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_data: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
    /// Sent in the provider's sandbox or test mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test_mode: bool,
}

/// A stored message: one copy per space it was routed to.
#[derive(Clone, Debug, PartialEq)]
pub struct MailMessage {
    /// UUIDv7.
    pub id: Uuid,
    pub space: Option<String>,
    /// Microsecond precision (see [`micros`]).
    pub received_at: DateTime<Utc>,
    pub source: MailSource,
    pub provider: Option<String>,
    pub envelope: Envelope,
    pub summary: MailSummary,
    /// Lowercase, sorted, without duplicates.
    pub labels: Vec<String>,
    pub provider_extras: Option<ProviderExtras>,
    pub transcript: Vec<TranscriptLine>,
    /// Lowercase subject and text the free-text search looks in, capped in size.
    pub search_text: String,
    pub raw: Bytes,
}

impl MailMessage {
    /// The message without its raw bytes, transcript and search text.
    pub fn entry(&self) -> MailEntry {
        MailEntry {
            id: self.id,
            space: self.space.clone(),
            received_at: self.received_at,
            source: self.source,
            provider: self.provider.clone(),
            envelope: self.envelope.clone(),
            summary: self.summary.clone(),
            labels: self.labels.clone(),
            provider_extras: self.provider_extras.clone(),
        }
    }
}

/// A message as lists, searches and events show it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailEntry {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "billing")]
    pub space: Option<String>,
    pub received_at: DateTime<Utc>,
    pub source: MailSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "sendgrid")]
    pub provider: Option<String>,
    pub envelope: Envelope,
    pub summary: MailSummary,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extras: Option<ProviderExtras>,
}
