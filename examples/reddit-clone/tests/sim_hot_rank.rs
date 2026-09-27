//! Deterministic simulation test for the hot-rank decay curve (0.7.0's
//! `#[sim_test]` harness — see `docs/guide/simulation-testing.md`).
//!
//! reddit-clone ranks its front page with a time-decay formula:
//!
//! ```text
//! hot_rank = score / (age_in_hours + 2) ^ 1.5
//! ```
//!
//! Every interesting property of that formula is a statement about **time
//! passing**, which is exactly what a conventional test cannot express: proving
//! that a post falls off the front page after a day would mean sleeping for a
//! day, and faking it by passing a hand-rolled `now` past
//! `calculate_hot_rank` would test the arithmetic while skipping the seam that
//! actually decides what "now" means in production.
//!
//! `#[sim_test]` removes the trade-off. It hands the test a seeded [`Sim`], a
//! paused current-thread executor, and a **virtual clock** that the mounted app
//! reads through the ordinary [`Clock`] extractor. `sim.advance(24h)` moves the
//! app's own notion of now by a day with zero wall-clock sleeping, so the
//! ranking route below is exercised across two days of simulated ageing in
//! milliseconds — through the real seam, not around it.
//!
//! It doubles as a regression test for those sim seams: if the injected clock
//! ever stops reaching handler extractors, every `always!` here fires.
//!
//! # Running
//!
//! ```text
//! cargo test -p reddit-clone --test sim_hot_rank
//! AUTUMN_SIM_SEED=0x9f3a cargo test -p reddit-clone --test sim_hot_rank
//! ```
//!
//! The second form is the replay line `#[sim_test]` prints on failure: the
//! scores below are drawn from `sim.rng()`, so a seed reproduces a run exactly.

use std::time::Duration;

use autumn_web::entropy::SeededEntropy;
use autumn_web::extract::Path;
use autumn_web::prelude::*;
use autumn_web::sim::{Sim, assert_all_sometimes_satisfied};
use autumn_web::test::TestApp;
use autumn_web::time::Clock;
use autumn_web::{always, sim_test, sometimes};

use reddit_clone::tasks::calculate_hot_rank;

/// The instant `Sim` pins its virtual clock to at the start of every run, and
/// therefore the instant every simulated post below was submitted. The first
/// assertion in the test pins this down rather than trusting it.
const SIM_EPOCH: &str = "2020-01-01T00:00:00";

/// A `hot_rank` at or above this keeps a post on the front page. Chosen so the
/// two score bands below land on opposite sides of it after a day of decay.
const FRONT_PAGE: f64 = 2.0;

/// Scores high enough that a day of decay still leaves them on the front page:
/// `400 / (24 + 2)^1.5 ≈ 3.0`, comfortably above [`FRONT_PAGE`].
const HOT_BAND: std::ops::Range<i64> = 400..900;

/// Scores low enough that a day of decay takes them off it:
/// `200 / (24 + 2)^1.5 ≈ 1.5`, below [`FRONT_PAGE`].
const COLD_BAND: std::ops::Range<i64> = 10..200;

/// How many posts are simulated in each band.
const POSTS_PER_BAND: usize = 4;

/// Checkpoints, in virtual hours since the epoch, at which every post is
/// re-ranked. `24` is the one the reachability assertions read.
const CHECKPOINTS_HOURS: [i64; 6] = [0, 1, 4, 12, 24, 48];

/// Rank a post with `score` submitted at [`SIM_EPOCH`], as of *now*.
///
/// The only thing that makes this route interesting is the [`Clock`]
/// extractor: it reads the injected clock, so under the sim it sees virtual
/// time and under `cargo run` it sees the wall clock. The handler itself has
/// no idea which it is — that is the seam being exercised.
#[get("/hot/{score}")]
async fn hot_rank(Path(score): Path<i64>, clock: Clock) -> String {
    let created_at = SIM_EPOCH
        .parse::<chrono::NaiveDateTime>()
        .expect("SIM_EPOCH is a valid naive timestamp");
    calculate_hot_rank(score, created_at, clock.now().naive_utc()).to_string()
}

/// Draw a deterministic score inside `band` from the sim's seeded RNG.
fn draw_score(sim: &mut Sim, band: &std::ops::Range<i64>) -> i64 {
    let span = u64::try_from(band.end - band.start).expect("band is non-empty and ascending");
    let offset = i64::try_from(sim.rng().next_u64() % span).expect("offset fits in i64");
    band.start + offset
}

#[sim_test]
async fn hot_rank_decays_monotonically_under_virtual_time(mut sim: Sim) {
    // Draw the whole workload up front: `rng()` borrows the sim mutably and the
    // client below borrows it immutably, so the two cannot be interleaved.
    let hot: Vec<i64> = (0..POSTS_PER_BAND)
        .map(|_| draw_score(&mut sim, &HOT_BAND))
        .collect();
    let cold: Vec<i64> = (0..POSTS_PER_BAND)
        .map(|_| draw_score(&mut sim, &COLD_BAND))
        .collect();

    sim.build(
        TestApp::new()
            .routes(routes![hot_rank])
            // Nothing in this test draws entropy yet, but wiring a seeded
            // source is the documented default: `Sim::build` injects the clock
            // automatically and entropy *only* on request, so a later handler
            // that mints an id would otherwise stop replaying from
            // `AUTUMN_SIM_SEED` without any visible signal.
            .with_entropy(SeededEntropy::new(sim.seed)),
    );

    // Pin the epoch the route computes ages against. If `Sim`'s starting
    // instant ever moves, every `age_hours` below is wrong and this fails
    // first, with a message that says so.
    let at_epoch = rank_of(&sim, hot[0]).await;
    let expected_at_epoch =
        f64::from(i32::try_from(hot[0]).expect("score fits in i32")) / 2.0_f64.powf(1.5);
    always!(
        (at_epoch - expected_at_epoch).abs() < 1e-9,
        "at the sim epoch a post's age must be zero, so its rank is score / 2^1.5; \
         got {at_epoch} for score {} instead of {expected_at_epoch} (seed={:#x})",
        hot[0],
        sim.seed,
    );

    // Walk the decay curve. Each step advances the *app's* clock — no sleeping.
    let mut previous: Option<(i64, Vec<f64>)> = None;
    let mut elapsed_hours = 0_i64;
    let mut ranks_at_a_day: Vec<f64> = Vec::new();

    for checkpoint in CHECKPOINTS_HOURS {
        let step = checkpoint - elapsed_hours;
        if step > 0 {
            sim.advance(Duration::from_secs(
                u64::try_from(step).expect("checkpoints ascend") * 3600,
            ))
            .await;
            elapsed_hours = checkpoint;
        }

        let mut current = Vec::with_capacity(hot.len() + cold.len());
        for score in hot.iter().chain(cold.iter()).copied() {
            let rank = rank_of(&sim, score).await;

            // A positive score never ranks at zero, however old it gets: the
            // denominator grows without bound but never divides to nothing.
            always!(
                rank > 0.0,
                "a post with score {score} ranked {rank} at hour {checkpoint} — \
                 decay must never reach zero (seed={:#x})",
                sim.seed,
            );

            if checkpoint == 24 {
                ranks_at_a_day.push(rank);
            }
            current.push(rank);
        }

        // Decay is monotonic: no post may climb without its score changing.
        // This is the invariant that breaks the moment the injected clock stops
        // reaching the handler — a frozen clock makes every checkpoint equal,
        // and a clock that reads real wall time makes them jump around.
        if let Some((previous_hour, previous_ranks)) = &previous {
            for (index, (before, after)) in previous_ranks.iter().zip(current.iter()).enumerate() {
                always!(
                    after < before,
                    "rank must strictly fall as a post ages: post {index} went \
                     {before} -> {after} between hour {previous_hour} and hour \
                     {checkpoint} (seed={:#x})",
                    sim.seed,
                );
            }
        }
        previous = Some((checkpoint, current));
    }

    // Reachability targets. Both bands exist precisely so a single run reaches
    // both sides of the front-page threshold at every seed, which is what makes
    // the `assert_all_sometimes_satisfied` below a real check rather than a
    // coin flip.
    for rank in &ranks_at_a_day {
        sometimes!(
            *rank >= FRONT_PAGE,
            "a high-scoring post was still on the front page after 24 virtual hours"
        );
        sometimes!(
            *rank < FRONT_PAGE,
            "a post decayed off the front page within 24 virtual hours"
        );
    }

    // A single run does not fail on an unsatisfied `sometimes!` by default —
    // that is what makes reachability assertions usable. Asking for the check
    // explicitly is how a test that *has* arranged for every label to be
    // reachable proves its green run was not vacuous.
    assert_all_sometimes_satisfied();
}

/// Ask the mounted app to rank `score` at the current virtual instant.
async fn rank_of(sim: &Sim, score: i64) -> f64 {
    let response = sim.client().get(&format!("/hot/{score}")).send().await;
    response.assert_ok();
    response
        .text()
        .parse::<f64>()
        .expect("the ranking route returns a float")
}
