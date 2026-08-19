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

    let mut input_fn = match parse::parse_async_handler(item) {
        Ok(f) => f,
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
        && let Some(err) = reject_ineligible_edge_route(http_method, &input_fn, marking.span)
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
    let handler_expr = build_handler_expr(
        &routing_fn,
        &handler_name,
        &interceptors,
        !body_guarded_replay && !intercepted_route,
    );
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
                    public: #is_public,
                    module_path: ::core::module_path!(),
                    source_file: ::core::file!(),
                    source_line: ::core::line!(),
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

/// Marker consts the auth/rate guards inject into a handler body. A guard that
/// expanded *before* this route macro left one of these behind instead of an
/// attribute, and missing it would ship a guarded route to the unauthenticated
/// edge lane.
const GUARD_MARKERS: &[&str] = &[
    "__AUTUMN_SECURED_ROLES",
    "__AUTUMN_SECURED_SCOPES",
    "__AUTUMN_STEP_UP_MAX_AGE",
    "__AUTUMN_THROTTLE_ROUTE_ID",
    "__AUTUMN_AUTHORIZE_BINDINGS",
];

/// Reject an `#[edge]` route the edge lane cannot serve, spanning the error at
/// the `#[edge]` attribute (or the handler name once it has expanded).
fn reject_ineligible_edge_route(
    http_method: &str,
    input_fn: &syn::ItemFn,
    span: Span,
) -> Option<TokenStream> {
    if http_method != "GET" {
        return Some(syn::Error::new(span, EDGE_METHOD_ERROR).to_compile_error());
    }
    if has_auth_or_rate_guard(input_fn) {
        return Some(syn::Error::new(span, EDGE_GUARD_ERROR).to_compile_error());
    }
    None
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
                    // primitive-output wrapper when there is one — so the two
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

fn has_step_up_guard(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "step_up")
    })
}

fn has_throttle_guard(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "throttle")
    })
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
        let generated = route_macro("POST", "post", quote! { "/notes/{id}" }, secured).to_string();

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
        let authorized =
            crate::authorize::authorize_macro(quote! { "update", resource = Note }, secured);
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
        let generated = route_macro("POST", "post", quote! { "/notes/{id}" }, secured).to_string();

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
}
