//! The orphan sweep of `autumn destroy` (issue #2557).
//!
//! `destroy` builds the plan again from the current templates. A file that an
//! older template wrote, and that the current templates renamed or dropped, is
//! in no action. The manifest still names it. This module finds such entries
//! and decides, for each one, to delete it, to keep it with a warning, or to
//! drop the stale entry.
//!
//! The paths come from the manifest, which is project content. Every check
//! here treats them as untrusted. See `docs/adr/0021-destroy-orphan-sweep.md`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::emit::{Action, split_migration_dir_name};
use super::provenance::{self, MANIFEST_PATH, Provenance, Recorded, Scope};

/// Files the sweep never deletes, lowercase. Generators edit them in place
/// and never own them, so an `owned` entry for one is not to be trusted.
const PROTECTED_FILES: &[&str] = &[
    "autumn.toml",
    "build.rs",
    "cargo.lock",
    "cargo.toml",
    "src/lib.rs",
    "src/main.rs",
];

/// Top-level directories the sweep never enters, lowercase.
const PROTECTED_DIRS: &[&str] = &[".autumn", ".git", "target"];

/// What `revert` does with the swept entries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Sweep {
    /// Untouched owned files to delete.
    pub remove: Vec<PathBuf>,
    /// Entries whose file is gone. Drop them from the manifest.
    pub forget: Vec<PathBuf>,
    /// Files kept, and why.
    pub warnings: Vec<String>,
}

/// Find the entries `invocation` wrote that no action in `actions` names.
///
/// Delete an `owned` file only when its digest still matches. Keep every
/// other file, with a warning that names it. `--force` has no effect here:
/// these paths come from the manifest, not from the plan.
pub(super) fn plan_sweep(
    project_root: &Path,
    recorded: &Provenance,
    invocation: &str,
    actions: &[Action],
) -> Sweep {
    let named = named_keys(project_root, actions);
    let suffixes = planned_migration_suffixes(&named);
    let mut sweep = Sweep::default();
    for entry in recorded.recorded_by(invocation) {
        sweep_one(project_root, entry, &named, &suffixes, &mut sweep);
    }
    sweep
}

/// Decide what to do with one entry.
fn sweep_one(
    project_root: &Path,
    entry: Recorded<'_>,
    named: &HashSet<String>,
    suffixes: &HashSet<String>,
    sweep: &mut Sweep,
) {
    let Some(parts) = plain_parts(entry.key) else {
        sweep.warnings.push(format!(
            "{:?} in {MANIFEST_PATH} is not a plain project path; destroy ignores it.",
            entry.key
        ));
        return;
    };
    let lower = entry.key.to_ascii_lowercase();
    // On a case-insensitive file system the two spellings are one file.
    if named.contains(&lower) {
        return;
    }
    let shown = entry.key;
    if lower.starts_with("migrations/") {
        // The plan matches migrations by suffix, not by timestamp. Only the
        // applied-migration guard of that pass may remove one.
        let planned = parts.len() == 3 && suffixes.contains(&migration_suffix(parts[1]));
        if !planned {
            sweep.warnings.push(format!(
                "{shown}: the current templates do not make this migration file. \
                 destroy never sweeps migrations. Remove it by hand if no database uses it."
            ));
        }
        return;
    }
    if PROTECTED_FILES.contains(&lower.as_str())
        || PROTECTED_DIRS.iter().any(|dir| {
            lower
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    {
        sweep
            .warnings
            .push(format!("{shown}: destroy never sweeps this file."));
        return;
    }
    let path = parts
        .iter()
        .fold(project_root.to_path_buf(), |p, part| p.join(part));
    match inspect(project_root, &parts) {
        Inspect::Missing => {
            sweep.forget.push(path);
            return;
        }
        Inspect::Refused(reason) => {
            sweep.warnings.push(format!(
                "{shown}: destroy does not sweep this file, because {reason}."
            ));
            return;
        }
        Inspect::File => {}
    }
    match entry.scope {
        None => sweep.warnings.push(format!(
            "{shown}: an older autumn recorded this file for this command, and the \
             current templates do not make it. destroy cannot prove that only this \
             resource uses it, so it keeps the file. Delete it by hand if nothing uses it."
        )),
        Some(Scope::Shared) => sweep.warnings.push(format!(
            "{shown}: this command made this shared file, and the current templates do \
             not make it. Other resources can use it, so destroy keeps it. Delete it by \
             hand if nothing uses it."
        )),
        Some(Scope::Owned) if untouched(&path, entry.digest) => sweep.remove.push(path),
        Some(Scope::Owned) => sweep.warnings.push(format!(
            "{shown}: the current templates do not make this file, and it changed after \
             generate wrote it. destroy keeps it. Delete it by hand if you do not need it."
        )),
    }
}

/// The keys of every path the plan names, lowercase.
fn named_keys(project_root: &Path, actions: &[Action]) -> HashSet<String> {
    actions
        .iter()
        .filter_map(|a| provenance::key(project_root, a.path()))
        .map(|k| k.to_ascii_lowercase())
        .collect()
}

/// The suffix of each migration directory the plan names.
fn planned_migration_suffixes(named: &HashSet<String>) -> HashSet<String> {
    named
        .iter()
        .filter_map(|key| {
            let mut parts = key.split('/');
            (parts.next() == Some("migrations"))
                .then(|| parts.next())
                .flatten()
                .filter(|_| parts.next().is_some())
                .map(migration_suffix)
        })
        .collect()
}

/// The suffix of a migration directory name, lowercase.
fn migration_suffix(dir: &str) -> String {
    split_migration_dir_name(dir).1.to_ascii_lowercase()
}

/// The segments of `key`, when every one is a plain file name.
///
/// Refuses an empty segment, `.`, `..`, a backslash, a colon (a Windows drive
/// or stream), and a NUL. A key that passes cannot leave the project.
fn plain_parts(key: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = key.split('/').collect();
    let plain = |part: &&str| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && !part.contains(['\\', ':', '\0'])
            && matches!(
                Path::new(part).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            )
    };
    parts.iter().all(plain).then_some(parts)
}

/// What is on disk at an entry's path.
enum Inspect {
    /// A regular file, reached through no symlink.
    File,
    /// Nothing.
    Missing,
    /// Something the sweep must not touch.
    Refused(String),
}

/// Walk to the entry one segment at a time, without following a symlink.
///
/// A symlinked directory on the way could point outside the project, and a
/// symlinked file is not the file `generate` wrote.
fn inspect(project_root: &Path, parts: &[&str]) -> Inspect {
    let mut cursor = project_root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        cursor.push(part);
        let last = i + 1 == parts.len();
        match fs::symlink_metadata(&cursor) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Inspect::Missing,
            Err(e) => return Inspect::Refused(format!("it cannot be read ({e})")),
            Ok(m) if m.file_type().is_symlink() => {
                return Inspect::Refused("it is reached through a symlink".to_owned());
            }
            Ok(m) if last && !m.is_file() => {
                return Inspect::Refused("it is not a regular file".to_owned());
            }
            Ok(_) => {}
        }
    }
    Inspect::File
}

/// Whether the file still holds what `generate` wrote.
///
/// The entry does not say text or bytes, so both digests are tried. The text
/// digest ignores line endings, so a CRLF checkout is not an edit.
fn untouched(path: &Path, digest: &str) -> bool {
    fs::read(path).is_ok_and(|bytes| {
        provenance::bytes_digest(&bytes) == digest
            || std::str::from_utf8(&bytes).is_ok_and(|text| provenance::text_digest(text) == digest)
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::generate::provenance::{Scope, text_digest};

    const ME: &str = "job\u{1f}Foo";

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    /// Write `contents` at `rel` and record it for `invocation`.
    fn generated(
        root: &Path,
        p: &mut Provenance,
        rel: &str,
        contents: &str,
        invocation: &str,
        scope: Option<Scope>,
    ) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        p.record(root, &path, text_digest(contents), invocation, scope);
        path
    }

    fn create(root: &Path, rel: &str) -> Action {
        Action::Create {
            path: root.join(rel),
            contents: String::new(),
        }
    }

    #[test]
    fn an_untouched_renamed_file_is_removed() {
        // The `old.rs` -> `new.rs` case from the issue.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(
            root,
            &mut p,
            "src/old.rs",
            "// v1\n",
            ME,
            Some(Scope::Owned),
        );

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/new.rs")]);

        assert_eq!(sweep.remove, vec![old]);
        assert!(sweep.warnings.is_empty(), "{:?}", sweep.warnings);
    }

    #[test]
    fn a_crlf_checkout_of_an_orphan_is_still_untouched() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(root, &mut p, "src/old.rs", "a\nb\n", ME, Some(Scope::Owned));
        fs::write(&old, "a\r\nb\r\n").unwrap();

        assert_eq!(plan_sweep(root, &p, ME, &[]).remove, vec![old]);
    }

    #[test]
    fn an_orphan_written_as_bytes_is_removed() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let bytes = [0x89_u8, b'P', b'N', b'G', 0xff, 0x00];
        let old = root.join("static/old.png");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, bytes).unwrap();
        p.record(
            root,
            &old,
            crate::generate::provenance::bytes_digest(&bytes),
            ME,
            Some(Scope::Owned),
        );

        assert_eq!(plan_sweep(root, &p, ME, &[]).remove, vec![old]);
    }

    #[test]
    fn an_edited_orphan_is_kept_with_a_warning() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(
            root,
            &mut p,
            "src/old.rs",
            "// v1\n",
            ME,
            Some(Scope::Owned),
        );
        fs::write(&old, "// mine\n").unwrap();

        let sweep = plan_sweep(root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1);
        assert!(
            sweep.warnings[0].contains("src/old.rs"),
            "{:?}",
            sweep.warnings
        );
    }

    #[test]
    fn a_shared_orphan_is_kept_with_a_warning() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(
            root,
            &mut p,
            "templates/mailers/_layout.html",
            "<html>\n",
            ME,
            Some(Scope::Shared),
        );

        let sweep = plan_sweep(root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1);
        assert!(sweep.warnings[0].contains("_layout.html"));
    }

    #[test]
    fn a_legacy_orphan_is_kept_with_a_warning() {
        // A 0.8.0 entry has no scope, so owned and shared look the same.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(root, &mut p, "src/old.rs", "// v1\n", ME, None);

        let sweep = plan_sweep(root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1);
        assert!(sweep.warnings[0].contains("src/old.rs"));
    }

    #[test]
    fn another_commands_entry_is_ignored() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(
            root,
            &mut p,
            "src/other.rs",
            "x",
            "job\u{1f}Bar",
            Some(Scope::Owned),
        );

        assert_eq!(plan_sweep(root, &p, ME, &[]), Sweep::default());
    }

    #[test]
    fn a_path_the_plan_names_is_left_to_the_plan() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(root, &mut p, "src/a.rs", "x", ME, Some(Scope::Owned));
        generated(root, &mut p, "src/b.rs", "y", ME, Some(Scope::Owned));
        let actions = [
            create(root, "src/a.rs"),
            Action::Modify {
                path: root.join("src/b.rs"),
                contents: String::new(),
            },
        ];

        assert_eq!(plan_sweep(root, &p, ME, &actions), Sweep::default());
    }

    #[test]
    fn the_plan_check_ignores_ascii_case() {
        // On a case-insensitive file system both spellings are one file.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(
            root,
            &mut p,
            "src/Models/Post.rs",
            "x",
            ME,
            Some(Scope::Owned),
        );

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/models/post.rs")]);

        assert_eq!(sweep, Sweep::default());
    }

    #[test]
    fn a_missing_orphan_drops_its_entry() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(root, &mut p, "src/old.rs", "x", ME, Some(Scope::Owned));
        fs::remove_file(&old).unwrap();

        let sweep = plan_sweep(root, &p, ME, &[]);

        assert_eq!(sweep.forget, vec![old]);
        assert!(sweep.remove.is_empty());
        assert!(sweep.warnings.is_empty());
    }

    #[test]
    fn a_migration_the_plan_matches_by_suffix_is_left_to_the_plan() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(
            root,
            &mut p,
            "migrations/20260101000000_create_posts/up.sql",
            "x",
            ME,
            Some(Scope::Owned),
        );
        let actions = [create(
            root,
            "migrations/20261021000000_create_posts/up.sql",
        )];

        assert_eq!(plan_sweep(root, &p, ME, &actions), Sweep::default());
    }

    #[test]
    fn an_orphan_migration_is_never_swept() {
        // Only the applied-migration guard may remove a migration.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(
            root,
            &mut p,
            "migrations/20260101000000_create_posts/up.sql",
            "x",
            ME,
            Some(Scope::Owned),
        );

        let sweep = plan_sweep(root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1);
        assert!(
            sweep.warnings[0].contains("migration"),
            "{:?}",
            sweep.warnings
        );
    }

    #[test]
    fn a_protected_project_file_is_never_swept() {
        let dir = tmp();
        let root = dir.path();
        for rel in [
            "src/main.rs",
            "Cargo.toml",
            "src/lib.rs",
            "autumn.toml",
            ".git/config",
        ] {
            let mut p = Provenance::default();
            let path = generated(root, &mut p, rel, "x", ME, Some(Scope::Owned));

            let sweep = plan_sweep(root, &p, ME, &[]);

            assert!(sweep.remove.is_empty(), "{rel} must not be swept");
            assert_eq!(sweep.warnings.len(), 1, "{rel}: {:?}", sweep.warnings);
            assert!(path.exists());
        }
    }

    /// Write a manifest whose only entry is `key`, with the digest of `x`.
    fn crafted(root: &Path, key: &str) -> Provenance {
        fs::create_dir_all(root.join(".autumn")).unwrap();
        let mut doc = toml::Table::new();
        let mut entry = toml::Table::new();
        entry.insert("digest".into(), text_digest("x").into());
        entry.insert("invocation".into(), ME.into());
        entry.insert("scope".into(), "owned".into());
        let mut files = toml::Table::new();
        files.insert(key.into(), entry.into());
        doc.insert("files".into(), files.into());
        fs::write(
            root.join(crate::generate::provenance::MANIFEST_PATH),
            toml::to_string(&doc).unwrap(),
        )
        .unwrap();
        Provenance::load(root)
    }

    #[test]
    fn a_crafted_key_never_reaches_outside_the_project() {
        let outer = tmp();
        let root = outer.path().join("app");
        fs::create_dir_all(&root).unwrap();
        let victim = outer.path().join("victim.rs");
        fs::write(&victim, "x").unwrap();
        let victim_key = victim.to_string_lossy().replace('\\', "/");

        for key in [
            "../victim.rs",
            "src/../../victim.rs",
            "./src/../../victim.rs",
            victim_key.as_str(),
            "C:/victim.rs",
            "src\\..\\..\\victim.rs",
            "",
            "src//a.rs",
        ] {
            let p = crafted(&root, key);
            let sweep = plan_sweep(&root, &p, ME, &[]);
            assert!(sweep.remove.is_empty(), "{key:?} must be refused");
            assert!(sweep.forget.is_empty(), "{key:?} must not be forgotten");
        }
        assert!(victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_orphan_behind_a_symlinked_directory_is_refused() {
        let outer = tmp();
        let root = outer.path().join("app");
        let elsewhere = outer.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("victim.rs"), "x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("src")).unwrap();

        let p = crafted(&root, "src/victim.rs");
        let sweep = plan_sweep(&root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
        assert!(elsewhere.join("victim.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_orphan_is_refused() {
        let outer = tmp();
        let root = outer.path().join("app");
        fs::create_dir_all(root.join("src")).unwrap();
        let victim = outer.path().join("victim.rs");
        fs::write(&victim, "x").unwrap();
        std::os::unix::fs::symlink(&victim, root.join("src/old.rs")).unwrap();

        let p = crafted(&root, "src/old.rs");
        let sweep = plan_sweep(&root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
    }

    #[test]
    fn a_directory_at_the_orphan_path_is_kept() {
        let dir = tmp();
        let root = dir.path();
        fs::create_dir_all(root.join("src/old.rs")).unwrap();
        let p = crafted(root, "src/old.rs");

        let sweep = plan_sweep(root, &p, ME, &[]);

        assert!(sweep.remove.is_empty());
        assert!(sweep.forget.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
    }
}
