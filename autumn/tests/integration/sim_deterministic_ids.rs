//! Deterministic identifier stream under a seeded entropy source (issue #1797,
//! W3). Proves the framework's high-value id sites replay byte-for-byte under a
//! fixed seed — the W3 Definition of Done — with no Docker.
//!
//! - The shared [`SeededEntropy`] primitive is byte-identical across two equal
//!   seeds (every migrated site draws from this).
//! - The request-id site is exercised **end-to-end through the real router**:
//!   two apps built with the same seeded entropy mint an identical
//!   `X-Request-Id` stream, and a differently-seeded app diverges.
//! - The seed-derived `derive_uuid(purpose_tag)` namespace helper is stable and
//!   order-independent.
//!
//! The remaining three migrated sites (jobs, idempotency, session) are proven
//! at the unit level next to their code (they draw from the same source): see
//! `job::tests::job_ids_are_deterministic_under_seeded_entropy`,
//! `idempotency::tests::in_flight_lock_owner_is_deterministic_under_seeded_entropy`,
//! and `session::tests::rotate_id_is_deterministic_under_seeded_entropy`.

use autumn_web::entropy::{Entropy, SeededEntropy};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// The shared seeded primitive every migrated id site draws from is
/// byte-identical under the same seed and diverges under a different seed.
#[test]
fn seeded_entropy_stream_is_byte_identical() {
    let a = SeededEntropy::new(0x5eed);
    let b = SeededEntropy::new(0x5eed);
    let stream_a: Vec<String> = (0..64).map(|_| a.uuid_v4().to_string()).collect();
    let stream_b: Vec<String> = (0..64).map(|_| b.uuid_v4().to_string()).collect();
    assert_eq!(stream_a, stream_b, "same seed ⇒ identical uuid stream");

    let c = SeededEntropy::new(0x1234);
    let stream_c: Vec<String> = (0..64).map(|_| c.uuid_v4().to_string()).collect();
    assert_ne!(stream_a, stream_c, "different seed ⇒ different stream");
}

/// End-to-end through the real router: `RequestIdLayer` mints the `X-Request-Id`
/// from the app's injected entropy, so two apps seeded identically produce the
/// same request-id stream and a differently-seeded app diverges.
#[tokio::test]
async fn request_ids_are_deterministic_end_to_end() {
    async fn first_ids(seed: u64) -> Vec<String> {
        let client = TestApp::new()
            .routes(routes![ping])
            .with_entropy(SeededEntropy::new(seed))
            .build();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let resp = client.get("/ping").send().await;
            resp.assert_ok();
            ids.push(
                resp.header("x-request-id")
                    .expect("X-Request-Id is stamped by RequestIdLayer")
                    .to_owned(),
            );
        }
        ids
    }

    let a = first_ids(0x5eed).await;
    let b = first_ids(0x5eed).await;
    assert_eq!(a.len(), 5);
    // Each request id is a well-formed UUID.
    assert!(uuid::Uuid::parse_str(&a[0]).is_ok());
    assert_eq!(
        a, b,
        "same seed ⇒ byte-identical X-Request-Id stream through the real router"
    );

    let c = first_ids(0x1234).await;
    assert_ne!(a, c, "a different seed ⇒ different request ids");
}

/// The seed-derived namespace helper is stable, tag-sensitive, and independent
/// of the main draw stream (order-independent) — usable for byte-reproducible
/// multi-tenant fixtures.
#[test]
fn derive_uuid_is_namespaced_and_order_independent() {
    let e = SeededEntropy::new(0x5eed);
    let acme = e.derive_uuid("tenant:acme");
    // Draw from the main stream in between; derivation must be unaffected.
    let _ = e.uuid_v4();
    let _ = e.next_u64();
    assert_eq!(acme, e.derive_uuid("tenant:acme"), "order-independent");
    assert_ne!(acme, e.derive_uuid("tenant:beta"), "tag-sensitive");
    assert_eq!(
        acme,
        SeededEntropy::new(0x5eed).derive_uuid("tenant:acme"),
        "reproducible from a fresh source with the same seed",
    );
    assert_ne!(
        acme,
        SeededEntropy::new(0x1234).derive_uuid("tenant:acme"),
        "diverges by seed",
    );
}
