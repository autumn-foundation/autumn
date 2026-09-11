//! `wire_client!` — generate a typed client from a callee's endpoint markers.
//!
//! ```rust,ignore
//! wire_client! {
//!     name = CatalogClient,
//!     endpoints = [
//!         catalog::get_item_endpoint(id),
//!         catalog::create_item_endpoint,
//!     ],
//! }
//! ```
//!
//! Each entry names an endpoint marker and, in parentheses, the path
//! parameters its route takes. The method's request and response types come
//! from the marker's associated types, so they are the callee's real types and
//! the compiler checks every call against them.
//!
//! The declared path parameters are the one thing written twice. A const
//! assertion holds them to the endpoint's own `PATH`, so a route change breaks
//! the build here rather than producing a 404 in production.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitStr, Path, Token, bracketed, parenthesized};

/// One `catalog::get_item_endpoint(id)` entry.
struct EndpointEntry {
    path: Path,
    params: Vec<Ident>,
}

impl Parse for EndpointEntry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let path: Path = input.parse()?;
        let mut params = Vec::new();
        if input.peek(syn::token::Paren) {
            let inner;
            parenthesized!(inner in input);
            for ident in Punctuated::<Ident, Token![,]>::parse_terminated(&inner)? {
                params.push(ident);
            }
        }
        Ok(Self { path, params })
    }
}

/// The whole `wire_client! { … }` invocation.
struct ClientDef {
    name: Ident,
    endpoints: Vec<EndpointEntry>,
}

impl Parse for ClientDef {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut endpoints = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse::<Ident>()?),
                "endpoints" => {
                    let inner;
                    bracketed!(inner in input);
                    endpoints = Some(
                        Punctuated::<EndpointEntry, Token![,]>::parse_terminated(&inner)?
                            .into_iter()
                            .collect::<Vec<_>>(),
                    );
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "unknown wire_client! key `{other}`; expected `name` or `endpoints`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        let name = name.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "wire_client! needs a client name: `name = CatalogClient`",
            )
        })?;
        let endpoints = endpoints.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "wire_client! needs an endpoint list: `endpoints = [catalog::get_item_endpoint]`",
            )
        })?;
        if endpoints.is_empty() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "wire_client! needs at least one endpoint",
            ));
        }
        Ok(Self { name, endpoints })
    }
}

/// The const endpoint table `#[contract_checked]` asserts against.
///
/// A sibling of the client, so the user's own path to the client also reaches
/// it. One table rather than one type alias per endpoint: a call site names a
/// *method*, and only a by-name lookup can answer "this client does not
/// declare that method" without naming a type that does not exist.
#[must_use]
pub fn endpoint_table_ident(client: &Ident) -> Ident {
    format_ident!("__AUTUMN_WIRE_ENDPOINTS_{}", client)
}

/// The client method name for an endpoint marker — its ident minus the suffix.
fn method_ident(path: &Path) -> syn::Result<Ident> {
    let last = path.segments.last().ok_or_else(|| {
        syn::Error::new_spanned(path, "wire_client! needs an endpoint marker type path")
    })?;
    let name = last.ident.to_string();
    let stem = name
        .strip_suffix("_endpoint")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            syn::Error::new_spanned(
                &last.ident,
                format!(
                    "`{name}` is not an endpoint marker: `#[endpoint]` names its marker \
                 `<endpoint>_endpoint`"
                ),
            )
        })?;
    Ok(format_ident!("{}", stem))
}

pub fn wire_client_macro(input: TokenStream) -> TokenStream {
    match expand(input) {
        Ok(ts) => ts,
        Err(err) => err.to_compile_error(),
    }
}

fn expand(input: TokenStream) -> Result<TokenStream, syn::Error> {
    let def: ClientDef = syn::parse2(input)?;
    let client = &def.name;

    let mut methods = Vec::new();
    let mut rows = Vec::new();
    let mut assertions = Vec::new();

    for entry in &def.endpoints {
        let method = method_ident(&entry.path)?;
        assertions.push(path_assertion(client, &method, entry));
        rows.push(table_row(&method, entry));
        methods.push(endpoint_method(&method, entry));
    }
    let table = endpoint_table_ident(client);
    let table_doc = format!(
        "Endpoint table for `{client}`, generated by `wire_client!` and asserted against by \
         `#[contract_checked]`."
    );

    let client_doc = format!(
        "Typed client for the `{client}` service, generated by `wire_client!`.\n\n\
         Every method's request and response types come from the callee's own handler \
         signatures."
    );

    Ok(quote! {
        #(#assertions)*

        #[doc = #client_doc]
        #[derive(::core::clone::Clone)]
        pub struct #client {
            base_url: ::std::string::String,
            http: ::autumn_web::http::Client,
        }

        impl ::core::fmt::Debug for #client {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(::core::stringify!(#client))
                    .field("base_url", &self.base_url)
                    .finish_non_exhaustive()
            }
        }

        impl #client {
            /// Build a client pointed at `base_url`.
            #[must_use]
            pub fn new(
                base_url: impl ::core::convert::Into<::std::string::String>,
                http: ::autumn_web::http::Client,
            ) -> Self {
                Self { base_url: base_url.into(), http }
            }

            /// The service base URL this client calls.
            #[must_use]
            pub fn base_url(&self) -> &str {
                &self.base_url
            }

            #(#methods)*
        }

        #[doc = #table_doc]
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub const #table: &[::autumn_web::wire::ClientEndpoint] = &[#(#rows),*];
    })
}

/// The declared path parameters, as string literals.
fn param_literals(entry: &EndpointEntry) -> Vec<LitStr> {
    entry
        .params
        .iter()
        .map(|p| LitStr::new(&p.to_string(), p.span()))
        .collect()
}

/// Hold the declared path parameters to the endpoint's own route path.
fn path_assertion(client: &Ident, method: &Ident, entry: &EndpointEntry) -> TokenStream {
    let ep = &entry.path;
    let names = param_literals(entry);
    let message = format!(
        "wire contract broken in client `{client}`: it declares path parameters ({}) for endpoint \
         `{method}`, but that endpoint's route path no longer takes exactly those, in that order",
        names
            .iter()
            .map(LitStr::value)
            .collect::<Vec<_>>()
            .join(", "),
    );
    quote! {
        const _: () = ::core::assert!(
            ::autumn_web::wire::path_params_are(
                <#ep as ::autumn_web::wire::Endpoint>::PATH,
                &[#(#names),*],
            ),
            #message
        );
    }
}

/// One row of the client's endpoint table.
fn table_row(method: &Ident, entry: &EndpointEntry) -> TokenStream {
    let ep = &entry.path;
    let name = method.to_string();
    quote! {
        ::autumn_web::wire::ClientEndpoint {
            method: #name,
            response: <#ep as ::autumn_web::wire::Endpoint>::RESPONSE_FIELDS,
            request: <#ep as ::autumn_web::wire::Endpoint>::REQUEST_FIELDS,
            request_sent: <#ep as ::autumn_web::wire::Endpoint>::REQUEST_SENT_FIELDS,
        }
    }
}

/// One client method: the path parameters, then the request body.
fn endpoint_method(method: &Ident, entry: &EndpointEntry) -> TokenStream {
    let ep = &entry.path;
    let params = &entry.params;
    let names = param_literals(entry);
    let doc = format!("Call the `{method}` endpoint.");
    quote! {
        #[doc = #doc]
        ///
        /// # Errors
        /// [`::autumn_web::wire::WireError`] on transport failure, a non-2xx
        /// status, or an undecodable response body.
        pub async fn #method(
            &self,
            #(#params: impl ::core::fmt::Display,)*
            request: <#ep as ::autumn_web::wire::Endpoint>::Request,
        ) -> ::core::result::Result<
            <#ep as ::autumn_web::wire::Endpoint>::Response,
            ::autumn_web::wire::WireError,
        > {
            let __autumn_rendered = ::autumn_web::wire::render_path(
                <#ep as ::autumn_web::wire::Endpoint>::PATH,
                &[#((#names, &::std::string::ToString::to_string(&#params))),*],
            );
            ::autumn_web::wire::call::<#ep>(
                &self.http,
                &self.base_url,
                &__autumn_rendered,
                &request,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(src: &str) -> String {
        let input: TokenStream = src.parse().expect("input parses");
        wire_client_macro(input).to_string()
    }

    #[test]
    fn a_client_gets_one_method_per_endpoint() {
        let out = expand_str(
            "name = CatalogClient, endpoints = [catalog::get_item_endpoint(id), catalog::create_item_endpoint]",
        );
        assert!(out.contains("pub struct CatalogClient"), "{out}");
        assert!(out.contains("pub async fn get_item"), "{out}");
        assert!(out.contains("pub async fn create_item"), "{out}");
    }

    #[test]
    fn request_and_response_types_come_from_the_endpoint_marker() {
        let out = expand_str("name = CatalogClient, endpoints = [catalog::get_item_endpoint(id)]");
        assert!(
            out.contains(
                "< catalog :: get_item_endpoint as :: autumn_web :: wire :: Endpoint > :: Request"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "< catalog :: get_item_endpoint as :: autumn_web :: wire :: Endpoint > :: Response"
            ),
            "{out}"
        );
    }

    #[test]
    fn declared_path_parameters_are_asserted_against_the_endpoints_own_path() {
        let out = expand_str("name = CatalogClient, endpoints = [catalog::get_item_endpoint(id)]");
        assert!(out.contains("path_params_are"), "{out}");
        assert!(out.contains("\"id\""), "{out}");
        assert!(out.contains("declares path parameters"), "{out}");
    }

    #[test]
    fn the_client_gets_a_const_endpoint_table() {
        let out = expand_str(
            "name = CatalogClient, endpoints = [catalog::get_item_endpoint(id), catalog::create_item_endpoint]",
        );
        assert!(
            out.contains("pub const __AUTUMN_WIRE_ENDPOINTS_CatalogClient"),
            "{out}"
        );
        assert!(out.contains("method : \"get_item\""), "{out}");
        assert!(out.contains("method : \"create_item\""), "{out}");
        assert!(out.contains("REQUEST_SENT_FIELDS"), "{out}");
    }

    #[test]
    fn a_marker_without_the_endpoint_suffix_is_refused() {
        let out = expand_str("name = CatalogClient, endpoints = [catalog::get_item]");
        assert!(out.contains("is not an endpoint marker"), "{out}");
    }

    #[test]
    fn a_marker_named_only_by_the_suffix_is_refused() {
        assert!(
            expand_str("name = CatalogClient, endpoints = [catalog::_endpoint]")
                .contains("is not an endpoint marker")
        );
    }

    #[test]
    fn an_empty_endpoint_list_is_refused() {
        assert!(
            expand_str("name = CatalogClient, endpoints = []").contains("at least one endpoint")
        );
    }

    #[test]
    fn a_missing_name_is_refused() {
        assert!(
            expand_str("endpoints = [catalog::get_item_endpoint]").contains("needs a client name")
        );
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        assert!(
            expand_str("name = C, endpoitns = [a::b_endpoint]")
                .contains("unknown wire_client! key")
        );
    }
}
