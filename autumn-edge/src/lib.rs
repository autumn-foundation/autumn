//! # autumn-edge
//!
//! The edge lane for [Autumn](https://crates.io/crates/autumn-web) apps: opt-in
//! read-path `GET` routes compiled into a portable `wasm32-wasip1` artifact
//! that a CDN can run, with the origin binary staying the authority and the
//! fallback.
//!
//! This is the only Autumn crate that compiles for **both** the host target and
//! `wasm32-wasip1`. It links no tokio, no hyper, no mio — just `axum` with
//! default features off, its `matchit` router, and a futures executor.
//!
//! ## The shape of it
//!
//! ```text
//!            ┌──────────── edge (wasm32-wasip1) ────────────┐
//!  request → │ capsule: same axum router, same handler code │ → response
//!            └───────────────────┬──────────────────────────┘
//!                                │ cannot serve this
//!                                ▼
//!            ┌──────────── origin (native binary) ──────────┐
//!  request → │ the whole app: db, sessions, auth, writes    │ → response
//!            └──────────────────────────────────────────────┘
//! ```
//!
//! An author writes one handler. The `#[edge]` macro emits a second, tiny
//! registration companion for it; `edge_routes![]` collects those into a
//! [`Vec<EdgeRoute>`]; `src/bin/edge-capsule.rs` hands them to [`serve`]. The
//! same function also stays mounted in the origin router, because the origin
//! serves everything — that is what makes fallthrough free of author glue.
//!
//! ## What the edge can and cannot do
//!
//! | Available | Not available |
//! | --- | --- |
//! | `Path`, `Query`, `HeaderMap` | sessions, auth, CSRF, flash |
//! | [`EdgeCache`] (a mediated, non-authoritative KV read) | a database, writes of any kind |
//! | any pure rendering | the `Clock`/`Rng` extractors, outbound HTTP |
//!
//! Ambient time and randomness (`SystemTime::now()`, `getrandom`) *compile* at
//! the edge but return whatever the host decides — the reference host pins
//! them for determinism, a real edge host will not — so keep them out of
//! handlers (see `docs/guide/edge.md`).
//!
//! The restriction is enforced by the type system: an edge handler must be an
//! `axum` handler over the unit [`EdgeState`], so a native-only extractor
//! cannot satisfy the [`EdgeHandler`] bound and the author gets a diagnostic
//! naming the fix.
//!
//! ## Everything else is a decline, not a failure
//!
//! A request the edge cannot serve produces an [`EdgeOutcome::Fallthrough`]
//! and the host forwards it upstream unchanged. Unknown route, write method,
//! an unmediated capability, an unsupported wire version, a trap — all of them
//! land in the same channel, and the origin answers as it always would.
//!
//! ## Determinism is the contract
//!
//! Byte-identity between origin and edge only holds if the handler is a
//! function of its request. That means no wall clock, no randomness, no
//! `HashMap` iteration order in rendered output, and no reliance on a KV hit.
//! [`conformance`] provides the projection and verdict the CI test uses to
//! prove it for a corpus of requests; see `docs/guide/edge.md` for the full
//! rules.
//!
//! [`Vec<EdgeRoute>`]: EdgeRoute

// autumn-panic-gate: request-path crate — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
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
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod conformance;
pub mod extract;
pub mod handler;
pub mod kv;
pub mod prelude;
pub mod reexports;
pub mod route;
pub mod router;
pub mod runtime;
pub mod wire;

#[cfg(feature = "host")]
#[cfg_attr(docsrs, doc(cfg(feature = "host")))]
pub mod host;

/// The route macros an edge-safe handler module needs.
///
/// An `#[edge]` module compiles for `wasm32-wasip1`, where `autumn-web` is not
/// in the dependency graph at all — so the macros have to be reachable from
/// here. This is target-clean: a proc-macro crate is compiled for the *host*
/// even when the crate using it is compiled for wasm, so `autumn-macros` never
/// enters a capsule's link graph.
///
/// The dependency runs one way only. `autumn-macros` emits `::autumn_edge::…`
/// paths as text and does not depend on this crate.
pub use autumn_macros::{edge, edge_routes, get};

pub use extract::{EdgeCache, EdgeCacheUnavailable};
pub use handler::{EdgeHandler, edge_get};
pub use kv::{EdgeKv, EmptyEdgeKv, InMemoryEdgeKv};
pub use route::{EdgeCapability, EdgeRoute, EdgeState};
pub use router::{CapabilityProbe, build_edge_router};
pub use runtime::{serve, serve_io};
pub use wire::{
    EdgeOutcome, EdgeRequest, EdgeResponse, FALLTHROUGH_SENTINEL, FallthroughReason,
    SENSITIVE_HEADERS, WIRE_VERSION, WireError,
};

/// The thing that makes this crate portable is a *manifest* property, not a
/// source property: one `default-features = true` on `axum` would pull hyper
/// and tokio in and the capsule would stop compiling for `wasm32-wasip1` — with
/// an error pointing at a C-shaped dependency three levels down, not at the
/// line that caused it.
///
/// These tests read this crate's own manifest and fail on that edit directly.
/// The full graph assertion belongs in CI, where the target is installed:
///
/// ```sh
/// cargo build -p autumn-edge --target wasm32-wasip1
/// ! cargo tree -p autumn-edge --target wasm32-wasip1 | grep -Ei 'tokio|hyper|mio'
/// ```
#[cfg(test)]
mod manifest_guard {
    const MANIFEST: &str = include_str!("../Cargo.toml");

    /// Every `name = …` line in `[dependencies]`, comments dropped.
    fn dependency_lines() -> Vec<&'static str> {
        MANIFEST
            .lines()
            .skip_while(|line| line.trim() != "[dependencies]")
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with('['))
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn axum_keeps_its_default_features_off() {
        let axum = dependency_lines()
            .into_iter()
            .find(|line| line.starts_with("axum"))
            .expect("axum is a dependency");

        assert!(
            axum.contains("default-features = false"),
            "axum's default features pull in hyper and tokio, which cannot compile for \
             wasm32-wasip1: {axum}"
        );
    }

    #[test]
    fn no_native_only_runtime_is_a_dependency() {
        for line in dependency_lines() {
            let name = line.split('=').next().unwrap_or_default().trim();
            assert!(
                !matches!(
                    name,
                    "tokio" | "hyper" | "mio" | "tokio-util" | "hyper-util"
                ),
                "`{name}` cannot compile for wasm32-wasip1; the edge crate must stay free of it"
            );
        }
    }

    #[test]
    fn wasmi_is_optional_so_a_capsule_never_links_its_own_interpreter() {
        let wasmi = dependency_lines()
            .into_iter()
            .find(|line| line.starts_with("wasmi"))
            .expect("wasmi is a dependency");

        assert!(wasmi.contains("optional = true"), "{wasmi}");
        assert!(
            MANIFEST.contains(r#"host = ["dep:wasmi"]"#),
            "the `host` feature must be the only thing that enables wasmi"
        );
    }
}
