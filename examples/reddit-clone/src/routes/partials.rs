//! Server-driven HTML partials for htmx hydration.
//!
//! Static pre-rendered pages (e.g. `/about`) can't embed session state at
//! build time. They render an anonymous nav skeleton and request this endpoint
//! on page load so the correct auth state is swapped in without a full reload.

use autumn_web::Markup;
use autumn_web::prelude::*;

use super::layout::nav_auth_content;

/// Return the nav auth fragment for the current session.
///
/// The static `/about` page fires `hx-get="/_partials/nav-auth"` on load so
/// that logged-in users see their username instead of the anonymous buttons.
/// Returns `nav_auth_content` (no htmx trigger) so the swap doesn't loop.
#[public]
#[get("/_partials/nav-auth")]
pub async fn nav_auth(session: Session) -> AutumnResult<Markup> {
    let username: Option<String> = session.get("username").await;
    Ok(nav_auth_content(username.as_deref()))
}
