//! The blocking-work gate of the sim runtime (issue #3067).
//!
//! A `SQLite` query in the sim runs on a tokio blocking thread. Without a gate,
//! the query can end while the runtime thread still runs other tasks. The task
//! that waits for the query then sees the result at once on some runs and at
//! its next poll on others. The task order, and with it the trace, changes from
//! run to run.
//!
//! The gate removes that race. On a runtime from [`runtime`], a gated
//! operation starts only in a park of the runtime thread that began after the
//! last gated operation ended. The runtime thread parks only when no task is
//! ready. So each operation runs while all tasks wait, and its result is seen
//! at the same point of every run.
//!
//! Gated work: each query and connection open of the sim's `SQLite`
//! substrate, each query a request-path `Db` makes, and each
//! `time::spawn_blocking` call. While no sim is alive, and while
//! `Sim::advance` or `Sim::run_to_idle` runs, gated work does not wait. On
//! any other runtime the gate does nothing.

// Only `SQLite` connections take turns at the gate. A build without `sqlite`
// keeps the runtime and its hooks, and not the turn logic.
#![cfg_attr(not(feature = "sqlite"), allow(dead_code))]

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// How long a gated operation waits for the runtime to park before it runs
/// anyway. The runtime thread does not park while it waits on the operation
/// itself: diesel-async waits that way for a query in flight when the runtime
/// drops its tasks. Each give-up is counted, because the order of that run is
/// not sure to replay.
const GIVE_UP: Duration = Duration::from_secs(2);

/// When the runtime thread parks, and which park gated work may use next.
#[derive(Default)]
pub struct Gate {
    state: Mutex<GateState>,
    turn: Condvar,
}

#[derive(Default)]
struct GateState {
    /// The runtime thread is parked: no task is ready.
    parked: bool,
    /// How many times the runtime thread parked.
    parks: u64,
    /// The park in which the last gated operation ended. The next one waits
    /// for a later park: only then has the runtime seen the result.
    done_in: u64,
    /// One gated operation already ran in this park.
    used: bool,
    /// The sims alive on this runtime. With none, the runtime drops what is
    /// left of their apps, and gated work does not wait.
    sims: u32,
    /// While above zero, gated work does not wait: `Sim::advance` and
    /// `Sim::run_to_idle` run, which yield and never park.
    open: u32,
    /// How many operations ran after the wait gave up.
    give_ups: u64,
}

impl Gate {
    fn on_park(&self) {
        let mut state = self.lock();
        state.parked = true;
        state.parks += 1;
        state.used = false;
        drop(state);
        self.turn.notify_all();
    }

    fn on_unpark(&self) {
        self.lock().parked = false;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until the runtime is parked, in a park that began after the last
    /// operation ended and that no other operation used. Then use it.
    fn take_turn(&self) {
        let mut state = self.lock();
        let deadline = std::time::Instant::now() + GIVE_UP;
        while !(state.parked && !state.used && state.parks > state.done_in)
            && state.sims > 0
            && state.open == 0
        {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                state.give_ups += 1;
                tracing::warn!(
                    "sim gate: the runtime did not park for {GIVE_UP:?}; a query runs \
                     without its turn (a task dropped a query in flight?)"
                );
                break;
            }
            state = self
                .turn
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.used = true;
    }

    fn end_turn(&self) {
        let mut state = self.lock();
        state.done_in = state.parks;
    }
}

thread_local! {
    /// The gate of the runtime that owns this blocking thread, if any.
    static GATE: RefCell<Option<Arc<Gate>>> = const { RefCell::new(None) };
    /// The gate of the sim runtime this thread runs, if any.
    static RUNS: RefCell<Option<Arc<Gate>>> = const { RefCell::new(None) };
    /// How deep this thread is in gated work. Only the outermost entry waits.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Start gated work on this thread. Call [`exit`] when it ends.
pub fn enter() {
    let depth = DEPTH.with(Cell::get);
    if depth == 0 {
        let gate = GATE.with(|gate| gate.borrow().clone());
        if let Some(gate) = gate {
            gate.take_turn();
        }
    }
    DEPTH.with(|cell| cell.set(depth.saturating_add(1)));
}

/// End gated work that [`enter`] started.
pub fn exit() {
    let depth = DEPTH.with(|depth| {
        let left = depth.get().saturating_sub(1);
        depth.set(left);
        left
    });
    if depth == 0
        && let Some(gate) = GATE.with(|gate| gate.borrow().clone())
    {
        gate.end_turn();
    }
}

/// The sims alive on this thread's sim runtime.
#[cfg(test)]
fn live_sims() -> u32 {
    runtime_gate().map_or(0, |gate| gate.lock().sims)
}

fn runtime_gate() -> Option<Arc<Gate>> {
    RUNS.with(|gate| gate.borrow().clone())
}

/// How many gated operations on this thread's sim runtime ran without their
/// turn, after the wait gave up. A run with any is not sure to replay.
pub fn give_ups() -> u64 {
    runtime_gate().map_or(0, |gate| gate.lock().give_ups)
}

/// A sim's place in the live count of its sim runtime.
///
/// A sim built inside its runtime takes its place at once. A sim built before
/// its runtime takes it when it is anchored. When the last sim leaves, gated
/// work does not wait, so the runtime can drop the tasks of the sims' apps.
#[derive(Default)]
pub struct SimSeat(Mutex<Option<Arc<Gate>>>);

impl SimSeat {
    /// Count the sim on this thread's sim runtime. A second call does nothing.
    pub fn take(&self) {
        let mut seat = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if seat.is_none()
            && let Some(gate) = runtime_gate()
        {
            gate.lock().sims += 1;
            *seat = Some(gate);
        }
    }

    /// Remove the sim from the count it took. A second call does nothing.
    pub fn leave(&self) {
        let gate = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(gate) = gate {
            let mut state = gate.lock();
            state.sims = state.sims.saturating_sub(1);
            drop(state);
            gate.turn.notify_all();
        }
    }
}

impl Drop for SimSeat {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Gated work runs without a turn while the value lives. `Sim::advance` and
/// `Sim::run_to_idle` hold one: they yield, and the runtime does not park
/// while they run. Database work then keeps the order of its finish, as it
/// did before the gate. Use `Sim::run_for` for a fixed order.
pub struct Open(Option<Arc<Gate>>);

impl Open {
    pub fn new() -> Self {
        let gate = runtime_gate();
        if let Some(gate) = &gate {
            gate.lock().open += 1;
            gate.turn.notify_all();
        }
        Self(gate)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        if let Some(gate) = &self.0 {
            let mut state = gate.lock();
            state.open = state.open.saturating_sub(1);
        }
    }
}

/// Gated work for the lifetime of the value.
pub struct Scope(());

impl Scope {
    pub fn enter() -> Self {
        enter();
        Self(())
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        exit();
    }
}

/// The gate's turn for the statement a connection runs.
///
/// Diesel can emit `StartQuery` and then skip `FinishQuery` when a bind fails.
/// So a new start first ends the turn the last start left open. A turn does
/// not change the thread's gated depth: a turn left open on a dropped
/// connection then does not let later work skip the gate.
#[derive(Debug, Default)]
pub struct QueryTurn {
    took: bool,
}

impl QueryTurn {
    pub fn start(&mut self) {
        self.finish();
        // Inside other gated work (a `Scope`), the turn is taken already.
        if DEPTH.with(Cell::get) == 0
            && let Some(gate) = GATE.with(|gate| gate.borrow().clone())
        {
            gate.take_turn();
            self.took = true;
        }
    }

    pub fn finish(&mut self) {
        if std::mem::take(&mut self.took)
            && let Some(gate) = GATE.with(|gate| gate.borrow().clone())
        {
            gate.end_turn();
        }
    }
}

/// Diesel instrumentation that gates each query of a connection.
#[cfg(feature = "db")]
#[derive(Default)]
pub struct GateInstrumentation(QueryTurn);

#[cfg(feature = "db")]
impl diesel::connection::Instrumentation for GateInstrumentation {
    fn on_connection_event(&mut self, event: diesel::connection::InstrumentationEvent<'_>) {
        use diesel::connection::InstrumentationEvent;
        match event {
            InstrumentationEvent::StartQuery { .. } => self.0.start(),
            InstrumentationEvent::FinishQuery { .. } => self.0.finish(),
            _ => {}
        }
    }
}

/// The runtime a deterministic sim needs (issue #3067).
///
/// Current-thread, with tokio's clock paused, one blocking thread, and the
/// blocking-work gate. `#[sim_test]` runs on it. Build your own sim runtime
/// with it when you do not use `#[sim_test]`.
///
/// # Errors
///
/// Returns the error of [`tokio::runtime::Builder::build`].
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let gate = Arc::new(Gate::default());
    // The thread that builds the runtime runs it (`#[sim_test]` does both).
    // The park hook sets it again, in case another thread runs it.
    RUNS.with(|cell| *cell.borrow_mut() = Some(Arc::clone(&gate)));
    let on_start = Arc::clone(&gate);
    let on_unpark = Arc::clone(&gate);
    let on_park = gate;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        // Blocking threads learn their gate. The runtime thread is the caller
        // of `block_on`, so it has none, and its own work is never gated.
        .on_thread_start(move || {
            GATE.with(|cell| *cell.borrow_mut() = Some(Arc::clone(&on_start)));
        })
        .on_thread_park(move || {
            RUNS.with(|cell| {
                let mut cell = cell.borrow_mut();
                if !cell
                    .as_ref()
                    .is_some_and(|gate| Arc::ptr_eq(gate, &on_park))
                {
                    *cell = Some(Arc::clone(&on_park));
                }
            });
            on_park.on_park();
        })
        .on_thread_unpark(move || on_unpark.on_unpark())
        .build()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{Scope, SimSeat, runtime};

    /// Gated work ends only while every task waits, so a fast blocking result
    /// never jumps ahead of local work.
    #[test]
    fn sim_gate_gated_work_ends_after_local_work() {
        for _ in 0..20 {
            let order = Arc::new(Mutex::new(Vec::new()));
            let rt = runtime().unwrap();
            let seat = SimSeat::default();
            seat.take();
            let seen = Arc::clone(&order);
            rt.block_on(async move {
                let log = Arc::clone(&seen);
                let blocking = tokio::task::spawn_blocking(move || {
                    let _gate = Scope::enter();
                    log.lock().unwrap().push("blocking");
                });
                for _ in 0..3 {
                    seen.lock().unwrap().push("local");
                    tokio::task::yield_now().await;
                }
                blocking.await.unwrap();
            });
            seat.leave();
            assert_eq!(
                *order.lock().unwrap(),
                ["local", "local", "local", "blocking"]
            );
        }
    }

    /// Each park lets one gated operation through, and nested gated work in
    /// it does not wait again.
    #[test]
    fn sim_gate_one_operation_per_park_and_nesting_passes() {
        let rt = runtime().unwrap();
        let seat = SimSeat::default();
        seat.take();
        let done = Arc::new(AtomicUsize::new(0));
        rt.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..4 {
                let done = Arc::clone(&done);
                handles.push(tokio::task::spawn_blocking(move || {
                    let _outer = Scope::enter();
                    let _inner = Scope::enter();
                    done.fetch_add(1, Ordering::SeqCst);
                }));
            }
            for handle in handles {
                handle.await.unwrap();
            }
        });
        seat.leave();
        assert_eq!(done.load(Ordering::SeqCst), 4);
    }

    /// With no sim alive, and while a drain holds `Open`, gated work does not
    /// wait for a park.
    #[test]
    fn sim_gate_does_not_wait_with_no_sim_or_while_open() {
        let rt = runtime().unwrap();
        // No sim: the blocking work runs at once, even while local work spins.
        rt.block_on(async {
            let handle = tokio::task::spawn_blocking(|| {
                let _gate = Scope::enter();
            });
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        });
        let seat = SimSeat::default();
        seat.take();
        rt.block_on(async {
            let _open = super::Open::new();
            let handle = tokio::task::spawn_blocking(|| {
                let _gate = Scope::enter();
            });
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        });
        seat.leave();
    }

    /// Diesel can skip `FinishQuery` after a bind error. A turn left open
    /// must not leave this thread inside gated work, or every later
    /// operation on the one blocking thread skips the gate.
    #[test]
    fn sim_gate_a_query_turn_left_open_does_not_leak() {
        let mut turn = super::QueryTurn::default();
        turn.start();
        drop(turn);
        assert_eq!(super::DEPTH.with(std::cell::Cell::get), 0);
    }

    /// A sim built before its runtime counts once it is anchored, and its
    /// drop removes only its own count.
    #[test]
    fn sim_gate_counts_a_sim_built_before_its_runtime_once_anchored() {
        let sim = crate::sim::Sim::from_seed(1);
        let rt = runtime().unwrap();
        rt.block_on(async {
            sim.anchor();
            assert_eq!(super::live_sims(), 1, "the anchored sim counts");
            sim.anchor();
            assert_eq!(super::live_sims(), 1, "a second anchor counts nothing");
            let inner = crate::sim::Sim::from_seed(2);
            assert_eq!(super::live_sims(), 2);
            drop(inner);
            assert_eq!(super::live_sims(), 1);
            drop(sim);
            assert_eq!(super::live_sims(), 0);
        });
    }

    #[test]
    fn sim_gate_off_a_sim_runtime_does_nothing() {
        let _scope = Scope::enter();
        let _again = Scope::enter();
    }
}
