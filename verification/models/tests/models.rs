//! Bounded model checks of the coordination protocols (issue #3071).
//!
//! Each correct model must satisfy every `always` property and reach every
//! `sometimes` property. Each seeded bug must give a counterexample. If a
//! seeded bug gives no counterexample, the test fails.

use autumn_protocol_models::job_claim::{self, JobClaimModel};
use autumn_protocol_models::lease_lock::{self, LeaseLockModel};
use autumn_protocol_models::tick_election::{self, TickElectionModel};
use autumn_protocol_models::{Report, check};

/// An upper limit on the states of one model. If a model exceeds this limit,
/// reduce its bounds.
const MAX_STATES: usize = 2_000_000;

fn assert_holds(report: &Report) {
    assert!(
        report.violations().is_empty(),
        "the correct model has counterexamples:\n{}",
        report.describe()
    );
    assert!(
        report.unreached().is_empty(),
        "the correct model does not reach {:?}; the check is vacuous",
        report.unreached()
    );
    assert!(
        report.states() < MAX_STATES,
        "{} states; reduce the bounds",
        report.states()
    );
}

fn assert_counterexample(report: &Report, property: &str) {
    assert!(
        report.violations().contains(&property),
        "the seeded bug gives no counterexample for {property:?}; violations: {:?}",
        report.violations()
    );
}

#[test]
fn job_claim_correct_protocol_holds() {
    assert_holds(&check(JobClaimModel::new(job_claim::Variant::Correct)));
}

#[test]
fn job_claim_settle_without_owner_fence_lets_a_stale_holder_settle() {
    let report = check(JobClaimModel::new(
        job_claim::Variant::SettleWithoutOwnerFence,
    ));
    assert_counterexample(&report, job_claim::STALE_SETTLE_REJECTED);
}

#[test]
fn job_claim_without_give_up_overlaps_executions() {
    let report = check(JobClaimModel::new(job_claim::Variant::NoGiveUp));
    assert_counterexample(&report, job_claim::ONE_EXECUTION_AT_A_TIME);
}

#[test]
fn job_claim_recovery_of_a_live_claim_overlaps_executions() {
    let report = check(JobClaimModel::new(job_claim::Variant::RecoverLiveClaim));
    assert_counterexample(&report, job_claim::ONE_EXECUTION_AT_A_TIME);
}

#[test]
fn tick_election_correct_protocol_holds() {
    assert_holds(&check(TickElectionModel::new(
        tick_election::Variant::Correct,
    )));
}

#[test]
fn tick_election_overwrite_on_conflict_runs_a_tick_twice() {
    let report = check(TickElectionModel::new(
        tick_election::Variant::OverwriteOnConflict,
    ));
    assert_counterexample(&report, tick_election::TICK_RUNS_AT_MOST_ONCE);
}

#[test]
fn tick_election_hold_shorter_than_period_runs_a_tick_twice() {
    let report = check(TickElectionModel::new(
        tick_election::Variant::HoldShorterThanPeriod,
    ));
    assert_counterexample(&report, tick_election::TICK_RUNS_AT_MOST_ONCE);
}

#[test]
fn tick_election_hold_of_the_larger_value_runs_a_tick_twice_under_skew() {
    let report = check(TickElectionModel::new(tick_election::Variant::HoldIsMax));
    assert_counterexample(&report, tick_election::TICK_RUNS_AT_MOST_ONCE);
}

/// The bug that this model found: a cost wait outlasts the tick row.
#[test]
fn tick_election_claim_with_no_lateness_check_runs_a_tick_twice() {
    let report = check(TickElectionModel::new(
        tick_election::Variant::NoLatenessCheck,
    ));
    assert_counterexample(&report, tick_election::TICK_RUNS_AT_MOST_ONCE);
}

#[test]
fn tick_election_free_without_generation_runs_a_tick_twice() {
    let report = check(TickElectionModel::new(
        tick_election::Variant::FreeWithoutGeneration,
    ));
    assert_counterexample(&report, tick_election::TICK_RUNS_AT_MOST_ONCE);
}

#[test]
fn lease_lock_correct_protocol_holds() {
    assert_holds(&check(LeaseLockModel::new(lease_lock::Variant::Correct)));
}

#[test]
fn lease_lock_resource_without_token_check_admits_a_stale_write() {
    let report = check(LeaseLockModel::new(
        lease_lock::Variant::ResourceIgnoresToken,
    ));
    assert_counterexample(&report, lease_lock::STALE_WRITE_REJECTED);
}

#[test]
fn lease_lock_acquire_without_increment_reuses_a_token() {
    let report = check(LeaseLockModel::new(
        lease_lock::Variant::AcquireKeepsGeneration,
    ));
    assert_counterexample(&report, lease_lock::TOKENS_ARE_UNIQUE);
    assert_counterexample(&report, lease_lock::STALE_WRITE_REJECTED);
}

#[test]
fn lease_lock_release_that_deletes_the_row_reuses_a_token() {
    let report = check(LeaseLockModel::new(lease_lock::Variant::ReleaseDeletesRow));
    assert_counterexample(&report, lease_lock::TOKENS_ARE_UNIQUE);
}

#[test]
fn lease_lock_renew_without_generation_lets_two_holders_trust_the_lease() {
    let report = check(LeaseLockModel::new(
        lease_lock::Variant::RenewIgnoresGeneration,
    ));
    assert_counterexample(&report, lease_lock::LIVE_HOLDERS_EXCLUSIVE);
}

/// `docs/adr/0015-fencing-lease-lock.md`: fencing makes an overlap of holders safe. An acquire that takes
/// a live lease is a bug, but the resource still rejects the stale write.
#[test]
fn lease_lock_fencing_tolerates_overlapping_holders() {
    let report = check(LeaseLockModel::new(lease_lock::Variant::AcquireWhileHeld));
    assert!(
        !report
            .violations()
            .contains(&lease_lock::STALE_WRITE_REJECTED),
        "{}",
        report.describe()
    );
    assert_counterexample(&report, lease_lock::LIVE_HOLDERS_EXCLUSIVE);
}
