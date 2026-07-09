//! WebSocket live feed — real-time notifications for post and comment activity.
//!
//! Demonstrates: `#[ws]` macro, `Channels` pub/sub, `CancellationToken`
//! for graceful shutdown, the durable app-db relay that keeps separate
//! web and worker processes on the same live-feed stream, and `Presence`
//! for live viewer counts on subreddit pages.

use autumn_web::extract::State;
use autumn_web::prelude::*;
use autumn_web::ws::{Message, WebSocket, WithShutdown, WsHandler};
use tokio_util::sync::CancellationToken;

/// WebSocket endpoint for the global activity feed.
///
/// Clients connect to `/ws/feed` and receive JSON messages whenever
/// a new post or comment is created anywhere on the site. The
/// pre-upgrade function captures `AppState` for channel access;
/// the returned closure owns the live socket.
#[ws("/ws/feed")]
pub async fn live_feed(state: AppState) -> impl WsHandler {
    let channels = state.channels().clone();
    let mut rx = channels.subscribe("feed");

    WithShutdown(
        |mut socket: WebSocket, shutdown: CancellationToken| async move {
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Ok(cm) => {
                                if socket.send(Message::Text(cm.into_string().into())).await.is_err() {
                                    break; // Client disconnected
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("feed subscriber lagged by {n} messages");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    () = shutdown.cancelled() => {
                        socket.send(Message::Close(None)).await.ok();
                        break;
                    }
                }
            }
        },
    )
}

/// JSON health snapshot for the durable live-feed relay.
///
/// Operators can poll this endpoint to see which wake path the process is
/// currently using, whether reconnect attempts are happening, and whether the
/// relay has recently replayed durable events.
#[get("/api/live/relay/health")]
pub async fn live_feed_health(
    State(state): State<AppState>,
) -> AutumnResult<Json<crate::live_events::LiveFeedRelayHealthSnapshot>> {
    crate::live_events::live_feed_relay_health_snapshot(&state)
        .map(Json)
        .ok_or_else(|| {
            AutumnError::service_unavailable_msg(
                "reddit-clone live-feed relay health is not installed",
            )
        })
}

/// WebSocket endpoint for a specific subreddit's activity.
///
/// Clients connect to `/ws/r/{slug}` and receive JSON notifications
/// for posts and comments in that community only.
#[ws("/ws/r/{slug}")]
pub async fn subreddit_feed(
    slug: autumn_web::extract::Path<String>,
    state: AppState,
) -> impl WsHandler {
    let channel_name = format!("r/{}", *slug);
    let channels = state.channels().clone();
    let mut rx = channels.subscribe(&channel_name);

    WithShutdown(
        |mut socket: WebSocket, shutdown: CancellationToken| async move {
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Ok(cm) => {
                                if socket.send(Message::Text(cm.into_string().into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    () = shutdown.cancelled() => {
                        socket.send(Message::Close(None)).await.ok();
                        break;
                    }
                }
            }
        },
    )
}

/// JSON list of who is currently viewing a subreddit.
///
/// Uses the `Presence` extractor — no extra setup required. The presence
/// topic mirrors the channel name so join/leave events are co-located with
/// the subreddit's live-feed channel on `presence:r/{slug}`.
///
/// ```bash
/// curl http://localhost:3000/api/r/rust/viewers
/// ```
#[get("/api/r/{slug}/viewers")]
pub async fn subreddit_viewers(
    presence: Presence,
    slug: Path<String>,
) -> Json<Vec<autumn_web::presence::PresenceEntry>> {
    Json(presence.list(&format!("r/{}", *slug)))
}

/// SSE endpoint that streams the current viewer count for a subreddit.
///
/// Intended for the subreddit sidebar badge (`N browsing`). Subscribers
/// receive an updated count whenever anyone joins or leaves.
///
/// Connect by adding `hx-ext="sse" sse-connect="/r/{slug}/viewers/stream"`
/// to a container element and `sse-swap="viewer-count"` on the badge.
#[get("/r/{slug}/viewers/stream")]
pub async fn subreddit_viewer_stream(
    State(state): State<AppState>,
    presence: Presence,
    slug: Path<String>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let topic = format!("r/{}", *slug);
    let presence_channel = format!("presence:{topic}");
    let mut rx = state.channels().subscribe(&presence_channel);

    // Generate a unique key so each browser tab is tracked independently.
    // In an authenticated app, use the session user ID as the key so that
    // multiple tabs from the same user collapse into one PresenceEntry.
    let tab_id = uuid::Uuid::new_v4().to_string();
    let handle = presence.track(&topic, tab_id, serde_json::json!({}));

    let initial_count = presence.list(&topic).len();
    let initial = Event::default()
        .event("viewer-count")
        .data(initial_count.to_string());

    // Refresh the presence lease on a cadence shorter than the 30 s sweep TTL
    // so long-lived SSE connections are not evicted while the browser is still
    // open.
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    heartbeat.tick().await; // consume the immediate first tick

    let stream = async_stream::stream! {
        yield Ok(initial);

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(_) => {
                            let count = presence.list(&topic).len();
                            yield Ok(Event::default().event("viewer-count").data(count.to_string()));
                        }
                        // Recover from a lagged broadcast buffer by re-reading the
                        // current count and continuing — avoids disconnecting viewers
                        // under bursts of join/leave events.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let count = presence.list(&topic).len();
                            yield Ok(Event::default().event("viewer-count").data(count.to_string()));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = heartbeat.tick() => {
                    handle.refresh();
                }
            }
        }

        // Explicit drop: when the SSE stream ends (browser navigates away),
        // this handle drops, broadcasting the leave event automatically.
        drop(handle);
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// SSE endpoint for real-time post OOB fragment updates.
///
/// Any mutation made through `PgPostRepository` (save / update_by_id /
/// delete_by_id) publishes an `hx-swap-oob` fragment on the `"posts"` channel,
/// which this stream relays to connected browsers.
///
/// Wire a list container with:
/// ```html
/// <ul id="posts-list"
///     hx-ext="sse"
///     sse-connect="/posts/stream"
///     sse-swap="message">
/// </ul>
/// ```
/// Each arriving fragment patches the matching `#post-{id}` element in place.
#[get("/posts/stream")]
pub async fn posts_stream(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    autumn_web::sse::stream(&state, "posts")
}

/// SSE endpoint for subreddit-specific post OOB updates.
#[get("/r/{slug}/posts/stream")]
pub async fn subreddit_posts_stream(
    State(state): State<AppState>,
    slug: Path<String>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let topic = format!("posts:r/{}", *slug);
    autumn_web::sse::stream(&state, &topic)
}
