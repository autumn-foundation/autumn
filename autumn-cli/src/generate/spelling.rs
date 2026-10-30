//! Case-only spellings of one path (issue #3382).
//!
//! A case-insensitive file system (macOS, Windows) reaches one directory entry
//! through `Foo.rs` and `foo.rs`. A case-sensitive one holds two entries. The
//! orphan sweep and the manifest lookups use this rule to tell the two apart.

use std::fs;
use std::path::Path;

/// Whether `a` and `b` are two spellings of one directory entry.
///
/// The paths must differ only in ASCII case, and both must reach one file.
/// At each name that differs, the directory must not list both names. Two
/// listed names (a hard link, a symlink) are two entries. The result is also
/// false for two equal paths, and when `a` or `b` does not exist.
pub(super) fn one_entry(a: &Path, b: &Path) -> bool {
    a != b && same_file(a, b) && case_only(a, b)
}

/// Walk `a` and `b` up to their common directory. At each name that
/// differs, the names must match in ASCII case, and the directory must not
/// list both.
fn case_only(mut a: &Path, mut b: &Path) -> bool {
    while a != b {
        match (a.file_name(), b.file_name(), a.parent(), b.parent()) {
            (Some(x), Some(y), Some(next_a), Some(next_b)) => {
                if x != y && (!x.eq_ignore_ascii_case(y) || (listed(a) && listed(b))) {
                    return false;
                }
                (a, b) = (next_a, next_b);
            }
            _ => return false,
        }
    }
    true
}

/// Whether `dir` holds an entry named exactly `name`, byte for byte.
pub(super) fn has_exact_entry(dir: &Path, name: &str) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == std::ffi::OsStr::new(name))
    })
}

/// Whether the directory of `path` lists an entry with exactly its name.
///
/// True when the directory cannot be read. An unknown listing must not make
/// two names one entry.
fn listed(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return true;
    };
    fs::read_dir(dir).map_or(true, |entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == name)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn two_spellings_are_one_entry_only_on_a_case_insensitive_host() {
        let dir = tmp();
        let upper = dir.path().join("Foo.rs");
        fs::write(&upper, "x").unwrap();
        let lower = dir.path().join("foo.rs");

        assert_eq!(one_entry(&upper, &lower), lower.exists());
        assert_eq!(one_entry(&lower, &upper), lower.exists());
    }

    #[test]
    fn two_spellings_of_a_directory_follow_the_same_rule() {
        let dir = tmp();
        let upper = dir.path().join("Widgets");
        fs::create_dir(&upper).unwrap();
        let lower = dir.path().join("widgets");

        assert_eq!(one_entry(&upper, &lower), lower.exists());
    }

    #[test]
    fn a_hard_link_is_two_entries() {
        let dir = tmp();
        let upper = dir.path().join("Foo.rs");
        fs::write(&upper, "x").unwrap();
        let lower = dir.path().join("foo.rs");
        if lower.exists() || fs::hard_link(&upper, &lower).is_err() {
            return; // case-insensitive, or no hard links: nothing to test.
        }

        assert!(!one_entry(&upper, &lower));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_two_entries() {
        let dir = tmp();
        let upper = dir.path().join("Foo.rs");
        fs::write(&upper, "x").unwrap();
        let lower = dir.path().join("foo.rs");
        if lower.exists() {
            return; // case-insensitive: one name, no room for a link.
        }
        std::os::unix::fs::symlink(&upper, &lower).unwrap();

        assert!(!one_entry(&upper, &lower));
    }

    #[test]
    fn a_file_in_two_spellings_of_a_directory_follows_the_same_rule() {
        // Only the directory changes case. The file name is the same.
        let dir = tmp();
        let upper = dir.path().join("Widgets/foo.rs");
        fs::create_dir(dir.path().join("Widgets")).unwrap();
        fs::write(&upper, "x").unwrap();
        let lower = dir.path().join("widgets/foo.rs");

        assert_eq!(one_entry(&upper, &lower), lower.exists());
    }

    #[test]
    fn two_equal_paths_are_not_two_spellings() {
        let dir = tmp();
        let file = dir.path().join("Foo.rs");
        fs::write(&file, "x").unwrap();

        assert!(!one_entry(&file, &file));
    }

    #[test]
    fn a_missing_path_is_no_entry() {
        let dir = tmp();
        let upper = dir.path().join("Foo.rs");
        fs::write(&upper, "x").unwrap();

        assert!(!one_entry(&upper, &dir.path().join("Bar.rs")));
        assert!(!one_entry(
            &dir.path().join("Bar.rs"),
            &dir.path().join("bar.rs")
        ));
    }

    #[test]
    fn a_directory_that_cannot_be_read_lists_every_name() {
        // Fail closed: an unknown listing must not make two names one entry.
        assert!(listed(Path::new("Foo.rs")), "the parent is \"\"");
        assert!(listed(&tmp().path().join("gone/Foo.rs")));
    }

    #[test]
    fn has_exact_entry_matches_bytes_not_case() {
        let dir = tmp();
        fs::write(dir.path().join("Foo.rs"), "x").unwrap();

        assert!(has_exact_entry(dir.path(), "Foo.rs"));
        assert!(!has_exact_entry(dir.path(), "foo.rs"));
    }
}
