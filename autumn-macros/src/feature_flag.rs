//! `#[feature_flag("key")]` proc macro implementation.
//!
//! Gates the entire route on a feature flag. If the flag is disabled for the
//! current actor the handler responds with 404 Not Found (default) or
//! delegates to a custom fallback handler specified with
//! `#[feature_flag("key", fallback = my_fallback_handler)]`.
//!
//! The flag check runs inside a dedicated `FromRequestParts` extractor so
//! Axum can short-circuit **before** body extractors (`Json`, `Form`) are
//! consumed and before other resources (DB connections, etc.) are acquired.
//!
//! ## Usage
//!
//! ```ignore
//! use autumn_web::prelude::*;
//!
//! #[get("/beta")]
//! #[feature_flag("beta_dashboard")]
//! async fn beta_dashboard() -> Markup {
//!     html! { h1 { "Beta!" } }
//! }
//! ```
//!
//! With a custom fallback:
//!
//! ```ignore
//! #[get("/experimental")]
//! #[feature_flag("experimental_feature", fallback = feature_disabled)]
//! async fn experimental() -> Markup { html! { "Experimental" } }
//!
//! async fn feature_disabled() -> impl IntoResponse {
//!     (StatusCode::NOT_FOUND, "Feature not available yet")
//! }
//! ```

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, ItemFn, LitStr, Token, parse::ParseStream, parse_quote};

struct FeatureFlagArgs {
    flag_key: String,
    fallback: Option<Expr>,
}

impl syn::parse::Parse for FeatureFlagArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let flag_key: LitStr = input.parse()?;
        let fallback = if input.peek(Token![,]) {
            let _: Token![,] = input.parse()?;
            let ident: syn::Ident = input.parse()?;
            if ident != "fallback" {
                return Err(syn::Error::new_spanned(
                    ident,
                    "expected `fallback = <handler_fn>`",
                ));
            }
            let _: Token![=] = input.parse()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Self {
            flag_key: flag_key.value(),
            fallback,
        })
    }
}

pub fn feature_flag_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args: FeatureFlagArgs = match syn::parse2(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let mut input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "#[feature_flag] can only be applied to async functions",
        )
        .to_compile_error();
    }

    // An attribute sharing #[authorize]'s argument grammar under a different
    // name is refused rather than guessed at — see
    // `authorize::reject_if_ambiguous_authorize_shape`'s doc comment.
    if let Some(err) = crate::authorize::reject_if_ambiguous_authorize_shape(&input_fn) {
        return err;
    }

    let flag_key = &args.flag_key;
    let fn_name = &input_fn.sig.ident;

    // One gate struct per handler keeps names unique within the module.
    let gate_ident = format_ident!("__AutumnFlagGate_{}", fn_name);

    // The gate's rejection: either the default 404 or a custom fallback.
    let disabled_rejection = args.fallback.as_ref().map_or_else(
        || {
            quote! {
                ::std::result::Result::Err(
                    ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                        ::autumn_web::reexports::http::StatusCode::NOT_FOUND,
                    )
                )
            }
        },
        |fallback_fn| {
            quote! {
                ::std::result::Result::Err(
                    ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                        #fallback_fn().await,
                    )
                )
            }
        },
    );

    // Whether THIS gate should also serve a cached idempotency replay: see
    // `idempotency_guard::should_own_replay` for the full ordering rationale.
    // Unconditionally serving one here — as this gate did before Codex review
    // on #2628 — would let it return a cached response before a still-pending
    // `#[secured]`/`#[authorize]` check (stacked on the same handler, expanding
    // after this one) ever ran.
    let owns_replay = crate::idempotency_guard::should_own_replay(&input_fn);
    let replay_check = if owns_replay {
        quote! {
            if let ::core::option::Option::Some(replay) = parts.extensions.get::<::autumn_web::idempotency::IdempotencyReplayResponse>() {
                let opt_ext = ::core::option::Option::Some(::autumn_web::reexports::axum::extract::Extension(replay.clone()));
                if let ::core::option::Option::Some(resp) = ::autumn_web::idempotency::__replay_response(&opt_ext) {
                    return ::std::result::Result::Err(resp);
                }
            }
        }
    } else {
        quote! {}
    };

    // Generate the gate struct and its FromRequestParts impl.
    //
    // Axum extracts all FromRequestParts items (including this gate) before
    // it extracts the body extractor (Json, Form, etc.).  If the flag is
    // disabled, from_request_parts returns Err(Response) and Axum returns that
    // response directly — the handler body and body extractors never run.
    let gate_impl = quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct #gate_ident;

        #[automatically_derived]
        impl ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState>
            for #gate_ident
        {
            type Rejection = ::autumn_web::reexports::axum::response::Response;

            async fn from_request_parts(
                parts: &mut ::autumn_web::reexports::http::request::Parts,
                state: &::autumn_web::AppState,
            ) -> ::std::result::Result<Self, Self::Rejection> {
                let flags = <::autumn_web::feature_flags::Flags
                    as ::autumn_web::reexports::axum::extract::FromRequestParts<
                        ::autumn_web::AppState,
                    >>::from_request_parts(parts, state)
                    .await
                    .map_err(|e| {
                        ::autumn_web::reexports::axum::response::IntoResponse::into_response(e)
                    })?;
                if flags.enabled(#flag_key) {
                    #replay_check
                    ::std::result::Result::Ok(#gate_ident)
                } else {
                    #disabled_rejection
                }
            }
        }
    };

    // Inject the gate as the first handler parameter.  The value is unused
    // in the handler body — its sole purpose is to trigger the extraction.
    let gate_param: syn::FnArg = parse_quote! { _: #gate_ident };
    input_fn.sig.inputs.insert(0, gate_param);

    input_fn
        .attrs
        .push(parse_quote!(#[allow(clippy::too_many_arguments)]));

    // The return type and body are left unchanged — the gate extractor handles
    // the disabled case before the handler is called, so no body injection or
    // return-type change is needed.
    quote! {
        #gate_impl
        #input_fn
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::feature_flag_macro;

    #[test]
    fn feature_flag_generates_valid_code_for_simple_flag() {
        let result = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                async fn my_handler() -> &'static str {
                    "hello"
                }
            },
        );
        let code = result.to_string();
        // Must contain the flag key lookup
        assert!(
            code.contains("my_flag"),
            "flag key must appear in generated code: {code}"
        );
        // Must emit a gate type
        assert!(
            code.contains("__AutumnFlagGate_my_handler"),
            "must emit a gate struct: {code}"
        );
        // Must handle the disabled case
        assert!(
            code.contains("NOT_FOUND") || code.contains("404"),
            "must have 404 fallback: {code}"
        );
        // Must NOT emit typed local binding
        assert!(
            !code.contains("let __autumn_inner"),
            "must not emit typed local binding: {code}"
        );
    }

    #[test]
    fn feature_flag_with_custom_fallback_uses_fallback_fn() {
        let result = feature_flag_macro(
            quote! { "my_flag", fallback = custom_fallback },
            quote! {
                async fn my_handler() -> &'static str {
                    "hello"
                }
            },
        );
        let code = result.to_string();
        assert!(
            code.contains("custom_fallback"),
            "custom fallback fn must appear in generated code: {code}"
        );
        assert!(
            !code.contains("NOT_FOUND"),
            "must NOT have default 404 when custom fallback provided: {code}"
        );
    }

    #[test]
    fn feature_flag_on_non_async_fn_is_compile_error() {
        let result = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                fn my_sync_handler() -> &'static str {
                    "hello"
                }
            },
        );
        let code = result.to_string();
        assert!(
            code.contains("compile_error"),
            "non-async fn must produce compile_error: {code}"
        );
    }

    #[test]
    fn feature_flag_impl_trait_return_does_not_emit_typed_binding() {
        // The gate approach never emits a typed local binding at all —
        // the handler body is unchanged and the return type is preserved.
        let result = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                async fn my_handler() -> Result<impl IntoResponse, String> {
                    Ok("hello")
                }
            },
        );
        let code = result.to_string();
        assert!(
            !code.contains("let __autumn_inner"),
            "must not emit typed local binding: {code}"
        );
    }

    #[test]
    fn feature_flag_with_invalid_arg_is_compile_error() {
        let result = feature_flag_macro(
            quote! { "my_flag", unknown_arg = something },
            quote! {
                async fn my_handler() -> &'static str {
                    "hello"
                }
            },
        );
        let code = result.to_string();
        assert!(
            code.contains("compile_error"),
            "unknown arg must produce compile_error: {code}"
        );
    }

    #[test]
    fn feature_flag_gate_runs_before_body_extractors() {
        // The gate extractor is injected as the first parameter (a
        // FromRequestParts impl), so Axum evaluates it before consuming the
        // request body (Json, Form, etc.).  Verify the generated code does NOT
        // contain the old body-wrapping pattern.
        let result = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                async fn my_handler(body: String) -> String {
                    body
                }
            },
        );
        let code = result.to_string();
        assert!(
            code.contains("FromRequestParts"),
            "must generate a FromRequestParts impl: {code}"
        );
        assert!(
            !code.contains("async move"),
            "must not wrap body in async move block: {code}"
        );
    }

    // ── Idempotency-replay ownership (Codex review on #2628) ────────────────

    #[test]
    fn owns_replay_when_unguarded() {
        let result = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                async fn my_handler() -> &'static str { "ok" }
            },
        );
        let code = result.to_string();
        assert!(
            code.contains("__replay_response"),
            "an unguarded handler's gate must still serve a cached replay: {code}"
        );
    }

    #[test]
    fn defers_replay_when_authorize_still_pending() {
        // Before this fix, the gate unconditionally checked for and served a
        // cached idempotency replay, regardless of what other guards were
        // stacked on the same handler. With #[authorize] written below
        // #[feature_flag] (so it hasn't expanded yet and its policy re-check
        // still needs to run), that let this gate return a stale cached
        // response before #[authorize] ever got a chance to deny it.
        let generated = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                #[authorize("update", resource = Post)]
                async fn h() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            !generated.contains("__replay_response"),
            "must defer replay-ownership while #[authorize] is still pending:\n{generated}"
        );
    }

    #[test]
    fn defers_replay_when_stacked_after_another_gate() {
        let generated = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                async fn h(_g: __AutumnSecuredGate_h) -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            !generated.contains("__replay_response"),
            "must defer replay-ownership to an earlier-inserted gate:\n{generated}"
        );
    }

    #[test]
    fn rejects_ambiguous_authorize_shaped_attribute() {
        let generated = feature_flag_macro(
            quote! { "my_flag" },
            quote! {
                #[authz_alias("update", resource = Note)]
                async fn h(note: Note) -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "an attribute sharing #[authorize]'s shape under another name must be refused:\n{generated}"
        );
    }
}
