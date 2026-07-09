use autumn_web::config::AutumnConfig;
use autumn_web::http_client::{Client, ClientError};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use axum::http::StatusCode;
use axum::{Router, routing::get};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(feature = "mail")]
use autumn_web::mail::{Mail, Mailer, Transport};

#[get("/call-downstream/{port}")]
async fn call_downstream(
    client: Client,
    axum::extract::Path(port): axum::extract::Path<u16>,
) -> Result<String, (StatusCode, String)> {
    let url = format!("http://127.0.0.1:{port}/downstream-target");
    match client.get(&url).send().await {
        Ok(res) => Ok(format!("Status: {}", res.status())),
        Err(ClientError::CircuitBreakerOpen) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "circuit breaker open".to_string(),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

#[get("/call-downstream-result/{port}")]
async fn call_downstream_result(
    client: Client,
    axum::extract::Path(port): axum::extract::Path<u16>,
) -> AutumnResult<String> {
    let url = format!("http://127.0.0.1:{port}/downstream-target");
    let res = client.get(&url).send().await?;
    Ok(format!("Status: {}", res.status()))
}

#[cfg(feature = "mail")]
#[get("/send-mail-result")]
async fn send_mail_result(mailer: Mailer) -> AutumnResult<&'static str> {
    let mail = Mail::builder()
        .from("noreply@example.com")
        .to("ada@example.com")
        .subject("Test")
        .text("hello")
        .build()?;
    mailer.send(mail).await?;
    Ok("ok")
}

#[get("/unrelated")]
async fn unrelated() -> &'static str {
    "unrelated ok"
}

#[tokio::test]
#[allow(clippy::too_many_lines, clippy::await_holding_lock)]
async fn test_circuit_breaker_downstream_outage_flow() {
    let _lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();
    // 1. Setup mock downstream server that can simulate an outage
    let is_outage = Arc::new(AtomicBool::new(false));
    let is_outage_clone = is_outage.clone();
    let mock_app = Router::new().route(
        "/downstream-target",
        get(move || {
            let outage = is_outage_clone.load(Ordering::SeqCst);
            async move {
                if outage {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    // 2. Configure Autumn app with short circuit breaker settings for testing
    let mut config = AutumnConfig::default();
    config.health.detailed = true;
    config
        .resilience
        .circuit_breaker
        .defaults
        .failure_ratio_threshold = Some(0.5);
    config
        .resilience
        .circuit_breaker
        .defaults
        .minimum_sample_count = Some(2);
    config
        .resilience
        .circuit_breaker
        .defaults
        .open_duration_secs = Some(1); // 1s open duration
    config
        .resilience
        .circuit_breaker
        .defaults
        .sample_window_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .half_open_trial_count = Some(1);

    let client = TestApp::new()
        .config(config)
        .routes(routes![call_downstream, unrelated])
        .build();

    // ── CLOSED State ──
    // Happy path request
    let resp = client.get(&format!("/call-downstream/{port}")).send().await;
    resp.assert_ok();

    // /actuator/health is UP
    let health_resp = client.get("/actuator/health").send().await;
    health_resp.assert_ok();
    health_resp.assert_json::<serde_json::Value, _>(|val| {
        assert_eq!(val["status"], "UP");
    });

    // ── Downstream Outage triggers ──
    is_outage.store(true, Ordering::SeqCst);

    // Fail 2 times to trip the breaker (minimum_sample_count = 2)
    for i in 0..2 {
        let resp = client.get(&format!("/call-downstream/{port}")).send().await;
        println!(
            "FAIL REQUEST {}: status={}, body={}",
            i,
            resp.status,
            resp.text()
        );
    }

    println!(
        "BREAKERS AFTER FAILURES: {:?}",
        autumn_web::circuit_breaker::global_registry()
            .all_breakers()
            .iter()
            .map(|b| (b.name().to_string(), b.state(), b.failure_ratio(),))
            .collect::<Vec<_>>()
    );

    // ── OPEN State ──
    // Next request should fail fast with 503 SERVICE_UNAVAILABLE from the breaker
    let start_fast = std::time::Instant::now();
    let resp_open = client.get(&format!("/call-downstream/{port}")).send().await;
    println!(
        "OPEN REQUEST status={}, body={}",
        resp_open.status,
        resp_open.text()
    );
    resp_open.assert_status(503);
    assert_eq!(resp_open.text(), "circuit breaker open");
    let fast_elapsed = start_fast.elapsed();
    assert!(
        fast_elapsed < Duration::from_millis(100),
        "Should fail fast under 100ms"
    );

    // Unrelated route latency stays low
    let start_unrelated = std::time::Instant::now();
    let resp_unrelated = client.get("/unrelated").send().await;
    resp_unrelated.assert_ok();
    assert_eq!(resp_unrelated.text(), "unrelated ok");
    let unrelated_elapsed = start_unrelated.elapsed();
    assert!(
        unrelated_elapsed < Duration::from_millis(50),
        "Unrelated latency must be very low"
    );

    // /health (compatibility) stays UP!
    let comp_health = client.get("/health").send().await;
    comp_health.assert_ok();
    comp_health.assert_json::<serde_json::Value, _>(|val| {
        assert_eq!(val["status"], "ok");
    });

    // /actuator/health is DOWN!
    let act_health = client.get("/actuator/health").send().await;
    act_health.assert_status(503);
    act_health.assert_json::<serde_json::Value, _>(|val| {
        assert_eq!(val["status"], "DOWN");
        let cb_details = &val["components"][&format!("circuit_breaker.127.0.0.1:{port}")];
        assert_eq!(cb_details["status"], "DOWN");
        assert_eq!(cb_details["details"]["state"], "OPEN");
    });

    // ── HALF-OPEN & CLOSED State Recovery ──
    // Wait for the open_duration (1s) to expire so it transitions to HalfOpen
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Resolve downstream outage
    is_outage.store(false, Ordering::SeqCst);

    // This request will be a trial in HalfOpen. Since downstream is healthy, it should succeed,
    // and since half_open_trial_count = 1, it should transition the breaker back to Closed.
    let resp_recover = client.get(&format!("/call-downstream/{port}")).send().await;
    resp_recover.assert_ok();

    // Now the breaker is Closed again. Subsequent request is successful.
    let resp_final = client.get(&format!("/call-downstream/{port}")).send().await;
    resp_final.assert_ok();

    // /actuator/health returns to UP
    let act_health_final = client.get("/actuator/health").send().await;
    act_health_final.assert_ok();
    act_health_final.assert_json::<serde_json::Value, _>(|val| {
        assert_eq!(val["status"], "UP");
        let cb_details = &val["components"][&format!("circuit_breaker.127.0.0.1:{port}")];
        assert_eq!(cb_details["status"], "UP");
        assert_eq!(cb_details["details"]["state"], "CLOSED");
    });
    autumn_web::circuit_breaker::global_registry().clear();
}

#[tokio::test]
#[allow(clippy::too_many_lines, clippy::await_holding_lock)]
async fn test_circuit_breaker_distinct_ports() {
    let _lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();

    // Setup two mock downstream servers on different ports
    // Port 1 always returns 500 (internal error)
    let mock_app_1 = Router::new().route(
        "/downstream-target",
        get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let listener_1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port1 = listener_1.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener_1, mock_app_1).await.unwrap();
    });

    // Port 2 always returns 200 (OK)
    let mock_app_2 = Router::new().route("/downstream-target", get(|| async { StatusCode::OK }));
    let listener_2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port2 = listener_2.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener_2, mock_app_2).await.unwrap();
    });

    // Configure Autumn app with minimum_sample_count = 2
    let mut config = AutumnConfig::default();
    config.health.detailed = true;
    config
        .resilience
        .circuit_breaker
        .defaults
        .failure_ratio_threshold = Some(0.5);
    config
        .resilience
        .circuit_breaker
        .defaults
        .minimum_sample_count = Some(2);
    config
        .resilience
        .circuit_breaker
        .defaults
        .open_duration_secs = Some(60);
    config
        .resilience
        .circuit_breaker
        .defaults
        .sample_window_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .half_open_trial_count = Some(1);

    let client = TestApp::new()
        .config(config)
        .routes(routes![call_downstream, unrelated])
        .build();

    // Call port 1 (failures) twice to trip its breaker
    let _ = client
        .get(&format!("/call-downstream/{port1}"))
        .send()
        .await;
    let _ = client
        .get(&format!("/call-downstream/{port1}"))
        .send()
        .await;

    // Call port 2 (successes) twice
    let resp2_1 = client
        .get(&format!("/call-downstream/{port2}"))
        .send()
        .await;
    resp2_1.assert_ok();
    let resp2_2 = client
        .get(&format!("/call-downstream/{port2}"))
        .send()
        .await;
    resp2_2.assert_ok();

    // Port 1 should fail fast now due to Open circuit breaker
    let resp1_fast = client
        .get(&format!("/call-downstream/{port1}"))
        .send()
        .await;
    resp1_fast.assert_status(503);
    assert_eq!(resp1_fast.text(), "circuit breaker open");

    // Port 2 should still be completely healthy and closed!
    let resp2_fast = client
        .get(&format!("/call-downstream/{port2}"))
        .send()
        .await;
    resp2_fast.assert_ok();

    autumn_web::circuit_breaker::global_registry().clear();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_circuit_breaker_blanket_from_http_client() {
    let _lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();

    let is_outage = Arc::new(AtomicBool::new(false));
    let is_outage_clone = is_outage.clone();
    let mock_app = Router::new().route(
        "/downstream-target",
        get(move || {
            let outage = is_outage_clone.load(Ordering::SeqCst);
            async move {
                if outage {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let mut config = AutumnConfig::default();
    config.health.detailed = true;
    config
        .resilience
        .circuit_breaker
        .defaults
        .failure_ratio_threshold = Some(0.5);
    config
        .resilience
        .circuit_breaker
        .defaults
        .minimum_sample_count = Some(2);
    config
        .resilience
        .circuit_breaker
        .defaults
        .open_duration_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .sample_window_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .half_open_trial_count = Some(1);

    let client = TestApp::new()
        .config(config)
        .routes(routes![call_downstream_result])
        .build();

    // CLOSED state
    let resp = client
        .get(&format!("/call-downstream-result/{port}"))
        .send()
        .await;
    resp.assert_ok();

    // Outage starts
    is_outage.store(true, Ordering::SeqCst);

    // Fail twice to trip the breaker
    for _ in 0..2 {
        let _ = client
            .get(&format!("/call-downstream-result/{port}"))
            .send()
            .await;
    }

    // Next request should fail fast with 503 from the mapped ClientError::CircuitBreakerOpen
    let resp_open = client
        .get(&format!("/call-downstream-result/{port}"))
        .send()
        .await;
    resp_open.assert_status(503);
    assert!(
        resp_open
            .text()
            .contains("outbound circuit breaker is open")
    );

    autumn_web::circuit_breaker::global_registry().clear();
}

#[tokio::test]
#[cfg(feature = "mail")]
#[allow(clippy::await_holding_lock)]
async fn test_circuit_breaker_blanket_from_smtp() {
    let _lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();

    let mut config = AutumnConfig::default();
    config.mail.transport = Transport::Smtp;
    config.mail.smtp.host = Some("127.0.0.1".to_string());
    config.mail.smtp.port = Some(1); // non-existent port to force connection failures
    config
        .resilience
        .circuit_breaker
        .defaults
        .failure_ratio_threshold = Some(0.5);
    config
        .resilience
        .circuit_breaker
        .defaults
        .minimum_sample_count = Some(2);
    config
        .resilience
        .circuit_breaker
        .defaults
        .open_duration_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .sample_window_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .half_open_trial_count = Some(1);

    let client = TestApp::new()
        .config(config)
        .routes(routes![send_mail_result])
        .build();

    // Trigger two failures (connection refused)
    let _ = client.get("/send-mail-result").send().await;
    let _ = client.get("/send-mail-result").send().await;

    // The third attempt should fail fast with 503 from SMTP breaker open mapped in From
    let resp_open = client.get("/send-mail-result").send().await;
    resp_open.assert_status(503);
    assert!(
        resp_open
            .text()
            .contains("smtp mailer circuit breaker is open")
    );

    autumn_web::circuit_breaker::global_registry().clear();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_circuit_breaker_non_detailed_unhealthy_visibility() {
    let _lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();

    let is_outage = Arc::new(AtomicBool::new(false));
    let is_outage_clone = is_outage.clone();
    let mock_app = Router::new().route(
        "/downstream-target",
        get(move || {
            let outage = is_outage_clone.load(Ordering::SeqCst);
            async move {
                if outage {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let mut config = AutumnConfig::default();
    config.health.detailed = false; // non-detailed mode!
    config
        .resilience
        .circuit_breaker
        .defaults
        .failure_ratio_threshold = Some(0.5);
    config
        .resilience
        .circuit_breaker
        .defaults
        .minimum_sample_count = Some(2);
    config
        .resilience
        .circuit_breaker
        .defaults
        .open_duration_secs = Some(10);

    let client = TestApp::new()
        .config(config)
        .routes(routes![call_downstream])
        .build();

    // 1. Initially CLOSED (UP). It should NOT be visible because health.detailed = false.
    let health_resp_1 = client.get("/actuator/health").send().await;
    health_resp_1.assert_ok();
    health_resp_1.assert_json::<serde_json::Value, _>(|val| {
        assert_eq!(val["status"], "UP");
        assert!(
            val["components"]
                .get(format!("circuit_breaker.127.0.0.1:{port}"))
                .is_none()
        );
    });

    // 2. Trigger failures to trip the breaker
    is_outage.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let _ = client.get(&format!("/call-downstream/{port}")).send().await;
    }

    // 3. Now OPEN (DOWN). It SHOULD be visible because it is unhealthy (DOWN), but its details should be omitted!
    let health_resp_2 = client.get("/actuator/health").send().await;
    health_resp_2.assert_status(503);
    health_resp_2.assert_json::<serde_json::Value, _>(|val| {
        assert_eq!(val["status"], "DOWN");
        let cb = &val["components"][&format!("circuit_breaker.127.0.0.1:{port}")];
        assert_eq!(cb["status"], "DOWN");
        assert!(cb.get("details").is_none() || cb["details"].is_null());
    });

    autumn_web::circuit_breaker::global_registry().clear();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_circuit_breaker_half_open_suppresses_retries() {
    let _lock = autumn_web::circuit_breaker::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::circuit_breaker::global_registry().clear();

    // 1. Setup mock downstream server that counts hits
    let hit_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hit_count_clone = hit_count.clone();
    let mock_app = Router::new().route(
        "/downstream-target",
        get(move || {
            let hits = hit_count_clone.fetch_add(1, Ordering::SeqCst);
            println!("Downstream hit: {}", hits + 1);
            async move { StatusCode::INTERNAL_SERVER_ERROR }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    // 2. Configure Autumn app with short circuit settings
    let mut config = AutumnConfig::default();
    config.health.detailed = true;
    config
        .resilience
        .circuit_breaker
        .defaults
        .failure_ratio_threshold = Some(0.5);
    config
        .resilience
        .circuit_breaker
        .defaults
        .minimum_sample_count = Some(2);
    config
        .resilience
        .circuit_breaker
        .defaults
        .open_duration_secs = Some(1); // 1s open
    config
        .resilience
        .circuit_breaker
        .defaults
        .sample_window_secs = Some(10);
    config
        .resilience
        .circuit_breaker
        .defaults
        .half_open_trial_count = Some(1);

    let client = TestApp::new()
        .config(config)
        .routes(routes![call_downstream])
        .build();

    // Trip the breaker to OPEN state by failing 2 times
    let _ = client.get(&format!("/call-downstream/{port}")).send().await;
    let _ = client.get(&format!("/call-downstream/{port}")).send().await;

    // Reset hit counter after tripping
    hit_count.store(0, Ordering::SeqCst);

    // Wait for open duration to expire (breaker moves to HalfOpen on next request)
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Send a request. Since the breaker is in HalfOpen, retries should be suppressed.
    // Downstream always returns 500. Without suppression, it would retry 3 times (4 attempts total).
    // With suppression, it should only hit downstream exactly 1 time.
    let resp = client.get(&format!("/call-downstream/{port}")).send().await;
    resp.assert_ok();
    assert!(resp.text().contains("500"));

    let total_hits = hit_count.load(Ordering::SeqCst);
    assert_eq!(
        total_hits, 1,
        "HalfOpen trial probe must issue exactly 1 downstream request"
    );

    autumn_web::circuit_breaker::global_registry().clear();
}

#[tokio::test]
async fn test_circuit_breaker_generic_open_error_mapping() {
    use autumn_web::circuit_breaker::{CircuitBreaker, CircuitBreakerError, CircuitBreakerPolicy};
    use autumn_web::error::AutumnError;

    let policy = CircuitBreakerPolicy {
        minimum_sample_count: 1,
        failure_ratio_threshold: 0.1,
        ..Default::default()
    };
    let breaker = CircuitBreaker::new("test_generic_open", policy);

    // Trip the breaker by running a failing future
    let res = breaker
        .run(async { Result::<(), std::io::Error>::Err(std::io::Error::other("failure")) })
        .await;
    assert!(res.is_err());

    // Now execution via run should return CircuitBreakerError::Open immediately
    let res2 = breaker
        .run(async { Result::<(), std::io::Error>::Ok(()) })
        .await;

    assert!(res2.is_err());
    let err = res2.unwrap_err();
    assert!(matches!(err, CircuitBreakerError::Open));

    // Convert to AutumnError
    let autumn_err = AutumnError::from(err);
    assert_eq!(autumn_err.status(), StatusCode::SERVICE_UNAVAILABLE);
}
