// autumn-panic-gate: request-path module — production code path must be panic-free.
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
//! Apply a [`BillingEvent`] to the mirror, once.
//!
//! Sequence: claim the event id in the ledger → upsert snapshots (the store
//! applies the ordering guard) → open or close dunning → notify → mark
//! applied. A failure after the claim releases it, so the provider's
//! redelivery is applied again; every step is idempotent.

use autumn_web::AppState;

use crate::BillingService;
use crate::error::BillingError;
use crate::event::BillingEvent;

/// What `apply` did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReconcileOutcome {
    /// The event changed, or confirmed, mirror state.
    Applied {
        /// Ledger label of the event kind.
        kind: &'static str,
    },
    /// The event id was already applied. Nothing changed.
    Duplicate,
    /// The provider event type is not mirrored. Recorded only.
    Ignored {
        /// Provider event type.
        event_type: String,
    },
}

/// Apply one event.
///
/// # Errors
///
/// Returns the store or hook error. The ledger claim is released first.
pub async fn apply(
    state: &AppState,
    service: &BillingService,
    event: BillingEvent,
) -> Result<ReconcileOutcome, BillingError> {
    let _ = (state, service, event);
    Err(BillingError::Unsupported("reconcile"))
}
