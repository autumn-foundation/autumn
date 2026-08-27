//! Admin-side wiring for user impersonation (issue #1394).
//!
//! The security-critical half lives in
//! [`autumn_web::auth::impersonation`]; this module is the UI: an extractor
//! that resolves the banner for the current request, the begin/revert routes,
//! and the helper an application calls to embed the banner in its **own**
//! layout.
//!
//! # Wiring
//!
//! ```rust,ignore
//! use autumn_admin_plugin::AdminPlugin;
//! use autumn_web::auth::impersonation::ImpersonationGate;
//!
//! autumn_web::app()
//!     .plugin(
//!         AdminPlugin::new()
//!             .register(UserAdmin::default())
//!             // Opt in: only the `admin` role may impersonate. Without this
//!             // call the routes are not mounted and the core primitive
//!             // default-denies.
//!             .with_impersonation(ImpersonationGate::allow_roles(["admin"])),
//!     )
//!     .run()
//!     .await;
//! ```
//!
//! That mounts `POST {prefix}/impersonate` (begin, behind the admin role gate)
//! and `POST {prefix}/impersonate/stop` (revert, deliberately **outside** the
//! gate so an operator impersonating a non-admin can always get back).
//!
//! Then render the banner in your application layout so it is visible on the
//! pages the operator is actually looking at:
//!
//! ```rust,ignore
//! #[get("/")]
//! async fn home(State(state): State<AppState>, session: Session, csrf: CsrfToken) -> Markup {
//!     let banner = autumn_admin_plugin::impersonation_banner_for(
//!         &state, &session, "/admin", csrf.token(), "_csrf",
//!     ).await;
//!     html! {
//!         body {
//!             @if let Some(banner) = banner { (banner) }
//!             main { "…" }
//!         }
//!     }
//! }
//! ```
//!
//! Add [`IMPERSONATION_BANNER_CSS`](crate::IMPERSONATION_BANNER_CSS) to that
//! layout's stylesheet; the plugin's own pages already include it.

use autumn_web::auth::impersonation;
use autumn_web::consent::safe_redirect_target;
use autumn_web::session::Session;
use autumn_web::{AppState, AutumnResult};
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use maud::Markup;
use serde::Deserialize;

use crate::routes::AdminPrefix;
use crate::templates::{ImpersonationBanner, impersonation_banner};

/// Request extractor resolving the impersonation banner for the current
/// session, or `None` when the session is not impersonating.
///
/// Infallible: a request with no `SessionLayer` (or no active impersonation)
/// simply yields `None`, so every admin page can take it unconditionally.
#[derive(Debug, Clone, Default)]
pub struct AdminImpersonation(impersonation::Impersonation);

impl AdminImpersonation {
    /// The active impersonation, if any.
    #[must_use]
    pub const fn state(&self) -> Option<&impersonation::ImpersonationState> {
        self.0.state()
    }

    /// Build the banner view-model for a page rendered under `prefix`.
    #[must_use]
    pub fn banner(
        &self,
        admin_prefix: &str,
        csrf_token: &str,
        csrf_form_field: &str,
    ) -> Option<ImpersonationBanner> {
        self.0
            .state()
            .map(|state| ImpersonationBanner::new(state, admin_prefix, csrf_token, csrf_form_field))
    }
}

impl FromRequestParts<AppState> for AdminImpersonation {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let inner = impersonation::Impersonation::from_request_parts(parts, state).await?;
        Ok(Self(inner))
    }
}

/// Render the impersonation banner for the current request, or `None` when the
/// session is not impersonating.
///
/// The one call an application makes to put the banner in its own layout.
/// `admin_prefix` is where the plugin is mounted (default `/admin`); the
/// "Stop impersonating" form posts to `{admin_prefix}/impersonate/stop`.
/// Pass an empty `csrf_token` when no `CsrfLayer` is installed — the hidden
/// field is then omitted rather than rendered blank.
pub async fn impersonation_banner_for(
    state: &AppState,
    session: &Session,
    admin_prefix: &str,
    csrf_token: &str,
    csrf_form_field: &str,
) -> Option<Markup> {
    let active = impersonation::impersonation_state(state, session).await?;
    Some(impersonation_banner(&ImpersonationBanner::new(
        &active,
        admin_prefix,
        csrf_token,
        csrf_form_field,
    )))
}

/// Form body for `POST {prefix}/impersonate`.
///
/// Deliberately carries **only** the target user id. The impersonated session's
/// role is resolved server-side by
/// [`ImpersonationPolicy::target_role`](autumn_web::auth::impersonation::ImpersonationPolicy::target_role);
/// accepting it from the request would let an operator mint a session more
/// privileged than the target really is.
#[derive(Debug, Deserialize)]
pub struct BeginForm {
    user_id: String,
    /// Where to send the browser afterwards. Validated as a same-origin
    /// relative path, so it can never become an open redirect.
    #[serde(default)]
    return_to: Option<String>,
}

/// `POST {prefix}/impersonate` — begin impersonating a user.
///
/// Mounted **inside** the admin role gate (and the step-up guard, when
/// enabled), then gated again by the app's
/// [`ImpersonationGate`](autumn_web::auth::impersonation::ImpersonationGate):
/// role membership alone is never sufficient.
pub async fn impersonate_begin(
    State(state): State<AppState>,
    session: Session,
    axum::extract::Form(form): axum::extract::Form<BeginForm>,
) -> AutumnResult<Response> {
    impersonation::begin_impersonation(&state, &session, form.user_id).await?;
    let target = form.return_to.as_deref().unwrap_or("/");
    Ok(Redirect::to(safe_redirect_target(target)).into_response())
}

/// `POST {prefix}/impersonate/stop` — revert to the original admin session.
///
/// Mounted **outside** the role gate on purpose: while impersonating, the
/// session no longer carries the admin role, so a gated revert would trap the
/// operator in the target's identity. It is self-gating — a session that is not
/// impersonating gets a `400` and nothing changes.
pub async fn impersonate_stop(
    State(state): State<AppState>,
    session: Session,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    body: axum::body::Bytes,
) -> AutumnResult<Response> {
    impersonation::end_impersonation(&state, &session).await?;
    // The banner posts the page it was rendered on, so reverting from an
    // application page returns there instead of dumping the operator into the
    // admin panel. Parsed straight from the body rather than through `Form<T>`
    // so a bare, body-less `POST` still reverts instead of being rejected for a
    // missing content type — the revert must never be the thing that fails.
    // Absent or unsafe values fall back to the admin prefix, and
    // `safe_redirect_target` makes an open redirect impossible either way.
    let return_to = form_urlencoded::parse(&body)
        .find(|(key, _)| key == "return_to")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let destination = if return_to.is_empty() {
        safe_redirect_target(&prefix).to_owned()
    } else {
        safe_redirect_target(&return_to).to_owned()
    };
    Ok(Redirect::to(&destination).into_response())
}
