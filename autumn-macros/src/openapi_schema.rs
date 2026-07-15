//! `#[derive(OpenApiSchema)]` — a standalone derive that gives a plain struct a
//! field-accurate `OpenApiSchema` impl (issue #1972).
//!
//! Before this derive, the only automatic `OpenApiSchema` impls came from
//! `#[model]` codegen and the primitive macro impls, so any other handler-arg
//! struct (a `Query<T>` param struct or a non-`#[model]` `Json<T>` body) had to
//! carry a hand-written impl plus an `OpenApiConfig::register_schema` call — or
//! its `OpenAPI` / MCP `inputSchema` degraded to a generic
//! `{"type":"object","title":"X"}` placeholder.
//!
//! This derive mirrors the schema `#[model]` already generates
//! (`crate::model::emit_schema_fn_body`): each field becomes a JSON-schema
//! property and every non-`Option` field is `required`. It additionally submits
//! the schema into the compile-time `DerivedSchemaDescriptor` inventory that the
//! spec/MCP back-fill loops consult, so a referenced struct resolves to its real
//! schema with no manual registration.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

pub fn derive_openapi_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    // The emitted impl uses the bare type name for both `schema_name()` and the
    // inventory descriptor, and the descriptor's `schema` field is a plain
    // `fn() -> Value` — none of which can carry generic parameters. Reject
    // generics with a clear message rather than emitting an impl that fails to
    // compile downstream.
    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "#[derive(OpenApiSchema)] does not support generic types",
        )
        .to_compile_error()
        .into();
    }

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    &input,
                    "#[derive(OpenApiSchema)] is only supported on structs with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(
                &input,
                "#[derive(OpenApiSchema)] is only supported on structs",
            )
            .to_compile_error()
            .into();
        }
    };

    // `emit_schema_fn_body` expects `&[&&Field]` (it was written against the
    // `Vec<&&Field>` the model macro collects); build that shape here.
    let field_refs: Vec<&syn::Field> = fields.iter().collect();
    let field_ref_refs: Vec<&&syn::Field> = field_refs.iter().collect();
    let schema_body = crate::model::emit_schema_fn_body(&field_ref_refs, false);

    quote! {
        impl ::autumn_web::openapi::OpenApiSchema for #name {
            fn schema_name() -> &'static str {
                ::core::stringify!(#name)
            }
            fn schema() -> ::autumn_web::reexports::serde_json::Value {
                #schema_body
            }
        }

        // Advertise the derived schema by name so the OpenAPI/MCP schema
        // back-fill resolves it instead of the generic object placeholder.
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::openapi::DerivedSchemaDescriptor {
                name: ::core::stringify!(#name),
                schema: <#name as ::autumn_web::openapi::OpenApiSchema>::schema,
            }
        }
    }
    .into()
}
