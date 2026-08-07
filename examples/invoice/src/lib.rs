//! Minimal invoice example — issue #1317.
//!
//! Demonstrates rendering a downloadable PDF from the same Maud view used for
//! the on-screen detail page: `invoice_view` is the single source of truth,
//! and the `/invoices/{id}/pdf` handler is five lines long.
//!
//! No database: invoices are synthesized in memory from the requested id, so
//! this example stays dependency-free like `hello`.

use autumn_web::extract::Path;
use autumn_web::pdf::Pdf;
use autumn_web::prelude::*;
use autumn_web::time::Clock;
use chrono::{DateTime, Utc};

struct LineItem {
    name: &'static str,
    quantity: u32,
    unit_price_cents: u64,
}

struct Invoice {
    id: i64,
    customer: String,
    items: Vec<LineItem>,
}

impl Invoice {
    /// Synthesize a demo invoice for `id` — no database required.
    fn demo(id: i64) -> Self {
        Self {
            id,
            customer: format!("Customer #{id}"),
            items: vec![
                LineItem {
                    name: "Widget",
                    quantity: 2,
                    unit_price_cents: 1_500,
                },
                LineItem {
                    name: "Gadget",
                    quantity: 1,
                    unit_price_cents: 1_200,
                },
            ],
        }
    }

    fn total_cents(&self) -> u64 {
        self.items
            .iter()
            .map(|item| item.quantity as u64 * item.unit_price_cents)
            .sum()
    }
}

fn format_cents(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// The single view shared by the on-screen detail page and the PDF export.
fn invoice_view(invoice: &Invoice, generated_at: DateTime<Utc>) -> Markup {
    html! {
        h1 { "Invoice #" (invoice.id) }
        p { "Billed to: " (invoice.customer) }
        table {
            tr { th { "Item" } th { "Qty" } th { "Amount" } }
            @for item in &invoice.items {
                tr {
                    td { (item.name) }
                    td { (item.quantity) }
                    td { (format_cents(item.quantity as u64 * item.unit_price_cents)) }
                }
            }
        }
        p { strong { "Total: " (format_cents(invoice.total_cents())) } }
        p { "Generated at " (generated_at.to_rfc3339()) }
    }
}

#[get("/invoices/{id}")]
pub async fn invoice_detail(id: Path<i64>, clock: Clock) -> Markup {
    invoice_view(&Invoice::demo(*id), clock.now())
}

#[get("/invoices/{id}/pdf")]
pub async fn invoice_pdf(id: Path<i64>, clock: Clock) -> Pdf {
    let invoice = Invoice::demo(*id);
    let filename = format!("invoice-{}.pdf", invoice.id);
    Pdf::from_markup(invoice_view(&invoice, clock.now())).filename(filename)
}
