//! `#[throttle]` proc macro implementation.
//!
//! Generates a per-route rate-limit guard that runs as a `FromRequestParts`
//! gate — a hidden, handler-unique parameter inserted ahead of the handler's
//! own parameters — instead of a statement inside the handler body (issue
//! #1668). Axum resolves every `FromRequestParts` extractor, left to right,
//! *before* it ever reaches a `FromRequest` body extractor (`Json` / `Form` /
//! `Multipart`) and short-circuits on the first rejection, so an over-limit
//! request is rejected with `429` before the request body is parsed or
//! buffered, rather than after.
//!
//! ## Forms
//!
//! - `#[throttle(limit = 5, per = "1m")]` -- inline limit, default key strategy
//! - `#[throttle(limit = 5, per = "1m", key = "ip")]` -- inline with key override
//! - `#[throttle(limit = 5, per = "1m", key = "principal")]`
//! - `#[throttle(limit = 5, per = "1m", key = "token")]`
//! - `#[throttle("login")]` -- named limiter defined in
//!   `[security.rate_limit.named.login]`

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{Expr, ItemFn, Lit, LitInt, LitStr, parse_quote};

use crate::idempotency_guard::should_own_replay;

/// Parsed `#[throttle(...)]` attribute arguments.
enum ThrottleAttrs {
    /// `#[throttle("login")]`
    Named(String),
    /// `#[throttle(limit = 5, per = "1m", key = "ip")]`
    Inline {
        limit: u32,
        per_secs: u64,
        key: Option<String>,
    },
}

fn parse_key_str(s: &str) -> Result<&'static str, String> {
    match s {
        "ip" => Ok("ip"),
        "principal" => Ok("principal"),
        "token" => Ok("token"),
        other => Err(format!(
            "#[throttle] key must be \"ip\", \"principal\", or \"token\" (got \"{other}\")"
        )),
    }
}

/// Parse a duration string like `"1m"`, `"30s"`, `"1h"` at macro-expand time.
fn parse_per_str(s: &str) -> Result<u64, String> {
    let mut total_secs: u64 = 0;
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if ch.is_ascii_alphabetic() {
            let num: u64 = current
                .parse()
                .map_err(|_| format!("invalid per: '{s}' (expected e.g. \"1m\")"))?;
            current.clear();
            match ch {
                's' => total_secs = total_secs.checked_add(num).ok_or("overflow")?,
                'm' => {
                    total_secs = total_secs
                        .checked_add(num.checked_mul(60).ok_or("overflow")?)
                        .ok_or("overflow")?;
                }
                'h' => {
                    total_secs = total_secs
                        .checked_add(num.checked_mul(3600).ok_or("overflow")?)
                        .ok_or("overflow")?;
                }
                'd' => {
                    total_secs = total_secs
                        .checked_add(num.checked_mul(86400).ok_or("overflow")?)
                        .ok_or("overflow")?;
                }
                _ => return Err(format!("invalid per: '{s}' (unit must be s/m/h/d)")),
            }
        } else if ch == ' ' {
            // skip
        } else {
            return Err(format!("invalid per: '{s}'"));
        }
    }
    if !current.is_empty() {
        return Err(format!("invalid per: '{s}' (trailing number without unit)"));
    }
    if total_secs == 0 {
        return Err(format!("invalid per: '{s}' (must be > 0)"));
    }
    Ok(total_secs)
}

fn parse_throttle_args(attr: TokenStream) -> syn::Result<ThrottleAttrs> {
    use proc_macro2::TokenTree;

    if attr.is_empty() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[throttle] requires either a named limiter (`#[throttle(\"name\")]`) or \
             `limit`/`per` arguments (`#[throttle(limit = 5, per = \"1m\")]`)",
        ));
    }

    // Peek for a leading bare string literal — the named form.
    let mut iter = attr.clone().into_iter().peekable();
    if let Some(TokenTree::Literal(lit)) = iter.peek() {
        let s: LitStr = syn::parse2(quote! { #lit })?;
        iter.next();
        // No more args allowed after the name.
        if iter.next().is_some() {
            return Err(syn::Error::new_spanned(
                &s,
                "#[throttle(\"name\")] takes no other arguments",
            ));
        }
        return Ok(ThrottleAttrs::Named(s.value()));
    }

    // Otherwise expect keyword form.
    let mut limit: Option<u32> = None;
    let mut per: Option<u64> = None;
    let mut key: Option<String> = None;

    syn::meta::parser(|meta| {
        if meta.path.is_ident("limit") {
            let expr: Expr = meta.value()?.parse()?;
            let n: u32 = match &expr {
                Expr::Lit(l) => match &l.lit {
                    Lit::Int(i) => i.base10_parse::<u32>()?,
                    _ => return Err(meta.error("`limit` must be a positive integer")),
                },
                _ => return Err(meta.error("`limit` must be a literal positive integer")),
            };
            if n == 0 {
                return Err(meta.error("`limit` must be greater than zero"));
            }
            limit = Some(n);
            Ok(())
        } else if meta.path.is_ident("per") {
            let s: LitStr = meta.value()?.parse()?;
            let secs = parse_per_str(&s.value()).map_err(|msg| syn::Error::new_spanned(&s, msg))?;
            per = Some(secs);
            Ok(())
        } else if meta.path.is_ident("key") {
            let s: LitStr = meta.value()?.parse()?;
            let name = parse_key_str(&s.value()).map_err(|msg| syn::Error::new_spanned(&s, msg))?;
            key = Some(name.to_owned());
            Ok(())
        } else {
            Err(meta.error("unsupported #[throttle] argument: expected `limit`, `per`, or `key`"))
        }
    })
    .parse2(attr)?;

    let limit = limit.ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[throttle] requires a `limit = N` argument",
        )
    })?;
    let per_secs = per.ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[throttle] requires a `per = \"1m\"` argument",
        )
    })?;

    Ok(ThrottleAttrs::Inline {
        limit,
        per_secs,
        key,
    })
}

/// Returns `true` if `ty` contains an `impl Trait` anywhere in its tree.
///
/// Rust rejects `impl Trait` in local variable type annotations, so the
/// wrapper must skip the explicit annotation when the handler return type
/// contains `impl Trait` at any depth (e.g. `AutumnResult<impl IntoResponse>`).
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

fn build_spec_tokens(attrs: &ThrottleAttrs) -> TokenStream {
    match attrs {
        ThrottleAttrs::Named(name) => {
            let name_lit = LitStr::new(name, proc_macro2::Span::call_site());
            quote! {
                ::autumn_web::security::ThrottleSpec::Named(#name_lit)
            }
        }
        ThrottleAttrs::Inline {
            limit,
            per_secs,
            key,
        } => {
            let limit_lit = LitInt::new(&limit.to_string(), proc_macro2::Span::call_site());
            let per_lit = LitInt::new(&per_secs.to_string(), proc_macro2::Span::call_site());
            let key_tokens = match key.as_deref() {
                None => quote! { ::core::option::Option::None },
                Some("ip") => quote! {
                    ::core::option::Option::Some(::autumn_web::security::KeyStrategy::Ip)
                },
                Some("principal") => quote! {
                    ::core::option::Option::Some(
                        ::autumn_web::security::KeyStrategy::AuthenticatedPrincipal
                    )
                },
                Some("token") => quote! {
                    ::core::option::Option::Some(::autumn_web::security::KeyStrategy::ApiToken)
                },
                Some(_) => unreachable!("key strategies validated at parse time"),
            };
            quote! {
                ::autumn_web::security::ThrottleSpec::Inline {
                    limit: #limit_lit,
                    per_secs: #per_lit,
                    key: #key_tokens,
                }
            }
        }
    }
}

/// Expand the `#[throttle(...)]` attribute.
#[allow(clippy::too_many_lines)]
pub fn throttle_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_throttle_args(attr) {
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
            "#[throttle] can only be applied to async functions",
        )
        .to_compile_error();
    }

    let fn_name = input_fn.sig.ident.clone();
    let fn_name_str = fn_name.to_string();
    let spec_tokens = build_spec_tokens(&attrs);
    let gate_ident = format_ident!("__AutumnThrottleGate_{}", fn_name);

    // Restated in the handler body (not just the gate below) so the route
    // macro's `RESPONSE_REWRITING_GUARD_MARKERS` scan — which recovers the
    // handler's pre-rewrite return type from the `__autumn_inner` binding
    // when `#[throttle]` expands before the route attribute (#1677) — finds
    // this guard's marker in the same block as that binding. Mirrors
    // `secured_macro`'s identical `markers`-in-body treatment.
    let markers = quote! {
        #[allow(dead_code)]
        const __AUTUMN_THROTTLE_ROUTE_ID: &str =
            ::core::concat!(::core::module_path!(), "::", #fn_name_str);
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

    // The throttle check must run BEFORE the idempotency-replay lookup.
    // Returning a cached response ahead of `__check_throttle` would let repeat
    // requests reusing the same `Idempotency-Key` bypass the per-route bucket
    // forever. Running the throttle check first ensures replays still consume
    // the bucket and can 429 once the route limit is exhausted; only if the
    // throttle check passes do we replay any cached response.
    //
    // Both the check and the replay lookup run inside a `FromRequestParts`
    // gate — a hidden parameter inserted ahead of the handler's own
    // parameters — rather than as a statement inside the handler body. Axum
    // resolves every `FromRequestParts` extractor before it ever reaches a
    // `FromRequest` body extractor (`Json` / `Form` / `Multipart`) and
    // short-circuits on the first rejection, so an over-limit or replayed
    // request never causes the body to be parsed or buffered.
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
                    // Stable per-handler bucket namespace. Two routes pointing
                    // at the same named limiter share their bucket via the
                    // runtime registry — the route_id here is only used for
                    // inline limiters and for uniqueness when a named entry is
                    // missing from config.
                    const __AUTUMN_THROTTLE_ROUTE_ID: &str =
                        ::core::concat!(::core::module_path!(), "::", #fn_name_str);
                    let __autumn_throttle_headers = parts.headers.clone();
                    // Optional because `MatchedPath` is absent for some routes
                    // (fallbacks, unnested handlers). When present, the
                    // runtime matched route pattern isolates an INLINE
                    // throttle's bucket per mounted path — a handler reused
                    // under two `scoped` prefixes maps to the same
                    // compile-time `route_id`, so folding the matched path in
                    // gives each mount its own bucket. `__check_throttle`
                    // consults it only for inline throttles.
                    let __autumn_throttle_matched_path = parts
                        .extensions
                        .get::<::autumn_web::reexports::axum::extract::MatchedPath>()
                        .cloned();
                    let __autumn_throttle_peer = parts
                        .extensions
                        .get::<::autumn_web::reexports::axum::extract::ConnectInfo<::std::net::SocketAddr>>()
                        .copied();
                    let __autumn_throttle_principal = parts
                        .extensions
                        .get::<::autumn_web::security::RateLimitPrincipal>()
                        .cloned();
                    // Optional so throttled routes without session middleware
                    // (or a per-route throttle that doesn't key on the
                    // principal) still compile and run. `__check_throttle`
                    // only consults it for `key = "principal"` when no
                    // `RateLimitPrincipal` extension was installed, deriving
                    // the principal from the same verified session
                    // `populate_rate_limit_principal` reads. Read directly
                    // from extensions (not the `Session` extractor) so a
                    // route with no `SessionLayer` installed does not panic.
                    let __autumn_throttle_session = parts
                        .extensions
                        .get::<::autumn_web::session::Session>()
                        .cloned();
                    let __autumn_throttle_exempt = parts
                        .extensions
                        .get::<::autumn_web::security::RateLimitExempt>()
                        .is_some();
                    if let ::core::result::Result::Err(__autumn_throttle_response) =
                        ::autumn_web::security::__check_throttle(
                            state,
                            __AUTUMN_THROTTLE_ROUTE_ID,
                            __autumn_throttle_matched_path.as_ref().map(|__mp| __mp.as_str()),
                            #spec_tokens,
                            &__autumn_throttle_headers,
                            __autumn_throttle_peer.map(|c| c.0),
                            __autumn_throttle_principal.as_ref(),
                            __autumn_throttle_session.as_ref(),
                            __autumn_throttle_exempt,
                        ).await
                    {
                        return ::core::result::Result::Err(__autumn_throttle_response);
                    }
                    #replay_check
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
        syn::ReturnType::Type(_, ty) if type_contains_impl_trait(ty) => quote! {
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                (async move #original_body).await
            )
        },
        // A bare numeric/bool primitive does not implement `IntoResponse`;
        // Autumn's plain primitive-output wrapper serves it by stringifying. The
        // route macro suppresses that wrapper when `#[throttle]` is present (the
        // handler is rewritten to return `Response`), so mirror the stringify
        // here to keep primitive-returning throttled handlers compiling.
        syn::ReturnType::Type(_, ty)
            if crate::route::should_stringify_primitive_output(&input_fn.sig.output) =>
        {
            quote! {
                let __autumn_inner: #ty = (async move #original_body).await;
                ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    ::std::string::ToString::to_string(&__autumn_inner)
                )
            }
        }
        syn::ReturnType::Type(_, ty) => quote! {
            let __autumn_inner: #ty = (async move #original_body).await;
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_inner)
        },
    };

    // Insert the gate as the FIRST parameter. Axum evaluates `FromRequestParts`
    // extractors left to right, so this must sit ahead of every other
    // extractor — including any earlier-inserted guard gate, which then
    // correctly runs AFTER (see `should_own_replay`'s doc comment: whichever
    // guard is applied to a still-unguarded function owns replay, and
    // "unguarded" is judged at each macro's OWN expansion time, before later
    // macros insert their gates further left).
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::throttle_macro;

    #[test]
    fn inline_form_generates_check_call() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn login() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__check_throttle"),
            "should emit runtime check call:\n{generated}"
        );
        assert!(
            generated.contains("ThrottleSpec :: Inline"),
            "should emit an inline spec:\n{generated}"
        );
        assert!(
            generated.contains("limit : 5") || generated.contains("limit: 5"),
            "should embed the limit value:\n{generated}"
        );
        assert!(
            generated.contains("60"),
            "1m should expand to 60 seconds:\n{generated}"
        );
    }

    #[test]
    fn named_form_generates_named_spec() {
        let generated = throttle_macro(
            quote! { "login" },
            quote! {
                async fn login() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("ThrottleSpec :: Named"),
            "should emit a named spec:\n{generated}"
        );
        assert!(
            generated.contains("\"login\""),
            "should reference the limiter name:\n{generated}"
        );
    }

    #[test]
    fn key_principal_selects_authenticated_principal() {
        let generated = throttle_macro(
            quote! { limit = 3, per = "1s", key = "principal" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("AuthenticatedPrincipal"),
            "should map key = \"principal\" to AuthenticatedPrincipal:\n{generated}"
        );
    }

    #[test]
    fn key_token_selects_api_token() {
        let generated = throttle_macro(
            quote! { limit = 3, per = "1s", key = "token" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("ApiToken"),
            "should map key = \"token\" to ApiToken:\n{generated}"
        );
    }

    #[test]
    fn key_ip_selects_ip_strategy() {
        let generated = throttle_macro(
            quote! { limit = 3, per = "1s", key = "ip" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("KeyStrategy :: Ip"),
            "should map key = \"ip\" to KeyStrategy::Ip:\n{generated}"
        );
    }

    #[test]
    fn rejects_sync_functions() {
        let generated = throttle_macro(
            quote! { limit = 3, per = "1s" },
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
    fn rejects_missing_arguments() {
        let generated = throttle_macro(
            quote! {},
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "should emit compile_error when no arguments supplied:\n{generated}"
        );
    }

    #[test]
    fn rejects_unknown_key_strategy() {
        let generated = throttle_macro(
            quote! { limit = 3, per = "1s", key = "unknown" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "should emit compile_error for unknown key strategy:\n{generated}"
        );
    }

    #[test]
    fn rejects_missing_per() {
        let generated = throttle_macro(
            quote! { limit = 5 },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "should emit compile_error when `per` is missing:\n{generated}"
        );
    }

    #[test]
    fn rejects_bad_per_syntax() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "not-a-duration" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "should emit compile_error for bad `per` string:\n{generated}"
        );
    }

    #[test]
    fn gate_reads_state_from_from_request_parts() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("FromRequestParts"),
            "check should run in a FromRequestParts gate, not a body statement:\n{generated}"
        );
        assert!(
            generated.contains("__check_throttle"),
            "should still emit the runtime check call:\n{generated}"
        );
        assert!(
            !generated.contains("__autumn_state"),
            "the old hidden State<AppState> handler parameter should be gone — the gate reads \
             state from its own `from_request_parts` argument instead:\n{generated}"
        );
    }

    #[test]
    fn injects_headers_and_connect_info() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__autumn_throttle_headers"),
            "should read headers from request parts:\n{generated}"
        );
        assert!(
            generated.contains("ConnectInfo"),
            "should read ConnectInfo from request parts:\n{generated}"
        );
    }

    #[test]
    fn injects_matched_path_extractor() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__autumn_throttle_matched_path"),
            "should read MatchedPath from request parts:\n{generated}"
        );
        assert!(
            generated.contains("MatchedPath"),
            "should reference axum MatchedPath:\n{generated}"
        );
    }

    #[test]
    fn emits_gate_struct_and_impl_as_sibling_items() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("struct __AutumnThrottleGate_handler"),
            "should emit a handler-unique gate marker struct:\n{generated}"
        );
        assert!(
            generated
                .contains("impl :: autumn_web :: reexports :: axum :: extract :: FromRequestParts"),
            "gate must implement FromRequestParts so Axum resolves it before the body extractor:\n{generated}"
        );
    }

    #[test]
    fn inserts_gate_as_first_parameter() {
        let generated_fn = {
            let generated = throttle_macro(
                quote! { limit = 5, per = "1m" },
                quote! {
                    async fn handler(::autumn_web::reexports::axum::extract::Json(_body): ::autumn_web::reexports::axum::extract::Json<String>) -> &'static str { "ok" }
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
            "__AutumnThrottleGate_handler",
            "the gate must be the FIRST parameter — ahead of the body extractor — so Axum \
             resolves (and can reject on) it before ever reaching the body"
        );
    }

    #[test]
    fn handler_body_no_longer_contains_the_throttle_check() {
        // The whole point of issue #1668: the runtime check must live in the
        // gate's `FromRequestParts` impl, not in a statement inside the
        // handler body (which only runs after every extractor — including a
        // body extractor — has already succeeded).
        let generated_fn = {
            let generated = throttle_macro(
                quote! { limit = 5, per = "1m" },
                quote! {
                    async fn handler() -> &'static str { "ok" }
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
        let body = quote! { #generated_fn }.to_string();
        assert!(
            !body.contains("__check_throttle"),
            "the handler body must not call the runtime check directly:\n{body}"
        );
    }

    #[test]
    fn owns_replay_when_unguarded() {
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated.contains("__replay_response"),
            "an otherwise-unguarded throttled handler's gate must own replay-serving:\n{generated}"
        );
    }

    #[test]
    fn defers_replay_to_an_earlier_gate_when_stacked() {
        // Simulate `#[secured]` having already expanded and inserted its own
        // gate parameter ahead of `#[throttle]`'s.
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
            quote! {
                async fn handler(_g: __AutumnSecuredGate_handler) -> &'static str { "ok" }
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
        // `#[authorize]` written below `#[throttle]` hasn't expanded yet, so
        // its in-body policy check will only run once the handler body is
        // invoked — strictly after this gate. If this gate served a cached
        // replay itself, `#[authorize]`'s check would never run for a replay.
        let generated = throttle_macro(
            quote! { limit = 5, per = "1m" },
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
    fn per_hours_and_seconds_parse() {
        let generated_h = throttle_macro(
            quote! { limit = 1, per = "2h" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated_h.contains("7200"),
            "2h should expand to 7200 seconds:\n{generated_h}"
        );

        let generated_s = throttle_macro(
            quote! { limit = 1, per = "30s" },
            quote! {
                async fn handler() -> &'static str { "ok" }
            },
        )
        .to_string();
        assert!(
            generated_s.contains("30"),
            "30s should expand to 30 seconds:\n{generated_s}"
        );
    }
}
