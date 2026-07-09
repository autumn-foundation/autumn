//!
//! Integration tests for config runtime drift.
//!
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;

#[tokio::test]
async fn config_runtime_drift_actuator_prefix_is_mounted() {
    #[allow(clippy::field_reassign_with_default)]
    let mut config = AutumnConfig::default();
    config.actuator.prefix = "/ops".to_owned();

    let app = TestApp::new().config(config).build();

    let response = app.get("/ops/health").send().await;
    response.assert_status(200);

    let response = app.get("/actuator/health").send().await;
    response.assert_status(404);
}
