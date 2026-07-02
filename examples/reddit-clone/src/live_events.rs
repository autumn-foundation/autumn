//! Durable live-feed events shared across web and worker processes.
//!
//! WebSocket subscribers still consume in-process Autumn channels, but those
//! channels are now fed from a durable app-database event log so separate web
//! and worker processes see the same activity stream. The relay can wake on
//! Postgres `LISTEN/NOTIFY` or Redis pub/sub; Redis deployments also keep a
//! Postgres wake safety-net so missed broker publishes do not degrade to slow
//! polling.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::AppState;
use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::error::AutumnError;
use autumn_web::plugin::Plugin;
use chrono::{NaiveDateTime, Utc};
use diesel::ExpressionMethods;
use diesel::QueryDsl;
use diesel::SelectableHelper;
use diesel::dsl::max;
use diesel::sql_types::Text;
use diesel_async::AsyncConnection;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use futures::StreamExt;
use redis::AsyncCommands;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::live_bus::{LiveFeedBusConfig, LiveFeedBusKind};

const LIVE_EVENT_RELAY_BATCH_SIZE: i64 = 128;
const LIVE_EVENT_POLL_INTERVAL_MS: u64 = 250;
const LIVE_EVENT_RECONNECT_INTERVAL_MS: u64 = 250;
const LIVE_EVENT_RETENTION_DAYS: i64 = 7;
const LIVE_EVENT_NOTIFY_QUEUE: &str = "reddit_live_feed";

fn live_event_notify_channel(queue_name: &str) -> String {
    format!("autumn_live_event_{queue_name}")
}

fn quote_pg_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

diesel::table! {
    live_feed_events (id) {
        id -> Int8,
        subreddit_slug -> Text,
        event -> Jsonb,
        created_at -> Timestamp,
    }
}

#[allow(dead_code)] // Row mirrors table state across relay queries and retention logic.
#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable)]
#[diesel(table_name = live_feed_events)]
struct LiveFeedEventRow {
    id: i64,
    subreddit_slug: String,
    event: Value,
    created_at: NaiveDateTime,
}

#[derive(diesel::Insertable)]
#[diesel(table_name = live_feed_events)]
struct NewLiveFeedEventRow<'a> {
    subreddit_slug: &'a str,
    event: Value,
}

#[derive(Clone)]
struct LiveFeedRelayOptions {
    database_url: Option<String>,
    bus: LiveFeedBusConfig,
    poll_interval: Duration,
    reconnect_interval: Duration,
    connector: Arc<dyn LiveEventListenerConnector>,
}

impl LiveFeedRelayOptions {
    fn from_config(config: &AutumnConfig, bus: LiveFeedBusConfig) -> Self {
        Self {
            database_url: config.database.url.clone(),
            bus,
            poll_interval: Duration::from_millis(LIVE_EVENT_POLL_INTERVAL_MS),
            reconnect_interval: Duration::from_millis(LIVE_EVENT_RECONNECT_INTERVAL_MS),
            connector: Arc::new(DefaultLiveEventListenerConnector),
        }
    }
}

impl std::fmt::Debug for LiveFeedRelayOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveFeedRelayOptions")
            .field(
                "database_url",
                &self.database_url.as_deref().map(|_| "<configured>"),
            )
            .field("bus", &self.bus)
            .field("poll_interval", &self.poll_interval)
            .field("reconnect_interval", &self.reconnect_interval)
            .finish()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LiveEventBusMessage {
    event_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum LiveFeedWakeSource {
    Redis,
    Postgres,
    PollFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveFeedWakeOutcome {
    Wake(LiveFeedWakeSource),
    TimedOut,
    ListenerClosed,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveFeedRelayHealthSnapshot {
    pub bus_kind: String,
    pub listener_state: String,
    pub reconnect_attempts: u64,
    pub reconnect_successes: u64,
    pub reconnect_failures: u64,
    pub publish_successes: u64,
    pub publish_failures: u64,
    pub wake_redis: u64,
    pub wake_postgres: u64,
    pub wake_poll: u64,
    pub replayed_events: u64,
    pub last_seen_id: i64,
    pub last_replayed_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
struct LiveFeedRelayHealth {
    inner: Arc<LiveFeedRelayHealthInner>,
}

struct LiveFeedRelayHealthInner {
    bus_kind: String,
    listener_state: RwLock<String>,
    reconnect_attempts: AtomicU64,
    reconnect_successes: AtomicU64,
    reconnect_failures: AtomicU64,
    publish_successes: AtomicU64,
    publish_failures: AtomicU64,
    wake_redis: AtomicU64,
    wake_postgres: AtomicU64,
    wake_poll: AtomicU64,
    replayed_events: AtomicU64,
    last_seen_id: AtomicI64,
    last_replayed_at: RwLock<Option<String>>,
    last_error: RwLock<Option<String>>,
}

impl LiveFeedRelayHealth {
    fn new(bus: &LiveFeedBusConfig) -> Self {
        Self {
            inner: Arc::new(LiveFeedRelayHealthInner {
                bus_kind: match bus.kind {
                    LiveFeedBusKind::PostgresNotify => "postgres_notify".to_owned(),
                    LiveFeedBusKind::RedisPubSub => "redis_pubsub".to_owned(),
                },
                listener_state: RwLock::new("initializing".to_owned()),
                reconnect_attempts: AtomicU64::new(0),
                reconnect_successes: AtomicU64::new(0),
                reconnect_failures: AtomicU64::new(0),
                publish_successes: AtomicU64::new(0),
                publish_failures: AtomicU64::new(0),
                wake_redis: AtomicU64::new(0),
                wake_postgres: AtomicU64::new(0),
                wake_poll: AtomicU64::new(0),
                replayed_events: AtomicU64::new(0),
                last_seen_id: AtomicI64::new(0),
                last_replayed_at: RwLock::new(None),
                last_error: RwLock::new(None),
            }),
        }
    }

    fn snapshot(&self) -> LiveFeedRelayHealthSnapshot {
        LiveFeedRelayHealthSnapshot {
            bus_kind: self.inner.bus_kind.clone(),
            listener_state: self
                .inner
                .listener_state
                .read()
                .map(|state| state.clone())
                .unwrap_or_else(|_| "poisoned".to_owned()),
            reconnect_attempts: AtomicU64::load(&self.inner.reconnect_attempts, Ordering::Relaxed),
            reconnect_successes: AtomicU64::load(
                &self.inner.reconnect_successes,
                Ordering::Relaxed,
            ),
            reconnect_failures: AtomicU64::load(&self.inner.reconnect_failures, Ordering::Relaxed),
            publish_successes: AtomicU64::load(&self.inner.publish_successes, Ordering::Relaxed),
            publish_failures: AtomicU64::load(&self.inner.publish_failures, Ordering::Relaxed),
            wake_redis: AtomicU64::load(&self.inner.wake_redis, Ordering::Relaxed),
            wake_postgres: AtomicU64::load(&self.inner.wake_postgres, Ordering::Relaxed),
            wake_poll: AtomicU64::load(&self.inner.wake_poll, Ordering::Relaxed),
            replayed_events: AtomicU64::load(&self.inner.replayed_events, Ordering::Relaxed),
            last_seen_id: AtomicI64::load(&self.inner.last_seen_id, Ordering::Relaxed),
            last_replayed_at: self
                .inner
                .last_replayed_at
                .read()
                .map(|ts| ts.clone())
                .unwrap_or(None),
            last_error: self
                .inner
                .last_error
                .read()
                .map(|error| error.clone())
                .unwrap_or_else(|_| Some("relay health lock poisoned".to_owned())),
        }
    }

    fn set_listener_state(&self, state: impl Into<String>) {
        if let Ok(mut listener_state) = self.inner.listener_state.write() {
            *listener_state = state.into();
        }
    }

    fn clear_error(&self) {
        if let Ok(mut last_error) = self.inner.last_error.write() {
            *last_error = None;
        }
    }

    fn set_error(&self, error: impl Into<String>) {
        if let Ok(mut last_error) = self.inner.last_error.write() {
            *last_error = Some(error.into());
        }
    }

    fn record_publish_success(&self) {
        self.inner.publish_successes.fetch_add(1, Ordering::Relaxed);
        self.clear_error();
    }

    fn record_publish_failure(&self, error: &AutumnError) {
        self.inner.publish_failures.fetch_add(1, Ordering::Relaxed);
        self.set_error(error.to_string());
    }

    fn record_reconnect_attempt(&self) {
        self.inner
            .reconnect_attempts
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_reconnect_success(&self, listener_state: &str) {
        self.inner
            .reconnect_successes
            .fetch_add(1, Ordering::Relaxed);
        self.set_listener_state(listener_state);
        self.clear_error();
    }

    fn record_reconnect_failure(&self, error: impl Into<String>) {
        self.inner
            .reconnect_failures
            .fetch_add(1, Ordering::Relaxed);
        self.set_listener_state("polling");
        self.set_error(error);
    }

    fn record_wake(&self, source: LiveFeedWakeSource) {
        match source {
            LiveFeedWakeSource::Redis => {
                self.inner.wake_redis.fetch_add(1, Ordering::Relaxed);
            }
            LiveFeedWakeSource::Postgres => {
                self.inner.wake_postgres.fetch_add(1, Ordering::Relaxed);
            }
            LiveFeedWakeSource::PollFallback => {
                self.inner.wake_poll.fetch_add(1, Ordering::Relaxed);
            }
        };
    }

    fn record_replay(&self, cursor: i64, replayed: usize, last_created_at: Option<NaiveDateTime>) {
        if replayed > 0 {
            self.inner
                .replayed_events
                .fetch_add(replayed as u64, Ordering::Relaxed);
            self.inner.last_seen_id.store(cursor, Ordering::Relaxed);
            if let Some(created_at) = last_created_at
                && let Ok(mut last_replayed_at) = self.inner.last_replayed_at.write()
            {
                *last_replayed_at = Some(
                    chrono::DateTime::<Utc>::from_naive_utc_and_offset(created_at, Utc)
                        .to_rfc3339(),
                );
            }
        }
    }
}

#[derive(Clone)]
struct LiveEventBusPublisher {
    inner: LiveEventBusPublisherInner,
    health: Arc<LiveFeedRelayHealth>,
}

#[derive(Clone)]
enum LiveEventBusPublisherInner {
    PostgresNotify {
        channel: String,
    },
    RedisPubSub {
        channel: String,
        client: redis::Client,
    },
}

struct LiveEventBusListener {
    label: String,
    redis: Option<redis::aio::PubSub>,
    postgres: Option<PostgresLiveEventListener>,
    #[cfg(test)]
    test_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LiveFeedWakeSource>>,
}

impl LiveEventBusListener {
    fn from_parts(
        redis: Option<redis::aio::PubSub>,
        postgres: Option<PostgresLiveEventListener>,
    ) -> Option<Self> {
        if redis.is_none() && postgres.is_none() {
            None
        } else {
            let label = match (redis.is_some(), postgres.is_some()) {
                (true, true) => "redis+postgres",
                (true, false) => "redis",
                (false, true) => "postgres",
                (false, false) => "polling",
            };
            Some(Self {
                label: label.to_owned(),
                redis,
                postgres,
                #[cfg(test)]
                test_rx: None,
            })
        }
    }

    #[cfg(test)]
    fn test(
        label: impl Into<String>,
        test_rx: tokio::sync::mpsc::UnboundedReceiver<LiveFeedWakeSource>,
    ) -> Self {
        Self {
            label: label.into(),
            redis: None,
            postgres: None,
            test_rx: Some(test_rx),
        }
    }

    fn label(&self) -> &str {
        &self.label
    }
}

struct PostgresLiveEventListener {
    conn: AsyncPgConnection,
}

impl PostgresLiveEventListener {
    async fn connect(database_url: &str, channel: &str) -> AutumnResult<Self> {
        let mut conn = AsyncPgConnection::establish(database_url)
            .await
            .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
        let listen = format!("LISTEN {}", quote_pg_identifier(channel));
        diesel::sql_query(listen).execute(&mut conn).await?;
        Ok(Self { conn })
    }
}

impl LiveEventBusPublisher {
    fn from_config(
        config: &LiveFeedBusConfig,
        health: Arc<LiveFeedRelayHealth>,
    ) -> AutumnResult<Self> {
        let inner = match config.kind {
            LiveFeedBusKind::PostgresNotify => LiveEventBusPublisherInner::PostgresNotify {
                channel: config.channel.clone(),
            },
            LiveFeedBusKind::RedisPubSub => {
                let redis_url = config.redis_url.as_deref().ok_or_else(|| {
                    AutumnError::service_unavailable_msg(
                        "distributed.live_feed_bus.redis_url is required when kind = redis_pubsub",
                    )
                })?;
                let client = redis::Client::open(redis_url)
                    .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
                LiveEventBusPublisherInner::RedisPubSub {
                    channel: config.channel.clone(),
                    client,
                }
            }
        };

        Ok(Self { inner, health })
    }

    fn postgres_notify_channel(&self) -> String {
        let channel = match &self.inner {
            LiveEventBusPublisherInner::PostgresNotify { channel }
            | LiveEventBusPublisherInner::RedisPubSub { channel, .. } => channel,
        };
        live_event_notify_channel(channel)
    }

    async fn publish(&self, event_id: i64) -> AutumnResult<()> {
        let result = match &self.inner {
            LiveEventBusPublisherInner::PostgresNotify { .. } => Ok(()),
            LiveEventBusPublisherInner::RedisPubSub { channel, client } => {
                let payload = serde_json::to_string(&LiveEventBusMessage { event_id })
                    .expect("live-event bus payload should serialize");
                match client.get_multiplexed_async_connection().await {
                    Ok(mut conn) => match conn.publish(channel.as_str(), payload).await {
                        Ok::<usize, redis::RedisError>(_published) => Ok(()),
                        Err(error) => Err(AutumnError::service_unavailable_msg(error.to_string())),
                    },
                    Err(error) => Err(AutumnError::service_unavailable_msg(error.to_string())),
                }
            }
        };

        match &result {
            Ok(()) => self.health.record_publish_success(),
            Err(error) => self.health.record_publish_failure(error),
        }

        let _ = event_id;
        result
    }
}

fn live_event_notify_channel_for_state(state: &AppState) -> String {
    state
        .extension::<LiveEventBusPublisher>()
        .map(|publisher| publisher.postgres_notify_channel())
        .unwrap_or_else(|| live_event_notify_channel(LIVE_EVENT_NOTIFY_QUEUE))
}

type LiveEventConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Option<LiveEventBusListener>> + Send + 'a>>;

trait LiveEventListenerConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        database_url: Option<&'a str>,
        bus: &'a LiveFeedBusConfig,
    ) -> LiveEventConnectFuture<'a>;
}

struct DefaultLiveEventListenerConnector;

impl LiveEventListenerConnector for DefaultLiveEventListenerConnector {
    fn connect<'a>(
        &'a self,
        database_url: Option<&'a str>,
        bus: &'a LiveFeedBusConfig,
    ) -> LiveEventConnectFuture<'a> {
        Box::pin(connect_live_event_listener(database_url, bus))
    }
}

#[must_use]
pub fn live_feed_relay_health_snapshot(state: &AppState) -> Option<LiveFeedRelayHealthSnapshot> {
    state
        .extension::<LiveFeedRelayHealth>()
        .map(|health| health.snapshot())
}

fn ensure_live_feed_relay_health(
    state: &AppState,
    bus: &LiveFeedBusConfig,
) -> Arc<LiveFeedRelayHealth> {
    if let Some(health) = state.extension::<LiveFeedRelayHealth>() {
        health
    } else {
        let health = LiveFeedRelayHealth::new(bus);
        state.insert_extension(health.clone());
        state
            .extension::<LiveFeedRelayHealth>()
            .expect("live-feed relay health should be installed after insertion")
    }
}

/// Plugin that spawns the durable live-feed relay during app startup.
///
/// Serves as an in-tree example of the [`Plugin`] trait for app-owned runtime
/// infrastructure.
#[derive(Default)]
pub struct LiveFeedPlugin;

impl LiveFeedPlugin {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for LiveFeedPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.on_startup(start_live_event_relay)
    }
}

pub async fn install_live_event_bus(state: &AppState) -> AutumnResult<()> {
    let config = LiveFeedBusConfig::load()
        .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
    install_live_event_bus_with_config(state, config).await
}

async fn install_live_event_bus_with_config(
    state: &AppState,
    config: LiveFeedBusConfig,
) -> AutumnResult<()> {
    let health = ensure_live_feed_relay_health(state, &config);
    state.insert_extension(LiveEventBusPublisher::from_config(&config, health)?);
    Ok(())
}

pub async fn publish_stored_live_event(state: &AppState, event_id: i64) -> AutumnResult<()> {
    let publisher = state.extension::<LiveEventBusPublisher>().ok_or_else(|| {
        AutumnError::service_unavailable_msg("reddit-clone live-event bus is not installed")
    })?;
    publisher.publish(event_id).await
}

pub async fn publish_stored_live_event_best_effort(state: &AppState, event_id: i64) {
    if let Err(error) = publish_stored_live_event(state, event_id).await {
        warn!(
            error = %error,
            event_id,
            "failed to publish reddit-clone live event to the configured bus"
        );
    }
}

pub async fn start_live_event_relay(state: AppState) -> AutumnResult<()> {
    if state.pool().is_none() {
        return Err(AutumnError::service_unavailable_msg(
            "reddit-clone live feed relay requires database.url",
        ));
    }

    let config = AutumnConfig::load()
        .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
    let bus = LiveFeedBusConfig::load()
        .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
    install_live_event_bus_with_config(&state, bus.clone()).await?;

    drop(
        start_live_event_relay_with_options(state, LiveFeedRelayOptions::from_config(&config, bus))
            .await?,
    );
    Ok(())
}

async fn start_live_event_relay_with_options(
    state: AppState,
    options: LiveFeedRelayOptions,
) -> AutumnResult<JoinHandle<()>> {
    let pool = state.pool().cloned().ok_or_else(|| {
        AutumnError::service_unavailable_msg("reddit-clone live feed relay requires database.url")
    })?;
    let initial_cursor = load_current_live_event_cursor(&pool).await?;
    let health = ensure_live_feed_relay_health(&state, &options.bus);
    let listener = options
        .connector
        .connect(options.database_url.as_deref(), &options.bus)
        .await;
    health.set_listener_state(listener.as_ref().map_or_else(
        || "polling".to_owned(),
        |listener| listener.label().to_owned(),
    ));
    Ok(spawn_live_event_relay_task(
        state,
        options,
        listener,
        initial_cursor,
        health,
    ))
}

async fn connect_live_event_listener(
    database_url: Option<&str>,
    bus: &LiveFeedBusConfig,
) -> Option<LiveEventBusListener> {
    match bus.kind {
        LiveFeedBusKind::PostgresNotify => LiveEventBusListener::from_parts(
            None,
            connect_postgres_live_event_listener(database_url, &bus.channel).await,
        ),
        LiveFeedBusKind::RedisPubSub => {
            let redis =
                connect_redis_live_event_listener(bus.redis_url.as_deref(), &bus.channel).await;
            let postgres = connect_postgres_live_event_listener(database_url, &bus.channel).await;
            if postgres.is_some() {
                debug!(
                    channel = %live_event_notify_channel(&bus.channel),
                    "reddit-clone live-feed Postgres backup listener connected for Redis bus"
                );
            }
            LiveEventBusListener::from_parts(redis, postgres)
        }
    }
}

async fn connect_postgres_live_event_listener(
    database_url: Option<&str>,
    channel: &str,
) -> Option<PostgresLiveEventListener> {
    match database_url {
        Some(database_url) => {
            let channel = live_event_notify_channel(channel);
            match PostgresLiveEventListener::connect(database_url, &channel).await {
                Ok(listener) => {
                    debug!(
                        channel = %channel,
                        "reddit-clone live-feed Postgres listener connected"
                    );
                    Some(listener)
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        channel = %channel,
                        "failed to start reddit-clone live-feed Postgres listener; falling back to polling"
                    );
                    None
                }
            }
        }
        None => None,
    }
}

async fn subscribe_redis_pubsub(
    mut pubsub: redis::aio::PubSub,
    channel: &str,
) -> Option<redis::aio::PubSub> {
    match pubsub.subscribe(channel).await {
        Ok(()) => {
            debug!(
                channel = %channel,
                "reddit-clone live-feed Redis listener connected"
            );
            Some(pubsub)
        }
        Err(error) => {
            warn!(
                error = %error,
                channel = %channel,
                "failed to subscribe reddit-clone live-feed Redis listener; falling back to polling"
            );
            None
        }
    }
}

async fn setup_redis_pubsub(client: redis::Client, channel: &str) -> Option<redis::aio::PubSub> {
    match client.get_async_pubsub().await {
        Ok(pubsub) => subscribe_redis_pubsub(pubsub, channel).await,
        Err(error) => {
            warn!(
                error = %error,
                "failed to connect reddit-clone live-feed Redis pubsub listener; falling back to polling"
            );
            None
        }
    }
}

async fn connect_redis_live_event_listener(
    redis_url: Option<&str>,
    channel: &str,
) -> Option<redis::aio::PubSub> {
    let Some(redis_url) = redis_url else {
        warn!("distributed.live_feed_bus.redis_url is missing; falling back to polling");
        return None;
    };
    match redis::Client::open(redis_url) {
        Ok(client) => setup_redis_pubsub(client, channel).await,
        Err(error) => {
            warn!(
                error = %error,
                "failed to construct reddit-clone Redis client; falling back to polling"
            );
            None
        }
    }
}

fn spawn_live_event_relay_task(
    state: AppState,
    options: LiveFeedRelayOptions,
    mut listener: Option<LiveEventBusListener>,
    mut last_seen_id: i64,
    health: Arc<LiveFeedRelayHealth>,
) -> JoinHandle<()> {
    let shutdown = state.shutdown_token();
    let poll_interval = options.poll_interval;
    let reconnect_interval = options.reconnect_interval;

    tokio::spawn(async move {
        let mut next_reconnect_at = tokio::time::Instant::now().checked_add(reconnect_interval).unwrap_or_else(tokio::time::Instant::now);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    debug!(last_seen_id, "reddit-clone live-feed relay shutting down");
                    break;
                }
                _ = tokio::time::sleep_until(next_reconnect_at), if listener.is_none() => {
                    health.record_reconnect_attempt();
                    match options
                        .connector
                        .connect(options.database_url.as_deref(), &options.bus)
                        .await
                    {
                        Some(new_listener) => {
                            health.record_reconnect_success(new_listener.label());
                            listener = Some(new_listener);
                        }
                        None => {
                            health.record_reconnect_failure(
                                "failed to reconnect reddit-clone live-feed listener; using poll fallback",
                            );
                        }
                    }
                    next_reconnect_at = tokio::time::Instant::now().checked_add(reconnect_interval).unwrap_or_else(tokio::time::Instant::now);
                }
                wake = wait_for_live_event_wakeup(listener.as_mut(), poll_interval) => {
                    match wake {
                        Ok(LiveFeedWakeOutcome::Wake(source)) => {
                            health.record_wake(source);
                            debug!(?source, "reddit-clone live-feed relay woke on the configured bus");
                        }
                        Ok(LiveFeedWakeOutcome::TimedOut) => {
                            if listener.is_none() {
                                health.record_wake(LiveFeedWakeSource::PollFallback);
                            }
                        }
                        Ok(LiveFeedWakeOutcome::ListenerClosed) => {
                            warn!("reddit-clone live-feed relay listener closed; attempting reconnect");
                            health.record_reconnect_failure(
                                "live-feed listener closed; retrying configured wake path",
                            );
                            listener = None;
                            next_reconnect_at = tokio::time::Instant::now();
                        }
                        Err(error) => {
                            warn!(
                                error = %error,
                                "reddit-clone live-feed relay listener failed; falling back to polling"
                            );
                            health.record_reconnect_failure(error.to_string());
                            listener = None;
                            next_reconnect_at = tokio::time::Instant::now();
                        }
                    }
                    match rebroadcast_pending_live_events(&state, last_seen_id).await {
                        Ok(progress) => {
                            if progress.cursor > last_seen_id {
                                debug!(
                                    previous_cursor = last_seen_id,
                                    cursor = progress.cursor,
                                    "reddit-clone live-feed relay broadcast pending events"
                                );
                                last_seen_id = progress.cursor;
                            }
                            health.record_replay(progress.cursor, progress.replayed, progress.last_created_at);
                        }
                        Err(error) => {
                            warn!(
                                error = %error,
                                last_seen_id,
                                "reddit-clone live-feed relay failed to rebroadcast events"
                            );
                            health.set_error(error.to_string());
                        }
                    }
                }
            }
        }
    })
}

pub async fn store_activity_event(
    conn: &mut AsyncPgConnection,
    subreddit_slug: &str,
    event: &Value,
) -> Result<i64, diesel::result::Error> {
    store_activity_event_on_channel(
        conn,
        subreddit_slug,
        event,
        &live_event_notify_channel(LIVE_EVENT_NOTIFY_QUEUE),
    )
    .await
}

pub async fn store_activity_event_for_state(
    state: &AppState,
    conn: &mut AsyncPgConnection,
    subreddit_slug: &str,
    event: &Value,
) -> Result<i64, diesel::result::Error> {
    store_activity_event_on_channel(
        conn,
        subreddit_slug,
        event,
        &live_event_notify_channel_for_state(state),
    )
    .await
}

async fn store_activity_event_on_channel(
    conn: &mut AsyncPgConnection,
    subreddit_slug: &str,
    event: &Value,
    notify_channel: &str,
) -> Result<i64, diesel::result::Error> {
    let event_id: i64 = diesel::insert_into(live_feed_events::table)
        .values(NewLiveFeedEventRow {
            subreddit_slug,
            event: event.clone(),
        })
        .returning(live_feed_events::id)
        .get_result(conn)
        .await?;
    notify_live_event_stored(conn, event_id, notify_channel).await?;

    Ok(event_id)
}

pub async fn prune_live_feed_events(state: AppState) -> AutumnResult<()> {
    let pool = state.pool().cloned().ok_or_else(|| {
        AutumnError::service_unavailable_msg("reddit-clone live-feed pruning requires database.url")
    })?;
    let mut conn = pool
        .get()
        .await
        .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;

    let deleted = diesel::sql_query(
        "DELETE FROM live_feed_events \
         WHERE created_at < NOW() - ($1 * INTERVAL '1 day')",
    )
    .bind::<diesel::sql_types::BigInt, _>(LIVE_EVENT_RETENTION_DAYS)
    .execute(&mut conn)
    .await?;

    if deleted > 0 {
        debug!(deleted, "pruned expired reddit-clone live-feed events");
    }

    Ok(())
}

#[must_use]
pub fn post_created_event(
    post_id: i64,
    title: &str,
    post_slug: &str,
    subreddit_slug: &str,
    author_username: &str,
) -> Value {
    json!({
        "type": "post_created",
        "post_id": post_id,
        "title": title,
        "post_slug": post_slug,
        "subreddit_slug": subreddit_slug,
        "author_username": author_username,
        "path": crate::routes::posts::__autumn_path_show(subreddit_slug, post_slug),
    })
}

#[must_use]
pub fn comment_created_event(
    comment_id: i64,
    post_id: i64,
    post_slug: &str,
    subreddit_slug: &str,
    author_username: &str,
    body: &str,
) -> Value {
    json!({
        "type": "comment_created",
        "comment_id": comment_id,
        "post_id": post_id,
        "post_slug": post_slug,
        "subreddit_slug": subreddit_slug,
        "author_username": author_username,
        "body_preview": comment_body_preview(body),
        "path": format!("{}#comment-{comment_id}", crate::routes::posts::__autumn_path_show(subreddit_slug, post_slug)),
    })
}

type AutumnResult<T> = Result<T, AutumnError>;

async fn load_current_live_event_cursor(
    pool: &diesel_async::pooled_connection::deadpool::Pool<AsyncPgConnection>,
) -> AutumnResult<i64> {
    let mut conn = pool
        .get()
        .await
        .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;

    Ok(live_feed_events::table
        .select(max(live_feed_events::id))
        .get_result::<Option<i64>>(&mut conn)
        .await?
        .unwrap_or(0))
}

async fn notify_live_event_stored(
    conn: &mut AsyncPgConnection,
    event_id: i64,
    channel: &str,
) -> Result<(), diesel::result::Error> {
    let payload = serde_json::to_string(&LiveEventBusMessage { event_id })
        .expect("live-feed notify payload should serialize");

    diesel::sql_query("SELECT pg_notify($1, $2)")
        .bind::<Text, _>(channel)
        .bind::<Text, _>(payload)
        .execute(conn)
        .await?;

    Ok(())
}

struct RebroadcastProgress {
    cursor: i64,
    replayed: usize,
    last_created_at: Option<NaiveDateTime>,
}

async fn rebroadcast_pending_live_events(
    state: &AppState,
    after_id: i64,
) -> AutumnResult<RebroadcastProgress> {
    let pool = state.pool().cloned().ok_or_else(|| {
        AutumnError::service_unavailable_msg("reddit-clone live-feed relay requires database.url")
    })?;
    let mut conn = pool
        .get()
        .await
        .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
    let mut cursor = after_id;
    let mut replayed = 0usize;
    let mut last_created_at = None;

    loop {
        let rows: Vec<LiveFeedEventRow> = live_feed_events::table
            .filter(live_feed_events::id.gt(cursor))
            .order(live_feed_events::id.asc())
            .limit(LIVE_EVENT_RELAY_BATCH_SIZE)
            .select(LiveFeedEventRow::as_select())
            .load(&mut conn)
            .await?;

        if rows.is_empty() {
            return Ok(RebroadcastProgress {
                cursor,
                replayed,
                last_created_at,
            });
        }

        for row in &rows {
            rebroadcast_row(state, row);
            cursor = row.id;
            replayed += 1;
            last_created_at = Some(row.created_at);
        }

        if rows.len() < LIVE_EVENT_RELAY_BATCH_SIZE as usize {
            return Ok(RebroadcastProgress {
                cursor,
                replayed,
                last_created_at,
            });
        }
    }
}

async fn wait_for_live_event_wakeup(
    listener: Option<&mut LiveEventBusListener>,
    poll_interval: Duration,
) -> AutumnResult<LiveFeedWakeOutcome> {
    match listener {
        Some(listener) => {
            #[cfg(test)]
            if let Some(test_rx) = listener.test_rx.as_mut() {
                return wait_for_test_wake(test_rx, poll_interval).await;
            }

            match (listener.redis.as_mut(), listener.postgres.as_mut()) {
                (Some(redis), Some(postgres)) => {
                    tokio::select! {
                        wake = wait_for_redis_message(redis, poll_interval) => wake,
                        wake = wait_for_postgres_notification(postgres, poll_interval) => wake,
                    }
                }
                (Some(redis), None) => wait_for_redis_message(redis, poll_interval).await,
                (None, Some(postgres)) => {
                    wait_for_postgres_notification(postgres, poll_interval).await
                }
                (None, None) => {
                    tokio::time::sleep(poll_interval).await;
                    Ok(LiveFeedWakeOutcome::TimedOut)
                }
            }
        }
        None => {
            tokio::time::sleep(poll_interval).await;
            Ok(LiveFeedWakeOutcome::TimedOut)
        }
    }
}

async fn wait_for_postgres_notification(
    listener: &mut PostgresLiveEventListener,
    poll_interval: Duration,
) -> AutumnResult<LiveFeedWakeOutcome> {
    let mut notifications = std::pin::pin!(listener.conn.notifications_stream());
    match tokio::time::timeout(poll_interval, notifications.next()).await {
        Ok(Some(Ok(_notification))) => Ok(LiveFeedWakeOutcome::Wake(LiveFeedWakeSource::Postgres)),
        Ok(Some(Err(error))) => Err(AutumnError::service_unavailable_msg(error.to_string())),
        Ok(None) => Ok(LiveFeedWakeOutcome::ListenerClosed),
        Err(_elapsed) => Ok(LiveFeedWakeOutcome::TimedOut),
    }
}

async fn wait_for_redis_message(
    listener: &mut redis::aio::PubSub,
    poll_interval: Duration,
) -> AutumnResult<LiveFeedWakeOutcome> {
    let mut stream = listener.on_message();
    match tokio::time::timeout(poll_interval, stream.next()).await {
        Ok(Some(message)) => {
            let payload: String = message
                .get_payload()
                .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
            let _message: LiveEventBusMessage = serde_json::from_str(&payload)
                .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))?;
            Ok(LiveFeedWakeOutcome::Wake(LiveFeedWakeSource::Redis))
        }
        Ok(None) => Ok(LiveFeedWakeOutcome::ListenerClosed),
        Err(_elapsed) => Ok(LiveFeedWakeOutcome::TimedOut),
    }
}

#[cfg(test)]
async fn wait_for_test_wake(
    listener: &mut tokio::sync::mpsc::UnboundedReceiver<LiveFeedWakeSource>,
    poll_interval: Duration,
) -> AutumnResult<LiveFeedWakeOutcome> {
    match tokio::time::timeout(poll_interval, listener.recv()).await {
        Ok(Some(source)) => Ok(LiveFeedWakeOutcome::Wake(source)),
        Ok(None) => Ok(LiveFeedWakeOutcome::ListenerClosed),
        Err(_elapsed) => Ok(LiveFeedWakeOutcome::TimedOut),
    }
}

fn rebroadcast_row(state: &AppState, row: &LiveFeedEventRow) {
    let payload = row.event.to_string();
    let channels = state.channels();

    channels.sender("feed").send(payload.as_str()).ok();
    channels
        .sender(&format!("r/{}", row.subreddit_slug))
        .send(payload)
        .ok();
}

/// ⚡ Bolt Optimization:
/// Avoids an intermediate `Vec<&str>` heap allocation and `join` overhead
/// by manually building the collapsed string into a pre-allocated buffer.
fn comment_body_preview(body: &str) -> String {
    const MAX_PREVIEW_LEN: usize = 120;

    let mut collapsed = String::with_capacity(body.len());
    for word in body.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }

    if collapsed.len() <= MAX_PREVIEW_LEN {
        return collapsed;
    }

    let mut preview = collapsed
        .chars()
        .take(MAX_PREVIEW_LEN.saturating_sub(3))
        .collect::<String>();
    preview.push_str("...");
    preview
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    use crate::live_bus::{LiveFeedBusConfig, LiveFeedBusKind};
    use autumn_web::config::DatabaseConfig;
    use autumn_web::db;
    use diesel::QueryableByName;
    use diesel_async::SimpleAsyncConnection;
    use testcontainers::ContainerAsync;
    use testcontainers_modules::postgres::Postgres;
    use testcontainers_modules::redis::{REDIS_PORT, Redis};
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

    const REDDIT_INIT_SQL: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");

    #[derive(Debug, QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn relay_rebroadcasts_runner_events_into_web_channels() {
        let (_container, database_url, web_state, runner_state) = setup_live_event_states().await;
        let mut feed = web_state.channels().subscribe("feed");
        let mut subreddit = web_state.channels().subscribe("r/rust");
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus: LiveFeedBusConfig::default(),
                poll_interval: Duration::from_millis(LIVE_EVENT_POLL_INTERVAL_MS),
                reconnect_interval: Duration::from_millis(LIVE_EVENT_RECONNECT_INTERVAL_MS),
                connector: Arc::new(DefaultLiveEventListenerConnector),
            },
        )
        .await
        .expect("relay should start");

        let mut conn = runner_state
            .pool()
            .expect("runner state should have a database pool")
            .get()
            .await
            .expect("runner pool should provide a connection");
        let event = post_created_event(42, "Ferris ships", "ferris-ships", "rust", "ferris");
        store_activity_event(&mut conn, "rust", &event)
            .await
            .expect("runner should persist the live event");

        let feed_msg = tokio::time::timeout(Duration::from_secs(3), feed.recv())
            .await
            .expect("feed relay timed out")
            .expect("feed channel closed unexpectedly");
        let subreddit_msg = tokio::time::timeout(Duration::from_secs(3), subreddit.recv())
            .await
            .expect("subreddit relay timed out")
            .expect("subreddit channel closed unexpectedly");

        let feed_json: Value =
            serde_json::from_str(feed_msg.as_str()).expect("feed message should be valid JSON");
        let subreddit_json: Value = serde_json::from_str(subreddit_msg.as_str())
            .expect("subreddit message should be valid JSON");

        assert_eq!(feed_json["type"], "post_created");
        assert_eq!(feed_json["post_id"], 42);
        assert_eq!(feed_json["subreddit_slug"], "rust");
        assert_eq!(subreddit_json, feed_json);

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn relay_notify_path_beats_long_poll_interval() {
        let (_container, database_url, web_state, runner_state) = setup_live_event_states().await;
        let mut feed = web_state.channels().subscribe("feed");
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus: LiveFeedBusConfig::default(),
                poll_interval: Duration::from_secs(30),
                reconnect_interval: Duration::from_millis(LIVE_EVENT_RECONNECT_INTERVAL_MS),
                connector: Arc::new(DefaultLiveEventListenerConnector),
            },
        )
        .await
        .expect("relay should start");

        let mut conn = runner_state
            .pool()
            .expect("runner state should have a database pool")
            .get()
            .await
            .expect("runner pool should provide a connection");
        let event = post_created_event(99, "Wake up", "wake-up", "rust", "ferris");
        store_activity_event(&mut conn, "rust", &event)
            .await
            .expect("runner should persist the live event");

        let feed_msg = tokio::time::timeout(Duration::from_secs(1), feed.recv())
            .await
            .expect("feed relay should wake long before poll fallback")
            .expect("feed channel closed unexpectedly");
        let feed_json: Value =
            serde_json::from_str(feed_msg.as_str()).expect("feed message should be valid JSON");

        assert_eq!(feed_json["post_id"], 99);
        assert_eq!(feed_json["subreddit_slug"], "rust");

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_bus_rebroadcasts_runner_events_into_web_channels() {
        let (_pg, _redis, database_url, redis_url, web_state, runner_state) =
            setup_live_event_states_with_redis().await;
        let bus = LiveFeedBusConfig {
            kind: LiveFeedBusKind::RedisPubSub,
            redis_url: Some(redis_url),
            channel: "reddit_live_feed".to_owned(),
        };
        install_live_event_bus_with_config(&runner_state, bus.clone())
            .await
            .expect("runner should install the live-event bus");
        let mut feed = web_state.channels().subscribe("feed");
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus,
                poll_interval: Duration::from_secs(30),
                reconnect_interval: Duration::from_millis(LIVE_EVENT_RECONNECT_INTERVAL_MS),
                connector: Arc::new(DefaultLiveEventListenerConnector),
            },
        )
        .await
        .expect("relay should start");

        let mut conn = runner_state
            .pool()
            .expect("runner state should have a database pool")
            .get()
            .await
            .expect("runner pool should provide a connection");
        let event = post_created_event(123, "Redis wakes the feed", "redis-feed", "rust", "ferris");
        let event_id = store_activity_event(&mut conn, "rust", &event)
            .await
            .expect("runner should persist the live event");
        publish_stored_live_event(&runner_state, event_id)
            .await
            .expect("runner should publish the durable event id");

        let feed_msg = tokio::time::timeout(Duration::from_secs(1), feed.recv())
            .await
            .expect("redis bus should wake the relay before the poll fallback")
            .expect("feed channel closed unexpectedly");
        let feed_json: Value =
            serde_json::from_str(feed_msg.as_str()).expect("feed message should be valid JSON");

        assert_eq!(feed_json["post_id"], 123);
        assert_eq!(feed_json["subreddit_slug"], "rust");

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_bus_still_wakes_on_postgres_backup_when_publish_is_missed() {
        let (_pg, _redis, database_url, redis_url, web_state, runner_state) =
            setup_live_event_states_with_redis().await;
        let mut feed = web_state.channels().subscribe("feed");
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus: LiveFeedBusConfig {
                    kind: LiveFeedBusKind::RedisPubSub,
                    redis_url: Some(redis_url),
                    channel: "reddit_live_feed".to_owned(),
                },
                poll_interval: Duration::from_secs(30),
                reconnect_interval: Duration::from_millis(LIVE_EVENT_RECONNECT_INTERVAL_MS),
                connector: Arc::new(DefaultLiveEventListenerConnector),
            },
        )
        .await
        .expect("relay should start");

        let mut conn = runner_state
            .pool()
            .expect("runner state should have a database pool")
            .get()
            .await
            .expect("runner pool should provide a connection");
        let event = post_created_event(
            124,
            "Postgres backup wake",
            "postgres-backup-wake",
            "rust",
            "ferris",
        );
        store_activity_event(&mut conn, "rust", &event)
            .await
            .expect("runner should persist the live event");

        let feed_msg = tokio::time::timeout(Duration::from_secs(1), feed.recv())
            .await
            .expect("postgres backup wake should beat the poll fallback even without Redis publish")
            .expect("feed channel closed unexpectedly");
        let feed_json: Value =
            serde_json::from_str(feed_msg.as_str()).expect("feed message should be valid JSON");

        assert_eq!(feed_json["post_id"], 124);
        assert_eq!(feed_json["subreddit_slug"], "rust");

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn prune_live_feed_events_uses_database_clock_for_retention() {
        let (_container, state) = setup_live_event_prune_state().await;
        let mut conn = state
            .pool()
            .expect("prune state should have a database pool")
            .get()
            .await
            .expect("pool should provide a connection");
        conn.batch_execute("SET TIME ZONE 'America/Chicago'")
            .await
            .expect("test connection should accept a non-UTC timezone");
        diesel::sql_query(
            "INSERT INTO live_feed_events (subreddit_slug, event, created_at) VALUES \
             ('rust', '{\"type\":\"old\"}'::jsonb, NOW() - INTERVAL '7 days' - INTERVAL '1 minute'), \
             ('rust', '{\"type\":\"keep\"}'::jsonb, NOW() - INTERVAL '7 days' + INTERVAL '1 minute')",
        )
        .execute(&mut conn)
        .await
        .expect("test should seed retention boundary events");
        drop(conn);

        prune_live_feed_events(state.clone())
            .await
            .expect("prune should succeed");

        let mut conn = state
            .pool()
            .expect("prune state should have a database pool")
            .get()
            .await
            .expect("pool should provide a connection");
        let remaining: CountRow =
            diesel::sql_query("SELECT COUNT(*) AS count FROM live_feed_events")
                .get_result(&mut conn)
                .await
                .expect("test should count remaining live events");
        assert_eq!(
            remaining.count, 1,
            "prune should use the database clock so the just-inside-retention row survives",
        );
    }

    #[tokio::test]
    async fn comment_event_payload_includes_preview_and_path() {
        let event = comment_created_event(
            7,
            42,
            "ferris-ships",
            "rust",
            "ferris",
            "Borrow checker approved this message.",
        );

        assert_eq!(event["type"], "comment_created");
        assert_eq!(event["comment_id"], 7);
        assert_eq!(event["post_id"], 42);
        assert_eq!(event["subreddit_slug"], "rust");
        assert_eq!(event["author_username"], "ferris");
        assert_eq!(event["path"], json!("/r/rust/posts/ferris-ships#comment-7"));
        assert_eq!(
            event["body_preview"],
            json!("Borrow checker approved this message.")
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn relay_health_snapshot_reports_runtime_state() {
        let (_container, database_url, web_state, runner_state) = setup_live_event_states().await;
        let (listener, wake_tx) = test_listener_pair("test-postgres");
        let mut feed = web_state.channels().subscribe("feed");
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus: LiveFeedBusConfig::default(),
                poll_interval: Duration::from_secs(30),
                reconnect_interval: Duration::from_millis(25),
                connector: Arc::new(TestListenerConnector::from_sequence(vec![Some(listener)])),
            },
        )
        .await
        .expect("relay should start");

        let mut conn = runner_state
            .pool()
            .expect("runner state should have a database pool")
            .get()
            .await
            .expect("runner pool should provide a connection");
        let event = post_created_event(211, "health snapshot", "health-snapshot", "rust", "ferris");
        let event_id = store_activity_event(&mut conn, "rust", &event)
            .await
            .expect("runner should persist the live event");
        wake_tx
            .send(LiveFeedWakeSource::Postgres)
            .expect("test listener should accept a wake signal");

        let _ = tokio::time::timeout(Duration::from_secs(1), feed.recv())
            .await
            .expect("relay should rebroadcast after the synthetic wake")
            .expect("feed channel closed unexpectedly");

        let snapshot =
            live_feed_relay_health_snapshot(&web_state).expect("relay health should be installed");
        assert_eq!(snapshot.bus_kind, "postgres_notify");
        assert_eq!(snapshot.listener_state, "test-postgres");
        assert_eq!(snapshot.wake_postgres, 1);
        assert_eq!(snapshot.replayed_events, 1);
        assert_eq!(snapshot.last_seen_id, event_id);
        assert!(snapshot.last_replayed_at.is_some());

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn relay_reconnects_after_listener_drop() {
        let (_container, database_url, web_state, runner_state) = setup_live_event_states().await;
        let (stale_listener, stale_wake_tx) = test_listener_pair("stale-listener");
        drop(stale_wake_tx);
        let (healthy_listener, healthy_wake_tx) = test_listener_pair("reconnected-listener");
        let connector = Arc::new(TestListenerConnector::from_sequence(vec![
            Some(stale_listener),
            Some(healthy_listener),
        ]));
        let mut feed = web_state.channels().subscribe("feed");
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus: LiveFeedBusConfig::default(),
                poll_interval: Duration::from_secs(30),
                reconnect_interval: Duration::from_millis(25),
                connector: connector.clone(),
            },
        )
        .await
        .expect("relay should start");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = live_feed_relay_health_snapshot(&web_state)
                    .expect("relay health should be installed");
                if snapshot.reconnect_attempts >= 1
                    && snapshot.reconnect_successes >= 1
                    && snapshot.listener_state == "reconnected-listener"
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("relay should reconnect well before the poll fallback");

        let mut conn = runner_state
            .pool()
            .expect("runner state should have a database pool")
            .get()
            .await
            .expect("runner pool should provide a connection");
        let event = post_created_event(212, "reconnect wake", "reconnect-wake", "rust", "ferris");
        store_activity_event(&mut conn, "rust", &event)
            .await
            .expect("runner should persist the live event");
        healthy_wake_tx
            .send(LiveFeedWakeSource::Redis)
            .expect("reconnected listener should accept wake signals");

        let feed_msg = tokio::time::timeout(Duration::from_secs(1), feed.recv())
            .await
            .expect("relay should rebroadcast once the listener reconnects")
            .expect("feed channel closed unexpectedly");
        let feed_json: Value =
            serde_json::from_str(feed_msg.as_str()).expect("feed message should be valid JSON");

        assert_eq!(feed_json["post_id"], 212);

        let snapshot =
            live_feed_relay_health_snapshot(&web_state).expect("relay health should be installed");
        assert!(snapshot.reconnect_attempts >= 1);
        assert!(snapshot.reconnect_successes >= 1);
        assert_eq!(snapshot.listener_state, "reconnected-listener");
        assert_eq!(snapshot.wake_redis, 1);
        assert_eq!(connector.attempts(), 2);

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    #[tokio::test]
    async fn postgres_notify_channel_uses_installed_bus_channel() {
        let state = AppState::detached();
        install_live_event_bus_with_config(
            &state,
            LiveFeedBusConfig {
                kind: LiveFeedBusKind::PostgresNotify,
                redis_url: None,
                channel: "custom_live_feed".to_owned(),
            },
        )
        .await
        .expect("custom live-event bus should install");

        assert_eq!(
            live_event_notify_channel_for_state(&state),
            "autumn_live_event_custom_live_feed"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn relay_health_tracks_publish_and_wake_sources() {
        let success_state = AppState::detached().with_profile("relay-metrics-success");
        install_live_event_bus_with_config(&success_state, LiveFeedBusConfig::default())
            .await
            .expect("publisher should install with default config");
        publish_stored_live_event(&success_state, 7)
            .await
            .expect("default publisher should record a successful publish");

        let success_snapshot = live_feed_relay_health_snapshot(&success_state)
            .expect("publisher install should create relay health");
        assert_eq!(success_snapshot.publish_successes, 1);
        assert_eq!(success_snapshot.publish_failures, 0);

        let failure_state = AppState::detached().with_profile("relay-metrics-failure");
        install_live_event_bus_with_config(
            &failure_state,
            LiveFeedBusConfig {
                kind: LiveFeedBusKind::RedisPubSub,
                redis_url: Some("redis://127.0.0.1:1/".to_owned()),
                channel: "reddit_live_feed".to_owned(),
            },
        )
        .await
        .expect("redis publisher config should install even before the socket exists");
        publish_stored_live_event(&failure_state, 8)
            .await
            .expect_err("publish should fail against an unreachable Redis socket");

        let failure_snapshot = live_feed_relay_health_snapshot(&failure_state)
            .expect("publisher install should create relay health");
        assert_eq!(failure_snapshot.publish_successes, 0);
        assert_eq!(failure_snapshot.publish_failures, 1);
        assert!(failure_snapshot.last_error.is_some());

        let (_container, database_url, web_state, _runner_state) = setup_live_event_states().await;
        let relay = start_live_event_relay_with_options(
            web_state.clone(),
            LiveFeedRelayOptions {
                database_url: Some(database_url),
                bus: LiveFeedBusConfig::default(),
                poll_interval: Duration::from_millis(25),
                reconnect_interval: Duration::from_millis(25),
                connector: Arc::new(TestListenerConnector::from_sequence(vec![None])),
            },
        )
        .await
        .expect("relay should start even when it has to fall back to polling");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = live_feed_relay_health_snapshot(&web_state)
                    .expect("relay health should be installed");
                if snapshot.wake_poll > 0 && snapshot.reconnect_attempts >= 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("relay should record poll wakeups and reconnect attempts");

        let poll_snapshot =
            live_feed_relay_health_snapshot(&web_state).expect("relay health should be installed");
        assert!(poll_snapshot.wake_poll > 0);
        assert!(poll_snapshot.reconnect_attempts >= 1);

        web_state.trigger_shutdown_for_test();
        relay.await.expect("relay task should shut down cleanly");
    }

    async fn setup_live_event_states() -> (ContainerAsync<Postgres>, String, AppState, AppState) {
        let container = Postgres::default()
            .with_init_sql(REDDIT_INIT_SQL.to_string().into_bytes())
            .start()
            .await
            .expect("failed to start Postgres container");

        let host = container
            .get_host()
            .await
            .expect("failed to get container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("failed to get container port");
        let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let web_pool = db::create_pool(&DatabaseConfig {
            url: Some(database_url.clone()),
            pool_size: 4,
            ..DatabaseConfig::default()
        })
        .expect("web pool config should build")
        .expect("web pool should exist");
        let runner_pool = db::create_pool(&DatabaseConfig {
            url: Some(database_url.clone()),
            pool_size: 4,
            ..DatabaseConfig::default()
        })
        .expect("runner pool config should build")
        .expect("runner pool should exist");

        let web_state = AppState::detached()
            .with_profile("redis-web")
            .with_pool(web_pool);
        let runner_state = AppState::detached()
            .with_profile("redis-worker")
            .with_pool(runner_pool);

        (container, database_url, web_state, runner_state)
    }

    async fn setup_live_event_prune_state() -> (ContainerAsync<Postgres>, AppState) {
        let container = Postgres::default()
            .with_init_sql(REDDIT_INIT_SQL.to_string().into_bytes())
            .start()
            .await
            .expect("failed to start Postgres container");

        let host = container
            .get_host()
            .await
            .expect("failed to get container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("failed to get container port");
        let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let pool = db::create_pool(&DatabaseConfig {
            url: Some(database_url),
            pool_size: 1,
            ..DatabaseConfig::default()
        })
        .expect("prune pool config should build")
        .expect("prune pool should exist");

        (container, AppState::for_test().with_pool(pool))
    }

    async fn setup_live_event_states_with_redis() -> (
        ContainerAsync<Postgres>,
        ContainerAsync<Redis>,
        String,
        String,
        AppState,
        AppState,
    ) {
        let (postgres, database_url, web_state, runner_state) = setup_live_event_states().await;
        let redis = Redis::default()
            .start()
            .await
            .expect("failed to start Redis container");
        let redis_host = redis.get_host().await.expect("failed to get Redis host");
        let redis_port = redis
            .get_host_port_ipv4(REDIS_PORT)
            .await
            .expect("failed to get Redis port");
        let redis_url = format!("redis://{redis_host}:{redis_port}/");

        (
            postgres,
            redis,
            database_url,
            redis_url,
            web_state,
            runner_state,
        )
    }

    #[derive(Clone)]
    struct TestListenerConnector {
        attempts: Arc<AtomicUsize>,
        listeners: Arc<Mutex<VecDeque<Option<LiveEventBusListener>>>>,
    }

    impl TestListenerConnector {
        fn from_sequence(listeners: Vec<Option<LiveEventBusListener>>) -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
                listeners: Arc::new(Mutex::new(listeners.into())),
            }
        }

        fn attempts(&self) -> usize {
            AtomicUsize::load(self.attempts.as_ref(), Ordering::Relaxed)
        }
    }

    impl LiveEventListenerConnector for TestListenerConnector {
        fn connect(
            &self,
            _database_url: Option<&str>,
            _bus: &LiveFeedBusConfig,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<LiveEventBusListener>> + Send + '_>,
        > {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            let listeners = self.listeners.clone();
            Box::pin(async move {
                listeners
                    .lock()
                    .expect("test connector queue poisoned")
                    .pop_front()
                    .flatten()
            })
        }
    }

    fn test_listener_pair(
        label: &'static str,
    ) -> (
        LiveEventBusListener,
        tokio::sync::mpsc::UnboundedSender<LiveFeedWakeSource>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (LiveEventBusListener::test(label, rx), tx)
    }
}
