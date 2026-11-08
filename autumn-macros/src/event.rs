//! `#[event]` proc macro implementation.
//!
//! Marks a struct as a typed domain event: applies the serde derives the event
//! bus needs to ship the payload across the durable job queue, and implements
//! the `Event` trait with a stable `NAME` and a schema `VERSION`. Mirrors the
//! ergonomics of `#[model]` (which applies serde derives directly via
//! `::serde::…`).
//!
//! The payload also gets a wire shape, as `#[derive(WireShape)]` gives it, and
//! a JSON descriptor. A `#[listener]` checks its reads against that shape.
//! `#[event(opaque)]` turns the shape off.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser as _;
use syn::{DeriveInput, LitInt, LitStr};

use crate::wire::ir::{EventDescriptor, WireTypeDescriptor};
use crate::wire::{shape, store};

struct EventAttrs {
    name: Option<String>,
    version: u32,
    opaque: bool,
}

fn parse_event_args(attr: TokenStream) -> syn::Result<EventAttrs> {
    let mut result = EventAttrs {
        name: None,
        version: 1,
        opaque: false,
    };
    if attr.is_empty() {
        return Ok(result);
    }
    syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            let value: LitStr = meta.value()?.parse()?;
            result.name = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("version") {
            let value: LitInt = meta.value()?.parse()?;
            let version = value.base10_parse::<u32>()?;
            if version == 0 {
                return Err(syn::Error::new_spanned(
                    value,
                    "#[event] version starts at 1",
                ));
            }
            result.version = version;
            Ok(())
        } else if meta.path.is_ident("opaque") {
            result.opaque = true;
            Ok(())
        } else {
            Err(meta.error("unsupported attribute: expected `name`, `version`, or `opaque`"))
        }
    })
    .parse2(attr)?;
    Ok(result)
}

pub fn event_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    // The descriptor key covers the arguments too: two events with one body
    // in two modules differ only in their `name`.
    let attr_text = attr.to_string();
    let attrs = match parse_event_args(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let input: DeriveInput = match syn::parse2(item) {
        Ok(input) => input,
        Err(err) => return err.to_compile_error(),
    };

    // A refusal still emits the struct, its serde derives and its `Event`
    // impl, so the uses of the type add no errors of their own.
    let mut refusal = version_refusal(&attrs);

    let ident = &input.ident;
    let ident_str = ident.to_string();
    let version = attrs.version;
    // Version 1 keeps the bare name, so an event that adds `version` keeps
    // its topic. A later version adds `.vN`, so two versions of one event
    // live side by side on the bus.
    let suffix = if version > 1 {
        format!(".v{version}")
    } else {
        String::new()
    };
    // Default the routing name to a module-qualified path so two event types
    // that share a short name (e.g. `Created`) in different modules never
    // collide on the registry's string key. An explicit `name = "..."` wins.
    let name_const = attrs.name.as_ref().map_or_else(
        || quote! { ::std::concat!(::std::module_path!(), "::", #ident_str, #suffix) },
        |name| {
            let name = format!("{name}{suffix}");
            quote! { #name }
        },
    );
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let (wire, shape_const, shape) = payload_shape(&attrs, &input, &mut refusal);
    let key = autumn_macros_support::wire::stable_key(&format!("{attr_text} {}", quote!(#input)));
    // A refused event writes no descriptor: the build fails anyway.
    let written = if refusal.is_empty() {
        write_descriptor(&attrs, &ident_str, shape, &key)
    } else {
        TokenStream::new()
    };
    let track = store::track_contract_dir();

    // `#[serde(crate = "...")]`'s value is a string serde parses into a path
    // itself (it never passes through this crate's own generic
    // `::autumn_web` token rewrite — see `crate_path`'s module doc, #1828),
    // so it must be built from the actively resolved crate name directly.
    let serde_crate_path = format!(
        "::{}::reexports::serde",
        autumn_macros_support::crate_path::escaped_target_path_segment(
            &autumn_macros_support::crate_path::current_target()
        )
    );

    quote! {
        #refusal

        // Derive serde through the framework's re-export (and point serde's
        // generated code at it) so `#[event]` works for apps that depend only
        // on `autumn-web`, without a direct `serde` dependency.
        #[derive(
            ::autumn_web::reexports::serde::Serialize,
            ::autumn_web::reexports::serde::Deserialize,
            ::std::clone::Clone,
            ::std::fmt::Debug,
        )]
        #[serde(crate = #serde_crate_path)]
        #input

        #track
        #written
        #wire

        impl #impl_generics ::autumn_web::events::Event for #ident #ty_generics #where_clause {
            const NAME: &'static str = #name_const;
            const VERSION: u32 = #version;
            #shape_const
        }
    }
}

/// The error for a `name` that already ends in `.vN` while `version` adds
/// one, else nothing.
fn version_refusal(attrs: &EventAttrs) -> TokenStream {
    match &attrs.name {
        Some(name) if attrs.version > 1 && has_version_suffix(name) => {
            let message = format!(
                "#[event] name `{name}` already ends in a version. Write the base name: \
                 `version = {}` adds `.v{}`",
                attrs.version, attrs.version
            );
            syn::Error::new(proc_macro2::Span::call_site(), message).to_compile_error()
        }
        _ => TokenStream::new(),
    }
}

/// The payload's wire shape: the `WireShape` tokens, the `SHAPE` const, and
/// the descriptor shape.
///
/// An opaque event has none. A shape `#[derive(WireShape)]` refuses adds its
/// error, with a pointer to the opt-out, to `refusal`, and has none either.
fn payload_shape(
    attrs: &EventAttrs,
    input: &DeriveInput,
    refusal: &mut TokenStream,
) -> (TokenStream, TokenStream, WireTypeDescriptor) {
    let none = || {
        (
            TokenStream::new(),
            TokenStream::new(),
            WireTypeDescriptor::default(),
        )
    };
    if attrs.opaque {
        return none();
    }
    match shape::wire_type_descriptor(input) {
        Ok(described) => (
            shape::wire_shape_tokens(&input.ident, &described),
            quote! {
                const SHAPE: ::std::option::Option<::autumn_web::wire::EventShape> =
                    ::std::option::Option::Some(::autumn_web::wire::EventShape {
                        serialized: <Self as ::autumn_web::wire::WireShape>::SERIALIZED,
                        deserialized: <Self as ::autumn_web::wire::WireShape>::DESERIALIZED,
                    });
            },
            described,
        ),
        Err(err) => {
            refusal.extend(
                syn::Error::new(
                    err.span(),
                    format!(
                        "{err}. To publish this payload without a wire contract, write \
                         `#[event(opaque)]`."
                    ),
                )
                .to_compile_error(),
            );
            none()
        }
    }
}

/// Write the event's descriptor, if a contract directory resolves.
fn write_descriptor(
    attrs: &EventAttrs,
    type_name: &str,
    shape: WireTypeDescriptor,
    key: &str,
) -> TokenStream {
    let Some(dir) = store::contract_dir() else {
        return TokenStream::new();
    };
    store::write_outcome(store::write_event(
        &dir,
        key,
        &EventDescriptor {
            krate: std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
            type_name: type_name.to_owned(),
            name: attrs.name.clone(),
            version: attrs.version,
            opaque: attrs.opaque,
            shape,
        },
    ))
}

/// Whether `name` ends in `.v<digits>`.
fn has_version_suffix(name: &str) -> bool {
    name.rsplit_once(".v")
        .is_some_and(|(_, tail)| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
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

    #[test]
    fn an_event_derives_its_wire_shape() {
        let expanded = event_macro(
            quote! {},
            quote! { struct ContactUpdated { id: i64, #[serde(skip_serializing)] email: String } },
        )
        .to_string();
        assert!(
            expanded.contains("impl :: autumn_web :: wire :: WireShape for ContactUpdated"),
            "{expanded}"
        );
        assert!(expanded.contains("const SHAPE"), "{expanded}");
        assert!(
            expanded.contains("< Self as :: autumn_web :: wire :: WireShape > :: SERIALIZED"),
            "{expanded}"
        );
        assert!(expanded.contains("AUTUMN_CONTRACT_DIR"), "{expanded}");
    }

    #[test]
    fn version_one_keeps_the_bare_name() {
        let expanded = event_macro(
            quote! { name = "contact.updated", version = 1 },
            quote! { struct ContactUpdated { id: i64 } },
        )
        .to_string();
        assert!(
            expanded.contains("const NAME : & 'static str = \"contact.updated\""),
            "{expanded}"
        );
        assert!(
            expanded.contains("const VERSION : u32 = 1u32"),
            "{expanded}"
        );
    }

    #[test]
    fn a_later_version_adds_a_suffix_to_the_name() {
        let expanded = event_macro(
            quote! { name = "contact.updated", version = 2 },
            quote! { struct ContactUpdatedV2 { id: i64 } },
        )
        .to_string();
        assert!(
            expanded.contains("const NAME : & 'static str = \"contact.updated.v2\""),
            "{expanded}"
        );
        assert!(
            expanded.contains("const VERSION : u32 = 2u32"),
            "{expanded}"
        );
    }

    #[test]
    fn a_later_version_of_a_default_name_adds_the_suffix_too() {
        let expanded = event_macro(
            quote! { version = 3 },
            quote! { struct ContactUpdated { id: i64 } },
        )
        .to_string();
        assert!(expanded.contains("\".v3\""), "{expanded}");
    }

    #[test]
    fn version_zero_is_refused() {
        let expanded =
            event_macro(quote! { version = 0 }, quote! { struct E { a: i64 } }).to_string();
        assert!(expanded.contains("compile_error"), "{expanded}");
        assert!(expanded.contains("starts at 1"), "{expanded}");
    }

    #[test]
    fn an_event_refuses_what_wire_shape_refuses() {
        let expanded = event_macro(
            quote! {},
            quote! { struct E { #[serde(flatten)] extra: Extra } },
        )
        .to_string();
        assert!(expanded.contains("compile_error"), "{expanded}");
        assert!(expanded.contains("flatten"), "{expanded}");
        assert!(
            expanded.contains("opaque"),
            "the refusal names the opt-out: {expanded}"
        );
        let generic = event_macro(quote! {}, quote! { struct E<T> { a: T } }).to_string();
        assert!(generic.contains("compile_error"), "{generic}");
        let an_enum = event_macro(quote! {}, quote! { enum E { A } }).to_string();
        assert!(an_enum.contains("compile_error"), "{an_enum}");
    }

    #[test]
    fn a_name_with_a_version_suffix_and_a_version_is_refused() {
        let expanded = event_macro(
            quote! { name = "order.placed.v2", version = 2 },
            quote! { struct OrderPlaced { id: i64 } },
        )
        .to_string();
        assert!(expanded.contains("already ends in a version"), "{expanded}");
        assert!(
            expanded.contains("struct OrderPlaced"),
            "the struct stays: {expanded}"
        );
        // Version 1 leaves the name alone, so a hand-versioned name is fine.
        let v1 = event_macro(
            quote! { name = "order.placed.v2" },
            quote! { struct OrderPlaced { id: i64 } },
        )
        .to_string();
        assert!(!v1.contains("compile_error"), "{v1}");
        assert!(
            has_version_suffix("a.v12")
                && !has_version_suffix("a.vx")
                && !has_version_suffix("a.v")
        );
    }

    #[test]
    fn the_descriptor_key_covers_the_arguments() {
        let key = |attr: TokenStream| {
            let attr_text = attr.to_string();
            let input = quote! { pub struct Created { pub id: i64 } };
            autumn_macros_support::wire::stable_key(&format!("{attr_text} {input}"))
        };
        assert_ne!(
            key(quote! { name = "a.created" }),
            key(quote! { name = "b.created" })
        );
    }

    #[test]
    fn a_refused_event_keeps_its_struct() {
        let expanded = event_macro(quote! {}, quote! { enum Status { Open } }).to_string();
        assert!(expanded.contains("compile_error"), "{expanded}");
        assert!(expanded.contains("enum Status"), "{expanded}");
        // The serde derives and the `Event` impl stay, so a `#[serde]`
        // attribute and the uses of the type add no errors.
        assert!(expanded.contains("serde :: Serialize"), "{expanded}");
        assert!(
            expanded.contains(":: autumn_web :: events :: Event for Status"),
            "{expanded}"
        );
        assert!(!expanded.contains("const SHAPE"), "{expanded}");
    }

    #[test]
    fn an_opaque_event_has_no_wire_shape() {
        let expanded = event_macro(
            quote! { opaque },
            quote! { struct Page<T> { #[serde(flatten)] items: T } },
        )
        .to_string();
        assert!(!expanded.contains("compile_error"), "{expanded}");
        assert!(!expanded.contains("WireShape"), "{expanded}");
        assert!(!expanded.contains("const SHAPE"), "{expanded}");
        assert!(
            expanded.contains("impl < T > :: autumn_web :: events :: Event for Page < T >"),
            "{expanded}"
        );
    }
}
