//! Replica lag query on a real Postgres (issue #3065).
//!
//! **Requires Docker.** A plain Postgres is not in recovery, so its lag is
//! `0` and reads go to the "replica" pool. This proves the lag SQL runs and
//! that `/ready` records the result.

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use std::time::Duration;

use autumn_web::AppState;
use autumn_web::config::ReplicaFallback;
use axum::extract::State;
use axum::response::IntoResponse;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

fn pool(url: &str, max_size: usize) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .expect("pool")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ready_measures_zero_lag_on_a_server_not_in_recovery() {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let state = AppState::for_test()
        .with_pool(pool(&url, 5))
        .with_replica_pool(pool(&url, 2));
    let probes = state.probes();
    probes.configure_replica_dependency(ReplicaFallback::FailReadiness);
    probes.configure_replica_max_lag(Some(Duration::from_secs(1)));
    probes.mark_startup_complete();

    let response = autumn_web::probe::ready_handler(State(state.clone()))
        .await
        .into_response();

    assert_eq!(response.status(), http::StatusCode::OK);
    let status = state.probes().replica_status().expect("replica configured");
    assert_eq!(status.lag_ms, Some(0), "{status:?}");
    assert!(status.ready, "{status:?}");
    assert_eq!(
        state.read_pool().map(|p| p.status().max_size),
        Some(2),
        "a fresh replica serves reads"
    );
}
