//! Entropy sources that make generated identifiers reproducible across a
//! capture/replay pair (#1634, Phase 3).
//!
//! Randomness is an input exactly the way wall-clock time is: a handler that
//! mints a session id, a CSRF token, a request id or a job id behaves
//! differently on the next draw, and any value it wrote to the database is in
//! the capsule's SQL binds. So a capsule records every draw the request took
//! and replay serves them back in the same order — the same discipline
//! [`clock`](crate::capsule::clock) applies to `now()`.
//!
//! Recording the *drawn bytes* rather than a seed is deliberate: production
//! runs on [`OsEntropy`](crate::entropy::OsEntropy), which has no seed to
//! record, and a re-seeded stream would mint different UUIDs than the ones the
//! recorded database traffic was bound with.

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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::capsule::capture::current_scope;
use crate::entropy::Entropy;

/// Wraps the application's entropy source and tees every draw into the capture
/// scope of the request that took it.
///
/// Installed over the configured source when `[failure_capture] enabled =
/// true`; draws taken outside a request (boot, schedulers, jobs) pass straight
/// through because no scope is active on those tasks.
#[derive(Debug)]
pub struct RecordingEntropy {
    inner: Arc<dyn Entropy>,
}

impl RecordingEntropy {
    /// Wrap an existing entropy source.
    #[must_use]
    pub const fn new(inner: Arc<dyn Entropy>) -> Self {
        Self { inner }
    }
}

impl Entropy for RecordingEntropy {
    fn next_u64(&self) -> u64 {
        let drawn = self.inner.next_u64();
        if let Some(scope) = current_scope() {
            // Recorded in the same little-endian spelling `next_u64` is
            // reconstructed from on replay, so the round trip is exact.
            scope.record_random(drawn.to_le_bytes().to_vec());
        }
        drawn
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        self.inner.fill_bytes(dest);
        if let Some(scope) = current_scope() {
            scope.record_random(dest.to_vec());
        }
    }

    fn unrecorded(&self) -> Option<Arc<dyn Entropy>> {
        Some(Arc::clone(&self.inner))
    }
}

/// Serves the draws a capsule recorded, in order.
///
/// A replayed handler that draws *more* than the recording did gets zero bytes
/// and the over-draw is counted, so the verdict can warn rather than silently
/// reaching for real randomness — which would make the run non-deterministic
/// and defeat the point. Draws *fewer* than the recording are counted too: a
/// branch that minted an identifier in production and no longer does is a
/// change the operator needs to see.
///
/// Only draws made inside
/// [`with_replay_request_scope`](crate::capsule::clock::with_replay_request_scope)
/// consume the queue, mirroring the capture side, where only scope-carrying
/// draws were recorded.
#[derive(Debug)]
pub struct ReplayEntropy {
    draws: Mutex<VecDeque<Vec<u8>>>,
    over_draws: AtomicUsize,
    /// Draws consumed so far, for the position a width mismatch reports.
    consumed: AtomicUsize,
    /// `(position, recorded width, requested width)` of each draw whose width
    /// differed from the recording (#2351 item 11).
    width_mismatches: Mutex<Vec<(usize, usize, usize)>>,
}

impl ReplayEntropy {
    /// Build an entropy source from a capsule's recorded draws.
    #[must_use]
    pub fn new(draws: Vec<Vec<u8>>) -> Self {
        Self {
            draws: Mutex::new(draws.into()),
            over_draws: AtomicUsize::new(0),
            consumed: AtomicUsize::new(0),
            width_mismatches: Mutex::new(Vec::new()),
        }
    }

    /// Each draw whose width differed from the recording, as
    /// `(position, recorded width, requested width)`.
    #[must_use]
    pub fn width_mismatches(&self) -> Vec<(usize, usize, usize)> {
        self.width_mismatches
            .lock()
            .map(|mismatches| mismatches.clone())
            .unwrap_or_default()
    }

    /// How many draws the replayed run made past the end of the recording.
    #[must_use]
    pub fn over_draws(&self) -> usize {
        self.over_draws.load(Ordering::SeqCst)
    }

    /// How many recorded draws the replayed run never took.
    #[must_use]
    pub fn unconsumed(&self) -> usize {
        self.draws.lock().map_or(0, |draws| draws.len())
    }

    /// The next recorded draw, or `None` when the tape is exhausted or this
    /// task is not the replayed request.
    ///
    /// `width` is the number of bytes the caller asked for. A recorded draw of
    /// another width is still consumed, so later draws stay aligned, and the
    /// mismatch is kept for the verdict.
    fn next_draw(&self, width: usize) -> Option<Vec<u8>> {
        if !crate::capsule::clock::in_replay_request() {
            return None;
        }
        let mut draws = self.draws.lock().ok()?;
        let Some(bytes) = draws.pop_front() else {
            drop(draws);
            self.over_draws.fetch_add(1, Ordering::SeqCst);
            return None;
        };
        drop(draws);
        let position = self.consumed.fetch_add(1, Ordering::SeqCst);
        if bytes.len() != width
            && let Ok(mut mismatches) = self.width_mismatches.lock()
        {
            mismatches.push((position, bytes.len(), width));
        }
        Some(bytes)
    }
}

impl Entropy for ReplayEntropy {
    fn next_u64(&self) -> u64 {
        let Some(bytes) = self.next_draw(8) else {
            return 0;
        };
        let mut buf = [0u8; 8];
        let take = bytes.len().min(8);
        if let (Some(head), Some(source)) = (buf.get_mut(..take), bytes.get(..take)) {
            head.copy_from_slice(source);
        }
        u64::from_le_bytes(buf)
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        // A zero fill is the honest answer for an exhausted tape: it is
        // deterministic, it is obviously not real randomness in a debugger,
        // and the over-draw counter makes the verdict say so out loud.
        dest.fill(0);
        let Some(bytes) = self.next_draw(dest.len()) else {
            return;
        };
        let take = dest.len().min(bytes.len());
        if let (Some(head), Some(source)) = (dest.get_mut(..take), bytes.get(..take)) {
            head.copy_from_slice(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsule::capture::{CaptureScope, CaptureSettings};
    use crate::capsule::clock::with_replay_request_scope;

    fn scope() -> Arc<CaptureScope> {
        Arc::new(CaptureScope::new(
            "entropy-test".to_owned(),
            Arc::new(CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ))
    }

    #[tokio::test]
    async fn a_recorded_uuid_replays_byte_for_byte() {
        let scope = scope();
        let recording = RecordingEntropy::new(Arc::new(crate::entropy::OsEntropy));
        let minted = crate::capsule::capture::with_capture_scope(Arc::clone(&scope), async {
            recording.uuid_v4()
        })
        .await;

        let draws: Vec<Vec<u8>> = scope
            .effects_snapshot()
            .random
            .into_iter()
            .map(|draw| draw.bytes)
            .collect();
        assert_eq!(draws.len(), 1, "a v4 UUID is exactly one 16-byte draw");

        let replay = ReplayEntropy::new(draws);
        let replayed = with_replay_request_scope(async { replay.uuid_v4() }).await;
        assert_eq!(
            replayed, minted,
            "the identifier the failing request minted must reappear on replay"
        );
        assert_eq!(replay.over_draws(), 0);
        assert_eq!(replay.unconsumed(), 0);
    }

    /// Issue #3054: framework draws that replay does not make again must not
    /// reach the tape.
    #[tokio::test]
    async fn unrecorded_draws_stay_off_the_tape() {
        let scope = scope();
        let recording = RecordingEntropy::new(Arc::new(crate::entropy::OsEntropy));
        let unrecorded = recording
            .unrecorded()
            .expect("a recording has an inner source");
        crate::capsule::capture::with_capture_scope(Arc::clone(&scope), async {
            let _ = unrecorded.next_u64();
            let _ = unrecorded.uuid_v4();
        })
        .await;
        assert!(scope.effects_snapshot().random.is_empty());
        assert!(crate::entropy::OsEntropy.unrecorded().is_none());
    }

    #[tokio::test]
    async fn drawing_past_the_recording_is_counted_not_faked() {
        let replay = ReplayEntropy::new(Vec::new());
        let value = with_replay_request_scope(async { replay.next_u64() }).await;
        assert_eq!(
            value, 0,
            "an exhausted tape must not reach for real entropy"
        );
        assert_eq!(replay.over_draws(), 1);
    }

    #[tokio::test]
    async fn draws_outside_the_replayed_request_do_not_consume_the_tape() {
        // Symmetric with the clock: work the handler spawned carried no
        // capture scope when the failure was recorded, so it must not eat
        // draws the recorded handler is still owed.
        let replay = ReplayEntropy::new(vec![vec![7u8; 16]]);
        let _ = replay.uuid_v4();
        assert_eq!(replay.unconsumed(), 1);
        assert_eq!(
            replay.over_draws(),
            0,
            "an out-of-scope draw is not an over-draw either"
        );
    }
}
