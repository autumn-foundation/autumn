//! AC7 for issue #1598: the whole loop, end to end.
//!
//! Phase 1 runs a real request against a real PostgreSQL through the
//! *recording* pool: a route reads rows with diesel and then fails, and the
//! failure-capture layer writes a capsule to disk. Phase 2 loads that capsule
//! back off disk — nothing is passed in memory between the phases — rebuilds
//! the same routes over the *replay* pool and the capsule's clock, and drives
//! the recorded request through the replay driver.
//!
//! The assertion is the acceptance criterion itself: the verdict is
//! [`Verdict::Reproduced`], with no database divergence, and the replayed
//! response is the response the client originally got — for a returned 500 and
//! for a panic alike.
//!
//! Docker-gated: phase 1 needs a live PostgreSQL. Phase 2 needs no database at
//! all (the "server" is an in-process stub fed from the capsule), which is the
//! point.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use autumn_web::AutumnError;
use autumn_web::capsule::{
    Capsule, CapsuleOutcome, DivergenceLog, ReplayClock, Verdict, execute, load_capsule,
    pool_from_capsule,
};
use autumn_web::config::AutumnConfig;
use autumn_web::db::Db;
use autumn_web::test::{TestApp, TestDb};
use autumn_web::{get, routes};
use chrono::{DateTime, Utc};
use diesel_async::RunQueryDsl as _;

// ── The application under capture ───────────────────────────────────────────

#[derive(diesel::QueryableByName, Debug)]
struct Widget {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

const WIDGETS_SQL: &str =
    "SELECT id, name FROM capsule_e2e_widgets WHERE kind = $1 ORDER BY id ASC";

async fn load_widgets(db: &mut Db, kind: &str) -> Result<Vec<Widget>, AutumnError> {
    diesel::sql_query(WIDGETS_SQL)
        .bind::<diesel::sql_types::Text, _>(kind.to_owned())
        .load(&mut **db)
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(format!("query failed: {error}")))
}

/// Reads real rows through the pool, then returns a 500 that quotes them — so
/// a replay that did not serve the same rows could not produce the same body.
#[get("/widgets/broken")]
async fn widgets_broken(mut db: Db) -> Result<String, AutumnError> {
    let widgets = load_widgets(&mut db, "tool").await?;
    Err(AutumnError::internal_server_error_msg(format!(
        "read {} widget(s) ({}) and then exploded",
        widgets.len(),
        widgets
            .iter()
            .map(|widget| format!("{}:{}", widget.id, widget.name))
            .collect::<Vec<_>>()
            .join(",")
    )))
}

/// Same read, but the handler panics instead of returning an error.
#[get("/widgets/panic")]
async fn widgets_panic(mut db: Db) -> String {
    let widgets = load_widgets(&mut db, "tool")
        .await
        .expect("the seeded rows must load");
    panic!("kaboom after reading {} widget(s)", widgets.len());
}

// ── Harness ─────────────────────────────────────────────────────────────────

/// Create and seed the table both phases read. Idempotent: the shared
/// testcontainer is reused across the whole binary.
async fn seed(url_source: &TestDb) {
    url_source
        .execute_sql(
            "CREATE TABLE IF NOT EXISTS capsule_e2e_widgets (
                 id BIGINT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 name TEXT NOT NULL
             )",
        )
        .await;
    url_source.execute_sql("DELETE FROM capsule_e2e_widgets").await;
    url_source
        .execute_sql(
            "INSERT INTO capsule_e2e_widgets (id, kind, name) VALUES
                 (1, 'tool', 'anvil'), (2, 'tool', 'rope'), (3, 'toy', 'kite')",
        )
        .await;
}

fn capture_config(dir: &Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.failure_capture.enabled = true;
    config.failure_capture.dir = dir.to_string_lossy().into_owned();
    config
}

/// Phase 2 must not capture a capsule of the replay itself.
fn replay_config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.failure_capture.enabled = false;
    config
}

fn capsule_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
}

/// The capsule is written from the reporting layer's detached task, so wait for
/// it rather than racing it.
async fn await_one_capsule(dir: &Path) -> PathBuf {
    for _ in 0..250 {
        if let [path] = capsule_paths(dir).as_slice() {
            return path.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "the failing request must leave exactly one capsule in {}, found {:?}",
        dir.display(),
        capsule_paths(dir)
    );
}

/// A [`ReplayClock`] the test keeps a handle on: `TestApp::with_clock` takes
/// ownership, and the driver needs the same clock to count over-reads.
struct SharedClock(Arc<ReplayClock>);

impl autumn_web::time::ClockSource for SharedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0.now()
    }
}

fn replay_clock(capsule: &Capsule) -> Arc<ReplayClock> {
    // `as_slice` disambiguates from diesel's `RunQueryDsl::first`, which is in
    // scope here and applies to `Vec` without an autoderef.
    let fallback = capsule
        .clock
        .as_slice()
        .first()
        .copied()
        .unwrap_or(capsule.captured_at);
    Arc::new(ReplayClock::new(capsule.clock.clone(), fallback))
}

fn status_of(outcome: &CapsuleOutcome) -> u16 {
    match outcome {
        CapsuleOutcome::Status { code, .. } => *code,
        CapsuleOutcome::Panic { status, .. } => *status,
    }
}

// ── AC7 ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn forced_500_on_a_db_reading_route_is_captured_and_replayed_identically() {
    let db = TestDb::shared().await;
    seed(db).await;
    let dir = tempfile::tempdir().expect("tempdir");

    // ── Phase 1: capture ────────────────────────────────────────────────
    let recording_pool =
        autumn_web::capsule::build_recording_pool(db.url(), 2, Duration::from_secs(10))
            .expect("recording pool builds");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![widgets_broken])
        .with_db(recording_pool)
        .build();

    client.get("/widgets/broken").send().await.assert_status(500);

    let path = await_one_capsule(dir.path()).await;
    // Nothing is handed between the phases in memory: the capsule is read back
    // off disk exactly as `autumn replay` would read it.
    let capsule = load_capsule(&path).expect("the written capsule must load");
    assert!(!capsule.truncated, "a two-row read must fit the budget");
    assert_eq!(status_of(&capsule.outcome), 500);
    // The client sees a generic problem document; the capsule keeps the real
    // error message, which here quotes the rows the handler read.
    let CapsuleOutcome::Status {
        message: recorded_message,
        ..
    } = &capsule.outcome
    else {
        panic!("a returned error must record a Status outcome: {:?}", capsule.outcome);
    };
    assert!(
        recorded_message.contains("read 2 widget(s) (1:anvil,2:rope)"),
        "the recorded failure must quote the rows it read: {recorded_message}"
    );
    let tape = capsule
        .db
        .as_ref()
        .expect("the capsule must carry the database traffic the handler generated");
    assert!(
        tape.connections
            .iter()
            .any(|connection| connection
                .exchanges
                .iter()
                .any(|exchange| exchange.sql == WIDGETS_SQL)),
        "the handler's SELECT must be on the tape: {:?}",
        tape.connections
            .iter()
            .flat_map(|connection| connection.exchanges.iter())
            .map(|exchange| exchange.sql.as_str())
            .collect::<Vec<_>>()
    );

    // ── Phase 2: replay, with no database at all ────────────────────────
    let divergences = Arc::new(DivergenceLog::new());
    let replay_pool =
        pool_from_capsule(&capsule, Arc::clone(&divergences)).expect("replay pool builds");
    let clock = replay_clock(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![widgets_broken])
        .with_db(replay_pool)
        .with_clock(SharedClock(Arc::clone(&clock)))
        .build()
        .into_router();

    let outcome = execute(router, &capsule, divergences, Some(clock.as_ref())).await;

    assert!(
        outcome.divergences.is_empty(),
        "a faithful replay must stay on the tape: {:?}",
        outcome.divergences
    );
    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the recorded 500 must reproduce offline: {outcome:?}"
    );
    assert_eq!(
        status_of(&outcome.actual),
        status_of(&capsule.outcome),
        "the replayed response must be the recorded one: {outcome:?}"
    );
    let CapsuleOutcome::Status {
        message: replayed_message,
        ..
    } = &outcome.actual
    else {
        panic!("the replay must produce a Status outcome: {:?}", outcome.actual);
    };
    assert_eq!(
        replayed_message, recorded_message,
        "the replayed handler must have decoded the same rows out of the tape"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_panicking_db_reading_route_is_captured_and_replayed_identically() {
    let db = TestDb::shared().await;
    seed(db).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let recording_pool =
        autumn_web::capsule::build_recording_pool(db.url(), 2, Duration::from_secs(10))
            .expect("recording pool builds");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![widgets_panic])
        .with_db(recording_pool)
        .build();

    client.get("/widgets/panic").send().await.assert_status(500);

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the written capsule must load");
    let CapsuleOutcome::Panic { payload, .. } = &capsule.outcome else {
        panic!(
            "a panicking handler must be recorded as a panic, got {:?}",
            capsule.outcome
        );
    };
    assert!(
        payload.contains("kaboom after reading 2 widget(s)"),
        "the recorded panic must carry the payload the rows produced: {payload}"
    );

    let divergences = Arc::new(DivergenceLog::new());
    let replay_pool =
        pool_from_capsule(&capsule, Arc::clone(&divergences)).expect("replay pool builds");
    let clock = replay_clock(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![widgets_panic])
        .with_db(replay_pool)
        .with_clock(SharedClock(Arc::clone(&clock)))
        .build()
        .into_router();

    let outcome = execute(router, &capsule, divergences, Some(clock.as_ref())).await;

    assert!(
        outcome.divergences.is_empty(),
        "a faithful replay must stay on the tape: {:?}",
        outcome.divergences
    );
    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the recorded panic must reproduce offline: {outcome:?}"
    );
    assert_eq!(status_of(&outcome.actual), 500, "{outcome:?}");
}
