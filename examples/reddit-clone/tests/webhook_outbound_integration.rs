//! Integration test: Outbound signed webhook delivery on user registration.
//!
//! Verifies that registering a new account successfully dispatches a "user.created"
//! outbound webhook event, which signs the payload and delivers it to a mock receiver.
//!
//! Requires Docker; skipped by default. Run with:
//!
//! ```text
//! cargo test -p reddit-clone --test webhook_outbound_integration -- --ignored
//! ```

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestDb};
use autumn_web::webhook_outbound::{
    InMemoryOutboundWebhookStore, OutboundWebhookPlugin, WebhookSubscription,
    WebhookSubscriptionStatus,
};
use diesel_migrations::{EmbeddedMigrations, embed_migrations};
use std::sync::Arc;
use std::time::Duration;

const REDDIT_MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_reddit_registration_triggers_outbound_webhook() {
    use autumn_web::job::{self, JobInfo, clear_global_job_client, global_job_runtime_test_lock};

    let _ = tracing_subscriber::fmt::try_init();

    // Acquire lock and clear global job client to prevent test pollution
    let _guard = global_job_runtime_test_lock().lock().await;
    clear_global_job_client();

    // 1. Initialize our TestDb and run framework + application migrations
    let db = TestDb::shared().await;

    // Run framework migrations (e.g. jobs, etc.)
    autumn_web::migrate::run_pending(db.url(), autumn_web::migrate::FRAMEWORK_MIGRATIONS)
        .expect("failed to run framework migrations");

    // Run reddit-clone specific migrations
    autumn_web::migrate::run_pending(db.url(), REDDIT_MIGRATIONS)
        .expect("failed to run reddit-clone migrations");

    // Clean tables before test run to avoid unique constraint violations
    db.execute_sql("TRUNCATE TABLE users CASCADE").await;
    db.execute_sql("TRUNCATE TABLE autumn_jobs CASCADE").await;

    // 2. Setup the Webhook Outbound Plugin with a process-local InMemory store
    let store = Arc::new(InMemoryOutboundWebhookStore::new());
    let plugin = OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(1);

    // Create a subscription targeting the mock receiver
    let sub = WebhookSubscription {
        id: "sub_reddit_signup".to_owned(),
        target_url: "http://mock-receiver/webhooks/signups".to_owned(),
        event_topics: vec!["user.created".to_owned()],
        secret: "reddit_clone_webhook_secret_key_32_bytes!!".to_owned(),
        status: WebhookSubscriptionStatus::Active,
        consecutive_failures: 0,
    };
    store.create_subscription(sub).await.unwrap();

    // 3. Build the TestApp
    let mut app_builder = TestApp::new()
        .plugin(plugin)
        .routes(routes![reddit_clone::routes::auth::register])
        .with_db(db.pool());

    // Register HTTP mock for the outbound signed webhook target
    let mock = app_builder
        .http_mock("http://mock-receiver/webhooks/signups")
        .post("/webhooks/signups")
        .respond_with(200, serde_json::json!({ "success": true }));

    let app = app_builder.build();
    let state = app.state();

    // 4. Start the background job runtime for the webhook delivery job
    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = autumn_web::config::JobConfig {
        backend: "postgres".to_string(),
        ..Default::default()
    };

    let mut jobs = reddit_clone::jobs::registered_jobs();
    jobs.push(JobInfo {
        name: "autumn_webhook_delivery".to_owned(),
        max_attempts: 1,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: autumn_web::webhook_outbound::deliver_webhook_job,
    });
    job::start_runtime(jobs, state, &shutdown, &config).unwrap();

    // 5. Fire a POST request to /register to trigger the user.created webhook event
    let response = app
        .post("/register")
        .form("username=webhook_user&email=webhook_user@example.com&password=supersecurepassword")
        .send()
        .await;

    response.assert_status(303); // Redirect to "/" indicating successful registration

    // 6. Wait for the background webhook delivery job to execute
    let mut logs = Vec::new();
    for _ in 0..50 {
        logs = store.get_delivery_logs().await.unwrap();
        if logs.first().is_some_and(|l| l.response_status.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 7. Verify the mock receiver was called and the signature exists
    mock.expect_called(1);

    // Verify delivery logs were recorded successfully
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    assert_eq!(log.topic, "user.created");
    assert_eq!(log.response_status, Some(200));
    assert!(log.request_headers.contains_key("Autumn-Signature"));

    // Clean up
    shutdown.cancel();
    clear_global_job_client();
}
