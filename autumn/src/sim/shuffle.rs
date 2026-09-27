//! The interleaving shuffler (issue #2967).
//!
//! Tokio polls ready tasks in a fixed order, so a sim explores timer and fault
//! orders but always the same task-poll interleaving. The shuffler changes
//! that order from the seed:
//!
//! - [`Sim::interleave`](crate::sim::Sim::interleave) runs futures
//!   concurrently. Each round it polls them in a seeded order, and it can hold
//!   one back for a round.
//! - [`Sim::spawn`](crate::sim::Sim::spawn) spawns a task that can yield
//!   before a poll, which moves it behind the other ready tasks.
//!
//! Every decision comes from a stream seeded from the sim seed and a per-call
//! counter, so the same seed replays the same interleaving.
//!
//! # Scope
//!
//! The shuffler reorders the futures and tasks given to it. A request sent
//! through the test client runs its handler inside the future, so handler
//! awaits are reordered too. Tasks the framework spawns itself (job workers,
//! scheduled loops) keep tokio's order.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::entropy::{Entropy, SeededEntropy};

/// Salt `XOR`ed into the sim seed for the shuffler stream, so it is
/// independent of the app entropy, chaos and crash streams.
pub const SHUFFLE_STREAM_SALT: u64 = 0x5_11F7_1E5E_ED00_u64;

/// One in this many poll decisions holds a future back for a round.
const SKIP_ONE_IN: u64 = 4;

/// A future is held back at most this many rounds in a row, so it always
/// makes progress.
const MAX_CONSECUTIVE_SKIPS: u8 = 3;

/// The seeded decision stream for one shuffler call.
pub fn stream(seed: u64, call: u64) -> Arc<dyn Entropy> {
    let derived = SeededEntropy::new(seed ^ SHUFFLE_STREAM_SALT).derive_uuid(call.to_le_bytes());
    let bytes: [u8; 8] = derived.as_bytes()[..8]
        .try_into()
        .expect("a uuid has 16 bytes");
    SeededEntropy::shared(u64::from_le_bytes(bytes))
}

/// Seeded skip decision with a cap on consecutive skips.
fn should_skip(rng: &dyn Entropy, skips: &mut u8) -> bool {
    if *skips < MAX_CONSECUTIVE_SKIPS && rng.next_u64().is_multiple_of(SKIP_ONE_IN) {
        *skips += 1;
        true
    } else {
        *skips = 0;
        false
    }
}

/// Runs futures concurrently and polls them in a seeded order.
pub struct Interleave<F: Future> {
    ops: Vec<Option<Pin<Box<F>>>>,
    outputs: Vec<Option<F::Output>>,
    skips: Vec<u8>,
    rng: Arc<dyn Entropy>,
}

impl<F: Future> Interleave<F> {
    pub fn new(ops: Vec<F>, rng: Arc<dyn Entropy>) -> Self {
        let len = ops.len();
        Self {
            ops: ops.into_iter().map(|op| Some(Box::pin(op))).collect(),
            outputs: (0..len).map(|_| None).collect(),
            skips: vec![0; len],
            rng,
        }
    }
}

// Every field is `Unpin`: the ops are boxed, and outputs are moved out only
// when ready.
impl<F: Future> Unpin for Interleave<F> {}

impl<F: Future> Future for Interleave<F> {
    type Output = Vec<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut order: Vec<usize> = (0..this.ops.len())
            .filter(|&index| this.ops[index].is_some())
            .collect();
        // Fisher-Yates shuffle from the seeded stream.
        for last in (1..order.len()).rev() {
            let pick = usize::try_from(this.rng.next_u64() % (last as u64 + 1))
                .expect("pick is at most `last`, which is a usize");
            order.swap(last, pick);
        }
        let mut skipped = false;
        for index in order {
            if should_skip(this.rng.as_ref(), &mut this.skips[index]) {
                skipped = true;
                continue;
            }
            let Some(op) = this.ops[index].as_mut() else {
                continue;
            };
            if let Poll::Ready(output) = op.as_mut().poll(cx) {
                this.outputs[index] = Some(output);
                this.ops[index] = None;
            }
        }
        if this.ops.iter().all(Option::is_none) {
            let outputs = this
                .outputs
                .iter_mut()
                .map(|output| output.take().expect("every op finished"))
                .collect();
            return Poll::Ready(outputs);
        }
        if skipped {
            // A held-back op was not polled, so nothing may wake us for it.
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

/// A task that can yield before a poll, from the seeded stream.
pub struct Shuffled<F> {
    op: Pin<Box<F>>,
    skips: u8,
    rng: Arc<dyn Entropy>,
}

impl<F> Shuffled<F> {
    pub fn new(op: F, rng: Arc<dyn Entropy>) -> Self {
        Self {
            op: Box::pin(op),
            skips: 0,
            rng,
        }
    }
}

impl<F: Future> Future for Shuffled<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if should_skip(this.rng.as_ref(), &mut this.skips) {
            // Yield: requeue behind the other ready tasks.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        this.op.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_CONSECUTIVE_SKIPS, should_skip, stream};

    #[test]
    fn skips_are_capped() {
        let rng = stream(0, 0);
        let mut skips = 0;
        let mut run = 0;
        for _ in 0..10_000 {
            if should_skip(rng.as_ref(), &mut skips) {
                run += 1;
                assert!(run <= MAX_CONSECUTIVE_SKIPS, "a skip run stays capped");
            } else {
                run = 0;
            }
        }
    }

    #[test]
    fn streams_are_seed_and_call_deterministic() {
        assert_eq!(stream(7, 1).next_u64(), stream(7, 1).next_u64());
        assert_ne!(stream(7, 1).next_u64(), stream(7, 2).next_u64());
        assert_ne!(stream(7, 1).next_u64(), stream(8, 1).next_u64());
    }
}
