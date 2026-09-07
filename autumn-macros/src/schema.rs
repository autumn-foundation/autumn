//! Field-level serde / JSON-schema helpers shared by the `#[model]` macro and
//! the `#[derive(OpenApiSchema)]` derive.
//!
//! These live in their own module (rather than inside [`crate::model`]) so the
//! always-compiled `OpenApiSchema` derive does not drag the database-oriented
//! `#[model]` codegen — which is gated behind the crate's `db` feature — into
//! a no-database build. See `openapi_schema.rs` for the derive and `model.rs`
//! for the macro; both go through the helpers here so a schema advertised by
//! one matches the schema advertised by the other.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Field;

/// Whether a field carries the named marker attribute (e.g. `#[id]`).
pub fn has_attr(field: &Field, name: &str) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident(name))
}

/// Whether a field is declared `#[translatable]` (issue #1384): its column
/// holds an `autumn_web::i18n::Translated` container — an independent value
/// per locale tag — instead of a single monolingual string.
pub fn field_is_translatable(field: &syn::Field) -> bool {
    has_attr(field, "translatable")
}

/// The struct-level `#[serde(rename_all = "...")]` casing rule that applies
/// to *serialization*, if any. Handles both the plain form and the split
/// `rename_all(serialize = "...", deserialize = "...")` form (taking the
/// `serialize` side — that is what `Changeset::field_value` indexes by).
///
/// Same parsing convention as `field_has_serde_rename`: a `#[serde(...)]`
/// list this parser can't fully walk simply yields no rule (the real serde
/// derive still validates the attribute itself).
pub fn serde_rename_all_serialize_rule(attrs: &[syn::Attribute]) -> Option<String> {
    let mut rule = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                if let Ok(value) = meta.value() {
                    // rename_all = "camelCase"
                    if let Ok(syn::Lit::Str(s)) = value.parse::<syn::Lit>() {
                        rule = Some(s.value());
                    }
                } else {
                    // rename_all(serialize = "...", deserialize = "...")
                    let _ = meta.parse_nested_meta(|inner| {
                        if let Ok(value) = inner.value()
                            && let Ok(syn::Lit::Str(s)) = value.parse::<syn::Lit>()
                            && inner.path.is_ident("serialize")
                        {
                            rule = Some(s.value());
                        }
                        Ok(())
                    });
                }
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    rule
}

/// The field-level `#[serde(rename = "...")]` name that applies to
/// *serialization*, if any. Handles both the plain form and the split
/// `rename(serialize = "...", deserialize = "...")` form (taking the
/// `serialize` side). Field-level `rename` overrides a struct-level
/// `rename_all`, mirroring serde's own precedence.
pub fn field_serde_serialize_rename(field: &syn::Field) -> Option<String> {
    let mut renamed = None;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                if let Ok(value) = meta.value() {
                    // rename = "headline"
                    if let Ok(syn::Lit::Str(s)) = value.parse::<syn::Lit>() {
                        renamed = Some(s.value());
                    }
                } else {
                    // rename(serialize = "...", deserialize = "...")
                    let _ = meta.parse_nested_meta(|inner| {
                        if let Ok(value) = inner.value()
                            && let Ok(syn::Lit::Str(s)) = value.parse::<syn::Lit>()
                            && inner.path.is_ident("serialize")
                        {
                            renamed = Some(s.value());
                        }
                        Ok(())
                    });
                }
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    renamed
}

/// Apply a struct-level `#[serde(rename_all = "...")]` casing rule to a
/// (`snake_case`) field identifier, mirroring `serde_derive`'s
/// `RenameRule::apply_to_field`. Returns `None` for a rule string serde
/// itself would reject (the `Serialize` derive on the emitted struct then
/// reports the error — no point duplicating it here).
pub fn apply_serde_rename_all_rule(rule: &str, field: &str) -> Option<String> {
    fn pascal(field: &str) -> String {
        field
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
            })
            .collect()
    }
    match rule {
        // serde treats fields as already snake_case/lowercase.
        "lowercase" | "snake_case" => Some(field.to_owned()),
        "UPPERCASE" | "SCREAMING_SNAKE_CASE" => Some(field.to_ascii_uppercase()),
        "PascalCase" => Some(pascal(field)),
        "camelCase" => {
            let pascal = pascal(field);
            let mut chars = pascal.chars();
            chars
                .next()
                .map(|first| first.to_lowercase().collect::<String>() + chars.as_str())
        }
        "kebab-case" => Some(field.replace('_', "-")),
        "SCREAMING-KEBAB-CASE" => Some(field.to_ascii_uppercase().replace('_', "-")),
        _ => None,
    }
}

/// Whether a `#[serde(...)]` attribute list carries a bare word from `words`.
///
/// For the marker attributes that take no value — `transparent`, `flatten`,
/// `skip`, `skip_serializing`, `skip_deserializing`, `untagged`, `default` in
/// its bare form. Returns the first match, so callers can name it in a
/// diagnostic.
pub fn serde_bare_word(attrs: &[syn::Attribute], words: &[&'static str]) -> Option<&'static str> {
    let mut found = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if let Some(word) = words.iter().find(|w| meta.path.is_ident(w)) {
                found = Some(*word);
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    found
}

/// Whether a `#[serde(...)]` attribute list carries `key = "..."` for any key in
/// `keys`, returning the first match.
///
/// For the value-taking attributes that change the wire shape: `into`, `from`,
/// `try_from`, `tag`, `content`, and the field-level `default = "path"`.
pub fn serde_valued_key(attrs: &[syn::Attribute], keys: &[&'static str]) -> Option<&'static str> {
    let mut found = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if let Some(key) = keys.iter().find(|k| meta.path.is_ident(k)) {
                found = Some(*key);
            }
            consume_unrecognized_meta(&meta)?;
            Ok(())
        });
    }
    found
}

/// Whether a field carries `#[serde(skip_serializing_if = "...")]`, so a
/// response omits it whenever the predicate matches.
///
/// Distinct from an unconditional `skip` / `skip_serializing`: the field DOES
/// appear in some responses, so its property belongs in the schema — it simply
/// cannot be `required`, because a legitimate response may leave it out.
pub fn field_has_skip_serializing_if(field: &syn::Field) -> bool {
    let mut conditional = false;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip_serializing_if") {
                conditional = true;
            }
            consume_unrecognized_meta(&meta)?;
            Ok(())
        });
    }
    conditional
}

/// Apply an enum-level `#[serde(rename_all = "...")]` casing rule to a
/// (`PascalCase`) variant identifier, mirroring `serde_derive`'s
/// `RenameRule::apply_to_variant`.
///
/// Deliberately NOT routed through [`apply_serde_rename_all_rule`]: that helper
/// takes an already-`snake_case` *field* name, so its `lowercase`/`snake_case`
/// arms are identity. A variant arrives in `PascalCase`, so each rule needs the
/// serde variant algorithm instead — `InProgress` must become `in_progress`
/// under `snake_case` and `inprogress` (not `in_progress`) under `lowercase`.
///
/// Returns `None` for a rule string serde itself would reject; the `Serialize`
/// derive on the same enum then reports the error, so this does not duplicate it.
pub fn apply_serde_rename_all_rule_to_variant(rule: &str, variant: &str) -> Option<String> {
    // serde's own variant→snake_case: insert `_` before every uppercase char
    // after the first, then lowercase. (`XMLHttpRequest` → `x_m_l_http_request`,
    // matching serde exactly rather than guessing at acronym runs.)
    fn snake(variant: &str) -> String {
        let mut out = String::with_capacity(variant.len() + 4);
        for (i, ch) in variant.char_indices() {
            if i > 0 && ch.is_uppercase() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        }
        out
    }
    match rule {
        "lowercase" => Some(variant.to_ascii_lowercase()),
        "UPPERCASE" => Some(variant.to_ascii_uppercase()),
        "PascalCase" => Some(variant.to_owned()),
        "camelCase" => {
            let mut chars = variant.chars();
            chars
                .next()
                .map(|first| first.to_lowercase().collect::<String>() + chars.as_str())
        }
        "snake_case" => Some(snake(variant)),
        "SCREAMING_SNAKE_CASE" => Some(snake(variant).to_ascii_uppercase()),
        "kebab-case" => Some(snake(variant).replace('_', "-")),
        "SCREAMING-KEBAB-CASE" => Some(snake(variant).to_ascii_uppercase().replace('_', "-")),
        _ => None,
    }
}

/// A container-level `#[serde(...)]` enum representation other than serde's
/// default (externally tagged), as the attribute word that selected it.
///
/// Each of these changes what a *unit* variant serializes to, so a schema
/// generator that ignores them advertises the wrong wire shape:
///
/// | Attribute | A unit variant serializes as |
/// |---|---|
/// | *(default, externally tagged)* | `"Variant"` — a JSON string |
/// | `#[serde(tag = "t")]` | `{"t": "Variant"}` — an object |
/// | `#[serde(tag = "t", content = "c")]` | `{"t": "Variant"}` — an object |
/// | `#[serde(untagged)]` | `null` |
/// | `#[serde(into = "u8")]` / `from` / `try_from` | whatever the conversion type serializes as |
///
/// The conversion attributes belong here for the same reason: serde routes the
/// value through another type entirely, so the variant names never reach the
/// wire and a string-enum schema would describe a payload the handler does not
/// accept.
///
/// Returns `None` for the default representation.
pub fn serde_enum_representation(attrs: &[syn::Attribute]) -> Option<&'static str> {
    let mut found = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            // `tag` wins the report when both `tag` and `content` are present:
            // it is the one that changes a unit variant's shape, and naming it
            // keeps the diagnostic pointing at the cause.
            if meta.path.is_ident("tag") {
                found = Some("tag");
            } else if meta.path.is_ident("untagged") {
                found = Some("untagged");
            } else if meta.path.is_ident("into") {
                found = Some("into");
            } else if meta.path.is_ident("from") {
                found = Some("from");
            } else if meta.path.is_ident("try_from") {
                found = Some("try_from");
            } else if meta.path.is_ident("content") && found.is_none() {
                found = Some("content");
            }
            consume_unrecognized_meta(&meta)?;
            Ok(())
        });
    }
    found
}

/// Consume whatever follows an unrecognized `#[serde(...)]` key so
/// `parse_nested_meta` can reach the keys that come after it.
///
/// Two shapes have to be swallowed, not one. `key = "value"` is the obvious
/// case. The other is a **list**, `key(a = "x", b = "y")` — and missing it is
/// not cosmetic: `meta.value()` fails on a list (there is no `=`), so the
/// parenthesized group stays unread, `parse_nested_meta` aborts on it, and
/// every later key goes unvisited. A caller that swallows the resulting error
/// then sees a clean "nothing found".
///
/// That is exactly how `#[serde(rename_all(serialize = "snake_case"), tag =
/// "kind")]` slipped past [`serde_enum_representation`]: `tag` was never
/// reached, so an internally tagged enum was advertised as a plain string enum.
/// Anything that gates on absence must therefore consume both shapes.
fn consume_unrecognized_meta(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    if let Ok(value) = meta.value() {
        let _: syn::Result<syn::Lit> = value.parse();
    } else if meta.input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in meta.input);
        let _: proc_macro2::TokenStream = content.parse()?;
    }
    Ok(())
}

/// The serde attributes on an enum variant, read for `rename` / `skip`.
///
/// Mirrors [`field_serde_serialize_rename`] but over a
/// [`syn::Variant`](syn::Variant)'s attribute list.
pub fn variant_serde_serialize_rename(variant: &syn::Variant) -> Option<String> {
    let mut renamed = None;
    for attr in variant.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                if let Ok(value) = meta.value() {
                    if let Ok(syn::Lit::Str(s)) = value.parse::<syn::Lit>() {
                        renamed = Some(s.value());
                    }
                } else {
                    let _ = meta.parse_nested_meta(|inner| {
                        if let Ok(value) = inner.value()
                            && let Ok(syn::Lit::Str(s)) = value.parse::<syn::Lit>()
                            && inner.path.is_ident("serialize")
                        {
                            renamed = Some(s.value());
                        }
                        Ok(())
                    });
                }
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    renamed
}

/// Whether a `#[serde(...)]` attribute list carries a **split** `rename_all` or
/// `rename` — the `name(serialize = "...", deserialize = "...")` form — where
/// the two sides disagree.
///
/// A symmetric `rename_all = "snake_case"` applies to both directions and is
/// exact. The split form is not: the schema can only advertise one string, so a
/// generated client sends the serialize spelling while the handler's
/// `Deserialize` accepts the other. Same asymmetry as a directional skip, same
/// answer — refuse rather than publish a value that only works one way.
///
/// Returns the attribute word (`rename_all` / `rename`) when the two sides are
/// present and differ.
pub fn serde_split_rename(attrs: &[syn::Attribute], key: &'static str) -> Option<&'static str> {
    let mut split = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(key)
                && meta.value().is_err()
                && meta.input.peek(syn::token::Paren)
            {
                let (mut ser, mut de) = (None::<String>, None::<String>);
                meta.parse_nested_meta(|inner| {
                    if let Ok(value) = inner.value()
                        && let Ok(syn::Lit::Str(lit)) = value.parse::<syn::Lit>()
                    {
                        if inner.path.is_ident("serialize") {
                            ser = Some(lit.value());
                        } else if inner.path.is_ident("deserialize") {
                            de = Some(lit.value());
                        }
                    }
                    Ok(())
                })?;
                // Asymmetric in either shape. Both sides present and
                // disagreeing is the obvious one. ONE side present is equally
                // asymmetric and easier to miss: `rename_all(serialize =
                // "snake_case")` renames only the output, so serde still
                // DESERIALIZES the original spelling — advertising the
                // serialize side would have a client send a value the handler
                // rejects. Only a split whose two sides are spelled the same
                // round-trips, and that is the sole accepted case.
                match (ser, de) {
                    (Some(ser), Some(de)) if ser == de => {}
                    (None, None) => {}
                    _ => split = Some(key),
                }
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    split
}

/// A **directional** skip on a variant — `skip_serializing` or
/// `skip_deserializing` — returned as the attribute word.
///
/// One schema describes both directions, so a variant present in only one of
/// them has no correct rendering. `skip_deserializing` is the dangerous
/// direction: the variant IS serialized, so a serialize-side schema advertises
/// it, and a client that sends it back gets an unknown-variant error from
/// serde. `skip_serializing` is the mirror — dropping it would deny an input
/// the handler accepts. Neither can be inferred away, so the derive refuses the
/// enum instead of publishing a half-true set. Plain `#[serde(skip)]` is
/// unambiguous (gone from both directions) and stays supported.
pub fn variant_directional_skip(variant: &syn::Variant) -> Option<&'static str> {
    let mut found = None;
    for attr in variant.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip_serializing") {
                found = Some("skip_serializing");
            } else if meta.path.is_ident("skip_deserializing") {
                found = Some("skip_deserializing");
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    found
}

/// The field-level twin of [`variant_directional_skip`], with the same reasoning:
/// a field present in only one serde direction has no correct rendering in a
/// schema that describes both.
pub fn variant_directional_skip_on_field(field: &syn::Field) -> Option<&'static str> {
    serde_bare_word(&field.attrs, &["skip_serializing", "skip_deserializing"])
}

/// Whether a variant carries `#[serde(skip)]`, in which case it never appears
/// on the wire in either direction and must not be advertised.
pub fn variant_is_serde_skipped(variant: &syn::Variant) -> bool {
    let mut skipped = false;
    for attr in variant.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                skipped = true;
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    skipped
}

/// The JSON-schema property name a field serializes to, honoring serde attrs.
///
/// Precedence mirrors serde: a field-level `#[serde(rename = "...")]` wins over
/// a container `#[serde(rename_all = "...")]`, which in turn overrides the raw
/// identifier. The raw-ident prefix (`r#`) is stripped first, so a field
/// `r#type` advertises the property name `"type"` (what the handler actually
/// deserializes), never the literal `"r#type"`.
///
/// KNOWN LIMITATION: this uses the *serialize* side of a split
/// `#[serde(rename(serialize = ..., deserialize = ...))]` /
/// `#[serde(rename_all(serialize = ..., deserialize = ...))]`. For the common
/// symmetric `rename` / `rename_all` (which apply to both sides) this is exact;
/// only the rare split-form input struct could differ between the advertised
/// schema and the deserialized wire name. This is deliberate: it keeps the
/// `#[derive(OpenApiSchema)]`, `#[model]`, and `FormModel` code paths in
/// lockstep on the same serde helpers rather than duplicating a
/// deserialize-side variant.
pub fn schema_property_name(field: &syn::Field, rename_all_rule: Option<&str>) -> Option<String> {
    let ident = field.ident.as_ref()?;
    let raw = ident.to_string();
    let raw = raw.strip_prefix("r#").unwrap_or(&raw).to_owned();
    Some(
        field_serde_serialize_rename(field)
            .or_else(|| rename_all_rule.and_then(|rule| apply_serde_rename_all_rule(rule, &raw)))
            .unwrap_or(raw),
    )
}

/// Check whether a type is `Option<...>`.
pub fn is_option_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(tp) = ty {
        tp.path
            .segments
            .last()
            .is_some_and(|seg| seg.ident == "Option")
    } else {
        false
    }
}

/// Return the final path segment name of a type (e.g. `foo::Bar` → `"Bar"`).
pub fn type_name_str(ty: &syn::Type) -> String {
    crate::api_doc::last_segment_name(ty).unwrap_or_else(|| "unknown".to_owned())
}

/// Emit the JSON-Schema `TokenStream` for one model field.
///
/// Identical to [`emit_json_schema_tokens`] except that a `#[translatable]`
/// field (issue #1384) is described inline as a locale-tag→string map, which is
/// exactly what its lossless `Serialize` emits. Without that, the field would
/// fall through to the `$ref` branch and `autumn openapi` would ship a spec
/// referencing a component nothing registers.
///
/// Keyed on the **attribute**, never on the type's name: an application type
/// that merely happens to be called `Translated` (`domain::Translated`) keeps
/// its ordinary `$ref`, so the advertised contract cannot silently disagree
/// with what that type actually serializes to.
pub fn emit_json_schema_tokens_for_field(field: &Field) -> TokenStream {
    if field_is_translatable(field) {
        return quote! {
            ::autumn_web::reexports::serde_json::json!({
                "type": "object",
                "description": "Per-locale content, keyed by locale tag (issue #1384).",
                "additionalProperties": { "type": "string" }
            })
        };
    }
    emit_json_schema_tokens(&field.ty)
}

/// Emit a `TokenStream` that evaluates (at runtime) to a `serde_json::Value`
/// representing the JSON Schema for the given Rust type.
///
/// Handles `Option<T>` (nullable), `Vec<T>` (array), primitives (`String`,
/// `i64`, etc.), and everything else as a `$ref` to a component schema.
pub fn emit_json_schema_tokens(ty: &syn::Type) -> TokenStream {
    // Option<T> → OpenAPI 3.1 nullable: oneOf [{T-schema}, {type:null}]
    if let Some(inner) = crate::api_doc::unwrap_single_generic(ty, "Option") {
        let inner_tokens = emit_json_schema_tokens(&inner);
        return quote! {{
            let __inner = #inner_tokens;
            ::autumn_web::reexports::serde_json::json!({ "oneOf": [__inner, { "type": "null" }] })
        }};
    }

    // Vec<T> → {"type": "array", "items": <T-schema>}
    if let Some(inner) = crate::api_doc::unwrap_single_generic(ty, "Vec") {
        let inner_tokens = emit_json_schema_tokens(&inner);
        return quote! {{
            let __items = #inner_tokens;
            ::autumn_web::reexports::serde_json::json!({ "type": "array", "items": __items })
        }};
    }

    let name = type_name_str(ty);

    // Types that serialize as a JSON scalar despite not being Rust primitives.
    // Without this they fall through to the `$ref` branch below and the spec
    // carries a dangling component nothing registers — which the back-fill then
    // resolves to the opaque object placeholder. `created_at` / `updated_at`
    // columns make `NaiveDateTime` near-universal across `#[model]` types, so
    // this was one untyped field on almost every model on an API boundary
    // (issue #802). Each maps to the standard OpenAPI `format` for what serde
    // actually writes.
    if let Some((json_type, format, description)) = scalar_json_schema(&name) {
        let format_insert = format.map(|f| {
            quote! { __scalar.insert("format".to_owned(), #f.into()); }
        });
        let description_insert = description.map(|d| {
            quote! { __scalar.insert("description".to_owned(), #d.into()); }
        });
        // Matching is on the type's LAST PATH SEGMENT, because a proc macro sees
        // only the tokens as written and `use chrono::NaiveDateTime;` is the
        // normal spelling. An application type that happens to share one of
        // these names would otherwise be described as the external scalar, so
        // check the derived-schema inventory FIRST at runtime: a colliding type
        // carrying `#[derive(OpenApiSchema)]` resolves to its own real schema,
        // and only a type nothing registered falls through to the scalar. (The
        // same last-segment limitation already governs `primitive_json_type`
        // for `String`, `bool` and the numerics.)
        return quote! {{
            match ::autumn_web::openapi::registered_derived_schema(
                ::core::any::type_name::<#ty>()
            ) {
                ::core::option::Option::Some(__derived) => __derived,
                ::core::option::Option::None => {
                    let mut __scalar = ::autumn_web::reexports::serde_json::Map::new();
                    __scalar.insert("type".to_owned(), #json_type.into());
                    #format_insert
                    #description_insert
                    ::autumn_web::reexports::serde_json::Value::Object(__scalar)
                }
            }
        }};
    }

    crate::api_doc::primitive_json_type(&name).map_or_else(
        || {
            // Emit the `$ref` against the field type's FULL `type_name` identity
            // (built at runtime), NOT its short last segment, so the finalize
            // collision index can match this nested ref to the exact producing
            // type and rewrite it to the same display key the top-level route
            // refs use — even when two types share a last segment (issue #1972).
            quote! {{
                let __ref_path = ::std::format!(
                    "#/components/schemas/{}",
                    ::core::any::type_name::<#ty>()
                );
                ::autumn_web::reexports::serde_json::json!({ "$ref": __ref_path })
            }}
        },
        |json_type| {
            quote! { ::autumn_web::reexports::serde_json::json!({ "type": #json_type }) }
        },
    )
}

/// JSON-Schema `type`, optional `format`, and optional `description` for a
/// non-primitive type that nevertheless serializes as a single scalar.
///
/// Deliberately narrow: only types whose serde output is unambiguous.
/// Numeric-adjacent wrappers (`Decimal`, `BigDecimal`) are left out on purpose —
/// whether they serialize as a JSON number or a string depends on which serde
/// feature the app enabled, and an opaque placeholder beats a confidently wrong
/// scalar.
///
/// The **naive** chrono types deliberately carry NO `format`. `OpenAPI`'s
/// `date-time` and `time` are RFC 3339 productions that *require* a UTC offset,
/// but `NaiveDateTime` / `NaiveTime` serialize without one
/// (`2026-09-06T18:00:00`). Claiming the standard format would make a strict
/// validator reject the server's real payload, and lead a generator to emit a
/// timezone-aware client type that cannot parse it. A bare `string` plus a
/// description is less specific but true. `NaiveDate` keeps `date`, whose RFC
/// 3339 production (`full-date`) has no offset to begin with, and `DateTime<Tz>`
/// keeps `date-time` because chrono does write an offset for it.
fn scalar_json_schema(
    name: &str,
) -> Option<(&'static str, Option<&'static str>, Option<&'static str>)> {
    Some(match name {
        // `DateTime<Utc>` reaches here as its last path segment, `DateTime`.
        "DateTime" => ("string", Some("date-time"), None),
        "NaiveDate" => ("string", Some("date"), None),
        "NaiveDateTime" => (
            "string",
            None,
            Some("ISO 8601 date-time with no UTC offset, e.g. 2026-09-06T18:00:00"),
        ),
        "NaiveTime" => (
            "string",
            None,
            Some("ISO 8601 time with no UTC offset, e.g. 18:00:00"),
        ),
        "Uuid" => ("string", Some("uuid"), None),
        _ => return None,
    })
}

/// Emit the body of `OpenApiSchema::schema()` for a list of fields.
///
/// `all_optional` is `true` for `Update*` structs where every field is
/// conceptually optional (backed by `Patch<T>`); `extra_required` names fields
/// to force into the `required` set; and `treat_as_optional` names fields that
/// must NOT be `required` even though their type is not `Option<T>`.
///
/// Requiredness has to follow what the generated `Deserialize` accepts, not what
/// the Rust type looks like. `#[model]` puts `#[serde(default)]` on a
/// non-`Option` `bool` in the `New*` struct, so a POST body may omit it and get
/// `false` — advertising it as required would force a generated client to send a
/// value the server does not need (issue #802).
pub fn emit_schema_fn_body_full(
    fields: &[&&Field],
    all_optional: bool,
    extra_required: &[&&Field],
    rename_all_rule: Option<&str>,
    treat_as_optional: &dyn Fn(&Field) -> bool,
) -> TokenStream {
    emit_schema_fn_body_named(
        fields,
        all_optional,
        extra_required,
        rename_all_rule,
        treat_as_optional,
        false,
    )
}

/// As [`emit_schema_fn_body_full`], plus `raw_field_names`: advertise each
/// property under its bare Rust identifier, ignoring every serde rename.
///
/// Needed for the `New*` / `Update*` companions. Those structs deliberately do
/// NOT inherit the model's `#[serde(rename_all)]` or field-level
/// `#[serde(rename)]` — a behaviour pinned by
/// `autumn/tests/integration/form_for_derive.rs` — so a schema built with the
/// model's rename metadata would advertise `authorName` for a body serde only
/// accepts as `author_name`, and every generated client's POST would fail with
/// a missing-field error (issue #802).
pub fn emit_schema_fn_body_named(
    fields: &[&&Field],
    all_optional: bool,
    extra_required: &[&&Field],
    rename_all_rule: Option<&str>,
    treat_as_optional: &dyn Fn(&Field) -> bool,
    raw_field_names: bool,
) -> TokenStream {
    let resolve_name = |f: &Field| -> Option<String> {
        if raw_field_names {
            let raw = f.ident.as_ref()?.to_string();
            Some(raw.strip_prefix("r#").unwrap_or(&raw).to_owned())
        } else {
            schema_property_name(f, rename_all_rule)
        }
    };
    // Resolve each field's advertised property name once — through the shared
    // serde helpers so the schema honors `#[serde(rename)]` /
    // `#[serde(rename_all)]` and strips raw-ident `r#` prefixes — and reuse the
    // same resolved name for BOTH the property key and the `required` entry, so
    // the two can never drift.
    let insertions: Vec<TokenStream> = fields
        .iter()
        .chain(extra_required.iter())
        .map(|f| {
            let field_name =
                resolve_name(f).unwrap_or_else(|| f.ident.as_ref().unwrap().to_string());
            let schema_expr = emit_json_schema_tokens_for_field(f);
            quote! {
                __props.insert(#field_name.to_owned(), #schema_expr);
            }
        })
        .collect();

    let mut required_names: Vec<String> = if all_optional {
        Vec::new()
    } else {
        fields
            .iter()
            .filter(|f| !is_option_type(&f.ty) && !treat_as_optional(f))
            .filter_map(|f| resolve_name(f))
            .collect()
    };
    for f in extra_required {
        if let Some(name) = resolve_name(f) {
            required_names.push(name);
        }
    }

    let required_tokens: Vec<TokenStream> = required_names
        .iter()
        .map(|name| {
            quote! { ::autumn_web::reexports::serde_json::json!(#name) }
        })
        .collect();

    quote! {
        let mut __props = ::autumn_web::reexports::serde_json::Map::new();
        #(#insertions)*
        let mut __schema = ::autumn_web::reexports::serde_json::Map::new();
        __schema.insert(
            "type".to_owned(),
            ::autumn_web::reexports::serde_json::json!("object"),
        );
        __schema.insert(
            "properties".to_owned(),
            ::autumn_web::reexports::serde_json::Value::Object(__props),
        );
        let __required: ::std::vec::Vec<::autumn_web::reexports::serde_json::Value> =
            ::std::vec![#(#required_tokens),*];
        if !__required.is_empty() {
            __schema.insert(
                "required".to_owned(),
                ::autumn_web::reexports::serde_json::Value::Array(__required),
            );
        }
        ::autumn_web::reexports::serde_json::Value::Object(__schema)
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;
    use syn::parse::Parser as _;

    use super::*;

    #[test]
    fn field_serde_serialize_rename_parses_plain_and_split_forms() {
        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { #[serde(rename = "headline")] pub title: String })
            .unwrap();
        assert_eq!(
            field_serde_serialize_rename(&field).as_deref(),
            Some("headline")
        );

        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! {
                #[serde(rename(serialize = "out", deserialize = "in"))]
                pub title: String
            })
            .unwrap();
        assert_eq!(field_serde_serialize_rename(&field).as_deref(), Some("out"));

        // Deserialize-only rename leaves the serialized key alone.
        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { #[serde(rename(deserialize = "in"))] pub title: String })
            .unwrap();
        assert_eq!(field_serde_serialize_rename(&field), None);

        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { #[serde(default)] pub title: String })
            .unwrap();
        assert_eq!(field_serde_serialize_rename(&field), None);
    }

    #[test]
    fn schema_property_name_resolves_renames_and_strips_raw_idents() {
        // field rename wins over rename_all.
        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { #[serde(rename = "kind")] pub category: String })
            .unwrap();
        assert_eq!(
            schema_property_name(&field, Some("camelCase")).as_deref(),
            Some("kind")
        );

        // container rename_all applies when there is no field rename.
        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { pub word_count: i64 })
            .unwrap();
        assert_eq!(
            schema_property_name(&field, Some("camelCase")).as_deref(),
            Some("wordCount")
        );

        // raw-ident prefix is stripped (advertise the wire name).
        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { pub r#type: String })
            .unwrap();
        assert_eq!(schema_property_name(&field, None).as_deref(), Some("type"));

        // no rule, plain field → the identifier verbatim.
        let field: syn::Field = syn::Field::parse_named
            .parse2(quote! { pub title: String })
            .unwrap();
        assert_eq!(schema_property_name(&field, None).as_deref(), Some("title"));
    }

    #[test]
    fn serde_rename_all_serialize_rule_parses_plain_and_split_forms() {
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[serde(rename_all = "camelCase")])];
        assert_eq!(
            serde_rename_all_serialize_rule(&attrs).as_deref(),
            Some("camelCase")
        );

        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(
            #[serde(rename_all(serialize = "kebab-case", deserialize = "camelCase"))]
        )];
        assert_eq!(
            serde_rename_all_serialize_rule(&attrs).as_deref(),
            Some("kebab-case")
        );

        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(#[serde(deny_unknown_fields)])];
        assert_eq!(serde_rename_all_serialize_rule(&attrs), None);
    }

    #[test]
    fn apply_serde_rename_all_rule_mirrors_serde_field_casings() {
        let cases = [
            ("lowercase", "word_count", "word_count"),
            ("snake_case", "word_count", "word_count"),
            ("UPPERCASE", "word_count", "WORD_COUNT"),
            ("SCREAMING_SNAKE_CASE", "word_count", "WORD_COUNT"),
            ("PascalCase", "word_count", "WordCount"),
            ("camelCase", "word_count", "wordCount"),
            ("camelCase", "title", "title"),
            ("kebab-case", "word_count", "word-count"),
            ("SCREAMING-KEBAB-CASE", "word_count", "WORD-COUNT"),
        ];
        for (rule, field, expected) in cases {
            assert_eq!(
                apply_serde_rename_all_rule(rule, field).as_deref(),
                Some(expected),
                "rule {rule} on {field}"
            );
        }
        // A rule serde itself rejects resolves to no rename here.
        assert_eq!(apply_serde_rename_all_rule("bogusCase", "word_count"), None);
    }
}
