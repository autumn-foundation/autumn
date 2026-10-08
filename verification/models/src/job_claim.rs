//! Durable job claim, heartbeat, recovery and settle (ADR 0016, #3051).
//!
//! Two workers race for one job row. The fence is `claimed_by = $me AND
//! status = 'running'`; the job has no generation column. Statements:
//!
//! - claim: `pg_claim_sql_single_queue` (`autumn/src/job.rs`).
//! - renew: `pg_renew_claim`, `UPDATE … SET claimed_at = NOW() WHERE id = $1
//!   AND claimed_by = $2 AND status = 'running'`.
//! - settle: `pg_ack_success` and `pg_nack_failure`, with the same guard.
//! - recover: `pg_recover_stale_claims`, `status = 'running' AND claimed_at <
//!   NOW() - visibility`.
//!
//! A worker renews every `HEARTBEAT` and gives up `GIVE_UP` after its last
//! successful renewal (`LeaseHeartbeat`). A renewal can be late, can fail,
//! or can apply with a lost reply. A settle can be late. The clock does not
//! move past a due give-up, because the give-up timer fires on time.

use stateright::{Model, Property};

/// A settle applies only for the worker that holds the current attempt.
pub const STALE_SETTLE_REJECTED: &str = "a stale holder's settle is rejected";
/// No two workers run the handler of the job at one time.
pub const ONE_EXECUTION_AT_A_TIME: &str = "at most one effective execution per job attempt";
/// Non-vacuity: the sweep recovers a claim.
pub const JOB_IS_RECOVERED: &str = "a claim is recovered";
/// Non-vacuity: a second worker claims the job after a recovery.
pub const JOB_IS_RECLAIMED: &str = "a second worker claims the job";
/// Non-vacuity: a settle that arrives after a recovery is attempted.
pub const LATE_SETTLE_ARRIVES: &str = "a late settle arrives after a recovery";
/// Non-vacuity: the job completes.
pub const JOB_COMPLETES: &str = "the job completes";

const WORKERS: u8 = 2;
const MAX_ATTEMPTS: u8 = 2;
/// The visibility timeout.
const VISIBILITY: u8 = 3;
/// `visibility / 3`.
const HEARTBEAT: u8 = 1;
/// `2 * visibility / 3`.
const GIVE_UP: u8 = 2;
const MAX_TIME: u8 = 8;

/// The protocol, or one seeded bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The protocol as shipped.
    Correct,
    /// Settle checks only `status = 'running'`, not `claimed_by`.
    SettleWithoutOwnerFence,
    /// The worker never gives up after failed renewals.
    NoGiveUp,
    /// The sweep recovers a running claim before its deadline.
    RecoverLiveClaim,
}

/// The `status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// Waiting for a claim.
    Enqueued,
    /// Claimed by `claimed_by`.
    Running,
    /// Settled as done.
    Completed,
    /// Dead-lettered.
    Failed,
}

/// The job row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Row {
    /// The `status` column.
    pub status: Status,
    /// The `attempt` column.
    pub attempt: u8,
    /// The `claimed_by` column.
    pub claimed_by: Option<u8>,
    /// The `claimed_at` column.
    pub claimed_at: u8,
}

/// What a worker does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Phase {
    /// No job.
    Idle,
    /// The handler runs for `attempt`.
    Running {
        /// The attempt that this run claimed.
        attempt: u8,
        /// The send time of the last successful renewal.
        renewed: u8,
        /// A renewal is in flight.
        renewing: bool,
    },
    /// The handler is done. The settle for `attempt` is in flight.
    Settling {
        /// The attempt that this run claimed.
        attempt: u8,
    },
}

/// A message between a worker and the database.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Msg {
    /// A renewal sent at `sent`.
    Renew {
        /// The sender.
        worker: u8,
        /// The attempt of the sender's run.
        attempt: u8,
        /// The send time.
        sent: u8,
    },
    /// The reply to a renewal.
    RenewReply {
        /// The sender of the renewal.
        worker: u8,
        /// The attempt of the sender's run.
        attempt: u8,
        /// The send time of the renewal.
        sent: u8,
        /// `Some(true)`: renewed. `Some(false)`: lost. `None`: error.
        outcome: Option<bool>,
    },
    /// A settle (ack, retry or dead letter).
    Settle {
        /// The sender.
        worker: u8,
        /// The attempt of the sender's run.
        attempt: u8,
    },
}

/// The global state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct State {
    /// The database clock.
    pub now: u8,
    /// The job row.
    pub row: Row,
    /// One phase per worker.
    pub workers: Vec<Phase>,
    /// Messages in flight, sorted.
    pub net: Vec<Msg>,
    /// Ghost: a settle applied for a worker that does not hold the attempt.
    pub stale_settle: bool,
    /// Ghost: the sweep recovered a claim.
    pub recovered: bool,
    /// Ghost: the workers that claimed the job.
    pub claimers: u8,
    /// Ghost: a settle arrived when its sender no longer held the row.
    pub late_settle: bool,
}

/// An action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// The worker claims the enqueued job.
    Claim(u8),
    /// The worker sends a renewal.
    SendRenew(u8),
    /// The database applies a renewal and replies.
    ApplyRenew(usize),
    /// The database applies a renewal; the reply is an error.
    ApplyRenewLoseReply(usize),
    /// The renewal fails before it applies.
    FailRenew(usize),
    /// The worker reads a renewal reply.
    ReadRenewReply(usize),
    /// The worker gives up its run.
    GiveUp(u8),
    /// The handler completes; the worker sends the settle.
    Finish(u8),
    /// The database applies a settle.
    ApplySettle(usize),
    /// The sweep recovers a stale claim.
    Recover,
    /// The clock moves forward.
    Tick,
}

/// The model.
#[derive(Debug, Clone)]
pub struct JobClaimModel {
    variant: Variant,
}

impl JobClaimModel {
    /// A model of `variant`.
    #[must_use]
    pub const fn new(variant: Variant) -> Self {
        Self { variant }
    }

    fn give_up_due(&self, state: &State, phase: &Phase) -> bool {
        self.variant != Variant::NoGiveUp
            && matches!(phase, Phase::Running { renewed, .. }
                if state.now.saturating_sub(*renewed) >= GIVE_UP)
    }

    /// The guard of renew and settle: `claimed_by = $me AND status = 'running'`.
    fn holds(row: &Row, worker: u8) -> bool {
        row.status == Status::Running && row.claimed_by == Some(worker)
    }

    fn settle_guard(&self, row: &Row, worker: u8) -> bool {
        match self.variant {
            Variant::SettleWithoutOwnerFence => row.status == Status::Running,
            _ => Self::holds(row, worker),
        }
    }

    fn recoverable(&self, state: &State) -> bool {
        state.row.status == Status::Running
            && match self.variant {
                Variant::RecoverLiveClaim => true,
                _ => state.now.saturating_sub(state.row.claimed_at) > VISIBILITY,
            }
    }
}

fn push(net: &mut Vec<Msg>, msg: Msg) {
    net.push(msg);
    net.sort();
}

impl Model for JobClaimModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<State> {
        vec![State {
            now: 0,
            row: Row {
                status: Status::Enqueued,
                attempt: 1,
                claimed_by: None,
                claimed_at: 0,
            },
            workers: vec![Phase::Idle; usize::from(WORKERS)],
            net: Vec::new(),
            stale_settle: false,
            recovered: false,
            claimers: 0,
            late_settle: false,
        }]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        for (w, phase) in (0..WORKERS).zip(&state.workers) {
            match phase {
                Phase::Idle if state.row.status == Status::Enqueued => {
                    actions.push(Action::Claim(w));
                }
                Phase::Running {
                    renewed, renewing, ..
                } => {
                    if !renewing && state.now.saturating_sub(*renewed) >= HEARTBEAT {
                        actions.push(Action::SendRenew(w));
                    }
                    if self.give_up_due(state, phase) {
                        actions.push(Action::GiveUp(w));
                    }
                    actions.push(Action::Finish(w));
                }
                _ => {}
            }
        }
        for (i, msg) in state.net.iter().enumerate() {
            match msg {
                Msg::Renew { .. } => {
                    actions.push(Action::ApplyRenew(i));
                    actions.push(Action::ApplyRenewLoseReply(i));
                    actions.push(Action::FailRenew(i));
                }
                Msg::RenewReply { .. } => actions.push(Action::ReadRenewReply(i)),
                Msg::Settle { .. } => actions.push(Action::ApplySettle(i)),
            }
        }
        if self.recoverable(state) {
            actions.push(Action::Recover);
        }
        let give_up_pending = state.workers.iter().any(|p| self.give_up_due(state, p));
        if state.now < MAX_TIME && !give_up_pending {
            actions.push(Action::Tick);
        }
    }

    #[allow(clippy::too_many_lines, reason = "one arm for each action")]
    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut s = last.clone();
        match action {
            Action::Claim(w) => {
                s.row.status = Status::Running;
                s.row.claimed_by = Some(w);
                s.row.claimed_at = s.now;
                s.workers[usize::from(w)] = Phase::Running {
                    attempt: s.row.attempt,
                    renewed: s.now,
                    renewing: false,
                };
                s.claimers |= 1 << w;
            }
            Action::SendRenew(w) => {
                if let Phase::Running {
                    attempt, renewing, ..
                } = &mut s.workers[usize::from(w)]
                {
                    *renewing = true;
                    let msg = Msg::Renew {
                        worker: w,
                        attempt: *attempt,
                        sent: s.now,
                    };
                    push(&mut s.net, msg);
                }
            }
            Action::ApplyRenew(i) | Action::ApplyRenewLoseReply(i) | Action::FailRenew(i) => {
                let Msg::Renew {
                    worker,
                    attempt,
                    sent,
                } = s.net.remove(i)
                else {
                    return None;
                };
                let outcome = if matches!(action, Action::FailRenew(_)) {
                    None
                } else {
                    let applied = Self::holds(&s.row, worker);
                    if applied {
                        s.row.claimed_at = s.now;
                    }
                    (!matches!(action, Action::ApplyRenewLoseReply(_))).then_some(applied)
                };
                let reply = Msg::RenewReply {
                    worker,
                    attempt,
                    sent,
                    outcome,
                };
                push(&mut s.net, reply);
            }
            Action::ReadRenewReply(i) => {
                let Msg::RenewReply {
                    worker,
                    attempt,
                    sent,
                    outcome,
                } = s.net.remove(i)
                else {
                    return None;
                };
                let phase = &mut s.workers[usize::from(worker)];
                if let Phase::Running {
                    attempt: run,
                    renewed,
                    renewing,
                } = phase
                    && *run == attempt
                {
                    *renewing = false;
                    match outcome {
                        Some(true) => *renewed = sent,
                        // The lease is lost: drop the handler, do not settle.
                        Some(false) => *phase = Phase::Idle,
                        None => {}
                    }
                }
            }
            Action::GiveUp(w) => s.workers[usize::from(w)] = Phase::Idle,
            Action::Finish(w) => {
                if let Phase::Running { attempt, .. } = s.workers[usize::from(w)] {
                    s.workers[usize::from(w)] = Phase::Settling { attempt };
                    push(&mut s.net, Msg::Settle { worker: w, attempt });
                }
            }
            Action::ApplySettle(i) => {
                let Msg::Settle { worker, attempt } = s.net.remove(i) else {
                    return None;
                };
                if !Self::holds(&s.row, worker) {
                    s.late_settle |= s.recovered;
                }
                if self.settle_guard(&s.row, worker) {
                    if !Self::holds(&s.row, worker) || s.row.attempt != attempt {
                        s.stale_settle = true;
                    }
                    s.row.status = Status::Completed;
                    s.row.claimed_by = None;
                }
                if s.workers[usize::from(worker)] == (Phase::Settling { attempt }) {
                    s.workers[usize::from(worker)] = Phase::Idle;
                }
            }
            Action::Recover => {
                s.row.status = if s.row.attempt < MAX_ATTEMPTS {
                    s.row.attempt = s.row.attempt.saturating_add(1);
                    Status::Enqueued
                } else {
                    Status::Failed
                };
                s.row.claimed_by = None;
                s.recovered = true;
            }
            Action::Tick => s.now = s.now.saturating_add(1),
        }
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always(STALE_SETTLE_REJECTED, |_, s: &State| !s.stale_settle),
            Property::always(ONE_EXECUTION_AT_A_TIME, |_, s: &State| {
                s.workers
                    .iter()
                    .filter(|p| matches!(p, Phase::Running { .. }))
                    .count()
                    <= 1
            }),
            Property::sometimes(JOB_IS_RECOVERED, |_, s: &State| s.recovered),
            Property::sometimes(JOB_IS_RECLAIMED, |_, s: &State| s.claimers == 0b11),
            Property::sometimes(LATE_SETTLE_ARRIVES, |_, s: &State| s.late_settle),
            Property::sometimes(JOB_COMPLETES, |_, s: &State| {
                s.row.status == Status::Completed
            }),
        ]
    }
}
