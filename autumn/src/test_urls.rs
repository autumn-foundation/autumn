//! Backend-parametric database targets for unit-test fixtures.
//!
//! The pool, topology, sharding and state machinery is backend-independent, so
//! its assertions must hold on whichever backend [`RuntimeConnection`] resolves
//! to. Fixtures that spell `postgres://` inline hold only on the default build:
//! `build_sqlite_pool` refuses a Postgres target outright, so under
//! `--features sqlite` each such test panicked at fixture construction — 52 of
//! them, unnoticed until the sqlite lane started running the lib tests.
//!
//! Routing fixtures through these helpers runs the same assertions on both
//! backends instead of silencing them on one.
//!
//! [`RuntimeConnection`]: crate::db::RuntimeConnection

/// A primary/write target named `name`.
///
/// The SQLite spelling is a SHARED-CACHE in-memory target on purpose: it needs
/// no files and no cleanup, and `sqlite_target_is_memory` exempts
/// `cache=shared` from the single-slot rule, so a configured `max_size` still
/// reaches the pool and the sizing assertions stay meaningful. deadpool is
/// lazy, so no connection is opened unless a test checks one out.
#[cfg(not(feature = "sqlite"))]
pub(crate) fn primary(name: &str) -> String {
    format!("postgres://localhost/{name}")
}

/// See [`primary`] (Postgres variant).
#[cfg(feature = "sqlite")]
pub(crate) fn primary(name: &str) -> String {
    format!("sqlite:file:{name}?mode=memory&cache=shared")
}

/// A target that no connection attempt can reach, on either backend.
///
/// Health-indicator tests need a pool whose checkout FAILS. "Unreachable" has
/// to be spelled per backend: nothing listens on TCP port 1, while a SQLite
/// in-memory target is always reachable — so the SQLite spelling is a file in a
/// directory that does not exist, which `sqlite3_open` refuses with
/// `SQLITE_CANTOPEN`.
#[cfg(not(feature = "sqlite"))]
pub(crate) fn unreachable(name: &str) -> String {
    format!("postgres://localhost:1/{name}")
}

/// See [`unreachable`] (Postgres variant).
#[cfg(feature = "sqlite")]
pub(crate) fn unreachable(name: &str) -> String {
    format!("sqlite:///autumn-no-such-directory/{name}.db")
}
