//!
//! Integration tests for config runtime drift.
//!
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn config_runtime_drift_actuator_prefix_is_mounted() {
    // Isolate this test from the process-global circuit-breaker registry.
    //
    // `<prefix>/health` aggregates the state of every breaker in
    // `circuit_breaker::global_registry()` (see `actuator.rs`): a single Open
    // or HalfOpen breaker flips the aggregate to DOWN and the endpoint returns
    // 503. Sibling tests (e.g. the circuit-breaker integration suite,
    // `http_client`, `mail`) register breakers there and, under the coverage
    // job's ~15x llvm-cov slowdown, one can still be tripped Open when this
    // test hits `/ops/health`, sporadically turning the expected 200 into a
    // 503. Serialize against those tests via the shared `TEST_LOCK` and clear
    // any leaked breakers up front — the same process-global-registry isolation
    // precedent used for the `#[throttle]` `THROTTLE_REGISTRY` (#1732) and by
    // the circuit-breaker integration tests themselves. The guard is held for
    // the whole test so no sibling can register an Open breaker mid-assertion.
    let _cb_lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();

    #[allow(clippy::field_reassign_with_default)]
    let mut config = AutumnConfig::default();
    config.actuator.prefix = "/ops".to_owned();

    let app = TestApp::new().config(config).build();

    let response = app.get("/ops/health").send().await;
    response.assert_status(200);

    let response = app.get("/actuator/health").send().await;
    response.assert_status(404);
}
