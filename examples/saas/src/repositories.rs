//! Data-access repositories.
//!
//! `#[repository(Project, tenant_scoped)]` generates `PgProjectRepository` with
//! the usual CRUD methods (`find_all`, `save`, `find_by_id`, …). Because it is
//! `tenant_scoped`, every read is filtered by the current tenant and every
//! insert stamps the current tenant id — enforced at the SQL level, so a tenant
//! can never read or write another tenant's rows.
//!
//! # Build-time cache coherence (issue #1716)
//!
//! [`cached_project_count`] memoizes a `Project` read for 30 seconds, so every
//! write through `ProjectRepository` can leave it stale. The
//! `invalidates(cached_project_count)` clause on the repository is what
//! discharges that obligation: it declares the edge, resolves — at compile
//! time — to the identity constant `#[cached]` generates beside the function,
//! and generates [`PgProjectRepository::invalidate_declared_caches`] for the
//! write paths to call.
//!
//! Delete that one clause and `autumn cache audit` fails the build, naming the
//! read, the write and the `Project` model they share. `tests/cache_coherence.rs`
//! proves both halves. See the Cache Coherence guide:
//! <https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/cache-coherence.md>

use crate::models::{
    NewPasswordResetToken, NewProject, PasswordResetToken, Project, UpdatePasswordResetToken,
    UpdateProject,
};
use crate::schema::{password_reset_tokens, projects};

#[autumn_web::repository(
    Project,
    table = "projects",
    tenant_scoped,
    invalidates(cached_project_count)
)]
pub trait ProjectRepository {
    /// Find this tenant's projects by name.
    fn find_by_name(name: String) -> Vec<Project>;
}

/// This tenant's project count, memoized for 30 seconds.
///
/// The dashboard header renders it on every request, and it changes only when a
/// project is created or deleted — the textbook case for a cached read, and the
/// textbook place to strand stale data.
///
/// `key(tenant_id)` keeps the repository handle out of the cache key: the
/// handle is per-request and is not part of the value's identity, the tenant is.
/// `reads(Project)` declares what the value is derived from, which is what the
/// build-time coherence gate proves against every `ProjectRepository` write.
#[autumn_web::cached(ttl = "30s", key(tenant_id), reads(Project), result)]
pub async fn cached_project_count(
    tenant_id: String,
    repo: &PgProjectRepository,
) -> autumn_web::AutumnResult<i64> {
    // `tenant_id` is the cache key, not a query input: the tenant_scoped
    // repository already filters by the ambient tenant context. Keying on it is
    // what stops one tenant being served another's count.
    repo.count().await
}

/// Retention-sweeps demo (issue #1342): `retention(after = "1d", basis =
/// created_at)` is the entire policy declaration — Autumn compiles it into a
/// batched, fleet-coordinated sweep with no `#[scheduled]` fn and no SQL. See
/// docs/guide/retention-sweeps.md.
///
/// Combined with `tenant_scoped` here on purpose: the sweep is a background
/// maintenance job with no tenant context, so it purges every tenant's
/// expired tokens on each run, not just one — the intended behavior for a
/// cleanup job, not a tenant-isolation gap. See "tenant_scoped Repositories"
/// in the retention-sweeps guide.
#[autumn_web::repository(
    PasswordResetToken,
    table = "password_reset_tokens",
    tenant_scoped,
    retention(after = "1d", basis = created_at)
)]
pub trait PasswordResetTokenRepository {}
