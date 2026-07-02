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
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{DeriveInput, Field, LitStr};

/// Process `#[model]` or `#[model(table = "...")]` attribute arguments.
fn parse_attr_args(attr: TokenStream) -> syn::Result<Option<String>> {
    if attr.is_empty() {
        return Ok(None);
    }

    let mut table = None;
    syn::meta::parser(|meta| {
        if meta.path.is_ident("table") {
            let value: LitStr = meta.value()?.parse()?;
            table = Some(value.value());
            Ok(())
        } else {
            Err(meta.error("unsupported model attribute"))
        }
    })
    .parse2(attr)?;

    Ok(table)
}

/// Check if a field has the `#[id]` attribute.
fn has_attr(field: &Field, name: &str) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident(name))
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
}

/// The join-table half of a many-to-many `has_many(..., through = ...)`
/// association.
struct ThroughSpec {
    /// The join table name, e.g. `post_tags`.
    table: String,
    /// The join table's column pointing at the target model, e.g. `tag_id`.
    target_fk: String,
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
            let name = format!("{snake_target}s");
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
fn parse_assoc_attr(
    attr: &syn::Attribute,
    kind: AssocKind,
    model_ident: &syn::Ident,
) -> syn::Result<Association> {
    use syn::parse::ParseStream;

    let (target, explicit_fk, explicit_name, explicit_through, explicit_target_fk) = attr
        .parse_args_with(|input: ParseStream| {
            let target: syn::Ident = input.parse()?;
            let mut explicit_fk: Option<String> = None;
            let mut explicit_name: Option<String> = None;
            let mut explicit_through: Option<String> = None;
            let mut explicit_target_fk: Option<String> = None;
            // Zero or more trailing `, key = value` pairs (`fk`, `name`,
            // `through`, `target_fk`), any order.
            while input.peek(syn::Token![,]) {
                input.parse::<syn::Token![,]>()?;
                let key: syn::Ident = input.parse()?;
                input.parse::<syn::Token![=]>()?;
                // Accept either a bare identifier (`fk = author_id`) or a string
                // literal (`fk = "author_id"`).
                let value = if input.peek(LitStr) {
                    input.parse::<LitStr>()?.value()
                } else {
                    input.parse::<syn::Ident>()?.to_string()
                };
                if key == "fk" {
                    explicit_fk = Some(value);
                } else if key == "name" {
                    explicit_name = Some(value);
                } else if key == "through" {
                    explicit_through = Some(value);
                } else if key == "target_fk" {
                    explicit_target_fk = Some(value);
                } else {
                    return Err(syn::Error::new_spanned(
                        &key,
                        "expected `fk = <column>`, `name = <accessor>`, \
                         `through = <join_table>`, or `target_fk = <column>` \
                         in association attribute",
                    ));
                }
            }
            Ok((
                target,
                explicit_fk,
                explicit_name,
                explicit_through,
                explicit_target_fk,
            ))
        })?;

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
    })
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
    Ok(out)
}

/// The singular form used to derive a many-to-many association's mutation
/// helper names (`add_{singular}`, `remove_{singular}`), mirroring the
/// codebase's naive `{target}s` pluralization in reverse: strip a trailing
/// `s` if present.
fn m2m_mutation_singular(assoc_name: &str) -> &str {
    assoc_name.strip_suffix('s').unwrap_or(assoc_name)
}

/// Reject a model whose many-to-many associations would generate colliding
/// `add_*`/`remove_*`/`set_*` mutation helper names (e.g. two `through =`
/// associations that both derive `add_tag`), rather than emitting a trait
/// with duplicate method definitions.
fn check_m2m_mutation_name_collisions(assocs: &[Association]) -> syn::Result<()> {
    let mut seen: std::collections::HashMap<&str, &syn::Ident> = std::collections::HashMap::new();
    for assoc in assocs {
        if assoc.through.is_none() {
            continue;
        }
        let singular = m2m_mutation_singular(&assoc.name);
        if let Some(_prev) = seen.insert(singular, &assoc.target) {
            return Err(syn::Error::new_spanned(
                &assoc.target,
                format!(
                    "many-to-many association `{}` would generate a mutation \
                     helper `add_{singular}`/`remove_{singular}` that collides \
                     with another `through =` association on this model; \
                     disambiguate with `name = ...`",
                    assoc.name
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
                            .select(<#target as ::autumn_web::reexports::diesel::SelectableHelper<::autumn_web::reexports::diesel::pg::Pg>>::as_select())
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
                            .select(<#target as ::autumn_web::reexports::diesel::SelectableHelper<::autumn_web::reexports::diesel::pg::Pg>>::as_select())
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
                            __groups.entry(__child.#fk_ident).or_default().push(__child);
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
                        &[::std::sync::Arc<::autumn_web::preload::Preloaded<#target>>],
                        ::autumn_web::preload::NotLoaded,
                    >;
                });
                accessor_impls.push(quote! {
                    fn #name_ident(&self) -> ::core::result::Result<
                        &[::std::sync::Arc<::autumn_web::preload::Preloaded<#target>>],
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
                {
                    // Many-to-many: the fk lives on a join table, not on the
                    // target. Emit a hidden module declaring the join table
                    // (so this and the target model can both be `through =`
                    // the same physical table without colliding), then a
                    // single batched `INNER JOIN` loader keyed on the join
                    // table's own two columns.
                    let model_snake = pascal_to_snake(&model_ident.to_string());
                    let join_mod_ident = format_ident!("__autumn_m2m_{model_snake}_{}", assoc.name);
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
                                        <#target as ::autumn_web::reexports::diesel::SelectableHelper<::autumn_web::reexports::diesel::pg::Pg>>::as_select(),
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
                            let mut __kept_pairs: ::std::vec::Vec<(i64, #target)> = ::std::vec::Vec::new();
                            for (__fk, __row) in __pairs {
                                if #target::__autumn_preload_keep(&__row)? {
                                    __kept_pairs.push((__fk, __row));
                                }
                            }
                            // The same target row can appear once per linking
                            // parent (that's the point of many-to-many), so
                            // recursing into nested associations must run on a
                            // *deduplicated* set of targets, not once per join
                            // row — otherwise two parents sharing a target
                            // would each get their own independent (and only
                            // one of them fully grouped) copy of its nested
                            // associations. Dedup by id, recurse once, then
                            // share the single recursed record across every
                            // parent via `Arc`.
                            let mut __unique_by_id: ::std::collections::HashMap<i64, #target> =
                                ::std::collections::HashMap::new();
                            for (_, __row) in &__kept_pairs {
                                __unique_by_id.entry(__row.id).or_insert_with(|| __row.clone());
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
                            for (__fk, __row) in &__kept_pairs {
                                if let ::core::option::Option::Some(__arc) = __arc_by_id.get(&__row.id) {
                                    __groups.entry(*__fk).or_default().push(::std::sync::Arc::clone(__arc));
                                }
                            }
                            for __r in records.iter_mut() {
                                let __v: #stored_ty = __groups.remove(&__r.id).unwrap_or_default();
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
                    let singular = m2m_mutation_singular(&assoc.name);
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
                                conn.transaction::<(), ::autumn_web::AutumnError, _>(|conn| {
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
                conn: &'__a mut ::autumn_web::reexports::diesel_async::AsyncPgConnection,
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

/// Extract `#[validate(...)]` attributes from a field (verbatim pass-through).
fn validate_attrs(field: &Field) -> Vec<&syn::Attribute> {
    field
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("validate"))
        .collect()
}

/// Filter out framework-specific attributes (`#[id]`, `#[indexed]`, `#[validate]`,
/// `#[default]`, `#[factory_assoc]`, `#[lock_version]`, `#[searchable]`,
/// `#[state_machine]`) that shouldn't be on the query struct
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
                && !a.path().is_ident("state_machine")
        })
        .collect()
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

/// Build a manual `Debug` impl that redacts encrypted fields, so plaintext
/// (held in memory as a `String` for ergonomics) never appears in `Debug`
/// output, panic backtraces, or framework error messages. The development-only
/// escape hatch (`encryption::set_debug_plaintext`) opts back into plaintext.
fn redacting_debug_impl(
    struct_name: &syn::Ident,
    field_idents: &[&syn::Ident],
    encrypted_names: &[&str],
) -> TokenStream {
    let stmts = field_idents.iter().map(|ident| {
        let nm = ident.to_string();
        if encrypted_names.contains(&nm.as_str()) {
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
    // `#[lock_version]`) would instead get a raw database value, which the
    // decrypting reader then rejects as a malformed envelope. Reject the combo.
    if has_attr(field, "default") || has_attr(field, "lock_version") || has_attr(field, "id") {
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

enum FieldSearchable {
    NotSearchable,
    SearchableDefault,
    SearchableWithWeight(String),
}

/// Parse the field-level weight from `#[searchable(weight = "...")]`
fn parse_field_searchable_weight(field: &syn::Field) -> syn::Result<FieldSearchable> {
    for attr in &field.attrs {
        if attr.path().is_ident("searchable") {
            if matches!(attr.meta, syn::Meta::Path(_)) {
                return Ok(FieldSearchable::SearchableDefault);
            }
            let mut weight = None;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("weight") {
                    let value: syn::LitStr = meta.value()?.parse()?;
                    weight = Some(value.value());
                    Ok(())
                } else {
                    Err(meta.error("unsupported field searchable attribute"))
                }
            })?;
            return Ok(weight.map_or(
                FieldSearchable::SearchableDefault,
                FieldSearchable::SearchableWithWeight,
            ));
        }
    }
    Ok(FieldSearchable::NotSearchable)
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

/// True if a field has `#[id]`, `#[default]`, or `#[lock_version]` — all
/// three are excluded from the `NewX` insert type.
///
/// `#[lock_version]` fields are excluded because the DB column must carry a
/// `DEFAULT 0` constraint; the initial version is always zero and is never
/// supplied by the caller on insert.
fn excluded_from_new(field: &Field) -> bool {
    has_attr(field, "id") || has_attr(field, "default") || has_attr(field, "lock_version")
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

/// Check whether a type is `Option<...>`.
fn is_option_type(ty: &syn::Type) -> bool {
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
fn type_name_str(ty: &syn::Type) -> String {
    crate::api_doc::last_segment_name(ty).unwrap_or_else(|| "unknown".to_owned())
}

/// Emit a `TokenStream` that evaluates (at runtime) to a `serde_json::Value`
/// representing the JSON Schema for the given Rust type.
///
/// Handles `Option<T>` (nullable), `Vec<T>` (array), primitives (`String`,
/// `i64`, etc.), and everything else as a `$ref` to a component schema.
fn emit_json_schema_tokens(ty: &syn::Type) -> TokenStream {
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
    crate::api_doc::primitive_json_type(&name).map_or_else(
        || {
            let ref_path = format!("#/components/schemas/{name}");
            quote! { ::autumn_web::reexports::serde_json::json!({ "$ref": #ref_path }) }
        },
        |json_type| {
            quote! { ::autumn_web::reexports::serde_json::json!({ "type": #json_type }) }
        },
    )
}

/// Emit the body of `OpenApiSchema::schema()` for a list of fields.
///
/// `all_optional` is `true` for `UpdateX` structs where every field is
/// conceptually optional (backed by `Patch<T>`).
fn emit_schema_fn_body(fields: &[&&Field], all_optional: bool) -> TokenStream {
    emit_schema_fn_body_ext(fields, all_optional, &[])
}

fn emit_schema_fn_body_ext(
    fields: &[&&Field],
    all_optional: bool,
    extra_required: &[&&Field],
) -> TokenStream {
    let insertions: Vec<TokenStream> = fields
        .iter()
        .chain(extra_required.iter())
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            let field_name = ident.to_string();
            let schema_expr = emit_json_schema_tokens(&f.ty);
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
            .filter(|f| !is_option_type(&f.ty))
            .filter_map(|f| f.ident.as_ref().map(ToString::to_string))
            .collect()
    };
    for f in extra_required {
        if let Some(id) = f.ident.as_ref() {
            required_names.push(id.to_string());
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

// ── State machine support ────────────────────────────────────────────────────

/// A single allowed transition between two named states.
struct StateMachineTransition {
    from: String,
    to: String,
    /// Optional guard: name of a `&self` bool method that must return `true`.
    guard: Option<String>,
}

/// Parsed `#[state_machine(transitions(...))]` spec for one field.
struct StateMachineSpec {
    field_ident: syn::Ident,
    transitions: Vec<StateMachineTransition>,
}

/// Parse the inner `transitions(a -> b, b -> c: "guard", ...)` token tree.
fn parse_transitions(
    input: syn::parse::ParseStream<'_>,
) -> syn::Result<Vec<StateMachineTransition>> {
    let kw: syn::Ident = input.parse()?;
    if kw != "transitions" {
        return Err(syn::Error::new(kw.span(), "expected `transitions(...)`"));
    }
    let content;
    syn::parenthesized!(content in input);

    let mut transitions = Vec::new();
    while !content.is_empty() {
        let from: syn::Ident = content.parse()?;
        content.parse::<syn::Token![->]>()?;
        let to: syn::Ident = content.parse()?;
        let guard = if content.peek(syn::Token![:]) {
            content.parse::<syn::Token![:]>()?;
            let lit: syn::LitStr = content.parse()?;
            let guard_str = lit.value();
            // Validate that the guard name is a plain Rust identifier so that
            // format_ident! doesn't panic on names like "can-ship" or "can ship".
            syn::parse_str::<syn::Ident>(&guard_str).map_err(|_| {
                syn::Error::new_spanned(
                    &lit,
                    format!(
                        "`{guard_str}` is not a valid Rust identifier; \
                         guard names must be a plain function name such as `can_ship`"
                    ),
                )
            })?;
            Some(guard_str)
        } else {
            None
        };
        transitions.push(StateMachineTransition {
            from: from.to_string(),
            to: to.to_string(),
            guard,
        });
        if content.peek(syn::Token![,]) {
            content.parse::<syn::Token![,]>()?;
        }
    }
    Ok(transitions)
}

/// Parse `#[state_machine(transitions(...))]` from a field, returning the spec when present.
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
            let transitions = attr.parse_args_with(parse_transitions)?;
            spec = Some(StateMachineSpec {
                field_ident: ident.clone(),
                transitions,
            });
        }
    }
    Ok(spec)
}

/// Emit the three state machine items for one field: a transitions constant,
/// a `can_transition_{field}_to` predicate, and a `transition_{field}_to` method.
fn emit_state_machine_impl(model_name: &syn::Ident, spec: &StateMachineSpec) -> TokenStream {
    let field = &spec.field_ident;
    let raw_field_str = field.to_string();
    // Strip the raw-identifier prefix so `r#type` produces `type`-derived names
    // rather than trying to create identifiers like `can_transition_r#type_to`.
    let field_str = raw_field_str.strip_prefix("r#").unwrap_or(&raw_field_str);
    let field_upper = field_str.to_uppercase();

    let const_name = format_ident!("__AUTUMN_SM_{field_upper}_TRANSITIONS");
    let can_fn = format_ident!("can_transition_{field_str}_to");
    let transition_fn = format_ident!("transition_{field_str}_to");

    let const_entries: Vec<TokenStream> = spec
        .transitions
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

    let match_arms: Vec<TokenStream> = spec
        .transitions
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
        Ok(explicit) => explicit.unwrap_or_else(|| infer_table_name(&input.ident)),
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

    let filtered_outer_attrs: Vec<&syn::Attribute> = outer_attrs
        .iter()
        .filter(|a| {
            !a.path().is_ident("searchable")
                && !is_association_attr(a)
                && !a.path().is_ident("shard_key")
        })
        .collect();

    let new_name = format_ident!("New{name}");
    let update_name = format_ident!("Update{name}");
    let changeset_name = format_ident!("__{}Changeset", name);

    // Classify fields
    let all_fields: Vec<&Field> = fields.named.iter().collect();

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

    // Collect state machine specs from all fields (RED → GREEN: declarative SM support).
    let mut state_machine_impls: Vec<TokenStream> = Vec::new();
    for field in &all_fields {
        match parse_state_machine_spec(field) {
            Ok(Some(spec)) => {
                state_machine_impls.push(emit_state_machine_impl(name, &spec));
            }
            Ok(None) => {}
            Err(err) => return err.to_compile_error(),
        }
    }

    let mut search_field_names = Vec::new();
    let mut search_field_weights = Vec::new();

    for field in &all_fields {
        match parse_field_searchable_weight(field) {
            Ok(FieldSearchable::NotSearchable) => {}
            Ok(weight_type) => {
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
                search_field_names.push(field_ident.to_string());
                search_field_weights.push(weight_char);
            }
            Err(err) => return err.to_compile_error(),
        }
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
    let encrypted_use = if encrypted_columns.is_empty() {
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
    ) = if encrypted_columns.is_empty() {
        (
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
            redacting_debug_impl(name, &all_idents, &encrypted_column_names),
            quote! {},
            redacting_debug_impl(&new_name, &new_idents, &encrypted_column_names),
            quote! {},
            redacting_debug_impl(&update_name, &mutable_idents, &encrypted_column_names),
            quote! {},
            redacting_debug_impl(&changeset_name, &mutable_idents, &encrypted_column_names),
        )
    };
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
    let query_fields: Vec<TokenStream> = all_fields
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            let attrs = user_attrs(f);
            // Encrypted columns route through an AEAD wrapper transparently:
            // `serialize_as` encrypts on write, `deserialize_as` decrypts on read.
            // The public field stays a plain `String` (plaintext in Rust code).
            let enc = encrypted_wrapper_path(
                parse_field_encrypted_mode(f).unwrap_or(EncryptedMode::None),
            )
            .map(|w| quote! { #[diesel(serialize_as = #w, deserialize_as = #w)] });
            quote! { #(#attrs)* #enc pub #ident: #ty }
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
            quote! { #(#val_attrs)* #enc pub #ident: #ty }
        })
        .collect();

    // Build UpdateX fields:
    // - Regular mutable fields: Patch<T> (no #[validate] — validation only
    //   applies to NewX and the merged model)
    // - #[lock_version] field: plain required T (the client supplies the
    //   version they read; the framework increments it atomically)
    let mut update_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            quote! {
                #[serde(default)]
                pub #ident: ::autumn_web::hooks::Patch<#ty>
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
            let ty = &f.ty;
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
            let ty = &f.ty;
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

    // `__autumn_preload_retain`: applies this model's read scoping to rows
    // loaded by another model's `preload`, in-memory, so eager-loaded
    // associations hide the same rows the model's repository finders do.
    // Built from the model's own field set (the loading model can't see these
    // columns): soft-delete drops `deleted_at IS NOT NULL`; tenant scoping
    // keeps only rows matching the ambient `CURRENT_TENANT` when one is set.
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
    // Per-row sibling of `__autumn_preload_retain`, built from the same
    // soft-delete/tenant predicates. Many-to-many (`through =`) preload
    // loaders pair each child row with its parent key before grouping, so
    // they can't run a batch `Vec<Self>::retain` without losing that pairing
    // — `__autumn_preload_keep` lets them filter `(parent_key, Self)` pairs
    // one row at a time while applying the identical scoping rules.
    let soft_delete_keep = match deleted_at_field {
        Some(f) if is_option_type(&f.ty) => quote! {
            if <Self>::__autumn_repo_soft_delete_scope()
                && ::core::option::Option::is_some(&row.deleted_at)
            {
                return ::core::result::Result::Ok(false);
            }
        },
        _ => quote! {},
    };
    let tenant_keep = tenant_id_field.as_ref().map_or_else(
        || quote! {},
        |f| {
            let cmp = if is_option_type(&f.ty) {
                quote! { row.tenant_id.as_deref() == ::core::option::Option::Some(__t.as_str()) }
            } else {
                quote! { row.tenant_id == __t }
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
                            if !(#cmp) {
                                return ::core::result::Result::Ok(false);
                            }
                        }
                        // Fail closed, exactly like `__autumn_preload_retain`:
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
            /// (`through =`) preload loaders.
            #[doc(hidden)]
            pub fn __autumn_preload_keep(row: &Self) -> ::autumn_web::AutumnResult<bool> {
                #preload_scope_in_scope
                #soft_delete_keep
                #tenant_keep
                ::core::result::Result::Ok(true)
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
            quote! { #enc pub #ident: Option<#ty> }
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
    let factory_name = format_ident!("{name}Factory");

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
                quote! { pub #ident: #ty }
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

            factory_assoc_type(f).map_or_else(
                // Normal field: a single setter that assigns directly.
                || {
                    vec![quote! {
                        #[must_use]
                        pub fn #ident(mut self, val: impl ::core::convert::Into<#ty>) -> Self {
                            self.#ident = val.into();
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
                            self
                        }
                    };
                    vec![explicit_setter, pre_built_setter]
                },
            )
        })
        .collect();

    // build() — assemble NewX.
    // - Normal fields:  `{ident}: self.{ident}`
    // - Assoc fields:   `{ident}: self.{ident}.unwrap_or_default()`
    let factory_build_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = &f.ident;
            if factory_assoc_type(f).is_some() {
                quote! { #ident: self.#ident.unwrap_or_default() }
            } else {
                quote! { #ident: self.#ident }
            }
        })
        .collect();

    // create() — auto-resolve assoc fields, then insert.
    //
    // For each assoc field, emit a `let __resolved_{ident}` that either uses the
    // supplied value or auto-creates the associated model via its factory.
    //
    // A thread-local depth counter guards against cyclic associations: if the
    // chain exceeds 32 levels the factory panics with a clear message rather than
    // overflowing the stack.
    let assoc_resolutions: Vec<TokenStream> = fields_for_new
        .iter()
        .filter_map(|f| {
            let assoc_type = factory_assoc_type(f)?;
            let ident = f.ident.as_ref().unwrap();
            let resolved = format_ident!("__resolved_{ident}");
            Some(quote! {
                let #resolved = match self.#ident {
                    ::core::option::Option::Some(id) => id,
                    ::core::option::Option::None => {
                        #assoc_type::factory().create(pool).await.__autumn_pk()
                    }
                };
            })
        })
        .collect();

    // NewX construction inside create() uses resolved values for assoc fields.
    let create_build_fields: Vec<TokenStream> = fields_for_new
        .iter()
        .map(|f| {
            let ident = f.ident.as_ref().unwrap();
            if factory_assoc_type(f).is_some() {
                let resolved = format_ident!("__resolved_{ident}");
                quote! { #ident: #resolved }
            } else {
                quote! { #ident: self.#ident }
            }
        })
        .collect();

    // create() inner body — shared by both the assoc and non-assoc paths.
    let create_inner_body = quote! {
        use ::autumn_web::reexports::diesel::prelude::*;
        use ::autumn_web::reexports::diesel_async::RunQueryDsl;

        #(#assoc_resolutions)*

        let new_record = #new_name {
            #(#create_build_fields,)*
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
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
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
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            ) -> #name {
                #create_inner_body
            }
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
    let all_field_refs: Vec<&&Field> = all_fields.iter().collect();
    let query_struct_schema_body = emit_schema_fn_body(&all_field_refs, false);
    let new_struct_schema_body = emit_schema_fn_body(&fields_for_new, false);
    let update_struct_schema_body = {
        let extra: &[&&Field] = lock_version_field.as_slice();
        emit_schema_fn_body_ext(&fields_for_new, true, extra)
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

    quote! {
        #encrypted_use

        #[derive(#name_debug_derive Clone, ::diesel::Queryable, ::diesel::Selectable, ::diesel::AsChangeset, ::diesel::Insertable)]
        #[derive(::serde::Serialize, ::serde::Deserialize)]
        #[diesel(table_name = #table_ident)]
        #(#filtered_outer_attrs)*
        #vis struct #name {
            #(#query_fields,)*
        }
        #name_debug_impl

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
        }

        #(#encrypted_inventory)*

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
                Changeset = impl ::autumn_web::reexports::diesel::query_builder::QueryFragment<::autumn_web::reexports::diesel::pg::Pg> + ::core::marker::Send + ::core::marker::Sync + 'static
            > + ::core::marker::Send + ::core::marker::Sync + 'static {
                use ::autumn_web::reexports::diesel::ExpressionMethods as _;
                (#(#upsert_columns,)*)
            }

            #[doc(hidden)]
            pub async fn __autumn_execute_upsert(
                chunk: &[Self],
                tenant_id: ::core::option::Option<&str>,
                conn: &mut ::autumn_web::reexports::diesel_async::AsyncPgConnection,
            ) -> ::core::result::Result<::std::vec::Vec<Self>, ::autumn_web::reexports::diesel::result::Error> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;

                let stmt = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                    // Owned `Vec` (not `&[Self]`): encrypted columns use diesel
                    // `serialize_as`, which only implements `Insertable` for owned
                    // values. `to_vec()` also works for plain models.
                    .values(chunk.to_vec())
                    .on_conflict(#table_ident::id)
                    .do_update()
                    .set(Self::__autumn_upsert_set());

                #execute_upsert_body
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
                conn: &mut ::autumn_web::reexports::diesel_async::AsyncPgConnection,
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
                Ok(Self::new_with_changes(current.clone(), after))
            }

            #(#draft_accessors)*
        }

        /// Factory builder for [`#name`].
        ///
        /// Produced by [`#name::factory()`]. All fields are pre-filled with
        /// `Default::default()` so callers only need to specify the fields that
        /// matter for their scenario.
        #[derive(Debug, Clone)]
        #vis struct #factory_name {
            #(#factory_struct_fields,)*
        }

        impl ::core::default::Default for #factory_name {
            fn default() -> Self {
                Self {
                    #(#factory_default_fields,)*
                }
            }
        }

        impl #factory_name {
            #(#factory_setters)*

            /// Build a [`#new_name`] instance from the current factory state.
            ///
            /// Does not touch the database. Use [`#factory_name::create`] to
            /// also persist the record.
            #[must_use]
            pub fn build(self) -> #new_name {
                #new_name {
                    #(#factory_build_fields,)*
                }
            }

            #factory_create_method
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
                let mut __autumn_object = ::autumn_web::reexports::serde_json::Map::new();
                #(#commit_hook_serialize_fields)*
                let mut __autumn_value =
                    ::autumn_web::reexports::serde_json::Value::Object(__autumn_object);
                // Encrypted columns must not be persisted in plaintext into the
                // durable `autumn_repository_commit_hooks` table (#805). Rewrite
                // them as recoverable ciphertext in their declared mode.
                #commit_hook_encrypt_stmt
                Ok(__autumn_value)
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

        // ── State machine impls (one per #[state_machine] field) ────────────
        #(#state_machine_impls)*

        // ── Associations + eager loading (belongs_to / has_many / has_one) ──
        #preload_retain_impl
        #association_items
    }
}

pub fn infer_table_name(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    let snake = pascal_to_snake(&name);
    format!("{snake}s")
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
    fn target_fk_without_through_is_error() {
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> =
            vec![syn::parse_quote!(#[has_many(Tag, target_fk = tag_id)])];
        assert!(resolve_associations(&model, &attrs).is_err());
    }

    #[test]
    fn m2m_colliding_mutation_method_names_rejected() {
        // `tags` and `name = tag` both derive an `add_tag` helper — reject at
        // macro time rather than emitting a trait with duplicate methods.
        let model: syn::Ident = syn::parse_quote!(Post);
        let attrs: Vec<syn::Attribute> = vec![
            syn::parse_quote!(#[has_many(Tag, through = post_tags)]),
            syn::parse_quote!(#[has_many(Label, through = post_labels, name = tag)]),
        ];
        assert!(resolve_associations(&model, &attrs).is_err());
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
}
