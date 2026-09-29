//! `edge-greeting` — one app, two substrates (issue #1790).
//!
//! The library half is split by what each module is allowed to depend on:
//!
//! - [`handlers`] is **edge-safe**. It compiles for `wasm32-wasip1`, depends
//!   only on `autumn-edge`, and holds nothing but `#[edge]` `GET` routes.
//! - [`origin`] is **origin-only**. It is compiled out for wasm and may use
//!   anything `autumn-web` offers.
//!
//! `src/main.rs` mounts both; `src/bin/edge-capsule.rs` mounts only the first.

pub mod handlers;

#[cfg(not(target_arch = "wasm32"))]
pub mod origin;

use std::sync::Arc;

use autumn_edge::{EdgeKv, InMemoryEdgeKv};

/// The demo key/value replica behind the [`EdgeCache`](autumn_edge::EdgeCache)
/// seam.
///
/// A real app passes `CacheEdgeKv::new(cache)` — the app's own cache, projected
/// onto the [`EdgeKv`] seam — and a real CDN answers `kv_get` from its own
/// replica. Both are the same shape: a read-only, non-authoritative store where
/// a miss is a legal answer.
///
/// [`InMemoryEdgeKv`] is a `BTreeMap`, not a `HashMap`, so a conformance run is
/// reproducible.
#[must_use]
pub fn demo_kv() -> Arc<dyn EdgeKv> {
    Arc::new(
        InMemoryEdgeKv::new()
            .with("greeting", "the origin published this note to the edge")
            .with("release", "0.6.0"),
    )
}
