//! A `LakeSink` that keeps its lake between loads.
//!
//! The sink uses only the public API, as a lake plugin does. It holds the
//! `fleet_notes` row count of each member. Its catalog is one manifest file
//! per member and generation, under its own prefix in object storage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

use autumn_web::db::fleet::DatabaseFleet;
use autumn_web::db::fleet_lake::{
    FleetLake, LakeError, LakeGeneration, LakeLoad, LakeMember, LakeSink, SkipReason,
};
use autumn_web::db::fleet_tombstones::{MARKER_OBJECT, Marker};
use autumn_web::fleet_layout::{FleetDbKey, FleetMode};
use autumn_web::reexports::diesel;
use autumn_web::replication::FileDestination;
use autumn_web::replication::destination::{DestinationError, ReplicaDestination};

use super::{Count, app, fleet, replicate};

const BASE: &str = "app/test";

/// The prefix of the test sink. It is outside `<base>/fleet/`.
const PREFIX: &str = "app/test/lake/kept";

/// The manifest of one member.
#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    member: String,
    generation: LakeGeneration,
    rows: i64,
}

/// What a load of [`KeptLake`] gives.
#[derive(Debug, Default)]
struct Kept {
    /// Row count per member.
    rows: BTreeMap<String, i64>,
    /// The members restored and added in this load, in order.
    added: Vec<String>,
}

/// A sink that keeps its lake in manifest files between loads.
struct KeptLake {
    destination: Arc<dyn ReplicaDestination>,
    /// `keep` fails for this member.
    fail_keep: Option<&'static str>,
    held: BTreeMap<String, Manifest>,
    seen: BTreeSet<String>,
    kept: Kept,
}

/// The directory of a member's manifests under [`PREFIX`].
fn member_dir(name: &str) -> String {
    format!("{PREFIX}/{}", name.replace(':', "/"))
}

impl KeptLake {
    /// Read the catalog of the last load.
    fn open(destination: Arc<dyn ReplicaDestination>) -> Self {
        let mut held = BTreeMap::new();
        for key in destination.list(&format!("{PREFIX}/")).unwrap() {
            let manifest: Manifest =
                serde_json::from_slice(&destination.get(&key).unwrap()).unwrap();
            held.insert(manifest.member.clone(), manifest);
        }
        Self {
            destination,
            fail_keep: None,
            held,
            seen: BTreeSet::new(),
            kept: Kept::default(),
        }
    }

    /// Remove every manifest of `name`, other than `except`.
    fn forget(&self, name: &str, except: Option<&str>) -> Result<(), LakeError> {
        let keys = self
            .destination
            .list(&format!("{}/", member_dir(name)))
            .map_err(|error| LakeError::Destination(error.to_string()))?;
        for key in keys.iter().filter(|key| Some(key.as_str()) != except) {
            self.destination
                .delete(key)
                .map_err(|error| LakeError::Destination(error.to_string()))?;
        }
        Ok(())
    }
}

impl LakeSink for KeptLake {
    type Lake = Kept;

    /// It counts rows per member, so it takes slot databases.
    const TAKES_SLOTS: bool = true;

    fn add(&mut self, member: LakeMember<'_>) -> Result<(), LakeError> {
        use diesel::{Connection as _, RunQueryDsl as _};

        let name = member.key.name();
        let failed = |detail: String| LakeError::Ingest {
            tenant: name.clone(),
            detail,
        };
        let mut conn = diesel::SqliteConnection::establish(&member.file.display().to_string())
            .map_err(|error| failed(error.to_string()))?;
        let count: Count = diesel::sql_query("SELECT COUNT(*) AS n FROM fleet_notes")
            .get_result(&mut conn)
            .map_err(|error| failed(error.to_string()))?;
        drop(conn);
        // Write the new manifest first: a failed write keeps the old one.
        let manifest = Manifest {
            member: name.clone(),
            generation: member.generation.clone(),
            rows: count.n,
        };
        let manifest_key = format!("{}/{}.json", member_dir(&name), member.generation.id);
        self.destination
            .put(&manifest_key, &serde_json::to_vec(&manifest).unwrap())
            .map_err(|error| LakeError::Destination(error.to_string()))?;
        self.forget(&name, Some(&manifest_key))?;
        self.kept.rows.insert(name.clone(), count.n);
        self.kept.added.push(name.clone());
        self.seen.insert(name);
        Ok(())
    }

    fn has(&self, key: &FleetDbKey, generation: &LakeGeneration) -> bool {
        self.held
            .get(&key.name())
            .is_some_and(|manifest| &manifest.generation == generation)
    }

    fn keep(&mut self, key: &FleetDbKey, _generation: &LakeGeneration) -> Result<(), LakeError> {
        let name = key.name();
        let failed = |detail: &str| LakeError::Ingest {
            tenant: name.clone(),
            detail: detail.to_owned(),
        };
        if self.fail_keep == Some(name.as_str()) {
            return Err(failed("keep failed"));
        }
        // Core calls `keep` only after `has` is `true`.
        let rows = self
            .held
            .get(&name)
            .ok_or_else(|| failed("keep without a manifest"))?
            .rows;
        self.kept.rows.insert(name.clone(), rows);
        self.seen.insert(name);
        Ok(())
    }

    fn finish(self) -> Result<Kept, LakeError> {
        for name in self.held.keys().filter(|name| !self.seen.contains(*name)) {
            self.forget(name, None)?;
        }
        Ok(self.kept)
    }
}

/// A destination that records each snapshot download. A restore makes
/// one. Planning reads only small objects (the head and the marker).
struct Reads {
    inner: FileDestination,
    read: Mutex<Vec<String>>,
}

impl Reads {
    /// The members restored since the last call.
    fn restored(&self) -> BTreeSet<String> {
        let fleet = format!("{BASE}/fleet/");
        self.read
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|key| {
                let rest = key.strip_prefix(&fleet)?;
                let mut parts = rest.split('/');
                Some(format!("{}:{}", parts.next()?, parts.next()?))
            })
            .collect()
    }
}

impl ReplicaDestination for Reads {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &FsPath) -> Result<(), DestinationError> {
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        self.inner.get(key)
    }
    fn get_to_file(&self, key: &str, path: &FsPath) -> Result<(), DestinationError> {
        self.read.lock().unwrap().push(key.to_owned());
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

struct Host {
    _tmp: tempfile::TempDir,
    fleet: DatabaseFleet,
    client: autumn_web::test::TestClient,
    reads: Arc<Reads>,
    lake: FleetLake,
}

fn host(mode: FleetMode) -> Host {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), mode, true);
    let replicas = tmp.path().join("replicas");
    let _replication = replicate(&fleet, &replicas);
    let client = app(tmp.path(), fleet.clone());
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let reads = Arc::new(Reads {
        inner: FileDestination::new(&replicas).unwrap(),
        read: Mutex::new(Vec::new()),
    });
    let lake = FleetLake::new(Arc::clone(&reads) as Arc<dyn ReplicaDestination>, BASE)
        .markers_only()
        .with_scratch_dir(&scratch);
    Host {
        _tmp: tmp,
        fleet,
        client,
        reads,
        lake,
    }
}

impl Host {
    async fn note(&self, tenant: &str, body: &str) {
        self.client
            .post(&format!("/notes/{body}"))
            .header("x-tenant-id", tenant)
            .send()
            .await
            .assert_status(200);
    }

    /// Write the notes, then close every database so its replica holds
    /// every commit.
    async fn notes(&self, notes: &[(&str, &str)]) {
        let mut keys = BTreeSet::new();
        for (tenant, body) in notes {
            self.note(tenant, body).await;
            keys.insert(self.fleet.key_for(tenant).unwrap());
        }
        for key in &keys {
            self.fleet.close(key).await;
        }
    }

    fn destination(&self) -> Arc<dyn ReplicaDestination> {
        Arc::clone(&self.reads) as Arc<dyn ReplicaDestination>
    }

    async fn load(&self) -> LakeLoad<Kept> {
        self.lake
            .load(KeptLake::open(self.destination()))
            .await
            .unwrap()
    }

    /// Copy the replica of `from` to the root of `to`.
    fn copy_replica(&self, from: &str, to: &str) {
        let source = format!("{BASE}/fleet/tenant/{from}/");
        for key in self.reads.list(&source).unwrap() {
            let body = self.reads.get(&key).unwrap();
            let target = format!("{BASE}/fleet/tenant/{to}/{}", &key[source.len()..]);
            self.reads.put(&target, &body).unwrap();
        }
    }
}

fn rows(load: &LakeLoad<Kept>) -> Vec<(&str, i64)> {
    load.lake
        .rows
        .iter()
        .map(|(name, rows)| (name.as_str(), *rows))
        .collect()
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

const THREE: [(&str, &str); 6] = [
    ("acme", "plan"),
    ("globex", "launch"),
    ("acme", "ship"),
    ("initech", "audit"),
    ("initech", "bill"),
    ("initech", "close"),
];

/// `has` skips the members whose generation the sink holds. The second
/// load restores only the tenant that changed.
#[tokio::test]
async fn a_second_load_restores_only_changed_tenants() {
    let host = host(FleetMode::Tenant);
    host.notes(&THREE).await;

    let first = host.load().await;
    assert_eq!(
        first.lake.added,
        ["tenant:acme", "tenant:globex", "tenant:initech"]
    );
    assert_eq!(
        rows(&first),
        [
            ("tenant:acme", 2),
            ("tenant:globex", 1),
            ("tenant:initech", 3)
        ]
    );
    assert_eq!(
        host.reads.restored(),
        names(&["tenant:acme", "tenant:globex", "tenant:initech"])
    );
    assert!(first.members.iter().all(|member| member.restored));

    // Only acme changes. Its generation id can stay the same: the WAL
    // position moves on.
    host.notes(&[("acme", "review")]).await;
    let second = host.load().await;
    assert_eq!(second.lake.added, ["tenant:acme"]);
    assert_eq!(
        rows(&second),
        [
            ("tenant:acme", 3),
            ("tenant:globex", 1),
            ("tenant:initech", 3)
        ]
    );
    assert_eq!(host.reads.restored(), names(&["tenant:acme"]));
    let restored: Vec<(String, bool)> = second
        .members
        .iter()
        .map(|member| (member.key.name(), member.restored))
        .collect();
    assert_eq!(
        restored,
        [
            ("tenant:acme".to_owned(), true),
            ("tenant:globex".to_owned(), false),
            ("tenant:initech".to_owned(), false),
        ]
    );
    assert!(second.skipped.is_empty());

    // No change: nothing is restored.
    let third = host.load().await;
    assert!(third.lake.added.is_empty());
    assert_eq!(rows(&third), rows(&second));
    assert!(host.reads.restored().is_empty());
}

/// A commit shipped in the same generation is a change. The generation id
/// stays the same, and the WAL position moves on.
#[tokio::test]
async fn a_commit_in_the_same_generation_is_a_change() {
    let host = host(FleetMode::Tenant);
    host.notes(&THREE).await;
    // Keep acme open, so its WAL ships into one generation.
    let acme = host.fleet.key_for("acme").unwrap();
    let open = host.fleet.open(&acme).await.unwrap();
    host.fleet.ship_now(&acme).await.unwrap();
    let first = host.load().await;
    host.reads.restored();

    host.note("acme", "review").await;
    host.fleet.ship_now(&acme).await.unwrap();
    let second = host.load().await;

    let of = |load: &LakeLoad<Kept>| {
        load.members
            .iter()
            .find(|member| member.key == acme)
            .unwrap()
            .generation
            .clone()
    };
    assert_eq!(of(&first).id, of(&second).id, "one generation");
    assert_ne!(of(&first).segment, of(&second).segment);
    assert_eq!(second.lake.added, ["tenant:acme"]);
    assert_eq!(host.reads.restored(), names(&["tenant:acme"]));
    assert_eq!(
        rows(&second),
        [
            ("tenant:acme", 3),
            ("tenant:globex", 1),
            ("tenant:initech", 3)
        ]
    );
    drop(open);
}

/// Core runs the inclusion rule before `has`. A tenant deleted since the
/// last load is not kept, even though the sink holds its generation, and
/// the sink drops it at `finish`.
#[tokio::test]
async fn a_tenant_deleted_since_the_last_load_leaves_the_kept_lake() {
    let host = host(FleetMode::Tenant);
    host.notes(&THREE).await;
    let first = host.load().await;
    assert_eq!(first.lake.rows.len(), 3);
    assert_eq!(host.reads.restored().len(), 3);

    // A tombstone marker newer than globex's generation.
    let now = chrono::Utc::now().timestamp_millis() + 1;
    let marker = Marker {
        deleted_at_ms: now,
        purge_after_ms: now + 3_600_000,
    };
    host.reads
        .put(
            &format!("{BASE}/fleet/tenant/globex/{MARKER_OBJECT}"),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
    // And a purge of initech: its replica is gone.
    for key in host
        .reads
        .list(&format!("{BASE}/fleet/tenant/initech/"))
        .unwrap()
    {
        host.reads.delete(&key).unwrap();
    }

    let second = host.load().await;
    assert_eq!(rows(&second), [("tenant:acme", 2)]);
    assert!(second.lake.added.is_empty());
    assert!(host.reads.restored().is_empty());
    let manifests = host.reads.list(&format!("{PREFIX}/")).unwrap();
    assert_eq!(manifests.len(), 1, "{manifests:?}");
    assert!(manifests[0].starts_with(&member_dir("tenant:acme")));
    assert!(
        second
            .skipped
            .iter()
            .any(|skip| skip.name == "globex" && skip.reason == SkipReason::Deleted)
    );
}

/// A member whose `keep` fails fails the load. With
/// `skip_failed_tenants`, core skips it, and the sink drops it at `finish`.
#[tokio::test]
async fn a_failed_keep_fails_the_load_or_skips_the_member() {
    let host = host(FleetMode::Tenant);
    host.notes(&THREE).await;
    host.load().await;

    let failing = || KeptLake {
        fail_keep: Some("tenant:globex"),
        ..KeptLake::open(host.destination())
    };
    let error = host.lake.load(failing()).await.unwrap_err();
    assert!(
        matches!(error, LakeError::Ingest { ref tenant, .. } if tenant == "tenant:globex"),
        "{error}"
    );

    let load = host
        .lake
        .clone()
        .skip_failed_tenants()
        .load(failing())
        .await
        .unwrap();
    assert_eq!(rows(&load), [("tenant:acme", 2), ("tenant:initech", 3)]);
    let skip = &load.skipped[0];
    assert_eq!(skip.name, "globex", "core names the skip from the key");
    assert!(matches!(skip.reason, SkipReason::Failed(_)));
    assert!(
        host.reads
            .list(&format!("{}/", member_dir("tenant:globex")))
            .unwrap()
            .is_empty()
    );
}

/// Core checks the identity row of each restored file before `add`, for
/// each sink. A replica under the root of another tenant is refused.
#[tokio::test]
async fn core_refuses_a_misplaced_replica_for_each_sink() {
    let host = host(FleetMode::Tenant);
    host.notes(&THREE).await;
    host.copy_replica("acme", "hooli");

    let error = host
        .lake
        .load(KeptLake::open(host.destination()))
        .await
        .unwrap_err();
    assert!(
        matches!(error, LakeError::Misplaced { ref tenant, .. } if tenant == "hooli"),
        "{error}"
    );

    let load = host
        .lake
        .clone()
        .skip_failed_tenants()
        .load(KeptLake::open(host.destination()))
        .await
        .unwrap();
    assert!(!load.lake.rows.contains_key("tenant:hooli"));
    assert_eq!(load.skipped.len(), 1);
    assert_eq!(load.skipped[0].name, "hooli");
}

/// A sink that takes slots loads a slot fleet. The `SQLite` sink still
/// refuses one.
#[tokio::test]
async fn a_sink_that_takes_slots_loads_a_slot_fleet() {
    let host = host(FleetMode::Slot);
    host.notes(&THREE).await;

    let refused = host.lake.materialize().await.unwrap_err();
    assert_eq!(refused, LakeError::SlotFleet);

    let load = host.load().await;
    assert!(!load.lake.added.is_empty());
    assert!(
        load.members
            .iter()
            .all(|member| matches!(member.key, FleetDbKey::Slot(_)))
    );
    assert!(load.lake.added.iter().all(|name| name.starts_with("slot:")));
    assert_eq!(load.lake.rows.values().sum::<i64>(), 6);
    assert!(load.skipped.is_empty(), "{:?}", load.skipped);

    // The second load restores nothing.
    let again = host.load().await;
    assert!(again.lake.added.is_empty());
    assert_eq!(again.lake.rows.values().sum::<i64>(), 6);
}
