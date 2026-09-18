//! The tenant-scoped dashboard.
//!
//! Tenancy is middleware-driven (see `autumn.toml`): the framework resolves the
//! tenant from the session on every non-public request and establishes the
//! tenant context that the `tenant_scoped` `PgProjectRepository` filters by — so
//! these handlers just query the repository and only ever see the signed-in
//! organisation's projects. The `Tenant` extractor surfaces the resolved id for
//! display; an unauthenticated visitor is redirected to `/login` by the
//! middleware before reaching here.

use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;

use crate::models::{NewProject, NewProjectForm, Project};
use crate::repositories::{PgProjectRepository, ProjectRepository, cached_project_count};

use super::layout::layout;

/// Render the dashboard: the project list plus the create-project form.
///
/// `name` re-populates the input and `error`, when present, is shown adjacent
/// to the form on a rejected submission (Wayfinder: error-path inventory) —
/// `create_project` below used to bounce a validation failure straight to the
/// framework's generic error page, losing the name the user had just typed
/// and dropping them off the project list entirely.
fn dashboard_page(
    tenant_id: &str,
    total: i64,
    projects: &[Project],
    name: &str,
    error: Option<&str>,
) -> Markup {
    layout(
        "Dashboard",
        true,
        html! {
            div class="flex items-center justify-between mb-6" {
                h1 class="text-2xl font-bold" { "Projects" }
                span class="text-sm text-gray-500" {
                    (total) " total · tenant: " code { (tenant_id) }
                }
            }

            @if let Some(error) = error {
                p class="mb-4 text-sm text-red-600" role="alert" { (error) }
            }

            form action="/dashboard/projects" method="post"
                 class="flex gap-2 mb-6 bg-white rounded-lg shadow p-4" {
                input name="name" value=(name) required placeholder="New project name" aria-label="New project name"
                      aria-invalid=[error.is_some().then(|| "true")]
                      class="flex-1 border rounded px-3 py-2";
                button type="submit"
                       class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                    "Create"
                }
            }

            ul class="space-y-2" {
                @for project in projects {
                    li class="bg-white rounded-lg shadow p-4 flex items-center justify-between" {
                        span class="font-medium" { (project.name) }
                        span class="text-xs text-gray-400" { (project.created_at.format("%Y-%m-%d %H:%M").to_string()) }
                    }
                }
                @if projects.is_empty() {
                    li class="text-gray-400 text-center py-8" { "No projects yet — create your first above." }
                }
            }
        },
    )
}

#[get("/dashboard")]
pub async fn dashboard(
    Tenant(tenant_id): Tenant,
    repo: PgProjectRepository,
) -> AutumnResult<Response> {
    // The tenant context is already established by the tenancy middleware, so the
    // tenant_scoped repository filters by it automatically.
    let projects = repo.find_all().await?;
    // A cached read (#1716): memoized for 30s and derived from `Project`, so
    // every write through `ProjectRepository` could strand it. The repository's
    // `invalidates(cached_project_count)` clause is what makes the build accept
    // that — and `create_project` below is what makes it true at runtime.
    let total = cached_project_count(tenant_id.clone(), &repo).await?;

    let page = dashboard_page(&tenant_id, total, &projects, "", None);
    Ok(page.into_response())
}

#[post("/dashboard/projects")]
pub async fn create_project(
    Tenant(tenant_id): Tenant,
    repo: PgProjectRepository,
    Form(form): Form<NewProjectForm>,
) -> AutumnResult<Response> {
    let name = form.name.trim().to_owned();
    // Mirror the `#[validate(length(min = 1, max = 200))]` constraint on the
    // Project model so the route rejects out-of-range names before saving.
    if name.is_empty() || name.chars().count() > 200 {
        // Redisplay the dashboard inline (HTTP 200) with the rejected name still
        // in the field and the project list intact, instead of navigating the
        // user away to the framework's generic error page (Wayfinder:
        // error-path inventory).
        let projects = repo.find_all().await?;
        let total = cached_project_count(tenant_id.clone(), &repo).await?;
        let page = dashboard_page(
            &tenant_id,
            total,
            &projects,
            &name,
            Some("Project name must be between 1 and 200 characters"),
        );
        return Ok(page.into_response());
    }

    // The tenant_id is stamped by the tenant_scoped repository from the context
    // established by the tenancy middleware, so it is not part of `NewProject`.
    repo.save(&NewProject { name }).await?;
    // Discharge the invalidation the repository declares. The build proves the
    // edge exists and names a real cached read; calling it is what makes the
    // next dashboard render show the new count instead of the 30s-old one.
    //
    // The return value is not decoration: `false` means the configured cache
    // backend could not drop the namespace, so the old count is still being
    // served and the dashboard will lie for up to the 30s TTL.
    if !PgProjectRepository::invalidate_declared_caches() {
        autumn_web::reexports::tracing::warn!(
            "cache backend cannot invalidate by namespace; the project count may be stale \
             until its TTL expires"
        );
    }
    Ok(Redirect::to("/dashboard").into_response())
}
