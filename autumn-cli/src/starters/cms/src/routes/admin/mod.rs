//! wp-admin — the back office.
//!
//! Every screen here is capability-gated through [`guard`]. There is no
//! `if user.role == "administrator"` anywhere in this module: the question a
//! screen asks is always "does this account hold this capability", which is
//! what makes adding a role a change to [`crate::capabilities`] alone.

pub mod appearance;
pub mod comments;
pub mod media;
pub mod posts;
pub mod settings;
pub mod terms;
pub mod tools;
pub mod users;

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;

use crate::capabilities::{Capability, Role};
use crate::content_types;
use crate::models::User;
use crate::repositories::{CommentRepository as _, PostRepository as _};

use super::site::{Csrf, Repos};

/// The outcome of a capability check: the account, or the response to send
/// instead.
///
/// A signed-out visitor gets a **redirect** to the login screen, because a 401
/// body on an admin URL is useless to a browser — this is what WordPress does
/// by bouncing to `wp-login.php`. A signed-in account that merely lacks the
/// capability gets a **403**: they are authenticated, so telling them their
/// role is insufficient leaks nothing they could not already infer, and a
/// silent redirect would look like a broken link.
pub type Guarded = Result<User, Response>;

/// Require a signed-in account holding `capability`. See [`Guarded`].
pub async fn guard(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    capability: Capability,
) -> AutumnResult<Guarded> {
    let Some(user) = repos.current_user(session).await? else {
        return Ok(Err(Redirect::to("/login").into_response()));
    };
    if !user.role().can(capability) {
        let message = format!(
            "Your role ({}) does not have the `{}` capability.",
            user.role().label(),
            capability.slug()
        );
        return Ok(Err((
            StatusCode::FORBIDDEN,
            layout(
                &user,
                csrf,
                "",
                "Not allowed",
                html! {
                    p class="text-gray-700" { (message) }
                    p class="mt-4" {
                        a href="/admin" class="text-indigo-700 hover:underline" {
                            "Back to the dashboard"
                        }
                    }
                },
            ),
        )
            .into_response()));
    }
    Ok(Ok(user))
}

/// Resolve a [`guard`] call, returning its denial response from the enclosing
/// handler.
///
/// Every admin handler starts with this. It is a macro rather than a `?`-able
/// error because the denial is a *rendered response* — a redirect or a styled
/// 403 — not an error condition, and routing it through `AutumnError` would
/// flatten both into the framework's generic error page.
#[macro_export]
macro_rules! require_capability {
    ($repos:expr, $session:expr, $csrf:expr, $capability:expr) => {
        match $crate::routes::admin::guard(&$repos, &$session, &$csrf, $capability).await? {
            Ok(user) => user,
            Err(response) => return Ok(response),
        }
    };
}

/// The admin chrome: sidebar navigation plus the page body.
///
/// The menu is filtered by capability, so a Contributor does not see a
/// "Settings" link that would 403 — a nav item you cannot use is a bug report
/// waiting to happen.
pub fn layout(user: &User, csrf: &Csrf, current: &str, title: &str, body: Markup) -> Markup {
    let role = user.role();
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Admin" }
                link rel="stylesheet" href="/static/css/app.css";
                script src=(autumn_web::htmx::HTMX_JS_PATH) {}
            }
            body class="bg-gray-100 text-gray-900" {
                a href="#admin-main"
                  class="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 \
                         focus:z-50 focus:px-4 focus:py-2 focus:bg-white focus:border \
                         focus:rounded focus:shadow" { "Skip to main content" }
                div class="flex min-h-screen" {
                    nav aria-label="Admin" class="w-56 shrink-0 bg-gray-900 text-gray-300 \
                                                  flex flex-col" {
                        a href="/admin" class="px-4 py-4 text-white font-semibold border-b \
                                               border-gray-800" { "Autumn CMS" }
                        ul class="flex-1 py-2 text-sm" {
                            @for (href, label, needed) in menu_items() {
                                @if role.can(needed) {
                                    li {
                                        a href=(href)
                                          aria-current=[(current == href).then_some("page")]
                                          class=(if current == href {
                                              "block px-4 py-2 bg-gray-800 text-white"
                                          } else {
                                              "block px-4 py-2 hover:bg-gray-800 hover:text-white"
                                          }) { (label) }
                                    }
                                }
                            }
                        }
                        div class="px-4 py-3 border-t border-gray-800 text-xs" {
                            p class="text-gray-400" { (user.public_name()) }
                            p class="text-gray-500 mb-2" { (role.label()) }
                            a href="/" class="hover:text-white" { "View site" }
                            " · "
                            form action="/logout" method="post" class="inline" {
                                (csrf.input())
                                button type="submit" class="hover:text-white" { "Log out" }
                            }
                        }
                    }
                    main id="admin-main" class="flex-1 min-w-0 p-8" {
                        h1 class="text-2xl font-bold mb-6" { (title) }
                        (body)
                    }
                }
            }
        }
    }
}

/// The admin menu: `(href, label, required capability)`.
fn menu_items() -> Vec<(String, String, Capability)> {
    let mut items = vec![(
        "/admin".to_owned(),
        "Dashboard".to_owned(),
        Capability::Read,
    )];
    // One entry per registered post type, so a custom type appears in the menu
    // the moment it is registered — no second registration step.
    for post_type in content_types::all_post_types() {
        items.push((
            format!("/admin/content/{}", post_type.slug),
            post_type.plural.to_owned(),
            Capability::EditPosts,
        ));
    }
    items.extend([
        (
            "/admin/media".to_owned(),
            "Media".to_owned(),
            Capability::UploadFiles,
        ),
        (
            "/admin/comments".to_owned(),
            "Comments".to_owned(),
            Capability::ModerateComments,
        ),
        (
            "/admin/terms/category".to_owned(),
            "Categories".to_owned(),
            Capability::ManageCategories,
        ),
        (
            "/admin/terms/post_tag".to_owned(),
            "Tags".to_owned(),
            Capability::ManageCategories,
        ),
        (
            "/admin/appearance".to_owned(),
            "Appearance".to_owned(),
            Capability::EditThemeOptions,
        ),
        (
            "/admin/users".to_owned(),
            "Users".to_owned(),
            Capability::ListUsers,
        ),
        (
            "/admin/tools".to_owned(),
            "Tools".to_owned(),
            Capability::ExportContent,
        ),
        (
            "/admin/settings".to_owned(),
            "Settings".to_owned(),
            Capability::ManageOptions,
        ),
    ]);
    items
}

/// The dashboard — WordPress's "At a Glance" panel.
#[get("/admin")]
pub async fn dashboard(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    // `EditPosts` rather than `Read` is the floor for the admin: a Subscriber
    // holds `read` but has no screen here, so gating on `read` would let them
    // in to an empty shell. This is the same predicate the post-login redirect
    // uses, so the two can never disagree about who lands where.
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);

    let mut counts = Vec::new();
    for post_type in content_types::all_post_types() {
        let published = repos
            .posts
            .count_by_post_type_and_status(post_type.slug.to_owned(), "publish".to_owned())
            .await
            .unwrap_or(0);
        let drafts = repos
            .posts
            .count_by_post_type_and_status(post_type.slug.to_owned(), "draft".to_owned())
            .await
            .unwrap_or(0);
        counts.push((
            post_type.plural.to_owned(),
            post_type.slug.to_owned(),
            published,
            drafts,
        ));
    }

    let pending_comments = repos
        .comments
        .count_by_status("pending".to_owned())
        .await
        .unwrap_or(0);

    let body = html! {
        div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4 mb-8" {
            @for (label, slug, published, drafts) in &counts {
                a href=(format!("/admin/content/{slug}"))
                  class="bg-white rounded-lg shadow p-5 hover:shadow-md transition-shadow" {
                    p class="text-sm text-gray-500" { (label) }
                    p class="text-3xl font-bold" { (published) }
                    p class="text-xs text-gray-400 mt-1" { (drafts) " draft" (if *drafts == 1 { "" } else { "s" }) }
                }
            }
            a href="/admin/comments?status=pending"
              class="bg-white rounded-lg shadow p-5 hover:shadow-md transition-shadow" {
                p class="text-sm text-gray-500" { "Comments awaiting moderation" }
                p class=(if pending_comments > 0 {
                    "text-3xl font-bold text-amber-600"
                } else {
                    "text-3xl font-bold"
                }) { (pending_comments) }
            }
        }

        section class="bg-white rounded-lg shadow p-5" {
            h2 class="font-semibold mb-3" { "Your role" }
            p class="text-sm text-gray-600 mb-3" {
                "You are signed in as a " strong { (user.role().label()) } "."
                " These are the capabilities that grants:"
            }
            ul class="flex flex-wrap gap-2" {
                @for capability in crate::capabilities::ALL_CAPABILITIES {
                    @if user.role().can(*capability) {
                        li class="px-2 py-0.5 bg-green-50 text-green-800 rounded text-xs font-mono" {
                            (capability.slug())
                        }
                    } @else {
                        li class="px-2 py-0.5 bg-gray-50 text-gray-400 rounded text-xs font-mono \
                                  line-through" {
                            (capability.slug())
                        }
                    }
                }
            }
        }
    };

    Ok(layout(&user, &csrf, "/admin", "Dashboard", body).into_response())
}

/// Shared helper: a role `<select>` for the user screens.
#[must_use]
pub fn role_options(selected: Role) -> Markup {
    html! {
        @for role in crate::capabilities::ALL_ROLES {
            option value=(role.slug()) selected[*role == selected] { (role.label()) }
        }
    }
}
