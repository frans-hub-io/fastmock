use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::bail;
use axum::http::Method;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    StubsRead,
    StubsWrite,
    StubsPurge,
    ImportsRun,
    JournalRead,
    JournalDelete,
    FilesRead,
    FilesWrite,
    RecordingsRead,
    RecordingsWrite,
    CallbacksRead,
    CallbacksDelete,
    StateRead,
    StateWrite,
    ChaosRead,
    ChaosWrite,
    GrpcRead,
    GrpcWrite,
    ContractsRead,
    ContractsDetails,
    ContractsWrite,
    ContractsSecrets,
    InfoRead,
    AuditRead,
    SystemReset,
    TokensRead,
    TokensWrite,
    AccessRead,
    MailRead,
    MailWrite,
    MailDelete,
    MessagesRead,
    MessagesWrite,
    MessagesDelete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Global,
    Space,
}

use Permission::*;

pub const ALL_PERMISSIONS: [Permission; 34] = [
    StubsRead,
    StubsWrite,
    StubsPurge,
    ImportsRun,
    JournalRead,
    JournalDelete,
    FilesRead,
    FilesWrite,
    RecordingsRead,
    RecordingsWrite,
    CallbacksRead,
    CallbacksDelete,
    StateRead,
    StateWrite,
    ChaosRead,
    ChaosWrite,
    GrpcRead,
    GrpcWrite,
    ContractsRead,
    ContractsDetails,
    ContractsWrite,
    ContractsSecrets,
    InfoRead,
    AuditRead,
    SystemReset,
    TokensRead,
    TokensWrite,
    AccessRead,
    MailRead,
    MailWrite,
    MailDelete,
    MessagesRead,
    MessagesWrite,
    MessagesDelete,
];

impl Permission {
    pub fn name(self) -> &'static str {
        match self {
            StubsRead => "stubs:read",
            StubsWrite => "stubs:write",
            StubsPurge => "stubs:purge",
            ImportsRun => "imports:run",
            JournalRead => "journal:read",
            JournalDelete => "journal:delete",
            FilesRead => "files:read",
            FilesWrite => "files:write",
            RecordingsRead => "recordings:read",
            RecordingsWrite => "recordings:write",
            CallbacksRead => "callbacks:read",
            CallbacksDelete => "callbacks:delete",
            StateRead => "state:read",
            StateWrite => "state:write",
            ChaosRead => "chaos:read",
            ChaosWrite => "chaos:write",
            GrpcRead => "grpc:read",
            GrpcWrite => "grpc:write",
            ContractsRead => "contracts:read",
            ContractsDetails => "contracts:details",
            ContractsWrite => "contracts:write",
            ContractsSecrets => "contracts:secrets",
            InfoRead => "info:read",
            AuditRead => "audit:read",
            SystemReset => "system:reset",
            TokensRead => "tokens:read",
            TokensWrite => "tokens:write",
            AccessRead => "access:read",
            MailRead => "mail:read",
            MailWrite => "mail:write",
            MailDelete => "mail:delete",
            MessagesRead => "messages:read",
            MessagesWrite => "messages:write",
            MessagesDelete => "messages:delete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        ALL_PERMISSIONS.into_iter().find(|p| p.name() == s)
    }

    pub fn resource(self) -> &'static str {
        self.name().split(':').next().unwrap_or_default()
    }

    /// Whether a binding limited to spaces can grant this permission.
    pub fn scope(self) -> ScopeKind {
        match self {
            StubsPurge | RecordingsRead | RecordingsWrite | ChaosRead | ChaosWrite | GrpcRead
            | GrpcWrite | ContractsSecrets | InfoRead | AuditRead | SystemReset | AccessRead => {
                ScopeKind::Global
            }
            _ => ScopeKind::Space,
        }
    }

    fn bit(self) -> u64 {
        1u64 << (self as u8)
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PermSet(u64);

impl PermSet {
    pub fn of(perms: &[Permission]) -> Self {
        let mut s = Self::default();
        for p in perms {
            s.insert(*p);
        }
        s
    }

    pub fn all() -> Self {
        Self::of(&ALL_PERMISSIONS)
    }

    pub fn insert(&mut self, p: Permission) {
        self.0 |= p.bit();
    }

    pub fn contains(self, p: Permission) -> bool {
        self.0 & p.bit() != 0
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn includes(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn space_scoped(self) -> Self {
        let mut out = Self::default();
        for p in self.iter() {
            if p.scope() == ScopeKind::Space {
                out.insert(p);
            }
        }
        out
    }

    pub fn iter(self) -> impl Iterator<Item = Permission> {
        ALL_PERMISSIONS
            .into_iter()
            .filter(move |p| self.contains(*p))
    }

    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(Permission::name).collect()
    }
}

pub const BUILTIN_ROLES: [&str; 3] = ["viewer", "editor", "admin"];

pub fn viewer_permissions() -> PermSet {
    PermSet::of(&[
        StubsRead,
        JournalRead,
        FilesRead,
        RecordingsRead,
        CallbacksRead,
        StateRead,
        ChaosRead,
        GrpcRead,
        ContractsRead,
        InfoRead,
        MailRead,
        MessagesRead,
    ])
}

pub fn editor_permissions() -> PermSet {
    viewer_permissions().union(PermSet::of(&[
        StubsWrite,
        ImportsRun,
        JournalDelete,
        FilesWrite,
        RecordingsWrite,
        CallbacksDelete,
        StateWrite,
        ChaosWrite,
        GrpcWrite,
        ContractsDetails,
        ContractsWrite,
        MailWrite,
        MailDelete,
        MessagesWrite,
        MessagesDelete,
    ]))
}

pub fn builtin_role(name: &str) -> Option<PermSet> {
    match name {
        "viewer" => Some(viewer_permissions()),
        "editor" => Some(editor_permissions()),
        "admin" => Some(PermSet::all()),
        _ => None,
    }
}

/// The lowest built-in role holding the permission.
pub fn minimal_role(p: Permission) -> &'static str {
    if viewer_permissions().contains(p) {
        "viewer"
    } else if editor_permissions().contains(p) {
        "editor"
    } else {
        "admin"
    }
}

pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpaceDef {
    #[schema(example = "payments")]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Data-plane path prefixes the space's stubs must stay under (empty: no restriction).
    pub path_prefixes: Vec<String>,
    /// Hosts the space's stubs must match (empty: no restriction).
    pub hosts: Vec<String>,
    /// Mail domains whose messages go to the space (lowercase; `*.domain` for subdomains).
    pub mail_domains: Vec<String>,
    /// Capture allow-lists of the space; they narrow the server-wide `FASTMOCK_CAPTURE_*` lists.
    #[serde(skip_serializing_if = "SpaceCapture::is_empty")]
    pub capture: SpaceCapture,
}

/// Allow-lists of what capture sources of one space may read. Each list narrows the server-wide list of
/// its protocol (a destination must be allowed by both); an absent list leaves the server-wide one as it
/// is, an empty list allows nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SpaceCapture {
    /// Kafka topics: names, `prefix*`, or `*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(["payments.*"]))]
    pub kafka_topics: Option<Vec<String>>,
    /// MQTT topic filters: names, `prefix*`, or `*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mqtt_topics: Option<Vec<String>>,
    /// AMQP exchanges: names, `prefix*`, or `*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amqp_exchanges: Option<Vec<String>>,
}

/// Entries of one capture allow-list of a space, at most.
pub const MAX_SPACE_CAPTURE_ENTRIES: usize = 100;

impl SpaceCapture {
    pub fn is_empty(&self) -> bool {
        self.kafka_topics.is_none() && self.mqtt_topics.is_none() && self.amqp_exchanges.is_none()
    }

    /// The lists with the access-file field names.
    pub fn lists(&self) -> [(&'static str, Option<&[String]>); 3] {
        [
            ("kafkaTopics", self.kafka_topics.as_deref()),
            ("mqttTopics", self.mqtt_topics.as_deref()),
            ("amqpExchanges", self.amqp_exchanges.as_deref()),
        ]
    }

    fn check(&self) -> anyhow::Result<()> {
        for (field, list) in self.lists() {
            let Some(list) = list else { continue };
            if list.len() > MAX_SPACE_CAPTURE_ENTRIES {
                bail!("capture.{field}: at most {MAX_SPACE_CAPTURE_ENTRIES} entries");
            }
            for e in list {
                let star = e.find('*');
                if e.is_empty()
                    || e.len() > 255
                    || e.chars().any(|c| c.is_control() || c.is_whitespace())
                    || star.is_some_and(|i| i != e.len() - 1)
                {
                    bail!(
                        "capture.{field}: `{e}` is not a name, prefix* or *: 1 to 255 characters without spaces, `*` only at the end"
                    );
                }
            }
        }
        Ok(())
    }
}
/// The `spaces` entry that stands for every space of the access file, current and future.
pub const ALL_SPACES: &str = "*";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingScope {
    Global,
    Spaces(Vec<String>),
    /// `spaces: ["*"]`: every space configured when the request is checked.
    AllSpaces,
}

impl BindingScope {
    /// `spaces` as written: `["*"]` for every space, empty for a global binding.
    pub fn space_list(&self) -> Vec<String> {
        match self {
            BindingScope::Global => vec![],
            BindingScope::Spaces(l) => l.clone(),
            BindingScope::AllSpaces => vec![ALL_SPACES.to_string()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub role: String,
    pub permissions: PermSet,
    pub scope: BindingScope,
}

impl Binding {
    pub fn global(role: &str, permissions: PermSet) -> Self {
        Self {
            role: role.to_string(),
            permissions,
            scope: BindingScope::Global,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    Token(String),
    User(String),
    Group(String),
}

impl Subject {
    fn parse(s: &str) -> anyhow::Result<Self> {
        let (kind, name) = s.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("subject must be token:<name>, user:<name> or group:<name>")
        })?;
        let name = name.trim();
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            bail!("subject name must be 1..256 printable characters");
        }
        Ok(match kind.trim() {
            "token" => Subject::Token(name.to_string()),
            "user" => Subject::User(name.to_string()),
            "group" => Subject::Group(name.to_string()),
            other => bail!("unknown subject kind `{other}`: use token, user or group"),
        })
    }
}

impl std::fmt::Display for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Subject::Token(n) => write!(f, "token:{n}"),
            Subject::User(n) => write!(f, "user:{n}"),
            Subject::Group(n) => write!(f, "group:{n}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum PrincipalKind {
    /// Static token from `FASTMOCK_ADMIN_TOKENS` or `FASTMOCK_ADMIN_TOKENS_FILE`.
    Token,
    /// Token managed through the admin API.
    ApiToken,
    /// OIDC user.
    User,
    /// Authentication is disabled (`FASTMOCK_AUTH_DISABLED=true`).
    Anonymous,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::Token => "token",
            PrincipalKind::ApiToken => "apiToken",
            PrincipalKind::User => "user",
            PrincipalKind::Anonymous => "anonymous",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessDoc {
    #[serde(default)]
    roles: Vec<RoleDoc>,
    #[serde(default)]
    spaces: Vec<SpaceDoc>,
    #[serde(default)]
    bindings: Vec<BindingDoc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleDoc {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    permissions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SpaceDoc {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    path_prefixes: Vec<String>,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    mail_domains: Vec<String>,
    #[serde(default)]
    capture: SpaceCapture,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingDoc {
    subject: String,
    role: String,
    #[serde(default)]
    spaces: Option<Vec<String>>,
    #[serde(default)]
    global: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct AccessConfig {
    roles: BTreeMap<String, PermSet>,
    /// Permissions each custom role names one by one (not through a `resource:*` wildcard).
    explicit: BTreeMap<String, PermSet>,
    spaces: BTreeMap<String, SpaceDef>,
    bindings: Vec<(Subject, Binding)>,
    /// `access file <path>`: the prefix of start-up errors about the file.
    origin: Option<String>,
}

/// The permissions of a role and those of them it names explicitly.
fn parse_permissions(list: &[String]) -> anyhow::Result<(PermSet, PermSet)> {
    if list.is_empty() {
        bail!("permissions must not be empty");
    }
    let mut set = PermSet::default();
    let mut explicit = PermSet::default();
    for raw in list {
        let p = raw.trim();
        if p == "*" {
            set = set.union(PermSet::all());
        } else if let Some(resource) = p.strip_suffix(":*") {
            let matching: Vec<Permission> = ALL_PERMISSIONS
                .into_iter()
                .filter(|x| x.resource() == resource)
                .collect();
            if matching.is_empty() {
                bail!("unknown permission `{p}`");
            }
            for m in matching {
                set.insert(m);
            }
        } else {
            let one =
                Permission::parse(p).ok_or_else(|| anyhow::anyhow!("unknown permission `{p}`"))?;
            set.insert(one);
            explicit.insert(one);
        }
    }
    Ok((set, explicit))
}

fn check_prefix(p: &str) -> anyhow::Result<()> {
    if !p.starts_with('/')
        || p.len() > 512
        || p.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("path prefix `{p}` must start with `/`, have no spaces and be at most 512 bytes");
    }
    if p.contains('?')
        || p.contains('#')
        || fastmock_core::ownership::has_dot_segment(p)
        || fastmock_core::ownership::has_encoded_separator(p)
    {
        bail!(
            "path prefix `{p}` must be a plain path, without `.` or `..` segments, encoded slashes or backslashes"
        );
    }
    Ok(())
}

pub(crate) fn check_host(h: &str) -> anyhow::Result<String> {
    let host = h.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        bail!("host `{h}` must be a host name, optionally with a port");
    }
    Ok(host)
}

impl AccessConfig {
    /// Reads and checks the access file; errors are one line that names the file and the entry.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading the access file {}: {e}", path.display()))?;
        let origin = format!("access file {}", path.display());
        let mut cfg = Self::parse(&text).map_err(|e| anyhow::anyhow!("{origin}: {e:#}"))?;
        cfg.origin = Some(origin);
        Ok(cfg)
    }

    /// `access file <path>` (or `access file` for a document not read from a file), for error messages.
    pub fn origin(&self) -> &str {
        self.origin.as_deref().unwrap_or("access file")
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let doc: AccessDoc = if text.trim().is_empty() {
            AccessDoc {
                roles: vec![],
                spaces: vec![],
                bindings: vec![],
            }
        } else {
            serde_yaml_ng::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))?
        };
        let mut cfg = AccessConfig::default();
        for (i, r) in doc.roles.iter().enumerate() {
            let at = format!("roles[{i}] ({})", r.name);
            if !valid_name(&r.name) {
                bail!("{at}: role names must match [a-z0-9]([a-z0-9-]{{0,61}}[a-z0-9])?");
            }
            if BUILTIN_ROLES.contains(&r.name.as_str()) {
                bail!("{at}: the built-in roles viewer, editor and admin cannot be redefined");
            }
            if cfg.roles.contains_key(&r.name) {
                bail!("{at}: role defined twice");
            }
            let (perms, explicit) =
                parse_permissions(&r.permissions).map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            cfg.roles.insert(r.name.clone(), perms);
            cfg.explicit.insert(r.name.clone(), explicit);
        }
        for (i, s) in doc.spaces.into_iter().enumerate() {
            let at = format!("spaces[{i}] ({})", s.name);
            if !valid_name(&s.name) {
                bail!("{at}: space names must match [a-z0-9]([a-z0-9-]{{0,61}}[a-z0-9])?");
            }
            if cfg.spaces.contains_key(&s.name) {
                bail!("{at}: space defined twice");
            }
            if s.description.as_ref().is_some_and(|d| d.len() > 1024) {
                bail!("{at}: description is limited to 1024 bytes");
            }
            for p in &s.path_prefixes {
                check_prefix(p).map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            }
            let hosts = s
                .hosts
                .iter()
                .map(|h| check_host(h))
                .collect::<anyhow::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            s.capture
                .check()
                .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            let mut mail_domains: Vec<String> = Vec::new();
            for d in &s.mail_domains {
                let d = fastmock_mail::routing::check_domain(d)
                    .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
                if !mail_domains.contains(&d) {
                    mail_domains.push(d);
                }
            }
            cfg.spaces.insert(
                s.name.clone(),
                SpaceDef {
                    name: s.name,
                    description: s.description,
                    path_prefixes: s.path_prefixes,
                    hosts,
                    mail_domains,
                    capture: s.capture,
                },
            );
        }
        cfg.check_ownership()?;
        for (i, b) in doc.bindings.into_iter().enumerate() {
            let at = format!("bindings[{i}] ({})", b.subject);
            let subject = Subject::parse(&b.subject).map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            let perms = cfg
                .role(&b.role)
                .ok_or_else(|| anyhow::anyhow!("{at}: unknown role `{}`", b.role))?;
            let scope = match (b.global.unwrap_or(false), b.spaces) {
                (true, None) => BindingScope::Global,
                (false, Some(list)) if !list.is_empty() => {
                    let all = list.iter().any(|s| s == ALL_SPACES);
                    if all && list.len() != 1 {
                        bail!("{at}: \"*\" must be the only entry in spaces");
                    }
                    if all && cfg.spaces.is_empty() {
                        bail!("{at}: spaces: [\"*\"] needs at least one space in the file");
                    }
                    let mut seen = BTreeSet::new();
                    for s in list.iter().filter(|_| !all) {
                        if !cfg.spaces.contains_key(s) {
                            bail!("{at}: unknown space `{s}`");
                        }
                        if !seen.insert(s.clone()) {
                            bail!("{at}: space `{s}` listed twice");
                        }
                    }
                    if cfg
                        .explicit
                        .get(&b.role)
                        .is_some_and(|e| e.contains(ContractsSecrets))
                    {
                        bail!(
                            "{at}: role `{}` lists contracts:secrets, a global permission that a binding limited to spaces cannot grant (contract secrets are server-wide); bind the role with global: true or remove contracts:secrets from it",
                            b.role
                        );
                    }
                    if all {
                        BindingScope::AllSpaces
                    } else {
                        BindingScope::Spaces(list)
                    }
                }
                (true, Some(_)) => bail!("{at}: set either spaces or global: true, not both"),
                _ => bail!("{at}: set spaces (a non-empty list) or global: true"),
            };
            cfg.bindings.push((
                subject,
                Binding {
                    role: b.role,
                    permissions: perms,
                    scope,
                },
            ));
        }
        Ok(cfg)
    }

    /// Each path prefix and host belongs to one space: overlaps between spaces are refused.
    fn check_ownership(&self) -> anyhow::Result<()> {
        use fastmock_core::ownership::{hosts_overlap, prefixes_overlap};
        let all: Vec<&SpaceDef> = self.spaces.values().collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                for pa in &a.path_prefixes {
                    if let Some(pb) = b.path_prefixes.iter().find(|pb| prefixes_overlap(pa, pb)) {
                        bail!(
                            "space {} and space {} overlap: path prefix `{pa}` of space {} and `{pb}` of space {}; each path belongs to one space",
                            a.name,
                            b.name,
                            a.name,
                            b.name
                        );
                    }
                }
                for ha in &a.hosts {
                    if let Some(hb) = b.hosts.iter().find(|hb| hosts_overlap(ha, hb)) {
                        bail!(
                            "space {} and space {} overlap: host `{ha}` of space {} and `{hb}` of space {}; each host belongs to one space",
                            a.name,
                            b.name,
                            a.name,
                            b.name
                        );
                    }
                }
                for da in &a.mail_domains {
                    if let Some(db) = b
                        .mail_domains
                        .iter()
                        .find(|db| fastmock_mail::routing::domains_overlap(da, db))
                    {
                        bail!(
                            "space {} and space {} overlap: mail domain `{da}` of space {} and `{db}` of space {}; each mail domain belongs to one space",
                            a.name,
                            b.name,
                            a.name,
                            b.name
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// The data-plane paths and hosts owned by spaces.
    pub fn ownership(&self) -> fastmock_core::ownership::Ownership {
        let mut o = fastmock_core::ownership::Ownership::default();
        for s in self.spaces.values() {
            for p in &s.path_prefixes {
                o.add_prefix(&s.name, p);
            }
            for h in &s.hosts {
                o.add_host(&s.name, h);
            }
        }
        o
    }

    /// The mail domains owned by spaces, for routing captured mail.
    pub fn mail_domains(&self) -> fastmock_mail::MailDomains {
        let mut d = fastmock_mail::MailDomains::default();
        for s in self.spaces.values() {
            for m in &s.mail_domains {
                d.add(&s.name, m);
            }
        }
        d
    }

    /// Permissions of a built-in or custom role.
    pub fn role(&self, name: &str) -> Option<PermSet> {
        builtin_role(name).or_else(|| self.roles.get(name).copied())
    }

    pub fn space(&self, name: &str) -> Option<&SpaceDef> {
        self.spaces.get(name)
    }

    /// Built-in roles, then the custom roles of the access file, with their permissions.
    pub fn roles(&self) -> Vec<(&str, PermSet, bool)> {
        BUILTIN_ROLES
            .iter()
            .filter_map(|n| builtin_role(n).map(|p| (*n, p, true)))
            .chain(self.roles.iter().map(|(n, p)| (n.as_str(), *p, false)))
            .collect()
    }

    pub fn spaces(&self) -> impl Iterator<Item = &SpaceDef> {
        self.spaces.values()
    }

    pub fn has_spaces(&self) -> bool {
        !self.spaces.is_empty()
    }

    /// The bindings of the access file with their subjects, in file order.
    pub fn bindings(&self) -> impl Iterator<Item = (&Subject, &Binding)> {
        self.bindings.iter().map(|(s, b)| (s, b))
    }

    /// `token:` subjects with the index of their binding in the file.
    pub fn token_subjects(&self) -> impl Iterator<Item = (usize, &str)> {
        self.bindings
            .iter()
            .enumerate()
            .filter_map(|(i, (s, _))| match s {
                Subject::Token(n) => Some((i, n.as_str())),
                _ => None,
            })
    }

    /// Bindings of the access file that apply to a principal. Static and managed tokens match
    /// `token:` subjects; users match `user:` subjects by name and `group:` subjects by group.
    pub fn bindings_for(&self, kind: PrincipalKind, name: &str, groups: &[String]) -> Vec<Binding> {
        self.bindings
            .iter()
            .filter(|(s, _)| match (s, kind) {
                (Subject::Token(n), PrincipalKind::Token) => n == name,
                (Subject::User(n), PrincipalKind::User) => n == name,
                (Subject::Group(g), PrincipalKind::User) => groups.iter().any(|x| x == g),
                _ => false,
            })
            .map(|(_, b)| b.clone())
            .collect()
    }
}

/// Effective permissions of a principal: global ones and those granted in single spaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    global: PermSet,
    spaces: BTreeMap<String, PermSet>,
    /// Space-scoped permissions granted through `spaces: ["*"]` (empty when no space is configured);
    /// they are also included in `spaces` for every configured space.
    all_spaces: PermSet,
}

impl Grants {
    /// The grants of the bindings; `spaces: ["*"]` covers every space `access` configures.
    pub fn from_bindings(bindings: &[Binding], access: &AccessConfig) -> Self {
        let mut g = Grants::default();
        for b in bindings {
            match &b.scope {
                BindingScope::Global => g.global = g.global.union(b.permissions),
                BindingScope::Spaces(list) => {
                    for s in list {
                        let e = g.spaces.entry(s.clone()).or_default();
                        *e = e.union(b.permissions.space_scoped());
                    }
                }
                BindingScope::AllSpaces if access.has_spaces() => {
                    g.all_spaces = g.all_spaces.union(b.permissions.space_scoped());
                }
                BindingScope::AllSpaces => {}
            }
        }
        if !g.all_spaces.is_empty() {
            for s in access.spaces.keys() {
                let e = g.spaces.entry(s.clone()).or_default();
                *e = e.union(g.all_spaces);
            }
        }
        if !bindings.is_empty() {
            g.global.insert(InfoRead);
        }
        g.spaces.retain(|_, p| !p.is_empty());
        g
    }

    pub fn admin() -> Self {
        Grants {
            global: PermSet::all(),
            spaces: BTreeMap::new(),
            all_spaces: PermSet::default(),
        }
    }

    /// Space-scoped permissions granted through `spaces: ["*"]`.
    pub fn all_spaces(&self) -> PermSet {
        self.all_spaces
    }

    /// Granted globally, or, for a space-scoped permission, through `spaces: ["*"]`: the permission
    /// holds in every space, including spaces added to the access file later.
    pub fn allowed_in_all_spaces(&self, p: Permission) -> bool {
        self.global.contains(p) || (p.scope() == ScopeKind::Space && self.all_spaces.contains(p))
    }

    pub fn global(&self) -> PermSet {
        self.global
    }

    /// `allowed(permission, space)`: granted globally, or granted in that space for a space-scoped permission.
    pub fn allowed(&self, p: Permission, space: Option<&str>) -> bool {
        self.global.contains(p)
            || (p.scope() == ScopeKind::Space
                && space.is_some_and(|s| self.spaces.get(s).is_some_and(|x| x.contains(p))))
    }

    pub fn global_has(&self, p: Permission) -> bool {
        self.global.contains(p)
    }

    pub fn anywhere(&self, p: Permission) -> bool {
        self.global.contains(p)
            || (p.scope() == ScopeKind::Space && self.spaces.values().any(|x| x.contains(p)))
    }

    /// Spaces where a space binding grants the permission (global grants not included).
    pub fn spaces_with(&self, p: Permission) -> Vec<String> {
        if p.scope() == ScopeKind::Global {
            return vec![];
        }
        self.spaces
            .iter()
            .filter(|(_, x)| x.contains(p))
            .map(|(s, _)| s.clone())
            .collect()
    }

    pub fn bound_spaces(&self) -> impl Iterator<Item = (&String, &PermSet)> {
        self.spaces.iter()
    }

    /// Effective space-scoped permissions in one space.
    pub fn in_space(&self, space: &str) -> PermSet {
        self.global
            .space_scoped()
            .union(self.spaces.get(space).copied().unwrap_or_default())
    }

    /// True when the principal holds some permission everywhere (apart from the implied `info:read`).
    pub fn sees_all_spaces(&self) -> bool {
        let mut g = self.global;
        g.0 &= !InfoRead.bit();
        !g.space_scoped().is_empty()
    }
}

#[derive(Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub struct PermissionsView {
    /// Permissions granted everywhere.
    #[schema(example = json!(["stubs:read", "info:read"]))]
    pub global: Vec<String>,
    /// Permissions granted only in single spaces, by space; the effective permissions in a space are these plus the global ones. Permissions granted through `spaces: ["*"]` appear under every configured space.
    #[schema(example = json!({"payments": ["stubs:write", "journal:read"]}))]
    pub spaces: BTreeMap<String, Vec<String>>,
    /// Permissions granted in every space through `spaces: ["*"]`, including spaces added to the access file later (empty when none).
    #[serde(rename = "allSpaces")]
    #[schema(example = json!(["stubs:read"]))]
    pub all_spaces: Vec<String>,
}

impl Serialize for Grants {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        PermissionsView {
            global: self.global.names().into_iter().map(String::from).collect(),
            spaces: self
                .spaces
                .iter()
                .map(|(k, v)| (k.clone(), v.names().into_iter().map(String::from).collect()))
                .collect(),
            all_spaces: self
                .all_spaces
                .names()
                .into_iter()
                .map(String::from)
                .collect(),
        }
        .serialize(s)
    }
}

/// Permission and scope kind a route needs before its handler runs. Handlers of space-scoped routes
/// check the space they act on. `None`: the route has no rule, so only an admin (every permission
/// globally) may call it.
pub fn route_rule(method: &Method, full_path: &str) -> Option<(Permission, ScopeKind)> {
    let path = full_path.strip_prefix("/__admin").unwrap_or(full_path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let m = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    let natural = |p: Permission| Some((p, p.scope()));
    let global = |p: Permission| Some((p, ScopeKind::Global));
    match (m, segs.as_slice()) {
        ("GET", ["stubs"]) => natural(StubsRead),
        ("POST" | "DELETE", ["stubs"]) => natural(StubsWrite),
        ("POST", ["stubs", "import"]) => natural(ImportsRun),
        ("POST", ["stubs", "validate" | "find-by-metadata"]) => natural(StubsRead),
        ("POST", ["stubs", "remove-by-metadata"]) => natural(StubsWrite),
        ("GET", ["stubs", "unused"]) => natural(StubsRead),
        ("GET", ["stubs", "counters"]) => natural(StateRead),
        ("DELETE", ["stubs", "counters"]) => natural(StateWrite),
        ("GET", ["stubs", _]) => natural(StubsRead),
        ("PUT" | "PATCH" | "DELETE", ["stubs", _]) => natural(StubsWrite),
        ("GET", ["export"]) => natural(StubsRead),
        ("GET", ["requests"]) => natural(JournalRead),
        ("DELETE", ["requests"]) => natural(JournalDelete),
        ("GET" | "POST", ["requests", "count"]) => natural(JournalRead),
        ("POST", ["requests", "near-misses" | "find"]) => natural(JournalRead),
        ("GET", ["requests", "unmatched"]) => global(JournalRead),
        ("POST", ["requests", "remove"]) => natural(JournalDelete),
        ("GET", ["requests", _]) => natural(JournalRead),
        ("POST", ["verify"] | ["verify", "sequence"]) => natural(JournalRead),
        ("GET", ["callbacks"]) => natural(CallbacksRead),
        ("DELETE", ["callbacks"]) => natural(CallbacksDelete),
        ("GET", ["scenarios"]) => natural(StateRead),
        ("DELETE", ["scenarios"]) | ("PUT", ["scenarios", _]) => natural(StateWrite),
        ("GET", ["kv", _]) => natural(StateRead),
        ("PUT" | "DELETE", ["kv", _]) => natural(StateWrite),
        ("POST", ["reset"]) => natural(SystemReset),
        ("GET", ["info" | "version" | "plugins" | "spaces"]) => natural(InfoRead),
        ("GET", ["audit"]) => natural(AuditRead),
        ("GET", ["settings"]) => global(ChaosRead),
        ("PUT", ["settings"]) => global(ChaosWrite),
        (
            "POST",
            [
                "import",
                "asyncapi" | "graphql" | "soapui" | "wsdl" | "microcks",
            ],
        ) => global(ImportsRun),
        ("POST", ["import"] | ["import", _]) => natural(ImportsRun),
        ("GET", ["recordings"]) => natural(RecordingsRead),
        ("POST", ["recordings", "start" | "stop" | "snapshot"]) => natural(RecordingsWrite),
        ("GET", ["chaos"]) => natural(ChaosRead),
        ("PUT" | "DELETE", ["chaos"]) => natural(ChaosWrite),
        ("GET", ["grpc", "descriptors"]) => natural(GrpcRead),
        ("PUT" | "DELETE", ["grpc", "descriptors", _, ..]) => natural(GrpcWrite),
        ("GET", ["files", ..]) => natural(FilesRead),
        ("PUT" | "DELETE", ["files", _, ..]) => natural(FilesWrite),
        ("GET", ["contract-tests"] | ["contract-tests", _]) => natural(ContractsRead),
        ("POST", ["contract-tests"]) | ("DELETE", ["contract-tests", _]) => natural(ContractsWrite),
        ("GET", ["services", ..]) => global(StubsRead),
        ("DELETE", ["services", _]) => global(StubsWrite),
        ("GET", ["async-mocks", ..]) => global(StubsRead),
        ("PUT" | "POST" | "DELETE", ["async-mocks", _, ..]) => global(StubsWrite),
        ("GET", ["tokens"]) => natural(TokensRead),
        ("POST", ["tokens"]) => natural(TokensWrite),
        ("GET", ["tokens", "options"]) => natural(TokensWrite),
        ("GET", ["tokens", "self"]) | ("DELETE", ["tokens", _]) => global(InfoRead),
        ("GET", ["tokens", _]) => natural(TokensRead),
        ("GET", ["access"]) => global(AccessRead),
        ("GET", ["mail", "messages"]) => natural(MailRead),
        ("POST", ["mail", "messages"]) => natural(MailWrite),
        ("DELETE", ["mail", "messages"]) => natural(MailDelete),
        ("POST", ["mail", "messages", "wait"]) | ("POST", ["mail", "verify"]) => natural(MailRead),
        ("GET", ["mail", "messages", _, ..]) => natural(MailRead),
        ("DELETE", ["mail", "messages", _]) => natural(MailDelete),
        ("PUT", ["mail", "messages", _, "labels"]) => natural(MailWrite),
        ("GET", ["mail", "stream"]) => natural(MailRead),
        ("GET", ["mail", "chaos" | "webhooks" | "mailboxes"]) => natural(MailRead),
        ("PUT" | "DELETE", ["mail", "chaos" | "webhooks"]) => natural(MailWrite),
        ("PUT" | "DELETE", ["mail", "mailboxes", _, "password"]) => natural(MailWrite),
        ("POST", ["mail", "inbound"]) => natural(MailWrite),
        ("GET", ["capture", "sources"] | ["capture", "sources", _]) => natural(MessagesRead),
        ("PUT" | "DELETE", ["capture", "sources", _]) => natural(MessagesWrite),
        ("GET", ["capture", "messages"] | ["capture", "messages", _, ..]) => natural(MessagesRead),
        ("POST", ["capture", "messages", "find" | "wait"] | ["capture", "verify"]) => {
            natural(MessagesRead)
        }
        ("DELETE", ["capture", "messages"] | ["capture", "messages", _]) => natural(MessagesDelete),
        ("GET" | "POST" | "DELETE", ["mcp"]) if path == full_path => global(InfoRead),
        ("GET", ["plugins", _, ..]) => global(StubsRead),
        (_, ["plugins", _, ..]) => global(StubsWrite),
        _ => None,
    }
}

/// The rule of earlier versions for paths without a rule (unknown paths included), used when neither an
/// access file nor OIDC is configured: reads need viewer, everything else editor.
pub fn legacy_fallback(method: &Method) -> (Permission, ScopeKind) {
    if method == Method::GET || method == Method::HEAD {
        (InfoRead, ScopeKind::Global)
    } else {
        (StubsWrite, ScopeKind::Global)
    }
}

/// The rule a principal holding only global built-in roles meets at the middleware for routes whose
/// handler checks the admin role itself, as before permissions existed: `/audit` as a read and
/// `/reset` as a change, so the handler answers with the earlier denial body.
pub fn legacy_gate(rule: (Permission, ScopeKind)) -> (Permission, ScopeKind) {
    match rule.0 {
        AuditRead => (InfoRead, ScopeKind::Global),
        SystemReset => (StubsWrite, ScopeKind::Global),
        _ => rule,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
roles:
  - name: stub-author
    permissions: [stubs:read, stubs:write, imports:run, journal:read, state:read]
  - name: files-all
    permissions: ["files:*"]
spaces:
  - name: payments
    description: Payment provider mocks
    pathPrefixes: [/payments/, /psp/]
    hosts: [PSP.example.com]
  - name: billing
bindings:
  - subject: group:payments-dev
    role: editor
    spaces: [payments]
  - subject: group:platform
    role: admin
    global: true
  - subject: token:ci-billing
    role: stub-author
    spaces: [billing]
  - subject: user:alice
    role: viewer
    global: true
"#;

    #[test]
    fn builtin_roles_match_the_design() {
        let v = viewer_permissions();
        for p in ALL_PERMISSIONS {
            let is_read = p.name().ends_with(":read");
            assert_eq!(
                v.contains(p),
                is_read && p != AuditRead && p != TokensRead && p != AccessRead,
                "{p}"
            );
        }
        let e = editor_permissions();
        assert!(e.includes(v));
        for p in [
            StubsPurge,
            AuditRead,
            SystemReset,
            ContractsSecrets,
            TokensRead,
            TokensWrite,
            AccessRead,
        ] {
            assert!(!e.contains(p), "{p}");
        }
        assert_eq!(builtin_role("admin"), Some(PermSet::all()));
        assert_eq!(minimal_role(StubsRead), "viewer");
        assert_eq!(minimal_role(StubsWrite), "editor");
        assert_eq!(minimal_role(StubsPurge), "admin");
        assert_eq!(minimal_role(AccessRead), "admin");
        assert_eq!(AccessRead.scope(), ScopeKind::Global);
        for p in ALL_PERMISSIONS {
            assert_eq!(Permission::parse(p.name()), Some(p));
        }
    }

    #[test]
    fn parses_the_design_example() {
        let c = AccessConfig::parse(EXAMPLE).unwrap();
        assert_eq!(
            c.role("stub-author").unwrap().names(),
            vec![
                "stubs:read",
                "stubs:write",
                "imports:run",
                "journal:read",
                "state:read"
            ]
        );
        assert_eq!(
            c.role("files-all").unwrap().names(),
            vec!["files:read", "files:write"]
        );
        let p = c.space("payments").unwrap();
        assert_eq!(p.hosts, vec!["psp.example.com"]);
        assert_eq!(p.path_prefixes, vec!["/payments/", "/psp/"]);
        assert!(c.space("billing").is_some());
        let b = c.bindings_for(PrincipalKind::Token, "ci-billing", &[]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].scope, BindingScope::Spaces(vec!["billing".into()]));
        let u = c.bindings_for(
            PrincipalKind::User,
            "alice",
            &["payments-dev".to_string(), "other".to_string()],
        );
        assert_eq!(u.len(), 2);
        assert!(
            c.bindings_for(PrincipalKind::User, "ci-billing", &[])
                .is_empty()
        );
        assert!(
            c.bindings_for(PrincipalKind::Token, "alice", &[])
                .is_empty()
        );
        assert_eq!(
            c.bindings_for(PrincipalKind::User, "bob", &["platform".into()])
                .len(),
            1
        );
    }

    #[test]
    fn wildcards_parse_in_flow_sequences() {
        let c = AccessConfig::parse(
            "roles:\n  - name: ci\n    permissions: [stubs:*, imports:run, journal:*, state:*, callbacks:*, files:*]\n",
        )
        .unwrap();
        let p = c.role("ci").unwrap();
        assert!(p.contains(StubsPurge) && p.contains(CallbacksDelete) && p.contains(FilesWrite));
        assert!(!p.contains(ContractsRead));
    }

    #[test]
    fn errors_name_the_entry() {
        let cases = [
            (
                "roles: [{name: viewer, permissions: [stubs:read]}]",
                "roles[0] (viewer)",
            ),
            (
                "roles: [{name: x, permissions: [stubs:wrte]}]",
                "unknown permission `stubs:wrte`",
            ),
            (
                "roles: [{name: x, permissions: [nothing:*]}]",
                "roles[0] (x): unknown permission",
            ),
            ("roles: [{name: x, permissions: []}]", "roles[0] (x)"),
            (
                "roles: [{name: Bad, permissions: [stubs:read]}]",
                "roles[0] (Bad)",
            ),
            (
                "roles: [{name: a, permissions: [stubs:read]}, {name: a, permissions: [stubs:read]}]",
                "roles[1] (a): role defined twice",
            ),
            ("spaces: [{name: -x}]", "spaces[0] (-x)"),
            (
                "spaces: [{name: a, pathPrefixes: [payments]}]",
                "spaces[0] (a): path prefix",
            ),
            (
                "spaces: [{name: a, pathPrefixes: [/pay/../bill/]}]",
                "spaces[0] (a): path prefix `/pay/../bill/` must be a plain path",
            ),
            (
                "spaces: [{name: a, pathPrefixes: ['/pay/..%2Fbill/']}]",
                "spaces[0] (a): path prefix `/pay/..%2Fbill/` must be a plain path",
            ),
            (
                "spaces: [{name: a, pathPrefixes: ['/pay%2f']}]",
                "must be a plain path",
            ),
            (
                "spaces: [{name: a, pathPrefixes: ['/pay%5c']}]",
                "must be a plain path",
            ),
            (
                "spaces: [{name: a, pathPrefixes: ['/pay\\bill/']}]",
                "must be a plain path",
            ),
            ("spaces: [{name: a, hosts: ['a b']}]", "spaces[0] (a): host"),
            (
                "spaces: [{name: a}, {name: a}]",
                "spaces[1] (a): space defined twice",
            ),
            (
                "bindings: [{subject: 'team:x', role: viewer, global: true}]",
                "bindings[0] (team:x)",
            ),
            (
                "bindings: [{subject: 'user:x', role: nobody, global: true}]",
                "bindings[0] (user:x): unknown role `nobody`",
            ),
            (
                "bindings: [{subject: 'user:x', role: viewer, spaces: [nope]}]",
                "unknown space `nope`",
            ),
            (
                "bindings: [{subject: 'user:x', role: viewer}]",
                "bindings[0] (user:x): set spaces",
            ),
            (
                "bindings: [{subject: 'user:x', role: viewer, spaces: []}]",
                "bindings[0] (user:x): set spaces",
            ),
            (
                "spaces: [{name: a}]\nbindings: [{subject: 'user:x', role: viewer, global: true, spaces: [a]}]",
                "not both",
            ),
            ("rolez: []", "unknown field"),
        ];
        for (doc, want) in cases {
            let e = format!("{:#}", AccessConfig::parse(doc).unwrap_err());
            assert!(e.contains(want), "{doc}: {e}");
        }
        assert!(AccessConfig::parse("").unwrap().spaces().next().is_none());
    }

    #[test]
    fn contract_secrets_are_global_only() {
        assert_eq!(ContractsSecrets.scope(), ScopeKind::Global);
        let e = format!(
            "{:#}",
            AccessConfig::parse(
                "roles: [{name: secret-runner, permissions: [contracts:write, contracts:secrets]}]\nspaces: [{name: billing}]\nbindings: [{subject: 'user:x', role: viewer, global: true}, {subject: 'group:qa', role: secret-runner, spaces: [billing]}]",
            )
            .unwrap_err()
        );
        assert!(
            e.starts_with(
                "bindings[1] (group:qa): role `secret-runner` lists contracts:secrets, a global permission"
            ),
            "{e}"
        );
        let c = AccessConfig::parse(
            "roles: [{name: secret-runner, permissions: [contracts:write, contracts:secrets]}, {name: tester, permissions: [\"contracts:*\"]}]\nspaces: [{name: billing}]\nbindings: [{subject: 'group:ops', role: secret-runner, global: true}, {subject: 'group:qa', role: tester, spaces: [billing]}, {subject: 'group:pay', role: admin, spaces: [billing]}]",
        )
        .unwrap();
        let ops = Grants::from_bindings(
            &c.bindings_for(PrincipalKind::User, "a", &["ops".into()]),
            &c,
        );
        assert!(ops.allowed(ContractsSecrets, None));
        assert!(ops.allowed(ContractsSecrets, Some("billing")));
        for group in ["qa", "pay"] {
            let g = Grants::from_bindings(
                &c.bindings_for(PrincipalKind::User, "a", &[group.into()]),
                &c,
            );
            assert!(g.allowed(ContractsWrite, Some("billing")), "{group}");
            assert!(!g.allowed(ContractsSecrets, Some("billing")), "{group}");
            assert!(!g.anywhere(ContractsSecrets), "{group}");
            let v = serde_json::to_value(&g).unwrap();
            assert!(
                !v["spaces"]["billing"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!("contracts:secrets")),
                "{group}: {v}"
            );
        }
    }

    #[test]
    fn load_errors_are_one_line_with_the_file_and_the_entry() {
        let f = std::env::temp_dir().join(format!("fm-access-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::write(
            &f,
            "roles: [{name: stub-author, permissions: [stubs:wrte]}]\n",
        )
        .unwrap();
        let e = AccessConfig::load(&f).unwrap_err();
        assert_eq!(e.chain().count(), 1, "{e:?}");
        assert_eq!(
            format!("{e:?}"),
            format!(
                "access file {}: roles[0] (stub-author): unknown permission `stubs:wrte`",
                f.display()
            )
        );
        std::fs::write(&f, "spaces: [{name: billing}]\n").unwrap();
        let ok = AccessConfig::load(&f).unwrap();
        assert_eq!(ok.origin(), format!("access file {}", f.display()));
        std::fs::remove_file(&f).unwrap();
        let missing = AccessConfig::load(&f).unwrap_err();
        assert_eq!(missing.chain().count(), 1);
        assert!(
            format!("{missing:?}").starts_with("reading the access file "),
            "{missing:?}"
        );
    }

    #[test]
    fn grants_follow_bindings() {
        let c = AccessConfig::parse(EXAMPLE).unwrap();
        let g = Grants::from_bindings(&c.bindings_for(PrincipalKind::Token, "ci-billing", &[]), &c);
        assert!(g.allowed(StubsWrite, Some("billing")));
        assert!(!g.allowed(StubsWrite, Some("payments")));
        assert!(!g.allowed(StubsWrite, None));
        assert!(g.allowed(InfoRead, None));
        assert!(g.anywhere(StubsRead));
        assert!(!g.anywhere(FilesRead));
        assert_eq!(g.spaces_with(StubsRead), vec!["billing"]);
        assert!(!g.sees_all_spaces());
        let admin_in_space = Grants::from_bindings(
            &[Binding {
                role: "admin".into(),
                permissions: PermSet::all(),
                scope: BindingScope::Spaces(vec!["payments".into()]),
            }],
            &c,
        );
        assert!(!admin_in_space.allowed(StubsPurge, Some("payments")));
        assert!(!admin_in_space.allowed(AuditRead, None));
        assert!(admin_in_space.allowed(TokensWrite, Some("payments")));
        let viewer = Grants::from_bindings(&[Binding::global("viewer", viewer_permissions())], &c);
        assert!(viewer.allowed(StubsRead, Some("payments")));
        assert!(viewer.sees_all_spaces());
        assert!(Grants::from_bindings(&[], &c).global().is_empty());
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["global"], serde_json::json!(["info:read"]));
        assert_eq!(
            v["spaces"]["billing"],
            serde_json::json!([
                "stubs:read",
                "stubs:write",
                "imports:run",
                "journal:read",
                "state:read"
            ])
        );
    }

    #[test]
    fn route_rules() {
        let r = |m: Method, p: &str| route_rule(&m, p).unwrap_or((SystemReset, ScopeKind::Global));
        assert_eq!(
            r(Method::GET, "/__admin/stubs"),
            (StubsRead, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::HEAD, "/__admin/stubs/"),
            (StubsRead, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::DELETE, "/__admin/stubs"),
            (StubsWrite, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::GET, "/__admin/stubs/counters"),
            (StateRead, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::POST, "/__admin/stubs/import"),
            (ImportsRun, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::GET, "/__admin/requests/unmatched"),
            (JournalRead, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::POST, "/__admin/reset"),
            (SystemReset, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::GET, "/__admin/files/a/b.json"),
            (FilesRead, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::PUT, "/__admin/files/a/b.json"),
            (FilesWrite, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::POST, "/__admin/import/asyncapi"),
            (ImportsRun, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::POST, "/__admin/import/har"),
            (ImportsRun, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::GET, "/__admin/plugins"),
            (InfoRead, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::POST, "/__admin/plugins/x/verify"),
            (StubsWrite, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::POST, "/__admin/tokens"),
            (TokensWrite, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::GET, "/__admin/tokens/self"),
            (InfoRead, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::DELETE, "/__admin/tokens/x"),
            (InfoRead, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::GET, "/__admin/tokens/x"),
            (TokensRead, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::GET, "/__admin/tokens/options"),
            (TokensWrite, ScopeKind::Space)
        );
        assert_eq!(
            r(Method::GET, "/__admin/access"),
            (AccessRead, ScopeKind::Global)
        );
        assert_eq!(route_rule(&Method::GET, "/__admin/nope"), None);
        assert_eq!(route_rule(&Method::OPTIONS, "/__admin/stubs"), None);
    }

    #[test]
    fn mail_permissions_follow_the_design() {
        let v = viewer_permissions();
        let e = editor_permissions();
        assert!(v.contains(MailRead) && !v.contains(MailWrite) && !v.contains(MailDelete));
        assert!(e.contains(MailRead) && e.contains(MailWrite) && e.contains(MailDelete));
        assert!(PermSet::all().contains(MailDelete));
        for p in [MailRead, MailWrite, MailDelete] {
            assert_eq!(p.scope(), ScopeKind::Space, "{p}");
            assert_eq!(p.resource(), "mail");
        }
        assert_eq!(minimal_role(MailRead), "viewer");
        assert_eq!(minimal_role(MailDelete), "editor");
        let c = AccessConfig::parse(
            "roles: [{name: mailer, permissions: [\"mail:*\"]}]\nspaces: [{name: billing}]\nbindings: [{subject: 'token:m', role: mailer, spaces: [billing]}]",
        )
        .unwrap();
        assert_eq!(
            c.role("mailer").unwrap().names(),
            vec!["mail:read", "mail:write", "mail:delete"]
        );
        let g = Grants::from_bindings(&c.bindings_for(PrincipalKind::Token, "m", &[]), &c);
        assert!(g.allowed(MailDelete, Some("billing")));
        assert!(!g.allowed(MailRead, None));
    }

    #[test]
    fn mail_route_rules() {
        let r = |m: Method, p: &str| route_rule(&m, p);
        let space = |p: Permission| Some((p, ScopeKind::Space));
        for (m, p, want) in [
            (Method::GET, "/__admin/mail/messages", MailRead),
            (Method::POST, "/__admin/mail/messages", MailWrite),
            (Method::DELETE, "/__admin/mail/messages", MailDelete),
            (Method::POST, "/__admin/mail/messages/wait", MailRead),
            (Method::POST, "/__admin/mail/verify", MailRead),
            (Method::GET, "/__admin/mail/messages/x", MailRead),
            (Method::HEAD, "/__admin/mail/messages/x/raw", MailRead),
            (Method::GET, "/__admin/mail/messages/x/parts/3", MailRead),
            (Method::GET, "/__admin/mail/messages/x/codes", MailRead),
            (Method::DELETE, "/__admin/mail/messages/x", MailDelete),
            (Method::PUT, "/__admin/mail/messages/x/labels", MailWrite),
            (Method::GET, "/__admin/mail/stream", MailRead),
            (Method::GET, "/__admin/mail/chaos", MailRead),
            (Method::PUT, "/__admin/mail/chaos", MailWrite),
            (Method::DELETE, "/__admin/mail/webhooks", MailWrite),
            (Method::GET, "/__admin/mail/mailboxes", MailRead),
            (
                Method::PUT,
                "/__admin/mail/mailboxes/a@example.test/password",
                MailWrite,
            ),
            (
                Method::DELETE,
                "/__admin/mail/mailboxes/a@example.test/password",
                MailWrite,
            ),
            (Method::POST, "/__admin/mail/inbound", MailWrite),
        ] {
            assert_eq!(r(m.clone(), p), space(want), "{m} {p}");
        }
        assert_eq!(r(Method::PUT, "/__admin/mail/messages/x"), None);
        assert_eq!(r(Method::POST, "/__admin/mail/messages/x/raw"), None);
        assert_eq!(r(Method::GET, "/__admin/mail"), None);
    }

    #[test]
    fn mail_domains_parse_route_and_must_not_overlap() {
        let c = AccessConfig::parse(
            r#"
spaces:
  - name: billing
    mailDomains: [Billing.Example.TEST., "*.invoices.example.test", billing.example.test]
  - name: crm
    mailDomains: [crm.example.test, invoices.example.test]
  - name: plain
"#,
        )
        .unwrap();
        assert_eq!(
            c.space("billing").unwrap().mail_domains,
            vec!["billing.example.test", "*.invoices.example.test"]
        );
        assert!(c.space("plain").unwrap().mail_domains.is_empty());
        let d = c.mail_domains();
        assert_eq!(d.owner("billing.example.test"), Some("billing"));
        assert_eq!(d.owner("eu.invoices.example.test"), Some("billing"));
        assert_eq!(d.owner("invoices.example.test"), Some("crm"));
        assert_eq!(d.owner("other.example"), None);
        let e = AccessConfig::parse(
            "spaces: [{name: a, mailDomains: [\"*.example.test\"]}, {name: b, mailDomains: [x.example.test]}]",
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("space a and space b overlap: mail domain `*.example.test` of space a and `x.example.test` of space b"),
            "{e}"
        );
        let e = AccessConfig::parse("spaces: [{name: a, mailDomains: [\"bad domain\"]}]")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("spaces[0] (a)") && e.contains("bad domain"),
            "{e}"
        );
        assert!(
            AccessConfig::parse(
                "spaces: [{name: a, mailDomains: [a.example.test]}, {name: b, mailDomains: [\"*.a.example.test\"]}]"
            )
            .is_ok()
        );
    }

    const ALL_SPACES_DOC: &str = r#"
roles:
  - name: tester
    permissions: ["contracts:*", stubs:read]
spaces:
  - name: payments
  - name: billing
bindings:
  - subject: token:all-viewer
    role: viewer
    spaces: ["*"]
  - subject: token:mixed
    role: viewer
    spaces: ["*"]
  - subject: token:mixed
    role: editor
    spaces: [billing]
  - subject: token:all-admin
    role: admin
    spaces: ["*"]
  - subject: group:qa
    role: tester
    spaces: ["*"]
"#;

    fn grants_of(c: &AccessConfig, kind: PrincipalKind, name: &str, groups: &[String]) -> Grants {
        Grants::from_bindings(&c.bindings_for(kind, name, groups), c)
    }

    #[test]
    fn all_spaces_bindings_parse_and_keep_star_literally() {
        let c = AccessConfig::parse(ALL_SPACES_DOC).unwrap();
        let b = c.bindings_for(PrincipalKind::Token, "all-viewer", &[]);
        assert_eq!(b[0].scope, BindingScope::AllSpaces);
        assert_eq!(b[0].scope.space_list(), vec!["*"]);
        assert!(BindingScope::Global.space_list().is_empty());
        let listed: Vec<(String, Vec<String>)> = c
            .bindings()
            .map(|(s, b)| (s.to_string(), b.scope.space_list()))
            .collect();
        assert_eq!(listed[0], ("token:all-viewer".into(), vec!["*".into()]));
        assert_eq!(listed[2], ("token:mixed".into(), vec!["billing".into()]));
        assert_eq!(listed[4].0, "group:qa");
    }

    #[test]
    fn all_spaces_errors_name_the_entry() {
        for (doc, want) in [
            (
                "spaces: [{name: billing}]\nbindings: [{subject: 'user:x', role: viewer, spaces: ['*', billing]}]",
                "bindings[0] (user:x): \"*\" must be the only entry in spaces",
            ),
            (
                "spaces: [{name: billing}]\nbindings: [{subject: 'user:x', role: viewer, spaces: [billing, '*']}]",
                "bindings[0] (user:x): \"*\" must be the only entry in spaces",
            ),
            (
                "spaces: [{name: billing}]\nbindings: [{subject: 'user:x', role: viewer, spaces: ['*', '*']}]",
                "bindings[0] (user:x): \"*\" must be the only entry in spaces",
            ),
            (
                "bindings: [{subject: 'user:x', role: viewer, spaces: ['*']}]",
                "bindings[0] (user:x): spaces: [\"*\"] needs at least one space in the file",
            ),
            (
                "roles: [{name: secret-runner, permissions: [contracts:write, contracts:secrets]}]\nspaces: [{name: billing}]\nbindings: [{subject: 'group:qa', role: secret-runner, spaces: ['*']}]",
                "bindings[0] (group:qa): role `secret-runner` lists contracts:secrets, a global permission that a binding limited to spaces cannot grant",
            ),
        ] {
            let e = format!("{:#}", AccessConfig::parse(doc).unwrap_err());
            assert!(e.starts_with(want), "{doc}: {e}");
        }
        let f = std::env::temp_dir().join(format!("fm-access-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::write(
            &f,
            "spaces: [{name: billing}]\nbindings: [{subject: 'user:x', role: viewer, spaces: ['*', billing]}]\n",
        )
        .unwrap();
        let e = AccessConfig::load(&f).unwrap_err();
        std::fs::remove_file(&f).unwrap();
        assert_eq!(
            format!("{e:?}"),
            format!(
                "access file {}: bindings[0] (user:x): \"*\" must be the only entry in spaces",
                f.display()
            )
        );
    }

    #[test]
    fn all_spaces_alone_grants_every_space_but_nothing_global() {
        let c = AccessConfig::parse(ALL_SPACES_DOC).unwrap();
        let g = grants_of(&c, PrincipalKind::Token, "all-viewer", &[]);
        for s in ["payments", "billing"] {
            assert!(g.allowed(StubsRead, Some(s)), "{s}");
            assert!(!g.allowed(StubsWrite, Some(s)), "{s}");
        }
        assert!(!g.allowed(StubsRead, None), "global data is not granted");
        assert!(!g.allowed(StubsRead, Some("unknown")));
        assert!(!g.sees_all_spaces());
        assert_eq!(g.spaces_with(StubsRead), vec!["billing", "payments"]);
        assert_eq!(g.global().names(), vec!["info:read"]);
        assert_eq!(g.all_spaces(), viewer_permissions().space_scoped());
        assert!(g.allowed_in_all_spaces(StubsRead));
        assert!(!g.allowed_in_all_spaces(StubsWrite));
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["global"], serde_json::json!(["info:read"]));
        assert_eq!(
            v["allSpaces"],
            serde_json::json!(
                viewer_permissions()
                    .space_scoped()
                    .names()
                    .into_iter()
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            v["spaces"].as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["billing", "payments"]
        );
        assert_eq!(v["spaces"]["payments"], v["allSpaces"]);
    }

    #[test]
    fn all_spaces_never_grants_global_only_permissions() {
        let c = AccessConfig::parse(ALL_SPACES_DOC).unwrap();
        let g = grants_of(&c, PrincipalKind::Token, "all-admin", &[]);
        for p in ALL_PERMISSIONS {
            let granted = p.scope() == ScopeKind::Space || p == InfoRead;
            assert_eq!(g.allowed_in_all_spaces(p), granted, "{p}");
            assert_eq!(g.allowed(p, Some("billing")), granted, "{p}");
            assert_eq!(g.global_has(p), p == InfoRead, "{p}");
        }
        for p in [
            ContractsSecrets,
            AuditRead,
            SystemReset,
            StubsPurge,
            AccessRead,
        ] {
            assert!(!g.anywhere(p), "{p}");
        }
        let v = serde_json::to_value(&g).unwrap();
        for list in [&v["allSpaces"], &v["spaces"]["payments"]] {
            for name in ["contracts:secrets", "audit:read", "access:read"] {
                assert!(!list.as_array().unwrap().contains(&serde_json::json!(name)));
            }
        }
        let qa = grants_of(&c, PrincipalKind::User, "a", &["qa".into()]);
        assert!(qa.allowed(ContractsWrite, Some("payments")));
        assert!(!qa.allowed(ContractsSecrets, Some("payments")));
        assert!(!qa.anywhere(ContractsSecrets));
    }

    #[test]
    fn all_spaces_mixed_with_a_space_binding() {
        let c = AccessConfig::parse(ALL_SPACES_DOC).unwrap();
        let g = grants_of(&c, PrincipalKind::Token, "mixed", &[]);
        assert!(g.allowed(StubsWrite, Some("billing")));
        assert!(!g.allowed(StubsWrite, Some("payments")));
        assert!(g.allowed(StubsRead, Some("payments")));
        assert_eq!(g.all_spaces(), viewer_permissions().space_scoped());
        assert!(!g.allowed_in_all_spaces(StubsWrite));
        assert_eq!(g.spaces_with(StubsWrite), vec!["billing"]);
        assert_eq!(g.in_space("billing"), editor_permissions().space_scoped());
    }

    #[test]
    fn all_spaces_follow_the_configured_spaces() {
        let binding = [Binding {
            role: "editor".into(),
            permissions: editor_permissions(),
            scope: BindingScope::AllSpaces,
        }];
        let before = AccessConfig::parse("spaces: [{name: payments}]").unwrap();
        let after = AccessConfig::parse("spaces: [{name: payments}, {name: billing}]").unwrap();
        let g = Grants::from_bindings(&binding, &before);
        assert!(!g.allowed(StubsWrite, Some("billing")));
        let g = Grants::from_bindings(&binding, &after);
        assert!(g.allowed(StubsWrite, Some("billing")));
        assert!(g.allowed(StubsWrite, Some("payments")));
        let none = Grants::from_bindings(&binding, &AccessConfig::default());
        assert_eq!(none.global().names(), vec!["info:read"]);
        assert!(none.all_spaces().is_empty());
        assert!(!none.anywhere(StubsWrite));
        assert!(!none.allowed_in_all_spaces(StubsWrite));
        let v = serde_json::to_value(&none).unwrap();
        assert_eq!(v["allSpaces"], serde_json::json!([]));
        assert_eq!(v["spaces"], serde_json::json!({}));
        let plain =
            Grants::from_bindings(&[Binding::global("viewer", viewer_permissions())], &after);
        assert_eq!(
            serde_json::to_value(&plain).unwrap()["allSpaces"],
            serde_json::json!([])
        );
    }

    #[test]
    fn access_read_is_global_and_matched_by_wildcards() {
        let c = AccessConfig::parse(
            "roles: [{name: auditor, permissions: [\"access:*\"]}, {name: everything, permissions: [\"*\"]}]\nspaces: [{name: billing}]\nbindings: [{subject: 'user:a', role: everything, spaces: [billing]}]",
        )
        .unwrap();
        assert_eq!(c.role("auditor").unwrap().names(), vec!["access:read"]);
        assert_eq!(c.role("everything").unwrap(), PermSet::all());
        assert!(builtin_role("admin").unwrap().contains(AccessRead));
        assert!(!editor_permissions().contains(AccessRead));
        let g = grants_of(&c, PrincipalKind::User, "a", &[]);
        assert!(!g.allowed(AccessRead, Some("billing")));
        assert!(!g.anywhere(AccessRead));
        assert!(g.allowed(TokensWrite, Some("billing")));
    }

    fn concrete(path: &str) -> String {
        path.split('/')
            .map(|seg| match seg {
                s if s.starts_with("{*") => "x/y".to_string(),
                s if s.starts_with('{') => "x".to_string(),
                s => s.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// `(method, path)` of every `.route(…)` in the sources of the core admin routers.
    fn router_routes() -> Vec<(String, String)> {
        const SOURCES: [&str; 11] = [
            include_str!("admin.rs"),
            include_str!("admin_core.rs"),
            include_str!("asyncmock.rs"),
            include_str!("contract.rs"),
            include_str!("files.rs"),
            include_str!("imports.rs"),
            include_str!("journal_api.rs"),
            include_str!("mail_api.rs"),
            include_str!("services.rs"),
            include_str!("snapshot.rs"),
            include_str!("tokens.rs"),
        ];
        const OUTSIDE_ADMIN: [&str; 4] = ["/healthz", "/readyz", "/metrics", "/openapi.json"];
        let mut out = Vec::new();
        for src in SOURCES {
            let mut rest = src;
            while let Some(i) = rest.find(".route(") {
                rest = &rest[i + 7..];
                let mut depth = 1;
                let end = rest
                    .char_indices()
                    .find(|(_, c)| {
                        match c {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        depth == 0
                    })
                    .map(|(j, _)| j)
                    .unwrap();
                let expr = &rest[..end];
                let Some(path) = expr.split('"').nth(1) else {
                    continue;
                };
                if !path.starts_with('/') || OUTSIDE_ADMIN.contains(&path) {
                    continue;
                }
                let after = &expr[expr.find(path).unwrap() + path.len() + 1..];
                for m in ["get", "post", "put", "delete", "patch"] {
                    let found = after.match_indices(&format!("{m}(")).any(|(k, _)| {
                        k == 0 || {
                            let b = after.as_bytes()[k - 1];
                            !(b.is_ascii_alphanumeric() || b == b'_')
                        }
                    });
                    if found {
                        out.push((
                            m.to_ascii_uppercase(),
                            format!("/__admin{}", path.replace("{*", "{")),
                        ));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_core_admin_route_has_an_explicit_rule() {
        let routes = router_routes();
        assert!(routes.len() >= 80, "only {} routes found", routes.len());
        let doc = serde_json::to_value(crate::openapi_doc::spec()).unwrap();
        let mut documented = Vec::new();
        for (path, item) in doc["paths"].as_object().unwrap() {
            if !path.starts_with("/__admin/") || path.starts_with("/__admin/auth/") {
                continue;
            }
            for m in ["get", "put", "post", "delete", "patch"] {
                if item.get(m).is_some() {
                    documented.push((m.to_ascii_uppercase(), path.clone()));
                }
            }
        }
        assert!(
            documented.len() >= 80,
            "only {} documented",
            documented.len()
        );
        for (m, path) in routes.iter().chain(documented.iter()) {
            let method = Method::from_bytes(m.as_bytes()).unwrap();
            assert!(
                route_rule(&method, &concrete(path)).is_some(),
                "{m} {path} has no explicit rule in route_rule"
            );
        }
        for (m, path) in &routes {
            assert!(
                documented.contains(&(m.clone(), path.clone())),
                "{m} {path} is routed but missing from the OpenAPI document"
            );
        }
    }
}
