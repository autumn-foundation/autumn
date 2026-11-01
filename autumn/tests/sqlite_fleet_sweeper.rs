//! The fleet sweeper (issue 3234): work in a closed fleet database is done
//! without a request for it.
//!
//! - An outbox row written to a tenant database is relayed after the
//!   database closes.
//! - Queued commit hooks drain without a request.
//! - A derivation added in a migration backfills a closed tenant.
//! - Rows of a deleted or unknown database are dropped.
//! - The sweeper never opens past `max_open` and never closes a leased
//!   database.
//! - Busy open databases do not take every pass from a closed one, and a
//!   full fleet still relays its open ones (issue 3350).
//!
//! Run:
//!
//! ```sh
//! cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_fleet_sweeper
//! ```
#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, DatabaseConfig, DatabaseFleetConfig, OutboxConfig};
use autumn_web::db::fleet::DatabaseFleet;
use autumn_web::db::fleet_sweeper::{FleetSweeper, FleetSweeperOptions, PendingKind};
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::fleet_layout::{FleetDbKey, FleetMode};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::outbox::{Outbox, OutboxMessage};
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async, serde_json};
use autumn_web::sharding::ShardedDb;
use autumn_web::test::{TestApp, TestClient};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use scoped_futures::ScopedFutureExt as _;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/fleet_sweeper_migrations");
/// The schema before the migration that adds the derived column.
const MIGRATIONS_V1: EmbeddedMigrations =
    embed_migrations!("tests/fixtures/fleet_sweeper_migrations_v1");

mod schema {
    autumn_web::reexports::diesel::table! {
        sw_posts (id) {
            id -> Int8,
            title -> Text,
            comment_count -> Int8,
        }
    }

    autumn_web::reexports::diesel::table! {
        sw_comments (id) {
            id -> Int8,
            post_id -> Int8,
            body -> Text,
        }
    }
}

use schema::{sw_comments, sw_posts};

#[autumn_web::model(table = "sw_posts")]
pub struct SwPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(SwPost, table = "sw_posts")]
pub trait SwPostRepository {}

#[autumn_web::model(table = "sw_comments")]
#[derivation(SwPost, column = "comment_count", fk = post_id)]
pub struct SwComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

#[autumn_web::repository(SwComment, table = "sw_comments")]
pub trait SwCommentRepository {}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// What the outbox handler saw: the tenant in scope, and the payload.
#[derive(Default)]
struct Relayed(Mutex<Vec<(Option<String>, serde_json::Value)>>);

fn relayed(state: &AppState) -> Vec<(Option<String>, serde_json::Value)> {
    state
        .extension::<Relayed>()
        .expect("installed")
        .0
        .lock()
        .unwrap()
        .clone()
}

async fn on_order_placed(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let tenant = autumn_web::tenancy::CURRENT_TENANT
        .try_with(Clone::clone)
        .ok()
        .flatten();
    state
        .extension::<Relayed>()
        .expect("installed")
        .0
        .lock()
        .unwrap()
        .push((tenant, message.payload));
    Ok(())
}

/// Writes one outbox row on the tenant database, in a transaction. With
/// `fail`, the transaction rolls back.
async fn write_order(db: &mut ShardedDb, outbox: Outbox, id: i64, fail: bool) -> AutumnResult<()> {
    autumn_web::db::scoped_transaction(&mut **db, |conn| {
        async move {
            outbox
                .write(
                    conn,
                    &format!("order:{id}"),
                    "order.placed",
                    &serde_json::json!({ "id": id }),
                )
                .await?;
            if fail {
                Err(AutumnError::bad_request_msg("roll back"))
            } else {
                Ok(())
            }
        }
        .scope_boxed()
    })
    .await
}

#[post("/orders/{id}")]
async fn place(mut db: ShardedDb, outbox: Outbox, Path(id): Path<i64>) -> AutumnResult<String> {
    write_order(&mut db, outbox, id, false).await?;
    Ok(db.shard().to_owned())
}

#[post("/orders/{id}/rollback")]
async fn place_and_roll_back(
    mut db: ShardedDb,
    outbox: Outbox,
    Path(id): Path<i64>,
) -> AutumnResult<String> {
    write_order(&mut db, outbox, id, true).await?;
    Ok(db.shard().to_owned())
}

fn control_pool(dir: &FsPath) -> Pool<RuntimeConnection> {
    create_pool(&DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .expect("control pool builds")
    .expect("url configured")
}

fn fleet_with(dir: &FsPath, max_open: usize, migrations: EmbeddedMigrations) -> DatabaseFleet {
    DatabaseFleet::builder(DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: dir.join("fleet").display().to_string(),
        path: None,
        max_open,
        pool_size: 2,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: std::time::Duration::ZERO,
        restore_check_sample: 0,
        replication_workers: 4,
        min_residency: std::time::Duration::from_secs(2),
    })
    .min_residency(Duration::ZERO)
    .migrations("app", migrations)
    .framework_migrations()
    .build()
    .expect("fleet builds")
}

fn fleet(dir: &FsPath, max_open: usize) -> DatabaseFleet {
    fleet_with(dir, max_open, MIGRATIONS)
}

fn config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.health.detailed = true;
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config
}

fn app(dir: &FsPath, fleet: DatabaseFleet) -> TestClient {
    TestApp::new()
        .config(config())
        .routes(routes![place, place_and_roll_back])
        .with_db(control_pool(dir))
        .with_fleet(fleet)
        .with_outbox(OutboxConfig::default())
        .outbox_handler("order.placed", on_order_placed)
        .state_initializer(|state| state.insert_extension(Relayed::default()))
        .build()
}

async fn install(client: &TestClient) -> FleetSweeper {
    install_with(client, FleetSweeperOptions::default()).await
}

async fn install_with(client: &TestClient, options: FleetSweeperOptions) -> FleetSweeper {
    FleetSweeper::install_with(client.state(), options)
        .await
        .expect("sweeper installs")
        .expect("the app has a fleet")
}

fn key(fleet: &DatabaseFleet, tenant: &str) -> FleetDbKey {
    fleet.key_for(tenant).unwrap()
}

/// Wait until no database is closing.
async fn settle(fleet: &DatabaseFleet) {
    for _ in 0..1000 {
        if fleet.stats().draining == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("drains did not settle");
}

async fn pending(sweeper: &FleetSweeper) -> Vec<(String, PendingKind)> {
    let mut rows: Vec<(String, PendingKind)> = sweeper
        .index()
        .pending(100)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| (entry.key, entry.kind))
        .collect();
    rows.sort();
    rows
}

async fn post(client: &TestClient, tenant: &str, path: &str) -> u16 {
    client
        .post(path)
        .header("x-tenant-id", tenant)
        .send()
        .await
        .status
        .as_u16()
}

#[tokio::test]
async fn an_outbox_row_in_a_tenant_database_is_relayed_after_the_database_closes() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install(&client).await;

    assert_eq!(post(&client, "acme", "/orders/1").await, 200);
    assert_eq!(
        pending(&sweeper).await,
        vec![("tenant:acme".to_owned(), PendingKind::Outbox)],
        "the write marked its database"
    );
    let acme = key(&fleet, "acme");
    fleet.close(&acme).await;
    assert!(fleet.open_keys().is_empty());
    assert!(
        relayed(client.state()).is_empty(),
        "nothing relays a closed database yet"
    );

    let report = sweeper.sweep_once().await;
    assert_eq!((report.visited, report.cleared), (1, 1), "{report:?}");
    assert_eq!(
        relayed(client.state()),
        vec![(Some("acme".to_owned()), serde_json::json!({ "id": 1 }))],
        "relayed once, in the tenant's scope"
    );
    assert!(pending(&sweeper).await.is_empty());
    settle(&fleet).await;
    assert!(
        fleet.open_keys().is_empty(),
        "the sweeper closes a database it opened"
    );

    // Nothing is sent twice.
    sweeper.sweep_once().await;
    assert_eq!(relayed(client.state()).len(), 1);
}

type Ran = Arc<Mutex<Vec<Option<String>>>>;

/// Register a hook runner under `handler_key` that records the tenant in
/// scope.
fn hook_runner(handler_key: &'static str) -> Ran {
    let ran: Ran = Arc::default();
    let observed = Arc::clone(&ran);
    autumn_web::__private::register_repository_commit_hook_runner(
        handler_key,
        move |_ctx, _record| {
            let observed = Arc::clone(&observed);
            async move {
                let tenant = autumn_web::tenancy::CURRENT_TENANT
                    .try_with(Clone::clone)
                    .ok()
                    .flatten();
                observed.lock().unwrap().push(tenant);
                Ok(())
            }
        },
        |_ctx, _record| async { Ok(()) },
        |_ctx, _record| async { Ok(()) },
    );
    ran
}

/// Queue one commit hook in `tenant`'s database, then close the database.
async fn queue_hook_and_close(fleet: &DatabaseFleet, tenant: &str, handler_key: &str) {
    let key = key(fleet, tenant);
    let db = fleet.open(&key).await.unwrap();
    {
        let mut conn = db.pool().get().await.unwrap();
        autumn_web::tenancy::with_tenant(tenant.to_owned(), async {
            autumn_web::__private::enqueue_repository_commit_hook_on_conn(
                &mut conn,
                handler_key,
                "create",
                Some("hook-1"),
                None,
                &serde_json::json!({ "op": "create" }),
                &serde_json::json!({ "id": 1 }),
            )
            .await
            .unwrap();
        })
        .await;
    }
    drop(db);
    fleet.close(&key).await;
}

#[tokio::test]
async fn queued_commit_hooks_drain_without_a_request() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install_with(
        &client,
        FleetSweeperOptions {
            hook_grace: Duration::ZERO,
            ..FleetSweeperOptions::default()
        },
    )
    .await;
    // The first pass records the derivations; no database exists to mark.
    sweeper.sweep_once().await;

    let ran = hook_runner("test::sqlite_fleet_sweeper::drained");
    queue_hook_and_close(&fleet, "acme", "test::sqlite_fleet_sweeper::drained").await;
    assert_eq!(
        pending(&sweeper).await,
        vec![("tenant:acme".to_owned(), PendingKind::Hooks)],
        "the enqueue marked its database"
    );
    assert!(fleet.open_keys().is_empty());

    let report = sweeper.sweep_once().await;
    assert_eq!((report.visited, report.cleared), (1, 1), "{report:?}");
    assert_eq!(
        *ran.lock().unwrap(),
        vec![Some("acme".to_owned())],
        "ran once, in the originating tenant's scope"
    );
    assert!(pending(&sweeper).await.is_empty());
}

#[tokio::test]
async fn young_hooks_of_a_closed_database_wait_for_the_grace() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install_with(
        &client,
        FleetSweeperOptions {
            // Long, so a slow runner cannot pass the grace.
            hook_grace: Duration::from_secs(3600),
            ..FleetSweeperOptions::default()
        },
    )
    .await;
    sweeper.sweep_once().await;

    let ran = hook_runner("test::sqlite_fleet_sweeper::young");
    queue_hook_and_close(&fleet, "acme", "test::sqlite_fleet_sweeper::young").await;
    // The process that has a database open drains its hooks; a worker waits
    // `hook_grace` before it opens the database for them.
    let report = sweeper.sweep_once().await;
    assert_eq!(report.visited, 0, "{report:?}");
    assert!(fleet.open_keys().is_empty());
    assert!(ran.lock().unwrap().is_empty());
    assert_eq!(pending(&sweeper).await.len(), 1);
}

#[tokio::test]
async fn a_derivation_added_in_a_migration_backfills_a_closed_tenant() {
    let tmp = tempfile::tempdir().unwrap();
    // Before the deploy: the schema has no derived column.
    let before = fleet_with(tmp.path(), 4, MIGRATIONS_V1);
    let acme = key(&before, "acme");
    let db = before.provision(&acme).await.unwrap();
    {
        let mut conn = db.pool().get().await.unwrap();
        // More parents than the on-open round repairs (8 batches of 1000),
        // so the sweeper's own rounds run too.
        for sql in [
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 9000) \
             INSERT INTO sw_posts (id, title) SELECT i, 'post' FROM n",
            "INSERT INTO sw_comments (post_id, body) \
             VALUES (1, 'a'), (1, 'b'), (1, 'c'), (9000, 'd'), (9000, 'e')",
        ] {
            diesel::sql_query(sql).execute(&mut conn).await.unwrap();
        }
    }
    drop(db);
    before.close_all().await;

    // The deploy: a migration adds the column, the binary declares the
    // derivation. Nobody asks for the tenant.
    let after = fleet(tmp.path(), 4);
    let client = app(tmp.path(), after.clone());
    let sweeper = install(&client).await;
    let report = sweeper.sweep_once().await;
    assert_eq!((report.visited, report.cleared), (1, 1), "{report:?}");
    assert!(
        report.handled > 0,
        "the sweeper ran batches itself: {report:?}"
    );
    assert!(pending(&sweeper).await.is_empty());
    settle(&after).await;
    assert!(after.open_keys().is_empty());

    let db = after.open(&acme).await.unwrap();
    let mut conn = db.pool().get().await.unwrap();
    let counts: Vec<Count> = diesel::sql_query(
        "SELECT comment_count AS n FROM sw_posts WHERE id IN (1, 2, 9000) ORDER BY id",
    )
    .load(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        counts.iter().map(|c| c.n).collect::<Vec<_>>(),
        vec![3, 0, 2]
    );
    drop(conn);
    drop(db);

    // The registry is recorded: the next pass marks nothing again.
    sweeper.sweep_once().await;
    assert!(pending(&sweeper).await.is_empty());
}

#[tokio::test]
async fn the_sweeper_never_opens_past_max_open_nor_closes_a_leased_database() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 1);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install(&client).await;
    let (acme, globex) = (key(&fleet, "acme"), key(&fleet, "globex"));

    assert_eq!(post(&client, "globex", "/orders/1").await, 200);
    fleet.close(&globex).await;
    // A hot tenant holds the one open slot, and a lease on it.
    let hot = fleet.open(&acme).await.unwrap();
    assert_eq!(post(&client, "acme", "/orders/2").await, 200);

    let report = sweeper.sweep_once().await;
    assert_eq!(report.deferred, 1, "no room for globex: {report:?}");
    assert_eq!(
        relayed(client.state()),
        vec![(Some("acme".to_owned()), serde_json::json!({ "id": 2 }))],
        "the open database is swept in place"
    );
    settle(&fleet).await;
    assert_eq!(fleet.open_keys(), vec![acme.clone()], "never past max_open");
    assert!(!hot.closed().is_cancelled(), "a leased database stays open");
    // The first pass of a binary with derivations also marks every database
    // for a backfill. The deferred database keeps both rows.
    assert_eq!(
        pending(&sweeper).await,
        vec![
            ("tenant:globex".to_owned(), PendingKind::Outbox),
            ("tenant:globex".to_owned(), PendingKind::Backfill),
        ]
    );

    // Once the slot is free, the deferred database is swept.
    drop(hot);
    fleet.close(&acme).await;
    let report = sweeper.sweep_once().await;
    assert_eq!(report.cleared, 1, "{report:?}");
    assert_eq!(relayed(client.state()).len(), 2);
    assert!(pending(&sweeper).await.is_empty());
}

#[tokio::test]
async fn a_rolled_back_write_leaves_a_mark_the_sweeper_clears() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install(&client).await;

    assert_eq!(post(&client, "acme", "/orders/1/rollback").await, 400);
    assert_eq!(
        pending(&sweeper).await,
        vec![("tenant:acme".to_owned(), PendingKind::Outbox)],
        "the mark is written before the commit, so a rollback leaves it"
    );
    fleet.close(&key(&fleet, "acme")).await;
    let report = sweeper.sweep_once().await;
    assert_eq!(report.cleared, 1, "{report:?}");
    assert!(relayed(client.state()).is_empty());
    assert!(pending(&sweeper).await.is_empty());
}

#[tokio::test]
async fn a_web_process_sweeps_only_the_databases_it_has_open() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install_with(
        &client,
        FleetSweeperOptions {
            relay: false,
            ..FleetSweeperOptions::default()
        },
    )
    .await;

    assert_eq!(post(&client, "acme", "/orders/1").await, 200);
    fleet.close(&key(&fleet, "acme")).await;
    let report = sweeper.sweep_once().await;
    assert_eq!(report.visited, 0, "a web process opens nothing: {report:?}");
    assert!(fleet.open_keys().is_empty());
    assert_eq!(pending(&sweeper).await.len(), 1);
    assert!(relayed(client.state()).is_empty(), "only a worker relays");
}

#[tokio::test]
async fn health_reports_the_backlog_and_the_oldest_pending_age() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let _sweeper = install(&client).await;
    assert_eq!(post(&client, "acme", "/orders/1").await, 200);
    assert_eq!(post(&client, "globex", "/orders/2").await, 200);

    let health: serde_json::Value = client.get("/actuator/health").send().await.json();
    let sweeper = &health["components"]["fleet:sweeper"];
    assert_eq!(sweeper["status"], "UP", "{health}");
    assert_eq!(sweeper["details"]["backlog"], 2, "{health}");
    assert!(
        sweeper["details"]["oldest_pending_secs"].is_u64(),
        "{health}"
    );
}

#[tokio::test]
async fn an_old_binary_in_a_rolling_deploy_never_runs_or_clears_a_backfill() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install(&client).await;
    // This binary records its derivations; then a newer binary records its own.
    sweeper.sweep_once().await;
    let mut control = client.state().pool().unwrap().get().await.unwrap();
    diesel::sql_query(
        "INSERT INTO _autumn_fleet_derivation_epochs (fingerprint) VALUES ('a-newer-binary')",
    )
    .execute(&mut control)
    .await
    .unwrap();
    drop(control);

    let acme = key(&fleet, "acme");
    drop(fleet.open(&acme).await.unwrap());
    fleet.close(&acme).await;
    sweeper
        .index()
        .mark("tenant:acme", PendingKind::Backfill)
        .await
        .unwrap();
    sweeper.sweep_once().await;
    assert_eq!(
        pending(&sweeper).await,
        vec![("tenant:acme".to_owned(), PendingKind::Backfill)],
        "the newer binary's row stays"
    );

    let db = fleet.open(&acme).await.unwrap();
    let mut conn = db.pool().get().await.unwrap();
    let reconciled: Count = diesel::sql_query("SELECT COUNT(*) AS n FROM _autumn_derivations")
        .get_result(&mut conn)
        .await
        .unwrap();
    assert_eq!(reconciled.n, 0, "the old definitions were never applied");
}

#[tokio::test]
async fn rows_of_a_deleted_or_unknown_database_are_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install(&client).await;

    assert_eq!(post(&client, "acme", "/orders/1").await, 200);
    let acme = key(&fleet, "acme");
    fleet.close(&acme).await;
    assert!(fleet.delete(&acme).await.unwrap(), "acme is deleted");
    // A slot name is not a database of a tenant fleet.
    sweeper
        .index()
        .mark("slot:00001", PendingKind::Outbox)
        .await
        .unwrap();
    assert_eq!(pending(&sweeper).await.len(), 2);

    let report = sweeper.sweep_once().await;
    assert_eq!((report.visited, report.cleared), (2, 2), "{report:?}");
    assert!(pending(&sweeper).await.is_empty(), "both rows are dropped");
    assert!(!fleet.exists(&acme), "the sweeper creates nothing");
    assert!(relayed(client.state()).is_empty());
}

#[tokio::test]
async fn a_busy_database_takes_its_turn_again_after_its_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install_with(
        &client,
        FleetSweeperOptions {
            max_messages_per_database: 1,
            ..FleetSweeperOptions::default()
        },
    )
    .await;
    for id in 1..=3 {
        assert_eq!(post(&client, "acme", &format!("/orders/{id}")).await, 200);
    }
    fleet.close(&key(&fleet, "acme")).await;

    for pass in 1..=2 {
        let report = sweeper.sweep_once().await;
        assert_eq!(
            (report.visited, report.cleared, report.handled),
            (1, 0, 1),
            "pass {pass} relays one row: {report:?}"
        );
        assert_eq!(relayed(client.state()).len(), pass);
        assert_eq!(
            pending(&sweeper).await,
            vec![("tenant:acme".to_owned(), PendingKind::Outbox)],
            "the row stays and is due at once"
        );
    }
    let report = sweeper.sweep_once().await;
    assert_eq!((report.visited, report.cleared), (1, 1), "{report:?}");
    assert_eq!(relayed(client.state()).len(), 3);
    assert!(pending(&sweeper).await.is_empty());
}

// Issue 3350: when open databases went first, busy open databases took
// every pass.
#[tokio::test]
async fn a_closed_database_is_visited_when_more_busy_databases_are_open_than_a_pass_visits() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let client = app(tmp.path(), fleet.clone());
    let budget = 2;
    let sweeper = install_with(
        &client,
        FleetSweeperOptions {
            max_databases_per_pass: budget,
            max_messages_per_database: 1,
            ..FleetSweeperOptions::default()
        },
    )
    .await;
    let busy = ["busy-a", "busy-b", "busy-c"];
    for tenant in busy {
        for id in 1..=5 {
            assert_eq!(post(&client, tenant, &format!("/orders/{id}")).await, 200);
        }
    }
    // The closed database has the newest work.
    assert_eq!(post(&client, "quiet", "/orders/100").await, 200);
    fleet.close(&key(&fleet, "quiet")).await;
    let quiet_relayed = |state: &AppState| {
        relayed(state)
            .iter()
            .any(|(tenant, _)| tenant.as_deref() == Some("quiet"))
    };

    // Each pass relays one message per database: the busy ones keep work.
    // Half of each pass goes to open databases, the rest in turn to all.
    let bound = busy.len().div_ceil(budget - budget / 2) + 1;
    for pass in 1..=bound {
        let report = sweeper.sweep_once().await;
        assert_eq!(report.visited, budget, "pass {pass}: {report:?}");
        if quiet_relayed(client.state()) {
            return;
        }
    }
    panic!("the closed database waited more than {bound} passes");
}

#[tokio::test]
async fn a_full_fleet_still_relays_the_open_databases() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 3);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install_with(
        &client,
        FleetSweeperOptions {
            max_databases_per_pass: 2,
            ..FleetSweeperOptions::default()
        },
    )
    .await;
    // The closed databases have the oldest work.
    for tenant in ["quiet-1", "quiet-2"] {
        assert_eq!(post(&client, tenant, "/orders/1").await, 200);
        fleet.close(&key(&fleet, tenant)).await;
    }
    for tenant in ["busy-a", "busy-b", "busy-c"] {
        assert_eq!(post(&client, tenant, "/orders/2").await, 200);
    }
    assert_eq!(fleet.stats().open, 3, "the fleet is full");

    let report = sweeper.sweep_once().await;
    assert_eq!(
        (report.visited, report.deferred),
        (2, 2),
        "the closed databases wait for room: {report:?}"
    );
    sweeper.sweep_once().await;
    assert_eq!(
        relayed(client.state()).len(),
        3,
        "each open database relays"
    );
}

#[tokio::test]
async fn install_refuses_a_second_sweeper_and_skips_an_app_without_a_fleet() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 4);
    let client = app(tmp.path(), fleet.clone());
    let sweeper = install(&client).await;
    assert!(format!("{sweeper:?}").starts_with("FleetSweeper"));
    assert!(format!("{:?}", sweeper.index()).starts_with("FleetPendingIndex"));
    assert!(
        FleetSweeper::install(client.state()).await.is_err(),
        "one sweeper per app"
    );

    let plain = TestApp::new().config(config()).build();
    assert!(
        FleetSweeper::install(plain.state())
            .await
            .unwrap()
            .is_none(),
        "no fleet, no sweeper"
    );
}
