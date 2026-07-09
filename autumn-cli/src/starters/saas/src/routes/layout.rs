//! Shared page layout.

use autumn_web::prelude::*;

/// Wrap page `content` in the site chrome.
///
/// `signed_in` toggles the nav between "Log in / Sign up" and "Dashboard /
/// Log out" so every page reflects the session state.
pub fn layout(title: &str, signed_in: bool, content: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · {{project_name}}" }
                link rel="stylesheet" href="/static/css/app.css";
                script src="/static/js/htmx.min.js" {}
            }
            body class="bg-gray-50 text-gray-900" {
                header class="border-b bg-white" {
                    nav class="max-w-3xl mx-auto flex items-center justify-between px-4 py-3" {
                        a href="/" class="font-bold text-lg" { "{{project_name}}" }
                        div class="flex items-center gap-4 text-sm" {
                            @if signed_in {
                                a href="/dashboard" class="text-gray-600 hover:text-gray-900" { "Dashboard" }
                                form action="/logout" method="post" class="inline" {
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
