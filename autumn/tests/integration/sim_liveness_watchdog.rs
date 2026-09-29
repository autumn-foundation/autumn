//! Sim-testing (issue #1797): an armed liveness watchdog fails a deadlocked
//! simulation with its seed.
//!
//! A deadlock parks every task with no timer that could wake one. Under the
//! paused runtime the only timer left is the watchdog, so tokio advances
//! virtual time straight to it and the run panics at once instead of hanging
//! CI. `#[sim_test]` arms it from `AUTUMN_SIM_LIVENESS_BUDGET_SECS`; these
//! tests arm it directly, so they do not depend on the environment.

use std::time::Duration;

use autumn_web::sim::__with_liveness_budget;

const ONE_YEAR: Duration = Duration::from_secs(365 * 24 * 3600);

/// Run `body` on a fresh paused current-thread runtime, as `#[sim_test]` does.
fn on_paused_runtime<F: std::future::Future>(body: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("paused runtime")
        .block_on(body)
}

#[test]
#[should_panic(expected = "sim liveness")]
fn a_deadlocked_body_panics_instead_of_hanging() {
    on_paused_runtime(__with_liveness_budget(0x5eed, Some(ONE_YEAR), async {
        // The sender stays alive and never sends, so this await never ends.
        let (_sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let _ = receiver.await;
    }));
}

#[test]
fn the_panic_names_the_seed() {
    let panic = std::panic::catch_unwind(|| {
        on_paused_runtime(__with_liveness_budget(0x5eed, Some(ONE_YEAR), async {
            std::future::pending::<()>().await;
        }));
    })
    .expect_err("a deadlock must panic");
    let message = panic
        .downcast_ref::<String>()
        .expect("the watchdog panics with a formatted message");
    assert!(message.contains("seed=0x5eed"), "{message}");
}

#[test]
fn a_body_that_waits_on_timers_is_not_a_deadlock() {
    on_paused_runtime(__with_liveness_budget(0, Some(ONE_YEAR), async {
        // A month of virtual sleeps stays far inside the budget.
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        }
    }));
}

#[test]
fn an_unarmed_watchdog_leaves_the_body_alone() {
    let answer = on_paused_runtime(__with_liveness_budget(0, None, async { 42 }));
    assert_eq!(answer, 42);
}
