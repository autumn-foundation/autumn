//! Transactional outbox and inbox (issue #3062).
//!
//! Write a message on the connection of your business transaction. It commits
//! or rolls back with your data. After commit, a relay sends it to the handler
//! of its topic. A crash cannot lose a committed message.
//!
//! The relay sends a message **at least once**. A crash after the handler and
//! before the relay marks the message sent causes a second send. Use
//! [`Inbox::seen`] in the consumer to drop the copy.
//!
//! ```rust,ignore
//! use autumn_web::outbox::{Inbox, Outbox};
//!
//! db.tx(|conn| Box::pin(async move {
//!     insert_order(conn, &order).await?;
//!     outbox.write(conn, &order.id, "order.placed", &order).await?;
//!     Ok::<_, AutumnError>(())
//! })).await?;
//!
//! // Consumer, registered with `AppBuilder::outbox_handler("order.placed", ..)`.
//! async fn on_order_placed(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
//!     let mut conn = state.pool().expect("database").get().await?;
//!     autumn_web::db::scoped_transaction(&mut *conn, |conn| async move {
//!         if Inbox::new("ledger").seen(conn, &message.id).await? {
//!             return Ok(()); // a copy: already done
//!         }
//!         post_to_ledger(conn, &message).await
//!     }.scope_boxed()).await
//! }
//! ```
//!
//! See `docs/guide/outbox.md`.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};
use futures::FutureExt as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::OutboxConfig;
use crate::db::RuntimeConnection;
use crate::entropy::Entropy;
use crate::events::Event;
use crate::time::ClockSource;
use crate::{AppState, AutumnError, AutumnResult};

/// Topic of a durable event listener call. See [`Outbox::publish`].
pub const TOPIC_EVENT: &str = "autumn.event";
/// Topic of a job enqueue. See [`Outbox::enqueue_job`].
pub const TOPIC_JOB: &str = "autumn.job";
/// Topic of a mail delivery. See [`Outbox::deliver_mail`].
#[cfg(feature = "mail")]
pub const TOPIC_MAIL: &str = "autumn.mail";
/// Topic of a webhook dispatch. See [`Outbox::dispatch_webhook`].
#[cfg(feature = "http-client")]
pub const TOPIC_WEBHOOK: &str = "autumn.webhook";

/// Topics that start with this prefix belong to the framework.
pub const RESERVED_TOPIC_PREFIX: &str = "autumn.";

/// How often the relay worker deletes old rows.
const PURGE_INTERVAL: Duration = Duration::from_secs(60);

// ── Schema ───────────────────────────────────────────────────────────────────

/// DDL of the outbox and inbox tables (Postgres).
///
/// The relay applies it at boot when `outbox.enabled = true`. If the database
/// role of the app cannot run `CREATE TABLE`, apply it before the deploy.
/// Times are epoch milliseconds from the app clock.
#[cfg(not(feature = "sqlite"))]
pub const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS autumn_outbox (
    seq           BIGSERIAL PRIMARY KEY,
    id            TEXT      NOT NULL UNIQUE,
    aggregate     TEXT      NOT NULL,
    topic         TEXT      NOT NULL,
    payload       TEXT      NOT NULL,
    created_at    BIGINT    NOT NULL,
    available_at  BIGINT    NOT NULL,
    attempts      INTEGER   NOT NULL DEFAULT 0,
    last_error    TEXT,
    claim_token   TEXT,
    locked_until  BIGINT,
    dispatched_at BIGINT,
    dead_at       BIGINT
);
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_pending
    ON autumn_outbox (aggregate, seq)
    WHERE dispatched_at IS NULL AND dead_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_dispatched
    ON autumn_outbox (dispatched_at)
    WHERE dispatched_at IS NOT NULL;
CREATE TABLE IF NOT EXISTS autumn_inbox (
    consumer   TEXT   NOT NULL,
    message_id TEXT   NOT NULL,
    seen_at    BIGINT NOT NULL,
    PRIMARY KEY (consumer, message_id)
);
CREATE INDEX IF NOT EXISTS idx_autumn_inbox_seen_at ON autumn_inbox (seen_at);";

/// DDL of the outbox and inbox tables (`SQLite`). See the Postgres variant.
#[cfg(feature = "sqlite")]
pub const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS autumn_outbox (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT    NOT NULL UNIQUE,
    aggregate     TEXT    NOT NULL,
    topic         TEXT    NOT NULL,
    payload       TEXT    NOT NULL,
    created_at    BIGINT  NOT NULL,
    available_at  BIGINT  NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT,
    claim_token   TEXT,
    locked_until  BIGINT,
    dispatched_at BIGINT,
    dead_at       BIGINT
);
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_pending
    ON autumn_outbox (aggregate, seq)
    WHERE dispatched_at IS NULL AND dead_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_dispatched
    ON autumn_outbox (dispatched_at)
    WHERE dispatched_at IS NOT NULL;
CREATE TABLE IF NOT EXISTS autumn_inbox (
    consumer   TEXT   NOT NULL,
    message_id TEXT   NOT NULL,
    seen_at    BIGINT NOT NULL,
    PRIMARY KEY (consumer, message_id)
);
CREATE INDEX IF NOT EXISTS idx_autumn_inbox_seen_at ON autumn_inbox (seen_at);";

/// Advisory lock key that serialises the boot DDL across replicas.
#[cfg(not(feature = "sqlite"))]
const SCHEMA_LOCK_KEY: i64 = 0x0A17_0B0C_0000_3062;

/// Create the outbox and inbox tables if they do not exist.
///
/// On Postgres, it sends no DDL when both tables exist. Thus a role that
/// cannot create or own tables can use tables made by a migration. An
/// advisory lock serializes replicas that create the tables together.
///
/// # Errors
///
/// Returns an error when no connection is available or the DDL fails.
pub async fn ensure_schema(pool: &Pool<RuntimeConnection>) -> AutumnResult<()> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox schema: no connection: {error}"))
    })?;
    crate::backend_select! {
        pg => {
            if tables_exist(&mut conn, &["autumn_outbox", "autumn_inbox"]).await? {
                return Ok(());
            }
            conn.batch_execute(&format!(
                "SELECT pg_advisory_xact_lock({SCHEMA_LOCK_KEY});\n{SCHEMA_SQL}"
            ))
            .await
            .map_err(|error| sql_error("outbox schema", &error))
        },
        sqlite => {
            conn.batch_execute(SCHEMA_SQL)
                .await
                .map_err(|error| sql_error("outbox schema", &error))
        },
    }
}

/// `true` when every table in `tables` exists (Postgres).
#[cfg(not(feature = "sqlite"))]
pub(crate) async fn tables_exist(
    conn: &mut RuntimeConnection,
    tables: &[&str],
) -> AutumnResult<bool> {
    #[derive(diesel::QueryableByName)]
    struct Present {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        present: bool,
    }
    for table in tables {
        let row = diesel::sql_query("SELECT to_regclass($1) IS NOT NULL AS present")
            .bind::<Text, _>(*table)
            .get_result::<Present>(&mut *conn)
            .await
            .map_err(|error| sql_error("schema probe", &error))?;
        if !row.present {
            return Ok(false);
        }
    }
    Ok(true)
}

// ── SQL ──────────────────────────────────────────────────────────────────────
//
// Statements are written once with Postgres `$n` placeholders, in bind order.
// `sql()` turns them into `SQLite` `?` placeholders and drops the row-lock
// clause. `SQLite` has one writer, so one `UPDATE ... RETURNING` claim is
// atomic there.

const INSERT_SQL: &str = "INSERT INTO autumn_outbox \
     (id, aggregate, topic, payload, created_at, available_at) \
     VALUES ($1, $2, $3, $4, $5, $6)";

const MARK_SENT_SQL: &str = "UPDATE autumn_outbox \
     SET dispatched_at = $1, claim_token = NULL, locked_until = NULL, last_error = NULL \
     WHERE seq = $2 AND claim_token = $3";

const NACK_SQL: &str = "UPDATE autumn_outbox \
     SET last_error = $1, available_at = $2, dead_at = $3, \
         claim_token = NULL, locked_until = NULL \
     WHERE seq = $4 AND claim_token = $5";

/// Dead-letter a message whose attempts crashes used. The claim that found
/// it counted no attempt, so the count goes back to the limit.
const EXHAUSTED_SQL: &str = "UPDATE autumn_outbox \
     SET attempts = $1, last_error = $2, dead_at = $3, \
         claim_token = NULL, locked_until = NULL \
     WHERE seq = $4 AND claim_token = $5";

/// Give back the rows of a claim that the relay did not handle. The claim
/// counted an attempt; this takes it back. The caller adds the `seq IN`
/// list of the unhandled rows, so a handled row keeps its attempt.
const RELEASE_SQL: &str = "UPDATE autumn_outbox \
     SET claim_token = NULL, locked_until = NULL, attempts = attempts - 1 \
     WHERE claim_token = $1 AND dispatched_at IS NULL AND dead_at IS NULL \
       AND seq IN";

const PURGE_OUTBOX_SQL: &str = "DELETE FROM autumn_outbox \
     WHERE dispatched_at IS NOT NULL AND dispatched_at < $1";

const PURGE_INBOX_SQL: &str = "DELETE FROM autumn_inbox WHERE seen_at < $1";

const INBOX_INSERT_SQL: &str = "INSERT INTO autumn_inbox (consumer, message_id, seen_at) \
     VALUES ($1, $2, $3) ON CONFLICT DO NOTHING";

const DEAD_LETTERS_SQL: &str = "SELECT seq, id, aggregate, topic, payload, attempts, \
     created_at, last_error, dead_at FROM autumn_outbox \
     WHERE dead_at IS NOT NULL ORDER BY seq LIMIT $1";

const REQUEUE_SQL: &str = "UPDATE autumn_outbox \
     SET dead_at = NULL, attempts = 0, last_error = NULL, available_at = $1 \
     WHERE id = $2 AND dead_at IS NOT NULL AND dispatched_at IS NULL";

/// Claim the oldest ready messages, and count one attempt for each.
///
/// A message is ready when its aggregate has no older pending message. Thus
/// the relay sends the messages of one aggregate in order. A crash or an
/// expired lease also uses an attempt, so a message that kills the relay
/// goes to the dead letters.
fn claim_sql(topics_in_list: &str) -> String {
    let lock = crate::backend_select! {
        pg => { " FOR UPDATE SKIP LOCKED" },
        sqlite => { "" },
    };
    // Convert the placeholders before the topic list goes in.
    sql(&format!(
        "UPDATE autumn_outbox SET claim_token = $1, locked_until = $2, attempts = attempts + 1 \
         WHERE seq IN ( \
           SELECT o.seq FROM autumn_outbox o \
           WHERE o.dispatched_at IS NULL AND o.dead_at IS NULL \
             AND o.available_at <= $3 \
             AND (o.locked_until IS NULL OR o.locked_until <= $4) \
             AND o.topic IN (__TOPICS__) \
             AND NOT EXISTS ( \
               SELECT 1 FROM autumn_outbox p \
               WHERE p.aggregate = o.aggregate AND p.seq < o.seq \
                 AND p.dispatched_at IS NULL AND p.dead_at IS NULL) \
           ORDER BY o.seq \
           LIMIT $5{lock}) \
         RETURNING seq, id, aggregate, topic, payload, attempts, created_at"
    ))
    .replace("__TOPICS__", topics_in_list)
}

/// Convert a Postgres statement for the compiled backend.
fn sql(pg: &str) -> String {
    crate::backend_select! {
        pg => { pg.to_owned() },
        sqlite => { to_sqlite_placeholders(pg) },
    }
}

/// Replace each `$n` placeholder with `?`. Binds must follow text order.
#[cfg_attr(not(any(feature = "sqlite", test)), allow(dead_code))]
pub(crate) fn to_sqlite_placeholders(pg: &str) -> String {
    let mut out = String::with_capacity(pg.len());
    let mut chars = pg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek().is_some_and(char::is_ascii_digit) {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            out.push('?');
        } else {
            out.push(c);
        }
    }
    out
}

fn sql_error(context: &str, error: &diesel::result::Error) -> AutumnError {
    AutumnError::internal_server_error_msg(format!("{context}: {error}"))
}

const fn to_millis(time: DateTime<Utc>) -> i64 {
    time.timestamp_millis()
}

fn from_millis(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

// ── Messages ─────────────────────────────────────────────────────────────────

/// A message the relay gives to a handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OutboxMessage {
    /// Stable id. It is the same on each send; use it with [`Inbox::seen`].
    pub id: String,
    /// Ordering key. The relay sends the messages of one aggregate in order.
    pub aggregate: String,
    /// Routing key. It selects the handler.
    pub topic: String,
    /// The JSON payload.
    pub payload: Value,
    /// The attempt number, starting at 1. In a [`DeadLetter`], the number of
    /// attempts made.
    pub attempt: u32,
    /// When the message was written (app clock).
    pub created_at: DateTime<Utc>,
}

impl OutboxMessage {
    /// A message at attempt 1. Use it to test a handler without a relay.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        aggregate: impl Into<String>,
        topic: impl Into<String>,
        payload: Value,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: id.into(),
            aggregate: aggregate.into(),
            topic: topic.into(),
            payload,
            attempt: 1,
            created_at,
        }
    }

    /// Deserialize the payload.
    ///
    /// # Errors
    ///
    /// Returns a `400` error when the payload does not match `T`.
    pub fn payload_as<T: DeserializeOwned>(&self) -> AutumnResult<T> {
        // The serde text can quote payload values, so log only its kind.
        serde_json::from_value(self.payload.clone()).map_err(|error| {
            AutumnError::bad_request_msg(format!(
                "outbox message {} ({}): payload does not match ({:?})",
                self.id,
                self.topic,
                error.classify()
            ))
        })
    }
}

/// A message the relay stopped after `outbox.max_attempts` failures.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeadLetter {
    /// The message.
    pub message: OutboxMessage,
    /// The error of the last attempt.
    pub last_error: Option<String>,
    /// When the relay stopped the message (app clock).
    pub dead_at: DateTime<Utc>,
}

#[derive(diesel::QueryableByName)]
struct ClaimedRow {
    #[diesel(sql_type = BigInt)]
    seq: i64,
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    aggregate: String,
    #[diesel(sql_type = Text)]
    topic: String,
    #[diesel(sql_type = Text)]
    payload: String,
    #[diesel(sql_type = Integer)]
    attempts: i32,
    #[diesel(sql_type = BigInt)]
    created_at: i64,
}

#[derive(diesel::QueryableByName)]
struct DeadRow {
    #[diesel(sql_type = BigInt)]
    seq: i64,
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    aggregate: String,
    #[diesel(sql_type = Text)]
    topic: String,
    #[diesel(sql_type = Text)]
    payload: String,
    #[diesel(sql_type = Integer)]
    attempts: i32,
    #[diesel(sql_type = BigInt)]
    created_at: i64,
    #[diesel(sql_type = Nullable<Text>)]
    last_error: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    dead_at: Option<i64>,
}

fn parse_payload(id: &str, raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|error| {
        tracing::warn!(message_id = %id, %error, "outbox payload is not JSON; sending it as a string");
        Value::String(raw.to_owned())
    })
}

impl ClaimedRow {
    fn message(&self) -> OutboxMessage {
        OutboxMessage {
            id: self.id.clone(),
            aggregate: self.aggregate.clone(),
            topic: self.topic.clone(),
            payload: parse_payload(&self.id, &self.payload),
            attempt: u32::try_from(self.attempts).unwrap_or(0).max(1),
            created_at: from_millis(self.created_at),
        }
    }
}

// ── Writer ───────────────────────────────────────────────────────────────────

/// Writes messages into the outbox on the caller's connection.
///
/// Give it the connection of the open transaction. Then the message commits
/// or rolls back with the business data.
///
/// Get one with [`Outbox::new`], or extract it in a handler.
#[derive(Clone)]
pub struct Outbox {
    state: AppState,
}

impl std::fmt::Debug for Outbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outbox").finish_non_exhaustive()
    }
}

impl axum::extract::FromRequestParts<AppState> for Outbox {
    type Rejection = AutumnError;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self::new(state))
    }
}

impl Outbox {
    /// An outbox writer for `state`. It uses the clock and entropy of `state`.
    #[must_use]
    pub fn new(state: &AppState) -> Self {
        Self {
            state: state.clone(),
        }
    }

    /// The installed relay. Without it, nothing sends a message, so a write
    /// is an error.
    fn relay(&self) -> AutumnResult<Arc<OutboxRelay>> {
        self.state.extension::<OutboxRelay>().ok_or_else(|| {
            AutumnError::internal_server_error_msg("the outbox is off; set outbox.enabled = true")
        })
    }

    async fn insert(
        &self,
        conn: &mut RuntimeConnection,
        aggregate: Option<&str>,
        topic: &str,
        body: &Value,
    ) -> AutumnResult<String> {
        self.relay()?;
        insert_message(
            conn,
            self.state.entropy(),
            self.state.clock(),
            aggregate,
            topic,
            body,
        )
        .await
    }

    /// Write one message. Returns its id.
    ///
    /// The relay sends the messages of one `aggregate` in write order. Use
    /// the id of the changed entity, for example `"order:42"`.
    ///
    /// # Errors
    ///
    /// Returns an error when the outbox is off, `topic` has no handler (a
    /// typing error, or a reserved `autumn.` topic), the payload does not
    /// serialize, or the insert fails.
    pub async fn write<T: Serialize + Sync + ?Sized>(
        &self,
        conn: &mut RuntimeConnection,
        aggregate: &str,
        topic: &str,
        payload: &T,
    ) -> AutumnResult<String> {
        if topic.starts_with(RESERVED_TOPIC_PREFIX) || !self.relay()?.handlers.contains_key(topic) {
            return Err(AutumnError::bad_request_msg(format!(
                "outbox topic {topic:?} has no handler; register it with outbox_handler"
            )));
        }
        let payload = to_json(payload, &format!("outbox payload for {topic}"))?;
        self.insert(conn, Some(aggregate), topic, &payload).await
    }

    /// Publish `event` to its durable listeners through the outbox.
    ///
    /// Writes one message per durable listener. After commit, the relay
    /// enqueues the job of each listener, so the listener keeps its own
    /// retry settings. Sync listeners run now, as with
    /// [`Events::publish`](crate::events::Events::publish).
    ///
    /// # Errors
    ///
    /// Returns an error when the outbox is off, the event does not
    /// serialize, or an insert fails. Sync listeners run in all cases.
    pub async fn publish<E: Event>(
        &self,
        conn: &mut RuntimeConnection,
        event: &E,
    ) -> AutumnResult<()> {
        let payload = to_json(event, &format!("event {}", E::NAME))?;
        if let Some(recorder) = self.state.extension::<crate::events::EventRecorder>() {
            recorder.record(E::NAME, payload.clone());
        }
        let Some(registry) = self.state.extension::<crate::events::EventRegistry>() else {
            return Ok(());
        };
        let listeners = registry.listeners_for(E::NAME);
        let mut first_error = None;
        for job_name in listeners
            .iter()
            .filter(|listener| listener.mode == crate::events::DispatchMode::Durable)
            .filter_map(|listener| listener.job_name.as_deref())
        {
            let body = serde_json::json!({ "name": job_name, "args": payload });
            if let Err(error) = self.insert(conn, None, TOPIC_EVENT, &body).await {
                first_error.get_or_insert(error);
            }
        }
        crate::events::run_sync_listeners(&self.state, listeners, &payload).await;
        first_error.map_or(Ok(()), Err)
    }

    /// Enqueue job `name` through the outbox. Returns the message id.
    ///
    /// Use it on the Redis and `SQLite` job backends, where
    /// [`enqueue_in_tx`](crate::job::enqueue_in_tx) cannot join the
    /// transaction. The relay enqueues the job after commit. A crash can
    /// enqueue it twice, so make the job idempotent. After the enqueue, the
    /// job backend holds the job: the `local` backend loses it in a crash.
    ///
    /// # Errors
    ///
    /// Returns an error when `args` does not serialize or the insert fails.
    pub async fn enqueue_job<A: Serialize + Sync + ?Sized>(
        &self,
        conn: &mut RuntimeConnection,
        name: &str,
        args: &A,
    ) -> AutumnResult<String> {
        let args = to_json(args, &format!("job {name} args"))?;
        let body = serde_json::json!({ "name": name, "args": args });
        self.insert(conn, None, TOPIC_JOB, &body).await
    }

    /// Deliver `mail` through the outbox. Returns the message id.
    ///
    /// The relay sends it with the app [`Mailer`](crate::mail::Mailer) after
    /// commit. It applies the mailer defaults now, as `deliver_later` does.
    ///
    /// # Errors
    ///
    /// Returns an error when the insert fails.
    #[cfg(feature = "mail")]
    pub async fn deliver_mail(
        &self,
        conn: &mut RuntimeConnection,
        mail: crate::mail::Mail,
    ) -> AutumnResult<String> {
        let mail = match self.state.extension::<crate::mail::Mailer>() {
            Some(mailer) => mailer.prepare_deferred(mail),
            None => mail,
        };
        let body = to_json(&mail, "mail")?;
        self.insert(conn, None, TOPIC_MAIL, &body).await
    }

    /// Dispatch webhook `topic` through the outbox. Returns the message id.
    ///
    /// After commit, the relay calls
    /// [`WebhookOutboundManager::dispatch`](crate::webhook_outbound::WebhookOutboundManager::dispatch).
    /// Each delivery id comes from the message id. A second relay send skips
    /// a delivery that has a result. Before a result, it can enqueue the
    /// delivery again, with the same `webhook-id`.
    ///
    /// # Errors
    ///
    /// Returns an error when `payload` does not serialize or the insert fails.
    #[cfg(feature = "http-client")]
    pub async fn dispatch_webhook<T: Serialize + Sync + ?Sized>(
        &self,
        conn: &mut RuntimeConnection,
        topic: &str,
        payload: &T,
    ) -> AutumnResult<String> {
        let payload = to_json(payload, &format!("webhook payload for {topic}"))?;
        let body = serde_json::json!({ "topic": topic, "payload": payload });
        self.insert(conn, None, TOPIC_WEBHOOK, &body).await
    }

    /// The dead letters, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn dead_letters(
        &self,
        conn: &mut RuntimeConnection,
        limit: usize,
    ) -> AutumnResult<Vec<DeadLetter>> {
        let rows = diesel::sql_query(sql(DEAD_LETTERS_SQL))
            .bind::<BigInt, _>(i64::try_from(limit).unwrap_or(i64::MAX))
            .load::<DeadRow>(conn)
            .await
            .map_err(|error| sql_error("outbox dead letters", &error))?;
        Ok(rows
            .into_iter()
            .map(|row| DeadLetter {
                message: ClaimedRow {
                    seq: row.seq,
                    id: row.id,
                    aggregate: row.aggregate,
                    topic: row.topic,
                    payload: row.payload,
                    attempts: row.attempts,
                    created_at: row.created_at,
                }
                .message(),
                last_error: row.last_error,
                dead_at: from_millis(row.dead_at.unwrap_or_default()),
            })
            .collect())
    }

    /// Send dead letter `id` again, from attempt 1. Returns `false` when `id`
    /// is not a dead letter.
    ///
    /// Later messages of the same aggregate can already be sent, so a
    /// requeued message can arrive out of order.
    ///
    /// # Errors
    ///
    /// Returns an error when the update fails.
    pub async fn requeue(&self, conn: &mut RuntimeConnection, id: &str) -> AutumnResult<bool> {
        let changed = diesel::sql_query(sql(REQUEUE_SQL))
            .bind::<BigInt, _>(to_millis(self.state.clock().now()))
            .bind::<Text, _>(id)
            .execute(conn)
            .await
            .map_err(|error| sql_error("outbox requeue", &error))?;
        Ok(changed > 0)
    }
}

fn to_json<T: Serialize + ?Sized>(value: &T, what: &str) -> AutumnResult<Value> {
    serde_json::to_value(value).map_err(|error| {
        AutumnError::internal_server_error_msg(format!("{what} does not serialize: {error}"))
    })
}

/// Insert one row. With no `aggregate`, the message id is the aggregate, so
/// the message has no order constraint.
async fn insert_message(
    conn: &mut RuntimeConnection,
    entropy: &dyn Entropy,
    clock: &dyn ClockSource,
    aggregate: Option<&str>,
    topic: &str,
    payload: &Value,
) -> AutumnResult<String> {
    let id = entropy.uuid_v4().to_string();
    let now = to_millis(clock.now());
    diesel::sql_query(sql(INSERT_SQL))
        .bind::<Text, _>(&id)
        .bind::<Text, _>(aggregate.unwrap_or(&id))
        .bind::<Text, _>(topic)
        .bind::<Text, _>(payload.to_string())
        .bind::<BigInt, _>(now)
        .bind::<BigInt, _>(now)
        .execute(conn)
        .await
        .map_err(|error| sql_error("outbox write", &error))?;
    Ok(id)
}

// ── Inbox ────────────────────────────────────────────────────────────────────

/// Drops copies of a message for one consumer.
///
/// Call [`seen`](Self::seen) in the same transaction as the side effect of the
/// consumer. Then the record and the effect commit together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbox {
    consumer: String,
}

impl Inbox {
    /// The inbox of `consumer`. Each consumer has its own set of ids.
    #[must_use]
    pub fn new(consumer: impl Into<String>) -> Self {
        Self {
            consumer: consumer.into(),
        }
    }

    /// Record `message_id`. Returns `true` when it was already recorded: the
    /// message is a copy, so skip it.
    ///
    /// # Errors
    ///
    /// Returns an error when the insert fails.
    pub async fn seen(&self, conn: &mut RuntimeConnection, message_id: &str) -> AutumnResult<bool> {
        let inserted = diesel::sql_query(sql(INBOX_INSERT_SQL))
            .bind::<Text, _>(&self.consumer)
            .bind::<Text, _>(message_id)
            .bind::<BigInt, _>(to_millis(crate::time::ambient_now()))
            .execute(conn)
            .await
            .map_err(|error| sql_error("inbox", &error))?;
        Ok(inserted == 0)
    }
}

// ── Relay ────────────────────────────────────────────────────────────────────

type HandlerFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;

/// A topic handler. It gets the app state and the message.
pub type OutboxHandler = Arc<dyn Fn(AppState, OutboxMessage) -> HandlerFuture + Send + Sync>;

/// Topic handlers collected by the app builder.
#[derive(Clone, Default)]
pub(crate) struct OutboxHandlers {
    handlers: HashMap<String, OutboxHandler>,
}

impl std::fmt::Debug for OutboxHandlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxHandlers")
            .field("topics", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// `true` when `topic` uses only `A-Z a-z 0-9 . _ : -`. The relay puts
/// topics in its SQL text, so no other character is allowed.
fn is_valid_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

impl OutboxHandlers {
    /// Set the handler of `topic`. A second call for one topic replaces it.
    ///
    /// # Panics
    ///
    /// Panics when `topic` is empty, has a character other than
    /// `A-Z a-z 0-9 . _ : -`, or starts with `autumn.` (reserved).
    pub(crate) fn insert<F, Fut>(&mut self, topic: impl Into<String>, handler: F)
    where
        F: Fn(AppState, OutboxMessage) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AutumnResult<()>> + Send + 'static,
    {
        let topic = topic.into();
        assert!(
            is_valid_topic(&topic),
            "outbox topic {topic:?} must not be empty and must use only A-Z a-z 0-9 . _ : -"
        );
        assert!(
            !topic.starts_with(RESERVED_TOPIC_PREFIX),
            "outbox topic {topic:?}: the `{RESERVED_TOPIC_PREFIX}` prefix is reserved"
        );
        self.insert_unchecked(topic, handler);
    }

    fn insert_unchecked<F, Fut>(&mut self, topic: String, handler: F)
    where
        F: Fn(AppState, OutboxMessage) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AutumnResult<()>> + Send + 'static,
    {
        let handler: OutboxHandler =
            Arc::new(move |state, message| Box::pin(handler(state, message)));
        self.handlers.insert(topic, handler);
    }

    /// Add every handler of `other`. A handler of `other` replaces one of
    /// `self` for the same topic.
    pub(crate) fn extend(&mut self, other: Self) {
        self.handlers.extend(other.handlers);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// The relay of one app: config and topic handlers. The app installs it on
/// [`AppState`] when `outbox.enabled = true`.
#[derive(Clone)]
pub struct OutboxRelay {
    config: OutboxConfig,
    handlers: Arc<HashMap<String, OutboxHandler>>,
    topics_in_list: Arc<str>,
    /// Indexes (in `relay_pools` order) of pools with no tables yet: the boot
    /// could not reach them. The relay tries again before each drain.
    schema_pending: Arc<std::sync::Mutex<std::collections::HashSet<usize>>>,
}

impl std::fmt::Debug for OutboxRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxRelay")
            .field("config", &self.config)
            .field("topics", &self.topics_in_list)
            .finish_non_exhaustive()
    }
}

impl OutboxRelay {
    /// A relay with the built-in topics and `handlers`.
    pub(crate) fn new(config: OutboxConfig, handlers: OutboxHandlers) -> Self {
        let mut all = handlers;
        all.insert_unchecked(TOPIC_EVENT.to_owned(), run_job_enqueue);
        all.insert_unchecked(TOPIC_JOB.to_owned(), run_job_enqueue);
        #[cfg(feature = "mail")]
        all.insert_unchecked(TOPIC_MAIL.to_owned(), run_mail_delivery);
        #[cfg(feature = "http-client")]
        all.insert_unchecked(TOPIC_WEBHOOK.to_owned(), run_webhook_dispatch);

        let mut topics: Vec<&String> = all.handlers.keys().collect();
        topics.sort();
        // `is_valid_topic` keeps quotes out; doubling them is a second guard.
        let topics_in_list = topics
            .iter()
            .map(|topic| format!("'{}'", topic.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            config,
            handlers: Arc::new(all.handlers),
            topics_in_list: topics_in_list.into(),
            schema_pending: Arc::default(),
        }
    }

    fn pending_schema(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<usize>> {
        self.schema_pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Create the tables on pool `index` if the boot could not.
    async fn ensure_pending_schema(
        &self,
        index: usize,
        pool: &Pool<RuntimeConnection>,
    ) -> AutumnResult<()> {
        if !self.pending_schema().contains(&index) {
            return Ok(());
        }
        ensure_schema(pool).await?;
        self.pending_schema().remove(&index);
        Ok(())
    }

    /// The relay settings.
    #[must_use]
    pub const fn config(&self) -> &OutboxConfig {
        &self.config
    }

    /// `true` when the relay has a handler for `topic`.
    #[must_use]
    pub fn handles(&self, topic: &str) -> bool {
        self.handlers.contains_key(topic)
    }
}

/// Install the relay on `state` when `config.enabled`. With the `mail`
/// feature, also send `deliver_later` through the outbox, unless the app
/// set its own queue.
pub(crate) fn install(state: &AppState, config: &OutboxConfig, handlers: OutboxHandlers) {
    if !config.enabled {
        if !handlers.is_empty() {
            tracing::warn!(
                "outbox handlers are registered but outbox.enabled = false; the relay does not run"
            );
        }
        return;
    }
    let Some(pool) = mail_pool(state) else {
        tracing::warn!("outbox.enabled = true needs a database; the relay does not run");
        return;
    };
    state.insert_extension(OutboxRelay::new(config.clone(), handlers));
    #[cfg(feature = "mail")]
    if state
        .extension::<crate::mail::MailDeliveryQueueHandle>()
        .is_none()
    {
        state.insert_extension(crate::mail::MailDeliveryQueueHandle::new(OutboxMailQueue {
            pool,
            entropy: state.entropy_arc(),
            clock: state.clock_arc(),
        }));
    }
    #[cfg(not(feature = "mail"))]
    drop(pool);
}

tokio::task_local! {
    static CURRENT_MESSAGE_ID: String;
}

/// The id of the message the relay is handling on this task, if any.
///
/// Use it in code that the handler calls and that does not get the
/// [`OutboxMessage`], for example with [`Inbox::seen`].
#[must_use]
pub fn current_message_id() -> Option<String> {
    CURRENT_MESSAGE_ID.try_with(Clone::clone).ok()
}

/// Send up to `max` ready messages from each relay pool (the app pool and
/// each shard) now. Returns how many the relay handled (sent or failed).
///
/// The relay worker calls this in a loop. Tests and `Sim::run_to_idle` call
/// it to drain the outbox without a worker.
///
/// # Errors
///
/// Returns an error when the relay is not installed (`outbox.enabled =
/// false`), the app has no database, or a claim fails.
pub async fn drain(state: &AppState, max: usize) -> AutumnResult<usize> {
    drain_until(state, max, None).await
}

/// [`drain`] that stops between messages when `shutdown` is cancelled.
async fn drain_until(
    state: &AppState,
    max: usize,
    shutdown: Option<&CancellationToken>,
) -> AutumnResult<usize> {
    let relay = state.extension::<OutboxRelay>().ok_or_else(|| {
        AutumnError::internal_server_error_msg(
            "outbox relay is not installed; set outbox.enabled = true",
        )
    })?;
    // Each pool gets its own budget, so a busy app pool cannot starve a
    // shard.
    // A failed pool (a shard that is down) does not stop the others. The
    // drain fails only when every pool fails.
    let pools = relay_pools(state)?;
    let mut handled = 0;
    let mut failures = Vec::new();
    for (index, pool) in pools.iter().enumerate() {
        let drained = match relay.ensure_pending_schema(index, pool).await {
            Ok(()) => drain_pool(state, &relay, pool, max, shutdown).await,
            Err(error) => Err(error),
        };
        match drained {
            Ok(count) => handled += count,
            Err(error) => {
                tracing::warn!(%error, "outbox relay could not drain one pool; it goes on with the others");
                failures.push(error);
            }
        }
    }
    fail_if_every_pool_failed(failures, pools.len())?;
    Ok(handled)
}

/// The pool `deliver_later` writes on. Mail has no shard key: it is the app
/// pool, or the first shard when there is no app pool.
fn mail_pool(state: &AppState) -> Option<Pool<RuntimeConnection>> {
    relay_pools(state).ok()?.into_iter().next()
}

/// The pools the relay drains: the app pool, if any, and the primary pool of
/// each shard. A write on a shard connection puts its row in that shard.
fn relay_pools(state: &AppState) -> AutumnResult<Vec<Pool<RuntimeConnection>>> {
    let mut pools: Vec<Pool<RuntimeConnection>> = state.pool().cloned().into_iter().collect();
    if let Some(shards) = state.shards() {
        pools.extend(shards.iter().map(|shard| shard.primary_pool().clone()));
    }
    if pools.is_empty() {
        return Err(AutumnError::internal_server_error_msg(
            "outbox relay needs a database",
        ));
    }
    Ok(pools)
}

/// Create the outbox tables on every pool the relay drains. A pool that
/// fails (a shard that is down) does not stop the boot: the relay tries
/// again before it drains that pool.
///
/// # Errors
///
/// Returns an error when every pool fails.
pub(crate) async fn ensure_relay_schema(state: &AppState) -> AutumnResult<()> {
    let relay = state.extension::<OutboxRelay>();
    let pools = relay_pools(state)?;
    let mut failures = Vec::new();
    for (index, pool) in pools.iter().enumerate() {
        if let Err(error) = ensure_schema(pool).await {
            tracing::warn!(%error, "outbox could not create its tables on one pool; the relay tries again later");
            if let Some(relay) = &relay {
                relay.pending_schema().insert(index);
            }
            failures.push(error);
        }
    }
    fail_if_every_pool_failed(failures, pools.len())
}

/// A multi-pool step fails only when every pool fails. Then it returns the
/// first error.
fn fail_if_every_pool_failed(failures: Vec<AutumnError>, pools: usize) -> AutumnResult<()> {
    if failures.len() == pools
        && let Some(error) = failures.into_iter().next()
    {
        return Err(error);
    }
    Ok(())
}

async fn drain_pool(
    state: &AppState,
    relay: &OutboxRelay,
    pool: &Pool<RuntimeConnection>,
    max: usize,
    shutdown: Option<&CancellationToken>,
) -> AutumnResult<usize> {
    let lease = duration_millis(Duration::from_millis(relay.config.lease_ms));
    let mut handled = 0;
    while handled < max {
        let batch = (max - handled).min(relay.config.batch_size.max(1));
        let claim_token = state.entropy().uuid_v4().to_string();
        let lease_end = to_millis(state.clock().now()).saturating_add(lease);
        let mut rows = claim(relay, pool, state, &claim_token, batch).await?;
        if rows.is_empty() {
            break;
        }
        rows.sort_by_key(|row| row.seq);
        let total = rows.len();
        let mut done = 0;
        for row in &rows {
            // After the lease, another relay can own the rest of the batch.
            // On shutdown, the next process sends it.
            let remaining = lease_end.saturating_sub(to_millis(state.clock().now()));
            if remaining <= 0 || shutdown.is_some_and(CancellationToken::is_cancelled) {
                break;
            }
            let budget = Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
            handle_row(relay, pool, state, &claim_token, row, budget).await;
            done += 1;
        }
        handled += done;
        if done < total {
            let unhandled: Vec<i64> = rows[done..].iter().map(|row| row.seq).collect();
            release(pool, &claim_token, &unhandled).await;
            break;
        }
    }
    Ok(handled)
}

async fn claim(
    relay: &OutboxRelay,
    pool: &Pool<RuntimeConnection>,
    state: &AppState,
    claim_token: &str,
    batch: usize,
) -> AutumnResult<Vec<ClaimedRow>> {
    let now = to_millis(state.clock().now());
    let lease = duration_millis(Duration::from_millis(relay.config.lease_ms));
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox claim: no connection: {error}"))
    })?;
    diesel::sql_query(claim_sql(&relay.topics_in_list))
        .bind::<Text, _>(claim_token)
        .bind::<BigInt, _>(now.saturating_add(lease))
        .bind::<BigInt, _>(now)
        .bind::<BigInt, _>(now)
        .bind::<BigInt, _>(i64::try_from(batch).unwrap_or(i64::MAX))
        .load::<ClaimedRow>(&mut conn)
        .await
        .map_err(|error| sql_error("outbox claim", &error))
}

/// Give back the unhandled rows `seqs` of a claim, so a relay can claim them
/// at once. On failure, they come back when the lease ends.
async fn release(pool: &Pool<RuntimeConnection>, claim_token: &str, seqs: &[i64]) {
    if seqs.is_empty() {
        return;
    }
    // `seq` values are integers from the claim, so the list is safe SQL text.
    let list = seqs
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let statement = format!("{} ({list})", sql(RELEASE_SQL));
    let released = match pool.get().await {
        Ok(mut conn) => diesel::sql_query(statement)
            .bind::<Text, _>(claim_token)
            .execute(&mut conn)
            .await
            .map_err(|error| sql_error("outbox release", &error)),
        Err(error) => Err(AutumnError::service_unavailable_msg(format!(
            "outbox release: no connection: {error}"
        ))),
    };
    if let Err(error) = released {
        tracing::warn!(%error, "outbox relay could not release its claim; the lease ends it");
    }
}

async fn handle_row(
    relay: &OutboxRelay,
    pool: &Pool<RuntimeConnection>,
    state: &AppState,
    claim_token: &str,
    row: &ClaimedRow,
    budget: Duration,
) {
    let message = row.message();
    if message.attempt > relay.config.max_attempts {
        // Claims that a crash or an expired lease ended used the attempts.
        // This claim is not an attempt: keep the count at the limit.
        let recorded = dead_letter_exhausted(relay, pool, state, claim_token, row.seq).await;
        tracing::error!(
            message_id = %message.id,
            topic = %message.topic,
            "outbox message used its attempts in crashes or expired leases; moved to dead letters"
        );
        if let Err(error) = recorded {
            tracing::warn!(message_id = %message.id, %error, "outbox relay could not record the dead letter");
        }
        return;
    }
    let outcome = run_handler(relay, state, &message, budget).await;
    let recorded = match outcome {
        Ok(()) => mark_sent(pool, state, claim_token, row.seq).await,
        Err(error) => {
            let attempt = message.attempt;
            let dead = attempt >= relay.config.max_attempts;
            if dead {
                tracing::error!(
                    message_id = %message.id,
                    topic = %message.topic,
                    attempt,
                    %error,
                    "outbox message failed its last attempt; moved to dead letters"
                );
            } else {
                tracing::warn!(
                    message_id = %message.id,
                    topic = %message.topic,
                    attempt,
                    %error,
                    "outbox message failed; the relay sends it again"
                );
            }
            nack(
                relay,
                pool,
                state,
                claim_token,
                row.seq,
                attempt,
                dead,
                &error,
            )
            .await
        }
    };
    if let Err(error) = recorded {
        tracing::warn!(
            message_id = %message.id,
            %error,
            "outbox relay could not record the outcome; the message is sent again after the lease"
        );
    }
}

/// Run the handler of `message`. A panic, or a handler that runs past the
/// lease (`budget`), is a failure.
async fn run_handler(
    relay: &OutboxRelay,
    state: &AppState,
    message: &OutboxMessage,
    budget: Duration,
) -> AutumnResult<()> {
    let Some(handler) = relay.handlers.get(&message.topic) else {
        return Err(AutumnError::internal_server_error_msg(format!(
            "no outbox handler for topic {}",
            message.topic
        )));
    };
    // A handler can panic when it makes its future, before the first poll.
    let Ok(future) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handler(state.clone(), message.clone())
    })) else {
        return Err(AutumnError::internal_server_error_msg(
            "outbox handler panicked",
        ));
    };
    let future = std::panic::AssertUnwindSafe(future).catch_unwind();
    let run = CURRENT_MESSAGE_ID.scope(message.id.clone(), future);
    match tokio::time::timeout(budget, run).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(AutumnError::internal_server_error_msg(
            "outbox handler panicked",
        )),
        Err(_) => Err(AutumnError::internal_server_error_msg(
            "outbox handler ran past the lease",
        )),
    }
}

async fn mark_sent(
    pool: &Pool<RuntimeConnection>,
    state: &AppState,
    claim_token: &str,
    seq: i64,
) -> AutumnResult<()> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox mark: no connection: {error}"))
    })?;
    diesel::sql_query(sql(MARK_SENT_SQL))
        .bind::<BigInt, _>(to_millis(state.clock().now()))
        .bind::<BigInt, _>(seq)
        .bind::<Text, _>(claim_token)
        .execute(&mut conn)
        .await
        .map_err(|error| sql_error("outbox mark", &error))?;
    Ok(())
}

async fn dead_letter_exhausted(
    relay: &OutboxRelay,
    pool: &Pool<RuntimeConnection>,
    state: &AppState,
    claim_token: &str,
    seq: i64,
) -> AutumnResult<()> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox dead letter: no connection: {error}"))
    })?;
    diesel::sql_query(sql(EXHAUSTED_SQL))
        .bind::<Integer, _>(i32::try_from(relay.config.max_attempts).unwrap_or(i32::MAX))
        .bind::<Text, _>("no attempts left: earlier attempts ended in a crash or an expired lease")
        .bind::<BigInt, _>(to_millis(state.clock().now()))
        .bind::<BigInt, _>(seq)
        .bind::<Text, _>(claim_token)
        .execute(&mut conn)
        .await
        .map_err(|error| sql_error("outbox dead letter", &error))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn nack(
    relay: &OutboxRelay,
    pool: &Pool<RuntimeConnection>,
    state: &AppState,
    claim_token: &str,
    seq: i64,
    attempt: u32,
    dead: bool,
    error: &AutumnError,
) -> AutumnResult<()> {
    let now = to_millis(state.clock().now());
    let delay = retry_delay_ms(&relay.config, attempt, state.entropy());
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox nack: no connection: {error}"))
    })?;
    diesel::sql_query(sql(NACK_SQL))
        .bind::<Text, _>(error.to_string())
        .bind::<BigInt, _>(now.saturating_add(i64::try_from(delay).unwrap_or(i64::MAX)))
        .bind::<Nullable<BigInt>, _>(dead.then_some(now))
        .bind::<BigInt, _>(seq)
        .bind::<Text, _>(claim_token)
        .execute(&mut conn)
        .await
        .map_err(|error| sql_error("outbox nack", &error))?;
    Ok(())
}

/// Delay before attempt `attempt + 1`: exponential, capped, with equal
/// jitter (a random value in `[delay / 2, delay]`).
fn retry_delay_ms(config: &OutboxConfig, attempt: u32, entropy: &dyn Entropy) -> u64 {
    let exponent = attempt.saturating_sub(1).min(32);
    let delay = config
        .initial_backoff_ms
        .saturating_mul(1_u64 << exponent)
        .min(config.max_backoff_ms);
    let half = delay / 2;
    half + entropy.next_u64() % (delay - half + 1)
}

/// Delete sent messages and inbox entries older than `outbox.retention_ms`.
/// Returns the deleted (outbox, inbox) row counts. Dead letters stay.
///
/// The outbox cutoff uses the app clock. The inbox cutoff uses the ambient
/// clock, as [`Inbox::seen`] does.
///
/// # Errors
///
/// Returns an error when the relay is not installed or a delete fails.
pub async fn purge(state: &AppState) -> AutumnResult<(usize, usize)> {
    let relay = state
        .extension::<OutboxRelay>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("outbox relay is not installed"))?;
    let retention = duration_millis(Duration::from_millis(relay.config.retention_ms));
    let outbox_cutoff = to_millis(state.clock().now()).saturating_sub(retention);
    let inbox_cutoff = to_millis(crate::time::ambient_now()).saturating_sub(retention);
    let pools = relay_pools(state)?;
    let (mut outbox, mut inbox) = (0, 0);
    let mut failures = Vec::new();
    for pool in &pools {
        match purge_pool(pool, outbox_cutoff, inbox_cutoff).await {
            Ok((o, i)) => {
                outbox += o;
                inbox += i;
            }
            Err(error) => {
                tracing::warn!(%error, "outbox purge failed on one pool; it goes on with the others");
                failures.push(error);
            }
        }
    }
    fail_if_every_pool_failed(failures, pools.len())?;
    Ok((outbox, inbox))
}

async fn purge_pool(
    pool: &Pool<RuntimeConnection>,
    outbox_cutoff: i64,
    inbox_cutoff: i64,
) -> AutumnResult<(usize, usize)> {
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox purge: no connection: {error}"))
    })?;
    let outbox = diesel::sql_query(sql(PURGE_OUTBOX_SQL))
        .bind::<BigInt, _>(outbox_cutoff)
        .execute(&mut conn)
        .await
        .map_err(|error| sql_error("outbox purge", &error))?;
    let inbox = diesel::sql_query(sql(PURGE_INBOX_SQL))
        .bind::<BigInt, _>(inbox_cutoff)
        .execute(&mut conn)
        .await
        .map_err(|error| sql_error("inbox purge", &error))?;
    Ok((outbox, inbox))
}

/// Run the relay until `shutdown`. It polls every `outbox.poll_interval_ms`,
/// and drains again at once after a full batch. On shutdown it stops between
/// messages and gives back the rest of its claim.
pub(crate) fn start_relay_worker(
    state: AppState,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let relay = state.extension::<OutboxRelay>()?;
    let poll = Duration::from_millis(relay.config.poll_interval_ms.max(1));
    let batch = relay.config.batch_size.max(1);
    Some(tokio::spawn(async move {
        let mut last_purge: Option<crate::time::MonotonicInstant> = None;
        while !shutdown.is_cancelled() {
            let full = match drain_until(&state, batch, Some(&shutdown)).await {
                Ok(handled) => handled >= batch,
                Err(error) => {
                    tracing::warn!(%error, "outbox relay poll failed");
                    false
                }
            };
            let now = state.monotonic();
            if last_purge.is_none_or(|at| now.saturating_duration_since(at) >= PURGE_INTERVAL) {
                last_purge = Some(now);
                if let Err(error) = purge(&state).await {
                    tracing::warn!(%error, "outbox purge failed");
                }
            }
            if full {
                continue;
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(poll) => {}
            }
        }
    }))
}

// ── Built-in topic handlers ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct JobBody {
    name: String,
    args: Value,
}

async fn run_job_enqueue(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let body: JobBody = message.payload_as()?;
    match state.extension::<crate::job::JobClient>() {
        Some(client) => client.enqueue(&body.name, body.args).await,
        None => crate::job::enqueue(&body.name, body.args).await,
    }
}

#[cfg(feature = "mail")]
async fn run_mail_delivery(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let mail: crate::mail::Mail = message.payload_as()?;
    let mailer = state.extension::<crate::mail::Mailer>().ok_or_else(|| {
        AutumnError::internal_server_error_msg("outbox mail: no mailer installed")
    })?;
    mailer
        .send(mail)
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(format!("outbox mail: {error}")))
}

#[cfg(feature = "http-client")]
#[derive(Deserialize)]
struct WebhookBody {
    topic: String,
    payload: Value,
}

#[cfg(feature = "http-client")]
async fn run_webhook_dispatch(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let body: WebhookBody = message.payload_as()?;
    let manager = state
        .extension::<crate::webhook_outbound::WebhookOutboundManager>()
        .ok_or_else(|| {
            AutumnError::internal_server_error_msg("outbox webhook: no webhook manager installed")
        })?;
    manager
        .dispatch_for_message(&state, &message.id, &body.topic, &body.payload)
        .await
}

// ── Mail queue ───────────────────────────────────────────────────────────────

/// A [`MailDeliveryQueue`](crate::mail::MailDeliveryQueue) that writes each
/// mail to the outbox.
///
/// Installed when `outbox.enabled = true` and the app sets no queue. The row
/// is written on its own connection, after the commit of the caller. To write
/// it in the transaction, use [`Outbox::deliver_mail`].
#[cfg(feature = "mail")]
pub struct OutboxMailQueue {
    pool: Pool<RuntimeConnection>,
    entropy: Arc<dyn Entropy>,
    clock: Arc<dyn ClockSource>,
}

#[cfg(feature = "mail")]
impl OutboxMailQueue {
    /// A queue that writes to the outbox of `state`.
    ///
    /// # Errors
    ///
    /// Returns an error when the outbox is off (no relay sends the mail) or
    /// `state` has no database.
    pub fn from_state(state: &AppState) -> AutumnResult<Self> {
        if state.extension::<OutboxRelay>().is_none() {
            return Err(AutumnError::internal_server_error_msg(
                "OutboxMailQueue needs the relay; set outbox.enabled = true",
            ));
        }
        let pool = mail_pool(state).ok_or_else(|| {
            AutumnError::internal_server_error_msg("OutboxMailQueue needs a database")
        })?;
        Ok(Self {
            pool,
            entropy: state.entropy_arc(),
            clock: state.clock_arc(),
        })
    }
}

#[cfg(feature = "mail")]
impl crate::mail::MailDeliveryQueue for OutboxMailQueue {
    fn enqueue<'a>(
        &'a self,
        mail: crate::mail::Mail,
    ) -> Pin<Box<dyn Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>> {
        Box::pin(async move {
            let queue_error = crate::mail::MailError::RuntimeUnavailable;
            let body =
                serde_json::to_value(&mail).map_err(|error| queue_error(error.to_string()))?;
            let mut conn = self
                .pool
                .get()
                .await
                .map_err(|error| queue_error(format!("outbox mail: no connection: {error}")))?;
            insert_message(
                &mut conn,
                self.entropy.as_ref(),
                self.clock.as_ref(),
                None,
                TOPIC_MAIL,
                &body,
            )
            .await
            .map(drop)
            .map_err(|error| queue_error(error.to_string()))
        })
    }
}

#[cfg(test)]
mod tests;
