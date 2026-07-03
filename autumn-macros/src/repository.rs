//! `#[repository(Model)]` proc macro implementation.
//!
//! Generates a concrete `PgXxxRepository` struct with:
//! - Auto-generated CRUD (`find_by_id`, `find_all`, save, update, `delete_by_id`, count, `exists_by_id`)
//! - Derived queries parsed from trait method names (`find_by_field`, `count_by_field`, etc.)
//! - `FromRequestParts` extractor impl
//!
//! Uses native async fn in traits (Rust 1.75+) - no `async_trait` crate needed.
//! Uses `diesel-async` `RunQueryDsl` for async queries - no sync `interact()`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::{Ident, ItemTrait, LitStr, TraitItem};

/// Parse `#[version_history(sensitive = ["col1", "col2"])]` from a trait's
/// outer attributes. Accumulates columns from ALL `#[version_history(...)]`
/// attributes (so columns may be split across multiple attributes).
/// Returns a `syn::Error` if any attribute contains a typo or unsupported
/// key, or if an array element is not a string literal — converting
/// silently-missed sensitive columns into a hard compile error.
fn parse_version_history_sensitive(attrs: &[syn::Attribute]) -> syn::Result<Vec<String>> {
    let mut cols: Vec<String> = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("version_history") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("sensitive") {
                    let value = meta.value()?;
                    let arr: syn::ExprArray = value.parse()?;
                    for elem in arr.elems {
                        match elem {
                            syn::Expr::Lit(syn::ExprLit {
                                lit: syn::Lit::Str(s),
                                ..
                            }) => cols.push(s.value()),
                            other => {
                                return Err(syn::Error::new_spanned(
                                    other,
                                    "sensitive column names must be string literals (e.g. `sensitive = [\"my_col\"]`)",
                                ));
                            }
                        }
                    }
                    Ok(())
                } else {
                    Err(meta.error(
                        "unknown version_history key; expected `sensitive = [\"col\", ...]`",
                    ))
                }
            })?;
        }
    }
    Ok(cols)
}

use crate::model::infer_table_name;

fn to_snake_case(name: &str) -> String {
    let mut result = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
        } else {
            result.push(ch);
        }
    }
    result
}

fn generate_topic_format(topic: &str, record_ident: &TokenStream) -> syn::Result<TokenStream> {
    let mut format_str = String::new();
    let mut args = Vec::new();
    let mut chars = topic.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut field = String::new();
            let mut closed = false;
            while let Some(&next_ch) = chars.peek() {
                if next_ch == '}' {
                    chars.next();
                    closed = true;
                    break;
                }
                field.push(chars.next().unwrap());
            }
            if closed {
                let trimmed = field.trim();
                if trimmed.is_empty() {
                    return Err(syn::Error::new(
                        proc_macro2::Span::call_site(),
                        "empty topic placeholder '{}' is not allowed",
                    ));
                }
                let field_ident = syn::parse_str::<syn::Ident>(trimmed).map_err(|e| {
                    syn::Error::new(
                        proc_macro2::Span::call_site(),
                        format!("invalid field name '{trimmed}' in topic placeholder: {e}"),
                    )
                })?;
                format_str.push_str("{}");
                args.push(quote! { ::autumn_web::repository::DisplayTopicField::to_topic_string(&#record_ident.#field_ident) });
            } else {
                format_str.push('{');
                format_str.push_str(&field);
            }
        } else {
            format_str.push(ch);
        }
    }
    let output = if args.is_empty() {
        quote! { ::std::string::ToString::to_string(#format_str) }
    } else {
        quote! { ::std::format!(#format_str, #(#args),*) }
    };
    Ok(output)
}

#[allow(clippy::struct_excessive_bools)]
struct RepoConfig {
    model_name: Ident,
    table_name: String,
    hooks_type: Option<Ident>,
    commit_hooks: bool,
    api_path: Option<String>,
    policy_type: Option<Ident>,
    scope_type: Option<Ident>,
    cursor_key: Option<String>,
    /// Rust type of the `cursor_key` field (e.g. `chrono::NaiveDateTime`).
    /// When provided the generated `cursor_page` emits a fully-typed two-part
    /// keyset filter.  When absent it falls back to an id-only cursor which is
    /// correct whenever `cursor_key` values are monotonically correlated with `id`.
    cursor_key_type: Option<syn::Path>,
    /// Enable soft-delete mode: `delete_by_id` sets `deleted_at = now()` instead
    /// of issuing `DELETE FROM`, and all default finders filter out soft-deleted
    /// rows. Requires the model table to have a `deleted_at TIMESTAMP NULL` column.
    soft_delete: bool,
    /// Enable row-level multi-tenancy: automatically scopes all queries to the
    /// active tenant context.
    tenant_scoped: bool,
    no_upsert_trait: bool,
    searchable: bool,
    /// Enable automatic record version history. When `true`, every successful
    /// insert, update, and delete produces an immutable `VersionEntry` in
    /// `_autumn_version_history`. Generates a `Model::history(id, &mut db, filter)`
    /// associated function on the repository.
    versioned: bool,
    /// When `true`, suppress the auto-generated `impl VersionedRecord for Model`.
    /// Use this when the model already has a hand-written `VersionedRecord`
    /// implementation (custom serialization, non-`i64` primary key, etc.) to
    /// avoid the duplicate-impl compile error (E0119).
    no_versioned_record_impl: bool,
    /// Pin generated read-only methods to the primary pool even when a read
    /// replica is configured (#971). Use for read-after-write-sensitive
    /// aggregates that cannot tolerate replication lag.
    primary_reads: bool,
    /// When `true`, the generated `FromRequestParts` resolves the tenant → shard
    /// automatically so handlers can extract the repository directly without a
    /// [`ShardedDb`] extractor. Requires shards to be configured in `[[database.shards]]`.
    /// Works with `tenant_scoped` for per-tenant routing and `across_tenants` for
    /// cross-shard fan-out reads. (issue #1209)
    sharded: bool,
    broadcasts: bool,
    broadcast_topic: Option<String>,
    broadcast_render: Option<syn::Path>,
    broadcast_container: Option<String>,
    generated_internal_hooks: bool,
}

#[allow(clippy::too_many_lines)]
fn parse_repo_args(attr: TokenStream) -> syn::Result<RepoConfig> {
    let mut model_name: Option<Ident> = None;
    let mut table_name: Option<String> = None;
    let mut hooks_type: Option<Ident> = None;
    let mut commit_hooks = false;
    let mut api_path: Option<String> = None;
    let mut policy_type: Option<Ident> = None;
    let mut scope_type: Option<Ident> = None;
    let mut cursor_key: Option<String> = None;
    let mut cursor_key_type: Option<syn::Path> = None;
    let mut soft_delete = false;
    let mut tenant_scoped = false;
    let mut no_upsert_trait = false;
    let mut searchable = false;
    let mut versioned = false;
    let mut no_versioned_record_impl = false;
    let mut primary_reads = false;
    let mut sharded = false;
    let mut broadcasts = false;
    let mut broadcast_topic: Option<String> = None;
    let mut broadcast_render: Option<syn::Path> = None;
    let mut broadcast_container: Option<String> = None;

    syn::meta::parser(|meta| {
        // `hooks = Ident` must be checked before the catch-all model_name case,
        // otherwise "hooks" would be parsed as the model name.
        if meta.path.is_ident("hooks") {
            let value: Ident = meta.value()?.parse()?;
            hooks_type = Some(value);
            Ok(())
        } else if meta.path.is_ident("commit_hooks") {
            let value: syn::LitBool = meta.value()?.parse()?;
            commit_hooks = value.value;
            Ok(())
        } else if meta.path.is_ident("broadcasts") {
            let value: syn::LitBool = meta.value()?.parse()?;
            broadcasts = value.value;
            Ok(())
        } else if meta.path.is_ident("topic") {
            let value: LitStr = meta.value()?.parse()?;
            broadcast_topic = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("render") {
            let value: syn::Path = meta.value()?.parse()?;
            broadcast_render = Some(value);
            Ok(())
        } else if meta.path.is_ident("container") {
            let value: LitStr = meta.value()?.parse()?;
            broadcast_container = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("table") {
            let value: LitStr = meta.value()?.parse()?;
            table_name = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("api") {
            let value: LitStr = meta.value()?.parse()?;
            api_path = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("policy") {
            let value: Ident = meta.value()?.parse()?;
            policy_type = Some(value);
            Ok(())
        } else if meta.path.is_ident("scope") {
            let value: Ident = meta.value()?.parse()?;
            scope_type = Some(value);
            Ok(())
        } else if meta.path.is_ident("cursor_key") {
            let value: Ident = meta.value()?.parse()?;
            cursor_key = Some(value.to_string());
            Ok(())
        } else if meta.path.is_ident("cursor_key_type") {
            let value: syn::Path = meta.value()?.parse()?;
            cursor_key_type = Some(value);
            Ok(())
        } else if meta.path.is_ident("soft_delete") {
            soft_delete = true;
            Ok(())
        } else if meta.path.is_ident("tenant_scoped") {
            tenant_scoped = true;
            Ok(())
        } else if meta.path.is_ident("no_upsert_trait") {
            no_upsert_trait = true;
            Ok(())
        } else if meta.path.is_ident("searchable") {
            searchable = true;
            Ok(())
        } else if meta.path.is_ident("versioned") {
            let value: syn::LitBool = meta.value()?.parse()?;
            versioned = value.value;
            Ok(())
        } else if meta.path.is_ident("no_versioned_record_impl") {
            no_versioned_record_impl = true;
            Ok(())
        } else if meta.path.is_ident("primary_reads") {
            primary_reads = true;
            Ok(())
        } else if meta.path.is_ident("sharded") {
            sharded = true;
            Ok(())
        } else if meta.path.get_ident().is_some() && model_name.is_none() {
            model_name = Some(meta.path.get_ident().unwrap().clone());
            Ok(())
        } else {
            Err(meta.error(
                "expected model name, table = \"...\", hooks = Type, commit_hooks = true, api = \"/path\", policy = Type, scope = Type, cursor_key = field, cursor_key_type = Type, soft_delete, tenant_scoped, no_upsert_trait, searchable, versioned = true, no_versioned_record_impl, primary_reads, sharded, broadcasts = true, topic = \"...\", render = fn, or container = \"...\"",
            ))
        }
    })
    .parse2(attr)?;

    let model = model_name.ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "expected model name: #[repository(ModelName)]",
        )
    })?;
    if commit_hooks && hooks_type.is_none() && !broadcasts {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "commit_hooks = true requires hooks = Type",
        ));
    }
    let table = table_name.unwrap_or_else(|| infer_table_name(&model));
    let generated_internal_hooks = false;

    Ok(RepoConfig {
        model_name: model,
        table_name: table,
        hooks_type,
        commit_hooks,
        api_path,
        policy_type,
        scope_type,
        cursor_key,
        cursor_key_type,
        soft_delete,
        tenant_scoped,
        no_upsert_trait,
        searchable,
        versioned,
        no_versioned_record_impl,
        primary_reads,
        sharded,
        broadcasts,
        broadcast_topic,
        broadcast_render,
        broadcast_container,
        generated_internal_hooks,
    })
}

/// Parse a derived query method name like `find_by_title_and_published`.
struct DerivedQuery {
    prefix: String,      // "find", "count", "delete", "exists"
    fields: Vec<String>, // ["title", "published"]
    #[allow(dead_code)] // reserved for Tier 2 OR support
    combinator: String, // "and" or "or"
}

fn parse_query_name(name: &str) -> Option<DerivedQuery> {
    let prefixes = ["find", "count", "delete", "exists"];
    let prefix = prefixes.iter().find(|p| name.starts_with(*p))?;

    let rest = name.strip_prefix(prefix)?;
    let rest = rest.strip_prefix("_by_")?;

    // Split on _and_ or _or_
    let (fields, combinator) = if rest.contains("_and_") {
        if rest.contains("_or_") {
            return None; // Can't mix
        }
        let parts: Vec<String> = rest.split("_and_").map(String::from).collect();
        (parts, "and".to_string())
    } else if rest.contains("_or_") {
        let parts: Vec<String> = rest.split("_or_").map(String::from).collect();
        (parts, "or".to_string())
    } else {
        (vec![rest.to_string()], "and".to_string())
    };

    Some(DerivedQuery {
        prefix: (*prefix).to_string(),
        fields,
        combinator,
    })
}

#[allow(clippy::too_many_lines)]
fn generate_derived_query_for_source(
    query: &DerivedQuery,
    table_ident: &Ident,
    model_name: &Ident,
    soft_delete: bool,
    query_source: &TokenStream,
    string_fields: &std::collections::HashSet<String>,
) -> TokenStream {
    let field_idents: Vec<Ident> = query.fields.iter().map(|f| format_ident!("{f}")).collect();
    let param_names: Vec<Ident> = query.fields.iter().map(|f| format_ident!("{f}")).collect();
    let table_name_str = table_ident.to_string();

    // Build the filter chain. For `String`-typed parameters we route the value
    // through the encrypted-column registry at runtime: a deterministic-encrypted
    // column is matched by its stable ciphertext, a randomized one errors (equality
    // is impossible), and a plain column passes through unchanged (#805). Non-string
    // params (e.g. an i64 id) can never target an encrypted column, so they filter
    // directly. The encoded value is bound to a `let` so its `?` lives in the async
    // method body rather than inside the Diesel expression.
    let mut encode_lets: Vec<TokenStream> = Vec::new();
    let filters: Vec<TokenStream> = field_idents
        .iter()
        .zip(param_names.iter())
        .map(|(field, param)| {
            if string_fields.contains(&field.to_string()) {
                let field_str = field.to_string();
                let enc_ident = format_ident!("__autumn_q_{field}");
                encode_lets.push(quote! {
                    let #enc_ident = ::autumn_web::encryption::encode_derived_query_param(
                        #table_name_str, #field_str, &#param,
                    )
                    .map_err(|__e| ::autumn_web::AutumnError::internal_server_error_msg(
                        __e.to_string(),
                    ))?;
                });
                quote! { .filter(#table_ident::#field.eq(&#enc_ident)) }
            } else {
                quote! { .filter(#table_ident::#field.eq(&#param)) }
            }
        })
        .collect();

    // Soft-delete repositories exclude archived rows in all derived find/count/exists queries.
    let soft_delete_filter = if soft_delete {
        quote! { .filter(#table_ident::deleted_at.is_null()) }
    } else {
        quote! {}
    };

    match query.prefix.as_str() {
        "find" => {
            quote! {
                #(#encode_lets)*
                let mut conn = self.__autumn_acquire_read_conn().await?;
                #query_source
                    #(#filters)*
                    #soft_delete_filter
                    .load::<#model_name>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            }
        }
        "count" => {
            quote! {
                #(#encode_lets)*
                let mut conn = self.__autumn_acquire_read_conn().await?;
                #query_source
                    #(#filters)*
                    #soft_delete_filter
                    .count()
                    .get_result::<i64>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            }
        }
        "delete" => {
            if soft_delete {
                quote! {
                    #(#encode_lets)*
                    let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    ::autumn_web::reexports::diesel::update(
                        #query_source #(#filters)* .filter(#table_ident::deleted_at.is_null())
                    )
                    .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                    .execute(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                    Ok(())
                }
            } else {
                quote! {
                    #(#encode_lets)*
                    let mut conn = self.__autumn_acquire_conn().await?;
                    ::autumn_web::reexports::diesel::delete(#query_source #(#filters)*)
                        .execute(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    Ok(())
                }
            }
        }
        "exists" => {
            quote! {
                #(#encode_lets)*
                let mut conn = self.__autumn_acquire_read_conn().await?;
                ::autumn_web::reexports::diesel::select(
                    ::autumn_web::reexports::diesel::dsl::exists(
                        #query_source #(#filters)* #soft_delete_filter
                    )
                )
                .get_result::<bool>(&mut conn)
                .await
                .map_err(::autumn_web::AutumnError::from)
            }
        }
        _ => {
            let msg = format!(
                "Unsupported query prefix: {}. Supported prefixes are find, count, delete, exists.",
                query.prefix
            );
            quote! { ::core::compile_error!(#msg); }
        }
    }
}

fn generate_derived_query(
    query: &DerivedQuery,
    table_ident: &Ident,
    model_name: &Ident,
    soft_delete: bool,
    tenant_scoped: bool,
    string_fields: &std::collections::HashSet<String>,
) -> TokenStream {
    if tenant_scoped {
        let across_tenants_query = generate_derived_query_for_source(
            query,
            table_ident,
            model_name,
            soft_delete,
            &quote! { #table_ident::table },
            string_fields,
        );
        let scoped_query = generate_derived_query_for_source(
            query,
            table_ident,
            model_name,
            soft_delete,
            &quote! { #table_ident::table.filter(#table_ident::tenant_id.eq(tenant_id)) },
            string_fields,
        );
        quote! {
            if self.across_tenants {
                #across_tenants_query
            } else {
                let tenant_id = match ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten() {
                    ::core::option::Option::Some(t) => t,
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(::autumn_web::AutumnError::internal_server_error_msg(
                            "no tenant context was established"
                        ));
                    }
                };
                #scoped_query
            }
        }
    } else {
        generate_derived_query_for_source(
            query,
            table_ident,
            model_name,
            soft_delete,
            &quote! { #table_ident::table },
            string_fields,
        )
    }
}

/// Whether a derived-query parameter type is a string (`String`, `&str`,
/// `&String`, …). Only string params can target an at-rest encrypted column
/// (encrypted columns are non-null `String` in v1), so only these are routed
/// through the runtime registry encoder.
fn is_string_param_type(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Reference(r) => is_string_param_type(&r.elem),
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "String" || s.ident == "str"),
        _ => false,
    }
}

/// Generate the token stream for a version-history INSERT into
/// `_autumn_version_history`. This is emitted inside an already-open
/// transaction so no extra nesting is needed.
///
/// Parameters:
/// - `table_name_str` — the literal string table name (e.g. `"posts"`)
/// - `op` — `"insert"`, `"update"`, or `"delete"`
/// - `with_ctx` — when `true` a `MutationContext` variable named `ctx`
///   is in scope; actor / `request_id` are taken from it.
///   When `false` the actor is hard-coded to `"system"` and `request_id` is `NULL`.
/// - `record_expr` — token stream for the record value (implements `Serialize`)
/// - `before_expr` — token stream for the "before" record value for updates
///   (same type; ignored for inserts/deletes).
/// - `conn_ident` — identifier of the `&mut AsyncPgConnection`-like variable
fn vh_insert_ts(
    table_name_str: &str,
    op: &str,
    with_ctx: bool,
    record_expr: &TokenStream,
    before_expr: Option<&TokenStream>,
    conn_ident: &TokenStream,
    model_ident: &proc_macro2::Ident,
) -> TokenStream {
    let actor_ts = if with_ctx {
        quote! { ctx.actor.as_deref().unwrap_or("system") }
    } else {
        quote! { "system" }
    };
    let request_id_ts = if with_ctx {
        quote! { ctx.request_id.as_deref() }
    } else {
        quote! { ::core::option::Option::None::<&str> }
    };

    let changes_ts = match op {
        "insert" => quote! {
            {
                use ::autumn_web::version_history::VersionedRecord as _;
                let __vh_json = (#record_expr).version_column_values();
                let __vh_changes = ::autumn_web::version_history::compute_insert_changes(&__vh_json, <#model_ident as ::autumn_web::version_history::VersionedRecord>::version_sensitive_columns());
                ::autumn_web::reexports::serde_json::to_string(&__vh_changes)
                    .unwrap_or_else(|_| "[]".to_string())
            }
        },
        "delete" => quote! {
            {
                use ::autumn_web::version_history::VersionedRecord as _;
                let __vh_json = (#record_expr).version_column_values();
                let __vh_changes = ::autumn_web::version_history::compute_delete_changes(&__vh_json, <#model_ident as ::autumn_web::version_history::VersionedRecord>::version_sensitive_columns());
                ::autumn_web::reexports::serde_json::to_string(&__vh_changes)
                    .unwrap_or_else(|_| "[]".to_string())
            }
        },
        _ => {
            // "update" — needs before and after
            let before = before_expr.unwrap_or(record_expr);
            quote! {
                {
                    use ::autumn_web::version_history::VersionedRecord as _;
                    let __vh_before_json = (#before).version_column_values();
                    let __vh_after_json = (#record_expr).version_column_values();
                    let __vh_changes = ::autumn_web::version_history::compute_diff(&__vh_before_json, &__vh_after_json, <#model_ident as ::autumn_web::version_history::VersionedRecord>::version_sensitive_columns());
                    ::autumn_web::reexports::serde_json::to_string(&__vh_changes)
                        .unwrap_or_else(|_| "[]".to_string())
                }
            }
        }
    };

    let table_name_ts = table_name_str.to_string();

    quote! {
        {
            let __vh_changes_str: ::std::string::String = #changes_ts;
            let __vh_record_id: i64 = {
                use ::autumn_web::version_history::VersionedRecord as _;
                (#record_expr).version_record_id()
            };
            let __vh_tenant_id: ::core::option::Option<&str> = {
                use ::autumn_web::version_history::VersionedRecord as _;
                (#record_expr).version_tenant_id()
            };
            let __vh_actor: &str = #actor_ts;
            let __vh_request_id: ::core::option::Option<&str> = #request_id_ts;
            ::autumn_web::reexports::diesel::sql_query(
                "INSERT INTO _autumn_version_history \
                 (table_name, tenant_id, record_id, op, actor, request_id, changes) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)"
            )
            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(#table_name_ts)
            .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>, _>(__vh_tenant_id)
            .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(__vh_record_id)
            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(#op)
            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(__vh_actor)
            .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>, _>(__vh_request_id)
            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(__vh_changes_str)
            .execute(#conn_ident)
            .await
            .map_err(::autumn_web::AutumnError::from)?;
        }
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::large_stack_frames
)]
#[allow(clippy::cognitive_complexity)]
pub fn repository_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut config = match parse_repo_args(attr) {
        Ok(c) => c,
        Err(err) => return err.to_compile_error(),
    };

    let trait_def: ItemTrait = match syn::parse2(item) {
        Ok(t) => t,
        Err(err) => return err.to_compile_error(),
    };

    if config.broadcasts && config.hooks_type.is_none() {
        config.hooks_type = Some(format_ident!("Pg{}InternalHooks", trait_def.ident));
        config.generated_internal_hooks = true;
    }

    let model_name = &config.model_name;
    let table_name = &config.table_name;
    let table_ident = format_ident!("{table_name}");
    let trait_name = &trait_def.ident;
    let pg_name = format_ident!("Pg{trait_name}");
    let new_name = format_ident!("New{model_name}");
    let update_name = format_ident!("Update{model_name}");
    let vis = &trait_def.vis;
    let commit_hooks_enabled = config.hooks_type.is_some() && config.commit_hooks;
    let tenant_extra = usize::from(config.tenant_scoped);

    // Soft-delete filter fragment: appended to every finder when soft_delete is true.
    let sd_filter = if config.soft_delete {
        quote! { .filter(#table_ident::deleted_at.is_null()) }
    } else {
        quote! {}
    };

    // Parse derived query methods from trait body
    let mut derived_trait_methods = Vec::new();
    let mut derived_impl_methods = Vec::new();
    // §1d: per-shard helpers for derived read methods that fan out under
    // across_tenants. Emitted into the inherent impl (a trait impl cannot hold
    // non-trait members), so kept in a separate list.
    let mut derived_one_shard_helpers = Vec::new();

    // §1d: cross-shard reject for derived *write* methods (delete_by_*). Writes
    // cannot be fanned out (no cross-shard transaction), so reject under
    // across_tenants on a sharded repo instead of silently hitting one shard.
    // Empty (zero-cost) unless sharded.
    let derived_cross_shard_write_guard = if config.sharded && config.tenant_scoped {
        quote! {
            if self.across_tenants && self.__autumn_shards.is_some() {
                return ::core::result::Result::Err(
                    ::autumn_web::AutumnError::bad_request_msg(
                        "cross-shard derived writes are not supported: across_tenants() cannot be \
                         used for mutation on a sharded repository"
                    )
                );
            }
        }
    } else {
        quote! {}
    };

    for item in &trait_def.items {
        if let TraitItem::Fn(method) = item {
            let method_name = method.sig.ident.to_string();
            if let Some(query) = parse_query_name(&method_name) {
                let fn_ident = &method.sig.ident;

                // Use the user's actual parameter types from the trait signature.
                // The user writes: fn find_by_tag(tag: String) -> Vec<Bookmark>
                // We extract the (name: Type) pairs directly.
                let user_params: Vec<TokenStream> = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|arg| {
                        if let syn::FnArg::Typed(pat_type) = arg {
                            let pat = &pat_type.pat;
                            let ty = &pat_type.ty;
                            Some(quote! { #pat: #ty })
                        } else {
                            None // skip `self`
                        }
                    })
                    .collect();

                // The subset of parameters whose type is a string — only these can
                // bind an at-rest encrypted column, so only these are routed through
                // the runtime registry encoder in the filter chain (#805).
                let string_fields: std::collections::HashSet<String> = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|arg| {
                        let syn::FnArg::Typed(pat_type) = arg else {
                            return None;
                        };
                        let syn::Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
                            return None;
                        };
                        is_string_param_type(&pat_type.ty).then(|| pat_ident.ident.to_string())
                    })
                    .collect();

                // Determine return type from prefix
                let return_type = match query.prefix.as_str() {
                    "find" => quote! { Vec<#model_name> },
                    "count" => quote! { i64 },
                    "exists" => quote! { bool },
                    _ => quote! { () }, // delete + unknown
                };

                let params = &user_params;

                // Bare parameter names, for forwarding to the per-shard helper.
                let param_idents: Vec<Ident> = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|arg| {
                        let syn::FnArg::Typed(pat_type) = arg else {
                            return None;
                        };
                        let syn::Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
                            return None;
                        };
                        Some(pat_ident.ident.clone())
                    })
                    .collect();

                // A borrowed (reference) parameter can't be cloned into an owned,
                // `'static` value for the per-shard fan-out futures (cloning a
                // `&str` yields a `&str`), so cross-shard fan-out of such a derived
                // read would not compile. Reject it instead, with guidance to use
                // an owned parameter type.
                let has_borrowed_param = method.sig.inputs.iter().any(|arg| {
                    matches!(
                        arg,
                        syn::FnArg::Typed(pat_type)
                            if matches!(pat_type.ty.as_ref(), syn::Type::Reference(_))
                    )
                });

                let body = generate_derived_query(
                    &query,
                    &table_ident,
                    model_name,
                    config.soft_delete,
                    config.tenant_scoped,
                    &string_fields,
                );

                derived_trait_methods.push(quote! {
                    fn #fn_ident(&self, #(#params),*) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<#return_type>> + Send;
                });

                // §1d: under across_tenants on a sharded repo, fan out read-prefix
                // derived methods (find/count/exists) across all shards and merge;
                // reject write-prefix ones (delete). Non-sharded repos keep the
                // plain body (zero-cost).
                let is_read_prefix = matches!(query.prefix.as_str(), "find" | "count" | "exists");
                if config.sharded && config.tenant_scoped && is_read_prefix && has_borrowed_param {
                    // Borrowed-param derived read on a sharded repo: can't fan out
                    // ('static futures need owned params), so reject cross-shard.
                    let method_name = fn_ident.to_string();
                    let msg = format!(
                        "cross-shard {method_name} is not supported: across_tenants() cannot fan \
                         out a derived query with a borrowed parameter; declare the parameter as \
                         an owned type (e.g. String) to enable fan-out, or query a specific shard"
                    );
                    derived_impl_methods.push(quote! {
                        async fn #fn_ident(&self, #(#params),*) -> ::autumn_web::AutumnResult<#return_type> {
                            use ::autumn_web::reexports::diesel::prelude::*;
                            use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                            if self.across_tenants && self.__autumn_shards.is_some() {
                                return ::core::result::Result::Err(
                                    ::autumn_web::AutumnError::bad_request_msg(#msg)
                                );
                            }
                            #body
                        }
                    });
                } else if config.sharded && config.tenant_scoped && is_read_prefix {
                    let one_shard_ident = format_ident!("__autumn_{}_one_shard", fn_ident);
                    let merge = match query.prefix.as_str() {
                        "find" => quote! { __results.into_iter().flatten().collect() },
                        "count" => quote! { __results.into_iter().sum() },
                        // "exists"
                        _ => quote! { __results.into_iter().any(|__b| __b) },
                    };
                    derived_one_shard_helpers.push(quote! {
                        #[doc(hidden)]
                        async fn #one_shard_ident(&self, #(#params),*) -> ::autumn_web::AutumnResult<#return_type> {
                            use ::autumn_web::reexports::diesel::prelude::*;
                            use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                            #body
                        }
                    });
                    derived_impl_methods.push(quote! {
                        async fn #fn_ident(&self, #(#params),*) -> ::autumn_web::AutumnResult<#return_type> {
                            use ::autumn_web::reexports::diesel::prelude::*;
                            use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                            if self.across_tenants {
                                if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                                    // Params are cloned once per shard (the fan-out closure is
                                    // `Fn`); allow it for `Copy` params too (e.g. an i64 filter).
                                    #[allow(clippy::clone_on_copy)]
                                    let __results = __shards.fan_out_shards(|__shard| {
                                        let __sub = self.__autumn_for_shard(__shard);
                                        #(let #param_idents = ::core::clone::Clone::clone(&#param_idents);)*
                                        async move { __sub.#one_shard_ident(#(#param_idents),*).await }
                                    }).await?;
                                    return ::core::result::Result::Ok(#merge);
                                }
                            }
                            #body
                        }
                    });
                } else {
                    derived_impl_methods.push(quote! {
                        async fn #fn_ident(&self, #(#params),*) -> ::autumn_web::AutumnResult<#return_type> {
                            use ::autumn_web::reexports::diesel::prelude::*;
                            use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                            #derived_cross_shard_write_guard
                            #body
                        }
                    });
                }
            }
        }
    }

    // ── Build struct fields, extractor init, and CRUD bodies ──────────────
    //
    // When `hooks_type` is present, the struct gains a `hooks` field,
    // the extractor initialises it with `Default::default()`, and the
    // save / update / delete methods are wrapped in a transactional
    // hook lifecycle (before_* ΓåÆ persist).
    //
    // When absent, the generated code is identical to the pre-hooks version
    // (zero-cost path).

    let tenant_struct_field = if config.tenant_scoped {
        quote! { across_tenants: bool, }
    } else {
        quote! {}
    };

    let tenant_clone_field = if config.tenant_scoped {
        quote! { across_tenants: self.across_tenants, }
    } else {
        quote! {}
    };

    let tenant_init_field = if config.tenant_scoped {
        quote! { across_tenants: false, }
    } else {
        quote! {}
    };

    // `__autumn_shards` carries the full ShardSet for cross-shard fan-out
    // under `across_tenants()`. Only present when `sharded = true`; `None`
    // in constructors that lack request context (from_shard, with_pool_untracked).
    let shards_struct_field = if config.sharded {
        quote! {
            #[doc(hidden)]
            __autumn_shards: ::core::option::Option<::autumn_web::sharding::ShardSet>,
        }
    } else {
        quote! {}
    };

    let shards_clone_field = if config.sharded {
        quote! { __autumn_shards: self.__autumn_shards.clone(), }
    } else {
        quote! {}
    };

    // The non-sharded extractor and shard-unaware constructors always use None.
    let shards_none_field = if config.sharded {
        quote! { __autumn_shards: ::core::option::Option::None, }
    } else {
        quote! {}
    };

    // The self-routing sharded extractor populates this from the resolved ShardSet.
    let shards_some_field = if config.sharded {
        quote! { __autumn_shards: ::core::option::Option::Some(__shard_set), }
    } else {
        quote! {}
    };

    // `from_shard(&ShardedDb)` carries the ShardSet from the ShardedDb so that
    // `across_tenants()` on a from_shard-built repo fans out / guards writes
    // exactly like the extractor path (it is the standard sharded constructor).
    let shards_from_db_field = if config.sharded {
        quote! {
            __autumn_shards: ::core::option::Option::Some(
                ::core::clone::Clone::clone(db.__autumn_shard_set()),
            ),
        }
    } else {
        quote! {}
    };

    // Broadcast field tokens — defined here so they are in scope for both
    // `across_tenants_method` (below) and the main struct/constructor block
    // (further down).  The full doc-comment version `bcast_struct_field` is
    // used only in the struct definition; the clone/none/state variants are
    // used in every constructor that builds a `Self { .. }` literal.
    let has_broadcasts = config.broadcasts;
    let bcast_struct_field = if has_broadcasts {
        quote! {
            /// Broadcast handle for live OOB fragment publishing (`broadcasts = "topic"`).
            /// `None` when the repository was built without an `AppState` (e.g. `with_pool_untracked`).
            __autumn_broadcast: ::std::option::Option<::autumn_web::channels::Broadcast>,
        }
    } else {
        quote! {}
    };
    let bcast_clone_field = if has_broadcasts {
        quote! { __autumn_broadcast: self.__autumn_broadcast.clone(), }
    } else {
        quote! {}
    };
    let bcast_field_none = if has_broadcasts {
        quote! { __autumn_broadcast: ::core::option::Option::None, }
    } else {
        quote! {}
    };
    let bcast_field_some_state = if has_broadcasts {
        quote! { __autumn_broadcast: ::std::option::Option::Some(state.broadcast()), }
    } else {
        quote! {}
    };

    let across_tenants_method = if config.tenant_scoped {
        if let Some(hooks_ident) = &config.hooks_type {
            let idempotency_clone_field = if commit_hooks_enabled {
                quote! {
                    idempotency: self.idempotency.clone(),
                }
            } else {
                quote! {}
            };
            quote! {
                pub fn across_tenants(&self) -> Self {
                    if ::core::cfg!(debug_assertions) {
                        ::autumn_web::reexports::tracing::warn!("across_tenants() called on tenant_scoped repository");
                    }
                    Self {
                        pool: self.pool.clone(),
                        hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksClone>::autumn_clone(&self.hooks),
                        #idempotency_clone_field
                        across_tenants: true,
                        #shards_clone_field
                        #bcast_clone_field
                        __autumn_read_route: self.__autumn_read_route.clone(),
                        __autumn_statement_timeout_ms: self.__autumn_statement_timeout_ms,
                        __autumn_slow_threshold: self.__autumn_slow_threshold,
                        __autumn_route: self.__autumn_route.clone(),
                    }
                }
            }
        } else {
            quote! {
                pub fn across_tenants(&self) -> Self {
                    if ::core::cfg!(debug_assertions) {
                        ::autumn_web::reexports::tracing::warn!("across_tenants() called on tenant_scoped repository");
                    }
                    Self {
                        pool: self.pool.clone(),
                        across_tenants: true,
                        #shards_clone_field
                        #bcast_clone_field
                        __autumn_read_route: self.__autumn_read_route.clone(),
                        __autumn_statement_timeout_ms: self.__autumn_statement_timeout_ms,
                        __autumn_slow_threshold: self.__autumn_slow_threshold,
                        __autumn_route: self.__autumn_route.clone(),
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    // §1d: build a per-shard sub-repo for cross-shard read fan-out. Mirrors
    // `across_tenants()` but routes to a specific shard: pool = that shard's
    // primary, read route = that shard's `read_route()` (so replica routing /
    // fail-closed is honored, not silently forced to primary), and
    // `__autumn_shards = None` to stop recursion. The parent's statement timeout
    // and slow-query threshold are preserved so a fan-out scan respects the same
    // limits as an ordinary generated read. Only emitted for sharded +
    // tenant_scoped repos (the only ones that fan out).
    let for_shard_method = if config.sharded && config.tenant_scoped {
        let hooks_field = config.hooks_type.as_ref().map(|hooks_ident| {
            quote! {
                hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksClone>::autumn_clone(&self.hooks),
            }
        });
        let idempotency_field = if commit_hooks_enabled {
            quote! { idempotency: self.idempotency.clone(), }
        } else {
            quote! {}
        };
        quote! {
            #[doc(hidden)]
            fn __autumn_for_shard(&self, __shard: &::autumn_web::sharding::Shard) -> Self {
                Self {
                    pool: ::core::clone::Clone::clone(__shard.primary_pool()),
                    #hooks_field
                    #idempotency_field
                    across_tenants: true,
                    __autumn_shards: ::core::option::Option::None,
                    // Honor the shard's read routing (replica / fail-closed),
                    // but preserve an explicit parent primary-read override
                    // (`primary_reads` or `on_primary()`) so cross-shard
                    // read-your-writes is not silently sent to replicas (#1d).
                    __autumn_read_route: match self.__autumn_read_route {
                        ::autumn_web::repository::ReadRoute::Primary =>
                            ::autumn_web::repository::ReadRoute::Primary,
                        _ => __shard.read_route(),
                    },
                    __autumn_statement_timeout_ms: self.__autumn_statement_timeout_ms,
                    __autumn_slow_threshold: self.__autumn_slow_threshold,
                    // Re-tag the route label with this shard so per-shard DB
                    // metrics and slow-query logs attribute fan-out work to the
                    // shard executing it, not the originally-routed shard.
                    __autumn_route: ::autumn_web::sharding::reshard_route_label(
                        self.__autumn_route.as_deref(),
                        __shard.name(),
                    ),
                    // Shard sub-repos are used for read fan-out only; mutations
                    // broadcast from the parent, not the per-shard instance.
                    #bcast_field_none
                }
            }
        }
    } else {
        quote! {}
    };

    // Bridge the repository's tenant/soft-delete config to the target model's
    // `__autumn_preload_retain` (generated by `#[model]`, which cannot see
    // `#[repository]` flags). Emitted as inherent associated fns that override
    // the default-`false` `AutumnPreloadScopeExt` blanket impl. Only the
    // enabled flags are emitted, so a non-scoped repository adds nothing and
    // preload scoping then matches finder behavior exactly.
    let preload_scope_impl = {
        let mut scope_fns: Vec<proc_macro2::TokenStream> = Vec::new();
        if config.soft_delete {
            scope_fns.push(quote! {
                #[doc(hidden)]
                #[must_use]
                pub fn __autumn_repo_soft_delete_scope() -> bool {
                    true
                }
            });
        }
        if config.tenant_scoped {
            scope_fns.push(quote! {
                #[doc(hidden)]
                #[must_use]
                pub fn __autumn_repo_tenant_scope() -> bool {
                    true
                }
            });
        }
        if scope_fns.is_empty() {
            quote! {}
        } else {
            quote! {
                impl #model_name {
                    #(#scope_fns)*
                }
            }
        }
    };

    // Bridge this repository to the many-to-many mutation traits `#[model]`
    // generates for `#[has_many(Target, through = ...)]` associations on
    // `#model_name` (see `M2mConnSource`). Emitted unconditionally — cheap
    // when the model has no `through =` associations (nothing calls it) —
    // so it doesn't need to know which associations exist, only its own
    // model and write-connection helper.
    let m2m_conn_source_impl = quote! {
        impl ::autumn_web::repository::M2mConnSource for #pg_name {
            type Model = #model_name;

            async fn __autumn_m2m_write_conn(
                &self,
            ) -> ::autumn_web::AutumnResult<
                ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Object<
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            > {
                self.__autumn_acquire_conn().await
            }
        }
    };

    // `preload` publishes this as an ambient task-local so a tenant-scoped
    // repository's `across_tenants()` choice reaches the (recursive) target
    // retains and skips the tenant predicate there too — matching finders.
    let preload_across_expr = if config.tenant_scoped {
        quote! { self.across_tenants }
    } else {
        quote! { false }
    };

    // §1d: cross-shard reject for preload. Empty unless sharded + tenant_scoped.
    let preload_cross_shard_guard = if config.sharded && config.tenant_scoped {
        quote! {
            if self.across_tenants && self.__autumn_shards.is_some() {
                return ::core::result::Result::Err(
                    ::autumn_web::AutumnError::bad_request_msg(
                        "cross-shard preload is not supported: \
                         associations cannot be loaded from a single routed \
                         connection across shards; preload per shard instead"
                    )
                );
            }
        }
    } else {
        quote! {}
    };

    let with_pool_method = {
        let hooks_field = config.hooks_type.as_ref().map_or_else(
            || quote! {},
            |hooks_ident| {
                quote! {
                    hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default(),
                }
            },
        );
        let idempotency_field = if commit_hooks_enabled {
            quote! { idempotency: ::core::option::Option::None, }
        } else {
            quote! {}
        };
        let register_hooks = if commit_hooks_enabled {
            quote! { Self::__autumn_register_repository_commit_hooks(); }
        } else {
            quote! {}
        };
        // #1274: snapshot the shard's read route so read-only methods reach
        // the shard's replica when one is healthy, mirroring the non-shard
        // `read_route_init` below. `primary_reads` still pins reads to the
        // shard primary at compile time.
        let from_shard_read_route = if config.primary_reads {
            quote! { ::autumn_web::repository::ReadRoute::Primary }
        } else {
            quote! { ::core::clone::Clone::clone(&__seed.read_route) }
        };
        let from_shard_reads_doc = if config.primary_reads {
            quote! {
                #[doc = "This repository is declared `primary_reads`, so reads stay on the shard's primary pool."]
            }
        } else {
            quote! {
                #[doc = "Read-only methods route to the shard's read replica when one is configured and healthy (honoring the shard's `replica_fallback` policy); mutating methods always use the shard's primary. Pin a single call chain to the primary with [`on_primary`](Self::on_primary)."]
            }
        };
        // When `broadcasts` is configured the test-helper constructor must be
        let test_ctor_method = if has_broadcasts {
            quote! {
                /// Construct this repository with an explicit broadcast handle.
                ///
                /// For use in tests only — simulates what `FromRequestParts<AppState>` does
                /// when building a repository that can publish OOB live fragments.
                #[doc(hidden)]
                #[must_use]
                pub fn __autumn_test_with_broadcast(
                    pool: ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                        ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                    >,
                    broadcast: ::autumn_web::channels::Broadcast,
                ) -> Self {
                    #register_hooks
                    Self {
                        pool,
                        #hooks_field
                        #idempotency_field
                        #tenant_init_field
                        #shards_none_field
                        __autumn_read_route: ::autumn_web::repository::ReadRoute::Primary,
                        __autumn_statement_timeout_ms: 0,
                        __autumn_slow_threshold: ::std::time::Duration::from_millis(500),
                        __autumn_route: ::core::option::Option::None,
                        __autumn_broadcast: ::core::option::Option::Some(broadcast),
                    }
                }
            }
        } else {
            quote! {}
        };
        quote! {
            /// Construct this repository from a [`ShardedDb`](::autumn_web::sharding::ShardedDb)
            /// extractor, preserving the full request instrumentation — statement
            /// timeout, slow-query threshold, and route metric label — from the
            /// shard context:
            ///
            /// ```rust,ignore
            /// #[post("/bookmarks")]
            /// async fn create(db: ShardedDb, Json(body): Json<Body>) -> AutumnResult<Json<Bookmark>> {
            ///     let repo = BookmarkRepository::from_shard(&db);
            ///     // repo carries the same timeout / slow-query / route label as `db`
            ///     let bookmark = repo.save(&body.into()).await?;
            ///     Ok(Json(bookmark))
            /// }
            /// ```
            ///
            #from_shard_reads_doc
            ///
            /// Use this constructor as the standard way to build a repository on
            /// a shard; prefer [`with_pool_untracked`](Self::with_pool_untracked)
            /// only when you intentionally want to bypass request instrumentation.
            #[must_use]
            pub fn from_shard(db: &::autumn_web::sharding::ShardedDb) -> Self {
                #register_hooks
                let __seed = db.__autumn_repository_seed();
                Self {
                    pool: ::core::clone::Clone::clone(&__seed.pool),
                    #hooks_field
                    #idempotency_field
                    #tenant_init_field
                    #shards_from_db_field
                    __autumn_read_route: #from_shard_read_route,
                    __autumn_statement_timeout_ms: __seed.statement_timeout_ms,
                    __autumn_slow_threshold: __seed.slow_query_threshold,
                    __autumn_route: ::core::clone::Clone::clone(&__seed.route),
                    #bcast_field_none
                }
            }

            /// Construct this repository over an explicit pool, **bypassing**
            /// request instrumentation.
            ///
            /// Statement timeout, slow-query threshold, and route metric labels
            /// are reset to framework defaults (no timeout, 500 ms threshold,
            /// no route label).  Prefer [`from_shard`](Self::from_shard) when
            /// you have a [`ShardedDb`](::autumn_web::sharding::ShardedDb)
            /// available; use this constructor only when you need an explicit
            /// pool without any request context:
            ///
            /// ```rust,ignore
            /// let repo = BookmarkRepository::with_pool_untracked(pool.clone());
            /// ```
            #[must_use]
            pub fn with_pool_untracked(
                pool: ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            ) -> Self {
                #register_hooks
                Self {
                    pool,
                    #hooks_field
                    #idempotency_field
                    #tenant_init_field
                    #shards_none_field
                    __autumn_read_route: ::autumn_web::repository::ReadRoute::Primary,
                    __autumn_statement_timeout_ms: 0,
                    __autumn_slow_threshold: ::std::time::Duration::from_millis(500),
                    __autumn_route: ::core::option::Option::None,
                    #bcast_field_none
                }
            }

            #test_ctor_method
        }
    };

    // #971: snapshot the read-routing decision once per extraction. The
    // `primary_reads` attribute pins read-after-write-sensitive aggregates
    // to the primary at compile time; otherwise the route follows
    // `AppState::read_pool` semantics (replica when healthy, primary
    // fallback or fail-fast per the configured `replica_fallback` policy).
    let read_route_init = if config.primary_reads {
        quote! {
            let __autumn_read_route = ::autumn_web::repository::ReadRoute::Primary;
        }
    } else {
        quote! {
            let __autumn_read_route =
                ::autumn_web::repository::ReadRoute::from_state(state);
        }
    };

    let (
        struct_fields,
        clone_impl,
        extractor_init,
        save_body,
        update_body,
        delete_body,
        hook_support_methods,
        hook_inventory_registration,
        save_many_body,
        save_many_skip_invalid_body,
        update_many_body,
        delete_many_body,
        upsert_many_body,
    ) = if let Some(ref hooks_ident) = config.hooks_type {
        // ── Struct fields with hooks ───────────────────────
        let idempotency_struct_field = if commit_hooks_enabled {
            quote! {
                idempotency: ::core::option::Option<::autumn_web::idempotency::IdempotencyContext>,
            }
        } else {
            quote! {}
        };
        let idempotency_clone_field = if commit_hooks_enabled {
            quote! {
                idempotency: self.idempotency.clone(),
            }
        } else {
            quote! {}
        };

        let struct_fields = quote! {
            pool: ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                ::autumn_web::reexports::diesel_async::AsyncPgConnection,
            >,
            hooks: #hooks_ident,
            #idempotency_struct_field
            #tenant_struct_field
            #shards_struct_field
            /// Read-routing snapshot for generated read-only methods (#971).
            __autumn_read_route: ::autumn_web::repository::ReadRoute,
            /// Statement timeout to apply on every connection checkout (ms). 0 = no limit.
            __autumn_statement_timeout_ms: u64,
            /// Slow-query logging threshold.
            __autumn_slow_threshold: ::std::time::Duration,
            /// Route path from `MatchedPath` for metrics labels.
            __autumn_route: ::std::option::Option<::std::string::String>,
            #bcast_struct_field
        };

        let clone_impl = quote! {
            impl ::core::clone::Clone for #pg_name {
                fn clone(&self) -> Self {
                    Self {
                        pool: self.pool.clone(),
                        hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksClone>::autumn_clone(&self.hooks),
                        #idempotency_clone_field
                        #tenant_clone_field
                        #shards_clone_field
                        __autumn_read_route: self.__autumn_read_route.clone(),
                        __autumn_statement_timeout_ms: self.__autumn_statement_timeout_ms,
                        __autumn_slow_threshold: self.__autumn_slow_threshold,
                        __autumn_route: self.__autumn_route.clone(),
                        #bcast_clone_field
                    }
                }
            }
        };

        let timeout_route_init = quote! {
            use ::autumn_web::db::DbState as _;
            // Postgres statement_timeout is a signed 32-bit integer (ms).
            const __AUTUMN_PG_TIMEOUT_MAX_MS: u64 = i32::MAX as u64;
            let __autumn_timeout_ms: u64 = _parts
                .extensions
                .get::<::autumn_web::db::StatementTimeout>()
                .map(|t| ::std::convert::TryFrom::try_from(t.0.as_millis()).unwrap_or(u64::MAX))
                .or_else(|| state.statement_timeout().map(|d| ::std::convert::TryFrom::try_from(d.as_millis()).unwrap_or(u64::MAX)))
                .unwrap_or(0u64)
                .min(__AUTUMN_PG_TIMEOUT_MAX_MS);
            let __autumn_slow_threshold = state.slow_query_threshold();
            let __autumn_route: ::std::option::Option<::std::string::String> = _parts
                .extensions
                .get::<::autumn_web::reexports::axum::extract::MatchedPath>()
                .map(|p| p.as_str().to_owned());
            #read_route_init
        };

        let extractor_init = if commit_hooks_enabled {
            quote! {
                #pg_name::__autumn_register_repository_commit_hooks();
                #timeout_route_init
                Ok(#pg_name {
                    pool,
                    hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default(),
                    idempotency: _parts
                        .extensions
                        .get::<::autumn_web::idempotency::IdempotencyContext>()
                        .cloned(),
                    #tenant_init_field
                    #shards_none_field
                    __autumn_read_route,
                    __autumn_statement_timeout_ms: __autumn_timeout_ms,
                    __autumn_slow_threshold,
                    __autumn_route,
                    #bcast_field_some_state
                })
            }
        } else {
            quote! {
                #timeout_route_init
                Ok(#pg_name {
                    pool,
                    hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default(),
                    #tenant_init_field
                    #shards_none_field
                    __autumn_read_route,
                    __autumn_statement_timeout_ms: __autumn_timeout_ms,
                    __autumn_slow_threshold,
                    __autumn_route,
                    #bcast_field_some_state
                })
            }
        };

        let (broadcast_create, broadcast_update, broadcast_delete) = if config.broadcasts {
            let base_topic_expr = match generate_topic_format(
                config
                    .broadcast_topic
                    .as_deref()
                    .unwrap_or(&config.table_name),
                &quote! { __record_ref },
            ) {
                Ok(expr) => expr,
                Err(err) => {
                    let compile_err = err.to_compile_error();
                    return quote! { #compile_err };
                }
            };

            let topic_expr = if config.tenant_scoped {
                quote! { ::std::format!("tenant:{}:{}", ::autumn_web::tenancy::DisplayTenantId::tenant_id_str(&__record_ref.tenant_id), #base_topic_expr) }
            } else {
                base_topic_expr
            };

            let default_container = format!("{}-list", config.table_name);
            let container_expr = config
                .broadcast_container
                .as_deref()
                .unwrap_or(&default_container);
            let model_prefix = to_snake_case(&config.model_name.to_string());

            let (render_expr, create_swap_id, create_swap_strategy, update_id_expr, delete_id_expr) =
                if let Some(ref render_path) = config.broadcast_render {
                    let extract_id = quote! {
                        ::autumn_web::htmx::extract_html_id(&{#render_path(__record_ref)}.into_string())
                            .unwrap_or_else(|| ::std::format!("{}-{}", #model_prefix, ::autumn_web::repository::ModelPrimaryKey::primary_key_value(__record_ref)))
                    };
                    (
                        quote! { #render_path(__record_ref) },
                        quote! { #container_expr.to_string() },
                        quote! { ::autumn_web::htmx::OobSwap::BeforeEnd },
                        extract_id.clone(),
                        extract_id,
                    )
                } else {
                    let dom_id = quote! { <#model_name as ::autumn_web::live::LiveFragment>::dom_id(__record_ref) };
                    (
                        quote! { <#model_name as ::autumn_web::live::LiveFragment>::render_fragment(__record_ref) },
                        quote! { #container_expr.to_string() },
                        quote! { <#model_name as ::autumn_web::live::LiveFragment>::insert_swap() },
                        dom_id.clone(),
                        dom_id,
                    )
                };

            let create = quote! {
                {
                    let __record_ref = &__record;
                    if let ::core::option::Option::Some(__channels) = ::autumn_web::__private::get_global_channels() {
                        let __topic = #topic_expr;
                        let __fragment = #render_expr;
                        let __create_id = #create_swap_id;
                        let __create_swap = #create_swap_strategy;
                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(&__topic, &__create_id, &__create_swap, &__fragment)
                        {
                            ::autumn_web::reexports::tracing::warn!(error = %__err, "auto-broadcast failed");
                        }
                    }
                }
            };

            let update = quote! {
                {
                    let __record_ref = &__record;
                    if let ::core::option::Option::Some(__channels) = ::autumn_web::__private::get_global_channels() {
                        let __topic = #topic_expr;
                        let __fragment = #render_expr;
                        let __id = #update_id_expr;

                        let __prev_id = __ctx_val
                            .get("__autumn_previous_id")
                            .and_then(|__v| __v.as_str());

                        let __topic_changed = __ctx_val
                            .get("__autumn_previous_topic")
                            .and_then(|__v| __v.as_str())
                            .map_or(false, |__prev_topic| __prev_topic != __topic);

                        if __topic_changed {
                            if let ::core::option::Option::Some(__prev_topic) = __ctx_val
                                .get("__autumn_previous_topic")
                                .and_then(|__v| __v.as_str())
                            {
                                let __delete_id = __prev_id.unwrap_or(&__id);
                                let __delete_fragment = ::autumn_web::html! {};
                                if let ::core::result::Result::Err(__err) = __channels
                                    .broadcast()
                                    .publish_oob(__prev_topic, __delete_id, &::autumn_web::htmx::OobSwap::Delete, &__delete_fragment)
                                {
                                    ::autumn_web::reexports::tracing::warn!(error = %__err, "auto-broadcast delete of old topic failed");
                                }
                            }
                        }

                        let (__target_id, __swap_strategy) = if __topic_changed {
                            (#container_expr, <#model_name as ::autumn_web::live::LiveFragment>::insert_swap())
                        } else {
                            let __strategy = if let ::core::option::Option::Some(__prev_id_val) = __prev_id {
                                if __prev_id_val != &__id {
                                    ::autumn_web::htmx::OobSwap::Target(
                                        ::autumn_web::htmx::OobMethod::OuterHTML,
                                        ::std::format!("#{}", __prev_id_val),
                                    )
                                } else {
                                    ::autumn_web::htmx::OobSwap::OuterHTML
                                }
                            } else {
                                ::autumn_web::htmx::OobSwap::OuterHTML
                            };
                            (__id.as_str(), __strategy)
                        };

                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(&__topic, __target_id, &__swap_strategy, &__fragment)
                        {
                            ::autumn_web::reexports::tracing::warn!(error = %__err, "auto-broadcast failed");
                        }
                    }
                }
            };

            let delete = quote! {
                {
                    let __record_ref = &__record;
                    if let ::core::option::Option::Some(__channels) = ::autumn_web::__private::get_global_channels() {
                        let __topic = #topic_expr;
                        let __id = #delete_id_expr;
                        let __fragment = ::autumn_web::html! {};
                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(&__topic, &__id, &::autumn_web::htmx::OobSwap::Delete, &__fragment)
                        {
                            ::autumn_web::reexports::tracing::warn!(error = %__err, "auto-broadcast failed");
                        }
                    }
                }
            };

            (create, update, delete)
        } else {
            (quote! {}, quote! {}, quote! {})
        };

        let (
            enqueue_context_setup,
            enqueue_context_ref,
            finalize_context_setup,
            finalize_context_ref,
        ) = if config.broadcasts {
            let base_topic_expr = match generate_topic_format(
                config
                    .broadcast_topic
                    .as_deref()
                    .unwrap_or(&config.table_name),
                &quote! { __record_ref },
            ) {
                Ok(expr) => expr,
                Err(err) => {
                    let compile_err = err.to_compile_error();
                    return quote! { #compile_err };
                }
            };

            let topic_expr = if config.tenant_scoped {
                quote! { ::std::format!("tenant:{}:{}", ::autumn_web::tenancy::DisplayTenantId::tenant_id_str(&__record_ref.tenant_id), #base_topic_expr) }
            } else {
                base_topic_expr
            };

            let prev_id_expr = if let Some(ref render_path) = config.broadcast_render {
                quote! { {
                    let __prev_fragment = #render_path(__record_ref);
                    ::autumn_web::htmx::extract_html_id(&__prev_fragment.into_string())
                } }
            } else {
                quote! { ::core::option::Option::Some(<#model_name as ::autumn_web::live::LiveFragment>::dom_id(__record_ref)) }
            };

            (
                quote! {
                    let (__autumn_previous_topic, __autumn_previous_id) = if let ::core::option::Option::Some(__record_val) = &__vh_before {
                        let __record_ref = __record_val;
                        let __prev_topic = #topic_expr;
                        let __prev_id = #prev_id_expr;
                        (::core::option::Option::Some(__prev_topic), __prev_id)
                    } else {
                        (::core::option::Option::None, ::core::option::Option::None)
                    };

                    let mut __autumn_ctx_val = ::autumn_web::reexports::serde_json::to_value(&ctx)
                        .map_err(|e| ::autumn_web::AutumnError::internal_server_error_msg(format!("serialize context: {e}")))?;

                    if let ::core::option::Option::Some(ref __prev_topic) = __autumn_previous_topic {
                        if let ::core::option::Option::Some(__map) = __autumn_ctx_val.as_object_mut() {
                            __map.insert(
                                "__autumn_previous_topic".to_string(),
                                ::autumn_web::reexports::serde_json::Value::String(__prev_topic.clone()),
                            );
                        }
                    }

                    if let ::core::option::Option::Some(ref __prev_id_val) = __autumn_previous_id {
                        if let ::core::option::Option::Some(__map) = __autumn_ctx_val.as_object_mut() {
                            __map.insert(
                                "__autumn_previous_id".to_string(),
                                ::autumn_web::reexports::serde_json::Value::String(__prev_id_val.clone()),
                            );
                        }
                    }
                },
                quote! { &__autumn_ctx_val },
                quote! {
                    let mut __autumn_finalized_ctx_val = ::autumn_web::reexports::serde_json::to_value(&ctx)
                        .map_err(|e| ::autumn_web::AutumnError::internal_server_error_msg(format!("serialize finalized context: {e}")))?;
                    if let ::core::option::Option::Some(ref __prev_topic) = __autumn_previous_topic {
                        if let ::core::option::Option::Some(__map) = __autumn_finalized_ctx_val.as_object_mut() {
                            __map.insert(
                                "__autumn_previous_topic".to_string(),
                                ::autumn_web::reexports::serde_json::Value::String(__prev_topic.clone()),
                            );
                        }
                    }
                    if let ::core::option::Option::Some(ref __prev_id) = __autumn_previous_id {
                        if let ::core::option::Option::Some(__map) = __autumn_finalized_ctx_val.as_object_mut() {
                            __map.insert(
                                "__autumn_previous_id".to_string(),
                                ::autumn_web::reexports::serde_json::Value::String(__prev_id.clone()),
                            );
                        }
                    }
                },
                quote! { &__autumn_finalized_ctx_val },
            )
        } else {
            (
                quote! {
                    let __autumn_previous_topic: ::core::option::Option<::std::string::String> = ::core::option::Option::None;
                    let __autumn_previous_id: ::core::option::Option<::std::string::String> = ::core::option::Option::None;
                },
                quote! { &ctx },
                quote! {},
                quote! { &ctx },
            )
        };

        let hook_support_methods = if commit_hooks_enabled {
            quote! {
            #[doc(hidden)]
            fn __autumn_repository_commit_hook_key() -> &'static str {
                ::core::concat!(
                    ::core::env!("CARGO_PKG_NAME"),
                    "::",
                    ::core::module_path!(),
                    "::",
                    ::core::stringify!(#table_ident),
                    "::",
                    ::core::stringify!(#model_name),
                    "::",
                    ::core::stringify!(#hooks_ident)
                )
            }

            #[doc(hidden)]
            fn __autumn_register_repository_commit_hooks() {
                static __AUTUMN_REGISTERED: ::std::sync::OnceLock<()> = ::std::sync::OnceLock::new();
                __AUTUMN_REGISTERED.get_or_init(|| {
                    ::autumn_web::__private::register_repository_commit_hook_runner(
                        Self::__autumn_repository_commit_hook_key(),
                        |__ctx, __record| async move {
                            let mut __ctx: ::autumn_web::hooks::MutationContext =
                                ::autumn_web::reexports::serde_json::from_value(__ctx)
                                    .map_err(|__error| {
                                        ::autumn_web::AutumnError::internal_server_error_msg(
                                            format!("deserialize repository create hook context: {__error}")
                                        )
                                    })?;
                            let __record: #model_name =
                                #model_name::__autumn_commit_hook_from_value(__record)?;
                            let __hooks =
                                <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default();
                            <#hooks_ident as ::autumn_web::hooks::MutationHooks>::after_create_commit(
                                &__hooks,
                                &mut __ctx,
                                &__record,
                            )
                            .await?;
                            #broadcast_create
                            Ok(())
                        },
                        |__ctx_val, __record| async move {
                            let mut __ctx: ::autumn_web::hooks::MutationContext =
                                ::autumn_web::reexports::serde_json::from_value(__ctx_val.clone())
                                    .map_err(|__error| {
                                        ::autumn_web::AutumnError::internal_server_error_msg(
                                            format!("deserialize repository update hook context: {__error}")
                                        )
                                    })?;
                            let __record: #model_name =
                                #model_name::__autumn_commit_hook_from_value(__record)?;
                            let __hooks =
                                <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default();
                            <#hooks_ident as ::autumn_web::hooks::MutationHooks>::after_update_commit(
                                &__hooks,
                                &mut __ctx,
                                &__record,
                            )
                            .await?;
                            #broadcast_update
                            Ok(())
                        },
                        |__ctx, __record| async move {
                            let mut __ctx: ::autumn_web::hooks::MutationContext =
                                ::autumn_web::reexports::serde_json::from_value(__ctx)
                                    .map_err(|__error| {
                                        ::autumn_web::AutumnError::internal_server_error_msg(
                                            format!("deserialize repository delete hook context: {__error}")
                                        )
                                    })?;
                            let __record: #model_name =
                                #model_name::__autumn_commit_hook_from_value(__record)?;
                            let __hooks =
                                <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default();
                            <#hooks_ident as ::autumn_web::hooks::MutationHooks>::after_delete_commit(
                                &__hooks,
                                &mut __ctx,
                                &__record,
                            )
                            .await?;
                            #broadcast_delete
                            Ok(())
                        },
                    );
                });
            }
            }
        } else {
            quote! {}
        };

        let hook_inventory_registration = if commit_hooks_enabled {
            quote! {
                ::autumn_web::reexports::inventory::submit! {
                    ::autumn_web::__private::RepositoryCommitHookDescriptor {
                        register: #pg_name::__autumn_register_repository_commit_hooks,
                    }
                }
            }
        } else {
            quote! {}
        };

        // ── save (hooked) ─────────────────────────────────
        // ── save (hooked) ─────────────────────────────────
        // Pre-compute version-history snippet for CREATE in commit_hooks paths.
        let vh_create_in_hooks = if config.versioned {
            let vh = vh_insert_ts(
                table_name,
                "insert",
                true,
                &quote! { record },
                None,
                &quote! { conn },
                model_name,
            );
            quote! { #vh }
        } else {
            quote! {}
        };

        let save_body = if config.tenant_scoped {
            if commit_hooks_enabled {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    let tenant_id = if self.across_tenants {
                        ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    Self::__autumn_register_repository_commit_hooks();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record) = conn
                        .transaction::<(#model_name, MutationContext, ::std::string::String, ::std::string::String, ::autumn_web::reexports::serde_json::Value), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut input = new.clone();
                                let mut ctx = MutationContext::new(MutationOp::Create);
                                let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                    ::core::option::Option::None;
                                if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                    ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                                    __autumn_commit_hook_discriminator =
                                        ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                }

                                // before_create can validate/reject/rewrite
                                self.hooks.before_create(&mut ctx, &mut input).await?;

                                let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                        .values(::autumn_web::tenancy::TenantInsertable::tenant_values(input.clone(), t))
                                        .get_result::<#model_name>(conn)
                                        .await
                                } else {
                                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                        .values(input)
                                        .get_result::<#model_name>(conn)
                                        .await
                                }
                                .map_err(::autumn_web::AutumnError::from)?;

                                #vh_create_in_hooks

                                let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                let (__autumn_commit_hook_id, __autumn_commit_hook_owner) = ::autumn_web::__private::enqueue_repository_commit_hook_pending_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "create",
                                    ctx.idempotency_key.as_deref(),
                                    __autumn_commit_hook_discriminator.as_deref(),
                                    &ctx,
                                    &__autumn_commit_hook_record,
                                )
                                .await?;

                                Ok((record, ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    let __autumn_pending_heartbeat =
                        ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                            self.pool.clone(),
                            __autumn_commit_hook_id.clone(),
                            __autumn_commit_hook_owner.clone(),
                        );
                    let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                        self.hooks.after_create(&mut ctx, &record)
                    )
                    .await;
                    match __autumn_after_create {
                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {}
                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                            let __autumn_error_message = ::std::format!("{__autumn_error}");
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                __autumn_error_message,
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                        ::core::result::Result::Err(__autumn_panic) => {
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                "after_create panicked",
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if self.idempotency.is_some() {
                                return ::core::result::Result::Err(
                                    ::autumn_web::idempotency::__cache_committed_error_response(
                                        ::autumn_web::AutumnError::internal_server_error_msg("after_create panicked")
                                    )
                                );
                            }
                            ::std::panic::resume_unwind(__autumn_panic);
                        }
                    }
                    let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                        &self.pool,
                        &__autumn_commit_hook_id,
                        &__autumn_commit_hook_owner,
                        &ctx,
                        &__autumn_commit_hook_record,
                    )
                    .await;
                    __autumn_pending_heartbeat.cancel();
                    match __autumn_finalize_result {
                        ::core::result::Result::Ok(()) => {
                            ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                        }
                        ::core::result::Result::Err(__autumn_error) => {
                            ::autumn_web::reexports::tracing::warn!(
                                hook_id = %__autumn_commit_hook_id,
                                error = %__autumn_error,
                                "failed to finalize repository create commit hook after mutation commit; failing request closed"
                            );
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                    }

                    Ok(record)
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    let tenant_id = if self.across_tenants {
                        ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx) = conn
                        .transaction::<(#model_name, MutationContext), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut input = new.clone();
                                let mut ctx = MutationContext::new(MutationOp::Create);

                                self.hooks.before_create(&mut ctx, &mut input).await?;

                                let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                        .values(::autumn_web::tenancy::TenantInsertable::tenant_values(input.clone(), t))
                                        .get_result::<#model_name>(conn)
                                        .await
                                } else {
                                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                        .values(input)
                                        .get_result::<#model_name>(conn)
                                        .await
                                }
                                .map_err(::autumn_web::AutumnError::from)?;

                                #vh_create_in_hooks

                                Ok((record, ctx))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    self.hooks.after_create(&mut ctx, &record).await?;

                    Ok(record)
                }
            }
        } else {
            if commit_hooks_enabled {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    Self::__autumn_register_repository_commit_hooks();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record) = conn
                        .transaction::<(#model_name, MutationContext, ::std::string::String, ::std::string::String, ::autumn_web::reexports::serde_json::Value), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut input = new.clone();
                                let mut ctx = MutationContext::new(MutationOp::Create);
                                let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                    ::core::option::Option::None;
                                if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                    ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                                    __autumn_commit_hook_discriminator =
                                        ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                }

                                // before_create can validate/reject/rewrite
                                self.hooks.before_create(&mut ctx, &mut input).await?;

                                let record = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(input)
                                    .get_result::<#model_name>(conn)
                                    .await
                                    .map_err(::autumn_web::AutumnError::from)?;

                                #vh_create_in_hooks

                                let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                let (__autumn_commit_hook_id, __autumn_commit_hook_owner) = ::autumn_web::__private::enqueue_repository_commit_hook_pending_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "create",
                                    ctx.idempotency_key.as_deref(),
                                    __autumn_commit_hook_discriminator.as_deref(),
                                    &ctx,
                                    &__autumn_commit_hook_record,
                                )
                                .await?;

                                Ok((record, ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    let __autumn_pending_heartbeat =
                        ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                            self.pool.clone(),
                            __autumn_commit_hook_id.clone(),
                            __autumn_commit_hook_owner.clone(),
                        );
                    let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                        self.hooks.after_create(&mut ctx, &record)
                    )
                    .await;
                    match __autumn_after_create {
                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {}
                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                            let __autumn_error_message = ::std::format!("{__autumn_error}");
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                __autumn_error_message,
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                        ::core::result::Result::Err(__autumn_panic) => {
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                "after_create panicked",
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if self.idempotency.is_some() {
                                return ::core::result::Result::Err(
                                    ::autumn_web::idempotency::__cache_committed_error_response(
                                        ::autumn_web::AutumnError::internal_server_error_msg("after_create panicked")
                                    )
                                );
                            }
                            ::std::panic::resume_unwind(__autumn_panic);
                        }
                    }
                    let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                        &self.pool,
                        &__autumn_commit_hook_id,
                        &__autumn_commit_hook_owner,
                        &ctx,
                        &__autumn_commit_hook_record,
                    )
                    .await;
                    __autumn_pending_heartbeat.cancel();
                    match __autumn_finalize_result {
                        ::core::result::Result::Ok(()) => {
                            ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                        }
                        ::core::result::Result::Err(__autumn_error) => {
                            ::autumn_web::reexports::tracing::warn!(
                                hook_id = %__autumn_commit_hook_id,
                                error = %__autumn_error,
                                "failed to finalize repository create commit hook after mutation commit; failing request closed"
                            );
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                    }

                    Ok(record)
                }
            } else if config.versioned {
                let vh_insert = vh_insert_ts(
                    table_name,
                    "insert",
                    true,
                    &quote! { record },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx) = conn
                        .transaction::<(#model_name, MutationContext), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut input = new.clone();
                                let mut ctx = MutationContext::new(MutationOp::Create);

                                self.hooks.before_create(&mut ctx, &mut input).await?;

                                let record = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(input)
                                    .get_result::<#model_name>(conn)
                                    .await
                                    .map_err(::autumn_web::AutumnError::from)?;

                                #vh_insert

                                Ok((record, ctx))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    self.hooks.after_create(&mut ctx, &record).await?;

                    Ok(record)
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx) = conn
                        .transaction::<(#model_name, MutationContext), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut input = new.clone();
                                let mut ctx = MutationContext::new(MutationOp::Create);

                                self.hooks.before_create(&mut ctx, &mut input).await?;

                                let record = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(input)
                                    .get_result::<#model_name>(conn)
                                    .await
                                    .map_err(::autumn_web::AutumnError::from)?;

                                Ok((record, ctx))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    self.hooks.after_create(&mut ctx, &record).await?;

                    Ok(record)
                }
            }
        };

        // ── update (hooked) ───────────────────────────────
        let draft_ext_trait = format_ident!("{}DraftExt", model_name);
        // Pre-compute version-history snippet for UPDATE in commit_hooks paths.
        let vh_update_in_hooks = if config.versioned {
            let vh = vh_insert_ts(
                table_name,
                "update",
                true,
                &quote! { record },
                Some(&quote! { __vh_before }),
                &quote! { conn },
                model_name,
            );
            quote! { #vh }
        } else {
            quote! {}
        };

        let update_body = if config.tenant_scoped {
            if commit_hooks_enabled {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks, UpdateDraft};
                    use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };

                    Self::__autumn_register_repository_commit_hooks();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record, __autumn_previous_topic, __autumn_previous_id) = conn
                        .transaction::<(#model_name, MutationContext, ::std::string::String, ::std::string::String, ::autumn_web::reexports::serde_json::Value, ::core::option::Option<::std::string::String>, ::core::option::Option<::std::string::String>), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Update);
                                let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                    ::core::option::Option::None;
                                if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                    ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                                    __autumn_commit_hook_discriminator =
                                        ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                }
                                let (record, __vh_before): (#model_name, ::core::option::Option<#model_name>) = if let ::core::option::Option::Some(expected_version) =
                                    changes.__autumn_lock_version_expected()
                                {
                                    let load_query = #table_ident::table.find(id);
                                    let current = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                                    } else {
                                        load_query.for_update().first::<#model_name>(conn).await
                                    }
                                    .optional()
                                    .map_err(::autumn_web::AutumnError::from)?
                                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                        format!("{} with id {} not found", stringify!(#model_name), id)
                                    ))?;

                                    if let ::core::option::Option::Some(actual_version) =
                                        current.__autumn_lock_version_actual()
                                    {
                                        if actual_version != expected_version {
                                            return Err(::autumn_web::AutumnError::conflict(
                                                ::autumn_web::RepositoryError::Conflict {
                                                    id,
                                                    expected_version,
                                                    actual_version: ::core::option::Option::Some(actual_version),
                                                },
                                            ));
                                        }
                                    }

                                    let __vh_before_inner = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }

                                    let proposed = draft.into_after();
                                    let update_target = #table_ident::table.find(id);
                                    let updated = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    } else {
                                        ::autumn_web::reexports::diesel::update(update_target)
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    }
                                    .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, ::core::option::Option::Some(__vh_before_inner))
                                } else {
                                    let load_query = #table_ident::table.find(id);
                                    let current = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                                    } else {
                                        load_query.for_update().first::<#model_name>(conn).await
                                    }
                                    .optional()
                                    .map_err(::autumn_web::AutumnError::from)?
                                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                        format!("{} with id {} not found", stringify!(#model_name), id)
                                    ))?;

                                    let __vh_before_inner = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }

                                    let proposed = draft.into_after();
                                    let update_target = #table_ident::table.find(id);
                                    let updated = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    } else {
                                        ::autumn_web::reexports::diesel::update(update_target)
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    }
                                    .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, ::core::option::Option::Some(__vh_before_inner))
                                };

                                if let ::core::option::Option::Some(ref __vh_before) = __vh_before {
                                    #vh_update_in_hooks
                                }

                                #enqueue_context_setup
                                let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                let (__autumn_commit_hook_id, __autumn_commit_hook_owner) = ::autumn_web::__private::enqueue_repository_commit_hook_pending_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "update",
                                    ctx.idempotency_key.as_deref(),
                                    __autumn_commit_hook_discriminator.as_deref(),
                                    #enqueue_context_ref,
                                    &__autumn_commit_hook_record,
                                )
                                .await?;

                                Ok((record, ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record, __autumn_previous_topic, __autumn_previous_id))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    let __autumn_pending_heartbeat =
                        ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                            self.pool.clone(),
                            __autumn_commit_hook_id.clone(),
                            __autumn_commit_hook_owner.clone(),
                        );
                    let __autumn_after_update = ::autumn_web::__private::catch_repository_after_hook_unwind(
                        self.hooks.after_update(&mut ctx, &record)
                    )
                    .await;
                    match __autumn_after_update {
                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {}
                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                            let __autumn_error_message = ::std::format!("{__autumn_error}");
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                __autumn_error_message,
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                        ::core::result::Result::Err(__autumn_panic) => {
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                "after_update panicked",
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if self.idempotency.is_some() {
                                return ::core::result::Result::Err(
                                    ::autumn_web::idempotency::__cache_committed_error_response(
                                        ::autumn_web::AutumnError::internal_server_error_msg("after_update panicked")
                                    )
                                );
                            }
                            ::std::panic::resume_unwind(__autumn_panic);
                        }
                    }
                    #finalize_context_setup
                    let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                        &self.pool,
                        &__autumn_commit_hook_id,
                        &__autumn_commit_hook_owner,
                        #finalize_context_ref,
                        &__autumn_commit_hook_record,
                    )
                    .await;
                    __autumn_pending_heartbeat.cancel();
                    match __autumn_finalize_result {
                        ::core::result::Result::Ok(()) => {
                            ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                        }
                        ::core::result::Result::Err(__autumn_error) => {
                            ::autumn_web::reexports::tracing::warn!(
                                hook_id = %__autumn_commit_hook_id,
                                error = %__autumn_error,
                                "failed to finalize repository update commit hook after mutation commit; failing request closed"
                            );
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                    }

                    Ok(record)
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks, UpdateDraft};
                    use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx) = conn
                        .transaction::<(#model_name, MutationContext), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Update);
                                let (record, __vh_before): (#model_name, ::core::option::Option<#model_name>) = if let ::core::option::Option::Some(expected_version) =
                                    changes.__autumn_lock_version_expected()
                                {
                                    let load_query = #table_ident::table.find(id);
                                    let current = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                                    } else {
                                        load_query.for_update().first::<#model_name>(conn).await
                                    }
                                    .optional()
                                    .map_err(::autumn_web::AutumnError::from)?
                                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                        format!("{} with id {} not found", stringify!(#model_name), id)
                                    ))?;

                                    if let ::core::option::Option::Some(actual_version) =
                                        current.__autumn_lock_version_actual()
                                    {
                                        if actual_version != expected_version {
                                            return Err(::autumn_web::AutumnError::conflict(
                                                ::autumn_web::RepositoryError::Conflict {
                                                    id,
                                                    expected_version,
                                                    actual_version: ::core::option::Option::Some(actual_version),
                                                },
                                            ));
                                        }
                                    }

                                    let __vh_before_inner = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }

                                    let proposed = draft.into_after();
                                    let update_target = #table_ident::table.find(id);
                                    let updated = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    } else {
                                        ::autumn_web::reexports::diesel::update(update_target)
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    }
                                    .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, ::core::option::Option::Some(__vh_before_inner))
                                } else {
                                    let load_query = #table_ident::table.find(id);
                                    let current = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                                    } else {
                                        load_query.for_update().first::<#model_name>(conn).await
                                    }
                                    .optional()
                                    .map_err(::autumn_web::AutumnError::from)?
                                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                        format!("{} with id {} not found", stringify!(#model_name), id)
                                    ))?;

                                    let __vh_before_inner = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;
                                    if let ::core::option::Option::Some(ref t) = tenant_id {
                                        draft.after.tenant_id = t.clone();
                                    }

                                    let proposed = draft.into_after();
                                    let update_target = #table_ident::table.find(id);
                                    let updated = if let ::core::option::Option::Some(ref t) = tenant_id {
                                        ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    } else {
                                        ::autumn_web::reexports::diesel::update(update_target)
                                            .set(proposed.clone())
                                            .get_result::<#model_name>(conn)
                                            .await
                                    }
                                    .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, ::core::option::Option::Some(__vh_before_inner))
                                };

                                if let ::core::option::Option::Some(ref __vh_before) = __vh_before {
                                    #vh_update_in_hooks
                                }

                                Ok((record, ctx))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    self.hooks.after_update(&mut ctx, &record).await?;

                    Ok(record)
                }
            }
        } else {
            if commit_hooks_enabled {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks, UpdateDraft};
                    use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                    Self::__autumn_register_repository_commit_hooks();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record, __autumn_previous_topic, __autumn_previous_id) = conn
                        .transaction::<(#model_name, MutationContext, ::std::string::String, ::std::string::String, ::autumn_web::reexports::serde_json::Value, ::core::option::Option<::std::string::String>, ::core::option::Option<::std::string::String>), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Update);
                                let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                    ::core::option::Option::None;
                                if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                    ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                                    __autumn_commit_hook_discriminator =
                                        ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                }
                                let (record, __vh_before): (#model_name, ::core::option::Option<#model_name>) = if let ::core::option::Option::Some(expected_version) =
                                    changes.__autumn_lock_version_expected()
                                {
                                    // SELECT FOR UPDATE grabs an exclusive row lock so
                                    // no concurrent writer can commit between our
                                    // version check and the UPDATE below.
                                    let current = #table_ident::table
                                        .find(id)
                                        .for_update()
                                        .first::<#model_name>(conn)
                                        .await
                                        .optional()
                                        .map_err(::autumn_web::AutumnError::from)?
                                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                            format!("{} with id {} not found", stringify!(#model_name), id)
                                        ))?;

                                    if let ::core::option::Option::Some(actual_version) =
                                        current.__autumn_lock_version_actual()
                                    {
                                        if actual_version != expected_version {
                                            return Err(::autumn_web::AutumnError::conflict(
                                                ::autumn_web::RepositoryError::Conflict {
                                                    id,
                                                    expected_version,
                                                    actual_version: ::core::option::Option::Some(actual_version),
                                                },
                                            ));
                                        }
                                    }

                                    let __vh_before_inner = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;

                                    let proposed = draft.into_after();
                                    let updated = ::autumn_web::reexports::diesel::update(#table_ident::table.find(id))
                                        .set(proposed.clone())
                                        .get_result::<#model_name>(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, ::core::option::Option::Some(__vh_before_inner))
                                } else {
                                    // Load current record
                                    let current = #table_ident::table
                                        .find(id)
                                        .for_update()
                                        .first::<#model_name>(conn)
                                        .await
                                        .optional()
                                        .map_err(::autumn_web::AutumnError::from)?
                                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                            format!("{} with id {} not found", stringify!(#model_name), id)
                                        ))?;

                                    let __vh_before_inner = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;

                                    let proposed = draft.into_after();
                                    let updated = ::autumn_web::reexports::diesel::update(#table_ident::table.find(id))
                                        .set(proposed.clone())
                                        .get_result::<#model_name>(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, ::core::option::Option::Some(__vh_before_inner))
                                };

                                if let ::core::option::Option::Some(ref __vh_before) = __vh_before {
                                    #vh_update_in_hooks
                                }

                                #enqueue_context_setup
                                let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                let (__autumn_commit_hook_id, __autumn_commit_hook_owner) = ::autumn_web::__private::enqueue_repository_commit_hook_pending_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "update",
                                    ctx.idempotency_key.as_deref(),
                                    __autumn_commit_hook_discriminator.as_deref(),
                                    #enqueue_context_ref,
                                    &__autumn_commit_hook_record,
                                )
                                .await?;

                                Ok((record, ctx, __autumn_commit_hook_id, __autumn_commit_hook_owner, __autumn_commit_hook_record, __autumn_previous_topic, __autumn_previous_id))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    let __autumn_pending_heartbeat =
                        ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                            self.pool.clone(),
                            __autumn_commit_hook_id.clone(),
                            __autumn_commit_hook_owner.clone(),
                        );
                    let __autumn_after_update = ::autumn_web::__private::catch_repository_after_hook_unwind(
                        self.hooks.after_update(&mut ctx, &record)
                    )
                    .await;
                    match __autumn_after_update {
                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {}
                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                            let __autumn_error_message = ::std::format!("{__autumn_error}");
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                __autumn_error_message,
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                        ::core::result::Result::Err(__autumn_panic) => {
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                &__autumn_commit_hook_id,
                                &__autumn_commit_hook_owner,
                                "after_update panicked",
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if self.idempotency.is_some() {
                                return ::core::result::Result::Err(
                                    ::autumn_web::idempotency::__cache_committed_error_response(
                                        ::autumn_web::AutumnError::internal_server_error_msg("after_update panicked")
                                    )
                                );
                            }
                            ::std::panic::resume_unwind(__autumn_panic);
                        }
                    }
                    #finalize_context_setup
                    let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                        &self.pool,
                        &__autumn_commit_hook_id,
                        &__autumn_commit_hook_owner,
                        #finalize_context_ref,
                        &__autumn_commit_hook_record,
                    )
                    .await;
                    __autumn_pending_heartbeat.cancel();
                    match __autumn_finalize_result {
                        ::core::result::Result::Ok(()) => {
                            ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                        }
                        ::core::result::Result::Err(__autumn_error) => {
                            ::autumn_web::reexports::tracing::warn!(
                                hook_id = %__autumn_commit_hook_id,
                                error = %__autumn_error,
                                "failed to finalize repository update commit hook after mutation commit; failing request closed"
                            );
                            return ::core::result::Result::Err(
                                ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                            );
                        }
                    }

                    Ok(record)
                }
            } else if config.versioned {
                let vh_insert = vh_insert_ts(
                    table_name,
                    "update",
                    true,
                    &quote! { record },
                    Some(&quote! { __vh_before }),
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks, UpdateDraft};
                    use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx) = conn
                        .transaction::<(#model_name, MutationContext), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Update);
                                let (record, __vh_before): (#model_name, #model_name) = if let ::core::option::Option::Some(expected_version) =
                                    changes.__autumn_lock_version_expected()
                                {
                                    let current = #table_ident::table
                                        .find(id)
                                        .for_update()
                                        .first::<#model_name>(conn)
                                        .await
                                        .optional()
                                        .map_err(::autumn_web::AutumnError::from)?
                                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                            format!("{} with id {} not found", stringify!(#model_name), id)
                                        ))?;

                                    if let ::core::option::Option::Some(actual_version) =
                                        current.__autumn_lock_version_actual()
                                    {
                                        if actual_version != expected_version {
                                            return Err(::autumn_web::AutumnError::conflict(
                                                ::autumn_web::RepositoryError::Conflict {
                                                    id,
                                                    expected_version,
                                                    actual_version: ::core::option::Option::Some(actual_version),
                                                },
                                            ));
                                        }
                                    }

                                    let __vh_before = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;

                                    let proposed = draft.into_after();
                                    let updated = ::autumn_web::reexports::diesel::update(#table_ident::table.find(id))
                                        .set(proposed.clone())
                                        .get_result::<#model_name>(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, __vh_before)
                                } else {
                                    let current = #table_ident::table
                                        .find(id)
                                        .first::<#model_name>(conn)
                                        .await
                                        .optional()
                                        .map_err(::autumn_web::AutumnError::from)?
                                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                            format!("{} with id {} not found", stringify!(#model_name), id)
                                        ))?;

                                    let __vh_before = current.clone();
                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;

                                    let proposed = draft.into_after();
                                    let updated = ::autumn_web::reexports::diesel::update(#table_ident::table.find(id))
                                        .set(proposed.clone())
                                        .get_result::<#model_name>(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?;
                                    (updated, __vh_before)
                                };

                                #vh_insert

                                Ok((record, ctx))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    self.hooks.after_update(&mut ctx, &record).await?;

                    Ok(record)
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks, UpdateDraft};
                    use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let (record, mut ctx) = conn
                        .transaction::<(#model_name, MutationContext), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Update);
                                let record: #model_name = if let ::core::option::Option::Some(expected_version) =
                                    changes.__autumn_lock_version_expected()
                                {
                                    let current = #table_ident::table
                                        .find(id)
                                        .for_update()
                                        .first::<#model_name>(conn)
                                        .await
                                        .optional()
                                        .map_err(::autumn_web::AutumnError::from)?
                                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                            format!("{} with id {} not found", stringify!(#model_name), id)
                                        ))?;

                                    if let ::core::option::Option::Some(actual_version) =
                                        current.__autumn_lock_version_actual()
                                    {
                                        if actual_version != expected_version {
                                            return Err(::autumn_web::AutumnError::conflict(
                                                ::autumn_web::RepositoryError::Conflict {
                                                    id,
                                                    expected_version,
                                                    actual_version: ::core::option::Option::Some(actual_version),
                                                },
                                            ));
                                        }
                                    }

                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;

                                    let proposed = draft.into_after();
                                    ::autumn_web::reexports::diesel::update(#table_ident::table.find(id))
                                        .set(proposed.clone())
                                        .get_result::<#model_name>(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?
                                } else {
                                    let current = #table_ident::table
                                        .find(id)
                                        .first::<#model_name>(conn)
                                        .await
                                        .optional()
                                        .map_err(::autumn_web::AutumnError::from)?
                                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                            format!("{} with id {} not found", stringify!(#model_name), id)
                                        ))?;

                                    let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(&current, changes)?;
                                    self.hooks.before_update(&mut ctx, &mut draft).await?;

                                    let proposed = draft.into_after();
                                    ::autumn_web::reexports::diesel::update(#table_ident::table.find(id))
                                        .set(proposed.clone())
                                        .get_result::<#model_name>(conn)
                                        .await
                                        .map_err(::autumn_web::AutumnError::from)?
                                };

                                Ok((record, ctx))
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    self.hooks.after_update(&mut ctx, &record).await?;

                    Ok(record)
                }
            }
        };

        // ── delete (hooked) ───────────────────────────────
        //
        // The core mutation differs for soft-delete repositories:
        // - hard delete: `DELETE FROM table WHERE id = $1`
        // - soft delete: `UPDATE table SET deleted_at = now() WHERE id = $1`
        // Both paths still fire before_delete / after_delete_commit hooks.
        let hooked_delete_mutation_stmt = if config.soft_delete {
            quote! {
                let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();
                let __autumn_deleted = ::autumn_web::reexports::diesel::update(
                    #table_ident::table.find(id).filter(#table_ident::deleted_at.is_null())
                )
                    .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                    .execute(conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                if __autumn_deleted == 0 {
                    return Err(::autumn_web::AutumnError::not_found_msg(
                        format!("{} with id {} not found", stringify!(#model_name), id)
                    ));
                }
            }
        } else {
            quote! {
                let __autumn_deleted = ::autumn_web::reexports::diesel::delete(#table_ident::table.find(id))
                    .execute(conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                if __autumn_deleted == 0 {
                    return Err(::autumn_web::AutumnError::not_found_msg(
                        format!("{} with id {} not found", stringify!(#model_name), id)
                    ));
                }
            }
        };

        // Pre-compute version-history snippets for DELETE in commit_hooks and no-hooks paths.
        let vh_delete_in_hooks = if config.versioned {
            let vh = vh_insert_ts(
                table_name,
                "delete",
                true,
                &quote! { record },
                None,
                &quote! { conn },
                model_name,
            );
            quote! { #vh }
        } else {
            quote! {}
        };

        let delete_body = if config.tenant_scoped {
            let tenant_id_setup = quote! {
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
            };
            if commit_hooks_enabled {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    #tenant_id_setup
                    Self::__autumn_register_repository_commit_hooks();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    conn
                        .transaction::<(), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Delete);
                                let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                    ::core::option::Option::None;
                                if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                    ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                                    __autumn_commit_hook_discriminator =
                                        ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                }

                                // Load current record for before_delete context.
                                // Apply the same soft-delete predicate as the mutation so
                                // hooks only run when the row is actually deletable.
                                let load_query = #table_ident::table.find(id) #sd_filter;
                                let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                                    load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                                } else {
                                    load_query.for_update().first::<#model_name>(conn).await
                                }
                                .optional()
                                .map_err(::autumn_web::AutumnError::from)?
                                .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                    format!("{} with id {} not found", stringify!(#model_name), id)
                                ))?;

                                self.hooks.before_delete(&mut ctx, &record).await?;

                                #hooked_delete_mutation_stmt

                                #vh_delete_in_hooks

                                let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                ::autumn_web::__private::enqueue_repository_commit_hook_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "delete",
                                    ctx.idempotency_key.as_deref(),
                                    __autumn_commit_hook_discriminator.as_deref(),
                                    &ctx,
                                    &__autumn_commit_hook_record,
                                )
                                .await?;

                                Ok(())
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);
                    ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);

                    Ok(())
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_conn().await?;
                    conn
                        .transaction::<(), ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let mut ctx = MutationContext::new(MutationOp::Delete);

                                let load_query = #table_ident::table.find(id);
                                let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                                    load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                                } else {
                                    load_query.for_update().first::<#model_name>(conn).await
                                }
                                .optional()
                                .map_err(::autumn_web::AutumnError::from)?
                                .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                    format!("{} with id {} not found", stringify!(#model_name), id)
                                ))?;

                                self.hooks.before_delete(&mut ctx, &record).await?;

                                #hooked_delete_mutation_stmt

                                #vh_delete_in_hooks

                                Ok(())
                            }
                            .scope_boxed()
                        })
                        .await?;
                    ::core::mem::drop(conn);

                    Ok(())
                }
            }
        } else if commit_hooks_enabled {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                Self::__autumn_register_repository_commit_hooks();
                let mut conn = self.__autumn_acquire_conn().await?;
                conn
                    .transaction::<(), ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let mut ctx = MutationContext::new(MutationOp::Delete);
                            let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                ::core::option::Option::None;
                            if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                                __autumn_commit_hook_discriminator =
                                    ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                            }

                            // Load current record for before_delete context.
                            let load_query = #table_ident::table.find(id) #sd_filter;
                            let record = load_query.for_update().first::<#model_name>(conn).await
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;

                            self.hooks.before_delete(&mut ctx, &record).await?;

                            #hooked_delete_mutation_stmt

                            #vh_delete_in_hooks

                            let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                            ::autumn_web::__private::enqueue_repository_commit_hook_on_conn(
                                conn,
                                Self::__autumn_repository_commit_hook_key(),
                                "delete",
                                ctx.idempotency_key.as_deref(),
                                __autumn_commit_hook_discriminator.as_deref(),
                                &ctx,
                                &__autumn_commit_hook_record,
                            )
                            .await?;

                            Ok(())
                        }
                        .scope_boxed()
                    })
                    .await?;
                ::core::mem::drop(conn);
                ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);

                Ok(())
            }
        } else if config.versioned {
            let vh_insert = vh_insert_ts(
                table_name,
                "delete",
                true,
                &quote! { record },
                None,
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                let mut conn = self.__autumn_acquire_conn().await?;
                conn
                    .transaction::<(), ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let mut ctx = MutationContext::new(MutationOp::Delete);

                            let load_query = #table_ident::table.find(id);
                            let record = load_query.for_update().first::<#model_name>(conn).await
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;

                            self.hooks.before_delete(&mut ctx, &record).await?;

                            #hooked_delete_mutation_stmt

                            #vh_insert

                            Ok(())
                        }
                        .scope_boxed()
                    })
                    .await?;
                ::core::mem::drop(conn);

                Ok(())
            }
        } else {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                let mut conn = self.__autumn_acquire_conn().await?;
                conn
                    .transaction::<(), ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let mut ctx = MutationContext::new(MutationOp::Delete);

                            let load_query = #table_ident::table.find(id);
                            let record = load_query.for_update().first::<#model_name>(conn).await
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;

                            self.hooks.before_delete(&mut ctx, &record).await?;

                            #hooked_delete_mutation_stmt

                            Ok(())
                        }
                        .scope_boxed()
                    })
                    .await?;
                ::core::mem::drop(conn);

                Ok(())
            }
        };

        let save_many_body = {
            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            let insert_expr = if config.tenant_scoped {
                quote! {
                    {
                        if let ::core::option::Option::Some(t) = tenant_id {
                            let values: Vec<_> = chunk.iter().cloned().map(|item| ::autumn_web::tenancy::TenantInsertable::tenant_values(item, t)).collect();
                            ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                .values(values)
                                .get_results::<#model_name>(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                .values(chunk.to_vec())
                                .get_results::<#model_name>(conn)
                                .await
                        }
                    }
                }
            } else {
                quote! {
                    {
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(chunk.to_vec())
                            .get_results::<#model_name>(conn)
                            .await
                    }
                }
            };

            let vh_create_many_in_hooks = if config.versioned {
                let vh = vh_insert_ts(
                    table_name,
                    "insert",
                    true,
                    &quote! { record },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    for (idx, record) in chunk_inserted.iter().enumerate() {
                        let global_idx = offset + idx;
                        let ctx = &contexts_ref[global_idx];
                        #vh
                    }
                }
            } else {
                quote! {}
            };

            if commit_hooks_enabled {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};
                    use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                    use ::autumn_web::repository::AutumnColumnCountFallback as _;
                    use ::autumn_web::repository::AutumnCorrelateExt as _;

                    if new.is_empty() {
                        return Ok(Vec::new());
                    }

                    #tenant_id_setup
                    Self::__autumn_register_repository_commit_hooks();

                    let mut inputs = new.to_vec();
                    let mut contexts = Vec::new();
                    for input in &mut inputs {
                        let mut ctx = MutationContext::new(MutationOp::Create);
                        let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                            ::core::option::Option::None;
                        if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                            ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                        }
                        self.hooks.before_create(&mut ctx, input).await?;
                        contexts.push(ctx);
                    }

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let contexts_ref = &contexts;
                    let (inserted_records, hook_infos, global_indices) = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let mut inserted_records = Vec::new();
                            let mut hook_infos = Vec::new();
                            let mut global_indices = Vec::new();
                            let mut offset = 0;
                            let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                            let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                            for chunk in inputs.chunks(chunk_size) {
                                let chunk_inserted = (#insert_expr)
                                    .map_err(::autumn_web::AutumnError::from)?;

                                #vh_create_many_in_hooks

                                let mut hook_records = Vec::new();
                                for record in &chunk_inserted {
                                    let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                        ::core::option::Option::None;
                                    if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                        __autumn_commit_hook_discriminator =
                                            ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                    }
                                    let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                    hook_records.push((__autumn_commit_hook_record, __autumn_commit_hook_discriminator));
                                }

                                let mapped_indices: Vec<usize> = (0..chunk_inserted.len()).collect();

                                for &mapped_idx in &mapped_indices {
                                    global_indices.push(offset + mapped_idx);
                                }

                                let hook_inputs: Vec<_> = chunk_inserted.iter().enumerate().map(|(idx, _)| {
                                    let mapped_idx = idx;
                                    let global_idx = offset + mapped_idx;
                                    let ctx = &contexts_ref[global_idx];
                                    let (ref record_val, ref discriminator) = hook_records[idx];
                                    (
                                        ctx.idempotency_key.clone(),
                                        discriminator.clone(),
                                        ctx,
                                        record_val,
                                    )
                                }).collect();


                                let chunk_hook_infos = ::autumn_web::__private::enqueue_repository_commit_hooks_pending_bulk_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "create",
                                    &hook_inputs,
                                )
                                .await?;

                                for (idx, info) in chunk_hook_infos.into_iter().enumerate() {
                                    hook_infos.push((info.0, info.1, hook_records[idx].0.clone()));
                                }

                                inserted_records.extend(chunk_inserted);
                                offset += chunk.len();
                            }
                            Ok((inserted_records, hook_infos, global_indices))
                        }
                        .scope_boxed()
                    })
                    .await?;

                    ::core::mem::drop(conn);

                    let mut __autumn_first_err: ::core::option::Option<::autumn_web::AutumnError> = ::core::option::Option::None;
                    let mut __autumn_first_panic: ::core::option::Option<::std::boxed::Box<dyn ::core::any::Any + ::core::marker::Send>> = ::core::option::Option::None;

                    // Run after_create hooks outside of transaction
                    for (idx, record) in inserted_records.iter().enumerate() {
                        let global_idx = global_indices[idx];
                        let mut ctx = contexts[global_idx].clone();
                        let (hook_id, hook_owner, hook_record) = &hook_infos[idx];

                        let __autumn_pending_heartbeat =
                            ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                                self.pool.clone(),
                                hook_id.clone(),
                                hook_owner.clone(),
                            );
                        let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                            self.hooks.after_create(&mut ctx, record)
                        )
                        .await;
                        match __autumn_after_create {
                            ::core::result::Result::Ok(::core::result::Result::Ok(())) => {
                                let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                                    &self.pool,
                                    hook_id,
                                    hook_owner,
                                    &ctx,
                                    hook_record,
                                )
                                .await;
                                __autumn_pending_heartbeat.cancel();
                                if let ::core::result::Result::Err(__autumn_error) = __autumn_finalize_result {
                                    ::autumn_web::reexports::tracing::warn!(
                                        hook_id = %hook_id,
                                        error = %__autumn_error,
                                        "failed to finalize repository create commit hook after mutation commit; failing request closed"
                                    );
                                    if __autumn_first_err.is_none() {
                                        __autumn_first_err = ::core::option::Option::Some(
                                            ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                                        );
                                    }
                                }
                            }
                            ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                                let __autumn_error_message = ::std::format!("{__autumn_error}");
                                ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                    &self.pool,
                                    hook_id,
                                    hook_owner,
                                    __autumn_error_message,
                                )
                                .await;
                                __autumn_pending_heartbeat.cancel();
                                if __autumn_first_err.is_none() {
                                    __autumn_first_err = ::core::option::Option::Some(
                                        ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                                    );
                                }
                            }
                            ::core::result::Result::Err(__autumn_panic) => {
                                ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                    &self.pool,
                                    hook_id,
                                    hook_owner,
                                    "after_create panicked",
                                )
                                .await;
                                __autumn_pending_heartbeat.cancel();
                                if __autumn_first_panic.is_none() {
                                    __autumn_first_panic = ::core::option::Option::Some(__autumn_panic);
                                }
                            }
                        }
                    }

                    if #commit_hooks_enabled {
                        ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                    }

                    if let ::core::option::Option::Some(err) = __autumn_first_err {
                        return ::core::result::Result::Err(err);
                    }
                    if let ::core::option::Option::Some(panic_val) = __autumn_first_panic {
                        ::std::panic::resume_unwind(panic_val);
                    }

                    Ok(inserted_records)
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};
                    use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                    use ::autumn_web::repository::AutumnColumnCountFallback as _;
                    use ::autumn_web::repository::AutumnCorrelateExt as _;

                    if new.is_empty() {
                        return Ok(Vec::new());
                    }

                    #tenant_id_setup

                    let mut inputs = new.to_vec();
                    let mut contexts = Vec::new();
                    for input in &mut inputs {
                        let mut ctx = MutationContext::new(MutationOp::Create);
                        self.hooks.before_create(&mut ctx, input).await?;
                        contexts.push(ctx);
                    }

                    let mut conn = self.__autumn_acquire_conn().await?;
                    let contexts_ref = &contexts;
                    let inputs_ref = &inputs;
                    let inserted_records = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let mut inserted = Vec::new();
                            let mut offset = 0;
                            let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                            let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                            for chunk in inputs_ref.chunks(chunk_size) {
                                let chunk_inserted = (#insert_expr)
                                    .map_err(::autumn_web::AutumnError::from)?;
                                #vh_create_many_in_hooks
                                inserted.extend(chunk_inserted);
                                offset += chunk.len();
                            }
                            Ok(inserted)
                        }
                        .scope_boxed()
                    })
                    .await?;

                    ::core::mem::drop(conn);

                    let mapped_indices: Vec<usize> = (0..inserted_records.len()).collect();

                    let mut __autumn_first_err: ::core::option::Option<::autumn_web::AutumnError> = ::core::option::Option::None;
                    // Run after_create hooks outside of transaction
                    for (idx, record) in inserted_records.iter().enumerate() {
                        let orig_idx = mapped_indices[idx];
                        let mut ctx = contexts[orig_idx].clone();
                        if let ::core::result::Result::Err(err) = self.hooks.after_create(&mut ctx, record).await {
                            if __autumn_first_err.is_none() {
                                __autumn_first_err = ::core::option::Option::Some(err);
                            }
                        }
                    }
                    if let ::core::option::Option::Some(err) = __autumn_first_err {
                        return ::core::result::Result::Err(err);
                    }

                    Ok(inserted_records)
                }
            }
        };

        let save_many_skip_invalid_body = {
            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            let insert_expr = if config.tenant_scoped {
                quote! {
                    {
                        if let ::core::option::Option::Some(t) = tenant_id {
                            let values: Vec<_> = chunk.iter().map(|item| ::autumn_web::tenancy::TenantInsertable::tenant_values(item.0.clone(), t)).collect();
                            ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                .values(values)
                                .get_results::<#model_name>(conn)
                                .await
                        } else {
                            let values: Vec<_> = chunk.iter().map(|item| item.0.clone()).collect();
                            ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                .values(values)
                                .get_results::<#model_name>(conn)
                                .await
                        }
                    }
                }
            } else {
                quote! {
                    {
                        let values: Vec<_> = chunk.iter().map(|item| item.0.clone()).collect();
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(values)
                            .get_results::<#model_name>(conn)
                            .await
                    }
                }
            };

            let row_insert_expr = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        let values = ::autumn_web::tenancy::TenantInsertable::tenant_values(item.0.clone(), t);
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(values)
                            .get_result::<#model_name>(conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(item.0.clone())
                            .get_result::<#model_name>(conn)
                            .await
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                        .values(item.0.clone())
                        .get_result::<#model_name>(conn)
                        .await
                }
            };

            let idempotency_setup = if commit_hooks_enabled {
                quote! {
                    if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                        ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                    }
                }
            } else {
                quote! {}
            };

            let register_commit_hooks = if commit_hooks_enabled {
                quote! { Self::__autumn_register_repository_commit_hooks(); }
            } else {
                quote! {}
            };

            let skip_invalid_impl = if commit_hooks_enabled {
                quote! {
                    if valid_items.is_empty() {
                        return Ok((successes, failures));
                    }
                    let cols = (&valid_items[0].0).__autumn_column_count() + #tenant_extra;
                    let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                    let mut offset = 0;
                    for chunk in valid_items.chunks(chunk_size) {
                        let batch_res = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                let chunk_inserted = (#insert_expr)
                                    .map_err(::autumn_web::AutumnError::from)?;

                                let mut hook_records = Vec::new();
                                for record in &chunk_inserted {
                                    let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                        ::core::option::Option::None;
                                    if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                        __autumn_commit_hook_discriminator =
                                            ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                    }
                                    let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                    hook_records.push((__autumn_commit_hook_record, __autumn_commit_hook_discriminator));
                                }

                                let mapped_indices: Vec<usize> = (0..chunk_inserted.len()).collect();

                                let hook_inputs: Vec<_> = chunk_inserted.iter().enumerate().map(|(idx, _)| {
                                    let mapped_idx = idx;
                                    let ctx = &chunk[mapped_idx].1;
                                    let (ref record_val, ref discriminator) = hook_records[idx];
                                    (
                                        ctx.idempotency_key.clone(),
                                        discriminator.clone(),
                                        ctx,
                                        record_val,
                                    )
                                }).collect();

                                let chunk_hook_infos = ::autumn_web::__private::enqueue_repository_commit_hooks_pending_bulk_on_conn(
                                    conn,
                                    Self::__autumn_repository_commit_hook_key(),
                                    "create",
                                    &hook_inputs,
                                )
                                .await?;

                                let mut hook_infos = Vec::new();
                                for (idx, info) in chunk_hook_infos.into_iter().enumerate() {
                                    hook_infos.push((info.0, info.1, hook_records[idx].0.clone()));
                                }

                                Ok((chunk_inserted, hook_infos, mapped_indices))
                            }
                            .scope_boxed()
                        })
                        .await;

                        match batch_res {
                            Ok((inserted_chunk, hook_infos, mapped_indices)) => {
                                for (idx, record) in inserted_chunk.into_iter().enumerate() {
                                    let mapped_idx = mapped_indices[idx];
                                    let mut ctx = chunk[mapped_idx].1.clone();
                                    let (hook_id, hook_owner, hook_record) = &hook_infos[idx];


                                    let __autumn_pending_heartbeat =
                                        ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                                            self.pool.clone(),
                                            hook_id.clone(),
                                            hook_owner.clone(),
                                        );
                                    let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                                        self.hooks.after_create(&mut ctx, &record)
                                    )
                                    .await;

                                    match __autumn_after_create {
                                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {
                                            let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                                                &self.pool,
                                                hook_id,
                                                hook_owner,
                                                &ctx,
                                                hook_record,
                                            )
                                            .await;
                                            __autumn_pending_heartbeat.cancel();
                                            if let ::core::result::Result::Err(__autumn_error) = __autumn_finalize_result {
                                                ::autumn_web::reexports::tracing::warn!(
                                                    hook_id = %hook_id,
                                                    error = %__autumn_error,
                                                    "failed to finalize repository create commit hook after mutation commit"
                                                );
                                                failures.push((chunk[mapped_idx].2, __autumn_error));
                                            } else {
                                                successes.push(record);
                                            }
                                        }
                                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                                            let __autumn_error_message = ::std::format!("{__autumn_error}");
                                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                                &self.pool,
                                                hook_id,
                                                hook_owner,
                                                __autumn_error_message,
                                            )
                                            .await;
                                            __autumn_pending_heartbeat.cancel();
                                            ::autumn_web::reexports::tracing::warn!(
                                                hook_id = %hook_id,
                                                error = %__autumn_error,
                                                "after_create hook failed during skip-invalid inserts"
                                            );
                                            failures.push((chunk[mapped_idx].2, __autumn_error));
                                        }
                                        ::core::result::Result::Err(__autumn_panic) => {
                                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                                &self.pool,
                                                hook_id,
                                                hook_owner,
                                                "after_create panicked",
                                            )
                                            .await;
                                            __autumn_pending_heartbeat.cancel();
                                            ::autumn_web::reexports::tracing::warn!(
                                                hook_id = %hook_id,
                                                "after_create hook panicked during skip-invalid inserts"
                                            );
                                            failures.push((chunk[mapped_idx].2, ::autumn_web::AutumnError::internal_server_error_msg("after_create hook panicked")));
                                        }
                                    }
                                }
                            }
                            Err(batch_err) => {
                                let is_constraint_error = if let ::core::option::Option::Some(diesel_err) = batch_err.downcast_ref::<::autumn_web::reexports::diesel::result::Error>() {
                                    match diesel_err {
                                        ::autumn_web::reexports::diesel::result::Error::DatabaseError(kind, _) => {
                                            match kind {
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::UniqueViolation |
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::ForeignKeyViolation |
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::NotNullViolation |
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::CheckViolation => true,
                                                _ => false,
                                            }
                                        }
                                        _ => false,
                                    }
                                } else {
                                    false
                                };

                                if !is_constraint_error {
                                    return ::core::result::Result::Err(batch_err);
                                }

                                for item in chunk {
                                    let row_res = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                                        async move {
                                            let record = #row_insert_expr
                                                .map_err(::autumn_web::AutumnError::from)?;

                                            let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                                ::core::option::Option::None;
                                            if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                                __autumn_commit_hook_discriminator =
                                                    ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                                            }
                                            let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                                            let __autumn_hook_info = ::autumn_web::__private::enqueue_repository_commit_hook_pending_on_conn(
                                                conn,
                                                Self::__autumn_repository_commit_hook_key(),
                                                "create",
                                                item.1.idempotency_key.as_deref(),
                                                __autumn_commit_hook_discriminator.as_deref(),
                                                &item.1,
                                                &__autumn_commit_hook_record,
                                            )
                                            .await?;

                                            Ok((record, __autumn_hook_info.0, __autumn_hook_info.1, __autumn_commit_hook_record))
                                        }
                                        .scope_boxed()
                                    })
                                    .await;

                                    match row_res {
                                        Ok((record, hook_id, hook_owner, hook_record)) => {
                                            let mut ctx = item.1.clone();
                                            let __autumn_pending_heartbeat =
                                                ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                                                    self.pool.clone(),
                                                    hook_id.clone(),
                                                    hook_owner.clone(),
                                                );
                                            let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                                                self.hooks.after_create(&mut ctx, &record)
                                            )
                                            .await;

                                            match __autumn_after_create {
                                                ::core::result::Result::Ok(::core::result::Result::Ok(())) => {
                                                    let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                                                        &self.pool,
                                                        &hook_id,
                                                        &hook_owner,
                                                        &ctx,
                                                        &hook_record,
                                                    )
                                                    .await;
                                                    __autumn_pending_heartbeat.cancel();
                                                    if let ::core::result::Result::Err(__autumn_error) = __autumn_finalize_result {
                                                        ::autumn_web::reexports::tracing::warn!(
                                                            hook_id = %hook_id,
                                                            error = %__autumn_error,
                                                            "failed to finalize repository create commit hook after mutation commit"
                                                        );
                                                        failures.push((item.2, __autumn_error));
                                                    } else {
                                                        successes.push(record);
                                                    }
                                                }
                                                ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                                                    let __autumn_error_message = ::std::format!("{__autumn_error}");
                                                    ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                                        &self.pool,
                                                        &hook_id,
                                                        &hook_owner,
                                                        __autumn_error_message,
                                                    )
                                                    .await;
                                                    __autumn_pending_heartbeat.cancel();
                                                    ::autumn_web::reexports::tracing::warn!(
                                                        hook_id = %hook_id,
                                                        error = %__autumn_error,
                                                        "after_create hook failed during skip-invalid inserts"
                                                    );
                                                    failures.push((item.2, __autumn_error));
                                                }
                                                ::core::result::Result::Err(__autumn_panic) => {
                                                    ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                                        &self.pool,
                                                        &hook_id,
                                                        &hook_owner,
                                                        "after_create panicked",
                                                    )
                                                    .await;
                                                    __autumn_pending_heartbeat.cancel();
                                                    ::autumn_web::reexports::tracing::warn!(
                                                        hook_id = %hook_id,
                                                        "after_create hook panicked during skip-invalid inserts"
                                                    );
                                                    failures.push((item.2, ::autumn_web::AutumnError::internal_server_error_msg("after_create hook panicked")));
                                                }
                                            }
                                        }
                                        Err(err) => {
                                            failures.push((item.2, err));
                                        }
                                    }
                                }
                            }
                        }
                        offset += chunk.len();
                    }

                    ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                }
            } else {
                quote! {
                    if valid_items.is_empty() {
                        return Ok((successes, failures));
                    }
                    let cols = (&valid_items[0].0).__autumn_column_count() + #tenant_extra;
                    let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                    for chunk in valid_items.chunks(chunk_size) {
                        let batch_res = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                            async move {
                                (#insert_expr)
                                    .map_err(::autumn_web::AutumnError::from)
                            }
                            .scope_boxed()
                        })
                        .await;

                        match batch_res {
                            Ok(inserted_chunk) => {
                                let mapped_indices: Vec<usize> = (0..inserted_chunk.len()).collect();

                                for (idx, record) in inserted_chunk.into_iter().enumerate() {
                                    let mapped_idx = mapped_indices[idx];
                                    let mut ctx = chunk[mapped_idx].1.clone();
                                    let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                                        self.hooks.after_create(&mut ctx, &record)
                                    )
                                    .await;
                                    match __autumn_after_create {
                                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {
                                            successes.push(record);
                                        }
                                        ::core::result::Result::Ok(::core::result::Result::Err(err)) => {
                                            ::autumn_web::reexports::tracing::warn!(
                                                error = %err,
                                                "after_create hook failed during skip-invalid inserts"
                                            );
                                            failures.push((chunk[mapped_idx].2, err));
                                        }
                                        ::core::result::Result::Err(_panic) => {
                                            ::autumn_web::reexports::tracing::warn!(
                                                "after_create hook panicked during skip-invalid inserts"
                                            );
                                            failures.push((chunk[mapped_idx].2, ::autumn_web::AutumnError::internal_server_error_msg("after_create hook panicked")));
                                        }
                                    }
                                }
                            }
                            Err(batch_err) => {
                                let is_constraint_error = if let ::core::option::Option::Some(diesel_err) = batch_err.downcast_ref::<::autumn_web::reexports::diesel::result::Error>() {
                                    match diesel_err {
                                        ::autumn_web::reexports::diesel::result::Error::DatabaseError(kind, _) => {
                                            match kind {
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::UniqueViolation |
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::ForeignKeyViolation |
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::NotNullViolation |
                                                ::autumn_web::reexports::diesel::result::DatabaseErrorKind::CheckViolation => true,
                                                _ => false,
                                            }
                                        }
                                        _ => false,
                                    }
                                } else {
                                    false
                                };

                                if !is_constraint_error {
                                    return ::core::result::Result::Err(batch_err);
                                }

                                for item in chunk {
                                    let row_res = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                                        async move {
                                            #row_insert_expr
                                                .map_err(::autumn_web::AutumnError::from)
                                        }
                                        .scope_boxed()
                                    })
                                    .await;

                                    match row_res {
                                        Ok(record) => {
                                            let mut ctx = item.1.clone();
                                            let __autumn_after_create = ::autumn_web::__private::catch_repository_after_hook_unwind(
                                                self.hooks.after_create(&mut ctx, &record)
                                            )
                                            .await;
                                            match __autumn_after_create {
                                                ::core::result::Result::Ok(::core::result::Result::Ok(())) => {
                                                    successes.push(record);
                                                }
                                                ::core::result::Result::Ok(::core::result::Result::Err(err)) => {
                                                    ::autumn_web::reexports::tracing::warn!(
                                                        error = %err,
                                                        "after_create hook failed during skip-invalid inserts"
                                                    );
                                                    failures.push((item.2, err));
                                                }
                                                ::core::result::Result::Err(_panic) => {
                                                    ::autumn_web::reexports::tracing::warn!(
                                                        "after_create hook panicked during skip-invalid inserts"
                                                    );
                                                    failures.push((item.2, ::autumn_web::AutumnError::internal_server_error_msg("after_create hook panicked")));
                                                }
                                            }
                                        }
                                        Err(err) => {
                                            failures.push((item.2, err));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;

                if new.is_empty() {
                    return Ok((Vec::new(), Vec::new()));
                }

                #tenant_id_setup
                #register_commit_hooks

                let mut conn = self.__autumn_acquire_conn().await?;
                let mut successes = Vec::new();
                let mut failures = Vec::new();

                // 1. Run before_create hooks sequentially
                let mut valid_items = Vec::new();
                for (idx, original_item) in new.iter().enumerate() {
                    let mut item = original_item.clone();
                    let mut ctx = MutationContext::new(MutationOp::Create);
                    #idempotency_setup
                    match self.hooks.before_create(&mut ctx, &mut item).await {
                        Ok(()) => {
                            valid_items.push((item, ctx, idx));
                        }
                        Err(err) => {
                            failures.push((idx, err));
                        }
                    }
                }

                // 2. Insert valid items in chunks
                #skip_invalid_impl

                Ok((successes, failures))
            }
        };

        let update_many_body = {
            let draft_ext_trait = format_ident!("{}DraftExt", model_name);

            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            let load_expr = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().load::<#model_name>(conn).await
                    } else {
                        load_query.for_update().load::<#model_name>(conn).await
                    }
                }
            } else {
                quote! {
                    load_query.for_update().load::<#model_name>(conn).await
                }
            };

            let tenant_assign = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        draft.after.tenant_id = t.clone();
                    }
                }
            } else {
                quote! {}
            };

            let update_expr = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                            .set(proposed)
                            .get_result::<#model_name>(conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::update(update_target)
                            .set(proposed)
                            .get_result::<#model_name>(conn)
                            .await
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::diesel::update(update_target)
                        .set(proposed)
                        .get_result::<#model_name>(conn)
                        .await
                }
            };

            let idempotency_setup = if commit_hooks_enabled {
                quote! {
                    if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                        ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                    }
                }
            } else {
                quote! {}
            };

            let commit_hooks_enqueue_block = if commit_hooks_enabled {
                if config.broadcasts {
                    let base_topic_expr = match generate_topic_format(
                        config
                            .broadcast_topic
                            .as_deref()
                            .unwrap_or(&config.table_name),
                        &quote! { __record_ref },
                    ) {
                        Ok(expr) => expr,
                        Err(err) => {
                            let compile_err = err.to_compile_error();
                            return quote! { #compile_err };
                        }
                    };

                    let topic_expr = if config.tenant_scoped {
                        quote! { ::std::format!("tenant:{}:{}", ::autumn_web::tenancy::DisplayTenantId::tenant_id_str(&__record_ref.tenant_id), #base_topic_expr) }
                    } else {
                        base_topic_expr
                    };

                    let prev_id_expr_bulk = if let Some(ref render_path) = config.broadcast_render {
                        quote! { ::autumn_web::htmx::extract_html_id(&{#render_path(__record_ref)}.into_string()) }
                    } else {
                        quote! { ::core::option::Option::Some(<#model_name as ::autumn_web::live::LiveFragment>::dom_id(__record_ref)) }
                    };

                    quote! {
                        let mut hook_records = Vec::new();
                        for record in &chunk_updated {
                            let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                ::core::option::Option::None;
                            if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                __autumn_commit_hook_discriminator =
                                    ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                            }
                            let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                            hook_records.push((__autumn_commit_hook_record, __autumn_commit_hook_discriminator));
                        }

                        let mut serialized_contexts = Vec::new();
                        let mut chunk_previous_topics = Vec::new();
                        let mut chunk_previous_ids = Vec::new();
                        for (idx, _record) in chunk_updated.iter().enumerate() {
                            let global_idx = offset + idx;
                            let ctx = &contexts[global_idx];
                            let mut ctx_val = ::autumn_web::reexports::serde_json::to_value(ctx)
                                .map_err(|e| ::autumn_web::AutumnError::internal_server_error_msg(format!("serialize context: {e}")))?;

                            let __record_val = &current_rows[global_idx];
                            let __record_ref = __record_val;
                            let __prev_topic = #topic_expr;
                            chunk_previous_topics.push(::core::option::Option::Some(__prev_topic.clone()));

                            let __prev_id = #prev_id_expr_bulk;
                            chunk_previous_ids.push(__prev_id.clone());

                            if let ::core::option::Option::Some(__prev_id_val) = __prev_id {
                                if let ::core::option::Option::Some(__map) = ctx_val.as_object_mut() {
                                    __map.insert(
                                        "__autumn_previous_id".to_string(),
                                        ::autumn_web::reexports::serde_json::Value::String(__prev_id_val),
                                    );
                                }
                            }

                            if let ::core::option::Option::Some(__map) = ctx_val.as_object_mut() {
                                __map.insert(
                                    "__autumn_previous_topic".to_string(),
                                    ::autumn_web::reexports::serde_json::Value::String(__prev_topic),
                                );
                            }
                            serialized_contexts.push(ctx_val);
                        }

                        let hook_inputs: Vec<_> = chunk_updated.iter().enumerate().map(|(idx, _)| {
                            let global_idx = offset + idx;
                            let (ref record_val, ref discriminator) = hook_records[idx];
                            (
                                contexts[global_idx].idempotency_key.clone(),
                                discriminator.clone(),
                                &serialized_contexts[idx],
                                record_val,
                            )
                        }).collect();

                        let chunk_hook_infos = ::autumn_web::__private::enqueue_repository_commit_hooks_pending_bulk_on_conn(
                            conn,
                            Self::__autumn_repository_commit_hook_key(),
                            "update",
                            &hook_inputs,
                        )
                        .await?;

                        for (idx, info) in chunk_hook_infos.into_iter().enumerate() {
                            hook_infos.push((
                                info.0,
                                info.1,
                                hook_records[idx].0.clone(),
                                chunk_previous_topics[idx].clone(),
                                chunk_previous_ids[idx].clone(),
                            ));
                        }
                    }
                } else {
                    quote! {
                        let mut hook_records = Vec::new();
                        for record in &chunk_updated {
                            let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                                ::core::option::Option::None;
                            if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                                __autumn_commit_hook_discriminator =
                                    ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                            }
                            let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                            hook_records.push((__autumn_commit_hook_record, __autumn_commit_hook_discriminator));
                        }

                        let hook_inputs: Vec<_> = chunk_updated.iter().enumerate().map(|(idx, _)| {
                            let global_idx = offset + idx;
                            let ctx = &contexts[global_idx];
                            let (ref record_val, ref discriminator) = hook_records[idx];
                            (
                                ctx.idempotency_key.clone(),
                                discriminator.clone(),
                                ctx,
                                record_val,
                            )
                        }).collect();

                        let chunk_hook_infos = ::autumn_web::__private::enqueue_repository_commit_hooks_pending_bulk_on_conn(
                            conn,
                            Self::__autumn_repository_commit_hook_key(),
                            "update",
                            &hook_inputs,
                        )
                        .await?;

                        for (idx, info) in chunk_hook_infos.into_iter().enumerate() {
                            hook_infos.push((
                                info.0,
                                info.1,
                                hook_records[idx].0.clone(),
                                ::core::option::Option::None,
                                ::core::option::Option::None,
                            ));
                        }
                    }
                }
            } else {
                quote! {}
            };

            let after_update_hook_block = if commit_hooks_enabled {
                let finalize_setup = if config.broadcasts {
                    quote! {
                        let mut __autumn_finalized_ctx_val = ::autumn_web::reexports::serde_json::to_value(&ctx)
                            .map_err(|e| ::autumn_web::AutumnError::internal_server_error_msg(format!("serialize finalized context: {e}")))?;
                        if let ::core::option::Option::Some(__prev_topic) = __autumn_previous_topic {
                            if let ::core::option::Option::Some(__map) = __autumn_finalized_ctx_val.as_object_mut() {
                                __map.insert(
                                    "__autumn_previous_topic".to_string(),
                                    ::autumn_web::reexports::serde_json::Value::String(__prev_topic.clone()),
                                );
                            }
                        }
                        if let ::core::option::Option::Some(__prev_id) = __autumn_previous_id {
                            if let ::core::option::Option::Some(__map) = __autumn_finalized_ctx_val.as_object_mut() {
                                __map.insert(
                                    "__autumn_previous_id".to_string(),
                                    ::autumn_web::reexports::serde_json::Value::String(__prev_id.clone()),
                                );
                            }
                        }
                    }
                } else {
                    quote! {}
                };
                let finalize_ref = if config.broadcasts {
                    quote! { &__autumn_finalized_ctx_val }
                } else {
                    quote! { &ctx }
                };

                quote! {
                    let (hook_id, hook_owner, hook_record, __autumn_previous_topic, __autumn_previous_id) = &hook_infos[idx];
                    let __autumn_pending_heartbeat =
                        ::autumn_web::__private::start_repository_commit_hook_pending_finalizer_heartbeat(
                            self.pool.clone(),
                            hook_id.clone(),
                            hook_owner.clone(),
                        );
                    let __autumn_after_update = ::autumn_web::__private::catch_repository_after_hook_unwind(
                        self.hooks.after_update(&mut ctx, record)
                    )
                    .await;

                    match __autumn_after_update {
                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {
                            #finalize_setup
                            let __autumn_finalize_result = ::autumn_web::__private::finalize_repository_commit_hook_after_hook(
                                &self.pool,
                                hook_id,
                                hook_owner,
                                #finalize_ref,
                                hook_record,
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if let ::core::result::Result::Err(__autumn_error) = __autumn_finalize_result {
                                ::autumn_web::reexports::tracing::warn!(
                                    hook_id = %hook_id,
                                    error = %__autumn_error,
                                    "failed to finalize repository update commit hook after mutation commit; failing request closed"
                                );
                                if __autumn_first_err.is_none() {
                                    __autumn_first_err = ::core::option::Option::Some(
                                        ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                                    );
                                }
                            }
                        }
                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                            let __autumn_error_message = ::std::format!("{__autumn_error}");
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                hook_id,
                                hook_owner,
                                __autumn_error_message,
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if __autumn_first_err.is_none() {
                                __autumn_first_err = ::core::option::Option::Some(
                                    ::autumn_web::idempotency::__cache_committed_error_response(__autumn_error)
                                );
                            }
                        }
                        ::core::result::Result::Err(__autumn_panic) => {
                            ::autumn_web::__private::mark_repository_commit_hook_after_hook_failed(
                                &self.pool,
                                hook_id,
                                hook_owner,
                                "after_update panicked",
                            )
                            .await;
                            __autumn_pending_heartbeat.cancel();
                            if __autumn_first_panic.is_none() {
                                __autumn_first_panic = ::core::option::Option::Some(__autumn_panic);
                            }
                        }
                    }
                }
            } else {
                quote! {
                    let __autumn_after_update = ::autumn_web::__private::catch_repository_after_hook_unwind(
                        self.hooks.after_update(&mut ctx, record)
                    )
                    .await;
                    match __autumn_after_update {
                        ::core::result::Result::Ok(::core::result::Result::Ok(())) => {}
                        ::core::result::Result::Ok(::core::result::Result::Err(__autumn_error)) => {
                            if __autumn_first_err.is_none() {
                                __autumn_first_err = ::core::option::Option::Some(__autumn_error);
                            }
                        }
                        ::core::result::Result::Err(__autumn_panic) => {
                            if __autumn_first_panic.is_none() {
                                __autumn_first_panic = ::core::option::Option::Some(__autumn_panic);
                            }
                        }
                    }
                }
            };

            let kick_dispatcher_block = if commit_hooks_enabled {
                quote! {
                    ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                }
            } else {
                quote! {}
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks, UpdateDraft};
                use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                if ids.is_empty() {
                    return Ok(Vec::new());
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_conn().await?;
                let (updated_records, contexts, hook_infos) = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut current_rows = Vec::new();
                        for chunk in ids.chunks(1000) {
                            let load_query = #table_ident::table.filter(#table_ident::id.eq_any(chunk))
                                .order(#table_ident::id.asc());
                            let chunk_rows = #load_expr
                                .map_err(::autumn_web::AutumnError::from)?;
                            current_rows.extend(chunk_rows);
                        }

                        // Optimistic concurrency version check
                        if let ::core::option::Option::Some(expected_version) =
                            changes.__autumn_lock_version_expected()
                        {
                            for current in &current_rows {
                                if let ::core::option::Option::Some(actual_version) =
                                    current.__autumn_lock_version_actual()
                                {
                                    if actual_version != expected_version {
                                        return Err(::autumn_web::AutumnError::conflict(
                                            ::autumn_web::RepositoryError::Conflict {
                                                id: current.id,
                                                expected_version,
                                                actual_version: ::core::option::Option::Some(actual_version),
                                            },
                                        ));
                                    }
                                }
                            }
                        }

                        let mut proposed_rows = Vec::new();
                        let mut contexts = Vec::new();
                        for current in &current_rows {
                            let mut ctx = MutationContext::new(MutationOp::Update);
                            #idempotency_setup

                            let mut draft = <UpdateDraft<#model_name> as #draft_ext_trait>::from_patch(current, changes)?;
                            #tenant_assign
                            self.hooks.before_update(&mut ctx, &mut draft).await?;
                            #tenant_assign

                            proposed_rows.push(draft.into_after());
                            contexts.push(ctx);
                        }

                        let mut updated_records = Vec::new();
                        let mut hook_infos: ::std::vec::Vec<(::std::string::String, ::std::string::String, ::serde_json::Value, ::core::option::Option<::std::string::String>, ::core::option::Option<::std::string::String>)> = ::std::vec::Vec::new();
                        let mut offset = 0;
                        for chunk in proposed_rows.chunks(1000) {
                            let mut chunk_updated = Vec::new();
                            for proposed in chunk {
                                let update_target = #table_ident::table.find(proposed.id);
                                let updated = #update_expr
                                    .map_err(::autumn_web::AutumnError::from)?;
                                chunk_updated.push(updated);
                            }

                            #commit_hooks_enqueue_block

                            updated_records.extend(chunk_updated);
                            offset += chunk.len();
                        }

                        Ok((updated_records, contexts, hook_infos))
                    }
                    .scope_boxed()
                })
                .await?;

                ::core::mem::drop(conn);

                let mut __autumn_first_err: ::core::option::Option<::autumn_web::AutumnError> = ::core::option::Option::None;
                let mut __autumn_first_panic: ::core::option::Option<::std::boxed::Box<dyn ::core::any::Any + ::core::marker::Send>> = ::core::option::Option::None;

                // Run after_update hooks outside of transaction
                for (idx, record) in updated_records.iter().enumerate() {
                    let mut ctx = contexts[idx].clone();

                    #after_update_hook_block
                }

                #kick_dispatcher_block

                if let ::core::option::Option::Some(err) = __autumn_first_err {
                    return ::core::result::Result::Err(err);
                }
                if let ::core::option::Option::Some(panic_val) = __autumn_first_panic {
                    ::std::panic::resume_unwind(panic_val);
                }

                Ok(updated_records)
            }
        };

        let delete_many_body = {
            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            let load_expr = if config.tenant_scoped {
                if config.soft_delete {
                    quote! {
                        if let ::core::option::Option::Some(t) = tenant_id {
                            load_query.filter(#table_ident::tenant_id.eq(t)).filter(#table_ident::deleted_at.is_null()).for_update().load::<#model_name>(conn).await
                        } else {
                            load_query.filter(#table_ident::deleted_at.is_null()).for_update().load::<#model_name>(conn).await
                        }
                    }
                } else {
                    quote! {
                        if let ::core::option::Option::Some(t) = tenant_id {
                            load_query.filter(#table_ident::tenant_id.eq(t)).for_update().load::<#model_name>(conn).await
                        } else {
                            load_query.for_update().load::<#model_name>(conn).await
                        }
                    }
                }
            } else {
                if config.soft_delete {
                    quote! {
                        load_query.filter(#table_ident::deleted_at.is_null()).for_update().load::<#model_name>(conn).await
                    }
                } else {
                    quote! {
                        load_query.for_update().load::<#model_name>(conn).await
                    }
                }
            };

            let delete_expr = if config.soft_delete {
                if config.tenant_scoped {
                    quote! {
                        let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null());
                        if let ::core::option::Option::Some(t) = tenant_id {
                            ::autumn_web::reexports::diesel::update(query.filter(#table_ident::tenant_id.eq(t)))
                                .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                .execute(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::update(query)
                                .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                .execute(conn)
                                .await
                        }
                    }
                } else {
                    quote! {
                        ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null()))
                            .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                            .execute(conn)
                            .await
                    }
                }
            } else {
                if config.tenant_scoped {
                    quote! {
                        let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk));
                        if let ::core::option::Option::Some(t) = tenant_id {
                            ::autumn_web::reexports::diesel::delete(query.filter(#table_ident::tenant_id.eq(t)))
                                .execute(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::delete(query)
                                .execute(conn)
                                .await
                        }
                    }
                } else {
                    quote! {
                        ::autumn_web::reexports::diesel::delete(#table_ident::table.filter(#table_ident::id.eq_any(chunk)))
                            .execute(conn)
                            .await
                    }
                }
            };

            let delete_returning_expr = if config.soft_delete {
                if config.tenant_scoped {
                    // Braces required: this fragment is assigned with
                    // `let chunk_deleted_ids = #delete_returning_expr` in the
                    // versioned path, so the leading `let query` must be inside
                    // a block expression.
                    quote! {
                        {
                            let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null());
                            if let ::core::option::Option::Some(t) = tenant_id {
                                ::autumn_web::reexports::diesel::update(query.filter(#table_ident::tenant_id.eq(t)))
                                    .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                    .returning(#table_ident::id)
                                    .get_results::<i64>(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::update(query)
                                    .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                    .returning(#table_ident::id)
                                    .get_results::<i64>(conn)
                                    .await
                            }
                        }
                    }
                } else {
                    quote! {
                        ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null()))
                            .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                            .returning(#table_ident::id)
                            .get_results::<i64>(conn)
                            .await
                    }
                }
            } else {
                if config.tenant_scoped {
                    quote! {
                        {
                            let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk));
                            if let ::core::option::Option::Some(t) = tenant_id {
                                ::autumn_web::reexports::diesel::delete(query.filter(#table_ident::tenant_id.eq(t)))
                                    .returning(#table_ident::id)
                                    .get_results::<i64>(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::delete(query)
                                    .returning(#table_ident::id)
                                    .get_results::<i64>(conn)
                                    .await
                            }
                        }
                    }
                } else {
                    quote! {
                        ::autumn_web::reexports::diesel::delete(#table_ident::table.filter(#table_ident::id.eq_any(chunk)))
                            .returning(#table_ident::id)
                            .get_results::<i64>(conn)
                            .await
                    }
                }
            };

            let vh_delete_write = if config.versioned {
                let vh = vh_insert_ts(
                    table_name,
                    "delete",
                    false,
                    &quote! { r },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    for r in &__vh_deleted_records {
                        #vh
                    }
                }
            } else {
                quote! {}
            };

            let delete_execution = if config.versioned {
                quote! {
                    let mut __vh_actually_deleted: ::std::collections::HashSet<i64> = ::std::collections::HashSet::new();
                    for chunk in ids.chunks(1000) {
                        let chunk_deleted_ids = #delete_returning_expr
                            .map_err(::autumn_web::AutumnError::from)?;
                        __vh_actually_deleted.extend(chunk_deleted_ids);
                    }
                    let mut __vh_deleted_records = current_rows.clone();
                    __vh_deleted_records.retain(|r| __vh_actually_deleted.contains(&r.id));
                    #vh_delete_write
                }
            } else {
                quote! {
                    for chunk in ids.chunks(1000) {
                        #delete_expr
                            .map_err(::autumn_web::AutumnError::from)?;
                    }
                }
            };

            let idempotency_setup = if commit_hooks_enabled {
                quote! {
                    if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                        ctx.set_idempotency_key(__autumn_idempotency.scoped_key());
                    }
                }
            } else {
                quote! {}
            };

            let delete_commit_hook_setup = if commit_hooks_enabled {
                quote! {
                    let mut __autumn_commit_hook_discriminator: ::core::option::Option<::std::string::String> =
                        ::core::option::Option::None;
                    if let ::core::option::Option::Some(__autumn_idempotency) = &self.idempotency {
                        __autumn_commit_hook_discriminator =
                            ::core::option::Option::Some(__autumn_idempotency.next_mutation_discriminator());
                    }

                    let __autumn_commit_hook_record = record.__autumn_commit_hook_to_value()?;
                    ::autumn_web::__private::enqueue_repository_commit_hook_on_conn(
                        conn,
                        Self::__autumn_repository_commit_hook_key(),
                        "delete",
                        ctx.idempotency_key.as_deref(),
                        __autumn_commit_hook_discriminator.as_deref(),
                        &ctx,
                        &__autumn_commit_hook_record,
                    )
                    .await?;
                }
            } else {
                quote! {}
            };

            let kick_dispatcher = if commit_hooks_enabled {
                quote! {
                    ::autumn_web::__private::kick_repository_commit_hook_dispatcher(&self.pool);
                }
            } else {
                quote! {}
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::hooks::{MutationContext, MutationOp, MutationHooks};

                if ids.is_empty() {
                    return Ok(());
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_conn().await?;
                let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut current_rows = Vec::new();
                        for chunk in ids.chunks(1000) {
                            let load_query = #table_ident::table.filter(#table_ident::id.eq_any(chunk))
                                .order(#table_ident::id.asc());
                            let chunk_rows = #load_expr
                                .map_err(::autumn_web::AutumnError::from)?;
                            current_rows.extend(chunk_rows);
                        }

                        for record in &current_rows {
                            let mut ctx = MutationContext::new(MutationOp::Delete);
                            #idempotency_setup
                            self.hooks.before_delete(&mut ctx, record).await?;
                            #delete_commit_hook_setup
                        }

                        #delete_execution

                        Ok(())
                    }
                    .scope_boxed()
                })
                .await?;

                ::core::mem::drop(conn);
                #kick_dispatcher

                Ok(())
            }
        };

        let upsert_many_body = quote! {
            unreachable!("upsert_many is not available when hooks are configured")
        };

        (
            struct_fields,
            clone_impl,
            extractor_init,
            save_body,
            update_body,
            delete_body,
            hook_support_methods,
            hook_inventory_registration,
            save_many_body,
            save_many_skip_invalid_body,
            update_many_body,
            delete_many_body,
            upsert_many_body,
        )
    } else {
        // ── No hooks: existing zero-cost path ─────────────

        let struct_fields = quote! {
            pool: ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                ::autumn_web::reexports::diesel_async::AsyncPgConnection,
            >,
            #tenant_struct_field
            #shards_struct_field
            /// Read-routing snapshot for generated read-only methods (#971).
            __autumn_read_route: ::autumn_web::repository::ReadRoute,
            /// Statement timeout to apply on every connection checkout (ms). 0 = no limit.
            __autumn_statement_timeout_ms: u64,
            /// Slow-query logging threshold.
            __autumn_slow_threshold: ::std::time::Duration,
            /// Route path from `MatchedPath` for metrics labels.
            __autumn_route: ::std::option::Option<::std::string::String>,
            #bcast_struct_field
        };

        let clone_impl = quote! {
            impl ::core::clone::Clone for #pg_name {
                fn clone(&self) -> Self {
                    Self {
                        pool: self.pool.clone(),
                        #tenant_clone_field
                        #shards_clone_field
                        __autumn_read_route: self.__autumn_read_route.clone(),
                        __autumn_statement_timeout_ms: self.__autumn_statement_timeout_ms,
                        __autumn_slow_threshold: self.__autumn_slow_threshold,
                        __autumn_route: self.__autumn_route.clone(),
                        #bcast_clone_field
                    }
                }
            }
        };

        let timeout_route_init = quote! {
            use ::autumn_web::db::DbState as _;
            // Postgres statement_timeout is a signed 32-bit integer (ms).
            const __AUTUMN_PG_TIMEOUT_MAX_MS: u64 = i32::MAX as u64;
            let __autumn_timeout_ms: u64 = _parts
                .extensions
                .get::<::autumn_web::db::StatementTimeout>()
                .map(|t| ::std::convert::TryFrom::try_from(t.0.as_millis()).unwrap_or(u64::MAX))
                .or_else(|| state.statement_timeout().map(|d| ::std::convert::TryFrom::try_from(d.as_millis()).unwrap_or(u64::MAX)))
                .unwrap_or(0u64)
                .min(__AUTUMN_PG_TIMEOUT_MAX_MS);
            let __autumn_slow_threshold = state.slow_query_threshold();
            let __autumn_route: ::std::option::Option<::std::string::String> = _parts
                .extensions
                .get::<::autumn_web::reexports::axum::extract::MatchedPath>()
                .map(|p| p.as_str().to_owned());
            #read_route_init
        };

        let extractor_init = quote! {
            #timeout_route_init
            Ok(#pg_name {
                pool,
                #tenant_init_field
                #shards_none_field
                __autumn_read_route,
                __autumn_statement_timeout_ms: __autumn_timeout_ms,
                __autumn_slow_threshold,
                __autumn_route,
                #bcast_field_some_state
            })
        };

        let save_body = if config.tenant_scoped && config.versioned {
            let vh_insert = vh_insert_ts(
                table_name,
                "insert",
                false,
                &quote! { record },
                None,
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                let tenant_id = if self.across_tenants {
                    ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let mut conn = self.__autumn_acquire_conn().await?;
                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| async move {
                    let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(::autumn_web::tenancy::TenantInsertable::tenant_values(new.clone(), t))
                            .get_result::<#model_name>(conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(new.clone())
                            .get_result::<#model_name>(conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)?;
                    #vh_insert
                    Ok(record)
                }.scope_boxed())
                .await
            }
        } else if config.tenant_scoped {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let tenant_id = if self.across_tenants {
                    ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let mut conn = self.__autumn_acquire_conn().await?;
                if let ::core::option::Option::Some(ref t) = tenant_id {
                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                        .values(::autumn_web::tenancy::TenantInsertable::tenant_values(new.clone(), t))
                        .get_result::<#model_name>(&mut conn)
                        .await
                } else {
                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                        .values(new.clone())
                        .get_result::<#model_name>(&mut conn)
                        .await
                }
                .map_err(::autumn_web::AutumnError::from)
            }
        } else if config.versioned {
            let vh_insert = vh_insert_ts(
                table_name,
                "insert",
                false,
                &quote! { record },
                None,
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                let mut conn = self.__autumn_acquire_conn().await?;
                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| async move {
                    let record = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                        .values(new.clone())
                        .get_result::<#model_name>(conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    #vh_insert
                    Ok(record)
                }.scope_boxed())
                .await
            }
        } else {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_conn().await?;
                ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                    .values(new.clone())
                    .get_result::<#model_name>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            }
        };

        let update_body = if config.tenant_scoped && config.versioned {
            let vh_insert = vh_insert_ts(
                table_name,
                "update",
                false,
                &quote! { record },
                Some(&quote! { current }),
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _, CanSetTenantId as _};
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let mut conn = self.__autumn_acquire_conn().await?;
                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let load_query = #table_ident::table.find(id);
                        let current = if let ::core::option::Option::Some(expected_version) =
                            changes.__autumn_lock_version_expected()
                        {
                            let c = if let ::core::option::Option::Some(ref t) = tenant_id {
                                load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                            } else {
                                load_query.for_update().first::<#model_name>(conn).await
                            }
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;
                            if let ::core::option::Option::Some(actual_version) = c.__autumn_lock_version_actual() {
                                if actual_version != expected_version {
                                    return Err(::autumn_web::AutumnError::conflict(
                                        ::autumn_web::RepositoryError::Conflict {
                                            id,
                                            expected_version,
                                            actual_version: ::core::option::Option::Some(actual_version),
                                        },
                                    ));
                                }
                            }
                            c
                        } else {
                            if let ::core::option::Option::Some(ref t) = tenant_id {
                                load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                            } else {
                                load_query.for_update().first::<#model_name>(conn).await
                            }
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?
                        };
                        let mut diesel_changeset = changes.__to_changeset();
                        if let ::core::option::Option::Some(ref t) = tenant_id {
                            diesel_changeset.set_tenant_id(t.clone());
                        }
                        let update_target = #table_ident::table.find(id);
                        let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                            ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                                .set(diesel_changeset)
                                .get_result::<#model_name>(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::update(update_target)
                                .set(diesel_changeset)
                                .get_result::<#model_name>(conn)
                                .await
                        }
                        .map_err(::autumn_web::AutumnError::from)?;
                        #vh_insert
                        Ok(record)
                    }
                    .scope_boxed()
                })
                .await
            }
        } else if config.tenant_scoped {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let mut conn = self.__autumn_acquire_conn().await?;

                if let ::core::option::Option::Some(expected_version) =
                    changes.__autumn_lock_version_expected()
                {
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;

                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            // SELECT FOR UPDATE grabs an exclusive row lock so
                            // no concurrent writer can commit between our
                            // version check and the UPDATE below.
                            let load_query = #table_ident::table.find(id);
                            let current = if let ::core::option::Option::Some(ref t) = tenant_id {
                                load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                            } else {
                                load_query.for_update().first::<#model_name>(conn).await
                            }
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;

                            if let ::core::option::Option::Some(actual_version) =
                                current.__autumn_lock_version_actual()
                            {
                                if actual_version != expected_version {
                                    return Err(::autumn_web::AutumnError::conflict(
                                        ::autumn_web::RepositoryError::Conflict {
                                            id,
                                            expected_version,
                                            actual_version: ::core::option::Option::Some(actual_version),
                                        },
                                    ));
                                }
                            }

                            let mut diesel_changeset = changes.__to_changeset();
                            if let ::core::option::Option::Some(ref t) = tenant_id {
                                use ::autumn_web::repository::CanSetTenantId as _;
                                diesel_changeset.set_tenant_id(t.clone());
                            }
                            let update_target = #table_ident::table.find(id);
                            if let ::core::option::Option::Some(ref t) = tenant_id {
                                ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                                    .set(diesel_changeset)
                                    .get_result::<#model_name>(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::update(update_target)
                                    .set(diesel_changeset)
                                    .get_result::<#model_name>(conn)
                                    .await
                            }
                            .map_err(::autumn_web::AutumnError::from)
                        }
                        .scope_boxed()
                    })
                    .await
                } else {
                    let mut diesel_changeset = changes.__to_changeset();
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        use ::autumn_web::repository::CanSetTenantId as _;
                        diesel_changeset.set_tenant_id(t.clone());
                    }
                    let update_target = #table_ident::table.find(id);
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::update(update_target.filter(#table_ident::tenant_id.eq(t)))
                            .set(diesel_changeset)
                            .get_result::<#model_name>(&mut conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::update(update_target)
                            .set(diesel_changeset)
                            .get_result::<#model_name>(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)
                }
            }
        } else if config.versioned {
            let vh_insert = vh_insert_ts(
                table_name,
                "update",
                false,
                &quote! { record },
                Some(&quote! { current }),
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};
                let mut conn = self.__autumn_acquire_conn().await?;

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let load_query = #table_ident::table.find(id);
                        let current = if let ::core::option::Option::Some(expected_version) =
                            changes.__autumn_lock_version_expected()
                        {
                            let c = load_query.for_update().first::<#model_name>(conn).await
                                .optional()
                                .map_err(::autumn_web::AutumnError::from)?
                                .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                    format!("{} with id {} not found", stringify!(#model_name), id)
                                ))?;
                            if let ::core::option::Option::Some(actual_version) =
                                c.__autumn_lock_version_actual()
                            {
                                if actual_version != expected_version {
                                    return Err(::autumn_web::AutumnError::conflict(
                                        ::autumn_web::RepositoryError::Conflict {
                                            id,
                                            expected_version,
                                            actual_version: ::core::option::Option::Some(actual_version),
                                        },
                                    ));
                                }
                            }
                            c
                        } else {
                            load_query.for_update().first::<#model_name>(conn).await
                                .optional()
                                .map_err(::autumn_web::AutumnError::from)?
                                .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                    format!("{} with id {} not found", stringify!(#model_name), id)
                                ))?
                        };
                        let diesel_changeset = changes.__to_changeset();
                        let update_target = #table_ident::table.find(id);
                        let record = ::autumn_web::reexports::diesel::update(update_target)
                            .set(diesel_changeset)
                            .get_result::<#model_name>(conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        #vh_insert
                        Ok(record)
                    }
                    .scope_boxed()
                })
                .await
            }
        } else {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};
                let mut conn = self.__autumn_acquire_conn().await?;

                if let ::core::option::Option::Some(expected_version) =
                    changes.__autumn_lock_version_expected()
                {
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;

                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let load_query = #table_ident::table.find(id);
                            let current = load_query.for_update().first::<#model_name>(conn).await
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;

                            if let ::core::option::Option::Some(actual_version) =
                                current.__autumn_lock_version_actual()
                            {
                                if actual_version != expected_version {
                                    return Err(::autumn_web::AutumnError::conflict(
                                        ::autumn_web::RepositoryError::Conflict {
                                            id,
                                            expected_version,
                                            actual_version: ::core::option::Option::Some(actual_version),
                                        },
                                    ));
                                }
                            }

                            let diesel_changeset = changes.__to_changeset();
                            let update_target = #table_ident::table.find(id);
                            ::autumn_web::reexports::diesel::update(update_target)
                                .set(diesel_changeset)
                                .get_result::<#model_name>(conn)
                                .await
                                .map_err(::autumn_web::AutumnError::from)
                        }
                        .scope_boxed()
                    })
                    .await
                } else {
                    let diesel_changeset = changes.__to_changeset();
                    let update_target = #table_ident::table.find(id);
                    ::autumn_web::reexports::diesel::update(update_target)
                        .set(diesel_changeset)
                        .get_result::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)
                }
            }
        };

        let delete_body = if config.tenant_scoped {
            let tenant_id_setup = quote! {
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
            };
            if config.soft_delete && config.versioned {
                let vh_insert = vh_insert_ts(
                    table_name,
                    "delete",
                    false,
                    &quote! { record },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    #tenant_id_setup
                    let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| async move {
                        let load_query = #table_ident::table.find(id).filter(#table_ident::deleted_at.is_null());
                        let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                            load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                        } else {
                            load_query.for_update().first::<#model_name>(conn).await
                        }
                        .optional()
                        .map_err(::autumn_web::AutumnError::from)?
                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ))?;
                        let delete_query = #table_ident::table.find(id).filter(#table_ident::deleted_at.is_null());
                        let __count = if let ::core::option::Option::Some(ref t) = tenant_id {
                            ::autumn_web::reexports::diesel::update(delete_query.filter(#table_ident::tenant_id.eq(t)))
                                .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                .execute(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::update(delete_query)
                                .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                .execute(conn)
                                .await
                        }
                        .map_err(::autumn_web::AutumnError::from)?;
                        if __count == 0 {
                            return Err(::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ));
                        }
                        #vh_insert
                        Ok(())
                    }.scope_boxed())
                    .await
                }
            } else if config.soft_delete {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    #tenant_id_setup
                    let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let delete_query = #table_ident::table.find(id).filter(#table_ident::deleted_at.is_null());
                    let __count = if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::update(delete_query.filter(#table_ident::tenant_id.eq(t)))
                            .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                            .execute(&mut conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::update(delete_query)
                            .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                            .execute(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }
            } else if config.versioned {
                let vh_insert = vh_insert_ts(
                    table_name,
                    "delete",
                    false,
                    &quote! { record },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_conn().await?;
                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| async move {
                        let load_query = #table_ident::table.find(id);
                        let record = if let ::core::option::Option::Some(ref t) = tenant_id {
                            load_query.filter(#table_ident::tenant_id.eq(t)).for_update().first::<#model_name>(conn).await
                        } else {
                            load_query.for_update().first::<#model_name>(conn).await
                        }
                        .optional()
                        .map_err(::autumn_web::AutumnError::from)?
                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ))?;
                        let delete_query = #table_ident::table.find(id);
                        let __count = if let ::core::option::Option::Some(ref t) = tenant_id {
                            ::autumn_web::reexports::diesel::delete(delete_query.filter(#table_ident::tenant_id.eq(t)))
                                .execute(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::delete(delete_query)
                                .execute(conn)
                                .await
                        }
                        .map_err(::autumn_web::AutumnError::from)?;
                        if __count == 0 {
                            return Err(::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ));
                        }
                        #vh_insert
                        Ok(())
                    }.scope_boxed())
                    .await
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let delete_query = #table_ident::table.find(id);
                    let __count = if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::delete(delete_query.filter(#table_ident::tenant_id.eq(t)))
                            .execute(&mut conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::delete(delete_query)
                            .execute(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }
            }
        } else if config.soft_delete {
            if config.versioned {
                let vh_insert = vh_insert_ts(
                    table_name,
                    "delete",
                    false,
                    &quote! { record },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    use ::autumn_web::reexports::diesel_async::AsyncConnection;
                    use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                    let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| async move {
                        let record = #table_ident::table.find(id)
                            .filter(#table_ident::deleted_at.is_null())
                            .for_update()
                            .first::<#model_name>(conn)
                            .await
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;
                        let __count = ::autumn_web::reexports::diesel::update(
                            #table_ident::table.find(id).filter(#table_ident::deleted_at.is_null())
                        )
                            .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                            .execute(conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        if __count == 0 {
                            return Err(::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ));
                        }
                        #vh_insert
                        Ok(())
                    }.scope_boxed())
                    .await
                }
            } else {
                quote! {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let delete_query = #table_ident::table.find(id).filter(#table_ident::deleted_at.is_null());
                    let __count = ::autumn_web::reexports::diesel::update(delete_query)
                        .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                        .execute(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }
            }
        } else if config.versioned {
            let vh_insert = vh_insert_ts(
                table_name,
                "delete",
                false,
                &quote! { record },
                None,
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                let mut conn = self.__autumn_acquire_conn().await?;
                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| async move {
                    let record = #table_ident::table.find(id)
                        .for_update()
                        .first::<#model_name>(conn)
                        .await
                        .optional()
                        .map_err(::autumn_web::AutumnError::from)?
                        .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ))?;
                    let __count = ::autumn_web::reexports::diesel::delete(#table_ident::table.find(id))
                        .execute(conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    #vh_insert
                    Ok(())
                }.scope_boxed())
                .await
            }
        } else {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_conn().await?;
                let delete_query = #table_ident::table.find(id);
                let __count = ::autumn_web::reexports::diesel::delete(delete_query)
                    .execute(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                if __count == 0 {
                    return Err(::autumn_web::AutumnError::not_found_msg(
                        format!("{} with id {} not found", stringify!(#model_name), id)
                    ));
                }
                Ok(())
            }
        };

        let save_many_body = if config.tenant_scoped && config.versioned {
            let vh_r = vh_insert_ts(
                table_name,
                "insert",
                false,
                &quote! { r },
                None,
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;
                if new.is_empty() {
                    return Ok(Vec::new());
                }
                let tenant_id = if self.across_tenants {
                    ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let tenant_id = tenant_id.as_ref();
                let mut conn = self.__autumn_acquire_conn().await?;

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut inserted = Vec::new();
                        let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                        let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                        for chunk in new.chunks(chunk_size) {
                            let chunk_inserted = if let ::core::option::Option::Some(t) = tenant_id {
                                let values: Vec<_> = chunk.iter().cloned().map(|item| ::autumn_web::tenancy::TenantInsertable::tenant_values(item, t)).collect();
                                ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(values)
                                    .get_results::<#model_name>(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(chunk.to_vec())
                                    .get_results::<#model_name>(conn)
                                    .await
                            }
                            .map_err(::autumn_web::AutumnError::from)?;
                            for r in &chunk_inserted {
                                #vh_r
                            }
                            inserted.extend(chunk_inserted);
                        }
                        Ok(inserted)
                    }
                    .scope_boxed()
                })
                .await
            }
        } else if config.tenant_scoped {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;
                if new.is_empty() {
                    return Ok(Vec::new());
                }
                let tenant_id = if self.across_tenants {
                    ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let tenant_id = tenant_id.as_ref();
                let mut conn = self.__autumn_acquire_conn().await?;

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut inserted = Vec::new();
                        let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                        let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                        for chunk in new.chunks(chunk_size) {
                            let chunk_inserted = if let ::core::option::Option::Some(t) = tenant_id {
                                let values: Vec<_> = chunk.iter().cloned().map(|item| ::autumn_web::tenancy::TenantInsertable::tenant_values(item, t)).collect();
                                ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(values)
                                    .get_results::<#model_name>(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                    .values(chunk.to_vec())
                                    .get_results::<#model_name>(conn)
                                    .await
                            }
                            .map_err(::autumn_web::AutumnError::from)?;
                            inserted.extend(chunk_inserted);
                        }
                        Ok(inserted)
                    }
                    .scope_boxed()
                })
                .await
            }
        } else if config.versioned {
            let vh_r = vh_insert_ts(
                table_name,
                "insert",
                false,
                &quote! { r },
                None,
                &quote! { conn },
                model_name,
            );
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;
                if new.is_empty() {
                    return Ok(Vec::new());
                }
                let mut conn = self.__autumn_acquire_conn().await?;

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut inserted = Vec::new();
                        let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                        let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                        for chunk in new.chunks(chunk_size) {
                            let chunk_inserted = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                .values(chunk.to_vec())
                                .get_results::<#model_name>(conn)
                                .await
                                .map_err(::autumn_web::AutumnError::from)?;
                            for r in &chunk_inserted {
                                #vh_r
                            }
                            inserted.extend(chunk_inserted);
                        }
                        Ok(inserted)
                    }
                    .scope_boxed()
                })
                .await
            }
        } else {
            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;
                if new.is_empty() {
                    return Ok(Vec::new());
                }
                let mut conn = self.__autumn_acquire_conn().await?;

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut inserted = Vec::new();
                        let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                        let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                        for chunk in new.chunks(chunk_size) {
                            let chunk_inserted = ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                                .values(chunk.to_vec())
                                .get_results::<#model_name>(conn)
                                .await
                                .map_err(::autumn_web::AutumnError::from)?;
                            inserted.extend(chunk_inserted);
                        }
                        Ok(inserted)
                    }
                    .scope_boxed()
                })
                .await
            }
        };

        let save_many_skip_invalid_body = {
            let vh_skip_batch = if config.versioned {
                let vh = vh_insert_ts(
                    table_name,
                    "insert",
                    false,
                    &quote! { r },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! { for r in &results { #vh } }
            } else {
                quote! {}
            };
            let vh_skip_row = if config.versioned {
                let vh = vh_insert_ts(
                    table_name,
                    "insert",
                    false,
                    &quote! { model },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! { #vh }
            } else {
                quote! {}
            };

            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            let insert_expr_conn = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        let values: Vec<_> = chunk.iter().cloned().map(|item| ::autumn_web::tenancy::TenantInsertable::tenant_values(item, t)).collect();
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(values)
                            .get_results::<#model_name>(conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(chunk.to_vec())
                            .get_results::<#model_name>(conn)
                            .await
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                        .values(chunk.to_vec())
                        .get_results::<#model_name>(conn)
                        .await
                }
            };

            let row_insert_expr_conn = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        let values = ::autumn_web::tenancy::TenantInsertable::tenant_values(item.clone(), t);
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(values)
                            .get_result::<#model_name>(conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                            .values(item.clone())
                            .get_result::<#model_name>(conn)
                            .await
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::diesel::insert_into(#table_ident::table)
                        .values(item.clone())
                        .get_result::<#model_name>(conn)
                        .await
                }
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;

                if new.is_empty() {
                    return Ok((Vec::new(), Vec::new()));
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_conn().await?;
                let mut successes = Vec::new();
                let mut failures = Vec::new();

                let mut offset = 0;
                let cols = (&new[0]).__autumn_column_count() + #tenant_extra;
                let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                for chunk in new.chunks(chunk_size) {
                    let batch_res = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            let results = (#insert_expr_conn)
                                .map_err(::autumn_web::AutumnError::from)?;
                            #vh_skip_batch
                            Ok(results)
                        }
                        .scope_boxed()
                    })
                    .await;

                    match batch_res {
                        Ok(results) => {
                            successes.extend(results);
                        }
                        Err(batch_err) => {
                            let is_constraint_error = if let ::core::option::Option::Some(diesel_err) = batch_err.downcast_ref::<::autumn_web::reexports::diesel::result::Error>() {
                                match diesel_err {
                                    ::autumn_web::reexports::diesel::result::Error::DatabaseError(kind, _) => {
                                        match kind {
                                            ::autumn_web::reexports::diesel::result::DatabaseErrorKind::UniqueViolation |
                                            ::autumn_web::reexports::diesel::result::DatabaseErrorKind::ForeignKeyViolation |
                                            ::autumn_web::reexports::diesel::result::DatabaseErrorKind::NotNullViolation |
                                            ::autumn_web::reexports::diesel::result::DatabaseErrorKind::CheckViolation => true,
                                            _ => false,
                                        }
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            };

                            if !is_constraint_error {
                                return ::core::result::Result::Err(batch_err);
                            }

                            // Fallback to row-by-row insertion for this chunk
                            for (idx, item) in chunk.iter().enumerate() {
                                let global_idx = offset + idx;
                                let res = conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                                    async move {
                                        let model = (#row_insert_expr_conn)
                                            .map_err(::autumn_web::AutumnError::from)?;
                                        #vh_skip_row
                                        Ok(model)
                                    }
                                    .scope_boxed()
                                })
                                .await;
                                match res {
                                    Ok(model) => successes.push(model),
                                    Err(err) => failures.push((global_idx, err)),
                                }
                            }
                        }
                    }
                    offset += chunk.len();
                }
                Ok((successes, failures))
            }
        };

        let update_many_body = {
            let vh_update_pair = if config.versioned {
                let vh = vh_insert_ts(
                    table_name,
                    "update",
                    false,
                    &quote! { after_rec },
                    Some(&quote! { before_rec }),
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    for after_rec in &chunk_updated {
                        if let ::core::option::Option::Some(before_rec) = __vh_before_map.get(&after_rec.id) {
                            #vh
                        }
                    }
                }
            } else {
                quote! {}
            };
            let vh_build_before_map_from_current = if config.versioned {
                quote! {
                    let __vh_before_map: ::std::collections::HashMap<i64, #model_name> =
                        current_rows.iter().map(|r| (r.id, r.clone())).collect();
                }
            } else {
                quote! {}
            };
            let vh_load_before_map_no_lock_expr = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().load::<#model_name>(conn).await
                    } else {
                        load_query.for_update().load::<#model_name>(conn).await
                    }
                }
            } else {
                quote! {
                    load_query.for_update().load::<#model_name>(conn).await
                }
            };
            let vh_load_before_map_no_lock = if config.versioned {
                quote! {
                    let mut __vh_before_map = ::std::collections::HashMap::<i64, #model_name>::new();
                    for chunk in ids.chunks(1000) {
                        let load_query = #table_ident::table.filter(#table_ident::id.eq_any(chunk))
                            .order(#table_ident::id.asc());
                        let chunk_rows = #vh_load_before_map_no_lock_expr
                            .map_err(::autumn_web::AutumnError::from)?;
                        for row in chunk_rows {
                            __vh_before_map.insert(row.id, row);
                        }
                    }
                }
            } else {
                quote! {}
            };

            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            // The changeset is rebuilt fresh inside each chunk iteration below
            // (see `update_expr_conn`): encrypted columns route through diesel
            // `serialize_as`, which consumes the changeset, so it cannot be
            // borrowed or reused across loop iterations. Rebuilding per chunk
            // also avoids requiring `Clone` on hand-written changesets.
            let set_tenant_expr = quote! {};

            let load_expr = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        load_query.filter(#table_ident::tenant_id.eq(t)).for_update().load::<#model_name>(conn).await
                    } else {
                        load_query.for_update().load::<#model_name>(conn).await
                    }
                }
            } else {
                quote! {
                    load_query.for_update().load::<#model_name>(conn).await
                }
            };

            let update_expr_conn = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(t) = tenant_id {
                        let mut diesel_changeset = changes.__to_changeset();
                        {
                            use ::autumn_web::repository::CanSetTenantId as _;
                            diesel_changeset.set_tenant_id(t.clone());
                        }
                        ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::tenant_id.eq(t)))
                            .set(diesel_changeset)
                            .get_results::<#model_name>(conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)))
                            .set(changes.__to_changeset())
                            .get_results::<#model_name>(conn)
                            .await
                    }
                }
            } else {
                quote! {
                    ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)))
                        .set(changes.__to_changeset())
                        .get_results::<#model_name>(conn)
                        .await
                }
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::{AutumnLockVersionModelExt as _, AutumnLockVersionUpdateExt as _};

                if ids.is_empty() {
                    return Ok(Vec::new());
                }

                // Deduplicate IDs so each row is updated at most once and
                // the before-state map stays accurate across chunk boundaries.
                let __ids_deduped: Vec<i64> = {
                    let mut __seen = ::std::collections::HashSet::new();
                    ids.iter().filter(|&&id| __seen.insert(id)).copied().collect()
                };
                let ids: &[i64] = &__ids_deduped;

                #tenant_id_setup
                #set_tenant_expr
                let mut conn = self.__autumn_acquire_conn().await?;

                if let ::core::option::Option::Some(expected_version) = changes.__autumn_lock_version_expected() {
                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            // Load existing to verify versions
                            let mut current_rows = Vec::new();
                            for chunk in ids.chunks(1000) {
                                let load_query = #table_ident::table.filter(#table_ident::id.eq_any(chunk))
                                    .order(#table_ident::id.asc());
                                let chunk_rows = #load_expr
                                    .map_err(::autumn_web::AutumnError::from)?;
                                current_rows.extend(chunk_rows);
                            }

                            for current in &current_rows {
                                if let ::core::option::Option::Some(actual_version) = current.__autumn_lock_version_actual() {
                                    if actual_version != expected_version {
                                        return Err(::autumn_web::AutumnError::conflict(
                                            ::autumn_web::RepositoryError::Conflict {
                                                id: current.id,
                                                expected_version,
                                                actual_version: ::core::option::Option::Some(actual_version),
                                            },
                                        ));
                                    }
                                }
                            }

                            #vh_build_before_map_from_current

                            let mut updated = Vec::new();
                            for chunk in ids.chunks(1000) {
                                let chunk_updated = #update_expr_conn
                                    .map_err(::autumn_web::AutumnError::from)?;
                                #vh_update_pair
                                updated.extend(chunk_updated);
                            }
                            Ok(updated)
                        }
                        .scope_boxed()
                    })
                    .await
                } else {
                    conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                        async move {
                            #vh_load_before_map_no_lock

                            let mut updated = Vec::new();
                            for chunk in ids.chunks(1000) {
                                let chunk_updated = #update_expr_conn
                                    .map_err(::autumn_web::AutumnError::from)?;
                                #vh_update_pair
                                updated.extend(chunk_updated);
                            }
                            Ok(updated)
                        }
                        .scope_boxed()
                    })
                    .await
                }
            }
        };

        let delete_many_body = {
            let vh_delete_load_before = if config.versioned {
                // Soft-delete preload must mirror the actual delete filter so that
                // already-deleted rows are not snapshotted as newly deleted.
                let soft_delete_filter = if config.soft_delete {
                    quote! { .filter(#table_ident::deleted_at.is_null()) }
                } else {
                    quote! {}
                };
                let load_chunk = if config.tenant_scoped {
                    quote! {
                        let load_query = #table_ident::table
                            .filter(#table_ident::id.eq_any(chunk))
                            #soft_delete_filter;
                        let chunk_rows = if let ::core::option::Option::Some(t) = tenant_id {
                            load_query.filter(#table_ident::tenant_id.eq(t)).for_update().load::<#model_name>(conn).await
                        } else {
                            load_query.for_update().load::<#model_name>(conn).await
                        }
                        .map_err(::autumn_web::AutumnError::from)?;
                    }
                } else {
                    quote! {
                        let load_query = #table_ident::table
                            .filter(#table_ident::id.eq_any(chunk))
                            #soft_delete_filter;
                        let chunk_rows = load_query.for_update().load::<#model_name>(conn).await
                            .map_err(::autumn_web::AutumnError::from)?;
                    }
                };
                quote! {
                    // Deduplicate IDs so that duplicate inputs don't produce
                    // duplicate history entries across chunk boundaries.
                    let __vh_unique_ids: Vec<i64> = {
                        let mut __seen = ::std::collections::HashSet::new();
                        ids.iter().filter(|&&id| __seen.insert(id)).copied().collect()
                    };
                    let mut __vh_deleted_records: Vec<#model_name> = Vec::new();
                    for chunk in __vh_unique_ids.chunks(1000) {
                        #load_chunk
                        __vh_deleted_records.extend(chunk_rows);
                    }
                }
            } else {
                quote! {}
            };
            let vh_delete_write = if config.versioned {
                let vh = vh_insert_ts(
                    table_name,
                    "delete",
                    false,
                    &quote! { r },
                    None,
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    for r in &__vh_deleted_records {
                        #vh
                    }
                }
            } else {
                quote! {}
            };

            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                }
            } else {
                quote! {}
            };

            // When versioned, use RETURNING id so we only write history for
            // rows that were actually mutated (BEFORE triggers can suppress
            // deletes/updates without error).
            let (delete_loop, vh_delete_filter) = if config.versioned {
                let delete_returning_expr = if config.soft_delete {
                    if config.tenant_scoped {
                        quote! {
                            {
                                let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null());
                                if let ::core::option::Option::Some(t) = tenant_id {
                                    ::autumn_web::reexports::diesel::update(query.filter(#table_ident::tenant_id.eq(t)))
                                        .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                        .returning(#table_ident::id)
                                        .get_results::<i64>(conn)
                                        .await
                                } else {
                                    ::autumn_web::reexports::diesel::update(query)
                                        .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                        .returning(#table_ident::id)
                                        .get_results::<i64>(conn)
                                        .await
                                }
                            }
                        }
                    } else {
                        quote! {
                            ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null()))
                                .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                .returning(#table_ident::id)
                                .get_results::<i64>(conn)
                                .await
                        }
                    }
                } else if config.tenant_scoped {
                    quote! {
                        {
                            let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk));
                            if let ::core::option::Option::Some(t) = tenant_id {
                                ::autumn_web::reexports::diesel::delete(query.filter(#table_ident::tenant_id.eq(t)))
                                    .returning(#table_ident::id)
                                    .get_results::<i64>(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::delete(query)
                                    .returning(#table_ident::id)
                                    .get_results::<i64>(conn)
                                    .await
                            }
                        }
                    }
                } else {
                    quote! {
                        ::autumn_web::reexports::diesel::delete(#table_ident::table.filter(#table_ident::id.eq_any(chunk)))
                            .returning(#table_ident::id)
                            .get_results::<i64>(conn)
                            .await
                    }
                };
                let loop_ts = quote! {
                    let mut __vh_actually_deleted: ::std::collections::HashSet<i64> = ::std::collections::HashSet::new();
                    for chunk in ids.chunks(1000) {
                        let chunk_deleted_ids = #delete_returning_expr
                            .map_err(::autumn_web::AutumnError::from)?;
                        __vh_actually_deleted.extend(chunk_deleted_ids);
                    }
                };
                let filter_ts = quote! {
                    __vh_deleted_records.retain(|r| __vh_actually_deleted.contains(&r.id));
                };
                (loop_ts, filter_ts)
            } else {
                let delete_expr = if config.soft_delete {
                    if config.tenant_scoped {
                        quote! {
                            let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null());
                            if let ::core::option::Option::Some(t) = tenant_id {
                                ::autumn_web::reexports::diesel::update(query.filter(#table_ident::tenant_id.eq(t)))
                                    .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                    .execute(conn)
                                    .await
                            } else {
                                ::autumn_web::reexports::diesel::update(query)
                                    .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                    .execute(conn)
                                    .await
                            }
                        }
                    } else {
                        quote! {
                            ::autumn_web::reexports::diesel::update(#table_ident::table.filter(#table_ident::id.eq_any(chunk)).filter(#table_ident::deleted_at.is_null()))
                                .set(#table_ident::deleted_at.eq(::core::option::Option::Some(__now)))
                                .execute(conn)
                                .await
                        }
                    }
                } else if config.tenant_scoped {
                    quote! {
                        let query = #table_ident::table.filter(#table_ident::id.eq_any(chunk));
                        if let ::core::option::Option::Some(t) = tenant_id {
                            ::autumn_web::reexports::diesel::delete(query.filter(#table_ident::tenant_id.eq(t)))
                                .execute(conn)
                                .await
                        } else {
                            ::autumn_web::reexports::diesel::delete(query)
                                .execute(conn)
                                .await
                        }
                    }
                } else {
                    quote! {
                        ::autumn_web::reexports::diesel::delete(#table_ident::table.filter(#table_ident::id.eq_any(chunk)))
                            .execute(conn)
                            .await
                    }
                };
                let loop_ts = quote! {
                    for chunk in ids.chunks(1000) {
                        #delete_expr
                            .map_err(::autumn_web::AutumnError::from)?;
                    }
                };
                (loop_ts, quote! {})
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;

                if ids.is_empty() {
                    return Ok(());
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_conn().await?;
                let __now = ::autumn_web::reexports::chrono::Utc::now().naive_utc();

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        #vh_delete_load_before
                        #delete_loop
                        #vh_delete_filter
                        #vh_delete_write
                        Ok(())
                    }
                    .scope_boxed()
                })
                .await
            }
        };

        let upsert_many_body = {
            let vh_upsert_write = if config.versioned {
                let vh_ins = vh_insert_ts(
                    table_name,
                    "insert",
                    false,
                    &quote! { r },
                    None,
                    &quote! { conn },
                    model_name,
                );
                let vh_upd = vh_insert_ts(
                    table_name,
                    "update",
                    false,
                    &quote! { r },
                    Some(&quote! { before_rec }),
                    &quote! { conn },
                    model_name,
                );
                quote! {
                    for r in &chunk_upserted {
                        if let ::core::option::Option::Some(before_rec) = __vh_before_map.get(&r.id) {
                            #vh_upd
                        } else {
                            #vh_ins
                        }
                    }
                }
            } else {
                quote! {}
            };
            let vh_upsert_before_collect = if config.versioned {
                quote! {
                    let __vh_before_map: ::std::collections::HashMap<i64, #model_name> =
                        existing_rows.iter().map(|r| (r.id, r.clone())).collect();
                }
            } else {
                quote! {}
            };
            let vh_upsert_lock_keys = if config.versioned {
                let table_name = table_name.clone();
                quote! {
                    let mut __autumn_upsert_lock_ids: Vec<_> =
                        records.iter().map(|r| r.id).collect();
                    __autumn_upsert_lock_ids.sort_unstable();
                    __autumn_upsert_lock_ids.dedup();
                    for __autumn_upsert_lock_id in __autumn_upsert_lock_ids {
                        let __autumn_upsert_lock_key =
                            ::autumn_web::repository::repository_upsert_advisory_lock_key(
                                #table_name,
                                __autumn_upsert_lock_id,
                            );
                        ::autumn_web::reexports::diesel::sql_query("SELECT pg_advisory_xact_lock($1)")
                            .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(
                                __autumn_upsert_lock_key,
                            )
                            .execute(conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                    }
                }
            } else {
                quote! {}
            };

            let tenant_id_setup = if config.tenant_scoped {
                quote! {
                    let tenant_id = if self.across_tenants {
                        ::core::option::Option::None
                    } else {
                        let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                            .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                        ::core::option::Option::Some(t)
                    };
                    let tenant_id = tenant_id.as_ref();
                    let mut records = records.to_vec();
                    if let ::core::option::Option::Some(t) = tenant_id {
                        for record in &mut records {
                            ::autumn_web::tenancy::ModelTenantIdMeta::try_set_tenant_id(record, t);
                        }
                    }
                }
            } else {
                quote! {
                    let tenant_id: ::core::option::Option<::std::string::String> = ::core::option::Option::None;
                    let tenant_id = tenant_id.as_ref();
                    let records = records;
                }
            };

            let load_expr = if config.tenant_scoped {
                quote! {
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        #table_ident::table
                            .filter(#table_ident::id.eq_any(&chunk_ids))
                            .filter(#table_ident::tenant_id.eq(t.clone()))
                            .for_update()
                            .load::<#model_name>(conn)
                            .await
                    } else {
                        #table_ident::table
                            .filter(#table_ident::id.eq_any(&chunk_ids))
                            .for_update()
                            .load::<#model_name>(conn)
                            .await
                    }
                }
            } else {
                quote! {
                    #table_ident::table
                        .filter(#table_ident::id.eq_any(&chunk_ids))
                        .for_update()
                        .load::<#model_name>(conn)
                        .await
                }
            };

            let upsert_expr = quote! {
                let chunk_upserted = #model_name::__autumn_execute_upsert(
                    chunk,
                    tenant_id.map(|t| t.as_str()),
                    conn,
                )
                .await
                .map_err(::autumn_web::AutumnError::from)?;
            };

            let size_check = if config.tenant_scoped {
                quote! {
                    if has_lock && upserted.len() != records.len() {
                        return Err(::autumn_web::AutumnError::conflict_msg(
                            format!(
                                "Conflict: only {} of {} records were upserted (potential lock-version/optimistic lock or tenant conflict)",
                                upserted.len(),
                                records.len()
                            )
                        ));
                    } else if !has_lock && upserted.len() != records.len() {
                        return Err(::autumn_web::AutumnError::bad_request_msg(
                            format!(
                                "Tenant conflict: only {} of {} records were upserted (potential cross-tenant conflict)",
                                upserted.len(),
                                records.len()
                            )
                        ));
                    }
                }
            } else {
                quote! {
                    if has_lock && upserted.len() != records.len() {
                        return Err(::autumn_web::AutumnError::conflict_msg(
                            format!(
                                "Conflict: only {} of {} records were upserted (potential lock-version/optimistic lock conflict)",
                                upserted.len(),
                                records.len()
                            )
                        ));
                    }
                }
            };

            quote! {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
                use ::autumn_web::repository::AutumnColumnCountSpecific as _;
                use ::autumn_web::repository::AutumnColumnCountFallback as _;
                use ::autumn_web::repository::AutumnUpsertSetExt as _;
                use ::autumn_web::repository::AutumnLockVersionModelExt as _;
                use ::autumn_web::repository::AutumnUpsertExecutionExt as _;


                if records.is_empty() {
                    return Ok(Vec::new());
                }

                let mut unique_ids = ::std::collections::HashSet::new();
                for record in records.iter() {
                    if !unique_ids.insert(record.id) {
                        return Err(::autumn_web::AutumnError::bad_request_msg(
                            format!("Duplicate record ID detected in bulk upsert: {}", record.id)
                        ));
                    }
                }

                let mut has_lock = false;
                if let ::core::option::Option::Some(first_rec) = records.first() {
                    if first_rec.__autumn_lock_version_actual().is_some() {
                        has_lock = true;
                    }
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_conn().await?;

                conn.transaction::<_, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let mut upserted = Vec::new();
                        let cols = (&records[0]).__autumn_column_count() + #tenant_extra;
                        let chunk_size = if cols == 0 { 1000 } else { (65535usize / cols).min(1000).max(1) };
                        #vh_upsert_lock_keys
                        for chunk in records.chunks(chunk_size) {
                            let chunk_ids: Vec<_> = chunk.iter().map(|r| r.id).collect();
                            let existing_rows = #load_expr
                                .map_err(::autumn_web::AutumnError::from)?;

                            if has_lock {
                                for existing in &existing_rows {
                                    if let ::core::option::Option::Some(db_lock) = existing.__autumn_lock_version_actual() {
                                        if let ::core::option::Option::Some(incoming) = chunk.iter().find(|r| r.id == existing.id) {
                                            if let ::core::option::Option::Some(incoming_lock) = incoming.__autumn_lock_version_actual() {
                                                if incoming_lock != db_lock {
                                                    return Err(::autumn_web::AutumnError::conflict(
                                                        ::autumn_web::RepositoryError::Conflict {
                                                            id: existing.id,
                                                            expected_version: incoming_lock,
                                                            actual_version: ::core::option::Option::Some(db_lock),
                                                        },
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            #vh_upsert_before_collect

                            #upsert_expr

                            #vh_upsert_write

                            upserted.extend(chunk_upserted);
                        }
                        #size_check
                        Ok(upserted)
                    }
                    .scope_boxed()
                })
                .await
            }
        };

        (
            struct_fields,
            clone_impl,
            extractor_init,
            save_body,
            update_body,
            delete_body,
            quote! {},
            quote! {},
            save_many_body,
            save_many_skip_invalid_body,
            update_many_body,
            delete_many_body,
            upsert_many_body,
        )
    };

    // §1d: cross-shard write guard.  When `sharded + tenant_scoped`, writes
    // that arrive with `across_tenants = true` AND `__autumn_shards.is_some()`
    // cannot be safely fanned-out (no cross-shard transaction semantics), so we
    // return a clear error instead of silently hitting only one shard. Shared
    // by the core write bodies below and the `upsert_many` body further down.
    let cross_shard_write_guard = if config.sharded && config.tenant_scoped {
        quote! {
            if self.across_tenants {
                if self.__autumn_shards.is_some() {
                    return ::core::result::Result::Err(
                        ::autumn_web::AutumnError::bad_request_msg(
                            "cross-shard writes are not supported: \
                             across_tenants() cannot be used for mutation \
                             on a sharded repository"
                        )
                    );
                }
            }
        }
    } else {
        quote! {}
    };

    let (
        save_body,
        update_body,
        delete_body,
        save_many_body,
        save_many_skip_invalid_body,
        update_many_body,
        delete_many_body,
    ) = if config.sharded && config.tenant_scoped {
        let write_guard = &cross_shard_write_guard;
        (
            quote! { #write_guard #save_body },
            quote! { #write_guard #update_body },
            quote! { #write_guard #delete_body },
            quote! { #write_guard #save_many_body },
            quote! { #write_guard #save_many_skip_invalid_body },
            quote! { #write_guard #update_many_body },
            quote! { #write_guard #delete_many_body },
        )
    } else {
        (
            save_body,
            update_body,
            delete_body,
            save_many_body,
            save_many_skip_invalid_body,
            update_many_body,
            delete_many_body,
        )
    };

    // Compute inline broadcast tokens for broadcasts-only repos (no commit_hooks).
    // These mirror the commit-hook broadcast tokens but use the result value from
    // the wrapped async block (`__autumn_result` / `__autumn_record`) instead of
    // the deserialized commit-hook payload.
    let (
        inline_broadcast_create,
        inline_broadcast_update,
        inline_broadcast_delete,
        inline_update_prefetch,
        inline_delete_prefetch,
        inline_broadcast_update_many,
        inline_delete_many_prefetch,
        inline_broadcast_delete_many,
    ) = if config.broadcasts && !commit_hooks_enabled {
        let base_topic_expr_outer = match generate_topic_format(
            config
                .broadcast_topic
                .as_deref()
                .unwrap_or(&config.table_name),
            &quote! { __record_ref },
        ) {
            Ok(expr) => expr,
            Err(err) => {
                let compile_err = err.to_compile_error();
                return quote! { #compile_err };
            }
        };

        let topic_expr_outer = if config.tenant_scoped {
            quote! {
                ::std::format!(
                    "tenant:{}:{}",
                    ::autumn_web::tenancy::DisplayTenantId::tenant_id_str(
                        &__record_ref.tenant_id
                    ),
                    #base_topic_expr_outer
                )
            }
        } else {
            base_topic_expr_outer
        };

        let model_prefix_outer = to_snake_case(&config.model_name.to_string());
        let default_container_outer = format!("{}-list", config.table_name);
        let container_expr_outer = config
            .broadcast_container
            .as_deref()
            .unwrap_or(&default_container_outer);

        let (render_expr_outer, create_swap_id_outer, create_swap_outer, update_id_outer) =
            if let Some(ref render_path) = config.broadcast_render {
                let extract_id = quote! {
                    ::autumn_web::htmx::extract_html_id(
                        &{#render_path(__record_ref)}.into_string()
                    )
                    .unwrap_or_else(|| ::std::format!(
                        "{}-{}",
                        #model_prefix_outer,
                        ::autumn_web::repository::ModelPrimaryKey::primary_key_value(
                            __record_ref
                        )
                    ))
                };
                (
                    quote! { #render_path(__record_ref) },
                    quote! { #container_expr_outer.to_string() },
                    quote! { ::autumn_web::htmx::OobSwap::BeforeEnd },
                    extract_id,
                )
            } else {
                (
                    quote! {
                        <#model_name as ::autumn_web::live::LiveFragment>::render_fragment(
                            __record_ref
                        )
                    },
                    quote! { #container_expr_outer.to_string() },
                    quote! {
                        <#model_name as ::autumn_web::live::LiveFragment>::insert_swap()
                    },
                    quote! {
                        <#model_name as ::autumn_web::live::LiveFragment>::dom_id(__record_ref)
                    },
                )
            };

        // Determine whether a pre-fetch is needed before the delete/update body.
        // Pre-fetch is required when:
        //  - the topic is dynamic (contains a field placeholder like "{category}"),
        //    because after deletion the record is gone and the topic cannot be
        //    interpolated; and for updates we need the pre-mutation topic to detect
        //    topic changes and publish a delete on the old channel.
        //  - broadcast_render is configured, because the custom render fn must run
        //    on the live record to extract the real DOM id for delete broadcasts.
        //
        // The simple case (static topic, no broadcast_render — what `--live`
        // scaffolds) keeps the existing fast, zero-extra-query path.
        let raw_topic_outer = config
            .broadcast_topic
            .as_deref()
            .unwrap_or(&config.table_name);
        let topic_is_dynamic_outer = raw_topic_outer.contains('{');
        // Tenant-scoped repos also need a prefetch on delete: the topic includes the
        // record's tenant_id (`"tenant:{id}:table"`), which is only available from the
        // live row, so we must read it before the DELETE.
        let delete_needs_prefetch =
            topic_is_dynamic_outer || config.broadcast_render.is_some() || config.tenant_scoped;
        // broadcast_render is included: when a custom render fn encodes a mutable field
        // in the element id, the pre-mutation id may differ from the post-mutation id.
        // Capturing __autumn_prev_id lets the update broadcast target the old element
        // even after its id has changed (OobSwap::Target(OuterHTML, "#old-id")).
        let update_needs_prefetch = topic_is_dynamic_outer || config.broadcast_render.is_some();

        // Static fallbacks retained for the non-prefetch path and as the
        // graceful-degradation fallback when the pre-fetch returns None.
        let static_delete_topic_outer: String = {
            if topic_is_dynamic_outer {
                config.table_name.clone()
            } else {
                raw_topic_outer.to_owned()
            }
        };
        // Always fall back to dom_id_for: it uses the user's defined id scheme
        // (e.g. "live-pf-post-{id}" with dashes) rather than the snake_case
        // model_prefix approximation ("live_pf_post-{id}") which would miss its target.
        let inline_delete_id_outer =
            quote! { <#model_name as ::autumn_web::live::LiveFragment>::dom_id_for(id) };

        let ic = quote! {
            if let ::core::result::Result::Ok(ref __autumn_record) = __autumn_result {
                let __record_ref = __autumn_record;
                if let ::core::option::Option::Some(__channels) =
                    ::autumn_web::__private::get_global_channels()
                {
                    let __topic = #topic_expr_outer;
                    let __fragment = #render_expr_outer;
                    let __create_id = #create_swap_id_outer;
                    let __create_swap = #create_swap_outer;
                    if let ::core::result::Result::Err(__err) = __channels
                        .broadcast()
                        .publish_oob(&__topic, &__create_id, &__create_swap, &__fragment)
                    {
                        ::autumn_web::reexports::tracing::warn!(
                            error = %__err,
                            "auto-broadcast create failed"
                        );
                    }
                }
            }
        };

        // ── Update broadcast ─────────────────────────────────────────────
        // When the topic is dynamic, a pre-fetch captures the pre-mutation topic
        // (via `inline_update_prefetch` below) so we can detect topic changes and
        // publish a delete on the old channel before inserting on the new one —
        // mirroring the commit-hook `__autumn_previous_topic` logic.
        let inline_update_prefetch = if update_needs_prefetch {
            quote! {
                let (__autumn_prev_topic, __autumn_prev_id):
                    (::core::option::Option<::std::string::String>,
                     ::core::option::Option<::std::string::String>) =
                    match self.find_by_id(id).await {
                        ::core::result::Result::Ok(
                            ::core::option::Option::Some(ref __record_ref),
                        ) => (
                            ::core::option::Option::Some(#topic_expr_outer),
                            ::core::option::Option::Some(#update_id_outer),
                        ),
                        ::core::result::Result::Ok(::core::option::Option::None) => {
                            (::core::option::Option::None, ::core::option::Option::None)
                        }
                        ::core::result::Result::Err(ref __pf_err) => {
                            ::autumn_web::reexports::tracing::warn!(
                                error = %__pf_err,
                                "auto-broadcast update pre-fetch failed; \
                                 topic-change detection skipped"
                            );
                            (::core::option::Option::None, ::core::option::Option::None)
                        }
                    };
            }
        } else {
            quote! {}
        };

        let iu = if update_needs_prefetch {
            quote! {
                if let ::core::result::Result::Ok(ref __autumn_record) = __autumn_result {
                    let __record_ref = __autumn_record;
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        let __topic = #topic_expr_outer;
                        let __fragment = #render_expr_outer;
                        let __id = #update_id_outer;

                        let __topic_changed = __autumn_prev_topic
                            .as_deref()
                            .map_or(false, |__prev| __prev != __topic);

                        if __topic_changed {
                            if let ::core::option::Option::Some(ref __prev_topic) =
                                __autumn_prev_topic
                            {
                                let __delete_id =
                                    __autumn_prev_id.as_deref().unwrap_or(&__id);
                                let __delete_fragment = ::autumn_web::html! {};
                                if let ::core::result::Result::Err(__err) = __channels
                                    .broadcast()
                                    .publish_oob(
                                        __prev_topic,
                                        __delete_id,
                                        &::autumn_web::htmx::OobSwap::Delete,
                                        &__delete_fragment,
                                    )
                                {
                                    ::autumn_web::reexports::tracing::warn!(
                                        error = %__err,
                                        "auto-broadcast delete of old topic failed"
                                    );
                                }
                            }
                        }

                        let (__target_id, __swap_strategy): (
                            ::std::string::String,
                            ::autumn_web::htmx::OobSwap,
                        ) = if __topic_changed {
                            (
                                #container_expr_outer.to_string(),
                                <#model_name as ::autumn_web::live::LiveFragment>::insert_swap(),
                            )
                        } else {
                            let __strategy = if let ::core::option::Option::Some(
                                ref __prev_id_val,
                            ) = __autumn_prev_id
                            {
                                if __prev_id_val != &__id {
                                    ::autumn_web::htmx::OobSwap::Target(
                                        ::autumn_web::htmx::OobMethod::OuterHTML,
                                        ::std::format!("#{}", __prev_id_val),
                                    )
                                } else {
                                    ::autumn_web::htmx::OobSwap::OuterHTML
                                }
                            } else {
                                ::autumn_web::htmx::OobSwap::OuterHTML
                            };
                            (__id.clone(), __strategy)
                        };

                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(
                                &__topic,
                                &__target_id,
                                &__swap_strategy,
                                &__fragment,
                            )
                        {
                            ::autumn_web::reexports::tracing::warn!(
                                error = %__err,
                                "auto-broadcast update failed"
                            );
                        }
                    }
                }
            }
        } else {
            quote! {
                if let ::core::result::Result::Ok(ref __autumn_record) = __autumn_result {
                    let __record_ref = __autumn_record;
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        let __topic = #topic_expr_outer;
                        let __fragment = #render_expr_outer;
                        let __id = #update_id_outer;
                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(
                                &__topic,
                                &__id,
                                &::autumn_web::htmx::OobSwap::OuterHTML,
                                &__fragment,
                            )
                        {
                            ::autumn_web::reexports::tracing::warn!(
                                error = %__err,
                                "auto-broadcast update failed"
                            );
                        }
                    }
                }
            }
        };

        // ── Delete broadcast ─────────────────────────────────────────────
        // When the topic is dynamic or broadcast_render is set, a pre-fetch loads
        // the record before the DELETE so that the correct topic and DOM id are
        // available even after the row is gone.  The `inline_delete_prefetch` token
        // stream (below) populates `__autumn_prefetched` before the body runs.
        let inline_delete_prefetch = if delete_needs_prefetch {
            quote! {
                let __autumn_prefetched: ::core::option::Option<#model_name> =
                    match self.find_by_id(id).await {
                        ::core::result::Result::Ok(v) => v,
                        ::core::result::Result::Err(ref __pf_err) => {
                            ::autumn_web::reexports::tracing::warn!(
                                error = %__pf_err,
                                "auto-broadcast delete pre-fetch failed; \
                                 static-topic fallback used"
                            );
                            ::core::option::Option::None
                        }
                    };
            }
        } else {
            quote! {}
        };

        let id_ = if delete_needs_prefetch {
            quote! {
                if let ::core::result::Result::Ok(()) = __autumn_result {
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        let (__del_topic, __del_id): (
                            ::std::string::String,
                            ::std::string::String,
                        ) = if let ::core::option::Option::Some(ref __record_ref) =
                            __autumn_prefetched
                        {
                            (#topic_expr_outer, #update_id_outer)
                        } else {
                            (
                                #static_delete_topic_outer.to_string(),
                                #inline_delete_id_outer,
                            )
                        };
                        let __fragment = ::autumn_web::html! {};
                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(
                                &__del_topic,
                                &__del_id,
                                &::autumn_web::htmx::OobSwap::Delete,
                                &__fragment,
                            )
                        {
                            ::autumn_web::reexports::tracing::warn!(
                                error = %__err,
                                "auto-broadcast delete failed"
                            );
                        }
                    }
                }
            }
        } else {
            quote! {
                if let ::core::result::Result::Ok(()) = __autumn_result {
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        let __del_id = #inline_delete_id_outer;
                        let __fragment = ::autumn_web::html! {};
                        if let ::core::result::Result::Err(__err) = __channels
                            .broadcast()
                            .publish_oob(
                                #static_delete_topic_outer,
                                &__del_id,
                                &::autumn_web::htmx::OobSwap::Delete,
                                &__fragment,
                            )
                        {
                            ::autumn_web::reexports::tracing::warn!(
                                error = %__err,
                                "auto-broadcast delete failed"
                            );
                        }
                    }
                }
            }
        };
        // ── update_many broadcast ─────────────────────────────────────────
        // Only broadcast OuterHTML for the simplest case: static topic + default render.
        // Skipped when update_needs_prefetch (dynamic topic OR custom render) because:
        // - dynamic topic: OuterHTML sent to the post-update topic leaves old-topic
        //   subscribers with stale elements and new-topic clients with a failed swap.
        // - custom render: the rendered element id may encode a mutable field; clients
        //   hold the element under the pre-update id, so a swap keyed by the post-update
        //   id misses its target.
        // Both cases require N pre-fetches to handle correctly; use commit_hooks = true.
        let ium = if update_needs_prefetch {
            quote! {}
        } else {
            quote! {
                if let ::core::result::Result::Ok(ref __autumn_result_vec) = __autumn_result {
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        for __autumn_record in __autumn_result_vec {
                            let __record_ref = __autumn_record;
                            let __topic = #topic_expr_outer;
                            let __fragment = #render_expr_outer;
                            let __id = #update_id_outer;
                            if let ::core::result::Result::Err(__err) = __channels
                                .broadcast()
                                .publish_oob(
                                    &__topic,
                                    &__id,
                                    &::autumn_web::htmx::OobSwap::OuterHTML,
                                    &__fragment,
                                )
                            {
                                ::autumn_web::reexports::tracing::warn!(
                                    error = %__err,
                                    "auto-broadcast update_many failed"
                                );
                            }
                        }
                    }
                }
            }
        }; // end if topic_is_dynamic_outer else branch

        // ── delete_many broadcast ─────────────────────────────────────────
        // When the topic is dynamic or broadcast_render is set, each record is
        // pre-fetched (via find_by_id) before the bulk DELETE so the correct
        // topic and DOM id are available after the rows are gone.
        // NOTE: this is N sequential queries for N ids.  Use commit_hooks = true
        // for better performance on large bulk deletes with dynamic topics.
        let inline_delete_many_prefetch = if delete_needs_prefetch {
            quote! {
                let __autumn_dm_pf: ::std::vec::Vec<(
                    ::std::string::String,
                    ::std::string::String,
                )> = {
                    let mut __pf_results = ::std::vec::Vec::new();
                    if ::autumn_web::__private::get_global_channels().is_some() {
                        for &id in ids {
                            match self.find_by_id(id).await {
                                ::core::result::Result::Ok(
                                    ::core::option::Option::Some(ref __record_ref),
                                ) => {
                                    __pf_results.push((#topic_expr_outer, #update_id_outer));
                                }
                                ::core::result::Result::Ok(
                                    ::core::option::Option::None,
                                ) => {}
                                ::core::result::Result::Err(ref __pf_err) => {
                                    ::autumn_web::reexports::tracing::warn!(
                                        error = %__pf_err,
                                        id,
                                        "auto-broadcast delete_many pre-fetch failed for id; \
                                         static-topic fallback used"
                                    );
                                    __pf_results.push((
                                        #static_delete_topic_outer.to_string(),
                                        <#model_name as ::autumn_web::live::LiveFragment>
                                            ::dom_id_for(id),
                                    ));
                                }
                            }
                        }
                    }
                    __pf_results
                };
            }
        } else {
            quote! {}
        };

        let idm = if delete_needs_prefetch {
            quote! {
                if let ::core::result::Result::Ok(()) = __autumn_result {
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        let __fragment = ::autumn_web::html! {};
                        for (__del_topic, __del_id) in &__autumn_dm_pf {
                            if let ::core::result::Result::Err(__err) = __channels
                                .broadcast()
                                .publish_oob(
                                    __del_topic,
                                    __del_id,
                                    &::autumn_web::htmx::OobSwap::Delete,
                                    &__fragment,
                                )
                            {
                                ::autumn_web::reexports::tracing::warn!(
                                    error = %__err,
                                    "auto-broadcast delete_many failed"
                                );
                            }
                        }
                    }
                }
            }
        } else {
            quote! {
                if let ::core::result::Result::Ok(()) = __autumn_result {
                    if let ::core::option::Option::Some(__channels) =
                        ::autumn_web::__private::get_global_channels()
                    {
                        let __fragment = ::autumn_web::html! {};
                        for &id in ids {
                            let __del_id = <#model_name as ::autumn_web::live::LiveFragment>
                                ::dom_id_for(id);
                            if let ::core::result::Result::Err(__err) = __channels
                                .broadcast()
                                .publish_oob(
                                    #static_delete_topic_outer,
                                    &__del_id,
                                    &::autumn_web::htmx::OobSwap::Delete,
                                    &__fragment,
                                )
                            {
                                ::autumn_web::reexports::tracing::warn!(
                                    error = %__err,
                                    "auto-broadcast delete_many failed"
                                );
                            }
                        }
                    }
                }
            }
        };

        (
            ic,
            iu,
            id_,
            inline_update_prefetch,
            inline_delete_prefetch,
            ium,
            inline_delete_many_prefetch,
            idm,
        )
    } else {
        (
            quote! {},
            quote! {},
            quote! {},
            quote! {},
            quote! {},
            quote! {},
            quote! {},
            quote! {},
        )
    };

    // For repos that declare `broadcasts = true` but have no commit_hooks, wire
    // inline broadcasts directly into the save / update / delete method bodies.
    // The commit-hook path already includes the broadcasts in the durable worker;
    // this path fires them synchronously after each mutation instead.
    //
    // When the topic is dynamic or broadcast_render is set, a pre-fetch step runs
    // before the mutation body so that topic interpolation and DOM-id extraction can
    // operate on the live record (delete) or detect topic changes (update).
    let (save_body, update_body, delete_body, update_many_body, delete_many_body) =
        if config.broadcasts && !commit_hooks_enabled {
            (
                quote! {
                    let __autumn_result: ::autumn_web::AutumnResult<#model_name> =
                        async { #save_body }.await;
                    #inline_broadcast_create
                    __autumn_result
                },
                quote! {
                    #inline_update_prefetch
                    let __autumn_result: ::autumn_web::AutumnResult<#model_name> =
                        async { #update_body }.await;
                    #inline_broadcast_update
                    __autumn_result
                },
                quote! {
                    #inline_delete_prefetch
                    let __autumn_result: ::autumn_web::AutumnResult<()> =
                        async { #delete_body }.await;
                    #inline_broadcast_delete
                    __autumn_result
                },
                quote! {
                    let __autumn_result: ::autumn_web::AutumnResult<
                        ::std::vec::Vec<#model_name>,
                    > = async { #update_many_body }.await;
                    #inline_broadcast_update_many
                    __autumn_result
                },
                quote! {
                    #inline_delete_many_prefetch
                    let __autumn_result: ::autumn_web::AutumnResult<()> =
                        async { #delete_many_body }.await;
                    #inline_broadcast_delete_many
                    __autumn_result
                },
            )
        } else {
            (
                save_body,
                update_body,
                delete_body,
                update_many_body,
                delete_many_body,
            )
        };

    let route_hook_registration = if commit_hooks_enabled {
        quote! { #pg_name::__autumn_register_repository_commit_hooks(); }
    } else {
        quote! {}
    };
    let versioned_inventory_registration = if config.versioned {
        quote! {
            ::autumn_web::reexports::inventory::submit! {
                ::autumn_web::__private::VersionedRepositoryDescriptor
            }
        }
    } else {
        quote! {}
    };

    // ── Pagination methods (`page` always; `cursor_page` when cursor_key is declared) ──
    //
    // `page` executes a COUNT(*) + a LIMIT/OFFSET query and wraps the result in
    // `Page<Model>`. `cursor_page` uses keyset pagination on the primary key `id`
    // (always i64 per the Autumn PK convention) so the cursor is stable and
    // requires no knowledge of the model's field types.  When `cursor_key = field`
    // is declared, the query also orders by that field (descending) as a secondary
    // sort key; the cursor payload remains the last-seen `id` so that filtering
    // is always correct.
    let pagination_trait_method = quote! {
        /// Fetch one page of records using offset pagination.
        ///
        /// Accepts a [`::autumn_web::pagination::PageRequest`] extractor value
        /// and returns a [`::autumn_web::pagination::Page`] containing the items
        /// together with total-elements / total-pages metadata.
        fn page(&self, req: &::autumn_web::pagination::PageRequest)
            -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>>> + Send;
    };

    // §1d: cross-shard paginate guard token stream (empty when not sharded).
    let page_cross_shard_guard = if config.sharded && config.tenant_scoped {
        quote! {
            if self.across_tenants {
                if self.__autumn_shards.is_some() {
                    return ::core::result::Result::Err(
                        ::autumn_web::AutumnError::bad_request_msg(
                            "cross-shard pagination is not supported: \
                             use find_all() with across_tenants() on a \
                             sharded repository instead"
                        )
                    );
                }
            }
        }
    } else {
        quote! {}
    };

    let pagination_impl_method = if config.tenant_scoped {
        quote! {
            async fn page(
                &self,
                req: &::autumn_web::pagination::PageRequest,
            ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>> {
                #page_cross_shard_guard
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
                let mut conn = self.__autumn_acquire_read_conn().await?;

                let query = #table_ident::table;
                if let ::core::option::Option::Some(ref t) = tenant_id {
                    let total: i64 = query
                        .filter(#table_ident::tenant_id.eq(t))
                        #sd_filter
                        .count()
                        .get_result::<i64>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    let items: ::std::vec::Vec<#model_name> = query
                        .filter(#table_ident::tenant_id.eq(t))
                        #sd_filter
                        .order(#table_ident::id.desc())
                        .limit(req.limit())
                        .offset(req.offset())
                        .select(#model_name::as_select())
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    ::core::result::Result::Ok(::autumn_web::pagination::Page::new(items, total, req))
                } else {
                    let total: i64 = query
                        #sd_filter
                        .count()
                        .get_result::<i64>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    let items: ::std::vec::Vec<#model_name> = query
                        #sd_filter
                        .order(#table_ident::id.desc())
                        .limit(req.limit())
                        .offset(req.offset())
                        .select(#model_name::as_select())
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    ::core::result::Result::Ok(::autumn_web::pagination::Page::new(items, total, req))
                }
            }
        }
    } else {
        quote! {
            async fn page(
                &self,
                req: &::autumn_web::pagination::PageRequest,
            ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_read_conn().await?;
                let total: i64 = #table_ident::table
                    #sd_filter
                    .count()
                    .get_result::<i64>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                let items: ::std::vec::Vec<#model_name> = #table_ident::table
                    #sd_filter
                    .order(#table_ident::id.desc())
                    .limit(req.limit())
                    .offset(req.offset())
                    .select(#model_name::as_select())
                    .load::<#model_name>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)?;
                ::core::result::Result::Ok(::autumn_web::pagination::Page::new(items, total, req))
            }
        }
    };

    // `cursor_page` is only generated when the user declares `cursor_key = field`.
    //
    // Two modes depending on whether `cursor_key_type` is also declared:
    //
    // **With `cursor_key_type = Type`** (always correct):
    //   Cursor payload is `(Type, i64)`.  The WHERE clause advances the
    //   `(cursor_key DESC, id DESC)` sort order exactly:
    //     WHERE (cursor_key < after_k) OR (cursor_key = after_k AND id < after_id)
    //
    // **Without `cursor_key_type`** (correct for correlated cursor_key / id):
    //   Cursor payload is `id` (i64) only.  The filter is `id < after_id`.
    //   This is correct when cursor_key values are monotonically correlated
    //   with id (e.g. `created_at` on an auto-increment table).  For
    //   non-monotonic data (backfills, imports) implement cursor_page manually.
    let tenant_query_filter = if config.tenant_scoped {
        quote! {
            if !self.across_tenants {
                let tenant_id = match ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten() {
                    ::core::option::Option::Some(t) => t,
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(::autumn_web::AutumnError::internal_server_error_msg(
                            "no tenant context was established"
                        ));
                    }
                };
                query = query.filter(#table_ident::tenant_id.eq(tenant_id));
            }
        }
    } else {
        quote! {}
    };

    let (cursor_page_trait_method, cursor_page_impl_method) = if let Some(ref ck) =
        config.cursor_key
    {
        let cursor_key_ident = format_ident!("{ck}");
        let trait_method = quote! {
            /// Fetch one page of records using keyset (cursor) pagination.
            ///
            /// The cursor token is opaque ΓÇö encode / decode it via
            /// [`::autumn_web::pagination::CursorRequest`].  The result is a
            /// [`::autumn_web::pagination::CursorPage`] containing a
            /// `next_cursor` token for the following page.
            fn cursor_page(&self, req: &::autumn_web::pagination::CursorRequest)
                -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<::autumn_web::pagination::CursorPage<#model_name>>> + Send;
        };
        // §1d: cross-shard cursor_page guard (empty when not sharded).
        let cursor_cross_shard_guard = if config.sharded && config.tenant_scoped {
            quote! {
                if self.across_tenants {
                    if self.__autumn_shards.is_some() {
                        return ::core::result::Result::Err(
                            ::autumn_web::AutumnError::bad_request_msg(
                                "cross-shard cursor pagination is not supported: \
                                 use find_all() with across_tenants() on a \
                                 sharded repository instead"
                            )
                        );
                    }
                }
            }
        } else {
            quote! {}
        };

        let impl_method = if let Some(ref key_type) = config.cursor_key_type {
            // Full two-part keyset filter ΓÇö always correct regardless of whether
            // cursor_key and id are monotonically correlated.
            quote! {
                async fn cursor_page(
                    &self,
                    req: &::autumn_web::pagination::CursorRequest,
                ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::CursorPage<#model_name>> {
                    #cursor_cross_shard_guard
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let mut query = #table_ident::table.into_boxed();
                    #tenant_query_filter
                    if let ::core::option::Option::Some((after_k, after_id)) =
                        req.decode::<(#key_type, i64)>()
                    {
                        query = query.filter(
                            #table_ident::#cursor_key_ident.lt(after_k.clone()).or(
                                #table_ident::#cursor_key_ident
                                    .eq(after_k)
                                    .and(#table_ident::id.lt(after_id)),
                            ),
                        );
                    }
                    // Apply soft-delete filter when enabled.
                    #[allow(unused_mut)]
                    let mut query = query #sd_filter;
                    let items: ::std::vec::Vec<#model_name> = query
                        .order((#table_ident::#cursor_key_ident.desc(), #table_ident::id.desc()))
                        .limit(req.fetch_limit())
                        .select(#model_name::as_select())
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    ::core::result::Result::Ok(
                        ::autumn_web::pagination::CursorPage::from_overfetched(
                            items,
                            req,
                            |row| (row.#cursor_key_ident.clone(), row.id),
                        )
                    )
                }
            }
        } else {
            // id-only cursor ΓÇö correct when cursor_key and id are monotonically
            // correlated (the common case for created_at + auto-increment id).
            quote! {
                async fn cursor_page(
                    &self,
                    req: &::autumn_web::pagination::CursorRequest,
                ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::CursorPage<#model_name>> {
                    #cursor_cross_shard_guard
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let mut query = #table_ident::table.into_boxed();
                    #tenant_query_filter
                    if let ::core::option::Option::Some(after_id) = req.decode::<i64>() {
                        query = query.filter(#table_ident::id.lt(after_id));
                    }
                    // Apply soft-delete filter when enabled.
                    #[allow(unused_mut)]
                    let mut query = query #sd_filter;
                    let items: ::std::vec::Vec<#model_name> = query
                        .order((#table_ident::#cursor_key_ident.desc(), #table_ident::id.desc()))
                        .limit(req.fetch_limit())
                        .select(#model_name::as_select())
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    ::core::result::Result::Ok(
                        ::autumn_web::pagination::CursorPage::from_overfetched(
                            items,
                            req,
                            |row| row.id,
                        )
                    )
                }
            }
        };
        (trait_method, impl_method)
    } else {
        (quote! {}, quote! {})
    };

    let tenant_scoped_traits = if config.tenant_scoped {
        quote! {
            impl ::autumn_web::tenancy::HasTenantIdColumn for #table_ident::table {
                type Column = #table_ident::tenant_id;
                fn column() -> Self::Column {
                    #table_ident::tenant_id
                }
            }

            impl<'a> ::autumn_web::tenancy::TenantInsertable<'a, #table_ident::table> for #new_name {
                type Values = <::autumn_web::tenancy::TenantInsertableValuesSelector<'a, Self, #table_ident::table, { <Self as ::autumn_web::tenancy::ModelTenantIdMeta>::HAS_MANUAL_TENANT_ID }> as ::autumn_web::tenancy::GetInsertableValues>::Values;

                fn tenant_values(self, tenant_id: &'a str) -> Self::Values {
                    ::autumn_web::tenancy::GetInsertableValues::get_values(::autumn_web::tenancy::TenantInsertableValuesSelector::<'a, Self, #table_ident::table, { <Self as ::autumn_web::tenancy::ModelTenantIdMeta>::HAS_MANUAL_TENANT_ID }> {
                        inner: self,
                        tenant_id,
                        _marker: ::core::marker::PhantomData,
                    })
                }
            }

            impl<'a> ::autumn_web::tenancy::TenantInsertable<'a, #table_ident::table> for #model_name {
                type Values = <::autumn_web::tenancy::TenantInsertableValuesSelector<'a, Self, #table_ident::table, { <Self as ::autumn_web::tenancy::ModelTenantIdMeta>::HAS_MANUAL_TENANT_ID }> as ::autumn_web::tenancy::GetInsertableValues>::Values;

                fn tenant_values(self, tenant_id: &'a str) -> Self::Values {
                    ::autumn_web::tenancy::GetInsertableValues::get_values(::autumn_web::tenancy::TenantInsertableValuesSelector::<'a, Self, #table_ident::table, { <Self as ::autumn_web::tenancy::ModelTenantIdMeta>::HAS_MANUAL_TENANT_ID }> {
                        inner: self,
                        tenant_id,
                        _marker: ::core::marker::PhantomData,
                    })
                }
            }
        }
    } else {
        quote! {}
    };

    // ── Build API handlers (when `api = "/path"` is present) ────────────
    let api_handlers = if let Some(ref api_path) = config.api_path {
        let prefix = to_snake_case(&model_name.to_string());

        let list_fn = format_ident!("{prefix}_api_list");
        let get_fn = format_ident!("{prefix}_api_get");
        let create_fn = format_ident!("{prefix}_api_create");
        let update_fn = format_ident!("{prefix}_api_update");
        let delete_fn = format_ident!("{prefix}_api_delete");

        let list_info = format_ident!("__autumn_route_info_{prefix}_api_list");
        let get_info = format_ident!("__autumn_route_info_{prefix}_api_get");
        let create_info = format_ident!("__autumn_route_info_{prefix}_api_create");
        let update_info = format_ident!("__autumn_route_info_{prefix}_api_update");
        let delete_info = format_ident!("__autumn_route_info_{prefix}_api_delete");

        let list_path_fn = format_ident!("__autumn_path_{prefix}_api_list");
        let get_path_fn = format_ident!("__autumn_path_{prefix}_api_get");
        let create_path_fn = format_ident!("__autumn_path_{prefix}_api_create");
        let update_path_fn = format_ident!("__autumn_path_{prefix}_api_update");
        let delete_path_fn = format_ident!("__autumn_path_{prefix}_api_delete");

        let id_path = format!("{api_path}/{{id}}");

        let has_policy = config.policy_type.is_some();
        let policy_check_show = if has_policy {
            quote! {
                ::autumn_web::authorization::__check_policy_scoped::<#model_name>(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                    "show",
                    &record,
                )
                .await?;
            }
        } else {
            quote! {}
        };
        // POST endpoint runs `can_create` *before* the insert so a
        // denied check never commits a row. Naive after-the-fact
        // policy checks would write the row, then return 403/404,
        // leaving the data behind.
        let policy_check_create_pre = if has_policy {
            quote! {
                ::autumn_web::authorization::__check_policy_create_payload_scoped::<#model_name>(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                    &__autumn_new_payload,
                )
                .await?;
            }
        } else {
            quote! {}
        };
        // Policy-backed create handlers keep the raw JSON value for
        // `can_create_payload` instead of serializing `NewModel` back
        // into JSON. That preserves hand-written `NewModel` types that
        // are `Deserialize + Insertable` but intentionally not `Serialize`.
        let create_payload_arg = if has_policy {
            quote! {
                ::autumn_web::prelude::Json(__autumn_new_payload): ::autumn_web::prelude::Json<
                    ::autumn_web::reexports::serde_json::Value
                >
            }
        } else {
            quote! {
                ::autumn_web::prelude::Json(new): ::autumn_web::prelude::Json<#new_name>
            }
        };
        let decode_create_payload = if has_policy {
            quote! {
                let new: #new_name = ::autumn_web::reexports::serde_json::from_value(
                    __autumn_new_payload.clone(),
                )
                .map_err(|err| ::autumn_web::AutumnError::unprocessable_msg(err.to_string()))?;
            }
        } else {
            quote! {}
        };
        let policy_check_update_pre = if has_policy {
            quote! {
                let __existing = repo.on_primary().find_by_id(id).await?
                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg("not found"))?;
                ::autumn_web::authorization::__check_policy_scoped::<#model_name>(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                    "update",
                    &__existing,
                )
                .await?;
            }
        } else {
            quote! {}
        };
        let policy_check_delete_pre = if has_policy {
            quote! {
                let __existing = repo.on_primary().find_by_id(id).await?
                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg("not found"))?;
                ::autumn_web::authorization::__check_policy_scoped::<#model_name>(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                    "delete",
                    &__existing,
                )
                .await?;
            }
        } else {
            quote! {}
        };
        let session_state_args = if has_policy {
            quote! {
                ::autumn_web::reexports::axum::extract::State(__autumn_state):
                    ::autumn_web::reexports::axum::extract::State<::autumn_web::AppState>,
                __autumn_session: ::autumn_web::session::Session,
                __autumn_token_scopes: ::core::option::Option<
                    ::autumn_web::reexports::axum::extract::Extension<
                        ::autumn_web::auth::ApiTokenScopes
                    >
                >,
                __autumn_idempotency_replay: ::core::option::Option<
                    ::autumn_web::reexports::axum::extract::Extension<
                        ::autumn_web::idempotency::IdempotencyReplayResponse
                    >
                >,
            }
        } else {
            quote! {}
        };
        // List endpoint behavior, in order of precedence:
        //
        // 1. `scope = SomeScope`: invoke the registered scope (the
        //    most efficient form ΓÇö the scope filters at the SQL level
        //    via Diesel).
        // 2. `policy = SomePolicy` without `scope`: load every
        //    record, then filter through `Policy::can_show` per row.
        //    Slower than (1) for large tables, but closes the
        //    "policy guards show/update/delete but list returns
        //    everything" data-exposure path. Users who care about
        //    perf should also set `scope = SomeScope`.
        // 3. Neither: plain `repo.find_all()` (public list).
        let scope_list_body = if config.scope_type.is_some() {
            quote! {
                let __scope = __autumn_state
                    .scope::<#model_name>()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg(
                        "missing scope registration"
                    ))?;
                let __ctx = ::autumn_web::authorization::PolicyContext::from_request_parts(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                ).await;
                let mut __conn = repo.__autumn_acquire_read_conn().await?;
                let records = __scope.list(&__ctx, &mut __conn).await?;
                Ok(::autumn_web::prelude::Json(records))
            }
        } else if has_policy {
            quote! {
                let __policy = __autumn_state
                    .policy::<#model_name>()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg(
                        "missing policy registration"
                    ))?;
                let __ctx = ::autumn_web::authorization::PolicyContext::from_request_parts(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                ).await;
                let __all = repo.find_all().await?;
                let mut __filtered = ::std::vec::Vec::with_capacity(__all.len());
                for __record in __all {
                    if __policy.can_show(&__ctx, &__record).await {
                        __filtered.push(__record);
                    }
                }
                Ok(::autumn_web::prelude::Json(__filtered))
            }
        } else {
            quote! {
                Ok(::autumn_web::prelude::Json(repo.find_all().await?))
            }
        };
        // Inject session + state extractors when *either* a scope
        // or a policy is configured ΓÇö both code paths above need
        // them.
        let list_session_state_args = if config.scope_type.is_some() || has_policy {
            quote! {
                ::autumn_web::reexports::axum::extract::State(__autumn_state):
                    ::autumn_web::reexports::axum::extract::State<::autumn_web::AppState>,
                __autumn_session: ::autumn_web::session::Session,
                __autumn_token_scopes: ::core::option::Option<
                    ::autumn_web::reexports::axum::extract::Extension<
                        ::autumn_web::auth::ApiTokenScopes
                    >
                >,
            }
        } else {
            quote! {}
        };
        let resource_type_name_lit = model_name.to_string();
        let api_path_lit = api_path.clone();

        // Compile-time assertion: when the user writes
        // `policy = SomePolicy`, the generated code references the
        // type so a typo (or a real type that doesn't `impl
        // Policy<Model>`) fails compilation here, not at the first
        // request with `500 missing policy registration`.
        let policy_type_assertion = if let Some(ref policy_type) = config.policy_type {
            quote! {
                const _: fn() = || {
                    fn __autumn_assert_policy<P: ::autumn_web::authorization::Policy<#model_name>>() {}
                    __autumn_assert_policy::<#policy_type>();
                };
            }
        } else {
            quote! {}
        };
        // Emit a type-erased registry probe so the app builder can
        // verify at startup that the policy was actually registered
        // via `.policy::<R, _>(...)`. Without this, forgetting the
        // `.policy(...)` call would compile and boot, then 500 on
        // every protected request.
        let policy_check_fn = if config.policy_type.is_some() {
            quote! {
                ::core::option::Option::Some(
                    (|registry: &::autumn_web::authorization::PolicyRegistry| {
                        registry.has_policy::<#model_name>()
                    }) as fn(&::autumn_web::authorization::PolicyRegistry) -> bool
                )
            }
        } else {
            quote! { ::core::option::Option::None }
        };
        // Companion probe for `scope = ...`. ONLY attached to the
        // `_api_list` route's metadata ΓÇö the other auto-generated
        // routes (`*_api_get` / `*_api_create` / `*_api_update` /
        // `*_api_delete`) never call `scope.list`, so flagging them
        // for missing scope registration would fire the prod fail-
        // fast even when the user intentionally mounted only
        // non-list endpoints with `scope = ...` configured (the
        // app's reads happen via custom queries, but the scope is
        // still declared so `Note::scope(&ctx)` works in hand-
        // written list handlers). The non-list routes below get
        // `scope_check: None` regardless.
        let list_scope_check_fn = if config.scope_type.is_some() {
            quote! {
                ::core::option::Option::Some(
                    (|registry: &::autumn_web::authorization::PolicyRegistry| {
                        registry.scope::<#model_name>().is_some()
                    }) as fn(&::autumn_web::authorization::PolicyRegistry) -> bool
                )
            }
        } else {
            quote! { ::core::option::Option::None }
        };
        let non_list_scope_check_fn = quote! { ::core::option::Option::None };
        let scope_type_assertion = if let Some(ref scope_type) = config.scope_type {
            quote! {
                const _: fn() = || {
                    fn __autumn_assert_scope<S: ::autumn_web::authorization::Scope<#model_name>>() {}
                    __autumn_assert_scope::<#scope_type>();
                };
            }
        } else {
            quote! {}
        };

        let create_return_type = if has_policy {
            quote! {
                ::autumn_web::idempotency::IdempotencyReplayOr<
                    ::autumn_web::AutumnResult<(
                        ::autumn_web::reexports::http::StatusCode,
                        ::autumn_web::prelude::Json<#model_name>
                    )>
                >
            }
        } else {
            quote! {
                ::autumn_web::AutumnResult<(
                    ::autumn_web::reexports::http::StatusCode,
                    ::autumn_web::prelude::Json<#model_name>
                )>
            }
        };
        let create_body = if has_policy {
            quote! {
                let new: #new_name = match ::autumn_web::reexports::serde_json::from_value(
                    __autumn_new_payload.clone(),
                ) {
                    ::core::result::Result::Ok(new) => new,
                    ::core::result::Result::Err(err) => {
                        return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                            ::core::result::Result::Err(
                                ::autumn_web::AutumnError::unprocessable_msg(err.to_string())
                            )
                        );
                    }
                };
                if let ::core::result::Result::Err(err) =
                    ::autumn_web::authorization::__check_policy_create_payload_scoped::<#model_name>(
                        &__autumn_state,
                        &__autumn_session,
                        __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                        &__autumn_new_payload,
                    )
                    .await
                {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                        ::core::result::Result::Err(err)
                    );
                }
                if let ::core::option::Option::Some(response) =
                    ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Replay(response);
                }
                let record = match repo.save(&new).await {
                    ::core::result::Result::Ok(record) => record,
                    ::core::result::Result::Err(err) => {
                        return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                            ::core::result::Result::Err(err)
                        );
                    }
                };
                ::autumn_web::idempotency::IdempotencyReplayOr::Inner(::core::result::Result::Ok((
                    ::autumn_web::reexports::http::StatusCode::CREATED,
                    ::autumn_web::prelude::Json(record)
                )))
            }
        } else {
            quote! {
                #decode_create_payload
                #policy_check_create_pre
                let record = repo.save(&new).await?;
                Ok((::autumn_web::reexports::http::StatusCode::CREATED, ::autumn_web::prelude::Json(record)))
            }
        };
        let update_return_type = if has_policy {
            quote! {
                ::autumn_web::idempotency::IdempotencyReplayOr<
                    ::autumn_web::AutumnResult<::autumn_web::prelude::Json<#model_name>>
                >
            }
        } else {
            quote! {
                ::autumn_web::AutumnResult<::autumn_web::prelude::Json<#model_name>>
            }
        };
        let update_body = if has_policy {
            quote! {
                let __existing = match repo.on_primary().find_by_id(id).await {
                    ::core::result::Result::Ok(::core::option::Option::Some(existing)) => existing,
                    ::core::result::Result::Ok(::core::option::Option::None) => {
                        return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                            ::core::result::Result::Err(::autumn_web::AutumnError::not_found_msg("not found"))
                        );
                    }
                    ::core::result::Result::Err(err) => {
                        return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                            ::core::result::Result::Err(err)
                        );
                    }
                };
                if let ::core::result::Result::Err(err) =
                    ::autumn_web::authorization::__check_policy_scoped::<#model_name>(
                        &__autumn_state,
                        &__autumn_session,
                        __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                        "update",
                        &__existing,
                    )
                    .await
                {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                        ::core::result::Result::Err(err)
                    );
                }
                if let ::core::option::Option::Some(response) =
                    ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Replay(response);
                }
                let record = match repo.update(id, &patch).await {
                    ::core::result::Result::Ok(record) => record,
                    ::core::result::Result::Err(err) => {
                        return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                            ::core::result::Result::Err(err)
                        );
                    }
                };
                ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                    ::core::result::Result::Ok(::autumn_web::prelude::Json(record))
                )
            }
        } else {
            quote! {
                #policy_check_update_pre
                let record = repo.update(id, &patch).await?;
                Ok(::autumn_web::prelude::Json(record))
            }
        };
        let delete_return_type = if has_policy {
            quote! {
                ::autumn_web::idempotency::IdempotencyReplayOr<
                    ::autumn_web::AutumnResult<::autumn_web::reexports::http::StatusCode>
                >
            }
        } else {
            quote! {
                ::autumn_web::AutumnResult<::autumn_web::reexports::http::StatusCode>
            }
        };
        let delete_body = if has_policy {
            quote! {
                let __autumn_replay_response =
                    ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay);
                let __autumn_replay_deleted_record =
                    ::autumn_web::idempotency::__replay_metadata(
                        &__autumn_idempotency_replay,
                        "repository.delete.record",
                    );
                let __existing = match repo.on_primary().find_by_id(id).await {
                    ::core::result::Result::Ok(::core::option::Option::Some(existing)) => existing,
                    ::core::result::Result::Ok(::core::option::Option::None) => {
                        if let ::core::option::Option::Some(bytes) = __autumn_replay_deleted_record {
                            match ::autumn_web::reexports::serde_json::from_slice::<#model_name>(&bytes) {
                                ::core::result::Result::Ok(existing) => existing,
                                ::core::result::Result::Err(err) => {
                                    return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                                        ::core::result::Result::Err(
                                            ::autumn_web::AutumnError::internal_server_error_msg(err.to_string())
                                        )
                                    );
                                }
                            }
                        } else {
                            return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                                ::core::result::Result::Err(::autumn_web::AutumnError::not_found_msg("not found"))
                            );
                        }
                    }
                    ::core::result::Result::Err(err) => {
                        return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                            ::core::result::Result::Err(err)
                        );
                    }
                };
                if let ::core::result::Result::Err(err) =
                    ::autumn_web::authorization::__check_policy_scoped::<#model_name>(
                        &__autumn_state,
                        &__autumn_session,
                        __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                        "delete",
                        &__existing,
                    )
                    .await
                {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                        ::core::result::Result::Err(err)
                    );
                }
                if let ::core::option::Option::Some(response) = __autumn_replay_response {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Replay(response);
                }
                let __autumn_deleted_record_metadata =
                    match ::autumn_web::reexports::serde_json::to_vec(&__existing) {
                        ::core::result::Result::Ok(bytes) => bytes,
                        ::core::result::Result::Err(err) => {
                            return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                                ::core::result::Result::Err(
                                    ::autumn_web::AutumnError::internal_server_error_msg(err.to_string())
                                )
                            );
                        }
                    };
                if let ::core::result::Result::Err(err) = repo.delete_by_id(id).await {
                    return ::autumn_web::idempotency::IdempotencyReplayOr::Inner(
                        ::core::result::Result::Err(err)
                    );
                }
                ::autumn_web::idempotency::IdempotencyReplayOr::InnerWithReplayMetadata(
                    ::core::result::Result::Ok(::autumn_web::reexports::http::StatusCode::NO_CONTENT),
                    ::std::vec![(
                        "repository.delete.record".to_owned(),
                        __autumn_deleted_record_metadata,
                    )],
                )
            }
        } else {
            quote! {
                #policy_check_delete_pre
                repo.delete_by_id(id).await?;
                Ok(::autumn_web::reexports::http::StatusCode::NO_CONTENT)
            }
        };
        let create_handler_expr = if has_policy {
            quote! { ::autumn_web::reexports::axum::routing::post(#create_fn) }
        } else {
            quote! {
                ::autumn_web::reexports::axum::routing::MethodRouter::<
                    ::autumn_web::AppState, ::core::convert::Infallible
                >::layer(
                    ::autumn_web::reexports::axum::routing::post(#create_fn),
                    ::autumn_web::idempotency::IdempotencyReplayLayer,
                )
            }
        };
        let update_handler_expr = if has_policy {
            quote! { ::autumn_web::reexports::axum::routing::put(#update_fn) }
        } else {
            quote! {
                ::autumn_web::reexports::axum::routing::MethodRouter::<
                    ::autumn_web::AppState, ::core::convert::Infallible
                >::layer(
                    ::autumn_web::reexports::axum::routing::put(#update_fn),
                    ::autumn_web::idempotency::IdempotencyReplayLayer,
                )
            }
        };
        let delete_handler_expr = if has_policy {
            quote! { ::autumn_web::reexports::axum::routing::delete(#delete_fn) }
        } else {
            quote! {
                ::autumn_web::reexports::axum::routing::MethodRouter::<
                    ::autumn_web::AppState, ::core::convert::Infallible
                >::layer(
                    ::autumn_web::reexports::axum::routing::delete(#delete_fn),
                    ::autumn_web::idempotency::IdempotencyReplayLayer,
                )
            }
        };

        quote! {
            // ── Auto-generated REST API handlers ─────────────────

            #policy_type_assertion
            #scope_type_assertion

            #vis async fn #list_fn(
                #list_session_state_args
                repo: #pg_name,
            ) -> ::autumn_web::AutumnResult<::autumn_web::prelude::Json<Vec<#model_name>>> {
                #scope_list_body
            }

            #[doc(hidden)]
            #vis fn #list_info() -> ::autumn_web::Route {
                #route_hook_registration
                ::autumn_web::Route {
                    method: ::autumn_web::reexports::http::Method::GET,
                    path: #api_path,
                    handler: ::autumn_web::reexports::axum::routing::MethodRouter::<
                        ::autumn_web::AppState, ::core::convert::Infallible
                    >::layer(
                        ::autumn_web::reexports::axum::routing::get(#list_fn),
                        ::autumn_web::idempotency::IdempotencyReplayLayer,
                    ),
                    name: ::core::stringify!(#list_fn),
                    api_doc: ::autumn_web::openapi::ApiDoc {
                        method: "GET",
                        path: #api_path,
                        operation_id: ::core::stringify!(#list_fn),
                        success_status: 200,
                        response: ::core::option::Option::Some(
                            ::autumn_web::openapi::SchemaEntry {
                                name: "array",
                                kind: ::autumn_web::openapi::SchemaKind::Array(
                                    &::autumn_web::openapi::SchemaEntry {
                                        name: ::core::stringify!(#model_name),
                                        kind: ::autumn_web::openapi::SchemaKind::Ref,
                                    }
                                ),
                            }
                        ),
                        ..::core::default::Default::default()
                    },
                    repository: ::core::option::Option::Some(::autumn_web::RepositoryApiMeta {
                        resource_type_name: #resource_type_name_lit,
                        api_path: #api_path_lit,
                        has_policy: #has_policy,
                        policy_check: #policy_check_fn,
                        scope_check: #list_scope_check_fn,
                    }),
                    idempotency: ::autumn_web::RouteIdempotency::ReplayThroughInner,
                    timeout: ::autumn_web::RouteTimeout::Inherit,
                    api_version: ::core::option::Option::None,
                    sunset_opt_out: false,
                }
            }

            #vis async fn #get_fn(
                #session_state_args
                ::autumn_web::extract::Path(id): ::autumn_web::extract::Path<i64>,
                repo: #pg_name,
            ) -> ::autumn_web::AutumnResult<::autumn_web::prelude::Json<#model_name>> {
                let record = repo.find_by_id(id).await?
                    .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg("not found"))?;
                #policy_check_show
                Ok(::autumn_web::prelude::Json(record))
            }

            #[doc(hidden)]
            #vis fn #get_info() -> ::autumn_web::Route {
                #route_hook_registration
                ::autumn_web::Route {
                    method: ::autumn_web::reexports::http::Method::GET,
                    path: #id_path,
                    handler: ::autumn_web::reexports::axum::routing::MethodRouter::<
                        ::autumn_web::AppState, ::core::convert::Infallible
                    >::layer(
                        ::autumn_web::reexports::axum::routing::get(#get_fn),
                        ::autumn_web::idempotency::IdempotencyReplayLayer,
                    ),
                    name: ::core::stringify!(#get_fn),
                    api_doc: ::autumn_web::openapi::ApiDoc {
                        method: "GET",
                        path: #id_path,
                        operation_id: ::core::stringify!(#get_fn),
                        path_params: &["id"],
                        success_status: 200,
                        response: ::core::option::Option::Some(
                            ::autumn_web::openapi::SchemaEntry {
                                name: ::core::stringify!(#model_name),
                                kind: ::autumn_web::openapi::SchemaKind::Ref,
                            }
                        ),
                        ..::core::default::Default::default()
                    },
                    repository: ::core::option::Option::Some(::autumn_web::RepositoryApiMeta {
                        resource_type_name: #resource_type_name_lit,
                        api_path: #api_path_lit,
                        has_policy: #has_policy,
                        policy_check: #policy_check_fn,
                        scope_check: #non_list_scope_check_fn,
                    }),
                    idempotency: ::autumn_web::RouteIdempotency::ReplayThroughInner,
                    timeout: ::autumn_web::RouteTimeout::Inherit,
                    api_version: ::core::option::Option::None,
                    sunset_opt_out: false,
                }
            }

            #vis async fn #create_fn(
                #session_state_args
                repo: #pg_name,
                #create_payload_arg,
            ) -> #create_return_type {
                #create_body
            }

            #[doc(hidden)]
            #vis fn #create_info() -> ::autumn_web::Route {
                #route_hook_registration
                ::autumn_web::Route {
                    method: ::autumn_web::reexports::http::Method::POST,
                    path: #api_path,
                    handler: #create_handler_expr,
                    name: ::core::stringify!(#create_fn),
                    api_doc: ::autumn_web::openapi::ApiDoc {
                        method: "POST",
                        path: #api_path,
                        operation_id: ::core::stringify!(#create_fn),
                        success_status: 201,
                        request_body: ::core::option::Option::Some(
                            ::autumn_web::openapi::SchemaEntry {
                                name: ::core::stringify!(#new_name),
                                kind: ::autumn_web::openapi::SchemaKind::Ref,
                            }
                        ),
                        response: ::core::option::Option::Some(
                            ::autumn_web::openapi::SchemaEntry {
                                name: ::core::stringify!(#model_name),
                                kind: ::autumn_web::openapi::SchemaKind::Ref,
                            }
                        ),
                        ..::core::default::Default::default()
                    },
                    repository: ::core::option::Option::Some(::autumn_web::RepositoryApiMeta {
                        resource_type_name: #resource_type_name_lit,
                        api_path: #api_path_lit,
                        has_policy: #has_policy,
                        policy_check: #policy_check_fn,
                        scope_check: #non_list_scope_check_fn,
                    }),
                    idempotency: ::autumn_web::RouteIdempotency::ReplayThroughInner,
                    timeout: ::autumn_web::RouteTimeout::Inherit,
                    api_version: ::core::option::Option::None,
                    sunset_opt_out: false,
                }
            }

            #vis async fn #update_fn(
                #session_state_args
                ::autumn_web::extract::Path(id): ::autumn_web::extract::Path<i64>,
                repo: #pg_name,
                ::autumn_web::prelude::Json(patch): ::autumn_web::prelude::Json<#update_name>,
            ) -> #update_return_type {
                #update_body
            }

            #[doc(hidden)]
            #vis fn #update_info() -> ::autumn_web::Route {
                #route_hook_registration
                ::autumn_web::Route {
                    method: ::autumn_web::reexports::http::Method::PUT,
                    path: #id_path,
                    handler: #update_handler_expr,
                    name: ::core::stringify!(#update_fn),
                    api_doc: ::autumn_web::openapi::ApiDoc {
                        method: "PUT",
                        path: #id_path,
                        operation_id: ::core::stringify!(#update_fn),
                        path_params: &["id"],
                        success_status: 200,
                        request_body: ::core::option::Option::Some(
                            ::autumn_web::openapi::SchemaEntry {
                                name: ::core::stringify!(#update_name),
                                kind: ::autumn_web::openapi::SchemaKind::Ref,
                            }
                        ),
                        response: ::core::option::Option::Some(
                            ::autumn_web::openapi::SchemaEntry {
                                name: ::core::stringify!(#model_name),
                                kind: ::autumn_web::openapi::SchemaKind::Ref,
                            }
                        ),
                        ..::core::default::Default::default()
                    },
                    repository: ::core::option::Option::Some(::autumn_web::RepositoryApiMeta {
                        resource_type_name: #resource_type_name_lit,
                        api_path: #api_path_lit,
                        has_policy: #has_policy,
                        policy_check: #policy_check_fn,
                        scope_check: #non_list_scope_check_fn,
                    }),
                    idempotency: ::autumn_web::RouteIdempotency::ReplayThroughInner,
                    timeout: ::autumn_web::RouteTimeout::Inherit,
                    api_version: ::core::option::Option::None,
                    sunset_opt_out: false,
                }
            }

            #vis async fn #delete_fn(
                #session_state_args
                ::autumn_web::extract::Path(id): ::autumn_web::extract::Path<i64>,
                repo: #pg_name,
            ) -> #delete_return_type {
                #delete_body
            }

            #[doc(hidden)]
            #vis fn #delete_info() -> ::autumn_web::Route {
                #route_hook_registration
                ::autumn_web::Route {
                    method: ::autumn_web::reexports::http::Method::DELETE,
                    path: #id_path,
                    handler: #delete_handler_expr,
                    name: ::core::stringify!(#delete_fn),
                    api_doc: ::autumn_web::openapi::ApiDoc {
                        method: "DELETE",
                        path: #id_path,
                        operation_id: ::core::stringify!(#delete_fn),
                        path_params: &["id"],
                        success_status: 204,
                        ..::core::default::Default::default()
                    },
                    repository: ::core::option::Option::Some(::autumn_web::RepositoryApiMeta {
                        resource_type_name: #resource_type_name_lit,
                        api_path: #api_path_lit,
                        has_policy: #has_policy,
                        policy_check: #policy_check_fn,
                        scope_check: #non_list_scope_check_fn,
                    }),
                    idempotency: ::autumn_web::RouteIdempotency::ReplayThroughInner,
                    timeout: ::autumn_web::RouteTimeout::Inherit,
                    api_version: ::core::option::Option::None,
                    sunset_opt_out: false,
                }
            }

            // ── Path helpers for API routes ───────────────────────

            #[doc(hidden)]
            #vis fn #list_path_fn() -> ::std::string::String {
                #api_path.to_owned()
            }

            #[doc(hidden)]
            #vis fn #get_path_fn(id: impl ::std::fmt::Display) -> ::std::string::String {
                format!("{}/{}", #api_path, ::autumn_web::paths::encode_path_segment(id))
            }

            #[doc(hidden)]
            #vis fn #create_path_fn() -> ::std::string::String {
                #api_path.to_owned()
            }

            #[doc(hidden)]
            #vis fn #update_path_fn(id: impl ::std::fmt::Display) -> ::std::string::String {
                format!("{}/{}", #api_path, ::autumn_web::paths::encode_path_segment(id))
            }

            #[doc(hidden)]
            #vis fn #delete_path_fn(id: impl ::std::fmt::Display) -> ::std::string::String {
                format!("{}/{}", #api_path, ::autumn_web::paths::encode_path_segment(id))
            }
        }
    } else {
        quote! {}
    };

    // Soft-delete extra trait/impl methods: restore, purge, with_deleted, only_deleted.
    let soft_delete_trait_methods = if config.soft_delete {
        quote! {
            fn restore(&self, id: i64) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
            fn purge(&self, id: i64) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
            fn with_deleted(&self) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send;
            fn only_deleted(&self) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send;
            fn page_only_deleted(&self, req: &::autumn_web::pagination::PageRequest) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>>> + Send;
        }
    } else {
        quote! {}
    };

    // §1d: cross-shard fan-out / reject fragments for the soft-delete readers.
    // `with_deleted` / `only_deleted` are unpaginated `Vec`-returning reads, so
    // under `across_tenants()` on a sharded repo they fan out across every shard
    // and concatenate the per-shard results (via inherent one-shard helpers, the
    // same pattern as `find_all`). `page_only_deleted` is paginated and a naive
    // fan-out would produce wrong page boundaries / totals, so it rejects (like
    // `page`). All fragments are empty (zero-cost) unless sharded + tenant_scoped.
    let mut soft_delete_one_shard_helpers: Vec<proc_macro2::TokenStream> = Vec::new();
    let (with_deleted_fan_out, only_deleted_fan_out, page_only_deleted_cross_shard_guard) =
        if config.soft_delete && config.sharded && config.tenant_scoped {
            let with_deleted_fan_out = quote! {
                if self.across_tenants {
                    if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                        let __vecs = __shards.fan_out_shards(|__shard| {
                            let __sub = self.__autumn_for_shard(__shard);
                            async move { __sub.__autumn_with_deleted_one_shard().await }
                        }).await?;
                        return ::core::result::Result::Ok(
                            __vecs.into_iter().flatten().collect()
                        );
                    }
                }
            };
            let only_deleted_fan_out = quote! {
                if self.across_tenants {
                    if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                        let __vecs = __shards.fan_out_shards(|__shard| {
                            let __sub = self.__autumn_for_shard(__shard);
                            async move { __sub.__autumn_only_deleted_one_shard().await }
                        }).await?;
                        return ::core::result::Result::Ok(
                            __vecs.into_iter().flatten().collect()
                        );
                    }
                }
            };
            let page_only_deleted_cross_shard_guard = quote! {
                if self.across_tenants && self.__autumn_shards.is_some() {
                    return ::core::result::Result::Err(
                        ::autumn_web::AutumnError::bad_request_msg(
                            "cross-shard page_only_deleted is not supported: \
                             across_tenants() fans out unpaginated reads only; \
                             page a specific shard instead"
                        )
                    );
                }
            };
            // The per-shard helpers re-run the single-shard body. The
            // sub-repo built by `__autumn_for_shard` has across_tenants = true
            // and __autumn_shards = None, so its tenant_id resolves to None and
            // the body loads every (deleted) row on that shard.
            soft_delete_one_shard_helpers.push(quote! {
                #[doc(hidden)]
                async fn __autumn_with_deleted_one_shard(&self) -> ::autumn_web::AutumnResult<::std::vec::Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    #table_ident::table
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)
                }

                #[doc(hidden)]
                async fn __autumn_only_deleted_one_shard(&self) -> ::autumn_web::AutumnResult<::std::vec::Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    #table_ident::table
                        .filter(#table_ident::deleted_at.is_not_null())
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)
                }
            });
            (
                with_deleted_fan_out,
                only_deleted_fan_out,
                page_only_deleted_cross_shard_guard,
            )
        } else {
            (quote! {}, quote! {}, quote! {})
        };

    let soft_delete_impl_methods = if config.soft_delete {
        if config.tenant_scoped {
            let tenant_id_setup = quote! {
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
            };

            quote! {
                async fn restore(&self, id: i64) -> ::autumn_web::AutumnResult<()> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    // §1d: restore is a write; reject cross-shard across_tenants
                    // (per-shard ids are ambiguous, like delete/purge).
                    #cross_shard_write_guard
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let query = #table_ident::table.find(id);
                    let __count = if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::update(query.filter(#table_ident::tenant_id.eq(t)))
                            .set(#table_ident::deleted_at.eq(::core::option::Option::None::<::autumn_web::reexports::chrono::NaiveDateTime>))
                            .execute(&mut conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::update(query)
                            .set(#table_ident::deleted_at.eq(::core::option::Option::None::<::autumn_web::reexports::chrono::NaiveDateTime>))
                            .execute(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }

                async fn purge(&self, id: i64) -> ::autumn_web::AutumnResult<()> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    // §1d: purge is a hard delete; reject cross-shard across_tenants
                    // (per-shard ids are ambiguous and could purge another tenant's row).
                    #cross_shard_write_guard
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let query = #table_ident::table.find(id);
                    let __count = if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::delete(query.filter(#table_ident::tenant_id.eq(t)))
                            .execute(&mut conn)
                            .await
                    } else {
                        ::autumn_web::reexports::diesel::delete(query)
                            .execute(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }

                async fn with_deleted(&self) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    // §1d: fan out across all shards under across_tenants. The
                    // dispatch runs before acquiring a routed connection so no
                    // drop is needed (see the find_all deadlock rule).
                    #with_deleted_fan_out
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let query = #table_ident::table;
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        query.filter(#table_ident::tenant_id.eq(t))
                            .load::<#model_name>(&mut conn)
                            .await
                    } else {
                        query
                            .load::<#model_name>(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)
                }

                async fn only_deleted(&self) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    // §1d: fan out across all shards under across_tenants.
                    #only_deleted_fan_out
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let query = #table_ident::table.filter(#table_ident::deleted_at.is_not_null());
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        query.filter(#table_ident::tenant_id.eq(t))
                            .load::<#model_name>(&mut conn)
                            .await
                    } else {
                        query
                            .load::<#model_name>(&mut conn)
                            .await
                    }
                    .map_err(::autumn_web::AutumnError::from)
                }

                async fn page_only_deleted(
                    &self,
                    req: &::autumn_web::pagination::PageRequest,
                ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    // §1d: paginated reads cannot fan out (page/total would be
                    // wrong across shards); reject under cross-shard access.
                    #page_only_deleted_cross_shard_guard
                    #tenant_id_setup
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let query = #table_ident::table.filter(#table_ident::deleted_at.is_not_null());
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        let total: i64 = query
                            .filter(#table_ident::tenant_id.eq(t))
                            .count()
                            .get_result::<i64>(&mut conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        let items: ::std::vec::Vec<#model_name> = query
                            .filter(#table_ident::tenant_id.eq(t))
                            .order(#table_ident::id.desc())
                            .limit(req.limit())
                            .offset(req.offset())
                            .select(#model_name::as_select())
                            .load::<#model_name>(&mut conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        ::core::result::Result::Ok(::autumn_web::pagination::Page::new(items, total, req))
                    } else {
                        let total: i64 = query
                            .count()
                            .get_result::<i64>(&mut conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        let items: ::std::vec::Vec<#model_name> = query
                            .order(#table_ident::id.desc())
                            .limit(req.limit())
                            .offset(req.offset())
                            .select(#model_name::as_select())
                            .load::<#model_name>(&mut conn)
                            .await
                            .map_err(::autumn_web::AutumnError::from)?;
                        ::core::result::Result::Ok(::autumn_web::pagination::Page::new(items, total, req))
                    }
                }
            }
        } else {
            quote! {
                async fn restore(&self, id: i64) -> ::autumn_web::AutumnResult<()> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let query = #table_ident::table.find(id);
                    let __count = ::autumn_web::reexports::diesel::update(query)
                        .set(#table_ident::deleted_at.eq(::core::option::Option::None::<::autumn_web::reexports::chrono::NaiveDateTime>))
                        .execute(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }

                async fn purge(&self, id: i64) -> ::autumn_web::AutumnResult<()> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_conn().await?;
                    let query = #table_ident::table.find(id);
                    let __count = ::autumn_web::reexports::diesel::delete(query)
                        .execute(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    if __count == 0 {
                        return Err(::autumn_web::AutumnError::not_found_msg(
                            format!("{} with id {} not found", stringify!(#model_name), id)
                        ));
                    }
                    Ok(())
                }

                async fn with_deleted(&self) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let query = #table_ident::table;
                    query
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)
                }

                async fn only_deleted(&self) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let query = #table_ident::table.filter(#table_ident::deleted_at.is_not_null());
                    query
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)
                }

                async fn page_only_deleted(
                    &self,
                    req: &::autumn_web::pagination::PageRequest,
                ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    let mut conn = self.__autumn_acquire_read_conn().await?;
                    let query = #table_ident::table.filter(#table_ident::deleted_at.is_not_null());
                    let total: i64 = query
                        .count()
                        .get_result::<i64>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    let items: ::std::vec::Vec<#model_name> = query
                        .order(#table_ident::id.desc())
                        .limit(req.limit())
                        .offset(req.offset())
                        .select(#model_name::as_select())
                        .load::<#model_name>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                    ::core::result::Result::Ok(::autumn_web::pagination::Page::new(items, total, req))
                }
            }
        }
    } else {
        quote! {}
    };

    let find_by_id_impl = if config.tenant_scoped {
        quote! {
            let tenant_id = if self.across_tenants {
                ::core::option::Option::None
            } else {
                let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                ::core::option::Option::Some(t)
            };
            let query = #table_ident::table.find(id);
            if let ::core::option::Option::Some(ref t) = tenant_id {
                query.filter(#table_ident::tenant_id.eq(t))
                    #sd_filter
                    .first::<#model_name>(&mut conn)
                    .await
                    .optional()
                    .map_err(::autumn_web::AutumnError::from)
            } else {
                query
                    #sd_filter
                    .first::<#model_name>(&mut conn)
                    .await
                    .optional()
                    .map_err(::autumn_web::AutumnError::from)
            }
        }
    } else {
        quote! {
            #table_ident::table
                .find(id)
                #sd_filter
                .first::<#model_name>(&mut conn)
                .await
                .optional()
                .map_err(::autumn_web::AutumnError::from)
        }
    };

    let find_all_impl = if config.tenant_scoped {
        quote! {
            let tenant_id = if self.across_tenants {
                ::core::option::Option::None
            } else {
                let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                ::core::option::Option::Some(t)
            };
            let query = #table_ident::table;
            if let ::core::option::Option::Some(ref t) = tenant_id {
                query.filter(#table_ident::tenant_id.eq(t))
                    #sd_filter
                    .load::<#model_name>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            } else {
                query
                    #sd_filter
                    .load::<#model_name>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            }
        }
    } else {
        quote! {
            #table_ident::table
                #sd_filter
                .load::<#model_name>(&mut conn)
                .await
                .map_err(::autumn_web::AutumnError::from)
        }
    };

    // §1d: when sharded + tenant_scoped, fan out across all shards concurrently
    // for across_tenants reads.  Uses ShardSet::fan_out_shards for concurrent
    // execution.  The sub-repo is built with `__autumn_for_shard`, which honors
    // each shard's read routing and the parent request context and sets
    // `__autumn_shards = None`, preventing recursion.
    // §1d: fan out `find_all` across all shards for `across_tenants()`. The
    // per-shard work runs through the inherent `__autumn_find_all_one_shard`
    // helper (emitted below) rather than the trait method, so `find_all`'s
    // RPITIT future never transitively names its own opaque type — which would
    // make its `Send` auto-trait unprovable once hooks/versioning add captured
    // state to the future.
    let (find_all_impl, find_all_one_shard_helper) = if config.sharded && config.tenant_scoped {
        let base = find_all_impl;
        let dispatch = quote! {
            // Dispatch the cross-shard fan-out BEFORE acquiring any connection:
            // no routed-shard connection is held here, so a dead parent replica
            // or exhausted parent pool can't fail the fan-out. Each shard's own
            // read route/fallback is chosen by `__autumn_for_shard` (#1d).
            if self.across_tenants {
                if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                    let __vecs = __shards.fan_out_shards(|__shard| {
                        let __sub = self.__autumn_for_shard(__shard);
                        async move { __sub.__autumn_find_all_one_shard().await }
                    }).await?;
                    return ::core::result::Result::Ok(
                        __vecs.into_iter().flatten().collect()
                    );
                }
            }
            let mut conn = self.__autumn_acquire_read_conn().await?;
            #base
        };
        let helper = quote! {
            #[doc(hidden)]
            async fn __autumn_find_all_one_shard(&self) -> ::autumn_web::AutumnResult<::std::vec::Vec<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_read_conn().await?;
                #base
            }
        };
        (dispatch, helper)
    } else {
        (find_all_impl, quote! {})
    };

    // §1d: fan out `find_by_id` across all shards for `across_tenants()`,
    // returning the first shard that has the row (ids are unique within a
    // shard). Without this, a cross-tenant `find_by_id` would only consult the
    // originally-routed shard and report rows on other shards as missing.
    let (find_by_id_impl, find_by_id_one_shard_helper) = if config.sharded && config.tenant_scoped {
        let base = find_by_id_impl;
        let dispatch = quote! {
            // Fan out before acquiring a parent-shard connection (see find_all).
            if self.across_tenants {
                if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                    let __found = __shards.fan_out_shards(|__shard| {
                        let __sub = self.__autumn_for_shard(__shard);
                        async move { __sub.__autumn_find_by_id_one_shard(id).await }
                    }).await?;
                    // Ids are unique only within a shard: with per-shard
                    // sequences two shards can hold the same id, so a match on
                    // more than one shard is ambiguous. Reject rather than
                    // silently returning whichever shard sorts first (#1d).
                    let mut __hits = __found.into_iter().flatten();
                    let __first = __hits.next();
                    if __hits.next().is_some() {
                        return ::core::result::Result::Err(
                            ::autumn_web::AutumnError::internal_server_error_msg(
                                "ambiguous cross-shard find_by_id: id matched rows on \
                                 multiple shards; query a specific shard instead"
                            )
                        );
                    }
                    return ::core::result::Result::Ok(__first);
                }
            }
            let mut conn = self.__autumn_acquire_read_conn().await?;
            #base
        };
        let helper = quote! {
            #[doc(hidden)]
            async fn __autumn_find_by_id_one_shard(&self, id: i64) -> ::autumn_web::AutumnResult<::core::option::Option<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_read_conn().await?;
                #base
            }
        };
        (dispatch, helper)
    } else {
        (find_by_id_impl, quote! {})
    };

    let count_impl = if config.tenant_scoped {
        quote! {
            let tenant_id = if self.across_tenants {
                ::core::option::Option::None
            } else {
                let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                ::core::option::Option::Some(t)
            };
            let query = #table_ident::table;
            if let ::core::option::Option::Some(ref t) = tenant_id {
                query.filter(#table_ident::tenant_id.eq(t))
                    #sd_filter
                    .count()
                    .get_result::<i64>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            } else {
                query
                    #sd_filter
                    .count()
                    .get_result::<i64>(&mut conn)
                    .await
                    .map_err(::autumn_web::AutumnError::from)
            }
        }
    } else {
        quote! {
            #table_ident::table
                #sd_filter
                .count()
                .get_result::<i64>(&mut conn)
                .await
                .map_err(::autumn_web::AutumnError::from)
        }
    };

    let (count_impl, count_one_shard_helper) = if config.sharded && config.tenant_scoped {
        let base = count_impl;
        let dispatch = quote! {
            // Fan out before acquiring a parent-shard connection (see find_all).
            if self.across_tenants {
                if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                    let __counts = __shards.fan_out_shards(|__shard| {
                        let __sub = self.__autumn_for_shard(__shard);
                        async move { __sub.__autumn_count_one_shard().await }
                    }).await?;
                    return ::core::result::Result::Ok(__counts.into_iter().sum());
                }
            }
            let mut conn = self.__autumn_acquire_read_conn().await?;
            #base
        };
        let helper = quote! {
            #[doc(hidden)]
            async fn __autumn_count_one_shard(&self) -> ::autumn_web::AutumnResult<i64> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_read_conn().await?;
                #base
            }
        };
        (dispatch, helper)
    } else {
        (count_impl, quote! {})
    };

    let exists_by_id_impl = if config.tenant_scoped {
        quote! {
            let tenant_id = if self.across_tenants {
                ::core::option::Option::None
            } else {
                let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                ::core::option::Option::Some(t)
            };
            let query = #table_ident::table.find(id);
            if let ::core::option::Option::Some(ref t) = tenant_id {
                ::autumn_web::reexports::diesel::select(
                    ::autumn_web::reexports::diesel::dsl::exists(
                        query.filter(#table_ident::tenant_id.eq(t)) #sd_filter
                    )
                )
                .get_result::<bool>(&mut conn)
                .await
                .map_err(::autumn_web::AutumnError::from)
            } else {
                ::autumn_web::reexports::diesel::select(
                    ::autumn_web::reexports::diesel::dsl::exists(
                        query #sd_filter
                    )
                )
                .get_result::<bool>(&mut conn)
                .await
                .map_err(::autumn_web::AutumnError::from)
            }
        }
    } else {
        quote! {
            ::autumn_web::reexports::diesel::select(
                ::autumn_web::reexports::diesel::dsl::exists(
                    #table_ident::table.find(id) #sd_filter
                )
            )
            .get_result::<bool>(&mut conn)
            .await
            .map_err(::autumn_web::AutumnError::from)
        }
    };

    let (exists_by_id_impl, exists_by_id_one_shard_helper) = if config.sharded
        && config.tenant_scoped
    {
        let base = exists_by_id_impl;
        let dispatch = quote! {
            // Fan out before acquiring a parent-shard connection (see find_all).
            if self.across_tenants {
                if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                    let __results = __shards.fan_out_shards(|__shard| {
                        let __sub = self.__autumn_for_shard(__shard);
                        async move { __sub.__autumn_exists_by_id_one_shard(id).await }
                    }).await?;
                    return ::core::result::Result::Ok(__results.into_iter().any(|b| b));
                }
            }
            let mut conn = self.__autumn_acquire_read_conn().await?;
            #base
        };
        let helper = quote! {
            #[doc(hidden)]
            async fn __autumn_exists_by_id_one_shard(&self, id: i64) -> ::autumn_web::AutumnResult<bool> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                let mut conn = self.__autumn_acquire_read_conn().await?;
                #base
            }
        };
        (dispatch, helper)
    } else {
        (exists_by_id_impl, quote! {})
    };

    let upsert_many_trait_method = if config.hooks_type.is_none() && !config.no_upsert_trait {
        quote! {
            fn upsert_many(&self, records: &[#model_name]) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send
            where
                #model_name: ::autumn_web::reexports::diesel::Insertable<#table_ident::table>;
        }
    } else {
        quote! {}
    };

    let bulk_trait_methods = quote! {
        fn save_many(&self, new: &[#new_name]) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send;
        fn save_many_skip_invalid(&self, new: &[#new_name]) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<(Vec<#model_name>, Vec<(usize, ::autumn_web::AutumnError)>)>> + Send;
        fn update_many(&self, ids: &[i64], changes: &#update_name) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send;
        fn delete_many(&self, ids: &[i64]) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
        #upsert_many_trait_method
    };

    let upsert_many_impl_method = if config.hooks_type.is_none() && !config.no_upsert_trait {
        quote! {
            async fn upsert_many(&self, records: &[#model_name]) -> ::autumn_web::AutumnResult<Vec<#model_name>>
            where
                #model_name: ::autumn_web::reexports::diesel::Insertable<#table_ident::table>
            {
                #cross_shard_write_guard
                #upsert_many_body
            }
        }
    } else {
        quote! {}
    };

    let config_soft_delete = config.soft_delete;
    let config_tenant_scoped = config.tenant_scoped;

    let second_stage_soft_delete_filter = if config_soft_delete {
        quote! {
            records_query = records_query.filter(#table_ident::deleted_at.is_null());
        }
    } else {
        quote! {}
    };

    let second_stage_tenant_filter = if config_tenant_scoped {
        quote! {
            if let ::core::option::Option::Some(ref t) = tenant_id {
                records_query = records_query.filter(#table_ident::tenant_id.eq(t.clone()));
            }
        }
    } else {
        quote! {}
    };

    let search_trait_methods = if config.searchable {
        quote! {
            fn search(&self, query: &str) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send;
            fn search_page(
                &self,
                query: &str,
                req: &::autumn_web::pagination::PageRequest,
            ) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>>> + Send;
        }
    } else {
        quote! {}
    };

    let search_compile_check = if config.searchable {
        quote! {
            const _: () = {
                fn assert_searchable<T: ::autumn_web::repository::AutumnSearchableModel>() {}
                let _ = assert_searchable::<#model_name>;
                if !<#model_name as ::autumn_web::repository::AutumnSearchableModel>::IS_SEARCHABLE {
                    ::core::panic!("The backing model is not marked with #[searchable] or has no searchable fields configured, but its repository has `searchable = true` enabled.");
                }
                if <#model_name as ::autumn_web::repository::AutumnSearchableModel>::SEARCH_FIELDS.is_empty() {
                    ::core::panic!("The backing model is marked with #[searchable] but has zero searchable fields configured, but its repository has `searchable = true` enabled.");
                }
            };
        }
    } else {
        quote! {}
    };

    let internal_hooks_defn = if config.generated_internal_hooks {
        let hooks_ident = config.hooks_type.as_ref().unwrap();
        quote! {
            #[derive(Default, Clone)]
            pub struct #hooks_ident;

            impl ::autumn_web::hooks::MutationHooks for #hooks_ident {
                type Model = #model_name;
                type NewModel = #new_name;
                type UpdateModel = #update_name;
            }
        }
    } else {
        quote! {}
    };

    let (search_impl_methods, search_one_shard_helpers) = if config.searchable {
        let tenant_id_setup = if config.tenant_scoped {
            quote! {
                let tenant_id = if self.across_tenants {
                    ::core::option::Option::None
                } else {
                    let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                        .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                    ::core::option::Option::Some(t)
                };
            }
        } else {
            quote! {
                let tenant_id = ::core::option::Option::None::<::std::string::String>;
            }
        };

        // §1d: under across_tenants on a sharded repo the `Vec`-returning
        // `search` fans out across every shard and concatenates results (ranking
        // is per-shard; the merged order is the per-shard ranking concatenated).
        // The paginated `search_page` rejects, since merging ranked pages across
        // shards would produce wrong page boundaries / totals.
        let search_fan_out = if config.sharded && config.tenant_scoped {
            quote! {
                if self.across_tenants {
                    if let ::core::option::Option::Some(ref __shards) = self.__autumn_shards {
                        let __q = ::std::string::ToString::to_string(query);
                        let __vecs = __shards.fan_out_shards(|__shard| {
                            let __sub = self.__autumn_for_shard(__shard);
                            let __q = ::core::clone::Clone::clone(&__q);
                            async move { __sub.__autumn_search_one_shard(&__q).await }
                        }).await?;
                        return ::core::result::Result::Ok(
                            __vecs.into_iter().flatten().collect()
                        );
                    }
                }
            }
        } else {
            quote! {}
        };
        let search_page_cross_shard_guard = if config.sharded && config.tenant_scoped {
            quote! {
                if self.across_tenants && self.__autumn_shards.is_some() {
                    return ::core::result::Result::Err(
                        ::autumn_web::AutumnError::bad_request_msg(
                            "cross-shard search_page is not supported: \
                             across_tenants() fans out unpaginated reads only; \
                             page a specific shard instead"
                        )
                    );
                }
            }
        } else {
            quote! {}
        };

        // The single-shard `search` body, shared by the trait method (as the
        // fan-out fallthrough) and the inherent `__autumn_search_one_shard`
        // helper, so the SQL is defined exactly once.
        let search_body = quote! {
                if query.trim().is_empty() {
                    return Ok(Vec::new());
                }

                #[derive(::autumn_web::reexports::diesel::QueryableByName)]
                struct SearchId {
                    #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::BigInt)]
                    id: i64,
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_read_conn().await?;
                let language = <#model_name as ::autumn_web::repository::AutumnSearchableModel>::SEARCH_LANGUAGE;

                let mut sql = format!(
                    "SELECT id FROM \"{}\" WHERE search_vector @@ websearch_to_tsquery($1::regconfig, $2)",
                    #table_name
                );
                if #config_soft_delete {
                    sql.push_str(" AND deleted_at IS NULL");
                }
                if let ::core::option::Option::Some(ref _t) = tenant_id {
                    sql.push_str(" AND tenant_id = $3");
                }
                sql.push_str(" ORDER BY ts_rank_cd(search_vector, websearch_to_tsquery($1::regconfig, $2)) DESC, id DESC");

                let ids = if #config_tenant_scoped {
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::sql_query(sql)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(t)
                            .load::<SearchId>(&mut conn)
                            .await?
                    } else {
                        ::autumn_web::reexports::diesel::sql_query(sql)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                            .load::<SearchId>(&mut conn)
                            .await?
                    }
                } else {
                    ::autumn_web::reexports::diesel::sql_query(sql)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                        .load::<SearchId>(&mut conn)
                        .await?
                };

                let id_list: Vec<i64> = ids.into_iter().map(|s| s.id).collect();
                if id_list.is_empty() {
                    return Ok(Vec::new());
                }

                let mut records_query = #table_ident::table
                    .filter(#table_ident::id.eq_any(&id_list))
                    .into_boxed();
                #second_stage_soft_delete_filter
                #second_stage_tenant_filter
                let records = records_query
                    .load::<#model_name>(&mut conn)
                    .await?;

                let mut record_map: ::std::collections::HashMap<i64, #model_name> = records
                    .into_iter()
                    .map(|r| (r.id, r))
                    .collect();

                let sorted_records: Vec<#model_name> = id_list
                    .iter()
                    .filter_map(|id| record_map.remove(id))
                    .collect();

                Ok(sorted_records)
        };

        // The inherent per-shard helper for `search`, emitted into the inherent
        // impl block (a trait impl cannot hold non-trait methods).
        let search_one_shard_helpers = if config.sharded && config.tenant_scoped {
            quote! {
                #[doc(hidden)]
                async fn __autumn_search_one_shard(&self, query: &str) -> ::autumn_web::AutumnResult<::std::vec::Vec<#model_name>> {
                    use ::autumn_web::reexports::diesel::prelude::*;
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                    #search_body
                }
            }
        } else {
            quote! {}
        };

        let search_methods = quote! {
            async fn search(&self, query: &str) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                // §1d: fan out across all shards under across_tenants.
                #search_fan_out
                #search_body
            }

            async fn search_page(
                &self,
                query: &str,
                req: &::autumn_web::pagination::PageRequest,
            ) -> ::autumn_web::AutumnResult<::autumn_web::pagination::Page<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                // §1d: paginated search cannot fan out; reject cross-shard.
                #search_page_cross_shard_guard

                if query.trim().is_empty() {
                    return Ok(::autumn_web::pagination::Page::new(Vec::new(), 0, req));
                }

                #[derive(::autumn_web::reexports::diesel::QueryableByName)]
                struct SearchId {
                    #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::BigInt)]
                    id: i64,
                }

                #[derive(::autumn_web::reexports::diesel::QueryableByName)]
                struct SearchCount {
                    #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::BigInt)]
                    count: i64,
                }

                #tenant_id_setup
                let mut conn = self.__autumn_acquire_read_conn().await?;
                let language = <#model_name as ::autumn_web::repository::AutumnSearchableModel>::SEARCH_LANGUAGE;
                let limit = req.limit();
                let offset = req.offset();

                let mut count_sql = format!(
                    "SELECT COUNT(*) AS count FROM \"{}\" WHERE search_vector @@ websearch_to_tsquery($1::regconfig, $2)",
                    #table_name
                );
                if #config_soft_delete {
                    count_sql.push_str(" AND deleted_at IS NULL");
                }
                if let ::core::option::Option::Some(ref _t) = tenant_id {
                    count_sql.push_str(" AND tenant_id = $3");
                }

                let total = if #config_tenant_scoped {
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::sql_query(count_sql)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(t)
                            .get_result::<SearchCount>(&mut conn)
                            .await?
                            .count
                    } else {
                        ::autumn_web::reexports::diesel::sql_query(count_sql)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                            .get_result::<SearchCount>(&mut conn)
                            .await?
                            .count
                    }
                } else {
                    ::autumn_web::reexports::diesel::sql_query(count_sql)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                        .get_result::<SearchCount>(&mut conn)
                        .await?
                        .count
                };

                let mut select_sql = format!(
                    "SELECT id FROM \"{}\" WHERE search_vector @@ websearch_to_tsquery($1::regconfig, $2)",
                    #table_name
                );
                if #config_soft_delete {
                    select_sql.push_str(" AND deleted_at IS NULL");
                }
                if let ::core::option::Option::Some(ref _t) = tenant_id {
                    select_sql.push_str(" AND tenant_id = $3");
                    select_sql.push_str(" ORDER BY ts_rank_cd(search_vector, websearch_to_tsquery($1::regconfig, $2)) DESC, id DESC");
                    select_sql.push_str(" LIMIT $4 OFFSET $5");
                } else {
                    select_sql.push_str(" ORDER BY ts_rank_cd(search_vector, websearch_to_tsquery($1::regconfig, $2)) DESC, id DESC");
                    select_sql.push_str(" LIMIT $3 OFFSET $4");
                }

                let ids = if #config_tenant_scoped {
                    if let ::core::option::Option::Some(ref t) = tenant_id {
                        ::autumn_web::reexports::diesel::sql_query(select_sql)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(t)
                            .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(limit)
                            .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(offset)
                            .load::<SearchId>(&mut conn)
                            .await?
                    } else {
                        ::autumn_web::reexports::diesel::sql_query(select_sql)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                            .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                            .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(limit)
                            .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(offset)
                            .load::<SearchId>(&mut conn)
                            .await?
                    }
                } else {
                    ::autumn_web::reexports::diesel::sql_query(select_sql)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(language)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(query)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(limit)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(offset)
                        .load::<SearchId>(&mut conn)
                        .await?
                };

                let id_list: Vec<i64> = ids.into_iter().map(|s| s.id).collect();
                if id_list.is_empty() {
                    return Ok(::autumn_web::pagination::Page::new(Vec::new(), total, req));
                }

                let mut records_query = #table_ident::table
                    .filter(#table_ident::id.eq_any(&id_list))
                    .into_boxed();
                #second_stage_soft_delete_filter
                #second_stage_tenant_filter
                let records = records_query
                    .load::<#model_name>(&mut conn)
                    .await?;

                let mut record_map: ::std::collections::HashMap<i64, #model_name> = records
                    .into_iter()
                    .map(|r| (r.id, r))
                    .collect();

                let sorted_records: Vec<#model_name> = id_list
                    .iter()
                    .filter_map(|id| record_map.remove(id))
                    .collect();

                Ok(::autumn_web::pagination::Page::new(sorted_records, total, req))
            }
        };
        (search_methods, search_one_shard_helpers)
    } else {
        (quote! {}, quote! {})
    };

    let upsert_set_ext_impl = quote! {};
    let upsert_execution_ext_impl = quote! {};
    let correlate_ext_impl = quote! {};

    // ── VersionedRecord impl (issue #700) ────────────────────────
    // When versioned = true, generate `impl VersionedRecord for Model` so
    // that the write paths (which call <Model as VersionedRecord>::…) compile.
    let versioned_record_impl = if config.versioned && !config.no_versioned_record_impl {
        let sensitive_cols = match parse_version_history_sensitive(&trait_def.attrs) {
            Ok(cols) => cols,
            Err(err) => return err.to_compile_error(),
        };
        let sensitive_ts = if sensitive_cols.is_empty() {
            quote! { &[] }
        } else {
            let col_lits: Vec<_> = sensitive_cols.iter().map(|c| quote! { #c }).collect();
            quote! { &[#(#col_lits),*] }
        };
        let tenant_id_method = if config.tenant_scoped {
            quote! {
                fn version_tenant_id(&self) -> ::core::option::Option<&str> {
                    ::autumn_web::version_history::VersionTenantIdValue::version_tenant_id(&self.tenant_id)
                }
            }
        } else {
            quote! {}
        };
        quote! {
            impl ::autumn_web::version_history::VersionedRecord for #model_name {
                fn version_table_name() -> &'static str {
                    #table_name
                }
                fn version_record_id(&self) -> i64 {
                    self.id
                }
                fn version_column_values(&self) -> ::autumn_web::reexports::serde_json::Value {
                    let mut __vh_value = ::autumn_web::reexports::serde_json::to_value(self)
                        .unwrap_or(::autumn_web::reexports::serde_json::Value::Object(Default::default()));
                    // Columns opted into `versioned_ciphertext` (#805) store
                    // ciphertext here; columns left as the default are excluded
                    // from this snapshot via `version_sensitive_columns()`.
                    ::autumn_web::encryption::encrypt_versioned_columns_in_value(
                        #table_name,
                        &mut __vh_value,
                    );
                    __vh_value
                }
                fn version_sensitive_columns() -> &'static [&'static str] {
                    // Merge declared sensitive columns with at-rest encrypted
                    // columns (#805) so version history never records plaintext.
                    static __VH_SENSITIVE: ::std::sync::OnceLock<::std::vec::Vec<&'static str>> =
                        ::std::sync::OnceLock::new();
                    __VH_SENSITIVE
                        .get_or_init(|| {
                            let declared: &[&'static str] = #sensitive_ts;
                            let mut cols: ::std::vec::Vec<&'static str> = declared.to_vec();
                            ::autumn_web::encryption::merge_encrypted_columns_for_table(
                                #table_name,
                                &mut cols,
                            );
                            cols
                        })
                        .as_slice()
                }
                #tenant_id_method
            }
        }
    } else {
        quote! {}
    };

    // ── Versioned history (issue #700) ────────────────────────────
    // When versioned = true, generate a `version_history` associated
    // function on the repository struct that queries _autumn_version_history.
    let version_history_tenant_setup = if config.tenant_scoped {
        quote! {
            let __version_history_tenant_id = if self.across_tenants {
                ::core::option::Option::None
            } else {
                let t = ::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone()).ok().flatten()
                    .ok_or_else(|| ::autumn_web::AutumnError::internal_server_error_msg("Query scoped to tenant, but no tenant context was established"))?;
                ::core::option::Option::Some(t)
            };
            let __version_history_tenant_id = __version_history_tenant_id.as_deref();
        }
    } else {
        quote! {
            let __version_history_tenant_id: ::core::option::Option<&str> = ::core::option::Option::None;
        }
    };

    // §1d: cross-shard reject for version_history (keyed by a single, per-shard
    // -ambiguous record_id and paginated). Empty unless sharded + tenant_scoped.
    let version_history_cross_shard_guard = if config.sharded && config.tenant_scoped {
        quote! {
            if self.across_tenants && self.__autumn_shards.is_some() {
                return ::core::result::Result::Err(
                    ::autumn_web::AutumnError::bad_request_msg(
                        "cross-shard version_history is not supported: \
                         record ids are unique only within a shard; \
                         query a specific shard instead"
                    )
                );
            }
        }
    } else {
        quote! {}
    };

    let versioned_history_impl = if config.versioned {
        quote! {
            impl #pg_name {
                /// Retrieve paginated version history for a record.
                ///
                /// Entries are returned in chronological order (oldest first).
                /// Use [`::autumn_web::VersionFilter`] to restrict by time range
                /// or to paginate.
                ///
                /// This method is generated automatically when the repository
                /// is declared with `versioned = true`.
                pub async fn version_history(
                    &self,
                    record_id: i64,
                    filter: ::autumn_web::version_history::VersionFilter,
                ) -> ::autumn_web::AutumnResult<::autumn_web::version_history::VersionPage> {
                    use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;

                    // §1d: version_history is keyed by a single `record_id` and
                    // is paginated; per-shard ids are ambiguous (per-shard
                    // sequences can mint the same id on multiple shards) and a
                    // naive page merge would be wrong. Reject under cross-shard
                    // access rather than silently consulting one shard.
                    #version_history_cross_shard_guard

                    // Private helper struct that can be deserialized from a raw SQL row.
                    #[derive(::autumn_web::reexports::diesel::QueryableByName)]
                    struct __AutumnVersionHistoryRow {
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::BigInt)]
                        id: i64,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::Text)]
                        table_name: ::std::string::String,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::BigInt)]
                        record_id: i64,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::Text)]
                        op: ::std::string::String,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::Text)]
                        actor: ::std::string::String,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>)]
                        request_id: ::core::option::Option<::std::string::String>,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::Text)]
                        changes: ::std::string::String,
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::Timestamptz)]
                        recorded_at: ::autumn_web::reexports::chrono::DateTime<::autumn_web::reexports::chrono::Utc>,
                    }

                    #[derive(::autumn_web::reexports::diesel::QueryableByName)]
                    struct __AutumnVersionHistoryCount {
                        #[diesel(sql_type = ::autumn_web::reexports::diesel::sql_types::BigInt)]
                        count: i64,
                    }

                    let __table_name: &str = #table_name;
                    let (limit, offset) = filter.limit_offset();
                    let page = filter.page();
                    let per_page = filter.per_page();
                    #version_history_tenant_setup

                    let mut conn = self.__autumn_acquire_read_conn().await?;

                    // Execute count query, optionally filtered by timestamp range.
                    let total: u64 = if filter.from.is_some() || filter.to.is_some() {
                        let rows = ::autumn_web::reexports::diesel::sql_query(
                            "SELECT COUNT(*)::bigint AS count \
                             FROM _autumn_version_history \
                             WHERE table_name = $1 AND record_id = $2 \
                             AND ($3::text IS NULL OR tenant_id = $3) \
                             AND ($4::timestamptz IS NULL OR recorded_at >= $4) \
                             AND ($5::timestamptz IS NULL OR recorded_at <= $5)"
                        )
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(__table_name)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(record_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>, _>(__version_history_tenant_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Timestamptz>, _>(filter.from)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Timestamptz>, _>(filter.to)
                        .get_results::<__AutumnVersionHistoryCount>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                        rows.into_iter().next().map(|r| r.count).unwrap_or(0).max(0) as u64
                    } else {
                        let rows = ::autumn_web::reexports::diesel::sql_query(
                            "SELECT COUNT(*)::bigint AS count \
                             FROM _autumn_version_history \
                             WHERE table_name = $1 AND record_id = $2 \
                             AND ($3::text IS NULL OR tenant_id = $3)"
                        )
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(__table_name)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(record_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>, _>(__version_history_tenant_id)
                        .get_results::<__AutumnVersionHistoryCount>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?;
                        rows.into_iter().next().map(|r| r.count).unwrap_or(0).max(0) as u64
                    };

                    // Execute the entries query.
                    let raw_rows: Vec<__AutumnVersionHistoryRow> = if filter.from.is_some() || filter.to.is_some() {
                        ::autumn_web::reexports::diesel::sql_query(
                            "SELECT id, table_name, record_id, op, actor, request_id, \
                             changes::text AS changes, recorded_at \
                             FROM _autumn_version_history \
                             WHERE table_name = $1 AND record_id = $2 \
                             AND ($3::text IS NULL OR tenant_id = $3) \
                             AND ($4::timestamptz IS NULL OR recorded_at >= $4) \
                             AND ($5::timestamptz IS NULL OR recorded_at <= $5) \
                             ORDER BY recorded_at ASC, id ASC \
                             LIMIT $6 OFFSET $7"
                        )
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(__table_name)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(record_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>, _>(__version_history_tenant_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Timestamptz>, _>(filter.from)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Timestamptz>, _>(filter.to)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(limit)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(offset)
                        .get_results::<__AutumnVersionHistoryRow>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?
                    } else {
                        ::autumn_web::reexports::diesel::sql_query(
                            "SELECT id, table_name, record_id, op, actor, request_id, \
                             changes::text AS changes, recorded_at \
                             FROM _autumn_version_history \
                             WHERE table_name = $1 AND record_id = $2 \
                             AND ($3::text IS NULL OR tenant_id = $3) \
                             ORDER BY recorded_at ASC, id ASC \
                             LIMIT $4 OFFSET $5"
                        )
                        .bind::<::autumn_web::reexports::diesel::sql_types::Text, _>(__table_name)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(record_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::Nullable<::autumn_web::reexports::diesel::sql_types::Text>, _>(__version_history_tenant_id)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(limit)
                        .bind::<::autumn_web::reexports::diesel::sql_types::BigInt, _>(offset)
                        .get_results::<__AutumnVersionHistoryRow>(&mut conn)
                        .await
                        .map_err(::autumn_web::AutumnError::from)?
                    };

                    // Map raw rows to VersionEntry.
                    let entries: Vec<::autumn_web::version_history::VersionEntry> = raw_rows
                        .into_iter()
                        .map(|row| {
                            let op = match row.op.as_str() {
                                "insert" => ::autumn_web::version_history::VersionOp::Insert,
                                "delete" => ::autumn_web::version_history::VersionOp::Delete,
                                _ => ::autumn_web::version_history::VersionOp::Update,
                            };
                            let changes: Vec<::autumn_web::version_history::ColumnChange> =
                                ::autumn_web::reexports::serde_json::from_str(&row.changes)
                                    .unwrap_or_default();
                            ::autumn_web::version_history::VersionEntry {
                                id: row.id,
                                table_name: row.table_name,
                                record_id: row.record_id,
                                op,
                                actor: row.actor,
                                request_id: row.request_id,
                                changes,
                                recorded_at: row.recorded_at,
                            }
                        })
                        .collect();

                    Ok(::autumn_web::version_history::VersionPage {
                        entries,
                        total,
                        page,
                        per_page,
                    })
                }
            }
        }
    } else {
        quote! {}
    };

    // Generate the trait, impl, and extractor.
    //
    // Key design decisions:
    // - Native async fn (no #[async_trait]) — Rust 1.75+ supports this
    // - Trait methods use `-> impl Future` for object safety with Send bound
    // - Uses diesel-async RunQueryDsl for async .load()/.first() etc.
    // - Table/New/Update types must be in scope where the macro is invoked
    //   (the user brings them in via `use crate::models::*` or similar)

    // §1209: when `sharded`, the extractor resolves tenant → shard via the
    // framework's __autumn_resolve_repo_seed helper instead of pulling the
    // control pool from AppState. The sharded extractor requires `mut parts`
    // so it can call resolve_shard_key (which may extract from request headers).
    let from_request_parts_impl = if config.sharded {
        // Build the idempotency init for the sharded path (reads from `_parts`
        // directly, since the seed doesn't carry it).
        let sharded_idempotency_init = if commit_hooks_enabled {
            quote! {
                idempotency: _parts
                    .extensions
                    .get::<::autumn_web::idempotency::IdempotencyContext>()
                    .cloned(),
            }
        } else {
            quote! {}
        };
        let sharded_hooks_init = config.hooks_type.as_ref().map_or_else(
            || quote! {},
            |hooks_ident| quote! {
                hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default(),
            },
        );
        let sharded_register_hooks = if commit_hooks_enabled {
            quote! { #pg_name::__autumn_register_repository_commit_hooks(); }
        } else {
            quote! {}
        };
        let sharded_read_route = if config.primary_reads {
            quote! { ::autumn_web::repository::ReadRoute::Primary }
        } else {
            quote! { ::core::clone::Clone::clone(&__seed.read_route) }
        };
        quote! {
            impl ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState> for #pg_name {
                type Rejection = ::autumn_web::AutumnError;

                async fn from_request_parts(
                    _parts: &mut ::autumn_web::reexports::http::request::Parts,
                    state: &::autumn_web::AppState,
                ) -> Result<Self, Self::Rejection> {
                    #sharded_register_hooks
                    let (__seed, __shard_set) =
                        ::autumn_web::sharding::__autumn_resolve_repo_seed(_parts, state).await?;
                    ::core::result::Result::Ok(Self {
                        pool: ::core::clone::Clone::clone(&__seed.pool),
                        #sharded_hooks_init
                        #sharded_idempotency_init
                        #tenant_init_field
                        #shards_some_field
                        __autumn_read_route: #sharded_read_route,
                        __autumn_statement_timeout_ms: __seed.statement_timeout_ms,
                        __autumn_slow_threshold: __seed.slow_query_threshold,
                        __autumn_route: ::core::clone::Clone::clone(&__seed.route),
                        #bcast_field_some_state
                    })
                }
            }
        }
    } else {
        quote! {
            // Extractor: pull pool from AppState (same pattern as Db extractor)
            impl ::autumn_web::reexports::axum::extract::FromRequestParts<::autumn_web::AppState> for #pg_name {
                type Rejection = ::autumn_web::AutumnError;

                async fn from_request_parts(
                    _parts: &mut ::autumn_web::reexports::http::request::Parts,
                    state: &::autumn_web::AppState,
                ) -> Result<Self, Self::Rejection> {
                    let pool = state.pool()
                        .ok_or_else(|| ::autumn_web::AutumnError::service_unavailable_msg("No database pool configured"))?
                        .clone();
                    #extractor_init
                }
            }
        }
    };

    // For tenant-scoped sharded repos, implement `CrossShardRepository` so the
    // `CrossShard<Self>` extractor can build an across_tenants() fan-out repo
    // for admin endpoints that have no tenant to route on. Mirrors the routed
    // extractor's field init, but seeds tenant-free (across_tenants: true, no
    // idempotency context — cross-shard writes are rejected anyway).
    let cross_shard_repository_impl = if config.sharded && config.tenant_scoped {
        let register_hooks = if commit_hooks_enabled {
            quote! { #pg_name::__autumn_register_repository_commit_hooks(); }
        } else {
            quote! {}
        };
        let hooks_init = config.hooks_type.as_ref().map_or_else(
            || quote! {},
            |hooks_ident| quote! {
                hooks: <#hooks_ident as ::autumn_web::hooks::RepositoryHooksDefault>::autumn_default(),
            },
        );
        let idempotency_init = if commit_hooks_enabled {
            quote! { idempotency: ::core::option::Option::None, }
        } else {
            quote! {}
        };
        let cross_read_route = if config.primary_reads {
            quote! { ::autumn_web::repository::ReadRoute::Primary }
        } else {
            quote! { ::core::clone::Clone::clone(&__seed.read_route) }
        };
        quote! {
            impl ::autumn_web::sharding::CrossShardRepository for #pg_name {
                fn __autumn_from_cross_shard(
                    __seed: ::autumn_web::sharding::ShardRepositorySeed,
                    __set: ::autumn_web::sharding::ShardSet,
                ) -> Self {
                    #register_hooks
                    Self {
                        pool: ::core::clone::Clone::clone(&__seed.pool),
                        #hooks_init
                        #idempotency_init
                        across_tenants: true,
                        __autumn_shards: ::core::option::Option::Some(__set),
                        __autumn_read_route: #cross_read_route,
                        __autumn_statement_timeout_ms: __seed.statement_timeout_ms,
                        __autumn_slow_threshold: __seed.slow_query_threshold,
                        __autumn_route: ::core::clone::Clone::clone(&__seed.route),
                        #bcast_field_none
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    // For sharded + tenant_scoped repos, the cross-shard read dispatch acquires
    // its own read connection lazily inside the single-shard branch, so a dead
    // parent replica or exhausted parent pool can't fail the fan-out before
    // per-shard routing chooses each shard's own read route/fallback. Other
    // configs acquire up front in the trait method as before.
    let read_conn_acquire = if config.sharded && config.tenant_scoped {
        quote! {}
    } else {
        quote! { let mut conn = self.__autumn_acquire_read_conn().await?; }
    };

    quote! {
        /// Generated repository trait with CRUD + derived queries.
        #vis trait #trait_name: Send + Sync {
            fn find_by_id(&self, id: i64) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Option<#model_name>>> + Send;
            fn find_all(&self) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<Vec<#model_name>>> + Send;
            fn save(&self, new: &#new_name) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<#model_name>> + Send;
            fn update(&self, id: i64, changes: &#update_name) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<#model_name>> + Send;
            fn delete_by_id(&self, id: i64) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<()>> + Send;
            fn count(&self) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<i64>> + Send;
            fn exists_by_id(&self, id: i64) -> impl ::std::future::Future<Output = ::autumn_web::AutumnResult<bool>> + Send;
            #pagination_trait_method
            #cursor_page_trait_method
            #(#derived_trait_methods)*
            #soft_delete_trait_methods
            #bulk_trait_methods
            #search_trait_methods
        }

        // Bridges this repository's tenant/soft-delete config to the model's
        // generated `preload` retain (see `preload_scope_impl`).
        #preload_scope_impl

        // Bridges this repository to `#model_name`'s many-to-many mutation
        // traits (see `M2mConnSource`).
        #m2m_conn_source_impl

        /// Postgres implementation of the repository.
        #vis struct #pg_name {
            #struct_fields
        }

        #from_request_parts_impl

        #cross_shard_repository_impl

        #clone_impl

        #[allow(clippy::useless_let_if_seq)]
        impl #trait_name for #pg_name {
            async fn find_by_id(&self, id: i64) -> ::autumn_web::AutumnResult<Option<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                #read_conn_acquire
                #find_by_id_impl
            }

            async fn find_all(&self) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                #read_conn_acquire
                #find_all_impl
            }

            async fn save(&self, new: &#new_name) -> ::autumn_web::AutumnResult<#model_name> {
                #save_body
            }

            async fn update(&self, id: i64, changes: &#update_name) -> ::autumn_web::AutumnResult<#model_name> {
                #update_body
            }

            async fn delete_by_id(&self, id: i64) -> ::autumn_web::AutumnResult<()> {
                #delete_body
            }

            async fn count(&self) -> ::autumn_web::AutumnResult<i64> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                #read_conn_acquire
                #count_impl
            }

            async fn exists_by_id(&self, id: i64) -> ::autumn_web::AutumnResult<bool> {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                #read_conn_acquire
                #exists_by_id_impl
            }

            #pagination_impl_method
            #cursor_page_impl_method
            #(#derived_impl_methods)*
            #soft_delete_impl_methods

            async fn save_many(&self, new: &[#new_name]) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                #save_many_body
            }

            async fn save_many_skip_invalid(&self, new: &[#new_name]) -> ::autumn_web::AutumnResult<(Vec<#model_name>, Vec<(usize, ::autumn_web::AutumnError)>)> {
                #save_many_skip_invalid_body
            }

            async fn update_many(&self, ids: &[i64], changes: &#update_name) -> ::autumn_web::AutumnResult<Vec<#model_name>> {
                #update_many_body
            }

            async fn delete_many(&self, ids: &[i64]) -> ::autumn_web::AutumnResult<()> {
                #delete_many_body
            }

            #upsert_many_impl_method
            #search_impl_methods
        }

        #[allow(clippy::useless_let_if_seq)]
        impl #pg_name {
            #across_tenants_method
            #for_shard_method
            #find_all_one_shard_helper
            #find_by_id_one_shard_helper
            #count_one_shard_helper
            #exists_by_id_one_shard_helper
            #(#derived_one_shard_helpers)*
            #(#soft_delete_one_shard_helpers)*
            #search_one_shard_helpers
            #with_pool_method
            #hook_support_methods

            /// Returns a clone of this repository whose generated read
            /// methods are pinned to the primary pool for the rest of the
            /// call chain — the read-your-writes escape hatch (#971).
            ///
            /// Use immediately after a write when replication lag would
            /// make a replica read stale:
            ///
            /// ```rust,ignore
            /// let created = repo.save(&new).await?;
            /// let fresh = repo.on_primary().find_by_id(created.id).await?;
            /// ```
            ///
            /// Mutating methods are unaffected — they always run on the
            /// primary.
            #[must_use]
            pub fn on_primary(&self) -> Self {
                let mut repo = ::core::clone::Clone::clone(self);
                repo.__autumn_read_route = ::autumn_web::repository::ReadRoute::Primary;
                repo
            }

            /// Eager-load associations for records returned by a finder.
            ///
            /// Wraps each record in [`Preloaded`](::autumn_web::preload::Preloaded)
            /// and loads every association named in `spec` with at most one
            /// batched `WHERE ... IN (...)` query per association level (plus
            /// one per nested level). No per-row fetches, and no implicit lazy
            /// loading — an un-preloaded association accessor returns
            /// [`NotLoaded`](::autumn_web::preload::NotLoaded).
            ///
            /// Preload SQL runs on the same read role as the finder (the
            /// repository's snapshotted [`ReadRoute`](::autumn_web::repository::ReadRoute)),
            /// so a replica-routed list keeps its preloads on the replica.
            ///
            /// ```rust,ignore
            /// let posts = repo.find_by_subreddit_id(id).await?;
            /// let posts = repo
            ///     .preload(posts, Post::preload().author().comments_with(Comment::preload().author()))
            ///     .await?;
            /// for post in &posts {
            ///     let author = post.author()?;        // typed accessor
            ///     for comment in post.comments()? { /* ... */ }
            /// }
            /// ```
            // Generic over the record + spec types so the `Preloadable` bound
            // rests on a *generic* parameter, not the concrete model. A bound
            // on a concrete type that has no `Preloadable` impl is rejected
            // eagerly; a bound on a generic type parameter is only checked at
            // call sites. This keeps the method available on repositories whose
            // model is hand-written (not via `#[model]`, e.g. zero-column test
            // models) — they simply never call `preload`. In normal use the
            // record type is inferred from the finder result, i.e. this
            // repository's model.
            pub async fn preload<__Model, __Spec>(
                &self,
                records: ::std::vec::Vec<__Model>,
                spec: __Spec,
            ) -> ::autumn_web::AutumnResult<
                ::std::vec::Vec<::autumn_web::preload::Preloaded<__Model>>
            >
            where
                __Model: ::autumn_web::preload::Preloadable<Spec = __Spec>,
                // `Sync` (in addition to `Send`): the preload future borrows
                // `&spec` across an `.await`, so the future is only `Send` when
                // `__Spec: Sync`. `Preloadable::Spec` already requires `Sync`,
                // so this is always satisfied at call sites.
                __Spec: ::core::marker::Send + ::core::marker::Sync,
            {
                // Nothing to preload: skip acquiring a connection entirely.
                if records.is_empty() {
                    return ::core::result::Result::Ok(::std::vec::Vec::new());
                }
                // §1d: a cross-shard result set cannot be preloaded from a
                // single routed connection (associations may live on any
                // shard). Reject under across_tenants on a sharded repo.
                #preload_cross_shard_guard
                let mut conn = self.__autumn_acquire_read_conn().await?;
                let mut wrapped: ::std::vec::Vec<
                    ::autumn_web::preload::Preloaded<__Model>
                > = records
                    .into_iter()
                    .map(::autumn_web::preload::Preloaded::new)
                    .collect();
                // Publish the cross-tenant choice ambiently so every target's
                // tenant retain (including nested levels) honors it, the way
                // `across_tenants()` makes finders skip the tenant predicate.
                ::autumn_web::preload::PRELOAD_ACROSS_TENANTS
                    .scope(#preload_across_expr, async {
                        <__Model as ::autumn_web::preload::Preloadable>::load_associations(
                            &mut wrapped, &spec, &mut *conn,
                        ).await
                    })
                    .await?;
                ::core::result::Result::Ok(wrapped)
            }

            /// The read route this repository snapshot uses for generated
            /// read-only methods. Exposed for tests; not a public API.
            #[doc(hidden)]
            pub fn __autumn_read_route(&self) -> &::autumn_web::repository::ReadRoute {
                &self.__autumn_read_route
            }

            /// The effective read route for the current request, applying any
            /// read-your-own-writes pin from the task-local context.
            ///
            /// When the snapshot is `ReadPool(_)` and the RYWW task-local is
            /// pinned (a primary write occurred earlier in this request or the
            /// incoming session cookie is fresh), returns `Primary` and records
            /// a pin-redirect metric/trace event. All other cases return the
            /// snapshot unchanged.
            ///
            /// Exposed for tests; not a public API.
            #[doc(hidden)]
            pub fn __autumn_effective_read_route(&self) -> ::autumn_web::repository::ReadRoute {
                match &self.__autumn_read_route {
                    ::autumn_web::repository::ReadRoute::ReadPool(_)
                        if ::autumn_web::read_your_writes::is_pinned() =>
                    {
                        ::autumn_web::read_your_writes::note_pin_redirect();
                        ::autumn_web::repository::ReadRoute::Primary
                    }
                    other => ::core::clone::Clone::clone(other),
                }
            }

            /// The primary/write pool. Exposed for tests; not a public API.
            #[doc(hidden)]
            pub fn __autumn_write_pool(
                &self,
            ) -> &::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                ::autumn_web::reexports::diesel_async::AsyncPgConnection,
            > {
                &self.pool
            }

            /// Statement timeout in milliseconds (`0` = no limit).
            /// Exposed for tests; not a public API.
            #[doc(hidden)]
            pub fn __autumn_statement_timeout_ms(&self) -> u64 {
                self.__autumn_statement_timeout_ms
            }

            /// Slow-query logging threshold.
            /// Exposed for tests; not a public API.
            #[doc(hidden)]
            pub fn __autumn_slow_threshold(&self) -> ::std::time::Duration {
                self.__autumn_slow_threshold
            }

            /// Acquire a primary-pool connection for mutating methods and
            /// pessimistic locks. Also used by `#[repository(scope = ...)]`
            /// generated endpoints; not part of the public surface.
            #[doc(hidden)]
            pub async fn __autumn_acquire_conn(
                &self,
            ) -> ::autumn_web::AutumnResult<
                ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Object<
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            > {
                let result = self.__autumn_acquire_from(&self.pool).await;
                // Mark write only after a successful primary checkout, mirroring
                // the guard in `Db::from_request_parts`. A failed acquire (pool
                // exhausted, timeout) must not pin subsequent reads to primary.
                if result.is_ok() {
                    ::autumn_web::read_your_writes::mark_write();
                }
                result
            }

            /// Acquire a connection for a generated read-only method,
            /// following the repository's effective read route: the replica pool
            /// when one is configured and healthy (and no RYWW pin is active),
            /// otherwise the primary (#971, #1201).
            #[doc(hidden)]
            pub async fn __autumn_acquire_read_conn(
                &self,
            ) -> ::autumn_web::AutumnResult<
                ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Object<
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            > {
                // Match by reference to avoid cloning `ReadRoute` on the common
                // (non-pinned) path. The pin check is inlined here so that
                // `__autumn_effective_read_route` (the test accessor) retains its
                // own clone-based return without adding overhead to the hot path.
                if ::autumn_web::read_your_writes::is_pinned() {
                    if matches!(
                        &self.__autumn_read_route,
                        ::autumn_web::repository::ReadRoute::ReadPool(_)
                    ) {
                        ::autumn_web::read_your_writes::note_pin_redirect();
                        return self.__autumn_acquire_from(&self.pool).await;
                    }
                }
                match &self.__autumn_read_route {
                    ::autumn_web::repository::ReadRoute::Primary => {
                        self.__autumn_acquire_from(&self.pool).await
                    }
                    ::autumn_web::repository::ReadRoute::ReadPool(pool) => {
                        self.__autumn_acquire_from(pool).await
                    }
                    ::autumn_web::repository::ReadRoute::Unavailable => {
                        ::core::result::Result::Err(
                            ::autumn_web::AutumnError::service_unavailable_msg(
                                "read replica is configured but not ready, and the \
                                 replica_fallback policy forbids primary reads",
                            ),
                        )
                    }
                }
            }

            async fn __autumn_acquire_from(
                &self,
                pool: &::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            ) -> ::autumn_web::AutumnResult<
                ::autumn_web::reexports::diesel_async::pooled_connection::deadpool::Object<
                    ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                >,
            > {
                use ::autumn_web::reexports::diesel_async::RunQueryDsl as _;
                let mut conn = pool.get().await.map_err(|e| {
                    ::autumn_web::reexports::tracing::error!(
                        "repository: failed to acquire database connection: {e}"
                    );
                    ::autumn_web::AutumnError::service_unavailable_msg(
                        ::std::format!("Database connection error: {e}")
                    )
                })?;
                let timeout_ms = self.__autumn_statement_timeout_ms;
                // Postgres statement_timeout is a signed 32-bit integer; cap to be safe.
                let timeout_ms = timeout_ms.min(i32::MAX as u64);

                ::autumn_web::reexports::diesel::sql_query(
                    ::std::format!("SET statement_timeout = {timeout_ms}")
                )
                .execute(&mut conn)
                .await
                .map_err(|e| {
                    ::autumn_web::reexports::tracing::error!(
                        "repository: failed to set statement_timeout to {timeout_ms}ms: {e}"
                    );
                    ::autumn_web::AutumnError::service_unavailable_msg(
                        ::std::format!("Database initialization error: {e}")
                    )
                })?;
                ::core::result::Result::Ok(conn)
            }

            /// Returns the route label for metrics, e.g. `"GET /users"`.
            /// Exposed for tests; not a public API.
            #[doc(hidden)]
            #[inline]
            pub fn __autumn_route_label(&self) -> &str {
                self.__autumn_route.as_deref().unwrap_or("unknown")
            }

            /// Pessimistic lock helper: SELECT FOR UPDATE the row with
            /// the given `id` inside a transaction, then call `f` with
            /// the locked record and the transaction connection.
            ///
            /// Returns `404 Not Found` if no row with `id` exists.
            pub async fn with_lock<F, T>(&self, id: i64, f: F) -> ::autumn_web::AutumnResult<T>
            where
                F: for<'c> ::core::ops::FnOnce(
                    #model_name,
                    &'c mut ::autumn_web::reexports::diesel_async::AsyncPgConnection,
                ) -> ::autumn_web::reexports::scoped_futures::ScopedBoxFuture<'c, 'c, ::autumn_web::AutumnResult<T>>
                    + ::core::marker::Send + 'static,
                T: ::core::marker::Send + 'static,
            {
                use ::autumn_web::reexports::diesel::prelude::*;
                use ::autumn_web::reexports::diesel_async::RunQueryDsl;
                use ::autumn_web::reexports::diesel_async::AsyncConnection;
                use ::autumn_web::reexports::scoped_futures::ScopedFutureExt as _;

                // with_lock is a write (SELECT ... FOR UPDATE + mutation): reject
                // cross-shard across_tenants(). It locks only the routed shard and
                // drops tenant scoping, so a per-shard-reused id could lock/mutate
                // another tenant's row.
                #cross_shard_write_guard

                let mut conn = self.__autumn_acquire_conn().await?;
                conn.transaction::<T, ::autumn_web::AutumnError, _>(|conn| {
                    async move {
                        let row = #table_ident::table
                            .find(id)
                            .for_update()
                            .first::<#model_name>(conn)
                            .await
                            .optional()
                            .map_err(::autumn_web::AutumnError::from)?
                            .ok_or_else(|| ::autumn_web::AutumnError::not_found_msg(
                                format!("{} with id {} not found", stringify!(#model_name), id)
                            ))?;
                        f(row, conn).await
                    }
                    .scope_boxed()
                })
                .await
            }
        }

        #hook_inventory_registration
        #versioned_inventory_registration

        #api_handlers

        #tenant_scoped_traits

        #upsert_set_ext_impl

        #upsert_execution_ext_impl

        #correlate_ext_impl

        #versioned_record_impl

        #versioned_history_impl

        #search_compile_check

        #internal_hooks_defn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_find_by_single_field() {
        let q = parse_query_name("find_by_title").unwrap();
        assert_eq!(q.prefix, "find");
        assert_eq!(q.fields, vec!["title"]);
    }

    #[test]
    fn parse_find_by_two_fields() {
        let q = parse_query_name("find_by_title_and_published").unwrap();
        assert_eq!(q.prefix, "find");
        assert_eq!(q.fields, vec!["title", "published"]);
        assert_eq!(q.combinator, "and");
    }

    #[test]
    fn parse_count_by() {
        let q = parse_query_name("count_by_published").unwrap();
        assert_eq!(q.prefix, "count");
        assert_eq!(q.fields, vec!["published"]);
    }

    #[test]
    fn parse_delete_by() {
        let q = parse_query_name("delete_by_published").unwrap();
        assert_eq!(q.prefix, "delete");
    }

    #[test]
    fn parse_exists_by() {
        let q = parse_query_name("exists_by_title").unwrap();
        assert_eq!(q.prefix, "exists");
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert!(parse_query_name("save").is_none());
        assert!(parse_query_name("custom_method").is_none());
    }

    #[test]
    fn mixed_and_or_returns_none() {
        assert!(parse_query_name("find_by_a_and_b_or_c").is_none());
    }

    #[test]
    fn parse_repo_args_with_hooks() {
        let tokens: proc_macro2::TokenStream = "Post, hooks = PostHooks".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert_eq!(
            config
                .hooks_type
                .as_ref()
                .map(std::string::ToString::to_string),
            Some("PostHooks".to_string())
        );
        assert!(
            !config.commit_hooks,
            "ordinary hooks must not opt into the durable commit-hook queue"
        );
    }

    #[test]
    fn parse_repo_args_with_commit_hooks() {
        let tokens: proc_macro2::TokenStream = "Post, hooks = PostHooks, commit_hooks = true"
            .parse()
            .unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(config.hooks_type.is_some());
        assert!(config.commit_hooks);
    }

    #[test]
    fn parse_repo_args_rejects_commit_hooks_without_hooks() {
        let tokens: proc_macro2::TokenStream = "Post, commit_hooks = true".parse().unwrap();
        let Err(err) = parse_repo_args(tokens) else {
            panic!("commit hooks require a hook type");
        };
        assert!(
            err.to_string().contains("requires hooks"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn repository_macro_implements_m2m_conn_source() {
        // The many-to-many mutation traits generated by `#[model]`
        // (`#[has_many(Tag, through = post_tags)]`) are blanket-implemented
        // for `M2mConnSource<Model = Post>`; `#[repository(Post, ...)]` must
        // emit exactly that impl on the generated `PgPostRepository`, wired
        // to the same primary-pool connection helper mutating methods use.
        let generated = repository_macro(
            quote! { Post, table = "posts" },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated
                .contains("impl :: autumn_web :: repository :: M2mConnSource for PgPostRepository"),
            "expected an M2mConnSource impl for the generated repository struct: {generated}"
        );
        assert!(
            generated.contains("type Model = Post ;"),
            "expected the M2mConnSource::Model associated type to be the repository's model: {generated}"
        );
        assert!(
            generated.contains("__autumn_m2m_write_conn"),
            "expected the M2mConnSource method to be generated"
        );
    }

    #[test]
    fn repository_macro_api_mutation_prechecks_pin_reads_to_primary() {
        let generated = repository_macro(
            quote! { Post, api = "/api/posts", policy = PostPolicy },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // The PUT/DELETE handlers load the row before writing (404 + policy
        // decision). Under replication lag a replica read could 404 or
        // authorize against a stale row even though the write itself runs on
        // the primary — mutation prechecks must be read-your-writes safe.
        assert_eq!(
            generated
                .matches("repo . on_primary () . find_by_id")
                .count(),
            2,
            "update/delete prechecks must pin find_by_id to the primary"
        );

        // The plain GET endpoint is an ordinary read and stays on the
        // replica route.
        let get_fn = generated
            .find("async fn post_api_get")
            .expect("get handler must be generated");
        let get_end = generated[get_fn..]
            .find("async fn post_api_create")
            .map_or(generated.len(), |offset| get_fn + offset);
        let section = &generated[get_fn..get_end];
        assert!(
            section.contains("repo . find_by_id"),
            "get handler must read through the repository: {section}"
        );
        assert!(
            !section.contains("on_primary"),
            "get handler reads must stay replica-eligible: {section}"
        );
    }

    #[test]
    fn parse_repo_args_with_primary_reads() {
        let tokens: proc_macro2::TokenStream = "Post, primary_reads".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(
            config.primary_reads,
            "primary_reads must pin generated reads to the primary pool"
        );
    }

    #[test]
    fn parse_repo_args_primary_reads_defaults_off() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(
            !config.primary_reads,
            "reads route to the replica by default"
        );
    }

    #[test]
    fn parse_repo_args_without_hooks() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(config.hooks_type.is_none());
    }

    #[test]
    fn parse_repo_args_with_table_and_hooks() {
        let tokens: proc_macro2::TokenStream =
            r#"Post, table = "blog_posts", hooks = PostHooks"#.parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert_eq!(config.table_name, "blog_posts");
        assert_eq!(
            config
                .hooks_type
                .as_ref()
                .map(std::string::ToString::to_string),
            Some("PostHooks".to_string())
        );
    }

    #[test]
    fn parse_repo_args_with_api() {
        let tokens: proc_macro2::TokenStream = r#"Post, api = "/api/posts""#.parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert_eq!(config.api_path.as_deref(), Some("/api/posts"));
    }

    #[test]
    fn policy_repository_api_replays_after_generated_policy_checks() {
        let generated = repository_macro(
            quote! { Post, api = "/api/posts", policy = PostPolicy },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("__replay_response"),
            "policy-backed repository routes must consume cached replays after policy checks: {generated}"
        );
        assert!(
            generated.contains("IdempotencyReplayOr"),
            "policy-backed repository mutations need a response wrapper for post-policy replay: {generated}"
        );
        assert!(
            generated.contains("__replay_metadata")
                && generated.contains("repository.delete.record")
                && generated.contains("InnerWithReplayMetadata"),
            "policy-backed delete retries must carry the deleted record so policy checks can run before replay: {generated}"
        );
    }

    #[test]
    fn parse_repo_args_with_hooks_and_api() {
        let tokens: proc_macro2::TokenStream =
            r#"Post, hooks = PostHooks, api = "/api/v1/posts""#.parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(config.hooks_type.is_some());
        assert_eq!(config.api_path.as_deref(), Some("/api/v1/posts"));
    }

    #[test]
    fn parse_repo_args_without_api() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(config.api_path.is_none());
    }

    #[test]
    fn hooked_repository_without_commit_hooks_uses_ordinary_hooks_only() {
        let output = repository_macro(
            quote! { Post, hooks = PostHooks },
            quote! { pub trait PostRepository {} },
        );
        let generated = output.to_string();

        assert!(
            generated.contains("self . hooks . before_create")
                && generated.contains("self . hooks . after_create")
                && generated.contains("self . hooks . before_update")
                && generated.contains("self . hooks . after_update"),
            "ordinary hooks should still be generated: {generated}"
        );
        assert!(
            !generated.contains("enqueue_repository_commit_hook")
                && !generated.contains("RepositoryCommitHookDescriptor")
                && !generated.contains("__autumn_register_repository_commit_hooks"),
            "ordinary hooks must not require or dispatch through the durable commit-hook queue: {generated}"
        );
    }

    fn durable_hook_repository_tokens() -> String {
        repository_macro(
            quote! { Post, hooks = PostHooks, commit_hooks = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string()
    }

    #[test]
    fn hooked_repository_commit_hooks_register_durable_runner_when_opted_in() {
        let generated = durable_hook_repository_tokens();

        assert!(
            generated.contains("IdempotencyContext"),
            "generated commit-hook repositories must extract the request idempotency context: {generated}"
        );
        assert!(
            generated.contains("ctx . set_idempotency_key"),
            "generated commit-hook repositories must seed MutationContext with the scoped idempotency key: {generated}"
        );
        assert!(
            generated.contains("ctx . idempotency_key . as_deref ()"),
            "generated commit-hook rows must use the scoped idempotency key for durable dedupe: {generated}"
        );
        assert!(
            generated.contains("next_mutation_discriminator"),
            "generated commit-hook rows must include a per-mutation idempotency discriminator: {generated}"
        );
        assert!(
            generated.contains("enqueue_repository_commit_hook_pending_on_conn"),
            "generated repositories must durably stage after_*_commit hooks before the mutation commits: {generated}"
        );
        assert!(
            generated.contains("finalize_repository_commit_hook_after_hook"),
            "generated repositories must only make staged after_*_commit hooks dispatchable after regular after hooks succeed: {generated}"
        );
        assert!(
            generated.contains("mark_repository_commit_hook_after_hook_failed"),
            "generated repositories must mark staged after_*_commit hooks non-dispatchable when regular after hooks fail: {generated}"
        );
        assert!(
            generated.contains("RepositoryCommitHookDescriptor"),
            "generated repositories must register hook runners at link time: {generated}"
        );
        assert!(
            !generated.contains("register_after_commit"),
            "generated after_*_commit hooks must not use the process-local callback registry"
        );
        assert!(
            generated.contains("module_path ! ()"),
            "durable hook handler keys must include the repository module path to avoid cross-module runner collisions: {generated}"
        );
        assert!(
            generated.contains("env ! (\"CARGO_PKG_NAME\")"),
            "durable hook handler keys should include a stable package/table/model identity: {generated}"
        );
        assert!(
            generated.contains("__autumn_commit_hook_to_value")
                && generated.contains("__autumn_commit_hook_from_value"),
            "generated repository hook runners must use the framework's full-fidelity record codec: {generated}"
        );
        assert!(
            !generated.contains("serde_json :: from_value (__record)"),
            "generated repository hook runners must not rehydrate records through public serde JSON: {generated}"
        );
        assert!(
            !generated.contains("self . hooks . after_create (& mut ctx , & record) . await ?"),
            "after_create errors must be reported without rolling back the inserted record: {generated}"
        );
        assert!(
            !generated.contains("self . hooks . after_update (& mut ctx , & record) . await ?"),
            "after_update errors must be reported without rolling back the updated record: {generated}"
        );
    }

    #[test]
    fn hooked_repository_commit_hook_post_commit_failures_are_idempotency_cacheable() {
        let generated = durable_hook_repository_tokens();

        assert!(
            generated.contains("__cache_committed_error_response (__autumn_error)"),
            "post-commit hook failures must be marked cacheable so idempotent retries do not duplicate the committed mutation: {generated}"
        );
        assert!(
            generated
                .contains("failed to finalize repository create commit hook after mutation commit; failing request closed")
                && generated.contains(
                    "failed to finalize repository update commit hook after mutation commit; failing request closed"
                ),
            "post-commit finalization failures should be logged as fail-closed outcomes: {generated}"
        );
    }

    #[test]
    fn hooked_repository_create_commit_hooks_finalize_after_regular_after_hook() {
        let generated = durable_hook_repository_tokens();

        let create_stage = generated
            .find(
                "\"create\" , ctx . idempotency_key . as_deref () , __autumn_commit_hook_discriminator . as_deref () , & ctx , & __autumn_commit_hook_record",
            )
            .expect("create commit hook staging should use the encoded record");
        let create_after = generated
            .find("self . hooks . after_create (& mut ctx , & record)")
            .expect("after_create hook should still be generated");
        let create_drop_conn = generated
            .find(":: core :: mem :: drop (conn)")
            .expect("create path should release the repository connection before after/finalize");
        let create_finalize = generated
            .find("finalize_repository_commit_hook_after_hook (& self . pool , & __autumn_commit_hook_id")
            .expect("create commit hook should be finalized after after_create succeeds");
        assert!(
            create_stage < create_after,
            "create commit hook rows must be staged inside the mutation transaction: {generated}"
        );
        assert!(
            create_stage < create_drop_conn && create_drop_conn < create_after,
            "create path must release its checked-out connection before after_create/finalize checks out from the pool: {generated}"
        );
        assert!(
            create_after < create_finalize,
            "after_create_commit dispatch must see the finalized MutationContext from after_create: {generated}"
        );
        let create_failure_mark = generated
            .find("mark_repository_commit_hook_after_hook_failed")
            .expect("after_create failure path should mark the staged row as non-dispatchable");
        let create_cancel = generated
            .find("__autumn_pending_heartbeat . cancel ()")
            .expect("after_create path should cancel the pending heartbeat");
        assert!(
            generated.contains("catch_repository_after_hook_unwind")
                && generated.contains(":: std :: panic :: resume_unwind")
                && generated.contains("after_create panicked")
                && generated.contains("__cache_committed_error_response"),
            "idempotent after_create panics must cache a committed error while non-idempotent calls still unwind: {generated}"
        );
        assert!(
            create_failure_mark < create_cancel,
            "after_create failure must mark the staged row before canceling its heartbeat: {generated}"
        );
    }

    #[test]
    fn hooked_repository_update_commit_hooks_finalize_after_regular_after_hook() {
        let generated = durable_hook_repository_tokens();

        let update_stage = generated
            .find(
                "\"update\" , ctx . idempotency_key . as_deref () , __autumn_commit_hook_discriminator . as_deref () , & ctx , & __autumn_commit_hook_record",
            )
            .expect("update commit hook staging should use the encoded record");
        let update_after = generated
            .find("self . hooks . after_update (& mut ctx , & record)")
            .expect("after_update hook should still be generated");
        let update_drop_conn = generated[update_stage..update_after]
            .find(":: core :: mem :: drop (conn)")
            .map(|idx| update_stage + idx)
            .expect("update path should release the repository connection before after/finalize");
        let update_finalize = generated
            .rfind("finalize_repository_commit_hook_after_hook (& self . pool , & __autumn_commit_hook_id")
            .expect("update commit hook should be finalized after after_update succeeds");
        assert!(
            update_stage < update_after,
            "update commit hook rows must be staged inside the mutation transaction: {generated}"
        );
        assert!(
            update_stage < update_drop_conn && update_drop_conn < update_after,
            "update path must release its checked-out connection before after_update/finalize checks out from the pool: {generated}"
        );
        assert!(
            update_after < update_finalize,
            "after_update_commit dispatch must see the finalized MutationContext from after_update: {generated}"
        );
        let update_failure_mark = generated[update_after..]
            .find("mark_repository_commit_hook_after_hook_failed")
            .map(|idx| update_after + idx)
            .expect("after_update failure path should mark the staged row as non-dispatchable");
        let update_cancel = generated[update_after..]
            .find("__autumn_pending_heartbeat . cancel ()")
            .map(|idx| update_after + idx)
            .expect("after_update path should cancel the pending heartbeat");
        assert!(
            update_failure_mark < update_cancel,
            "after_update failure must mark the staged row before canceling its heartbeat: {generated}"
        );
        assert!(
            generated.contains("after_update panicked")
                && generated.contains("__cache_committed_error_response")
                && generated.contains(":: std :: panic :: resume_unwind"),
            "idempotent after_update panics must cache a committed error while non-idempotent calls still unwind: {generated}"
        );
    }

    #[test]
    fn hooked_repository_delete_commit_hooks_lock_and_check_deleted_count() {
        let generated = durable_hook_repository_tokens();

        let delete_start = generated
            .find("MutationOp :: Delete")
            .expect("delete path should still be generated");
        let delete_generated = &generated[delete_start..];
        let delete_lock = delete_generated
            .find(". for_update ()")
            .expect("delete path should lock the row before before_delete");
        let before_delete = delete_generated
            .find("before_delete")
            .expect("before_delete hook should still be generated");
        assert!(
            delete_lock < before_delete,
            "delete path must lock the row before invoking before_delete: {generated}"
        );
        assert!(
            generated.contains("let __autumn_deleted =")
                && generated.contains("if __autumn_deleted == 0"),
            "delete path must not enqueue after_delete_commit when no row was deleted: {generated}"
        );
    }

    #[test]
    fn hooked_repository_versioned_save_many_writes_history_before_extending_results() {
        let generated = repository_macro(
            quote! { Post, hooks = PostHooks, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let save_many_pos = generated
            .find("let inputs_ref = & inputs")
            .expect("hooked save_many should keep a reference to prepared inputs");
        let section = &generated[save_many_pos..];
        let insert_pos = section
            .find("let chunk_inserted =")
            .expect("hooked save_many should insert chunks");
        let history_pos = section
            .find("INSERT INTO _autumn_version_history")
            .expect("hooked versioned save_many must write create history");
        let extend_pos = section
            .find("inserted . extend (chunk_inserted)")
            .expect("hooked save_many should extend inserted results");

        assert!(
            insert_pos < history_pos && history_pos < extend_pos,
            "hooked versioned save_many must record history for each inserted record before moving chunk results: {section}"
        );
        assert!(
            section.contains("ctx . actor . as_deref"),
            "hooked versioned save_many history should use the per-record MutationContext: {section}"
        );
    }

    #[test]
    fn hooked_commit_repository_versioned_save_many_writes_history_before_enqueuing_hooks() {
        let generated = repository_macro(
            quote! { Post, hooks = PostHooks, commit_hooks = true, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let save_many_pos = generated
            .find("let (inserted_records , hook_infos , global_indices)")
            .expect("commit-hook save_many should collect inserted records and hook metadata");
        let section = &generated[save_many_pos..];
        let insert_pos = section
            .find("let chunk_inserted =")
            .expect("commit-hook save_many should insert chunks");
        let history_pos = section
            .find("INSERT INTO _autumn_version_history")
            .expect("commit-hook versioned save_many must write create history");
        let enqueue_pos = section
            .find("enqueue_repository_commit_hooks_pending_bulk_on_conn")
            .expect("commit-hook save_many should enqueue create hooks");

        assert!(
            insert_pos < history_pos && history_pos < enqueue_pos,
            "commit-hook versioned save_many must record history inside the mutation transaction before hook enqueue: {section}"
        );
    }

    #[test]
    fn hooked_repository_versioned_delete_many_writes_history() {
        let generated = repository_macro(
            quote! { Post, hooks = PostHooks, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let delete_many_pos = generated
            .find("async fn delete_many")
            .expect("hooked repository should implement delete_many");
        let section = &generated[delete_many_pos..];

        let history_pos = section
            .find("INSERT INTO _autumn_version_history")
            .expect("hooked versioned delete_many must write delete history");

        let delete_pos = section
            .find("diesel :: delete")
            .or_else(|| section.find("diesel :: update"))
            .expect("hooked versioned delete_many must delete/update records");

        assert!(
            delete_pos < history_pos,
            "hooked versioned delete_many must record history after database deletion/update: {section}"
        );
    }

    #[test]
    fn snake_case_simple() {
        assert_eq!(to_snake_case("Bookmark"), "bookmark");
    }

    #[test]
    fn snake_case_multi_word() {
        assert_eq!(to_snake_case("PageRevision"), "page_revision");
    }

    #[test]
    fn snake_case_already_lower() {
        assert_eq!(to_snake_case("widget"), "widget");
    }

    // ── Pagination method generation (issue #681) ──────────────────

    #[test]
    fn parse_repo_args_with_cursor_key() {
        let tokens: proc_macro2::TokenStream = "Post, cursor_key = created_at".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert_eq!(
            config.cursor_key.as_deref(),
            Some("created_at"),
            "cursor_key attribute must be stored on RepoConfig"
        );
        assert!(
            config.cursor_key_type.is_none(),
            "cursor_key_type must be None when not specified"
        );
    }

    #[test]
    fn parse_repo_args_with_cursor_key_and_type() {
        let tokens: proc_macro2::TokenStream =
            "Post, cursor_key = created_at, cursor_key_type = chrono::NaiveDateTime"
                .parse()
                .unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.cursor_key.as_deref(), Some("created_at"));
        assert!(
            config.cursor_key_type.is_some(),
            "cursor_key_type must be parsed when specified"
        );
        assert_eq!(
            config.cursor_key_type.as_ref().map(|p| p
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::")),
            Some("chrono::NaiveDateTime".to_string()),
        );
    }

    #[test]
    fn parse_repo_args_cursor_key_combined_with_api() {
        let tokens: proc_macro2::TokenStream =
            r#"Post, api = "/api/posts", cursor_key = created_at"#
                .parse()
                .unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.api_path.as_deref(), Some("/api/posts"));
        assert_eq!(config.cursor_key.as_deref(), Some("created_at"));
    }

    #[test]
    fn repository_macro_generates_page_method_in_trait_and_impl() {
        let generated =
            repository_macro(quote! { Post }, quote! { pub trait PostRepository {} }).to_string();

        assert!(
            generated.contains("fn page"),
            "repository macro must generate a page() method in the trait: {generated}"
        );
        assert!(
            generated.contains("PageRequest"),
            "page() method must accept a PageRequest parameter: {generated}"
        );
        assert!(
            generated.contains("Page <"),
            "page() method must return Page<Model> in the trait: {generated}"
        );
        assert!(
            generated.contains("order"),
            "page() method must include ORDER BY for deterministic results: {generated}"
        );
    }

    #[test]
    fn repository_macro_generates_cursor_page_when_cursor_key_set() {
        let generated = repository_macro(
            quote! { Post, cursor_key = created_at },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("fn cursor_page"),
            "cursor_key attribute must cause cursor_page() to be generated: {generated}"
        );
        assert!(
            generated.contains("CursorRequest"),
            "cursor_page() must accept a CursorRequest parameter: {generated}"
        );
        assert!(
            generated.contains("CursorPage <"),
            "cursor_page() must return CursorPage<Model>: {generated}"
        );
        // Without cursor_key_type the id-only cursor is used (always compiles).
        assert!(
            generated.contains("decode :: < i64 >") || generated.contains("decode::<i64>"),
            "cursor_page() without cursor_key_type must use id-only cursor: {generated}"
        );
    }

    #[test]
    fn repository_macro_generates_two_part_filter_when_cursor_key_type_set() {
        let generated = repository_macro(
            quote! { Post, cursor_key = created_at, cursor_key_type = chrono::NaiveDateTime },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("fn cursor_page"),
            "cursor_key + cursor_key_type must generate cursor_page(): {generated}"
        );
        // The two-part keyset filter (lt + or + eq + and) must be emitted.
        assert!(
            generated.contains("lt") && generated.contains("eq") && generated.contains("and"),
            "cursor_key_type must cause a two-part keyset filter: {generated}"
        );
        // Concrete type ΓÇö no inference placeholder.
        assert!(
            generated.contains("NaiveDateTime"),
            "cursor_key_type must appear in the decode call: {generated}"
        );
    }

    #[test]
    fn repository_macro_does_not_generate_cursor_page_without_cursor_key() {
        let generated =
            repository_macro(quote! { Post }, quote! { pub trait PostRepository {} }).to_string();

        assert!(
            !generated.contains("cursor_page"),
            "cursor_page() must only be generated when cursor_key is declared: {generated}"
        );
    }

    // ── Soft-delete generation (issue #689) ───────────────────────

    #[test]
    fn parse_repo_args_recognizes_soft_delete_flag() {
        let tokens: proc_macro2::TokenStream = "Post, soft_delete".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(
            config.soft_delete,
            "soft_delete flag must be stored on RepoConfig"
        );
    }

    #[test]
    fn soft_delete_config_is_false_by_default() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(
            !config.soft_delete,
            "soft_delete must default to false when not specified"
        );
    }

    #[test]
    fn soft_delete_combined_with_api() {
        let tokens: proc_macro2::TokenStream =
            r#"Post, soft_delete, api = "/api/posts""#.parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(config.soft_delete);
        assert_eq!(config.api_path.as_deref(), Some("/api/posts"));
    }

    #[test]
    fn soft_delete_combined_with_hooks() {
        let tokens: proc_macro2::TokenStream =
            "Post, soft_delete, hooks = PostHooks".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(config.soft_delete);
        assert!(config.hooks_type.is_some());
    }

    #[test]
    fn repository_macro_soft_delete_generates_restore_and_purge_methods() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("fn restore"),
            "soft_delete must generate a restore() method in the trait: {generated}"
        );
        assert!(
            generated.contains("fn purge"),
            "soft_delete must generate a purge() method in the trait: {generated}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_generates_with_deleted_and_only_deleted() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("fn with_deleted"),
            "soft_delete must generate a with_deleted() method: {generated}"
        );
        assert!(
            generated.contains("fn only_deleted"),
            "soft_delete must generate an only_deleted() method: {generated}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_delete_uses_update_not_hard_delete() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // The delete_by_id body must issue UPDATE, not DELETE FROM
        assert!(
            generated.contains("deleted_at"),
            "soft_delete delete_by_id must reference deleted_at column: {generated}"
        );
        // Must not issue a hard DELETE in delete_by_id (purge is separate)
        // The only DELETE FROM is in purge().
        let delete_count = generated.matches("diesel :: delete").count();
        let purge_count = generated.matches("fn purge").count();
        assert!(
            delete_count <= purge_count + 1,
            "soft_delete delete_by_id must not issue a hard DELETE; only purge() should: {generated}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_find_all_filters_deleted_at_is_null() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("deleted_at") && generated.contains("is_null"),
            "soft_delete find_all must filter rows where deleted_at IS NULL: {generated}"
        );
    }

    #[test]
    fn repository_macro_without_soft_delete_does_not_generate_restore_or_purge() {
        let generated =
            repository_macro(quote! { Post }, quote! { pub trait PostRepository {} }).to_string();

        assert!(
            !generated.contains("fn restore"),
            "non-soft-delete repository must not generate restore(): {generated}"
        );
        assert!(
            !generated.contains("fn purge"),
            "non-soft-delete repository must not generate purge(): {generated}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_purge_issues_hard_delete() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("diesel :: delete"),
            "purge() must issue a hard DELETE FROM: {generated}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_only_deleted_filters_is_not_null() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("is_not_null"),
            "only_deleted() must filter rows where deleted_at IS NOT NULL: {generated}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_find_by_id_excludes_deleted_rows() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // Locate find_by_id impl and check it has the is_null filter.
        let find_by_id = generated
            .find("async fn find_by_id")
            .expect("find_by_id must be generated");
        let section = &generated[find_by_id..find_by_id + 400];
        assert!(
            section.contains("is_null"),
            "find_by_id must filter deleted_at IS NULL: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_find_all_impl_filters_deleted_rows() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let find_all = generated
            .find("async fn find_all")
            .expect("find_all must be generated");
        let section = &generated[find_all..find_all + 400];
        assert!(
            section.contains("is_null"),
            "find_all impl must filter deleted_at IS NULL: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_count_filters_deleted_rows() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let count_pos = generated
            .find("async fn count")
            .expect("count must be generated");
        let section = &generated[count_pos..count_pos + 400];
        assert!(
            section.contains("is_null"),
            "count impl must filter deleted_at IS NULL: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_exists_by_id_filters_deleted_rows() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let exists_pos = generated
            .find("async fn exists_by_id")
            .expect("exists_by_id must be generated");
        let section = &generated[exists_pos..exists_pos + 800];
        assert!(
            section.contains("is_null"),
            "exists_by_id impl must filter deleted_at IS NULL: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_delete_by_id_targets_only_non_deleted() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let delete_pos = generated
            .find("async fn delete_by_id")
            .expect("delete_by_id must be generated");
        let section = &generated[delete_pos..delete_pos + 600];
        assert!(
            section.contains("is_null"),
            "delete_by_id soft-delete UPDATE must add deleted_at IS NULL guard: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_hooked_prefetch_applies_sd_filter() {
        // With hooks, the prefetch SELECT must include deleted_at IS NULL so
        // before_delete only fires for actually-deletable rows.
        let generated = repository_macro(
            quote! { Post, soft_delete, hooks = PostHooks },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // Locate the prefetch (before_delete context load) inside the
        // transaction block - search for the for_update call site.
        let prefetch_pos = generated
            .find("for_update")
            .expect("hooked delete must generate a for_update prefetch");
        // The is_null filter must appear BEFORE for_update in the chain.
        let preamble = &generated[..prefetch_pos];
        let last_is_null = preamble.rfind("is_null");
        assert!(
            last_is_null.is_some(),
            "hooked prefetch must apply deleted_at IS NULL before for_update: {preamble}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_derived_delete_uses_update_not_hard_delete() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {
                fn delete_by_title(title: String);
            } },
        )
        .to_string();

        // The derived delete_by_title impl (not the trait signature) must be
        // an UPDATE that sets deleted_at, not a hard DELETE FROM.
        let impl_delete = generated
            .find("async fn delete_by_title")
            .expect("delete_by_title impl must be generated");
        let section = &generated[impl_delete..impl_delete + 800];
        assert!(
            section.contains("deleted_at"),
            "derived delete_by_title must reference deleted_at in soft-delete mode: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_cursor_page_applies_sd_filter() {
        let generated = repository_macro(
            quote! { Post, soft_delete, cursor_key = created_at },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let cursor_pos = generated
            .find("async fn cursor_page")
            .expect("cursor_page impl must be generated");
        let section = &generated[cursor_pos..cursor_pos + 800];
        assert!(
            section.contains("is_null"),
            "cursor_page impl must apply deleted_at IS NULL filter in soft-delete mode: {section}"
        );
    }

    #[test]
    fn repository_macro_soft_delete_generates_page_only_deleted_method() {
        let generated = repository_macro(
            quote! { Post, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("page_only_deleted"),
            "soft_delete must generate a page_only_deleted() method: {generated}"
        );
    }

    // ── Tenant Scoped generation (issue #695) ───────────────────

    #[test]
    fn parse_repo_args_recognizes_tenant_scoped_flag() {
        let tokens: proc_macro2::TokenStream = "Post, tenant_scoped".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(
            config.tenant_scoped,
            "tenant_scoped flag must be stored on RepoConfig"
        );
    }

    #[test]
    fn tenant_scoped_config_is_false_by_default() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(
            !config.tenant_scoped,
            "tenant_scoped must default to false when not specified"
        );
    }

    #[test]
    fn repository_macro_tenant_scoped_generates_across_tenants() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("across_tenants"),
            "tenant_scoped must generate an across_tenants() method on the struct: {generated}"
        );
    }

    // ── Versioned history (issue #700) ─────────────────────────────

    #[test]
    fn parse_repo_args_recognizes_versioned_flag() {
        let tokens: proc_macro2::TokenStream = "Post, versioned = true".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(
            config.versioned,
            "versioned = true flag must be stored on RepoConfig"
        );
    }

    #[test]
    fn versioned_config_is_false_by_default() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(
            !config.versioned,
            "versioned must default to false when not specified"
        );
    }

    #[test]
    fn parse_repo_args_versioned_false_explicitly() {
        let tokens: proc_macro2::TokenStream = "Post, versioned = false".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(!config.versioned, "versioned = false must remain false");
    }

    #[test]
    fn repository_macro_versioned_generates_history_method() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("version_history") || generated.contains("versioned"),
            "versioned = true must generate version-history-related code: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_registers_framework_migration_descriptor() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("VersionedRepositoryDescriptor"),
            "versioned repositories must register a link-time descriptor so app startup installs the version-history migration: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_generates_versioned_record_impl() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("VersionedRecord"),
            "versioned = true must generate impl VersionedRecord for Post: {generated}"
        );
        assert!(
            generated.contains("version_table_name"),
            "generated VersionedRecord impl must include version_table_name: {generated}"
        );
        assert!(
            generated.contains("version_sensitive_columns"),
            "generated VersionedRecord impl must include version_sensitive_columns: {generated}"
        );
    }

    #[test]
    fn repository_macro_tenant_scoped_versioned_stores_tenant_id_in_history() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("tenant_id, record_id")
                && generated.contains("__vh_tenant_id")
                && generated.contains("version_tenant_id"),
            "tenant-scoped history writes must persist tenant_id for fail-closed history reads: {generated}"
        );
    }

    #[test]
    fn repository_macro_tenant_scoped_version_history_filters_current_tenant() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("CURRENT_TENANT")
                && generated.contains("tenant_id = $3")
                && generated.contains("self . across_tenants"),
            "tenant-scoped version_history must default to CURRENT_TENANT and only bypass through across_tenants(): {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_update_locks_before_history_diff_without_expected_version() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let no_expected_branch = generated
            .find("} else { load_query")
            .expect("versioned update should have a no-expected-version load branch");
        let section = &generated[no_expected_branch..];
        let lock_pos = section
            .find(". for_update ()")
            .expect("versioned update must lock the row before computing history diff");
        let first_pos = section
            .find(". first :: < Post >")
            .expect("versioned update should load the row before applying the update");
        let history_pos = section
            .find("INSERT INTO _autumn_version_history")
            .expect("versioned update should write history");

        assert!(
            lock_pos < first_pos && first_pos < history_pos,
            "versioned update must SELECT FOR UPDATE before loading the before image and writing history: {section}"
        );
    }

    #[test]
    fn repository_macro_versioned_upsert_many_locks_keys_before_history_snapshot() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let lock_pos = generated
            .find("pg_advisory_xact_lock")
            .expect("versioned upsert_many must lock logical upsert keys before pre-reading rows");
        let upsert_section = &generated[lock_pos..];
        let load_pos = upsert_section
            .find("let existing_rows =")
            .expect("versioned upsert_many should load existing rows before writing history");
        let before_map_pos = upsert_section
            .find("let __vh_before_map")
            .expect("versioned upsert_many should snapshot before images for history");
        let history_pos = upsert_section
            .find("INSERT INTO _autumn_version_history")
            .expect("versioned upsert_many should write history entries");

        assert!(
            load_pos < before_map_pos && before_map_pos < history_pos,
            "versioned upsert_many must serialize keys before classifying insert/update history: {generated}"
        );
        assert!(
            generated.contains("repository_upsert_advisory_lock_key"),
            "versioned upsert_many must derive stable advisory lock keys through the runtime helper: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_upsert_many_locks_all_keys_before_chunk_loop() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let lock_pos = generated
            .find("pg_advisory_xact_lock")
            .expect("versioned upsert_many must acquire advisory locks");
        let chunk_loop_pos = generated
            .find("records . chunks (chunk_size)")
            .expect("versioned upsert_many should still chunk database writes");

        assert!(
            lock_pos < chunk_loop_pos,
            "versioned upsert_many must acquire all advisory locks in global sorted order before chunking: {generated}"
        );
        assert!(
            generated.contains("records . iter () . map (| r | r . id)"),
            "versioned upsert_many lock set must be collected from all input records, not the current chunk: {generated}"
        );
    }

    #[test]
    fn repository_macro_tenant_scoped_upsert_many_rejects_partial() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("! has_lock && upserted . len () != records . len ()"),
            "tenant-scoped upsert_many must reject partial upserts when lock versioning is absent: {generated}"
        );
    }

    #[test]
    fn repository_macro_tenant_scoped_versioned_tenant_id_handles_optional_string() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            !generated.contains("self . tenant_id . as_str"),
            "tenant-scoped VersionedRecord must not assume tenant_id is a bare String: {generated}"
        );
        assert!(
            generated.contains("VersionTenantIdValue :: version_tenant_id (& self . tenant_id)"),
            "tenant-scoped VersionedRecord must delegate tenant_id extraction to the runtime helper: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_sensitive_columns_parsed_from_attr() {
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! {
                #[version_history(sensitive = ["password_digest", "reset_token"])]
                pub trait PostRepository {}
            },
        )
        .to_string();

        assert!(
            generated.contains("password_digest"),
            "sensitive columns must appear in the generated VersionedRecord impl: {generated}"
        );
        assert!(
            generated.contains("reset_token"),
            "all sensitive columns must appear in the generated VersionedRecord impl: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_sensitive_columns_merged_from_multiple_attrs() {
        // Columns split across two #[version_history(...)] attrs must all appear.
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! {
                #[version_history(sensitive = ["password_digest"])]
                #[version_history(sensitive = ["api_key"])]
                pub trait PostRepository {}
            },
        )
        .to_string();

        assert!(
            generated.contains("password_digest"),
            "first attr sensitive columns must be present: {generated}"
        );
        assert!(
            generated.contains("api_key"),
            "second attr sensitive columns must be present: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_non_string_in_sensitive_is_compile_error() {
        // A non-string element (e.g. a bare identifier instead of a quoted
        // string) must produce a compile error rather than silently being
        // skipped and leaving the column unredacted.
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! {
                #[version_history(sensitive = [password_digest])]
                pub trait PostRepository {}
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "non-string element in sensitive list must produce a compile error: {generated}"
        );
        // The compile_error must not also produce a working VersionedRecord impl
        // (version_table_name is only present in the generated impl block).
        assert!(
            !generated.contains("version_table_name"),
            "a compile-error output must not also contain a working impl: {generated}"
        );
    }

    #[test]
    fn repository_macro_versioned_typo_in_sensitive_attr_is_compile_error() {
        // A typo in the key name (e.g. "sensitve") must NOT silently compile
        // and leave sensitive columns unredacted.  The macro must emit a
        // compile_error token stream so the build fails with a clear message.
        let generated = repository_macro(
            quote! { Post, versioned = true },
            quote! {
                #[version_history(sensitve = ["password_digest"])]
                pub trait PostRepository {}
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error") || generated.contains("unknown version_history"),
            "typo in sensitive attribute must produce a compile error, not silently succeed: {generated}"
        );
        assert!(
            !generated.contains("password_digest"),
            "typo must not cause sensitive column to be silently omitted: {generated}"
        );
    }

    #[test]
    fn repository_macro_no_versioned_record_impl_suppresses_generated_impl() {
        // When a model already has a manual VersionedRecord impl, use
        // no_versioned_record_impl to avoid the duplicate-impl (E0119) error.
        let generated = repository_macro(
            quote! { Post, versioned = true, no_versioned_record_impl },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // version_table_name is only emitted inside the impl VersionedRecord for Model
        // block; its absence proves the impl was suppressed.  VersionedRecord itself
        // still appears in the write-path call sites, so we cannot check for that.
        assert!(
            !generated.contains("version_table_name"),
            "no_versioned_record_impl must suppress the generated impl block: {generated}"
        );
        // The write paths and version_history query method must still be present.
        assert!(
            generated.contains("version_history"),
            "version_history query method must still be generated: {generated}"
        );
    }

    #[test]
    fn repository_macro_no_versioned_record_impl_uses_trait_hooks_for_history_writes() {
        // Manual VersionedRecord impls own the values written to history. The
        // generated write paths must call those hooks instead of serializing the
        // model directly, otherwise redaction and normalization are bypassed.
        let generated = repository_macro(
            quote! { Post, versioned = true, no_versioned_record_impl },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("version_column_values"),
            "history writes must call VersionedRecord::version_column_values(): {generated}"
        );
        assert!(
            generated.contains("version_record_id"),
            "history writes must call VersionedRecord::version_record_id(): {generated}"
        );
    }

    #[test]
    fn repository_macro_non_versioned_does_not_emit_versioned_record_impl() {
        let generated =
            repository_macro(quote! { Post }, quote! { pub trait PostRepository {} }).to_string();

        assert!(
            !generated.contains("VersionedRecord"),
            "non-versioned repository must not generate VersionedRecord impl: {generated}"
        );
    }

    #[test]
    fn repository_macro_non_versioned_does_not_regress() {
        // Repositories that do not opt in must compile and run unchanged.
        // Verify the generated output does not contain any history code.
        let generated =
            repository_macro(quote! { Post }, quote! { pub trait PostRepository {} }).to_string();

        // The basic CRUD methods must still be present
        assert!(
            generated.contains("find_by_id"),
            "non-versioned repository must still generate find_by_id"
        );
        assert!(
            generated.contains("save"),
            "non-versioned repository must still generate save"
        );
    }

    #[test]
    fn parse_repo_args_sharded_flag() {
        let tokens: proc_macro2::TokenStream = "Post, tenant_scoped, sharded".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert_eq!(config.model_name.to_string(), "Post");
        assert!(config.sharded, "sharded attribute must be set");
        assert!(config.tenant_scoped, "tenant_scoped must also be set");
    }

    #[test]
    fn parse_repo_args_sharded_defaults_off() {
        let tokens: proc_macro2::TokenStream = "Post".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(!config.sharded, "sharded must default to false");
    }

    #[test]
    fn parse_repo_args_sharded_alone() {
        // `sharded` without `tenant_scoped` is allowed — routing uses
        // ShardKeyOverride or the tenancy config.
        let tokens: proc_macro2::TokenStream = "Post, sharded".parse().unwrap();
        let config = parse_repo_args(tokens).unwrap();
        assert!(config.sharded);
        assert!(!config.tenant_scoped);
    }

    // ── §1d: sharded + tenant_scoped across_tenants fan-out ────────

    #[test]
    fn sharded_tenant_scoped_find_all_contains_fan_out_guard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // The fan-out guard iterates __shards
        assert!(
            generated.contains("__shards"),
            "sharded+tenant_scoped find_all must contain fan-out guard over __shards: {generated}"
        );
        // It must build a per-shard sub-repo via __autumn_for_shard, which
        // honors that shard's read routing and the parent request context
        // (rather than with_pool_untracked, which forces primary + resets the
        // timeout).
        assert!(
            generated.contains("__autumn_for_shard"),
            "fan-out guard must build sub-repo via __autumn_for_shard: {generated}"
        );
    }

    #[test]
    fn sharded_for_shard_helper_honors_read_route_and_timeout() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // __autumn_for_shard must source pool + read route from the shard and
        // carry the parent's statement timeout / slow threshold.
        assert!(
            generated.contains("fn __autumn_for_shard"),
            "must emit the __autumn_for_shard helper: {generated}"
        );
        assert!(
            generated.contains("__shard . read_route ()")
                || generated.contains("__shard.read_route()"),
            "sub-repo must adopt the shard's read_route: {generated}"
        );
        assert!(
            generated
                .contains("__autumn_statement_timeout_ms : self . __autumn_statement_timeout_ms")
                || generated
                    .contains("__autumn_statement_timeout_ms: self.__autumn_statement_timeout_ms"),
            "sub-repo must preserve the parent statement timeout: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_find_by_id_fans_out() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("__autumn_find_by_id_one_shard"),
            "sharded+tenant_scoped find_by_id must fan out via a one-shard helper: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_derived_read_fans_out() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! {
                pub trait PostRepository {
                    async fn find_by_title(&self, title: String) -> Vec<Post>;
                }
            },
        )
        .to_string();

        // A derived read fans out via a generated one-shard helper rather than
        // hitting only the originally-routed shard.
        assert!(
            generated.contains("__autumn_find_by_title_one_shard"),
            "derived read must fan out via a one-shard helper: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_derived_read_with_borrowed_param_rejects() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! {
                pub trait PostRepository {
                    async fn find_by_title(&self, title: &str) -> Vec<Post>;
                }
            },
        )
        .to_string();

        // A borrowed param can't be cloned into an owned 'static value for the
        // fan-out futures, so it must reject cross-shard rather than emit a
        // one-shard helper (which would not compile).
        assert!(
            !generated.contains("__autumn_find_by_title_one_shard"),
            "borrowed-param derived read must not fan out: {generated}"
        );
        assert!(
            generated.contains("cross-shard find_by_title is not supported"),
            "borrowed-param derived read must reject cross-shard: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_derived_write_rejects_cross_shard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! {
                pub trait PostRepository {
                    async fn delete_by_title(&self, title: String);
                }
            },
        )
        .to_string();

        assert!(
            generated.contains("cross-shard derived writes are not supported"),
            "derived write on a sharded across_tenants repo must reject: {generated}"
        );
    }

    #[test]
    fn sharded_fan_out_calls_inherent_one_shard_helpers_not_trait_methods() {
        // Regression: the fan-out must route through inherent
        // `__autumn_*_one_shard` helpers rather than recursively calling the
        // RPITIT trait methods, or the read futures' `Send` auto-trait becomes
        // unprovable once hooks/versioning add captured state (issue #1209 §1d).
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded, versioned = true, hooks = PostHooks },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        for helper in [
            "__autumn_find_all_one_shard",
            "__autumn_count_one_shard",
            "__autumn_exists_by_id_one_shard",
        ] {
            assert!(
                generated.contains(helper),
                "fan-out must define and call the inherent helper {helper}: {generated}"
            );
        }
    }

    #[test]
    fn versioned_tenant_scoped_delete_many_chunk_is_a_block_expression() {
        // Regression: the chunked delete in the versioned path is assigned with
        // `let chunk_deleted_ids = <expr>`, so the tenant-scoped
        // `delete_returning_expr` must be a braced block — otherwise it emits
        // `let chunk_deleted_ids = let query = ...` (issue #1209 §1e).
        let generated = repository_macro(
            quote! { Post, tenant_scoped, versioned = true, hooks = PostHooks },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            generated.contains("let chunk_deleted_ids = {"),
            "versioned+tenant_scoped delete_many must wrap the chunk expression in a block: {generated}"
        );
        assert!(
            !generated.contains("let chunk_deleted_ids = let "),
            "versioned+tenant_scoped delete_many must not emit `let x = let y` : {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_count_contains_fan_out_guard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let count_pos = generated
            .find("async fn count")
            .expect("count must be generated");
        let section = &generated[count_pos..count_pos + 600];
        assert!(
            section.contains("__shards"),
            "sharded+tenant_scoped count must fan out across shards: {section}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_exists_by_id_contains_fan_out_guard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let pos = generated
            .find("async fn exists_by_id")
            .expect("exists_by_id must be generated");
        let section = &generated[pos..pos + 800];
        assert!(
            section.contains("__shards"),
            "sharded+tenant_scoped exists_by_id must fan out across shards: {section}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_write_methods_have_cross_shard_guard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // The error message for cross-shard writes must appear in generated code
        assert!(
            generated.contains("cross-shard writes are not supported"),
            "sharded+tenant_scoped write methods must include cross-shard write guard: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_page_has_cross_shard_guard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        let page_pos = generated
            .find("async fn page")
            .expect("page must be generated");
        let section = &generated[page_pos..page_pos + 600];
        assert!(
            section.contains("cross-shard pagination is not supported"),
            "sharded+tenant_scoped page() must include cross-shard pagination guard: {section}"
        );
    }

    #[test]
    fn non_sharded_tenant_scoped_has_no_fan_out_guard() {
        // Non-sharded repos must NOT have the shard fan-out code
        let generated = repository_macro(
            quote! { Post, tenant_scoped },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            !generated.contains("cross-shard writes are not supported"),
            "non-sharded repo must not contain cross-shard write guard: {generated}"
        );
        assert!(
            !generated.contains("cross-shard pagination is not supported"),
            "non-sharded repo must not contain cross-shard pagination guard: {generated}"
        );
    }

    // ── §1d: cross-shard soft-delete / search / version_history / preload ──

    #[test]
    fn sharded_tenant_scoped_soft_delete_readers_fan_out_and_reject() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // with_deleted / only_deleted fan out via inherent one-shard helpers.
        assert!(
            generated.contains("__autumn_with_deleted_one_shard"),
            "with_deleted must fan out via a one-shard helper: {generated}"
        );
        assert!(
            generated.contains("__autumn_only_deleted_one_shard"),
            "only_deleted must fan out via a one-shard helper: {generated}"
        );
        // page_only_deleted is paginated and must reject cross-shard.
        assert!(
            generated.contains("cross-shard page_only_deleted is not supported"),
            "page_only_deleted must reject cross-shard: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_search_fans_out_and_search_page_rejects() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded, searchable },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // search (Vec-returning) fans out via an inherent one-shard helper.
        assert!(
            generated.contains("__autumn_search_one_shard"),
            "search must fan out via a one-shard helper: {generated}"
        );
        // search_page is paginated and must reject cross-shard.
        assert!(
            generated.contains("cross-shard search_page is not supported"),
            "search_page must reject cross-shard: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_version_history_rejects_cross_shard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded, versioned = true },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // version_history is keyed by a single (per-shard ambiguous) record_id
        // and is paginated, so it rejects rather than fanning out.
        assert!(
            generated.contains("cross-shard version_history is not supported"),
            "version_history must reject cross-shard: {generated}"
        );
    }

    #[test]
    fn sharded_tenant_scoped_preload_rejects_cross_shard() {
        let generated = repository_macro(
            quote! { Post, tenant_scoped, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        // preload cannot load associations from a single routed connection
        // across shards, so it rejects cross-shard.
        assert!(
            generated.contains("cross-shard preload is not supported"),
            "preload must reject cross-shard: {generated}"
        );
    }

    #[test]
    fn non_sharded_soft_delete_readers_have_no_fan_out() {
        // Non-sharded soft_delete repos keep the plain readers (no helpers,
        // no reject messages).
        let generated = repository_macro(
            quote! { Post, tenant_scoped, soft_delete },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            !generated.contains("__autumn_with_deleted_one_shard"),
            "non-sharded with_deleted must not emit a one-shard helper: {generated}"
        );
        assert!(
            !generated.contains("cross-shard page_only_deleted is not supported"),
            "non-sharded page_only_deleted must not reject cross-shard: {generated}"
        );
    }

    #[test]
    fn sharded_without_tenant_scoped_has_no_write_guard() {
        // `sharded` alone (no `tenant_scoped`) means no across_tenants(), so
        // the write guard is also not generated.
        let generated = repository_macro(
            quote! { Post, sharded },
            quote! { pub trait PostRepository {} },
        )
        .to_string();

        assert!(
            !generated.contains("cross-shard writes are not supported"),
            "sharded-only repo must not contain cross-shard write guard: {generated}"
        );
    }
}
