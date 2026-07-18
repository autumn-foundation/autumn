//! Durable repository commit-hook worker on the `SQLite` runtime backend
//! (issue #1996 item 5).
//!
//! Ports the Postgres durable dispatcher (`FOR UPDATE SKIP LOCKED` claim + NOTIFY
//! kick + retry/backoff + dead-letter + stale recovery + graceful drain) to the
//! `SQLite` backend flip, where the claim is a `BEGIN IMMEDIATE` single-writer
//! transaction and the kick is an in-process `Notify`. These tests drive the real
//! worker (`start_repository_commit_hook_worker`) and the real enqueue surface
//! (`enqueue_repository_commit_hook_on_conn`) against an in-memory `SQLite`
//! database — no Docker — and prove:
//!
//!  * (a) a queued hook survives a "restart" (enqueued before any worker exists)
//!    and is delivered exactly once, with request-level idempotency dedup holding;
//!  * (b) transient failures retry on the recorded backoff and then succeed;
//!  * (c) a permanently-failing hook is dead-lettered (`status = 'failed'`) after
//!    `max_attempts`;
//!  * (d) a graceful shutdown settles the in-flight leased entry before exiting;
//!  * (e) tenant isolation is INVIOLABLE and fail-closed: a hook enqueued under
//!    tenant A executes with tenant A's scope and writes ONLY tenant A's data even
//!    while tenant B has its own queued hook, and a hook enqueued with NO tenant
//!    fails closed (it never runs against a default/global scope).
//!
//! Only meaningful under `--features sqlite`; the file is `#![cfg(feature = "sqlite")]`
//! so a default `cargo test` compiles it to an empty (passing) binary. Run:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_commit_hook_worker`.
#![cfg(feature = "sqlite")]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async, serde_json, tokio, tokio_util};
use autumn_web::tenancy::{CURRENT_TENANT, with_tenant};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

type SqlitePool = Pool<RuntimeConnection>;

static HANDLER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Read an atomic counter. Fully qualified because `diesel_async::RunQueryDsl`'s
/// blanket `load(self, conn)` is in scope and otherwise shadows `AtomicU64::load`.
fn load(counter: &std::sync::atomic::AtomicU64) -> u64 {
    std::sync::atomic::AtomicU64::load(counter, Ordering::SeqCst)
}

/// A leaked, process-unique handler key so each test registers an isolated runner
/// (the runner registry is global; a shared key would let one test's worker run a
/// different test's closure over the wrong pool).
fn unique_handler_key() -> &'static str {
    let n = HANDLER_SEQ.fetch_add(1, Ordering::Relaxed);
    Box::leak(format!("test::sqlite_commit_hook_worker::handler::{n}").into_boxed_str())
}

mod schema {
    autumn_web::reexports::diesel::table! {
        hook_notes (id) {
            id -> Int8,
            body -> Text,
            tenant_id -> Text,
        }
    }
}

use schema::hook_notes;

// A tenant-scoped target the durable hook writes into. Proving a hook's write
// lands under the ORIGINATING tenant (and never another tenant) is the strongest
// adversarial tenant-isolation assertion.
#[autumn_web::model(table = "hook_notes")]
pub struct HookNote {
    #[id]
    pub id: i64,
    pub body: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(HookNote, table = "hook_notes", tenant_scoped)]
pub trait HookNoteRepository {}

async fn boot(db_name: &str) -> SqlitePool {
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");

    let mut conn = pool.get().await.expect("checkout sqlite connection");
    diesel::sql_query(
        "CREATE TABLE hook_notes (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             body TEXT NOT NULL, \
             tenant_id TEXT NOT NULL\
         )",
    )
    .execute(&mut conn)
    .await
    .expect("create hook_notes table");
    // The SQLite durable commit-hook queue (same DDL the #1996 item-5 migration
    // installs; indexes omitted — they are a pure perf concern for these tests).
    diesel::sql_query(
        "CREATE TABLE autumn_repository_commit_hooks (\
             id                  TEXT    PRIMARY KEY, \
             handler_key         TEXT    NOT NULL, \
             hook_name           TEXT    NOT NULL, \
             context             TEXT    NOT NULL DEFAULT '{}', \
             record              TEXT    NOT NULL DEFAULT '{}', \
             status              TEXT    NOT NULL DEFAULT 'enqueued', \
             attempt             INTEGER NOT NULL DEFAULT 1, \
             max_attempts        INTEGER NOT NULL DEFAULT 5, \
             initial_backoff_ms  BIGINT  NOT NULL DEFAULT 1000, \
             enqueued_at         TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             started_at          TEXT, \
             finished_at         TEXT, \
             claimed_by          TEXT, \
             claimed_at          TEXT, \
             last_error          TEXT, \
             run_at              TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP\
         )",
    )
    .execute(&mut conn)
    .await
    .expect("create commit hook queue table");
    pool
}

#[derive(diesel::QueryableByName)]
struct StatusRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    last_error: Option<String>,
}

async fn hook_row_status(pool: &SqlitePool, id: &str) -> Option<(String, Option<String>)> {
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT status, last_error FROM autumn_repository_commit_hooks WHERE id = ?")
        .bind::<diesel::sql_types::Text, _>(id)
        .get_result::<StatusRow>(&mut conn)
        .await
        .ok()
        .map(|row| (row.status, row.last_error))
}

async fn count_notes(pool: &SqlitePool, tenant: &str) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT COUNT(*) AS n FROM hook_notes WHERE tenant_id = ?")
        .bind::<diesel::sql_types::Text, _>(tenant)
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count notes")
        .n
}

async fn total_notes(pool: &SqlitePool) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT COUNT(*) AS n FROM hook_notes")
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count notes")
        .n
}

/// Enqueue a durable hook through the REAL enqueue surface, inside the ambient
/// tenant scope (so the SQLite worker's tenant round-trip is exercised).
async fn enqueue(
    pool: &SqlitePool,
    handler_key: &str,
    idempotency_key: Option<&str>,
    record: &serde_json::Value,
) {
    let mut conn = pool.get().await.expect("checkout");
    autumn_web::__private::enqueue_repository_commit_hook_on_conn(
        &mut conn,
        handler_key,
        "create",
        idempotency_key,
        None,
        &json!({ "op": "create" }),
        record,
    )
    .await
    .expect("enqueue durable hook");
}

/// Insert a queue row directly so tests can control `initial_backoff_ms` /
/// `max_attempts` (the public enqueue hardcodes 1000ms / 5, too slow for
/// retry/dead-letter timing).
async fn insert_raw_row(
    pool: &SqlitePool,
    id: &str,
    handler_key: &str,
    max_attempts: i32,
    initial_backoff_ms: i64,
) {
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query(
        "INSERT INTO autumn_repository_commit_hooks \
         (id, handler_key, hook_name, context, record, status, attempt, max_attempts, \
          initial_backoff_ms, enqueued_at, run_at) \
         VALUES (?, ?, 'create', '{}', '{}', 'enqueued', 1, ?, ?, \
                 '2000-01-01 00:00:00.000', '2000-01-01 00:00:00.000')",
    )
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(handler_key)
    .bind::<diesel::sql_types::Integer, _>(max_attempts)
    .bind::<diesel::sql_types::BigInt, _>(initial_backoff_ms)
    .execute(&mut conn)
    .await
    .expect("insert raw hook row");
}

/// Insert a queue row with a caller-supplied `context` JSON so a test can simulate
/// a persisted row whose embedded `__autumn_tenant` is blank / whitespace (as an
/// older, pre-fix producer — or a tampered row — could have written). Exercises the
/// EXTRACT path's fail-closed normalization independent of the enqueue surface.
async fn insert_raw_row_with_context(
    pool: &SqlitePool,
    id: &str,
    handler_key: &str,
    context: &str,
) {
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query(
        "INSERT INTO autumn_repository_commit_hooks \
         (id, handler_key, hook_name, context, record, status, attempt, max_attempts, \
          initial_backoff_ms, enqueued_at, run_at) \
         VALUES (?, ?, 'create', ?, '{}', 'enqueued', 1, 5, 1000, \
                 '2000-01-01 00:00:00.000', '2000-01-01 00:00:00.000')",
    )
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(handler_key)
    .bind::<diesel::sql_types::Text, _>(context)
    .execute(&mut conn)
    .await
    .expect("insert raw hook row with context");
}

/// Count notes whose `tenant_id` is empty or whitespace-only — the exact rows a
/// fail-closed worker must NEVER create from a blank/whitespace tenant scope.
async fn count_blank_tenant_notes(pool: &SqlitePool) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT COUNT(*) AS n FROM hook_notes WHERE trim(tenant_id) = ''")
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count blank-tenant notes")
        .n
}

/// Read the persisted `context` JSON of the single queue row (embed-path assertion).
async fn single_row_context(pool: &SqlitePool) -> Option<String> {
    #[derive(diesel::QueryableByName)]
    struct ContextRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        context: String,
    }
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT context FROM autumn_repository_commit_hooks LIMIT 1")
        .get_result::<ContextRow>(&mut conn)
        .await
        .ok()
        .map(|row| row.context)
}

async fn wait_until<F, Fut>(label: &str, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if predicate().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {label}");
}

// ── (a) delivered-once after simulated restart + idempotency dedup ────────────

#[tokio::test]
async fn queued_hook_survives_restart_and_is_delivered_exactly_once_on_sqlite() {
    let pool = boot("hook_deliver_once").await;
    let handler_key = unique_handler_key();

    let observed = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let runner_pool = pool.clone();
    let runner_observed = observed.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let pool = runner_pool.clone();
            let observed = runner_observed.clone();
            async move {
                let tenant = CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
                observed.lock().unwrap().push(tenant);
                let repo = PgHookNoteRepository::with_pool_untracked(pool);
                repo.save(&NewHookNote {
                    body: "delivered".to_string(),
                })
                .await?;
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    // Enqueue TWICE with the same idempotency key BEFORE any worker exists — the
    // second call collapses onto the first (ON CONFLICT DO NOTHING). The row is
    // durable in the queue table; the worker starts "after a restart".
    with_tenant("tenant-a".to_string(), async {
        enqueue(&pool, handler_key, Some("idem-1"), &json!({ "id": 1 })).await;
        enqueue(&pool, handler_key, Some("idem-1"), &json!({ "id": 1 })).await;
    })
    .await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    wait_until("hook delivered", || {
        let observed = observed.clone();
        async move { observed.lock().unwrap().len() >= 1 }
    })
    .await;
    // Give any (erroneous) duplicate delivery a chance to also land before asserting once.
    tokio::time::sleep(Duration::from_millis(400)).await;
    shutdown.cancel();

    let runs = observed.lock().unwrap().clone();
    assert_eq!(
        runs.len(),
        1,
        "the durable hook must be delivered exactly once (idempotency dedup), got: {runs:?}"
    );
    assert_eq!(
        runs[0].as_deref(),
        Some("tenant-a"),
        "the worker must re-establish the originating tenant before running the hook"
    );
    assert_eq!(
        count_notes(&pool, "tenant-a").await,
        1,
        "the hook's tenant-scoped write must land under tenant-a exactly once"
    );
    assert_eq!(
        count_notes(&pool, "tenant-b").await,
        0,
        "the hook must never write another tenant's data"
    );
}

// ── (e) adversarial cross-tenant isolation ────────────────────────────────────

#[tokio::test]
async fn cross_tenant_hooks_never_leak_across_tenants_on_sqlite() {
    let pool = boot("hook_cross_tenant").await;
    let handler_key = unique_handler_key();

    let runner_pool = pool.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, record| {
            let pool = runner_pool.clone();
            async move {
                let body = record
                    .get("body")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let repo = PgHookNoteRepository::with_pool_untracked(pool);
                repo.save(&NewHookNote { body }).await?;
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    with_tenant("tenant-a".to_string(), async {
        enqueue(
            &pool,
            handler_key,
            Some("a-1"),
            &json!({ "id": 1, "body": "from-a" }),
        )
        .await;
    })
    .await;
    with_tenant("tenant-b".to_string(), async {
        enqueue(
            &pool,
            handler_key,
            Some("b-1"),
            &json!({ "id": 2, "body": "from-b" }),
        )
        .await;
    })
    .await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    wait_until("both hooks delivered", {
        let pool = pool.clone();
        move || {
            let pool = pool.clone();
            async move { total_notes(&pool).await >= 2 }
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    shutdown.cancel();

    // Each tenant sees ONLY its own hook's write; neither leaked into the other.
    assert_eq!(
        count_notes(&pool, "tenant-a").await,
        1,
        "tenant-a note count"
    );
    assert_eq!(
        count_notes(&pool, "tenant-b").await,
        1,
        "tenant-b note count"
    );

    let repo = PgHookNoteRepository::with_pool_untracked(pool.clone());
    let a_notes = with_tenant("tenant-a".to_string(), async {
        repo.find_all().await.expect("list tenant-a")
    })
    .await;
    let b_notes = with_tenant("tenant-b".to_string(), async {
        repo.find_all().await.expect("list tenant-b")
    })
    .await;
    assert_eq!(
        a_notes.iter().map(|n| n.body.as_str()).collect::<Vec<_>>(),
        vec!["from-a"],
        "tenant-a must only contain its own hook's write"
    );
    assert_eq!(
        b_notes.iter().map(|n| n.body.as_str()).collect::<Vec<_>>(),
        vec!["from-b"],
        "tenant-b must only contain its own hook's write"
    );
}

// ── (e) missing tenant fails closed ───────────────────────────────────────────

#[tokio::test]
async fn hook_with_no_tenant_fails_closed_and_writes_nothing_on_sqlite() {
    let pool = boot("hook_fail_closed").await;
    let handler_key = unique_handler_key();

    let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner_pool = pool.clone();
    let runner_attempts = attempts.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let pool = runner_pool.clone();
            let attempts = runner_attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                // A tenant_scoped write with NO ambient tenant must error — the
                // worker must NOT have established a default/global scope.
                let repo = PgHookNoteRepository::with_pool_untracked(pool);
                repo.save(&NewHookNote {
                    body: "should-never-persist".to_string(),
                })
                .await?;
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    // Enqueue OUTSIDE any tenant scope → no tenant embedded in context.
    enqueue(&pool, handler_key, Some("no-tenant"), &json!({ "id": 9 })).await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    // Wait until the hook has been attempted at least once, then confirm nothing persisted.
    wait_until("hook attempted", {
        let attempts = attempts.clone();
        move || {
            let attempts = attempts.clone();
            async move { load(&attempts) >= 1 }
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.cancel();

    assert_eq!(
        total_notes(&pool).await,
        0,
        "a hook with no tenant context must fail closed — never writing to any (default/global) scope"
    );
}

// ── (e) blank / whitespace persisted tenant fails closed (EXTRACT path) ───────

// Regression for the P1 tenant-isolation finding: a persisted `__autumn_tenant`
// that is `""` or whitespace-only must be treated as UNSET, not as an established
// tenant. Without the extract-side normalization the drain loop scopes
// `CURRENT_TENANT` to the blank string and a `tenant_scoped` repo happily
// reads/writes `tenant_id = ''` — a fail-closed breach. Both blank and whitespace
// rows must run with the tenant UNSET and therefore write NOTHING.
#[tokio::test]
async fn hook_with_blank_or_whitespace_persisted_tenant_fails_closed_on_sqlite() {
    let pool = boot("hook_blank_tenant").await;
    let handler_key = unique_handler_key();

    let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner_pool = pool.clone();
    let runner_attempts = attempts.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let pool = runner_pool.clone();
            let attempts = runner_attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                // The persisted tenant was blank/whitespace: the worker must have
                // left CURRENT_TENANT UNSET, so this tenant_scoped write fails
                // closed rather than persisting a tenant_id = '' (or whitespace) row.
                let repo = PgHookNoteRepository::with_pool_untracked(pool);
                repo.save(&NewHookNote {
                    body: "should-never-persist".to_string(),
                })
                .await?;
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    // Two already-persisted rows whose embedded __autumn_tenant is blank / whitespace.
    insert_raw_row_with_context(
        &pool,
        "blank-tenant",
        handler_key,
        r#"{"op":"create","__autumn_tenant":""}"#,
    )
    .await;
    insert_raw_row_with_context(
        &pool,
        "whitespace-tenant",
        handler_key,
        r#"{"op":"create","__autumn_tenant":"   "}"#,
    )
    .await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    // Both rows must be attempted at least once (the fail-closed write errors).
    wait_until("both blank/whitespace hooks attempted", {
        let attempts = attempts.clone();
        move || {
            let attempts = attempts.clone();
            async move { load(&attempts) >= 2 }
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.cancel();

    // Critical negative: NO row was written under a blank/whitespace tenant.
    assert_eq!(
        count_blank_tenant_notes(&pool).await,
        0,
        "a blank/whitespace persisted tenant must fail closed — never writing a tenant_id = '' (or whitespace) row"
    );
    assert_eq!(
        total_notes(&pool).await,
        0,
        "a hook whose persisted tenant is blank/whitespace must write NOTHING (fail closed)"
    );
}

// ── (e) blank / whitespace ambient tenant is never embedded (EMBED path) ──────

// Defense-in-depth for the same finding at the producer side: a blank/whitespace
// ambient tenant must never be persisted into the context in the first place, so a
// blank tenant is neither stored nor trusted. The enqueued hook then runs with the
// tenant UNSET and its tenant_scoped write fails closed.
#[tokio::test]
async fn blank_ambient_tenant_is_never_embedded_on_sqlite() {
    let pool = boot("hook_blank_embed").await;
    let handler_key = unique_handler_key();

    let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner_pool = pool.clone();
    let runner_attempts = attempts.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let pool = runner_pool.clone();
            let attempts = runner_attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                let repo = PgHookNoteRepository::with_pool_untracked(pool);
                repo.save(&NewHookNote {
                    body: "should-never-persist".to_string(),
                })
                .await?;
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    // Enqueue inside a whitespace-only ambient tenant scope → the embed site must
    // persist NO __autumn_tenant key (blank is never written).
    with_tenant("   ".to_string(), async {
        enqueue(&pool, handler_key, Some("blank-embed"), &json!({ "id": 7 })).await;
    })
    .await;

    let context = single_row_context(&pool).await.expect("one queue row");
    assert!(
        !context.contains("__autumn_tenant"),
        "a blank/whitespace ambient tenant must never be embedded into the context, got: {context}"
    );

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    wait_until("blank-embed hook attempted", {
        let attempts = attempts.clone();
        move || {
            let attempts = attempts.clone();
            async move { load(&attempts) >= 1 }
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.cancel();

    assert_eq!(
        count_blank_tenant_notes(&pool).await,
        0,
        "a hook enqueued under a blank/whitespace tenant must fail closed — no tenant_id = '' row"
    );
    assert_eq!(
        total_notes(&pool).await,
        0,
        "a hook enqueued under a blank/whitespace tenant must write NOTHING (fail closed)"
    );
}

// ── (b) retry / backoff on transient failure ──────────────────────────────────

#[tokio::test]
async fn transient_hook_failure_retries_then_succeeds_on_sqlite() {
    let pool = boot("hook_retry").await;
    let handler_key = unique_handler_key();

    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner_calls = calls.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let calls = runner_calls.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(autumn_web::AutumnError::internal_server_error_msg(
                        "transient failure on first attempt",
                    ))
                } else {
                    Ok(())
                }
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    // Small backoff so the retry lands quickly.
    insert_raw_row(&pool, "retry-1", handler_key, 5, 10).await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    wait_until("hook completed after retry", {
        let pool = pool.clone();
        move || {
            let pool = pool.clone();
            async move {
                matches!(hook_row_status(&pool, "retry-1").await, Some((s, _)) if s == "completed")
            }
        }
    })
    .await;
    shutdown.cancel();

    assert_eq!(
        load(&calls),
        2,
        "the hook must be attempted twice: one transient failure, then a successful retry"
    );
}

// ── (c) dead-letter after max attempts ────────────────────────────────────────

#[tokio::test]
async fn permanently_failing_hook_is_dead_lettered_on_sqlite() {
    let pool = boot("hook_dead_letter").await;
    let handler_key = unique_handler_key();

    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner_calls = calls.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let calls = runner_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(autumn_web::AutumnError::internal_server_error_msg(
                    "permanent failure",
                ))
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    // max_attempts = 2, tiny backoff → dead-letter fast.
    insert_raw_row(&pool, "dead-1", handler_key, 2, 5).await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());

    wait_until("hook dead-lettered", {
        let pool = pool.clone();
        move || {
            let pool = pool.clone();
            async move {
                matches!(hook_row_status(&pool, "dead-1").await, Some((s, _)) if s == "failed")
            }
        }
    })
    .await;
    shutdown.cancel();

    let (status, last_error) = hook_row_status(&pool, "dead-1").await.expect("row exists");
    assert_eq!(status, "failed", "exhausted hooks must be dead-lettered");
    assert!(
        last_error.is_some_and(|e| e.contains("permanent failure")),
        "the dead-lettered row must retain the last failure detail"
    );
    assert_eq!(
        load(&calls),
        2,
        "the hook must be attempted exactly max_attempts (2) times before dead-lettering"
    );
}

// ── (d) graceful shutdown settles in-flight work ──────────────────────────────

#[tokio::test]
async fn graceful_shutdown_settles_in_flight_hook_on_sqlite() {
    let pool = boot("hook_drain").await;
    let handler_key = unique_handler_key();

    let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner_done = done.clone();
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let done = runner_done.clone();
            async move {
                // A slow hook so the shutdown signal arrives while it is in-flight.
                tokio::time::sleep(Duration::from_millis(300)).await;
                done.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );

    enqueue(&pool, handler_key, Some("drain-1"), &json!({ "id": 1 })).await;

    let shutdown = CancellationToken::new();
    autumn_web::__private::start_repository_commit_hook_worker(pool.clone(), shutdown.clone());
    // Wake the worker immediately (exercises the SQLite Notify kick), then cancel
    // while the hook is still sleeping mid-flight.
    kick(&pool);
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.cancel();

    // Despite the shutdown arriving mid-flight, the claimed hook must settle.
    wait_until("in-flight hook settles after shutdown", {
        let done = done.clone();
        move || {
            let done = done.clone();
            async move { load(&done) >= 1 }
        }
    })
    .await;
    assert_eq!(
        load(&done),
        1,
        "the in-flight leased hook must finish exactly once before the worker exits"
    );

    // And it must have been acknowledged (not left leased/abandoned): the single
    // queue row reaches the terminal `completed` state.
    let row_id = single_row_id(&pool).await.expect("one queue row");
    wait_until("in-flight hook acknowledged", {
        let pool = pool.clone();
        let row_id = row_id.clone();
        move || {
            let pool = pool.clone();
            let row_id = row_id.clone();
            async move {
                matches!(hook_row_status(&pool, &row_id).await, Some((s, _)) if s == "completed")
            }
        }
    })
    .await;
}

fn kick(pool: &SqlitePool) {
    autumn_web::__private::kick_repository_commit_hook_dispatcher(pool);
}

/// The drain test enqueues through the real surface (an idempotent, derived row
/// id); recover it so the settle assertion can read the row's terminal status.
async fn single_row_id(pool: &SqlitePool) -> Option<String> {
    #[derive(diesel::QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        id: String,
    }
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT id FROM autumn_repository_commit_hooks LIMIT 1")
        .get_result::<IdRow>(&mut conn)
        .await
        .ok()
        .map(|row| row.id)
}
