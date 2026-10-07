//! `/ready` pings the primary database instead of reading pool saturation
//! (issue #3059).
//!
//! - A busy pool is not a dead database: a fully checked-out pool with a
//!   healthy database is ready.
//! - An idle pool is not a live database: an unreachable database is not
//!   ready, and the probe answers within the ping timeout.
//! - Many probes at the same time cause at most one ping per cache window.
//!
//! The "unreachable" database is a local TCP listener that accepts and never
//! answers. It needs no Docker. The healthy-database tests need Docker.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use autumn_web::AppState;
use autumn_web::config::DatabaseConfig;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use diesel_async::pooled_connection::deadpool::Pool;
use tower::ServiceExt as _;

type DbPool = Pool<autumn_web::db::RuntimeConnection>;

fn pool_for(url: String, size: usize, wait_secs: u64) -> DbPool {
    let config = DatabaseConfig {
        url: Some(url),
        pool_size: size,
        connect_timeout_secs: wait_secs,
        ..Default::default()
    };
    autumn_web::db::create_pool(&config)
        .expect("valid pool config")
        .expect("pool url is set")
}

fn ready_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/ready",
            axum::routing::get(autumn_web::probe::ready_handler::<AppState>),
        )
        .with_state(state)
}

async fn get_ready(router: &Router) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible")
        .status()
}

/// A TCP listener that accepts every connection, never answers, and counts
/// the connections it accepts.
struct BlackHole {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl BlackHole {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind black hole");
        let addr = listener.local_addr().expect("local addr");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                // Hold the socket open and never answer.
                tokio::spawn(async move {
                    let _socket = socket;
                    std::future::pending::<()>().await;
                });
            }
        });
        Self {
            addr,
            accepted,
            task,
        }
    }

    fn url(&self) -> String {
        format!("postgres://autumn@{}/autumn", self.addr)
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for BlackHole {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn ready_is_unavailable_within_timeout_when_db_is_unreachable_and_pool_is_idle() {
    let db = BlackHole::start().await;
    let pool = pool_for(db.url(), 4, 30);
    let state = AppState::for_test().with_pool(pool.clone());
    let timeout = Duration::from_millis(500);
    state
        .probes()
        .configure_db_check(Duration::from_secs(1), timeout);
    let router = ready_router(state);

    assert_eq!(pool.status().size, 0, "pool is idle: no connections");
    let started = Instant::now();
    let status = get_ready(&router).await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        elapsed < timeout + Duration::from_secs(3),
        "/ready took {elapsed:?}; the ping timeout is {timeout:?}"
    );
}

#[tokio::test]
async fn concurrent_ready_probes_ping_the_db_at_most_once_per_cache_window() {
    let db = BlackHole::start().await;
    let state = AppState::for_test().with_pool(pool_for(db.url(), 4, 30));
    let ttl = Duration::from_secs(1);
    state
        .probes()
        .configure_db_check(ttl, Duration::from_millis(200));
    let router = ready_router(state);

    let burst = || {
        let probes: Vec<_> = (0..32)
            .map(|_| {
                let router = router.clone();
                tokio::spawn(async move { get_ready(&router).await })
            })
            .collect();
        async move {
            for probe in probes {
                assert_eq!(
                    probe.await.expect("probe task"),
                    StatusCode::SERVICE_UNAVAILABLE
                );
            }
        }
    };

    burst().await;
    assert_eq!(db.accepted(), 1, "32 probes, one window: one ping");

    tokio::time::sleep(ttl + Duration::from_millis(100)).await;
    burst().await;
    assert_eq!(db.accepted(), 2, "second window: one more ping");
}

#[tokio::test]
async fn ready_and_actuator_health_share_one_ping_per_cache_window() {
    let db = BlackHole::start().await;
    let state = AppState::for_test().with_pool(pool_for(db.url(), 4, 30));
    state
        .probes()
        .configure_db_check(Duration::from_secs(60), Duration::from_millis(200));
    let router = Router::new()
        .route(
            "/ready",
            axum::routing::get(autumn_web::probe::ready_handler::<AppState>),
        )
        .route(
            "/actuator/health",
            axum::routing::get(autumn_web::actuator::health::<AppState>),
        )
        .with_state(state);

    let probes: Vec<_> = (0..32)
        .map(|i| {
            let router = router.clone();
            let uri = if i % 2 == 0 {
                "/ready"
            } else {
                "/actuator/health"
            };
            tokio::spawn(async move {
                router
                    .oneshot(
                        Request::builder()
                            .uri(uri)
                            .body(Body::empty())
                            .expect("request"),
                    )
                    .await
                    .expect("infallible")
                    .status()
            })
        })
        .collect();
    for probe in probes {
        assert_eq!(
            probe.await.expect("probe task"),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    assert_eq!(db.accepted(), 1, "both endpoints use one cached ping");
}

mod with_postgres {
    use super::*;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    async fn start_postgres() -> (testcontainers::ContainerAsync<Postgres>, String) {
        let container = Postgres::default()
            .start()
            .await
            .expect("start Postgres container");
        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("container port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        (container, url)
    }

    /// Backend pids of the other connections to this database.
    async fn other_backend_pids(
        conn: &mut diesel_async::pooled_connection::deadpool::Object<
            autumn_web::db::RuntimeConnection,
        >,
    ) -> Vec<i32> {
        use diesel::sql_types::Integer;
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Pid {
            #[diesel(sql_type = Integer)]
            pid: i32,
        }
        diesel::sql_query(
            "SELECT pid FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() ORDER BY pid",
        )
        .load::<Pid>(conn)
        .await
        .expect("list backends")
        .into_iter()
        .map(|row| row.pid)
        .collect()
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn ready_is_ok_when_pool_is_fully_checked_out_and_db_is_healthy() {
        let (_container, url) = start_postgres().await;
        let pool = pool_for(url, 2, 30);
        let held_a = pool.get().await.expect("first connection");
        let held_b = pool.get().await.expect("second connection");
        // A request that waits for a connection: the old rule
        // `available > 0 || waiting == 0` reported this as not ready.
        let waiter = {
            let pool = pool.clone();
            tokio::spawn(async move { drop(pool.get().await) })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while pool.status().waiting == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a request waits for a connection");
        assert_eq!(pool.status().available, 0, "pool is fully checked out");

        let state = AppState::for_test().with_pool(pool.clone());
        let status = get_ready(&ready_router(state)).await;

        assert_eq!(status, StatusCode::OK);
        drop((held_a, held_b));
        waiter.await.expect("waiter task");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn ready_turns_unavailable_when_the_db_stops() {
        let (container, url) = start_postgres().await;
        let state = AppState::for_test().with_pool(pool_for(url, 2, 30));
        let timeout = Duration::from_millis(500);
        state.probes().configure_db_check(Duration::ZERO, timeout);
        let router = ready_router(state);
        assert_eq!(get_ready(&router).await, StatusCode::OK);

        container.stop().await.expect("stop Postgres");
        let started = Instant::now();
        let status = get_ready(&router).await;
        let elapsed = started.elapsed();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            elapsed < timeout + Duration::from_secs(3),
            "/ready took {elapsed:?}; the ping timeout is {timeout:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn ready_survives_a_ping_connection_the_server_closed() {
        let (_container, url) = start_postgres().await;
        let pool = pool_for(url, 2, 30);
        let state = AppState::for_test().with_pool(pool.clone());
        state
            .probes()
            .configure_db_check(Duration::ZERO, Duration::from_secs(2));
        let router = ready_router(state);
        assert_eq!(get_ready(&router).await, StatusCode::OK);

        // Close the kept ping connection from the server side, as an idle
        // timeout or a pooler restart does. Wait until its backend is gone.
        let mut conn = pool.get().await.expect("connection");
        let old_pids = other_backend_pids(&mut conn).await;
        assert_eq!(old_pids.len(), 1, "one kept ping connection");
        {
            use diesel_async::RunQueryDsl as _;
            diesel::sql_query(format!("SELECT pg_terminate_backend({})", old_pids[0]))
                .execute(&mut conn)
                .await
                .expect("terminate ping backend");
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while other_backend_pids(&mut conn).await == old_pids {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("ping backend exits");

        assert_eq!(get_ready(&router).await, StatusCode::OK);
        let new_pids = other_backend_pids(&mut conn).await;
        assert_eq!(new_pids.len(), 1);
        assert_ne!(new_pids, old_pids, "the ping reconnected");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn ready_reuses_one_dedicated_connection() {
        let (_container, url) = start_postgres().await;
        let pool = pool_for(url, 2, 30);
        let state = AppState::for_test().with_pool(pool.clone());
        state
            .probes()
            .configure_db_check(Duration::ZERO, Duration::from_secs(2));
        let router = ready_router(state);

        for _ in 0..5 {
            assert_eq!(get_ready(&router).await, StatusCode::OK);
        }

        let mut conn = pool.get().await.expect("connection");
        let backends: i64 = {
            use diesel::sql_types::BigInt;
            use diesel_async::RunQueryDsl as _;
            #[derive(diesel::QueryableByName)]
            struct Count {
                #[diesel(sql_type = BigInt)]
                n: i64,
            }
            diesel::sql_query(
                "SELECT count(*) AS n FROM pg_stat_activity \
                 WHERE datname = current_database() AND pid <> pg_backend_pid()",
            )
            .get_result::<Count>(&mut conn)
            .await
            .expect("count backends")
            .n
        };
        assert_eq!(backends, 1, "five probes share one ping connection");
        assert_eq!(pool.status().size, 1, "probes do not use the pool");
    }
}
