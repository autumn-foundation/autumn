//! Repository proc macros for the Autumn web framework.
//!
//! This crate provides the `#[repository]` attribute macro. It is split
//! out of `autumn-macros` so a build that never touches the database
//! layer never compiles this codegen.
//!
//! Users should not depend on this crate directly — use `autumn-web`
//! instead, which re-exports everything behind its `db` feature.

mod api;
mod repository;
mod retention;

use proc_macro::TokenStream;

/// Derive a repository with CRUD operations and derived queries.
///
/// Generates a `PgXxxRepository` struct implementing the annotated trait,
/// with auto-generated CRUD methods and query-by-name derived methods.
///
/// # Read replica routing
///
/// When `database.replica_url` is configured, generated read-only methods
/// (`find_by_id`, `find_all`, `count`, `paginate`, `cursor_page`, derived
/// `find_by_*`, search reads) acquire their connection from the replica
/// pool; mutating methods always use the primary. Add `primary_reads` to
/// pin a read-after-write-sensitive repository's reads to the primary, or
/// call the generated `on_primary()` method to pin a single call chain
/// (read-your-writes).
///
/// # Examples
///
/// ```ignore
/// use autumn_web::repository;
///
/// #[repository(Post)]
/// trait PostRepository {
///     fn find_by_published(published: bool) -> Vec<Post>;
/// }
///
/// // Reads pinned to the primary even when a replica is configured.
/// #[repository(LedgerEntry, primary_reads)]
/// trait LedgerEntryRepository {}
///
/// // Cache coherence (#1716): every write below can strand
/// // `views::recent_posts`, so the edge is declared here. The path resolves
/// // to the identity constant `#[cached]` generates beside that function, so
/// // naming anything else does not compile.
/// #[repository(Post, invalidates(crate::views::recent_posts))]
/// trait CoherentPostRepository {
///     // A per-method edge adds to the trait-level ones.
///     #[invalidates(crate::views::by_author)]
///     fn delete_by_author_id(author_id: i64) -> ();
/// }
/// ```
///
/// # Cache coherence (issue #1716)
///
/// Every generated write method publishes which model it mutates, so
/// `autumn cache audit` can fail the build when a write's model appears in a
/// `#[cached]` read's dependency set with no invalidation covering the pair.
/// Discharge the obligation with `invalidates(...)` — on the attribute for
/// every write, or as `#[invalidates(...)]` on one trait method — or opt out
/// with `acknowledge_stale = "reason"`. A repository that declares any edge
/// also gets a generated `invalidate_declared_caches()` for its write paths to
/// call. See `docs/guide/cache-coherence.md`.
#[proc_macro_attribute]
pub fn repository(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) =
        match autumn_macros_support::crate_path::extract_crate_override(attr.into()) {
            Ok(pair) => pair,
            Err(err) => return err.into(),
        };
    let _guard = autumn_macros_support::crate_path::set_target(crate_override.as_deref());
    autumn_macros_support::crate_path::finalize(repository::repository_macro(attr, item.into()))
        .into()
}
