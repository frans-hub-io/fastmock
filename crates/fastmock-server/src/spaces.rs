use axum::http::StatusCode;
use fastmock_core::ownership::under;
use fastmock_core::{SpaceFilter, Stub};
use serde::Deserialize;

use crate::access::{Permission, SpaceDef};
use crate::admin::{ApiError, ApiResult, bad, not_found, require};
use crate::auth::Principal;
use crate::state::AppState;

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SpaceQuery {
    /// Space to act on. Without it, reads cover everything the caller may read and writes go to the global
    /// area, or to the caller's only space when it is bound to spaces.
    pub space: Option<String>,
}

/// Whether the principal may see a space at all: it holds some permission everywhere, or a binding
/// names the space.
pub fn visible(p: &Principal, space: &str) -> bool {
    p.permissions.sees_all_spaces()
        || p.permissions
            .bound_spaces()
            .any(|(n, _)| n.as_str() == space)
}

/// A configured space the principal may see; other names answer 404 alike.
pub fn known(app: &AppState, p: &Principal, space: &str) -> ApiResult<()> {
    if app.auth.access().space(space).is_none() || !visible(p, space) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("space {space} not found"),
        ));
    }
    Ok(())
}

/// What a read (or a deletion that selects existing items) covers: the requested space, everything for a
/// principal holding the permission globally, otherwise the spaces where it holds the permission.
pub fn select(
    app: &AppState,
    p: &Principal,
    perm: Permission,
    requested: Option<&str>,
) -> ApiResult<SpaceFilter> {
    if let Some(s) = requested {
        known(app, p, s)?;
        require(p, perm, Some(s))?;
        return Ok(SpaceFilter::In(vec![s.to_string()]));
    }
    if p.permissions.global_has(perm) {
        return Ok(SpaceFilter::Any);
    }
    let list = p.permissions.spaces_with(perm);
    if list.is_empty() {
        require(p, perm, None)?;
    }
    Ok(SpaceFilter::In(list))
}

/// The one area a write or a single-entry operation acts on: the requested space, the global area for a
/// principal holding the permission globally, or the only space of a principal bound to spaces.
pub fn target(
    app: &AppState,
    p: &Principal,
    perm: Permission,
    requested: Option<&str>,
) -> ApiResult<Option<String>> {
    if let Some(s) = requested {
        known(app, p, s)?;
        require(p, perm, Some(s))?;
        return Ok(Some(s.to_string()));
    }
    if p.permissions.global_has(perm) {
        return Ok(None);
    }
    let mut list = p.permissions.spaces_with(perm);
    match list.len() {
        0 => {
            require(p, perm, None)?;
            Ok(None)
        }
        1 => Ok(list.pop()),
        _ => Err(bad(format!(
            "several spaces are bound ({}); set space",
            list.join(", ")
        ))),
    }
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ImportQuery {
    /// Space the stubs go to. Without it, the global area, or the caller's only space when it is bound to
    /// spaces.
    pub space: Option<String>,
    /// `request.host` for every imported stub that has none. For a space that owns hosts it must be one of
    /// them; it defaults to the space's only host and is required when the space owns several.
    pub host: Option<String>,
}

/// The host that imported stubs without a `request.host` matcher get: the `host` parameter (one of the
/// space's hosts when the space owns hosts), else the only host of the space.
pub fn import_host(
    app: &AppState,
    space: Option<&str>,
    host: Option<&str>,
) -> ApiResult<Option<String>> {
    let owned: &[String] = space
        .and_then(|s| app.auth.access().space(s))
        .map(|d| d.hosts.as_slice())
        .unwrap_or_default();
    let name = space.unwrap_or_default();
    let Some(raw) = host.map(str::trim).filter(|h| !h.is_empty()) else {
        return match owned {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            many => Err(bad(format!(
                "space {name} owns several hosts ({}): set the host parameter to the one the imported stubs answer",
                many.join(", ")
            ))),
        };
    };
    let h = crate::access::check_host(raw).map_err(|e| bad(e.to_string()))?;
    if !owned.is_empty() && !owned.contains(&h) {
        return Err(bad(format!(
            "host {raw} is not a host of space {name}; use one of {}",
            owned.join(", ")
        )));
    }
    Ok(Some(h))
}

/// Gives a stub without a `request.host` matcher the import host.
pub fn set_import_host(stub: &mut Stub, host: Option<&str>) {
    if let Some(h) = host
        && stub.request.host.is_none()
    {
        stub.request.host = Some(fastmock_core::model::StringMatcher {
            equal_to: Some(h.to_string()),
            ..Default::default()
        });
    }
}

/// Access to one existing item that lives in `space`: 404 when the caller cannot even read there, 403
/// when it can read but lacks `perm`.
pub fn item(
    p: &Principal,
    read: Permission,
    perm: Permission,
    space: Option<&str>,
    what: &str,
) -> ApiResult<()> {
    if p.allowed(perm, space) {
        return Ok(());
    }
    if !p.allowed(read, space) {
        return Err(not_found(what));
    }
    require(p, perm, space)
}

/// Ownership rules of a space: with `pathPrefixes`, the URL matcher must be an exact path or a path
/// template under one of them; with `hosts`, the `Host` matcher must equal one of them.
pub fn ownership(def: &SpaceDef, stub: &Stub) -> Result<(), String> {
    let r = &stub.request;
    let name = &def.name;
    if !def.path_prefixes.is_empty() {
        let owned = def.path_prefixes.join(", ");
        if r.url_pattern.is_some() || r.url_path_pattern.is_some() {
            return Err(format!(
                "space {name} owns {owned}: regular expression URL matchers (urlPattern, urlPathPattern) are not allowed there"
            ));
        }
        let paths: Vec<&str> = [
            r.url
                .as_deref()
                .map(|u| u.split(['?', '#']).next().unwrap_or(u)),
            r.url_path.as_deref(),
            r.url_path_template.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if paths.is_empty() {
            return Err(format!(
                "space {name} owns {owned}: the stub needs a url, urlPath or urlPathTemplate under one of them"
            ));
        }
        for path in paths {
            if fastmock_core::ownership::has_dot_segment(path) {
                return Err(format!(
                    "space {name} owns {owned}: {path} has a `.` or `..` segment; use the normalised path"
                ));
            }
            if fastmock_core::ownership::has_encoded_separator(path) {
                return Err(format!(
                    "space {name} owns {owned}: {path} has an encoded slash or backslash (%2F, %5C) or a backslash; use the plain path"
                ));
            }
            if !def.path_prefixes.iter().any(|pre| under(path, pre)) {
                return Err(format!(
                    "space {name} owns {owned}: {path} lies outside these prefixes"
                ));
            }
        }
    }
    if !def.hosts.is_empty() {
        let owned = def.hosts.join(", ");
        let ok = r.host.as_ref().is_some_and(|m| {
            m.equal_to.as_ref().is_some_and(|h| {
                let h = h.trim_end_matches('.').to_ascii_lowercase();
                def.hosts.contains(&h)
                    && *m
                        == fastmock_core::model::StringMatcher {
                            equal_to: m.equal_to.clone(),
                            case_insensitive: m.case_insensitive,
                            ..Default::default()
                        }
            })
        });
        if !ok {
            return Err(format!(
                "space {name} owns hosts {owned}: the stub needs request.host with equalTo one of them"
            ));
        }
    }
    Ok(())
}

/// A stored stub that breaks the rules of its space: outside its path prefixes or hosts, or in a space
/// the access file does not define. Stubs are checked when they are written, so this happens only when the
/// access file changes later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnershipProblem {
    pub stub_id: uuid::Uuid,
    pub space: String,
    pub reason: String,
}

impl std::fmt::Display for OwnershipProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stub {} in space {}: {}",
            self.stub_id, self.space, self.reason
        )
    }
}

pub fn ownership_problems<'a>(
    access: &crate::access::AccessConfig,
    stubs: impl Iterator<Item = &'a Stub>,
) -> Vec<OwnershipProblem> {
    stubs
        .filter_map(|stub| {
            let space = stub.space.as_ref()?;
            let reason = match access.space(space) {
                None => format!("space {space} is not defined in the access file"),
                Some(def) => ownership(def, stub).err()?,
            };
            Some(OwnershipProblem {
                stub_id: stub.id,
                space: space.clone(),
                reason,
            })
        })
        .collect()
}

/// Validation of a stub before it is stored: the engine rules and the ownership rules of its space.
pub fn check_stub(app: &AppState, stub: &Stub) -> Result<(), String> {
    app.engine.validate(stub)?;
    if let Some(space) = &stub.space {
        let def = app
            .auth
            .access()
            .space(space)
            .ok_or_else(|| format!("space {space} not found"))?;
        ownership(def, stub)?;
    }
    Ok(())
}

/// Rejects stubs whose id already belongs to a stub in another area, so that an import cannot move or
/// overwrite stubs of other spaces.
pub async fn check_ids(app: &AppState, stubs: &[Stub], space: Option<&str>) -> ApiResult<()> {
    let existing = app.stores.stubs.list().await?;
    let areas: std::collections::HashMap<uuid::Uuid, Option<&str>> = existing
        .iter()
        .map(|s| (s.id, s.space.as_deref()))
        .collect();
    for (i, s) in stubs.iter().enumerate() {
        if let Some(other) = areas.get(&s.id)
            && *other != space
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("stubs[{i}]: id {} is already in use", s.id),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(prefixes: &[&str], hosts: &[&str]) -> SpaceDef {
        SpaceDef {
            name: "payments".into(),
            description: None,
            path_prefixes: prefixes.iter().map(|s| s.to_string()).collect(),
            hosts: hosts.iter().map(|s| s.to_string()).collect(),
            mail_domains: vec![],
        }
    }

    fn stub(request: serde_json::Value) -> Stub {
        serde_json::from_value(json!({"request": request})).unwrap()
    }

    #[test]
    fn ownership_rules() {
        let d = def(&["/payments/", "/psp"], &[]);
        for ok in [
            json!({"urlPath": "/payments/x"}),
            json!({"url": "/psp/a?b=1"}),
            json!({"urlPathTemplate": "/payments/{id}"}),
            json!({"urlPath": "/psp"}),
            json!({"urlPath": "/payments/...x/.hidden/a..b"}),
            json!({"url": "/payments/x?next=../y"}),
            json!({"url": "/payments/x?next=%2Fy"}),
        ] {
            assert!(ownership(&d, &stub(ok.clone())).is_ok(), "{ok}");
        }
        for bad in [
            json!({"urlPath": "/paymentsx"}),
            json!({"urlPath": "/billing/x"}),
            json!({"urlPathPattern": "/payments/.*"}),
            json!({"urlPattern": "/payments/.*"}),
            json!({"method": "GET"}),
            json!({"urlPathTemplate": "/{tenant}/payments"}),
            json!({"url": "/payments/../billing/x"}),
            json!({"url": "/payments/./x?a=/.."}),
            json!({"urlPath": "/payments/%2e%2e/billing/y"}),
            json!({"urlPath": "/payments/%2E%2e/billing/y"}),
            json!({"urlPath": "/payments/.%2E/billing/y"}),
            json!({"urlPath": "/payments/%2e"}),
            json!({"urlPath": "/payments/x/.."}),
            json!({"urlPathTemplate": "/payments/../billing/{id}"}),
            json!({"urlPathTemplate": "/psp/%2E%2E/{id}"}),
            json!({"url": "/payments/..%2fbilling/x"}),
            json!({"urlPath": "/payments/..%2Fbilling/x"}),
            json!({"urlPath": "/payments/a%5cb"}),
            json!({"urlPath": "/payments/a%5Cb"}),
            json!({"urlPath": "/payments\\..\\billing/x"}),
            json!({"urlPathTemplate": "/psp/%2f/{id}"}),
        ] {
            assert!(ownership(&d, &stub(bad.clone())).is_err(), "{bad}");
        }
        assert!(ownership(&def(&[], &[]), &stub(json!({}))).is_ok());
        let h = def(&[], &["psp.example.com"]);
        assert!(ownership(&h, &stub(json!({"host": {"equalTo": "PSP.example.com"}}))).is_ok());
        assert!(ownership(&h, &stub(json!({"host": {"equalTo": "other.example.com"}}))).is_err());
        assert!(ownership(&h, &stub(json!({"host": {"matches": ".*"}}))).is_err());
        assert!(
            ownership(
                &h,
                &stub(json!({"host": {"equalTo": "psp.example.com", "contains": "x"}}))
            )
            .is_err()
        );
        assert!(ownership(&h, &stub(json!({"urlPath": "/x"}))).is_err());
    }
}
