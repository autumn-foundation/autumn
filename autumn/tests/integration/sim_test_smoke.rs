//! Smoke test proving the sim-testing W1 Definition of Done (issue #1797):
//! an empty `#[sim_test]` compiles and runs deterministically at seed 0.
//!
//! The replay-line-on-panic path is covered by the `__replay_line` unit test in
//! `autumn/src/sim.rs` and the macro-expansion assertions in
//! `autumn-macros/src/sim_test.rs`; this test deliberately does **not** panic
//! (that would fail CI).

use autumn_web::sim::Sim;
use autumn_web::sim_test;

#[sim_test]
async fn empty_sim_test_runs_at_seed_zero(sim: Sim) {
    assert_eq!(sim.seed, 0);
}
