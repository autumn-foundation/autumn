//! `#[secured]` proc macro implementation.
//!
//! Generates an authentication/authorization guard that runs before
//! the handler body. Injects hidden `Session` and `AppState` extractors
//! and prepends a call to the runtime check function.
//!
//! ## Forms
//!
//! - `#[secured]` -- require authenticated session (session key exists)
//! - `#[secured("admin")]` -- require a specific role
//! - `#[secured("admin", "editor")]` -- require any of the listed roles
//! - `#[secured(scopes = ["posts:write"])]` -- require a scoped API token that
//!   grants every listed scope. **No session is required** for a scopes-only
//!   gate, so a pure service token (no logged-in user) authorizes on scopes
//!   alone. Default-deny: a token lacking a required scope gets `403`.
//! - `#[secured("admin", scopes = ["posts:write"])]` -- require **both** the
//!   role (via the session) **and** the scope (AND semantics).

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser as _;
use syn::{Expr, ExprLit, ItemFn, Lit, LitStr, Meta, Token, parse_quote};

use crate::idempotency_guard::block_has_replay_guard;
use crate::param_helpers::has_input_named;

/// Parsed `#[secured(...)]` arguments: positional role literals plus an
/// optional `scopes = [...]` list of token abilities.
#[derive(Default)]
struct SecuredArgs {
    roles: Vec<String>,
    scopes: Vec<String>,
}

/// Parse the `#[secured(...)]` attribute arguments.
///
/// Grammar: zero or more leading bare string literals (roles), optionally
/// followed by `scopes = ["a", "b"]`. Examples that must parse:
/// `#[secured]`, `#[secured("admin")]`, `#[secured("a", "b")]`,
/// `#[secured(scopes = ["x"])]`, `#[secured("admin", scopes = ["x"])]`.
fn parse_secured_args(attr: TokenStream) -> syn::Result<SecuredArgs> {
    use proc_macro2::TokenTree;

    if attr.is_empty() {
        return Ok(SecuredArgs::default());
    }

    // Peel off leading bare string literals as roles; bare literals are not
    // valid `Meta`, so they must be consumed before the keyword-style parse.
    let mut iter = attr.into_iter().peekable();
    let mut roles = Vec::new();
    while let Some(TokenTree::Literal(lit)) = iter.peek() {
        let s: LitStr = syn::parse2(quote! { #lit })?;
        roles.push(s.value());
        iter.next();
        if let Some(TokenTree::Punct(p)) = iter.peek()
            && p.as_char() == ','
        {
            iter.next();
        } else {
            break;
        }
    }

    let rest: TokenStream = iter.collect();
    let mut scopes = Vec::new();
    if !rest.is_empty() {
        let metas =
            syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated.parse2(rest)?;
        for meta in metas {
            match meta {
                Meta::NameValue(nv) if nv.path.is_ident("scopes") => {
                    scopes = parse_scope_array(&nv.value)?;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected role string literals and/or `scopes = [\"...\"]`",
                    ));
                }
            }
        }
    }

    Ok(SecuredArgs { roles, scopes })
}

/// Parse `["a", "b"]` into a vec of strings, erroring on non-string elements.
fn parse_scope_array(expr: &Expr) -> syn::Result<Vec<String>> {
    let Expr::Array(arr) = expr else {
        return Err(syn::Error::new_spanned(
            expr,
            "`scopes` must be an array of string literals, e.g. scopes = [\"posts:write\"]",
        ));
    };
    arr.elems
        .iter()
        .map(|el| match el {
            Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) => Ok(s.value()),
            other => Err(syn::Error::new_spanned(
                other,
                "scope entries must be string literals",
            )),
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
pub fn secured_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let SecuredArgs { roles, scopes } = match parse_secured_args(attr) {
        Ok(r) => r,
        Err(err) => return err.to_compile_error(),
    };

    let mut input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "#[secured] can only be applied to async functions",
        )
        .to_compile_error();
    }

    // The session/role check is emitted for the classic forms (`#[secured]`,
    // `#[secured("admin")]`) and whenever a role is required. It is OMITTED for
    // a scopes-ONLY gate so a pure service token with no session authorizes on
    // its scopes alone (injecting the Session extractor would otherwise require
    // a SessionLayer and reject token-only requests).
    let emit_session_check = !roles.is_empty() || scopes.is_empty();
    let emit_scope_check = !scopes.is_empty();

    let role_literals = roles.iter().map(|role| quote! { #role });
    let scope_literals = scopes.iter().map(|scope| quote! { #scope });

    let session_check = if emit_session_check {
        quote! {
            if let ::core::result::Result::Err(__autumn_error) = ::autumn_web::auth::__check_secured_with_key(
                &__autumn_session,
                __autumn_state.auth_session_key(),
                __AUTUMN_SECURED_ROLES,
            ).await {
                if __autumn_error.status() == ::autumn_web::reexports::http::StatusCode::UNAUTHORIZED {
                    if let ::core::option::Option::Some(__autumn_response) =
                        ::autumn_web::idempotency::__replay_finalized_session_response(&__autumn_idempotency_replay)
                    {
                        return __autumn_response;
                    }
                }
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_error);
            }
        }
    } else {
        quote! {}
    };

    let scope_check = if emit_scope_check {
        quote! {
            if let ::core::result::Result::Err(__autumn_error) = ::autumn_web::auth::__check_secured_scopes(
                __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                __AUTUMN_SECURED_SCOPES,
            ).await {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_error);
            }
        }
    } else {
        quote! {}
    };

    let check_call = quote! {
        // Route macros read these markers when #[secured] expands before #[get]/#[post]/etc.
        const __AUTUMN_SECURED_ROLES: &[&str] = &[#(#role_literals),*];
        const __AUTUMN_SECURED_SCOPES: &[&str] = &[#(#scope_literals),*];
        #session_check
        #scope_check
    };
    let original_body = &input_fn.block;
    let original_response = match &input_fn.sig.output {
        syn::ReturnType::Default => quote! {
            let __autumn_inner: () = (async move #original_body).await;
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_inner)
        },
        syn::ReturnType::Type(_, ty) if matches!(ty.as_ref(), syn::Type::ImplTrait(_)) => quote! {
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                (async move #original_body).await
            )
        },
        syn::ReturnType::Type(_, ty) => quote! {
            let __autumn_inner: #ty = (async move #original_body).await;
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_inner)
        },
    };
    let body_already_has_replay_guard = block_has_replay_guard(original_body);
    let replay_stop = if body_already_has_replay_guard {
        quote! {}
    } else {
        quote! {
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        }
    };

    // Inject hidden State<AppState> and Session parameters at the start of
    // the parameter list, but only if no other macro (typically `#[authorize]`) has
    // already injected them. Without these guards, stacking
    // `#[authorize]` + `#[secured]` in either attribute order would
    // produce duplicate hidden parameters and fail to
    // compile.
    //
    // For a scopes-ONLY gate the session/role check is not emitted, so we skip
    // injecting the Session extractor — a token-only route has no SessionLayer
    // and the extractor would otherwise reject the request.
    if emit_session_check && !has_input_named(&input_fn, "__autumn_state") {
        let state_param: syn::FnArg = syn::parse_quote! {
            ::autumn_web::reexports::axum::extract::State(__autumn_state):
                ::autumn_web::reexports::axum::extract::State<::autumn_web::AppState>
        };
        input_fn.sig.inputs.insert(0, state_param);
    }
    if emit_session_check && !has_input_named(&input_fn, "__autumn_session") {
        let session_param: syn::FnArg = syn::parse_quote! {
            __autumn_session: ::autumn_web::session::Session
        };
        input_fn.sig.inputs.insert(0, session_param);
    }
    // Inject the granted-scopes extension when a scope gate is present.
    if emit_scope_check && !has_input_named(&input_fn, "__autumn_token_scopes") {
        let scopes_param: syn::FnArg = syn::parse_quote! {
            __autumn_token_scopes: ::core::option::Option<
                ::autumn_web::reexports::axum::extract::Extension<
                    ::autumn_web::auth::ApiTokenScopes
                >
            >
        };
        input_fn.sig.inputs.insert(0, scopes_param);
    }
    if !has_input_named(&input_fn, "__autumn_idempotency_replay") {
        let idempotency_param: syn::FnArg = syn::parse_quote! {
            __autumn_idempotency_replay: ::core::option::Option<
                ::autumn_web::reexports::axum::extract::Extension<
                    ::autumn_web::idempotency::IdempotencyReplayResponse
                >
            >
        };
        input_fn.sig.inputs.insert(0, idempotency_param);
    }

    input_fn
        .attrs
        .push(parse_quote!(#[allow(clippy::too_many_arguments)]));
    input_fn.sig.output = parse_quote! {
        -> ::autumn_web::reexports::axum::response::Response
    };

    input_fn.block = syn::parse_quote! {
        {
            #check_call
            #replay_stop
            #original_response
        }
    };

    quote! { #input_fn }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::{parse_secured_args, secured_macro};

    #[test]
    fn secured_string_literal_replay_guard_still_injects_replay_stop() {
        let generated = secured_macro(
            quote! {},
            quote! {
                async fn guarded() -> &'static str {
                    let _ = "__AUTUMN_IDEMPOTENCY_REPLAY_GUARD";
                    "ok"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("__replay_response"),
            "plain handler text must not suppress the generated replay stop: {generated}"
        );
    }

    // ── Parser (#1158) ───────────────────────────────────────────────────────

    #[test]
    fn parses_empty() {
        let a = parse_secured_args(quote! {}).unwrap();
        assert!(a.roles.is_empty());
        assert!(a.scopes.is_empty());
    }

    #[test]
    fn parses_roles_only() {
        let a = parse_secured_args(quote! { "admin", "editor" }).unwrap();
        assert_eq!(a.roles, vec!["admin", "editor"]);
        assert!(a.scopes.is_empty());
    }

    #[test]
    fn parses_scopes_only() {
        let a = parse_secured_args(quote! { scopes = ["posts:read", "posts:write"] }).unwrap();
        assert!(a.roles.is_empty());
        assert_eq!(a.scopes, vec!["posts:read", "posts:write"]);
    }

    #[test]
    fn parses_roles_and_scopes() {
        let a = parse_secured_args(quote! { "admin", scopes = ["posts:write"] }).unwrap();
        assert_eq!(a.roles, vec!["admin"]);
        assert_eq!(a.scopes, vec!["posts:write"]);
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(parse_secured_args(quote! { foo = ["x"] }).is_err());
    }

    #[test]
    fn rejects_non_string_scope_entries() {
        assert!(parse_secured_args(quote! { scopes = [1, 2] }).is_err());
    }

    // ── Codegen (#1158) ──────────────────────────────────────────────────────

    #[test]
    fn scopes_only_emits_scope_check_and_no_session_check() {
        let generated = secured_macro(
            quote! { scopes = ["posts:write"] },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("__check_secured_scopes"));
        assert!(
            !generated.contains("__check_secured_with_key"),
            "a scopes-only gate must not emit the session/role check: {generated}"
        );
        assert!(generated.contains("__AUTUMN_SECURED_SCOPES"));
        // No Session extractor is injected for a token-only route.
        assert!(!generated.contains("__autumn_session"));
    }

    #[test]
    fn roles_and_scopes_emits_both_checks() {
        let generated = secured_macro(
            quote! { "admin", scopes = ["posts:write"] },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("__check_secured_with_key"));
        assert!(generated.contains("__check_secured_scopes"));
        assert!(generated.contains("__autumn_session"));
    }

    #[test]
    fn roles_only_preserves_three_arg_session_check_and_marker() {
        let generated = secured_macro(
            quote! { "admin" },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("__check_secured_with_key"));
        assert!(!generated.contains("__check_secured_scopes"));
        // Both markers always emitted for OpenAPI extraction.
        assert!(generated.contains("__AUTUMN_SECURED_ROLES"));
        assert!(generated.contains("__AUTUMN_SECURED_SCOPES"));
    }
}
