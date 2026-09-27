//! Sim Phase 2 (issue #2967): kill between any two awaits.
//!
//! `crash_at` drops an operation at its N-th suspension point. The work before
//! that point stays done. The work after it never runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::prelude::*;
use autumn_web::sim::{CrashOutcome, Sim, crash_at};
use autumn_web::sim_test;
use autumn_web::test::TestApp;

/// Three steps with one suspension point between each pair.
async fn three_steps(done: &AtomicU64) -> u64 {
    done.fetch_add(1, Ordering::SeqCst);
    tokio::task::yield_now().await;
    done.fetch_add(1, Ordering::SeqCst);
    tokio::task::yield_now().await;
    done.fetch_add(1, Ordering::SeqCst);
    done.load(Ordering::SeqCst)
}

#[sim_test]
async fn sim_crash_at_drops_the_op_at_each_await(_sim: Sim) {
    for index in 0..2 {
        let done = AtomicU64::new(0);
        let outcome = crash_at(index, three_steps(&done)).await;
        assert_eq!(outcome, CrashOutcome::Crashed { await_index: index });
        assert!(outcome.is_crashed());
        assert_eq!(
            done.load(Ordering::SeqCst),
            index + 1,
            "only the steps before await {index} ran"
        );
    }
}

#[sim_test]
async fn sim_crash_at_past_the_last_await_completes(_sim: Sim) {
    let done = AtomicU64::new(0);
    let outcome = crash_at(2, three_steps(&done)).await;
    assert_eq!(outcome, CrashOutcome::Completed(3));
    assert_eq!(outcome.completed(), Some(3));
}

#[sim_test]
async fn sim_crash_at_reads_the_seeded_crash_point(sim: Sim) {
    let point = sim.crash_point().expect("the schedule is never empty");
    let done = AtomicU64::new(0);
    let outcome = crash_at(point.await_index, three_steps(&done)).await;
    match outcome {
        CrashOutcome::Crashed { await_index } => {
            assert_eq!(await_index, point.await_index);
            assert_eq!(done.load(Ordering::SeqCst), point.await_index + 1);
        }
        CrashOutcome::Completed(steps) => {
            assert!(
                point.await_index >= 2,
                "only an index past the last await completes"
            );
            assert_eq!(steps, 3);
        }
        _ => unreachable!("CrashOutcome has two variants today"),
    }
}

/// Counts the handler steps that ran, shared with the test.
#[derive(Clone, Default)]
struct Steps(Arc<AtomicU64>);

#[get("/two-step")]
async fn two_step(State(state): State<AppState>) -> &'static str {
    let steps = state.extension::<Steps>().expect("steps installed");
    steps.0.fetch_add(1, Ordering::SeqCst);
    tokio::task::yield_now().await;
    steps.0.fetch_add(1, Ordering::SeqCst);
    "done"
}

#[sim_test]
async fn sim_crash_at_kills_a_request_mid_handler(mut sim: Sim) {
    let steps = Steps::default();
    let app = TestApp::new().routes(routes![two_step]).state_initializer({
        let steps = steps.clone();
        move |state| state.insert_extension(steps)
    });
    sim.build(app);

    // Sweep every await of the request until it completes.
    let mut index = 0;
    let mut crashed_mid_handler = false;
    loop {
        let before = steps.0.load(Ordering::SeqCst);
        let outcome = crash_at(index, sim.client().get("/two-step").send()).await;
        if let Some(response) = outcome.completed() {
            response.assert_ok();
            break;
        }
        let ran = steps.0.load(Ordering::SeqCst) - before;
        assert!(ran <= 1, "a crash before the second step leaves it undone");
        crashed_mid_handler |= ran == 1;
        index += 1;
        assert!(index < 64, "the request completes within 64 awaits");
    }
    assert!(
        crashed_mid_handler,
        "one crash point sits between the two handler steps"
    );
}

#[sim_test]
async fn sim_crash_at_counts_the_same_awaits_under_interleave(sim: Sim) {
    let (a, b) = (AtomicU64::new(0), AtomicU64::new(0));
    // `b` wakes often, which polls the whole interleave again. `a` must still
    // crash at its own second await.
    let outcomes = sim
        .interleave(vec![
            crash_at(1, three_steps(&a)),
            crash_at(9, three_steps(&b)),
        ])
        .await;
    assert_eq!(outcomes[0], CrashOutcome::Crashed { await_index: 1 });
    assert_eq!(a.load(Ordering::SeqCst), 2, "steps before await 1 only");
    assert_eq!(outcomes[1], CrashOutcome::Completed(3));
}
