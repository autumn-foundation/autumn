//! `#[commentable]` — the polymorphic association kind (issue #1367).
//!
//! Consumed by `#[model]` (like `#[votable]`, and for the same reason: the
//! attribute must be *below* `#[model]`, which strips it), this expands to
//!
//! 1. a private `static` [`CommentableSpec`] holding every table/column name
//!    the runtime's dynamic SQL splices, plus the `inventory` registration that
//!    lets [`autumn_web::commentable::router`] serve any commentable model
//!    without the app writing a route per model;
//! 2. inherent `COMMENTABLE_TYPE` / `commentable_spec()` items on the model
//!    (inherent, so no trait import is needed at the call site — the same
//!    shadowing trick `counter_caches()` uses);
//! 3. a `{Model}Comments` trait — `add_comment` / `comment_thread` /
//!    `delete_comment` — blanket-implemented over
//!    `M2mConnSource<Model = Model>`, exactly like `#[votable]`'s
//!    `{Model}Reactions`, so method resolution stays unambiguous when several
//!    models' comment traits are in scope.
//!
//! Everything the generated SQL interpolates is validated here to be a plain
//! Rust identifier; the runtime re-checks it under `debug_assertions`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::LitStr;

use crate::model::{infer_table_name, pascal_to_snake};

/// A resolved `#[commentable(...)]` declaration.
///
/// Every field has a convention-derived default, so the canonical declaration
/// is the one-liner `#[commentable(by = User)]` → a shared
/// `comments(commentable_type, commentable_id, parent_id, author_id, body)`
/// table maintaining `posts.comment_count`.
#[derive(Debug)]
pub struct CommentableSpec {
    /// The author model named by `by = <Model>` (e.g. `User`), when the
    /// declaration names one. `None` is fine — the comments table stores a
    /// bare `i64` author id — but then nothing resolves a display name, so
    /// `author_name` requires either `by` or an explicit `author_table`.
    pub author_model: Option<syn::Ident>,
    /// The discriminator value stored in `commentable_type`. Defaults to the
    /// model's own Rust type name.
    pub type_name: String,
    /// The shared comments table, default `comments`.
    pub table: String,
    /// The comments table's primary key, default `id`.
    pub comment_pk: String,
    /// Default `commentable_type`.
    pub type_column: String,
    /// Default `commentable_id`.
    pub id_column: String,
    /// Default `parent_id`.
    pub parent_column: String,
    /// Default `author_id`.
    pub author_column: String,
    /// Default `body`.
    pub body_column: String,
    /// Default `created_at`.
    pub created_at_column: String,
    /// Whether the comments table carries `deleted_at`. Default `true`.
    pub soft_delete: bool,
    /// The maintained counter column on *this* model, default `comment_count`.
    /// `None` when `counter_cache = false`.
    pub counter_column: Option<String>,
    /// The author table, default `pluralize(snake(by))`.
    pub author_table: String,
    /// The author table's primary key, default `id`.
    pub author_pk: String,
    /// The author display-name column. `None` (the default) resolves no name —
    /// the framework refuses to guess a column.
    pub author_name_column: Option<String>,
    /// Maximum nesting depth; a top-level comment is depth `0`.
    pub max_depth: u32,
    /// Cap on one comment body, in bytes.
    pub max_body_bytes: usize,
    /// Span of the attribute, for diagnostics raised after parsing.
    pub span: proc_macro2::Span,
}

/// Whether an attribute is the `#[commentable]` declaration consumed by
/// `#[model]` (and therefore must not be re-emitted onto the Diesel struct,
/// where it would fail with "cannot find attribute `commentable` in this
/// scope").
pub fn is_commentable_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("commentable")
}

/// Reject a `#[commentable(key = value)]` value that is not a plain
/// identifier.
///
/// Every name-shaped value here is spliced verbatim into generated SQL. The
/// runtime's `debug_assert` is the backstop; this is the directed, spanned
/// error that keeps the mistake from ever getting that far.
fn check_ident_value(key: &syn::Ident, value: &str, span: proc_macro2::Span) -> syn::Result<()> {
    let plain = !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        return Ok(());
    }
    Err(syn::Error::new(
        span,
        format!(
            "`{value}` is not a valid identifier for `{key} = ...` in \
             `#[commentable]`: the value is spliced verbatim into the generated \
             SQL's table and column names, so it must be a plain identifier — \
             ASCII letters, digits and underscores only, and no leading digit"
        ),
    ))
}

/// The exclusive upper bound on `max_depth`, mirroring the runtime's
/// `RECURSION_GUARD`. The proc-macro crate cannot reference `autumn_web`, so
/// the two are kept in step by a test in `autumn/tests/integration/commentable.rs`.
const MAX_DEPTH_CEILING: u32 = 1_000;

/// Every key `#[commentable(...)]` accepts, for the unknown-key diagnostic.
const KNOWN_KEYS: &[&str] = &[
    "by",
    "type_name",
    "table",
    "comment_pk",
    "type_column",
    "id_column",
    "parent_column",
    "author_column",
    "body_column",
    "created_at_column",
    "soft_delete",
    "counter_cache",
    "author_table",
    "author_pk",
    "author_name",
    "max_depth",
    "max_body",
];

/// One parsed `key = value` pair: the raw text plus where it came from.
struct KeyValue {
    value: String,
    span: proc_macro2::Span,
    /// `true` when the value was a bare `true`/`false` literal.
    boolean: Option<bool>,
    /// `Some` when the value was an integer literal.
    integer: Option<u64>,
}

/// Parse a single `#[commentable(...)]` attribute.
///
/// Grammar — a `key = value` loop, each value a bare identifier, a string
/// literal, an integer, or `true`/`false`:
///
/// ```text
/// #[commentable(by = User, type_name = "Post", table = comments,
///               comment_pk = id, type_column = commentable_type,
///               id_column = commentable_id, parent_column = parent_id,
///               author_column = author_id, body_column = body,
///               created_at_column = created_at, soft_delete = true,
///               counter_cache = comment_count | false,
///               author_table = users, author_pk = id, author_name = username,
///               max_depth = 5, max_body = 10000)]
/// ```
// Seventeen independent keys, each with its own parse + validation arm, plus
// the convention-derived defaults for the sixteen optional ones.
#[allow(clippy::too_many_lines)]
pub fn parse_commentable_attr(
    attr: &syn::Attribute,
    model_ident: &syn::Ident,
) -> syn::Result<CommentableSpec> {
    use syn::parse::ParseStream;

    let span = attr.path().span_of_attr();

    let mut author_model: Option<syn::Ident> = None;
    let mut pairs: std::collections::HashMap<String, KeyValue> = std::collections::HashMap::new();

    if !matches!(attr.meta, syn::Meta::Path(_)) {
        attr.parse_args_with(|input: ParseStream| {
            while !input.is_empty() {
                let key: syn::Ident = input.parse()?;
                input.parse::<syn::Token![=]>()?;
                let parsed = if input.peek(LitStr) {
                    let lit: LitStr = input.parse()?;
                    KeyValue {
                        value: lit.value(),
                        span: lit.span(),
                        boolean: None,
                        integer: None,
                    }
                } else if input.peek(syn::LitInt) {
                    let lit: syn::LitInt = input.parse()?;
                    KeyValue {
                        value: lit.base10_digits().to_owned(),
                        span: lit.span(),
                        boolean: None,
                        integer: Some(lit.base10_parse::<u64>()?),
                    }
                } else if input.peek(syn::LitBool) {
                    let lit: syn::LitBool = input.parse()?;
                    KeyValue {
                        value: lit.value.to_string(),
                        span: lit.span(),
                        boolean: Some(lit.value),
                        integer: None,
                    }
                } else {
                    let ident: syn::Ident = input.parse()?;
                    if key == "by" {
                        author_model = Some(ident.clone());
                    }
                    KeyValue {
                        value: ident.to_string(),
                        span: ident.span(),
                        boolean: None,
                        integer: None,
                    }
                };
                // A repeat would silently win last-write, so a typo'd
                // `table = a, table = b` would build SQL against the wrong one.
                if let Some(previous) = pairs.insert(key.to_string(), parsed) {
                    let now = &pairs[&key.to_string()].value;
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "duplicate `{key} = ...` in `#[commentable]` (was \
                             `{}`, now `{now}`): each key may be given at most \
                             once — the later value would silently win",
                            previous.value
                        ),
                    ));
                }
                if !input.is_empty() {
                    input.parse::<syn::Token![,]>()?;
                }
            }
            Ok(())
        })?;
    }

    for (key, parsed) in &pairs {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(syn::Error::new(
                parsed.span,
                format!(
                    "unknown key `{key}` in `#[commentable]`; expected one of: {}",
                    KNOWN_KEYS.join(", ")
                ),
            ));
        }
    }

    // Every name-shaped value reaches generated SQL, so validate each one at
    // the point it is read rather than trusting the loop above.
    let ident_value = |key: &str, default: &str| -> syn::Result<String> {
        match pairs.get(key) {
            None => Ok(default.to_owned()),
            Some(parsed) => {
                let key_ident = syn::Ident::new(key, parsed.span);
                check_ident_value(&key_ident, &parsed.value, parsed.span)?;
                Ok(parsed.value.clone())
            }
        }
    };

    // `by` is what supplies the author table a display-name lookup joins
    // against. Without it (and without an explicit `author_table`) there is
    // nothing to join, so `author_name` has no meaning — say so rather than
    // silently rendering `user #id` forever.
    let author_table_default = author_model
        .as_ref()
        .map_or_else(String::new, infer_table_name);
    if let Some(parsed) = pairs.get("author_name")
        && author_table_default.is_empty()
        && !pairs.contains_key("author_table")
    {
        return Err(syn::Error::new(
            parsed.span,
            "`author_name` in `#[commentable]` needs a table to read the name \
             from: add `by = <AuthorModel>` (whose table is derived by \
             convention) or `author_table = <table>`",
        ));
    }
    let counter_column = match pairs.get("counter_cache") {
        Some(parsed) if parsed.boolean == Some(false) => None,
        Some(parsed) if parsed.boolean == Some(true) => Some("comment_count".to_owned()),
        Some(parsed) => {
            let key_ident = syn::Ident::new("counter_cache", parsed.span);
            check_ident_value(&key_ident, &parsed.value, parsed.span)?;
            Some(parsed.value.clone())
        }
        None => Some("comment_count".to_owned()),
    };
    let author_name_column = match pairs.get("author_name") {
        None => None,
        Some(parsed) => {
            let key_ident = syn::Ident::new("author_name", parsed.span);
            check_ident_value(&key_ident, &parsed.value, parsed.span)?;
            Some(parsed.value.clone())
        }
    };
    let soft_delete = match pairs.get("soft_delete") {
        None => true,
        Some(parsed) => parsed.boolean.ok_or_else(|| {
            syn::Error::new(
                parsed.span,
                "`soft_delete` in `#[commentable]` takes `true` or `false`",
            )
        })?,
    };
    let max_depth = match pairs.get("max_depth") {
        None => 5_u32,
        Some(parsed) => {
            let raw = parsed.integer.ok_or_else(|| {
                syn::Error::new(
                    parsed.span,
                    "`max_depth` in `#[commentable]` takes an integer",
                )
            })?;
            let depth = u32::try_from(raw).map_err(|_| {
                syn::Error::new(parsed.span, "`max_depth` in `#[commentable]` is too large")
            })?;
            // The runtime measures depth with a `WITH RECURSIVE` CTE that stops
            // at 1000. A `max_depth` at or above that could never be enforced —
            // the probe would report the guard, not the real depth — so the cap
            // would silently stop existing. Keep the two in step here, where
            // the mistake has a span.
            if depth >= MAX_DEPTH_CEILING {
                return Err(syn::Error::new(
                    parsed.span,
                    format!(
                        "`max_depth` in `#[commentable]` must be below {MAX_DEPTH_CEILING}: \
                         the runtime's depth probe stops recursing there, so a larger cap \
                         could not be enforced"
                    ),
                ));
            }
            depth
        }
    };
    let max_body_bytes = match pairs.get("max_body") {
        None => 10_000_usize,
        Some(parsed) => {
            let raw = parsed.integer.ok_or_else(|| {
                syn::Error::new(
                    parsed.span,
                    "`max_body` in `#[commentable]` takes an integer (bytes)",
                )
            })?;
            if raw == 0 {
                return Err(syn::Error::new(
                    parsed.span,
                    "`max_body` in `#[commentable]` must be at least 1: a cap of \
                     zero rejects every comment",
                ));
            }
            usize::try_from(raw).map_err(|_| {
                syn::Error::new(parsed.span, "`max_body` in `#[commentable]` is too large")
            })?
        }
    };
    // `type_name` alone is free-form (it is a bound *value*, never spliced into
    // SQL), but an empty discriminator would make every commentable model's
    // rows indistinguishable, so it still has to say something.
    let type_name = match pairs.get("type_name") {
        None => model_ident.to_string(),
        Some(parsed) if parsed.value.trim().is_empty() => {
            return Err(syn::Error::new(
                parsed.span,
                "`type_name` in `#[commentable]` must not be empty: it is the \
                 discriminator stored in `commentable_type`, and an empty one \
                 cannot tell two models' comments apart",
            ));
        }
        Some(parsed) => parsed.value.clone(),
    };

    Ok(CommentableSpec {
        author_model,
        type_name,
        table: ident_value("table", "comments")?,
        comment_pk: ident_value("comment_pk", "id")?,
        type_column: ident_value("type_column", "commentable_type")?,
        id_column: ident_value("id_column", "commentable_id")?,
        parent_column: ident_value("parent_column", "parent_id")?,
        author_column: ident_value("author_column", "author_id")?,
        body_column: ident_value("body_column", "body")?,
        created_at_column: ident_value("created_at_column", "created_at")?,
        soft_delete,
        counter_column,
        author_table: ident_value("author_table", &author_table_default)?,
        author_pk: ident_value("author_pk", "id")?,
        author_name_column,
        max_depth,
        max_body_bytes,
        span,
    })
}

/// Resolve the (at most one) `#[commentable]` declaration on a model's outer
/// attributes.
///
/// # Errors
///
/// Returns a [`syn::Error`] when more than one `#[commentable]` is declared —
/// both would generate the same `{Model}Comments` trait — or when the single
/// declaration fails [`parse_commentable_attr`]'s validation.
pub fn resolve_commentable(
    model_ident: &syn::Ident,
    attrs: &[syn::Attribute],
) -> syn::Result<Option<CommentableSpec>> {
    let mut found: Option<&syn::Attribute> = None;
    for attr in attrs {
        if !is_commentable_attr(attr) {
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "at most one `#[commentable]` per model: the generated \
                 `{Model}Comments` trait's `add_comment`/`comment_thread`/\
                 `delete_comment` methods would otherwise collide (several \
                 comment kinds per model are not supported — see \
                 https://github.com/autumn-foundation/autumn/issues/1367)",
            ));
        }
        found = Some(attr);
    }
    found
        .map(|attr| parse_commentable_attr(attr, model_ident))
        .transpose()
}

/// Emit everything a `#[commentable]` declaration generates.
///
/// `pk_ident` is the model's primary-key field, resolved by the caller exactly
/// as the CRUD codegen resolves it — it becomes `parent_pk` in the spec, so a
/// model whose `#[id]` is not named `id` still gets a correct parent probe.
///
/// `has_deleted_at` / `has_tenant_id` mirror `#[votable]`: the parent's
/// soft-delete and tenant columns are *projected into the spec* rather than
/// assumed, so a model without them emits SQL byte-for-byte identical to what
/// it would emit if the features did not exist.
// One straight-line assembly of the generated items and their rendered docs,
// mirroring `model::emit_votable_items`. Splitting it would mean threading a
// dozen `TokenStream` fragments through helper signatures for no gain.
#[allow(clippy::too_many_lines)]
pub fn emit_commentable_items(
    model_ident: &syn::Ident,
    vis: &syn::Visibility,
    spec: &CommentableSpec,
    parent_table: &str,
    has_deleted_at: bool,
    has_tenant_id: bool,
    pk_ident: Option<&syn::Ident>,
) -> TokenStream {
    let model_snake = pascal_to_snake(&model_ident.to_string());
    let spec_static = format_ident!("__AUTUMN_COMMENTABLE_SPEC_{}", model_snake.to_uppercase());
    let trait_ident = format_ident!("{model_ident}Comments");
    let type_name = &spec.type_name;

    let comments_table = &spec.table;
    let comment_pk = &spec.comment_pk;
    let type_column = &spec.type_column;
    let id_column = &spec.id_column;
    let parent_column = &spec.parent_column;
    let author_column = &spec.author_column;
    let body_column = &spec.body_column;
    let created_at_column = &spec.created_at_column;
    let soft_delete = spec.soft_delete;
    let parent_pk = pk_ident.map_or_else(|| "id".to_owned(), std::string::ToString::to_string);
    let max_depth = spec.max_depth;
    let max_body_bytes = spec.max_body_bytes;

    let counter_column = spec.counter_column.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |column| quote! { ::core::option::Option::Some(#column) },
    );
    let parent_tenant_column = if has_tenant_id {
        quote! { ::core::option::Option::Some("tenant_id") }
    } else {
        quote! { ::core::option::Option::None }
    };
    // The author table is only ever consulted to resolve a display name, so it
    // is projected together with the column: naming a table but no column would
    // emit a join whose selected expression is `NULL`.
    let (author_table, author_name_column) = spec.author_name_column.as_ref().map_or_else(
        || {
            (
                quote! { ::core::option::Option::None },
                quote! { ::core::option::Option::None },
            )
        },
        |column| {
            let table = &spec.author_table;
            (
                quote! { ::core::option::Option::Some(#table) },
                quote! { ::core::option::Option::Some(#column) },
            )
        },
    );
    let author_pk = &spec.author_pk;

    // ── Compile-time guards ──────────────────────────────────────────────
    // The whole surface is typed on `i64` ids: `add_comment(parent_id: i64,
    // author_id: i64, …)` binds them directly, and the spec's `parent_pk`
    // addresses a `BIGINT`. A UUID- or i32-keyed model would otherwise compile
    // the trait fine and only fail as a database type error on first use.
    let pk_guard = pk_ident.map(|pk| {
        quote! {
            const _: fn(&#model_ident) -> i64 = |__autumn_commentable_model| {
                __autumn_commentable_model.#pk
            };
        }
    });
    // `by = <Author>` is otherwise never mentioned in the generated code (the
    // comments table stores a bare `i64` author fk), so a typo'd model name
    // would compile silently. Force its name resolution, exactly as
    // `#[votable]`'s `by` does — and deliberately name-resolution only, since
    // `by` accepts hand-written author structs that implement no framework
    // trait. Omitting `by` is legitimate (a project whose author model lives
    // outside this crate, or which renders no names), and then there is
    // nothing to resolve.
    // The counter is read back as `i64` and moved with `SET c = c + 1`, so
    // anything else is a schema mismatch. `model.rs` rejects the spellings that
    // are definitely wrong with a directed message; this is the guard that
    // catches the rest — an alias, a fully-qualified path — by name resolution
    // rather than token text, exactly as `#[votable]` does for its aggregate.
    let counter_guard = spec.counter_column.as_ref().map(|column| {
        let counter_ident = format_ident!("{column}");
        quote! {
            const _: fn(&#model_ident) -> i64 = |__autumn_commentable_model| {
                __autumn_commentable_model.#counter_ident
            };
        }
    });
    let author_guard = spec.author_model.as_ref().map(|author_model| {
        quote! {
            const _: ::core::marker::PhantomData<#author_model> = ::core::marker::PhantomData;
        }
    });
    let author_model_name = spec
        .author_model
        .as_ref()
        .map_or_else(|| "<unset>".to_owned(), ToString::to_string);

    let spec_doc = format!(
        "The polymorphic comment binding for `{model_ident}`: the shared \
         `{comments_table}` table keyed on `({type_column}, {id_column})` with \
         `{parent_column}` threading, attached to `{parent_table}`."
    );
    let type_doc = format!(
        "The discriminator this model stores in `{comments_table}.{type_column}`.\n\
         \n\
         Renaming the Rust type changes this value, which orphans existing \
         comment rows — pin it with `#[commentable(type_name = \"…\")]` before \
         renaming a model that already has comments in production."
    );
    let spec_fn_doc = format!(
        "This model's [`CommentableSpec`](autumn_web::commentable::CommentableSpec).\n\
         \n\
         An **inherent** associated function, so no trait import is needed at \
         the call site. Emitted by `#[commentable]`; the same static backs this \
         model's registry entry, so `commentable_spec_for({type_name:?})` \
         returns the very same reference."
    );
    let trait_doc = format!(
        "Threaded, polymorphic comment helpers for `{model_ident}`'s \
         `#[commentable(by = {author_model_name})]` declaration: the shared \
         `{comments_table}` table, discriminated as `{type_name:?}`."
    );
    let counter_note = spec.counter_column.as_ref().map_or_else(
        || "This model keeps no comment counter, so no counter statement is issued.".to_owned(),
        |column| {
            format!(
                "`{parent_table}.{column}` is incremented in the **same** \
                 transaction, with the counter-cache mechanism's atomic \
                 `SET {column} = {column} + 1`."
            )
        },
    );
    let tenant_note = if has_tenant_id {
        "This model has a `tenant_id` column, so a `tenant_scoped` repository \
         matches the parent on it too: another tenant's `parent_id` is \
         `NotFound` before anything is written. `across_tenants()` opts out; a \
         `tenant_scoped` repository with no tenant context is an error.\n\n"
    } else {
        ""
    };
    let add_doc = format!(
        "Post a comment on this record, optionally as a reply to `reply_to`.\n\
         \n\
         {counter_note}\n\
         \n\
         {tenant_note}\
         Runs on its **own** pooled connection — it does not join an enclosing \
         `Db::tx`. Do not hold a `Db` extractor across this call on a small \
         pool.\n\
         \n\
         # Errors\n\
         \n\
         - `422` when the body is blank or over the {max_body_bytes}-byte cap, \
         when `reply_to` is not a live comment on **this** record, or when the \
         reply would nest deeper than `max_depth` ({max_depth}).\n\
         - `404` when this record does not exist (or is soft-deleted).\n\
         - Any database error."
    );
    let thread_doc = format!(
        "This record's live comment thread, replies nested under their parent \
         in stable creation order.\n\
         \n\
         One query for the comments plus one parent-visibility probe, whatever \
         the depth — the tree is assembled in Rust, never with an N+1 walk. \
         Soft-deleted comments are filtered out.\n\
         \n\
         {tenant_note}\
         # Errors\n\
         \n\
         - `404` when this record does not exist (or is soft-deleted).\n\
         - Any database error."
    );
    let recompute_doc = format!(
        "Rebuild this record's comment counter from the comments table, and \
         return the value written.\n\
         \n\
         The repair half of the counter: counters drift when rows arrive by \
         import, seed, or hand-written SQL, and they are deliberately not \
         clamped, so a drifted one can go negative. Idempotent — running it \
         twice writes the same number. Returns `0`, having written nothing, for \
         a model that keeps no counter.\n\
         \n\
         Do **not** reach for `counter_cache_recompute` instead: it keys on the \
         foreign-key column alone, and `commentable_id` is shared across \
         models, so it would count another model's comments that happen to \
         share the id.\n\
         \n\
         # Errors\n\
         \n\
         - `404` when this record does not exist (or is soft-deleted).\n\
         - Any database error."
    );
    let delete_doc = format!(
        "Delete `comment_id` **and every reply beneath it**, returning how many \
         comments were removed.\n\
         \n\
         `parent_id` is part of the lookup, not decoration: without it any \
         comment id would be deletable from any record of this model — the \
         mirror image of the cross-record `reply_to` graft `add_comment` \
         refuses.\n\
         \n\
         {counter_note} The decrement is the number of rows the cascade \
         actually removed, so a repeat delete moves nothing.\n\
         \n\
         # Errors\n\
         \n\
         - `404` when `comment_id` is not a comment on **this** record, or the \
         record is not visible.\n\
         - Any database error."
    );

    // The tenant scope call is emitted UNCONDITIONALLY — even for a model with
    // no `tenant_id` column, where the value is discarded — because the method
    // also carries the cross-shard reject: on a sharded repository in
    // `across_tenants()` mode there is no single right shard for a comment to
    // land on, and the guard errors before any connection is acquired.
    let (resolve_tenant, tenant_arg) = if has_tenant_id {
        (
            quote! {
                let __tenant: ::core::option::Option<::std::string::String> =
                    self.__autumn_m2m_tenant_scope()?;
            },
            quote! { __tenant.as_deref() },
        )
    } else {
        (
            quote! {
                let _: ::core::option::Option<::std::string::String> =
                    self.__autumn_m2m_tenant_scope()?;
            },
            quote! { ::core::option::Option::None },
        )
    };

    quote! {
        #pk_guard
        #counter_guard
        #author_guard

        #[doc = #spec_doc]
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #spec_static: ::autumn_web::commentable::CommentableSpec =
            ::autumn_web::commentable::CommentableSpec {
                comments_table: #comments_table,
                comment_pk: #comment_pk,
                type_column: #type_column,
                id_column: #id_column,
                parent_column: #parent_column,
                author_column: #author_column,
                body_column: #body_column,
                created_at_column: #created_at_column,
                soft_delete: #soft_delete,
                parent_table: #parent_table,
                parent_pk: #parent_pk,
                parent_soft_delete: #has_deleted_at,
                counter_column: #counter_column,
                parent_tenant_column: #parent_tenant_column,
                author_table: #author_table,
                author_pk: #author_pk,
                author_name_column: #author_name_column,
                max_depth: #max_depth,
                max_body_bytes: #max_body_bytes,
            };

        // Registered so the framework's generic comment router can serve THIS
        // model without the app naming it a second time — the whole of AC5.
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::commentable::CommentableDescriptor {
                type_name: #type_name,
                spec: &#spec_static,
            }
        }

        impl #model_ident {
            #[doc = #type_doc]
            pub const COMMENTABLE_TYPE: &'static str = #type_name;

            #[doc = #spec_fn_doc]
            #[must_use]
            pub fn commentable_spec() -> &'static ::autumn_web::commentable::CommentableSpec {
                &#spec_static
            }
        }

        #[doc = #trait_doc]
        #vis trait #trait_ident {
            #[doc = #add_doc]
            fn add_comment(
                &self,
                parent_id: i64,
                author_id: i64,
                body: &str,
                reply_to: ::core::option::Option<i64>,
            ) -> impl ::std::future::Future<
                Output = ::autumn_web::AutumnResult<::autumn_web::commentable::Comment>
            > + Send;

            #[doc = #thread_doc]
            fn comment_thread(
                &self,
                parent_id: i64,
            ) -> impl ::std::future::Future<
                Output = ::autumn_web::AutumnResult<
                    ::std::vec::Vec<::autumn_web::commentable::CommentNode>
                >
            > + Send;

            #[doc = #delete_doc]
            fn delete_comment(
                &self,
                parent_id: i64,
                comment_id: i64,
            ) -> impl ::std::future::Future<
                Output = ::autumn_web::AutumnResult<usize>
            > + Send;

            #[doc = #recompute_doc]
            fn recompute_comment_count(
                &self,
                parent_id: i64,
            ) -> impl ::std::future::Future<
                Output = ::autumn_web::AutumnResult<i64>
            > + Send;
        }

        impl<__R> #trait_ident for __R
        where
            __R: ::autumn_web::repository::M2mConnSource<Model = #model_ident>
                + ::core::marker::Sync,
        {
            async fn add_comment(
                &self,
                parent_id: i64,
                author_id: i64,
                body: &str,
                reply_to: ::core::option::Option<i64>,
            ) -> ::autumn_web::AutumnResult<::autumn_web::commentable::Comment> {
                // Resolved before the connection is taken, so a tenant_scoped
                // repository with no tenant context fails closed without
                // occupying a pooled connection.
                #resolve_tenant
                let mut conn = self.__autumn_m2m_write_conn().await?;
                ::autumn_web::commentable::add_comment(
                    &mut *conn,
                    &#spec_static,
                    #type_name,
                    parent_id,
                    author_id,
                    body,
                    reply_to,
                    #tenant_arg,
                )
                .await
            }

            async fn comment_thread(
                &self,
                parent_id: i64,
            ) -> ::autumn_web::AutumnResult<
                ::std::vec::Vec<::autumn_web::commentable::CommentNode>
            > {
                #resolve_tenant
                // A read: routed per the repository's `ReadRoute`, and it does
                // not mark the read-your-writes pin.
                let mut conn = self.__autumn_m2m_read_conn().await?;
                ::autumn_web::commentable::comment_thread(
                    &mut *conn,
                    &#spec_static,
                    #type_name,
                    parent_id,
                    #tenant_arg,
                )
                .await
            }

            async fn delete_comment(
                &self,
                parent_id: i64,
                comment_id: i64,
            ) -> ::autumn_web::AutumnResult<usize> {
                #resolve_tenant
                let mut conn = self.__autumn_m2m_write_conn().await?;
                ::autumn_web::commentable::delete_comment(
                    &mut *conn,
                    &#spec_static,
                    #type_name,
                    parent_id,
                    comment_id,
                    #tenant_arg,
                )
                .await
            }

            async fn recompute_comment_count(
                &self,
                parent_id: i64,
            ) -> ::autumn_web::AutumnResult<i64> {
                #resolve_tenant
                let mut conn = self.__autumn_m2m_write_conn().await?;
                ::autumn_web::commentable::recompute_comment_count(
                    &mut *conn,
                    &#spec_static,
                    #type_name,
                    parent_id,
                    #tenant_arg,
                )
                .await
            }
        }
    }
}

/// `syn::Path::span()` without pulling `syn::spanned::Spanned` into every
/// caller's scope.
trait SpanOfAttr {
    fn span_of_attr(&self) -> proc_macro2::Span;
}

impl SpanOfAttr for syn::Path {
    fn span_of_attr(&self) -> proc_macro2::Span {
        use syn::spanned::Spanned as _;
        self.span()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens as _;

    /// Parse one `#[commentable(...)]` attribute written on `Post`.
    fn parse(attr: &proc_macro2::TokenStream) -> syn::Result<CommentableSpec> {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attr: syn::Attribute = syn::parse_quote!(#[commentable #attr]);
        parse_commentable_attr(&attr, &model)
    }

    fn error(attr: &proc_macro2::TokenStream) -> String {
        parse(attr).expect_err("expected a rejection").to_string()
    }

    #[test]
    fn a_bare_attribute_takes_every_convention() {
        let spec = parse(&quote! {}).expect("`#[commentable]` alone is valid");
        assert!(spec.author_model.is_none());
        assert_eq!(spec.type_name, "Post");
        assert_eq!(spec.table, "comments");
        assert_eq!(spec.type_column, "commentable_type");
        assert_eq!(spec.id_column, "commentable_id");
        assert_eq!(spec.parent_column, "parent_id");
        assert_eq!(spec.author_column, "author_id");
        assert_eq!(spec.body_column, "body");
        assert_eq!(spec.created_at_column, "created_at");
        assert_eq!(spec.comment_pk, "id");
        assert_eq!(spec.author_pk, "id");
        assert!(spec.soft_delete);
        assert_eq!(spec.counter_column.as_deref(), Some("comment_count"));
        assert_eq!(spec.author_name_column, None);
        assert_eq!(spec.max_depth, 5);
        assert_eq!(spec.max_body_bytes, 10_000);
    }

    /// `by` supplies the author table by the same pluralize-the-snake-case
    /// convention every other association uses.
    #[test]
    fn by_derives_the_author_table() {
        let spec = parse(&quote! { (by = AppUser, author_name = username) }).expect("valid");
        assert_eq!(
            spec.author_model
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("AppUser")
        );
        assert_eq!(spec.author_table, "app_users");
        assert_eq!(spec.author_name_column.as_deref(), Some("username"));
    }

    #[test]
    fn every_key_can_be_overridden() {
        let spec = parse(&quote! {(
            by = User,
            type_name = "LegacyPost",
            table = discussion,
            comment_pk = pk,
            type_column = kind,
            id_column = target_id,
            parent_column = reply_to,
            author_column = writer_id,
            body_column = text,
            created_at_column = posted_at,
            soft_delete = false,
            counter_cache = discussion_count,
            author_table = accounts,
            author_pk = account_id,
            author_name = handle,
            max_depth = 2,
            max_body = 500,
        )})
        .expect("valid");
        assert_eq!(spec.type_name, "LegacyPost");
        assert_eq!(spec.table, "discussion");
        assert_eq!(spec.comment_pk, "pk");
        assert_eq!(spec.type_column, "kind");
        assert_eq!(spec.id_column, "target_id");
        assert_eq!(spec.parent_column, "reply_to");
        assert_eq!(spec.author_column, "writer_id");
        assert_eq!(spec.body_column, "text");
        assert_eq!(spec.created_at_column, "posted_at");
        assert!(!spec.soft_delete);
        assert_eq!(spec.counter_column.as_deref(), Some("discussion_count"));
        assert_eq!(spec.author_table, "accounts");
        assert_eq!(spec.author_pk, "account_id");
        assert_eq!(spec.author_name_column.as_deref(), Some("handle"));
        assert_eq!(spec.max_depth, 2);
        assert_eq!(spec.max_body_bytes, 500);
    }

    #[test]
    fn counter_cache_false_opts_out_of_the_counter() {
        assert_eq!(
            parse(&quote! { (counter_cache = false) })
                .expect("valid")
                .counter_column,
            None
        );
        // `true` is the same as omitting it, rather than a parse error — the
        // spelling reads naturally next to `counter_cache = false`.
        assert_eq!(
            parse(&quote! { (counter_cache = true) })
                .expect("valid")
                .counter_column
                .as_deref(),
            Some("comment_count")
        );
    }

    /// A repeated key would silently win last-write, so a typo'd
    /// `table = a, table = b` would build SQL against the wrong one.
    #[test]
    fn a_repeated_key_is_rejected() {
        let message = error(&quote! { (table = a, table = b) });
        assert!(message.contains("duplicate `table = ...`"), "{message}");
    }

    #[test]
    fn an_unknown_key_lists_the_ones_that_exist() {
        let message = error(&quote! { (autor_name = username) });
        assert!(message.contains("unknown key `autor_name`"), "{message}");
        assert!(message.contains("author_name"), "{message}");
    }

    /// Every name-shaped value is spliced verbatim into generated SQL, so a
    /// non-identifier must never get that far.
    #[test]
    fn a_non_identifier_value_is_rejected_before_it_reaches_sql() {
        for attr in [
            quote! { (table = "comments\"; DROP TABLE posts --") },
            quote! { (counter_cache = "1bad") },
            quote! { (by = User, author_name = "") },
            quote! { (body_column = "has space") },
        ] {
            let message = error(&attr);
            assert!(
                message.contains("is not a valid identifier"),
                "expected an identifier rejection, got: {message}"
            );
        }
    }

    /// An empty discriminator cannot tell two models' comments apart.
    #[test]
    fn an_empty_type_name_is_rejected() {
        let message = error(&quote! { (type_name = "  ") });
        assert!(message.contains("must not be empty"), "{message}");
    }

    /// Nothing joins to a display name without a table to read it from.
    #[test]
    fn author_name_without_a_table_is_rejected() {
        let message = error(&quote! { (author_name = username) });
        assert!(message.contains("needs a table"), "{message}");
        // …and either way of supplying one is accepted.
        assert!(parse(&quote! { (by = User, author_name = username) }).is_ok());
        assert!(parse(&quote! { (author_table = users, author_name = username) }).is_ok());
    }

    #[test]
    fn typed_keys_reject_the_wrong_literal_kind() {
        assert!(
            error(&quote! { (soft_delete = yes) }).contains("takes `true` or `false`"),
            "soft_delete must be a bool"
        );
        assert!(
            error(&quote! { (max_depth = deep) }).contains("takes an integer"),
            "max_depth must be an integer"
        );
        assert!(
            error(&quote! { (max_body = big) }).contains("takes an integer"),
            "max_body must be an integer"
        );
    }

    /// A cap of zero rejects every comment, which is never what anyone means.
    #[test]
    fn a_zero_max_body_is_rejected() {
        let message = error(&quote! { (max_body = 0) });
        assert!(message.contains("must be at least 1"), "{message}");
    }

    /// A second declaration would generate the same `{Model}Comments` trait.
    #[test]
    fn at_most_one_commentable_per_model() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[commentable(by = User)]),
            syn::parse_quote!(#[commentable(by = User, table = other)]),
        ];
        let message = resolve_commentable(&model, &attrs)
            .expect_err("a second declaration must be rejected")
            .to_string();
        assert!(
            message.contains("at most one `#[commentable]`"),
            "{message}"
        );

        // One is fine; none resolves to `None` and emits nothing at all.
        assert!(
            resolve_commentable(&model, &attrs[..1])
                .expect("one is fine")
                .is_some()
        );
        assert!(
            resolve_commentable(&model, &[])
                .expect("none is fine")
                .is_none()
        );
    }

    /// The emitted surface is what the runtime and the router bind to, so the
    /// names are contract rather than cosmetics.
    #[test]
    fn the_emitted_items_carry_the_documented_names() {
        let spec = parse(&quote! { (by = User, author_name = username) }).expect("valid");
        let model: syn::Ident = syn::parse_quote!(Post);
        let vis: syn::Visibility = syn::parse_quote!(pub);
        let pk: syn::Ident = syn::parse_quote!(id);
        let emitted = emit_commentable_items(&model, &vis, &spec, "posts", false, false, Some(&pk))
            .to_token_stream()
            .to_string();

        assert!(emitted.contains("PostComments"), "the trait name");
        assert!(emitted.contains("COMMENTABLE_TYPE"));
        assert!(emitted.contains("commentable_spec"));
        assert!(emitted.contains("CommentableDescriptor"), "registry entry");
        assert!(emitted.contains("add_comment"));
        assert!(emitted.contains("comment_thread"));
        assert!(emitted.contains("delete_comment"));
        // The author guard is what turns a typo'd `by` into a name-resolution
        // error rather than silence.
        assert!(emitted.contains("PhantomData"));
    }

    /// A model with neither a tenant nor a soft-delete column must emit no
    /// trace of either — that zero-cost path is the whole reason the spec
    /// projects them rather than assuming them.
    #[test]
    fn a_plain_model_emits_no_tenant_or_soft_delete_machinery() {
        let spec = parse(&quote! { (by = User) }).expect("valid");
        let model: syn::Ident = syn::parse_quote!(Post);
        let vis: syn::Visibility = syn::parse_quote!(pub);
        let pk: syn::Ident = syn::parse_quote!(id);

        let plain = emit_commentable_items(&model, &vis, &spec, "posts", false, false, Some(&pk))
            .to_token_stream()
            .to_string();
        assert!(plain.contains("parent_soft_delete : false"), "{plain}");
        assert!(
            plain.contains("parent_tenant_column : :: core :: option :: Option :: None"),
            "{plain}"
        );

        let tenanted = emit_commentable_items(&model, &vis, &spec, "posts", true, true, Some(&pk))
            .to_token_stream()
            .to_string();
        assert!(tenanted.contains("parent_soft_delete : true"), "{tenanted}");
        assert!(tenanted.contains("\"tenant_id\""), "{tenanted}");
    }
}
