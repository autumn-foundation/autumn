//! Styled error pages and dev-mode error badge overlay.
//!
//! Autumn provides default HTML error pages for common HTTP error codes
//! (404, 422, 500) and a Next.js-style error badge overlay in dev mode.
//!
//! # Error page override
//!
//! Implement [`ErrorPageRenderer`] to provide custom error pages:
//!
//! ```rust,no_run
//! use autumn_web::error_pages::{ErrorPageRenderer, ErrorContext};
//! use maud::{Markup, html};
//!
//! struct MyErrorPages;
//!
//! impl ErrorPageRenderer for MyErrorPages {
//!     fn render_error(&self, ctx: &ErrorContext) -> Markup {
//!         html! {
//!             h1 { (ctx.status.as_u16()) " - " (ctx.path) }
//!         }
//!     }
//! }
//! ```
//!
//! Register it on the app builder:
//!
//! ```rust,ignore
//! autumn_web::app()
//!     .error_pages(MyErrorPages)
//!     .routes(routes![...])
//!     .run()
//!     .await;
//! ```
//!
//! # Dev error badge
//!
//! In dev profile, error responses automatically include a floating badge
//! in the bottom-left corner showing status code, error type, and message.
//! The badge uses inline CSS so it works even when Tailwind is unavailable.
//! It is **never** shown in production.

mod defaults;
pub(crate) mod dev_badge;
pub(crate) mod renderer;
pub(crate) mod source;

pub use defaults::DefaultErrorPages;
pub use renderer::{ErrorContext, ErrorPageRenderer};

use axum::http::StatusCode;
use maud::Markup;
use std::sync::Arc;

/// Render an error page using the given renderer (or defaults).
///
/// Returns the full HTML response body for the given status code.
pub(crate) fn render_error_page(
    renderer: &dyn ErrorPageRenderer,
    status: StatusCode,
    ctx: &ErrorContext,
) -> Markup {
    match status {
        StatusCode::NOT_FOUND => renderer.render_404(ctx),
        StatusCode::UNPROCESSABLE_ENTITY => renderer.render_422(ctx),
        StatusCode::INTERNAL_SERVER_ERROR => renderer.render_500(ctx),
        _ => renderer.render_error(ctx),
    }
}

/// Shared error page renderer stored in app state / middleware.
pub(crate) type SharedRenderer = Arc<dyn ErrorPageRenderer>;

/// Create the default shared renderer.
pub(crate) fn default_renderer() -> SharedRenderer {
    Arc::new(DefaultErrorPages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maud::html;

    struct TestRenderer;

    impl ErrorPageRenderer for TestRenderer {
        fn render_error(&self, ctx: &ErrorContext) -> Markup {
            html! { "Custom " (ctx.status.as_u16()) }
        }

        fn render_404(&self, _ctx: &ErrorContext) -> Markup {
            html! { "Custom 404" }
        }

        fn render_422(&self, _ctx: &ErrorContext) -> Markup {
            html! { "Custom 422" }
        }

        fn render_500(&self, _ctx: &ErrorContext) -> Markup {
            html! { "Custom 500" }
        }
    }

    #[test]
    fn render_error_page_delegates_correctly() {
        let renderer = TestRenderer;
        let ctx = ErrorContext {
            status: StatusCode::OK,
            path: "/".to_string(),
            message: "Test message".to_string(),
            request_id: None,
            details: None,
            is_dev: false,
        };

        assert_eq!(
            render_error_page(&renderer, StatusCode::NOT_FOUND, &ctx).into_string(),
            "Custom 404"
        );
        assert_eq!(
            render_error_page(&renderer, StatusCode::UNPROCESSABLE_ENTITY, &ctx).into_string(),
            "Custom 422"
        );
        assert_eq!(
            render_error_page(&renderer, StatusCode::INTERNAL_SERVER_ERROR, &ctx).into_string(),
            "Custom 500"
        );

        let ctx_400 = ErrorContext {
            status: StatusCode::BAD_REQUEST,
            path: "/".to_string(),
            message: "Test message".to_string(),
            request_id: None,
            details: None,
            is_dev: false,
        };
        assert_eq!(
            render_error_page(&renderer, StatusCode::BAD_REQUEST, &ctx_400).into_string(),
            "Custom 400"
        );
    }

    #[test]
    fn default_renderer_creates_default_error_pages() {
        let renderer = default_renderer();
        let ctx = ErrorContext {
            status: StatusCode::BAD_REQUEST,
            path: "/".to_string(),
            message: "Test message".to_string(),
            request_id: None,
            details: None,
            is_dev: false,
        };

        let html = render_error_page(&*renderer, StatusCode::BAD_REQUEST, &ctx).into_string();
        assert!(html.contains("html"));
        assert!(html.contains("400"));
    }
}
