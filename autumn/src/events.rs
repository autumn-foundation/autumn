//! Typed domain event bus with decoupled, durable listeners.
//!
//! A *domain event* is a typed value (declared with `#[event]`) describing
//! something that happened in your application — `UserSignedUp { user_id }`,
//! `OrderPlaced { .. }`. *Listeners* (declared with `#[listener]`) react to an
//! event independently of the code that emitted it: adding a new reaction is a
//! new listener and **zero edits** to the emitter.
//!
//! ```ignore
//! use autumn_web::prelude::*;
//!
//! #[event]
//! struct UserSignedUp { user_id: i64 }
//!
//! // Durable: rides the #[job] queue, survives restarts, retried on failure.
//! #[listener(UserSignedUp, durable)]
//! async fn send_welcome_email(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
//!     // ...
//!     Ok(())
//! }
//!
//! #[post("/signup")]
//! async fn signup(events: Events) -> AutumnResult<&'static str> {
//!     events.publish(UserSignedUp { user_id: 42 }).await?;
//!     Ok("ok")
//! }
//! ```
//!
//! # Dispatch
//!
//! - **Sync** listeners run in-request, before the response is returned — use
//!   these for invariants the caller depends on. Each runs independently with
//!   panic/error isolation: one failing listener never blocks the others, and
//!   never fails the publish.
//! - **Durable** listeners are enqueued onto the existing `#[job]` queue, so
//!   they survive a process restart and inherit the queue's retry + DLQ
//!   semantics (at-least-once delivery).
//!
//! A published event with no registered listeners is a **no-op**, not an error.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{AppState, AutumnError, AutumnResult};

/// A typed domain event.
///
/// Implemented by the `#[event]` macro, which also derives the serde +
/// `Clone`/`Debug` impls the bus needs to carry the payload across the durable
/// job queue.
pub trait Event: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Stable identifier used to route the event to its listeners and to name
    /// the durable listener jobs.
    const NAME: &'static str;
}

/// How a listener is dispatched when its event is published.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DispatchMode {
    /// Runs in-request, before the response, with panic/error isolation.
    Sync,
    /// Enqueued onto the `#[job]` queue (retry + DLQ + restart-safe).
    Durable,
}

/// The async function signature for an event listener.
///
/// Intentionally identical to [`crate::job::JobHandler`] so a durable listener
/// becomes a [`crate::job::JobInfo`] with no adapter.
pub type ListenerHandler =
    fn(AppState, Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;

/// Metadata describing a registered event listener.
///
/// Produced by the `#[listener]` macro's `__autumn_listener_info_*` companion
/// and collected by `listeners![]`.
#[derive(Clone)]
pub struct ListenerInfo {
    /// The [`Event::NAME`] this listener subscribes to.
    pub event_name: &'static str,
    /// Fully-qualified, per-listener identity (`module::fn`).
    pub listener_name: String,
    /// Sync (in-request) or Durable (job queue).
    pub mode: DispatchMode,
    /// For durable listeners, the registered job name; `None` for sync.
    pub job_name: Option<String>,
    /// Durable retry cap (mirrors [`crate::job::JobInfo`]); ignored for sync.
    pub max_attempts: u32,
    /// Durable initial backoff in ms; ignored for sync.
    pub initial_backoff_ms: u64,
    /// Runs the listener: deserialize the event payload, call the function.
    pub handler: ListenerHandler,
}

/// Routes published events to their registered listeners.
///
/// Built from the listeners registered with `AppBuilder::listeners` and
/// installed onto [`AppState`] as a typed extension.
#[derive(Clone, Default)]
pub struct EventRegistry {
    by_event: Arc<HashMap<&'static str, Vec<ListenerInfo>>>,
}

impl EventRegistry {
    /// Group listeners by event name, preserving registration order.
    #[must_use]
    pub fn from_listeners(listeners: Vec<ListenerInfo>) -> Self {
        let mut by_event: HashMap<&'static str, Vec<ListenerInfo>> = HashMap::new();
        for listener in listeners {
            by_event
                .entry(listener.event_name)
                .or_default()
                .push(listener);
        }
        Self {
            by_event: Arc::new(by_event),
        }
    }

    /// Listeners registered for `event_name` (empty slice if none).
    #[must_use]
    pub fn listeners_for(&self, event_name: &str) -> &[ListenerInfo] {
        self.by_event.get(event_name).map_or(&[][..], Vec::as_slice)
    }

    /// Synthesize a [`crate::job::JobInfo`] for each durable listener so the
    /// app builder can register them with the job runtime.
    ///
    /// # Panics
    ///
    /// Panics if a durable listener is missing its `job_name` (the `#[listener]`
    /// macro always sets one, so this only fires on a hand-built `ListenerInfo`).
    #[must_use]
    pub fn durable_job_infos(&self) -> Vec<crate::job::JobInfo> {
        self.by_event
            .values()
            .flatten()
            .filter(|listener| listener.mode == DispatchMode::Durable)
            .map(|listener| crate::job::JobInfo {
                name: listener
                    .job_name
                    .clone()
                    .expect("durable listener must carry a job_name"),
                max_attempts: listener.max_attempts,
                initial_backoff_ms: listener.initial_backoff_ms,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: listener.handler,
            })
            .collect()
    }
}

/// A single recorded publication, captured by [`EventRecorder`] in tests.
#[derive(Clone, Debug)]
pub struct RecordedEvent {
    /// The [`Event::NAME`] of the published event.
    pub event_name: &'static str,
    /// The serialized event payload.
    pub payload: Value,
}

/// Records published events so tests can assert on them without standing up the
/// job runner. Installed onto [`AppState`] by the test client.
#[derive(Default)]
pub struct EventRecorder {
    events: Mutex<Vec<RecordedEvent>>,
}

impl EventRecorder {
    fn record(&self, event_name: &'static str, payload: Value) {
        self.events
            .lock()
            .expect("event recorder lock poisoned")
            .push(RecordedEvent {
                event_name,
                payload,
            });
    }

    /// Deserialize every recorded publication of event type `E`.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's internal lock is poisoned.
    #[must_use]
    pub fn published<E: Event>(&self) -> Vec<E> {
        self.events
            .lock()
            .expect("event recorder lock poisoned")
            .iter()
            .filter(|recorded| recorded.event_name == E::NAME)
            .filter_map(|recorded| serde_json::from_value(recorded.payload.clone()).ok())
            .collect()
    }

    /// How many times event type `E` was published.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's internal lock is poisoned.
    #[must_use]
    pub fn count<E: Event>(&self) -> usize {
        self.events
            .lock()
            .expect("event recorder lock poisoned")
            .iter()
            .filter(|recorded| recorded.event_name == E::NAME)
            .count()
    }

    /// All recorded events, in publication order.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's internal lock is poisoned.
    #[must_use]
    pub fn all(&self) -> Vec<RecordedEvent> {
        self.events
            .lock()
            .expect("event recorder lock poisoned")
            .clone()
    }
}

/// Injectable event publisher.
///
/// Extracted in handlers/services just like the `Mailer`. Call
/// [`Events::publish`] to emit a typed event to its listeners.
#[derive(Clone)]
pub struct Events {
    registry: Arc<EventRegistry>,
    recorder: Option<Arc<EventRecorder>>,
    state: AppState,
}

impl Events {
    /// Publish a typed event to its registered listeners.
    ///
    /// Durable listeners are enqueued onto the job queue; sync listeners run
    /// in-request with panic/error isolation. A missing-listener event is a
    /// no-op. Returns `Ok(())` even when sync listeners fail (the emitter stays
    /// decoupled); only a durable **enqueue** failure propagates.
    ///
    /// # Errors
    ///
    /// Returns an error if the event cannot be serialized, or if enqueueing a
    /// durable listener onto the job queue fails.
    pub async fn publish<E: Event>(&self, event: E) -> AutumnResult<()> {
        let payload = serialize_event(&event)?;
        dispatch(
            &self.registry,
            self.recorder.as_deref(),
            &self.state,
            E::NAME,
            payload,
        )
        .await
    }
}

impl axum::extract::FromRequestParts<AppState> for Events {
    type Rejection = AutumnError;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A missing registry is not an error — publishing is then a safe no-op.
        let registry = state
            .extension::<EventRegistry>()
            .unwrap_or_else(|| Arc::new(EventRegistry::default()));
        let recorder = state.extension::<EventRecorder>();
        Ok(Self {
            registry,
            recorder,
            state: state.clone(),
        })
    }
}

fn serialize_event<E: Event>(event: &E) -> AutumnResult<Value> {
    serde_json::to_value(event).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "event serialization failed: {e}"
        )))
    })
}

/// Core dispatch shared by [`Events::publish`] and the module-level [`publish`].
async fn dispatch(
    registry: &EventRegistry,
    recorder: Option<&EventRecorder>,
    state: &AppState,
    event_name: &'static str,
    payload: Value,
) -> AutumnResult<()> {
    // 1. Record first so tests observe the event even without a job runner.
    if let Some(recorder) = recorder {
        recorder.record(event_name, payload.clone());
    }

    let listeners = registry.listeners_for(event_name);
    if listeners.is_empty() {
        return Ok(());
    }

    // 2. Durable listeners: enqueue onto the job queue (at-least-once).
    //
    // Prefer this app's own `JobClient` (installed onto `AppState` by the job
    // runtime) over the process-global client, so durable dispatch is scoped to
    // the publishing app rather than whichever app last started a runtime. This
    // keeps parallel in-process apps (notably tests) from contending. We fall
    // back to the global client only if no app-local one is present.
    let app_client = state.extension::<crate::job::JobClient>();
    let mut durable_error = None;
    for listener in listeners
        .iter()
        .filter(|listener| listener.mode == DispatchMode::Durable)
    {
        let job_name = listener
            .job_name
            .as_deref()
            .expect("durable listener must carry a job_name");
        // Enqueue *after commit* so publishing inside a `Db::tx` defers the
        // durable reaction until the transaction commits — a rolled-back event
        // never fires its listeners, and (on Postgres) the job is not claimed on
        // another connection before the event's data is visible. Outside a
        // transaction this enqueues immediately.
        let enqueued = if let Some(client) = &app_client {
            client.enqueue_after_commit(job_name, payload.clone()).await
        } else {
            crate::job::enqueue_after_commit(job_name, payload.clone()).await
        };
        // Don't let one durable enqueue failure skip the in-request sync
        // listeners (or the remaining durable enqueues); remember the first
        // error and surface it after sync listeners have run.
        if let Err(error) = enqueued
            && durable_error.is_none()
        {
            durable_error = Some(error);
        }
    }

    // 3. Sync listeners: run independently, isolated from each other — these run
    // even if a durable enqueue above failed.
    run_sync_listeners(state, listeners, &payload).await;

    durable_error.map_or(Ok(()), Err)
}

/// Run every sync listener concurrently on the caller's task, isolating each
/// from its siblings with `catch_unwind` (the same panic-isolation the job
/// runtime uses), then await them all (they finish before the response).
///
/// Running directly — rather than `tokio::spawn` — keeps the ambient app context
/// (`CURRENT_EVENT_APP`) and tracing span in scope, so a listener that itself
/// calls the free [`publish`] dispatches against the right app and stays
/// log-correlated. It also ties the listeners to the publish future's lifecycle,
/// so cancelling the request (timeout, disconnect) cancels the listeners instead
/// of leaving detached tasks running after the response is abandoned.
async fn run_sync_listeners(state: &AppState, listeners: &[ListenerInfo], payload: &Value) {
    use futures::FutureExt as _;

    let runs = listeners
        .iter()
        .filter(|listener| listener.mode == DispatchMode::Sync)
        .map(|listener| {
            let state = state.clone();
            let payload = payload.clone();
            let run = listener.handler;
            let name = listener.listener_name.clone();
            async move {
                match std::panic::AssertUnwindSafe(run(state, payload))
                    .catch_unwind()
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::error!(listener = %name, %error, "sync event listener failed");
                    }
                    Err(_panic) => {
                        tracing::error!(listener = %name, "sync event listener panicked");
                    }
                }
            }
        });
    futures::future::join_all(runs).await;
}

struct GlobalBus {
    registry: Arc<EventRegistry>,
    recorder: Option<Arc<EventRecorder>>,
    state: AppState,
}

static GLOBAL_EVENT_BUS: RwLock<Option<Arc<GlobalBus>>> = RwLock::new(None);

fn global_bus() -> Option<Arc<GlobalBus>> {
    GLOBAL_EVENT_BUS.read().ok().and_then(|guard| guard.clone())
}

/// Install the process-global event bus used by the module-level [`publish`].
///
/// Called by the app builder (and the test client) after the registry is built.
pub(crate) fn init_global_event_bus(
    registry: &EventRegistry,
    state: &AppState,
    recorder: Option<Arc<EventRecorder>>,
) {
    let bus = Arc::new(GlobalBus {
        registry: Arc::new(registry.clone()),
        recorder,
        state: state.clone(),
    });
    if let Ok(mut guard) = GLOBAL_EVENT_BUS.write() {
        *guard = Some(bus);
    }
}

/// Reset the process-global event bus (used for test isolation).
pub fn clear_global_event_bus() {
    if let Ok(mut guard) = GLOBAL_EVENT_BUS.write() {
        *guard = None;
    }
}

tokio::task_local! {
    /// The ambient app for the current request or job, used by the free
    /// [`publish`] so it resolves *this* app rather than the process-global bus.
    static CURRENT_EVENT_APP: AppState;
}

/// Run `future` with `state` installed as the ambient app for the free
/// [`publish`]. Scoped by the request pipeline and the job runtime so a handler,
/// service, or job that calls `publish` dispatches against its own app —
/// keeping parallel in-process apps (notably tests) isolated.
pub(crate) fn scope_event_app<F>(
    state: AppState,
    future: F,
) -> tokio::task::futures::TaskLocalFuture<AppState, F>
where
    F: Future,
{
    CURRENT_EVENT_APP.scope(state, future)
}

fn current_event_app() -> Option<AppState> {
    CURRENT_EVENT_APP.try_with(AppState::clone).ok()
}

/// Publish an event without a request context (services, jobs, scheduled tasks).
///
/// Resolves the **current app** from the ambient request/job context when one is
/// set (so parallel apps stay isolated), falling back to the process-global bus
/// installed at startup. Inside a request, the injectable [`Events`] extractor is
/// equivalent and slightly more explicit.
///
/// # Errors
///
/// Returns an error if the event cannot be serialized or if enqueueing a
/// durable listener fails. If no app context is available the call is a no-op.
pub async fn publish<E: Event>(event: E) -> AutumnResult<()> {
    let payload = serialize_event(&event)?;

    // Prefer the ambient app context (set per request and per job) for isolation.
    if let Some(state) = current_event_app() {
        let registry = state.extension::<EventRegistry>();
        let empty = EventRegistry::default();
        let registry_ref = registry.as_deref().unwrap_or(&empty);
        let recorder = state.extension::<EventRecorder>();
        return dispatch(registry_ref, recorder.as_deref(), &state, E::NAME, payload).await;
    }

    // No ambient app (e.g. a startup hook or bare task) — fall back to the
    // process-global bus, or no-op if nothing wired it.
    let Some(bus) = global_bus() else {
        return Ok(());
    };
    dispatch(
        &bus.registry,
        bus.recorder.as_deref(),
        &bus.state,
        E::NAME,
        payload,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Serialize, Deserialize, Clone, Debug)]
    struct Ping {
        n: i64,
    }
    impl Event for Ping {
        const NAME: &'static str = "Ping";
    }

    fn ok_handler() -> ListenerHandler {
        |_state, _payload| Box::pin(async { Ok(()) })
    }

    fn sync_listener(name: &str, handler: ListenerHandler) -> ListenerInfo {
        ListenerInfo {
            event_name: Ping::NAME,
            listener_name: name.to_string(),
            mode: DispatchMode::Sync,
            job_name: None,
            max_attempts: 0,
            initial_backoff_ms: 0,
            handler,
        }
    }

    fn durable_listener(name: &str) -> ListenerInfo {
        ListenerInfo {
            event_name: Ping::NAME,
            listener_name: name.to_string(),
            mode: DispatchMode::Durable,
            job_name: Some(format!("__event_listener::{name}")),
            max_attempts: 4,
            initial_backoff_ms: 250,
            handler: ok_handler(),
        }
    }

    #[test]
    fn registry_groups_by_event_name() {
        let registry = EventRegistry::from_listeners(vec![
            sync_listener("a", ok_handler()),
            sync_listener("b", ok_handler()),
        ]);
        assert_eq!(registry.listeners_for("Ping").len(), 2);
        assert!(registry.listeners_for("Other").is_empty());
    }

    #[test]
    fn durable_listeners_become_job_infos() {
        let registry = EventRegistry::from_listeners(vec![
            sync_listener("a", ok_handler()),
            durable_listener("seed_workspace"),
        ]);
        let jobs = registry.durable_job_infos();
        assert_eq!(jobs.len(), 1, "only durable listeners become jobs");
        assert_eq!(jobs[0].name, "__event_listener::seed_workspace");
        assert_eq!(jobs[0].max_attempts, 4);
        assert_eq!(jobs[0].initial_backoff_ms, 250);
    }

    #[tokio::test]
    async fn sync_listeners_are_isolated_from_panics_and_errors() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        RAN.store(0, Ordering::SeqCst);

        let panicking: ListenerHandler = |_state, _payload| Box::pin(async { panic!("boom") });
        let erroring: ListenerHandler = |_state, _payload| {
            Box::pin(async {
                Err(AutumnError::internal_server_error(std::io::Error::other(
                    "nope",
                )))
            })
        };
        let counting: ListenerHandler = |_state, _payload| {
            Box::pin(async {
                RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };

        let registry = EventRegistry::from_listeners(vec![
            sync_listener("panics", panicking),
            sync_listener("errors", erroring),
            sync_listener("counts", counting),
        ]);
        let state = AppState::for_test();

        // No recorder, no durable listeners: a panicking/erroring sibling must
        // not stop the third listener from running, and publish still succeeds.
        let result = dispatch(
            &registry,
            None,
            &state,
            Ping::NAME,
            serde_json::json!({"n": 1}),
        )
        .await;
        assert!(result.is_ok(), "publish stays Ok despite listener failures");
        assert_eq!(RAN.load(Ordering::SeqCst), 1, "surviving listener ran");
    }

    #[tokio::test]
    async fn sync_listeners_run_even_when_a_durable_enqueue_fails() {
        // With no app-local client and no global job runtime, the durable
        // enqueue fails — but the sync listener must still run (and the error
        // is surfaced afterwards rather than short-circuiting dispatch).
        static RAN: AtomicU32 = AtomicU32::new(0);
        crate::job::clear_global_job_client();
        RAN.store(0, Ordering::SeqCst);
        let counting: ListenerHandler = |_state, _payload| {
            Box::pin(async {
                RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        let registry = EventRegistry::from_listeners(vec![
            durable_listener("seed_workspace"),
            sync_listener("counts", counting),
        ]);
        let state = AppState::for_test();
        let _ = dispatch(
            &registry,
            None,
            &state,
            Ping::NAME,
            serde_json::json!({"n": 1}),
        )
        .await;
        // The key guarantee: the durable failure did not skip the sync listener.
        assert_eq!(RAN.load(Ordering::SeqCst), 1, "sync listener ran anyway");
    }

    #[tokio::test]
    async fn missing_listener_is_a_noop() {
        let registry = EventRegistry::default();
        let state = AppState::for_test();
        let result = dispatch(
            &registry,
            None,
            &state,
            Ping::NAME,
            serde_json::json!({"n": 1}),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn recorder_captures_published_events() {
        let registry = EventRegistry::default();
        let recorder = EventRecorder::default();
        let state = AppState::for_test();
        let payload = serialize_event(&Ping { n: 7 }).unwrap();
        dispatch(&registry, Some(&recorder), &state, Ping::NAME, payload)
            .await
            .unwrap();
        assert_eq!(recorder.count::<Ping>(), 1);
        let published = recorder.published::<Ping>();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].n, 7);
    }
}
