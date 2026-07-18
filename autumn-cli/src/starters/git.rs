//! Git starter resolution: shorthand expansion, ref pinning, and fetching.
//!
//! Community starters (issue #993) are distributed as git repositories. A
//! `--starter` value that is neither a built-in name nor a local directory is
//! treated as a git source: either a full URL (`https://`, `git@`, `ssh://`) or
//! an `owner/repo` GitHub shorthand. A ref (tag / branch / rev) may be pinned
//! either with an `@ref` suffix on the value or via `--starter-ref`.

use std::path::Path;
use std::process::Command;

use super::StarterError;

/// A resolved git source: where to clone from and the optional ref to pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    /// The clone URL (already expanded from any `owner/repo` shorthand).
    pub url: String,
    /// The tag / branch / rev to check out, if pinned. `None` clones the
    /// repository's default branch.
    pub reference: Option<String>,
}

/// True when `value` looks like a full git URL rather than an `owner/repo`
/// shorthand.
fn is_full_url(value: &str) -> bool {
    value.starts_with("https://")
        || value.starts_with("http://")
        || value.starts_with("git@")
        || value.starts_with("ssh://")
        || value.starts_with("git://")
        || value.starts_with("file://")
}

/// Resolve a `--starter` value (known to be a git source) plus an optional
/// `--starter-ref` override into a [`GitSource`].
///
/// Ref precedence: an explicit `ref_override` wins; otherwise an `@ref` suffix
/// embedded in `value` is used. The two are mutually exclusive — supplying both
/// is rejected so the pinned ref is never ambiguous.
///
/// `owner/repo` shorthands expand to `https://github.com/owner/repo.git`. Full
/// URLs (including `scp`-style `git@host:owner/repo`) pass through untouched and
/// are *not* split on `@`, so the user/host separator is preserved.
///
/// # Errors
/// Returns [`StarterError::AmbiguousRef`] when both an `@ref` suffix and
/// `--starter-ref` are given, or [`StarterError::InvalidSource`] when a
/// shorthand is not a valid `owner/repo`.
pub fn resolve(value: &str, ref_override: Option<&str>) -> Result<GitSource, StarterError> {
    if is_full_url(value) {
        // Full URLs may legitimately contain `@` (scp-style `git@host:...`), so
        // never split them; only `--starter-ref` can pin a ref here.
        return Ok(GitSource {
            url: value.to_owned(),
            reference: ref_override.map(str::to_owned),
        });
    }

    // Shorthand form: optionally `owner/repo@ref`.
    let (repo, suffix_ref) = match value.split_once('@') {
        Some((repo, r)) => (repo, Some(r)),
        None => (value, None),
    };

    if let (Some(_), Some(_)) = (suffix_ref, ref_override) {
        return Err(StarterError::AmbiguousRef);
    }

    let reference = ref_override
        .map(str::to_owned)
        .or_else(|| suffix_ref.map(str::to_owned));

    let url = expand_shorthand(repo)?;
    Ok(GitSource { url, reference })
}

/// Expand an `owner/repo` GitHub shorthand into a clone URL.
fn expand_shorthand(repo: &str) -> Result<String, StarterError> {
    let parts: Vec<&str> = repo.split('/').collect();
    let valid = parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        });
    if !valid {
        return Err(StarterError::InvalidSource(format!(
            "'{repo}' is not a valid starter source — expected a built-in name, \
             a local path, a full git URL, or an 'owner/repo' GitHub shorthand"
        )));
    }
    Ok(format!("https://github.com/{repo}.git"))
}

/// True when `reference` is a full commit SHA (40-hex SHA-1 or 64-hex SHA-256).
///
/// `git clone --branch` rejects raw commit SHAs; those require a separate
/// fetch+checkout flow instead of `--branch`.
fn is_full_sha(r: &str) -> bool {
    (r.len() == 40 || r.len() == 64) && r.chars().all(|c| c.is_ascii_hexdigit())
}

/// Shallow-clone `source` into `dest`, checking out a pinned ref when present.
///
/// Shells out to the system `git` (consistent with the repo's existing tooling;
/// no `git2`/`gix` dependency). A missing `git` binary is mapped to a clear,
/// actionable error.
///
/// Ref handling:
/// - No ref: `git clone --depth 1 <url> <dest>` (default branch).
/// - Branch / tag: `git clone --depth 1 --branch <ref> <url> <dest>`.
/// - Full 40-hex SHA: full clone then `git checkout <sha>`, because
///   `--branch` does not accept raw commit SHAs.
///
/// # Errors
/// Returns [`StarterError::GitNotInstalled`] when `git` is not on `PATH`, or
/// [`StarterError::CloneFailed`] when any git command exits non-zero.
pub fn clone_into(source: &GitSource, dest: &Path) -> Result<(), StarterError> {
    let map_io = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StarterError::GitNotInstalled
        } else {
            StarterError::CloneFailed(e.to_string())
        }
    };

    match &source.reference {
        Some(r) if is_full_sha(r) => {
            // Full clone then checkout — the only reliable path for raw SHAs.
            let output = Command::new("git")
                .args(["clone", &source.url])
                .arg(dest)
                .output()
                .map_err(map_io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(StarterError::CloneFailed(format!(
                    "git clone of {} failed: {}",
                    source.url,
                    stderr.trim()
                )));
            }
            let output = Command::new("git")
                .args(["-C"])
                .arg(dest)
                .args(["checkout", r])
                .output()
                .map_err(map_io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(StarterError::CloneFailed(format!(
                    "git checkout of {} in {} failed: {}",
                    r,
                    dest.display(),
                    stderr.trim()
                )));
            }
        }
        ref_ => {
            // Shallow clone; optionally pin a branch or tag with --branch.
            let mut cmd = Command::new("git");
            cmd.arg("clone").arg("--depth").arg("1");
            if let Some(reference) = ref_ {
                cmd.arg("--branch").arg(reference);
            }
            cmd.arg(&source.url).arg(dest);

            let output = cmd.output().map_err(map_io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(StarterError::CloneFailed(format!(
                    "git clone of {} failed: {}",
                    source.url,
                    stderr.trim()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_expands_to_github_https() {
        let s = resolve("madmax983/autumn-starter-blog", None).unwrap();
        assert_eq!(
            s.url,
            "https://github.com/madmax983/autumn-starter-blog.git"
        );
        assert_eq!(s.reference, None);
    }

    #[test]
    fn shorthand_with_at_ref_pins_ref() {
        let s = resolve("owner/repo@v1.2.0", None).unwrap();
        assert_eq!(s.url, "https://github.com/owner/repo.git");
        assert_eq!(s.reference.as_deref(), Some("v1.2.0"));
    }

    #[test]
    fn starter_ref_flag_pins_ref() {
        let s = resolve("owner/repo", Some("main")).unwrap();
        assert_eq!(s.reference.as_deref(), Some("main"));
    }

    #[test]
    fn at_suffix_and_ref_flag_conflict() {
        let err = resolve("owner/repo@v1", Some("main")).unwrap_err();
        assert!(matches!(err, StarterError::AmbiguousRef));
    }

    #[test]
    fn full_https_url_passes_through() {
        let s = resolve("https://gitlab.com/group/proj.git", None).unwrap();
        assert_eq!(s.url, "https://gitlab.com/group/proj.git");
    }

    #[test]
    fn full_https_url_ref_only_from_flag() {
        let s = resolve("https://gitlab.com/group/proj.git", Some("dev")).unwrap();
        assert_eq!(s.url, "https://gitlab.com/group/proj.git");
        assert_eq!(s.reference.as_deref(), Some("dev"));
    }

    #[test]
    fn scp_style_url_not_split_on_at() {
        // `git@github.com:owner/repo.git` must keep its user@host intact.
        let s = resolve("git@github.com:owner/repo.git", None).unwrap();
        assert_eq!(s.url, "git@github.com:owner/repo.git");
        assert_eq!(s.reference, None);
    }

    #[test]
    fn invalid_shorthand_rejected() {
        assert!(matches!(
            resolve("not-a-repo", None).unwrap_err(),
            StarterError::InvalidSource(_)
        ));
        assert!(matches!(
            resolve("too/many/parts", None).unwrap_err(),
            StarterError::InvalidSource(_)
        ));
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Drive `clone_into` against a real local repository over `file://`, so the
    /// git fetch path is exercised end-to-end without any network. Skipped when
    /// `git` is not installed.
    #[test]
    fn clone_into_fetches_a_file_url_repo() {
        if !git_available() {
            eprintln!("skipping: git not installed");
            return;
        }

        let origin = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(origin.path())
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t.test"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(origin.path().join("hello.txt"), "hi").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        let dest = tempfile::TempDir::new().unwrap();
        let clone_dir = dest.path().join("clone");
        let source = GitSource {
            url: format!("file://{}", origin.path().display()),
            reference: None,
        };
        clone_into(&source, &clone_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(clone_dir.join("hello.txt")).unwrap(),
            "hi"
        );
    }
}
