//! Route macro implementation.
//!
//! Generates two companion functions for each annotated handler:
//!
//! 1. `__autumn_route_info_{name}()` — returns a `Route` (existing behaviour).
//! 2. `__autumn_path_{helper_name}(params…) -> String` — typed path helper
//!    that accepts one `impl Display` argument per `{param}` segment in the URL
//!    and returns the formatted absolute path string.
//!
//! The `helper_name` defaults to the handler function name but can be
//! overridden with the `name = "custom_name"` route attribute argument.
//!
//! [`ApiDoc`]: ../../autumn_web/openapi/struct.ApiDoc.html

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{FnArg, LitStr, ReturnType, Type};

use crate::api_doc;
use crate::idempotency_guard::block_has_replay_guard;
use crate::parse;

/// Core implementation shared by all route macros (`#[get]`, `#[post]`, etc.).
///
/// `http_method` is the uppercase method name (e.g., `"GET"`).
/// `axum_fn` is the lowercase axum routing function name (e.g., `"get"`).
#[allow(clippy::too_many_lines)]
pub fn route_macro(
    http_method: &str,
    axum_fn: &str,
    attr: TokenStream,
    item: TokenStream,
) -> TokenStream {
    let route_args = match parse::parse_route_attr(attr) {
        Ok(a) => a,
        Err(err) => return err,
    };
    let path = route_args.path.clone();

    let (leading_items, mut input_fn) = match parse::parse_async_handler_with_leading_items(item) {
        Ok(v) => v,
        Err(err) => return err,
    };

    // Extract #[intercept(LayerType)] attributes from the handler.
    let interceptors = parse::extract_interceptors(&mut input_fn.attrs);

    // Extract #[api_doc(...)] overrides before emitting the function, so
    // the attribute doesn't leak onto the emitted fn definition and
    // trigger an "unknown attribute" error.
    let api_doc_attr = match api_doc::extract(&mut input_fn.attrs) {
        Ok(v) => v,
        Err(err) => return err,
    };

    // ── `#[edge]` opt-in (#1790) ────────────────────────────────
    // Detected before any code is generated so an ineligible route fails with
    // one purpose-written diagnostic instead of a wall of downstream type
    // errors from the emitted edge companion.
    let edge = crate::edge::detect(&input_fn);
    if let Some(marking) = edge
        && let Some(err) = reject_ineligible_edge_route(
            http_method,
            &input_fn,
            !interceptors.is_empty(),
            marking.span,
        )
    {
        return err;
    }

    let fn_name = &input_fn.sig.ident;
    let route_info_name = format_ident!("__autumn_route_info_{}", fn_name);
    let vis = &input_fn.vis;

    // Determine the path-helper name: use `name = "..."` override when set,
    // otherwise default to the handler function name.
    let helper_ident = route_args.helper_ident(fn_name);
    let path_helper_name = format_ident!("__autumn_path_{}", helper_ident);
    let fn_name_alias = emit_fn_name_alias(
        route_args.name_override.as_ref(),
        fn_name,
        &path_helper_name,
    );

    let method_const = format_ident!("{}", http_method); // e.g., GET
    let routing_fn = format_ident!("{}", axum_fn); // e.g., get

    // When #[feature_flag] is stacked, it prepends a gate parameter of type
    // `__AutumnFlagGate_{handler_name}` to the handler inputs. Since route macros
    // run before attribute macros lower down the chain, we must detect this attribute
    // and manually propagate the gate parameter to the primitive wrapper so that
    // the wrapper's call to the handler compiles.
    let has_feature_flag_attr = input_fn.attrs.iter().any(|a| {
        a.path()
            .segments
            .last()
            .is_some_and(|s| s.ident == "feature_flag")
    });
    // Body guards that rewrite the handler's return type to `Response` (and
    // inject hidden extractors): #[throttle], #[step_up], #[authorize], and
    // #[secured]. This route macro is outermost, so when one of these guards is
    // still present as an attribute it has not expanded yet — the signature we
    // see here still declares the original primitive return type, but the guard
    // will lower it to `Response`. Emitting the primitive `.to_string()` wrapper
    // (which calls the handler with only the user's original args and stringifies
    // the result) would then fail to compile, so skip it and route through the
    // normal `Response` path. (#[feature_flag] does NOT rewrite the return type;
    // it propagates its gate param instead, so the primitive wrapper is kept.)
    let has_response_rewriting_guard = has_throttle_guard(&input_fn)
        || has_step_up_guard(&input_fn)
        || has_secured_attr(&input_fn)
        || has_authorize_attr(&input_fn);
    let primitive_wrapper = if should_stringify_primitive_output(&input_fn.sig.output)
        && !has_response_rewriting_guard
    {
        let wrapper_name = format_ident!("__autumn_primitive_handler_{}", fn_name);
        let mut wrapper_inputs = Vec::new();
        let mut call_args = Vec::new();

        if has_feature_flag_attr {
            let gate_ident = format_ident!("__AutumnFlagGate_{}", fn_name);
            wrapper_inputs.push(quote! { __autumn_gate: #gate_ident });
            call_args.push(quote! { __autumn_gate });
        }

        for (idx, arg) in input_fn.sig.inputs.iter().enumerate() {
            match arg {
                FnArg::Typed(pat_type) => {
                    let arg_name = format_ident!("__autumn_arg_{idx}");
                    let ty = &pat_type.ty;
                    wrapper_inputs.push(quote! { #arg_name: #ty });
                    call_args.push(quote! { #arg_name });
                }
                FnArg::Receiver(receiver) => {
                    return syn::Error::new_spanned(
                        receiver,
                        "Autumn route handlers cannot take a self receiver",
                    )
                    .to_compile_error();
                }
            }
        }

        Some(quote! {
            #[doc(hidden)]
            async fn #wrapper_name(#(#wrapper_inputs),*) -> ::std::string::String {
                #fn_name(#(#call_args),*).await.to_string()
            }
        })
    } else {
        None
    };

    let handler_name = primitive_wrapper.as_ref().map_or_else(
        || fn_name.clone(),
        |_| format_ident!("__autumn_primitive_handler_{}", fn_name),
    );

    // ── Edge lane codegen (#1790) ───────────────────────────────
    // For an edge route every item that mentions `::autumn_web` is gated off
    // the wasm32 target (autumn-web is never compiled for wasm), while the
    // handler itself and the edge companion stay unconditional so one source
    // file serves both the origin binary and the capsule. For every other
    // route both values are empty and the expansion is unchanged.
    let (native_cfg, edge_companion) = emit_edge_items(edge, fn_name, &handler_name, vis, &path);

    // ── OpenAPI metadata ────────────────────────────────────────
    let pools_tokens = emit_pool_slice(&derive_pools(&input_fn));
    let path_params = api_doc::extract_path_params(&path.value());
    let path_params_tokens = api_doc::emit_path_param_slice(&path_params);
    let request_body = api_doc::schema_option(api_doc::infer_request_body(&input_fn));
    let response_body = api_doc::schema_option(api_doc::infer_response_body(&input_fn));
    let query_schema = api_doc::schema_option(api_doc::infer_query_params(&input_fn));
    let (secured, required_roles, required_scopes) = api_doc::extract_secured_info(&input_fn);
    let is_public = api_doc::is_public(&input_fn);
    let has_feature_flag = has_feature_flag_attr || has_expanded_feature_flag_gate(&input_fn);
    let body_guarded_replay = secured
        || has_authorize_guard(&input_fn)
        || has_feature_flag
        || has_step_up_guard(&input_fn)
        || has_throttle_guard(&input_fn);
    let intercepted_route = !interceptors.is_empty();
    let mut handler_expr = build_handler_expr(
        &routing_fn,
        &handler_name,
        &interceptors,
        !body_guarded_replay && !intercepted_route,
    );
    if edge.is_some() {
        // The wire runtime strips SENSITIVE_HEADERS before the capsule
        // dispatches, so the native mount must strip the same set before the
        // handler: otherwise a handler that reads `cookie`/`authorization`
        // through `HeaderMap` would serve different bytes at the origin than
        // at the edge. Route-local — middleware above the router still sees
        // the original request.
        handler_expr = quote! {
            #handler_expr.layer(::autumn_edge::reexports::axum::middleware::map_request(
                ::autumn_edge::strip_request_credentials,
            ))
        };
    }
    let route_idempotency = if intercepted_route {
        quote! { ::autumn_web::RouteIdempotency::Direct }
    } else {
        quote! { ::autumn_web::RouteIdempotency::ReplayThroughInner }
    };
    let api_doc_fields = api_doc_attr.emit_ident_fields(fn_name);
    let http_method_lit = LitStr::new(http_method, Span::call_site());
    let api_version_expr = route_args.api_version.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |lit| quote! { ::core::option::Option::Some(#lit) },
    );
    let sunset_opt_out_val = route_args.sunset_opt_out;
    let route_timeout = match route_args.timeout {
        crate::parse::RouteTimeoutAttr::Inherit => {
            quote! { ::autumn_web::RouteTimeout::Inherit }
        }
        crate::parse::RouteTimeoutAttr::Ms(ms) => {
            quote! {
                ::autumn_web::RouteTimeout::Override(
                    ::core::time::Duration::from_millis(#ms)
                )
            }
        }
        crate::parse::RouteTimeoutAttr::Disabled => {
            quote! { ::autumn_web::RouteTimeout::Disabled }
        }
    };
    let has_policy_val = has_policy_only(&input_fn);
    // Source order is preserved here on purpose: `ApiDoc` stays faithful to the
    // handler as written, and the route listing canonicalizes (sorts/dedupes)
    // when it projects these onto the wire.
    let authorize_bindings =
        api_doc::emit_authorize_binding_slice(&api_doc::extract_authorize_bindings(&input_fn));
    // `#[agent_operable]` names the authority static after the handler, so the
    // route macro only needs to know *whether* the handler is governed — the
    // grant, the proved effects and the const assertions are the analyser's.
    let agent_authority = if api_doc::extract_agent_authority(&input_fn) {
        let authority_static = format_ident!("__AUTUMN_AGENT_AUTHORITY_{}", fn_name);
        quote! { ::core::option::Option::Some(&#authority_static) }
    } else {
        quote! { ::core::option::Option::None }
    };
    let seo_defaults = route_args.seo.emit();

    // ── Path helper ─────────────────────────────────────────────
    let path_helper = emit_path_helper(&path_helper_name, &path, &path_params);
    // The alias re-exports the (possibly gated) helper, so it carries the same
    // gate. An absent alias is an empty token stream — prefixing a `#[cfg]`
    // onto nothing would emit a dangling attribute.
    let fn_name_alias = if fn_name_alias.is_empty() {
        fn_name_alias
    } else {
        quote! { #native_cfg #fn_name_alias }
    };

    quote! {
        // A guard macro that already expanded above this route attribute
        // (#[secured]/#[step_up]/#[throttle], #1668) leaves its hidden
        // `FromRequestParts` gate type here, ahead of the handler — re-emit
        // it verbatim; empty when no such guard expanded first.
        #leading_items

        // ECHO-001: We want to apply #[axum::debug_handler] but without forcing the user
        // to import axum manually. However, the path resolution in Axum macros makes this impossible
        // natively. Custom compile errors handle the type checks.
        #input_fn
        #primitive_wrapper

        #native_cfg
        #[doc(hidden)]
        #vis fn #route_info_name() -> ::autumn_web::Route {
            ::autumn_web::Route {
                method: ::autumn_web::reexports::http::Method::#method_const,
                path: #path,
                handler: #handler_expr,
                name: ::core::stringify!(#fn_name),
                api_version: #api_version_expr,
                sunset_opt_out: #sunset_opt_out_val,
                api_doc: ::autumn_web::openapi::ApiDoc {
                    method: #http_method_lit,
                    path: #path,
                    path_params: #path_params_tokens,
                    request_body: #request_body,
                    response: #response_body,
                    query_schema: #query_schema,
                    secured: #secured,
                    required_roles: #required_roles,
                    required_scopes: #required_scopes,
                    register_schemas: ::core::option::Option::None,
                    api_version: #api_version_expr,
                    sunset_opt_out: #sunset_opt_out_val,
                    has_policy: #has_policy_val,
                    authorize_bindings: #authorize_bindings,
                    pools: #pools_tokens,
                    public: #is_public,
                    module_path: ::core::module_path!(),
                    source_file: ::core::file!(),
                    source_line: ::core::line!(),
                    agent_authority: #agent_authority,
                    #api_doc_fields
                },
                repository: ::core::option::Option::None,
                idempotency: #route_idempotency,
                timeout: #route_timeout,
                seo: #seo_defaults,
            }
        }

        #native_cfg
        #path_helper
        #fn_name_alias

        #edge_companion
    }
}

/// Compile error for `#[edge]` on a route the edge lane cannot serve.
const EDGE_METHOD_ERROR: &str = "`#[edge]` is only supported on `#[get]` routes; \
                                 the edge lane is read-path only (issue #1790)";

/// Compile error for `#[edge]` stacked with an auth or rate guard.
const EDGE_GUARD_ERROR: &str = "`#[edge]` cannot be combined with \
                                `#[secured]`/`#[authorize]`/`#[step_up]`/`#[throttle]` — the edge \
                                capsule has no session or auth state; serve this route from the \
                                origin";

/// Compile error for `#[edge]` stacked with `#[intercept(...)]`.
const EDGE_INTERCEPT_ERROR: &str = "`#[edge]` cannot be combined with `#[intercept(...)]` — \
                                    interceptor layers are origin-only tower middleware and are \
                                    not carried into the edge capsule, so the two lanes would \
                                    serve different bytes; serve this route from the origin";

/// Compile error for an `#[edge]` handler taking `Extension<T>`.
const EDGE_EXTENSION_ERROR: &str = "`#[edge]` handlers cannot take `Extension<...>` — the \
                                    capsule installs no request extensions (the `EdgeCache` seam \
                                    is the one mediated capability), so a missing extension \
                                    would be served as a 500 instead of falling through; use \
                                    `EdgeCache`, or serve this route from the origin";

/// Compile error for `#[edge]` stacked with `#[agent_operable]`.
const EDGE_AGENT_OPERABLE_ERROR: &str = "`#[edge]` cannot be combined with `#[agent_operable(...)]` — the edge lane is \
     read-only, and the capsule has no audit sink or `AppState` to record an agent \
     invocation against, so a governed action would run there unaudited; serve this \
     route from the origin.\n\nServe the route from the origin, or drop \
     `#[agent_operable]`. See docs/guide/agent-authority.md.";

/// Marker consts a guard that expanded *before* this route macro left behind
/// in the handler body instead of an attribute, and missing it would ship a
/// guarded route to the unauthenticated edge lane.
///
/// `#[step_up]`/`#[throttle]` don't appear here: their check moved into a
/// pre-body `FromRequestParts` gate (#1668) and they never left an
/// OpenAPI-readable body marker to begin with, so `has_auth_or_rate_guard`
/// recognizes them via `has_step_up_guard`/`has_throttle_guard`'s own
/// gate-param check instead of this body scan.
const GUARD_MARKERS: &[&str] = &[
    "__AUTUMN_SECURED_ROLES",
    "__AUTUMN_SECURED_SCOPES",
    "__AUTUMN_AUTHORIZE_BINDINGS",
    "__AUTUMN_AGENT_OPERABLE",
];

/// Reject an `#[edge]` route the edge lane cannot serve, spanning the error at
/// the `#[edge]` attribute (or the handler name once it has expanded).
fn reject_ineligible_edge_route(
    http_method: &str,
    input_fn: &syn::ItemFn,
    has_interceptors: bool,
    span: Span,
) -> Option<TokenStream> {
    if http_method != "GET" {
        return Some(syn::Error::new(span, EDGE_METHOD_ERROR).to_compile_error());
    }
    // Checked before the generic auth/rate guard sweep below: the marker is in
    // `GUARD_MARKERS` too (so a governed handler can never slip past that
    // sweep), but its own diagnostic names the real reason — the edge lane is
    // read-only — instead of talking about session state.
    if crate::api_doc::extract_agent_authority(input_fn) {
        return Some(syn::Error::new(span, EDGE_AGENT_OPERABLE_ERROR).to_compile_error());
    }
    if has_auth_or_rate_guard(input_fn) {
        return Some(syn::Error::new(span, EDGE_GUARD_ERROR).to_compile_error());
    }
    // `#[intercept(...)]` attrs were already stripped off `input_fn` by
    // `parse::extract_interceptors`, so the caller passes the extracted list's
    // emptiness instead of this fn re-scanning attributes.
    if has_interceptors {
        return Some(syn::Error::new(span, EDGE_INTERCEPT_ERROR).to_compile_error());
    }
    if has_extension_param(input_fn) {
        return Some(syn::Error::new(span, EDGE_EXTENSION_ERROR).to_compile_error());
    }
    None
}

/// Whether any parameter's type names `Extension` — `Extension<T>`,
/// `axum::Extension<T>`, or nested as `Option<Extension<T>>`.
///
/// `Extension` satisfies axum's `Handler<_, EdgeState>` bound for *any* state,
/// so the type system alone cannot keep it out of the edge lane; this
/// syntactic check is the enforcement point. A type alias that hides the name
/// is not resolved — the same recognition limit every source-level check in
/// this crate documents.
fn has_extension_param(input_fn: &syn::ItemFn) -> bool {
    input_fn.sig.inputs.iter().any(|arg| {
        let syn::FnArg::Typed(pat_type) = arg else {
            return false;
        };
        tokens_contain_ident(&quote! { #pat_type }, "Extension")
    })
}

/// Extractor identifiers that prove a route holds a pooled or external
/// resource for the length of the request, paired with the pool tag recorded
/// in the capacity contract (issue #1733).
///
/// Deliberately narrow: only extractors that *are* a handle on the resource,
/// for the length of the request, are listed. `DeferredDb` is excluded on both
/// counts — it is `pub(crate)`, so no app author can write it, and its whole
/// point is *not* holding a connection across the body read. An extractor that merely happens to consult the database on
/// some paths (`Session`, `Tenant`, `Flags`) is not, because a contract that
/// over-claims is worse than one that under-claims — see the "provable
/// subset" caveat on `RouteInfo::pools`.
const POOL_EXTRACTORS: &[(&str, &str)] = &[
    ("CrossShard", "db"),
    ("Db", "db"),
    ("Events", "events"),
    ("Mailer", "mail"),
    ("Notifications", "notifications"),
    ("Presence", "presence"),
    ("ShardedDb", "db"),
    ("ShardedReadDb", "db"),
    ("Shards", "db"),
];

/// The pool tags a handler's declared extractors prove it touches, sorted and
/// deduplicated so the emitted contract diff is stable.
///
/// Only the parameter *types* are scanned, never the binding names, so a
/// parameter called `events` cannot be mistaken for the `Events` extractor.
/// Like every source-level check in this crate, a type alias that hides the
/// extractor's name is not resolved.
fn derive_pools(input_fn: &syn::ItemFn) -> Vec<&'static str> {
    let mut pools: Vec<&'static str> = Vec::new();
    for arg in &input_fn.sig.inputs {
        let syn::FnArg::Typed(pat_type) = arg else {
            continue;
        };
        let Some(extractor) = declared_extractor(&pat_type.ty) else {
            continue;
        };
        for (name, pool) in POOL_EXTRACTORS {
            if extractor == *name && !pools.contains(pool) {
                pools.push(pool);
            }
        }
    }
    pools.sort_unstable();
    pools
}

/// The name of the extractor a parameter *declares*, or `None` when the type
/// is not a plain path.
///
/// Deliberately NOT a recursive search for a resource name anywhere in the
/// type: `State<AppState<Db>>` declares `State`, not `Db`. Matching nested
/// generic arguments would contradict the documented rule that a pool held
/// through an application `State` value is invisible to this derivation, and
/// would write false `db-bound` shapes — and digest drift — into the contract.
/// Under-claiming is the safe direction here; over-claiming is a lie.
///
/// Transparent wrappers axum itself sees through are peeled, so `Option<Db>`
/// and `&Db` still declare `Db`. Like every source-level check in this crate,
/// a type alias that hides the name is not resolved.
fn declared_extractor(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Reference(inner) => declared_extractor(&inner.elem),
        syn::Type::Paren(inner) => declared_extractor(&inner.elem),
        syn::Type::Group(inner) => declared_extractor(&inner.elem),
        syn::Type::Path(path) => {
            let segment = path.path.segments.last()?;
            if segment.ident == "Option"
                && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
                && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
            {
                return declared_extractor(inner);
            }
            Some(segment.ident.to_string())
        }
        _ => None,
    }
}

/// Emit the derived pool set as a `&'static [&'static str]` literal.
fn emit_pool_slice(pools: &[&'static str]) -> TokenStream {
    if pools.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[#(#pools),*] }
    }
}

/// Whether `stream` contains `needle` as an exact identifier, at any nesting.
fn tokens_contain_ident(stream: &TokenStream, needle: &str) -> bool {
    stream.clone().into_iter().any(|tree| match tree {
        proc_macro2::TokenTree::Ident(ident) => ident == needle,
        proc_macro2::TokenTree::Group(group) => tokens_contain_ident(&group.stream(), needle),
        _ => false,
    })
}

/// Whether an auth or rate guard is declared on the handler, in either
/// attribute order: still a live attribute, or already expanded into the marker
/// const it injects.
fn has_auth_or_rate_guard(input_fn: &syn::ItemFn) -> bool {
    has_secured_attr(input_fn)
        || has_authorize_attr(input_fn)
        || has_step_up_guard(input_fn)
        || has_throttle_guard(input_fn)
        || GUARD_MARKERS
            .iter()
            .any(|marker| crate::edge::stmts_have_marker(&input_fn.block.stmts, marker))
}

/// Build the wasm32 gate and the edge companion for an `#[edge]` route.
///
/// Returns two empty token streams for every other route, which keeps the
/// expansion of a non-edge route byte-identical to what it was before the edge
/// lane existed.
fn emit_edge_items(
    edge: Option<crate::edge::EdgeMarking>,
    fn_name: &proc_macro2::Ident,
    handler_name: &proc_macro2::Ident,
    vis: &syn::Visibility,
    path: &LitStr,
) -> (TokenStream, TokenStream) {
    let Some(marking) = edge else {
        return (TokenStream::new(), TokenStream::new());
    };

    let needs = if marking.needs_kv {
        quote! { &[::autumn_edge::EdgeCapability::Kv] }
    } else {
        quote! { &[] }
    };
    let edge_route_name = format_ident!("__autumn_edge_route_{}", fn_name);

    (
        quote! { #[cfg(not(target_arch = "wasm32"))] },
        quote! {
            #[doc(hidden)]
            #vis fn #edge_route_name() -> ::autumn_edge::EdgeRoute {
                ::autumn_edge::EdgeRoute {
                    method: ::autumn_edge::reexports::http::Method::GET,
                    path: #path,
                    // The same handler axum mounts natively — including the
                    // primitive-output wrapper when there is one. Everything
                    // else the native mount can wrap around a handler
                    // (`#[intercept]` layers, auth guards) is refused for edge
                    // routes by `reject_ineligible_edge_route`, so the two
                    // lanes cannot diverge on how a response is produced.
                    handler: ::autumn_edge::edge_get(#handler_name),
                    name: ::core::stringify!(#fn_name),
                    needs: #needs,
                }
            }
        },
    )
}

/// Build the axum handler expression, applying interceptor layers in reverse
/// attribute order so the first `#[intercept(...)]` is the outermost layer.
fn build_handler_expr(
    routing_fn: &proc_macro2::Ident,
    handler_name: &proc_macro2::Ident,
    interceptors: &[syn::Path],
    include_replay_layer: bool,
) -> TokenStream {
    let mut expr = quote! { ::autumn_web::reexports::axum::routing::#routing_fn(#handler_name) };
    if include_replay_layer {
        expr = quote! {
            ::autumn_web::reexports::axum::routing::MethodRouter::<
                ::autumn_web::AppState, ::core::convert::Infallible
            >::layer(#expr, ::autumn_web::idempotency::IdempotencyReplayLayer)
        };
    }
    for interceptor in interceptors.iter().rev() {
        // Explicit type annotation avoids inference ambiguity with chained .layer() calls.
        expr = quote! {
            ::autumn_web::reexports::axum::routing::MethodRouter::<
                ::autumn_web::AppState, ::core::convert::Infallible
            >::layer(#expr, #interceptor)
        };
    }
    expr
}

fn has_authorize_guard(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "authorize")
    }) || block_has_replay_guard(&input_fn.block)
        || crate::api_doc::has_policy_check_in_stmts(&input_fn.block.stmts)
}

/// Whether `#[step_up]` applies to `input_fn`, in either attribute order: a
/// still-unexpanded `#[step_up]` attribute, or (issue #1668) the
/// `__AutumnStepUpGate_*` pre-body `FromRequestParts` gate parameter it
/// expands into. The gate parameter check matters because `#[step_up]` no
/// longer leaves any recognizable shape in the handler *body* — its check now
/// runs in a sibling gate, before the body ever executes — so `body_guarded_replay`
/// (which must stay true whenever a gate owns replay-serving, to keep the
/// standalone `IdempotencyReplayLayer` from serving a cached response ahead of
/// the gate's check) can no longer rely on a body-shape scan alone.
fn has_step_up_guard(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "step_up")
    }) || crate::param_helpers::has_guard_gate_param_with_prefix(input_fn, "__AutumnStepUpGate_")
}

/// Whether `#[throttle]` applies to `input_fn`. See [`has_step_up_guard`] for
/// why the pre-body gate parameter is checked alongside the attribute.
fn has_throttle_guard(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "throttle")
    }) || crate::param_helpers::has_guard_gate_param_with_prefix(input_fn, "__AutumnThrottleGate_")
}

/// Whether a `#[secured]` attribute is still present on the handler (i.e. it has
/// not expanded yet because this route macro is outermost). Used to disable the
/// primitive-output wrapper, since `#[secured]` rewrites the return type to
/// `Response`.
fn has_secured_attr(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "secured")
    })
}

/// Whether an `#[authorize]` attribute is still present on the handler. Used to
/// disable the primitive-output wrapper, since `#[authorize]` rewrites the
/// return type to `Response`. This is intentionally narrower than
/// [`has_authorize_guard`], which also matches inline policy checks that do not
/// rewrite the return type.
fn has_authorize_attr(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "authorize")
    })
}

fn has_policy_only(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "authorize")
    }) || crate::api_doc::has_policy_check_in_stmts(&input_fn.block.stmts)
}

fn has_expanded_feature_flag_gate(input_fn: &syn::ItemFn) -> bool {
    input_fn.sig.inputs.iter().any(|arg| {
        let FnArg::Typed(pat_type) = arg else {
            return false;
        };
        let Type::Path(type_path) = pat_type.ty.as_ref() else {
            return false;
        };
        let Some(last_segment) = type_path.path.segments.last() else {
            return false;
        };
        last_segment
            .ident
            .to_string()
            .starts_with("__AutumnFlagGate_")
    })
}

/// When a `name = "..."` override is active, emit a `pub use` alias for the
/// handler's own function name so that `paths![fn_name]` resolves alongside
/// the override's `paths![custom_name]`.
fn emit_fn_name_alias(
    name_override: Option<&syn::LitStr>,
    fn_name: &proc_macro2::Ident,
    path_helper_name: &proc_macro2::Ident,
) -> TokenStream {
    let fn_path_helper_name = format_ident!("__autumn_path_{}", fn_name);
    if name_override.is_some() && fn_path_helper_name != *path_helper_name {
        quote! {
            #[doc(hidden)]
            pub use self::#path_helper_name as #fn_path_helper_name;
        }
    } else {
        quote! {}
    }
}

/// Emit the typed path helper function.
///
/// For `/posts/{id}/comments/{comment_id}` this emits:
/// ```ignore
/// pub fn __autumn_path_handler(id: impl Display, comment_id: impl Display) -> String {
///     format!("/posts/{}/comments/{}", id, comment_id)
/// }
/// ```
///
/// Helpers are always emitted as `pub` regardless of handler visibility so
/// that `paths![]` can re-export them without hitting E0364.
///
/// Positional `{}` placeholders are used (rather than named captures) so that
/// route params whose names are Rust keywords — e.g. `/{type}` or `/{match}` —
/// do not produce invalid `format!` invocations. Parameter idents are emitted
/// as raw identifiers (`r#type`) so they are valid in the function signature.
fn emit_path_helper(
    helper_name: &proc_macro2::Ident,
    path: &LitStr,
    params: &[String],
) -> TokenStream {
    // Build parameter idents: strip `*` catch-all prefix, replace `-` → `_`,
    // then emit as raw identifiers so Rust keywords are valid param names.
    let param_idents: Vec<proc_macro2::Ident> = params
        .iter()
        .map(|p| {
            let sanitized = p.trim_start_matches('*').replace('-', "_");
            proc_macro2::Ident::new_raw(&sanitized, proc_macro2::Span::call_site())
        })
        .collect();

    // Build a positional format string: each `{param}` / `{param:regex}` → `{}`.
    // Positional placeholders avoid named-capture errors when param names are
    // Rust keywords (you cannot write `format!("{type}")` in generated code).
    let format_str = positional_format_string(&path.value());
    let format_lit = LitStr::new(&format_str, path.span());
    let encoded_params: Vec<TokenStream> = params
        .iter()
        .zip(param_idents.iter())
        .map(|(param, ident)| {
            if param.starts_with('*') {
                quote! { ::autumn_web::paths::encode_catch_all_param(#ident) }
            } else {
                quote! { ::autumn_web::paths::encode_path_segment(#ident) }
            }
        })
        .collect();

    quote! {
        #[doc(hidden)]
        pub fn #helper_name(#(#param_idents: impl ::std::fmt::Display),*) -> ::std::string::String {
            format!(#format_lit, #(#encoded_params),*)
        }
    }
}

/// Replace every `{...}` placeholder in a route path with `{}` (positional).
///
/// Handles nested braces from regex quantifiers like `{id:[0-9]{1,3}}` by
/// tracking brace depth, so the outer `{...}` is consumed correctly.
/// Escaped braces (`{{` / `}}`) are passed through unchanged as literal
/// format-string escapes representing a single `{` or `}` in the output.
fn positional_format_string(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                // Escaped literal brace `{{` — pass through for format string.
                chars.next();
                result.push_str("{{");
            }
            '{' => {
                // Path parameter — emit positional placeholder and skip contents.
                result.push_str("{}");
                let mut depth: u32 = 1;
                for inner in chars.by_ref() {
                    match inner {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            '}' if chars.peek() == Some(&'}') => {
                // Escaped closing brace `}}` — pass through.
                chars.next();
                result.push_str("}}");
            }
            _ => result.push(c),
        }
    }
    result
}

/// Whether a route handler's declared return type is a bare numeric/bool
/// primitive that Autumn serves by stringifying it (primitives do not implement
/// axum's `IntoResponse`). Shared with the `#[throttle]` macro so a throttled
/// primitive-returning handler stringifies its result the same way the plain
/// primitive-output wrapper does.
pub fn should_stringify_primitive_output(output: &ReturnType) -> bool {
    let ReturnType::Type(_, ty) = output else {
        return false;
    };

    let Type::Path(path) = ty.as_ref() else {
        return false;
    };

    if path.qself.is_some() || path.path.segments.len() != 1 {
        return false;
    }

    let ident = path.path.segments[0].ident.to_string();
    matches!(
        ident.as_str(),
        "bool"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
    )
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::{positional_format_string, route_macro};

    #[test]
    fn positional_plain_params() {
        assert_eq!(positional_format_string("/posts/{id}"), "/posts/{}");
    }

    #[test]
    fn positional_regex_constrained_params() {
        assert_eq!(positional_format_string("/users/{id:[0-9]+}"), "/users/{}");
    }

    #[test]
    fn positional_multiple_params() {
        assert_eq!(
            positional_format_string("/posts/{year}/{slug}"),
            "/posts/{}/{}"
        );
    }

    #[test]
    fn positional_static_path() {
        assert_eq!(positional_format_string("/hello"), "/hello");
    }

    #[test]
    fn positional_catch_all_param() {
        assert_eq!(positional_format_string("/files/{*path}"), "/files/{}");
    }

    #[test]
    fn positional_hyphenated_param() {
        assert_eq!(positional_format_string("/items/{item-id}"), "/items/{}");
    }

    #[test]
    fn positional_regex_with_quantifier_braces() {
        // Regex quantifiers like {1,3} must not end the outer capture early.
        assert_eq!(
            positional_format_string("/users/{id:[0-9]{1,3}}"),
            "/users/{}"
        );
    }

    #[test]
    fn positional_keyword_param() {
        // Keyword params like `type` must produce a valid positional placeholder.
        assert_eq!(positional_format_string("/items/{type}"), "/items/{}");
    }

    #[test]
    fn positional_escaped_braces_pass_through() {
        // `{{` / `}}` are literal braces in the route, not parameters.
        assert_eq!(positional_format_string("/{{hello}}"), "/{{hello}}");
        // Escaped brace followed by a real param.
        assert_eq!(
            positional_format_string("/{{literal}}/{id}"),
            "/{{literal}}/{}"
        );
    }

    #[test]
    fn route_macro_string_literal_replay_guard_still_injects_layer() {
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/items" },
            quote! {
                async fn create_item() -> &'static str {
                    let _ = "__AUTUMN_IDEMPOTENCY_REPLAY_GUARD";
                    "created"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("IdempotencyReplayLayer"),
            "plain handler text must not be mistaken for a generated replay stop: {generated}"
        );
    }

    #[test]
    fn route_macro_interceptor_uses_direct_idempotency() {
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/items" },
            quote! {
                #[intercept(TenantLayer)]
                async fn create_item() -> &'static str {
                    "created"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("RouteIdempotency :: Direct"),
            "intercepted routes must fail closed when replay scope is not explicit: {generated}"
        );
        assert!(
            !generated.contains("IdempotencyReplayLayer"),
            "intercepted routes must not advertise an implicit replay stop: {generated}"
        );
    }

    #[test]
    fn route_macro_suppresses_replay_layer_when_throttle_attribute_present() {
        // Ordering A: `#[post]` outermost, `#[throttle]` still an attribute below
        // it. The route macro detects the attribute (`has_throttle_guard`) and
        // must NOT add the outer IdempotencyReplayLayer — replay handling moves
        // into the (later-expanded) throttle body, after the throttle check.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/items" },
            quote! {
                #[throttle(limit = 1, per = "60s", key = "ip")]
                async fn create_item() -> &'static str { "created" }
            },
        )
        .to_string();

        assert!(
            !generated.contains("IdempotencyReplayLayer"),
            "throttled route (route outermost) must not add the outer replay layer: {generated}"
        );
    }

    #[test]
    fn route_macro_suppresses_replay_layer_for_expanded_throttle_prologue() {
        // Ordering B: `#[throttle]` written ABOVE `#[post]`, so the throttle macro
        // expands FIRST — it removes its own attribute and injects the throttle
        // prologue into the body. The route macro no longer sees a `#[throttle]`
        // attribute and must instead recognize the generated throttle prologue in
        // the body to suppress the outer IdempotencyReplayLayer; otherwise a
        // cached replay would be served before the in-body throttle check.
        let throttled = crate::throttle::throttle_macro(
            quote! { limit = 1, per = "60s", key = "ip" },
            quote! {
                async fn create_item() -> &'static str { "created" }
            },
        );
        let generated = route_macro("POST", "post", quote! { "/items" }, throttled).to_string();

        assert!(
            !generated.contains("IdempotencyReplayLayer"),
            "throttled route (throttle expanded first) must not add the outer replay layer: \
             {generated}"
        );
    }

    // ── OpenAPI response schema survives guard-outermost expansion (#1677) ──
    //
    // All four body guards rewrite `sig.output` to `Response` when they
    // expand. When a guard is written ABOVE the route attribute it expands
    // FIRST, so by the time the route macro runs and calls
    // `infer_response_body`, the real `Json<T>` return type is already gone
    // from `sig.output` — unless the route macro recovers it from the
    // `__autumn_inner: T` binding the guard left behind.

    #[test]
    fn route_macro_infers_response_schema_when_throttle_expands_first() {
        let throttled = crate::throttle::throttle_macro(
            quote! { limit = 5, per = "1m", key = "ip" },
            quote! {
                async fn create() -> ::autumn_web::reexports::axum::Json<Created> { todo!() }
            },
        );
        let generated = route_macro("POST", "post", quote! { "/users" }, throttled).to_string();

        assert!(
            generated.contains("response : :: core :: option :: Option :: Some")
                && generated.contains("\"Created\""),
            "a #[throttle]-above-#[post] route must still document its Json<Created> response: \
             {generated}"
        );
    }

    #[test]
    fn route_macro_infers_response_schema_when_secured_expands_first() {
        let secured = crate::secured::secured_macro(
            quote! { "admin" },
            quote! {
                async fn create() -> ::autumn_web::reexports::axum::Json<Created> { todo!() }
            },
        );
        let generated = route_macro("POST", "post", quote! { "/users" }, secured).to_string();

        assert!(
            generated.contains("response : :: core :: option :: Option :: Some")
                && generated.contains("\"Created\""),
            "a #[secured]-above-#[post] route must still document its Json<Created> response: \
             {generated}"
        );
    }

    #[test]
    fn route_macro_infers_response_schema_when_step_up_expands_first() {
        let stepped_up = crate::step_up::step_up_macro(
            quote! {},
            quote! {
                async fn create() -> ::autumn_web::reexports::axum::Json<Created> { todo!() }
            },
        );
        let generated = route_macro("POST", "post", quote! { "/users" }, stepped_up).to_string();

        assert!(
            generated.contains("response : :: core :: option :: Option :: Some")
                && generated.contains("\"Created\""),
            "a #[step_up]-above-#[post] route must still document its Json<Created> response: \
             {generated}"
        );
    }

    #[test]
    fn route_macro_infers_response_schema_when_authorize_expands_first() {
        let authorized = crate::authorize::authorize_macro(
            quote! { "update", resource = Note },
            quote! {
                async fn update_note(note: Note) -> ::autumn_web::reexports::axum::Json<Created> {
                    todo!()
                }
            },
        );
        let generated =
            route_macro("POST", "post", quote! { "/notes/{id}" }, authorized).to_string();

        assert!(
            generated.contains("response : :: core :: option :: Option :: Some")
                && generated.contains("\"Created\""),
            "an #[authorize]-above-#[post] route must still document its Json<Created> response: \
             {generated}"
        );
    }

    #[test]
    fn route_macro_infers_response_schema_under_stacked_guards_above_route() {
        // `#[secured]` above `#[throttle]` above `#[post]`: throttle expands
        // first and captures the real type, then secured wraps throttle's
        // whole generated body one level deeper. Only the innermost
        // `__autumn_inner` binding carries `Json<Created>` — the outer one
        // reads `Response`.
        let throttled = crate::throttle::throttle_macro(
            quote! { limit = 5, per = "1m", key = "ip" },
            quote! {
                async fn create() -> ::autumn_web::reexports::axum::Json<Created> { todo!() }
            },
        );
        let secured = crate::secured::secured_macro(quote! { "admin" }, throttled);
        let generated = route_macro("POST", "post", quote! { "/users" }, secured).to_string();

        assert!(
            generated.contains("response : :: core :: option :: Option :: Some")
                && generated.contains("\"Created\""),
            "stacked guards above the route macro must not drop the response schema: {generated}"
        );
    }

    #[test]
    fn route_macro_parses_api_version_and_sunset_opt_out() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/items", api_version = "v1", sunset_opt_out = true },
            quote! {
                async fn get_items() -> &'static str {
                    "items"
                }
            },
        )
        .to_string();

        // Check that api_version and sunset_opt_out are generated in Route constructor
        assert!(
            generated.contains("api_version"),
            "should generate api_version field: {generated}"
        );
        assert!(
            generated.contains("sunset_opt_out"),
            "should generate sunset_opt_out field: {generated}"
        );
    }

    #[test]
    fn route_macro_defaults_timeout_to_inherit() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/items" },
            quote! {
                async fn get_items() -> &'static str { "items" }
            },
        )
        .to_string();

        assert!(
            generated.contains("RouteTimeout :: Inherit"),
            "routes without a timeout attribute must inherit the global deadline: {generated}"
        );
    }

    #[test]
    fn route_macro_parses_timeout_ms_override() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/export", timeout_ms = 120000 },
            quote! {
                async fn export() -> &'static str { "report" }
            },
        )
        .to_string();

        assert!(
            generated.contains("RouteTimeout :: Override"),
            "timeout_ms must emit a RouteTimeout::Override: {generated}"
        );
        assert!(
            generated.contains("from_millis") && generated.contains("120000"),
            "override must carry the configured millisecond budget: {generated}"
        );
    }

    #[test]
    fn route_macro_defaults_public_false() {
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/widgets" },
            quote! { async fn create_widget() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(
            generated.contains("public : false"),
            "an unannotated route must record public = false: {generated}"
        );
        // The handler's module path is captured for audit diagnostics.
        assert!(generated.contains("module_path"));
    }

    #[test]
    fn route_macro_marks_public_when_public_attribute_present() {
        // Ordering A: `#[post]` outermost, `#[public]` still an attribute below.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/pricing" },
            quote! {
                #[public]
                async fn pricing() -> &'static str { "free" }
            },
        )
        .to_string();
        assert!(
            generated.contains("public : true"),
            "a #[public] handler must record public = true: {generated}"
        );
    }

    #[test]
    fn route_macro_marks_public_from_expanded_marker() {
        // Ordering B: `#[public]` written above `#[post]`, so it expands first and
        // injects the `__AUTUMN_PUBLIC` marker into the body; the route macro must
        // recognize the marker.
        let public_fn = crate::public::public_macro(
            quote! {},
            quote! { async fn pricing() -> &'static str { "free" } },
        );
        let generated = route_macro("POST", "post", quote! { "/pricing" }, public_fn).to_string();
        assert!(
            generated.contains("public : true"),
            "a route macro over an expanded #[public] marker must record public = true: {generated}"
        );
    }

    // ── seo(...) route-level defaults (#1182) ───────────────────────────────

    #[test]
    fn route_macro_defaults_seo_to_empty() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about" },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("SeoRouteDefaults :: EMPTY"),
            "a route without seo(...) must record the empty defaults: {generated}"
        );
    }

    #[test]
    fn route_macro_emits_declared_seo_defaults() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(title = "About", description = "Learn about us") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("with_title (\"About\")"),
            "seo(title = ...) must populate the title default: {generated}"
        );
        assert!(
            generated.contains("with_description (\"Learn about us\")"),
            "seo(description = ...) must populate the description default: {generated}"
        );
        assert!(
            generated.contains("SeoRouteDefaults :: EMPTY . with_title"),
            "unset seo keys must fall back to the empty defaults: {generated}"
        );
    }

    #[test]
    fn route_macro_seo_composes_with_other_keys() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(title = "About"), name = "about_page", timeout_ms = 500 },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("with_title (\"About\")"),
            "seo(...) must parse when it precedes other keys: {generated}"
        );
        assert!(
            generated.contains("RouteTimeout :: Override"),
            "keys after seo(...) must still parse: {generated}"
        );
        assert!(
            generated.contains("__autumn_path_about_page"),
            "name override after seo(...) must still apply: {generated}"
        );
    }

    #[test]
    fn route_macro_never_emits_a_seo_struct_literal() {
        // The expansion lands in the *user's* crate, so a struct literal there
        // would pin `SeoRouteDefaults` as exhaustively constructible forever —
        // a fourteenth SEO key could then never be added without a breaking
        // change — and would trip `clippy::needless_update` once every key is
        // spelled out. Chained `const fn` setters avoid both.
        let generated = route_macro(
            "GET",
            "get",
            quote! {
                "/full",
                seo(
                    title = "t",
                    description = "d",
                    canonical = "c",
                    og_title = "ot",
                    og_description = "od",
                    og_image = "oi",
                    og_type = "oty",
                    og_url = "ou",
                    twitter_card = "tc",
                    twitter_title = "tt",
                    twitter_description = "td",
                    twitter_image = "ti",
                    robots = "noindex"
                )
            },
            quote! { async fn full() -> &'static str { "full" } },
        )
        .to_string();

        assert!(
            generated.contains("with_robots (\"noindex\")"),
            "every declared key must still be emitted: {generated}"
        );
        assert!(
            !generated.contains("SeoRouteDefaults {"),
            "the expansion must not construct SeoRouteDefaults by struct literal: {generated}"
        );
        // Scoped to the emitted `seo:` value. The surrounding expansion
        // legitimately contains `..Default::default()` in other literals, so
        // asserting over the whole token stream would be a trap for future
        // edits (the `#[static_get]` expansion already trips it).
        let seo_value = generated
            .split("seo : ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .expect("expansion should assign the seo field");
        assert!(
            seo_value.contains("SeoRouteDefaults :: EMPTY"),
            "sanity: seo value should build from EMPTY: {seo_value}"
        );
        assert!(
            !seo_value.contains(".."),
            "the seo value must not use struct-update syntax: {seo_value}"
        );
    }

    #[test]
    fn route_macro_rejects_empty_seo_group() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo() },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "an empty seo() must be a compile error, not a silent no-op: {generated}"
        );
    }

    #[test]
    fn route_macro_rejects_repeated_seo_argument() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(title = "A"), seo(og_type = "website") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a second seo(...) argument must be a compile error: {generated}"
        );
    }

    #[test]
    fn route_macro_rejects_seo_keys_without_separating_comma() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(title = "A" og_type = "website") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a missing comma must be rejected rather than dropping later keys: {generated}"
        );
    }

    #[test]
    fn route_macro_rejects_unknown_seo_key() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(titel = "About") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "an unknown seo key must be a compile error: {generated}"
        );
        assert!(
            generated.contains("titel"),
            "the error must name the offending key: {generated}"
        );
    }

    #[test]
    fn route_macro_rejects_duplicate_seo_key() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(title = "A", title = "B") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a duplicate seo key must be a compile error: {generated}"
        );
    }

    #[test]
    fn route_macro_rejects_non_string_seo_value() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo(title = 42) },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a non-string seo value must be a compile error: {generated}"
        );
    }

    #[test]
    fn route_macro_rejects_seo_without_parentheses() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about", seo = "title" },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "`seo = ...` must be rejected in favour of `seo(...)`: {generated}"
        );
    }

    #[test]
    fn route_macro_parses_timeout_off_disabled() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/stream", timeout = "off" },
            quote! {
                async fn stream() -> &'static str { "data" }
            },
        )
        .to_string();

        assert!(
            generated.contains("RouteTimeout :: Disabled"),
            "timeout = \"off\" must emit RouteTimeout::Disabled: {generated}"
        );
    }

    // ── #[authorize] bindings recorded in ApiDoc (#1627) ────────────────────

    #[test]
    fn route_macro_emits_empty_authorize_bindings_by_default() {
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/widgets" },
            quote! { async fn create_widget() -> &'static str { "ok" } },
        )
        .to_string();

        assert!(
            generated.contains("authorize_bindings : & []"),
            "an unguarded route must record no authorization bindings: {generated}"
        );
    }

    #[test]
    fn route_macro_records_authorize_binding_when_attribute_present() {
        // Ordering A: `#[post]` outermost, `#[authorize]` still an attribute
        // below it, so the route macro reads the arguments straight off the
        // unexpanded attribute.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/notes/{id}" },
            quote! {
                #[authorize("update", resource = Note)]
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        )
        .to_string();

        assert!(
            generated.contains(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#),
            "an #[authorize] attribute must record its (action, resource) binding: {generated}"
        );
    }

    #[test]
    fn route_macro_records_authorize_binding_from_expanded_marker() {
        // Ordering B: `#[authorize]` written ABOVE `#[post]`, so it expands
        // first, removes its own attribute and leaves only the generated body.
        // The binding must survive in the marker const it injects.
        let authorized = crate::authorize::authorize_macro(
            quote! { "update", resource = Note },
            quote! {
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        );
        let generated =
            route_macro("POST", "post", quote! { "/notes/{id}" }, authorized).to_string();

        assert!(
            generated.contains(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#),
            "an already-expanded #[authorize] must still record its binding: {generated}"
        );
    }

    #[test]
    fn route_macro_records_authorize_binding_under_secured_wrapper() {
        // `#[authorize]` above `#[secured]` above `#[post]`: authorize expands
        // first, then secured wraps that whole body in
        // `(async move { … }).await`, burying the marker one level down. The
        // walk must descend the generated wrapper instead of stopping at the
        // outermost statement list.
        let authorized = crate::authorize::authorize_macro(
            quote! { "update", resource = Note },
            quote! {
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        );
        let secured = crate::secured::secured_macro(quote! { "admin" }, authorized);
        let secured_fn = crate::param_helpers::extract_fn_item(secured, "update_note");
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/notes/{id}" },
            quote! { #secured_fn },
        )
        .to_string();

        assert!(
            generated.contains(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#),
            "a binding nested inside a #[secured] wrapper must not be lost: {generated}"
        );
    }

    #[test]
    fn route_macro_records_all_bindings_for_stacked_authorize_attributes() {
        // Every attribute contributes: the existing `policy` boolean collapses
        // N bindings into one flag, so the list must not do the same.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/notes/{id}" },
            quote! {
                #[authorize("update", resource = Note)]
                #[authorize("publish", resource = Note)]
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        )
        .to_string();

        let update = generated
            .find(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#)
            .unwrap_or_else(|| {
                panic!("the first stacked #[authorize] must record a binding: {generated}")
            });
        let publish = generated
            .find(r#"AuthorizeBinding { action : "publish" , resource : "Note" }"#)
            .unwrap_or_else(|| {
                panic!("the second stacked #[authorize] must record a binding: {generated}")
            });
        assert!(
            update < publish,
            "stacked bindings must be recorded in source order: {generated}"
        );
    }

    #[test]
    fn route_macro_preserves_secured_roles_when_authorize_wraps_them() {
        // `#[secured("admin")]` above `#[authorize]` above `#[post]`: secured
        // expands first and leaves its role marker in the body; authorize then
        // wraps that body in `let __autumn_inner: T = (async move { … }).await;`.
        // The roles walk must descend that generated wrapper — otherwise the
        // route silently reports `required_roles: &[]` and a `provable` manifest
        // dimension under-states the posture.
        let secured = crate::secured::secured_macro(
            quote! { "admin" },
            quote! {
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        );
        let secured_fn = crate::param_helpers::extract_fn_item(secured, "update_note");
        let authorized = crate::authorize::authorize_macro(
            quote! { "update", resource = Note },
            quote! { #secured_fn },
        );
        let generated =
            route_macro("POST", "post", quote! { "/notes/{id}" }, authorized).to_string();

        assert!(
            generated.contains(r#"required_roles : & ["admin"]"#),
            "roles from a #[secured] buried under an #[authorize] wrapper must survive: \
             {generated}"
        );
        assert!(
            generated.contains(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#),
            "…and the authorize binding must be recorded alongside them: {generated}"
        );
    }

    #[test]
    fn route_macro_preserves_secured_roles_when_authorize_is_still_an_attribute() {
        // The remaining ordering of {secured, route, authorize}: `#[secured]`
        // ABOVE `#[post]` (already expanded into markers) with `#[authorize]`
        // BELOW it (still a live attribute). The marker read must not be
        // short-circuited by the live-authorize fallback — both idioms are the
        // documented house style, and losing the roles here means deleting the
        // `#[secured(...)]` line produces zero manifest diff on a `provable`
        // dimension.
        let secured = crate::secured::secured_macro(
            quote! { "admin", scopes = ["notes:write"] },
            quote! {
                #[authorize("update", resource = Note)]
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        );
        let secured_fn = crate::param_helpers::extract_fn_item(secured, "update_note");
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/notes/{id}" },
            quote! { #secured_fn },
        )
        .to_string();

        assert!(
            generated.contains(r#"required_roles : & ["admin"]"#),
            "roles from an expanded #[secured] must survive a live #[authorize] attribute: \
             {generated}"
        );
        assert!(
            generated.contains(r#"required_scopes : & ["notes:write"]"#),
            "scopes must survive alongside the roles: {generated}"
        );
        assert!(
            generated.contains(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#),
            "…and the authorize binding must be recorded alongside them: {generated}"
        );
    }

    #[test]
    fn route_macro_orders_marker_binding_before_live_attribute() {
        // Mixed arrangement: `#[authorize(A)]` ABOVE `#[post]` (already
        // expanded into a marker by the time the route macro runs) and
        // `#[authorize(B)]` BELOW it (still a live attribute). A is higher in
        // the source than B, so the recorded order must be [A, B] — the marker
        // before the attribute, not the collection-mechanism order.
        let authorized = crate::authorize::authorize_macro(
            quote! { "update", resource = Note },
            quote! {
                async fn update_note(note: Note) -> &'static str { "ok" }
            },
        );
        let mixed = quote! {
            #[authorize("publish", resource = Note)]
            #authorized
        };
        let generated = route_macro("POST", "post", quote! { "/notes/{id}" }, mixed).to_string();

        let update = generated
            .find(r#"AuthorizeBinding { action : "update" , resource : "Note" }"#)
            .unwrap_or_else(|| panic!("the marker binding must be recorded: {generated}"));
        let publish = generated
            .find(r#"AuthorizeBinding { action : "publish" , resource : "Note" }"#)
            .unwrap_or_else(|| panic!("the live-attribute binding must be recorded: {generated}"));
        assert!(
            update < publish,
            "the marker comes from the attribute above the route macro, so it precedes the \
             live attribute below it in source order: {generated}"
        );
    }

    // ── `#[edge]` opt-in (#1790) ────────────────────────────────────────────

    /// The edge companion is emitted last, so everything from its `fn` keyword
    /// to the end of the stream *is* the companion. Comparing that slice across
    /// two attribute stackings proves detection produced the same route, not
    /// merely that both stackings emitted *something*.
    fn edge_companion_fragment(generated: &str) -> &str {
        let start = generated
            .find("fn __autumn_edge_route_")
            .unwrap_or_else(|| panic!("expansion should emit an edge companion: {generated}"));
        &generated[start..]
    }

    #[test]
    fn route_macro_edge_get_emits_edge_companion() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/greet" },
            quote! {
                #[edge]
                async fn greet() -> &'static str { "hi" }
            },
        )
        .to_string();

        assert!(
            generated.contains("fn __autumn_edge_route_greet () -> :: autumn_edge :: EdgeRoute"),
            "an #[edge] GET route must emit the edge companion: {generated}"
        );
        assert!(
            generated.contains(":: autumn_edge :: edge_get (greet)"),
            "the edge companion must adapt the handler through edge_get: {generated}"
        );
        assert!(
            generated.contains(":: autumn_edge :: reexports :: http :: Method :: GET"),
            "the edge companion must carry the GET method: {generated}"
        );
        assert!(
            generated.contains("path : \"/greet\""),
            "the edge companion must carry the route path: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_companion_mounts_the_primitive_wrapper() {
        // A primitive-returning handler is mounted natively through a
        // `.to_string()` wrapper (primitives are not `IntoResponse`). The edge
        // companion must mount the *same* wrapper, or the two lanes would
        // disagree about what the response body is — and `edge_get(stats)`
        // would not even compile.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/stats" },
            quote! {
                #[edge]
                async fn stats() -> usize { 42 }
            },
        )
        .to_string();

        assert!(
            generated.contains(":: autumn_edge :: edge_get (__autumn_primitive_handler_stats)"),
            "the edge companion must mount the primitive wrapper: {generated}"
        );
        // …and that wrapper is pure std, so it stays available on wasm32.
        let wrapper = generated
            .find("async fn __autumn_primitive_handler_stats")
            .unwrap_or_else(|| panic!("the primitive wrapper must be emitted: {generated}"));
        assert!(
            !generated[..wrapper].contains("cfg"),
            "the primitive wrapper references no autumn_web item, so it is not gated: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_cfg_gates_native_companions() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/posts/{id}" },
            quote! {
                #[edge]
                async fn show(id: Path<String>) -> &'static str { "post" }
            },
        )
        .to_string();

        // Native companions reference `::autumn_web`, which never compiles for
        // wasm32 — they must be gated off the edge target.
        let gate = "# [cfg (not (target_arch = \"wasm32\"))]";
        assert!(
            generated.contains(&format!(
                "{gate} # [doc (hidden)] fn __autumn_route_info_show"
            )),
            "the route-info companion must be gated off wasm32: {generated}"
        );
        assert!(
            generated.contains(&format!(
                "{gate} # [doc (hidden)] pub fn __autumn_path_show"
            )),
            "the path helper must be gated too — it calls ::autumn_web::paths: {generated}"
        );
        assert_eq!(
            generated.matches(gate).count(),
            2,
            "exactly the two native companions are gated: {generated}"
        );
        // The handler itself and the edge companion stay unconditional.
        assert!(
            !edge_companion_fragment(&generated).contains("cfg"),
            "the edge companion must not be gated: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_gates_the_path_helper_alias() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/greet", name = "greeting" },
            quote! {
                #[edge]
                async fn greet() -> &'static str { "hi" }
            },
        )
        .to_string();

        assert!(
            generated.contains(
                "# [cfg (not (target_arch = \"wasm32\"))] # [doc (hidden)] \
                 pub use self :: __autumn_path_greeting as __autumn_path_greet ;"
            ),
            "the alias re-exports a gated helper, so it must be gated too: {generated}"
        );
        assert_eq!(
            generated
                .matches("# [cfg (not (target_arch = \"wasm32\"))]")
                .count(),
            3,
            "route info + path helper + alias are all native-only: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_detection_is_identical_for_both_attribute_orders() {
        // Ordering A: `#[get]` outermost, `#[edge]` still a live attribute.
        let below = route_macro(
            "GET",
            "get",
            quote! { "/greet" },
            quote! {
                #[edge]
                async fn greet() -> &'static str { "hi" }
            },
        )
        .to_string();

        // Ordering B: `#[edge]` above `#[get]`, so it expanded first and left
        // only the `__AUTUMN_EDGE` marker in the body.
        let edged = crate::edge::edge_macro(
            quote! {},
            quote! { async fn greet() -> &'static str { "hi" } },
        );
        let above = route_macro("GET", "get", quote! { "/greet" }, edged).to_string();

        assert_eq!(
            edge_companion_fragment(&below),
            edge_companion_fragment(&above),
            "both stacking orders must produce the same edge route"
        );
    }

    #[test]
    fn route_macro_edge_needs_kv_populates_the_needs_slice() {
        let edged = crate::edge::edge_macro(
            quote! { needs(kv) },
            quote! { async fn note() -> &'static str { "note" } },
        );
        let generated = route_macro("GET", "get", quote! { "/note" }, edged).to_string();

        assert!(
            generated.contains("needs : & [:: autumn_edge :: EdgeCapability :: Kv]"),
            "needs(kv) must declare the Kv capability on the edge route: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_needs_kv_is_read_from_a_live_attribute_too() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/note" },
            quote! {
                #[edge(needs(kv))]
                async fn note() -> &'static str { "note" }
            },
        )
        .to_string();

        assert!(
            generated.contains("needs : & [:: autumn_edge :: EdgeCapability :: Kv]"),
            "a live #[edge(needs(kv))] attribute must declare the capability: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_without_needs_declares_no_capabilities() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/greet" },
            quote! {
                #[edge]
                async fn greet() -> &'static str { "hi" }
            },
        )
        .to_string();

        assert!(
            generated.contains("needs : & []"),
            "a bare #[edge] route must declare no capabilities: {generated}"
        );
    }

    #[test]
    fn route_macro_non_edge_expansion_is_untouched() {
        // The edge work must be inert for every route that did not opt in:
        // no cfg attribute, no edge companion, nothing.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/posts/{id}", name = "post_page" },
            quote! {
                async fn show(id: Path<String>) -> &'static str { "post" }
            },
        )
        .to_string();

        assert!(
            !generated.contains("cfg"),
            "a non-edge route must not gain a cfg attribute: {generated}"
        );
        assert!(
            !generated.contains("__autumn_edge_route_"),
            "a non-edge route must not gain an edge companion: {generated}"
        );
        assert!(
            !generated.contains("autumn_edge"),
            "a non-edge route must not reference autumn_edge at all: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_on_post_is_a_compile_error() {
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/items" },
            quote! {
                #[edge]
                async fn create_item() -> &'static str { "created" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] on a write-path route must be rejected: {generated}"
        );
        assert!(
            generated.contains("only supported on"),
            "the error must say the edge lane is GET-only: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_secured_attribute_is_a_compile_error() {
        // Ordering A: both guards still live attributes below `#[get]`.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/dashboard" },
            quote! {
                #[edge]
                #[secured]
                async fn dashboard() -> &'static str { "dash" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] + #[secured] must be rejected: {generated}"
        );
        assert!(
            generated.contains("no session or auth state"),
            "the error must explain why the edge lane cannot authenticate: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_expanded_secured_marker_is_a_compile_error() {
        // Ordering B: `#[secured]` above `#[get]` (already expanded into
        // markers), `#[edge]` still a live attribute below it.
        let secured = crate::secured::secured_macro(
            quote! { "admin" },
            quote! {
                #[edge]
                async fn dashboard() -> &'static str { "dash" }
            },
        );
        let generated = route_macro("GET", "get", quote! { "/dashboard" }, secured).to_string();

        assert!(
            generated.contains("compile_error"),
            "an already-expanded #[secured] must still be detected: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_marker_buried_under_secured_wrapper_is_a_compile_error() {
        // `#[edge]` above `#[secured]` above `#[get]`: edge expands first, then
        // secured buries its marker inside `(async move { … }).await`. Losing
        // the marker here would silently ship an authenticated route to the
        // edge lane.
        let edged = crate::edge::edge_macro(
            quote! {},
            quote! { async fn dashboard() -> &'static str { "dash" } },
        );
        let secured = crate::secured::secured_macro(quote! { "admin" }, edged);
        let generated = route_macro("GET", "get", quote! { "/dashboard" }, secured).to_string();

        assert!(
            generated.contains("compile_error"),
            "a buried #[edge] marker must still be detected next to #[secured]: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_intercept_is_a_compile_error() {
        // `#[intercept]` layers wrap only the native mount; the edge companion
        // mounts the bare handler, so allowing the pair would let the two
        // lanes serve different bytes. Refused whichever side of `#[get]` the
        // interceptor sits on.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/stamped" },
            quote! {
                #[edge]
                #[intercept(StampLayer)]
                async fn stamped() -> &'static str { "stamped" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] + #[intercept] must be rejected: {generated}"
        );
        assert!(
            generated.contains("origin-only tower middleware"),
            "the error must explain that interceptors do not cross into the capsule: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_marker_with_intercept_is_a_compile_error() {
        // Ordering B: `#[edge]` above `#[get]` (already expanded into its
        // marker const), `#[intercept]` still a live attribute.
        let edged = crate::edge::edge_macro(
            quote! {},
            quote! {
                #[intercept(StampLayer)]
                async fn stamped() -> &'static str { "stamped" }
            },
        );
        let generated = route_macro("GET", "get", quote! { "/stamped" }, edged).to_string();

        assert!(
            generated.contains("compile_error"),
            "an expanded #[edge] marker + live #[intercept] must be rejected: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_native_mount_strips_request_credentials() {
        // The wire runtime strips SENSITIVE_HEADERS before capsule dispatch;
        // the native mount must strip the same set so a HeaderMap-reading
        // handler observes identical headers on both substrates.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/greet/{name}" },
            quote! {
                #[edge]
                async fn greet(Path(name): Path<String>) -> String { name }
            },
        )
        .to_string();

        assert!(
            generated.contains("strip_request_credentials"),
            "the edge route's native mount must carry the credential strip: {generated}"
        );

        let plain = route_macro(
            "GET",
            "get",
            quote! { "/greet/{name}" },
            quote! {
                async fn greet(Path(name): Path<String>) -> String { name }
            },
        )
        .to_string();
        assert!(
            !plain.contains("strip_request_credentials"),
            "a non-edge route must not be rewritten: {plain}"
        );
    }

    #[test]
    fn route_macro_edge_with_extension_param_is_a_compile_error() {
        // `Extension<T>` satisfies the Handler bound for any state, so the
        // type system cannot keep it out of the capsule; the macro must.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/tenant" },
            quote! {
                #[edge]
                async fn tenant(ext: Extension<TenantConfig>) -> String { ext.name() }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] + Extension<T> must be rejected: {generated}"
        );
        assert!(
            generated.contains("installs no request extensions"),
            "the error must explain the missing-extension 500 hazard: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_nested_extension_param_is_a_compile_error() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/tenant" },
            quote! {
                #[edge]
                async fn tenant(ext: Option<axum::Extension<TenantConfig>>) -> String { greet() }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a nested Extension extractor must be rejected: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_cache_param_is_not_mistaken_for_an_extension() {
        // EdgeCache is delivered through an extension internally, but the
        // parameter type the author writes never names `Extension`.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/note/{key}" },
            quote! {
                #[edge(needs(kv))]
                async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
                    cache.get_string(&key).unwrap_or_default()
                }
            },
        )
        .to_string();

        assert!(
            !generated.contains("compile_error"),
            "EdgeCache must stay accepted: {generated}"
        );
    }

    #[test]
    fn route_macro_intercept_without_edge_is_unaffected() {
        // The refusal must not leak into the ordinary interceptor path.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/stamped" },
            quote! {
                #[intercept(StampLayer)]
                async fn stamped() -> &'static str { "stamped" }
            },
        )
        .to_string();

        assert!(
            !generated.contains("compile_error"),
            "#[intercept] without #[edge] must keep compiling: {generated}"
        );
        assert!(
            generated.contains("StampLayer"),
            "the interceptor layer must still wrap the native mount: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_throttle_is_a_compile_error() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/search" },
            quote! {
                #[edge]
                #[throttle(limit = 1, per = "60s", key = "ip")]
                async fn search() -> &'static str { "results" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] + #[throttle] must be rejected: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_expanded_throttle_marker_is_a_compile_error() {
        let throttled = crate::throttle::throttle_macro(
            quote! { limit = 1, per = "60s", key = "ip" },
            quote! {
                #[edge]
                async fn search() -> &'static str { "results" }
            },
        );
        let generated = route_macro("GET", "get", quote! { "/search" }, throttled).to_string();

        assert!(
            generated.contains("compile_error"),
            "an already-expanded #[throttle] must still be detected: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_step_up_is_a_compile_error() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/vault" },
            quote! {
                #[edge]
                #[step_up]
                async fn vault() -> &'static str { "secrets" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] + #[step_up] must be rejected: {generated}"
        );
    }

    #[test]
    fn route_macro_edge_with_authorize_is_a_compile_error() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/notes/{id}" },
            quote! {
                #[edge]
                #[authorize("read", resource = Note)]
                async fn show_note(note: Note) -> &'static str { "note" }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] + #[authorize] must be rejected: {generated}"
        );
    }

    #[test]
    fn route_macro_string_literal_edge_marker_is_not_an_edge_route() {
        // Handler *text* that merely spells the marker const must not opt the
        // route into the edge lane: the marker is decoded structurally.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/greet" },
            quote! {
                async fn greet() -> &'static str {
                    let _ = "const __AUTUMN_EDGE: () = ();";
                    "hi"
                }
            },
        )
        .to_string();

        assert!(
            !generated.contains("__autumn_edge_route_"),
            "a string literal must not be mistaken for the edge marker: {generated}"
        );
    }

    #[test]
    fn route_macro_string_literal_authorize_marker_is_not_a_binding() {
        // Handler *text* that merely spells the marker const must not be
        // mistaken for one: the marker is decoded structurally, never scanned
        // for as a string.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/items" },
            quote! {
                async fn create_item() -> &'static str {
                    let _ = "const __AUTUMN_AUTHORIZE_BINDINGS: &[(&str, &str)] = &[(\"update\", \"Note\")];";
                    "created"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("authorize_bindings : & []"),
            "a string literal must not be mistaken for a generated binding marker: {generated}"
        );
    }

    // ── `#[agent_operable]` recorded in ApiDoc (#1691) ──────────────────────
    //
    // The route macro must fill `ApiDoc::agent_authority` from EITHER stacking
    // order, exactly like `#[secured]`/`#[authorize]`/`#[edge]`: the attribute
    // when it is still live below the route macro, the body marker const when
    // `#[agent_operable]` expanded above it and deleted its own attribute.
    // Losing it in one order would silently ship a governed handler as an
    // *ungoverned* MCP tool — audited with `reversibility=unknown` and missing
    // from the authority manifest's `actions` — with no diff anywhere.

    /// The `Some(&…)` initializer the route macro must emit for a handler
    /// governed by `#[agent_operable(grant = RefundDrafter)]`.
    const DRAFT_REFUND_AUTHORITY: &str = "agent_authority : :: core :: option :: Option :: Some (& __AUTUMN_AGENT_AUTHORITY_draft_refund)";

    #[test]
    fn route_macro_sets_agent_authority_from_live_attribute() {
        // Ordering A: `#[post]` outermost, `#[agent_operable]` still an
        // attribute below it, so the route macro reads it straight off the
        // unexpanded attribute.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/refunds" },
            quote! {
                #[agent_operable(grant = RefundDrafter)]
                async fn draft_refund() -> &'static str { "drafted" }
            },
        )
        .to_string();

        assert!(
            generated.contains(DRAFT_REFUND_AUTHORITY),
            "a live #[agent_operable] attribute must point ApiDoc at the handler's \
             authority static: {generated}"
        );
    }

    #[test]
    fn route_macro_sets_agent_authority_from_body_marker() {
        // Ordering B: `#[agent_operable]` written ABOVE `#[post]`, so it
        // expanded first, removed its own attribute and left only the marker
        // const in the body. The marker names the grant, and the authority
        // static is named after the handler either way.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/refunds" },
            quote! {
                async fn draft_refund() -> &'static str {
                    #[allow(dead_code)]
                    const __AUTUMN_AGENT_OPERABLE: &str = "RefundDrafter";
                    "drafted"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains(DRAFT_REFUND_AUTHORITY),
            "an already-expanded #[agent_operable] must still point ApiDoc at the \
             authority static: {generated}"
        );
    }

    #[test]
    fn route_macro_without_agent_operable_emits_none() {
        // The default must stay `None`: `Some` here is a claim that the body
        // was walked and every effect const-asserted against a grant, so an
        // ungoverned handler must never accidentally assert one.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/refunds" },
            quote! { async fn draft_refund() -> &'static str { "drafted" } },
        )
        .to_string();

        assert!(
            generated.contains("agent_authority : :: core :: option :: Option :: None"),
            "a handler with no #[agent_operable] records no authority: {generated}"
        );
        assert!(
            !generated.contains("__AUTUMN_AGENT_AUTHORITY_"),
            "…and must not reference an authority static that was never emitted: {generated}"
        );
    }

    #[test]
    fn route_macro_string_literal_agent_operable_marker_is_not_an_authority() {
        // Handler *text* that merely spells the marker const must not opt the
        // route into a governed authority: the marker is decoded structurally,
        // never scanned for as a string.
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/refunds" },
            quote! {
                async fn draft_refund() -> &'static str {
                    let _ = "const __AUTUMN_AGENT_OPERABLE: &str = \"RefundDrafter\";";
                    "drafted"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("agent_authority : :: core :: option :: Option :: None"),
            "a string literal must not be mistaken for the generated marker: {generated}"
        );
    }

    #[test]
    fn edge_rejects_agent_operable() {
        // The edge lane is read-only and has no session, audit sink or state
        // to record an agent invocation against, so a governed handler must
        // never be served from it. Both stacking orders are refused — the
        // marker is in `GUARD_MARKERS` for exactly this reason.

        // Ordering A: both attributes still live below `#[get]`.
        let attribute_form = route_macro(
            "GET",
            "get",
            quote! { "/refunds" },
            quote! {
                #[edge]
                #[agent_operable(grant = RefundDrafter)]
                async fn draft_refund() -> &'static str { "drafted" }
            },
        )
        .to_string();

        assert!(
            attribute_form.contains("compile_error"),
            "#[edge] + #[agent_operable] must be rejected: {attribute_form}"
        );
        assert!(
            attribute_form.contains("read-only"),
            "the error must say the edge lane is read-only: {attribute_form}"
        );

        // Ordering B: `#[agent_operable]` expanded above `#[get]`, leaving only
        // its marker const in the body.
        let marker_form = route_macro(
            "GET",
            "get",
            quote! { "/refunds" },
            quote! {
                #[edge]
                async fn draft_refund() -> &'static str {
                    #[allow(dead_code)]
                    const __AUTUMN_AGENT_OPERABLE: &str = "RefundDrafter";
                    "drafted"
                }
            },
        )
        .to_string();

        assert!(
            marker_form.contains("compile_error"),
            "an already-expanded #[agent_operable] must still be detected next to \
             #[edge]: {marker_form}"
        );
        assert!(
            marker_form.contains("read-only"),
            "the error must say the edge lane is read-only: {marker_form}"
        );
    }

    // ── capacity contract: statically derived pool set (#1733) ───────────

    #[test]
    fn route_macro_records_the_database_pool_from_a_db_extractor() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/posts" },
            quote! {
                async fn index(db: Db) -> String {
                    let _ = db;
                    String::new()
                }
            },
        )
        .to_string();

        assert!(
            generated.contains(r#"pools : & ["db"]"#),
            "a `Db` extractor must prove the database pool: {generated}"
        );
    }

    #[test]
    fn route_macro_records_no_pools_for_a_compute_only_handler() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/about" },
            quote! {
                async fn about() -> &'static str {
                    "about"
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("pools : & []"),
            "a handler declaring no resource extractor proves no pool: {generated}"
        );
    }

    #[test]
    fn route_macro_records_each_distinct_pool_once_and_in_order() {
        let generated = route_macro(
            "POST",
            "post",
            quote! { "/posts" },
            quote! {
                async fn create(db: Db, mailer: Mailer, events: Events) -> String {
                    let _ = (db, mailer, events);
                    String::new()
                }
            },
        )
        .to_string();

        assert!(
            generated.contains(r#"pools : & ["db" , "events" , "mail"]"#),
            "pools must be sorted and deduplicated for a stable contract diff: {generated}"
        );
    }

    #[test]
    fn route_macro_recognizes_sharded_database_extractors() {
        for extractor in ["ShardedDb", "ShardedReadDb", "Shards", "CrossShard"] {
            let ty = syn::Ident::new(extractor, proc_macro2::Span::call_site());
            let generated = route_macro(
                "GET",
                "get",
                quote! { "/posts" },
                quote! {
                    async fn index(db: #ty) -> String {
                        let _ = db;
                        String::new()
                    }
                },
            )
            .to_string();

            assert!(
                generated.contains(r#"pools : & ["db"]"#),
                "`{extractor}` is a database handle: {generated}"
            );
        }
    }

    #[test]
    fn route_macro_sees_a_pool_extractor_through_a_qualified_path() {
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/posts" },
            quote! {
                async fn index(db: autumn_web::db::Db) -> String {
                    let _ = db;
                    String::new()
                }
            },
        )
        .to_string();

        assert!(
            generated.contains(r#"pools : & ["db"]"#),
            "a fully qualified extractor path still proves the pool: {generated}"
        );
    }

    #[test]
    fn route_macro_does_not_read_a_pool_out_of_a_nested_generic() {
        // `State<AppState<Db>>` is an application-held state value, not a `Db`
        // extractor. Recording the database pool here would contradict the
        // documented rule that `State`-held resources are invisible, and would
        // put false `db-bound` shapes (and digest drift) into the contract.
        let generated = route_macro(
            "GET",
            "get",
            quote! { "/posts" },
            quote! {
                async fn index(state: State<AppState<Db>>) -> String {
                    let _ = state;
                    String::new()
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("pools : & []"),
            "a pool name nested inside another type is not a declared extractor: {generated}"
        );
    }

    #[test]
    fn route_macro_still_sees_a_pool_through_option_and_reference_wrappers() {
        // The recognizer looks at the declared extractor, so the transparent
        // wrappers axum itself understands must not hide it.
        for spelling in ["Option < Db >", "& Db"] {
            let ty: syn::Type = syn::parse_str(spelling).expect("type parses");
            let generated = route_macro(
                "GET",
                "get",
                quote! { "/posts" },
                quote! {
                    async fn index(db: #ty) -> String {
                        let _ = db;
                        String::new()
                    }
                },
            )
            .to_string();

            assert!(
                generated.contains(r#"pools : & ["db"]"#),
                "`{spelling}` still declares the database extractor: {generated}"
            );
        }
    }
}
