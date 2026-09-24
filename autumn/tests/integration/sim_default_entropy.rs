//! Sim-testing (issue #1797): `Sim::build` seeds the app's entropy by default.
//!
//! A framework-minted id (here the `X-Request-Id` that `RequestIdLayer` draws
//! from the app's entropy) must replay from `AUTUMN_SIM_SEED` without the test
//! calling `with_entropy`. An explicit `with_entropy` still wins, and a
//! restarted app draws a new stream that is still a function of the seed.

use autumn_web::entropy::SeededEntropy;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::test::TestApp;

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// Build a sim for `seed` on a fresh paused runtime, run `body`, and return
/// the request ids it collected.
fn run_sim<F>(seed: u64, body: F) -> Vec<String>
where
    F: AsyncFnOnce(&mut Sim) -> Vec<String>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("paused runtime");
    runtime.block_on(async move {
        let mut sim = Sim::from_seed(seed);
        body(&mut sim).await
    })
}

async fn request_ids(sim: &Sim, count: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        let response = sim.client().get("/ping").send().await;
        response.assert_ok();
        ids.push(
            response
                .header("x-request-id")
                .expect("RequestIdLayer stamps X-Request-Id")
                .to_owned(),
        );
    }
    ids
}

#[test]
fn a_built_app_replays_its_ids_from_the_seed() {
    let first = run_sim(7, async |sim| {
        sim.build(TestApp::new().routes(routes![ping]));
        request_ids(sim, 4).await
    });
    let second = run_sim(7, async |sim| {
        sim.build(TestApp::new().routes(routes![ping]));
        request_ids(sim, 4).await
    });
    assert_eq!(
        first, second,
        "same seed, no with_entropy: same request ids"
    );

    let other = run_sim(8, async |sim| {
        sim.build(TestApp::new().routes(routes![ping]));
        request_ids(sim, 4).await
    });
    assert_ne!(first, other, "a different seed gives different request ids");
}

#[test]
fn the_default_is_the_documented_seeded_source() {
    let defaulted = run_sim(7, async |sim| {
        sim.build(TestApp::new().routes(routes![ping]));
        request_ids(sim, 4).await
    });
    let explicit = run_sim(7, async |sim| {
        let seed = sim.seed;
        sim.build(
            TestApp::new()
                .routes(routes![ping])
                .with_entropy(SeededEntropy::new(seed)),
        );
        request_ids(sim, 4).await
    });
    assert_eq!(
        defaulted, explicit,
        "the default equals `with_entropy(SeededEntropy::new(sim.seed))`",
    );
}

#[test]
fn an_explicit_entropy_source_wins() {
    let defaulted = run_sim(7, async |sim| {
        sim.build(TestApp::new().routes(routes![ping]));
        request_ids(sim, 4).await
    });
    let overridden = run_sim(7, async |sim| {
        sim.build(
            TestApp::new()
                .routes(routes![ping])
                .with_entropy(SeededEntropy::new(99)),
        );
        request_ids(sim, 4).await
    });
    assert_ne!(defaulted, overridden, "with_entropy replaces the default");
}

#[test]
fn a_restarted_app_draws_a_new_stream_that_still_replays() {
    let run = || {
        run_sim(7, async |sim| {
            sim.build(TestApp::new().routes(routes![ping]));
            let mut ids = request_ids(sim, 2).await;
            sim.crash_and_restart(TestApp::new().routes(routes![ping]));
            ids.extend(request_ids(sim, 2).await);
            ids
        })
    };
    let first = run();
    assert_ne!(
        first[..2],
        first[2..],
        "a restart must not replay the crashed process's ids",
    );
    assert_eq!(first, run(), "the restarted stream is still seed-driven");
}
