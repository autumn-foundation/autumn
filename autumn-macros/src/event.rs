//! `#[event]` proc macro implementation.
//!
//! Marks a struct as a typed domain event: applies the serde derives the event
//! bus needs to ship the payload across the durable job queue, and implements
//! the `Event` trait with a stable `NAME`. Mirrors the ergonomics of `#[model]`
//! (which applies serde derives directly via `::serde::…`).

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser as _;
use syn::{DeriveInput, LitStr};

struct EventAttrs {
    name: Option<String>,
}

fn parse_event_args(attr: TokenStream) -> syn::Result<EventAttrs> {
    let mut result = EventAttrs { name: None };
    if attr.is_empty() {
        return Ok(result);
    }
    syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            let value: LitStr = meta.value()?.parse()?;
            result.name = Some(value.value());
            Ok(())
        } else {
            Err(meta.error("unsupported attribute: expected `name`"))
        }
    })
    .parse2(attr)?;
    Ok(result)
}

pub fn event_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_event_args(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let input: DeriveInput = match syn::parse2(item) {
        Ok(input) => input,
        Err(err) => return err.to_compile_error(),
    };

    let ident = &input.ident;
    let ident_str = ident.to_string();
    // Default the routing name to a module-qualified path so two event types
    // that share a short name (e.g. `Created`) in different modules never
    // collide on the registry's string key. An explicit `name = "..."` wins.
    let name_const = attrs.name.map_or_else(
        || quote! { ::std::concat!(::std::module_path!(), "::", #ident_str) },
        |name| quote! { #name },
    );
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    quote! {
        // Derive serde through the framework's re-export (and point serde's
        // generated code at it) so `#[event]` works for apps that depend only
        // on `autumn-web`, without a direct `serde` dependency.
        #[derive(
            ::autumn_web::reexports::serde::Serialize,
            ::autumn_web::reexports::serde::Deserialize,
            ::std::clone::Clone,
            ::std::fmt::Debug,
        )]
        #[serde(crate = "::autumn_web::reexports::serde")]
        #input

        impl #impl_generics ::autumn_web::events::Event for #ident #ty_generics #where_clause {
            const NAME: &'static str = #name_const;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    #[test]
    fn applies_serde_derives_and_implements_event() {
        let expanded = event_macro(
            quote! {},
            quote! {
                struct UserSignedUp {
                    user_id: i64,
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains("autumn_web :: reexports :: serde :: Serialize"),
            "{expanded}"
        );
        assert!(
            expanded.contains("autumn_web :: reexports :: serde :: Deserialize"),
            "{expanded}"
        );
        assert!(
            expanded.contains("serde (crate = \"::autumn_web::reexports::serde\")"),
            "{expanded}"
        );
        assert!(
            expanded.contains("impl :: autumn_web :: events :: Event for UserSignedUp"),
            "{expanded}"
        );
        // Default name is module-qualified to avoid cross-module collisions.
        assert!(
            expanded.contains("module_path ! ()") && expanded.contains("\"UserSignedUp\""),
            "{expanded}"
        );
    }

    #[test]
    fn name_attribute_overrides_event_name() {
        let expanded = event_macro(
            quote! { name = "user.signed_up" },
            quote! {
                struct UserSignedUp {
                    user_id: i64,
                }
            },
        )
        .to_string();
        assert!(
            expanded.contains("const NAME : & 'static str = \"user.signed_up\""),
            "{expanded}"
        );
    }

    #[test]
    fn rejects_unknown_attribute() {
        let expanded =
            event_macro(quote! { topic = "x" }, quote! { struct E { a: i64 } }).to_string();
        assert!(expanded.contains("compile_error"), "{expanded}");
    }
}
