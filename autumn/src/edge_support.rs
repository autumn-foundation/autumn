//! Origin-side glue between an Autumn app and the edge capsule (issue #1790).
//!
//! The edge lane's whole point is that one handler source serves from two
//! substrates. That only works if every seam it touches is mediated by the
//! framework, so this module supplies the origin half of the one seam the first
//! slice mediates: a key/value read.
//!
//! | Substrate | Behind [`EdgeCache`] sits… | Installed by |
//! | --- | --- | --- |
//! | Origin | [`CacheEdgeKv`] over the app's own `Cache` | [`AppBuilder::with_edge_kv`](crate::app::AppBuilder::with_edge_kv) |
//! | Edge | the capsule runtime's dialogue-backed reader | `autumn_edge::serve` |
//!
//! A handler sees neither: it takes [`EdgeCache`] and cannot tell which store
//! answered. That is what makes the identical source portable, and it is why
//! the adapter here reads through the *same* `insert_cached` / `get_cached`
//! serde path the rest of the framework writes through — an origin route that
//! caches bytes under a key has, by that act alone, published them to the edge
//! lane.
//!
//! # This is not a database (ADR-0004 category 2)
//!
//! [`EdgeKv`] is a replica-local, opportunistic read accelerator, never a
//! source of truth. It has no `put`; a miss is always a legal answer; staleness
//! is expected and there is no invalidation protocol. A route whose correctness
//! depends on the value being present, current, or authoritative does not
//! belong in the edge lane — serve it from the origin, where the database is.
//! [`CacheEdgeKv`] inherits exactly those properties from the cache it wraps.
//!
//! [`EdgeCache`]: autumn_edge::EdgeCache
//! [`EdgeKv`]: autumn_edge::EdgeKv

// This module runs on the request path: `EdgeKv::get` is called by the
// `EdgeCache` extractor while a request is in flight. Production code here must
// therefore be panic-free. It is deliberately not listed in
// `scripts/check-panic-gate.sh`'s manifest: that check refuses a manifest entry
// whose feature no enforcing CI clippy lane compiles, and no lane enables
// `edge` yet (see check 8 / FEATURE_LINT_EXEMPT in that script). The deny block
// below is real and is enforced by any `cargo clippy -p autumn-web --features
// edge` run; add the manifest entry in the same change that adds `edge` to a
// linted CI lane.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::sync::Arc;

use autumn_edge::EdgeKv;

use crate::cache::Cache;

/// An [`EdgeKv`] backed by the application's own cache.
///
/// This is the adapter that makes an `#[edge(needs(kv))]` handler work at the
/// origin: it projects the seven-method, type-erased [`Cache`] onto the
/// one-method byte-oriented seam the edge lane can mediate.
///
/// # What the origin has to do to publish a value
///
/// Nothing edge-specific. Write bytes through the ordinary serde-aware cache
/// path and an edge handler reading the same key sees them:
///
/// ```rust
/// use std::sync::Arc;
///
/// use autumn_web::CacheEdgeKv;
/// use autumn_web::cache::{Cache, MokaCache, insert_cached};
/// use autumn_web::edge::EdgeKv;
///
/// let cache = MokaCache::new(128, None);
/// insert_cached(&cache, "banner", b"Autumn is up".to_vec(), None);
///
/// let kv = CacheEdgeKv::new(Arc::new(cache) as Arc<dyn Cache>);
/// assert_eq!(kv.get("banner"), Some(b"Autumn is up".to_vec()));
/// assert_eq!(kv.get("nothing-here"), None);
/// ```
///
/// `Vec<u8>` is the wire currency on purpose: it is the only shape that
/// survives both an in-process backend (stored as-is) and a serializing one
/// like Redis (JSON round-tripped), so a value published on one replica reads
/// back identically on another. A key holding some *other* type is reported as
/// a miss rather than an error — the seam has exactly one failure mode, and a
/// handler already has to render something sensible for it.
pub struct CacheEdgeKv(Arc<dyn Cache>);

impl CacheEdgeKv {
    /// Adapt a cache backend into the edge key/value seam.
    ///
    /// Pass the same backend the app serves from — typically the one given to
    /// [`AppBuilder::with_cache_backend`](crate::app::AppBuilder::with_cache_backend)
    /// — so the edge lane observes what the origin publishes.
    #[must_use]
    pub const fn new(cache: Arc<dyn Cache>) -> Self {
        Self(cache)
    }

    /// The cache this adapter reads through.
    #[must_use]
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.0
    }
}

impl fmt::Debug for CacheEdgeKv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn Cache` is not `Debug`, and a cache's contents are the last thing
        // that belongs in a log line anyway.
        f.write_str("CacheEdgeKv(..)")
    }
}

impl EdgeKv for CacheEdgeKv {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        crate::cache::get_cached::<Vec<u8>>(self.0.as_ref(), key)
    }
}

#[cfg(all(test, feature = "cache-moka"))]
mod tests {
    use super::*;
    use crate::cache::{MokaCache, insert_cached};

    fn cache_with(key: &str, value: &[u8]) -> Arc<dyn Cache> {
        let cache = MokaCache::new(16, None);
        insert_cached(&cache, key, value.to_vec(), None);
        Arc::new(cache)
    }

    #[test]
    fn reads_bytes_written_through_the_ordinary_cache_path() {
        let kv = CacheEdgeKv::new(cache_with("banner", b"hello"));
        assert_eq!(kv.get("banner"), Some(b"hello".to_vec()));
    }

    #[test]
    fn an_absent_key_is_a_miss() {
        let kv = CacheEdgeKv::new(cache_with("banner", b"hello"));
        assert_eq!(kv.get("absent"), None);
    }

    #[test]
    fn a_value_of_another_type_is_a_miss_not_a_failure() {
        let cache = MokaCache::new(16, None);
        insert_cached(&cache, "count", 7_u64, None);
        let kv = CacheEdgeKv::new(Arc::new(cache));

        assert_eq!(kv.get("count"), None);
    }

    #[test]
    fn a_serializing_backend_round_trips_through_the_raw_bytes_path() {
        // What a cross-replica backend (Redis) stores: JSON bytes under
        // `RawCacheBytes`, not the concrete `Vec<u8>`. The seam must read that
        // shape too, or a value published on one replica would vanish at the
        // edge of another.
        let cache = MokaCache::new(16, None);
        let json = serde_json::to_vec(&b"hello".to_vec()).expect("Vec<u8> serializes");
        cache.insert_value("banner", Arc::new(crate::cache::RawCacheBytes(json)));
        let kv = CacheEdgeKv::new(Arc::new(cache));

        assert_eq!(kv.get("banner"), Some(b"hello".to_vec()));
    }

    #[test]
    fn debug_names_the_adapter_without_leaking_contents() {
        let kv = CacheEdgeKv::new(cache_with("secret", b"s3cr3t"));
        let rendered = format!("{kv:?}");

        assert!(rendered.contains("CacheEdgeKv"), "{rendered}");
        assert!(!rendered.contains("s3cr3t"), "{rendered}");
    }
}
