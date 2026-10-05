//! Database tests for the fencing lease lock (`autumn_web::lock::LeaseLock`,
//! issue #3053).
//!
//! These tests need Docker (testcontainers). They are `#[ignore]`d, and the CI
//! Docker sweep runs them.

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use autumn_web::lock::{FencingToken, LeaseLock, LockError};
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::net::{TcpListener, TcpStream};

type PgPool = Pool<AsyncPgConnection>;

async fn start_postgres() -> (String, testcontainers::ContainerAsync<Postgres>) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        container,
    )
}

fn pool_for(url: &str) -> PgPool {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager)
        .max_size(16)
        .wait_timeout(Some(Duration::from_secs(2)))
        .create_timeout(Some(Duration::from_secs(2)))
        .runtime(deadpool::Runtime::Tokio1)
        .build()
        .expect("pool")
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// A TCP proxy that a test can cut. A cut closes every open socket and refuses
/// new ones. This is how a test makes a holder lose its connection.
struct CuttableProxy {
    addr: std::net::SocketAddr,
    cut: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CuttableProxy {
    async fn start(upstream: std::net::SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        let cut = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (cut_task, notify_task) = (Arc::clone(&cut), Arc::clone(&notify));
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                if AtomicBool::load(&cut_task, Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let notify = Arc::clone(&notify_task);
                tokio::spawn(async move {
                    let Ok(mut server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        () = notify.notified() => {}
                    }
                });
            }
        });
        Self { addr, cut, notify }
    }

    /// Close all open sockets and refuse new ones.
    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Accept new sockets again.
    fn heal(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }

    /// `upstream` with its host and port replaced by the proxy's.
    fn url_for(&self, upstream: &str) -> String {
        let mut url = url::Url::parse(upstream).expect("url");
        url.set_host(Some("127.0.0.1")).expect("host");
        url.set_port(Some(self.addr.port())).expect("port");
        url.to_string()
    }
}

async fn upstream_addr(url: &str) -> std::net::SocketAddr {
    let parsed = url::Url::parse(url).expect("url");
    let host = parsed.host_str().expect("host");
    let port = parsed.port().expect("port");
    tokio::net::lookup_host((host, port))
        .await
        .expect("resolve")
        .find(std::net::SocketAddr::is_ipv4)
        .expect("ipv4 address")
}

async fn create_resource(pool: &PgPool) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS fenced_resource ( \
           id BIGINT PRIMARY KEY, value TEXT NOT NULL, fencing_token BIGINT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create resource table");
    diesel::sql_query(
        "INSERT INTO fenced_resource (id, value, fencing_token) VALUES (1, 'initial', 0) \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(&mut conn)
    .await
    .expect("seed resource");
}

/// The conditional write from the guide. It returns the number of rows it
/// changed: 1 when the token is current, 0 when the token is stale.
async fn fenced_write(pool: &PgPool, value: &str, token: FencingToken) -> usize {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "UPDATE fenced_resource SET value = $1, fencing_token = $2 \
         WHERE id = 1 AND fencing_token <= $2",
    )
    .bind::<diesel::sql_types::Text, _>(value)
    .bind::<diesel::sql_types::BigInt, _>(token.as_i64())
    .execute(&mut conn)
    .await
    .expect("fenced write")
}

async fn resource_value(pool: &PgPool) -> String {
    #[derive(diesel::QueryableByName)]
    struct Value {
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT value FROM fenced_resource WHERE id = 1")
        .get_result::<Value>(&mut conn)
        .await
        .expect("read resource")
        .value
}

/// Move a lease expiry into the past. This stands for a holder that paused
/// (GC, VM freeze) for longer than its TTL.
async fn force_expire(pool: &PgPool, name: &str) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "UPDATE autumn_lease_locks SET expires_at = now() - interval '1 second' WHERE name = $1",
    )
    .bind::<diesel::sql_types::Text, _>(name)
    .execute(&mut conn)
    .await
    .expect("force expire");
}

async fn acquire_within(lock: &LeaseLock, budget: Duration) -> autumn_web::lock::LeaseGuard {
    lock.lock_timeout(budget)
        .await
        .expect("lock should become free within the budget")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn try_lock_is_exclusive_and_tokens_increase() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let a = LeaseLock::new(pool.clone(), "lease-exclusive");
    let b = LeaseLock::new(pool.clone(), "lease-exclusive");

    let first = a.try_lock().await.expect("a").expect("a acquires");
    assert!(
        b.try_lock().await.expect("b").is_none(),
        "the lease must stay exclusive while held"
    );
    let t1 = first.fencing_token();
    first.release().await.expect("release a");

    let second = b.try_lock().await.expect("b").expect("b acquires");
    let t2 = second.fencing_token();
    second.release().await.expect("release b");

    let third = a.try_lock().await.expect("a").expect("a acquires again");
    let t3 = third.fencing_token();
    third.release().await.expect("release a again");

    assert!(t1 < t2 && t2 < t3, "tokens must increase: {t1} {t2} {t3}");
    assert_eq!(t1, FencingToken::try_from(1).expect("token"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn distinct_names_have_independent_generations() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let ga = LeaseLock::new(pool.clone(), "lease-name-a")
        .try_lock()
        .await
        .expect("a")
        .expect("a acquires");
    let gb = LeaseLock::new(pool.clone(), "lease-name-b")
        .try_lock()
        .await
        .expect("b")
        .expect("b acquires while a is held");
    assert_eq!(ga.fencing_token().get(), 1);
    assert_eq!(gb.fencing_token().get(), 1);
    ga.release().await.expect("release a");
    gb.release().await.expect("release b");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn release_keeps_the_generation() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let lock = LeaseLock::new(pool.clone(), "lease-keeps-generation");
    for _ in 0..3 {
        let guard = lock.try_lock().await.expect("acquire").expect("free");
        guard.release().await.expect("release");
    }
    let mut conn = pool.get().await.expect("conn");
    let row = diesel::sql_query(
        "SELECT generation AS n FROM autumn_lease_locks \
         WHERE name = 'lease-keeps-generation' AND owner IS NULL",
    )
    .get_result::<Count>(&mut conn)
    .await
    .expect("the row stays after release");
    assert_eq!(row.n, 3, "release must not reset the generation");
}

/// AC: holder A loses its connection, B acquires, and A's write with the old
/// token is rejected.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn partitioned_holder_loses_lease_and_its_stale_write_is_rejected() {
    let (url, _container) = start_postgres().await;
    let direct = pool_for(&url);
    create_resource(&direct).await;

    let proxy = CuttableProxy::start(upstream_addr(&url).await).await;
    let via_proxy = pool_for(&proxy.url_for(&url));

    let name = "lease-partition";
    let a_lock = LeaseLock::new(via_proxy.clone(), name).with_lease_ttl(Duration::from_secs(4));
    let a = a_lock.try_lock().await.expect("a").expect("a acquires");
    let token_a = a.fencing_token();
    assert_eq!(fenced_write(&direct, "a-before", token_a).await, 1);

    // A loses its connection to the database.
    proxy.cut();
    tokio::time::timeout(Duration::from_secs(8), a.lease_lost())
        .await
        .expect("A must see lease_lost");
    assert!(a.is_lost());
    // A learns of the loss before the database lets another holder in.
    assert!(
        LeaseLock::new(direct.clone(), name)
            .try_lock()
            .await
            .expect("probe")
            .is_none(),
        "the local loss signal must come before the database expiry"
    );

    // B acquires once A's lease expires in the database.
    let b_lock = LeaseLock::new(direct.clone(), name).with_poll_interval(Duration::from_millis(50));
    let b = acquire_within(&b_lock, Duration::from_secs(10)).await;
    let token_b = b.fencing_token();
    assert!(
        token_b > token_a,
        "B's token must be larger: {token_a} {token_b}"
    );
    assert_eq!(fenced_write(&direct, "b", token_b).await, 1);

    // A's late write with its old token is rejected.
    assert_eq!(
        fenced_write(&direct, "a-stale", token_a).await,
        0,
        "a stale token must not write"
    );
    assert_eq!(resource_value(&direct).await, "b");

    // A reconnects and releases. That must not free B's lease.
    proxy.heal();
    a.release()
        .await
        .expect("the stale release reaches the database");
    assert!(
        LeaseLock::new(direct.clone(), name)
            .try_lock()
            .await
            .expect("probe")
            .is_none(),
        "a stale release must not free the successor's lease"
    );
    b.release().await.expect("release b");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn expired_holder_sees_lease_lost_and_cannot_free_successor() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let name = "lease-expired-holder";
    let lock = LeaseLock::new(pool.clone(), name).with_lease_ttl(Duration::from_secs(1));
    let a = lock.try_lock().await.expect("a").expect("a acquires");

    force_expire(&pool, name).await;
    let b = LeaseLock::new(pool.clone(), name)
        .try_lock()
        .await
        .expect("b")
        .expect("b takes the expired lease");
    assert!(b.fencing_token() > a.fencing_token());

    tokio::time::timeout(Duration::from_secs(3), a.lease_lost())
        .await
        .expect("A's next renewal must find the lease lost");

    a.release()
        .await
        .expect("the stale release reaches the database");
    assert!(
        LeaseLock::new(pool.clone(), name)
            .try_lock()
            .await
            .expect("probe")
            .is_none(),
        "A's release must not free B"
    );
    b.release().await.expect("release b");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn renewal_keeps_a_long_section_alive() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let name = "lease-renewal";
    let guard = LeaseLock::new(pool.clone(), name)
        .with_lease_ttl(Duration::from_secs(3))
        .try_lock()
        .await
        .expect("acquire")
        .expect("free");
    let token = guard.fencing_token();
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            LeaseLock::new(pool.clone(), name)
                .try_lock()
                .await
                .expect("probe")
                .is_none(),
            "a renewed lease must stay held past its TTL"
        );
        assert!(!guard.is_lost());
        assert_eq!(
            guard.fencing_token(),
            token,
            "renewal must not change the token"
        );
    }
    guard.release().await.expect("release");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn try_with_cancels_the_section_when_the_lease_is_lost() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let name = "lease-cancel-section";
    let lock = LeaseLock::new(pool.clone(), name).with_lease_ttl(Duration::from_secs(1));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

    // Expire and take the lease only after the section starts, so the
    // holder's next renewal finds it gone.
    let taker = {
        let pool = pool.clone();
        tokio::spawn(async move {
            started_rx.await.expect("the section starts");
            force_expire(&pool, name).await;
            LeaseLock::new(pool, name)
                .try_lock()
                .await
                .expect("taker")
                .expect("takes the expired lease")
        })
    };

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        lock.try_with(|lease| async move {
            assert_eq!(lease.fencing_token().get(), 1);
            started_tx.send(()).expect("signal start");
            std::future::pending::<()>().await;
        }),
    )
    .await
    .expect("try_with must stop when the lease is lost");
    assert!(
        matches!(result, Err(LockError::LeaseLost { .. })),
        "expected LeaseLost, got {result:?}"
    );
    taker
        .await
        .expect("taker")
        .release()
        .await
        .expect("release taker");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn with_runs_the_section_and_releases() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let lock = LeaseLock::new(pool.clone(), "lease-with");
    let token = lock
        .with(|lease| async move { lease.fencing_token() })
        .await
        .expect("with acquires");
    assert_eq!(token.get(), 1);
    let again = lock.try_lock().await.expect("probe").expect("released");
    assert_eq!(again.fencing_token().get(), 2);
    again.release().await.expect("release");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lock_timeout_expires_while_held() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let lock =
        LeaseLock::new(pool.clone(), "lease-timeout").with_poll_interval(Duration::from_millis(20));
    let held = lock.try_lock().await.expect("acquire").expect("free");
    let err = lock
        .lock_timeout(Duration::from_millis(150))
        .await
        .expect_err("must time out while held");
    assert!(matches!(err, LockError::Timeout { .. }), "got {err:?}");
    held.release().await.expect("release");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dropped_guard_frees_the_lease() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let lock =
        LeaseLock::new(pool.clone(), "lease-drop").with_poll_interval(Duration::from_millis(20));
    drop(lock.try_lock().await.expect("acquire").expect("free"));
    let again = acquire_within(&lock, Duration::from_secs(2)).await;
    assert_eq!(again.fencing_token().get(), 2);
    again.release().await.expect("release");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_first_use_creates_the_table_once() {
    let (url, _container) = start_postgres().await;
    let pool = pool_for(&url);
    let mut tasks = Vec::new();
    for i in 0..8 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            LeaseLock::new(pool, format!("lease-first-use-{i}"))
                .try_lock()
                .await
                .expect("first use must not race the DDL")
                .expect("distinct names are free")
                .release()
                .await
                .expect("release");
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }
}

/// AC: generations are strictly increasing under concurrent acquire and
/// release. Each worker records its token inside the lease, so the record
/// order is the grant order.
#[test]
#[ignore = "requires Docker (testcontainers)"]
fn generations_strictly_increase_under_concurrency() {
    use proptest::prelude::*;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    let (url, _container) = rt.block_on(start_postgres());
    let pool = pool_for(&url);
    let case = std::sync::atomic::AtomicUsize::new(0);

    let config = ProptestConfig {
        cases: 8,
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    proptest!(config, |(workers in 2_usize..6, rounds in 1_usize..6)| {
        let name = format!("lease-prop-{}", case.fetch_add(1, Ordering::SeqCst));
        let tokens = rt.block_on(contend(pool.clone(), name, workers, rounds));
        prop_assert_eq!(tokens.len(), workers * rounds);
        for pair in tokens.windows(2) {
            prop_assert!(pair[0] < pair[1], "tokens must strictly increase: {:?}", tokens);
        }
        prop_assert_eq!(tokens[0].get(), 1);
        prop_assert_eq!(
            tokens.last().map(|t| t.get()),
            Some(u64::try_from(workers * rounds).expect("fits"))
        );
    });
}

async fn contend(pool: PgPool, name: String, workers: usize, rounds: usize) -> Vec<FencingToken> {
    let tokens = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for _ in 0..workers {
        let lock =
            LeaseLock::new(pool.clone(), name.clone()).with_poll_interval(Duration::from_millis(5));
        let tokens = Arc::clone(&tokens);
        tasks.push(tokio::spawn(async move {
            for _ in 0..rounds {
                let guard = acquire_within(&lock, Duration::from_secs(30)).await;
                tokens.lock().await.push(guard.fencing_token());
                guard.release().await.expect("release");
            }
        }));
    }
    for task in tasks {
        task.await.expect("worker");
    }
    Arc::try_unwrap(tokens).expect("sole owner").into_inner()
}
