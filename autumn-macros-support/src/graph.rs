//! Shared tokens for the architecture-graph descriptors (issue #1747).

use proc_macro2::TokenStream;
use quote::quote;

/// The declaring crate's version, as a `&'static str` expression.
///
/// Expands at the macro call site, so Cargo gives the version of the crate
/// that holds the element. Outside Cargo it is `""`. The graph adds it to a
/// node id when two versions of one crate give the same id (#2520 item 14).
#[must_use]
pub fn crate_version() -> TokenStream {
    quote! {
        match ::core::option_env!("CARGO_PKG_VERSION") {
            ::core::option::Option::Some(version) => version,
            ::core::option::Option::None => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_comes_from_the_call_site_crate() {
        let tokens = crate_version().to_string();
        assert!(tokens.contains("option_env !"), "{tokens}");
        assert!(tokens.contains("\"CARGO_PKG_VERSION\""), "{tokens}");
    }
}
