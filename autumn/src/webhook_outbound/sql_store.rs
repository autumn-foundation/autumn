//! A durable [`OutboundWebhookHandler`] on the app database (issue #3062).
//!
//! Subscriptions and delivery logs live in two tables, so they survive a
//! restart and all replicas see them. Postgres and `SQLite`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, TimeZone as _, Utc};
use diesel::OptionalExtension as _;
use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};

use super::{
    OutboundWebhookHandler, WebhookDeliveryLog, WebhookSubscription, WebhookSubscriptionStatus,
};
use crate::db::RuntimeConnection;
use crate::{AutumnError, AutumnResult};

/// The newest DLQ logs that `get_dlq_logs` reads.
const DLQ_READ_LIMIT: usize = 1_000;

/// Consecutive failures that move an active subscription to `Failed`.
/// The in-memory store uses the same limit.
const FAILURE_LIMIT: i32 = 50;

/// DDL of the webhook tables. It is valid on Postgres and on `SQLite`.
///
/// [`SqlOutboundWebhookStore::ensure_schema`] applies it. If the database
/// role of the app cannot run `CREATE TABLE`, apply it before the deploy.
///
/// `secret` holds the signing secret of each subscription. Limit read access
/// to this table.
pub const WEBHOOK_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS autumn_webhook_subscriptions (
    id                   TEXT    PRIMARY KEY,
    target_url           TEXT    NOT NULL,
    event_topics         TEXT    NOT NULL,
    secret               TEXT    NOT NULL,
    status               TEXT    NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS autumn_webhook_deliveries (
    id              TEXT    PRIMARY KEY,
    subscription_id TEXT    NOT NULL,
    topic           TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    request_headers TEXT    NOT NULL,
    response_status INTEGER,
    response_body   TEXT,
    elapsed_ms      BIGINT  NOT NULL,
    attempt         INTEGER NOT NULL,
    max_attempts    INTEGER NOT NULL,
    is_dlq          BOOLEAN NOT NULL,
    last_error      TEXT,
    logged_at       BIGINT  NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_autumn_webhook_deliveries_dlq
    ON autumn_webhook_deliveries (logged_at)
    WHERE is_dlq;";

#[cfg(not(feature = "sqlite"))]
const SCHEMA_LOCK_KEY: i64 = 0x0A17_0B0C_0001_3062;

const UPSERT_SUBSCRIPTION_SQL: &str = "INSERT INTO autumn_webhook_subscriptions \
     (id, target_url, event_topics, secret, status, consecutive_failures) \
     VALUES ($1, $2, $3, $4, $5, $6) \
     ON CONFLICT (id) DO UPDATE SET target_url = excluded.target_url, \
       event_topics = excluded.event_topics, secret = excluded.secret, \
       status = excluded.status, consecutive_failures = excluded.consecutive_failures";

const SUBSCRIPTION_COLUMNS: &str =
    "id, target_url, event_topics, secret, status, consecutive_failures";

const UPSERT_LOG_SQL: &str = "INSERT INTO autumn_webhook_deliveries \
     (id, subscription_id, topic, payload, request_headers, response_status, response_body, \
      elapsed_ms, attempt, max_attempts, is_dlq, last_error, logged_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
     ON CONFLICT (id) DO UPDATE SET subscription_id = excluded.subscription_id, \
       topic = excluded.topic, payload = excluded.payload, \
       request_headers = excluded.request_headers, \
       response_status = excluded.response_status, response_body = excluded.response_body, \
       elapsed_ms = excluded.elapsed_ms, attempt = excluded.attempt, \
       max_attempts = excluded.max_attempts, is_dlq = excluded.is_dlq, \
       last_error = excluded.last_error, logged_at = excluded.logged_at";

const LOG_COLUMNS: &str = "id, subscription_id, topic, payload, request_headers, \
     response_status, response_body, elapsed_ms, attempt, max_attempts, is_dlq, last_error, \
     logged_at";

const RESET_FAILURES_SQL: &str = "UPDATE autumn_webhook_subscriptions \
     SET consecutive_failures = 0 WHERE id = $1";

const SUCCESS_SQL: &str = "UPDATE autumn_webhook_subscriptions \
     SET consecutive_failures = 0 WHERE id = $1 AND status = 'active'";

const FAILURE_SQL: &str = "UPDATE autumn_webhook_subscriptions \
     SET consecutive_failures = consecutive_failures + 1, \
         status = CASE WHEN consecutive_failures + 1 >= $1 THEN 'failed' ELSE status END \
     WHERE id = $2 AND status = 'active'";

const REACTIVATE_SQL: &str = "UPDATE autumn_webhook_subscriptions \
     SET consecutive_failures = 0, \
         status = CASE WHEN status = 'failed' THEN 'active' ELSE status END \
     WHERE id = $1";

fn sql(pg: &str) -> String {
    crate::backend_select! {
        pg => { pg.to_owned() },
        sqlite => { crate::outbox::to_sqlite_placeholders(pg) },
    }
}

fn db_error(context: &str, error: &dyn std::fmt::Display) -> AutumnError {
    AutumnError::internal_server_error_msg(format!("webhook store {context}: {error}"))
}

#[derive(diesel::QueryableByName)]
struct SubscriptionRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    target_url: String,
    #[diesel(sql_type = Text)]
    event_topics: String,
    #[diesel(sql_type = Text)]
    secret: String,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Integer)]
    consecutive_failures: i32,
}

impl SubscriptionRow {
    fn into_subscription(self) -> AutumnResult<WebhookSubscription> {
        Ok(WebhookSubscription {
            event_topics: serde_json::from_str(&self.event_topics)
                .map_err(|error| db_error("event_topics", &error))?,
            status: parse_status(&self.status)?,
            consecutive_failures: u32::try_from(self.consecutive_failures).unwrap_or(0),
            id: self.id,
            target_url: self.target_url,
            secret: self.secret,
        })
    }
}

#[derive(diesel::QueryableByName)]
struct LogRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    subscription_id: String,
    #[diesel(sql_type = Text)]
    topic: String,
    #[diesel(sql_type = Text)]
    payload: String,
    #[diesel(sql_type = Text)]
    request_headers: String,
    #[diesel(sql_type = Nullable<Integer>)]
    response_status: Option<i32>,
    #[diesel(sql_type = Nullable<Text>)]
    response_body: Option<String>,
    #[diesel(sql_type = BigInt)]
    elapsed_ms: i64,
    #[diesel(sql_type = Integer)]
    attempt: i32,
    #[diesel(sql_type = Integer)]
    max_attempts: i32,
    #[diesel(sql_type = Bool)]
    is_dlq: bool,
    #[diesel(sql_type = Nullable<Text>)]
    last_error: Option<String>,
    #[diesel(sql_type = BigInt)]
    logged_at: i64,
}

impl LogRow {
    fn into_log(self) -> AutumnResult<WebhookDeliveryLog> {
        Ok(WebhookDeliveryLog {
            request_headers: serde_json::from_str::<HashMap<String, String>>(&self.request_headers)
                .map_err(|error| db_error("request_headers", &error))?,
            response_status: self
                .response_status
                .and_then(|status| u16::try_from(status).ok()),
            elapsed_ms: u64::try_from(self.elapsed_ms).unwrap_or(0),
            attempt: u32::try_from(self.attempt).unwrap_or(0),
            max_attempts: u32::try_from(self.max_attempts).unwrap_or(0),
            timestamp: Utc
                .timestamp_millis_opt(self.logged_at)
                .single()
                .unwrap_or(DateTime::<Utc>::MIN_UTC),
            id: self.id,
            subscription_id: self.subscription_id,
            topic: self.topic,
            payload: self.payload,
            response_body: self.response_body,
            is_dlq: self.is_dlq,
            last_error: self.last_error,
        })
    }
}

fn parse_status(status: &str) -> AutumnResult<WebhookSubscriptionStatus> {
    match status {
        "active" => Ok(WebhookSubscriptionStatus::Active),
        "disabled" => Ok(WebhookSubscriptionStatus::Disabled),
        "failed" => Ok(WebhookSubscriptionStatus::Failed),
        other => Err(db_error("status", &format!("unknown status {other:?}"))),
    }
}

/// The delivery outcome a log records, for the failure counter.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Success,
    Failure,
    Pending,
}

const fn outcome(log: &WebhookDeliveryLog) -> Outcome {
    match log.response_status {
        Some(status) if status >= 200 && status < 300 => Outcome::Success,
        Some(_) => Outcome::Failure,
        None if log.last_error.is_some() => Outcome::Failure,
        None => Outcome::Pending,
    }
}

/// A durable [`OutboundWebhookHandler`] on the app database.
///
/// Call [`ensure_schema`](Self::ensure_schema) once at boot, or apply
/// [`WEBHOOK_SCHEMA_SQL`] yourself. The failure counter follows the
/// in-memory store: 50 failures in a row move a subscription to `Failed`.
#[derive(Clone)]
pub struct SqlOutboundWebhookStore {
    pool: Pool<RuntimeConnection>,
}

impl std::fmt::Debug for SqlOutboundWebhookStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlOutboundWebhookStore")
            .finish_non_exhaustive()
    }
}

impl SqlOutboundWebhookStore {
    /// A store on `pool`.
    #[must_use]
    pub const fn new(pool: Pool<RuntimeConnection>) -> Self {
        Self { pool }
    }

    async fn conn(
        &self,
    ) -> AutumnResult<diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>> {
        self.pool
            .get()
            .await
            .map_err(|error| db_error("connection", &error))
    }

    /// Create the tables if they do not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the DDL fails.
    pub async fn ensure_schema(&self) -> AutumnResult<()> {
        let mut conn = self.conn().await?;
        let ddl = crate::backend_select! {
            pg => {
                // No DDL when the tables exist: the app role can lack the
                // rights to create or own them.
                let tables = ["autumn_webhook_subscriptions", "autumn_webhook_deliveries"];
                if crate::outbox::tables_exist(&mut conn, &tables).await? {
                    return Ok(());
                }
                format!("SELECT pg_advisory_xact_lock({SCHEMA_LOCK_KEY});\n{WEBHOOK_SCHEMA_SQL}")
            },
            sqlite => { WEBHOOK_SCHEMA_SQL.to_owned() },
        };
        conn.batch_execute(&ddl)
            .await
            .map_err(|error| db_error("schema", &error))
    }

    /// Delete delivery logs older than `cutoff`, except DLQ logs. Returns the
    /// number of deleted rows. Nothing calls it for you: the table grows
    /// until you call it.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn purge_deliveries_before(&self, cutoff: DateTime<Utc>) -> AutumnResult<usize> {
        diesel::sql_query(sql(
            "DELETE FROM autumn_webhook_deliveries WHERE logged_at < $1 AND NOT is_dlq",
        ))
        .bind::<BigInt, _>(cutoff.timestamp_millis())
        .execute(&mut self.conn().await?)
        .await
        .map_err(|error| db_error("purge", &error))
    }

    /// Insert or replace a subscription.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn create_subscription(
        &self,
        sub: WebhookSubscription,
    ) -> AutumnResult<WebhookSubscription> {
        let topics =
            serde_json::to_string(&sub.event_topics).map_err(|error| db_error("topics", &error))?;
        diesel::sql_query(sql(UPSERT_SUBSCRIPTION_SQL))
            .bind::<Text, _>(&sub.id)
            .bind::<Text, _>(&sub.target_url)
            .bind::<Text, _>(topics)
            .bind::<Text, _>(&sub.secret)
            .bind::<Text, _>(sub.status.as_str())
            .bind::<Integer, _>(i32::try_from(sub.consecutive_failures).unwrap_or(i32::MAX))
            .execute(&mut self.conn().await?)
            .await
            .map_err(|error| db_error("subscription write", &error))?;
        Ok(sub)
    }

    /// The newest `limit` delivery logs.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn get_delivery_logs(&self, limit: usize) -> AutumnResult<Vec<WebhookDeliveryLog>> {
        self.load_logs(
            &format!("SELECT {LOG_COLUMNS} FROM autumn_webhook_deliveries ORDER BY logged_at DESC LIMIT $1"),
            limit,
        )
        .await
    }

    async fn load_logs(&self, query: &str, limit: usize) -> AutumnResult<Vec<WebhookDeliveryLog>> {
        diesel::sql_query(sql(query))
            .bind::<BigInt, _>(i64::try_from(limit).unwrap_or(i64::MAX))
            .load::<LogRow>(&mut self.conn().await?)
            .await
            .map_err(|error| db_error("log read", &error))?
            .into_iter()
            .map(LogRow::into_log)
            .collect()
    }

    async fn write_log(&self, log: &WebhookDeliveryLog) -> AutumnResult<()> {
        let headers = serde_json::to_string(&log.request_headers)
            .map_err(|error| db_error("headers", &error))?;
        diesel::sql_query(sql(UPSERT_LOG_SQL))
            .bind::<Text, _>(&log.id)
            .bind::<Text, _>(&log.subscription_id)
            .bind::<Text, _>(&log.topic)
            .bind::<Text, _>(&log.payload)
            .bind::<Text, _>(headers)
            .bind::<Nullable<Integer>, _>(log.response_status.map(i32::from))
            .bind::<Nullable<Text>, _>(log.response_body.as_deref())
            .bind::<BigInt, _>(i64::try_from(log.elapsed_ms).unwrap_or(i64::MAX))
            .bind::<Integer, _>(i32::try_from(log.attempt).unwrap_or(i32::MAX))
            .bind::<Integer, _>(i32::try_from(log.max_attempts).unwrap_or(i32::MAX))
            .bind::<Bool, _>(log.is_dlq)
            .bind::<Nullable<Text>, _>(log.last_error.as_deref())
            .bind::<BigInt, _>(log.timestamp.timestamp_millis())
            .execute(&mut self.conn().await?)
            .await
            .map_err(|error| db_error("log write", &error))?;
        Ok(())
    }

    async fn update_subscription(&self, query: &str, id: &str) -> AutumnResult<()> {
        diesel::sql_query(sql(query))
            .bind::<Text, _>(id)
            .execute(&mut self.conn().await?)
            .await
            .map_err(|error| db_error("subscription update", &error))?;
        Ok(())
    }

    async fn subscription(&self, id: &str) -> AutumnResult<Option<WebhookSubscription>> {
        diesel::sql_query(sql(&format!(
            "SELECT {SUBSCRIPTION_COLUMNS} FROM autumn_webhook_subscriptions WHERE id = $1"
        )))
        .bind::<Text, _>(id)
        .get_result::<SubscriptionRow>(&mut self.conn().await?)
        .await
        .optional()
        .map_err(|error| db_error("subscription read", &error))?
        .map(SubscriptionRow::into_subscription)
        .transpose()
    }
}

type StoreFuture<T> = Pin<Box<dyn Future<Output = AutumnResult<T>> + Send>>;

impl OutboundWebhookHandler for SqlOutboundWebhookStore {
    fn get_subscriptions(&self, topic: &str) -> StoreFuture<Vec<WebhookSubscription>> {
        let store = self.clone();
        let topic = topic.to_owned();
        Box::pin(async move {
            let rows = diesel::sql_query(format!(
                "SELECT {SUBSCRIPTION_COLUMNS} FROM autumn_webhook_subscriptions \
                 WHERE status = 'active' ORDER BY id"
            ))
            .load::<SubscriptionRow>(&mut store.conn().await?)
            .await
            .map_err(|error| db_error("subscription read", &error))?;
            let mut subs = Vec::new();
            for row in rows {
                let sub = row.into_subscription()?;
                if sub.event_topics.contains(&topic) {
                    subs.push(sub);
                }
            }
            Ok(subs)
        })
    }

    fn log_delivery(&self, log: WebhookDeliveryLog) -> StoreFuture<()> {
        let store = self.clone();
        Box::pin(async move {
            store.write_log(&log).await?;
            match outcome(&log) {
                Outcome::Success => {
                    store
                        .update_subscription(SUCCESS_SQL, &log.subscription_id)
                        .await
                }
                Outcome::Failure => {
                    let changed = diesel::sql_query(sql(FAILURE_SQL))
                        .bind::<Integer, _>(FAILURE_LIMIT)
                        .bind::<Text, _>(&log.subscription_id)
                        .execute(&mut store.conn().await?)
                        .await
                        .map_err(|error| db_error("subscription update", &error))?;
                    if changed > 0
                        && store
                            .subscription(&log.subscription_id)
                            .await?
                            .is_some_and(|sub| sub.status == WebhookSubscriptionStatus::Failed)
                    {
                        tracing::warn!(
                            subscription_id = %log.subscription_id,
                            "Webhook subscription auto-disabled due to 50 consecutive failures"
                        );
                    }
                    Ok(())
                }
                Outcome::Pending => Ok(()),
            }
        })
    }

    fn replace_delivery_log(&self, log: WebhookDeliveryLog) -> StoreFuture<()> {
        let store = self.clone();
        Box::pin(async move { store.write_log(&log).await })
    }

    fn get_subscription(&self, id: &str) -> StoreFuture<Option<WebhookSubscription>> {
        let store = self.clone();
        let id = id.to_owned();
        Box::pin(async move { store.subscription(&id).await })
    }

    fn get_dlq_logs(&self) -> StoreFuture<Vec<WebhookDeliveryLog>> {
        let store = self.clone();
        Box::pin(async move {
            store
                .load_logs(
                    &format!(
                        "SELECT {LOG_COLUMNS} FROM autumn_webhook_deliveries \
                         WHERE is_dlq ORDER BY logged_at DESC LIMIT $1"
                    ),
                    DLQ_READ_LIMIT,
                )
                .await
        })
    }

    fn get_delivery_log(&self, id: &str) -> StoreFuture<Option<WebhookDeliveryLog>> {
        let store = self.clone();
        let id = id.to_owned();
        Box::pin(async move {
            diesel::sql_query(sql(&format!(
                "SELECT {LOG_COLUMNS} FROM autumn_webhook_deliveries WHERE id = $1"
            )))
            .bind::<Text, _>(&id)
            .get_result::<LogRow>(&mut store.conn().await?)
            .await
            .optional()
            .map_err(|error| db_error("log read", &error))?
            .map(LogRow::into_log)
            .transpose()
        })
    }

    fn reset_subscription_failures(&self, id: &str) -> StoreFuture<()> {
        let store = self.clone();
        let id = id.to_owned();
        Box::pin(async move { store.update_subscription(RESET_FAILURES_SQL, &id).await })
    }

    fn reactivate_failed_subscription(&self, id: &str) -> StoreFuture<()> {
        let store = self.clone();
        let id = id.to_owned();
        Box::pin(async move { store.update_subscription(REACTIVATE_SQL, &id).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(status: Option<u16>, error: Option<&str>) -> WebhookDeliveryLog {
        WebhookDeliveryLog {
            id: "l".to_owned(),
            subscription_id: "s".to_owned(),
            topic: "t".to_owned(),
            payload: "{}".to_owned(),
            request_headers: HashMap::new(),
            response_status: status,
            response_body: None,
            elapsed_ms: 0,
            attempt: 1,
            max_attempts: 5,
            is_dlq: false,
            last_error: error.map(str::to_owned),
            timestamp: DateTime::<Utc>::MIN_UTC,
        }
    }

    #[test]
    fn outcome_matches_the_in_memory_store() {
        assert_eq!(outcome(&log(Some(204), None)), Outcome::Success);
        assert_eq!(outcome(&log(Some(500), Some("500"))), Outcome::Failure);
        assert_eq!(outcome(&log(Some(302), None)), Outcome::Failure);
        assert_eq!(outcome(&log(None, Some("timeout"))), Outcome::Failure);
        assert_eq!(outcome(&log(None, None)), Outcome::Pending);
    }

    #[test]
    fn status_round_trips() {
        for status in [
            WebhookSubscriptionStatus::Active,
            WebhookSubscriptionStatus::Disabled,
            WebhookSubscriptionStatus::Failed,
        ] {
            assert_eq!(parse_status(status.as_str()).unwrap(), status);
        }
        assert!(parse_status("paused").is_err());
    }
}
