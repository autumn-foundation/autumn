# Autumn Invoice Example

A minimal example showing `autumn_web::pdf::Pdf` (issue #1317): render a
downloadable PDF from the same Maud view used for the on-screen page. No
database — invoices are synthesized in memory, so this stays as
dependency-free as `hello`.

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|---------------|
| `autumn_web::pdf::Pdf` | `src/lib.rs` | Renders a `maud::Markup` view to a downloadable PDF |
| One view, two responses | `src/lib.rs` | `invoice_view` backs both the HTML detail page and the PDF export |
| `Clock` extractor | `src/lib.rs` | The "Generated at" timestamp comes from the injected clock, not `Utc::now()`, so tests can pin it |
| `TestResponse::assert_pdf_contains` | `tests/invoice.rs` | Asserts on rendered PDF text via the in-process test client, no headless browser |

## Prerequisites

- Rust 1.88.0+

No database or external services required.

## Quick start

From the **workspace root** (`autumn/`):

```bash
cargo run -p invoice
```

The server starts on `http://localhost:3000`.

### Prove it works

```bash
curl http://localhost:3000/invoices/42
# => HTML detail page

curl -OJ http://localhost:3000/invoices/42/pdf
# => downloads invoice-42.pdf
```

## Available routes

| Method | Path | Response |
|--------|------|----------|
| GET | `/invoices/{id}` | HTML detail page |
| GET | `/invoices/{id}/pdf` | Downloadable `application/pdf` |

## Tests

```bash
cargo test -p invoice
```

Covers the HTML/PDF header contract, that the PDF's extracted text matches
the model, and that rendering is deterministic given a fixed `Clock`.
