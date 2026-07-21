//! Startup-migration proof for the `SQLite` runtime lane (issue #1614, PR3).
//!
//! PR2 built the `SQLite` deadpool pool + boot/serve but *gated* registered
//! startup migrations (a `sqlite://` target with `.migrations(...)` failed
//! fast). PR3 replaces that gate with a working `SQLite` migration path. This
//! test exercises the whole chain the way an app does:
//!
//! 1. **Registered migrations apply** — an [`EmbeddedMigrations`] set (exactly
//!    the type [`AppBuilder::migrations`](autumn_web) stores) is applied to a
//!    `sqlite://` file target through the new
//!    [`autumn_web::migrate::run_pending_sqlite`] path — diesel's
//!    `MigrationHarness` on a `SqliteConnection`, with **no** Postgres advisory
//!    lock.
//! 2. **The migrated schema is visible to the runtime pool** — the public
//!    [`autumn_web::db::create_pool`] entry builds the real `SQLite` runtime
//!    pool over the *same* database file, and a checked-out connection reads and
//!    writes the migration-created `widgets` table.
//! 3. **A DB-backed request serves** — a minimal Axum router holds the pool in
//!    state and answers an HTTP request by `INSERT`ing then `SELECT`ing from
//!    `widgets`, driving one real request/response through the migrated schema.
//!
//! A **file** target (tempfile) is deliberate: an in-memory `SQLite` database is
//! private per connection, so migrations applied on the (separate) migration
//! connection would be invisible to the pool. File targets share the database.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_migrations
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations, run_pending_sqlite};
use autumn_web::reexports::{axum, diesel, diesel_async};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use tower::ServiceExt as _; // for `oneshot`

/// The app-registered migration set — the identical `EmbeddedMigrations` type
/// `.migrations(MIGRATIONS)` stores. It creates the `widgets` table.
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sqlite_migrations");

/// A multi-migration set (six `CREATE TABLE` steps, each without `IF NOT
/// EXISTS`) used by the concurrency test. Several migrations widen the
/// read→plan→apply window so racing migrators genuinely overlap, and a
/// double-applied step errors hard ("table already exists") — so an unserialized
/// path reliably surfaces a false failure, while the write-locked path drains the
/// set exactly once.
const CONCURRENT_MIGRATIONS: EmbeddedMigrations =
    embed_migrations!("tests/fixtures/sqlite_migrations_concurrent");

/// The runtime pool type. Under `--features sqlite` `RuntimeConnection` resolves
/// to `SyncConnectionWrapper<SqliteConnection>`.
type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct Widget {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// Answers `POST /widgets` by writing a row into the migration-created table and
/// reading it back — proving the migrated schema is live through the pool.
async fn create_and_read_widget(State(pool): State<SqlitePool>) -> Result<String, StatusCode> {
    let mut conn = pool
        .get()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    diesel::sql_query("INSERT INTO widgets (id, name) VALUES (1, 'sprocket')")
        .execute(&mut *conn)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows: Vec<Widget> = diesel::sql_query("SELECT name FROM widgets WHERE id = 1")
        .load(&mut *conn)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    rows.into_iter()
        .next()
        .map(|w| w.name)
        .ok_or(StatusCode::NOT_FOUND)
}

#[tokio::test]
async fn registered_migrations_apply_to_sqlite_and_serve() {
    // A tempfile-backed database so the migration connection and every pooled
    // connection observe the same `widgets` table.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("migrate.db");
    let url = format!("sqlite://{}", db_path.display());

    // (1) Apply the registered migrations through the new SQLite path (no
    //     advisory lock). This is the behavior PR2 rejected with a fail-fast
    //     gate; PR3 makes it work.
    let result =
        run_pending_sqlite(&url, MIGRATIONS).expect("registered migrations apply on sqlite");
    assert_eq!(
        result.applied.len(),
        1,
        "exactly the one registered migration is applied (got {:?})",
        result.applied
    );

    // Re-running is a no-op: the migration is already recorded.
    let again =
        run_pending_sqlite(&url, MIGRATIONS).expect("re-running pending migrations is a no-op");
    assert!(
        again.applied.is_empty(),
        "second run applies nothing (got {:?})",
        again.applied
    );

    // (2) Build the real SQLite runtime pool over the same file via the public
    //     `create_pool` entry, and confirm the migrated `widgets` table exists
    //     on a checked-out pooled connection.
    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        // If the migration had not applied, this SELECT would error ("no such
        // table: widgets").
        let rows: Vec<Widget> = diesel::sql_query("SELECT name FROM widgets WHERE 1 = 0")
            .load(&mut *conn)
            .await
            .expect("the migrated `widgets` table is visible to the runtime pool");
        assert!(rows.is_empty(), "no rows seeded yet");
    }

    // (3) A minimal router backed by the SQLite pool serves a DB-backed request
    //     against the migrated schema.
    let app: Router = Router::new()
        .route("/widgets", post(create_and_read_widget))
        .with_state(pool);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/widgets")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router serves the request");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "DB-backed route against the migrated schema is 200"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        &body[..],
        b"sprocket",
        "response body is the row written to and read from the migrated `widgets` table"
    );
}

/// A **private** in-memory target with registered migrations is rejected up
/// front with an actionable error (issue #1614 follow-up). Each `:memory:`
/// connection is its own empty database, so migrations applied on the transient
/// migration connection could never reach the runtime pool — surfacing the
/// error beats silently applying to a database the pool never sees. The remedy
/// is a **file-backed** database only.
#[test]
fn private_in_memory_target_with_registered_migrations_is_rejected() {
    for url in [
        "sqlite::memory:",
        ":memory:",
        "sqlite://:memory:",
        "file::memory:",
    ] {
        let err = run_pending_sqlite(url, MIGRATIONS)
            .expect_err("private in-memory + registered migrations must be rejected");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("in-memory"),
            "error names the in-memory problem (got {msg:?})"
        );
        assert!(
            msg.contains("file-backed"),
            "error gives the file-backed remedy (got {msg:?})"
        );
    }
}

/// A **shared-cache** in-memory target (`file::memory:?cache=shared`) with
/// registered migrations is ALSO rejected up front (issue #1614 follow-up).
/// Although a shared-cache database is shareable across the runtime pool's live
/// connections, the migration runs on a *transient* synchronous connection and
/// SQLite destroys a shared in-memory database the moment its last connection
/// closes; the runtime deadpool is created lazily and may not have anchored a
/// connection yet, so the pool's first checkout opens a fresh, empty database
/// and every DB-backed request then 500s. The remedy is a **file-backed**
/// database — the corrected recommendation no longer suggests `cache=shared`.
/// (The private-vs-shared sizing classification stays unchanged and is unit
/// tested in `db::sqlite_target_is_any_in_memory_covers_shared_cache`.)
#[test]
fn shared_cache_in_memory_target_with_registered_migrations_is_rejected() {
    let err = run_pending_sqlite("file::memory:?cache=shared", MIGRATIONS).expect_err(
        "shared-cache in-memory + registered migrations must be rejected: the schema is \
         lost before the runtime pool anchors it",
    );
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("in-memory"),
        "error names the in-memory problem (got {msg:?})"
    );
    assert!(
        msg.contains("file-backed"),
        "error gives the file-backed remedy (got {msg:?})"
    );
    assert!(
        !msg.contains("`file::memory:?cache=shared`"),
        "the corrected message no longer recommends a shared-cache in-memory URL as a remedy \
         (got {msg:?})"
    );
}

/// Many concurrent `run_pending_sqlite` calls against the SAME file-backed
/// database must ALL succeed — the winner applies the pending set once, every
/// loser queues on the write lock, re-reads an already-drained pending set, and
/// cleanly no-ops — instead of a loser re-running an already-applied migration
/// and reporting a false failure (issue #2065, deferred from PR #2062).
///
/// Before the shared `BEGIN IMMEDIATE` serialization primitive, the
/// list→plan→apply window was unlocked, so racing migrators read the same empty
/// applied set and double-applied the same `up.sql`, and the loser surfaced a
/// failed migration. The single-file write lock now serializes the whole
/// sequence, so a racer that waits and then finds nothing pending is a clean
/// no-op, never an error.
#[test]
fn concurrent_run_pending_sqlite_serializes_without_false_failure() {
    use std::sync::{Arc, Barrier};

    // Enough racers to reliably force the wait-on-lock path.
    const THREADS: usize = 8;
    // The six-step set widens the race window; a double-applied step errors.
    const EXPECTED_APPLIED: usize = 6;

    // A file target so every migration connection contends on the same on-disk
    // write lock (a `:memory:` target is private per connection — no contention).
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("concurrent.db");
    let url: Arc<str> = Arc::from(format!("sqlite://{}", db_path.display()));

    // A `Barrier` releases every racer together so their read→apply windows
    // genuinely overlap.
    let barrier = Arc::new(Barrier::new(THREADS));

    // Spawn ALL threads before joining ANY: collecting the handles first is
    // load-bearing (fusing the spawn+join iterators would join each thread
    // before spawning the next, serialising the racers and defeating the test).
    #[allow(clippy::needless_collect)]
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let url = Arc::clone(&url);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                run_pending_sqlite(&url, CONCURRENT_MIGRATIONS)
            })
        })
        .collect();

    let results: Vec<Result<_, _>> = handles
        .into_iter()
        .map(|h| h.join().expect("migration thread did not panic"))
        .collect();

    // (1) No racer reports a false failure.
    for (i, result) in results.iter().enumerate() {
        assert!(
            result.is_ok(),
            "concurrent migrator {i} reported a failure instead of a clean no-op: {:?}",
            result.as_ref().err()
        );
    }

    // (2) Every registered migration is applied EXACTLY once across all racers:
    //     the winner (holding the write lock) drains the whole set, and every
    //     loser then reads an already-empty pending set. A double-applied step
    //     under an unserialized path would instead error above.
    let per_thread_applied: Vec<usize> = results
        .iter()
        .map(|r| r.as_ref().expect("ok checked above").applied.len())
        .collect();
    let total_applied: usize = per_thread_applied.iter().sum();
    assert_eq!(
        total_applied, EXPECTED_APPLIED,
        "each registered migration must apply exactly once across all racers \
         (per-thread applied counts = {per_thread_applied:?})"
    );

    // (3) The schema converged and the migrations are recorded: a fresh
    //     `run_pending_sqlite` now finds nothing pending (idempotent no-op).
    let after = run_pending_sqlite(&url, CONCURRENT_MIGRATIONS)
        .expect("a post-convergence run is a clean no-op, not a failure");
    assert!(
        after.applied.is_empty(),
        "after the concurrent batch converged, re-running applies nothing (got {:?})",
        after.applied
    );
}
