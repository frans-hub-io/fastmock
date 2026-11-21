//! Mail chaos (M6): rules per area (a space or the global area), evaluated in order at an SMTP stage or
//! at the provider APIs; the first rule that fires wins. Rules and their counters live behind
//! [`ChaosBackend`], which the server implements over its shared stores, so that `times`, `every` and
//! seeded `probability` count alike on every replica.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fastmock_core::matcher::CompiledString;
use fastmock_core::model::StringMatcher;
use serde::{Deserialize, Serialize};

/// Most rules per area.
pub const MAX_RULES: usize = 100;
/// Longest `delay`.
pub const MAX_DELAY: Duration = Duration::from_secs(600);
/// Counters expire after a week without use.
pub const COUNTER_TTL: Duration = Duration::from_secs(7 * 86_400);
/// How long a replica keeps the rules of an area before reading them again.
pub const CACHE_TTL: Duration = Duration::from_secs(2);

/// Where a rule applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChaosStage {
    /// A new connection, before the greeting (the reply replaces the greeting and closes).
    Connect,
    /// `STARTTLS` (the reply replaces `220 Ready`); on SMTPS before the handshake, where a reply or
    /// disconnect closes the connection.
    Tls,
    /// `EHLO`/`HELO`.
    Ehlo,
    /// `AUTH`, after the credentials were read.
    Auth,
    /// `MAIL FROM`.
    Mail,
    /// Each `RCPT TO`.
    Rcpt,
    /// The end of the message (after `<CRLF>.<CRLF>` or `BDAT ... LAST`), before it is stored.
    Data,
    /// A provider HTTP API call (`/__mail/<provider>/...`).
    Provider,
}

impl ChaosStage {
    pub fn as_str(self) -> &'static str {
        match self {
            ChaosStage::Connect => "connect",
            ChaosStage::Tls => "tls",
            ChaosStage::Ehlo => "ehlo",
            ChaosStage::Auth => "auth",
            ChaosStage::Mail => "mail",
            ChaosStage::Rcpt => "rcpt",
            ChaosStage::Data => "data",
            ChaosStage::Provider => "provider",
        }
    }

    fn allows(self, field: MatchField) -> bool {
        use ChaosStage as S;
        match field {
            MatchField::AuthUser => matches!(self, S::Auth | S::Mail | S::Rcpt | S::Data),
            MatchField::MailFrom => matches!(self, S::Mail | S::Rcpt | S::Data | S::Provider),
            MatchField::RcptTo => matches!(self, S::Rcpt | S::Data | S::Provider),
            MatchField::Provider => self == S::Provider,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchField {
    RcptTo,
    MailFrom,
    AuthUser,
    Provider,
}

impl MatchField {
    fn name(self) -> &'static str {
        match self {
            MatchField::RcptTo => "rcptTo",
            MatchField::MailFrom => "mailFrom",
            MatchField::AuthUser => "authUser",
            MatchField::Provider => "provider",
        }
    }
}

/// Conditions of a rule, written with the stub string matchers; all given ones must hold. `rcptTo`
/// holds when one recipient matches (at `rcpt`: the recipient of that command).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChaosMatch {
    /// Stages `rcpt`, `data`, `provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rcpt_to: Option<StringMatcher>,
    /// Stages `mail`, `rcpt`, `data`, `provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_from: Option<StringMatcher>,
    /// Stages `auth`, `mail`, `rcpt`, `data`; a session without AUTH has no user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_user: Option<StringMatcher>,
    /// Stage `provider`: the provider name (`ses`, `sendgrid`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<StringMatcher>,
}

impl ChaosMatch {
    fn fields(&self) -> [(MatchField, Option<&StringMatcher>); 4] {
        [
            (MatchField::RcptTo, self.rcpt_to.as_ref()),
            (MatchField::MailFrom, self.mail_from.as_ref()),
            (MatchField::AuthUser, self.auth_user.as_ref()),
            (MatchField::Provider, self.provider.as_ref()),
        ]
    }
}

/// An SMTP reply sent instead of the normal one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChaosReply {
    /// 400-599. 421 also closes the connection.
    #[schema(example = 451)]
    pub code: u16,
    /// Enhanced status code whose class matches the code; default `<class>.0.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "4.3.0")]
    pub enhanced: Option<String>,
    /// Default `FastMock chaos rule <id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Try again later")]
    pub text: Option<String>,
}

/// What a firing rule does: optionally wait (`delay`), then one of `reply`, `disconnect` or
/// `providerError`, or continue normally after a delay alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChaosAction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ChaosReply>,
    /// `500ms`, `2s`, `1m` (at most 10 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "2s")]
    pub delay: Option<String>,
    /// Close the connection without a reply.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disconnect: bool,
    /// Stage `provider` only: the error the provider API answers with, in the provider's format
    /// (for example `MessageRejected`, `rateLimit`, `invalidKey`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "rateLimit")]
    pub provider_error: Option<String>,
}

/// A mail chaos rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailChaosRule {
    /// Letters, digits, `.`, `_`, `-` (at most 64); `rule-<n>` (its position) when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "third-message-fails")]
    pub id: Option<String>,
    pub stage: ChaosStage,
    #[serde(default, rename = "match")]
    pub r#match: ChaosMatch,
    pub action: ChaosAction,
    /// Fire at most this many times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u64>,
    /// Fire only on every n-th matching event (the n-th, 2n-th, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<u64>,
    /// Chance to fire, 0-100 (percent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
    /// With `probability`: decide from the seed and the number of the matching event instead of at
    /// random, so a run repeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl MailChaosRule {
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or_default()
    }
}

/// The stored rules of an area. `version` changes on every replacement, which starts the counters anew.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredChaos {
    pub version: String,
    pub rules: Vec<MailChaosRule>,
}

/// How often a rule matched and fired since its rules were stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChaosCounters {
    pub id: String,
    /// Events that met stage and match.
    pub matched: u64,
    /// Times the rule fired (with `times`, attempts beyond it are counted too).
    pub fired: u64,
}

/// Storage of rules and counters, shared by all replicas.
#[async_trait]
pub trait ChaosBackend: Send + Sync {
    async fn load(&self, area: Option<&str>) -> anyhow::Result<Option<StoredChaos>>;
    async fn save(&self, area: Option<&str>, chaos: &StoredChaos) -> anyhow::Result<()>;
    /// Returns whether rules were stored.
    async fn remove(&self, area: Option<&str>) -> anyhow::Result<bool>;
    /// Adds one and returns the new value.
    async fn incr(&self, key: &str, ttl: Duration) -> anyhow::Result<i64>;
    async fn counter(&self, key: &str) -> anyhow::Result<i64>;
    async fn clear_counters(&self, prefix: &str) -> anyhow::Result<()>;
}

/// The key of an area in counter names: the space name, or `_global` (space names have no `_`).
pub fn area_key(area: Option<&str>) -> &str {
    area.unwrap_or("_global")
}

/// Prefix of the counters of an area: `mail:chaos:<area>:`.
pub fn counter_prefix(area: Option<&str>) -> String {
    format!("mail:chaos:{}:", area_key(area))
}

fn counter_key(area: Option<&str>, version: &str, id: &str, kind: &str) -> String {
    format!("{}{version}:{id}:{kind}", counter_prefix(area))
}

/// Parses `500ms`, `2s`, `1m` (a number alone is seconds).
pub fn parse_delay(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("delay `{s}` must look like 500ms, 2s or 1m"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        _ => return Err(format!("delay `{s}` must look like 500ms, 2s or 1m")),
    };
    if d > MAX_DELAY {
        return Err(format!("delay `{s}` is longer than 10 minutes"));
    }
    Ok(d)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn check_enhanced(code: u16, e: &str) -> Result<(), String> {
    let parts: Vec<&str> = e.split('.').collect();
    let ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        return Err(format!("enhanced `{e}` must look like 4.3.0"));
    }
    if parts[0] != (code / 100).to_string() {
        return Err(format!(
            "enhanced `{e}` must start with {} like the code {code}",
            code / 100
        ));
    }
    Ok(())
}

fn check_rule(r: &MailChaosRule) -> Result<(), String> {
    for (field, m) in r.r#match.fields() {
        let Some(m) = m else { continue };
        if !r.stage.allows(field) {
            return Err(format!(
                "match.{} does not apply at stage {}",
                field.name(),
                r.stage.as_str()
            ));
        }
        CompiledString::compile(m).map_err(|e| format!("match.{}: {e}", field.name()))?;
    }
    let a = &r.action;
    if let Some(d) = &a.delay {
        parse_delay(d)?;
    }
    let effects = [a.reply.is_some(), a.disconnect, a.provider_error.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    if effects > 1 {
        return Err("action takes one of reply, disconnect and providerError".into());
    }
    if effects == 0 && a.delay.is_none() {
        return Err("action needs reply, delay, disconnect or providerError".into());
    }
    if r.stage == ChaosStage::Provider {
        if a.reply.is_some() || a.disconnect {
            return Err(
                "stage provider takes providerError or delay, not reply or disconnect".into(),
            );
        }
    } else if a.provider_error.is_some() {
        return Err("providerError applies only at stage provider".into());
    }
    if let Some(p) = &a.provider_error
        && !(valid_id(p))
    {
        return Err(format!(
            "providerError `{p}` must be a name of letters, digits, `.`, `_`, `-`"
        ));
    }
    if let Some(rep) = &a.reply {
        if !(400..=599).contains(&rep.code) {
            return Err(format!("reply code {} must be 4xx or 5xx", rep.code));
        }
        if let Some(e) = &rep.enhanced {
            check_enhanced(rep.code, e)?;
        }
        if let Some(t) = &rep.text
            && (t.len() > 200 || t.chars().any(|c| c.is_control()))
        {
            return Err("reply text must be at most 200 characters on one line".into());
        }
    }
    if r.times == Some(0) {
        return Err("times must be at least 1".into());
    }
    if r.every == Some(0) {
        return Err("every must be at least 1".into());
    }
    if let Some(p) = r.probability
        && !(p.is_finite() && (0.0..=100.0).contains(&p))
    {
        return Err("probability must be between 0 and 100".into());
    }
    if r.seed.is_some() && r.probability.is_none() {
        return Err("seed needs probability".into());
    }
    Ok(())
}

/// Checks the rules of an area and gives rules without an id `rule-<n>`.
pub fn validate(rules: &mut [MailChaosRule]) -> Result<(), String> {
    if rules.len() > MAX_RULES {
        return Err(format!("at most {MAX_RULES} mail chaos rules per space"));
    }
    let mut seen: Vec<String> = Vec::new();
    for (i, r) in rules.iter().enumerate() {
        if let Some(id) = &r.id {
            if !valid_id(id) {
                return Err(format!(
                    "rules[{i}]: id `{id}` must be 1-64 letters, digits, `.`, `_`, `-`"
                ));
            }
            if seen.contains(id) {
                return Err(format!("rules[{i}]: id `{id}` is used twice"));
            }
            seen.push(id.clone());
        }
        check_rule(r).map_err(|e| format!("rules[{i}]: {e}"))?;
    }
    for (i, r) in rules.iter_mut().enumerate() {
        if r.id.is_none() {
            let mut n = i + 1;
            while seen.contains(&format!("rule-{n}")) {
                n += 1;
            }
            let id = format!("rule-{n}");
            seen.push(id.clone());
            r.id = Some(id);
        }
    }
    Ok(())
}

/// What is known when a stage is evaluated.
#[derive(Clone, Copy, Debug)]
pub struct ChaosEvent<'a> {
    pub stage: ChaosStage,
    pub mail_from: Option<&'a str>,
    pub rcpt_to: &'a [String],
    pub auth_user: Option<&'a str>,
    pub provider: Option<&'a str>,
}

impl<'a> ChaosEvent<'a> {
    pub fn new(stage: ChaosStage) -> Self {
        Self {
            stage,
            mail_from: None,
            rcpt_to: &[],
            auth_user: None,
            provider: None,
        }
    }
}

/// What a firing rule asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChaosEffect {
    /// Only the delay; then continue normally.
    Continue,
    /// Reply with this line (`code enhanced text`).
    Reply {
        code: u16,
        enhanced: String,
        text: String,
    },
    Disconnect,
    /// Stage `provider`: the error name to answer in the provider's format.
    ProviderError(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChaosDecision {
    pub rule_id: String,
    /// The area whose rule fired.
    pub area: Option<String>,
    pub delay: Option<Duration>,
    pub effect: ChaosEffect,
}

struct CompiledRule {
    rule: MailChaosRule,
    checks: Vec<(MatchField, CompiledString)>,
    delay: Option<Duration>,
}

impl CompiledRule {
    fn new(rule: MailChaosRule) -> Result<Self, String> {
        let mut checks = Vec::new();
        for (f, m) in rule.r#match.fields() {
            if let Some(m) = m {
                checks.push((f, CompiledString::compile(m).map_err(|e| e.to_string())?));
            }
        }
        let delay = rule.action.delay.as_deref().map(parse_delay).transpose()?;
        Ok(Self {
            rule,
            checks,
            delay,
        })
    }

    fn matches(&self, e: &ChaosEvent<'_>) -> bool {
        if self.rule.stage != e.stage {
            return false;
        }
        self.checks.iter().all(|(f, m)| match f {
            MatchField::RcptTo if e.rcpt_to.is_empty() => m.matches(None),
            MatchField::RcptTo => e.rcpt_to.iter().any(|r| m.matches(Some(r))),
            MatchField::MailFrom => m.matches(e.mail_from),
            MatchField::AuthUser => m.matches(e.auth_user),
            MatchField::Provider => m.matches(e.provider),
        })
    }

    fn decision(&self, area: Option<&str>) -> ChaosDecision {
        let id = self.rule.id().to_string();
        let a = &self.rule.action;
        let effect = if let Some(r) = &a.reply {
            ChaosEffect::Reply {
                code: r.code,
                enhanced: r
                    .enhanced
                    .clone()
                    .unwrap_or_else(|| format!("{}.0.0", r.code / 100)),
                text: r
                    .text
                    .clone()
                    .unwrap_or_else(|| format!("FastMock chaos rule {id}")),
            }
        } else if a.disconnect {
            ChaosEffect::Disconnect
        } else if let Some(p) = &a.provider_error {
            ChaosEffect::ProviderError(p.clone())
        } else {
            ChaosEffect::Continue
        };
        ChaosDecision {
            rule_id: id,
            area: area.map(String::from),
            delay: self.delay,
            effect,
        }
    }
}

struct CompiledSet {
    version: String,
    rules: Vec<CompiledRule>,
}

type Cache = HashMap<Option<String>, (Instant, Arc<CompiledSet>)>;

/// A seeded decision: the same seed and event number always give the same answer.
pub fn seeded_fires(seed: u64, n: u64, probability: f64) -> bool {
    let mut z = seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    ((z % 10_000) as f64) < probability * 100.0
}

#[derive(Debug, thiserror::Error)]
pub enum ChaosError {
    #[error("{0}")]
    Invalid(String),
    #[error("mail chaos storage failed: {0:#}")]
    Store(anyhow::Error),
}

/// Rules, counters and evaluation; one per server, shared by the SMTP listener, the admin API and the
/// provider APIs.
pub struct MailChaos {
    backend: Arc<dyn ChaosBackend>,
    cache: Mutex<Cache>,
    cache_ttl: Duration,
}

impl MailChaos {
    pub fn new(backend: Arc<dyn ChaosBackend>) -> Self {
        Self::with_cache_ttl(backend, CACHE_TTL)
    }

    pub fn with_cache_ttl(backend: Arc<dyn ChaosBackend>, cache_ttl: Duration) -> Self {
        Self {
            backend,
            cache: Mutex::new(HashMap::new()),
            cache_ttl,
        }
    }

    fn invalidate(&self, area: Option<&str>) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&area.map(String::from));
    }

    /// The stored rules of an area (empty when none).
    pub async fn get(&self, area: Option<&str>) -> anyhow::Result<StoredChaos> {
        Ok(self.backend.load(area).await?.unwrap_or_default())
    }

    /// The counters of each rule.
    pub async fn counters(
        &self,
        area: Option<&str>,
        chaos: &StoredChaos,
    ) -> anyhow::Result<Vec<ChaosCounters>> {
        let mut out = Vec::with_capacity(chaos.rules.len());
        for r in &chaos.rules {
            let matched = counter_key(area, &chaos.version, r.id(), "matched");
            let fired = counter_key(area, &chaos.version, r.id(), "fired");
            out.push(ChaosCounters {
                id: r.id().to_string(),
                matched: self.backend.counter(&matched).await?.max(0) as u64,
                fired: self.backend.counter(&fired).await?.max(0) as u64,
            });
        }
        Ok(out)
    }

    /// Validates and stores the rules of an area; the counters start from zero.
    pub async fn replace(
        &self,
        area: Option<&str>,
        mut rules: Vec<MailChaosRule>,
    ) -> Result<StoredChaos, ChaosError> {
        validate(&mut rules).map_err(ChaosError::Invalid)?;
        let chaos = StoredChaos {
            version: uuid::Uuid::now_v7().simple().to_string(),
            rules,
        };
        self.backend
            .save(area, &chaos)
            .await
            .map_err(ChaosError::Store)?;
        if let Err(e) = self.backend.clear_counters(&counter_prefix(area)).await {
            tracing::warn!(error = %e, "clearing old mail chaos counters failed");
        }
        self.invalidate(area);
        Ok(chaos)
    }

    /// Removes the rules of an area and their counters; returns whether there were rules.
    pub async fn clear(&self, area: Option<&str>) -> anyhow::Result<bool> {
        let had = self.backend.remove(area).await?;
        if let Err(e) = self.backend.clear_counters(&counter_prefix(area)).await {
            tracing::warn!(error = %e, "clearing mail chaos counters failed");
        }
        self.invalidate(area);
        Ok(had)
    }

    async fn compiled(&self, area: Option<&str>) -> Arc<CompiledSet> {
        let key = area.map(String::from);
        let cached = {
            let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            c.get(&key).cloned()
        };
        if let Some((at, set)) = &cached
            && at.elapsed() < self.cache_ttl
        {
            return set.clone();
        }
        let set = match self.backend.load(area).await {
            Ok(stored) => {
                let stored = stored.unwrap_or_default();
                let mut rules = Vec::with_capacity(stored.rules.len());
                for r in stored.rules {
                    match CompiledRule::new(r) {
                        Ok(c) => rules.push(c),
                        Err(e) => {
                            tracing::error!(error = %e, space = area, "stored mail chaos rule is not valid; skipping it")
                        }
                    }
                }
                Arc::new(CompiledSet {
                    version: stored.version,
                    rules,
                })
            }
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "mail_chaos")
                    .increment(1);
                tracing::warn!(error = %e, space = area, "reading mail chaos rules failed; keeping the previous set");
                cached.map(|(_, s)| s).unwrap_or_else(|| {
                    Arc::new(CompiledSet {
                        version: String::new(),
                        rules: vec![],
                    })
                })
            }
        };
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (Instant::now(), set.clone()));
        set
    }

    async fn count(&self, key: String) -> Option<u64> {
        match self.backend.incr(&key, COUNTER_TTL).await {
            Ok(n) => Some(n.max(0) as u64),
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "mail_chaos")
                    .increment(1);
                tracing::warn!(error = %e, "counting a mail chaos rule failed; the rule does not fire");
                None
            }
        }
    }

    /// Evaluates the rules of one area for an event: the first rule whose stage and match hold and that
    /// fires (after `every`, `probability` and `times`) decides. Storage errors never fire a rule.
    pub async fn evaluate(
        &self,
        area: Option<&str>,
        event: &ChaosEvent<'_>,
    ) -> Option<ChaosDecision> {
        let set = self.compiled(area).await;
        for r in set.rules.iter().filter(|r| r.matches(event)) {
            let id = r.rule.id();
            let Some(n) = self
                .count(counter_key(area, &set.version, id, "matched"))
                .await
            else {
                continue;
            };
            if r.rule.every.is_some_and(|e| n % e != 0) {
                continue;
            }
            if let Some(p) = r.rule.probability {
                let fires = match r.rule.seed {
                    Some(seed) => seeded_fires(seed, n, p),
                    None => rand::random::<f64>() * 100.0 < p,
                };
                if !fires {
                    continue;
                }
            }
            let Some(fired) = self
                .count(counter_key(area, &set.version, id, "fired"))
                .await
            else {
                continue;
            };
            if r.rule.times.is_some_and(|t| fired > t) {
                continue;
            }
            metrics::counter!("fastmock_mail_chaos_injections_total", "stage" => event.stage.as_str())
                .increment(1);
            tracing::info!(
                stage = event.stage.as_str(),
                rule = id,
                space = area,
                "mail chaos rule fired"
            );
            return Some(r.decision(area));
        }
        None
    }

    /// Evaluates the areas in order; the first decision wins.
    pub async fn evaluate_areas(
        &self,
        areas: &[Option<String>],
        event: &ChaosEvent<'_>,
    ) -> Option<ChaosDecision> {
        for a in areas {
            if let Some(d) = self.evaluate(a.as_deref(), event).await {
                return Some(d);
            }
        }
        None
    }

    /// The provider-stage decision for a provider API call into `area` (phase C3): `None` means
    /// continue normally; otherwise wait `delay`, then answer `ProviderError(name)` in the provider's
    /// format, or continue for `Continue`.
    pub async fn evaluate_provider(
        &self,
        area: Option<&str>,
        provider: &str,
        mail_from: &str,
        rcpt_to: &[String],
    ) -> Option<ChaosDecision> {
        let e = ChaosEvent {
            stage: ChaosStage::Provider,
            mail_from: Some(mail_from),
            rcpt_to,
            auth_user: None,
            provider: Some(provider),
        };
        self.evaluate(area, &e).await
    }
}

/// [`ChaosBackend`] in this process only (tests, fuzzing).
#[derive(Default)]
pub struct MemoryChaosBackend {
    rules: Mutex<HashMap<Option<String>, StoredChaos>>,
    counters: Mutex<HashMap<String, i64>>,
}

#[async_trait]
impl ChaosBackend for MemoryChaosBackend {
    async fn load(&self, area: Option<&str>) -> anyhow::Result<Option<StoredChaos>> {
        Ok(self
            .rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&area.map(String::from))
            .cloned())
    }

    async fn save(&self, area: Option<&str>, chaos: &StoredChaos) -> anyhow::Result<()> {
        self.rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(area.map(String::from), chaos.clone());
        Ok(())
    }

    async fn remove(&self, area: Option<&str>) -> anyhow::Result<bool> {
        Ok(self
            .rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&area.map(String::from))
            .is_some())
    }

    async fn incr(&self, key: &str, _ttl: Duration) -> anyhow::Result<i64> {
        let mut c = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        let v = c.entry(key.to_string()).or_insert(0);
        *v += 1;
        Ok(*v)
    }

    async fn counter(&self, key: &str) -> anyhow::Result<i64> {
        Ok(*self
            .counters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .unwrap_or(&0))
    }

    async fn clear_counters(&self, prefix: &str) -> anyhow::Result<()> {
        self.counters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| !k.starts_with(prefix));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(v: serde_json::Value) -> Vec<MailChaosRule> {
        serde_json::from_value(v).unwrap()
    }

    fn chaos() -> MailChaos {
        MailChaos::with_cache_ttl(Arc::new(MemoryChaosBackend::default()), Duration::ZERO)
    }

    fn bad(v: serde_json::Value) -> String {
        validate(&mut rules(v)).unwrap_err()
    }

    #[test]
    fn validation() {
        let mut r = rules(json!([
            {"stage": "rcpt", "action": {"reply": {"code": 451}}},
            {"id": "rule-1", "stage": "data", "action": {"delay": "2s"}},
            {"stage": "connect", "action": {"disconnect": true}}
        ]));
        validate(&mut r).unwrap();
        let ids: Vec<&str> = r.iter().map(|r| r.id()).collect();
        assert_eq!(ids, vec!["rule-2", "rule-1", "rule-3"]);
        assert!(bad(json!([{"stage": "mail", "match": {"rcptTo": {"equalTo": "a"}}, "action": {"disconnect": true}}])).contains("match.rcptTo does not apply at stage mail"));
        assert!(bad(json!([{"stage": "ehlo", "match": {"authUser": {"equalTo": "a"}}, "action": {"disconnect": true}}])).contains("authUser"));
        assert!(bad(json!([{"stage": "rcpt", "match": {"rcptTo": {"matches": "("}}, "action": {"disconnect": true}}])).contains("match.rcptTo"));
        assert!(bad(json!([{"stage": "rcpt", "action": {}}])).contains("needs reply"));
        assert!(
            bad(json!([{"stage": "rcpt", "action": {"reply": {"code": 250}}}]))
                .contains("4xx or 5xx")
        );
        assert!(
            bad(
                json!([{"stage": "rcpt", "action": {"reply": {"code": 451, "enhanced": "5.0.0"}}}])
            )
            .contains("must start with 4")
        );
        assert!(
            bad(
                json!([{"stage": "rcpt", "action": {"reply": {"code": 451, "enhanced": "4.x.0"}}}])
            )
            .contains("like 4.3.0")
        );
        assert!(
            bad(json!([{"stage": "rcpt", "action": {"reply": {"code": 451, "text": "a\r\nb"}}}]))
                .contains("one line")
        );
        assert!(
            bad(json!([{"stage": "rcpt", "action": {"reply": {"code": 451}, "disconnect": true}}]))
                .contains("one of")
        );
        assert!(bad(json!([{"stage": "rcpt", "action": {"delay": "1h"}}])).contains("500ms"));
        assert!(bad(json!([{"stage": "rcpt", "action": {"delay": "11m"}}])).contains("10 minutes"));
        assert!(
            bad(json!([{"stage": "rcpt", "action": {"providerError": "rateLimit"}}]))
                .contains("only at stage provider")
        );
        assert!(
            bad(json!([{"stage": "provider", "action": {"reply": {"code": 451}}}]))
                .contains("not reply")
        );
        assert!(
            bad(json!([{"stage": "provider", "action": {"providerError": "a b"}}]))
                .contains("providerError")
        );
        assert!(
            bad(json!([{"stage": "rcpt", "times": 0, "action": {"disconnect": true}}]))
                .contains("times")
        );
        assert!(
            bad(json!([{"stage": "rcpt", "every": 0, "action": {"disconnect": true}}]))
                .contains("every")
        );
        assert!(
            bad(json!([{"stage": "rcpt", "probability": 101, "action": {"disconnect": true}}]))
                .contains("probability")
        );
        assert!(
            bad(json!([{"stage": "rcpt", "seed": 1, "action": {"disconnect": true}}]))
                .contains("seed needs")
        );
        assert!(
            bad(json!([{"id": "a b", "stage": "rcpt", "action": {"disconnect": true}}]))
                .contains("id `a b`")
        );
        assert!(bad(json!([{"id": "x", "stage": "rcpt", "action": {"disconnect": true}}, {"id": "x", "stage": "rcpt", "action": {"disconnect": true}}])).contains("twice"));
        let many: Vec<_> = (0..101)
            .map(|_| json!({"stage": "rcpt", "action": {"disconnect": true}}))
            .collect();
        assert!(bad(json!(many)).contains("at most 100"));
        assert!(
            serde_json::from_value::<Vec<MailChaosRule>>(json!([{"stage": "smtp", "action": {}}]))
                .is_err()
        );
        assert!(
            serde_json::from_value::<Vec<MailChaosRule>>(
                json!([{"stage": "rcpt", "action": {}, "x": 1}])
            )
            .is_err()
        );
        validate(&mut rules(json!([{"stage": "provider", "match": {"provider": {"equalTo": "ses"}, "rcptTo": {"contains": "@"}}, "action": {"providerError": "MessageRejected", "delay": "100ms"}}]))).unwrap();
        assert_eq!(parse_delay("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_delay("3"), Ok(Duration::from_secs(3)));
        assert_eq!(parse_delay("1m"), Ok(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn first_firing_rule_wins_with_counters() {
        let c = chaos();
        let stored = c
            .replace(
                Some("billing"),
                rules(json!([
                    {"id": "third", "stage": "data", "every": 3, "times": 1, "action": {"reply": {"code": 451, "enhanced": "4.3.0", "text": "Try later"}}},
                    {"id": "vip", "stage": "rcpt", "match": {"rcptTo": {"contains": "vip@"}}, "action": {"reply": {"code": 550}}},
                    {"id": "slow", "stage": "rcpt", "action": {"delay": "10ms"}}
                ])),
            )
            .await
            .unwrap();
        let data = ChaosEvent::new(ChaosStage::Data);
        let mut fired = vec![];
        for _ in 0..7 {
            fired.push(c.evaluate(Some("billing"), &data).await.is_some());
        }
        assert_eq!(fired, vec![false, false, true, false, false, false, false]);
        let d = {
            let mut e = data;
            e.stage = ChaosStage::Data;
            c.evaluate(None, &e).await
        };
        assert_eq!(d, None, "rules belong to their area");
        let rcpt = vec!["vip@billing.example.test".to_string()];
        let e = ChaosEvent {
            rcpt_to: &rcpt,
            ..ChaosEvent::new(ChaosStage::Rcpt)
        };
        let d = c.evaluate(Some("billing"), &e).await.unwrap();
        assert_eq!(d.rule_id, "vip");
        assert_eq!(
            d.effect,
            ChaosEffect::Reply {
                code: 550,
                enhanced: "5.0.0".into(),
                text: "FastMock chaos rule vip".into()
            }
        );
        let other = vec!["a@billing.example.test".to_string()];
        let e = ChaosEvent {
            rcpt_to: &other,
            ..ChaosEvent::new(ChaosStage::Rcpt)
        };
        let d = c.evaluate(Some("billing"), &e).await.unwrap();
        assert_eq!(d.rule_id, "slow");
        assert_eq!(d.effect, ChaosEffect::Continue);
        assert_eq!(d.delay, Some(Duration::from_millis(10)));
        let counters = c.counters(Some("billing"), &stored).await.unwrap();
        assert_eq!(
            counters[0],
            ChaosCounters {
                id: "third".into(),
                matched: 7,
                fired: 2
            }
        );
        assert_eq!((counters[1].matched, counters[1].fired), (1, 1));
        assert_eq!((counters[2].matched, counters[2].fired), (1, 1));

        let again = c
            .replace(Some("billing"), stored.rules.clone())
            .await
            .unwrap();
        assert_ne!(again.version, stored.version);
        assert_eq!(
            c.counters(Some("billing"), &again).await.unwrap()[0].matched,
            0
        );
        assert!(c.clear(Some("billing")).await.unwrap());
        assert!(!c.clear(Some("billing")).await.unwrap());
        assert_eq!(c.evaluate(Some("billing"), &data).await, None);
        assert!(c.get(Some("billing")).await.unwrap().rules.is_empty());
    }

    #[tokio::test]
    async fn seeded_probability_repeats() {
        let run = || async {
            let c = chaos();
            c.replace(
                None,
                rules(json!([{"stage": "mail", "probability": 30, "seed": 42, "action": {"disconnect": true}}])),
            )
            .await
            .unwrap();
            let mut v = vec![];
            for _ in 0..200 {
                v.push(
                    c.evaluate(None, &ChaosEvent::new(ChaosStage::Mail))
                        .await
                        .is_some(),
                );
            }
            v
        };
        let a = run().await;
        assert_eq!(a, run().await);
        let n = a.iter().filter(|x| **x).count();
        assert!((30..=90).contains(&n), "{n}");
        assert!((0..100).all(|i| !seeded_fires(1, i, 0.0)));
        assert!((0..100).all(|i| seeded_fires(1, i, 100.0)));
    }

    #[tokio::test]
    async fn provider_stage_and_matchers() {
        let c = chaos();
        c.replace(
            Some("crm"),
            rules(json!([
                {"stage": "provider", "match": {"provider": {"equalTo": "ses"}}, "action": {"providerError": "MessageRejected"}},
                {"stage": "auth", "match": {"authUser": {"absent": true}}, "action": {"disconnect": true}},
                {"stage": "mail", "match": {"mailFrom": {"equalTo": "bad@example.com"}, "authUser": {"equalTo": "app"}}, "action": {"reply": {"code": 421}}}
            ])),
        )
        .await
        .unwrap();
        let d = c
            .evaluate_provider(Some("crm"), "ses", "a@example.com", &[])
            .await
            .unwrap();
        assert_eq!(
            d.effect,
            ChaosEffect::ProviderError("MessageRejected".into())
        );
        assert_eq!(d.area.as_deref(), Some("crm"));
        assert!(
            c.evaluate_provider(Some("crm"), "sendgrid", "a@example.com", &[])
                .await
                .is_none()
        );
        let e = ChaosEvent {
            auth_user: Some("app"),
            ..ChaosEvent::new(ChaosStage::Auth)
        };
        assert!(c.evaluate(Some("crm"), &e).await.is_none());
        assert!(
            c.evaluate(Some("crm"), &ChaosEvent::new(ChaosStage::Auth))
                .await
                .is_some()
        );
        let e = ChaosEvent {
            auth_user: Some("app"),
            mail_from: Some("bad@example.com"),
            ..ChaosEvent::new(ChaosStage::Mail)
        };
        let areas = vec![None, Some("crm".to_string())];
        let d = c.evaluate_areas(&areas, &e).await.unwrap();
        assert_eq!(d.area.as_deref(), Some("crm"));
        assert!(matches!(d.effect, ChaosEffect::Reply { code: 421, .. }));
    }
}
