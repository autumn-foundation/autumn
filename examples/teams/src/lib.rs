//! Organization membership, roles, and email invitations for Autumn
//! (issue #1261).
//!
//! Composed entirely from shipped primitives: session auth, row-level
//! multi-tenancy (`#[repository(tenant_scoped)]` — issue #695, with the
//! *active organization* as the tenant), policy/session-based authorization
//! (issue #496, with `role.rs`'s `require_role` as a hierarchy-aware guard
//! over the same session `"role"` key), and the Mail stack (`#[mailer]`) for
//! the invite email.

pub mod mailers;
pub mod models;
pub mod repositories;
pub mod role;
pub mod routes;
pub mod schema;

use autumn_web::prelude::*;

/// Root: signed-in users go to their member list, everyone else to login.
#[get("/")]
pub async fn index(session: Session) -> Redirect {
    if session.get("user_id").await.is_some() {
        Redirect::to("/members")
    } else {
        Redirect::to("/login")
    }
}
