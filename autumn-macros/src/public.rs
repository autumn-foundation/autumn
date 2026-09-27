//! `#[public]` proc macro implementation.
//!
//! `#[public]` is a *marker* attribute: it declares that a handler is
//! deliberately unauthenticated. Unlike [`#[secured]`](crate::secured) it
//! injects no runtime guard and does not rewrite the handler signature — it
//! only emits a `__AUTUMN_PUBLIC` marker const that the route macros
//! (`#[get]`, `#[post]`, …) read back to populate `ApiDoc::public`.
//!
//! The marker mirrors `#[secured]`'s `__AUTUMN_SECURED_ROLES` mechanism so the
//! route macro can detect the declaration whether `#[public]` sits above or
//! below the route attribute in the stack.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{ItemFn, parse_quote};

pub fn public_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    if attr.into_iter().next().is_some() {
        return syn::Error::new(Span::call_site(), "#[public] does not take any arguments")
            .to_compile_error();
    }

    let mut input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    // Prepend the marker const so a route macro that expands *after* this one
    // can read the declaration back out of the handler body. When the route
    // macro is outermost instead, it detects the `#[public]` attribute directly
    // (see `api_doc::is_public`).
    let marker: syn::Stmt = parse_quote! {
        // Route macros read this marker when #[public] expands before #[get]/#[post]/etc.
        const __AUTUMN_PUBLIC: () = ();
    };
    input_fn.block.stmts.insert(0, marker);

    quote! { #input_fn }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::public_macro;

    #[test]
    fn injects_public_marker() {
        let generated =
            public_macro(quote! {}, quote! { async fn h() -> &'static str { "ok" } }).to_string();
        assert!(
            generated.contains("__AUTUMN_PUBLIC"),
            "public marker must be emitted: {generated}"
        );
    }

    #[test]
    fn preserves_handler_signature() {
        let generated =
            public_macro(quote! {}, quote! { async fn h() -> &'static str { "ok" } }).to_string();
        // No return-type rewrite and no injected extractors — the marker only.
        assert!(generated.contains("async fn h"));
        assert!(!generated.contains("Response"));
        assert!(!generated.contains("__autumn_session"));
    }

    #[test]
    fn rejects_arguments() {
        let generated = public_macro(quote! { "oops" }, quote! { async fn h() {} }).to_string();
        assert!(generated.contains("compile_error"));
    }
}
