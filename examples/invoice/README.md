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
| `#[lifecycle]` | `src/lib.rs` | Declares the invoice lifecycle once; the macro proves it sound at compile time and generates a typestate where an illegal transition does not exist |

## The invoice lifecycle

`InvoiceState` in `src/lib.rs` declares the states an invoice moves through:

```text
Draft ──> Issued ──> Paid      (terminal)
  │          │
  └──────────┴─────> Void      (terminal)
```

`#[lifecycle]` proves that graph sound while the crate compiles — every state
reachable from `Draft`, and every non-terminal state able to reach a terminal.
Adding a state no edge targets, or one with no way out, stops the build. It also
generates the `invoice_state` typestate module, so `settle()` chains
`start().to_issued().to_paid()` and an out-of-order call does not compile.

Render or re-check it from the workspace root:

```bash
autumn lifecycle check examples/invoice
autumn lifecycle diagram examples/invoice
```

See [Typed Lifecycles](../../docs/guide/lifecycle.md).

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
