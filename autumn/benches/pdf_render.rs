//! Renders a realistic multi-page invoice through `autumn_web::pdf::Pdf`,
//! the same `Pdf::from_html`/`Pdf::render` path `examples/invoice`'s
//! `invoice_pdf` handler drives from a real `Invoice` (heading, a "billed
//! to" line, a line-items table, bold total, status/timestamp lines), so
//! `valgrind --tool=callgrind|dhat` can attribute the per-render cost of the
//! HTML parser (`pdf::html`) and the layout walker/word-wrapper/PDF writer
//! (`pdf::layout`) — see that module's docs for why this is a deliberately
//! small HTML-subset layout engine, not a CSS box-model one.
//!
//! The table carries 60 line items (three columns each), long enough to
//! paginate across multiple pages — a plausible shape for a monthly usage
//! invoice or a report export, not a stress payload. Item names vary in
//! length (some wrap, most don't) so the word-wrapper's both paths run.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing: it is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench pdf_render --features pdf
//! BIN=$(find target/release/deps -maxdepth 1 -name "pdf_render-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 500
//! callgrind_annotate --threshold=80 callgrind.out | head -60
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 500
//! ```
//!
//! `--iterations N` renders the 60-row invoice N times after a fixed
//! 20-render warm-up.

use std::fmt::Write as _;
use std::hint::black_box;

use autumn_web::pdf::Pdf;

/// Realistic line-item names, cycled across rows: a mix of short and long
/// so the word-wrapper's wrap and no-wrap paths both run, matching how a
/// real product catalog reads (a few long descriptive names, mostly short
/// SKUs).
const ITEM_NAMES: &[&str] = &[
    "Widget",
    "Premium support plan (monthly)",
    "API usage overage — 10k requests",
    "Gadget",
    "Onboarding & migration assistance",
    "Storage add-on (100GB)",
    "Sprocket",
    "Custom integration development",
];

fn invoice_html(rows: usize) -> String {
    let mut html = String::with_capacity(4096);
    html.push_str("<h1>Invoice #4217</h1>");
    html.push_str("<p>Billed to: Acme Corporation, 500 Market St, Suite 200</p>");
    html.push_str("<table><tr><th>Item</th><th>Qty</th><th>Amount</th></tr>");
    let mut total_cents: u64 = 0;
    for i in 0..rows {
        let name = ITEM_NAMES[i % ITEM_NAMES.len()];
        let qty = 1 + (i % 5);
        let unit_price_cents = 1299 + (i as u64 % 7) * 450;
        let amount_cents = qty as u64 * unit_price_cents;
        total_cents += amount_cents;
        let _ = write!(
            html,
            "<tr><td>{name}</td><td>{qty}</td><td>${}.{:02}</td></tr>",
            amount_cents / 100,
            amount_cents % 100
        );
    }
    html.push_str("</table>");
    let _ = write!(
        html,
        "<p><strong>Total: ${}.{:02}</strong></p>",
        total_cents / 100,
        total_cents % 100
    );
    html.push_str("<p>Status: Issued</p>");
    html.push_str("<p>Generated at 2026-09-18T00:00:00Z</p>");
    html
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    let html = invoice_html(60);

    for _ in 0..20 {
        black_box(Pdf::from_html(html.clone()).render());
    }

    for _ in 0..iterations {
        black_box(Pdf::from_html(html.clone()).render());
    }

    println!("completed {} PDF renders", iterations + 20);
}
