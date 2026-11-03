//! The scripts under `scripts/`, found by listing the directory.
//!
//! Nothing here is a hand-kept registry: a script added to `scripts/` shows up
//! in `cargo xtask list` and runs with `cargo xtask script <name>` without an
//! edit here. Its description is the first paragraph of its header comment
//! (or a Python module's docstring), so a script without a header is a test
//! failure.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result, bail};

use crate::runner;

/// One runnable script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// File name, e.g. `check-msrv.sh`.
    pub file: String,
    /// One-line summary from the header.
    pub about: String,
}

impl Script {
    /// The file name without its extension, e.g. `check-msrv`.
    #[must_use]
    pub fn name(&self) -> &str {
        self.file
            .rsplit_once('.')
            .map_or(self.file.as_str(), |(stem, _)| stem)
    }

    /// The heading `cargo xtask list` files it under.
    #[must_use]
    pub fn group(&self) -> &'static str {
        let name = self.name();
        if name.starts_with("check-docs") {
            "docs gates"
        } else if name.starts_with("check-") {
            "gates"
        } else if name.starts_with("verify-") {
            "formal verification"
        } else if name.starts_with("ci-") {
            "CI helpers"
        } else {
            "tools"
        }
    }
}

/// Every `*.sh` and `*.py` directly under `root/scripts`, sorted by file name.
///
/// # Errors
///
/// Returns an error when the directory or a script cannot be read.
pub fn discover(root: &Path) -> Result<Vec<Script>> {
    let dir = root.join("scripts");
    let mut scripts = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_file() || runner::interpreter_for(&path.to_string_lossy()).is_none() {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .context("non-UTF-8 script name")?
            .to_owned();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        scripts.push(Script {
            about: describe(&text),
            file,
        });
    }
    scripts.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(scripts)
}

/// The first sentence of a script's header, at most 100 characters.
///
/// Reads the first paragraph of `#` comments after the shebang, or the first
/// paragraph of a leading `"""` docstring. Empty when there is neither.
#[must_use]
pub fn describe(text: &str) -> String {
    let mut lines = text.lines().peekable();
    if lines.peek().is_some_and(|l| l.starts_with("#!")) {
        lines.next();
    }
    while lines.peek().is_some_and(|l| l.trim().is_empty()) {
        lines.next();
    }
    let mut paragraph = Vec::new();
    match lines.peek() {
        Some(l) if l.trim_start().starts_with("\"\"\"") => {
            for (i, line) in lines.enumerate() {
                let mut line = line.trim();
                if i == 0 {
                    line = line.trim_start_matches("\"\"\"");
                }
                let end = line.contains("\"\"\"");
                let line = line.split("\"\"\"").next().unwrap_or("").trim();
                if line.is_empty() && i > 0 {
                    break;
                }
                if !line.is_empty() {
                    paragraph.push(line.to_owned());
                }
                if end {
                    break;
                }
            }
        }
        Some(l) if l.starts_with('#') => {
            for line in lines {
                let Some(body) = line.strip_prefix('#') else {
                    break;
                };
                let body = body.trim_start_matches('#').trim();
                if body.is_empty() {
                    if paragraph.is_empty() {
                        continue;
                    }
                    break;
                }
                paragraph.push(body.to_owned());
            }
        }
        _ => {}
    }
    first_sentence(&paragraph.join(" "), 100)
}

fn first_sentence(text: &str, max: usize) -> String {
    let sentence = text.find(". ").map_or(text, |i| &text[..=i]).trim();
    if sentence.chars().count() <= max {
        return sentence.to_owned();
    }
    let mut cut: String = sentence.chars().take(max - 1).collect();
    if let Some(space) = cut.rfind(' ') {
        cut.truncate(space);
    }
    format!("{cut}…")
}

/// Find a script by file name or name without extension.
///
/// # Errors
///
/// Returns an error, with close matches, when nothing matches.
pub fn resolve(root: &Path, name: &str) -> Result<Script> {
    let scripts = discover(root)?;
    let wanted = name.trim_start_matches("scripts/");
    if let Some(found) = scripts
        .iter()
        .find(|s| s.file == wanted || s.name() == wanted)
    {
        return Ok(found.clone());
    }
    let near: Vec<_> = scripts
        .iter()
        .filter(|s| s.name().contains(wanted) || wanted.contains(s.name()))
        .map(|s| s.name().to_owned())
        .collect();
    if near.is_empty() {
        bail!("no script `{name}` under scripts/; `cargo xtask list` shows them all");
    }
    bail!(
        "no script `{name}` under scripts/; did you mean: {}",
        near.join(", ")
    )
}

/// Run a script from `root` with `args`, inheriting stdio.
///
/// # Errors
///
/// Returns an error when the interpreter cannot be started.
pub fn run(root: &Path, script: &Script, args: &[String]) -> Result<ExitStatus> {
    let mut words = vec![format!("scripts/{}", script.file)];
    words.extend(args.iter().cloned());
    let path: PathBuf = root.join("scripts").join(&script.file);
    runner::command_for(root, &words)?
        .current_dir(root)
        .status()
        .with_context(|| format!("starting {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        crate::repo_root()
    }

    #[test]
    fn every_script_has_a_header_to_describe_it() {
        let scripts = discover(root()).unwrap();
        assert!(scripts.len() > 20, "found only {} scripts", scripts.len());
        let bare: Vec<_> = scripts
            .iter()
            .filter(|s| s.about.is_empty())
            .map(|s| s.file.as_str())
            .collect();
        assert!(
            bare.is_empty(),
            "these scripts have no header comment for `cargo xtask list`: {bare:?}"
        );
    }

    #[test]
    fn discovery_skips_data_files_and_subdirectories() {
        let scripts = discover(root()).unwrap();
        assert!(scripts.iter().all(|s| {
            Path::new(&s.file)
                .extension()
                .is_some_and(|e| e == "sh" || e == "py")
        }));
        assert!(!scripts.iter().any(|s| s.file.contains('/')));
    }

    #[test]
    fn describe_reads_the_first_comment_paragraph() {
        let text = "#!/usr/bin/env bash\n# Verify that the MSRV is declared consistently across the workspace,\n# the README, and the CI matrix. Exit non-zero if any source disagrees.\n#\n# Later text.\nset -e\n";
        assert_eq!(
            describe(text),
            "Verify that the MSRV is declared consistently across the workspace, the README, and the CI matrix."
        );
    }

    #[test]
    fn describe_skips_leading_blank_comment_lines() {
        assert_eq!(describe("#!/bin/sh\n#\n# Do a thing.\n"), "Do a thing.");
    }

    #[test]
    fn describe_reads_a_python_docstring() {
        let text =
            "#!/usr/bin/env python3\n\"\"\"Gate: every workload has a profile.\n\nMore.\n\"\"\"\n";
        assert_eq!(describe(text), "Gate: every workload has a profile.");
        assert_eq!(describe("\"\"\"One line.\"\"\"\n"), "One line.");
    }

    #[test]
    fn describe_is_empty_without_a_header() {
        assert_eq!(describe("#!/bin/sh\nset -e\n"), "");
    }

    #[test]
    fn long_descriptions_are_cut_at_a_word() {
        let long = format!("# {}\n", "word ".repeat(40));
        let about = describe(&long);
        assert!(about.chars().count() <= 100, "{about}");
        assert!(about.ends_with("word…"), "{about}");
    }

    #[test]
    fn resolve_accepts_the_file_name_or_the_stem() {
        assert_eq!(resolve(root(), "check-msrv").unwrap().file, "check-msrv.sh");
        assert_eq!(
            resolve(root(), "check-msrv.sh").unwrap().file,
            "check-msrv.sh"
        );
        assert_eq!(
            resolve(root(), "scripts/check-msrv.sh").unwrap().file,
            "check-msrv.sh"
        );
        assert_eq!(
            resolve(root(), "check-perf-profiles").unwrap().file,
            "check-perf-profiles.py"
        );
    }

    #[test]
    fn resolve_suggests_close_names() {
        let err = resolve(root(), "docs-orphans").unwrap_err().to_string();
        assert!(err.contains("check-docs-orphans"), "{err}");
        let err = resolve(root(), "zzzz").unwrap_err().to_string();
        assert!(err.contains("cargo xtask list"), "{err}");
    }

    #[test]
    fn groups_follow_the_name() {
        let s = |file: &str| Script {
            file: file.into(),
            about: String::new(),
        };
        assert_eq!(s("check-docs-links.sh").group(), "docs gates");
        assert_eq!(s("check-msrv.sh").group(), "gates");
        assert_eq!(s("verify-tla.sh").group(), "formal verification");
        assert_eq!(s("ci-prepull-images.sh").group(), "CI helpers");
        assert_eq!(s("update-changelog.sh").group(), "tools");
        assert_eq!(s("update-changelog.sh").name(), "update-changelog");
    }
}
