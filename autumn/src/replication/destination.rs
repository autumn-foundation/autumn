//! Where replicated bytes go: the object-store seam the replicator, the restore
//! planner and the periodic verifier all share (issue #1628).
//!
//! The trait is deliberately **blocking**. Every caller already runs off the
//! async runtime — the replicator does its file and `SQLite` work inside
//! `spawn_blocking`, and `autumn db replica restore` is a synchronous CLI with no
//! reactor at all — so a blocking seam is the one shape both can use without
//! either dragging a runtime into the other.
//!
//! Two implementations ship:
//!
//! * [`FileDestination`] — a directory. Not a test double: a second disk, an NFS
//!   or SSHFS mount, or a bind-mounted volume is a legitimate offsite target, and
//!   it is what makes the end-to-end proof of the whole loop run in the ordinary
//!   `cargo test` lane with no container and no network.
//! * [`super::s3::S3Destination`] — any S3-compatible endpoint, under the
//!   `http-client` feature.

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::path::{Path, PathBuf};

/// Failure modes of a replica destination.
///
/// Deliberately credential-free: a variant carries an operation name, an HTTP
/// status, a provider error code and a key — never a URL with credentials in it,
/// never a header, never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DestinationError {
    /// The object does not exist.
    NotFound {
        /// The key that was requested.
        key: String,
    },
    /// Local I/O against the destination (or a staging file) failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// I/O detail.
        detail: String,
    },
    /// The remote endpoint returned a non-success status.
    Remote {
        /// What was being attempted.
        op: &'static str,
        /// HTTP status code.
        status: u16,
        /// Provider error code, when the response body carried one.
        code: Option<String>,
    },
    /// The destination itself is misconfigured or refused the request before it
    /// was sent (bad endpoint, missing credential, unusable key).
    Rejected {
        /// Why.
        detail: String,
    },
}

impl DestinationError {
    /// Build an [`Io`](Self::Io) mapper for `?` on a `std::io::Result`.
    pub(crate) fn io(op: &'static str) -> impl Fn(std::io::Error) -> Self {
        move |e| Self::Io {
            op,
            detail: e.to_string(),
        }
    }

    /// Whether this failure means "the object is simply not there", which the
    /// restore planner treats differently from a transport failure.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

impl fmt::Display for DestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { key } => write!(f, "replica object {key:?} does not exist"),
            Self::Io { op, detail } => write!(f, "replica {op} failed: {detail}"),
            Self::Remote { op, status, code } => match code {
                Some(code) => write!(f, "replica {op} failed: HTTP {status} ({code})"),
                None => write!(f, "replica {op} failed: HTTP {status}"),
            },
            Self::Rejected { detail } => write!(f, "replica destination rejected: {detail}"),
        }
    }
}

impl std::error::Error for DestinationError {}

/// A flat key/value object store the replicator ships to and restores from.
///
/// Keys are `/`-separated, never absolute, and never contain `.` or `..`
/// segments — implementations must refuse anything else rather than escaping
/// their namespace.
pub trait ReplicaDestination: Send + Sync {
    /// A credential-free description for logs, health details and errors.
    fn describe(&self) -> String;

    /// Store `body` at `key`, replacing any existing object.
    ///
    /// # Errors
    ///
    /// See [`DestinationError`].
    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError>;

    /// Store the contents of `path` at `key`, streaming rather than buffering.
    ///
    /// # Errors
    ///
    /// See [`DestinationError`].
    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError>;

    /// Fetch `key` in full.
    ///
    /// # Errors
    ///
    /// See [`DestinationError`]; a missing object is
    /// [`DestinationError::NotFound`].
    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError>;

    /// Stream `key` into `path`, creating or truncating it.
    ///
    /// # Errors
    ///
    /// See [`DestinationError`].
    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError>;

    /// Every key under `prefix`, sorted ascending.
    ///
    /// # Errors
    ///
    /// See [`DestinationError`].
    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError>;

    /// Remove `key`. Removing a key that does not exist is not an error.
    ///
    /// # Errors
    ///
    /// See [`DestinationError`].
    fn delete(&self, key: &str) -> Result<(), DestinationError>;
}

/// Reject a key that could escape the destination's namespace.
///
/// # Errors
///
/// Returns [`DestinationError::Rejected`] for an empty key, an absolute key, a
/// key with a `.`/`..`/empty segment, or one containing a backslash or NUL.
pub fn validate_key(key: &str) -> Result<(), DestinationError> {
    let reject = |detail: String| DestinationError::Rejected { detail };
    if key.is_empty() {
        return Err(reject("object key is empty".to_owned()));
    }
    if key.starts_with('/') {
        return Err(reject(format!("object key {key:?} must be relative")));
    }
    // `\` and `:` are refused because a destination may be a directory: on
    // Windows both are path syntax, and `PathBuf::push` of a segment carrying a
    // drive prefix REPLACES the path rather than appending to it. Autumn never
    // generates a key containing either.
    if key.contains('\\') || key.contains('\0') || key.contains(':') {
        return Err(reject(format!(
            "object key {key:?} contains an illegal character"
        )));
    }
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(reject(format!(
                "object key {key:?} has an illegal path segment {segment:?}"
            )));
        }
    }
    Ok(())
}

/// How deep the filesystem destination will walk when listing. Autumn's own
/// layout is four levels; anything deeper is either a mistake or a planted tree.
const MAX_LIST_DEPTH: usize = 16;

/// A destination backed by a local directory.
///
/// Writes are staged to a sibling temporary file and `rename`d into place, so a
/// crash mid-write leaves the previous object intact and never a half-written
/// one — the same all-or-nothing guarantee an object-store PUT gives.
#[derive(Debug, Clone)]
pub struct FileDestination {
    root: PathBuf,
}

impl FileDestination {
    /// Target `root`, creating it if needed.
    ///
    /// # Errors
    ///
    /// Returns [`DestinationError::Io`] when `root` cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, DestinationError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(DestinationError::io("create destination root"))?;
        Ok(Self { root })
    }

    /// The directory this destination writes under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &str) -> Result<PathBuf, DestinationError> {
        validate_key(key)?;
        let mut path = self.root.clone();
        for segment in key.split('/') {
            path.push(segment);
        }
        Ok(path)
    }

    /// Write `write_body` to `key` through a temp file + rename.
    fn atomic_write(
        &self,
        key: &str,
        write_body: impl FnOnce(&mut std::fs::File) -> Result<(), DestinationError>,
    ) -> Result<(), DestinationError> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(DestinationError::io("create key directory"))?;
        }
        // Unpredictable, and created with O_EXCL. A destination directory can be
        // shared (the module docs endorse an NFS or bind mount), and a staging
        // name an attacker can predict is a staging name they can pre-create as a
        // symlink — `File::create` would then write the object's bytes through it
        // with this process's privileges.
        let mut staging = path.clone();
        let mut name = staging.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".tmp-{:016x}", rand::random::<u64>()));
        staging.set_file_name(name);

        {
            let mut file = std::fs::File::create_new(&staging)
                .map_err(DestinationError::io("create object"))?;
            write_body(&mut file)?;
            file.sync_all()
                .map_err(DestinationError::io("fsync object"))?;
        }
        std::fs::rename(&staging, &path).map_err(DestinationError::io("publish object"))?;
        // Durability of the rename itself, which matters on the removable /
        // network destinations this is meant for.
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Walk `dir`, pushing every file's key (relative to the root) into `out`.
    ///
    /// Symlinks are skipped rather than listed: a symlink planted in a shared
    /// destination would otherwise become an "object" whose `get` reads whatever
    /// it points at. Recursion is depth-bounded for the same reason — a deep tree
    /// planted there must not overflow the stack.
    fn collect(
        &self,
        dir: &Path,
        depth: usize,
        out: &mut Vec<String>,
    ) -> Result<(), DestinationError> {
        if depth > MAX_LIST_DEPTH {
            return Err(DestinationError::Rejected {
                detail: format!(
                    "the destination directory nests deeper than {MAX_LIST_DEPTH} levels"
                ),
            });
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(DestinationError::io("list objects")(e)),
        };
        for entry in entries {
            let entry = entry.map_err(DestinationError::io("list objects"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(DestinationError::io("list objects"))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                self.collect(&path, depth.saturating_add(1), out)?;
            } else if let Ok(rel) = path.strip_prefix(&self.root) {
                let key = rel
                    .components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .collect::<Vec<_>>()
                    .join("/");
                // Staging files are not objects.
                if !key.contains(".tmp-") {
                    out.push(key);
                }
            }
        }
        Ok(())
    }
}

impl ReplicaDestination for FileDestination {
    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }

    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        self.atomic_write(key, |file| {
            use std::io::Write as _;
            file.write_all(body)
                .map_err(DestinationError::io("write object"))
        })
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.atomic_write(key, |file| {
            let mut source =
                std::fs::File::open(path).map_err(DestinationError::io("open upload source"))?;
            std::io::copy(&mut source, file)
                .map(|_| ())
                .map_err(DestinationError::io("write object"))
        })
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        let path = self.path_for(key)?;
        std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DestinationError::NotFound {
                    key: key.to_owned(),
                }
            } else {
                DestinationError::Io {
                    op: "read object",
                    detail: e.to_string(),
                }
            }
        })
    }

    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        let source_path = self.path_for(key)?;
        let mut source = std::fs::File::open(&source_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DestinationError::NotFound {
                    key: key.to_owned(),
                }
            } else {
                DestinationError::Io {
                    op: "read object",
                    detail: e.to_string(),
                }
            }
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(DestinationError::io("create download directory"))?;
        }
        let mut target =
            std::fs::File::create(path).map_err(DestinationError::io("create download"))?;
        std::io::copy(&mut source, &mut target)
            .map(|_| ())
            .map_err(DestinationError::io("download object"))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        let mut keys = Vec::new();
        self.collect(&self.root.clone(), 0, &mut keys)?;
        keys.retain(|k| k.starts_with(prefix));
        keys.sort();
        Ok(keys)
    }

    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        let path = self.path_for(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DestinationError::Io {
                op: "delete object",
                detail: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_key_refuses_escapes() {
        assert!(validate_key("a/b/c.seg").is_ok());
        for bad in [
            "", "/abs", "a//b", "a/./b", "a/../b", "..", "a\\b", "a\0b", "C:/x",
        ] {
            assert!(validate_key(bad).is_err(), "key {bad:?} must be refused");
        }
    }

    #[test]
    fn file_destination_round_trips_and_lists_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = FileDestination::new(dir.path()).expect("dest");
        assert!(dest.describe().starts_with("file://"));

        dest.put("prod/generations/g1/segments/0000000001-1.seg", b"one")
            .expect("put");
        dest.put("prod/generations/g1/segments/0000000000-0.seg", b"zero")
            .expect("put");
        dest.put("prod/generations/g0/snapshot.json", b"{}")
            .expect("put");

        assert_eq!(
            dest.get("prod/generations/g1/segments/0000000000-0.seg")
                .expect("get"),
            b"zero"
        );
        assert_eq!(
            dest.list("prod/generations/g1/segments/").expect("list"),
            vec![
                "prod/generations/g1/segments/0000000000-0.seg".to_owned(),
                "prod/generations/g1/segments/0000000001-1.seg".to_owned(),
            ]
        );
        assert_eq!(dest.list("prod/generations/").expect("list").len(), 3);
    }

    #[test]
    fn file_destination_reports_a_missing_object_as_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = FileDestination::new(dir.path()).expect("dest");
        let err = dest.get("prod/missing").expect_err("must fail");
        assert!(err.is_not_found(), "unexpected error: {err}");
        assert!(dest.delete("prod/missing").is_ok(), "delete is idempotent");
    }

    #[test]
    fn file_destination_streams_files_both_ways() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = FileDestination::new(dir.path().join("dest")).expect("dest");
        let source = dir.path().join("snapshot.db");
        std::fs::write(&source, vec![7u8; 4096]).expect("write source");

        dest.put_file("prod/generations/g/snapshot.db.gz", &source)
            .expect("put_file");
        let back = dir.path().join("restored/snapshot.db");
        dest.get_to_file("prod/generations/g/snapshot.db.gz", &back)
            .expect("get_to_file");
        assert_eq!(std::fs::read(&back).expect("read"), vec![7u8; 4096]);
    }

    #[test]
    fn file_destination_refuses_a_traversing_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = FileDestination::new(dir.path()).expect("dest");
        assert!(matches!(
            dest.put("../escape", b"x"),
            Err(DestinationError::Rejected { .. })
        ));
    }
}
