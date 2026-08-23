//! Benchmark: rendering a realistic scaffolded form with `autumn::form`
//! helpers.
//!
//! Drives the same `text_input`/`password_input`/`textarea_input`/
//! `number_input`/`checkbox_input`/`date_input` helpers a generated
//! create/edit view calls, over a 12-field changeset shaped like a typical
//! scaffolded model (title, slug, description, price, quantity, ... with a
//! couple of fields carrying validation errors, as on a re-rendered failed
//! submission).
//!
//! Profiling this surfaced a bigger cost than the id-string `format!` calls
//! each helper does: every helper that renders a value
//! (`text_input`/`textarea_input`/`number_input`/`date_input`) calls
//! `Changeset::field_value`, which used to run `serde_json::to_value(&self.data)`
//! — serializing the *entire* record — on every call, just to read one field
//! out of it. A 12-field form re-serialized the whole record up to 9 times
//! per render. See `Changeset::field_value` in `autumn/src/form.rs` for the
//! fix (caches the serialization on the changeset).
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing: it is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench form_render
//! BIN=$(find target/release/deps -maxdepth 1 -name "form_render-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 2000
//! callgrind_annotate --threshold=90 callgrind.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 2000
//! ```

use std::collections::HashMap;
use std::hint::black_box;

use autumn_web::form::{
    Changeset, checkbox_input, date_input, number_input, password_input, text_input, textarea_input,
};
use serde::Serialize;

/// Shape of a typical scaffolded model: a mix of text, numeric, boolean, and
/// date fields, the same variety `autumn generate scaffold` produces.
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
/// realistic re-render of a failed create/update submission (the branch that
/// also emits the `<div id="{field}-error">` block).
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

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);

    let changeset = sample_changeset();

    for _ in 0..50 {
        black_box(render_form(&changeset).into_string());
    }

    for _ in 0..iterations {
        black_box(render_form(&changeset).into_string());
    }

    println!("completed {} form renders", iterations + 50);
}
