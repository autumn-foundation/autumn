//! H4: one report at a time.

use tokio::sync::SemaphorePermit;

/// The layer for `#[intercept(ReportGate)]`.
#[derive(Clone, Copy, Debug)]
pub struct ReportGate;

impl<S> tower::Layer<S> for ReportGate {
    type Service = S;

    fn layer(&self, _inner: S) -> S {
        todo!("H4")
    }
}

/// Take the only report slot. Tests use it to fill the gate.
#[must_use]
pub fn hold_report_slot() -> Option<SemaphorePermit<'static>> {
    todo!("H4")
}
