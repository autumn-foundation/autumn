//! Fail-closed boot guard for `database.statement_timeout` under the `SQLite`
//! backend (issue #1996, item 4).
//!
//! `SQLite` cannot enforce a per-statement wall-clock timeout through the async
//! connection wrapper: diesel's `SqliteConnection` exposes no
//! interrupt/progress-handler hook (nor the raw `sqlite3` handle) through
//! `SyncConnectionWrapper`, so a runaway query cannot be aborted mid-flight.
//! True parity is impossible, so — per the fail-closed rule — autumn-web refuses
//! to boot rather than silently ignore a configured guarantee it cannot honor:
//! `db::reject_sqlite_statement_timeout` (called from every pool-construction
//! entry point) turns a non-zero `statement_timeout` under the sqlite backend
//! into a loud, typed [`autumn_web::db::PoolError`] naming the config key.
//!
//! A `None` or zero timeout (the default) asks for no guarantee and boots
//! cleanly, so ordinary `SQLite` apps are unaffected.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_statement_timeout_fail_closed
//! ```
#![cfg(feature = "sqlite")]

use std::time::Duration;

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{PoolError, create_pool};

/// A non-zero `statement_timeout` under the sqlite backend must fail the boot
/// fast with a loud, specific, TYPED error — not a panic, and not a silent
/// no-op that pretends the guarantee holds.
#[test]
fn create_pool_rejects_configured_statement_timeout() {
    let config = DatabaseConfig {
        url: Some("sqlite::memory:".to_string()),
        statement_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    };

    let Err(err) = create_pool(&config) else {
        panic!("create_pool must fail-closed when statement_timeout is set under sqlite");
    };

    // A real typed error, not a panic-in-disguise.
    assert!(
        matches!(err, PoolError::UnsupportedBackend(_)),
        "expected UnsupportedBackend, got: {err:?}"
    );

    let msg = err.to_string();
    assert!(
        msg.contains("database.statement_timeout"),
        "error must name the config key: {msg}"
    );
    assert!(
        msg.contains("SQLite"),
        "error must mention the SQLite backend: {msg}"
    );
    assert!(
        msg.contains("30000ms"),
        "error should report the configured timeout in ms: {msg}"
    );
}

/// The same rejection applies to the topology builder (primary + optional
/// replica), which is the boot path most apps actually take.
#[test]
fn create_topology_rejects_configured_statement_timeout() {
    let config = DatabaseConfig {
        url: Some("sqlite::memory:".to_string()),
        statement_timeout: Some(Duration::from_millis(1500)),
        ..Default::default()
    };

    let Err(err) = autumn_web::db::create_topology(&config) else {
        panic!("create_topology must fail-closed when statement_timeout is set under sqlite");
    };
    assert!(
        matches!(err, PoolError::UnsupportedBackend(_)),
        "expected UnsupportedBackend, got: {err:?}"
    );
    assert!(
        err.to_string().contains("database.statement_timeout"),
        "topology error must name the config key: {err}"
    );
}

/// No configured timeout (the default) boots cleanly — the guard must never bite
/// an ordinary `SQLite` deployment.
#[test]
fn create_pool_without_statement_timeout_succeeds() {
    let config = DatabaseConfig {
        url: Some("sqlite::memory:".to_string()),
        statement_timeout: None,
        ..Default::default()
    };

    let pool = create_pool(&config)
        .expect("sqlite pool must build cleanly with no statement_timeout")
        .expect("a url is configured");
    // Building the pool is the assertion; drop it.
    drop(pool);
}

/// An explicit zero timeout is the "disabled" convention (parity with
/// `ARROYO_RECORDING_RETENTION_DAYS=0` / the Postgres `statement_timeout = 0`
/// meaning) and must also boot cleanly.
#[test]
fn create_pool_with_zero_statement_timeout_succeeds() {
    let config = DatabaseConfig {
        url: Some("sqlite::memory:".to_string()),
        statement_timeout: Some(Duration::from_secs(0)),
        ..Default::default()
    };

    let pool = create_pool(&config)
        .expect("a zero statement_timeout means disabled and must build cleanly")
        .expect("a url is configured");
    drop(pool);
}
