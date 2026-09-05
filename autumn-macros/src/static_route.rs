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
//! - `#[static_get("/about", seo(title = "About"))]` -- route-level SEO defaults

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitInt, LitStr, Token};

/// Parsed attributes for `#[static_get("/path", params = fn, revalidate = N)]`.
struct StaticGetAttrs {
    path: LitStr,
    params_fn: Option<syn::Path>,
    revalidate: Option<u64>,
    /// Route-level SEO meta tag defaults from the `seo(...)` argument (#1182).
    seo: crate::parse::SeoAttrArgs,
}

impl Parse for StaticGetAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;

        let mut params_fn = None;
        let mut revalidate = None;
        let mut seo = crate::parse::SeoAttrArgs::default();
        let mut seen_seo = false;

        while input.peek(Token![,]) {
            let _comma: Token![,] = input.parse()?;

            // Allow trailing comma
            if input.is_empty() {
                break;
            }

            let key: Ident = input.parse()?;

            // `seo(...)` is the one call-shaped argument; everything else is
            // `key = value`, so branch before consuming the `=`.
            if key == "seo" {
                if !input.peek(syn::token::Paren) {
                    return Err(syn::Error::new(
                        key.span(),
                        "`seo` takes parenthesized keys, e.g. \
                         `seo(title = \"About\", description = \"…\")`.",
                    ));
                }
                if seen_seo {
                    return Err(syn::Error::new(
                        key.span(),
                        "duplicate `seo(...)` argument. Declare all SEO keys in one `seo(...)`.",
                    ));
                }
                seen_seo = true;
                seo = crate::parse::SeoAttrArgs::parse_group(input)?;
                continue;
            }

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
                        format!(
                            "Unknown attribute `{other}`. Expected `params`, `revalidate`, \
                             or `seo(...)`."
                        ),
                    ));
                }
            }
        }

        Ok(Self {
            path,
            params_fn,
            revalidate,
            seo,
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
    // `#[edge]` would be inert here and silently so: the page is rendered at
    // build time and served from the CDN already (#1790).
    if let Some(err) = crate::edge::reject_if_edge(
        &item,
        "a `#[static_get]` route is already pre-rendered and served CDN-side; \
         `#[edge]` adds nothing",
    ) {
        return err;
    }

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
    let (leading_guard_items, input_fn) = match crate::parse::parse_async_handler(item) {
        Ok(v) => v,
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
    let seo_defaults = attrs.seo.emit();

    quote! {
        #leading_guard_items
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
                    source_file: ::core::file!(),
                    source_line: ::core::line!(),
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
                seo: #seo_defaults,
            }
        }

        #[doc(hidden)]
        #vis fn #static_meta_name() -> ::autumn_web::static_gen::StaticRouteMeta {
            ::autumn_web::static_gen::StaticRouteMeta {
                path: #path,
                name: ::core::stringify!(#fn_name),
                revalidate: #revalidate_expr,
                params_fn: #params_fn_expr,
                seo: #seo_defaults,
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

    // ── seo(...) route-level defaults (#1182) ───────────────────────────────

    #[test]
    fn static_get_defaults_seo_to_empty() {
        let generated = static_get_macro(
            quote! { "/about" },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("SeoRouteDefaults :: EMPTY"),
            "a static route without seo(...) must record the empty defaults: {generated}"
        );
    }

    #[test]
    fn static_get_emits_declared_seo_defaults() {
        let generated = static_get_macro(
            quote! { "/about", seo(title = "About", og_type = "website") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("with_title (\"About\")"),
            "seo(title = ...) must populate the static route defaults: {generated}"
        );
        assert!(
            generated.contains("with_og_type (\"website\")"),
            "seo(og_type = ...) must populate the static route defaults: {generated}"
        );
    }

    #[test]
    fn static_get_seo_composes_with_params_and_revalidate() {
        let generated = static_get_macro(
            quote! { "/posts/{slug}", seo(og_type = "article"), params = list_slugs, revalidate = 60 },
            quote! { async fn show() -> &'static str { "post" } },
        )
        .to_string();
        assert!(
            generated.contains("with_og_type (\"article\")"),
            "seo(...) must parse alongside params/revalidate: {generated}"
        );
        assert!(
            generated.contains("list_slugs"),
            "params must still parse after seo(...): {generated}"
        );
        assert!(
            generated.contains("60"),
            "revalidate must still parse after seo(...): {generated}"
        );
    }

    #[test]
    fn static_get_rejects_unknown_seo_key() {
        let generated = static_get_macro(
            quote! { "/about", seo(titel = "About") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "an unknown seo key must be a compile error on static routes: {generated}"
        );
    }

    // `#[static_get]` has its own attribute parser, so the rejection paths the
    // `#[get]` family covers are re-asserted here rather than assumed.

    #[test]
    fn static_get_rejects_duplicate_seo_key() {
        let generated = static_get_macro(
            quote! { "/about", seo(title = "A", title = "B") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "a duplicate seo key must be a compile error on static routes: {generated}"
        );
    }

    #[test]
    fn static_get_rejects_non_string_seo_value() {
        let generated = static_get_macro(
            quote! { "/about", seo(title = 42) },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "a non-string seo value must be a compile error on static routes: {generated}"
        );
    }

    #[test]
    fn static_get_rejects_seo_without_parentheses() {
        let generated = static_get_macro(
            quote! { "/about", seo = "title" },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "`seo = ...` must be rejected on static routes too: {generated}"
        );
    }

    #[test]
    fn static_get_rejects_empty_seo_group() {
        let generated = static_get_macro(
            quote! { "/about", seo() },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "an empty seo() must be a compile error on static routes: {generated}"
        );
    }

    #[test]
    fn static_get_rejects_repeated_seo_argument() {
        let generated = static_get_macro(
            quote! { "/about", seo(title = "A"), seo(og_type = "website") },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "a second seo(...) argument must be a compile error on static routes: {generated}"
        );
    }

    // ── `#[edge]` is meaningless on a pre-rendered route (#1790) ────────────

    #[test]
    fn static_get_rejects_a_live_edge_attribute() {
        let generated = static_get_macro(
            quote! { "/about" },
            quote! {
                #[edge]
                async fn about() -> &'static str { "about" }
            },
        )
        .to_string();
        assert!(
            generated.contains("compile_error"),
            "#[edge] on a #[static_get] route must be rejected: {generated}"
        );
        assert!(
            generated.contains("already pre-rendered"),
            "the error must explain that the page is already served CDN-side: {generated}"
        );
    }

    #[test]
    fn static_get_rejects_an_expanded_edge_marker() {
        // `#[edge]` written above `#[static_get]`, so it expanded first and
        // left only the `__AUTUMN_EDGE` marker in the body.
        let edged = crate::edge::edge_macro(
            quote! {},
            quote! { async fn about() -> &'static str { "about" } },
        );
        let generated = static_get_macro(quote! { "/about" }, edged).to_string();
        assert!(
            generated.contains("compile_error"),
            "an already-expanded #[edge] must be rejected on #[static_get] too: {generated}"
        );
    }

    #[test]
    fn static_get_without_edge_is_untouched() {
        let generated = static_get_macro(
            quote! { "/about" },
            quote! { async fn about() -> &'static str { "about" } },
        )
        .to_string();
        assert!(
            !generated.contains("compile_error"),
            "an ordinary static route must still compile: {generated}"
        );
        assert!(
            !generated.contains("autumn_edge"),
            "an ordinary static route must not reference autumn_edge: {generated}"
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
