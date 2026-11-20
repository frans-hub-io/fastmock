use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, bail};
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Global,
    Space,
}

use Permission::*;

pub const ALL_PERMISSIONS: [Permission; 27] = [
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
            | GrpcWrite | InfoRead | AuditRead | SystemReset => ScopeKind::Global,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingScope {
    Global,
    Spaces(Vec<String>),
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
    spaces: BTreeMap<String, SpaceDef>,
    bindings: Vec<(Subject, Binding)>,
}

fn parse_permissions(list: &[String]) -> anyhow::Result<PermSet> {
    if list.is_empty() {
        bail!("permissions must not be empty");
    }
    let mut set = PermSet::default();
    for raw in list {
        let p = raw.trim();
        if let Some(resource) = p.strip_suffix(":*") {
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
            set.insert(
                Permission::parse(p).ok_or_else(|| anyhow::anyhow!("unknown permission `{p}`"))?,
            );
        }
    }
    Ok(set)
}

fn check_prefix(p: &str) -> anyhow::Result<()> {
    if !p.starts_with('/')
        || p.len() > 512
        || p.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("path prefix `{p}` must start with `/`, have no spaces and be at most 512 bytes");
    }
    if p.contains('?') || p.contains('#') || p.split('/').any(|s| s == "..") {
        bail!("path prefix `{p}` must be a plain path");
    }
    Ok(())
}

fn check_host(h: &str) -> anyhow::Result<String> {
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
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the access file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("access file {}", path.display()))
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
            let perms =
                parse_permissions(&r.permissions).map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            cfg.roles.insert(r.name.clone(), perms);
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
            cfg.spaces.insert(
                s.name.clone(),
                SpaceDef {
                    name: s.name,
                    description: s.description,
                    path_prefixes: s.path_prefixes,
                    hosts,
                },
            );
        }
        for (i, b) in doc.bindings.into_iter().enumerate() {
            let at = format!("bindings[{i}] ({})", b.subject);
            let subject = Subject::parse(&b.subject).map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            let perms = cfg
                .role(&b.role)
                .ok_or_else(|| anyhow::anyhow!("{at}: unknown role `{}`", b.role))?;
            let scope = match (b.global.unwrap_or(false), b.spaces) {
                (true, None) => BindingScope::Global,
                (false, Some(list)) if !list.is_empty() => {
                    let mut seen = BTreeSet::new();
                    for s in &list {
                        if !cfg.spaces.contains_key(s) {
                            bail!("{at}: unknown space `{s}`");
                        }
                        if !seen.insert(s.clone()) {
                            bail!("{at}: space `{s}` listed twice");
                        }
                    }
                    BindingScope::Spaces(list)
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

    /// Permissions of a built-in or custom role.
    pub fn role(&self, name: &str) -> Option<PermSet> {
        builtin_role(name).or_else(|| self.roles.get(name).copied())
    }

    pub fn space(&self, name: &str) -> Option<&SpaceDef> {
        self.spaces.get(name)
    }

    pub fn spaces(&self) -> impl Iterator<Item = &SpaceDef> {
        self.spaces.values()
    }

    pub fn has_spaces(&self) -> bool {
        !self.spaces.is_empty()
    }

    pub fn token_subjects(&self) -> impl Iterator<Item = &str> {
        self.bindings.iter().filter_map(|(s, _)| match s {
            Subject::Token(n) => Some(n.as_str()),
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
}

impl Grants {
    pub fn from_bindings(bindings: &[Binding]) -> Self {
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
        }
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
    /// Permissions granted only in single spaces, by space; the effective permissions in a space are these plus the global ones.
    #[schema(example = json!({"payments": ["stubs:write", "journal:read"]}))]
    pub spaces: BTreeMap<String, Vec<String>>,
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
        }
        .serialize(s)
    }
}

/// Permission and scope kind a route needs before its handler runs. Handlers of space-scoped routes
/// check the space they act on.
pub fn route_rule(method: &Method, full_path: &str) -> (Permission, ScopeKind) {
    let path = full_path.strip_prefix("/__admin").unwrap_or(full_path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let m = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    let natural = |p: Permission| (p, p.scope());
    let global = |p: Permission| (p, ScopeKind::Global);
    let fallback = || {
        if m == "GET" {
            global(InfoRead)
        } else {
            global(StubsWrite)
        }
    };
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
        ("GET", ["plugins", _, ..]) => global(StubsRead),
        (_, ["plugins", _, ..]) => global(StubsWrite),
        _ => fallback(),
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
                is_read && p != AuditRead && p != TokensRead,
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
        ] {
            assert!(!e.contains(p), "{p}");
        }
        assert_eq!(builtin_role("admin"), Some(PermSet::all()));
        assert_eq!(minimal_role(StubsRead), "viewer");
        assert_eq!(minimal_role(StubsWrite), "editor");
        assert_eq!(minimal_role(StubsPurge), "admin");
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
    fn grants_follow_bindings() {
        let c = AccessConfig::parse(EXAMPLE).unwrap();
        let g = Grants::from_bindings(&c.bindings_for(PrincipalKind::Token, "ci-billing", &[]));
        assert!(g.allowed(StubsWrite, Some("billing")));
        assert!(!g.allowed(StubsWrite, Some("payments")));
        assert!(!g.allowed(StubsWrite, None));
        assert!(g.allowed(InfoRead, None));
        assert!(g.anywhere(StubsRead));
        assert!(!g.anywhere(FilesRead));
        assert_eq!(g.spaces_with(StubsRead), vec!["billing"]);
        assert!(!g.sees_all_spaces());
        let admin_in_space = Grants::from_bindings(&[Binding {
            role: "admin".into(),
            permissions: PermSet::all(),
            scope: BindingScope::Spaces(vec!["payments".into()]),
        }]);
        assert!(!admin_in_space.allowed(StubsPurge, Some("payments")));
        assert!(!admin_in_space.allowed(AuditRead, None));
        assert!(admin_in_space.allowed(TokensWrite, Some("payments")));
        let viewer = Grants::from_bindings(&[Binding::global("viewer", viewer_permissions())]);
        assert!(viewer.allowed(StubsRead, Some("payments")));
        assert!(viewer.sees_all_spaces());
        assert!(Grants::from_bindings(&[]).global().is_empty());
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
        let r = |m: Method, p: &str| route_rule(&m, p);
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
            r(Method::GET, "/__admin/nope"),
            (InfoRead, ScopeKind::Global)
        );
        assert_eq!(
            r(Method::OPTIONS, "/__admin/stubs"),
            (StubsWrite, ScopeKind::Global)
        );
    }
}
