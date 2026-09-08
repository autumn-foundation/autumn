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
use syn::{Expr, ExprLit, Ident, Lit, LitStr, Meta, Token, parse_quote};

/// Parsed `#[authorize(...)]` arguments.
///
/// Visible to the crate so the route macro's metadata extractor
/// (`api_doc::extract_authorize_bindings`) reads an attribute that has not
/// expanded yet through this same grammar, keeping one source of truth for the
/// syntax.
#[derive(Default)]
pub struct AuthorizeArgs {
    pub action: Option<String>,
    pub resource: Option<Ident>,
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
// `item` is only ever borrowed via `split_leading_items_and_fn(&item)` now,
// but keeps the owned `TokenStream` signature every macro entry point in
// this crate shares (and the proc-macro boundary in `lib.rs` requires).
#[allow(clippy::needless_pass_by_value)]
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

    let (leading_items, mut input_fn) = match crate::parse::split_leading_items_and_fn(&item) {
        Ok(v) => v,
        Err(err) => return err,
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "#[authorize] can only be applied to async functions",
        )
        .to_compile_error();
    }

    // `#[authorize]` written below `#[static_get]`/`#[ws]` — including under
    // an alias those macros' own by-name attribute scan cannot see — is
    // caught here instead, once this guard's own macro is the one running
    // (Codex review on #2513, tenth finding). See
    // `param_helpers::STATIC_ROUTE_HANDLER_MARKER`'s doc comment.
    if let Some(err) = crate::param_helpers::reject_if_incompatible_route_marker(&input_fn) {
        return err;
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
    let resource_lit =
        syn::LitStr::new(&resource_ident.to_string(), proc_macro2::Span::call_site());
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
            // Route macros read this marker when #[authorize] expands before
            // #[get]/#[post]/etc. It records the resource *identifier as
            // written*, not the `Policy` impl that serves the check — that is
            // resolved from the registry at boot and has no compile-time name.
            // Inert: nothing reads it at runtime.
            const __AUTUMN_AUTHORIZE_BINDINGS: &[(&str, &str)] = &[(#action_lit, #resource_lit)];
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

    quote! {
        #leading_items
        #input_fn
    }
}

/// Variant of [`parse_authorize_args`] that allows a leading bare
/// string literal as the action: `#[authorize("update", resource = Foo)]`.
/// Standard `Meta` parsing rejects bare literals as the first item,
/// so we strip and re-thread it before the punctuated parse.
pub fn parse_with_leading_literal(attr: TokenStream) -> syn::Result<AuthorizeArgs> {
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

/// Whether `attr` is literally spelled `#[authorize(...)]`.
///
/// Deliberately name-only: a proc-macro attribute never sees the enclosing
/// module's `use` declarations, so there is no reliable way to tell an
/// aliased `use ::autumn_web::authorize as x; #[x(...)]` apart from an
/// unrelated attribute that happens to share `#[authorize]`'s exact argument
/// grammar (`"action", resource = Type[, from = ident]`) — including one
/// that also happens to sit on a handler with a matching parameter name,
/// which a first attempt at this check used as an extra filter and Codex
/// review on #2628 showed still collides for a plausible `#[audit(...)]`-
/// style attribute. Guessing either way is unsafe: treating every alias as
/// "not authorize" reopens a demonstrated authorization bypass (a stale
/// idempotency replay skips `#[authorize]`'s policy re-check); treating
/// every shape-alike as "authorize" silently drops `.idempotent()`'s dedup
/// guarantee for a route whose real owner never materializes.
///
/// So this stays exact-name-only, and [`reject_if_ambiguous_authorize_shape`]
/// turns the genuinely ambiguous case into a compile error instead of a
/// guess in either direction.
///
/// Also recurses into `#[cfg_attr(predicate, ...)]`: `cfg_attr` is a
/// built-in attribute the compiler does not resolve until after every
/// attribute *macro* has finished expanding, so
/// `#[cfg_attr(feature = "auth", authorize(...))]` (or an aliased spelling
/// of it) reaches this scan with a path of `cfg_attr`, not `authorize` —
/// the same gap `param_helpers::attr_or_cfg_attr_matches_any` closes for
/// `#[secured]`/`#[static_get]` (Codex review on #2513, ninth finding; here,
/// on #2628).
pub fn attr_is_authorize_shaped(attr: &syn::Attribute, _input_fn: &syn::ItemFn) -> bool {
    for_each_conditionally_applied_meta(attr, meta_is_literally_authorize)
}

fn meta_is_literally_authorize(meta: &syn::Meta) -> bool {
    meta.path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "authorize")
}

/// Whether `meta` (an attribute's own [`syn::Meta`], or one nested inside a
/// `#[cfg_attr(predicate, ...)]`) is *not* literally `#[authorize(...)]` but
/// parses through its argument grammar with both the required `action` and
/// `resource` present.
fn meta_is_ambiguous_authorize_shape(meta: &syn::Meta) -> bool {
    if meta_is_literally_authorize(meta) {
        return false;
    }
    let syn::Meta::List(list) = meta else {
        return false;
    };
    let Ok(args) = parse_with_leading_literal(list.tokens.clone()) else {
        return false;
    };
    args.action.is_some() && args.resource.is_some()
}

/// Runs `check` against `attr`'s own [`syn::Meta`], or — if `attr` is
/// `#[cfg_attr(predicate, meta1, meta2, ...)]` — against each conditionally-
/// applied `meta1, meta2, ...` (skipping the leading predicate), recursing
/// when one of those is itself a nested `cfg_attr` (e.g.
/// `#[cfg_attr(a, cfg_attr(b, authorize(...)))]`). Short-circuits on the
/// first match, same as `Iterator::any`.
///
/// The recursion matters: a single level of unwrapping only reaches a
/// `cfg_attr` that wraps the target attribute directly, not one that wraps
/// *another* `cfg_attr` (Codex review on #2628, fifth finding).
fn for_each_conditionally_applied_meta(
    attr: &syn::Attribute,
    check: impl Fn(&syn::Meta) -> bool,
) -> bool {
    meta_or_nested_cfg_attr_matches(&attr.meta, &check)
}

fn meta_or_nested_cfg_attr_matches(meta: &syn::Meta, check: &impl Fn(&syn::Meta) -> bool) -> bool {
    if meta.path().is_ident("cfg_attr") {
        let syn::Meta::List(list) = meta else {
            return false;
        };
        let Ok(nested) = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
            .parse2(list.tokens.clone())
        else {
            return false;
        };
        return nested
            .iter()
            .skip(1)
            .any(|nested_meta| meta_or_nested_cfg_attr_matches(nested_meta, check));
    }
    check(meta)
}

/// Refuses to compile a handler carrying an attribute — plainly written, or
/// conditionally applied via `#[cfg_attr(predicate, ...)]` — that shares
/// `#[authorize]`'s exact argument grammar (`"action", resource = Type[, from
/// = ident]`) under a different name.
///
/// Also refuses a *literal* `#[authorize(...)]` when it only reaches the
/// handler conditionally, through `#[cfg_attr(predicate, authorize(...))]`
/// (at any nesting depth) — unlike a plainly-written `#[authorize(...)]`,
/// whether this one applies at all depends on a `cfg` predicate Autumn
/// cannot evaluate at macro-expansion time (Codex review on #2628, fifth
/// finding). See the doc comment below for why that presence ambiguity is
/// just as unsafe to guess as the name ambiguity.
///
/// Idempotency-replay ownership — which of a `#[secured]`/`#[step_up]`/
/// `#[throttle]` gate, the route macro's standalone `IdempotencyReplayLayer`,
/// or `#[authorize]`'s own in-body check ends up serving a cached replay —
/// depends on knowing, at macro-expansion time, whether a not-yet-expanded
/// `#[authorize]` is present. That can never be resolved reliably by name
/// alone (see [`attr_is_authorize_shaped`]), and a shape-based guess is
/// unsafe in *either* direction, so an attribute matching the grammar under
/// any other name is refused outright: the author must either spell it
/// `#[authorize(...)]` by its real name (no alias) if that's what it is, or
/// rename the unrelated attribute so its shape no longer collides.
///
/// The same two-directions-are-both-unsafe argument applies to a `cfg_attr`-
/// conditional `#[authorize]`, even spelled correctly: treating it as
/// "present" (keeping the standalone replay layer off, an earlier gate
/// deferring to it) silently drops the idempotency guarantee whenever the
/// predicate turns out false and nothing else ends up owning replay;
/// treating it as "absent" reopens the original stale-authorization bypass
/// whenever the predicate turns out true and a cached reply is served before
/// `#[authorize]`'s in-body policy re-check ever runs. So it is refused
/// outright too: the author must apply `#[authorize(...)]` unconditionally
/// (moving any conditional behavior inside the handler body or the policy
/// itself) rather than behind `cfg_attr`.
pub fn reject_if_ambiguous_authorize_shape(input_fn: &syn::ItemFn) -> Option<TokenStream> {
    const AMBIGUOUS_NAME_MESSAGE: &str = "this attribute's arguments match #[authorize]'s grammar (\"action\", \
         resource = Type[, from = ident]) under a different name, which Autumn \
         cannot resolve: a proc-macro attribute never sees `use` aliases, so this \
         could be #[authorize] reached through `use ::autumn_web::authorize as ...;`, \
         or an unrelated attribute that happens to share its shape -- and guessing \
         either way is unsafe for idempotency-replay handling. Spell it \
         `#[authorize(...)]` by its real name if that's what this is, or rename the \
         other attribute so its argument shape no longer collides.";
    const CFG_ATTR_MESSAGE: &str = "an #[authorize]-shaped attribute applied conditionally via #[cfg_attr(...)] \
         cannot be resolved at compile time: Autumn cannot evaluate whether the cfg \
         predicate holds, so it cannot safely decide whether the standalone \
         idempotency-replay layer or an earlier gate should defer to #[authorize]'s \
         own in-body check. Treating it as present would silently drop the \
         idempotency guarantee if the predicate turns out false; treating it as \
         absent would let a cached reply skip #[authorize]'s policy check if the \
         predicate turns out true. Apply #[authorize(...)] unconditionally instead \
         of behind cfg_attr.";

    for attr in &input_fn.attrs {
        if attr.path().is_ident("cfg_attr") {
            if let Some(offending) = find_unsafe_cfg_attr_authorize_meta(&attr.meta) {
                return Some(
                    syn::Error::new_spanned(offending, CFG_ATTR_MESSAGE).to_compile_error(),
                );
            }
            continue;
        }
        if meta_is_ambiguous_authorize_shape(&attr.meta) {
            return Some(syn::Error::new_spanned(attr, AMBIGUOUS_NAME_MESSAGE).to_compile_error());
        }
    }
    None
}

/// Whether `meta` (nested inside a `cfg_attr`) matches #[authorize]'s
/// grammar under a different name, OR is literally `#[authorize(...)]` --
/// unlike at the top level, a literal name nested inside `cfg_attr` is still
/// unsafe to resolve, since its presence (not just its identity) depends on
/// a predicate Autumn cannot evaluate.
fn conditionally_applied_meta_is_unsafe_to_resolve(meta: &syn::Meta) -> bool {
    meta_is_literally_authorize(meta) || meta_is_ambiguous_authorize_shape(meta)
}

/// Recursively unwraps `meta` (expected to be a `cfg_attr(...)` [`syn::Meta`])
/// looking for a nested meta that [`conditionally_applied_meta_is_unsafe_to_resolve`],
/// descending into any nested `cfg_attr` it finds along the way.
fn find_unsafe_cfg_attr_authorize_meta(meta: &syn::Meta) -> Option<syn::Meta> {
    let syn::Meta::List(list) = meta else {
        return None;
    };
    if !list.path.is_ident("cfg_attr") {
        return None;
    }
    let nested = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .ok()?;
    for candidate in nested.iter().skip(1) {
        if candidate.path().is_ident("cfg_attr") {
            if let Some(found) = find_unsafe_cfg_attr_authorize_meta(candidate) {
                return Some(found);
            }
        } else if conditionally_applied_meta_is_unsafe_to_resolve(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── `attr_is_authorize_shaped` / `reject_if_ambiguous_authorize_shape`
    //    (Codex review on #2628 — two rounds) ────────────────────────────────

    #[test]
    fn attr_is_authorize_shaped_is_name_only() {
        // A first attempt at this check fell back to parsing the attribute's
        // argument shape when the name didn't match literally (to catch an
        // aliased #[authorize]), then narrowed that to also require a
        // matching parameter binding. Codex review on #2628 showed even the
        // narrowed version still collides with a plausible #[audit(...)]-
        // style attribute stacked on a handler that legitimately has a
        // same-named resource parameter. There is no shape-based check that
        // is safe in both directions, so this function is exact-name-only —
        // disambiguation happens at compile time instead, in
        // `reject_if_ambiguous_authorize_shape`.
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let attr: syn::Attribute = syn::parse_quote! { #[audit("update", resource = Note)] };
        assert!(!attr_is_authorize_shaped(&attr, &input_fn));

        let aliased: syn::Attribute =
            syn::parse_quote! { #[authz_alias("update", resource = Note)] };
        assert!(
            !attr_is_authorize_shaped(&aliased, &input_fn),
            "an aliased #[authorize] is indistinguishable from an unrelated attribute by name \
             alone — that's exactly why it must be a compile error, not a guess"
        );

        let literal: syn::Attribute = syn::parse_quote! { #[authorize("update", resource = Note)] };
        assert!(attr_is_authorize_shaped(&literal, &input_fn));
    }

    #[test]
    fn rejects_an_aliased_authorize_shape() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[authz_alias("update", resource = Note)]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let generated = reject_if_ambiguous_authorize_shape(&input_fn)
            .expect("an aliased #[authorize]-shaped attribute must be refused")
            .to_string();
        assert!(generated.contains("compile_error"));
    }

    #[test]
    fn rejects_an_unrelated_attribute_sharing_the_shape() {
        // The exact false positive Codex reported: a custom `#[audit(...)]`
        // that happens to share #[authorize]'s grammar *and* sits on a
        // handler with a matching `note` parameter. No real #[authorize]
        // anywhere — this must still be refused, not silently accepted in
        // either direction.
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[audit("update", resource = Note)]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let generated = reject_if_ambiguous_authorize_shape(&input_fn)
            .expect("a shape collision with no real #[authorize] must still be refused")
            .to_string();
        assert!(generated.contains("compile_error"));
    }

    #[test]
    fn rejects_an_aliased_authorize_shape_behind_cfg_attr() {
        // Codex review on #2628 (fourth finding): `cfg_attr` is a built-in
        // attribute the compiler leaves unexpanded until every attribute
        // *macro* has run, so `#[cfg_attr(feature = "auth", authz(...))]`
        // reaches this scan with a path of `cfg_attr`, not `authz` — the
        // same gap `param_helpers::attr_or_cfg_attr_matches_any` already
        // closes for `#[secured]`/`#[static_get]`.
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[cfg_attr(feature = "auth", authz_alias("update", resource = Note))]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let generated = reject_if_ambiguous_authorize_shape(&input_fn)
            .expect("an aliased #[authorize] hidden behind cfg_attr must still be refused")
            .to_string();
        assert!(generated.contains("compile_error"));
    }

    #[test]
    fn rejects_an_unrelated_shape_behind_cfg_attr() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[cfg_attr(feature = "audit_log", audit("update", resource = Note))]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let generated = reject_if_ambiguous_authorize_shape(&input_fn)
            .expect("a shape collision hidden behind cfg_attr must still be refused")
            .to_string();
        assert!(generated.contains("compile_error"));
    }

    #[test]
    fn rejects_the_literal_name_behind_cfg_attr_since_presence_is_unknowable() {
        // Codex review on #2628 (fifth finding): even the *correctly spelled*
        // #[authorize] is unsafe here, because cfg_attr means Autumn cannot
        // know at macro-expansion time whether the predicate holds -- and
        // idempotency-replay ownership needs a definite answer regardless of
        // which way the predicate actually goes at build time.
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[cfg_attr(feature = "auth", authorize("update", resource = Note))]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let generated = reject_if_ambiguous_authorize_shape(&input_fn)
            .expect(
                "a literal #[authorize] whose presence depends on a cfg predicate must still \
                 be refused -- Autumn can't decide replay ownership without knowing whether it \
                 will actually apply",
            )
            .to_string();
        assert!(generated.contains("compile_error"));
    }

    #[test]
    fn rejects_an_aliased_authorize_shape_behind_nested_cfg_attr() {
        // Same finding as above, but nested: a single level of cfg_attr
        // unwrapping only reaches a #[cfg_attr] that wraps the target
        // attribute directly, not one that wraps *another* #[cfg_attr].
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[cfg_attr(feature = "a", cfg_attr(feature = "b", authz_alias("update", resource = Note)))]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let generated = reject_if_ambiguous_authorize_shape(&input_fn)
            .expect("an aliased #[authorize] hidden behind nested cfg_attr must still be refused")
            .to_string();
        assert!(generated.contains("compile_error"));
    }

    #[test]
    fn attr_is_authorize_shaped_recognizes_the_literal_name_behind_nested_cfg_attr() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let attr: syn::Attribute = syn::parse_quote! {
            #[cfg_attr(feature = "a", cfg_attr(feature = "b", authorize("update", resource = Note)))]
        };
        assert!(attr_is_authorize_shaped(&attr, &input_fn));
    }

    #[test]
    fn attr_is_authorize_shaped_recognizes_the_literal_name_behind_cfg_attr() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn h(note: Note) -> &'static str { "ok" }
        };
        let attr: syn::Attribute = syn::parse_quote! {
            #[cfg_attr(feature = "auth", authorize("update", resource = Note))]
        };
        assert!(attr_is_authorize_shaped(&attr, &input_fn));
    }

    #[test]
    fn accepts_the_literal_name_without_ambiguity() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[authorize("update", resource = Note)]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        assert!(
            reject_if_ambiguous_authorize_shape(&input_fn).is_none(),
            "the literal #[authorize] spelling is never ambiguous"
        );
    }

    #[test]
    fn accepts_an_unrelated_attribute_with_a_different_shape() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[instrument(skip(note))]
            async fn h(note: Note) -> &'static str { "ok" }
        };
        assert!(
            reject_if_ambiguous_authorize_shape(&input_fn).is_none(),
            "an attribute with a different argument shape is not ambiguous with #[authorize]"
        );
    }

    #[test]
    fn authorize_rejects_when_invoked_on_a_static_route_handler_via_an_alias() {
        // See `secured::tests::secured_rejects_when_invoked_on_a_static_route_handler_via_an_alias`
        // for the full rationale (Codex review on #2513, tenth finding).
        let accepted = crate::static_route::static_get_macro(
            quote::quote! { "/private" },
            quote::quote! {
                #[auth]
                async fn update_post(post: Post) -> &'static str { "ok" }
            },
        );
        assert!(
            !accepted.to_string().contains("compile_error"),
            "static_get_macro cannot recognize an aliased guard attribute by name: {accepted}"
        );

        let accepted_fn = crate::param_helpers::extract_fn_item(accepted, "update_post");
        let generated = authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! { #accepted_fn },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "authorize_macro must reject a handler already marked as a #[static_get] route, \
             regardless of what alias attribute name the source used to invoke it: {generated}"
        );
    }

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

    #[test]
    fn authorize_emits_binding_marker_const() {
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
            generated.contains(
                r#"const __AUTUMN_AUTHORIZE_BINDINGS : & [(& str , & str)] = & [("update" , "Post")] ;"#
            ),
            "#[authorize] must leave a binding marker the route macro can read back when it \
             expands first: {generated}"
        );
    }

    #[test]
    fn authorize_marker_precedes_policy_check() {
        let generated = authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! {
                async fn update_post(post: Post) -> &'static str {
                    "ok"
                }
            },
        )
        .to_string();

        let marker = generated
            .find("__AUTUMN_AUTHORIZE_BINDINGS")
            .unwrap_or_else(|| panic!("binding marker must be emitted: {generated}"));
        let check = generated
            .find("__check_policy_scoped")
            .unwrap_or_else(|| panic!("policy check must be emitted: {generated}"));
        assert!(
            marker < check,
            "the binding marker must be the first statement of the guarded body, so extractors \
             find it before any generated control flow: {generated}"
        );
    }
}
