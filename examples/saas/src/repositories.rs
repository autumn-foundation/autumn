//! Data-access repositories.
//!
//! `#[repository(Project, tenant_scoped)]` generates `PgProjectRepository` with
//! the usual CRUD methods (`find_all`, `save`, `find_by_id`, …). Because it is
//! `tenant_scoped`, every read is filtered by the current tenant and every
//! insert stamps the current tenant id — enforced at the SQL level, so a tenant
//! can never read or write another tenant's rows.

use crate::models::{NewProject, Project, UpdateProject};
use crate::schema::projects;

#[autumn_web::repository(Project, table = "projects", tenant_scoped)]
pub trait ProjectRepository {
    /// Find this tenant's projects by name.
    fn find_by_name(name: String) -> Vec<Project>;
}
