//! Rejection-counter coverage for mutual TLS (issue #1640).
//!
//! Its own `[[test]]` binary, deliberately: the metric registry is a
//! process-global singleton with a bounded instrument count, so a suite that
//! reads it back cannot share a process with the ~1700 tests in the
//! consolidated `integration_tests` binary — the instruments it asserts on
//! would be evicted before the first assertion ran. See CLAUDE.md,
//! "Process-global state needs its own binary".

#![cfg(feature = "tls")]

#[path = "integration/mtls_support.rs"]
mod mtls_support;
// Included whole because `mtls_support` needs its fixtures and parser; this
// binary exercises only the rejection counters, so most of it is unused here.
// The consolidated `integration_tests` binary is what covers the rest.
#[allow(dead_code)]
#[path = "integration/tls_support.rs"]
mod tls_support;

use autumn_web::config::ClientAuthMode;

use crate::mtls_support::{
    CA_PEM, CRL_PEM, REVOKED_CERT_PEM, REVOKED_KEY_PEM, TrustFixture, UNTRUSTED_CERT_PEM,
    UNTRUSTED_KEY_PEM, eventually, mtls_get, serve_mtls,
};

/// The current value of `tls_client_auth_rejected_total` for `reason`.
///
/// Metrics are process-global, so these tests read a DELTA around the request
/// under test rather than an absolute — a sibling test rejecting for the same
/// reason must not be able to make this one pass or fail.
fn rejected_count(reason: &str) -> u64 {
    autumn_web::metrics::snapshot()
        .into_iter()
        .find(|i| i.name == autumn_web::tls::client_auth::REJECTED_METRIC)
        .and_then(|i| {
            i.series
                .into_iter()
                .find(|s| s.labels.get("reason").is_some_and(|r| r == reason))
                .map(|s| match s.value {
                    autumn_web::metrics::SeriesValue::Counter { value } => value,
                    _ => 0,
                })
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn every_rejection_reason_is_counted_for_the_operator() {
    // AC: rejection counts reach the metrics/actuator seams, with a
    // distinguishing reason. `no_certificate` in particular never reaches the
    // certificate verifier — rustls raises it before calling one — so it can
    // only be counted where the handshake error surfaces.
    let trust = TrustFixture::write(CA_PEM, Some(CRL_PEM));
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    for (reason, client) in [
        ("no_certificate", None),
        (
            "untrusted_ca",
            Some((UNTRUSTED_CERT_PEM, UNTRUSTED_KEY_PEM)),
        ),
        ("revoked", Some((REVOKED_CERT_PEM, REVOKED_KEY_PEM))),
    ] {
        let before = rejected_count(reason);
        let outcome = mtls_get(server.addr, "/open", client).await;
        assert!(outcome.is_err(), "{reason} should be rejected");
        // The handshake failure surfaces client-side as soon as the alert
        // arrives, which can beat the server task that records it.
        eventually(&format!("the {reason} rejection is counted"), async || {
            rejected_count(reason) > before
        })
        .await;
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_route_level_rejection_is_counted_separately() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(
        &trust,
        ClientAuthMode::Optional,
        vec!["/internal/".to_owned()],
    )
    .await;

    let before = autumn_web::metrics::snapshot()
        .into_iter()
        .find(|i| i.name == autumn_web::tls::client_auth::ROUTE_REJECTED_METRIC)
        .and_then(|i| {
            i.series.into_iter().next().map(|s| match s.value {
                autumn_web::metrics::SeriesValue::Counter { value } => value,
                _ => 0,
            })
        })
        .unwrap_or(0);

    let denied = mtls_get(server.addr, "/internal/keys", None)
        .await
        .expect("connection succeeds");
    assert_eq!(denied.status, 403);

    let after = autumn_web::metrics::snapshot()
        .into_iter()
        .find(|i| i.name == autumn_web::tls::client_auth::ROUTE_REJECTED_METRIC)
        .and_then(|i| {
            i.series.into_iter().next().map(|s| match s.value {
                autumn_web::metrics::SeriesValue::Counter { value } => value,
                _ => 0,
            })
        })
        .unwrap_or(0);
    assert!(
        after > before,
        "a route-level mTLS refusal must be observable, not just a 403"
    );

    server.shutdown().await;
}
