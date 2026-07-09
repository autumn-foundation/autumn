//! Server-Sent Events (SSE) support for Autumn applications.
//!
//! This module provides ergonomic SSE handling, integrating with Autumn's
//! ecosystem to easily yield real-time updates to the client. SSE is a lightweight
//! alternative to `WebSockets` for one-way server-to-client event streams.
//!
//! # Examples
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::sse::{Sse, Event, keep_alive};
//! use futures::stream::Stream;
//! use std::convert::Infallible;
//!
//! #[get("/stream")]
//! async fn stream(state: AppState) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
//!     let mut rx = state.channels().subscribe("lobby");
//!
//!     let stream = async_stream::stream! {
//!         while let Ok(msg) = rx.recv().await {
//!             yield Ok(Event::default().data(msg.into_string()));
//!         }
//!     };
//!
//!     Sse::new(stream).keep_alive(keep_alive())
//! }
//! ```

pub use axum::response::sse::{Event, KeepAlive, Sse};
#[cfg(feature = "ws")]
use std::convert::Infallible;
#[cfg(feature = "ws")]
use std::future::Future;
use std::time::Duration;

/// Returns a default `KeepAlive` configuration for Server-Sent Events.
///
/// Sends a keep-alive message every 15 seconds to prevent proxies or load
/// balancers from dropping the connection during idle periods.
pub fn keep_alive() -> KeepAlive {
    KeepAlive::new().interval(Duration::from_secs(15))
}

/// Convert a channel subscriber into an SSE response stream.
#[cfg(feature = "ws")]
pub fn from_subscriber(
    subscriber: crate::channels::Subscriber,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>> + use<>> {
    use tokio_stream::StreamExt;

    let stream = subscriber
        .into_stream()
        .map(|msg| Ok(Event::default().data(msg.into_string())));
    Sse::new(stream).keep_alive(keep_alive())
}

/// SSE `event` type used for the replay-gap sentinel.
#[cfg(feature = "ws")]
const GAP_EVENT: &str = "gap";

/// JSON payload carried by the `gap` sentinel event.
#[cfg(feature = "ws")]
const GAP_MARKER: &str = "{\"gap\":true}";

/// Read and parse an inbound `Last-Event-ID` header.
///
/// Returns the parsed id, or `None` when the header is absent, empty, or not a
/// valid `u64`. This is the value clients echo back (per the SSE spec) after a
/// dropped connection so the server can resume where they left off.
#[must_use]
pub fn last_event_id(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Axum extractor for the inbound `Last-Event-ID` header.
///
/// Never fails: an absent or unparseable header yields `LastEventId(None)`.
///
/// ```rust
/// use autumn_web::sse::{last_event_id, LastEventId};
///
/// // A browser reconnecting after a drop echoes back the last id it saw in the
/// // `Last-Event-ID` header (the SSE spec does this automatically).
/// let mut headers = axum::http::HeaderMap::new();
/// headers.insert("last-event-id", "42".parse().unwrap());
///
/// // `last_event_id` parses the header; the `LastEventId` extractor wraps the
/// // same value so a handler can take it as an argument.
/// assert_eq!(last_event_id(&headers), Some(42));
/// let LastEventId(last) = LastEventId(last_event_id(&headers));
/// assert_eq!(last, Some(42));
/// ```
pub struct LastEventId(pub Option<u64>);

impl<S> axum::extract::FromRequestParts<S> for LastEventId
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(last_event_id(&parts.headers)))
    }
}

/// Subscribe to a channel topic and return a **resumable** SSE response stream.
///
/// Unlike [`stream`], every event carries a monotonic per-topic `id`, and an
/// inbound `Last-Event-ID` (see [`last_event_id`] / [`LastEventId`]) replays the
/// events a client missed during a brief disconnect before continuing live —
/// with no duplicated or skipped events at the seam.
///
/// - A cold connection (`last_event_id == None`) behaves exactly like [`stream`]:
///   no replay, just live events.
/// - When the requested id has aged out of the retained replay window, a single
///   `gap` sentinel event (`event: gap`, `data: {"gap":true}`, no `id`) is
///   emitted before the partial replay so clients can detect missed events.
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::sse::LastEventId;
///
/// #[get("/events")]
/// async fn events(State(state): State<AppState>, LastEventId(last): LastEventId) -> impl IntoResponse {
///     autumn_web::sse::stream_resumable(&state, "feed", last)
/// }
/// ```
#[cfg(feature = "ws")]
pub fn stream_resumable(
    state: &crate::AppState,
    topic: &str,
    last_event_id: Option<u64>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>> + use<>> {
    use futures::StreamExt as _;
    use tokio::sync::broadcast::error::RecvError;

    let crate::channels::ResumeHandle {
        subscriber,
        replay,
        gap,
        next_live_id,
        resumable,
    } = state.channels().resume(topic, last_event_id);

    // A live-only backend (Redis fan-out, or any backend without a replay
    // buffer) cannot honour a `Last-Event-ID`: it has no history to replay. When
    // a client reconnects asking to resume, surface the same `gap` sentinel the
    // local backend emits on overflow so the client learns replay was
    // unavailable instead of silently losing the events it missed.
    let missed_on_live_only = !resumable && last_event_id.is_some();

    // Prefix: an optional gap sentinel, then each replayed event in order.
    let mut prefix: Vec<Result<Event, Infallible>> = Vec::with_capacity(replay.len() + 1);
    if gap || missed_on_live_only {
        prefix.push(Ok(Event::default().event(GAP_EVENT).data(GAP_MARKER)));
    }
    for sequenced in replay {
        prefix.push(Ok(Event::default()
            .id(sequenced.id.to_string())
            .data(sequenced.message.into_string())));
    }

    // Live tail: assign ids from `next_live_id` upward. On a broadcast lag,
    // advance the counter by the number of skipped messages (ids are dense) and
    // surface a gap sentinel so clients can react.
    let live = futures::stream::unfold(
        (subscriber, next_live_id),
        |(mut subscriber, next_id)| async move {
            match subscriber.recv().await {
                Ok(msg) => {
                    let event = Event::default()
                        .id(next_id.to_string())
                        .data(msg.into_string());
                    Some((Ok(event), (subscriber, next_id.saturating_add(1))))
                }
                Err(RecvError::Lagged(skipped)) => {
                    let event = Event::default().event(GAP_EVENT).data(GAP_MARKER);
                    Some((Ok(event), (subscriber, next_id.saturating_add(skipped))))
                }
                Err(RecvError::Closed) => None,
            }
        },
    );

    let stream = futures::stream::iter(prefix).chain(live);
    Sse::new(stream).keep_alive(keep_alive())
}

/// Subscribe to a channel topic and return an SSE response stream.
///
/// This is the one-line route primitive for htmx's SSE extension:
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/events")]
/// async fn events(State(state): State<AppState>) -> impl IntoResponse {
///     autumn_web::sse::stream(&state, "feed")
/// }
/// ```
#[cfg(feature = "ws")]
pub fn stream(
    state: &crate::AppState,
    topic: &str,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>> + use<>> {
    from_subscriber(state.channels().subscribe(topic))
}

/// Authorize an SSE channel subscription before the subscriber is created.
///
/// This preserves the "outer handler checks access, returned stream owns the
/// live client" shape used by Autumn's WebSocket support.
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/events")]
/// async fn events(
///     State(state): State<AppState>,
///     session: Session,
/// ) -> AutumnResult<impl IntoResponse> {
///     autumn_web::sse::stream_authorized(&state, "private-feed", |_| async move {
///         if session.contains_key("user_id").await {
///             Ok(())
///         } else {
///             Err(AutumnError::unauthorized_msg("login required"))
///         }
///     })
///     .await
/// }
/// ```
///
/// # Errors
///
/// Returns the error produced by the authorization hook.
#[cfg(feature = "ws")]
pub async fn stream_authorized<E, F, Fut>(
    state: &crate::AppState,
    topic: &str,
    authorize: F,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>> + use<E, F, Fut>>, E>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let subscriber = state
        .channels()
        .subscribe_authorized(topic, authorize)
        .await?;
    Ok(from_subscriber(subscriber))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keep_alive_default() {
        let ka = keep_alive();
        // Since KeepAlive fields are private in axum, we just ensure it constructs successfully.
        // We can format it to string using debug, to verify it's properly initialized.
        let debug_str = format!("{ka:?}");
        assert!(debug_str.contains("KeepAlive"));
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn stream_helper_builds_sse_from_app_state_channels() {
        let state = crate::AppState::for_test();
        let _sse = stream(&state, "lobby");
    }

    #[test]
    fn last_event_id_parses_valid_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        assert_eq!(last_event_id(&headers), Some(42));
    }

    #[test]
    fn last_event_id_trims_whitespace() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("last-event-id", "  7  ".parse().unwrap());
        assert_eq!(last_event_id(&headers), Some(7));
    }

    #[test]
    fn last_event_id_returns_none_for_absent_or_unparseable() {
        let empty = axum::http::HeaderMap::new();
        assert_eq!(last_event_id(&empty), None);

        let mut bad = axum::http::HeaderMap::new();
        bad.insert("last-event-id", "not-a-number".parse().unwrap());
        assert_eq!(last_event_id(&bad), None);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn stream_resumable_builds_sse_from_app_state_channels() {
        let state = crate::AppState::for_test();
        let _sse = stream_resumable(&state, "lobby", Some(3));
        let _cold = stream_resumable(&state, "lobby", None);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn stream_authorized_rejects_before_subscription() {
        let state = crate::AppState::for_test();

        let result = stream_authorized(&state, "private", |topic| async move {
            assert_eq!(topic, "private");
            Err::<(), &'static str>("denied")
        })
        .await;

        assert!(matches!(result, Err("denied")));
        assert!(!state.channels().snapshot().contains_key("private"));
    }

    /// Drain an SSE response body to a raw string, stopping once no data frame
    /// arrives within `idle` (the live tail never terminates on its own).
    #[cfg(feature = "ws")]
    async fn collect_body(body: axum::body::Body, idle: Duration) -> String {
        use http_body_util::BodyExt as _;

        let mut body = body;
        let mut raw = Vec::new();
        while let Ok(Some(Ok(frame))) = tokio::time::timeout(idle, body.frame()).await {
            if let Some(data) = frame.data_ref() {
                raw.extend_from_slice(data);
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// A channels backend with no replay buffer: it uses the trait-default
    /// [`crate::channels::ChannelsBackend::resume`], so its `ResumeHandle` has
    /// `resumable == false` — exactly like the Redis fan-out backend.
    #[cfg(feature = "ws")]
    struct LiveOnlyBackend(crate::channels::LocalChannelsBackend);

    #[cfg(feature = "ws")]
    impl crate::channels::ChannelsBackend for LiveOnlyBackend {
        fn publish(
            &self,
            topic: &str,
            msg: crate::channels::ChannelMessage,
        ) -> Result<usize, crate::channels::ChannelPublishError> {
            self.0.publish(topic, msg)
        }

        fn ensure_topic(
            &self,
            topic: &str,
        ) -> std::sync::Arc<tokio::sync::broadcast::Sender<crate::channels::ChannelMessage>>
        {
            self.0.ensure_topic(topic)
        }

        fn subscribe(&self, topic: &str) -> crate::channels::Subscriber {
            self.0.subscribe(topic)
        }

        fn channel_count(&self) -> usize {
            self.0.channel_count()
        }

        fn gc(&self) {
            self.0.gc();
        }

        fn snapshot(&self) -> std::collections::HashMap<String, crate::channels::ChannelStats> {
            self.0.snapshot()
        }
    }

    // Fix #2: a live-only backend (no replay) that receives a `Last-Event-ID`
    // cannot replay the missed events, so `stream_resumable` must lead with a
    // `gap` sentinel instead of silently dropping them.
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn stream_resumable_live_only_backend_signals_gap_on_resume_request() {
        use crate::channels::{Channels, LocalChannelsBackend};

        let mut state = crate::AppState::for_test();
        state.channels = Channels::with_backend(LiveOnlyBackend(LocalChannelsBackend::new(32)));

        // Reconnect *with* a Last-Event-ID against a backend that keeps no
        // history: the client must be told replay was unavailable.
        let sse = stream_resumable(&state, "feed", Some(7));
        let body = axum::response::IntoResponse::into_response(sse).into_body();
        let raw = collect_body(body, Duration::from_millis(150)).await;

        assert!(
            raw.contains("event: gap") && raw.contains("\"gap\":true"),
            "live-only resume must emit a gap sentinel, got: {raw:?}"
        );
        assert!(
            !raw.contains("id:"),
            "the gap sentinel carries no id: {raw:?}"
        );

        // A cold connection (no Last-Event-ID) on the same backend must NOT gap.
        let cold = stream_resumable(&state, "feed", None);
        let cold_body = axum::response::IntoResponse::into_response(cold).into_body();
        let cold_raw = collect_body(cold_body, Duration::from_millis(150)).await;
        assert!(
            !cold_raw.contains("event: gap"),
            "cold connect must not gap on a live-only backend: {cold_raw:?}"
        );
    }

    // Fix #5: a live subscriber that falls behind the broadcast buffer receives
    // `RecvError::Lagged`; `stream_resumable` must surface a `gap` sentinel and
    // keep the post-lag ids dense and matching the real published ids.
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn stream_resumable_live_lag_emits_gap_and_keeps_ids_dense() {
        // Broadcast capacity 2: a burst of 10 overruns a not-yet-polled
        // subscriber, so its first `recv` lags by 8 and the buffer retains the
        // last two (ids 9 and 10).
        let mut state = crate::AppState::for_test();
        state.channels = crate::channels::Channels::new(2);
        let topic = "lag";

        // Cold connect subscribes now, but we do not poll the body yet.
        let sse = stream_resumable(&state, topic, None);

        for i in 1..=10 {
            state
                .channels()
                .publish(topic, format!("e{i}"))
                .expect("publish should not fail");
        }

        let body = axum::response::IntoResponse::into_response(sse).into_body();
        let raw = collect_body(body, Duration::from_millis(200)).await;

        let gap_pos = raw
            .find("event: gap")
            .unwrap_or_else(|| panic!("a broadcast lag must emit a gap sentinel: {raw:?}"));
        assert!(raw[gap_pos..].contains("\"gap\":true"));

        // Post-lag frames keep dense ids (9, 10) matching the real published ids.
        let id9 = raw
            .find("id: 9")
            .unwrap_or_else(|| panic!("post-lag id 9 must appear: {raw:?}"));
        let id10 = raw
            .find("id: 10")
            .unwrap_or_else(|| panic!("post-lag id 10 must appear: {raw:?}"));
        assert!(
            gap_pos < id9 && id9 < id10,
            "gap must precede the dense post-lag ids, in order: {raw:?}"
        );
        assert!(
            raw.contains("data: e9") && raw.contains("data: e10"),
            "post-lag payloads must match the real published ids: {raw:?}"
        );
    }
}
