//! `always!` / `sometimes!` assertion macros + non-vacuity registry (W6, #1797).
//!
//! Isolated `[[test]]` binary. The reachability registry is a thread-local:
//! libtest runs each `#[test]` on its own thread, so tests here never contend on
//! it — but each test that inspects the registry resets it first (or constructs a
//! fresh `Sim`, which resets it) to stay hermetic regardless of harness threading.

use autumn_web::sim::{
    Sim, assert_all_sometimes_satisfied, reset_sometimes_registry, sometimes_snapshot,
    sometimes_unsatisfied,
};
use autumn_web::{always, sometimes};

#[test]
fn always_true_is_a_noop() {
    always!(true);
    always!(1 + 1 == 2);
    always!(2 > 1, "formatted {} arg", 42);
}

#[test]
fn always_false_panics_with_greppable_message() {
    let payload = std::panic::catch_unwind(|| {
        always!(false);
    })
    .expect_err("always!(false) must panic");
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("panic payload should be a string");
    assert!(
        msg.contains("always! invariant violated"),
        "message must be greppable: {msg}"
    );
    assert!(
        msg.contains("false"),
        "message must include the stringified condition: {msg}"
    );
}

#[test]
fn always_false_with_format_args_includes_user_message() {
    let balance = -3;
    let payload = std::panic::catch_unwind(|| {
        always!(balance >= 0, "balance went negative: {}", balance);
    })
    .expect_err("always! with a false condition must panic");
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("panic payload should be a string");
    assert!(
        msg.contains("always! invariant violated"),
        "message must be greppable: {msg}"
    );
    assert!(
        msg.contains("balance went negative: -3"),
        "message must include the formatted user message: {msg}"
    );
}

#[test]
fn sometimes_records_satisfied_and_observed_only() {
    reset_sometimes_registry();
    sometimes!(true, "satisfied-label");
    sometimes!(false, "observed-only-label");

    let (observed, satisfied) = sometimes_snapshot();
    assert!(observed.contains("satisfied-label"));
    assert!(observed.contains("observed-only-label"));
    assert!(satisfied.contains("satisfied-label"));
    assert!(!satisfied.contains("observed-only-label"));

    // The non-vacuity gap is exactly the observed-but-never-satisfied label.
    assert_eq!(
        sometimes_unsatisfied().into_iter().collect::<Vec<_>>(),
        vec!["observed-only-label".to_owned()]
    );
}

#[test]
fn sometimes_evaluates_condition_once() {
    reset_sometimes_registry();
    let mut calls = 0;
    sometimes!(
        {
            calls += 1;
            true
        },
        "counts-once"
    );
    assert_eq!(
        calls, 1,
        "sometimes! must evaluate its condition exactly once"
    );
}

#[test]
fn from_seed_resets_registry_between_runs() {
    // First "seed run": observe a label but leave it unsatisfied.
    let _first = Sim::from_seed(1);
    sometimes!(false, "leaked-across-seeds");
    assert!(sometimes_snapshot().0.contains("leaked-across-seeds"));

    // Constructing the next Sim resets the registry — a fresh seed starts clean,
    // so the prior seed's labels never leak into this one.
    let _second = Sim::from_seed(2);
    let (observed, satisfied) = sometimes_snapshot();
    assert!(
        observed.is_empty(),
        "from_seed must clear observed labels: {observed:?}"
    );
    assert!(
        satisfied.is_empty(),
        "from_seed must clear satisfied labels: {satisfied:?}"
    );
}

#[test]
fn assert_all_sometimes_satisfied_passes_when_all_satisfied() {
    reset_sometimes_registry();
    sometimes!(true, "a");
    sometimes!(true, "b");
    assert_all_sometimes_satisfied(); // must not panic
}

#[test]
fn assert_all_sometimes_satisfied_panics_on_unsatisfied_label() {
    reset_sometimes_registry();
    sometimes!(true, "green-ok");
    sometimes!(false, "never-fired");

    let payload = std::panic::catch_unwind(assert_all_sometimes_satisfied)
        .expect_err("an observed-but-unsatisfied label must fail the assertion");
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("panic payload should be a string");
    assert!(
        msg.contains("never-fired"),
        "panic must name the unsatisfied label: {msg}"
    );
    assert!(
        !msg.contains("green-ok"),
        "panic must not list satisfied labels: {msg}"
    );
}
