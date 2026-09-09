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
/// Does this attribute list carry `#[serde(default)]`, bare or `= "path"`?
///
/// Shared by the derive's emitter (which marks a defaulted field not-`required`)
/// and its audit (which must NOT refuse `skip_serializing_if` on a field serde
/// can fill in). Those two have to agree on what "defaulted" means, so they ask
/// the same function rather than each spelling the check out (issue #802).
pub fn has_serde_default(attrs: &[syn::Attribute]) -> bool {
    serde_bare_word(attrs, &["default"]).is_some()
        || serde_valued_key(attrs, &["default"]).is_some()
}

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
/// Does this variant carry a `#[serde(alias = "…")]`?
///
/// `alias` is deserialize-only: it adds an accepted input spelling without
/// changing what serialization writes. That asymmetry has no correct rendering
/// in a single schema serving both directions, so the derive refuses it
/// (issue #802).
///
/// Routed through [`consume_unrecognized_meta`] like every other scanner here,
/// so a sibling list-valued attribute — `#[serde(bound(deserialize = "…"),
/// alias = "legacy")]` — cannot abort the walk before `alias` is reached.
pub fn variant_has_serde_alias(variant: &syn::Variant) -> bool {
    has_serde_alias(&variant.attrs)
}

/// Does this attribute list carry `#[serde(alias = "…")]`?
///
/// serde accepts `alias` on a FIELD and on a VARIANT, and it means the same
/// deserialize-only widening in both places. One predicate serves both, so the
/// two callers cannot drift the way the audit and the emitter drifted over
/// `default` (issue #802).
pub fn has_serde_alias(attrs: &[syn::Attribute]) -> bool {
    let mut found = false;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("alias") {
                // Consume the value so the walk continues cleanly.
                if let Ok(value) = meta.value() {
                    let _ = value.parse::<syn::Lit>();
                }
                found = true;
            } else {
                consume_unrecognized_meta(&meta)?;
            }
            Ok(())
        });
    }
    found
}

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

/// Emit the schema for a type whose last path segment is `Option`.
///
/// Split out of [`emit_json_schema_tokens`] only for length: the nullable case
/// carries two runtime guards (the wrapper's own identity, and — for
/// `Option<Value>` — the inner type's) and their reasoning does not compress.
fn emit_option_schema_tokens(ty: &syn::Type, inner: &syn::Type) -> TokenStream {
    // Everything below describes `ty` as NULLABLE, which is only true if
    // `ty` is `std`'s `Option`. `unwrap_single_generic` matched the last
    // path segment, so an application's own `domain::Option<T>` reaches
    // here too — and it is an ordinary struct, not a nullable anything.
    // The whole nullable body is therefore emitted as the MATCHED arm of
    // the runtime identity guard: a registered `domain::Option<T>` gets its
    // real schema, an unregistered one gets the honest `$ref` (which
    // `--strict` then reports as opaque), and only genuine `Option` is
    // described as nullable.

    // `Option<serde_json::Value>` must NOT be wrapped. The unconstrained
    // schema already admits null, and `oneOf` demands that EXACTLY ONE
    // branch match — so `oneOf [{unconstrained}, {"type":"null"}]` would
    // reject the very null it is meant to permit, because null matches both.
    //
    // But `is_serde_json_value` matches the LAST PATH SEGMENT, so it also
    // fires for an application type of one's own called `Value`. That type
    // is ordinary: if it carries `#[derive(OpenApiSchema)]` its schema is a
    // normal non-null object and it NEEDS the null branch, or serializing
    // `None` emits a null the schema forbids. A proc macro cannot tell the
    // two apart, so the choice is deferred to runtime — the same escape the
    // scalar table uses for its own last-segment collisions.
    if is_serde_json_value(&type_name_str(inner)) {
        // THREE outcomes, not two. The inventory check alone cannot tell the
        // genuine `serde_json::Value` from an application `Value` that
        // derives nothing — it answers `None` for both — so the identity is
        // checked as well, exactly as `emit_identity_guarded` does for the
        // non-optional path. Without it an underived colliding `Value` was
        // published as arbitrary JSON: `--strict` passed while a client
        // still received `unknown` for a field with a fixed wire shape.
        let inner_ty = inner;
        let matched = quote! {{
            match ::autumn_web::openapi::registered_derived_schema(
                ::core::any::type_name::<#inner_ty>()
            ) {
                // A colliding application `Value` with a real schema: wrap
                // it like any other optional type.
                ::core::option::Option::Some(__derived) => {
                    ::autumn_web::reexports::serde_json::json!({
                        "oneOf": [__derived, { "type": "null" }]
                    })
                }
                ::core::option::Option::None => {
                    let __identity = ::core::any::type_name::<#inner_ty>();
                    if __identity == "serde_json::value::Value" {
                        // Genuine `serde_json::Value`: unconstrained already
                        // admits null, and wrapping it in `oneOf` would
                        // REJECT that null (it matches both branches).
                        ::autumn_web::reexports::serde_json::json!({
                            "description": "Arbitrary JSON: an object, array, string, \
                                            number, boolean or null.",
                        })
                    } else {
                        // An underived application `Value`: an ordinary
                        // type, so it gets the ordinary nullable `$ref`.
                        let __ref_path = ::std::format!(
                            "#/components/schemas/{}",
                            __identity
                        );
                        ::autumn_web::reexports::serde_json::json!({
                            "oneOf": [{ "$ref": __ref_path }, { "type": "null" }]
                        })
                    }
                }
            }
        }};
        return emit_identity_guarded(
            ty,
            &std_wrapper_predicate(&OPTION_IDENTITY_PREFIXES),
            &matched,
        );
    }
    let inner_tokens = emit_json_schema_tokens(inner);
    let matched = quote! {{
        let __inner = #inner_tokens;
        ::autumn_web::reexports::serde_json::json!({ "oneOf": [__inner, { "type": "null" }] })
    }};
    emit_identity_guarded(
        ty,
        &std_wrapper_predicate(&OPTION_IDENTITY_PREFIXES),
        &matched,
    )
}

/// Emit a `TokenStream` that evaluates (at runtime) to a `serde_json::Value`
/// representing the JSON Schema for the given Rust type.
///
/// Handles `Option<T>` (nullable), `Vec<T>` (array), primitives (`String`,
/// `i64`, etc.), and everything else as a `$ref` to a component schema.
pub fn emit_json_schema_tokens(ty: &syn::Type) -> TokenStream {
    // Option<T> → OpenAPI 3.1 nullable: oneOf [{T-schema}, {type:null}]
    if let Some(inner) = crate::api_doc::unwrap_single_generic(ty, "Option") {
        return emit_option_schema_tokens(ty, &inner);
    }

    // Vec<T> → {"type": "array", "items": <T-schema>}
    //
    // Guarded exactly like `Option` above: `domain::Vec<T>` is matched by the
    // last path segment but is not an array, so the array body is the MATCHED
    // arm and anything else falls through to its registered schema or an
    // honest `$ref`.
    if let Some(inner) = crate::api_doc::unwrap_single_generic(ty, "Vec") {
        let inner_tokens = emit_json_schema_tokens(&inner);
        let matched = quote! {{
            let __items = #inner_tokens;
            ::autumn_web::reexports::serde_json::json!({ "type": "array", "items": __items })
        }};
        return emit_identity_guarded(ty, &std_wrapper_predicate(&VEC_IDENTITY_PREFIXES), &matched);
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
    if is_serde_json_value(&name) {
        return unconstrained_json_tokens(ty);
    }

    if let Some((json_type, format, description)) = scalar_json_schema(&name) {
        let format_insert = format.map(|f| {
            quote! { __scalar.insert("format".to_owned(), #f.into()); }
        });
        let description_insert = description.map(|d| {
            quote! { __scalar.insert("description".to_owned(), #d.into()); }
        });
        // Matching is on the type's LAST PATH SEGMENT, because a proc macro sees
        // only the tokens as written and `use chrono::NaiveDateTime;` is the
        // normal spelling. Two runtime checks then establish that the type
        // really IS the external scalar before it is described as one:
        //
        //   1. The derived-schema inventory. A colliding application type
        //      carrying `#[derive(OpenApiSchema)]` resolves to its own schema.
        //   2. Its FULL runtime path. An application `Uuid` or `DateTime` that
        //      derives NOTHING used to fall through to the scalar and be
        //      advertised as a uuid/date-time string even though it serializes
        //      as an object — check (1) alone could not see it, because there
        //      was nothing registered to find. `type_name` gives the
        //      fully-qualified path, so only the genuine `chrono::`/`uuid::`
        //      types take this branch; anything else falls through to the same
        //      `$ref` the fallback below emits, where it is either resolved or
        //      honestly reported as opaque.
        //
        // (The same last-segment limitation still governs `primitive_json_type`
        // for `String`, `bool` and the numerics, where the stakes are lower: a
        // colliding `String` would have to be a non-string-serializing type of
        // that exact name.)
        // The predicate is derived from the REAL type through `autumn_web`'s
        // re-export (see `scalar_identity_predicate`), not from a hand-written
        // prefix: `starts_with("chrono::")` accepted every type in a crate of
        // that name, and a downstream crate named `chrono` with its own
        // `DateTime` was inlined as a string even when serde writes an object.
        let identity_predicate = scalar_identity_predicate(&name)
            .expect("scalar_json_schema and scalar_identity_predicate cover the same names");
        return emit_identity_guarded(
            ty,
            &identity_predicate,
            &quote! {{
                let mut __scalar = ::autumn_web::reexports::serde_json::Map::new();
                __scalar.insert("type".to_owned(), #json_type.into());
                #format_insert
                #description_insert
                ::autumn_web::reexports::serde_json::Value::Object(__scalar)
            }},
        );
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
            // Guarded like every other last-segment table. `String` is the
            // realistic collision — it is a std type, not a language primitive,
            // so `struct String { .. }` in an application is ordinary code and
            // used to be advertised as a JSON string however it serialized. The
            // language primitives are listed too so the rule has no exceptions
            // to remember; their `type_name` is just the bare name.
            let expected: &[&str] = match json_type {
                "string" => &["alloc::string::String", "str", "&str"],
                "boolean" => &["bool"],
                "number" => &["f32", "f64"],
                _ => &[
                    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "isize", "usize",
                ],
            };
            emit_identity_guarded(
                ty,
                &quote! { [#(#expected),*].contains(&__identity) },
                &quote! {
                    ::autumn_web::reexports::serde_json::json!({ "type": #json_type })
                },
            )
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
/// Emit a runtime-guarded mapping for a type matched by its LAST PATH SEGMENT.
///
/// Every table in this module matches on the last segment, because a proc macro
/// sees only the tokens as written and `use serde_json::Value;` is the ordinary
/// spelling. A name match is therefore a HYPOTHESIS, not a fact: an application
/// type of the same name is indistinguishable at expansion time.
///
/// So the emitted code checks two things at runtime before committing to the
/// mapping, and falls back to the full-identity `$ref` otherwise:
///
///   1. The derived-schema inventory — a colliding type carrying
///      `#[derive(OpenApiSchema)]` resolves to its own real schema.
///   2. `type_name`, against the genuine type's fully-qualified path — which
///      catches the colliding type that derives NOTHING, where check (1) has
///      nothing to find and silently says "not a collision".
///
/// Both checks are needed: (1) alone let an underived `domain::Uuid` be
/// advertised as a uuid string, and (2) alone would ignore an application type
/// that had correctly registered itself. Written once here so a new table
/// cannot be added with only half the guard (issue #802).
fn emit_identity_guarded(
    ty: &syn::Type,
    identity_predicate: &TokenStream,
    matched: &TokenStream,
) -> TokenStream {
    quote! {{
        match ::autumn_web::openapi::registered_derived_schema(
            ::core::any::type_name::<#ty>()
        ) {
            ::core::option::Option::Some(__derived) => __derived,
            ::core::option::Option::None => {
                let __identity = ::core::any::type_name::<#ty>();
                if #identity_predicate {
                    #matched
                } else {
                    // Same full-identity `$ref` the general fallback emits, so
                    // the finalize collision index can rewrite it.
                    let __ref_path = ::std::format!(
                        "#/components/schemas/{}",
                        __identity
                    );
                    ::autumn_web::reexports::serde_json::json!({ "$ref": __ref_path })
                }
            }
        }
    }}
}

/// The `type_name` prefixes that identify `std`'s own `Option` / `Vec`.
///
/// Both spellings are listed for each because `core::option::Option` /
/// `alloc::vec::Vec` are what today's rustc renders, while `std::option::` /
/// `std::vec::` are the re-export paths a future rustc could plausibly print.
/// Accepting both costs one string comparison and removes a silent-wrong-spec
/// failure mode from a toolchain upgrade.
const OPTION_IDENTITY_PREFIXES: [&str; 2] = ["core::option::Option<", "std::option::Option<"];
const VEC_IDENTITY_PREFIXES: [&str; 2] = ["alloc::vec::Vec<", "std::vec::Vec<"];

/// Emit an expression that is `true` at runtime iff `__identity` (the enclosing
/// scope's `type_name::<T>()`) names one of `prefixes`.
///
/// `Option` and `Vec` are matched by the macro on their LAST PATH SEGMENT, for
/// the same reason every other type here is: a proc macro sees only the tokens
/// as written. But an application's own `domain::Option<T>` — an ordinary
/// struct that happens to spell that segment — is not nullable, and a
/// `domain::Vec<T>` is not an array. Describing them as such advertised a wire
/// shape those types do not serialize, and (because no opaque component was
/// emitted) `autumn openapi export --strict` passed while doing it. Deferring
/// the decision to `type_name` is the same escape the scalar table uses for its
/// own last-segment collisions.
fn std_wrapper_predicate(prefixes: &[&str]) -> TokenStream {
    let checks = prefixes
        .iter()
        .map(|p| quote! { __identity.starts_with(#p) });
    quote! { #(#checks)||* }
}

/// Emit an expression that is `true` at runtime iff `ty` really is `std`'s
/// wrapper named by `prefixes`, for use OUTSIDE [`emit_identity_guarded`]
/// (which binds `__identity` itself).
fn emit_is_std_wrapper(ty: &syn::Type, prefixes: &[&str]) -> TokenStream {
    let predicate = std_wrapper_predicate(prefixes);
    quote! {{
        let __identity = ::core::any::type_name::<#ty>();
        #predicate
    }}
}

/// Is this type spelled `serde_json::Value` (or a `Value` alias of it)?
///
/// Matched on the LAST PATH SEGMENT for the same reason the scalar table is: a
/// proc macro sees only the tokens as written, and `use serde_json::Value;` is
/// the normal spelling. A colliding application type is handled the same way
/// too — the runtime check consults the derived-schema inventory first, so a
/// `Value` of one's own carrying `#[derive(OpenApiSchema)]` wins.
fn is_serde_json_value(name: &str) -> bool {
    name == "Value"
}

/// The schema for arbitrary JSON: no constraint at all.
///
/// A `json` / `jsonb` column may legitimately hold an object, an array, a
/// string, a number, a boolean or null, so any `"type"` here would be a lie for
/// some rows. Emitting only a description leaves the schema unconstrained,
/// which is the honest answer and is true of BOTH directions (issue #802).
fn unconstrained_json_tokens(ty: &syn::Type) -> TokenStream {
    emit_identity_guarded(
        ty,
        &quote! { __identity == "serde_json::value::Value" },
        &quote! {
            ::autumn_web::reexports::serde_json::json!({
                "description": "Arbitrary JSON: an object, array, string, number, boolean or null.",
            })
        },
    )
}

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

/// A runtime predicate that is `true` only for the GENUINE external scalar this
/// table entry describes.
///
/// The identity is compared against `type_name` of the real type, reached
/// through `autumn_web`'s own re-export — never against a hand-written string.
/// Two things follow. It cannot drift from the dependency: if `chrono` moves
/// `NaiveDateTime` between internal modules, both sides move together. And it
/// is exact rather than namespace-wide: the previous `starts_with("chrono::")`
/// accepted ANY type in a crate that happens to be named `chrono` — including a
/// downstream crate of that name defining its own `DateTime` — and inlined it as
/// a string even when serde writes an object, with no opaque component for
/// `--strict` to catch.
///
/// `DateTime<Tz>` is compared on the part before `<`, because the zone is a
/// parameter: `DateTime<Utc>`, `DateTime<Local>` and `DateTime<Tz>` are all
/// genuinely chrono's. Everything else is compared whole.
fn scalar_identity_predicate(name: &str) -> Option<TokenStream> {
    let chrono = quote! { ::autumn_web::reexports::chrono };
    let real: TokenStream = match name {
        "DateTime" => {
            // Generic: match the path up to the `<`, so any zone qualifies while
            // an unrelated `DateTime` still does not.
            return Some(quote! {{
                let __real = ::core::any::type_name::<#chrono::DateTime<#chrono::Utc>>();
                let __head = |__s: &'static str| __s.split('<').next().unwrap_or(__s);
                __head(__identity) == __head(__real)
            }});
        }
        "NaiveDate" => quote! { #chrono::NaiveDate },
        "NaiveDateTime" => quote! { #chrono::NaiveDateTime },
        "NaiveTime" => quote! { #chrono::NaiveTime },
        "Uuid" => quote! { ::autumn_web::reexports::uuid::Uuid },
        _ => return None,
    };
    Some(quote! { __identity == ::core::any::type_name::<#real>() })
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
    patch_nullable: bool,
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
    // `patch_nullable` applies to `fields` ONLY, never to `extra_required`.
    // On an `UpdateModel` every entry of `fields` is declared `Patch<T>` while
    // the `extra_required` lock-version column stays a plain `T` (issue #802).
    let insertions: Vec<TokenStream> = fields
        .iter()
        .map(|f| (f, patch_nullable))
        .chain(extra_required.iter().map(|f| (f, false)))
        .map(|(f, nullable)| {
            let field_name =
                resolve_name(f).unwrap_or_else(|| f.ident.as_ref().unwrap().to_string());
            let base = emit_json_schema_tokens_for_field(f);
            // `Option<T>` already emits `oneOf [T, null]`, so wrapping it again
            // would only nest a second, redundant null branch.
            //
            // `is_option_type` matches the LAST PATH SEGMENT, though, so an
            // application's own `domain::Option<T>` answers `true` here while
            // `emit_json_schema_tokens` (identity-guarded above) correctly
            // emits a NON-nullable schema for it. Skipping the wrap on that
            // answer alone would leave a `Patch` field whose `null` — the wire
            // form of `Clear` — is forbidden by its own schema. So when the
            // compile-time answer is `true` the choice is deferred to the same
            // runtime identity the emitter uses, and the two agree by
            // construction.
            let schema_expr = if !nullable {
                base
            } else if is_option_type(&f.ty) {
                let is_std_option = emit_is_std_wrapper(&f.ty, &OPTION_IDENTITY_PREFIXES);
                quote! {{
                    let __inner = #base;
                    if #is_std_option {
                        __inner
                    } else {
                        ::autumn_web::reexports::serde_json::json!({
                            "oneOf": [__inner, { "type": "null" }]
                        })
                    }
                }}
            } else {
                quote! {{
                    let __inner = #base;
                    ::autumn_web::reexports::serde_json::json!({
                        "oneOf": [__inner, { "type": "null" }]
                    })
                }}
            };
            quote! {
                __props.insert(#field_name.to_owned(), #schema_expr);
            }
        })
        .collect();

    // A field is required unless it is genuinely optional.
    //
    // `treat_as_optional` reads serde attributes, which a proc macro sees
    // exactly and completely — that answer is final. `is_option_type` does not:
    // it matches the LAST PATH SEGMENT, so an application's own
    // `domain::Option<T>` — an ordinary struct the client must actually send —
    // answered `true` and was silently dropped from `required`, while its
    // property schema (identity-guarded in `emit_json_schema_tokens`) correctly
    // described a non-nullable object. Requiredness is therefore emitted as a
    // runtime-conditional push for exactly the fields that answer `true`,
    // keyed on the same `type_name` identity the emitter uses, so a property's
    // shape and its requiredness cannot disagree.
    //
    // The pushes are emitted in the original order — model fields first, then
    // `extra_required` — so `required` keeps the ordering it has always had.
    let mut required_pushes: Vec<TokenStream> = Vec::new();
    if !all_optional {
        for f in fields {
            // `skip_serializing_if` means a RESPONSE may omit the field, so
            // `required` is wrong for it whatever its type. The standalone
            // derive's audit already refuses this attribute on anything that is
            // neither `Option`-named nor defaulted — precisely because the
            // survivors are describable as optional — but it makes that
            // judgement from the last path segment, so an application's own
            // `domain::Option<T>` passed the audit and then landed in `required`
            // anyway once the emitter's identity guard recognised the impostor.
            // Deciding it here, on the attribute rather than on the type, makes
            // the audit's stated premise actually true and needs no runtime
            // identity: the attribute is exactly what a proc macro can see.
            if treat_as_optional(f) || field_has_skip_serializing_if(f) {
                continue;
            }
            let Some(name) = resolve_name(f) else {
                continue;
            };
            let push = quote! {
                __required.push(::autumn_web::reexports::serde_json::json!(#name));
            };
            required_pushes.push(if is_option_type(&f.ty) {
                let is_std_option = emit_is_std_wrapper(&f.ty, &OPTION_IDENTITY_PREFIXES);
                quote! { if !(#is_std_option) { #push } }
            } else {
                push
            });
        }
    }
    for f in extra_required {
        if let Some(name) = resolve_name(f) {
            required_pushes.push(quote! {
                __required.push(::autumn_web::reexports::serde_json::json!(#name));
            });
        }
    }

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
        #[allow(unused_mut)]
        let mut __required: ::std::vec::Vec<::autumn_web::reexports::serde_json::Value> =
            ::std::vec::Vec::new();
        #(#required_pushes)*
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
