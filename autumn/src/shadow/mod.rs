//! Live-traffic shadow mirroring and primary-vs-shadow response diffing
//! (issue #1653).
//!
//! Canary, blue/green, and rolling deploys all answer the same question — *are
//! the aggregate metrics still healthy?* — and all of them route **real**
//! traffic to the new build to answer it. None of them catches the regression
//! that returns `200 OK` with a dropped field, a reordered list, or an
//! off-by-one total, because nothing ever compares two responses to the same
//! request. This module does.
//!
//! ```text
//!                        ┌──────────────────┐
//!   client ──request──▶  │  live build      │ ──response──▶ client
//!                        └────────┬─────────┘        │
//!                                 │ (copy)           │ (tee)
//!                        ┌────────▼─────────┐   ┌────▼─────┐
//!                        │ candidate build  │──▶│  differ  │──▶ /actuator/shadow
//!                        └──────────────────┘   └──────────┘        + metrics
//! ```
//!
//! The candidate's response reaches the differ and nothing else. It is never a
//! `Response`, never touches the primary's state, and never delays the client.
//!
//! # Scope of this slice
//!
//! Idempotent (`GET`/`HEAD`) traffic only, against an operator-provided shadow
//! target. Mirroring mutating methods requires virtualizing the candidate's
//! effects — DB writes, outbound HTTP, `#[job]` enqueues — which is the
//! deliberate follow-up slice. The method allowlist is therefore a constant,
//! not a config key.
//!
//! # Wiring
//!
//! Configured through [`ShadowConfig`](crate::shadow::ShadowConfig) (`[shadow]` in
//! `autumn.toml`) and
//! assembled into the ingress stack by the framework router; nothing here needs
//! to be called by hand. See `docs/guide/staged-deploys.md`.

// Private modules behind a curated re-export list, matching
// `crate::middleware`: the module split is an implementation detail, and
// everything below is deliberate public surface. `transport` stays public
// because a third party may want to implement [`ShadowTransport`] against a
// non-HTTP candidate.
pub(crate) mod config;
pub(crate) mod diff;
pub(crate) mod layer;
pub(crate) mod registry;
pub(crate) mod sample;
pub mod transport;

pub use config::ShadowConfig;
pub use diff::{
    Comparison, Divergence, DivergenceKind, NormalizedBody, ResponseFacts, TRUNCATION_MARKER,
    compare, normalize_body, redact_path_and_query, status_class,
};
pub use layer::{
    COMPARISONS_METRIC, DIVERGENCES_METRIC, MirrorSettings, ShadowMirrorLayer, ShadowMirrorService,
};
pub use registry::{
    DivergenceRecord, LabelledCount, OVERFLOW_ROUTE_LABEL, Recorded, RequestContext,
    ShadowRegistry, ShadowSnapshot, ShadowStats,
};
pub use sample::{
    MIRRORABLE_METHODS, MirrorDecision, MirrorSelector, SHADOW_HEADER, SHADOW_HEADER_VALUE,
    SkipReason, roll_from,
};
#[cfg(feature = "http-client")]
pub use transport::HttpShadowTransport;
pub use transport::{ShadowError, ShadowFuture, ShadowRequest, ShadowTransport};

/// What `{actuator-prefix}/shadow` needs to report on a mirror run: the
/// registry plus the two facts that live in config rather than in it.
///
/// Installed into [`crate::AppState`]'s runtime extension map when the mirror
/// layer is assembled, so a replica with mirroring switched off simply has no
/// handle and the endpoint reports a disabled mirror.
#[derive(Clone, Debug)]
pub struct ShadowHandle {
    /// Shared counters and the bounded divergence ring.
    pub registry: ShadowRegistry,
    /// Whether mirroring is switched on for this replica.
    pub enabled: bool,
    /// The configured candidate target.
    pub target: Option<String>,
}

impl ShadowHandle {
    /// Build the payload the actuator endpoint publishes.
    #[must_use]
    pub fn snapshot(&self) -> ShadowSnapshot {
        self.registry.snapshot(self.enabled, self.target.as_deref())
    }

    /// The payload for a replica that never assembled a mirror.
    #[must_use]
    pub fn disabled_snapshot() -> ShadowSnapshot {
        ShadowSnapshot::disabled()
    }

    /// A handle for a replica that configured `[shadow]` but could not assemble
    /// a mirror — today, a build without the `http-client` feature, which has
    /// no way to dial the candidate.
    ///
    /// Installed so the actuator can distinguish "configured but inert here"
    /// from "never configured": both report `enabled: false`, but only this one
    /// reports the target the operator asked for.
    #[must_use]
    pub fn inactive(target: impl Into<String>) -> Self {
        Self {
            registry: ShadowRegistry::new(1),
            enabled: false,
            target: Some(target.into()),
        }
    }
}
