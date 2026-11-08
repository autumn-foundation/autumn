//! Tests of [`DatabaseFleet::restore_from_file`]: restore one database from
//! a local backup file.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diesel_async::RunQueryDsl as _;

use super::sqlite::{IDENTITY_TABLE, read_epoch_stamp, write_epoch_stamp};
use super::{
    DatabaseFleet, FleetDatabase, FleetError, FleetLifecycle, LocalRestore, LocalRestoreOutcome,
};
use crate::config::DatabaseFleetConfig;
use crate::db::fleet_ownership::{FleetOwnership, MemoryOwnership, OwnerId, SlotRange};
use crate::db::fleet_replication::FleetReplication;
use crate::db::fleet_tombstones::FleetTombstones;
use crate::fleet_layout::{FleetDbKey, FleetMode};
use crate::replication::{FileDestination, ReplicaDestination, ReplicationSettings};

/// The longest a test waits for a call that must not hang.
const NOT_STUCK: Duration = Duration::from_secs(60);

fn config(root: &Path, retention: Duration) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open: 8,
        max_resident_bytes: 0,
        pool_size: 2,
        create_on_demand: None,
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: retention,
        restore_check_sample: 0,
        replication_workers: 4,
        min_residency: Duration::ZERO,
        mmap_size: 0,
        cache_size_kib: 0,
    }
}

/// A tenant fleet with the version-history set. `apply` is the boot's
/// migration policy.
fn fleet_with(root: &Path, apply: bool) -> DatabaseFleet {
    DatabaseFleet::builder(config(root, Duration::ZERO))
        .min_residency(Duration::ZERO)
        .connect_timeout_secs(30)
        .apply_migrations_on_open(apply)
        .migrations(
            "version-history",
            crate::version_history::VERSION_HISTORY_MIGRATIONS,
        )
        .build()
        .unwrap()
}

fn fleet(root: &Path) -> DatabaseFleet {
    fleet_with(root, true)
}

async fn exec(db: &FleetDatabase, sql: &str) {
    let mut conn = db.pool().get().await.unwrap();
    diesel::sql_query(sql).execute(&mut *conn).await.unwrap();
}

async fn bodies(db: &FleetDatabase) -> Vec<String> {
    #[derive(diesel::QueryableByName)]
    struct Body {
        #[diesel(sql_type = diesel::sql_types::Text)]
        body: String,
    }
    let mut conn = db.pool().get().await.unwrap();
    diesel::sql_query("SELECT body FROM notes ORDER BY rowid")
        .load::<Body>(&mut *conn)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.body)
        .collect()
}

async fn table_exists(db: &FleetDatabase, table: &str) -> bool {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = db.pool().get().await.unwrap();
    let count: Count = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind::<diesel::sql_types::Text, _>(table)
    .get_result(&mut *conn)
    .await
    .unwrap();
    count.n == 1
}

/// Provision `id` with one note, `kept`, and back it up to `dir/<id>.db`.
async fn backed_up(fleet: &DatabaseFleet, id: &str, dir: &Path) -> (FleetDbKey, PathBuf) {
    let key = fleet.key_for(id).unwrap();
    let db = fleet.provision(&key).await.unwrap();
    exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
    exec(&db, "INSERT INTO notes (body) VALUES ('kept')").await;
    drop(db);
    let backup = dir.join(format!("{id}.db"));
    fleet.backup(&key, &backup).await.unwrap();
    (key, backup)
}

/// Add a note after the backup.
async fn write_later(fleet: &DatabaseFleet, key: &FleetDbKey) {
    let db = fleet.open(key).await.unwrap();
    exec(&db, "INSERT INTO notes (body) VALUES ('later')").await;
}

/// No staging file or directory is left under `root`.
fn no_staging_files(root: &Path) -> bool {
    walk(root).iter().all(|path| {
        let text = path.to_string_lossy();
        !text.contains(".restoring-") && !text.contains(".importing-")
    })
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            paths.extend(walk(&path));
        }
        paths.push(path);
    }
    paths
}

fn sqlite_exec(path: &Path, sql: &str) {
    use diesel::Connection as _;
    use diesel::connection::SimpleConnection as _;
    diesel::SqliteConnection::establish(&path.display().to_string())
        .unwrap()
        .batch_execute(sql)
        .unwrap();
}

async fn restore(
    fleet: &DatabaseFleet,
    key: &FleetDbKey,
    src: &Path,
    opts: LocalRestore,
) -> Result<LocalRestoreOutcome, FleetError> {
    tokio::time::timeout(NOT_STUCK, fleet.restore_from_file(key, src, opts))
        .await
        .expect("the restore did not hang")
}

// ── Round trip ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_round_trip_brings_back_the_rows_of_the_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    write_later(&fleet, &key).await;

    let outcome = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    assert!(outcome.replaced, "a database was there before");
    assert_eq!(outcome.rekeyed_from, None);
    assert!(!outcome.tombstone_cleared);
    assert_eq!(outcome.shipped, None, "no replication: nothing ships");
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    assert!(backup.is_file(), "the source stays in place");
    assert!(no_staging_files(fleet.root()));
    drop(db);
    fleet.close_all().await;
}

#[tokio::test]
async fn without_replication_the_restore_is_not_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    assert!(fleet.replication().is_none());
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;

    let restored = restore(&fleet, &key, &backup, LocalRestore::new()).await;

    assert!(
        !matches!(restored, Err(FleetError::NotReplicated { .. })),
        "{restored:?}"
    );
    restored.unwrap();
    fleet.close_all().await;
}

#[tokio::test]
async fn a_key_with_no_file_is_created_from_the_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    assert!(fleet.delete(&key).await.unwrap());

    let outcome = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    assert!(!outcome.replaced);
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    fleet.close_all().await;
}

/// The restored file must not read rows from a `-wal` of the old file.
#[tokio::test]
async fn no_sidecar_of_the_old_file_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    write_later(&fleet, &key).await;
    fleet.close(&key).await;
    let path = fleet.path_of(&key);
    for sidecar in crate::fleet_layout::sidecar_paths(&path) {
        std::fs::write(&sidecar, b"stale").unwrap();
    }

    restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    for sidecar in crate::fleet_layout::sidecar_paths(&path) {
        assert!(!sidecar.exists(), "{} stays", sidecar.display());
    }
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    fleet.close_all().await;
}

// ── Identity ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_backup_of_another_key_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (_, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    let (globex, _) = backed_up(&fleet, "globex", tmp.path()).await;
    write_later(&fleet, &globex).await;

    let refused = restore(&fleet, &globex, &backup, LocalRestore::new())
        .await
        .unwrap_err();

    assert!(
        matches!(&refused, FleetError::Restore { detail, .. } if detail.contains("tenant:acme")),
        "{refused}"
    );
    let db = fleet.open(&globex).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept", "later"], "the live file stays");
    assert!(no_staging_files(fleet.root()));
    drop(db);
    fleet.close_all().await;
}

#[tokio::test]
async fn allow_rekey_restores_a_backup_of_another_key() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (_, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    let globex = fleet.key_for("globex").unwrap();

    let outcome = restore(
        &fleet,
        &globex,
        &backup,
        LocalRestore::new().allow_rekey(true),
    )
    .await
    .unwrap();

    assert_eq!(outcome.rekeyed_from.as_deref(), Some("tenant:acme"));
    assert_eq!(
        fleet.inspect(&globex).await.unwrap().identity.as_deref(),
        Some("tenant:globex"),
        "the identity row records the new key"
    );
    // The open checks the identity row: it passes.
    let db = fleet.open(&globex).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    fleet.close_all().await;
}

// ── Bad files ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bad_file_is_refused_and_the_live_database_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    write_later(&fleet, &key).await;

    let junk = tmp.path().join("junk.db");
    std::fs::write(&junk, b"not a database, not at all, no").unwrap();
    // Every page after the first is overwritten.
    let corrupt = tmp.path().join("corrupt.db");
    let mut bytes = std::fs::read(&backup).unwrap();
    for byte in bytes.iter_mut().skip(4096) {
        *byte = 0xA5;
    }
    std::fs::write(&corrupt, bytes).unwrap();
    // A plain SQLite file records no identity: no fleet made it.
    let plain = tmp.path().join("plain.db");
    sqlite_exec(&plain, "CREATE TABLE notes (body TEXT NOT NULL);");
    // An identity "table" that is a view answers any name.
    let forged = tmp.path().join("forged.db");
    sqlite_exec(
        &forged,
        &format!("CREATE VIEW {IDENTITY_TABLE} AS SELECT 1 AS id, 'tenant:acme' AS name;"),
    );

    let sources = [junk, corrupt, plain, forged, tmp.path().join("absent.db")];
    // `allow_rekey` accepts another key, never a file with no identity.
    for opts in [LocalRestore::new(), LocalRestore::new().allow_rekey(true)] {
        for source in &sources {
            let refused = restore(&fleet, &key, source, opts.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(refused, FleetError::Restore { .. }),
                "{}: {refused}",
                source.display()
            );
            let db = fleet.open(&key).await.unwrap();
            assert_eq!(bodies(&db).await, ["kept", "later"], "the live file stays");
        }
    }
    assert!(no_staging_files(fleet.root()));
    fleet.close_all().await;
}

// ── Concurrency ─────────────────────────────────────────────────────────────

/// Holds the restore in its `release` step until the test lets it go.
#[derive(Default)]
struct Gate {
    armed: Mutex<Option<(Sender<()>, Receiver<()>)>>,
}

impl FleetLifecycle for Gate {
    fn release(&self, _key: &FleetDbKey) {
        let armed = self
            .armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((entered, go)) = armed {
            entered.send(()).unwrap();
            go.recv_timeout(NOT_STUCK).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_open_waits_for_the_restore_and_sees_the_restored_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    write_later(&fleet, &key).await;
    let gate = Arc::new(Gate::default());
    fleet.add_lifecycle(Arc::clone(&gate) as _);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    *gate.armed.lock().unwrap() = Some((entered_tx, go_rx));

    let restoring = {
        let (fleet, key, backup) = (fleet.clone(), key.clone(), backup.clone());
        tokio::spawn(async move {
            fleet
                .restore_from_file(&key, &backup, LocalRestore::new())
                .await
        })
    };
    crate::time::spawn_blocking(move || entered_rx.recv_timeout(NOT_STUCK))
        .await
        .unwrap()
        .expect("the restore reached its release step");
    let opening = {
        let (fleet, key) = (fleet.clone(), key.clone());
        tokio::spawn(async move {
            let db = fleet.open(&key).await?;
            Ok::<_, FleetError>(bodies(&db).await)
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!opening.is_finished(), "the open waits for the restore");

    go_tx.send(()).unwrap();
    tokio::time::timeout(NOT_STUCK, restoring)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let rows = tokio::time::timeout(NOT_STUCK, opening)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rows, ["kept"], "the open sees the restored file");
    fleet.close_all().await;
}

#[tokio::test]
async fn a_restore_is_refused_while_another_process_has_the_file_open() {
    use diesel::connection::SimpleConnection as _;
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    write_later(&fleet, &key).await;
    fleet.close(&key).await;
    let path = fleet.path_of(&key);
    let mut other =
        crate::db::establish_sqlite_migration_connection(&format!("sqlite://{}", path.display()))
            .unwrap();
    other.batch_execute("SELECT 1 FROM notes").unwrap();

    let refused = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap_err();

    assert!(
        matches!(refused, FleetError::InUseElsewhere { .. }),
        "{refused}"
    );
    drop(other);
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept", "later"]);
    assert!(no_staging_files(fleet.root()));
    drop(db);
    fleet.close_all().await;
}

#[tokio::test]
async fn a_restore_is_refused_once_shutdown_has_begun() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    fleet.close_all().await;

    let refused = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap_err();

    assert!(
        matches!(refused, FleetError::Unavailable { .. }),
        "{refused}"
    );
}

// ── Migrations ──────────────────────────────────────────────────────────────

/// A backup from a fleet with no migration sets, so the version-history set
/// is pending in it.
async fn bare_backup(dir: &Path, id: &str) -> PathBuf {
    let bare = DatabaseFleet::builder(config(&dir.join("bare"), Duration::ZERO))
        .build()
        .unwrap();
    let key = bare.key_for(id).unwrap();
    let db = bare.provision(&key).await.unwrap();
    exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
    exec(&db, "INSERT INTO notes (body) VALUES ('old schema')").await;
    drop(db);
    let backup = dir.join("bare-backup.db");
    bare.backup(&key, &backup).await.unwrap();
    bare.close_all().await;
    backup
}

#[tokio::test]
async fn the_restore_applies_pending_migrations_when_opens_do() {
    let tmp = tempfile::tempdir().unwrap();
    let backup = bare_backup(tmp.path(), "acme").await;
    let fleet = fleet(&tmp.path().join("fleet"));
    let key = fleet.key_for("acme").unwrap();
    drop(fleet.provision(&key).await.unwrap());

    let outcome = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    assert!(outcome.migrations_applied > 0);
    let db = fleet.open(&key).await.unwrap();
    assert!(table_exists(&db, "_autumn_version_history").await);
    assert_eq!(bodies(&db).await, ["old schema"]);
    drop(db);
    fleet.close_all().await;
}

#[tokio::test]
async fn pending_migrations_are_refused_when_opens_do_not_apply_them() {
    let tmp = tempfile::tempdir().unwrap();
    let backup = bare_backup(tmp.path(), "acme").await;
    let fleet = fleet_with(&tmp.path().join("fleet"), false);
    let key = fleet.key_for("acme").unwrap();
    let db = fleet.provision(&key).await.unwrap();
    exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
    exec(&db, "INSERT INTO notes (body) VALUES ('live')").await;
    drop(db);

    let refused = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, FleetError::PendingMigrations { .. }),
        "{refused}"
    );
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["live"], "the live file stays");
    drop(db);

    // The caller can ask for the migrations.
    let outcome = restore(
        &fleet,
        &key,
        &backup,
        LocalRestore::new().apply_migrations(true),
    )
    .await
    .unwrap();
    assert!(outcome.migrations_applied > 0);
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["old schema"]);
    drop(db);
    fleet.close_all().await;
}

// ── Epoch ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn without_ownership_the_backup_epoch_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    write_epoch_stamp(&backup, &key.name(), 99).unwrap();

    restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();
    fleet.close_all().await;

    assert_eq!(
        read_epoch_stamp(&fleet.path_of(&key), &key.name()).unwrap(),
        0
    );
    assert_eq!(read_epoch_stamp(&backup, &key.name()).unwrap(), 99);
}

#[tokio::test]
async fn with_ownership_the_restored_file_records_the_current_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let ownership = Arc::new(
        FleetOwnership::new(
            Arc::clone(&store) as _,
            OwnerId::new("host-a").unwrap(),
            vec![SlotRange::ALL],
            Duration::from_secs(30),
        )
        .unwrap(),
    );
    fleet.start_ownership(ownership).await.unwrap();
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    let epoch = fleet.open(&key).await.unwrap().epoch().unwrap().get();
    write_epoch_stamp(&backup, &key.name(), epoch + 50).unwrap();

    restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    let db = fleet.open(&key).await.expect("the stamp is not above ours");
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    fleet.close_all().await;
    assert_eq!(
        read_epoch_stamp(&fleet.path_of(&key), &key.name()).unwrap(),
        epoch
    );
}

// ── Replication ─────────────────────────────────────────────────────────────

struct Replicated {
    fleet: DatabaseFleet,
    tombstones: Arc<FleetTombstones>,
}

fn control(
    dir: &Path,
) -> diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection> {
    crate::db::create_pool(&crate::config::DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .unwrap()
    .unwrap()
}

/// A replicated fleet with tombstones. `retention` keeps a deleted key's
/// replica and its tombstone row.
fn replicated(dir: &Path, retention: Duration) -> Replicated {
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(dir.join("replicas")).unwrap());
    replicated_to(dir, retention, destination)
}

/// [`replicated`] with its own destination.
fn replicated_to(
    dir: &Path,
    retention: Duration,
    destination: Arc<dyn ReplicaDestination>,
) -> Replicated {
    let fleet = DatabaseFleet::builder(config(&dir.join("fleet"), retention))
        .min_residency(Duration::ZERO)
        .connect_timeout_secs(30)
        .build()
        .unwrap();
    let clock: Arc<dyn crate::time::ClockSource> = Arc::new(crate::time::SystemClock);
    let tombstones = Arc::new(FleetTombstones::new(control(dir), Arc::clone(&clock)));
    let replication = Arc::new(
        FleetReplication::new(
            destination,
            ReplicationSettings {
                database_path: PathBuf::new(),
                root: "app/local-restore".to_owned(),
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
    assert!(fleet.install_replication(replication).is_ok());
    Replicated { fleet, tombstones }
}

#[tokio::test]
async fn with_replication_a_later_replica_restore_does_not_roll_the_restore_back() {
    let tmp = tempfile::tempdir().unwrap();
    let host = replicated(tmp.path(), Duration::ZERO);
    let (key, backup) = backed_up(&host.fleet, "acme", tmp.path()).await;
    write_later(&host.fleet, &key).await;
    // The replica holds 'later' now.
    host.fleet.close(&key).await;

    let outcome = restore(&host.fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    let shipped = outcome.shipped.expect("a new base snapshot ships");
    assert!(shipped.shipped > 0, "{shipped:?}");
    host.fleet.restore_latest(&key).await.unwrap();
    let db = host.fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"], "the replica holds the restore");
    drop(db);
    host.fleet.close_all().await;
}

#[tokio::test]
async fn a_tombstoned_key_is_refused_unless_the_caller_clears_the_tombstone() {
    let tmp = tempfile::tempdir().unwrap();
    let host = replicated(tmp.path(), Duration::from_secs(3600));
    let (key, backup) = backed_up(&host.fleet, "acme", tmp.path()).await;
    host.fleet.close(&key).await;
    assert!(host.fleet.delete(&key).await.unwrap());
    assert!(host.tombstones.get(&key.name()).await.unwrap().is_some());

    let refused = restore(&host.fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, FleetError::Restore { detail, .. } if detail.contains("clear_tombstone")),
        "{refused}"
    );
    assert!(!host.fleet.exists(&key), "nothing is published");
    assert!(host.tombstones.get(&key.name()).await.unwrap().is_some());

    let outcome = restore(
        &host.fleet,
        &key,
        &backup,
        LocalRestore::new().clear_tombstone(true),
    )
    .await
    .unwrap();
    assert!(outcome.tombstone_cleared);
    assert!(outcome.shipped.is_some());
    assert!(host.tombstones.get(&key.name()).await.unwrap().is_none());
    let db = host.fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    host.fleet.close_all().await;
}

#[tokio::test]
async fn a_refused_backup_keeps_the_tombstone_and_the_replica() {
    let tmp = tempfile::tempdir().unwrap();
    let host = replicated(tmp.path(), Duration::from_secs(3600));
    let (key, _) = backed_up(&host.fleet, "acme", tmp.path()).await;
    host.fleet.close(&key).await;
    assert!(host.fleet.delete(&key).await.unwrap());
    let junk = tmp.path().join("junk.db");
    std::fs::write(&junk, b"not a database, not at all, no").unwrap();

    let refused = restore(
        &host.fleet,
        &key,
        &junk,
        LocalRestore::new().clear_tombstone(true),
    )
    .await
    .unwrap_err();

    assert!(matches!(refused, FleetError::Restore { .. }), "{refused}");
    assert!(host.tombstones.get(&key.name()).await.unwrap().is_some());
    // The replica still undoes the delete.
    host.fleet.restore_latest(&key).await.unwrap();
    let db = host.fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    host.fleet.close_all().await;
}

#[tokio::test]
async fn a_failed_swap_keeps_the_tombstone_and_the_replica() {
    use diesel::connection::SimpleConnection as _;
    let tmp = tempfile::tempdir().unwrap();
    let host = replicated(tmp.path(), Duration::from_secs(3600));
    let (key, backup) = backed_up(&host.fleet, "acme", tmp.path()).await;
    host.fleet.close(&key).await;
    assert!(host.fleet.delete(&key).await.unwrap());
    // A file another process holds open: the swap must refuse.
    let path = host.fleet.path_of(&key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(&backup, &path).unwrap();
    let mut other =
        crate::db::establish_sqlite_migration_connection(&format!("sqlite://{}", path.display()))
            .unwrap();
    // A fleet file is in WAL mode. There the guard sees another reader.
    other
        .batch_execute("PRAGMA journal_mode = WAL; SELECT 1 FROM notes;")
        .unwrap();

    let refused = restore(
        &host.fleet,
        &key,
        &backup,
        LocalRestore::new().clear_tombstone(true),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(refused, FleetError::InUseElsewhere { .. }),
        "{refused}"
    );
    assert!(
        host.tombstones.get(&key.name()).await.unwrap().is_some(),
        "the tombstone stays"
    );
    drop(other);
    std::fs::remove_file(&path).unwrap();
    // The replica still undoes the delete.
    host.fleet.restore_latest(&key).await.unwrap();
    let db = host.fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    host.fleet.close_all().await;
}

/// A file destination whose writes fail while `failing` is set.
struct Flaky {
    inner: FileDestination,
    failing: std::sync::atomic::AtomicBool,
}

impl Flaky {
    fn fail(&self) -> Result<(), crate::replication::DestinationError> {
        if std::sync::atomic::AtomicBool::load(&self.failing, std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::replication::DestinationError::Io {
                op: "put",
                detail: "the destination is down".to_owned(),
            });
        }
        Ok(())
    }
}

impl ReplicaDestination for Flaky {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), crate::replication::DestinationError> {
        self.fail()?;
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &Path) -> Result<(), crate::replication::DestinationError> {
        self.fail()?;
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>, crate::replication::DestinationError> {
        self.inner.get(key)
    }
    fn get_to_file(
        &self,
        key: &str,
        path: &Path,
    ) -> Result<(), crate::replication::DestinationError> {
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, crate::replication::DestinationError> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> Result<(), crate::replication::DestinationError> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn a_failed_ship_reports_the_error_and_keeps_the_restored_file() {
    let tmp = tempfile::tempdir().unwrap();
    let flaky = Arc::new(Flaky {
        inner: FileDestination::new(tmp.path().join("replicas")).unwrap(),
        failing: std::sync::atomic::AtomicBool::new(false),
    });
    let host = replicated_to(tmp.path(), Duration::ZERO, Arc::clone(&flaky) as _);
    let (key, backup) = backed_up(&host.fleet, "acme", tmp.path()).await;
    write_later(&host.fleet, &key).await;
    host.fleet.close(&key).await;
    flaky
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let outcome = restore(&host.fleet, &key, &backup, LocalRestore::new())
        .await
        .expect("the restore succeeds: the file is in place");

    assert!(outcome.replaced);
    assert_eq!(outcome.shipped, None);
    assert!(outcome.ship_error.is_some(), "{outcome:?}");
    let db = host.fleet.open(&key).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"]);
    drop(db);
    flaky
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    host.fleet.close_all().await;
}

#[tokio::test]
async fn a_slot_this_host_does_not_own_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fleet(&tmp.path().join("source"));
    let (key, backup) = backed_up(&source, "acme", tmp.path()).await;
    source.close_all().await;
    let other = u16::from(key.routing_slot() == 0);
    let fleet = fleet(&tmp.path().join("fleet"));
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let ownership = Arc::new(
        FleetOwnership::new(
            Arc::clone(&store) as _,
            OwnerId::new("host-a").unwrap(),
            vec![SlotRange::new(other, other).unwrap()],
            Duration::from_secs(30),
        )
        .unwrap(),
    );
    fleet.start_ownership(ownership).await.unwrap();

    let refused = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap_err();

    assert!(matches!(refused, FleetError::NotOwner { .. }), "{refused}");
    assert!(!fleet.exists(&key));
    fleet.close_all().await;
}

/// A provision that has not reported its creation must not report the
/// restored file as its own.
#[tokio::test]
async fn the_restore_takes_the_creation_mark_of_a_provision() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (key, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    fleet.lock_state().unacknowledged.insert(key.clone());

    restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap();

    assert!(!fleet.lock_state().unacknowledged.contains(&key));
    assert!(matches!(
        fleet.provision(&key).await,
        Err(FleetError::AlreadyExists { .. })
    ));
    fleet.close_all().await;
}

// ── Triggers ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_trigger_on_the_identity_table_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("fleet"));
    let (_, backup) = backed_up(&fleet, "acme", tmp.path()).await;
    // It would block the write of the new key.
    sqlite_exec(
        &backup,
        &format!(
            "CREATE TRIGGER keep BEFORE UPDATE ON {IDENTITY_TABLE} \
             BEGIN SELECT RAISE(IGNORE); END;"
        ),
    );
    let (globex, _) = backed_up(&fleet, "globex", tmp.path()).await;

    let refused = restore(
        &fleet,
        &globex,
        &backup,
        LocalRestore::new().allow_rekey(true),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&refused, FleetError::Restore { detail, .. } if detail.contains("trigger")),
        "{refused}"
    );
    let db = fleet.open(&globex).await.unwrap();
    assert_eq!(bodies(&db).await, ["kept"], "the live file stays");
    assert_eq!(
        fleet.inspect(&globex).await.unwrap().identity.as_deref(),
        Some("tenant:globex")
    );
    drop(db);
    fleet.close_all().await;
}

#[tokio::test]
async fn an_identity_that_a_migration_trigger_changes_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let backup = bare_backup(tmp.path(), "acme").await;
    // The trigger fires when the restore records a migration.
    sqlite_exec(
        &backup,
        &format!(
            "CREATE TABLE IF NOT EXISTS __diesel_schema_migrations (\
                 version VARCHAR(50) PRIMARY KEY NOT NULL, \
                 run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP); \
             CREATE TRIGGER rename AFTER INSERT ON __diesel_schema_migrations \
             BEGIN UPDATE {IDENTITY_TABLE} SET name = 'tenant:other'; END;"
        ),
    );
    let fleet = fleet(&tmp.path().join("fleet"));
    let key = fleet.key_for("acme").unwrap();

    let refused = restore(&fleet, &key, &backup, LocalRestore::new())
        .await
        .unwrap_err();

    assert!(
        matches!(&refused, FleetError::Restore { detail, .. } if detail.contains("identity")),
        "{refused}"
    );
    assert!(!fleet.exists(&key));
    assert!(no_staging_files(fleet.root()));
}
