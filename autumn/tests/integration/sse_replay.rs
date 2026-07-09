//! Integration tests for resumable SSE streams (issue #1356).
//!
//! Exercises the `Last-Event-ID` replay buffer across the channel backend and
//! the `sse::stream_resumable` serialization seam: cold connections, replay of
//! events missed during a disconnect, gap sentinels on buffer overflow, and the
//! gapless replay/live boundary.

#![cfg(feature = "ws")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::time::Duration;

use autumn_web::channels::{ChannelMessage, Channels};
use axum::response::IntoResponse as _;
use tower::ServiceExt as _;

// ── SSE body parsing helpers ──────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SseFrame {
    id: Option<String>,
    event: Option<String>,
    data: String,
}

/// Parse a raw `text/event-stream` byte buffer into its events, ignoring
/// keep-alive comment lines (`: ...`).
fn parse_sse(raw: &[u8]) -> Vec<SseFrame> {
    let text = String::from_utf8_lossy(raw);
    let mut frames = Vec::new();
    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut frame = SseFrame::default();
        let mut data_lines = Vec::new();
        let mut saw_field = false;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("id:") {
                frame.id = Some(rest.trim().to_owned());
                saw_field = true;
            } else if let Some(rest) = line.strip_prefix("event:") {
                frame.event = Some(rest.trim().to_owned());
                saw_field = true;
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
                saw_field = true;
            }
            // Lines beginning with ':' are keep-alive comments — skip them.
        }
        if saw_field {
            frame.data = data_lines.join("\n");
            frames.push(frame);
        }
    }
    frames
}

/// Drain SSE frames from a response body until the stream goes idle for `idle`.
///
/// The live tail of a resumable stream never terminates on its own, so we stop
/// reading once no frame arrives within the idle window.
async fn collect_sse(body: axum::body::Body, idle: Duration) -> Vec<SseFrame> {
    use http_body_util::BodyExt as _;

    let mut body = body;
    let mut raw = Vec::new();
    // `while let` exits the moment a frame fails to arrive within the idle
    // window (or the stream ends/errors): the live tail never terminates on its
    // own, so the idle timeout is our stop condition.
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(idle, body.frame()).await {
        if let Some(data) = frame.data_ref() {
            raw.extend_from_slice(data);
        }
    }
    parse_sse(&raw)
}

fn publish(channels: &Channels, topic: &str, payload: &str) {
    channels
        .publish(topic, ChannelMessage::from(payload))
        .expect("publish should not fail");
}

// ── AC#6: kill-and-resume delivers every missed event ─────────────────────────

#[tokio::test]
async fn kill_and_resume_delivers_full_sequence_zero_missed() {
    let channels = Channels::new(64);
    let topic = "orders";

    // First connection: read the two live events it sees.
    let mut first = channels.resume(topic, None);
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");
    assert_eq!(first.subscriber.recv().await.unwrap().as_str(), "e1");
    assert_eq!(first.subscriber.recv().await.unwrap().as_str(), "e2");
    let last_seen = 2; // ids are 1-based and dense

    // Client disconnects.
    drop(first);

    // Events published *during the gap* — the reconnect must recover these.
    publish(&channels, topic, "e3");
    publish(&channels, topic, "e4");

    // Reconnect with the last id the client saw.
    let mut resumed = channels.resume(topic, Some(last_seen));
    assert!(!resumed.gap, "nothing aged out of a 256-deep buffer");
    let replayed: Vec<(u64, String)> = resumed
        .replay
        .iter()
        .map(|s| (s.id, s.message.as_str().to_owned()))
        .collect();
    assert_eq!(
        replayed,
        vec![(3, "e3".to_owned()), (4, "e4".to_owned())],
        "replay must cover exactly the events missed during the gap"
    );
    assert_eq!(resumed.next_live_id, 5);

    // Live continues seamlessly after the replay.
    publish(&channels, topic, "e5");
    assert_eq!(resumed.subscriber.recv().await.unwrap().as_str(), "e5");

    // Full reconstructed sequence: replay ++ live, each exactly once, in order.
    let mut full: Vec<String> = replayed.into_iter().map(|(_, m)| m).collect();
    full.push("e5".to_owned());
    assert_eq!(full, vec!["e3", "e4", "e5"]);
}

// ── AC#6 through real SSE serialization ───────────────────────────────────────

#[tokio::test]
async fn stream_resumable_serializes_replay_then_live_frames() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "sse-orders";

    // Seed three events (ids 1,2,3) before the client resumes.
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");
    publish(&channels, topic, "e3");

    // Resume from id 1 — this subscribes live under the replay lock.
    let sse = autumn_web::sse::stream_resumable(&state, topic, Some(1));

    // Publish more after the resume subscription: these are live events.
    publish(&channels, topic, "e4");
    publish(&channels, topic, "e5");

    let response = sse.into_response();
    let frames = collect_sse(response.into_body(), Duration::from_millis(300)).await;

    let seen: Vec<(Option<String>, String)> = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| (f.id.clone(), f.data.clone()))
        .collect();

    assert_eq!(
        seen,
        vec![
            (Some("2".to_owned()), "e2".to_owned()),
            (Some("3".to_owned()), "e3".to_owned()),
            (Some("4".to_owned()), "e4".to_owned()),
            (Some("5".to_owned()), "e5".to_owned()),
        ],
        "replayed (2,3) then live (4,5) with monotonic ids, no dup/skip at the seam"
    );
}

// ── AC#6 via a real axum router + Last-Event-ID header ─────────────────────────

#[tokio::test]
async fn router_oneshot_resumes_from_last_event_id_header() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "router-feed";

    publish(&channels, topic, "a");
    publish(&channels, topic, "b");
    publish(&channels, topic, "c");

    let handler_state = state.clone();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let state = handler_state.clone();
            async move {
                let last = autumn_web::sse::last_event_id(&headers);
                autumn_web::sse::stream_resumable(&state, "router-feed", last)
            }
        }),
    );

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/events")
                .header("last-event-id", "1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let frames = collect_sse(response.into_body(), Duration::from_millis(300)).await;
    let seen: Vec<(Option<String>, String)> = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| (f.id.clone(), f.data.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (Some("2".to_owned()), "b".to_owned()),
            (Some("3".to_owned()), "c".to_owned()),
        ],
        "Last-Event-ID: 1 replays events 2 and 3"
    );
}

// ── AC#6 on the wire: disconnect, publish during the gap, reconnect ───────────

/// The literal AC#6 scenario end-to-end through the real SSE serialization and
/// an axum router: connect, read `id:`/`data:` frames, DROP the response stream
/// (disconnect), publish more events during the gap, then issue a SECOND request
/// with `Last-Event-ID` set to the last id seen — every gap event must be
/// delivered exactly once, in order, on the wire.
#[tokio::test]
async fn wire_disconnect_then_resume_delivers_gap_events_exactly_once() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "wire-orders";

    let handler_state = state.clone();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let state = handler_state.clone();
            async move {
                let last = autumn_web::sse::last_event_id(&headers);
                autumn_web::sse::stream_resumable(&state, "wire-orders", last)
            }
        }),
    );

    // Seed two events before the first connection ever exists.
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");

    // First connection: no Last-Event-ID (cold). The handler subscribes live as
    // it runs, so events published *after* the response exists are delivered.
    let first = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/events")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), axum::http::StatusCode::OK);

    // These arrive live on the first connection (ids 3, 4).
    publish(&channels, topic, "e3");
    publish(&channels, topic, "e4");

    // Read what the first connection saw, then DROP the stream (disconnect):
    // `collect_sse` consumes the body by value and drops it here.
    let first_frames = collect_sse(first.into_body(), Duration::from_millis(300)).await;
    let seen_first: Vec<(Option<String>, String)> = first_frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| (f.id.clone(), f.data.clone()))
        .collect();
    assert_eq!(
        seen_first,
        vec![
            (Some("3".to_owned()), "e3".to_owned()),
            (Some("4".to_owned()), "e4".to_owned()),
        ],
        "cold first connection sees only the live events e3, e4"
    );
    let last_seen: u64 = first_frames
        .iter()
        .filter_map(|f| f.id.as_deref())
        .filter_map(|id| id.parse().ok())
        .max()
        .expect("first connection must have seen at least one id");
    assert_eq!(last_seen, 4);

    // Events published *while disconnected* — the reconnect must recover these.
    publish(&channels, topic, "e5");
    publish(&channels, topic, "e6");

    // Second request resumes from the last id seen.
    let second = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/events")
                .header("last-event-id", last_seen.to_string())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), axum::http::StatusCode::OK);

    let frames = collect_sse(second.into_body(), Duration::from_millis(300)).await;
    assert!(
        frames.iter().all(|f| f.event.as_deref() != Some("gap")),
        "buffer is deep enough to replay the gap without a sentinel: {frames:?}"
    );
    let recovered: Vec<(Option<String>, String)> = frames
        .iter()
        .map(|f| (f.id.clone(), f.data.clone()))
        .collect();
    assert_eq!(
        recovered,
        vec![
            (Some("5".to_owned()), "e5".to_owned()),
            (Some("6".to_owned()), "e6".to_owned()),
        ],
        "reconnect delivers every event published during the gap exactly once, in order, zero missed"
    );
}

// ── AC#4: gap sentinel when the resume point aged out ─────────────────────────

#[tokio::test]
async fn overflowed_buffer_emits_gap_sentinel_and_replays_from_oldest() {
    // Replay capacity 3; overflow it so early ids age out.
    let channels = Channels::with_shared_backend(std::sync::Arc::new(
        autumn_web::channels::LocalChannelsBackend::with_replay_capacity(64, 3),
    ));
    let topic = "audit";
    for i in 1..=6 {
        publish(&channels, topic, &format!("m{i}"));
    }

    // Client last saw id 1 → id 2 aged out (retained ids are 4,5,6).
    let handle = channels.resume(topic, Some(1));
    assert!(handle.gap, "aged-out resume point must raise a gap");
    let ids: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec![4, 5, 6],
        "replay starts from the oldest retained id"
    );
}

#[tokio::test]
async fn stream_resumable_emits_gap_event_frame_on_overflow() {
    // `AppState::for_test` uses the default replay capacity (256); overflow it
    // by publishing more than that so early ids age out.
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "sse-audit";
    let total = 300u64;
    for i in 1..=total {
        publish(&channels, topic, &format!("m{i}"));
    }
    // With cap 256, retained ids are (total-255)..=total.
    let oldest_retained = total - 255;

    let sse = autumn_web::sse::stream_resumable(&state, topic, Some(1));
    let frames = collect_sse(sse.into_response().into_body(), Duration::from_millis(300)).await;

    assert_eq!(
        frames.first().and_then(|f| f.event.clone()).as_deref(),
        Some("gap"),
        "the gap sentinel must lead the frame sequence"
    );
    assert_eq!(frames[0].data, "{\"gap\":true}");
    assert!(frames[0].id.is_none(), "gap sentinel carries no id");

    // Replay frames follow, starting at the oldest retained id.
    assert_eq!(
        frames.get(1).and_then(|f| f.id.clone()),
        Some(oldest_retained.to_string()),
        "replay must start at the oldest retained id after a gap"
    );
    let replay_count = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .count();
    assert_eq!(replay_count, 256, "replay retains exactly the capacity");
}

// ── AC#5: cold connection behaves like today ──────────────────────────────────

#[tokio::test]
async fn cold_connection_has_no_replay_and_no_gap() {
    let channels = Channels::new(64);
    let topic = "cold";
    publish(&channels, topic, "past-1");
    publish(&channels, topic, "past-2");

    let mut handle = channels.resume(topic, None);
    assert!(
        handle.replay.is_empty(),
        "cold connect must not replay history"
    );
    assert!(!handle.gap);
    assert_eq!(handle.next_live_id, 3);

    // Only live events flow.
    publish(&channels, topic, "live-1");
    assert_eq!(handle.subscriber.recv().await.unwrap().as_str(), "live-1");
}

#[tokio::test]
async fn stream_resumable_cold_serializes_only_live_events() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "sse-cold";
    publish(&channels, topic, "old");

    let sse = autumn_web::sse::stream_resumable(&state, topic, None);
    publish(&channels, topic, "new");

    let frames = collect_sse(sse.into_response().into_body(), Duration::from_millis(300)).await;
    let seen: Vec<(Option<String>, String)> = frames
        .iter()
        .map(|f| (f.id.clone(), f.data.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![(Some("2".to_owned()), "new".to_owned())],
        "cold stream replays nothing; live event keeps its dense id"
    );
}

// ── AC#3: seam stays gapless under a concurrent publish ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_publish_around_resume_has_no_dup_or_skip() {
    use std::sync::{Arc, Barrier};

    // Track that the interleaving genuinely lands the boundary message in BOTH
    // the replay snapshot and the live tail across trials — otherwise one branch
    // is dead code and the test proves nothing.
    let mut saw_replay = false;
    let mut saw_live = false;

    // Run many trials; a barrier releases the publisher and the resumer at the
    // same instant so their contention for the replay lock is a real race.
    for _ in 0..256 {
        let channels = Channels::new(256);
        let topic = "seam";
        for i in 1..=10 {
            publish(&channels, topic, &format!("m{i}"));
        }

        // Publish m11 (the boundary message) concurrently with resume-from-10.
        // The barrier makes both threads reach the contended replay lock together
        // so which one wins varies from trial to trial.
        let barrier = Arc::new(Barrier::new(2));
        let bg = {
            let channels = channels.clone();
            let barrier = Arc::clone(&barrier);
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                publish(&channels, topic, "m11");
            })
        };
        barrier.wait();
        let mut handle = channels.resume(topic, Some(10));
        bg.await.unwrap();

        // m11 is delivered exactly once: either in the replay snapshot OR as the
        // first live event — never both, never neither.
        let in_replay: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
        assert!(
            in_replay.iter().all(|id| *id == 11),
            "resume from id 10 can only replay id 11: {in_replay:?}"
        );

        if in_replay.contains(&11) {
            saw_replay = true;
            // Delivered via replay; the live subscriber must not repeat it.
            assert_eq!(handle.next_live_id, 12);
            publish(&channels, topic, "m12");
            assert_eq!(handle.subscriber.recv().await.unwrap().as_str(), "m12");
        } else {
            saw_live = true;
            // Not in replay → must arrive live as the very next event, id 11.
            assert_eq!(handle.next_live_id, 11);
            let live = tokio::time::timeout(Duration::from_secs(1), handle.subscriber.recv())
                .await
                .expect("m11 must arrive live")
                .unwrap();
            assert_eq!(live.as_str(), "m11");
        }
    }

    // The whole point of the race: over the trials the boundary message must
    // have landed on BOTH sides of the seam, so neither branch is dead.
    assert!(
        saw_replay && saw_live,
        "interleaving must exercise both the replay and the live-tail branch \
         (saw_replay={saw_replay}, saw_live={saw_live})"
    );
}

// ── AC#1/AC#2: monotonic ids, bounded buffer ──────────────────────────────────

#[tokio::test]
async fn ids_are_monotonic_and_buffer_is_bounded() {
    let cap = 8;
    let channels = Channels::with_shared_backend(std::sync::Arc::new(
        autumn_web::channels::LocalChannelsBackend::with_replay_capacity(64, cap),
    ));
    let topic = "bounded";
    // Publish far more than the capacity.
    for i in 1..=200 {
        publish(&channels, topic, &format!("m{i}"));
    }

    // Resume from the very beginning: buffer retains at most `cap` events and
    // their ids are strictly increasing and dense (the newest `cap`).
    let handle = channels.resume(topic, Some(0));
    assert!(
        handle.replay.len() <= cap,
        "replay retained {} > cap {cap}",
        handle.replay.len()
    );
    assert!(handle.gap, "resuming from 0 after overflow signals a gap");
    let ids: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![193, 194, 195, 196, 197, 198, 199, 200]);
    for pair in ids.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "ids must be dense and monotonic");
    }
    assert_eq!(handle.next_live_id, 201);
}

// ── AC#7: existing non-resumable helpers unchanged ────────────────────────────

#[tokio::test]
async fn existing_stream_and_from_subscriber_remain_idless() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "legacy";

    // `sse::stream` (non-resumable) still yields id-less frames.
    let sse = autumn_web::sse::stream(&state, topic);
    publish(&channels, topic, "hello");

    let frames = collect_sse(sse.into_response().into_body(), Duration::from_millis(300)).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "hello");
    assert!(
        frames[0].id.is_none(),
        "non-resumable stream must remain id-less: {:?}",
        frames[0]
    );

    // `from_subscriber` still delivers plain messages.
    let sub = channels.subscribe(topic);
    let sse2 = autumn_web::sse::from_subscriber(sub);
    publish(&channels, topic, "world");
    let frames2 = collect_sse(sse2.into_response().into_body(), Duration::from_millis(300)).await;
    assert_eq!(frames2.len(), 1);
    assert_eq!(frames2[0].data, "world");
    assert!(frames2[0].id.is_none());
}
