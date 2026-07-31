//! Static about page — demonstrates `#[static_get]` for pre-rendered content.

use autumn_web::i18n::Locale;
use autumn_web::seo::SeoMeta;
use autumn_web::{Markup, html, static_get};

use super::posts::layout_with_seo;

/// A static about page rendered at build time.
///
/// Uses `#[static_get]` instead of `#[get]`, marking it for pre-rendering
/// by `autumn build`. At runtime, the pre-rendered HTML is served from disk
/// without touching the application.
///
/// The page's meta tags are declared entirely on the attribute via `seo(...)`
/// and arrive in the handler through the [`SeoMeta`] extractor, so a fully
/// static page needs no hand-built builder at all. `#[static_get]` honours the
/// argument like any other route macro, so the pre-rendered HTML carries the
/// tags.
#[static_get(
    "/about",
    seo(
        title = "About \u{2022} Autumn Blog",
        description = "Why this blog exists and what the Autumn web framework is for.",
        og_type = "website"
    )
)]
pub async fn about(locale: Locale, seo: SeoMeta) -> Markup {
    layout_with_seo(
        &locale,
        seo,
        html! {
            article {
                a href="/"
                   class="inline-flex items-center gap-1 text-sm text-stone-600 \
                          hover:text-amber-700 transition-colors mb-8" {
                    "\u{2190} Back to blog"
                }

                header class="mb-8" {
                    h1 class="text-3xl font-bold tracking-tight text-stone-900 mb-3" {
                        "About This Blog"
                    }
                    p class="text-stone-500 text-sm" {
                        "A demo blog built with the Autumn web framework"
                    }
                }

                div class="prose prose-stone max-w-none space-y-4" {
                    p class="text-stone-700 leading-relaxed" {
                        "This blog is built with "
                        strong { "Autumn" }
                        " \u{2014} an opinionated, convention-over-configuration web framework for Rust. "
                        "It assembles Axum, Maud, Diesel, htmx, and Tailwind CSS into a cohesive "
                        "full-stack experience inspired by Spring Boot."
                    }

                    h2 class="text-xl font-semibold text-stone-900 pt-4" {
                        "Tech Stack"
                    }

                    ul class="space-y-2 text-stone-700" {
                        li class="flex items-start gap-2" {
                            span class="text-amber-600 mt-1" { "\u{2022}" }
                            span { strong { "Axum" } " \u{2014} Fast, ergonomic HTTP routing and middleware" }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-amber-600 mt-1" { "\u{2022}" }
                            span { strong { "Maud" } " \u{2014} Type-safe, compiled HTML templates" }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-amber-600 mt-1" { "\u{2022}" }
                            span { strong { "Diesel" } " \u{2014} Safe, composable database queries" }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-amber-600 mt-1" { "\u{2022}" }
                            span { strong { "htmx" } " \u{2014} Lightweight interactivity without JavaScript frameworks" }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-amber-600 mt-1" { "\u{2022}" }
                            span { strong { "Tailwind CSS" } " \u{2014} Utility-first styling" }
                        }
                    }

                    h2 class="text-xl font-semibold text-stone-900 pt-4" {
                        "Hybrid Rendering"
                    }

                    p class="text-stone-700 leading-relaxed" {
                        "This about page uses "
                        code class="px-1.5 py-0.5 bg-stone-100 rounded text-sm font-mono" { "#[static_get]" }
                        " instead of "
                        code class="px-1.5 py-0.5 bg-stone-100 rounded text-sm font-mono" { "#[get]" }
                        ". That means it can be pre-rendered at build time by "
                        code class="px-1.5 py-0.5 bg-stone-100 rounded text-sm font-mono" { "autumn build" }
                        " and served as a static HTML file \u{2014} zero compute per request. "
                        "Dynamic routes like the post listing and admin panel continue to render on every request."
                    }

                    div class="bg-amber-50 border border-amber-200 rounded-lg p-4 mt-6" {
                        p class="text-sm text-amber-900" {
                            strong { "\u{1F342} Fun fact: " }
                            "Autumn is the first Rust web framework with integrated hybrid rendering \u{2014} "
                            "static generation and server rendering in a single coherent stack."
                        }
                    }
                }
            }
        },
    )
}
