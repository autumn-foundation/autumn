use diesel::prelude::{Insertable, Queryable, Selectable};
use serde::Deserialize;

use crate::schema::{invitations, memberships, organizations, users};

// ── User ────────────────────────────────────────────────────────────────────
//
// Plain Diesel structs (not a tenant-scoped repository): a user must be found
// by email *before* any organization is known, and a user can belong to many
// organizations (via `Membership`), so the row itself carries no tenant id.

/// An account row.
#[derive(Queryable, Selectable, Clone)]
#[diesel(table_name = users)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct User {
    pub id: i64,
    pub email: String,
    pub password_hash: String,
    pub created_at: chrono::NaiveDateTime,
}

/// Data needed to create a new account.
#[derive(Insertable)]
#[diesel(table_name = users)]
pub struct NewUser {
    pub email: String,
    pub password_hash: String,
}

// ── Organization ─────────────────────────────────────────────────────────────
//
// The tenant itself. Not tenant-scoped: creating one, and listing the ones a
// user belongs to, both necessarily run before/across any single active
// tenant.

/// An organization (tenant).
#[autumn_web::model(table = "organizations")]
pub struct Organization {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 200))]
    pub name: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Membership ───────────────────────────────────────────────────────────────
//
// The user <-> organization join row, carrying the closed `Role` (stored as
// `TEXT`, validated by `crate::role::Role::parse` everywhere it's read).
// `tenant_scoped` fills `organization_id` from the active-organization
// session context and filters every read by it — reusing #695, not a second
// isolation mechanism.

/// A user's role within a single organization.
///
/// `tenant_id` holds the owning `Organization.id` in its string form (the
/// `tenant_scoped` repository macro's generated queries filter on a column
/// literally named `tenant_id` — see `migrations/`); look the `Organization`
/// row itself up via `tenant_id.parse::<i64>()` when needed.
#[autumn_web::model(table = "memberships")]
pub struct Membership {
    #[id]
    pub id: i64,
    #[default]
    pub tenant_id: String,
    pub user_id: i64,
    pub role: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Invitation ───────────────────────────────────────────────────────────────

/// A pending (or resolved) email invitation into an organization. See
/// [`Membership`]'s doc comment for why `tenant_id` is a `String`.
#[autumn_web::model(table = "invitations")]
pub struct Invitation {
    #[id]
    pub id: i64,
    #[default]
    pub tenant_id: String,
    pub email: String,
    pub role: String,
    pub token_hash: String,
    // Not `#[default]`: revoking/resending/accepting need to write this
    // field through `UpdateInvitation`, and `#[default]` fields are excluded
    // from the generated `Update*` struct as well as `New*`. The DB column's
    // `DEFAULT 'pending'` is a redundant safety net; every insert site here
    // also sets it explicitly.
    pub status: String,
    pub invited_by_user_id: i64,
    pub expires_at: chrono::NaiveDateTime,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Forms ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignupForm {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Deserialize)]
pub struct NextQuery {
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Deserialize)]
pub struct NewOrganizationForm {
    pub name: String,
}

#[derive(Deserialize)]
pub struct InviteForm {
    pub email: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct ChangeRoleForm {
    pub role: String,
}
