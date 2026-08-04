//! The closed membership-role enum and the `require_role` guard.
//!
//! `Membership.role` is stored as `TEXT` (with a SQL `CHECK` constraint as a
//! defense-in-depth backstop), but every place that *reads* a role goes
//! through [`Role::parse`] — an unrecognized string can never silently pass
//! an authorization check.
//!
//! [`require_role`] reads the same session `"role"` key that
//! `#[secured("...")]` and `PolicyContext::has_role` already read (populated
//! from the caller's active-organization `Membership` at login/signup/org
//! switch/invite-accept — see `routes/auth.rs`), so this is a hierarchy-aware
//! guard layered on top of the existing session/Policy plumbing, not a second
//! authorization mechanism (issue #1261 AC2).

use autumn_web::prelude::*;

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
/// `required` or higher, e.g. `require_role(&session, Role::Admin).await?`.
///
/// Reads the session `"role"` key established for the active organization
/// (see `establish_session` / `switch_active_organization` in
/// `routes/auth.rs`). Returns the resolved role on success so callers that
/// need it (e.g. to decide whether to show owner-only controls) don't have to
/// look it up twice.
///
/// # Errors
///
/// - `401 Unauthorized` when no role is present in the session (no active
///   organization — the caller isn't a member of one, or isn't signed in).
/// - `403 Forbidden` when the resolved role does not meet `required`.
pub async fn require_role(session: &Session, required: Role) -> AutumnResult<Role> {
    let Some(role_str) = session.get("role").await else {
        return Err(AutumnError::unauthorized_msg("no active organization"));
    };
    let Some(role) = Role::parse(&role_str) else {
        return Err(AutumnError::forbidden_msg("insufficient permissions"));
    };
    if role.at_least(required) {
        Ok(role)
    } else {
        Err(AutumnError::forbidden_msg("insufficient permissions"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn session_with_role(role: Option<&str>) -> Session {
        let mut data = HashMap::new();
        if let Some(r) = role {
            data.insert("role".to_owned(), r.to_owned());
        }
        Session::new_for_test(String::new(), data)
    }

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

    #[tokio::test]
    async fn require_role_unauthorized_without_session_role() {
        let session = session_with_role(None);
        let err = require_role(&session, Role::Member).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_role_forbidden_for_garbage_session_role() {
        let session = session_with_role(Some("superadmin"));
        let err = require_role(&session, Role::Member).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn require_role_forbidden_when_below_required() {
        let session = session_with_role(Some("member"));
        let err = require_role(&session, Role::Admin).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn require_role_ok_when_at_or_above_required() {
        let session = session_with_role(Some("owner"));
        let role = require_role(&session, Role::Admin).await.unwrap();
        assert_eq!(role, Role::Owner);
    }
}
