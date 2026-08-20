//! Everything an edge-safe handler module usually needs, and nothing that
//! would not compile at the edge.
//!
//! ```rust
//! use autumn_edge::prelude::*;
//!
//! async fn greet(Path(name): Path<String>) -> String {
//!     format!("hello {name}")
//! }
//!
//! async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
//!     cache.get_string(&key).unwrap_or_else(|| "miss".to_owned())
//! }
//! ```
//!
//! The list is short by design: an extractor that is not here is one the edge
//! cannot mediate, and reaching for it should be a compile error rather than a
//! surprise at deploy time.

pub use autumn_macros::{edge, edge_routes, get};
pub use axum::extract::{Path, Query};
// `http` is not a direct dependency of an app that depends only on
// `autumn-edge`, so a handler returning `(StatusCode, …)` or a hand-written
// `EdgeRoute` (whose `method` is an `http::Method`) has no way to name these
// without them. `crate::reexports::http` is the escape hatch for the rest.
pub use http::{HeaderMap, Method, StatusCode};

pub use crate::extract::EdgeCache;
pub use crate::kv::EdgeKv;
pub use crate::route::{EdgeCapability, EdgeRoute};
