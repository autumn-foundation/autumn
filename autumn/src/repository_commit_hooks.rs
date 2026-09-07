//! Durable dispatch for generated repository after-commit hooks.
//!
//! Generated repositories enqueue `after_*_commit` work into Postgres inside
//! the same transaction as the mutation. Any replica can later claim and run a
//! queued hook using the generated runner registered in this process.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
// `AtomicBool`/`Ordering` back the Postgres kick-worker's pending flag, which is
// `cfg`-gated off under `sqlite` (the durable hook worker is Postgres-only).
#[cfg(not(feature = "sqlite"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use diesel::OptionalExtension as _;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use futures::FutureExt as _;
// `scope_boxed` boxes the `SQLite` claim's transaction closure future for
// `scoped_immediate_transaction`.
#[cfg(feature = "sqlite")]
use scoped_futures::ScopedFutureExt as _;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest as _;
// `Notify` backs the Postgres kick-worker's coalesced pending flag, and (under
// `sqlite`) the single-node poll-loop wake used in place of Postgres LISTEN/NOTIFY.
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{AutumnError, AutumnResult};

// The Postgres worker is typed on `PgPool`; the shared enqueue/finalize surface
// and the `SQLite` worker are typed on the runtime pool alias `RtPool` (on
// Postgres the two are the SAME type, since `RuntimeConnection = AsyncPgConnection`).
#[cfg(not(feature = "sqlite"))]
type PgPool = Pool<diesel_async::AsyncPgConnection>;
type RtPool = Pool<crate::db::RuntimeConnection>;
type HookFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;
type HookRunner = Arc<dyn Fn(Value, Value) -> HookFuture + Send + Sync + 'static>;

/// Reserved key under which the `SQLite` durable worker embeds the originating
/// tenant into the persisted `context` JSON, so a claimed hook re-establishes
/// the SAME tenant scope it was enqueued under before executing. `MutationContext`
/// does not carry a tenant field, so tenant isolation for durable hooks would
/// otherwise be lost across the process boundary. The key is stripped before the
/// context is handed to the generated runner (which deserializes `MutationContext`).
#[cfg(feature = "sqlite")]
const TENANT_CONTEXT_KEY: &str = "__autumn_tenant";

/// Framework migration set for the durable commit-hook queue table.
///
/// Backend-forked (#1996 item 5): the Postgres DDL (`JSONB`/`TIMESTAMPTZ`/
/// `DEFAULT NOW()`) is not valid `SQLite`, so the `SQLite` build embeds a parallel
/// set (`TEXT` JSON / `TEXT` timestamps / `DEFAULT CURRENT_TIMESTAMP`) under the
/// same `20260515000000_create_repository_commit_hook_queue` version dir name,
/// keeping `__diesel_schema_migrations` bookkeeping identical across backends.
#[cfg(not(feature = "sqlite"))]
pub const REPOSITORY_COMMIT_HOOK_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("repository_commit_hook_migrations");

/// `SQLite` variant of [`REPOSITORY_COMMIT_HOOK_MIGRATIONS`]. See that item for
/// the backend-fork rationale.
#[cfg(feature = "sqlite")]
pub const REPOSITORY_COMMIT_HOOK_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("repository_commit_hook_migrations_sqlite");

const HOOK_WORKER_IDLE_SLEEP: Duration = Duration::from_millis(250);
const HOOK_STALE_CLAIM_AFTER: Duration = Duration::from_secs(60);
const HOOK_CLAIM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HOOK_PENDING_FINALIZER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HOOK_AFTER_HOOK_FAILURE_MARK_RETRY_SLEEP: Duration = Duration::from_millis(100);
const HOOK_AFTER_HOOK_FAILURE_MARK_MAX_ATTEMPTS: usize = 3;

// ── Backend-forked SQL (#1996 item 5) ────────────────────────────────────────
//
// Only one backend's arm is ever compiled. The Postgres SQL is byte-identical to
// the original (numbered `$n` placeholders, `NOW()`, `::JSONB` casts). The `SQLite`
// SQL uses `?` positional placeholders (bound in the SAME order as the Postgres
// binds so a single shared function body serves both), `CURRENT_TIMESTAMP` in
// place of `NOW()`, no `::JSONB` casts (the columns are already `TEXT`), and
// lowercase `excluded` in the upsert. Structurally-divergent statements (claim,
// nack retry/dead-letter, stale recovery, and bulk enqueue) are forked at the
// function level instead — see `*_claim_next` / `*_nack_*` / `*_recover_stale*`.

#[cfg(not(feature = "sqlite"))]
const HOOK_SELECT_COLS: &str = "id, handler_key, hook_name, context::TEXT AS context, \
    record::TEXT AS record, status, attempt, max_attempts, initial_backoff_ms";
#[cfg(feature = "sqlite")]
const HOOK_SELECT_COLS: &str = "id, handler_key, hook_name, context, \
    record, status, attempt, max_attempts, initial_backoff_ms";

#[cfg(not(feature = "sqlite"))]
const HOOK_ACK_SUCCESS_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = 'completed', finished_at = NOW(), \
         context = '{}'::JSONB, record = '{}'::JSONB, \
         claimed_by = NULL, claimed_at = NULL, last_error = NULL \
     WHERE id = $1 AND claimed_by = $2 AND status = 'running'";
#[cfg(feature = "sqlite")]
const HOOK_ACK_SUCCESS_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = 'completed', finished_at = CURRENT_TIMESTAMP, \
         context = '{}', record = '{}', \
         claimed_by = NULL, claimed_at = NULL, last_error = NULL \
     WHERE id = ? AND claimed_by = ? AND status = 'running'";

#[cfg(not(feature = "sqlite"))]
const HOOK_EXTEND_CLAIM_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET claimed_at = NOW() \
     WHERE id = $1 AND claimed_by = $2 AND status = 'running'";
#[cfg(feature = "sqlite")]
const HOOK_EXTEND_CLAIM_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET claimed_at = CURRENT_TIMESTAMP \
     WHERE id = ? AND claimed_by = ? AND status = 'running'";

#[cfg(not(feature = "sqlite"))]
const HOOK_ENQUEUE_INSERT_SQL: &str = "INSERT INTO autumn_repository_commit_hooks \
     (id, handler_key, hook_name, context, record, status, attempt, \
      max_attempts, initial_backoff_ms, enqueued_at, run_at) \
     VALUES ($1, $2, $3, $4::JSONB, $5::JSONB, 'enqueued', 1, 5, 1000, NOW(), NOW()) \
     ON CONFLICT (id) DO NOTHING";
#[cfg(feature = "sqlite")]
const HOOK_ENQUEUE_INSERT_SQL: &str = "INSERT INTO autumn_repository_commit_hooks \
     (id, handler_key, hook_name, context, record, status, attempt, \
      max_attempts, initial_backoff_ms, enqueued_at, run_at) \
     VALUES (?, ?, ?, ?, ?, 'enqueued', 1, 5, 1000, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
     ON CONFLICT (id) DO NOTHING";

#[cfg(not(feature = "sqlite"))]
const HOOK_PENDING_INSERT_SQL: &str = "INSERT INTO autumn_repository_commit_hooks \
     (id, handler_key, hook_name, context, record, status, attempt, \
       max_attempts, initial_backoff_ms, enqueued_at, run_at, claimed_by, claimed_at) \
     VALUES ($1, $2, $3, $4::JSONB, $5::JSONB, 'pending_after_hook', 1, 5, 1000, NOW(), NOW(), $6, NOW()) \
     ON CONFLICT (id) DO UPDATE \
      SET handler_key = EXCLUDED.handler_key, hook_name = EXCLUDED.hook_name, \
          context = EXCLUDED.context, record = EXCLUDED.record, \
          status = 'pending_after_hook', attempt = 1, \
          max_attempts = EXCLUDED.max_attempts, \
          initial_backoff_ms = EXCLUDED.initial_backoff_ms, \
          enqueued_at = EXCLUDED.enqueued_at, run_at = EXCLUDED.run_at, \
          claimed_by = EXCLUDED.claimed_by, claimed_at = EXCLUDED.claimed_at, \
          started_at = NULL, finished_at = NULL, last_error = NULL \
      WHERE autumn_repository_commit_hooks.status IN ('pending_after_hook', 'after_hook_failed')";
#[cfg(feature = "sqlite")]
const HOOK_PENDING_INSERT_SQL: &str = "INSERT INTO autumn_repository_commit_hooks \
     (id, handler_key, hook_name, context, record, status, attempt, \
       max_attempts, initial_backoff_ms, enqueued_at, run_at, claimed_by, claimed_at) \
     VALUES (?, ?, ?, ?, ?, 'pending_after_hook', 1, 5, 1000, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, ?, CURRENT_TIMESTAMP) \
     ON CONFLICT (id) DO UPDATE \
      SET handler_key = excluded.handler_key, hook_name = excluded.hook_name, \
          context = excluded.context, record = excluded.record, \
          status = 'pending_after_hook', attempt = 1, \
          max_attempts = excluded.max_attempts, \
          initial_backoff_ms = excluded.initial_backoff_ms, \
          enqueued_at = excluded.enqueued_at, run_at = excluded.run_at, \
          claimed_by = excluded.claimed_by, claimed_at = excluded.claimed_at, \
          started_at = NULL, finished_at = NULL, last_error = NULL \
      WHERE autumn_repository_commit_hooks.status IN ('pending_after_hook', 'after_hook_failed')";

#[cfg(not(feature = "sqlite"))]
const HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET context = $1::JSONB, record = $2::JSONB, status = 'after_hook_succeeded', \
          claimed_at = NOW(), last_error = NULL \
     WHERE id = $3 AND claimed_by = $4 AND status = 'pending_after_hook'";
#[cfg(feature = "sqlite")]
const HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET context = ?, record = ?, status = 'after_hook_succeeded', \
          claimed_at = CURRENT_TIMESTAMP, last_error = NULL \
     WHERE id = ? AND claimed_by = ? AND status = 'pending_after_hook'";

#[cfg(not(feature = "sqlite"))]
const HOOK_FINALIZE_AFTER_HOOK_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = 'enqueued', run_at = NOW(), \
          enqueued_at = COALESCE(enqueued_at, NOW()), \
          claimed_by = NULL, claimed_at = NULL, last_error = NULL \
      WHERE id = $1 AND claimed_by = $2 AND status = 'after_hook_succeeded'";
#[cfg(feature = "sqlite")]
const HOOK_FINALIZE_AFTER_HOOK_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = 'enqueued', run_at = CURRENT_TIMESTAMP, \
          enqueued_at = COALESCE(enqueued_at, CURRENT_TIMESTAMP), \
          claimed_by = NULL, claimed_at = NULL, last_error = NULL \
      WHERE id = ? AND claimed_by = ? AND status = 'after_hook_succeeded'";

#[cfg(not(feature = "sqlite"))]
const HOOK_DISCARD_PENDING_SQL: &str = "DELETE FROM autumn_repository_commit_hooks \
     WHERE id = $1 AND claimed_by = $2 AND status = 'pending_after_hook'";
#[cfg(feature = "sqlite")]
const HOOK_DISCARD_PENDING_SQL: &str = "DELETE FROM autumn_repository_commit_hooks \
     WHERE id = ? AND claimed_by = ? AND status = 'pending_after_hook'";

#[cfg(not(feature = "sqlite"))]
const HOOK_AFTER_HOOK_FAILED_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = 'after_hook_failed', \
         finished_at = NOW(), \
         context = '{}'::JSONB, record = '{}'::JSONB, \
         claimed_by = NULL, claimed_at = NULL, last_error = $1 \
      WHERE id = $2 AND claimed_by = $3 AND status = 'pending_after_hook'";
#[cfg(feature = "sqlite")]
const HOOK_AFTER_HOOK_FAILED_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = 'after_hook_failed', \
         finished_at = CURRENT_TIMESTAMP, \
         context = '{}', record = '{}', \
         claimed_by = NULL, claimed_at = NULL, last_error = ? \
      WHERE id = ? AND claimed_by = ? AND status = 'pending_after_hook'";

#[cfg(not(feature = "sqlite"))]
const HOOK_EXTEND_PENDING_FINALIZER_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET claimed_at = NOW() \
     WHERE id = $1 AND claimed_by = $2 AND status = 'pending_after_hook'";
#[cfg(feature = "sqlite")]
const HOOK_EXTEND_PENDING_FINALIZER_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET claimed_at = CURRENT_TIMESTAMP \
     WHERE id = ? AND claimed_by = ? AND status = 'pending_after_hook'";

#[cfg(not(feature = "sqlite"))]
const HOOK_RECOVER_STALE_RUNNING_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = CASE \
           WHEN attempt < max_attempts THEN 'enqueued' \
           ELSE 'failed' \
         END, \
         attempt = CASE \
           WHEN attempt < max_attempts THEN attempt + 1 \
           ELSE attempt \
         END, \
         run_at = CASE \
           WHEN attempt < max_attempts THEN NOW() \
           ELSE run_at \
         END, \
         started_at = NULL, \
         finished_at = CASE \
           WHEN attempt >= max_attempts THEN NOW() \
           ELSE NULL \
         END, \
         claimed_by = NULL, \
         claimed_at = NULL, \
         last_error = $1 \
     WHERE status = 'running' \
       AND claimed_at < NOW() - ($2::BIGINT * INTERVAL '1 millisecond')";
#[cfg(not(feature = "sqlite"))]
const HOOK_RECOVER_STALE_PENDING_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = CASE \
            WHEN status = 'after_hook_succeeded' THEN 'enqueued' \
            ELSE 'after_hook_failed' \
          END, \
          run_at = CASE \
            WHEN status = 'after_hook_succeeded' THEN NOW() \
            ELSE run_at \
          END, \
          enqueued_at = CASE \
            WHEN status = 'after_hook_succeeded' THEN COALESCE(enqueued_at, NOW()) \
            ELSE enqueued_at \
          END, \
          context = CASE \
            WHEN status = 'pending_after_hook' THEN '{}'::JSONB \
            ELSE context \
          END, \
          record = CASE \
            WHEN status = 'pending_after_hook' THEN '{}'::JSONB \
            ELSE record \
          END, \
          finished_at = CASE \
            WHEN status = 'pending_after_hook' THEN NOW() \
            ELSE finished_at \
          END, \
          started_at = NULL, \
          claimed_by = NULL, \
          claimed_at = NULL, \
          last_error = COALESCE(last_error, $1) \
      WHERE status IN ('pending_after_hook', 'after_hook_succeeded') \
        AND claimed_at < NOW() - ($2::BIGINT * INTERVAL '1 millisecond')";

// `SQLite` stale recovery: identical branch semantics, but the `NOW() - INTERVAL`
// arithmetic is replaced by a Rust-computed cutoff bound (bound SECOND, matching
// its textual position after the `last_error` message) — `WHERE claimed_at < ?`
// compares against a UTC `YYYY-MM-DD HH:MM:SS.fff` string. Binds: (last_error, cutoff).
#[cfg(feature = "sqlite")]
const HOOK_RECOVER_STALE_RUNNING_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = CASE \
           WHEN attempt < max_attempts THEN 'enqueued' \
           ELSE 'failed' \
         END, \
         attempt = CASE \
           WHEN attempt < max_attempts THEN attempt + 1 \
           ELSE attempt \
         END, \
         run_at = CASE \
           WHEN attempt < max_attempts THEN CURRENT_TIMESTAMP \
           ELSE run_at \
         END, \
         started_at = NULL, \
         finished_at = CASE \
           WHEN attempt >= max_attempts THEN CURRENT_TIMESTAMP \
           ELSE NULL \
         END, \
         claimed_by = NULL, \
         claimed_at = NULL, \
         last_error = ? \
     WHERE status = 'running' \
       AND claimed_at < ?";
#[cfg(feature = "sqlite")]
const HOOK_RECOVER_STALE_PENDING_SQL: &str = "UPDATE autumn_repository_commit_hooks \
     SET status = CASE \
            WHEN status = 'after_hook_succeeded' THEN 'enqueued' \
            ELSE 'after_hook_failed' \
          END, \
          run_at = CASE \
            WHEN status = 'after_hook_succeeded' THEN CURRENT_TIMESTAMP \
            ELSE run_at \
          END, \
          enqueued_at = CASE \
            WHEN status = 'after_hook_succeeded' THEN COALESCE(enqueued_at, CURRENT_TIMESTAMP) \
            ELSE enqueued_at \
          END, \
          context = CASE \
            WHEN status = 'pending_after_hook' THEN '{}' \
            ELSE context \
          END, \
          record = CASE \
            WHEN status = 'pending_after_hook' THEN '{}' \
            ELSE record \
          END, \
          finished_at = CASE \
            WHEN status = 'pending_after_hook' THEN CURRENT_TIMESTAMP \
            ELSE finished_at \
          END, \
          started_at = NULL, \
          claimed_by = NULL, \
          claimed_at = NULL, \
          last_error = COALESCE(last_error, ?) \
      WHERE status IN ('pending_after_hook', 'after_hook_succeeded') \
        AND claimed_at < ?";

static REPOSITORY_COMMIT_HOOK_RUNNERS: OnceLock<
    RwLock<HashMap<String, RepositoryCommitHookRegistration>>,
> = OnceLock::new();
#[cfg(not(feature = "sqlite"))]
static REPOSITORY_COMMIT_HOOK_KICKERS: OnceLock<
    RwLock<HashMap<usize, Arc<RepositoryCommitHookKickState>>>,
> = OnceLock::new();

#[cfg(not(feature = "sqlite"))]
struct RepositoryCommitHookKickState {
    notify: Notify,
    pending: AtomicBool,
    #[cfg(feature = "ws")]
    channels: OnceLock<crate::channels::Channels>,
}

#[cfg(not(feature = "sqlite"))]
impl Default for RepositoryCommitHookKickState {
    fn default() -> Self {
        Self {
            notify: Notify::new(),
            pending: AtomicBool::new(false),
            #[cfg(feature = "ws")]
            channels: OnceLock::new(),
        }
    }
}

#[cfg(not(feature = "sqlite"))]
impl RepositoryCommitHookKickState {
    fn request_kick(&self) -> bool {
        !self.pending.swap(true, Ordering::AcqRel)
    }

    fn take_pending_kick(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

// `SQLite` is single-node and single-writer, so it needs no LISTEN/NOTIFY: an
// in-process `Notify` per pool wakes that pool's poll loop the moment a mutation
// commits. A missed kick is harmless — the loop also wakes on its idle sleep and
// drains, so durability never depends on the kick landing.
#[cfg(feature = "sqlite")]
static SQLITE_HOOK_KICKERS: OnceLock<RwLock<HashMap<usize, Arc<Notify>>>> = OnceLock::new();

#[cfg(feature = "sqlite")]
fn sqlite_repository_commit_hook_kick(pool: &RtPool) -> Arc<Notify> {
    let key = std::ptr::from_ref(pool.manager()).addr();
    let registry = SQLITE_HOOK_KICKERS.get_or_init(|| RwLock::new(HashMap::new()));
    {
        let registry = registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(notify) = registry.get(&key) {
            return notify.clone();
        }
    }
    let mut registry = registry
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry
        .entry(key)
        .or_insert_with(|| Arc::new(Notify::new()))
        .clone()
}

#[doc(hidden)]
#[must_use]
pub struct RepositoryCommitHookPendingHeartbeat {
    shutdown: CancellationToken,
}

impl RepositoryCommitHookPendingHeartbeat {
    const fn new(shutdown: CancellationToken) -> Self {
        Self { shutdown }
    }

    pub fn cancel(&self) {
        self.shutdown.cancel();
    }
}

impl Drop for RepositoryCommitHookPendingHeartbeat {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Link-time descriptor emitted by generated repositories with commit hooks.
///
/// The worker replays these descriptors at startup so queued hook rows can be
/// claimed after a process restart without waiting for request traffic to touch
/// the repository type first.
#[doc(hidden)]
pub struct RepositoryCommitHookDescriptor {
    /// Registers the generated runner for one repository type.
    pub register: fn(),
}

inventory::collect!(RepositoryCommitHookDescriptor);

#[derive(Clone)]
struct RepositoryCommitHookRegistration {
    create: HookRunner,
    update: HookRunner,
    delete: HookRunner,
}

impl RepositoryCommitHookRegistration {
    fn runner(&self, hook_name: &str) -> Option<HookRunner> {
        match hook_name {
            "create" => Some(self.create.clone()),
            "update" => Some(self.update.clone()),
            "delete" => Some(self.delete.clone()),
            _ => None,
        }
    }
}

#[derive(diesel::QueryableByName, Debug, Clone)]
#[allow(dead_code)]
struct RepositoryCommitHookRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    handler_key: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    hook_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    context: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    record: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    attempt: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    max_attempts: i32,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    initial_backoff_ms: i64,
}

/// Register generated runners for one hooked repository type.
///
/// This is called by proc-macro-generated code and is intentionally hidden
/// behind `autumn_web::__private`.
pub fn register_repository_commit_hook_runner<
    Create,
    CreateFut,
    Update,
    UpdateFut,
    Delete,
    DeleteFut,
>(
    handler_key: &'static str,
    create: Create,
    update: Update,
    delete: Delete,
) where
    Create: Fn(Value, Value) -> CreateFut + Send + Sync + 'static,
    CreateFut: Future<Output = AutumnResult<()>> + Send + 'static,
    Update: Fn(Value, Value) -> UpdateFut + Send + Sync + 'static,
    UpdateFut: Future<Output = AutumnResult<()>> + Send + 'static,
    Delete: Fn(Value, Value) -> DeleteFut + Send + Sync + 'static,
    DeleteFut: Future<Output = AutumnResult<()>> + Send + 'static,
{
    let registration = RepositoryCommitHookRegistration {
        create: Arc::new(move |ctx, record| Box::pin(create(ctx, record))),
        update: Arc::new(move |ctx, record| Box::pin(update(ctx, record))),
        delete: Arc::new(move |ctx, record| Box::pin(delete(ctx, record))),
    };

    REPOSITORY_COMMIT_HOOK_RUNNERS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(handler_key.to_owned(), registration);
}

fn register_inventory_repository_commit_hook_runners() {
    for descriptor in inventory::iter::<RepositoryCommitHookDescriptor> {
        (descriptor.register)();
    }
}

pub fn has_repository_commit_hook_descriptors() -> bool {
    inventory::iter::<RepositoryCommitHookDescriptor>
        .into_iter()
        .next()
        .is_some()
}

/// Insert a generated repository commit hook row using the caller's open
/// connection. The row participates in the caller's transaction.
///
/// # Errors
///
/// Returns an error when the context or record cannot be serialized, or when
/// Postgres rejects the enqueue insert.
pub async fn enqueue_repository_commit_hook_on_conn<C, R>(
    conn: &mut crate::db::RuntimeConnection,
    handler_key: &str,
    hook_name: &str,
    idempotency_key: Option<&str>,
    idempotency_discriminator: Option<&str>,
    context: &C,
    record: &R,
) -> AutumnResult<()>
where
    C: Serialize + Sync + ?Sized,
    R: Serialize + Sync + ?Sized,
{
    let (context, record) = serialize_repository_commit_hook_payloads(context, record)?;
    let id = repository_commit_hook_id(
        idempotency_key,
        idempotency_discriminator,
        handler_key,
        hook_name,
        &record,
    );

    diesel::sql_query(HOOK_ENQUEUE_INSERT_SQL)
        .bind::<diesel::sql_types::Text, _>(id)
        .bind::<diesel::sql_types::Text, _>(handler_key)
        .bind::<diesel::sql_types::Text, _>(hook_name)
        .bind::<diesel::sql_types::Text, _>(context)
        .bind::<diesel::sql_types::Text, _>(record)
        .execute(conn)
        .await
        .map(|_| ())
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook enqueue failed: {error}"
            ))
        })
}

/// Insert a generated repository commit hook row in a staged state.
///
/// The row participates in the caller's transaction but cannot be claimed by a
/// dispatcher until [`finalize_repository_commit_hook_after_hook`] promotes it
/// after the regular `after_*` hook has succeeded.
///
/// # Errors
///
/// Returns an error when the context or record cannot be serialized, or when
/// Postgres rejects the staged insert.
pub async fn enqueue_repository_commit_hook_pending_on_conn<C, R>(
    conn: &mut crate::db::RuntimeConnection,
    handler_key: &str,
    hook_name: &str,
    idempotency_key: Option<&str>,
    idempotency_discriminator: Option<&str>,
    context: &C,
    record: &R,
) -> AutumnResult<(String, String)>
where
    C: Serialize + Sync + ?Sized,
    R: Serialize + Sync + ?Sized,
{
    let (context, record) = serialize_repository_commit_hook_payloads(context, record)?;
    let id = repository_commit_hook_id(
        idempotency_key,
        idempotency_discriminator,
        handler_key,
        hook_name,
        &record,
    );
    let owner = repository_commit_hook_pending_owner_id();

    diesel::sql_query(HOOK_PENDING_INSERT_SQL)
        .bind::<diesel::sql_types::Text, _>(id.clone())
        .bind::<diesel::sql_types::Text, _>(handler_key)
        .bind::<diesel::sql_types::Text, _>(hook_name)
        .bind::<diesel::sql_types::Text, _>(context)
        .bind::<diesel::sql_types::Text, _>(record)
        .bind::<diesel::sql_types::Text, _>(owner.clone())
        .execute(conn)
        .await
        .map(|_| (id, owner))
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook staging failed: {error}"
            ))
        })
}

/// Insert multiple generated repository commit hook rows in a staged state in a single query.
///
/// # Errors
///
/// Returns an error when any context or record cannot be serialized, or when
/// Postgres rejects the staged insert.
pub async fn enqueue_repository_commit_hooks_pending_bulk_on_conn<C, R>(
    conn: &mut crate::db::RuntimeConnection,
    handler_key: &str,
    hook_name: &str,
    inputs: &[(Option<String>, Option<String>, &C, &R)],
) -> AutumnResult<Vec<(String, String)>>
where
    C: Serialize + Sync + ?Sized,
    R: Serialize + Sync + ?Sized,
{
    // Postgres stages every row in one `UNNEST`-driven statement. `SQLite` has no
    // array binds, so its arm loops the single-row staged insert on the SAME
    // caller connection (already inside the mutation transaction, so the rows
    // still commit atomically with the domain write). Both dedupe/restage
    // identically via `ON CONFLICT (id) DO UPDATE`.
    #[cfg(not(feature = "sqlite"))]
    const SQL: &str = "INSERT INTO autumn_repository_commit_hooks \
         (id, handler_key, hook_name, context, record, status, attempt, \
          max_attempts, initial_backoff_ms, enqueued_at, run_at, claimed_by, claimed_at) \
         SELECT \
             t.id, t.handler_key, t.hook_name, t.context::JSONB, t.record::JSONB, \
             'pending_after_hook', 1, 5, 1000, NOW(), NOW(), t.claimed_by, NOW() \
         FROM UNNEST($1::TEXT[], $2::TEXT[], $3::TEXT[], $4::TEXT[], $5::TEXT[], $6::TEXT[]) \
           AS t(id, handler_key, hook_name, context, record, claimed_by) \
         ON CONFLICT (id) DO UPDATE \
          SET handler_key = EXCLUDED.handler_key, hook_name = EXCLUDED.hook_name, \
              context = EXCLUDED.context, record = EXCLUDED.record, \
              status = 'pending_after_hook', attempt = 1, \
              max_attempts = EXCLUDED.max_attempts, \
              initial_backoff_ms = EXCLUDED.initial_backoff_ms, \
              enqueued_at = EXCLUDED.enqueued_at, run_at = EXCLUDED.run_at, \
              claimed_by = EXCLUDED.claimed_by, claimed_at = EXCLUDED.claimed_at, \
              started_at = NULL, finished_at = NULL, last_error = NULL \
          WHERE autumn_repository_commit_hooks.status IN ('pending_after_hook', 'after_hook_failed')";

    if inputs.is_empty() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::with_capacity(inputs.len());
    let mut handler_keys = Vec::with_capacity(inputs.len());
    let mut hook_names = Vec::with_capacity(inputs.len());
    let mut contexts = Vec::with_capacity(inputs.len());
    let mut records = Vec::with_capacity(inputs.len());
    let mut owners = Vec::with_capacity(inputs.len());
    let mut results = Vec::with_capacity(inputs.len());

    let owner = repository_commit_hook_pending_owner_id();

    for &(ref idempotency_key, ref idempotency_discriminator, context, record) in inputs {
        let (context_str, record_str) = serialize_repository_commit_hook_payloads(context, record)?;
        let id = repository_commit_hook_id(
            idempotency_key.as_deref(),
            idempotency_discriminator.as_deref(),
            handler_key,
            hook_name,
            &record_str,
        );

        ids.push(id.clone());
        handler_keys.push(handler_key.to_string());
        hook_names.push(hook_name.to_string());
        contexts.push(context_str);
        records.push(record_str);
        owners.push(owner.clone());
        results.push((id, owner.clone()));
    }

    crate::backend_select! {
        pg => {{
            diesel::sql_query(SQL)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(ids)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(handler_keys)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(hook_names)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(contexts)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(records)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(owners)
                .execute(conn)
                .await
                .map(|_| results)
                .map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "repository commit hook bulk staging failed: {error}"
                    ))
                })
        }},
        sqlite => {{
            for index in 0..ids.len() {
                diesel::sql_query(HOOK_PENDING_INSERT_SQL)
                    .bind::<diesel::sql_types::Text, _>(ids[index].clone())
                    .bind::<diesel::sql_types::Text, _>(handler_keys[index].clone())
                    .bind::<diesel::sql_types::Text, _>(hook_names[index].clone())
                    .bind::<diesel::sql_types::Text, _>(contexts[index].clone())
                    .bind::<diesel::sql_types::Text, _>(records[index].clone())
                    .bind::<diesel::sql_types::Text, _>(owners[index].clone())
                    .execute(&mut *conn)
                    .await
                    .map_err(|error| {
                        AutumnError::internal_server_error_msg(format!(
                            "repository commit hook bulk staging failed: {error}"
                        ))
                    })?;
            }
            Ok(results)
        }},
    }
}

/// Insert multiple generated repository commit hook rows directly in an enqueued state in a single query.
///
/// # Errors
///
/// Returns an error when any context or record cannot be serialized, or when
/// Postgres rejects the staged insert.
pub async fn enqueue_repository_commit_hooks_bulk_on_conn<C, R>(
    conn: &mut crate::db::RuntimeConnection,
    handler_key: &str,
    hook_name: &str,
    inputs: &[(Option<String>, Option<String>, &C, &R)],
) -> AutumnResult<()>
where
    C: Serialize + Sync + ?Sized,
    R: Serialize + Sync + ?Sized,
{
    // See `enqueue_repository_commit_hooks_pending_bulk_on_conn` for the pg/sqlite
    // fork rationale (UNNEST batch vs. per-row loop on the same txn connection).
    #[cfg(not(feature = "sqlite"))]
    const SQL: &str = "INSERT INTO autumn_repository_commit_hooks \
         (id, handler_key, hook_name, context, record, status, attempt, \
          max_attempts, initial_backoff_ms, enqueued_at, run_at) \
         SELECT \
             t.id, t.handler_key, t.hook_name, t.context::JSONB, t.record::JSONB, \
             'enqueued', 1, 5, 1000, NOW(), NOW() \
         FROM UNNEST($1::TEXT[], $2::TEXT[], $3::TEXT[], $4::TEXT[], $5::TEXT[]) \
           AS t(id, handler_key, hook_name, context, record) \
         ON CONFLICT (id) DO NOTHING";

    if inputs.is_empty() {
        return Ok(());
    }

    let mut ids = Vec::with_capacity(inputs.len());
    let mut handler_keys = Vec::with_capacity(inputs.len());
    let mut hook_names = Vec::with_capacity(inputs.len());
    let mut contexts = Vec::with_capacity(inputs.len());
    let mut records = Vec::with_capacity(inputs.len());

    for &(ref idempotency_key, ref idempotency_discriminator, context, record) in inputs {
        let (context_str, record_str) = serialize_repository_commit_hook_payloads(context, record)?;
        let id = repository_commit_hook_id(
            idempotency_key.as_deref(),
            idempotency_discriminator.as_deref(),
            handler_key,
            hook_name,
            &record_str,
        );

        ids.push(id);
        handler_keys.push(handler_key.to_string());
        hook_names.push(hook_name.to_string());
        contexts.push(context_str);
        records.push(record_str);
    }

    crate::backend_select! {
        pg => {{
            diesel::sql_query(SQL)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(ids)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(handler_keys)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(hook_names)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(contexts)
                .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(records)
                .execute(conn)
                .await
                .map(|_| ())
                .map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "repository commit hook bulk enqueue failed: {error}"
                    ))
                })
        }},
        sqlite => {{
            for index in 0..ids.len() {
                diesel::sql_query(HOOK_ENQUEUE_INSERT_SQL)
                    .bind::<diesel::sql_types::Text, _>(ids[index].clone())
                    .bind::<diesel::sql_types::Text, _>(handler_keys[index].clone())
                    .bind::<diesel::sql_types::Text, _>(hook_names[index].clone())
                    .bind::<diesel::sql_types::Text, _>(contexts[index].clone())
                    .bind::<diesel::sql_types::Text, _>(records[index].clone())
                    .execute(&mut *conn)
                    .await
                    .map_err(|error| {
                        AutumnError::internal_server_error_msg(format!(
                            "repository commit hook bulk enqueue failed: {error}"
                        ))
                    })?;
            }
            Ok(())
        }},
    }
}

/// Promote a staged create/update commit hook after the regular after hook
/// succeeds, rewriting the row with the finalized mutation context.
///
/// # Errors
///
/// Returns an error when serialization fails, the database cannot be reached,
/// or the staged row is no longer present.
pub async fn finalize_repository_commit_hook_after_hook<C, R>(
    pool: &RtPool,
    hook_id: &str,
    owner: &str,
    context: &C,
    record: &R,
) -> AutumnResult<()>
where
    C: Serialize + Sync + ?Sized,
    R: Serialize + Sync + ?Sized,
{
    let (context, record) = serialize_repository_commit_hook_payloads(context, record)?;
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("pg pool error: {error}"))
    })?;

    let rows = diesel::sql_query(HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL)
        .bind::<diesel::sql_types::Text, _>(context)
        .bind::<diesel::sql_types::Text, _>(record)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(owner)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook after-hook success mark failed: {error}"
            ))
        })?;

    if rows == 0 {
        return missing_repository_commit_hook_finalization_result(hook_id);
    }

    let rows = diesel::sql_query(HOOK_FINALIZE_AFTER_HOOK_SQL)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(owner)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook finalization failed: {error}"
            ))
        })?;

    if rows == 0 {
        return Err(AutumnError::internal_server_error_msg(format!(
            "repository commit hook finalization skipped marked row: {hook_id}"
        )));
    }

    Ok(())
}

fn missing_repository_commit_hook_finalization_result(hook_id: &str) -> AutumnResult<()> {
    Err(AutumnError::internal_server_error_msg(format!(
        "repository commit hook finalization skipped missing staged row: {hook_id}"
    )))
}

/// Discard a staged create/update commit hook after the regular after hook
/// fails. This preserves the previous lifecycle: after-commit work is only
/// registered after the regular after hook succeeds.
///
/// # Errors
///
/// Returns an error when the database cannot be reached or rejects the delete.
pub async fn discard_repository_commit_hook_pending(
    pool: &RtPool,
    hook_id: &str,
    owner: &str,
) -> AutumnResult<()> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("pg pool error: {error}"))
    })?;

    diesel::sql_query(HOOK_DISCARD_PENDING_SQL)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(owner)
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook pending discard failed: {error}"
            ))
        })
}

/// Mark a staged create/update commit hook as permanently non-dispatchable
/// after the regular `after_*` hook failed or panicked.
///
/// This retries transient pool or database failures a bounded number of times
/// so callers can return the original hook error without hanging forever.
pub async fn mark_repository_commit_hook_after_hook_failed(
    pool: &RtPool,
    hook_id: &str,
    owner: &str,
    failure: impl Into<String>,
) {
    let failure = failure.into();
    for attempt in 1..=HOOK_AFTER_HOOK_FAILURE_MARK_MAX_ATTEMPTS {
        match mark_repository_commit_hook_after_hook_failed_once(pool, hook_id, owner, &failure)
            .await
        {
            Ok(true) => return,
            Ok(false) => {
                tracing::warn!(
                    hook_id = %hook_id,
                    "repository commit hook staged row was already unavailable while marking after-hook failure"
                );
                return;
            }
            Err(error) => {
                if attempt == HOOK_AFTER_HOOK_FAILURE_MARK_MAX_ATTEMPTS {
                    tracing::warn!(
                        hook_id = %hook_id,
                        error = %error,
                        attempts = HOOK_AFTER_HOOK_FAILURE_MARK_MAX_ATTEMPTS,
                        "failed to mark repository commit hook after-hook failure; giving up so the committed mutation can return"
                    );
                    return;
                }
                tracing::warn!(
                    hook_id = %hook_id,
                    error = %error,
                    attempt,
                    max_attempts = HOOK_AFTER_HOOK_FAILURE_MARK_MAX_ATTEMPTS,
                    "failed to mark repository commit hook after-hook failure; retrying"
                );
                tokio::time::sleep(HOOK_AFTER_HOOK_FAILURE_MARK_RETRY_SLEEP).await;
            }
        }
    }
}

async fn mark_repository_commit_hook_after_hook_failed_once(
    pool: &RtPool,
    hook_id: &str,
    owner: &str,
    failure: &str,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("pg pool error: {error}"))
    })?;

    diesel::sql_query(HOOK_AFTER_HOOK_FAILED_SQL)
        .bind::<diesel::sql_types::Text, _>(failure)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(owner)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook after-hook failure mark failed: {error}"
            ))
        })
}

/// Catch panics from a regular repository `after_*` hook while preserving its
/// `AutumnResult`.
///
/// # Errors
///
/// Returns `Err` when the hook future panics. A hook that completes normally
/// still returns its own `AutumnResult` inside `Ok`.
pub async fn catch_repository_after_hook_unwind<Fut>(
    future: Fut,
) -> Result<AutumnResult<()>, Box<dyn Any + Send>>
where
    Fut: Future<Output = AutumnResult<()>> + Send,
{
    std::panic::AssertUnwindSafe(future).catch_unwind().await
}

#[doc(hidden)]
pub fn start_repository_commit_hook_pending_finalizer_heartbeat(
    pool: RtPool,
    hook_id: String,
    owner: String,
) -> RepositoryCommitHookPendingHeartbeat {
    let shutdown = CancellationToken::new();
    let heartbeat_shutdown = shutdown.child_token();
    tokio::spawn(heartbeat_repository_commit_hook_pending_finalizer(
        pool,
        hook_id,
        owner,
        heartbeat_shutdown,
    ));
    RepositoryCommitHookPendingHeartbeat::new(shutdown)
}

fn serialize_repository_commit_hook_payloads<C, R>(
    context: &C,
    record: &R,
) -> AutumnResult<(String, String)>
where
    C: Serialize + ?Sized,
    R: Serialize + ?Sized,
{
    let context = serde_json::to_string(context).map_err(|error| {
        AutumnError::internal_server_error_msg(format!(
            "serialize repository commit hook context: {error}"
        ))
    })?;
    let record = serde_json::to_string(record).map_err(|error| {
        AutumnError::internal_server_error_msg(format!(
            "serialize repository commit hook record: {error}"
        ))
    })?;
    let context = embed_originating_tenant_in_context(context)?;

    Ok((context, record))
}

/// Postgres identity: the persisted `context` is byte-identical to the serialized
/// `MutationContext`, so the Postgres worker's behaviour is unchanged. The
/// fallible signature (and lint allowances) mirror the `SQLite` arm, which can
/// genuinely fail while re-serializing the tenant-embedded context.
#[cfg(not(feature = "sqlite"))]
#[inline]
#[allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)]
fn embed_originating_tenant_in_context(context: String) -> AutumnResult<String> {
    Ok(context)
}

/// `SQLite`: capture the ambient `CURRENT_TENANT` at enqueue/finalize time (both run
/// inside the request's tenant scope) and embed it into the persisted `context`
/// JSON under [`TENANT_CONTEXT_KEY`], so the durable worker can re-establish the
/// SAME tenant before executing the hook. `MutationContext` carries no tenant
/// field, so without this a `tenant_scoped` repository touched by a durable hook
/// would lose its scope across the process boundary. A hook enqueued with no
/// ambient tenant embeds nothing (the worker then runs it with the tenant UNSET,
/// so a `tenant_scoped` repo fails closed rather than touching a default scope).
#[cfg(feature = "sqlite")]
fn embed_originating_tenant_in_context(context: String) -> AutumnResult<String> {
    let tenant = crate::tenancy::CURRENT_TENANT
        .try_with(std::clone::Clone::clone)
        .ok()
        .flatten();
    // Fail closed: only embed a genuinely non-blank tenant. A blank or
    // whitespace-only ambient tenant is never persisted, so the worker runs the
    // hook with `CURRENT_TENANT` UNSET and a `tenant_scoped` repo fails closed
    // rather than resolving to `tenant_id = ''` (or whitespace).
    let tenant = tenant.filter(|value| !value.trim().is_empty());
    let Some(tenant) = tenant else {
        return Ok(context);
    };

    let mut value: Value = serde_json::from_str(&context).map_err(|error| {
        AutumnError::internal_server_error_msg(format!(
            "embed tenant into repository commit hook context: {error}"
        ))
    })?;
    if let Value::Object(map) = &mut value {
        map.insert(TENANT_CONTEXT_KEY.to_owned(), Value::String(tenant));
    }
    serde_json::to_string(&value).map_err(|error| {
        AutumnError::internal_server_error_msg(format!(
            "reserialize repository commit hook context with tenant: {error}"
        ))
    })
}

/// `SQLite`: pull the originating tenant back out of the persisted `context` JSON
/// (and strip the reserved key so the generated runner still deserializes a clean
/// `MutationContext`). Returns the parsed context [`Value`] and the tenant to
/// re-establish (`None` when the hook was enqueued outside any tenant scope).
#[cfg(feature = "sqlite")]
fn extract_originating_tenant_from_context(context: &str) -> (Value, Option<String>) {
    // A context that does not parse is handled downstream when the runner
    // re-parses it; surface it untouched with no tenant so the hook still runs
    // (and fails closed if it touches a tenant_scoped repo).
    serde_json::from_str::<Value>(context).map_or((Value::Null, None), |mut value| {
        let tenant = match &mut value {
            Value::Object(map) => map
                .remove(TENANT_CONTEXT_KEY)
                .and_then(|value| match value {
                    // Fail closed: a blank or whitespace-only persisted tenant is
                    // NOT an established tenant. Normalize it to `None` so the drain
                    // loop leaves `CURRENT_TENANT` UNSET rather than scoping a
                    // `tenant_scoped` repo to `tenant_id = ''` (or whitespace).
                    // A non-blank tenant is preserved verbatim (never trimmed).
                    Value::String(tenant) if tenant.trim().is_empty() => None,
                    Value::String(tenant) => Some(tenant),
                    _ => None,
                }),
            _ => None,
        };
        (value, tenant)
    })
}

/// `SQLite`: a UTC `YYYY-MM-DD HH:MM:SS.fff` timestamp string. Ordered
/// lexicographically it compares correctly against `CURRENT_TIMESTAMP` defaults
/// (which share the leading second-precision prefix), and the fractional part
/// gives the worker sub-second retry/lease resolution.
#[cfg(feature = "sqlite")]
fn sqlite_timestamp(instant: chrono::DateTime<chrono::Utc>) -> String {
    instant.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

#[cfg(all(feature = "ws", not(feature = "sqlite")))]
pub fn start_repository_commit_hook_worker(
    pool: PgPool,
    channels: Option<crate::channels::Channels>,
    shutdown: CancellationToken,
) {
    register_inventory_repository_commit_hook_runners();
    if !should_start_repository_commit_hook_worker(&registered_handler_keys()) {
        return;
    }

    if let Some(ch) = channels.clone() {
        let kick_state = repository_commit_hook_kick_state(&pool);
        let _ = kick_state.channels.set(ch);
    }

    let worker_id = repository_commit_hook_worker_id();
    tokio::spawn(async move {
        if let Some(ch) = channels {
            CURRENT_CHANNELS
                .scope(ch, async move {
                    loop {
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            () = tokio::time::sleep(HOOK_WORKER_IDLE_SLEEP) => {
                                recover_stale_repository_commit_hooks(&pool, &worker_id).await;
                                drain_ready_repository_commit_hooks(&pool, &worker_id, 32).await;
                            }
                        }
                    }
                })
                .await;
        } else {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(HOOK_WORKER_IDLE_SLEEP) => {
                        recover_stale_repository_commit_hooks(&pool, &worker_id).await;
                        drain_ready_repository_commit_hooks(&pool, &worker_id, 32).await;
                    }
                }
            }
        }
    });
}

#[cfg(all(not(feature = "ws"), not(feature = "sqlite")))]
pub fn start_repository_commit_hook_worker(pool: PgPool, shutdown: CancellationToken) {
    register_inventory_repository_commit_hook_runners();
    if !should_start_repository_commit_hook_worker(&registered_handler_keys()) {
        return;
    }

    let worker_id = repository_commit_hook_worker_id();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(HOOK_WORKER_IDLE_SLEEP) => {
                    recover_stale_repository_commit_hooks(&pool, &worker_id).await;
                    drain_ready_repository_commit_hooks(&pool, &worker_id, 32).await;
                }
            }
        }
    });
}

// ── `SQLite` durable worker (#1996 item 5) ─────────────────────────────────────
//
// Single-node, single-writer: no `LISTEN/NOTIFY`, no `FOR UPDATE SKIP LOCKED`.
// The worker polls on the idle sleep AND wakes immediately on an in-process
// `Notify` kick, then claims one ready row at a time under a `BEGIN IMMEDIATE`
// write lock (`sqlite_claim_next_repository_commit_hook`). Graceful shutdown is
// the same `tokio::select!` on the child `CancellationToken` the Postgres worker
// uses: the loop finishes the in-flight `drain` iteration (claim → run →
// ack/nack) it is on and settles it before observing `cancelled()` on the next
// pass, so no leased row is abandoned mid-flight.
#[cfg(all(feature = "ws", feature = "sqlite"))]
pub fn start_repository_commit_hook_worker(
    pool: RtPool,
    channels: Option<crate::channels::Channels>,
    shutdown: CancellationToken,
) {
    register_inventory_repository_commit_hook_runners();
    if !should_start_repository_commit_hook_worker(&registered_handler_keys()) {
        return;
    }

    let worker_id = repository_commit_hook_worker_id();
    let kick = sqlite_repository_commit_hook_kick(&pool);
    tokio::spawn(async move {
        if let Some(ch) = channels {
            CURRENT_CHANNELS
                .scope(
                    ch,
                    sqlite_repository_commit_hook_worker_loop(pool, worker_id, kick, shutdown),
                )
                .await;
        } else {
            sqlite_repository_commit_hook_worker_loop(pool, worker_id, kick, shutdown).await;
        }
    });
}

#[cfg(all(not(feature = "ws"), feature = "sqlite"))]
pub fn start_repository_commit_hook_worker(pool: RtPool, shutdown: CancellationToken) {
    register_inventory_repository_commit_hook_runners();
    if !should_start_repository_commit_hook_worker(&registered_handler_keys()) {
        return;
    }

    let worker_id = repository_commit_hook_worker_id();
    let kick = sqlite_repository_commit_hook_kick(&pool);
    tokio::spawn(sqlite_repository_commit_hook_worker_loop(
        pool, worker_id, kick, shutdown,
    ));
}

#[cfg(feature = "sqlite")]
async fn sqlite_repository_commit_hook_worker_loop(
    pool: RtPool,
    worker_id: String,
    kick: Arc<Notify>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            () = kick.notified() => {
                recover_stale_repository_commit_hooks(&pool, &worker_id).await;
                sqlite_drain_ready_repository_commit_hooks(&pool, &worker_id, 32).await;
            }
            () = tokio::time::sleep(HOOK_WORKER_IDLE_SLEEP) => {
                recover_stale_repository_commit_hooks(&pool, &worker_id).await;
                sqlite_drain_ready_repository_commit_hooks(&pool, &worker_id, 32).await;
            }
        }
    }
}

/// Nudge dispatch after a mutation commits, without relying on this replica for
/// durability. Polling workers on all replicas can still claim the row later.
#[cfg(not(feature = "sqlite"))]
pub fn kick_repository_commit_hook_dispatcher(pool: &PgPool) {
    register_inventory_repository_commit_hook_runners();
    if !should_start_repository_commit_hook_worker(&registered_handler_keys()) {
        return;
    }

    let state = repository_commit_hook_kick_state(pool);
    if state.request_kick() {
        state.notify.notify_one();
    }
}

/// `SQLite` variant of the commit-hook dispatcher kick (#1996 item 5).
///
/// `SQLite` has no `LISTEN/NOTIFY`, but it is single-node, so an in-process
/// `Notify` wakes this pool's poll loop the instant a mutation commits. A missed
/// kick is harmless: the loop also wakes on its idle sleep and drains, so
/// durability never depends on the kick landing.
#[cfg(feature = "sqlite")]
pub fn kick_repository_commit_hook_dispatcher(pool: &RtPool) {
    register_inventory_repository_commit_hook_runners();
    if !should_start_repository_commit_hook_worker(&registered_handler_keys()) {
        return;
    }

    sqlite_repository_commit_hook_kick(pool).notify_one();
}

#[cfg(not(feature = "sqlite"))]
fn repository_commit_hook_kick_state(pool: &PgPool) -> Arc<RepositoryCommitHookKickState> {
    let key = repository_commit_hook_pool_key(pool);
    let registry = REPOSITORY_COMMIT_HOOK_KICKERS.get_or_init(|| RwLock::new(HashMap::new()));
    let existing = {
        let registry = registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.get(&key).cloned()
    };
    if let Some(state) = existing {
        return state;
    }

    let mut registry = registry
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = registry.get(&key).cloned() {
        return state;
    }

    let state = Arc::new(RepositoryCommitHookKickState::default());
    spawn_repository_commit_hook_kick_worker(pool.clone(), state.clone());
    registry.insert(key, state.clone());
    state
}

#[cfg(not(feature = "sqlite"))]
fn repository_commit_hook_pool_key(pool: &PgPool) -> usize {
    std::ptr::from_ref(pool.manager()).addr()
}

#[cfg(not(feature = "sqlite"))]
fn spawn_repository_commit_hook_kick_worker(
    pool: PgPool,
    state: Arc<RepositoryCommitHookKickState>,
) {
    let worker_id = repository_commit_hook_worker_id();
    tokio::spawn(async move {
        loop {
            state.notify.notified().await;
            while state.take_pending_kick() {
                #[cfg(feature = "ws")]
                {
                    let ch_opt = state.channels.get().cloned().or_else(get_global_channels);
                    if let Some(ch) = ch_opt {
                        let pool_clone = pool.clone();
                        let worker_id_clone = worker_id.clone();
                        CURRENT_CHANNELS
                            .scope(ch, async move {
                                drain_ready_repository_commit_hooks(
                                    &pool_clone,
                                    &worker_id_clone,
                                    32,
                                )
                                .await;
                            })
                            .await;
                        continue;
                    }
                }
                drain_ready_repository_commit_hooks(&pool, &worker_id, 32).await;
            }
        }
    });
}

#[cfg(not(feature = "sqlite"))]
pub async fn drain_ready_repository_commit_hooks(pool: &PgPool, worker_id: &str, max_rows: usize) {
    for _ in 0..max_rows {
        let Some(row) = pg_claim_next_repository_commit_hook(pool, worker_id).await else {
            break;
        };

        let heartbeat_shutdown = CancellationToken::new();
        let heartbeat_task = tokio::spawn(heartbeat_repository_commit_hook_claim(
            pool.clone(),
            row.id.clone(),
            worker_id.to_owned(),
            heartbeat_shutdown.child_token(),
        ));
        let result = run_repository_commit_hook_row(&row).await;

        match result {
            Ok(()) => {
                if let Err(error) =
                    ack_repository_commit_hook_success(pool, &row.id, worker_id).await
                {
                    tracing::warn!(
                        hook_id = %row.id,
                        error = %error,
                        "failed to ack repository commit hook success"
                    );
                }
            }
            Err(error) => {
                let failures_total = crate::db::record_after_commit_failure();
                tracing::error!(
                    hook_id = %row.id,
                    handler_key = %row.handler_key,
                    hook_name = %row.hook_name,
                    autumn.after_commit.failures_total = failures_total,
                    "repository after_commit hook failed: {error}"
                );
                if let Err(nack_error) =
                    pg_nack_repository_commit_hook_failure(pool, &row.id, worker_id, &error, &row)
                        .await
                {
                    tracing::warn!(
                        hook_id = %row.id,
                        error = %nack_error,
                        "failed to record repository commit hook failure"
                    );
                }
            }
        }

        heartbeat_shutdown.cancel();
        if let Err(error) = heartbeat_task.await {
            tracing::warn!(
                hook_id = %row.id,
                error = %error,
                "repository commit hook heartbeat task failed"
            );
        }
    }
}

#[cfg(not(feature = "sqlite"))]
async fn run_repository_commit_hook_row(row: &RepositoryCommitHookRow) -> Result<(), String> {
    let context = serde_json::from_str::<Value>(&row.context)
        .map_err(|error| format!("decode repository hook context: {error}"))?;
    run_repository_commit_hook_row_value(row, context).await
}

/// Run a claimed hook with an already-parsed `context` [`Value`]. The `SQLite`
/// worker parses (and strips the tenant key from) the context before calling this;
/// the Postgres worker parses it in `run_repository_commit_hook_row`.
async fn run_repository_commit_hook_row_value(
    row: &RepositoryCommitHookRow,
    context: Value,
) -> Result<(), String> {
    let record = serde_json::from_str::<Value>(&row.record)
        .map_err(|error| format!("decode repository hook record: {error}"))?;
    let result = std::panic::AssertUnwindSafe(run_registered_repository_commit_hook(
        &row.handler_key,
        &row.hook_name,
        context,
        record,
    ))
    .catch_unwind()
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        // `message`, not `Display`: both nack paths persist this in
        // `last_error`, so it must not move when `Display` gains the field
        // list of a hook that returned a validation error.
        Ok(Err(error)) => Err(error.message()),
        Err(panic) => Err(format_repository_commit_hook_panic(&*panic)),
    }
}

async fn run_registered_repository_commit_hook(
    handler_key: &str,
    hook_name: &str,
    context: Value,
    record: Value,
) -> AutumnResult<()> {
    let runner = {
        let registry = REPOSITORY_COMMIT_HOOK_RUNNERS
            .get_or_init(|| RwLock::new(HashMap::new()))
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .get(handler_key)
            .and_then(|registration| registration.runner(hook_name))
    };

    let Some(runner) = runner else {
        return Err(AutumnError::internal_server_error_msg(format!(
            "repository commit hook runner not registered: handler_key={handler_key}, hook_name={hook_name}"
        )));
    };

    runner(context, record).await
}

async fn heartbeat_repository_commit_hook_claim(
    pool: RtPool,
    hook_id: String,
    worker_id: String,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(HOOK_CLAIM_HEARTBEAT_INTERVAL) => {
                match extend_repository_commit_hook_claim(&pool, &hook_id, &worker_id).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        tracing::warn!(
                            hook_id = %hook_id,
                            error = %error,
                            "failed to extend repository commit hook claim"
                        );
                    }
                }
            }
        }
    }
}

async fn heartbeat_repository_commit_hook_pending_finalizer(
    pool: RtPool,
    hook_id: String,
    owner: String,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(HOOK_PENDING_FINALIZER_HEARTBEAT_INTERVAL) => {
                match extend_repository_commit_hook_pending_finalizer(&pool, &hook_id, &owner).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        tracing::warn!(
                            hook_id = %hook_id,
                            error = %error,
                            "failed to extend repository commit hook pending finalizer lease"
                        );
                    }
                }
            }
        }
    }
}

fn registered_handler_keys() -> Vec<String> {
    REPOSITORY_COMMIT_HOOK_RUNNERS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect()
}

const fn should_start_repository_commit_hook_worker(handler_keys: &[String]) -> bool {
    !handler_keys.is_empty()
}

#[cfg(not(feature = "sqlite"))]
async fn pg_claim_next_repository_commit_hook(
    pool: &PgPool,
    worker_id: &str,
) -> Option<RepositoryCommitHookRow> {
    let handler_keys = registered_handler_keys();
    if handler_keys.is_empty() {
        return None;
    }

    let mut conn = pool.get().await.ok()?;
    let sql = format!(
        "UPDATE autumn_repository_commit_hooks \
         SET status = 'running', started_at = NOW(), claimed_by = $2, claimed_at = NOW() \
         WHERE id = ( \
           SELECT id FROM autumn_repository_commit_hooks \
           WHERE status = 'enqueued' \
             AND run_at <= NOW() \
             AND handler_key = ANY($1) \
           ORDER BY run_at ASC, enqueued_at ASC \
           LIMIT 1 \
           FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING {HOOK_SELECT_COLS}"
    );

    diesel::sql_query(sql)
        .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(handler_keys)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .get_result::<RepositoryCommitHookRow>(&mut *conn)
        .await
        .optional()
        .unwrap_or_else(|error| {
            if is_missing_hook_table_error(&error) {
                tracing::debug!(
                    error = %error,
                    "repository commit hook queue table is not available yet"
                );
            } else {
                tracing::warn!(error = %error, "repository commit hook claim query failed");
            }
            None
        })
}

async fn ack_repository_commit_hook_success(
    pool: &RtPool,
    hook_id: &str,
    worker_id: &str,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("commit hook pool error: {error}"))
    })?;

    diesel::sql_query(HOOK_ACK_SUCCESS_SQL)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook ack failed: {error}"
            ))
        })
}

async fn extend_repository_commit_hook_claim(
    pool: &RtPool,
    hook_id: &str,
    worker_id: &str,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("commit hook pool error: {error}"))
    })?;

    diesel::sql_query(HOOK_EXTEND_CLAIM_SQL)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook claim heartbeat failed: {error}"
            ))
        })
}

async fn extend_repository_commit_hook_pending_finalizer(
    pool: &RtPool,
    hook_id: &str,
    owner: &str,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("commit hook pool error: {error}"))
    })?;

    diesel::sql_query(HOOK_EXTEND_PENDING_FINALIZER_SQL)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(owner)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook pending finalizer heartbeat failed: {error}"
            ))
        })
}

#[cfg(not(feature = "sqlite"))]
async fn pg_nack_repository_commit_hook_failure(
    pool: &PgPool,
    hook_id: &str,
    worker_id: &str,
    error: &str,
    row: &RepositoryCommitHookRow,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("pg pool error: {error}"))
    })?;

    if row.attempt < row.max_attempts {
        let delay_ms = retry_delay_ms(row.initial_backoff_ms, row.attempt);
        diesel::sql_query(
            "UPDATE autumn_repository_commit_hooks \
             SET status = 'enqueued', \
                 attempt = attempt + 1, \
                 run_at = NOW() + ($1::BIGINT * INTERVAL '1 millisecond'), \
                 started_at = NULL, \
                 finished_at = NULL, \
                 claimed_by = NULL, \
                 claimed_at = NULL, \
                 last_error = $2 \
             WHERE id = $3 AND claimed_by = $4 AND status = 'running'",
        )
        .bind::<diesel::sql_types::BigInt, _>(delay_ms)
        .bind::<diesel::sql_types::Text, _>(error)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook retry failed: {error}"
            ))
        })
    } else {
        diesel::sql_query(
            "UPDATE autumn_repository_commit_hooks \
             SET status = 'failed', \
                 finished_at = NOW(), \
                 claimed_by = NULL, \
                 claimed_at = NULL, \
                 last_error = $1 \
             WHERE id = $2 AND claimed_by = $3 AND status = 'running'",
        )
        .bind::<diesel::sql_types::Text, _>(error)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook dead-letter failed: {error}"
            ))
        })
    }
}

async fn recover_stale_repository_commit_hooks(pool: &RtPool, worker_id: &str) {
    let Ok(mut conn) = pool.get().await else {
        return;
    };

    // Postgres computes the stale cutoff inline (`NOW() - INTERVAL`); `SQLite` has
    // no interval arithmetic, so the cutoff is computed here and bound as a
    // comparable UTC timestamp string. Same recovery semantics on both.
    let running_result = crate::backend_select! {
        pg => {{
            let stale_after_ms =
                i64::try_from(HOOK_STALE_CLAIM_AFTER.as_millis()).unwrap_or(i64::MAX);
            diesel::sql_query(HOOK_RECOVER_STALE_RUNNING_SQL)
                .bind::<diesel::sql_types::Text, _>(format!("stale claim recovered by {worker_id}"))
                .bind::<diesel::sql_types::BigInt, _>(stale_after_ms)
                .execute(&mut *conn)
                .await
        }},
        sqlite => {{
            let cutoff = sqlite_timestamp(sqlite_stale_claim_cutoff());
            diesel::sql_query(HOOK_RECOVER_STALE_RUNNING_SQL)
                .bind::<diesel::sql_types::Text, _>(format!("stale claim recovered by {worker_id}"))
                .bind::<diesel::sql_types::Text, _>(cutoff)
                .execute(&mut *conn)
                .await
        }},
    };
    if let Err(error) = running_result {
        if is_missing_hook_table_error(&error) {
            tracing::debug!(
                error = %error,
                "repository commit hook queue table is not available yet"
            );
        } else {
            tracing::warn!(error = %error, "repository commit hook stale recovery failed");
        }
    }

    let pending_result = crate::backend_select! {
        pg => {{
            let stale_after_ms =
                i64::try_from(HOOK_STALE_CLAIM_AFTER.as_millis()).unwrap_or(i64::MAX);
            diesel::sql_query(HOOK_RECOVER_STALE_PENDING_SQL)
                .bind::<diesel::sql_types::Text, _>(format!(
                    "stale pending after hook recovered by {worker_id}"
                ))
                .bind::<diesel::sql_types::BigInt, _>(stale_after_ms)
                .execute(&mut *conn)
                .await
        }},
        sqlite => {{
            let cutoff = sqlite_timestamp(sqlite_stale_claim_cutoff());
            diesel::sql_query(HOOK_RECOVER_STALE_PENDING_SQL)
                .bind::<diesel::sql_types::Text, _>(format!(
                    "stale pending after hook recovered by {worker_id}"
                ))
                .bind::<diesel::sql_types::Text, _>(cutoff)
                .execute(&mut *conn)
                .await
        }},
    };
    if let Err(error) = pending_result {
        if is_missing_hook_table_error(&error) {
            tracing::debug!(
                error = %error,
                "repository commit hook queue table is not available yet"
            );
        } else {
            tracing::warn!(
                error = %error,
                "repository commit hook stale pending recovery failed"
            );
        }
    }
}

/// `SQLite`: the UTC instant before which a still-`running` (or leased-pending)
/// claim is considered abandoned by a dead worker.
#[cfg(feature = "sqlite")]
fn sqlite_stale_claim_cutoff() -> chrono::DateTime<chrono::Utc> {
    let stale_after = chrono::Duration::from_std(HOOK_STALE_CLAIM_AFTER)
        .unwrap_or_else(|_| chrono::Duration::seconds(60));
    chrono::Utc::now() - stale_after
}

// ── `SQLite` claim / drain / nack (#1996 item 5) ───────────────────────────────

/// Claim the next ready hook row under a `BEGIN IMMEDIATE` write lock. `SQLite` is
/// single-writer, so instead of `FOR UPDATE SKIP LOCKED` the worker takes the
/// write lock up front, `SELECT`s the oldest ready+registered row, flips it to
/// `running`, and returns it — no other writer can interleave between the select
/// and the update.
#[cfg(feature = "sqlite")]
async fn sqlite_claim_next_repository_commit_hook(
    pool: &RtPool,
    worker_id: &str,
) -> Option<RepositoryCommitHookRow> {
    let handler_keys = registered_handler_keys();
    if handler_keys.is_empty() {
        return None;
    }

    let mut conn = pool.get().await.ok()?;
    let now = sqlite_timestamp(chrono::Utc::now());
    // The registered handler keys are internal Rust type-path strings (no
    // user input); inline them as an escaped `IN (...)` literal list so the raw
    // `sql_query` needs a fixed bind arity (`run_at <= ?` only). Single quotes
    // are still doubled defensively.
    let handler_in_list = handler_keys
        .iter()
        .map(|key| format!("'{}'", key.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let select_sql = format!(
        "SELECT {HOOK_SELECT_COLS} FROM autumn_repository_commit_hooks \
         WHERE status = 'enqueued' \
           AND run_at <= ? \
           AND handler_key IN ({handler_in_list}) \
         ORDER BY run_at ASC, enqueued_at ASC \
         LIMIT 1"
    );
    let worker_id = worker_id.to_owned();

    let claim = crate::db::scoped_immediate_transaction::<
        Option<RepositoryCommitHookRow>,
        diesel::result::Error,
        _,
    >(&mut conn, move |conn| {
        async move {
            let Some(row) = diesel::sql_query(select_sql)
                .bind::<diesel::sql_types::Text, _>(now.clone())
                .get_result::<RepositoryCommitHookRow>(&mut *conn)
                .await
                .optional()?
            else {
                return Ok(None);
            };

            diesel::sql_query(
                "UPDATE autumn_repository_commit_hooks \
                 SET status = 'running', started_at = ?, claimed_by = ?, claimed_at = ? \
                 WHERE id = ? AND status = 'enqueued'",
            )
            .bind::<diesel::sql_types::Text, _>(now.clone())
            .bind::<diesel::sql_types::Text, _>(worker_id)
            .bind::<diesel::sql_types::Text, _>(now)
            .bind::<diesel::sql_types::Text, _>(row.id.clone())
            .execute(&mut *conn)
            .await?;

            Ok(Some(row))
        }
        .scope_boxed()
    })
    .await;

    match claim {
        Ok(row) => row,
        Err(error) => {
            if is_missing_hook_table_error(&error) {
                tracing::debug!(
                    error = %error,
                    "repository commit hook queue table is not available yet"
                );
            } else {
                tracing::warn!(error = %error, "repository commit hook claim query failed");
            }
            None
        }
    }
}

/// `SQLite` drain loop: claim → lease heartbeat → run (under the originating tenant
/// scope) → ack/nack, one row at a time. Mirrors the Postgres
/// `drain_ready_repository_commit_hooks` shape.
#[cfg(feature = "sqlite")]
pub async fn sqlite_drain_ready_repository_commit_hooks(
    pool: &RtPool,
    worker_id: &str,
    max_rows: usize,
) {
    for _ in 0..max_rows {
        let Some(row) = sqlite_claim_next_repository_commit_hook(pool, worker_id).await else {
            break;
        };

        let heartbeat_shutdown = CancellationToken::new();
        let heartbeat_task = tokio::spawn(heartbeat_repository_commit_hook_claim(
            pool.clone(),
            row.id.clone(),
            worker_id.to_owned(),
            heartbeat_shutdown.child_token(),
        ));

        // Tenant isolation (INVIOLABLE): re-establish the SAME tenant the hook was
        // enqueued under before running it, so a `tenant_scoped` repository the
        // hook touches resolves to the originating tenant — never another tenant,
        // and never a default/global scope. A hook enqueued with no tenant runs
        // with `CURRENT_TENANT` UNSET, so such a repo fails closed.
        let (context_value, tenant) = extract_originating_tenant_from_context(&row.context);
        let result = match tenant {
            Some(tenant) => {
                crate::tenancy::CURRENT_TENANT
                    .scope(
                        Some(tenant),
                        run_repository_commit_hook_row_value(&row, context_value),
                    )
                    .await
            }
            None => run_repository_commit_hook_row_value(&row, context_value).await,
        };

        match result {
            Ok(()) => {
                if let Err(error) =
                    ack_repository_commit_hook_success(pool, &row.id, worker_id).await
                {
                    tracing::warn!(
                        hook_id = %row.id,
                        error = %error,
                        "failed to ack repository commit hook success"
                    );
                }
            }
            Err(error) => {
                let failures_total = crate::db::record_after_commit_failure();
                tracing::error!(
                    hook_id = %row.id,
                    handler_key = %row.handler_key,
                    hook_name = %row.hook_name,
                    autumn.after_commit.failures_total = failures_total,
                    "repository after_commit hook failed: {error}"
                );
                if let Err(nack_error) = sqlite_nack_repository_commit_hook_failure(
                    pool, &row.id, worker_id, &error, &row,
                )
                .await
                {
                    tracing::warn!(
                        hook_id = %row.id,
                        error = %nack_error,
                        "failed to record repository commit hook failure"
                    );
                }
            }
        }

        heartbeat_shutdown.cancel();
        if let Err(error) = heartbeat_task.await {
            tracing::warn!(
                hook_id = %row.id,
                error = %error,
                "repository commit hook heartbeat task failed"
            );
        }
    }
}

/// `SQLite` retry / dead-letter. Same attempt-count and exponential-backoff
/// semantics as the Postgres `pg_nack_*`, but the `run_at` retry target is
/// computed in Rust (`SQLite` has no `NOW() + INTERVAL`) and bound as a UTC string.
#[cfg(feature = "sqlite")]
async fn sqlite_nack_repository_commit_hook_failure(
    pool: &RtPool,
    hook_id: &str,
    worker_id: &str,
    error: &str,
    row: &RepositoryCommitHookRow,
) -> AutumnResult<bool> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("commit hook pool error: {error}"))
    })?;

    if row.attempt < row.max_attempts {
        let delay_ms = retry_delay_ms(row.initial_backoff_ms, row.attempt);
        let run_at = sqlite_timestamp(
            chrono::Utc::now()
                + chrono::Duration::try_milliseconds(delay_ms)
                    .unwrap_or_else(chrono::Duration::zero),
        );
        diesel::sql_query(
            "UPDATE autumn_repository_commit_hooks \
             SET status = 'enqueued', \
                 attempt = attempt + 1, \
                 run_at = ?, \
                 started_at = NULL, \
                 finished_at = NULL, \
                 claimed_by = NULL, \
                 claimed_at = NULL, \
                 last_error = ? \
             WHERE id = ? AND claimed_by = ? AND status = 'running'",
        )
        .bind::<diesel::sql_types::Text, _>(run_at)
        .bind::<diesel::sql_types::Text, _>(error)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook retry failed: {error}"
            ))
        })
    } else {
        diesel::sql_query(
            "UPDATE autumn_repository_commit_hooks \
             SET status = 'failed', \
                 finished_at = CURRENT_TIMESTAMP, \
                 claimed_by = NULL, \
                 claimed_at = NULL, \
                 last_error = ? \
             WHERE id = ? AND claimed_by = ? AND status = 'running'",
        )
        .bind::<diesel::sql_types::Text, _>(error)
        .bind::<diesel::sql_types::Text, _>(hook_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(|rows| rows > 0)
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "repository commit hook dead-letter failed: {error}"
            ))
        })
    }
}

fn is_missing_hook_table_error(error: &diesel::result::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("autumn_repository_commit_hooks")
        // Postgres reports the missing table as "does not exist" / "UndefinedTable";
        // SQLite reports it as "no such table: autumn_repository_commit_hooks".
        && (message.contains("does not exist")
            || message.contains("undefinedtable")
            || message.contains("no such table"))
}

fn retry_delay_ms(initial_backoff_ms: i64, attempt: i32) -> i64 {
    let exp = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    initial_backoff_ms.saturating_mul(2_i64.saturating_pow(exp))
}

fn format_repository_commit_hook_panic(payload: &(dyn Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str));

    message.map_or_else(
        || "repository commit hook panicked".to_owned(),
        |message| format!("repository commit hook panicked: {message}"),
    )
}

fn repository_commit_hook_id(
    idempotency_key: Option<&str>,
    idempotency_discriminator: Option<&str>,
    handler_key: &str,
    hook_name: &str,
    record: &str,
) -> String {
    let Some(idempotency_key) = idempotency_key.filter(|key| !key.is_empty()) else {
        return uuid::Uuid::new_v4().to_string();
    };

    let mut hasher = sha2::Sha256::new();
    push_hook_id_component(&mut hasher, "handler", handler_key.as_bytes());
    push_hook_id_component(&mut hasher, "hook", hook_name.as_bytes());
    push_hook_id_component(&mut hasher, "idempotency", idempotency_key.as_bytes());
    if let Some(discriminator) = idempotency_discriminator {
        push_hook_id_component(&mut hasher, "mutation", discriminator.as_bytes());
    } else {
        push_hook_id_component(&mut hasher, "record", record.as_bytes());
    }
    format!("idempotent:{}", hex_lower(hasher.finalize()))
}

fn push_hook_id_component(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(
        String::with_capacity(bytes.as_ref().len() * 2),
        |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

pub fn repository_commit_hook_worker_id() -> String {
    format!("repository-hook-{}", uuid::Uuid::new_v4())
}

fn repository_commit_hook_pending_owner_id() -> String {
    format!("repository-hook-pending-{}", uuid::Uuid::new_v4())
}

#[cfg(feature = "ws")]
tokio::task_local! {
    pub static CURRENT_CHANNELS: crate::channels::Channels;
}

#[cfg(feature = "ws")]
static GLOBAL_CHANNELS: std::sync::RwLock<Option<crate::channels::Channels>> =
    std::sync::RwLock::new(None);

#[cfg(feature = "ws")]
pub fn set_global_channels(channels: crate::channels::Channels) {
    if let Ok(mut lock) = GLOBAL_CHANNELS.write() {
        *lock = Some(channels);
    }
}

#[cfg(feature = "ws")]
pub fn clear_global_channels() {
    if let Ok(mut lock) = GLOBAL_CHANNELS.write() {
        *lock = None;
    }
}

#[cfg(feature = "ws")]
#[must_use]
pub fn get_global_channels() -> Option<crate::channels::Channels> {
    CURRENT_CHANNELS
        .try_with(std::clone::Clone::clone)
        .ok()
        .or_else(|| GLOBAL_CHANNELS.read().ok().and_then(|lock| lock.clone()))
}

#[cfg(not(feature = "ws"))]
#[derive(Clone)]
pub struct Channels;

#[cfg(not(feature = "ws"))]
pub struct DummyBroadcast;

#[cfg(not(feature = "ws"))]
impl Channels {
    #[allow(clippy::unused_self)]
    pub const fn broadcast(&self) -> DummyBroadcast {
        DummyBroadcast
    }
}

#[cfg(not(feature = "ws"))]
impl DummyBroadcast {
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub fn publish_oob<T, S>(
        &self,
        _topic: &str,
        _id: &str,
        _swap: S,
        _fragment: &T,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

#[cfg(not(feature = "ws"))]
#[allow(clippy::missing_const_for_fn)]
pub fn set_global_channels(_channels: Channels) {}

#[cfg(not(feature = "ws"))]
pub const fn clear_global_channels() {}

#[cfg(not(feature = "ws"))]
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn get_global_channels() -> Option<Channels> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn registered_runner_executes_matching_hook() {
        let calls = Arc::new(AtomicUsize::new(0));
        let create_calls = calls.clone();
        let handler_key: &'static str = Box::leak(
            format!(
                "test::registered_runner_executes_matching_hook::{}",
                uuid::Uuid::new_v4()
            )
            .into_boxed_str(),
        );

        register_repository_commit_hook_runner(
            handler_key,
            move |_ctx, _record| {
                let create_calls = create_calls.clone();
                async move {
                    create_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            |_ctx, _record| async { Ok(()) },
            |_ctx, _record| async { Ok(()) },
        );

        run_registered_repository_commit_hook(handler_key, "create", Value::Null, Value::Null)
            .await
            .unwrap();

        assert_eq!(calls.as_ref().load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_runner_returns_recoverable_error() {
        let err = run_registered_repository_commit_hook(
            "missing-handler",
            "create",
            Value::Null,
            Value::Null,
        )
        .await
        .expect_err("missing runner should be reported");

        assert!(
            err.to_string().contains("runner not registered"),
            "unexpected error: {err}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn after_hook_failure_marking_returns_when_pool_is_unavailable() {
        use diesel_async::AsyncPgConnection;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;

        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new("not-a-postgres-url");
        let pool = Pool::builder(manager)
            .max_size(1)
            .runtime(deadpool::Runtime::Tokio1)
            .build()
            .expect("pool");

        let result = tokio::time::timeout(
            Duration::from_millis(750),
            mark_repository_commit_hook_after_hook_failed(&pool, "hook-id", "owner", "boom"),
        )
        .await;

        assert!(
            result.is_ok(),
            "after-hook failure marking must not block a committed mutation forever when the pool/database is down"
        );
    }

    #[test]
    fn worker_start_is_disabled_without_registered_handlers() {
        assert!(
            !should_start_repository_commit_hook_worker(&[]),
            "unhooked DB apps must not poll the hook queue"
        );
    }

    #[test]
    fn worker_start_is_enabled_with_registered_handlers() {
        assert!(
            should_start_repository_commit_hook_worker(&["handler".to_owned()]),
            "hooked DB apps should poll the hook queue"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn dispatcher_kick_state_coalesces_pending_notifications() {
        let state = RepositoryCommitHookKickState::default();

        assert!(state.request_kick(), "first kick should notify the worker");
        assert!(
            !state.request_kick(),
            "repeated kicks while one is pending must coalesce"
        );
        assert!(
            state.take_pending_kick(),
            "worker should observe one wakeup"
        );
        assert!(
            !state.take_pending_kick(),
            "observed wakeup should clear pending state"
        );
        assert!(
            state.request_kick(),
            "a later kick after the worker drains should notify again"
        );
    }

    #[test]
    fn retry_delay_is_exponential() {
        assert_eq!(retry_delay_ms(100, 1), 100);
        assert_eq!(retry_delay_ms(100, 2), 200);
        assert_eq!(retry_delay_ms(100, 3), 400);
    }

    #[test]
    fn idempotent_hook_ids_are_deterministic_and_safely_delimited() {
        let record = serde_json::json!({ "id": 1, "title": "first" }).to_string();
        let first = repository_commit_hook_id(
            Some("v2:request"),
            Some("0"),
            "pkg::module::posts::Post",
            "create",
            &record,
        );
        let second = repository_commit_hook_id(
            Some("v2:request"),
            Some("0"),
            "pkg::module::posts::Post",
            "create",
            &record,
        );
        let other_hook = repository_commit_hook_id(
            Some("v2:request"),
            Some("0"),
            "pkg::module::posts::Post",
            "update",
            &record,
        );

        assert_eq!(first, second);
        assert_ne!(first, other_hook);
        assert!(first.starts_with("idempotent:"));
        assert!(!first.contains("v2:request"));
        assert!(!first.contains("pkg::module::posts::Post"));
    }

    #[test]
    fn non_idempotent_hook_ids_remain_fresh() {
        let record = serde_json::json!({ "id": 1 }).to_string();
        let first = repository_commit_hook_id(None, None, "handler", "create", &record);
        let second = repository_commit_hook_id(None, None, "handler", "create", &record);

        assert_ne!(first, second);
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        assert!(uuid::Uuid::parse_str(&second).is_ok());
    }

    #[test]
    fn hook_insert_sql_ignores_duplicate_idempotent_rows() {
        assert!(
            HOOK_ENQUEUE_INSERT_SQL.contains("ON CONFLICT (id) DO NOTHING"),
            "direct delete commit hooks must dedupe duplicate idempotency rows"
        );
        assert!(
            HOOK_PENDING_INSERT_SQL.contains("ON CONFLICT (id) DO UPDATE")
                && HOOK_PENDING_INSERT_SQL.contains(
                    "WHERE autumn_repository_commit_hooks.status IN ('pending_after_hook', 'after_hook_failed')"
                ),
            "staged create/update commit hooks must dedupe successful duplicate rows while allowing a retry to reclaim unfinalized or failed staged rows"
        );
    }

    #[test]
    fn idempotent_hook_ids_distinguish_records_in_same_request() {
        let first_record = serde_json::json!({ "id": 1, "title": "first" }).to_string();
        let second_record = serde_json::json!({ "id": 2, "title": "second" }).to_string();

        let first = repository_commit_hook_id(
            Some("v2:request"),
            Some("0"),
            "pkg::module::posts::Post",
            "create",
            &first_record,
        );
        let second = repository_commit_hook_id(
            Some("v2:request"),
            Some("1"),
            "pkg::module::posts::Post",
            "create",
            &second_record,
        );

        assert_ne!(
            first, second,
            "one idempotent request can stage multiple committed records for the same hook"
        );
    }

    #[test]
    fn idempotent_hook_ids_distinguish_same_record_sequences_in_same_request() {
        let record = serde_json::json!({ "id": 1, "title": "same" }).to_string();

        let first = repository_commit_hook_id(
            Some("v2:request"),
            Some("0"),
            "pkg::module::posts::Post",
            "update",
            &record,
        );
        let second = repository_commit_hook_id(
            Some("v2:request"),
            Some("1"),
            "pkg::module::posts::Post",
            "update",
            &record,
        );
        let first_again = repository_commit_hook_id(
            Some("v2:request"),
            Some("0"),
            "pkg::module::posts::Post",
            "update",
            &record,
        );

        assert_eq!(
            first, first_again,
            "the same mutation sequence must dedupe across duplicate request attempts"
        );
        assert_ne!(
            first, second,
            "distinct mutations in one request must not collapse just because their final record serializes identically"
        );
    }

    #[test]
    fn missing_idempotent_finalization_fails_closed() {
        let err = missing_repository_commit_hook_finalization_result("idempotent:abc")
            .expect_err("missing idempotent staged rows should fail closed");

        assert!(
            err.to_string()
                .contains("finalization skipped missing staged row"),
            "unexpected error: {err}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn pending_insert_reclaims_only_unfinalized_or_failed_rows() {
        assert!(
            HOOK_PENDING_INSERT_SQL.contains("ON CONFLICT (id) DO UPDATE")
                && HOOK_PENDING_INSERT_SQL.contains("status = 'pending_after_hook'")
                && HOOK_PENDING_INSERT_SQL.contains("context = EXCLUDED.context")
                && HOOK_PENDING_INSERT_SQL.contains("record = EXCLUDED.record")
                && HOOK_PENDING_INSERT_SQL.contains("claimed_by = EXCLUDED.claimed_by")
                && HOOK_PENDING_INSERT_SQL.contains("last_error = NULL"),
            "a retried idempotent mutation must be able to restage durable hooks after an earlier unfinalized or failed regular after-hook"
        );
        assert!(
            HOOK_PENDING_INSERT_SQL.contains(
                "WHERE autumn_repository_commit_hooks.status IN ('pending_after_hook', 'after_hook_failed')"
            ),
            "restaging must reclaim unfinalized pending rows but not replace already finalized, enqueued, running, completed, or worker-failed rows"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pending_insert_reclaims_only_unfinalized_or_failed_rows_sqlite() {
        assert!(
            HOOK_PENDING_INSERT_SQL.contains("ON CONFLICT (id) DO UPDATE")
                && HOOK_PENDING_INSERT_SQL.contains("status = 'pending_after_hook'")
                && HOOK_PENDING_INSERT_SQL.contains("context = excluded.context")
                && HOOK_PENDING_INSERT_SQL.contains("record = excluded.record")
                && HOOK_PENDING_INSERT_SQL.contains("claimed_by = excluded.claimed_by")
                && HOOK_PENDING_INSERT_SQL.contains("last_error = NULL"),
            "a retried idempotent mutation must be able to restage durable hooks after an earlier unfinalized or failed regular after-hook"
        );
        assert!(
            HOOK_PENDING_INSERT_SQL.contains(
                "WHERE autumn_repository_commit_hooks.status IN ('pending_after_hook', 'after_hook_failed')"
            ),
            "restaging must reclaim unfinalized pending rows but not replace already finalized, enqueued, running, completed, or worker-failed rows"
        );
    }

    #[test]
    fn missing_non_idempotent_finalization_remains_an_error() {
        let err = missing_repository_commit_hook_finalization_result("random-id")
            .expect_err("missing non-idempotent staged rows should still be reported");

        assert!(
            err.to_string()
                .contains("finalization skipped missing staged row"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn claim_heartbeat_runs_before_stale_recovery() {
        assert!(
            HOOK_CLAIM_HEARTBEAT_INTERVAL < HOOK_STALE_CLAIM_AFTER,
            "heartbeat interval must be shorter than stale recovery threshold"
        );
        assert!(
            HOOK_PENDING_FINALIZER_HEARTBEAT_INTERVAL < HOOK_STALE_CLAIM_AFTER,
            "pending finalizer heartbeat interval must be shorter than stale recovery threshold"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn success_ack_clears_retained_payloads() {
        assert!(
            HOOK_ACK_SUCCESS_SQL.contains("context = '{}'::JSONB"),
            "success ack must clear serialized context payload"
        );
        assert!(
            HOOK_ACK_SUCCESS_SQL.contains("record = '{}'::JSONB"),
            "success ack must clear serialized record payload"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn success_ack_clears_retained_payloads_sqlite() {
        assert!(
            HOOK_ACK_SUCCESS_SQL.contains("context = '{}'"),
            "success ack must clear serialized context payload"
        );
        assert!(
            HOOK_ACK_SUCCESS_SQL.contains("record = '{}'"),
            "success ack must clear serialized record payload"
        );
        assert!(
            !HOOK_ACK_SUCCESS_SQL.contains("::JSONB") && !HOOK_ACK_SUCCESS_SQL.contains("NOW()"),
            "the SQLite ack must not carry Postgres-only JSONB casts or NOW()"
        );
    }

    #[test]
    fn stale_running_recovery_counts_against_max_attempts() {
        assert!(
            HOOK_RECOVER_STALE_RUNNING_SQL.contains("attempt < max_attempts"),
            "stale running recovery must branch on retry exhaustion"
        );
        assert!(
            HOOK_RECOVER_STALE_RUNNING_SQL.contains("attempt = CASE"),
            "stale running recovery must not requeue without updating attempt accounting"
        );
        assert!(
            HOOK_RECOVER_STALE_RUNNING_SQL.contains("attempt + 1"),
            "stale running recovery must consume the abandoned attempt"
        );
        assert!(
            HOOK_RECOVER_STALE_RUNNING_SQL.contains("ELSE 'failed'"),
            "stale running recovery must dead-letter rows already at max_attempts"
        );
        assert!(
            !HOOK_RECOVER_STALE_RUNNING_SQL.contains("SET status = 'enqueued'"),
            "stale running recovery must not unconditionally requeue exhausted rows"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn staged_hooks_are_not_dispatchable_until_finalized_after_regular_hooks() {
        assert!(
            HOOK_PENDING_INSERT_SQL.contains("status, attempt")
                && HOOK_PENDING_INSERT_SQL.contains("'pending_after_hook'"),
            "create/update hooks must first be staged in a non-dispatchable lifecycle state"
        );
        assert!(
            HOOK_PENDING_INSERT_SQL.contains("claimed_by, claimed_at"),
            "staged rows must carry a finalizer lease so recovery can distinguish live after hooks from abandoned rows"
        );
        assert!(
            HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("status = 'after_hook_succeeded'")
                && HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("context = $1::JSONB")
                && HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("record = $2::JSONB"),
            "regular after-hook success must durably persist finalized hook payload before enqueue"
        );
        assert!(
            HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL
                .contains("WHERE id = $3 AND claimed_by = $4 AND status = 'pending_after_hook'"),
            "success marking must only advance the staged row it owns"
        );
        assert!(
            HOOK_FINALIZE_AFTER_HOOK_SQL.contains("status = 'enqueued'")
                && HOOK_FINALIZE_AFTER_HOOK_SQL.contains(
                    "WHERE id = $1 AND claimed_by = $2 AND status = 'after_hook_succeeded'"
                ),
            "after-hook finalization must only enqueue rows with a durable regular-hook success marker"
        );
        assert!(
            HOOK_AFTER_HOOK_FAILED_SQL.contains("status = 'after_hook_failed'")
                && HOOK_AFTER_HOOK_FAILED_SQL.contains("context = '{}'::JSONB")
                && HOOK_AFTER_HOOK_FAILED_SQL.contains("record = '{}'::JSONB")
                && HOOK_AFTER_HOOK_FAILED_SQL.contains(
                    "WHERE id = $2 AND claimed_by = $3 AND status = 'pending_after_hook'"
                ),
            "failed regular after hooks must mark only the owner-scoped staged row terminal and non-dispatchable"
        );
        assert!(
            !HOOK_AFTER_HOOK_FAILED_SQL.contains("claimed_by IS NULL")
                && !HOOK_AFTER_HOOK_FAILED_SQL.contains("'enqueued'"),
            "duplicate idempotent retries must not dead-letter already finalized hook rows"
        );
        assert!(
            HOOK_EXTEND_PENDING_FINALIZER_SQL.contains("claimed_at = NOW()")
                && HOOK_EXTEND_PENDING_FINALIZER_SQL.contains("status = 'pending_after_hook'"),
            "long-running regular after hooks must heartbeat their staged-row finalizer lease"
        );
        assert!(
            HOOK_RECOVER_STALE_PENDING_SQL
                .contains("status IN ('pending_after_hook', 'after_hook_succeeded')")
                && HOOK_RECOVER_STALE_PENDING_SQL
                    .contains("WHEN status = 'after_hook_succeeded' THEN 'enqueued'")
                && HOOK_RECOVER_STALE_PENDING_SQL.contains("ELSE 'after_hook_failed'"),
            "stale recovery must enqueue only rows with a durable regular-hook success marker"
        );
        assert!(
            HOOK_RECOVER_STALE_PENDING_SQL
                .contains("WHEN status = 'pending_after_hook' THEN '{}'::JSONB"),
            "ambiguous stale pending rows must be failed closed without retaining payloads"
        );
        assert!(
            HOOK_RECOVER_STALE_PENDING_SQL
                .contains("WHEN status = 'pending_after_hook' THEN NOW()"),
            "ambiguous stale pending rows must be marked terminal when failed closed"
        );
    }

    // `SQLite` fork of the staged-lifecycle invariants: identical status-machine
    // wording, but `CURRENT_TIMESTAMP`/`excluded`/bare-`'{}'` in place of the
    // Postgres `NOW()`/`EXCLUDED`/`::JSONB`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn staged_hooks_are_not_dispatchable_until_finalized_after_regular_hooks_sqlite() {
        assert!(
            HOOK_PENDING_INSERT_SQL.contains("'pending_after_hook'")
                && HOOK_PENDING_INSERT_SQL.contains("claimed_by, claimed_at"),
            "create/update hooks must first be staged in a non-dispatchable leased state"
        );
        assert!(
            HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("status = 'after_hook_succeeded'")
                && HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("context = ?")
                && HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("record = ?")
                && HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL
                    .contains("WHERE id = ? AND claimed_by = ? AND status = 'pending_after_hook'"),
            "regular after-hook success must durably persist finalized hook payload before enqueue"
        );
        assert!(
            HOOK_FINALIZE_AFTER_HOOK_SQL.contains("status = 'enqueued'")
                && HOOK_FINALIZE_AFTER_HOOK_SQL.contains(
                    "WHERE id = ? AND claimed_by = ? AND status = 'after_hook_succeeded'"
                ),
            "after-hook finalization must only enqueue rows with a durable regular-hook success marker"
        );
        assert!(
            HOOK_AFTER_HOOK_FAILED_SQL.contains("status = 'after_hook_failed'")
                && HOOK_AFTER_HOOK_FAILED_SQL.contains("context = '{}'")
                && HOOK_AFTER_HOOK_FAILED_SQL.contains("record = '{}'")
                && HOOK_AFTER_HOOK_FAILED_SQL
                    .contains("WHERE id = ? AND claimed_by = ? AND status = 'pending_after_hook'"),
            "failed regular after hooks must mark only the owner-scoped staged row terminal and non-dispatchable"
        );
        assert!(
            HOOK_RECOVER_STALE_PENDING_SQL
                .contains("status IN ('pending_after_hook', 'after_hook_succeeded')")
                && HOOK_RECOVER_STALE_PENDING_SQL
                    .contains("WHEN status = 'after_hook_succeeded' THEN 'enqueued'")
                && HOOK_RECOVER_STALE_PENDING_SQL.contains("ELSE 'after_hook_failed'")
                && HOOK_RECOVER_STALE_PENDING_SQL
                    .contains("WHEN status = 'pending_after_hook' THEN '{}'"),
            "stale recovery must enqueue only durably-succeeded rows and fail ambiguous ones closed"
        );
        assert!(
            !HOOK_PENDING_INSERT_SQL.contains("::JSONB")
                && !HOOK_AFTER_HOOK_FAILED_SQL.contains("::JSONB")
                && !HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL.contains("::JSONB"),
            "the SQLite staged-lifecycle SQL must carry no Postgres-only JSONB casts"
        );
    }

    #[test]
    fn pending_heartbeat_guard_cancels_on_drop() {
        let guard = RepositoryCommitHookPendingHeartbeat::new(CancellationToken::new());
        let child = guard.shutdown.child_token();

        drop(guard);

        assert!(
            child.is_cancelled(),
            "dropping the pending heartbeat guard must cancel recovery-blocking heartbeats"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn claim_heartbeat_is_owner_scoped() {
        assert!(
            HOOK_EXTEND_CLAIM_SQL.contains("claimed_at = NOW()"),
            "heartbeat must extend the stale-recovery lease"
        );
        assert!(
            HOOK_EXTEND_CLAIM_SQL.contains("claimed_by = $2"),
            "heartbeat must only extend this worker's claim"
        );
        assert!(
            HOOK_EXTEND_CLAIM_SQL.contains("status = 'running'"),
            "heartbeat must only touch running rows"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn claim_heartbeat_is_owner_scoped_sqlite() {
        assert!(
            HOOK_EXTEND_CLAIM_SQL.contains("claimed_at = CURRENT_TIMESTAMP"),
            "heartbeat must extend the stale-recovery lease"
        );
        assert!(
            HOOK_EXTEND_CLAIM_SQL.contains("claimed_by = ?"),
            "heartbeat must only extend this worker's claim"
        );
        assert!(
            HOOK_EXTEND_CLAIM_SQL.contains("status = 'running'"),
            "heartbeat must only touch running rows"
        );
    }

    #[test]
    fn missing_hook_table_error_is_detected_for_quiet_polling() {
        let error = diesel::result::Error::QueryBuilderError(
            std::io::Error::other("relation \"autumn_repository_commit_hooks\" does not exist")
                .into(),
        );

        assert!(is_missing_hook_table_error(&error));

        // SQLite reports the missing queue table differently.
        let sqlite_error = diesel::result::Error::QueryBuilderError(
            std::io::Error::other("no such table: autumn_repository_commit_hooks").into(),
        );

        assert!(is_missing_hook_table_error(&sqlite_error));

        // An unrelated error must not be misclassified as a missing table.
        let unrelated = diesel::result::Error::QueryBuilderError(
            std::io::Error::other("connection reset by peer").into(),
        );

        assert!(!is_missing_hook_table_error(&unrelated));
    }
}
