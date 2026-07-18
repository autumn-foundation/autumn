//! A multi-tenant SaaS starter for Autumn.
//!
//! Signup → login → a tenant-scoped dashboard, composed from shipped Autumn
//! primitives: session auth (`Session` + bcrypt), and row-level multi-tenancy
//! (`#[repository(tenant_scoped)]` + `with_tenant`). Exposed as a library so the
//! integration tests in `tests/` can drive the real routes.

pub mod models;
pub mod remember;
pub mod repositories;
pub mod routes;
pub mod schema;

use autumn_web::prelude::*;

/// Root: signed-in users go to their dashboard, everyone else to login.
#[get("/")]
pub async fn index(session: Session) -> Redirect {
    if session.get("user_id").await.is_some() {
        Redirect::to("/dashboard")
    } else {
        Redirect::to("/login")
    }
}
