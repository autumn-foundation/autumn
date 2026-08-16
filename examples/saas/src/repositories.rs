//! Data-access repositories.
//!
//! `#[repository(Project, tenant_scoped)]` generates `PgProjectRepository` with
//! the usual CRUD methods (`find_all`, `save`, `find_by_id`, …). Because it is
//! `tenant_scoped`, every read is filtered by the current tenant and every
//! insert stamps the current tenant id — enforced at the SQL level, so a tenant
//! can never read or write another tenant's rows.

use crate::models::{
    NewPasswordResetToken, NewProject, PasswordResetToken, Project, UpdatePasswordResetToken,
    UpdateProject,
};
use crate::schema::{password_reset_tokens, projects};

#[autumn_web::repository(Project, table = "projects", tenant_scoped)]
pub trait ProjectRepository {
    /// Find this tenant's projects by name.
    fn find_by_name(name: String) -> Vec<Project>;
}

/// Retention-sweeps demo (issue #1342): `retention(after = "1d", basis =
/// created_at)` is the entire policy declaration — Autumn compiles it into a
/// batched, fleet-coordinated sweep with no `#[scheduled]` fn and no SQL. See
/// docs/guide/retention-sweeps.md.
#[autumn_web::repository(
    PasswordResetToken,
    table = "password_reset_tokens",
    tenant_scoped,
    retention(after = "1d", basis = created_at)
)]
pub trait PasswordResetTokenRepository {}
