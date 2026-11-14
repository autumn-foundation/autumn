//! `autumn generate admin` — admin resource adapter for `autumn-admin-plugin`.
//!
//! For an existing `#[model]` (produced by `autumn generate model` or
//! `autumn generate scaffold`), emits:
//!
//! - `src/admin/<snake>.rs` — a full `AdminModel` implementation with list,
//!   detail, create, edit, delete, search, pagination, and bulk-delete.
//! - `tests/<snake>_admin.rs` — a request-level smoke test proving anonymous
//!   access is rejected and an admin user can load the list page.
//!
//! The generator derives safe field metadata from the supplied field tokens
//! and lets the user refine it through `--hidden`, `--readonly`, `--password`,
//! `--select`, and `--exclude` flags.

use std::path::Path;

use super::dsl::{Field, FieldKind, parse_fields};
use super::emit::Plan;
use super::naming::{humanize_label, pascal, pluralize, snake};
use super::schema_edit::add_mod_declaration;
use super::{GenerateError, ensure_project_root, read_or_empty};

/// A parsed `--select FIELD=val1,val2,...` spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectSpec {
    /// Field name.
    pub field: String,
    /// Ordered list of option values (labels are humanized from values).
    pub values: Vec<String>,
}

impl std::str::FromStr for SelectSpec {
    type Err = GenerateError;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        let (field, values) = match token.split_once('=') {
            Some((f, v)) => (
                f.trim().to_owned(),
                v.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>(),
            ),
            None => (token.trim().to_owned(), vec![]),
        };
        if field.is_empty() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "field name is empty in --select spec".into(),
            });
        }
        Ok(Self { field, values })
    }
}

/// Parse `--select` tokens of the form `field=val1,val2,...` or `field`.
///
/// The bare `field` form (no `=`) emits a `Select(vec![])` placeholder so the
/// user can fill in options without editing generated internals.
///
/// # Errors
/// Returns [`GenerateError::InvalidField`] if the token is blank.
pub fn parse_select_specs(tokens: &[String]) -> Result<Vec<SelectSpec>, GenerateError> {
    tokens.iter().map(|t| t.parse()).collect()
}

/// Options specific to `autumn generate admin`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdminOptions {
    /// Field names to render as `AdminFieldKind::Hidden`.
    pub hidden: Vec<String>,
    /// Field names to mark as read-only (`.readonly()`).
    pub readonly: Vec<String>,
    /// Field names to render as `AdminFieldKind::Password`.
    pub password: Vec<String>,
    /// Fields to render as `AdminFieldKind::Select` with fixed option values.
    /// Each entry is a [`SelectSpec`] parsed from a `field=val1,val2,...` token.
    pub select: Vec<SelectSpec>,
    /// Field names to omit from the generated `fields()` list entirely.
    pub exclude: Vec<String>,
    /// Field names that are `#[encrypted]` at rest (#805): emit `.encrypted()` so
    /// the admin redacts and disables them. Auto-detected from the model source.
    pub encrypted: Vec<String>,
    /// Field names that are `#[encrypted(admin_visible)]`: emit
    /// `.encrypted_visible()` so the admin shows plaintext in read views but still
    /// never pre-fills the edit form. Auto-detected from the model source.
    pub encrypted_visible: Vec<String>,
    /// The subset of the encrypted columns above declared
    /// `#[encrypted(deterministic)]`. Auto-detected from the model source.
    ///
    /// Needed only by the `lock_version` update path, which writes a raw
    /// `col.eq(value)` tuple rather than the `Update{Model}` changeset — so it
    /// has to name the encrypting diesel wrapper itself, and the wrapper differs
    /// per mode. Every other path routes through `serialize_as` and never needs
    /// to know (issue #1340).
    pub encrypted_deterministic: Vec<String>,
    /// The plan is for `autumn destroy admin`, not `generate`. The
    /// model-AST `#[translatable]` refusal (#2291) guards what a generated
    /// admin would do at runtime, so it must not stop the removal of an admin
    /// generated before the field became `#[translatable]`.
    pub for_destroy: bool,
}

/// The generated statement that back-fills absent encrypted keys before the
/// submitted form map is deserialized into `New{Model}` (issue #1340), or an
/// empty string when the model has no encrypted column.
///
/// The admin edit form renders an encrypted column as a disabled control with
/// no `name` attribute — it is managed outside the admin — so the submitted map
/// simply has no key for it. `New{Model}` nevertheless requires one (the
/// encrypted field is a non-null `String`), so `serde_json::from_value` failed
/// with a "missing field" error and EVERY edit 422'd, even one touching only a
/// plaintext column.
///
/// The placeholder exists purely to satisfy that deserialization. It can never
/// reach the database: [`is_update_writable`] excludes encrypted columns, so no
/// update path ever reads the field back off `new_row`.
fn encrypted_placeholder_stmt(options: &AdminOptions) -> String {
    use std::fmt::Write as _;
    let mut encrypted: Vec<&str> = options
        .encrypted
        .iter()
        .chain(options.encrypted_visible.iter())
        .map(String::as_str)
        .collect();
    encrypted.sort_unstable();
    encrypted.dedup();
    if encrypted.is_empty() {
        return String::new();
    }
    let mut keys = String::new();
    for (i, col) in encrypted.iter().enumerate() {
        if i > 0 {
            keys.push_str(", ");
        }
        let _ = write!(keys, "\"{col}\"");
    }
    format!(
        "            // The edit form renders at-rest encrypted columns as a disabled\n\
         \t\t\t// control with NO `name` (they are managed outside the admin), so the\n\
         \t\t\t// submitted map has no key for them — but `New{{Model}}` requires one.\n\
         \t\t\t// These placeholders only satisfy deserialization: encrypted columns are\n\
         \t\t\t// excluded from the update below, so the value never reaches the database.\n\
         \t\t\tlet mut data = data;\n\
         \t\t\tif let Some(obj) = data.as_object_mut() {{\n\
         \t\t\t\tfor col in [{keys}] {{\n\
         \t\t\t\t\tobj.entry(col.to_string())\n\
         \t\t\t\t\t\t.or_insert_with(|| serde_json::Value::String(String::new()));\n\
         \t\t\t\t}}\n\
         \t\t\t}}\n"
    )
}

/// Whether the backing model carries any at-rest encrypted column./// Whether the backing model carries any at-rest encrypted column.
///
/// `AdminOptions::encrypted`/`encrypted_visible` are populated exclusively by
/// [`detect_encrypted_fields`] reading the model's own AST — there is no
/// `--encrypted` CLI flag — so this is exactly "does the `#[model]` struct have
/// a `#[diesel(serialize_as = …)]` field", which is what decides whether diesel
/// implements `Insertable`/`AsChangeset` for the borrowed or only the owned
/// form (issue #1340).
const fn model_has_encrypted_columns(options: &AdminOptions) -> bool {
    !options.encrypted.is_empty() || !options.encrypted_visible.is_empty()
}

/// Compute the file actions for `autumn generate admin` with default options.
///
/// # Errors
/// Returns [`GenerateError`] if the project layout is invalid, the name is
/// malformed, a field token cannot be parsed, or the target model file does
/// not exist.
#[cfg(test)]
pub fn plan_admin(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
) -> Result<Plan, GenerateError> {
    plan_admin_with_options(project_root, name, field_tokens, &AdminOptions::default())
}

/// Compute the file actions for `autumn generate admin`, with full options.
///
/// # Errors
/// Same as `plan_admin`, plus options-level errors (unknown field names).
pub fn plan_admin_with_options(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    options: &AdminOptions,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    let snake_name = snake(name);
    let pascal_name = pascal(name);

    let model_path = project_root
        .join("src")
        .join("models")
        .join(format!("{snake_name}.rs"));
    if !model_path.exists() {
        return Err(GenerateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "model file not found: {}; run `autumn generate model {pascal_name} …` first, \
                 or `autumn generate scaffold {pascal_name} …` to include a repository",
                model_path.display()
            ),
        )));
    }

    let model_source = std::fs::read_to_string(&model_path).map_err(GenerateError::Io)?;

    if model_has_uuid_id(&model_source, &pascal_name) {
        return Err(GenerateError::Config(format!(
            "UUID primary keys are not yet supported for `generate admin`: the admin \
             adapter and admin plugin trait are limited to i64 primary keys, so the \
             generated admin for `{pascal_name}` would not compile. Regenerate the \
             model without `--id uuid` (default BIGSERIAL key) to use the admin generator."
        )));
    }

    let lock_version_field = detect_lock_version_field(&model_source, &pascal_name);

    // Merge the encrypted columns detected on the model, so the admin redacts exactly those (#805).
    let (det_encrypted, det_visible, det_deterministic) =
        detect_encrypted_fields(&model_source, &pascal_name);
    let mut options = options.clone();
    options.encrypted.extend(det_encrypted);
    options.encrypted_visible.extend(det_visible);
    options.encrypted_deterministic.extend(det_deterministic);

    let fields = parse_fields(field_tokens)?;
    // The model decides whether a column is per-locale: `#[translatable]` makes `Translated` refuse
    // the bare string the admin's text control submits (#2291). Skipped on destroy, so an admin
    // generated before the field became `#[translatable]` can still be reverted.
    let model_translatable = if options.for_destroy {
        Vec::new()
    } else {
        detect_translatable_fields(&model_source, &pascal_name)
    };
    reject_translatable_fields(&fields, &model_translatable)?;
    // The model decides whether a column is encrypted: `#[encrypted]` adds the `serialize_as`
    // wrapper (#1340). A `{encrypted}` token without it would only redact the admin UI while the
    // create form writes plaintext, a false promise of encryption at rest.
    for field in &fields {
        if field.is_encrypted()
            && !options.encrypted.contains(&field.name)
            && !options.encrypted_visible.contains(&field.name)
        {
            return Err(GenerateError::InvalidField {
                token: field.name.clone(),
                reason: format!(
                    "field '{}' is declared `{{encrypted}}` here, but `{}` in \
                     src/models/{snake_name}.rs has no `#[encrypted]` attribute on it. The \
                     model is what encrypts: without the attribute the column stores \
                     plaintext, and redacting it in the admin would only hide that. Add \
                     `#[encrypted]` to the model field (or regenerate it with \
                     `autumn generate model {pascal_name} '{}:String{{encrypted}}' --force`), \
                     then re-run this command.",
                    field.name, pascal_name, field.name
                ),
            });
        }
    }
    let options = &options;
    let plural = pluralize(&snake_name);
    let plural_pascal = pascal(&plural);

    let mut plan = Plan::new(project_root);

    let admin_dir = project_root.join("src").join("admin");
    plan.create(
        admin_dir.join(format!("{snake_name}.rs")),
        render_admin_file(
            &pascal_name,
            &snake_name,
            &plural,
            &plural_pascal,
            &fields,
            options,
            lock_version_field.as_deref(),
        ),
    );

    let admin_mod_path = admin_dir.join("mod.rs");
    plan.modify(
        admin_mod_path.clone(),
        add_mod_declaration(&read_or_empty(&admin_mod_path), &snake_name),
    );
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: admin_mod_path,
        name: snake_name.clone(),
    });

    plan.create(
        project_root
            .join("tests")
            .join(format!("{snake_name}_admin.rs")),
        render_admin_smoke_test(
            &pascal_name,
            &snake_name,
            &plural,
            &fields,
            options,
            lock_version_field.as_deref(),
        ),
    );

    Ok(plan)
}

/// Fallback destroy-only plan for `autumn destroy admin <name>` when the
/// backing model file no longer exists — e.g. `autumn destroy model <name>`
/// already ran first (issue #1048 PR review). `plan_admin_with_options`
/// needs the model source to detect fields/lock-version/encrypted columns
/// for rendering, which is meaningless (and impossible) once the model is
/// gone, so it can't be reused here.
///
/// The two generated files' expected content is unknowable without the
/// model, so they're recorded with an empty placeholder — real content
/// (never empty) always counts as diverged, so they're only removed with
/// `--force`, exactly like any other file destroy can't confirm is safe to
/// delete. The `src/admin/mod.rs` declaration removal is unaffected: it's a
/// pure text edit ([`crate::generate::emit::Revert::ModDecl`]) that never
/// depended on the model's content.
///
/// # Errors
/// Returns [`GenerateError`] when `project_root` isn't a valid project, or
/// `name` fails validation.
/// Refuse a `{translatable}` column (issue #1384), for the same reason
/// `generate scaffold` does.
///
/// The admin emits a plain text control bound to the whole column and hands its
/// string straight to `serde_json::from_value::<New{Model}>`. `Translated`
/// deliberately refuses a bare string — accepting one would let a single-locale
/// value replace the whole container — so every generated create and update
/// would fail validation on a required field, and a value that *did* decode
/// would wipe every other language. Editing translated content needs a
/// per-locale editor, which is translation-workflow UI and out of scope here.
///
/// The refusal keys off BOTH the DSL token and the model AST (issue #2291):
/// a submitted field whose backing model field is `#[translatable]` is
/// refused no matter how the token is spelled — the model is the source of
/// truth, mirroring the `#[encrypted]` handling from #1340.
fn reject_translatable_fields(
    fields: &[Field],
    model_translatable: &[String],
) -> Result<(), GenerateError> {
    let Some(field) = fields.iter().find(|f| f.is_translatable()) else {
        return reject_model_translatable_fields(fields, model_translatable);
    };
    Err(GenerateError::Config(format!(
        "a `{{translatable}}` field (`{}`) is not supported by `generate admin`: the generated \
         admin binds one plain text control to the whole per-locale container, and `Translated` \
         refuses a bare string — so create and update would fail validation, and a value that \
         did decode would replace every other locale. Leave the column out of the admin field \
         list, or write a per-locale editor: `record.available_locales(\"{}\")` and \
         `record.set_{}(locale, value)` are generated for you.",
        field.name, field.name, field.name
    )))
}

/// Refuse a submitted field bound to a `#[translatable]` model field (issue
/// #2291), even when the DSL token carries no `{translatable}` marker. Same
/// hazard as the token-based refusal above; the only difference is where the
/// marker was found.
fn reject_model_translatable_fields(
    fields: &[Field],
    model_translatable: &[String],
) -> Result<(), GenerateError> {
    let Some(field) = fields
        .iter()
        .find(|f| model_translatable.iter().any(|t| t == &f.name))
    else {
        return Ok(());
    };
    Err(GenerateError::Config(format!(
        "field `{}` is `#[translatable]` on the model, and `generate admin` does not support \
         translatable columns: the generated admin binds one plain text control to the whole \
         per-locale container, and `Translated` refuses a bare string — so create and update \
         would fail validation, and a value that did decode would replace every other locale. \
         Leave the column out of the admin field list, or write a per-locale editor: \
         `record.available_locales(\"{}\")` and `record.set_{}(locale, value)` are generated \
         for you.",
        field.name, field.name, field.name
    )))
}

pub fn plan_admin_destroy_fallback(project_root: &Path, name: &str) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    // No model-file check narrows `name` here (the file is gone), so reject path separators and
    // `..` before building paths (#1048).
    super::model::validate_resource_name(name)?;

    let snake_name = snake(name);
    let mut plan = Plan::new(project_root);

    let admin_dir = project_root.join("src").join("admin");
    plan.create(admin_dir.join(format!("{snake_name}.rs")), String::new());

    let admin_mod_path = admin_dir.join("mod.rs");
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: admin_mod_path,
        name: snake_name.clone(),
    });

    plan.create(
        project_root
            .join("tests")
            .join(format!("{snake_name}_admin.rs")),
        String::new(),
    );

    Ok(plan)
}

/// Returns true if the model struct `pascal_name` has a primary-key field `id`
/// whose type is a UUID, written either fully-qualified (`uuid::Uuid`) or bare
/// (`Uuid`). Parses the model AST (like [`detect_lock_version_field`]) so it is
/// not fooled by spelling or module-qualification differences — a substring
/// check would miss `pub id: Uuid`.
fn model_has_uuid_id(model_source: &str, pascal_name: &str) -> bool {
    let Ok(parsed) = syn::parse_file(model_source) else {
        return false;
    };
    parsed.items.into_iter().any(|item| {
        let syn::Item::Struct(item_struct) = item else {
            return false;
        };
        if item_struct.ident != pascal_name {
            return false;
        }
        let syn::Fields::Named(fields) = item_struct.fields else {
            return false;
        };
        fields.named.into_iter().any(|field| {
            let Some(ident) = field.ident else {
                return false;
            };
            if ident != "id" {
                return false;
            }
            let syn::Type::Path(type_path) = field.ty else {
                return false;
            };
            type_path
                .path
                .segments
                .last()
                .is_some_and(|seg| seg.ident == "Uuid" || seg.ident == "SqliteUuid")
        })
    })
}

fn detect_lock_version_field(model_source: &str, pascal_name: &str) -> Option<String> {
    let parsed = syn::parse_file(model_source).ok()?;
    parsed.items.into_iter().find_map(|item| {
        let syn::Item::Struct(item_struct) = item else {
            return None;
        };
        if item_struct.ident != pascal_name {
            return None;
        }
        let syn::Fields::Named(fields) = item_struct.fields else {
            return None;
        };
        fields.named.into_iter().find_map(|field| {
            let has_lock_version = field.attrs.iter().any(|attr| {
                attr.path()
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "lock_version")
            });
            has_lock_version.then(|| field.ident.map(|ident| ident.to_string()))?
        })
    })
}

/// Detect `#[encrypted(...)]` fields on the model struct, partitioning them by
/// whether they opted into `admin_visible`. Lets the generated admin mark exactly
/// the encrypted columns (#805) — a per-field flag, not a global name lookup, so
/// an unrelated same-named plaintext column on another model stays editable.
///
/// Returns `(encrypted_redacted, encrypted_visible, deterministic)` field-name
/// lists. The third is the subset of the first two declared
/// `#[encrypted(deterministic)]`, which the `lock_version` update path needs to
/// pick the right encrypting wrapper (issue #1340).
fn detect_encrypted_fields(
    model_source: &str,
    pascal_name: &str,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut redacted = Vec::new();
    let mut visible = Vec::new();
    let mut deterministic = Vec::new();
    let Ok(parsed) = syn::parse_file(model_source) else {
        return (redacted, visible, deterministic);
    };
    for item in parsed.items {
        let syn::Item::Struct(item_struct) = item else {
            continue;
        };
        if item_struct.ident != pascal_name {
            continue;
        }
        let syn::Fields::Named(fields) = item_struct.fields else {
            continue;
        };
        for field in fields.named {
            let Some(attr) = field.attrs.iter().find(|attr| {
                attr.path()
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "encrypted")
            }) else {
                continue;
            };
            let Some(name) = field.ident.as_ref().map(ToString::to_string) else {
                continue;
            };
            // `admin_visible` / `deterministic` opt-ins appear in the
            // attribute's token list, e.g. `#[encrypted(deterministic, admin_visible)]`.
            let opts = match &attr.meta {
                syn::Meta::List(list) => list.tokens.to_string(),
                // A bare `#[encrypted]` is randomized and redacted.
                _ => String::new(),
            };
            if opts.contains("deterministic") {
                deterministic.push(name.clone());
            }
            if opts.contains("admin_visible") {
                visible.push(name);
            } else {
                redacted.push(name);
            }
        }
    }
    (redacted, visible, deterministic)
}

/// Detect `#[translatable]` fields from the model AST (issue #2291).
///
/// Mirrors [`detect_encrypted_fields`]: the marker's last path segment is the
/// spelling that matters, so a bare `#[translatable]` and a qualified
/// `#[autumn_web::translatable]` both match. A field that carries the marker
/// must be refused by `generate admin` no matter how its DSL token is spelled
/// — see [`reject_translatable_fields`].
fn detect_translatable_fields(model_source: &str, pascal_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(parsed) = syn::parse_file(model_source) else {
        return out;
    };
    for item in parsed.items {
        let syn::Item::Struct(item_struct) = item else {
            continue;
        };
        if item_struct.ident != pascal_name {
            continue;
        }
        let syn::Fields::Named(fields) = item_struct.fields else {
            continue;
        };
        for field in fields.named {
            let carries_marker = field.attrs.iter().any(|attr| {
                attr.path()
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "translatable")
            });
            if carries_marker && let Some(name) = field.ident.as_ref().map(ToString::to_string) {
                out.push(name);
            }
        }
    }
    out
}

// ── Field metadata derivation ────────────────────────────────────────────────

const fn admin_field_kind(field: &Field) -> &'static str {
    match field.kind {
        // `Enum` never emits `Text`: `render_fields_vec` replaces it with a `Select` (#1030).
        // `Decimal` is text, not `Float`: the admin coerces `Float` input through `f64`, which
        // rounds. Text keeps the string exact for `rust_decimal`.
        // `Time` (#3405) is text too: the admin has no time kind, and `NaiveTime` parses
        // `HH:MM[:SS]`.
        FieldKind::String
        | FieldKind::Uuid
        | FieldKind::Enum
        | FieldKind::Decimal { .. }
        | FieldKind::Slug
        | FieldKind::Time => "AdminFieldKind::Text",
        // `RichText` (#1255) holds Markdown source, and the admin is a raw-value editor: a plain
        // textarea, not the app-facing `rich_text_area`.
        FieldKind::Text | FieldKind::RichText => "AdminFieldKind::TextArea",
        // A foreign-key id renders the same as any other integer column.
        FieldKind::I32 | FieldKind::I64 | FieldKind::References => "AdminFieldKind::Integer",
        FieldKind::Bool => "AdminFieldKind::Boolean",
        FieldKind::F32 | FieldKind::F64 => "AdminFieldKind::Float",
        FieldKind::NaiveDateTime | FieldKind::DateTime => "AdminFieldKind::DateTime",
        // Binary blobs and file metadata are not editable inline. `Position` (#1358) and
        // `Commentable` (#1367) are framework-maintained and absent from `New*`/`Update*`.
        FieldKind::Bytea | FieldKind::Attachment | FieldKind::Position | FieldKind::Commentable => {
            "AdminFieldKind::Hidden"
        }
        // `Json` (#1341): `coerce_form_value` turns the textarea back into a JSON value and
        // rejects malformed input. `Hidden` would not work: hidden values are stripped before the
        // model adapter sees them, so a required json column could never be saved and a nullable
        // one would be nulled.
        FieldKind::Json => "AdminFieldKind::Json",
    }
}

/// Whether a field is an at-rest encrypted column (#805), per the auto-detected
/// `encrypted` / `encrypted_visible` option lists.
fn admin_field_is_encrypted(options: &AdminOptions, name: &str) -> bool {
    options.encrypted.iter().any(|c| c == name)
        || options.encrypted_visible.iter().any(|c| c == name)
}

const fn is_default_searchable(field: &Field) -> bool {
    matches!(
        field.kind,
        FieldKind::String | FieldKind::Text | FieldKind::RichText
    )
}

const fn is_default_filterable(field: &Field) -> bool {
    matches!(field.kind, FieldKind::Bool)
}

const fn is_default_readonly(field: &Field) -> bool {
    matches!(field.kind, FieldKind::NaiveDateTime | FieldKind::DateTime)
}

const fn is_default_optional(field: &Field) -> bool {
    field.nullable || matches!(field.kind, FieldKind::NaiveDateTime | FieldKind::DateTime)
}

fn is_default_hide_from_list(field: &Field) -> bool {
    // Long bodies and blobs (text, rich text, binary, JSON) clutter the list; updated_at is noise.
    matches!(
        field.kind,
        FieldKind::Text | FieldKind::RichText | FieldKind::Bytea | FieldKind::Json
    ) || field.name == "updated_at"
}

fn is_lock_version_field(field: &Field, lock_version_field: Option<&str>) -> bool {
    lock_version_field.is_some_and(|name| field.name == name)
}

fn is_update_writable(
    field: &Field,
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> bool {
    !options.exclude.contains(&field.name)
        && !options.hidden.contains(&field.name)
        && !options.readonly.contains(&field.name)
        && !is_lock_version_field(field, lock_version_field)
        && !is_default_readonly(field)
        // `#[position]` (#1358) and `#[commentable]` (#1367) columns are absent from `New{Model}`
        // and `Update{Model}`, so writing them would not compile.
        && !field.kind.is_position()
        && !field.kind.is_commentable()
        && !matches!(field.kind, FieldKind::Bytea)
        // An encrypted column (#1340) renders a disabled control with no `name`, so an update would
        // write a placeholder. Create still takes a value and encrypts it through `serialize_as`.
        && !admin_field_is_encrypted(options, &field.name)
}

// ── Template rendering ───────────────────────────────────────────────────────

#[allow(
    clippy::too_many_lines,
    reason = "Single template function — splitting produces less readable output, not more."
)]
fn render_admin_file(
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    plural_pascal: &str,
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> String {
    // `serialize_as` on an `#[encrypted]` column consumes the value, so diesel implements
    // `Insertable` for the owned `New{Model}` only (#1340). Plain models keep `&` so their output
    // is unchanged.
    let insert_record = if model_has_encrypted_columns(options) {
        "new_row"
    } else {
        "&new_row"
    };
    let fields_vec = render_fields_vec(fields, options, lock_version_field);
    let apply_filters = render_apply_filters(plural, fields, options, lock_version_field);
    let apply_sort = render_apply_sort(plural, fields, options, lock_version_field);
    let update_body = render_update_body(
        pascal_name,
        snake_name,
        plural,
        fields,
        options,
        lock_version_field,
    );
    let patch_import = if lock_version_field.is_some() {
        ""
    } else {
        "use autumn_web::Patch;\n"
    };
    let model_imports = if lock_version_field.is_some() {
        format!("{{{pascal_name}, New{pascal_name}}}")
    } else {
        format!("{{{pascal_name}, New{pascal_name}, Update{pascal_name}}}")
    };

    format!(
        r#"//! Generated by `autumn generate admin`.
//!
//! Implements `AdminModel` for `{pascal_name}` using `autumn-admin-plugin`.
//! Edit freely — once generated, this is ordinary user code.

use autumn_admin_plugin::prelude::*;
{patch_import}use diesel::OptionalExtension;
use diesel::prelude::*;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Pool;
use serde_json::Value;

use crate::models::{snake_name}::{model_imports};
use crate::schema::{plural};

#[derive(Clone, Copy, Default)]
pub struct {pascal_name}Admin;

impl {pascal_name}Admin {{
    fn pool_error(e: impl std::fmt::Display) -> AdminError {{
        AdminError::Database(e.to_string())
    }}
    fn validation_error(e: impl std::fmt::Display) -> AdminError {{
        AdminError::Validation(e.to_string())
    }}
    fn other_error(e: impl std::fmt::Display) -> AdminError {{
        AdminError::Other(e.to_string())
    }}
    fn serialize_{snake_name}(row: {pascal_name}) -> Result<Value, AdminError> {{
        serde_json::to_value(row).map_err(Self::other_error)
    }}

{apply_filters}

{apply_sort}
}}

impl AdminModel for {pascal_name}Admin {{
    fn slug(&self) -> &'static str {{
        "{plural}"
    }}

    fn display_name(&self) -> &'static str {{
        "{pascal_name}"
    }}

    fn display_name_plural(&self) -> &'static str {{
        "{plural_pascal}"
    }}

    fn fields(&self) -> Vec<AdminField> {{
        vec![
            AdminField::new("id", AdminFieldKind::Hidden).readonly().hide_from_list(),
{fields_vec}        ]
    }}

    fn list(
        &self,
        pool: &Pool<AsyncPgConnection>,
        params: ListParams,
    ) -> AdminFuture<'_, ListResult> {{
        let pool = pool.clone();
        Box::pin(async move {{
            let mut conn = pool.get().await.map_err(Self::pool_error)?;

            let total: i64 = Self::apply_filters({plural}::table.into_boxed(), &params)
                .count()
                .get_result(&mut conn)
                .await
                .map_err(Self::pool_error)?;

            let mut query = Self::apply_sort(
                Self::apply_filters({plural}::table.into_boxed(), &params),
                &params,
            );
            if params.per_page > 0 {{
                let offset = params
                    .page
                    .saturating_sub(1)
                    .saturating_mul(params.per_page);
                query = query.limit(params.per_page as i64).offset(offset as i64);
            }}

            let records = query
                .select({pascal_name}::as_select())
                .load::<{pascal_name}>(&mut conn)
                .await
                .map_err(Self::pool_error)?
                .into_iter()
                .map(Self::serialize_{snake_name})
                .collect::<Result<Vec<_>, _>>()?;

            Ok(ListResult {{
                records,
                total: total as u64,
                page: params.page,
                per_page: params.per_page,
            }})
        }})
    }}

    fn get(&self, pool: &Pool<AsyncPgConnection>, id: i64) -> AdminFuture<'_, Option<Value>> {{
        let pool = pool.clone();
        Box::pin(async move {{
            let mut conn = pool.get().await.map_err(Self::pool_error)?;
            let row = {plural}::table
                .find(id)
                .select({pascal_name}::as_select())
                .first::<{pascal_name}>(&mut conn)
                .await
                .optional()
                .map_err(Self::pool_error)?;
            row.map(Self::serialize_{snake_name}).transpose()
        }})
    }}

    fn create(&self, pool: &Pool<AsyncPgConnection>, data: Value) -> AdminFuture<'_, Value> {{
        let pool = pool.clone();
        Box::pin(async move {{
            let new_row: New{pascal_name} =
                serde_json::from_value(data).map_err(Self::validation_error)?;
            let mut conn = pool.get().await.map_err(Self::pool_error)?;
            let created = diesel::insert_into({plural}::table)
                .values({insert_record})
                .returning({pascal_name}::as_returning())
                .get_result::<{pascal_name}>(&mut conn)
                .await
                .map_err(Self::pool_error)?;
            Self::serialize_{snake_name}(created)
        }})
    }}

    fn update(
        &self,
        pool: &Pool<AsyncPgConnection>,
        id: i64,
        data: Value,
    ) -> AdminFuture<'_, Value> {{
        let pool = pool.clone();
        Box::pin(async move {{
{update_body}
        }})
    }}

    fn delete(&self, pool: &Pool<AsyncPgConnection>, id: i64) -> AdminFuture<'_, ()> {{
        let pool = pool.clone();
        Box::pin(async move {{
            let mut conn = pool.get().await.map_err(Self::pool_error)?;
            let deleted = diesel::delete({plural}::table.find(id))
                .execute(&mut conn)
                .await
                .map_err(Self::pool_error)?;
            if deleted == 0 {{
                return Err(AdminError::NotFound);
            }}
            Ok(())
        }})
    }}

    fn execute_action(
        &self,
        pool: &Pool<AsyncPgConnection>,
        action: &str,
        ids: Vec<i64>,
    ) -> AdminFuture<'_, u64> {{
        // `{pascal_name}Admin` never declares soft delete
        // (`supports_soft_delete()` is the trait default, `false`), so
        // `actions()` (autumn-admin-plugin's `traits.rs`) only ever offers
        // `"delete"` -- the admin UI can't reach `"restore"` or `"purge"` for
        // this model. Only `"delete"` needs the batched fast path below;
        // `"restore"`, `"purge"`, and any other action name fall through to
        // the shared `dispatch_restore_purge_or_unhandled` helper, which
        // gives a direct or out-of-band call the same "does not support soft
        // delete" (or "unhandled action") error the trait default always did.
        if action == "delete" {{
            let pool = pool.clone();
            return Box::pin(async move {{
                // One round trip for the whole selection instead of the
                // trait default's one-`DELETE`-per-id loop (an operator
                // selecting hundreds of rows in the admin list and clicking
                // "Delete selected" otherwise costs hundreds of statements
                // and pool checkouts for what is, on the wire, one
                // predicate).
                //
                // The returned count is the number of rows the `DELETE`
                // actually matched, not `ids.len()`: unlike `delete()`
                // above, a missing id here is silently a no-op rather than
                // an `AdminError::NotFound` that aborts the whole batch --
                // the same "missing selection is a no-op" contract the
                // scaffolded (`autumn generate scaffold`) bulk-delete route
                // and this crate's own `TokenAdminModel`/
                // `FeatureFlagAdminModel` overrides already use, and a
                // strictly better-defined outcome than the loop this
                // replaces, whose partial application on a missing id
                // depended on where in the id list the miss fell.
                let mut conn = pool.get().await.map_err(Self::pool_error)?;
                let deleted = diesel::delete({plural}::table.filter({plural}::id.eq_any(&ids)))
                    .execute(&mut conn)
                    .await
                    .map_err(Self::pool_error)?;
                Ok(u64::try_from(deleted).unwrap_or(u64::MAX))
            }});
        }}

        dispatch_restore_purge_or_unhandled(self, pool, action, ids)
    }}
}}
"#
    )
}

fn render_select_kind(spec: &SelectSpec) -> String {
    if spec.values.is_empty() {
        return "AdminFieldKind::Select(vec![])".into();
    }
    let opts = spec
        .values
        .iter()
        .map(|v| {
            let label = humanize_label(v);
            format!("SelectOption {{ value: \"{v}\".into(), label: \"{label}\".into() }}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("AdminFieldKind::Select(vec![{opts}])")
}

fn render_fields_vec(
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for f in fields {
        if options.exclude.contains(&f.name) || is_lock_version_field(f, lock_version_field) {
            continue;
        }
        // An `enum{...}` field is a `Select` by construction (#1030); an explicit `--select`
        // overrides the derived one.
        let effective_select: Option<SelectSpec> = options
            .select
            .iter()
            .find(|s| s.field == f.name)
            .cloned()
            .or_else(|| {
                f.is_enum().then(|| SelectSpec {
                    field: f.name.clone(),
                    values: f.variants.clone(),
                })
            });
        let kind_str: String =
            if options.hidden.contains(&f.name) || matches!(f.kind, FieldKind::Bytea) {
                "AdminFieldKind::Hidden".into()
            } else if options.password.contains(&f.name) {
                "AdminFieldKind::Password".into()
            } else if let Some(spec) = &effective_select {
                render_select_kind(spec)
            } else {
                admin_field_kind(f).into()
            };
        let _ = write!(
            out,
            "            AdminField::new(\"{name}\", {kind_str})",
            name = f.name
        );
        if options.readonly.contains(&f.name) || is_default_readonly(f) {
            let _ = write!(out, "\n                .readonly()");
        }
        // Select and hidden fields are not text-searchable, nor is an encrypted column: ILIKE over
        // ciphertext never matches plaintext (#805).
        let is_select_or_hidden = effective_select.is_some() || options.hidden.contains(&f.name);
        if is_default_searchable(f)
            && !is_select_or_hidden
            && !admin_field_is_encrypted(options, &f.name)
        {
            let _ = write!(out, "\n                .searchable()");
        }
        if is_default_filterable(f) && !is_select_or_hidden {
            let _ = write!(out, "\n                .filterable()");
        }
        if is_default_optional(f) {
            let _ = write!(out, "\n                .optional()");
        }
        if is_default_hide_from_list(f) {
            let _ = write!(out, "\n                .hide_from_list()");
        }
        // Encrypted columns (#805) are redacted and disabled, or shown in read views with
        // `admin_visible`.
        if options.encrypted_visible.contains(&f.name) {
            let _ = write!(out, "\n                .encrypted_visible()");
        } else if options.encrypted.contains(&f.name) {
            let _ = write!(out, "\n                .encrypted()");
        }
        out.push_str(",\n");
    }
    out
}

fn render_apply_filters(
    plural: &str,
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> String {
    let searchable: Vec<&Field> = fields
        .iter()
        .filter(|f| !options.exclude.contains(&f.name))
        .filter(|f| !is_lock_version_field(f, lock_version_field))
        .filter(|f| is_default_searchable(f))
        // Encrypted columns store ciphertext, so plaintext ILIKE never matches (#805).
        .filter(|f| !admin_field_is_encrypted(options, &f.name))
        .collect();

    let filterable_bool: Vec<&Field> = fields
        .iter()
        .filter(|f| !options.exclude.contains(&f.name))
        .filter(|f| !is_lock_version_field(f, lock_version_field))
        .filter(|f| is_default_filterable(f))
        .collect();

    let has_search = !searchable.is_empty();
    let has_filters = !filterable_bool.is_empty();

    // With no search or filters the generated body neither reassigns `query` nor reads `params`,
    // so drop `mut` and prefix `_` to keep the generated code warning-free.
    let query_param = if has_search || has_filters {
        "mut query"
    } else {
        "query"
    };
    let params_prefix = if has_search || has_filters { "" } else { "_" };

    let search_block = if has_search {
        let conditions = searchable
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == 0 {
                    format!("{plural}::{}.ilike(pattern.clone())", f.name)
                } else {
                    format!(".or({plural}::{}.ilike(pattern.clone()))", f.name)
                }
            })
            .collect::<Vec<_>>()
            .join("\n                    ");
        format!(
            "        if let Some(search) = params.search.as_deref() {{\n\
             \t\t\tlet pattern = format!(\"%{{search}}%\");\n\
             \t\t\tquery = query.filter(\n\
             \t\t\t\t{conditions}\n\
             \t\t\t);\n\
             \t\t}}\n"
        )
    } else {
        String::new()
    };

    let filter_block = if has_filters {
        use std::fmt::Write;
        let mut arms = String::new();
        for f in &filterable_bool {
            let _ = write!(
                arms,
                "                \"{name}\" => match value.as_str() {{\n\
                 \t\t\t\t\t\"true\" | \"1\" | \"yes\" => \
                     query = query.filter({plural}::{name}.eq(true)),\n\
                 \t\t\t\t\t\"false\" | \"0\" | \"no\" => \
                     query = query.filter({plural}::{name}.eq(false)),\n\
                 \t\t\t\t\t_ => {{}}\n\
                 \t\t\t\t}},\n",
                name = f.name
            );
        }
        format!(
            "        for (name, value) in &params.filters {{\n\
             \t\t\tmatch name.as_str() {{\n\
             {arms}\
             \t\t\t\t_ => {{}}\n\
             \t\t\t}}\n\
             \t\t}}\n"
        )
    } else {
        String::new()
    };

    format!(
        "    fn apply_filters<'a>(\n\
         \t\t{query_param}: {plural}::BoxedQuery<'a, diesel::pg::Pg>,\n\
         \t\t{params_prefix}params: &'a ListParams,\n\
         \t) -> {plural}::BoxedQuery<'a, diesel::pg::Pg> {{\n\
         {search_block}\
         {filter_block}\
         \t\tquery\n\
         \t}}"
    )
}

fn render_apply_sort(
    plural: &str,
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> String {
    use std::fmt::Write;
    let sortable: Vec<&Field> = fields
        .iter()
        .filter(|f| !options.exclude.contains(&f.name))
        .filter(|f| !is_lock_version_field(f, lock_version_field))
        // An encrypted column stores a ciphertext envelope, so sorting by it is meaningless. It
        // falls through to the `id` default, as in the scaffolded index.
        .filter(|f| !admin_field_is_encrypted(options, &f.name))
        .collect();

    let mut arms = String::new();
    let _ = write!(
        arms,
        "            (Some(\"id\"), SortDirection::Asc) => \
             query = query.order({plural}::id.asc()),\n\
         \t\t\t(Some(\"id\"), SortDirection::Desc) => \
             query = query.order({plural}::id.desc()),\n"
    );
    for f in &sortable {
        let _ = write!(
            arms,
            "            (Some(\"{name}\"), SortDirection::Asc) => \
                 query = query.order({plural}::{name}.asc()),\n\
             \t\t\t(Some(\"{name}\"), SortDirection::Desc) => \
                 query = query.order({plural}::{name}.desc()),\n",
            name = f.name
        );
    }
    let _ = write!(
        arms,
        "            (_, SortDirection::Asc) => query = query.order({plural}::id.asc()),\n\
         \t\t\t_ => query = query.order({plural}::id.desc()),\n"
    );

    format!(
        "    fn apply_sort<'a>(\n\
         \t\tmut query: {plural}::BoxedQuery<'a, diesel::pg::Pg>,\n\
         \t\tparams: &ListParams,\n\
         \t) -> {plural}::BoxedQuery<'a, diesel::pg::Pg> {{\n\
         \t\t\tmatch (params.sort_by.as_deref(), params.sort_dir) {{\n\
         {arms}\
         \t\t\t}}\n\
         \t\tquery\n\
         \t}}"
    )
}

fn render_update_body(
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> String {
    use std::fmt::Write;

    if let Some(lock_version_field) = lock_version_field {
        return render_lock_version_update_body(
            pascal_name,
            snake_name,
            plural,
            fields,
            options,
            lock_version_field,
        );
    }

    let writable: Vec<&Field> = fields
        .iter()
        .filter(|f| is_update_writable(f, options, lock_version_field))
        .collect();

    let mut changes_fields = String::new();
    for f in &writable {
        let _ = writeln!(
            changes_fields,
            "                {name}: Patch::Set(new_row.{name}),",
            name = f.name
        );
    }
    // As with the insert: once a column is encrypted, diesel implements `AsChangeset` for the
    // owned changeset only (#1340).
    let changeset_record = if model_has_encrypted_columns(options) {
        "diesel_changeset"
    } else {
        "&diesel_changeset"
    };

    let encrypted_placeholders = encrypted_placeholder_stmt(options);
    format!(
        "{encrypted_placeholders}\
         \t\t\tlet new_row: New{pascal_name} =\n\
         \t\t\t\tserde_json::from_value(data).map_err(Self::validation_error)?;\n\
         \t\t\tlet changes = Update{pascal_name} {{\n\
         {changes_fields}\
         \t\t\t\t..Default::default()\n\
         \t\t\t}};\n\
         \t\t\tlet diesel_changeset = changes.__to_changeset();\n\
         \t\t\tlet mut conn = pool.get().await.map_err(Self::pool_error)?;\n\
         \t\t\tlet updated = diesel::update({plural}::table.find(id))\n\
         \t\t\t\t.set({changeset_record})\n\
         \t\t\t\t.returning({pascal_name}::as_returning())\n\
         \t\t\t\t.get_result::<{pascal_name}>(&mut conn)\n\
         \t\t\t\t.await\n\
         \t\t\t\t.optional()\n\
         \t\t\t\t.map_err(Self::pool_error)?;\n\
         \t\t\tupdated\n\
         \t\t\t\t.ok_or(AdminError::NotFound)\n\
         \t\t\t\t.and_then(Self::serialize_{snake_name})"
    )
}

fn render_lock_version_update_body(
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: &str,
) -> String {
    use std::fmt::Write;
    let writable: Vec<&Field> = fields
        .iter()
        .filter(|f| is_update_writable(f, options, Some(lock_version_field)))
        .collect();

    let mut set_fields = String::new();
    for f in &writable {
        // This raw `col.eq(value)` tuple bypasses the `serialize_as` wrapper and would write
        // plaintext into an encrypted column (#1340). `is_update_writable` keeps those columns out.
        debug_assert!(
            !admin_field_is_encrypted(options, &f.name),
            "an encrypted column must never reach the raw lock-version set tuple"
        );
        let _ = writeln!(
            set_fields,
            "                    {plural}::{name}.eq(new_row.{name}),",
            name = f.name
        );
    }
    let _ = writeln!(
        set_fields,
        "                    {plural}::{lock_version_field}.eq({plural}::{lock_version_field} + 1),"
    );

    let encrypted_placeholders = encrypted_placeholder_stmt(options);
    format!(
        "{encrypted_placeholders}\
         \t\t\tlet new_row: New{pascal_name} =\n\
         \t\t\t\tserde_json::from_value(data).map_err(Self::validation_error)?;\n\
         \t\t\tlet mut conn = pool.get().await.map_err(Self::pool_error)?;\n\
         \t\t\tlet updated = diesel::update({plural}::table.find(id))\n\
         \t\t\t\t.set((\n\
         {set_fields}\
         \t\t\t\t))\n\
         \t\t\t\t.returning({pascal_name}::as_returning())\n\
         \t\t\t\t.get_result::<{pascal_name}>(&mut conn)\n\
         \t\t\t\t.await\n\
         \t\t\t\t.optional()\n\
         \t\t\t\t.map_err(Self::pool_error)?;\n\
         \t\t\tupdated\n\
         \t\t\t\t.ok_or(AdminError::NotFound)\n\
         \t\t\t\t.and_then(Self::serialize_{snake_name})"
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "Single template function — splitting produces less readable output, not more."
)]
fn render_admin_smoke_test(
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    fields: &[Field],
    options: &AdminOptions,
    lock_version_field: Option<&str>,
) -> String {
    let writable: Vec<&Field> = fields
        .iter()
        .filter(|f| is_update_writable(f, options, lock_version_field))
        .collect();

    let sample_form_body = if writable.is_empty() {
        String::new()
    } else {
        writable
            .iter()
            .map(|f| {
                // An enum's validation rejects "test", so use its first declared variant.
                let sample = f.variants.first().map_or("test", String::as_str);
                format!("{}={sample}", f.name)
            })
            .collect::<Vec<_>>()
            .join("&")
    };
    let content_length = sample_form_body.len();

    format!(
        "//! Smoke tests generated by `autumn generate admin`.\n\
         //!\n\
         //! These tests require a running Autumn server. Set the environment variables\n\
         //! below and run `cargo test` against a live instance.\n\
         //!\n\
         //!   AUTUMN_TEST_BASE_URL=http://localhost:3000\n\
         //!   AUTUMN_TEST_ADMIN_SESSION=<session_cookie_value>\n\
         //!\n\
         //! If your app enables CSRF protection, configure `autumn.toml` to skip\n\
         //! CSRF in the `test` profile, or provide a valid token in the POST body.\n\
         \n\
         use std::io::{{Read, Write}};\n\
         \n\
         fn connect(base: &str) -> (std::net::TcpStream, String) {{\n\
         \tlet host_port = base\n\
         \t\t.trim_start_matches(\"http://\")\n\
         \t\t.trim_start_matches(\"https://\");\n\
         \t(\n\
         \t\tstd::net::TcpStream::connect(host_port)\n\
         \t\t\t.unwrap_or_else(|_| panic!(\"could not connect to {{base}}\")),\n\
         \t\thost_port.to_owned(),\n\
         \t)\n\
         }}\n\
         \n\
         fn read_response(stream: &mut std::net::TcpStream) -> String {{\n\
         \tlet mut response = String::new();\n\
         \tstream.read_to_string(&mut response).expect(\"read failed\");\n\
         \tresponse\n\
         }}\n\
         \n\
         /// Anonymous GET `/admin/{plural}` must be rejected — 302, 401, or 403.\n\
         #[test]\n\
         fn {snake_name}_admin_anonymous_access_is_rejected() {{\n\
         \tlet Ok(base) = std::env::var(\"AUTUMN_TEST_BASE_URL\") else {{\n\
         \t\teprintln!(\"skipping: AUTUMN_TEST_BASE_URL not set\");\n\
         \t\treturn;\n\
         \t}};\n\
         \tlet base = base.trim_end_matches('/');\n\
         \tlet (mut stream, host_port) = connect(base);\n\
         \tstream\n\
         \t\t.write_all(\n\
         \t\t\tformat!(\n\
         \t\t\t\t\"GET /admin/{plural} HTTP/1.1\\r\\nHost: {{host_port}}\\r\\nConnection: close\\r\\n\\r\\n\"\n\
         \t\t\t)\n\
         \t\t\t.as_bytes(),\n\
         \t\t)\n\
         \t\t.expect(\"write failed\");\n\
         \tlet response = read_response(&mut stream);\n\
         \tlet status_line = response.lines().next().unwrap_or(\"\");\n\
         \tassert!(\n\
         \t\tstatus_line.contains(\" 302\")\n\
         \t\t\t|| status_line.contains(\" 401\")\n\
         \t\t\t|| status_line.contains(\" 403\"),\n\
         \t\t\"{pascal_name} admin list should reject anonymous access, got: {{status_line}}\"\n\
         \t);\n\
         }}\n\
         \n\
         /// Admin GET `/admin/{plural}` with a valid session must return 200.\n\
         #[test]\n\
         fn {snake_name}_admin_list_loads_for_admin_user() {{\n\
         \tlet Ok(base) = std::env::var(\"AUTUMN_TEST_BASE_URL\") else {{\n\
         \t\teprintln!(\"skipping: AUTUMN_TEST_BASE_URL not set\");\n\
         \t\treturn;\n\
         \t}};\n\
         \tlet Ok(session_cookie) = std::env::var(\"AUTUMN_TEST_ADMIN_SESSION\") else {{\n\
         \t\teprintln!(\"skipping: AUTUMN_TEST_ADMIN_SESSION not set\");\n\
         \t\treturn;\n\
         \t}};\n\
         \tlet base = base.trim_end_matches('/');\n\
         \tlet (mut stream, host_port) = connect(base);\n\
         \tstream\n\
         \t\t.write_all(\n\
         \t\t\tformat!(\n\
         \t\t\t\t\"GET /admin/{plural} HTTP/1.1\\r\\n\\\n\
         \t\t\t\t Host: {{host_port}}\\r\\n\\\n\
         \t\t\t\t Cookie: {{session_cookie}}\\r\\n\\\n\
         \t\t\t\t Connection: close\\r\\n\\r\\n\"\n\
         \t\t\t)\n\
         \t\t\t.as_bytes(),\n\
         \t\t)\n\
         \t\t.expect(\"write failed\");\n\
         \tlet response = read_response(&mut stream);\n\
         \tassert!(\n\
         \t\tresponse.starts_with(\"HTTP/1.1 200\") || response.starts_with(\"HTTP/1.0 200\"),\n\
         \t\t\"{pascal_name} admin list did not return 200:\\n{{response}}\"\n\
         \t);\n\
         }}\n\
         \n\
         /// Admin POST `/admin/{plural}` with a valid session creates a record and\n\
         /// redirects (303) on success.\n\
         #[test]\n\
         fn {snake_name}_admin_create_redirects_for_admin_user() {{\n\
         \tlet Ok(base) = std::env::var(\"AUTUMN_TEST_BASE_URL\") else {{\n\
         \t\teprintln!(\"skipping: AUTUMN_TEST_BASE_URL not set\");\n\
         \t\treturn;\n\
         \t}};\n\
         \tlet Ok(session_cookie) = std::env::var(\"AUTUMN_TEST_ADMIN_SESSION\") else {{\n\
         \t\teprintln!(\"skipping: AUTUMN_TEST_ADMIN_SESSION not set\");\n\
         \t\treturn;\n\
         \t}};\n\
         \tlet base = base.trim_end_matches('/');\n\
         \tlet body = \"{sample_form_body}\";\n\
         \tlet (mut stream, host_port) = connect(base);\n\
         \tstream\n\
         \t\t.write_all(\n\
         \t\t\tformat!(\n\
         \t\t\t\t\"POST /admin/{plural} HTTP/1.1\\r\\n\\\n\
         \t\t\t\t Host: {{host_port}}\\r\\n\\\n\
         \t\t\t\t Cookie: {{session_cookie}}\\r\\n\\\n\
         \t\t\t\t Content-Type: application/x-www-form-urlencoded\\r\\n\\\n\
         \t\t\t\t Content-Length: {content_length}\\r\\n\\\n\
         \t\t\t\t Connection: close\\r\\n\\r\\n\\\n\
         \t\t\t\t {{body}}\"\n\
         \t\t\t)\n\
         \t\t\t.as_bytes(),\n\
         \t\t)\n\
         \t\t.expect(\"write failed\");\n\
         \tlet response = read_response(&mut stream);\n\
         \tlet status_line = response.lines().next().unwrap_or(\"\");\n\
         \tassert!(\n\
         \t\tstatus_line.contains(\" 200\")\n\
         \t\t\t|| status_line.contains(\" 302\")\n\
         \t\t\t|| status_line.contains(\" 303\"),\n\
         \t\t\"{pascal_name} admin create did not redirect on success:\\n{{status_line}}\"\n\
         \t);\n\
         }}\n"
    )
}

#[cfg(test)]
// Test inputs such as `"email:String{encrypted:deterministic}"` are DSL tokens, not format
// strings.
#[allow(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal project root: Cargo.toml + src/models/<snake>.rs.
    fn project_with_model(snake: &str) -> TempDir {
        project_with_model_source(snake, "// stub\n")
    }

    #[test]
    fn sqlite_uuid_id_counts_as_a_uuid_id() {
        for ty in [
            "uuid::Uuid",
            "Uuid",
            "autumn_web::db::sqlite_types::SqliteUuid",
        ] {
            let src = format!("pub struct Post {{ #[id] pub id: {ty}, pub title: String }}");
            assert!(model_has_uuid_id(&src, "Post"), "{ty}");
        }
        assert!(!model_has_uuid_id(
            "pub struct Post { #[id] pub id: i64 }",
            "Post"
        ));
    }

    #[test]
    fn richtext_column_is_a_textarea_hidden_from_the_list() {
        // `richtext` (#1255) edits as a plain textarea and stays out of the list, like `Text`.
        let field = |kind: crate::generate::dsl::FieldKind| Field {
            name: "body".to_owned(),
            kind,
            nullable: false,
            variants: Vec::new(),
            unique: false,
            constraints: crate::generate::dsl::FieldConstraints::default(),
            state_machine: None,
        };
        let rich = field(FieldKind::RichText);
        let text = field(FieldKind::Text);
        assert_eq!(admin_field_kind(&rich), "AdminFieldKind::TextArea");
        assert!(
            is_default_hide_from_list(&rich),
            "a Markdown body must not clutter the admin list table"
        );
        assert_eq!(admin_field_kind(&rich), admin_field_kind(&text));
        assert_eq!(
            is_default_hide_from_list(&rich),
            is_default_hide_from_list(&text)
        );
    }

    #[test]
    fn json_column_uses_the_existing_admin_json_kind_and_is_hidden_from_the_list() {
        // The `json` kind (#1341) maps to the existing `AdminFieldKind::Json`, not `Hidden`: hidden
        // values are stripped before the model adapter sees them.
        let field = Field {
            name: "config".to_owned(),
            kind: FieldKind::Json,
            nullable: false,
            variants: Vec::new(),
            unique: false,
            constraints: crate::generate::dsl::FieldConstraints::default(),
            state_machine: None,
        };
        assert_eq!(admin_field_kind(&field), "AdminFieldKind::Json");
        assert!(
            is_default_hide_from_list(&field),
            "a structured-data blob must not clutter the admin list table"
        );
    }

    fn project_with_model_source(snake: &str, model_source: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(models_dir.join(format!("{snake}.rs")), model_source).unwrap();
        tmp
    }

    /// #1384 (Codex round 4): the admin binds one plain text control to the
    /// whole per-locale container and hands its string to
    /// `serde_json::from_value::<New{Model}>`. `Translated` refuses a bare
    /// string, so every generated create/update would fail validation — and a
    /// value that did decode would replace every other locale.
    #[test]
    fn plan_admin_rejects_a_translatable_field() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Post {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub title: String,\n\
            }\n";
        let tmp = project_with_model_source("post", model_source);
        let err = plan_admin(tmp.path(), "Post", &["title:String{translatable}".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("translatable"), "{err}");
        assert!(err.contains("title"), "{err}");
    }

    /// #2291: the refusal keys off the model AST, not the DSL token. A model
    /// whose `title` is `#[translatable]` must be refused even when the admin
    /// field token carries no `{translatable}` marker — otherwise the token
    /// is an opt-out of a guard the model demands.
    #[test]
    fn plan_admin_rejects_a_model_translatable_field_without_token_marker() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Post {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   #[translatable]\n\
            \x20   pub title: autumn_web::i18n::Translated,\n\
            }\n";
        let tmp = project_with_model_source("post", model_source);
        let err = plan_admin(tmp.path(), "Post", &["title:String".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("translatable"), "{err}");
        assert!(err.contains("title"), "{err}");
        assert!(
            err.contains("#[translatable]"),
            "the refusal must name the model attribute, got: {err}"
        );
    }

    /// #2291: the model-AST refusal holds for every spelling of the field
    /// token, since the token is never the source of truth here.
    #[test]
    fn plan_admin_rejects_a_model_translatable_field_for_every_token_spelling() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Post {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   #[translatable]\n\
            \x20   pub title: autumn_web::i18n::Translated,\n\
            }\n";
        for token in ["title:String", "title:Text", "title:String{nullable}"] {
            let tmp = project_with_model_source("post", model_source);
            let err = plan_admin(tmp.path(), "Post", &[token.into()])
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("translatable") && err.contains("title"),
                "token `{token}` on a #[translatable] model field must be refused, got: {err}"
            );
        }
    }

    /// #2291: the model-AST refusal is generation-only. `destroy admin` for an
    /// admin generated before the field became `#[translatable]` must still
    /// plan, or its files could never be removed while the model exists.
    #[test]
    fn destroy_plan_ignores_a_model_translatable_field() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Post {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   #[translatable]\n\
            \x20   pub title: autumn_web::i18n::Translated,\n\
            }\n";
        let tmp = project_with_model_source("post", model_source);
        let options = AdminOptions {
            for_destroy: true,
            ..Default::default()
        };
        assert!(
            plan_admin_with_options(tmp.path(), "Post", &["title:String".into()], &options).is_ok(),
            "destroying an admin must not be blocked by the generation-only guard"
        );
    }

    /// #2291 regression guard: a plain model with a plain token must NOT be
    /// refused by the new model-AST half of the gate.
    #[test]
    fn plan_admin_accepts_a_plain_field_on_a_plain_model() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Post {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub title: String,\n\
            }\n";
        let tmp = project_with_model_source("post", model_source);
        assert!(
            plan_admin(tmp.path(), "Post", &["title:String".into()]).is_ok(),
            "a non-translatable field on a non-translatable model must not be refused"
        );
    }

    /// Unit pins for the AST detector itself: bare and qualified marker
    /// spellings match; other structs, other attributes, and other fields do
    /// not leak in.
    #[test]
    fn detect_translatable_fields_pins_marker_spellings() {
        let source = "pub struct Post {\n\
            \x20   pub id: i64,\n\
            \x20   #[translatable]\n\
            \x20   pub title: String,\n\
            \x20   #[autumn_web::translatable]\n\
            \x20   pub summary: String,\n\
            \x20   #[encrypted]\n\
            \x20   pub secret: String,\n\
            }\n\
            pub struct Other {\n\
            \x20   #[translatable]\n\
            \x20   pub title: String,\n\
            }\n";
        assert_eq!(
            detect_translatable_fields(source, "Post"),
            vec!["title".to_owned(), "summary".to_owned()]
        );
        assert!(detect_translatable_fields(source, "Missing").is_empty());
        assert!(detect_translatable_fields("not rust at all", "Post").is_empty());
    }

    #[test]
    fn plan_admin_fails_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_admin(tmp.path(), "Post", &[]).unwrap_err();
        assert!(
            matches!(err, GenerateError::NotInProject),
            "expected NotInProject, got {err:?}"
        );
    }

    #[test]
    fn plan_admin_fails_when_model_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let err = plan_admin(tmp.path(), "Post", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("model file not found"),
            "expected 'model file not found' in error, got: {msg}"
        );
        assert!(
            msg.contains("post.rs"),
            "error should name the missing file, got: {msg}"
        );
        assert!(
            msg.contains("autumn generate model Post"),
            "error should suggest the fix, got: {msg}"
        );
    }

    #[test]
    fn plan_admin_rejects_uuid_keyed_model() {
        // Both the qualified and the bare `Uuid` spelling must be rejected.
        for id_ty in ["uuid::Uuid", "Uuid"] {
            let model_source = format!(
                "#[autumn_web::model]\n\
                 pub struct Post {{\n\
                 \x20   #[id]\n\
                 \x20   pub id: {id_ty},\n\
                 \x20   pub title: String,\n\
                 }}\n"
            );
            let tmp = project_with_model_source("post", &model_source);
            let err = plan_admin(tmp.path(), "Post", &[]).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("UUID primary keys are not yet supported")
                    && msg.contains("generate admin"),
                "expected a UUID admin-gate error for id type `{id_ty}`, got: {msg}"
            );
        }
    }

    #[test]
    fn plan_admin_allows_i64_keyed_model() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Post {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub title: String,\n\
            }\n";
        let tmp = project_with_model_source("post", model_source);
        assert!(
            plan_admin(tmp.path(), "Post", &[]).is_ok(),
            "i64-keyed model must not be rejected by the UUID admin gate"
        );
    }

    #[test]
    fn plan_admin_creates_three_actions() {
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "body:Text".into(),
                "published:bool".into(),
            ],
        )
        .unwrap();

        let paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();

        for expected in [
            "src/admin/post.rs",
            "src/admin/mod.rs",
            "tests/post_admin.rs",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "missing expected action for {expected}; got {paths:?}"
            );
        }
        assert_eq!(
            plan.actions.len(),
            3,
            "expected exactly 3 actions, got {paths:?}"
        );
    }

    #[test]
    fn plan_admin_accepts_pascal_or_snake_name() {
        let tmp = project_with_model("blog_post");
        let plan_snake = plan_admin(tmp.path(), "blog_post", &[]).unwrap();
        let plan_pascal = plan_admin(tmp.path(), "BlogPost", &[]).unwrap();

        let paths_snake: Vec<_> = plan_snake
            .actions
            .iter()
            .map(|a| a.path().to_path_buf())
            .collect();
        let paths_pascal: Vec<_> = plan_pascal
            .actions
            .iter()
            .map(|a| a.path().to_path_buf())
            .collect();
        assert_eq!(paths_snake, paths_pascal);
    }

    #[test]
    fn plan_admin_rejects_invalid_field_tokens() {
        let tmp = project_with_model("post");
        let err = plan_admin(tmp.path(), "Post", &["title".into()]).unwrap_err();
        assert!(
            matches!(err, GenerateError::InvalidField { .. }),
            "expected InvalidField, got {err:?}"
        );
    }

    // ── Generated content ─────────────────────────────────────────────────

    #[test]
    fn execute_writes_admin_file_with_struct_and_impl() {
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "body:Text".into(),
                "published:bool".into(),
            ],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();

        assert!(admin.contains("pub struct PostAdmin;"));
        assert!(admin.contains("impl AdminModel for PostAdmin {"));

        assert!(admin.contains("\"posts\""), "slug must be 'posts'");
        assert!(admin.contains("\"Post\""), "display_name must be 'Post'");
        assert!(
            admin.contains("\"Posts\""),
            "display_name_plural must be 'Posts'"
        );

        // Id is always hidden + readonly + hidden from list
        assert!(admin.contains("AdminField::new(\"id\", AdminFieldKind::Hidden)"));
        assert!(admin.contains(".readonly()"));
        assert!(admin.contains(".hide_from_list()"));

        assert!(
            admin.contains("AdminField::new(\"title\", AdminFieldKind::Text)"),
            "title should map to Text"
        );
        assert!(
            admin.contains("AdminField::new(\"body\", AdminFieldKind::TextArea)"),
            "body should map to TextArea"
        );
        assert!(
            admin.contains("AdminField::new(\"published\", AdminFieldKind::Boolean)"),
            "published should map to Boolean"
        );

        assert!(
            admin.contains(".searchable()"),
            "text fields should be searchable"
        );
        assert!(
            admin.contains(".filterable()"),
            "bool fields should be filterable"
        );

        assert!(admin.contains("fn list("));
        assert!(admin.contains("fn get("));
        assert!(admin.contains("fn create("));
        assert!(admin.contains("fn update("));
        assert!(admin.contains("fn delete("));

        assert!(admin.contains("use autumn_web::Patch;"));
        assert!(admin.contains("use crate::models::post::{Post, NewPost, UpdatePost};"));
        assert!(admin.contains("use crate::schema::posts;"));
    }

    #[test]
    fn execute_writes_admin_file_with_apply_filters_and_sort() {
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &["title:String".into(), "published:bool".into()],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains("fn apply_filters"),
            "must have apply_filters fn"
        );
        assert!(admin.contains("fn apply_sort"), "must have apply_sort fn");
        assert!(
            admin.contains("posts::title.ilike"),
            "title should be in ilike search"
        );
        assert!(
            admin.contains("posts::published.eq(true)"),
            "published should have bool filter"
        );
        assert!(admin.contains("\"title\""), "sort arm for title");
        assert!(admin.contains("\"published\""), "sort arm for published");
    }

    #[test]
    fn execute_writes_update_body_with_update_type() {
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &["title:String".into(), "published:bool".into()],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(admin.contains("let new_row: NewPost ="));
        assert!(admin.contains("let changes = UpdatePost {"));
        assert!(admin.contains("title: Patch::Set(new_row.title)"));
        assert!(admin.contains("published: Patch::Set(new_row.published)"));
        assert!(admin.contains("..Default::default()"));
        assert!(admin.contains("let diesel_changeset = changes.__to_changeset()"));
        assert!(admin.contains(".set(&diesel_changeset)"));
    }

    #[test]
    fn position_field_excluded_from_admin_update_body() {
        // `#[position]` (#1358) is absent from `New{Model}`/`Update{Model}`, so a reference to it
        // would not compile.
        let tmp = project_with_model("task");
        let plan = plan_admin(
            tmp.path(),
            "Task",
            &["title:String".into(), "rank:position".into()],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/task.rs")).unwrap();
        assert!(
            !admin.contains("new_row.rank") && !admin.contains("rank: Patch::Set"),
            "a position field must never be referenced in the New/Update struct \
             construction, both of which exclude it:\n{admin}"
        );
        assert!(
            admin.contains("title: Patch::Set(new_row.title)"),
            "the ordinary field must still be writable:\n{admin}"
        );
    }

    #[test]
    fn commentable_counter_excluded_from_admin_update_body() {
        // The commentable counter (#1367) is `#[default]` and absent from `New{Model}` and
        // `Update{Model}`, so `Patch::Set(new_row.comment_count)` would not compile.
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &["title:String".into(), "comments:commentable".into()],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            !admin.contains("new_row.comment_count")
                && !admin.contains("comment_count: Patch::Set"),
            "the commentable counter must never be referenced in the New/Update \
             struct construction:\n{admin}"
        );
        assert!(
            admin.contains("title: Patch::Set(new_row.title)"),
            "the ordinary field must still be writable:\n{admin}"
        );
    }

    #[test]
    fn lock_version_admin_update_bumps_current_column_without_update_default() {
        let tmp = project_with_model_source(
            "post",
            r"
#[autumn_web::model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[lock_version]
    pub lock_version: i64,
}
",
        );
        let plan = plan_admin(tmp.path(), "Post", &["title:String".into()]).unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(admin.contains("let new_row: NewPost ="));
        assert!(
            admin.contains("posts::title.eq(new_row.title)"),
            "admin update should still write editable fields"
        );
        assert!(
            admin.contains("posts::lock_version.eq(posts::lock_version + 1)"),
            "admin update must bump the current stored lock_version"
        );
        assert!(
            !admin.contains("let changes = UpdatePost"),
            "lock-versioned admin update must not build UpdatePost with Default"
        );
        assert!(
            !admin.contains("__to_changeset"),
            "lock-versioned admin update must not call UpdatePost::__to_changeset()"
        );
        assert!(
            !admin.contains("UpdatePost"),
            "lock-versioned admin adapter should not import the unused UpdatePost type"
        );
        assert!(
            !admin.contains("use autumn_web::Patch;"),
            "lock-versioned direct updates should not import unused Patch"
        );
    }

    #[test]
    fn lock_version_field_is_not_rendered_as_an_admin_editable_field() {
        let tmp = project_with_model_source(
            "post",
            r"
#[autumn_web::model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[lock_version]
    pub lock_version: i64,
}
",
        );
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i64".into()],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            !admin.contains("AdminField::new(\"lock_version\""),
            "repository-managed lock_version must not appear in fields()"
        );
        assert!(
            !admin.contains("\"lock_version\" =>"),
            "repository-managed lock_version must not get filter or sort arms"
        );
        assert!(
            !admin.contains("lock_version=test"),
            "smoke test form body must not submit lock_version"
        );
    }

    #[test]
    fn encrypted_columns_are_detected_and_marked_in_generated_admin() {
        let tmp = project_with_model_source(
            "account",
            r"
#[autumn_web::model]
pub struct Account {
    #[id]
    pub id: i64,
    pub username: String,
    #[encrypted]
    pub api_token: String,
    #[encrypted(deterministic, admin_visible)]
    pub email: String,
}
",
        );
        let plan = plan_admin(
            tmp.path(),
            "Account",
            &[
                "username:String".into(),
                "api_token:String".into(),
                "email:String".into(),
            ],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/account.rs")).unwrap();
        assert!(
            admin.contains("AdminField::new(\"api_token\", AdminFieldKind::Text)")
                && admin.contains(".encrypted()"),
            "randomized encrypted column must be marked .encrypted(): {admin}"
        );
        assert!(
            admin.contains(".encrypted_visible()"),
            "admin_visible encrypted column must be marked .encrypted_visible(): {admin}"
        );
        assert!(
            !admin.contains(
                "AdminField::new(\"username\", AdminFieldKind::Text)\n                .encrypted"
            ),
            "plaintext column must not be marked encrypted: {admin}"
        );
        assert!(
            admin.contains(
                "AdminField::new(\"username\", AdminFieldKind::Text)\n                .searchable()"
            ),
            "plaintext String column stays searchable: {admin}"
        );
        // ILIKE over ciphertext never matches plaintext (#805), so only `username` is searchable.
        assert_eq!(
            admin.matches(".searchable()").count(),
            1,
            "only the non-encrypted column is searchable: {admin}"
        );
    }

    #[test]
    fn execute_writes_smoke_test_with_three_tests() {
        let tmp = project_with_model("post");
        let plan = plan_admin(tmp.path(), "Post", &["title:String".into()]).unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/post_admin.rs")).unwrap();

        assert!(test.contains("fn post_admin_anonymous_access_is_rejected()"));
        assert!(test.contains("fn post_admin_list_loads_for_admin_user()"));
        assert!(test.contains("fn post_admin_create_redirects_for_admin_user()"));

        assert!(test.contains("AUTUMN_TEST_BASE_URL"));
        assert!(test.contains("AUTUMN_TEST_ADMIN_SESSION"));

        // Anonymous test expects 302/401/403
        assert!(test.contains("302") && test.contains("401") && test.contains("403"));

        // Admin list test expects 200
        assert!(test.contains("200"));

        assert!(test.contains("/admin/posts"));
    }

    #[test]
    fn admin_mod_rs_gets_mod_declaration() {
        let tmp = project_with_model("post");
        let plan = plan_admin(tmp.path(), "Post", &[]).unwrap();
        plan.execute(Flags::default()).unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/admin/mod.rs")).unwrap();
        assert!(
            mod_rs.contains("pub mod post;"),
            "src/admin/mod.rs must declare the new module; got: {mod_rs}"
        );
    }

    #[test]
    fn dry_run_does_not_write_admin_file() {
        let tmp = project_with_model("post");
        let plan = plan_admin(tmp.path(), "Post", &[]).unwrap();
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(
            !tmp.path().join("src/admin/post.rs").exists(),
            "dry-run must not create files"
        );
    }

    #[test]
    fn collision_without_force_errors() {
        let tmp = project_with_model("post");
        // An existing admin file triggers the collision check.
        let admin_dir = tmp.path().join("src/admin");
        fs::create_dir_all(&admin_dir).unwrap();
        fs::write(admin_dir.join("post.rs"), "// existing").unwrap();

        let plan = plan_admin(tmp.path(), "Post", &[]).unwrap();
        let err = plan.execute(Flags::default()).unwrap_err();
        assert!(
            err.to_string().contains("post.rs"),
            "collision error must name the conflicting file"
        );
    }

    #[test]
    fn exclude_option_omits_field() {
        let tmp = project_with_model("post");
        let options = AdminOptions {
            exclude: vec!["body".into()],
            ..Default::default()
        };
        let plan = plan_admin_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "body:Text".into()],
            &options,
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            !admin.contains("\"body\""),
            "excluded field 'body' must not appear in generated fields()"
        );
        assert!(
            admin.contains("\"title\""),
            "non-excluded 'title' must still appear"
        );
    }

    #[test]
    fn hidden_option_changes_field_kind() {
        let tmp = project_with_model("post");
        let options = AdminOptions {
            hidden: vec!["title".into()],
            ..Default::default()
        };
        let plan = plan_admin_with_options(tmp.path(), "Post", &["title:String".into()], &options)
            .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains("AdminField::new(\"title\", AdminFieldKind::Hidden)"),
            "hidden option must override field kind to Hidden"
        );
    }

    #[test]
    fn password_option_changes_field_kind() {
        let tmp = project_with_model("user");
        let options = AdminOptions {
            password: vec!["password_hash".into()],
            ..Default::default()
        };
        let plan = plan_admin_with_options(
            tmp.path(),
            "user",
            &["password_hash:String".into()],
            &options,
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/user.rs")).unwrap();
        assert!(
            admin.contains("AdminField::new(\"password_hash\", AdminFieldKind::Password)"),
            "password option must change kind to Password"
        );
    }

    #[test]
    fn readonly_option_adds_readonly_call() {
        let tmp = project_with_model("post");
        let options = AdminOptions {
            readonly: vec!["title".into()],
            ..Default::default()
        };
        let plan = plan_admin_with_options(tmp.path(), "Post", &["title:String".into()], &options)
            .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains(".readonly()"),
            "readonly option must add .readonly() call"
        );
    }

    #[test]
    fn datetime_fields_are_readonly_and_optional_by_default() {
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &[
                "created_at:DateTime".into(),
                "updated_at:NaiveDateTime".into(),
            ],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains("AdminFieldKind::DateTime"),
            "datetime fields should map to DateTime kind"
        );
        assert!(
            admin.contains(".readonly()"),
            "datetime fields must be readonly by default"
        );
        assert!(
            admin.contains(".optional()"),
            "datetime fields must be optional by default"
        );
    }

    #[test]
    fn admin_field_kind_maps_all_types() {
        use super::super::dsl::parse_field;
        let cases = [
            ("x:String", "AdminFieldKind::Text"),
            ("x:Text", "AdminFieldKind::TextArea"),
            ("x:i32", "AdminFieldKind::Integer"),
            ("x:i64", "AdminFieldKind::Integer"),
            ("x:bool", "AdminFieldKind::Boolean"),
            ("x:f32", "AdminFieldKind::Float"),
            ("x:f64", "AdminFieldKind::Float"),
            ("x:Uuid", "AdminFieldKind::Text"),
            ("x:NaiveDateTime", "AdminFieldKind::DateTime"),
            ("x:DateTime", "AdminFieldKind::DateTime"),
            ("x:Bytea", "AdminFieldKind::Hidden"),
        ];
        for (token, expected_kind) in cases {
            let field = parse_field(token).unwrap();
            assert_eq!(
                admin_field_kind(&field),
                expected_kind,
                "field token '{token}' should map to {expected_kind}"
            );
        }
    }

    #[test]
    fn datetime_fields_excluded_from_update_by_default() {
        let tmp = project_with_model("post");
        let plan = plan_admin(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "created_at:DateTime".into(),
                "updated_at:NaiveDateTime".into(),
            ],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            !admin.contains("created_at: Patch::Set(new_row.created_at)"),
            "created_at should not appear in UpdatePost (readonly by default)"
        );
        assert!(
            !admin.contains("updated_at: Patch::Set(new_row.updated_at)"),
            "updated_at should not appear in UpdatePost (readonly by default)"
        );
        assert!(
            admin.contains("title: Patch::Set(new_row.title)"),
            "title should appear in UpdatePost"
        );
    }

    #[test]
    fn required_uuid_fields_are_editable_by_default() {
        let tmp = project_with_model("api_token");
        let plan = plan_admin(tmp.path(), "ApiToken", &["token:Uuid".into()]).unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/api_token.rs")).unwrap();
        assert!(
            admin.contains("AdminField::new(\"token\", AdminFieldKind::Text),"),
            "required user-provided UUID fields should be editable by default:\n{admin}"
        );
        assert!(
            admin.contains("token: Patch::Set(new_row.token)"),
            "required user-provided UUID fields must be included in admin updates:\n{admin}"
        );
    }

    #[test]
    fn select_option_generates_select_kind_with_options() {
        let tmp = project_with_model("post");
        let options = AdminOptions {
            select: vec![SelectSpec {
                field: "status".into(),
                values: vec!["draft".into(), "published".into(), "archived".into()],
            }],
            ..Default::default()
        };
        let plan = plan_admin_with_options(tmp.path(), "Post", &["status:String".into()], &options)
            .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains("AdminFieldKind::Select(vec!["),
            "select option must emit Select kind"
        );
        assert!(admin.contains("\"draft\""), "option values must appear");
        assert!(admin.contains("\"published\""), "option values must appear");
        assert!(admin.contains("\"archived\""), "option values must appear");
        assert!(admin.contains("\"Draft\""), "labels must be title-cased");
    }

    #[test]
    fn enum_field_auto_derives_select_kind() {
        // An `enum{...}` field renders as a `Select` without a repeated `--select` (#1030).
        let tmp = project_with_model("post");
        let plan = plan_admin_with_options(
            tmp.path(),
            "Post",
            &["status:enum{draft,published,archived}".into()],
            &AdminOptions::default(),
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains(
                "AdminField::new(\"status\", AdminFieldKind::Select(vec![SelectOption { value: \"draft\".into(), label: \"Draft\".into() }, SelectOption { value: \"published\".into(), label: \"Published\".into() }, SelectOption { value: \"archived\".into(), label: \"Archived\".into() }]))"
            ),
            "got:\n{admin}"
        );
    }

    #[test]
    fn explicit_select_overrides_enum_auto_derivation() {
        let tmp = project_with_model("post");
        let options = AdminOptions {
            select: vec![SelectSpec {
                field: "status".into(),
                values: vec!["draft".into(), "published".into()],
            }],
            ..Default::default()
        };
        let plan = plan_admin_with_options(
            tmp.path(),
            "Post",
            &["status:enum{draft,published,archived}".into()],
            &options,
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(!admin.contains("\"archived\""), "got:\n{admin}");
    }

    #[test]
    fn admin_smoke_test_uses_first_variant_for_enum_fields() {
        // The smoke test posts a sample for each writable field; `status=test` would fail the
        // enum's validation instead of redirecting.
        let tmp = project_with_model("post");
        let plan = plan_admin_with_options(
            tmp.path(),
            "Post",
            &["status:enum{draft,published,archived}".into()],
            &AdminOptions::default(),
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let smoke = fs::read_to_string(tmp.path().join("tests/post_admin.rs")).unwrap();
        assert!(
            smoke.contains("status=draft"),
            "expected the enum's first variant in the sample form body; got:\n{smoke}"
        );
        assert!(
            !smoke.contains("status=test"),
            "must not submit an out-of-set sample value for an enum field; got:\n{smoke}"
        );
    }

    #[test]
    fn select_option_bare_field_emits_empty_select() {
        let tmp = project_with_model("post");
        let options = AdminOptions {
            select: vec![SelectSpec {
                field: "status".into(),
                values: vec![],
            }],
            ..Default::default()
        };
        let plan = plan_admin_with_options(tmp.path(), "Post", &["status:String".into()], &options)
            .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/post.rs")).unwrap();
        assert!(
            admin.contains("AdminFieldKind::Select(vec![])"),
            "bare --select must emit empty Select placeholder"
        );
    }

    // `SelectOption` must resolve from `autumn_admin_plugin::prelude::*`, or generated select
    // adapters do not compile (#782).
    #[test]
    fn select_option_generated_code_uses_prelude_importable_type() {
        let tmp = project_with_model("ticket");
        let options = AdminOptions {
            select: vec![SelectSpec {
                field: "priority".into(),
                values: vec!["low".into(), "medium".into(), "high".into()],
            }],
            ..Default::default()
        };
        let plan =
            plan_admin_with_options(tmp.path(), "Ticket", &["priority:String".into()], &options)
                .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/ticket.rs")).unwrap();

        assert!(
            admin.contains("use autumn_admin_plugin::prelude::*;"),
            "generated adapter must use prelude glob import"
        );
        assert!(
            admin.contains("SelectOption {"),
            "generated adapter must emit SelectOption struct literals for select fields"
        );
        assert!(admin.contains("\"low\""), "option value 'low' must appear");
        assert!(
            admin.contains("\"medium\""),
            "option value 'medium' must appear"
        );
        assert!(
            admin.contains("\"high\""),
            "option value 'high' must appear"
        );
    }

    #[test]
    fn parse_select_specs_parses_field_with_values() {
        let specs = parse_select_specs(&["status=draft,published,archived".into()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].field, "status");
        assert_eq!(specs[0].values, vec!["draft", "published", "archived"]);
    }

    #[test]
    fn parse_select_specs_bare_field_produces_empty_values() {
        let specs = parse_select_specs(&["status".into()]).unwrap();
        assert_eq!(specs[0].field, "status");
        assert!(specs[0].values.is_empty());
    }

    #[test]
    fn parse_select_specs_rejects_empty_token() {
        let err = parse_select_specs(&[String::new()]).unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
    }

    #[test]
    fn no_searchable_fields_produces_valid_apply_filters() {
        let tmp = project_with_model("counter");
        let plan = plan_admin(
            tmp.path(),
            "counter",
            &["count:i64".into(), "active:bool".into()],
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let admin = fs::read_to_string(tmp.path().join("src/admin/counter.rs")).unwrap();
        assert!(
            admin.contains("fn apply_filters"),
            "apply_filters must exist"
        );
        assert!(
            !admin.contains(".ilike("),
            "no ilike when there are no searchable fields"
        );
    }

    // ── `plan_admin_destroy_fallback` (#1048) ──────────────────────────────

    #[test]
    fn destroy_admin_after_model_already_destroyed_still_removes_admin_outputs() {
        // `destroy model Post` then `destroy admin Post` must not strand the admin outputs once the
        // model file is gone.
        let tmp = project_with_model("post");
        let plan = plan_admin(tmp.path(), "Post", &["title:String".into()]).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/admin/post.rs").exists());
        assert!(tmp.path().join("tests/post_admin.rs").exists());

        // Simulate `autumn destroy model Post` having already run.
        fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(plan_admin(tmp.path(), "Post", &["title:String".into()]).is_err());

        let fallback_plan = plan_admin_destroy_fallback(tmp.path(), "Post").unwrap();
        // The fallback plan cannot reproduce the content — it never read the
        // model — but the digest `generate` recorded still proves these files
        // are its own untouched output, so no --force is needed (issue #1835).
        fallback_plan.revert(Flags::default()).unwrap();
        assert!(!tmp.path().join("src/admin/post.rs").exists());
        assert!(!tmp.path().join("tests/post_admin.rs").exists());
        assert!(
            !fs::read_to_string(tmp.path().join("src/admin/mod.rs"))
                .unwrap_or_default()
                .contains("post"),
            "the mod declaration removal doesn't depend on the model and must succeed too"
        );
    }

    #[test]
    fn destroy_admin_fallback_still_refuses_a_hand_edited_file() {
        // The recorded digest (#1835) makes the fallback usable; an edited file still blocks it.
        let tmp = project_with_model("post");
        let plan = plan_admin(tmp.path(), "Post", &["title:String".into()]).unwrap();
        plan.execute(Flags::default()).unwrap();
        fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();
        fs::write(tmp.path().join("src/admin/post.rs"), "// my own code\n").unwrap();

        let err = plan_admin_destroy_fallback(tmp.path(), "Post")
            .unwrap()
            .revert(Flags::default())
            .unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(tmp.path().join("src/admin/post.rs").exists());
    }

    #[test]
    fn destroy_admin_fallback_without_provenance_still_needs_force() {
        // A project generated before the manifest existed (#1835) has no digest, so its files
        // count as diverged and need --force.
        let tmp = project_with_model("post");
        let plan = plan_admin(tmp.path(), "Post", &["title:String".into()]).unwrap();
        plan.execute(Flags::default()).unwrap();
        fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();
        fs::remove_file(tmp.path().join(crate::generate::provenance::MANIFEST_PATH)).unwrap();

        let err = plan_admin_destroy_fallback(tmp.path(), "Post")
            .unwrap()
            .revert(Flags::default())
            .unwrap_err();
        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(tmp.path().join("src/admin/post.rs").exists());

        plan_admin_destroy_fallback(tmp.path(), "Post")
            .unwrap()
            .revert(Flags {
                dry_run: false,
                force: true,
            })
            .unwrap();
        assert!(!tmp.path().join("src/admin/post.rs").exists());
        assert!(!tmp.path().join("tests/post_admin.rs").exists());
    }

    #[test]
    fn plan_admin_destroy_fallback_fails_outside_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_admin_destroy_fallback(tmp.path(), "Post").unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_admin_destroy_fallback_rejects_path_traversal_in_name() {
        // No model-file check narrows `name` here, so path separators must be rejected before they
        // build deletion paths.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let err = plan_admin_destroy_fallback(tmp.path(), "../../foo").unwrap_err();
        assert!(
            matches!(err, GenerateError::InvalidName(..)),
            "expected InvalidName, got {err:?}"
        );
    }

    /// The generated `src/admin/<snake>.rs` body from an admin plan.
    fn admin_adapter_contents(plan: &Plan) -> String {
        plan.actions
            .iter()
            .find(|a| {
                a.path()
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with("src/admin/account.rs")
            })
            .map(|a| match a {
                crate::generate::emit::Action::Create { contents, .. }
                | crate::generate::emit::Action::Modify { contents, .. } => contents.clone(),
                _ => String::new(),
            })
            .expect("admin adapter action")
    }

    // ── `{encrypted}` DSL → admin redaction, end-to-end (issue #1340) ───────

    /// AC5: the model text the *generator* emits for a `{encrypted}` DSL token
    /// must be picked up by the admin's existing `detect_encrypted_fields`,
    /// with no extra flags — closing the loop the issue is about.
    ///
    /// The model source here is not hand-written: it is exactly what
    /// `render_model_file_for_test` produces for those tokens, so a drift in
    /// the emitted attribute spelling fails this test rather than silently
    /// un-redacting a scaffolded PII column.
    #[test]
    fn generator_emitted_encrypted_attribute_is_detected_by_the_admin() {
        let tokens = [
            "username:String".to_owned(),
            "api_token:String{encrypted}".to_owned(),
            "email:String{encrypted:deterministic}".to_owned(),
        ];
        let fields = crate::generate::dsl::parse_fields(&tokens).unwrap();
        let model_source =
            crate::generate::model::render_model_file_for_test("Account", "accounts", &fields);
        // Sanity: the source under test really carries the attributes.
        assert!(model_source.contains("#[encrypted]"), "{model_source}");
        assert!(
            model_source.contains("#[encrypted(deterministic)]"),
            "{model_source}"
        );

        let tmp = project_with_model_source("account", &model_source);
        let plan = plan_admin(tmp.path(), "Account", &tokens).unwrap();
        let admin = admin_adapter_contents(&plan);

        assert!(
            admin.contains(".encrypted()"),
            "the admin must redact the generator-emitted encrypted columns:\n{admin}"
        );
        assert!(
            !admin.contains(".encrypted_visible()"),
            "no column opted into admin visibility:\n{admin}"
        );
        // Redaction is per-field: the plaintext column stays editable and searchable.
        assert!(
            admin.contains("AdminField::new(\"username\""),
            "admin: {admin}"
        );
        assert_eq!(
            admin.matches(".encrypted()").count(),
            2,
            "exactly the two encrypted columns must be redacted:\n{admin}"
        );
        // ILIKE over ciphertext never matches plaintext, so encrypted columns stay out of search.
        assert!(
            !admin.contains("api_token.ilike") && !admin.contains("email.ilike"),
            "admin: {admin}"
        );
    }

    /// The MODEL is the only source of truth for at-rest encryption: `#[encrypted]`
    /// is what installs the `serialize_as` wrapper on the write path. A
    /// `{encrypted}` DSL token that the model does not back must therefore be
    /// refused, not honoured.
    ///
    /// Honouring it would mark the admin field `.encrypted()`, which changes the
    /// UI only — the admin's own create form would still write the submitted
    /// value to an unwrapped column as PLAINTEXT and then render `••••••••` over
    /// it. That manufactures a false at-rest guarantee, which is worse than not
    /// redacting at all.
    #[test]
    fn encrypted_dsl_token_without_the_model_attribute_is_refused() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Account {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub api_token: String,\n\
            }\n";
        let tmp = project_with_model_source("account", model_source);
        let err = plan_admin(
            tmp.path(),
            "Account",
            &["api_token:String{encrypted}".to_owned()],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_token"), "must name the field: {msg}");
        assert!(
            msg.contains("no `#[encrypted]` attribute"),
            "must say the model lacks the attribute: {msg}"
        );
        assert!(
            msg.contains("stores plaintext"),
            "must explain why redacting anyway is wrong: {msg}"
        );
    }

    /// Diesel implements `Insertable`/`AsChangeset` for the OWNED value only once
    /// a column uses `serialize_as`, so a generated admin over an encrypted model
    /// must not borrow — otherwise it does not compile.
    #[test]
    fn admin_over_an_encrypted_model_passes_owned_insert_and_changeset() {
        let tokens = [
            "username:String".to_owned(),
            "api_token:String{encrypted}".to_owned(),
        ];
        let fields = crate::generate::dsl::parse_fields(&tokens).unwrap();
        let model_source =
            crate::generate::model::render_model_file_for_test("Account", "accounts", &fields);
        let tmp = project_with_model_source("account", &model_source);
        let admin = admin_adapter_contents(&plan_admin(tmp.path(), "Account", &tokens).unwrap());

        assert!(admin.contains(".values(new_row)"), "admin: {admin}");
        assert!(!admin.contains(".values(&new_row)"), "admin: {admin}");
        assert!(admin.contains(".set(diesel_changeset)"), "admin: {admin}");
        assert!(!admin.contains(".set(&diesel_changeset)"), "admin: {admin}");
    }

    /// …and a plaintext model keeps the borrowed form byte-for-byte.
    #[test]
    fn admin_over_a_plaintext_model_keeps_the_borrowed_insert_and_changeset() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Account {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub username: String,\n\
            }\n";
        let tmp = project_with_model_source("account", model_source);
        let admin = admin_adapter_contents(
            &plan_admin(tmp.path(), "Account", &["username:String".to_owned()]).unwrap(),
        );
        assert!(admin.contains(".values(&new_row)"), "admin: {admin}");
        assert!(admin.contains(".set(&diesel_changeset)"), "admin: {admin}");
    }

    /// An at-rest encrypted column is never updatable through the admin, on
    /// either update path.
    ///
    /// The plugin renders its edit control disabled and with NO `name`
    /// ("managed outside the admin"), so the form cannot submit one. Two things
    /// follow, and both are asserted here:
    ///
    /// 1. The column must be absent from the update — otherwise the handler
    ///    would write whatever stood in for the key it never received. On the
    ///    `lock_version` path that matters twice over: it emits a RAW
    ///    `col.eq(value)` tuple that never builds an `Update{Model}` changeset,
    ///    so it would bypass `serialize_as` and store PLAINTEXT in the
    ///    encrypted column — the same defect this PR fixed in the scaffolded
    ///    HTML update handler.
    /// 2. `New{Model}` still requires the field, so the handler must back-fill
    ///    a placeholder or `serde_json::from_value` fails with
    ///    "missing field" and EVERY edit 422s, even one touching only a
    ///    plaintext column.
    #[test]
    fn admin_never_updates_encrypted_columns_on_either_path() {
        let encrypted_model = "#[autumn_web::model]\n\
            pub struct Account {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub username: String,\n\
            \x20   #[encrypted]\n\
            \x20   pub api_token: String,\n\
            \x20   #[encrypted(deterministic)]\n\
            \x20   pub email: String,\n";
        let tokens = [
            "username:String".to_owned(),
            "api_token:String{encrypted}".to_owned(),
            "email:String{encrypted:deterministic}".to_owned(),
        ];

        // Both the ordinary changeset path and the `lock_version` raw-tuple path.
        for (suffix, extra_token) in [
            ("}\n", None),
            (
                "\x20   #[lock_version]\n\x20   pub lock_version: i32,\n}\n",
                Some("lock_version:i32".to_owned()),
            ),
        ] {
            let tmp = project_with_model_source("account", &format!("{encrypted_model}{suffix}"));
            let mut tokens = tokens.to_vec();
            tokens.extend(extra_token);
            let admin =
                admin_adapter_contents(&plan_admin(tmp.path(), "Account", &tokens).unwrap());

            // (1) No update path may reference an encrypted column's value.
            for leaked in [
                "api_token: Patch::Set",
                "email: Patch::Set",
                "accounts::api_token.eq(",
                "accounts::email.eq(",
            ] {
                assert!(
                    !admin.contains(leaked),
                    "encrypted column must not be updated: `{leaked}`\n{admin}"
                );
            }
            assert!(
                admin.contains("username: Patch::Set(new_row.username)")
                    || admin.contains("accounts::username.eq(new_row.username)"),
                "the plaintext column must still update:\n{admin}"
            );
            // (2) The placeholder back-fill keeps `New{Model}` deserializable.
            assert!(
                admin.contains("obj.entry(col.to_string())"),
                "the handler must back-fill absent encrypted keys:\n{admin}"
            );
            for col in ["\"api_token\"", "\"email\""] {
                assert!(
                    admin.contains(col),
                    "the back-fill must name {col}:\n{admin}"
                );
            }
        }
    }

    /// A plaintext model emits no back-fill at all — the whole block is absent,
    /// so those admins stay byte-for-byte as before.
    #[test]
    fn admin_over_a_plaintext_model_emits_no_encrypted_placeholder() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Account {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   pub username: String,\n\
            }\n";
        let tmp = project_with_model_source("account", model_source);
        let admin = admin_adapter_contents(
            &plan_admin(tmp.path(), "Account", &["username:String".to_owned()]).unwrap(),
        );
        assert!(!admin.contains("let mut data = data;"), "admin: {admin}");
        assert!(
            admin.contains("username: Patch::Set(new_row.username)"),
            "admin: {admin}"
        );
    }

    /// Review finding: the generated admin emitted `ORDER BY` arms for
    /// encrypted columns, i.e. it sorted by base64 envelope bytes — arbitrary
    /// for a randomized column and unrelated to plaintext order for a
    /// deterministic one. Those arms are now omitted so the sort falls through
    /// to the honest `id` default, matching the scaffolded index's
    /// `.sortable(..)` suppression.
    #[test]
    fn admin_does_not_sort_by_encrypted_columns() {
        let tokens = [
            "username:String".to_owned(),
            "api_token:String{encrypted}".to_owned(),
        ];
        let fields = crate::generate::dsl::parse_fields(&tokens).unwrap();
        let model_source =
            crate::generate::model::render_model_file_for_test("Account", "accounts", &fields);
        let tmp = project_with_model_source("account", &model_source);
        let plan = plan_admin(tmp.path(), "Account", &tokens).unwrap();
        let admin = admin_adapter_contents(&plan);

        assert!(
            admin.contains("accounts::username.asc()"),
            "a plaintext column keeps its sort arms:\n{admin}"
        );
        assert!(
            !admin.contains("accounts::api_token.asc()")
                && !admin.contains("accounts::api_token.desc()"),
            "an encrypted column must not get an ORDER BY arm:\n{admin}"
        );
    }

    /// Precedence when the model file and the DSL token disagree: an explicit,
    /// hand-written `#[encrypted(admin_visible)]` is a deliberate opt-in the
    /// DSL cannot spell, so a bare `{encrypted}` token must not silently revoke
    /// it. (The reverse — a model with no attribute, a token that says
    /// encrypted — fails closed and redacts; see the test above.)
    #[test]
    fn model_admin_visible_opt_in_wins_over_a_bare_encrypted_token() {
        let model_source = "#[autumn_web::model]\n\
            pub struct Account {\n\
            \x20   #[id]\n\
            \x20   pub id: i64,\n\
            \x20   #[encrypted(admin_visible)]\n\
            \x20   pub api_token: String,\n\
            }\n";
        let tmp = project_with_model_source("account", model_source);
        let plan = plan_admin(
            tmp.path(),
            "Account",
            &["api_token:String{encrypted}".to_owned()],
        )
        .unwrap();
        let admin = admin_adapter_contents(&plan);
        assert!(
            admin.contains(".encrypted_visible()"),
            "the model's explicit opt-in must survive:\n{admin}"
        );
        assert!(
            !admin.contains(".encrypted()"),
            "the column must not be marked both ways:\n{admin}"
        );
    }
}
