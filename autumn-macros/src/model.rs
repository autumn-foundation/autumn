//! `#[model]` attribute macro implementation.
//!
//! Generates four types from a single struct:
//! - The query type (original struct) with `Queryable`, `Selectable`
//! - A `NewX` insert type with `Insertable` (ID fields excluded)
//! - An `UpdateX` patch type with `Default` (ID fields excluded, all `Patch<T>`)
//! - A `XField` enum with one variant per mutable field (for audit/CDC payloads)
//!
//! Also generates on `UpdateDraft<Model>`:
//! - `from_patch(current, patch)` — merges a `Patch`-based update into a draft
//! - Per-field `DraftField` accessor methods for inspecting/overriding changes
//!
//! Recognises `#[id]`, `#[indexed]`, and `#[validate(...)]` field attributes.

use proc_macro2::TokenStream;
use quote::{ToTokens as _, format_ident, quote};
use syn::parse::Parser as _;
use syn::{DeriveInput, Field, LitStr};

use crate::commentable::{emit_commentable_items, is_commentable_attr, resolve_commentable};
use crate::schema::{
    apply_serde_rename_all_rule, emit_schema_fn_body, emit_schema_fn_body_ext,
    field_is_translatable, field_serde_serialize_rename, has_attr, is_option_type,
    serde_rename_all_serialize_rule, type_name_str,
};

/// Parsed `#[model(...)]` attribute arguments.
///
/// `managed` is a declarative-schema opt-in marker (#1975, Decision 4). It is
/// accepted and validated here, but has **zero effect on generated code** —
/// the marker is schema-authoritative for the CLI toolchain only; the codegen
/// side is wired in a later slice.
#[derive(Default, Debug)]
struct ModelArgs {
    /// Explicit `table = "..."` override, if any.
    table: Option<String>,
    /// Whether the model opted into declarative-schema management via
    /// `#[model(managed)]`. Captured but intentionally unused by codegen.
    #[allow(dead_code)]
    managed: bool,
}

/// Process `#[model]`, `#[model(table = "...")]`, and `#[model(managed)]`
/// attribute arguments.
fn parse_attr_args(attr: TokenStream) -> syn::Result<ModelArgs> {
    let mut args = ModelArgs::default();
    if attr.is_empty() {
        return Ok(args);
    }

    syn::meta::parser(|meta| {
        if meta.path.is_ident("table") {
            let value: LitStr = meta.value()?.parse()?;
            args.table = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("managed") {
            // The required form is a bare `managed` path. Both `managed = <expr>`
            // and `managed(...)` are rejected with a clear message rather than
            // falling through to a generic `syn` parser error.
            if meta.input.peek(syn::Token![=]) || meta.input.peek(syn::token::Paren) {
                return Err(
                    meta.error("`managed` takes no value; write a bare `#[model(managed)]`")
                );
            }
            args.managed = true;
            Ok(())
        } else {
            Err(meta.error(
                "unsupported `#[model(...)]` argument; expected `table = \"...\"` or `managed`",
            ))
        }
    })
    .parse2(attr)?;

    Ok(args)
}

/// Validate the declarative-schema field markers `#[unique]` and
/// `#[references(...)]` (#1975).
///
/// These markers are schema-authoritative for the CLI toolchain (the slice-2
/// syn parser reads them from source text); the `#[model]` macro only needs to
/// **accept** them and reject malformed shapes with a clear, actionable error.
/// No FK/index codegen is produced here — the markers are validated then
/// stripped by [`user_attrs`], leaving generated code byte-for-byte unchanged.
///
/// Accepted shapes (mirroring `autumn-cli`'s `schema::parse`):
/// - `#[unique]` — a bare marker; any argument (`#[unique(...)]` / `#[unique =
///   ...]`) is an error.
/// - `#[references]` — bare; the target table is inferred from the field name.
/// - `#[references(table = "other_table")]` — an explicit target table.
fn validate_field_schema_markers(field: &Field) -> syn::Result<()> {
    for attr in &field.attrs {
        if attr.path().is_ident("unique") {
            if !matches!(attr.meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    "`#[unique]` takes no arguments; write a bare `#[unique]`",
                ));
            }
        } else if attr.path().is_ident("position") {
            // Bare marker only (issue #1358): the column name and optional
            // scope live in the generated `#[repository(..., position(column
            // = "...", scope = "..."))]` attribute, not here — this marker's
            // only job is excluding the field from `New{Model}`/`Update{Model}`
            // (see `excluded_from_new`), the same way `#[lock_version]` does.
            if !matches!(attr.meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    "`#[position]` takes no arguments; write a bare `#[position]`",
                ));
            }
        } else if attr.path().is_ident("references") {
            // Bare `#[references]` is valid (target inferred from field name).
            if matches!(attr.meta, syn::Meta::Path(_)) {
                continue;
            }
            // Only the list form `#[references(...)]` carries arguments. A
            // name-value shape like `#[references = "accounts"]` would make
            // `parse_nested_meta` fail with a generic "expected attribute list"
            // error, so reject it here with an actionable message.
            if !matches!(attr.meta, syn::Meta::List(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    "`#[references]` must be a bare attribute or have the form `#[references(table = \"...\")]`",
                ));
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("table") {
                    let _value: LitStr = meta.value()?.parse()?;
                    Ok(())
                } else {
                    Err(meta.error(
                        "unsupported `#[references(...)]` argument; expected `table = \"...\"`",
                    ))
                }
            })?;
        }
    }
    Ok(())
}

/// The three declarative association kinds supported on `#[model]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AssocKind {
    /// `#[belongs_to(Target, fk = ...)]` — the foreign key lives on *this*
    /// model and points at the target's primary key.
    BelongsTo,
    /// `#[has_many(Target, fk = ...)]` — the foreign key lives on the *target*
    /// and points back at this model's primary key.
    HasMany,
    /// `#[has_one(Target, fk = ...)]` — like `has_many`, but at most one
    /// related record.
    HasOne,
}

/// The `dependent = <action>` / `on_delete = <action>` cascade action declared
/// on a model `#[has_many]` / `#[has_one]` association (#1738). Mirrors the
/// repository-attribute `on_delete` actions one-for-one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DependentAction {
    Destroy,
    DeleteAll,
    Nullify,
    Restrict,
}

impl DependentAction {
    /// Parse the spelling accepted after `dependent =` / `on_delete =`. Returns
    /// `None` for an unknown action so the caller can emit a directed error.
    fn parse(action: &str) -> Option<Self> {
        match action {
            "destroy" => Some(Self::Destroy),
            "delete_all" => Some(Self::DeleteAll),
            "nullify" => Some(Self::Nullify),
            "restrict" => Some(Self::Restrict),
            _ => None,
        }
    }

    /// The `::autumn_web::repository::DependentAction` variant token.
    fn variant_ident(self) -> proc_macro2::Ident {
        let name = match self {
            Self::Destroy => "Destroy",
            Self::DeleteAll => "DeleteAll",
            Self::Nullify => "Nullify",
            Self::Restrict => "Restrict",
        };
        format_ident!("{name}")
    }
}

/// A resolved `counter_cache` declaration on a `#[belongs_to]` (#1325).
///
/// `#[belongs_to(Post, counter_cache)]` is the bare flag; `counter_cache =
/// "comment_count"` names the parent column explicitly.
#[derive(Clone, Debug)]
struct CounterCacheDecl {
    /// The explicitly named counter column, when given. `None` means the
    /// convention applies: `{snake(ChildModel)}_count`.
    ///
    /// The convention is deliberately **singular** (`comment_count`, not Rails'
    /// `comments_count`): it matches `#[votable(aggregate = count)]`'s
    /// `{name}_count` and the `posts.comment_count` /
    /// `subreddits.subscriber_count` columns autumn's own examples have shipped
    /// since their first migration.
    column: Option<String>,
    /// The parent's tenant-discriminator column, from
    /// `counter_cache_tenant = "<column>"`.
    ///
    /// Explicit rather than inferred: a counter update writes a parent row the
    /// caller only had to name the id of, so on a shared multi-tenant table it
    /// must be confined to the caller's tenant — but `#[model]` on the *child*
    /// cannot see the parent's fields, and guessing `tenant_id` would break
    /// every app whose tenant-scoped child hangs off a global parent with a hard
    /// `column does not exist` error. Naming it is the author asserting the
    /// parent really has it.
    tenant_column: Option<String>,
    /// Span of the `counter_cache` key, for diagnostics raised after parsing.
    span: proc_macro2::Span,
}

/// A resolved association declaration: kind, target model, the (possibly
/// inferred) foreign-key column, and the accessor/store name.
struct Association {
    kind: AssocKind,
    target: syn::Ident,
    /// The foreign-key column name. For `belongs_to` it is a column on this
    /// model; for `has_many`/`has_one` it is a column on the target. For a
    /// `through =` (many-to-many) `has_many`, this is the join table's
    /// column pointing back at *this* model (e.g. `post_id`).
    fk: String,
    /// The accessor method name and association store key, e.g. `author`,
    /// `comments`, `subreddit`.
    name: String,
    /// Set for a many-to-many `has_many(Target, through = join_table)`
    /// association: the join table this association is preloaded/mutated
    /// through, plus the join table's column pointing at the target model.
    through: Option<ThroughSpec>,
    /// The `dependent = <action>` / `on_delete = <action>` cascade action, when
    /// declared on a `#[has_many]` / `#[has_one]` (#1738). Drives the runtime
    /// [`AutumnDependents`] impl `#[model]` emits so the parent repository's
    /// `delete_by_id` cascades this association without a repository-attribute
    /// `dependent(…)`.
    dependent: Option<DependentAction>,
    /// Explicit override for the singular used to derive the many-to-many
    /// `add_`/`remove_` mutation helper names (`helper = "follower"` →
    /// `add_follower`/`remove_follower`). When absent, the singular is derived
    /// from the target type name. Distinct overrides let a model declare two
    /// m2m associations to the *same* target type (e.g. `followers` and
    /// `following`, both through `Friendship` to `User`) without their
    /// target-derived helpers colliding.
    helper: Option<String>,
    /// The `counter_cache` declaration on a `#[belongs_to]` (#1325). Drives the
    /// runtime [`AutumnCounterCaches`] impl `#[model]` emits, which the child's
    /// generated repository consults to keep `{parent}.{child}_count` current.
    counter_cache: Option<CounterCacheDecl>,
}

/// The join-table half of a many-to-many `has_many(..., through = ...)`
/// association.
struct ThroughSpec {
    /// The join table name, e.g. `post_tags`.
    table: String,
    /// The join table's column pointing at the target model, e.g. `tag_id`.
    target_fk: String,
}

/// How a `#[votable]` association aggregates its edges into the model's
/// aggregate column (#1362).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoteAggregate {
    /// Signed up/down votes: the aggregate is `SUM(value)` over the edge
    /// table's `value_column`.
    Sum,
    /// Unary likes/bookmarks: the aggregate is `COUNT(*)` and the edge table
    /// carries no value column at all.
    Count,
}

/// A resolved `#[votable(by = <Reactor>, ...)]` declaration (#1362): the
/// reaction edge table plus the aggregate column maintained on the model.
///
/// Every field except `reactor` has a convention-derived default, so the
/// canonical declaration on a conventionally-named schema is the one-liner
/// `#[votable(by = User, aggregate = sum)]` → `votes(user_id, post_id, value)`
/// maintaining `posts.score`.
struct VotableSpec {
    /// The reactor model type named by `by = <Model>` (e.g. `User`).
    reactor: syn::Ident,
    /// `sum` (default) or `count`.
    aggregate: VoteAggregate,
    /// The reaction name, default `"vote"`. Drives `table` (pluralized) and,
    /// in count mode, the `{name}_count` aggregate column.
    name: String,
    /// The edge table, default `pluralize(name)` → `votes` / `likes`.
    table: String,
    /// The edge column pointing at the reactor, default `{snake(by)}_id`.
    reactor_fk: String,
    /// The edge column pointing at this model, default `{snake(Model)}_id`.
    target_fk: String,
    /// The edge's signed value column, default `value` — `None` in count mode,
    /// whose edge rows are pure membership and store no value.
    value_column: Option<String>,
    /// The aggregate column on *this* model, default `score` (sum) /
    /// `{name}_count` (count).
    column: String,
}

/// Reject a `#[votable(key = value)]` value that is not a plain Rust
/// identifier.
///
/// Every name-shaped value in the attribute is spliced into generated code
/// through `format_ident!` — as a table/column ident inside the hidden
/// `diesel::table!` declarations, or (for `name`) as a fragment of the hidden
/// module ident. `format_ident!` **panics** on a non-identifier, and a
/// proc-macro panic reaches the user as an opaque "proc macro panicked" with no
/// span pointing at the mistake. A raw identifier (`r#type`) does not panic but
/// renders its `r#` prefix into the generated name, silently producing a
/// column/table that cannot exist. Both are rejected here, spanned on the
/// offending value.
///
/// # Errors
///
/// Returns a [`syn::Error`] naming the value and the key it was given for.
fn check_votable_ident_value(
    key: &syn::Ident,
    value: &str,
    span: proc_macro2::Span,
) -> syn::Result<()> {
    if !value.starts_with("r#") && syn::parse_str::<syn::Ident>(value).is_ok() {
        return Ok(());
    }
    Err(syn::Error::new(
        span,
        format!(
            "`{value}` is not a valid identifier for `{key} = ...` in \
             `#[votable]`: the value is spliced verbatim into generated table, \
             column and module names, so it must be a plain Rust identifier — \
             no spaces or punctuation, no leading digit, no keyword, and no \
             `r#` prefix"
        ),
    ))
}

/// Parse a single `#[votable(...)]` attribute into a resolved [`VotableSpec`].
///
/// Grammar (a `key = value` loop, each value a bare ident or a string literal;
/// there is deliberately **no** positional head — the reactor is always named,
/// because a bare `#[votable(User)]` reads ambiguously next to
/// `#[has_many(Comment)]`, where the positional argument is the association
/// *target* rather than the actor):
///
/// ```text
/// #[votable(by = User, aggregate = sum | count, name = vote, table = votes,
///           reactor_fk = user_id, target_fk = post_id,
///           value_column = value, column = score)]
/// ```
// Eight independent keys, each with its own parse + validation arm, plus the
// convention-derived defaults for the seven optional ones.
#[allow(clippy::too_many_lines)]
fn parse_votable_attr(attr: &syn::Attribute, model_ident: &syn::Ident) -> syn::Result<VotableSpec> {
    use syn::parse::ParseStream;

    if matches!(attr.meta, syn::Meta::Path(_)) {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[votable]` requires `by = <ReactorModel>` \
             (e.g. `#[votable(by = User)]`)",
        ));
    }

    let mut reactor: Option<syn::Ident> = None;
    let mut aggregate: Option<VoteAggregate> = None;
    let mut name: Option<String> = None;
    let mut table: Option<String> = None;
    let mut reactor_fk: Option<String> = None;
    let mut target_fk: Option<String> = None;
    let mut value_column: Option<syn::Ident> = None;
    let mut value_column_value: Option<String> = None;
    let mut column: Option<String> = None;

    // Every key already seen in *this* attribute, mapped to the value it was
    // given. A repeat would otherwise silently win last-write, so a typo'd
    // `by = A, by = B` would compile against the wrong reactor.
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    attr.parse_args_with(|input: ParseStream| {
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            // Accept either a bare identifier (`table = votes`) or a string
            // literal (`table = "votes"`), exactly like the association attrs.
            let (value_ident, value, value_span) = if input.peek(LitStr) {
                let lit: LitStr = input.parse()?;
                let span = lit.span();
                (None, lit.value(), span)
            } else {
                let ident: syn::Ident = input.parse()?;
                let span = ident.span();
                (Some(ident.clone()), ident.to_string(), span)
            };
            if let Some(previous) = seen.insert(key.to_string(), value.clone()) {
                return Err(syn::Error::new_spanned(
                    &key,
                    format!(
                        "duplicate `{key} = ...` in `#[votable]` (was \
                         `{previous}`, now `{value}`): each key may be given \
                         at most once — the later value would silently win"
                    ),
                ));
            }
            if key == "by" {
                // Every value that becomes part of a generated ident is
                // validated, so a mistake is a directed error rather than a
                // `format_ident!` panic (or, for `r#`, a garbage name).
                check_votable_ident_value(&key, &value, value_span)?;
                reactor = Some(match value_ident {
                    Some(ident) => ident,
                    None => syn::parse_str::<syn::Ident>(&value).map_err(|_| {
                        syn::Error::new_spanned(
                            &key,
                            format!("`by = \"{value}\"` is not a valid model type name"),
                        )
                    })?,
                });
            } else if key == "aggregate" {
                aggregate = Some(match value.as_str() {
                    "sum" => VoteAggregate::Sum,
                    "count" => VoteAggregate::Count,
                    other => {
                        return Err(syn::Error::new_spanned(
                            &key,
                            format!(
                                "unknown aggregate `{other}`; expected `sum` \
                                 (signed up/down votes) or `count` (unary likes)"
                            ),
                        ));
                    }
                });
            } else if key == "name" {
                // `name` feeds `format_ident!` composites (the hidden module
                // ident and the `{name}_count` aggregate column), so it is held
                // to the same identifier rule as the column names.
                check_votable_ident_value(&key, &value, value_span)?;
                name = Some(value);
            } else if key == "table" {
                check_votable_ident_value(&key, &value, value_span)?;
                table = Some(value);
            } else if key == "reactor_fk" {
                check_votable_ident_value(&key, &value, value_span)?;
                reactor_fk = Some(value);
            } else if key == "target_fk" {
                check_votable_ident_value(&key, &value, value_span)?;
                target_fk = Some(value);
            } else if key == "value_column" {
                check_votable_ident_value(&key, &value, value_span)?;
                value_column = Some(key.clone());
                value_column_value = Some(value);
            } else if key == "column" {
                check_votable_ident_value(&key, &value, value_span)?;
                column = Some(value);
            } else {
                return Err(syn::Error::new_spanned(
                    &key,
                    "expected `by = <Model>`, `aggregate = sum|count`, \
                     `name = <ident>`, `table = <table>`, \
                     `reactor_fk = <column>`, `target_fk = <column>`, \
                     `value_column = <column>`, or \
                     `column = <aggregate_column>` in `#[votable]`",
                ));
            }
            if input.peek(syn::Token![,]) {
                input.parse::<syn::Token![,]>()?;
            } else {
                break;
            }
        }
        Ok(())
    })?;

    let Some(reactor) = reactor else {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[votable]` requires `by = <ReactorModel>` \
             (e.g. `#[votable(by = User)]`)",
        ));
    };
    let aggregate = aggregate.unwrap_or(VoteAggregate::Sum);

    if aggregate == VoteAggregate::Count
        && let Some(key) = value_column
    {
        return Err(syn::Error::new_spanned(
            key,
            "`value_column = ...` has no meaning with `aggregate = count`: a \
             count reaction's edge table stores no value column, its rows are \
             pure membership — use `aggregate = sum` (signed values) or drop \
             the key",
        ));
    }

    let name = name.unwrap_or_else(|| "vote".to_owned());
    let table = table.unwrap_or_else(|| pluralize_word(&name));
    let reactor_fk =
        reactor_fk.unwrap_or_else(|| format!("{}_id", pascal_to_snake(&reactor.to_string())));
    let target_fk =
        target_fk.unwrap_or_else(|| format!("{}_id", pascal_to_snake(&model_ident.to_string())));
    let value_column = match aggregate {
        VoteAggregate::Sum => Some(value_column_value.unwrap_or_else(|| "value".to_owned())),
        VoteAggregate::Count => None,
    };
    let column = column.unwrap_or_else(|| match aggregate {
        VoteAggregate::Sum => "score".to_owned(),
        VoteAggregate::Count => format!("{name}_count"),
    });

    Ok(VotableSpec {
        reactor,
        aggregate,
        name,
        table,
        reactor_fk,
        target_fk,
        value_column,
        column,
    })
}

/// Resolve the (at most one) `#[votable]` declaration on a model's outer
/// attributes.
///
/// A second `#[votable]` is a directed compile error rather than a silently
/// dropped declaration: both would generate the same `{Model}Reactions` trait
/// with the same `react`/`reaction_of` methods.
///
/// # Errors
///
/// Returns a [`syn::Error`] when more than one `#[votable]` is declared, or
/// when the single declaration fails [`parse_votable_attr`]'s validation.
fn resolve_votable(
    model_ident: &syn::Ident,
    attrs: &[syn::Attribute],
) -> syn::Result<Option<VotableSpec>> {
    let mut found: Option<&syn::Attribute> = None;
    for attr in attrs {
        if !is_votable_attr(attr) {
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "at most one `#[votable]` per model: the generated \
                 `{Model}Reactions` trait's `react`/`reaction_of` methods would \
                 otherwise collide (multiple reaction kinds per model are not \
                 yet supported — see \
                 https://github.com/autumn-foundation/autumn/issues/1362)",
            ));
        }
        found = Some(attr);
    }
    found
        .map(|attr| parse_votable_attr(attr, model_ident))
        .transpose()
}

/// Whether an attribute is the `#[votable]` declaration consumed by `#[model]`
/// (and therefore must not be re-emitted onto the Diesel struct, where it
/// would fail with "cannot find attribute `votable` in this scope").
fn is_votable_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("votable")
}

/// Resolve the foreign-key column and accessor name for an association,
/// applying autumn's conventions when the `fk` is not given explicitly.
///
/// * `belongs_to(User)` on `Post` → fk `user_id`, name `user`.
/// * `belongs_to(User, fk = author_id)` on `Post` → fk `author_id`, name `author`.
/// * `has_many(Comment)` on `Post` → fk `post_id` (on `Comment`), name `comments`.
/// * `has_one(Profile)` on `User` → fk `user_id` (on `Profile`), name `profile`.
fn resolve_fk_and_name(
    kind: AssocKind,
    model_ident: &syn::Ident,
    target_ident: &syn::Ident,
    explicit_fk: Option<&str>,
) -> (String, String) {
    let snake_target = pascal_to_snake(&target_ident.to_string());
    let snake_source = pascal_to_snake(&model_ident.to_string());
    match kind {
        AssocKind::BelongsTo => {
            let fk = explicit_fk.map_or_else(|| format!("{snake_target}_id"), ToOwned::to_owned);
            let name = fk.strip_suffix("_id").unwrap_or(&fk).to_owned();
            (fk, name)
        }
        AssocKind::HasMany => {
            let fk = explicit_fk.map_or_else(|| format!("{snake_source}_id"), ToOwned::to_owned);
            let name = pluralize_word(&snake_target);
            (fk, name)
        }
        AssocKind::HasOne => {
            let fk = explicit_fk.map_or_else(|| format!("{snake_source}_id"), ToOwned::to_owned);
            (fk, snake_target)
        }
    }
}

/// Parse a single association attribute body, e.g.
/// `User, fk = author_id` or `Post, fk = author_id, name = authored_posts`.
///
/// `name = …` overrides the derived accessor/store name, so multiple
/// associations can target the same model without colliding (e.g.
/// `#[has_many(Post, fk = author_id, name = authored)]` plus
/// `#[has_many(Post, fk = approver_id, name = approved)]`).
/// Resolve a `dependent = <action>` / `on_delete = <action>` key on a model
/// association attribute into a validated [`DependentAction`], or a directed
/// error (#1738).
///
/// On `#[has_many]` / `#[has_one]` the spelling is now wired: `#[model]` emits a
/// runtime [`AutumnDependents`] impl so the parent repository's `delete_by_id`
/// cascades this association (resolving the child repository via the
/// `Pg{Child}Repository` naming convention) — the same transactional cascade
/// the repository-attribute `dependent(PgChildRepository, …)` form produces. An
/// unknown action is still rejected. On `#[belongs_to]` the key remains an
/// error: the child foreign key lives on that side, so there is no dependent to
/// cascade.
fn parse_dependent_action(
    kind: AssocKind,
    key: &syn::Ident,
    action: &str,
) -> syn::Result<DependentAction> {
    if kind == AssocKind::BelongsTo {
        return Err(syn::Error::new_spanned(
            key,
            "`dependent`/`on_delete` is not valid on `#[belongs_to]`: the child \
             foreign key lives on this (belongs_to) side, so there is no \
             dependent to cascade — declare the cascade on the parent's \
             `#[has_many]`/`#[has_one]` instead",
        ));
    }
    DependentAction::parse(action).ok_or_else(|| {
        syn::Error::new_spanned(
            key,
            format!(
                "unknown dependent action `{action}`; expected one of \
                 `destroy`, `delete_all`, `nullify`, `restrict`"
            ),
        )
    })
}

// Accumulates per-feature parsing/validation for each association key (`fk`,
// `name`, `through`, `target_fk`, `dependent`/`on_delete`, `helper`), so it
// grows past the line lint as association options are added.
#[allow(clippy::too_many_lines)]
fn parse_assoc_attr(
    attr: &syn::Attribute,
    kind: AssocKind,
    model_ident: &syn::Ident,
) -> syn::Result<Association> {
    use syn::parse::ParseStream;

    let (
        target,
        explicit_fk,
        explicit_name,
        explicit_through,
        explicit_target_fk,
        dependent,
        explicit_helper,
        counter_cache,
        counter_cache_tenant,
    ) = attr.parse_args_with(|input: ParseStream| {
        let target: syn::Ident = input.parse()?;
        let mut explicit_fk: Option<String> = None;
        let mut explicit_name: Option<String> = None;
        let mut explicit_through: Option<String> = None;
        let mut explicit_target_fk: Option<String> = None;
        // The key's span rides along so a later cross-key rejection (the
        // `through =` combination below) can point its caret at the offending
        // `dependent`/`on_delete` key — the thing the fix removes — rather than
        // at the association's target ident.
        let mut dependent: Option<(DependentAction, proc_macro2::Span)> = None;
        let mut explicit_helper: Option<String> = None;
        let mut counter_cache: Option<CounterCacheDecl> = None;
        let mut counter_cache_tenant: Option<(String, proc_macro2::Span)> = None;
        // Zero or more trailing `, key = value` pairs (`fk`, `name`,
        // `through`, `target_fk`, `helper`), any order — plus `counter_cache`,
        // the one key that is also legal as a bare flag.
        while input.peek(syn::Token![,]) {
            input.parse::<syn::Token![,]>()?;
            let key: syn::Ident = input.parse()?;
            // `counter_cache` may appear bare (`#[belongs_to(Post,
            // counter_cache)]`) or with an explicit column
            // (`counter_cache = "comment_count"`), so its `=` is optional.
            if key == "counter_cache" && !input.peek(syn::Token![=]) {
                counter_cache = Some(CounterCacheDecl {
                    column: None,
                    tenant_column: None,
                    span: key.span(),
                });
                continue;
            }
            input.parse::<syn::Token![=]>()?;
            // Accept either a bare identifier (`fk = author_id`) or a string
            // literal (`fk = "author_id"`).
            let value = if input.peek(LitStr) {
                input.parse::<LitStr>()?.value()
            } else {
                input.parse::<syn::Ident>()?.to_string()
            };
            if key == "counter_cache" {
                check_counter_cache_column(&key, &value)?;
                counter_cache = Some(CounterCacheDecl {
                    column: Some(value),
                    tenant_column: None,
                    span: key.span(),
                });
            } else if key == "counter_cache_tenant" {
                check_counter_cache_column(&key, &value)?;
                counter_cache_tenant = Some((value, key.span()));
            } else if key == "fk" {
                explicit_fk = Some(value);
            } else if key == "name" {
                explicit_name = Some(value);
            } else if key == "through" {
                explicit_through = Some(value);
            } else if key == "target_fk" {
                explicit_target_fk = Some(value);
            } else if key == "helper" {
                explicit_helper = Some(value);
            } else if key == "dependent" || key == "on_delete" {
                dependent = Some((parse_dependent_action(kind, &key, &value)?, key.span()));
            } else {
                return Err(syn::Error::new_spanned(
                    &key,
                    "expected `fk = <column>`, `name = <accessor>`, \
                         `through = <join_table>`, `target_fk = <column>`, \
                         `helper = <singular>`, `counter_cache` / \
                         `counter_cache = <column>`, or \
                         `counter_cache_tenant = <column>` in association \
                         attribute",
                ));
            }
        }
        Ok((
            target,
            explicit_fk,
            explicit_name,
            explicit_through,
            explicit_target_fk,
            dependent,
            explicit_helper,
            counter_cache,
            counter_cache_tenant,
        ))
    })?;

    // `counter_cache_tenant` scopes the counter cache; on its own it means
    // nothing, and silently ignoring it would leave a multi-tenant app believing
    // it was protected.
    let counter_cache = match (counter_cache, counter_cache_tenant) {
        (Some(decl), Some((tenant_column, _))) => Some(CounterCacheDecl {
            tenant_column: Some(tenant_column),
            ..decl
        }),
        (decl @ Some(_), None) => decl,
        (None, Some((_, span))) => {
            return Err(syn::Error::new(
                span,
                "`counter_cache_tenant = \"<column>\"` scopes a counter cache to \
                 the caller's tenant, so it requires `counter_cache` on the same \
                 association",
            ));
        }
        (None, None) => None,
    };

    if let Some(decl) = counter_cache.as_ref()
        && kind != AssocKind::BelongsTo
    {
        return Err(syn::Error::new(
            decl.span,
            format!(
                "`counter_cache` is a `#[belongs_to]` option: the counter is \
                 maintained by the child's repository — the side that owns the \
                 foreign key and runs the insert/delete — so there is nothing on \
                 this side to hang the maintenance off. Move it to the child \
                 model's `#[belongs_to(...)]`, i.e. `#[belongs_to({model_ident}, \
                 counter_cache)]` on `{target}`"
            ),
        ));
    }
    if let Some(decl) = counter_cache.as_ref()
        && explicit_through.is_some()
    {
        return Err(syn::Error::new(
            decl.span,
            "`counter_cache` is not supported on a `through = <join_table>` \
             (many-to-many) association: its foreign key names a column on the \
             join table, not on this model, so the generated increment would \
             read a column that does not exist. Counters over join tables are \
             out of scope — map the join table as its own model and put \
             `counter_cache` on its `#[belongs_to]`",
        ));
    }

    if explicit_through.is_some() && kind != AssocKind::HasMany {
        return Err(syn::Error::new_spanned(
            &target,
            "`through = <join_table>` (many-to-many) is only supported on \
             `has_many`, not `belongs_to`/`has_one`",
        ));
    }
    if explicit_target_fk.is_some() && explicit_through.is_none() {
        return Err(syn::Error::new_spanned(
            &target,
            "`target_fk = <column>` requires `through = <join_table>`",
        ));
    }
    if let Some((_, dependent_span)) = dependent.as_ref()
        && explicit_through.is_some()
    {
        // A `through = <join_table>` association's `fk` names a column on the
        // *join table*, not on the target model. The emitted cascade calls the
        // target repository's `__autumn_apply_dependent_on_conn`, whose SQL
        // treats `fk` as a column on the target table — so the cascade would
        // hit e.g. `tags.post_id` (nonexistent) instead of the join table.
        // Reject the combination directed rather than silently mis-cascading.
        // (Generating a real join-table cascade is a possible future
        // enhancement; a clean reject is the correct minimal behavior.)
        return Err(syn::Error::new(
            *dependent_span,
            "`dependent`/`on_delete` cascade is not supported on a `through = \
             <join_table>` (many-to-many) association: its foreign key names a \
             column on the join table, not on the target model, so the cascade \
             would delete/nullify the wrong rows — remove `dependent`/\
             `on_delete`, or declare the cascade on a model that maps the join \
             table directly",
        ));
    }
    if explicit_helper.is_some() && explicit_through.is_none() {
        return Err(syn::Error::new_spanned(
            &target,
            "`helper = <singular>` overrides the many-to-many `add_`/`remove_` \
             mutation-helper names and therefore requires `through = \
             <join_table>` (it has no effect on plain `belongs_to`/`has_one`/\
             non-`through` `has_many` associations)",
        ));
    }

    let (fk, derived_name) =
        resolve_fk_and_name(kind, model_ident, &target, explicit_fk.as_deref());
    let name = explicit_name.unwrap_or(derived_name);

    let through = explicit_through.map(|table| {
        let snake_target = pascal_to_snake(&target.to_string());
        let target_fk = explicit_target_fk.unwrap_or_else(|| format!("{snake_target}_id"));
        ThroughSpec { table, target_fk }
    });

    Ok(Association {
        kind,
        target,
        fk,
        name,
        through,
        dependent: dependent.map(|(action, _span)| action),
        helper: explicit_helper,
        counter_cache,
    })
}

/// The `T` of an `Option<T>`, for any spelling of the path (`Option<T>`,
/// `std::option::Option<T>`, `::core::option::Option<T>`).
fn option_inner(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

/// Reject a `counter_cache = "<column>"` value that is not a plain identifier.
///
/// The value is spliced verbatim into generated SQL — `UPDATE posts SET
/// <column> = <column> + $1 …` — so it is the one user-controlled name in the
/// counter-cache codegen that reaches `format!`. Rejecting it here, spanned on
/// the key, is what keeps that splice safe; the run-time
/// `is_plain_identifier` guard in `autumn_web::counter_cache` is the backstop.
fn check_counter_cache_column(key: &syn::Ident, value: &str) -> syn::Result<()> {
    let plain = !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        key,
        format!(
            "`{value}` is not a valid column name for `counter_cache = ...`: \
             the value is spliced verbatim into the generated `UPDATE <parent> \
             SET <column> = <column> + $1` statement, so it must be a plain \
             identifier — ASCII letters, digits and underscores only, and no \
             leading digit"
        ),
    ))
}

/// Collect all `#[belongs_to]` / `#[has_many]` / `#[has_one]` declarations from
/// a model's outer attributes, in source order.
fn resolve_associations(
    model_ident: &syn::Ident,
    attrs: &[syn::Attribute],
) -> syn::Result<Vec<Association>> {
    let mut out = Vec::new();
    for attr in attrs {
        let kind = if attr.path().is_ident("belongs_to") {
            AssocKind::BelongsTo
        } else if attr.path().is_ident("has_many") {
            AssocKind::HasMany
        } else if attr.path().is_ident("has_one") {
            AssocKind::HasOne
        } else {
            continue;
        };
        out.push(parse_assoc_attr(attr, kind, model_ident)?);
    }
    check_m2m_mutation_name_collisions(&out)?;
    check_counter_cache_collisions(model_ident, &out)?;
    Ok(out)
}

/// The parent column a counter-cached association maintains: the explicit
/// `counter_cache = "..."` override, else `{snake(ChildModel)}_count`.
fn counter_cache_column(model_ident: &syn::Ident, assoc: &Association) -> Option<String> {
    let decl = assoc.counter_cache.as_ref()?;
    Some(
        decl.column
            .clone()
            .unwrap_or_else(|| format!("{}_count", pascal_to_snake(&model_ident.to_string()))),
    )
}

/// Reject two counter-cached legs that resolve onto the **same**
/// `(parent table, counter column)` pair.
///
/// Both legs would move one column on every insert, double-counting silently
/// and permanently. This is not a hypothetical: the default column name is
/// derived from the *child* model, so two `belongs_to` legs to the same parent
/// type (`Message` → `sender` / `recipient`, both `User`) collide by default.
///
/// Legs to *different* parent tables that happen to share a column name are
/// fine — they are different columns on different tables — so the key is the
/// pair, not the name.
fn check_counter_cache_collisions(
    model_ident: &syn::Ident,
    assocs: &[Association],
) -> syn::Result<()> {
    let mut seen: Vec<(String, String)> = Vec::new();
    for assoc in assocs {
        let Some(column) = counter_cache_column(model_ident, assoc) else {
            continue;
        };
        let parent_table = infer_table_name(&assoc.target);
        let key = (parent_table.clone(), column.clone());
        if seen.contains(&key) {
            let span = assoc
                .counter_cache
                .as_ref()
                .map_or_else(proc_macro2::Span::call_site, |d| d.span);
            return Err(syn::Error::new(
                span,
                format!(
                    "two counter-cached associations on `{model_ident}` both \
                     maintain `{parent_table}.{column}`, so every insert would \
                     count twice. The column name defaults to \
                     `{{snake(model)}}_count`, which collides whenever two \
                     `belongs_to` legs point at the same parent — give at least \
                     one of them an explicit `counter_cache = \"<column>\"` (and \
                     a matching column on `{parent_table}`)"
                ),
            ));
        }
        seen.push(key);
    }
    Ok(())
}

/// The singular form used to derive a many-to-many association's mutation
/// helper names (`add_{singular}`, `remove_{singular}`).
///
/// Derived from the association's *target type* name (its `pascal_to_snake`),
/// **not** by de-pluralizing the accessor name. A type's singular comes from
/// the type — `Category` → `category`, `Person` → `person` — independent of
/// how its plural accessor is spelled. This keeps the smart-pluralized
/// accessor name (`categories`, `people`) while still yielding correct helpers
/// (`add_category`, `add_person`). De-pluralizing the accessor by stripping a
/// trailing `s` regressed here once the accessor started using the smart
/// pluralizer (#1753): `categories` → `categorie`, `people` → `people`.
fn m2m_mutation_singular(target: &syn::Ident) -> String {
    pascal_to_snake(&target.to_string())
}

/// The *resolved* singular for an association's `add_`/`remove_` mutation
/// helpers: the explicit per-association `helper = "..."` override when present,
/// otherwise the target-type-derived singular.
///
/// The override is the escape hatch (#1785) that lets a model declare more than
/// one many-to-many association to the *same* target type — e.g. `followers`
/// and `following`, both `#[has_many(User, through = Friendship, ...)]` — by
/// giving each a distinct `helper` (`add_follower` vs. `add_following`) instead
/// of the colliding target-derived `add_user`. It is opt-in and explicit: no
/// inference/inverse-inflection (deliberately avoided as the fragile class of
/// bug #1779 fixed).
fn resolved_m2m_singular(assoc: &Association) -> String {
    assoc
        .helper
        .clone()
        .unwrap_or_else(|| m2m_mutation_singular(&assoc.target))
}

/// Reject a model whose many-to-many associations would generate colliding
/// `add_*`/`remove_*`/`set_*` mutation helper names (e.g. two `through =`
/// associations that both derive `add_tag`), rather than emitting a trait
/// with duplicate method definitions.
fn check_m2m_mutation_name_collisions(assocs: &[Association]) -> syn::Result<()> {
    let mut seen: std::collections::HashMap<String, &syn::Ident> = std::collections::HashMap::new();
    for assoc in assocs {
        if assoc.through.is_none() {
            continue;
        }
        let singular = resolved_m2m_singular(assoc);
        if seen.insert(singular.clone(), &assoc.target).is_some() {
            return Err(syn::Error::new_spanned(
                &assoc.target,
                format!(
                    "many-to-many association `{}` (target `{}`) resolves to the \
                     same mutation helpers `add_{singular}`/`remove_{singular}` \
                     as another `through =` association on this model. Two m2m \
                     associations to the same target type would otherwise \
                     generate colliding helpers. Give at least one an explicit \
                     per-association override, e.g. `#[has_many({}, through = \
                     ..., helper = \"...\")]`, so each gets a distinct \
                     `add_`/`remove_` name (the followers/following-through-a-\
                     join pattern) — see \
                     https://github.com/autumn-foundation/autumn/issues/1785",
                    assoc.name, assoc.target, assoc.target,
                ),
            ));
        }
    }
    Ok(())
}

/// Whether an attribute is one of the association declarations consumed by
/// `#[model]` (and therefore must not be re-emitted onto the Diesel struct).
fn is_association_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("belongs_to")
        || attr.path().is_ident("has_many")
        || attr.path().is_ident("has_one")
}

/// Emit the inherent `dependents()` associated function on the model (#1738):
/// the runtime dependent-cascade specs the parent repository's generated
/// `delete_by_id` iterates. Only produced when at least one `#[has_many]` /
/// `#[has_one]` association declares `dependent = <action>`; otherwise the
/// blanket [`AutumnDependents`] impl supplies an empty slice and this emits
/// nothing (so a model without dependents keeps its exact prior codegen).
///
/// Each dependent association resolves its child repository through the
/// `Pg{Child}Repository` naming convention and generates a type-erased thunk
/// into that repository's `__autumn_apply_dependent_on_conn` leaf executor. The
/// thunk owns the child repository across the (immediately awaited) cascade,
/// mirroring the repository-attribute cascade's inline call so the lifetimes of
/// the borrowed connection / visited set line up.
fn emit_dependents_impl(model_ident: &syn::Ident, assocs: &[Association]) -> TokenStream {
    let deps: Vec<&Association> = assocs.iter().filter(|a| a.dependent.is_some()).collect();
    if deps.is_empty() {
        return quote! {};
    }

    let mut thunk_fns: Vec<TokenStream> = Vec::new();
    let mut spec_entries: Vec<TokenStream> = Vec::new();
    for (i, assoc) in deps.iter().enumerate() {
        let action = assoc.dependent.expect("filtered to Some above");
        let action_variant = action.variant_ident();
        let fk = &assoc.fk;
        // Naming convention: the child model `Comment` is served by
        // `PgCommentRepository` (its `#[repository]` trait `CommentRepository`
        // expands to `Pg{trait}`). A child whose repository does not follow this
        // convention (or lives in another crate) uses the repository-attribute
        // `dependent(...)` escape hatch instead.
        let child_repo = format_ident!("Pg{}Repository", assoc.target);
        let thunk_ident = format_ident!("__autumn_dependent_cascade_{}", i);
        thunk_fns.push(quote! {
            fn #thunk_ident<'__a>(
                __pool: &'__a ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                    ::autumn_web::RuntimeConnection,
                >,
                __conn: &'__a mut ::autumn_web::RuntimeConnection,
                __parent_id: i64,
                __parent_soft: bool,
                // Codex round-5-B: the active recursion path (cycle-break) and the
                // monotonic HANDLED set (soft OR physical), threaded through
                // separately. #1800 case 1: `__physical` additionally tracks only
                // physically-removed rows for the diamond revisit-skip.
                __path: &'__a mut ::std::collections::HashSet<(&'static str, i64)>,
                __deleted: &'__a mut ::std::collections::HashSet<(&'static str, i64)>,
                __physical: &'__a mut ::std::collections::HashSet<(&'static str, i64)>,
            ) -> ::autumn_web::repository::RuntimeDependentCascadeFuture<'__a> {
                ::std::boxed::Box::pin(async move {
                    // Own the child repository inside the async block so the
                    // borrow `__autumn_apply_dependent_on_conn` takes of it stays
                    // valid for the whole (immediately awaited) cascade.
                    let __autumn_child_repo = #child_repo::with_pool_untracked(
                        ::core::clone::Clone::clone(__pool),
                    );
                    __autumn_child_repo
                        .__autumn_apply_dependent_on_conn(
                            __conn,
                            #fk,
                            __parent_id,
                            ::autumn_web::repository::DependentAction::#action_variant,
                            __parent_soft,
                            __path,
                            __deleted,
                            __physical,
                        )
                        .await
                })
            }
        });
        spec_entries.push(quote! {
            ::autumn_web::repository::RuntimeDependentSpec {
                fk: #fk,
                action: ::autumn_web::repository::DependentAction::#action_variant,
                cascade: #thunk_ident,
            }
        });
    }

    quote! {
        impl #model_ident {
            /// Runtime dependent-cascade specs consulted by the parent
            /// repository's generated `delete_by_id` (#1738). An inherent shadow
            /// of `AutumnDependents::dependents`; framework plumbing, not a
            /// public API.
            #[doc(hidden)]
            #[must_use]
            pub fn dependents() -> &'static [::autumn_web::repository::RuntimeDependentSpec] {
                #(#thunk_fns)*
                const __AUTUMN_DEPENDENTS: &[::autumn_web::repository::RuntimeDependentSpec] = &[
                    #(#spec_entries),*
                ];
                __AUTUMN_DEPENDENTS
            }
        }
    }
}

/// Emit the inherent counter-cache items on the model (#1325): the
/// `HAS_COUNTER_CACHES` flag and the `counter_caches()` spec slice the child's
/// generated repository consults to keep each parent's `{child}_count` column
/// current.
///
/// Only produced when at least one `#[belongs_to(…, counter_cache)]` is
/// declared; otherwise the blanket `AutumnCounterCaches` impl supplies `false` /
/// an empty slice and this emits nothing, so a model without counter caches
/// keeps its exact prior codegen and its repository's mutation paths stay on
/// their existing (transaction-free, where they were) shape.
///
/// Both items are **inherent**, which is what shadows the blanket impl — and
/// which is why the generated repository names them by concrete path
/// (`Comment::counter_caches()`) rather than through a generic bound.
///
/// The parent's table is convention-derived from the target type
/// (`Post` → `posts`), the same assumption the preload codegen already makes for
/// `belongs_to`; the parent's primary key is assumed to be `id`, matching the
/// dependent-cascade specs. `fk_of` is emitted as a plain `fn` item per leg,
/// which doubles as the type guard: a foreign-key field that is not `i64` /
/// `Option<i64>` fails to coerce to `fn(&Model) -> Option<i64>`.
// One resolution + validation arm per spec field (column, tenant column, the
// foreign key's existence and nullability, the primary key's arity), so it grows
// past the line lint as counter-cache options are added.
#[allow(clippy::too_many_lines)]
fn emit_counter_caches_impl(
    model_ident: &syn::Ident,
    table_name: &str,
    pk_ident: Option<&syn::Ident>,
    has_deleted_at: bool,
    assocs: &[Association],
    all_fields: &[&syn::Field],
) -> syn::Result<TokenStream> {
    let cached: Vec<&Association> = assocs
        .iter()
        .filter(|a| a.counter_cache.is_some())
        .collect();
    if cached.is_empty() {
        return Ok(TokenStream::new());
    }

    // A composite key cannot back a counter cache: the maintenance addresses the
    // child by ONE value, so with two `#[id]` fields the decrement would key on
    // whichever rows share the first component and move every one of their
    // parents. Reject rather than silently corrupt (the same call `#[votable]`
    // makes for the same reason).
    let id_fields: Vec<&syn::Ident> = all_fields
        .iter()
        .filter(|f| has_attr(f, "id"))
        .filter_map(|f| f.ident.as_ref())
        .collect();
    if id_fields.len() > 1 {
        let listed = id_fields
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let span = cached[0]
            .counter_cache
            .as_ref()
            .map_or_else(proc_macro2::Span::call_site, |d| d.span);
        return Err(syn::Error::new(
            span,
            format!(
                "`counter_cache` requires a single primary key: model \
                 `{model_ident}` declares a composite key ({listed}), and the \
                 counter maintenance identifies its child row by one id — the \
                 decrement would key on every row sharing the first component"
            ),
        ));
    }
    let Some(pk_ident) = pk_ident else {
        return Err(syn::Error::new_spanned(
            model_ident,
            "`counter_cache` requires the child model to have a primary-key \
             field: the maintenance resolves the parent from the child row by \
             primary key. Mark one with `#[id]`",
        ));
    };
    let pk_column = pk_ident.to_string();

    // One shared primary-key extractor: the bulk update path matches a
    // post-update record back to the foreign keys captured for it before the
    // update, and every leg keys off the same child primary key.
    // A soft-deleted child is counted by nobody. The generated `update` does not
    // filter `deleted_at`, so one can still be re-parented — without this the
    // move would decrement a parent that had already dropped the row and
    // increment another for a row that stays deleted.
    let live_body = if has_deleted_at {
        quote! { __autumn_cc_record.deleted_at.is_none() }
    } else {
        quote! { { let _ = __autumn_cc_record; true } }
    };
    let mut fk_fns: Vec<TokenStream> = vec![
        quote! {
            fn __autumn_counter_cache_pk(__autumn_cc_record: &#model_ident) -> i64 {
                __autumn_cc_record.#pk_ident
            }
        },
        quote! {
            fn __autumn_counter_cache_live(__autumn_cc_record: &#model_ident) -> bool {
                #live_body
            }
        },
    ];
    let mut spec_entries: Vec<TokenStream> = Vec::new();
    for (index, assoc) in cached.iter().enumerate() {
        let decl = assoc
            .counter_cache
            .as_ref()
            .expect("filtered to Some above");
        let column = counter_cache_column(model_ident, assoc)
            .expect("filtered to a counter-cached association above");
        // Explicit opt-in (`counter_cache_tenant = "<column>"`): the author is
        // asserting the PARENT carries this column. Inferring it from the child
        // would break every tenant-scoped child hanging off a global parent with
        // a hard `column does not exist`, and this macro cannot see the parent's
        // fields to check.
        let tenant_column = decl.tenant_column.as_deref().map_or_else(
            || quote! { ::core::option::Option::None },
            |tenant| quote! { ::core::option::Option::Some(#tenant) },
        );
        let parent_table = infer_table_name(&assoc.target);
        let fk = &assoc.fk;
        let fk_ident = format_ident!("{fk}");

        // The foreign key has to be a real field: without this check the
        // generated `fn` would fail with a bare "no field `post_id`" pointing
        // into macro-expanded code instead of at the attribute.
        let fk_field = all_fields
            .iter()
            .find(|f| f.ident.as_ref().is_some_and(|i| i == fk));
        let Some(fk_field) = fk_field else {
            return Err(syn::Error::new(
                decl.span,
                format!(
                    "`counter_cache` foreign key `{fk}` is not a field of model \
                     `{model_ident}`; add it, or name the right column with \
                     `#[belongs_to({}, fk = <column>, counter_cache)]`",
                    assoc.target
                ),
            ));
        };
        // A nullable foreign key yields the field as-is; a non-null one is
        // wrapped, so both shapes satisfy `fn(&M) -> Option<i64>` and an
        // unparented child is a no-op rather than a `WHERE id = NULL`.
        // `option_inner_type` matches the LAST path segment, so every spelling
        // of the type (`Option<i64>`, `std::option::Option<i64>`,
        // `::core::option::Option<i64>`) takes the same arm — a token-text
        // prefix check would send the qualified spellings down the wrapping arm
        // and fail with a type error inside macro-expanded code.
        let fk_expr = if option_inner(&fk_field.ty).is_some() {
            quote! { __autumn_cc_record.#fk_ident }
        } else {
            quote! { ::core::option::Option::Some(__autumn_cc_record.#fk_ident) }
        };
        let fk_fn = format_ident!("__autumn_counter_cache_fk_{index}");
        fk_fns.push(quote! {
            fn #fk_fn(__autumn_cc_record: &#model_ident) -> ::core::option::Option<i64> {
                #fk_expr
            }
        });
        spec_entries.push(quote! {
            ::autumn_web::repository::CounterCacheSpec {
                child_table: #table_name,
                child_pk: #pk_column,
                child_soft_delete: #has_deleted_at,
                fk_column: #fk,
                parent_table: #parent_table,
                parent_pk: "id",
                counter_column: #column,
                fk_of: #fk_fn,
                pk_of: __autumn_counter_cache_pk,
                live_of: __autumn_counter_cache_live,
                tenant_column: #tenant_column,
            }
        });
    }

    Ok(quote! {
        impl #model_ident {
            /// Whether this model maintains any counter cache (#1325). An
            /// inherent shadow of `AutumnCounterCaches::HAS_COUNTER_CACHES`;
            /// framework plumbing, not a public API.
            #[doc(hidden)]
            pub const HAS_COUNTER_CACHES: bool = true;

            /// Runtime counter-cache specs consulted by this model's generated
            /// repository (#1325). An inherent shadow of
            /// `AutumnCounterCaches::counter_caches`; framework plumbing, not a
            /// public API.
            #[doc(hidden)]
            #[must_use]
            pub fn counter_caches()
                -> &'static [::autumn_web::repository::CounterCacheSpec<#model_ident>]
            {
                #(#fk_fns)*
                const __AUTUMN_COUNTER_CACHES:
                    &[::autumn_web::repository::CounterCacheSpec<#model_ident>] = &[
                        #(#spec_entries),*
                    ];
                __AUTUMN_COUNTER_CACHES
            }
        }
    })
}

/// Generate everything needed to make a model's associations preloadable:
///
/// 1. A `{Model}Preload` spec builder (one optional nested spec per association).
/// 2. A `{Model}Associations` accessor trait, implemented for
///    `Preloaded<{Model}>`, returning typed `NotLoaded` on un-preloaded access.
/// 3. An `impl Preloadable for {Model}` whose `load_associations` issues one
///    batched `WHERE ... IN (...)` query per association and recurses into
///    nested specs.
///
/// Always emits the `Preloadable`/spec/trait scaffolding even with no
/// associations, so that a model is always a valid association *target* (its
/// `Spec` is the empty [`NoPreload`]).
#[allow(clippy::too_many_lines)]
fn emit_association_items(
    model_ident: &syn::Ident,
    table_ident: &syn::Ident,
    vis: &syn::Visibility,
    assocs: &[Association],
) -> TokenStream {
    let preload_spec_ident = format_ident!("{model_ident}Preload");
    let assoc_trait_ident = format_ident!("{model_ident}Associations");
    let model_str = model_ident.to_string();

    // Spec struct fields + builder methods, one per association.
    let mut spec_fields: Vec<TokenStream> = Vec::new();
    let mut spec_builders: Vec<TokenStream> = Vec::new();
    // Accessor trait method signatures + implementations.
    let mut accessor_sigs: Vec<TokenStream> = Vec::new();
    let mut accessor_impls: Vec<TokenStream> = Vec::new();
    // Loader body statements (one block per association).
    let mut loader_blocks: Vec<TokenStream> = Vec::new();
    // Top-level items for many-to-many (`through =`) associations: the
    // hidden join-table module and the per-association mutation trait +
    // blanket impl. Emitted alongside (not inside) the `Preloadable` impl.
    let mut m2m_items: Vec<TokenStream> = Vec::new();

    for assoc in assocs {
        let name_ident = format_ident!("{}", assoc.name);
        let with_ident = format_ident!("{}_with", assoc.name);
        let target = &assoc.target;
        let target_table = format_ident!("{}", infer_table_name(target));
        let fk_ident = format_ident!("{}", assoc.fk);
        let key = &assoc.name;
        // Box the nested spec: associations can be mutually recursive
        // (`Post` belongs_to `Subreddit`, `Subreddit` has_many `Post`), so an
        // inline `Option<TargetSpec>` would be an infinitely-sized type.
        let spec_ty = quote! {
            ::core::option::Option<
                ::std::boxed::Box<<#target as ::autumn_web::preload::Preloadable>::Spec>
            >
        };

        spec_fields.push(quote! { #name_ident: #spec_ty });
        spec_builders.push(quote! {
            /// Preload this association (no nested associations).
            #[must_use]
            #vis fn #name_ident(mut self) -> Self {
                self.#name_ident = ::core::option::Option::Some(
                    ::std::boxed::Box::new(::core::default::Default::default())
                );
                self
            }
            /// Preload this association together with a nested preload spec.
            #[must_use]
            #vis fn #with_ident(
                mut self,
                spec: <#target as ::autumn_web::preload::Preloadable>::Spec,
            ) -> Self {
                self.#name_ident = ::core::option::Option::Some(::std::boxed::Box::new(spec));
                self
            }
        });

        match assoc.kind {
            AssocKind::BelongsTo | AssocKind::HasOne => {
                // Single related record, shared via Arc.
                let stored_ty = quote! {
                    ::core::option::Option<::std::sync::Arc<::autumn_web::preload::Preloaded<#target>>>
                };
                accessor_sigs.push(quote! {
                    /// The preloaded related record, or `Ok(None)` when there is
                    /// no matching row. `Err(NotLoaded)` if it was not preloaded.
                    fn #name_ident(&self) -> ::core::result::Result<
                        ::core::option::Option<&::autumn_web::preload::Preloaded<#target>>,
                        ::autumn_web::preload::NotLoaded,
                    >;
                });
                accessor_impls.push(quote! {
                    fn #name_ident(&self) -> ::core::result::Result<
                        ::core::option::Option<&::autumn_web::preload::Preloaded<#target>>,
                        ::autumn_web::preload::NotLoaded,
                    > {
                        match self.associations().get::<#stored_ty>(#key) {
                            ::core::option::Option::Some(v) => ::core::result::Result::Ok(v.as_deref()),
                            ::core::option::Option::None => ::core::result::Result::Err(
                                ::autumn_web::preload::NotLoaded::new(#model_str, #key),
                            ),
                        }
                    }
                });

                let (key_expr, filter_col) = match assoc.kind {
                    // belongs_to: fk is on *this* model, points at target's id.
                    AssocKind::BelongsTo => {
                        (quote! { __r.#fk_ident }, quote! { #target_table::id })
                    }
                    // has_one: fk is on the *target*, points at this model's id.
                    _ => (quote! { __r.id }, quote! { #target_table::#fk_ident }),
                };
                // For has_one the lookup map keys on the target's fk column; for
                // belongs_to it keys on the target's id.
                let map_key_expr = if assoc.kind == AssocKind::BelongsTo {
                    quote! { __child.id }
                } else {
                    quote! { __child.#fk_ident }
                };

                loader_blocks.push(quote! {
                    if let ::core::option::Option::Some(__child_spec) = &spec.#name_ident {
                        let mut __keys: ::std::vec::Vec<i64> =
                            records.iter().map(|__r| #key_expr).collect();
                        __keys.sort_unstable();
                        __keys.dedup();
                        let __rows: ::std::vec::Vec<#target> = #target_table::table
                            .filter(#filter_col.eq_any(__keys))
                            .select(<#target as ::autumn_web::reexports::diesel::SelectableHelper<::autumn_web::RuntimeBackend>>::as_select())
                            .load::<#target>(&mut *conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        // Apply the target's own read scoping (tenant isolation +
                        // soft-delete) to the freshly loaded rows, mirroring what
                        // the target's repository finders would hide. The source
                        // macro can't see the target's columns, so the target
                        // generates this helper from its own field set.
                        let __rows = #target::__autumn_preload_retain(__rows)?;
                        let mut __children: ::std::vec::Vec<
                            ::autumn_web::preload::Preloaded<#target>
                        > = __rows.into_iter().map(::autumn_web::preload::Preloaded::new).collect();
                        <#target as ::autumn_web::preload::Preloadable>::load_associations(
                            &mut __children, &**__child_spec, &mut *conn,
                        ).await?;
                        let mut __map: ::std::collections::HashMap<
                            i64, ::std::sync::Arc<::autumn_web::preload::Preloaded<#target>>
                        > = __children
                            .into_iter()
                            .map(|__child| (#map_key_expr, ::std::sync::Arc::new(__child)))
                            .collect();
                        for __r in records.iter_mut() {
                            let __v: #stored_ty = __map.get(&(#key_expr)).map(::std::sync::Arc::clone);
                            __r.associations_mut().insert::<#stored_ty>(#key, __v);
                        }
                    }
                });
            }
            AssocKind::HasMany if assoc.through.is_none() => {
                // Many related records owned per-parent. Each child row is
                // fetched at most once (its own primary key is unique in the
                // `WHERE fk IN (...)` result set), so it can be moved
                // directly into its one owning parent's `Vec` — no sharing
                // needed.
                let stored_ty = quote! {
                    ::std::vec::Vec<::autumn_web::preload::Preloaded<#target>>
                };
                accessor_sigs.push(quote! {
                    /// The preloaded related records (possibly empty).
                    /// `Err(NotLoaded)` if this association was not preloaded.
                    fn #name_ident(&self) -> ::core::result::Result<
                        &[::autumn_web::preload::Preloaded<#target>],
                        ::autumn_web::preload::NotLoaded,
                    >;
                });
                accessor_impls.push(quote! {
                    fn #name_ident(&self) -> ::core::result::Result<
                        &[::autumn_web::preload::Preloaded<#target>],
                        ::autumn_web::preload::NotLoaded,
                    > {
                        match self.associations().get::<#stored_ty>(#key) {
                            ::core::option::Option::Some(v) => ::core::result::Result::Ok(v.as_slice()),
                            ::core::option::Option::None => ::core::result::Result::Err(
                                ::autumn_web::preload::NotLoaded::new(#model_str, #key),
                            ),
                        }
                    }
                });
                loader_blocks.push(quote! {
                    if let ::core::option::Option::Some(__child_spec) = &spec.#name_ident {
                        let mut __keys: ::std::vec::Vec<i64> =
                            records.iter().map(|__r| __r.id).collect();
                        __keys.sort_unstable();
                        __keys.dedup();
                        let __rows: ::std::vec::Vec<#target> = #target_table::table
                            .filter(#target_table::#fk_ident.eq_any(__keys))
                            .select(<#target as ::autumn_web::reexports::diesel::SelectableHelper<::autumn_web::RuntimeBackend>>::as_select())
                            .load::<#target>(&mut *conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        // Apply the target's own read scoping (tenant isolation +
                        // soft-delete) to the freshly loaded rows, mirroring what
                        // the target's repository finders would hide. The source
                        // macro can't see the target's columns, so the target
                        // generates this helper from its own field set.
                        let __rows = #target::__autumn_preload_retain(__rows)?;
                        let mut __children: ::std::vec::Vec<
                            ::autumn_web::preload::Preloaded<#target>
                        > = __rows.into_iter().map(::autumn_web::preload::Preloaded::new).collect();
                        <#target as ::autumn_web::preload::Preloadable>::load_associations(
                            &mut __children, &**__child_spec, &mut *conn,
                        ).await?;
                        let mut __groups: ::std::collections::HashMap<i64, #stored_ty> =
                            ::std::collections::HashMap::new();
                        for __child in __children {
                            // Normalize the child's FK (which may be `i64` or a
                            // nullable `Option<i64>`, as every `dependent =
                            // nullify` child has) to an `Option<i64>` key and
                            // drop orphan (`None`-FK, detached) children — they
                            // belong to no loaded parent. Non-null FKs are always
                            // `Some`, so this is byte-identical for them.
                            if let ::core::option::Option::Some(__k) =
                                ::autumn_web::preload::FkKey::autumn_fk_key(&__child.#fk_ident)
                            {
                                __groups.entry(__k).or_default().push(__child);
                            }
                        }
                        for __r in records.iter_mut() {
                            let __v: #stored_ty = __groups.remove(&__r.id).unwrap_or_default();
                            __r.associations_mut().insert::<#stored_ty>(#key, __v);
                        }
                    }
                });
            }
            AssocKind::HasMany => {
                let through = assoc
                    .through
                    .as_ref()
                    .expect("through checked by guard above");
                // Many-to-many: unlike plain has_many, the *same* target row
                // can legitimately belong to more than one currently-loaded
                // parent (the whole point of m2m), so children are shared via
                // `Arc` — mirroring belongs_to/has_one — rather than moved
                // into one owning parent's `Vec`.
                let stored_ty = quote! {
                    ::std::vec::Vec<::std::sync::Arc<::autumn_web::preload::Preloaded<#target>>>
                };
                accessor_sigs.push(quote! {
                    /// The preloaded related records (possibly empty).
                    /// `Err(NotLoaded)` if this association was not preloaded.
                    fn #name_ident(&self) -> ::core::result::Result<
                        ::std::vec::Vec<&::autumn_web::preload::Preloaded<#target>>,
                        ::autumn_web::preload::NotLoaded,
                    >;
                });
                accessor_impls.push(quote! {
                    fn #name_ident(&self) -> ::core::result::Result<
                        ::std::vec::Vec<&::autumn_web::preload::Preloaded<#target>>,
                        ::autumn_web::preload::NotLoaded,
                    > {
                        match self.associations().get::<#stored_ty>(#key) {
                            ::core::option::Option::Some(v) => ::core::result::Result::Ok(
                                v.iter().map(|__a| &**__a).collect()
                            ),
                            ::core::option::Option::None => ::core::result::Result::Err(
                                ::autumn_web::preload::NotLoaded::new(#model_str, #key),
                            ),
                        }
                    }
                });
                {
                    // Many-to-many: the fk lives on a join table, not on the
                    // target. Emit a hidden module declaring the join table
                    // (so this and the target model can both be `through =`
                    // the same physical table without colliding), then a
                    // single batched `INNER JOIN` loader keyed on the join
                    // table's own two columns.
                    let model_snake = pascal_to_snake(&model_ident.to_string());
                    // Length-prefix `model_snake` so the module name can't
                    // collide between two different (model, association)
                    // pairs, e.g. model `Post` assoc `tag_things` vs. model
                    // `PostTag` assoc `things` would otherwise both produce
                    // `__autumn_m2m_post_tag_things`.
                    let join_mod_ident = format_ident!(
                        "__autumn_m2m_{}_{model_snake}_{}",
                        model_snake.len(),
                        assoc.name
                    );
                    let join_table_ident = format_ident!("{}", through.table);
                    let target_fk_ident = format_ident!("{}", through.target_fk);

                    m2m_items.push(quote! {
                        // Hidden Diesel table declaration for the
                        // `#join_table_ident` join table backing
                        // `#model_ident::#name_ident` (`through = #join_table_ident`).
                        // Scoped to its own module (keyed by model + association
                        // name) so two models declaring `through` on the same
                        // physical join table don't produce colliding types.
                        #[allow(
                            missing_docs,
                            unreachable_pub,
                            clippy::all,
                            clippy::pedantic,
                            clippy::nursery
                        )]
                        mod #join_mod_ident {
                            #[allow(unused_imports)]
                            use super::*;
                            ::autumn_web::reexports::diesel::table! {
                                #join_table_ident (#fk_ident, #target_fk_ident) {
                                    #fk_ident -> Int8,
                                    #target_fk_ident -> Int8,
                                }
                            }
                            ::autumn_web::reexports::diesel::allow_tables_to_appear_in_same_query!(
                                #join_table_ident, #target_table
                            );
                        }
                    });

                    loader_blocks.push(quote! {
                        if let ::core::option::Option::Some(__child_spec) = &spec.#name_ident {
                            #[allow(unused_imports)]
                            use ::autumn_web::reexports::diesel::query_dsl::JoinOnDsl as _;
                            let mut __keys: ::std::vec::Vec<i64> =
                                records.iter().map(|__r| __r.id).collect();
                            __keys.sort_unstable();
                            __keys.dedup();
                            let __pairs: ::std::vec::Vec<(i64, #target)> =
                                #join_mod_ident::#join_table_ident::table
                                    .inner_join(
                                        #target_table::table.on(
                                            #target_table::id.eq(
                                                #join_mod_ident::#join_table_ident::#target_fk_ident
                                            )
                                        )
                                    )
                                    .filter(
                                        #join_mod_ident::#join_table_ident::#fk_ident.eq_any(__keys)
                                    )
                                    .select((
                                        #join_mod_ident::#join_table_ident::#fk_ident,
                                        <#target as ::autumn_web::reexports::diesel::SelectableHelper<::autumn_web::RuntimeBackend>>::as_select(),
                                    ))
                                    .load::<(i64, #target)>(&mut *conn)
                                    .await
                                    .map_err(::autumn_web::AutumnError::from)?;
                            // Fail-closed parity probe: run the target's batch
                            // retain against an empty `Vec` so a tenant-scoped
                            // target with no tenant context errors exactly like
                            // belongs_to/has_one/has_many, even when every
                            // parent's join rows happen to be empty (in which
                            // case the per-row `__autumn_preload_keep` loop
                            // below never runs).
                            let _ = #target::__autumn_preload_retain(::std::vec::Vec::new())?;
                            // The same target row can appear once per linking
                            // parent — that is the point of many-to-many — so
                            // recursing into nested associations must run on a
                            // deduplicated set of targets, not once per join row.
                            // Otherwise two parents sharing a target would each
                            // get their own independent copy of its nested
                            // associations, only one of them fully grouped.
                            // Dedup by id, recurse once, then share the single
                            // recursed record across every parent via `Arc`.
                            // Filtering and dedup happen in one pass: each kept
                            // row moves directly into `__unique_by_id` with no
                            // clone, and only its lightweight `(parent_key,
                            // target_id)` pair stays in `__links` for the
                            // grouping pass below.
                            let mut __unique_by_id: ::std::collections::HashMap<i64, #target> =
                                ::std::collections::HashMap::new();
                            let mut __links: ::std::vec::Vec<(i64, i64)> = ::std::vec::Vec::new();
                            for (__fk, __row) in __pairs {
                                if let ::core::option::Option::Some(__row) =
                                    #target::__autumn_preload_keep(__row)?
                                {
                                    let __id = __row.id;
                                    __links.push((__fk, __id));
                                    __unique_by_id.entry(__id).or_insert(__row);
                                }
                            }
                            let mut __unique_children: ::std::vec::Vec<
                                ::autumn_web::preload::Preloaded<#target>
                            > = __unique_by_id
                                .into_values()
                                .map(::autumn_web::preload::Preloaded::new)
                                .collect();
                            <#target as ::autumn_web::preload::Preloadable>::load_associations(
                                &mut __unique_children, &**__child_spec, &mut *conn,
                            ).await?;
                            let __arc_by_id: ::std::collections::HashMap<
                                i64, ::std::sync::Arc<::autumn_web::preload::Preloaded<#target>>
                            > = __unique_children
                                .into_iter()
                                .map(|__c| (__c.id, ::std::sync::Arc::new(__c)))
                                .collect();
                            let mut __groups: ::std::collections::HashMap<i64, #stored_ty> =
                                ::std::collections::HashMap::new();
                            for (__fk, __id) in &__links {
                                if let ::core::option::Option::Some(__arc) = __arc_by_id.get(__id) {
                                    __groups.entry(*__fk).or_default().push(::std::sync::Arc::clone(__arc));
                                }
                            }
                            for __r in records.iter_mut() {
                                let __v: #stored_ty = __groups.get(&__r.id).cloned().unwrap_or_default();
                                __r.associations_mut().insert::<#stored_ty>(#key, __v);
                            }
                        }
                    });

                    // Mutation helpers: `add_{singular}` / `remove_{singular}`
                    // / `set_{plural}` (replace-all), generated once per
                    // `through =` association and blanket-implemented for any
                    // repository whose `M2mConnSource::Model` is this model —
                    // keeping method resolution unambiguous when a model has
                    // more than one m2m association, or when two models' m2m
                    // traits are both in scope.
                    let mutation_trait_ident =
                        format_ident!("{model_ident}{}Mutations", pascal_case(&assoc.name));
                    let singular = resolved_m2m_singular(assoc);
                    let add_ident = format_ident!("add_{singular}");
                    let remove_ident = format_ident!("remove_{singular}");
                    let set_ident = format_ident!("set_{}", assoc.name);
                    let mutation_trait_doc = format!(
                        "Mutation helpers for the `{}` many-to-many association \
                         (`#[has_many({}, through = {})]`). Each method acquires \
                         its own primary-pool connection and is idempotent; \
                         `{set_ident}` wraps its delete-then-insert in a single \
                         transaction.",
                        assoc.name, target, through.table,
                    );

                    m2m_items.push(quote! {
                        #[doc = #mutation_trait_doc]
                        #vis trait #mutation_trait_ident {
                            /// Link `child_id` to `parent_id`. A duplicate call
                            /// is a no-op (`ON CONFLICT DO NOTHING` on the join
                            /// table's composite primary key), not a
                            /// unique-constraint error.
                            fn #add_ident(
                                &self,
                                parent_id: i64,
                                child_id: i64,
                            ) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
                            /// Unlink `child_id` from `parent_id`. A no-op if
                            /// the pair was not linked.
                            fn #remove_ident(
                                &self,
                                parent_id: i64,
                                child_id: i64,
                            ) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
                            /// Replace the full set of children linked to
                            /// `parent_id` with exactly `child_ids`
                            /// (deduplicated), in a single transaction.
                            fn #set_ident(
                                &self,
                                parent_id: i64,
                                child_ids: &[i64],
                            ) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
                        }

                        impl<__R> #mutation_trait_ident for __R
                        where
                            __R: ::autumn_web::repository::M2mConnSource<Model = #model_ident>
                                + ::core::marker::Sync,
                        {
                            async fn #add_ident(
                                &self,
                                parent_id: i64,
                                child_id: i64,
                            ) -> ::autumn_web::AutumnResult<()> {
                                use ::autumn_web::reexports::diesel::{ExpressionMethods as _, QueryDsl as _};
                                use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                                let mut conn = self.__autumn_m2m_write_conn().await?;
                                ::autumn_web::reexports::diesel::insert_into(
                                    #join_mod_ident::#join_table_ident::table
                                )
                                .values((
                                    #join_mod_ident::#join_table_ident::#fk_ident.eq(parent_id),
                                    #join_mod_ident::#join_table_ident::#target_fk_ident.eq(child_id),
                                ))
                                .on_conflict((
                                    #join_mod_ident::#join_table_ident::#fk_ident,
                                    #join_mod_ident::#join_table_ident::#target_fk_ident,
                                ))
                                .do_nothing()
                                .execute(&mut conn)
                                .await
                                .map_err(::autumn_web::AutumnError::from)?;
                                ::core::result::Result::Ok(())
                            }

                            async fn #remove_ident(
                                &self,
                                parent_id: i64,
                                child_id: i64,
                            ) -> ::autumn_web::AutumnResult<()> {
                                use ::autumn_web::reexports::diesel::{ExpressionMethods as _, QueryDsl as _};
                                use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                                let mut conn = self.__autumn_m2m_write_conn().await?;
                                ::autumn_web::reexports::diesel::delete(
                                    #join_mod_ident::#join_table_ident::table
                                        .filter(
                                            #join_mod_ident::#join_table_ident::#fk_ident.eq(parent_id)
                                        )
                                        .filter(
                                            #join_mod_ident::#join_table_ident::#target_fk_ident.eq(child_id)
                                        ),
                                )
                                .execute(&mut conn)
                                .await
                                .map_err(::autumn_web::AutumnError::from)?;
                                ::core::result::Result::Ok(())
                            }

                            async fn #set_ident(
                                &self,
                                parent_id: i64,
                                child_ids: &[i64],
                            ) -> ::autumn_web::AutumnResult<()> {
                                use ::autumn_web::reexports::diesel::{ExpressionMethods as _, QueryDsl as _};
                                use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                                use ::autumn_web::reexports::diesel_async::AsyncConnection as _;
                                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                                let mut __ids: ::std::vec::Vec<i64> = child_ids.to_vec();
                                __ids.sort_unstable();
                                __ids.dedup();
                                let mut conn = self.__autumn_m2m_write_conn().await?;
                                ::autumn_web::__private::scoped_transaction::<(), ::autumn_web::AutumnError, _, _>(&mut *conn, |conn| {
                                    async move {
                                        ::autumn_web::reexports::diesel::delete(
                                            #join_mod_ident::#join_table_ident::table.filter(
                                                #join_mod_ident::#join_table_ident::#fk_ident
                                                    .eq(parent_id)
                                            ),
                                        )
                                        .execute(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?;
                                        if !__ids.is_empty() {
                                            let __values: ::std::vec::Vec<_> = __ids
                                                .iter()
                                                .map(|__child_id| (
                                                    #join_mod_ident::#join_table_ident::#fk_ident
                                                        .eq(parent_id),
                                                    #join_mod_ident::#join_table_ident::#target_fk_ident
                                                        .eq(*__child_id),
                                                ))
                                                .collect();
                                            ::autumn_web::reexports::diesel::insert_into(
                                                #join_mod_ident::#join_table_ident::table
                                            )
                                            .values(__values)
                                            .on_conflict((
                                                #join_mod_ident::#join_table_ident::#fk_ident,
                                                #join_mod_ident::#join_table_ident::#target_fk_ident,
                                            ))
                                            .do_nothing()
                                            .execute(conn)
                                            .await
                                            .map_err(::autumn_web::AutumnError::from)?;
                                        }
                                        ::core::result::Result::Ok(())
                                    }
                                    .scope_boxed()
                                })
                                .await
                            }
                        }
                    });
                }
            }
        }
    }

    quote! {
        /// Eager-loading specification for this model's associations.
        ///
        /// Build it fluently and pass it to a `#[repository]` `preload(...)`
        /// call. Each method enables one association; the `_with` variants take
        /// a nested spec for the related model.
        #[derive(::core::default::Default)]
        #vis struct #preload_spec_ident {
            #(#spec_fields,)*
        }

        impl #preload_spec_ident {
            /// An empty preload set.
            #[must_use]
            #vis fn new() -> Self {
                ::core::default::Default::default()
            }
            #(#spec_builders)*
        }

        impl #model_ident {
            /// Start building an eager-loading spec for this model's
            /// associations. Pass the result to a `#[repository]`
            /// `preload(...)` call.
            #[must_use]
            #vis fn preload() -> #preload_spec_ident {
                #preload_spec_ident::new()
            }
        }

        /// Typed accessors for this model's preloaded associations.
        ///
        /// Accessing an association that was not preloaded returns
        /// [`NotLoaded`](::autumn_web::preload::NotLoaded) rather than issuing
        /// SQL — autumn never lazy-loads.
        #vis trait #assoc_trait_ident {
            #(#accessor_sigs)*
        }

        impl #assoc_trait_ident for ::autumn_web::preload::Preloaded<#model_ident> {
            #(#accessor_impls)*
        }

        impl ::autumn_web::preload::Preloadable for #model_ident {
            type Spec = #preload_spec_ident;

            fn load_associations<'__a>(
                records: &'__a mut [::autumn_web::preload::Preloaded<Self>],
                spec: &'__a Self::Spec,
                conn: &'__a mut ::autumn_web::RuntimeConnection,
            ) -> ::autumn_web::preload::PreloadFuture<'__a> {
                ::std::boxed::Box::pin(async move {
                    #[allow(unused_imports)]
                    use ::autumn_web::reexports::diesel::{
                        QueryDsl as _, ExpressionMethods as _,
                    };
                    #[allow(unused_imports)]
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                    // No parents => nothing to key any `WHERE ... IN (...)` on.
                    // Return before issuing any (empty) association queries.
                    if records.is_empty() {
                        return ::core::result::Result::Ok(());
                    }
                    let _ = (&records, &spec, &conn, #table_ident::table);
                    #(#loader_blocks)*
                    ::core::result::Result::Ok(())
                })
            }
        }

        #(#m2m_items)*
    }
}

/// Emit everything a `#[votable]` declaration generates (#1362):
///
/// 1. a hidden, length-prefixed `mod __autumn_votable_{len}_{model}_{name}`
///    declaring the reaction edge table *and* a minimal projection of the
///    target table (`id`, the aggregate column, and `deleted_at` when the model
///    soft-deletes). Projecting the target keeps the codegen self-contained: it
///    needs no `crate::schema::*` in scope at the `#[model]` site and cannot
///    pick up a conflicting column type;
/// 2. a `{Model}Reactions` trait with `react` / `reaction_of`, blanket
///    implemented over `M2mConnSource<Model = #model_ident>` exactly like the
///    many-to-many mutation helpers, so method resolution stays unambiguous
///    when several models' reaction traits are in scope.
///
/// `react()` runs S1–S5 (lock target → read edge → toggle/flip/insert →
/// recompute the aggregate from ground truth → persist) inside one
/// `scoped_immediate_transaction`. The Postgres `SELECT ... FOR NO KEY UPDATE`
/// row lock is held from before the edge is read until commit, so concurrent
/// reactions on one target are serialized and the recomputed aggregate is
/// exact; `SQLite` gets strictly stronger mutual exclusion from `BEGIN
/// IMMEDIATE`, which is why `for_no_key_update()` lives only in the `pg` arm of
/// `backend_select!` (it is a parse error on `SQLite`, and the unselected arm
/// is never type-checked).
///
/// `pk_ident` is the model's primary-key field (resolved by the caller exactly
/// as the CRUD codegen resolves it). It is used both for the `i64`-primary-key
/// compile-time guard and as the primary-key column of the hidden target
/// projection — a model whose `#[id]` field is not named `id` (e.g. `memo_id`)
/// has no `id` column for S1/S5 to lock and update.
///
/// `has_tenant_id` mirrors `has_deleted_at`: when the model carries a
/// `tenant_id` column the projection declares it and S1/S5 (and
/// `reaction_of`'s target probe) gain a second, tenant-filtered arm, selected
/// at runtime from `M2mConnSource::__autumn_m2m_tenant_scope()`. A model
/// without the column emits none of it and is byte-for-byte unchanged.
#[allow(clippy::too_many_lines)]
fn emit_votable_items(
    model_ident: &syn::Ident,
    table_ident: &syn::Ident,
    vis: &syn::Visibility,
    spec: &VotableSpec,
    has_deleted_at: bool,
    has_tenant_id: bool,
    pk_ident: Option<&syn::Ident>,
) -> TokenStream {
    let model_snake = pascal_to_snake(&model_ident.to_string());
    // Length-prefixed for the same reason as the m2m join module: it keeps two
    // different (model, reaction name) pairs from colliding.
    let edge_mod = format_ident!(
        "__autumn_votable_{}_{model_snake}_{}",
        model_snake.len(),
        spec.name
    );
    let edge_table = format_ident!("{}", spec.table);
    let reactor_fk = format_ident!("{}", spec.reactor_fk);
    let target_fk = format_ident!("{}", spec.target_fk);
    let agg_column = format_ident!("{}", spec.column);
    // The target projection must name the model's real primary-key column:
    // `react()` locks and updates `WHERE #pk_column = $target_id`, and a
    // hard-coded `id` would miss (or worse, hit an unrelated column on) a
    // model whose `#[id]` field is named differently. `None` only happens for
    // models the rest of the macro already refuses to generate CRUD for; keep
    // the historical `id` there so the error surface is unchanged.
    let pk_column = pk_ident.map_or_else(|| format_ident!("id"), ::std::clone::Clone::clone);
    let trait_ident = format_ident!("{model_ident}Reactions");
    let is_sum = spec.aggregate == VoteAggregate::Sum;

    // ── Compile-time guards ──────────────────────────────────────────────
    // The whole reaction surface is typed on `i64` ids: the hidden edge table
    // declares both foreign keys `Int8`, and `react(reactor_id: i64, target_id:
    // i64)` binds them directly. A UUID- or i32-keyed model would otherwise
    // compile the trait fine and only fail deep inside a Diesel bound (or, on
    // an i32 key, silently widen). Pin the model's own primary key here so the
    // error points at the model.
    let pk_guard = pk_ident.map(|pk| {
        quote! {
            const _: fn(&#model_ident) -> i64 = |__autumn_votable_model| {
                __autumn_votable_model.#pk
            };
        }
    });
    // The aggregate field must *be* `i64`, whatever it is spelled as
    // (`std::primitive::i64`, a type alias, …). The macro-level check only
    // rejects the definitely-wrong spellings with a directed message; this
    // guard is what actually enforces the type, by name resolution rather than
    // token text.
    let agg_ty_guard = quote! {
        const _: fn(&#model_ident) -> i64 = |__autumn_votable_model| {
            __autumn_votable_model.#agg_column
        };
    };
    // `by = <Reactor>` is otherwise never mentioned in the generated code — the edge
    // table stores a bare `i64` reactor fk — so a typo'd model name would compile
    // silently. Force its name resolution the way every other association attribute
    // does.
    //
    // Name resolution only, deliberately not a `ModelPrimaryKey<IdType = i64>` bound:
    // `by` accepts hand-written reactor structs (reddit-clone's `User` keeps
    // `password_hash` out of `#[model]`'s generated surface on purpose), and those
    // implement no framework trait to constrain. The reactor's `i64`-primary-key
    // requirement is documented contract; a non-BIGINT reactor fk fails loudly on
    // first use with a database type error, not silent corruption.
    let reactor_ident = &spec.reactor;
    let reactor_guard = quote! {
        const _: ::core::marker::PhantomData<#reactor_ident> =
            ::core::marker::PhantomData;
    };

    // ── The hidden module ────────────────────────────────────────────────
    let value_column_decl = spec.value_column.as_ref().map(|value_column| {
        let value_ident = format_ident!("{value_column}");
        quote! { #value_ident -> Int2, }
    });
    let deleted_at_decl = has_deleted_at.then(|| {
        quote! { deleted_at -> Nullable<Timestamp>, }
    });
    // Projected for the same reason `deleted_at` is: S1/S5 filter on it, so
    // the column has to exist in the hidden target `table!`.
    let tenant_id_decl = has_tenant_id.then(|| {
        quote! { tenant_id -> Text, }
    });
    let hidden_module = quote! {
        // Hidden Diesel declarations backing `#model_ident`'s `#[votable]`
        // reactions: the `#edge_table` edge table (keyed on the composite
        // `(#reactor_fk, #target_fk)` pair that is also the `ON CONFLICT`
        // arbiter) and a minimal `#table_ident` projection. Scoped to its own
        // module so it can never collide with the application's own
        // `crate::schema::#edge_table`.
        #[allow(
            missing_docs,
            unreachable_pub,
            clippy::all,
            clippy::pedantic,
            clippy::nursery
        )]
        mod #edge_mod {
            ::autumn_web::reexports::diesel::table! {
                #edge_table (#reactor_fk, #target_fk) {
                    #reactor_fk -> Int8,
                    #target_fk -> Int8,
                    #value_column_decl
                }
            }
            ::autumn_web::reexports::diesel::table! {
                #table_ident (#pk_column) {
                    #pk_column -> Int8,
                    #agg_column -> Int8,
                    #tenant_id_decl
                    #deleted_at_decl
                }
            }
            // Lets the tenant `EXISTS` subquery on the target appear inside a
            // query on the edge table (and any future cross-table predicate).
            ::autumn_web::reexports::diesel::allow_tables_to_appear_in_same_query!(
                #edge_table,
                #table_ident,
            );
        }
    };

    // ── Shared query fragments ───────────────────────────────────────────
    // `AND deleted_at IS NULL`, emitted only when the model actually has the
    // field — a model that does not soft-delete pays nothing (AC6).
    let live_filter = has_deleted_at.then(|| {
        quote! { .filter(#edge_mod::#table_ident::deleted_at.is_null()) }
    });
    let edge_of_reactor = quote! {
        #edge_mod::#edge_table::table
            .filter(#edge_mod::#edge_table::#reactor_fk.eq(reactor_id))
            .filter(#edge_mod::#edge_table::#target_fk.eq(target_id))
    };
    let not_found_msg = format!("{model_ident} not found");

    // ── Tenant isolation (PR #2177 review, P1) ───────────────────────────
    //
    // Through a `#[repository(..., tenant_scoped)]` repository, filtering the target by
    // primary key alone lets a caller who can guess an id react to another tenant's
    // row: an edge insert plus an aggregate UPDATE across the tenant boundary. So when
    // the model carries a `tenant_id` column, the predicate the repository's finders
    // would apply is resolved once up front and threaded through S1 (the locking
    // existence guard), S5 (the aggregate UPDATE), and `reaction_of`'s target probe.
    //
    // `__autumn_m2m_tenant_scope()` is three-valued and matches the finders exactly:
    // `Some(tenant)` scopes, `None` (non-`tenant_scoped`, or `across_tenants()`) does
    // not, and a `tenant_scoped` repository with no tenant context is an error before
    // anything is read or written.
    //
    // Both arms stay whole, statically-typed queries rather than one boxed query with a
    // conditional predicate: the `pg` arm of S1 carries `.for_no_key_update()`, which an
    // `into_boxed()` query cannot express. The call is emitted unconditionally, even
    // for a model with no `tenant_id` column, because it also carries the cross-shard
    // reject: on a sharded repository in `across_tenants()` mode there is no single
    // right shard for a reaction, and the guard errors before any connection is taken.
    let resolve_tenant = if has_tenant_id {
        quote! {
            let __tenant: ::core::option::Option<::std::string::String> =
                self.__autumn_m2m_tenant_scope()?;
        }
    } else {
        quote! {
            let _: ::core::option::Option<::std::string::String> =
                self.__autumn_m2m_tenant_scope()?;
        }
    };
    let tenant_filter = quote! {
        .filter(#edge_mod::#table_ident::tenant_id.eq(__t))
    };

    // S1's existence/soft-delete guard: `lock` adds the Postgres row lock,
    // `scoped` the tenant predicate.
    let target_probe = |scoped: bool, lock: bool| {
        let tenant_predicate = scoped.then(|| tenant_filter.clone());
        let lock_clause = lock.then(|| quote! { .for_no_key_update() });
        quote! {
            #edge_mod::#table_ident::table
                .filter(#edge_mod::#table_ident::#pk_column.eq(target_id))
                #tenant_predicate
                #live_filter
                .select(#edge_mod::#table_ident::#pk_column)
                #lock_clause
                .first::<i64>(conn)
                .await
                .optional()
                .map_err(::autumn_web::AutumnError::from)?
        }
    };
    let s1_arm = |lock: bool| {
        let unscoped = target_probe(false, lock);
        if has_tenant_id {
            let scoped = target_probe(true, lock);
            quote! {
                match __tenant {
                    ::core::option::Option::Some(ref __t) => { #scoped }
                    ::core::option::Option::None => { #unscoped }
                }
            }
        } else {
            unscoped
        }
    };
    let s1_pg = s1_arm(true);
    let s1_sqlite = s1_arm(false);

    // S5: persist the recomputed aggregate. Tenant-filtered on the same terms
    // as S1 — belt and braces, since S1 already proved the target is in this
    // tenant and holds its lock, but a zero-row S5 is the loud failure the
    // `__persisted == 0` guard below is there for.
    let aggregate_update = |scoped: bool| {
        let tenant_predicate = scoped.then(|| tenant_filter.clone());
        quote! {
            ::autumn_web::reexports::diesel::update(
                #edge_mod::#table_ident::table
                    .filter(#edge_mod::#table_ident::#pk_column.eq(target_id))
                    #tenant_predicate
                    #live_filter
            )
            .set(#edge_mod::#table_ident::#agg_column.eq(__aggregate))
            .execute(conn)
            .await
            .map_err(::autumn_web::AutumnError::from)?
        }
    };
    let s5_persist = if has_tenant_id {
        let scoped = aggregate_update(true);
        let unscoped = aggregate_update(false);
        quote! {
            let __persisted: usize = match __tenant {
                ::core::option::Option::Some(ref __t) => { #scoped }
                ::core::option::Option::None => { #unscoped }
            };
        }
    } else {
        let unscoped = aggregate_update(false);
        quote! { let __persisted: usize = #unscoped; }
    };

    // `reaction_of`'s tenant boundary. The edge table has no tenant column, so the
    // target row is the boundary: a target owned by another tenant has no visible
    // reaction here, which is `Ok(None)` rather than an error — the same thing the
    // reactor would see for a target with no edge. The predicate is an `EXISTS` on the
    // target projection folded into the edge lookup itself, not a separate probe: two
    // statements under READ COMMITTED would leave a TOCTOU window where a concurrent
    // tenant reassignment lands between them and the foreign edge leaks anyway.
    // Deliberately unlocked and not soft-delete filtered, mirroring `reaction_of`'s
    // stance of reporting the edge regardless.
    let reaction_of_tenant_exists = quote! {
        .filter(::autumn_web::reexports::diesel::dsl::exists(
            #edge_mod::#table_ident::table
                .filter(#edge_mod::#table_ident::#pk_column.eq(target_id))
                .filter(#edge_mod::#table_ident::tenant_id.eq(__t))
        ))
    };

    // ── S2: the reactor's current edge, S3: the three-way branch ─────────
    let (react_value_param, read_current, branch, aggregate_query) = if is_sum {
        let value_ident = format_ident!(
            "{}",
            spec.value_column
                .as_ref()
                .expect("sum mode always resolves a value column")
        );
        (
            Some(quote! { value: i16, }),
            quote! {
                let __current: ::core::option::Option<i16> = #edge_of_reactor
                    .select(#edge_mod::#edge_table::#value_ident)
                    .first::<i16>(conn)
                    .await
                    .optional()
                    .map_err(::autumn_web::AutumnError::from)?;
            },
            quote! {
                let (__new_value, __outcome) = match __current {
                    // (a) toggle-off: the same value again removes the edge.
                    ::core::option::Option::Some(__existing) if __existing == value => {
                        ::autumn_web::reexports::diesel::delete(#edge_of_reactor)
                            .execute(conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        (
                            ::core::option::Option::None,
                            ::autumn_web::repository::ReactionOutcome::Removed,
                        )
                    }
                    // (b) flip: replace the value in place, never a second row.
                    ::core::option::Option::Some(_) => {
                        ::autumn_web::reexports::diesel::update(#edge_of_reactor)
                            .set(#edge_mod::#edge_table::#value_ident.eq(value))
                            .execute(conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        (
                            ::core::option::Option::Some(value),
                            ::autumn_web::repository::ReactionOutcome::Flipped,
                        )
                    }
                    // (c) insert. The explicit `(reactor, target)` arbiter is
                    // load-bearing: an edge table may carry more than one
                    // unique constraint, and a bare `ON CONFLICT DO UPDATE`
                    // is a syntax error. Under the target-row lock the
                    // conflict arm is unreachable; it is emitted so that a
                    // lock-bypassing writer produces an idempotent update
                    // rather than a `23505` escaping to the caller.
                    ::core::option::Option::None => {
                        ::autumn_web::reexports::diesel::insert_into(
                            #edge_mod::#edge_table::table
                        )
                        .values((
                            #edge_mod::#edge_table::#reactor_fk.eq(reactor_id),
                            #edge_mod::#edge_table::#target_fk.eq(target_id),
                            #edge_mod::#edge_table::#value_ident.eq(value),
                        ))
                        .on_conflict((
                            #edge_mod::#edge_table::#reactor_fk,
                            #edge_mod::#edge_table::#target_fk,
                        ))
                        .do_update()
                        .set(
                            #edge_mod::#edge_table::#value_ident.eq(
                                ::autumn_web::reexports::diesel::upsert::excluded(
                                    #edge_mod::#edge_table::#value_ident
                                )
                            )
                        )
                        .execute(conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                        (
                            ::core::option::Option::Some(value),
                            ::autumn_web::repository::ReactionOutcome::Inserted,
                        )
                    }
                };
            },
            // `sum(SmallInt)` is typed `Nullable<BigInt>` by Diesel, so no
            // backend-specific cast is needed; the `NULL` (no edges) case is
            // coalesced in Rust rather than in SQL.
            quote! {
                let __total: ::core::option::Option<i64> = #edge_mod::#edge_table::table
                    .filter(#edge_mod::#edge_table::#target_fk.eq(target_id))
                    .select(::autumn_web::reexports::diesel::dsl::sum(
                        #edge_mod::#edge_table::#value_ident
                    ))
                    .get_result::<::core::option::Option<i64>>(conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                let __aggregate: i64 = __total.unwrap_or(0);
            },
        )
    } else {
        (
            None,
            quote! {
                let __current: ::core::option::Option<i64> = #edge_of_reactor
                    .select(#edge_mod::#edge_table::#target_fk)
                    .first::<i64>(conn)
                    .await
                    .optional()
                    .map_err(::autumn_web::AutumnError::from)?;
            },
            quote! {
                let (__new_value, __outcome) = match __current {
                    // Count mode is unary membership: a repeat click can only
                    // toggle the row off, never flip it.
                    ::core::option::Option::Some(_) => {
                        ::autumn_web::reexports::diesel::delete(#edge_of_reactor)
                            .execute(conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        (
                            ::core::option::Option::None,
                            ::autumn_web::repository::ReactionOutcome::Removed,
                        )
                    }
                    ::core::option::Option::None => {
                        ::autumn_web::reexports::diesel::insert_into(
                            #edge_mod::#edge_table::table
                        )
                        .values((
                            #edge_mod::#edge_table::#reactor_fk.eq(reactor_id),
                            #edge_mod::#edge_table::#target_fk.eq(target_id),
                        ))
                        .on_conflict((
                            #edge_mod::#edge_table::#reactor_fk,
                            #edge_mod::#edge_table::#target_fk,
                        ))
                        .do_nothing()
                        .execute(conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                        (
                            ::core::option::Option::Some(1i16),
                            ::autumn_web::repository::ReactionOutcome::Inserted,
                        )
                    }
                };
            },
            quote! {
                let __aggregate: i64 = #edge_mod::#edge_table::table
                    .filter(#edge_mod::#edge_table::#target_fk.eq(target_id))
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
            },
        )
    };

    // One statically-typed lookup per (mode, scoped) combination, mirroring
    // `target_probe`: the scoped arms carry the single-snapshot tenant
    // `EXISTS`, the unscoped arms are byte-identical to the pre-tenant code.
    let reaction_of_lookup = |scoped: bool| {
        let tenant_predicate = scoped.then(|| reaction_of_tenant_exists.clone());
        if is_sum {
            let value_ident = format_ident!(
                "{}",
                spec.value_column
                    .as_ref()
                    .expect("sum mode always resolves a value column")
            );
            quote! {
                #edge_of_reactor
                    #tenant_predicate
                    .select(#edge_mod::#edge_table::#value_ident)
                    .first::<i16>(&mut conn)
                    .await
                    .optional()
                    .map_err(::autumn_web::AutumnError::from)
            }
        } else {
            quote! {
                {
                    let __row: ::core::option::Option<i64> = #edge_of_reactor
                        #tenant_predicate
                        .select(#edge_mod::#edge_table::#target_fk)
                        .first::<i64>(&mut conn)
                        .await
                        .optional()
                        .map_err(::autumn_web::AutumnError::from)?;
                    // Uniform `Option<i16>` in both modes: a present membership
                    // row reports `Some(1)`, so view/widget code is
                    // mode-independent.
                    ::core::result::Result::Ok(__row.map(|_| 1i16))
                }
            }
        }
    };
    let reaction_of_body = if has_tenant_id {
        let scoped = reaction_of_lookup(true);
        let unscoped = reaction_of_lookup(false);
        quote! {
            match __tenant {
                ::core::option::Option::Some(ref __t) => #scoped,
                ::core::option::Option::None => #unscoped,
            }
        }
    } else {
        reaction_of_lookup(false)
    };

    // ── Docs ─────────────────────────────────────────────────────────────
    let aggregate_word = if is_sum { "SUM(value)" } else { "COUNT(*)" };
    // Only documented where it can apply. A model without a `tenant_id`
    // column emits no tenant scoping at all, so promising it there would be a
    // lie — and the shape tests use exactly that to prove the zero-cost path.
    let (react_tenant_doc, react_tenant_not_found, react_tenant_error) = if has_tenant_id {
        (
            "This model has a `tenant_id` column, so a `tenant_scoped` \
             repository matches the target on it too: another tenant's \
             `target_id` is `NotFound` before any write. `across_tenants()` \
             opts out; a `tenant_scoped` repository with no tenant context is \
             an error.\n\n",
            ", or belongs to another tenant",
            "- An error when this repository is `tenant_scoped` and no tenant \
             context was established.\n",
        )
    } else {
        ("", "", "")
    };
    // Applies regardless of `has_tenant_id`: the scope call carrying the
    // reject is emitted unconditionally.
    let cross_shard_error = "- `AutumnError::bad_request` when this repository is sharded and in \
                             `across_tenants()` mode: there is no single right shard for a \
                             reaction, so cross-shard reactions are rejected.\n";
    let (reaction_of_tenant_doc, reaction_of_tenant_error) = if has_tenant_id {
        (
            "Tenant-isolated on the same terms as `react()`: through a \
             `tenant_scoped` repository, a target belonging to another tenant \
             reports `None` rather than that tenant's reaction.\n\n",
            "- An error when this repository is `tenant_scoped` and no tenant \
             context was established.\n",
        )
    } else {
        ("", "")
    };
    let trait_doc = format!(
        "Reaction helpers for `{model_ident}`'s `#[votable(by = {}, aggregate \
         = {})]` declaration: the `{}` edge table keyed on `({}, {})`, \
         aggregated into `{}.{}`.",
        spec.reactor,
        if is_sum { "sum" } else { "count" },
        spec.table,
        spec.reactor_fk,
        spec.target_fk,
        table_ident,
        spec.column,
    );
    let react_doc = format!(
        "Toggle / flip / insert this reactor's reaction on `target_id`, and \
         recompute `{}.{}` from ground truth in the **same** transaction, so a \
         reader never observes edge/aggregate disagreement.\n\
         \n\
         Race-safe: the target row is locked for the whole \
         read-decide-write-recompute window, so N concurrent calls converge to \
         at most one edge per `({}, {})` and the persisted aggregate always \
         equals `{aggregate_word}`.\n\
         \n\
         **Not idempotent — it is a toggle.** Calling it twice with the same \
         arguments reacts and then un-reacts. In particular, blindly retrying \
         a call that timed out (or whose response was lost) can *invert* the \
         outcome: the first attempt may well have committed. Callers that need \
         retry safety must dedupe above this layer — an idempotency key on the \
         HTTP request, or re-reading `reaction_of()` before retrying.\n\
         \n\
         Runs on its **own** pooled connection — it does not join an enclosing \
         `Db::tx`. Do not hold a `Db` extractor across this call on a small \
         pool.\n\
         \n\
         `value` is **not** validated: it is written to the edge table as \
         given, so the edge table's `CHECK` constraint is what keeps the sum \
         meaningful. Never bind it straight from a request body.\n\
         \n\
         {react_tenant_doc}\
         # Errors\n\
         \n\
         - `AutumnError::not_found` when `target_id` does not exist (or is \
         soft-deleted{react_tenant_not_found}).\n\
         {react_tenant_error}\
         {cross_shard_error}\
         - Any database error from the enclosing transaction.",
        table_ident, spec.column, spec.reactor_fk, spec.target_fk,
    );
    let reaction_of_doc = format!(
        "This reactor's current reaction on `target_id`, or `None`.\n\
         \n\
         A single indexed lookup on `{}`; safe to call per-row on a detail \
         page. For feed pages prefer rendering un-highlighted controls over an \
         N+1 — a batch accessor is tracked as a follow-up.\n\
         \n\
         A **read**: it routes per this repository's read route, so a \
         configured replica serves it, and it does not pin read-your-writes. \
         It can therefore lag a `react()` this request just committed — render \
         from the `Reaction` that `react()` returned instead of re-reading, or \
         use a `primary_reads` / `on_primary()` repository.\n\
         \n\
         {reaction_of_tenant_doc}\
         # Errors\n\
         \n\
         {reaction_of_tenant_error}\
         {cross_shard_error}\
         - Any database error.",
        spec.table,
    );

    quote! {
        #hidden_module

        #pk_guard
        #agg_ty_guard
        #reactor_guard

        #[doc = #trait_doc]
        #vis trait #trait_ident {
            #[doc = #react_doc]
            fn react(
                &self,
                reactor_id: i64,
                target_id: i64,
                #react_value_param
            ) -> impl ::std::future::Future<
                Output = ::autumn_web::AutumnResult<::autumn_web::repository::Reaction>
            > + Send;

            #[doc = #reaction_of_doc]
            fn reaction_of(
                &self,
                reactor_id: i64,
                target_id: i64,
            ) -> impl ::std::future::Future<
                Output = ::autumn_web::AutumnResult<::core::option::Option<i16>>
            > + Send;
        }

        impl<__R> #trait_ident for __R
        where
            __R: ::autumn_web::repository::M2mConnSource<Model = #model_ident>
                + ::core::marker::Sync,
        {
            async fn react(
                &self,
                reactor_id: i64,
                target_id: i64,
                #react_value_param
            ) -> ::autumn_web::AutumnResult<::autumn_web::repository::Reaction> {
                use ::autumn_web::reexports::diesel::result::OptionalExtension as _;
                use ::autumn_web::reexports::diesel::{ExpressionMethods as _, QueryDsl as _};
                use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                // Resolved before the connection is taken, so a tenant_scoped
                // repository with no tenant context fails closed without
                // occupying a pooled connection.
                #resolve_tenant
                let mut conn = self.__autumn_m2m_write_conn().await?;
                ::autumn_web::__private::scoped_immediate_transaction::<
                    ::autumn_web::repository::Reaction,
                    ::autumn_web::AutumnError,
                    _,
                >(&mut *conn, |conn| {
                    async move {
                        // S1: take the row lock on the target before anything
                        // is read, in the same statement as the existence and
                        // soft-delete guard. `FOR NO KEY UPDATE`, not `FOR
                        // UPDATE`: this transaction only ever writes the
                        // target's aggregate column, never its key. The weaker
                        // mode still conflicts with itself, so reactions on one
                        // target stay serialized, but does not conflict with the
                        // `FOR KEY SHARE` locks Postgres takes for foreign-key
                        // checks, so a concurrent `INSERT INTO comments
                        // (post_id) …` does not queue behind a vote. On SQLite
                        // the enclosing `BEGIN IMMEDIATE` already serializes
                        // writers, so the clause is redundant as well as
                        // unemittable.
                        let __target: ::core::option::Option<i64> =
                            ::autumn_web::backend_select! {
                                pg => { #s1_pg },
                                sqlite => { #s1_sqlite },
                            };
                        if __target.is_none() {
                            return ::core::result::Result::Err(
                                ::autumn_web::AutumnError::not_found_msg(#not_found_msg)
                            );
                        }

                        // S2 — safe: the target lock is held.
                        #read_current

                        // S3 — exactly one of delete / update / upsert.
                        #branch

                        // S4 — ground truth, not an accumulated delta, so any
                        // historical drift self-heals on the next reaction.
                        #aggregate_query

                        // S5 — persist, in the same transaction as S3.
                        #s5_persist

                        // Defense in depth: unreachable while S1's lock holds —
                        // the target existed and was live when we locked it, and
                        // nobody can delete or soft-delete it underneath us. If
                        // the lock strength ever regresses this fails loudly
                        // instead of committing an edge whose aggregate was
                        // silently dropped on the floor.
                        if __persisted == 0 {
                            return ::core::result::Result::Err(
                                ::autumn_web::AutumnError::not_found_msg(#not_found_msg)
                            );
                        }

                        ::core::result::Result::Ok(
                            ::autumn_web::repository::Reaction::__new(
                                __new_value,
                                __aggregate,
                                __outcome,
                            )
                        )
                    }
                    .scope_boxed()
                })
                .await
            }

            async fn reaction_of(
                &self,
                reactor_id: i64,
                target_id: i64,
            ) -> ::autumn_web::AutumnResult<::core::option::Option<i16>> {
                use ::autumn_web::reexports::diesel::result::OptionalExtension as _;
                use ::autumn_web::reexports::diesel::{ExpressionMethods as _, QueryDsl as _};
                use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                #resolve_tenant
                // A read: routed per the repository's `ReadRoute`, and it does
                // not mark the read-your-writes pin.
                let mut conn = self.__autumn_m2m_read_conn().await?;
                #reaction_of_body
            }
        }
    }
}

/// Extract `#[validate(...)]` attributes from a field (verbatim pass-through).
fn validate_attrs(field: &Field) -> Vec<&syn::Attribute> {
    field
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("validate"))
        .collect()
}

/// `validator` validators that cannot be soundly enforced on the generated
/// `UpdateModel` `Patch<T>` fields: either they have NO `Patch<T>` per-field
/// trait impl (struct-level / cross-field rules), or their `Patch<T>` impl
/// inverts our absent-field skip semantics (`does_not_contain`). See
/// `validate_attrs_for_patch` for the full rationale.
///
/// `nested` is listed here too: it has no `Patch<T>` impl at all, so it must
/// be dropped from the `Update{Model}` field the same as `custom`. Separately
/// from the `Patch<T>` question, `nested` on the read model / `NewModel` can
/// hit a real `E0034: multiple applicable items in scope` if the *model's own
/// defining module* also imports `autumn_web::prelude::ValidateExt` (a
/// blanket `impl<T: validator::Validate> ValidateExt for T` also named
/// `validate`, which collides with `validator_derive`'s bare
/// `(&field).validate()` nested codegen). This crate cannot detect that from
/// here -- a derive/attribute macro only sees the item it's attached to, not
/// the rest of its enclosing module's `use` statements -- so it is not
/// rejected at macro-expansion time; see the hazard note on
/// `ValidateExt`'s doc comment (`autumn/src/validation.rs`) for the full
/// explanation and the workaround (keep the model's own module free of that
/// import, or use `#[validate(custom(...))]` instead).
const NON_PATCH_VALIDATORS: &[&str] = &[
    // See the module-level rationale above for why `nested` is listed here
    // without a `reject_*`-style macro-time guard.
    "custom",
    "must_match",
    "nested",
    "credit_card",
    "non_control_character",
    "does_not_contain",
];

/// `validator` validators whose `Patch<T>` per-field impl (in `autumn/src/hooks.rs`)
/// delegates to `T`, but for which `validator` provides **no `impl … for
/// Option<T>`** — so they cannot be enforced on an `Option<_>`-typed model
/// field's `UpdateModel` `Patch<T>` field.
///
/// Background (#1719 / Codex P2): the `#[derive(Validate)]` on `NewModel`
/// syntactically unwraps `Option<Inner>` and calls the validator on the inner
/// `Inner` (e.g. `String`), so `#[validate(ip)] ip: Option<String>` compiles
/// on create. But `UpdateModel`'s field is `Patch<Option<String>>`; the derive
/// does NOT recognise `Patch<…>` as an `Option`, so it calls the validator on
/// the whole `Patch<Option<String>>`. Our `impl<T: ValidateIp> ValidateIp for
/// Patch<T>` then requires `Option<String>: ValidateIp`. In `validator` 0.20,
/// `ValidateIp` is supplied ONLY by the blanket `impl<T: ToString> ValidateIp
/// for T` (validation/ip.rs:13) — there is NO `impl ValidateIp for Option<T>`
/// and `Option<String>` is not `ToString`/`Display`, so `Option<String>:
/// ValidateIp` is unsatisfied and `UpdateModel` **fails to compile**.
///
/// The other per-field validators our `Patch<T>` block implements are NOT
/// affected because `validator` 0.20 DOES ship an `Option<T>` impl for each:
/// `length` (validation/length.rs:115), `range` (validation/range.rs:65),
/// `email` (validation/email.rs:99), `url` (validation/urls.rs:56), `contains`
/// (validation/contains.rs:15), and even `regex` (validation/regex.rs:76). So
/// `ip` is the sole Option-incompatible validator in our supported set.
///
/// These are filtered from the PATCH path **only when the field is
/// `Option<…>`-typed**: on a non-`Option` field (e.g. `#[validate(ip)] ip:
/// String`) `Patch<String>: ValidateIp` holds via the `ToString` blanket, so
/// `ip` must stay enforced on update there. On create, `ip` still runs for the
/// `Option` field via the derive's `Option`-unwrap on `NewModel`.
const OPTION_INCOMPATIBLE_VALIDATORS: &[&str] = &["ip"];

/// Like [`validate_attrs`], but tailored for the `UpdateModel` `Patch<T>` fields
/// (#1719): drop the nested validators that `Patch<T>` cannot enforce.
///
/// `NewModel` fields carry the bare `T` and derive `validator::Validate`
/// directly, so they keep every validator verbatim. `UpdateModel` fields are
/// `Patch<T>`, which only implements validator's per-field *declarative* traits
/// (`length`, `email`, `url`, `range`, `contains`, `ip`, `regex`, `required`,
/// …). The validators in [`NON_PATCH_VALIDATORS`] (`custom`, `must_match`,
/// `nested`, `credit_card`, `non_control_character`) have no *usable*
/// `Patch<T>` impl (`must_match` technically compiles via `Patch<T>: Eq`, but
/// with the wrong semantics — comparing raw `Unchanged`/`Set`/`Clear`
/// sentinels rather than the underlying values), so propagating them verbatim
/// would break `UpdateModel` compilation or its correctness even though
/// `NewModel` still compiles — a latent footgun for a user who adds e.g.
/// `#[validate(custom(...))]` to a model field. (`nested` has its own,
/// separate hazard even where it does compile -- see [`NON_PATCH_VALIDATORS`]'s
/// doc comment.)
///
/// `required` is deliberately NOT in the denylist (#1719 / Codex P2): it must
/// propagate so the `UpdateModel` rejects an explicit `null` on a required
/// field. `Patch<T>` implements `ValidateRequired` with tri-state semantics
/// (`Unchanged` skips, `Clear`/`Set(None)` fail); see the impl in
/// `autumn/src/hooks.rs`. `required` is only sensible on `Option`-typed fields,
/// whose patch field is `Patch<Option<T>>` and satisfies the impl's
/// `Option<T>: ValidateRequired` bound. (`required` on a non-`Option` field
/// never compiles even on `NewModel`, since `validator` supplies
/// `ValidateRequired` only for `Option<T>`, so no filtering is needed for it.)
///
/// `does_not_contain` is also filtered, for a subtler reason: it does compile
/// on `Patch<T>` (validator supplies it via the blanket
/// `impl<T: ValidateContains> ValidateDoesNotContain for T`, defined as
/// `!validate_contains(...)`), but that inverts our skip semantics. Our
/// `ValidateContains for Patch<T>` returns `true` for an absent field so
/// `contains` is *skipped*; the blanket flips that to `false`, so an OMITTED
/// `does_not_contain` field would spuriously *fail* with 422. Since one
/// `ValidateContains` value can't satisfy both skip directions (and coherence
/// blocks a direct `ValidateDoesNotContain for Patch<T>` impl), we drop
/// `does_not_contain` from the PATCH path and enforce it on create only.
/// `contains` itself stays on the PATCH path — its absent→pass behaviour is
/// correct.
///
/// This is a *denylist* (not an allowlist): unknown/future validators pass
/// through untouched, so a newly-supported declarative validator is never
/// silently dropped.
///
/// Additionally, when the model field is syntactically `Option<…>`, the
/// [`OPTION_INCOMPATIBLE_VALIDATORS`] (e.g. `ip`) are ALSO dropped: `validator`
/// ships no `impl … for Option<T>` for them, so `Patch<Option<T>>` would not
/// implement the corresponding per-field trait and `UpdateModel` would fail to
/// compile. They still run on create (the `NewModel` derive unwraps the
/// `Option`) and remain enforced on non-`Option` update fields. See
/// [`OPTION_INCOMPATIBLE_VALIDATORS`] for the full derivation.
///
/// `Option<…>` is detected the same way `validator` itself detects it — by the
/// last path segment's ident being `Option` (via [`is_option_type`]) — which
/// also matches fully-qualified `std::option::Option` / `core::option::Option`.
/// Limitation: a type *alias* to `Option` is not detected (no worse than the
/// derive's own behaviour, which also inspects the syntactic type).
///
/// Documented limitation: `custom`/`must_match`/`does_not_contain`/etc. are
/// enforced on create (via `NewModel`) but NOT on the PATCH update path unless
/// the model builds a draft via `from_patch`, which validates the merged
/// concrete model and closes this gap for `custom`, `must_match`,
/// `does_not_contain`, and `ip` on `Option<…>` fields (issues #1778/#1751).
/// `nested` is filtered from `Patch<T>` the same way, and the same
/// merged-model mechanism would run it too -- but see [`NON_PATCH_VALIDATORS`]'s
/// doc comment for the separate `ValidateExt` hazard that applies to it on
/// create and the merged model alike, independent of this Patch-vs-update
/// question. (`required` IS enforced on the PATCH path via the tri-state
/// `Patch<T>` impl.)
fn validate_attrs_for_patch(field: &Field) -> Vec<syn::Attribute> {
    let field_is_option = is_option_type(&field.ty);
    let mut out = Vec::new();
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("validate")) {
        let syn::Meta::List(list) = &attr.meta else {
            // A bare `#[validate]` with no nested items — nothing to filter.
            out.push(attr.clone());
            continue;
        };
        let parser = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated;
        let Ok(nested) = parser.parse2(list.tokens.clone()) else {
            // If the nested metas don't parse, fall back to verbatim
            // pass-through rather than silently dropping validation.
            out.push(attr.clone());
            continue;
        };
        let kept: Vec<syn::Meta> = nested
            .into_iter()
            .filter(|m| {
                let Some(id) = m.path().get_ident() else {
                    return true;
                };
                if NON_PATCH_VALIDATORS.iter().any(|v| id == *v) {
                    return false;
                }
                // Option-incompatible validators (e.g. `ip`) only break the
                // PATCH path on `Option<…>`-typed fields; keep them otherwise.
                if field_is_option && OPTION_INCOMPATIBLE_VALIDATORS.iter().any(|v| id == *v) {
                    return false;
                }
                true
            })
            .collect();
        if kept.is_empty() {
            // Filtering emptied the `#[validate(...)]` — drop the attr entirely.
            continue;
        }
        out.push(syn::parse_quote!(#[validate(#(#kept),*)]));
    }
    out
}

/// Filter out framework-specific attributes (`#[id]`, `#[indexed]`, `#[validate]`,
/// `#[default]`, `#[factory_assoc]`, `#[lock_version]`, `#[position]`,
/// `#[searchable]`, `#[state_machine]`) that shouldn't be on the query struct
/// (they'd confuse Diesel derives).
fn user_attrs(field: &Field) -> Vec<&syn::Attribute> {
    field
        .attrs
        .iter()
        .filter(|a| {
            !a.path().is_ident("id")
                && !a.path().is_ident("indexed")
                && !a.path().is_ident("validate")
                && !a.path().is_ident("default")
                && !a.path().is_ident("factory_assoc")
                && !a.path().is_ident("lock_version")
                && !a.path().is_ident("searchable")
                && !a.path().is_ident("encrypted")
                // #1654: `#[classified]` is a marker the model macro reads; the
                // behaviour lives in the field's `Classified<T, Marker>` type, so
                // the attribute itself must never reach the Diesel derives.
                && !a.path().is_ident("classified")
                && !a.path().is_ident("private")
                && !a.path().is_ident("normalize")
                && !a.path().is_ident("state_machine")
                // Declarative-schema field markers (#1975). Accepted and
                // validated, but stripped from the generated query struct so
                // they never leak onto the Diesel derives; codegen is unchanged.
                && !a.path().is_ident("unique")
                && !a.path().is_ident("references")
                && !a.path().is_ident("position")
                // #1384: `#[translatable]` is a marker the model macro reads;
                // the behaviour lives in the field's `Translated` type, so the
                // attribute itself must never reach the Diesel derives.
                && !a.path().is_ident("translatable")
        })
        .collect()
}

/// Whether a field is marked `#[private]` (issue #1374): excluded from the
/// model's `Serialize` impl (JSON responses) while remaining a normal,
/// queryable Rust field mapped to its DB column. The write path (`New*` /
/// `Update*` / `Changeset`) is unaffected, so a client can still *set* the
/// value while never *reading* it back.
fn field_is_private(field: &Field) -> bool {
    has_attr(field, "private")
}

/// Whether a field's serialized form should be hidden from JSON. A field is
/// hidden when it is explicitly `#[private]`, or when it is `#[encrypted]`
/// without opting back in via `admin_visible` — ciphertext/plaintext of an
/// encrypted column must never leak to the public API by default (#1374 AC).
/// Exposure re-uses the existing `admin_visible` knob rather than a second one.
fn field_hidden_from_json(field: &Field) -> bool {
    if field_is_private(field) {
        return true;
    }
    let enc = parse_field_encrypted(field).unwrap_or(EncryptedSpec::NONE);
    enc.is_encrypted() && !enc.admin_visible
}

/// Whether a field already carries a serde `skip`/`skip_serializing` so we do
/// not emit a duplicate attribute (serde rejects duplicates).
fn field_already_skips_serialization(field: &Field) -> bool {
    let mut skips = false;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") || meta.path.is_ident("skip_serializing") {
                skips = true;
            }
            if let Ok(value) = meta.value() {
                // `name = value` form — consume the value literal.
                let _: syn::Result<syn::Lit> = value.parse();
            } else if meta.input.peek(syn::token::Paren) {
                // Nested-list form, e.g. `bound(serialize = "...")`. Consume the
                // parenthesized tokens so `parse_nested_meta` can continue to the
                // next item in the same `#[serde(...)]` (otherwise the loop errors
                // out early and a later `skip_serializing` is missed, producing a
                // duplicate injected attribute).
                let _ = meta.parse_nested_meta(|_| Ok(()));
            }
            Ok(())
        });
    }
    skips
}

/// Encryption mode requested by an `#[encrypted]` field attribute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EncryptedMode {
    /// Not an encrypted field.
    None,
    /// `#[encrypted]` — randomized AEAD (default; no equality lookups).
    Randomized,
    /// `#[encrypted(deterministic)]` — stable ciphertext; supports equality
    /// lookups, at the cost of leaking plaintext equality through ciphertext.
    Deterministic,
}

/// Parsed `#[encrypted(...)]` field specification.
#[derive(Clone, Copy)]
struct EncryptedSpec {
    mode: EncryptedMode,
    /// `admin_visible` — render decrypted plaintext in admin views (the admin
    /// surface itself is authorization-gated; #496). Default: redacted.
    admin_visible: bool,
    /// `versioned_ciphertext` — store encrypted before/after ciphertext in record
    /// version history instead of the default "changed (encrypted)" marker.
    versioned_ciphertext: bool,
}

impl EncryptedSpec {
    const NONE: Self = Self {
        mode: EncryptedMode::None,
        admin_visible: false,
        versioned_ciphertext: false,
    };
    fn is_encrypted(self) -> bool {
        self.mode != EncryptedMode::None
    }
}

/// Parse an `#[encrypted]` / `#[encrypted(deterministic, admin_visible, ...)]`
/// field attribute.
fn parse_field_encrypted(field: &syn::Field) -> syn::Result<EncryptedSpec> {
    for attr in &field.attrs {
        if !attr.path().is_ident("encrypted") {
            continue;
        }
        // `#[encrypted]` (bare path) -> randomized, no opt-ins.
        if matches!(attr.meta, syn::Meta::Path(_)) {
            return Ok(EncryptedSpec {
                mode: EncryptedMode::Randomized,
                ..EncryptedSpec::NONE
            });
        }
        let mut spec = EncryptedSpec {
            mode: EncryptedMode::Randomized,
            ..EncryptedSpec::NONE
        };
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("deterministic") {
                spec.mode = EncryptedMode::Deterministic;
                Ok(())
            } else if meta.path.is_ident("randomized") {
                spec.mode = EncryptedMode::Randomized;
                Ok(())
            } else if meta.path.is_ident("admin_visible") {
                spec.admin_visible = true;
                Ok(())
            } else if meta.path.is_ident("versioned_ciphertext") {
                spec.versioned_ciphertext = true;
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[encrypted]` option; expected one of \
                     `deterministic`, `randomized`, `admin_visible`, `versioned_ciphertext`",
                ))
            }
        })?;
        return Ok(spec);
    }
    Ok(EncryptedSpec::NONE)
}

/// Convenience: just the mode (used by the diesel-wrapper routing).
fn parse_field_encrypted_mode(field: &syn::Field) -> syn::Result<EncryptedMode> {
    Ok(parse_field_encrypted(field)?.mode)
}

// ── #1654: `#[classified]` field attribute ───────────────────────────────────

/// Whether a field is marked `#[classified]` (issue #1654): its value carries a
/// data classification on the *type*, so it cannot reach a gated sink without
/// passing a declared declassification boundary.
fn field_is_classified(field: &syn::Field) -> bool {
    has_attr(field, "classified")
}

/// Parse `#[classified]` / `#[classified(personal_data)]`.
///
/// The first slice supports exactly one tier, so the only accepted spelling
/// beyond the bare marker is the tier's own name -- written out so a second tier
/// is additive rather than a breaking respelling.
fn validate_classified_tier(field: &syn::Field) -> syn::Result<()> {
    for attr in &field.attrs {
        if !attr.path().is_ident("classified") {
            continue;
        }
        // `continue`, not `return`: a bare marker validates itself, but a field
        // can carry more than one `#[classified]` attribute and every one of
        // them has to be checked. Returning here let
        // `#[classified] #[classified(top_secret)]` through, recording the
        // column as `personal_data` while silently ignoring the tier the author
        // actually asked for.
        if matches!(attr.meta, syn::Meta::Path(_)) {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("personal_data") {
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[classified]` tier; the first slice supports \
                     `personal_data` only (issue #1654). Write `#[classified]` or \
                     `#[classified(personal_data)]`.",
                ))
            }
        })?;
    }
    Ok(())
}

/// The generated zero-sized marker naming one classified column, e.g.
/// `Customer::email` -> `CustomerEmailClassified`.
///
/// The name is load bearing twice over: it is what the compiler prints when a
/// leak is attempted (so it has to read as the field it guards), and it is what
/// `declassify!` names to tie a boundary to exactly one column.
fn classified_marker_ident(model: &syn::Ident, field: &syn::Ident) -> syn::Ident {
    let mut camel = String::new();
    let mut upper_next = true;
    for ch in unraw_ident(field).chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            camel.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            camel.push(ch);
        }
    }
    // `Classified`, not `Field`: `{Model}Field` is already the generated
    // field enum, so `Customer::email` -> `CustomerEmailClassified` would collide
    // with a sibling `#[model] struct CustomerEmail`'s field enum. No column
    // name can be empty, so `{Model}{Column}Classified` cannot collide with
    // another model's marker either.
    format_ident!(
        "{}{}Classified",
        unraw_ident(model),
        camel,
        span = field.span()
    )
}

/// Validate a `#[classified]` field.
///
/// v1 classifies non-null `String` columns, mirroring `#[encrypted]`: those are
/// the realistic personal-data columns (email, phone, address), and restricting
/// the shape keeps one Diesel column representation instead of a generic one.
/// Every rejected combination below is a pair whose two halves disagree about
/// whether the value may be read back out in the clear.
fn validate_classified_field(field: &syn::Field) -> syn::Result<()> {
    if !field_is_classified(field) {
        return Ok(());
    }
    validate_classified_tier(field)?;

    for (marker, why) in [
        (
            "encrypted",
            "an encrypted column already routes through its own Diesel wrapper, and \
             the two wrappers cannot both own the column's representation. \
             `#[encrypted]` protects the value at rest; `#[classified]` proves where \
             it may go. Combining them lands in a later slice",
        ),
        (
            "searchable",
            "full-text search indexes the stored column, which would put the personal \
             data in a search vector the classification cannot gate",
        ),
        (
            "normalize",
            "normalizers rewrite the column in place, which needs the plaintext out of \
             the classification with no boundary to record it",
        ),
        (
            "translatable",
            "a per-locale container is a JSON document, not the single value a \
             classification tier applies to",
        ),
        (
            "id",
            "a primary key is echoed back in URLs, ETags and pagination cursors, none \
             of which is a gated sink",
        ),
        (
            "lock_version",
            "the optimistic-lock column is framework-managed and must stay a plain integer",
        ),
        (
            "position",
            "the position column is framework-managed and must stay a plain integer",
        ),
        (
            "state_machine",
            "a state column must hold one state name, which the state-machine codegen \
             reads and writes directly",
        ),
    ] {
        if has_attr(field, marker) {
            return Err(syn::Error::new_spanned(
                field,
                format!("`#[classified]` cannot be combined with `#[{marker}]`: {why}."),
            ));
        }
    }

    // The `String`-only rule is checked *after* the conflicting-marker loop on
    // purpose: several of those markers (`#[translatable]`, `#[lock_version]`,
    // `#[position]`, `#[id]`) imply a non-`String` column, and the reason that
    // names the conflict is the one that tells the author what to do.
    let is_string = matches!(&field.ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "String"));
    if !is_string {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[classified]` is only supported on non-null `String` fields in v1 \
             (issue #1654). Model the column as a `String`, or classify a \
             neighbouring string column that holds the personal data.",
        ));
    }

    // `tenant_id` is read directly by the tenancy codegen (preload scoping,
    // `ModelTenantIdMeta`, the search-text projection), all of which expect the
    // declared type. It is also an isolation key, not personal data.
    if field
        .ident
        .as_ref()
        .is_some_and(|i| unraw_ident(i) == "tenant_id")
    {
        return Err(syn::Error::new_spanned(
            field,
            "`#[classified]` cannot be applied to `tenant_id`: the tenancy codegen reads \
             the column directly as its isolation key, and a tenant identifier is not the \
             personal data this tier describes.",
        ));
    }
    // A custom serde adapter is written against the declared type and would be
    // handed the taint wrapper instead -- and a `serialize_with` is a serializer
    // for the very value the classification is withholding.
    if has_hook_serde_adapter(field, SerdeAdapterMode::Serialize)
        || has_hook_serde_adapter(field, SerdeAdapterMode::Deserialize)
    {
        return Err(syn::Error::new_spanned(
            field,
            "`#[classified]` cannot be combined with `#[serde(with = ...)]`, \
             `#[serde(serialize_with = ...)]` or `#[serde(deserialize_with = ...)]`: the \
             adapter is written against the declared type, and a `serialize_with` is a \
             serializer for the value the classification withholds.",
        ));
    }
    // The classified column is registered in the data-flow manifest under its
    // Rust field name. A `#[serde(rename)]` would desync the manifest row from
    // the wire name a reviewer reads it against.
    if field_has_serde_rename(field) {
        return Err(syn::Error::new_spanned(
            field,
            "`#[classified]` fields cannot use `#[serde(rename = ...)]` in v1: the \
             column is registered in the data-flow manifest under its Rust name, \
             which must match the name the manifest is reviewed against.",
        ));
    }
    Ok(())
}

/// Build a manual `Debug` impl that redacts encrypted fields, so plaintext
/// (held in memory as a `String` for ergonomics) never appears in `Debug`
/// output, panic backtraces, or framework error messages. The development-only
/// escape hatch (`encryption::set_debug_plaintext`) opts back into plaintext.
fn redacting_debug_impl(
    struct_name: &syn::Ident,
    field_idents: &[&syn::Ident],
    encrypted_names: &[&str],
    classified_names: &[&str],
) -> TokenStream {
    let stmts = field_idents.iter().map(|ident| {
        let nm = ident.to_string();
        if classified_names.contains(&nm.as_str()) {
            // #1654: the read model's column is already a `Classified<..>` with a
            // redacting `Debug`, but `New*`/`Update*`/`Changeset` hold the plain
            // `String` (the write path is not a gated sink), and their `Debug`
            // reaches panic output and error pages just the same.
            quote! { s.field(#nm, &::core::format_args!("<classified>")); }
        } else if encrypted_names.contains(&nm.as_str()) {
            quote! {
                if ::autumn_web::encryption::debug_plaintext_enabled() {
                    s.field(#nm, &self.#ident);
                } else {
                    s.field(#nm, &::core::format_args!("<encrypted>"));
                }
            }
        } else {
            quote! { s.field(#nm, &self.#ident); }
        }
    });
    quote! {
        impl ::core::fmt::Debug for #struct_name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                let mut s = f.debug_struct(stringify!(#struct_name));
                #(#stmts)*
                s.finish()
            }
        }
    }
}

/// The `serialize_as`/`deserialize_as` wrapper path for an encrypted mode.
fn encrypted_wrapper_path(mode: EncryptedMode) -> Option<TokenStream> {
    match mode {
        EncryptedMode::None => None,
        EncryptedMode::Randomized => Some(quote! { ::autumn_web::encryption::RandomizedText }),
        EncryptedMode::Deterministic => {
            Some(quote! { ::autumn_web::encryption::DeterministicText })
        }
    }
}

/// Validate that `#[encrypted]` is only applied to a plain `String` field.
///
/// v1 supports non-null `String` columns (the realistic targets: tokens, SSNs,
/// emails). `Option<String>` and other types are rejected with a clear message.
fn validate_encrypted_field(field: &syn::Field) -> syn::Result<()> {
    if !parse_field_encrypted(field)?.is_encrypted() {
        return Ok(());
    }
    let is_string = matches!(&field.ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "String"));
    if !is_string {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[encrypted]` is only supported on non-null `String` fields in v1 \
             (encrypt before storing structured/optional data)",
        ));
    }
    // `#[encrypted]` columns must flow through the `serialize_as` wrapper on
    // insert. Fields excluded from the insert (`#[id]`, `#[default]`,
    // `#[lock_version]`, `#[position]`) would instead get a raw database
    // value, which the decrypting reader then rejects as a malformed
    // envelope. Reject the combo.
    if has_attr(field, "default")
        || has_attr(field, "lock_version")
        || has_attr(field, "id")
        || has_attr(field, "position")
    {
        return Err(syn::Error::new_spanned(
            field,
            "`#[encrypted]` cannot be combined with `#[default]`, `#[lock_version]`, \
             or `#[id]`: those fields bypass the insert path, so the column would \
             store an unencrypted value. Set the encrypted value explicitly on insert.",
        ));
    }
    // Full-text search builds the stored `search_vector` from the database column
    // value, which for an encrypted field is ciphertext. Indexing/querying that
    // would match envelope tokens, not the plaintext, so the repository's `search`
    // would silently miss encrypted content. Reject the combination.
    if has_attr(field, "searchable") {
        return Err(syn::Error::new_spanned(
            field,
            "`#[encrypted]` cannot be combined with `#[searchable]`: full-text search \
             indexes the stored column, which holds ciphertext, so plaintext searches \
             would never match. Remove `#[searchable]` from the encrypted field (keep a \
             separate non-encrypted column if you need to search).",
        ));
    }
    // The encrypted column is registered under its Rust field name, which the
    // log-scrub / version-history / admin compositions match against the
    // serde-serialized key. A `#[serde(rename)]` would desync those, leaking the
    // renamed plaintext (e.g. into version history). Reject it in v1.
    if field_has_serde_rename(field) {
        return Err(syn::Error::new_spanned(
            field,
            "`#[encrypted]` fields cannot use `#[serde(rename = ...)]` in v1: the \
             column is registered under its Rust name, which must match the \
             serialized key used by version history / log scrubbing / admin redaction.",
        ));
    }
    Ok(())
}

// ── #1384: `#[translatable]` field attribute ─────────────────────────────────

/// Validate a `#[translatable]` field.
///
/// The attribute is a marker: the *type* carries the behaviour, so the type
/// has to be right. Everything else rejected here is a combination whose two
/// halves disagree about what the column contains — a JSON container is not a
/// string to encrypt, index, normalize, or full-text search.
fn validate_translatable_field(field: &syn::Field) -> syn::Result<()> {
    if !field_is_translatable(field) {
        return Ok(());
    }
    // The type must be `Translated` (however it is spelled: bare, or fully
    // qualified through any path). `Option<Translated>` is rejected on
    // purpose — the container already models "no translation" as an empty
    // map, and a nullable column would give two spellings for one state.
    let is_translated = matches!(
        &field.ty,
        syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Translated")
            && p.path.segments.last().is_some_and(|s| s.arguments.is_empty())
    );
    if !is_translated {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[translatable]` requires the field type `autumn_web::i18n::Translated` \
             (a per-locale container), not a plain string. Change the field to \
             `pub <name>: autumn_web::i18n::Translated`; it renders the active \
             locale through `Display` and keeps every other locale intact on write. \
             The check is syntactic — a type alias for `Translated` is not \
             recognised, and conversely any type whose last path segment is \
             `Translated` is accepted, so spell the real type here.",
        ));
    }
    // Combinations whose two halves disagree about the column's contents.
    for (marker, why) in [
        (
            "encrypted",
            "an encrypted column stores one opaque ciphertext envelope, which has no \
             per-locale structure to resolve",
        ),
        (
            "searchable",
            "full-text search indexes the stored column, which for a translatable field \
             is a JSON container — the index would match locale tags and JSON \
             punctuation, not the prose",
        ),
        (
            "normalize",
            "normalizers rewrite a single string; they cannot see inside the per-locale \
             container",
        ),
        (
            "unique",
            "uniqueness would compare whole JSON containers, so two records translated \
             into different locale sets would never collide even with identical text",
        ),
        (
            "indexed",
            "an equality index over a JSON container matches whole containers, never a \
             single locale's value",
        ),
        ("id", "a primary key must be a single scalar value"),
        (
            "lock_version",
            "the optimistic-lock column is framework-managed and must stay a plain integer",
        ),
        (
            "position",
            "the position column is framework-managed and must stay a plain integer",
        ),
        (
            "state_machine",
            "a state column must hold one state name, not a per-locale container",
        ),
    ] {
        if has_attr(field, marker) {
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "`#[translatable]` cannot be combined with `#[{marker}]`: {why}. \
                     Keep a separate non-translatable column for that."
                ),
            ));
        }
    }
    // The column is registered under its Rust field name, which the registry
    // and the generated field-name-keyed accessors match against. A
    // `#[serde(rename)]` would desync them.
    if field_has_serde_rename(field) {
        return Err(syn::Error::new_spanned(
            field,
            "`#[translatable]` fields cannot use `#[serde(rename = ...)]`: the column is \
             registered under its Rust name, which must match the field name passed to \
             `available_locales(..)` / `is_translated(..)`.",
        ));
    }
    // `#[diesel(column_name = ...)]` is the spelling that actually renames the
    // *database* column, so it desyncs the registry harder than a serde rename:
    // `TranslatableColumnDescriptor` would name a column that does not exist on
    // the table. Refuse it for the same reason, and say which one.
    if field_has_diesel_column_name(field) {
        return Err(syn::Error::new_spanned(
            field,
            "`#[translatable]` fields cannot use `#[diesel(column_name = ...)]`: the column \
             is registered for framework surfaces under its Rust name, so a renamed database \
             column would be advertised under a name that does not exist on the table. Name \
             the Rust field after the column instead.",
        ));
    }
    Ok(())
}

/// An identifier's name with any raw-identifier prefix removed (`r#type` ->
/// `type`), matching the DB column and the key callers pass to the
/// field-name-driven accessors.
fn unraw_ident(ident: &syn::Ident) -> String {
    let raw = ident.to_string();
    raw.strip_prefix("r#").unwrap_or(&raw).to_owned()
}

/// Whether a field carries `#[diesel(column_name = ...)]`, which renames the
/// database column out from under the Rust field name.
fn field_has_diesel_column_name(field: &syn::Field) -> bool {
    let mut found = false;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("diesel")) {
        // Swallow any parse error: this is a detector, not a validator —
        // diesel's own derive reports malformed input with a better message.
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("column_name") {
                found = true;
            }
            let _ = meta
                .value()
                .and_then(syn::parse::ParseBuffer::parse::<syn::Expr>);
            Ok(())
        });
    }
    found
}

/// Build the `impl` block a model's `#[translatable]` fields contribute:
/// per-field accessors plus the field-name-keyed surface an app renders a
/// "needs translation" affordance from (issue #1384 AC5).
///
/// Returns an empty token stream when the model has no translatable field, so
/// a model that never opts in expands byte-for-byte as before.
fn emit_translatable_items(model: &syn::Ident, fields: &[&syn::Ident]) -> TokenStream {
    if fields.is_empty() {
        return quote! {};
    }
    // Strip the raw-identifier prefix (the house idiom, see `pascal_case`):
    // `Ident`'s `Display` keeps `r#`, so a `#[translatable] pub r#type:
    // Translated` field would be keyed as `"r#type"` — a name no `SELECT`
    // resolves and no caller of `available_locales("type")` would ever pass.
    let names: Vec<String> = fields.iter().map(|f| unraw_ident(f)).collect();
    let per_field = fields.iter().map(|ident| {
        let name = ident.to_string();
        let localized = format_ident!("{}_localized", ident);
        let in_locale = format_ident!("{}_in", ident);
        let setter = format_ident!("set_{}", ident);
        let locales = format_ident!("{}_locales", ident);
        let is_translated = format_ident!("{}_is_translated", ident);
        let doc_localized = format!(
            "`{name}` resolved against the request's active locale, walking the \
             configured fallback chain on miss. `None` once the chain is exhausted."
        );
        let doc_in =
            format!("`{name}` resolved against an explicit locale, then the fallback chain.");
        let doc_set = format!(
            "Store `value` as `{name}`'s translation for `locale`, leaving every other \
             locale untouched."
        );
        let doc_locales = format!("Every locale `{name}` has been translated into, in tag order.");
        let doc_is_translated = format!("Whether `{name}` has its own translation for `locale`.");
        quote! {
            #[doc = #doc_localized]
            #[must_use]
            pub fn #localized(&self) -> ::core::option::Option<&str> {
                self.#ident.resolve()
            }

            #[doc = #doc_in]
            #[must_use]
            pub fn #in_locale(&self, locale: &str) -> ::core::option::Option<&str> {
                self.#ident.resolve_in(locale)
            }

            #[doc = #doc_set]
            pub fn #setter(
                &mut self,
                locale: impl ::core::convert::Into<::std::string::String>,
                value: impl ::core::convert::Into<::std::string::String>,
            ) -> &mut Self {
                self.#ident.set(locale, value);
                self
            }

            #[doc = #doc_locales]
            #[must_use]
            pub fn #locales(&self) -> ::std::vec::Vec<&str> {
                self.#ident.available_locales()
            }

            #[doc = #doc_is_translated]
            #[must_use]
            pub fn #is_translated(&self, locale: &str) -> bool {
                self.#ident.is_translated(locale)
            }
        }
    });
    let match_arms = fields.iter().zip(names.iter()).map(|(ident, name)| {
        quote! { #name => ::core::option::Option::Some(&self.#ident), }
    });
    quote! {
        impl #model {
            #(#per_field)*

            /// Field names on this model declared `#[translatable]`.
            #[must_use]
            pub const fn translatable_fields() -> &'static [&'static str] {
                Self::__AUTUMN_TRANSLATABLE_COLUMNS
            }

            /// The raw per-locale container for `field`, or `None` when the
            /// model has no translatable field by that name.
            #[must_use]
            pub fn translated(
                &self,
                field: &str,
            ) -> ::core::option::Option<&::autumn_web::i18n::Translated> {
                match field {
                    #(#match_arms)*
                    _ => ::core::option::Option::None,
                }
            }

            /// Which locales `field` has been translated into — the
            /// "needs translation" affordance. Empty for an unknown field.
            #[must_use]
            pub fn available_locales(&self, field: &str) -> ::std::vec::Vec<&str> {
                self.translated(field)
                    .map(::autumn_web::i18n::Translated::available_locales)
                    .unwrap_or_default()
            }

            /// Whether `field` has its own translation for `locale`.
            #[must_use]
            pub fn is_translated(&self, field: &str, locale: &str) -> bool {
                self.translated(field)
                    .is_some_and(|t| t.is_translated(locale))
            }

            /// `field` resolved against the request's active locale.
            #[must_use]
            pub fn localized(&self, field: &str) -> ::core::option::Option<&str> {
                self.translated(field)
                    .and_then(::autumn_web::i18n::Translated::resolve)
            }
        }
    }
}

// ── #1379: `#[normalize(...)]` field normalization ────────────────────────

/// One normalizer step from a `#[normalize(...)]` attribute, applied
/// left-to-right in declaration order.
#[derive(Clone)]
enum Normalizer {
    /// `trim` — strip leading/trailing whitespace.
    Trim,
    /// `downcase` — lowercase (str casing).
    Downcase,
    /// `upcase` — uppercase (str casing).
    Upcase,
    /// `squish` — trim and collapse internal whitespace runs to one space.
    Squish,
    /// `strip_nul` — remove every NUL (`U+0000`), which a Postgres
    /// `TEXT`/`VARCHAR` column cannot store (#2423).
    StripNul,
    /// `with = path::to::fn` — user escape hatch (`fn(&str) -> String`).
    With(syn::Path),
}

/// Parse a field's
/// `#[normalize(trim, downcase, upcase, squish, strip_nul, with = path)]`
/// attribute into an ordered list of normalizers. Returns an empty list when
/// the field has no `#[normalize]` attribute.
fn parse_field_normalize(field: &syn::Field) -> syn::Result<Vec<Normalizer>> {
    let mut ops = Vec::new();
    for attr in &field.attrs {
        if !attr.path().is_ident("normalize") {
            continue;
        }
        // Bare `#[normalize]` and empty `#[normalize()]` are both errors: each
        // would otherwise register an identity no-op that silently does nothing.
        let is_empty = match &attr.meta {
            syn::Meta::Path(_) => true,
            syn::Meta::List(list) => list.tokens.is_empty(),
            syn::Meta::NameValue(_) => false,
        };
        if is_empty {
            return Err(syn::Error::new_spanned(
                attr,
                "`#[normalize]` requires at least one normalizer, e.g. \
                 `#[normalize(trim, downcase)]`",
            ));
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("trim") {
                ops.push(Normalizer::Trim);
            } else if meta.path.is_ident("downcase") {
                ops.push(Normalizer::Downcase);
            } else if meta.path.is_ident("upcase") {
                ops.push(Normalizer::Upcase);
            } else if meta.path.is_ident("squish") {
                ops.push(Normalizer::Squish);
            } else if meta.path.is_ident("strip_nul") {
                ops.push(Normalizer::StripNul);
            } else if meta.path.is_ident("with") {
                let path: syn::Path = meta.value()?.parse()?;
                ops.push(Normalizer::With(path));
            } else {
                return Err(meta.error(
                    "unsupported `#[normalize]` option; expected one of \
                     `trim`, `downcase`, `upcase`, `squish`, `strip_nul`, or \
                     `with = path`",
                ));
            }
            Ok(())
        })?;
    }
    Ok(ops)
}

/// Whether a field carries a `#[normalize(...)]` attribute.
fn field_has_normalize(field: &syn::Field) -> bool {
    has_attr(field, "normalize")
}

/// Validate that `#[normalize]` is only applied to a plain `String` field
/// (mirrors the `#[encrypted]` non-`String` diagnostic; #1379 AC7). Also
/// surfaces malformed-option errors early.
fn validate_normalize_field(field: &syn::Field) -> syn::Result<()> {
    if !field_has_normalize(field) {
        return Ok(());
    }
    // Surface option-parse errors (e.g. empty `#[normalize]`, bad option).
    parse_field_normalize(field)?;
    let is_string = matches!(&field.ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "String"));
    if !is_string {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[normalize]` is only supported on non-null `String` fields \
             (normalize a `String` column; `Option<String>` and other types are \
             out of scope in this slice)",
        ));
    }
    Ok(())
}

/// Emit an expression that normalizes `value_expr` (an owned `String`) through
/// the field's normalizer chain, left-to-right. Each built-in is a
/// `fn(&str) -> String` in `autumn_web::normalize`; `with = path` calls the
/// user function with the same signature.
fn emit_normalize_expr(ops: &[Normalizer], value_expr: &TokenStream) -> TokenStream {
    let steps = ops.iter().map(|op| match op {
        Normalizer::Trim => quote! { __autumn_n = ::autumn_web::normalize::trim(&__autumn_n); },
        Normalizer::Downcase => {
            quote! { __autumn_n = ::autumn_web::normalize::downcase(&__autumn_n); }
        }
        Normalizer::Upcase => {
            quote! { __autumn_n = ::autumn_web::normalize::upcase(&__autumn_n); }
        }
        Normalizer::Squish => {
            quote! { __autumn_n = ::autumn_web::normalize::squish(&__autumn_n); }
        }
        Normalizer::StripNul => {
            quote! { __autumn_n = ::autumn_web::normalize::strip_nul(&__autumn_n); }
        }
        Normalizer::With(path) => quote! { __autumn_n = #path(&__autumn_n); },
    });
    quote! {{
        let mut __autumn_n: ::std::string::String = #value_expr;
        #(#steps)*
        __autumn_n
    }}
}

/// Whether any attribute is a struct-level `#[serde(rename_all = "...")]`.
fn attrs_have_serde_rename_all(attrs: &[syn::Attribute]) -> bool {
    let mut found = false;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                found = true;
            }
            if let Ok(value) = meta.value() {
                let _: syn::Result<syn::Lit> = value.parse();
            }
            Ok(())
        });
    }
    found
}

/// Whether a field carries a `#[serde(rename = "...")]` (which would desync the
/// encrypted-column registry from the serialized key).
fn field_has_serde_rename(field: &syn::Field) -> bool {
    let mut renamed = false;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                renamed = true;
            }
            // Consume any `= value` so sibling metas keep parsing.
            if let Ok(value) = meta.value() {
                let _: syn::Result<syn::Lit> = value.parse();
            }
            Ok(())
        });
    }
    renamed
}

/// Parse the struct-level language dictionary configuration from `#[searchable(language = "...")]`
fn parse_model_searchable_lang(attrs: &[syn::Attribute]) -> syn::Result<Option<String>> {
    for attr in attrs {
        if attr.path().is_ident("searchable") {
            if matches!(attr.meta, syn::Meta::Path(_)) {
                return Ok(Some("simple".to_string()));
            }
            let mut lang = None;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("language") {
                    let value: syn::LitStr = meta.value()?.parse()?;
                    lang = Some(value.value());
                    Ok(())
                } else {
                    Err(meta.error("unsupported searchable attribute"))
                }
            })?;
            return Ok(Some(lang.unwrap_or_else(|| "simple".to_string())));
        }
    }
    Ok(None)
}

/// Parse `#[shard_key = "field_name"]` from struct-level outer attributes.
///
/// Returns `Some(field_name)` when the attribute is present, `None` otherwise.
/// The named field must exist on the model struct; validation happens after
/// `all_fields` is constructed in `model_macro`.
fn parse_model_shard_key(attrs: &[syn::Attribute]) -> syn::Result<Option<String>> {
    for attr in attrs {
        if attr.path().is_ident("shard_key") {
            let syn::Meta::NameValue(ref nv) = attr.meta else {
                return Err(syn::Error::new_spanned(
                    attr,
                    "shard_key attribute requires a string value: #[shard_key = \"field\"]",
                ));
            };
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(ref lit_str),
                ..
            }) = nv.value
            else {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "shard_key value must be a string literal",
                ));
            };
            return Ok(Some(lit_str.value()));
        }
    }
    Ok(None)
}

#[derive(Debug)]
enum FieldSearchable {
    NotSearchable,
    SearchableDefault,
    SearchableWithWeight(String),
}

/// Field-level `#[searchable(...)]` configuration.
///
/// #842 introduced the weight; #1191 adds `embed`, which nominates the field
/// whose text is embedded for vector / "find similar" search. The two are
/// independent: `#[searchable(weight = "B", embed)]` both ranks the field at
/// weight B for keyword search and embeds it for k-NN search.
#[derive(Debug)]
struct FieldSearchableConfig {
    kind: FieldSearchable,
    embed: bool,
}

/// Whether `ty` is `String` or `Option<String>` — the shape the tenancy path
/// assumes for a `tenant_id` column.
fn is_string_or_option_string(ty: &syn::Type) -> bool {
    fn is_string(ty: &syn::Type) -> bool {
        matches!(ty, syn::Type::Path(tp) if tp.path.segments.last()
            .is_some_and(|s| s.ident == "String"))
    }
    if is_string(ty) {
        return true;
    }
    let syn::Type::Path(tp) = ty else {
        return false;
    };
    let Some(segment) = tp.path.segments.last() else {
        return false;
    };
    if segment.ident != "Option" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return false;
    };
    matches!(args.args.first(), Some(syn::GenericArgument::Type(inner)) if is_string(inner))
}

/// Parse the full field-level `#[searchable(weight = "...", embed)]` config.
fn parse_field_searchable(field: &syn::Field) -> syn::Result<FieldSearchableConfig> {
    for attr in &field.attrs {
        if attr.path().is_ident("searchable") {
            if matches!(attr.meta, syn::Meta::Path(_)) {
                return Ok(FieldSearchableConfig {
                    kind: FieldSearchable::SearchableDefault,
                    embed: false,
                });
            }
            let mut weight = None;
            let mut embed = false;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("weight") {
                    let value: syn::LitStr = meta.value()?.parse()?;
                    weight = Some(value.value());
                    Ok(())
                } else if meta.path.is_ident("embed") {
                    if embed {
                        return Err(meta.error("duplicate `embed` in #[searchable(...)]"));
                    }
                    embed = true;
                    Ok(())
                } else {
                    Err(meta.error(
                        "unsupported field searchable attribute (expected `weight = \"A\"` or `embed`)",
                    ))
                }
            })?;
            return Ok(FieldSearchableConfig {
                kind: weight.map_or(
                    FieldSearchable::SearchableDefault,
                    FieldSearchable::SearchableWithWeight,
                ),
                embed,
            });
        }
    }
    Ok(FieldSearchableConfig {
        kind: FieldSearchable::NotSearchable,
        embed: false,
    })
}

#[derive(Clone, Copy)]
enum SerdeAdapterMode {
    Serialize,
    Deserialize,
}

#[derive(Default)]
struct SerdeAdapterAttrs {
    with: Option<LitStr>,
    serialize_with: Option<LitStr>,
    deserialize_with: Option<LitStr>,
}

fn serde_adapter_attrs(field: &Field) -> SerdeAdapterAttrs {
    let mut adapters = SerdeAdapterAttrs::default();
    for attr in field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("serde"))
    {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("with") {
                adapters.with = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("serialize_with") {
                adapters.serialize_with = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("deserialize_with") {
                adapters.deserialize_with = Some(meta.value()?.parse()?);
            }
            Ok(())
        });
    }
    adapters
}

fn hook_serde_adapter_attrs(field: &Field, mode: SerdeAdapterMode) -> Vec<TokenStream> {
    let adapters = serde_adapter_attrs(field);
    let mut entries = Vec::new();
    if let Some(with) = adapters.with {
        entries.push(quote! { with = #with });
    }
    match mode {
        SerdeAdapterMode::Serialize => {
            if let Some(serialize_with) = adapters.serialize_with {
                entries.push(quote! { serialize_with = #serialize_with });
            }
        }
        SerdeAdapterMode::Deserialize => {
            if let Some(deserialize_with) = adapters.deserialize_with {
                entries.push(quote! { deserialize_with = #deserialize_with });
            }
        }
    }

    if entries.is_empty() {
        Vec::new()
    } else {
        vec![quote! { #[serde(#(#entries),*)] }]
    }
}

fn has_hook_serde_adapter(field: &Field, mode: SerdeAdapterMode) -> bool {
    let adapters = serde_adapter_attrs(field);
    adapters.with.is_some()
        || match mode {
            SerdeAdapterMode::Serialize => adapters.serialize_with.is_some(),
            SerdeAdapterMode::Deserialize => adapters.deserialize_with.is_some(),
        }
}

enum SerdeDefaultKind {
    Default,
    Path(syn::Path),
}

fn serde_default_kind(field: &Field) -> Option<SerdeDefaultKind> {
    let mut default = None;
    for attr in field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("serde"))
    {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") {
                if meta.input.peek(syn::Token![=]) {
                    let value: LitStr = meta.value()?.parse()?;
                    if let Ok(path) = value.parse::<syn::Path>() {
                        default = Some(SerdeDefaultKind::Path(path));
                    }
                } else {
                    default = Some(SerdeDefaultKind::Default);
                }
            }
            Ok(())
        });
    }
    default
}

fn commit_hook_missing_field_default_expr(field: &Field) -> Option<TokenStream> {
    match serde_default_kind(field) {
        Some(SerdeDefaultKind::Default) => Some(quote! { ::core::default::Default::default() }),
        Some(SerdeDefaultKind::Path(path)) => Some(quote! { #path() }),
        None if is_option_type(&field.ty) => Some(quote! { ::core::option::Option::None }),
        None => None,
    }
}

/// Extract the associated model type from `#[factory_assoc(TypeName)]` if present.
///
/// Returns `Some(Ident)` for the associated type, or `None` if the attribute is absent.
/// Panics if `factory_assoc` is present but fails to parse — callers should run
/// `validate_factory_assoc_attrs` first to surface a proper compile error.
fn factory_assoc_type(field: &Field) -> Option<syn::Ident> {
    for attr in &field.attrs {
        if attr.path().is_ident("factory_assoc")
            && let Ok(ident) = attr.parse_args::<syn::Ident>()
        {
            return Some(ident);
        }
    }
    None
}

/// The identifier of a type's last path segment (e.g. `String`, `i64`,
/// `DateTime`, `Uuid`), if the type is a simple path type.
fn ty_last_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// The identifier of the first generic type argument of a path type's last
/// segment (e.g. `Utc` for `DateTime<Utc>`, `String` for `Vec<String>`).
fn ty_last_generic_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(tp) = ty {
        let seg = tp.path.segments.last()?;
        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
            for arg in &args.args {
                if let syn::GenericArgument::Type(inner) = arg {
                    return ty_last_ident(inner);
                }
            }
        }
    }
    None
}

/// The inner type `T` of an `Option<T>`, if `ty` is an `Option`.
fn option_inner_type(ty: &syn::Type) -> Option<&syn::Type> {
    if let syn::Type::Path(tp) = ty {
        let seg = tp.path.segments.last()?;
        if seg.ident != "Option" {
            return None;
        }
        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
            for arg in &args.args {
                if let syn::GenericArgument::Type(inner) = arg {
                    return Some(inner);
                }
            }
        }
    }
    None
}

/// Infer the fake-data expression for a factory field when `.fake()` is active.
///
/// Selection order (per issue #1343):
/// 1. **Name-based rules** — for `String` targets, the field identifier
///    (case-insensitive, `contains` unless noted) picks a specialized generator
///    (`email`, `url`, `title`, `body`, `slug`, …). Numeric/temporal/decimal
///    fields already map cleanly from their type, so name hints for them
///    coincide with the type fallback and need no special-casing.
/// 2. **Type fallback** — the Rust type of the field maps to the natural
///    generator (`String` → `sentence()`, integers → `int_range`, `bool` →
///    `boolean()`, `Decimal` → `decimal()`, `DateTime` → `recent_datetime()`,
///    `Uuid` → `uuid()`, …).
/// 3. `Option<T>` wraps the inner expression in `Some(..)`.
///
/// Returns `None` when no sensible fake value can be produced — the caller then
/// leaves the field at its `Default::default()` value. This function must NEVER
/// emit an expression that fails to compile: when unsure, return `None`.
fn fake_expr_for_field(ident: &syn::Ident, ty: &syn::Type) -> Option<TokenStream> {
    let raw = ident.to_string();
    let name = raw.strip_prefix("r#").unwrap_or(&raw).to_ascii_lowercase();

    // Option<T>: fake the inner value, wrap in Some.
    if let Some(inner) = option_inner_type(ty) {
        let inner_expr = fake_expr_core(&name, inner)?;
        return Some(quote! { ::core::option::Option::Some(#inner_expr) });
    }

    fake_expr_core(&name, ty)
}

/// Core inference over a non-`Option` target type. See [`fake_expr_for_field`].
fn fake_expr_core(name: &str, ty: &syn::Type) -> Option<TokenStream> {
    let last = ty_last_ident(ty)?;
    match last.as_str() {
        "String" => Some(fake_string_expr(name)),
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "u128" | "i128" | "usize"
        | "isize" => {
            let cast = format_ident!("{last}");
            // Default numeric range covers the name-based hints (count/age/qty →
            // small non-negative ints). `int_range` works in `i64`, so pick an
            // upper bound that both fits `i64` and stays inside the target type:
            // for the narrow `i8`/`u8` types the default 1000 would overflow the
            // `as` cast (`1000 as u8 == 232`, `1000 as i8 == -24`), so clamp to
            // that type's own maximum. All wider types keep the 1000 default.
            let hi: i64 = match last.as_str() {
                "i8" => i64::from(i8::MAX),
                "u8" => i64::from(u8::MAX),
                _ => 1000,
            };
            Some(quote! { (::autumn_web::fake::int_range(0, #hi) as #cast) })
        }
        "f32" | "f64" => {
            let cast = format_ident!("{last}");
            Some(quote! { (::autumn_web::fake::decimal_f64() as #cast) })
        }
        "bool" => Some(quote! { ::autumn_web::fake::boolean() }),
        "Decimal" => Some(quote! { ::autumn_web::fake::decimal() }),
        "Uuid" => Some(quote! { ::autumn_web::fake::uuid() }),
        // The SQLite newtypes (issue #1924) wrap exactly those values. Without
        // these arms every faked row falls back to `Default` — one shared nil
        // UUID, which collides on a `:unique` column the first time twice.
        "SqliteDecimal" => Some(quote! { ::autumn_web::fake::decimal().into() }),
        "SqliteUuid" => Some(quote! { ::autumn_web::fake::uuid().into() }),
        // `recent_datetime()` yields `DateTime<Utc>`, so only fake a `DateTime`
        // whose timezone parameter is `Utc`. Other zones (e.g. `Local`,
        // `FixedOffset`) fall through to Default to avoid a type mismatch.
        "DateTime" if ty_last_generic_ident(ty).as_deref() == Some("Utc") => {
            Some(quote! { ::autumn_web::fake::recent_datetime() })
        }
        "NaiveDateTime" => Some(quote! { ::autumn_web::fake::recent_datetime().naive_utc() }),
        "NaiveDate" => Some(quote! { ::autumn_web::fake::recent_datetime().date_naive() }),
        _ => None,
    }
}

/// Choose a string generator from the field name (name-based rules for #1343).
fn fake_string_expr(name: &str) -> TokenStream {
    if name.contains("email") {
        quote! { ::autumn_web::fake::email() }
    } else if name == "username" || name.contains("user_name") {
        quote! { ::autumn_web::fake::username() }
    } else if name.contains("url") || name.contains("link") || name.contains("website") {
        quote! { ::autumn_web::fake::url() }
    } else if name.contains("slug") {
        quote! {{
            let __autumn_slug = ::autumn_web::fake::words(3);
            __autumn_slug
                .split_whitespace()
                .collect::<::std::vec::Vec<&str>>()
                .join("-")
        }}
    } else if name.contains("body")
        || name.contains("content")
        || name.contains("description")
        || name.contains("summary")
        || name.contains("bio")
        || name.contains("text")
    {
        quote! { ::autumn_web::fake::paragraph() }
    } else if name.contains("first_name") || name.contains("firstname") {
        quote! { ::autumn_web::fake::first_name() }
    } else if name.contains("last_name") || name.contains("lastname") || name.contains("surname") {
        quote! { ::autumn_web::fake::last_name() }
    } else if name == "name" {
        quote! { ::autumn_web::fake::name() }
    } else if name.contains("title") || name.contains("name") {
        quote! { ::autumn_web::fake::words(3) }
    } else {
        quote! { ::autumn_web::fake::sentence() }
    }
}

/// Validate that every `#[factory_assoc(...)]` attribute contains a valid Ident.
///
/// Returns a compile error token stream on the first malformed attribute so the
/// user gets a clear diagnostic instead of silent fallback-to-normal-field behavior.
fn validate_factory_assoc_attrs(fields: &[&Field]) -> Option<TokenStream> {
    for field in fields {
        for attr in &field.attrs {
            if attr.path().is_ident("factory_assoc") {
                // Reject unparseable attribute argument.
                if let Err(err) = attr.parse_args::<syn::Ident>() {
                    return Some(err.to_compile_error());
                }
                // Reject Option<T> fields — the factory uses Option<T> itself to
                // represent "not yet set vs. explicit value", so Option<Option<T>>
                // would be generated, leading to an arm-type mismatch in create().
                if is_option_type(&field.ty) {
                    return Some(
                        syn::Error::new_spanned(
                            attr,
                            "#[factory_assoc] cannot be applied to an Option<T> field; \
                             factory_assoc is designed for non-nullable FK fields (e.g. i64). \
                             Use a plain field setter to supply a nullable association.",
                        )
                        .to_compile_error(),
                    );
                }
            }
        }
    }
    None
}

/// True if a field has `#[id]`, `#[default]`, `#[lock_version]`, or
/// `#[position]` — all four are excluded from the `NewX` insert type (and,
/// via `fields_for_new`, from `UpdateX` too — `#[lock_version]` is the one
/// exception, re-added to `UpdateX` separately as a plain required field).
///
/// `#[lock_version]` fields are excluded because the DB column must carry a
/// `DEFAULT 0` constraint; the initial version is always zero and is never
/// supplied by the caller on insert. `#[position]` fields (issue #1358) are
/// excluded because the generated repository assigns the next contiguous
/// value on insert and only ever changes it through `move_to`/`move_before`/
/// `move_after`/`move_up`/`move_down` — never a direct create/update payload,
/// which would let a caller silently break the contiguous invariant.
fn excluded_from_new(field: &Field) -> bool {
    has_attr(field, "id")
        || has_attr(field, "default")
        || has_attr(field, "lock_version")
        || has_attr(field, "position")
}

/// Convert a `snake_case` identifier to `PascalCase`.
fn pascal_case(s: &str) -> String {
    // Strip the raw-identifier prefix so `r#type` produces `Type`, not `R#type`.
    let s = s.strip_prefix("r#").unwrap_or(s);
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().to_string() + &chars.collect::<String>()
            })
        })
        .collect()
}

/// Humanize a `snake_case` field name into a `<label>`-friendly title
/// (e.g. `published_at` -> `"Published At"`). Mirrors the scaffold's
/// `humanize_label` so a derived form and a hand-written one read alike.
fn humanize_field_label(name: &str) -> String {
    let name = name.strip_prefix("r#").unwrap_or(name);
    name.split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().to_string() + &chars.collect::<String>()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Emit the `::autumn_web::form::FieldControl` expression for a model field,
/// derived from its (Option-unwrapped) Rust type. `nullable` is `true` for
/// `Option<...>` fields.
///
/// The mapping mirrors the scaffold's `render_changeset_form_inputs` control
/// selection (#1131): strings/UUID -> text, integers -> stepped number,
/// floats/decimals -> free-step number, `bool` -> checkbox (nullable `bool` ->
/// a tri-state select so `NULL` stays reachable), dates/datetimes -> the
/// corresponding pickers. Unrecognized types (e.g. user enums) fall back to a
/// plain text control; callers can promote them to a `Select` via
/// `FormFor::override_field` (which is exactly what the scaffold does for enum
/// columns, whose variants it knows statically).
fn form_control_tokens(inner_ty: &syn::Type, nullable: bool) -> TokenStream {
    let name = type_name_str(inner_ty);
    match name.as_str() {
        "bool" => {
            if nullable {
                quote! {
                    ::autumn_web::form::FieldControl::Select {
                        options: ::std::vec![
                            (::std::string::String::from(""), ::std::string::String::from("— Unset —")),
                            (::std::string::String::from("true"), ::std::string::String::from("Yes")),
                            (::std::string::String::from("false"), ::std::string::String::from("No")),
                        ],
                    }
                }
            } else {
                quote! { ::autumn_web::form::FieldControl::Checkbox }
            }
        }
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "usize" => quote! {
            ::autumn_web::form::FieldControl::Number {
                step: ::core::option::Option::Some(::std::string::String::from("1")),
            }
        },
        "f32" | "f64" | "Decimal" | "BigDecimal" | "SqliteDecimal" => quote! {
            ::autumn_web::form::FieldControl::Number {
                step: ::core::option::Option::Some(::std::string::String::from("any")),
            }
        },
        "NaiveDate" => quote! { ::autumn_web::form::FieldControl::Date },
        "NaiveDateTime" => quote! { ::autumn_web::form::FieldControl::DateTime },
        // `<input type="datetime-local">` posts an offsetless wall-clock value, so only
        // zone parameters with a sound interpretation of that shape get the picker:
        // `Utc` and the server's `Local`, each wired to a matching tolerant deserializer
        // (see `datetime_local_serde_attr`). Any other zone — `FixedOffset`, a chrono-tz
        // zone, or a bare `DateTime` alias whose zone the derive cannot see — falls back
        // to a text input, pre-filled with the field's serialized RFC 3339 string, which
        // chrono's default `Deserialize` round-trips as-is. Plainer, but honest, rather
        // than a picker whose submission 400s.
        "DateTime" => {
            let picker_zone = crate::api_doc::unwrap_single_generic(inner_ty, "DateTime")
                .is_some_and(|tz| matches!(type_name_str(&tz).as_str(), "Utc" | "Local"));
            if picker_zone {
                quote! { ::autumn_web::form::FieldControl::DateTime }
            } else {
                quote! { ::autumn_web::form::FieldControl::Text }
            }
        }
        // `String`/`str`/`Uuid` render as text, and so do unknown types
        // (user enums, JSON, custom newtypes) as a safe fallback; promote
        // via `.override_field(...)` where a richer control is known.
        _ => quote! { ::autumn_web::form::FieldControl::Text },
    }
}

/// For a `NewX` field whose column `form_for` renders as an HTML
/// `datetime-local` control (see `form_control_tokens`), emit the serde
/// attribute wiring the matching datetime-local-tolerant deserializer from
/// `autumn_web::form`.
///
/// `<input type="datetime-local">` posts an offsetless value (and not always
/// with seconds), which chrono's default `Deserialize` for `DateTime<Utc>`
/// rejects — so a `form_for` submission would 400 before validation even when
/// the pre-filled value is untouched. The referenced helpers accept both the
/// browser shape (`YYYY-MM-DDTHH:MM[:SS[.f]]`, interpreted as UTC for
/// `DateTime<Utc>` columns and as the server's local wall clock for
/// `DateTime<Local>` ones) *and* RFC 3339 (offset converted to the field's
/// zone), so JSON API create bodies posted to the same `NewX` keep decoding.
///
/// Returns `None` for non-datetime fields, and for `DateTime<Tz>` with a
/// zone parameter other than `Utc`/`Local` — those columns don't render a
/// `datetime-local` control in the first place (`form_control_tokens` falls
/// back to a text input whose RFC 3339 value chrono's default `Deserialize`
/// round-trips), so they keep that default.
fn datetime_local_serde_attr(ty: &syn::Type) -> Option<TokenStream> {
    let nullable = is_option_type(ty);
    let inner = if nullable {
        crate::api_doc::unwrap_single_generic(ty, "Option")?
    } else {
        ty.clone()
    };
    let base = match type_name_str(&inner).as_str() {
        "NaiveDateTime" => "deserialize_naive_datetime_local",
        "DateTime" => {
            let tz = crate::api_doc::unwrap_single_generic(&inner, "DateTime")?;
            match type_name_str(&tz).as_str() {
                "Utc" => "deserialize_datetime_local_utc",
                "Local" => "deserialize_datetime_local_local",
                _ => return None,
            }
        }
        _ => return None,
    };
    // `deserialize_with`'s value is a string serde parses into a path itself
    // (it never passes through this crate's own generic `::autumn_web`
    // token rewrite — see `crate_path`'s module doc, #1828), so it must be
    // built from the actively resolved crate name directly.
    let crate_root =
        crate::crate_path::escaped_target_path_segment(&crate::crate_path::current_target());
    if nullable {
        // `deserialize_with` disables serde's implicit missing-`Option`-field
        // -is-`None` handling; `default` restores it so a JSON body may still
        // omit the nullable column. (The `_option` helper itself maps a
        // present-but-empty form value to `None`.)
        let path = format!("::{crate_root}::form::{base}_option");
        Some(quote! { #[serde(default, deserialize_with = #path)] })
    } else {
        let path = format!("::{crate_root}::form::{base}");
        Some(quote! { #[serde(deserialize_with = #path)] })
    }
}

/// Emit `impl ::autumn_web::form::FormModel for #name` (issue #1135), listing
/// one `FormField` descriptor per user-editable column (the same
/// `fields_for_new` set the insertable `NewX` struct uses -- i.e. excluding the
/// primary key, `#[default]`, and `#[lock_version]` columns).
///
/// This is what lets `form_for::<T>(...)` render a whole form in one call: the
/// descriptor carries each field's name (the Rust identifier — the POST key
/// the generated `NewX`/`UpdateX` decode by), humanized label,
/// type-appropriate control, and `required` flag (derived from non-`Option`).
///
/// `rename_all_rule` is the struct-level `#[serde(rename_all = "...")]`
/// serialization rule, if any. When a column's serde-effective *serialized*
/// key differs from its identifier (field-level `#[serde(rename)]` wins over
/// `rename_all`, mirroring serde), the descriptor records that key as
/// `FormField::value_name` so `form_for`'s pre-fill lookup
/// (`Changeset::field_value`, which indexes the changeset's serialized data)
/// still finds the value — while the rendered input `name` stays the
/// identifier the insert struct expects.
fn emit_form_model_impl(
    name: &syn::Ident,
    fields_for_new: &[&&Field],
    rename_all_rule: Option<&str>,
) -> TokenStream {
    let field_exprs: Vec<TokenStream> = fields_for_new
        .iter()
        .filter_map(|f| {
            let ident = f.ident.as_ref()?;
            let field_name = ident.to_string();
            let field_name = field_name.strip_prefix("r#").unwrap_or(&field_name);
            let label = humanize_field_label(field_name);
            let nullable = is_option_type(&f.ty);
            let inner = crate::api_doc::unwrap_single_generic(&f.ty, "Option")
                .unwrap_or_else(|| f.ty.clone());
            let control = form_control_tokens(&inner, nullable);
            let required = !nullable;
            let serialized_name = field_serde_serialize_rename(f).or_else(|| {
                rename_all_rule.and_then(|rule| apply_serde_rename_all_rule(rule, field_name))
            });
            let value_name = serialized_name
                .filter(|serialized| serialized != field_name)
                .map(|serialized| quote! { .with_value_name(#serialized) });
            Some(quote! {
                ::autumn_web::form::FormField::new(
                    #field_name,
                    #label,
                    #control,
                    #required,
                )
                #value_name
            })
        })
        .collect();

    quote! {
        impl ::autumn_web::form::FormModel for #name {
            fn form_fields() -> ::std::vec::Vec<::autumn_web::form::FormField> {
                ::std::vec![
                    #(#field_exprs),*
                ]
            }
        }
    }
}

// ── State machine support ────────────────────────────────────────────────────

/// A single allowed transition between two named states.
struct StateMachineTransition {
    from: String,
    to: String,
    /// Optional guard: name of a `&self` bool method that must return `true`.
    guard: Option<String>,
    /// Optional after-commit effect (issue #1973): the path of a `#[job]`
    /// struct to enqueue transactionally when this specific edge fires.
    /// Declarable on both the inline `transitions(...)` path and the
    /// `lifecycle = <Enum>` path's `effects(...)` clause (issue #1973); both
    /// route through the same effect codegen.
    on_commit: Option<syn::Path>,
    /// Optional sync in-transaction effect (issue #1973): the name of an
    /// `async fn(&self, conn) -> AutumnResult<()>` method run inside the
    /// transition's transaction when this edge fires. `Err` rolls the
    /// transition back (mirrors `before_*` mutation hooks). Declarable on both
    /// the inline `transitions(...)` path and the `lifecycle = <Enum>` path's
    /// `effects(...)` clause.
    on: Option<String>,
}

/// The declared source of a field's transition table: either an inline
/// `transitions(...)` list (the original shorthand) or a reference to a
/// `#[lifecycle]` enum whose typed edges are the source of truth (issue #1911).
enum StateMachineSource {
    /// `#[state_machine(transitions(a -> b, ...))]`.
    Inline(Vec<StateMachineTransition>),
    /// `#[state_machine(lifecycle = SomeEnum)]` — transitions derived from the
    /// referenced `#[lifecycle]` enum's `Lifecycle::STATE_MACHINE_TRANSITIONS`.
    Lifecycle {
        path: syn::Path,
        /// Per-edge effects declared at the binding site via `effects(...)`.
        /// Empty when no clause is present (byte-identical to pre-#1973). Guards
        /// are rejected here — lifecycle transitions are unguarded.
        effects: Vec<StateMachineTransition>,
    },
}

/// Parsed `#[state_machine(...)]` spec for one field.
struct StateMachineSpec {
    field_ident: syn::Ident,
    source: StateMachineSource,
}

/// Validate that a guard name literal is a plain Rust identifier so that
/// `format_ident!` doesn't panic on names like `"can-ship"` or `"can ship"`.
fn validate_guard_ident(lit: &syn::LitStr) -> syn::Result<String> {
    let guard_str = lit.value();
    syn::parse_str::<syn::Ident>(&guard_str).map_err(|_| {
        syn::Error::new_spanned(
            lit,
            format!(
                "`{guard_str}` is not a valid Rust identifier; \
                 guard names must be a plain function name such as `can_ship`"
            ),
        )
    })?;
    Ok(guard_str)
}

/// Parse the inner edge list of a `transitions(...)` group (the surrounding
/// `transitions(` / `)` already consumed by the caller).
///
/// Each edge is `From -> To` with an optional per-edge suffix after `:`:
/// - the legacy bare-string guard shorthand `From -> To: "guard"`, or
/// - a `key = value` meta list `From -> To: guard = "guard", on = "handler",
///   on_commit = Job` (issue #1973), where `guard` names a `&self -> bool`
///   method, `on` names an `async fn(&self, conn) -> AutumnResult<()>` method
///   run in the transition's transaction (`Err` rolls it back), and
///   `on_commit` names a `#[job]` struct to enqueue transactionally when the
///   edge fires. Keys may appear in any order and are each optional.
fn parse_transition_list(
    content: syn::parse::ParseStream<'_>,
) -> syn::Result<Vec<StateMachineTransition>> {
    let mut transitions = Vec::new();
    while !content.is_empty() {
        let from: syn::Ident = content.parse()?;
        content.parse::<syn::Token![->]>()?;
        let to: syn::Ident = content.parse()?;
        let mut guard: Option<String> = None;
        let mut on_commit: Option<syn::Path> = None;
        let mut on: Option<String> = None;
        if content.peek(syn::Token![:]) {
            content.parse::<syn::Token![:]>()?;
            if content.peek(syn::LitStr) {
                // Legacy shorthand: `: "guard"`.
                let lit: syn::LitStr = content.parse()?;
                guard = Some(validate_guard_ident(&lit)?);
            } else {
                // New `key = value[, key = value]` meta list (issue #1973).
                loop {
                    let key: syn::Ident = content.parse()?;
                    content.parse::<syn::Token![=]>()?;
                    if key == "guard" {
                        if guard.is_some() {
                            return Err(syn::Error::new_spanned(
                                &key,
                                "duplicate `guard` on a single transition",
                            ));
                        }
                        let lit: syn::LitStr = content.parse()?;
                        guard = Some(validate_guard_ident(&lit)?);
                    } else if key == "on_commit" {
                        if on_commit.is_some() {
                            return Err(syn::Error::new_spanned(
                                &key,
                                "duplicate `on_commit` on a single transition",
                            ));
                        }
                        on_commit = Some(content.parse::<syn::Path>()?);
                    } else if key == "on" {
                        if on.is_some() {
                            return Err(syn::Error::new_spanned(
                                &key,
                                "duplicate `on` on a single transition",
                            ));
                        }
                        let lit: syn::LitStr = content.parse()?;
                        on = Some(validate_guard_ident(&lit)?);
                    } else {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "expected `guard = \"...\"`, `on = \"...\"`, or `on_commit = <Job>`",
                        ));
                    }
                    // Continue the meta list only when a `, <ident> =` follows
                    // (another key for this edge). A `, <State> ->` (or the end)
                    // belongs to the next edge, so leave the comma for the edge
                    // separator below.
                    if !content.peek(syn::Token![,]) {
                        break;
                    }
                    let ahead = content.fork();
                    ahead.parse::<syn::Token![,]>()?;
                    if ahead.peek(syn::Ident) && ahead.peek2(syn::Token![=]) {
                        content.parse::<syn::Token![,]>()?;
                    } else {
                        break;
                    }
                }
            }
        }
        transitions.push(StateMachineTransition {
            from: from.to_string(),
            to: to.to_string(),
            guard,
            on_commit,
            on,
        });
        if content.peek(syn::Token![,]) {
            content.parse::<syn::Token![,]>()?;
        }
    }
    Ok(transitions)
}

/// Parse the `#[state_machine(...)]` argument as either `transitions(...)` or
/// `lifecycle = <Path>`.
fn parse_state_machine_source(
    input: syn::parse::ParseStream<'_>,
) -> syn::Result<StateMachineSource> {
    let key: syn::Ident = input.parse()?;
    if key == "transitions" {
        let content;
        syn::parenthesized!(content in input);
        Ok(StateMachineSource::Inline(parse_transition_list(&content)?))
    } else if key == "lifecycle" {
        input.parse::<syn::Token![=]>()?;
        let path: syn::Path = input.parse()?;
        // Optionally consume a trailing `, effects(...)` clause declaring
        // per-edge effects at the binding site (issue #1973). Absent → no
        // effects (byte-identical to a plain `lifecycle = <Enum>`).
        let effects: Vec<StateMachineTransition> = if input.peek(syn::Token![,]) {
            input.parse::<syn::Token![,]>()?;
            let effects_kw: syn::Ident = input.parse()?;
            if effects_kw != "effects" {
                return Err(syn::Error::new(
                    effects_kw.span(),
                    "expected `effects(...)` after `lifecycle = <Enum>`",
                ));
            }
            let content;
            syn::parenthesized!(content in input);
            let parsed = parse_transition_list(&content)?;
            // Reject guards + require at least one effect per edge.
            for t in &parsed {
                if t.guard.is_some() {
                    return Err(syn::Error::new(
                        effects_kw.span(),
                        "guards are not supported on `lifecycle = <Enum>` effects; \
                         lifecycle transitions are unguarded (declare effects only)",
                    ));
                }
                if t.on.is_none() && t.on_commit.is_none() {
                    return Err(syn::Error::new(
                        effects_kw.span(),
                        "each `effects(...)` edge must declare `on = \"...\"` and/or `on_commit = <Job>`",
                    ));
                }
            }
            parsed
        } else {
            Vec::new()
        };
        Ok(StateMachineSource::Lifecycle { path, effects })
    } else {
        Err(syn::Error::new(
            key.span(),
            "expected `transitions(...)` or `lifecycle = <Enum>`",
        ))
    }
}

/// Parse `#[state_machine(...)]` from a field, returning the spec when present.
///
/// Accepts either the inline `transitions(...)` shorthand or a
/// `lifecycle = <Enum>` reference to a `#[lifecycle]` enum (issue #1911).
///
/// Validates:
/// - Only `String` fields are supported (the generated `.as_str()` call requires it).
/// - Multiple `#[state_machine]` attributes on the same field are rejected.
fn parse_state_machine_spec(field: &syn::Field) -> syn::Result<Option<StateMachineSpec>> {
    let Some(ident) = field.ident.as_ref() else {
        return Ok(None);
    };
    let mut spec: Option<StateMachineSpec> = None;
    for attr in &field.attrs {
        if attr.path().is_ident("state_machine") {
            if spec.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "multiple `#[state_machine]` attributes are not allowed on a single field",
                ));
            }
            let is_string = matches!(&field.ty, syn::Type::Path(p)
                if p.path.segments.last().is_some_and(|s| s.ident == "String"));
            if !is_string {
                return Err(syn::Error::new_spanned(
                    &field.ty,
                    "`#[state_machine]` is only supported on `String` fields",
                ));
            }
            let source = attr.parse_args_with(parse_state_machine_source)?;
            spec = Some(StateMachineSpec {
                field_ident: ident.clone(),
                source,
            });
        }
    }
    Ok(spec)
}

/// Names of the three generated state-machine items for a field.
struct StateMachineNames {
    const_name: syn::Ident,
    can_fn: syn::Ident,
    transition_fn: syn::Ident,
    /// The raw-prefix-stripped field name, for use in generated messages.
    field_str: String,
}

fn state_machine_names(field: &syn::Ident) -> StateMachineNames {
    let raw_field_str = field.to_string();
    // Strip the raw-identifier prefix so `r#type` produces `type`-derived names
    // rather than trying to create identifiers like `can_transition_r#type_to`.
    let field_str = raw_field_str
        .strip_prefix("r#")
        .unwrap_or(&raw_field_str)
        .to_string();
    let field_upper = field_str.to_uppercase();
    StateMachineNames {
        const_name: format_ident!("__AUTUMN_SM_{field_upper}_TRANSITIONS"),
        can_fn: format_ident!("can_transition_{field_str}_to"),
        transition_fn: format_ident!("transition_{field_str}_to"),
        field_str,
    }
}

/// Emit the three state machine items for one field: a transitions constant,
/// a `can_transition_{field}_to` predicate, and a `transition_{field}_to` method.
///
/// Dispatches on the declared [`StateMachineSource`]: an inline
/// `transitions(...)` list generates a literal table + match arms (guards
/// supported), while a `lifecycle = <Enum>` reference derives its table from the
/// enum's `Lifecycle::STATE_MACHINE_TRANSITIONS` const (issue #1911).
fn emit_state_machine_impl(
    model_name: &syn::Ident,
    spec: &StateMachineSpec,
    pk_ident: Option<&syn::Ident>,
) -> TokenStream {
    match &spec.source {
        StateMachineSource::Inline(transitions) => {
            emit_state_machine_inline(model_name, &spec.field_ident, transitions, pk_ident)
        }
        StateMachineSource::Lifecycle { path, effects } => {
            emit_state_machine_lifecycle(model_name, &spec.field_ident, path, effects, pk_ident)
        }
    }
}

/// Emit the state-machine items for an inline `transitions(...)` list.
///
/// When at least one edge declares `on_commit = <Job>` (issue #1973) this also
/// emits a `transition_{field}_to_on_conn` method that validates the transition
/// (via the pure `transition_{field}_to`) and, for the fired edge, enqueues the
/// named job transactionally on the caller's connection with a derived
/// idempotency key. Fields with no `on_commit` edge generate byte-for-byte the
/// same items as before — the new method is purely additive/opt-in.
fn emit_state_machine_inline(
    model_name: &syn::Ident,
    field: &syn::Ident,
    transitions: &[StateMachineTransition],
    pk_ident: Option<&syn::Ident>,
) -> TokenStream {
    let StateMachineNames {
        const_name,
        can_fn,
        transition_fn,
        field_str,
    } = state_machine_names(field);

    let const_entries: Vec<TokenStream> = transitions
        .iter()
        .map(|t| {
            let from = &t.from;
            let to = &t.to;
            t.guard.as_ref().map_or_else(
                || quote! { (#from, #to, ::core::option::Option::None) },
                |g| quote! { (#from, #to, ::core::option::Option::Some(#g)) },
            )
        })
        .collect();

    let match_arms: Vec<TokenStream> = transitions
        .iter()
        .map(|t| {
            let from = &t.from;
            let to = &t.to;
            t.guard.as_ref().map_or_else(
                || quote! { (#from, #to) => true },
                |g| {
                    let guard_fn = format_ident!("{g}");
                    quote! { (#from, #to) => self.#guard_fn() }
                },
            )
        })
        .collect();

    // After-commit transition effects (issue #1973): only emitted when at least
    // one edge names an `on_commit` job. This keeps no-`on_commit` models
    // byte-for-byte identical to before (purely additive/opt-in).
    let on_conn_impl = emit_state_machine_on_conn(
        model_name,
        field,
        &field_str,
        &transition_fn,
        transitions,
        pk_ident,
    );

    quote! {
        impl #model_name {
            #[doc(hidden)]
            pub const #const_name: &'static [(&'static str, &'static str, ::core::option::Option<&'static str>)] = &[
                #(#const_entries,)*
            ];

            /// Returns `true` when this record's `{field}` can transition to `target`.
            ///
            /// For guarded transitions the corresponding guard method is called first.
            pub fn #can_fn(&self, target: &str) -> bool {
                match (&*self.#field, target) {
                    #(#match_arms,)*
                    _ => false,
                }
            }

            /// Attempts to transition `{field}` to `target`, returning the new state value.
            ///
            /// Returns `Err` if the transition is not defined or a guard rejects it.
            pub fn #transition_fn(&self, target: &str) -> ::autumn_web::AutumnResult<::std::string::String> {
                if self.#can_fn(target) {
                    ::core::result::Result::Ok(::std::string::String::from(target))
                } else {
                    ::core::result::Result::Err(::autumn_web::AutumnError::bad_request_msg(
                        ::std::format!(
                            "Cannot transition `{}` from `{}` to `{}`",
                            #field_str,
                            self.#field,
                            target,
                        ),
                    ))
                }
            }
        }

        #on_conn_impl
    }
}

/// Emit the connection-taking `transition_{field}_to_on_conn` method for an
/// inline state machine that declares one or more per-edge effects — a sync
/// `on = "handler"` and/or an `on_commit = <Job>` (issue #1973). Returns an
/// empty token stream when no edge declares an effect, so machines without
/// effects generate exactly the pre-existing items.
///
/// The generated method: (1) validates the transition via the pure
/// `transition_{field}_to` (returning its `Err` unchanged when the edge is
/// disallowed or a guard rejects it), (2) for the specific fired `(from, to)`
/// edge, first runs its declared sync `on` handler on `conn` inside the
/// transaction (an `Err` propagates out, rolling the transition back), then
/// enqueues its `on_commit` job **transactionally** on `conn` (so a rollback
/// drops it) with a [`TransitionEffect`] payload whose `idempotency_key` is
/// derived from `(model, field, record_id, from, to)`, and (3) returns the new
/// state string for the caller to persist — mirroring the explicit-call
/// contract of `transition_{field}_to`.
fn emit_state_machine_on_conn(
    model_name: &syn::Ident,
    field: &syn::Ident,
    field_str: &str,
    transition_fn: &syn::Ident,
    transitions: &[StateMachineTransition],
    pk_ident: Option<&syn::Ident>,
) -> TokenStream {
    let has_effects = transitions
        .iter()
        .any(|t| t.on_commit.is_some() || t.on.is_some());
    if !has_effects {
        return TokenStream::new();
    }

    let on_conn_fn = format_ident!("{transition_fn}_on_conn");
    let model_str = model_name.to_string();

    // The record id renders through `Debug` so any primary-key type works
    // (i32/i64/Uuid/String all implement it). A model with an `on_commit` edge
    // is a `#[model]`, so a primary key is always present; the empty-string
    // fallback is defensive only.
    let record_id_expr = pk_ident.map_or_else(
        || quote! { ::std::string::String::new() },
        |pk| quote! { ::std::format!("{:?}", self.#pk) },
    );

    let effect_arms: Vec<TokenStream> = transitions
        .iter()
        .filter_map(|t| {
            if t.on.is_none() && t.on_commit.is_none() {
                return None;
            }
            let from = &t.from;
            let to = &t.to;
            // Sync in-transaction effect: runs first; `?` rolls the transition
            // back (mirrors a failing `before_*` mutation hook).
            let sync_effect = t.on.as_ref().map(|name| {
                let handler = format_ident!("{name}");
                quote! { self.#handler(conn).await?; }
            });
            // After-commit effect: enqueued transactionally, dispatched post-commit.
            let commit_effect = t.on_commit.as_ref().map(|job| {
                quote! {
                    let __record_id: ::std::string::String = #record_id_expr;
                    let __idempotency_key = ::std::format!(
                        "{}:{}:{}:{}:{}",
                        #model_str, #field_str, __record_id, #from, #to,
                    );
                    let __effect = ::autumn_web::TransitionEffect {
                        model: ::std::string::String::from(#model_str),
                        field: ::std::string::String::from(#field_str),
                        record_id: __record_id,
                        from_state: ::std::string::String::from(#from),
                        to_state: ::std::string::String::from(#to),
                        idempotency_key: __idempotency_key,
                    };
                    ::autumn_web::job::enqueue_on_conn(
                        <#job>::NAME,
                        &__effect,
                        conn,
                    ).await?;
                }
            });
            Some(quote! {
                (#from, #to) => {
                    #sync_effect
                    #commit_effect
                }
            })
        })
        .collect();

    quote! {
        impl #model_name {
            /// Validates a `{field}` transition and, for the fired edge, runs
            /// its declared effects on `conn` (issue #1973): first a sync
            /// `on = "handler"` method inside the transaction (an `Err` rolls
            /// the transition back), then an `on_commit = <Job>` enqueue that
            /// runs after the surrounding transaction commits. Returns the new
            /// state value for the caller to persist; both effects roll back
            /// with the caller's transaction.
            pub async fn #on_conn_fn(
                &self,
                conn: &mut ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                target: &str,
            ) -> ::autumn_web::AutumnResult<::std::string::String> {
                let __new_state = self.#transition_fn(target)?;
                match (&*self.#field, target) {
                    #(#effect_arms,)*
                    _ => {}
                }
                ::core::result::Result::Ok(__new_state)
            }
        }
    }
}

/// Strip a leading raw-identifier (`r#`) prefix from a state name, mirroring
/// `lifecycle::variant_name_str` so effect-edge state strings align with the
/// enum's already-stripped `STATE_MACHINE_TRANSITIONS` entries.
fn strip_raw(s: &str) -> &str {
    s.strip_prefix("r#").unwrap_or(s)
}

/// Emit the state-machine items for a `lifecycle = <Enum>` reference (#1911).
///
/// The transitions constant is an alias of the referenced enum's
/// `<#path as ::autumn_web::Lifecycle>::STATE_MACHINE_TRANSITIONS`, so the table
/// lives in exactly one place and stays typed. Because the concrete edge strings
/// are not known at macro-expansion time (they come from the enum in another
/// module/crate), the predicate iterates that table at runtime rather than
/// emitting literal match arms — producing an allowed/denied set identical to the
/// equivalent inline table. Referencing a type that does not implement
/// `Lifecycle` (i.e. is not a `#[lifecycle]` enum) fails to compile with an
/// unsatisfied trait bound.
///
/// Lifecycle transitions are unguarded (every table `guard` slot is `None`), and
/// a runtime string table cannot dispatch a named guard method anyway, so the
/// predicate requires the guard slot to be absent — guards remain an
/// inline-only shorthand feature (see the `Lifecycle` trait docs for the
/// rationale).
///
/// Per-edge effects (a sync `on = "handler"` and/or an `on_commit = <Job>`)
/// declared at the binding site via `effects(...)` (issue #1973) route through
/// the shared [`emit_state_machine_on_conn`], so the lifecycle path emits the
/// identical `transition_{field}_to_on_conn` method as the inline path. When
/// `effects` is empty the helper returns an empty token stream, keeping the
/// lifecycle codegen byte-for-byte identical to before.
fn emit_state_machine_lifecycle(
    model_name: &syn::Ident,
    field: &syn::Ident,
    path: &syn::Path,
    effects: &[StateMachineTransition],
    pk_ident: Option<&syn::Ident>,
) -> TokenStream {
    let StateMachineNames {
        const_name,
        can_fn,
        transition_fn,
        field_str,
    } = state_machine_names(field);

    // Normalize each effect edge's `from`/`to` by stripping a leading raw-identifier
    // prefix (#1973, Codex P2 on #2027). The `#[lifecycle]` macro stores variant names
    // in `STATE_MACHINE_TRANSITIONS` with `r#` already stripped (see
    // `lifecycle::variant_name_str`), but the effect edges parsed at the binding site
    // keep it, as in `Draft -> r#type`. Without this alignment the const assertion
    // below would check `"r#type"` against a table containing `"type"`, falsely
    // rejecting a real edge, and the generated match arm would never fire, silently
    // dropping the effect. Guard, on_commit, and on are carried through unchanged.
    let normalized_effects: Vec<StateMachineTransition> = effects
        .iter()
        .map(|t| StateMachineTransition {
            from: strip_raw(&t.from).to_owned(),
            to: strip_raw(&t.to).to_owned(),
            guard: t.guard.clone(),
            on_commit: t.on_commit.clone(),
            on: t.on.clone(),
        })
        .collect();
    let effects = &normalized_effects;

    // Per-edge effect method (issue #1973): only emitted when the binding site
    // declares one or more `effects(...)` edges. Empty `effects` returns an
    // empty token stream, so a plain `lifecycle = <Enum>` is unchanged.
    let on_conn_impl = emit_state_machine_on_conn(
        model_name,
        field,
        &field_str,
        &transition_fn,
        effects,
        pk_ident,
    );

    // Compile-time guard (issue #1973): the referenced enum's transition table
    // is not resolvable at macro-expansion time, so validate each declared
    // effect edge against `<Enum>::STATE_MACHINE_TRANSITIONS` with a const
    // assertion instead. An effect declared on an edge the lifecycle does not
    // permit would otherwise compile with a silently-unreachable match arm and
    // drop the effect; this turns that into a compile error. Empty `effects`
    // emits no assertions, keeping a plain `lifecycle = <Enum>` unchanged.
    let effect_edge_assertions: Vec<TokenStream> = effects
        .iter()
        .map(|t| {
            let from = &t.from;
            let to = &t.to;
            let msg = format!(
                "state-machine effect declared on edge `{from} -> {to}`, which is not a \
                 transition of the referenced lifecycle enum; declare the effect on a real \
                 transition edge (or add the edge to the lifecycle)"
            );
            quote! {
                const _: () = ::core::assert!(
                    ::autumn_web::__transition_edge_declared(
                        <#path as ::autumn_web::Lifecycle>::STATE_MACHINE_TRANSITIONS,
                        #from,
                        #to,
                    ),
                    #msg,
                );
            }
        })
        .collect();

    quote! {
        #(#effect_edge_assertions)*

        impl #model_name {
            #[doc(hidden)]
            pub const #const_name: &'static [(&'static str, &'static str, ::core::option::Option<&'static str>)] =
                <#path as ::autumn_web::Lifecycle>::STATE_MACHINE_TRANSITIONS;

            /// Returns `true` when this record's `{field}` can transition to `target`.
            ///
            /// Derived from the referenced `#[lifecycle]` enum's transition table.
            pub fn #can_fn(&self, target: &str) -> bool {
                let __current: &str = &self.#field;
                Self::#const_name
                    .iter()
                    .any(|&(__from, __to, __guard)| {
                        __from == __current && __to == target && __guard.is_none()
                    })
            }

            /// Attempts to transition `{field}` to `target`, returning the new state value.
            ///
            /// Returns `Err` if the transition is not defined by the referenced lifecycle.
            pub fn #transition_fn(&self, target: &str) -> ::autumn_web::AutumnResult<::std::string::String> {
                if self.#can_fn(target) {
                    ::core::result::Result::Ok(::std::string::String::from(target))
                } else {
                    ::core::result::Result::Err(::autumn_web::AutumnError::bad_request_msg(
                        ::std::format!(
                            "Cannot transition `{}` from `{}` to `{}`",
                            #field_str,
                            self.#field,
                            target,
                        ),
                    ))
                }
            }
        }

        #on_conn_impl
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::cognitive_complexity)]
pub fn model_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input: DeriveInput = match syn::parse2(item) {
        Ok(input) => input,
        Err(err) => return err.to_compile_error(),
    };

    let syn::Data::Struct(syn::DataStruct {
        fields: syn::Fields::Named(ref fields),
        ..
    }) = input.data
    else {
        return syn::Error::new_spanned(
            &input.ident,
            "#[model] can only be applied to structs with named fields",
        )
        .to_compile_error();
    };

    let table_name = match parse_attr_args(attr) {
        Ok(model_args) => model_args
            .table
            .unwrap_or_else(|| infer_table_name(&input.ident)),
        Err(err) => return err.to_compile_error(),
    };

    let table_ident = syn::Ident::new(&table_name, input.ident.span());
    let name = &input.ident;
    let vis = &input.vis;
    let outer_attrs = &input.attrs;

    let searchable_lang = match parse_model_searchable_lang(outer_attrs) {
        Ok(lang) => lang,
        Err(err) => return err.to_compile_error(),
    };
    let is_searchable = searchable_lang.is_some();
    let search_language = searchable_lang.unwrap_or_else(|| "simple".to_string());

    let shard_key_field = match parse_model_shard_key(outer_attrs) {
        Ok(key) => key,
        Err(err) => return err.to_compile_error(),
    };

    let associations = match resolve_associations(name, outer_attrs) {
        Ok(assocs) => assocs,
        Err(err) => return err.to_compile_error(),
    };
    let association_items = emit_association_items(name, &table_ident, vis, &associations);
    let dependents_impl = emit_dependents_impl(name, &associations);

    // `#[votable(by = ..., ...)]` (#1362). Resolved here next to the
    // associations; emitted below, once `all_fields` is known (the aggregate
    // column must name a real field, and the soft-delete guard is emitted only
    // when the model has a `deleted_at`).
    let votable = match resolve_votable(name, outer_attrs) {
        Ok(spec) => spec,
        Err(err) => return err.to_compile_error(),
    };

    // `#[commentable(by = ..., ...)]` (#1367) — the polymorphic association
    // kind. Resolved here beside `#[votable]`; emitted below, once `all_fields`
    // is known (the counter column must name a real `i64` field, and the
    // parent's soft-delete / tenant columns are projected into the spec).
    let commentable = match resolve_commentable(name, outer_attrs) {
        Ok(spec) => spec,
        Err(err) => return err.to_compile_error(),
    };

    // ── Architecture-graph relation tables (#1747) ───────────────────────
    // The edge tables this model's declared relations write. `#[votable]` and
    // `#[commentable]` generate methods on the model's *repository*, so a route
    // holding that repository reaches these tables without ever naming them.
    // Sorted and deduplicated so the emitted graph is byte-deterministic.
    let mut graph_relation_tables: Vec<String> = Vec::new();
    if let Some(spec) = votable.as_ref() {
        graph_relation_tables.push(spec.table.clone());
    }
    if let Some(spec) = commentable.as_ref() {
        graph_relation_tables.push(spec.table.clone());
    }
    graph_relation_tables.sort();
    graph_relation_tables.dedup();
    let graph_relations = if graph_relation_tables.is_empty() {
        quote! { &[] }
    } else {
        let tables = graph_relation_tables.iter().map(String::as_str);
        quote! { &[#(#tables),*] }
    };

    let filtered_outer_attrs: Vec<&syn::Attribute> = outer_attrs
        .iter()
        .filter(|a| {
            !a.path().is_ident("searchable")
                && !is_association_attr(a)
                && !is_votable_attr(a)
                && !is_commentable_attr(a)
                && !a.path().is_ident("shard_key")
        })
        .collect();

    let new_name = format_ident!("New{name}");
    let update_name = format_ident!("Update{name}");
    let changeset_name = format_ident!("__{}Changeset", name);

    // Classify fields
    let all_fields: Vec<&Field> = fields.named.iter().collect();

    // Validate the declarative-schema field markers (#1975) before any codegen,
    // so a malformed `#[unique]` / `#[references(...)]` yields a single clean
    // compile error rather than a cascade of downstream failures.
    for field in &all_fields {
        if let Err(err) = validate_field_schema_markers(field) {
            return err.to_compile_error();
        }
    }

    // Validate that the declared shard_key names an existing field (or "id").
    if let Some(ref key) = shard_key_field {
        let field_exists = key == "id"
            || all_fields
                .iter()
                .any(|f| f.ident.as_ref().is_some_and(|i| i == key));
        if !field_exists {
            let attr = outer_attrs
                .iter()
                .find(|a| a.path().is_ident("shard_key"))
                .expect("attribute was parsed above");
            return syn::Error::new_spanned(
                attr,
                format!("shard_key field `{key}` not found on model"),
            )
            .to_compile_error();
        }
    }

    // Validate that `#[votable]`'s aggregate column names an existing field of
    // the right type, mirroring the shard_key check above. Without the
    // existence check the hidden edge module declares the column regardless and
    // the mistake only surfaces as a runtime `42703 column "score" does not
    // exist` on the very first reaction; without the type check the column is
    // projected as `Int8` and a real `i32`/`Option<i64>` field only fails as an
    // opaque Diesel trait-resolution wall inside the generated `react()`.
    let votable_items = match votable {
        None => TokenStream::new(),
        Some(ref spec) => {
            let aggregate_field = all_fields
                .iter()
                .find(|f| f.ident.as_ref().is_some_and(|i| i == &spec.column));
            let Some(aggregate_field) = aggregate_field else {
                let attr = outer_attrs
                    .iter()
                    .find(|a| is_votable_attr(a))
                    .expect("attribute was parsed above");
                return syn::Error::new_spanned(
                    attr,
                    format!(
                        "votable aggregate column `{}` not found on model `{name}`; \
                         add the field (e.g. `pub {}: i64`) or override it with \
                         `#[votable(..., column = <field>)]`",
                        spec.column, spec.column,
                    ),
                )
                .to_compile_error();
            };
            // The aggregate is `SUM(value)` or `COUNT(*)`, both `BIGINT`, and the
            // generated `Reaction::aggregate` is `i64`, so anything else is a schema
            // mismatch rather than a convenience to coerce. Two layers (PR #2177
            // review): a directed error here for the spellings that are definitely
            // wrong — a bare non-`i64` primitive, an `Option` — and a generated `const`
            // type guard (`emit_votable_items`) for everything else. So
            // `std::primitive::i64` and aliases that really are `i64` compile, while a
            // wrong alias fails at the guard rather than at runtime.
            let aggregate_ty = aggregate_field.ty.to_token_stream().to_string();
            let definitely_not_i64: &[&str] = &[
                "i8", "i16", "i32", "i128", "u8", "u16", "u32", "u64", "u128", "usize", "isize",
                "f32", "f64", "bool", "String",
            ];
            if definitely_not_i64.contains(&aggregate_ty.as_str())
                || aggregate_ty.starts_with("Option <")
            {
                return syn::Error::new_spanned(
                    &aggregate_field.ty,
                    format!(
                        "votable aggregate column `{}` on model `{name}` must be \
                         `i64` (BIGINT), found `{aggregate_ty}`; the aggregate is \
                         `SUM(value)` / `COUNT(*)` and is written back as an \
                         `i64` — widen the column and the field",
                        spec.column,
                    ),
                )
                .to_compile_error();
            }
            let has_deleted_at = all_fields
                .iter()
                .any(|f| f.ident.as_ref().is_some_and(|i| i == "deleted_at"));
            // Same field-presence precedent as `deleted_at`: the column has to
            // exist for S1/S5 to filter on it. Whether it is actually *applied*
            // is the repository's call, resolved at runtime through
            // `M2mConnSource::__autumn_m2m_tenant_scope()`.
            let has_tenant_id = all_fields
                .iter()
                .any(|f| f.ident.as_ref().is_some_and(|i| i == "tenant_id"));
            // A composite key cannot back a reaction: `react(target_id)`
            // identifies the target by ONE value, so with two `#[id]` fields
            // S1 would lock whichever rows share the first component and S5
            // would update all of them. Reject rather than silently keying on
            // the first component (PR #2177 review).
            let id_fields: Vec<&syn::Ident> = all_fields
                .iter()
                .filter(|f| has_attr(f, "id"))
                .filter_map(|f| f.ident.as_ref())
                .collect();
            if id_fields.len() > 1 {
                let attr = outer_attrs
                    .iter()
                    .find(|a| is_votable_attr(a))
                    .expect("attribute was parsed above");
                let listed = id_fields
                    .iter()
                    .map(|i| format!("`{i}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return syn::Error::new_spanned(
                    attr,
                    format!(
                        "`#[votable]` requires a single `i64` primary key: model \
                         `{name}` declares a composite key ({listed}), and \
                         `react(reactor_id, target_id)` identifies its target by \
                         one id — the edge table and the aggregate UPDATE cannot \
                         address a composite-keyed row"
                    ),
                )
                .to_compile_error();
            }
            let pk_ident = all_fields
                .iter()
                .find(|f| has_attr(f, "id"))
                .or_else(|| {
                    all_fields.iter().find(|f| match &f.ty {
                        syn::Type::Path(tp) => tp.path.is_ident("i32") || tp.path.is_ident("i64"),
                        _ => false,
                    })
                })
                .and_then(|f| f.ident.as_ref());
            emit_votable_items(
                name,
                &table_ident,
                vis,
                spec,
                has_deleted_at,
                has_tenant_id,
                pk_ident,
            )
        }
    };

    // ── Polymorphic comments (#[commentable], #1367) ────────────────────────
    // Validated on the same terms as `#[votable]`'s aggregate column: the
    // maintained counter must name a real `i64` field, or the mistake only
    // surfaces as a runtime `42703 column "comment_count" does not exist` on
    // the very first comment.
    let commentable_items = match commentable {
        None => TokenStream::new(),
        Some(ref spec) => {
            if let Some(counter_column) = spec.counter_column.as_deref() {
                let counter_field = all_fields
                    .iter()
                    .find(|f| f.ident.as_ref().is_some_and(|i| i == counter_column));
                let Some(counter_field) = counter_field else {
                    return syn::Error::new(
                        spec.span,
                        format!(
                            "commentable counter column `{counter_column}` not found on \
                             model `{name}`; add the field (e.g. `#[default] pub \
                             {counter_column}: i64`), point the attribute at an \
                             existing one with `#[commentable(..., counter_cache = \
                             <field>)]`, or opt out with `#[commentable(..., \
                             counter_cache = false)]`"
                        ),
                    )
                    .to_compile_error();
                };
                // Two layers, exactly like `#[votable]`: a directed error for
                // the spellings that are definitely wrong, and the generated
                // `const` guard below for everything else — so
                // `std::primitive::i64` and honest aliases still compile.
                let counter_ty = counter_field.ty.to_token_stream().to_string();
                let definitely_not_i64: &[&str] = &[
                    "i8", "i16", "i32", "i128", "u8", "u16", "u32", "u64", "u128", "usize",
                    "isize", "f32", "f64", "bool", "String",
                ];
                if definitely_not_i64.contains(&counter_ty.as_str())
                    || counter_ty.starts_with("Option <")
                {
                    return syn::Error::new_spanned(
                        &counter_field.ty,
                        format!(
                            "commentable counter column `{counter_column}` on model \
                             `{name}` must be `i64` (BIGINT), found `{counter_ty}`; the \
                             counter is maintained with `SET c = c + 1` and read back \
                             as an `i64` — widen the column and the field"
                        ),
                    )
                    .to_compile_error();
                }
            }
            let cmt_has_deleted_at = all_fields
                .iter()
                .any(|f| f.ident.as_ref().is_some_and(|i| i == "deleted_at"));
            let cmt_has_tenant_id = all_fields
                .iter()
                .any(|f| f.ident.as_ref().is_some_and(|i| i == "tenant_id"));
            // A composite key cannot back a polymorphic parent: `commentable_id`
            // is ONE column, so a two-`#[id]` model has no single value to store
            // there. Reject rather than silently keying on the first component.
            let cmt_id_fields: Vec<&syn::Ident> = all_fields
                .iter()
                .filter(|f| has_attr(f, "id"))
                .filter_map(|f| f.ident.as_ref())
                .collect();
            if cmt_id_fields.len() > 1 {
                let listed = cmt_id_fields
                    .iter()
                    .map(|i| format!("`{i}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return syn::Error::new(
                    spec.span,
                    format!(
                        "`#[commentable]` requires a single `i64` primary key: model \
                         `{name}` declares a composite key ({listed}), and \
                         `commentable_id` is one column — it cannot address a \
                         composite-keyed row"
                    ),
                )
                .to_compile_error();
            }
            let cmt_pk_ident = all_fields
                .iter()
                .find(|f| has_attr(f, "id"))
                .or_else(|| {
                    all_fields.iter().find(|f| match &f.ty {
                        syn::Type::Path(tp) => tp.path.is_ident("i32") || tp.path.is_ident("i64"),
                        _ => false,
                    })
                })
                .and_then(|f| f.ident.as_ref());
            emit_commentable_items(
                name,
                vis,
                spec,
                &table_name,
                &crate::commentable::ParentShape {
                    has_deleted_at: cmt_has_deleted_at,
                    has_tenant_id: cmt_has_tenant_id,
                    is_sharded: shard_key_field.is_some(),
                    pk_ident: cmt_pk_ident,
                },
            )
        }
    };

    // ── Counter caches (#1325) ──────────────────────────────────────────────
    // Resolved here (rather than beside `resolve_associations`) because the
    // spec needs `all_fields`: the foreign key must be a real field, and its
    // nullability decides whether `fk_of` wraps or forwards. A `deleted_at`
    // column makes the count reflect live rows only.
    let counter_caches_impl = {
        let cc_has_deleted_at = all_fields
            .iter()
            .any(|f| f.ident.as_ref().is_some_and(|i| i == "deleted_at"));
        // Same fallback every other primary-key consumer in this macro uses
        // (`id_field_names`, the factory PK, `#[votable]`): an explicit `#[id]`,
        // else the first integer field. Hard-requiring `#[id]` would reject
        // models the rest of `#[model]` accepts — and, because the error
        // short-circuits the whole macro, would bury it under a cascade of
        // "cannot find type" errors from the paired `#[repository]`.
        let cc_pk_ident = all_fields
            .iter()
            .find(|f| has_attr(f, "id"))
            .or_else(|| {
                all_fields.iter().find(|f| match &f.ty {
                    syn::Type::Path(tp) => tp.path.is_ident("i32") || tp.path.is_ident("i64"),
                    _ => false,
                })
            })
            .and_then(|f| f.ident.as_ref());
        match emit_counter_caches_impl(
            name,
            &table_name,
            cc_pk_ident,
            cc_has_deleted_at,
            &associations,
            &all_fields,
        ) {
            Ok(tokens) => tokens,
            Err(err) => return err.to_compile_error(),
        }
    };

    let mut search_field_names = Vec::new();
    let mut search_field_weights = Vec::new();
    // #1191: the field idents backing the searchable columns, so the derived
    // `SearchIndexed::search_document` can extract their values, plus the
    // single `#[searchable(embed)]` field (if any) that feeds vector search.
    let mut search_field_idents: Vec<syn::Ident> = Vec::new();
    let mut search_embed_field: Option<String> = None;

    for field in &all_fields {
        let cfg = match parse_field_searchable(field) {
            Ok(cfg) => cfg,
            Err(err) => return err.to_compile_error(),
        };
        match cfg.kind {
            FieldSearchable::NotSearchable => {
                if cfg.embed {
                    // Unreachable through the parser (a bare `embed` still
                    // yields a searchable kind), but keep the invariant local.
                    return syn::Error::new_spanned(
                        field,
                        "`embed` requires the field to be #[searchable]",
                    )
                    .to_compile_error();
                }
            }
            weight_type => {
                let field_ident = field.ident.as_ref().unwrap();
                let weight = match weight_type {
                    FieldSearchable::SearchableWithWeight(w) => w,
                    FieldSearchable::SearchableDefault | FieldSearchable::NotSearchable => {
                        "D".to_string()
                    }
                };
                if weight.len() != 1 {
                    return syn::Error::new_spanned(
                        field_ident,
                        "searchable weight must be a single character (A, B, C, or D)",
                    )
                    .to_compile_error();
                }
                let weight_char = weight.chars().next().unwrap();
                if !['A', 'B', 'C', 'D'].contains(&weight_char) {
                    return syn::Error::new_spanned(
                        field_ident,
                        "searchable weight must be A, B, C, or D",
                    )
                    .to_compile_error();
                }
                if cfg.embed {
                    if let Some(existing) = &search_embed_field {
                        return syn::Error::new_spanned(
                            field_ident,
                            format!(
                                "only one field may be marked #[searchable(embed)]; `{existing}` \
                                 already is. A model has a single embedding per record — \
                                 concatenate the fields you want embedded into one column."
                            ),
                        )
                        .to_compile_error();
                    }
                    search_embed_field = Some(field_ident.to_string());
                }
                search_field_names.push(field_ident.to_string());
                search_field_weights.push(weight_char);
                search_field_idents.push(field_ident.clone());
            }
        }
    }

    if !is_searchable && search_embed_field.is_some() {
        return syn::Error::new_spanned(
            name,
            "#[searchable(embed)] requires the model to be marked #[searchable] \
             (add `#[searchable]` or `#[searchable(language = \"...\")]` above the struct)",
        )
        .to_compile_error();
    }

    let id_fields: Vec<&&Field> = all_fields.iter().filter(|f| has_attr(f, "id")).collect();

    // If no explicit #[id], default to first i32/i64 field
    let id_field_names: Vec<&syn::Ident> = if id_fields.is_empty() {
        all_fields
            .iter()
            .filter(|f| {
                if let syn::Type::Path(tp) = &f.ty {
                    tp.path.is_ident("i32") || tp.path.is_ident("i64")
                } else {
                    false
                }
            })
            .take(1)
            .filter_map(|f| f.ident.as_ref())
            .collect()
    } else {
        id_fields.iter().filter_map(|f| f.ident.as_ref()).collect()
    };

    // PK field ident + type — used by the test-support factory to generate
    // `__autumn_pk()` so association setters don't hardcode `.id`.
    let pk_field_for_factory: Option<(&syn::Ident, &syn::Type)> = if id_fields.is_empty() {
        all_fields
            .iter()
            .find(|f| {
                if let syn::Type::Path(tp) = &f.ty {
                    tp.path.is_ident("i32") || tp.path.is_ident("i64")
                } else {
                    false
                }
            })
            .and_then(|f| f.ident.as_ref().map(|id| (id, &f.ty)))
    } else {
        id_fields
            .first()
            .and_then(|f| f.ident.as_ref().map(|id| (id, &f.ty)))
    };

    // Collect state machine specs from all fields (RED → GREEN: declarative SM
    // support). Emitted here — after the primary key is resolved — so an
    // `on_commit` effect (issue #1973) can derive its idempotency key from the
    // record's primary-key value.
    let sm_pk_ident: Option<&syn::Ident> = pk_field_for_factory.map(|(id, _)| id);
    let mut state_machine_impls: Vec<TokenStream> = Vec::new();
    for field in &all_fields {
        match parse_state_machine_spec(field) {
            Ok(Some(spec)) => {
                state_machine_impls.push(emit_state_machine_impl(name, &spec, sm_pk_ident));
            }
            Ok(None) => {}
            Err(err) => return err.to_compile_error(),
        }
    }

    // ── #1191: the engine-agnostic search index definition and document ─────
    //
    // #842 already emits `AutumnSearchableModel`, the metadata the in-core
    // Postgres/SQLite FTS reads. `SearchIndexed` is the pluggable half: it hands
    // `autumn-search` an `IndexDefinition` and a per-record `SearchDocument`, so any
    // backend — Postgres with pgvector, Meilisearch, a vector store — can index the
    // model with no hand-written glue.
    //
    // Emitted only for a model that is searchable, declares at least one searchable
    // field, and has an `i64` primary key, the id type every search index keys on and
    // the in-core FTS `search()` already assumes. A `#[searchable]` model with a `Uuid`
    // key keeps its #842 behaviour and simply has no plugin index.
    let search_pk_ident: Option<&syn::Ident> = pk_field_for_factory.and_then(|(id, ty)| {
        matches!(ty, syn::Type::Path(tp) if tp.path.is_ident("i64")).then_some(id)
    });
    let search_indexed_impl = match (is_searchable, search_pk_ident) {
        (true, Some(pk_ident)) if !search_field_idents.is_empty() => {
            // Diesel maps a struct field to the identically-named column, so
            // the `#[id]` field's ident IS the key column name.
            let search_pk_column = pk_ident.to_string();
            let field_names = search_field_names.clone();
            let field_weights = search_field_weights.clone();
            let field_idents = search_field_idents.clone();
            let embed_const = search_embed_field.as_ref().map_or_else(
                || quote! { ::core::option::Option::None },
                |field| quote! { ::core::option::Option::Some(#field) },
            );
            let embed_extract = search_embed_field.as_ref().map_or_else(
                || quote! {},
                |field| {
                    let ident = syn::Ident::new(field, name.span());
                    quote! {
                        // An empty embed source stays `None`: embedding the
                        // empty string would cost a provider call and pollute
                        // k-NN results with a meaningless vector.
                        let __autumn_embed =
                            ::autumn_web::search::SearchTextValue::search_text_value(&self.#ident);
                        if !__autumn_embed.is_empty() {
                            __autumn_doc.embed_text = ::core::option::Option::Some(__autumn_embed);
                        }
                    }
                },
            );
            // A model with a `tenant_id` column carries its tenant into every document,
            // so any backend can fail closed on a cross-tenant read.
            //
            // Keyed on the name and the type. An unrelated `tenant_id: i64` would
            // otherwise stamp `tenant_id = "42"` on every document and make every
            // real-tenant search return nothing, and a `tenant_id` of some other type
            // would fail to compile inside generated code, pointing at a trait the user
            // never mentioned. The rest of the macro's tenancy path already assumes
            // `String`/`Option<String>`.
            //
            // This is the column check only. Whether the index is scoped to the tenant
            // is resolved at runtime from the repository — see `#tenant_scoped_expr`.
            let has_tenant_column = all_fields.iter().any(|f| {
                f.ident.as_ref().is_some_and(|i| i == "tenant_id")
                    && is_string_or_option_string(&f.ty)
            });
            // Tenant scoping follows `#[repository(tenant_scoped)]`, exactly as
            // soft-delete follows `#[repository(soft_delete)]`, and for the same reason.
            // A `tenant_id` column that is denormalized or audit data, on a repository
            // that does not opt in, has unscoped finders; marking its index
            // tenant-scoped anyway would make every search outside a tenant context
            // fail with `TenantContextMissing` and every search inside one silently
            // filter, neither of which matches the app's own reads.
            //
            // Column presence is still required: with no `tenant_id` the documents
            // carry no tenant to filter on, so scoping would match nothing.
            let tenant_scoped_expr = if has_tenant_column {
                quote! { <Self>::__autumn_repo_tenant_scope() }
            } else {
                quote! { false }
            };
            let tenant_extract = if has_tenant_column {
                quote! {
                    let __autumn_tenant =
                        ::autumn_web::search::SearchTextValue::search_text_value(&self.tenant_id);
                    if !__autumn_tenant.is_empty() {
                        __autumn_doc.tenant_id = ::core::option::Option::Some(__autumn_tenant);
                    }
                }
            } else {
                quote! {}
            };

            quote! {
                impl ::autumn_web::search::SearchIndexed for #name {
                    const SEARCH_INDEX: &'static str = #table_name;
                    const SEARCH_EMBED_FIELD: ::core::option::Option<&'static str> = #embed_const;

                    fn index_definition() -> ::autumn_web::search::IndexDefinition {
                        // In scope so the blanket `false` defaults resolve for
                        // a model whose repository is not `soft_delete` /
                        // `tenant_scoped` (or that has no repository at all).
                        // An inherent override emitted by `#[repository(...)]`
                        // still wins — see the unqualified `<Self>::` calls.
                        use ::autumn_web::preload::AutumnPreloadScopeExt as _;

                        const __AUTUMN_SEARCH_INDEX_FIELDS:
                            &[::autumn_web::search::SearchIndexField] = &[
                            #(::autumn_web::search::SearchIndexField::new(
                                #field_names,
                                #field_weights,
                            )),*
                        ];
                        ::autumn_web::search::IndexDefinition::new(
                            #table_name,
                            #search_language,
                            __AUTUMN_SEARCH_INDEX_FIELDS,
                            #embed_const,
                            #tenant_scoped_expr,
                        )
                        // The REAL key column, not the conventional `id`: a
                        // backend that rebuilds documents from the source table
                        // (backfill, reindex) selects and paginates on it, and
                        // a model keyed on `article_id` would otherwise fail at
                        // runtime with an undefined column.
                        .with_key_column(#search_pk_column)
                        // Resolved at runtime through the same seam preload
                        // uses: `#[repository(soft_delete)]` overrides this
                        // inherently on the model, and `#[model]` cannot see the
                        // repository attribute. A `deleted_at` column alone does
                        // not mean soft delete — it is often audit history, and
                        // those rows must stay indexed because the model's
                        // finders still return them.
                        //
                        // Unqualified `<Self>::`, never `<Self as Trait>::`: the
                        // override is an inherent associated fn, and naming the
                        // trait would resolve past it to the blanket `false`
                        // default, silently disabling the filter for every
                        // genuinely soft-deleted model.
                        .with_soft_delete(<Self>::__autumn_repo_soft_delete_scope())
                    }

                    fn search_id(&self) -> i64 {
                        self.#pk_ident
                    }

                    fn search_document(&self) -> ::autumn_web::search::SearchDocument {
                        let mut __autumn_doc = ::autumn_web::search::SearchDocument::new(
                            #table_name,
                            self.#pk_ident,
                        );
                        #(
                            __autumn_doc = __autumn_doc.with_field(
                                #field_names,
                                #field_weights,
                                ::autumn_web::search::SearchTextValue::search_text_value(
                                    &self.#field_idents,
                                ),
                            );
                        )*
                        #embed_extract
                        #tenant_extract
                        __autumn_doc
                    }
                }
            }
        }
        _ => quote! {},
    };

    // Fields for NewX: exclude #[id], #[default], #[lock_version], and auto-detected ID fields
    let fields_for_new: Vec<&&Field> = all_fields
        .iter()
        .filter(|f| {
            !excluded_from_new(f)
                && f.ident
                    .as_ref()
                    .is_some_and(|id| !id_field_names.contains(&id))
        })
        .collect();

    // The single #[lock_version] field (if any). Only one is supported; the
    // first one wins. The field is excluded from NewX but is included in
    // UpdateX as a plain (non-Patch) required field so the client always
    // sends the version they read.
    let lock_version_field: Option<&&Field> =
        all_fields.iter().find(|f| has_attr(f, "lock_version"));

    // Validate #[factory_assoc] attributes before using them.
    if let Some(err) = validate_factory_assoc_attrs(&all_fields) {
        return err;
    }

    // Collect `#[encrypted]` columns (validated to be non-null `String`).
    // Each entry: (column, deterministic, admin_visible, versioned_ciphertext).
    let mut encrypted_columns: Vec<(String, bool, bool, bool)> = Vec::new();
    for f in &all_fields {
        if let Err(err) = validate_encrypted_field(f) {
            return err.to_compile_error();
        }
        match parse_field_encrypted(f) {
            Ok(spec) if spec.is_encrypted() => {
                let col = f.ident.as_ref().unwrap().to_string();
                encrypted_columns.push((
                    col,
                    spec.mode == EncryptedMode::Deterministic,
                    spec.admin_visible,
                    spec.versioned_ciphertext,
                ));
            }
            Ok(_) => {}
            Err(err) => return err.to_compile_error(),
        }
    }

    // Collect `#[classified]` columns (issue #1654, validated to be non-null
    // `String`). Each entry: (field ident, column name, generated field marker).
    let mut classified_columns: Vec<(&syn::Ident, String, syn::Ident)> = Vec::new();
    for f in &all_fields {
        if let Err(err) = validate_classified_field(f) {
            return err.to_compile_error();
        }
        if !field_is_classified(f) {
            continue;
        }
        let ident = f.ident.as_ref().unwrap();
        classified_columns.push((
            ident,
            unraw_ident(ident),
            classified_marker_ident(name, ident),
        ));
    }
    let has_classified = !classified_columns.is_empty();
    // A struct-level `#[serde(rename_all = ...)]` desyncs the manifest row (Rust
    // name) from the wire name it is reviewed against, exactly as it does for
    // encrypted columns.
    if has_classified && attrs_have_serde_rename_all(outer_attrs) {
        return syn::Error::new_spanned(
            name,
            "`#[serde(rename_all = ...)]` cannot be combined with `#[classified]` fields in \
             v1: classified columns are registered in the data-flow manifest under their \
             Rust names, which must match the names the manifest is reviewed against.",
        )
        .to_compile_error();
    }
    let classified_column_names: Vec<&str> = classified_columns
        .iter()
        .map(|(_, col, _)| col.as_str())
        .collect();

    // Collect `#[translatable]` columns (issue #1384, validated to be
    // non-null `Translated`). Each entry is the field ident; the column name is
    // the Rust field name, which is also the key the field-name-driven
    // `available_locales` / `is_translated` accessors match on.
    let mut translatable_columns: Vec<&syn::Ident> = Vec::new();
    for f in &all_fields {
        if let Err(err) = validate_translatable_field(f) {
            return err.to_compile_error();
        }
        if field_is_translatable(f)
            && let Some(ident) = f.ident.as_ref()
        {
            translatable_columns.push(ident);
        }
    }
    // `unraw()` for the same reason as in `emit_translatable_items`: the column
    // registered here must be the real DB column name.
    let translatable_column_names: Vec<String> = translatable_columns
        .iter()
        .map(|i| unraw_ident(i))
        .collect();
    let translatable_inventory: Vec<TokenStream> = translatable_column_names
        .iter()
        .map(|col| {
            quote! {
                ::autumn_web::reexports::inventory::submit! {
                    ::autumn_web::i18n::TranslatableColumnDescriptor {
                        model: stringify!(#name),
                        table: #table_name,
                        column: #col,
                    }
                }
            }
        })
        .collect();
    let translatable_items = emit_translatable_items(name, &translatable_columns);

    // Collect `#[normalize]` columns (validated to be non-null `String`).
    // Each entry: (field ident, lookup key, normalizer chain).
    // The lookup key is the *Rust* field name (the diesel column), because the
    // derived `#[repository]` `find_by_`/`count_by_` finder passes the Rust
    // field name to `normalize_lookup` (mirroring how `#[encrypted]` keys its
    // registry off the field ident). Keying on the serde-serialized name would
    // desync the arm from the finder and silently skip normalization for a
    // renamed column. (#1379)
    let mut normalized_columns: Vec<(&syn::Ident, String, Vec<Normalizer>)> = Vec::new();
    for f in &all_fields {
        if let Err(err) = validate_normalize_field(f) {
            return err.to_compile_error();
        }
        if !field_has_normalize(f) {
            continue;
        }
        let ident = f.ident.as_ref().unwrap();
        let lookup_key = ident.to_string();
        match parse_field_normalize(f) {
            Ok(ops) => normalized_columns.push((ident, lookup_key, ops)),
            Err(err) => return err.to_compile_error(),
        }
    }

    // A struct-level `#[serde(rename_all = ...)]` also desyncs encrypted-column
    // registration (Rust name) from the serialized key — reject it when any field
    // is encrypted (see `field_has_serde_rename` for the per-field case).
    if !encrypted_columns.is_empty() && attrs_have_serde_rename_all(outer_attrs) {
        return syn::Error::new_spanned(
            name,
            "`#[serde(rename_all = ...)]` cannot be combined with `#[encrypted]` fields in v1: \
             encrypted columns are registered under their Rust names, which must match the \
             serialized keys used by version history / log scrubbing / admin redaction.",
        )
        .to_compile_error();
    }
    let encrypted_column_names: Vec<&str> =
        encrypted_columns.iter().map(|(c, ..)| c.as_str()).collect();
    // Diesel's `AsChangeset`/`Insertable` derives expand `column.eq(value)` in
    // the model's module scope when `serialize_as` is present, which needs
    // `ExpressionMethods` in scope. Bring it in anonymously (only for models with
    // encrypted columns) so app authors don't have to add the import themselves.
    let encrypted_use = if encrypted_columns.is_empty() && classified_columns.is_empty() {
        quote! {}
    } else {
        quote! {
            #[allow(unused_imports)]
            use ::autumn_web::reexports::diesel::ExpressionMethods as _;
        }
    };
    // Encrypt encrypted columns in the durable commit-hook payload so secrets are
    // never persisted in plaintext to `autumn_repository_commit_hooks` (#805).
    let commit_hook_encrypt_stmt = if encrypted_columns.is_empty() {
        quote! {}
    } else {
        quote! {
            ::autumn_web::encryption::encrypt_persisted_columns_in_value(
                #table_name,
                &mut __autumn_value,
            );
        }
    };
    // Symmetric inverse: when a durable commit-hook record is read back to drive
    // `after_*_commit`, decrypt the encrypted columns first so replayed hooks
    // receive plaintext model values, exactly as on the normal repository path.
    let commit_hook_decrypt_stmt = if encrypted_columns.is_empty() {
        quote! {}
    } else {
        quote! {
            ::autumn_web::encryption::decrypt_persisted_columns_in_value(
                #table_name,
                &mut __autumn_decoded_value,
            );
        }
    };
    // For models with encrypted columns, replace the derived `Debug` on every
    // plaintext-holding struct (query, New*, Update*, Changeset) with a redacting
    // manual impl so values never leak through `Debug`/panic output — including
    // update payloads whose `Patch<String>` would otherwise print `Set("secret")`
    // (#805 AC, composes with #697).
    // (`factory_name` is bound here rather than beside the factory builder so
    // the redacting Debug impl below can name it, mirroring `changeset_name`.)
    let factory_name = format_ident!("{name}Factory");
    let lock_version_ident: Option<&syn::Ident> = lock_version_field.and_then(|f| f.ident.as_ref());
    let mutable_idents: Vec<&syn::Ident> = fields_for_new
        .iter()
        .map(|f| f.ident.as_ref().unwrap())
        .chain(lock_version_ident)
        .collect();
    let (
        name_debug_derive,
        name_debug_impl,
        new_debug_derive,
        new_debug_impl,
        update_debug_derive,
        update_debug_impl,
        changeset_debug_derive,
        changeset_debug_impl,
        factory_debug_derive,
        factory_debug_impl,
    ) = if encrypted_columns.is_empty() && classified_columns.is_empty() {
        (
            quote! { Debug, },
            quote! {},
            quote! { Debug, },
            quote! {},
            quote! { Debug, },
            quote! {},
            quote! { Debug, },
            quote! {},
            quote! { Debug, },
            quote! {},
        )
    } else {
        let all_idents: Vec<&syn::Ident> = all_fields
            .iter()
            .map(|f| f.ident.as_ref().unwrap())
            .collect();
        let new_idents: Vec<&syn::Ident> = fields_for_new
            .iter()
            .map(|f| f.ident.as_ref().unwrap())
            .collect();
        (
            quote! {},
            redacting_debug_impl(
                name,
                &all_idents,
                &encrypted_column_names,
                &classified_column_names,
            ),
            quote! {},
            redacting_debug_impl(
                &new_name,
                &new_idents,
                &encrypted_column_names,
                &classified_column_names,
            ),
            quote! {},
            redacting_debug_impl(
                &update_name,
                &mutable_idents,
                &encrypted_column_names,
                &classified_column_names,
            ),
            quote! {},
            redacting_debug_impl(
                &changeset_name,
                &mutable_idents,
                &encrypted_column_names,
                &classified_column_names,
            ),
            // #2373: the factory holds the same plaintext-bearing columns and is
            // the struct most likely to be printed while debugging a test, so it
            // gets the same redacting `Debug` as the other four rather than a
            // derived one.
            quote! {},
            redacting_debug_impl(
                &factory_name,
                &new_idents,
                &encrypted_column_names,
                &classified_column_names,
            ),
        )
    };
    // #1654: a model holding a classified column does not implement `Serialize`.
    // That is the whole guarantee at the model level: every sink autumn gates is
    // reached through `Serialize`, so withholding it makes `Json(model)` a build
    // failure instead of a leak. `Deserialize` is untouched -- taking the data in
    // is not a release.
    let query_serde_derive = if has_classified {
        quote! { #[derive(::serde::Deserialize)] }
    } else {
        quote! { #[derive(::serde::Serialize, ::serde::Deserialize)] }
    };
    // One zero-sized marker per classified column. It names the field inside the
    // compiler diagnostic when a leak is attempted, keys a `declassify!` boundary
    // to exactly one column, and publishes the manifest row.
    let classified_items: Vec<TokenStream> = classified_columns
        .iter()
        .map(|(ident, col, marker)| {
            let doc = format!(
                "Field marker for the `#[classified]` column `{}::{}` (issue #1654).\n\n\
                 Name it in [`declassify!`](autumn_web::declassify) to declare a boundary \
                 that releases this column, and nothing else.",
                unraw_ident(name),
                col,
            );
            quote! {
                #[doc = #doc]
                #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
                #vis struct #marker;

                impl ::autumn_web::classify::ClassifiedField for #marker {
                    const MODEL: &'static str = stringify!(#name);
                    // Module-qualified, because the manifest keys on this: two
                    // linked crates can each define a `Customer` with a
                    // classified `email`, and the bare name would merge them
                    // into one row.
                    const MODEL_PATH: &'static str =
                        concat!(module_path!(), "::", stringify!(#name));
                    const FIELD: &'static str = #col;
                    const CLASSIFICATION: ::autumn_web::classify::Classification =
                        ::autumn_web::classify::Classification::PersonalData;
                }

                ::autumn_web::reexports::inventory::submit! {
                    ::autumn_web::classify::manifest::ClassifiedFieldDescriptor {
                        model: stringify!(#name),
                        model_path: concat!(module_path!(), "::", stringify!(#name)),
                        field: #col,
                        classification: ::autumn_web::classify::Classification::PersonalData,
                    }
                }

                // Keep the field ident referenced so a marker can never outlive a
                // renamed column without the rename being noticed here.
                const _: () = {
                    #[allow(dead_code)]
                    fn __autumn_classified_column_exists(m: &#name) -> &::autumn_web::classify::Classified<::std::string::String, #marker> {
                        &m.#ident
                    }
                };
            }
        })
        .collect();

    let encrypted_inventory: Vec<TokenStream> = encrypted_columns
        .iter()
        .map(
            |(col, deterministic, admin_visible, versioned_ciphertext)| {
                quote! {
                    ::autumn_web::reexports::inventory::submit! {
                        ::autumn_web::encryption::EncryptedColumnDescriptor {
                            model: stringify!(#name),
                            table: #table_name,
                            column: #col,
                            deterministic: #deterministic,
                            admin_visible: #admin_visible,
                            versioned_ciphertext: #versioned_ciphertext,
                        }
                    }
                }
            },
        )
        .collect();

    // Fields for UpdateX: Patch fields (from fields_for_new) plus the
    // lock_version field (plain required type, not Patch<T>).

    // Check if any field has #[validate(...)]
    let has_validation = all_fields.iter().any(|f| !validate_attrs(f).is_empty());

    // Build query struct fields (strip #[id], #[indexed], #[validate])
    // #1654: the field marker for a `#[classified]` column, or `None`. Every
    // generated struct that carries the column -- the read struct, `NewX`,
    // `UpdateX` and the changeset -- looks it up through here so they cannot
    // drift apart on whether the column is classified.
    let classified_marker_for = |f: &Field| -> Option<&syn::Ident> {
        let ident = f.ident.as_ref()?;
        classified_columns
            .iter()
            .find(|(cid, ..)| *cid == ident)
            .map(|(_, _, marker)| marker)
    };
    // The Diesel column mapping for a classified field. Write-only structs
    // (`NewX`, the changeset) need `serialize_as` alone; the read struct also
    // needs `deserialize_as`.
    let classified_serialize_as = |marker: &syn::Ident| {
        quote! {
            #[diesel(serialize_as = ::autumn_web::classify::ClassifiedText<#marker>)]
        }
    };

    let query_fields: Vec<TokenStream> = all_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            let attrs = user_attrs(f);
            // #1778: keep the field's full `#[validate(...)]` rules on the read
            // model, so the effective merged model (existing row plus patch) can be
            // validated on the update path via `from_patch`. The model's fields are
            // concrete `T`, not `Patch<T>`, so every validator compiles and runs
            // here — including the ones `Patch<T>` cannot express (`ip` on
            // `Option`, `does_not_contain`, and the cross-field `custom`,
            // `must_match`, `nested`) — without hitting the E0119 coherence walls
            // that block them there. `nested` carries its own hazard even here: see
            // `NON_PATCH_VALIDATORS`'s doc comment for the `ValidateExt` collision.
            // The struct derives `validator::Validate` only when `has_validation` is
            // set, so the attribute is always registered when present.
            let val_attrs = validate_attrs(f);
            // Encrypted columns route through an AEAD wrapper transparently:
            // `serialize_as` encrypts on write, `deserialize_as` decrypts on read.
            // The public field stays a plain `String` (plaintext in Rust code).
            let enc = encrypted_wrapper_path(
                parse_field_encrypted_mode(f).unwrap_or(EncryptedMode::None),
            )
            .map(|w| quote! { #[diesel(serialize_as = #w, deserialize_as = #w)] });
            // #1374: a `#[private]` column (or an `#[encrypted]` column not
            // opted back in via `admin_visible`) is excluded from the model's
            // `Serialize` impl so it never leaks into JSON responses, while the
            // field itself stays a normal queryable column and the write path
            // (New*/Update*/Changeset) is unaffected. `skip_serializing` (not
            // `skip`) keeps `Deserialize` intact.
            let private = (field_hidden_from_json(f) && !field_already_skips_serialization(f))
                .then(|| quote! { #[serde(skip_serializing)] });
            // #1654: a `#[classified]` column is generated as the taint wrapper
            // rather than a bare `String`, so the classification is a property of the
            // type. Diesel round-trips through the opaque `ClassifiedText` column
            // wrapper, the only conversion out of `Classified` that is not a
            // declassification, and it dead-ends in the database. The Diesel wrapper
            // carries the same marker as the field: an `F`-erasing column type would
            // let a value go in as one column and come back out as another, releasing
            // it through the wrong boundary and recording it against the wrong column.
            let classified_marker = classified_marker_for(f);
            let (ident_ty, classified_diesel) = classified_marker.map_or_else(
                || (quote! { #ty }, quote! {}),
                |marker| {
                    (
                        quote! { ::autumn_web::classify::Classified<#ty, #marker> },
                        quote! {
                            #[diesel(
                                serialize_as = ::autumn_web::classify::ClassifiedText<#marker>,
                                deserialize_as = ::autumn_web::classify::ClassifiedText<#marker>
                            )]
                        },
                    )
                },
            );
            quote! { #(#val_attrs)* #(#attrs)* #enc #classified_diesel #private pub #ident: #ident_ty }
        })
        .collect();

    // Build NewX fields (non-ID, propagate #[validate])
    let new_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            let val_attrs = validate_attrs(f);
            let enc = encrypted_wrapper_path(
                parse_field_encrypted_mode(f).unwrap_or(EncryptedMode::None),
            )
            .map(|w| quote! { #[diesel(serialize_as = #w)] });
            // Non-nullable `bool` columns render as a checkbox in `form_for`
            // (see `emit_form_model_impl`), and an unchecked HTML checkbox
            // submits *no* key at all — a hidden `false` sibling is not an
            // option because serde_urlencoded rejects duplicate keys (see
            // `checkbox_input`'s doc in autumn/src/form.rs). Mark the field
            // `#[serde(default)]` so a missing key decodes as `false` instead
            // of failing with "missing field", mirroring the scaffold's
            // `{Model}Form` convention.
            let bool_default = (!is_option_type(ty) && type_name_str(ty) == "bool")
                .then(|| quote! { #[serde(default)] });
            // Datetime columns render as `<input type="datetime-local">` in
            // `form_for`, whose submitted value carries no timezone offset —
            // chrono's default `Deserialize` for `DateTime<Utc>` would reject
            // even an unchanged pre-filled value as a 400. Wire the tolerant
            // deserializer (which also still accepts RFC 3339 JSON bodies);
            // see `datetime_local_serde_attr`.
            let datetime_local = datetime_local_serde_attr(ty);
            // #1654 (review round 2): the write structs carry the taint wrapper too.
            // Taking personal data in is not a release — `Classified` keeps its
            // `Deserialize`, and the declarative validators see through it (see
            // `autumn/src/classify/validate.rs`), so the form and validation paths
            // are unaffected. What must not happen is the plaintext moving back out:
            // these fields are `pub`, so a bare `String` here let a handler write
            // `Json(View { email: input.email })` and release the value with no
            // boundary and no audit record. `skip_serializing` alone cannot stop that
            // — it governs only serializing the write struct itself.
            let classified = classified_marker_for(f);
            let classified_skip = (classified.is_some()
                && !field_already_skips_serialization(f))
            .then(|| quote! { #[serde(skip_serializing)] });
            let (new_ty, classified_diesel) = classified.map_or_else(
                || (quote! { #ty }, quote! {}),
                |marker| {
                    (
                        quote! { ::autumn_web::classify::Classified<#ty, #marker> },
                        classified_serialize_as(marker),
                    )
                },
            );
            quote! { #(#val_attrs)* #enc #classified_diesel #bool_default #datetime_local #classified_skip pub #ident: #new_ty }
        })
        .collect();

    // Build UpdateX fields:
    // - Regular mutable fields: `Patch<T>`, propagating the field's `#[validate]`
    //   attributes (#1719). The struct derives `validator::Validate` below and
    //   `Patch<T>` implements validator's per-field traits (see
    //   `autumn/src/hooks.rs`), so a failing declarative rule (`length`, `email`,
    //   `url`, `range`, `contains`, …) on a `Set` value surfaces as a 422 on
    //   PATCH/PUT, while an absent `Unchanged`/`Clear` field is skipped, mirroring
    //   create. `required` is the one non-skip rule: its `Patch<T>` impl fails
    //   `Clear`/`Set(None)`, so a PATCH cannot null a required column.
    //   Non-declarative and struct-level validators (`custom`, `must_match`,
    //   `nested`, `credit_card`, `non_control_character`) have no usable `Patch<T>`
    //   impl and are filtered out by `validate_attrs_for_patch`; they still run on
    //   `NewX`. See that helper for the create-vs-update limitation.
    // - `#[lock_version]` field: a plain required `T`. The client supplies the
    //   version it read, and the framework increments it atomically.
    let mut update_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            // #1719: `Patch<T>` only implements validator's per-field
            // declarative traits, so non-declarative/struct-level validators
            // (`custom`, `must_match`, `nested`, …) must be stripped here or the
            // `UpdateModel` would fail to compile or misvalidate. `NewModel`
            // keeps them all.
            let val_attrs = validate_attrs_for_patch(f);
            // #1654: see the `NewX` comment -- the patch carries the wrapper for
            // the same reason. `Patch<T>` forwards `Deserialize` and every
            // declarative validator to `T`, so `Patch<Classified<T, F>>` keeps
            // both.
            let classified = classified_marker_for(f);
            let classified_skip = (classified.is_some() && !field_already_skips_serialization(f))
                .then(|| quote! { #[serde(skip_serializing)] });
            let patch_ty = classified.map_or_else(
                || quote! { #ty },
                |marker| quote! { ::autumn_web::classify::Classified<#ty, #marker> },
            );
            quote! {
                #(#val_attrs)*
                #[serde(default)]
                #classified_skip
                pub #ident: ::autumn_web::hooks::Patch<#patch_ty>
            }
        })
        .collect();
    if let Some(lv_field) = lock_version_field {
        let ident = &lv_field.ident;
        let ty = &lv_field.ty;
        update_fields.push(quote! {
            pub #ident: #ty
        });
    }

    // Build XField enum variants (one per mutable field, PascalCase)
    let field_enum_name = format_ident!("{name}Field");
    let field_enum_variants: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let variant = format_ident!("{}", pascal_case(&ident.to_string()));
            quote! { #variant }
        })
        .collect();

    // Conditional Validate derive
    let validate_derive = if has_validation {
        quote! { #[derive(::autumn_web::reexports::validator::Validate)] }
    } else {
        quote! {}
    };

    // #1778: the statement that validates the effective merged model inside
    // `from_patch` (existing row plus patch). Because `after` is a concrete `#name`
    // and not `Patch<T>`, the model's `#[validate(...)]` rules run against real
    // values, so the validators that hit E0119 coherence walls on `Patch<T>` (`ip` on
    // `Option`, `does_not_contain`) and the cross-field ones (`custom`, `must_match`,
    // `nested`) are all enforced on the update path, returning the same 422 field-error
    // map as create. `nested` still carries its own `ValidateExt` hazard here — see
    // `NON_PATCH_VALIDATORS`'s doc comment (#1751). Runs before the `before_update`
    // hook, mirroring create. Emitted only when the model declares validation.
    let merged_validate_stmt = if has_validation {
        quote! {
            {
                #[allow(unused_imports)]
                use ::autumn_web::validation::{
                    MaybeValidateFallback as _, MaybeValidateViaValidator as _,
                };
                (&::autumn_web::validation::MaybeValidate(&after)).autumn_maybe_validate()?;
            }
        }
    } else {
        quote! {}
    };

    // #1654: a field's generated type -- the taint wrapper for a
    // `#[classified]` column, the declared type otherwise. Everything that
    // touches the field has to agree with the struct definition, so it goes
    // through this.
    let model_field_ty = |f: &Field| -> TokenStream {
        let ty = &f.ty;
        classified_marker_for(f).map_or_else(
            || quote! { #ty },
            |marker| quote! { ::autumn_web::classify::Classified<#ty, #marker> },
        )
    };

    // Build merge arms for `from_patch` (applies Patch fields onto a cloned model)
    let mut merge_arms: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let is_option = is_option_type(&f.ty);
            if is_option {
                quote! {
                    match &patch.#ident {
                        ::autumn_web::hooks::Patch::Set(v) => after.#ident = v.clone(),
                        ::autumn_web::hooks::Patch::Clear => after.#ident = None,
                        ::autumn_web::hooks::Patch::Unchanged => {}
                    }
                }
            } else {
                quote! {
                    match &patch.#ident {
                        ::autumn_web::hooks::Patch::Set(v) => after.#ident = v.clone(),
                        ::autumn_web::hooks::Patch::Clear => {
                            return Err(::autumn_web::AutumnError::bad_request_msg(
                                format!("Cannot clear non-nullable field `{}`", stringify!(#ident))
                            ));
                        }
                        ::autumn_web::hooks::Patch::Unchanged => {}
                    }
                }
            }
        })
        .collect();
    // For #[lock_version] fields, from_patch always increments the version in
    // `after` by one — the client-supplied patch.{field} is the expected
    // (before) version; the repository validates it and the changeset carries
    // the incremented value into the DB.
    if let Some(lv_field) = lock_version_field {
        let ident = lv_field.ident.as_ref().unwrap();
        merge_arms.push(quote! {
            after.#ident = current.#ident.wrapping_add(1);
        });
    }

    // Build per-field DraftField accessor method signatures (for the trait)
    let draft_accessor_sigs: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let ty = model_field_ty(f);
            quote! {
                fn #ident(&mut self) -> ::autumn_web::hooks::DraftField<'_, #ty>;
            }
        })
        .collect();

    // Build per-field DraftField accessor method implementations
    let draft_accessors: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let ty = model_field_ty(f);
            quote! {
                fn #ident(&mut self) -> ::autumn_web::hooks::DraftField<'_, #ty> {
                    ::autumn_web::hooks::DraftField::new(&self.before.#ident, &mut self.after.#ident)
                }
            }
        })
        .collect();

    // Trait name for draft extension methods
    let draft_ext_name = format_ident!("{name}DraftExt");

    let column_count = all_fields.len();
    let new_column_count = fields_for_new.len();

    // Build Diesel-compatible changeset bridge (private struct with Option<T> fields)
    // (`changeset_name` is bound earlier so the redacting Debug impl can use it.)

    let tenant_id_field = all_fields
        .iter()
        .find(|f| f.ident.as_ref().is_some_and(|id| id == "tenant_id"))
        .copied();

    // `__autumn_preload_retain` applies this model's read scoping, in memory, to rows
    // loaded by another model's `preload`, so eager-loaded associations hide the same
    // rows the model's repository finders do. It is built from the model's own field
    // set, which the loading model cannot see: soft-delete drops `deleted_at IS NOT
    // NULL`, and tenant scoping keeps only rows matching the ambient `CURRENT_TENANT`
    // when one is set.
    //
    // Do not add an `if rows.is_empty() { return Ok(rows); }` early return ahead of the
    // tenant check below. Many-to-many preload loaders (`through =`) call
    // `__autumn_preload_retain(Vec::new())` as a fail-closed parity probe, precisely to
    // get the "no tenant context" error even when their join returns zero rows. An
    // empty-input early return would skip that check and break tenant isolation for a
    // whole class of m2m preloads. See
    // `preload_retain_empty_rows_still_fails_closed_without_tenant` below.
    let deleted_at_field = all_fields
        .iter()
        .find(|f| f.ident.as_ref().is_some_and(|id| id == "deleted_at"))
        .copied();
    // Soft-delete retain. Gated at *runtime* on the model's repository being
    // declared `soft_delete` (via the inherent override of
    // `AutumnPreloadScopeExt::__autumn_repo_soft_delete_scope`), so a model that
    // merely *has* a `deleted_at` column (e.g. audit/history) but whose
    // repository is not `soft_delete` is left unfiltered — matching its
    // finders. The field check below is only the compile-time column guard.
    let soft_delete_retain = match deleted_at_field {
        Some(f) if is_option_type(&f.ty) => quote! {
            if <Self>::__autumn_repo_soft_delete_scope() {
                rows.retain(|__r| ::core::option::Option::is_none(&__r.deleted_at));
            }
        },
        _ => quote! {},
    };
    // Tenant retain. Gated at runtime on the repository being `tenant_scoped`
    // (inherent override of `__autumn_repo_tenant_scope`) AND not running under
    // `across_tenants()` (the ambient `preload_across_tenants()` flag a
    // repository's `preload` publishes). Field presence is only the column
    // guard; a `tenant_id` column without a `tenant_scoped` repository stays
    // unfiltered, matching finders.
    let tenant_retain = tenant_id_field.as_ref().map_or_else(
        || quote! {},
        |f| {
            let cmp = if is_option_type(&f.ty) {
                quote! { __r.tenant_id.as_deref() == ::core::option::Option::Some(__t.as_str()) }
            } else {
                quote! { __r.tenant_id == __t }
            };
            quote! {
                if <Self>::__autumn_repo_tenant_scope()
                    && !::autumn_web::preload::preload_across_tenants()
                {
                    match ::autumn_web::tenancy::CURRENT_TENANT
                        .try_with(|__c| __c.clone())
                        .ok()
                        .flatten()
                    {
                        ::core::option::Option::Some(__t) => {
                            rows.retain(|__r| #cmp);
                        }
                        // Fail closed, exactly like a tenant-scoped finder:
                        // never attach cross-tenant rows when tenant context is
                        // missing (job/admin path that lost the tenant, etc.).
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                ::autumn_web::AutumnError::internal_server_error_msg(
                                    "Query scoped to tenant, but no tenant context was established"
                                )
                            );
                        }
                    }
                }
            }
        },
    );
    let preload_scope_in_scope = if deleted_at_field.is_some() || tenant_id_field.is_some() {
        // Bring the default-`false` trait into scope so `Self::…scope()`
        // resolves to the blanket default when the repository macro emitted no
        // inherent override (inherent wins when it exists).
        quote! { use ::autumn_web::preload::AutumnPreloadScopeExt as _; }
    } else {
        quote! {}
    };
    let preload_retain_rows = if deleted_at_field.is_some() || tenant_id_field.is_some() {
        quote! { mut rows }
    } else {
        quote! { rows }
    };
    let preload_retain_impl = quote! {
        impl #name {
            /// Apply this model's repository read scoping (tenant isolation +
            /// soft-delete) to rows loaded by another model's `preload`, so
            /// preloaded associations hide the same rows the model's finders
            /// do. Gated on the repository's `tenant_scoped`/`soft_delete`
            /// config (see `AutumnPreloadScopeExt`); identity for models whose
            /// repository opts out (or has no `tenant_id`/`deleted_at`). Fails
            /// closed — like a tenant-scoped finder — when the target is
            /// tenant-scoped but no tenant context is set.
            #[doc(hidden)]
            pub fn __autumn_preload_retain(
                #preload_retain_rows: ::std::vec::Vec<Self>,
            ) -> ::autumn_web::AutumnResult<::std::vec::Vec<Self>> {
                #preload_scope_in_scope
                #soft_delete_retain
                #tenant_retain
                ::core::result::Result::Ok(rows)
            }

            /// Per-row sibling of `__autumn_preload_retain`, applying the
            /// identical scoping rules to a single row. Used by many-to-many
            /// (`through =`) preload loaders, which pair each child row with
            /// its parent key before grouping and so can't run a batch
            /// `Vec<Self>::retain` without losing that pairing. Takes `row`
            /// by value (no `Clone` bound needed) and hands it back on
            /// `Some` when it passes scoping. Delegates to
            /// `__autumn_preload_retain` (a single-row batch) rather than
            /// re-deriving the soft-delete/tenant predicates, so the two
            /// can never drift apart.
            #[doc(hidden)]
            pub fn __autumn_preload_keep(
                row: Self,
            ) -> ::autumn_web::AutumnResult<::core::option::Option<Self>> {
                let mut __kept = <Self>::__autumn_preload_retain(::std::vec![row])?;
                ::core::result::Result::Ok(__kept.pop())
            }
        }
    };

    let new_has_tenant_id = fields_for_new
        .iter()
        .any(|f| f.ident.as_ref().is_some_and(|id| id == "tenant_id"));

    let can_set_tenant_id_impl = if new_has_tenant_id {
        let f = fields_for_new
            .iter()
            .find(|f| f.ident.as_ref().is_some_and(|id| id == "tenant_id"))
            .unwrap();
        let is_option = is_option_type(&f.ty);
        let val = if is_option {
            quote! { ::core::option::Option::Some(::core::option::Option::Some(t)) }
        } else {
            quote! { ::core::option::Option::Some(t) }
        };
        quote! {
            impl ::autumn_web::repository::CanSetTenantId for #changeset_name {
                fn set_tenant_id(&mut self, t: ::std::string::String) {
                    self.tenant_id = #val;
                }
            }
        }
    } else {
        quote! {
            impl ::autumn_web::repository::CanSetTenantId for #changeset_name {
                fn set_tenant_id(&mut self, _t: ::std::string::String) {}
            }
        }
    };

    let model_tenant_id_meta_impl = tenant_id_field.as_ref().map_or_else(
        || {
            quote! {
                impl ::autumn_web::tenancy::ModelTenantIdMeta for #new_name {
                    const HAS_MANUAL_TENANT_ID: bool = false;
                    fn try_set_tenant_id(&mut self, _tenant_id: &str) {}
                }
                impl ::autumn_web::tenancy::ModelTenantIdMeta for #name {
                    const HAS_MANUAL_TENANT_ID: bool = false;
                    fn try_set_tenant_id(&mut self, _tenant_id: &str) {}
                }
            }
        },
        |f| {
            let is_option = is_option_type(&f.ty);
            let set_field = if is_option {
                quote! { self.tenant_id = ::core::option::Option::Some(tenant_id.to_string()); }
            } else {
                quote! { self.tenant_id = tenant_id.to_string(); }
            };

            let new_set_field = if new_has_tenant_id {
                set_field.clone()
            } else {
                quote! {}
            };

            quote! {
                impl ::autumn_web::tenancy::ModelTenantIdMeta for #new_name {
                    const HAS_MANUAL_TENANT_ID: bool = #new_has_tenant_id;
                    fn try_set_tenant_id(&mut self, tenant_id: &str) {
                        #new_set_field
                    }
                }
                impl ::autumn_web::tenancy::ModelTenantIdMeta for #name {
                    const HAS_MANUAL_TENANT_ID: bool = true;
                    fn try_set_tenant_id(&mut self, tenant_id: &str) {
                        #set_field
                    }
                }
            }
        },
    );

    let mut upsert_columns: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            quote! {
                #table_ident::#ident.eq(::autumn_web::reexports::diesel::upsert::excluded(#table_ident::#ident))
            }
        })
        .collect();

    if upsert_columns.is_empty() {
        upsert_columns.push(quote! {
            #table_ident::id.eq(::autumn_web::reexports::diesel::pg::upsert::excluded(#table_ident::id))
        });
    }

    if let Some(lv_field) = lock_version_field {
        let ident = lv_field.ident.as_ref().unwrap();
        upsert_columns.push(quote! {
            #table_ident::#ident.eq(#table_ident::#ident + 1)
        });
    }

    let mut upsert_types: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            quote! {
                ::autumn_web::reexports::diesel::dsl::Eq<
                    #table_ident::#ident,
                    ::autumn_web::reexports::diesel::upsert::Excluded<#table_ident::#ident>
                >
            }
        })
        .collect();

    if upsert_types.is_empty() {
        upsert_types.push(quote! {
            ::autumn_web::reexports::diesel::dsl::Eq<
                #table_ident::id,
                ::autumn_web::reexports::diesel::upsert::Excluded<#table_ident::id>
            >
        });
    }

    if let Some(lv_field) = lock_version_field {
        let ident = lv_field.ident.as_ref().unwrap();
        let ty = &lv_field.ty;
        upsert_types.push(quote! {
            ::autumn_web::reexports::diesel::dsl::Eq<
                #table_ident::#ident,
                ::autumn_web::reexports::diesel::helper_types::Add<
                    #table_ident::#ident,
                    ::autumn_web::reexports::diesel::expression::bound::Bound<
                        <#table_ident::#ident as ::autumn_web::reexports::diesel::Expression>::SqlType,
                        #ty
                    >
                >
            >
        });
    }

    let has_tenant_id = tenant_id_field.is_some();
    let execute_upsert_body = if has_tenant_id {
        lock_version_field.map_or_else(
            || quote! {
                if let ::core::option::Option::Some(t) = tenant_id {
                    let stmt = ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, #table_ident::tenant_id.eq(t.to_string()));
                    stmt.get_results::<Self>(conn).await
                } else {
                    stmt.get_results::<Self>(conn).await
                }
            },
            |lv_field| {
                let lv_ident = lv_field.ident.as_ref().unwrap();
                quote! {
                    let lv_cond = #table_ident::#lv_ident.eq(::autumn_web::reexports::diesel::pg::upsert::excluded(#table_ident::#lv_ident));
                    if let ::core::option::Option::Some(t) = tenant_id {
                        let stmt = ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, lv_cond.and(#table_ident::tenant_id.eq(t.to_string())));
                        stmt.get_results::<Self>(conn).await
                    } else {
                        let stmt = ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, lv_cond);
                        stmt.get_results::<Self>(conn).await
                    }
                }
            },
        )
    } else {
        lock_version_field.map_or_else(
            || quote! {
                stmt.get_results::<Self>(conn).await
            },
            |lv_field| {
                let lv_ident = lv_field.ident.as_ref().unwrap();
                quote! {
                    let lv_cond = #table_ident::#lv_ident.eq(::autumn_web::reexports::diesel::pg::upsert::excluded(#table_ident::#lv_ident));
                    let stmt = ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, lv_cond);
                    stmt.get_results::<Self>(conn).await
                }
            },
        )
    };

    // SQLite counterpart of `#execute_upsert_body`: the per-row loop body that builds
    // one `INSERT … ON CONFLICT(id) DO UPDATE …`, applies the same tenant and
    // lock-version `DO UPDATE … WHERE` refinements Postgres uses — SQLite's `ON
    // CONFLICT DO UPDATE` supports a `WHERE` clause — runs it, and pushes any RETURNING
    // row (#1996, PR #2021). Matching Postgres's WHERE is load-bearing twice over:
    //   * tenant isolation (security): the conflict target is `id` alone and the shared
    //     set-clause (`__autumn_upsert_set`) writes `tenant_id`, so without `WHERE
    //     tenant_id = <current>` an id owned by another tenant would be moved into the
    //     caller's tenant. With the predicate the DO UPDATE does not fire for a
    //     cross-tenant id: the row is left untouched and drops out of RETURNING.
    //   * optimistic locking: `WHERE lock_version = excluded.lock_version` leaves a
    //     stale-version row untouched, dropping it from RETURNING.
    // A dropped row yields no RETURNING output, so per-row `get_result` returns
    // `Error::NotFound` and `.optional()?` maps that to `None`, leaving the row absent
    // from the returned Vec — exactly as Postgres's `get_results` omits filtered rows.
    // The caller's fail-closed reconciliation then behaves identically on both backends.
    let execute_upsert_body_sqlite = {
        let build_stmt = quote! {
            let stmt = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                .values(__autumn_row)
                .on_conflict(#table_ident::id)
                .do_update()
                .set(Self::__autumn_upsert_set());
        };
        if has_tenant_id {
            lock_version_field.map_or_else(
                || quote! {
                    #build_stmt
                    let __autumn_r = if let ::core::option::Option::Some(t) = tenant_id {
                        ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, #table_ident::tenant_id.eq(t.to_string()))
                            .get_result::<Self>(conn)
                            .await
                            .optional()?
                    } else {
                        stmt.get_result::<Self>(conn).await.optional()?
                    };
                    if let ::core::option::Option::Some(__autumn_r) = __autumn_r {
                        __autumn_upserted.push(__autumn_r);
                    }
                },
                |lv_field| {
                    let lv_ident = lv_field.ident.as_ref().unwrap();
                    quote! {
                        #build_stmt
                        let lv_cond = #table_ident::#lv_ident.eq(::autumn_web::reexports::diesel::upsert::excluded(#table_ident::#lv_ident));
                        let __autumn_r = if let ::core::option::Option::Some(t) = tenant_id {
                            ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, lv_cond.and(#table_ident::tenant_id.eq(t.to_string())))
                                .get_result::<Self>(conn)
                                .await
                                .optional()?
                        } else {
                            ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, lv_cond)
                                .get_result::<Self>(conn)
                                .await
                                .optional()?
                        };
                        if let ::core::option::Option::Some(__autumn_r) = __autumn_r {
                            __autumn_upserted.push(__autumn_r);
                        }
                    }
                },
            )
        } else {
            lock_version_field.map_or_else(
                || quote! {
                    #build_stmt
                    let __autumn_r = stmt.get_result::<Self>(conn).await.optional()?;
                    if let ::core::option::Option::Some(__autumn_r) = __autumn_r {
                        __autumn_upserted.push(__autumn_r);
                    }
                },
                |lv_field| {
                    let lv_ident = lv_field.ident.as_ref().unwrap();
                    quote! {
                        #build_stmt
                        let lv_cond = #table_ident::#lv_ident.eq(::autumn_web::reexports::diesel::upsert::excluded(#table_ident::#lv_ident));
                        let __autumn_r = ::autumn_web::reexports::diesel::query_dsl::methods::FilterDsl::filter(stmt, lv_cond)
                            .get_result::<Self>(conn)
                            .await
                            .optional()?;
                        if let ::core::option::Option::Some(__autumn_r) = __autumn_r {
                            __autumn_upserted.push(__autumn_r);
                        }
                    }
                },
            )
        }
    };

    // Field-by-field equality, shared by `__autumn_correlate_model` (two models)
    // and `__autumn_correlate_new` (a `New*` input against a persisted model).
    // #1654: one expression serves both because the write struct carries the
    // same taint wrapper as the model, so the two sides always have the same
    // field types. Comparing `Classified` to `Classified` also keeps the
    // correlation off the release path -- unwrapping either side to compare
    // would be an unrecorded release.
    let compare_fields = fields_for_new.iter().map(|f| {
        let ident = &f.ident;
        quote! { input.#ident == record.#ident }
    });
    let compare_expr = if fields_for_new.is_empty() {
        quote! { true }
    } else {
        quote! { #(#compare_fields)&&* }
    };

    let mut changeset_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            // For both nullable and non-nullable columns, Diesel's AsChangeset
            // treats Option<T> as "skip if None, set if Some". For nullable
            // columns (Option<Inner>), this becomes Option<Option<Inner>> which
            // also handles "set to NULL" via Some(None).
            //
            // For encrypted columns the inner value is routed through the AEAD
            // wrapper via `serialize_as` (Diesel maps the `Option` skip itself),
            // so updates write ciphertext while the API stays plaintext.
            let enc = encrypted_wrapper_path(
                parse_field_encrypted_mode(f).unwrap_or(EncryptedMode::None),
            )
            .map(|w| quote! { #[diesel(serialize_as = #w)] });
            // #1654: the changeset is built from `UpdateX`'s `Patch` values, so
            // it holds the wrapper too and writes through the opaque Diesel
            // column type -- the same round trip the read struct uses, and the
            // only conversion out of `Classified` that is not a declassification.
            let (changeset_ty, classified_diesel) = classified_marker_for(f).map_or_else(
                || (quote! { #ty }, quote! {}),
                |marker| {
                    (
                        quote! { ::autumn_web::classify::Classified<#ty, #marker> },
                        classified_serialize_as(marker),
                    )
                },
            );
            quote! { #enc #classified_diesel pub #ident: Option<#changeset_ty> }
        })
        .collect();
    // The lock_version column must be in the changeset so the UPDATE can
    // atomically bump it to current+1.
    if let Some(lv_field) = lock_version_field {
        let ident = &lv_field.ident;
        let ty = &lv_field.ty;
        changeset_fields.push(quote! { pub #ident: Option<#ty> });
    }

    let mut changeset_conversions: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let is_option = is_option_type(&f.ty);
            if is_option {
                // For nullable fields: Set(v) -> Some(v), Clear -> Some(None), Unchanged -> None
                quote! {
                    #ident: match &self.#ident {
                        ::autumn_web::hooks::Patch::Set(v) => Some(v.clone()),
                        ::autumn_web::hooks::Patch::Clear => Some(None),
                        ::autumn_web::hooks::Patch::Unchanged => None,
                    }
                }
            } else {
                // For non-nullable fields: Set(v) -> Some(v), Unchanged -> None, Clear -> panic
                quote! {
                    #ident: match &self.#ident {
                        ::autumn_web::hooks::Patch::Set(v) => Some(v.clone()),
                        ::autumn_web::hooks::Patch::Clear => {
                            panic!("Cannot clear non-nullable field `{}`", stringify!(#ident));
                        }
                        ::autumn_web::hooks::Patch::Unchanged => None,
                    }
                }
            }
        })
        .collect();
    // The lock_version field in UpdateX holds the version the client expects;
    // the changeset always sets it to current+1 (wrapping to avoid overflow).
    if let Some(lv_field) = lock_version_field {
        let ident = lv_field.ident.as_ref().unwrap();
        changeset_conversions.push(quote! {
            #ident: Some(self.#ident.wrapping_add(1))
        });
    }

    // ── Factory builder ────────────────────────────────────────
    // PK ident/type used for __autumn_pk() — fall back to a dummy `id: i64` if
    // no PK can be detected (the factory will fail to compile at the call site,
    // which is a better diagnostic than a macro panic).
    let (pk_id, pk_ty): (&syn::Ident, &syn::Type) = pk_field_for_factory.unwrap_or_else(|| {
        // Dummy values — unreachable for well-formed models, which always
        // have at least one i32/i64 field or an explicit #[id] annotation.
        panic!("#[model]: could not detect primary-key field for factory generation")
    });

    let model_primary_key_impl = quote! {
        impl ::autumn_web::repository::ModelPrimaryKey for #name {
            type IdType = #pk_ty;
            fn primary_key_value(&self) -> Self::IdType {
                ::core::clone::Clone::clone(&self.#pk_id)
            }
        }
    };

    // Whether any factory field is an association (drives depth-check generation).
    let has_assoc_fields = fields_for_new
        .iter()
        .any(|f| factory_assoc_type(f).is_some());

    // Factory struct fields.
    // - Normal fields:  `pub {ident}: {ty}`
    // - Assoc fields:   `pub {ident}: Option<{ty}>` (None = auto-create on create())
    let factory_struct_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            if factory_assoc_type(f).is_some() {
                quote! { pub #ident: ::core::option::Option<#ty> }
            } else {
                // #2373: the factory field is `pub` and lives for the whole
                // builder chain, so a bare `String` here was a way around the
                // guarantee entirely -- `Customer::factory().email` could be
                // moved into a serializable view with no boundary and no audit
                // record. Classifying only at `build()` time was too late. The
                // factory is emitted in every build, not just test ones.
                let field_ty = model_field_ty(f);
                quote! { pub #ident: #field_ty }
            }
        })
        .collect();

    // Default impl.
    // - Normal fields:  `{ident}: Default::default()`
    // - Assoc fields:   `{ident}: None`
    let factory_default_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            if factory_assoc_type(f).is_some() {
                quote! { #ident: ::core::option::Option::None }
            } else {
                quote! { #ident: ::core::default::Default::default() }
            }
        })
        .collect();

    // Per-field setter methods.
    // - Normal fields:  `pub fn {ident}(mut self, val: impl Into<T>) -> Self`
    // - Assoc fields:   same setter (stores `Some(val.into())`), PLUS
    //                   `pub fn {assoc_snake}(mut self, val: &AssocType) -> Self`
    //                   that extracts `.id` from a pre-built instance.
    let factory_setters: Vec<TokenStream> = fields_for_new
        .iter()
        .flat_map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let ty = &f.ty;
            // The field-name string recorded in `__autumn_set` when this setter
            // runs, so `.fake()` skips explicitly-set fields. Must match the
            // string used by the build/create fake bindings below.
            let field_lit = ident.to_string();

            factory_assoc_type(f).map_or_else(
                // Normal field: a single setter that assigns directly.
                || {
                    // #2373: the setter keeps its `impl Into<#ty>` bound over the
                    // declared type and classifies inside. Binding it to the wrapper
                    // would break every `.email("ada@example.com")` call, because
                    // `Classified<String, F>` converts only from `String`, not from
                    // `&str`, and a blanket `From<U: Into<T>>` would overlap the
                    // existing `From<T>`. Taking plaintext in is not a release, so the
                    // setter is the right place to wrap: the value is classified from
                    // the moment it is stored, and callers are unaffected.
                    let store = if classified_marker_for(f).is_some() {
                        quote! { ::autumn_web::classify::Classified::new(val.into()) }
                    } else {
                        quote! { val.into() }
                    };
                    vec![quote! {
                        #[must_use]
                        pub fn #ident(mut self, val: impl ::core::convert::Into<#ty>) -> Self {
                            self.#ident = #store;
                            self.__autumn_set.insert(#field_lit);
                            self
                        }
                    }]
                },
                // Assoc field: two setters — explicit id and pre-built instance.
                |assoc_type| {
                    let explicit_setter = quote! {
                        #[must_use]
                        pub fn #ident(mut self, val: impl ::core::convert::Into<#ty>) -> Self {
                            self.#ident = ::core::option::Option::Some(val.into());
                            self.__autumn_set.insert(#field_lit);
                            self
                        }
                    };
                    // Name derived from the field ident by stripping the `_id` suffix:
                    // `user_id` → `.user()`, `author_id` → `.author()`.
                    let field_str = ident.to_string();
                    let assoc_snake = if field_str.ends_with("_id") {
                        format_ident!("{}", &field_str[..field_str.len() - 3])
                    } else {
                        format_ident!("{}_assoc", field_str)
                    };
                    let pre_built_setter = quote! {
                        /// Override the association with a pre-built instance.
                        /// Extracts the primary key so no additional DB insert is performed on `create()`.
                        #[must_use]
                        pub fn #assoc_snake(mut self, val: &#assoc_type) -> Self {
                            self.#ident = ::core::option::Option::Some(val.__autumn_pk());
                            self.__autumn_set.insert(#field_lit);
                            self
                        }
                    };
                    vec![explicit_setter, pre_built_setter]
                },
            )
        })
        .collect();

    // Per-field value bindings for non-assoc fields, honoring `.fake()`.
    //
    // In `.fake()` mode, a field not explicitly set via its setter draws a fake value
    // inferred from the field name and type; otherwise the value already stored on the
    // factory is used. Each binding is a `let {ident} = …;`, so both `build()` and
    // `create()` can construct the record with struct shorthand.
    //
    // `.clone()`, rather than moving `self.{ident}`, is required because the fake
    // branch reads `self.__autumn_fake`/`self.__autumn_set` and the value is only
    // conditionally consumed. All `NewX` field types are `Clone`.
    let factory_value_bindings: Vec<TokenStream> = fields_for_new
        .iter()
        .filter(|f| factory_assoc_type(f).is_none())
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let field_lit = ident.to_string();
            // #2373: the stored field is now the wrapper, but `fake_expr_for_field`
            // yields a plain value, so the generated side is classified to match.
            let fake_wrap = |expr: TokenStream| -> TokenStream {
                if classified_marker_for(f).is_some() {
                    quote! { ::autumn_web::classify::Classified::new(#expr) }
                } else {
                    expr
                }
            };
            fake_expr_for_field(ident, &f.ty).map_or_else(
                // No fake expression available for this type: leave the value
                // as-is (its Default when `.fake()` was requested).
                || {
                    quote! {
                        let #ident = ::core::clone::Clone::clone(&self.#ident);
                    }
                },
                |fake_expr| {
                    let fake_expr = fake_wrap(fake_expr);
                    quote! {
                        let #ident = if self.__autumn_fake
                            && !self.__autumn_set.contains(#field_lit)
                        {
                            #fake_expr
                        } else {
                            ::core::clone::Clone::clone(&self.#ident)
                        };
                    }
                },
            )
        })
        .collect();

    // build(): assoc fields resolve to their supplied value or `Default`.
    let build_assoc_bindings: Vec<TokenStream> = fields_for_new
        .iter()
        .filter_map(|f| {
            factory_assoc_type(f)?;
            let ident = f.ident.as_ref().unwrap();
            Some(quote! {
                let #ident = self.#ident.unwrap_or_default();
            })
        })
        .collect();

    // create() — auto-resolve assoc fields, then insert.
    //
    // For each assoc field, bind `{ident}` to either the supplied value or an
    // auto-created associated model's primary key.
    //
    // A task-local depth counter guards against cyclic associations: if the
    // chain exceeds 32 levels the factory panics with a clear message rather than
    // overflowing the stack.
    let create_assoc_bindings: Vec<TokenStream> = fields_for_new
        .iter()
        .filter_map(|f| {
            let assoc_type = factory_assoc_type(f)?;
            let ident = f.ident.as_ref().unwrap();
            Some(quote! {
                let #ident = match self.#ident {
                    ::core::option::Option::Some(id) => id,
                    ::core::option::Option::None => {
                        #assoc_type::factory().create(pool).await.__autumn_pk()
                    }
                };
            })
        })
        .collect();

    // Struct-shorthand field list for `NewX { … }` (local bindings named to match).
    // #2373: the factory now stores the wrapper, so the binding already has the
    // write struct's field type and no conversion is needed here.
    let new_construct_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            quote! { #ident }
        })
        .collect();

    // create() inner body — shared by both the assoc and non-assoc paths.
    let create_inner_body = quote! {
        use ::autumn_web::reexports::diesel::prelude::*;
        use ::autumn_web::reexports::diesel_async::RunQueryDsl;

        #(#factory_value_bindings)*
        #(#create_assoc_bindings)*

        let new_record = #new_name {
            #(#new_construct_fields,)*
        };
        let mut conn = pool
            .get()
            .await
            .expect("factory: failed to acquire db connection");
        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
            // Owned (not `&new_record`): encrypted columns route through diesel
            // `serialize_as`, which consumes the value, so `Insertable` is only
            // implemented for the owned record. Owned also works for plain models.
            .values(new_record)
            .returning(#name::as_returning())
            .get_result(&mut *conn)
            .await
            .expect("factory: insert failed")
    };

    // create() — insert via Diesel and return the persisted model.
    //
    // For models with #[factory_assoc] fields, the body is wrapped in a
    // `tokio::task_local` scope so the depth counter is maintained correctly
    // when the future migrates between worker threads (work-stealing runtimes).
    // Thread-local storage would corrupt the counter across await points.
    let factory_create_method = if has_assoc_fields {
        quote! {
            /// Insert a record built from this factory into the database and return
            /// the fully-populated model (with server-assigned primary key).
            ///
            /// Fields annotated with `#[factory_assoc(Type)]` are auto-created via
            /// `Type::factory().create(pool).await` when no explicit value was set.
            /// Supply a pre-built instance with the `.{type_snake}(instance)` setter
            /// to skip the extra insert.
            ///
            /// Panics if the insert fails or if a cyclic association chain is detected
            /// (depth > 32).
            pub async fn create(
                self,
                pool: &::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                    ::autumn_web::RuntimeConnection,
                >,
            ) -> #name {
                let __depth = ::autumn_web::__private::FACTORY_DEPTH
                    .try_with(|d| d + 1)
                    .unwrap_or(1_u32);
                assert!(
                    __depth <= 32,
                    "factory `{}`: cyclic #[factory_assoc] chain exceeds depth 32 — \
                     break the cycle by supplying a pre-built instance via a pre-built setter.",
                    stringify!(#name),
                );
                ::autumn_web::__private::FACTORY_DEPTH
                    .scope(__depth, async move { #create_inner_body })
                    .await
            }
        }
    } else {
        quote! {
            /// Insert a record built from this factory into the database and return
            /// the fully-populated model (with server-assigned primary key).
            ///
            /// Panics if the insert fails.
            pub async fn create(
                self,
                pool: &::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                    ::autumn_web::RuntimeConnection,
                >,
            ) -> #name {
                #create_inner_body
            }
        }
    };

    // create_many() — persist `count` records, cloning the factory per row so
    // each iteration takes the same create() path. Under `.fake()`, every row
    // re-draws its fake fields, yielding distinct rows (and distinct DB-assigned
    // primary keys). Mirrors `create`'s signature exactly (returns Vec<#name>).
    let factory_create_many_method = quote! {
        /// Insert `count` records built from this factory and return them.
        ///
        /// The factory is cloned for each row, so with `.fake()` each record
        /// gets freshly-generated field values. Without `.fake()` the records
        /// are identical apart from database-assigned primary keys.
        ///
        /// Panics if any insert fails.
        pub async fn create_many(
            self,
            count: usize,
            pool: &::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                ::autumn_web::RuntimeConnection,
            >,
        ) -> ::std::vec::Vec<#name> {
            let mut out = ::std::vec::Vec::with_capacity(count);
            for _ in 0..count {
                out.push(::core::clone::Clone::clone(&self).create(pool).await);
            }
            out
        }
    };

    // ── Optimistic-lock helper bodies ──────────────────────────────────────
    // Generate the bodies for the two hidden lock-version methods.
    // For models without #[lock_version] both bodies return `None` so the
    // repository macro can call them unconditionally.
    let lock_version_actual_body: TokenStream = lock_version_field.map_or_else(
        || quote! { ::core::option::Option::None },
        |lv_field| {
            let ident = lv_field.ident.as_ref().unwrap();
            quote! { ::core::option::Option::Some(self.#ident as i64) }
        },
    );

    let lock_version_expected_body: TokenStream = lock_version_field.map_or_else(
        || quote! { ::core::option::Option::None },
        |lv_field| {
            let ident = lv_field.ident.as_ref().unwrap();
            quote! { ::core::option::Option::Some(self.#ident as i64) }
        },
    );

    // Generate `pub fn etag(&self) -> ::autumn_web::etag::ETag` only when the
    // model carries a `#[lock_version]` field.  For models without one, the
    // method is omitted entirely — it would be meaningless.
    let etag_method: TokenStream = lock_version_field.map_or_else(
        || quote! {},
        |lv_field| {
            let ident = lv_field.ident.as_ref().unwrap();
            quote! {
                /// Derive an ETag from this model's lock version.
                ///
                /// Use with `autumn_web::etag::fresh_when` for one-liner
                /// conditional-GET support:
                ///
                /// ```rust,ignore
                /// let fw = fresh_when(&headers, post.etag());
                /// Ok(fw.or(html! { ... }))
                /// ```
                ///
                /// The ETag is deterministic: same `lock_version` ⇒ same ETag
                /// on every replica, with no dependence on wall clock or RNG.
                #[inline]
                pub fn etag(&self) -> ::autumn_web::etag::ETag {
                    ::autumn_web::etag::IntoETag::into_etag(self.#ident as i64)
                }
            }
        },
    );

    // Compute schema bodies for OpenApiSchema impls.
    // all_fields is Vec<&Field>; emit_schema_fn_body expects &[&&Field].
    // Thread the container `#[serde(rename_all)]` rule so the advertised schema
    // property names match the wire names the (de)serialized struct uses.
    let schema_rename_all_rule = serde_rename_all_serialize_rule(outer_attrs);
    let schema_rename_all_rule = schema_rename_all_rule.as_deref();
    let all_field_refs: Vec<&&Field> = all_fields.iter().collect();
    // #1654: a classified column has no `Serialize` impl, so it can never appear
    // in a JSON body. Advertising it in the read schema would document a property
    // no response can carry. The write schemas keep it: a client still *sets* it.
    let serializable_field_refs: Vec<&&Field> = all_field_refs
        .iter()
        .filter(|f| !field_is_classified(f))
        .copied()
        .collect();
    let query_struct_schema_body =
        emit_schema_fn_body(&serializable_field_refs, false, schema_rename_all_rule);
    let new_struct_schema_body =
        emit_schema_fn_body(&fields_for_new, false, schema_rename_all_rule);
    let update_struct_schema_body = {
        let extra: &[&&Field] = lock_version_field.as_slice();
        emit_schema_fn_body_ext(&fields_for_new, true, extra, schema_rename_all_rule)
    };
    let commit_hook_serialize_fields: Vec<TokenStream> = all_fields
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().expect("named field");
            let ty = &f.ty;
            let field_name = LitStr::new(&ident.to_string(), ident.span());
            let field_value = if has_hook_serde_adapter(f, SerdeAdapterMode::Serialize) {
                let serde_attrs = hook_serde_adapter_attrs(f, SerdeAdapterMode::Serialize);
                quote! {
                    {
                        #[derive(::serde::Serialize)]
                        struct __AutumnCommitHookSerializeField {
                            #(#serde_attrs)*
                            value: #ty,
                        }
                        let __autumn_field = __AutumnCommitHookSerializeField {
                            value: self.#ident.clone(),
                        };
                        let __autumn_field_value =
                            ::autumn_web::reexports::serde_json::to_value(&__autumn_field)
                                .map_err(|__error| {
                                    ::autumn_web::AutumnError::internal_server_error_msg(format!(
                                        "serialize repository commit hook record field {}.{}: {}",
                                        stringify!(#name),
                                        #field_name,
                                        __error
                                    ))
                                })?;
                        match __autumn_field_value {
                            ::autumn_web::reexports::serde_json::Value::Object(mut __autumn_field_object) => {
                                __autumn_field_object.remove("value").ok_or_else(|| {
                                    ::autumn_web::AutumnError::internal_server_error_msg(format!(
                                        "serialize repository commit hook record field {}.{}: missing adapter output",
                                        stringify!(#name),
                                        #field_name
                                    ))
                                })?
                            }
                            __autumn_other => {
                                return Err(::autumn_web::AutumnError::internal_server_error_msg(format!(
                                    "serialize repository commit hook record field {}.{}: expected adapter object, got {}",
                                    stringify!(#name),
                                    #field_name,
                                    __autumn_other
                                )));
                            }
                        }
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::serde_json::to_value(&self.#ident)
                        .map_err(|__error| {
                            ::autumn_web::AutumnError::internal_server_error_msg(format!(
                                "serialize repository commit hook record field {}.{}: {}",
                                stringify!(#name),
                                #field_name,
                                __error
                            ))
                        })?
                }
            };
            quote! {
                __autumn_object.insert(
                    ::std::string::String::from(#field_name),
                    #field_value
                );
            }
        })
        .collect();
    let commit_hook_deserialize_fields: Vec<TokenStream> = all_fields
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().expect("named field");
            let ty = &f.ty;
            let field_name = LitStr::new(&ident.to_string(), ident.span());
            let missing_default = commit_hook_missing_field_default_expr(f);
            let field_value = if has_hook_serde_adapter(f, SerdeAdapterMode::Deserialize) {
                let serde_attrs = hook_serde_adapter_attrs(f, SerdeAdapterMode::Deserialize);
                quote! {
                    {
                        #[derive(::serde::Deserialize)]
                        struct __AutumnCommitHookDeserializeField {
                            #(#serde_attrs)*
                            value: #ty,
                        }
                        let mut __autumn_wrapper_object =
                            ::autumn_web::reexports::serde_json::Map::new();
                        __autumn_wrapper_object.insert(
                            ::std::string::String::from("value"),
                            __autumn_field,
                        );
                        let __autumn_wrapper: __AutumnCommitHookDeserializeField =
                            ::autumn_web::reexports::serde_json::from_value(
                                ::autumn_web::reexports::serde_json::Value::Object(
                                    __autumn_wrapper_object,
                                ),
                            )
                            .map_err(|__error| {
                                ::autumn_web::AutumnError::internal_server_error_msg(format!(
                                    "deserialize repository commit hook record field {}.{}: {}",
                                    stringify!(#name),
                                    #field_name,
                                    __error
                                ))
                            })?;
                        __autumn_wrapper.value
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::serde_json::from_value(__autumn_field)
                        .map_err(|__error| {
                            ::autumn_web::AutumnError::internal_server_error_msg(format!(
                                "deserialize repository commit hook record field {}.{}: {}",
                                stringify!(#name),
                                #field_name,
                                __error
                            ))
                        })?
                }
            };
            // #1654: the local binding feeds `Self { #ident: #ident }`, so it has
            // to be the *model's* field type -- the taint wrapper for a
            // classified column, which deserializes transparently. A
            // `#[serde(default = "path")]` fallback returns the *declared* type,
            // so it is classified on the way into the binding.
            let ty = model_field_ty(f);
            let missing_default = if classified_marker_for(f).is_some() {
                missing_default
                    .map(|d| quote! { ::autumn_web::classify::Classified::new(#d) })
            } else {
                missing_default
            };
            missing_default.map_or_else(
                || {
                    quote! {
                    let #ident: #ty = {
                        let __autumn_field = __autumn_object.remove(#field_name)
                            .ok_or_else(|| {
                                ::autumn_web::AutumnError::internal_server_error_msg(format!(
                                    "deserialize repository commit hook record field {}.{}: missing field",
                                    stringify!(#name),
                                    #field_name
                                ))
                            })?;
                        #field_value
                    };
                }
                },
                |missing_default| {
                    quote! {
                    let #ident: #ty = match __autumn_object.remove(#field_name) {
                        ::core::option::Option::Some(__autumn_field) => {
                            #field_value
                        }
                        ::core::option::Option::None => {
                            #missing_default
                        }
                    };
                }
                },
            )
        })
        .collect();
    let commit_hook_construct_fields: Vec<TokenStream> = all_fields
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().expect("named field");
            quote! { #ident: #ident }
        })
        .collect();
    // #1654: the durable commit-hook and ledger payload is a second copy of the record,
    // assembled by serializing every column into JSON. A classified column has no
    // `Serialize` impl — that is the guarantee — so the payload cannot be built, and
    // building it through a back door would put the value in a `serde_json::Value` that
    // anything could hand to a sink. Gating the durable-payload sink is a follow-up
    // slice; until then the combination fails loudly where it is used, naming the
    // column and the fix. Both callers (`commit_hooks = true`, `ledgered = true`) are
    // opt-in, so an ordinary repository over a classified model is unaffected.
    let commit_hook_serialize_body = if has_classified {
        let cols = classified_column_names.join("`, `");
        let message = format!(
            "`{}` has `#[classified]` column(s) `{cols}`, so it cannot be written into the \
             durable commit-hook payload: that payload is a second, unclassified copy of the \
             record. Gating it is a follow-up slice of issue #1654 -- until then, use \
             non-durable `after_commit` hooks on this model, or drop `#[classified]` from the \
             column. (`versioned = true` and `ledgered = true` take the same snapshot and are \
             rejected earlier, at compile time.)",
            unraw_ident(name),
        );
        quote! {
            let _ = &self;
            return ::core::result::Result::Err(
                ::autumn_web::AutumnError::internal_server_error_msg(#message),
            );
        }
    } else {
        quote! {
            let mut __autumn_object = ::autumn_web::reexports::serde_json::Map::new();
            #(#commit_hook_serialize_fields)*
            let mut __autumn_value =
                ::autumn_web::reexports::serde_json::Value::Object(__autumn_object);
            // Encrypted columns must not be persisted in plaintext into the
            // durable `autumn_repository_commit_hooks` table (#805). Rewrite
            // them as recoverable ciphertext in their declared mode.
            #commit_hook_encrypt_stmt
            ::core::result::Result::Ok(__autumn_value)
        }
    };

    let commit_hook_serialize_bounds: Vec<TokenStream> = all_fields
        .iter()
        .filter(|f| !has_hook_serde_adapter(f, SerdeAdapterMode::Serialize))
        .map(|f| {
            let ty = &f.ty;
            quote! { #ty: ::serde::Serialize }
        })
        .collect();
    let mut commit_hook_deserialize_bounds: Vec<TokenStream> = all_fields
        .iter()
        .filter(|f| !has_hook_serde_adapter(f, SerdeAdapterMode::Deserialize))
        .map(|f| {
            let ty = &f.ty;
            quote! { #ty: ::serde::de::DeserializeOwned }
        })
        .collect();
    commit_hook_deserialize_bounds.extend(
        all_fields
            .iter()
            .filter(|f| !is_option_type(&f.ty))
            .filter(|f| matches!(serde_default_kind(f), Some(SerdeDefaultKind::Default)))
            .map(|f| {
                let ty = &f.ty;
                quote! { #ty: ::core::default::Default }
            }),
    );
    let commit_hook_serialize_where = if commit_hook_serialize_bounds.is_empty() {
        quote! {}
    } else {
        quote! { where #(#commit_hook_serialize_bounds,)* }
    };
    let commit_hook_deserialize_where = if commit_hook_deserialize_bounds.is_empty() {
        quote! {}
    } else {
        quote! { where #(#commit_hook_deserialize_bounds,)* }
    };

    // `impl FormModel for #name` (issue #1135) -- one descriptor per editable
    // column, driving the single-call `form_for::<#name>(...)` builder. The
    // struct-level `rename_all` serialization rule (attrs pass through to the
    // emitted query struct's `Serialize` derive) feeds the descriptors'
    // pre-fill lookup keys for serde-renamed columns.
    let form_model_impl = emit_form_model_impl(
        name,
        &fields_for_new,
        serde_rename_all_serialize_rule(outer_attrs).as_deref(),
    );

    // #1379: normalization codegen.
    //
    // `impl Normalize` canonicalizes each `#[normalize]` column in place; it is
    // generated for the `New*` insert struct (write path) and the model itself.
    // `impl NormalizedModel` powers derived-finder argument normalization and is
    // generated for *every* model (empty match when no columns normalize) so the
    // generic finder call compiles uniformly. Draft (update) normalization is
    // woven into `from_patch` via `normalize_draft_stmts`.
    let names_for_new: std::collections::HashSet<String> = fields_for_new
        .iter()
        .filter_map(|f| f.ident.as_ref().map(std::string::ToString::to_string))
        .collect();
    let normalize_new_stmts: Vec<TokenStream> = normalized_columns
        .iter()
        .filter(|(ident, _, _)| names_for_new.contains(&ident.to_string()))
        .map(|(ident, _, ops)| {
            let expr = emit_normalize_expr(ops, &quote! { ::core::mem::take(&mut self.#ident) });
            quote! { self.#ident = #expr; }
        })
        .collect();
    let normalize_model_stmts: Vec<TokenStream> = normalized_columns
        .iter()
        .map(|(ident, _, ops)| {
            let expr = emit_normalize_expr(ops, &quote! { ::core::mem::take(&mut self.#ident) });
            quote! { self.#ident = #expr; }
        })
        .collect();
    let normalize_draft_stmts: Vec<TokenStream> = normalized_columns
        .iter()
        .map(|(ident, _, ops)| {
            let expr = emit_normalize_expr(ops, &quote! { ::core::mem::take(&mut after.#ident) });
            quote! { after.#ident = #expr; }
        })
        .collect();
    let normalize_lookup_arms: Vec<TokenStream> = normalized_columns
        .iter()
        .map(|(_, lookup_key, ops)| {
            let expr = emit_normalize_expr(ops, &quote! { value.to_owned() });
            quote! { #lookup_key => ::core::option::Option::Some(#expr), }
        })
        .collect();
    let normalize_impls = quote! {
        impl ::autumn_web::normalize::Normalize for #new_name {
            fn normalize(&mut self) {
                #(#normalize_new_stmts)*
            }
        }
        impl ::autumn_web::normalize::Normalize for #name {
            fn normalize(&mut self) {
                #(#normalize_model_stmts)*
            }
        }
        impl ::autumn_web::normalize::NormalizedModel for #name {
            fn normalize_lookup(
                column: &str,
                value: &str,
            ) -> ::core::option::Option<::std::string::String> {
                match column {
                    #(#normalize_lookup_arms)*
                    _ => ::core::option::Option::None,
                }
            }
        }
    };

    // ── #1126: allowlisted sort/filter helpers ──────────────────────────
    //
    // `#[repository]` is applied to a *trait* and cannot see the model's
    // columns, so the typed, injection-safe ordering/filtering DSL lives here
    // (the `#[model]` macro knows every field + type) and is called from the
    // generated `list()` method. The allowlist is the set of the model's own
    // scalar columns; an unknown `sort`/`filter` key hits the default match arm
    // and is silently ignored — it can never be interpolated into SQL.
    let list_boxed_ty = quote! {
        ::autumn_web::reexports::diesel::helper_types::IntoBoxed<
            '__lq,
            #table_ident::table,
            ::autumn_web::RuntimeBackend,
        >
    };
    // Single primary key used for the default order + tie-break. Absent for
    // composite/id-less models, in which case ordering is best-effort.
    let list_pk: Option<&syn::Ident> = if id_field_names.len() == 1 {
        Some(id_field_names[0])
    } else {
        None
    };
    let mut list_sort_arms: Vec<TokenStream> = Vec::new();
    let mut list_filter_arms: Vec<TokenStream> = Vec::new();
    for field in &all_fields {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        // #1654: `?filter[col]=` and `?sort=col` are client-controlled, so a
        // classified column in either allowlist is a leak that compiles: the
        // filter is an unauthenticated equality oracle over the personal data,
        // and the sort orders the whole result set by it. Neither goes through a
        // declassification boundary, so neither can appear in the manifest.
        // Look it up by name explicitly rather than by trusting the DB column,
        // then leave it out of both allowlists.
        if classified_columns.iter().any(|(cid, ..)| *cid == ident) {
            continue;
        }
        let raw = ident.to_string();
        let col = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
        let is_option = option_inner_type(&field.ty).is_some();
        let base_ty = option_inner_type(&field.ty).unwrap_or(&field.ty);
        let Some(last) = ty_last_ident(base_ty) else {
            continue;
        };
        // Sortable: any scalar column that maps to an orderable SQL type
        // (nullable columns included — ordering nulls is well-defined).
        let orderable = matches!(
            last.as_str(),
            "String"
                | "i16"
                | "i32"
                | "i64"
                | "bool"
                | "f32"
                | "f64"
                | "Decimal"
                | "Uuid"
                // `SqliteUuid` (issue #1924) is hyphenated lowercase text, which
                // sorts in UUID byte order. `SqliteDecimal` is deliberately
                // absent: its column is TEXT, so SQL would order it
                // lexicographically ("9" after "10") and the header would lie.
                | "SqliteUuid"
                | "NaiveDateTime"
                | "NaiveDate"
                | "NaiveTime"
                | "DateTime"
        );
        if orderable {
            let is_pk = list_pk.is_some_and(|pk| pk == ident);
            let tie_break = match (is_pk, list_pk) {
                // No redundant `ORDER BY id, id`, and no tie-break when the
                // model has no single primary key.
                (true, _) | (_, None) => quote! {},
                (false, Some(pk)) => quote! { .then_order_by(#table_ident::#pk.desc()) },
            };
            list_sort_arms.push(quote! {
                ::core::option::Option::Some(#col) => match __dir {
                    ::autumn_web::pagination::SortDir::Asc =>
                        __q.order(#table_ident::#ident.asc()) #tie_break,
                    ::autumn_web::pagination::SortDir::Desc =>
                        __q.order(#table_ident::#ident.desc()) #tie_break,
                },
            });
        }
        // Filterable (equality only): non-null String / integer / bool. Other
        // types are excluded — see the `list()` doc comment.
        if !is_option {
            match last.as_str() {
                "String" => list_filter_arms.push(quote! {
                    #col => { __q = __q.filter(#table_ident::#ident.eq(__val.to_owned())); }
                }),
                "i16" | "i32" | "i64" | "bool" => {
                    let parse_ty = format_ident!("{last}");
                    list_filter_arms.push(quote! {
                        #col => {
                            if let ::core::result::Result::Ok(__v) = __val.parse::<#parse_ty>() {
                                __q = __q.filter(#table_ident::#ident.eq(__v));
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    }
    let list_default_order = list_pk.map_or_else(
        || quote! { __q },
        |pk| quote! { __q.order(#table_ident::#pk.desc()) },
    );
    let list_filter_body = if list_filter_arms.is_empty() {
        quote! { let _ = &__query; __q }
    } else {
        quote! {
            use ::autumn_web::reexports::diesel::prelude::*;
            for (__col, __val) in __query.filters() {
                match __col {
                    #(#list_filter_arms)*
                    _ => {}
                }
            }
            __q
        }
    };
    let list_order_body = if list_sort_arms.is_empty() {
        quote! {
            use ::autumn_web::reexports::diesel::prelude::*;
            let _ = __query;
            #list_default_order
        }
    } else {
        quote! {
            use ::autumn_web::reexports::diesel::prelude::*;
            let __dir = __query.direction();
            match __query.sort() {
                #(#list_sort_arms)*
                _ => #list_default_order,
            }
        }
    };
    let list_query_helpers = quote! {
        impl #name {
            /// Apply the allowlisted equality filters carried by a
            /// [`::autumn_web::pagination::ListQuery`] to a boxed query.
            ///
            /// Generated by `#[model]` for the repository `list()` method; not
            /// part of the public API. Only the model's own non-null
            /// `String`/integer/`bool` columns are filterable — any other
            /// requested `filter[..]` key is ignored.
            #[doc(hidden)]
            #[allow(unused_mut, clippy::allow_attributes)]
            pub fn __autumn_list_apply_filters<'__lq>(
                mut __q: #list_boxed_ty,
                __query: &::autumn_web::pagination::ListQuery,
            ) -> #list_boxed_ty {
                #list_filter_body
            }

            /// Apply the allowlisted ordering carried by a
            /// [`::autumn_web::pagination::ListQuery`] to a boxed query,
            /// defaulting to primary-key-descending when the requested `sort`
            /// key is empty or names a non-orderable column.
            ///
            /// Generated by `#[model]`; not part of the public API.
            #[doc(hidden)]
            #[allow(clippy::allow_attributes)]
            pub fn __autumn_list_apply_order<'__lq>(
                __q: #list_boxed_ty,
                __query: &::autumn_web::pagination::ListQuery,
            ) -> #list_boxed_ty {
                #list_order_body
            }
        }
    };

    quote! {
        #encrypted_use

        #(#classified_items)*

        #list_query_helpers

        #[derive(#name_debug_derive Clone, ::diesel::Queryable, ::diesel::Selectable, ::diesel::AsChangeset, ::diesel::Insertable)]
        #query_serde_derive
        // #1778: derive `validator::Validate` on the read model (gated on
        // `has_validation`, symmetric with New*/Update*) so the merged model
        // built by `from_patch` can be validated on the update path. See the
        // `query_fields` comment for why concrete `T` fields dodge the E0119
        // walls that block the same validators on `Patch<T>`.
        #validate_derive
        #[diesel(table_name = #table_ident)]
        #(#filtered_outer_attrs)*
        #vis struct #name {
            #(#query_fields,)*
        }
        #name_debug_impl

        #form_model_impl

        #normalize_impls

        #[derive(#new_debug_derive Clone, ::diesel::Insertable)]
        #[derive(::serde::Serialize, ::serde::Deserialize)]
        #validate_derive
        #[diesel(table_name = #table_ident)]
        #vis struct #new_name {
            #(#new_fields,)*
        }
        #new_debug_impl

        #[derive(#update_debug_derive Clone, Default)]
        #[derive(::serde::Serialize, ::serde::Deserialize)]
        #validate_derive
        #vis struct #update_name {
            #(#update_fields,)*
        }
        #update_debug_impl

        /// Diesel-compatible changeset derived from `Patch<T>` fields.
        ///
        /// This type bridges the `Patch`-based `UpdateX` and Diesel's
        /// `AsChangeset` trait. Use `UpdateX::__to_changeset()` to convert.
        #[doc(hidden)]
        #[derive(#changeset_debug_derive Clone, ::diesel::AsChangeset)]
        #[diesel(table_name = #table_ident)]
        pub struct #changeset_name {
            #(#changeset_fields,)*
        }
        #changeset_debug_impl

        impl #name {
            /// Column names on this model that are at-rest encrypted.
            ///
            /// Emitted for every model (empty when none are encrypted) so that
            /// version history, log scrubbing, and the admin plugin can redact
            /// encrypted columns by default. See `autumn_web::encryption`.
            #[doc(hidden)]
            pub const __AUTUMN_ENCRYPTED_COLUMNS: &'static [&'static str] =
                &[#(#encrypted_column_names),*];

            /// Column names on this model marked `#[classified]` (#1654).
            ///
            /// Emitted for every model (empty when none are classified). The
            /// compile-time guarantee is carried by the columns' types, not by
            /// this list -- it exists so a surface without a compile-time view
            /// of the model (the admin plugin, an operator report) can ask which
            /// columns hold personal data, and so the `Json` sink diagnostic can
            /// point somewhere when it can only name the model.
            #[doc(hidden)]
            pub const __AUTUMN_CLASSIFIED_COLUMNS: &'static [&'static str] =
                &[#(#classified_column_names),*];

            /// Column names on this model declared `#[translatable]` (#1384).
            ///
            /// Emitted for every model (empty when none are translatable) so
            /// that surfaces without a compile-time view of the model can ask
            /// which columns hold a per-locale container.
            #[doc(hidden)]
            pub const __AUTUMN_TRANSLATABLE_COLUMNS: &'static [&'static str] =
                &[#(#translatable_column_names),*];
        }

        #(#encrypted_inventory)*

        #translatable_items
        #(#translatable_inventory)*

        impl #update_name {
            #[doc(hidden)]
            #[must_use]
            pub fn __to_changeset(&self) -> #changeset_name {
                #changeset_name {
                    #(#changeset_conversions,)*
                }
            }
        }

        impl #name {
            pub const __AUTUMN_COLUMN_COUNT: usize = #column_count;

            #[doc(hidden)]
            pub fn __autumn_column_count(&self) -> usize {
                Self::__AUTUMN_COLUMN_COUNT
            }

            #[doc(hidden)]
            pub fn __autumn_upsert_set() -> impl ::autumn_web::reexports::diesel::query_builder::AsChangeset<
                Target = #table_ident::table,
                Changeset = impl ::autumn_web::reexports::diesel::query_builder::QueryFragment<::autumn_web::RuntimeBackend> + ::core::marker::Send + ::core::marker::Sync + 'static
            > + ::core::marker::Send + ::core::marker::Sync + 'static {
                use ::autumn_web::reexports::diesel::ExpressionMethods as _;
                (#(#upsert_columns,)*)
            }

            #[doc(hidden)]
            pub async fn __autumn_execute_upsert(
                chunk: &[Self],
                tenant_id: ::core::option::Option<&str>,
                conn: &mut ::autumn_web::RuntimeConnection,
            ) -> ::core::result::Result<::std::vec::Vec<Self>, ::autumn_web::reexports::diesel::result::Error> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;

                // Postgres builds one batched `INSERT … ON CONFLICT … DO UPDATE …
                // RETURNING` over the whole chunk. SQLite cannot express a multi-row
                // `VALUES` with the `DEFAULT` keyword — `BatchInsert<…, false>` has no
                // `QueryFragment<Sqlite>` — so it upserts row by row, each `INSERT …
                // ON CONFLICT(id) DO UPDATE … WHERE … RETURNING`, valid single-row on
                // SQLite with the `returning_clauses` flag. The caller
                // (`save_many`/`upsert_many`) already runs this inside a transaction,
                // so the per-row loop stays atomic. The per-row body
                // (`#execute_upsert_body_sqlite`) applies the same tenant and
                // lock-version `DO UPDATE … WHERE` refinements as the Postgres arm —
                // `diesel::upsert::excluded` is the backend-agnostic form — so
                // cross-tenant ids are left untouched, with no cross-tenant row
                // movement, and stale lock-version rows drop out of RETURNING, matching
                // Postgres exactly (#1996, PR #2021).
                ::autumn_web::backend_select! {
                    pg => {
                        let stmt = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            // Owned `Vec` (not `&[Self]`): encrypted columns use diesel
                            // `serialize_as`, which only implements `Insertable` for owned
                            // values. `to_vec()` also works for plain models.
                            .values(chunk.to_vec())
                            .on_conflict(#table_ident::id)
                            .do_update()
                            .set(Self::__autumn_upsert_set());

                        #execute_upsert_body
                    },
                    sqlite => {
                        let mut __autumn_upserted = ::std::vec::Vec::new();
                        for __autumn_row in chunk.to_vec() {
                            #execute_upsert_body_sqlite
                        }
                        ::core::result::Result::Ok(__autumn_upserted)
                    },
                }
            }



            #[doc(hidden)]
            pub fn __autumn_correlate_new(
                inputs: &[#new_name],
                record: &Self,
                matched: &mut [bool],
            ) -> ::core::option::Option<usize> {
                for (i, input) in inputs.iter().enumerate() {
                    if !matched[i] {
                        if #compare_expr {
                            return ::core::option::Option::Some(i);
                        }
                    }
                }
                ::core::option::Option::None
            }

            #[doc(hidden)]
            pub fn __autumn_correlate_model(
                inputs: &[Self],
                record: &Self,
                matched: &mut [bool],
                ) -> ::core::option::Option<usize> {
                for (i, input) in inputs.iter().enumerate() {
                    if !matched[i] {
                        if #compare_expr {
                            return ::core::option::Option::Some(i);
                        }
                    }
                }
                ::core::option::Option::None
            }
        }

        impl ::autumn_web::repository::AutumnUpsertSetExt for #name {
            type UpsertSet = ::autumn_web::reexports::diesel::dsl::Eq<
                #table_ident::id,
                #table_ident::id,
            >;
            fn __autumn_upsert_set() -> Self::UpsertSet {
                use ::autumn_web::reexports::diesel::ExpressionMethods as _;
                #table_ident::id.eq(#table_ident::id)
            }
        }

        impl ::autumn_web::repository::AutumnUpsertExecutionExt for #name {
            type Model = Self;
            async fn __autumn_execute_upsert(
                chunk: &[Self::Model],
                tenant_id: ::core::option::Option<&str>,
                conn: &mut ::autumn_web::RuntimeConnection,
            ) -> ::core::result::Result<::std::vec::Vec<Self::Model>, ::autumn_web::reexports::diesel::result::Error> {
                Self::__autumn_execute_upsert(chunk, tenant_id, conn).await
            }
        }

        impl ::autumn_web::repository::AutumnCorrelateExt for #name {
            type NewModel = #new_name;
            fn __autumn_correlate_new(
                inputs: &[Self::NewModel],
                record: &Self,
                matched: &mut [bool],
            ) -> ::core::option::Option<usize> {
                Self::__autumn_correlate_new(inputs, record, matched)
            }

            fn __autumn_correlate_model(
                inputs: &[Self],
                record: &Self,
                matched: &mut [bool],
            ) -> ::core::option::Option<usize> {
                Self::__autumn_correlate_model(inputs, record, matched)
            }
        }


        impl #new_name {
            pub const __AUTUMN_COLUMN_COUNT: usize = #new_column_count;

            #[doc(hidden)]
            pub fn __autumn_column_count(&self) -> usize {
                Self::__AUTUMN_COLUMN_COUNT
            }
        }

        #can_set_tenant_id_impl
        #model_tenant_id_meta_impl
        #model_primary_key_impl

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #vis enum #field_enum_name {
            #(#field_enum_variants,)*
        }

        /// Extension trait providing `from_patch` and per-field `DraftField` accessors
        /// for `UpdateDraft<#name>`.
        ///
        /// Generated by `#[model]`. Import this trait to call `from_patch()` or
        /// field accessor methods on `UpdateDraft<#name>`.
        #vis trait #draft_ext_name {
            /// Build a draft by merging the current record with a patch.
            ///
            /// Returns `Err` if a non-nullable field has `Patch::Clear`.
            fn from_patch(current: &#name, patch: &#update_name) -> ::autumn_web::AutumnResult<::autumn_web::hooks::UpdateDraft<#name>>;

            #(#draft_accessor_sigs)*
        }

        impl #draft_ext_name for ::autumn_web::hooks::UpdateDraft<#name> {
            fn from_patch(current: &#name, patch: &#update_name) -> ::autumn_web::AutumnResult<Self> {
                let mut after = current.clone();
                #(#merge_arms)*
                // #1379: normalize `#[normalize]` columns on the update path
                // (before validation / persistence), so the DB observes the
                // canonical value. No-op when no columns normalize.
                #(#normalize_draft_stmts)*
                // #1778: validate the effective merged model (existing row ∪
                // patch, post-normalization) so the full `#[validate(...)]` set
                // runs against concrete values. No-op when the model declares no
                // validation.
                #merged_validate_stmt
                Ok(Self::new_with_changes(current.clone(), after))
            }

            #(#draft_accessors)*
        }

        /// Factory builder for [`#name`].
        ///
        /// Produced by [`#name::factory()`]. All fields are pre-filled with
        /// `Default::default()` so callers only need to specify the fields that
        /// matter for their scenario.
        #[derive(#factory_debug_derive Clone)]
        #vis struct #factory_name {
            #(#factory_struct_fields,)*
            /// Names of fields the caller explicitly set via a setter. `.fake()`
            /// skips these so explicit overrides always win. (#1343)
            #[doc(hidden)]
            pub __autumn_set: ::std::collections::HashSet<&'static str>,
            /// Whether `.fake()` was requested. When true, unset fields are
            /// filled with generated data at `build()`/`create()` time. (#1343)
            #[doc(hidden)]
            pub __autumn_fake: bool,
        }

        #factory_debug_impl

        impl ::core::default::Default for #factory_name {
            fn default() -> Self {
                Self {
                    #(#factory_default_fields,)*
                    __autumn_set: ::std::collections::HashSet::new(),
                    __autumn_fake: false,
                }
            }
        }

        impl #factory_name {
            #(#factory_setters)*

            /// Fill every field that was not explicitly set with realistic
            /// fake data when building. The value is inferred from each field's
            /// name and type (see `autumn_web::fake`).
            ///
            /// This only flips a flag — values are drawn at `build()`/`create()`
            /// time, so each build produces fresh, varied rows. Set
            /// `AUTUMN_FAKE_SEED` (or call `autumn_web::fake::reseed`) for
            /// reproducible output.
            #[must_use]
            pub fn fake(mut self) -> Self {
                self.__autumn_fake = true;
                self
            }

            /// Alias for [`fake`](Self::fake): fill every field that was not
            /// explicitly set with realistic fake data when building. Provided
            /// so both spellings from the `.fake()`/`.fake_all()` API read
            /// naturally (#1343 AC2).
            #[must_use]
            pub fn fake_all(self) -> Self {
                self.fake()
            }

            /// Build a [`#new_name`] instance from the current factory state.
            ///
            /// Does not touch the database. Use [`#factory_name::create`] to
            /// also persist the record.
            #[must_use]
            pub fn build(self) -> #new_name {
                #(#factory_value_bindings)*
                #(#build_assoc_bindings)*
                #new_name {
                    #(#new_construct_fields,)*
                }
            }

            /// Build `count` [`#new_name`] instances. With `.fake()` each row is
            /// re-drawn, so text fields vary across the batch. Without `.fake()`
            /// the rows are identical copies of the factory state.
            #[must_use]
            pub fn build_many(self, count: usize) -> ::std::vec::Vec<#new_name> {
                let mut out = ::std::vec::Vec::with_capacity(count);
                for _ in 0..count {
                    out.push(::core::clone::Clone::clone(&self).build());
                }
                out
            }

            #factory_create_method

            #factory_create_many_method
        }

        impl #name {
            /// Create a factory builder for constructing [`#name`] instances.
            ///
            /// Returns a [`#factory_name`] with all fields at their [`Default`]
            /// value. Override any subset with the fluent setter methods, then call
            /// `build()` for an in-memory instance or `create(pool)` to persist it.
            #[must_use]
            pub fn factory() -> #factory_name {
                #factory_name::default()
            }

            /// Returns the primary-key value of this model.
            ///
            /// Used by generated `#[factory_assoc]` code to extract the PK from a
            /// pre-built associated instance without hardcoding the field name.
            #[doc(hidden)]
            #[inline]
            pub fn __autumn_pk(&self) -> #pk_ty {
                self.#pk_id.clone()
            }
        }

        // ── #1343 AC4: fake-seeder registration ─────────────────────────
        // Register this model's factory so `autumn seed --count N --model M`
        // (and any seed binary) can generate faked rows by name, without the
        // user editing `src/bin/seed.rs`. The forwarding macro expands to an
        // `inventory::submit!` only when autumn-web is built with the `seed`
        // feature (which implies `db`, and hence `create_many`); otherwise it
        // expands to nothing, so models compile unchanged when seeding is off.
        ::autumn_web::__autumn_register_fake_seeder!(#name, stringify!(#name));

        // ── Architecture-graph node (#1747) ────────────────────────────
        // The model's own declaration of the table it maps to: the join key
        // every route, job and repository edge resolves against.
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::graph::ModelGraphDescriptor {
                model: ::core::stringify!(#name),
                model_path: ::core::concat!(
                    ::core::module_path!(),
                    "::",
                    ::core::stringify!(#name)
                ),
                table: #table_name,
                relations: #graph_relations,
                module_path: ::core::module_path!(),
                file: ::core::file!(),
                line: ::core::line!(),
            }
        }

        // ── Durable commit-hook codec ───────────────────────────────────
        // Hidden durable commit-hook codec. These methods serialize fields
        // individually so public serde visibility attributes do not drop
        // payload data needed by after_*_commit runners.
        impl #name {
            #[doc(hidden)]
            pub fn __autumn_commit_hook_to_value(
                &self,
            ) -> ::autumn_web::AutumnResult<::autumn_web::reexports::serde_json::Value>
            #commit_hook_serialize_where
            {
                #commit_hook_serialize_body
            }

            #[doc(hidden)]
            pub fn __autumn_commit_hook_from_value(
                __autumn_value: ::autumn_web::reexports::serde_json::Value,
            ) -> ::autumn_web::AutumnResult<Self>
            #commit_hook_deserialize_where
            {
                // Encrypted columns are persisted as ciphertext (see
                // `__autumn_commit_hook_to_value`); recover plaintext before the
                // model is reconstructed so replayed hooks see real values.
                let mut __autumn_decoded_value = __autumn_value;
                #commit_hook_decrypt_stmt
                let mut __autumn_object = match __autumn_decoded_value {
                    ::autumn_web::reexports::serde_json::Value::Object(__autumn_object) => __autumn_object,
                    __autumn_other => {
                        return Err(::autumn_web::AutumnError::internal_server_error_msg(format!(
                            "deserialize repository commit hook record for {}: expected object, got {}",
                            stringify!(#name),
                            __autumn_other
                        )));
                    }
                };
                #(#commit_hook_deserialize_fields)*
                Ok(Self {
                    #(#commit_hook_construct_fields,)*
                })
            }
        }

        // ── Optimistic-lock helpers ─────────────────────────────────────
        // Always emitted so the generated repository code can call them
        // unconditionally regardless of whether the model has a
        // `#[lock_version]` field. The `None` paths compile away with zero
        // overhead for models that don't use optimistic locking.
        impl #name {
            /// Returns the current stored lock version, or `None` if this model
            /// does not have a `#[lock_version]` field.
            #[doc(hidden)]
            #[inline]
            pub fn __autumn_lock_version_actual(&self) -> ::core::option::Option<i64> {
                #lock_version_actual_body
            }

            #etag_method
        }

        impl #update_name {
            /// Returns the client-supplied expected lock version, or `None`
            /// if this model does not have a `#[lock_version]` field.
            ///
            /// The repository compares this against the stored version and
            /// returns `RepositoryError::Conflict` on a mismatch.
            #[doc(hidden)]
            #[inline]
            pub fn __autumn_lock_version_expected(&self) -> ::core::option::Option<i64> {
                #lock_version_expected_body
            }
        }

        // ── OpenAPI schema impls ────────────────────────────────────────
        // Always emitted (OpenApiSchema is not feature-gated) so external
        // crates can register rich schemas without the openapi feature.

        impl ::autumn_web::openapi::OpenApiSchema for #name {
            fn schema_name() -> &'static str { stringify!(#name) }
            fn schema() -> ::serde_json::Value {
                #query_struct_schema_body
            }
        }

        impl ::autumn_web::openapi::OpenApiSchema for #new_name {
            fn schema_name() -> &'static str { stringify!(#new_name) }
            fn schema() -> ::serde_json::Value {
                #new_struct_schema_body
            }
        }

        impl ::autumn_web::openapi::OpenApiSchema for #update_name {
            fn schema_name() -> &'static str { stringify!(#update_name) }
            fn schema() -> ::serde_json::Value {
                #update_struct_schema_body
            }
        }

        impl ::autumn_web::repository::AutumnSearchableModel for #name {
            const IS_SEARCHABLE: bool = #is_searchable;
            const SEARCH_LANGUAGE: &'static str = #search_language;
            const SEARCH_FIELDS: &'static [(&'static str, char)] = &[
                #((#search_field_names, #search_field_weights)),*
            ];
        }

        // ── Pluggable search index definition + document (#1191) ────────────
        #search_indexed_impl

        // ── State machine impls (one per #[state_machine] field) ────────────
        #(#state_machine_impls)*

        // ── Associations + eager loading (belongs_to / has_many / has_one) ──
        #preload_retain_impl
        #association_items

        // ── Model-declared dependent cascade specs (#1738) ──────────────────
        #dependents_impl

        // ── Model-declared counter-cache specs (#1325) ──────────────────────
        #counter_caches_impl

        // ── Votable reactions (#[votable], #1362) ───────────────────────────
        #votable_items

        // ── Polymorphic comments (#[commentable], #1367) ────────────────────
        #commentable_items
    }
}

pub fn infer_table_name(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    let snake = pascal_to_snake(&name);
    // Pluralize only the last snake_case segment, mirroring
    // `autumn-cli`'s `naming::pluralize`: `blog_post` → `blog_posts`,
    // `category` → `categories`.
    let (prefix, last) = snake.rfind('_').map_or(("", snake.as_str()), |idx| {
        (&snake[..=idx], &snake[idx + 1..])
    });
    format!("{prefix}{}", pluralize_word(last))
}

/// English pluraliser for a single word: irregulars, sibilant endings
/// (`+es`), consonant+`y` (`y` → `ies`), otherwise `+s`.
///
/// This is a FAITHFUL copy of [`autumn_web::format::pluralize_word`], which is
/// the canonical implementation (see `autumn/src/format.rs::pluralize_word`).
/// It MUST stay in sync with that function: the CLI scaffold's `src/schema.rs`
/// pluralises table names through `autumn_web::format::pluralize_word` (via
/// `naming::pluralize`), and the `#[model]`/`#[repository]` derives here must
/// produce the same table name so the generated app compiles. It is duplicated
/// rather than imported because this proc-macro crate cannot depend on
/// `autumn-web` (that would create a dependency cycle: `autumn-web` depends on
/// `autumn-macros`).
fn pluralize_word(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    match word {
        "person" => return "people".to_owned(),
        "child" => return "children".to_owned(),
        "man" => return "men".to_owned(),
        "woman" => return "women".to_owned(),
        "mouse" => return "mice".to_owned(),
        "goose" => return "geese".to_owned(),
        _ => {}
    }
    let lower = word.to_ascii_lowercase();
    if lower.ends_with("ss")
        || lower.ends_with('x')
        || lower.ends_with('z')
        || lower.ends_with("ch")
        || lower.ends_with("sh")
    {
        return format!("{word}es");
    }
    if lower.ends_with('y') {
        // 'y' is 1-byte ASCII, so slicing off the last byte stays on a char boundary.
        let prefix = &word[..word.len() - 1];
        if let Some(prev) = prefix.chars().next_back()
            && !"aeiouAEIOU".contains(prev)
        {
            return format!("{prefix}ies");
        }
    }
    format!("{word}s")
}

pub fn pascal_to_snake(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Fake integer inference width safety (#1343 FIX4) ──────────────────

    #[test]
    fn fake_int_ranges_never_truncate_the_cast() {
        // Narrow types must clamp the range to their own maximum so the `as`
        // cast can't wrap (`1000 as u8 == 232`, `1000 as i8 == -24`).
        let expr = |ty: syn::Type| {
            fake_expr_core("count", &ty)
                .expect("integer type should infer a fake expr")
                .to_string()
        };

        let i8_expr = expr(syn::parse_quote!(i8));
        assert!(i8_expr.contains("as i8"), "{i8_expr}");
        assert!(
            i8_expr.contains("127i64"),
            "i8 must clamp to i8::MAX: {i8_expr}"
        );

        let u8_expr = expr(syn::parse_quote!(u8));
        assert!(u8_expr.contains("as u8"), "{u8_expr}");
        assert!(
            u8_expr.contains("255i64"),
            "u8 must clamp to u8::MAX: {u8_expr}"
        );

        // Wider types keep the default 1000 upper bound (all fit comfortably).
        for wide in ["i16", "i32", "i64", "u16", "u32", "u64", "usize", "isize"] {
            let ty: syn::Type = syn::parse_str(wide).unwrap();
            let e = expr(ty);
            assert!(e.contains(&format!("as {wide}")), "{e}");
            assert!(e.contains("1000"), "{wide} should keep 1000 default: {e}");
        }

        // 128-bit widths are matched (previously fell through to Default 0).
        let i128_expr = expr(syn::parse_quote!(i128));
        assert!(
            i128_expr.contains("as i128"),
            "i128 must be matched: {i128_expr}"
        );
        let u128_expr = expr(syn::parse_quote!(u128));
        assert!(
            u128_expr.contains("as u128"),
            "u128 must be matched: {u128_expr}"
        );
    }

    // ── RED: #[lock_version] detection ────────────────────────────────────
    // These tests cover the new `excluded_from_new` behaviour (must also
    // exclude `#[lock_version]` fields) and the helper that detects whether
    // a field carries the attribute.

    // ── Association parsing / convention inference ───────────────────────

    #[test]
    fn belongs_to_explicit_fk_derives_name_from_fk() {
        let post = syn::parse_quote!(Post);
        let user = syn::parse_quote!(User);
        let (fk, name) = resolve_fk_and_name(AssocKind::BelongsTo, &post, &user, Some("author_id"));
        assert_eq!(fk, "author_id");
        assert_eq!(name, "author");
    }

    #[test]
    fn belongs_to_infers_fk_and_name_from_target() {
        let post = syn::parse_quote!(Post);
        let subreddit = syn::parse_quote!(Subreddit);
        let (fk, name) = resolve_fk_and_name(AssocKind::BelongsTo, &post, &subreddit, None);
        assert_eq!(fk, "subreddit_id");
        assert_eq!(name, "subreddit");
    }

    #[test]
    fn has_many_infers_fk_from_source_and_pluralizes_name() {
        let post = syn::parse_quote!(Post);
        let comment = syn::parse_quote!(Comment);
        let (fk, name) = resolve_fk_and_name(AssocKind::HasMany, &post, &comment, None);
        assert_eq!(fk, "post_id");
        assert_eq!(name, "comments");
    }

    #[test]
    fn has_many_pluralizes_name_with_irregular_rules() {
        // #1753: irregular plurals must use the smart pluraliser, not `{}s`.
        let store = syn::parse_quote!(Store);
        let category = syn::parse_quote!(Category);
        let (fk, name) = resolve_fk_and_name(AssocKind::HasMany, &store, &category, None);
        assert_eq!(fk, "store_id");
        assert_eq!(name, "categories");
    }

    #[test]
    fn has_one_infers_fk_from_source_and_singular_name() {
        let user = syn::parse_quote!(User);
        let profile = syn::parse_quote!(Profile);
        let (fk, name) = resolve_fk_and_name(AssocKind::HasOne, &user, &profile, None);
        assert_eq!(fk, "user_id");
        assert_eq!(name, "profile");
    }

    #[test]
    fn resolve_associations_parses_all_kinds() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[belongs_to(User, fk = author_id)]),
            syn::parse_quote!(#[has_many(Comment)]),
            syn::parse_quote!(#[belongs_to(Subreddit)]),
        ];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 3);
        assert_eq!(assocs[0].kind, AssocKind::BelongsTo);
        assert_eq!(assocs[0].fk, "author_id");
        assert_eq!(assocs[0].name, "author");
        assert_eq!(assocs[1].kind, AssocKind::HasMany);
        assert_eq!(assocs[1].fk, "post_id");
        assert_eq!(assocs[1].name, "comments");
        assert_eq!(assocs[2].name, "subreddit");
    }

    #[test]
    fn resolve_associations_rejects_unknown_key() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(User, bogus = author_id)])];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    // ── `dependent` / `on_delete` on model associations (#1738) ──────────
    // The model-declared cascade is now wired: the parser RECOGNIZES the
    // `dependent = <action>` / `on_delete = <action>` spelling on
    // `#[has_many]` / `#[has_one]`, validates the action, and records it on the
    // association so `#[model]` can emit the runtime `AutumnDependents` dispatch.
    // An unknown action is still rejected; `#[belongs_to]` still errors.

    // ── `counter_cache` on `#[belongs_to]` (#1325) ────────────────────────
    //
    // The attribute parses as a bare flag and as an explicit column, resolves the
    // column by the `{snake(child)}_count` convention, and rejects the shapes that
    // would silently produce wrong SQL: a counter on the parent's `has_many`, where
    // nothing owns the foreign key; a counter over a `through =` join table, where the
    // foreign key is a join-table column; a non-identifier column name, since it is
    // spliced into `format!`ed SQL; and two legs resolving onto one parent column,
    // where both would move it.

    /// `resolve_associations`' `Ok` type is not `Debug`, so unwrap the error by
    /// matching rather than through `expect_err`.
    fn expect_assoc_error(model: &syn::Ident, attrs: &[syn::Attribute]) -> String {
        match resolve_associations(model, attrs) {
            Ok(_) => panic!("expected the association to be rejected"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn counter_cache_bare_flag_derives_the_column_from_the_child() {
        let model: syn::Ident = syn::parse_quote!(Comment);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(Post, counter_cache)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 1);
        assert!(assocs[0].counter_cache.is_some());
        assert_eq!(
            counter_cache_column(&model, &assocs[0]).as_deref(),
            Some("comment_count"),
            "the default column is singular: {{snake(child)}}_count"
        );
    }

    #[test]
    fn counter_cache_accepts_an_explicit_column() {
        let model: syn::Ident = syn::parse_quote!(Subscription);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(Subreddit, counter_cache = "subscriber_count")])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(
            counter_cache_column(&model, &assocs[0]).as_deref(),
            Some("subscriber_count")
        );
    }

    #[test]
    fn a_belongs_to_without_counter_cache_records_none() {
        let model: syn::Ident = syn::parse_quote!(Comment);
        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(#[belongs_to(Post)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert!(assocs[0].counter_cache.is_none());
        assert_eq!(counter_cache_column(&model, &assocs[0]), None);
    }

    #[test]
    fn counter_cache_tenant_is_explicit_and_recorded() {
        let model: syn::Ident = syn::parse_quote!(Comment);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[belongs_to(Post, counter_cache, counter_cache_tenant = "tenant_id")]),
        ];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(
            assocs[0]
                .counter_cache
                .as_ref()
                .and_then(|d| d.tenant_column.clone())
                .as_deref(),
            Some("tenant_id")
        );
    }

    #[test]
    fn counter_cache_defaults_to_no_tenant_predicate() {
        // Not inferred from the child's fields: the parent's schema is invisible
        // here, and guessing would break a tenant-scoped child hanging off a
        // global parent with a hard `column does not exist`.
        let model: syn::Ident = syn::parse_quote!(Comment);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(Post, counter_cache)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert!(
            assocs[0]
                .counter_cache
                .as_ref()
                .expect("counter cache")
                .tenant_column
                .is_none()
        );
    }

    #[test]
    fn counter_cache_tenant_without_counter_cache_is_rejected() {
        let model: syn::Ident = syn::parse_quote!(Comment);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(Post, counter_cache_tenant = "tenant_id")])];
        let message = expect_assoc_error(&model, &attrs);
        assert!(message.contains("requires `counter_cache`"), "{message}");
    }

    #[test]
    fn counter_cache_on_has_many_is_rejected() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Comment, counter_cache)])];
        let message = expect_assoc_error(&model, &attrs);
        assert!(
            message.contains("`#[belongs_to]` option")
                && message.contains("Move it to the child model's"),
            "the error must name the leg to move it to: {message}"
        );
    }

    #[test]
    fn counter_cache_on_a_join_table_association_is_rejected() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(Tag, through = post_tags, counter_cache)])];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    #[test]
    fn a_non_identifier_counter_cache_column_is_rejected() {
        let model: syn::Ident = syn::parse_quote!(Comment);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[belongs_to(Post, counter_cache = "comment_count; DROP TABLE posts")]),
        ];
        let message = expect_assoc_error(&model, &attrs);
        assert!(message.contains("plain identifier"), "{message}");
    }

    #[test]
    fn two_legs_onto_one_parent_column_collide() {
        // Both legs point at `User` and both default to `message_count`, so
        // every insert would count twice.
        let model: syn::Ident = syn::parse_quote!(Message);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[belongs_to(User, fk = sender_id, name = sender, counter_cache)]),
            syn::parse_quote!(#[belongs_to(User, fk = recipient_id, name = recipient, counter_cache)]),
        ];
        let message = expect_assoc_error(&model, &attrs);
        assert!(message.contains("users.message_count"), "{message}");
    }

    #[test]
    fn two_legs_onto_different_parent_tables_may_share_a_column_name() {
        // `users.message_count` and `rooms.message_count` are different columns
        // on different tables — not a collision.
        let model: syn::Ident = syn::parse_quote!(Message);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[belongs_to(User, fk = sender_id, name = sender, counter_cache)]),
            syn::parse_quote!(#[belongs_to(Room, fk = room_id, counter_cache)]),
        ];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 2);
    }

    #[test]
    fn has_many_dependent_destroy_is_recorded() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Comment, dependent = destroy)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 1);
        assert_eq!(assocs[0].kind, AssocKind::HasMany);
        assert_eq!(assocs[0].fk, "post_id");
        assert_eq!(assocs[0].dependent, Some(DependentAction::Destroy));
    }

    #[test]
    fn has_many_on_delete_destroy_is_recorded() {
        // The `on_delete = <action>` spelling is an accepted alias for
        // `dependent = <action>` and records the same action (#1738).
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Comment, on_delete = destroy)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 1);
        assert_eq!(assocs[0].dependent, Some(DependentAction::Destroy));
    }

    #[test]
    fn has_many_dependent_unknown_action_is_rejected() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Comment, dependent = bogus)])];
        let Err(err) = resolve_associations(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("bogus"),
            "expected the unknown action named, got: {msg}"
        );
        assert!(
            msg.contains("destroy")
                && msg.contains("delete_all")
                && msg.contains("nullify")
                && msg.contains("restrict"),
            "expected the valid actions listed, got: {msg}"
        );
    }

    #[test]
    fn has_one_dependent_nullify_is_recorded() {
        let model: syn::Ident = syn::parse_quote!(User);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_one(Profile, dependent = nullify)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 1);
        assert_eq!(assocs[0].kind, AssocKind::HasOne);
        assert_eq!(assocs[0].dependent, Some(DependentAction::Nullify));
    }

    #[test]
    fn model_emits_dependents_impl_for_declared_cascade() {
        // The `#[model]` expansion must generate the runtime `AutumnDependents`
        // dispatch (an inherent `dependents()` returning `RuntimeDependentSpec`s
        // resolving the child repo via the `Pg{Child}Repository` convention)
        // rather than erroring, once a `#[has_many(dependent = …)]` is declared.
        let item: TokenStream = quote! {
            #[has_many(Comment, dependent = destroy)]
            struct Post {
                #[id]
                id: i64,
                title: String,
            }
        };
        let generated = model_macro(quote! {}, item).to_string();
        assert!(
            generated.contains("fn dependents"),
            "expected an inherent dependents() fn, got: {generated}"
        );
        assert!(
            generated.contains("RuntimeDependentSpec"),
            "expected RuntimeDependentSpec entries, got: {generated}"
        );
        assert!(
            generated.contains("PgCommentRepository"),
            "expected the child repo resolved by convention, got: {generated}"
        );
        assert!(
            generated.contains("__autumn_apply_dependent_on_conn"),
            "expected the cascade thunk to call the leaf executor, got: {generated}"
        );
    }

    #[test]
    fn belongs_to_dependent_is_rejected_as_meaningless() {
        // The child FK lives on the belongs_to side, so there is no dependent
        // to cascade — `dependent`/`on_delete` here is meaningless.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(User, dependent = destroy)])];
        let Err(err) = resolve_associations(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("belongs_to"),
            "expected a belongs_to-specific rejection, got: {msg}"
        );
    }

    #[test]
    fn has_many_without_dependent_still_parses() {
        // Regression guard: recognizing `dependent` must not disturb normal
        // `#[has_many]` parsing.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[has_many(Comment)]),
            syn::parse_quote!(#[has_many(Comment, fk = "post_id")]),
        ];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 2);
        assert_eq!(assocs[0].fk, "post_id");
        assert_eq!(assocs[1].fk, "post_id");
    }

    #[test]
    fn name_override_disambiguates_same_target() {
        // Two has_many to the same target, distinguished by `name =`.
        let model: syn::Ident = syn::parse_quote!(User);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[has_many(Post, fk = author_id, name = authored)]),
            syn::parse_quote!(#[has_many(Post, fk = approver_id, name = approved)]),
        ];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 2);
        assert_eq!(assocs[0].fk, "author_id");
        assert_eq!(assocs[0].name, "authored");
        assert_eq!(assocs[1].fk, "approver_id");
        assert_eq!(assocs[1].name, "approved");
    }

    // ── Many-to-many (`through = join_table`) parsing (#1324) ────────────

    #[test]
    fn has_many_through_infers_join_columns_and_name() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Tag, through = post_tags)])];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs.len(), 1);
        assert_eq!(assocs[0].kind, AssocKind::HasMany);
        // Join-table column pointing back at the source model.
        assert_eq!(assocs[0].fk, "post_id");
        assert_eq!(assocs[0].name, "tags");
        let through = assocs[0].through.as_ref().expect("through present");
        assert_eq!(through.table, "post_tags");
        // Join-table column pointing at the target model.
        assert_eq!(through.target_fk, "tag_id");
    }

    #[test]
    fn has_many_through_accepts_fk_and_target_fk_overrides() {
        let model: syn::Ident = syn::parse_quote!(Article);
        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(
            #[has_many(Label, through = taggings, fk = piece_id, target_fk = sticker_id)]
        )];
        let assocs = resolve_associations(&model, &attrs).expect("parse ok");
        assert_eq!(assocs[0].fk, "piece_id");
        assert_eq!(assocs[0].name, "labels");
        let through = assocs[0].through.as_ref().expect("through present");
        assert_eq!(through.table, "taggings");
        assert_eq!(through.target_fk, "sticker_id");
    }

    #[test]
    fn through_rejected_on_belongs_to_and_has_one() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let belongs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[belongs_to(User, through = post_users)])];
        assert!(resolve_associations(&model, &belongs).is_err());
        let has_one: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_one(Profile, through = post_profiles)])];
        assert!(resolve_associations(&model, &has_one).is_err());
    }

    #[test]
    fn dependent_on_through_association_is_rejected() {
        // A `through = <join_table>` association's fk names a column on the
        // join table, not on the target model, so the emitted cascade would
        // call the target repo's `__autumn_apply_dependent_on_conn` with a
        // column that does not exist there (e.g. `tags.post_id`) — deleting /
        // nullifying the wrong rows. Reject the combination directed rather
        // than silently mis-cascading (Codex P2).
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Tag, through = post_tags, dependent = destroy)])];
        let Err(err) = resolve_associations(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("through"),
            "expected a through-specific rejection, got: {msg}"
        );
        assert!(
            msg.contains("dependent") || msg.contains("on_delete"),
            "expected the cascade key named, got: {msg}"
        );
    }

    #[test]
    fn on_delete_on_through_association_is_rejected() {
        // The `on_delete =` alias is rejected on a `through =` association for
        // the same reason as `dependent =` (Codex P2).
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Tag, through = post_tags, on_delete = nullify)])];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    #[test]
    fn target_fk_without_through_is_error() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Tag, target_fk = tag_id)])];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    #[test]
    fn m2m_colliding_mutation_method_names_rejected() {
        // The mutation-helper singular comes from the *target type*, so two
        // `through =` associations to the same target both derive an `add_tag`
        // helper — even with a distinct `name = ...` (which only renames the
        // accessor/trait, not the target-derived `add_`/`remove_`). Reject at
        // macro time rather than emitting a trait with duplicate methods.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[has_many(Tag, through = post_tags)]),
            syn::parse_quote!(#[has_many(Tag, through = featured_post_tags, name = featured_tags)]),
        ];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    #[test]
    fn m2m_helper_override_re_enables_two_relations_to_same_target() {
        // #1785 escape hatch: two m2m associations to the *same* target type
        // (`User`, both through `friendships`) compile when each carries a
        // distinct `helper = "..."` override, because the collision check keys
        // on the *resolved* singular (the override) instead of the
        // target-derived one.
        let model: syn::Ident = syn::parse_quote!(User);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(
                #[has_many(User, through = friendships, name = followers,
                           fk = followed_id, target_fk = follower_id,
                           helper = follower)]
            ),
            syn::parse_quote!(
                #[has_many(User, through = friendships, name = following,
                           fk = follower_id, target_fk = followed_id,
                           helper = following)]
            ),
        ];
        assert!(
            resolve_associations(&model, &attrs).is_ok(),
            "distinct `helper = ...` overrides must re-enable dual m2m to one target"
        );
    }

    #[test]
    fn m2m_helper_override_matching_derived_still_collides() {
        // A `helper` override that resolves to the same singular as another
        // association's target-derived singular still collides — the check
        // keys on the resolved name, override or not.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[has_many(Tag, through = post_tags)]),
            syn::parse_quote!(
                #[has_many(Label, through = post_labels, name = labels, helper = tag)]
            ),
        ];
        assert!(
            resolve_associations(&model, &attrs).is_err(),
            "an override colliding with a derived singular must still be rejected"
        );
    }

    #[test]
    fn m2m_helper_override_requires_through() {
        // `helper = ...` only affects m2m mutation helpers, so it is rejected
        // on a non-`through` association rather than silently ignored.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Comment, helper = commentary)])];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    #[test]
    fn model_macro_m2m_helper_override_generates_distinct_helpers() {
        // End-to-end #1785: the followers/following-through-`Friendship`
        // pattern generates distinct `add_follower`/`add_following` (and
        // `remove_`) helpers keyed on the `helper = ...` override, not the
        // colliding target-derived `add_user`.
        let generated = model_macro(
            quote! {},
            quote! {
                #[has_many(User, through = friendships, name = followers,
                           fk = followed_id, target_fk = follower_id,
                           helper = follower)]
                #[has_many(User, through = friendships, name = following,
                           fk = follower_id, target_fk = followed_id,
                           helper = following)]
                pub struct User {
                    #[id]
                    pub id: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("add_follower"),
            "expected add_follower helper, got: {generated}"
        );
        assert!(
            generated.contains("remove_follower"),
            "expected remove_follower helper"
        );
        assert!(
            generated.contains("add_following"),
            "expected add_following helper"
        );
        assert!(
            generated.contains("remove_following"),
            "expected remove_following helper"
        );
        assert!(
            !generated.contains("add_user"),
            "must not fall back to the colliding target-derived add_user"
        );
        assert!(
            generated.contains("set_followers") && generated.contains("set_following"),
            "the set_ (replace-all) helpers keep their per-association accessor names"
        );
    }

    #[test]
    fn m2m_mutation_singular_derives_from_target_type() {
        // #1753 regression (Codex): the m2m mutation-helper singular must come
        // from the *target type* (`pascal_to_snake`), not by stripping a
        // trailing `s` from the smart-pluralized accessor name. Otherwise
        // irregular plurals produce broken helpers: `categories` → `categorie`
        // (should be `category`), `people` → `people` (should be `person`).
        let category: syn::Ident = syn::parse_quote!(Category); // -ies class
        let person: syn::Ident = syn::parse_quote!(Person); // irregular class
        let comment: syn::Ident = syn::parse_quote!(Comment); // plain class
        assert_eq!(m2m_mutation_singular(&category), "category");
        assert_eq!(m2m_mutation_singular(&person), "person");
        assert_eq!(m2m_mutation_singular(&comment), "comment");
    }

    #[test]
    fn model_macro_m2m_mutation_helper_singular_uses_target_for_irregular_plurals() {
        // End-to-end #1753 guard: a `through =` association to an
        // irregular-plural target keeps the smart-pluralized accessor/`set_`
        // name (`categories`) while the `add_`/`remove_` helpers use the
        // target type's singular (`category`), never the broken de-pluralized
        // accessor (`categorie`).
        let generated = model_macro(
            quote! {},
            quote! {
                #[has_many(Category, through = post_categories)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("add_category"),
            "expected add_category helper, got: {generated}"
        );
        assert!(
            generated.contains("remove_category"),
            "expected remove_category helper"
        );
        assert!(
            !generated.contains("add_categorie"),
            "must not emit the broken de-pluralized `add_categorie`"
        );
        assert!(
            generated.contains("set_categories"),
            "the set_ (replace-all) helper keeps the smart plural accessor name"
        );
    }

    #[test]
    fn model_macro_m2m_mutation_helper_singular_uses_target_for_irregular_person() {
        // `Person` → accessor `people`; the `add_`/`remove_` helpers must use
        // the target singular `person`, not the (unchanged-by-strip-`s`)
        // `people`.
        let generated = model_macro(
            quote! {},
            quote! {
                #[has_many(Person, through = team_people)]
                pub struct Team {
                    #[id]
                    pub id: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("add_person"),
            "expected add_person helper, got: {generated}"
        );
        assert!(
            generated.contains("remove_person"),
            "expected remove_person helper"
        );
        assert!(
            !generated.contains("add_people"),
            "must not emit the broken `add_people`"
        );
    }

    // ── Many-to-many codegen shape (#1324) ────────────────────────────────

    #[test]
    fn model_macro_m2m_emits_hidden_join_table_module() {
        let generated = model_macro(
            quote! {},
            quote! {
                #[has_many(Tag, through = post_tags)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("__autumn_m2m_post_tags") || generated.contains("__autumn_m2m"),
            "expected a hidden m2m join-table module, got: {generated}"
        );
        assert!(
            generated.contains("table !"),
            "expected a diesel table! invocation"
        );
        assert!(
            generated.contains("allow_tables_to_appear_in_same_query"),
            "expected the join table to be allowed alongside the target table"
        );
    }

    #[test]
    fn model_macro_m2m_loader_uses_single_inner_join_and_keep() {
        let generated = model_macro(
            quote! {},
            quote! {
                #[has_many(Tag, through = post_tags)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("inner_join"),
            "expected a single inner_join loader"
        );
        assert!(
            generated.contains("eq_any"),
            "expected a batched WHERE ... IN filter"
        );
        assert!(
            generated.contains("__autumn_preload_keep"),
            "expected the m2m loader to scope rows with the per-row keep predicate"
        );
    }

    #[test]
    fn model_macro_m2m_emits_mutation_trait() {
        let generated = model_macro(
            quote! {},
            quote! {
                #[has_many(Tag, through = post_tags)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("PostTagsMutations"),
            "expected a per-association mutation trait"
        );
        assert!(
            generated.contains("add_tag"),
            "expected an add_tag mutation helper"
        );
        assert!(
            generated.contains("remove_tag"),
            "expected a remove_tag mutation helper"
        );
        assert!(
            generated.contains("set_tags"),
            "expected a set_tags (replace-all) mutation helper"
        );
        assert!(
            generated.contains("on_conflict_do_nothing") || generated.contains("on_conflict"),
            "expected add_tag to be idempotent via ON CONFLICT DO NOTHING"
        );
        assert!(
            generated.contains("M2mConnSource"),
            "expected the mutation trait to be blanket-implemented over M2mConnSource"
        );
    }

    // ── Votable (#1362) parsing ───────────────────────────────────────────
    //
    // `#[votable(by = <Reactor>, ...)]` declares a reaction edge table plus an
    // aggregate column maintained on the model. Every key except `by` is
    // optional and inferred from conventions, so the defaults are the part
    // most worth pinning down: they are what makes the attribute a one-liner
    // on a conventionally-named schema.

    #[test]
    fn votable_defaults_map_onto_conventional_columns() {
        // The canonical declaration on a conventionally-named schema: every
        // column name is inferred, and the inference must land exactly on
        // `votes(user_id, post_id, value)` + `posts.score`.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregate = sum)])];
        let spec = resolve_votable(&model, &attrs)
            .expect("parse ok")
            .expect("a #[votable] spec");
        assert_eq!(spec.reactor.to_string(), "User");
        assert_eq!(spec.aggregate, VoteAggregate::Sum);
        assert_eq!(spec.name, "vote");
        assert_eq!(spec.table, "votes");
        assert_eq!(spec.reactor_fk, "user_id");
        assert_eq!(spec.target_fk, "post_id");
        assert_eq!(spec.value_column.as_deref(), Some("value"));
        assert_eq!(spec.column, "score");
    }

    #[test]
    fn votable_defaults_to_sum_when_aggregate_is_omitted() {
        // The attribute is *votable* — a vote is signed, so `sum` is the
        // default and `count` is the opt-in.
        let model: syn::Ident = syn::parse_quote!(Post);
        let bare: Vec<syn::Attribute> = vec![syn::parse_quote!(#[votable(by = User)])];
        let explicit: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregate = sum)])];
        let bare = resolve_votable(&model, &bare)
            .expect("parse ok")
            .expect("a #[votable] spec");
        let explicit = resolve_votable(&model, &explicit)
            .expect("parse ok")
            .expect("a #[votable] spec");
        assert_eq!(bare.aggregate, VoteAggregate::Sum);
        assert_eq!(bare.aggregate, explicit.aggregate);
        assert_eq!(bare.table, explicit.table);
        assert_eq!(bare.column, explicit.column);
        assert_eq!(bare.value_column, explicit.value_column);
    }

    #[test]
    fn votable_count_mode_infers_name_count_column() {
        // `aggregate = count` is unary membership: the aggregate column is
        // `{name}_count` and the edge table has no value column at all (its
        // rows are pure membership, like an m2m join row).
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregate = count)])];
        let spec = resolve_votable(&model, &attrs)
            .expect("parse ok")
            .expect("a #[votable] spec");
        assert_eq!(spec.aggregate, VoteAggregate::Count);
        assert_eq!(spec.name, "vote");
        assert_eq!(spec.table, "votes");
        assert_eq!(spec.column, "vote_count");
        assert_eq!(
            spec.value_column, None,
            "a count-mode edge table stores no value column"
        );
    }

    #[test]
    fn votable_name_override_drives_table_and_column() {
        // `name` is the single knob a likes feature needs: it drives both the
        // pluralized table name and the `{name}_count` aggregate column, so
        // `name = like` yields `likes` / `like_count` with no other overrides.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregate = count, name = like)])];
        let spec = resolve_votable(&model, &attrs)
            .expect("parse ok")
            .expect("a #[votable] spec");
        assert_eq!(spec.name, "like");
        assert_eq!(spec.table, "likes");
        assert_eq!(spec.column, "like_count");
        assert_eq!(spec.reactor_fk, "user_id");
        assert_eq!(spec.target_fk, "post_id");
    }

    #[test]
    fn votable_explicit_overrides_win() {
        // Every inferred name has an override for schemas that do not follow
        // the convention; none of the defaults may leak through.
        let model: syn::Ident = syn::parse_quote!(Article);
        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(
            #[votable(by = Member, aggregate = sum, name = rating, table = ratings,
                      reactor_fk = member_id, target_fk = piece_id,
                      value_column = weight, column = rating_total)]
        )];
        let spec = resolve_votable(&model, &attrs)
            .expect("parse ok")
            .expect("a #[votable] spec");
        assert_eq!(spec.reactor.to_string(), "Member");
        assert_eq!(spec.aggregate, VoteAggregate::Sum);
        assert_eq!(spec.name, "rating");
        assert_eq!(spec.table, "ratings");
        assert_eq!(spec.reactor_fk, "member_id");
        assert_eq!(spec.target_fk, "piece_id");
        assert_eq!(spec.value_column.as_deref(), Some("weight"));
        assert_eq!(spec.column, "rating_total");
    }

    #[test]
    fn votable_accepts_string_literal_values() {
        // Like the association attributes, each value may be spelled as a bare
        // ident or as a string literal — the two must resolve identically.
        let model: syn::Ident = syn::parse_quote!(Post);
        let idents: Vec<syn::Attribute> = vec![syn::parse_quote!(
            #[votable(by = User, name = vote, table = votes, reactor_fk = user_id,
                      target_fk = post_id, value_column = value, column = score)]
        )];
        let literals: Vec<syn::Attribute> = vec![syn::parse_quote!(
            #[votable(by = User, name = "vote", table = "votes", reactor_fk = "user_id",
                      target_fk = "post_id", value_column = "value", column = "score")]
        )];
        let idents = resolve_votable(&model, &idents)
            .expect("bare idents parse ok")
            .expect("a #[votable] spec");
        let literals = resolve_votable(&model, &literals)
            .expect("string literals parse ok")
            .expect("a #[votable] spec");
        assert_eq!(idents.name, literals.name);
        assert_eq!(idents.table, literals.table);
        assert_eq!(idents.reactor_fk, literals.reactor_fk);
        assert_eq!(idents.target_fk, literals.target_fk);
        assert_eq!(idents.value_column, literals.value_column);
        assert_eq!(idents.column, literals.column);
    }

    #[test]
    fn votable_requires_by() {
        // There is no positional head: the reactor model is always named, so a
        // `#[votable]` with no `by =` must say exactly what to add.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(#[votable(aggregate = sum)])];
        let Err(err) = resolve_votable(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("requires `by = <ReactorModel>`"),
            "expected the missing-`by` error to name the key, got: {msg}"
        );
    }

    #[test]
    fn votable_rejects_unknown_key() {
        // A typo'd key must enumerate the accepted vocabulary rather than
        // being silently ignored.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregat = sum)])];
        let Err(err) = resolve_votable(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        for key in [
            "by",
            "aggregate",
            "name",
            "table",
            "reactor_fk",
            "target_fk",
            "value_column",
            "column",
        ] {
            assert!(
                msg.contains(key),
                "expected the unknown-key error to enumerate `{key}`, got: {msg}"
            );
        }
        assert!(
            msg.contains("votable"),
            "expected the unknown-key error to name the attribute, got: {msg}"
        );
    }

    #[test]
    fn votable_rejects_unknown_aggregate() {
        // Only the two supported modes exist; `avg`/`star` style ratings are
        // explicitly out of scope for #1362 and must not resolve to a default.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregate = avg)])];
        let Err(err) = resolve_votable(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("unknown aggregate `avg`"),
            "expected the offending aggregate quoted back, got: {msg}"
        );
        assert!(
            msg.contains("sum") && msg.contains("count"),
            "expected both supported modes named, got: {msg}"
        );
    }

    #[test]
    fn votable_rejects_value_column_in_count_mode() {
        // A count-mode edge table has no value column, so `value_column = ...`
        // is a category error rather than a harmless extra key.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![syn::parse_quote!(
            #[votable(by = User, aggregate = count, value_column = value)]
        )];
        let Err(err) = resolve_votable(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("value_column"),
            "expected the offending key named, got: {msg}"
        );
        assert!(
            msg.contains("aggregate = count"),
            "expected the conflicting mode named, got: {msg}"
        );
        assert!(
            msg.contains("aggregate = sum"),
            "expected the fix (switch to sum, or drop the key) spelled out, got: {msg}"
        );
    }

    #[test]
    fn votable_rejects_duplicate_attribute() {
        // At most one `#[votable]` per model: the emitted trait is
        // `{Model}Reactions` with `react`/`reaction_of`, so a second
        // declaration would generate colliding methods.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[votable(by = User, aggregate = sum)]),
            syn::parse_quote!(#[votable(by = User, aggregate = count, name = like)]),
        ];
        let Err(err) = resolve_votable(&model, &attrs) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("at most one `#[votable]` per model"),
            "expected the one-per-model rule stated, got: {msg}"
        );
    }

    #[test]
    fn votable_rejects_composite_primary_keys() {
        // PR #2177 review (P1): with two `#[id]` fields the pk resolution
        // would silently take the first component — S1 could lock, and S5
        // update, every row sharing it, and distinct targets would collapse
        // onto one edge key. A composite-keyed model must be a directed
        // compile error, not a wrong-row runtime hazard.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Enrollment {
                    #[id]
                    pub course_id: i64,
                    #[id]
                    pub student_id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "expected a compile error for a composite-keyed votable model, \
             got: {generated}"
        );
        assert!(
            generated.contains("requires a single `i64` primary key")
                && generated.contains("`course_id`, `student_id`"),
            "expected the directed composite-key message naming both key \
             components, got: {generated}"
        );
    }

    #[test]
    fn votable_rejects_non_identifier_string_values() {
        // Every name-shaped value is spliced into a generated ident through
        // `format_ident!`, which *panics* on a non-identifier — an opaque
        // "proc macro panicked" with no span. Each key must instead produce a
        // directed error naming the offending value.
        let model: syn::Ident = syn::parse_quote!(Post);
        for (key, attr) in [
            (
                "table",
                syn::parse_quote!(#[votable(by = User, table = "my votes")]),
            ),
            (
                "name",
                syn::parse_quote!(#[votable(by = User, name = "my vote")]),
            ),
            (
                "reactor_fk",
                syn::parse_quote!(#[votable(by = User, reactor_fk = "user id")]),
            ),
            (
                "target_fk",
                syn::parse_quote!(#[votable(by = User, target_fk = "post-id")]),
            ),
            (
                "value_column",
                syn::parse_quote!(#[votable(by = User, value_column = "1value")]),
            ),
            (
                "column",
                syn::parse_quote!(#[votable(by = User, column = "score; DROP TABLE posts")]),
            ),
            ("by", syn::parse_quote!(#[votable(by = "Not A Type")])),
        ] {
            let attrs: Vec<syn::Attribute> = vec![attr];
            let Err(err) = resolve_votable(&model, &attrs) else {
                panic!("expected `{key}` with a non-identifier value to be rejected");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("is not a valid identifier") || msg.contains("valid model type name"),
                "expected a directed identifier error for `{key}`, got: {msg}"
            );
            assert!(
                msg.contains(key),
                "expected the error to name the offending key `{key}`, got: {msg}"
            );
        }
    }

    #[test]
    fn votable_rejects_raw_identifier_values() {
        // A raw identifier parses as an `Ident` but renders its `r#` prefix
        // into the generated table/column/module names, silently producing a
        // column that cannot exist. Reject it with the same directed error
        // rather than emitting garbage.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, name = r#type)])];
        let Err(err) = resolve_votable(&model, &attrs) else {
            panic!("expected a raw-identifier `name` to be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("is not a valid identifier"),
            "expected the identifier error, got: {msg}"
        );
        assert!(
            msg.contains("r#"),
            "expected the error to mention the `r#` prefix, got: {msg}"
        );
    }

    #[test]
    fn votable_rejects_duplicate_key_within_one_attribute() {
        // A repeated key silently last-writes today, so `by = User, by = Bot`
        // would compile against the wrong reactor. Reject it, quoting both
        // values so the mistake is obvious.
        let model: syn::Ident = syn::parse_quote!(Post);

        let by: Vec<syn::Attribute> = vec![syn::parse_quote!(#[votable(by = User, by = Bot)])];
        let Err(err) = resolve_votable(&model, &by) else {
            panic!("expected a duplicate `by` to be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate `by = ...`"),
            "expected the duplicate-key error to name the key, got: {msg}"
        );
        assert!(
            msg.contains("User") && msg.contains("Bot"),
            "expected both the previous and the new value quoted back, got: {msg}"
        );

        let aggregate: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[votable(by = User, aggregate = sum, aggregate = count)])];
        let Err(err) = resolve_votable(&model, &aggregate) else {
            panic!("expected a duplicate `aggregate` to be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate `aggregate = ...`"),
            "expected the duplicate-key error to name the key, got: {msg}"
        );
        assert!(
            msg.contains("sum") && msg.contains("count"),
            "expected both the previous and the new value quoted back, got: {msg}"
        );
    }

    #[test]
    fn votable_aggregate_column_must_exist_on_model() {
        // The hidden edge module declares the aggregate column regardless, so
        // a missing field would otherwise only surface as a runtime `42703` on
        // the first vote. Mirror the `shard_key` field-existence check.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "expected a compile error for the missing aggregate column, got: {generated}"
        );
        assert!(
            generated.contains("votable aggregate column `score` not found on model `Post`"),
            "expected the directed missing-column message, got: {generated}"
        );
        assert!(
            generated.contains("column = "),
            "expected the error to point at the `column = <field>` override, got: {generated}"
        );
    }

    #[test]
    fn votable_aggregate_column_must_be_i64() {
        // The aggregate is `SUM(value)` / `COUNT(*)` — BIGINT — projected as
        // `Int8` in the hidden module and written back as an `i64`. An `i32` or
        // `Option<i64>` field would otherwise fail as an opaque Diesel
        // trait-resolution wall inside the generated `react()`.
        for (ty, rendered) in [
            (quote! { i32 }, "i32"),
            (quote! { Option<i64> }, "Option < i64 >"),
        ] {
            let generated = model_macro(
                quote! {},
                quote! {
                    #[votable(by = User, aggregate = sum)]
                    pub struct Post {
                        #[id]
                        pub id: i64,
                        pub score: #ty,
                    }
                },
            )
            .to_string();

            assert!(
                generated.contains("compile_error"),
                "expected a compile error for a `{rendered}` aggregate column, got: {generated}"
            );
            assert!(
                generated
                    .contains("votable aggregate column `score` on model `Post` must be `i64`"),
                "expected the directed wrong-type message, got: {generated}"
            );
            assert!(
                generated.contains(&format!("found `{rendered}`")),
                "expected the offending type quoted back, got: {generated}"
            );
        }
    }

    #[test]
    fn votable_aggregate_accepts_equivalent_i64_spellings_via_typed_guard() {
        // PR #2177 review: the macro-level check must only reject spellings
        // that are *definitely* wrong. `std::primitive::i64` (or an alias) IS
        // `i64`; token-text equality would reject it. Such spellings pass the
        // macro and are enforced by the emitted `const` guard instead — which
        // must also be present in the plain-`i64` case as the backstop for
        // wrong aliases.
        for ty in [quote! { std::primitive::i64 }, quote! { i64 }] {
            let generated = model_macro(
                quote! {},
                quote! {
                    #[votable(by = User, aggregate = sum)]
                    pub struct Post {
                        #[id]
                        pub id: i64,
                        pub score: #ty,
                    }
                },
            )
            .to_string();

            assert!(
                !generated.contains("compile_error"),
                "an i64-equivalent spelling must not be rejected, got: {generated}"
            );
            assert!(
                generated.contains("__autumn_votable_model . score"),
                "expected the aggregate-field type guard to be emitted, got: {generated}"
            );
        }
    }

    // ── Votable (#1362) codegen shape ─────────────────────────────────────
    //
    // These assert on the *shape* of the emitted token stream (substrings of
    // `TokenStream::to_string()`, which space-separates tokens). Behaviour is
    // covered by the Docker-gated integration tests; what matters here is that
    // the race-safety primitives (immediate transaction, pg-only `FOR UPDATE`,
    // explicit `ON CONFLICT` arbiter) are actually present in the codegen.

    #[test]
    fn votable_emits_hidden_edge_table_module() {
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("mod __autumn_votable_4_post_vote"),
            "expected a length-prefixed hidden edge-table module, got: {generated}"
        );
        assert!(
            generated.contains("votes (user_id , post_id)"),
            "expected the edge table keyed on the composite (reactor, target) \
             pair — the load-bearing ON CONFLICT arbiter, got: {generated}"
        );
        assert!(
            generated.contains("user_id -> Int8") && generated.contains("post_id -> Int8"),
            "expected non-nullable Int8 fk columns (a NULL target would defeat \
             the unique arbiter), got: {generated}"
        );
        assert!(
            generated.contains("value -> Int2"),
            "expected the sum-mode value column typed SMALLINT, got: {generated}"
        );
    }

    #[test]
    fn votable_emits_target_projection_in_the_hidden_module() {
        // Deliberately self-contained: the target table is projected inside the
        // hidden module (id + aggregate [+ deleted_at]) rather than referencing
        // the app's `crate::schema::*`, so the codegen cannot pick up a
        // conflicting column type and needs no schema module in scope.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("posts (id)"),
            "expected a minimal target-table projection, got: {generated}"
        );
        assert!(
            generated.contains("score -> Int8"),
            "expected the aggregate column projected as BIGINT, got: {generated}"
        );
    }

    #[test]
    fn votable_target_projection_keys_on_the_models_real_primary_key() {
        // PR #2177 review (P1): a model whose `#[id]` field is not named `id`
        // (e.g. `memo_id`) has no `id` column at all — or worse, an unrelated
        // one. The hidden projection, the S1 lock, and the S5 aggregate UPDATE
        // must all key on the resolved primary-key column, never a hard-coded
        // `id`.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Memo {
                    #[id]
                    pub memo_id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("memos (memo_id)"),
            "expected the target projection keyed on the model's real primary \
             key, got: {generated}"
        );
        assert!(
            !generated.contains("memos (id)"),
            "no hard-coded `id` primary key may survive on the target \
             projection, got: {generated}"
        );
        assert!(
            generated
                .matches("memos :: memo_id . eq (target_id)")
                .count()
                >= 3,
            "S1 (both backend arms) and S5 must filter the target table on the \
             resolved primary key, got: {generated}"
        );
    }

    #[test]
    fn votable_emits_reactions_trait_and_blanket_impl() {
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("trait PostReactions"),
            "expected a per-model reactions trait, got: {generated}"
        );
        assert!(
            generated.contains("fn react"),
            "expected the react() helper, got: {generated}"
        );
        assert!(
            generated.contains("fn reaction_of"),
            "expected the reaction_of() accessor, got: {generated}"
        );
        assert!(
            generated.contains("M2mConnSource < Model = Post >"),
            "expected the trait blanket-implemented over the repository's \
             connection source, got: {generated}"
        );
    }

    #[test]
    fn votable_react_runs_in_an_immediate_transaction() {
        // The edge mutation and the aggregate recompute must commit together,
        // and the SQLite arm needs `BEGIN IMMEDIATE` (single writer) rather
        // than a deferred snapshot that upgrades mid-transaction.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("scoped_immediate_transaction"),
            "expected react() to wrap its statements in the write-path \
             transaction primitive, got: {generated}"
        );
    }

    #[test]
    fn votable_react_locks_the_target_row_on_pg_only() {
        // A locking clause is a parse error on SQLite, so the row lock lives in
        // the `pg` arm of `backend_select!` — the unselected arm is never
        // type-checked — and SQLite gets its mutual exclusion from BEGIN
        // IMMEDIATE. `FOR NO KEY UPDATE`, not `FOR UPDATE`: `react()` writes only
        // a non-key column, and the weaker mode still self-conflicts, keeping
        // reactions on one target serialized, without conflicting with the `FOR
        // KEY SHARE` locks Postgres takes for foreign-key checks, so a concurrent
        // `INSERT INTO comments (post_id) …` does not queue behind a vote.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("backend_select"),
            "expected the per-backend split, got: {generated}"
        );
        // The lock must be part of the S1 locking SELECT itself (same statement
        // as the existence/soft-delete guard), not merely present somewhere.
        assert!(
            generated.contains(
                "pg => { __autumn_votable_4_post_vote :: posts :: table . filter \
                 (__autumn_votable_4_post_vote :: posts :: id . eq (target_id)) . select \
                 (__autumn_votable_4_post_vote :: posts :: id) . for_no_key_update ()"
            ),
            "expected FOR NO KEY UPDATE chained onto the pg arm's S1 target \
             select, got: {generated}"
        );
        // …and it must live *only* there: the sqlite arm cannot even parse it.
        let s1 = generated
            .split_once("let __target")
            .expect("the S1 locking select")
            .1;
        let pg_arm = s1
            .split_once("sqlite =>")
            .expect("the backend_select! sqlite arm")
            .0;
        assert!(
            pg_arm.contains(". for_no_key_update ()"),
            "expected the row lock inside the pg arm, got: {pg_arm}"
        );
        assert_eq!(
            generated.matches("for_no_key_update").count(),
            1,
            "expected exactly one locking clause (pg arm only), got: {generated}"
        );
    }

    #[test]
    fn votable_react_upserts_with_an_explicit_arbiter() {
        // An `ON CONFLICT DO UPDATE` with no arbiter is a syntax error, and the
        // wrong column list raises 42P10 on a table carrying more than one
        // unique constraint (reddit-clone's `votes` has two).
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        // The full arbiter column list, not just "an on_conflict somewhere": a
        // regression to a single-column arbiter (`(user_id)`) is exactly the
        // 42P10 this pins, and would still satisfy a bare `contains`.
        assert!(
            generated.contains(
                "on_conflict ((__autumn_votable_4_post_vote :: votes :: user_id , \
                 __autumn_votable_4_post_vote :: votes :: post_id ,))"
            ),
            "expected the composite (reactor_fk, target_fk) ON CONFLICT \
             arbiter, got: {generated}"
        );
        assert!(
            generated.contains("do_update"),
            "expected ON CONFLICT DO UPDATE (not DO NOTHING), got: {generated}"
        );
        assert!(
            generated.contains("excluded"),
            "expected the conflicting row to take EXCLUDED.value, got: {generated}"
        );
    }

    #[test]
    fn votable_count_mode_react_has_no_value_parameter() {
        // A count reaction is unary membership, so `react()` drops the `value`
        // argument entirely rather than taking an ignored one.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = count)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub vote_count: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("trait PostReactions") && generated.contains("fn react"),
            "expected the reactions trait in count mode too, got: {generated}"
        );
        assert!(
            !generated.contains("value : i16"),
            "count-mode react() must not take a value parameter, got: {generated}"
        );
        assert!(
            !generated.contains("-> Int2"),
            "a count-mode edge table has no value column, got: {generated}"
        );
    }

    #[test]
    fn votable_soft_delete_target_gates_the_lock_and_the_update() {
        // AC6: when the model has a `deleted_at` field, both the locking SELECT
        // and the aggregate UPDATE carry `deleted_at IS NULL`, so a
        // soft-deleted target is NotFound and its aggregate is untouched. A
        // model without the field pays nothing.
        let soft = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                    pub deleted_at: Option<chrono::NaiveDateTime>,
                }
            },
        )
        .to_string();
        let hard = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        // Three sites, all load-bearing: the pg arm's locking S1 select, the
        // sqlite arm's S1 select, and the S5 aggregate UPDATE. Dropping any one
        // of them lets a soft-deleted target be reacted on (or its aggregate
        // rewritten), so count them rather than accepting a single hit.
        assert!(
            soft.matches("deleted_at . is_null ()").count() >= 3,
            "expected the soft-delete guard on both S1 arms and the S5 update \
             (>= 3 sites), got {}: {soft}",
            soft.matches("deleted_at . is_null ()").count()
        );
        assert!(
            soft.contains("deleted_at -> Nullable < Timestamp >"),
            "expected deleted_at projected into the hidden target table, got: {soft}"
        );
        assert!(
            !hard.contains("is_null"),
            "a model without deleted_at must not emit a soft-delete guard, got: {hard}"
        );
    }

    /// The hidden `__autumn_votable_*` module and everything the `#[votable]`
    /// declaration emits after it. Scoping the tenant assertions to this slice
    /// keeps them from matching the *rest* of `#[model]`'s codegen, which
    /// legitimately mentions `tenant_id` for a model that has the column
    /// (`HasTenantIdColumn`, the insertable selector, …).
    fn votable_slice(generated: &str) -> &str {
        let start = generated
            .find("mod __autumn_votable_")
            .expect("the votable codegen starts at its hidden module");
        &generated[start..]
    }

    #[test]
    fn votable_tenant_scoped_target_is_filtered_in_s1_s5_and_reaction_of() {
        // PR #2177 review (P1): through a `#[repository(..., tenant_scoped)]`
        // repository, filtering the target by primary key alone lets a caller
        // react to ANOTHER tenant's row — an edge insert plus an aggregate
        // UPDATE across the tenant boundary. When the model carries a
        // `tenant_id` column the codegen must project it and emit a
        // tenant-filtered arm everywhere the target is touched.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub tenant_id: String,
                    pub score: i64,
                }
            },
        )
        .to_string();
        let votable = votable_slice(&generated);

        assert!(
            votable.contains("tenant_id -> Text"),
            "expected tenant_id projected into the hidden target table, got: {votable}"
        );
        assert!(
            votable.contains("self . __autumn_m2m_tenant_scope ()"),
            "expected the tenant predicate resolved through the repository's \
             M2mConnSource, not read from the task-local directly, got: {votable}"
        );

        // Exactly four tenant-filtered sites, each load-bearing and each the scoped
        // half of a two-arm match; the other arm is the unfiltered query, taken for a
        // non-`tenant_scoped` repository or `across_tenants()`:
        //
        //   1. S1, `pg` arm     — the locking existence guard,
        //   2. S1, `sqlite` arm — the same guard without the row lock,
        //   3. S5               — the aggregate UPDATE,
        //   4. `reaction_of`    — the tenant EXISTS folded into the edge lookup.
        //
        // The arms stay separate whole queries rather than one boxed query with a
        // conditional predicate, because S1's `pg` arm carries `.for_no_key_update()`.
        // Pinned exactly: a fifth would mean a duplicated statement, and a third that
        // some path lost its filter and can write across the tenant boundary again.
        assert_eq!(
            votable.matches("tenant_id . eq").count(),
            4,
            "expected exactly 4 tenant-filtered target sites (S1 pg, S1 sqlite, \
             S5, reaction_of's EXISTS), got: {votable}"
        );
        // reaction_of's tenant boundary must be a single-snapshot predicate —
        // an EXISTS on the target inside the edge lookup itself — never a
        // separate probe statement, which under READ COMMITTED is a TOCTOU
        // window: a concurrent tenant reassignment lands between probe and
        // lookup and the foreign edge leaks anyway (PR #2177 review).
        assert!(
            votable.contains(":: dsl :: exists"),
            "reaction_of's tenant check must be an EXISTS folded into the edge \
             lookup, got: {votable}"
        );
        assert!(
            !votable.contains("__in_tenant"),
            "the two-statement tenant probe must not survive, got: {votable}"
        );
        assert!(
            votable.contains("for_no_key_update"),
            "the tenant-filtered pg arm must keep the row lock, got: {votable}"
        );
        // Both halves of each match survive: the unfiltered arm is what a
        // non-tenant_scoped repository (and `across_tenants()`) still takes.
        // Both halves of every match survive. The unfiltered arm is what a
        // non-`tenant_scoped` repository — and `across_tenants()` — still
        // takes, so the target lookup appears twice at each of S1's two
        // backend arms and at S5, plus once in `reaction_of`'s probe: 3 * 2 + 1.
        assert_eq!(
            votable.matches("posts :: id . eq (target_id)").count(),
            7,
            "expected a tenant-filtered AND an unfiltered arm at S1 (both \
             backends) and S5, plus reaction_of's probe, got: {votable}"
        );
    }

    #[test]
    fn votable_model_without_tenant_id_emits_no_tenant_scoping() {
        // AC: zero query cost. A model with no `tenant_id` column can have no
        // meaningful `tenant_scoped` repository — its own derived queries would not
        // compile — so the reaction queries must carry no tenant projection,
        // predicate, or runtime branch. The one tenant token that must still appear is
        // the discarded `__autumn_m2m_tenant_scope()?` call in each method: it carries
        // the cross-shard reject (PR #2177 — `across_tenants()` on a sharded repository
        // has no single right shard for a reaction), and on a plain repository it is a
        // constant `Ok(None)`.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();
        let votable = votable_slice(&generated);

        assert!(
            !votable.contains("tenant_id"),
            "a model without tenant_id must emit no tenant column, predicate, \
             or projection anywhere in the votable items, got: {votable}"
        );
        assert_eq!(
            votable
                .matches("self . __autumn_m2m_tenant_scope ()")
                .count(),
            2,
            "react() and reaction_of() must each still call the tenant-scope \
             method (discarded) so the cross-shard reject fires before any \
             connection is acquired, got: {votable}"
        );
        assert!(
            !votable.contains("__tenant"),
            "no tenant runtime branch may survive on a tenant-less model, \
             got: {votable}"
        );
    }

    #[test]
    fn votable_emits_i64_primary_key_and_reactor_type_guards() {
        // Two compile-time guards the rest of the codegen silently assumes:
        //
        // * the whole reaction surface is typed on `i64` ids (both edge fks are
        //   `Int8`, `react(reactor_id: i64, target_id: i64)` binds them
        //   directly), so a UUID- or i32-keyed model must fail *at the model*;
        // * `by = <Reactor>` is otherwise never mentioned in the emitted code —
        //   the edge stores a bare `i64` — so a typo'd model name would compile
        //   silently unless its name resolution is forced.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("const _ : fn (& Post) -> i64"),
            "expected the i64-primary-key guard, got: {generated}"
        );
        assert!(
            generated.contains("PhantomData < User >"),
            "expected the reactor-type name-resolution guard, got: {generated}"
        );
    }

    #[test]
    fn votable_react_returns_a_reaction_via_the_hidden_constructor() {
        // `Reaction` is `#[non_exhaustive]`, so the generated code — which
        // expands in the *application's* crate — cannot write a struct literal.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("repository :: Reaction :: __new"),
            "expected the Reaction to be built through its hidden constructor, \
             got: {generated}"
        );
        assert!(
            !generated.contains("Reaction { value"),
            "a struct literal would not compile downstream of #[non_exhaustive], \
             got: {generated}"
        );
    }

    #[test]
    fn votable_react_fails_loudly_when_the_aggregate_update_hits_no_row() {
        // Defense in depth (S5): unreachable while S1's lock holds, but if the
        // lock strength ever regresses, committing an edge whose aggregate was
        // silently dropped is the one failure mode that leaves the two out of
        // sync forever.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("let __persisted : usize"),
            "expected the S5 update's affected-row count to be captured, got: {generated}"
        );
        assert!(
            generated.contains("if __persisted == 0"),
            "expected a zero-row S5 update to abort the transaction, got: {generated}"
        );
    }

    #[test]
    fn votable_reaction_of_reads_on_a_read_connection() {
        // `reaction_of` is a read: it routes per the repository's ReadRoute
        // (replica-eligible) and must NOT mark the read-your-writes pin, which
        // acquiring the write connection would do. `react` keeps the write one.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("self . __autumn_m2m_read_conn ()"),
            "expected reaction_of to take a read connection, got: {generated}"
        );
        assert_eq!(
            generated.matches("__autumn_m2m_write_conn").count(),
            1,
            "expected exactly one write-connection acquisition (react's), got: {generated}"
        );
    }

    #[test]
    fn votable_react_doc_warns_that_a_toggle_is_not_idempotent() {
        // A toggle is not idempotent, and the dangerous corollary is that a
        // blind retry of a timed-out call can invert the outcome. The generated
        // rustdoc must say so rather than claiming idempotence.
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            !generated.contains("Idempotent"),
            "react() is a toggle — the docs must not claim idempotence, got: {generated}"
        );
        assert!(
            generated.contains("Not idempotent"),
            "expected the toggle caveat in react()'s docs, got: {generated}"
        );
        assert!(
            generated.contains("idempotency key"),
            "expected the retry-safety guidance in react()'s docs, got: {generated}"
        );
        assert!(
            generated.contains("does not pin read-your-writes") || generated.contains("read route"),
            "expected reaction_of's read-routing note, got: {generated}"
        );
    }

    #[test]
    fn votable_attribute_is_stripped_from_the_diesel_struct() {
        // `#[votable]` is consumed by `#[model]`; re-emitting it onto the
        // generated Diesel struct would fail with "cannot find attribute
        // `votable` in this scope".
        let generated = model_macro(
            quote! {},
            quote! {
                #[votable(by = User, aggregate = sum)]
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            !generated.contains("# [votable"),
            "the votable attribute must not leak onto the emitted struct, got: {generated}"
        );
    }

    #[test]
    fn model_without_votable_emits_no_reaction_items() {
        // The reaction surface is opt-in: a model that never declares
        // `#[votable]` must not gain a trait, a hidden module, or dead code.
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub score: i64,
                }
            },
        )
        .to_string();

        assert!(
            !generated.contains("Reactions"),
            "expected no reactions trait without #[votable], got: {generated}"
        );
        assert!(
            !generated.contains("__autumn_votable"),
            "expected no hidden edge module without #[votable], got: {generated}"
        );
    }

    #[test]
    fn lock_version_attr_detected_by_has_attr() {
        let field: syn::Field = syn::parse_quote! {
            #[lock_version]
            pub version: i32
        };
        assert!(has_attr(&field, "lock_version"));
    }

    #[test]
    fn lock_version_field_is_excluded_from_new() {
        let field: syn::Field = syn::parse_quote! {
            #[lock_version]
            pub lock_version: i32
        };
        // A #[lock_version] field must be absent from NewModel (the DB
        // supplies the initial value via a DEFAULT constraint).
        assert!(excluded_from_new(&field));
    }

    #[test]
    fn regular_field_is_not_excluded_from_new() {
        let field: syn::Field = syn::parse_quote! {
            pub title: String
        };
        assert!(!excluded_from_new(&field));
    }

    #[test]
    fn id_field_is_still_excluded_from_new() {
        let field: syn::Field = syn::parse_quote! {
            #[id]
            pub id: i64
        };
        assert!(excluded_from_new(&field));
    }

    #[test]
    fn position_attr_detected_by_has_attr() {
        let field: syn::Field = syn::parse_quote! {
            #[position]
            pub rank: i64
        };
        assert!(has_attr(&field, "position"));
    }

    #[test]
    fn position_field_is_excluded_from_new() {
        let field: syn::Field = syn::parse_quote! {
            #[position]
            pub rank: i64
        };
        // A #[position] field must be absent from NewModel/UpdateModel — the
        // generated repository assigns and maintains it entirely (#1358).
        assert!(excluded_from_new(&field));
    }

    #[test]
    fn position_bare_attribute_with_args_is_rejected() {
        let field: syn::Field = syn::parse_quote! {
            #[position(scope = "board_id")]
            pub rank: i64
        };
        let err = validate_field_schema_markers(&field).unwrap_err();
        assert!(
            err.to_string().contains("position"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn position_attribute_is_stripped_from_the_diesel_struct() {
        // `#[position]` is consumed by `#[model]`; re-emitting it onto the
        // generated Diesel struct would fail with "cannot find attribute
        // `position` in this scope".
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Task {
                    #[id]
                    pub id: i64,
                    #[position]
                    pub rank: i64,
                }
            },
        )
        .to_string();

        assert!(
            !generated.contains("# [position]"),
            "the position attribute must not leak onto the emitted struct, got: {generated}"
        );
    }

    #[test]
    fn position_field_excluded_from_new_and_update_structs() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Task {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    #[position]
                    pub rank: i64,
                }
            },
        )
        .to_string();

        let new_struct = generated
            .split("struct NewTask")
            .nth(1)
            .expect("NewTask struct should be emitted");
        let new_struct_body = &new_struct[..new_struct.find('}').unwrap_or(new_struct.len())];
        assert!(
            !new_struct_body.contains("rank"),
            "NewTask must not contain the server-managed position field, got: {new_struct_body}"
        );

        let update_struct = generated
            .split("struct UpdateTask")
            .nth(1)
            .expect("UpdateTask struct should be emitted");
        let update_struct_body =
            &update_struct[..update_struct.find('}').unwrap_or(update_struct.len())];
        assert!(
            !update_struct_body.contains("rank"),
            "UpdateTask must not contain the server-managed position field, got: {update_struct_body}"
        );
    }

    #[test]
    fn encrypted_string_field_is_accepted() {
        let field: syn::Field = syn::parse_quote! {
            #[encrypted]
            pub token: String
        };
        assert!(validate_encrypted_field(&field).is_ok());
    }

    #[test]
    fn encrypted_plus_searchable_is_rejected() {
        // Search indexes the stored ciphertext, so plaintext queries would miss —
        // the combination must be a compile error (#805).
        let field: syn::Field = syn::parse_quote! {
            #[encrypted]
            #[searchable]
            pub token: String
        };
        let err = validate_encrypted_field(&field).unwrap_err();
        assert!(err.to_string().contains("searchable"));
    }

    // ── #1384: `#[translatable]` field attribute ────────────────────────────

    /// #1384 (Codex round 5): the locale-map schema is keyed on the
    /// `#[translatable]` MARKER, not on the type's leaf name. An application
    /// type that merely happens to be called `Translated` must keep its
    /// ordinary `$ref`, or the advertised `OpenAPI` contract would silently
    /// disagree with what that type actually serializes to.
    #[test]
    fn translatable_schema_is_keyed_on_the_attribute_not_the_type_name() {
        let generated = model_macro(
            quote! { table = "posts" },
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    #[translatable]
                    pub title: ::autumn_web::i18n::Translated,
                    // A user's OWN type with the same leaf name, unmarked.
                    pub note: domain::Translated,
                }
            },
        )
        .to_string();

        // The marked field is described inline as a locale-tag -> string map,
        // so `autumn openapi` emits no reference to a component nothing
        // registers.
        assert!(
            generated.contains("additionalProperties"),
            "translatable field should carry an inline map schema"
        );
        // The unmarked look-alike still gets the ordinary `$ref` branch.
        assert!(
            generated.contains("components/schemas"),
            "an unmarked `Translated` look-alike must keep its $ref"
        );
    }

    /// A model with NO translatable field emits no inline map schema at all.
    #[test]
    fn an_unmarked_translated_lookalike_alone_emits_no_map_schema() {
        let generated = model_macro(
            quote! { table = "posts" },
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub note: domain::Translated,
                }
            },
        )
        .to_string();
        assert!(
            !generated.contains("additionalProperties"),
            "no `#[translatable]` field means no locale-map schema"
        );
    }

    #[test]
    fn translatable_field_is_accepted_on_a_translated_column() {
        let field: syn::Field = syn::parse_quote! {
            #[translatable]
            pub title: Translated
        };
        assert!(validate_translatable_field(&field).is_ok());
        let qualified: syn::Field = syn::parse_quote! {
            #[translatable]
            pub title: ::autumn_web::i18n::Translated
        };
        assert!(validate_translatable_field(&qualified).is_ok());
    }

    #[test]
    fn translatable_on_a_plain_string_is_rejected_with_the_fix() {
        let field: syn::Field = syn::parse_quote! {
            #[translatable]
            pub title: String
        };
        let err = validate_translatable_field(&field).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Translated"), "{msg}");
    }

    #[test]
    fn translatable_option_is_rejected() {
        // The container itself models "no translation" (an empty map), so a
        // nullable column would give two ways to say the same thing.
        let field: syn::Field = syn::parse_quote! {
            #[translatable]
            pub title: Option<Translated>
        };
        assert!(validate_translatable_field(&field).is_err());
    }

    #[test]
    fn translatable_plus_encrypted_is_rejected() {
        let field: syn::Field = syn::parse_quote! {
            #[translatable]
            #[encrypted]
            pub title: Translated
        };
        let err = validate_translatable_field(&field).unwrap_err();
        assert!(err.to_string().contains("encrypted"), "{err}");
    }

    #[test]
    fn translatable_plus_searchable_is_rejected() {
        // The stored column is a JSON container; a tsvector built from it
        // would index JSON punctuation and locale tags, not the prose.
        let field: syn::Field = syn::parse_quote! {
            #[translatable]
            #[searchable]
            pub title: Translated
        };
        let err = validate_translatable_field(&field).unwrap_err();
        assert!(err.to_string().contains("searchable"), "{err}");
    }

    #[test]
    fn translatable_plus_equality_markers_are_rejected() {
        for marker in [
            "unique",
            "indexed",
            "normalize",
            "id",
            "lock_version",
            "position",
        ] {
            let marker_ident = syn::Ident::new(marker, proc_macro2::Span::call_site());
            let field: syn::Field = syn::parse_quote! {
                #[translatable]
                #[#marker_ident]
                pub title: Translated
            };
            assert!(
                validate_translatable_field(&field).is_err(),
                "`#[{marker}]` must not combine with `#[translatable]`"
            );
        }
    }

    #[test]
    fn translatable_model_emits_accessors_and_registry() {
        let generated = model_macro(
            quote! { table = "posts" },
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    #[translatable]
                    pub title: ::autumn_web::i18n::Translated,
                    pub body: String,
                }
            },
        )
        .to_string();

        // The attribute is stripped from the emitted struct (rustc has no
        // `#[translatable]` attribute of its own).
        let query_struct = generated
            .split("pub struct Post")
            .nth(1)
            .expect("Post struct emitted");
        let body = &query_struct[..query_struct.find('}').unwrap_or(query_struct.len())];
        assert!(
            !body.contains("translatable"),
            "attr must be stripped: {body}"
        );

        // Per-field accessors (AC2 / AC5).
        for expected in [
            "fn title_localized",
            "fn title_in",
            "fn set_title",
            "fn title_locales",
            "fn title_is_translated",
        ] {
            assert!(generated.contains(expected), "missing {expected}");
        }
        // Model-level, field-name-keyed surface (AC5 wording).
        for expected in [
            "fn translatable_fields",
            "fn available_locales",
            "fn is_translated",
            "fn localized",
            "__AUTUMN_TRANSLATABLE_COLUMNS",
        ] {
            assert!(generated.contains(expected), "missing {expected}");
        }
        // Registry submission (AC5 observability from outside the model).
        assert!(generated.contains("TranslatableColumnDescriptor"));
    }

    #[test]
    fn model_without_translatable_fields_emits_no_translatable_accessors() {
        // AC7: purely additive — a model that opts out is untouched apart from
        // the always-emitted (empty) column constant.
        let generated = model_macro(
            quote! { table = "posts" },
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        )
        .to_string();
        assert!(!generated.contains("TranslatableColumnDescriptor"));
        assert!(!generated.contains("fn translatable_fields"));
        assert!(!generated.contains("fn available_locales"));
        assert!(generated.contains("__AUTUMN_TRANSLATABLE_COLUMNS"));
    }

    // ── #1191: `#[searchable(embed)]` parsing ───────────────────────────────

    #[test]
    fn tenant_id_detection_requires_a_string_shaped_column() {
        assert!(is_string_or_option_string(&syn::parse_quote!(String)));
        assert!(is_string_or_option_string(&syn::parse_quote!(
            Option<String>
        )));
        assert!(is_string_or_option_string(&syn::parse_quote!(
            ::std::option::Option<::std::string::String>
        )));
        // An unrelated `tenant_id: i64` must NOT make the index tenant-scoped.
        assert!(!is_string_or_option_string(&syn::parse_quote!(i64)));
        assert!(!is_string_or_option_string(&syn::parse_quote!(Option<i64>)));
        assert!(!is_string_or_option_string(&syn::parse_quote!(Uuid)));
    }

    #[test]
    fn a_non_string_tenant_id_column_does_not_make_the_index_tenant_scoped() {
        let input: TokenStream = quote! {
            #[searchable]
            pub struct Reading {
                #[id]
                pub id: i64,
                #[searchable]
                pub label: String,
                // A sensor reading's "tenant_id" that is really a device id.
                pub tenant_id: i64,
            }
        };
        let expanded = model_macro(quote! {}, input).to_string();
        assert!(expanded.contains("SearchIndexed for Reading"), "{expanded}");
        // The tenant extraction (and the tenant_scoped flag) must be absent.
        assert!(
            !expanded.contains("__autumn_tenant"),
            "a non-String tenant_id must not be extracted: {expanded}"
        );
    }

    #[test]
    fn searchable_embed_flag_is_parsed_alongside_the_weight() {
        let field: syn::Field = syn::parse_quote! {
            #[searchable(weight = "B", embed)]
            pub body: String
        };
        let cfg = parse_field_searchable(&field).expect("parses");
        assert!(cfg.embed);
        assert!(matches!(
            cfg.kind,
            FieldSearchable::SearchableWithWeight(ref w) if w == "B"
        ));
    }

    #[test]
    fn searchable_embed_alone_defaults_the_weight() {
        let field: syn::Field = syn::parse_quote! {
            #[searchable(embed)]
            pub body: String
        };
        let cfg = parse_field_searchable(&field).expect("parses");
        assert!(cfg.embed);
        assert!(matches!(cfg.kind, FieldSearchable::SearchableDefault));
    }

    #[test]
    fn searchable_without_embed_leaves_the_flag_off() {
        let field: syn::Field = syn::parse_quote! {
            #[searchable(weight = "A")]
            pub title: String
        };
        assert!(!parse_field_searchable(&field).expect("parses").embed);

        let bare: syn::Field = syn::parse_quote! {
            #[searchable]
            pub title: String
        };
        assert!(!parse_field_searchable(&bare).expect("parses").embed);

        let none: syn::Field = syn::parse_quote! {
            pub title: String
        };
        let cfg = parse_field_searchable(&none).expect("parses");
        assert!(!cfg.embed);
        assert!(matches!(cfg.kind, FieldSearchable::NotSearchable));
    }

    #[test]
    fn unknown_searchable_key_names_the_supported_set() {
        let field: syn::Field = syn::parse_quote! {
            #[searchable(vectorize)]
            pub body: String
        };
        let err = parse_field_searchable(&field).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("weight"), "{message}");
        assert!(message.contains("embed"), "{message}");
    }

    #[test]
    fn duplicate_embed_is_rejected() {
        let field: syn::Field = syn::parse_quote! {
            #[searchable(embed, embed)]
            pub body: String
        };
        let err = parse_field_searchable(&field).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn two_embed_fields_are_rejected_by_the_model_macro() {
        // A record has ONE embedding; two `embed` fields is ambiguous, so it
        // must be a compile error rather than a silent last-wins.
        let input: TokenStream = quote! {
            #[searchable(language = "english")]
            pub struct Article {
                #[id]
                pub id: i64,
                #[searchable(weight = "A", embed)]
                pub title: String,
                #[searchable(weight = "B", embed)]
                pub body: String,
            }
        };
        let expanded = model_macro(quote! {}, input).to_string();
        assert!(
            expanded.contains("only one field may be marked"),
            "{expanded}"
        );
    }

    #[test]
    fn embed_without_a_model_level_searchable_is_rejected() {
        // `#[searchable(embed)]` on a field of a model that is not itself
        // `#[searchable]` would produce an index nothing ever queries.
        let input: TokenStream = quote! {
            pub struct Article {
                #[id]
                pub id: i64,
                #[searchable(embed)]
                pub body: String,
            }
        };
        let expanded = model_macro(quote! {}, input).to_string();
        assert!(
            expanded.contains("requires the model to be marked"),
            "{expanded}"
        );
    }

    // ── #1654: compile-time data classification ─────────────────────────

    fn classified_model() -> String {
        model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    pub name: String,
                    #[classified]
                    pub email: String,
                }
            },
        )
        .to_string()
    }

    #[test]
    fn a_classified_column_is_generated_as_the_taint_wrapper() {
        let generated = classified_model();
        assert!(
            generated.contains("classify :: Classified < String , CustomerEmailClassified >"),
            "the read struct must carry the classification on the type: {generated}"
        );
        assert!(
            generated.contains("serialize_as = :: autumn_web :: classify :: ClassifiedText"),
            "the column must round-trip through the opaque Diesel wrapper: {generated}"
        );
    }

    #[test]
    fn the_write_structs_carry_the_classification_too() {
        // #1654 review round 2 (P1): `NewCustomer`/`UpdateCustomer` are the
        // primary way an application *receives* a classified value, and their
        // fields are `pub`. Leaving them as a bare `String` meant a handler
        // could move the plaintext straight into a serializable view --
        // `Json(View { email: input.email })` -- releasing personal data with
        // no boundary and no audit record. `skip_serializing` only stops the
        // write struct itself from being serialized; it does nothing about the
        // value being moved out of it.
        let generated = classified_model();
        let write_structs = generated
            .split("pub struct NewCustomer")
            .nth(1)
            .expect("the generated write structs");
        assert!(
            write_structs.contains("classify :: Classified < String , CustomerEmailClassified >"),
            "NewCustomer must carry the classification on the field type: {write_structs}"
        );
        assert!(
            write_structs.contains(
                "Patch < :: autumn_web :: classify :: Classified < String , \
                 CustomerEmailClassified > >"
            ),
            "UpdateCustomer's patch field must carry it too: {write_structs}"
        );
        // The unclassified column is untouched, so this is not a blanket rewrite.
        assert!(
            write_structs.contains("pub name : String"),
            "an unclassified column must stay exactly as declared: {write_structs}"
        );
    }

    #[test]
    fn a_classified_model_does_not_derive_serialize() {
        let generated = classified_model();
        // `Deserialize` stays: taking personal data in is not a release.
        assert!(
            generated.contains("derive (:: serde :: Deserialize)"),
            "{generated}"
        );
        assert!(
            !generated
                .contains("derive (:: serde :: Serialize , :: serde :: Deserialize) ] # [ doc"),
            "{generated}"
        );
        let query_struct = generated
            .split("pub struct Customer")
            .next()
            .expect("query struct preamble");
        assert!(
            !query_struct.contains(":: serde :: Serialize"),
            "the read struct must not implement Serialize: {query_struct}"
        );
    }

    #[test]
    fn a_classified_column_publishes_its_field_marker_and_manifest_row() {
        let generated = classified_model();
        assert!(
            generated.contains("struct CustomerEmailClassified"),
            "{generated}"
        );
        assert!(
            generated.contains("classify :: ClassifiedField for CustomerEmailClassified"),
            "{generated}"
        );
        assert!(
            generated.contains("ClassifiedFieldDescriptor"),
            "the column must reach the data-flow manifest: {generated}"
        );
        assert!(
            generated.contains("module_path !"),
            "the manifest identity must be module-qualified so two `Customer`s \
             in different crates cannot share a row: {generated}"
        );
        assert!(
            generated.contains("Classification :: PersonalData"),
            "{generated}"
        );
    }

    #[test]
    fn the_factory_carries_the_classification_too() {
        // #2373. The generated factory was the last struct of the family still
        // holding plaintext: `Customer::factory().email` was a public `String`,
        // so it could be moved into a serializable view with no boundary and no
        // audit record, and the factory's derived `Debug` printed the real
        // value. Classifying only at `build()` time is too late -- the
        // plaintext is readable on the factory for its whole lifetime. The
        // factory is emitted in every build, not just tests.
        let generated = classified_model();
        // Scope to the struct body: everything after the header up to its
        // closing brace. Splitting alone would match the whole rest of the
        // expansion and pass on any other struct's classified field.
        let after = generated
            .split("pub struct CustomerFactory")
            .nth(1)
            .expect("the generated factory struct");
        let factory = &after[..after.find('}').expect("struct body ends")];
        assert!(
            factory.contains("classify :: Classified < String , CustomerEmailClassified >"),
            "the factory field must carry the classification: {factory}"
        );
        assert!(
            factory.contains("pub name : String"),
            "an unclassified column must stay exactly as declared: {factory}"
        );
    }

    #[test]
    fn the_factory_debug_redacts_a_classified_column() {
        let generated = classified_model();
        let factory_debug = generated
            .split("impl :: core :: fmt :: Debug for CustomerFactory")
            .nth(1)
            .expect("the factory must get a redacting Debug, not a derived one");
        let body = &factory_debug[..factory_debug.len().min(400)];
        assert!(
            body.contains("<classified>"),
            "the factory's Debug must redact the classified column: {body}"
        );
    }

    #[test]
    fn the_write_structs_never_serialize_a_classified_column() {
        // The companion to `the_write_structs_carry_the_classification_too`:
        // the wrapper stops the value being moved out, and `skip_serializing`
        // stops the write struct itself from carrying it into a response body.
        // `skip_serializing` (not `skip`) keeps `Deserialize` intact, which is
        // what makes the create/PATCH bodies decode at all.
        let generated = classified_model();
        assert!(
            generated.contains("serde (skip_serializing)"),
            "the write structs must not serialize a classified column: {generated}"
        );
    }

    #[test]
    fn the_changeset_writes_a_classified_column_through_the_opaque_column_type() {
        let generated = classified_model();
        let changeset = generated
            .split("pub struct __CustomerChangeset")
            .nth(1)
            .expect("the generated changeset");
        assert!(
            changeset.contains("ClassifiedText < CustomerEmailClassified >"),
            "the changeset must write through the opaque Diesel type: {changeset}"
        );
        assert!(
            changeset.contains(
                "Option < :: autumn_web :: classify :: Classified < String , \
                 CustomerEmailClassified > >"
            ),
            "and hold the wrapper, not the plaintext: {changeset}"
        );
    }

    #[test]
    fn a_classified_column_is_redacted_from_debug() {
        let generated = classified_model();
        assert!(generated.contains("<classified>"), "{generated}");
    }

    #[test]
    fn a_classified_model_refuses_the_durable_commit_hook_payload() {
        let generated = classified_model();
        assert!(
            generated.contains("durable commit-hook payload"),
            "the second, unclassified copy of the record must be refused: {generated}"
        );
    }

    #[test]
    fn a_classified_column_is_not_client_filterable_or_sortable() {
        // `?filter[email]=` and `?sort=email` are client-controlled. A
        // classified column in either allowlist is an equality oracle and an
        // ordering leak that compiles, with no boundary and no manifest row.
        let generated = classified_model();
        let filters_start = generated
            .find("fn __autumn_list_apply_filters")
            .expect("filter helper must be generated");
        let orders_start = generated
            .find("fn __autumn_list_apply_order")
            .expect("order helper must be generated");
        let filters = &generated[filters_start..orders_start];
        assert!(filters.contains("\"name\""), "{filters}");
        assert!(!filters.contains("\"email\""), "{filters}");
        // The order helper is the last item in its `impl` block; bound the slice
        // so the rest of the expansion (which legitimately names the column)
        // cannot satisfy the assertion.
        let orders_end = generated[orders_start..]
            .find("} impl ")
            .map_or(generated.len(), |o| orders_start + o);
        let orders = &generated[orders_start..orders_end];
        assert!(orders.contains("\"name\""), "{orders}");
        assert!(!orders.contains("\"email\""), "{orders}");
    }

    #[test]
    fn every_model_publishes_its_classified_column_list() {
        assert!(
            classified_model().contains("__AUTUMN_CLASSIFIED_COLUMNS"),
            "the column list is what a surface with no compile-time view of the model reads"
        );
    }

    #[test]
    fn classified_is_rejected_on_the_tenant_isolation_key() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified]
                    pub tenant_id: String,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("cannot be applied to `tenant_id`"),
            "{generated}"
        );
    }

    #[test]
    fn an_unclassified_model_is_completely_unchanged() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Widget {
                    #[id]
                    pub id: i64,
                    pub label: String,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("derive (:: serde :: Serialize , :: serde :: Deserialize)"),
            "{generated}"
        );
        assert!(!generated.contains("Classified"), "{generated}");
        assert!(!generated.contains("<classified>"), "{generated}");
    }

    #[test]
    fn classified_is_rejected_on_a_non_string_column() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified]
                    pub age: i32,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("only supported on non-null `String` fields"),
            "{generated}"
        );
    }

    #[test]
    fn classified_rejects_an_unknown_tier() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified(top_secret)]
                    pub email: String,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("unsupported `#[classified]` tier"),
            "{generated}"
        );
    }

    #[test]
    fn classified_rejects_conflicting_column_markers() {
        for marker in ["encrypted", "searchable", "normalize", "state_machine"] {
            let marker_ident = format_ident!("{marker}");
            let extra = if marker == "normalize" {
                quote! { #[#marker_ident(trim)] }
            } else if marker == "state_machine" {
                quote! { #[#marker_ident(transitions(a -> b))] }
            } else {
                quote! { #[#marker_ident] }
            };
            let generated = model_macro(
                quote! {},
                quote! {
                    pub struct Customer {
                        #[id]
                        pub id: i64,
                        #[classified]
                        #extra
                        pub email: String,
                    }
                },
            )
            .to_string();
            assert!(
                generated.contains("cannot be combined with"),
                "`#[classified]` + `#[{marker}]` must be rejected: {generated}"
            );
        }
    }

    #[test]
    fn classified_rejects_an_unknown_tier_behind_a_bare_marker() {
        // The bare marker must not short-circuit validation of the attributes
        // after it, or the column is recorded as `personal_data` while the tier
        // the author asked for is silently dropped.
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified]
                    #[classified(top_secret)]
                    pub email: String,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("unsupported `#[classified]` tier"),
            "{generated}"
        );
    }

    #[test]
    fn classified_rejects_a_custom_serde_adapter() {
        for adapter in [
            quote! { #[serde(with = "my_codec")] },
            quote! { #[serde(serialize_with = "my_ser")] },
            quote! { #[serde(deserialize_with = "my_de")] },
        ] {
            let generated = model_macro(
                quote! {},
                quote! {
                    pub struct Customer {
                        #[id]
                        pub id: i64,
                        #[classified]
                        #adapter
                        pub email: String,
                    }
                },
            )
            .to_string();
            assert!(
                generated.contains("cannot be combined with `#[serde(with"),
                "{generated}"
            );
        }
    }

    #[test]
    fn a_conflicting_marker_reports_the_conflict_not_the_string_rule() {
        // `#[translatable]` implies a non-`String` column, so checking the type
        // first would bury the reason that tells the author what to do.
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified]
                    #[translatable]
                    pub bio: Translated,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("cannot be combined with `#[translatable]`"),
            "{generated}"
        );
    }

    #[test]
    fn classified_rejects_a_serde_rename() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified]
                    #[serde(rename = "e_mail")]
                    pub email: String,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("cannot use `#[serde(rename"),
            "{generated}"
        );
    }

    #[test]
    fn the_field_marker_camel_cases_a_multi_word_column() {
        let generated = model_macro(
            quote! {},
            quote! {
                pub struct Customer {
                    #[id]
                    pub id: i64,
                    #[classified]
                    pub home_address_line: String,
                }
            },
        )
        .to_string();
        assert!(
            generated.contains("struct CustomerHomeAddressLineClassified"),
            "{generated}"
        );
    }

    #[test]
    fn searchable_model_emits_the_search_indexed_impl() {
        let input: TokenStream = quote! {
            #[searchable(language = "english")]
            pub struct Article {
                #[id]
                pub id: i64,
                #[searchable(weight = "A")]
                pub title: String,
                #[searchable(weight = "B", embed)]
                pub body: String,
            }
        };
        let expanded = model_macro(quote! {}, input).to_string();
        assert!(expanded.contains("SearchIndexed for Article"), "{expanded}");
        assert!(expanded.contains("SEARCH_EMBED_FIELD"), "{expanded}");
    }

    #[test]
    fn non_searchable_model_emits_no_search_indexed_impl() {
        let input: TokenStream = quote! {
            pub struct Article {
                #[id]
                pub id: i64,
                pub title: String,
            }
        };
        let expanded = model_macro(quote! {}, input).to_string();
        assert!(
            !expanded.contains("SearchIndexed for Article"),
            "{expanded}"
        );
    }

    #[test]
    fn searchable_model_with_a_non_i64_key_emits_no_search_indexed_impl() {
        // The plugin index keys on `i64` (as the in-core FTS already does), so
        // a `Uuid`-keyed model keeps its #842 behaviour and simply has no
        // pluggable index — rather than emitting an impl that cannot compile.
        let input: TokenStream = quote! {
            #[searchable]
            pub struct Article {
                #[id]
                pub id: uuid::Uuid,
                #[searchable]
                pub title: String,
            }
        };
        let expanded = model_macro(quote! {}, input).to_string();
        assert!(
            !expanded.contains("SearchIndexed for Article"),
            "{expanded}"
        );
    }

    #[test]
    fn already_skips_serialization_with_nested_list_before_skip() {
        // Regression: a nested-list item (`bound(serialize = ...)`, which has no
        // `= value`) previously errored out `parse_nested_meta` mid-attribute, so
        // a later `skip_serializing` in the SAME `#[serde(...)]` was never seen and
        // the macro injected a duplicate `#[serde(skip_serializing)]`.
        let field: syn::Field = syn::parse_quote! {
            #[serde(bound(serialize = "T: Clone"), skip_serializing)]
            pub secret: String
        };
        assert!(
            field_already_skips_serialization(&field),
            "a nested list before `skip_serializing` must not hide the skip"
        );
    }

    // ── #1374: `#[private]` hides a column from JSON serialization ─────────

    #[test]
    fn private_attr_filtered_from_user_attrs() {
        // The raw `#[private]` marker must not leak onto the generated Diesel
        // query struct — Diesel doesn't understand it.
        let field: syn::Field = syn::parse_quote! {
            #[private]
            pub password_hash: String
        };
        let attrs = user_attrs(&field);
        assert!(
            attrs.iter().all(|a| !a.path().is_ident("private")),
            "`#[private]` must be stripped from the query struct's attrs"
        );
    }

    #[test]
    fn private_field_gets_skip_serializing_in_query_struct() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    pub email: String,
                    #[private]
                    pub password_hash: String,
                }
            },
        );
        let generated = output.to_string();
        // The generated model struct field for `password_hash` must carry
        // `#[serde(skip_serializing)]` so it never appears in JSON output,
        // while `email` (public) must not be skipped.
        assert!(
            generated.contains("skip_serializing"),
            "a `#[private]` field must emit `#[serde(skip_serializing)]`: {generated}"
        );
        // The field is still a real, queryable Rust field on the struct.
        assert!(
            generated.contains("pub password_hash : String"),
            "the `#[private]` column must remain a normal queryable field: {generated}"
        );
    }

    #[test]
    fn private_field_still_writable_on_new_struct() {
        // AC: write/deserialize path unaffected — the NewUser struct must still
        // bind `password_hash` so a client can set (but never read back) it.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    pub email: String,
                    #[private]
                    pub password_hash: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("struct NewUser"),
            "NewUser must be generated: {generated}"
        );
        // NewUser must contain the password_hash field (write path intact) and
        // must NOT skip it on the write struct.
        let new_start = generated.find("struct NewUser").unwrap();
        let new_section = &generated[new_start..new_start + 400];
        assert!(
            new_section.contains("password_hash"),
            "NewUser must still bind the `#[private]` column for writes: {new_section}"
        );
    }

    #[test]
    fn encrypted_field_is_private_in_json_by_default() {
        // AC: `#[encrypted]` fields are `#[private]` in JSON by default —
        // plaintext (held in Rust) / ciphertext must never leak to the API.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Account {
                    #[id]
                    pub id: i64,
                    #[encrypted]
                    pub ssn: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("skip_serializing"),
            "an `#[encrypted]` field must be skipped from Serialize by default: {generated}"
        );
    }

    #[test]
    fn encrypted_admin_visible_field_is_serialized() {
        // AC: opt-in exposure mirrors the existing `admin_visible` pattern —
        // an `#[encrypted(admin_visible)]` field opts back into serialization.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Account {
                    #[id]
                    pub id: i64,
                    #[encrypted(admin_visible)]
                    pub tier: String,
                }
            },
        );
        let generated = output.to_string();
        // With only an admin_visible encrypted field and no `#[private]` field,
        // there must be no skip_serializing injected by our logic.
        assert!(
            !generated.contains("skip_serializing"),
            "`admin_visible` encrypted field must remain serialized: {generated}"
        );
    }

    #[test]
    fn public_field_is_not_skipped() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Widget {
                    #[id]
                    pub id: i64,
                    pub name: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            !generated.contains("skip_serializing"),
            "a model with no private/encrypted fields must not skip anything: {generated}"
        );
    }

    // ── #1379: `#[normalize]` canonicalizes String columns ────────────────

    #[test]
    fn normalize_attr_filtered_from_user_attrs() {
        let field: syn::Field = syn::parse_quote! {
            #[normalize(trim, downcase)]
            pub email: String
        };
        let attrs = user_attrs(&field);
        assert!(
            attrs.iter().all(|a| !a.path().is_ident("normalize")),
            "`#[normalize]` must be stripped from the query struct's attrs"
        );
    }

    #[test]
    fn parse_field_normalize_reads_builtins_left_to_right() {
        let field: syn::Field = syn::parse_quote! {
            #[normalize(trim, downcase, squish, upcase)]
            pub email: String
        };
        let ops = parse_field_normalize(&field).unwrap();
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], Normalizer::Trim));
        assert!(matches!(ops[1], Normalizer::Downcase));
        assert!(matches!(ops[2], Normalizer::Squish));
        assert!(matches!(ops[3], Normalizer::Upcase));
    }

    #[test]
    fn parse_field_normalize_reads_with_escape_hatch() {
        let field: syn::Field = syn::parse_quote! {
            #[normalize(trim, with = my_crate::canonicalize)]
            pub slug: String
        };
        let ops = parse_field_normalize(&field).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], Normalizer::Trim));
        assert!(matches!(ops[1], Normalizer::With(_)));
    }

    #[test]
    fn normalize_without_normalizers_is_rejected() {
        // `Vec<Normalizer>` isn't `Debug`, so `unwrap_err` won't compile; assert
        // on the error message via an explicit match instead.
        let err_msg = |field: &syn::Field| match parse_field_normalize(field) {
            Ok(_) => panic!("expected `#[normalize]` to error"),
            Err(e) => e.to_string(),
        };

        // Bare `#[normalize]` must error rather than register a no-op.
        let bare: syn::Field = syn::parse_quote! {
            #[normalize]
            pub email: String
        };
        assert!(
            err_msg(&bare).contains("requires at least one normalizer"),
            "bare `#[normalize]` must error"
        );

        // Empty `#[normalize()]` is likewise a silent identity no-op — reject it
        // with the same diagnostic instead of registering a do-nothing chain.
        let empty: syn::Field = syn::parse_quote! {
            #[normalize()]
            pub email: String
        };
        assert!(
            err_msg(&empty).contains("requires at least one normalizer"),
            "empty `#[normalize()]` must error"
        );
    }

    #[test]
    fn normalize_non_string_field_is_rejected() {
        // AC7: clear compile error on non-String, mirroring `#[encrypted]`.
        let field: syn::Field = syn::parse_quote! {
            #[normalize(trim)]
            pub age: i64
        };
        let err = validate_normalize_field(&field).unwrap_err();
        assert!(
            err.to_string().contains("String"),
            "error must mention String: {err}"
        );

        // `Option<String>` is also rejected in this slice.
        let field: syn::Field = syn::parse_quote! {
            #[normalize(trim)]
            pub nickname: Option<String>
        };
        assert!(validate_normalize_field(&field).is_err());
    }

    #[test]
    fn normalize_generates_normalize_and_lookup_impls() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    #[normalize(trim, downcase)]
                    pub email: String,
                }
            },
        );
        let generated = output.to_string();
        // Normalize impl for the insert struct (write path).
        assert!(
            generated.contains("impl :: autumn_web :: normalize :: Normalize for NewUser"),
            "must generate `impl Normalize for NewUser`: {generated}"
        );
        // NormalizedModel impl for finder-argument normalization.
        assert!(
            generated.contains("impl :: autumn_web :: normalize :: NormalizedModel for User"),
            "must generate `impl NormalizedModel for User`: {generated}"
        );
        // The lookup match must key on the serialized column name.
        assert!(
            generated.contains("\"email\""),
            "normalize_lookup must match the `email` column: {generated}"
        );
        // The builtin normalizers must be referenced.
        assert!(
            generated.contains("normalize :: trim") && generated.contains("normalize :: downcase"),
            "must chain the trim+downcase builtins: {generated}"
        );
    }

    #[test]
    fn update_model_drops_non_declarative_validators_but_new_model_keeps_them() {
        // #1719: `Patch<T>` implements validator's per-field declarative traits
        // (length/email/…) but NOT `custom`/`must_match`/`nested`/etc. The
        // `UpdateModel` fields must therefore drop the non-declarative validators
        // (or they'd fail to compile), while the `NewModel` keeps every validator.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    #[validate(length(min = 1), custom(function = "v"))]
                    pub name: String,
                }
            },
        );
        let generated = output.to_string();

        // Slice the `NewUser` struct body (up to its closing brace).
        let new_start = generated
            .find("struct NewUser")
            .expect("NewUser struct must be generated");
        let new_end = new_start
            + generated[new_start..]
                .find('}')
                .expect("NewUser struct must close");
        let new_section = &generated[new_start..new_end];
        assert!(
            new_section.contains("length"),
            "NewUser must keep the `length` validator: {new_section}"
        );
        assert!(
            new_section.contains("custom"),
            "NewUser must keep the `custom` validator: {new_section}"
        );

        // Slice the `UpdateUser` struct body (up to its closing brace).
        let upd_start = generated
            .find("struct UpdateUser")
            .expect("UpdateUser struct must be generated");
        let upd_end = upd_start
            + generated[upd_start..]
                .find('}')
                .expect("UpdateUser struct must close");
        let upd_section = &generated[upd_start..upd_end];
        assert!(
            upd_section.contains("length"),
            "UpdateUser must keep the declarative `length` validator: {upd_section}"
        );
        assert!(
            !upd_section.contains("custom"),
            "UpdateUser must NOT carry the non-declarative `custom` validator: {upd_section}"
        );
    }

    #[test]
    fn read_model_keeps_full_validator_set_and_from_patch_validates_merged_model() {
        // #1778: the read model retains EVERY `#[validate(...)]` rule (including
        // the ones dropped from the `Patch<T>` update fields, e.g. `custom`) and
        // derives `validator::Validate`, so `from_patch` can validate the
        // effective merged model (existing row ∪ patch) on the update path.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    #[validate(length(min = 1), custom(function = "v"))]
                    pub name: String,
                }
            },
        );
        let generated = output.to_string();

        // Slice the read-model `struct User` body (first occurrence, before
        // `NewUser`/`UpdateUser`/`UserChangeset`).
        let read_start = generated
            .find("struct User")
            .expect("read model struct must be generated");
        let read_end = read_start
            + generated[read_start..]
                .find('}')
                .expect("read model struct must close");
        let read_section = &generated[read_start..read_end];
        assert!(
            read_section.contains("length") && read_section.contains("custom"),
            "read model must keep the FULL validator set (incl. `custom`, which the \
             Patch<T> update fields drop) so the merged model can enforce it: {read_section}"
        );

        // The read model now derives `Validate` too, so the derive appears three
        // times (read model + NewUser + UpdateUser) rather than twice.
        let derive_count = generated.matches("validator :: Validate").count();
        assert_eq!(
            derive_count, 3,
            "read model, NewModel, and UpdateModel must each derive `validator::Validate`: {generated}"
        );

        // `from_patch` validates the merged concrete model via the autoref
        // `MaybeValidate` specialization (same 422 mapping as create).
        assert!(
            generated.contains("from_patch") && generated.contains("autumn_maybe_validate"),
            "from_patch must validate the merged model via autumn_maybe_validate: {generated}"
        );
    }

    #[test]
    fn read_model_without_validation_does_not_derive_validate_or_validate_on_merge() {
        // Symmetric guard: a model with no `#[validate(...)]` rules must NOT gain
        // a `Validate` derive on the read model nor a merged-model check in
        // `from_patch` — the autoref no-op is only paid for by validated models.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Plain {
                    #[id]
                    pub id: i64,
                    pub name: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            !generated.contains("validator :: Validate"),
            "an unvalidated model must not derive `validator::Validate`: {generated}"
        );
        assert!(
            !generated.contains("autumn_maybe_validate"),
            "an unvalidated model's from_patch must not emit a merged-model check: {generated}"
        );
    }

    #[test]
    fn update_model_drops_every_non_patch_validator_but_new_model_keeps_them() {
        // #1751 (the residual tail of #1742/#1719): lock in that the full
        // `NON_PATCH_VALIDATORS` denylist, not just `custom`, is stripped from the
        // generated `UpdateModel` `Patch<T>` fields while `NewModel` keeps every one.
        // These are enforced on create only and are genuinely unfixable on the PATCH
        // path without the merged-model redesign:
        //   * `must_match` and `nested` are cross-field or struct-level, and no
        //     single-field `Patch<T>` trait exists for them. `nested` also carries a
        //     separate, `Patch<T>`-independent `ValidateExt` hazard even where it does
        //     compile — see `NON_PATCH_VALIDATORS`'s doc comment — but that is
        //     orthogonal to this token-level test.
        //   * `credit_card` (`ValidateCreditCard`) and `non_control_character`
        //     (`ValidateNonControlCharacter`) are not exported under this workspace's
        //     `validator` feature set, so no `Patch<T>` impl can be written without
        //     enabling new features.
        // This is pure token-level filtering — `model_macro` does not compile its
        // output — so the combination need not be semantically valid; only that each
        // validator ident is dropped from the patch struct.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Signup {
                    #[id]
                    pub id: i64,
                    #[validate(
                        length(min = 1),
                        must_match(other = "confirm"),
                        nested,
                        credit_card,
                        non_control_character
                    )]
                    pub password: String,
                    pub confirm: String,
                }
            },
        );
        let generated = output.to_string();

        // Slice the `NewSignup` struct body (up to its closing brace).
        let new_start = generated
            .find("struct NewSignup")
            .expect("NewSignup struct must be generated");
        let new_end = new_start
            + generated[new_start..]
                .find('}')
                .expect("NewSignup struct must close");
        let new_section = &generated[new_start..new_end];
        for kept in [
            "length",
            "must_match",
            "nested",
            "credit_card",
            "non_control_character",
        ] {
            assert!(
                new_section.contains(kept),
                "NewSignup must keep the `{kept}` validator (enforced on create): {new_section}"
            );
        }

        // Slice the `UpdateSignup` struct body (up to its closing brace).
        let upd_start = generated
            .find("struct UpdateSignup")
            .expect("UpdateSignup struct must be generated");
        let upd_end = upd_start
            + generated[upd_start..]
                .find('}')
                .expect("UpdateSignup struct must close");
        let upd_section = &generated[upd_start..upd_end];
        // The lone declarative validator is retained on the patch field.
        assert!(
            upd_section.contains("length"),
            "UpdateSignup must keep the declarative `length` validator: {upd_section}"
        );
        // Every non-declarative validator is stripped from the patch field.
        for dropped in [
            "must_match",
            "nested",
            "credit_card",
            "non_control_character",
        ] {
            assert!(
                !upd_section.contains(dropped),
                "UpdateSignup must NOT carry the non-Patch `{dropped}` validator \
                 (would break UpdateModel compilation / has no Patch<T> impl): {upd_section}"
            );
        }
    }

    #[test]
    fn update_model_drops_does_not_contain_but_new_model_keeps_it() {
        // #1719 follow-up: `does_not_contain` reaches `Patch<T>` through
        // validator's blanket `impl<T: ValidateContains> ValidateDoesNotContain`,
        // which computes `!validate_contains(...)`. Our `ValidateContains for
        // Patch<T>` returns `true` for an absent field (so `contains` passes),
        // which inverts to `false` for `does_not_contain` — an OMITTED patch
        // field would then spuriously fail with 422. So `does_not_contain` must
        // be dropped from the `UpdateModel` `Patch<T>` fields (enforced on create
        // via `NewModel` only), while `contains`/`length` must be RETAINED.
        //
        // #1751: a hand-written `ValidateDoesNotContain for Patch<T>` (which would
        // let us keep it with correct skip semantics) is impossible — it collides
        // with validator's blanket `impl<T: ValidateContains> ValidateDoesNotContain
        // for T` (E0119: `Patch<T>` already impls `ValidateContains`). So the
        // create-only filtering below is a genuine coherence wall, not a stopgap.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Doc {
                    #[id]
                    pub id: i64,
                    #[validate(length(min = 1), contains(pattern = "ok"), does_not_contain(pattern = "bad"))]
                    pub name: String,
                }
            },
        );
        let generated = output.to_string();

        // Slice the `NewDoc` struct body (up to its closing brace).
        let new_start = generated
            .find("struct NewDoc")
            .expect("NewDoc struct must be generated");
        let new_end = new_start
            + generated[new_start..]
                .find('}')
                .expect("NewDoc struct must close");
        let new_section = &generated[new_start..new_end];
        assert!(
            new_section.contains("does_not_contain"),
            "NewDoc must keep the `does_not_contain` validator: {new_section}"
        );
        assert!(
            new_section.contains("contains"),
            "NewDoc must keep the `contains` validator: {new_section}"
        );
        assert!(
            new_section.contains("length"),
            "NewDoc must keep the `length` validator: {new_section}"
        );

        // Slice the `UpdateDoc` struct body (up to its closing brace).
        let upd_start = generated
            .find("struct UpdateDoc")
            .expect("UpdateDoc struct must be generated");
        let upd_end = upd_start
            + generated[upd_start..]
                .find('}')
                .expect("UpdateDoc struct must close");
        let upd_section = &generated[upd_start..upd_end];
        assert!(
            !upd_section.contains("does_not_contain"),
            "UpdateDoc must NOT carry `does_not_contain` (inverts the Patch skip \
             value into a spurious 422 for omitted fields): {upd_section}"
        );
        // Not over-filtered: `contains` and `length` are still valid on Patch<T>.
        assert!(
            upd_section.contains("contains"),
            "UpdateDoc must keep the declarative `contains` validator: {upd_section}"
        );
        assert!(
            upd_section.contains("length"),
            "UpdateDoc must keep the declarative `length` validator: {upd_section}"
        );
    }

    #[test]
    fn update_model_retains_required_on_patch_fields() {
        // #1719 / Codex P2: `required` MUST propagate to the `UpdateModel`
        // `Patch<Option<T>>` fields. A PATCH/PUT sending explicit JSON `null`
        // deserializes to `Patch::Clear`; if `required` were dropped the update
        // would silently write SQL `NULL`, violating the model's `required`
        // contract (create still rejects `None`). The tri-state
        // `impl ValidateRequired for Patch<T>` (autumn/src/hooks.rs) enforces it:
        // `Unchanged` skips, `Clear`/`Set(None)` fail (422). So both `NewUser`
        // and `UpdateUser` must carry `required`.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    #[validate(required)]
                    pub nickname: Option<String>,
                }
            },
        );
        let generated = output.to_string();

        // Slice the `NewUser` struct body (up to its closing brace).
        let new_start = generated
            .find("struct NewUser")
            .expect("NewUser struct must be generated");
        let new_end = new_start
            + generated[new_start..]
                .find('}')
                .expect("NewUser struct must close");
        let new_section = &generated[new_start..new_end];
        assert!(
            new_section.contains("required"),
            "NewUser must keep the `required` validator: {new_section}"
        );

        // Slice the `UpdateUser` struct body (up to its closing brace).
        let upd_start = generated
            .find("struct UpdateUser")
            .expect("UpdateUser struct must be generated");
        let upd_end = upd_start
            + generated[upd_start..]
                .find('}')
                .expect("UpdateUser struct must close");
        let upd_section = &generated[upd_start..upd_end];
        assert!(
            upd_section.contains("required"),
            "UpdateUser must RETAIN the `required` validator so a PATCH sending \
             `null` (Patch::Clear) is rejected with 422: {upd_section}"
        );
    }

    #[test]
    fn update_model_drops_ip_only_on_option_fields_new_model_keeps_all() {
        // #1719 / Codex P2: `validator` provides no `impl ValidateIp for Option<T>`,
        // only the `impl<T: ToString> ValidateIp for T` blanket, so
        // `Patch<Option<String>>: ValidateIp` is unsatisfied and the generated
        // `UpdateModel` would fail to compile for an `Option<String>` field carrying
        // `#[validate(ip)]`. So `ip` is dropped from the patch fields only when the
        // field is `Option<…>`, while:
        //   * `ip` is kept on a non-`Option` field, where `Patch<String>: ValidateIp`
        //     holds via the `ToString` blanket,
        //   * `length` is kept on the `Option` field, since validator ships an
        //     `Option<T>` impl and it must not be over-filtered,
        //   * every validator is kept on `NewModel`, whose derive unwraps `Option`.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Server {
                    #[id]
                    pub id: i64,
                    #[validate(ip, length(min = 1))]
                    pub ip: Option<String>,
                    #[validate(ip)]
                    pub ip2: String,
                    #[validate(length(min = 1))]
                    pub name: Option<String>,
                }
            },
        );
        let generated = output.to_string();

        // Slice the `NewServer` struct body (up to its closing brace).
        let new_start = generated
            .find("struct NewServer")
            .expect("NewServer struct must be generated");
        let new_end = new_start
            + generated[new_start..]
                .find('}')
                .expect("NewServer struct must close");
        let new_section = &generated[new_start..new_end];
        // NewServer keeps `ip` on BOTH ip fields (the derive unwraps Option):
        // the combined attr on the Option field and the bare attr on ip2.
        assert!(
            new_section.contains("validate (ip , length"),
            "NewServer must keep the `ip` (and `length`) validator on the \
             Option<String> field: {new_section}"
        );
        assert!(
            new_section.contains("validate (ip)"),
            "NewServer must keep the `ip` validator on the non-Option field: {new_section}"
        );

        // Slice the `UpdateServer` struct body (up to its closing brace).
        let upd_start = generated
            .find("struct UpdateServer")
            .expect("UpdateServer struct must be generated");
        let upd_end = upd_start
            + generated[upd_start..]
                .find('}')
                .expect("UpdateServer struct must close");
        let upd_section = &generated[upd_start..upd_end];

        // A `#[validate(ip)]` attr renders as `validate (ip)`; the pre-fix
        // combined attr on the Option field would render `validate (ip ,
        // length (min = 1))`. After the fix, the ONLY surviving `ip` validator
        // in UpdateServer is the non-Option `ip2` field, so `validate (ip`
        // must appear exactly once.
        assert_eq!(
            upd_section.matches("validate (ip").count(),
            1,
            "UpdateServer must retain `ip` on ONLY the non-Option `ip2` field \
             (the Option<String> `ip` field must drop it — no `impl ValidateIp \
             for Option<T>`): {upd_section}"
        );
        // The retained one is exactly `validate (ip)` (bare), i.e. ip2's attr.
        assert!(
            upd_section.contains("validate (ip)"),
            "UpdateServer's non-Option `ip2` field must RETAIN the `ip` \
             validator (Patch<String>: ValidateIp holds via the ToString \
             blanket): {upd_section}"
        );
        // The Option `ip` field's combined attr must NOT survive with `ip`.
        assert!(
            !upd_section.contains("validate (ip ,"),
            "UpdateServer's Option<String> `ip` field must DROP the `ip` \
             validator: {upd_section}"
        );
        // `length` on the Option fields must NOT be over-filtered (validator
        // ships an `Option<T>` impl for it): both the `ip` field's leftover
        // `length` and the `name` field's `length` remain.
        assert_eq!(
            upd_section.matches("length (min = 1)").count(),
            2,
            "UpdateServer must KEEP `length` on both Option fields (`ip` \
             leftover after dropping ip, and `name`): {upd_section}"
        );
    }

    #[test]
    fn normalize_runs_in_from_patch_update_path() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct User {
                    #[id]
                    pub id: i64,
                    #[normalize(trim, downcase)]
                    pub email: String,
                }
            },
        );
        let generated = output.to_string();
        let fp = generated
            .find("fn from_patch")
            .expect("from_patch must be generated");
        let section = &generated[fp..fp + 1200];
        assert!(
            section.contains("normalize"),
            "from_patch (update path) must normalize the draft: {section}"
        );
    }

    #[test]
    fn model_without_normalize_still_impls_normalized_model() {
        // Every model impls NormalizedModel (empty) so the generic finder call
        // compiles uniformly.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Widget {
                    #[id]
                    pub id: i64,
                    pub name: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("impl :: autumn_web :: normalize :: NormalizedModel for Widget"),
            "every model must impl NormalizedModel: {generated}"
        );
    }

    #[test]
    fn normalize_lookup_keys_on_rust_field_name_not_serde_rename() {
        // #1379 regression: the derived `#[repository]` finder passes the Rust
        // field name (the diesel column) to `normalize_lookup`, so the match
        // arms must be keyed by that Rust name — not the serde-serialized name.
        // Under a struct-level `#[serde(rename_all = "camelCase")]` a multi-word
        // column (`display_name`) serializes to `displayName`; keying the arm on
        // the serde name would make the finder's Rust-name lookup fall through to
        // `None` and silently skip normalization of the argument.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                #[serde(rename_all = "camelCase")]
                pub struct User {
                    #[id]
                    pub id: i64,
                    #[normalize(trim, downcase)]
                    pub display_name: String,
                }
            },
        );
        let generated = output.to_string();
        let fp = generated
            .find("fn normalize_lookup")
            .expect("normalize_lookup must be generated");
        let end = (fp + 600).min(generated.len());
        let section = &generated[fp..end];
        assert!(
            section.contains("\"display_name\""),
            "normalize_lookup arm must key on the Rust field name `display_name`: {section}"
        );
        assert!(
            !section.contains("\"displayName\""),
            "normalize_lookup arm must NOT key on the serde-renamed name `displayName`: {section}"
        );
    }

    #[test]
    fn lock_version_filtered_from_user_attrs() {
        let field: syn::Field = syn::parse_quote! {
            #[lock_version]
            pub version: i32
        };
        let attrs = user_attrs(&field);
        // The lock_version attribute must not leak onto the generated Diesel
        // struct — Diesel doesn't know about it and would emit a warning/error.
        assert!(attrs.is_empty());
    }

    // --- Declarative-schema markers (#1975, slice 3.5) -------------------
    // Acceptance-only: the `#[model]` macro must ACCEPT `#[model(managed)]`
    // and the `#[unique]` / `#[references(...)]` field markers, validate their
    // shapes, strip them from generated code, and change NO codegen behavior.

    #[test]
    fn model_managed_arg_accepted_by_parse_attr_args() {
        let args = parse_attr_args(quote! { managed }).expect("`managed` must parse");
        assert!(
            args.managed,
            "`#[model(managed)]` must set the managed flag"
        );
        assert!(args.table.is_none());
    }

    #[test]
    fn model_managed_and_table_accepted_together() {
        let args =
            parse_attr_args(quote! { table = "accounts", managed }).expect("both args must parse");
        assert!(args.managed);
        assert_eq!(args.table.as_deref(), Some("accounts"));
    }

    #[test]
    fn model_managed_with_value_rejected() {
        let err = parse_attr_args(quote! { managed = true })
            .expect_err("`managed = ...` must be rejected");
        assert!(
            err.to_string().contains("`managed` takes no value"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn model_unknown_arg_still_rejected() {
        let err =
            parse_attr_args(quote! { bogus }).expect_err("an unknown `#[model]` arg must error");
        assert!(
            err.to_string()
                .contains("unsupported `#[model(...)]` argument"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn empty_model_args_default() {
        let args = parse_attr_args(TokenStream::new()).expect("empty args must parse");
        assert!(!args.managed);
        assert!(args.table.is_none());
    }

    #[test]
    fn unique_and_references_filtered_from_user_attrs() {
        let field: syn::Field = syn::parse_quote! {
            #[unique]
            #[references(table = "accounts")]
            pub account_id: i64
        };
        let attrs = user_attrs(&field);
        // Neither marker may leak onto the generated Diesel query struct.
        assert!(
            attrs.is_empty(),
            "`#[unique]` / `#[references]` must be stripped: {attrs:?}",
            attrs = attrs
                .iter()
                .map(|a| quote!(#a).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn bare_unique_and_references_pass_validation() {
        let field: syn::Field = syn::parse_quote! {
            #[unique]
            #[references]
            pub account_id: i64
        };
        validate_field_schema_markers(&field).expect("bare markers must validate");

        let explicit: syn::Field = syn::parse_quote! {
            #[references(table = "accounts")]
            pub account_id: i64
        };
        validate_field_schema_markers(&explicit).expect("explicit references must validate");
    }

    #[test]
    fn unique_with_args_rejected() {
        let field: syn::Field = syn::parse_quote! {
            #[unique(x)]
            pub account_id: i64
        };
        let err =
            validate_field_schema_markers(&field).expect_err("`#[unique(...)]` must be rejected");
        assert!(
            err.to_string().contains("`#[unique]` takes no arguments"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn references_with_bad_key_rejected() {
        let field: syn::Field = syn::parse_quote! {
            #[references(bogus = "x")]
            pub account_id: i64
        };
        let err = validate_field_schema_markers(&field)
            .expect_err("`#[references(bogus = ...)]` must be rejected");
        assert!(
            err.to_string()
                .contains("unsupported `#[references(...)]` argument"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn model_managed_with_paren_args_rejected() {
        // `#[model(managed(foo))]` (parenthesized) must produce our clear
        // message, not a generic `syn` "expected identifier" parser error.
        let err =
            parse_attr_args(quote! { managed(foo) }).expect_err("`managed(...)` must be rejected");
        assert!(
            err.to_string().contains("`managed` takes no value"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn references_namevalue_rejected() {
        // `#[references = "accounts"]` (name-value) must produce our clear
        // message, not a generic `parse_nested_meta` "expected attribute list"
        // error.
        let field: syn::Field = syn::parse_quote! {
            #[references = "accounts"]
            pub account_id: i64
        };
        let err = validate_field_schema_markers(&field)
            .expect_err("`#[references = \"...\"]` must be rejected");
        assert!(
            err.to_string()
                .contains("`#[references]` must be a bare attribute"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn unique_namevalue_rejected() {
        // `#[unique = "x"]` (name-value) is already covered by the existing
        // Path-only check; assert it yields the clear message rather than a
        // generic parser error.
        let field: syn::Field = syn::parse_quote! {
            #[unique = "x"]
            pub account_id: i64
        };
        let err = validate_field_schema_markers(&field)
            .expect_err("`#[unique = \"x\"]` must be rejected");
        assert!(
            err.to_string().contains("`#[unique]` takes no arguments"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn schema_markers_do_not_change_codegen() {
        // The generated output for a model that USES the markers must be
        // identical (modulo the stripped marker attrs) to the same model
        // WITHOUT them — i.e. the markers contribute nothing to codegen.
        let with_markers = model_macro(
            quote! { managed },
            quote! {
                pub struct Membership {
                    #[id]
                    pub id: i64,
                    #[unique]
                    #[references(table = "accounts")]
                    pub account_id: i64,
                }
            },
        )
        .to_string();
        let without_markers = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Membership {
                    #[id]
                    pub id: i64,
                    pub account_id: i64,
                }
            },
        )
        .to_string();
        assert_eq!(
            with_markers, without_markers,
            "schema markers must not alter generated code"
        );
    }

    #[test]
    fn model_commit_hook_codec_includes_serde_skipped_fields() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Account {
                    #[id]
                    pub id: i64,
                    pub email: String,
                    #[serde(skip_serializing)]
                    pub password_hash: String,
                    #[serde(skip)]
                    pub reset_token: Option<String>,
                }
            },
        );
        let generated = output.to_string();

        assert!(
            generated.contains("__autumn_commit_hook_to_value")
                && generated.contains("__autumn_commit_hook_from_value"),
            "models must implement the full-fidelity commit hook codec: {generated}"
        );
        assert!(
            generated.contains("\"password_hash\""),
            "commit hook codec must serialize skip_serializing fields: {generated}"
        );
        assert!(
            generated.contains("\"reset_token\""),
            "commit hook codec must serialize skip fields instead of defaulting them: {generated}"
        );
    }

    #[test]
    fn model_commit_hook_codec_preserves_serde_adapters() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct LedgerEntry {
                    #[id]
                    pub id: i64,
                    #[serde(with = "cents_adapter")]
                    pub amount_cents: i64,
                    #[serde(
                        serialize_with = "token_adapter::serialize",
                        deserialize_with = "token_adapter::deserialize"
                    )]
                    pub external_token: String,
                }
            },
        );
        let generated = output.to_string();

        assert!(
            generated.contains("__AutumnCommitHookSerializeField"),
            "commit hook codec must serialize adapted fields through serde field helpers: {generated}"
        );
        assert!(
            generated.contains("__AutumnCommitHookDeserializeField"),
            "commit hook codec must deserialize adapted fields through serde field helpers: {generated}"
        );
        assert!(
            generated.contains("with = \"cents_adapter\""),
            "commit hook codec must preserve serde with adapters: {generated}"
        );
        assert!(
            generated.contains("serialize_with = \"token_adapter::serialize\""),
            "commit hook codec must preserve serialize_with adapters: {generated}"
        );
        assert!(
            generated.contains("deserialize_with = \"token_adapter::deserialize\""),
            "commit hook codec must preserve deserialize_with adapters: {generated}"
        );
    }

    // ── Existing tests ────────────────────────────────────────────────────

    #[test]
    fn model_commit_hook_codec_defaults_missing_compatible_fields() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Account {
                    #[id]
                    pub id: i64,
                    pub reset_token: Option<String>,
                    #[serde(default = "default_reset_token")]
                    pub special_token: Option<String>,
                    #[serde(default)]
                    pub display_name: String,
                    #[serde(default = "default_status")]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();

        assert!(
            generated.contains(":: core :: option :: Option :: None"),
            "missing Option fields in old durable payloads should default to None: {generated}"
        );
        assert!(
            generated.contains(":: core :: default :: Default :: default ()"),
            "missing #[serde(default)] fields in old durable payloads should use Default::default(): {generated}"
        );
        assert!(
            generated.contains("default_status ()"),
            "missing #[serde(default = \"...\")] fields in old durable payloads should call the configured default function: {generated}"
        );
        assert!(
            generated.contains("default_reset_token ()"),
            "explicit serde defaults should beat the generic Option::None fallback: {generated}"
        );
    }

    #[test]
    fn pascal_to_snake_simple() {
        assert_eq!(pascal_to_snake("User"), "user");
    }

    #[test]
    fn pascal_to_snake_multi_word() {
        assert_eq!(pascal_to_snake("BlogPost"), "blog_post");
    }

    #[test]
    fn pascal_to_snake_three_words() {
        assert_eq!(
            pascal_to_snake("UserProfileSettings"),
            "user_profile_settings"
        );
    }

    #[test]
    fn pascal_case_simple() {
        assert_eq!(pascal_case("title"), "Title");
    }

    #[test]
    fn pascal_case_multi_word() {
        assert_eq!(pascal_case("approved_at"), "ApprovedAt");
    }

    #[test]
    fn pascal_case_single_char() {
        assert_eq!(pascal_case("x"), "X");
    }

    #[test]
    fn infer_table_name_simple() {
        let ident = syn::Ident::new("User", proc_macro2::Span::call_site());
        assert_eq!(infer_table_name(&ident), "users");
    }

    #[test]
    fn infer_table_name_multi_word() {
        let ident = syn::Ident::new("BlogPost", proc_macro2::Span::call_site());
        assert_eq!(infer_table_name(&ident), "blog_posts");
    }

    // Irregular-plural inference (#1753): the derived table name MUST match the
    // CLI scaffold's `src/schema.rs`, which pluralises through
    // `autumn_web::format::pluralize_word`. These mirror the assertions in
    // `autumn/src/format.rs` and `autumn-cli/src/generate/naming.rs` so all
    // three implementations agree.
    #[test]
    fn infer_table_name_irregular_plurals() {
        let cases = [
            ("Category", "categories"),
            ("Company", "companies"),
            ("City", "cities"),
            ("Story", "stories"),
            ("Box", "boxes"),
            ("Buzz", "buzzes"),
            ("Class", "classes"),
            ("Watch", "watches"),
            ("Dish", "dishes"),
            ("Person", "people"),
            ("Child", "children"),
            ("Post", "posts"),
            ("Node", "nodes"),
            ("Comment", "comments"),
            ("BlogPost", "blog_posts"),
            ("Day", "days"),
        ];
        for (input, expected) in cases {
            let ident = syn::Ident::new(input, proc_macro2::Span::call_site());
            assert_eq!(infer_table_name(&ident), expected, "input: {input}");
        }
    }

    #[test]
    fn pluralize_word_matches_canonical_rules() {
        assert_eq!(pluralize_word(""), "");
        assert_eq!(pluralize_word("category"), "categories");
        assert_eq!(pluralize_word("day"), "days");
        assert_eq!(pluralize_word("box"), "boxes");
        assert_eq!(pluralize_word("buzz"), "buzzes");
        assert_eq!(pluralize_word("class"), "classes");
        assert_eq!(pluralize_word("watch"), "watches");
        assert_eq!(pluralize_word("dish"), "dishes");
        assert_eq!(pluralize_word("person"), "people");
        assert_eq!(pluralize_word("goose"), "geese");
        assert_eq!(pluralize_word("post"), "posts");
    }

    // ── RED: etag() derivation from #[lock_version] ────────────────────────

    #[test]
    fn lock_version_model_emits_etag_method() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    #[lock_version]
                    pub lock_version: i64,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("pub fn etag"),
            "model with #[lock_version] must emit `pub fn etag`: {generated}"
        );
    }

    #[test]
    fn model_without_lock_version_does_not_emit_etag_method() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            !generated.contains("pub fn etag"),
            "model without #[lock_version] must NOT emit `pub fn etag`: {generated}"
        );
    }

    #[test]
    fn etag_method_calls_into_etag_on_lock_version_field() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    #[lock_version]
                    pub lock_version: i64,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("IntoETag") || generated.contains("into_etag"),
            "etag() must call IntoETag::into_etag on the lock_version field: {generated}"
        );
        assert!(
            generated.contains("lock_version"),
            "etag() method body must reference the lock_version field: {generated}"
        );
    }

    // ── RED: declarative state machines ───────────────────────────────────────
    // These tests define the expected generated API for `#[state_machine(...)]`
    // field attributes. All will fail until the feature is implemented.

    #[test]
    fn state_machine_emits_can_transition_method() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("can_transition_status_to"),
            "#[state_machine] must emit `can_transition_status_to`: {generated}"
        );
    }

    #[test]
    fn state_machine_emits_transition_to_method() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("transition_status_to"),
            "#[state_machine] must emit `transition_status_to`: {generated}"
        );
    }

    #[test]
    fn state_machine_emits_transitions_constant() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("__AUTUMN_SM_STATUS_TRANSITIONS"),
            "#[state_machine] must emit `__AUTUMN_SM_STATUS_TRANSITIONS` constant: {generated}"
        );
    }

    #[test]
    fn state_machine_transition_table_contains_from_to_pairs() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("\"pending\"") && generated.contains("\"processing\""),
            "transition table must contain the from/to state strings: {generated}"
        );
        assert!(
            generated.contains("\"shipped\""),
            "transition table must contain all destination states: {generated}"
        );
    }

    #[test]
    fn list_helpers_allowlist_is_typed_and_injection_safe() {
        // #1126 AC5: the generated sort/filter DSL is an allowlist of the
        // model's OWN columns, matched with typed Diesel expressions. An
        // attacker-supplied `sort=id;DROP TABLE users` has no match arm, so it
        // can only ever hit the default `_ =>` arm (order by the primary key).
        // Assert this STRUCTURALLY — there is no code path that interpolates a
        // request-supplied column name into SQL.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Post {
                    #[id]
                    pub id: i64,
                    pub title: String,
                    pub views: i64,
                    pub published: bool,
                }
            },
        );
        let generated = output.to_string();

        // The order helper matches on `query.sort()` with a typed column arm
        // per real column, plus a primary-key default.
        assert!(
            generated.contains("fn __autumn_list_apply_order"),
            "model must generate the ordering helper: {generated}"
        );
        assert!(
            generated.contains("Some (\"title\")")
                && generated.contains("Some (\"views\")")
                && generated.contains("Some (\"published\")"),
            "each real column must be an allowlisted sort arm: {generated}"
        );
        // The default arm orders by the primary key — this is where every
        // unknown/malicious `sort` lands.
        assert!(
            generated.contains("__q . order (posts :: id . desc ())"),
            "unknown sort must fall back to the primary-key default order: {generated}"
        );

        // The filter helper matches on the column key with typed `.eq(..)` on
        // real columns only (non-null String/integer/bool).
        assert!(
            generated.contains("fn __autumn_list_apply_filters"),
            "model must generate the filter helper: {generated}"
        );
        assert!(
            generated.contains("posts :: title . eq")
                && generated.contains("posts :: views . eq")
                && generated.contains("posts :: published . eq"),
            "filters must be typed column equality on allowlisted columns: {generated}"
        );

        // Injection safety, asserted structurally: NO request-derived string is
        // ever turned into SQL. There is no raw `sql_query`, no `ORDER BY`
        // string, and no `format!` building a clause. The only way a column
        // reaches SQL is through the compile-time-checked `posts :: <col>` paths
        // above, so `id;DROP TABLE users` (which is not a column) is inert.
        let order_start = generated
            .find("fn __autumn_list_apply_order")
            .expect("order helper present");
        let filter_start = generated
            .find("fn __autumn_list_apply_filters")
            .expect("filter helper present");
        let helpers_region = {
            let lo = order_start.min(filter_start);
            // Grab a generous window covering both helper bodies.
            &generated[lo..(lo + 4000).min(generated.len())]
        };
        assert!(
            !helpers_region.contains("sql_query") && !helpers_region.contains("ORDER BY"),
            "sort/filter must never build raw SQL from request input: {helpers_region}"
        );
        assert!(
            !helpers_region.contains("format !"),
            "sort/filter must never string-format a column into a query: {helpers_region}"
        );
    }

    #[test]
    fn state_machine_with_guard_calls_guard_method() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: "can_ship",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("can_ship"),
            "guarded transition must call the guard method `can_ship`: {generated}"
        );
    }

    #[test]
    fn state_machine_guard_stored_in_transition_table() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: "can_ship",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("Some (\"can_ship\")") || generated.contains("Some(\"can_ship\")"),
            "guarded transition must store the guard name in the transition table: {generated}"
        );
    }

    #[test]
    fn state_machine_unguarded_transition_table_entry_has_none() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("None"),
            "unguarded transition must store None in the transition table: {generated}"
        );
    }

    #[test]
    fn state_machine_transition_method_returns_autumn_result() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("AutumnResult"),
            "transition_*_to must return AutumnResult: {generated}"
        );
    }

    #[test]
    fn state_machine_attribute_not_leaked_to_diesel_struct() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // The `state_machine` attribute must not appear inside the Diesel struct
        // definition — Diesel doesn't know about it and would emit errors.
        // We check that it does NOT appear as a field-level #[state_machine].
        // The generated constant/methods may contain the word though.
        let struct_block = generated
            .find("pub struct Order")
            .map_or("", |i| &generated[i..i + 500]);
        assert!(
            !struct_block.contains("# [state_machine]")
                && !struct_block.contains("#[state_machine]"),
            "`state_machine` attribute must not appear on the generated Diesel struct field: {struct_block}"
        );
    }

    #[test]
    fn state_machine_on_non_string_field_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(pending -> processing))]
                    pub amount: i64,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("only supported on `String` fields"),
            "#[state_machine] on a non-String field must emit a compile error: {generated}"
        );
    }

    #[test]
    fn state_machine_duplicate_attribute_on_same_field_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(pending -> processing))]
                    #[state_machine(transitions(processing -> shipped))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("multiple `#[state_machine]` attributes are not allowed"),
            "duplicate #[state_machine] on same field must emit a compile error: {generated}"
        );
    }

    #[test]
    fn state_machine_invalid_guard_identifier_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing: "can-ship",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("not a valid Rust identifier"),
            "invalid guard identifier must emit a compile error: {generated}"
        );
    }

    #[test]
    fn state_machine_raw_identifier_field_generates_clean_names() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                    ))]
                    pub r#type: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("can_transition_type_to"),
            "raw identifier field must strip r# prefix for generated method name: {generated}"
        );
        assert!(
            generated.contains("__AUTUMN_SM_TYPE_TRANSITIONS"),
            "raw identifier field must strip r# prefix for generated const name: {generated}"
        );
    }

    // ── on_commit transition effects (#1973) ──────────────────────────────────

    #[test]
    fn state_machine_without_on_commit_does_not_emit_on_conn_method() {
        // A machine with no `on_commit` edge is byte-for-byte unchanged: no
        // `_on_conn` method and no `TransitionEffect`/`enqueue_on_conn` code.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped: "can_ship",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            !generated.contains("transition_status_to_on_conn"),
            "no `on_commit` edge must not emit the `_on_conn` method: {generated}"
        );
        assert!(
            !generated.contains("TransitionEffect") && !generated.contains("enqueue_on_conn"),
            "no `on_commit` edge must not emit any effect codegen: {generated}"
        );
    }

    #[test]
    fn state_machine_on_commit_emits_on_conn_method() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped: on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("transition_status_to_on_conn"),
            "an `on_commit` edge must emit the connection-taking method: {generated}"
        );
        assert!(
            generated.contains("AsyncPgConnection"),
            "the `_on_conn` method must take a connection: {generated}"
        );
        // The job is enqueued transactionally by its registered NAME.
        assert!(
            generated.contains("enqueue_on_conn")
                && generated.contains("< SendShippedEmailJob > :: NAME"),
            "the effect must enqueue the named job on the connection: {generated}"
        );
    }

    #[test]
    fn state_machine_on_commit_enqueues_only_on_the_named_edge() {
        // Only `processing -> shipped` carries an effect; `pending -> processing`
        // must not enqueue anything. The effect arm keys on both from/to states.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped: on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // Exactly one enqueue call is generated (the single effect edge).
        assert_eq!(
            generated.matches("enqueue_on_conn").count(),
            1,
            "exactly the one `on_commit` edge must enqueue: {generated}"
        );
        // The effect arm dispatches on the fired edge's from/to pair.
        assert!(
            generated.contains("(\"processing\" , \"shipped\")"),
            "the effect arm must match the fired edge: {generated}"
        );
    }

    #[test]
    fn state_machine_on_commit_derives_idempotency_key_from_edge_context() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // The idempotency key is model:field:record_id:from:to and is carried on
        // the TransitionEffect payload so a `unique, by = ["idempotency_key"]`
        // job coalesces a retried transition.
        assert!(
            generated.contains("idempotency_key"),
            "the effect must carry a derived idempotency key: {generated}"
        );
        assert!(
            generated.contains("\"{}:{}:{}:{}:{}\""),
            "the idempotency key must combine model/field/record_id/from/to: {generated}"
        );
        assert!(
            generated.contains("\"Order\"") && generated.contains("\"status\""),
            "the key must embed the model and field names: {generated}"
        );
        // The record id is derived from the primary-key field.
        assert!(
            generated.contains("self . id"),
            "the record id must come from the primary key: {generated}"
        );
    }

    #[test]
    fn state_machine_guard_and_on_commit_compose_on_one_edge() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Article {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        draft -> published,
                        published -> archived: guard = "can_archive", on_commit = AnnounceArchiveJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // The guard is still stored in the const table and dispatched in can_*.
        assert!(
            generated.contains("Some (\"can_archive\")"),
            "guard must remain in the transition table when composed with on_commit: {generated}"
        );
        assert!(
            generated.contains("self . can_archive ()"),
            "guarded+effect edge must still call the guard in `can_transition_*`: {generated}"
        );
        // The effect is emitted for the same edge.
        assert!(
            generated.contains("transition_status_to_on_conn")
                && generated.contains("< AnnounceArchiveJob > :: NAME"),
            "the composed edge must also enqueue its on_commit job: {generated}"
        );
    }

    #[test]
    fn state_machine_on_commit_unknown_meta_key_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: bogus = "x",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("compile_error") && generated.contains("on_commit = <Job>"),
            "an unknown per-edge meta key must emit a compile error: {generated}"
        );
        // The refreshed unknown-key message now also advertises `on = "..."`.
        assert!(
            generated.contains("`on = "),
            "the unknown per-edge meta key error must mention `on = \"...\"`: {generated}"
        );
    }

    // ── sync `on = "handler"` transition effects (#1973) ──────────────────────

    #[test]
    fn state_machine_on_sync_effect_emits_on_conn_method() {
        // An edge with a sync `on = "handler"` emits the connection-taking
        // method whose fired-edge arm calls `self.<handler>(conn).await?`.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        pending -> processing,
                        processing -> shipped: on = "record_audit",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("transition_status_to_on_conn"),
            "an `on` edge must emit the connection-taking method: {generated}"
        );
        assert!(
            generated.contains("AsyncPgConnection"),
            "the `_on_conn` method must take a connection: {generated}"
        );
        // The sync effect calls the named `&self` handler with the connection.
        assert!(
            generated.contains("self . record_audit (conn) . await ?"),
            "the `on` edge must call its handler in-transaction: {generated}"
        );
    }

    #[test]
    fn state_machine_on_sync_effect_only_does_not_enqueue() {
        // An `on`-only machine (no `on_commit` anywhere) emits the method but no
        // after-commit enqueue/`TransitionEffect` codegen.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: on = "record_audit",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("transition_status_to_on_conn"),
            "an `on` edge must still emit the connection-taking method: {generated}"
        );
        assert!(
            !generated.contains("enqueue_on_conn") && !generated.contains("TransitionEffect"),
            "an `on`-only machine must not emit after-commit effect codegen: {generated}"
        );
    }

    #[test]
    fn state_machine_on_and_on_commit_compose_on_one_edge() {
        // A single edge with both `on` and `on_commit` emits both effects, with
        // the sync handler call ordered BEFORE the after-commit enqueue.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: on = "record_audit", on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        let sync_at = generated
            .find("self . record_audit (conn) . await ?")
            .expect("sync handler call must be emitted");
        let commit_at = generated
            .find("enqueue_on_conn")
            .expect("on_commit enqueue must be emitted");
        assert!(
            sync_at < commit_at,
            "the sync `on` handler must run before the `on_commit` enqueue: {generated}"
        );
        assert!(
            generated.contains("< SendShippedEmailJob > :: NAME"),
            "the composed edge must enqueue its on_commit job: {generated}"
        );
    }

    #[test]
    fn state_machine_guard_and_on_and_on_commit_compose() {
        // Guard + sync `on` + `on_commit` on one edge: the guard stays in the
        // const table and `can_*` dispatch; both effects emit.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Article {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        draft -> published,
                        published -> archived: guard = "can_archive", on = "audit", on_commit = AnnounceArchiveJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // Guard remains in the transition table and `can_*` dispatch.
        assert!(
            generated.contains("Some (\"can_archive\")"),
            "guard must remain in the transition table when composed: {generated}"
        );
        assert!(
            generated.contains("self . can_archive ()"),
            "guarded edge must still call the guard in `can_transition_*`: {generated}"
        );
        // Both effects emit on the composed edge.
        assert!(
            generated.contains("self . audit (conn) . await ?"),
            "the composed edge must run its sync `on` handler: {generated}"
        );
        assert!(
            generated.contains("< AnnounceArchiveJob > :: NAME"),
            "the composed edge must enqueue its on_commit job: {generated}"
        );
    }

    #[test]
    fn state_machine_duplicate_on_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(
                        processing -> shipped: on = "a", on = "b",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("compile_error") && generated.contains("duplicate `on`"),
            "a duplicate `on` on one edge must emit a compile error: {generated}"
        );
    }

    // ── #[state_machine(lifecycle = Enum)] derivation (#1911) ─────────────────

    #[test]
    fn state_machine_lifecycle_const_aliases_trait_table() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState)]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // The transitions const is an alias of the enum's Lifecycle trait const,
        // so the table is defined once on the enum and stays typed.
        assert!(
            generated.contains("__AUTUMN_SM_STATUS_TRANSITIONS"),
            "lifecycle SM must emit the transitions const: {generated}"
        );
        assert!(
            generated.contains(
                "< OrderState as :: autumn_web :: Lifecycle > :: STATE_MACHINE_TRANSITIONS"
            ),
            "lifecycle SM const must alias the enum's Lifecycle trait table: {generated}"
        );
        // No inline string edges are baked into the model — the source of truth
        // is the enum.
        assert!(
            !generated.contains("(\"pending\""),
            "lifecycle SM must not inline literal edge strings: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_emits_predicate_and_transition_methods() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState)]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("can_transition_status_to"),
            "lifecycle SM must emit `can_transition_status_to`: {generated}"
        );
        assert!(
            generated.contains("transition_status_to"),
            "lifecycle SM must emit `transition_status_to`: {generated}"
        );
        assert!(
            generated.contains("AutumnResult"),
            "lifecycle SM `transition_*_to` must return AutumnResult: {generated}"
        );
        // The predicate iterates the derived table (no literal match arms are
        // possible — the edge strings live on the enum).
        assert!(
            generated.contains(". iter ()") && generated.contains(". any"),
            "lifecycle SM predicate must iterate the derived table: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_accepts_path_reference() {
        // A module-qualified path to the lifecycle enum is accepted.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = crate::states::OrderState)]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains(
                "< crate :: states :: OrderState as :: autumn_web :: Lifecycle > :: STATE_MACHINE_TRANSITIONS"
            ),
            "lifecycle SM must accept a qualified path to the enum: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_on_non_string_field_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState)]
                    pub amount: i64,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("only supported on `String` fields"),
            "lifecycle SM on a non-String field must be rejected: {generated}"
        );
    }

    // ── lifecycle `effects(...)` per-edge effects (#1973) ─────────────────────

    #[test]
    fn state_machine_lifecycle_with_effects_emits_on_conn_method() {
        // An `effects(...)` clause routes the lifecycle path through the shared
        // `_on_conn` emitter: the connection-taking method is emitted, keyed on
        // the fired edge, and enqueues the named job.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        processing -> shipped: on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("transition_status_to_on_conn"),
            "a lifecycle `effects` edge must emit the connection-taking method: {generated}"
        );
        // The const still aliases the enum's trait table (transitions come from
        // the enum, effects come from the binding site).
        assert!(
            generated.contains(
                "< OrderState as :: autumn_web :: Lifecycle > :: STATE_MACHINE_TRANSITIONS"
            ),
            "lifecycle `effects` must not replace the enum-derived transition table: {generated}"
        );
        assert!(
            generated.contains("enqueue_on_conn")
                && generated.contains("< SendShippedEmailJob > :: NAME"),
            "the effect must enqueue the named job on the connection: {generated}"
        );
        assert!(
            generated.contains("(\"processing\" , \"shipped\")"),
            "the effect arm must match the fired edge: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_without_effects_emits_no_on_conn_method() {
        // A plain `lifecycle = <Enum>` (no `effects(...)`) is byte-for-byte
        // unchanged: no `_on_conn` method and no after-commit effect codegen.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState)]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            !generated.contains("transition_status_to_on_conn"),
            "a lifecycle SM with no `effects` must not emit the `_on_conn` method: {generated}"
        );
        assert!(
            !generated.contains("enqueue_on_conn"),
            "a lifecycle SM with no `effects` must not emit after-commit effect codegen: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_effects_sync_on_emits_handler_call() {
        // An `on = "handler"` effect edge emits the in-transaction handler call
        // and, being `on`-only, no after-commit enqueue.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        shipped -> archived: on = "audit",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("transition_status_to_on_conn"),
            "a lifecycle `on` effect edge must emit the connection-taking method: {generated}"
        );
        assert!(
            generated.contains("self . audit (conn) . await ?"),
            "the `on` effect must call its handler in-transaction: {generated}"
        );
        assert!(
            !generated.contains("enqueue_on_conn"),
            "an `on`-only lifecycle effect must not emit after-commit codegen: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_effects_guard_is_rejected() {
        // Guards are meaningless on lifecycle edges (the runtime table validator
        // ignores them), so declaring one on an `effects(...)` edge is an error.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        processing -> shipped: guard = "can_ship",
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("compile_error"),
            "a guard on a lifecycle `effects` edge must be rejected: {generated}"
        );
        assert!(
            generated.contains("guards are not supported"),
            "the rejection must mention guards not being supported: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_effects_empty_edge_is_rejected() {
        // An `effects(...)` edge with neither `on` nor `on_commit` is pointless
        // and rejected — declare it in the enum's transition table instead.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        processing -> shipped,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("compile_error"),
            "an effect-less `effects` edge must be rejected: {generated}"
        );
        assert!(
            generated.contains("must declare `on"),
            "the rejection must require an `on`/`on_commit` effect: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_effects_derive_idempotency_key() {
        // The lifecycle path reuses the identical idempotency-key derivation as
        // the inline path: model:field:record_id:from:to.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        processing -> shipped: on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("idempotency_key"),
            "the effect must carry a derived idempotency key: {generated}"
        );
        assert!(
            generated.contains("\"{}:{}:{}:{}:{}\""),
            "the idempotency key must combine model/field/record_id/from/to: {generated}"
        );
        assert!(
            generated.contains("\"Order\"") && generated.contains("\"status\""),
            "the key must embed the model and field names: {generated}"
        );
        assert!(
            generated.contains("self . id"),
            "the record id must come from the primary key: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_effects_emit_edge_validation_asserts() {
        // Issue #1973: on the lifecycle path an `effects(...)` edge that is not a
        // real transition of the referenced enum would otherwise compile with a
        // silently-unreachable match arm and drop the effect. The codegen emits a
        // per-edge compile-time const assertion against the enum's
        // `STATE_MACHINE_TRANSITIONS` table to reject that at compile time.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        processing -> shipped: on_commit = SendShippedEmailJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("__transition_edge_declared"),
            "each effect edge must be validated against the lifecycle table: {generated}"
        );
        assert!(
            generated.contains("const _ : () ="),
            "the edge validation must be a compile-time const assertion: {generated}"
        );
        assert!(
            generated.contains("STATE_MACHINE_TRANSITIONS"),
            "the assertion must check the referenced enum's transition table: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_without_effects_emits_no_edge_validation() {
        // A plain `lifecycle = <Enum>` (no `effects(...)`) must be byte-identical
        // to before: no per-edge validation assertions are emitted.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState)]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            !generated.contains("__transition_edge_declared"),
            "a no-effects lifecycle model must emit no edge validation: {generated}"
        );
    }

    #[test]
    fn state_machine_lifecycle_effects_normalize_raw_identifier_edges() {
        // Codex P2 on #2027: a lifecycle effect edge naming a raw-identifier
        // variant (e.g. `ready -> r#type`) must be normalized to the enum's
        // already-stripped `STATE_MACHINE_TRANSITIONS` entry (`"type"`) — the
        // `#[lifecycle]` macro strips `r#` when building that table, but the
        // effect edge parsed here preserves it. Both the compile-time edge
        // assertion and the runtime effect match arm must use the stripped name
        // so a real edge is not falsely rejected and the effect is not silently
        // dropped.
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(lifecycle = OrderState, effects(
                        ready -> r#type: on_commit = SendTypedJob,
                    ))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        // The stripped name is what the enum table stores and what the codegen
        // (const assertion, match arm, idempotency key) must emit.
        assert!(
            generated.contains("\"type\""),
            "normalized effect edge must use the stripped `type` state: {generated}"
        );
        // The raw-identifier form must never survive into an assertion / match
        // literal — that is exactly the mismatch the bug produced.
        assert!(
            !generated.contains("\"r#type\""),
            "the raw `r#type` form must be stripped before codegen: {generated}"
        );
        // The per-edge validation is still emitted (now against the aligned name).
        assert!(
            generated.contains("__transition_edge_declared"),
            "the normalized edge must still be validated at compile time: {generated}"
        );
    }

    #[test]
    fn state_machine_unknown_argument_is_rejected() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Order {
                    #[id]
                    pub id: i64,
                    #[state_machine(bogus(pending -> processing))]
                    pub status: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("expected `transitions(...)` or `lifecycle = <Enum>`"),
            "an unknown #[state_machine] argument must be rejected: {generated}"
        );
    }

    #[test]
    fn state_machine_multiple_fields_emit_separate_methods() {
        let output = model_macro(
            TokenStream::new(),
            quote! {
                pub struct Ticket {
                    #[id]
                    pub id: i64,
                    #[state_machine(transitions(open -> in_progress, in_progress -> closed))]
                    pub status: String,
                    #[state_machine(transitions(low -> medium, medium -> high))]
                    pub priority: String,
                }
            },
        );
        let generated = output.to_string();
        assert!(
            generated.contains("can_transition_status_to"),
            "multi-sm model must emit `can_transition_status_to`: {generated}"
        );
        assert!(
            generated.contains("can_transition_priority_to"),
            "multi-sm model must emit `can_transition_priority_to`: {generated}"
        );
        assert!(
            generated.contains("transition_status_to"),
            "multi-sm model must emit `transition_status_to`: {generated}"
        );
        assert!(
            generated.contains("transition_priority_to"),
            "multi-sm model must emit `transition_priority_to`: {generated}"
        );
    }

    // ── Datetime control/deserializer selection per zone parameter (#1135) ─

    /// Regression test (Codex P2 on #1587): only `DateTime<Utc>` and
    /// `DateTime<Local>` — the zones whose offsetless `datetime-local`
    /// submission has a matching tolerant deserializer — may render the
    /// datetime picker. Any other zone parameter (`FixedOffset`, chrono-tz
    /// zones, a bare un-parameterized `DateTime` alias) must fall back to a
    /// text control whose RFC 3339 value round-trips through chrono's
    /// default `Deserialize`; giving them the picker would 400 every
    /// submission.
    #[test]
    fn form_control_tokens_gates_datetime_picker_on_zone_param() {
        let control = |ty: syn::Type| form_control_tokens(&ty, false).to_string();

        let utc = control(syn::parse_quote!(chrono::DateTime<chrono::Utc>));
        assert!(utc.contains("FieldControl :: DateTime"), "{utc}");
        let local = control(syn::parse_quote!(chrono::DateTime<chrono::Local>));
        assert!(local.contains("FieldControl :: DateTime"), "{local}");

        let fixed = control(syn::parse_quote!(chrono::DateTime<chrono::FixedOffset>));
        assert!(fixed.contains("FieldControl :: Text"), "{fixed}");
        let tz = control(syn::parse_quote!(chrono::DateTime<chrono_tz::Tz>));
        assert!(tz.contains("FieldControl :: Text"), "{tz}");
        // A bare alias hides the zone from the derive — no picker either.
        let bare = control(syn::parse_quote!(DateTime));
        assert!(bare.contains("FieldControl :: Text"), "{bare}");
    }

    /// The deserializer wiring must stay in lockstep with the control choice
    /// above: `Utc`/`Local` get their zone-matching tolerant deserializer
    /// (`_option` for nullable), everything else keeps chrono's default
    /// `Deserialize` (fine — those columns render as text, whose RFC 3339
    /// value the default parses).
    #[test]
    fn datetime_local_serde_attr_matches_zone_param() {
        let attr = |ty: syn::Type| datetime_local_serde_attr(&ty).map(|t| t.to_string());

        let utc = attr(syn::parse_quote!(chrono::DateTime<chrono::Utc>)).unwrap();
        assert!(utc.contains("deserialize_datetime_local_utc"), "{utc}");

        let local = attr(syn::parse_quote!(chrono::DateTime<chrono::Local>)).unwrap();
        assert!(
            local.contains("deserialize_datetime_local_local"),
            "{local}"
        );
        assert!(!local.contains("_option"), "{local}");

        let nullable = attr(syn::parse_quote!(Option<chrono::DateTime<chrono::Local>>)).unwrap();
        assert!(
            nullable.contains("deserialize_datetime_local_local_option"),
            "{nullable}"
        );
        assert!(nullable.contains("default"), "{nullable}");

        assert_eq!(
            attr(syn::parse_quote!(chrono::DateTime<chrono::FixedOffset>)),
            None
        );
        assert_eq!(attr(syn::parse_quote!(DateTime)), None);
    }
}
