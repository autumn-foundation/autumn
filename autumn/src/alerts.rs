//! Operator alerts connect Autumn's built-in failure signals to its existing
//! delivery channels — **with zero application code**.
//!
//! The signals reach the configured mailer and the signed outbound webhook
//! behind a small set of `[alerts]` config keys.
//!
//! # What this is
//!
//! Autumn already knows when things go wrong: a job is dead-lettered, a health
//! indicator reports `Down`, the 5xx rate spikes, or a framework-scheduled task
//! fails. Historically an operator had to wire each of those signals to a
//! notification sink by hand. This module closes that gap: provide an operator
//! email and/or a webhook URL in `[alerts]` and every built-in condition is
//! delivered to you, deduplicated, with a recovery notice when it clears.
//!
//! ## Built-in alertable conditions
//!
//! | Condition | Fires when | Where to look |
//! |-----------|------------|---------------|
//! | [`AlertCondition::DeadLetteredJob`] | a background job exhausts its retries and is dead-lettered | `/actuator/jobs` |
//! | [`AlertCondition::HealthIndicatorDown`] | a registered health indicator reports `Down` past the grace period | `/actuator/health` |
//! | [`AlertCondition::HighErrorRate`] | the rolling 5xx rate crosses the configured threshold | `/actuator/metrics` |
//! | [`AlertCondition::ScheduledTaskFailure`] | a framework-scheduled task (backup, cert-renewal, cron/fixed-delay) fails | `/actuator/tasks` |
//!
//! # Delivery is a trait (extension point)
//!
//! Every destination implements [`AlertChannel`]. The [`Alerter`] holds a
//! fan-out list of channels and delivers each alert to all of them on a
//! detached task — so a slow or unreachable channel never adds latency to a
//! request or blocks the others. Two built-in channels ship:
//! [`MailAlertChannel`] (reuses the app's [`Mailer`](crate::mail::Mailer)) and
//! [`WebhookAlertChannel`] (a signed, Stripe-style HMAC POST reusing the same
//! signing scheme as [`webhook_outbound`](crate::webhook_outbound)).
//!
//! **Design intent (follow-up #1630):** PagerDuty / Slack / Discord transports
//! are added purely by implementing [`AlertChannel`] and registering them with
//! [`AppBuilder::with_alert_channel`](crate::app::AppBuilder::with_alert_channel);
//! the core never changes. That is why every [`Alert`] carries a **stable dedup
//! key** ([`Alert::dedup_key`], which PagerDuty correlates on), a **severity
//! class** ([`Alert::severity`]), and a **trigger vs resolve** discriminator
//! ([`Alert::event`]).
//!
//! # Deduplication and recovery
//!
//! A sustained or repeating condition does **not** produce one notification per
//! occurrence. [`AlertDeduplicator`] bounds notifications to **at most one per
//! condition per dedup window** (default 15 minutes); the condition re-notifies
//! once per window while it persists. When a previously-alerted condition
//! clears, a single [`AlertEventKind::Resolve`] recovery notification is sent.
//!
//! # Fail-safe
//!
//! Delivery is best-effort and off the request path: if a channel is
//! unreachable the app keeps serving, the failure is logged, and no latency is
//! added. See [`AppBuilder::with_alert_channel`](crate::app::AppBuilder::with_alert_channel)
//! and `docs/guide/operator-alerts.md` for the full guide.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

// ── Core value types ────────────────────────────────────────────────────────

/// The built-in condition that produced an [`Alert`].
///
/// The condition determines the stable dedup-key prefix and the "where to look
/// next" actuator pointer. New transports (#1630) match on this to route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlertCondition {
    /// A background job exhausted its retries and was dead-lettered.
    DeadLetteredJob,
    /// A registered health indicator reported `Down` past the grace period.
    HealthIndicatorDown,
    /// The rolling 5xx error rate crossed the configured threshold.
    HighErrorRate,
    /// A framework-scheduled task (cron or fixed-delay) failed.
    ScheduledTaskFailure,
}

impl AlertCondition {
    /// A short, stable machine name for this condition (used in dedup keys and
    /// webhook payloads).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeadLetteredJob => "dead_lettered_job",
            Self::HealthIndicatorDown => "health_indicator_down",
            Self::HighErrorRate => "high_error_rate",
            Self::ScheduledTaskFailure => "scheduled_task_failure",
        }
    }

    /// The actuator endpoint an operator should consult for this condition,
    /// under the **default** actuator prefix (`/actuator`).
    ///
    /// This is the builder default. When the app configures a custom
    /// `[actuator] prefix`, the emitted alert's `where_to_look` is rebuilt from
    /// the effective prefix (see [`actuator_where_to_look`]) so it points at the
    /// real endpoint rather than a `/actuator/*` 404. Keep the suffixes here in
    /// sync with [`Self::actuator_suffix`].
    #[must_use]
    pub const fn where_to_look(self) -> &'static str {
        match self {
            Self::DeadLetteredJob => "/actuator/jobs",
            Self::HealthIndicatorDown => "/actuator/health",
            Self::HighErrorRate => "/actuator/metrics",
            Self::ScheduledTaskFailure => "/actuator/tasks",
        }
    }

    /// The actuator endpoint suffix (path *below* the actuator prefix) an
    /// operator should consult for this condition. Combined with the effective
    /// prefix by [`actuator_where_to_look`].
    const fn actuator_suffix(self) -> &'static str {
        match self {
            Self::DeadLetteredJob => "/jobs",
            Self::HealthIndicatorDown => "/health",
            Self::HighErrorRate => "/metrics",
            Self::ScheduledTaskFailure => "/tasks",
        }
    }
}

/// Build the actuator `where_to_look` pointer for `condition` under the
/// effective actuator `prefix`.
///
/// Uses the same path builder as the router
/// ([`actuator_route_path`](crate::actuator::actuator_route_path)) so the
/// derived path matches the mounted endpoint exactly — including prefix
/// normalization (leading slash added, trailing slash trimmed, `/` or empty
/// treated as root) — and never yields `//` or a missing slash. With the
/// default prefix (`/actuator`) this reproduces
/// [`AlertCondition::where_to_look`] byte-for-byte.
fn actuator_where_to_look(prefix: &str, condition: AlertCondition) -> String {
    crate::actuator::actuator_route_path(prefix, condition.actuator_suffix())
}

/// Severity class of an [`Alert`]. External routers (`PagerDuty` etc.) map this
/// onto their own severity/priority taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AlertSeverity {
    /// A condition is firing and needs operator attention.
    Critical,
    /// A previously-firing condition has recovered (informational).
    Recovery,
}

/// Whether this alert opens (trigger) or closes (resolve) a condition. This is
/// the field an incident manager keys on to auto-resolve a correlated alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AlertEventKind {
    /// The condition started (or is still) firing.
    Trigger,
    /// The condition cleared; correlated triggers may be auto-resolved.
    Resolve,
}

/// A single operator alert.
///
/// Carries everything AC #4 requires: **what** failed ([`title`](Self::title) /
/// [`summary`](Self::summary)), **when** ([`timestamp`](Self::timestamp)), on
/// **which host/replica** ([`host`](Self::host)), and **where to look next**
/// ([`where_to_look`](Self::where_to_look)) — plus the machine-routing fields
/// ([`dedup_key`](Self::dedup_key), [`severity`](Self::severity),
/// [`event`](Self::event)).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Alert {
    /// Stable key identifying the *condition instance* (e.g. the specific job
    /// or indicator). Identical across trigger and its recovery so an external
    /// system can correlate them. Never contains a timestamp.
    pub dedup_key: String,
    /// The built-in condition family.
    pub condition: AlertCondition,
    /// Severity class.
    pub severity: AlertSeverity,
    /// Trigger vs resolve.
    pub event: AlertEventKind,
    /// One-line human summary of what failed.
    pub title: String,
    /// Longer human-readable detail (the underlying error, counts, etc.).
    pub summary: String,
    /// When the alert was produced (UTC).
    pub timestamp: DateTime<Utc>,
    /// Host / replica identity the alert originated from.
    pub host: String,
    /// Where the operator should look next (an actuator endpoint or a log
    /// correlation id).
    pub where_to_look: String,
    /// Extra structured context (job name, error string, rates, …).
    pub details: HashMap<String, String>,
}

impl Alert {
    /// Build a trigger alert for `condition` with a stable dedup `key`.
    #[must_use]
    pub fn trigger(condition: AlertCondition, key: impl Into<String>) -> AlertBuilder {
        AlertBuilder::new(condition, key.into(), AlertEventKind::Trigger)
    }

    /// Build a recovery (resolve) alert for `condition` with the same stable
    /// dedup `key` its trigger used.
    #[must_use]
    pub fn recovery(condition: AlertCondition, key: impl Into<String>) -> AlertBuilder {
        AlertBuilder::new(condition, key.into(), AlertEventKind::Resolve)
    }
}

/// Builder for an [`Alert`]. Fills host, timestamp, severity, and the
/// where-to-look pointer from sensible defaults.
#[derive(Debug, Clone)]
pub struct AlertBuilder {
    alert: Alert,
}

impl AlertBuilder {
    fn new(condition: AlertCondition, dedup_key: String, event: AlertEventKind) -> Self {
        let severity = match event {
            AlertEventKind::Trigger => AlertSeverity::Critical,
            AlertEventKind::Resolve => AlertSeverity::Recovery,
        };
        Self {
            alert: Alert {
                dedup_key,
                condition,
                severity,
                event,
                title: String::new(),
                summary: String::new(),
                timestamp: Utc::now(),
                host: host_id(),
                where_to_look: condition.where_to_look().to_owned(),
                details: HashMap::new(),
            },
        }
    }

    /// Set the one-line title.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.alert.title = title.into();
        self
    }

    /// Set the longer human summary.
    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.alert.summary = summary.into();
        self
    }

    /// Override the "where to look next" pointer (defaults to the condition's
    /// actuator endpoint). Use this to attach a log correlation id.
    #[must_use]
    pub fn where_to_look(mut self, where_to_look: impl Into<String>) -> Self {
        self.alert.where_to_look = where_to_look.into();
        self
    }

    /// Add a structured detail field.
    #[must_use]
    pub fn detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.alert.details.insert(key.into(), value.into());
        self
    }

    /// Finish building.
    #[must_use]
    pub fn build(self) -> Alert {
        self.alert
    }
}

/// Resolve the host/replica identity to stamp on every alert.
///
/// Prefers an explicit `AUTUMN_REPLICA_ID`, then the container/host `HOSTNAME`,
/// falling back to `"unknown-host"`.
#[must_use]
pub fn host_id() -> String {
    std::env::var("AUTUMN_REPLICA_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "unknown-host".to_owned())
}

// ── Delivery trait ──────────────────────────────────────────────────────────

/// The future returned by [`AlertChannel::deliver`].
pub type AlertDeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), AlertDeliveryError>> + Send + 'a>>;

/// A delivery failure. Channels return this so the [`Alerter`] can log it; a
/// failure never propagates to the request path.
#[derive(Debug, thiserror::Error)]
#[error("alert delivery via {channel} failed: {message}")]
pub struct AlertDeliveryError {
    /// Name of the channel that failed.
    pub channel: &'static str,
    /// Human-readable failure detail.
    pub message: String,
}

impl AlertDeliveryError {
    /// Construct a delivery error for `channel`.
    #[must_use]
    pub fn new(channel: &'static str, message: impl Into<String>) -> Self {
        Self {
            channel,
            message: message.into(),
        }
    }
}

/// A destination that operator alerts are delivered to.
///
/// This is the framework's extension point for #1630: implement it for
/// `PagerDuty`, Slack, Discord, or any sink and register it with
/// [`AppBuilder::with_alert_channel`](crate::app::AppBuilder::with_alert_channel).
/// Implementations MUST be non-blocking and swallow nothing silently — return
/// [`AlertDeliveryError`] so the framework can log it. Delivery runs on a
/// detached task, so an implementation that is slow or panics can never affect
/// a live request.
pub trait AlertChannel: Send + Sync + 'static {
    /// A short static name for logs (e.g. `"mail"`, `"webhook"`).
    fn name(&self) -> &'static str;

    /// Deliver `alert` to this channel.
    fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a>;
}

// ── Deduplication ───────────────────────────────────────────────────────────

/// Decision returned by the [`AlertDeduplicator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupDecision {
    /// Deliver this alert.
    Send,
    /// Suppress this alert (a bounded duplicate, or a recovery for something
    /// that was never alerted).
    Suppress,
}

impl DedupDecision {
    /// Whether the alert should be delivered.
    #[must_use]
    pub const fn should_send(self) -> bool {
        matches!(self, Self::Send)
    }
}

#[derive(Debug, Clone)]
struct KeyState {
    /// Whether a trigger is currently outstanding (used to gate recovery).
    active: bool,
    /// When we last *delivered* a trigger for this key.
    last_sent: DateTime<Utc>,
}

/// Bounds alert volume for sustained/repeating conditions and gates recovery
/// notifications.
///
/// Policy (AC #3): the first occurrence of a condition alerts immediately;
/// further occurrences within `window` are suppressed. After `window` elapses
/// while the condition still fires, exactly one re-notification is allowed — so
/// the steady-state rate is bounded to **at most one notification per condition
/// per window**, never one-per-occurrence. A recovery is delivered exactly once
/// when a previously-alerted key clears, and never for a key that never fired.
#[derive(Debug)]
pub struct AlertDeduplicator {
    window: chrono::Duration,
    keys: HashMap<String, KeyState>,
}

impl AlertDeduplicator {
    /// Create a deduplicator with the given re-notification window.
    #[must_use]
    pub fn new(window: std::time::Duration) -> Self {
        Self {
            window: chrono::Duration::from_std(window)
                .unwrap_or_else(|_| chrono::Duration::seconds(900)),
            keys: HashMap::new(),
        }
    }

    /// Decide whether a trigger for `key` at `now` should be delivered.
    pub fn on_trigger(&mut self, key: &str, now: DateTime<Utc>) -> DedupDecision {
        match self.keys.get_mut(key) {
            Some(state) if state.active && (now - state.last_sent) < self.window => {
                // Still firing, still inside the window: bounded suppression.
                DedupDecision::Suppress
            }
            Some(state) => {
                // Either it had recovered, or the window has elapsed: re-notify.
                state.active = true;
                state.last_sent = now;
                DedupDecision::Send
            }
            None => {
                self.keys.insert(
                    key.to_owned(),
                    KeyState {
                        active: true,
                        last_sent: now,
                    },
                );
                DedupDecision::Send
            }
        }
    }

    /// Decide whether a recovery for `key` should be delivered. Only keys with
    /// an outstanding (delivered) trigger recover; the key is then cleared so a
    /// future trigger alerts immediately.
    pub fn on_resolve(&mut self, key: &str) -> DedupDecision {
        match self.keys.get_mut(key) {
            Some(state) if state.active => {
                state.active = false;
                DedupDecision::Send
            }
            _ => DedupDecision::Suppress,
        }
    }
}

// ── Alerter ─────────────────────────────────────────────────────────────────

/// Tunables the [`Alerter`] and its background evaluation loop read.
#[derive(Debug, Clone)]
pub struct AlerterSettings {
    /// Dedup / re-notification window.
    pub dedup_window: std::time::Duration,
    /// How long an indicator must stay `Down` before it alerts (condition b).
    pub health_grace: std::time::Duration,
    /// 5xx fraction (of requests in the sample window) that trips the alert.
    pub error_rate_threshold: f64,
    /// Minimum requests in the sample window before the rate is evaluated.
    pub error_rate_min_requests: u64,
    /// Background evaluation cadence (conditions b and c).
    pub eval_interval: std::time::Duration,
    /// The effective actuator URL prefix (`[actuator] prefix`, default
    /// `/actuator`). Every alert's `where_to_look` pointer is built from this so
    /// operators are sent to the real actuator endpoint even when the prefix is
    /// customized. Stored raw; normalization happens in [`actuator_where_to_look`].
    pub actuator_prefix: String,
}

impl AlerterSettings {
    fn from_config(config: &AlertConfig, actuator_prefix: String) -> Self {
        Self {
            dedup_window: std::time::Duration::from_secs(config.dedup_window_secs.max(1)),
            health_grace: std::time::Duration::from_secs(config.health_grace_secs),
            error_rate_threshold: config.error_rate_threshold,
            error_rate_min_requests: config.error_rate_min_requests.max(1),
            eval_interval: std::time::Duration::from_secs(config.eval_interval_secs.max(1)),
            actuator_prefix,
        }
    }
}

struct AlerterInner {
    channels: Vec<Arc<dyn AlertChannel>>,
    dedup: Mutex<AlertDeduplicator>,
    settings: AlerterSettings,
}

/// Runtime fan-out hub for operator alerts, installed as an [`AppState`]
/// extension so the built-in condition hooks can reach it.
///
/// Cloning is cheap (shared `Arc`). Use [`notify`](Self::notify) /
/// [`recover`](Self::recover) to emit alerts; both deduplicate and dispatch on
/// a detached task so nothing blocks the caller.
#[derive(Clone)]
pub struct Alerter {
    inner: Arc<AlerterInner>,
}

impl Alerter {
    /// Build an alerter from a channel list and settings.
    #[must_use]
    pub fn new(channels: Vec<Arc<dyn AlertChannel>>, settings: AlerterSettings) -> Self {
        let dedup = AlertDeduplicator::new(settings.dedup_window);
        Self {
            inner: Arc::new(AlerterInner {
                channels,
                dedup: Mutex::new(dedup),
                settings,
            }),
        }
    }

    /// Whether any channel is registered. When false, emitting is a no-op.
    #[must_use]
    pub fn has_channels(&self) -> bool {
        !self.inner.channels.is_empty()
    }

    pub(crate) fn settings(&self) -> &AlerterSettings {
        &self.inner.settings
    }

    /// Emit a trigger alert. Deduplicated; delivered on a detached task.
    /// Returns whether the alert passed the dedup gate (was dispatched).
    #[must_use]
    pub fn notify(&self, alert: Alert) -> bool {
        let decision = self
            .inner
            .dedup
            .lock()
            .map_or(DedupDecision::Send, |mut d| {
                d.on_trigger(&alert.dedup_key, alert.timestamp)
            });
        if decision.should_send() {
            self.dispatch(alert);
            true
        } else {
            false
        }
    }

    /// Emit a recovery alert if `dedup_key` had an outstanding trigger. The
    /// caller supplies the fully-built recovery [`Alert`]. Returns whether a
    /// recovery was actually dispatched.
    #[must_use]
    pub fn recover(&self, alert: Alert) -> bool {
        let decision = self
            .inner
            .dedup
            .lock()
            .map_or(DedupDecision::Suppress, |mut d| {
                d.on_resolve(&alert.dedup_key)
            });
        if decision.should_send() {
            self.dispatch(alert);
            true
        } else {
            false
        }
    }

    /// Fan the alert out to every channel on a detached task (fail-safe).
    fn dispatch(&self, alert: Alert) {
        if self.inner.channels.is_empty() {
            return;
        }
        // Dispatch is best-effort: with a Tokio runtime handle we spawn one
        // detached task *per channel* so a slow or hanging channel never blocks
        // delivery to the others; with no handle available (unlikely from a hook
        // site) the alert is logged and skipped, never blocking the caller.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no tokio runtime available for alert dispatch; skipping");
            return;
        };
        let alert = Arc::new(alert);
        for channel in &self.inner.channels {
            let channel = Arc::clone(channel);
            let alert = Arc::clone(&alert);
            handle.spawn(async move {
                let name = channel.name();
                match channel.deliver(&alert).await {
                    Ok(()) => tracing::debug!(
                        channel = name,
                        dedup_key = %alert.dedup_key,
                        "operator alert delivered"
                    ),
                    Err(error) => tracing::error!(
                        channel = name,
                        dedup_key = %alert.dedup_key,
                        error = %error,
                        "operator alert delivery failed; app continues serving"
                    ),
                }
            });
        }
    }
}

// ── Built-in condition hooks ────────────────────────────────────────────────

/// Look up the installed [`Alerter`] on `state`, if any.
#[must_use]
pub fn alerter(state: &AppState) -> Option<Arc<Alerter>> {
    state.extension::<Alerter>()
}

/// Condition (a): a background job was dead-lettered. Called from the job
/// backends' dead-letter sites. No-op when no alerter is installed.
pub fn notify_dead_lettered_job(state: &AppState, job_name: &str, error: &str) {
    let Some(alerter) = alerter(state) else {
        return;
    };
    let alert = Alert::trigger(
        AlertCondition::DeadLetteredJob,
        format!("dead_lettered_job:{job_name}"),
    )
    .title(format!("Job '{job_name}' was dead-lettered"))
    .summary(format!(
        "Background job '{job_name}' exhausted its retries and was moved to the dead-letter queue. Last error: {error}"
    ))
    .where_to_look(actuator_where_to_look(
        &alerter.settings().actuator_prefix,
        AlertCondition::DeadLetteredJob,
    ))
    .detail("job", job_name)
    .detail("error", error)
    .build();
    let _ = alerter.notify(alert);
}

/// Condition (d): a framework-scheduled task failed. Called from the scheduler's
/// failure arms. No-op when no alerter is installed.
pub fn notify_scheduled_task_failure(state: &AppState, task_name: &str, error: &str) {
    let Some(alerter) = alerter(state) else {
        return;
    };
    let alert = Alert::trigger(
        AlertCondition::ScheduledTaskFailure,
        format!("scheduled_task_failure:{task_name}"),
    )
    .title(format!("Scheduled task '{task_name}' failed"))
    .summary(format!(
        "The framework-scheduled task '{task_name}' returned an error on its last run: {error}"
    ))
    .where_to_look(actuator_where_to_look(
        &alerter.settings().actuator_prefix,
        AlertCondition::ScheduledTaskFailure,
    ))
    .detail("task", task_name)
    .detail("error", error)
    .build();
    let _ = alerter.notify(alert);
}

/// Recovery for condition (d): a previously-failing framework-scheduled task
/// completed successfully. Called from the scheduler's success arms.
///
/// No-op when no alerter is installed. Uses the SAME dedup key as
/// [`notify_scheduled_task_failure`] so the recovery clears an outstanding
/// failure; the dedup gate ([`AlertDeduplicator::on_resolve`]) makes it a no-op
/// when the failure key was never active, so a task that has only ever succeeded
/// sends nothing.
pub fn notify_scheduled_task_recovered(state: &AppState, task_name: &str) {
    let Some(alerter) = alerter(state) else {
        return;
    };
    let alert = Alert::recovery(
        AlertCondition::ScheduledTaskFailure,
        format!("scheduled_task_failure:{task_name}"),
    )
    .title(format!("Scheduled task '{task_name}' recovered"))
    .summary(format!(
        "The framework-scheduled task '{task_name}' completed successfully after a previous failure."
    ))
    .where_to_look(actuator_where_to_look(
        &alerter.settings().actuator_prefix,
        AlertCondition::ScheduledTaskFailure,
    ))
    .detail("task", task_name)
    .build();
    let _ = alerter.recover(alert);
}

// ── Config ──────────────────────────────────────────────────────────────────

const fn default_alerts_enabled() -> bool {
    true
}
const fn default_dedup_window_secs() -> u64 {
    900
}
const fn default_health_grace_secs() -> u64 {
    60
}
const fn default_error_rate_threshold() -> f64 {
    0.05
}
const fn default_error_rate_min_requests() -> u64 {
    20
}
const fn default_eval_interval_secs() -> u64 {
    30
}

/// `[alerts]` configuration.
///
/// Providing **only** a destination (an operator [`email`](Self::email) and/or
/// a [`webhook_url`](Self::webhook_url)) is enough to receive alerts for every
/// built-in condition — no application code required. Every destination and
/// tuning knob is also settable via `AUTUMN_ALERTS__*` environment variables.
///
/// ```toml
/// [alerts]
/// email = "oncall@example.com"
/// webhook_url = "https://alerts.example.com/hooks/autumn"
/// webhook_secret = "..."          # prefer AUTUMN_ALERTS__WEBHOOK_SECRET
/// # Tuning (defaults shown):
/// dedup_window_secs = 900         # at most one notice per condition per 15 min
/// health_grace_secs = 60          # indicator must stay Down this long
/// error_rate_threshold = 0.05     # 5% of sampled requests are 5xx
/// error_rate_min_requests = 20    # ignore the rate below this sample size
/// eval_interval_secs = 30         # background evaluation cadence
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AlertConfig {
    /// Master switch. Alerts are only ever emitted when this is `true` **and**
    /// at least one destination is configured.
    pub enabled: bool,
    /// Operator email destination (mail channel). Delivered via the app's
    /// configured mailer with suppression bypassed (alerts are security-class).
    pub email: Option<String>,
    /// Signed outbound webhook destination.
    pub webhook_url: Option<String>,
    /// HMAC signing secret for the webhook destination. Prefer the
    /// `AUTUMN_ALERTS__WEBHOOK_SECRET` env var over committing it.
    pub webhook_secret: Option<String>,
    /// At most one notification per condition per this many seconds.
    pub dedup_window_secs: u64,
    /// How long (seconds) an indicator must stay `Down` before it alerts.
    pub health_grace_secs: u64,
    /// 5xx fraction of the sampled request window that trips the alert.
    pub error_rate_threshold: f64,
    /// Minimum requests in the sample window before the 5xx rate is evaluated.
    pub error_rate_min_requests: u64,
    /// Background evaluation cadence (seconds) for the health and 5xx-rate
    /// conditions.
    pub eval_interval_secs: u64,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: default_alerts_enabled(),
            email: None,
            webhook_url: None,
            webhook_secret: None,
            dedup_window_secs: default_dedup_window_secs(),
            health_grace_secs: default_health_grace_secs(),
            error_rate_threshold: default_error_rate_threshold(),
            error_rate_min_requests: default_error_rate_min_requests(),
            eval_interval_secs: default_eval_interval_secs(),
        }
    }
}

impl AlertConfig {
    /// Whether a delivery destination is configured (email and/or webhook URL).
    #[must_use]
    pub fn has_destination(&self) -> bool {
        self.email.as_ref().is_some_and(|s| !s.trim().is_empty())
            || self
                .webhook_url
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    /// Whether alerts should actually be active (enabled + a destination).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.has_destination()
    }
}

// ── Built-in channels ───────────────────────────────────────────────────────

/// Mail alert channel: delivers each alert as an email through the app's
/// configured [`Mailer`](crate::mail::Mailer).
///
/// The message is built with
/// [`Mail::ignore_suppression`](crate::mail::MailBuilder::ignore_suppression):
/// operator alerts are security-class and must never be silently dropped by the
/// bounce/complaint suppression list.
#[cfg(feature = "mail")]
pub struct MailAlertChannel {
    mailer: Arc<crate::mail::Mailer>,
    to: String,
}

#[cfg(feature = "mail")]
impl MailAlertChannel {
    /// Create a mail channel delivering to `to` via `mailer`.
    #[must_use]
    pub fn new(mailer: Arc<crate::mail::Mailer>, to: impl Into<String>) -> Self {
        Self {
            mailer,
            to: to.into(),
        }
    }

    fn render_text(alert: &Alert) -> String {
        use std::fmt::Write as _;
        let mut body = format!(
            "{}\n\nSeverity: {:?}\nEvent: {:?}\nCondition: {}\nHost/replica: {}\nWhen: {}\nWhere to look next: {}\n\n{}\n",
            alert.title,
            alert.severity,
            alert.event,
            alert.condition.as_str(),
            alert.host,
            alert.timestamp.to_rfc3339(),
            alert.where_to_look,
            alert.summary,
        );
        if !alert.details.is_empty() {
            body.push_str("\nDetails:\n");
            let mut keys: Vec<&String> = alert.details.keys().collect();
            keys.sort();
            for k in keys {
                if let Some(v) = alert.details.get(k) {
                    let _ = writeln!(body, "  {k}: {v}");
                }
            }
        }
        body
    }
}

#[cfg(feature = "mail")]
impl AlertChannel for MailAlertChannel {
    fn name(&self) -> &'static str {
        "mail"
    }

    fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a> {
        Box::pin(async move {
            let prefix = match alert.event {
                AlertEventKind::Trigger => "[ALERT]",
                AlertEventKind::Resolve => "[RECOVERED]",
            };
            let subject = format!("{prefix} {}", alert.title);
            let mail = crate::mail::Mail::builder()
                .to(self.to.clone())
                .subject(subject)
                .text(Self::render_text(alert))
                .ignore_suppression()
                .build()
                .map_err(|e| AlertDeliveryError::new("mail", e.to_string()))?;
            self.mailer
                .send(mail)
                .await
                .map_err(|e| AlertDeliveryError::new("mail", e.to_string()))
        })
    }
}

/// Signed-webhook alert channel: POSTs the alert as JSON to a configured URL.
///
/// The request is signed with the same Stripe-style
/// `Autumn-Signature: t=<ts>,v1=<hmac>` scheme as the outbound webhook
/// machinery ([`webhook_outbound`](crate::webhook_outbound)) — no new
/// dependency.
#[cfg(feature = "http-client")]
pub struct WebhookAlertChannel {
    client: crate::http_client::Client,
    url: String,
    secret: Option<String>,
}

#[cfg(feature = "http-client")]
impl WebhookAlertChannel {
    /// Create a webhook channel posting to `url`, optionally HMAC-signing with
    /// `secret`.
    #[must_use]
    pub fn new(
        client: crate::http_client::Client,
        url: impl Into<String>,
        secret: Option<String>,
    ) -> Self {
        Self {
            client,
            url: url.into(),
            secret,
        }
    }
}

#[cfg(feature = "http-client")]
impl AlertChannel for WebhookAlertChannel {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a> {
        Box::pin(async move {
            let body = serde_json::to_string(alert)
                .map_err(|e| AlertDeliveryError::new("webhook", e.to_string()))?;
            let mut req = self
                .client
                .named(&self.url)
                .post(&self.url)
                .header("Content-Type", "application/json");
            if let Some(secret) = self.secret.as_ref() {
                let timestamp = Utc::now().timestamp();
                let signing_payload = format!("{timestamp}.{body}");
                let signature = crate::security::config::hmac_sha256_hex(
                    secret.as_bytes(),
                    signing_payload.as_bytes(),
                );
                req = req.header("Autumn-Signature", format!("t={timestamp},v1={signature}"));
            }
            let response = req
                .text_body(body)
                .send()
                .await
                .map_err(|e| AlertDeliveryError::new("webhook", e.to_string()))?;
            if response.is_success() {
                Ok(())
            } else {
                Err(AlertDeliveryError::new(
                    "webhook",
                    format!("endpoint returned status {}", response.status()),
                ))
            }
        })
    }
}

// ── Wiring ──────────────────────────────────────────────────────────────────

/// Install the operator alerter from `config` plus any builder channels.
///
/// Builds the built-in channels from `config`, combines them with any
/// `extra_channels` registered via the builder, installs the resulting
/// [`Alerter`] onto `state`, and starts the background evaluation loop for the
/// health and 5xx-rate conditions.
///
/// `config.enabled` is the master off switch. When it is `false` the *entire*
/// alerting subsystem is silent: no built-in channels, no custom/extra channels
/// registered via [`AppBuilder::with_alert_channel`](crate::app::AppBuilder::with_alert_channel),
/// no background evaluation loop, and no [`Alerter`] installed onto `state` (so
/// the `notify_*` hooks become no-ops). Nothing is installed either when
/// alerting is enabled but there is no destination and no extra channel — so an
/// app with no configured destination pays nothing.
pub fn install_from_config(
    state: &AppState,
    config: &AlertConfig,
    extra_channels: Vec<Arc<dyn AlertChannel>>,
) {
    // Master off switch: `enabled = false` silences EVERYTHING, including any
    // custom channel registered via `with_alert_channel`. Install nothing, do
    // not start the evaluation loop, and do not put an alerter onto `state`.
    if !config.enabled {
        return;
    }

    let mut channels: Vec<Arc<dyn AlertChannel>> = Vec::new();

    #[cfg(feature = "mail")]
    if let Some(email) = config.email.as_ref().filter(|s| !s.trim().is_empty()) {
        if let Some(mailer) = state.extension::<crate::mail::Mailer>() {
            // A `Mailer` backed by the disabled (no-op) transport accepts and
            // silently drops every message, so registering a `MailAlertChannel`
            // here would make an email-only prod config look active while
            // delivering nothing. Warn and skip the dead channel instead.
            if mailer.is_disabled() {
                tracing::warn!(
                    "alerts: an operator email is configured but [mail] transport is disabled; \
                     no email alerts will be delivered. Set [mail] transport to a real backend \
                     or use a webhook destination."
                );
            } else {
                channels.push(Arc::new(MailAlertChannel::new(mailer, email.clone())));
            }
        } else {
            tracing::warn!(
                "alerts: an operator email is configured but no mailer is installed; \
                 configure [mail] to enable email alerts"
            );
        }
    }
    // Without the `mail` feature the email branch above is compiled out, so an
    // email-only config would silently install no channels and deliver no
    // alerts. Warn loudly so operators don't believe email alerts are active.
    #[cfg(not(feature = "mail"))]
    if config.email.as_ref().is_some_and(|s| !s.trim().is_empty()) {
        tracing::warn!(
            "operator-alerts: an email destination is configured but the `mail` feature is not \
             enabled; no email alerts will be delivered. Enable the `mail` feature or use a \
             webhook destination."
        );
    }
    #[cfg(feature = "http-client")]
    if let Some(url) = config.webhook_url.as_ref().filter(|s| !s.trim().is_empty()) {
        // Alert webhooks are ALWAYS signed: the operator guide documents receivers
        // verifying the `Autumn-Signature` header. A configured webhook with no
        // non-empty signing secret would send UNSIGNED requests that a documented
        // receiver rejects (or accepts unauthenticated) — so refuse to register the
        // channel and warn, mirroring the disabled-mail-transport skip above.
        // Never send unsigned.
        if let Some(secret) = config
            .webhook_secret
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            let client = crate::http_client::Client::from_state(state);
            channels.push(Arc::new(WebhookAlertChannel::new(
                client,
                url.clone(),
                Some(secret.clone()),
            )));
        } else {
            tracing::warn!(
                "alerts: a webhook destination is configured but no `webhook_secret` is set; \
                 alert webhooks are always signed, so no webhook alerts will be delivered. Set \
                 [alerts] webhook_secret (or the AUTUMN_ALERTS__WEBHOOK_SECRET env var)."
            );
        }
    }
    // Without the `http-client` feature the webhook branch above is compiled out,
    // so a webhook-only config would silently install no channels and deliver no
    // alerts. Warn loudly so operators don't believe webhook alerts are active.
    #[cfg(not(feature = "http-client"))]
    if config
        .webhook_url
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        tracing::warn!(
            "operator-alerts: a webhook destination is configured but the `http-client` feature \
             is not enabled; no webhook alerts will be delivered. Enable the `http-client` \
             feature or use an email destination."
        );
    }

    channels.extend(extra_channels);

    if channels.is_empty() {
        return;
    }

    // Capture the effective actuator prefix so every alert's `where_to_look`
    // points at the real actuator endpoints even under a custom `[actuator]
    // prefix`. The full config is installed on `state` before this runs.
    let actuator_prefix = state.config().actuator.prefix;
    let settings = AlerterSettings::from_config(config, actuator_prefix);
    let alerter = Alerter::new(channels, settings);
    state.insert_extension(alerter.clone());
    spawn_evaluation_loop(state.clone(), alerter);
}

/// Spawn the background loop that evaluates the *pull-based* conditions:
/// health-indicator down-duration (b) and the rolling 5xx rate (c). Both are
/// evaluated off the request path so AC #6 (no request-path latency) holds.
fn spawn_evaluation_loop(state: AppState, alerter: Alerter) {
    let settings = alerter.settings().clone();
    tokio::spawn(async move {
        // Per-indicator: when it first went Down (None once it is Up again).
        let mut down_since: HashMap<String, DateTime<Utc>> = HashMap::new();
        // Prime the cumulative counters so the first tick measures a real delta.
        let (mut last_requests, mut last_5xx) = {
            let snap = state.metrics().snapshot();
            (snap.http.requests_total, snap.http.by_status.s5xx)
        };
        loop {
            tokio::time::sleep(settings.eval_interval).await;
            evaluate_error_rate(
                &alerter,
                &state,
                &settings,
                &mut last_requests,
                &mut last_5xx,
            );
            evaluate_health(&alerter, &state, &settings, &mut down_since).await;
        }
    });
}

/// Advance the 5xx sampling window for one tick.
///
/// Returns `Some((req_delta, err_delta))` — and advances the baseline counters
/// — only when the accumulated request delta has reached `min_requests`, i.e.
/// when a real evaluation should happen. When the tick is still below the
/// threshold it returns `None` and leaves the baselines **untouched**, so a
/// low-traffic app whose per-tick traffic never reaches `min_requests` keeps
/// accumulating requests across ticks instead of resetting every tick. The
/// window therefore measures "since the last evaluation" rather than "since the
/// last tick."
const fn sample_error_window(
    total: u64,
    s5xx: u64,
    last_requests: &mut u64,
    last_5xx: &mut u64,
    min_requests: u64,
) -> Option<(u64, u64)> {
    let req_delta = total.saturating_sub(*last_requests);
    if req_delta < min_requests {
        // Leave baselines untouched so counts accumulate across ticks.
        return None;
    }
    let err_delta = s5xx.saturating_sub(*last_5xx);
    *last_requests = total;
    *last_5xx = s5xx;
    Some((req_delta, err_delta))
}

/// Condition (c): compute the 5xx rate over the requests seen since the last
/// evaluation and compare to the threshold. Reads existing cumulative counters
/// — the request path is untouched.
fn evaluate_error_rate(
    alerter: &Alerter,
    state: &AppState,
    settings: &AlerterSettings,
    last_requests: &mut u64,
    last_5xx: &mut u64,
) {
    let snap = state.metrics().snapshot();
    let total = snap.http.requests_total;
    let s5xx = snap.http.by_status.s5xx;
    let Some((req_delta, err_delta)) = sample_error_window(
        total,
        s5xx,
        last_requests,
        last_5xx,
        settings.error_rate_min_requests,
    ) else {
        return;
    };
    #[allow(clippy::cast_precision_loss)]
    let rate = err_delta as f64 / req_delta as f64;
    let key = "high_error_rate:5xx".to_owned();
    if rate >= settings.error_rate_threshold {
        let pct = rate * 100.0;
        let threshold_pct = settings.error_rate_threshold * 100.0;
        let alert = Alert::trigger(AlertCondition::HighErrorRate, key)
            .title(format!("5xx error rate is {pct:.1}%"))
            .summary(format!(
                "{err_delta} of the last {req_delta} requests returned 5xx ({pct:.1}%), \
                 crossing the {threshold_pct:.1}% threshold."
            ))
            .where_to_look(actuator_where_to_look(
                &settings.actuator_prefix,
                AlertCondition::HighErrorRate,
            ))
            .detail("rate", format!("{rate:.4}"))
            .detail("errors", err_delta.to_string())
            .detail("requests", req_delta.to_string())
            .build();
        let _ = alerter.notify(alert);
    } else {
        let alert = Alert::recovery(AlertCondition::HighErrorRate, key)
            .title("5xx error rate recovered")
            .summary(format!(
                "The 5xx error rate is back under the threshold ({pct:.1}% of {req_delta} requests).",
                pct = rate * 100.0,
            ))
            .where_to_look(actuator_where_to_look(
                &settings.actuator_prefix,
                AlertCondition::HighErrorRate,
            ))
            .build();
        let _ = alerter.recover(alert);
    }
}

/// How a health indicator's current status maps onto the alert state machine.
///
/// Factored out of [`evaluate_health`] so the three-way branch is unit-testable
/// without an `AppState`/registry. The key subtlety: recovery must be gated on
/// [`HealthStatus::is_healthy`], NOT merely `!= Down`. `OutOfService` is a
/// non-`Down` status that is still UNHEALTHY (`/actuator/health` reports
/// non-200), so treating it as "recovered" would emit a false recovery while the
/// service is still degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthTransition {
    /// Status is `Down`: run the grace-period trigger logic.
    Down,
    /// Status is genuinely healthy again (`is_healthy()`): clear tracking and
    /// emit a recovery for any outstanding alert.
    Recover,
    /// Status is non-`Down` but still not healthy (e.g. `OutOfService`): hold
    /// the active alert as-is — no recovery, and no new trigger (only `Down`
    /// triggers, matching existing behavior).
    Hold,
}

/// Classify a health indicator's status into the alert-state transition it
/// drives. Uses [`HealthStatus::is_healthy`] (not `== Up`) so `Unknown` — which
/// the actuator reports as healthy — recovers, while `OutOfService` holds.
const fn classify_health_transition(status: crate::actuator::HealthStatus) -> HealthTransition {
    if matches!(status, crate::actuator::HealthStatus::Down) {
        HealthTransition::Down
    } else if status.is_healthy() {
        HealthTransition::Recover
    } else {
        HealthTransition::Hold
    }
}

/// Condition (b): run all health indicators, track how long each has been
/// `Down`, and alert once an indicator has stayed `Down` past the grace period.
/// Recover only when it reports a genuinely healthy status again — a non-`Down`
/// but still-unhealthy status (e.g. `OutOfService`) holds the active alert
/// rather than emitting a false recovery.
async fn evaluate_health(
    alerter: &Alerter,
    state: &AppState,
    settings: &AlerterSettings,
    down_since: &mut HashMap<String, DateTime<Utc>>,
) {
    let results = state.health_indicator_registry().run_all().await;
    let now = Utc::now();
    let grace = chrono::Duration::from_std(settings.health_grace)
        .unwrap_or_else(|_| chrono::Duration::seconds(60));

    for result in &results {
        let key = format!("health_indicator_down:{}", result.name);
        match classify_health_transition(result.output.status) {
            HealthTransition::Down => {
                let first = *down_since.entry(result.name.clone()).or_insert(now);
                if now - first >= grace {
                    let secs = (now - first).num_seconds().max(0);
                    let alert = Alert::trigger(AlertCondition::HealthIndicatorDown, key)
                        .title(format!("Health indicator '{}' is Down", result.name))
                        .summary(format!(
                            "Health indicator '{}' has reported Down for {secs}s, past the \
                             {grace_secs}s grace period.",
                            result.name,
                            grace_secs = settings.health_grace.as_secs(),
                        ))
                        .where_to_look(actuator_where_to_look(
                            &settings.actuator_prefix,
                            AlertCondition::HealthIndicatorDown,
                        ))
                        .detail("indicator", result.name.clone())
                        .detail("down_seconds", secs.to_string())
                        .build();
                    let _ = alerter.notify(alert);
                }
            }
            HealthTransition::Recover => {
                // Only clear the down-tracking (and emit a recovery) when the
                // indicator is genuinely healthy again. `recover` is itself gated
                // by `on_resolve`, so it is a no-op unless an alert was active.
                if down_since.remove(&result.name).is_some() {
                    let alert = Alert::recovery(AlertCondition::HealthIndicatorDown, key)
                        .title(format!("Health indicator '{}' recovered", result.name))
                        .summary(format!(
                            "Health indicator '{}' is reporting healthy again.",
                            result.name
                        ))
                        .where_to_look(actuator_where_to_look(
                            &settings.actuator_prefix,
                            AlertCondition::HealthIndicatorDown,
                        ))
                        .detail("indicator", result.name.clone())
                        .build();
                    let _ = alerter.recover(alert);
                }
            }
            HealthTransition::Hold => {
                // Non-`Down` but still unhealthy (e.g. `OutOfService`): leave the
                // active alert and `down_since` bookkeeping untouched so we do not
                // emit a "recovered" alert while `/actuator/health` is non-healthy.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("valid timestamp")
    }

    // ── Dedup window bounding (AC #3) ────────────────────────────────────────

    #[test]
    fn first_trigger_sends() {
        let mut d = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        assert_eq!(d.on_trigger("k", ts(0)), DedupDecision::Send);
    }

    #[test]
    fn repeated_triggers_within_window_are_suppressed() {
        let mut d = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        assert_eq!(d.on_trigger("k", ts(0)), DedupDecision::Send);
        // Many occurrences inside the window -> all suppressed (not one-per).
        for s in 1..100 {
            assert_eq!(
                d.on_trigger("k", ts(s)),
                DedupDecision::Suppress,
                "occurrence at {s}s inside window must be suppressed"
            );
        }
    }

    #[test]
    fn trigger_renotifies_once_after_window_elapses() {
        let mut d = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        assert_eq!(d.on_trigger("k", ts(0)), DedupDecision::Send);
        assert_eq!(d.on_trigger("k", ts(500)), DedupDecision::Suppress);
        // Window elapsed: exactly one re-notification.
        assert_eq!(d.on_trigger("k", ts(901)), DedupDecision::Send);
        assert_eq!(d.on_trigger("k", ts(902)), DedupDecision::Suppress);
    }

    #[test]
    fn distinct_keys_are_independent() {
        let mut d = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        assert_eq!(d.on_trigger("a", ts(0)), DedupDecision::Send);
        assert_eq!(d.on_trigger("b", ts(0)), DedupDecision::Send);
    }

    // ── Recovery emission (AC #3) ────────────────────────────────────────────

    #[test]
    fn recovery_sends_only_for_active_key() {
        let mut d = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        // Never triggered: no recovery.
        assert_eq!(d.on_resolve("k"), DedupDecision::Suppress);
        // Trigger, then recover once.
        assert_eq!(d.on_trigger("k", ts(0)), DedupDecision::Send);
        assert_eq!(d.on_resolve("k"), DedupDecision::Send);
        // Double-recover: suppressed.
        assert_eq!(d.on_resolve("k"), DedupDecision::Suppress);
    }

    #[test]
    fn trigger_after_recovery_alerts_immediately() {
        let mut d = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        assert_eq!(d.on_trigger("k", ts(0)), DedupDecision::Send);
        assert_eq!(d.on_resolve("k"), DedupDecision::Send);
        // Fresh trigger inside the original window still sends because the key
        // recovered in between.
        assert_eq!(d.on_trigger("k", ts(10)), DedupDecision::Send);
    }

    // ── Health-indicator recovery classification (P2: recover only when healthy)
    // The three-way branch in `evaluate_health`: `Down` triggers, a genuinely
    // healthy status recovers, and any other non-healthy status (`OutOfService`)
    // holds the active alert instead of emitting a false recovery.

    use crate::actuator::HealthStatus;

    #[test]
    fn health_down_status_drives_trigger_branch() {
        assert_eq!(
            classify_health_transition(HealthStatus::Down),
            HealthTransition::Down
        );
    }

    #[test]
    fn healthy_statuses_recover() {
        // `is_healthy()` treats both `Up` and `Unknown` as healthy, so both
        // recover — the actuator reports 200 for either.
        assert_eq!(
            classify_health_transition(HealthStatus::Up),
            HealthTransition::Recover
        );
        assert_eq!(
            classify_health_transition(HealthStatus::Unknown),
            HealthTransition::Recover
        );
    }

    #[test]
    fn out_of_service_holds_active_alert_instead_of_recovering() {
        // `OutOfService` is non-`Down` but still UNHEALTHY: it must NOT recover.
        assert!(!HealthStatus::OutOfService.is_healthy());
        assert_eq!(
            classify_health_transition(HealthStatus::OutOfService),
            HealthTransition::Hold
        );
    }

    /// Drive the exact `down_since` + deduplicator bookkeeping `evaluate_health`
    /// performs for a single indicator across a status sequence, returning the
    /// dedup decision for any recovery emitted on the final tick (`None` when no
    /// recovery was attempted). Proves the recovery is gated on a genuinely
    /// healthy status, not merely `!= Down`.
    fn recovery_decision_for_sequence(statuses: &[HealthStatus]) -> Option<DedupDecision> {
        let name = "db";
        let key = format!("health_indicator_down:{name}");
        let mut dedup = AlertDeduplicator::new(std::time::Duration::from_secs(900));
        let mut down_since: HashMap<String, DateTime<Utc>> = HashMap::new();
        // Zero grace period so a single `Down` tick alerts immediately.
        let grace = chrono::Duration::seconds(0);
        let mut last = None;
        for (i, status) in statuses.iter().enumerate() {
            last = None;
            let now = ts(i as i64);
            match classify_health_transition(*status) {
                HealthTransition::Down => {
                    let first = *down_since.entry(name.to_owned()).or_insert(now);
                    if now - first >= grace {
                        let _ = dedup.on_trigger(&key, now);
                    }
                }
                HealthTransition::Recover => {
                    if down_since.remove(name).is_some() {
                        last = Some(dedup.on_resolve(&key));
                    }
                }
                HealthTransition::Hold => {}
            }
        }
        last
    }

    #[test]
    fn down_then_out_of_service_does_not_recover() {
        // Down (alert) → OutOfService: the alert stays active; no recovery emitted.
        let decision =
            recovery_decision_for_sequence(&[HealthStatus::Down, HealthStatus::OutOfService]);
        assert_eq!(
            decision, None,
            "OutOfService after Down must NOT emit a recovery (service still unhealthy)"
        );
    }

    #[test]
    fn down_then_up_recovers() {
        // Down (alert) → Up: a genuine recovery is emitted for the active key.
        let decision = recovery_decision_for_sequence(&[HealthStatus::Down, HealthStatus::Up]);
        assert_eq!(
            decision,
            Some(DedupDecision::Send),
            "a genuinely healthy status after Down must emit a recovery"
        );
    }

    #[test]
    fn down_then_out_of_service_then_up_recovers_once() {
        // The alert survives the OutOfService dip and recovers when finally Up.
        let decision = recovery_decision_for_sequence(&[
            HealthStatus::Down,
            HealthStatus::OutOfService,
            HealthStatus::Up,
        ]);
        assert_eq!(decision, Some(DedupDecision::Send));
    }

    // ── Severity classification ──────────────────────────────────────────────

    #[test]
    fn trigger_is_critical_resolve_is_recovery() {
        let t = Alert::trigger(AlertCondition::DeadLetteredJob, "k").build();
        assert_eq!(t.severity, AlertSeverity::Critical);
        assert_eq!(t.event, AlertEventKind::Trigger);
        let r = Alert::recovery(AlertCondition::DeadLetteredJob, "k").build();
        assert_eq!(r.severity, AlertSeverity::Recovery);
        assert_eq!(r.event, AlertEventKind::Resolve);
    }

    #[test]
    fn where_to_look_defaults_per_condition() {
        assert_eq!(
            Alert::trigger(AlertCondition::DeadLetteredJob, "k")
                .build()
                .where_to_look,
            "/actuator/jobs"
        );
        assert_eq!(
            Alert::trigger(AlertCondition::HighErrorRate, "k")
                .build()
                .where_to_look,
            "/actuator/metrics"
        );
        assert_eq!(
            Alert::trigger(AlertCondition::HealthIndicatorDown, "k")
                .build()
                .where_to_look,
            "/actuator/health"
        );
        assert_eq!(
            Alert::trigger(AlertCondition::ScheduledTaskFailure, "k")
                .build()
                .where_to_look,
            "/actuator/tasks"
        );
    }

    #[test]
    fn actuator_where_to_look_honors_prefix() {
        // Default prefix reproduces the const builder default byte-for-byte.
        assert_eq!(
            actuator_where_to_look("/actuator", AlertCondition::DeadLetteredJob),
            "/actuator/jobs"
        );
        assert_eq!(
            actuator_where_to_look("/actuator", AlertCondition::HealthIndicatorDown),
            "/actuator/health"
        );
        // A custom prefix is honored for every condition.
        assert_eq!(
            actuator_where_to_look("/_ops", AlertCondition::DeadLetteredJob),
            "/_ops/jobs"
        );
        assert_eq!(
            actuator_where_to_look("/_ops", AlertCondition::HighErrorRate),
            "/_ops/metrics"
        );
        assert_eq!(
            actuator_where_to_look("/_ops", AlertCondition::ScheduledTaskFailure),
            "/_ops/tasks"
        );
        // Normalization matches the router: a trailing slash is trimmed and a
        // missing leading slash is added, so no `//` or missing-slash paths.
        assert_eq!(
            actuator_where_to_look("_ops/", AlertCondition::HealthIndicatorDown),
            "/_ops/health"
        );
        // Root ("/" or empty) prefix yields the bare suffix, matching the router.
        assert_eq!(
            actuator_where_to_look("/", AlertCondition::DeadLetteredJob),
            "/jobs"
        );
    }

    #[test]
    fn stable_dedup_key_survives_trigger_and_recovery() {
        let key = "dead_lettered_job:emailer";
        let t = Alert::trigger(AlertCondition::DeadLetteredJob, key).build();
        let r = Alert::recovery(AlertCondition::DeadLetteredJob, key).build();
        assert_eq!(t.dedup_key, r.dedup_key);
    }

    // ── Fan-out (delivery to every channel) ──────────────────────────────────

    #[derive(Default)]
    struct CapturingChannel {
        received: Arc<StdMutex<Vec<Alert>>>,
        name: &'static str,
    }

    impl AlertChannel for CapturingChannel {
        fn name(&self) -> &'static str {
            self.name
        }
        fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a> {
            let received = Arc::clone(&self.received);
            let cloned = alert.clone();
            Box::pin(async move {
                received.lock().expect("lock").push(cloned);
                Ok(())
            })
        }
    }

    fn settings() -> AlerterSettings {
        AlerterSettings {
            dedup_window: std::time::Duration::from_secs(900),
            health_grace: std::time::Duration::from_secs(60),
            error_rate_threshold: 0.05,
            error_rate_min_requests: 20,
            eval_interval: std::time::Duration::from_secs(30),
            actuator_prefix: "/actuator".to_owned(),
        }
    }

    #[tokio::test]
    async fn dispatch_fans_out_to_all_channels() {
        let a = Arc::new(StdMutex::new(Vec::new()));
        let b = Arc::new(StdMutex::new(Vec::new()));
        let channels: Vec<Arc<dyn AlertChannel>> = vec![
            Arc::new(CapturingChannel {
                received: Arc::clone(&a),
                name: "a",
            }),
            Arc::new(CapturingChannel {
                received: Arc::clone(&b),
                name: "b",
            }),
        ];
        let alerter = Alerter::new(channels, settings());
        assert!(
            alerter.notify(
                Alert::trigger(AlertCondition::DeadLetteredJob, "dead_lettered_job:x")
                    .title("x failed")
                    .build()
            )
        );
        // Let the detached task run.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(a.lock().expect("lock").len(), 1);
        assert_eq!(b.lock().expect("lock").len(), 1);
        assert_eq!(a.lock().expect("lock")[0].title, "x failed");
    }

    #[tokio::test]
    async fn sustained_condition_dispatches_once_per_window() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let channels: Vec<Arc<dyn AlertChannel>> = vec![Arc::new(CapturingChannel {
            received: Arc::clone(&seen),
            name: "cap",
        })];
        let alerter = Alerter::new(channels, settings());
        // 50 occurrences of the same condition in quick succession.
        let mut sent = 0;
        for _ in 0..50 {
            if alerter.notify(
                Alert::trigger(AlertCondition::HighErrorRate, "high_error_rate:5xx").build(),
            ) {
                sent += 1;
            }
        }
        assert_eq!(sent, 1, "sustained condition must alert once per window");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(seen.lock().expect("lock").len(), 1);
    }

    #[test]
    fn config_active_requires_enabled_and_destination() {
        let mut c = AlertConfig::default();
        assert!(!c.is_active(), "no destination -> inactive");
        c.email = Some("ops@example.com".to_owned());
        assert!(c.is_active());
        c.enabled = false;
        assert!(!c.is_active(), "disabled -> inactive even with destination");
    }

    #[test]
    fn sub_threshold_ticks_accumulate_until_window_crosses_min_requests() {
        // min_requests = 10; each tick adds 4 requests (below threshold) with
        // 1 new 5xx. Baselines must NOT reset on the sub-threshold ticks so the
        // requests accumulate until the summed window finally crosses 10.
        let min_requests = 10;
        let mut last_requests = 0;
        let mut last_5xx = 0;

        // Tick 1: total=4, 5xx=1 -> below threshold, no evaluation.
        assert_eq!(
            sample_error_window(4, 1, &mut last_requests, &mut last_5xx, min_requests),
            None,
            "first sub-threshold tick must not evaluate"
        );
        // Baselines must be untouched so progress is preserved.
        assert_eq!(
            last_requests, 0,
            "sub-threshold tick must not reset baseline"
        );
        assert_eq!(
            last_5xx, 0,
            "sub-threshold tick must not reset 5xx baseline"
        );

        // Tick 2: total=8, 5xx=2 -> still below threshold (delta from 0 is 8).
        assert_eq!(
            sample_error_window(8, 2, &mut last_requests, &mut last_5xx, min_requests),
            None,
            "second sub-threshold tick must not evaluate"
        );
        assert_eq!(last_requests, 0);
        assert_eq!(last_5xx, 0);

        // Tick 3: total=12, 5xx=3 -> accumulated delta 12 >= 10, evaluate now
        // over the whole accumulated window (12 requests, 3 errors).
        assert_eq!(
            sample_error_window(12, 3, &mut last_requests, &mut last_5xx, min_requests),
            Some((12, 3)),
            "window must evaluate once summed requests cross the threshold"
        );
        // Only now do the baselines advance to the evaluated point.
        assert_eq!(last_requests, 12, "baseline advances only after evaluation");
        assert_eq!(last_5xx, 3);

        // Next sub-threshold tick starts a fresh accumulation from 12.
        assert_eq!(
            sample_error_window(15, 3, &mut last_requests, &mut last_5xx, min_requests),
            None
        );
        assert_eq!(
            last_requests, 12,
            "baseline held across the next sub-threshold tick"
        );
        assert_eq!(last_5xx, 3);
    }

    #[test]
    fn alert_serializes_to_json_for_webhook() {
        let alert = Alert::trigger(AlertCondition::DeadLetteredJob, "dead_lettered_job:x")
            .title("x failed")
            .detail("job", "x")
            .build();
        let v: serde_json::Value = serde_json::to_value(&alert).expect("serialize");
        assert_eq!(v["dedup_key"], "dead_lettered_job:x");
        assert_eq!(v["condition"], "dead_lettered_job");
        assert_eq!(v["severity"], "critical");
        assert_eq!(v["event"], "trigger");
        assert_eq!(v["where_to_look"], "/actuator/jobs");
    }
}
