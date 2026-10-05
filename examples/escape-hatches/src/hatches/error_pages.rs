//! H10: the stockroom error pages.

use autumn_web::error_pages::{ErrorContext, ErrorPageRenderer};
use maud::Markup;

/// Error pages that know the stockroom.
pub struct StockroomErrorPages;

impl ErrorPageRenderer for StockroomErrorPages {
    fn render_error(&self, _ctx: &ErrorContext) -> Markup {
        todo!("H10")
    }
}
