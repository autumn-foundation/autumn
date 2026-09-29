//! The "food" corpus fed to the flocking simulation.
//!
//! This is a **static snapshot** of real Autumn source code — representative
//! excerpts stitched together from `autumn/src/lib.rs`, `autumn/src/prelude.rs`,
//! and the macro-heavy `autumn-macros/src/route.rs` — embedded via
//! `include_str!` so the island crate is fully self-contained and does not
//! depend on the framework's on-disk file layout at compile time.
//!
//! Every non-whitespace character becomes a piece of food; a boid that eats it
//! adopts that character as its glyph. The punctuation-dense Rust source is what
//! gives the flock its "literary" texture: `#[get("/")]`, `html!`, `->`, `{}`.

/// Static snapshot of Autumn's own source, used as the flock's food supply.
pub const AUTUMN_SOURCE: &str = include_str!("corpus.txt");
