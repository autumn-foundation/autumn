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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::emit::{Action, split_migration_dir_name};
use super::provenance::{self, MANIFEST_PATH, Provenance, Recorded, Scope};

/// Files the sweep never deletes, lowercase. Generators edit them in place
/// and never own them, so the sweep does not trust an `owned` entry for one.
const PROTECTED_FILES: &[&str] = &[
    "autumn.toml",
    "build.rs",
    "cargo.lock",
    "cargo.toml",
    "src/lib.rs",
    "src/main.rs",
];

/// What `revert` does with the swept entries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Sweep {
    /// Untouched owned files to delete.
    pub remove: Vec<PathBuf>,
    /// The recorded digest of each file in `remove`. `revert` checks it again
    /// just before the delete.
    pub digests: HashMap<PathBuf, String>,
    /// Entries whose file is gone. Drop them from the manifest.
    pub forget: Vec<PathBuf>,
    /// Files kept, and why.
    pub warnings: Vec<String>,
}

/// Find the entries `invocation` wrote that no action in `actions` names.
///
/// Delete an `owned` file only when its digest still matches and the plan
/// creates a file in the same directory. Keep every other file, with a
/// warning that names it. `--force` has no effect here: these paths come
/// from the manifest, not from the plan.
pub(super) fn plan_sweep(
    project_root: &Path,
    recorded: &Provenance,
    invocation: &str,
    actions: &[Action],
    revert_targets: &[&Path],
) -> Sweep {
    let mut planned = Planned::new(project_root, actions);
    // A revert can rewrite or delete its target. Leave that file to it.
    for path in revert_targets {
        if let Some(key) = provenance::key(project_root, path) {
            planned
                .folded
                .push((key.to_ascii_lowercase(), path.to_path_buf()));
            planned.keys.insert(key);
        }
    }
    let mut sweep = Sweep::default();
    for entry in recorded.recorded_by(invocation) {
        sweep_one(project_root, entry, &planned, &mut sweep);
    }
    sweep
}

/// What the plan names, in the forms the sweep compares against.
struct Planned {
    /// Every key, exactly as spelled.
    keys: HashSet<String>,
    /// Every key in lowercase, with its path.
    folded: Vec<(String, PathBuf)>,
    /// The parent directory key of each `Create`/`CreateBytes`, exact case.
    owned_dirs: HashSet<String>,
    /// The suffix of each migration directory, lowercase.
    migration_suffixes: HashSet<String>,
}

impl Planned {
    fn new(project_root: &Path, actions: &[Action]) -> Self {
        let mut planned = Self {
            keys: HashSet::new(),
            folded: Vec::new(),
            owned_dirs: HashSet::new(),
            migration_suffixes: HashSet::new(),
        };
        for action in actions {
            let Some(key) = provenance::key(project_root, action.path()) else {
                continue;
            };
            let lower = key.to_ascii_lowercase();
            if matches!(action, Action::Create { .. } | Action::CreateBytes { .. }) {
                planned.owned_dirs.insert(parent_key(&key).to_owned());
            }
            if let ["migrations", dir, _] = lower.split('/').collect::<Vec<_>>().as_slice() {
                planned.migration_suffixes.insert(migration_suffix(dir));
            }
            planned.folded.push((lower, action.path().to_path_buf()));
            planned.keys.insert(key);
        }
        planned
    }

    /// Whether the plan handles the file at `key`.
    ///
    /// An exact match does. A match that differs only in ASCII case does when
    /// both spellings reach one directory entry, as on a case-insensitive
    /// file system. Two entries are two names to remove, even when they are
    /// hard links to one file, so the old one is then an orphan.
    fn names(&self, key: &str, path: &Path) -> bool {
        if self.keys.contains(key) {
            return true;
        }
        let lower = key.to_ascii_lowercase();
        self.folded.iter().any(|(k, planned)| {
            *k == lower && same_file(path, planned) && !(listed(path) && listed(planned))
        })
    }
}

/// Whether the directory of `path` lists an entry with exactly its name.
fn listed(path: &Path) -> bool {
    match (path.parent(), path.file_name().and_then(|n| n.to_str())) {
        (Some(dir), Some(name)) => has_exact_entry(dir, name),
        _ => false,
    }
}

/// The directory part of a key, or `""` for a top-level file.
fn parent_key(key: &str) -> &str {
    key.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Whether `a` and `b` are one file on disk.
///
/// On Unix this compares device and inode. Elsewhere it compares the
/// canonical paths, which ignore case only where the file system does.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

/// Decide what to do with one entry.
fn sweep_one(project_root: &Path, entry: Recorded<'_>, planned: &Planned, sweep: &mut Sweep) {
    let Some(parts) = plain_parts(entry.key) else {
        sweep.warnings.push(format!(
            "{:?} in {MANIFEST_PATH} is not a plain project path. autumn destroy does not \
             use it.",
            entry.key
        ));
        return;
    };
    let shown = entry.key;
    let lower = shown.to_ascii_lowercase();
    let path = parts
        .iter()
        .fold(project_root.to_path_buf(), |p, part| p.join(part));
    if planned.names(shown, &path) {
        return;
    }
    let found = inspect(project_root, &parts);
    if matches!(found, Inspect::Missing) {
        // Dropping the entry of a missing file deletes nothing.
        sweep.forget.push(path);
        return;
    }
    let warn = |sweep: &mut Sweep, why: &str| sweep.warnings.push(format!("{shown}: {why}"));
    if lower.starts_with("migrations/") {
        // The plan matches migrations by suffix, not by timestamp. Only the
        // applied-migration guard of that pass may remove one.
        let matched = parts.len() == 3
            && planned
                .migration_suffixes
                .contains(&migration_suffix(parts[1]));
        if !matched {
            warn(
                sweep,
                "the current templates do not make this migration file. autumn destroy \
                 never sweeps migrations. Remove it by hand if no database uses it.",
            );
        }
        return;
    }
    // A dot segment (`.env`, `.git/`, `.autumn/`) or `target/` is never a
    // generator's own output to sweep, whatever the manifest says.
    if PROTECTED_FILES.contains(&lower.as_str())
        || parts.iter().any(|part| part.starts_with('.'))
        || parts[0].eq_ignore_ascii_case("target")
    {
        warn(sweep, "autumn destroy never sweeps this file.");
        return;
    }
    if let Inspect::Refused(reason) = found {
        warn(
            sweep,
            &format!("autumn destroy does not sweep this file, because {reason}."),
        );
        return;
    }
    match entry.scope {
        None => warn(
            sweep,
            "an older Autumn CLI recorded this file for this command, and the current \
             templates do not make it. autumn destroy cannot prove that only this resource \
             uses it, so it keeps the file. Delete it by hand if nothing uses it.",
        ),
        Some(Scope::Shared) => warn(
            sweep,
            "this command made this shared file, and the current templates do not make it. \
             Other resources can use it, so autumn destroy keeps it. Delete it by hand if \
             nothing uses it.",
        ),
        // The manifest is project content, so a forged entry can name any
        // file and its digest. Delete only in a directory where the current
        // templates also create a file. Then a forged entry cannot reach a
        // file outside the generator's own directories.
        Some(Scope::Owned) if !planned.owned_dirs.contains(parent_key(shown)) => warn(
            sweep,
            "the current templates do not make this file, and they create no file in its \
             directory. autumn destroy keeps it. Delete it by hand if you do not need it.",
        ),
        Some(Scope::Owned) if !untouched(&path, entry.digest) => warn(
            sweep,
            "the current templates do not make this file, and it changed after generate \
             wrote it. autumn destroy keeps it. Delete it by hand if you do not need it.",
        ),
        // The reverts come from the current templates, so they do not remove
        // the `mod` line an older template added for this file. Deleting a
        // file that a `mod` line still declares stops the build.
        Some(Scope::Owned) if declared_module(project_root, &parts) => warn(
            sweep,
            "the current templates do not make this file, but a `mod` line still declares \
             it. autumn destroy keeps it. Remove the `mod` line and the file by hand if you \
             do not need them.",
        ),
        Some(Scope::Owned) => {
            sweep.digests.insert(path.clone(), entry.digest.to_owned());
            sweep.remove.push(path);
        }
    }
}

/// Whether a `mod` item can still load the Rust file at `parts`.
///
/// Reads every Rust file in the project, because a parent module can be
/// inline in any of them (`mod jobs { mod foo; }` in `src/main.rs`). Any
/// `mod <stem>` counts, and so does a `#[path]` that names the file. This
/// errs on the side of "declared": a module of the same name elsewhere, or a
/// comment that looks like one, also keeps the file.
fn declared_module(project_root: &Path, parts: &[&str]) -> bool {
    let Some(file) = parts.last() else {
        return false;
    };
    let Some(stem) = file.strip_suffix(".rs") else {
        return false;
    };
    any_rust_file(project_root, |text| {
        declares(text, stem) || names_path(text, file)
    })
}

/// Whether `text` holds `mod`, whitespace, `<stem>` (or `r#<stem>`), then
/// `;` or `{`. Any Rust whitespace counts, newlines too.
fn declares(text: &str, stem: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices("mod").any(|(at, _)| {
        let before_ok = text[..at].chars().next_back().is_none_or(|c| !ident(c));
        let rest = &text[at + 3..];
        let name = rest.trim_start();
        let spaced = name.len() < rest.len();
        let name = name.strip_prefix("r#").unwrap_or(name);
        before_ok
            && spaced
            && name
                .strip_prefix(stem)
                .is_some_and(|tail| tail.trim_start().starts_with([';', '{']))
    })
}

/// Whether any Rust file in the project satisfies `test`.
fn any_rust_file(project_root: &Path, test: impl Fn(&str) -> bool) -> bool {
    let mut pending = vec![project_root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // Build output and hidden directories hold no project source.
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    pending.push(entry.path());
                }
            } else if kind.is_file()
                && name.ends_with(".rs")
                && fs::read_to_string(entry.path()).is_ok_and(|text| test(&text))
            {
                return true;
            }
        }
    }
    false
}

/// Whether `text` holds `path`, `=`, and a string literal (plain or raw) that
/// ends in `file`.
fn names_path(text: &str, file: &str) -> bool {
    text.match_indices("path").any(|(at, _)| {
        let Some(rest) = text[at + 4..].trim_start().strip_prefix('=') else {
            return false;
        };
        // A plain `"…"` or a raw `r"…"` / `r#"…"#` literal.
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix('r')
            .map_or(rest, |raw| raw.trim_start_matches('#'));
        let Some(literal) = rest.strip_prefix('"') else {
            return false;
        };
        literal.split('"').next().is_some_and(|value| {
            value
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|last| last == file)
        })
    })
}

/// The suffix of a migration directory name, lowercase.
fn migration_suffix(dir: &str) -> String {
    split_migration_dir_name(dir).1.to_ascii_lowercase()
}

/// The segments of `key`, when every one is a plain file name.
///
/// Refuses an empty segment, `.`, `..`, a backslash, a colon (a Windows drive
/// or stream), a NUL, a `~` (a Windows short name), and a trailing dot or
/// space (Windows drops them). A key that passes cannot leave the project.
fn plain_parts(key: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = key.split('/').collect();
    let plain = |part: &&str| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && !part.contains(['\\', ':', '\0', '~'])
            && !part.ends_with(['.', ' '])
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
/// symlinked file is not the file `generate` wrote. Each segment must also be
/// the exact name on disk: a case-insensitive or Windows file system can
/// resolve another spelling to the same file, which would let one file pass
/// a check under one name and go under another.
fn inspect(project_root: &Path, parts: &[&str]) -> Inspect {
    let mut cursor = project_root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        let parent = cursor.clone();
        cursor.push(part);
        let last = i + 1 == parts.len();
        match fs::symlink_metadata(&cursor) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Inspect::Missing,
            Err(e) => return Inspect::Refused(format!("it cannot be read ({e})")),
            Ok(_) if !has_exact_entry(&parent, part) => {
                return Inspect::Refused("its name on disk is spelled differently".to_owned());
            }
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

/// Whether `dir` holds an entry named exactly `name`, byte for byte.
fn has_exact_entry(dir: &Path, name: &str) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == std::ffi::OsStr::new(name))
    })
}

/// Whether `path` is still a regular file, reached through no symlink, that
/// holds `digest`.
///
/// `revert` calls this again just before it deletes a swept file. So a
/// directory swapped for a symlink, or an edit saved after the plan was made,
/// keeps the file.
pub(super) fn still_sweepable(project_root: &Path, path: &Path, digest: &str) -> bool {
    provenance::key(project_root, path).is_some_and(|key| {
        plain_parts(&key)
            .is_some_and(|parts| matches!(inspect(project_root, &parts), Inspect::File))
    }) && untouched(path, digest)
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

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/new.rs")], &[]);

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

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/new.rs")], &[]);
        assert_eq!(sweep.remove, vec![old]);
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

        let sweep = plan_sweep(root, &p, ME, &[create(root, "static/new.png")], &[]);
        assert_eq!(sweep.remove, vec![old]);
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

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/new.rs")], &[]);

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

        let sweep = plan_sweep(root, &p, ME, &[], &[]);

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

        let sweep = plan_sweep(root, &p, ME, &[], &[]);

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

        assert_eq!(plan_sweep(root, &p, ME, &[], &[]), Sweep::default());
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

        assert_eq!(plan_sweep(root, &p, ME, &actions, &[]), Sweep::default());
    }

    #[test]
    fn a_case_only_match_is_one_file_only_when_it_is_one_entry() {
        // A case-insensitive file system lists one entry for both spellings:
        // the plan removes it. A case-sensitive one can hold two hard-linked
        // entries: removing `post.rs` leaves `Post.rs`, so it is an orphan.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(
            root,
            &mut p,
            "src/models/Post.rs",
            "x",
            ME,
            Some(Scope::Owned),
        );
        let new = root.join("src/models/post.rs");
        let one_entry = new.exists();
        if !one_entry && fs::hard_link(&old, &new).is_err() {
            return; // no hard links on this file system; nothing to test.
        }

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/models/post.rs")], &[]);

        if one_entry {
            assert_eq!(sweep, Sweep::default());
        } else {
            assert_eq!(sweep.remove, vec![old]);
        }
    }

    #[test]
    fn a_case_only_rename_on_a_case_sensitive_host_is_swept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(
            root,
            &mut p,
            "src/models/Post.rs",
            "x",
            ME,
            Some(Scope::Owned),
        );
        if root.join("src/models/post.rs").exists() {
            return; // a case-insensitive file system: one file, not two.
        }

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/models/post.rs")], &[]);

        assert_eq!(sweep.remove, vec![old]);
    }

    #[test]
    fn an_orphan_outside_every_planned_directory_is_kept() {
        // A forged entry can name any file and its digest. Without a planned
        // file next to it, the sweep must not delete it.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let readme = generated(root, &mut p, "README.md", "# app\n", ME, Some(Scope::Owned));
        let other = generated(root, &mut p, "src/other/x.rs", "x", ME, Some(Scope::Owned));

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/widgets/new.rs")], &[]);

        assert!(sweep.remove.is_empty(), "{:?}", sweep.remove);
        assert_eq!(sweep.warnings.len(), 2, "{:?}", sweep.warnings);
        assert!(readme.exists() && other.exists());
    }

    #[test]
    fn a_modify_target_does_not_make_its_directory_sweepable() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(root, &mut p, "src/notes.rs", "x", ME, Some(Scope::Owned));
        let actions = [Action::Modify {
            path: root.join("src/routes.rs"),
            contents: String::new(),
        }];

        let sweep = plan_sweep(root, &p, ME, &actions, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1);
    }

    #[test]
    fn a_missing_orphan_drops_its_entry() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(root, &mut p, "src/old.rs", "x", ME, Some(Scope::Owned));
        fs::remove_file(&old).unwrap();

        let sweep = plan_sweep(root, &p, ME, &[], &[]);

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

        assert_eq!(plan_sweep(root, &p, ME, &actions, &[]), Sweep::default());
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

        let sweep = plan_sweep(root, &p, ME, &[], &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1);
        assert!(
            sweep.warnings[0].contains("migration"),
            "{:?}",
            sweep.warnings
        );
    }

    #[test]
    fn an_orphan_migration_beside_another_planned_migration_is_never_swept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(
            root,
            &mut p,
            "migrations/20260101000000_create_widgets/up.sql",
            "x",
            ME,
            Some(Scope::Owned),
        );
        let actions = [create(
            root,
            "migrations/20261021000000_create_posts/up.sql",
        )];

        let sweep = plan_sweep(root, &p, ME, &actions, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
        assert!(old.exists());
    }

    #[test]
    fn a_nested_migration_file_is_never_swept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(
            root,
            &mut p,
            "migrations/20260101000000_create_posts/data/seed.sql",
            "x",
            ME,
            Some(Scope::Owned),
        );
        let actions = [create(
            root,
            "migrations/20261021000000_create_posts/up.sql",
        )];

        let sweep = plan_sweep(root, &p, ME, &actions, &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
    }

    #[test]
    fn a_protected_project_file_is_never_swept() {
        for rel in [
            "src/main.rs",
            "Cargo.toml",
            "CARGO.TOML",
            "Cargo.lock",
            "build.rs",
            "src/lib.rs",
            "autumn.toml",
            ".git/config",
            ".env",
            ".autumn/generated.toml",
            "src/.hidden/x.rs",
            "target/debug/x",
        ] {
            let dir = tmp();
            let root = dir.path();
            let mut p = Provenance::default();
            let path = generated(root, &mut p, rel, "x", ME, Some(Scope::Owned));
            // A planned file next to it, so only the protection can stop it.
            let sibling = match parent_key(rel) {
                "" => "planned.rs".to_owned(),
                dir => format!("{dir}/planned.rs"),
            };

            let sweep = plan_sweep(root, &p, ME, &[create(root, &sibling)], &[]);

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
            "src/./a.rs",
            "src/a\0b.rs",
        ] {
            let p = crafted(&root, key);
            let sweep = plan_sweep(&root, &p, ME, &[create(&root, "src/new.rs")], &[]);
            assert!(sweep.remove.is_empty(), "{key:?} must be refused");
            assert!(sweep.forget.is_empty(), "{key:?} must not be forgotten");
            assert_eq!(sweep.warnings.len(), 1, "{key:?}: {:?}", sweep.warnings);
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
        let sweep = plan_sweep(&root, &p, ME, &[create(&root, "src/new.rs")], &[]);

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
        let sweep = plan_sweep(&root, &p, ME, &[create(&root, "src/new.rs")], &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
    }

    #[test]
    fn a_directory_at_the_orphan_path_is_kept() {
        let dir = tmp();
        let root = dir.path();
        fs::create_dir_all(root.join("src/old.rs")).unwrap();
        let p = crafted(root, "src/old.rs");

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/new.rs")], &[]);

        assert!(sweep.remove.is_empty());
        assert!(sweep.forget.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
    }

    #[test]
    fn an_orphan_a_mod_line_still_declares_is_kept() {
        // v1 wrote `src/jobs/foo.rs` and `pub mod foo;`. v2 names `foo_job.rs`,
        // so its revert misses `pub mod foo;`. Deleting `foo.rs` would stop
        // the build.
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(root, &mut p, "src/jobs/foo.rs", "x", ME, Some(Scope::Owned));
        fs::write(root.join("src/jobs/mod.rs"), "pub mod bar;\npub mod foo;\n").unwrap();

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/jobs/foo_job.rs")], &[]);

        assert!(sweep.remove.is_empty());
        assert_eq!(sweep.warnings.len(), 1, "{:?}", sweep.warnings);
        assert!(sweep.warnings[0].contains("mod"), "{:?}", sweep.warnings);
        assert!(old.exists());
    }

    #[test]
    fn an_orphan_declared_in_main_rs_is_kept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(root, &mut p, "src/helpers.rs", "x", ME, Some(Scope::Owned));
        fs::write(root.join("src/main.rs"), "mod helpers;\nfn main() {}\n").unwrap();

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/new.rs")], &[]);

        assert!(sweep.remove.is_empty());
    }

    #[test]
    fn an_orphan_declared_in_a_sibling_module_file_is_kept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        generated(root, &mut p, "src/jobs/foo.rs", "x", ME, Some(Scope::Owned));
        fs::write(root.join("src/jobs.rs"), "pub mod foo {}\n").unwrap();

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/jobs/new.rs")], &[]);

        assert!(sweep.remove.is_empty());
    }

    #[test]
    fn an_orphan_an_inline_parent_module_declares_is_kept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(root, &mut p, "src/jobs/foo.rs", "x", ME, Some(Scope::Owned));
        fs::write(
            root.join("src/main.rs"),
            "mod jobs {\n    pub mod foo;\n}\nfn main() {}\n",
        )
        .unwrap();

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/jobs/new.rs")], &[]);

        assert!(sweep.remove.is_empty(), "{:?}", sweep.remove);
        assert!(old.exists());
    }

    #[test]
    fn declares_matches_only_the_whole_module_name() {
        assert!(declares("pub mod foo;", "foo"));
        assert!(declares("mod foo ;", "foo"));
        assert!(declares("pub(crate) mod foo {", "foo"));
        assert!(!declares("pub mod foo_bar;", "foo"));
        assert!(!declares("pub mod barfoo;", "foo"));
        assert!(!declares("pub mod bar;", "foo"));
        assert!(!declares("// a mod for foo\n", "foo"));
        assert!(declares("mod\tfoo;", "foo"));
        assert!(declares("pub mod\nfoo\n;", "foo"));
        assert!(declares("mod r#foo;", "foo"));
        assert!(!declares("modfoo;", "foo"));
    }

    #[test]
    fn an_orphan_a_path_attribute_loads_is_kept() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let old = generated(root, &mut p, "src/jobs/foo.rs", "x", ME, Some(Scope::Owned));
        fs::write(
            root.join("src/main.rs"),
            "#[path = \"jobs/foo.rs\"]\nmod bar;\nfn main() {}\n",
        )
        .unwrap();

        let sweep = plan_sweep(root, &p, ME, &[create(root, "src/jobs/new.rs")], &[]);

        assert!(sweep.remove.is_empty(), "{:?}", sweep.remove);
        assert!(old.exists());
    }

    #[test]
    fn names_path_matches_only_the_file_name() {
        assert!(names_path("#[path = \"jobs/foo.rs\"]", "foo.rs"));
        assert!(names_path("#[path=\"foo.rs\"]", "foo.rs"));
        assert!(names_path("#[path = \"a\\\\foo.rs\"]", "foo.rs"));
        assert!(!names_path("#[path = \"jobs/foo.rs.bak\"]", "foo.rs"));
        assert!(!names_path("#[path = \"jobs/barfoo.rs\"]", "foo.rs"));
        assert!(!names_path("let path = 3;", "foo.rs"));
        assert!(names_path("#[path = r\"jobs/foo.rs\"]", "foo.rs"));
        assert!(names_path("#[path = r#\"foo.rs\"#]", "foo.rs"));
    }

    #[test]
    fn a_revert_target_is_left_to_the_revert() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let target = generated(
            root,
            &mut p,
            "src/jobs/list.rs",
            "x",
            ME,
            Some(Scope::Owned),
        );

        let sweep = plan_sweep(
            root,
            &p,
            ME,
            &[create(root, "src/jobs/new.rs")],
            &[target.as_path()],
        );

        assert_eq!(sweep, Sweep::default());
    }

    #[test]
    fn a_missing_migration_or_protected_entry_is_forgotten() {
        let dir = tmp();
        let root = dir.path();
        let mut p = Provenance::default();
        let migration = generated(
            root,
            &mut p,
            "migrations/20260101000000_create_widgets/up.sql",
            "x",
            ME,
            Some(Scope::Owned),
        );
        let dotfile = generated(root, &mut p, ".env.example", "x", ME, Some(Scope::Owned));
        fs::remove_file(&migration).unwrap();
        fs::remove_file(&dotfile).unwrap();

        let mut sweep = plan_sweep(root, &p, ME, &[], &[]);
        sweep.forget.sort();

        assert_eq!(sweep.forget, vec![dotfile, migration]);
        assert!(sweep.warnings.is_empty(), "{:?}", sweep.warnings);
    }

    #[test]
    fn windows_aliases_are_not_plain_parts() {
        for key in ["Cargo.toml.", "src/main.rs ", "PROGRA~1/x.rs", "src/a.rs."] {
            assert!(plain_parts(key).is_none(), "{key:?}");
        }
        assert_eq!(plain_parts("src/a.rs"), Some(vec!["src", "a.rs"]));
    }

    #[test]
    fn a_differently_spelled_name_is_refused_where_it_resolves() {
        // Only a case-insensitive file system resolves `SRC/old.rs` to
        // `src/old.rs`. There, the exact-name check must refuse it.
        let dir = tmp();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/old.rs"), "x").unwrap();
        if !root.join("SRC/old.rs").exists() {
            return; // case-sensitive: the alias is simply missing.
        }
        let p = crafted(root, "SRC/old.rs");

        let sweep = plan_sweep(root, &p, ME, &[create(root, "SRC/new.rs")], &[]);

        assert!(sweep.remove.is_empty());
        assert!(root.join("src/old.rs").exists());
    }

    #[test]
    fn still_sweepable_rechecks_the_path() {
        let dir = tmp();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/old.rs");
        fs::write(&file, "x").unwrap();
        let digest = text_digest("x");
        assert!(still_sweepable(root, &file, &digest));
        fs::write(&file, "edited").unwrap();
        assert!(!still_sweepable(root, &file, &digest), "an edit saved late");
        fs::remove_file(&file).unwrap();
        assert!(!still_sweepable(root, &file, &digest));
        assert!(!still_sweepable(root, &root.join("../x.rs"), &digest));
    }
}
