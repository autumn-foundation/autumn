//! A plugin engine in `ShipMode::Generations` survives a takeover. The new
//! host starts with no volume and serves the data from the replica.
//!
//! The engine uses only the public API, as an engine plugin does. One
//! database is a directory with three files: `identity` (key and epoch),
//! `schema` (never changes) and `data` (one value per line).

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use autumn_web::config::DatabaseFleetConfig;
use autumn_web::db::fleet::{DatabaseFleet, FleetEngine, FleetError, ShipMode};
use autumn_web::db::fleet_ownership::{FleetOwnership, OwnerId, SlotRange, SqlSlotOwnership};
use autumn_web::db::fleet_replication::FleetReplication;
use autumn_web::db::fleet_tombstones::FleetTombstones;
use autumn_web::fleet::{FleetLease, resolve};
use autumn_web::fleet_layout::{FleetDbKey, FleetMode};
use autumn_web::lock::FencingToken;
use autumn_web::prelude::*;
use autumn_web::replication::{FileDestination, ReplicaDestination, ReplicationSettings};
use autumn_web::test::TestApp;
use autumn_web::time::{ClockSource, TickingClock};

use super::{control_pool, header_tenancy};

/// The lease of the test: the shortest the fleet takes.
const TTL: Duration = autumn_web::db::fleet_ownership::MIN_LEASE_TTL;

/// A directory database engine.
#[derive(Debug, Default)]
struct DirEngine;

/// An open directory database.
#[derive(Debug, Clone)]
struct DirHandle(Arc<DirState>);

#[derive(Debug)]
struct DirState {
    path: PathBuf,
    closed: AtomicBool,
}

fn io(op: &'static str) -> impl FnOnce(std::io::Error) -> FleetError {
    move |e| FleetError::Io {
        op,
        detail: e.to_string(),
    }
}

/// The key and the epoch of the database at `path`.
fn identity(path: &FsPath) -> Result<(String, u64), FleetError> {
    let text = std::fs::read_to_string(path.join("identity")).map_err(io("read identity"))?;
    let mut lines = text.lines();
    let name = lines.next().unwrap_or_default().to_owned();
    let epoch = lines.next().unwrap_or("0").parse().unwrap_or(0);
    Ok((name, epoch))
}

impl FleetEngine for DirEngine {
    type Handle = DirHandle;

    async fn open(
        &self,
        key: &FleetDbKey,
        path: &FsPath,
        epoch: Option<FencingToken>,
    ) -> Result<DirHandle, FleetError> {
        let name = key.name();
        let epoch = epoch.map_or(0, FencingToken::get);
        if path.is_dir() {
            let (found, stamp) = identity(path)?;
            if found != name {
                return Err(FleetError::Misplaced { name, found });
            }
            if epoch > 0 && stamp > epoch {
                return Err(FleetError::NewerEpoch {
                    name,
                    epoch,
                    found: stamp,
                });
            }
            if epoch > stamp {
                std::fs::write(path.join("identity"), format!("{name}\n{epoch}\n"))
                    .map_err(io("stamp"))?;
            }
        } else {
            std::fs::create_dir_all(path).map_err(io("create"))?;
            std::fs::write(path.join("identity"), format!("{name}\n{epoch}\n"))
                .map_err(io("create"))?;
            std::fs::write(path.join("schema"), "v1: one value per line\n")
                .map_err(io("create"))?;
            std::fs::write(path.join("data"), "").map_err(io("create"))?;
        }
        Ok(DirHandle(Arc::new(DirState {
            path: path.to_path_buf(),
            closed: AtomicBool::new(false),
        })))
    }

    fn close(&self, handle: &DirHandle) {
        handle.0.closed.store(true, Ordering::Release);
    }

    fn in_use(&self, _: &DirHandle) -> bool {
        false
    }

    async fn snapshot(
        &self,
        _: &DirHandle,
        path: &FsPath,
        dir: &FsPath,
    ) -> Result<Vec<PathBuf>, FleetError> {
        let copy = dir.join(path.file_name().ok_or_else(|| FleetError::Io {
            op: "snapshot",
            detail: "no file name".to_owned(),
        })?);
        std::fs::create_dir_all(&copy).map_err(io("snapshot"))?;
        let mut files = Vec::new();
        for name in ["identity", "schema", "data"] {
            std::fs::copy(path.join(name), copy.join(name)).map_err(io("snapshot"))?;
            files.push(copy.join(name));
        }
        Ok(files)
    }

    fn remove(&self, _: &FleetDbKey, path: &FsPath) -> Result<bool, FleetError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io("remove")(e)),
        }
    }

    fn resident_bytes(&self, _: &DirHandle) -> u64 {
        1
    }

    fn ship_mode(&self) -> ShipMode {
        ShipMode::Generations {
            every: Duration::from_hours(1),
        }
    }

    fn exists(&self, path: &FsPath) -> bool {
        path.is_dir()
    }

    fn epoch_stamp(&self, _: &FleetDbKey, path: &FsPath) -> Result<Option<u64>, FleetError> {
        identity(path).map(|(_, epoch)| Some(epoch))
    }
}

fn values(handle: &DirHandle) -> Vec<String> {
    std::fs::read_to_string(handle.0.path.join("data"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The extractor the engine plugin writes over `resolve`.
struct DirDb(FleetLease<DirEngine>);

impl axum::extract::FromRequestParts<AppState> for DirDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, AutumnError> {
        resolve::<DirEngine>(parts, state)
            .await
            .map(Self)
            .map_err(AutumnError::from)
    }
}

#[post("/dir/{value}")]
async fn dir_write(db: DirDb, Path(value): Path<String>) -> AutumnResult<String> {
    if db.0.0.closed.load(Ordering::Acquire) {
        return Err(AutumnError::internal_server_error_msg("closed"));
    }
    let failed = |e: std::io::Error| AutumnError::internal_server_error_msg(e.to_string());
    let mut text = std::fs::read_to_string(db.0.0.path.join("data")).map_err(failed)?;
    text.push_str(&value);
    text.push('\n');
    std::fs::write(db.0.0.path.join("data"), text).map_err(failed)?;
    Ok(value)
}

#[get("/dir")]
async fn dir_read(db: DirDb) -> AutumnResult<Json<Vec<String>>> {
    Ok(Json(values(&db.0)))
}

struct Host {
    fleet: DatabaseFleet<DirEngine>,
    ownership: Arc<FleetOwnership>,
    client: autumn_web::test::TestClient,
}

/// A host on its own volume. It shares the control database, so the
/// ownership table and the tombstones, and the replica directory.
async fn host(dir: &FsPath, volume: &str, owner: &str, clock: &TickingClock) -> Host {
    let fleet = DatabaseFleet::with_engine(
        &DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: dir.join(volume).display().to_string(),
            path: None,
            max_open: 4,
            max_resident_bytes: 0,
            min_residency: Duration::ZERO,
            pool_size: 1,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing: true,
            ownership: None,
            replica_retention_after_delete: Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 1,
            mmap_size: 0,
            cache_size_kib: 0,
        },
        DirEngine,
    )
    .unwrap();
    let clock: Arc<dyn ClockSource> = Arc::new(clock.clone());
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(dir.join("replicas")).unwrap());
    let replication = Arc::new(
        FleetReplication::new(
            destination,
            ReplicationSettings {
                database_path: PathBuf::new(),
                root: "app/test".to_owned(),
                sync_interval: Duration::from_secs(1),
                snapshot_interval: Duration::from_hours(1),
                max_wal_bytes: 4 * 1024 * 1024,
                retention: Duration::from_hours(1),
                verify_interval: None,
            },
            Duration::from_secs(30),
            true,
            Arc::clone(&clock),
        )
        .with_tombstones(Arc::new(FleetTombstones::new(
            control_pool(dir),
            Arc::clone(&clock),
        ))),
    );
    assert!(fleet.install_replication(replication).is_ok());
    let store = SqlSlotOwnership::new(control_pool(dir), Arc::clone(&clock));
    store.ensure_schema().await.unwrap();
    let ownership = Arc::new(
        FleetOwnership::new(
            Arc::new(store),
            OwnerId::new(owner).unwrap(),
            vec![SlotRange::ALL],
            TTL,
        )
        .unwrap(),
    );
    fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
    let installed = fleet.clone();
    let client = TestApp::new()
        .config(header_tenancy())
        .routes(routes![dir_write, dir_read])
        .state_initializer(move |state| state.insert_extension(installed))
        .build();
    Host {
        fleet,
        ownership,
        client,
    }
}

async fn post(host: &Host, value: &str) {
    host.client
        .post(&format!("/dir/{value}"))
        .header("x-tenant-id", "acme")
        .send()
        .await
        .assert_status(200);
}

/// The holder generation of `file` in the newest manifest of `key`.
fn holder(dir: &FsPath, key: &FleetDbKey, file: &str) -> String {
    let FleetDbKey::Tenant { id, .. } = key else {
        panic!("a tenant key");
    };
    let root = dir.join("replicas/app/test/fleet/tenant").join(id.as_str());
    let mut manifests: Vec<PathBuf> = walk(&root)
        .into_iter()
        .filter(|path| path.ends_with("manifest.json"))
        .collect();
    manifests.sort();
    let newest = manifests.last().expect("a manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap();
    manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"].as_str().unwrap().ends_with(file))
        .unwrap()["holder"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn walk(dir: &FsPath) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generations_engine_survives_a_takeover_with_no_volume_on_the_new_host() {
    let tmp = tempfile::tempdir().unwrap();
    let clock =
        TickingClock::starting_at(chrono::DateTime::from_timestamp_millis(1_000_000).unwrap());

    // Host A serves the tenant and ships a generation.
    let a = host(tmp.path(), "vol-a", "host-a", &clock).await;
    let key = a.fleet.key_for("acme").unwrap();
    post(&a, "plan").await;
    post(&a, "ship").await;
    let shipped = a.fleet.ship_now(&key).await.unwrap();
    assert_eq!(shipped.shipped, 3, "the first generation ships every file");
    let first = shipped.generation.expect("a generation");
    // A write after the last generation: inside the RPO, so a crash loses it.
    post(&a, "unshipped").await;

    // Host A stops renewing (it crashed). Its lease lapses at the store.
    clock.advance(TTL + Duration::from_secs(1));

    // Host B starts on an empty volume and takes the slot over.
    assert!(!tmp.path().join("vol-b").exists());
    let b = host(tmp.path(), "vol-b", "host-b", &clock).await;
    let epoch_b = b
        .ownership
        .epoch_for(key.routing_slot())
        .expect("B owns the slot");
    assert!(epoch_b.get() > 1);
    let read: Vec<String> = b
        .client
        .get("/dir")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .json();
    assert_eq!(
        read,
        ["plan", "ship"],
        "B serves the replica, not the lost tail"
    );

    // B writes and ships. The schema did not change: B does not ship it again.
    post(&b, "takeover").await;
    let report = b.fleet.ship_now(&key).await.unwrap();
    assert!(report.reused >= 1, "{report:?}");
    assert_eq!(holder(tmp.path(), &key, "schema"), first);

    // A comes back with its old lease. Every ship of A is refused.
    let refused = a.fleet.ship_now(&key).await;
    assert!(
        matches!(refused, Err(FleetError::Fenced { .. })),
        "{refused:?}"
    );
    assert_eq!(a.ownership.epoch_for(key.routing_slot()), None);

    // A third host, also with no volume, reads B's data.
    b.fleet.close_all().await;
    assert!(b.fleet.release_ownership().await);
    let c = host(tmp.path(), "vol-c", "host-c", &clock).await;
    let read: Vec<String> = c
        .client
        .get("/dir")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .json();
    assert_eq!(read, ["plan", "ship", "takeover"]);
    c.fleet.close_all().await;
    a.fleet.close_all().await;
}
