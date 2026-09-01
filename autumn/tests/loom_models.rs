//! Loom model-checks of three concurrency-sensitive production algorithms.
//!
//! This binary is EMPTY under a normal build (`#![cfg(loom_models)]`), so
//! `cargo test` compiles it instantly and runs nothing. It only compiles/runs
//! under `RUSTFLAGS="--cfg loom_models"`, in a dedicated CI job:
//!
//! ```text
//! RUSTFLAGS="--cfg loom_models" LOOM_MAX_PREEMPTIONS=3 \
//!   cargo test -p autumn-web --release --test loom_models
//! ```
//!
//! NOTE: a custom `loom_models` cfg is used deliberately instead of the usual
//! `--cfg loom`. loom's runtime always explores interleavings whenever a test
//! uses `loom::sync`/`loom::thread` directly (as these do, and as the existing
//! `chaos_job_client_loom` model does) — `--cfg loom` is only the std→loom
//! type-swap convention. Applying `--cfg loom` to this workspace's full
//! dependency tree breaks `hyper-util`/`tokio`, which carry their own
//! `#[cfg(loom)]` code paths. The neutral `loom_models` cfg gives us the
//! empty-in-normal-build gating without poisoning those dependencies.
//!
//! loom cannot see inside `std::sync` types, so — exactly like the existing
//! `chaos_job_client_loom` model — each test RE-IMPLEMENTS the core algorithm
//! with `loom::sync` primitives and asserts the invariant across every
//! interleaving loom explores. The data structures (`VecDeque`, `HashMap`) stay
//! std; only the SYNC primitives are loom's, which is what loom instruments.
#![cfg(loom_models)]

use std::collections::{HashMap, VecDeque};

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// Models the per-topic `ReplayBuffer` monotonic-id + bounded-cap ring buffer.
///
/// Mirrors `ReplayBuffer` / `send_without_publish_metric` in
/// `autumn/src/channels.rs:149-156,703-711`. Crucially, production locks the
/// replay buffer once PER MESSAGE (`state.replay.lock()` at channels.rs:703 is
/// re-acquired on every `send`), assigns `next_id`, `saturating_add`s it,
/// `push_back`s, and evicts over cap via `pop_front`. The snapshot half of
/// `resume_local` (`autumn/src/channels.rs:732-785`) reads the buffer under that
/// same per-message lock, so a resumer can win the lock BETWEEN two independent
/// publishes and legitimately observe a partial buffer.
///
/// Invariant: when two publishers race the shared `next_id` counter (each with
/// its own independent lock/assign/increment/push), the two assigned ids are
/// unique and dense — together exactly the set `{1, 2}`, with no duplicate id
/// (double-assign) and no gap (skipped counter). And any snapshot a concurrent
/// resumer observes is a contiguous, gap-free, duplicate-free run of ids, every
/// one of which is still retained in the final buffer — NOT all-or-nothing.
struct ReplayBuffer {
    next_id: u64,
    cap: usize,
    buf: VecDeque<(u64, u64)>, // (id, payload)
}

impl ReplayBuffer {
    fn new(cap: usize) -> Self {
        Self {
            next_id: 1,
            cap,
            buf: VecDeque::new(),
        }
    }

    /// Assign the next monotonic id, append, and evict past cap — the
    /// under-one-lock body of `send_without_publish_metric`.
    fn push(&mut self, payload: u64) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.buf.push_back((id, payload));
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
        id
    }

    /// Same-epoch resume snapshot: retained entries with `id > last_seen`, in
    /// ascending id order (`resume_local`'s `filter(|(seq, _)| *seq > ev.seq)`).
    fn snapshot_after(&self, last_seen: u64) -> Vec<u64> {
        self.buf
            .iter()
            .filter(|(id, _)| *id > last_seen)
            .map(|(id, _)| *id)
            .collect()
    }
}

#[test]
fn replay_ring_buffer_monotonic_ids() {
    loom::model(|| {
        // cap 2, two INDEPENDENT publishers. Each acquires the replay lock ON ITS
        // OWN, assigns next_id, increments, and pushes ONE message — mirroring the
        // per-message locking in `send_without_publish_metric` (channels.rs:703).
        // This is the real race `next_id` guards: two concurrent sends contending
        // for the same counter. loom explores every ordering of the two lock
        // acquisitions (and the resumer's).
        let replay = Arc::new(Mutex::new(ReplayBuffer::new(2)));

        let p1 = {
            let replay = replay.clone();
            thread::spawn(move || replay.lock().unwrap().push(10))
        };
        let p2 = {
            let replay = replay.clone();
            thread::spawn(move || replay.lock().unwrap().push(20))
        };

        // Concurrent resumer: snapshots whatever is buffered when it wins the lock
        // (channels.rs:732-785 snapshots under the same per-message lock a publish
        // holds). Because each publish locks independently, this can fall BETWEEN
        // the two pushes and observe a partial buffer — which is legal.
        let resumer = {
            let replay = replay.clone();
            thread::spawn(move || replay.lock().unwrap().snapshot_after(0))
        };

        let id1 = p1.join().unwrap();
        let id2 = p2.join().unwrap();
        let snap = resumer.join().unwrap();

        // next_id race invariant: the two ids the publishers were assigned are
        // DISTINCT and DENSE — together exactly {1, 2}. A duplicate (both saw the
        // same next_id) or a gap (a counter value skipped) would violate this.
        let mut ids = [id1, id2];
        ids.sort_unstable();
        assert_eq!(
            ids,
            [1, 2],
            "concurrent publishers must be assigned the unique, dense id set {{1, 2}}"
        );

        // Final buffer retains the same dense id set {1, 2} (cap 2, no eviction).
        let final_ids: Vec<u64> = {
            let replay = replay.lock().unwrap();
            replay.buf.iter().map(|(id, _)| *id).collect()
        };
        let mut sorted_final = final_ids.clone();
        sorted_final.sort_unstable();
        assert_eq!(
            sorted_final,
            vec![1, 2],
            "final buffer retains the dense id set {{1, 2}}"
        );

        // Snapshot invariant — the TRUE production invariant, NOT all-or-nothing.
        // A partial snapshot like [1] is LEGAL: the resumer can win the lock after
        // the first push but before the second, since each publish locks
        // independently. What must hold under EVERY interleaving is that the
        // snapshot is a contiguous, gap-free, duplicate-free run of ids, each of
        // which is retained in the final buffer.
        for w in snap.windows(2) {
            assert_eq!(
                w[1],
                w[0] + 1,
                "snapshot ids must be strictly contiguous (no gap, no duplicate): {snap:?}"
            );
        }
        for id in &snap {
            assert!(
                final_ids.contains(id),
                "snapshot id {id} must be retained in the final buffer {final_ids:?}"
            );
        }
    });
}

/// Models the `THROTTLE_REGISTRY` get-or-insert plus token-bucket consume.
///
/// Mirrors the registry get-or-insert in
/// `autumn/src/security/rate_limit.rs:1074-1076,1484-1498`
/// (`Mutex<HashMap<String, Arc<Limiter>>>` + `entry(..).or_insert_with(..)`) and
/// the token-bucket consume in `MemoryStore::decide`
/// (`autumn/src/security/rate_limit.rs:171-203`, `tokens -= 1.0` when
/// `tokens >= 1.0`).
///
/// Invariant: two threads racing the insert for the same key resolve to exactly
/// ONE shared bucket (`Arc::ptr_eq`, refcount reflects a single insert), and
/// concurrent consumes against that one bucket never let total consumed exceed
/// the burst capacity (no double-spend past capacity).
struct Bucket {
    /// Whole tokens remaining (integer model of the f64 token count).
    tokens: Mutex<u64>,
}

impl Bucket {
    fn new(burst: u64) -> Self {
        Self {
            tokens: Mutex::new(burst),
        }
    }

    /// `decide`'s consume step: take one token iff at least one remains.
    fn try_consume(&self) -> bool {
        let mut tokens = self.tokens.lock().unwrap();
        if *tokens >= 1 {
            *tokens -= 1;
            true
        } else {
            false
        }
    }
}

#[test]
fn rate_limit_bucket_single_winner() {
    loom::model(|| {
        // Registry starts empty; both threads race to insert key "r".
        let registry: Arc<Mutex<HashMap<&'static str, Arc<Bucket>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // burst = 1: capacity 1, so at most ONE consume may succeed. If the
        // get-or-insert ever handed out two distinct buckets, both consumes
        // would succeed and total would be 2 > capacity.
        let get_or_insert = |registry: &Arc<Mutex<HashMap<&'static str, Arc<Bucket>>>>| {
            let mut reg = registry.lock().unwrap();
            Arc::clone(reg.entry("r").or_insert_with(|| Arc::new(Bucket::new(1))))
        };

        let t1 = {
            let registry = registry.clone();
            thread::spawn(move || {
                let bucket = get_or_insert(&registry);
                let consumed = bucket.try_consume();
                (bucket, consumed)
            })
        };

        let t2 = {
            let registry = registry.clone();
            thread::spawn(move || {
                let bucket = get_or_insert(&registry);
                let consumed = bucket.try_consume();
                (bucket, consumed)
            })
        };

        let (b1, c1) = t1.join().unwrap();
        let (b2, c2) = t2.join().unwrap();

        // Exactly one shared bucket for the key: both threads observe the same
        // Arc, and the registry holds a single entry.
        assert!(
            Arc::ptr_eq(&b1, &b2),
            "both threads must resolve to the SAME shared bucket"
        );
        assert_eq!(registry.lock().unwrap().len(), 1, "one bucket per key");

        // No double-spend past capacity: burst 1 => at most one success, and the
        // shared bucket ends at 0 tokens.
        let succeeded = usize::from(c1) + usize::from(c2);
        assert_eq!(
            succeeded, 1,
            "exactly one consume may succeed against a burst-1 bucket"
        );
        assert_eq!(
            *b1.tokens.lock().unwrap(),
            0,
            "shared bucket must reflect exactly one consume"
        );
    });
}

/// Models the exactly-once `requests_active` decrement across the
/// `MetricsFuture` completion race.
///
/// A single in-flight request is counted once by `increment_active`
/// (`autumn/src/middleware/metrics.rs:586`). Its active count is then released by
/// ONE of two mutually-exclusive completion paths that both consult the same
/// `Option<MetricsCollector>` guard: the `Poll::Ready` arm records + decrements
/// (`metrics.rs:638-648`) and `PinnedDrop` decrements
/// (`metrics.rs:621-622`). Both claim ownership with `Option::take()`, so exactly
/// one of them sees `Some` and performs the decrement — the other sees `None` and
/// does nothing. This model reifies that guard as a shared `Mutex<Option<()>>`
/// and RACES the two paths against it.
///
/// Invariant: the decrement fires EXACTLY ONCE under every interleaving — never
/// twice (which would underflow the gauge past 0 -> corruption) and never zero
/// times (which would leak an active count forever). Starting the gauge at 1, it
/// must end at exactly 0.
///
/// Non-vacuity: setting `METRICS_LOOM_SHOW_BUG` swaps the atomic take-once claim
/// for a naive check-then-decrement (`is_some()` under one lock, decrement under
/// another). Both racers can then observe `Some` before either clears it, so both
/// decrement -> double-dec -> underflow, and loom finds the failing interleaving.
/// The default (no env var) take-once path passes across all interleavings.
#[test]
fn metrics_active_gauge_balance() {
    let show_bug = std::env::var_os("METRICS_LOOM_SHOW_BUG").is_some();
    loom::model(move || {
        // Gauge starts at 1: one in-flight request already counted by
        // increment_active (metrics.rs:586). The Ready and Drop paths now race to
        // finalize it; the gauge must land at exactly 0.
        let active = Arc::new(AtomicUsize::new(1));
        // The finalize guard. Production is the future's `Option<MetricsCollector>`
        // claimed via `Option::take()` in BOTH the Poll::Ready arm and PinnedDrop;
        // modeled here as a shared `Mutex<Option<()>>` whose `take()` hands the
        // decrement token to exactly one racer.
        let guard = Arc::new(Mutex::new(Some(())));

        // Finalize body shared by both paths: claim the guard, decrement iff won.
        fn finalize(show_bug: bool, active: &Arc<AtomicUsize>, guard: &Arc<Mutex<Option<()>>>) {
            if show_bug {
                // BUGGY: check-then-decrement WITHOUT an atomic claim. The
                // is_some() check and the clear happen under separate lock
                // acquisitions, so both racers can observe Some and both
                // decrement -> double-dec -> underflow past 0.
                let present = guard.lock().unwrap().is_some();
                if present {
                    active.fetch_sub(1, Ordering::Relaxed);
                    *guard.lock().unwrap() = None;
                }
            } else if guard.lock().unwrap().take().is_some() {
                // CORRECT: take() atomically hands Some to exactly one racer.
                active.fetch_sub(1, Ordering::Relaxed);
            }
        }

        // Ready path: future resolved (records + decrements iff it claims).
        let ready = {
            let active = active.clone();
            let guard = guard.clone();
            thread::spawn(move || finalize(show_bug, &active, &guard))
        };
        // Drop path: future dropped (decrements iff it claims the SAME guard).
        let dropper = {
            let active = active.clone();
            let guard = guard.clone();
            thread::spawn(move || finalize(show_bug, &active, &guard))
        };

        ready.join().unwrap();
        dropper.join().unwrap();

        // Exactly-once decrement: never twice (underflow past 0 -> gauge
        // corruption) and never zero times (leaked active count). The correct
        // take-once holds this under EVERY interleaving.
        assert_eq!(
            active.load(Ordering::Relaxed),
            0,
            "requests_active must be decremented exactly once (no leak, no underflow)"
        );
    });
}

#[test]
fn presence_event_ordering_race() {
    let show_bug = std::env::var_os("_LOOM_SHOW_BUG").is_some();

    loom::model(move || {
        // State represents the number of active connections for a user.
        // 1 means user is present, 0 means user has left.
        let state = Arc::new(Mutex::new(1));
        let events = Arc::new(Mutex::new(Vec::new()));

        let t1 = {
            let state = state.clone();
            let events = events.clone();
            thread::spawn(move || {
                // Thread 1: drop(PresenceHandle) for the LAST connection
                if show_bug {
                    // BUGGY: update state, release lock, then publish
                    let fully_removed = {
                        let mut st = state.lock().unwrap();
                        if *st == 1 {
                            *st = 0;
                            true
                        } else {
                            false
                        }
                    };
                    if fully_removed {
                        events.lock().unwrap().push("Leave");
                    }
                } else {
                    // CORRECT: publish while holding the state lock
                    let mut st = state.lock().unwrap();
                    if *st == 1 {
                        *st = 0;
                        events.lock().unwrap().push("Leave");
                    }
                }
            })
        };

        let t2 = {
            let state = state.clone();
            let events = events.clone();
            thread::spawn(move || {
                // Thread 2: track() a NEW connection
                if show_bug {
                    // BUGGY: update state, release lock, then publish
                    {
                        let mut st = state.lock().unwrap();
                        *st = 1;
                    }
                    events.lock().unwrap().push("Join");
                } else {
                    // CORRECT: publish while holding the state lock
                    let mut st = state.lock().unwrap();
                    *st = 1;
                    events.lock().unwrap().push("Join");
                }
            })
        };

        t1.join().unwrap();
        t2.join().unwrap();

        let final_state = *state.lock().unwrap();
        let evs = events.lock().unwrap();

        // The sequence of events must be consistent with the final state.
        if !evs.is_empty() {
            let last_event = evs.last().unwrap();
            if final_state == 1 {
                assert_eq!(*last_event, "Join", "state is 1 (present) but last event is {}", last_event);
            } else {
                assert_eq!(*last_event, "Leave", "state is 0 (absent) but last event is {}", last_event);
            }
        }
    });
}
