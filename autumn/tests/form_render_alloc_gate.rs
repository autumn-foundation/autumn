//! Isolated integration test: allocation gate for the scaffolded form-render
//! helpers (`text_input`, `password_input`, `textarea_input`, `number_input`,
//! `checkbox_input`, `date_input`) on the same 12-field realistic workload as
//! the committed `autumn/benches/form_render.rs` profiling harness (title,
//! slug, password, summary, body, price, quantity, rating, published,
//! `published_at`, sku, notes — two fields, title and price, carrying
//! validation errors, matching a re-rendered failed submission).
//!
//! Its own binary for the same `allocation-counter` global-allocator reason
//! as `config_alloc_gate`/`password_policy_alloc_gate`: a process-wide
//! counting `#[global_allocator]`, not worth taxing onto the consolidated
//! suite to measure a handful of calls here.

use std::collections::HashMap;

use autumn_web::form::{
    Changeset, checkbox_input, date_input, number_input, password_input, text_input, textarea_input,
};
use serde::Serialize;

/// Shape of a typical scaffolded model — identical to `benches/form_render.rs`'s
/// `Article`, kept in sync deliberately so this gate measures the same
/// workload the profiler points at.
#[derive(Serialize)]
struct Article {
    title: String,
    slug: String,
    author_password: String,
    summary: String,
    body: String,
    price: String,
    quantity: String,
    rating: String,
    published: String,
    published_at: String,
    sku: String,
    notes: String,
}

fn sample_article() -> Article {
    Article {
        title: "Autumn Ships Named Futures".to_owned(),
        slug: "autumn-ships-named-futures".to_owned(),
        author_password: String::new(),
        summary: "A short summary of the release, long enough to be realistic.".to_owned(),
        body: "A much longer body field with several sentences of prose, the kind of \
               content a real article body would carry when re-rendered after a \
               failed validation submission."
            .to_owned(),
        price: "19.99".to_owned(),
        quantity: "3".to_owned(),
        rating: "4.5".to_owned(),
        published: "true".to_owned(),
        published_at: "2026-08-01".to_owned(),
        sku: "ART-1042".to_owned(),
        notes: "Internal editorial notes field.".to_owned(),
    }
}

/// A changeset with two fields carrying validation errors, matching a
/// realistic re-render of a failed create/update submission.
fn sample_changeset() -> Changeset<Article> {
    let mut errors = HashMap::new();
    errors.insert(
        "title".to_owned(),
        vec!["can't be blank".to_owned(), "is too short".to_owned()],
    );
    errors.insert("price".to_owned(), vec!["must be a number".to_owned()]);
    Changeset::from_errors(sample_article(), errors)
}

fn render_form(changeset: &Changeset<Article>) -> maud::Markup {
    maud::html! {
        form method="post" {
            (text_input(changeset, "title", "Title"))
            (text_input(changeset, "slug", "Slug"))
            (password_input(changeset, "author_password", "Author password"))
            (textarea_input(changeset, "summary", "Summary"))
            (textarea_input(changeset, "body", "Body"))
            (number_input(changeset, "price", "Price", Some("0.01")))
            (number_input(changeset, "quantity", "Quantity", Some("1")))
            (number_input(changeset, "rating", "Rating", Some("0.1")))
            (checkbox_input(changeset, "published", "Published"))
            (date_input(changeset, "published_at", "Published at"))
            (text_input(changeset, "sku", "SKU"))
            (textarea_input(changeset, "notes", "Notes"))
        }
    }
}

/// Enough repetitions that a per-render allocation cannot hide inside noise.
const RENDERS: u64 = 200;

#[test]
fn scaffolded_form_render_allocations_on_a_12_field_workload() {
    // Warm-up outside the measured window: a fresh Changeset per render (not
    // one reused across the whole run), same reasoning as the committed bench
    // — a real request builds a new Changeset and renders it once, so
    // `field_value`'s internal cache fills on the render's first field access
    // and is reused only for the rest of *that* render.
    for _ in 0..50 {
        let changeset = sample_changeset();
        let _ = std::hint::black_box(render_form(&changeset).into_string());
    }

    let info = allocation_counter::measure(|| {
        for _ in 0..RENDERS {
            let changeset = sample_changeset();
            let s = render_form(&changeset).into_string();
            std::hint::black_box(&s);
        }
    });

    let per_render_blocks = info.count_total / RENDERS;
    let per_render_bytes = info.bytes_total / RENDERS;
    println!(
        "scaffolded form render: {per_render_blocks} blocks / {per_render_bytes} bytes per \
         render ({RENDERS} renders, {} blocks / {} bytes total)",
        info.count_total, info.bytes_total
    );

    // Debug profile, default features, deterministic across runs. Before the
    // fix, every text/password/textarea/number/checkbox/date helper
    // unconditionally built `format!("{field}-error")` even on the 10 of 12
    // fields with no validation error, where the string is never read: 124
    // blocks / 22,491 bytes per render, 24,800 blocks / 4,498,200 bytes total
    // for 200 renders. See `autumn/src/form.rs`'s helpers for the fix (defers
    // the allocation until `has_errors` is confirmed true). After: **104
    // blocks** / 22,479 bytes per render, 20,800 / 4,495,800 total (-16.1%
    // blocks; bytes barely move — each wasted allocation is a handful of
    // bytes against a ~22KB/render budget dominated by larger buffers).
    //
    // Bolt follow-up (looking at `maud::escape::escape_to_string`, a naive
    // per-byte scan/match/push loop that was 26% of a release-profile
    // instruction count on this exact workload — see
    // `benches/form_render.rs`): the six helpers now pre-escape field
    // values/labels/errors themselves (`autumn_web::form::fast_escape`,
    // bulk `push_str` per clean run, zero-allocation `Cow::Borrowed` when
    // nothing needs escaping — every value in this fixture takes that path)
    // and hand the result to `maud::PreEscaped` instead of letting `html!`
    // re-scan it byte by byte. Block count is untouched — **104** per render,
    // 20,800 total, identical to the pre-Bolt-follow-up baseline above, since
    // escaping never allocates for this fixture's clean values. Bytes rose to
    // **23,023** per render / **4,604,600** total (+2.4%): `maud_macros`
    // sizes its output buffer from the *source token length* of the `html!`
    // block (`input.to_string().len()`), not from runtime content, and the
    // longer `PreEscaped`-wrapped interpolations read as "expect more
    // output" and over-reserve a bit of initial capacity that a shorter
    // `(field)` interpolation didn't. That reservation is never grown again
    // (block count proves it), so it's unused slack, not extra allocator
    // work. Ceiling sits at the current measurement plus a little headroom
    // for feature-set/toolchain variance, same convention as
    // `config_alloc_gate`/`password_policy_alloc_gate`; a failure a hair over
    // the line means re-measure and re-derive, not nudge upwards.
    assert!(
        info.count_total <= 21_500,
        "scaffolded form render allocated {} blocks over {RENDERS} renders, over the \
         21,500-block ceiling (20,800 measured; 24,800 was the pre-fix baseline)",
        info.count_total,
    );
    assert!(
        info.bytes_total <= 4_660_000,
        "scaffolded form render allocated {} bytes over {RENDERS} renders, over the \
         4,660,000-byte ceiling (4,604,600 measured; 4,495,800 was the pre-Bolt-follow-up \
         baseline; 4,498,200 was the original pre-fix baseline)",
        info.bytes_total,
    );
}
