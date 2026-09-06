//! Shared owner-only atomic-write helper (issue #1864).
//!
//! Stages `data` to a fresh, unpredictable sibling temp file, then `rename`s
//! it over the target, so a reader never observes a partial write and the
//! bytes are never briefly group- or world-readable. `rename` is atomic
//! within a directory, so a crash mid-write can never leave a torn file: the
//! target is either the old contents or the complete new contents.
//!
//! Used for ACME account/certificate material ([`crate::acme::store`]) and
//! failure capsules ([`crate::capsule::persist`]) — both hold secrets, or
//! must never be read half-written, and both used to carry their own copy of
//! this idiom.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Create `dir` (and parents), tightened to owner-only (`0700`) on Unix.
///
/// `create_dir_all` applies the process umask, so a permissive umask would
/// otherwise leave the directory group/world-readable; the explicit
/// `set_permissions` re-asserts owner-only regardless of umask. Best-effort:
/// filesystems that reject `chmod` (e.g. some network mounts) still get a
/// created directory, just not the tightened mode.
pub fn ensure_owner_only_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write `data` to a fresh, unpredictable sibling of `path` with owner-only
/// (`0600`) permissions on Unix, WITHOUT renaming it into place, and return
/// the temp path.
///
/// The temp file is opened `create_new` under a random name, so the write
/// can never follow a symlink an attacker planted at a predictable path, and
/// never truncates a file it did not create. Splitting staging from
/// [`publish_staged`] lets a multi-file publish (e.g. a cert chain + its
/// key) stage every file before renaming any of them, shrinking the window
/// in which a crash could tear a multi-file pair down to the back-to-back
/// rename syscalls.
pub fn stage_owner_only(path: &Path, data: &[u8]) -> std::io::Result<PathBuf> {
    let tmp = temp_sibling(path);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Belt-and-suspenders: `OpenOptions::mode` is also umask-masked, so
        // re-assert 0600 explicitly after creation.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.sync_all()?;
    Ok(tmp)
}

/// Atomically publish a staged temp file by renaming it over `path`. On
/// error, best-effort clean up the temp file so it does not accumulate.
pub fn publish_staged(tmp: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(tmp);
            Err(error)
        }
    }
}

/// Stage then publish in one call — the common case for a single-file
/// owner-only atomic write.
pub fn write_owner_only(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = stage_owner_only(path, data)?;
    publish_staged(&tmp, path)
}

/// An unpredictable sibling path for the temp file.
///
/// A fixed `<path>.tmp` name is guessable, and `create_new` on a guessable
/// path fails outright once something already sits there — a denial of
/// capture, and on a shared directory a way to point the write somewhere
/// else. The suffix comes from the same entropy the framework uses
/// elsewhere.
fn temp_sibling(path: &Path) -> PathBuf {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{nonce}.tmp"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_sibling_is_unpredictable_and_ends_in_tmp() {
        let path = Path::new("tmp/capsules/20250101T000000-000000-req.json");
        let first = temp_sibling(path);
        let second = temp_sibling(path);
        assert_ne!(
            first, second,
            "a predictable temp path can be pre-created or symlinked by anyone \
             who can write the directory"
        );
        for candidate in [&first, &second] {
            assert!(
                candidate.to_string_lossy().ends_with(".tmp"),
                "the temp file must not look like the real file to a directory scan: {candidate:?}"
            );
            assert_eq!(candidate.parent(), path.parent());
        }
    }

    #[test]
    fn write_owner_only_creates_the_file_with_the_expected_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        write_owner_only(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn write_owner_only_overwrites_an_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        std::fs::write(&path, b"old").unwrap();
        write_owner_only(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn write_owner_only_leaves_no_leftover_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        write_owner_only(&path, b"hello").unwrap();
        let mut entries = std::fs::read_dir(dir.path()).unwrap();
        let names: Vec<_> = std::iter::from_fn(|| entries.next())
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "no staged temp file should linger: {names:?}"
        );
    }

    #[test]
    fn stage_then_publish_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cert.chain.pem");
        let tmp = stage_owner_only(&path, b"CHAIN").unwrap();
        assert!(tmp.exists());
        assert!(!path.exists(), "not published yet");
        publish_staged(&tmp, &path).unwrap();
        assert!(path.exists());
        assert!(!tmp.exists(), "temp file consumed by rename");
        assert_eq!(std::fs::read(&path).unwrap(), b"CHAIN");
    }

    #[test]
    fn publish_staged_cleans_up_the_temp_file_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cert.chain.pem");
        let tmp = stage_owner_only(&path, b"CHAIN").unwrap();
        // A directory as the destination makes `rename` fail.
        std::fs::create_dir(&path).unwrap();
        let target = path.join("nested").join("impossible");
        let err = publish_staged(&tmp, &target);
        assert!(err.is_err());
        assert!(!tmp.exists(), "temp file must be cleaned up on failure");
    }

    #[test]
    fn ensure_owner_only_dir_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        ensure_owner_only_dir(&nested).unwrap();
        assert!(nested.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn written_files_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("secrets");
        ensure_owner_only_dir(&dir).unwrap();
        let path = dir.join("account.json");
        write_owner_only(&path, b"secret").unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file should be 0600, was {file_mode:o}");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "directory should be 0700, was {dir_mode:o}"
        );
    }
}
