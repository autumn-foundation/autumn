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
    let primitive_wrapper = if should_stringify_primitive_output(&input_fn.sig.output) {
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
    // ── OpenAPI metadata ────────────────────────────────────────
    let path_params = api_doc::extract_path_params(&path.value());
    let path_params_tokens = api_doc::emit_path_param_slice(&path_params);
    let request_body = api_doc::schema_option(api_doc::infer_request_body(&input_fn));
    let response_body = api_doc::schema_option(api_doc::infer_response_body(&input_fn));
    let query_schema = api_doc::schema_option(api_doc::infer_query_params(&input_fn));
    let (secured, required_roles, required_scopes) = api_doc::extract_secured_info(&input_fn);
    let has_feature_flag = has_feature_flag_attr || has_expanded_feature_flag_gate(&input_fn);
    let body_guarded_replay = secured
        || has_authorize_guard(&input_fn)
        || has_feature_flag
        || has_step_up_guard(&input_fn);
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

    // ── Path helper ─────────────────────────────────────────────
    let path_helper = emit_path_helper(&path_helper_name, &path, &path_params);

    quote! {
        // ECHO-001: We want to apply #[axum::debug_handler] but without forcing the user
        // to import axum manually. However, the path resolution in Axum macros makes this impossible
        // natively. Custom compile errors handle the type checks.
        #input_fn
        #primitive_wrapper

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
                    #api_doc_fields
                },
                repository: ::core::option::Option::None,
                idempotency: #route_idempotency,
                timeout: #route_timeout,
            }
        }

        #path_helper
        #fn_name_alias
    }
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

fn should_stringify_primitive_output(output: &ReturnType) -> bool {
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
}
