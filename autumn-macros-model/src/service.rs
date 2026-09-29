//! `#[service]` proc macro implementation.
//!
//! Generates a concrete `XxxServiceImpl` struct with:
//! - Fields for each declared dependency (repositories, other services, etc.)
//! - `FromRequestParts` extractor impl for dependency injection into handlers
//!
//! The user writes all business logic in `impl XxxServiceImpl { ... }`.
//! Unlike `#[repository]`, no method bodies are generated — the macro
//! only provides the struct scaffolding and DI wiring.
//!
//! # When to use `#[service]` vs `#[repository]`
//!
//! - Single-model CRUD, validation, hooks → `#[repository]`
//! - Cross-model orchestration, non-DB side effects → `#[service]`

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, Ident, ItemTrait, TraitItem, Type};

struct DepField {
    name: Ident,
    ty: Type,
}

/// Extract dependency fields from the `fn deps(...)` declaration in the trait.
fn parse_deps(trait_def: &ItemTrait) -> syn::Result<Vec<DepField>> {
    let mut deps_method = None;

    for item in &trait_def.items {
        if let TraitItem::Fn(method) = item
            && method.sig.ident == "deps"
        {
            if deps_method.is_some() {
                return Err(syn::Error::new_spanned(
                    &method.sig.ident,
                    "duplicate `deps` declaration",
                ));
            }
            deps_method = Some(method);
        }
    }

    let method = deps_method.ok_or_else(|| {
        syn::Error::new_spanned(
            &trait_def.ident,
            "service trait must include a `fn deps(...)` declaration listing dependencies",
        )
    })?;

    let mut fields = Vec::new();
    for arg in &method.sig.inputs {
        match arg {
            FnArg::Receiver(_) => {
                return Err(syn::Error::new_spanned(arg, "`deps` must not take `self`"));
            }
            FnArg::Typed(pat_type) => {
                let name = match pat_type.pat.as_ref() {
                    syn::Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                    other => {
                        return Err(syn::Error::new_spanned(
                            other,
                            "expected a simple identifier for dependency name",
                        ));
                    }
                };
                fields.push(DepField {
                    name,
                    ty: (*pat_type.ty).clone(),
                });
            }
        }
    }

    Ok(fields)
}

pub fn service_macro(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let trait_def: ItemTrait = match syn::parse2(item) {
        Ok(t) => t,
        Err(err) => return err.to_compile_error(),
    };

    let deps = match parse_deps(&trait_def) {
        Ok(d) => d,
        Err(err) => return err.to_compile_error(),
    };

    let trait_name = &trait_def.ident;
    let impl_name = format_ident!("{}Impl", trait_name);
    let vis = &trait_def.vis;

    let field_names: Vec<&Ident> = deps.iter().map(|d| &d.name).collect();
    let field_types: Vec<&Type> = deps.iter().map(|d| &d.ty).collect();

    // Each dependency is extracted via its own FromRequestParts impl.
    // All Autumn-generated extractors (PgXxxRepository, other services, Db)
    // use AutumnError as their Rejection type, so the conversion is direct.
    let extractions: Vec<TokenStream> = deps
        .iter()
        .map(|d| {
            let name = &d.name;
            let ty = &d.ty;
            quote! {
                let #name = <#ty as ::autumn_web::reexports::axum::extract::FromRequestParts<
                    ::autumn_web::AppState
                >>::from_request_parts(parts, state)
                    .await
                    .map_err(|_| ::autumn_web::AutumnError::service_unavailable_msg(
                        concat!("Failed to extract dependency: ", stringify!(#name))
                    ))?;
            }
        })
        .collect();

    quote! {
        /// Generated service struct. Write your business logic in
        /// `impl #impl_name { ... }`.
        #[derive(Clone)]
        #vis struct #impl_name {
            #( pub #field_names: #field_types, )*
        }

        impl ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState> for #impl_name {
            type Rejection = ::autumn_web::AutumnError;

            async fn from_request_parts(
                parts: &mut ::autumn_web::reexports::http::request::Parts,
                state: &::autumn_web::AppState,
            ) -> Result<Self, Self::Rejection> {
                #( #extractions )*
                Ok(#impl_name { #( #field_names, )* })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_trait(input: &str) -> ItemTrait {
        syn::parse_str(input).expect("failed to parse trait")
    }

    #[test]
    fn parse_deps_extracts_fields() {
        let t = parse_trait(
            "pub trait OrderService {
                fn deps(order_repo: PgOrderRepository, email: EmailClient);
            }",
        );
        let deps = parse_deps(&t).unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "order_repo");
        assert_eq!(deps[1].name, "email");
    }

    #[test]
    fn parse_deps_empty_is_ok() {
        let t = parse_trait(
            "pub trait PureService {
                fn deps();
            }",
        );
        let deps = parse_deps(&t).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn parse_deps_missing_returns_error() {
        let t = parse_trait(
            "pub trait BadService {
                async fn do_thing(&self);
            }",
        );
        assert!(parse_deps(&t).is_err());
    }

    #[test]
    fn parse_deps_duplicate_returns_error() {
        let t = parse_trait(
            "pub trait BadService {
                fn deps(a: Foo);
                fn deps(b: Bar);
            }",
        );
        assert!(parse_deps(&t).is_err());
    }

    #[test]
    fn parse_deps_self_returns_error() {
        let t = parse_trait(
            "pub trait BadService {
                fn deps(&self, a: Foo);
            }",
        );
        assert!(parse_deps(&t).is_err());
    }

    #[test]
    fn naming_convention() {
        let t = parse_trait(
            "pub trait OrderService {
                fn deps();
            }",
        );
        let trait_name = &t.ident;
        let impl_name = format_ident!("{}Impl", trait_name);
        assert_eq!(impl_name.to_string(), "OrderServiceImpl");
    }

    #[test]
    fn generated_output_contains_struct_and_extractor() {
        let t = parse_trait(
            "pub trait OrderService {
                fn deps(repo: PgOrderRepo);
            }",
        );
        let output = service_macro(TokenStream::new(), quote! { #t });
        let output_str = output.to_string();
        assert!(
            output_str.contains("OrderServiceImpl"),
            "should contain struct name"
        );
        assert!(
            output_str.contains("FromRequestParts"),
            "should contain extractor impl"
        );
        assert!(output_str.contains("repo"), "should contain field name");
    }
}
