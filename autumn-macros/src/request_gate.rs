//! Shared shape of the pre-body `FromRequestParts` "gate" that
//! `#[secured]`, `#[step_up]`, and `#[throttle]` each install ahead of the
//! handler's own parameters (issue #1668), so a failing check rejects a
//! request with the guard's own status before its body is ever parsed —
//! rather than after, when a body extractor (`Json`/`Form`/`Multipart`)
//! would otherwise run first and mask the guard's outcome behind a
//! 400/422 body-parse error.
//!
//! Each macro still builds its own `check_body` — the ordered checks (and
//! any replay lookup it owns) that make sense for what it enforces; only
//! the surrounding gate type and its `FromRequestParts` impl are common.

use proc_macro2::TokenStream;
use quote::quote;

/// Wrap `check_body` inside a zero-sized gate type named `gate_ident` that
/// implements `FromRequestParts<AppState>`: run `check_body`, then resolve
/// to the gate on success. `check_body` runs inside the impl's `async move`
/// block, where `parts` and `state` are in scope exactly as they are for a
/// hand-written `FromRequestParts::from_request_parts`.
pub fn wrap_gate(gate_ident: &syn::Ident, check_body: &TokenStream) -> TokenStream {
    quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #gate_ident;

        #[doc(hidden)]
        impl ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState>
            for #gate_ident
        {
            type Rejection = ::autumn_web::reexports::axum::response::Response;

            fn from_request_parts(
                parts: &mut ::autumn_web::reexports::axum::http::request::Parts,
                state: &::autumn_web::AppState,
            ) -> impl ::core::future::Future<Output = ::core::result::Result<Self, Self::Rejection>>
                + Send {
                async move {
                    #check_body
                    ::core::result::Result::Ok(#gate_ident)
                }
            }
        }
    }
}

/// Insert `gate_ident` as `input_fn`'s first parameter and rewrite its
/// signature to return a plain `Response` — the mechanical edit every
/// pre-body guard macro applies to the handler once its gate item is built.
pub fn insert_gate_param(input_fn: &mut syn::ItemFn, gate_ident: &syn::Ident) {
    let gate_param: syn::FnArg = syn::parse_quote! { _: #gate_ident };
    input_fn.sig.inputs.insert(0, gate_param);

    input_fn
        .attrs
        .push(syn::parse_quote!(#[allow(clippy::too_many_arguments)]));
    input_fn.sig.output = syn::parse_quote! {
        -> ::autumn_web::reexports::axum::response::Response
    };
}
