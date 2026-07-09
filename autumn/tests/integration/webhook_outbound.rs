use autumn_web::test::TestApp;
use autumn_web::webhook_outbound::{
    InMemoryOutboundWebhookStore, OutboundWebhookPlugin, OutboundWebhookStore,
    WebhookOutboundManager, WebhookSubscription, WebhookSubscriptionStatus,
};
use std::sync::Arc;
use std::time::Duration;

/// Poll `condition` every 10 ms until it returns `true` or `timeout` elapses.
///
/// Replaces fixed `tokio::time::sleep` calls that are fragile on slow CI
/// runners (especially Windows): the condition is checked as soon as the
/// background work finishes rather than waiting a worst-case wall-clock time.
async fn poll_until<F, Fut>(timeout: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            // Let the assertion that follows produce the real failure message.
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_webhook_outbound_lifecycle() {
    use autumn_web::job::{self, JobInfo, clear_global_job_client, global_job_runtime_test_lock};

    let _guard = global_job_runtime_test_lock().lock().await;
    clear_global_job_client();

    // 1. Setup a TestApp with our OutboundWebhookPlugin configured with 5ms retry delay
    let store = Arc::new(InMemoryOutboundWebhookStore::new());
    let plugin = OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(5);

    let mut app_builder = TestApp::new().plugin(plugin);

    // 2. Register a mock for our outbound request before building
    let mock = app_builder
        .http_mock("http://mock-receiver/webhooks/orders")
        .post("/webhooks/orders")
        .respond_with(200, serde_json::json!({ "received": true }));

    let app = app_builder.build();
    let state = app.state();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = autumn_web::config::JobConfig::default();
    let job_info = JobInfo {
        name: "autumn_webhook_delivery".to_owned(),
        max_attempts: 1,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: autumn_web::webhook_outbound::deliver_webhook_job,
    };
    job::start_runtime(vec![job_info], state, &shutdown, &config).unwrap();

    // 3. Create a webhook subscription for topic "order.created"
    let sub = WebhookSubscription {
        id: "sub_123".to_owned(),
        target_url: "http://mock-receiver/webhooks/orders".to_owned(),
        event_topics: vec!["order.created".to_owned()],
        secret: "my_webhook_signing_secret_32_bytes!!".to_owned(),
        status: WebhookSubscriptionStatus::Active,
        consecutive_failures: 0,
    };
    store.create_subscription(sub.clone()).await.unwrap();

    // 4. Dispatch the webhook
    let manager = state
        .extension::<WebhookOutboundManager>()
        .expect("WebhookOutboundManager should be registered in extensions");

    let payload = serde_json::json!({
        "order_id": "ord_999",
        "amount": 4999
    });
    manager
        .dispatch(state, "order.created", &payload)
        .await
        .unwrap();

    // 5. Wait for background delivery job to execute.
    //    response_status is set after the HTTP call; is_dlq covers permanent failure.
    //    Do NOT poll on logs.is_empty() — the log is created BEFORE the HTTP call.
    poll_until(Duration::from_secs(5), || {
        let store = store.clone();
        async move {
            store
                .get_delivery_logs()
                .await
                .is_ok_and(|logs| logs.iter().any(|l| l.response_status.is_some() || l.is_dlq))
        }
    })
    .await;

    // 6. Assert mock was called
    mock.expect_called(1);

    // 7. Verify delivery log was saved and is successful
    let logs = store.get_delivery_logs().await.unwrap();
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    assert_eq!(log.subscription_id, "sub_123");
    assert_eq!(log.topic, "order.created");
    assert_eq!(log.response_status, Some(200));
    assert!(!log.is_dlq);
    assert!(log.last_error.is_none());

    // 8. Assert Stripe-style signature is present in request headers
    assert!(log.request_headers.contains_key("Autumn-Signature"));
    let sig_header = log.request_headers.get("Autumn-Signature").unwrap();
    assert!(sig_header.starts_with("t="));
    assert!(sig_header.contains(",v1="));

    shutdown.cancel();
    clear_global_job_client();
}

#[tokio::test]
async fn test_webhook_outbound_retries_and_dlq() {
    use autumn_web::job::{self, JobInfo, clear_global_job_client, global_job_runtime_test_lock};

    let _guard = global_job_runtime_test_lock().lock().await;
    clear_global_job_client();

    let store = Arc::new(InMemoryOutboundWebhookStore::new());
    let plugin = OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(1); // 1ms base retry delay for lightning-fast testing

    let mut app_builder = TestApp::new().plugin(plugin);

    // Register a mock returning 500
    let mock = app_builder
        .http_mock("http://mock-receiver/webhooks/fail")
        .post("/webhooks/fail")
        .respond_with(500, serde_json::json!({ "error": "server error" }));

    let app = app_builder.build();
    let state = app.state();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = autumn_web::config::JobConfig::default();
    let job_info = JobInfo {
        name: "autumn_webhook_delivery".to_owned(),
        max_attempts: 5,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: autumn_web::webhook_outbound::deliver_webhook_job,
    };
    job::start_runtime(vec![job_info], state, &shutdown, &config).unwrap();

    let sub = WebhookSubscription {
        id: "sub_retry".to_owned(),
        target_url: "http://mock-receiver/webhooks/fail".to_owned(),
        event_topics: vec!["retry.topic".to_owned()],
        secret: "my_webhook_signing_secret_32_bytes!!".to_owned(),
        status: WebhookSubscriptionStatus::Active,
        consecutive_failures: 0,
    };
    store.create_subscription(sub.clone()).await.unwrap();

    let manager = state
        .extension::<WebhookOutboundManager>()
        .expect("WebhookOutboundManager registered");

    // Dispatch
    let payload = serde_json::json!({ "data": "test" });
    manager
        .dispatch(state, "retry.topic", &payload)
        .await
        .unwrap();

    // Wait for the first attempt and all subsequent retries (max_attempts = 5).
    //    All retries update the same log entry in place; is_dlq is set only
    //    after the final attempt exhausts max_attempts.
    poll_until(Duration::from_secs(5), || {
        let store = store.clone();
        async move {
            store
                .get_delivery_logs()
                .await
                .is_ok_and(|logs| logs.iter().any(|l| l.is_dlq))
        }
    })
    .await;

    // Verify mock was hit 5 times (1 original + 4 retries)
    mock.expect_called(5);

    // Check delivery logs
    let logs = store.get_delivery_logs().await.unwrap();
    assert!(!logs.is_empty());

    // Assert it was retried and reached DLQ
    let dlq_logs = store.get_dlq_logs().await.unwrap();
    assert_eq!(dlq_logs.len(), 1);

    let dlq_log = &dlq_logs[0];
    assert_eq!(dlq_log.subscription_id, "sub_retry");
    assert_eq!(dlq_log.attempt, 5); // 5 attempts max
    assert!(dlq_log.is_dlq);
    assert_eq!(dlq_log.response_status, Some(500));

    shutdown.cancel();
    clear_global_job_client();
}

#[tokio::test]
async fn test_webhook_outbound_failure_caps_deactivation() {
    use autumn_web::job::{self, JobInfo, clear_global_job_client, global_job_runtime_test_lock};

    let _guard = global_job_runtime_test_lock().lock().await;
    clear_global_job_client();

    let store = Arc::new(InMemoryOutboundWebhookStore::new());
    let plugin = OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(1);

    let mut app_builder = TestApp::new().plugin(plugin);

    let _mock = app_builder
        .http_mock("http://mock-receiver/webhooks/fail_cap")
        .post("/webhooks/fail_cap")
        .respond_with(500, serde_json::json!({ "error": "server error" }));

    let app = app_builder.build();
    let state = app.state();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = autumn_web::config::JobConfig::default();
    let job_info = JobInfo {
        name: "autumn_webhook_delivery".to_owned(),
        max_attempts: 5,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: autumn_web::webhook_outbound::deliver_webhook_job,
    };
    job::start_runtime(vec![job_info], state, &shutdown, &config).unwrap();

    let sub = WebhookSubscription {
        id: "sub_cap".to_owned(),
        target_url: "http://mock-receiver/webhooks/fail_cap".to_owned(),
        event_topics: vec!["cap.topic".to_owned()],
        secret: "my_webhook_signing_secret_32_bytes!!".to_owned(),
        status: WebhookSubscriptionStatus::Active,
        consecutive_failures: 48, // Start at 48 failures
    };
    store.create_subscription(sub.clone()).await.unwrap();

    let manager = state
        .extension::<WebhookOutboundManager>()
        .expect("WebhookOutboundManager registered");

    // Dispatch
    let payload = serde_json::json!({ "data": "test" });
    manager
        .dispatch(state, "cap.topic", &payload)
        .await
        .unwrap();

    // Wait for attempts to fail and subscription to be marked Failed
    poll_until(Duration::from_secs(5), || {
        let store = store.clone();
        async move {
            store
                .get_subscription("sub_cap")
                .await
                .ok()
                .flatten()
                .is_some_and(|s| s.status == WebhookSubscriptionStatus::Failed)
        }
    })
    .await;

    // Verify subscription status is now Failed due to exceeding the cap of 50 failures
    let updated_sub = store.get_subscription("sub_cap").await.unwrap().unwrap();
    assert_eq!(updated_sub.status, WebhookSubscriptionStatus::Failed);

    shutdown.cancel();
    clear_global_job_client();
}

#[tokio::test]
async fn test_webhook_outbound_actuator_endpoints() {
    use autumn_web::job::{self, JobInfo, clear_global_job_client, global_job_runtime_test_lock};
    use autumn_web::webhook_outbound::WebhookDeliveryLog;

    let _guard = global_job_runtime_test_lock().lock().await;
    clear_global_job_client();

    let store = Arc::new(InMemoryOutboundWebhookStore::new());
    let plugin = OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(1);

    let mut config = autumn_web::config::AutumnConfig::default();
    config.actuator.sensitive = true;

    let mut app_builder = TestApp::new().plugin(plugin).config(config);

    // Mock for webhook delivery
    let mock = app_builder
        .http_mock("http://mock-receiver/webhooks/actuator")
        .post("/webhooks/actuator")
        .respond_with(500, serde_json::json!({ "error": "failed" }));

    let app = app_builder.build();
    let state = app.state();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = autumn_web::config::JobConfig::default();
    let job_info = JobInfo {
        name: "autumn_webhook_delivery".to_owned(),
        max_attempts: 5,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: autumn_web::webhook_outbound::deliver_webhook_job,
    };
    job::start_runtime(vec![job_info], state, &shutdown, &config).unwrap();

    // 1. Initial DLQ should be empty
    let res = app.get("/actuator/webhooks/dlq").send().await;
    res.assert_ok();
    let initial_dlq: Vec<WebhookDeliveryLog> = res.json();
    assert!(initial_dlq.is_empty());

    // 2. Dispatch a webhook that will fail permanently (max_attempts = 5) and go to DLQ
    let sub = WebhookSubscription {
        id: "sub_actuator".to_owned(),
        target_url: "http://mock-receiver/webhooks/actuator".to_owned(),
        event_topics: vec!["actuator.topic".to_owned()],
        secret: "my_webhook_signing_secret_32_bytes!!".to_owned(),
        status: WebhookSubscriptionStatus::Active,
        consecutive_failures: 0,
    };
    store.create_subscription(sub.clone()).await.unwrap();

    let manager = state
        .extension::<WebhookOutboundManager>()
        .expect("WebhookOutboundManager registered");

    let payload = serde_json::json!({ "event": "test" });
    manager
        .dispatch(state, "actuator.topic", &payload)
        .await
        .unwrap();

    // Wait for it to fail permanently (5 attempts)
    poll_until(Duration::from_secs(5), || {
        let store = store.clone();
        async move {
            store
                .get_delivery_logs()
                .await
                .is_ok_and(|logs| logs.iter().any(|l| l.is_dlq))
        }
    })
    .await;
    mock.expect_called(5);

    // 3. DLQ should now have 1 item
    let res = app.get("/actuator/webhooks/dlq").send().await;
    res.assert_ok();
    let dlq_logs: Vec<WebhookDeliveryLog> = res.json();
    assert_eq!(dlq_logs.len(), 1);
    let failed_log_id = dlq_logs[0].id.clone();
    assert!(dlq_logs[0].is_dlq);
    assert_eq!(dlq_logs[0].subscription_id, "sub_actuator");

    // Let's set up a new mock response for the replay that returns 200 OK!
    // Since we're re-requesting the same mock, let's configure the mock to succeed now.
    // Wait, the mock library might not support dynamic reconfiguration easily, but we can
    // check if it has a way to override. Alternatively, we can let it fail again and see it re-attempted.
    // Actually, let's configure a new mock or just verify that the replay request re-enqueues it.
    // Wait, if it is replayed, the DLQ status is reset, so even if it fails, it will start retrying again.
    // Let's just verify it is replayed successfully.
    let replay_res = app
        .post("/actuator/webhooks/replay")
        .json(&serde_json::json!({ "log_id": failed_log_id }))
        .send()
        .await;
    replay_res.assert_ok();

    // Verify it's no longer marked as DLQ'd in the store (or the DLQ list is empty now)
    let res_after = app.get("/actuator/webhooks/dlq").send().await;
    res_after.assert_ok();
    let dlq_after: Vec<WebhookDeliveryLog> = res_after.json();
    assert!(dlq_after.is_empty());

    shutdown.cancel();
    clear_global_job_client();
}
