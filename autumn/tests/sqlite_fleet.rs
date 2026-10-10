//! A `SQLite` fleet behind the sharding extractors, end to end (ADR 0019).
//!
//! The app code is the same code a Postgres-sharded app runs: `ShardedDb`,
//! `Shards` and a `#[repository(tenant_scoped, sharded)]` repository. The
//! shard set is a [`DatabaseFleet`], so each tenant (or slot) gets its own
//! `SQLite` file, opened and migrated on first use.
//!
//! Needs `test-support` for [`TestApp`]. Run explicitly:
//!
//! ```sh
//! cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_fleet
//! ```
#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::path::Path as FsPath;

use autumn_web::config::{AutumnConfig, DatabaseConfig, DatabaseFleetConfig};
use autumn_web::db::fleet::DatabaseFleet;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::fleet_layout::{FleetDbKey, FleetMode};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sharding::{CrossShard, ShardedDb, Shards};
use autumn_web::test::TestApp;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/fleet_migrations");

mod schema {
    autumn_web::reexports::diesel::table! {
        fleet_notes (id) {
            id -> Int8,
            body -> Text,
            tenant_id -> Nullable<Text>,
        }
    }
}

use schema::fleet_notes;

#[autumn_web::model]
pub struct FleetNote {
    #[id]
    pub id: i64,
    pub body: String,
    pub tenant_id: Option<String>,
}

#[autumn_web::repository(FleetNote, tenant_scoped, sharded)]
pub trait FleetNoteRepository {}

#[derive(diesel::QueryableByName)]
struct Body {
    #[diesel(sql_type = diesel::sql_types::Text)]
    body: String,
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// Writes through `ShardedDb`: raw SQL on the tenant's own database.
#[post("/raw/{body}")]
async fn raw_write(mut db: ShardedDb, Path(body): Path<String>) -> AutumnResult<String> {
    diesel::sql_query("INSERT INTO fleet_notes (body, tenant_id) VALUES (?, NULL)")
        .bind::<diesel::sql_types::Text, _>(&body)
        .execute(&mut *db)
        .await?;
    Ok(db.shard().to_owned())
}

#[get("/raw")]
async fn raw_read(mut db: ShardedDb) -> AutumnResult<Json<Vec<String>>> {
    let rows: Vec<Body> = diesel::sql_query("SELECT body FROM fleet_notes ORDER BY id")
        .load(&mut *db)
        .await?;
    Ok(Json(rows.into_iter().map(|r| r.body).collect()))
}

/// Writes through the generated sharded repository.
#[post("/notes/{body}")]
async fn repo_write(
    repo: PgFleetNoteRepository,
    tenant: autumn_web::tenancy::Tenant,
    Path(body): Path<String>,
) -> AutumnResult<String> {
    let note = repo
        .save(&NewFleetNote {
            body,
            tenant_id: Some(tenant.0),
        })
        .await?;
    Ok(note.body)
}

#[get("/notes")]
async fn repo_read(repo: PgFleetNoteRepository) -> AutumnResult<Json<Vec<String>>> {
    let mut notes: Vec<String> = repo.find_all().await?.into_iter().map(|n| n.body).collect();
    notes.sort();
    Ok(Json(notes))
}

/// Admin read across every database on disk.
#[get("/admin/notes")]
async fn admin_notes(
    CrossShard(repo): CrossShard<PgFleetNoteRepository>,
) -> AutumnResult<Json<Vec<String>>> {
    let mut notes: Vec<String> = repo.find_all().await?.into_iter().map(|n| n.body).collect();
    notes.sort();
    Ok(Json(notes))
}

/// `Shards::each_shard` over the fleet: one entry per database file.
#[get("/admin/counts")]
async fn admin_counts(shards: Shards) -> AutumnResult<Json<Vec<(String, i64)>>> {
    let results = shards
        .each_shard(|shard, mut db| {
            let name = shard.name().to_owned();
            async move {
                let count: Count = diesel::sql_query("SELECT COUNT(*) AS n FROM fleet_notes")
                    .get_result(&mut *db)
                    .await?;
                Ok((name, count.n))
            }
        })
        .await;
    let mut out = Vec::new();
    for (_, result) in results {
        out.push(result?);
    }
    Ok(Json(out))
}

/// Provisioning a tenant from a handler.
#[post("/admin/tenants/{id}")]
async fn provision(shards: Shards, Path(id): Path<String>) -> AutumnResult<String> {
    let fleet = shards.fleet().expect("fleet-backed");
    let key = fleet.key_for(&id)?;
    let db = fleet.provision(&key).await?;
    Ok(db.key().name())
}

#[get("/admin/db/{name}")]
async fn db_by_name(shards: Shards, Path(name): Path<String>) -> AutumnResult<String> {
    let mut db = shards.db_on(&name).await?;
    let count: Count = diesel::sql_query("SELECT COUNT(*) AS n FROM fleet_notes")
        .get_result(&mut *db)
        .await?;
    Ok(count.n.to_string())
}

fn control_pool(dir: &FsPath) -> Pool<RuntimeConnection> {
    create_pool(&DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .expect("control pool builds")
    .expect("url configured")
}

fn fleet(dir: &FsPath, mode: FleetMode, create_on_demand: bool) -> DatabaseFleet {
    DatabaseFleet::builder(DatabaseFleetConfig {
        mode,
        root: dir.join("fleet").display().to_string(),
        path: None,
        max_open: 4,
        pool_size: 2,
        create_on_demand: Some(create_on_demand),
        idle_close_secs: 0,
        restore_missing: false,
    })
    .migrations("app", MIGRATIONS)
    .migrations(
        "version-history",
        autumn_web::version_history::VERSION_HISTORY_MIGRATIONS,
    )
    .build()
    .expect("fleet builds")
}

fn header_tenancy() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.profile = Some("test".into());
    config.security.csrf.enabled = false;
    config.tenancy.enabled = true;
    config.tenancy.source = "header".to_owned();
    config.tenancy.header_name = "x-tenant-id".to_owned();
    // Admin routes span tenants, so they run without one.
    config.tenancy.public_paths = vec!["/admin".to_owned()];
    config
}

fn app(dir: &FsPath, fleet: DatabaseFleet) -> autumn_web::test::TestClient {
    TestApp::new()
        .config(header_tenancy())
        .routes(routes![
            raw_write,
            raw_read,
            repo_write,
            repo_read,
            admin_notes,
            admin_counts,
            provision,
            db_by_name
        ])
        .with_db(control_pool(dir))
        .with_fleet(fleet)
        .build()
}

#[tokio::test]
async fn each_tenant_writes_to_its_own_database_file() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), FleetMode::Tenant, true);
    let client = app(tmp.path(), fleet.clone());

    for (tenant, body) in [("acme", "a1"), ("acme", "a2"), ("globex", "g1")] {
        client
            .post(&format!("/raw/{body}"))
            .header("x-tenant-id", tenant)
            .send()
            .await
            .assert_status(200)
            .assert_body_eq(&format!("tenant:{tenant}"));
    }
    let acme: Vec<String> = client
        .get("/raw")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .json();
    assert_eq!(acme, vec!["a1", "a2"]);
    let globex: Vec<String> = client
        .get("/raw")
        .header("x-tenant-id", "globex")
        .send()
        .await
        .json();
    assert_eq!(
        globex,
        vec!["g1"],
        "no tenant_id filter, yet no leak: separate files"
    );

    let keys = fleet.list().await.unwrap();
    assert_eq!(
        keys.iter().map(FleetDbKey::name).collect::<Vec<_>>(),
        vec!["tenant:acme", "tenant:globex"]
    );
    for key in &keys {
        assert!(fleet.path_of(key).is_file());
        assert!(
            fleet
                .path_of(key)
                .starts_with(tmp.path().join("fleet").canonicalize().unwrap())
        );
    }
    // The control database holds no tenant rows.
    assert!(!tmp.path().join("fleet").join("control.db").exists());
}

#[tokio::test]
async fn the_generated_sharded_repository_routes_to_the_tenant_database() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), FleetMode::Tenant, true);
    let client = app(tmp.path(), fleet);

    for (tenant, body) in [("acme", "plan"), ("globex", "launch"), ("acme", "ship")] {
        client
            .post(&format!("/notes/{body}"))
            .header("x-tenant-id", tenant)
            .send()
            .await
            .assert_status(200);
    }
    let acme: Vec<String> = client
        .get("/notes")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .json();
    assert_eq!(acme, vec!["plan", "ship"]);

    // CrossShard fans out over every database on disk.
    let all: Vec<String> = client.get("/admin/notes").send().await.json();
    assert_eq!(all, vec!["launch", "plan", "ship"]);

    let counts: Vec<(String, i64)> = client.get("/admin/counts").send().await.json();
    assert_eq!(
        counts,
        vec![
            ("tenant:acme".to_owned(), 2),
            ("tenant:globex".to_owned(), 1)
        ]
    );
    client
        .get("/admin/db/tenant:globex")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("1");
}

#[tokio::test]
async fn tenant_ids_that_cannot_name_a_file_are_rejected_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let client = app(tmp.path(), fleet(tmp.path(), FleetMode::Tenant, true));
    for bad in ["..", "Acme", "a.b", "con"] {
        client
            .get("/raw")
            .header("x-tenant-id", bad)
            .send()
            .await
            .assert_status(400);
    }
    assert!(
        std::fs::read_dir(tmp.path().join("fleet"))
            .unwrap()
            .next()
            .is_none(),
        "nothing was created for a refused id"
    );
}

#[tokio::test]
async fn an_unprovisioned_tenant_gets_404_until_provisioned() {
    let tmp = tempfile::tempdir().unwrap();
    let client = app(tmp.path(), fleet(tmp.path(), FleetMode::Tenant, false));
    client
        .get("/raw")
        .header("x-tenant-id", "initech")
        .send()
        .await
        .assert_status(404);
    client
        .post("/admin/tenants/initech")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("tenant:initech");
    client
        .post("/admin/tenants/initech")
        .send()
        .await
        .assert_status(409);
    client
        .get("/raw")
        .header("x-tenant-id", "initech")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("[]");
    client
        .get("/admin/db/tenant:nobody")
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn a_slot_fleet_shares_one_file_per_slot_and_isolates_by_tenant_id() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), FleetMode::Slot, true);
    let client = app(tmp.path(), fleet.clone());

    for (tenant, body) in [("acme", "a"), ("globex", "g")] {
        let shard = client
            .post(&format!("/notes/{body}"))
            .header("x-tenant-id", tenant)
            .send()
            .await
            .assert_status(200)
            .text();
        assert_eq!(shard, body);
    }
    // tenant_scoped keeps tenants apart even when they share a slot file.
    let acme: Vec<String> = client
        .get("/notes")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .json();
    assert_eq!(acme, vec!["a"]);

    let slot = |t: &str| autumn_web::sharding::slot_for_key(t.into()).0;
    let mut expected: Vec<FleetDbKey> = ["acme", "globex"]
        .iter()
        .map(|t| FleetDbKey::slot(slot(t)).unwrap())
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(fleet.list().await.unwrap(), expected);
    let name = client
        .post("/raw/x")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .text();
    assert_eq!(name, format!("slot:{:05}", slot("acme")));
}

#[tokio::test]
async fn the_fleet_health_indicator_reports_the_counters() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = header_tenancy();
    config.health.detailed = true;
    let client = TestApp::new()
        .config(config)
        .routes(routes![raw_write])
        .with_db(control_pool(tmp.path()))
        .with_fleet(fleet(tmp.path(), FleetMode::Tenant, true))
        .build();
    client
        .post("/raw/x")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .assert_status(200);
    let health: serde_json::Value = client.get("/actuator/health").send().await.json();
    let fleet = &health["components"]["db:fleet"];
    assert_eq!(fleet["status"], "UP", "{health}");
    assert_eq!(fleet["details"]["mode"], "tenant");
    assert_eq!(fleet["details"]["open"], 1);
    assert_eq!(fleet["details"]["created_total"], 1);
}
