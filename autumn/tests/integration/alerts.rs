//! End-to-end coverage for operator alerts (issue #1610).
//!
//! Proves the acceptance-criteria contract: a dead-lettered job (condition a)
//! and a failed scheduled task (condition d) produce an alert on the configured
//! channel; the built-in mail channel reuses the app's existing `Mailer` and
//! bypasses the suppression list (alerts are security-class); and a
//! sustained/repeating condition is deduplicated with a recovery notice when it
//! clears.

use std::sync::{Arc, Mutex};

use autumn_web::alerts::{
    Alert, AlertChannel, AlertCondition, AlertConfig, AlertDeliveryError, AlertDeliveryFuture,
    AlertEventKind, AlertSeverity, Alerter, AlerterSettings,
};
use autumn_web::test::TestApp;

/// Test channel that records every alert it is asked to deliver.
#[derive(Clone, Default)]
struct CapturingChannel {
    received: Arc<Mutex<Vec<Alert>>>,
}

impl CapturingChannel {
    fn alerts(&self) -> Vec<Alert> {
        self.received.lock().expect("lock").clone()
    }
}

impl AlertChannel for CapturingChannel {
    fn name(&self) -> &'static str {
        "capturing"
    }

    fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a> {
        let received = Arc::clone(&self.received);
        let cloned = alert.clone();
        Box::pin(async move {
            received.lock().expect("lock").push(cloned);
            Ok(())
        })
    }
}

fn test_settings() -> AlerterSettings {
    settings_with_prefix("/actuator")
}

fn settings_with_prefix(prefix: &str) -> AlerterSettings {
    settings_with_prefix_sensitive(prefix, true)
}

fn settings_with_prefix_sensitive(prefix: &str, actuator_sensitive: bool) -> AlerterSettings {
    // `actuator_sensitive` defaults to `true` for the prefix helper so the
    // dead-lettered-job/scheduled-task alerts point straight at the (mounted)
    // `/jobs` and `/tasks` endpoints — the behavior those tests assert. The
    // `sensitive = false` fallback is covered explicitly by
    // `sensitive_false_alerts_point_at_mounted_endpoint`.
    AlerterSettings {
        dedup_window: std::time::Duration::from_secs(900),
        health_grace: std::time::Duration::from_secs(60),
        error_rate_threshold: 0.05,
        error_rate_min_requests: 20,
        eval_interval: std::time::Duration::from_secs(30),
        actuator_prefix: prefix.to_owned(),
        actuator_sensitive,
    }
}

/// Wait for the detached delivery task(s) to run, polling until `channel` has at
/// least `n` alerts or the deadline elapses.
async fn wait_for(channel: &CapturingChannel, n: usize) {
    for _ in 0..100 {
        if channel.alerts().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// AC #8: induce a dead-lettered job and assert the alert arrives on the
/// configured channel. Drives the exact seam function
/// (`alerts::notify_dead_lettered_job`) that every job backend's dead-letter
/// site calls, over a real `AppState` with the alerter installed.
#[tokio::test]
async fn dead_lettered_job_delivers_alert_to_configured_channel() {
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(vec![Arc::new(channel.clone())], test_settings());

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();

    // Induce the dead-letter signal exactly as the job runtime does.
    autumn_web::alerts::notify_dead_lettered_job(
        client.state(),
        "reporting_job",
        "connection refused after 5 attempts",
    );

    wait_for(&channel, 1).await;
    let alerts = channel.alerts();
    assert_eq!(alerts.len(), 1, "exactly one alert delivered");
    let alert = &alerts[0];
    assert_eq!(alert.condition, AlertCondition::DeadLetteredJob);
    assert_eq!(alert.severity, AlertSeverity::Critical);
    assert_eq!(alert.event, AlertEventKind::Trigger);
    assert_eq!(alert.dedup_key, "dead_lettered_job:reporting_job");
    // AC #4: states what, where to look, and on which host.
    assert_eq!(alert.where_to_look, "/actuator/jobs");
    assert!(alert.title.contains("reporting_job"));
    assert!(alert.summary.contains("connection refused"));
    assert!(!alert.host.is_empty());
}

/// Finding 2: an alert's `where_to_look` must honor the configured `[actuator]
/// prefix`. With a custom prefix, the emitted alert points operators at the real
/// actuator endpoint (`/_ops/jobs`) instead of a hard-coded `/actuator/jobs`
/// that would 404. The default-prefix case is covered by
/// `dead_lettered_job_delivers_alert_to_configured_channel` above (`/actuator/jobs`).
#[tokio::test]
async fn where_to_look_honors_custom_actuator_prefix() {
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(
        vec![Arc::new(channel.clone())],
        settings_with_prefix("/_ops"),
    );

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();

    autumn_web::alerts::notify_dead_lettered_job(client.state(), "reporting_job", "boom");

    wait_for(&channel, 1).await;
    let alerts = channel.alerts();
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        alerts[0].where_to_look, "/_ops/jobs",
        "where_to_look must use the configured actuator prefix, not a hard-coded /actuator"
    );

    // A scheduled-task failure uses the same prefix for its /tasks pointer.
    autumn_web::alerts::notify_scheduled_task_failure(
        client.state(),
        "nightly_backup",
        "disk full",
    );
    wait_for(&channel, 2).await;
    let alerts = channel.alerts();
    assert_eq!(alerts[1].where_to_look, "/_ops/tasks");
}

/// AC #2 (condition d): a framework-scheduled task failure produces an alert.
#[tokio::test]
async fn scheduled_task_failure_delivers_alert() {
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(vec![Arc::new(channel.clone())], test_settings());

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();

    autumn_web::alerts::notify_scheduled_task_failure(
        client.state(),
        "nightly_backup",
        "disk full",
    );

    wait_for(&channel, 1).await;
    let alerts = channel.alerts();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].condition, AlertCondition::ScheduledTaskFailure);
    assert_eq!(alerts[0].where_to_look, "/actuator/tasks");
    assert!(alerts[0].summary.contains("disk full"));
}

/// Code-review fix: the `/actuator/jobs` and `/actuator/tasks` endpoints are
/// mounted ONLY when `[actuator] sensitive = true` (default `false`). With the
/// default `sensitive = false`, the dead-lettered-job and scheduled-task-failure
/// alerts must NOT point operators at those unmounted (404) endpoints — they must
/// point at the always-mounted `/actuator/health` and hint that the richer
/// endpoint needs `[actuator] sensitive = true`. With `sensitive = true` the
/// alerts point straight at `/jobs` / `/tasks`.
#[tokio::test]
async fn sensitive_false_alerts_point_at_mounted_endpoint() {
    // sensitive = false (the production default): fallback + hint, never a 404.
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(
        vec![Arc::new(channel.clone())],
        settings_with_prefix_sensitive("/actuator", false),
    );
    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();

    autumn_web::alerts::notify_dead_lettered_job(client.state(), "reporting_job", "boom");
    autumn_web::alerts::notify_scheduled_task_failure(
        client.state(),
        "nightly_backup",
        "disk full",
    );
    wait_for(&channel, 2).await;
    let alerts = channel.alerts();
    assert_eq!(alerts.len(), 2);

    let dead = &alerts[0].where_to_look;
    assert!(
        !dead.starts_with("/actuator/jobs"),
        "dead-letter alert must not link the unmounted /actuator/jobs: {dead}"
    );
    assert!(
        dead.contains("/actuator/health") && dead.contains("[actuator] sensitive = true"),
        "must point at /actuator/health and hint the sensitive requirement: {dead}"
    );

    let task = &alerts[1].where_to_look;
    assert!(
        !task.starts_with("/actuator/tasks"),
        "scheduled-task alert must not link the unmounted /actuator/tasks: {task}"
    );
    assert!(
        task.contains("/actuator/health") && task.contains("[actuator] sensitive = true"),
        "must point at /actuator/health and hint the sensitive requirement: {task}"
    );

    // sensitive = true: the endpoints are mounted, so link them directly.
    let channel2 = CapturingChannel::default();
    let alerter2 = Alerter::new(
        vec![Arc::new(channel2.clone())],
        settings_with_prefix_sensitive("/actuator", true),
    );
    let client2 = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter2.clone()))
        .build();

    autumn_web::alerts::notify_dead_lettered_job(client2.state(), "reporting_job", "boom");
    autumn_web::alerts::notify_scheduled_task_failure(
        client2.state(),
        "nightly_backup",
        "disk full",
    );
    wait_for(&channel2, 2).await;
    let alerts2 = channel2.alerts();
    assert_eq!(alerts2.len(), 2);
    assert_eq!(alerts2[0].where_to_look, "/actuator/jobs");
    assert_eq!(alerts2[1].where_to_look, "/actuator/tasks");
}

/// AC #3: a sustained/repeating condition is deduplicated (bounded, not
/// one-per-occurrence), and a recovery notice is sent when it clears.
#[tokio::test]
async fn repeated_condition_is_deduplicated_then_recovers() {
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(vec![Arc::new(channel.clone())], test_settings());

    // 10 dead-letters of the SAME job in quick succession.
    for _ in 0..10 {
        let _ = alerter.notify(
            Alert::trigger(AlertCondition::DeadLetteredJob, "dead_lettered_job:emailer")
                .title("emailer dead-lettered")
                .build(),
        );
    }
    // A recovery for that same condition.
    let recovered = alerter.recover(
        Alert::recovery(AlertCondition::DeadLetteredJob, "dead_lettered_job:emailer")
            .title("emailer recovered")
            .build(),
    );
    assert!(
        recovered,
        "recovery of a previously-alerted condition sends"
    );

    wait_for(&channel, 2).await;
    let alerts = channel.alerts();
    assert_eq!(
        alerts.len(),
        2,
        "one trigger (deduped) + one recovery, not one-per-occurrence: {alerts:?}"
    );
    assert_eq!(alerts[0].event, AlertEventKind::Trigger);
    assert_eq!(alerts[1].event, AlertEventKind::Resolve);
    assert_eq!(alerts[1].severity, AlertSeverity::Recovery);
    // Stable dedup key correlates trigger and recovery.
    assert_eq!(alerts[0].dedup_key, alerts[1].dedup_key);
}

/// Finding 2 (recovery): a scheduled task that fails, then succeeds, delivers a
/// recovery notice; and a subsequent failure within the dedup window alerts
/// again rather than being suppressed as if still failing. Proves the scheduler
/// success arms clear the `scheduled_task_failure:{task}` dedup key.
#[tokio::test]
async fn scheduled_task_recovers_then_realerts_on_new_failure() {
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(vec![Arc::new(channel.clone())], test_settings());

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();
    let state = client.state();

    // Fail → alert.
    autumn_web::alerts::notify_scheduled_task_failure(state, "nightly_backup", "disk full");
    wait_for(&channel, 1).await;

    // Succeed → recovery notice for the same key.
    autumn_web::alerts::notify_scheduled_task_recovered(state, "nightly_backup");
    wait_for(&channel, 2).await;

    // Fail again within the (900s) dedup window → must alert again, not be
    // suppressed as if still failing (the success cleared the active key).
    autumn_web::alerts::notify_scheduled_task_failure(state, "nightly_backup", "disk full again");
    wait_for(&channel, 3).await;

    let alerts = channel.alerts();
    assert_eq!(
        alerts.len(),
        3,
        "fail → recover → fail must produce trigger, resolve, trigger: {alerts:?}"
    );
    assert_eq!(alerts[0].event, AlertEventKind::Trigger);
    assert_eq!(alerts[0].condition, AlertCondition::ScheduledTaskFailure);
    assert_eq!(alerts[1].event, AlertEventKind::Resolve);
    assert_eq!(alerts[1].severity, AlertSeverity::Recovery);
    assert_eq!(alerts[2].event, AlertEventKind::Trigger);
    // Stable dedup key correlates the whole lifecycle.
    assert_eq!(alerts[0].dedup_key, "scheduled_task_failure:nightly_backup");
    assert_eq!(alerts[0].dedup_key, alerts[1].dedup_key);
    assert_eq!(alerts[1].dedup_key, alerts[2].dedup_key);
}

/// Recovery is a no-op when the task never failed: a success for a task with no
/// outstanding failure delivers nothing (the dedup gate suppresses it), so a
/// task that has only ever succeeded stays silent.
#[tokio::test]
async fn scheduled_task_recovery_without_prior_failure_is_silent() {
    let channel = CapturingChannel::default();
    let alerter = Alerter::new(vec![Arc::new(channel.clone())], test_settings());

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();

    autumn_web::alerts::notify_scheduled_task_recovered(client.state(), "healthy_task");

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        channel.alerts().is_empty(),
        "a success with no outstanding failure must deliver no recovery: {:?}",
        channel.alerts()
    );
}

/// Finding 3: alert webhooks are ALWAYS signed, so a webhook destination with no
/// signing secret must NOT register a webhook channel — sending unsigned would be
/// rejected by receivers following the documented verification. With the webhook
/// as the only destination, `install_from_config` installs no alerter at all.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn webhook_without_secret_does_not_register_channel() {
    let client = TestApp::new().build();

    let config = AlertConfig {
        webhook_url: Some("https://alerts.example.com/hooks/autumn".to_owned()),
        webhook_secret: None,
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_none(),
        "a webhook destination with no signing secret must not install an (unsigned) channel"
    );
}

/// Companion: a whitespace-only secret is treated as absent — still no channel.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn webhook_with_blank_secret_does_not_register_channel() {
    let client = TestApp::new().build();

    let config = AlertConfig {
        webhook_url: Some("https://alerts.example.com/hooks/autumn".to_owned()),
        webhook_secret: Some("   ".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_none(),
        "a blank signing secret must be treated as absent (no unsigned channel)"
    );
}

/// Companion: a webhook destination WITH a non-empty secret registers the signed
/// webhook channel, so an alerter is installed.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn webhook_with_secret_registers_channel() {
    let client = TestApp::new().build();

    let config = AlertConfig {
        webhook_url: Some("https://alerts.example.com/hooks/autumn".to_owned()),
        webhook_secret: Some("s3cr3t".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_some(),
        "a webhook destination with a signing secret must install the webhook channel"
    );
}

/// P2 finding: the RUNTIME path must reject an unusable webhook URL before
/// registering the channel. A relative/malformed `webhook_url` (here
/// `hooks.example/x`) — even WITH a secret — is not an absolute `http(s)` URL, so
/// `http_client::Client::build_request` would never dispatch it (it has no
/// base-url alias to resolve against) and every alert POST would fail
/// `reqwest::Url::parse`. `install_from_config` must skip the channel and, with
/// the webhook as the only destination, install no alerter at all — so the caller
/// is unaffected.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn webhook_with_relative_url_does_not_register_channel() {
    let client = TestApp::new().build();

    let config = AlertConfig {
        webhook_url: Some("hooks.example/x".to_owned()),
        webhook_secret: Some("s3cr3t".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_none(),
        "a relative/malformed webhook_url must not install a non-delivering webhook channel"
    );
}

/// P2 finding, whitespace case: a copied env var commonly carries leading/
/// trailing whitespace. The URL is otherwise valid, so `install_from_config` must
/// TRIM it, register the channel, and deliver to the TRIMMED URL. We prove the
/// trimmed URL is the one actually dispatched by registering an HTTP mock at the
/// clean path and asserting the delivered alert hits it exactly once — an
/// untrimmed `"  https://…  "` would fail to match (and, under the mock, error as
/// `NoMock`) so the mock would never be called.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn webhook_with_whitespace_padded_url_registers_and_uses_trimmed_url() {
    let trimmed = "https://alerts.example.com/hooks/autumn";
    let mut app = TestApp::new();
    // The webhook channel names its client with the (trimmed) URL, so the mock's
    // alias must equal that trimmed URL for the outbound POST to match.
    let mock = app
        .http_mock(trimmed)
        .post("/hooks/autumn")
        .respond_with_status(200);
    let client = app.build();

    let config = AlertConfig {
        webhook_url: Some(format!("  {trimmed}  ")),
        webhook_secret: Some("s3cr3t".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    // The channel must be installed despite the surrounding whitespace.
    assert!(
        client.state().extension::<Alerter>().is_some(),
        "a whitespace-padded but valid webhook_url must install the webhook channel"
    );

    // Fire an alert and wait for the detached delivery task to POST to the mock.
    autumn_web::alerts::notify_dead_lettered_job(client.state(), "reporting_job", "boom");
    for _ in 0..100 {
        if mock.call_count() >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    mock.expect_called(1);
}

/// Companion: BOTH `https://` and `http://` absolute URLs (with a secret) are
/// usable destinations and must register the webhook channel — the runtime rule
/// accepts either scheme, so the install path must too.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn webhook_with_absolute_http_and_https_urls_register_channel() {
    for url in [
        "https://alerts.example.com/hooks/autumn",
        "http://alerts.example.com/hooks/autumn",
    ] {
        let client = TestApp::new().build();
        let config = AlertConfig {
            webhook_url: Some(url.to_owned()),
            webhook_secret: Some("s3cr3t".to_owned()),
            ..AlertConfig::default()
        };
        autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());
        assert!(
            client.state().extension::<Alerter>().is_some(),
            "an absolute {url} webhook must install the webhook channel"
        );
    }
}

/// AC #1/#2: the built-in mail channel reuses the app's configured `Mailer` and
/// delivers the alert even to a suppressed address (alerts are security-class;
/// `ignore_suppression()` is called on the builder).
#[cfg(feature = "mail")]
#[tokio::test]
async fn mail_channel_reuses_mailer_and_bypasses_suppression() {
    use autumn_web::alerts::MailAlertChannel;
    use autumn_web::mail::suppression::{
        InMemorySuppressionStore, SuppressionReason, SuppressionStore, SuppressionStoreHandle,
    };
    use autumn_web::mail::{Mail, MailError, MailTransport, Mailer};

    #[derive(Clone, Default)]
    struct RecordingTransport {
        sent_to: Arc<Mutex<Vec<String>>>,
    }
    impl MailTransport for RecordingTransport {
        fn send<'a>(
            &'a self,
            mail: Mail,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.sent_to
                    .lock()
                    .expect("lock")
                    .extend(mail.to.iter().cloned());
                Ok(())
            })
        }
    }

    // The operator address is on the suppression list — a normal send would be
    // skipped, but an alert must still be delivered.
    let store = InMemorySuppressionStore::new();
    store
        .suppress("oncall@example.com", SuppressionReason::HardBounce)
        .await
        .expect("suppress");

    let transport = RecordingTransport::default();
    let mailer = Arc::new(
        Mailer::with_transport(transport.clone())
            .with_suppression(SuppressionStoreHandle::new(store)),
    );

    let channel = MailAlertChannel::new(mailer, "oncall@example.com");
    let alert = Alert::trigger(AlertCondition::HighErrorRate, "high_error_rate:5xx")
        .title("5xx spike")
        .summary("10% of requests are failing")
        .build();

    channel.deliver(&alert).await.expect("alert delivered");

    let recipients = transport.sent_to.lock().expect("lock").clone();
    assert_eq!(
        recipients,
        vec!["oncall@example.com".to_owned()],
        "alert delivered to the suppressed operator address (ignore_suppression)"
    );
}

/// Regression: with the `mail` feature on but the mail transport disabled
/// (the default `DisabledTransport`, or a prod profile turning it off), an
/// email-only alert config must NOT register a `MailAlertChannel`. That channel
/// would report `Ok(())` while the disabled transport silently drops the
/// message, making an email-only prod config look active while sending nothing.
/// `install_from_config` must skip the dead channel and, with no other
/// destination, install no alerter at all.
#[cfg(feature = "mail")]
#[tokio::test]
async fn disabled_mail_transport_does_not_register_email_channel() {
    use autumn_web::mail::{Mail, MailError, MailTransport, Mailer};

    #[derive(Clone, Default)]
    struct DisabledTestTransport;
    impl MailTransport for DisabledTestTransport {
        fn send<'a>(
            &'a self,
            _mail: Mail,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailError>> + Send + 'a>>
        {
            Box::pin(async move { Ok(()) })
        }
        fn is_disabled(&self) -> bool {
            true
        }
    }

    let mailer = Arc::new(Mailer::with_transport(DisabledTestTransport));
    assert!(mailer.is_disabled(), "sanity: transport reports disabled");

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension((*mailer).clone()))
        .build();

    let config = AlertConfig {
        email: Some("oncall@example.com".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    // The only configured destination was email over a disabled transport, so
    // no channel is registered and therefore no alerter is installed — the
    // `notify_*` hooks stay no-ops rather than pretending to deliver.
    assert!(
        client.state().extension::<Alerter>().is_none(),
        "email-only config over a disabled mail transport must not install an alerter"
    );
}

/// Companion to the above: an ENABLED mail transport with an email destination
/// DOES register the mail channel (proving the guard keys off `is_disabled`).
#[cfg(feature = "mail")]
#[tokio::test]
async fn enabled_mail_transport_registers_email_channel() {
    use autumn_web::mail::{Mail, MailError, MailTransport, Mailer};

    #[derive(Clone, Default)]
    struct EnabledTestTransport;
    impl MailTransport for EnabledTestTransport {
        fn send<'a>(
            &'a self,
            _mail: Mail,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailError>> + Send + 'a>>
        {
            Box::pin(async move { Ok(()) })
        }
    }

    let mailer = Arc::new(Mailer::with_transport(EnabledTestTransport));
    assert!(!mailer.is_disabled(), "sanity: transport reports enabled");

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension((*mailer).clone()))
        .build();

    let config = AlertConfig {
        email: Some("oncall@example.com".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_some(),
        "an enabled mail transport with an email destination must install the mail channel"
    );
}

/// A non-disabled mail transport that would `send` fine at the transport layer,
/// used to isolate the address/`from` preconditions from the disabled-transport
/// skip. `is_disabled` returns the trait default (`false`).
#[cfg(feature = "mail")]
#[derive(Clone, Default)]
struct UsableTestTransport;

#[cfg(feature = "mail")]
impl autumn_web::mail::MailTransport for UsableTestTransport {
    fn send<'a>(
        &'a self,
        _mail: autumn_web::mail::Mail,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), autumn_web::mail::MailError>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(()) })
    }
}

/// Regression: lettre parses `[alerts] email` only at SEND time, so a
/// present-but-unparsable address (`not-an-address`, a `mailto:` URI) passes
/// every earlier gate yet fails EVERY delivery with `MailError::InvalidAddress`.
/// With a usable (non-disabled) transport, `install_from_config` must still SKIP
/// the mail channel — and, with no other destination, install no alerter — rather
/// than register a channel that can never deliver.
#[cfg(feature = "mail")]
#[tokio::test]
async fn invalid_email_address_does_not_register_email_channel() {
    use autumn_web::mail::Mailer;

    for invalid in ["not-an-address", "mailto:oncall@example.com"] {
        let mailer = Arc::new(Mailer::with_transport(UsableTestTransport));
        assert!(!mailer.is_disabled(), "sanity: transport reports enabled");

        // A usable, `from`-free transport, so the ONLY reason to skip is the
        // malformed address.
        let mut config_full = autumn_web::config::AutumnConfig::default();
        config_full.mail.transport = autumn_web::mail::Transport::Log;

        let client = TestApp::new()
            .config(config_full)
            .state_initializer(move |state| state.insert_extension((*mailer).clone()))
            .build();

        let config = AlertConfig {
            email: Some(invalid.to_owned()),
            ..AlertConfig::default()
        };
        autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

        assert!(
            client.state().extension::<Alerter>().is_none(),
            "an invalid [alerts] email ({invalid}) must not install a mail channel"
        );
    }
}

/// Regression: the alert mail carries no per-message `from`, so it falls back to
/// the mailer default (`[mail] from`). Under `transport = "smtp"` with no `[mail]
/// from`, `lettre_message` fails with "mail from address is required" and every
/// delivery fails. `install_from_config` must SKIP the mail channel — and, with
/// no other destination, install no alerter.
#[cfg(feature = "mail")]
#[tokio::test]
async fn smtp_transport_without_from_does_not_register_email_channel() {
    use autumn_web::mail::Mailer;

    let mailer = Arc::new(Mailer::with_transport(UsableTestTransport));
    assert!(!mailer.is_disabled(), "sanity: transport reports enabled");

    let mut config_full = autumn_web::config::AutumnConfig::default();
    config_full.mail.transport = autumn_web::mail::Transport::Smtp;
    // A host is required only so `TestApp` can build its throwaway config mailer;
    // our `UsableTestTransport` mailer (inserted below) is what the alerter uses.
    config_full.mail.smtp.host = Some("smtp.example.com".to_owned());
    config_full.mail.from = None;

    let client = TestApp::new()
        .config(config_full)
        .state_initializer(move |state| state.insert_extension((*mailer).clone()))
        .build();

    let config = AlertConfig {
        email: Some("oncall@example.com".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_none(),
        "an SMTP transport with no [mail] from must not install a mail channel"
    );
}

/// Companion: a valid address on an SMTP transport WITH a `[mail] from` set is a
/// working destination, so `install_from_config` registers the mail channel.
#[cfg(feature = "mail")]
#[tokio::test]
async fn smtp_transport_with_from_registers_email_channel() {
    use autumn_web::mail::Mailer;

    let mailer = Arc::new(Mailer::with_transport(UsableTestTransport));
    assert!(!mailer.is_disabled(), "sanity: transport reports enabled");

    let mut config_full = autumn_web::config::AutumnConfig::default();
    config_full.mail.transport = autumn_web::mail::Transport::Smtp;
    // A host is required only so `TestApp` can build its throwaway config mailer;
    // our `UsableTestTransport` mailer (inserted below) is what the alerter uses.
    config_full.mail.smtp.host = Some("smtp.example.com".to_owned());
    config_full.mail.from = Some("alerts@example.com".to_owned());

    let client = TestApp::new()
        .config(config_full)
        .state_initializer(move |state| state.insert_extension((*mailer).clone()))
        .build();

    let config = AlertConfig {
        email: Some("oncall@example.com".to_owned()),
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

    assert!(
        client.state().extension::<Alerter>().is_some(),
        "a valid email on an SMTP transport with [mail] from must install the mail channel"
    );
}

/// Regression (no over-gating): the `log` and `file` transports render `from`
/// only when present and deliver without it, so a valid email on a `log`
/// transport with NO `[mail] from` is a working destination —
/// `install_from_config` must still register the mail channel. Only SMTP is
/// blocked by a missing `from`.
#[cfg(feature = "mail")]
#[tokio::test]
async fn log_transport_without_from_registers_email_channel() {
    use autumn_web::mail::Mailer;

    for transport in [
        autumn_web::mail::Transport::Log,
        autumn_web::mail::Transport::File,
    ] {
        let mailer = Arc::new(Mailer::with_transport(UsableTestTransport));
        assert!(!mailer.is_disabled(), "sanity: transport reports enabled");

        let mut config_full = autumn_web::config::AutumnConfig::default();
        config_full.mail.transport = transport;
        config_full.mail.from = None;

        let client = TestApp::new()
            .config(config_full)
            .state_initializer(move |state| state.insert_extension((*mailer).clone()))
            .build();

        let config = AlertConfig {
            email: Some("oncall@example.com".to_owned()),
            ..AlertConfig::default()
        };
        autumn_web::alerts::install_from_config(client.state(), &config, Vec::new());

        assert!(
            client.state().extension::<Alerter>().is_some(),
            "a {transport:?} transport with no [mail] from must still install the mail channel"
        );
    }
}

/// AC #1 (fan-out): every registered channel receives the alert.
#[tokio::test]
async fn alert_fans_out_to_every_channel() {
    let a = CapturingChannel::default();
    let b = CapturingChannel::default();
    let alerter = Alerter::new(
        vec![Arc::new(a.clone()), Arc::new(b.clone())],
        test_settings(),
    );

    let client = TestApp::new()
        .state_initializer(move |state| state.insert_extension(alerter.clone()))
        .build();

    autumn_web::alerts::notify_dead_lettered_job(client.state(), "widget_job", "boom");

    wait_for(&a, 1).await;
    wait_for(&b, 1).await;
    assert_eq!(a.alerts().len(), 1);
    assert_eq!(b.alerts().len(), 1);
}

/// Master switch: `enabled = false` silences EVERYTHING, including a custom
/// channel registered via the builder. `install_from_config` must install no
/// alerter (so the `notify_*` hooks are no-ops) and must not deliver anything to
/// the custom channel nor spawn the evaluation loop.
#[tokio::test]
async fn disabled_master_switch_silences_custom_channels() {
    let channel = CapturingChannel::default();

    let client = TestApp::new().build();

    // A production-shaped config with alerting fully disabled, wired together
    // with a custom channel exactly as `with_alert_channel` would surface it.
    let config = AlertConfig {
        enabled: false,
        ..AlertConfig::default()
    };
    autumn_web::alerts::install_from_config(
        client.state(),
        &config,
        vec![Arc::new(channel.clone())],
    );

    // With the master switch off, no alerter is installed onto state.
    assert!(
        client.state().extension::<Alerter>().is_none(),
        "disabled alerting must not install an alerter (hooks stay no-ops)"
    );

    // Triggering a condition through the real seam delivers nothing.
    autumn_web::alerts::notify_dead_lettered_job(client.state(), "reporting_job", "boom");
    autumn_web::alerts::notify_scheduled_task_failure(client.state(), "nightly_backup", "boom");

    // Give any (erroneously) spawned delivery task a chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        channel.alerts().is_empty(),
        "custom channel must receive nothing when alerting is disabled: {:?}",
        channel.alerts()
    );
}

/// AC #6 (fail-safe): a channel that always errors never propagates the failure
/// to the caller — the emit returns normally and the app keeps running.
#[tokio::test]
async fn unreachable_channel_does_not_break_the_caller() {
    struct FailingChannel;
    impl AlertChannel for FailingChannel {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn deliver<'a>(&'a self, _alert: &'a Alert) -> AlertDeliveryFuture<'a> {
            Box::pin(async move { Err(AlertDeliveryError::new("failing", "unreachable")) })
        }
    }

    let alerter = Alerter::new(vec![Arc::new(FailingChannel)], test_settings());
    // notify() returns synchronously with the dedup decision; delivery happens
    // on a detached task and its failure is only logged.
    let dispatched = alerter.notify(
        Alert::trigger(AlertCondition::DeadLetteredJob, "dead_lettered_job:x")
            .title("x")
            .build(),
    );
    assert!(
        dispatched,
        "emit returns without surfacing the delivery error"
    );
    // Give the detached task a chance to run and swallow its error.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}
