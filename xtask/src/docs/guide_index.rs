//! Guide-index completeness gate: every page in `docs/guide/` must be listed
//! in the index that owns it, and `docs/guide/index.md` must be linked from
//! `README.md`.
//!
//! Reachable is not findable. A guide page whose only inbound link is a
//! crosslink from another guide page, or a mention in an agent `SKILL.md`,
//! can be reached, but a reader with a question has no way to land on it.
//! The first run of this gate found 92 of 147 guide pages in no
//! reader-facing index, among them middleware, testing, migrations, jobs and
//! authorization. The index is a page whose only job is navigation, so it
//! carries no answers of its own to drift against the pages it lists.
//!
//! # What it checks
//!
//! 1. Every tracked page under `docs/guide/` is listed exactly once, counted
//!    across all indexes, by the index that owns it. A page listed twice is a
//!    defect, and so is a page listed by an index that does not own it: the
//!    second entry is the one that rots.
//! 2. Every guide page an index links exists.
//! 3. Every entry sits under a `## ` section heading.
//! 4. `README.md` carries a markdown link (inline or reference) whose target
//!    resolves to the index. A plain-text or code mention does not count.
//!
//! # What an entry is
//!
//! An entry is a column-zero list item whose content begins with a link:
//!
//! ```text
//! - [Forms, Validation and Normalization](forms.md) — re-rendering a …
//! 1. [Project Setup](01-project-setup.md) — scaffold a project, run …
//! ```
//!
//! Matching that shape, rather than "every link a reader could click",
//! rejects fenced code, comments, indented code, images, escaped brackets
//! and raw HTML without a rule for each, and it fails safe: a row written
//! any other way leaves its page listed nowhere, which is reported. It also
//! separates an index's rows from its prose, so "see the [i18n guide]" in a
//! paragraph indexes nothing. The cost is that rows are top level.
//!
//! [`blocks::readable`] removes only the multi-line regions that can still
//! put a row-shaped line at column zero: fenced code, HTML comments and raw
//! HTML blocks, plus the inline constructs a README link could hide in.
//!
//! # Delegation
//!
//! A directory under `docs/guide/` with its own `index.md` (the tutorial)
//! is listed in the parent index by that `index.md` alone, and its index
//! owns its pages. Each page is checked against the nearest index above it,
//! to any depth, so an unlisted chapter fails against its own index rather
//! than leaving the required set.
//!
//! The truth set is `git ls-files docs/guide`, read at run time: a page added
//! in a commit must be listed in the same commit.

mod blocks;
mod entities;
mod scan;
mod text;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use self::scan::{ATX_H1, ATX_H2, Labels, SETEXT_H1, setext_context};
use self::text::Text;
use crate::docs::corpus;
use crate::docs::re::{py_strip, unquote};

const GUIDE: &str = "docs/guide/";
const INDEX: &str = "docs/guide/index.md";
const README: &str = "README.md";

/// How to fix each kind of defect, printed to stderr on failure.
const FIX_HINT: &str = "
FAIL: the guide index does not account for every page in docs/guide/.

Fix each one where it lives:
  - page listed nowhere   -> add one line to docs/guide/index.md, under the
                             `## ` section a reader with that question would
                             scan; write the line in the reader's words, not
                             the internal feature name
  - page listed twice     -> keep the entry under the section a reader would
                             look in first, and delete the other
  - link to nothing       -> the page moved or was deleted; point at the
                             current path, or drop the entry
  - entry above every `## ` heading -> move it under the section it belongs to
  - README link missing   -> restore the `docs/guide/index.md` link in
                             README.md's `## Documentation` list";

/// The outcome of one run.
#[derive(Debug, Default)]
pub struct Report {
    /// Tracked guide pages.
    pub pages: usize,
    /// Every index, sorted.
    pub indexes: Vec<String>,
    /// Pages some index must list.
    pub required: usize,
    /// Entries found across all indexes.
    pub linked: usize,
    /// `(path, why)` for each defect, in report order.
    pub defects: Vec<(String, String)>,
    /// `docs/guide/index.md` is not tracked, so every page is unlisted.
    pub missing_index: bool,
}

impl Report {
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.defects.is_empty() && !self.missing_index
    }
}

/// The tracked markdown files under `docs/guide/`, sorted.
fn tracked(root: &Path) -> Result<Vec<String>> {
    let mut pages: Vec<String> = corpus::tracked_markdown(root)?
        .into_iter()
        .filter(|p| p.starts_with(GUIDE))
        .collect();
    pages.sort();
    Ok(pages)
}

/// Read a file as Python's `open(path, encoding="utf-8")` does: strict UTF-8,
/// with `\r\n` and `\r` read as `\n`.
fn read(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let text =
        String::from_utf8(raw).with_context(|| format!("{} is not UTF-8", path.display()))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Every index under `docs/guide/` mapped to the pages it must list: each
/// page belongs to the nearest index above it, and the top-level index to
/// nobody.
fn index_plan(pages: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let indexes: Vec<&String> = pages
        .iter()
        .filter(|p| *p == INDEX || p.ends_with("/index.md"))
        .collect();
    let mut plan: BTreeMap<String, BTreeSet<String>> = indexes
        .iter()
        .map(|i| ((*i).clone(), BTreeSet::new()))
        .collect();
    for p in pages {
        if p == INDEX {
            continue;
        }
        // No two indexes share a directory, so the longest one is unique.
        let owner = indexes
            .iter()
            .filter(|idx| **idx != p && p.starts_with(&dir_of(idx)))
            .max_by_key(|idx| dir_of(idx).len());
        if let Some(owner) = owner {
            plan.entry((*owner).clone()).or_default().insert(p.clone());
        }
    }
    plan
}

/// `docs/guide/tutorial/index.md` → `docs/guide/tutorial/`.
fn dir_of(path: &str) -> String {
    let dir = path.rsplit_once('/').map_or(path, |(d, _)| d);
    format!("{dir}/")
}

/// Resolve a link target written in a file under `base` (`""` for the repo
/// root) to a repo-relative guide path, or `None` when it leaves the guide.
///
/// A target inside the guide is returned whether or not the page exists, so
/// a link to a deleted page is reported. Character references decode before
/// the fragment is split, then the query, percent-encoding and markdown
/// escapes come off, as the link gate resolves a destination. Any URI scheme
/// and any root-relative path leave the repository.
fn normalise(target: &str, base: &str) -> Option<String> {
    let mut target = py_strip(target).to_owned();
    if scan::is_angle(&target) {
        target = py_strip(&target[1..target.len() - 1]).to_owned();
    }
    let target = scan::decode_char_refs(&target);
    let target = target
        .split('#')
        .next()
        .unwrap_or_default()
        .trim_end_matches(crate::docs::re::is_py_space);
    let target = target.split('?').next().unwrap_or_default();
    let target = scan::unescape_punct(&unquote(target));
    let target = target.trim_end_matches('/');
    if target.is_empty() || scan::has_scheme(target) || target.starts_with('/') {
        return None;
    }
    let target = target.strip_prefix("./").unwrap_or(target);
    let path = if target.starts_with(GUIDE) {
        target.to_owned()
    } else {
        format!("{base}{target}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            ".." => {
                parts.pop()?;
            }
            "" | "." => {}
            _ => parts.push(seg),
        }
    }
    let path = parts.join("/");
    path.starts_with(GUIDE).then_some(path)
}

/// One entry: the page it lists, its line, and whether it sits under a
/// `## ` heading.
struct Entry {
    path: String,
    line: usize,
    in_section: bool,
}

/// Every entry in an index file whose directory is `base`.
///
/// Block structure is decided per line; the link on a row is read at its
/// offset in the whole text, because link text and destination may cross a
/// soft line break. Rows are read with definitions blanked, so a row-shaped
/// line inside a definition's title is not an entry, while the definitions
/// themselves are collected from the whole file first.
fn entries(text: &str, base: &str) -> Vec<Entry> {
    let body = blocks::readable(&Text::new(text), None);
    let defs = scan::definitions(&body);
    let rows = scan::blank_defns(&body);
    let pairs = scan::bracket_pairs(&rows);
    let resolved: Labels = defs.keys().cloned().collect();
    let mut out = Vec::new();
    let mut in_section = false;
    let (mut prev, mut offset) = ("", 0);
    for (n, line) in rows.view().split('\n').enumerate() {
        let start = offset;
        offset += line.len() + 1;
        let above = std::mem::replace(&mut prev, line);
        if ATX_H2.is_match(line) {
            in_section = true;
            continue;
        }
        // A level-one heading ends the section; a `### ` does not.
        if ATX_H1.is_match(line) || (SETEXT_H1.is_match(line) && setext_context(above)) {
            in_section = false;
            continue;
        }
        let Some(row) = scan::row_at(line) else {
            continue;
        };
        let target = match scan::link_at(&rows, start + row, &pairs, &resolved) {
            Some((_, dest)) if !dest.is_empty() => Some(dest),
            _ => scan::ref_at(&rows, start + row, &pairs, &resolved)
                .and_then(|(_, label)| scan::label_key(&label))
                .and_then(|key| defs.get(&key).cloned()),
        };
        if let Some(path) = target.and_then(|t| normalise(&t, base)) {
            out.push(Entry {
                path,
                line: n + 1,
                in_section,
            });
        }
    }
    out
}

/// True when `t` (already [`blocks::readable`]) carries a clickable link,
/// inline or by reference, to the index.
fn reaches_index(t: &Text) -> bool {
    let reaches = |dest: &str| normalise(dest, "").as_deref() == Some(INDEX);
    // A definition renders nothing, so a link inside one's title is not
    // navigation.
    let candidates = blocks::candidate_labels(t);
    if scan::link_spans(&scan::blank_defns(t), &candidates)
        .iter()
        .any(|(_, _, dest)| reaches(dest))
    {
        return true;
    }
    let defs = scan::definitions(t);
    if defs.is_empty() {
        return false;
    }
    // Uses are read with links and definitions blanked: a label inside a
    // title, or inside its own definition, is not a use.
    let blanked = scan::blank_links(&scan::blank_defns(t));
    let resolved: Labels = defs.keys().cloned().collect();
    let used: HashSet<String> = scan::ref_labels(&blanked, &resolved)
        .iter()
        .filter_map(|l| scan::label_key(l))
        .collect();
    used.iter()
        .any(|label| defs.get(label).is_some_and(|d| reaches(d)))
}

/// Run the gate over a corpus given as its tracked guide pages and a reader
/// for the index and README files.
fn check_corpus(pages: &[String], read: &dyn Fn(&str) -> Result<String>) -> Result<Report> {
    if pages.is_empty() {
        bail!(
            "`git ls-files docs/guide` matched no markdown. The guide directory was \
             moved or renamed; this gate has no truth set to read and would pass everything. \
             Fix GUIDE in xtask/src/docs/guide_index.rs."
        );
    }
    let page_set: HashSet<&str> = pages.iter().map(String::as_str).collect();
    let plan = index_plan(pages);
    let mut report = Report {
        pages: pages.len(),
        indexes: plan.keys().cloned().collect(),
        required: plan.values().map(BTreeSet::len).sum(),
        ..Report::default()
    };
    if !plan.contains_key(INDEX) {
        report.missing_index = true;
        return Ok(report);
    }

    // Entries from every index are gathered before anything is judged: a
    // page listed once in each of two indexes is invisible from inside
    // either.
    let owner_of: BTreeMap<&str, &str> = plan
        .iter()
        .flat_map(|(idx, need)| need.iter().map(move |p| (p.as_str(), idx.as_str())))
        .collect();
    let mut seen: BTreeMap<String, Vec<(String, usize, bool)>> = BTreeMap::new();
    for index in plan.keys() {
        for e in entries(&read(index)?, &dir_of(index)) {
            seen.entry(e.path)
                .or_default()
                .push((index.clone(), e.line, e.in_section));
        }
    }
    report.linked = seen.values().map(Vec::len).sum();
    let defects = &mut report.defects;

    for (path, index) in &owner_of {
        if !seen.contains_key(*path) {
            defects.push((
                (*path).to_owned(),
                format!("listed in no section of {index}"),
            ));
        }
    }
    for (path, hits) in &seen {
        if hits.len() > 1 {
            let at: Vec<String> = hits
                .iter()
                .map(|(i, n, _)| format!("{i} line {n}"))
                .collect();
            defects.push((
                path.clone(),
                format!("listed {} times ({})", hits.len(), at.join(", ")),
            ));
        }
        if let Some(owner) = owner_of.get(path.as_str()) {
            for (index, line, _) in hits {
                if index != owner {
                    defects.push((
                        path.clone(),
                        format!(
                            "{index} line {line}: listed here, but {owner} owns this page — \
                             an entry belongs to exactly one index"
                        ),
                    ));
                }
            }
        }
    }
    for (path, hits) in &seen {
        if !page_set.contains(path.as_str()) {
            let (index, line, _) = &hits[0];
            defects.push((
                path.clone(),
                format!("{index} line {line}: no such guide page"),
            ));
        }
    }
    for (path, hits) in &seen {
        for (index, line, in_section) in hits {
            if !in_section {
                defects.push((
                    path.clone(),
                    format!("{index} line {line}: not under any `## ` section heading"),
                ));
            }
        }
    }

    let readme = blocks::readable(&Text::new(&read(README)?), None);
    if !reaches_index(&readme) {
        defects.push((
            README.to_owned(),
            format!(
                "has no markdown link whose target resolves to {INDEX}; a mention the reader \
                 cannot click — plain text, inline code, a fenced example, or a commented-out \
                 link — leaves the index unfindable"
            ),
        ));
    }
    Ok(report)
}

/// Run the gate over the checkout at `root`.
///
/// # Errors
///
/// Returns an error when git cannot list the guide, when it lists no page,
/// or when an index or the README cannot be read as UTF-8.
pub fn check(root: &Path) -> Result<Report> {
    check_corpus(&tracked(root)?, &|rel| read(root, rel))
}

/// Run the gate and print its report. The fix hint goes to stderr.
///
/// # Errors
///
/// Returns an error when the gate cannot run, or on a failed write.
pub fn run(root: &Path, out: &mut impl Write) -> Result<bool> {
    writeln!(
        out,
        "Checking that every guide page is listed in the guide index..."
    )?;
    let report = check(root).inspect_err(|_| eprintln!("{FIX_HINT}"))?;
    writeln!(out, "corpus: {} pages under {GUIDE}", report.pages)?;
    if report.missing_index {
        writeln!(out, "defects: {}", report.pages)?;
        eprintln!(
            "FAIL: {INDEX} does not exist, so none of the {} guide pages is listed in a \
             reader-facing index.\n{FIX_HINT}",
            report.pages
        );
        return Ok(false);
    }
    writeln!(
        out,
        "indexes: {} ({})",
        report.indexes.len(),
        report.indexes.join(", ")
    )?;
    writeln!(
        out,
        "entries: {} required, {} linked",
        report.required, report.linked
    )?;
    writeln!(out, "defects: {}", report.defects.len())?;
    if report.passed() {
        writeln!(out, "Guide index gate OK.")?;
    } else {
        writeln!(out)?;
        for (path, why) in &report.defects {
            writeln!(out, "  {path}: {why}")?;
        }
        eprintln!("{FIX_HINT}");
    }
    Ok(report.passed())
}

#[cfg(test)]
mod cases;

#[cfg(test)]
mod tests {
    use super::*;

    /// Run one captured self-test case: `tracked` is the corpus's tracked
    /// guide files, `files` the index and README contents.
    pub(super) fn case(passes: bool, tracked: &[&str], files: &[(&str, &str)]) {
        let pages: Vec<String> = tracked
            .iter()
            .filter(|p| Path::new(p).extension().is_some_and(|e| e == "md"))
            .map(|p| (*p).to_owned())
            .collect();
        let read = |rel: &str| {
            files
                .iter()
                .find(|(p, _)| *p == rel)
                .map(|(_, text)| (*text).to_owned())
                .with_context(|| format!("{rel} is not in this corpus"))
        };
        let got = check_corpus(&pages, &read).map(|r| {
            let ok = r.passed();
            (ok, r.defects)
        });
        match got {
            Ok((ok, defects)) => assert_eq!(ok, passes, "defects: {defects:#?}"),
            Err(e) => assert!(!passes, "the gate could not run: {e:#}"),
        }
    }

    #[test]
    fn this_repository_passes() {
        let report = check(crate::repo_root()).unwrap();
        assert!(report.passed(), "{:#?}", report.defects);
        assert!(report.pages > 100, "{report:?}");
        assert!(
            report
                .indexes
                .iter()
                .any(|i| i == "docs/guide/tutorial/index.md")
        );
    }

    #[test]
    fn the_report_names_the_counts() {
        let mut out = Vec::new();
        assert!(run(crate::repo_root(), &mut out).unwrap());
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("indexes: "), "{out}");
        assert!(out.ends_with("Guide index gate OK.\n"), "{out}");
    }

    #[test]
    fn ownership_goes_to_the_nearest_index() {
        let pages: Vec<String> = [
            "docs/guide/a.md",
            "docs/guide/index.md",
            "docs/guide/t/index.md",
            "docs/guide/t/one.md",
            "docs/guide/t/deep/index.md",
            "docs/guide/t/deep/x.md",
        ]
        .map(String::from)
        .to_vec();
        let plan = index_plan(&pages);
        let owned = |i: &str| plan[i].iter().cloned().collect::<Vec<_>>();
        assert_eq!(owned(INDEX), ["docs/guide/a.md", "docs/guide/t/index.md"]);
        assert_eq!(
            owned("docs/guide/t/index.md"),
            ["docs/guide/t/deep/index.md", "docs/guide/t/one.md"]
        );
        assert_eq!(
            owned("docs/guide/t/deep/index.md"),
            ["docs/guide/t/deep/x.md"]
        );
    }

    #[test]
    fn targets_resolve_like_the_link_gate() {
        let n = |t: &str, base: &str| normalise(t, base);
        assert_eq!(n("alpha.md", GUIDE).as_deref(), Some("docs/guide/alpha.md"));
        assert_eq!(
            n("docs/guide/alpha.md", GUIDE).as_deref(),
            Some("docs/guide/alpha.md")
        );
        assert_eq!(
            n("<my file.md>", GUIDE).as_deref(),
            Some("docs/guide/my file.md")
        );
        assert_eq!(
            n("alpha&#46;md#top", GUIDE).as_deref(),
            Some("docs/guide/alpha.md")
        );
        assert_eq!(
            n("alpha%2Emd?plain=1", GUIDE).as_deref(),
            Some("docs/guide/alpha.md")
        );
        assert_eq!(n("../../README.md", GUIDE), None);
        assert_eq!(n("../../../x.md", GUIDE), None, "past the root");
        assert_eq!(n("HTTPS://x/docs/guide/a.md", ""), None);
        assert_eq!(
            n("/docs/guide/index.md", ""),
            None,
            "root-relative leaves the repo"
        );
        assert_eq!(n("./docs/guide/index.md", "").as_deref(), Some(INDEX));
    }

    #[test]
    fn files_read_as_python_reads_them() {
        let dir = smop::fs::temp_dir().unwrap();
        std::fs::write(dir.path().join("a.md"), "x\r\ny\rz").unwrap();
        std::fs::write(dir.path().join("b.md"), b"\xff").unwrap();
        assert_eq!(read(dir.path(), "a.md").unwrap(), "x\ny\nz");
        assert!(read(dir.path(), "b.md").is_err());
    }

    #[test]
    fn an_empty_guide_is_an_error_not_a_pass() {
        let err = check_corpus(&[], &|_| Ok(String::new())).unwrap_err();
        assert!(err.to_string().contains("matched no markdown"), "{err}");
    }
}
