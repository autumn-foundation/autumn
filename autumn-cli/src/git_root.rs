//! Locate the git repository root.

use std::path::{Path, PathBuf};

/// Find the nearest ancestor of `dir` (inclusive) that holds a `.git` entry.
///
/// A worktree or submodule has a `.git` FILE, not a directory, so any entry
/// counts. Returns `None` when `dir` is not inside a git repository.
pub fn find_git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_git_directory_in_an_ancestor() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_git_root(&nested).as_deref(), Some(tmp.path()));
    }

    #[test]
    fn a_git_file_marks_a_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(find_git_root(tmp.path()).as_deref(), Some(tmp.path()));
    }

    #[test]
    fn the_nearest_root_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let inner = tmp.path().join("sub");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join(".git"), "gitdir: elsewhere\n").unwrap();
        let deep = inner.join("x");
        std::fs::create_dir(&deep).unwrap();
        assert_eq!(find_git_root(&deep).as_deref(), Some(inner.as_path()));
    }
}
