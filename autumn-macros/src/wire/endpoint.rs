//! `#[endpoint]` — mark a typed handler as a service endpoint.
//!
//! The attribute reads the handler's own signature: the `Json<T>` parameter is
//! the request shape, the `Json<T>` in the return type is the response shape,
//! and the route attribute below it supplies the method and path. Nothing is
//! restated, so nothing can drift.
//!
//! It emits a marker type plus its [`Endpoint`] impl, and leaves the handler
//! untouched. The marker carries the real Rust types as associated types, which
//! is what lets a caller in another crate name them without knowing a module
//! path. It also writes the endpoint's JSON descriptor.
//!
//! `#[endpoint]` must sit **above** the route attribute. A route attribute
//! expands first when it is outermost and rewrites the signature, leaving
//! nothing to read; being outermost is what guarantees the pristine signature.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{Ident, ItemFn, LitStr};

use crate::param_helpers::attr_or_cfg_attr_matches_any;
use crate::wire::ir::EndpointDescriptor;
use crate::wire::store;

/// The route attributes `#[endpoint]` can read a method and path from.
const ROUTE_ATTRS: [(&str, &str); 5] = [
    ("get", "GET"),
    ("post", "POST"),
    ("put", "PUT"),
    ("patch", "PATCH"),
    ("delete", "DELETE"),
];

/// Parsed `#[endpoint(...)]` arguments.
struct Args {
    service: String,
    name: Option<String>,
}

impl syn::parse::Parse for Args {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut service = None;
        let mut name = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            let value: LitStr = input.parse()?;
            match key.to_string().as_str() {
                "service" => service = Some(value.value()),
                "name" => name = Some(value.value()),
                other => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "unknown #[endpoint] argument `{other}`; expected `service` or `name`"
                        ),
                    ));
                }
            }
            if input.peek(syn::Token![,]) {
                input.parse::<syn::Token![,]>()?;
            }
        }
        let service = service.ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "#[endpoint] needs a service name: #[endpoint(service = \"catalog\")]",
            )
        })?;
        // A service name reaches a descriptor's file name, so anything outside
        // this set could steer the write out of the contract directory.
        if service.is_empty() || !service.chars().all(is_name_char) {
            return Err(syn::Error::new(
                Span::call_site(),
                format!(
                    "#[endpoint] service must be a non-empty name of letters, digits, `_` or \
                     `-`, and is `{service}`"
                ),
            ));
        }
        // An endpoint name reaches the file name too, AND becomes the marker
        // type's identifier — so it has to be identifier-shaped, not merely
        // file-safe. A `-` or a leading digit would otherwise panic the macro.
        if let Some(name) = &name
            && !is_identifier(name)
        {
            return Err(syn::Error::new(
                Span::call_site(),
                format!(
                    "#[endpoint] name must be a Rust identifier — it becomes the `{name}_endpoint` \
                     marker type — and is `{name}`"
                ),
            ));
        }
        Ok(Self { service, name })
    }
}

/// Characters an endpoint's service and name may use — the set that is safe in
/// a file name on every platform the workspace builds on.
const fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

/// Whether `name` can be used as a Rust identifier.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn endpoint_macro(attr: TokenStream, item: &TokenStream) -> TokenStream {
    match expand(attr, item) {
        Ok(ts) => ts,
        Err(err) => {
            let err = err.to_compile_error();
            quote! { #err #item }
        }
    }
}

fn expand(attr: TokenStream, item: &TokenStream) -> Result<TokenStream, syn::Error> {
    let args: Args = syn::parse2(attr)?;
    let func: ItemFn = syn::parse2(item.clone())?;

    let (method, path) = route_of(&func)?;
    let name = args.name.unwrap_or_else(|| func.sig.ident.to_string());
    let marker = format_ident!("{}_endpoint", name);

    let (request_ty, response_ty) = body_types(&func)?;

    let has_body = request_ty.is_some();
    let request_tokens = request_ty.as_ref().map_or_else(
        || quote! { ::autumn_web::wire::NoBody },
        |ty| quote! { #ty },
    );

    let written = store::contract_dir().map_or_else(TokenStream::new, |dir| {
        let site = Site {
            krate: std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
            module: store::source_module(func.sig.ident.span()),
            service: args.service.clone(),
            name: name.clone(),
            handler: func.sig.ident.to_string(),
            marker: marker.to_string(),
            method: method.clone(),
            path: path.clone(),
        };
        let descriptor = endpoint_descriptor(&site, request_ty.as_ref(), &response_ty);
        store::write_outcome(store::write_endpoint(&dir, &descriptor))
    });

    let doc = format!(
        "Wire contract for the `{}.{}` endpoint (`{method} {path}`).\n\n\
         Generated by `#[endpoint]`. Name it from `wire_client!` to call it.",
        args.service, name
    );
    let service = &args.service;
    let track = store::track_contract_dir();

    Ok(quote! {
        #track
        #written

        #[doc = #doc]
        #[allow(non_camel_case_types)]
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
        pub struct #marker;

        impl ::autumn_web::wire::Endpoint for #marker {
            type Request = #request_tokens;
            type Response = #response_ty;
            const SERVICE: &'static str = #service;
            const NAME: &'static str = #name;
            const METHOD: &'static str = #method;
            const PATH: &'static str = #path;
            const HAS_BODY: bool = #has_body;
        }

        #item
    })
}

/// The `Json<T>` a handler returns, however it is wrapped.
fn response_type(func: &ItemFn) -> Option<syn::Type> {
    let ty = crate::api_doc::sig_output_type(func)?;
    let ty = crate::api_doc::unwrap_result_ok(&ty).unwrap_or(ty);
    crate::api_doc::find_json_in_type(&ty)
}

/// The method and path of the route attribute below `#[endpoint]`.
fn route_of(func: &ItemFn) -> Result<(String, String), syn::Error> {
    for attr in &func.attrs {
        let Some((_, method)) = ROUTE_ATTRS
            .iter()
            .find(|(name, _)| attr_or_cfg_attr_matches_any(attr, &[name]))
        else {
            continue;
        };
        // `#[cfg_attr(predicate, get("/items"))]` is a supported spelling
        // elsewhere, and its own argument list starts with the predicate — so
        // the route's tokens are nested one level down, not where a bare
        // `#[get(...)]` keeps them.
        let route_tokens = if attr.path().is_ident("cfg_attr") {
            let Some(tokens) = nested_route_tokens(attr) else {
                continue;
            };
            tokens
        } else {
            attr.parse_args_with(|input: syn::parse::ParseStream<'_>| input.parse::<TokenStream>())?
        };
        let path = syn::parse2::<RoutePath>(route_tokens)?.0;
        return Ok(((*method).to_owned(), path.value()));
    }
    Err(syn::Error::new_spanned(
        &func.sig,
        "#[endpoint] must sit directly above the route attribute (`#[get]`, `#[post]`, \
         `#[put]`, `#[patch]`, `#[delete]`): it reads the method and path from it, and \
         the handler's signature before the route macro rewrites it",
    ))
}

/// A route attribute's own argument list: a path literal, then anything else.
struct RoutePath(LitStr);

impl syn::parse::Parse for RoutePath {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        // A route attribute carries more than a path (`seo(...)`, and so on).
        // Only the first literal is wanted here.
        input.parse::<TokenStream>()?;
        Ok(Self(path))
    }
}

/// The route attribute's own tokens from inside a `#[cfg_attr(pred, route(…))]`.
fn nested_route_tokens(attr: &syn::Attribute) -> Option<TokenStream> {
    let syn::Meta::List(list) = &attr.meta else {
        return None;
    };
    let nested = list
        .parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .ok()?;
    nested.into_iter().find_map(|meta| match meta {
        syn::Meta::List(inner)
            if ROUTE_ATTRS
                .iter()
                .any(|(name, _)| inner.path.is_ident(name)) =>
        {
            Some(inner.tokens)
        }
        _ => None,
    })
}

/// The `Json<T>` request body type, if any, and the `Json<T>` response type.
fn body_types(func: &ItemFn) -> Result<(Option<syn::Type>, syn::Type), syn::Error> {
    let request_ty = func.sig.inputs.iter().find_map(|arg| match arg {
        syn::FnArg::Typed(pat) => crate::api_doc::unwrap_json_body(&pat.ty),
        syn::FnArg::Receiver(_) => None,
    });
    let response_ty = response_type(func).ok_or_else(|| {
        syn::Error::new_spanned(
            &func.sig,
            "#[endpoint] needs a JSON response: the handler must return `Json<T>`, \
             `AutumnResult<Json<T>>`, or `(StatusCode, Json<T>)`",
        )
    })?;
    Ok((request_ty, response_ty))
}

/// The element type of `Vec<T>`, or `None` for any other type.
fn list_element(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let last = path.path.segments.last()?;
    if last.ident != "Vec" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    match args.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

/// A body type: its descriptor name, its path as written, and whether it
/// is a JSON array.
///
/// `Vec<T>` has the shape of `T` on the wire, inside an array. The name and
/// the path are the element's, so a reader finds the element's descriptor.
struct Body {
    name: String,
    path: String,
    list: bool,
}

fn body(ty: &syn::Type) -> Body {
    let (inner, list) = list_element(ty).map_or((ty, false), |inner| (inner, true));
    Body {
        name: autumn_macros_support::schema::type_name_str(inner),
        path: type_path(inner),
        list,
    }
}

/// A type's path as written, with no generic arguments. A leading `::`
/// stays: `::shared::Item` names the crate even beside a local module
/// `shared`. Another kind of type gives its name.
fn type_path(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => {
            let joined = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            if path.path.leading_colon.is_some() {
                format!("::{joined}")
            } else {
                joined
            }
        }
        syn::Type::Group(group) => type_path(&group.elem),
        syn::Type::Paren(paren) => type_path(&paren.elem),
        _ => autumn_macros_support::schema::type_name_str(ty),
    }
}

/// Where an endpoint is, and what its route attribute and arguments say.
struct Site {
    krate: String,
    module: String,
    service: String,
    name: String,
    handler: String,
    marker: String,
    method: String,
    path: String,
}

/// This endpoint's JSON descriptor.
fn endpoint_descriptor(
    site: &Site,
    request_ty: Option<&syn::Type>,
    response_ty: &syn::Type,
) -> EndpointDescriptor {
    let request = request_ty.map_or_else(
        || Body {
            name: "NoBody".to_owned(),
            path: String::new(),
            list: false,
        },
        body,
    );
    let response = body(response_ty);
    EndpointDescriptor {
        service: site.service.clone(),
        name: site.name.clone(),
        endpoint_ident: site.marker.clone(),
        krate: site.krate.clone(),
        method: site.method.clone(),
        path: site.path.clone(),
        request_type: request.name,
        response_type: response.name,
        has_body: request_ty.is_some(),
        request_list: request.list,
        response_list: response.list,
        handler: site.handler.clone(),
        module: site.module.clone(),
        request_path: request.path,
        response_path: response.path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(attr: &str, item: &str) -> String {
        let attr: TokenStream = attr.parse().expect("attr parses");
        let item: TokenStream = item.parse().expect("item parses");
        endpoint_macro(attr, &item).to_string()
    }

    #[test]
    fn a_get_handler_gets_a_body_less_endpoint_impl() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[get(\"/items/{id}\")] async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> { todo!() }",
        );
        assert!(out.contains("pub struct get_item_endpoint"), "{out}");
        assert!(
            out.contains("type Request = :: autumn_web :: wire :: NoBody"),
            "{out}"
        );
        assert!(out.contains("type Response = Item"), "{out}");
        assert!(
            out.contains("const METHOD : & 'static str = \"GET\""),
            "{out}"
        );
        assert!(
            out.contains("const PATH : & 'static str = \"/items/{id}\""),
            "{out}"
        );
        assert!(out.contains("const HAS_BODY : bool = false"), "{out}");
        assert!(
            out.contains("async fn get_item"),
            "the handler is re-emitted: {out}"
        );
    }

    #[test]
    fn a_post_handler_takes_its_json_parameter_as_the_request() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[post(\"/items\")] async fn create(body: Json<NewItem>) -> AutumnResult<Json<Item>> { todo!() }",
        );
        assert!(out.contains("type Request = NewItem"), "{out}");
        assert!(out.contains("const HAS_BODY : bool = true"), "{out}");
        assert!(
            out.contains("const METHOD : & 'static str = \"POST\""),
            "{out}"
        );
    }

    #[test]
    fn a_validated_json_parameter_is_still_the_request() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[post(\"/items\")] async fn create(body: Valid<Json<NewItem>>) -> Json<Item> { todo!() }",
        );
        assert!(out.contains("type Request = NewItem"), "{out}");
    }

    #[test]
    fn an_explicit_name_renames_the_endpoint_and_its_marker() {
        let out = expand_str(
            "service = \"catalog\", name = \"fetch\"",
            "#[get(\"/items\")] async fn get_item() -> Json<Item> { todo!() }",
        );
        assert!(out.contains("pub struct fetch_endpoint"), "{out}");
        assert!(
            out.contains("const NAME : & 'static str = \"fetch\""),
            "{out}"
        );
    }

    #[test]
    fn a_handler_with_no_route_attribute_is_refused() {
        let out = expand_str(
            "service = \"catalog\"",
            "async fn get_item() -> Json<Item> { todo!() }",
        );
        assert!(
            out.contains("must sit directly above the route attribute"),
            "{out}"
        );
    }

    #[test]
    fn a_handler_with_no_json_response_is_refused() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[get(\"/items\")] async fn get_item() -> &'static str { todo!() }",
        );
        assert!(out.contains("must return `Json<T>`"), "{out}");
    }

    #[test]
    fn a_missing_service_name_is_refused() {
        let out = expand_str(
            "",
            "#[get(\"/items\")] async fn get_item() -> Json<Item> { todo!() }",
        );
        assert!(out.contains("needs a service name"), "{out}");
    }

    #[test]
    fn a_service_name_that_could_steer_a_file_write_is_refused() {
        for bad in ["../../etc/passwd", "cat/alog", "", "cat.alog"] {
            let out = expand_str(
                &format!("service = \"{bad}\""),
                "#[get(\"/items\")] async fn get_item() -> Json<Item> { todo!() }",
            );
            assert!(
                out.contains("must be a non-empty name") || out.contains("needs a service name"),
                "`{bad}` must be refused: {out}"
            );
        }
    }

    /// The name becomes a Rust identifier, so anything else would panic the
    /// macro rather than produce a diagnostic.
    #[test]
    fn an_endpoint_name_that_is_not_an_identifier_is_refused() {
        for bad in ["../oops", "get-item", "2fast", "", "get item"] {
            let out = expand_str(
                &format!("service = \"catalog\", name = \"{bad}\""),
                "#[get(\"/items\")] async fn get_item() -> Json<Item> { todo!() }",
            );
            assert!(
                out.contains("must be a Rust identifier"),
                "`{bad}` must be refused: {out}"
            );
        }
    }

    #[test]
    fn a_route_attribute_behind_cfg_attr_still_yields_its_method_and_path() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[cfg_attr(feature = \"x\", get(\"/items/{id}\"))] async fn get_item(id: Path<String>) -> Json<Item> { todo!() }",
        );
        assert!(
            out.contains("const METHOD : & 'static str = \"GET\""),
            "{out}"
        );
        assert!(
            out.contains("const PATH : & 'static str = \"/items/{id}\""),
            "{out}"
        );
    }

    #[test]
    fn an_unknown_argument_is_refused_rather_than_ignored() {
        let out = expand_str(
            "service = \"catalog\", verison = \"2\"",
            "#[get(\"/items\")] async fn get_item() -> Json<Item> { todo!() }",
        );
        assert!(out.contains("unknown #[endpoint] argument"), "{out}");
    }

    #[test]
    fn a_route_attribute_with_extra_arguments_still_yields_its_path() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[get(\"/items\", seo(title = \"Items\"))] async fn get_item() -> Json<Item> { todo!() }",
        );
        assert!(
            out.contains("const PATH : & 'static str = \"/items\""),
            "{out}"
        );
    }

    #[test]
    fn the_expansion_tracks_the_contract_dir_variable() {
        let out = expand_str(
            "service = \"catalog\"",
            "#[get(\"/items/{id}\")] pub async fn get_item(id: Path<String>) -> Json<Item> { todo!() }",
        );
        assert!(out.contains("option_env"), "{out}");
        assert!(out.contains("AUTUMN_CONTRACT_DIR"), "{out}");
    }

    fn descriptor_of(item: &str) -> EndpointDescriptor {
        let func: ItemFn = syn::parse_str(item).expect("fixture parses");
        let (request, response) = body_types(&func).expect("json response");
        let site = Site {
            krate: "k".to_owned(),
            module: String::new(),
            service: "catalog".to_owned(),
            name: "f".to_owned(),
            handler: func.sig.ident.to_string(),
            marker: "f_endpoint".to_owned(),
            method: "GET".to_owned(),
            path: "/x".to_owned(),
        };
        endpoint_descriptor(&site, request.as_ref(), &response)
    }

    #[test]
    fn a_list_response_records_its_element_and_the_array() {
        let d = descriptor_of("async fn f() -> Json<Vec<Item>> { todo!() }");
        assert_eq!(d.response_type, "Item");
        assert!(d.response_list);
        assert!(!d.has_body);
        assert_eq!(d.request_type, "NoBody");
    }

    #[test]
    fn the_descriptor_records_type_paths_as_written_and_the_handler() {
        let d = descriptor_of(
            "async fn f(b: Json<shared::NewItem>) -> AutumnResult<Json<::shared::Item>> { todo!() }",
        );
        assert_eq!(d.request_path, "shared::NewItem");
        assert_eq!(
            d.response_path, "::shared::Item",
            "an absolute path stays absolute"
        );
        assert_eq!(d.response_type, "Item");
        assert_eq!(d.handler, "f");
        let d = descriptor_of("async fn f() -> Json<Vec<crate::dto::Item>> { todo!() }");
        assert_eq!(d.response_path, "crate::dto::Item");
        assert!(d.response_list);
        assert_eq!(d.request_path, "", "no body: no path");
    }

    #[test]
    fn a_json_no_body_parameter_counts_as_a_body() {
        let d = descriptor_of("async fn f(b: Json<NoBody>) -> Json<Item> { todo!() }");
        assert_eq!(d.request_type, "NoBody");
        assert!(d.has_body);
        let d = descriptor_of("async fn f(b: Json<Vec<NewItem>>) -> Json<Item> { todo!() }");
        assert_eq!(d.request_type, "NewItem");
        assert!(d.request_list);
    }
}
