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
//! Configured through [`ShadowConfig`] (`[shadow]` in `autumn.toml`) and
//! assembled into the ingress stack by the framework router; nothing here needs
//! to be called by hand. See `docs/guide/staged-deploys.md`.

pub mod config;
pub mod diff;
pub mod layer;
pub mod registry;
pub mod sample;
pub mod transport;

pub use config::ShadowConfig;
pub use diff::{Comparison, Divergence, DivergenceKind, ResponseFacts, compare};
pub use layer::{MirrorSettings, ShadowMirrorLayer};
pub use registry::{DivergenceRecord, RequestContext, ShadowRegistry, ShadowSnapshot, ShadowStats};
pub use sample::{MirrorDecision, MirrorSelector, SHADOW_HEADER, SHADOW_HEADER_VALUE, SkipReason};
pub use transport::{ShadowError, ShadowRequest, ShadowTransport};

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
        ShadowSnapshot {
            enabled: false,
            target: None,
            stats: ShadowStats::default(),
            divergences: Vec::new(),
        }
    }
}
