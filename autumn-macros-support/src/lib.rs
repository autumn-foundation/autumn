//! Shared implementation helpers for Autumn's proc-macro crates.
//!
//! This is a plain library — **not** a proc-macro crate. It holds the
//! parsing, path-rewriting, schema, and naming helpers used by
//! `autumn-macros`, `autumn-macros-model`, and `autumn-macros-repository`.
//! Each proc-macro dylib links its own copy, so the logic is written once
//! but the per-dylib compile cost stays proportional to the macros that
//! dylib actually registers.
//!
//! Users should not depend on this crate directly — use `autumn-web`
//! instead, which re-exports everything.

pub mod crate_path;
pub mod naming;
pub mod schema;
