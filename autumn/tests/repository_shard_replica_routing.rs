//! Issue #1274: `ShardedDb` transparent read-replica routing for SELECT methods.
//!
//! A repository built with the generated `from_shard(&ShardedDb)` constructor
//! routes its read-only methods to the shard's read replica when one is
//! configured and healthy, while mutating methods stay on the shard primary —
//! transparently, with no handler changes. This mirrors the non-sharded
//! replica routing (#971) but per-shard.
//!
//! This is a live-DB test: it needs a reachable Postgres so the `ShardedDb`
//! extractor can check out a primary connection and the per-shard health
//! indicator can probe the replica pool to mark it ready. Both the shard
//! primary and replica point at the same testcontainer (distinguished only by
//! pool `max_size`), so no real replication is required — only the routing
//! decision is under test, surfaced via `ReadRoute`'s `Debug` output.
//!
//! **Requires Docker** to be running. Run with:
//! `cargo test -p autumn-web --test repository_shard_replica_routing -- --ignored`

#![cfg(feature = "db")]

use autumn_web::config::{AutumnConfig, DatabaseConfig, ReplicaFallback, ShardConfig};
use autumn_web::reexports::axum;
use autumn_web::sharding::{ShardKeyOverride, ShardedDb, ShardedReadDb};
use autumn_web::test::{TestApp, TestClient};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;

mod schema {
    autumn_web::reexports::diesel::table! {
        shard_replica_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        shard_pinned_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }
}

use schema::{shard_pinned_notes, shard_replica_notes};

#[autumn_web::model(table = "shard_replica_notes")]
pub struct ShardReplicaNote {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::model(table = "shard_pinned_notes")]
pub struct ShardPinnedNote {
    #[id]
    pub id: i64,
    pub content: String,
}

/// Default routing: reads follow the shard's read route (replica when ready).
#[autumn_web::repository(ShardReplicaNote, table = "shard_replica_notes")]
pub trait ShardReplicaNoteRepository {}

/// Opt-out: `primary_reads` pins reads to the shard primary.
#[autumn_web::repository(ShardPinnedNote, table = "shard_pinned_notes", primary_reads)]
pub trait ShardPinnedNoteRepository {}

const PRIMARY_SIZE: usize = 5;
const REPLICA_SIZE: usize = 2;

// ── Handlers exercising the real ShardedDb + from_shard path ────────────────

/// Reports the read route a `from_shard` repository snapshots for this shard.
#[autumn_web::get("/read-route")]
async fn read_route(db: ShardedDb) -> String {
    let repo = PgShardReplicaNoteRepository::from_shard(&db);
    format!("{:?}", repo.__autumn_read_route())
}

/// Reports the write pool — must always be the shard primary.
#[autumn_web::get("/write-pool")]
async fn write_pool(db: ShardedDb) -> String {
    let repo = PgShardReplicaNoteRepository::from_shard(&db);
    repo.__autumn_write_pool().status().max_size.to_string()
}

/// `primary_reads` repository must never adopt the replica route.
#[autumn_web::get("/pinned-route")]
async fn pinned_route(db: ShardedDb) -> String {
    let repo = PgShardPinnedNoteRepository::from_shard(&db);
    format!("{:?}", repo.__autumn_read_route())
}

/// A real read method: fails fast when the read route is `Unavailable`.
#[autumn_web::get("/find-all")]
async fn find_all(db: ShardedDb) -> String {
    let repo = PgShardReplicaNoteRepository::from_shard(&db);
    match repo.find_all().await {
        Ok(rows) => format!("ok:{}", rows.len()),
        Err(error) => format!("err:{error}"),
    }
}

/// Explicit replica-only extractor (issue #1275): exercises the
/// `ShardedReadDb` success path — a real replica checkout — plus its full
/// accessor surface (`shard`, `shard_id`, `span`, `db_mut`, `AsMut`) and the
/// `Deref`/`DerefMut` to `AsyncPgConnection` by running a query on the replica
/// connection. Reachable only once the replica has passed its readiness check.
#[autumn_web::get("/replica-only")]
async fn replica_only(mut db: ShardedReadDb) -> String {
    use autumn_web::reexports::diesel;
    use autumn_web::reexports::diesel_async::RunQueryDsl as _;

    let shard = db.shard().to_owned();
    let shard_id = db.shard_id();
    let _span = db.span();
    // AsMut<Db> and db_mut() both borrow the inner Db.
    let _as_mut: &mut autumn_web::db::Db = db.as_mut();
    let _db_mut: &mut autumn_web::db::Db = db.db_mut();
    // Deref/DerefMut to AsyncPgConnection: run a real query on the replica.
    let rows = diesel::sql_query("SELECT 1")
        .execute(&mut *db)
        .await
        .expect("replica query runs");
    format!("replica shard={shard} id={} rows={rows}", shard_id.0)
}

// ── Test harness ────────────────────────────────────────────────────────────

/// One shared Postgres container reused across the (ignored) tests in this file.
static CONTAINER: OnceCell<(ContainerAsync<Postgres>, String)> = OnceCell::const_new();

async fn db_url() -> &'static str {
    let (_container, url) = CONTAINER
        .get_or_init(|| async {
            let container = Postgres::default()
                .start()
                .await
                .expect("failed to start postgres container");
            let host = container.get_host().await.expect("failed to get host");
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("failed to get port");
            let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
            (container, url)
        })
        .await;
    url
}

/// Resolve every request to a fixed shard key so `ShardedDb` extracts without
/// needing tenancy middleware configured.
async fn inject_shard_key(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut()
        .insert(ShardKeyOverride("tenant-1".to_owned()));
    next.run(req).await
}

fn config(url: &str, fallback: ReplicaFallback) -> AutumnConfig {
    AutumnConfig {
        database: DatabaseConfig {
            connect_timeout_secs: 5,
            shards: vec![ShardConfig {
                name: "shard0".to_owned(),
                primary_url: url.to_owned(),
                // Same physical DB; only pool sizes distinguish the roles.
                replica_url: Some(url.to_owned()),
                slots: None,
                primary_pool_size: Some(PRIMARY_SIZE),
                replica_pool_size: Some(REPLICA_SIZE),
                replica_fallback: Some(fallback),
            }],
            ..Default::default()
        },
        ..Default::default()
    }
}

fn app(url: &str, fallback: ReplicaFallback) -> TestClient {
    TestApp::new()
        .routes(autumn_web::routes![
            read_route,
            write_pool,
            pinned_route,
            find_all,
            replica_only
        ])
        .layer(axum::middleware::from_fn(inject_shard_key))
        .config(config(url, fallback))
        .build()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sharded_reads_switch_to_replica_once_it_is_ready() {
    let url = db_url().await;
    let client = app(url, ReplicaFallback::Primary);

    // Before any readiness probe the replica is unchecked, so the `primary`
    // fallback policy keeps reads on the shard primary pool.
    let before = client.get("/read-route").send().await;
    before.assert_ok();
    assert_eq!(
        before.text(),
        format!("ReadRoute::ReadPool(max={PRIMARY_SIZE})"),
        "an unchecked replica under primary fallback must keep reads on the primary"
    );

    // Hitting /ready runs the per-shard health indicator, which probes the
    // replica pool and marks it ready — no handler change required.
    client.get("/ready").send().await.assert_ok();

    let after = client.get("/read-route").send().await;
    after.assert_ok();
    assert_eq!(
        after.text(),
        format!("ReadRoute::ReadPool(max={REPLICA_SIZE})"),
        "reads must transparently route to the shard replica once it is ready"
    );

    // Writes always target the shard primary regardless of replica readiness.
    let write = client.get("/write-pool").send().await;
    write.assert_ok();
    assert_eq!(
        write.text(),
        PRIMARY_SIZE.to_string(),
        "mutating methods must always use the shard primary pool"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn primary_reads_repository_never_routes_to_the_replica() {
    let url = db_url().await;
    let client = app(url, ReplicaFallback::Primary);

    // Mark the replica ready; a `primary_reads` repo must still ignore it.
    client.get("/ready").send().await.assert_ok();

    let route = client.get("/pinned-route").send().await;
    route.assert_ok();
    assert_eq!(
        route.text(),
        "ReadRoute::Primary",
        "primary_reads repositories must keep reads on the shard primary"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sharded_read_db_serves_only_a_ready_replica() {
    let url = db_url().await;
    // fail_readiness so an unchecked replica is genuinely unavailable, not a
    // silent primary fallback — ShardedReadDb must 503 either way.
    let client = app(url, ReplicaFallback::FailReadiness);

    // Before the readiness probe there is no healthy replica → 503.
    let before = client.get("/replica-only").send().await;
    assert_eq!(
        before.status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "ShardedReadDb must fail fast with 503 until the replica is ready"
    );

    // The readiness probe marks the replica ready; the extractor now checks
    // out the replica and the handler exercises its accessor surface.
    client.get("/ready").send().await.assert_ok();

    let after = client.get("/replica-only").send().await;
    after.assert_ok();
    let body = after.text();
    assert!(
        body.starts_with("replica shard=shard0") && body.contains("rows="),
        "replica-only handler must serve the ready replica, got: {body}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fail_readiness_reads_are_unavailable_until_the_replica_is_ready() {
    let url = db_url().await;
    let client = app(url, ReplicaFallback::FailReadiness);

    // The replica has not passed a readiness check and the policy forbids
    // falling back to the primary, so reads are unavailable.
    let route = client.get("/read-route").send().await;
    route.assert_ok();
    assert_eq!(
        route.text(),
        "ReadRoute::Unavailable",
        "fail_readiness must not silently fall back to the primary"
    );

    // A real read method fails fast with a replica-specific error.
    let find = client.get("/find-all").send().await;
    find.assert_ok();
    let body = find.text();
    assert!(
        body.starts_with("err:") && body.to_lowercase().contains("replica"),
        "read must fail fast explaining the replica is unavailable, got: {body}"
    );
}
