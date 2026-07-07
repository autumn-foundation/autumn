//! Postgres conformance for `PgSyncBackend` — the same shared suite the
//! in-memory backend passes, against real shadow tables.
//!
//! **Requires Docker** to be running (CI-only, like the other
//! testcontainers-backed suites).

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
