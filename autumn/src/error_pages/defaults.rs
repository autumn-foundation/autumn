//! Default styled error pages using Maud and Tailwind.

use super::renderer::{ErrorContext, ErrorPageRenderer};
use maud::{Markup, PreEscaped, html};

/// Default error page renderer using Maud templates with Tailwind styling.
///
/// Produces clean, minimal, professional error pages with:
/// - Dark background option via Tailwind's dark mode classes
/// - Status code badge
/// - Contextual messages (path for 404, request ID for 500, field details for 422)
/// - "Go back" link
pub struct DefaultErrorPages;

impl ErrorPageRenderer for DefaultErrorPages {
    fn render_404(&self, ctx: &ErrorContext) -> Markup {
        error_page_layout(
            ctx,
            &html! {
                div class="text-center" {
                    div class="text-8xl font-bold text-gray-200 dark:text-gray-700 select-none" {
                        "404"
                    }
                    h1 class="mt-4 text-2xl font-semibold text-gray-900 dark:text-gray-100" {
                        "Page not found"
                    }
                    p class="mt-2 text-gray-600 dark:text-gray-400" {
                        "The page "
                        code class="px-2 py-0.5 bg-gray-100 dark:bg-gray-800 rounded text-sm font-mono text-gray-800 dark:text-gray-300" {
                            (ctx.path)
                        }
                        " could not be found."
                    }
                    (home_link())
                }
            },
        )
    }

    fn render_500(&self, ctx: &ErrorContext) -> Markup {
        error_page_layout(
            ctx,
            &html! {
                div class="text-center" {
                    div class="text-8xl font-bold text-gray-200 dark:text-gray-700 select-none" {
                        "500"
                    }
                    h1 class="mt-4 text-2xl font-semibold text-gray-900 dark:text-gray-100" {
                        "Internal server error"
                    }
                    p class="mt-2 text-gray-600 dark:text-gray-400" {
                        "Something went wrong. Please try again later."
                    }
                    @if let Some(ref req_id) = ctx.request_id {
                        p class="mt-4 text-xs text-gray-400 dark:text-gray-500 font-mono" {
                            "Request ID: " (req_id)
                        }
                    }
                    (home_link())
                }
            },
        )
    }

    fn render_422(&self, ctx: &ErrorContext) -> Markup {
        error_page_layout(
            ctx,
            &html! {
                div class="text-center" {
                    div class="text-8xl font-bold text-gray-200 dark:text-gray-700 select-none" {
                        "422"
                    }
                    h1 class="mt-4 text-2xl font-semibold text-gray-900 dark:text-gray-100" {
                        "Validation error"
                    }
                    p class="mt-2 text-gray-600 dark:text-gray-400" {
                        (ctx.message)
                    }
                    @if let Some(ref details) = ctx.details {
                        div class="mt-6 max-w-md mx-auto text-left" {
                            @for (field, errors) in details {
                                div class="mb-3" {
                                    p class="text-sm font-medium text-gray-700 dark:text-gray-300" {
                                        (field)
                                    }
                                    @for error in errors {
                                        p class="text-sm text-red-600 dark:text-red-400 ml-2" {
                                            "- " (error)
                                        }
                                    }
                                }
                            }
                        }
                    }
                    (home_link())
                }
            },
        )
    }

    fn render_error(&self, ctx: &ErrorContext) -> Markup {
        let status_code = ctx.status.as_u16();
        let reason = ctx.status.canonical_reason().unwrap_or("Error");

        error_page_layout(
            ctx,
            &html! {
                div class="text-center" {
                    div class="text-8xl font-bold text-gray-200 dark:text-gray-700 select-none" {
                        (status_code)
                    }
                    h1 class="mt-4 text-2xl font-semibold text-gray-900 dark:text-gray-100" {
                        (reason)
                    }
                    p class="mt-2 text-gray-600 dark:text-gray-400" {
                        (ctx.message)
                    }
                    @if let Some(ref req_id) = ctx.request_id {
                        p class="mt-4 text-xs text-gray-400 dark:text-gray-500 font-mono" {
                            "Request ID: " (req_id)
                        }
                    }
                    (home_link())
                }
            },
        )
    }
}

/// Shared HTML layout wrapper for all error pages.
/// Reusable "Go to homepage" button for error pages
fn home_link() -> Markup {
    html! {
        div class="mt-8" {
            a href="/"
              class="inline-block px-4 py-2 text-sm font-medium text-white bg-gray-900 dark:bg-gray-100 dark:text-gray-900 rounded-md hover:bg-gray-700 dark:hover:bg-gray-300 transition-colors" {
                "Go to homepage"
            }
        }
    }
}

fn error_page_layout(ctx: &ErrorContext, content: &Markup) -> Markup {
    let status_code = ctx.status.as_u16();
    let reason = ctx.status.canonical_reason().unwrap_or("Error");
    let title = format!("{status_code} {reason}");

    html! {
        (maud::DOCTYPE)
        html lang="en" class="dark" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " | Autumn" }
                // Inline minimal Tailwind-compatible styles as a fallback.
                // When Tailwind CSS is loaded, its utility classes take over.
                (PreEscaped(FALLBACK_STYLES))
            }
            body class="min-h-screen bg-white dark:bg-gray-950 flex items-center justify-center p-4" {
                main class="w-full max-w-lg" {
                    (content)
                }
            }
        }
    }
}

/// Minimal inline CSS fallback so error pages look reasonable even without
/// Tailwind CSS loaded. Provides the essential layout and dark mode styling.
///
/// Built at compile time by concatenating the shared [`crate::ui::tokens::TOKENS_CSS`]
/// block with a small set of layout rules. The tokens carry the dark scheme.
const FALLBACK_STYLES: &str = concat!(
    "<style>",
    include_str!("../ui/tokens.css"),
    "
body {
    font-family: var(--font-family);
    margin: 0;
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 1rem;
    background: var(--bg);
    color: var(--text);
}
.text-center { text-align: center; }
code {
    padding: 0.125rem 0.5rem;
    border-radius: 0.25rem;
    font-size: 0.875rem;
    font-family: ui-monospace, monospace;
}
a {
    display: inline-block;
    padding: 0.5rem 1rem;
    font-size: 0.875rem;
    border-radius: 0.375rem;
    text-decoration: none;
    transition: opacity 0.15s;
}
a:hover { opacity: 0.8; }
</style>"
);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn make_ctx(status: StatusCode) -> ErrorContext {
        ErrorContext {
            status,
            message: "test error".into(),
            path: "/test/path".into(),
            request_id: Some("req-123".into()),
            details: None,
            is_dev: false,
        }
    }

    /// Issue #3396: the core tokens carry the dark scheme. The error page
    /// must not keep a second, partial dark palette.
    #[test]
    fn fallback_styles_use_the_core_dark_tokens() {
        assert!(FALLBACK_STYLES.contains(crate::ui::tokens::TOKENS_CSS));
        assert!(
            !FALLBACK_STYLES.contains("prefers-color-scheme"),
            "{FALLBACK_STYLES}"
        );
    }

    #[test]
    fn default_404_contains_path() {
        let pages = DefaultErrorPages;
        let html = pages.render_404(&make_ctx(StatusCode::NOT_FOUND));
        let s = html.into_string();
        assert!(
            s.contains("/test/path"),
            "404 page should show request path"
        );
        assert!(s.contains("Page not found"));
        assert!(s.contains("404"));
    }

    #[test]
    fn default_500_contains_request_id() {
        let pages = DefaultErrorPages;
        let html = pages.render_500(&make_ctx(StatusCode::INTERNAL_SERVER_ERROR));
        let s = html.into_string();
        assert!(s.contains("req-123"), "500 page should show request ID");
        assert!(s.contains("Internal server error"));
        assert!(s.contains("500"));
    }

    #[test]
    fn default_500_hides_error_details_in_prod() {
        let pages = DefaultErrorPages;
        let ctx = ErrorContext {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "secret db password exposed".into(),
            path: "/api/data".into(),
            request_id: None,
            details: None,
            is_dev: false,
        };
        let html = pages.render_500(&ctx);
        let s = html.into_string();
        assert!(
            !s.contains("secret db password"),
            "500 page must not show error details in prod"
        );
    }

    #[test]
    fn default_422_shows_validation_details() {
        let mut details = std::collections::HashMap::new();
        details.insert("email".into(), vec!["must be valid".into()]);

        let pages = DefaultErrorPages;
        let ctx = ErrorContext {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "Validation failed".into(),
            path: "/signup".into(),
            request_id: None,
            details: Some(details),
            is_dev: false,
        };
        let html = pages.render_422(&ctx);
        let s = html.into_string();
        assert!(s.contains("email"), "422 page should show field names");
        assert!(s.contains("must be valid"), "422 page should show errors");
        assert!(s.contains("422"));
    }

    #[test]
    fn default_generic_error_page() {
        let pages = DefaultErrorPages;
        let ctx = make_ctx(StatusCode::FORBIDDEN);
        let html = pages.render_error(&ctx);
        let s = html.into_string();
        assert!(s.contains("403"));
        assert!(s.contains("Forbidden"));
    }

    #[test]
    fn error_page_is_valid_html() {
        let pages = DefaultErrorPages;
        let html = pages.render_404(&make_ctx(StatusCode::NOT_FOUND));
        let s = html.into_string();
        assert!(s.contains("<!DOCTYPE html>"));
        assert!(s.contains("<html"));
        assert!(s.contains("</html>"));
    }

    #[test]
    fn default_pages_do_not_use_javascript_urls() {
        let pages = DefaultErrorPages;
        let html = pages.render_422(&make_ctx(StatusCode::UNPROCESSABLE_ENTITY));
        let s = html.into_string();
        assert!(
            !s.contains("javascript:"),
            "default error pages must work under script-src 'self'",
        );
    }

    #[test]
    fn test_canonical_reason_fallback() {
        let pages = DefaultErrorPages;
        let ctx = make_ctx(StatusCode::from_u16(599).unwrap());
        let html = pages.render_error(&ctx);
        let s = html.into_string();
        assert!(s.contains("599"));
        assert!(s.contains("Error"));
    }
}
