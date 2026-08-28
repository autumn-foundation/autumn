//! `#[cached]` proc macro implementation.
//!
//! Wraps an async (or sync) function with an in-memory cache backed by
//! `autumn_web::cache::MokaCache` (default) via the `autumn_web::cache::Cache`
//! trait. Each annotated function gets its own `static` cache instance,
//! keyed by a hash of the function arguments.
//!
//! # Supported attributes
//!
//! | Attribute | Example | Description |
//! |-----------|---------|-------------|
//! | `ttl` | `"5m"` | Time-to-live per entry (parsed at startup) |
//! | `max` | `1000` | Max entries; LRU eviction via moka |
//! | `result` | (flag) | Only cache `Ok` values; pass `Err` through |
//! | `key` | `key(tenant_id)` | Build the cache key from *these* parameters only |
//! | `reads` | `reads(Post, Comment)` | Declared cache-coherence dependency set (#1716) |
//! | `acknowledge_stale` | `"5s ttl is tight enough"` | Opt this read out of the coherence gate |
//!
//! # Cache coherence (#1716)
//!
//! Every annotated function also publishes a
//! [`CachedReadDescriptor`](autumn_web::cache::coherence::CachedReadDescriptor)
//! through `inventory`, recording which models the cached value is derived
//! from. `reads(...)` declares that set; without it the set is *derived* from
//! the function's own signature and body, and a function nothing could be
//! recovered from is recorded as `undetermined` (reported, never gated).
//!
//! The registration is split so this stays usable everywhere `#[cached]` already
//! was: the identity constant and the invalidator are valid associated items, and
//! the `inventory` submission — an anonymous `const _`, which an `impl` block
//! cannot hold — goes inside the function body.

use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ext::IdentExt as _;
use syn::parse::Parser as _;
use syn::punctuated::Punctuated;
use syn::visit_mut::VisitMut;
use syn::{Expr, Ident, ItemFn, LitInt, LitStr, Token};

struct CachedAttrs {
    ttl: Option<String>,
    max: Option<usize>,
    result: bool,
    /// `key(tenant_id, page)` — build the cache key from these parameters only.
    ///
    /// Without it every parameter is hashed, which makes `#[cached]` unusable
    /// over a repository read: the handle a cached read needs (`repo:
    /// &PgProjectRepository`) is `Clone` but not `Hash`, and it is not part of
    /// the value's identity anyway. Naming the key parameters explicitly is
    /// what lets a cached read take the handle it reads through — the shape
    /// this whole feature assumes.
    key: Vec<syn::Ident>,
    /// `reads(Post, Comment)` — the declared cache-coherence dependency set.
    /// Empty means "not declared", which sends the macro to derivation.
    reads: Vec<syn::Path>,
    /// `acknowledge_stale = "reason"` — opt this read out of the coherence
    /// gate. The reason is mandatory and must be non-blank so an escape hatch
    /// always carries its justification into the manifest.
    acknowledge_stale: Option<String>,
}

/// Try to parse `max` as either a string literal or an integer literal.
fn parse_max_value(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<usize> {
    let expr: Expr = meta.value()?.parse()?;
    match &expr {
        Expr::Lit(lit) => match &lit.lit {
            syn::Lit::Int(int) => int.base10_parse::<usize>(),
            syn::Lit::Str(s) => s
                .value()
                .parse::<usize>()
                .map_err(|_| syn::Error::new_spanned(s, "max must be a positive integer")),
            _ => Err(syn::Error::new_spanned(&expr, "max must be an integer")),
        },
        _ => Err(syn::Error::new_spanned(
            &expr,
            "max must be a literal integer",
        )),
    }
}

fn parse_cached_args(attr: TokenStream) -> syn::Result<CachedAttrs> {
    let mut result = CachedAttrs {
        ttl: None,
        max: None,
        result: false,
        key: Vec::new(),
        reads: Vec::new(),
        acknowledge_stale: None,
    };

    if attr.is_empty() {
        return Ok(result);
    }

    syn::meta::parser(|meta| {
        if meta.path.is_ident("ttl") {
            let value: LitStr = meta.value()?.parse()?;
            result.ttl = Some(value.value());
            Ok(())
        } else if meta.path.is_ident("max") {
            result.max = Some(parse_max_value(&meta)?);
            Ok(())
        } else if meta.path.is_ident("result") {
            result.result = true;
            Ok(())
        } else if meta.path.is_ident("key") {
            // Parsed through an explicit `parenthesized!` rather than
            // `parse_nested_meta` so that the EMPTY case reaches our own
            // diagnostic instead of syn's "expected nested attribute": an empty
            // list is the mistake worth explaining.
            let content;
            syn::parenthesized!(content in meta.input);
            let params: Punctuated<Ident, Token![,]> =
                content.parse_terminated(Ident::parse_any, Token![,])?;
            if params.is_empty() {
                return Err(meta.error(
                    "`key()` must name at least one parameter, e.g. `key(tenant_id)`; omit it \
                     entirely to key on every parameter",
                ));
            }
            result.key.extend(params);
            Ok(())
        } else if meta.path.is_ident("reads") {
            // `reads(Post, crate::models::Comment)` — every entry is a model
            // *path*, so a typo is a rustc error at the declaration site rather
            // than a silently-unmatched string in the manifest.
            let content;
            syn::parenthesized!(content in meta.input);
            let models: Punctuated<syn::Path, Token![,]> =
                content.parse_terminated(syn::Path::parse_mod_style, Token![,])?;
            if models.is_empty() {
                return Err(meta.error(
                    "`reads()` must name at least one model, e.g. `reads(Post)`; omit it \
                     entirely to let the macro derive the dependency set",
                ));
            }
            result.reads.extend(models);
            Ok(())
        } else if meta.path.is_ident("acknowledge_stale") {
            let value: LitStr = meta.value()?.parse()?;
            let reason = value.value();
            if reason.trim().is_empty() {
                return Err(syn::Error::new_spanned(
                    &value,
                    "`acknowledge_stale` requires a non-empty reason: it is the only record of \
                     why this cached read is allowed to serve stale data",
                ));
            }
            result.acknowledge_stale = Some(reason);
            Ok(())
        } else {
            Err(meta.error(
                "unsupported attribute: expected ttl, max, result, key, reads, or \
                 acknowledge_stale",
            ))
        }
    })
    .parse2(attr)?;

    Ok(result)
}

// ── #1716: dependency derivation ─────────────────────────────────────

/// Associated functions on a model type that read persistent rows.
///
/// Deliberately a closed list of *reading* verbs. A path call like
/// `Post::find_all(db)` is evidence the cached value is derived from `Post`
/// rows; `Post::new(..)` is not.
const MODEL_READ_VERBS: &[&str] = &[
    "find_all",
    "find_by_id",
    "count",
    "exists_by_id",
    "list",
    "page",
    "all",
    "load",
    "first",
];

/// Recover the model name from a repository type name.
///
/// `PgPostRepository` and `PostRepository` both *look like* the `Post` model —
/// the concrete struct and the trait `#[repository(Post)]` generates.
///
/// This is a guess about a NAME, and the name can lie: `#[repository(Comment)]
/// trait ModerationRepository` is supported, and there the guess says
/// `Moderation` while the repository really writes `Comment`. It is used only
/// to *recognise* a repository type (#1716), never to state the model a read
/// depends on — the concrete `Pg*` path carries
/// `__AUTUMN_MODEL_NAME`, and that is what the descriptor emits, so rustc
/// resolves the model rather than this heuristic guessing it.
fn model_from_repository_ident(ident: &str) -> Option<String> {
    let base = ident.strip_suffix("Repository")?;
    let base = base.strip_prefix("Pg").unwrap_or(base);
    let mut chars = base.chars();
    if !chars.next()?.is_ascii_uppercase() {
        return None;
    }
    Some(base.to_string())
}

/// Whether this ident is the CONCRETE repository struct, the one
/// `#[repository]` hangs `__AUTUMN_MODEL_NAME` on.
///
/// The trait of the same family (`PostRepository`) does not carry the constant,
/// so a read bounded on the trait cannot have its model resolved here.
fn is_concrete_repository_ident(ident: &str) -> bool {
    ident.starts_with("Pg") && model_from_repository_ident(ident).is_some()
}

/// Whether an ident looks like a model type: `Post`, `LineItem`.
fn is_model_ident(ident: &str) -> bool {
    ident.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !ident.contains('_')
}

/// What a function was found to read, and whether anything was left unresolved.
#[derive(Default)]
struct DerivedDependencies {
    /// Concrete `Pg*Repository` paths. The model comes from the repository's
    /// own `__AUTUMN_MODEL_NAME`, so an escape-hatch name like
    /// `#[repository(Comment)] trait ModerationRepository` resolves to
    /// `Comment` rather than to the `Moderation` its name suggests.
    repositories: Vec<syn::Path>,
    /// Model types named outright in the body (`Post::find_all(db)`).
    models: BTreeSet<String>,
    /// A repository reached only through its TRAIT (`&impl PostRepository`),
    /// whose model this analysis cannot resolve: the constant lives on the
    /// concrete struct. Guessing from the trait's name is what produced a
    /// wrong dependency — and a wrong dependency makes a real violation audit
    /// clean — so the read is recorded `Undetermined` instead.
    unresolved_repository: bool,
}

/// Walks a function looking for evidence of which models it reads.
#[derive(Default)]
struct DependencyVisitor {
    found: DerivedDependencies,
    /// Paths already recorded, so `&PgPostRepository` mentioned twice does not
    /// register the model twice.
    seen_repositories: BTreeSet<String>,
}

// `VisitMut` rather than `Visit`: the workspace enables syn's `visit-mut`
// feature (for `#[query_budget]`'s rewriting pass) but not `visit`, and a
// read-only analysis over a throwaway clone costs less than pulling syn's
// second generated traversal module into every macro build.
impl VisitMut for DependencyVisitor {
    fn visit_path_mut(&mut self, path: &mut syn::Path) {
        // Any repository type mentioned anywhere — a parameter type, a turbofish,
        // an `impl PostRepository` bound — points at the model it was generated
        // for. Which model that is comes from the repository itself, not from
        // its name: only the concrete `Pg*` struct carries the constant that
        // says so.
        for (i, segment) in path.segments.iter().enumerate() {
            let ident = segment.ident.to_string();
            if model_from_repository_ident(&ident).is_none() {
                continue;
            }
            if is_concrete_repository_ident(&ident) {
                let mut prefix = path.clone();
                // Keep only the segments up to the repository itself, so
                // `PgPostRepository::find_all` yields the type, not the call.
                prefix.segments = path.segments.iter().take(i + 1).cloned().collect();
                // A type path spells its generics without `::`.
                if let Some(last) = prefix.segments.last_mut()
                    && let syn::PathArguments::AngleBracketed(args) = &mut last.arguments
                {
                    args.colon2_token = None;
                }
                if self.seen_repositories.insert(quote!(#prefix).to_string()) {
                    self.found.repositories.push(prefix);
                }
            } else {
                self.found.unresolved_repository = true;
            }
            break;
        }
        syn::visit_mut::visit_path_mut(self, path);
    }

    fn visit_expr_call_mut(&mut self, call: &mut syn::ExprCall) {
        // `Post::find_all(db)` — a reading associated function on a model type.
        if let Expr::Path(p) = &*call.func {
            let segments = &p.path.segments;
            if segments.len() >= 2 {
                let verb = segments[segments.len() - 1].ident.to_string();
                let owner = segments[segments.len() - 2].ident.to_string();
                if MODEL_READ_VERBS.contains(&verb.as_str())
                    && is_model_ident(&owner)
                    && model_from_repository_ident(&owner).is_none()
                {
                    self.found.models.insert(owner);
                }
            }
        }
        syn::visit_mut::visit_expr_call_mut(self, call);
    }
}

/// Recover the set of models a cached function reads, from its signature and
/// body.
///
/// Conservative in the direction that matters: it only reports what it can
/// actually see. A dependency reached through a helper function this analysis
/// cannot read is **missed**, which is why an underivable function is recorded
/// as `undetermined` rather than as an empty — and therefore trivially
/// coherent — dependency set. Declare `reads(...)` to get the strong claim.
///
/// Returns the repository paths and the directly-named model idents.
fn derive_dependencies(func: &ItemFn) -> DerivedDependencies {
    let mut visitor = DependencyVisitor::default();
    let mut scratch = func.clone();
    visitor.visit_signature_mut(&mut scratch.sig);
    visitor.visit_block_mut(&mut scratch.block);
    visitor.found
}

/// Name of the generated constant carrying a cached read's identity.
fn read_id_const_ident(fn_name: &syn::Ident) -> syn::Ident {
    format_ident!("__AUTUMN_CACHE_READ_ID__{}", fn_name)
}

/// The read's identity, as an expression.
///
/// Spliced verbatim everywhere the identity is needed — the cache key, the
/// namespace registration, the descriptor, the invalidator, and the constant
/// `invalidates(...)` resolves to — rather than having those sites reference
/// the constant by path. In an `impl` block the constant is an ASSOCIATED
/// const, which a bare path cannot reach; and `module_path!()` names the
/// enclosing module either way, so every splice is the same string.
fn read_id_expr(fn_name_str: &str) -> TokenStream {
    quote! { concat!(module_path!(), "::", #fn_name_str) }
}

/// Name of the generated per-read invalidator.
fn invalidator_ident(fn_name: &syn::Ident) -> syn::Ident {
    format_ident!("__autumn_cache_invalidate__{}", fn_name)
}

/// The cache-coherence artifacts a `#[cached]` expansion carries, split by
/// where each may legally be placed.
struct CoherenceItems {
    /// The identity constant and the invalidator. Both are valid *associated*
    /// items, so they sit beside the function wherever it lives.
    items: TokenStream,
    /// The `inventory` registration. It expands to an anonymous `const _`,
    /// which an `impl` block cannot hold — so it goes inside the function body,
    /// where a block-scoped anonymous const is legal and the linker section is
    /// registered exactly the same. Without this split, adding coherence
    /// registration would have silently broken every `#[cached]` associated
    /// function in existing code.
    registration: TokenStream,
}

/// The cache-coherence registration items emitted alongside the wrapped
/// function: the identity constant `#[invalidates(...)]` resolves to, the
/// callable invalidator, and the `inventory` descriptor the manifest is built
/// from.
fn generate_coherence_items(
    attrs: &CachedAttrs,
    input_fn: &ItemFn,
    fn_name: &syn::Ident,
    fn_name_str: &str,
) -> CoherenceItems {
    let vis = &input_fn.vis;
    let id_const = read_id_const_ident(fn_name);
    let id_expr = read_id_expr(fn_name_str);
    let invalidator = invalidator_ident(fn_name);

    // Declared beats derived: an explicit `reads(...)` is the strongest claim
    // the manifest can carry, so it is never diluted by the heuristic.
    let (read_exprs, provenance) = if attrs.reads.is_empty() {
        let derived = derive_dependencies(input_fn);

        // A repository states its own model. `#[repository(Comment)] trait
        // ModerationRepository` is supported, and inferring `Moderation` from
        // the type's NAME would register a model that does not exist — a
        // `Comment` write would then intersect nothing and the audit would
        // report clean on a read that really can go stale (#1716/#2357). The
        // mutation side already resolves this through the same constant; this
        // is the read side matching it, so rustc decides the model.
        let mut exprs: Vec<TokenStream> = derived
            .repositories
            .iter()
            .map(|path| quote! { <#path>::__AUTUMN_MODEL_NAME })
            .collect();
        // A model named outright recovers a bare ident (`Post`), not a
        // nameable type: the model type is often not in scope at the cached
        // function at all. The checker matches on the last path segment for
        // exactly this reason.
        exprs.extend(derived.models.iter().map(|m| quote! { || #m }));

        // Honest over useful: a repository reached only through its trait has
        // no constant to resolve, so the dependency set is *unknown*, not
        // empty and not guessable. `Undetermined` is reported and never gates,
        // which is the right answer for something this analysis cannot see.
        let provenance = if derived.unresolved_repository || exprs.is_empty() {
            quote! { Undetermined }
        } else {
            quote! { Derived }
        };
        (exprs, provenance)
    } else {
        let exprs: Vec<TokenStream> = attrs
            .reads
            .iter()
            .map(|path| quote! { || ::core::any::type_name::<#path>() })
            .collect();
        (exprs, quote! { Declared })
    };

    let acknowledged = attrs.acknowledge_stale.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |reason| quote! { ::core::option::Option::Some(#reason) },
    );

    let registration = quote! {
        ::autumn_web::reexports::inventory::submit! {
            ::autumn_web::cache::coherence::CachedReadDescriptor {
                id: #id_expr,
                kind: ::autumn_web::cache::coherence::ReadKind::Cached,
                reads: &[#(#read_exprs),*],
                provenance:
                    ::autumn_web::cache::coherence::DependencyProvenance::#provenance,
                acknowledged_stale: #acknowledged,
                location: concat!(file!(), ":", line!()),
            }
        }
    };

    let items = quote! {
        /// Cache-coherence identity of the adjacent `#[cached]` function: the
        /// namespace every one of its cache keys is prefixed with.
        ///
        /// `#[repository(..., invalidates(path::to::this_fn))]` resolves to this
        /// constant, so rustc — not a string table — proves an invalidation edge
        /// names a real cached read.
        #[doc(hidden)]
        #[allow(non_upper_case_globals, dead_code)]
        #vis const #id_const: &'static ::core::primitive::str = #id_expr;

        /// Drop every entry of the adjacent `#[cached]` function.
        ///
        /// Returns whether the invalidation was complete — see
        /// [`autumn_web::cache::coherence::invalidate_namespace`].
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        #vis fn #invalidator() -> ::core::primitive::bool {
            ::autumn_web::cache::coherence::invalidate_namespace(#id_expr)
        }
    };

    CoherenceItems {
        items,
        registration,
    }
}

/// The name a parameter is referred to by, for `key(...)` matching and for the
/// diagnostic that lists the declared parameters.
///
/// `mut tenant_id: String` is the `tenant_id` parameter — the binding mode is
/// not part of its name, and matching on the rendered pattern would make
/// `key(tenant_id)` fail to find it.
fn param_name(pat: &syn::Pat) -> String {
    match pat {
        syn::Pat::Ident(ident) if ident.subpat.is_none() => ident.ident.to_string(),
        other => quote!(#other).to_string(),
    }
}

/// Build the tuple expression the cache key is hashed from.
///
/// Without `key(...)` that is every parameter, exactly as before. With it, only
/// the named ones — which is what lets a cached read take the repository handle
/// it reads through, since the handle is `Clone` but not `Hash` and was never
/// part of the value's identity.
///
/// Each parameter enters the tuple as the expression naming its *value*. For
/// the ordinary `a: i64` that must be the bare ident, because a `mut a: i64`
/// binding would otherwise splice as `mut a.clone()`. A destructuring pattern
/// like `(a, b): (i64, i64)` has no single ident, so the pattern itself is
/// spliced, which reconstructs the tuple and clones correctly.
fn key_tuple(attrs: &CachedAttrs, param_names: &[&syn::Pat]) -> syn::Result<TokenStream> {
    let key_params: Vec<&syn::Pat> = if attrs.key.is_empty() {
        param_names.to_vec()
    } else {
        let declared: Vec<String> = param_names.iter().map(|pat| param_name(pat)).collect();
        // Rendered once rather than per comparison: both the validation below
        // and the filter after it walk this list for every parameter.
        let wanted: Vec<String> = attrs.key.iter().map(ToString::to_string).collect();
        if let Some((unknown, _)) = attrs
            .key
            .iter()
            .zip(&wanted)
            .find(|(_, name)| !declared.contains(*name))
        {
            return Err(syn::Error::new_spanned(
                unknown,
                format!(
                    "`key({unknown})` names no parameter of this function; \
                     declared parameters are: {}",
                    declared.join(", ")
                ),
            ));
        }
        param_names
            .iter()
            .copied()
            .filter(|pat| wanted.contains(&param_name(pat)))
            .collect()
    };

    if key_params.is_empty() {
        return Ok(quote! { &() });
    }
    let key_exprs: Vec<TokenStream> = key_params
        .iter()
        .map(|pat| match pat {
            syn::Pat::Ident(ident) if ident.subpat.is_none() => {
                let name = &ident.ident;
                quote! { #name }
            }
            other => quote! { #other },
        })
        .collect();
    Ok(quote! { &(#(#key_exprs.clone(),)*) })
}

/// Generate the cache wrapper body for a single function.
fn generate_cache_body(
    attrs: &CachedAttrs,
    fn_name: &syn::Ident,
    fn_block: &syn::Block,
    is_async: bool,
    key_args: &TokenStream,
    ret_type: &TokenStream,
    value_type: &TokenStream,
) -> TokenStream {
    let ttl_expr = attrs.ttl.as_ref().map_or_else(
        || quote! { None },
        |ttl| {
            let ttl_str = ttl.clone();
            quote! {
                Some(
                    ::autumn_web::task::parse_duration(#ttl_str)
                        .expect(concat!("invalid duration in #[cached(ttl = \"", #ttl_str, "\")]"))
                )
            }
        },
    );

    let max_expr = attrs.max.map_or_else(
        || quote! { 10_000 },
        |max| {
            let max_lit = LitInt::new(&max.to_string(), proc_macro2::Span::call_site());
            quote! { #max_lit }
        },
    );

    let compute = if is_async {
        quote! { (|| async move #fn_block)().await }
    } else {
        quote! { (|| #fn_block)() }
    };

    let id_expr = read_id_expr(&fn_name.to_string());
    let cache_init = quote! {
        // `Arc` rather than a bare `MokaCache` (#1716): the store stays a
        // per-function static, but a clone is handed to the coherence registry
        // on first use so the generated invalidator can reach — and clear — a
        // store that lives inside this function body.
        static __AUTUMN_CACHE: ::std::sync::OnceLock<(
            ::std::sync::Arc<::autumn_web::cache::MokaCache>,
            ::std::sync::Arc<::std::sync::atomic::AtomicU64>,
        )> = ::std::sync::OnceLock::new();
        // Evaluate TTL once; Duration is Copy so it can be used for both
        // the Moka initializer and the Redis insert call.
        let __autumn_ttl: ::std::option::Option<::std::time::Duration> = #ttl_expr;
        let (__autumn_moka, __autumn_epoch_cell) = __AUTUMN_CACHE.get_or_init(|| {
            let __autumn_store = ::std::sync::Arc::new(
                ::autumn_web::cache::MokaCache::new(#max_expr, __autumn_ttl)
            );
            let __autumn_epoch = ::autumn_web::cache::coherence::register_namespace_store(
                #id_expr,
                ::std::clone::Clone::clone(&__autumn_store) as ::std::sync::Arc<dyn ::autumn_web::cache::Cache>,
            );
            (__autumn_store, __autumn_epoch)
        });
        // Sampled BEFORE the lookup, so it covers the whole miss-and-fill
        // window: an invalidation landing while this call computes bumps the
        // epoch, and the insert below is skipped rather than resurrecting the
        // value that was just dropped.
        let __autumn_epoch_at_entry =
            ::std::sync::atomic::AtomicU64::load(
                __autumn_epoch_cell,
                ::std::sync::atomic::Ordering::Acquire,
            );
        // Use the process-level shared backend when registered, otherwise fall
        // back to the per-function Moka store so zero-config local dev still works.
        let __autumn_global = ::autumn_web::cache::global_cache();
        let __autumn_cache: &dyn ::autumn_web::cache::Cache =
            __autumn_global
                .as_deref()
                .unwrap_or(&**__autumn_moka as &dyn ::autumn_web::cache::Cache);
        let __autumn_key = ::autumn_web::cache::make_cache_key(#id_expr, #key_args);
    };

    if attrs.result {
        quote! {
            #cache_init
            if let Some(__autumn_cached) = ::autumn_web::cache::get_cached::<#value_type>(__autumn_cache, &__autumn_key) {
                return <#ret_type as ::autumn_web::cache::CacheableResult>::from_ok(__autumn_cached);
            }
            let __autumn_result = #compute;
            match <#ret_type as ::autumn_web::cache::CacheableResult>::into_result(__autumn_result) {
                Ok(__autumn_val) => {
                    // The epoch check and the insert are one indivisible step:
                    // checking first and inserting after leaves a window where
                    // an invalidation bumps and clears in between, and this
                    // pre-invalidation value lands after the clear.
                    ::autumn_web::cache::coherence::with_fill_fence(
                        __autumn_epoch_cell, __autumn_epoch_at_entry,
                        || ::autumn_web::cache::insert_cached::<#value_type>(__autumn_cache, &__autumn_key, __autumn_val.clone(), __autumn_ttl),
                    );
                    <#ret_type as ::autumn_web::cache::CacheableResult>::from_ok(__autumn_val)
                }
                Err(__autumn_err) => Err(__autumn_err),
            }
        }
    } else {
        quote! {
            #cache_init
            if let Some(__autumn_cached) = ::autumn_web::cache::get_cached::<#value_type>(__autumn_cache, &__autumn_key) {
                return __autumn_cached;
            }
            let __autumn_result = #compute;
            // The epoch check and the insert are one indivisible step: checking
            // first and inserting after leaves a window where an invalidation
            // bumps and clears in between, and this pre-invalidation value
            // lands after the clear.
            ::autumn_web::cache::coherence::with_fill_fence(
                __autumn_epoch_cell, __autumn_epoch_at_entry,
                || ::autumn_web::cache::insert_cached::<#value_type>(__autumn_cache, &__autumn_key, __autumn_result.clone(), __autumn_ttl),
            );
            __autumn_result
        }
    }
}

pub fn cached_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_cached_args(attr) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error(),
    };

    let input_fn: ItemFn = match syn::parse2(item) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error(),
    };

    let vis = &input_fn.vis;
    let sig = &input_fn.sig;
    let fn_name = &sig.ident;
    let fn_name_str = fn_name.to_string();
    let fn_attrs = &input_fn.attrs;
    let fn_block = &input_fn.block;
    let is_async = sig.asyncness.is_some();

    // Collect function parameters for cache key construction.
    let mut param_names = Vec::new();
    for arg in &sig.inputs {
        match arg {
            syn::FnArg::Receiver(_) => {
                return syn::Error::new_spanned(
                    arg,
                    "#[cached] does not support methods with `self`",
                )
                .to_compile_error();
            }
            syn::FnArg::Typed(pat_type) => {
                param_names.push(&*pat_type.pat);
            }
        }
    }

    let key_args = match key_tuple(&attrs, &param_names) {
        Ok(args) => args,
        Err(err) => return err.to_compile_error(),
    };

    let ret_type = match &sig.output {
        syn::ReturnType::Default => quote! { () },
        syn::ReturnType::Type(_, ty) => quote! { #ty },
    };

    let value_type = if attrs.result {
        quote! { <#ret_type as ::autumn_web::cache::CacheableResult>::Ok }
    } else {
        ret_type.clone()
    };

    let body = generate_cache_body(
        &attrs,
        fn_name,
        fn_block,
        is_async,
        &key_args,
        &ret_type,
        &value_type,
    );

    // #1716: publish what this read is derived from, so the build can prove no
    // repository write strands it.
    let CoherenceItems {
        items: coherence_items,
        registration,
    } = generate_coherence_items(&attrs, &input_fn, fn_name, &fn_name_str);

    quote! {
        #(#fn_attrs)*
        #vis #sig {
            #registration
            #body
        }

        #coherence_items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_attrs() {
        let attrs = parse_cached_args(TokenStream::new()).unwrap();
        assert!(attrs.ttl.is_none());
        assert!(attrs.max.is_none());
        assert!(!attrs.result);
    }

    #[test]
    fn parse_ttl_only() {
        let tokens: TokenStream = quote! { ttl = "5m" };
        let attrs = parse_cached_args(tokens).unwrap();
        assert_eq!(attrs.ttl.as_deref(), Some("5m"));
        assert!(attrs.max.is_none());
        assert!(!attrs.result);
    }

    #[test]
    fn parse_all_attrs() {
        let tokens: TokenStream = quote! { ttl = "1h", max = 100, result };
        let attrs = parse_cached_args(tokens).unwrap();
        assert_eq!(attrs.ttl.as_deref(), Some("1h"));
        assert_eq!(attrs.max, Some(100));
        assert!(attrs.result);
    }

    #[test]
    fn parse_max_as_integer() {
        let tokens: TokenStream = quote! { max = 500 };
        let attrs = parse_cached_args(tokens).unwrap();
        assert_eq!(attrs.max, Some(500));
    }

    #[test]
    fn parse_result_flag_only() {
        let tokens: TokenStream = quote! { result };
        let attrs = parse_cached_args(tokens).unwrap();
        assert!(attrs.result);
    }

    #[test]
    fn parse_unknown_attr_errors() {
        let tokens: TokenStream = quote! { foo = "bar" };
        assert!(parse_cached_args(tokens).is_err());
    }

    #[test]
    fn generated_output_uses_moka() {
        let attr: TokenStream = quote! { ttl = "5m" };
        let item: TokenStream = quote! {
            async fn get_user(id: i64) -> String {
                format!("user-{id}")
            }
        };
        let output = cached_macro(attr, item);
        let output_str = output.to_string();
        assert!(
            output_str.contains("MokaCache"),
            "should reference MokaCache"
        );
        assert!(
            output_str.contains("make_cache_key"),
            "should use make_cache_key"
        );
        assert!(
            output_str.contains("OnceLock"),
            "should use OnceLock for static"
        );
        assert!(
            output_str.contains("get_cached"),
            "should use get_cached for serde-aware retrieval"
        );
        assert!(
            output_str.contains("insert_cached"),
            "should use insert_cached for serde-aware storage"
        );
    }

    #[test]
    fn generated_output_result_mode() {
        let attr: TokenStream = quote! { result };
        let item: TokenStream = quote! {
            async fn get_user(id: i64) -> Result<String, Error> {
                Ok(format!("user-{id}"))
            }
        };
        let output = cached_macro(attr, item);
        let output_str = output.to_string();
        assert!(
            output_str.contains("CacheableResult"),
            "result mode should use CacheableResult trait"
        );
    }

    #[test]
    fn no_args_function() {
        let attr: TokenStream = quote! {};
        let item: TokenStream = quote! {
            async fn get_config() -> Vec<String> {
                vec!["a".into()]
            }
        };
        let output = cached_macro(attr, item);
        let output_str = output.to_string();
        assert!(
            output_str.contains("MokaCache"),
            "should still generate cache"
        );
    }

    #[test]
    fn self_receiver_errors() {
        let attr: TokenStream = quote! {};
        let item: TokenStream = quote! {
            async fn get_thing(&self) -> String {
                "hi".into()
            }
        };
        let output = cached_macro(attr, item);
        let output_str = output.to_string();
        assert!(
            output_str.contains("compile_error"),
            "should produce compile error for self"
        );
    }

    #[test]
    fn default_max_capacity() {
        let attr: TokenStream = quote! {};
        let item: TokenStream = quote! {
            fn compute(x: i32) -> i32 { x }
        };
        let output = cached_macro(attr, item);
        let output_str = output.to_string();
        assert!(
            output_str.contains("10_000"),
            "default max should be 10_000"
        );
    }

    #[test]
    fn parse_key_selects_the_key_parameters() {
        let attrs = parse_cached_args(quote! { key(tenant_id, page) }).unwrap();
        let names: Vec<String> = attrs.key.iter().map(ToString::to_string).collect();
        assert_eq!(names, vec!["tenant_id", "page"]);
    }

    #[test]
    fn parse_empty_key_is_an_error() {
        assert!(parse_cached_args(quote! { key() }).is_err());
    }

    #[test]
    fn key_narrows_the_cache_key_to_the_named_parameters() {
        let out = cached_macro(
            quote! { key(tenant_id), reads(Project) },
            quote! {
                async fn project_count(tenant_id: String, repo: &PgProjectRepository) -> i64 { 0 }
            },
        )
        .to_string();
        assert!(
            out.contains("& (tenant_id . clone () ,)"),
            "only the named parameter may enter the key: {out}"
        );
        assert!(
            !out.contains("repo . clone ()"),
            "a non-Hash handle must never be hashed into the key: {out}"
        );
    }

    #[test]
    fn key_naming_an_unknown_parameter_is_a_compile_error() {
        let out = cached_macro(
            quote! { key(nope) },
            quote! { async fn project_count(tenant_id: String) -> i64 { 0 } },
        )
        .to_string();
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("names no parameter"), "{out}");
    }

    #[test]
    fn without_key_every_parameter_still_enters_the_cache_key() {
        let out = cached_macro(
            TokenStream::new(),
            quote! { async fn f(a: i64, b: i64) -> i64 { a + b } },
        )
        .to_string();
        assert!(out.contains("& (a . clone () , b . clone () ,)"), "{out}");
    }

    #[test]
    fn key_matches_a_mut_binding_by_its_name() {
        // `mut tenant_id: String` is still the `tenant_id` parameter, and the
        // key expression must be the bare ident — `mut tenant_id.clone()` is
        // not an expression.
        let out = cached_macro(
            quote! { key(tenant_id) },
            quote! { async fn f(mut tenant_id: String, other: i64) -> i64 { 0 } },
        )
        .to_string();
        assert!(!out.contains("compile_error"), "{out}");
        assert!(out.contains("& (tenant_id . clone () ,)"), "{out}");
    }

    #[test]
    fn a_destructuring_parameter_still_enters_the_key_as_a_pattern() {
        let out = cached_macro(
            TokenStream::new(),
            quote! { fn f((a, b): (i64, i64)) -> i64 { a + b } },
        )
        .to_string();
        assert!(out.contains("(a , b) . clone ()"), "{out}");
    }

    #[test]
    fn the_expansion_fences_a_fill_against_a_concurrent_invalidation() {
        // The epoch is sampled before the lookup and re-checked before the
        // insert, so a miss that computes across an invalidation cannot
        // resurrect the value that was just dropped.
        let out =
            cached_macro(TokenStream::new(), quote! { async fn recent() -> u8 { 0 } }).to_string();
        assert!(out.contains("__autumn_epoch_at_entry"), "{out}");
        assert!(
            out.contains("with_fill_fence (__autumn_epoch_cell , __autumn_epoch_at_entry ,"),
            "the insert must be gated on the fence: {out}"
        );
        // Sampled before the hit path, so it covers the whole miss-and-fill
        // window rather than only the compute.
        let sample = out.find("__autumn_epoch_at_entry =").expect("sampled");
        let lookup = out.find("get_cached").expect("lookup");
        assert!(
            sample < lookup,
            "the epoch must be sampled before the lookup"
        );
    }

    #[test]
    fn result_mode_fences_its_insert_too() {
        let out = cached_macro(
            quote! { result },
            quote! { async fn recent() -> Result<u8, Error> { Ok(0) } },
        )
        .to_string();
        assert!(out.contains("with_fill_fence"), "{out}");
    }

    /// The check and the insert must be *one* step, not two adjacent ones.
    ///
    /// A bare `if fill_is_still_current(..) { insert_cached(..) }` reads as
    /// fenced and is not: an invalidation can bump the epoch and clear the
    /// namespace between the `true` and the insert, and the pre-invalidation
    /// value then lands after the clear and sits there until its TTL — or
    /// forever, when the read has none. Pin the shape that closes it: every
    /// `insert_cached` in the expansion is an argument to `with_fill_fence`,
    /// and no `if`-gated `fill_is_still_current` guards one.
    #[test]
    fn the_insert_happens_inside_the_fence_not_merely_after_a_check() {
        for (attr, item) in [
            (TokenStream::new(), quote! { async fn recent() -> u8 { 0 } }),
            (
                quote! { result },
                quote! { async fn recent() -> Result<u8, Error> { Ok(0) } },
            ),
        ] {
            let out = cached_macro(attr, item).to_string();
            assert!(
                !out.contains("if :: autumn_web :: cache :: coherence :: fill_is_still_current"),
                "the check must not merely precede the insert: {out}"
            );
            let fence = out.find("with_fill_fence").expect("fenced");
            let insert = out.find("insert_cached").expect("inserts");
            assert!(
                fence < insert,
                "insert_cached must sit inside the with_fill_fence call: {out}"
            );
            assert_eq!(
                out.matches("insert_cached").count(),
                out.matches("with_fill_fence").count(),
                "every insert must be fenced: {out}"
            );
        }
    }

    // ── #1716: cache-coherence dependency declaration ────────────────

    #[test]
    fn parse_reads_declares_the_dependency_set() {
        let tokens: TokenStream = quote! { reads(Post, Comment) };
        let attrs = parse_cached_args(tokens).unwrap();
        let names: Vec<String> = attrs
            .reads
            .iter()
            .map(|p| p.segments.last().unwrap().ident.to_string())
            .collect();
        assert_eq!(names, vec!["Post", "Comment"]);
    }

    #[test]
    fn parse_reads_accepts_qualified_paths() {
        let tokens: TokenStream = quote! { ttl = "5m", reads(crate::models::Post) };
        let attrs = parse_cached_args(tokens).unwrap();
        assert_eq!(attrs.reads.len(), 1);
        assert_eq!(attrs.ttl.as_deref(), Some("5m"));
    }

    #[test]
    fn parse_empty_reads_is_an_error() {
        let tokens: TokenStream = quote! { reads() };
        assert!(parse_cached_args(tokens).is_err());
    }

    #[test]
    fn parse_acknowledge_stale_requires_a_nonempty_reason() {
        let ok: TokenStream = quote! { acknowledge_stale = "ttl is 2s" };
        assert_eq!(
            parse_cached_args(ok).unwrap().acknowledge_stale.as_deref(),
            Some("ttl is 2s")
        );
        let empty: TokenStream = quote! { acknowledge_stale = "" };
        assert!(parse_cached_args(empty).is_err());
        let blank: TokenStream = quote! { acknowledge_stale = "   " };
        assert!(parse_cached_args(blank).is_err());
    }

    /// The repository paths a derivation found, rendered for comparison.
    fn derived_repositories(f: &ItemFn) -> Vec<String> {
        derive_dependencies(f)
            .repositories
            .iter()
            .map(|p| quote!(#p).to_string())
            .collect()
    }

    /// The directly-named models a derivation found.
    fn derived_models(f: &ItemFn) -> Vec<String> {
        derive_dependencies(f).models.into_iter().collect()
    }

    #[test]
    fn derives_dependencies_from_repository_types_in_the_signature() {
        let f: ItemFn = syn::parse_quote! {
            async fn recent_posts(repo: &PgPostRepository) -> Vec<String> {
                repo.find_all().await.unwrap_or_default()
            }
        };
        // The repository is recorded as a PATH, not as a guessed model name:
        // the model comes from the repository's own `__AUTUMN_MODEL_NAME`.
        assert_eq!(derived_repositories(&f), vec!["PgPostRepository"]);
        assert!(derived_models(&f).is_empty());
        assert!(!derive_dependencies(&f).unresolved_repository);
    }

    /// The bug this shape used to have: the model was guessed from the type's
    /// NAME, and `#[repository(Comment)] trait ModerationRepository` is
    /// supported, so the guess said `Moderation` — a model that does not
    /// exist. A `Comment` write then intersected nothing and the audit
    /// reported clean on a read that really could go stale (#2357).
    #[test]
    fn a_repository_whose_name_differs_from_its_model_is_not_guessed_from_the_name() {
        let f: ItemFn = syn::parse_quote! {
            async fn recent_comments(repo: &PgModerationRepository) -> Vec<String> {
                repo.find_all().await.unwrap_or_default()
            }
        };
        let derived = derive_dependencies(&f);
        assert_eq!(derived_repositories(&f), vec!["PgModerationRepository"]);
        assert!(
            !derived.models.contains("Moderation"),
            "the name must not become a dependency — the repository states its own model"
        );

        // And the expansion resolves it through the constant, so rustc — not
        // this heuristic — decides which model the read depends on.
        let out = cached_macro(
            TokenStream::new(),
            quote! {
                async fn recent_comments(repo: &PgModerationRepository) -> Vec<String> {
                    repo.find_all().await.unwrap_or_default()
                }
            },
        )
        .to_string();
        assert!(
            out.contains("< PgModerationRepository > :: __AUTUMN_MODEL_NAME"),
            "{out}"
        );
        assert!(!out.contains("|| \"Moderation\""), "{out}");
    }

    /// The trait carries no `__AUTUMN_MODEL_NAME` — only the concrete `Pg*`
    /// struct does — so the model is genuinely unknown here. Unknown must be
    /// `Undetermined` (reported, never gating), not a guess: a guess that is
    /// wrong turns a real violation into a clean audit.
    #[test]
    fn a_repository_reached_only_through_its_trait_is_undetermined() {
        let f: ItemFn = syn::parse_quote! {
            async fn feed(repo: &impl CommentRepository) -> Vec<String> { Vec::new() }
        };
        let derived = derive_dependencies(&f);
        assert!(derived.repositories.is_empty());
        assert!(derived.models.is_empty());
        assert!(derived.unresolved_repository);

        let out = cached_macro(
            TokenStream::new(),
            quote! {
                async fn feed(repo: &impl CommentRepository) -> Vec<String> { Vec::new() }
            },
        )
        .to_string();
        assert!(
            out.contains("DependencyProvenance :: Undetermined"),
            "{out}"
        );
        assert!(!out.contains("|| \"Comment\""), "{out}");
    }

    #[test]
    fn derives_dependencies_from_model_finder_calls_in_the_body() {
        let f: ItemFn = syn::parse_quote! {
            async fn recent(db: &mut Db) -> Vec<String> {
                let rows = Post::find_all(db).await;
                Vec::new()
            }
        };
        assert_eq!(derived_models(&f), vec!["Post".to_string()]);
        assert!(derived_repositories(&f).is_empty());
    }

    #[test]
    fn derivation_is_sorted_and_deduplicated() {
        let f: ItemFn = syn::parse_quote! {
            async fn feed(posts: &PgPostRepository, comments: &PgCommentRepository) -> u8 {
                let _ = Post::find_all(posts);
                0
            }
        };
        assert_eq!(
            derived_repositories(&f),
            vec!["PgPostRepository", "PgCommentRepository"]
        );
        assert_eq!(derived_models(&f), vec!["Post".to_string()]);
    }

    /// One repository named twice must not register twice.
    #[test]
    fn a_repository_mentioned_twice_is_recorded_once() {
        let f: ItemFn = syn::parse_quote! {
            async fn feed(a: &PgPostRepository, b: &PgPostRepository) -> u8 { 0 }
        };
        assert_eq!(derived_repositories(&f), vec!["PgPostRepository"]);
    }

    /// A qualified path must yield the repository TYPE, not the call.
    #[test]
    fn a_qualified_repository_path_keeps_only_the_type_segments() {
        let f: ItemFn = syn::parse_quote! {
            async fn feed(db: &mut Db) -> u8 {
                let _ = crate::repositories::PgPostRepository::find_all(db);
                0
            }
        };
        assert_eq!(
            derived_repositories(&f),
            vec!["crate :: repositories :: PgPostRepository"]
        );
    }

    #[test]
    fn derivation_finds_nothing_in_a_pure_function() {
        let f: ItemFn = syn::parse_quote! {
            fn double(x: i32) -> i32 { x * 2 }
        };
        let derived = derive_dependencies(&f);
        assert!(derived.repositories.is_empty());
        assert!(derived.models.is_empty());
        assert!(!derived.unresolved_repository);
    }

    #[test]
    fn derivation_ignores_non_repository_camel_case_types() {
        let f: ItemFn = syn::parse_quote! {
            fn build(cfg: &HashMap<String, String>) -> String { String::new() }
        };
        let derived = derive_dependencies(&f);
        assert!(derived.repositories.is_empty());
        assert!(derived.models.is_empty());
        assert!(!derived.unresolved_repository);
    }

    #[test]
    fn generated_output_registers_a_declared_cached_read() {
        let attr: TokenStream = quote! { reads(Post) };
        let item: TokenStream = quote! {
            async fn recent_posts() -> Vec<String> { Vec::new() }
        };
        let out = cached_macro(attr, item).to_string();
        assert!(out.contains("CachedReadDescriptor"), "{out}");
        assert!(out.contains("Declared"), "{out}");
        assert!(out.contains("type_name"), "{out}");
        assert!(
            out.contains("__AUTUMN_CACHE_READ_ID__recent_posts"),
            "must emit the compiler-checkable id constant: {out}"
        );
        assert!(
            out.contains("__autumn_cache_invalidate__recent_posts"),
            "must emit the callable invalidator: {out}"
        );
    }

    #[test]
    fn generated_output_marks_underivable_reads_undetermined() {
        let out = cached_macro(
            TokenStream::new(),
            quote! { fn double(x: i32) -> i32 { x * 2 } },
        )
        .to_string();
        assert!(out.contains("Undetermined"), "{out}");
    }

    #[test]
    fn generated_output_marks_body_derived_reads_derived() {
        let out = cached_macro(
            TokenStream::new(),
            quote! {
                async fn recent(repo: &PgPostRepository) -> u8 { 0 }
            },
        )
        .to_string();
        assert!(out.contains("Derived"), "{out}");
        // The model is the repository's own constant, not the literal "Post"
        // guessed from its name (#2357).
        assert!(
            out.contains("< PgPostRepository > :: __AUTUMN_MODEL_NAME"),
            "{out}"
        );
    }

    #[test]
    fn generated_output_carries_the_acknowledged_stale_reason() {
        let out = cached_macro(
            quote! { reads(Post), acknowledge_stale = "5s ttl is tight enough" },
            quote! { async fn recent() -> u8 { 0 } },
        )
        .to_string();
        assert!(out.contains("5s ttl is tight enough"), "{out}");
    }

    #[test]
    fn every_use_of_the_identity_is_the_same_expression() {
        let out =
            cached_macro(TokenStream::new(), quote! { async fn recent() -> u8 { 0 } }).to_string();
        // The identity is spliced, not referenced through the constant: in an
        // `impl` block that constant is an ASSOCIATED const and a bare path to
        // it does not resolve. Splicing keeps the runtime key prefix, the
        // registered descriptor, the invalidator's namespace and the constant
        // `invalidates(...)` resolves to provably the same string.
        let id = "concat ! (module_path ! () , \"::\" , \"recent\")";
        assert_eq!(
            out.matches(id).count(),
            5,
            "key, namespace registration, descriptor, id constant, invalidator: {out}"
        );
        assert!(
            out.contains(&format!("make_cache_key ({id}")),
            "the cache key must be prefixed with the identity: {out}"
        );
        assert!(out.contains(&format!("id : {id}")), "{out}");
        assert!(
            out.contains(&format!("invalidate_namespace ({id})")),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "const __AUTUMN_CACHE_READ_ID__recent : & 'static :: core :: primitive :: str = {id}"
            )),
            "{out}"
        );
    }

    #[test]
    fn the_expansion_survives_a_shadowed_str_or_bool() {
        // The generated const and invalidator name primitives, so a user type
        // called `str` or `bool` in scope must not capture them.
        let out =
            cached_macro(TokenStream::new(), quote! { async fn recent() -> u8 { 0 } }).to_string();
        assert!(out.contains(":: core :: primitive :: str"), "{out}");
        assert!(out.contains(":: core :: primitive :: bool"), "{out}");
    }
}
