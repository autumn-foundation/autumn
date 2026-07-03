//! Eager-loading ("preload") runtime for `#[model]` associations.
//!
//! Autumn stays explicit about database work: there is no implicit lazy
//! loading. You declare relationships on a `#[model]` with
//! `#[belongs_to(...)]`, `#[has_many(...)]`, and `#[has_one(...)]`, then ask a
//! `#[repository]` finder to [`preload`](#preload) them in a bounded number of
//! batched `WHERE ... IN (...)` queries. Accessing an association that was not
//! preloaded returns a typed [`NotLoaded`] error instead of silently issuing
//! SQL.
//!
//! # The pieces
//!
//! * [`Preloaded<T>`] wraps a record and carries its loaded associations. It
//!   derefs to `T`, so field access (`post.title`) keeps working; generated
//!   accessor traits add `post.author()` / `post.comments()`.
//! * [`Associations`](crate::preload::Associations) is the type-erased store
//!   behind a [`Preloaded`].
//! * [`NotLoaded`] is returned by an accessor when its association was not part
//!   of the preload set.
//! * [`Preloadable`](crate::preload::Preloadable) (db-only) is implemented by
//!   `#[model]` for each record type and drives the batched loading, including
//!   nested preload paths.
//!
//! See `docs/adr/0008-associations-and-eager-loading.md` for the design
//! rationale, including how preload interacts with the primary/replica
//! topology and with cursor pagination.

use std::any::Any;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use thiserror::Error;

/// Returned when a preloadable association is accessed without first being
/// preloaded.
///
/// Autumn never lazy-loads: if you call `post.author()` on a record that was
/// not loaded with `.preload(...)`, you get this error rather than a hidden
/// SQL round trip.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "association `{association}` on `{model}` was accessed but not preloaded; \
     add it to the `.preload(...)` set on the finder query"
)]
pub struct NotLoaded {
    /// The model whose association was accessed, e.g. `"Post"`.
    pub model: &'static str,
    /// The association name that was accessed, e.g. `"author"`.
    pub association: &'static str,
}

impl NotLoaded {
    /// Construct a [`NotLoaded`] for a model/association pair.
    #[must_use]
    pub const fn new(model: &'static str, association: &'static str) -> Self {
        Self { model, association }
    }
}

/// Type-erased store of preloaded associations attached to a [`Preloaded`]
/// record.
///
/// Keys are the association names declared on the `#[model]`. Values are the
/// loaded records, boxed as `dyn Any`:
///
/// * `belongs_to` / `has_one` store an `Option<Arc<Preloaded<Target>>>`
///   (`Arc` because several parents may share one related record).
/// * `has_many` stores a `Vec<Preloaded<Target>>`.
///
/// Generated accessors downcast back to the concrete type. A missing key means
/// "not preloaded" and yields [`NotLoaded`].
#[derive(Default)]
pub struct Associations {
    map: HashMap<&'static str, Box<dyn Any + Send + Sync>>,
}

impl Associations {
    /// Create an empty association store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a loaded association under `key`.
    pub fn insert<T: Any + Send + Sync>(&mut self, key: &'static str, value: T) {
        self.map.insert(key, Box::new(value));
    }

    /// Borrow a loaded association as `T`, or `None` if the key was never
    /// preloaded (or the stored type does not match `T`).
    #[must_use]
    pub fn get<T: Any + Send + Sync>(&self, key: &'static str) -> Option<&T> {
        self.map.get(key).and_then(|b| b.downcast_ref::<T>())
    }

    /// Mutably borrow a loaded association as `T`. Used by nested preloading to
    /// recurse into already-loaded children.
    #[must_use]
    pub fn get_mut<T: Any + Send + Sync>(&mut self, key: &'static str) -> Option<&mut T> {
        self.map.get_mut(key).and_then(|b| b.downcast_mut::<T>())
    }

    /// Whether `key` has been preloaded.
    #[must_use]
    pub fn contains(&self, key: &'static str) -> bool {
        self.map.contains_key(key)
    }

    /// Number of preloaded associations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no associations have been preloaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl std::fmt::Debug for Associations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys: Vec<&&str> = self.map.keys().collect();
        keys.sort_unstable();
        f.debug_struct("Associations")
            .field("loaded", &keys)
            .finish()
    }
}

/// A record paired with its preloaded associations.
///
/// `Preloaded<T>` [`Deref`]s to `T`, so all of the record's own fields and
/// inherent methods are available directly (`post.title`, `post.id`).
/// Generated association accessor traits — for example `PostAssociations` with
/// `author()` and `comments()` — are implemented for `Preloaded<Post>`.
///
/// ```rust
/// use autumn_web::preload::Preloaded;
///
/// #[derive(Debug)]
/// struct Post { id: i64, title: String }
///
/// let post = Preloaded::new(Post { id: 1, title: "hi".into() });
/// // Deref: the record's own fields are reachable.
/// assert_eq!(post.id, 1);
/// assert_eq!(post.title, "hi");
/// // Nothing has been preloaded yet, so the association store is empty.
/// assert!(post.associations().is_empty());
/// ```
#[derive(Debug)]
pub struct Preloaded<T> {
    inner: T,
    associations: Associations,
}

impl<T> Preloaded<T> {
    /// Wrap a record with an empty association store.
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            associations: Associations::new(),
        }
    }

    /// Borrow the wrapped record.
    #[must_use]
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Consume the wrapper and return the bare record, dropping associations.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// Borrow the association store (read-only).
    #[must_use]
    pub const fn associations(&self) -> &Associations {
        &self.associations
    }

    /// Mutably borrow the association store. Used by generated loaders to
    /// attach freshly loaded records.
    pub const fn associations_mut(&mut self) -> &mut Associations {
        &mut self.associations
    }
}

impl<T> Deref for Preloaded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for Preloaded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> From<T> for Preloaded<T> {
    fn from(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T: serde::Serialize> serde::Serialize for Preloaded<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize transparently as the inner record. Associations are a
        // server-side loading concern, not part of the record's wire shape.
        self.inner.serialize(serializer)
    }
}

/// The empty preload specification, used as `Preloadable::Spec` for models that
/// declare no associations (or for manual models that opt in without nested
/// preload support).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPreload;

/// Reports a model's *repository* read-scoping so that `preload`'s in-memory
/// retain can match what generated finders do.
///
/// The decision to apply tenant isolation or soft-delete filtering lives on
/// `#[repository(..., tenant_scoped, soft_delete)]`, which the `#[model]` macro
/// (the one that generates the retain) cannot see. So the `#[repository]` macro
/// overrides these — as *inherent* associated fns on the model, which take
/// priority over this blanket default — for the flags it enables. Models with
/// no repository (or a non-scoped one) keep the `false` defaults, so preload
/// applies no scoping they wouldn't get from a finder.
///
/// This is framework plumbing, not a public API.
#[doc(hidden)]
pub trait AutumnPreloadScopeExt {
    /// Whether the model's repository is `soft_delete`.
    #[must_use]
    fn __autumn_repo_soft_delete_scope() -> bool {
        false
    }
    /// Whether the model's repository is `tenant_scoped`.
    #[must_use]
    fn __autumn_repo_tenant_scope() -> bool {
        false
    }
}

impl<T: ?Sized> AutumnPreloadScopeExt for T {}

#[cfg(feature = "db")]
tokio::task_local! {
    /// Set by a repository's `preload` to the repository's `across_tenants`
    /// choice, so the (recursive) target tenant-retain can skip the tenant
    /// predicate for cross-tenant admin/reporting loads — the same way
    /// `across_tenants()` makes finders skip it. Defaults to `false` (scoped).
    #[doc(hidden)]
    pub static PRELOAD_ACROSS_TENANTS: bool;
}

/// Whether the current preload should bypass tenant scoping (i.e. it was
/// started from a repository pinned with `across_tenants()`). `false` outside a
/// preload or when not pinned. Framework plumbing, not a public API.
#[cfg(feature = "db")]
#[doc(hidden)]
#[must_use]
pub fn preload_across_tenants() -> bool {
    PRELOAD_ACROSS_TENANTS.try_with(|v| *v).unwrap_or(false)
}

/// Implement [`Preloadable`] for a hand-written model as a leaf association
/// target.
///
/// `#[model]` implements [`Preloadable`] automatically. Use this macro for
/// manually-defined models (those not using `#[model]`) that need to appear as
/// the *target* of a `#[belongs_to]` / `#[has_one]` / `#[has_many]` on another
/// model. A leaf target loads no associations of its own (its `Spec` is
/// [`NoPreload`]), so it can be preloaded and wrapped in [`Preloaded`] but not
/// nested into.
///
/// The type must implement `diesel::Queryable`/`Selectable` for its table.
///
/// ```ignore
/// autumn_web::impl_preloadable_leaf!(User);
/// ```
#[cfg(feature = "db")]
#[macro_export]
macro_rules! impl_preloadable_leaf {
    ($ty:ty) => {
        impl $ty {
            /// Identity scoping for a hand-written preload target: rows are
            /// returned unchanged (no tenant/soft-delete filtering is applied
            /// for manually-defined models). See [`Preloaded`].
            #[doc(hidden)]
            pub fn __autumn_preload_retain(
                rows: ::std::vec::Vec<Self>,
            ) -> $crate::AutumnResult<::std::vec::Vec<Self>> {
                ::core::result::Result::Ok(rows)
            }

            /// Identity per-row scoping for a hand-written preload target. See
            /// [`Preloaded`] and the per-row sibling of `__autumn_preload_retain`
            /// used by many-to-many (`through =`) preload loaders.
            #[doc(hidden)]
            pub fn __autumn_preload_keep(
                row: Self,
            ) -> $crate::AutumnResult<::core::option::Option<Self>> {
                ::core::result::Result::Ok(::core::option::Option::Some(row))
            }
        }

        impl $crate::preload::Preloadable for $ty {
            type Spec = $crate::preload::NoPreload;
            fn load_associations<'__a>(
                _records: &'__a mut [$crate::preload::Preloaded<Self>],
                _spec: &'__a Self::Spec,
                _conn: &'__a mut $crate::reexports::diesel_async::AsyncPgConnection,
            ) -> $crate::preload::PreloadFuture<'__a> {
                ::std::boxed::Box::pin(async move { ::core::result::Result::Ok(()) })
            }
        }
    };
}

#[cfg(feature = "db")]
pub use db_support::{PreloadFuture, Preloadable};

#[cfg(feature = "db")]
mod db_support {
    use super::Preloaded;
    use crate::AutumnResult;
    use std::future::Future;
    use std::pin::Pin;

    /// Boxed future returned by [`Preloadable::load_associations`]. Boxing
    /// breaks the recursion in nested preload paths (a loader that loads
    /// children then asks the children's loader to run).
    pub type PreloadFuture<'a> = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'a>>;

    /// Implemented by every `#[model]` to drive batched eager loading.
    ///
    /// `#[model]` generates the implementation; you rarely implement this by
    /// hand. A manual implementation is only needed for hand-written models
    /// that appear as the *target* of an association declared elsewhere (so
    /// they can be wrapped in [`Preloaded`] and, optionally, nested into).
    pub trait Preloadable: Sized + Send + Sync + 'static {
        /// The builder describing which associations (and nested associations)
        /// to load. `#[model]` generates a `{Model}Preload` type;
        /// association-free models use [`super::NoPreload`].
        type Spec: Default + Send + Sync + 'static;

        /// Load every association named in `spec` for `records`, issuing at
        /// most one batched query per association level, then recurse into any
        /// nested specs. All queries run on `conn` so preloads share the read
        /// role of the parent query.
        fn load_associations<'a>(
            records: &'a mut [Preloaded<Self>],
            spec: &'a Self::Spec,
            conn: &'a mut crate::reexports::diesel_async::AsyncPgConnection,
        ) -> PreloadFuture<'a>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Debug, PartialEq)]
    struct User {
        id: i64,
        name: String,
    }

    #[derive(Debug, PartialEq)]
    struct Post {
        id: i64,
        author_id: i64,
        title: String,
    }

    #[derive(Debug, PartialEq)]
    struct Comment {
        id: i64,
        post_id: i64,
        body: String,
    }

    #[test]
    fn deref_exposes_inner_fields() {
        let post = Preloaded::new(Post {
            id: 1,
            author_id: 7,
            title: "hello".into(),
        });
        assert_eq!(post.id, 1);
        assert_eq!(post.title, "hello");
    }

    #[test]
    fn belongs_to_happy_path_returns_loaded_parent() {
        let mut post = Preloaded::new(Post {
            id: 1,
            author_id: 7,
            title: "t".into(),
        });
        let author = Arc::new(Preloaded::new(User {
            id: 7,
            name: "ada".into(),
        }));
        post.associations_mut()
            .insert::<Option<Arc<Preloaded<User>>>>("author", Some(author));

        let got = post
            .associations()
            .get::<Option<Arc<Preloaded<User>>>>("author")
            .expect("author preloaded")
            .as_ref()
            .expect("author present");
        assert_eq!(got.name, "ada");
    }

    #[test]
    fn belongs_to_missing_parent_is_some_none() {
        let mut post = Preloaded::new(Post {
            id: 1,
            author_id: 99,
            title: "t".into(),
        });
        // Preloaded, but no matching parent row exists.
        post.associations_mut()
            .insert::<Option<Arc<Preloaded<User>>>>("author", None);

        // Key present => preloaded; value None => parent genuinely missing.
        assert!(post.associations().contains("author"));
        let got = post
            .associations()
            .get::<Option<Arc<Preloaded<User>>>>("author")
            .unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn has_many_groups_children() {
        let mut post = Preloaded::new(Post {
            id: 1,
            author_id: 7,
            title: "t".into(),
        });
        let comments = vec![
            Preloaded::new(Comment {
                id: 10,
                post_id: 1,
                body: "a".into(),
            }),
            Preloaded::new(Comment {
                id: 11,
                post_id: 1,
                body: "b".into(),
            }),
        ];
        post.associations_mut()
            .insert::<Vec<Preloaded<Comment>>>("comments", comments);

        let got = post
            .associations()
            .get::<Vec<Preloaded<Comment>>>("comments")
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "a");
    }

    #[test]
    fn has_many_empty_children_is_empty_vec() {
        let mut post = Preloaded::new(Post {
            id: 1,
            author_id: 7,
            title: "t".into(),
        });
        post.associations_mut()
            .insert::<Vec<Preloaded<Comment>>>("comments", Vec::new());

        let got = post
            .associations()
            .get::<Vec<Preloaded<Comment>>>("comments")
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn not_preloaded_key_is_absent() {
        let post = Preloaded::new(Post {
            id: 1,
            author_id: 7,
            title: "t".into(),
        });
        assert!(!post.associations().contains("author"));
        assert!(
            post.associations()
                .get::<Option<Arc<Preloaded<User>>>>("author")
                .is_none()
        );
    }

    #[test]
    fn not_loaded_error_carries_model_and_association() {
        let err = NotLoaded::new("Post", "author");
        assert_eq!(err.model, "Post");
        assert_eq!(err.association, "author");
        let msg = err.to_string();
        assert!(msg.contains("Post"));
        assert!(msg.contains("author"));
        assert!(msg.contains("not preloaded"));
    }

    #[test]
    fn shared_parent_via_arc_is_cheap_to_clone() {
        let author = Arc::new(Preloaded::new(User {
            id: 7,
            name: "ada".into(),
        }));
        let mut p1 = Preloaded::new(Post {
            id: 1,
            author_id: 7,
            title: "t1".into(),
        });
        let mut p2 = Preloaded::new(Post {
            id: 2,
            author_id: 7,
            title: "t2".into(),
        });
        p1.associations_mut()
            .insert::<Option<Arc<Preloaded<User>>>>("author", Some(Arc::clone(&author)));
        p2.associations_mut()
            .insert::<Option<Arc<Preloaded<User>>>>("author", Some(author));

        let a1 = p1
            .associations()
            .get::<Option<Arc<Preloaded<User>>>>("author")
            .unwrap()
            .as_ref()
            .unwrap();
        let a2 = p2
            .associations()
            .get::<Option<Arc<Preloaded<User>>>>("author")
            .unwrap()
            .as_ref()
            .unwrap();
        assert!(Arc::ptr_eq(a1, a2));
    }
}
