//! Static about page — demonstrates `#[static_get]` for pre-rendered content.
//!
//! This page is rendered at build time by `autumn build` and served as
//! static HTML — zero compute per request. It showcases all the Autumn
//! features used in this Reddit clone example.

use autumn_web::seo::SeoMeta;
use autumn_web::{Markup, html, static_get};

use super::layout::layout_with_seo;

/// The about page, pre-rendered at build time.
///
/// The `seo(...)` argument on the attribute holds every meta value this page
/// needs, and the `SeoMeta` parameter receives them. The handler adds nothing:
/// a page whose text never changes needs no per-request builder.
///
/// `#[static_get]` honours `seo(...)` in the same way as `#[get]`, so the
/// pre-rendered HTML carries the tags too.
///
/// This route also puts `/about` in `/sitemap.xml`. The framework derives a
/// sitemap entry for each `#[static_get]` path when `[seo] base_url` is set,
/// so `RedditSitemapSource` does not list this page. See `docs/guide/seo.md`.
#[allow(clippy::too_many_lines)] // Template-heavy function
#[static_get(
    "/about",
    seo(
        title = "About \u{2022} Autumn Reddit",
        description = "Autumn Reddit is a demo link-sharing site. It shows every major feature of \
                       the Autumn web framework for Rust in one application.",
        og_type = "website",
        twitter_card = "summary"
    )
)]
pub async fn about(seo: SeoMeta) -> Markup {
    layout_with_seo(
        seo,
        None,
        None,
        html! {
            div class="max-w-3xl mx-auto" {
                h1 class="text-3xl font-bold mb-6" { "About Autumn Reddit" }

                div class="bg-white rounded-lg shadow-sm border border-gray-200 p-6 space-y-6" {
                    p class="text-gray-700 leading-relaxed" {
                        "This is a Reddit clone built with "
                        strong { "Autumn" }
                        " \u{2014} a convention-over-configuration web framework for Rust "
                        "inspired by Spring Boot. It demonstrates every major feature of the framework "
                        "in a single, cohesive application."
                    }

                    h2 class="text-xl font-semibold text-gray-900 pt-2" {
                        "Features Demonstrated"
                    }

                    ul class="space-y-3 text-gray-700" {
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Route macros" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[get]" }
                                ", "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[post]" }
                                ", "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[delete]" }
                                ", "
                                code class="text-sm bg-gray-100 px-1 rounded" { "routes![]" }
                                ", and "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[autumn_web::main]" }
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Hybrid rendering" }
                                " \u{2014} This about page uses "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[static_get]" }
                                " for build-time pre-rendering via "
                                code class="text-sm bg-gray-100 px-1 rounded" { "autumn build" }
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Database ergonomics" }
                                " \u{2014} Async Postgres with Diesel, "
                                code class="text-sm bg-gray-100 px-1 rounded" { "Db" }
                                " extractor, "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[model]" }
                                " macro, and embedded migrations"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Repository pattern" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[repository]" }
                                " with derived queries, generated REST API, and mutation hooks"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Authentication" }
                                " \u{2014} Session cookies, bcrypt password hashing, "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[secured]" }
                                " route protection, and role-based access"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "CSRF protection" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "CsrfToken" }
                                " extractor with hidden form fields"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Validation" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[validate]" }
                                " on model fields (length, URL)"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Background tasks" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "#[scheduled(every = \"15m\")]" }
                                " hot-rank recalculation"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Mutation hooks" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "MutationHooks" }
                                " for auto-slug generation and logging on post create/update"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "HTML stack" }
                                " \u{2014} Maud templates, bundled htmx for inline voting, "
                                "Tailwind CSS styling, static asset serving"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Configuration" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "autumn.toml" }
                                " + "
                                code class="text-sm bg-gray-100 px-1 rounded" { "autumn-dev.toml" }
                                " profile overrides"
                            }
                        }
                        li class="flex items-start gap-2" {
                            span class="text-orange-500 mt-1 font-bold" { "\u{2022}" }
                            span {
                                strong { "Operations" }
                                " \u{2014} "
                                code class="text-sm bg-gray-100 px-1 rounded" { "/health" }
                                ", "
                                code class="text-sm bg-gray-100 px-1 rounded" { "/actuator/*" }
                                " endpoints, structured logging, graceful shutdown"
                            }
                        }
                    }

                    div class="bg-orange-50 border border-orange-200 rounded-lg p-4 mt-4" {
                        p class="text-sm text-orange-800" {
                            strong { "Note: " }
                            "This page was pre-rendered at build time using "
                            code class="text-sm bg-orange-100 px-1 rounded" { "#[static_get]" }
                            " and is served as static HTML with zero runtime cost."
                        }
                    }
                }
            }
        },
    )
}
