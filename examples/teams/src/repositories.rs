//! Data-access repositories.
//!
//! `Organization` is not tenant-scoped — it *is* the tenant, so creating one
//! and listing the ones a user belongs to both necessarily run outside any
//! single active tenant.
//!
//! `Membership` and `Invitation` are `tenant_scoped` by the active
//! organization (`organization_id`, resolved from the session — see
//! `autumn.toml`). Every read/write against them is filtered/stamped by it
//! automatically. The few places that must legitimately cross organizations
//! (resolving *all* of a user's memberships to pick one at login; looking up
//! an invitation by its token before any organization is active) use the
//! explicit, auditable `across_tenants()` escape hatch from issue #695 rather
//! than a second isolation mechanism.

use crate::models::{
    Invitation, Membership, NewInvitation, NewMembership, NewOrganization, Organization,
    UpdateInvitation, UpdateMembership, UpdateOrganization,
};
use crate::schema::{invitations, memberships, organizations};

#[autumn_web::repository(Organization, table = "organizations")]
pub trait OrganizationRepository {}

#[autumn_web::repository(Membership, table = "memberships", tenant_scoped)]
pub trait MembershipRepository {
    /// All of a user's memberships, across every organization. Always called
    /// via `.across_tenants()` — resolving which organizations a user
    /// belongs to has to run before any one of them is "the" active tenant.
    fn find_by_user_id(user_id: i64) -> Vec<Membership>;
}

#[autumn_web::repository(Invitation, table = "invitations", tenant_scoped)]
pub trait InvitationRepository {
    /// Look up an invitation by its token hash. Always called via
    /// `.across_tenants()` — an invite accept link is followed before the
    /// visitor has an active organization (they may not even be signed in
    /// yet).
    fn find_by_token_hash(token_hash: String) -> Vec<Invitation>;
}
