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
        // Both reach a descriptor's file name, so anything outside this set
        // could steer the write out of the contract directory.
        for (label, value) in [("service", Some(&service)), ("name", name.as_ref())] {
            let Some(value) = value else { continue };
            if value.is_empty() || !value.chars().all(is_name_char) {
                return Err(syn::Error::new(
                    Span::call_site(),
                    format!(
                        "#[endpoint] {label} must be a non-empty name of letters, digits, `_` \
                         or `-`, and is `{value}`"
                    ),
                ));
            }
        }
        Ok(Self { service, name })
    }
}

/// Characters an endpoint's service and name may use — the set that is safe in
/// a file name on every platform the workspace builds on.
const fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
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

    let request_ty = func.sig.inputs.iter().find_map(|arg| match arg {
        syn::FnArg::Typed(pat) => crate::api_doc::unwrap_json_body(&pat.ty),
        syn::FnArg::Receiver(_) => None,
    });
    let response_ty = response_type(&func).ok_or_else(|| {
        syn::Error::new_spanned(
            &func.sig,
            "#[endpoint] needs a JSON response: the handler must return `Json<T>`, \
             `AutumnResult<Json<T>>`, or `(StatusCode, Json<T>)`",
        )
    })?;

    let has_body = request_ty.is_some();
    let request_tokens = request_ty.as_ref().map_or_else(
        || quote! { ::autumn_web::wire::NoBody },
        |ty| quote! { #ty },
    );

    write_descriptor(
        &args.service,
        &name,
        &marker,
        &method,
        &path,
        request_ty.as_ref(),
        &response_ty,
    );

    let doc = format!(
        "Wire contract for the `{}.{}` endpoint (`{method} {path}`).\n\n\
         Generated by `#[endpoint]`. Name it from `wire_client!` to call it.",
        args.service, name
    );
    let service = &args.service;

    Ok(quote! {
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
        let path: LitStr = attr.parse_args_with(|input: syn::parse::ParseStream<'_>| {
            let lit: LitStr = input.parse()?;
            // A route attribute carries more than a path (`seo(...)`, and so
            // on). Only the first literal is wanted here.
            input.parse::<TokenStream>()?;
            Ok(lit)
        })?;
        return Ok(((*method).to_owned(), path.value()));
    }
    Err(syn::Error::new_spanned(
        &func.sig,
        "#[endpoint] must sit directly above the route attribute (`#[get]`, `#[post]`, \
         `#[put]`, `#[patch]`, `#[delete]`): it reads the method and path from it, and \
         the handler's signature before the route macro rewrites it",
    ))
}

/// Write this endpoint's JSON descriptor, if a contract directory resolves.
fn write_descriptor(
    service: &str,
    name: &str,
    marker: &Ident,
    method: &str,
    path: &str,
    request_ty: Option<&syn::Type>,
    response_ty: &syn::Type,
) {
    let Some(dir) = store::contract_dir() else {
        return;
    };
    let krate = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    store::write_endpoint(
        &dir,
        &EndpointDescriptor {
            service: service.to_owned(),
            name: name.to_owned(),
            endpoint_ident: marker.to_string(),
            krate,
            method: method.to_owned(),
            path: path.to_owned(),
            request_type: request_ty
                .map_or_else(|| "NoBody".to_owned(), crate::schema::type_name_str),
            response_type: crate::schema::type_name_str(response_ty),
        },
    );
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

    #[test]
    fn an_endpoint_name_that_could_steer_a_file_write_is_refused() {
        let out = expand_str(
            "service = \"catalog\", name = \"../oops\"",
            "#[get(\"/items\")] async fn get_item() -> Json<Item> { todo!() }",
        );
        assert!(out.contains("must be a non-empty name"), "{out}");
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
}
