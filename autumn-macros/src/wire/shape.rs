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

use crate::wire::ir::{TypeDescriptor, WireFieldDescriptor, WireType, WireTypeDescriptor};
use crate::wire::store;
use crate::wire::wire_type::{wire_attrs, wire_type_of};
use autumn_macros_support::schema;

/// One field's shape in one direction, ready to emit or to serialize.
struct Direction {
    fields: Vec<WireFieldDescriptor>,
}

pub fn derive_wire_shape(input: &DeriveInput) -> Result<TokenStream, syn::Error> {
    let shape = wire_type_descriptor(input)?;
    let written = write_descriptor(&shape, input.ident.span());
    let track = store::track_contract_dir();
    let shape_impl = wire_shape_tokens(&input.ident, &shape);
    Ok(quote! { #track #written #shape_impl })
}

/// The `WireShape` impl for a shape already read. It writes no descriptor.
///
/// `#[event]` uses it alone: its own `event.*` descriptor carries the shape,
/// and a `type.*` file would share its name with an HTTP DTO of the same type
/// name in another module.
#[must_use]
pub fn wire_shape_tokens(name: &syn::Ident, shape: &WireTypeDescriptor) -> TokenStream {
    let serialized = field_tokens(&shape.serialized);
    let deserialized = field_tokens(&shape.deserialized);
    let type_name = &shape.name;
    quote! {
        impl ::autumn_web::wire::WireShape for #name {
            const TYPE_NAME: &'static str = #type_name;
            const SERIALIZED: &'static [::autumn_web::wire::WireField] = &[#(#serialized),*];
            const DESERIALIZED: &'static [::autumn_web::wire::WireField] = &[#(#deserialized),*];
        }
    }
}

/// Write this type's JSON descriptor, if a contract directory resolves.
///
/// Best effort: the descriptor only enriches a diagnostic, so a build that
/// cannot write it still holds the contract through the const tables above.
fn write_descriptor(shape: &WireTypeDescriptor, span: proc_macro2::Span) -> TokenStream {
    let Some(dir) = store::contract_dir() else {
        return TokenStream::new();
    };
    let krate = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let module = store::source_module(span);
    store::write_outcome(store::write_type(
        &dir,
        &type_descriptor(&krate, &module, shape),
    ))
}

/// The descriptor of a type in `krate`, in the module of its source file.
#[must_use]
pub fn type_descriptor(krate: &str, module: &str, shape: &WireTypeDescriptor) -> TypeDescriptor {
    TypeDescriptor {
        krate: krate.to_owned(),
        module: module.to_owned(),
        shape: shape.clone(),
    }
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

    refuse_cfg(input, named)?;
    check_wire_attrs(input)?;

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
        // serde writes a unit struct as `null`, not as an object.
        return Ok(WireTypeDescriptor {
            name: input.ident.to_string(),
            serialized: Vec::new(),
            deserialized: Vec::new(),
            closed,
            unit: true,
        });
    };
    let own = input.ident.to_string();
    let (serialized, deserialized) =
        directions(named, &own, rename_all.as_deref(), container_default)?;
    Ok(WireTypeDescriptor {
        name: input.ident.to_string(),
        serialized: serialized.fields,
        deserialized: deserialized.fields,
        closed,
        unit: false,
    })
}

/// Refuse what depends on a cfg the macro cannot see: a `serde(…)` behind
/// `cfg_attr`, on the container or a field, and a field behind `cfg`.
fn refuse_cfg(input: &DeriveInput, named: Option<&FieldsNamed>) -> Result<(), syn::Error> {
    // A serde attribute behind `cfg_attr` is not expanded yet: the shape it
    // gives depends on a cfg the macro cannot see.
    if let Some(attr) = cfg_attr_serde(&input.attrs) {
        return Err(cfg_attr_refusal(attr));
    }
    if let Some(attr) = named
        .into_iter()
        .flat_map(|fields| fields.named.iter())
        .find_map(|field| cfg_attr_serde(&field.attrs))
    {
        return Err(cfg_attr_refusal(attr));
    }
    // A field behind `#[cfg(…)]` is on the wire in one build and gone in
    // another, and the macro cannot see which.
    if let Some(attr) = named
        .into_iter()
        .flat_map(|fields| fields.named.iter())
        .find_map(|field| field.attrs.iter().find(|a| a.path().is_ident("cfg")))
    {
        return Err(syn::Error::new_spanned(
            attr,
            "#[derive(WireShape)] refuses a field behind `#[cfg(…)]`: the macro cannot see the \
             cfg, so the descriptor could list a field the build does not have",
        ));
    }
    Ok(())
}

/// The first `#[cfg_attr(…, serde(…))]` in `attrs`, if any.
fn cfg_attr_serde(attrs: &[syn::Attribute]) -> Option<&syn::Attribute> {
    attrs.iter().find(|attr| {
        attr.path().is_ident("cfg_attr")
            && attr
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|metas| metas.iter().skip(1).any(|m| m.path().is_ident("serde")))
    })
}

fn cfg_attr_refusal(attr: &syn::Attribute) -> syn::Error {
    syn::Error::new_spanned(
        attr,
        "#[derive(WireShape)] refuses `#[cfg_attr(…, serde(…))]`: the serde attribute depends \
         on a cfg the macro cannot see, so the descriptor could claim a shape the build does \
         not have",
    )
}

/// Split a struct's fields into what it produces and what it accepts.
fn directions(
    named: &FieldsNamed,
    own: &str,
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
        let wire = wire_attrs(field)?;
        let defaulted = container_default || schema::has_serde_default(&field.attrs);
        // `#[serde(with)]` / `#[serde(deserialize_with)]` replace the whole
        // field deserializer, and serde's generated code then returns
        // `missing_field` for an absent key instead of the `Option` shortcut.
        // So the key is mandatory even on an `Option<T>`, unless a default
        // fills it in.
        let custom_de =
            schema::serde_valued_key(&field.attrs, &["with", "deserialize_with"]).is_some();
        let custom_ser =
            schema::serde_valued_key(&field.attrs, &["with", "serialize_with"]).is_some();
        // A custom serde function can write any JSON, and only an override
        // names it. With a custom function on one side only, the override
        // names that side, and serde's own form holds on the other.
        let one_sided = custom_ser != custom_de;
        let named = |custom: bool| wire.wire_type.as_ref().filter(|_| custom || !one_sided);
        let option = schema::is_option_type(&field.ty);
        let mapped = wire_type_of(&field.ty, own, wire.opaque);
        let wire_type = |custom: bool| match named(custom) {
            Some(named) if custom => named.clone(),
            // serde still writes `null` for `None`.
            Some(named) if option => WireType::nullable(named.clone()),
            Some(named) => named.clone(),
            None if custom => WireType::Any,
            None => mapped.clone(),
        };
        // An override says what is on the wire, so the export does not look
        // for a shape of the Rust type on that side.
        let opaque = |custom: bool| wire.opaque || named(custom).is_some();
        let optional = option || matches!(named(custom_de), Some(WireType::Nullable { .. }));

        if !skip_serializing {
            serialized.push(WireFieldDescriptor {
                rust_name: rust_name.clone(),
                wire_name: wire_name.clone(),
                ty: ty.clone(),
                wire_type: Some(wire_type(custom_ser)),
                // A conditionally-skipped field may be absent from a response,
                // so a caller cannot count on it arriving.
                required: !schema::field_has_skip_serializing_if(field),
                aliases: Vec::new(),
                opaque: opaque(custom_ser),
                nested: Vec::new(),
            });
        }
        if !skip_deserializing {
            deserialized.push(WireFieldDescriptor {
                rust_name,
                wire_name,
                ty,
                wire_type: Some(wire_type(custom_de)),
                // `Option<T>`, a field default, or a container default each
                // let the key be absent from a request body — but a custom
                // deserializer takes the `Option` shortcut away.
                required: !defaulted && (custom_de || !optional),
                aliases: serde_aliases(field),
                opaque: opaque(custom_de),
                nested: Vec::new(),
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

/// Refuse a `#[wire(…)]` on the container, or a bad one on a field.
///
/// `#[event(opaque)]` reads no shape, so it calls this alone.
///
/// # Errors
/// A `#[wire]` on the container, or a bad `#[wire(…)]` on a field.
pub fn check_wire_attrs(input: &DeriveInput) -> Result<(), syn::Error> {
    if let Some(attr) = input.attrs.iter().find(|a| a.path().is_ident("wire")) {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[wire(…)]` goes on a field, not on the type",
        ));
    }
    if let Data::Struct(data) = &input.data {
        for field in &data.fields {
            wire_attrs(field)?;
        }
    }
    Ok(())
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
    fn a_unit_struct_is_marked_unit_and_an_empty_object_is_not() {
        assert!(shape("struct Empty;").unit, "JSON null");
        assert!(!shape("struct Empty {}").unit, "JSON object");
        assert!(!shape("struct Item { id: String }").unit);
    }

    #[test]
    fn wire_opaque_marks_a_field_in_both_directions() {
        let s = shape("struct Item { #[wire(opaque)] status: Status, id: String }");
        assert!(s.serialized[0].opaque && s.deserialized[0].opaque);
        assert!(!s.serialized[1].opaque);
    }

    #[test]
    fn an_unknown_wire_argument_is_refused() {
        let err = refusal("struct Item { #[wire(opaq)] status: Status }");
        assert!(err.contains("opaque") && err.contains("type"), "{err}");
        assert!(refusal("#[wire(opaque)] struct Item { id: String }").contains("on a field"));
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
    fn a_serde_attribute_behind_cfg_attr_is_refused() {
        assert!(
            refusal(
                "struct Item { #[cfg_attr(feature = \"redact\", serde(skip_serializing))] email: String }"
            )
            .contains("cfg_attr")
        );
        assert!(
            refusal(
                "#[cfg_attr(test, serde(rename_all = \"camelCase\"))] struct Item { a: String }"
            )
            .contains("cfg_attr")
        );
        assert!(
            refusal("struct Item { #[cfg(feature = \"details\")] details: String }")
                .contains("#[cfg(")
        );
        // A cfg_attr that adds no serde attribute is fine.
        let s = shape("struct Item { #[cfg_attr(test, allow(dead_code))] a: String }");
        assert_eq!(names(&s.serialized), ["a"]);
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

    #[test]
    fn the_descriptor_records_the_module_given() {
        let input: DeriveInput = syn::parse_str("struct Item;").expect("fixture parses");
        let shape = wire_type_descriptor(&input).expect("shape");
        let d = type_descriptor("catalog", "api::items", &shape);
        assert_eq!(d.module, "api::items");
        assert_eq!(d.artifact_file_name(), "type.catalog.api.items.Item.json");
    }

    /// One field of each mapped type, and each override form.
    const ALL_TYPES: &str = r#"
        struct Everything {
            text: String,
            borrowed: &'static str,
            letter: char,
            cow: std::borrow::Cow<'static, str>,
            boxed: Box<str>,
            path: std::path::PathBuf,
            i_8: i8,
            i_16: i16,
            i_32: i32,
            i_64: i64,
            i_128: i128,
            i_size: isize,
            u_8: u8,
            u_16: u16,
            u_32: u32,
            u_64: u64,
            u_128: u128,
            u_size: usize,
            non_zero: std::num::NonZeroU32,
            non_zero_generic: NonZero<i16>,
            f_32: f32,
            f_64: f64,
            flag: bool,
            list: Vec<u32>,
            deque: VecDeque<bool>,
            set: HashSet<String>,
            tree_set: BTreeSet<i64>,
            array: [u8; 4],
            slice: &'static [i64],
            bytes: Vec<u8>,
            maybe: Option<String>,
            maybe_twice: Option<Option<u8>>,
            map: HashMap<String, u64>,
            tree_map: std::collections::BTreeMap<String, Vec<String>>,
            index_map: IndexMap<String, bool>,
            uuid_keys: HashMap<Uuid, f64>,
            int_keys: BTreeMap<u32, String>,
            tuple_keys: HashMap<(u8, u8), bool>,
            json_map: serde_json::Map<String, serde_json::Value>,
            date_time: chrono::DateTime<chrono::Utc>,
            date_time_offset: DateTime<FixedOffset>,
            naive_date: chrono::NaiveDate,
            naive_date_time: NaiveDateTime,
            naive_time: NaiveTime,
            offset_date_time: time::OffsetDateTime,
            primitive_date_time: time::PrimitiveDateTime,
            time_date: time::Date,
            uuid: uuid::Uuid,
            decimal: rust_decimal::Decimal,
            big_decimal: BigDecimal,
            value: serde_json::Value,
            number: serde_json::Number,
            duration: std::time::Duration,
            url: url::Url,
            ip: std::net::IpAddr,
            ipv4: Ipv4Addr,
            ipv6: Ipv6Addr,
            socket: SocketAddr,
            raw: bytes::Bytes,
            byte_buf: serde_bytes::ByteBuf,
            rc: std::rc::Rc<str>,
            path_ref: &'static std::path::Path,
            linked: LinkedList<u8>,
            heap: BinaryHeap<u16>,
            index_set: IndexSet<String>,
            cell: std::cell::Cell<bool>,
            ref_cell: RefCell<String>,
            reverse: std::cmp::Reverse<i32>,
            wrapping: std::num::Wrapping<u64>,
            small_vec: SmallVec<[u32; 4]>,
            small_vec_2: SmallVec<u32, 4>,
            boxed_alloc: Box<Details, Global>,
            phantom: PhantomData<u8>,
            naive_alias: sea_orm::prelude::DateTime,
            bson_date_time: bson::DateTime,
            nested: Details,
            nested_path: crate::dto::Details,
            nested_list: Vec<shared::Details>,
            nested_box: Box<Details>,
            nested_arc: std::sync::Arc<Details>,
            recursive: Vec<Box<Self>>,
            tuple: (u8, u8),
            unit: (),
            #[wire(opaque)]
            status: Status,
            #[wire(opaque)]
            maybe_status: Option<Status>,
            #[serde(with = "chrono::serde::ts_seconds")]
            with_no_override: chrono::DateTime<chrono::Utc>,
            #[serde(serialize_with = "as_text")]
            serialize_with: u32,
            #[serde(deserialize_with = "from_text")]
            deserialize_with: u32,
            #[serde(with = "chrono::serde::ts_seconds")]
            #[wire(type = "integer", format = "unix-seconds")]
            stamp: chrono::DateTime<chrono::Utc>,
            #[wire(type = "string", nullable)]
            nickname: MaybeName,
            #[wire(type = "number")]
            score: Score,
            #[wire(type = "boolean")]
            on: Toggle,
            #[wire(type = "any")]
            blob: Blob,
            #[wire(type = "string", format = "email")]
            email: Email,
            #[serde(serialize_with = "as_text")]
            #[wire(type = "string")]
            text_out: u32,
            #[serde(deserialize_with = "from_text")]
            #[wire(type = "string")]
            text_in: u32,
            #[wire(type = "number")]
            maybe_score: Option<Score>,
            #[serde(with = "text")]
            #[wire(type = "string")]
            maybe_text: Option<u32>,
            #[wire(opaque)]
            opaque_self: Vec<Self>,
        }
    "#;

    /// One line per field: name, Rust type, the serialized and the
    /// deserialized wire type, and whether a request needs it.
    fn wire_table(s: &WireTypeDescriptor) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (ser, de) in s.serialized.iter().zip(&s.deserialized) {
            assert_eq!(ser.rust_name, de.rust_name);
            let ser_type = ser.wire_type.as_ref().expect("serialized wire type");
            let de_type = de.wire_type.as_ref().expect("deserialized wire type");
            let both = if ser_type == de_type {
                ser_type.to_string()
            } else {
                format!("{ser_type} / {de_type}")
            };
            let need = if de.required { "required" } else { "optional" };
            let opaque = if de.opaque { ", opaque" } else { "" };
            writeln!(
                out,
                "{}: {} => {both} ({need}{opaque})",
                ser.rust_name, ser.ty
            )
            .expect("write to a string");
        }
        out
    }

    #[test]
    fn the_wire_type_of_each_mapped_type_matches_the_golden_file() {
        let golden = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/wire_types.txt");
        let table = wire_table(&shape(ALL_TYPES));
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(golden, &table).expect("write golden");
        }
        let want = std::fs::read_to_string(golden).expect("golden file");
        assert_eq!(
            table, want,
            "the wire types differ from {golden}. If the change is intended, run \
             `UPDATE_GOLDEN=1 cargo test -p autumn-macros --lib wire_type` and review the diff."
        );
    }

    fn wire(fields: &[WireFieldDescriptor], name: &str) -> WireType {
        fields
            .iter()
            .find(|f| f.rust_name == name)
            .and_then(|f| f.wire_type.clone())
            .unwrap_or_else(|| panic!("no wire type for `{name}`"))
    }

    #[test]
    fn serde_with_without_an_override_is_any() {
        let s =
            shape("struct T { #[serde(with = \"chrono::serde::ts_seconds\")] at: DateTime<Utc> }");
        assert_eq!(wire(&s.serialized, "at"), WireType::Any);
        assert_eq!(wire(&s.deserialized, "at"), WireType::Any);
        // Any is not a nested shape: the field is not opaque.
        assert!(!s.serialized[0].opaque);
    }

    #[test]
    fn serialize_with_and_deserialize_with_change_one_direction_only() {
        let s = shape(
            "struct T { #[serde(serialize_with = \"f\")] a: u32, #[serde(deserialize_with = \"g\")] b: u32 }",
        );
        assert_eq!(wire(&s.serialized, "a"), WireType::Any);
        assert_eq!(wire(&s.deserialized, "a"), WireType::integer(32, false));
        assert_eq!(wire(&s.serialized, "b"), WireType::integer(32, false));
        assert_eq!(wire(&s.deserialized, "b"), WireType::Any);
    }

    #[test]
    fn a_wire_type_override_wins_over_serde_with() {
        let s = shape(
            "struct T { #[serde(with = \"ts_seconds\")] #[wire(type = \"integer\", format = \"unix-seconds\")] at: DateTime<Utc> }",
        );
        let want = WireType::Integer {
            bits: 64,
            signed: true,
            format: Some("unix-seconds".to_owned()),
        };
        assert_eq!(wire(&s.serialized, "at"), want);
        assert_eq!(wire(&s.deserialized, "at"), want);
        assert_eq!(s.serialized[0].ty, "DateTime<Utc>", "ty is kept");
        assert!(
            s.serialized[0].opaque,
            "an override is not looked up as a shape"
        );
    }

    #[test]
    fn an_option_alias_with_a_wire_type_override_is_nullable_and_optional() {
        // `type MaybeName = Option<String>;` The name does not say `Option`.
        let plain = shape("struct T { nickname: MaybeName }");
        assert!(required(&plain.deserialized, "nickname"));
        assert_eq!(
            wire(&plain.deserialized, "nickname"),
            WireType::object("MaybeName")
        );

        let s = shape("struct T { #[wire(type = \"string\", nullable)] nickname: MaybeName }");
        assert_eq!(
            wire(&s.deserialized, "nickname"),
            WireType::nullable(WireType::string())
        );
        assert!(!required(&s.deserialized, "nickname"));
        // serde still writes `null`, so a response always has the key.
        assert!(required(&s.serialized, "nickname"));
        assert!(s.deserialized[0].opaque);
    }

    #[test]
    fn a_nullable_override_with_a_custom_deserializer_stays_required() {
        let s = shape(
            "struct T { #[serde(deserialize_with = \"f\")] #[wire(type = \"string\", nullable)] n: MaybeName }",
        );
        assert!(required(&s.deserialized, "n"));
    }

    #[test]
    fn an_override_with_a_one_sided_custom_function_names_that_side_only() {
        let s = shape(
            "struct T { #[serde(serialize_with = \"f\")] #[wire(type = \"string\")] a: u32, #[serde(deserialize_with = \"g\")] #[wire(type = \"string\")] b: u32 }",
        );
        assert_eq!(wire(&s.serialized, "a"), WireType::string());
        assert_eq!(wire(&s.deserialized, "a"), WireType::integer(32, false));
        assert_eq!(wire(&s.serialized, "b"), WireType::integer(32, false));
        assert_eq!(wire(&s.deserialized, "b"), WireType::string());
    }

    #[test]
    fn a_one_sided_nullable_override_leaves_the_other_side_required() {
        let s = shape(
            "struct T { #[serde(serialize_with = \"f\")] #[wire(type = \"string\", nullable)] n: Name }",
        );
        assert_eq!(
            wire(&s.serialized, "n"),
            WireType::nullable(WireType::string())
        );
        assert!(required(&s.deserialized, "n"));
    }

    #[test]
    fn a_one_sided_override_keeps_the_other_side_a_nested_shape() {
        let s = shape(
            "struct T { #[serde(serialize_with = \"f\")] #[wire(type = \"string\")] d: Details }",
        );
        assert_eq!(wire(&s.serialized, "d"), WireType::string());
        assert!(s.serialized[0].opaque);
        assert_eq!(wire(&s.deserialized, "d"), WireType::object("Details"));
        assert!(!s.deserialized[0].opaque);
    }

    #[test]
    fn an_override_on_an_option_field_keeps_null() {
        let s = shape("struct T { #[wire(type = \"number\")] score: Option<Score> }");
        assert_eq!(
            wire(&s.serialized, "score"),
            WireType::nullable(WireType::Number)
        );
        assert_eq!(
            wire(&s.deserialized, "score"),
            WireType::nullable(WireType::Number)
        );
        // A custom function writes what the override says.
        let s = shape(
            "struct T { #[serde(with = \"text\")] #[wire(type = \"string\")] n: Option<u32> }",
        );
        assert_eq!(wire(&s.serialized, "n"), WireType::string());
    }

    #[test]
    fn a_date_time_with_no_time_zone_argument_is_any() {
        let s = shape("struct T { a: sea_orm::prelude::DateTime, b: bson::DateTime }");
        assert_eq!(wire(&s.serialized, "a"), WireType::Any);
        assert_eq!(wire(&s.serialized, "b"), WireType::Any);
    }

    #[test]
    fn self_in_an_opaque_field_is_any() {
        let s = shape("struct T { #[wire(opaque)] kids: Vec<Self> }");
        assert_eq!(wire(&s.serialized, "kids"), WireType::array(WireType::Any));
    }

    #[test]
    fn a_wrapper_maps_its_first_type_argument() {
        let s =
            shape("struct T { a: Box<Item, Global>, b: SmallVec<u8, 4>, c: SmallVec<[u8; 4]> }");
        assert_eq!(wire(&s.serialized, "a"), WireType::object("Item"));
        let bytes = WireType::array(WireType::integer(8, false));
        assert_eq!(wire(&s.serialized, "b"), bytes);
        assert_eq!(wire(&s.serialized, "c"), bytes);
    }

    #[test]
    fn an_unknown_type_in_an_opaque_field_is_any() {
        let s = shape("struct T { #[wire(opaque)] status: Option<Status> }");
        assert_eq!(
            wire(&s.serialized, "status"),
            WireType::nullable(WireType::Any)
        );
    }

    #[test]
    fn a_nested_type_is_an_object_by_the_path_as_written() {
        let s = shape("struct T { a: shared::Item, b: Vec<::shop::Item>, c: Box<Self> }");
        assert_eq!(wire(&s.serialized, "a"), WireType::object("shared::Item"));
        assert_eq!(
            wire(&s.serialized, "b"),
            WireType::array(WireType::object("::shop::Item"))
        );
        assert_eq!(wire(&s.serialized, "c"), WireType::object("T"));
    }

    #[test]
    fn an_event_payload_records_wire_types_too() {
        // `#[event]` reads its shape through `wire_type_descriptor`.
        let s = shape("struct ContactUpdated { id: uuid::Uuid, at: Option<DateTime<Utc>> }");
        assert_eq!(wire(&s.serialized, "id"), WireType::string_in("uuid"));
        assert_eq!(
            wire(&s.deserialized, "at"),
            WireType::nullable(WireType::string_in("date-time"))
        );
    }

    #[test]
    fn an_unknown_wire_type_is_refused() {
        let err = refusal("struct T { #[wire(type = \"timestamp\")] at: DateTime<Utc> }");
        assert!(err.contains("unknown wire type `timestamp`"), "{err}");
        assert!(
            err.contains("`string`, `integer`, `number`, `boolean`, `any`"),
            "{err}"
        );
    }

    #[test]
    fn a_bad_wire_override_is_refused() {
        for (src, want) in [
            (
                "struct T { #[wire(format = \"date\")] a: Day }",
                "`format` needs `type`",
            ),
            (
                "struct T { #[wire(nullable)] a: Day }",
                "`nullable` needs `type`",
            ),
            (
                "struct T { #[wire(type = \"boolean\", format = \"yes-no\")] a: Flag }",
                "`format` applies to `string` and `integer` only",
            ),
            (
                "struct T { #[wire(type = \"string\", format = \"Date Time\")] a: Day }",
                "a format is lowercase words joined by `-`",
            ),
            (
                "struct T { #[wire(type = \"string\")] #[wire(type = \"integer\")] a: Day }",
                "`type` is set twice",
            ),
            (
                "struct T { #[wire(type = \"string\", format = \"date\", format = \"uuid\")] a: Day }",
                "`format` is set twice",
            ),
        ] {
            let err = refusal(src);
            assert!(err.contains(want), "{src}: {err}");
        }
    }

    #[test]
    fn an_override_on_an_opaque_event_payload_is_still_checked() {
        let input: DeriveInput =
            syn::parse_str("struct T { #[wire(type = \"date\")] a: Day }").expect("parses");
        let err = check_wire_attrs(&input).expect_err("refused").to_string();
        assert!(err.contains("unknown wire type"), "{err}");
    }

    #[test]
    fn the_derive_tracks_the_contract_dir_variable() {
        let input: DeriveInput =
            syn::parse_str("struct Item { id: String }").expect("fixture parses");
        let out = derive_wire_shape(&input).expect("derives").to_string();
        assert!(out.contains("option_env"), "{out}");
        assert!(out.contains("AUTUMN_CONTRACT_DIR"), "{out}");
    }
}
