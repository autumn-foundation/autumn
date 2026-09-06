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
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::config::GENERATE_CONFIG_FILENAME;

/// Project-relative location of the generator provenance manifest.
///
/// Under `.autumn/` next to `scaffold.toml`: machine-written bookkeeping, kept
/// out of the one directory a developer reads. Meant to be committed — its
/// value is being the baseline a later checkout compares against.
pub const MANIFEST_PATH: &str = ".autumn/generated.toml";

/// Joins the parts of one argument list. No shell argument carries a unit
/// separator, so two lists cannot collide by concatenation.
const UNIT_SEPARATOR: char = '\u{1f}';

/// Joins the argument list to the config fingerprint, and one config source to
/// the next.
const RECORD_SEPARATOR: char = '\u{1e}';

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

/// SHA-256, hex encoded. Not used for security — it only has to tell "these are
/// the bytes Autumn wrote" from "these are not", stably across hosts.
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// One recorded file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    /// Digest of the file as it was written.
    digest: String,
    /// The command that wrote it, normalised by [`current_invocation`].
    ///
    /// `destroy` honours the digest only for the same command with the same
    /// arguments — the contract its help text already states. Without this the
    /// digest is keyed by path alone, so `autumn destroy model Post` with the
    /// fields omitted would delete the model file it can no longer render
    /// while the `schema.rs` and `Cargo.toml` reverts, which ARE derived from
    /// those fields, silently do nothing: a half-destroyed project where the
    /// pre-#1835 code refused outright. It also keeps one command from
    /// claiming another's output — `autumn new --starter` writes files through
    /// this same engine, and `autumn destroy auth` must not delete them.
    invocation: String,
}

/// The on-disk shape of [`MANIFEST_PATH`].
#[derive(Default, Serialize, Deserialize)]
struct ManifestFile {
    /// Project-relative path → what was written there, and by what.
    #[serde(default)]
    files: BTreeMap<String, Entry>,
}

/// What identifies the generator inputs of this run, for a project at `root`.
///
/// Two parts, because the arguments alone are not the whole input:
///
/// - This process's command line, reduced to what names the resource. Stable
///   across CLI versions, unlike anything derived from the argument structs:
///   it is the user's own words. The `generate`/`destroy` verb drops out so the
///   two spellings of one resource agree, and `--force`/`--dry-run` drop out
///   because they legitimately differ between the two runs.
/// - A digest of the generator config those arguments resolve from — the
///   auto-discovered [`GENERATE_CONFIG_FILENAME`], which does not appear in the
///   arguments at all, and any file a `--config` names. Without it, editing the
///   recipe between `generate` and a textually identical `destroy` would leave
///   the arguments looking unchanged while the plan is rebuilt from different
///   fields: `destroy` would accept the old owned files by digest and then
///   apply shared-file reverts the original generation never made.
///
/// Editing the config for any resource therefore drops the baseline for all of
/// them, back to comparing against the current render. Deliberate: a stale
/// baseline is worse than no baseline.
#[must_use]
pub fn current_invocation(root: &Path) -> String {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let inputs = normalize_invocation(args.iter().cloned());
    let config = config_fingerprint(root, &args);
    format!("{inputs}{RECORD_SEPARATOR}{config}")
}

/// An identity built from a generator's RESOLVED inputs instead of its raw
/// arguments.
///
/// For the generators that recover omitted arguments on the destroy path:
/// `autumn destroy webhook <provider> <Name>` reads a `--path`/`--secret-env`
/// used at generation time back out of `autumn.toml`, so the recomputed plan is
/// right while the arguments differ. Keyed on the raw arguments, the baseline
/// would then be refused for a file nobody has touched — the very case this
/// manifest exists to serve.
///
/// No config fingerprint: `parts` already carries every resolved input, so
/// there is nothing left for a config file to change.
#[must_use]
pub fn resolved_invocation(parts: &[&str]) -> String {
    parts.join(&UNIT_SEPARATOR.to_string())
}

/// A digest over every generator config file this run could have read.
fn config_fingerprint(root: &Path, args: &[String]) -> String {
    let mut sources = vec![root.join(GENERATE_CONFIG_FILENAME)];
    let mut next_is_path = false;
    for arg in args {
        if next_is_path {
            sources.push(PathBuf::from(arg));
            next_is_path = false;
        } else if let Some(path) = arg.strip_prefix("--config=") {
            sources.push(PathBuf::from(path));
        } else if arg == "--config" {
            next_is_path = true;
        }
    }
    sources.sort();
    sources.dedup();

    let mut material = String::new();
    for source in sources {
        let contents = std::fs::read(&source).map_or_else(|_| "absent".to_owned(), |b| digest(&b));
        material.push_str(&source.to_string_lossy());
        material.push(UNIT_SEPARATOR);
        material.push_str(&contents);
        material.push(RECORD_SEPARATOR);
    }
    digest(material.as_bytes())
}

fn normalize_invocation(args: impl Iterator<Item = String>) -> String {
    let mut verb_seen = false;
    let mut parts = Vec::new();
    for arg in args {
        if !verb_seen && (arg == "generate" || arg == "destroy") {
            verb_seen = true;
            continue;
        }
        if matches!(arg.as_str(), "--force" | "--dry-run") {
            continue;
        }
        parts.push(arg);
    }
    parts.join(&UNIT_SEPARATOR.to_string())
}

/// What `autumn generate` recorded about the files it owns.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Provenance {
    entries: BTreeMap<String, Entry>,
}

impl Provenance {
    /// Read the manifest under `root`. A missing, unreadable, or malformed
    /// manifest is an empty one: no baseline is the safe answer, never an error
    /// that would abort a generator run.
    ///
    /// A symlinked manifest reads as empty too. Read through one and whoever
    /// controls the target supplies the digests that decide what `destroy`
    /// deletes — from outside the repository, where no diff shows it.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        if refuse_symlink(root, &root.join(MANIFEST_PATH)).is_err() {
            return Self::default();
        }
        let text = std::fs::read_to_string(root.join(MANIFEST_PATH)).unwrap_or_default();
        let file: ManifestFile = toml::from_str(&text).unwrap_or_default();
        Self {
            entries: file.files,
        }
    }

    /// Whether an entry is recorded for this project-relative key.
    #[cfg(test)]
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Whether nothing is recorded — a project with no baseline.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `invocation` wrote exactly `digest` to `path`.
    ///
    /// False when nothing is recorded — an unproven file is never assumed ours
    /// — and false when a different command recorded it (see [`Entry`]).
    #[must_use]
    pub fn is_ours(&self, root: &Path, path: &Path, digest: &str, invocation: &str) -> bool {
        key(root, path).is_some_and(|k| {
            self.entries
                .get(&k)
                .is_some_and(|e| e.digest == digest && e.invocation == invocation)
        })
    }

    /// Whether ANY command wrote exactly `digest` to `path`.
    ///
    /// For a file several resources share: only the first writer is recorded,
    /// so requiring the destroying command to be that writer would strand the
    /// file the moment a template change stopped the content compare from
    /// matching. Sound only where the caller has already established that no
    /// sibling still needs the file — which is what the `CreateIfAbsent` pass
    /// does before it asks.
    #[must_use]
    pub fn was_written(&self, root: &Path, path: &Path, digest: &str) -> bool {
        key(root, path).is_some_and(|k| self.entries.get(&k).is_some_and(|e| e.digest == digest))
    }

    /// Record `digest` as the content `invocation` wrote to `path`.
    ///
    /// A path outside `root` cannot be keyed and is dropped: the manifest
    /// describes one project, and destroy would never look it up.
    pub fn record(&mut self, root: &Path, path: &Path, digest: String, invocation: &str) {
        if let Some(k) = key(root, path) {
            // Keyed by path, so the newest writer replaces the previous one:
            // a file has exactly one owner, and the manifest cannot grow an
            // entry per regeneration.
            self.entries.insert(
                k,
                Entry {
                    digest,
                    invocation: invocation.to_owned(),
                },
            );
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
    /// Filesystem failures, and a symlinked manifest path — writing or
    /// unlinking through one could reach outside the project.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = root.join(MANIFEST_PATH);
        // Before the removal below as well as the write: `unlink` resolves the
        // directories on the way to its target, so a symlinked `.autumn/` would
        // delete a file outside the project.
        refuse_symlink(root, &path)?;
        if self.entries.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
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
///
/// This makes each save atomic, not the read-modify-write around it: two
/// generators running in one project at once can still lose one run's entries.
/// Accepted — the loser's files fall back to comparing against the current
/// render, which is what `destroy` did before the manifest existed.
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            // Anything else is unknown, which is not the same as absent.
            Err(e) => return Err(e),
            Ok(m) if m.file_type().is_symlink() => {
                return Err(std::io::Error::other(format!(
                    "{} is a symlink; reading or writing through it could reach \
                     outside the project",
                    path.display()
                )));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// `path` as a project-relative, forward-slashed manifest key.
///
/// Built from ordinary components only. `strip_prefix` is lexical, so
/// `<root>/../secret` strips to `../secret` — a key naming a file outside the
/// project. Rejecting anything but [`Component::Normal`] keeps every key
/// project-relative, and joining the parts spells a key the same way on Windows
/// as on Unix without rewriting a separator a filename may legitimately hold.
fn key(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    const OWNER: &str = "model\u{1f}Post\u{1f}title:String";

    #[test]
    fn records_and_reloads_a_digest() {
        let dir = tmp();
        let file = dir.path().join("src/models/post.rs");
        let mut p = Provenance::default();
        p.record(dir.path(), &file, text_digest("// model\n"), OWNER);
        p.save(dir.path()).unwrap();

        let reloaded = Provenance::load(dir.path());
        assert!(reloaded.is_ours(dir.path(), &file, &text_digest("// model\n"), OWNER));
        assert!(!reloaded.is_ours(dir.path(), &file, &text_digest("// edited\n"), OWNER));
    }

    #[test]
    fn a_different_command_never_owns_a_recorded_file() {
        let dir = tmp();
        let file = dir.path().join("src/models/post.rs");
        let mut p = Provenance::default();
        p.record(dir.path(), &file, text_digest("// model\n"), OWNER);

        assert!(!p.is_ours(
            dir.path(),
            &file,
            &text_digest("// model\n"),
            "model\u{1f}Post"
        ));
    }

    #[test]
    fn the_invocation_drops_the_verb_and_the_run_only_flags() {
        let normalize = |args: &[&str]| normalize_invocation(args.iter().map(|a| (*a).to_owned()));
        assert_eq!(
            normalize(&["generate", "model", "Post", "title:String"]),
            normalize(&["destroy", "model", "Post", "title:String", "--force"]),
        );
        assert_eq!(
            normalize(&["generate", "model", "Post", "--dry-run"]),
            normalize(&["generate", "model", "Post"]),
        );
        assert_ne!(
            normalize(&["generate", "model", "Post", "title:String"]),
            normalize(&["destroy", "model", "Post"]),
            "omitted fields are different arguments"
        );
        assert_ne!(
            normalize(&["new", "--starter", "saas", "app"]),
            normalize(&["destroy", "auth", "User"]),
            "a starter scaffold never owns a generator's output"
        );
    }

    #[test]
    fn arguments_cannot_collide_by_concatenation() {
        let normalize = |args: &[&str]| normalize_invocation(args.iter().map(|a| (*a).to_owned()));
        assert_ne!(
            normalize(&["model", "Post Tag"]),
            normalize(&["model", "Post", "Tag"])
        );
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
        assert!(!p.is_ours(
            dir.path(),
            &dir.path().join("x.rs"),
            &text_digest(""),
            OWNER
        ));
    }

    #[test]
    fn a_path_outside_the_project_is_dropped() {
        let dir = tmp();
        let mut p = Provenance::default();
        p.record(dir.path(), Path::new("/etc/passwd"), text_digest(""), OWNER);
        assert_eq!(p, Provenance::default());
    }

    #[test]
    fn a_path_escaping_through_parent_segments_is_dropped() {
        let dir = tmp();
        let mut p = Provenance::default();
        p.record(
            dir.path(),
            &dir.path().join("../secret.toml"),
            text_digest(""),
            OWNER,
        );
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
        p.record(dir.path(), &file, text_digest("x"), OWNER);
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
            OWNER,
        );
        p.record(
            dir.path(),
            &dir.path().join("migrations/ab/up.sql"),
            "2".to_owned(),
            OWNER,
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
        p.record(
            dir.path(),
            &dir.path().join("a.rs"),
            text_digest("x"),
            OWNER,
        );

        assert!(p.save(dir.path()).is_err());
        assert!(!outside.path().join("escaped.toml").exists());
    }
}
