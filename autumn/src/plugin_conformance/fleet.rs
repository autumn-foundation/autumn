//! Fleet engine conformance (ADR 0040 §7).
//!
//! Checks a [`FleetEngine`] against the engine contract in
//! [`crate::db::fleet_engine`]. Run it in the engine crate's test suite with a
//! [`FleetFixture`]:
//!
//! ```rust,ignore
//! let report = run_fleet_conformance("duckdb", &MyFixture, tmp.path()).await;
//! assert!(report.passed(), "{}", report.to_text_report());
//! ```
//!
//! | Check | Fails when |
//! |---|---|
//! | `fleet-identity` | A copy of one key's database opens as another key. |
//! | `fleet-epoch` | An open under an epoch below the stamp is not refused, or the stamp goes down. |
//! | `fleet-reopen` | A closed database stays in use, or its data is gone after a reopen. |
//! | `fleet-snapshot-restore` | A snapshot put at a database path does not open with the data. |
//! | `fleet-delete` | A delete leaves the database, or its data comes back in a new database. |
//! | `fleet-delete-restore` | The fixture's restore fails before the delete, or brings the database back after it. Skipped without a restore. |
//! | `fleet-budget` | An open database reports no memory, or the budget does not close the oldest idle one. |
//!
//! Each check uses its own fleet root under the scratch directory.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::DatabaseFleetConfig;
use crate::db::fleet::{DatabaseFleet, FleetError};
use crate::db::fleet_engine::FleetEngine;
use crate::fleet_layout::{FleetDbKey, FleetMode};
use crate::lock::FencingToken;
use crate::plugin_conformance::{CheckResult, CheckStatus, ConformanceReport};

/// How long one check may run before it fails.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a check waits for the fleet to close a database.
const SETTLE: Duration = Duration::from_secs(10);

/// What the kit needs from an engine crate: a fleet of the engine, and a way
/// to write and read a value through a handle.
pub trait FleetFixture: Send + Sync {
    /// The engine under test.
    type Engine: FleetEngine;

    /// A fleet of the engine with `config`. Install what the fleet needs
    /// here (replication, lifecycle hooks).
    fn fleet(
        &self,
        config: DatabaseFleetConfig,
    ) -> impl Future<Output = Result<DatabaseFleet<Self::Engine>, String>> + Send;

    /// Stores `value` in the database.
    fn write(
        &self,
        handle: &<Self::Engine as FleetEngine>::Handle,
        value: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Every value stored in the database, in write order.
    fn read(
        &self,
        handle: &<Self::Engine as FleetEngine>::Handle,
    ) -> impl Future<Output = Result<Vec<String>, String>> + Send;

    /// Restores `key` from its replica. `None` when the fleet has no
    /// replica: the kit then skips `fleet-delete-restore`.
    fn restore(
        &self,
        fleet: &DatabaseFleet<Self::Engine>,
        key: &FleetDbKey,
    ) -> impl Future<Output = Option<Result<(), FleetError>>> + Send {
        let _ = (fleet, key);
        async { None }
    }
}

/// The result of one check: a pass message, or why it failed. `Ok(None)`
/// skips the check.
type Outcome = Result<Option<String>, String>;

/// Runs every check against `fixture`. `name` names the engine in the
/// report. `scratch` holds the fleet roots: use an empty temporary
/// directory.
#[must_use]
pub async fn run_fleet_conformance<F: FleetFixture>(
    name: &str,
    fixture: &F,
    scratch: &Path,
) -> ConformanceReport {
    let mut checks = vec![
        check("fleet-identity", identity(fixture, scratch)).await,
        check("fleet-epoch", epoch(fixture, scratch)).await,
        check("fleet-reopen", reopen(fixture, scratch)).await,
        check("fleet-snapshot-restore", snapshot_restore(fixture, scratch)).await,
    ];
    let (deleted, restored) = delete(fixture, scratch).await;
    checks.push(result("fleet-delete", deleted));
    checks.push(result("fleet-delete-restore", restored));
    checks.push(check("fleet-budget", budget(fixture, scratch)).await);
    ConformanceReport {
        plugin_name: name.to_owned(),
        checks,
    }
}

async fn check(name: &str, outcome: impl Future<Output = Outcome>) -> CheckResult {
    let outcome = tokio::time::timeout(CHECK_TIMEOUT, outcome)
        .await
        .unwrap_or_else(|_| Err(format!("the check did not end in {CHECK_TIMEOUT:?}")));
    result(name, outcome)
}

fn result(name: &str, outcome: Outcome) -> CheckResult {
    let (status, message, diagnostics) = match outcome {
        Ok(Some(message)) => (CheckStatus::Pass, message, Vec::new()),
        Ok(None) => (
            CheckStatus::Skip,
            "the fixture has no replica restore".to_owned(),
            Vec::new(),
        ),
        Err(problem) => (CheckStatus::Fail, problem.clone(), vec![problem]),
    };
    CheckResult {
        name: name.to_owned(),
        status,
        message,
        diagnostics,
    }
}

/// A tenant fleet that creates on demand, with no budget.
fn config(root: &Path) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open: 64,
        max_resident_bytes: 0,
        min_residency: Duration::ZERO,
        pool_size: 2,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: Duration::ZERO,
        restore_check_sample: 0,
        replication_workers: 1,
    }
}

async fn fleet<F: FleetFixture>(
    fixture: &F,
    scratch: &Path,
    dir: &str,
    budget: u64,
) -> Result<DatabaseFleet<F::Engine>, String> {
    let mut config = config(&scratch.join(dir).join("fleet"));
    config.max_resident_bytes = budget;
    fixture
        .fleet(config)
        .await
        .map_err(|e| format!("the fixture could not make a fleet: {e}"))
}

fn key<E: FleetEngine>(fleet: &DatabaseFleet<E>, tenant: &str) -> Result<FleetDbKey, String> {
    fleet.key_for(tenant).map_err(|e| e.to_string())
}

fn fail(what: &str) -> impl FnOnce(FleetError) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Puts the `files` of a snapshot of `source` in `dir` at `target`: the
/// entry named like `source` takes the name of `target`.
fn place(files: &[PathBuf], dir: &Path, source: &Path, target: &Path) -> Result<(), String> {
    let parent = target.parent().ok_or("the target has no parent")?;
    let name = target.file_name().ok_or("the target has no file name")?;
    let own = source.file_name().ok_or("the source has no file name")?;
    for file in files {
        let relative = file
            .strip_prefix(dir)
            .map_err(|_| format!("{} is not in the snapshot directory", file.display()))?;
        if relative
            .components()
            .next()
            .map(std::path::Component::as_os_str)
            != Some(own)
        {
            return Err(format!(
                "the snapshot wrote {}: every file must be `{}` or under it",
                relative.display(),
                own.to_string_lossy()
            ));
        }
        if !file.is_file() {
            return Err(format!(
                "the snapshot lists {}, which is no file",
                file.display()
            ));
        }
        // The first component is the source database. The rest is its
        // layout, kept as is.
        let rest: PathBuf = relative.components().skip(1).collect();
        let dest = if rest.as_os_str().is_empty() {
            parent.join(name)
        } else {
            parent.join(name).join(rest)
        };
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::copy(file, &dest).map_err(|e| format!("copy {}: {e}", file.display()))?;
    }
    Ok(())
}

/// Takes a snapshot of `key` into a new directory under `scratch`.
async fn snapshot<E: FleetEngine>(
    fleet: &DatabaseFleet<E>,
    key: &FleetDbKey,
    dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let files = fleet
        .snapshot(key, dir)
        .await
        .map_err(fail("snapshot failed"))?;
    if files.is_empty() {
        return Err("the snapshot wrote no file".to_owned());
    }
    if let Some(outside) = files.iter().find(|file| !file.starts_with(dir)) {
        return Err(format!(
            "the snapshot wrote {} outside its directory",
            outside.display()
        ));
    }
    Ok(files)
}

/// Waits until `cond` holds, for at most [`SETTLE`].
async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let polls = SETTLE.as_millis() / 10;
    for _ in 0..polls {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

async fn identity<F: FleetFixture>(fixture: &F, scratch: &Path) -> Outcome {
    let fleet = fleet(fixture, scratch, "identity", 0).await?;
    let (a, b) = (key(&fleet, "conformance-a")?, key(&fleet, "conformance-b")?);
    let db = fleet.open(&a).await.map_err(fail("open failed"))?;
    fixture.write(db.handle(), "a").await?;
    drop(db);
    let dir = scratch.join("identity").join("snapshot");
    let files = snapshot(&fleet, &a, &dir).await?;
    fleet.close(&a).await;
    place(&files, &dir, &fleet.path_of(&a), &fleet.path_of(&b))?;
    let opened = fleet.open(&b).await;
    fleet.close_all().await;
    match opened {
        Err(FleetError::Misplaced { found, .. }) if found == a.name() => Ok(Some(
            "a copy of one key's database is refused under another key".to_owned(),
        )),
        Err(FleetError::Misplaced { found, .. }) => {
            Err(format!("the refusal names {found}, not {}", a.name()))
        }
        Err(other) => Err(format!(
            "the open failed, but not with `Misplaced`: {other}"
        )),
        Ok(_) => Err(format!(
            "a copy of {} opened as {}: the engine does not check the key",
            a.name(),
            b.name()
        )),
    }
}

fn token(epoch: i64) -> Result<FencingToken, String> {
    FencingToken::try_from(epoch).map_err(|e| e.to_string())
}

async fn epoch<F: FleetFixture>(fixture: &F, scratch: &Path) -> Outcome {
    let fleet = fleet(fixture, scratch, "epoch", 0).await?;
    let engine = fleet.engine();
    let key = key(&fleet, "conformance-epoch")?;
    let path = fleet.path_of(&key);
    let open = |epoch: i64| {
        let (key, path) = (key.clone(), path.clone());
        async move {
            let token = token(epoch)?;
            Ok::<_, String>(engine.open(&key, &path, Some(token)).await)
        }
    };
    let close = |handle: &<F::Engine as FleetEngine>::Handle| {
        engine.close(handle);
    };
    let first = open(5).await?.map_err(fail("open under epoch 5 failed"))?;
    close(&first);
    match open(3).await? {
        Err(FleetError::NewerEpoch { found: 5, .. }) => {}
        Err(other) => {
            return Err(format!(
                "an open under epoch 3 of a database stamped 5 failed, but not with \
                 `NewerEpoch {{ found: 5 }}`: {other}"
            ));
        }
        Ok(handle) => {
            close(&handle);
            return Err("an open under epoch 3 of a database stamped 5 was accepted".to_owned());
        }
    }
    let newer = open(7).await?.map_err(fail("open under epoch 7 failed"))?;
    close(&newer);
    match open(6).await? {
        Err(FleetError::NewerEpoch { found: 7, .. }) => {}
        Err(other) => {
            return Err(format!(
                "an open under epoch 6 after epoch 7 failed, but not with \
                 `NewerEpoch {{ found: 7 }}`: {other}"
            ));
        }
        Ok(handle) => {
            close(&handle);
            return Err("the stamp did not move up to 7: epoch 6 was accepted".to_owned());
        }
    }
    let again = open(7)
        .await?
        .map_err(fail("a second open under epoch 7 failed"))?;
    close(&again);
    fleet.close_all().await;
    Ok(Some(
        "a lower epoch is refused, the stamp moves up, the same epoch opens again".to_owned(),
    ))
}

async fn reopen<F: FleetFixture>(fixture: &F, scratch: &Path) -> Outcome {
    let fleet = fleet(fixture, scratch, "reopen", 0).await?;
    let key = key(&fleet, "conformance-reopen")?;
    let db = fleet.open(&key).await.map_err(fail("open failed"))?;
    fixture.write(db.handle(), "first").await?;
    fleet.close(&key).await;
    if !db.closed().is_cancelled() {
        return Err("the fleet did not mark the database closed".to_owned());
    }
    if fleet.engine().in_use(db.handle()) {
        return Err("the engine reports the closed database still in use".to_owned());
    }
    drop(db);
    let again = fleet.open(&key).await.map_err(fail("the reopen failed"))?;
    let values = fixture.read(again.handle()).await?;
    drop(again);
    fleet.close_all().await;
    if values != ["first"] {
        return Err(format!(
            "after a close and a reopen the database holds {values:?}, not [\"first\"]"
        ));
    }
    Ok(Some("a closed database reopens with its data".to_owned()))
}

async fn snapshot_restore<F: FleetFixture>(fixture: &F, scratch: &Path) -> Outcome {
    let source = fleet(fixture, scratch, "snapshot-source", 0).await?;
    let key = key(&source, "conformance-snapshot")?;
    let db = source.open(&key).await.map_err(fail("open failed"))?;
    fixture.write(db.handle(), "kept").await?;
    drop(db);
    let dir = scratch.join("snapshot-files");
    let files = snapshot(&source, &key, &dir).await?;
    source.close_all().await;
    let target = fleet(fixture, scratch, "snapshot-target", 0).await?;
    place(&files, &dir, &source.path_of(&key), &target.path_of(&key))?;
    let restored = target
        .open(&key)
        .await
        .map_err(fail("the restored copy did not open"))?;
    let values = fixture.read(restored.handle()).await?;
    drop(restored);
    target.close_all().await;
    if values != ["kept"] {
        return Err(format!(
            "the restored copy holds {values:?}, not [\"kept\"]"
        ));
    }
    Ok(Some(format!(
        "a snapshot of {} file(s) restores with its data",
        files.len()
    )))
}

/// The delete check, and the restore after it.
async fn delete<F: FleetFixture>(fixture: &F, scratch: &Path) -> (Outcome, Outcome) {
    let run = async {
        let fleet = fleet(fixture, scratch, "delete", 0).await?;
        let key = key(&fleet, "conformance-delete")?;
        let db = fleet.open(&key).await.map_err(fail("open failed"))?;
        fixture.write(db.handle(), "gone").await?;
        drop(db);
        // A positive control: the restore works while the database is
        // live, so a refusal after the delete is the delete's.
        fleet.close(&key).await;
        let control = fixture.restore(&fleet, &key).await;
        if let Some(Err(error)) = &control {
            return Err(format!(
                "the restore failed before the delete, so the check cannot run: {error}"
            ));
        }
        match fleet.delete(&key).await {
            Ok(true) => {}
            Ok(false) => return Err("the delete found no database".to_owned()),
            Err(e) => return Err(format!("the delete failed: {e}")),
        }
        if fleet.exists(&key) {
            return Err("the database is still on disk after the delete".to_owned());
        }
        let restored = match (control, fixture.restore(&fleet, &key).await) {
            (None, _) | (_, None) => Ok(None),
            (_, Some(Ok(()))) => Err("the restore brought a deleted database back".to_owned()),
            (_, Some(Err(refusal))) => Ok(Some(format!("the restore is refused: {refusal}"))),
        };
        let fresh = fleet
            .open(&key)
            .await
            .map_err(fail("the open after the delete failed"))?;
        let values = fixture.read(fresh.handle()).await?;
        drop(fresh);
        fleet.close_all().await;
        if !values.is_empty() {
            return Err(format!(
                "a new database at the deleted key holds old data: {values:?}"
            ));
        }
        Ok((
            Some("a delete removes the database and its data".to_owned()),
            restored,
        ))
    };
    match tokio::time::timeout(CHECK_TIMEOUT, run).await {
        Ok(Ok((deleted, restored))) => (Ok(deleted), restored),
        Ok(Err(problem)) => (
            Err(problem),
            Err("the delete check failed first".to_owned()),
        ),
        Err(_) => {
            let late = format!("the check did not end in {CHECK_TIMEOUT:?}");
            (Err(late.clone()), Err(late))
        }
    }
}

async fn budget<F: FleetFixture>(fixture: &F, scratch: &Path) -> Outcome {
    // One byte: any two open databases are over it.
    let fleet = fleet(fixture, scratch, "budget", 1).await?;
    let (a, b) = (
        key(&fleet, "conformance-old")?,
        key(&fleet, "conformance-new")?,
    );
    let db = fleet.open(&a).await.map_err(fail("open failed"))?;
    fixture.write(db.handle(), "a").await?;
    if fleet.resident_bytes() == 0 {
        return Err(
            "the engine reports 0 bytes for an open database: the budget cannot close it"
                .to_owned(),
        );
    }
    drop(db);
    drop(fleet.open(&b).await.map_err(fail("open failed"))?);
    let closed = eventually(|| fleet.open_keys() == [b.clone()]).await;
    let open = fleet.open_keys();
    fleet.close_all().await;
    if !closed {
        return Err(format!(
            "over the budget the oldest idle database stays open: {open:?}"
        ));
    }
    Ok(Some(
        "over the budget the oldest idle database closes".to_owned(),
    ))
}

#[cfg(test)]
mod tests;
