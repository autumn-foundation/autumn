//! Integration tests for the typed domain event bus (#1338).
//!
//! Acceptance criteria covered end-to-end through the in-process test client:
//! - a typed `#[event]` is published from a handler via the injectable `Events`
//!   extractor;
//! - multiple listeners on the same event each run, and a panicking/erroring
//!   listener does not stop the others;
//! - sync listeners run in-request before the response; durable listeners run
//!   via the in-process job runtime;
//! - adding a listener requires zero edits to the publishing handler;
//! - a published event with no listeners is a no-op;
//! - the test recorder lets `assert_event_published::<T>()` work without
//!   inspecting listeners.
//!
//! Listeners are plain `fn` pointers, so per-test observability lives in an
//! `AppState` extension (`Counters`) installed via `state_initializer` — this
//! keeps the tests isolated under parallel execution rather than racing on
//! process-global statics.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use autumn_web::events::Events;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::{event, get, listener, listeners, routes};

#[derive(Default)]
struct Counters {
    sync_a: AtomicU32,
    survivor: AtomicU32,
    durable: AtomicU32,
}

fn bump(state: &AppState, pick: fn(&Counters) -> &AtomicU32) {
    if let Some(counters) = state.extension::<Counters>() {
        pick(&counters).fetch_add(1, Ordering::SeqCst);
    }
}

fn read(state: &AppState, pick: fn(&Counters) -> &AtomicU32) -> u32 {
    state
        .extension::<Counters>()
        .map_or(0, |counters| pick(&counters).load(Ordering::SeqCst))
}

// ── A typed domain event ───────────────────────────────────────────
#[event]
struct UserSignedUp {
    user_id: i64,
}

#[event]
struct NothingListens {
    value: i64,
}

#[event]
struct ChainedFollowup {
    source_user: i64,
}

// ── Listeners (decoupled from the emitter) ─────────────────────────
#[listener(UserSignedUp)]
async fn sync_a(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    assert_eq!(event.user_id, 42);
    bump(&state, |c| &c.sync_a);
    Ok(())
}

#[listener(UserSignedUp)]
async fn sync_panics(_state: AppState, _event: UserSignedUp) -> AutumnResult<()> {
    panic!("this listener blows up but must not affect the others");
}

#[listener(UserSignedUp)]
async fn sync_survivor(state: AppState, _event: UserSignedUp) -> AutumnResult<()> {
    bump(&state, |c| &c.survivor);
    Ok(())
}

#[listener(UserSignedUp, durable, max_attempts = 3)]
async fn durable_seed(state: AppState, _event: UserSignedUp) -> AutumnResult<()> {
    bump(&state, |c| &c.durable);
    Ok(())
}

// A sync listener that itself publishes a follow-up event via the free
// publisher — exercising that the ambient app context survives into listeners.
#[listener(UserSignedUp)]
async fn republish_followup(_state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    autumn_web::events::publish(ChainedFollowup {
        source_user: event.user_id,
    })
    .await
}

/// The full listener set, including the durable one. Durable dispatch is scoped
/// to each app's own `JobClient` (installed on `AppState`), so several of these
/// tests can register the durable listener and run in parallel without stomping
/// on each other's job runtime.
fn all_listeners() -> Vec<autumn_web::events::ListenerInfo> {
    listeners![sync_a, sync_panics, sync_survivor, durable_seed]
}

// ── Emitting handler (never edited when listeners are added) ────────
#[get("/signup")]
async fn signup(events: Events) -> AutumnResult<&'static str> {
    events.publish(UserSignedUp { user_id: 42 }).await?;
    Ok("ok")
}

#[get("/quiet")]
async fn quiet(events: Events) -> AutumnResult<&'static str> {
    // No listener subscribes to this event — publishing must be a no-op.
    events.publish(NothingListens { value: 1 }).await?;
    Ok("ok")
}

#[get("/signup-free")]
async fn signup_free() -> AutumnResult<&'static str> {
    // Uses the module-level free publisher (no `Events` extractor). It must
    // resolve *this* request's app via the ambient context, not a global.
    autumn_web::events::publish(UserSignedUp { user_id: 42 }).await?;
    Ok("ok")
}

fn app_with(listeners: Vec<autumn_web::events::ListenerInfo>) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![signup, quiet, signup_free])
        .state_initializer(|state| state.insert_extension(Counters::default()))
        .listeners(listeners)
        .build()
}

#[tokio::test]
async fn publishing_runs_sync_listeners_with_panic_isolation() {
    let client = app_with(all_listeners());

    client.get("/signup").send().await.assert_ok();

    // Both non-panicking sync listeners ran despite a sibling panicking.
    assert_eq!(read(client.state(), |c| &c.sync_a), 1, "sync_a ran");
    assert_eq!(
        read(client.state(), |c| &c.survivor),
        1,
        "sync_survivor ran even though a sibling panicked"
    );
}

#[tokio::test]
async fn durable_listener_runs_via_the_job_runtime() {
    // Durable dispatch uses this app's own JobClient, so registering the durable
    // listener here is safe even though other parallel tests do the same.
    let client = app_with(all_listeners());

    client.get("/signup").send().await.assert_ok();

    // The durable listener is enqueued onto the job queue; under the in-process
    // local backend it runs shortly after. Poll briefly for it.
    let mut ran = false;
    for _ in 0..100 {
        if read(client.state(), |c| &c.durable) >= 1 {
            ran = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(ran, "durable listener should have run via the job runtime");
}

#[tokio::test]
async fn test_recorder_captures_published_events() {
    let client = app_with(all_listeners());

    client.get("/signup").send().await.assert_ok();

    client.assert_event_published::<UserSignedUp>();
    let published = client.published_events::<UserSignedUp>();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].user_id, 42);
}

#[tokio::test]
async fn event_with_no_listeners_is_a_noop() {
    // No listeners registered at all.
    let client = app_with(Vec::new());
    // Publishing an event nobody listens to must succeed, not error.
    client.get("/quiet").send().await.assert_ok();
    // It is still recorded, so the recorder works regardless of listeners.
    client.assert_event_published::<NothingListens>();
}

#[tokio::test]
async fn adding_a_listener_needs_zero_emitter_edits() {
    // The emitter (`signup`) is identical here; we register only one listener.
    // This is the "+1 file, 0 emitter edits" property: registration is the only
    // thing that changes when a reaction is added.
    let client = app_with(listeners![sync_a]);

    client.get("/signup").send().await.assert_ok();
    assert_eq!(read(client.state(), |c| &c.sync_a), 1);
    // The other listeners were not registered, so they never ran.
    assert_eq!(read(client.state(), |c| &c.survivor), 0);
}

#[tokio::test]
async fn free_publisher_dispatches_to_the_current_app() {
    // A handler using the module-level `events::publish` (not the extractor)
    // must still reach this app's listeners and recorder via the ambient context.
    let client = app_with(all_listeners());

    client.get("/signup-free").send().await.assert_ok();

    client.assert_event_published::<UserSignedUp>();
    assert_eq!(
        read(client.state(), |c| &c.sync_a),
        1,
        "sync_a ran via free publish"
    );
}

#[tokio::test]
async fn sync_listener_can_publish_a_followup_event() {
    // The sync listener runs on the request's task and calls the free publisher;
    // the ambient app context must carry through so the follow-up is recorded
    // against this app (it would be lost if listeners were detached via spawn).
    let client = TestApp::new()
        .routes(routes![signup])
        .state_initializer(|state| state.insert_extension(Counters::default()))
        .listeners(listeners![republish_followup])
        .build();

    client.get("/signup").send().await.assert_ok();

    client.assert_event_published::<UserSignedUp>();
    client.assert_event_published::<ChainedFollowup>();
    let followups = client.published_events::<ChainedFollowup>();
    assert_eq!(followups.len(), 1);
    assert_eq!(followups[0].source_user, 42);
}

#[tokio::test]
async fn free_publisher_is_isolated_across_parallel_apps() {
    // Two apps live at once. Publishing through the free function on app A must
    // not leak into app B's recorder — proving the path is app-scoped, not global.
    let app_a = app_with(all_listeners());
    let app_b = app_with(all_listeners());

    app_a.get("/signup-free").send().await.assert_ok();

    assert_eq!(
        app_a.published_events::<UserSignedUp>().len(),
        1,
        "app A recorded its own event"
    );
    assert_eq!(
        app_b.published_events::<UserSignedUp>().len(),
        0,
        "app B must not see app A's event"
    );
}

// Keep an explicit reference so the unused-import lints stay quiet if the test
// set shrinks during refactors.
#[allow(dead_code)]
fn _type_anchor(_: Arc<Counters>) {}
