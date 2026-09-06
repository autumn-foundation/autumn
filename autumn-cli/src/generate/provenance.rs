//! Provenance for generator-owned files (issue #1835).
//!
//! `autumn destroy` recomputes the plan a matching `autumn generate` would
//! build, then compares each owned file against that plan before deleting it.
//! The comparison answers "did the developer edit this file?" — but it asks
//! "does this file match what the CLI renders *today*?", and those are the same
//! question only while the template is unchanged. A newer CLI whose template
//! moved on therefore reports `Diverged` for every untouched file it wrote, and
//! `--force` — which also bypasses the genuine-edit guard — becomes routine.
//!
//! So `generate` records a digest of every file it owns as it writes it, in the
//! manifest at [`MANIFEST_PATH`], and `destroy` accepts a file that matches
//! either the current render or that recorded digest. A real edit matches
//! neither and is still refused.
//!
//! The same merge-base idea backs `autumn upgrade`'s scaffold manifest
//! (`.autumn/scaffold.toml`); this one covers `generate`'s output instead, and
//! is a separate file because the two are written by different commands with
//! different lifetimes.
//!
//! A project generated before this manifest existed has no baseline. That is
//! not an error: it falls back to comparing against the current render only —
//! exactly the previous behaviour, `--force` included.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Project-relative location of the generator provenance manifest.
///
/// Under `.autumn/` next to `scaffold.toml`: machine-written bookkeeping, kept
/// out of the one directory a developer reads. Meant to be committed — its
/// value is being the baseline a later checkout compares against.
pub const MANIFEST_PATH: &str = ".autumn/generated.toml";

/// Line endings normalised, so a CRLF checkout is not read as an edit.
///
/// `git config core.autocrlf true` rewrites every text file on checkout. Hashing
/// the bytes as they sit on disk would report the developer had rewritten every
/// generated file — on exactly the platform least able to diagnose it.
fn normalize(contents: &str) -> String {
    contents.replace("\r\n", "\n")
}

/// Digest of a text file, over the LF-normalised bytes.
#[must_use]
pub fn text_digest(contents: &str) -> String {
    digest(normalize(contents).as_bytes())
}

/// Digest of a binary file, over the bytes exactly as written.
///
/// No normalisation: a `CreateBytes` asset is opaque, and rewriting CRLF inside
/// a PNG would make two different files hash alike.
#[must_use]
pub fn bytes_digest(bytes: &[u8]) -> String {
    digest(bytes)
}

/// SHA-256, hex encoded. Not a commitment to anything — it only has to tell
/// "these are the bytes Autumn wrote" from "these are not", stably across hosts.
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// The on-disk shape of [`MANIFEST_PATH`].
#[derive(Default, Serialize, Deserialize)]
struct ManifestFile {
    /// Project-relative path → digest of the file as `generate` wrote it.
    #[serde(default)]
    files: BTreeMap<String, String>,
}

/// What `autumn generate` recorded about the files it owns.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Provenance {
    entries: BTreeMap<String, String>,
}

impl Provenance {
    /// Read the manifest under `root`. A missing, unreadable, or malformed
    /// manifest is an empty one: no baseline is the safe answer, never an error
    /// that would abort a generator run.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let text = std::fs::read_to_string(root.join(MANIFEST_PATH)).unwrap_or_default();
        let file: ManifestFile = toml::from_str(&text).unwrap_or_default();
        Self {
            entries: file.files,
        }
    }

    /// Whether a digest is recorded for this project-relative key.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Whether nothing is recorded — a project with no baseline.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `digest` is what `generate` recorded for `path`.
    ///
    /// False when nothing is recorded — an unproven file is never assumed ours.
    #[must_use]
    pub fn is_ours(&self, root: &Path, path: &Path, digest: &str) -> bool {
        key(root, path).is_some_and(|k| self.entries.get(&k).is_some_and(|d| d == digest))
    }

    /// Record `digest` as the content `generate` wrote to `path`.
    ///
    /// A path outside `root` cannot be keyed and is dropped: the manifest
    /// describes one project, and destroy would never look it up.
    pub fn record(&mut self, root: &Path, path: &Path, digest: String) {
        if let Some(k) = key(root, path) {
            self.entries.insert(k, digest);
        }
    }

    /// Drop the entry for `path`, if any.
    pub fn forget(&mut self, root: &Path, path: &Path) {
        if let Some(k) = key(root, path) {
            self.entries.remove(&k);
        }
    }

    /// Drop every entry for a file under `dir` — what removing a whole
    /// generated directory (a migration) leaves behind.
    pub fn forget_dir(&mut self, root: &Path, dir: &Path) {
        let Some(prefix) = key(root, dir) else { return };
        let prefix = format!("{prefix}/");
        self.entries.retain(|k, _| !k.starts_with(&prefix));
    }

    /// Write the manifest under `root`, removing it once it holds nothing.
    ///
    /// # Errors
    /// Filesystem failures, and a symlinked manifest path — writing through one
    /// could write outside the project.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = root.join(MANIFEST_PATH);
        if self.entries.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        refuse_symlink(root, &path)?;
        let body = toml::to_string(&ManifestFile {
            files: self.entries.clone(),
        })
        .map_err(std::io::Error::other)?;
        let directory = path
            .parent()
            .ok_or_else(|| std::io::Error::other("manifest path has no parent directory"))?;
        std::fs::create_dir_all(directory)?;
        write_atomically(directory, &path, &body)
    }
}

/// Publish `body` at `path` through a temp file in the same directory, so a
/// concurrent reader sees either the old manifest or the new one, never a
/// half-written file.
fn write_atomically(directory: &Path, path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    // Created through ordinary `0o666` open semantics so the process umask
    // applies, as it does to the `fs::write` the generator itself uses;
    // `tempfile`'s own constructor would create `0600`.
    let mut temp = tempfile::Builder::new()
        .prefix(".autumn-generated-")
        .suffix(".tmp")
        .make_in(directory, |p| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
        })?;
    temp.as_file_mut().write_all(body.as_bytes())?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Reject a manifest path reached through a symlink, at any component.
///
/// `symlink_metadata` does not follow, which is the point: a link's own
/// metadata is what says it is a link.
fn refuse_symlink(root: &Path, path: &Path) -> std::io::Result<()> {
    let mut cursor = root.to_path_buf();
    for component in MANIFEST_PATH.split('/') {
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            // Not there yet — creating it is exactly what a first save does.
            Err(_) => return Ok(()),
            Ok(m) if m.file_type().is_symlink() => {
                return Err(std::io::Error::other(format!(
                    "{} is a symlink; writing through it could write outside the project",
                    path.display()
                )));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// `path` as a project-relative, forward-slashed manifest key.
fn key(root: &Path, path: &Path) -> Option<String> {
    let relative: PathBuf = path.strip_prefix(root).ok()?.to_path_buf();
    let key = relative.to_str()?.replace('\\', "/");
    (!key.is_empty()).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn records_and_reloads_a_digest() {
        let dir = tmp();
        let file = dir.path().join("src/models/post.rs");
        let mut p = Provenance::default();
        p.record(dir.path(), &file, text_digest("// model\n"));
        p.save(dir.path()).unwrap();

        let reloaded = Provenance::load(dir.path());
        assert!(reloaded.is_ours(dir.path(), &file, &text_digest("// model\n")));
        assert!(!reloaded.is_ours(dir.path(), &file, &text_digest("// edited\n")));
    }

    #[test]
    fn crlf_hashes_the_same_as_lf() {
        assert_eq!(text_digest("a\r\nb\r\n"), text_digest("a\nb\n"));
    }

    #[test]
    fn bytes_are_hashed_verbatim() {
        assert_ne!(bytes_digest(b"a\r\nb"), bytes_digest(b"a\nb"));
    }

    #[test]
    fn an_unrecorded_file_is_never_ours() {
        let dir = tmp();
        let p = Provenance::default();
        assert!(!p.is_ours(dir.path(), &dir.path().join("x.rs"), &text_digest("")));
    }

    #[test]
    fn a_path_outside_the_project_is_dropped() {
        let dir = tmp();
        let mut p = Provenance::default();
        p.record(dir.path(), Path::new("/etc/passwd"), text_digest(""));
        assert_eq!(p, Provenance::default());
    }

    #[test]
    fn a_malformed_manifest_reads_as_no_baseline() {
        let dir = tmp();
        std::fs::create_dir_all(dir.path().join(".autumn")).unwrap();
        std::fs::write(dir.path().join(MANIFEST_PATH), "not : toml [[[").unwrap();
        assert_eq!(Provenance::load(dir.path()), Provenance::default());
    }

    #[test]
    fn saving_an_empty_manifest_removes_it() {
        let dir = tmp();
        let file = dir.path().join("a.rs");
        let mut p = Provenance::default();
        p.record(dir.path(), &file, text_digest("x"));
        p.save(dir.path()).unwrap();
        assert!(dir.path().join(MANIFEST_PATH).is_file());

        p.forget(dir.path(), &file);
        p.save(dir.path()).unwrap();
        assert!(!dir.path().join(MANIFEST_PATH).exists());
    }

    #[test]
    fn forget_dir_drops_only_that_directory() {
        let dir = tmp();
        let mut p = Provenance::default();
        p.record(
            dir.path(),
            &dir.path().join("migrations/a/up.sql"),
            "1".to_owned(),
        );
        p.record(
            dir.path(),
            &dir.path().join("migrations/ab/up.sql"),
            "2".to_owned(),
        );
        p.forget_dir(dir.path(), &dir.path().join("migrations/a"));

        assert!(!p.contains("migrations/a/up.sql"));
        assert!(p.contains("migrations/ab/up.sql"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_manifest_is_refused() {
        let dir = tmp();
        let outside = tmp();
        std::fs::create_dir_all(dir.path().join(".autumn")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("escaped.toml"),
            dir.path().join(MANIFEST_PATH),
        )
        .unwrap();

        let mut p = Provenance::default();
        p.record(dir.path(), &dir.path().join("a.rs"), text_digest("x"));

        assert!(p.save(dir.path()).is_err());
        assert!(!outside.path().join("escaped.toml").exists());
    }
}
