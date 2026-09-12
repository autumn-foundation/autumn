//! `#[derive(WireShape)]` — record a DTO's serde-visible field shape.
//!
//! The derive emits two const tables: what the type puts on the wire, and what
//! it takes off it. They differ, and the difference is the point — a field
//! carrying `#[serde(skip_serializing)]` is accepted but never produced, so a
//! caller that reads it is broken even though the code compiles.
//!
//! Anything whose wire shape cannot be read off the struct is refused rather
//! than guessed at: a half-true table is worse than none, because the whole
//! contract check rests on it.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, FieldsNamed};

use crate::schema;
use crate::wire::ir::{TypeDescriptor, WireFieldDescriptor, WireTypeDescriptor};
use crate::wire::store;

/// One field's shape in one direction, ready to emit or to serialize.
struct Direction {
    fields: Vec<WireFieldDescriptor>,
}

pub fn derive_wire_shape(input: &DeriveInput) -> Result<TokenStream, syn::Error> {
    let name = &input.ident;
    let shape = wire_type_descriptor(input)?;
    let serialized = field_tokens(&shape.serialized);
    let deserialized = field_tokens(&shape.deserialized);
    let type_name = &shape.name;
    write_descriptor(&shape);

    Ok(quote! {
        impl ::autumn_web::wire::WireShape for #name {
            const TYPE_NAME: &'static str = #type_name;
            const SERIALIZED: &'static [::autumn_web::wire::WireField] = &[#(#serialized),*];
            const DESERIALIZED: &'static [::autumn_web::wire::WireField] = &[#(#deserialized),*];
        }
    })
}

/// Write this type's JSON descriptor, if a contract directory resolves.
///
/// Best effort: the descriptor only enriches a diagnostic, so a build that
/// cannot write it still holds the contract through the const tables above.
fn write_descriptor(shape: &WireTypeDescriptor) {
    let Some(dir) = store::contract_dir() else {
        return;
    };
    store::write_type(
        &dir,
        &TypeDescriptor {
            krate: std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
            shape: shape.clone(),
        },
    );
}

/// Read a type's serde-visible shape, or say why it cannot be read.
///
/// Shared with `#[endpoint]`, which needs the same shape for the JSON
/// descriptor it writes.
pub fn wire_type_descriptor(input: &DeriveInput) -> Result<WireTypeDescriptor, syn::Error> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "#[derive(WireShape)] does not support generic types: one contract must describe one \
             concrete wire shape",
        ));
    }
    let named = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => Some(named),
            Fields::Unit => None,
            Fields::Unnamed(fields) => {
                return Err(syn::Error::new_spanned(
                    fields,
                    "#[derive(WireShape)] needs named fields: a tuple struct has no field names \
                     for a call site to read or set",
                ));
            }
        },
        Data::Enum(_) | Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "#[derive(WireShape)] applies to structs: an enum has no single field set to \
                 check a call site against",
            ));
        }
    };

    // A container attribute that replaces or reshapes the object makes every
    // field row below a fiction. The module's rule is to refuse what it cannot
    // read rather than guess, and these cannot be read off the struct at all.
    if let Some(word) = schema::serde_bare_word(&input.attrs, &["transparent"]) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            format!(
                "#[derive(WireShape)] refuses `#[serde({word})]`: the type serializes as its one \
                 field's value, so there is no object on the wire to describe"
            ),
        ));
    }
    if let Some(key) = schema::serde_valued_key(&input.attrs, &["into", "from", "try_from"]) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            format!(
                "#[derive(WireShape)] refuses `#[serde({key} = …)]`: the wire shape is the other \
                 type's, and this struct's fields are not on the wire"
            ),
        ));
    }
    if let Some(key) = schema::serde_valued_key(&input.attrs, &["tag"]) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            format!(
                "#[derive(WireShape)] refuses `#[serde({key} = …)]` on a struct: it adds a tag key \
                 to the wire that is not a field of this type"
            ),
        ));
    }

    for key in ["rename", "rename_all"] {
        if schema::serde_split_rename(&input.attrs, key).is_some() {
            return Err(syn::Error::new_spanned(
                &input.ident,
                format!(
                    "#[derive(WireShape)] refuses a split `#[serde({key}(serialize = …, \
                     deserialize = …))]`: the two directions carry different wire names, so no \
                     single descriptor is true of both"
                ),
            ));
        }
    }

    let rename_all = schema::serde_rename_all_serialize_rule(&input.attrs);
    let container_default = schema::has_serde_default(&input.attrs);
    let closed = schema::serde_bare_word(&input.attrs, &["deny_unknown_fields"]).is_some();
    let Some(named) = named else {
        return Ok(WireTypeDescriptor {
            name: input.ident.to_string(),
            serialized: Vec::new(),
            deserialized: Vec::new(),
            closed,
        });
    };
    let (serialized, deserialized) = directions(named, rename_all.as_deref(), container_default)?;
    Ok(WireTypeDescriptor {
        name: input.ident.to_string(),
        serialized: serialized.fields,
        deserialized: deserialized.fields,
        closed,
    })
}

/// Split a struct's fields into what it produces and what it accepts.
fn directions(
    named: &FieldsNamed,
    rename_all: Option<&str>,
    container_default: bool,
) -> Result<(Direction, Direction), syn::Error> {
    let mut serialized = Vec::new();
    let mut deserialized = Vec::new();

    for field in &named.named {
        // `skip` first: `#[serde(skip, flatten)]` is a plain skip to serde —
        // the field is on neither wire — so there is nothing to refuse.
        // Asked one word at a time on purpose: `schema::serde_bare_word`
        // returns only the LAST word it matched, so asking it for both at once
        // cannot tell "one of them" from "both" — and both together is exactly
        // `skip`, a field on neither wire.
        let skip_serializing =
            schema::serde_bare_word(&field.attrs, &["skip_serializing"]).is_some();
        let skip_deserializing =
            schema::serde_bare_word(&field.attrs, &["skip_deserializing"]).is_some();
        if schema::serde_bare_word(&field.attrs, &["skip"]).is_some()
            || (skip_serializing && skip_deserializing)
        {
            continue;
        }
        if schema::serde_bare_word(&field.attrs, &["flatten"]).is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "#[derive(WireShape)] refuses `#[serde(flatten)]`: the flattened keys are not \
                 visible here, so the descriptor would claim a shape it cannot see",
            ));
        }
        if schema::serde_split_rename(&field.attrs, "rename").is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "#[derive(WireShape)] refuses a split `#[serde(rename(serialize = …, \
                 deserialize = …))]`: the two directions carry different wire names",
            ));
        }
        let Some(rust_name) = field.ident.as_ref().map(ToString::to_string) else {
            continue;
        };
        let rust_name = rust_name
            .strip_prefix("r#")
            .unwrap_or(&rust_name)
            .to_owned();
        let Some(wire_name) = schema::schema_property_name(field, rename_all) else {
            continue;
        };
        let ty = render_type(&field.ty);
        let optional = schema::is_option_type(&field.ty);
        let defaulted = container_default || schema::has_serde_default(&field.attrs);
        // `#[serde(with)]` / `#[serde(deserialize_with)]` replace the whole
        // field deserializer, and serde's generated code then returns
        // `missing_field` for an absent key instead of the `Option` shortcut.
        // So the key is mandatory even on an `Option<T>`, unless a default
        // fills it in.
        let custom_de =
            schema::serde_valued_key(&field.attrs, &["with", "deserialize_with"]).is_some();

        if !skip_serializing {
            serialized.push(WireFieldDescriptor {
                rust_name: rust_name.clone(),
                wire_name: wire_name.clone(),
                ty: ty.clone(),
                // A conditionally-skipped field may be absent from a response,
                // so a caller cannot count on it arriving.
                required: !schema::field_has_skip_serializing_if(field),
                aliases: Vec::new(),
            });
        }
        if !skip_deserializing {
            deserialized.push(WireFieldDescriptor {
                rust_name,
                wire_name,
                ty,
                // `Option<T>`, a field default, or a container default each
                // let the key be absent from a request body — but a custom
                // deserializer takes the `Option` shortcut away.
                required: !defaulted && (custom_de || !optional),
                aliases: serde_aliases(field),
            });
        }
    }

    Ok((
        Direction { fields: serialized },
        Direction {
            fields: deserialized,
        },
    ))
}

/// Every `#[serde(alias = "…")]` on a field, in source order.
fn serde_aliases(field: &syn::Field) -> Vec<String> {
    let mut aliases = Vec::new();
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("alias") {
                if let Ok(value) = meta.value()
                    && let Ok(syn::Lit::Str(lit)) = value.parse::<syn::Lit>()
                {
                    aliases.push(lit.value());
                }
            } else if let Ok(value) = meta.value() {
                let _: syn::Result<syn::Lit> = value.parse();
            } else if meta.input.peek(syn::token::Paren) {
                let content;
                syn::parenthesized!(content in meta.input);
                let _: proc_macro2::TokenStream = content.parse()?;
            }
            Ok(())
        });
    }
    aliases
}

/// A field's type as written, normalized to one canonical spelling.
///
/// Descriptors are compared across builds, so `Option<String>` and
/// `Option < String >` must not read as two different types. A space is kept
/// only where two words meet (`dyn Trait`, `&'a mut T`).
fn render_type(ty: &syn::Type) -> String {
    let mut out = String::new();
    let mut prev_word = false;
    append_tokens(quote!(#ty), &mut out, &mut prev_word);
    out
}

/// Flatten a token stream into `out`, descending into delimiter groups.
fn append_tokens(tokens: TokenStream, out: &mut String, prev_word: &mut bool) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Group(group) => {
                let (open, close) = match group.delimiter() {
                    proc_macro2::Delimiter::Parenthesis => ("(", ")"),
                    proc_macro2::Delimiter::Bracket => ("[", "]"),
                    proc_macro2::Delimiter::Brace => ("{", "}"),
                    proc_macro2::Delimiter::None => ("", ""),
                };
                out.push_str(open);
                *prev_word = false;
                append_tokens(group.stream(), out, prev_word);
                out.push_str(close);
                *prev_word = false;
            }
            other => {
                let text = other.to_string();
                let is_word = text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if is_word && *prev_word {
                    out.push(' ');
                }
                out.push_str(&text);
                *prev_word = is_word;
            }
        }
    }
}

/// Emit the const `WireField` initializers for one direction.
fn field_tokens(fields: &[WireFieldDescriptor]) -> Vec<TokenStream> {
    fields
        .iter()
        .map(|f| {
            let (rust_name, wire_name, ty, required) =
                (&f.rust_name, &f.wire_name, &f.ty, f.required);
            quote! {
                ::autumn_web::wire::WireField {
                    rust_name: #rust_name,
                    wire_name: #wire_name,
                    ty: #ty,
                    required: #required,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(src: &str) -> WireTypeDescriptor {
        let input: DeriveInput = syn::parse_str(src).expect("fixture parses");
        wire_type_descriptor(&input).expect("fixture has a readable shape")
    }

    fn refusal(src: &str) -> String {
        let input: DeriveInput = syn::parse_str(src).expect("fixture parses");
        wire_type_descriptor(&input)
            .expect_err("fixture must be refused")
            .to_string()
    }

    fn names(fields: &[WireFieldDescriptor]) -> Vec<&str> {
        fields.iter().map(|f| f.rust_name.as_str()).collect()
    }

    fn required(fields: &[WireFieldDescriptor], name: &str) -> bool {
        fields
            .iter()
            .find(|f| f.rust_name == name)
            .unwrap_or_else(|| panic!("no field `{name}`"))
            .required
    }

    #[test]
    fn a_field_type_is_recorded_in_one_canonical_spelling() {
        let s = shape(
            "struct T { a: Option<String>, b: Vec<std::collections::HashMap<String, u32>>, c: (u8, u8) }",
        );
        let types: Vec<&str> = s.serialized.iter().map(|f| f.ty.as_str()).collect();
        assert_eq!(
            types,
            [
                "Option<String>",
                "Vec<std::collections::HashMap<String,u32>>",
                "(u8,u8)"
            ]
        );
    }

    #[test]
    fn a_plain_struct_has_the_same_shape_in_both_directions() {
        let s = shape("struct Item { id: String, name: String }");
        assert_eq!(names(&s.serialized), ["id", "name"]);
        assert_eq!(names(&s.deserialized), ["id", "name"]);
        assert!(required(&s.deserialized, "id"));
    }

    #[test]
    fn an_option_field_is_not_required_on_a_request() {
        let s = shape("struct NewItem { name: String, note: Option<String> }");
        assert!(required(&s.deserialized, "name"));
        assert!(!required(&s.deserialized, "note"));
        // It is still always produced: serde writes `null`, not nothing.
        assert!(required(&s.serialized, "note"));
    }

    #[test]
    fn a_field_default_makes_a_request_field_optional() {
        let s = shape("struct NewItem { #[serde(default)] tier: String }");
        assert!(!required(&s.deserialized, "tier"));
    }

    #[test]
    fn a_container_default_makes_every_request_field_optional() {
        let s = shape("#[serde(default)] struct NewItem { a: String, b: String }");
        assert!(!required(&s.deserialized, "a"));
        assert!(!required(&s.deserialized, "b"));
    }

    #[test]
    fn skip_serializing_if_means_a_response_field_may_be_absent() {
        let s = shape(
            "struct Item { #[serde(skip_serializing_if = \"Option::is_none\")] note: Option<String> }",
        );
        assert!(!required(&s.serialized, "note"));
    }

    /// The break the type checker cannot see: the field still exists in Rust,
    /// so a caller reading it compiles, but it never arrives.
    #[test]
    fn skip_serializing_removes_a_field_from_the_response_only() {
        let s = shape("struct Item { id: String, #[serde(skip_serializing)] secret: String }");
        assert_eq!(names(&s.serialized), ["id"]);
        assert_eq!(names(&s.deserialized), ["id", "secret"]);
    }

    #[test]
    fn skip_deserializing_removes_a_field_from_the_request_only() {
        let s = shape("struct Item { id: String, #[serde(skip_deserializing)] derived: String }");
        assert_eq!(names(&s.serialized), ["id", "derived"]);
        assert_eq!(names(&s.deserialized), ["id"]);
    }

    #[test]
    fn a_fully_skipped_field_is_on_neither_side() {
        let s = shape("struct Item { id: String, #[serde(skip)] internal: String }");
        assert_eq!(names(&s.serialized), ["id"]);
        assert_eq!(names(&s.deserialized), ["id"]);
    }

    #[test]
    fn rename_and_rename_all_change_the_wire_name_not_the_rust_name() {
        let s = shape(
            "#[serde(rename_all = \"camelCase\")] struct Item { price_cents: u32, #[serde(rename = \"sku\")] code: String }",
        );
        let price = &s.serialized[0];
        assert_eq!(
            (price.rust_name.as_str(), price.wire_name.as_str()),
            ("price_cents", "priceCents")
        );
        let code = &s.serialized[1];
        assert_eq!(
            (code.rust_name.as_str(), code.wire_name.as_str()),
            ("code", "sku")
        );
    }

    #[test]
    fn a_unit_struct_is_an_empty_shape() {
        let s = shape("struct Ping;");
        assert!(s.serialized.is_empty() && s.deserialized.is_empty());
    }

    #[test]
    fn a_raw_identifier_records_the_name_serde_uses() {
        let s = shape("struct Item { r#type: String }");
        assert_eq!(names(&s.serialized), ["type"]);
        assert_eq!(s.serialized[0].wire_name, "type");
    }

    #[test]
    fn generics_are_refused() {
        assert!(refusal("struct Page<T> { items: Vec<T> }").contains("generic"));
    }

    #[test]
    fn an_enum_is_refused() {
        assert!(refusal("enum Status { Open }").contains("structs"));
    }

    #[test]
    fn a_tuple_struct_is_refused() {
        assert!(refusal("struct Id(String);").contains("named fields"));
    }

    #[test]
    fn serde_flatten_is_refused_rather_than_described_wrongly() {
        assert!(
            refusal("struct Item { #[serde(flatten)] extra: Extra }").contains("flatten"),
            "a flattened field hides keys the descriptor would have to invent"
        );
    }

    #[test]
    fn a_split_rename_is_refused_on_the_container_and_on_a_field() {
        assert!(
            refusal(
                "#[serde(rename_all(serialize = \"camelCase\", deserialize = \"snake_case\"))] struct Item { a: String }"
            )
            .contains("split")
        );
        assert!(
            refusal(
                "struct Item { #[serde(rename(serialize = \"a\", deserialize = \"b\"))] x: String }"
            )
            .contains("split")
        );
    }

    #[test]
    fn the_emitted_impl_carries_both_tables() {
        let input: DeriveInput =
            syn::parse_str("struct Item { id: String, #[serde(skip_serializing)] s: String }")
                .expect("fixture parses");
        let out = derive_wire_shape(&input).expect("emits").to_string();
        assert!(
            out.contains("impl :: autumn_web :: wire :: WireShape for Item"),
            "{out}"
        );
        assert!(out.contains("const SERIALIZED"), "{out}");
        assert!(out.contains("const DESERIALIZED"), "{out}");
        assert!(out.contains("\"id\""), "{out}");
    }
}
