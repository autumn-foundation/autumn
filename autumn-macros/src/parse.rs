//! Shared parsing and validation helpers for route macros.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::ParseStream;
use syn::{Attribute, Ident, ItemFn, LitStr, Token};

/// Keys accepted inside a route attribute's `seo(...)` argument, in the order
/// they are documented. Each one maps 1:1 onto a
/// `autumn_web::seo::SeoRouteDefaults` field and a `SeoMeta` builder method.
const SEO_KEYS: &[&str] = &[
    "title",
    "description",
    "canonical",
    "og_title",
    "og_description",
    "og_image",
    "og_type",
    "og_url",
    "twitter_card",
    "twitter_title",
    "twitter_description",
    "twitter_image",
    "robots",
];

/// Parsed `seo(...)` route attribute argument (#1182).
///
/// Holds the declared keys in source order, each paired with its string
/// literal. An absent `seo(...)` argument is an empty `Vec`, which emits
/// `SeoRouteDefaults::EMPTY`.
#[derive(Default)]
pub struct SeoAttrArgs {
    fields: Vec<(Ident, LitStr)>,
}

impl SeoAttrArgs {
    /// Parse the body of a `seo(...)` argument, given a stream positioned at
    /// the `seo` identifier's parenthesized group.
    ///
    /// Rejects unknown keys, repeated keys, and non-string values so a typo
    /// surfaces as a compile error rather than silently-dropped metadata.
    pub fn parse_group(input: ParseStream) -> syn::Result<Self> {
        let content;
        syn::parenthesized!(content in input);

        let mut fields: Vec<(Ident, LitStr)> = Vec::new();
        while !content.is_empty() {
            let key: Ident = content.parse()?;
            let key_name = key.to_string();
            if !SEO_KEYS.contains(&key_name.as_str()) {
                return Err(syn::Error::new(
                    key.span(),
                    format!(
                        "unknown `seo(...)` key `{key_name}`. Supported keys: `{}`.",
                        SEO_KEYS.join("`, `")
                    ),
                ));
            }
            if fields.iter().any(|(existing, _)| *existing == key) {
                return Err(syn::Error::new(
                    key.span(),
                    format!("duplicate `seo(...)` key `{key_name}`. Declare each key once."),
                ));
            }
            let _eq: Token![=] = content.parse()?;
            let value: LitStr = content.parse().map_err(|_| {
                syn::Error::new(
                    key.span(),
                    format!("`seo({key_name} = ...)` expects a string literal."),
                )
            })?;
            fields.push((key, value));

            if content.peek(Token![,]) {
                let _comma: Token![,] = content.parse()?;
            } else {
                break;
            }
        }

        // Anything left over means the argument list is malformed (e.g. a
        // missing comma). Reject it rather than silently dropping keys.
        if !content.is_empty() {
            return Err(content.error("expected `,` between `seo(...)` keys"));
        }

        Ok(Self { fields })
    }

    /// Emit the `autumn_web::seo::SeoRouteDefaults` initializer for these
    /// declared keys, falling back to `SeoRouteDefaults::EMPTY` for the rest.
    pub fn emit(&self) -> TokenStream {
        if self.fields.is_empty() {
            return quote! { ::autumn_web::seo::SeoRouteDefaults::EMPTY };
        }
        let assignments = self.fields.iter().map(|(key, value)| {
            quote! { #key: ::core::option::Option::Some(#value) }
        });
        // Omit the struct-update tail once every key is spelled out: it would
        // have no effect there, and `clippy::needless_update` fires on
        // macro-generated code inside the *user's* crate, where they cannot
        // reasonably silence it.
        if self.fields.len() == SEO_KEYS.len() {
            return quote! {
                ::autumn_web::seo::SeoRouteDefaults {
                    #(#assignments,)*
                }
            };
        }
        quote! {
            ::autumn_web::seo::SeoRouteDefaults {
                #(#assignments,)*
                ..::autumn_web::seo::SeoRouteDefaults::EMPTY
            }
        }
    }
}

/// Parsed route macro attribute arguments.
///
/// Supports:
/// - `"/path"` — path only
/// - `"/path", name = "helper_name"` — path with custom helper name
/// - `"/path", seo(title = "…", description = "…")` — route-level SEO defaults
pub struct RouteAttrArgs {
    pub path: LitStr,
    /// Override for the path-helper function name. When `None`, the helper
    /// name matches the handler function name.
    pub name_override: Option<LitStr>,
    /// API version of the route (e.g. "v1")
    pub api_version: Option<LitStr>,
    /// Whether this route opts out of sunset 410 response
    pub sunset_opt_out: bool,
    /// Per-route override for the global inbound request timeout.
    pub timeout: RouteTimeoutAttr,
    /// Route-level SEO meta tag defaults from the `seo(...)` argument.
    pub seo: SeoAttrArgs,
}

/// Parsed `timeout_ms = ...` / `timeout = "off"` route attribute.
#[derive(Clone, Copy)]
pub enum RouteTimeoutAttr {
    /// No override — inherit the global `request_timeout_ms` deadline.
    Inherit,
    /// Override the global deadline with this many milliseconds.
    Ms(u64),
    /// Exempt this route from the global deadline entirely.
    Disabled,
}

impl RouteAttrArgs {
    /// Return the helper name as an `Ident`, using the override if set.
    /// `handler_name` is used as the fallback.
    pub fn helper_ident(&self, handler_name: &Ident) -> Ident {
        self.name_override.as_ref().map_or_else(
            || handler_name.clone(),
            // Safety: already validated as a valid identifier in `parse_route_attr`.
            |lit| format_ident!("{}", lit.value()),
        )
    }
}

impl syn::parse::Parse for RouteAttrArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        let mut name_override = None;
        let mut api_version = None;
        let mut sunset_opt_out = false;
        let mut timeout = RouteTimeoutAttr::Inherit;
        let mut seo = SeoAttrArgs::default();
        let mut seen_seo = false;

        while input.peek(Token![,]) {
            let _comma: Token![,] = input.parse()?;
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
                seo = SeoAttrArgs::parse_group(input)?;
                continue;
            }
            let _eq: Token![=] = input.parse()?;
            if key == "name" {
                name_override = Some(input.parse::<LitStr>()?);
            } else if key == "api_version" {
                api_version = Some(input.parse::<LitStr>()?);
            } else if key == "sunset_opt_out" {
                let val: syn::LitBool = input.parse()?;
                sunset_opt_out = val.value();
            } else if key == "timeout_ms" {
                let val: syn::LitInt = input.parse()?;
                timeout = RouteTimeoutAttr::Ms(val.base10_parse::<u64>()?);
            } else if key == "timeout" {
                // Only the disabling form is accepted as a string: `timeout = "off"`.
                let val: LitStr = input.parse()?;
                match val.value().as_str() {
                    "off" | "disabled" | "none" => timeout = RouteTimeoutAttr::Disabled,
                    other => {
                        return Err(syn::Error::new(
                            val.span(),
                            format!(
                                "invalid `timeout` value {other:?}. Use `timeout = \"off\"` to \
                                 disable the request deadline, or `timeout_ms = <millis>` to \
                                 override it."
                            ),
                        ));
                    }
                }
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    format!(
                        "unknown route attribute key `{key}`. Supported keys: `name`, \
                         `api_version`, `sunset_opt_out`, `timeout_ms`, `timeout`, `seo(...)`."
                    ),
                ));
            }
        }

        Ok(Self {
            path,
            name_override,
            api_version,
            sunset_opt_out,
            timeout,
            seo,
        })
    }
}

/// Parse and validate a route attribute with optional `name = "..."` override.
///
/// Returns `Ok(args)` if valid, or a compile error `TokenStream` if not.
pub fn parse_route_attr(attr: TokenStream) -> Result<RouteAttrArgs, TokenStream> {
    let args: RouteAttrArgs = syn::parse2(attr).map_err(|err| err.to_compile_error())?;
    validate_path(&args.path)?;
    if let Some(ref name_lit) = args.name_override {
        syn::parse_str::<Ident>(&name_lit.value()).map_err(|_| {
            syn::Error::new(
                name_lit.span(),
                format!(
                    "route `name` override {:?} is not a valid Rust identifier",
                    name_lit.value()
                ),
            )
            .to_compile_error()
        })?;
    }
    Ok(args)
}

/// Parse and validate a route path from macro attributes.
///
/// Returns `Ok(path)` if valid, or a compile error `TokenStream` if not.
/// Validates: non-empty, starts with '/'.
pub fn parse_route_path(attr: TokenStream) -> Result<LitStr, TokenStream> {
    let path: LitStr = syn::parse2(attr).map_err(|err| err.to_compile_error())?;
    validate_path(&path)?;
    Ok(path)
}

fn validate_path(path: &LitStr) -> Result<(), TokenStream> {
    if path.value().is_empty() {
        return Err(syn::Error::new(path.span(), "Route path must not be empty").to_compile_error());
    }

    if !path.value().starts_with('/') {
        let suggested = format!("/{}", path.value());
        return Err(syn::Error::new(
            path.span(),
            format!("Route path must start with '/'. Did you mean \"{suggested}\"?"),
        )
        .to_compile_error());
    }

    Ok(())
}

/// Parse and validate an async handler function from macro input.
///
/// Returns `Ok(func)` if valid, or a compile error `TokenStream` if not.
/// Validates: is a function, is async.
pub fn parse_async_handler(item: TokenStream) -> Result<ItemFn, TokenStream> {
    let input_fn: ItemFn = syn::parse2(item.clone()).map_err(|_| {
        syn::Error::new_spanned(item, "route macros can only be applied to functions")
            .to_compile_error()
    })?;

    if input_fn.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "Autumn route handlers must be async functions",
        )
        .to_compile_error());
    }

    Ok(input_fn)
}

/// Extract `#[intercept(LayerType)]` attributes from a function's attribute
/// list, removing them so they don't appear on the emitted function.
///
/// Returns the type paths in the order they appeared.
pub fn extract_interceptors(attrs: &mut Vec<Attribute>) -> Vec<syn::Path> {
    let mut interceptors = Vec::new();
    attrs.retain(|attr| {
        if attr.path().is_ident("intercept") {
            if let Ok(path) = attr.parse_args::<syn::Path>() {
                interceptors.push(path);
            }
            false // remove from the attribute list
        } else {
            true // keep
        }
    });
    interceptors
}
