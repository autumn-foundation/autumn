//! Online-safe `SQLite` snapshot and restore for `autumn db backup` /
//! `autumn db restore` (issue #1909).
//!
//! `pg_dump`/`pg_restore` are Postgres-only. The `SQLite` tier (#1614) ships one
//! binary and no external tools, so a `SQLite` target uses `VACUUM INTO`. One
//! transactional statement copies the database. In WAL mode the copy does not
//! block the writer, so a backup of a live app neither corrupts nor stalls it.
//!
//! `verify` calls `autumn_web::replication::sqlite::integrity_check`, the same
//! check the in-process replication verifier uses.
//!
//! diesel's `SQLite` backend is in the CLI's dependency graph unconditionally
//! (the workspace `diesel` pin enables it), so none of this needs the
//! `--features sqlite` backend flip, exactly like `autumn db replica`.

use std::path::{Path, PathBuf};

use autumn_web::replication;
use diesel::connection::SimpleConnection as _;
use diesel::prelude::*;
use diesel::sql_types::Text;

/// How long a snapshot/restore connection waits for a lock before giving up.
///
/// Longer than the replicator's one-second tick. A backup is one-shot and an
/// operator waits for it. On a rollback-journal database a writer blocks readers,
/// so wait it out rather than fail.
const BUSY_TIMEOUT_MS: u32 = 30_000;

/// Failure modes of a `SQLite` snapshot or restore. `Display` is credential-safe:
/// a `SQLite` target carries no password, and the one variant that can be reached
/// with a foreign URL redacts it.
#[derive(Debug)]
pub enum SnapshotError {
    /// The target names no database file (an in-memory database, which no
    /// snapshot can outlive).
    NotAFile {
        /// The configured target. Redacted before display: a mis-dispatched
        /// restore can reach here with a Postgres URL, which carries a password.
        target: String,
    },
    /// The database file does not exist.
    Missing {
        /// The resolved path.
        path: String,
    },
    /// The database (or artifact) could not be opened.
    Open {
        /// The resolved path.
        path: String,
        /// diesel/`SQLite` detail.
        detail: String,
    },
    /// A statement failed.
    Query {
        /// What was being run.
        op: &'static str,
        /// diesel/`SQLite` detail.
        detail: String,
    },
    /// The artifact failed `PRAGMA integrity_check`.
    Integrity {
        /// `SQLite`'s first complaint.
        detail: String,
    },
    /// A filesystem operation failed.
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        detail: String,
    },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAFile { target } => write!(
                f,
                "the target {:?} names no SQLite database file (an in-memory database cannot \
                 be backed up, and a Postgres URL is not a SQLite target).\n  Point \
                 `database.url` at a file, e.g. sqlite:///var/lib/myapp/app.db.",
                replication::redact_credentials(target)
            ),
            Self::Missing { path } => write!(
                f,
                "the SQLite database file {path} does not exist.\n  Run `autumn migrate` (or \
                 start the app) to create it before backing it up."
            ),
            Self::Open { path, detail } => {
                write!(f, "could not open the SQLite database at {path}: {detail}")
            }
            Self::Query { op, detail } => write!(f, "{op} failed: {detail}"),
            Self::Integrity { detail } => write!(f, "SQLite integrity check failed: {detail}"),
            Self::Io { context, detail } => write!(f, "{context}: {detail}"),
        }
    }
}

impl SnapshotError {
    fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |e| Self::Io {
            context,
            detail: e.to_string(),
        }
    }
}

/// Resolve the database FILE a `SQLite` target names, refusing every in-memory
/// spelling.
///
/// The spelling rules come from `autumn_web::replication::database_file`, the
/// runtime's own resolver, so the CLI and the app agree on which file a URL
/// means.
///
/// # Errors
///
/// [`SnapshotError::NotAFile`] when the target is in-memory (or not a `SQLite`
/// URL at all).
pub fn database_path(target: &str) -> Result<PathBuf, SnapshotError> {
    replication::database_file(target).ok_or_else(|| SnapshotError::NotAFile {
        target: target.to_owned(),
    })
}

/// Open a private connection to a `SQLite` database file.
///
/// Never creates the file. `sqlite3_open` creates an empty database, so a typo'd
/// path would back up zero tables and report success.
fn open(path: &Path) -> Result<SqliteConnection, SnapshotError> {
    if !path.exists() {
        return Err(SnapshotError::Missing {
            path: path.display().to_string(),
        });
    }
    // Through the runtime's own `connection_string`, so the CLI opens exactly what
    // the app opens. It refuses a path no `&str` can carry (`file:app%FF.db`
    // decodes to such a name), and it keeps a filename that itself begins with
    // `file:` from being re-read as a URI naming a different database.
    let Some(target) = replication::connection_string(path) else {
        return Err(SnapshotError::Open {
            path: path.display().to_string(),
            detail: "the path is not valid UTF-8, and SQLite is opened through a UTF-8 \
                     connection string"
                .to_owned(),
        });
    };
    let mut conn = SqliteConnection::establish(&target).map_err(|e| SnapshotError::Open {
        path: target.clone(),
        detail: e.to_string(),
    })?;
    conn.batch_execute(&format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"))
        .map_err(|e| SnapshotError::Query {
            op: "busy_timeout",
            detail: e.to_string(),
        })?;
    Ok(conn)
}

/// Write an online-safe snapshot of the `SQLite` database `target` names into
/// `out_path`.
///
/// `VACUUM INTO` runs inside a read transaction, so the snapshot is a single
/// consistent point in time even while the app writes. `out_path` must not name
/// an existing database; the caller's fresh backup run directory guarantees that.
///
/// The artifact is flushed before it is reported, so retention cannot prune a
/// good older run in favour of one a power loss would have left empty.
///
/// # Errors
///
/// See [`SnapshotError`].
pub fn snapshot(target: &str, out_path: &Path) -> Result<(), SnapshotError> {
    let db = database_path(target)?;
    let mut conn = open(&db)?;
    let out = out_path.to_string_lossy().into_owned();
    diesel::sql_query("VACUUM INTO ?")
        .bind::<Text, _>(&out)
        .execute(&mut conn)
        .map_err(|e| SnapshotError::Query {
            op: "VACUUM INTO",
            detail: e.to_string(),
        })?;
    flush(out_path)
}

/// Grade a `SQLite` artifact: it must be a non-empty file that opens and passes
/// `PRAGMA integrity_check`.
///
/// # Errors
///
/// See [`SnapshotError`].
pub fn verify(path: &Path) -> Result<(), SnapshotError> {
    let len = std::fs::metadata(path)
        .map_err(SnapshotError::io(format!("stat {}", path.display())))?
        .len();
    if len == 0 {
        return Err(SnapshotError::Integrity {
            detail: format!("{} is empty (0 bytes)", path.display()),
        });
    }
    let mut conn = open(path)?;
    replication::sqlite::integrity_check(&mut conn).map_err(|e| SnapshotError::Integrity {
        detail: e.to_string(),
    })
}

/// Every sidecar `SQLite` can leave beside a database file.
///
/// Both journal modes matter. WAL mode leaves `-wal` and `-shm`; the default
/// rollback-journal mode leaves `-journal`. A restore must clear all three: each
/// describes pages of the file it replaces, and `SQLite` replays a hot `-journal`
/// (or `-wal`) onto whatever now sits at that path.
///
/// `VACUUM INTO` writes its output in rollback-journal mode whatever the source
/// used, so a restored database really can acquire a `-journal`.
fn sidecars(db: &Path) -> [PathBuf; 3] {
    let mut journal = db.as_os_str().to_owned();
    journal.push("-journal");
    [
        replication::wal::wal_path(db),
        replication::wal::shm_path(db),
        PathBuf::from(journal),
    ]
}

/// A staging path beside `db`, unique to this process.
///
/// Unique because a fixed name lets two concurrent restores stage over each
/// other and publish a half-copied file.
fn staging_path(db: &Path) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(format!(".autumn-restore-{}.tmp", std::process::id()));
    PathBuf::from(path)
}

/// Copy `artifact` to `staged`, give it the target's mode and owner, verify it,
/// and flush it to disk.
///
/// The staged copy is verified again, not just the source: a short write or a
/// full disk must not survive as far as the rename.
fn stage(artifact: &Path, staged: &Path, db: &Path) -> Result<(), SnapshotError> {
    std::fs::copy(artifact, staged).map_err(SnapshotError::io(format!(
        "copying {} to {}",
        artifact.display(),
        staged.display()
    )))?;
    inherit_target_permissions(db, staged)?;
    verify(staged)?;
    flush(staged)
}

/// Flush a file to disk.
///
/// Opened for WRITING, not with `File::open`: Windows refuses `FlushFileBuffers`
/// on a read-only handle with "Access is denied", where POSIX allows `fsync` on
/// a read-only descriptor.
fn flush(path: &Path) -> Result<(), SnapshotError> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(SnapshotError::io(format!("opening {}", path.display())))?;
    file.sync_all()
        .map_err(SnapshotError::io(format!("flushing {}", path.display())))
}

/// Give `staged` the mode and owner of the database it replaces.
///
/// `std::fs::copy` copies the ARTIFACT's mode, and the staged file belongs to
/// whoever ran the command. Without this a `0600` database can come back
/// world-readable, and a restore run under `sudo` can leave a root-owned file the
/// service account cannot write.
///
/// Best-effort on ownership: only a privileged user may change it, so a failed
/// `chown` is ignored rather than failing a restore that is otherwise correct.
/// A missing target (a first restore onto a fresh path) leaves both alone.
#[cfg(unix)]
fn inherit_target_permissions(db: &Path, staged: &Path) -> Result<(), SnapshotError> {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(existing) = std::fs::metadata(db) else {
        return Ok(());
    };
    std::fs::set_permissions(staged, existing.permissions()).map_err(SnapshotError::io(
        format!("setting the mode of {}", staged.display()),
    ))?;
    let _ = std::os::unix::fs::chown(staged, Some(existing.uid()), Some(existing.gid()));
    Ok(())
}

/// Non-unix mode inheritance: `set_permissions` carries the read-only flag, and
/// there is no ownership to copy.
#[cfg(not(unix))]
fn inherit_target_permissions(db: &Path, staged: &Path) -> Result<(), SnapshotError> {
    let Ok(existing) = std::fs::metadata(db) else {
        return Ok(());
    };
    std::fs::set_permissions(staged, existing.permissions()).map_err(SnapshotError::io(format!(
        "setting the mode of {}",
        staged.display()
    )))
}

/// Resolve a symlink at the configured database path.
///
/// A deployed `SQLite` app reaches its database through a link in the release
/// dir (`autumn deploy` keeps the real file in `shared/data`). Renaming over the
/// LINK would detach the app from its own database and let the next deploy undo
/// the restore, and the sidecars would be cleared in the wrong directory.
///
/// Falls back to the given path when it names nothing yet, and resolves a
/// dangling link by hand — `canonicalize` refuses one.
fn follow_link(db: PathBuf) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(&db) {
        return real;
    }
    match std::fs::read_link(&db) {
        Ok(target) if target.is_absolute() => target,
        Ok(target) => db
            .parent()
            .map_or_else(|| target.clone(), |dir| dir.join(&target)),
        Err(_) => db,
    }
}

/// Flush a directory entry so a rename survives a power loss. Best-effort:
/// opening a directory is not portable, and a failure here costs durability, not
/// correctness.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Restore `artifact` over the `SQLite` database `target` names.
///
/// **Stop the app first.** A running process keeps its own open file handles, so
/// after the rename it serves the old database and loses every write it makes.
///
/// A symlink at the configured path is followed first, so a deployed app whose
/// release dir links to `shared/data` gets its real database replaced, not the
/// link.
///
/// The artifact is verified, copied beside the target, verified again, flushed,
/// and renamed onto it, so a failure part-way through never publishes a
/// half-written database.
///
/// The sidecars go BEFORE the rename. A `-wal` or `-journal` left from the old
/// database describes pages of a file that no longer exists, and `SQLite` would
/// replay it onto the restored one. The reverse order has a worse failure: a
/// removal that fails after the rename leaves a stale journal over the NEW
/// database. Losing the old WAL costs nothing on the success path — the restore
/// discards that database anyway — but a rename that fails afterwards has already
/// discarded it, so its error says so.
///
/// # Errors
///
/// See [`SnapshotError`].
pub fn restore(artifact: &Path, target: &str) -> Result<(), SnapshotError> {
    let db = follow_link(database_path(target)?);
    verify(artifact)?;

    if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(SnapshotError::io(format!("creating {}", parent.display())))?;
    }

    // Stage beside the target so the final `rename` is same-filesystem (atomic).
    let staged = staging_path(&db);
    let _ = std::fs::remove_file(&staged);
    if let Err(e) = stage(artifact, &staged, &db) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }

    for sidecar in sidecars(&db) {
        if let Err(e) = std::fs::remove_file(&sidecar)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            let _ = std::fs::remove_file(&staged);
            return Err(SnapshotError::io(format!("removing {}", sidecar.display()))(e));
        }
    }

    if let Err(e) = std::fs::rename(&staged, &db) {
        let _ = std::fs::remove_file(&staged);
        return Err(SnapshotError::Io {
            context: format!(
                "replacing {} with the restored database. Its write-ahead log was already \
                 removed, so uncommitted-to-disk writes from before the restore are gone",
                db.display()
            ),
            detail: e.to_string(),
        });
    }
    if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed a WAL-mode database with one row and leave the connection open, so
    /// the file has a live `-wal` beside it (the realistic live-app shape).
    fn seeded(path: &Path) -> SqliteConnection {
        let mut conn =
            SqliteConnection::establish(&path.to_string_lossy()).expect("establish sqlite");
        conn.batch_execute(
            "PRAGMA journal_mode = WAL; \
             CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL); \
             INSERT INTO t (v) VALUES ('a'), ('b');",
        )
        .expect("seed");
        conn
    }

    #[derive(QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    fn row_count(path: &Path) -> i64 {
        let mut conn =
            SqliteConnection::establish(&path.to_string_lossy()).expect("establish sqlite");
        let rows: Vec<Count> = diesel::sql_query("SELECT COUNT(*) AS n FROM t")
            .load(&mut conn)
            .expect("count");
        rows.first().map_or(-1, |r| r.n)
    }

    #[test]
    fn database_path_resolves_every_accepted_spelling_and_refuses_in_memory() {
        assert_eq!(
            database_path("sqlite:///var/lib/app.db").expect("abs"),
            PathBuf::from("/var/lib/app.db")
        );
        assert_eq!(
            database_path("sqlite:app.db").expect("rel"),
            PathBuf::from("app.db")
        );
        for memory in ["sqlite::memory:", "sqlite://:memory:", "sqlite://"] {
            let err = database_path(memory).expect_err("in-memory must be refused");
            assert!(
                matches!(err, SnapshotError::NotAFile { .. }),
                "{memory}: {err}"
            );
            assert!(
                err.to_string().contains("in-memory"),
                "the refusal must name the reason: {err}"
            );
        }
        // A mis-dispatched restore can reach here with a Postgres URL, which
        // carries a password. It must be refused, and the password must not be
        // printed.
        let err = database_path("postgres://app:hunter2@db.example.com/app")
            .expect_err("a Postgres URL is not a SQLite target");
        let message = err.to_string();
        assert!(!message.contains("hunter2"), "credential leaked: {message}");
        assert!(message.contains("<redacted>@"), "{message}");
    }

    /// The resolver and the database engine must name the SAME file. diesel opens
    /// with `SQLITE_OPEN_URI`, so a `file:` target is percent-decoded — this opens
    /// one through diesel and asserts the file that appears on disk is the one
    /// `database_path` reports.
    #[test]
    fn database_path_names_the_file_diesel_actually_opens_for_a_file_uri() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("file:{}/app%20data.db?mode=rwc", dir.path().display());

        let mut conn = SqliteConnection::establish(&url).expect("diesel opens the URI");
        conn.batch_execute("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .expect("write");
        drop(conn);

        let resolved = database_path(&url).expect("a file: URI names a file");
        assert!(
            resolved.is_file(),
            "the resolver named {resolved:?}, which does not exist"
        );
        assert_eq!(resolved, dir.path().join("app data.db"));
    }

    #[test]
    fn snapshot_captures_a_live_wal_database_and_the_copy_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("app.db");
        // The seeding connection stays open for the whole test: the snapshot must
        // work against a database another connection is holding, WAL and all.
        let mut live = seeded(&db);
        assert!(
            dir.path().join("app.db-wal").exists(),
            "the seed must leave a -wal beside the database"
        );

        let out = dir.path().join("control.sqlite");
        snapshot(&format!("sqlite://{}", db.display()), &out).expect("snapshot");
        verify(&out).expect("the snapshot must verify");
        assert_eq!(row_count(&out), 2, "the snapshot must carry the WAL's rows");

        // …and the source is still writable afterwards: the backup neither locked
        // nor corrupted the live database.
        live.batch_execute("INSERT INTO t (v) VALUES ('c');")
            .expect("the live app must still write after a backup");
        assert_eq!(row_count(&db), 3);
        assert_eq!(row_count(&out), 2, "the snapshot is a point in time");
    }

    #[test]
    fn snapshot_refuses_a_missing_database_instead_of_creating_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("absent.db");
        let out = dir.path().join("control.sqlite");
        let err = snapshot(&format!("sqlite://{}", db.display()), &out)
            .expect_err("a missing database must fail");
        assert!(matches!(err, SnapshotError::Missing { .. }), "{err}");
        assert!(
            !db.exists(),
            "the failed backup must not create the database"
        );
        assert!(!out.exists(), "no artifact may be left behind");
    }

    #[test]
    fn verify_rejects_an_empty_and_a_truncated_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = dir.path().join("empty.sqlite");
        std::fs::write(&empty, b"").expect("write");
        assert!(matches!(
            verify(&empty).expect_err("empty must fail"),
            SnapshotError::Integrity { .. }
        ));

        let db = dir.path().join("app.db");
        drop(seeded(&db));
        let good = dir.path().join("good.sqlite");
        snapshot(&format!("sqlite://{}", db.display()), &good).expect("snapshot");
        let bytes = std::fs::read(&good).expect("read");
        let torn = dir.path().join("torn.sqlite");
        std::fs::write(&torn, &bytes[..bytes.len() / 2]).expect("write");
        verify(&torn).expect_err("a truncated artifact must fail");
    }

    #[test]
    fn restore_replaces_the_target_and_clears_the_stale_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.db");
        drop(seeded(&source));
        let artifact = dir.path().join("control.sqlite");
        snapshot(&format!("sqlite://{}", source.display()), &artifact).expect("snapshot");

        // The restore target is a DIFFERENT database, left with the -wal/-shm a
        // crashed app leaves behind (a clean close checkpoints and removes them).
        // Those describe pages of a file the restore is about to replace, so
        // SQLite would replay them onto the restored database and corrupt it.
        let target = dir.path().join("target.db");
        let mut live = seeded(&target);
        live.batch_execute("INSERT INTO t (v) VALUES ('c');")
            .expect("third row");
        drop(live);
        let wal = dir.path().join("target.db-wal");
        let shm = dir.path().join("target.db-shm");
        std::fs::write(&wal, b"stale wal frames").expect("write");
        std::fs::write(&shm, b"stale shm").expect("write");

        restore(&artifact, &format!("sqlite://{}", target.display())).expect("restore");
        assert!(!wal.exists(), "the stale -wal must be removed");
        assert!(!shm.exists(), "the stale -shm must be removed");
        assert_eq!(row_count(&target), 2, "the target must hold the artifact");
    }

    /// The rollback-journal sidecar is the one a WAL-only guard misses.
    /// `VACUUM INTO` writes its output in rollback-journal mode whatever the
    /// source used, so a restored database really can acquire a `-journal`, and a
    /// hot one left from the OLD database is replayed onto the new file and
    /// destroys it.
    #[test]
    fn restore_clears_the_rollback_journal_sidecar_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.db");
        drop(seeded(&source));
        let artifact = dir.path().join("control.sqlite");
        snapshot(&format!("sqlite://{}", source.display()), &artifact).expect("snapshot");

        let target = dir.path().join("target.db");
        drop(seeded(&target));
        let journal = dir.path().join("target.db-journal");
        std::fs::write(&journal, b"stale rollback journal").expect("write");

        restore(&artifact, &format!("sqlite://{}", target.display())).expect("restore");
        assert!(!journal.exists(), "the stale -journal must be removed");
        assert_eq!(row_count(&target), 2);
    }

    /// A `0600` database must not come back world-readable because the artifact
    /// was `0644` (an offsite download is created with the default umask).
    #[cfg(unix)]
    #[test]
    fn restore_keeps_the_targets_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.db");
        drop(seeded(&source));
        let artifact = dir.path().join("control.sqlite");
        snapshot(&format!("sqlite://{}", source.display()), &artifact).expect("snapshot");
        std::fs::set_permissions(&artifact, std::fs::Permissions::from_mode(0o644))
            .expect("chmod artifact");

        let target = dir.path().join("target.db");
        drop(seeded(&target));
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .expect("chmod target");

        restore(&artifact, &format!("sqlite://{}", target.display())).expect("restore");
        let mode = std::fs::metadata(&target)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the restored database must keep the target's mode"
        );
    }

    /// `autumn deploy` links a release at `shared/data/app.db`, so the configured
    /// path is a symlink. A restore must replace the FILE the link points at.
    /// Replacing the link would detach the app from its database and let the next
    /// deploy re-link over the restored data.
    #[cfg(unix)]
    #[test]
    fn restore_follows_a_symlink_to_the_real_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.db");
        drop(seeded(&source));
        let artifact = dir.path().join("control.sqlite");
        snapshot(&format!("sqlite://{}", source.display()), &artifact).expect("snapshot");

        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).expect("mkdir");
        let real = shared.join("app.db");
        let mut live = seeded(&real);
        live.batch_execute("INSERT INTO t (v) VALUES ('c');")
            .expect("third row");
        drop(live);
        let stale_wal = shared.join("app.db-wal");
        std::fs::write(&stale_wal, b"stale wal frames").expect("write");

        let release = dir.path().join("release");
        std::fs::create_dir_all(&release).expect("mkdir");
        let link = release.join("app.db");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        restore(&artifact, &format!("sqlite://{}", link.display())).expect("restore");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("stat")
                .file_type()
                .is_symlink(),
            "the release path must still be a link, not a real file"
        );
        assert_eq!(
            row_count(&real),
            2,
            "the shared file must hold the artifact"
        );
        assert!(
            !stale_wal.exists(),
            "the sidecars must be cleared beside the REAL file"
        );
    }

    #[test]
    fn restore_refuses_a_corrupt_artifact_without_touching_the_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.db");
        drop(seeded(&target));
        let before = std::fs::read(&target).expect("read");

        let artifact = dir.path().join("junk.sqlite");
        std::fs::write(&artifact, b"not a sqlite database at all").expect("write");
        restore(&artifact, &format!("sqlite://{}", target.display()))
            .expect_err("a corrupt artifact must be refused");

        assert_eq!(
            std::fs::read(&target).expect("read"),
            before,
            "a refused restore must leave the target byte-identical"
        );
        assert!(
            !staging_path(&target).exists(),
            "no staging file may be left behind"
        );
    }
}
