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

use std::path::{Path, PathBuf};

use super::emit::Action;
use super::provenance::Provenance;

/// What the sweep wants `revert` to do.
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
pub(super) fn plan_sweep(
    project_root: &Path,
    recorded: &Provenance,
    invocation: &str,
    actions: &[Action],
) -> Sweep {
    let _ = (project_root, recorded, invocation, actions);
    Sweep::default()
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
