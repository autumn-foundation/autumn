//! Shared parsing and validation helpers for route macros.

use proc_macro2::TokenStream;
use quote::format_ident;
use syn::{Attribute, Ident, ItemFn, LitStr, Token};

/// Parsed route macro attribute arguments.
///
/// Supports:
/// - `"/path"` — path only
/// - `"/path", name = "helper_name"` — path with custom helper name
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

        while input.peek(Token![,]) {
            let _comma: Token![,] = input.parse()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
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
                         `api_version`, `sunset_opt_out`, `timeout_ms`, `timeout`."
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
