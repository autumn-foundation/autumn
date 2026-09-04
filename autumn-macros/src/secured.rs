//! `#[secured]` proc macro implementation.
//!
//! Generates an authentication/authorization guard that runs as a
//! `FromRequestParts` gate — a hidden, handler-unique parameter inserted
//! ahead of the handler's own parameters — instead of a statement inside the
//! handler body (issue #1668). Axum resolves every `FromRequestParts`
//! extractor, left to right, *before* it ever reaches a `FromRequest` body
//! extractor (`Json` / `Form` / `Multipart`) and short-circuits on the first
//! rejection, so an unauthenticated/unauthorized request is rejected with
//! `401`/`403` before the request body is parsed, rather than after.
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
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{Expr, ExprLit, ItemFn, Lit, LitStr, Meta, Token, parse_quote};

use crate::idempotency_guard::should_own_replay;

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
    // its scopes alone (extracting a real `Session` would otherwise require a
    // `SessionLayer` and reject token-only requests).
    let emit_session_check = !roles.is_empty() || scopes.is_empty();
    let emit_scope_check = !scopes.is_empty();

    let role_literals = roles.iter().map(|role| quote! { #role });
    let scope_literals = scopes.iter().map(|scope| quote! { #scope });
    // A single `TokenStream` (cheaply `Clone`) so the role/scope consts can be
    // emitted twice — once in the handler body for OpenAPI extraction (where
    // nothing in the body reads them any more, hence `allow(dead_code)`), once
    // inside the gate for the actual runtime check — without moving the
    // (single-use) role/scope literal iterators twice.
    let role_scope_consts = quote! {
        #[allow(dead_code)]
        const __AUTUMN_SECURED_ROLES: &[&str] = &[#(#role_literals),*];
        #[allow(dead_code)]
        const __AUTUMN_SECURED_SCOPES: &[&str] = &[#(#scope_literals),*];
    };
    let fn_name = input_fn.sig.ident.clone();
    let gate_ident = format_ident!("__AutumnSecuredGate_{}", fn_name);

    let session_check = if emit_session_check {
        quote! {
            // A real `Session` extraction (not a raw extensions lookup) so a
            // missing `SessionLayer` still fails loudly, exactly as the
            // hidden `__autumn_session: Session` handler parameter this
            // replaces did.
            let __autumn_session: ::autumn_web::session::Session = match
                <::autumn_web::session::Session as ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState>>
                    ::from_request_parts(parts, state).await
            {
                ::core::result::Result::Ok(__session) => __session,
                ::core::result::Result::Err(__never) => match __never {},
            };
            if let ::core::result::Result::Err(__autumn_error) = ::autumn_web::auth::__check_secured_with_key(
                &__autumn_session,
                state.auth_session_key(),
                __AUTUMN_SECURED_ROLES,
            ).await {
                if __autumn_error.status() == ::autumn_web::reexports::http::StatusCode::UNAUTHORIZED {
                    let __autumn_idempotency_replay = parts
                        .extensions
                        .get::<::autumn_web::idempotency::IdempotencyReplayResponse>()
                        .cloned()
                        .map(::autumn_web::reexports::axum::extract::Extension);
                    if let ::core::option::Option::Some(__autumn_response) =
                        ::autumn_web::idempotency::__replay_finalized_session_response(&__autumn_idempotency_replay)
                    {
                        return ::core::result::Result::Err(__autumn_response);
                    }
                }
                return ::core::result::Result::Err(
                    ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_error),
                );
            }
        }
    } else {
        quote! {}
    };

    let scope_check = if emit_scope_check {
        quote! {
            let __autumn_token_scopes = parts
                .extensions
                .get::<::autumn_web::auth::ApiTokenScopes>()
                .cloned();
            if let ::core::result::Result::Err(__autumn_error) = ::autumn_web::auth::__check_secured_scopes(
                __autumn_token_scopes.as_ref(),
                __AUTUMN_SECURED_SCOPES,
            ).await {
                return ::core::result::Result::Err(
                    ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_error),
                );
            }
        }
    } else {
        quote! {}
    };

    // Whether THIS gate should also serve a cached idempotency replay: see
    // `should_own_replay` for the full ordering rationale (issue #1668's
    // pre-body gates and `#[authorize]`'s in-body check must never both skip
    // replay-ownership, nor both claim it).
    let owns_replay = should_own_replay(&input_fn);
    let replay_check = if owns_replay {
        quote! {
            let __autumn_idempotency_replay = parts
                .extensions
                .get::<::autumn_web::idempotency::IdempotencyReplayResponse>()
                .cloned()
                .map(::autumn_web::reexports::axum::extract::Extension);
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return ::core::result::Result::Err(__autumn_response);
            }
        }
    } else {
        quote! {}
    };

    // The marker consts stay in the handler body (not just the gate below) so
    // `api_doc::extract_secured_info` can still recover the role/scope values
    // for OpenAPI when `#[secured]` expands before the route macro.
    let markers = role_scope_consts.clone();

    // Both checks — and any replay lookup this gate owns — run inside a
    // `FromRequestParts` gate: a hidden parameter inserted ahead of the
    // handler's own parameters, rather than as a statement inside the handler
    // body. Axum resolves every `FromRequestParts` extractor before it ever
    // reaches a `FromRequest` body extractor (`Json` / `Form` / `Multipart`)
    // and short-circuits on the first rejection, so an unauthenticated /
    // unauthorized / replayed request never causes the body to be parsed.
    let gate_item = quote! {
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
                    #role_scope_consts
                    #session_check
                    #scope_check
                    #replay_check
                    ::core::result::Result::Ok(#gate_ident)
                }
            }
        }
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

    // Insert the gate as the FIRST parameter — ahead of every other
    // extractor, including any earlier-inserted guard gate (which then
    // correctly runs AFTER this one; see `should_own_replay`'s doc comment).
    let gate_param: syn::FnArg = parse_quote! { _: #gate_ident };
    input_fn.sig.inputs.insert(0, gate_param);

    input_fn
        .attrs
        .push(parse_quote!(#[allow(clippy::too_many_arguments)]));
    input_fn.sig.output = parse_quote! {
        -> ::autumn_web::reexports::axum::response::Response
    };

    input_fn.block = syn::parse_quote! {
        {
            #markers
            #original_response
        }
    };

    quote! {
        #gate_item
        #input_fn
    }
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

    // ── Pre-body FromRequestParts gate (#1668) ──────────────────────────────

    #[test]
    fn check_runs_in_a_from_request_parts_gate() {
        let generated = secured_macro(
            quote! { "admin" },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(
            generated.contains("FromRequestParts"),
            "the check must run in a FromRequestParts gate, not a body statement:\n{generated}"
        );
        assert!(
            generated.contains("struct __AutumnSecuredGate_h"),
            "should emit a handler-unique gate marker struct:\n{generated}"
        );
    }

    #[test]
    fn inserts_gate_as_first_parameter() {
        let generated_fn = {
            let generated = secured_macro(
                quote! { "admin" },
                quote! {
                    async fn h(::autumn_web::reexports::axum::extract::Json(_body): ::autumn_web::reexports::axum::extract::Json<String>) -> &'static str { "ok" }
                },
            );
            let items: syn::File = syn::parse2(generated).expect("generated tokens must parse");
            items
                .items
                .into_iter()
                .find_map(|item| match item {
                    syn::Item::Fn(f) if f.sig.ident == "h" => Some(f),
                    _ => None,
                })
                .expect("handler fn must be present in the expansion")
        };
        let first_param = generated_fn
            .sig
            .inputs
            .first()
            .expect("handler must have at least the gate parameter");
        let syn::FnArg::Typed(pat_type) = first_param else {
            panic!("first parameter must be a typed gate parameter");
        };
        let syn::Type::Path(type_path) = pat_type.ty.as_ref() else {
            panic!("gate parameter must be a named type");
        };
        assert_eq!(
            type_path.path.segments.last().unwrap().ident,
            "__AutumnSecuredGate_h",
            "the gate must be the FIRST parameter — ahead of the body extractor — so Axum \
             resolves (and can reject on) it before ever reaching the body"
        );
    }

    #[test]
    fn handler_body_no_longer_contains_the_session_check() {
        let generated_fn = {
            let generated = secured_macro(
                quote! { "admin" },
                quote! { async fn h() -> &'static str { "ok" } },
            );
            let items: syn::File = syn::parse2(generated).expect("generated tokens must parse");
            items
                .items
                .into_iter()
                .find_map(|item| match item {
                    syn::Item::Fn(f) if f.sig.ident == "h" => Some(f),
                    _ => None,
                })
                .expect("handler fn must be present in the expansion")
        };
        let body = quote! { #generated_fn }.to_string();
        assert!(
            !body.contains("__check_secured_with_key"),
            "the handler body must not call the runtime check directly:\n{body}"
        );
    }

    #[test]
    fn defers_replay_to_an_earlier_gate_when_stacked() {
        let generated = secured_macro(
            quote! { "admin" },
            quote! {
                async fn h(_g: __AutumnThrottleGate_h) -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            !generated.contains("__replay_response"),
            "must defer replay-ownership to the earlier-inserted gate:\n{generated}"
        );
    }

    #[test]
    fn defers_replay_when_authorize_still_pending() {
        let generated = secured_macro(
            quote! { "admin" },
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
}
