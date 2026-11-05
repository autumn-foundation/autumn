//! The tracked markdown corpus: which files the docs gates read, and how they
//! are decoded.

use std::path::Path;

use anyhow::{Context, Result};
use smop::sh;

/// Every tracked `*.md` file under `root`, as git spells it (`/`-separated,
/// relative to `root`), in git's order.
///
/// `examples/<app>/content/` is left out: that tree is seed content for an
/// example app, served by the app's own routes, so its `/docs/configuration`
/// is a live route rather than a file path. The match is anchored to that
/// shape, so a real docs tree that happens to be named `content` stays in.
///
/// # Errors
///
/// Returns an error when git cannot list the files. A gate that cannot see
/// its corpus must fail, not report a clean empty one.
pub fn tracked_markdown(root: &Path) -> Result<Vec<String>> {
    // NUL-delimited, so a path with whitespace stays one record and git does
    // not quote unusual names.
    let listed = sh::cmd("git")
        .args(["ls-files", "-z", "*.md"])
        .dir(root)
        .output()
        .with_context(|| format!("listing the markdown corpus in {}", root.display()))?;
    Ok(listed
        .split('\0')
        .filter(|f| !f.is_empty() && !is_example_content(f))
        .map(str::to_owned)
        .collect())
}

/// True for `examples/<app>/content/…`.
#[must_use]
pub fn is_example_content(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("examples/") else {
        return false;
    };
    rest.split_once('/')
        .is_some_and(|(app, tail)| !app.is_empty() && tail.starts_with("content/"))
}

/// Read and decode a corpus file, sniffing a byte-order mark.
///
/// UTF-16 (either order, with its BOM) and UTF-8 with or without a BOM are
/// accepted. `None` means the file is missing or not valid text in any of
/// those encodings; callers report that rather than skip it, because a file
/// that is counted but never read lets a broken link pass.
#[must_use]
pub fn read(path: &Path) -> Option<String> {
    decode(&std::fs::read(path).ok()?)
}

/// The decoding half of [`read`].
#[must_use]
pub fn decode(raw: &[u8]) -> Option<String> {
    if let Some(rest) = raw.strip_prefix(b"\xff\xfe") {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = raw.strip_prefix(b"\xfe\xff") {
        return utf16(rest, u16::from_be_bytes);
    }
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    String::from_utf8(raw.to_vec()).ok()
}

fn utf16(raw: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| unit(pair))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_content_is_matched_by_shape() {
        assert!(is_example_content("examples/wiki/content/a.md"));
        assert!(is_example_content("examples/wiki/content/deep/a.md"));
        assert!(!is_example_content("docs/content/page.md"));
        assert!(!is_example_content("examples/wiki/README.md"));
        assert!(!is_example_content("examples//content/a.md"));
        assert!(!is_example_content("examples/wiki/contents/a.md"));
    }

    #[test]
    fn decode_sniffs_the_bom() {
        assert_eq!(decode(b"plain").as_deref(), Some("plain"));
        assert_eq!(decode(b"\xef\xbb\xbfbom").as_deref(), Some("bom"));
        assert_eq!(decode(b"\xff\xfea\x00b\x00").as_deref(), Some("ab"));
        assert_eq!(decode(b"\xfe\xff\x00a\x00b").as_deref(), Some("ab"));
        assert_eq!(decode(b"Caf\xe9"), None, "Latin-1 is not UTF-8");
        assert_eq!(decode(b"\xff\xfea"), None, "odd UTF-16 length");
        assert_eq!(decode(b"\xff\xfe\x00\xd8"), None, "lone surrogate");
    }

    #[test]
    fn this_repository_has_a_markdown_corpus() {
        let files = tracked_markdown(crate::repo_root()).unwrap();
        assert!(files.iter().any(|f| f == "README.md"));
        assert!(files.iter().all(|f| !is_example_content(f)));
    }

    #[test]
    fn a_directory_outside_git_is_an_error() {
        let dir = smop::fs::temp_dir().unwrap();
        // Only meaningful where the temp dir is not itself inside a checkout.
        let inside = sh::cmd("git")
            .args(["rev-parse", "--git-dir"])
            .dir(dir.path())
            .success()
            .unwrap_or(false);
        let listed = tracked_markdown(dir.path());
        if !inside {
            assert!(
                listed.is_err(),
                "a gate must not see an empty corpus: {listed:?}"
            );
        }
    }
}
