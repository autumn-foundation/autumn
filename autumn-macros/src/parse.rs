//! Shared parsing and validation helpers for route macros.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::ParseStream;
use syn::{Attribute, Ident, ItemFn, LitStr, Token};

/// Keys accepted inside a route attribute's `seo(...)` argument, in the order
/// they are documented. Each one maps 1:1 onto a
/// `autumn_web::seo::SeoRouteDefaults` field and a `SeoMeta` builder method.
///
/// [`SeoAttrArgs::emit`] derives the setter call for a key by concatenating
/// `with_` onto it, and that setter lives in the *other* crate — so adding a
/// key here without adding the matching
/// `SeoRouteDefaults::with_<key>` still compiles this crate and only fails in
/// a user's crate ("no method named `with_…`"). The
/// `every_supported_key_round_trips` integration test in
/// `autumn/tests/integration/seo.rs` exercises all of them and is what catches
/// that mismatch; extend it when extending this list.
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
    /// Declared `(key, value)` pairs in source order. The key is stored as a
    /// plain `String` (raw-ident prefix already stripped) because it is only
    /// ever used to build a `with_<key>` setter name.
    fields: Vec<(String, LitStr)>,
}

impl SeoAttrArgs {
    /// Parse the body of a `seo(...)` argument, given a stream positioned at
    /// the `seo` identifier's parenthesized group.
    ///
    /// Rejects unknown keys, repeated keys, and non-string values so a typo
    /// surfaces as a compile error rather than silently-dropped metadata.
    pub fn parse_group(input: ParseStream) -> syn::Result<Self> {
        let span = input.span();
        let content;
        syn::parenthesized!(content in input);

        let mut fields: Vec<(String, LitStr)> = Vec::new();
        while !content.is_empty() {
            let key: Ident = content.parse()?;
            // Compare against the unprefixed name so `r#title` isn't reported
            // as unknown while the diagnostic lists `title` as supported.
            let key_name = key.to_string();
            let key_name = key_name.strip_prefix("r#").unwrap_or(&key_name).to_owned();
            if !SEO_KEYS.contains(&key_name.as_str()) {
                return Err(syn::Error::new(
                    key.span(),
                    format!(
                        "unknown `seo(...)` key `{key_name}`. Supported keys: `{}`.",
                        SEO_KEYS.join("`, `")
                    ),
                ));
            }
            if fields.iter().any(|(existing, _)| *existing == key_name) {
                return Err(syn::Error::new(
                    key.span(),
                    format!("duplicate `seo(...)` key `{key_name}`. Declare each key once."),
                ));
            }
            let _eq: Token![=] = content.parse()?;
            // Span the *value*, not the key: with `seo(og_image = OG_URL)` the
            // key is the part the user got right.
            let value_span = content.span();
            let value: LitStr = content.parse().map_err(|_| {
                syn::Error::new(
                    value_span,
                    format!("`seo({key_name} = ...)` expects a string literal."),
                )
            })?;
            fields.push((key_name, value));

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

        // An empty `seo()` is almost certainly an unfinished edit. Rejecting it
        // is consistent with rejecting typo'd keys: the whole point is that
        // declared SEO metadata never silently fails to render.
        if fields.is_empty() {
            return Err(syn::Error::new(
                span,
                format!(
                    "empty `seo(...)`. Declare at least one key, e.g. \
                     `seo(title = \"…\")`. Supported keys: `{}`.",
                    SEO_KEYS.join("`, `")
                ),
            ));
        }

        Ok(Self { fields })
    }

    /// Emit the `autumn_web::seo::SeoRouteDefaults` value for these declared
    /// keys, leaving the rest at their `SeoRouteDefaults::EMPTY` value.
    ///
    /// Deliberately built by chaining the `const fn with_*` setters rather than
    /// by emitting a struct literal. A literal in the *user's* crate would pin
    /// `SeoRouteDefaults` as exhaustively-constructible forever — adding a
    /// fourteenth SEO key later would then be a breaking change — and it would
    /// also trip `clippy::needless_update` there once every key is spelled out.
    pub fn emit(&self) -> TokenStream {
        let setters = self.fields.iter().map(|(key, value)| {
            let setter = format_ident!("with_{}", key);
            quote! { .#setter(#value) }
        });
        quote! {
            ::autumn_web::seo::SeoRouteDefaults::EMPTY #(#setters)*
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

/// Split a macro input into any leading items plus the trailing function.
///
/// Returns `None` when the stream is not a sequence of items ending in a
/// function, so callers can fall back to their own diagnostic.
///
/// The body guards (`#[secured]`, `#[step_up]`, `#[throttle]`, `#[authorize]`)
/// stack on one another, and since #1668 each emits a `FromRequestParts` gate
/// type ALONGSIDE the handler. So whenever an inner guard has already expanded,
/// an outer guard's input is `[items…] fn` rather than a bare `fn`, and parsing
/// it as a single `ItemFn` fails. Each guard re-emits the prelude ahead of its
/// own output so the inner gate types stay in scope.
pub fn split_handler_prelude(item: TokenStream) -> Option<(TokenStream, ItemFn)> {
    let file: syn::File = syn::parse2(item).ok()?;
    let (last, prelude) = file.items.split_last()?;
    let syn::Item::Fn(input_fn) = last else {
        return None;
    };
    Some((quote! { #(#prelude)* }, input_fn.clone()))
}

/// Parse and validate an async handler function from macro input, together
/// with any items that precede it.
///
/// Returns `Ok((prelude, func))` if valid, or a compile error `TokenStream` if
/// not. Validates: the trailing item is a function, and it is async.
///
/// WHY A PRELUDE: a guard attribute written ABOVE a route attribute —
/// `#[secured]` / `#[step_up]` / `#[throttle]` over `#[get]` — expands FIRST
/// and hands its output to the route macro. Since #1668 each of those guards
/// emits a handler-unique `FromRequestParts` gate type ALONGSIDE the handler
/// and inserts a `_: Gate` parameter referring to it, so what arrives here is
/// `[items…] fn`, not a bare `fn`. Parsing the whole stream as a single
/// `ItemFn` rejected that with "route macros can only be applied to
/// functions", which is how #2488 broke the four
/// `route_macro_infers_response_schema_*` tests #2484 had just added.
///
/// The leading items are handed back untouched for the caller to re-emit ahead
/// of its own output, which is what keeps the gate type in scope for the
/// parameter the guard inserted. A bare `fn` yields an empty prelude, so the
/// single-item path is unchanged.
pub fn parse_async_handler_with_prelude(
    item: TokenStream,
) -> Result<(TokenStream, ItemFn), TokenStream> {
    // Cloned once up front purely to span the diagnostic; `item` itself is then
    // consumed by the parse below.
    let item_for_err = item.clone();
    let not_a_fn = move || {
        syn::Error::new_spanned(
            &item_for_err,
            "route macros can only be applied to functions",
        )
        .to_compile_error()
    };

    // `syn::File` rather than `ItemFn`: it accepts a sequence of items, and the
    // handler is the last of them. Anything that is not a sequence of items, or
    // whose final item is not a function, still fails with the original
    // diagnostic.
    let file: syn::File = syn::parse2(item).map_err(|_| not_a_fn())?;
    // One arm covers both rejections: an empty stream (no last item) and a
    // stream whose final item is not a function.
    let Some((syn::Item::Fn(input_fn), prelude)) = file.items.split_last() else {
        return Err(not_a_fn());
    };
    let input_fn = input_fn.clone();

    if input_fn.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "Autumn route handlers must be async functions",
        )
        .to_compile_error());
    }

    Ok((quote! { #(#prelude)* }, input_fn))
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
