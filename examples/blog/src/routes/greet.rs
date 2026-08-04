//! Demonstrates the autumn-web i18n module end-to-end, including
//! locale-prefixed routing and the path-preserving switcher (issue #1251).
//!
//! Loads its strings from `i18n/{en,es}.ftl` via the bundle registered in
//! `main.rs` with `.i18n_auto()`. With `[i18n] locale_prefix_enabled = true`
//! in `autumn.toml`, this same handler serves `/en/greet` and `/es/greet`
//! directly — `/greet` (bare) 308-redirects to whichever locale negotiation
//! picks. `Locale` resolves the request locale (URL prefix first), `t!()`
//! performs the lookup, and `locale_switcher()` renders the language links
//! with zero hand-built `<a href>`s.

use autumn_web::prelude::*;

use super::posts::layout;

#[get("/greet")]
pub async fn greet(locale: Locale, uri: autumn_web::reexports::http::Uri) -> Markup {
    let title = t!(locale, "greet.title");
    let supported: &[String] = locale.bundle().map_or(&[], |b| b.supported_locales());
    let path_and_query = match uri.query() {
        Some(query) => format!("{}?{query}", uri.path()),
        None => uri.path().to_owned(),
    };
    let body = html! {
        article class="space-y-6" {
            header class="border-b border-stone-200 pb-4" {
                h1 class="text-3xl font-bold tracking-tight text-stone-900" {
                    (title)
                }
            }

            p class="text-stone-700 leading-relaxed" {
                (t!(locale, "greet.greeting", name = "Ada"))
            }

            // Locale switcher (issue #1251) — one call, preserves the
            // current path and query; only the locale segment changes.
            div class="bg-amber-50 border border-amber-200 rounded-lg p-4 space-y-2" {
                p class="text-sm text-amber-900 font-medium" {
                    (t!(locale, "nav.locale.label")) ":"
                }
                (locale_switcher(&path_and_query, locale.tag(), supported))
                p class="text-xs text-amber-800 mt-2" {
                    (t!(locale, "greet.switcher_help"))
                }
                p class="text-xs text-amber-800" {
                    (t!(locale, "greet.try"))
                }
            }

            p class="text-xs text-stone-500" {
                "Locale: " code class="px-1 bg-stone-100 rounded font-mono" { (locale.tag()) }
            }
        }
    };
    layout(&locale, Some(&path_and_query), &title, body)
}
