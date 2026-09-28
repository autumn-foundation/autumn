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

/// Attempts for each SLA job before it goes to the dead letters.
const MAX_ATTEMPTS: u32 = 5;

/// The first retry delay of an SLA job.
const BACKOFF_MS: u64 = 1_000;

/// Attempts to release a claim after a failed enqueue.
const RELEASE_ATTEMPTS: u32 = 5;

/// The first delay between two release attempts. It doubles each time.
const RELEASE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

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
    /// How many times an early check put itself on the queue again. It is
    /// part of the unique key, so a requeue from a running check is not
    /// merged into that check.
    #[serde(default)]
    requeue: u32,
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
                    &["key", "generation", "due_at", "requeue"],
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
    /// It is safe to call again. If it fails after it stored the record (for
    /// example, the job queue refused the check), the record stays and the
    /// next call schedules the check.
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
        let zone = self.zone_of(obligation, calendar);
        let resolved = obligation
            .clone()
            .zone(zone)
            .starting_at(obligation.started_at().unwrap_or(now));
        let key = resolved.key();
        // Refuse before the insert, so a refused obligation leaves no record.
        let Some(due_at) = resolved.status_with(calendar, zone, now).due_at else {
            return Err(SlaError::NoDeadline(key));
        };
        let store = &self.engine.store;
        let generation = self.state.entropy().uuid_v4();
        let (mut record, created) = store
            .insert(ObligationRecord::new(resolved, generation).with_due_at(due_at))
            .await?;
        // A new record has the met instant already. An older one gets it now.
        if !created && let Some(met) = obligation.met() {
            if store.mark_met(&key, record.generation, met).await? {
                record.obligation.set_met(met);
            } else if let Some(fresh) = store.get(&key).await?
                && fresh.generation == record.generation
            {
                // Another call changed the record. Use the stored state.
                record = fresh;
            }
        }
        self.schedule(&key, &record, now).await
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

    /// Store the deadline of each open record on the current calendars, and
    /// put a check job on the queue there. Returns the number of records it
    /// checked.
    ///
    /// Call it once from the new version after a deploy that changes a
    /// calendar, for example from an `on_startup` hook. Without it, a record
    /// keeps the deadline that `track` stored. It is safe to call at any
    /// time: a check that is already on the queue is not added again.
    ///
    /// # Errors
    ///
    /// Returns an error for a store failure, an unknown calendar, or no job
    /// runtime.
    pub async fn reconcile(&self) -> Result<usize, SlaError> {
        let now = self.now();
        let mut scheduled = 0_usize;
        let store = &self.engine.store;
        for record in store.list().await? {
            // The deadline on this replica's calendars, not the stored one.
            let status = self.calendar_status(&record, now)?;
            let Some(due) = status.due_at else { continue };
            if status.escalated_at.is_some() || status.state == ObligationState::Met {
                continue;
            }
            if record.due_at != Some(due)
                && !store.set_due(&status.key, record.generation, due).await?
            {
                // The record was escalated or replaced after the read.
                continue;
            }
            self.schedule_check(&status.key, record.generation, due)
                .await?;
            scheduled = scheduled.saturating_add(1);
        }
        Ok(scheduled)
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

    /// The status of `record` on this replica's calendar.
    fn calendar_status(
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

    /// The status of `record`. The stored deadline, if set, decides the
    /// deadline and the met or breached state. Thus a replica with an old
    /// calendar agrees with the record.
    fn status_of(
        &self,
        record: &ObligationRecord,
        now: DateTime<Utc>,
    ) -> Result<ObligationStatus, SlaError> {
        let mut status = self.calendar_status(record, now)?;
        if let Some(due) = record.due_at
            && status.due_at != Some(due)
        {
            status.due_at = Some(due);
            status.state = match status.met_at {
                Some(met) if met <= due => ObligationState::Met,
                Some(_) => ObligationState::Breached,
                None if now >= due => ObligationState::Breached,
                None if status.state == ObligationState::Breached => ObligationState::Running,
                None => status.state,
            };
            if status.state == ObligationState::Breached {
                status.remaining = std::time::Duration::ZERO;
            }
        }
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
            requeue: 0,
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

    /// If the injected clock is before the deadline in `args`, put the check
    /// on the queue again after the difference, and return `true`.
    ///
    /// A check job runs early when the queue clock leads the app clock. The
    /// requeue uses no retry attempt, so a large clock difference does not
    /// send the check to the dead letters.
    async fn requeue_if_early(&self, args: &CheckArgs) -> Result<bool, SlaError> {
        let Some(early) = args
            .due_at
            .signed_duration_since(self.now())
            .to_std()
            .ok()
            .filter(|early| !early.is_zero())
        else {
            return Ok(false);
        };
        if args.requeue > 0 {
            tracing::warn!(
                key = %args.key,
                ?early,
                requeue = args.requeue,
                "SLA check ran before its deadline again; the job queue clock leads the app clock"
            );
        }
        let again = CheckArgs {
            key: args.key.clone(),
            generation: args.generation,
            due_at: args.due_at,
            requeue: args.requeue.saturating_add(1),
        };
        let client = self
            .state
            .extension::<crate::job::JobClient>()
            .ok_or(SlaError::NoJobRuntime)?;
        let payload = serde_json::to_value(&again).map_err(|err| SlaError::Job(err.to_string()))?;
        client
            .enqueue_relative(CHECK_JOB, payload, early)
            .await
            .map_err(|err| SlaError::Job(err.to_string()))?;
        Ok(true)
    }

    /// Run the breach check of `key`. `due_hint` is the deadline in the job.
    async fn check(
        &self,
        key: &str,
        generation: Option<uuid::Uuid>,
        due_hint: DateTime<Utc>,
    ) -> Result<(), SlaError> {
        let now = self.now();
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
            if let Err(release) = release_claim(store.as_ref(), key, record.generation, now).await {
                tracing::error!(
                    %key,
                    enqueue = %err,
                    release = %release,
                    "SLA escalation is claimed but not enqueued; this escalation is lost"
                );
            }
            return Err(err);
        }
        Ok(())
    }

    /// Whether the record of `breach` was met by its deadline after the
    /// claim, for example by a late `track` with an earlier met instant. If
    /// so, release the claim: this breach does not run.
    async fn met_on_time(&self, breach: &SlaBreach) -> Result<bool, SlaError> {
        let store = &self.engine.store;
        let on_time = store.get(&breach.key).await?.is_some_and(|record| {
            record.generation == breach.generation
                && record
                    .obligation
                    .met()
                    .is_some_and(|met| met <= breach.due_at)
        });
        if on_time {
            store
                .release_escalation(&breach.key, breach.generation, breach.escalated_at)
                .await?;
        }
        Ok(on_time)
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
        let sla = Sla::from_state(&state)?;
        if !sla.requeue_if_early(&args).await? {
            sla.check(&args.key, args.generation, args.due_at).await?;
        }
        Ok(())
    })
}

fn escalate_job(state: AppState, payload: Value) -> BreachFuture {
    Box::pin(async move {
        let breach: SlaBreach = serde_json::from_value(payload)?;
        let sla = Sla::from_state(&state)?;
        if sla.met_on_time(&breach).await? {
            return Ok(());
        }
        let Some(handler) = sla.handler_for(&breach.obligation) else {
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

/// Release the claim of `key` after a failed enqueue. Try again after a store
/// error, up to [`RELEASE_ATTEMPTS`] times. A claim that stays set stops each
/// retry of the check, so the escalation is lost.
pub(super) async fn release_claim(
    store: &dyn ObligationStore,
    key: &str,
    generation: uuid::Uuid,
    claimed_at: DateTime<Utc>,
) -> Result<(), SlaError> {
    let mut delay = RELEASE_BACKOFF;
    for attempt in 1..RELEASE_ATTEMPTS {
        match store.release_escalation(key, generation, claimed_at).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(%key, attempt, error = %err, "SLA claim release failed; trying again");
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
        }
    }
    store.release_escalation(key, generation, claimed_at).await
}
