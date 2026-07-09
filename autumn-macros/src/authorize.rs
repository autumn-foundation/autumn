//! `#[authorize]` proc macro implementation.
//!
//! Generates a record-level authorization guard that runs as the
//! first statement of the handler body. Resolves the
//! `Policy` registered for the
//! resource type, calls the matching action method, and returns
//! the configured deny response (`403` or `404`) on failure.
//!
//! ## Forms
//!
//! - `#[authorize("update", resource = Post)]` — call
//!   `Post`'s registered policy with action `"update"` against a
//!   handler argument named `post` (`snake_case` of `Post`).
//! - `#[authorize("update", resource = Post, from = post)]` — same,
//!   with an explicit argument name. Use when the handler binds
//!   the loaded resource under a different name.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{Expr, ExprLit, Ident, ItemFn, Lit, LitStr, Meta, Token, parse_quote};

#[derive(Default)]
struct AuthorizeArgs {
    action: Option<String>,
    resource: Option<Ident>,
    from: Option<Ident>,
}

fn parse_authorize_args(attr: TokenStream) -> syn::Result<AuthorizeArgs> {
    if attr.is_empty() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[authorize] requires an action argument: #[authorize(\"update\", resource = Type)]",
        ));
    }

    let metas = syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;
    let mut args = AuthorizeArgs::default();

    for meta in metas {
        match meta {
            Meta::Path(p) => {
                // Bare path: treat as the action verb (after the leading literal).
                if let Some(ident) = p.get_ident()
                    && args.action.is_none()
                {
                    args.action = Some(ident.to_string());
                    continue;
                }
                return Err(syn::Error::new_spanned(
                    p,
                    "expected `action` literal or `key = value`",
                ));
            }
            Meta::NameValue(nv) => {
                let key = nv
                    .path
                    .get_ident()
                    .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected identifier"))?
                    .to_string();
                match key.as_str() {
                    "resource" => {
                        let ident = expect_ident(&nv.value, "resource = TypeName")?;
                        args.resource = Some(ident);
                    }
                    "from" => {
                        let ident = expect_ident(&nv.value, "from = param_name")?;
                        args.from = Some(ident);
                    }
                    other => {
                        return Err(syn::Error::new_spanned(
                            &nv.path,
                            format!("unknown #[authorize] key: {other}"),
                        ));
                    }
                }
            }
            Meta::List(l) => {
                if l.path.is_ident("action") {
                    let lit: LitStr = syn::parse2(l.tokens.clone())?;
                    args.action = Some(lit.value());
                } else {
                    return Err(syn::Error::new_spanned(
                        &l.path,
                        "unexpected list-style argument",
                    ));
                }
            }
        }
    }

    if let Some(action) = first_string_literal(args.action.as_ref()) {
        args.action = Some(action);
    }

    Ok(args)
}

fn first_string_literal(action: Option<&String>) -> Option<String> {
    action.and_then(|s| {
        // Strip surrounding quotes if the action came in as a stringified literal.
        let trimmed = s.trim();
        if (trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        {
            Some(trimmed[1..trimmed.len() - 1].to_owned())
        } else {
            None
        }
    })
}

fn expect_ident(expr: &Expr, hint: &str) -> syn::Result<Ident> {
    match expr {
        Expr::Path(p) if p.path.get_ident().is_some() => Ok(p.path.get_ident().unwrap().clone()),
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Ok(format_ident!("{}", s.value())),
        _ => Err(syn::Error::new_spanned(expr, format!("expected `{hint}`"))),
    }
}

use crate::idempotency_guard::block_has_replay_guard;
use crate::param_helpers::has_input_named;

fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[allow(clippy::too_many_lines)]
pub fn authorize_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse the args first; the parser may surface a leading `"action"`
    // string literal as the bare-Path action via the `Meta::Path` branch
    // by pre-parsing it.
    let mut args = match parse_with_leading_literal(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let Some(action_str) = args.action.take() else {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[authorize] requires an action: #[authorize(\"update\", resource = Type)]",
        )
        .to_compile_error();
    };

    let Some(resource_ident) = args.resource else {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[authorize] requires `resource = TypeName`",
        )
        .to_compile_error();
    };

    let from_ident = args.from.unwrap_or_else(|| {
        let name = snake_case(&resource_ident.to_string());
        format_ident!("{}", name)
    });

    let mut input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "#[authorize] can only be applied to async functions",
        )
        .to_compile_error();
    }

    // Inject hidden `Session` and `State<AppState>` arguments so the
    // check can read the user id from the session and resolve the
    // registered policy from `AppState`. We wrap AppState in
    // `State<...>` because AppState itself is not a
    // `FromRequestParts` extractor — only `State<AppState>` is.
    //
    // Skip injection when the function already has a parameter
    // bound to `__autumn_session` / `__autumn_state` — the common
    // case is stacking `#[authorize]` on top of `#[secured]`, which
    // already injects `__autumn_session`. Re-injecting would
    // produce a duplicate parameter name and fail to compile.
    if !has_input_named(&input_fn, "__autumn_state") {
        let state_param: syn::FnArg = parse_quote! {
            ::autumn_web::reexports::axum::extract::State(__autumn_state):
                ::autumn_web::reexports::axum::extract::State<::autumn_web::AppState>
        };
        input_fn.sig.inputs.insert(0, state_param);
    }
    if !has_input_named(&input_fn, "__autumn_session") {
        let session_param: syn::FnArg = parse_quote! {
            __autumn_session: ::autumn_web::session::Session
        };
        input_fn.sig.inputs.insert(0, session_param);
    }
    if !has_input_named(&input_fn, "__autumn_idempotency_replay") {
        let idempotency_param: syn::FnArg = parse_quote! {
            __autumn_idempotency_replay: ::core::option::Option<
                ::autumn_web::reexports::axum::extract::Extension<
                    ::autumn_web::idempotency::IdempotencyReplayResponse
                >
            >
        };
        input_fn.sig.inputs.insert(0, idempotency_param);
    }
    if !has_input_named(&input_fn, "__autumn_route_version") {
        let route_version_param: syn::FnArg = parse_quote! {
            __autumn_route_version: ::core::option::Option<
                ::autumn_web::reexports::axum::extract::Extension<
                    ::autumn_web::RouteVersionMetadata
                >
            >
        };
        input_fn.sig.inputs.insert(0, route_version_param);
    }
    // Inject the granted-scopes extension so the policy check can decide on
    // `ctx.has_scope(...)` for token-authenticated principals. Guarded so
    // stacking `#[secured(scopes = ...)]` + `#[authorize]` doesn't double-inject.
    if !has_input_named(&input_fn, "__autumn_token_scopes") {
        let scopes_param: syn::FnArg = parse_quote! {
            __autumn_token_scopes: ::core::option::Option<
                ::autumn_web::reexports::axum::extract::Extension<
                    ::autumn_web::auth::ApiTokenScopes
                >
            >
        };
        input_fn.sig.inputs.insert(0, scopes_param);
    }

    let action_lit = syn::LitStr::new(&action_str, proc_macro2::Span::call_site());
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
    input_fn
        .attrs
        .push(parse_quote!(#[allow(clippy::too_many_arguments)]));
    input_fn.sig.output = parse_quote! {
        -> ::autumn_web::reexports::axum::response::Response
    };
    input_fn.block = parse_quote! {
        {
            if let ::core::result::Result::Err(__autumn_error) = ::autumn_web::authorization::__check_policy_scoped::<#resource_ident>(
                &__autumn_state,
                &__autumn_session,
                __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                #action_lit,
                &#from_ident,
            ).await {
                if let ::core::option::Option::Some(__autumn_response) =
                    ::autumn_web::idempotency::__replay_finalized_session_response_for_anonymous(
                        &__autumn_session,
                        __autumn_state.auth_session_key(),
                        &__autumn_idempotency_replay,
                    )
                    .await
                {
                    return __autumn_response;
                }
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_error);
            }
            if let ::core::option::Option::Some(::autumn_web::reexports::axum::extract::Extension(__autumn_meta)) = &__autumn_route_version {
                if let ::core::option::Option::Some(__autumn_response) = ::autumn_web::__private::check_sunset(
                    &__autumn_state,
                    __autumn_meta,
                ) {
                    return __autumn_response;
                }
            }
            #replay_stop
            #original_response
        }
    };

    quote! { #input_fn }
}

/// Variant of [`parse_authorize_args`] that allows a leading bare
/// string literal as the action: `#[authorize("update", resource = Foo)]`.
/// Standard `Meta` parsing rejects bare literals as the first item,
/// so we strip and re-thread it before the punctuated parse.
fn parse_with_leading_literal(attr: TokenStream) -> syn::Result<AuthorizeArgs> {
    use proc_macro2::TokenTree;
    let mut iter = attr.into_iter().peekable();
    let mut leading_action: Option<String> = None;
    if let Some(TokenTree::Literal(lit)) = iter.peek() {
        let lit_str = lit.to_string();
        if (lit_str.starts_with('"') && lit_str.ends_with('"'))
            || (lit_str.starts_with('\'') && lit_str.ends_with('\''))
        {
            // Reparse as a syn::LitStr to strip quotes correctly.
            let s: LitStr = syn::parse2(quote! { #lit })?;
            leading_action = Some(s.value());
            iter.next();
            // Skip the comma that follows, if present.
            if let Some(TokenTree::Punct(p)) = iter.peek()
                && p.as_char() == ','
            {
                iter.next();
            }
        }
    }
    let rest: TokenStream = iter.collect();
    let mut parsed = if rest.is_empty() {
        AuthorizeArgs::default()
    } else {
        parse_authorize_args(rest)?
    };
    if let Some(action) = leading_action {
        parsed.action = Some(action);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_action_and_resource() {
        let tokens: TokenStream = r#""update", resource = Post"#.parse().unwrap();
        let args = parse_with_leading_literal(tokens).unwrap();
        assert_eq!(args.action.as_deref(), Some("update"));
        assert_eq!(args.resource.unwrap().to_string(), "Post");
    }

    #[test]
    fn parses_with_explicit_from() {
        let tokens: TokenStream = r#""delete", resource = Post, from = the_post"#.parse().unwrap();
        let args = parse_with_leading_literal(tokens).unwrap();
        assert_eq!(args.action.as_deref(), Some("delete"));
        assert_eq!(args.from.unwrap().to_string(), "the_post");
    }

    #[test]
    fn rejects_missing_action() {
        let tokens: TokenStream = "resource = Post".parse().unwrap();
        let args = parse_with_leading_literal(tokens).unwrap();
        assert!(args.action.is_none());
    }

    #[test]
    fn snake_case_handles_pascal_case() {
        assert_eq!(snake_case("Post"), "post");
        assert_eq!(snake_case("BlogPost"), "blog_post");
        assert_eq!(snake_case("HTTPRequest"), "h_t_t_p_request");
    }

    #[test]
    fn authorize_string_literal_replay_guard_still_injects_replay_stop() {
        let generated = authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! {
                async fn update_post(post: Post) -> &'static str {
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

    #[test]
    fn authorize_denial_can_replay_finalized_session_response_for_old_cookie() {
        let generated = authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! {
                async fn update_post(post: Post) -> &'static str {
                    "ok"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("__replay_finalized_session_response_for_anonymous"),
            "authorized handlers must let old destroyed-session retries receive cached finalized Set-Cookie responses: {generated}"
        );
    }

    #[test]
    fn authorize_injects_token_scopes_and_calls_scoped_policy_check() {
        let generated = authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! {
                async fn update_post(post: Post) -> &'static str {
                    "ok"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("__check_policy_scoped"),
            "#[authorize] must call __check_policy_scoped so token scopes reach the policy: {generated}"
        );
        assert!(
            generated.contains("__autumn_token_scopes"),
            "#[authorize] must inject __autumn_token_scopes parameter: {generated}"
        );
        // The old 4-arg form must NOT appear — we always use the scoped variant.
        assert!(
            !generated.contains("__check_policy (") && !generated.contains("__check_policy("),
            "#[authorize] must not generate the old unscoped __check_policy call: {generated}"
        );
    }
}
