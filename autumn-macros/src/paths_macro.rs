//! `paths![]` collection macro implementation.
//!
//! Expands `paths![handler_a, handler_b]` into a `pub mod paths { … }` module
//! that re-exports each handler's `__autumn_path_*` companion function under
//! its short name, so callers can write `paths::handler_a(arg)` instead of
//! the internal `__autumn_path_handler_a(arg)`.
//!
//! Module-qualified paths are supported: `paths![posts::show, users::index]`
//! re-exports them as `paths::show` and `paths::index` respectively.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Path, Token, punctuated::Punctuated};

/// Expand `paths![handler_a, module::handler_b]` into a `pub mod paths { … }`.
pub fn paths_macro(input: TokenStream) -> TokenStream {
    if input.is_empty() {
        return quote! {
            pub mod paths {}
        };
    }

    let paths: Punctuated<Path, Token![,]> =
        match syn::parse::Parser::parse2(Punctuated::parse_terminated, input) {
            Ok(p) => p,
            Err(err) => return err.to_compile_error(),
        };

    let uses: Vec<_> = paths
        .iter()
        .map(|path| {
            // The short alias is the last segment's ident (e.g. `show` from `posts::show`).
            let alias = match path.segments.last() {
                Some(seg) => seg.ident.clone(),
                None => return quote! {},
            };

            // Build the companion path: prefix the last segment with `__autumn_path_`.
            let mut companion = path.clone();
            if let Some(last) = companion.segments.last_mut() {
                last.ident = format_ident!("__autumn_path_{}", last.ident);
            }

            // Emit a `pub use` that reaches the companion. Paths with a
            // leading `::` or starting with `crate` are truly absolute and
            // used verbatim. Simple / module-qualified relative paths (including
            // those starting with `super` or `self`) must be reached via an
            // additional `super::` from inside the generated `pub mod paths`.
            let is_absolute = companion.leading_colon.is_some()
                || companion
                    .segments
                    .first()
                    .is_some_and(|s| s.ident == "crate");

            if is_absolute {
                quote! { pub use #companion as #alias; }
            } else {
                quote! { pub use super::#companion as #alias; }
            }
        })
        .collect();

    quote! {
        pub mod paths {
            #(#uses)*
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    #[test]
    fn test_empty_input() {
        let input = quote! {};
        let output = paths_macro(input);
        let expected = quote! {
            pub mod paths {}
        };
        assert_eq!(output.to_string(), expected.to_string());
    }

    #[test]
    fn test_single_simple_path() {
        let input = quote! { handler_a };
        let output = paths_macro(input);
        let expected = quote! {
            pub mod paths {
                pub use super::__autumn_path_handler_a as handler_a;
            }
        };
        assert_eq!(output.to_string(), expected.to_string());
    }

    #[test]
    fn test_module_qualified_paths() {
        let input = quote! { posts::show, users::index };
        let output = paths_macro(input);
        let expected = quote! {
            pub mod paths {
                pub use super::posts::__autumn_path_show as show;
                pub use super::users::__autumn_path_index as index;
            }
        };
        assert_eq!(output.to_string(), expected.to_string());
    }

    #[test]
    fn test_absolute_paths() {
        let input = quote! { crate::posts::show, ::users::index };
        let output = paths_macro(input);
        let expected = quote! {
            pub mod paths {
                pub use crate::posts::__autumn_path_show as show;
                pub use ::users::__autumn_path_index as index;
            }
        };
        assert_eq!(output.to_string(), expected.to_string());
    }

    #[test]
    fn test_syntax_error() {
        let input = quote! { not a path };
        let output = paths_macro(input);
        assert!(output.to_string().contains("compile_error"));
    }
}
