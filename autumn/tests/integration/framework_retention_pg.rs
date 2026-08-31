//! Postgres-backed sweeps for the unified framework-owned data-retention
//! policy (issue #1605).
//!
//! **Requires Docker** to be running.
//!
//! Proves AC #2 end to end for every sweep-enforced dataset: records aged
//! past their window are removed on a run while newer records survive; a dry
//! run counts without deleting; a GDPR legal hold (AC #5) vetoes the dataset
//! entirely; and every run that touched a dataset leaves an auditable record
//! carrying the dataset, the cutoff, and the number of rows removed (AC #6).

#![cfg(feature = "db")]

use std::sync::Arc;

use autumn_web::AppState;
use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink, AuditStatus};
use autumn_web::config::AutumnConfig;
use autumn_web::data_retention::{RetentionRunOptions, run_retention};
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
use diesel::Connection as _;
use diesel::PgConnection;
use diesel::connection::SimpleConnection as _;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::Mutex;

const JOBS_DDL: &str = include_str!("../../migrations/20260513000000_create_job_queue/up.sql");
const JOB_TRACKING_DDL: &str =
    include_str!("../../migrations/20260702000000_create_job_tracking/up.sql");
const EXPERIMENTS_DDL: &str =
    include_str!("../../migrations/20260530300000_create_experiments/up.sql");
const COMMIT_HOOKS_DDL: &str =
    include_str!("../../migrations/20260515000000_create_repository_commit_hook_queue/up.sql");
const UNIQUENESS_DDL: &str =
    include_str!("../../migrations/20260610000000_add_job_uniqueness_concurrency/up.sql");
const RETENTION_INDEX_DDL: &str =
    include_str!("../../migrations/20260831000000_retention_sweep_indexes/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct TimestampRow {
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    value: chrono::DateTime<chrono::Utc>,
}

#[derive(diesel::QueryableByName)]
struct TextRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

/// An audit sink that records every event, so a test can assert on the
/// retention record's own contents.
#[derive(Default)]
struct RecordingAuditSink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

impl AuditSink for RecordingAuditSink {
    fn write(
        &self,
        event: AuditEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AuditError>> + Send + '_>>
    {
        let events = self.events.clone();
        Box::pin(async move {
            events.lock().await.push(event);
            Ok(())
        })
    }
}

async fn start_pg() -> (
    Pool<AsyncPgConnection>,
    String,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let mut sync_conn = PgConnection::establish(&url).expect("db connection");
    for ddl in [
        JOBS_DDL,
        UNIQUENESS_DDL,
        JOB_TRACKING_DDL,
        EXPERIMENTS_DDL,
        COMMIT_HOOKS_DDL,
        RETENTION_INDEX_DDL,
    ] {
        sync_conn.batch_execute(ddl).expect("migration");
    }

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");
    (pool, url, container)
}

/// Insert one `autumn_jobs` row with the given status and `finished_at`
/// offset (negative days = in the past).
async fn insert_job(pool: &Pool<AsyncPgConnection>, id: &str, status: &str, age_days: i64) {
    let mut conn = pool.get().await.expect("conn");
    let finished = if status == "enqueued" {
        "NULL".to_owned()
    } else {
        format!("NOW() - INTERVAL '{age_days} days'")
    };
    diesel::sql_query(format!(
        "INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at) \
         VALUES ($1, 'demo', $2, NOW() - INTERVAL '{age_days} days', {finished})"
    ))
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(status)
    .execute(&mut conn)
    .await
    .expect("insert job");
}

async fn insert_tracking(pool: &Pool<AsyncPgConnection>, key: &str, age_days: i64) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "INSERT INTO autumn_job_tracking (key, record, updated_at, expires_at) \
         VALUES ($1, '{{}}'::JSONB, NOW() - INTERVAL '{age_days} days', \
                 NOW() + INTERVAL '365 days')"
    ))
    .bind::<diesel::sql_types::Text, _>(key)
    .execute(&mut conn)
    .await
    .expect("insert tracking");
}

/// Insert an assignment for experiment `exp`, which by default has no
/// `autumn_experiments` row at all — the "the experiment is gone" case.
async fn insert_assignment(pool: &Pool<AsyncPgConnection>, actor: &str, age_days: i64) {
    insert_assignment_for(pool, "exp", actor, age_days).await;
}

async fn insert_assignment_for(
    pool: &Pool<AsyncPgConnection>,
    experiment: &str,
    actor: &str,
    age_days: i64,
) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "INSERT INTO autumn_experiment_assignments (experiment, actor, variant, assigned_at) \
         VALUES ($1, $2, 'control', NOW() - INTERVAL '{age_days} days')"
    ))
    .bind::<diesel::sql_types::Text, _>(experiment)
    .bind::<diesel::sql_types::Text, _>(actor)
    .execute(&mut conn)
    .await
    .expect("insert assignment");
}

/// Create an experiment row in the given state.
async fn insert_experiment(pool: &Pool<AsyncPgConnection>, name: &str, state: &str) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "INSERT INTO autumn_experiments (name, state) \
         VALUES ($1, '{state}'::autumn_experiment_state)"
    ))
    .bind::<diesel::sql_types::Text, _>(name)
    .execute(&mut conn)
    .await
    .expect("insert experiment");
}

/// Insert a terminal `#[after_commit]` hook row.
async fn insert_commit_hook(pool: &Pool<AsyncPgConnection>, id: &str, status: &str, age_days: i64) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "INSERT INTO autumn_repository_commit_hooks \
             (id, handler_key, hook_name, status, enqueued_at, finished_at) \
         VALUES ($1, 'h', 'after_save', $2, NOW() - INTERVAL '{age_days} days', \
                 NOW() - INTERVAL '{age_days} days')"
    ))
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(status)
    .execute(&mut conn)
    .await
    .expect("insert commit hook");
}

/// Insert a terminal job row that still holds a TTL-window dedup key.
async fn insert_ttl_unique_job(pool: &Pool<AsyncPgConnection>, id: &str, age_days: i64) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "INSERT INTO autumn_jobs \
             (id, name, status, enqueued_at, finished_at, unique_key, unique_window) \
         VALUES ($1, 'welcome_email', 'completed', NOW() - INTERVAL '{age_days} days', \
                 NOW() - INTERVAL '{age_days} days', $1, 'ttl')"
    ))
    .bind::<diesel::sql_types::Text, _>(id)
    .execute(&mut conn)
    .await
    .expect("insert ttl-unique job");
}

async fn count(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count")
        .count
}

async fn ids(pool: &Pool<AsyncPgConnection>, sql: &str) -> Vec<String> {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .load::<TextRow>(&mut conn)
        .await
        .expect("ids")
        .into_iter()
        .map(|row| row.value)
        .collect()
}

/// An `AppState` carrying the pool and a config with the given windows.
fn state_with(pool: &Pool<AsyncPgConnection>, config: AutumnConfig) -> AppState {
    let state = AppState::for_test().with_pool(pool.clone());
    state.insert_extension(config);
    state
}

fn config_with_windows() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.retention.job_history = Some("30d".to_owned());
    config.retention.job_tracking = Some("30d".to_owned());
    config.retention.experiment_assignments = Some("30d".to_owned());
    // The job-tracking window competes with this knob; keep it wide so the
    // policy window is the bound under test.
    config.jobs.tracking.ttl_secs = 365 * 86_400;
    config
}

// ── AC #2: aged records are removed, newer records survive ───────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_removes_aged_records_and_leaves_newer_ones() {
    let (pool, _url, _container) = start_pg().await;

    insert_job(&pool, "old-completed", "completed", 90).await;
    insert_job(&pool, "old-failed", "failed", 90).await;
    insert_job(&pool, "recent-completed", "completed", 1).await;
    insert_tracking(&pool, "old-key", 90).await;
    insert_tracking(&pool, "recent-key", 1).await;
    insert_assignment(&pool, "old-actor", 90).await;
    insert_assignment(&pool, "recent-actor", 1).await;

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(&state, &RetentionRunOptions::default())
        .await
        .expect("sweep runs");

    let job_history = reports
        .iter()
        .find(|r| r.dataset == "job_history")
        .expect("job_history reported");
    assert_eq!(job_history.error, None, "{job_history:?}");
    assert_eq!(job_history.rows_removed, 2);
    assert_eq!(job_history.eligible_rows, Some(2));

    assert_eq!(
        ids(&pool, "SELECT id AS value FROM autumn_jobs ORDER BY id").await,
        vec!["recent-completed".to_owned()],
        "only the recent job row survives"
    );
    assert_eq!(
        ids(
            &pool,
            "SELECT key AS value FROM autumn_job_tracking ORDER BY key"
        )
        .await,
        vec!["recent-key".to_owned()]
    );
    assert_eq!(
        ids(
            &pool,
            "SELECT actor AS value FROM autumn_experiment_assignments ORDER BY actor"
        )
        .await,
        vec!["recent-actor".to_owned()]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_never_touches_a_live_job_row_however_old() {
    // The single most dangerous thing a job-history sweep could do: delete a
    // job that has not run yet. An enqueued row has no `finished_at`, and a
    // row waiting on a retry is back in `enqueued` — neither may ever match.
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "ancient-enqueued", "enqueued", 400).await;
    insert_job(&pool, "ancient-running", "running", 400).await;
    insert_job(&pool, "ancient-completed", "completed", 400).await;

    let state = state_with(&pool, config_with_windows());
    run_retention(&state, &RetentionRunOptions::default())
        .await
        .expect("sweep runs");

    let survivors = ids(&pool, "SELECT id AS value FROM autumn_jobs ORDER BY id").await;
    assert_eq!(
        survivors,
        vec!["ancient-enqueued".to_owned(), "ancient-running".to_owned()],
        "only the terminal row may be swept"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_dry_run_counts_without_deleting() {
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "old-1", "completed", 90).await;
    insert_job(&pool, "old-2", "completed", 90).await;

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: true,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("dry run");

    assert_eq!(reports.len(), 1, "--dataset narrows the run");
    assert_eq!(reports[0].eligible_rows, Some(2));
    assert_eq!(reports[0].rows_removed, 0);
    assert!(reports[0].dry_run);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS count FROM autumn_jobs").await,
        2
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn no_configured_window_sweeps_nothing() {
    // AC #1: a dataset nothing bounds is left entirely alone. (The
    // "not even a query is issued" half of that guarantee belongs to
    // `framework_retention_task` returning `None`, which is asserted in
    // `framework_retention.rs` — a `run_retention` call, as here, still
    // *counts* any dataset that has a bound from a subsystem knob.)
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "ancient", "completed", 4_000).await;

    let state = state_with(&pool, AutumnConfig::default());
    let reports = run_retention(&state, &RetentionRunOptions::default())
        .await
        .expect("run");

    let job_history = reports
        .iter()
        .find(|r| r.dataset == "job_history")
        .expect("reported");
    assert_eq!(job_history.rows_removed, 0);
    assert_eq!(job_history.window_secs, None);
    assert_eq!(
        job_history.skipped.as_deref(),
        Some("no retention window configured")
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS count FROM autumn_jobs").await,
        1
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_batches_past_its_batch_size() {
    // The sweep deletes in bounded batches; a table with more stale rows than
    // one batch must still end up fully drained in a single run.
    let (pool, _url, _container) = start_pg().await;
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at) \
             SELECT 'job-' || g, 'demo', 'completed', NOW() - INTERVAL '90 days', \
                    NOW() - INTERVAL '90 days' \
             FROM generate_series(1, 1200) AS g",
        )
        .execute(&mut conn)
        .await
        .expect("bulk insert");
    }

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].rows_removed, 1_200);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS count FROM autumn_jobs").await,
        0
    );
}

// ── AC #3: precedence with the pre-existing knobs ────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_shorter_of_the_policy_and_tracking_ttl_is_what_gets_swept() {
    let (pool, _url, _container) = start_pg().await;
    insert_tracking(&pool, "two-days-old", 2).await;
    insert_tracking(&pool, "ten-days-old", 10).await;

    // Policy 30d, subsystem TTL 5d ⇒ the 5d bound governs, so only the
    // ten-day-old record is stale.
    let mut config = AutumnConfig::default();
    config.retention.job_tracking = Some("30d".to_owned());
    config.jobs.tracking.ttl_secs = 5 * 86_400;

    let state = state_with(&pool, config);
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_tracking"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].source, "jobs.tracking.ttl_secs");
    assert_eq!(reports[0].rows_removed, 1);
    assert_eq!(
        ids(
            &pool,
            "SELECT key AS value FROM autumn_job_tracking ORDER BY key"
        )
        .await,
        vec!["two-days-old".to_owned()]
    );
}

// ── AC #5: legal hold ────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_legal_hold_stops_the_sweep_from_deleting_anything() {
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "held-1", "completed", 400).await;
    insert_job(&pool, "held-2", "failed", 400).await;
    insert_assignment(&pool, "not-held", 400).await;

    let state = state_with(&pool, config_with_windows());
    state.insert_extension(GdprRegistry::new().register(ModelRegistration::retain(
        "autumn_jobs",
        "litigation hold 2026-CV-1",
    )));

    let reports = run_retention(&state, &RetentionRunOptions::default())
        .await
        .expect("run");

    let job_history = reports
        .iter()
        .find(|r| r.dataset == "job_history")
        .expect("reported");
    assert_eq!(job_history.rows_removed, 0);
    assert_eq!(
        job_history.skipped.as_deref(),
        Some("legal hold: litigation hold 2026-CV-1")
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS count FROM autumn_jobs").await,
        2,
        "a held dataset must not lose a single row"
    );
    // The hold is per dataset, not global: an unheld dataset still sweeps.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS count FROM autumn_experiment_assignments"
        )
        .await,
        0
    );
}

// ── AC #6: every sweep emits an auditable record ─────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_sweep_emits_an_audit_record_with_dataset_cutoff_and_rows() {
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "old", "completed", 90).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, config_with_windows());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));

    run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    // Cloned out of the guard so the lock is not held across the
    // assertions below (clippy::significant_drop_tightening).
    let recorded = events.lock().await.clone();
    let event = recorded
        .iter()
        .find(|event| event.action == "retention.sweep")
        .expect("a retention sweep must be audited");
    assert_eq!(event.status, AuditStatus::Success);
    assert_eq!(event.target_resource_id, "job_history");
    assert_eq!(
        event.metadata.get("dataset").map(String::as_str),
        Some("job_history")
    );
    assert_eq!(
        event.metadata.get("rows_removed").map(String::as_str),
        Some("1")
    );
    let cutoff = event.metadata.get("cutoff").expect("cutoff recorded");
    assert!(
        chrono::DateTime::parse_from_rfc3339(cutoff).is_ok(),
        "the cutoff must be a real RFC-3339 timestamp: {cutoff}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_dataset_held_back_by_a_legal_hold_is_still_audited() {
    // "The policy wanted to delete this and did not" is exactly what a
    // compliance reviewer needs to see.
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "held", "completed", 400).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, config_with_windows());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));
    state.insert_extension(
        GdprRegistry::new().register(ModelRegistration::retain("autumn_jobs", "tax records")),
    );

    run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("run");

    // Cloned out of the guard so the lock is not held across the
    // assertions below (clippy::significant_drop_tightening).
    let recorded = events.lock().await.clone();
    let event = recorded
        .iter()
        .find(|event| event.action == "retention.sweep")
        .expect("a legal-hold skip must still be audited");
    assert_eq!(
        event.metadata.get("skipped").map(String::as_str),
        Some("legal hold: tax records")
    );
    assert_eq!(
        event.metadata.get("rows_removed").map(String::as_str),
        Some("0")
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn only_datasets_that_are_actually_bounded_are_audited() {
    // An audit trail that records "did nothing" for every unbounded dataset
    // buries the entries that matter. A default config bounds three datasets
    // — `job_tracking` (jobs.tracking.ttl_secs), `idempotency` and `sessions`
    // (both 24h) — but only `job_tracking` is *sweep*-enforced, and only a
    // sweep is a deletion worth auditing.
    let (pool, _url, _container) = start_pg().await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, AutumnConfig::default());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));

    run_retention(&state, &RetentionRunOptions::default())
        .await
        .expect("run");

    // Cloned out of the guard so the lock is not held across the
    // assertions below (clippy::significant_drop_tightening).
    let recorded = events.lock().await.clone();
    let datasets: Vec<&str> = recorded
        .iter()
        .map(|event| event.target_resource_id.as_str())
        .collect();
    assert_eq!(
        datasets,
        vec!["job_tracking"],
        "only the dataset with an actual retention bound may be audited"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_dry_run_writes_no_audit_record() {
    // A dry run deletes nothing, so it is not a sweep and must not appear in
    // the compliance trail as one.
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "old", "completed", 90).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, config_with_windows());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));

    run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: true,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("dry run");

    assert!(events.lock().await.is_empty());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_that_removed_nothing_is_still_audited() {
    // "We enforced the policy and there was nothing to delete" is a claim a
    // compliance reviewer needs evidence for.
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "recent", "completed", 1).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, config_with_windows());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));

    run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    // Cloned out of the guard so the lock is not held across the
    // assertions below (clippy::significant_drop_tightening).
    let recorded = events.lock().await.clone();
    let event = recorded
        .iter()
        .find(|event| event.target_resource_id == "job_history")
        .expect("a zero-row sweep is still a sweep");
    assert_eq!(
        event.metadata.get("rows_removed").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        event.metadata.get("eligible_rows").map(String::as_str),
        Some("0")
    );
}

// ── Predicates that guard other subsystems' invariants ───────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_removes_discarded_jobs_too() {
    // `discarded` is a terminal status set by an operator cancel or discard,
    // with `finished_at` recorded. Excluding it would leave every cancelled
    // job accumulating forever despite a declared window — the exact
    // unbounded growth the policy exists to stop.
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "old-discarded", "discarded", 90).await;
    insert_job(&pool, "recent-discarded", "discarded", 1).await;

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].rows_removed, 1);
    assert_eq!(
        ids(&pool, "SELECT id AS value FROM autumn_jobs ORDER BY id").await,
        vec!["recent-discarded".to_owned()]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_never_deletes_a_live_ttl_uniqueness_token() {
    // `#[job(unique, unique_for_ms = N)]` enforces its window purely by the
    // historical row's continued existence — a completed twin is what
    // suppresses a duplicate enqueue. Deleting it would silently run the job
    // a second time.
    let (pool, _url, _container) = start_pg().await;
    insert_ttl_unique_job(&pool, "welcome-user-42", 400).await;
    insert_job(&pool, "ordinary-completed", "completed", 400).await;

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].rows_removed, 1);
    assert_eq!(
        ids(&pool, "SELECT id AS value FROM autumn_jobs ORDER BY id").await,
        vec!["welcome-user-42".to_owned()],
        "the dedup token must survive; only the ordinary row is swept"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_sweep_keeps_assignments_for_a_running_experiment() {
    // A sticky assignment is what keeps an actor on one variant while an
    // experiment runs. Deleting it re-buckets that actor through the current
    // weights, contaminating the results, and can admit them into a sibling
    // experiment in the same exclusion group.
    let (pool, _url, _container) = start_pg().await;
    insert_experiment(&pool, "live", "running").await;
    insert_experiment(&pool, "finished", "concluded").await;
    insert_assignment_for(&pool, "live", "actor-live", 400).await;
    insert_assignment_for(&pool, "finished", "actor-finished", 400).await;
    // No `autumn_experiments` row at all — the experiment is gone.
    insert_assignment_for(&pool, "orphan", "actor-orphan", 400).await;

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("experiment_assignments"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].rows_removed, 2);
    assert_eq!(
        ids(
            &pool,
            "SELECT actor AS value FROM autumn_experiment_assignments ORDER BY actor"
        )
        .await,
        vec!["actor-live".to_owned()],
        "only a running experiment's assignments survive"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_commit_hook_queue_is_swept_like_job_history() {
    let (pool, _url, _container) = start_pg().await;
    insert_commit_hook(&pool, "old-hook", "completed", 90).await;
    insert_commit_hook(&pool, "recent-hook", "completed", 1).await;
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO autumn_repository_commit_hooks (id, handler_key, hook_name, status) \
             VALUES ('live-hook', 'h', 'after_save', 'enqueued')",
        )
        .execute(&mut conn)
        .await
        .expect("insert live hook");
    }

    let mut config = AutumnConfig::default();
    config.retention.commit_hooks = Some("30d".to_owned());
    let state = state_with(&pool, config);
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("commit_hooks"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].rows_removed, 1);
    assert_eq!(
        ids(
            &pool,
            "SELECT id AS value FROM autumn_repository_commit_hooks ORDER BY id"
        )
        .await,
        vec!["live-hook".to_owned(), "recent-hook".to_owned()],
        "an unfinished hook is never swept, however old"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_cutoff_comes_from_the_database_clock() {
    // The columns the predicates compare against are all written with the
    // database's `NOW()`, so the cutoff has to come from the same clock —
    // otherwise a replica running fast deletes rows younger than the window.
    // Asserting it against the database's own `NOW() - INTERVAL` is the only
    // check that can tell the two clocks apart.
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "old", "completed", 90).await;

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: true,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("dry run");

    let reported = reports[0].cutoff.as_deref().expect("a cutoff is reported");
    let reported = chrono::DateTime::parse_from_rfc3339(reported).expect("an RFC-3339 cutoff");

    let mut conn = pool.get().await.expect("conn");
    let expected = diesel::sql_query("SELECT NOW() - INTERVAL '30 days' AS value")
        .get_result::<TimestampRow>(&mut conn)
        .await
        .expect("db cutoff")
        .value;

    let drift = (expected - reported.with_timezone(&chrono::Utc))
        .num_seconds()
        .abs();
    assert!(
        drift < 60,
        "the reported cutoff must be the database's NOW() minus the window \
         (expected≈{expected}, reported={reported}, drift={drift}s)"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_failing_dataset_is_reported_without_aborting_the_run() {
    // One dataset erroring must never stop another table from being bounded,
    // and the failure has to reach the report (and the audit trail) rather
    // than looking like a clean sweep of zero rows.
    let (pool, _url, _container) = start_pg().await;
    insert_assignment(&pool, "old-actor", 400).await;
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("DROP TABLE autumn_jobs")
            .execute(&mut conn)
            .await
            .expect("drop autumn_jobs");
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, config_with_windows());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));

    let reports = run_retention(&state, &RetentionRunOptions::default())
        .await
        .expect("the run itself must not fail");

    let job_history = reports
        .iter()
        .find(|r| r.dataset == "job_history")
        .expect("reported");
    assert!(job_history.error.is_some(), "{job_history:?}");
    let assignments = reports
        .iter()
        .find(|r| r.dataset == "experiment_assignments")
        .expect("reported");
    assert_eq!(assignments.error, None);
    assert_eq!(
        assignments.rows_removed, 1,
        "a sibling dataset must still be swept"
    );

    // Cloned out of the guard so the lock is not held across the
    // assertions below (clippy::significant_drop_tightening).
    let recorded = events.lock().await.clone();
    let failure = recorded
        .iter()
        .find(|event| event.target_resource_id == "job_history")
        .expect("a failed sweep must still be audited");
    assert_eq!(failure.status, AuditStatus::Failure);
    assert!(failure.metadata.contains_key("error"), "{failure:?}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_short_batch_does_not_end_the_sweep_while_rows_remain() {
    // Regression (#1605 Codex round 2): the loop used to stop on any batch
    // smaller than the batch size, and reported `truncated = false` from it.
    // A batch is short whenever rows stopped qualifying between the
    // sub-select and the re-checked outer delete, so that inferred both the
    // stop and the completeness wrongly. Completion is now a fresh count.
    let (pool, _url, _container) = start_pg().await;
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at) \
             SELECT 'job-' || g, 'demo', 'completed', NOW() - INTERVAL '90 days', \
                    NOW() - INTERVAL '90 days' \
             FROM generate_series(1, 1700) AS g",
        )
        .execute(&mut conn)
        .await
        .expect("bulk insert");
    }

    let state = state_with(&pool, config_with_windows());
    let reports = run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    assert_eq!(reports[0].rows_removed, 1_700);
    assert!(
        !reports[0].truncated,
        "a fully drained table must never be reported as truncated: {:?}",
        reports[0]
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS count FROM autumn_jobs").await,
        0
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_completed_sweep_reports_truncated_false_and_audits_it() {
    let (pool, _url, _container) = start_pg().await;
    insert_job(&pool, "old", "completed", 90).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let state = state_with(&pool, config_with_windows());
    state.insert_extension(AuditLogger::new().with_sink(Arc::new(RecordingAuditSink {
        events: events.clone(),
    })));

    run_retention(
        &state,
        &RetentionRunOptions {
            dry_run: false,
            dataset: Some("job_history"),
        },
    )
    .await
    .expect("sweep");

    let recorded = events.lock().await.clone();
    let event = recorded
        .iter()
        .find(|event| event.target_resource_id == "job_history")
        .expect("audited");
    assert_eq!(
        event.metadata.get("truncated").map(String::as_str),
        Some("false"),
        "a drained sweep must record completeness honestly: {event:?}"
    );
}
