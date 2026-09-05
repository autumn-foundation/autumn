//! `#[step_up]` proc macro implementation.
//!
//! Generates a step-up authentication guard that runs as a
//! `FromRequestParts` gate — a hidden, handler-unique parameter inserted
//! ahead of the handler's own parameters — instead of a statement inside the
//! handler body (issue #1668). Axum resolves every `FromRequestParts`
//! extractor, left to right, *before* it ever reaches a `FromRequest` body
//! extractor (`Json` / `Form` / `Multipart`) and short-circuits on the first
//! rejection, so a stale/missing step-up session is rejected before the
//! request body is parsed, rather than after.
//!
//! ## Forms
//!
//! - `#[step_up]` -- require fresh auth with the default max-age (5 minutes)
//! - `#[step_up(max_age = "5m")]` -- require fresh auth within 5 minutes
//! - `#[step_up(max_age = "1h")]` -- require fresh auth within 1 hour

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{LitStr, parse_quote};

use crate::idempotency_guard::should_own_replay;

/// Parse the `#[step_up(max_age = "…")]` attribute arguments.
///
/// Returns `Some(seconds)` when `max_age` is specified, `None` for bare
/// `#[step_up]`.
fn parse_step_up_args(attr: TokenStream) -> syn::Result<Option<u64>> {
    if attr.is_empty() {
        return Ok(None);
    }

    let meta: syn::MetaNameValue = syn::parse2(attr)?;
    let key = meta.path.get_ident().map(std::string::ToString::to_string);
    if key.as_deref() != Some("max_age") {
        return Err(syn::Error::new_spanned(
            &meta.path,
            "#[step_up] only accepts a `max_age` argument (e.g. #[step_up(max_age = \"5m\")])",
        ));
    }

    let value_str: LitStr = match &meta.value {
        syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
            syn::Lit::Str(s) => s.clone(),
            _ => {
                return Err(syn::Error::new_spanned(
                    &meta.value,
                    "max_age must be a string literal, e.g. \"5m\"",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &meta.value,
                "max_age must be a string literal, e.g. \"5m\"",
            ));
        }
    };

    let secs = parse_max_age_str_at_compile_time(&value_str)
        .map_err(|msg| syn::Error::new_spanned(&value_str, msg))?;
    Ok(Some(secs))
}

/// Parse a duration string at macro-expansion time.
fn parse_max_age_str_at_compile_time(lit: &LitStr) -> Result<u64, String> {
    let s = lit.value();
    if let Some(mins) = s.strip_suffix('m') {
        return mins
            .parse::<u64>()
            .map(|m| m * 60)
            .map_err(|_| format!("invalid max_age: '{s}' (expected e.g. \"5m\")"));
    }
    if let Some(hours) = s.strip_suffix('h') {
        return hours
            .parse::<u64>()
            .map(|h| h * 3600)
            .map_err(|_| format!("invalid max_age: '{s}' (expected e.g. \"1h\")"));
    }
    if let Some(secs) = s.strip_suffix('s') {
        return secs
            .parse::<u64>()
            .map_err(|_| format!("invalid max_age: '{s}' (expected e.g. \"30s\")"));
    }
    s.parse::<u64>()
        .map_err(|_| format!("invalid max_age: '{s}' (expected seconds or e.g. \"5m\")"))
}

/// Build the runtime freshness-check token stream that runs inside the gate's
/// `from_request_parts`, after the extractor prelude has bound `state`,
/// `__autumn_session`, `__autumn_step_up_headers`, `__autumn_step_up_uri`,
/// `__autumn_step_up_method`, and `__autumn_idempotency_replay`.
fn build_check_call(max_age_tokens: &TokenStream) -> TokenStream {
    quote! {
        const __AUTUMN_STEP_UP_MAX_AGE: ::core::option::Option<u64> = #max_age_tokens;
        // Resolve max_age before the check so the response can advertise the
        // exact value actually enforced (not the compile-time default).
        let __max_age_secs: u64 =
            ::autumn_web::step_up::__resolve_step_up_max_age(state, __AUTUMN_STEP_UP_MAX_AGE);
        if let ::core::result::Result::Err(__autumn_step_up_error) =
            ::autumn_web::step_up::__check_step_up_with_config(
                &__autumn_session,
                state,
                __AUTUMN_STEP_UP_MAX_AGE,
            ).await
        {
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_finalized_session_response_for_anonymous(
                    &__autumn_session,
                    state.auth_session_key(),
                    &__autumn_idempotency_replay,
                )
                .await
            {
                return ::core::result::Result::Err(__autumn_response);
            }
            let __wants_json: bool = __autumn_step_up_headers
                .get(::autumn_web::reexports::axum::http::header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("application/json") || s.contains("+json"))
                .unwrap_or(false);
            if __wants_json {
                return ::core::result::Result::Err(
                    ::autumn_web::step_up::__step_up_json_response(__max_age_secs),
                );
            } else {
                // For non-GET requests: prefer Referer so the user returns to
                // the page with the form after reauth rather than a POST/DELETE
                // endpoint that has no GET handler.
                // For GET requests: use the current URI so the user is sent
                // directly back to the page they were trying to open.
                let __is_mutating = __autumn_step_up_method != ::autumn_web::reexports::axum::http::Method::GET;
                let __return_to: ::std::string::String = if __is_mutating {
                    let __referer_path = __autumn_step_up_headers
                        .get(::autumn_web::reexports::axum::http::header::REFERER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(::autumn_web::step_up::referer_path);
                    let __path = __referer_path.as_deref().unwrap_or_else(|| {
                        __autumn_step_up_uri
                            .path_and_query()
                            .map(|pq| pq.as_str())
                            .unwrap_or_else(|| __autumn_step_up_uri.path())
                    });
                    ::autumn_web::step_up::encode_return_to(__path)
                } else {
                    ::autumn_web::step_up::encode_return_to(
                        __autumn_step_up_uri
                            .path_and_query()
                            .map(|pq| pq.as_str())
                            .unwrap_or_else(|| __autumn_step_up_uri.path()),
                    )
                };
                return ::core::result::Result::Err(
                    ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                        ::autumn_web::reexports::axum::response::Redirect::to(
                            &::std::format!("/reauth?return_to={__return_to}")
                        )
                    ),
                );
            }
        }
    }
}

/// Returns `true` if `ty` contains an `impl Trait` anywhere in its tree.
///
/// Rust forbids `impl Trait` in local variable type annotations, so the
/// macro must skip the explicit annotation for return types like
/// `AutumnResult<impl IntoResponse>` even though the top-level type is not
/// `impl Trait` itself.
fn type_contains_impl_trait(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::ImplTrait(_) => true,
        syn::Type::Path(tp) => tp.path.segments.iter().any(|seg| match &seg.arguments {
            syn::PathArguments::AngleBracketed(args) => args.args.iter().any(|arg| match arg {
                syn::GenericArgument::Type(t) => type_contains_impl_trait(t),
                _ => false,
            }),
            syn::PathArguments::Parenthesized(args) => {
                args.inputs.iter().any(type_contains_impl_trait)
                    || matches!(&args.output,
                            syn::ReturnType::Type(_, t) if type_contains_impl_trait(t))
            }
            syn::PathArguments::None => false,
        }),
        syn::Type::Reference(r) => type_contains_impl_trait(&r.elem),
        syn::Type::Tuple(t) => t.elems.iter().any(type_contains_impl_trait),
        _ => false,
    }
}

/// Expand the `#[step_up]` / `#[step_up(max_age = "Nm")]` attribute.
#[allow(clippy::too_many_lines)]
pub fn step_up_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let max_age_opt = match parse_step_up_args(attr) {
        Ok(v) => v,
        Err(err) => return err.to_compile_error(),
    };
    // `parse_async_handler_with_preamble` also tolerates zero or more item
    // definitions ahead of the function — the gate `struct` + `impl
    // FromRequestParts` a guard macro that already expanded above this one
    // (e.g. `#[secured]` above `#[step_up]`) leaves behind — and already
    // validates the trailing function is async.
    let (preamble, mut input_fn) = match crate::parse::parse_async_handler_with_preamble(item) {
        Ok(v) => v,
        Err(err) => return err,
    };

    let max_age_tokens = max_age_opt.map_or_else(
        || quote! { ::core::option::Option::None },
        |n| {
            let lit = proc_macro2::Literal::u64_suffixed(n);
            quote! { ::core::option::Option::Some(#lit) }
        },
    );
    let check_call = build_check_call(&max_age_tokens);
    let fn_name = input_fn.sig.ident.clone();
    let gate_ident = format_ident!("__AutumnStepUpGate_{}", fn_name);

    // Whether THIS gate should also serve a cached idempotency replay: see
    // `should_own_replay` for the full ordering rationale (issue #1668's
    // pre-body gates and `#[authorize]`'s in-body check must never both skip
    // replay-ownership, nor both claim it). Replay is checked BEFORE the
    // step-up freshness check (unchanged from before this gate existed): a
    // cached response for an already-completed mutation needs no fresh
    // step-up to replay, since replaying serves the exact stored bytes rather
    // than re-executing the handler.
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
        quote! {
            let __autumn_idempotency_replay = parts
                .extensions
                .get::<::autumn_web::idempotency::IdempotencyReplayResponse>()
                .cloned()
                .map(::autumn_web::reexports::axum::extract::Extension);
        }
    };

    // Both the freshness check and the replay lookup run inside a
    // `FromRequestParts` gate: a hidden parameter inserted ahead of the
    // handler's own parameters, rather than as a statement inside the handler
    // body. Axum resolves every `FromRequestParts` extractor before it ever
    // reaches a `FromRequest` body extractor (`Json` / `Form` / `Multipart`)
    // and short-circuits on the first rejection, so a stale/missing step-up
    // session never causes the body to be parsed.
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
                    // A real `Session` extraction (not a raw extensions
                    // lookup) so a missing `SessionLayer` still fails loudly,
                    // exactly as the hidden `__autumn_session: Session`
                    // handler parameter this replaces did.
                    let __autumn_session: ::autumn_web::session::Session = match
                        <::autumn_web::session::Session as ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState>>
                            ::from_request_parts(parts, state).await
                    {
                        ::core::result::Result::Ok(__session) => __session,
                        ::core::result::Result::Err(__never) => match __never {},
                    };
                    let __autumn_step_up_headers = parts.headers.clone();
                    let __autumn_step_up_uri = parts.uri.clone();
                    let __autumn_step_up_method = parts.method.clone();
                    #replay_check
                    #check_call
                    ::core::result::Result::Ok(#gate_ident)
                }
            }
        }
    };

    let original_body = input_fn.block.clone();
    let original_response = match &input_fn.sig.output {
        syn::ReturnType::Default => quote! {
            let __autumn_inner: () = (async move #original_body).await;
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_inner)
        },
        // Avoid `let x: T = …` when T contains `impl Trait` at any depth.
        // Rust rejects `impl Trait` in local variable type annotations; drop
        // the annotation and let type inference handle it instead.
        syn::ReturnType::Type(_, ty) if type_contains_impl_trait(ty) => quote! {
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
    // A dead-code marker mirroring the real `__AUTUMN_STEP_UP_MAX_AGE` const
    // the gate carries (#1668 moved the runtime check itself into the gate's
    // own `impl` block, out of the handler body). `api_doc::infer_response_body`
    // recovers a guard-rewritten handler's real return type from the
    // `__autumn_inner` binding below, but only accepts that binding when one
    // of `RESPONSE_REWRITING_GUARD_MARKERS` is present earlier in the *same*
    // block — so `#[step_up]` needs its own marker in-body too, exactly like
    // `#[secured]`'s role/scope consts, or a route stacking `#[step_up]`
    // above `#[route]` silently loses its OpenAPI response schema (#1677).
    let body_marker = quote! {
        #[allow(dead_code)]
        const __AUTUMN_STEP_UP_MAX_AGE: ::core::option::Option<u64> = #max_age_tokens;
    };

    input_fn.block = syn::parse_quote! {
        {
            #body_marker
            #original_response
        }
    };

    quote! {
        #preamble
        #gate_item
        #input_fn
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::step_up_macro;

    #[test]
    fn step_up_bare_generates_check_call() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn delete_account() -> &'static str {
                    "deleted"
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("__check_step_up_with_config"),
            "bare #[step_up] should generate a step-up check:\n{generated}"
        );
    }

    #[test]
    fn step_up_with_max_age_minutes_emits_seconds() {
        let generated = step_up_macro(
            quote! { max_age = "5m" },
            quote! {
                async fn delete_account() -> &'static str {
                    "deleted"
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("__check_step_up_with_config"),
            "should contain step-up check:\n{generated}"
        );
        assert!(
            generated.contains("300u64"),
            "5m should expand to 300u64:\n{generated}"
        );
    }

    #[test]
    fn step_up_with_max_age_hours() {
        let generated = step_up_macro(
            quote! { max_age = "1h" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("3600u64"),
            "1h should expand to 3600u64:\n{generated}"
        );
    }

    #[test]
    fn step_up_with_max_age_seconds() {
        let generated = step_up_macro(
            quote! { max_age = "30s" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("30u64"),
            "30s should expand to 30u64:\n{generated}"
        );
    }

    #[test]
    fn step_up_injects_session_parameter() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__autumn_session"),
            "should inject session parameter:\n{generated}"
        );
    }

    #[test]
    fn check_runs_in_a_from_request_parts_gate() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("FromRequestParts"),
            "the check must run in a FromRequestParts gate, not a body statement:\n{generated}"
        );
        assert!(
            generated.contains("struct __AutumnStepUpGate_handler"),
            "should emit a handler-unique gate marker struct:\n{generated}"
        );
        assert!(
            !generated.contains("__autumn_state"),
            "the old hidden State<AppState> handler parameter should be gone — the gate reads \
             state from its own `from_request_parts` argument instead:\n{generated}"
        );
    }

    #[test]
    fn step_up_injects_headers_parameter() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__autumn_step_up_headers"),
            "should inject headers parameter:\n{generated}"
        );
    }

    #[test]
    fn step_up_injects_uri_parameter() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__autumn_step_up_uri"),
            "should inject URI parameter:\n{generated}"
        );
    }

    #[test]
    fn step_up_rejects_sync_functions() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                fn sync_handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "should emit compile_error for non-async functions:\n{generated}"
        );
    }

    #[test]
    fn step_up_rejects_unknown_attribute_key() {
        let generated = step_up_macro(
            quote! { unknown_arg = "value" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "should emit compile_error for unknown attribute key:\n{generated}"
        );
    }

    #[test]
    fn step_up_generates_redirect_for_html_client() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        // Should redirect to /reauth?return_to=… for non-JSON clients
        assert!(
            generated.contains("/reauth"),
            "should redirect to /reauth for HTML clients:\n{generated}"
        );
    }

    #[test]
    fn step_up_generates_json_response_branch() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        // Should call __step_up_json_response for JSON clients
        assert!(
            generated.contains("__step_up_json_response"),
            "should call JSON response helper for API clients:\n{generated}"
        );
    }

    #[test]
    fn inserts_gate_as_first_parameter_when_stacked_with_secured() {
        // Simulate `#[secured]` having already expanded and inserted its own
        // gate parameter ahead of `#[step_up]`'s. Each gate is now an
        // independent `FromRequestParts` parameter (issue #1668) rather than a
        // shared hidden `Session`/`State` pair, so there is no more
        // duplicate-parameter risk to guard against — but `#[step_up]`'s own
        // gate must still land as the new FIRST parameter, ahead of
        // `#[secured]`'s (which then correctly runs second; see
        // `should_own_replay`'s doc comment).
        let generated_fn = {
            let generated = step_up_macro(
                quote! {},
                quote! {
                    async fn handler(_g: __AutumnSecuredGate_handler) -> &'static str { "ok" }
                },
            );
            let items: syn::File = syn::parse2(generated).expect("generated tokens must parse");
            items
                .items
                .into_iter()
                .find_map(|item| match item {
                    syn::Item::Fn(f) if f.sig.ident == "handler" => Some(f),
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
            "__AutumnStepUpGate_handler",
            "the newly-inserted gate must be the FIRST parameter"
        );
    }

    #[test]
    fn defers_replay_when_authorize_still_pending() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                #[authorize("update", resource = Post)]
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            !generated.contains("__replay_response"),
            "must defer replay-ownership while #[authorize] is still pending:\n{generated}"
        );
    }

    #[test]
    fn step_up_injects_method_parameter() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__autumn_step_up_method"),
            "should inject method parameter for GET/POST distinction:\n{generated}"
        );
    }

    #[test]
    fn step_up_uses_resolve_max_age_for_json_response() {
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__resolve_step_up_max_age"),
            "should call __resolve_step_up_max_age so WWW-Authenticate max-age \
             reflects the actual configured value:\n{generated}"
        );
    }

    #[test]
    fn step_up_handles_nested_impl_trait_return_type() {
        // Rust rejects `impl Trait` in local variable type annotations.
        // A handler returning `Result<impl IntoResponse, _>` or
        // `AutumnResult<impl IntoResponse>` must not produce
        // `let __autumn_inner: Result<impl IntoResponse, _> = …`.
        let generated = step_up_macro(
            quote! {},
            quote! {
                async fn handler() -> Result<impl IntoResponse, String> {
                    Ok("ok")
                }
            },
        )
        .to_string();
        // The generated code must NOT contain the explicit local-type annotation
        // when the return type contains impl Trait.
        assert!(
            !generated.contains("__autumn_inner :"),
            "should not emit an explicit local annotation for nested impl Trait: {generated}"
        );
    }
}
