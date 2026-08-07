//! The closed membership-role enum and the `require_role` guard.
//!
//! `Membership.role` is stored as `TEXT` (with a SQL `CHECK` constraint as a
//! defense-in-depth backstop), but every place that *reads* a role goes
//! through [`Role::parse`] — an unrecognized string can never silently pass
//! an authorization check.
//!
//! [`require_role`] resolves the caller's role by looking up their live
//! `Membership` row in the active organization — it only uses the session to
//! find *which user* is asking (`"user_id"`), the same identity signal
//! `#[secured("...")]`/`PolicyContext::has_role` rely on (issue #496), so this
//! is a hierarchy-aware guard layered on top of the existing session/Policy
//! plumbing, not a second authorization mechanism (issue #1261 AC2). Roles are
//! never trusted from the session's cached `"role"` string: that value can go
//! stale the instant another request revokes or changes the caller's
//! membership, so every check re-reads the database.

use autumn_web::prelude::*;

use crate::repositories::{MembershipRepository, PgMembershipRepository};

/// A membership role, ordered `Member < Admin < Owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Member,
    Admin,
    Owner,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            _ => None,
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Role::Member => 0,
            Role::Admin => 1,
            Role::Owner => 2,
        }
    }

    /// Whether this role satisfies a `required` role or higher in the
    /// hierarchy (e.g. an `Owner` satisfies `require_role(Role::Admin)`).
    #[must_use]
    pub const fn at_least(self, required: Role) -> bool {
        self.rank() >= required.rank()
    }
}

/// Require that the signed-in user's role in the active organization is
/// `required` or higher, e.g.
/// `require_role(&session, &membership_repo, Role::Admin).await?`.
///
/// Resolves `user_id` from the session, then re-reads that user's
/// `Membership` row for the active organization (`membership_repo` is
/// `tenant_scoped`, so this is automatically filtered to the caller's active
/// tenant — see `repositories.rs`) rather than trusting the session's cached
/// `"role"` string, which can go stale as soon as another request changes or
/// revokes the caller's membership. Returns the resolved role on success so
/// callers that need it (e.g. to decide whether to show owner-only controls)
/// don't have to look it up twice.
///
/// # Errors
///
/// - `401 Unauthorized` when there's no signed-in user, or the signed-in user
///   has no membership in the active organization.
/// - `403 Forbidden` when the resolved role does not meet `required`.
pub async fn require_role(
    session: &Session,
    membership_repo: &PgMembershipRepository,
    required: Role,
) -> AutumnResult<Role> {
    let Some(user_id) = session
        .get("user_id")
        .await
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Err(AutumnError::unauthorized_msg("no active organization"));
    };
    let memberships = membership_repo.find_by_user_id(user_id).await?;
    let Some(membership) = memberships.into_iter().next() else {
        return Err(AutumnError::unauthorized_msg("no active organization"));
    };
    let Some(role) = Role::parse(&membership.role) else {
        return Err(AutumnError::forbidden_msg("insufficient permissions"));
    };
    if role.at_least(required) {
        Ok(role)
    } else {
        Err(AutumnError::forbidden_msg("insufficient permissions"))
    }
}

// `require_role` itself now needs a live `Membership` row (see above), so it
// is exercised end-to-end via `tests/integration_test.rs`
// (`removed_member_loses_access_immediately_despite_cached_session` and the
// role-gated `admin_cannot_grant_owner_role` / `member_cannot_invite` tests)
// rather than with a session-only unit test here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_owner_satisfies_admin_and_member() {
        assert!(Role::Owner.at_least(Role::Owner));
        assert!(Role::Owner.at_least(Role::Admin));
        assert!(Role::Owner.at_least(Role::Member));
    }

    #[test]
    fn hierarchy_admin_satisfies_member_but_not_owner() {
        assert!(Role::Admin.at_least(Role::Admin));
        assert!(Role::Admin.at_least(Role::Member));
        assert!(!Role::Admin.at_least(Role::Owner));
    }

    #[test]
    fn hierarchy_member_satisfies_only_member() {
        assert!(Role::Member.at_least(Role::Member));
        assert!(!Role::Member.at_least(Role::Admin));
        assert!(!Role::Member.at_least(Role::Owner));
    }

    #[test]
    fn parse_round_trips_known_roles() {
        for role in [Role::Owner, Role::Admin, Role::Member] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
    }

    #[test]
    fn parse_rejects_unknown_strings() {
        assert_eq!(Role::parse("superadmin"), None);
        assert_eq!(Role::parse(""), None);
        assert_eq!(Role::parse("Owner"), None); // case-sensitive, no silent coercion
    }
}
