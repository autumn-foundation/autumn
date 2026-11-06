use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::config::DatabaseFleetConfig;
use crate::db::fleet::{DatabaseFleet, FleetDatabase, FleetError};
use crate::db::fleet_replication::FleetReplication;
use crate::fleet_layout::{FleetDbKey, FleetMode};
use crate::replication::FileDestination;
use crate::replication::destination::DestinationError;
use crate::replication::engine::ReplicationSettings;

const BASE: &str = "app/test";

struct Host {
    _tmp: tempfile::TempDir,
    fleet: DatabaseFleet,
    destination: Arc<dyn ReplicaDestination>,
    replication: Arc<FleetReplication>,
    tombstones: Arc<FleetTombstones>,
    root: PathBuf,
    scratch: PathBuf,
}

/// Connect timeout of the test pools. The lake tests restore and load in
/// parallel; on a busy Windows runner the default 5 s is too short.
const CONNECT_TIMEOUT_SECS: u64 = 60;

fn control(dir: &Path) -> crate::db::Pool<crate::db::RuntimeConnection> {
    crate::db::create_pool(&crate::config::DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        connect_timeout_secs: CONNECT_TIMEOUT_SECS,
        ..Default::default()
    })
    .unwrap()
    .unwrap()
}

fn fleet_config(root: &Path, retention: Duration) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open: 8,
        pool_size: 2,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: retention,
        restore_check_sample: 0,
        replication_workers: 4,
        min_residency: Duration::from_secs(2),
        mmap_size: 0,
        cache_size_kib: 0,
    }
}

/// A replicated tenant fleet with tombstones. `retention`: how long a
/// deleted database's replica stays.
fn host(retention: Duration) -> Host {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("volume").join("fleet");
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let fleet = DatabaseFleet::builder(fleet_config(&root, retention))
        .connect_timeout_secs(CONNECT_TIMEOUT_SECS)
        .build()
        .unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(tmp.path().join("replicas")).unwrap());
    let clock: Arc<dyn crate::time::ClockSource> = Arc::new(crate::time::SystemClock);
    let tombstones = Arc::new(FleetTombstones::new(
        control(tmp.path()),
        Arc::clone(&clock),
    ));
    let replication = Arc::new(
        FleetReplication::new(
            Arc::clone(&destination),
            ReplicationSettings {
                database_path: PathBuf::new(),
                root: BASE.to_owned(),
                sync_interval: Duration::from_secs(1),
                snapshot_interval: Duration::from_secs(3600),
                max_wal_bytes: 4 * 1024 * 1024,
                retention: Duration::from_secs(24 * 3600),
                verify_interval: None,
            },
            Duration::from_secs(30),
            false,
            clock,
        )
        .with_tombstones(Arc::clone(&tombstones))
        .with_retention_after_delete(retention),
    );
    assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
    let root = fleet.root().to_path_buf();
    Host {
        _tmp: tmp,
        fleet,
        destination,
        replication,
        tombstones,
        root,
        scratch,
    }
}

async fn exec(db: &FleetDatabase, sql: &str) {
    let mut conn = db.pool().get().await.unwrap();
    diesel_async::RunQueryDsl::execute(diesel::sql_query(sql), &mut *conn)
        .await
        .unwrap();
}

/// Make `id`, run `statements`, and close it, so its replica holds every
/// commit.
async fn ship(host: &Host, id: &str, statements: &[&str]) -> FleetDbKey {
    let key = host.fleet.key_for(id).unwrap();
    let db = host.fleet.provision(&key).await.unwrap();
    for sql in statements {
        exec(&db, sql).await;
    }
    drop(db);
    host.fleet.close(&key).await;
    key
}

const NOTES: &str = "CREATE TABLE notes (body TEXT NOT NULL, n INTEGER, price REAL, data BLOB)";

async fn three_tenants(host: &Host) {
    ship(
        host,
        "acme",
        &[
            NOTES,
            "INSERT INTO notes VALUES ('a1', 1, 1.5, x'01'), ('a2', 2, NULL, NULL)",
        ],
    )
    .await;
    ship(
        host,
        "globex",
        &[NOTES, "INSERT INTO notes VALUES ('g1', 10, 0.1, x'ff')"],
    )
    .await;
    ship(
        host,
        "initech",
        &[
            NOTES,
            "INSERT INTO notes VALUES ('i1', 100, 2.25, NULL), ('i2', NULL, 0.2, x'0a0b'), \
             ('i3', 300, NULL, NULL)",
        ],
    )
    .await;
}

fn lake(host: &Host) -> FleetLake {
    FleetLake::new(Arc::clone(&host.destination), BASE)
        .with_tombstones(Arc::clone(&host.tombstones))
        .with_scratch_dir(&host.scratch)
}

fn ids(snapshot: &LakeSnapshot) -> Vec<String> {
    snapshot
        .tenants()
        .iter()
        .map(|t| t.tenant.as_str().to_owned())
        .collect()
}

fn skip_of(snapshot: &LakeSnapshot, name: &str) -> Option<SkipReason> {
    snapshot
        .skipped()
        .iter()
        .find(|skip| skip.name == name)
        .map(|skip| skip.reason.clone())
}

fn text(value: &LakeValue) -> String {
    value.as_str().unwrap().to_owned()
}

#[derive(diesel::QueryableByName)]
struct Totals {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    rows: i64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    total: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Double>)]
    price: Option<f64>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    blobs: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    bodies: String,
}

const TOTALS: &str = "SELECT COUNT(*) AS rows, SUM(n) AS total, SUM(price) AS price, \
                      COUNT(data) AS blobs, group_concat(body, ',' ORDER BY body) AS bodies \
                      FROM notes";

// ── AC: the lake matches the fleet, with no volume ──────────────────────────

#[tokio::test]
async fn three_tenants_aggregate_matches_the_fleet_with_no_volume() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;

    // The cross-tenant read on the volume.
    let per_tenant = host
        .fleet
        .each(2, |db| async move {
            let mut conn = db
                .pool()
                .get()
                .await
                .map_err(crate::AutumnError::internal_server_error)?;
            let totals: Totals =
                diesel_async::RunQueryDsl::get_result(diesel::sql_query(TOTALS), &mut *conn)
                    .await
                    .map_err(crate::AutumnError::internal_server_error)?;
            Ok((db.key().name(), totals))
        })
        .await
        .unwrap();
    let mut rows = 0;
    let mut total = 0;
    let mut price = 0.0;
    let mut blobs = 0;
    let mut bodies = Vec::new();
    let mut by_tenant = Vec::new();
    for (_, result) in per_tenant {
        let (name, t) = result.unwrap();
        rows += t.rows;
        total += t.total.unwrap_or(0);
        price += t.price.unwrap_or(0.0);
        blobs += t.blobs;
        bodies.extend(t.bodies.split(',').map(str::to_owned));
        by_tenant.push((
            name.trim_start_matches("tenant:").to_owned(),
            t.rows,
            t.total,
        ));
    }
    bodies.sort();

    // No volume: the lake reads object storage only.
    host.fleet.close_all().await;
    std::fs::remove_dir_all(&host.root).unwrap();

    let snapshot = lake(&host).materialize().await.unwrap();
    assert_eq!(ids(&snapshot), ["acme", "globex", "initech"]);
    let result = snapshot.query(TOTALS).await.unwrap();
    assert_eq!(
        result.columns,
        ["rows", "total", "price", "blobs", "bodies"]
    );
    let row = &result.rows[0];
    assert_eq!(row[0].as_i64(), Some(rows));
    assert_eq!(row[1].as_i64(), Some(total));
    assert!((row[2].as_f64().unwrap() - price).abs() < 1e-9);
    assert_eq!(row[3].as_i64(), Some(blobs));
    assert_eq!(text(&row[4]), bodies.join(","));

    let grouped = snapshot
        .query(&format!(
            "SELECT {TENANT_COLUMN} AS tenant, COUNT(*), SUM(n) FROM notes GROUP BY 1 ORDER BY 1"
        ))
        .await
        .unwrap();
    let lake_by_tenant: Vec<_> = grouped
        .rows
        .iter()
        .map(|r| (text(&r[0]), r[1].as_i64().unwrap(), r[2].as_i64()))
        .collect();
    assert_eq!(lake_by_tenant, by_tenant);

    drop(snapshot);
    assert!(
        !host.root.exists(),
        "the lake made nothing under the fleet root"
    );
}

#[tokio::test]
async fn the_one_shot_query_matches_the_snapshot() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let rows = lake(&host)
        .query("SELECT COUNT(*) FROM notes")
        .await
        .unwrap();
    assert_eq!(rows.rows, vec![vec![LakeValue::Integer(6)]]);
    assert_eq!(
        std::fs::read_dir(&host.scratch).unwrap().count(),
        0,
        "the one-shot query removes its lake"
    );
}

#[tokio::test]
async fn more_tenants_than_the_attach_cap_load() {
    let host = host(Duration::ZERO);
    for i in 0..12 {
        ship(
            &host,
            &format!("t{i:02}"),
            &[NOTES, "INSERT INTO notes (body) VALUES ('x')"],
        )
        .await;
    }
    let snapshot = lake(&host).with_workers(3).materialize().await.unwrap();
    assert_eq!(snapshot.tenants().len(), 12);
    let rows = snapshot.query("SELECT COUNT(*) FROM notes").await.unwrap();
    assert_eq!(rows.rows[0][0], LakeValue::Integer(12));
    // Rows go in tenant order, whatever the worker count.
    let order = snapshot
        .query(&format!("SELECT {TENANT_COLUMN} FROM notes ORDER BY rowid"))
        .await
        .unwrap();
    let order: Vec<String> = order.rows.iter().map(|r| text(&r[0])).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
}

// ── AC: deleted tenants are excluded ────────────────────────────────────────

#[tokio::test]
async fn a_deleted_tenant_is_excluded_by_its_row_and_by_its_marker() {
    let host = host(Duration::from_secs(3600));
    three_tenants(&host).await;
    let globex = host.fleet.key_for("globex").unwrap();
    assert!(host.fleet.delete(&globex).await.unwrap());

    // The replica stays for the retention.
    let snapshot = lake(&host).materialize().await.unwrap();
    assert_eq!(ids(&snapshot), ["acme", "initech"]);
    assert_eq!(skip_of(&snapshot, "globex"), Some(SkipReason::Deleted));
    let rows = snapshot
        .query("SELECT COUNT(*) FROM notes WHERE body = 'g1'")
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], LakeValue::Integer(0));

    // A host with no control database reads the marker.
    let snapshot = FleetLake::new(Arc::clone(&host.destination), BASE)
        .markers_only()
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    assert_eq!(ids(&snapshot), ["acme", "initech"]);
    assert_eq!(skip_of(&snapshot, "globex"), Some(SkipReason::Deleted));
}

#[tokio::test]
async fn a_tombstone_row_with_no_marker_yet_excludes() {
    let host = host(Duration::from_secs(3600));
    three_tenants(&host).await;
    // The window between the row and the marker of a delete.
    host.tombstones
        .record("tenant:globex", Duration::from_secs(3600), true, "test")
        .await
        .unwrap();

    let snapshot = lake(&host).materialize().await.unwrap();
    assert_eq!(skip_of(&snapshot, "globex"), Some(SkipReason::Deleted));

    // markers_only() accepts this window by name.
    let snapshot = FleetLake::new(Arc::clone(&host.destination), BASE)
        .markers_only()
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    assert_eq!(ids(&snapshot), ["acme", "globex", "initech"]);
}

#[tokio::test]
async fn a_lake_with_no_tombstone_source_is_refused() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let error = FleetLake::new(Arc::clone(&host.destination), BASE)
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap_err();
    assert_eq!(error, LakeError::NoTombstoneSource);
}

#[tokio::test]
async fn a_purged_tenant_is_gone() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let globex = host.fleet.key_for("globex").unwrap();
    assert!(host.fleet.delete(&globex).await.unwrap());

    let snapshot = lake(&host).materialize().await.unwrap();
    assert_eq!(ids(&snapshot), ["acme", "initech"]);
    assert_eq!(
        skip_of(&snapshot, "globex"),
        None,
        "nothing is left to list"
    );
}

#[tokio::test]
async fn a_marker_excludes_only_generations_it_covers() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let marker = |deleted_at_ms: i64| {
        serde_json::to_vec(&super::super::fleet_tombstones::Marker {
            deleted_at_ms,
            purge_after_ms: deleted_at_ms,
        })
        .unwrap()
    };
    // A marker older than the generation: a database made again.
    host.destination
        .put(&format!("{BASE}/fleet/tenant/acme/tombstone"), &marker(1))
        .unwrap();
    // A marker newer than the generation: a deleted database.
    host.destination
        .put(
            &format!("{BASE}/fleet/tenant/globex/tombstone"),
            // 2096: after every generation.
            &marker(4_000_000_000_000),
        )
        .unwrap();

    let snapshot = lake(&host).materialize().await.unwrap();
    assert_eq!(ids(&snapshot), ["acme", "initech"]);
    assert_eq!(skip_of(&snapshot, "globex"), Some(SkipReason::Deleted));
}

#[tokio::test]
async fn an_unreadable_marker_fails_the_load() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    host.destination
        .put(&format!("{BASE}/fleet/tenant/acme/tombstone"), b"not json")
        .unwrap();
    let error = lake(&host).materialize().await.unwrap_err();
    assert!(
        matches!(error, LakeError::Marker { ref tenant, .. } if tenant == "acme"),
        "{error}"
    );
    assert_eq!(std::fs::read_dir(&host.scratch).unwrap().count(), 0);
}

#[test]
fn decide_matches_the_tombstone_rule() {
    let at = |ms| DateTime::from_timestamp_millis(ms).unwrap();
    // `included` in verification/fleet_lake.rs, before the plan.
    assert_eq!(
        decide(true, None, Some(at(5))),
        Decision::Skip(Skip::Deleted)
    );
    assert_eq!(
        decide(false, Some(at(5)), Some(at(5))),
        Decision::Skip(Skip::Deleted)
    );
    assert_eq!(decide(false, Some(at(5)), Some(at(6))), Decision::Plan);
    assert_eq!(
        decide(false, Some(at(5)), None),
        Decision::Skip(Skip::Deleted),
        "a marker with no generation left is a purge in progress"
    );
    assert_eq!(
        decide(false, None, None),
        Decision::Skip(Skip::Unreplicated)
    );
    assert_eq!(decide(false, None, Some(at(1))), Decision::Plan);
}

// ── Discovery ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn invalid_names_and_slot_databases_are_skipped() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    host.destination
        .put(&format!("{BASE}/fleet/tenant/Bad.Name/generations/x"), b"x")
        .unwrap();
    host.destination
        .put(&format!("{BASE}/fleet/slot/00003/generations/x"), b"x")
        .unwrap();
    let snapshot = lake(&host).materialize().await.unwrap();
    assert_eq!(ids(&snapshot), ["acme", "globex", "initech"]);
    assert_eq!(
        skip_of(&snapshot, "Bad.Name"),
        Some(SkipReason::InvalidName)
    );
    assert_eq!(skip_of(&snapshot, "00003"), Some(SkipReason::SlotDatabase));
}

#[tokio::test]
async fn a_slot_fleet_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(tmp.path()).unwrap());
    destination
        .put(&format!("{BASE}/fleet/slot/00001/generations/x"), b"x")
        .unwrap();
    let error = FleetLake::new(destination, BASE)
        .markers_only()
        .with_scratch_dir(tmp.path())
        .materialize()
        .await
        .unwrap_err();
    assert_eq!(error, LakeError::SlotFleet);
}

#[tokio::test]
async fn an_empty_destination_gives_an_empty_lake() {
    let tmp = tempfile::tempdir().unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(tmp.path().join("replicas")).unwrap());
    let snapshot = FleetLake::new(destination, BASE)
        .markers_only()
        .with_scratch_dir(tmp.path())
        .materialize()
        .await
        .unwrap();
    assert!(snapshot.tenants().is_empty());
    assert!(snapshot.skipped().is_empty());
}

/// Counts `list` calls, so discovery stays one listing.
struct Counting {
    inner: Arc<dyn ReplicaDestination>,
    lists: AtomicUsize,
}

impl ReplicaDestination for Counting {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        self.inner.get(key)
    }
    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn discovery_lists_the_destination_once() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let counting = Arc::new(Counting {
        inner: Arc::clone(&host.destination),
        lists: AtomicUsize::new(0),
    });
    let snapshot = FleetLake::new(Arc::clone(&counting) as Arc<dyn ReplicaDestination>, BASE)
        .with_tombstones(Arc::clone(&host.tombstones))
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    assert_eq!(snapshot.tenants().len(), 3);
    assert_eq!(AtomicUsize::load(&counting.lists, Ordering::SeqCst), 1);
}

/// A fleet listing older than the replica: it hides the newest segment,
/// or it gives a listing taken before.
struct Stale {
    inner: Arc<dyn ReplicaDestination>,
    hide_newest_segment: bool,
    frozen: Option<Vec<String>>,
}

impl ReplicaDestination for Stale {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        self.inner.get(key)
    }
    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        if !prefix.ends_with("/fleet/") {
            return self.inner.list(prefix);
        }
        if let Some(frozen) = &self.frozen {
            return Ok(frozen.clone());
        }
        let mut keys = self.inner.list(prefix)?;
        if self.hide_newest_segment
            && let Some(at) = keys
                .iter()
                .rposition(|key| Path::new(key).extension().is_some_and(|ext| ext == "seg"))
        {
            keys.remove(at);
        }
        Ok(keys)
    }
    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn a_tenant_that_ships_after_the_listing_is_planned_again() {
    let host = host(Duration::ZERO);
    // Ship segments while the database is open.
    let key = host.fleet.key_for("acme").unwrap();
    let db = host.fleet.provision(&key).await.unwrap();
    for sql in [NOTES, "INSERT INTO notes (body) VALUES ('a1')"] {
        exec(&db, sql).await;
        let replication = Arc::clone(&host.replication);
        crate::time::spawn_blocking(move || replication.tick_all())
            .await
            .unwrap();
    }
    exec(&db, "INSERT INTO notes (body) VALUES ('a2')").await;
    drop(db);
    host.fleet.close(&key).await;
    let prefix = format!("{BASE}/fleet/tenant/acme/");
    assert!(
        host.destination
            .list(&prefix)
            .unwrap()
            .iter()
            .any(|key| Path::new(key).extension().is_some_and(|ext| ext == "seg")),
        "the test needs a segment to hide"
    );
    let stale = Arc::new(Stale {
        inner: Arc::clone(&host.destination),
        hide_newest_segment: true,
        frozen: None,
    });
    let snapshot = FleetLake::new(stale, BASE)
        .with_tombstones(Arc::clone(&host.tombstones))
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    let rows = snapshot.query("SELECT COUNT(*) FROM notes").await.unwrap();
    assert_eq!(
        rows.rows[0][0],
        LakeValue::Integer(2),
        "the newest segment too"
    );
}

#[tokio::test]
async fn a_purge_that_ends_after_the_listing_skips_the_tenant() {
    let host = host(Duration::from_secs(3600));
    three_tenants(&host).await;
    assert!(
        host.fleet
            .delete(&host.fleet.key_for("globex").unwrap())
            .await
            .unwrap()
    );
    // The listing still has globex's replica and marker; then the purge
    // removes them all.
    let frozen = host.destination.list(&format!("{BASE}/fleet/")).unwrap();
    for key in host
        .destination
        .list(&format!("{BASE}/fleet/tenant/globex/"))
        .unwrap()
    {
        host.destination.delete(&key).unwrap();
    }
    let stale = Arc::new(Stale {
        inner: Arc::clone(&host.destination),
        hide_newest_segment: false,
        frozen: Some(frozen),
    });
    let snapshot = FleetLake::new(stale, BASE)
        .markers_only()
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    assert_eq!(ids(&snapshot), ["acme", "initech"]);
    assert_eq!(skip_of(&snapshot, "globex"), Some(SkipReason::Unreplicated));
}

// ── Failures ────────────────────────────────────────────────────────────────

fn break_snapshot(host: &Host, id: &str) {
    let prefix = format!("{BASE}/fleet/tenant/{id}/");
    for key in host.destination.list(&prefix).unwrap() {
        if key.ends_with("snapshot.db.gz") {
            host.destination.put(&key, b"not gzip").unwrap();
        }
    }
}

#[tokio::test]
async fn a_broken_replica_fails_the_load_unless_skipped() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    break_snapshot(&host, "globex");

    let error = lake(&host).materialize().await.unwrap_err();
    assert!(
        matches!(error, LakeError::Restore { ref tenant, .. } if tenant == "globex"),
        "{error}"
    );
    assert_eq!(std::fs::read_dir(&host.scratch).unwrap().count(), 0);

    let snapshot = lake(&host)
        .skip_failed_tenants()
        .materialize()
        .await
        .unwrap();
    assert_eq!(ids(&snapshot), ["acme", "initech"]);
    assert!(matches!(
        skip_of(&snapshot, "globex"),
        Some(SkipReason::Failed(_))
    ));
}

#[tokio::test]
async fn a_skipped_tenant_rolls_back_cleanly_after_a_large_copy() {
    let host = host(Duration::ZERO);
    let table = "CREATE TABLE a_big (body TEXT)";
    let fill = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40000) \
                INSERT INTO a_big SELECT printf('%0200d', i) FROM n";
    ship(&host, "acme", &[table, fill]).await;
    // More rows than the page cache holds, then a refused table.
    ship(
        &host,
        "beta",
        &[
            table,
            fill,
            &format!("CREATE TABLE z_bad ({TENANT_COLUMN} TEXT)"),
        ],
    )
    .await;
    let snapshot = lake(&host)
        .skip_failed_tenants()
        .materialize()
        .await
        .unwrap();
    assert_eq!(ids(&snapshot), ["acme"]);
    assert!(matches!(
        skip_of(&snapshot, "beta"),
        Some(SkipReason::Failed(_))
    ));
    let check = snapshot.query("PRAGMA integrity_check").await;
    assert!(check.is_err(), "a PRAGMA is not a query");
    let rows = snapshot
        .query(&format!(
            "SELECT {TENANT_COLUMN}, COUNT(*) FROM a_big GROUP BY 1"
        ))
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![
            LakeValue::Text("acme".into()),
            LakeValue::Integer(40000)
        ]]
    );
    let ok = snapshot
        .query("SELECT integrity_check FROM pragma_integrity_check")
        .await
        .unwrap();
    assert_eq!(ok.rows, vec![vec![LakeValue::Text("ok".into())]]);
}

#[tokio::test]
async fn a_misplaced_replica_fails_the_load() {
    let host = host(Duration::ZERO);
    ship(&host, "acme", &[NOTES]).await;
    // Copy acme's replica under another tenant's prefix.
    let from = format!("{BASE}/fleet/tenant/acme/");
    for key in host.destination.list(&from).unwrap() {
        let body = host.destination.get(&key).unwrap();
        let to = key.replacen("/tenant/acme/", "/tenant/evil/", 1);
        host.destination.put(&to, &body).unwrap();
    }
    let error = lake(&host).materialize().await.unwrap_err();
    assert!(
        matches!(error, LakeError::Misplaced { ref tenant, .. } if tenant == "evil"),
        "{error}"
    );
}

// ── Point in time ───────────────────────────────────────────────────────────

#[tokio::test]
async fn as_of_takes_each_tenant_at_or_before_the_target() {
    let host = host(Duration::ZERO);
    ship(
        &host,
        "acme",
        &[NOTES, "INSERT INTO notes (body) VALUES ('before')"],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let target = crate::time::ambient_now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let acme = host.fleet.open_for("acme").await.unwrap();
    exec(&acme, "INSERT INTO notes (body) VALUES ('after')").await;
    let key = acme.key().clone();
    drop(acme);
    host.fleet.close(&key).await;
    ship(&host, "later", &[NOTES]).await;

    let snapshot = lake(&host).as_of(target).materialize().await.unwrap();
    assert_eq!(ids(&snapshot), ["acme"]);
    assert!(snapshot.tenants()[0].effective <= target);
    assert_eq!(
        skip_of(&snapshot, "later"),
        Some(SkipReason::AbsentAtTarget)
    );
    let rows = snapshot.query("SELECT body FROM notes").await.unwrap();
    assert_eq!(rows.rows, vec![vec![LakeValue::Text("before".into())]]);

    let latest = lake(&host).materialize().await.unwrap();
    let rows = latest
        .query("SELECT body FROM notes ORDER BY body")
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
}

#[tokio::test]
async fn the_manifest_table_records_each_tenant() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let rows = snapshot
        .query(&format!(
            "SELECT tenant, generation, effective_ms FROM {MANIFEST_TABLE} ORDER BY tenant"
        ))
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 3);
    for (row, tenant) in rows.rows.iter().zip(snapshot.tenants()) {
        assert_eq!(text(&row[0]), tenant.tenant.as_str());
        assert_eq!(text(&row[1]), tenant.generation);
        assert_eq!(row[2].as_i64(), Some(tenant.effective.timestamp_millis()));
    }
}

// ── Tables and columns ──────────────────────────────────────────────────────

#[tokio::test]
async fn columns_union_by_name_and_keep_their_storage_class() {
    let host = host(Duration::ZERO);
    ship(
        &host,
        "acme",
        &[
            "CREATE TABLE items (name TEXT, qty INTEGER)",
            "INSERT INTO items VALUES ('bolt', 7)",
        ],
    )
    .await;
    ship(
        &host,
        "globex",
        &[
            "CREATE TABLE items (qty TEXT, colour TEXT)",
            "INSERT INTO items VALUES ('007', 'red')",
        ],
    )
    .await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let rows = snapshot
        .query(&format!(
            "SELECT {TENANT_COLUMN}, name, qty, typeof(qty), colour FROM items ORDER BY 1"
        ))
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![
                LakeValue::Text("acme".into()),
                LakeValue::Text("bolt".into()),
                LakeValue::Integer(7),
                LakeValue::Text("integer".into()),
                LakeValue::Null,
            ],
            vec![
                LakeValue::Text("globex".into()),
                LakeValue::Null,
                LakeValue::Text("007".into()),
                LakeValue::Text("text".into()),
                LakeValue::Text("red".into()),
            ],
        ]
    );
}

#[tokio::test]
async fn a_source_column_named_like_the_tag_is_refused() {
    let host = host(Duration::ZERO);
    ship(
        &host,
        "acme",
        &[&format!("CREATE TABLE bad ({TENANT_COLUMN} TEXT)")],
    )
    .await;
    let error = lake(&host).materialize().await.unwrap_err();
    assert_eq!(
        error,
        LakeError::ReservedColumn {
            tenant: "acme".into(),
            table: "bad".into()
        }
    );
}

#[tokio::test]
async fn framework_tables_and_views_are_not_loaded() {
    let host = host(Duration::ZERO);
    ship(
        &host,
        "acme",
        &[
            NOTES,
            "CREATE VIEW note_bodies AS SELECT body FROM notes",
            "CREATE TABLE \"odd \"\"name\" (\"a \"\"b\" INTEGER)",
            "INSERT INTO \"odd \"\"name\" VALUES (1)",
        ],
    )
    .await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let rows = snapshot
        .query("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .await
        .unwrap();
    let names: Vec<String> = rows.rows.iter().map(|r| text(&r[0])).collect();
    assert_eq!(names, [MANIFEST_TABLE, "notes", "odd \"name"]);
    let odd = snapshot
        .query("SELECT \"a \"\"b\" FROM \"odd \"\"name\"")
        .await
        .unwrap();
    assert_eq!(odd.rows, vec![vec![LakeValue::Integer(1)]]);
}

#[tokio::test]
async fn with_tables_loads_only_those() {
    let host = host(Duration::ZERO);
    ship(&host, "acme", &[NOTES, "CREATE TABLE other (x INTEGER)"]).await;
    let snapshot = lake(&host)
        .with_tables(["notes"])
        .materialize()
        .await
        .unwrap();
    let rows = snapshot
        .query("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'other'")
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], LakeValue::Integer(0));
}

// ── Queries ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_query_runs_one_select_only() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    for sql in [
        "DELETE FROM notes",
        "SELECT 1; DELETE FROM notes",
        "ATTACH DATABASE 'x.db' AS x",
        "PRAGMA writable_schema = ON",
        "SELECT ?",
        "CREATE TABLE t (a)",
    ] {
        let error = snapshot.query(sql).await.unwrap_err();
        assert!(matches!(error, LakeError::Query(_)), "{sql}: {error}");
    }
    let rows = snapshot.query("SELECT COUNT(*) FROM notes;").await.unwrap();
    assert_eq!(rows.rows[0][0], LakeValue::Integer(6), "nothing changed");
    let rows = snapshot
        .query("SELECT COUNT(*) FROM notes; -- the total\n-- by tenant\n")
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], LakeValue::Integer(6));
    let rows = snapshot
        .query("WITH t AS (SELECT n FROM notes) SELECT MAX(n) FROM t -- comment")
        .await
        .unwrap();
    assert_eq!(rows.rows[0][0], LakeValue::Integer(300));
    // `;` and `--` inside a literal are not the end of the statement.
    let rows = snapshot
        .query("SELECT 'before; --after' /* c; */ ; -- note")
        .await
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![LakeValue::Text("before; --after".into())]]
    );
    let rows = snapshot.query("SELECT 'before; --after';").await.unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![LakeValue::Text("before; --after".into())]]
    );
    let rows = snapshot.query("VALUES (1, 'a')").await.unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![LakeValue::Integer(1), LakeValue::Text("a".into())]]
    );
}

#[tokio::test]
async fn values_round_trip() {
    let host = host(Duration::ZERO);
    ship(&host, "acme", &[NOTES]).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let rows = snapshot
        .query(
            "SELECT NULL, -9223372036854775808, 0.1 + 0.2, 1e308 * 10, -1e308 * 10, \
             'it''s' || char(10) || 'ok', x'00ff', 1.0, 'x' AS a, 'y' AS a, \
             'before' || char(0) || 'after'",
        )
        .await
        .unwrap();
    assert_eq!(rows.columns[8], "a");
    assert_eq!(rows.columns[9], "a:1");
    assert_eq!(
        rows.rows[0],
        vec![
            LakeValue::Null,
            LakeValue::Integer(i64::MIN),
            LakeValue::Real(0.1 + 0.2),
            LakeValue::Real(f64::INFINITY),
            LakeValue::Real(f64::NEG_INFINITY),
            LakeValue::Text("it's\nok".into()),
            LakeValue::Blob(vec![0, 255]),
            LakeValue::Real(1.0),
            LakeValue::Text("x".into()),
            LakeValue::Text("y".into()),
            LakeValue::Text("before\0after".into()),
        ]
    );
}

/// Each value is computed once: its type and its bytes agree.
#[tokio::test]
async fn a_random_value_is_encoded_from_one_evaluation() {
    let host = host(Duration::ZERO);
    ship(&host, "acme", &[NOTES]).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let rows = snapshot
        .query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500) \
             SELECT CASE WHEN random() % 2 = 0 THEN 'abc' ELSE x'ff' END FROM n",
        )
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 500);
    for row in &rows.rows {
        assert!(
            row[0] == LakeValue::Text("abc".into()) || row[0] == LakeValue::Blob(vec![0xff]),
            "{row:?}"
        );
    }
}

/// Rows stream: a stop after the first row ends even an endless query.
#[tokio::test]
async fn an_endless_query_stops_at_the_first_break() {
    let host = host(Duration::ZERO);
    ship(&host, "acme", &[NOTES]).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let given = snapshot.for_each_row(
            "WITH RECURSIVE n(x) AS (VALUES (1) UNION ALL SELECT x + 1 FROM n) SELECT x FROM n",
            |_, _| ControlFlow::Break(()),
        );
        let _ = sent.send(given);
    });
    let given = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the first row comes before the whole result");
    assert_eq!(given.unwrap(), 1);
}

#[tokio::test]
async fn for_each_row_streams_and_stops_early() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let mut seen = Vec::new();
    let given = snapshot
        .for_each_row("SELECT body FROM notes ORDER BY body", |columns, row| {
            assert_eq!(columns, ["body"]);
            seen.push(row[0].clone());
            if seen.len() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
    assert_eq!(given, 2);
    assert_eq!(
        seen,
        [LakeValue::Text("a1".into()), LakeValue::Text("a2".into())]
    );
}

#[test]
fn literals_parse() {
    assert_eq!(parse_literal("NULL").unwrap(), LakeValue::Null);
    assert_eq!(parse_literal("42").unwrap(), LakeValue::Integer(42));
    assert_eq!(parse_literal("-7").unwrap(), LakeValue::Integer(-7));
    assert_eq!(parse_literal("1.5").unwrap(), LakeValue::Real(1.5));
    assert_eq!(parse_literal("1.0e+20").unwrap(), LakeValue::Real(1.0e20));
    assert_eq!(
        parse_literal("9.0e+999").unwrap(),
        LakeValue::Real(f64::INFINITY)
    );
    assert_eq!(
        parse_literal("'a''b'").unwrap(),
        LakeValue::Text("a'b".into())
    );
    assert_eq!(parse_literal("''").unwrap(), LakeValue::Text(String::new()));
    assert_eq!(
        parse_literal("X'00FF'").unwrap(),
        LakeValue::Blob(vec![0, 255])
    );
    assert_eq!(parse_literal("X''").unwrap(), LakeValue::Blob(Vec::new()));
    assert_eq!(
        parse_literal("T610062").unwrap(),
        LakeValue::Text("a\0b".into())
    );
    assert_eq!(parse_literal("T").unwrap(), LakeValue::Text(String::new()));
    for bad in ["", "abc", "'open", "X'0'", "X'zz'", "T0", "Tzz"] {
        assert!(parse_literal(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_statement_end_skips_literals_and_comments() {
    assert_eq!(statement("SELECT 1;"), "SELECT 1");
    assert_eq!(statement("SELECT 1 ; -- a\n/* b; */ ;\n"), "SELECT 1");
    assert_eq!(statement("SELECT 'a;--b';"), "SELECT 'a;--b'");
    assert_eq!(statement("SELECT 'it''s';"), "SELECT 'it''s'");
    assert_eq!(
        statement("SELECT \"a;b\", [c;d], `e;f`;"),
        "SELECT \"a;b\", [c;d], `e;f`"
    );
    assert_eq!(statement("SELECT 1; SELECT 2;"), "SELECT 1; SELECT 2");
    assert_eq!(statement("SELECT 'open"), "SELECT 'open");
    assert_eq!(statement("SELECT 'é';"), "SELECT 'é'");
    assert_eq!(statement("  ;  "), "");
}

#[test]
fn identifiers_are_quoted() {
    assert_eq!(quote_ident("notes"), "\"notes\"");
    assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
}

// ── Scratch ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scratch_is_private_and_removed_on_drop() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let dir = snapshot.path().parent().unwrap().to_path_buf();
    assert!(dir.starts_with(host.scratch.canonicalize().unwrap()));
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "only lake.db stays after the load"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    drop(snapshot);
    assert!(!dir.exists());
}

#[tokio::test]
async fn persist_writes_a_standalone_copy() {
    #[derive(diesel::QueryableByName)]
    struct N {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    let out = host.scratch.join("kept.db");
    snapshot.persist(&out).unwrap();
    assert!(snapshot.persist(&out).is_err(), "never replaces a file");
    drop(snapshot);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let mut conn =
        crate::db::establish_sqlite_migration_connection(&out.display().to_string()).unwrap();
    let n: N = diesel::RunQueryDsl::get_result(
        diesel::sql_query("SELECT COUNT(*) AS n FROM notes"),
        &mut conn,
    )
    .unwrap();
    assert_eq!(n.n, 6);
}

/// A name that starts with `file:` is a file name, not a URI.
#[cfg(unix)]
#[tokio::test]
async fn persist_writes_the_named_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let snapshot = lake(&host).materialize().await.unwrap();
    std::fs::create_dir(host.scratch.join("sub")).unwrap();
    let out = host.scratch.join("file:sub%2Fkept.db");
    snapshot.persist(&out).unwrap();
    assert!(!host.scratch.join("sub/kept.db").exists());
    assert!(std::fs::metadata(&out).unwrap().len() > 0);
    let mode = std::fs::metadata(&out).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn the_fleet_lake_refuses_a_scratch_dir_under_the_root() {
    let host = host(Duration::ZERO);
    three_tenants(&host).await;
    let inside = host.root.join("analytics");
    std::fs::create_dir_all(&inside).unwrap();
    let error = host
        .fleet
        .lake()
        .unwrap()
        .with_scratch_dir(&inside)
        .materialize()
        .await
        .unwrap_err();
    assert!(matches!(error, LakeError::UnderRoot { .. }), "{error}");

    #[cfg(unix)]
    {
        let link = host.scratch.join("link");
        std::os::unix::fs::symlink(&host.root, &link).unwrap();
        let error = host
            .fleet
            .lake()
            .unwrap()
            .with_scratch_dir(&link)
            .materialize()
            .await
            .unwrap_err();
        assert!(matches!(error, LakeError::UnderRoot { .. }), "{error}");
    }

    // A kept copy under the root is refused too.
    let snapshot = host
        .fleet
        .lake()
        .unwrap()
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    let error = snapshot.persist(&host.root.join("kept.db")).unwrap_err();
    assert!(matches!(error, LakeError::UnderRoot { .. }), "{error}");
    assert!(!host.root.join("kept.db").exists());
    drop(snapshot);

    // The fleet lake carries the fleet's tombstones.
    let snapshot = host
        .fleet
        .lake()
        .unwrap()
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    assert_eq!(snapshot.tenants().len(), 3);
}

#[tokio::test]
async fn a_fleet_with_no_replication_has_no_lake() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = DatabaseFleet::builder(fleet_config(&tmp.path().join("fleet"), Duration::ZERO))
        .build()
        .unwrap();
    assert!(matches!(
        fleet.lake(),
        Err(FleetError::NotReplicated { .. })
    ));
}
