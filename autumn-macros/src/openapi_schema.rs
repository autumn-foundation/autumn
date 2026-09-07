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
                let container_default = crate::schema::has_serde_default(&input.attrs);
                crate::schema::emit_schema_fn_body_full(
                    &field_ref_refs,
                    container_default,
                    &[],
                    rename_all_rule.as_deref(),
                    // A `#[serde(default)]` field may be omitted from a request
                    // and is always present in a response, so "not required" is
                    // true of both directions — no conflict, unlike the
                    // directional attributes refused above.
                    &|f: &syn::Field| crate::schema::has_serde_default(&f.attrs),
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
        // A (de)serialization adapter can put ANY shape on the wire — an `i64`
        // amount written as a string is the common one — so the field's Rust
        // type stops describing it. All three spellings are rejected, not just
        // the serialize half: this schema serves requests AND responses, so an
        // adapter on either side can make it wrong for that side. `with` sets
        // both at once.
        if let Some(key) = crate::schema::serde_valued_key(
            &field.attrs,
            &["with", "serialize_with", "deserialize_with"],
        ) {
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "#[derive(OpenApiSchema)] cannot describe a field with \
                     `#[serde({key} = ...)]`: the adapter decides what actually reaches the \
                     wire, so a schema built from the field's Rust type would advertise a \
                     shape serde neither writes nor accepts. Write the `OpenApiSchema` impl \
                     by hand and register it with `OpenApiConfig::register_schema`."
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
        // Direction-dependent for the same reason, but only when nothing else
        // already makes omission valid on the REQUEST side. Three things can:
        // an `Option<T>` type, a field `#[serde(default)]`, or a container
        // `#[serde(default)]`. With any of them the rejection's own premise --
        // "serde still rejects a request that omits it" -- is simply false:
        // deserialization fills the field in, serialization may omit it, and a
        // not-`required` property describes both directions accurately. The
        // emitter below already marks such a field optional, so refusing it here
        // rejected a type it could in fact describe.
        if crate::schema::field_has_skip_serializing_if(field)
            && !crate::schema::is_option_type(&field.ty)
            && !crate::schema::has_serde_default(&field.attrs)
            && !crate::schema::has_serde_default(&input.attrs)
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
/// Refuse a unit variant carrying `#[serde(alias = "…")]`.
///
/// Split out of [`reject_undescribable_enum`] to keep that function within the
/// crate's line budget; it is one rule, checked in one place.
fn reject_aliased_variants(data: &syn::DataEnum) -> syn::Result<()> {
    // `#[serde(alias = "…")]` is a DESERIALIZE-only widening: the alias is
    // accepted on input and never written on output. A closed string set built
    // from the canonical spellings is therefore too narrow for a request (an
    // OpenAPI validator rejects a value the handler happily takes) and listing
    // the aliases would make it too wide for a response (advertising values the
    // server never emits). Same asymmetry as a directional skip, same answer.
    if let Some(variant) = data
        .variants
        .iter()
        .find(|v| crate::schema::variant_has_serde_alias(v))
    {
        return Err(syn::Error::new_spanned(
            variant,
            "#[derive(OpenApiSchema)] cannot describe a variant with `#[serde(alias = \"…\")]`: \
             the alias is accepted when deserializing but never written when serializing, so one \
             string set cannot be right for both requests and responses — listing it advertises \
             a value the server never emits, omitting it rejects one the handler accepts. Use \
             `#[serde(rename = \"…\")]` if the wire spelling should change in both directions, \
             or write the `OpenApiSchema` impl by hand and register it with \
             `OpenApiConfig::register_schema`.",
        ));
    }
    Ok(())
}

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

    reject_aliased_variants(data)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    /// Run the struct audit over a fixture, returning the rejection message.
    fn audit(input: &DeriveInput) -> Result<(), String> {
        let Data::Struct(data) = &input.data else {
            panic!("fixture must be a struct");
        };
        let Fields::Named(named) = &data.fields else {
            panic!("fixture must have named fields");
        };
        reject_undescribable_struct(input, named).map_err(|e| e.to_string())
    }

    /// A serialization adapter decides what actually reaches the wire, so the
    /// field's Rust type stops describing it. Rejected rather than guessed:
    /// publishing the Rust shape would be confidently wrong.
    #[test]
    fn a_field_serialize_adapter_is_refused() {
        let err = audit(&parse_quote! {
            struct Money {
                #[serde(serialize_with = "as_string")]
                amount: i64,
            }
        })
        .expect_err("a serialize adapter must be refused");
        assert!(
            err.contains("serialize_with"),
            "the message must name the attribute it refused: {err}"
        );
    }

    /// `with` sets both directions at once, so it is wrong for both.
    #[test]
    fn a_field_with_adapter_is_refused() {
        let err = audit(&parse_quote! {
            struct Money {
                #[serde(with = "string_money")]
                amount: i64,
            }
        })
        .expect_err("a `with` adapter must be refused");
        assert!(err.contains("with"), "{err}");
    }

    /// The deserialize half is refused too, and deliberately: this one schema
    /// serves requests as well as responses, so an adapter on either side makes
    /// it wrong for that side. (`#[model]`'s read schema, which describes only a
    /// response, keeps `deserialize_with` for exactly that reason.)
    #[test]
    fn a_field_deserialize_adapter_is_refused_too() {
        let err = audit(&parse_quote! {
            struct Money {
                #[serde(deserialize_with = "lenient")]
                amount: i64,
            }
        })
        .expect_err("a deserialize adapter must be refused");
        assert!(err.contains("deserialize_with"), "{err}");
    }

    /// The guard must not fire on an ordinary struct — over-rejection would
    /// break every existing derive.
    #[test]
    fn a_plain_struct_still_passes() {
        audit(&parse_quote! {
            struct Plain {
                id: i64,
                #[serde(rename = "displayName")]
                name: String,
                tags: Option<Vec<String>>,
            }
        })
        .expect("a plain struct must still be describable");
    }

    /// `skip_serializing_if` alone IS direction-dependent: the response may
    /// omit the field while serde still demands it on a request.
    #[test]
    fn skip_serializing_if_alone_is_still_refused() {
        let err = audit(&parse_quote! {
            struct Row {
                #[serde(skip_serializing_if = "String::is_empty")]
                tags: String,
            }
        })
        .expect_err("a bare skip_serializing_if on a non-Option field must be refused");
        assert!(err.contains("skip_serializing_if"), "{err}");
    }

    /// Adding a field `#[serde(default)]` makes omission valid in BOTH
    /// directions, so the type becomes describable and must not be refused.
    #[test]
    fn a_field_default_rescues_skip_serializing_if() {
        audit(&parse_quote! {
            struct Row {
                #[serde(default, skip_serializing_if = "String::is_empty")]
                tags: String,
            }
        })
        .expect("default + skip_serializing_if is describable");
    }

    /// The valued spelling counts too.
    #[test]
    fn a_valued_field_default_rescues_skip_serializing_if() {
        audit(&parse_quote! {
            struct Row {
                #[serde(default = "empty_tags", skip_serializing_if = "String::is_empty")]
                tags: String,
            }
        })
        .expect("default = \"path\" + skip_serializing_if is describable");
    }

    /// A CONTAINER default covers every field, so it rescues them too.
    #[test]
    fn a_container_default_rescues_skip_serializing_if() {
        audit(&parse_quote! {
            #[serde(default)]
            struct Row {
                #[serde(skip_serializing_if = "String::is_empty")]
                tags: String,
            }
        })
        .expect("a container default makes every field omissible on input");
    }

    fn audit_enum(input: &DeriveInput) -> Result<(), String> {
        let Data::Enum(data) = &input.data else {
            panic!("fixture must be an enum");
        };
        reject_undescribable_enum(input, data).map_err(|e| e.to_string())
    }

    /// `alias` widens what deserialization ACCEPTS without changing what
    /// serialization writes, so no single string set is right for both
    /// directions.
    #[test]
    fn a_variant_alias_is_refused() {
        let err = audit_enum(&parse_quote! {
            enum Status {
                Active,
                #[serde(alias = "legacy")]
                Retired,
            }
        })
        .expect_err("an aliased variant must be refused");
        assert!(err.contains("alias"), "{err}");
    }

    /// The scan must survive a sibling list-valued attribute — the parser bug
    /// class that has bitten this crate before, where an unconsumed `bound(..)`
    /// aborts the walk and the attribute after it reads as absent.
    #[test]
    fn an_alias_after_a_list_valued_attribute_is_still_seen() {
        let err = audit_enum(&parse_quote! {
            #[serde(bound(deserialize = "T: Clone"))]
            enum Status {
                Active,
                #[serde(bound(deserialize = "T: Clone"), alias = "legacy")]
                Retired,
            }
        })
        .expect_err("an alias behind a list-valued sibling must still be found");
        assert!(err.contains("alias"), "{err}");
    }

    /// `rename` changes BOTH directions, so it stays describable — the guard
    /// must not swallow the ordinary case.
    #[test]
    fn a_renamed_variant_is_still_describable() {
        audit_enum(&parse_quote! {
            enum Status {
                Active,
                #[serde(rename = "retired")]
                Retired,
            }
        })
        .expect("a symmetric rename must still be describable");
    }

    /// A neighbouring serde key that merely *contains* an adapter name is not
    /// one: the scan matches whole keys.
    #[test]
    fn a_lookalike_key_is_not_an_adapter() {
        audit(&parse_quote! {
            struct Plain {
                #[serde(default = "with_default")]
                amount: i64,
            }
        })
        .expect("`default = \"with_default\"` is not an adapter");
    }
}
