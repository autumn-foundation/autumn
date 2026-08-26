//! Shared page layout.

use autumn_web::prelude::*;
use autumn_web::security::CsrfToken;

/// The CSRF token to embed in a form's hidden `_csrf` field, or `""` when
/// `CsrfLayer` isn't mounted (e.g. `[security.csrf] enabled = false`, as
/// `TestApp`'s default test config sets — see `tests/integration_test.rs`).
/// Handlers extract `Option<CsrfToken>` (not the bare, non-optional
/// `CsrfToken`) specifically so they degrade gracefully rather than 500ing
/// when the layer is absent; an empty field is harmless because enforcement
/// is off in exactly the same condition.
#[must_use]
pub fn csrf_value(csrf: &Option<CsrfToken>) -> &str {
    csrf.as_ref().map_or("", CsrfToken::token)
}

/// Wrap page `content` in the site chrome.
///
/// `csrf_token` is embedded in the logout form's hidden `_csrf` field (every
/// other form on the page carries its own, extracted by its own handler) —
/// the framework's `CsrfLayer` is on by default (see `autumn/src/config.rs`),
/// so every `<form method="post">` in this app needs one.
pub fn layout(title: &str, signed_in: bool, csrf_token: &str, content: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · teams" }
                link rel="stylesheet" href="/static/css/app.css";
                script src="/static/js/htmx.min.js" {}
            }
            body class="bg-gray-50 text-gray-900" {
                header class="border-b bg-white" {
                    nav class="max-w-3xl mx-auto flex items-center justify-between px-4 py-3" {
                        a href="/" class="font-bold text-lg" { "teams" }
                        div class="flex items-center gap-4 text-sm" {
                            @if signed_in {
                                a href="/members" class="text-gray-600 hover:text-gray-900" { "Members" }
                                form action="/logout" method="post" class="inline" {
                                    input type="hidden" name="_csrf" value=(csrf_token);
                                    button type="submit" class="text-gray-600 hover:text-gray-900" { "Log out" }
                                }
                            } @else {
                                a href="/login" class="text-gray-600 hover:text-gray-900" { "Log in" }
                                a href="/signup"
                                  class="px-3 py-1.5 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                                    "Sign up"
                                }
                            }
                        }
                    }
                }
                main class="max-w-3xl mx-auto px-4 py-8" {
                    (content)
                }
            }
        }
    }
}

/// Render a clear, styled error page for a dead invitation link (expired,
/// revoked, or already consumed) — AC6: "never a panic".
pub fn invitation_error_page(csrf_token: &str, message: &str) -> Markup {
    layout(
        "Invitation unavailable",
        false,
        csrf_token,
        html! {
            div class="bg-white rounded-lg shadow p-6 max-w-md" {
                h1 class="text-xl font-bold mb-2" { "This invitation isn't available" }
                p class="text-gray-600" { (message) }
                p class="mt-4" {
                    a href="/login" class="text-indigo-600 hover:underline" { "Go to login" }
                }
            }
        },
    )
}
