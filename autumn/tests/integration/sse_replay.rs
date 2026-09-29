//! Integration tests for resumable SSE streams (issue #1356).
//!
//! Exercises the `Last-Event-ID` replay buffer across the channel backend and
//! the `sse::stream_resumable` serialization seam: cold connections, replay of
//! events missed during a disconnect, gap sentinels on buffer overflow, and the
//! gapless replay/live boundary.

#![cfg(feature = "ws")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::time::Duration;

use autumn_web::channels::{ChannelMessage, Channels, EventId};
use axum::response::IntoResponse as _;
use tower::ServiceExt as _;

/// Split an on-the-wire SSE id (`"{epoch}.{seq}"`) into its `(epoch, seq)` parts.
fn parse_event_id(s: &str) -> (u64, u64) {
    let (epoch, seq) = s
        .split_once('.')
        .unwrap_or_else(|| panic!("event id must be epoch.seq, got {s:?}"));
    (
        epoch.parse().expect("epoch is u64"),
        seq.parse().expect("seq is u64"),
    )
}

/// Read a topic's current epoch (subscribing then dropping the handle).
fn topic_epoch(channels: &Channels, topic: &str) -> u64 {
    channels.resume(topic, None).epoch
}

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
    // Normalize CRLF so block/line splitting works regardless of line endings.
    let text = String::from_utf8_lossy(raw).replace("\r\n", "\n");
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
    let epoch = first.epoch;
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");
    assert_eq!(first.subscriber.recv().await.unwrap().as_str(), "e1");
    assert_eq!(first.subscriber.recv().await.unwrap().as_str(), "e2");
    let last_seen = EventId { epoch, seq: 2 }; // seqs are 1-based and dense

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

    // Seed three events (seqs 1,2,3) before the client resumes.
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");
    publish(&channels, topic, "e3");

    // Resume from seq 1 — this subscribes live under the replay lock.
    let epoch = topic_epoch(&channels, topic);
    let sse = autumn_web::sse::stream_resumable(&state, topic, Some(EventId { epoch, seq: 1 }));

    // Publish more after the resume subscription: these are live events.
    publish(&channels, topic, "e4");
    publish(&channels, topic, "e5");

    let response = sse.into_response();
    let frames = collect_sse(response.into_body(), Duration::from_millis(300)).await;

    let seen: Vec<((u64, u64), String)> = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| {
            (
                parse_event_id(f.id.as_deref().expect("replay/live frame has id")),
                f.data.clone(),
            )
        })
        .collect();

    // Every frame within the connection shares the topic epoch, and the seq
    // parts are the expected 2,3 (replay) then 4,5 (live) — no dup/skip.
    assert!(
        seen.iter().all(|((e, _), _)| *e == epoch),
        "all frames share the topic epoch {epoch}: {seen:?}"
    );
    let seqs_and_data: Vec<(u64, String)> =
        seen.iter().map(|((_, seq), d)| (*seq, d.clone())).collect();
    assert_eq!(
        seqs_and_data,
        vec![
            (2, "e2".to_owned()),
            (3, "e3".to_owned()),
            (4, "e4".to_owned()),
            (5, "e5".to_owned()),
        ],
        "replayed (2,3) then live (4,5) with monotonic seqs, no dup/skip at the seam"
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
    let epoch = topic_epoch(&channels, topic);

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

    // The browser echoes back the full epoch-tagged id it last saw.
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/events")
                .header("last-event-id", EventId { epoch, seq: 1 }.to_string())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let frames = collect_sse(response.into_body(), Duration::from_millis(300)).await;
    let seen: Vec<((u64, u64), String)> = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| {
            (
                parse_event_id(f.id.as_deref().expect("replay frame has id")),
                f.data.clone(),
            )
        })
        .collect();
    assert!(
        seen.iter().all(|((e, _), _)| *e == epoch),
        "all replay frames share the topic epoch: {seen:?}"
    );
    let seqs_and_data: Vec<(u64, String)> =
        seen.iter().map(|((_, seq), d)| (*seq, d.clone())).collect();
    assert_eq!(
        seqs_and_data,
        vec![(2, "b".to_owned()), (3, "c".to_owned())],
        "Last-Event-ID epoch.1 replays events 2 and 3"
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
    let seen_first: Vec<((u64, u64), String)> = first_frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| {
            (
                parse_event_id(f.id.as_deref().expect("live frame has id")),
                f.data.clone(),
            )
        })
        .collect();
    let seqs_first: Vec<(u64, String)> = seen_first
        .iter()
        .map(|((_, seq), d)| (*seq, d.clone()))
        .collect();
    assert_eq!(
        seqs_first,
        vec![(3, "e3".to_owned()), (4, "e4".to_owned())],
        "cold first connection sees only the live events e3, e4"
    );
    // The browser echoes back the full epoch-tagged id string (highest seq).
    let last_seen_id: String = first_frames
        .iter()
        .filter_map(|f| f.id.clone())
        .max_by_key(|id| parse_event_id(id).1)
        .expect("first connection must have seen at least one id");
    assert_eq!(parse_event_id(&last_seen_id).1, 4);

    // Events published *while disconnected* — the reconnect must recover these.
    publish(&channels, topic, "e5");
    publish(&channels, topic, "e6");

    // Second request resumes from the last id seen.
    let second = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/events")
                .header("last-event-id", last_seen_id)
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
    let recovered: Vec<(u64, String)> = frames
        .iter()
        .map(|f| {
            (
                parse_event_id(f.id.as_deref().expect("recovered frame has id")).1,
                f.data.clone(),
            )
        })
        .collect();
    assert_eq!(
        recovered,
        vec![(5, "e5".to_owned()), (6, "e6".to_owned())],
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

    // Client last saw seq 1 → seq 2 aged out (retained seqs are 4,5,6).
    let epoch = topic_epoch(&channels, topic);
    let handle = channels.resume(topic, Some(EventId { epoch, seq: 1 }));
    assert!(handle.gap, "aged-out resume point must raise a gap");
    let ids: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec![4, 5, 6],
        "replay starts from the oldest retained seq"
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
    // With cap 256, retained seqs are (total-255)..=total.
    let oldest_retained = total - 255;
    let epoch = topic_epoch(&channels, topic);

    let sse = autumn_web::sse::stream_resumable(&state, topic, Some(EventId { epoch, seq: 1 }));
    let frames = collect_sse(sse.into_response().into_body(), Duration::from_millis(300)).await;

    assert_eq!(
        frames.first().and_then(|f| f.event.clone()).as_deref(),
        Some("gap"),
        "the gap sentinel must lead the frame sequence"
    );
    assert_eq!(frames[0].data, "{\"gap\":true}");
    assert!(frames[0].id.is_none(), "gap sentinel carries no id");

    // Replay frames follow, starting at the oldest retained seq, all in-epoch.
    assert_eq!(
        frames
            .get(1)
            .and_then(|f| f.id.as_deref())
            .map(parse_event_id),
        Some((epoch, oldest_retained)),
        "replay must start at the oldest retained seq (in-epoch) after a gap"
    );
    let replay_count = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .count();
    assert_eq!(replay_count, 256, "replay retains exactly the capacity");
}

// ── Same-epoch future seq: defensive gap without dropping the replay ───────────

#[tokio::test]
async fn resume_with_future_same_epoch_seq_signals_gap() {
    // Defensive: within the current epoch a client should never present a seq
    // ahead of the server, but if it does we flag a gap rather than silently
    // returning an empty replay.
    let channels = Channels::new(64);
    let topic = "epoch";
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");
    // start_seq is now 2.
    let epoch = topic_epoch(&channels, topic);

    let handle = channels.resume(topic, Some(EventId { epoch, seq: 500 }));
    assert!(
        handle.gap,
        "a same-epoch seq beyond the current server seq must raise a gap"
    );
    assert!(
        handle.replay.is_empty(),
        "no retained seq exceeds the future resume point, so nothing replays"
    );
    assert_eq!(handle.next_live_id, 3);
}

#[tokio::test]
async fn stream_resumable_emits_gap_event_frame_on_future_same_epoch_seq() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "sse-epoch";
    publish(&channels, topic, "e1");
    publish(&channels, topic, "e2");
    let epoch = topic_epoch(&channels, topic);

    // Resume with a future same-epoch seq: the stream must lead with a gap
    // sentinel and replay nothing.
    let sse = autumn_web::sse::stream_resumable(&state, topic, Some(EventId { epoch, seq: 500 }));
    let frames = collect_sse(sse.into_response().into_body(), Duration::from_millis(300)).await;

    assert_eq!(
        frames.first().and_then(|f| f.event.clone()).as_deref(),
        Some("gap"),
        "the gap sentinel must lead the frame sequence"
    );
    assert_eq!(frames[0].data, "{\"gap\":true}");
    let replay_count = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .count();
    assert_eq!(
        replay_count, 0,
        "nothing to replay for a future same-epoch seq"
    );
}

// ── Codex regression: topic GC/epoch reset must NOT silently drop early events ─

/// The headline fix. A topic is garbage-collected and recreated, resetting its
/// per-topic seq counter to 1 under a NEW epoch. A client reconnects with a
/// stale `Last-Event-ID` from the OLD epoch whose seq (4) the new epoch has
/// already passed. Before epoch tagging, the old-epoch seq 4 was
/// indistinguishable from the new-epoch seq 4, so the resume replayed only
/// new-epoch seqs 5..10 and SILENTLY DROPPED new-epoch seqs 1..4. With epoch
/// tagging the mismatch is detected: the stream gaps and replays the whole
/// retained current epoch (1..10).
#[tokio::test]
async fn resume_after_topic_gc_epoch_reset_replays_full_epoch_not_partial() {
    // Small replay cap (32) still retains all 10 new-epoch events.
    let channels = Channels::with_shared_backend(std::sync::Arc::new(
        autumn_web::channels::LocalChannelsBackend::with_replay_capacity(64, 32),
    ));
    let topic = "gc-epoch";

    // Epoch E1: subscribe + publish so the topic exists with seqs E1.1..E1.4.
    let handle1 = channels.resume(topic, None);
    let e1 = handle1.epoch;
    publish(&channels, topic, "old-1");
    publish(&channels, topic, "old-2");
    publish(&channels, topic, "old-3");
    publish(&channels, topic, "old-4"); // the client "last saw" E1 seq 4
    drop(handle1); // drop the only subscriber (and its receiver)

    // Force the topic to be collected: no live receivers and no retained
    // `Sender` keep it, so gc removes it (and its replay buffer + epoch).
    channels.gc();
    assert_eq!(
        channels.channel_count(),
        0,
        "topic must be collected so it is recreated with a fresh epoch"
    );

    // Epoch E2: the next publish recreates the topic; the seq counter restarts
    // at 1 under a brand-new epoch.
    for i in 1..=10 {
        publish(&channels, topic, &format!("new-{i}"));
    }

    // Stale reconnect from E1 seq 4 (a seq the new epoch E2 has already passed).
    let handle = channels.resume(topic, Some(EventId { epoch: e1, seq: 4 }));
    let e2 = handle.epoch;
    println!("epoch-reset regression observed E1={e1} E2={e2}");
    assert_ne!(
        e1, e2,
        "the recreated topic must have a distinct epoch (E1={e1}, E2={e2})"
    );
    assert!(handle.gap, "a cross-epoch resume must signal a gap");
    let seqs: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
    assert_eq!(
        seqs,
        (1..=10).collect::<Vec<_>>(),
        "cross-epoch resume must replay ALL retained current-epoch events (1..10), \
         NOT just seq > 4 — new-epoch seqs 1..4 must not be silently dropped"
    );
}

/// The SSE-wire variant of the regression: after a topic GC/epoch reset, the
/// stream must lead with a `gap` frame and then replay the full retained current
/// epoch, all frames carrying the new epoch.
#[tokio::test]
async fn stream_resumable_after_topic_gc_leads_with_gap_then_full_epoch_replay() {
    let state = autumn_web::AppState::for_test();
    let channels = state.channels().clone();
    let topic = "sse-gc-epoch";

    // Epoch E1.
    let handle1 = channels.resume(topic, None);
    let e1 = handle1.epoch;
    for i in 1..=4 {
        publish(&channels, topic, &format!("old-{i}"));
    }
    drop(handle1);

    channels.gc();
    assert_eq!(
        channels.channel_count(),
        0,
        "topic must be collected so it is recreated with a fresh epoch"
    );

    // Epoch E2: seqs 1..8 (default replay cap 256 retains them all).
    for i in 1..=8 {
        publish(&channels, topic, &format!("new-{i}"));
    }

    // Resume with the stale E1 id (seq 4, already passed by E2).
    let sse = autumn_web::sse::stream_resumable(&state, topic, Some(EventId { epoch: e1, seq: 4 }));
    let frames = collect_sse(sse.into_response().into_body(), Duration::from_millis(300)).await;

    assert_eq!(
        frames.first().and_then(|f| f.event.clone()).as_deref(),
        Some("gap"),
        "a cross-epoch resume must lead with a gap sentinel: {frames:?}"
    );
    assert!(frames[0].id.is_none(), "gap sentinel carries no id");

    // The replay covers the full retained current epoch (seqs 1..8), and every
    // replayed frame carries the same NEW epoch E2 (≠ E1).
    let replay: Vec<(u64, u64, String)> = frames
        .iter()
        .filter(|f| f.event.as_deref() != Some("gap"))
        .map(|f| {
            let (e, seq) = parse_event_id(f.id.as_deref().expect("replay frame has id"));
            (e, seq, f.data.clone())
        })
        .collect();
    let e2 = replay
        .first()
        .map(|(e, _, _)| *e)
        .expect("replay is non-empty");
    assert_ne!(
        e1, e2,
        "replayed frames must carry the new epoch (E1={e1}, E2={e2})"
    );
    assert!(
        replay.iter().all(|(e, _, _)| *e == e2),
        "all replayed frames share the new epoch: {replay:?}"
    );
    let seqs: Vec<u64> = replay.iter().map(|(_, seq, _)| *seq).collect();
    assert_eq!(
        seqs,
        (1..=8).collect::<Vec<_>>(),
        "cross-epoch resume replays the full retained epoch (1..8), not just seq > 4: {replay:?}"
    );
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
    assert_eq!(frames.len(), 1, "cold stream replays nothing: {frames:?}");
    let (_, seq) = parse_event_id(frames[0].id.as_deref().expect("live frame has id"));
    assert_eq!(
        (seq, frames[0].data.clone()),
        (2, "new".to_owned()),
        "cold stream replays nothing; live event keeps its dense seq"
    );
}

// ── AC#3: seam stays gapless under a concurrent publish ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_publish_around_resume_has_no_dup_or_skip() {
    use std::sync::{Arc, Barrier};

    // Which side of the seam the boundary message (m11) is forced onto for a
    // trial. The two deterministic orderings prove BOTH branches are exercised
    // regardless of runner scheduling; `Race` additionally stresses the real
    // lock contention that a barrier releases both threads into at once.
    enum SeamOrdering {
        /// Publish m11 *before* resuming: it is already in the replay buffer, so
        /// the snapshot must include it (replay branch).
        PublishThenResume,
        /// Resume *before* m11 exists: the snapshot is empty and m11 must arrive
        /// on the live subscriber (live-tail branch).
        ResumeThenPublish,
        /// Genuine race: a barrier releases publisher and resumer together so
        /// their contention for the replay lock is real; whichever wins, the
        /// exactly-once correctness assertions below must still hold.
        Race,
    }

    // Run one seam trial under `ordering`. Publishes m11 around a
    // resume-from-seq-10 and asserts m11 is delivered exactly once — either in
    // the replay snapshot OR as the first live event, never both, never neither,
    // with a consistent `next_live_id`. Returns `true` when m11 landed in the
    // replay snapshot (replay branch), `false` when it arrived live (live-tail
    // branch), so the caller can prove both branches are covered.
    async fn run_seam_trial(ordering: &SeamOrdering) -> bool {
        let channels = Channels::new(256);
        let topic = "seam";
        for i in 1..=10 {
            publish(&channels, topic, &format!("m{i}"));
        }
        let epoch = topic_epoch(&channels, topic);

        let mut handle = match ordering {
            SeamOrdering::PublishThenResume => {
                publish(&channels, topic, "m11");
                channels.resume(topic, Some(EventId { epoch, seq: 10 }))
            }
            SeamOrdering::ResumeThenPublish => {
                let handle = channels.resume(topic, Some(EventId { epoch, seq: 10 }));
                publish(&channels, topic, "m11");
                handle
            }
            SeamOrdering::Race => {
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
                let handle = channels.resume(topic, Some(EventId { epoch, seq: 10 }));
                bg.await.unwrap();
                handle
            }
        };

        // m11 is delivered exactly once: either in the replay snapshot OR as the
        // first live event — never both, never neither.
        let in_replay: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
        assert!(
            in_replay.iter().all(|id| *id == 11),
            "resume from id 10 can only replay id 11: {in_replay:?}"
        );

        if in_replay.contains(&11) {
            // Delivered via replay; the live subscriber must not repeat it.
            assert_eq!(handle.next_live_id, 12);
            publish(&channels, topic, "m12");
            assert_eq!(handle.subscriber.recv().await.unwrap().as_str(), "m12");
            true
        } else {
            // Not in replay → must arrive live as the very next event, id 11.
            assert_eq!(handle.next_live_id, 11);
            let live = tokio::time::timeout(Duration::from_secs(1), handle.subscriber.recv())
                .await
                .expect("m11 must arrive live")
                .unwrap();
            assert_eq!(live.as_str(), "m11");
            false
        }
    }

    // Deterministically exercise BOTH sides of the seam — no reliance on runner
    // scheduling to hit each branch (the old flaky guard).
    let saw_replay = run_seam_trial(&SeamOrdering::PublishThenResume).await;
    let saw_live = !run_seam_trial(&SeamOrdering::ResumeThenPublish).await;
    assert!(
        saw_replay,
        "publish-then-resume must land m11 in the replay snapshot"
    );
    assert!(
        saw_live,
        "resume-then-publish must deliver m11 on the live tail"
    );

    // Additional genuine-race trials stress the contended replay lock; the
    // exactly-once assertions inside each trial hold whichever side wins.
    for _ in 0..256 {
        run_seam_trial(&SeamOrdering::Race).await;
    }
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
    // their seqs are strictly increasing and dense (the newest `cap`).
    let epoch = topic_epoch(&channels, topic);
    let handle = channels.resume(topic, Some(EventId { epoch, seq: 0 }));
    assert!(
        handle.replay.len() <= cap,
        "replay retained {} > cap {cap}",
        handle.replay.len()
    );
    assert!(
        handle.gap,
        "resuming from seq 0 after overflow signals a gap"
    );
    let ids: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![193, 194, 195, 196, 197, 198, 199, 200]);
    for pair in ids.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "seqs must be dense and monotonic");
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
