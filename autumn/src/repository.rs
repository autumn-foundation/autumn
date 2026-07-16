//! Repository support types for framework-generated CRUD operations.
//!
//! Generated `#[repository]` read-only methods (`find_by_id`, `find_all`,
//! `count`, `paginate`, `cursor_page`, derived `find_by_*`, full-text-search
//! reads) route to the configured read replica automatically: set
//! `database.replica_url` and the extractor snapshots a [`ReadRoute`] per
//! request via [`crate::AppState::read_pool`]. Mutating methods (`save`,
//! `update`, `delete_by_id`, bulk writes) always run on the primary pool.
//! Pin a read-after-write-sensitive repository to the primary with
//! `#[repository(Model, primary_reads)]`, or pin a single call chain with
//! the generated `on_primary()` method. When no replica is configured, all
//! methods use the primary — nothing changes for single-pool apps.
//!
//! Sharded repositories built from a
//! [`ShardedDb`](crate::sharding::ShardedDb) via the generated `from_shard`
//! constructor get the same treatment **per shard**: reads route to the
//! shard's replica when one is configured and healthy (honoring the shard's
//! `replica_fallback` policy via
//! [`Shard::read_route`](crate::sharding::Shard::read_route)), writes stay on
//! the shard primary, and `primary_reads` / `on_primary()` pin reads back to
//! the shard primary.
//!
//! [`RepositoryError`] surfaces typed errors that arise during repository
//! operations — most notably optimistic-lock conflicts when two replicas
//! write the same row concurrently.

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Where a generated repository routes its read-only methods (`find_by_id`,
/// `find_all`, `count`, `paginate`, `cursor_page`, derived `find_by_*`,
/// full-text-search reads, …).
///
/// The route is snapshotted from [`crate::AppState`] when the repository is
/// extracted, so every read within a request sees one consistent decision.
/// Mutating methods (`save`, `update`, `delete_by_id`, bulk writes) always
/// use the primary pool regardless of this route, as do pessimistic-lock
/// reads (`with_lock`) and reads running on an explicit transaction
/// connection.
#[cfg(feature = "db")]
#[derive(Clone)]
pub enum ReadRoute {
    /// Reads use the primary/write pool: no replica is configured, the
    /// repository was declared with `#[repository(..., primary_reads)]`, or
    /// the caller pinned this instance with `on_primary()`.
    Primary,
    /// Reads use this read-role pool snapshot — the replica when healthy,
    /// or the primary when the replica is unready and the
    /// [`ReplicaFallback::Primary`](crate::config::ReplicaFallback) policy
    /// applies.
    ReadPool(diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>),
    /// A replica is configured but currently unready, and the
    /// [`ReplicaFallback::FailReadiness`](crate::config::ReplicaFallback)
    /// policy forbids falling back to the primary. Generated reads fail
    /// fast with `503 Service Unavailable` instead of silently serving
    /// from the wrong role.
    Unavailable,
}

#[cfg(feature = "db")]
impl ReadRoute {
    /// Snapshot the read-routing decision for one request from the app
    /// state, mirroring [`crate::AppState::read_pool`] semantics.
    #[must_use]
    pub fn from_state(state: &crate::AppState) -> Self {
        if state.replica_pool().is_some() {
            state
                .read_pool()
                .map_or(Self::Unavailable, |pool| Self::ReadPool(pool.clone()))
        } else {
            Self::Primary
        }
    }
}

#[cfg(feature = "db")]
impl std::fmt::Debug for ReadRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Primary => f.write_str("ReadRoute::Primary"),
            Self::ReadPool(pool) => {
                write!(f, "ReadRoute::ReadPool(max={})", pool.status().max_size)
            }
            Self::Unavailable => f.write_str("ReadRoute::Unavailable"),
        }
    }
}

/// Typed errors returned by generated repository methods.
///
/// Distinct from [`crate::AutumnError`] so callers can match on the
/// variant without parsing an HTTP status code.
#[derive(Debug, Clone, Error)]
pub enum RepositoryError {
    /// Two writers raced on the same row.
    ///
    /// Returned by generated `update`/`save` methods when the
    /// `#[lock_version]` field no longer matches the value the client
    /// sent — meaning another replica committed a write in the meantime.
    ///
    /// Map this to `409 Conflict` via [`crate::AutumnError::conflict`].
    #[error(
        "optimistic lock conflict on record {id}: \
         client expected version {expected_version}, \
         row was already modified (actual: {actual_version:?})"
    )]
    Conflict {
        /// Primary key of the contested record.
        id: i64,
        /// The version the client read and expected to still be current.
        expected_version: i64,
        /// The version actually stored when the conflict was detected,
        /// or `None` if the row was deleted between the read and the write.
        actual_version: Option<i64>,
    },
}

/// How a parent's dependent children are handled when the parent is deleted.
///
/// Declared per association on the `#[repository]` macro via
/// `dependent(ChildRepository, fk = "parent_fk", on_delete = <action>)`, and
/// applied inside the parent's `delete_by_id` transaction so a parent delete
/// never leaves orphaned child rows (issue #1369).
///
/// The cascade runs app-side (not via a DB `ON DELETE` constraint) so that
/// soft-delete, lifecycle/commit hooks, and counter caches stay correct — a
/// guarantee raw `ON DELETE CASCADE` cannot provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependentAction {
    /// Delete each child through the child repository's delete path, honoring
    /// the child's soft-delete mode and firing its lifecycle/commit hooks.
    Destroy,
    /// Bulk hard-delete every child in a single `DELETE`, skipping per-row hooks.
    DeleteAll,
    /// Set the child's foreign-key column to `NULL` (the child rows survive,
    /// detached from the deleted parent).
    Nullify,
    /// Refuse to delete the parent while any child exists, surfacing a typed
    /// `409 Conflict` error instead of orphaning or cascading.
    Restrict,
}

/// The boxed, `Send` future a [`RuntimeDependentCascadeFn`] returns.
///
/// It resolves to the deferred `(topic, dom_id)` OOB delete broadcasts a single
/// child association's cascade accumulated (empty for a non-broadcasting child).
pub type RuntimeDependentCascadeFuture<'a> = ::std::pin::Pin<
    ::std::boxed::Box<
        dyn ::std::future::Future<Output = crate::AutumnResult<Vec<(String, String)>>> + Send + 'a,
    >,
>;

/// A type-erased entry point into one child repository's
/// `__autumn_apply_dependent_on_conn` leaf executor (#1738).
///
/// `#[model]` emits one of these per model-declared
/// `#[has_many(Child, dependent = …)]` association, resolving the child
/// repository through the `Pg{Child}Repository` naming convention. Given the
/// parent repository's pool, its live connection/transaction, the parent id,
/// the parent's soft-delete kind, and the two shared cascade-guard sets, it
/// constructs the child repository and applies this one association's cascade,
/// returning any deferred OOB delete broadcasts to publish post-commit.
///
/// The three guard sets serve distinct roles (Codex round-5-B + #1800 case 1):
/// `__path` is the ACTIVE recursion stack (pushed before descending into a node's
/// children, popped once that subtree completes) used only to break
/// self-/mutual-referential cycles; `__deleted` is a monotonic set of every row
/// HANDLED by the cascade — soft OR physical — used by the bulk `delete_many` root
/// dedup / hook double-fire guard to skip a root already processed as another
/// root's descendant (so its `before_delete` never fires twice); `__physical` is
/// the subset of rows PHYSICALLY removed, used only by the diamond traversal
/// revisit-skip so a hard-delete path can still physically remove a row a
/// soft-delete path merely marked `deleted_at` on. Keeping `__deleted` and
/// `__physical` separate is what lets the bulk root skip stay complete for
/// soft-delete graphs while the diamond hard path is still free to run; keeping
/// `__path` separate lets a batch process a descendant root before its ancestor
/// without the ancestor's cascade skipping a still-referenced intermediate
/// (immediate-FK failure).
///
/// All references share one lifetime so the returned future can borrow the
/// connection and all three guard sets for exactly as long as it is awaited.
pub type RuntimeDependentCascadeFn = for<'a> fn(
    &'a ::diesel_async::pooled_connection::deadpool::Pool<::diesel_async::AsyncPgConnection>,
    &'a mut ::diesel_async::AsyncPgConnection,
    i64,
    bool,
    &'a mut ::std::collections::HashSet<(&'static str, i64)>,
    &'a mut ::std::collections::HashSet<(&'static str, i64)>,
    &'a mut ::std::collections::HashSet<(&'static str, i64)>,
) -> RuntimeDependentCascadeFuture<'a>;

/// One model-declared dependent-cascade association, produced at compile time
/// by `#[model]` and consulted at run time by the parent's `delete_by_id`
/// (#1738).
///
/// The `#[model]` and `#[repository]` derives are separate proc-macro
/// invocations, so the repository macro — which owns the `delete_by_id` cascade
/// codegen — never sees the model struct's `#[has_many]` attributes. This spec
/// bridges the two at run time: the parent iterates
/// [`AutumnDependents::dependents`] and drives each spec's [`cascade`] inside
/// its transaction, reproducing exactly the cascade the repository-attribute
/// `dependent(PgChildRepository, …)` form produces.
///
/// Framework plumbing; not constructed by hand.
///
/// [`cascade`]: RuntimeDependentSpec::cascade
pub struct RuntimeDependentSpec {
    /// The child foreign-key column referencing the parent id. Informational:
    /// the [`cascade`] thunk already binds it. Mirrors the repository-attribute
    /// `fk = "…"`.
    ///
    /// [`cascade`]: RuntimeDependentSpec::cascade
    pub fk: &'static str,
    /// What to do with the children when the parent is deleted.
    pub action: DependentAction,
    /// Type-erased entry into the child repository's cascade leaf executor.
    pub cascade: RuntimeDependentCascadeFn,
}

/// Exposes a model's runtime dependent-cascade specs to the parent
/// repository's generated `delete_by_id` (#1738).
///
/// A blanket impl returns an empty slice for every type; `#[model]` emits an
/// *inherent* `dependents()` associated function — which shadows this trait
/// method under Rust's inherent-before-trait resolution, mirroring the
/// [`AutumnLockVersionModelExt`] pattern — only when the model declares at
/// least one `#[has_many(…, dependent = …)]` / `#[has_one(…, dependent = …)]`
/// association. The generated repository calls `Model::dependents()`, so a
/// model with no dependents (or one defined without `#[model]`) resolves to the
/// empty blanket and keeps its exact prior delete codegen.
///
/// Precedence: the repository-attribute `dependent(…)` form is the explicit
/// escape hatch (needed for child repositories that do not follow the
/// `Pg{Child}Repository` convention, e.g. cross-crate children). When a
/// repository declares `dependent(…)`, its compile-time specs are authoritative
/// and the model-declared runtime specs are ignored; a repository with no
/// `dependent(…)` drives the cascade from `Model::dependents()`.
pub trait AutumnDependents {
    /// The model's dependent-cascade specs, in declaration order. Defaults to
    /// none; `#[model]` overrides via an inherent shadow when dependents exist.
    #[must_use]
    fn dependents() -> &'static [RuntimeDependentSpec] {
        &[]
    }
}

// Blanket fallback — any type without an inherent `dependents()` (i.e. a model
// with no dependent associations, or a type not built through `#[model]`)
// resolves to the empty slice.
impl<T: ?Sized> AutumnDependents for T {}

/// Publish a batch of deferred dependent-cascade OOB *delete* broadcasts,
/// accumulated during a `dependent = destroy` cascade over `broadcasts = true`
/// children and published **after** the parent transaction commits (#1369).
///
/// Each entry is `(topic, dom_id)`; the fragment is empty and the swap is
/// [`OobSwap::Delete`](crate::htmx::OobSwap::Delete). Deferring to post-commit
/// means a rolled-back cascade publishes nothing, so live clients are never told
/// to remove a child that still exists. Commit-hook children defer via the
/// durable outbox instead and are not routed through here.
///
/// Framework plumbing; not a public API. No-op when the live-channel /
/// `maud` / `htmx` features are not built in (the buffer is always empty then).
#[cfg(all(feature = "ws", feature = "maud", feature = "htmx"))]
#[doc(hidden)]
pub fn publish_deferred_dependent_broadcasts(broadcasts: Vec<(String, String)>) {
    if broadcasts.is_empty() {
        return;
    }
    if let Some(channels) = crate::__private::get_global_channels() {
        let fragment = crate::html! {};
        // Consume the buffer so the owned (topic, dom_id) strings move straight
        // into the publish call (no needless clone, and the `Vec` is consumed —
        // avoiding `clippy::needless_pass_by_value`).
        for (topic, dom_id) in broadcasts {
            if let Err(err) = channels.broadcast().publish_oob(
                &topic,
                &dom_id,
                &crate::htmx::OobSwap::Delete,
                &fragment,
            ) {
                tracing::warn!(
                    error = %err,
                    "auto-broadcast dependent-destroy delete failed"
                );
            }
        }
    }
}

/// No-op fallback when live broadcasting is not compiled in. The accumulation
/// side only runs for `broadcasts = true` children (which require these
/// features), so the buffer is always empty in this configuration.
#[cfg(not(all(feature = "ws", feature = "maud", feature = "htmx")))]
#[doc(hidden)]
pub fn publish_deferred_dependent_broadcasts(_broadcasts: Vec<(String, String)>) {}

/// Extension trait that provides a fallback `None` for model structs that do
/// not have a `#[lock_version]` field — or that are defined manually without
/// going through `#[model]`.
///
/// `#[model]` generates an *inherent* method with the same name on the model
/// and on `UpdateModel`; inherent methods take priority over trait methods in
/// Rust's method-resolution order.  For types without `#[lock_version]` (or
/// without `#[model]` altogether), the trait provides the `None` fallback so
/// the generated repository code can call these methods unconditionally.
#[doc(hidden)]
pub trait AutumnLockVersionModelExt {
    fn __autumn_lock_version_actual(&self) -> Option<i64> {
        None
    }
}

#[doc(hidden)]
pub trait AutumnLockVersionUpdateExt {
    fn __autumn_lock_version_expected(&self) -> Option<i64> {
        None
    }
}

// Blanket impls — any type that doesn't have an inherent implementation
// (generated by `#[model]`) falls through to these, returning `None`.
impl<T: ?Sized> AutumnLockVersionModelExt for T {}
impl<T: ?Sized> AutumnLockVersionUpdateExt for T {}

#[doc(hidden)]
pub trait AutumnColumnCountExt {
    fn __autumn_column_count(&self) -> usize;
}

#[doc(hidden)]
pub trait AutumnColumnCountSpecific {
    fn __autumn_column_count(self) -> usize;
}
impl<T: AutumnColumnCountExt> AutumnColumnCountSpecific for &T {
    fn __autumn_column_count(self) -> usize {
        self.__autumn_column_count()
    }
}

#[doc(hidden)]
pub trait AutumnColumnCountFallback {
    fn __autumn_column_count(self) -> usize;
}
impl<T: ?Sized> AutumnColumnCountFallback for &&T {
    fn __autumn_column_count(self) -> usize {
        30
    }
}

#[doc(hidden)]
pub trait AutumnUpsertSetExt {
    type UpsertSet;
    fn __autumn_upsert_set() -> Self::UpsertSet;
}

#[doc(hidden)]
pub trait AutumnUpsertExecutionExt {
    type Model;
    fn __autumn_execute_upsert<'a>(
        chunk: &'a [Self::Model],
        tenant_id: ::core::option::Option<&'a str>,
        conn: &'a mut ::diesel_async::AsyncPgConnection,
    ) -> impl ::std::future::Future<
        Output = ::core::result::Result<::std::vec::Vec<Self::Model>, ::diesel::result::Error>,
    > + Send
    + 'a;
}

#[doc(hidden)]
pub trait AutumnCorrelateExt: Sized {
    type NewModel: Sized;
    fn __autumn_correlate_new(
        inputs: &[Self::NewModel],
        record: &Self,
        matched: &mut [bool],
    ) -> ::core::option::Option<usize>;

    fn __autumn_correlate_model(
        inputs: &[Self],
        record: &Self,
        matched: &mut [bool],
    ) -> ::core::option::Option<usize>;
}

/// Extension trait to override `tenant_id` on changesets in tenant-scoped updates.
pub trait CanSetTenantId {
    fn set_tenant_id(&mut self, tenant_id: String);
}

/// Trait implemented by models to expose their primary key value.
pub trait ModelPrimaryKey {
    type IdType: ::std::fmt::Display + ::core::clone::Clone + Send + Sync + 'static;
    fn primary_key_value(&self) -> Self::IdType;
}

/// Framework plumbing bridging a `#[repository]`-generated struct to the
/// many-to-many (`#[has_many(Target, through = join_table)]`) mutation
/// helpers `#[model]` generates. `#[repository(Model, ...)]` implements this
/// once, unconditionally, alongside the model's other generated impls; the
/// `Model` associated type is what keeps `add_*`/`remove_*`/`set_*` method
/// resolution unambiguous when more than one m2m trait is in scope (e.g. two
/// models both associate `through` the same join table, or a model has more
/// than one m2m association) — each mutation trait is blanket-implemented
/// only for `M2mConnSource<Model = TheAssociationsOwner>`.
///
/// Not part of the public API; not implemented by hand.
#[cfg(feature = "db")]
#[doc(hidden)]
pub trait M2mConnSource: Send + Sync {
    /// The model whose `#[has_many(..., through = ...)]` associations this
    /// repository's mutation helpers operate on.
    type Model;

    /// Acquire a primary-pool connection for an `add_*`/`remove_*`/`set_*`
    /// many-to-many mutation. Mirrors the write-connection acquisition every
    /// other mutating generated method uses (marks the read-your-writes pin
    /// on success).
    fn __autumn_m2m_write_conn(
        &self,
    ) -> impl ::std::future::Future<
        Output = crate::AutumnResult<
            diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
        >,
    > + Send;
}

/// Metadata trait implemented for model structs to expose FTS configuration.
pub trait AutumnSearchableModel {
    const IS_SEARCHABLE: bool;
    const SEARCH_LANGUAGE: &'static str;
    const SEARCH_FIELDS: &'static [(&'static str, char)];
}

/// Derive a stable signed 64-bit advisory lock key for repository upserts.
///
/// Generated versioned repositories use this before pre-reading rows for
/// `upsert_many`, so concurrent generated upserts for the same table/id cannot
/// classify audit history from a stale missing-row snapshot.
#[doc(hidden)]
#[must_use]
pub fn repository_upsert_advisory_lock_key(table_name: &str, record_id: i64) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(b"repository_upsert\0");
    hasher.update(table_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(record_id.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// Trait for formatting repository model fields into SSE broadcast topic segments.
///
/// This provides a uniform display format for both primitive types and optional/nullable values.
pub trait DisplayTopicField {
    /// Formats the field into a string suitable for a topic name.
    fn to_topic_string(&self) -> String;
}

impl DisplayTopicField for String {
    fn to_topic_string(&self) -> String {
        self.clone()
    }
}

impl DisplayTopicField for &str {
    fn to_topic_string(&self) -> String {
        (*self).to_string()
    }
}

impl DisplayTopicField for bool {
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

impl DisplayTopicField for char {
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

macro_rules! impl_display_topic_field_num {
    ($($t:ty),*) => {
        $(
            impl DisplayTopicField for $t {
                fn to_topic_string(&self) -> String {
                    self.to_string()
                }
            }
        )*
    };
}

impl_display_topic_field_num!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl DisplayTopicField for ::uuid::Uuid {
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

impl DisplayTopicField for ::chrono::NaiveDateTime {
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

impl DisplayTopicField for ::chrono::NaiveDate {
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

impl DisplayTopicField for ::chrono::NaiveTime {
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

impl<T> DisplayTopicField for ::chrono::DateTime<T>
where
    T: ::chrono::TimeZone,
    T::Offset: std::fmt::Display,
{
    fn to_topic_string(&self) -> String {
        self.to_string()
    }
}

impl<T: DisplayTopicField> DisplayTopicField for Option<T> {
    fn to_topic_string(&self) -> String {
        self.as_ref()
            .map_or_else(|| "none".to_string(), DisplayTopicField::to_topic_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_variant_stores_all_fields() {
        let err = RepositoryError::Conflict {
            id: 42,
            expected_version: 3,
            actual_version: Some(4),
        };
        match err {
            RepositoryError::Conflict {
                id,
                expected_version,
                actual_version,
            } => {
                assert_eq!(id, 42);
                assert_eq!(expected_version, 3);
                assert_eq!(actual_version, Some(4));
            }
        }
    }

    #[test]
    fn conflict_with_no_actual_version() {
        let err = RepositoryError::Conflict {
            id: 1,
            expected_version: 0,
            actual_version: None,
        };
        assert!(matches!(
            err,
            RepositoryError::Conflict {
                actual_version: None,
                ..
            }
        ));
    }

    #[test]
    fn conflict_display_includes_id_and_expected_version() {
        let err = RepositoryError::Conflict {
            id: 99,
            expected_version: 7,
            actual_version: Some(8),
        };
        let s = err.to_string();
        assert!(s.contains("99"), "display should include id");
        assert!(s.contains('7'), "display should include expected_version");
    }

    #[test]
    fn conflict_is_clone() {
        let err = RepositoryError::Conflict {
            id: 1,
            expected_version: 0,
            actual_version: Some(1),
        };
        let cloned = err.clone();
        assert!(matches!(err, RepositoryError::Conflict { id: 1, .. }));
        assert!(matches!(cloned, RepositoryError::Conflict { id: 1, .. }));
    }

    #[test]
    fn conflict_implements_std_error() {
        let err = RepositoryError::Conflict {
            id: 1,
            expected_version: 0,
            actual_version: None,
        };
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn repository_upsert_advisory_lock_key_is_stable_for_same_table_and_id() {
        let a = repository_upsert_advisory_lock_key("posts", 42);
        let b = repository_upsert_advisory_lock_key("posts", 42);

        assert_eq!(a, b);
        assert_ne!(a, 0);
    }

    #[test]
    fn repository_upsert_advisory_lock_key_separates_table_and_id() {
        let key = repository_upsert_advisory_lock_key("posts", 42);

        assert_ne!(key, repository_upsert_advisory_lock_key("comments", 42));
        assert_ne!(key, repository_upsert_advisory_lock_key("posts", 43));
    }
}
