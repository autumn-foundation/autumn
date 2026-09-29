//! Crates the `#[edge]` macro expansion and edge-safe handler modules name.
//!
//! An `#[edge]` handler module compiles for `wasm32-wasip1`, where
//! `autumn-web` is not in the dependency graph at all. Everything such a
//! module needs therefore has to be reachable through `autumn_edge::`, and
//! macro-emitted code has to be able to name `http` and `axum` by an absolute
//! path that is guaranteed to resolve without the author adding a dependency.
//!
//! Prefer [`crate::prelude`] in hand-written code; this module exists for
//! generated code and for the occasional escape hatch.

pub use axum;
pub use http;
