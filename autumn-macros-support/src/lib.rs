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

mod agent_authority;
pub mod cascade;
pub mod crate_path;
pub mod graph;
pub mod naming;
#[doc(hidden)]
pub mod parse;
mod query_budget;
pub mod schema;
pub mod wire;

/// The two largest macro analysers. `autumn-macros` registers them.
///
/// They live in this library, not in the proc-macro crate. Cargo builds a
/// library in parallel with other crates. It builds a proc-macro crate late,
/// and `autumn-web` waits for that crate (#2795).
#[doc(hidden)]
pub mod analysers {
    use proc_macro2::TokenStream;

    /// The body of `#[query_budget]`.
    #[must_use]
    pub fn query_budget(attr: TokenStream, item: TokenStream) -> TokenStream {
        crate::query_budget::query_budget_macro(attr, item)
    }

    /// The body of `#[agent_operable]`.
    #[must_use]
    pub fn agent_operable(attr: TokenStream, item: TokenStream) -> TokenStream {
        crate::agent_authority::agent_operable_macro(attr, item)
    }
}

#[cfg(test)]
mod tests {
    /// A large analyser in the proc-macro crate makes every cold build wait.
    #[test]
    fn the_large_analysers_are_not_in_the_proc_macro_crate() {
        let macros =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../autumn-macros/src"));
        for file in ["query_budget.rs", "agent_authority.rs"] {
            assert!(!macros.join(file).exists(), "{file} is in autumn-macros");
            let here = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).join(file);
            assert!(here.exists(), "{file} is not in autumn-macros-support");
        }
    }
}
