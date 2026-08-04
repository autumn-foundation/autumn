//! Creating additional organizations and switching the active one.
//!
//! Resolving and switching the active organization is explicitly in scope for
//! issue #1261 ("Cross-org user switching UI polish" is not — see the issue's
//! Out of Scope section); this is the minimal, unstyled version of that.

use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::tenancy::with_tenant;

use crate::models::{Membership, NewMembership, NewOrganization};
use crate::repositories::{
    MembershipRepository, OrganizationRepository, PgMembershipRepository, PgOrganizationRepository,
};
use crate::role::Role;

use super::auth::establish_session;

/// Create a new organization; the caller becomes its `Owner` and it becomes
/// the active organization (same rule as signup — issue #1261 AC3).
#[post("/organizations")]
pub async fn create_organization(
    session: Session,
    org_repo: PgOrganizationRepository,
    membership_repo: PgMembershipRepository,
    Form(form): Form<crate::models::NewOrganizationForm>,
) -> AutumnResult<Response> {
    let Some(user_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    let name = form.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AutumnError::unprocessable_msg(
            "Organization name must be between 1 and 200 characters",
        ));
    }

    let org = org_repo.save(&NewOrganization { name }).await?;
    with_tenant(org.id.to_string(), async {
        membership_repo
            .save(&NewMembership {
                user_id,
                role: Role::Owner.as_str().to_owned(),
            })
            .await
    })
    .await?;

    establish_session(&session, user_id, &org.id.to_string(), Role::Owner).await;
    Ok(Redirect::to("/members").into_response())
}

/// Switch the active organization to one the caller already belongs to.
#[post("/organizations/{id}/switch")]
pub async fn switch_organization(
    session: Session,
    membership_repo: PgMembershipRepository,
    Path(target_org_id): Path<i64>,
) -> AutumnResult<Response> {
    let Some(user_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    let target_tenant_id = target_org_id.to_string();
    let memberships: Vec<Membership> = membership_repo
        .across_tenants()
        .find_by_user_id(user_id)
        .await?;
    let Some(membership) = memberships
        .into_iter()
        .find(|m| m.tenant_id == target_tenant_id)
    else {
        return Err(AutumnError::forbidden_msg(
            "You are not a member of that organization",
        ));
    };
    let Some(role) = Role::parse(&membership.role) else {
        return Err(AutumnError::internal_server_error_msg(
            "Corrupt membership role",
        ));
    };

    establish_session(&session, user_id, &target_tenant_id, role).await;
    Ok(Redirect::to("/members").into_response())
}
