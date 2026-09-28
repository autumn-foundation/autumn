//! The plugin, the extractor and the jobs of the SLA engine.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use serde_json::Value;

use super::{
    BusinessCalendar, MemoryObligationStore, Obligation, ObligationRecord, ObligationState,
    ObligationStatus, ObligationStore, SlaError,
};
use crate::job::{JobInfo, JobUniqueness, JobUniquenessWindow};
use crate::{AppState, AutumnResult};

/// The job that checks an obligation at its deadline.
pub const CHECK_JOB: &str = "autumn_sla_check";

/// The job that runs the breach handler of an obligation.
pub const ESCALATE_JOB: &str = "autumn_sla_escalate";

/// If a check job runs before its deadline, it waits. It waits 5 minutes at
/// most.
const MAX_EARLY_CHECK: std::time::Duration = std::time::Duration::from_secs(300);

/// Attempts to pin a tracked record that a failed creator removed.
const PIN_ATTEMPTS: usize = 3;

/// Attempts for each SLA job before it goes to the dead letters.
const MAX_ATTEMPTS: u32 = 5;

/// The first retry delay of an SLA job.
const BACKOFF_MS: u64 = 1_000;

type BreachFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>;
type BreachHandler = Arc<dyn Fn(AppState, SlaBreach) -> BreachFuture + Send + Sync>;

/// The typed escalation payload of a breached obligation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SlaBreach {
    /// The unique key, `"<name>/<subject>"`.
    pub key: String,
    /// The obligation name.
    pub obligation: String,
    /// The subject, such as `"ticket:42"`.
    pub subject: String,
    /// The calendar name.
    pub calendar: String,
    /// The IANA name of the time zone.
    pub zone: String,
    /// The generation of the tracked record. See
    /// [`ObligationRecord::generation`].
    pub generation: uuid::Uuid,
    /// The start instant.
    pub started_at: DateTime<Utc>,
    /// The deadline.
    pub due_at: DateTime<Utc>,
    /// The instant when the escalation was claimed.
    pub escalated_at: DateTime<Utc>,
}

/// The payload of [`CHECK_JOB`].
#[derive(Debug, Serialize, Deserialize)]
struct CheckArgs {
    key: String,
    /// The record that this check is for. A check for a replaced record
    /// stops. `None` checks any record for the key.
    #[serde(default)]
    generation: Option<uuid::Uuid>,
    due_at: DateTime<Utc>,
}

/// The shared state of the SLA engine, kept as an [`AppState`] extension.
struct SlaEngine {
    calendars: BTreeMap<String, BusinessCalendar>,
    store: Arc<dyn ObligationStore>,
    handlers: BTreeMap<String, BreachHandler>,
    fallback: Option<BreachHandler>,
}

/// Installs the SLA engine.
///
/// It adds the calendars, the [`ObligationStore`] and the breach handlers, and
/// registers [`CHECK_JOB`] and [`ESCALATE_JOB`].
pub struct SlaPlugin {
    calendars: BTreeMap<String, BusinessCalendar>,
    store: Arc<dyn ObligationStore>,
    handlers: BTreeMap<String, BreachHandler>,
    fallback: Option<BreachHandler>,
}

impl std::fmt::Debug for SlaPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlaPlugin")
            .field("calendars", &self.calendars.keys().collect::<Vec<_>>())
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .field("fallback", &self.fallback.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for SlaPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SlaPlugin {
    /// Make a plugin with no calendars and a [`MemoryObligationStore`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            calendars: BTreeMap::new(),
            store: Arc::new(MemoryObligationStore::new()),
            handlers: BTreeMap::new(),
            fallback: None,
        }
    }

    /// Add a calendar called `name`.
    #[must_use]
    pub fn calendar(mut self, name: impl Into<String>, calendar: BusinessCalendar) -> Self {
        self.calendars.insert(name.into(), calendar);
        self
    }

    /// Use `store` for tracked obligations.
    #[must_use]
    pub fn store(mut self, store: impl ObligationStore) -> Self {
        self.store = Arc::new(store);
        self
    }

    /// Run `handler` when an obligation called `obligation` is breached.
    #[must_use]
    pub fn on_breach<F, Fut>(mut self, obligation: impl Into<String>, handler: F) -> Self
    where
        F: Fn(AppState, SlaBreach) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AutumnResult<()>> + Send + 'static,
    {
        self.handlers.insert(obligation.into(), boxed(handler));
        self
    }

    /// Run `handler` for a breach that has no named handler.
    #[must_use]
    pub fn on_any_breach<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(AppState, SlaBreach) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AutumnResult<()>> + Send + 'static,
    {
        self.fallback = Some(boxed(handler));
        self
    }
}

fn boxed<F, Fut>(handler: F) -> BreachHandler
where
    F: Fn(AppState, SlaBreach) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AutumnResult<()>> + Send + 'static,
{
    Arc::new(move |state, breach| Box::pin(handler(state, breach)))
}

impl crate::plugin::Plugin for SlaPlugin {
    fn build(self, app: crate::app::AppBuilder) -> crate::app::AppBuilder {
        let engine = SlaEngine {
            calendars: self.calendars,
            store: self.store,
            handlers: self.handlers,
            fallback: self.fallback,
        };
        app.state_initializer(move |state| state.insert_extension(engine))
            .jobs(vec![
                sla_job(
                    CHECK_JOB,
                    check_job,
                    &["key", "generation", "due_at"],
                    JobUniquenessWindow::Running,
                ),
                sla_job(
                    ESCALATE_JOB,
                    escalate_job,
                    &["key", "generation"],
                    JobUniquenessWindow::Running,
                ),
            ])
    }
}

fn sla_job(
    name: &str,
    handler: crate::job::JobHandler,
    unique_by: &[&str],
    window: JobUniquenessWindow,
) -> JobInfo {
    let mut info = JobInfo::new(name, MAX_ATTEMPTS, BACKOFF_MS, handler);
    info.uniqueness = Some(JobUniqueness {
        by: unique_by.iter().map(|field| (*field).to_owned()).collect(),
        window,
    });
    info
}

/// The SLA engine for one request or job.
///
/// Use it as a handler extractor, or get it with [`Sla::from_state`]. It reads
/// the time from the injected clock.
#[derive(Clone)]
pub struct Sla {
    state: AppState,
    engine: Arc<SlaEngine>,
}

impl std::fmt::Debug for Sla {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sla")
            .field(
                "calendars",
                &self.engine.calendars.keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Sla {
    /// Get the engine from `state`.
    ///
    /// # Errors
    ///
    /// Returns [`SlaError::NotInstalled`] when the app has no [`SlaPlugin`].
    pub fn from_state(state: &AppState) -> Result<Self, SlaError> {
        let engine = state
            .extension::<SlaEngine>()
            .ok_or(SlaError::NotInstalled)?;
        Ok(Self {
            state: state.clone(),
            engine,
        })
    }

    /// The current instant of the injected clock.
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        self.state.clock().now()
    }

    /// The calendar called `name`.
    #[must_use]
    pub fn calendar(&self, name: &str) -> Option<&BusinessCalendar> {
        self.engine.calendars.get(name)
    }

    /// Calculate the status of `obligation` now. This does not track it.
    ///
    /// # Errors
    ///
    /// Returns [`SlaError::UnknownCalendar`] for an unknown calendar.
    pub fn status(&self, obligation: &Obligation) -> Result<ObligationStatus, SlaError> {
        let calendar = self.calendar_of(obligation)?;
        let zone = self.zone_of(obligation, calendar);
        Ok(obligation.status_with(calendar, zone, self.now()))
    }

    /// Track `obligation` and schedule its breach check.
    ///
    /// A second call with the same key keeps the first record. A met instant
    /// on `obligation` marks the record met.
    ///
    /// # Errors
    ///
    /// If this call made the record and a later step fails, it removes the
    /// record again, unless another `track` call scheduled it.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero budget, an unknown calendar, an
    /// obligation with no deadline ([`SlaError::NoDeadline`]), a store
    /// failure, or no job runtime.
    pub async fn track(&self, obligation: &Obligation) -> Result<ObligationStatus, SlaError> {
        let now = self.now();
        let calendar = self.calendar_of(obligation)?;
        if obligation.budget().resolve(calendar).is_zero() {
            return Err(SlaError::InvalidDuration(format!(
                "{}: the budget is zero working time; set it with Obligation::within",
                obligation.key()
            )));
        }
        let resolved = obligation
            .clone()
            .zone(self.zone_of(obligation, calendar))
            .starting_at(obligation.started_at().unwrap_or(now))
            .met_at(None);
        let key = resolved.key();
        let store = &self.engine.store;
        let generation = self.state.entropy().uuid_v4();
        let (record, created) = store
            .insert(ObligationRecord::new(resolved, generation))
            .await?;
        let status = match self.mark_and_schedule(&key, &record, obligation, now).await {
            Ok(status) => status,
            Err(err) => {
                // Do not keep a record that this call made and could not
                // schedule, unless another call scheduled it.
                if created {
                    store.remove_unscheduled(&key, record.generation).await?;
                }
                return Err(err);
            }
        };
        // Pin the instance that this call scheduled. If a failed creator
        // removed it after our check job went on the queue, put the stored
        // record back for that job.
        for _ in 0..PIN_ATTEMPTS {
            if store.mark_scheduled(&key, record.generation).await? {
                return Ok(status);
            }
            let (current, _) = store.insert(record.clone()).await?;
            if current.generation != record.generation {
                // A `forget` and a new `track` replaced this instance. The
                // new call owns the new record.
                return Ok(status);
            }
            if let Some(met) = obligation.met() {
                store.mark_met(&key, record.generation, met).await?;
            }
        }
        Err(SlaError::Store(format!(
            "{key}: the record keeps disappearing"
        )))
    }

    /// Mark a stored record met, if `obligation` is met, then schedule it.
    async fn mark_and_schedule(
        &self,
        key: &str,
        record: &ObligationRecord,
        obligation: &Obligation,
        now: DateTime<Utc>,
    ) -> Result<ObligationStatus, SlaError> {
        let mut record = record.clone();
        if let Some(met) = obligation.met()
            && self
                .engine
                .store
                .mark_met(key, record.generation, met)
                .await?
        {
            record.obligation.set_met(met);
        }
        self.schedule(key, &record, now).await
    }

    /// Put the check job of `record` on the queue, if it is still open.
    async fn schedule(
        &self,
        key: &str,
        record: &ObligationRecord,
        now: DateTime<Utc>,
    ) -> Result<ObligationStatus, SlaError> {
        let status = self.status_of(record, now)?;
        let Some(due) = status.due_at else {
            return Err(SlaError::NoDeadline(key.to_owned()));
        };
        if status.escalated_at.is_none() && status.state != ObligationState::Met {
            self.schedule_check(key, record.generation, due).await?;
        }
        Ok(status)
    }

    /// Mark the obligation `key` met now.
    ///
    /// Returns `None` when `key` is not tracked.
    ///
    /// # Errors
    ///
    /// Returns an error for a store failure.
    pub async fn meet(&self, key: &str) -> Result<Option<ObligationStatus>, SlaError> {
        let store = &self.engine.store;
        if let Some(record) = store.get(key).await? {
            store.mark_met(key, record.generation, self.now()).await?;
        }
        self.get(key).await
    }

    /// The status of the tracked obligation `key`.
    ///
    /// # Errors
    ///
    /// Returns an error for a store failure or an unknown calendar.
    pub async fn get(&self, key: &str) -> Result<Option<ObligationStatus>, SlaError> {
        let now = self.now();
        self.engine
            .store
            .get(key)
            .await?
            .map(|record| self.status_of(&record, now))
            .transpose()
    }

    /// The status of each tracked obligation, sorted by key.
    ///
    /// # Errors
    ///
    /// Returns an error for a store failure or an unknown calendar.
    pub async fn statuses(&self) -> Result<Vec<ObligationStatus>, SlaError> {
        let now = self.now();
        self.engine
            .store
            .list()
            .await?
            .iter()
            .map(|record| self.status_of(record, now))
            .collect()
    }

    /// Stop tracking `key`. Returns `true` if it was tracked.
    ///
    /// # Errors
    ///
    /// Returns an error for a store failure.
    pub async fn forget(&self, key: &str) -> Result<bool, SlaError> {
        self.engine.store.remove(key).await
    }

    fn calendar_of(&self, obligation: &Obligation) -> Result<&BusinessCalendar, SlaError> {
        let name = obligation.calendar_name();
        self.calendar(name)
            .ok_or_else(|| SlaError::UnknownCalendar(name.to_owned()))
    }

    /// The zone of the obligation, then of the calendar, then the app default.
    fn zone_of(&self, obligation: &Obligation, calendar: &BusinessCalendar) -> chrono_tz::Tz {
        obligation
            .time_zone()
            .or_else(|| calendar.home_zone())
            .unwrap_or_else(|| self.state.config_arc().time_zone.default_tz())
    }

    fn status_of(
        &self,
        record: &ObligationRecord,
        now: DateTime<Utc>,
    ) -> Result<ObligationStatus, SlaError> {
        let calendar = self.calendar_of(&record.obligation)?;
        let zone = self.zone_of(&record.obligation, calendar);
        let mut status = record.obligation.status_with(calendar, zone, now);
        status.escalated_at = record.escalated_at;
        Ok(status)
    }

    async fn schedule_check(
        &self,
        key: &str,
        generation: uuid::Uuid,
        due_at: DateTime<Utc>,
    ) -> Result<(), SlaError> {
        let args = CheckArgs {
            key: key.to_owned(),
            generation: Some(generation),
            due_at,
        };
        self.enqueue(CHECK_JOB, &args, Some(due_at)).await
    }

    async fn enqueue<T: Serialize + Sync>(
        &self,
        name: &str,
        args: &T,
        due_at: Option<DateTime<Utc>>,
    ) -> Result<(), SlaError> {
        let client = self
            .state
            .extension::<crate::job::JobClient>()
            .ok_or(SlaError::NoJobRuntime)?;
        let payload = serde_json::to_value(args).map_err(|err| SlaError::Job(err.to_string()))?;
        client
            .enqueue_due(name, payload, due_at)
            .await
            .map_err(|err| SlaError::Job(err.to_string()))
    }

    /// Run the breach check of `key`. `due_hint` is the deadline in the job.
    async fn check(
        &self,
        key: &str,
        generation: Option<uuid::Uuid>,
        due_hint: DateTime<Utc>,
    ) -> Result<(), SlaError> {
        let store = &self.engine.store;
        let Some(record) = store.get(key).await? else {
            tracing::warn!(
                %key,
                "SLA check found no tracked obligation; replicas need a shared ObligationStore"
            );
            return Ok(());
        };
        if generation.is_some_and(|generation| generation != record.generation) {
            // A `forget` and a new `track` replaced this record. The new
            // record has its own check.
            return Ok(());
        }
        if record.escalated_at.is_some() {
            return Ok(());
        }
        let mut now = self.now();
        if let Some(early) = due_hint.signed_duration_since(now).to_std().ok()
            && !early.is_zero()
        {
            // The job ran before the deadline on this clock (clock skew).
            // Wait for the deadline. Do not guess the status at a later time.
            if early > MAX_EARLY_CHECK {
                return Err(SlaError::Job(format!(
                    "SLA check for {key} ran {early:?} before its deadline"
                )));
            }
            tokio::time::sleep(early).await;
            now = self.now().max(due_hint);
        }
        let status = self.status_of(&record, now)?;
        let due_at = match (status.state, status.due_at) {
            (ObligationState::Breached, Some(due_at)) => due_at,
            (ObligationState::Running | ObligationState::Paused, Some(due_at))
                if due_at != due_hint =>
            {
                // The deadline moved. Check again at the new deadline.
                return self.schedule_check(key, record.generation, due_at).await;
            }
            _ => return Ok(()),
        };
        // Claim this instance only: a `forget` and `track` since the read
        // makes a new record with another generation.
        if !store
            .claim_escalation(key, record.generation, due_at, now)
            .await?
        {
            return Ok(());
        }
        let obligation = &record.obligation;
        let breach = SlaBreach {
            key: key.to_owned(),
            obligation: obligation.name().to_owned(),
            subject: obligation.subject().to_owned(),
            calendar: obligation.calendar_name().to_owned(),
            zone: status.zone.name().to_owned(),
            generation: record.generation,
            started_at: status.started_at,
            due_at,
            escalated_at: now,
        };
        if let Err(err) = self.enqueue(ESCALATE_JOB, &breach, None).await {
            // Release the claim, so that the retry of this check can claim again.
            if let Err(release) = store.release_escalation(key, record.generation, now).await {
                tracing::error!(
                    %key,
                    enqueue = %err,
                    release = %release,
                    "SLA escalation is claimed but not enqueued"
                );
            }
            return Err(err);
        }
        Ok(())
    }

    fn handler_for(&self, obligation: &str) -> Option<BreachHandler> {
        self.engine
            .handlers
            .get(obligation)
            .or(self.engine.fallback.as_ref())
            .cloned()
    }
}

fn check_job(state: AppState, payload: Value) -> BreachFuture {
    Box::pin(async move {
        let args: CheckArgs = serde_json::from_value(payload)?;
        Sla::from_state(&state)?
            .check(&args.key, args.generation, args.due_at)
            .await?;
        Ok(())
    })
}

fn escalate_job(state: AppState, payload: Value) -> BreachFuture {
    Box::pin(async move {
        let breach: SlaBreach = serde_json::from_value(payload)?;
        let Some(handler) = Sla::from_state(&state)?.handler_for(&breach.obligation) else {
            tracing::warn!(key = %breach.key, "SLA breach has no handler");
            return Ok(());
        };
        handler(state, breach).await
    })
}

impl axum::extract::FromRequestParts<AppState> for Sla {
    type Rejection = crate::AutumnError;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self::from_state(state)?)
    }
}
