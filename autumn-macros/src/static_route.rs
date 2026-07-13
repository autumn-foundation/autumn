//! `#[static_get]` macro implementation.
//!
//! Generates both a regular route companion (`__autumn_route_info_{name}`)
//! AND a static metadata companion (`__autumn_static_meta_{name}`), marking
//! the handler for static pre-rendering at build time.
//!
//! ## Supported forms
//!
//! - `#[static_get("/about")]` -- simple static route
//! - `#[static_get("/posts/{slug}", params = list_slugs)]` -- parameterized
//! - `#[static_get("/posts/{slug}", params = list_slugs, revalidate = 60)]` -- with ISR

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitInt, LitStr, Token};

/// Parsed attributes for `#[static_get("/path", params = fn, revalidate = N)]`.
struct StaticGetAttrs {
    path: LitStr,
    params_fn: Option<syn::Path>,
    revalidate: Option<u64>,
}

impl Parse for StaticGetAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;

        let mut params_fn = None;
        let mut revalidate = None;

        while input.peek(Token![,]) {
            let _comma: Token![,] = input.parse()?;

            // Allow trailing comma
            if input.is_empty() {
                break;
            }

            let key: Ident = input.parse()?;
            let _eq: Token![=] = input.parse()?;

            match key.to_string().as_str() {
                "params" => {
                    let path: syn::Path = input.parse()?;
                    params_fn = Some(path);
                }
                "revalidate" => {
                    let lit: LitInt = input.parse()?;
                    revalidate = Some(lit.base10_parse::<u64>()?);
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("Unknown attribute `{other}`. Expected `params` or `revalidate`."),
                    ));
                }
            }
        }

        Ok(Self {
            path,
            params_fn,
            revalidate,
        })
    }
}

/// Core implementation for the `#[static_get("/path")]` attribute macro.
///
/// Emits:
/// 1. The original `async fn` unchanged.
/// 2. `__autumn_route_info_{name}()` returning `::autumn_web::Route`
///    (identical to what `#[get]` produces).
/// 3. `__autumn_static_meta_{name}()` returning
///    `::autumn_web::static_gen::StaticRouteMeta`.
#[allow(clippy::too_many_lines)]
pub fn static_get_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs: StaticGetAttrs = match syn::parse2(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let path = &attrs.path;

    // Validate path
    if path.value().is_empty() {
        return syn::Error::new(path.span(), "Route path must not be empty").to_compile_error();
    }

    if !path.value().starts_with('/') {
        let suggested = format!("/{}", path.value());
        return syn::Error::new(
            path.span(),
            format!("Route path must start with '/'. Did you mean \"{suggested}\"?"),
        )
        .to_compile_error();
    }

    // Parameterized routes require a params function
    let has_params = path.value().contains('{');
    if has_params && attrs.params_fn.is_none() {
        return syn::Error::new(
            path.span(),
            "Parameterized static routes require a `params` function. \
             Example: #[static_get(\"/posts/{slug}\", params = list_slugs)]",
        )
        .to_compile_error();
    }

    // Non-parameterized routes should not have a params function
    if !has_params && attrs.params_fn.is_some() {
        return syn::Error::new(
            path.span(),
            "Static route has no path parameters but a `params` function was provided. \
             Either add path parameters or remove the `params` attribute.",
        )
        .to_compile_error();
    }

    // Parse the async handler function
    let input_fn = match crate::parse::parse_async_handler(item) {
        Ok(f) => f,
        Err(err) => return err,
    };

    let fn_name = &input_fn.sig.ident;
    let route_info_name = format_ident!("__autumn_route_info_{}", fn_name);
    let static_meta_name = format_ident!("__autumn_static_meta_{}", fn_name);
    let vis = &input_fn.vis;

    // Honor a `#[public]` marker on the handler so the route audit gate can
    // classify this static route as `public` rather than `unclassified`.
    // Mirrors the standard `#[get]`/`#[post]` route macro (see
    // `crate::route`), which is the only place `is_public` was previously
    // wired.
    let is_public = crate::api_doc::is_public(&input_fn);

    // Build the revalidate expression
    let revalidate_expr = attrs.revalidate.map_or_else(
        || quote! { ::core::option::Option::None },
        |secs| quote! { ::core::option::Option::Some(#secs) },
    );

    // Build the params_fn expression
    let params_fn_expr = attrs.params_fn.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |pf| {
            quote! {
                ::core::option::Option::Some(
                    |router: ::autumn_web::reexports::axum::Router|
                        -> ::core::pin::Pin<Box<dyn ::core::future::Future<
                            Output = Vec<::autumn_web::static_gen::StaticParams>
                        > + Send>> {
                        Box::pin(#pf(router))
                    }
                )
            }
        },
    );

    let path_value = path.value();
    let path_params = crate::api_doc::extract_path_params(&path_value);
    let path_params_tokens = crate::api_doc::emit_path_param_slice(&path_params);

    quote! {
        #input_fn

        #[doc(hidden)]
        #vis fn #route_info_name() -> ::autumn_web::Route {
            ::autumn_web::Route {
                method: ::autumn_web::reexports::http::Method::GET,
                path: #path,
                handler: ::autumn_web::reexports::axum::routing::get(#fn_name),
                name: ::core::stringify!(#fn_name),
                api_doc: ::autumn_web::openapi::ApiDoc {
                    method: "GET",
                    path: #path,
                    operation_id: ::core::stringify!(#fn_name),
                    summary: ::core::option::Option::None,
                    description: ::core::option::Option::None,
                    tags: &[],
                    path_params: #path_params_tokens,
                    request_body: ::core::option::Option::None,
                    response: ::core::option::Option::None,
                    success_status: 200,
                    hidden: false,
                    query_schema: ::core::option::Option::None,
                    secured: false,
                    required_roles: &[],
                    register_schemas: ::core::option::Option::None,
                    api_version: ::core::option::Option::None,
                    public: #is_public,
                    module_path: ::core::module_path!(),
                    ..::core::default::Default::default()
                },
                repository: ::core::option::Option::None,
                idempotency: ::autumn_web::RouteIdempotency::Direct,
                // Live inbound requests to this GET route (a manifest miss, no
                // `dist`, or a cache read that falls through to the dynamic
                // router) inherit the global deadline like any other route, so a
                // slow/hung handler can't bypass `request_timeout_ms`. The
                // build/ISR prerenders that drive this route internally are
                // exempted instead via the `RenderDeadlineExempt` request marker
                // (see `autumn_web::static_gen`), scoping the exemption to those
                // internal renders rather than the live route metadata.
                timeout: ::autumn_web::RouteTimeout::Inherit,
                api_version: ::core::option::Option::None,
                sunset_opt_out: false,
            }
        }

        #[doc(hidden)]
        #vis fn #static_meta_name() -> ::autumn_web::static_gen::StaticRouteMeta {
            ::autumn_web::static_gen::StaticRouteMeta {
                path: #path,
                name: ::core::stringify!(#fn_name),
                revalidate: #revalidate_expr,
                params_fn: #params_fn_expr,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::static_get_macro;

    #[test]
    fn static_get_defaults_public_false() {
        let generated = static_get_macro(
            quote! { "/about" },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("public : false"),
            "an unannotated static route must record public = false: {generated}"
        );
        // The handler's module path is captured for audit diagnostics.
        assert!(generated.contains("module_path"));
    }

    #[test]
    fn static_get_marks_public_when_public_attribute_present() {
        // Ordering A: `#[static_get]` outermost, `#[public]` still an attribute below.
        let generated = static_get_macro(
            quote! { "/about" },
            quote! {
                #[public]
                async fn about() -> &'static str { "about" }
            },
        )
        .to_string();
        assert!(
            generated.contains("public : true"),
            "a #[public] static route must record public = true: {generated}"
        );
    }

    #[test]
    fn static_get_marks_public_from_expanded_marker() {
        // Ordering B: `#[public]` written above `#[static_get]`, so it expands
        // first and injects the `__AUTUMN_PUBLIC` marker into the body; the
        // static route macro must recognize the marker.
        let public_fn = crate::public::public_macro(
            quote! {},
            quote! { async fn about() -> &'static str { "about" } },
        );
        let generated = static_get_macro(quote! { "/about" }, public_fn).to_string();
        assert!(
            generated.contains("public : true"),
            "a static route macro over an expanded #[public] marker must record public = true: {generated}"
        );
    }
}
