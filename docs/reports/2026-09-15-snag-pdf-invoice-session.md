# 🪝 Snag: exploratory QA session report — `autumn_web::pdf` via examples/invoice

**Charter:** a developer rendering a PDF from a Maud/HTML view via
`autumn_web::pdf::Pdf` (issue #1317) — boundary and data tour on the HTML
subset the renderer accepts, entered through `examples/invoice` (the only
shipped consumer of the `pdf` feature) and, once the example's own routes
turned out to have no free-text input surface, driven directly against the
framework's `Pdf::from_html`/`render`/`extract_text` API.

**Time spent:** ~1 hour (build + live HTTP boundary tour + in-process
probes), well under the 30-CI-minute generative-sweep budget.

**Environment:** commit `b46b083d5aa5815452431f7c556486985fbf1fbd` (trunk),
workspace version 0.7.0, Linux container, rustc/cargo `1.94.1`. No Docker
available in this sandbox (consistent with prior sessions' notes); irrelevant
here since `examples/invoice` and the `pdf` feature need no database.
`examples/invoice` built and run as the real compiled binary
(`cargo build -p invoice`, `127.0.0.1:3105`) for the HTTP half; the
in-process half added a temporary `[[test]]` target
(`autumn/tests/snag_pdf_probe.rs`, `required-features = ["pdf"]`) to call
`autumn_web::pdf::{Pdf, extract_text}` directly — removed before this report
was committed, since it was a scratch probe, not a permanent regression test
(see "Filed" below for why no code change accompanies this report).

## Method

1. **HTTP boundary tour on `examples/invoice`'s only input, `Path<i64>` id**:
   `0`, `-1`, `i64::MAX`, `i64::MAX+1` (overflow), `i64::MIN`, non-numeric,
   `1.5`, leading zero, leading `+`, trailing slash — against both
   `/invoices/{id}` (HTML) and `/invoices/{id}/pdf`. All handled sanely (200
   for any in-range `i64`, 400 for anything that doesn't parse as one, 404
   for the trailing-slash non-route) with no crash and no HTML/PDF content
   divergence for the same id. This app's only input is an id substituted
   into a fixed template with two static line items — no unicode/injection
   surface to drive a data tour through, and the existing test suite
   (`examples/invoice/tests/invoice.rs`) already covers the HTML/PDF
   content-parity and fixed-clock-determinism claims tightly. **Solid area,
   nothing further to add.**
2. Read `autumn/src/pdf.rs`'s module docs for the renderer's own claims —
   the highest-value oracle source — and noted three specifically testable
   ones: (a) CJK/emoji render as `?` rather than corrupting output, (b) a
   single table cell that overflows a page is clipped rather than spilling
   to a second page, (c) any tag the renderer doesn't specifically
   recognize degrades to its text content rather than dropping content or
   erroring. Also noted the parser's own documented stack-safety guarantee
   ("adversarially deep nesting can't blow the stack") and its existing
   `deeply_nested_input_does_not_overflow_the_stack` /
   `deeply_nested_wrapper_tags_do_not_overflow_the_stack` tests.
3. Since `examples/invoice` has no surface to carry adversarial HTML through
   HTTP, moved the boundary/data tour in-process against `Pdf::from_html` +
   `render()` + `extract_text()` directly — legitimate framework-level QA
   surface (both are public, documented API any Autumn app can call with
   real user content, not a private implementation detail), run with a
   per-case timeout guard (`catch_unwind` + `mpsc::recv_timeout`) to catch a
   hang as well as a panic:
   - empty string, a 200,000-byte unbroken "word" (no spaces — a classic
     line-wrap infinite-loop shape), a 50,000-row table, CJK/emoji/Arabic
     text, a single 20,000-line table cell, 5,000 stray unmatched closing
     tags, an unterminated comment, edge-case numeric HTML entities
     (`&#0;`, `&#x110000;`), nested tables, 5,000 attributes on one tag, and
     — the one that found something — deeply nested wrapper tags
     (`"<b>".repeat(n) + "MARKER" + "</b>".repeat(n)`) at `n` from 1 up to
     200,000.
   - Every case rendered without panicking or hanging (confirming the
     parser's own stack-safety claim holds, including well past the
     `n=50,000` its own test already covers) **except** the deep-nesting
     one, where the *rendered content itself* silently changed shape well
     before any performance or stability limit was reached.

## Findings

**Filed: [issue #2801](https://github.com/autumn-foundation/autumn/issues/2801)**
— `🪝 Snag: HTML deeper than 512 tags silently vanishes from a rendered PDF,
with no error or warning (medium, repro 10/10)`. Binary search pinned the
cutover exactly at 512/513 nested tags — `<b>`, `<i>`, `<span>`, and `<div>`
all reproduce identically; `<p>` doesn't, only because this renderer
auto-closes `<p>` (so 513 of them never build a 513-deep tree in the first
place). Root cause: `autumn/src/pdf/layout.rs`'s `MAX_DEPTH: u32 = 512`, a
recursion-depth cap on the tree-walking layout stage (the HTML *parser*
itself is iterative and genuinely stack-safe, exactly as documented — the
cap lives one layer up), hit in six separate functions
(`inline_spans`, `inline_list_items`, `extract_list_items`,
`extract_table_rows`, `flatten_blocks`, `flatten_into_pending`), every one
of which just `return`s with no log line, no error, and no visible marker
in the output once `depth > MAX_DEPTH`. The maintainers already know this
specific cap exists and drops content — there's a doc comment calling it
"defense in depth" and a test
(`deeply_nested_wrapper_tags_do_not_overflow_the_stack`) that explicitly
only asserts "does not panic, content beyond MAX_DEPTH is allowed to be
dropped." What that internal comment doesn't address, and what the *public*
module docs promise the opposite of, is the manner of the drop: silent,
with a well-formed, complete-looking PDF as the result — exactly the "wrong
answer delivered confidently, recipient cannot detect it" shape Snag's own
impact floor calls out as worst-in-class. Not filed as a fix PR: the right
correction (a visible truncation marker, a `tracing::warn!`, a
`Result`-returning render path, or just documenting the ceiling publicly)
is a design decision, not a ≤10-line unambiguous one.

No other findings. **Solid areas** (toured, held up): the parser's
stack-safety guarantee under genuinely adversarial nesting (200,000 levels,
well past its own existing test's 50,000), the CJK/emoji-to-`?` fallback,
oversized single-cell handling, malformed/unterminated markup (stray
closes, unterminated comments, edge-case entities), and
`examples/invoice`'s own id-boundary handling and HTML/PDF content parity.

## Proposed next charters

1. **The MAX_DEPTH fix itself**, once a maintainer picks a correction
   strategy for issue #2801 — a natural quarantined-regression-test
   candidate once that decision is made.
2. **A real consumer with recursive/user-generated content piped through
   `Pdf`** — no shipped example currently renders threaded/nested content
   (comments, replies, blockquote chains) to PDF, so issue #2801's realistic
   trigger path (a comment-tree "export thread" feature, say) is inferred
   rather than observed live. If any future example adds such a feature,
   this charter is "drive it end-to-end and confirm/rule out the same
   silent-drop shape at whatever depth that feature's data can reach."
3. **`Pdf::from_html` fed a full server-rendered page** (not a
   purpose-built fragment) — the module docs specifically call out
   `<script>`/`<style>`/`<head>`/`<title>` as excluded from visible text
   "so passing a full server-rendered page... doesn't leak inline CSS/JS
   source into the PDF." Untested this session; worth a direct check that a
   real page (e.g. one of `examples/cms`'s rendered templates) piped
   through `Pdf::from_html` actually holds that claim rather than leaking
   `<style>`/`<script>` bodies as visible text.

## Reproduce

```bash
cd autumn  # crate is autumn-web, package dir is autumn/
cat > /tmp/snag_pdf_depth.rs <<'RUST'
use autumn_web::pdf::{Pdf, extract_text};

#[test]
fn content_past_512_nesting_levels_vanishes_silently() {
    for n in [512usize, 513] {
        let html = format!("{}MARKER{}", "<b>".repeat(n), "</b>".repeat(n));
        let bytes = Pdf::from_html(html).render();
        let text = extract_text(&bytes).unwrap();
        println!("n={n} contains_marker={}", text.contains("MARKER"));
    }
}
RUST
cp /tmp/snag_pdf_depth.rs tests/snag_pdf_depth_probe.rs
# add to Cargo.toml:
#   [[test]]
#   name = "snag_pdf_depth_probe"
#   path = "tests/snag_pdf_depth_probe.rs"
#   required-features = ["pdf"]
cd ..
cargo test -p autumn-web --test snag_pdf_depth_probe --features pdf -- --nocapture
# expect: n=512 contains_marker=true / n=513 contains_marker=false
```
