#![allow(
    clippy::significant_drop_tightening,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
//! Outbound signed webhook delivery with retries, DLQ, and subscription management.

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSubscription {
    pub id: String,
    pub target_url: String,
    pub event_topics: Vec<String>,
    pub secret: String,
    pub status: WebhookSubscriptionStatus,
    pub consecutive_failures: u32,
}

/// A structured log of an outbound webhook delivery attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl WebhookOutboundManager {
    /// Create a new webhook manager with a handler.
    pub fn new(handler: Arc<dyn OutboundWebhookHandler>) -> Self {
        Self {
            handler,
            client: Client::new(),
            initial_backoff_ms: 1000,
        }
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
        let serialized = serde_json::to_string(payload).map_err(|e| {
            AutumnError::internal_server_error_msg(format!("failed to serialize payload: {e}"))
        })?;

        let mut errors = Vec::new();
        let subs = self.handler.get_subscriptions(topic).await?;
        for sub in subs {
            if sub.status == WebhookSubscriptionStatus::Disabled {
                continue;
            }

            let log_id = uuid::Uuid::new_v4().to_string();
            let log = WebhookDeliveryLog {
                id: log_id.clone(),
                subscription_id: sub.id.clone(),
                topic: topic.to_owned(),
                payload: serialized.clone(),
                request_headers: HashMap::new(),
                response_status: None,
                response_body: None,
                elapsed_ms: 0,
                attempt: 1,
                max_attempts: 5,
                is_dlq: false,
                last_error: None,
                timestamp: Utc::now(),
            };

            // Register the initial attempt in local storage
            if let Err(e) = self.handler.log_delivery(log.clone()).await {
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
                        errors.push(
                            self.record_delivery_enqueue_failure(log, e.to_string())
                                .await,
                        );
                    }
                } else {
                    errors.push(
                        self.record_delivery_enqueue_failure(
                            log,
                            "Global job client is unavailable; fallback webhook delivery job not enqueued"
                                .to_owned(),
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

    async fn record_delivery_enqueue_failure(
        &self,
        mut log: WebhookDeliveryLog,
        message: String,
    ) -> AutumnError {
        log.is_dlq = true;
        log.last_error = Some(message.clone());
        log.timestamp = Utc::now();

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

fn install_outbound_webhook_manager(
    state: &AppState,
    store: Arc<dyn OutboundWebhookHandler>,
    initial_backoff_ms: u64,
) {
    let manager = WebhookOutboundManager::new(store)
        .with_initial_backoff_ms(initial_backoff_ms)
        .with_client_from_state(state);
    state.insert_extension(manager);
}

/// Asynchronous background job that delivers a webhook payload (legacy fallback).
#[must_use]
#[allow(clippy::redundant_closure_for_method_calls, clippy::too_many_lines)]
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

            // If this log has already been attempted (i.e. is running a retry from the job runner),
            // increment the attempt counter and write the pre-send log.
            if log.response_status.is_some() || log.last_error.is_some() {
                log.attempt = log.attempt.saturating_add(1);
                log.response_status = None;
                log.response_body = None;
                log.last_error = None;
                manager.store().log_delivery(log.clone()).await?;
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

            // If this log has already been attempted (i.e. is running a retry from the job runner),
            // increment the attempt counter and write the pre-send log.
            if log.response_status.is_some() || log.last_error.is_some() {
                log.attempt = log.attempt.saturating_add(1);
                log.response_status = None;
                log.response_body = None;
                log.last_error = None;
                manager.store().log_delivery(log.clone()).await?;
            }

            // Load latest subscription state to respect emergency rotations/disable
            let sub = load_current_subscription(&manager, &log).await?;
            (sub, log)
        };

        if sub.status == WebhookSubscriptionStatus::Disabled {
            tracing::info!(subscription_id = %sub.id, "Webhook subscription is disabled; skipping delivery");
            log.last_error = Some("Subscription is disabled".to_owned());
            log.timestamp = Utc::now();
            if is_replay {
                log.is_dlq = true;
            }
            manager.store().log_delivery(log).await?;
            return Ok(());
        }

        if sub.status == WebhookSubscriptionStatus::Failed && !is_replay {
            tracing::info!(subscription_id = %sub.id, "Webhook subscription has failed; skipping delivery");
            log.last_error = Some("Subscription has failed due to consecutive errors".to_owned());
            log.timestamp = Utc::now();
            manager.store().log_delivery(log).await?;
            return Ok(());
        }
        if sub.status == WebhookSubscriptionStatus::Failed {
            tracing::info!(subscription_id = %sub.id, "Replaying webhook delivery for failed subscription");
        }

        // Stripe-style payload signing: t=<timestamp>,v1=<signature>
        let timestamp = Utc::now().timestamp();
        let signing_payload = format!("{timestamp}.{}", log.payload);
        let signature = crate::security::config::hmac_sha256_hex(
            sub.secret.as_bytes(),
            signing_payload.as_bytes(),
        );
        let signature_header = format!("t={timestamp},v1={signature}");

        let mut request_headers = HashMap::new();
        request_headers.insert("Content-Type".to_owned(), "application/json".to_owned());
        request_headers.insert("Autumn-Signature".to_owned(), signature_header.clone());

        let start = std::time::Instant::now();
        let req = manager
            .client
            .named(&sub.target_url)
            .post(&sub.target_url)
            .header("Content-Type", "application/json")
            .header("Autumn-Signature", signature_header)
            .text_body(log.payload.clone());

        let response = req.send().await;
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        tracing::debug!(
            log_id = %log.id,
            status = ?response.as_ref().map(|r| r.status()),
            "deliver_webhook_job: webhook HTTP request finished"
        );

        log.elapsed_ms = elapsed;
        log.timestamp = Utc::now();
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
                    if log.attempt < log.max_attempts {
                        manager.store().log_delivery(log.clone()).await?;
                    }
                    handle_delivery_failure(&manager, &sub, log, status_err).await
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                log.last_error = Some(error_str.clone());
                if log.attempt < log.max_attempts {
                    manager.store().log_delivery(log.clone()).await?;
                }
                handle_delivery_failure(&manager, &sub, log, error_str).await
            }
        }
    })
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
    if log.attempt < log.max_attempts {
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
    store: Arc<dyn OutboundWebhookHandler>,
    initial_backoff_ms: u64,
}

impl OutboundWebhookPlugin {
    /// Create a new outbound webhook plugin using the specified store.
    #[must_use]
    pub fn new(store: Arc<dyn OutboundWebhookHandler>) -> Self {
        Self {
            store,
            initial_backoff_ms: 1000,
        }
    }

    /// Override the initial backoff retry delay.
    #[must_use]
    pub const fn with_initial_backoff_ms(mut self, ms: u64) -> Self {
        self.initial_backoff_ms = ms;
        self
    }
}

impl crate::plugin::Plugin for OutboundWebhookPlugin {
    fn build(self, app: crate::app::AppBuilder) -> crate::app::AppBuilder {
        let store = self.store;
        let initial_backoff_ms = self.initial_backoff_ms;

        app.state_initializer(move |state| {
            install_outbound_webhook_manager(state, store.clone(), initial_backoff_ms);
        })
        .jobs(vec![crate::job::JobInfo {
            name: "autumn_webhook_delivery".to_string(),
            max_attempts: 10, // Retries are handled durably via the background job engine
            initial_backoff_ms,
            queue: "default".to_string(),
            uniqueness: None,
            concurrency: None,
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

    #[tokio::test]
    async fn replay_job_sends_failed_subscription_instead_of_skipping() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/replay")
            .post("/webhooks/replay")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1);

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

    #[tokio::test]
    async fn replay_job_keeps_disabled_subscription_log_in_dlq() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/disabled")
            .post("/webhooks/disabled")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1);

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

    #[tokio::test]
    async fn self_contained_delivery_uses_latest_subscription_state() {
        let state = AppState::for_test();
        let store = Arc::new(InMemoryOutboundWebhookHandler::new());
        let registry = Arc::new(MockRegistry::new());
        let stale_mock = mock_builder(registry.clone(), "http://mock-receiver/webhooks/stale")
            .post("/webhooks/stale")
            .respond_with(200, serde_json::json!({ "received": true }));
        state.insert_extension(HttpMockRegistryExt(registry));
        install_outbound_webhook_manager(&state, store.clone(), 1);

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
        install_outbound_webhook_manager(&state, store.clone(), 1);

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
        install_outbound_webhook_manager(&state, store.clone(), 1);

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
