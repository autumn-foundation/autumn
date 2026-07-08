//! Postgres conformance for `PgSyncBackend` — the same shared suite the
//! in-memory backend passes, against real shadow tables.
//!
//! **Requires Docker** to be running. CI runs it in the Docker-dependent
//! step of the Linux test job (`-- --ignored offline_sync_pg`); it is also
//! runnable manually wherever Docker is available.
//!
//! Note on concurrency: this suite is a sequential script, so it cannot
//! observe the two races `apply_push`'s `pg_advisory_xact_lock` guards
//! against — (1) sequence versions committing out of order, letting a
//! READ COMMITTED pull skip an in-flight lower version forever, and
//! (2) concurrent first-inserts of one pk bypassing the conflict resolver
//! because `SELECT … FOR UPDATE` locks nothing for absent rows. Those
//! guarantees are encoded (and documented) at the lock site in
//! `autumn/src/sync/server.rs`; the suite still pins the *sequential*
//! semantics both races would corrupt (clean version prefixes on pull and
//! resolver engagement for base-version-0 pushes onto existing pks).

#![cfg(feature = "offline-sync")]

use autumn_web::sync::PgSyncBackend;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_backend_passes_conformance() {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let backend = PgSyncBackend::new(url);
    backend.ensure_schema().expect("ensure schema");
    // The DDL is CREATE-IF-NOT-EXISTS all the way down.
    backend
        .ensure_schema()
        .expect("ensure schema twice (idempotent)");

    tokio::task::spawn_blocking(move || {
        super::offline_sync_conformance::run_backend_conformance(&backend);
    })
    .await
    .expect("conformance suite");
}
