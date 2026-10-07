//! Per-replica database faults (issue #3067).
//!
//! A [`DbLink`] sits between one replica and the shared `SQLite` database of a
//! simulation. [`SqliteSubstrate::replica_pool`](crate::sim::substrate::SqliteSubstrate::replica_pool)
//! builds the replica's pool behind it. The link injects three faults:
//!
//! - **Session loss.** [`DbLink::lose_session`] refuses new checkouts until
//!   [`DbLink::restore_session`]. The idle connection closes. A connection that
//!   a task holds closes when the task returns it. A lease the replica holds
//!   then cannot renew.
//! - **Mid-query error.** [`DbLink::mid_query_errors`] makes a write to a table
//!   fail part way. The statement does not apply.
//! - **Commit ambiguity.** [`DbLink::commit_ambiguity`] makes a write to a table
//!   apply, then return an error. The caller cannot tell that it applied.
//!
//! The faults hit only this replica. The other replicas use the same database
//! through their own links.
//!
//! # How
//!
//! The pool hooks refuse a checkout while the session is lost. For the write
//! faults, each connection of the link registers the SQL function
//! `autumn_sim_fault(kind, table)` and creates `TEMP` triggers on the faulted
//! tables. A temp trigger exists only on its own connection, so other replicas
//! do not see it. A `BEFORE` trigger with `RAISE(ABORT)` is the mid-query
//! error. An `AFTER` trigger with `RAISE(FAIL)` is the commit ambiguity: `FAIL`
//! keeps the row change it already made.
//!
//! # Limits
//!
//! - Only writes (`INSERT`, `UPDATE`, `DELETE`) fault. `SQLite` has no trigger
//!   on `SELECT`.
//! - A trigger fires per row. On a write to many rows, an ambiguous fault keeps
//!   the rows before it.
//! - Inside an explicit transaction, the caller rolls back on the error, so an
//!   ambiguous fault acts as a mid-query error.
//! - A table that does not exist yet gets its triggers at a later checkout.
//!
//! # Determinism
//!
//! Each link draws its decisions from its own seeded stream, one draw per row
//! the faulted table sees on this link. [`DbLink::events`] records each
//! decision, so two runs of a seed can compare them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use diesel::expression::functions::declare_sql_function;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;

use crate::db::RuntimeConnection;
use crate::entropy::{Entropy, SeededEntropy};

/// A fault kind on a replica's database link.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DbFaultKind {
    /// The replica lost its database session. A checkout failed.
    SessionLost,
    /// A write failed part way and did not apply.
    MidQuery,
    /// A write applied, but the caller got an error.
    CommitAmbiguous,
}

impl DbFaultKind {
    /// The name the SQL triggers pass to `autumn_sim_fault`.
    const fn sql_name(self) -> &'static str {
        match self {
            Self::SessionLost => "session_lost",
            Self::MidQuery => "mid_query",
            Self::CommitAmbiguous => "commit_ambiguous",
        }
    }

    fn from_sql_name(name: &str) -> Option<Self> {
        match name {
            "mid_query" => Some(Self::MidQuery),
            "commit_ambiguous" => Some(Self::CommitAmbiguous),
            _ => None,
        }
    }
}

/// One fault decision on a [`DbLink`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFaultEvent {
    kind: DbFaultKind,
    table: String,
    seq: u64,
    fired: bool,
}

impl DbFaultEvent {
    /// The fault kind.
    #[must_use]
    pub const fn kind(&self) -> DbFaultKind {
        self.kind
    }

    /// The table, or an empty string for a session fault.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The position of this decision on the link, from 0.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether the fault fired.
    #[must_use]
    pub const fn fired(&self) -> bool {
        self.fired
    }
}

/// The fault switch between one replica and the shared database.
///
/// Cheap to clone: clones share one link. Get one from
/// [`Sim::db_link`](crate::sim::Sim::db_link), which seeds it from the sim
/// seed and the replica name, or build one with [`DbLink::new`].
#[derive(Clone)]
pub struct DbLink {
    inner: Arc<LinkInner>,
}

struct LinkInner {
    name: String,
    stream: Arc<dyn Entropy>,
    session_lost: AtomicBool,
    state: Mutex<LinkState>,
}

#[derive(Default)]
struct LinkState {
    /// Fault probability per `(table, kind)`.
    rules: BTreeMap<(String, DbFaultKind), f64>,
    seq: u64,
    events: Vec<DbFaultEvent>,
}

impl std::fmt::Debug for DbLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("DbLink")
            .field("name", &self.inner.name)
            .field("session_lost", &self.is_session_lost())
            .field("rules", &state.rules)
            .finish_non_exhaustive()
    }
}

impl DbLink {
    /// A link for the replica `name`, with fault decisions drawn from `seed`.
    #[must_use]
    pub fn new(name: impl Into<String>, seed: u64) -> Self {
        Self {
            inner: Arc::new(LinkInner {
                name: name.into(),
                stream: SeededEntropy::shared(seed),
                session_lost: AtomicBool::new(false),
                state: Mutex::new(LinkState::default()),
            }),
        }
    }

    /// The replica name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Refuse new checkouts. The idle connection closes. A connection that a
    /// task holds now closes when the task returns it. Each refused checkout
    /// is a [`DbFaultKind::SessionLost`] event.
    pub fn lose_session(&self) {
        self.inner.session_lost.store(true, Ordering::SeqCst);
    }

    /// Let the replica open a session again.
    pub fn restore_session(&self) {
        self.inner.session_lost.store(false, Ordering::SeqCst);
    }

    /// Whether the session is lost now.
    #[must_use]
    pub fn is_session_lost(&self) -> bool {
        self.inner.session_lost.load(Ordering::SeqCst)
    }

    /// Make a write to `table` on this link fail part way, with `probability`
    /// per row. The write does not apply. `probability` is clamped to
    /// `[0.0, 1.0]`.
    ///
    /// # Panics
    ///
    /// Panics if `table` is not a plain identifier (ASCII letters, digits and
    /// `_`).
    pub fn mid_query_errors(&self, table: &str, probability: f64) {
        self.set_rule(table, DbFaultKind::MidQuery, probability);
    }

    /// Make a write to `table` on this link apply and then return an error,
    /// with `probability` per row. `probability` is clamped to `[0.0, 1.0]`.
    ///
    /// # Panics
    ///
    /// Panics if `table` is not a plain identifier (ASCII letters, digits and
    /// `_`).
    pub fn commit_ambiguity(&self, table: &str, probability: f64) {
        self.set_rule(table, DbFaultKind::CommitAmbiguous, probability);
    }

    /// Remove every table fault. The session state does not change.
    pub fn clear_faults(&self) {
        let mut state = self.lock();
        state.rules.clear();
    }

    /// Every fault decision so far, in order.
    #[must_use]
    pub fn events(&self) -> Vec<DbFaultEvent> {
        self.lock().events.clone()
    }

    fn set_rule(&self, table: &str, kind: DbFaultKind, probability: f64) {
        assert!(
            is_identifier(table),
            "DbLink: `{table}` is not a plain table name (ASCII letters, digits, `_`)"
        );
        let mut state = self.lock();
        state.rules.insert(
            (table.to_owned(), kind),
            super::chaos::clamp_prob(probability),
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LinkState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn record(state: &mut LinkState, kind: DbFaultKind, table: &str, fired: bool) {
        let seq = state.seq;
        state.seq += 1;
        state.events.push(DbFaultEvent {
            kind,
            table: table.to_owned(),
            seq,
            fired,
        });
    }

    /// The decision the SQL function returns for one row. No rule, no draw.
    fn decide(&self, kind: &str, table: &str) -> bool {
        let Some(kind) = DbFaultKind::from_sql_name(kind) else {
            return false;
        };
        let mut state = self.lock();
        let Some(&probability) = state.rules.get(&(table.to_owned(), kind)) else {
            return false;
        };
        let draw = super::chaos::unit_from_draw(self.inner.stream.next_u64());
        let fired = draw < probability;
        Self::record(&mut state, kind, table, fired);
        drop(state);
        fired
    }

    fn note_refused(&self) {
        let mut state = self.lock();
        Self::record(&mut state, DbFaultKind::SessionLost, "", true);
    }

    /// The faulted tables. Each connection checks its own triggers.
    fn fault_tables(&self) -> Vec<String> {
        let mut tables: Vec<String> = self
            .lock()
            .rules
            .keys()
            .map(|(table, _)| table.clone())
            .collect();
        tables.dedup();
        tables
    }
}

/// Whether `name` is safe to splice into the trigger DDL.
fn is_identifier(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[declare_sql_function]
extern "SQL" {
    /// `1` when the link fault `kind` fires for a row of `table`.
    fn autumn_sim_fault(
        kind: diesel::sql_types::Text,
        table: diesel::sql_types::Text,
    ) -> diesel::sql_types::Integer;
}

/// The trigger DDL for one faulted table.
fn trigger_statements(table: &str) -> Vec<String> {
    let mut statements = Vec::with_capacity(6);
    for op in ["INSERT", "UPDATE", "DELETE"] {
        let lower = op.to_ascii_lowercase();
        let mid = DbFaultKind::MidQuery.sql_name();
        let ambiguous = DbFaultKind::CommitAmbiguous.sql_name();
        statements.push(format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS \"autumn_sim_mq_{table}_{lower}\" \
             BEFORE {op} ON \"main\".\"{table}\" FOR EACH ROW \
             WHEN autumn_sim_fault('{mid}', '{table}') \
             BEGIN SELECT RAISE(ABORT, 'autumn sim: mid-query fault on {table}'); END"
        ));
        statements.push(format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS \"autumn_sim_ca_{table}_{lower}\" \
             AFTER {op} ON \"main\".\"{table}\" FOR EACH ROW \
             WHEN autumn_sim_fault('{ambiguous}', '{table}') \
             BEGIN SELECT RAISE(FAIL, 'autumn sim: commit outcome unknown on {table}'); END"
        ));
    }
    statements
}

type Manager = AsyncDieselConnectionManager<RuntimeConnection>;
type HookError = deadpool::managed::HookError<diesel_async::pooled_connection::PoolError>;

const SESSION_LOST: &str = "autumn sim: the database session is lost";

/// Give each faulted table its triggers on this connection. `IF NOT EXISTS`
/// makes it cheap to repeat. A table that does not exist yet is tried again at
/// the next checkout.
fn install_triggers(conn: &mut diesel::SqliteConnection, link: &DbLink) {
    use diesel::connection::SimpleConnection as _;

    for table in link.fault_tables() {
        for statement in trigger_statements(&table) {
            if let Err(error) = conn.batch_execute(&statement)
                && !error.to_string().contains("no such table")
            {
                tracing::warn!(%error, table, "sim db link: a fault trigger failed");
            }
        }
    }
}

/// Open one gated connection: connect, set the pragmas, and with a link,
/// register the fault function and its triggers.
async fn open(url: String, link: Option<DbLink>) -> diesel::ConnectionResult<RuntimeConnection> {
    use diesel::Connection as _;
    use diesel::connection::SimpleConnection as _;

    let link = std::panic::AssertUnwindSafe(link);
    // The helper runs the closure in a turn of the gate.
    let conn = crate::time::spawn_blocking(move || {
        let link = link;
        if let Some(link) = link.as_ref()
            && link.is_session_lost()
        {
            link.note_refused();
            return Err(diesel::ConnectionError::BadConnection(
                SESSION_LOST.to_owned(),
            ));
        }
        let mut conn = diesel::SqliteConnection::establish(&url)?;
        conn.batch_execute(crate::db::sqlite_connection_pragmas(
            false,
            crate::db::sqlite_replication_active(),
        ))
        .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        if let Some(link) = link.0 {
            // The function never panics, so no state is left half updated.
            let decider = std::panic::AssertUnwindSafe(link.clone());
            autumn_sim_fault_utils::register_nondeterministic_impl(
                &mut conn,
                move |kind: String, table: String| i32::from(decider.decide(&kind, &table)),
            )
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
            install_triggers(&mut conn, &link);
        }
        conn.set_instrumentation(super::gate::GateInstrumentation::default());
        Ok(conn)
    })
    .await
    .map_err(|error| diesel::ConnectionError::BadConnection(error.to_string()))??;
    Ok(RuntimeConnection::new(conn))
}

/// A one-slot pool on the sim database at `url`. Every connection is gated
/// (see [`super::gate`]). With `link`, the pool also takes the link's faults.
pub(crate) fn pool(
    url: &str,
    link: Option<&DbLink>,
) -> Result<
    diesel_async::pooled_connection::deadpool::Pool<RuntimeConnection>,
    deadpool::managed::BuildError,
> {
    use deadpool::managed::Hook;

    let mut config = diesel_async::pooled_connection::ManagerConfig::<RuntimeConnection>::default();
    let setup_link = link.cloned();
    config.custom_setup =
        Box::new(move |url: &str| Box::pin(open(url.to_owned(), setup_link.clone())));
    let timeout = std::time::Duration::from_secs(
        crate::config::DatabaseConfig::default().connect_timeout_secs,
    );
    let builder = deadpool::managed::Pool::builder(Manager::new_with_config(url, config))
        .max_size(1)
        .wait_timeout(Some(timeout))
        .create_timeout(Some(timeout))
        .runtime(deadpool::Runtime::Tokio1);
    let Some(link) = link else {
        return builder.build();
    };
    let on_recycle = link.clone();
    let on_reuse = link.clone();
    builder
        // A lost session closes the idle connection: deadpool drops it, and
        // the open that follows is refused.
        .pre_recycle(Hook::sync_fn(move |_conn, _metrics| {
            if on_recycle.is_session_lost() {
                Err(HookError::message(SESSION_LOST))
            } else {
                Ok(())
            }
        }))
        .post_recycle(Hook::async_fn(
            move |conn: &mut RuntimeConnection, _metrics| {
                let link = on_reuse.clone();
                Box::pin(async move {
                    if link.fault_tables().is_empty() {
                        return Ok(());
                    }
                    conn.spawn_blocking(move |conn| {
                        let _gate = super::gate::Scope::enter();
                        install_triggers(conn, &link);
                        Ok(())
                    })
                    .await
                    .map_err(|error| {
                        HookError::message(format!("autumn sim: fault triggers: {error}"))
                    })
                })
            },
        ))
        .build()
}

#[cfg(test)]
mod tests {
    use super::{DbFaultKind, DbLink, is_identifier, trigger_statements};

    #[test]
    fn only_plain_identifiers_reach_the_ddl() {
        assert!(is_identifier("autumn_jobs"));
        assert!(is_identifier("T1"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("jobs; DROP TABLE x"));
        assert!(!is_identifier("main.jobs"));
        assert!(!is_identifier("\"jobs\""));
    }

    #[test]
    #[should_panic(expected = "not a plain table name")]
    fn a_bad_table_name_panics() {
        DbLink::new("a", 0).mid_query_errors("x; --", 1.0);
    }

    #[test]
    fn triggers_cover_every_write_and_both_faults() {
        let statements = trigger_statements("t");
        assert_eq!(statements.len(), 6);
        for op in ["INSERT", "UPDATE", "DELETE"] {
            assert!(
                statements
                    .iter()
                    .any(|s| s.contains(&format!("BEFORE {op}")))
            );
            assert!(
                statements
                    .iter()
                    .any(|s| s.contains(&format!("AFTER {op}")))
            );
        }
        assert!(statements.iter().all(|s| s.contains("TEMP TRIGGER")));
        assert!(statements.iter().any(|s| s.contains("RAISE(ABORT")));
        assert!(statements.iter().any(|s| s.contains("RAISE(FAIL")));
    }

    #[test]
    fn decisions_draw_only_for_a_rule_and_replay_from_the_seed() {
        let draws = |seed| {
            let link = DbLink::new("a", seed);
            assert!(!link.decide("mid_query", "t"), "no rule, no fault");
            link.mid_query_errors("t", 0.5);
            let fired: Vec<bool> = (0..64).map(|_| link.decide("mid_query", "t")).collect();
            assert!(!link.decide("commit_ambiguous", "t"), "other kind, no rule");
            assert!(!link.decide("bogus", "t"));
            (fired, link.events())
        };
        let (a, events) = draws(9);
        assert_eq!(draws(9).0, a, "same seed, same faults");
        assert_eq!(events.len(), 64, "one event per decision");
        assert!(
            events
                .iter()
                .all(|e| e.kind() == DbFaultKind::MidQuery && e.table() == "t")
        );
        assert_eq!(events.last().map(super::DbFaultEvent::seq), Some(63));
        assert!(a.iter().any(|f| *f) && a.iter().any(|f| !*f), "p=0.5 mixes");
    }

    #[test]
    fn clear_faults_keeps_the_session_state() {
        let link = DbLink::new("a", 0);
        link.lose_session();
        link.commit_ambiguity("t", 1.0);
        link.clear_faults();
        assert!(link.is_session_lost());
        assert!(!link.decide("commit_ambiguous", "t"));
        link.restore_session();
        assert!(!link.is_session_lost());
    }
}
