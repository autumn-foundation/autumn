//! Admin **user impersonation** — "log in as this user", without losing the
//! real admin's identity (issue #1394).
//!
//! Every support team eventually needs to reproduce a bug, verify a
//! permission, or walk a customer through a screen *as that customer*. Doing it
//! by hand — `session.insert("user_id", target)` — silently destroys the audit
//! trail: from that moment on every version row and audit event claims the
//! customer did it. This module is the primitive that makes the secure version
//! the easy one.
//!
//! # What it does
//!
//! [`begin_impersonation`] swaps the session's **effective** user to the target
//! and records the real admin separately under [`IMPERSONATOR_SESSION_KEY`].
//! Because the effective user lives in the ordinary auth session key,
//! everything that resolves "the current user" — [`Auth`](super::Auth),
//! `#[secured]`, [`PolicyContext`] — transparently sees the *impersonated*
//! user, exactly as if they had logged in. Meanwhile the framework's ambient
//! [current actor](crate::current::Current) — the value that seeds
//! `#[repository(versioned)]` version rows and audit events — is published as
//! the **real impersonator**, so writes made during the session stay honestly
//! attributed.
//!
//! [`end_impersonation`] reverses it, restoring the admin's user id and role.
//! Both directions rotate the session id (no fixation) and write an audit
//! event naming both parties.
//!
//! # Default-deny
//!
//! Beginning impersonation requires an [`ImpersonationGate`] installed in
//! [`AppState`]. With no gate registered every attempt is refused with `403` —
//! an app opts in explicitly:
//!
//! ```rust,no_run
//! use autumn_web::AppBuilder;
//! use autumn_web::auth::impersonation::ImpersonationGate;
//!
//! # fn wire(app: AppBuilder) -> AppBuilder {
//! app.state_initializer(|state| {
//!     state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
//! })
//! # }
//! ```
//!
//! `autumn-admin-plugin` wraps this in `AdminPlugin::with_impersonation(gate)`,
//! which registers the gate *and* mounts the begin/revert routes plus the
//! "Viewing as … — Stop impersonating" banner.
//!
//! # Scope
//!
//! Session-based auth only: an API-token principal has no session to swap, so
//! the token authentication path is untouched. Same-tenant only — the
//! [`ImpersonationPolicy`] is the seam where an app enforces its tenancy
//! boundary (it receives the full [`PolicyContext`], including the session and
//! the DB pool). Step-up (`#[step_up]`) still evaluates the *real admin's*
//! freshness claim, which `begin_impersonation` deliberately leaves untouched:
//! impersonation never launders a sensitive action past step-up.
//!
//! [`AppState`]: crate::AppState
//! [`PolicyContext`]: crate::authorization::PolicyContext

use std::sync::Arc;

use crate::AppState;
use crate::audit::{AuditEvent, AuditStatus};
use crate::authorization::{BoxFuture, PolicyContext};
use crate::current::Current;
use crate::session::Session;

/// Session key holding the **real** admin's id while impersonation is active.
///
/// Reserved by the framework: application code must not write it directly —
/// doing so would forge an impersonation that never passed the
/// [`ImpersonationGate`] and never produced an audit event. Read it through
/// [`impersonator_id`] instead.
pub const IMPERSONATOR_SESSION_KEY: &str = "impersonator_id";

/// Session key stashing the real admin's role for the duration of the
/// impersonation, so [`end_impersonation`] can restore it exactly. Reserved by
/// the framework, like [`IMPERSONATOR_SESSION_KEY`].
pub const IMPERSONATOR_ROLE_SESSION_KEY: &str = "impersonator_role";

/// Session key holding the current role. Mirrors the key `#[secured("role")]`
/// and the admin plugin's role middleware read.
const ROLE_SESSION_KEY: &str = "role";

/// Audit action recorded when impersonation begins.
pub const BEGIN_AUDIT_ACTION: &str = "auth.impersonation.begin";

/// Audit action recorded when impersonation ends.
pub const END_AUDIT_ACTION: &str = "auth.impersonation.end";

// ── Target ───────────────────────────────────────────────────────

/// The user an operator is asking to impersonate.
///
/// Constructed from any string-like id; `role` is an **optional, trusted,
/// server-side** decision about which role the impersonated session should
/// carry. It must never be populated from request input — that would let an
/// operator mint a session more privileged than the target really is. Leave it
/// unset and let [`ImpersonationPolicy::target_role`] resolve it from the app's
/// own user store (which is what `autumn-admin-plugin`'s route does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpersonationTarget {
    user_id: String,
    role: Option<String>,
}

impl ImpersonationTarget {
    /// A target identified by user id, with no role decided yet.
    #[must_use]
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            role: None,
        }
    }

    /// Attach the role the impersonated session should carry.
    ///
    /// Trusted input only — see the type-level note.
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    /// The target user's id.
    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The explicitly-attached role, if any.
    #[must_use]
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }
}

impl From<String> for ImpersonationTarget {
    fn from(user_id: String) -> Self {
        Self::new(user_id)
    }
}

impl From<&str> for ImpersonationTarget {
    fn from(user_id: &str) -> Self {
        Self::new(user_id)
    }
}

impl From<&String> for ImpersonationTarget {
    fn from(user_id: &String) -> Self {
        Self::new(user_id)
    }
}

// ── Policy + gate ────────────────────────────────────────────────

/// Decides whether the current principal may impersonate a given target.
///
/// Default-deny: [`can_impersonate`](Self::can_impersonate) has no default
/// implementation, and an app with no gate registered refuses every attempt.
///
/// ```rust
/// use autumn_web::authorization::{BoxFuture, PolicyContext};
/// use autumn_web::auth::impersonation::{ImpersonationPolicy, ImpersonationTarget};
///
/// struct SupportDesk;
///
/// impl ImpersonationPolicy for SupportDesk {
///     fn can_impersonate<'a>(
///         &'a self,
///         ctx: &'a PolicyContext,
///         target: &'a ImpersonationTarget,
///     ) -> BoxFuture<'a, bool> {
///         // Same-tenant only: consult the app's own store through `ctx`.
///         Box::pin(async move { ctx.has_role("support") && target.user_id() != "root" })
///     }
/// }
/// ```
pub trait ImpersonationPolicy: Send + Sync + 'static {
    /// `true` to allow the impersonation, `false` to refuse it with `403`.
    fn can_impersonate<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, bool>;

    /// The role the impersonated session should carry, resolved **server-side**
    /// from the app's own user store.
    ///
    /// Only consulted when the [`ImpersonationTarget`] carries no explicit
    /// role. Defaults to `None`, which drops the role entirely for the duration
    /// of the impersonation — the safe default, since the framework cannot know
    /// what role the target really holds and inheriting the *admin's* role would
    /// be a straight privilege escalation.
    fn target_role<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
}

/// Refuses everything. The behavior an app gets when no gate is registered.
struct DenyAll;

impl ImpersonationPolicy for DenyAll {
    fn can_impersonate<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
}

/// Allows any principal holding one of the listed roles.
struct AllowRoles(Vec<String>);

impl ImpersonationPolicy for AllowRoles {
    fn can_impersonate<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        _target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_any_role(&self.0) })
    }
}

/// The installed impersonation authorization gate.
///
/// Register one in [`AppState`](crate::AppState) to opt an app into
/// impersonation; without it [`begin_impersonation`] always returns `403`.
#[derive(Clone)]
pub struct ImpersonationGate(Arc<dyn ImpersonationPolicy>);

impl ImpersonationGate {
    /// A gate that refuses everything — the framework default, spelled out.
    #[must_use]
    pub fn deny_all() -> Self {
        Self(Arc::new(DenyAll))
    }

    /// Allow any principal whose session role is one of `roles`.
    ///
    /// The one-liner for "our `admin` role may impersonate":
    /// `ImpersonationGate::allow_roles(["admin"])`. The impersonated session
    /// still receives **no** role (see [`ImpersonationPolicy::target_role`]);
    /// use [`custom`](Self::custom) to resolve the target's real role.
    #[must_use]
    pub fn allow_roles<I, S>(roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(Arc::new(AllowRoles(
            roles.into_iter().map(Into::into).collect(),
        )))
    }

    /// Install an application-defined [`ImpersonationPolicy`].
    #[must_use]
    pub fn custom<P: ImpersonationPolicy>(policy: P) -> Self {
        Self(Arc::new(policy))
    }

    /// Borrow the underlying policy.
    #[must_use]
    pub fn policy(&self) -> &dyn ImpersonationPolicy {
        &*self.0
    }
}

impl std::fmt::Debug for ImpersonationGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImpersonationGate")
    }
}

// ── State ────────────────────────────────────────────────────────

/// A snapshot of an active impersonation: who is really acting, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpersonationState {
    /// The **real** operator — the id audit and version writes are attributed
    /// to while the impersonation is active.
    pub impersonator_id: String,
    /// The user the session currently resolves as.
    pub effective_user_id: String,
}

/// The real operator behind the current session, or `None` when the session is
/// not impersonating.
///
/// The companion to ordinary current-user resolution: `session.get("user_id")`
/// (and everything built on it) answers *"who does this request act as?"*, this
/// answers *"who is really doing it?"*.
pub async fn impersonator_id(session: &Session) -> Option<String> {
    session.get(IMPERSONATOR_SESSION_KEY).await
}

/// Whether the session is currently impersonating someone.
pub async fn is_impersonating(session: &Session) -> bool {
    session.contains_key(IMPERSONATOR_SESSION_KEY).await
}

/// The full [`ImpersonationState`] for the session, or `None` when it is not
/// impersonating. Reads the effective user through the app's configured auth
/// session key.
pub async fn impersonation_state(
    state: &AppState,
    session: &Session,
) -> Option<ImpersonationState> {
    let impersonator_id = impersonator_id(session).await?;
    let effective_user_id = session
        .get(state.auth_session_key())
        .await
        .unwrap_or_default();
    Some(ImpersonationState {
        impersonator_id,
        effective_user_id,
    })
}

/// The id that audit and version writes made by this session should carry.
///
/// Returns the real impersonator while impersonation is active, and
/// `effective_user_id` otherwise. This is the single rule the framework's three
/// session-based [`Current::set_actor`] seams apply, so `#[repository(versioned)]`
/// writes and [`AuditEvent`]s stay attributed to the human responsible.
pub async fn audit_actor_id(session: &Session, effective_user_id: &str) -> String {
    impersonator_id(session)
        .await
        .unwrap_or_else(|| effective_user_id.to_owned())
}

// ── Begin / end ──────────────────────────────────────────────────

/// Begin impersonating `target` from the current admin session.
///
/// On success the session's effective user becomes the target, the real admin
/// is recorded under [`IMPERSONATOR_SESSION_KEY`], the session id is rotated,
/// and a [`BEGIN_AUDIT_ACTION`] audit event carrying
/// `{impersonator_id, target_id}` is written.
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::auth::impersonation;
///
/// #[autumn_web::post("/support/impersonate/{user_id}")]
/// #[autumn_web::secured("support")]
/// async fn impersonate(
///     State(state): State<AppState>,
///     session: Session,
///     Path(user_id): Path<String>,
/// ) -> AutumnResult<Redirect> {
///     impersonation::begin_impersonation(&state, &session, user_id).await?;
///     Ok(Redirect::to("/"))
/// }
/// ```
///
/// # Errors
///
/// * `400` — the target id is blank, or names the caller themselves.
/// * `401` — the session is not authenticated.
/// * `403` — no [`ImpersonationGate`] is installed, or the installed policy
///   refused. The refusal is itself audited as a failure.
/// * `409` — the session is **already** impersonating. Impersonation does not
///   nest, so it cannot be chained to escalate.
/// * `500` — the audit event could not be written. A privileged identity swap
///   that cannot be recorded does not happen at all.
pub async fn begin_impersonation(
    state: &AppState,
    session: &Session,
    target: impl Into<ImpersonationTarget>,
) -> crate::AutumnResult<ImpersonationState> {
    let target = target.into();
    let target_id = target.user_id().trim();
    if target_id.is_empty() {
        return Err(crate::AutumnError::bad_request_msg(
            "impersonation target is required",
        ));
    }

    let auth_key = state.auth_session_key().to_owned();
    let Some(real_id) = session.get(&auth_key).await else {
        return Err(crate::AutumnError::unauthorized_msg(
            "authentication required",
        ));
    };

    // No nesting: an already-impersonated session cannot start a second hop,
    // so impersonation can never be chained into an escalation.
    if is_impersonating(session).await {
        return Err(crate::AutumnError::conflict_msg(
            "already impersonating; stop the current impersonation first",
        ));
    }

    if target_id == real_id {
        return Err(crate::AutumnError::bad_request_msg(
            "cannot impersonate yourself",
        ));
    }

    let ctx = PolicyContext::from_request(state, session).await;
    let gate = state.extension::<ImpersonationGate>();
    let allowed = match gate.as_deref() {
        Some(gate) => gate.policy().can_impersonate(&ctx, &target).await,
        // Default-deny: an app that never opted in refuses every attempt.
        None => false,
    };

    if !allowed {
        let _ = crate::audit::write_from_state(
            state,
            AuditEvent::new(
                &real_id,
                BEGIN_AUDIT_ACTION,
                target_id,
                None,
                AuditStatus::Failure,
            ),
        )
        .await;
        return Err(crate::AutumnError::forbidden_msg(
            "not permitted to impersonate",
        ));
    }

    // Audit *before* the swap, and fail closed: an identity swap that cannot be
    // recorded must not take effect. The worst case is an over-recorded event,
    // never a silent one.
    crate::audit::write_from_state(
        state,
        AuditEvent::new(
            &real_id,
            BEGIN_AUDIT_ACTION,
            target_id,
            None,
            AuditStatus::Success,
        ),
    )
    .await
    .map_err(|error| {
        crate::AutumnError::internal_server_error_msg(format!(
            "impersonation refused: audit write failed: {error}"
        ))
    })?;

    // Resolve the impersonated role server-side only. An explicit role on the
    // target wins (a trusted, in-process caller decided it); otherwise the
    // policy resolves it from the app's own user store; otherwise the session
    // carries no role at all — never the admin's.
    let target_role = match target.role() {
        Some(role) => Some(role.to_owned()),
        None => match gate.as_deref() {
            Some(gate) => gate.policy().target_role(&ctx, &target).await,
            None => None,
        },
    };

    // Swap the identity. Only these keys move: everything else in the session
    // (flash, CSRF, the admin's own step-up claim) is deliberately preserved.
    session
        .insert(IMPERSONATOR_SESSION_KEY, real_id.clone())
        .await;
    match session.get(ROLE_SESSION_KEY).await {
        Some(role) => session.insert(IMPERSONATOR_ROLE_SESSION_KEY, role).await,
        None => {
            session.remove(IMPERSONATOR_ROLE_SESSION_KEY).await;
        }
    }
    session.insert(&auth_key, target_id).await;
    match target_role {
        Some(role) => session.insert(ROLE_SESSION_KEY, role).await,
        None => {
            session.remove(ROLE_SESSION_KEY).await;
        }
    }
    // Privilege change ⇒ new session id (no fixation).
    session.rotate_id().await;

    // The remainder of *this* request is the impersonator's work too.
    Current::set_actor(real_id.clone());

    Ok(ImpersonationState {
        impersonator_id: real_id,
        effective_user_id: target_id.to_owned(),
    })
}

/// End the active impersonation and restore the original admin session.
///
/// Restores the admin's user id and role, clears the impersonation keys,
/// rotates the session id, and writes an [`END_AUDIT_ACTION`] audit event
/// carrying `{impersonator_id, target_id}`.
///
/// Deliberately **not** gated by the [`ImpersonationGate`]: reverting is always
/// permitted, so an operator impersonating a user who has lost the granting
/// role is never trapped in that identity.
///
/// # Errors
///
/// Returns `400` when the session is not impersonating. An audit-sink failure
/// is logged but does not block the revert — the opposite trade-off from
/// [`begin_impersonation`], because refusing to *drop* privilege would be the
/// less safe outcome.
pub async fn end_impersonation(
    state: &AppState,
    session: &Session,
) -> crate::AutumnResult<ImpersonationState> {
    let Some(real_id) = impersonator_id(session).await else {
        return Err(crate::AutumnError::bad_request_msg("not impersonating"));
    };
    let auth_key = state.auth_session_key().to_owned();
    let effective_user_id = session.get(&auth_key).await.unwrap_or_default();

    if let Err(error) = crate::audit::write_from_state(
        state,
        AuditEvent::new(
            &real_id,
            END_AUDIT_ACTION,
            &effective_user_id,
            None,
            AuditStatus::Success,
        ),
    )
    .await
    {
        tracing::error!(
            target: "autumn.audit",
            impersonator_id = %real_id,
            target_id = %effective_user_id,
            %error,
            "failed to write the impersonation-end audit event; reverting anyway"
        );
    }

    session.insert(&auth_key, real_id.clone()).await;
    session.remove(IMPERSONATOR_SESSION_KEY).await;
    match session.remove(IMPERSONATOR_ROLE_SESSION_KEY).await {
        Some(role) => session.insert(ROLE_SESSION_KEY, role).await,
        None => {
            session.remove(ROLE_SESSION_KEY).await;
        }
    }
    session.rotate_id().await;

    Current::set_actor(real_id.clone());

    Ok(ImpersonationState {
        impersonator_id: real_id,
        effective_user_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn session_with(pairs: &[(&str, &str)]) -> Session {
        let data: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Session::new_for_test("sid-1".to_owned(), data)
    }

    #[tokio::test]
    async fn impersonator_accessors_read_the_reserved_key() {
        let session = session_with(&[("user_id", "target"), (IMPERSONATOR_SESSION_KEY, "admin")]);
        assert!(is_impersonating(&session).await);
        assert_eq!(impersonator_id(&session).await, Some("admin".to_owned()));
    }

    #[tokio::test]
    async fn a_plain_session_is_not_impersonating() {
        let session = session_with(&[("user_id", "u1")]);
        assert!(!is_impersonating(&session).await);
        assert_eq!(impersonator_id(&session).await, None);
    }

    #[tokio::test]
    async fn audit_actor_prefers_the_impersonator_and_falls_back_to_the_user() {
        let impersonating =
            session_with(&[("user_id", "target"), (IMPERSONATOR_SESSION_KEY, "admin")]);
        assert_eq!(audit_actor_id(&impersonating, "target").await, "admin");

        let plain = session_with(&[("user_id", "u1")]);
        assert_eq!(audit_actor_id(&plain, "u1").await, "u1");
    }

    #[test]
    fn a_target_is_built_from_any_string_like_id() {
        assert_eq!(ImpersonationTarget::from("u1").user_id(), "u1");
        assert_eq!(ImpersonationTarget::from("u1".to_owned()).user_id(), "u1");
        assert_eq!(ImpersonationTarget::from(&"u1".to_owned()).user_id(), "u1");
        assert_eq!(ImpersonationTarget::new("u1").role(), None);
        assert_eq!(
            ImpersonationTarget::new("u1").with_role("member").role(),
            Some("member")
        );
    }

    #[tokio::test]
    async fn deny_all_refuses_and_allow_roles_matches_any_listed_role() {
        let session = session_with(&[("user_id", "admin-1"), ("role", "admin")]);
        let ctx = PolicyContext::from_session(&session, "user_id").await;
        let target = ImpersonationTarget::new("u9");

        assert!(
            !ImpersonationGate::deny_all()
                .policy()
                .can_impersonate(&ctx, &target)
                .await
        );
        assert!(
            ImpersonationGate::allow_roles(["support", "admin"])
                .policy()
                .can_impersonate(&ctx, &target)
                .await
        );
        assert!(
            !ImpersonationGate::allow_roles(["support"])
                .policy()
                .can_impersonate(&ctx, &target)
                .await
        );
    }

    #[tokio::test]
    async fn the_default_target_role_is_none_so_no_role_is_inherited() {
        let session = session_with(&[("user_id", "admin-1"), ("role", "admin")]);
        let ctx = PolicyContext::from_session(&session, "user_id").await;
        let target = ImpersonationTarget::new("u9");
        assert_eq!(
            ImpersonationGate::allow_roles(["admin"])
                .policy()
                .target_role(&ctx, &target)
                .await,
            None,
            "an impersonated session must never inherit the admin's role"
        );
    }
}
