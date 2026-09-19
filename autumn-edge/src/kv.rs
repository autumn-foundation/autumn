//! The mediated key/value seam.
//!
//! # Why a new trait instead of widening `Cache`
//!
//! `autumn_web::cache::Cache` is a type-erased, `Arc<dyn Any>`-shaped,
//! seven-method trait with fill locks and TTLs. None of that can cross a WASI
//! boundary, and none of it is needed to read bytes. [`EdgeKv`] is the
//! narrowest trait that still lets the *same handler source* run at the origin
//! and at the edge: **one** required method, returning bytes.
//!
//! # ADR-0004 category 2: a non-authoritative read accelerator
//!
//! An `EdgeKv` is explicitly **not** a database and **not** a source of truth.
//! It is a replica-local, opportunistic accelerator in the sense of ADR-0004
//! category 2:
//!
//! - **Reads only.** There is no `put`. The origin owns every write; the edge
//!   observes.
//! - **A miss is always legal.** `None` is a normal answer, not an error. A
//!   correct handler renders something sensible on a miss — it may not assume
//!   the key is there, because a different edge replica may not have it yet.
//! - **Staleness is expected.** No coherence protocol, no invalidation
//!   broadcast, no read-your-writes guarantee across replicas.
//! - **Never authoritative.** Anything a user's money, permissions, or safety
//!   depends on is a read the origin must serve. A route that cannot tolerate
//!   a stale or missing value does not belong in the edge lane.
//!
//! The single method is also a stability commitment: future seams arrive as
//! *defaulted* methods so an existing implementor keeps compiling.
//!
//! # The two substrates
//!
//! | Where | Implementation | Injected by |
//! | --- | --- | --- |
//! | Origin | `autumn_web::CacheEdgeKv` over the app's `Cache` | `AppBuilder::with_edge_kv` |
//! | Edge | the capsule runtime's dialogue-backed reader | [`serve`](crate::runtime::serve) |
//!
//! Handlers see neither: they take [`EdgeCache`](crate::extract::EdgeCache).

use std::collections::BTreeMap;

/// A non-authoritative, read-only key/value replica.
///
/// See the [module documentation](self) for the guarantees this trait
/// deliberately does *not* make.
///
/// Implementations must be `Send + Sync`: at the origin one instance is shared
/// by every request on every worker thread.
pub trait EdgeKv: Send + Sync {
    /// Read `key`. `None` means "not here" — a miss, an expiry, or a replica
    /// that has not caught up yet. Callers may not distinguish these.
    fn get(&self, key: &str) -> Option<Vec<u8>>;
}

/// An [`EdgeKv`] that is always empty.
///
/// Useful as the "capability declared but nothing behind it" case in tests and
/// as a placeholder in examples.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyEdgeKv;

impl EdgeKv for EmptyEdgeKv {
    fn get(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }
}

/// An in-memory [`EdgeKv`] backed by a [`BTreeMap`].
///
/// Ordered rather than hashed on purpose: conformance runs must be
/// reproducible, and a `HashMap` iteration order is not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemoryEdgeKv(BTreeMap<String, Vec<u8>>);

impl InMemoryEdgeKv {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value` under `key`, returning `self` for chaining.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    /// Store `value` under `key`.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.0.insert(key.into(), value.into());
    }
}

impl EdgeKv for InMemoryEdgeKv {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.get(key).cloned()
    }
}

impl<T: EdgeKv + ?Sized> EdgeKv for std::sync::Arc<T> {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        (**self).get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_kv_always_misses() {
        assert_eq!(EmptyEdgeKv.get("anything"), None);
    }

    #[test]
    fn in_memory_kv_hits_and_misses() {
        let kv = InMemoryEdgeKv::new().with("greeting", "hello");
        assert_eq!(kv.get("greeting"), Some(b"hello".to_vec()));
        assert_eq!(kv.get("absent"), None);
    }

    #[test]
    fn arc_forwards_to_the_inner_store() {
        let kv: std::sync::Arc<dyn EdgeKv> =
            std::sync::Arc::new(InMemoryEdgeKv::new().with("k", "v"));
        assert_eq!(kv.get("k"), Some(b"v".to_vec()));
    }
}
