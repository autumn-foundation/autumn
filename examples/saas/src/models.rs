use diesel::prelude::{Insertable, Queryable, Selectable};
use serde::Deserialize;

use crate::schema::{projects, users};

// ── User ────────────────────────────────────────────────────────────────────
//
// Plain Diesel structs (not a tenant-scoped repository): a user must be found
// by email *before* we know which tenant they belong to, so user lookups
// deliberately run outside any tenant scope.

/// An account row.
#[derive(Queryable, Selectable)]
#[diesel(table_name = users)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct User {
    pub id: i64,
    pub email: String,
    pub password_hash: String,
    pub tenant_id: String,
    pub created_at: chrono::NaiveDateTime,
}

/// Data needed to create a new account.
#[derive(Insertable)]
#[diesel(table_name = users)]
pub struct NewUser {
    pub email: String,
    pub password_hash: String,
    pub tenant_id: String,
}

// ── Project ──────────────────────────────────────────────────────────────────
//
// The tenant-scoped domain model. `#[default] tenant_id` is filled in by the
// `tenant_scoped` repository from the current tenant context on insert, so it
// is omitted from the generated `NewProject` insert struct.

/// A project belonging to a single tenant.
#[autumn_web::model(table = "projects")]
pub struct Project {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 200))]
    pub name: String,
    #[default]
    pub tenant_id: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

/// Form body for creating a project. The tenant is taken from the session, not
/// the form, so it is intentionally absent here.
#[derive(Deserialize)]
pub struct NewProjectForm {
    pub name: String,
}

// ── PasswordResetToken ───────────────────────────────────────────────────────
//
// A retention-sweeps demo (issue #1342): a reset token is only useful for
// roughly a day, so nobody should have to hand-write a `#[scheduled]` job
// and a batched DELETE to keep this table from growing forever. See the
// `retention(after = "1d", basis = created_at)` on `PasswordResetTokenRepository`
// in repositories.rs, and docs/guide/retention-sweeps.md.

/// A single-use password-reset token.
#[autumn_web::model(table = "password_reset_tokens")]
pub struct PasswordResetToken {
    #[id]
    pub id: i64,
    #[default]
    pub tenant_id: String,
    pub user_id: i64,
    pub token_hash: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
