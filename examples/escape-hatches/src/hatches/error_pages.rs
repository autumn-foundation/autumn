//! H10: error pages that know the stockroom, with `.error_pages(..)`.
//!
//! The default error pages use the framework's own look, with a link to `/`.
//! This renderer puts each error page (an unknown route, a 409, a 500) in
//! the app's page frame, with its navigation.
//!
//! It also helps with one case. Staff open `/products/{sku}` in a browser.
//! If no product has that SKU, staff need the supplier catalog to order the
//! item. So the 404 page for that path links to the SKU in the catalog.

use autumn_web::error_pages::{ErrorContext, ErrorPageRenderer};
use maud::{Markup, html};

use crate::pages::layout;

/// Error pages in the stockroom frame.
pub struct StockroomErrorPages;

impl ErrorPageRenderer for StockroomErrorPages {
    fn render_404(&self, ctx: &ErrorContext) -> Markup {
        let sku = ctx
            .path
            .strip_prefix("/products/")
            .filter(|sku| !sku.is_empty() && !sku.contains('/'));
        let Some(sku) = sku else {
            return self.render_error(ctx);
        };
        layout(
            "Unknown SKU",
            &html! {
                h1 { "Unknown SKU " (sku) }
                p { a href={ "/supplier/items/" (sku) } { "Find " (sku) " in the supplier catalog" } }
                p { a href="/" { "Back to products" } }
            },
        )
    }

    fn render_error(&self, ctx: &ErrorContext) -> Markup {
        let reason = ctx.status.canonical_reason().unwrap_or("Error");
        layout(
            reason,
            &html! {
                h1 { (ctx.status.as_u16()) " " (reason) }
                p { (ctx.message) }
                @if let Some(id) = &ctx.request_id {
                    p { small { "Request ID: " (id) } }
                }
                p { a href="/" { "Back to products" } }
            },
        )
    }
}
