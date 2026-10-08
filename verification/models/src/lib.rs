//! Stateright models of the Autumn coordination protocols (issue #3071).
//!
//! Each module models one protocol from its SQL statements. Time is one
//! integer clock: the database clock. Each model has a [`Variant`]: the
//! correct protocol, or a seeded bug. A seeded bug must give a
//! counterexample, so the check is not vacuous.
//!
//! - [`job_claim`]: claim, heartbeat, recovery and settle of a durable job
//!   (`autumn/src/job.rs`, ADR 0016).
//! - [`tick_election`]: the scheduler tick record
//!   (`autumn/src/scheduler.rs`, #3052).
//! - [`lease_lock`]: acquire, renew and expire of `LeaseLock`, with fencing
//!   (`autumn/src/lock/lease.rs`, ADR 0015).
//!
//! [`Variant`]: job_claim::Variant

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::hash::Hash;

use stateright::{Checker, Expectation, Model};

pub mod job_claim;
pub mod lease_lock;
pub mod tick_election;

/// The result of one exhaustive breadth-first check.
#[derive(Debug, Clone)]
pub struct Report {
    states: usize,
    violations: BTreeSet<&'static str>,
    reached: BTreeSet<&'static str>,
    unreached: BTreeSet<&'static str>,
    traces: Vec<String>,
}

impl Report {
    /// The number of unique states that the checker visited.
    #[must_use]
    pub const fn states(&self) -> usize {
        self.states
    }

    /// The `always` properties that have a counterexample.
    #[must_use]
    pub const fn violations(&self) -> &BTreeSet<&'static str> {
        &self.violations
    }

    /// The `sometimes` properties that have an example.
    #[must_use]
    pub fn reached(&self, property: &str) -> bool {
        self.reached.contains(property)
    }

    /// The `sometimes` properties that have no example.
    #[must_use]
    pub const fn unreached(&self) -> &BTreeSet<&'static str> {
        &self.unreached
    }

    /// The counterexample traces, one action list for each violation.
    #[must_use]
    pub fn describe(&self) -> String {
        self.traces.join("\n")
    }
}

/// Check every reachable state of `model` and return the result.
///
/// The model must be finite. Each model in this crate bounds its clock and
/// its counters.
#[must_use]
pub fn check<M>(model: M) -> Report
where
    M: Model + Send + Sync + 'static,
    M::State: Clone + Debug + Hash + PartialEq + Send + Sync + 'static,
    M::Action: Clone + Debug + PartialEq + Send + Sync + 'static,
{
    let properties = model.properties();
    let checker = model.checker().spawn_bfs().join();
    let mut report = Report {
        states: checker.unique_state_count(),
        violations: BTreeSet::new(),
        reached: BTreeSet::new(),
        unreached: BTreeSet::new(),
        traces: Vec::new(),
    };
    for property in properties {
        let found = checker.discovery(property.name);
        match (property.expectation, found) {
            (Expectation::Always | Expectation::Eventually, Some(path)) => {
                report.violations.insert(property.name);
                report
                    .traces
                    .push(format!("{}: {:?}", property.name, path.into_actions()));
            }
            (Expectation::Sometimes, Some(_)) => {
                report.reached.insert(property.name);
            }
            (Expectation::Sometimes, None) => {
                report.unreached.insert(property.name);
            }
            (Expectation::Always | Expectation::Eventually, None) => {}
        }
    }
    report
}
