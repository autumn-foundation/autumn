//! Render server-side templates to downloadable PDF documents.
//!
//! [`Pdf`](crate::pdf::Pdf) turns an HTML string — typically a [`maud::Markup`] view you
//! already render on-screen — into a PDF document served with the right
//! `Content-Type: application/pdf` and `Content-Disposition` headers (via
//! [`Download`](crate::download::Download), which already handles RFC
//! 6266-safe filenames and the `inline`/`attachment` switch — see its module
//! docs for that half of the contract).
//!
//! # Quick example
//!
//! ```rust,no_run
//! use autumn_web::pdf::Pdf;
//! use autumn_web::prelude::*;
//!
//! #[get("/invoices/{id}/pdf")]
//! async fn invoice_pdf(id: Path<i64>) -> Pdf {
//!     let markup = html! {
//!         h1 { "Invoice #" (*id) }
//!         p { "Total: $42.00" }
//!     };
//!     Pdf::from_markup(markup).filename("invoice.pdf")
//! }
//! ```
//!
//! # Not a CSS layout engine
//!
//! This renders a **deliberately small HTML subset** — headings, paragraphs,
//! tables, lists, `<strong>`/`<em>` emphasis, `<br>`/`<hr>` — flowed
//! top-to-bottom in a single column with the built-in PDF base-14 fonts
//! (Helvetica). It does **not** parse CSS, does not lay out `<div>`s as boxes
//! with padding/borders/floats, and does not do pixel-perfect text metrics —
//! that is out of scope by design (see issue #1317's "Out of Scope" section).
//! Any tag this renderer doesn't specifically recognize (`<div>`, `<span>`,
//! `<a>`, widget-generated wrapper markup, ...) is treated as a transparent
//! container: its text content still renders, just without special styling —
//! so a typical scaffold view degrades gracefully instead of dropping
//! content or erroring. `<script>`, `<style>`, `<noscript>`, `<template>`,
//! `<head>`, and `<title>` are the exception — their content is never
//! rendered as visible text, matching how a browser treats them, so passing
//! a full server-rendered page (not just a purpose-built fragment) to
//! [`Pdf::from_html`](crate::pdf::Pdf::from_html) doesn't leak inline CSS/JS source into the PDF.
//!
//! Text outside the base-14 fonts' WinAnsi encoding (CJK, emoji, ...) is
//! rendered as `?` by the underlying PDF writer rather than corrupting the
//! output — a known limitation of avoiding embedded font files (see the
//! "Runtime dependencies" section below). A single table cell whose content
//! wraps to more than a full page of lines is clipped at the bottom margin
//! rather than spilling onto a second page — realistic scaffold tables
//! (invoice line items, a handful of columns) never approach this.
//!
//! # Determinism
//!
//! Rendering the same HTML input always produces the same visible content:
//! [`extract_text`](crate::pdf::extract_text) (and the [`assert_pdf_contains`](crate::test::TestResponse::assert_pdf_contains)
//! test helper built on it) returns identical text for identical input, with
//! no wall-clock or other hidden state read during rendering — any
//! timestamp that should appear in the document (e.g. an invoice's "Generated
//! at" line) is the caller's responsibility to render into the HTML using
//! the injected [`Clock`](crate::time::Clock), not something this module
//! reads on its own.
//!
//! The **raw bytes** are not guaranteed to be identical between renders: the
//! underlying PDF writer ([`printpdf`]) assigns each document a random
//! trailer `/ID`, per the PDF spec's file-identification convention, and
//! that generator isn't exposed as configurable. Nothing else in the file
//! varies with identical input, but this rules out a byte-for-byte equality
//! assertion — text-content equality is the supported determinism contract.
//!
//! # Runtime dependencies
//!
//! Rendering uses [`printpdf`]'s core PDF writer with `default-features =
//! false` — no system-installed browser or renderer, and no embedded font
//! files: the base-14 fonts (Helvetica, Times, Courier, ...) are guaranteed
//! present in every PDF-compliant viewer, so nothing needs to ship inside
//! (or be downloaded by) your binary. This keeps PDF generation compatible
//! with the single-binary deployment story (issue #1004).

use axum::response::{IntoResponse, Response};

use crate::download::Download;

mod html;
mod layout;
mod metrics;

/// A PDF document rendered from an HTML string, ready to return from a
/// handler.
///
/// Construct with [`from_html`](Pdf::from_html) or (with the `maud` feature)
/// [`from_markup`](Pdf::from_markup), then chain [`filename`](Pdf::filename)
/// / [`inline`](Pdf::inline) and return it — it implements [`IntoResponse`].
///
/// See the [module docs](crate::pdf) for what HTML is supported.
#[derive(Debug, Clone)]
#[must_use = "a Pdf does nothing unless returned from a handler or converted with `into_response`"]
pub struct Pdf {
    html: String,
    filename: Option<String>,
    inline: bool,
}

impl Pdf {
    /// Render `html` (an HTML-subset string — see the [module docs](crate::pdf))
    /// into a PDF document.
    pub fn from_html(html: impl Into<String>) -> Self {
        Self {
            html: html.into(),
            filename: None,
            inline: false,
        }
    }

    /// Render a [`maud::Markup`] view into a PDF document — the natural
    /// source for a PDF is the same Maud view you already render on-screen.
    #[cfg(feature = "maud")]
    pub fn from_markup(markup: maud::Markup) -> Self {
        Self::from_html(markup.into_string())
    }

    /// Set the download filename (`Content-Disposition`'s `filename`
    /// parameter). Defaults to `document.pdf`.
    ///
    /// RFC 6266-safe for non-ASCII/spaces and sanitized against header
    /// injection — see [`Download::filename`](crate::download::Download::filename),
    /// which this delegates to.
    #[must_use = "builder setters return a new Pdf; use the returned value"]
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// Render inline (`Content-Disposition: inline`) so a browser displays
    /// the PDF instead of forcing a save dialog.
    #[must_use = "builder setters return a new Pdf; use the returned value"]
    pub const fn inline(mut self) -> Self {
        self.inline = true;
        self
    }

    /// Render to PDF bytes without building an HTTP response — useful for
    /// emailing an invoice as an attachment, writing it to a
    /// [`Blob`](crate::storage::Blob) store, or testing.
    #[must_use]
    pub fn render(&self) -> Vec<u8> {
        render_bytes(&self.html)
    }
}

fn render_bytes(html: &str) -> Vec<u8> {
    let pages = layout::render_pages(html);
    let mut doc = printpdf::PdfDocument::new("PDF Document");
    let mut warnings = Vec::new();
    doc.pages = pages;
    doc.save(&printpdf::PdfSaveOptions::default(), &mut warnings)
}

impl IntoResponse for Pdf {
    fn into_response(self) -> Response {
        let bytes = self.render();
        let filename = self.filename.unwrap_or_else(|| "document.pdf".to_owned());
        let download = Download::from_bytes(bytes)
            .content_type("application/pdf")
            .filename(filename);
        let download = if self.inline {
            download.inline()
        } else {
            download
        };
        download.into_response()
    }
}

/// [`extract_text`] could not read `bytes` back as a PDF.
#[derive(Debug, thiserror::Error)]
#[error("not a parseable PDF: {0}")]
pub struct PdfParseError(String);

/// Extract the visible text of a rendered PDF as one space-joined string —
/// enough to assert on with a plain substring check.
///
/// Backed by `printpdf`'s own PDF parser (via
/// [`PdfDocument::extract_text`](printpdf::PdfDocument::extract_text)), so it
/// reads back exactly what [`Pdf`] (or any other well-formed PDF) wrote,
/// rather than re-implementing PDF text extraction. `printpdf` returns one
/// chunk per text-showing operator rather than one per visual line — [`Pdf`]
/// draws each word with its own operator (to position bold/italic runs
/// independently), so this joins every chunk, across every page, with a
/// single space rather than trying to reconstruct line/page boundaries.
/// That means a phrase that happens to wrap across two lines is still found
/// as one contiguous substring, at the cost of not distinguishing "same
/// line" from "next line" in the returned string. It also means that two
/// *differently styled* words with no whitespace between them in the source
/// HTML (e.g. `$<strong>42.00</strong>`) render visually adjacent, with no
/// gap, but still come back from this function as two separate
/// space-joined chunks (`"$ 42.00"`) — match on the two pieces separately
/// (or drop styling at the boundary) rather than the single glued string.
///
/// # Errors
///
/// Returns [`PdfParseError`] if `bytes` is not a parseable PDF.
pub fn extract_text(bytes: &[u8]) -> Result<String, PdfParseError> {
    let mut warnings = Vec::new();
    let doc =
        printpdf::PdfDocument::parse(bytes, &printpdf::PdfParseOptions::default(), &mut warnings)
            .map_err(PdfParseError)?;
    let mut out = String::new();
    for page in doc.extract_text() {
        for chunk in page {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&chunk);
        }
    }
    Ok(out)
}
