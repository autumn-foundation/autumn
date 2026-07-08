//! Tests for ISR coordination: lock-key derivation and coordinator behavior.
//!
//! These tests exercise the public API of `autumn_web::static_gen::isr_coordinator`
//! without requiring a real database or network. The Postgres coordinator is
//! covered by integration tests in the `bookmarks-distributed` example; here
//! we focus on the local coordinator and the pure key-derivation functions.

use std::sync::Arc;

use autumn_web::static_gen::isr_coordinator::{
    IsrCoordinator, LocalIsrCoordinator, isr_advisory_lock_key, isr_window_key,
};

// ---------------------------------------------------------------------------
// isr_window_key: stability within a window, change across windows
// ---------------------------------------------------------------------------

#[test]
fn isr_window_key_is_stable_within_interval() {
    // Bucket 28_333_333 covers unix seconds [1_699_999_980, 1_700_000_039].
    // Two timestamps within that range must produce the same key.
    let key_a = isr_window_key("/about", 60, 1_700_000_000);
    let key_b = isr_window_key("/about", 60, 1_700_000_039);
    assert_eq!(
        key_a, key_b,
        "same revalidation window should yield the same key"
    );
}

#[test]
fn isr_window_key_changes_on_new_interval() {
    // 1_700_000_039 is the last second of bucket 28_333_333.
    // 1_700_000_040 is the first second of bucket 28_333_334.
    let key_a = isr_window_key("/about", 60, 1_700_000_039);
    let key_b = isr_window_key("/about", 60, 1_700_000_040);
    assert_ne!(
        key_a, key_b,
        "adjacent revalidation windows should yield different keys"
    );
}

#[test]
fn isr_window_key_includes_route_path() {
    // Different routes in the same time window must produce different keys so
    // that their distributed locks are independent.
    let home = isr_window_key("/", 60, 1_700_000_010);
    let about = isr_window_key("/about", 60, 1_700_000_010);
    assert_ne!(
        home, about,
        "different routes must produce different window keys"
    );
}

#[test]
fn isr_window_key_handles_sub_second_bucket_boundary() {
    // The 1-second revalidate edge: timestamps 99 and 100 cross a bucket.
    let key_a = isr_window_key("/live", 1, 99);
    let key_b = isr_window_key("/live", 1, 100);
    assert_ne!(key_a, key_b);
}

#[test]
fn isr_window_key_revalidate_zero_treated_as_one() {
    // A revalidate of 0 should not cause a division-by-zero panic.
    let key = isr_window_key("/edge", 0, 1_700_000_000);
    assert!(!key.is_empty());
}

// ---------------------------------------------------------------------------
// isr_advisory_lock_key: deterministic, route- and window-sensitive i64
// ---------------------------------------------------------------------------

#[test]
fn isr_advisory_lock_key_is_stable() {
    let a = isr_advisory_lock_key("/about", "/about:28333333");
    let b = isr_advisory_lock_key("/about", "/about:28333333");
    assert_eq!(a, b, "advisory lock key must be deterministic");
}

#[test]
fn isr_advisory_lock_key_differs_by_route() {
    let home = isr_advisory_lock_key("/", "/about:28333333");
    let about = isr_advisory_lock_key("/about", "/about:28333333");
    assert_ne!(home, about);
}

#[test]
fn isr_advisory_lock_key_differs_by_window() {
    let window_a = isr_advisory_lock_key("/about", "/about:28333333");
    let window_b = isr_advisory_lock_key("/about", "/about:28333334");
    assert_ne!(window_a, window_b);
}

// ---------------------------------------------------------------------------
// LocalIsrCoordinator: in-process acquisition / release
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_coordinator_always_grants_acquisition() {
    // LocalIsrCoordinator is a true no-op: local dedup is handled by the
    // AtomicBool in StaticFileLayer, not by this coordinator.
    let coord = LocalIsrCoordinator::new();
    assert!(
        coord.try_acquire("/about", "window-1").await,
        "first call should succeed"
    );
    // A second concurrent call also returns true — no HashMap tracking.
    assert!(
        coord.try_acquire("/about", "window-1").await,
        "local coordinator always grants (no-op dedup)"
    );
}

#[tokio::test]
async fn local_coordinator_release_is_noop_and_does_not_panic() {
    let coord = LocalIsrCoordinator::new();
    // release without a prior acquire must not panic.
    coord.release("/about", "window-1").await;
    // Subsequent acquire still works.
    assert!(coord.try_acquire("/about", "window-1").await);
}

#[tokio::test]
async fn local_coordinator_different_routes_are_independent() {
    let coord = LocalIsrCoordinator::new();
    let home = coord.try_acquire("/", "window-1").await;
    let about = coord.try_acquire("/about", "window-1").await;
    assert!(home, "/ should be acquirable");
    assert!(about, "/about should be acquirable independently of /");
}

#[tokio::test]
async fn local_coordinator_different_windows_are_independent() {
    let coord = LocalIsrCoordinator::new();
    let w1 = coord.try_acquire("/about", "window-1").await;
    let w2 = coord.try_acquire("/about", "window-2").await;
    assert!(w1, "window-1 should be acquirable");
    assert!(
        w2,
        "window-2 should be acquirable independently of window-1"
    );
}

#[tokio::test]
async fn local_coordinator_backend_name() {
    let coord = LocalIsrCoordinator::new();
    assert_eq!(coord.backend(), "local");
}

// ---------------------------------------------------------------------------
// StaticFileLayer: accepts a custom coordinator via builder
// ---------------------------------------------------------------------------

#[test]
fn static_file_layer_with_isr_coordinator_accepts_local() {
    use autumn_web::static_gen::{ManifestEntry, StaticFileLayer, StaticManifest};
    use std::collections::HashMap;

    // Build a minimal dist dir
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<h1>Home</h1>").unwrap();

    let mut routes = HashMap::new();
    routes.insert(
        "/".to_owned(),
        ManifestEntry {
            file: "index.html".to_owned(),
            revalidate: None,
        },
    );
    let manifest = StaticManifest {
        generated_at: "2026-05-06T00:00:00Z".to_owned(),
        autumn_version: "0.3.0".to_owned(),
        routes,
    };
    let json = serde_json::to_string(&manifest).unwrap();
    std::fs::write(dist.join("manifest.json"), json).unwrap();

    // Should compile and not panic
    let _layer = StaticFileLayer::new(&dist)
        .expect("layer")
        .with_isr_coordinator(Arc::new(LocalIsrCoordinator::new()));
}
