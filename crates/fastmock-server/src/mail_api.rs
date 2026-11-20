//! Admin API for captured mail (`/__admin/mail/...`, phase B2). Handlers reach the mail module through
//! [`crate::state::AppState::mail`] (absent while `FASTMOCK_MAIL_ENABLED` is off) and select areas with
//! [`crate::spaces::select`] and [`crate::spaces::target`] using the `mail:*` permissions, whose route
//! rules are already in [`crate::access::route_rule`].

use axum::Router;

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
}
