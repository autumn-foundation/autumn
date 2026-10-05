//! H10: error pages that know the stockroom, with `.error_pages(..)`.
//!
//! A scanner reads a barcode and opens `/products/{sku}`. If the SKU is not
//! in stock, the default 404 page shows the status and a link to `/`. Staff
//! then need the supplier catalog to order the item. This renderer adds that
//! link, inside the app's own page frame.

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
            "Not in stock",
            &html! {
                h1 { "SKU " (sku) " is not in the stockroom" }
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
