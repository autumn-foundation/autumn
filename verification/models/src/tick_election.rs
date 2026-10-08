//! Scheduler tick election with the durable tick record (#3052).
//!
//! Three replicas race for one tick of one task. Statements
//! (`PostgresTickSchedulerCoordinator`, `autumn/src/scheduler.rs`):
//!
//! - prune: `DELETE FROM autumn_scheduler_ticks WHERE expires_at < now()`.
//! - claim: `INSERT … expires_at = now() + hold ON CONFLICT DO NOTHING
//!   RETURNING generation`, with `hold = retention + period`.
//! - free: `DELETE … WHERE tick_key = $3 AND generation = $4`. A replica
//!   frees an unrun claim when the cost gate rises, then claims again.
//!
//! A replica can wait (the cost gate, a slow claim) between the choice of the
//! tick and the insert. `execute_cron_task` checks on its own clock that the
//! tick is not past its window, before and after the claim. The model checks
//! at the insert. The check is closed (`<=`), which over-approximates
//! continuous time. Each replica clock has a skew of up to
//! `RETENTION`; the database clock decides the row expiry.
//! A free can be sent again after it applied (a retry after a lost reply).
//! Release keeps the row, so it is not an action.

use stateright::{Model, Property};

/// Each tick runs at most once.
pub const TICK_RUNS_AT_MOST_ONCE: &str = "each tick runs at most once";
/// Non-vacuity: the tick runs.
pub const TICK_RUNS: &str = "the tick runs";
/// Non-vacuity: a replica frees an unrun claim and another replica claims.
pub const FREED_TICK_IS_RECLAIMED: &str = "a freed tick is claimed again";
/// Non-vacuity: the prune deletes the row.
pub const ROW_IS_PRUNED: &str = "the tick row is pruned";

const REPLICAS: u8 = 3;
/// The tick period: the window of the tick.
const PERIOD: u8 = 3;
/// `scheduler.tick_retention`. It covers the clock skew between replicas.
const RETENTION: u8 = 1;
/// The clock skew of each replica, from `-RETENTION` to `RETENTION`.
const SKEW: [i8; 3] = [-1, 0, 1];
const MAX_TIME: u8 = 6;

/// The protocol, or one seeded bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The protocol as shipped.
    Correct,
    /// The claim uses `ON CONFLICT DO UPDATE`: it takes an existing row.
    OverwriteOnConflict,
    /// The row expires one step before the end of the window.
    HoldShorterThanPeriod,
    /// The row holds for `max(retention, period)`, not their sum.
    HoldIsMax,
    /// A replica claims after a long wait with no window check (the bug
    /// that this model found in `execute_cron_task`).
    NoLatenessCheck,
    /// Free deletes the row of the tick with no generation check.
    FreeWithoutGeneration,
}

/// A row of `autumn_scheduler_ticks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Row {
    /// The `owner` column.
    pub owner: u8,
    /// The `generation` column.
    pub generation: u8,
    /// The `expires_at` column.
    pub expires_at: u8,
}

/// What a replica does with the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// The replica has not claimed the tick.
    Idle,
    /// The replica holds the claim with `generation`.
    Holding(u8),
    /// The claim found a row; the replica skips the tick.
    Skipped,
    /// The replica ran the tick.
    Ran,
}

/// The global state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct State {
    /// The database clock.
    pub now: u8,
    /// The tick row.
    pub row: Option<Row>,
    /// The next `generation` value.
    pub next_generation: u8,
    /// One phase per replica.
    pub replicas: Vec<Phase>,
    /// The replicas that freed a claim (a bit each). A replica frees once.
    pub freed: u8,
    /// Free retries in flight: the generation of each, sorted.
    pub retries: Vec<u8>,
    /// Ghost: the number of runs of the tick.
    pub runs: u8,
    /// Ghost: a claim succeeded after a free.
    pub reclaimed: bool,
    /// Ghost: the prune deleted the row.
    pub pruned: bool,
}

/// An action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// The prune statement.
    Prune,
    /// The replica runs the claim statement.
    Claim(u8),
    /// The replica runs the tick.
    Run(u8),
    /// The replica frees its unrun claim.
    Free(u8),
    /// A free retry arrives.
    RetryFree(usize),
    /// The clock moves forward.
    Tick,
}

/// The model.
#[derive(Debug, Clone)]
pub struct TickElectionModel {
    variant: Variant,
}

impl TickElectionModel {
    /// A model of `variant`.
    #[must_use]
    pub const fn new(variant: Variant) -> Self {
        Self { variant }
    }

    const fn hold(&self) -> u8 {
        match self.variant {
            Variant::HoldShorterThanPeriod => PERIOD.saturating_sub(1),
            Variant::HoldIsMax => {
                if RETENTION > PERIOD {
                    RETENTION
                } else {
                    PERIOD
                }
            }
            _ => RETENTION.saturating_add(PERIOD),
        }
    }

    /// The window check of replica `r`, on its own clock.
    fn in_window(&self, state: &State, r: u8) -> bool {
        let skew = SKEW.get(usize::from(r)).copied().unwrap_or(0);
        self.variant == Variant::NoLatenessCheck
            || i16::from(state.now) + i16::from(skew) <= i16::from(PERIOD)
    }

    fn free(&self, state: &mut State, generation: u8) {
        if state.row.is_some_and(|row| {
            self.variant == Variant::FreeWithoutGeneration || row.generation == generation
        }) {
            state.row = None;
        }
    }
}

impl Model for TickElectionModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<State> {
        vec![State {
            now: 0,
            row: None,
            next_generation: 1,
            replicas: vec![Phase::Idle; usize::from(REPLICAS)],
            freed: 0,
            retries: Vec::new(),
            runs: 0,
            reclaimed: false,
            pruned: false,
        }]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        if state.row.is_some_and(|row| row.expires_at < state.now) {
            actions.push(Action::Prune);
        }
        for (r, phase) in (0..REPLICAS).zip(&state.replicas) {
            match phase {
                Phase::Idle if self.in_window(state, r) => actions.push(Action::Claim(r)),
                Phase::Holding(_) => {
                    actions.push(Action::Run(r));
                    if state.freed & (1 << r) == 0 {
                        actions.push(Action::Free(r));
                    }
                }
                _ => {}
            }
        }
        for i in 0..state.retries.len() {
            actions.push(Action::RetryFree(i));
        }
        if state.now < MAX_TIME {
            actions.push(Action::Tick);
        }
    }

    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut s = last.clone();
        match action {
            Action::Prune => {
                s.row = None;
                s.pruned = true;
            }
            Action::Claim(r) => {
                let generation = s.next_generation;
                s.next_generation = s.next_generation.saturating_add(1);
                if s.row.is_none() || self.variant == Variant::OverwriteOnConflict {
                    s.row = Some(Row {
                        owner: r,
                        generation,
                        expires_at: s.now.saturating_add(self.hold()),
                    });
                    s.replicas[usize::from(r)] = Phase::Holding(generation);
                    s.reclaimed |= s.freed != 0;
                } else {
                    s.replicas[usize::from(r)] = Phase::Skipped;
                }
            }
            Action::Run(r) => {
                s.replicas[usize::from(r)] = Phase::Ran;
                s.runs = s.runs.saturating_add(1);
            }
            Action::Free(r) => {
                let Phase::Holding(generation) = s.replicas[usize::from(r)] else {
                    return None;
                };
                self.free(&mut s, generation);
                s.replicas[usize::from(r)] = Phase::Idle;
                s.freed |= 1 << r;
                s.retries.push(generation);
                s.retries.sort_unstable();
            }
            Action::RetryFree(i) => {
                let generation = s.retries.remove(i);
                self.free(&mut s, generation);
            }
            Action::Tick => s.now = s.now.saturating_add(1),
        }
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always(TICK_RUNS_AT_MOST_ONCE, |_, s: &State| s.runs <= 1),
            Property::sometimes(TICK_RUNS, |_, s: &State| s.runs == 1),
            Property::sometimes(FREED_TICK_IS_RECLAIMED, |_, s: &State| s.reclaimed),
            Property::sometimes(ROW_IS_PRUNED, |_, s: &State| s.pruned),
        ]
    }
}
