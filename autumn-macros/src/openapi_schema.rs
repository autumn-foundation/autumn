//! `#[derive(OpenApiSchema)]` — a standalone derive that gives a plain struct or
//! unit-variant enum a field-accurate `OpenApiSchema` impl (issue #1972).
//!
//! Before this derive, the only automatic `OpenApiSchema` impls came from
//! `#[model]` codegen and the primitive macro impls, so any other handler-arg
//! struct (a `Query<T>` param struct or a non-`#[model]` `Json<T>` body) had to
//! carry a hand-written impl plus an `OpenApiConfig::register_schema` call — or
//! its `OpenAPI` / MCP `inputSchema` degraded to a generic
//! `{"type":"object","title":"X"}` placeholder.
//!
//! For structs this mirrors the schema `#[model]` already generates
//! (`crate::schema::emit_schema_fn_body_full`): each field becomes a JSON-schema
//! property and every non-`Option` field is `required`. For enums it emits the
//! closed-set form (`{"type":"string","enum":[…]}`) that serde's default
//! externally-tagged representation produces for unit variants — the shape a
//! client generator turns into a TypeScript string union or a Rust enum.
//!
//! Either way the derive submits the schema into the compile-time
//! `DerivedSchemaDescriptor` inventory that the spec/MCP back-fill loops
//! consult, so a referenced type resolves to its real schema with no manual
//! registration.

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

    let schema_body = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => {
                if let Err(e) = reject_undescribable_struct(&input, named) {
                    return e.to_compile_error().into();
                }
                // The emitters expect `&[&&Field]` (they were written against
                // the `Vec<&&Field>` the model macro collects); build that
                // shape here, minus the fields serde never puts on the wire.
                let field_refs: Vec<&syn::Field> = named
                    .named
                    .iter()
                    .filter(|f| crate::schema::serde_bare_word(&f.attrs, &["skip"]).is_none())
                    .collect();
                let field_ref_refs: Vec<&&syn::Field> = field_refs.iter().collect();
                // Honor a container `#[serde(rename_all = "...")]` — the split
                // form is refused above, so this side is the only side.
                let rename_all_rule = crate::schema::serde_rename_all_serialize_rule(&input.attrs);
                // A container `#[serde(default)]` (bare or `= "path"`) lets
                // EVERY field be absent from a request, filled from the struct
                // default. Nothing is required then. Safe in both directions:
                // a response still carries every field, so a client that does
                // not demand them is not misled, while a request client is no
                // longer forced to send what the handler does not need.
                let container_default = crate::schema::serde_bare_word(&input.attrs, &["default"])
                    .is_some()
                    || crate::schema::serde_valued_key(&input.attrs, &["default"]).is_some();
                crate::schema::emit_schema_fn_body_full(
                    &field_ref_refs,
                    container_default,
                    &[],
                    rename_all_rule.as_deref(),
                    // A `#[serde(default)]` field may be omitted from a request
                    // and is always present in a response, so "not required" is
                    // true of both directions — no conflict, unlike the
                    // directional attributes refused above.
                    &|f: &syn::Field| {
                        crate::schema::serde_bare_word(&f.attrs, &["default"]).is_some()
                            || crate::schema::serde_valued_key(&f.attrs, &["default"]).is_some()
                    },
                )
            }
            _ => {
                return syn::Error::new_spanned(
                    &input,
                    "#[derive(OpenApiSchema)] is only supported on structs with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        Data::Enum(data) => match enum_schema_body(&input, data) {
            Ok(body) => body,
            Err(e) => return e.to_compile_error().into(),
        },
        Data::Union(_) => {
            return syn::Error::new_spanned(
                &input,
                "#[derive(OpenApiSchema)] is only supported on structs and enums",
            )
            .to_compile_error()
            .into();
        }
    };

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
                identity: ::autumn_web::openapi::type_name_of::<#name>,
                schema: <#name as ::autumn_web::openapi::OpenApiSchema>::schema,
            }
        }
    }
    .into()
}

/// Emit the `schema()` body for a unit-variant enum: the closed string set
/// serde's default representation puts on the wire.
///
/// Data-carrying variants are rejected rather than approximated. Serde's
/// externally-tagged form for those is a `oneOf` of single-key wrapper objects
/// whose exact shape also depends on `#[serde(tag/content/untagged)]`, so
/// guessing would advertise a contract the handler does not actually accept —
/// worse than the placeholder, because it is confidently wrong. The error names
/// the hand-written escape hatch instead.
fn enum_schema_body(
    input: &DeriveInput,
    data: &syn::DataEnum,
) -> syn::Result<proc_macro2::TokenStream> {
    reject_undescribable_enum(input, data)?;

    let rename_all_rule = crate::schema::serde_rename_all_serialize_rule(&input.attrs);
    let values: Vec<String> = data
        .variants
        .iter()
        .filter(|v| !crate::schema::variant_is_serde_skipped(v))
        .map(|v| {
            let raw = v.ident.to_string();
            let raw = raw.strip_prefix("r#").unwrap_or(&raw).to_owned();
            // Precedence mirrors serde: a variant-level `#[serde(rename)]` wins
            // over the container `#[serde(rename_all)]`, which wins over the
            // raw identifier.
            crate::schema::variant_serde_serialize_rename(v)
                .or_else(|| {
                    rename_all_rule.as_deref().and_then(|rule| {
                        crate::schema::apply_serde_rename_all_rule_to_variant(rule, &raw)
                    })
                })
                .unwrap_or(raw)
        })
        .collect();

    if values.is_empty() {
        return Err(syn::Error::new_spanned(
            input,
            "#[derive(OpenApiSchema)] needs at least one non-skipped variant to advertise",
        ));
    }

    Ok(quote! {
        ::autumn_web::reexports::serde_json::json!({
            "type": "string",
            "enum": [#(#values),*],
        })
    })
}

/// Refuse every struct shape whose real wire form the derive cannot describe.
///
/// Written as one audit of serde's attribute surface rather than a rule per
/// report. Seven review rounds on issue #802 landed fixes scoped to whichever
/// attribute was named, and each time an adjacent one was still wrong; the
/// generalisation is that an object schema is only the truth when every field
/// appears under a single symmetric name and the container is not re-shaped.
///
/// | Attribute | What serde does | Why an object schema is wrong |
/// |---|---|---|
/// | `transparent` | writes the inner value | there is no object at all |
/// | `into` / `from` / `try_from` | routes through another type | shape is that type's |
/// | `tag` / `untagged` | re-tags the container | adds or removes an object level |
/// | `flatten` (field) | merges the field's keys upward | the field is not a nested property |
/// | `skip_serializing` / `skip_deserializing` (field) | one direction only | one schema serves both |
/// | split `rename_all` / `rename` | two different names | one schema advertises one |
///
/// Accepted and handled by the caller rather than refused: `skip` (absent both
/// ways, so the field is dropped), `default` (omissible on input, present on
/// output — "not required" is true either way), and a symmetric `rename_all` /
/// `rename`.
///
/// KNOWN RESIDUAL: `with` / `serialize_with` / `deserialize_with` can put an
/// arbitrary shape on the wire, and the schema still describes the Rust type.
/// Not refused, because the attribute is common and usually only reformats a
/// value of the same JSON type — but a converter that changes the type is
/// misdescribed. Register such a type's schema by hand.
fn reject_undescribable_struct(input: &DeriveInput, named: &syn::FieldsNamed) -> syn::Result<()> {
    // ── Container ────────────────────────────────────────────────────
    if let Some(word) = crate::schema::serde_bare_word(&input.attrs, &["transparent", "untagged"]) {
        return Err(syn::Error::new_spanned(
            input,
            format!(
                "#[derive(OpenApiSchema)] cannot describe `#[serde({word})]`: serde does not \
                 put an object with these fields on the wire, so the derived schema would \
                 advertise a shape the handler neither accepts nor returns. Write the \
                 `OpenApiSchema` impl by hand and register it with \
                 `OpenApiConfig::register_schema`."
            ),
        ));
    }
    if let Some(key) =
        crate::schema::serde_valued_key(&input.attrs, &["into", "from", "try_from", "tag"])
    {
        return Err(syn::Error::new_spanned(
            input,
            format!(
                "#[derive(OpenApiSchema)] cannot describe `#[serde({key} = ...)]`: it re-shapes \
                 what reaches the wire, so an object schema built from these fields would be \
                 wrong. Write the `OpenApiSchema` impl by hand and register it with \
                 `OpenApiConfig::register_schema`."
            ),
        ));
    }
    if let Some(key) = crate::schema::serde_split_rename(&input.attrs, "rename_all") {
        return Err(syn::Error::new_spanned(input, split_rename_message(key)));
    }

    // ── Fields ───────────────────────────────────────────────────────
    for field in &named.named {
        if let Some(word) = crate::schema::serde_bare_word(&field.attrs, &["flatten"]) {
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "#[derive(OpenApiSchema)] cannot describe `#[serde({word})]`: serde merges \
                     this field's keys into the containing object, while the derived schema \
                     would publish it as a nested property — a generated client would send a \
                     nesting the handler does not accept and expect one the server never \
                     emits. Write the `OpenApiSchema` impl by hand and register it with \
                     `OpenApiConfig::register_schema`."
                ),
            ));
        }
        if let Some(word) = crate::schema::variant_directional_skip_on_field(field) {
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "#[derive(OpenApiSchema)] cannot describe a field skipped in only one serde \
                     direction (`#[serde({word})]`): one schema covers both requests and \
                     responses. Use `#[serde(skip)]` if the field should not appear at all, or \
                     write the `OpenApiSchema` impl by hand and register it with \
                     `OpenApiConfig::register_schema`."
                ),
            ));
        }
        // Direction-dependent for the same reason, but only when the type does
        // not already make it optional.
        if crate::schema::field_has_skip_serializing_if(field)
            && !crate::schema::is_option_type(&field.ty)
        {
            return Err(syn::Error::new_spanned(
                field,
                "#[derive(OpenApiSchema)] cannot describe a non-`Option` field with \
                 `#[serde(skip_serializing_if = ...)]`: that attribute governs serialization \
                 only, so a response may omit the field while serde still rejects a request \
                 that does. Make the field `Option<T>` (where it costs nothing, being optional \
                 already), or write the `OpenApiSchema` impl by hand and register it with \
                 `OpenApiConfig::register_schema`.",
            ));
        }
        if let Some(key) = crate::schema::serde_split_rename(&field.attrs, "rename") {
            return Err(syn::Error::new_spanned(field, split_rename_message(key)));
        }
    }

    Ok(())
}

/// The shared diagnostic for a split `rename_all` / `rename` whose sides differ.
fn split_rename_message(key: &str) -> String {
    format!(
        "#[derive(OpenApiSchema)] cannot describe a split `#[serde({key}(serialize = ..., \
         deserialize = ...))]` whose two sides differ: one schema is advertised for both \
         requests and responses, so a client generated from the serialize spelling would send \
         a value the handler's `Deserialize` rejects. Use a symmetric `{key} = \"...\"`, or \
         write the `OpenApiSchema` impl by hand and register it with \
         `OpenApiConfig::register_schema`."
    )
}

/// Refuse every enum shape whose real wire form the derive cannot describe.
///
/// Split out of [`enum_schema_body`] to keep that function inside the
/// line budget, and because these four checks are one idea: a JSON string enum
/// is only the truth when serde's default representation applies to unit
/// variants with a single, symmetric spelling. Anything else is refused rather
/// than approximated — a confidently wrong contract is worse than no derive,
/// since a generated client acts on it.
fn reject_undescribable_enum(input: &DeriveInput, data: &syn::DataEnum) -> syn::Result<()> {
    if let Some(variant) = data
        .variants
        .iter()
        .find(|v| !matches!(v.fields, Fields::Unit))
    {
        return Err(syn::Error::new_spanned(
            variant,
            "#[derive(OpenApiSchema)] supports only enums whose variants are all unit variants \
             (they map to a JSON string enum). For a data-carrying enum, write the \
             `OpenApiSchema` impl by hand and register it with \
             `OpenApiConfig::register_schema`.",
        ));
    }

    // All-unit is not sufficient: a non-default container representation
    // changes what a unit variant serializes to, so the string-enum schema
    // below would be confidently wrong. `#[serde(tag = "t")]` makes each
    // variant the object `{"t":"Variant"}`, and `#[serde(untagged)]` makes it
    // `null` — a generated client built from the string enum would send a bare
    // string to a handler that accepts neither. Refuse rather than guess, on
    // the same reasoning that refuses data-carrying variants.
    if let Some(repr) = crate::schema::serde_enum_representation(&input.attrs) {
        // `untagged` is a bare word; `tag` / `content` take a value. Render each
        // the way it is actually written, so the diagnostic quotes real syntax.
        let (written, becomes) = match repr {
            "untagged" => ("#[serde(untagged)]", "`null`"),
            "tag" => ("#[serde(tag = \"...\")]", "an object"),
            "content" => ("#[serde(content = \"...\")]", "an object"),
            // Conversion attributes route the value through another type
            // entirely, so the variant names never reach the wire at all.
            "into" => (
                "#[serde(into = \"...\")]",
                "whatever the conversion type serializes as",
            ),
            "from" => (
                "#[serde(from = \"...\")]",
                "whatever the conversion type serializes as",
            ),
            _ => (
                "#[serde(try_from = \"...\")]",
                "whatever the conversion type serializes as",
            ),
        };
        return Err(syn::Error::new_spanned(
            input,
            format!(
                "#[derive(OpenApiSchema)] supports only serde's default (externally tagged) \
                 enum representation; `{written}` makes a unit variant serialize as {becomes}, \
                 not the JSON string the derived schema would advertise — a generated client \
                 would send a contract the handler does not accept. Write the `OpenApiSchema` \
                 impl by hand and register it with `OpenApiConfig::register_schema`."
            ),
        ));
    }

    // A variant present in only one serde direction has no correct rendering in
    // a document that describes both. Refuse rather than publish a set that is
    // right for responses and wrong for requests (or the reverse).
    if let Some(variant) = data
        .variants
        .iter()
        .find(|v| crate::schema::variant_directional_skip(v).is_some())
    {
        let attribute = crate::schema::variant_directional_skip(variant)
            .expect("the find predicate just matched it");
        let consequence = if attribute == "skip_deserializing" {
            "it is still serialized, so advertising it would tell a client it may send a \
             value serde rejects as an unknown variant"
        } else {
            "it is still accepted on input, so omitting it would deny a value the handler takes"
        };
        return Err(syn::Error::new_spanned(
            variant,
            format!(
                "#[derive(OpenApiSchema)] cannot describe a variant skipped in only one serde \
                 direction: one schema covers both requests and responses, and with \
                 `#[serde({attribute})]` {consequence}. Use `#[serde(skip)]` if the variant \
                 should not appear at all, or write the `OpenApiSchema` impl by hand and \
                 register it with `OpenApiConfig::register_schema`."
            ),
        ));
    }

    // A split rename is the same asymmetry as a directional skip: one schema,
    // two disagreeing wire spellings. Advertising the serialize side would have
    // a generated client send `in_progress` to a handler whose `Deserialize`
    // accepts `inProgress`.
    let split = crate::schema::serde_split_rename(&input.attrs, "rename_all")
        .map(|key| (key, None))
        .or_else(|| {
            data.variants.iter().find_map(|v| {
                crate::schema::serde_split_rename(&v.attrs, "rename").map(|key| (key, Some(v)))
            })
        });
    if let Some((key, variant)) = split {
        let message = format!(
            "#[derive(OpenApiSchema)] cannot describe a split `#[serde({key}(serialize = ..., \
             deserialize = ...))]` whose two sides differ: one schema is advertised for both \
             requests and responses, so a client generated from the serialize spelling would \
             send a value the handler's `Deserialize` rejects. Use a symmetric \
             `{key} = \"...\"`, or write the `OpenApiSchema` impl by hand and register it with \
             `OpenApiConfig::register_schema`."
        );
        return Err(variant.map_or_else(
            || syn::Error::new_spanned(input, message.clone()),
            |v| syn::Error::new_spanned(v, message.clone()),
        ));
    }
    Ok(())
}
