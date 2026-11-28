//! Lake tests over a fleet in `ShipMode::Generations`, on `TextEngine`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use super::*;
use crate::config::DatabaseFleetConfig;
use crate::db::fleet::DatabaseFleet;
use crate::db::fleet::generations::{self, Manifest};
use crate::db::fleet::test_engine::TextEngine;
use crate::db::fleet_replication::FleetReplication;
use crate::fleet_layout::FleetMode;
use crate::replication::destination::DestinationError;
use crate::replication::{FileDestination, ReplicationSettings};

const BASE: &str = "app/gen";
const HOUR: Duration = Duration::from_hours(1);

struct Host {
    _tmp: tempfile::TempDir,
    fleet: DatabaseFleet<TextEngine>,
    destination: Arc<dyn ReplicaDestination>,
    replication: Arc<FleetReplication>,
    scratch: PathBuf,
}

fn config(root: &Path, retention: Duration) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open: 8,
        max_resident_bytes: 0,
        min_residency: Duration::ZERO,
        max_open_policy: crate::config::FleetMaxOpenPolicy::Soft,
        pool_size: 1,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: retention,
        restore_check_sample: 0,
        replication_workers: 1,
        mmap_size: 0,
        cache_size_kib: 0,
    }
}

/// A tenant fleet that ships generations, with tombstones. `retention`:
/// how long a deleted database's replica stays.
fn host(retention: Duration) -> Host {
    let tmp = tempfile::tempdir().unwrap();
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let mut engine = TextEngine::new(100);
    engine.every = Some(HOUR);
    let fleet =
        DatabaseFleet::with_engine(&config(&tmp.path().join("volume"), retention), engine).unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(tmp.path().join("replicas")).unwrap());
    let clock: Arc<dyn crate::time::ClockSource> = Arc::new(crate::time::SystemClock);
    let control = crate::db::create_pool(&crate::config::DatabaseConfig {
        url: Some(format!(
            "sqlite://{}",
            tmp.path().join("control.db").display()
        )),
        connect_timeout_secs: 60,
        ..Default::default()
    })
    .unwrap()
    .unwrap();
    let tombstones = Arc::new(FleetTombstones::new(control, Arc::clone(&clock)));
    let replication = Arc::new(
        FleetReplication::new(
            Arc::clone(&destination),
            ReplicationSettings {
                database_path: PathBuf::new(),
                root: BASE.to_owned(),
                sync_interval: Duration::from_secs(1),
                snapshot_interval: HOUR,
                max_wal_bytes: 4 * 1024 * 1024,
                retention: HOUR,
                verify_interval: None,
            },
            Duration::from_secs(30),
            false,
            clock,
        )
        .with_tombstones(tombstones)
        .with_retention_after_delete(retention),
    );
    assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
    Host {
        _tmp: tmp,
        fleet,
        destination,
        replication,
        scratch,
    }
}

impl Host {
    fn key(&self, id: &str) -> FleetDbKey {
        self.fleet.key_for(id).unwrap()
    }

    fn lake(&self) -> FleetLake {
        self.replication.lake().with_scratch_dir(&self.scratch)
    }

    /// Write `values` to `id` and ship one generation. Returns its id.
    async fn ship(&self, id: &str, values: &[&str]) -> String {
        let key = self.key(id);
        let db = self.fleet.open(&key).await.unwrap();
        for value in values {
            TextEngine::write(db.handle(), value).unwrap();
        }
        drop(db);
        self.fleet
            .ship_now(&key)
            .await
            .unwrap()
            .generation
            .expect("a new generation")
    }

    /// The complete generations of `id`, oldest first.
    fn complete(&self, id: &str) -> Vec<generations::Complete> {
        let root = self.replication.root_for(&self.key(id));
        let keys = self.destination.list(&format!("{root}/")).unwrap();
        generations::complete_generations(&root, &keys)
    }
}

/// One member that [`TextSink::add`] took.
#[derive(Debug, Clone)]
struct Added {
    name: String,
    generation: LakeGeneration,
    format: LakeFormat,
    values: Vec<String>,
}

/// A sink for `TextEngine` files. Line 1 of a file is its key name.
#[derive(Debug, Default)]
struct TextSink {
    /// The states [`has`](LakeSink::has) holds.
    held: HashMap<FleetDbKey, LakeGeneration>,
    /// The members `check_identity` saw, in order.
    checked: RefCell<Vec<String>>,
    added: Vec<Added>,
    kept: Vec<(String, LakeGeneration)>,
    /// The file of the last member `add` took.
    last_file: Option<PathBuf>,
    /// `add` found the file of the member before it.
    leftover: bool,
    /// `keep` fails.
    keep_fails: bool,
}

impl LakeSink for TextSink {
    type Lake = Self;

    fn check_identity(&self, member: LakeMember<'_>) -> Result<(), LakeError> {
        let name = member_name(member.key);
        self.checked.borrow_mut().push(name.clone());
        let text = std::fs::read_to_string(member.file).map_err(|error| LakeError::Ingest {
            tenant: name.clone(),
            detail: error.to_string(),
        })?;
        let found = text.lines().next().unwrap_or_default();
        if found == member.key.name() {
            Ok(())
        } else {
            Err(LakeError::Misplaced {
                tenant: name,
                detail: format!("the file names {found}"),
            })
        }
    }

    fn add(&mut self, member: LakeMember<'_>) -> Result<(), LakeError> {
        let name = member_name(member.key);
        assert!(
            self.checked.borrow().contains(&name),
            "core checks the identity of {name} before add"
        );
        if self.last_file.as_deref().is_some_and(Path::exists) {
            self.leftover = true;
        }
        let text = std::fs::read_to_string(member.file).unwrap();
        self.added.push(Added {
            name,
            generation: member.generation.clone(),
            format: member.format,
            values: text.lines().skip(2).map(str::to_owned).collect(),
        });
        self.last_file = Some(member.file.to_path_buf());
        Ok(())
    }

    fn has(&self, key: &FleetDbKey, generation: &LakeGeneration) -> bool {
        self.held.get(key) == Some(generation)
    }

    fn keep(&mut self, key: &FleetDbKey, generation: &LakeGeneration) -> Result<(), LakeError> {
        if self.keep_fails {
            return Err(LakeError::Ingest {
                tenant: member_name(key),
                detail: "keep failed".to_owned(),
            });
        }
        self.kept.push((member_name(key), generation.clone()));
        Ok(())
    }

    fn finish(self) -> Result<Self, LakeError> {
        Ok(self)
    }
}

fn names(load: &LakeLoad<TextSink>) -> Vec<String> {
    load.members.iter().map(|m| member_name(&m.key)).collect()
}

// ── Acceptance ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_generations_fleet_gives_each_member_to_the_sink_restored_and_checked() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    let acme = host.ship("acme", &["a2"]).await;
    let globex = host.ship("globex", &["g1"]).await;
    let initech = host.ship("initech", &[]).await;
    host.fleet.close_all().await;

    let load = host
        .lake()
        .with_workers(2)
        .load(TextSink::default())
        .await
        .unwrap();
    assert_eq!(names(&load), ["acme", "globex", "initech"]);
    assert!(load.skipped.is_empty(), "{:?}", load.skipped);
    assert!(load.members.iter().all(|m| m.restored));
    let sink = load.lake;
    assert_eq!(*sink.checked.borrow(), ["acme", "globex", "initech"]);
    assert!(!sink.leftover, "core removes each member after add");
    let added: Vec<(&str, &str, Vec<&str>)> = sink
        .added
        .iter()
        .map(|a| {
            assert_eq!(a.format, LakeFormat::Generations);
            assert_eq!(a.generation.segment, None);
            (
                a.name.as_str(),
                a.generation.id.as_str(),
                a.values.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(
        added,
        [
            ("acme", acme.as_str(), vec!["a1", "a2"]),
            ("globex", globex.as_str(), vec!["g1"]),
            ("initech", initech.as_str(), vec![]),
        ]
    );
    let created = host.complete("acme").last().unwrap().created_ms;
    assert_eq!(
        sink.added[0].generation.effective,
        segment::timestamp_from_ms(created)
    );
}

#[tokio::test]
async fn has_and_keep_take_a_generation_id_from_a_manifest() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    let globex_before = host.ship("globex", &["g1"]).await;
    let first = host.lake().load(TextSink::default()).await.unwrap();

    let globex_after = host.ship("globex", &["g2"]).await;
    host.fleet.close_all().await;
    let held = first
        .members
        .iter()
        .map(|m| (m.key.clone(), m.generation.clone()))
        .collect();
    let second = host
        .lake()
        .load(TextSink {
            held,
            ..TextSink::default()
        })
        .await
        .unwrap();
    assert_eq!(names(&second), ["acme", "globex"]);
    let restored: Vec<bool> = second.members.iter().map(|m| m.restored).collect();
    assert_eq!(restored, [false, true]);
    let sink = second.lake;
    assert_eq!(sink.kept.len(), 1);
    assert_eq!(sink.kept[0].0, "acme");
    assert_eq!(sink.kept[0].1, first.members[0].generation);
    assert_eq!(
        *sink.checked.borrow(),
        ["globex"],
        "a kept member is not restored"
    );
    assert_eq!(sink.added.len(), 1);
    assert_ne!(globex_before, globex_after);
    assert_eq!(sink.added[0].generation.id, globex_after);
    assert_eq!(sink.added[0].values, ["g1", "g2"]);
}

#[tokio::test]
async fn a_sink_that_cannot_read_generations_fails_each_member_and_says_why() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    host.ship("globex", &["g1"]).await;
    host.ship("hooli", &["h1"]).await;
    host.ship("initech", &["i1"]).await;
    host.fleet.close_all().await;

    // The call that loaded 0 tenants and skipped each as `Unreplicated`.
    let snapshot = FleetLake::new(Arc::clone(&host.destination), BASE)
        .markers_only()
        .skip_failed_tenants()
        .with_scratch_dir(&host.scratch)
        .materialize()
        .await
        .unwrap();
    assert!(snapshot.tenants().is_empty());
    assert_eq!(snapshot.skipped().len(), 4);
    for skip in snapshot.skipped() {
        assert!(
            matches!(&skip.reason, SkipReason::Failed(detail) if detail.contains("generations")),
            "{skip:?}"
        );
    }

    // The `SQLite` lake has no identity check for generations.
    let error = host.lake().materialize().await.unwrap_err();
    assert!(
        matches!(error, LakeError::Ingest { ref tenant, ref detail }
            if tenant == "acme" && detail.contains("generations")),
        "{error}"
    );
    let snapshot = host
        .lake()
        .skip_failed_tenants()
        .materialize()
        .await
        .unwrap();
    assert!(snapshot.tenants().is_empty());
    let skipped: Vec<(&str, bool)> = snapshot
        .skipped()
        .iter()
        .map(|s| (s.name.as_str(), matches!(s.reason, SkipReason::Failed(_))))
        .collect();
    assert_eq!(
        skipped,
        [
            ("acme", true),
            ("globex", true),
            ("hooli", true),
            ("initech", true)
        ]
    );
}

/// A sink that keeps every default method but `add` and `finish`.
struct Plain;

impl LakeSink for Plain {
    type Lake = ();

    fn add(&mut self, _member: LakeMember<'_>) -> Result<(), LakeError> {
        panic!("core never adds a member it did not check");
    }

    fn finish(self) -> Result<(), LakeError> {
        Ok(())
    }
}

#[tokio::test]
async fn the_default_identity_check_refuses_a_generations_member() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    host.fleet.close_all().await;
    let load = host.lake().skip_failed_tenants().load(Plain).await.unwrap();
    assert!(load.members.is_empty());
    assert_eq!(load.skipped.len(), 1);
    assert!(
        matches!(&load.skipped[0].reason, SkipReason::Failed(detail) if detail.contains("generations")),
        "{:?}",
        load.skipped
    );
}

#[tokio::test]
async fn a_manifest_of_another_version_is_an_error_not_unreplicated() {
    let host = host(Duration::ZERO);
    let id = host.ship("acme", &["a1"]).await;
    host.fleet.close_all().await;
    let root = host.replication.root_for(&host.key("acme"));
    let key = generations::manifest_key(&root, &id);
    let mut manifest: Manifest =
        serde_json::from_slice(&host.destination.get(&key).unwrap()).unwrap();
    manifest.version = 99;
    host.destination
        .put(&key, &serde_json::to_vec(&manifest).unwrap())
        .unwrap();

    let error = host.lake().load(TextSink::default()).await.unwrap_err();
    assert!(
        matches!(error, LakeError::Restore { ref tenant, ref detail }
            if tenant == "acme" && detail.contains("version 99")),
        "{error}"
    );
    let load = host
        .lake()
        .skip_failed_tenants()
        .load(TextSink::default())
        .await
        .unwrap();
    assert!(matches!(load.skipped[0].reason, SkipReason::Failed(_)));
}

#[tokio::test]
async fn a_file_that_does_not_match_its_manifest_fails_the_load() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    host.fleet.close_all().await;
    let root = host.replication.root_for(&host.key("acme"));
    for key in host.destination.list(&format!("{root}/")).unwrap() {
        if key.contains(&format!("/{}/", generations::FILES_DIR)) {
            host.destination.put(&key, b"tampered").unwrap();
        }
    }
    let error = host.lake().load(TextSink::default()).await.unwrap_err();
    assert!(
        matches!(error, LakeError::Restore { ref detail, .. } if detail.contains("does not match")),
        "{error}"
    );
}

#[tokio::test]
async fn a_misplaced_generations_replica_fails_the_load() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    host.fleet.close_all().await;
    let from = format!("{BASE}/fleet/tenant/acme/");
    for key in host.destination.list(&from).unwrap() {
        let body = host.destination.get(&key).unwrap();
        let to = key.replacen("/tenant/acme/", "/tenant/evil/", 1);
        host.destination.put(&to, &body).unwrap();
    }
    let error = host.lake().load(TextSink::default()).await.unwrap_err();
    assert!(
        matches!(error, LakeError::Misplaced { ref tenant, .. } if tenant == "evil"),
        "{error}"
    );
    // As for a WAL member: skipped as failed, with the reason.
    let load = host
        .lake()
        .skip_failed_tenants()
        .load(TextSink::default())
        .await
        .unwrap();
    assert_eq!(names(&load), ["acme"]);
    assert_eq!(load.skipped[0].name, "evil");
    assert!(
        matches!(&load.skipped[0].reason, SkipReason::Failed(detail) if detail.contains("misplaced")),
        "{:?}",
        load.skipped
    );
}

// ── Inclusion ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn as_of_takes_the_manifest_at_or_before_the_target() {
    let host = host(Duration::ZERO);
    let first = host.ship("acme", &["a1"]).await;
    host.ship("acme", &["a2"]).await;
    host.fleet.close_all().await;
    let complete = host.complete("acme");
    assert_eq!(complete.len(), 2);
    assert!(complete[0].created_ms < complete[1].created_ms);
    let at = segment::timestamp_from_ms(complete[0].created_ms);

    let load = host
        .lake()
        .as_of(at)
        .load(TextSink::default())
        .await
        .unwrap();
    assert_eq!(load.lake.added[0].generation.id, first);
    assert_eq!(load.lake.added[0].values, ["a1"]);

    let before = segment::timestamp_from_ms(complete[0].created_ms - 1);
    let load = host
        .lake()
        .as_of(before)
        .load(TextSink::default())
        .await
        .unwrap();
    assert!(load.members.is_empty());
    assert_eq!(load.skipped[0].reason, SkipReason::AbsentAtTarget);
}

#[tokio::test]
async fn a_deleted_generations_tenant_is_skipped_by_its_row_and_by_its_marker() {
    let host = host(HOUR);
    host.ship("acme", &["a1"]).await;
    host.ship("globex", &["g1"]).await;
    assert!(host.fleet.delete(&host.key("globex")).await.unwrap());
    host.fleet.close_all().await;
    assert!(
        !host.complete("globex").is_empty(),
        "the replica stays in its retention"
    );

    for lake in [
        host.lake(),
        FleetLake::new(Arc::clone(&host.destination), BASE)
            .markers_only()
            .with_scratch_dir(&host.scratch),
    ] {
        let load = lake.load(TextSink::default()).await.unwrap();
        assert_eq!(names(&load), ["acme"]);
        assert_eq!(load.skipped.len(), 1);
        assert_eq!(load.skipped[0].name, "globex");
        assert_eq!(load.skipped[0].reason, SkipReason::Deleted);
    }
    // The row alone excludes too.
    let root = host.replication.root_for(&host.key("globex"));
    host.destination
        .delete(&format!("{root}/{MARKER_OBJECT}"))
        .unwrap();
    let load = host.lake().load(TextSink::default()).await.unwrap();
    assert_eq!(load.skipped[0].reason, SkipReason::Deleted);
}

/// A listing of `<base>/fleet/` taken before: it can name an object that
/// is gone now.
struct Frozen {
    inner: Arc<dyn ReplicaDestination>,
    listing: Vec<String>,
}

impl ReplicaDestination for Frozen {
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
        if prefix.ends_with("/fleet/") {
            Ok(self.listing.clone())
        } else {
            self.inner.list(prefix)
        }
    }
    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn a_manifest_gone_since_the_listing_is_planned_again() {
    let host = host(Duration::ZERO);
    let first = host.ship("acme", &["a1"]).await;
    let second = host.ship("acme", &["a2"]).await;
    host.fleet.close_all().await;
    let listing = host.destination.list(&format!("{BASE}/fleet/")).unwrap();
    // The newest manifest goes after the listing.
    let root = host.replication.root_for(&host.key("acme"));
    host.destination
        .delete(&generations::manifest_key(&root, &second))
        .unwrap();

    let frozen = Arc::new(Frozen {
        inner: Arc::clone(&host.destination),
        listing,
    });
    let load = FleetLake::new(frozen, BASE)
        .markers_only()
        .with_scratch_dir(&host.scratch)
        .load(TextSink::default())
        .await
        .unwrap();
    assert_eq!(load.lake.added.len(), 1);
    assert_eq!(load.lake.added[0].generation.id, first);
    assert_eq!(load.lake.added[0].values, ["a1"]);
}

#[test]
fn the_listing_counts_a_manifest_as_a_complete_generation() {
    let root = format!("{BASE}/fleet/tenant/acme");
    let id = segment::generation_id(1_700_000_000_000, 7);
    let keys = vec![generations::manifest_key(&root, &id)];
    assert_eq!(
        newest_generation(&root, &keys),
        Some(segment::timestamp_from_ms(1_700_000_000_000))
    );
    assert_eq!(
        decide(false, None, newest_generation(&root, &keys)),
        Decision::Plan
    );
    // A file with no manifest is a ship in progress: not complete.
    let files = vec![generations::file_key(&root, &id, &"a".repeat(64))];
    assert_eq!(newest_generation(&root, &files), None);
}

#[test]
fn the_newer_format_wins_and_a_manifest_wins_a_tie() {
    use LakeFormat::{Generations, Wal};
    assert_eq!(newer_format(None, None), None);
    assert_eq!(newer_format(Some((0, 5)), None), Some(Wal));
    assert_eq!(newer_format(None, Some((0, 5))), Some(Generations));
    assert_eq!(newer_format(Some((0, 6)), Some((0, 5))), Some(Wal));
    assert_eq!(newer_format(Some((0, 5)), Some((0, 6))), Some(Generations));
    assert_eq!(newer_format(Some((0, 5)), Some((0, 5))), Some(Generations));
    // The epoch comes first: a fenced old owner never wins.
    assert_eq!(newer_format(Some((2, 1)), Some((1, 9))), Some(Wal));
    assert_eq!(newer_format(Some((1, 9)), Some((2, 1))), Some(Generations));
}

#[tokio::test]
async fn a_keep_that_fails_skips_the_member_with_skip_failed_tenants() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    host.fleet.close_all().await;
    let first = host.lake().load(TextSink::default()).await.unwrap();
    let held: HashMap<_, _> = first
        .members
        .iter()
        .map(|m| (m.key.clone(), m.generation.clone()))
        .collect();
    let load = host
        .lake()
        .skip_failed_tenants()
        .load(TextSink {
            held,
            keep_fails: true,
            ..TextSink::default()
        })
        .await
        .unwrap();
    assert!(load.members.is_empty());
    assert!(
        matches!(&load.skipped[0].reason, SkipReason::Failed(detail) if detail.contains("keep failed")),
        "{:?}",
        load.skipped
    );
}

#[tokio::test]
async fn a_marker_excludes_a_manifest_it_covers_and_keeps_a_newer_one() {
    let host = host(Duration::ZERO);
    let old = host.ship("acme", &["a1"]).await;
    let new = host.ship("acme", &["a2"]).await;
    host.fleet.close_all().await;
    let complete = host.complete("acme");
    assert_eq!(complete.len(), 2);
    // A delete at the time of the old generation, and a new one after it.
    let root = host.replication.root_for(&host.key("acme"));
    let marker = crate::db::fleet_tombstones::Marker {
        deleted_at_ms: complete[0].created_ms,
        purge_after_ms: complete[0].created_ms,
    };
    host.destination
        .put(
            &format!("{root}/{MARKER_OBJECT}"),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
    let lake = || {
        FleetLake::new(Arc::clone(&host.destination), BASE)
            .markers_only()
            .with_scratch_dir(&host.scratch)
    };

    let latest = lake().load(TextSink::default()).await.unwrap();
    assert_eq!(latest.lake.added[0].generation.id, new);

    let at_old = lake()
        .as_of(segment::timestamp_from_ms(complete[0].created_ms))
        .load(TextSink::default())
        .await
        .unwrap();
    assert!(at_old.members.is_empty(), "{old} is covered");
    assert_eq!(at_old.skipped[0].reason, SkipReason::Deleted);
}

// ── A snapshot entry that is a directory ────────────────────────────────────

/// Put a complete generation of `files` (snapshot path, content) under
/// `root`. Returns its id.
pub(super) fn put_generation(
    destination: &dyn ReplicaDestination,
    root: &str,
    created_ms: i64,
    epoch: Option<u64>,
    files: &[(&str, &[u8])],
) -> String {
    use sha2::{Digest as _, Sha256};

    let id = segment::generation_id(created_ms, 1);
    if let Some(epoch) = epoch {
        destination
            .put(&segment::generation_epoch_key(root, &id, epoch), b"")
            .unwrap();
    }
    let mut listed = Vec::new();
    for (path, body) in files {
        let sha256 = hex::encode(Sha256::digest(body));
        destination
            .put(&generations::file_key(root, &id, &sha256), body)
            .unwrap();
        listed.push(generations::ManifestFile {
            path: (*path).to_owned(),
            sha256,
            len: body.len() as u64,
            holder: id.clone(),
        });
    }
    let manifest = Manifest {
        version: generations::MANIFEST_VERSION,
        generation: id.clone(),
        created_ms,
        epoch,
        files: listed,
    };
    destination
        .put(
            &generations::manifest_key(root, &id),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    id
}

/// A sink for a directory entry: `data` holds the key name.
#[derive(Default)]
struct DirSink {
    /// Each member: its entry name and the content of `sub/x`.
    added: Vec<(String, String)>,
    /// The private directory of the last member.
    last_dir: Option<PathBuf>,
    leftover: bool,
}

impl LakeSink for DirSink {
    type Lake = Self;

    fn check_identity(&self, member: LakeMember<'_>) -> Result<(), LakeError> {
        let data = std::fs::read_to_string(member.file.join("data")).unwrap_or_default();
        if data == member.key.name() {
            Ok(())
        } else {
            Err(LakeError::Misplaced {
                tenant: member_name(member.key),
                detail: data,
            })
        }
    }

    fn add(&mut self, member: LakeMember<'_>) -> Result<(), LakeError> {
        assert!(member.file.is_dir(), "the entry is a directory");
        if self.last_dir.as_deref().is_some_and(Path::exists) {
            self.leftover = true;
        }
        self.last_dir = member.file.parent().map(Path::to_path_buf);
        let name = member
            .file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let x = std::fs::read_to_string(member.file.join("sub").join("x")).unwrap();
        self.added.push((name, x));
        Ok(())
    }

    fn finish(self) -> Result<Self, LakeError> {
        Ok(self)
    }
}

#[tokio::test]
async fn a_directory_entry_keeps_its_name_and_is_removed_after_add() {
    let tmp = tempfile::tempdir().unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(tmp.path().join("replicas")).unwrap());
    for (id, x) in [("acme", "one"), ("globex", "two")] {
        let key = tenant_key(TenantDbId::parse(id).unwrap());
        let root = replica_root(BASE, &key);
        put_generation(
            destination.as_ref(),
            &root,
            1_700_000_000_000,
            None,
            &[
                ("t.duck/data", key.name().as_bytes()),
                ("t.duck/sub/x", x.as_bytes()),
            ],
        );
    }
    let load = FleetLake::new(destination, BASE)
        .markers_only()
        .with_workers(1)
        .with_scratch_dir(tmp.path())
        .load(DirSink::default())
        .await
        .unwrap();
    assert_eq!(load.members.len(), 2, "{:?}", load.skipped);
    let sink = load.lake;
    assert_eq!(
        sink.added,
        [
            ("t.duck".to_owned(), "one".to_owned()),
            ("t.duck".to_owned(), "two".to_owned())
        ]
    );
    assert!(!sink.leftover, "core removes the member's directory");
    assert!(!sink.last_dir.unwrap().exists());
}

/// A frozen listing, and a prune of generation `gone` at its first file
/// download.
struct PruneOnDownload {
    inner: Arc<dyn ReplicaDestination>,
    listing: Vec<String>,
    gone: String,
    pruned: std::sync::atomic::AtomicBool,
}

impl ReplicaDestination for PruneOnDownload {
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
        let prefix = format!("/{}/", self.gone);
        if key.contains(&prefix) && !self.pruned.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // The manifest goes first, as in a prune.
            let mut doomed: Vec<String> = self
                .inner
                .list(&format!("{BASE}/fleet/"))?
                .into_iter()
                .filter(|k| k.contains(&prefix))
                .collect();
            doomed.sort_by_key(|k| !k.ends_with(generations::MANIFEST_OBJECT));
            for k in doomed {
                self.inner.delete(&k)?;
            }
        }
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        if prefix.ends_with("/fleet/") {
            Ok(self.listing.clone())
        } else {
            self.inner.list(prefix)
        }
    }
    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn a_generation_pruned_during_the_load_is_planned_again() {
    let host = host(Duration::ZERO);
    host.ship("acme", &["a1"]).await;
    let planned = host.ship("acme", &["a2"]).await;
    let listing = host.destination.list(&format!("{BASE}/fleet/")).unwrap();
    let newer = host.ship("acme", &["a3"]).await;
    host.fleet.close_all().await;

    let destination = Arc::new(PruneOnDownload {
        inner: Arc::clone(&host.destination),
        listing,
        gone: planned.clone(),
        pruned: std::sync::atomic::AtomicBool::new(false),
    });
    let load = FleetLake::new(destination, BASE)
        .markers_only()
        .with_scratch_dir(&host.scratch)
        .load(TextSink::default())
        .await
        .unwrap();
    assert!(load.skipped.is_empty(), "{:?}", load.skipped);
    assert_eq!(load.lake.added.len(), 1);
    assert_eq!(load.lake.added[0].generation.id, newer);
    assert_eq!(load.lake.added[0].values, ["a1", "a2", "a3"]);
    assert_eq!(load.members[0].generation.id, newer);
}
