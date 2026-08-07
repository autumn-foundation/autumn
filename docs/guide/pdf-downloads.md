# Generating PDFs

Autumn ships a typed [`Pdf`] response so a handler can turn an HTML string —
typically the same [`maud::Markup`] view you already render on-screen — into
a downloadable PDF, with the right `Content-Type: application/pdf` and
`Content-Disposition` headers handled for you by [`Download`], which `Pdf`
is built on.

`Pdf` is a plain `IntoResponse`, so returning it from a `#[get]` handler is a
single expression.

## Why it matters

The idiomatic answer in most mature web frameworks (Rails' `wicked_pdf`,
Django + WeasyPrint, Phoenix's `ChromicPDF`) is: reuse the HTML/template
layer you already have to produce a PDF, rather than hand-rolling one with a
bespoke drawing API. Autumn does the same — the natural source for an
invoice, receipt, or report PDF is the Maud view you already render for the
on-screen detail page.

## Quick start

```rust
use autumn_web::pdf::Pdf;
use autumn_web::prelude::*;

#[get("/invoices/{id}/pdf")]
async fn invoice_pdf(id: Path<i64>) -> Pdf {
    let markup = html! {
        h1 { "Invoice #" (*id) }
        p { "Total: $42.00" }
    };
    Pdf::from_markup(markup).filename("invoice.pdf")
}
```

That is the whole handler: construct a [`Pdf`] from a Maud view, name the
file, return it.

## Constructing a PDF

- [`Pdf::from_markup`] — renders a `maud::Markup` view (requires the `maud`
  feature; enabled together with `pdf` in the quick start above).
- [`Pdf::from_html`] — renders any HTML string, for callers that don't use
  Maud or already have a string in hand.

## Builder options

```rust
use autumn_web::pdf::Pdf;

# fn demo() -> Pdf {
Pdf::from_html("<h1>Receipt</h1>")
    .filename("receipt.pdf")   // Content-Disposition filename (default: document.pdf)
    .inline()                  // render in-browser instead of forcing a save dialog
# }
```

- [`filename`] — sets the `Content-Disposition` filename. RFC 6266-safe for
  non-ASCII/spaces and sanitized against header injection, same as
  [`Download::filename`].
- [`inline`] — serves the PDF inline (`Content-Disposition: inline`) instead
  of forcing a save dialog.
- [`render`] — renders to raw PDF bytes without building an HTTP response, for
  emailing an invoice as an attachment, writing it to a stored
  [`Blob`](https://docs.rs/autumn-web/latest/autumn_web/storage/struct.Blob.html),
  or use in a test.

## What HTML is supported

`Pdf` is **not** a CSS layout engine — pixel-perfect multi-page layout is
explicitly out of scope (see issue #1317). It renders a deliberately small
HTML subset, flowed top-to-bottom in a single column with the built-in PDF
base-14 fonts:

| Supported | Behavior |
|---|---|
| `h1`–`h6`, `p` | Block text at a size scaled to the heading level |
| `table` / `tr` / `th` / `td` | Naive equal-width columns; `th` renders bold |
| `ul` / `ol` / `li` | Bulleted / numbered list items |
| `strong` / `b`, `em` / `i` | Bold / italic (Helvetica's built-in bold and oblique faces) |
| `br`, `hr` | Line break / horizontal rule |
| anything else (`div`, `span`, `a`, widget-generated wrapper markup, ...) | Transparent — its text content still renders, without special styling |

Because unrecognized tags pass their text through transparently instead of
being dropped, a typical scaffold view (`property_list`, `data_table`, ...)
degrades gracefully rather than erroring.

Text outside the base-14 fonts' WinAnsi encoding (CJK, emoji, ...) renders as
`?` rather than corrupting the output — a known limitation of not embedding
extra font files (see "Runtime dependencies" below).

## Determinism and testing

Rendering the same HTML always produces the same visible text — nothing in
`Pdf` reads the wall clock or other hidden state. If your document needs a
timestamp (an invoice's "Generated at" line), render it into the HTML
yourself using the injected [`Clock`] extractor, and it becomes part of the
deterministic input like any other model field:

```rust
use autumn_web::pdf::Pdf;
use autumn_web::prelude::*;
use autumn_web::time::Clock;

#[get("/invoices/{id}/pdf")]
async fn invoice_pdf(id: Path<i64>, clock: Clock) -> Pdf {
    let markup = html! {
        h1 { "Invoice #" (*id) }
        p { "Generated at " (clock.now().to_rfc3339()) }
    };
    Pdf::from_markup(markup).filename("invoice.pdf")
}
```

Assert on the rendered content through the in-process test client — no
headless browser required:

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

# async fn demo() {
let client = TestApp::new().routes(routes![]).build();
client
    .get("/invoices/42/pdf")
    .send()
    .await
    .assert_header("content-type", "application/pdf")
    .assert_pdf_contains("Total: $42.00");
# }
```

[`TestResponse::assert_pdf_contains`] extracts text via [`pdf::extract_text`],
which reads back exactly what `Pdf` wrote. Note the guarantee is
**text-stable**, not byte-stable: the underlying PDF writer assigns each
document a random trailer `/ID` per the PDF spec's file-identification
convention, so raw bytes vary between renders even though the extracted text
never does.

## Runtime dependencies

Rendering uses [`printpdf`]'s core PDF writer with no system-installed
browser or renderer, and no embedded font files — the base-14 fonts
(Helvetica, Times, Courier, ...) are guaranteed present in every
PDF-compliant viewer, so nothing needs to ship inside (or be downloaded by)
your binary. This keeps PDF generation compatible with Autumn's single-binary
deployment story ([issue #1004]).

## Working example

The [`invoice`](../../examples/invoice) example exposes
`GET /invoices/{id}/pdf`, rendering the same `invoice_view` Maud function
used by the on-screen `GET /invoices/{id}` page:

```bash
# HTML detail page
curl http://localhost:3000/invoices/42

# Downloadable PDF (200, Content-Disposition: attachment; filename="invoice-42.pdf")
curl -OJ http://localhost:3000/invoices/42/pdf
```

Its [test suite](../../examples/invoice/tests/invoice.rs) asserts the header
contract, that the PDF's extracted text matches the on-screen model, and that
rendering is deterministic given a fixed `Clock`.

[`Pdf`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/struct.Pdf.html
[`Pdf::from_markup`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/struct.Pdf.html#method.from_markup
[`Pdf::from_html`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/struct.Pdf.html#method.from_html
[`filename`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/struct.Pdf.html#method.filename
[`inline`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/struct.Pdf.html#method.inline
[`render`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/struct.Pdf.html#method.render
[`pdf::extract_text`]: https://docs.rs/autumn-web/latest/autumn_web/pdf/fn.extract_text.html
[`TestResponse::assert_pdf_contains`]: https://docs.rs/autumn-web/latest/autumn_web/test/struct.TestResponse.html#method.assert_pdf_contains
[`Download`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html
[`Download::filename`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.filename
[`Clock`]: https://docs.rs/autumn-web/latest/autumn_web/time/struct.Clock.html
[`printpdf`]: https://docs.rs/printpdf/latest/printpdf/
[issue #1004]: https://github.com/autumn-foundation/autumn/issues/1004
