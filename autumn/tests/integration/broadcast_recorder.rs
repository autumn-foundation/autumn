#![cfg(feature = "ws")]

//! Integration tests for the opt-in channel broadcast recorder and the
//! `assert_broadcast*` assertion helpers on `TestClient` (issue #1043).
//!
//! Mirrors the mail recorder tests (issue #1034) but for the channels
//! interceptor seam. The recorder is opt-in via [`TestApp::record_broadcasts`].

use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// A handler that publishes twice on the same topic.
#[post("/pub-two")]
async fn pub_two(State(state): State<AppState>) -> &'static str {
    state.broadcast().publish("room", "first").unwrap();
    state.broadcast().publish("room", "second").unwrap();
    "ok"
}

// A handler that publishes on two different topics.
#[post("/pub-ab")]
async fn pub_ab(State(state): State<AppState>) -> &'static str {
    state.broadcast().publish("a", "1").unwrap();
    state.broadcast().publish("b", "2").unwrap();
    "ok"
}

// ── AC2: captured publications, in order ──────────────────────────────────

#[tokio::test]
async fn records_broadcasts_in_order() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    let events = client.broadcasts();
    assert_eq!(events.len(), 2, "both publications recorded");
    assert_eq!(events[0].topic(), "room");
    assert_eq!(events[0].payload(), "first");
    assert_eq!(events[1].topic(), "room");
    assert_eq!(events[1].payload(), "second");
}

// ── AC2: filter by topic ──────────────────────────────────────────────────

#[tokio::test]
async fn filters_by_topic() {
    let client = TestApp::new()
        .routes(routes![pub_ab])
        .record_broadcasts()
        .build();

    client.post("/pub-ab").send().await.assert_ok();

    let on_a = client.broadcasts_on("a");
    assert_eq!(on_a.len(), 1, "only the 'a' publication");
    assert_eq!(on_a[0].payload(), "1");

    let on_b = client.broadcasts_on("b");
    assert_eq!(on_b.len(), 1, "only the 'b' publication");
    assert_eq!(on_b[0].payload(), "2");
}

// ── AC2: publish_html payload captured (HTML/OOB) ─────────────────────────

#[cfg(all(feature = "maud", feature = "htmx"))]
#[post("/pub-html")]
async fn pub_html(State(state): State<AppState>) -> &'static str {
    state
        .broadcast()
        .publish_html("feed", &html! { div id="notice" { "Saved" } })
        .unwrap();
    "ok"
}

#[cfg(all(feature = "maud", feature = "htmx"))]
#[tokio::test]
async fn captures_publish_html_payload() {
    let client = TestApp::new()
        .routes(routes![pub_html])
        .record_broadcasts()
        .build();

    client.post("/pub-html").send().await.assert_ok();

    let events = client.broadcasts_on("feed");
    assert_eq!(events.len(), 1, "html publish recorded");
    let payload = events[0].payload();
    assert!(
        payload.contains("hx-swap-oob"),
        "recorded payload should contain the OOB envelope, got: {payload}"
    );
    assert!(
        payload.contains("id=\"notice\""),
        "recorded payload should contain the rendered fragment, got: {payload}"
    );
}

// ── AC3: assert_broadcast (pass + fail) ───────────────────────────────────

#[tokio::test]
async fn assert_broadcast_passes_with_matching_predicate() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    client
        .assert_broadcast("room", |b| b.payload() == "first")
        .assert_broadcast("room", |b| b.payload() == "second");
}

#[tokio::test]
#[should_panic(expected = "room")]
async fn assert_broadcast_fails_and_lists_published() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    // No publication on "room" matches "nope" — panics, self-diagnosing.
    client.assert_broadcast("room", |b| b.payload() == "nope");
}

// ── AC3: assert_broadcast_count (pass + fail) ─────────────────────────────

#[tokio::test]
async fn assert_broadcast_count_passes() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    client.assert_broadcast_count("room", 2);
}

#[tokio::test]
#[should_panic(expected = "room")]
async fn assert_broadcast_count_fails() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    client.assert_broadcast_count("room", 5);
}

// ── AC3: assert_no_broadcasts (pass + fail) ───────────────────────────────

#[tokio::test]
async fn assert_no_broadcasts_passes_for_quiet_topic() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    client.assert_no_broadcasts("other-topic");
}

#[tokio::test]
#[should_panic(expected = "room")]
async fn assert_no_broadcasts_fails_when_present() {
    let client = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();

    client.post("/pub-two").send().await.assert_ok();

    client.assert_no_broadcasts("room");
}

// ── AC6: opt-in — helpers panic without record_broadcasts() ───────────────

#[tokio::test]
#[should_panic(expected = "record_broadcasts")]
async fn broadcasts_reader_panics_when_not_opted_in() {
    let client = TestApp::new().routes(routes![pub_two]).build();

    client.post("/pub-two").send().await.assert_ok();

    // No .record_broadcasts() — reading recorded broadcasts must panic and
    // point the user at the builder.
    let _ = client.broadcasts();
}

// ── AC6: production behavior intact — real subscriber still receives ──────

#[tokio::test]
async fn subscriber_receives_without_recorder() {
    let client = TestApp::new().routes(routes![pub_two]).build();

    // Subscribe before the publish happens inside the handler.
    let mut sub = client.state().channels().subscribe("room");

    client.post("/pub-two").send().await.assert_ok();

    let first = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
        .await
        .expect("timeout waiting for first broadcast")
        .expect("recv error");
    assert_eq!(first.as_str(), "first");
}

// ── AC4: no cross-test / cross-app leak ───────────────────────────────────

#[tokio::test]
async fn no_cross_test_leak() {
    let client_a = TestApp::new()
        .routes(routes![pub_two])
        .record_broadcasts()
        .build();
    let client_b = TestApp::new()
        .routes(routes![pub_ab])
        .record_broadcasts()
        .build();

    client_a.post("/pub-two").send().await.assert_ok();
    client_b.post("/pub-ab").send().await.assert_ok();

    // Each recorder only sees its own app's publications.
    assert_eq!(client_a.broadcasts().len(), 2);
    assert_eq!(client_a.broadcasts_on("a").len(), 0);

    assert_eq!(client_b.broadcasts().len(), 2);
    assert_eq!(client_b.broadcasts_on("room").len(), 0);
}
