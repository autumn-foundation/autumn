//! Named broadcast channel registry for real-time messaging.
//!
//! [`Channels`] provides a lightweight pub-sub primitive with a local
//! in-process backend by default and an optional Redis pub/sub backend for
//! multi-replica fan-out.
//!
//! # Examples
//!
//! ```rust
//! use autumn_web::channels::Channels;
//!
//! let channels = Channels::new(32);
//! let tx = channels.sender("lobby");
//! let mut rx = channels.subscribe("lobby");
//!
//! tx.send("hello").ok();
//! # // In async context: let msg = rx.recv().await.expect("should receive");
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::broadcast;

#[cfg(feature = "redis")]
const REDIS_PUBLISH_QUEUE_CAPACITY: usize = 1024;

/// A registry of named broadcast channels.
#[derive(Clone)]
pub struct Channels {
    backend: Arc<dyn ChannelsBackend>,
}

/// Backend abstraction for channel fan-out.
pub trait ChannelsBackend: Send + Sync + 'static {
    /// Publish one message to a topic.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelPublishError`] if the backend cannot accept the
    /// publish request.
    fn publish(&self, topic: &str, msg: ChannelMessage) -> Result<usize, ChannelPublishError>;

    /// Ensure a local topic exists and return a keepalive sender handle.
    fn ensure_topic(&self, topic: &str) -> Arc<broadcast::Sender<ChannelMessage>>;

    /// Subscribe to future messages on a topic.
    fn subscribe(&self, topic: &str) -> Subscriber;

    /// Resume a subscription, replaying buffered events newer than
    /// `last_event_id` before continuing live.
    ///
    /// The default implementation is live-only: it returns a fresh subscriber
    /// with no replay and no history, so backends without a replay buffer (such
    /// as the Redis fan-out backend) degrade gracefully to today's behaviour.
    fn resume(&self, topic: &str, last_event_id: Option<u64>) -> ResumeHandle {
        let _ = last_event_id;
        ResumeHandle {
            subscriber: self.subscribe(topic),
            replay: Vec::new(),
            gap: false,
            next_live_id: 1,
            resumable: false,
        }
    }

    /// Return the number of topics known to this backend.
    fn channel_count(&self) -> usize;

    /// Remove idle local topic registries when supported.
    fn gc(&self);

    /// Return per-topic subscriber and delivery metrics.
    fn snapshot(&self) -> HashMap<String, ChannelStats>;
}

/// Local in-process [`tokio::sync::broadcast`] channel backend.
#[derive(Clone)]
pub struct LocalChannelsBackend {
    inner: Arc<LocalChannelsInner>,
}

struct LocalChannelsInner {
    capacity: usize,
    replay_capacity: usize,
    registry: Mutex<HashMap<String, Arc<TopicState>>>,
    metrics: Arc<ChannelMetrics>,
}

/// Default per-topic replay ring buffer capacity when none is configured.
const DEFAULT_REPLAY_CAPACITY: usize = 256;

/// Per-topic state shared by the publish and resume paths.
///
/// The `sender` (a broadcast fan-out handle) and the `replay` ring buffer live
/// behind the same [`Arc`] so that a publish can assign the next monotonic id,
/// append to the buffer, and broadcast — all under one lock — while a
/// concurrent [`ChannelsBackend::resume`] can subscribe under that same lock
/// and snapshot the buffer for a gapless seam.
struct TopicState {
    sender: Arc<broadcast::Sender<ChannelMessage>>,
    replay: Mutex<ReplayBuffer>,
}

/// Bounded per-topic ring buffer of recently published messages.
struct ReplayBuffer {
    /// Id to assign to the next published message (starts at 1).
    next_id: u64,
    /// Retention capacity, `N` (always `>= 1`).
    cap: usize,
    /// Retained `(id, message)` pairs in ascending id order.
    buf: VecDeque<(u64, ChannelMessage)>,
}

/// A replayed message paired with its monotonic per-topic id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencedMessage {
    /// Monotonic per-topic event id.
    pub id: u64,
    /// The buffered message payload.
    pub message: ChannelMessage,
}

/// Outcome of a [`ChannelsBackend::resume`] request.
///
/// Combines the buffered events a client missed (`replay`), a live
/// `subscriber` for everything published afterwards, and the id bookkeeping the
/// SSE layer needs to keep event ids monotonic across the replay/live seam.
pub struct ResumeHandle {
    /// Live subscriber for messages published after the resume point.
    pub subscriber: Subscriber,
    /// Buffered events with `id > last_event_id`, in ascending id order.
    pub replay: Vec<SequencedMessage>,
    /// `true` when the requested resume point predates the oldest retained id
    /// (the buffer overflowed), signalling missed events the replay cannot
    /// recover.
    pub gap: bool,
    /// Id to assign to the first live message read from `subscriber`.
    pub next_live_id: u64,
    /// `true` only for the in-process local backend, which actually retains a
    /// replay buffer. Other backends return a live-only handle.
    pub resumable: bool,
}

/// A message sent through a broadcast channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelMessage(pub String);

impl From<String> for ChannelMessage {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ChannelMessage {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl ChannelMessage {
    /// Get the message content as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the message, returning the inner `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ChannelMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-topic channel metrics exposed by `/actuator/channels`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ChannelStats {
    /// Current active subscriber count.
    pub subscriber_count: usize,
    /// Successful local deliveries for this topic over this process lifetime.
    pub lifetime_publish_count: u64,
    /// Messages dropped because no local receiver accepted them.
    pub dropped_count: u64,
    /// Messages skipped by slow subscribers.
    pub lagged_count: u64,
}

#[derive(Default)]
struct ChannelMetrics {
    counters: Mutex<HashMap<String, ChannelMetricCounters>>,
}

#[derive(Clone, Default)]
struct ChannelMetricCounters {
    publishes: u64,
    drops: u64,
    lags: u64,
}

impl ChannelMetrics {
    fn ensure_topic(&self, topic: &str) {
        let mut counters = self.counters.lock().expect("channel metrics lock poisoned");
        counters.entry(topic.to_owned()).or_default();
    }

    fn record_publish(&self, topic: &str) {
        let mut counters = self.counters.lock().expect("channel metrics lock poisoned");
        let stats = counters.entry(topic.to_owned()).or_default();
        stats.publishes = stats.publishes.saturating_add(1);
        drop(counters);
    }

    fn record_dropped(&self, topic: &str, count: u64) {
        let mut counters = self.counters.lock().expect("channel metrics lock poisoned");
        let stats = counters.entry(topic.to_owned()).or_default();
        stats.drops = stats.drops.saturating_add(count);
        drop(counters);
    }

    fn record_lagged(&self, topic: &str, count: u64) {
        let mut counters = self.counters.lock().expect("channel metrics lock poisoned");
        let stats = counters.entry(topic.to_owned()).or_default();
        stats.lags = stats.lags.saturating_add(count);
        drop(counters);
    }

    fn snapshot(&self) -> HashMap<String, ChannelMetricCounters> {
        self.counters
            .lock()
            .expect("channel metrics lock poisoned")
            .clone()
    }

    fn remove_topics(&self, topics: &HashSet<String>) {
        if topics.is_empty() {
            return;
        }

        let mut counters = self.counters.lock().expect("channel metrics lock poisoned");
        counters.retain(|topic, _| !topics.contains(topic));
        drop(counters);
    }
}

/// Error returned when a channel backend cannot accept a publish request.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ChannelPublishError {
    /// The backend has shut down and can no longer accept publish requests.
    #[error("channel backend is closed")]
    BackendClosed,
    /// The backend's bounded publish queue is full.
    #[error("channel backend publish queue is full")]
    QueueFull,
}

/// Error returned by the htmx/raw broadcast facade.
#[derive(Debug, Error)]
pub enum BroadcastError {
    /// Raw byte payloads must be UTF-8 because htmx SSE and WebSocket text
    /// transports consume text frames.
    #[error("broadcast payload is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),

    /// The selected channel backend rejected the publish request.
    #[error(transparent)]
    Publish(#[from] ChannelPublishError),
}

/// Raw broadcast payload accepted by [`Broadcast::publish`].
pub enum BroadcastPayload {
    /// Text payload.
    Text(String),
    /// Byte payload, decoded as UTF-8 before publishing.
    Bytes(Vec<u8>),
}

impl From<&str> for BroadcastPayload {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for BroadcastPayload {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&String> for BroadcastPayload {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

impl From<Vec<u8>> for BroadcastPayload {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<&[u8]> for BroadcastPayload {
    fn from(value: &[u8]) -> Self {
        Self::Bytes(value.to_vec())
    }
}

impl<const N: usize> From<&[u8; N]> for BroadcastPayload {
    fn from(value: &[u8; N]) -> Self {
        Self::Bytes(value.to_vec())
    }
}

/// Productive publishing facade for htmx-oriented applications.
#[derive(Clone)]
pub struct Broadcast {
    channels: Channels,
}

impl Broadcast {
    /// Create a broadcast facade from a channel registry.
    #[must_use]
    pub const fn new(channels: Channels) -> Self {
        Self { channels }
    }

    /// Publish a raw UTF-8 payload to a topic.
    ///
    /// ```
    /// use autumn_web::channels::Channels;
    ///
    /// let channels = Channels::new(16);
    /// channels
    ///     .broadcast()
    ///     .publish("feed", b"raw fragment".as_slice())
    ///     .expect("raw publish should succeed");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`BroadcastError::InvalidUtf8`] for invalid byte payloads or
    /// [`BroadcastError::Publish`] when the backend rejects the publish.
    pub fn publish(
        &self,
        topic: &str,
        payload: impl Into<BroadcastPayload>,
    ) -> Result<usize, BroadcastError> {
        let message = match payload.into() {
            BroadcastPayload::Text(text) => ChannelMessage(text),
            BroadcastPayload::Bytes(bytes) => ChannelMessage(String::from_utf8(bytes)?),
        };
        Ok(self.channels.publish(topic, message)?)
    }

    /// Publish a Maud fragment wrapped in an htmx out-of-band envelope.
    ///
    /// ```
    /// use autumn_web::channels::Channels;
    /// use maud::html;
    ///
    /// let channels = Channels::new(16);
    /// channels
    ///     .broadcast()
    ///     .publish_html("feed", &html! { div id="notice" { "Saved" } })
    ///     .expect("html publish should succeed");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`BroadcastError::Publish`] when the selected backend rejects
    /// the publish request.
    #[cfg(feature = "maud")]
    pub fn publish_html(
        &self,
        topic: &str,
        fragment: &maud::Markup,
    ) -> Result<usize, BroadcastError> {
        self.publish(topic, htmx_oob_envelope(fragment))
    }

    /// Publish a Maud fragment wrapped in a custom htmx out-of-band swap strategy.
    ///
    /// ```
    /// use autumn_web::channels::Channels;
    /// use autumn_web::htmx::OobSwap;
    /// use maud::html;
    ///
    /// let channels = Channels::new(16);
    /// channels
    ///     .broadcast()
    ///     .publish_oob("feed", "notice", &OobSwap::OuterHTML, &html! { div id="notice" { "Saved" } })
    ///     .expect("html publish should succeed");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`BroadcastError::Publish`] when the selected backend rejects
    /// the publish request.
    #[cfg(feature = "maud")]
    pub fn publish_oob(
        &self,
        topic: &str,
        id: &str,
        strategy: &crate::htmx::OobSwap,
        fragment: &maud::Markup,
    ) -> Result<usize, BroadcastError> {
        use maud::Render;
        let html = fragment.render().into_string();
        let envelope = sse_oob_envelope(id, strategy, &html);
        self.publish(topic, envelope)
    }
}

#[cfg(feature = "maud")]
fn htmx_oob_envelope(fragment: &maud::Markup) -> String {
    use crate::htmx::HtmxFragments;
    use maud::Render;
    HtmxFragments::oob_only()
        .oob("", fragment.clone())
        .render()
        .into_string()
}

/// Format an OOB fragment for delivery over SSE.
///
/// Unlike HTTP responses (where htmx's full swap pipeline unwraps `<template>`
/// elements), the SSE swap pipeline processes `hx-swap-oob` on the *element
/// itself*. Wrapping in `<template hx-swap-oob="...">` causes the attribute to
/// land on the template node, whose `childNodes` is always empty — so htmx
/// performs the swap on nothing.
///
/// Correct SSE formats:
/// - **`OobSwap::True`** (update) — inject `hx-swap-oob="true"` onto the root
///   element; htmx replaces the matching DOM element via outerHTML.
/// - **`OobSwap::Delete`** — emit a tombstone `<div id="{id}" hx-swap-oob="delete"></div>`;
///   htmx deletes the matching element.
/// - **All other strategies** — wrap the fragment in a `<div hx-swap-oob="…">`
///   container so that htmx inserts the container's *children* at the target.
#[cfg(feature = "maud")]
fn sse_oob_envelope(id: &str, strategy: &crate::htmx::OobSwap, fragment_html: &str) -> String {
    use crate::htmx::{OobMethod, OobSwap};
    match strategy {
        OobSwap::Delete => {
            format!("<div id=\"{id}\" hx-swap-oob=\"delete\"></div>")
        }
        OobSwap::True => inject_oob_attr(fragment_html, "true"),
        OobSwap::OuterHTML => inject_oob_attr(fragment_html, "outerHTML"),
        // For targeted outerHTML, htmx replaces the CSS-selected element with
        // whichever element carries hx-swap-oob. Inject the attribute onto the
        // fragment root instead of wrapping it so the rendered row (not a synthetic
        // div) becomes the replacement.
        OobSwap::Target(OobMethod::OuterHTML, selector) => {
            let value = format!("outerHTML:{selector}");
            inject_oob_attr(fragment_html, &value)
        }
        OobSwap::Raw => fragment_html.to_string(),
        // For outerHTML custom values inject the attribute on the fragment root
        // so htmx replaces the selected element with this element directly.
        // For all other strategies (beforeend, afterbegin, …) wrap in a <div>
        // so htmx inserts the div's *children* at the target rather than the
        // carrier element's children, which would strip the fragment's root tag.
        OobSwap::Custom(val) if val == "outerHTML" || val.starts_with("outerHTML:") => {
            inject_oob_attr(fragment_html, val)
        }
        OobSwap::Custom(val) => {
            let escaped = val.replace('"', "&quot;");
            format!("<div hx-swap-oob=\"{escaped}\">{fragment_html}</div>")
        }
        _ => {
            let value = strategy.format_value(id).replace('"', "&quot;");
            format!("<div hx-swap-oob=\"{value}\">{fragment_html}</div>")
        }
    }
}

/// Inject `hx-swap-oob="{value}"` into the opening tag of the root element.
///
/// Finds the first tag name boundary (space or `>`) and inserts the attribute
/// before it, e.g. `<li id="x">` → `<li hx-swap-oob="true" id="x">`.
#[cfg(feature = "maud")]
pub(crate) fn inject_oob_attr(html: &str, value: &str) -> String {
    if let Some(lt) = html.find('<') {
        let after_lt = &html[lt + 1..];
        if let Some(pos) = after_lt.find([' ', '>']) {
            let insert_at = lt + 1 + pos;
            return format!(
                "{} hx-swap-oob=\"{value}\"{}",
                &html[..insert_at],
                &html[insert_at..]
            );
        }
    }
    html.to_string()
}

/// A sender handle for a broadcast channel.
#[derive(Clone)]
pub struct Sender {
    topic: String,
    backend: Arc<dyn ChannelsBackend>,
    keepalive: Arc<broadcast::Sender<ChannelMessage>>,
}

impl Sender {
    /// Broadcast a message to all current subscribers of this channel.
    ///
    /// Publishing to a topic with no subscribers is not fatal; the backend
    /// records a drop metric and returns `Ok(0)`.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelPublishError`] if the backend is closed.
    pub fn send(&self, msg: impl Into<ChannelMessage>) -> Result<usize, ChannelPublishError> {
        self.backend.publish(&self.topic, msg.into())
    }

    /// Returns the current number of active subscribers.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.keepalive.receiver_count()
    }
}

/// A subscriber handle for a broadcast channel.
pub struct Subscriber {
    topic: String,
    inner: broadcast::Receiver<ChannelMessage>,
    metrics: Arc<ChannelMetrics>,
}

impl Subscriber {
    /// Receive the next message from the channel.
    ///
    /// # Errors
    ///
    /// Returns `RecvError::Closed` if all senders have been dropped, or
    /// `RecvError::Lagged(n)` if messages were skipped.
    pub async fn recv(&mut self) -> Result<ChannelMessage, broadcast::error::RecvError> {
        match self.inner.recv().await {
            Err(broadcast::error::RecvError::Lagged(count)) => {
                self.metrics.record_lagged(&self.topic, count);
                Err(broadcast::error::RecvError::Lagged(count))
            }
            result => result,
        }
    }

    /// Try to receive a message without waiting.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`broadcast::Receiver::try_recv`].
    pub fn try_recv(&mut self) -> Result<ChannelMessage, broadcast::error::TryRecvError> {
        match self.inner.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(count)) => {
                self.metrics.record_lagged(&self.topic, count);
                Err(broadcast::error::TryRecvError::Lagged(count))
            }
            result => result,
        }
    }

    /// Convert this subscriber into a stream of channel messages.
    #[cfg(feature = "ws")]
    pub fn into_stream(self) -> impl tokio_stream::Stream<Item = ChannelMessage> {
        use tokio_stream::StreamExt;
        let topic = self.topic;
        let metrics = self.metrics;
        tokio_stream::wrappers::BroadcastStream::new(self.inner).filter_map(move |result| {
            if let Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(count)) =
                &result
            {
                metrics.record_lagged(&topic, *count);
            }
            result.ok()
        })
    }
}

impl LocalChannelsBackend {
    /// Create a local backend with the given per-topic buffer capacity.
    ///
    /// The replay ring buffer defaults to [`DEFAULT_REPLAY_CAPACITY`]. Use
    /// [`LocalChannelsBackend::with_replay_capacity`] to override it.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_replay_capacity(capacity, DEFAULT_REPLAY_CAPACITY)
    }

    /// Create a local backend with explicit broadcast and replay capacities.
    ///
    /// `capacity` sizes the live [`tokio::sync::broadcast`] ring; `replay_capacity`
    /// (`N`) is the number of most-recent events retained per topic for
    /// `Last-Event-ID` replay. Memory is `O(N)` per topic regardless of
    /// throughput.
    #[must_use]
    pub fn with_replay_capacity(capacity: usize, replay_capacity: usize) -> Self {
        Self {
            inner: Arc::new(LocalChannelsInner {
                capacity: capacity.clamp(1, 16_384),
                replay_capacity: replay_capacity.clamp(1, 1_048_576),
                registry: Mutex::new(HashMap::new()),
                metrics: Arc::new(ChannelMetrics::default()),
            }),
        }
    }

    fn get_or_create_topic(&self, topic: &str) -> Arc<TopicState> {
        let mut registry = self.inner.registry.lock().expect("channels lock poisoned");

        #[allow(clippy::option_if_let_else)]
        if let Some(state) = registry.get(topic) {
            Arc::clone(state)
        } else {
            let sender = Arc::new(broadcast::channel(self.inner.capacity).0);
            let state = Arc::new(TopicState {
                sender,
                replay: Mutex::new(ReplayBuffer {
                    next_id: 1,
                    cap: self.inner.replay_capacity.max(1),
                    buf: VecDeque::new(),
                }),
            });
            registry.insert(topic.to_owned(), Arc::clone(&state));
            state
        }
    }

    fn get_or_create_sender(&self, topic: &str) -> Arc<broadcast::Sender<ChannelMessage>> {
        Arc::clone(&self.get_or_create_topic(topic).sender)
    }

    fn publish_local(&self, topic: &str, msg: ChannelMessage) -> usize {
        let count = self.send_without_publish_metric(topic, msg);
        if count > 0 {
            self.inner.metrics.record_publish(topic);
        }
        count
    }

    fn send_without_publish_metric(&self, topic: &str, msg: ChannelMessage) -> usize {
        let state = self.get_or_create_topic(topic);
        // Assign the id, append to the replay buffer, and broadcast — all while
        // holding the replay lock. `resume` subscribes under this same lock, so
        // every message a resumed subscriber receives is published strictly
        // after its snapshot, keeping the replay/live seam gapless. The id is
        // assigned even when there are zero receivers so ids stay dense.
        let mut replay = state.replay.lock().expect("channel replay lock poisoned");
        let id = replay.next_id;
        replay.next_id = replay.next_id.saturating_add(1);
        replay.buf.push_back((id, msg.clone()));
        while replay.buf.len() > replay.cap {
            replay.buf.pop_front();
        }
        let result = state.sender.send(msg);
        drop(replay);

        match result {
            Ok(count) => count,
            Err(_error) => {
                self.inner.metrics.record_dropped(topic, 1);
                0
            }
        }
    }

    fn resume_local(&self, topic: &str, last_event_id: Option<u64>) -> ResumeHandle {
        let state = self.get_or_create_topic(topic);
        self.inner.metrics.ensure_topic(topic);

        // Subscribe and snapshot under the replay lock so the seam is clean:
        // publish holds this same lock while assigning ids and broadcasting, so
        // `rx` can only ever observe messages published strictly after this
        // point (ids `start_id + 1, start_id + 2, ...`), none of which are in
        // the snapshot below.
        let replay_guard = state.replay.lock().expect("channel replay lock poisoned");
        let rx = state.sender.subscribe();
        let start_id = replay_guard.next_id.saturating_sub(1);
        let oldest = replay_guard.buf.front().map(|(id, _)| *id);

        let (replay, gap) = last_event_id.map_or_else(
            // Cold connection: no replay, just live events.
            || (Vec::new(), false),
            |last| {
                let replay: Vec<SequencedMessage> = replay_guard
                    .buf
                    .iter()
                    .filter(|(id, _)| *id > last)
                    .map(|(id, message)| SequencedMessage {
                        id: *id,
                        message: message.clone(),
                    })
                    .collect();
                // Gap in two cases:
                //  1. The requested resume point precedes the oldest retained
                //     id: the client's next-expected id (`last + 1`) aged out of
                //     the window, so the replay would be partial.
                //  2. The client's `last` id is in the future relative to the
                //     current server state (`last > start_id`): after a process
                //     restart or topic GC the monotonic counter resets to `1`,
                //     so a client reconnecting with a stale-large
                //     `Last-Event-ID` would otherwise get an empty replay and no
                //     signal that its history is unrecoverable. Flag the epoch
                //     reset so the client can resynchronise.
                let gap =
                    oldest.is_some_and(|oldest| oldest > last.saturating_add(1)) || last > start_id;
                (replay, gap)
            },
        );
        drop(replay_guard);

        let subscriber = Subscriber {
            topic: topic.to_owned(),
            inner: rx,
            metrics: Arc::clone(&self.inner.metrics),
        };

        ResumeHandle {
            subscriber,
            replay,
            gap,
            next_live_id: start_id.saturating_add(1),
            resumable: true,
        }
    }
}

impl ChannelsBackend for LocalChannelsBackend {
    fn publish(&self, topic: &str, msg: ChannelMessage) -> Result<usize, ChannelPublishError> {
        Ok(self.publish_local(topic, msg))
    }

    fn ensure_topic(&self, topic: &str) -> Arc<broadcast::Sender<ChannelMessage>> {
        // NOTE: this returns the RAW `broadcast::Sender`; sending directly on it
        // bypasses replay-id assignment and the replay buffer. Publish via
        // `Channels::publish` / `Broadcast::publish` / `Channels::sender().send()`
        // (all of which route through `publish` and stay id-assigned) to keep
        // resumable topics resumable (see the resumable-SSE limitations in
        // docs/guide/realtime.md, issue #1356).
        self.inner.metrics.ensure_topic(topic);
        self.get_or_create_sender(topic)
    }

    fn subscribe(&self, topic: &str) -> Subscriber {
        let tx = self.ensure_topic(topic);
        Subscriber {
            topic: topic.to_owned(),
            inner: tx.subscribe(),
            metrics: Arc::clone(&self.inner.metrics),
        }
    }

    fn resume(&self, topic: &str, last_event_id: Option<u64>) -> ResumeHandle {
        self.resume_local(topic, last_event_id)
    }

    fn channel_count(&self) -> usize {
        let registry = self.inner.registry.lock().expect("channels lock poisoned");
        registry.len()
    }

    fn gc(&self) {
        // NOTE: removing a topic here drops its replay buffer with it, so a
        // topic kept alive only by transient SSE subscribers can lose its
        // replay history during a disconnect window (resumable-SSE is in-process
        // best-effort — see docs/guide/realtime.md, issue #1356).
        let mut registry = self.inner.registry.lock().expect("channels lock poisoned");
        let mut removed_topics = HashSet::new();
        registry.retain(|topic, state| {
            // Keep topics with live receivers, or with outstanding keepalive
            // `Sender` handles (which hold clones of `state.sender`, bumping its
            // strong count above the single reference held by `state`).
            let keep = state.sender.receiver_count() > 0 || Arc::strong_count(&state.sender) > 1;
            if !keep {
                removed_topics.insert(topic.clone());
            }
            keep
        });
        drop(registry);

        self.inner.metrics.remove_topics(&removed_topics);
    }

    fn snapshot(&self) -> HashMap<String, ChannelStats> {
        // Keep registry and metrics collection in separate phases. Publish and
        // subscribe paths touch metrics before registry, so snapshot must never
        // hold the registry mutex while reading metrics.
        let subscriber_counts: HashMap<String, usize> = {
            let registry = self.inner.registry.lock().expect("channels lock poisoned");
            registry
                .iter()
                .map(|(topic, state)| (topic.clone(), state.sender.receiver_count()))
                .collect()
        };
        let metric_counters = self.inner.metrics.snapshot();

        let mut topics: HashSet<String> = metric_counters.keys().cloned().collect();
        topics.extend(subscriber_counts.keys().cloned());

        topics
            .into_iter()
            .map(|topic| {
                let subscriber_count = subscriber_counts.get(&topic).copied().unwrap_or(0);
                let counters = metric_counters.get(&topic).cloned().unwrap_or_default();
                (
                    topic,
                    ChannelStats {
                        subscriber_count,
                        lifetime_publish_count: counters.publishes,
                        dropped_count: counters.drops,
                        lagged_count: counters.lags,
                    },
                )
            })
            .collect()
    }
}

#[cfg(feature = "redis")]
#[derive(Clone)]
struct RedisChannelsBackend {
    local: LocalChannelsBackend,
    publisher: tokio::sync::mpsc::Sender<RedisPublishCommand>,
    origin_id: String,
    key_prefix: String,
}

#[cfg(feature = "redis")]
struct RedisPublishCommand {
    redis_channel: String,
    envelope: RedisEnvelope,
}

#[cfg(feature = "redis")]
#[derive(serde::Deserialize, serde::Serialize)]
struct RedisEnvelope {
    origin: String,
    topic: String,
    payload: String,
}

/// Channel backend configuration error.
#[derive(Debug, Error)]
pub enum ChannelBackendConfigError {
    /// `channels.backend = "redis"` needs `channels.redis.url`.
    #[error("channels.redis.url is required when channels.backend = \"redis\"")]
    MissingRedisUrl,
    /// Redis URL failed validation by the Redis client.
    #[error("invalid channels.redis.url: {0}")]
    InvalidRedisUrl(String),
    /// The `redis` cargo feature is required for the Redis backend.
    #[error("channels.backend = \"redis\" requires the redis cargo feature")]
    RedisFeatureDisabled,
}

#[cfg(feature = "redis")]
impl RedisChannelsBackend {
    fn from_config(
        config: &crate::config::ChannelConfig,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ChannelBackendConfigError> {
        let url = config
            .redis
            .url
            .clone()
            .filter(|url| !url.trim().is_empty())
            .ok_or(ChannelBackendConfigError::MissingRedisUrl)?;
        let client = redis::Client::open(url)
            .map_err(|error| ChannelBackendConfigError::InvalidRedisUrl(error.to_string()))?;
        let local =
            LocalChannelsBackend::with_replay_capacity(config.capacity, config.replay_buffer);
        let (publisher, receiver) = tokio::sync::mpsc::channel(REDIS_PUBLISH_QUEUE_CAPACITY);
        let origin_id = uuid::Uuid::new_v4().to_string();
        let backend = Self {
            local: local.clone(),
            publisher,
            origin_id: origin_id.clone(),
            key_prefix: config.redis.key_prefix.clone(),
        };
        spawn_redis_publisher(client.clone(), receiver, shutdown.clone());
        spawn_redis_listener(
            client,
            local,
            origin_id,
            config.redis.key_prefix.clone(),
            shutdown,
        );
        Ok(backend)
    }

    fn redis_channel(&self, topic: &str) -> String {
        redis_channel_name(&self.key_prefix, topic)
    }
}

#[cfg(feature = "redis")]
fn redis_channel_name(prefix: &str, topic: &str) -> String {
    format!("{prefix}:{topic}")
}

#[cfg(feature = "redis")]
fn redis_channel_topic<'a>(channel_prefix: &str, channel: &'a str) -> Option<&'a str> {
    channel.strip_prefix(channel_prefix)
}

#[cfg(feature = "redis")]
fn redis_channel_pattern(prefix: &str) -> String {
    format!("{prefix}:*")
}

#[cfg(feature = "redis")]
fn spawn_redis_publisher(
    client: redis::Client,
    mut receiver: tokio::sync::mpsc::Receiver<RedisPublishCommand>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        use redis::AsyncCommands as _;
        use redis::aio::{ConnectionManager, ConnectionManagerConfig};

        let mut connection =
            match ConnectionManager::new_lazy_with_config(client, ConnectionManagerConfig::new()) {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(error = %error, "failed to create Redis channels publisher");
                    return;
                }
            };

        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                Some(command) = receiver.recv() => {
                    let Ok(payload) = serde_json::to_string(&command.envelope) else {
                        tracing::warn!("failed to serialize Redis channel envelope");
                        continue;
                    };
                    if let Err(error) = connection
                        .publish::<_, _, usize>(&command.redis_channel, payload)
                        .await
                    {
                        tracing::warn!(error = %error, channel = %command.redis_channel, "Redis channel publish failed");
                    }
                }
                else => break,
            }
        }
    });
}

#[cfg(feature = "redis")]
fn spawn_redis_listener(
    client: redis::Client,
    local: LocalChannelsBackend,
    origin_id: String,
    key_prefix: String,
    shutdown: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        use futures::StreamExt as _;

        let channel_prefix = redis_channel_name(&key_prefix, "");
        let pattern = redis_channel_pattern(&key_prefix);
        loop {
            if shutdown.is_cancelled() {
                break;
            }

            let mut pubsub = match client.get_async_pubsub().await {
                Ok(pubsub) => pubsub,
                Err(error) => {
                    tracing::warn!(error = %error, "failed to connect Redis channels listener");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };

            if let Err(error) = pubsub.psubscribe(&pattern).await {
                tracing::warn!(error = %error, pattern = %pattern, "failed to subscribe Redis channels listener");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }

            let mut stream = pubsub.on_message();
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    message = stream.next() => {
                        let Some(message) = message else {
                            break;
                        };
                        let redis_channel = message.get_channel_name();
                        let payload: String = match message.get_payload() {
                            Ok(payload) => payload,
                            Err(error) => {
                                tracing::warn!(error = %error, "failed to decode Redis channel payload");
                                continue;
                            }
                        };
                        let envelope: RedisEnvelope = match serde_json::from_str(&payload) {
                            Ok(envelope) => envelope,
                            Err(error) => {
                                tracing::warn!(error = %error, "failed to parse Redis channel envelope");
                                continue;
                            }
                        };
                        deliver_redis_envelope(
                            &local,
                            &origin_id,
                            &channel_prefix,
                            redis_channel,
                            envelope,
                        );
                    }
                }
            }
        }
    });
}

#[cfg(feature = "redis")]
fn deliver_redis_envelope(
    local: &LocalChannelsBackend,
    origin_id: &str,
    channel_prefix: &str,
    redis_channel: &str,
    envelope: RedisEnvelope,
) {
    let Some(topic) = redis_channel_topic(channel_prefix, redis_channel) else {
        tracing::warn!(channel = %redis_channel, "Redis channel name did not match channel prefix");
        return;
    };

    if envelope.topic != topic {
        tracing::warn!(
            channel = %redis_channel,
            channel_topic = %topic,
            envelope_topic = %envelope.topic,
            "Redis channel envelope topic mismatch"
        );
        return;
    }

    if envelope.origin == origin_id {
        return;
    }

    local.publish_local(topic, ChannelMessage(envelope.payload));
}

#[cfg(feature = "redis")]
impl ChannelsBackend for RedisChannelsBackend {
    fn publish(&self, topic: &str, msg: ChannelMessage) -> Result<usize, ChannelPublishError> {
        let command = RedisPublishCommand {
            redis_channel: self.redis_channel(topic),
            envelope: RedisEnvelope {
                origin: self.origin_id.clone(),
                topic: topic.to_owned(),
                payload: msg.as_str().to_owned(),
            },
        };
        self.publisher
            .try_send(command)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => ChannelPublishError::QueueFull,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    ChannelPublishError::BackendClosed
                }
            })?;
        Ok(self.local.publish_local(topic, msg))
    }

    fn ensure_topic(&self, topic: &str) -> Arc<broadcast::Sender<ChannelMessage>> {
        self.local.ensure_topic(topic)
    }

    fn subscribe(&self, topic: &str) -> Subscriber {
        self.local.subscribe(topic)
    }

    fn channel_count(&self) -> usize {
        self.local.channel_count()
    }

    fn gc(&self) {
        self.local.gc();
    }

    fn snapshot(&self) -> HashMap<String, ChannelStats> {
        self.local.snapshot()
    }
}

#[cfg(feature = "ws")]
#[derive(Clone)]
pub struct InterceptedChannelsBackend {
    inner: Arc<dyn ChannelsBackend>,
    interceptors: Vec<Arc<dyn crate::interceptor::ChannelsInterceptor>>,
}

#[cfg(feature = "ws")]
impl InterceptedChannelsBackend {
    #[must_use]
    pub fn new(
        inner: Arc<dyn ChannelsBackend>,
        interceptors: Vec<Arc<dyn crate::interceptor::ChannelsInterceptor>>,
    ) -> Self {
        Self {
            inner,
            interceptors,
        }
    }
}

#[cfg(feature = "ws")]
fn run_chain(
    topic: &str,
    msg: &ChannelMessage,
    interceptors: &[Arc<dyn crate::interceptor::ChannelsInterceptor>],
    inner: &dyn ChannelsBackend,
    idx: usize,
) -> Result<usize, ChannelPublishError> {
    if idx < interceptors.len() {
        let interceptor = &interceptors[idx];
        let next = |t: &str, m: &ChannelMessage| run_chain(t, m, interceptors, inner, idx + 1);
        interceptor.intercept_publish(topic, msg, &next)
    } else {
        inner.publish(topic, msg.clone())
    }
}

#[cfg(feature = "ws")]
impl ChannelsBackend for InterceptedChannelsBackend {
    fn publish(&self, topic: &str, msg: ChannelMessage) -> Result<usize, ChannelPublishError> {
        let inner = &self.inner;
        let interceptors = &self.interceptors;

        run_chain(topic, &msg, interceptors, &**inner, 0)
    }

    fn ensure_topic(&self, topic: &str) -> Arc<broadcast::Sender<ChannelMessage>> {
        self.inner.ensure_topic(topic)
    }

    fn subscribe(&self, topic: &str) -> Subscriber {
        self.inner.subscribe(topic)
    }

    fn resume(&self, topic: &str, last_event_id: Option<u64>) -> ResumeHandle {
        self.inner.resume(topic, last_event_id)
    }

    fn channel_count(&self) -> usize {
        self.inner.channel_count()
    }

    fn gc(&self) {
        self.inner.gc();
    }

    fn snapshot(&self) -> HashMap<String, ChannelStats> {
        self.inner.snapshot()
    }
}

impl Channels {
    /// Return the underlying backend.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn ChannelsBackend> {
        &self.backend
    }

    /// Create a new local channel registry with the given buffer capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_backend(LocalChannelsBackend::new(capacity))
    }

    /// Create a registry from any backend implementation.
    #[must_use]
    pub fn with_backend(backend: impl ChannelsBackend) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Create a registry from a shared backend implementation.
    #[must_use]
    pub fn with_shared_backend(backend: Arc<dyn ChannelsBackend>) -> Self {
        Self { backend }
    }

    /// Create a channel registry from resolved framework config.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelBackendConfigError`] when a Redis backend is requested
    /// without usable Redis configuration or without the `redis` feature.
    pub fn from_config(
        config: &crate::config::ChannelConfig,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ChannelBackendConfigError> {
        match config.backend {
            crate::config::ChannelBackend::InProcess => Ok(Self::with_backend(
                LocalChannelsBackend::with_replay_capacity(config.capacity, config.replay_buffer),
            )),
            crate::config::ChannelBackend::Redis => Self::redis_from_config(config, shutdown),
        }
    }

    #[cfg(feature = "redis")]
    fn redis_from_config(
        config: &crate::config::ChannelConfig,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ChannelBackendConfigError> {
        Ok(Self::with_backend(RedisChannelsBackend::from_config(
            config, shutdown,
        )?))
    }

    #[cfg(not(feature = "redis"))]
    fn redis_from_config(
        _config: &crate::config::ChannelConfig,
        _shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ChannelBackendConfigError> {
        Err(ChannelBackendConfigError::RedisFeatureDisabled)
    }

    /// Return a htmx-friendly broadcast facade.
    #[must_use]
    pub fn broadcast(&self) -> Broadcast {
        Broadcast::new(self.clone())
    }

    /// Publish a raw channel message through the selected backend.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelPublishError`] if the backend is closed.
    pub fn publish(
        &self,
        topic: &str,
        msg: impl Into<ChannelMessage>,
    ) -> Result<usize, ChannelPublishError> {
        self.backend.publish(topic, msg.into())
    }

    /// Get or create a sender for the named channel.
    #[must_use]
    pub fn sender(&self, name: &str) -> Sender {
        let keepalive = self.backend.ensure_topic(name);
        Sender {
            topic: name.to_owned(),
            backend: Arc::clone(&self.backend),
            keepalive,
        }
    }

    /// Subscribe to the named channel.
    #[must_use]
    pub fn subscribe(&self, name: &str) -> Subscriber {
        self.backend.subscribe(name)
    }

    /// Resume a subscription, replaying buffered events newer than
    /// `last_event_id` before continuing live.
    ///
    /// Only the in-process local backend retains a replay buffer; other
    /// backends return a live-only [`ResumeHandle`]. Prefer
    /// [`crate::sse::stream_resumable`] for the SSE route primitive.
    #[must_use]
    pub fn resume(&self, topic: &str, last_event_id: Option<u64>) -> ResumeHandle {
        self.backend.resume(topic, last_event_id)
    }

    /// Authorize a channel subscription before allocating the subscriber.
    ///
    /// The hook receives the requested topic name. If it returns an error,
    /// no subscriber is created and the error is returned unchanged.
    ///
    /// ```rust,no_run
    /// use autumn_web::channels::Channels;
    ///
    /// # async fn example(channels: Channels) -> autumn_web::AutumnResult<()> {
    /// let mut rx = channels
    ///     .subscribe_authorized("private-feed", |topic| async move {
    ///         if topic == "private-feed" {
    ///             Ok(())
    ///         } else {
    ///             Err(autumn_web::AutumnError::forbidden_msg("not your feed"))
    ///         }
    ///     })
    ///     .await?;
    /// # let _ = &mut rx;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the error produced by the authorization hook.
    pub async fn subscribe_authorized<E, Fut>(
        &self,
        name: &str,
        authorize: impl FnOnce(String) -> Fut,
    ) -> Result<Subscriber, E>
    where
        Fut: Future<Output = Result<(), E>>,
    {
        authorize(name.to_owned()).await?;
        Ok(self.subscribe(name))
    }

    /// Returns the number of active topics in the registry.
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.backend.channel_count()
    }

    /// Remove channels with no active senders or receivers.
    pub fn gc(&self) {
        self.backend.gc();
    }

    /// Get a snapshot of all active channels and their metrics.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, ChannelStats> {
        self.backend.snapshot()
    }

    /// Creates an SSE response stream for a channel.
    #[cfg(feature = "ws")]
    pub fn sse_stream(
        &self,
        name: &str,
    ) -> axum::response::sse::Sse<
        impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
        + use<>,
    > {
        crate::sse::from_subscriber(self.subscribe(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_channels() {
        let channels = Channels::new(16);
        assert_eq!(channels.channel_count(), 0);
    }

    #[test]
    fn sender_creates_channel_lazily() {
        let channels = Channels::new(16);
        let _tx = channels.sender("test");
        assert_eq!(channels.channel_count(), 1);
    }

    #[test]
    fn subscribe_creates_channel_lazily() {
        let channels = Channels::new(16);
        let _rx = channels.subscribe("test");
        assert_eq!(channels.channel_count(), 1);
    }

    #[tokio::test]
    async fn send_and_receive() -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let tx = channels.sender("chat");
        let mut rx = channels.subscribe("chat");

        tx.send("hello").expect("should send");
        let msg = rx.recv().await?;
        assert_eq!(msg.as_str(), "hello");
        Ok(())
    }

    #[tokio::test]
    async fn multiple_subscribers() -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let tx = channels.sender("chat");
        let mut rx1 = channels.subscribe("chat");
        let mut rx2 = channels.subscribe("chat");

        tx.send("broadcast").expect("should send");

        let msg1 = rx1.recv().await?;
        let msg2 = rx2.recv().await?;
        assert_eq!(msg1.as_str(), "broadcast");
        assert_eq!(msg2.as_str(), "broadcast");
        Ok(())
    }

    #[test]
    fn sender_receiver_count() {
        let channels = Channels::new(16);
        let tx = channels.sender("chat");
        assert_eq!(tx.receiver_count(), 0);

        let _rx1 = channels.subscribe("chat");
        assert_eq!(tx.receiver_count(), 1);

        let _rx2 = channels.subscribe("chat");
        assert_eq!(tx.receiver_count(), 2);
    }

    #[test]
    fn channel_message_conversions() {
        let msg: ChannelMessage = "hello".into();
        assert_eq!(msg.as_str(), "hello");
        assert_eq!(msg.to_string(), "hello");

        let msg2: ChannelMessage = String::from("world").into();
        assert_eq!(msg2.into_string(), "world");
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn channels_is_clone() {
        let channels = Channels::new(16);
        let _cloned = channels.clone();
    }

    #[test]
    fn snapshot_returns_counts() {
        let channels = Channels::new(16);
        let _tx = channels.sender("empty");

        let _tx2 = channels.sender("one");
        let _rx_one = channels.subscribe("one");

        let _tx3 = channels.sender("two");
        let _rx_two_1 = channels.subscribe("two");
        let _rx_two_2 = channels.subscribe("two");

        let snap = channels.snapshot();
        assert_eq!(
            snap.get("empty").map(|stats| stats.subscriber_count),
            Some(0)
        );
        assert_eq!(snap.get("one").map(|stats| stats.subscriber_count), Some(1));
        assert_eq!(snap.get("two").map(|stats| stats.subscriber_count), Some(2));
        assert_eq!(snap.len(), 3);
    }

    #[cfg(all(feature = "ws", feature = "maud"))]
    #[tokio::test]
    async fn broadcast_publish_html_wraps_fragment_in_hx_swap_oob_envelope()
    -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let broadcast = Broadcast::new(channels.clone());
        let mut rx = channels.subscribe("feed");

        let sent = broadcast
            .publish_html(
                "feed",
                &maud::html! {
                    li id="item-1" { "one" }
                },
            )
            .expect("html publish should succeed");

        assert_eq!(sent, 1);
        let msg = rx.recv().await?;
        assert!(msg.as_str().contains("hx-swap-oob"));
        assert!(msg.as_str().contains("<li id=\"item-1\">one</li>"));
        Ok(())
    }

    #[cfg(all(feature = "ws", feature = "maud"))]
    #[tokio::test]
    async fn broadcast_publish_oob_custom_strategy() -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let broadcast = Broadcast::new(channels.clone());
        let mut rx = channels.subscribe("feed");

        let sent = broadcast
            .publish_oob(
                "feed",
                "badge",
                &crate::htmx::OobSwap::BeforeEnd,
                &maud::html! {
                    span { "3" }
                },
            )
            .expect("oob publish should succeed");

        assert_eq!(sent, 1);
        let msg = rx.recv().await?;
        assert_eq!(
            msg.as_str(),
            "<div hx-swap-oob=\"beforeend:#badge\"><span>3</span></div>"
        );
        Ok(())
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn broadcast_publish_raw_bytes_delivers_text_payload()
    -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let broadcast = Broadcast::new(channels.clone());
        let mut rx = channels.subscribe("raw");

        let sent = broadcast
            .publish("raw", b"hello".as_slice())
            .expect("raw publish should succeed");

        assert_eq!(sent, 1);
        assert_eq!(rx.recv().await?.as_str(), "hello");
        Ok(())
    }

    #[cfg(feature = "ws")]
    #[test]
    fn broadcast_publish_rejects_invalid_utf8_bytes() {
        let channels = Channels::new(16);
        let broadcast = Broadcast::new(channels);

        let error = broadcast
            .publish("raw", vec![0xff, 0xfe])
            .expect_err("invalid UTF-8 should be rejected before publishing");

        assert!(matches!(error, BroadcastError::InvalidUtf8(_)));
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn snapshot_returns_channel_metrics() -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let broadcast = Broadcast::new(channels.clone());
        let mut rx = channels.subscribe("metrics");

        broadcast
            .publish("metrics", "one")
            .expect("publish should succeed");
        let _ = rx.recv().await?;

        let snap = channels.snapshot();
        let stats = snap.get("metrics").expect("topic should be tracked");
        assert_eq!(stats.subscriber_count, 1);
        assert_eq!(stats.lifetime_publish_count, 1);
        assert_eq!(stats.dropped_count, 0);
        assert_eq!(stats.lagged_count, 0);
        Ok(())
    }

    #[cfg(feature = "ws")]
    #[test]
    fn snapshot_counts_dropped_publish_without_successful_delivery() {
        let channels = Channels::new(16);
        let sent = channels
            .broadcast()
            .publish("metrics", "one")
            .expect("publish with no subscribers should not fail");

        assert_eq!(sent, 0);
        let snap = channels.snapshot();
        let stats = snap.get("metrics").expect("topic should be tracked");
        assert_eq!(stats.subscriber_count, 0);
        assert_eq!(stats.lifetime_publish_count, 0);
        assert_eq!(stats.dropped_count, 1);
        assert_eq!(stats.lagged_count, 0);
    }

    #[test]
    fn gc_prunes_metrics_for_removed_idle_topics() {
        let channels = Channels::new(16);
        channels
            .publish("tenant:gone", "one")
            .expect("publish with no subscribers should only record a drop");

        let before_gc = channels.snapshot();
        assert!(before_gc.contains_key("tenant:gone"));

        channels.gc();

        let after_gc = channels.snapshot();
        assert!(!after_gc.contains_key("tenant:gone"));
        assert_eq!(channels.channel_count(), 0);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_listener_rejects_envelope_topic_that_mismatches_channel() {
        let local = LocalChannelsBackend::new(16);
        let mut private_rx = local.subscribe("private");
        let channel_prefix = redis_channel_name("autumn:channels", "");

        deliver_redis_envelope(
            &local,
            "local-origin",
            &channel_prefix,
            "autumn:channels:public",
            RedisEnvelope {
                origin: "remote-origin".to_owned(),
                topic: "private".to_owned(),
                payload: "secret".to_owned(),
            },
        );

        assert!(matches!(
            private_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(!local.snapshot().contains_key("public"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_listener_counts_successful_remote_deliveries() {
        let local = LocalChannelsBackend::new(16);
        let mut rx = local.subscribe("public");
        let channel_prefix = redis_channel_name("autumn:channels", "");

        deliver_redis_envelope(
            &local,
            "local-origin",
            &channel_prefix,
            "autumn:channels:public",
            RedisEnvelope {
                origin: "remote-origin".to_owned(),
                topic: "public".to_owned(),
                payload: "hello".to_owned(),
            },
        );

        assert_eq!(
            rx.try_recv()
                .expect("remote message should fan out")
                .as_str(),
            "hello"
        );
        let snapshot = local.snapshot();
        let stats = snapshot.get("public").expect("topic should be tracked");
        assert_eq!(stats.lifetime_publish_count, 1);
        assert_eq!(stats.dropped_count, 0);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_publish_rejects_when_bounded_queue_is_full() {
        let local = LocalChannelsBackend::new(16);
        let mut rx = local.subscribe("queue");
        let (publisher, _receiver) = tokio::sync::mpsc::channel(1);
        publisher
            .try_send(RedisPublishCommand {
                redis_channel: "autumn:channels:queue".to_owned(),
                envelope: RedisEnvelope {
                    origin: "origin".to_owned(),
                    topic: "queue".to_owned(),
                    payload: "already queued".to_owned(),
                },
            })
            .expect("first command should fill the queue");

        let backend = RedisChannelsBackend {
            local,
            publisher,
            origin_id: "origin".to_owned(),
            key_prefix: "autumn:channels".to_owned(),
        };

        let error = backend
            .publish("queue", ChannelMessage::from("second"))
            .expect_err("full Redis queue should reject the publish");

        assert_eq!(error, ChannelPublishError::QueueFull);
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn snapshot_releases_registry_before_waiting_on_metrics() {
        let backend = LocalChannelsBackend::new(16);
        backend.ensure_topic("race");

        let metrics_guard = backend
            .inner
            .metrics
            .counters
            .lock()
            .expect("channel metrics lock should not be poisoned");
        let registry_guard = backend
            .inner
            .registry
            .lock()
            .expect("channel registry lock should not be poisoned");
        let snapshot_backend = backend.clone();

        let handle = std::thread::spawn(move || {
            let snapshot = snapshot_backend.snapshot();
            assert!(snapshot.contains_key("race"));
        });

        std::thread::sleep(std::time::Duration::from_millis(25));
        drop(registry_guard);
        std::thread::sleep(std::time::Duration::from_millis(25));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let registry_released_before_metrics = loop {
            match backend.inner.registry.try_lock() {
                Ok(registry) => {
                    drop(registry);
                    break true;
                }
                Err(std::sync::TryLockError::WouldBlock)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::yield_now();
                }
                Err(std::sync::TryLockError::WouldBlock) => break false,
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    panic!("channel registry lock should not be poisoned: {error}");
                }
            }
        };

        drop(metrics_guard);
        handle.join().expect("snapshot thread should finish");
        assert!(
            registry_released_before_metrics,
            "snapshot held the registry mutex while waiting on metrics"
        );
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn app_state_broadcast_uses_state_channels() -> Result<(), broadcast::error::RecvError> {
        let state = crate::AppState::for_test();
        let mut rx = state.channels().subscribe("state-topic");

        state
            .broadcast()
            .publish("state-topic", "from-state")
            .expect("publish should succeed");

        assert_eq!(rx.recv().await?.as_str(), "from-state");
        Ok(())
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn subscribe_authorized_rejects_before_creating_subscriber() {
        let channels = Channels::new(16);

        let result: Result<Subscriber, &'static str> = channels
            .subscribe_authorized("private", |topic| async move {
                assert_eq!(topic, "private");
                Err("denied")
            })
            .await;

        assert!(matches!(result, Err("denied")));
        assert!(!channels.snapshot().contains_key("private"));
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn subscribe_authorized_allows_after_hook_passes()
    -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let mut rx = channels
            .subscribe_authorized("private", |topic| async move {
                assert_eq!(topic, "private");
                Ok::<(), std::convert::Infallible>(())
            })
            .await
            .expect("authorization should pass");

        channels
            .broadcast()
            .publish("private", "secret")
            .expect("publish should succeed");

        assert_eq!(rx.recv().await?.as_str(), "secret");
        Ok(())
    }

    #[test]
    fn gc_removes_dead_channels() {
        let channels = Channels::new(16);
        let _tx = channels.sender("alive");
        {
            let _tx = channels.sender("dead");
        }
        assert_eq!(channels.channel_count(), 2);
        channels.gc();
        assert_eq!(channels.channel_count(), 1);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn subscriber_into_stream() {
        use tokio_stream::StreamExt;
        let channels = Channels::new(16);
        let tx = channels.sender("test_stream");
        let rx = channels.subscribe("test_stream");

        tx.send("message 1").unwrap();
        tx.send("message 2").unwrap();

        let mut stream = rx.into_stream();
        let msg1 = stream.next().await.unwrap();
        assert_eq!(msg1.as_str(), "message 1");

        let msg2 = stream.next().await.unwrap();
        assert_eq!(msg2.as_str(), "message 2");
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn channels_sse_stream() {
        let channels = Channels::new(16);
        let tx = channels.sender("test_sse");

        let sse = channels.sse_stream("test_sse");

        tx.send("sse message").unwrap();
        let _stream = sse;
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn test_publish_oob_injects_without_template_wrapper()
    -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let mut rx = channels.subscribe("test_publish_oob");

        let oob = maud::html! { li id="item-2" { "Value" } };
        channels
            .broadcast()
            .publish_oob(
                "test_publish_oob",
                "list-id",
                &crate::htmx::OobSwap::BeforeEnd,
                &oob,
            )
            .unwrap();

        assert_eq!(
            rx.recv().await?.as_str(),
            "<div hx-swap-oob=\"beforeend:#list-id\"><li id=\"item-2\">Value</li></div>"
        );
        Ok(())
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn test_publish_oob_injects_for_outerhtml() -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let mut rx = channels.subscribe("test_publish_oob_outerhtml");

        let oob = maud::html! { li id="item-3" { "Value" } };
        channels
            .broadcast()
            .publish_oob(
                "test_publish_oob_outerhtml",
                "item-3",
                &crate::htmx::OobSwap::OuterHTML,
                &oob,
            )
            .unwrap();

        assert_eq!(
            rx.recv().await?.as_str(),
            "<li hx-swap-oob=\"outerHTML\" id=\"item-3\">Value</li>"
        );
        Ok(())
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn test_publish_oob_escapes_attributes() -> Result<(), broadcast::error::RecvError> {
        let channels = Channels::new(16);
        let mut rx = channels.subscribe("test_publish_oob_escape");

        let oob = maud::html! { li id="item-4" { "Value" } };
        channels
            .broadcast()
            .publish_oob(
                "test_publish_oob_escape",
                "\"bad-id\"",
                &crate::htmx::OobSwap::BeforeEnd,
                &oob,
            )
            .unwrap();

        assert_eq!(
            rx.recv().await?.as_str(),
            "<div hx-swap-oob=\"beforeend:#&quot;bad-id&quot;\"><li id=\"item-4\">Value</li></div>"
        );
        Ok(())
    }

    // ── sse_oob_envelope branch coverage ────────────────────────────────────────

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_delete_emits_tombstone() {
        let result = sse_oob_envelope("item-7", &crate::htmx::OobSwap::Delete, "");
        assert_eq!(result, "<div id=\"item-7\" hx-swap-oob=\"delete\"></div>");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_true_injects_on_root() {
        let frag = "<li id=\"item-8\">X</li>";
        let result = sse_oob_envelope("item-8", &crate::htmx::OobSwap::True, frag);
        assert!(
            result.contains("hx-swap-oob=\"true\""),
            "missing attr: {result}"
        );
        assert!(result.contains("<li"), "root tag stripped: {result}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_raw_passes_through_unchanged() {
        let frag = "<custom-element>data</custom-element>";
        let result = sse_oob_envelope("x", &crate::htmx::OobSwap::Raw, frag);
        assert_eq!(result, frag);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_target_outerhtml_injects_on_root() {
        use crate::htmx::{OobMethod, OobSwap};
        let frag = "<li id=\"item-9\">X</li>";
        let result = sse_oob_envelope(
            "item-9",
            &OobSwap::Target(OobMethod::OuterHTML, "#item-9".to_string()),
            frag,
        );
        assert!(
            result.contains("hx-swap-oob=\"outerHTML:#item-9\""),
            "got: {result}"
        );
        assert!(result.contains("<li"), "root tag stripped: {result}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_custom_outerhtml_injects_on_root() {
        use crate::htmx::OobSwap;
        let frag = "<li id=\"item-10\">X</li>";
        let result = sse_oob_envelope("item-10", &OobSwap::Custom("outerHTML".to_string()), frag);
        assert!(
            result.contains("hx-swap-oob=\"outerHTML\""),
            "got: {result}"
        );
        assert!(result.contains("<li"), "root tag stripped: {result}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_custom_outerhtml_selector_injects_on_root() {
        use crate::htmx::OobSwap;
        let frag = "<li id=\"item-11\">X</li>";
        let result = sse_oob_envelope(
            "item-11",
            &OobSwap::Custom("outerHTML:#item-11".to_string()),
            frag,
        );
        assert!(
            result.contains("hx-swap-oob=\"outerHTML:#item-11\""),
            "got: {result}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_custom_non_outerhtml_wraps_in_div() {
        use crate::htmx::OobSwap;
        let frag = "<li id=\"item-12\">X</li>";
        let result = sse_oob_envelope(
            "item-12",
            &OobSwap::Custom("beforeend:#items".to_string()),
            frag,
        );
        assert!(
            result.starts_with("<div hx-swap-oob=\"beforeend:#items\">"),
            "got: {result}"
        );
        assert!(
            result.contains("<li id=\"item-12\">"),
            "fragment missing: {result}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_outerhtml_injects_on_root() {
        use crate::htmx::OobSwap;
        let frag = "<li id=\"item-13\">X</li>";
        let result = sse_oob_envelope("item-13", &OobSwap::OuterHTML, frag);
        assert!(
            result.contains("hx-swap-oob=\"outerHTML\""),
            "missing outerHTML attr: {result}"
        );
        assert!(result.contains("<li"), "root tag stripped: {result}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn oob_envelope_target_beforeend_uses_catchall() {
        use crate::htmx::{OobMethod, OobSwap};
        let frag = "<li>item</li>";
        let result = sse_oob_envelope(
            "item-14",
            &OobSwap::Target(OobMethod::BeforeEnd, "#list".to_string()),
            frag,
        );
        assert!(
            result.starts_with("<div hx-swap-oob="),
            "catch-all must wrap in div: {result}"
        );
        assert!(result.contains("beforeend:#list"), "got: {result}");
    }

    // ── replay buffer / resume ──────────────────────────────────────────────

    #[test]
    fn replay_buffer_is_bounded_and_ids_are_monotonic() {
        let backend = LocalChannelsBackend::with_replay_capacity(16, 4);
        for i in 0..10 {
            backend.publish_local("topic", ChannelMessage::from(format!("m{i}")));
        }
        let state = backend.get_or_create_topic("topic");
        let (len, ids, next_id) = {
            let replay = state.replay.lock().expect("replay lock");
            let ids: Vec<u64> = replay.buf.iter().map(|(id, _)| *id).collect();
            (replay.buf.len(), ids, replay.next_id)
        };
        // Buffer never exceeds N.
        assert_eq!(len, 4);
        // Retains the most recent N (ids 7..=10), ascending.
        assert_eq!(ids, vec![7, 8, 9, 10]);
        // Next id keeps climbing densely.
        assert_eq!(next_id, 11);
    }

    #[test]
    fn resume_cold_connection_has_no_replay_or_gap() {
        let backend = LocalChannelsBackend::with_replay_capacity(16, 8);
        backend.publish_local("t", ChannelMessage::from("a"));
        backend.publish_local("t", ChannelMessage::from("b"));

        let handle = backend.resume("t", None);
        assert!(handle.replay.is_empty(), "cold connect must not replay");
        assert!(!handle.gap);
        assert!(handle.resumable);
        // Two messages published → next live id is 3.
        assert_eq!(handle.next_live_id, 3);
    }

    #[test]
    fn resume_replays_only_newer_than_last_event_id() {
        let backend = LocalChannelsBackend::with_replay_capacity(16, 8);
        for i in 1..=5 {
            backend.publish_local("t", ChannelMessage::from(format!("m{i}")));
        }
        let handle = backend.resume("t", Some(2));
        let ids: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        assert!(!handle.gap, "everything since id 2 is retained");
        assert_eq!(handle.next_live_id, 6);
    }

    #[test]
    fn resume_signals_gap_when_last_event_id_aged_out() {
        // cap 3: after publishing 5, retained ids are 3,4,5.
        let backend = LocalChannelsBackend::with_replay_capacity(16, 3);
        for i in 1..=5 {
            backend.publish_local("t", ChannelMessage::from(format!("m{i}")));
        }
        // Client last saw id 1; id 2 has aged out → gap, replay starts at oldest.
        let handle = backend.resume("t", Some(1));
        assert!(handle.gap, "aged-out resume point must signal a gap");
        let ids: Vec<u64> = handle.replay.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn resume_no_gap_when_last_event_id_is_current_tail() {
        let backend = LocalChannelsBackend::with_replay_capacity(16, 3);
        for i in 1..=5 {
            backend.publish_local("t", ChannelMessage::from(format!("m{i}")));
        }
        // Client already saw the latest id (5): nothing to replay, no gap.
        let handle = backend.resume("t", Some(5));
        assert!(handle.replay.is_empty());
        assert!(!handle.gap);
        assert_eq!(handle.next_live_id, 6);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn resume_seam_delivers_replay_then_live_without_gap()
    -> Result<(), broadcast::error::RecvError> {
        let backend = LocalChannelsBackend::with_replay_capacity(16, 8);
        for i in 1..=3 {
            backend.publish_local("t", ChannelMessage::from(format!("m{i}")));
        }
        // Resume from id 1: replay 2,3 then live 4,5.
        let mut handle = backend.resume("t", Some(1));
        assert_eq!(
            handle.replay.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(handle.next_live_id, 4);

        backend.publish_local("t", ChannelMessage::from("m4"));
        backend.publish_local("t", ChannelMessage::from("m5"));

        assert_eq!(handle.subscriber.recv().await?.as_str(), "m4");
        assert_eq!(handle.subscriber.recv().await?.as_str(), "m5");
        Ok(())
    }

    #[test]
    fn resume_via_default_backend_is_live_only() {
        // The trait default (used by Redis) is live-only, non-resumable.
        struct LiveOnly(LocalChannelsBackend);
        impl ChannelsBackend for LiveOnly {
            fn publish(
                &self,
                topic: &str,
                msg: ChannelMessage,
            ) -> Result<usize, ChannelPublishError> {
                self.0.publish(topic, msg)
            }
            fn ensure_topic(&self, topic: &str) -> Arc<broadcast::Sender<ChannelMessage>> {
                self.0.ensure_topic(topic)
            }
            fn subscribe(&self, topic: &str) -> Subscriber {
                self.0.subscribe(topic)
            }
            fn channel_count(&self) -> usize {
                self.0.channel_count()
            }
            fn gc(&self) {
                self.0.gc();
            }
            fn snapshot(&self) -> HashMap<String, ChannelStats> {
                self.0.snapshot()
            }
        }

        let backend = LiveOnly(LocalChannelsBackend::new(16));
        backend.publish("t", ChannelMessage::from("a")).unwrap();
        let handle = backend.resume("t", Some(0));
        assert!(handle.replay.is_empty());
        assert!(!handle.gap);
        assert!(!handle.resumable, "default resume is live-only");
        assert_eq!(handle.next_live_id, 1);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn inject_oob_attr_fallback_no_lt() {
        let result = inject_oob_attr("no-tags-here", "true");
        assert_eq!(
            result, "no-tags-here",
            "fallback must return html unchanged"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn inject_oob_attr_fallback_no_boundary() {
        let result = inject_oob_attr("<", "true");
        assert_eq!(result, "<", "fallback must return html unchanged");
    }
}
