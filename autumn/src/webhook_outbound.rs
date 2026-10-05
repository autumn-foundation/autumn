#![allow(
    clippy::significant_drop_tightening,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
//! Outbound signed webhook delivery with retries, DLQ, and subscription management.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use crate::http_client::Client;
use crate::{AppState, AutumnError, AutumnResult};

const MAX_LOGGED_RESPONSE_BODY_BYTES: usize = 16 * 1024;
const TRUNCATED_RESPONSE_BODY_SUFFIX: &str = "\n[truncated]";

/// The status of a webhook subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookSubscriptionStatus {
    Active,
    Disabled,
    Failed,
}

impl WebhookSubscriptionStatus {
    /// Return the lower-case status label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for WebhookSubscriptionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registered webhook subscription targeting a consumer endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookSubscription {
    pub id: String,
    pub target_url: String,
    pub event_topics: Vec<String>,
    pub secret: String,
    pub status: WebhookSubscriptionStatus,
    pub consecutive_failures: u32,
}

/// A structured log of an outbound webhook delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDeliveryLog {
    pub id: String,
    pub subscription_id: String,
    pub topic: String,
    pub payload: String,
    pub request_headers: HashMap<String, String>,
    pub response_status: Option<u16>,
    pub response_body: Option<String>,
    pub elapsed_ms: u64,
    pub attempt: u32,
    pub max_attempts: u32,
    pub is_dlq: bool,
    pub last_error: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Pluggable handler interface for outbound webhook subscriptions and delivery logs.
pub trait OutboundWebhookHandler: Send + Sync + 'static {
    /// Retrieve active subscriptions registered for a specific event topic.
    fn get_subscriptions(
        &self,
        topic: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookSubscription>>> + Send>>;

    /// Log a webhook delivery attempt and handle failure counters/statuses.
    fn log_delivery(
        &self,
        log: WebhookDeliveryLog,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>;

    /// Replace a stored delivery log without treating it as a new delivery outcome.
    ///
    /// Implementations must perform a plain record replacement. This must not
    /// update subscription failure counters or auto-failure state.
    fn replace_delivery_log(
        &self,
        log: WebhookDeliveryLog,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>;

    /// Retrieve a specific webhook subscription by ID (regardless of status/active state).
    fn get_subscription(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookSubscription>>> + Send>>;

    /// Optional: List only permanently failed delivery attempts archived in the Dead Letter Queue.
    fn get_dlq_logs(
        &self,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookDeliveryLog>>> + Send>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Get a specific delivery log by ID.
    fn get_delivery_log(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookDeliveryLog>>> + Send>>;

    /// Optional: Reset consecutive failures for a subscription.
    fn reset_subscription_failures(
        &self,
        _id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        Box::pin(async { Ok(()) })
    }

    /// Optional: Reactivate a subscription that was auto-marked as failed.
    ///
    /// Manual DLQ replays need to bypass the automatic failure guard without
    /// re-enabling subscriptions that an operator explicitly disabled.
    fn reactivate_failed_subscription(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        self.reset_subscription_failures(id)
    }
}

/// Legacy alias for backward compatibility.
pub use OutboundWebhookHandler as OutboundWebhookStore;

#[cfg(feature = "db")]
mod sql_store;
#[cfg(feature = "db")]
pub use sql_store::{SqlOutboundWebhookStore, WEBHOOK_SCHEMA_SQL};

/// Bounded, thread-safe, process-local in-memory implementation of the outbound webhook handler.
#[derive(Debug, Default)]
pub struct InMemoryOutboundWebhookHandler {
    subscriptions: RwLock<HashMap<String, WebhookSubscription>>,
    logs: RwLock<HashMap<String, WebhookDeliveryLog>>,
}

/// Legacy alias for backward compatibility.
pub type InMemoryOutboundWebhookStore = InMemoryOutboundWebhookHandler;

impl InMemoryOutboundWebhookHandler {
    /// Create a new, empty in-memory handler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Helper to register a subscription in memory for testing/dev.
    #[allow(clippy::unused_async)]
    pub async fn create_subscription(
        &self,
        sub: WebhookSubscription,
    ) -> AutumnResult<WebhookSubscription> {
        let mut subs = self
            .subscriptions
            .write()
            .expect("subscriptions write lock poisoned");
        subs.insert(sub.id.clone(), sub.clone());
        Ok(sub)
    }

    /// Helper to retrieve logged deliveries for testing/dev.
    #[allow(clippy::unused_async)]
    pub async fn get_delivery_logs(&self) -> AutumnResult<Vec<WebhookDeliveryLog>> {
        let logs = self.logs.read().expect("logs read lock poisoned");
        let mut list: Vec<WebhookDeliveryLog> = logs.values().cloned().collect();
        list.sort_by_key(|l| l.timestamp);
        list.reverse();
        Ok(list)
    }

    /// Helper to fetch a single subscription.
    #[allow(clippy::unused_async)]
    pub async fn get_subscription(&self, id: &str) -> AutumnResult<Option<WebhookSubscription>> {
        let subs = self
            .subscriptions
            .read()
            .expect("subscriptions read lock poisoned");
        Ok(subs.get(id).cloned())
    }
}

impl OutboundWebhookHandler for InMemoryOutboundWebhookHandler {
    fn get_subscriptions(
        &self,
        topic: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookSubscription>>> + Send>> {
        let subs = self
            .subscriptions
            .read()
            .expect("subscriptions read lock poisoned");
        let topic = topic.to_owned();
        let list: Vec<WebhookSubscription> = subs
            .values()
            .filter(|sub| {
                sub.event_topics.iter().any(|t| t == &topic)
                    && sub.status == WebhookSubscriptionStatus::Active
            })
            .cloned()
            .collect();
        Box::pin(async move { Ok(list) })
    }

    fn get_subscription(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookSubscription>>> + Send>> {
        let subs = self
            .subscriptions
            .read()
            .expect("subscriptions read lock poisoned");
        let sub = subs.get(id).cloned();
        Box::pin(async move { Ok(sub) })
    }

    fn log_delivery(
        &self,
        log: WebhookDeliveryLog,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        let mut logs = self.logs.write().expect("logs write lock poisoned");
        logs.insert(log.id.clone(), log.clone());

        // Manage subscription consecutive failures and auto-disabling state
        let mut subs = self
            .subscriptions
            .write()
            .expect("subscriptions write lock poisoned");
        if let Some(sub) = subs.get_mut(&log.subscription_id) {
            let is_active = sub.status == WebhookSubscriptionStatus::Active;
            if is_active {
                if let Some(status) = log.response_status {
                    if (200..300).contains(&status) {
                        sub.consecutive_failures = 0;
                    } else {
                        sub.consecutive_failures = sub.consecutive_failures.saturating_add(1);
                        if sub.consecutive_failures >= 50 {
                            sub.status = WebhookSubscriptionStatus::Failed;
                            tracing::warn!(subscription_id = %sub.id, "Webhook subscription auto-disabled due to 50 consecutive failures");
                        }
                    }
                } else if log.last_error.is_some() {
                    sub.consecutive_failures = sub.consecutive_failures.saturating_add(1);
                    if sub.consecutive_failures >= 50 {
                        sub.status = WebhookSubscriptionStatus::Failed;
                        tracing::warn!(subscription_id = %sub.id, "Webhook subscription auto-disabled due to 50 consecutive failures");
                    }
                }
            }
        }

        Box::pin(async move { Ok(()) })
    }

    fn replace_delivery_log(
        &self,
        log: WebhookDeliveryLog,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        let mut logs = self.logs.write().expect("logs write lock poisoned");
        logs.insert(log.id.clone(), log);
        Box::pin(async move { Ok(()) })
    }

    fn get_dlq_logs(
        &self,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookDeliveryLog>>> + Send>> {
        let list = {
            let logs = self.logs.read().expect("logs read lock poisoned");
            let mut list: Vec<WebhookDeliveryLog> =
                logs.values().filter(|l| l.is_dlq).cloned().collect();
            list.sort_by_key(|l| l.timestamp);
            list.reverse();
            list
        };
        Box::pin(async move { Ok(list) })
    }

    fn get_delivery_log(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookDeliveryLog>>> + Send>> {
        let log = self
            .logs
            .read()
            .expect("logs read lock poisoned")
            .get(id)
            .cloned();
        Box::pin(async move { Ok(log) })
    }

    fn reset_subscription_failures(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        {
            let mut subs = self
                .subscriptions
                .write()
                .expect("subscriptions write lock poisoned");
            if let Some(sub) = subs.get_mut(id) {
                sub.consecutive_failures = 0;
            }
        }
        Box::pin(async move { Ok(()) })
    }

    fn reactivate_failed_subscription(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        {
            let mut subs = self
                .subscriptions
                .write()
                .expect("subscriptions write lock poisoned");
            if let Some(sub) = subs.get_mut(id) {
                sub.consecutive_failures = 0;
                if sub.status == WebhookSubscriptionStatus::Failed {
                    sub.status = WebhookSubscriptionStatus::Active;
                }
            }
        }
        Box::pin(async move { Ok(()) })
    }
}

/// A runtime delegation callback type to bridge core autumn to autumn-harvest dynamically.
pub type WebhookDelegate = Arc<
    dyn Fn(
            &AppState,
            WebhookSubscription,
            WebhookDeliveryLog,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>
        + Send
        + Sync,
>;

/// `AppState` extension for the runtime delegation hook.
#[derive(Clone)]
pub struct WebhookDelegateExt(pub WebhookDelegate);

/// The runtime manager for outbound webhooks.
#[derive(Clone)]
pub struct WebhookOutboundManager {
    handler: Arc<dyn OutboundWebhookHandler>,
    client: Client,
    initial_backoff_ms: u64,
    max_attempts: u32,
}

/// Default number of delivery attempts for one webhook.
pub const DEFAULT_WEBHOOK_MAX_ATTEMPTS: u32 = 5;

impl WebhookOutboundManager {
    /// Create a new webhook manager with a handler.
    pub fn new(handler: Arc<dyn OutboundWebhookHandler>) -> Self {
        Self {
            handler,
            client: Client::new(),
            initial_backoff_ms: 1000,
            max_attempts: DEFAULT_WEBHOOK_MAX_ATTEMPTS,
        }
    }

    /// Set the number of delivery attempts per webhook. Default: 5.
    /// A value of 0 is read as 1. The last failed attempt moves the delivery
    /// to the DLQ.
    ///
    /// This does not change the `autumn_webhook_delivery` job. That job needs
    /// 2 job attempts per delivery attempt. Use
    /// [`OutboundWebhookPlugin::with_max_attempts`], which sets both.
    #[must_use]
    pub const fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = if max_attempts == 0 { 1 } else { max_attempts };
        self
    }

    /// The number of delivery attempts per webhook.
    #[must_use]
    pub const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Set a custom initial backoff for retries.
    #[must_use]
    pub const fn with_initial_backoff_ms(mut self, ms: u64) -> Self {
        self.initial_backoff_ms = ms;
        self
    }

    fn with_client_from_state(mut self, state: &AppState) -> Self {
        self.client = Client::from_state(state);
        self
    }

    /// Access the underlying webhook handler (compatibility/actuator support).
    #[must_use]
    pub fn store(&self) -> &Arc<dyn OutboundWebhookHandler> {
        &self.handler
    }

    /// Access the underlying http client.
    #[must_use]
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// Dispatch a signed webhook payload to all subscriptions interested in `topic`.
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] if payload serialization or queueing fails.
    pub async fn dispatch<T: Serialize + Sync>(
        &self,
        state: &AppState,
        topic: &str,
        payload: &T,
    ) -> AutumnResult<()> {
        self.dispatch_inner(state, topic, payload, None).await
    }

    /// Dispatch through the transactional outbox, on the connection of the
    /// open transaction. Returns the outbox message id.
    ///
    /// The relay calls [`dispatch`](Self::dispatch) after commit. See
    /// [`Outbox::dispatch_webhook`](crate::outbox::Outbox::dispatch_webhook).
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] if serialization or the outbox insert fails.
    #[cfg(feature = "db")]
    pub async fn dispatch_in_tx<T: Serialize + Sync + ?Sized>(
        &self,
        state: &AppState,
        conn: &mut crate::db::RuntimeConnection,
        topic: &str,
        payload: &T,
    ) -> AutumnResult<String> {
        crate::outbox::Outbox::new(state)
            .dispatch_webhook(conn, topic, payload)
            .await
    }

    /// Dispatch for outbox message `message_id`.
    ///
    /// Each delivery id comes from the message id and the subscription id.
    /// A second call for one message skips each delivery that has a result.
    /// It enqueues a delivery with no result again, with the same id.
    #[cfg(feature = "db")]
    pub(crate) async fn dispatch_for_message(
        &self,
        state: &AppState,
        message_id: &str,
        topic: &str,
        payload: &serde_json::Value,
    ) -> AutumnResult<()> {
        self.dispatch_inner(state, topic, payload, Some(message_id))
            .await
    }

    async fn dispatch_inner<T: Serialize + Sync + ?Sized>(
        &self,
        state: &AppState,
        topic: &str,
        payload: &T,
        message_id: Option<&str>,
    ) -> AutumnResult<()> {
        let serialized = serde_json::to_string(payload).map_err(|e| {
            AutumnError::internal_server_error_msg(format!("failed to serialize payload: {e}"))
        })?;

        let mut errors = Vec::new();
        let subs = self.handler.get_subscriptions(topic).await?;
        for sub in subs {
            if sub.status == WebhookSubscriptionStatus::Disabled {
                continue;
            }

            // A relay re-send finds the log of its first send. If a delivery
            // attempt started, the job exists: skip. If not, the first send
            // can have stopped before the enqueue: enqueue again. The copy
            // has the same `webhook-id`, so the receiver can drop it.
            let mut existing = None;
            let log_id = match message_id {
                Some(message_id) => {
                    let log_id = delivery_id_for_message(message_id, &sub.id);
                    if let Some(log) = self.handler.get_delivery_log(&log_id).await? {
                        if log.response_status.is_some() || log.last_error.is_some() || log.is_dlq {
                            continue;
                        }
                        existing = Some(log);
                    }
                    log_id
                }
                None => state.entropy().uuid_v4().to_string(),
            };
            let log = existing.clone().unwrap_or_else(|| WebhookDeliveryLog {
                id: log_id.clone(),
                subscription_id: sub.id.clone(),
                topic: topic.to_owned(),
                payload: serialized.clone(),
                request_headers: HashMap::new(),
                response_status: None,
                response_body: None,
                elapsed_ms: 0,
                attempt: 1,
                max_attempts: self.max_attempts,
                is_dlq: false,
                last_error: None,
                timestamp: crate::time::ambient_now(),
            });

            // Register the initial attempt in local storage
            if existing.is_none()
                && let Err(e) = self.handler.log_delivery(log.clone()).await
            {
                errors.push(e);
                continue;
            }

            // If a delegate extension is registered, run it (delegates to Harvest workflow)
            if let Some(delegate_ext) = state.extension::<WebhookDelegateExt>() {
                tracing::info!(subscription_id = %sub.id, "WebhookOutboundManager::dispatch: delegating webhook delivery via runtime hook");
                if let Err(e) = (delegate_ext.0)(state, sub, log).await {
                    errors.push(e);
                }
            } else {
                // Fallback: enqueue a standard background job
                tracing::debug!(subscription_id = %sub.id, "WebhookOutboundManager::dispatch: enqueuing fallback webhook delivery job");
                if let Some(job_client) = crate::job::global_job_client() {
                    let job_payload = serde_json::json!({
                        "log_id": log.id.clone(),
                    });
                    if let Err(e) = job_client
                        .enqueue("autumn_webhook_delivery", job_payload)
                        .await
                    {
                        errors.push(self.enqueue_failed(log, e.to_string(), message_id).await);
                    }
                } else {
                    errors.push(
                        self.enqueue_failed(
                            log,
                            "Global job client is unavailable; fallback webhook delivery job not enqueued"
                                .to_owned(),
                            message_id,
                        )
                        .await,
                    );
                }
            }
        }

        if !errors.is_empty() {
            return Err(errors.remove(0));
        }

        Ok(())
    }

    /// An enqueue failed. From the outbox, keep the log pending and return
    /// the error: the relay sends the message again, and the next send
    /// enqueues the delivery. Else, move the log to the DLQ.
    async fn enqueue_failed(
        &self,
        log: WebhookDeliveryLog,
        message: String,
        message_id: Option<&str>,
    ) -> AutumnError {
        if message_id.is_some() {
            return AutumnError::internal_server_error_msg(message);
        }
        self.record_delivery_enqueue_failure(log, message).await
    }

    async fn record_delivery_enqueue_failure(
        &self,
        mut log: WebhookDeliveryLog,
        message: String,
    ) -> AutumnError {
        log.is_dlq = true;
        log.last_error = Some(message.clone());
        log.timestamp = crate::time::ambient_now();

        if let Err(e) = self.handler.replace_delivery_log(log).await {
            tracing::error!(
                error = %e,
                "Failed to mark webhook delivery log as DLQ after enqueue failure"
            );
            return e;
        }

        AutumnError::internal_server_error_msg(message)
    }
}

/// A delivery id made from an outbox message and a subscription. It has the
/// same UUID format as a random delivery id.
fn delivery_id_for_message(message_id: &str, subscription_id: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::new()
        .chain_update(b"autumn.webhook.delivery\0")
        .chain_update(message_id.as_bytes())
        .chain_update(b"\0")
        .chain_update(subscription_id.as_bytes())
        .finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    crate::entropy::uuid_v4_from_bytes(bytes).to_string()
}

fn install_outbound_webhook_manager(
    state: &AppState,
    store: Arc<dyn OutboundWebhookHandler>,
    initial_backoff_ms: u64,
    max_attempts: u32,
) {
    let manager = WebhookOutboundManager::new(store)
        .with_initial_backoff_ms(initial_backoff_ms)
        .with_max_attempts(max_attempts)
        .with_client_from_state(state);
    state.insert_extension(manager);
}

/// The `webhook-signature` value of the Standard Webhooks spec: `v1,` and the
/// base64 HMAC-SHA256 of `{id}.{timestamp}.{body}`.
///
/// The spec keys the HMAC with the base64 part of a `whsec_` secret. Other
/// secrets key it with their raw bytes.
fn standard_webhooks_signature(secret: &str, id: &str, timestamp: i64, body: &str) -> String {
    use base64::Engine as _;
    use hmac::Mac as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let key = secret
        .strip_prefix("whsec_")
        .and_then(|encoded| engine.decode(encoded).ok())
        .unwrap_or_else(|| secret.as_bytes().to_vec());
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).expect("HMAC accepts a key of any length");
    mac.update(format!("{id}.{timestamp}.{body}").as_bytes());
    format!("v1,{}", engine.encode(mac.finalize().into_bytes()))
}

/// Asynchronous background job that delivers a webhook payload (legacy fallback).
#[allow(
    clippy::redundant_closure_for_method_calls,
    clippy::too_many_lines,
    clippy::must_use_candidate
)]
pub fn deliver_webhook_job(
    state: AppState,
    payload: serde_json::Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        let is_replay = payload
            .get("replay")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let manager = state.extension::<WebhookOutboundManager>().ok_or_else(|| {
            AutumnError::internal_server_error_msg("WebhookOutboundManager not found in extensions")
        })?;

        // Support both self-contained payload structure and legacy log_id lookup (for replays)
        let (sub, mut log) = if let Some(sub_val) = payload.get("subscription") {
            let _payload_sub: WebhookSubscription = serde_json::from_value(sub_val.clone())
                .map_err(|e| {
                    AutumnError::bad_request_msg(format!("failed to parse subscription: {e}"))
                })?;
            let mut log: WebhookDeliveryLog = serde_json::from_value(
                payload
                    .get("log")
                    .cloned()
                    .ok_or_else(|| AutumnError::bad_request_msg("missing log in job payload"))?,
            )
            .map_err(|e| AutumnError::bad_request_msg(format!("failed to parse log: {e}")))?;

            if !begin_attempt(&manager, &mut log, is_replay).await? {
                return Ok(());
            }

            let sub = load_current_subscription(&manager, &log).await?;
            (sub, log)
        } else {
            let log_id = payload
                .get("log_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AutumnError::bad_request_msg("missing log_id in job payload"))?;

            tracing::debug!(log_id = %log_id, "deliver_webhook_job: starting webhook delivery via log lookup");

            let log_opt = manager.store().get_delivery_log(log_id).await?;
            let mut log = log_opt.ok_or_else(|| {
                AutumnError::not_found_msg(format!("delivery log {log_id} not found"))
            })?;

            // A second job for a delivered log (an outbox re-send can enqueue
            // one) must not send again.
            if !is_replay
                && log
                    .response_status
                    .is_some_and(|status| (200..300).contains(&status))
            {
                tracing::debug!(log_id = %log_id, "webhook delivery already succeeded; skipping");
                return Ok(());
            }

            if !begin_attempt(&manager, &mut log, is_replay).await? {
                return Ok(());
            }

            // Load latest subscription state to respect emergency rotations/disable
            let sub = load_current_subscription(&manager, &log).await?;
            (sub, log)
        };

        if sub.status == WebhookSubscriptionStatus::Disabled {
            tracing::info!(subscription_id = %sub.id, "Webhook subscription is disabled; skipping delivery");
            log.last_error = Some("Subscription is disabled".to_owned());
            log.timestamp = crate::time::ambient_now();
            if is_replay {
                log.is_dlq = true;
            }
            manager.store().log_delivery(log).await?;
            return Ok(());
        }

        if sub.status == WebhookSubscriptionStatus::Failed && !is_replay {
            tracing::info!(subscription_id = %sub.id, "Webhook subscription has failed; skipping delivery");
            log.last_error = Some("Subscription has failed due to consecutive errors".to_owned());
            log.timestamp = crate::time::ambient_now();
            manager.store().log_delivery(log).await?;
            return Ok(());
        }
        if sub.status == WebhookSubscriptionStatus::Failed {
            tracing::info!(subscription_id = %sub.id, "Replaying webhook delivery for failed subscription");
        }

        // Stripe-style payload signing: t=<timestamp>,v1=<signature>
        // The receiver checks `t=` against its own real clock, so sign with
        // the ambient clock: real time outside a `Sim` (issue #2967).
        let timestamp = crate::time::ambient_now().timestamp();
        let signing_payload = format!("{timestamp}.{}", log.payload);
        let signature = crate::security::config::hmac_sha256_hex(
            sub.secret.as_bytes(),
            signing_payload.as_bytes(),
        );
        let signature_header = format!("t={timestamp},v1={signature}");

        // Standard Webhooks headers (issue #3062). `webhook-id` is the log
        // id. Job retries use the same log, so each attempt sends the same
        // id. `webhook-signature` signs the id too, so a replay with a new id
        // fails the check.
        let mut request_headers = HashMap::new();
        request_headers.insert("Content-Type".to_owned(), "application/json".to_owned());
        request_headers.insert("Autumn-Signature".to_owned(), signature_header);
        request_headers.insert("webhook-id".to_owned(), log.id.clone());
        request_headers.insert("webhook-timestamp".to_owned(), timestamp.to_string());
        request_headers.insert(
            "webhook-signature".to_owned(),
            standard_webhooks_signature(&sub.secret, &log.id, timestamp, &log.payload),
        );

        let start = crate::time::ambient_monotonic();
        // `target_url` is a subscriber-chosen destination, not one the app
        // itself picked — exactly the case `ssrf_safe()` exists for. Without
        // it this POST carries none of the private/link-local/loopback/cloud-
        // metadata deny-list `Client::get_ssrf_safe` documents. `ssrf_safe()`
        // alone would route through the custom send path, which (like the
        // mock path) bypasses `send_recorded`'s circuit breaker by default —
        // right for a one-off fetch of an arbitrary URL, wrong for repeated
        // deliveries to the same subscriber host. `breaker_scoped()` keeps
        // the fail-fast-on-a-down-receiver behavior every other outbound call
        // gets, correctly ordered with the mock bypass, capsule replay, and
        // capsule *recording* of the attempt — all handled inside
        // `send_recorded` itself, not layered on here. See
        // docs/security/2026-09-03-webhook-ssrf/README.md.
        let mut req = manager
            .client
            .named(&sub.target_url)
            .post(&sub.target_url)
            .ssrf_safe()
            .breaker_scoped();
        // Send exactly the headers the log records.
        let mut header_names: Vec<&String> = request_headers.keys().collect();
        header_names.sort();
        for name in header_names {
            req = req.header(name, &request_headers[name]);
        }
        let req = req.text_body(log.payload.clone());

        let response = req.send().await;
        let elapsed = u64::try_from(
            crate::time::ambient_monotonic()
                .saturating_duration_since(start)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);

        tracing::debug!(
            log_id = %log.id,
            status = ?response.as_ref().map(|r| r.status()),
            "deliver_webhook_job: webhook HTTP request finished"
        );

        log.elapsed_ms = elapsed;
        log.timestamp = crate::time::ambient_now();
        log.request_headers = request_headers;

        match response {
            Ok(res) => {
                let status = res.status();
                log.response_status = Some(status.as_u16());
                let is_success = res.is_success();
                let body_str = cap_logged_response_body(res.text());
                log.response_body = Some(body_str);

                if is_success {
                    log.last_error = None;
                    manager.store().log_delivery(log).await?;
                    reset_subscription_after_success(&manager, &sub).await;
                    Ok(())
                } else {
                    let status_err = format!("server returned status: {status}");
                    log.last_error = Some(status_err.clone());
                    if attempts_remain(&manager, &log) {
                        manager.store().log_delivery(log.clone()).await?;
                    }
                    handle_delivery_failure(&manager, &sub, log, status_err).await
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                log.last_error = Some(error_str.clone());
                if attempts_remain(&manager, &log) {
                    manager.store().log_delivery(log.clone()).await?;
                }
                handle_delivery_failure(&manager, &sub, log, error_str).await
            }
        }
    })
}

/// `true` when this delivery has attempts left. The limit is the smaller of
/// the log's own limit and the manager's. A log written before a deploy that
/// lowered the limit then still moves to the DLQ before the job engine runs
/// out of job attempts (2 per delivery attempt).
fn attempts_remain(manager: &WebhookOutboundManager, log: &WebhookDeliveryLog) -> bool {
    log.attempt < log.max_attempts.min(manager.max_attempts)
}

/// Prepares the log for this run of the delivery job. Returns `false` when
/// the delivery must not be sent.
///
/// A log that was already attempted (it has a status or an error) is a retry
/// from the job engine. The retry gets the next attempt number, and the
/// pre-send log is written. But first the limit is checked: a deploy can lower
/// the limit while a retry waits, and that retry must go to the DLQ without a
/// further request. A manual DLQ replay is not limited.
async fn begin_attempt(
    manager: &WebhookOutboundManager,
    log: &mut WebhookDeliveryLog,
    is_replay: bool,
) -> AutumnResult<bool> {
    if log.response_status.is_none() && log.last_error.is_none() {
        return Ok(true);
    }
    if !is_replay && !attempts_remain(manager, log) {
        log.is_dlq = true;
        manager.store().log_delivery(log.clone()).await?;
        tracing::warn!(
            subscription_id = %log.subscription_id,
            attempt = log.attempt,
            "Webhook delivery reached its lowered attempt limit; sent to DLQ"
        );
        return Ok(false);
    }
    log.attempt = log.attempt.saturating_add(1);
    log.response_status = None;
    log.response_body = None;
    log.last_error = None;
    manager.store().log_delivery(log.clone()).await?;
    Ok(true)
}

async fn load_current_subscription(
    manager: &WebhookOutboundManager,
    log: &WebhookDeliveryLog,
) -> AutumnResult<WebhookSubscription> {
    manager
        .store()
        .get_subscription(&log.subscription_id)
        .await?
        .ok_or_else(|| {
            AutumnError::not_found_msg(format!("subscription {} not found", log.subscription_id))
        })
}

fn cap_logged_response_body(mut body: String) -> String {
    if body.len() <= MAX_LOGGED_RESPONSE_BODY_BYTES {
        return body;
    }

    let body_budget =
        MAX_LOGGED_RESPONSE_BODY_BYTES.saturating_sub(TRUNCATED_RESPONSE_BODY_SUFFIX.len());
    let mut cutoff = body_budget.min(body.len());
    while cutoff > 0 && !body.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    body.truncate(cutoff);
    body.push_str(TRUNCATED_RESPONSE_BODY_SUFFIX);
    body
}

async fn reset_subscription_after_success(
    manager: &WebhookOutboundManager,
    sub: &WebhookSubscription,
) {
    if let Err(e) = manager
        .store()
        .reactivate_failed_subscription(&sub.id)
        .await
    {
        tracing::warn!(
            subscription_id = %sub.id,
            "Webhook delivery succeeded but subscription failure state could not be reset: {}",
            e
        );
    }
}

async fn handle_delivery_failure(
    manager: &WebhookOutboundManager,
    sub: &WebhookSubscription,
    mut log: WebhookDeliveryLog,
    error_msg: String,
) -> AutumnResult<()> {
    if attempts_remain(manager, &log) {
        // Return an error to signal the background job runner to retry this job
        Err(AutumnError::internal_server_error_msg(format!(
            "delivery attempt {} failed, scheduled retry: {error_msg}",
            log.attempt
        )))
    } else {
        log.is_dlq = true;
        manager.store().log_delivery(log).await?;
        // Return Ok(()) to mark the permanently failed job as complete and send to DLQ
        tracing::warn!(subscription_id = %sub.id, "Webhook delivery failed permanently; sent to DLQ: {}", error_msg);
        Ok(())
    }
}

/// `AppBuilder` plugin for outbound signed webhook delivery infrastructure.
pub struct OutboundWebhookPlugin {
    store: StoreSource,
    initial_backoff_ms: u64,
    max_attempts: u32,
}

/// Where the plugin gets its store.
enum StoreSource {
    Given(Arc<dyn OutboundWebhookHandler>),
    /// A [`SqlOutboundWebhookStore`] on the app pool.
    #[cfg(feature = "db")]
    AppDatabase,
}

impl OutboundWebhookPlugin {
    /// Create a new outbound webhook plugin using the specified store.
    #[must_use]
    pub fn new(store: Arc<dyn OutboundWebhookHandler>) -> Self {
        Self {
            store: StoreSource::Given(store),
            initial_backoff_ms: 1000,
            max_attempts: DEFAULT_WEBHOOK_MAX_ATTEMPTS,
        }
    }

    /// A plugin with a durable [`SqlOutboundWebhookStore`] on the app
    /// database. At startup it creates the tables if they do not exist.
    /// `TestApp` runs no startup hook: call
    /// [`SqlOutboundWebhookStore::ensure_schema`] in the test.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn sql() -> Self {
        Self {
            store: StoreSource::AppDatabase,
            initial_backoff_ms: 1000,
            max_attempts: DEFAULT_WEBHOOK_MAX_ATTEMPTS,
        }
    }

    /// Override the initial backoff retry delay.
    #[must_use]
    pub const fn with_initial_backoff_ms(mut self, ms: u64) -> Self {
        self.initial_backoff_ms = ms;
        self
    }

    /// Set the number of delivery attempts per webhook. Default: 5.
    /// A value of 0 is read as 1. The last failed attempt moves the delivery
    /// to the DLQ.
    #[must_use]
    pub const fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = if max_attempts == 0 { 1 } else { max_attempts };
        self
    }
}

impl crate::plugin::Plugin for OutboundWebhookPlugin {
    fn build(self, app: crate::app::AppBuilder) -> crate::app::AppBuilder {
        let initial_backoff_ms = self.initial_backoff_ms;
        let max_attempts = self.max_attempts;
        let app = match self.store {
            StoreSource::Given(store) => app.state_initializer(move |state| {
                install_outbound_webhook_manager(
                    state,
                    store.clone(),
                    initial_backoff_ms,
                    max_attempts,
                );
            }),
            #[cfg(feature = "db")]
            StoreSource::AppDatabase => app
                .state_initializer(move |state| {
                    if let Some(pool) = state.pool() {
                        install_outbound_webhook_manager(
                            state,
                            Arc::new(SqlOutboundWebhookStore::new(pool.clone())),
                            initial_backoff_ms,
                            max_attempts,
                        );
                    } else {
                        tracing::error!(
                            "OutboundWebhookPlugin::sql needs a database; webhooks are off"
                        );
                    }
                })
                .on_startup(|state| async move {
                    match state.pool() {
                        Some(pool) => {
                            SqlOutboundWebhookStore::new(pool.clone())
                                .ensure_schema()
                                .await
                        }
                        None => Ok(()),
                    }
                }),
        };
        app.jobs(vec![crate::job::JobInfo {
            name: "autumn_webhook_delivery".to_string(),
            // The job engine runs the retries. Two job attempts per delivery
            // attempt leave room for store errors, which use a job attempt
            // but not a delivery attempt.
            max_attempts: max_attempts.saturating_mul(2),
            initial_backoff_ms,
            queue: "default".to_string(),
            uniqueness: None,
            concurrency: None,
            version: 1,
            handler: deliver_webhook_job,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{HttpMockRegistryExt, MockRegistry, MockSetupBuilder};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mock_builder(registry: Arc<MockRegistry>, alias: &str) -> MockSetupBuilder {
        MockSetupBuilder {
            registry,
            alias: alias.to_owned(),
            method: None,
            path: None,
        }
    }

    fn sample_subscription(
        id: &str,
        target_url: &str,
        status: WebhookSubscriptionStatus,
    ) -> WebhookSubscription {
        WebhookSubscription {
            id: id.to_owned(),
            target_url: target_url.to_owned(),
            event_topics: vec!["orders.created".to_owned()],
            secret: "my_webhook_signing_secret_32_bytes!!".to_owned(),
            status,
            consecutive_failures: if status == WebhookSubscriptionStatus::Failed {
                50
            } else {
                0
            },
        }
    }

    fn sample_log(id: &str, subscription_id: &str) -> WebhookDeliveryLog {
        WebhookDeliveryLog {
            id: id.to_owned(),
            subscription_id: subscription_id.to_owned(),
            topic: "orders.created".to_owned(),
            payload: serde_json::json!({ "order_id": "ord_123" }).to_string(),
            request_headers: HashMap::new(),
            response_status: None,
            response_body: None,
            elapsed_ms: 0,
            attempt: 1,
            max_attempts: 5,
            is_dlq: false,
            last_error: None,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn outbound_webhook_plugin_installs_manager_without_startup_hook() {
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let builder = crate::app().plugin(OutboundWebhookPlugin::new(store));

        assert!(
            builder.startup_hooks.is_empty(),
            "webhook manager must be installed before job workers start, not from a startup hook"
        );
        assert_eq!(builder.state_initializers.len(), 1);
    }

    /// Issue #3054: the number of delivery attempts is configurable.
    #[tokio::test]
    async fn max_attempts_reaches_the_delivery_log_and_the_job() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let mut builder =
            crate::app().plugin(OutboundWebhookPlugin::new(store.clone()).with_max_attempts(3));
        assert_eq!(
            builder.jobs[0].max_attempts, 6,
            "two job attempts per delivery"
        );

        let state = AppState::for_test();
        let initializer = builder.state_initializers.remove(0);
        initializer(&state);
        let manager = state
            .extension::<WebhookOutboundManager>()
            .expect("manager installed");
        assert_eq!(manager.max_attempts(), 3);

        store
            .create_subscription(sample_subscription(
                "sub_n",
                "http://mock-receiver/n",
                WebhookSubscriptionStatus::Active,
            ))
            .await
            .unwrap();
        // No job client: the dispatch fails, but the log is written first.
        let _ = manager
            .dispatch(&state, "orders.created", &serde_json::json!({}))
            .await;
        let logs = store.get_delivery_logs().await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].max_attempts, 3);
    }

    #[test]
    fn zero_max_attempts_reads_as_one() {
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        assert_eq!(
            WebhookOutboundManager::new(store)
                .with_max_attempts(0)
                .max_attempts(),
            1
        );
    }

    #[tokio::test]
    async fn replay_job_sends_failed_subscription_instead_of_skipping() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/replay")
            .post("/webhooks/replay")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let sub = sample_subscription(
            "sub_failed",
            "http://mock-receiver/webhooks/replay",
            WebhookSubscriptionStatus::Failed,
        );
        store.create_subscription(sub).await.unwrap();
        store
            .replace_delivery_log(sample_log("log_replay", "sub_failed"))
            .await
            .unwrap();

        deliver_webhook_job(
            state,
            serde_json::json!({
                "log_id": "log_replay",
                "replay": true,
            }),
        )
        .await
        .unwrap();

        mock.expect_called(1);
        let log = store
            .get_delivery_log("log_replay")
            .await
            .unwrap()
            .expect("log should remain stored");
        assert_eq!(log.response_status, Some(200));
        assert!(!log.is_dlq);
        assert!(log.last_error.is_none());

        let updated_sub = store
            .get_subscription("sub_failed")
            .await
            .unwrap()
            .expect("subscription should remain stored");
        assert_eq!(updated_sub.status, WebhookSubscriptionStatus::Active);
        assert_eq!(updated_sub.consecutive_failures, 0);
    }

    /// Issue #3062: each retry sends the same `webhook-id`, so a receiver can
    /// drop a duplicate.
    #[tokio::test]
    async fn webhook_retries_carry_identical_webhook_id() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/flaky")
            .post("/webhooks/flaky")
            .respond_with(500, serde_json::json!({ "error": "down" }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let sub = sample_subscription(
            "sub_flaky",
            "http://mock-receiver/webhooks/flaky",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub).await.unwrap();
        store
            .replace_delivery_log(sample_log("log_flaky", "sub_flaky"))
            .await
            .unwrap();

        let mut ids = Vec::new();
        for attempt in 1..=3_u32 {
            let result =
                deliver_webhook_job(state.clone(), serde_json::json!({ "log_id": "log_flaky" }))
                    .await;
            assert!(result.is_err(), "a 500 asks the job runtime to retry");
            let log = store.get_delivery_log("log_flaky").await.unwrap().unwrap();
            assert_eq!(log.attempt, attempt);
            let id = log
                .request_headers
                .get("webhook-id")
                .cloned()
                .expect("every attempt sends webhook-id");
            let timestamp = log
                .request_headers
                .get("webhook-timestamp")
                .expect("every attempt sends webhook-timestamp");
            let signature = &log.request_headers["Autumn-Signature"];
            assert!(signature.starts_with(&format!("t={timestamp},")));
            ids.push(id);
        }

        mock.expect_called(3);
        assert!(ids.iter().all(|id| id == "log_flaky"), "ids: {ids:?}");
    }

    /// Issue #3062: a relay re-send of one outbox message makes no second
    /// delivery once an attempt started, and enqueues again before that.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn outbox_resend_reuses_the_delivery_id() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);
        let delegated = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let delegate: WebhookDelegate = {
            let delegated = delegated.clone();
            Arc::new(move |_, _, log| {
                delegated.lock().unwrap().push(log.id);
                Box::pin(async { Ok(()) })
            })
        };
        state.insert_extension(WebhookDelegateExt(delegate));
        store
            .create_subscription(sample_subscription(
                "sub_outbox",
                "http://mock-receiver/hook",
                WebhookSubscriptionStatus::Active,
            ))
            .await
            .unwrap();
        let manager = state.extension::<WebhookOutboundManager>().unwrap();
        let payload = serde_json::json!({ "order_id": "ord_1" });

        for _ in 0..2 {
            manager
                .dispatch_for_message(&state, "msg-1", "orders.created", &payload)
                .await
                .unwrap();
        }
        let ids = delegated.lock().unwrap().clone();
        assert_eq!(ids.len(), 2, "no attempt yet: enqueue again");
        assert_eq!(
            ids[0], ids[1],
            "one delivery id per message and subscription"
        );
        assert_eq!(ids[0], delivery_id_for_message("msg-1", "sub_outbox"));
        assert_eq!(store.get_delivery_logs().await.unwrap().len(), 1);

        let mut log = store.get_delivery_log(&ids[0]).await.unwrap().unwrap();
        log.response_status = Some(200);
        store.replace_delivery_log(log).await.unwrap();
        manager
            .dispatch_for_message(&state, "msg-1", "orders.created", &payload)
            .await
            .unwrap();
        assert_eq!(
            delegated.lock().unwrap().len(),
            2,
            "an attempt started: skip"
        );

        manager
            .dispatch_for_message(&state, "msg-2", "orders.created", &payload)
            .await
            .unwrap();
        assert_ne!(
            delegated.lock().unwrap()[2],
            ids[0],
            "a new message gets a new id"
        );
    }

    /// The test vector of the Standard Webhooks spec.
    #[test]
    fn webhook_signature_matches_the_standard_webhooks_vector() {
        assert_eq!(
            standard_webhooks_signature(
                "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw",
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1_614_265_330,
                r#"{"test": 2432232314}"#,
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
        // A secret without the prefix signs with its raw bytes.
        assert_ne!(
            standard_webhooks_signature("raw-secret", "id", 1, "{}"),
            standard_webhooks_signature("other-secret", "id", 1, "{}")
        );
    }

    /// A second job for a delivered log (an outbox re-send) sends nothing.
    #[tokio::test]
    async fn duplicate_job_after_success_does_not_send_again() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/once")
            .post("/webhooks/once")
            .respond_with(200, serde_json::json!({ "ok": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);
        store
            .create_subscription(sample_subscription(
                "sub_once",
                "http://mock-receiver/webhooks/once",
                WebhookSubscriptionStatus::Active,
            ))
            .await
            .unwrap();
        store
            .replace_delivery_log(sample_log("log_once", "sub_once"))
            .await
            .unwrap();

        for _ in 0..2 {
            deliver_webhook_job(state.clone(), serde_json::json!({ "log_id": "log_once" }))
                .await
                .unwrap();
        }
        mock.expect_called(1);
        let log = store.get_delivery_log("log_once").await.unwrap().unwrap();
        assert_eq!(log.attempt, 1);
        assert!(log.request_headers["webhook-signature"].starts_with("v1,"));
    }

    /// From the outbox, a failed enqueue keeps the log pending, so the next
    /// relay send enqueues it. Outside the outbox, it goes to the DLQ.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn outbox_enqueue_failure_stays_retryable() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);
        store
            .create_subscription(sample_subscription(
                "sub_retry",
                "http://mock-receiver/hook",
                WebhookSubscriptionStatus::Active,
            ))
            .await
            .unwrap();
        let manager = state.extension::<WebhookOutboundManager>().unwrap();
        let payload = serde_json::json!({ "order_id": "ord_1" });

        // No job client: the enqueue fails.
        assert!(
            manager
                .dispatch_for_message(&state, "msg-r", "orders.created", &payload)
                .await
                .is_err()
        );
        let id = delivery_id_for_message("msg-r", "sub_retry");
        let log = store.get_delivery_log(&id).await.unwrap().unwrap();
        assert!(
            !log.is_dlq && log.last_error.is_none(),
            "the log stays pending"
        );

        // The relay sends again; now the enqueue works.
        let delegated = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let delegate: WebhookDelegate = {
            let delegated = delegated.clone();
            Arc::new(move |_, _, log| {
                delegated.lock().unwrap().push(log.id);
                Box::pin(async { Ok(()) })
            })
        };
        state.insert_extension(WebhookDelegateExt(delegate));
        manager
            .dispatch_for_message(&state, "msg-r", "orders.created", &payload)
            .await
            .unwrap();
        assert_eq!(*delegated.lock().unwrap(), [id]);
    }

    #[tokio::test]
    async fn replay_job_keeps_disabled_subscription_log_in_dlq() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/disabled")
            .post("/webhooks/disabled")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let sub = sample_subscription(
            "sub_disabled",
            "http://mock-receiver/webhooks/disabled",
            WebhookSubscriptionStatus::Disabled,
        );
        store.create_subscription(sub).await.unwrap();
        store
            .replace_delivery_log(sample_log("log_disabled_replay", "sub_disabled"))
            .await
            .unwrap();

        deliver_webhook_job(
            state,
            serde_json::json!({
                "log_id": "log_disabled_replay",
                "replay": true,
            }),
        )
        .await
        .unwrap();

        mock.expect_called(0);
        let log = store
            .get_delivery_log("log_disabled_replay")
            .await
            .unwrap()
            .expect("log should remain stored");
        assert!(log.is_dlq, "disabled replay must remain visible in DLQ");
        assert_eq!(log.last_error.as_deref(), Some("Subscription is disabled"));
        assert_eq!(log.response_status, None);
    }

    /// 🛡 Warden — SSRF via subscriber-chosen `target_url` (2026-09-03).
    ///
    /// `WebhookSubscription::target_url` is exactly the kind of destination
    /// `docs/guide/outbound-webhooks.md` describes as "a consumer's registered
    /// endpoint" — supplied by whoever registers the subscription, not chosen
    /// by the app. An attacker who can register (or edit) a subscription can
    /// point it at an internal service, the app's own DB host, or a cloud
    /// metadata endpoint (169.254.169.254); Autumn's own background job then
    /// makes the outbound call from inside the app's network. No app code in
    /// this path is doing anything the guide warns against — the guide's only
    /// stated security mechanism is the outbound HMAC signature, which says
    /// nothing about where the request is allowed to go.
    ///
    /// `127.0.0.1` stands in for that internal destination: it is on the
    /// framework's own SSRF deny-list ([`crate::http_client::is_blocked_ip`]),
    /// so a real local listener lets the test observe, without touching the
    /// network beyond loopback, whether the connection was ever attempted.
    #[tokio::test]
    async fn deliver_webhook_job_refuses_ssrf_target_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let port = listener
            .local_addr()
            .expect("listener has a local address")
            .port();
        let target_url = format!("http://127.0.0.1:{port}/hook");

        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        // Deliberately NOT registering an `HttpMockRegistryExt` — the mock
        // path short-circuits before the SSRF check runs (`send_recorded`
        // checks `self.mock.is_some()` first), so this test needs the real
        // send path to observe whether the connection was actually blocked.
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let sub = sample_subscription("sub_ssrf", &target_url, WebhookSubscriptionStatus::Active);
        store.create_subscription(sub).await.unwrap();
        store
            .replace_delivery_log(sample_log("log_ssrf", "sub_ssrf"))
            .await
            .unwrap();

        let accept = tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept());

        let job_result =
            deliver_webhook_job(state, serde_json::json!({ "log_id": "log_ssrf" })).await;

        assert!(
            accept.await.is_err(),
            "the listener standing in for an internal service must never see a \
             connection — a blocked destination has to be rejected before dial"
        );
        assert!(
            job_result.is_err(),
            "delivery to a blocked destination must not report success"
        );

        let log = store
            .get_delivery_log("log_ssrf")
            .await
            .unwrap()
            .expect("log should remain stored");
        assert!(
            log.last_error
                .as_deref()
                .is_some_and(|e| e.contains("SSRF")),
            "delivery log should record the SSRF block, got: {:?}",
            log.last_error
        );
        assert_eq!(log.response_status, None);
    }

    #[tokio::test]
    async fn self_contained_delivery_uses_latest_subscription_state() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let stale_mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/stale")
            .post("/webhooks/stale")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let stored_sub = sample_subscription(
            "sub_refresh",
            "http://mock-receiver/webhooks/current-disabled",
            WebhookSubscriptionStatus::Disabled,
        );
        store.create_subscription(stored_sub).await.unwrap();
        let stale_sub = sample_subscription(
            "sub_refresh",
            "http://mock-receiver/webhooks/stale",
            WebhookSubscriptionStatus::Active,
        );
        let log = sample_log("log_refresh", "sub_refresh");

        deliver_webhook_job(
            state,
            serde_json::json!({
                "subscription": stale_sub,
                "log": log,
            }),
        )
        .await
        .unwrap();

        stale_mock.expect_called(0);
        let stored = store
            .get_delivery_log("log_refresh")
            .await
            .unwrap()
            .expect("delivery log should exist");
        assert_eq!(stored.response_status, None);
        assert_eq!(
            stored.last_error.as_deref(),
            Some("Subscription is disabled")
        );
    }

    #[tokio::test]
    async fn dispatch_marks_log_dlq_when_fallback_enqueue_fails() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let manager = WebhookOutboundManager::new(store.clone()).with_initial_backoff_ms(1);
        let sub = sample_subscription(
            "sub_enqueue_missing",
            "http://mock-receiver/webhooks/enqueue-missing",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub).await.unwrap();

        let err = manager
            .dispatch(&state, "orders.created", &serde_json::json!({ "id": 42 }))
            .await
            .expect_err("dispatch should report the missing fallback job runtime");
        assert!(
            err.to_string().contains("not enqueued"),
            "error should describe the enqueue failure: {err}"
        );

        let logs = store.get_delivery_logs().await.unwrap();
        assert_eq!(logs.len(), 1);
        let log = &logs[0];
        assert!(
            log.is_dlq,
            "enqueue failure must leave a replayable DLQ record"
        );
        assert!(
            log.last_error
                .as_deref()
                .is_some_and(|msg| msg.contains("not enqueued")),
            "DLQ log should record enqueue failure: {:?}",
            log.last_error
        );
        assert_eq!(log.response_status, None);

        let sub = store
            .get_subscription("sub_enqueue_missing")
            .await
            .unwrap()
            .expect("subscription should remain stored");
        assert_eq!(sub.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn delivery_log_response_body_is_capped() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let large_body = "x".repeat(MAX_LOGGED_RESPONSE_BODY_BYTES + 1024);
        let _mock = mock_builder(
            registry.clone(),
            "http://mock-receiver/webhooks/large-error",
        )
        .post("/webhooks/large-error")
        .respond_with(500, serde_json::json!({ "error": large_body }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let sub = sample_subscription(
            "sub_large_error",
            "http://mock-receiver/webhooks/large-error",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub.clone()).await.unwrap();
        let mut log = sample_log("log_large_error", "sub_large_error");
        log.max_attempts = 1;

        deliver_webhook_job(
            state,
            serde_json::json!({
                "subscription": sub,
                "log": log,
            }),
        )
        .await
        .unwrap();

        let stored = store
            .get_delivery_log("log_large_error")
            .await
            .unwrap()
            .expect("delivery log should exist");
        let body = stored
            .response_body
            .expect("response body should be logged");
        assert!(
            body.len() <= MAX_LOGGED_RESPONSE_BODY_BYTES,
            "stored response body should be capped, got {} bytes",
            body.len()
        );
        assert!(body.ends_with("[truncated]"));
    }

    /// A log written with a limit of 10 moves to the DLQ at the manager's
    /// lower limit of 2, so the job engine cannot run out first.
    #[tokio::test]
    async fn a_lowered_limit_moves_an_old_log_to_the_dlq() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let _mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/lowered")
            .post("/webhooks/lowered")
            .respond_with(500, serde_json::json!({}));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, 2);

        let sub = sample_subscription(
            "sub_lowered",
            "http://mock-receiver/webhooks/lowered",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub.clone()).await.unwrap();
        let mut log = sample_log("log_lowered", "sub_lowered");
        log.attempt = 2;
        log.max_attempts = 10;

        deliver_webhook_job(
            state,
            serde_json::json!({ "subscription": sub, "log": log }),
        )
        .await
        .expect("the last attempt settles the job");

        let stored = store
            .get_delivery_log("log_lowered")
            .await
            .unwrap()
            .expect("delivery log should exist");
        assert!(stored.is_dlq, "attempt 2 of a limit of 2 goes to the DLQ");
    }

    /// A retry that waits while a deploy lowers the limit goes to the DLQ
    /// without another request. Attempt 2 failed under a limit of 10; the
    /// manager's limit is now 2, so attempt 3 must not be sent.
    #[tokio::test]
    async fn a_lowered_limit_stops_a_waiting_retry_before_it_is_sent() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/waiting")
            .post("/webhooks/waiting")
            .respond_with(500, serde_json::json!({}));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, 2);

        let sub = sample_subscription(
            "sub_waiting",
            "http://mock-receiver/webhooks/waiting",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub.clone()).await.unwrap();
        let mut log = sample_log("log_waiting", "sub_waiting");
        log.attempt = 2;
        log.max_attempts = 10;
        log.last_error = Some("HTTP 500".to_owned());

        deliver_webhook_job(
            state,
            serde_json::json!({ "subscription": sub, "log": log }),
        )
        .await
        .expect("the exhausted retry settles the job");

        assert_eq!(mock.call_count(), 0, "attempt 3 must not be sent");
        let stored = store
            .get_delivery_log("log_waiting")
            .await
            .unwrap()
            .expect("delivery log should exist");
        assert!(stored.is_dlq, "the waiting retry goes to the DLQ");
        assert_eq!(stored.attempt, 2, "no attempt 3 is started");
        assert_eq!(
            stored.last_error.as_deref(),
            Some("HTTP 500"),
            "the last real error is kept"
        );
    }

    /// A manual DLQ replay is not limited: an exhausted log is sent again.
    #[tokio::test]
    async fn a_replay_of_an_exhausted_log_is_still_sent() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/exhausted")
            .post("/webhooks/exhausted")
            .respond_with(200, serde_json::json!({}));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, 2);

        store
            .create_subscription(sample_subscription(
                "sub_exhausted",
                "http://mock-receiver/webhooks/exhausted",
                WebhookSubscriptionStatus::Active,
            ))
            .await
            .unwrap();
        let mut log = sample_log("log_exhausted", "sub_exhausted");
        log.attempt = 2;
        log.max_attempts = 2;
        log.last_error = Some("HTTP 500".to_owned());
        log.is_dlq = true;
        store.replace_delivery_log(log).await.unwrap();

        deliver_webhook_job(
            state,
            serde_json::json!({ "log_id": "log_exhausted", "replay": true }),
        )
        .await
        .expect("the replay is delivered");

        assert_eq!(mock.call_count(), 1, "the replay sends the request");
    }

    struct CountingReplacementStore {
        log_delivery_calls: AtomicUsize,
    }

    impl CountingReplacementStore {
        fn new() -> Self {
            Self {
                log_delivery_calls: AtomicUsize::new(0),
            }
        }

        fn log_delivery_count(&self) -> usize {
            self.log_delivery_calls.load(Ordering::SeqCst)
        }
    }

    impl OutboundWebhookHandler for CountingReplacementStore {
        fn get_subscriptions(
            &self,
            _topic: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookSubscription>>> + Send>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn log_delivery(
            &self,
            _log: WebhookDeliveryLog,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
            self.log_delivery_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn replace_delivery_log(
            &self,
            _log: WebhookDeliveryLog,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
            Box::pin(async { Ok(()) })
        }

        fn get_subscription(
            &self,
            _id: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookSubscription>>> + Send>>
        {
            Box::pin(async { Ok(None) })
        }

        fn get_delivery_log(
            &self,
            _id: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookDeliveryLog>>> + Send>>
        {
            Box::pin(async { Ok(None) })
        }
    }

    #[tokio::test]
    async fn replace_delivery_log_is_not_a_delivery_outcome() {
        let store = CountingReplacementStore::new();
        let mut log = sample_log("log_replace", "sub_replace");
        log.response_status = Some(500);
        log.last_error = Some("server returned status: 500 Internal Server Error".to_owned());
        log.is_dlq = true;

        store.replace_delivery_log(log).await.unwrap();

        assert_eq!(
            store.log_delivery_count(),
            0,
            "plain delivery-log replacement must not call log_delivery"
        );
    }

    struct ResetFailingStore {
        inner: InMemoryOutboundWebhookHandler,
    }

    impl ResetFailingStore {
        fn new() -> Self {
            Self {
                inner: InMemoryOutboundWebhookHandler::new(),
            }
        }

        async fn create_subscription(&self, sub: WebhookSubscription) {
            self.inner.create_subscription(sub).await.unwrap();
        }

        async fn delivery_log(&self, id: &str) -> WebhookDeliveryLog {
            self.inner
                .get_delivery_log(id)
                .await
                .unwrap()
                .expect("delivery log should exist")
        }
    }

    impl OutboundWebhookHandler for ResetFailingStore {
        fn get_subscriptions(
            &self,
            topic: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookSubscription>>> + Send>> {
            <InMemoryOutboundWebhookHandler as OutboundWebhookHandler>::get_subscriptions(
                &self.inner,
                topic,
            )
        }

        fn log_delivery(
            &self,
            log: WebhookDeliveryLog,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
            <InMemoryOutboundWebhookHandler as OutboundWebhookHandler>::log_delivery(
                &self.inner,
                log,
            )
        }

        fn replace_delivery_log(
            &self,
            log: WebhookDeliveryLog,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
            <InMemoryOutboundWebhookHandler as OutboundWebhookHandler>::replace_delivery_log(
                &self.inner,
                log,
            )
        }

        fn get_subscription(
            &self,
            id: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookSubscription>>> + Send>>
        {
            <InMemoryOutboundWebhookHandler as OutboundWebhookHandler>::get_subscription(
                &self.inner,
                id,
            )
        }

        fn get_delivery_log(
            &self,
            id: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookDeliveryLog>>> + Send>>
        {
            <InMemoryOutboundWebhookHandler as OutboundWebhookHandler>::get_delivery_log(
                &self.inner,
                id,
            )
        }

        fn reset_subscription_failures(
            &self,
            _id: &str,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
            Box::pin(async {
                Err(AutumnError::internal_server_error_msg(
                    "reset backend unavailable",
                ))
            })
        }
    }

    #[tokio::test]
    async fn successful_delivery_does_not_retry_when_failure_reset_fails() {
        let state = AppState::for_test();
        let store = Arc::new(ResetFailingStore::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/success")
            .post("/webhooks/success")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1, DEFAULT_WEBHOOK_MAX_ATTEMPTS);

        let sub = sample_subscription(
            "sub_success",
            "http://mock-receiver/webhooks/success",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub.clone()).await;
        let log = sample_log("log_success", "sub_success");

        deliver_webhook_job(
            state,
            serde_json::json!({
                "subscription": sub,
                "log": log,
            }),
        )
        .await
        .expect("accepted webhook delivery must not be retried because counter reset failed");

        mock.expect_called(1);
        let persisted = store.delivery_log("log_success").await;
        assert_eq!(persisted.response_status, Some(200));
        assert!(persisted.last_error.is_none());
    }

    #[tokio::test]
    async fn webhook_manager_uses_http_client_config_base_urls() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let plugin = OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(1);
        let mut config = crate::config::AutumnConfig::default();
        config.http.client.base_urls.insert(
            "hook-service".to_owned(),
            "http://mock-receiver/base".to_owned(),
        );

        let mut app_builder = crate::test::TestApp::new().config(config).plugin(plugin);
        let mock = app_builder
            .http_mock("hook-service")
            .post("/base/hook-service")
            .respond_with(200, serde_json::json!({ "received": true }));
        let app = app_builder.build();
        let state = app.state();

        let sub = sample_subscription(
            "sub_config",
            "hook-service",
            WebhookSubscriptionStatus::Active,
        );
        store.create_subscription(sub.clone()).await.unwrap();
        let log = sample_log("log_config", "sub_config");

        deliver_webhook_job(
            state.clone(),
            serde_json::json!({
                "subscription": sub,
                "log": log,
            }),
        )
        .await
        .unwrap();

        mock.expect_called(1);
        crate::job::clear_global_job_client();
    }
}
