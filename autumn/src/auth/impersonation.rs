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
//! everything the framework resolves "the current user" from — `#[secured]`,
//! [`RequireAuth`](super::RequireAuth), [`PolicyContext`] — transparently sees
//! the *impersonated* user, exactly as if they had logged in. ([`Auth<T>`] is
//! populated by the app's own loader middleware from request extensions, so it
//! follows only if that loader reads the auth session key.) Meanwhile the ambient
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
//! use autumn_web::app::AppBuilder;
//! use autumn_web::auth::impersonation::ImpersonationGate;
//!
//! # fn wire(app: AppBuilder) -> AppBuilder {
//! app.impersonation_gate(ImpersonationGate::allow_roles(["admin"]))
//! # }
//! ```
//!
//! `autumn-admin-plugin` wraps this in `AdminPlugin::with_impersonation(gate)`,
//! which registers the gate *and* mounts the begin/revert routes plus the
//! "Viewing as … — Stop impersonating" banner.
//!
//! # An audit sink is required
//!
//! [`begin_impersonation`] refuses with `500` unless the app has an
//! [`AuditLogger`](crate::audit::AuditLogger) with at least one sink installed
//! — `audit::write_from_state` is a silent no-op without one, and an
//! unrecorded identity swap is exactly what this module exists to prevent.
//! `AppBuilder::with_audit_sink(TracingAuditSink)` is the minimum; a durable
//! sink (`JsonlFileAuditSink`, or your own) is what you actually want. Ending
//! an impersonation is deliberately *not* subject to this: dropping privilege
//! must always succeed.
//!
//! # Scope
//!
//! Session-based auth only: an API-token principal has no session to swap, so
//! the token authentication path is untouched. Same-tenant only — the
//! [`ImpersonationPolicy`] is the seam where an app enforces its tenancy
//! boundary (it receives the full [`PolicyContext`], including the session and
//! the DB pool) — the framework itself enforces no tenancy, and
//! [`ImpersonationGate::allow_roles`] in particular does not look at the target
//! at all, so a multi-tenant app wants a real [`ImpersonationPolicy`].
//!
//! Step-up does **not** carry over: [`begin_impersonation`] stashes the
//! operator's `last_strong_auth_at` claim and drops it from the impersonated
//! session, so a `#[step_up]` route cannot run a sensitive action on the
//! target's account on the strength of the *operator's* re-authentication.
//! [`end_impersonation`] restores the operator's own claim.
//!
//! # Reserved session keys
//!
//! [`IMPERSONATOR_SESSION_KEY`], [`IMPERSONATED_SESSION_KEY`],
//! [`IMPERSONATOR_ROLE_SESSION_KEY`] and [`IMPERSONATOR_STEP_UP_SESSION_KEY`]
//! belong to the framework; do not configure `[auth].session_key` to collide
//! with them, and do not write them by hand. Call [`clear`] from any flow that
//! replaces the session's user outright (a login, a magic-link or passkey
//! promotion) so a record left behind by an operator who never reverted is not
//! inherited by whoever logs in next.
//!
//! [`Auth<T>`]: super::Auth
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

/// Session key recording **which** user the impersonation record describes.
///
/// The record is only honored while this still matches the session's effective
/// user. Without that binding, a stale record left behind by an operator who
/// never reverted would be picked up by whoever logs in on that session next —
/// handing them the admin's identity through the revert route, and
/// misattributing their writes to the admin in the meantime. Reserved by the
/// framework, like [`IMPERSONATOR_SESSION_KEY`].
pub const IMPERSONATED_SESSION_KEY: &str = "impersonated_id";

/// Session key stashing the real admin's role for the duration of the
/// impersonation, so [`end_impersonation`] can restore it exactly. Reserved by
/// the framework, like [`IMPERSONATOR_SESSION_KEY`].
pub const IMPERSONATOR_ROLE_SESSION_KEY: &str = "impersonator_role";

/// Session key stashing the real admin's step-up claim.
///
/// Holds [`step_up::STEP_UP_SESSION_KEY`](crate::step_up::STEP_UP_SESSION_KEY)
/// for the duration of the impersonation, so the impersonated session cannot
/// spend the operator's freshness. Reserved by the framework, like
/// [`IMPERSONATOR_SESSION_KEY`].
pub const IMPERSONATOR_STEP_UP_SESSION_KEY: &str = "impersonator_last_strong_auth_at";

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

    /// The same target with its user id trimmed of surrounding whitespace.
    ///
    /// [`begin_impersonation`] applies this **before** the
    /// [`ImpersonationPolicy`] ever sees the target, so the id the policy
    /// authorizes, the id the audit event records, and the id written to the
    /// session are always one and the same string.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.user_id = self.user_id.trim().to_owned();
        self
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
#[non_exhaustive]
pub struct ImpersonationState {
    /// The **real** operator — the id audit and version writes are attributed
    /// to while the impersonation is active.
    pub impersonator_id: String,
    /// The user the session currently resolves as.
    pub effective_user_id: String,
}

/// The raw impersonation record on the session, **unvalidated**.
///
/// Returns `(impersonator_id, impersonated_id)` when both reserved keys are
/// present. Callers must confirm `impersonated_id` still matches the session's
/// effective user before honoring it — see [`audit_actor_id`], which is the
/// validating form every framework seam uses.
async fn raw_record(session: &Session) -> Option<(String, String)> {
    let impersonator = session.get(IMPERSONATOR_SESSION_KEY).await?;
    let impersonated = session.get(IMPERSONATED_SESSION_KEY).await?;
    Some((impersonator, impersonated))
}

/// The real operator behind the current session, or `None` when the session is
/// not impersonating.
///
/// The companion to ordinary current-user resolution: `session.get("user_id")`
/// (and everything built on it) answers *"who does this request act as?"*, this
/// answers *"who is really doing it?"*.
///
/// Validated: a record whose recorded target no longer matches the session's
/// effective user is stale — someone logged in on this session after an
/// operator walked away without reverting — and is reported as "not
/// impersonating" rather than silently attributing that person's work to the
/// operator. Clear such a record with [`clear`].
pub async fn impersonator_id(state: &AppState, session: &Session) -> Option<String> {
    impersonation_state(state, session)
        .await
        .map(|active| active.impersonator_id)
}

/// Whether the session is currently impersonating someone.
///
/// Validated the same way as [`impersonator_id`].
pub async fn is_impersonating(state: &AppState, session: &Session) -> bool {
    impersonation_state(state, session).await.is_some()
}

/// The full [`ImpersonationState`] for the session, or `None` when it is not
/// impersonating. Reads the effective user through the app's configured auth
/// session key and verifies the record still describes that user.
pub async fn impersonation_state(
    state: &AppState,
    session: &Session,
) -> Option<ImpersonationState> {
    let effective_user_id = session.get(state.auth_session_key()).await?;
    let (impersonator_id, impersonated_id) = raw_record(session).await?;
    (impersonated_id == effective_user_id).then_some(ImpersonationState {
        impersonator_id,
        effective_user_id,
    })
}

/// Drop any impersonation record from the session.
///
/// Audits nothing and restores nothing.
///
/// This is **not** the revert — use [`end_impersonation`] for that. It exists
/// for identity transitions that replace the session's user outright, where the
/// old record no longer describes anyone: a login, a magic-link or passkey
/// promotion, an account switch. Call it wherever your app writes the auth
/// session key directly, so a record left behind by an operator who never
/// reverted cannot be inherited by the next person to log in on that session.
///
/// ```rust,no_run
/// # use autumn_web::session::Session;
/// # use autumn_web::auth::impersonation;
/// async fn establish_session(session: &Session, user_id: &str) {
///     session.rotate_id().await;
///     impersonation::clear(session).await;
///     session.insert("user_id", user_id).await;
/// }
/// ```
pub async fn clear(session: &Session) {
    session.remove(IMPERSONATOR_SESSION_KEY).await;
    session.remove(IMPERSONATED_SESSION_KEY).await;
    session.remove(IMPERSONATOR_ROLE_SESSION_KEY).await;
    session.remove(IMPERSONATOR_STEP_UP_SESSION_KEY).await;
}

/// The id that audit and version writes made by this session should carry.
///
/// Returns the real impersonator while impersonation is active, and
/// `effective_user_id` otherwise. This is the single rule the framework's
/// session-based [`Current::set_actor`] seams apply, so
/// `#[repository(versioned)]` writes and [`AuditEvent`]s stay attributed to the
/// human responsible.
///
/// Validating by construction: the caller supplies the effective user, so a
/// stale record — one describing a *different* user than the session now
/// resolves as — is ignored and the effective user is returned. That keeps a
/// forgotten impersonation from misattributing the next person's writes to the
/// operator.
pub async fn audit_actor_id(session: &Session, effective_user_id: &str) -> String {
    match raw_record(session).await {
        Some((impersonator, impersonated)) if impersonated == effective_user_id => impersonator,
        _ => effective_user_id.to_owned(),
    }
}

// ── Extractor ────────────────────────────────────────────────────

/// Request extractor resolving the session's active impersonation, if any.
///
/// Infallible — a request with no session, or one that is not impersonating,
/// yields `Impersonation(None)` — so a handler can take it unconditionally and
/// branch on it. The ergonomic form of [`impersonation_state`] for apps that use
/// the core primitive without `autumn-admin-plugin`.
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::auth::impersonation::Impersonation;
///
/// #[autumn_web::get("/")]
/// async fn home(impersonation: Impersonation) -> String {
///     match impersonation.state() {
///         Some(active) => format!("viewing as {}", active.effective_user_id),
///         None => "your own account".to_owned(),
///     }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Impersonation(Option<ImpersonationState>);

impl Impersonation {
    /// The active impersonation, or `None` when the session is not
    /// impersonating (or there is no session).
    #[must_use]
    pub const fn state(&self) -> Option<&ImpersonationState> {
        self.0.as_ref()
    }

    /// Consume the extractor, yielding the active impersonation.
    #[must_use]
    pub fn into_state(self) -> Option<ImpersonationState> {
        self.0
    }

    /// Whether the request is running under an impersonation.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.0.is_some()
    }
}

impl axum::extract::FromRequestParts<AppState> for Impersonation {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(session) = parts.extensions.get::<Session>().cloned() else {
            return Ok(Self(None));
        };
        Ok(Self(impersonation_state(state, &session).await))
    }
}

// ── Begin / end ──────────────────────────────────────────────────

/// Move `session` from the operator's identity onto the target's.
///
/// Only the impersonation-relevant keys move; everything else in the session
/// (flash, CSRF, wizard progress) is deliberately preserved. The operator's
/// role and step-up claim are stashed rather than discarded, so
/// [`end_impersonation`] can restore them exactly.
async fn swap_identity(
    session: &Session,
    auth_key: &str,
    real_id: &str,
    target_id: &str,
    target_role: Option<&str>,
) {
    session.insert(IMPERSONATOR_SESSION_KEY, real_id).await;
    session.insert(IMPERSONATED_SESSION_KEY, target_id).await;
    stash(session, ROLE_SESSION_KEY, IMPERSONATOR_ROLE_SESSION_KEY).await;
    // `last_strong_auth_at` is a bare timestamp with no identity bound to it,
    // so carrying it across the swap would let a `#[step_up]` route run a
    // destructive action *on the target's account* on the strength of the
    // operator's re-authentication — impersonation laundering a credential
    // check. Every other identity transition in the framework drops this key
    // for the same reason.
    stash(
        session,
        crate::step_up::STEP_UP_SESSION_KEY,
        IMPERSONATOR_STEP_UP_SESSION_KEY,
    )
    .await;

    session.insert(auth_key, target_id).await;
    set_or_remove(session, ROLE_SESSION_KEY, target_role).await;
    // Privilege change ⇒ new session id (no fixation).
    session.rotate_id().await;
}

/// Move `from` to `to`, leaving neither key set when `from` was absent.
async fn stash(session: &Session, from: &str, to: &str) {
    let value = session.remove(from).await;
    set_or_remove(session, to, value.as_deref()).await;
}

/// Set `key` to `value`, or remove it entirely when `value` is `None`.
async fn set_or_remove(session: &Session, key: &str, value: Option<&str>) {
    match value {
        Some(value) => session.insert(key, value).await,
        None => {
            session.remove(key).await;
        }
    }
}

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
/// * `500` — no audit sink is configured, or the audit event could not be
///   written. A privileged identity swap that cannot be recorded does not
///   happen at all, so an app must register an [`AuditSink`](crate::audit::AuditSink)
///   before enabling impersonation.
pub async fn begin_impersonation(
    state: &AppState,
    session: &Session,
    target: impl Into<ImpersonationTarget>,
) -> crate::AutumnResult<ImpersonationState> {
    // Normalize ONCE, before anything sees the target. The policy, the audit
    // event, and the session write must all reason about the same string: if the
    // gate were handed a raw `" root "` while the session received `"root"`, a
    // policy denying `"root"` would authorize an id it never approved.
    let target = target.into().normalized();
    let target_id = target.user_id();

    let auth_key = state.auth_session_key().to_owned();
    let Some(real_id) = session.get(&auth_key).await else {
        return Err(crate::AutumnError::unauthorized_msg(
            "authentication required",
        ));
    };

    if target_id.is_empty() {
        return Err(crate::AutumnError::bad_request_msg(
            "impersonation target is required",
        ));
    }

    // No nesting: an already-impersonated session cannot start a second hop,
    // so impersonation can never be chained into an escalation. Uses the
    // validated form, so a *stale* record (one describing a user this session
    // no longer resolves as) does not permanently wedge the session at 409.
    if is_impersonating(state, session).await {
        return Err(crate::AutumnError::conflict_msg(
            "already impersonating; stop the current impersonation first",
        ));
    }

    if target_id == real_id.trim() {
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

    // Fail closed on a *missing* sink, not just a failing one.
    // `audit::write_from_state` deliberately returns `Ok(())` when no
    // `AuditLogger` is installed, so without this check an app that enabled
    // impersonation but never configured a sink would swap identities with no
    // record at all — precisely the outcome this module exists to prevent.
    // Checked after the authorization gate so an unauthorized caller still gets
    // a plain `403` and learns nothing about the app's audit configuration.
    let audit_enabled = state
        .extension::<crate::audit::AuditLogger>()
        .is_some_and(|logger| logger.is_enabled());
    if !audit_enabled {
        return Err(crate::AutumnError::internal_server_error_msg(
            "impersonation refused: no audit sink is configured. Register one \
             (e.g. `AppBuilder::with_audit_sink(TracingAuditSink)`) before \
             enabling impersonation — an unrecorded identity swap is exactly \
             what this feature exists to prevent",
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

    swap_identity(
        session,
        &auth_key,
        &real_id,
        target_id,
        target_role.as_deref(),
    )
    .await;

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
    let auth_key = state.auth_session_key().to_owned();
    let Some(active) = impersonation_state(state, session).await else {
        // Either there is no record, or there is a *stale* one describing a user
        // this session no longer resolves as — an operator walked away without
        // reverting and somebody else logged in on the same session. Honoring
        // that record would hand the new user the operator's identity and role
        // with no credential, so drop it instead of restoring it.
        clear(session).await;
        return Err(crate::AutumnError::bad_request_msg("not impersonating"));
    };
    let real_id = active.impersonator_id;
    let effective_user_id = active.effective_user_id;

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
    session.remove(IMPERSONATED_SESSION_KEY).await;
    stash(session, IMPERSONATOR_ROLE_SESSION_KEY, ROLE_SESSION_KEY).await;
    // Put the operator's own step-up claim back exactly as it was, and discard
    // anything the impersonated session accrued — that freshness belonged to
    // the target's credential, not the operator's.
    stash(
        session,
        IMPERSONATOR_STEP_UP_SESSION_KEY,
        crate::step_up::STEP_UP_SESSION_KEY,
    )
    .await;
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
    async fn audit_actor_prefers_the_impersonator_and_falls_back_to_the_user() {
        let impersonating = session_with(&[
            ("user_id", "target"),
            (IMPERSONATOR_SESSION_KEY, "admin"),
            (IMPERSONATED_SESSION_KEY, "target"),
        ]);
        assert_eq!(audit_actor_id(&impersonating, "target").await, "admin");

        let plain = session_with(&[("user_id", "u1")]);
        assert_eq!(audit_actor_id(&plain, "u1").await, "u1");
    }

    #[tokio::test]
    async fn a_stale_record_is_ignored_rather_than_misattributed() {
        // The record describes `target`, but the session now resolves as
        // `carol` — somebody logged in after an operator walked away without
        // reverting. Carol's writes are hers.
        let stale = session_with(&[
            ("user_id", "carol"),
            (IMPERSONATOR_SESSION_KEY, "admin"),
            (IMPERSONATED_SESSION_KEY, "target"),
        ]);
        assert_eq!(audit_actor_id(&stale, "carol").await, "carol");
    }

    #[tokio::test]
    async fn a_half_written_record_is_ignored() {
        // Only the framework writes both keys together, so a session carrying
        // just one (an app poking at the reserved key by hand) is not a record.
        let half = session_with(&[("user_id", "target"), (IMPERSONATOR_SESSION_KEY, "admin")]);
        assert_eq!(audit_actor_id(&half, "target").await, "target");
    }

    #[tokio::test]
    async fn clear_drops_every_reserved_key() {
        let session = session_with(&[
            ("user_id", "target"),
            (IMPERSONATOR_SESSION_KEY, "admin"),
            (IMPERSONATED_SESSION_KEY, "target"),
            (IMPERSONATOR_ROLE_SESSION_KEY, "admin"),
            (IMPERSONATOR_STEP_UP_SESSION_KEY, "123"),
        ]);
        clear(&session).await;
        for key in [
            IMPERSONATOR_SESSION_KEY,
            IMPERSONATED_SESSION_KEY,
            IMPERSONATOR_ROLE_SESSION_KEY,
            IMPERSONATOR_STEP_UP_SESSION_KEY,
        ] {
            assert_eq!(session.get(key).await, None, "{key} must be cleared");
        }
        assert_eq!(
            session.get("user_id").await,
            Some("target".to_owned()),
            "clear() drops the record, not the session"
        );
    }

    #[test]
    fn normalizing_a_target_trims_the_id() {
        assert_eq!(
            ImpersonationTarget::new("  root  ").normalized().user_id(),
            "root"
        );
        assert_eq!(
            ImpersonationTarget::new("u1")
                .with_role("member")
                .normalized()
                .role(),
            Some("member"),
            "normalizing preserves the trusted role decision"
        );
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
