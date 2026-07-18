//! Static Site Generation (SSG) and Incremental Static Regeneration (ISR).
//!
//! Sometimes, rendering a page on every request is just too slow. For pages where the
//! data changes rarely (like a blog post or a marketing landing page), it is much faster
//! to generate the HTML once at build time.
//!
//! This module provides the engine to pre-render routes annotated with `#[static_get]`
//! and serve them lightning-fast via the [`crate::static_gen::StaticFileLayer`].
//!
//! ## How it works
//!
//! 1. **Build phase:** `autumn build` discovers your `#[static_get]` routes and runs them, saving the HTML to `dist/`.
//! 2. **Runtime phase:** The `StaticFileLayer` intercepts requests. If a pre-rendered file exists, it serves it directly!
//!
//! ## Build-time-only vs runtime ISR: choosing the right model
//!
//! ### Build-time-only (`revalidate = None`)
//!
//! Pages are rendered once during `autumn build` and served read-only at
//! runtime. `dist/` can be baked into a container image or uploaded to a
//! CDN -- no process needs write access at runtime.
//!
//! **When to use:** marketing pages, blog posts, documentation -- any page
//! where staleness of a full redeploy cycle is acceptable.
//!
//! ### Runtime ISR (`#[static_get(..., revalidate = N)]`)
//!
//! Pages are pre-rendered at build time **and** refreshed in the background
//! when they become stale (stale-while-revalidate). The serving process
//! needs write access to `dist/` at runtime.
//!
//! **When to use:** product listings, leaderboards, dashboards -- pages
//! that should stay fresh without a full redeploy but don't need to be
//! up-to-the-second accurate.
//!
//! #### Multi-replica ISR
//!
//! In single-replica / development deployments the default
//! [`crate::static_gen::isr_coordinator::LocalIsrCoordinator`] is sufficient: an `AtomicBool`
//! per route prevents duplicate background tasks within the same process.
//!
//! In **multi-replica** deployments sharing a writable `dist/` volume or
//! object-backed storage, each replica independently detects staleness and
//! can race to regenerate the same page. Supply a
//! [`crate::static_gen::isr_coordinator::PostgresIsrCoordinator`] (feature `db`) via
//! [`crate::static_gen::StaticFileLayer::with_isr_coordinator`] so that only one replica wins
//! the lock per revalidation window:
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use autumn_web::static_gen::{StaticFileLayer, PostgresIsrCoordinator};
//!
//! let layer = StaticFileLayer::new("dist")
//!     .unwrap()
//!     .with_router(app_router.clone())
//!     .with_isr_coordinator(Arc::new(PostgresIsrCoordinator::new(db_pool)));
//! ```
//!
//! See [`crate::static_gen::isr_coordinator`] for the full deployment contract table.

pub(crate) mod build;
pub mod isr_coordinator;
mod middleware;
mod types;

/// Request-extension marker for the internal `oneshot` render calls made by
/// `autumn build` and ISR background regeneration.
///
/// These renders drive a `#[static_get]` GET route directly to capture its HTML
/// — there is no inbound client connection whose `request_timeout_ms` deadline
/// should apply, and a legitimately slow prerender must not fail the build or
/// skip an ISR refresh with a `503`. The request-timeout middleware skips its
/// deadline whenever this marker is present on the request.
///
/// Live inbound requests to the same route (a manifest miss, no `dist`, or a
/// cache read that falls through to the dynamic router) do NOT carry this
/// marker, so they remain bounded by the configured deadline. This scopes the
/// build/ISR exemption to internal renders instead of the live route metadata.
#[derive(Clone, Copy, Debug)]
pub struct RenderDeadlineExempt;

pub use build::{BuildError, render_static_routes};
#[cfg(feature = "db")]
pub use isr_coordinator::PostgresIsrCoordinator;
pub use isr_coordinator::{
    IsrCoordinator, LocalIsrCoordinator, isr_advisory_lock_key, isr_window_key,
};
pub use middleware::StaticFileLayer;
pub use types::{
    ManifestEntry, ParamsFn, StaticManifest, StaticParams, StaticRouteMeta, url_to_file_path,
};
