//! Actuator-path drift gate: every `/actuator/…` URL the reader-facing docs
//! tell someone to request must name an endpoint the framework mounts.
//!
//! The other docs gates cover the links, commands, `AUTUMN_*` variables,
//! `autumn.toml` keys and `autumn_web::…` paths a reader copies off a page.
//! This covers the URL they `curl`. A 404 from the actuator arrives against a
//! running app, and because most of the actuator sits behind
//! `[actuator] sensitive = true`, it reads like an endpoint the reader failed
//! to enable rather than one that was never there. The first run found four,
//! including a Spring→Autumn migration table that kept Spring's
//! `/actuator/scheduledtasks` where Autumn serves `/actuator/tasks`.
//!
//! # Truth set
//!
//! The suffix literals passed to `actuator_route_path(…)` at a `.route(…)`
//! mount, across tracked non-test Rust source. That helper is the one path
//! builder every actuator mount goes through. Its other callers (the
//! startup-barrier and route-listing inventories, an alert's `where_to_look`
//! pointer) are second copies of the list, not evidence that a URL answers,
//! so they are not read; nor is anything inside a `#[cfg(test)] mod`, where
//! paths are written to prove they are wrong.
//!
//! # Resolution
//!
//! Segment by segment, and permissive in three ways, each of which can only
//! report fewer paths:
//!
//! - a mounted `{param}` segment matches any documented segment;
//! - a documented `*` segment matches anything (`/actuator/webhooks/*`);
//! - a documented path that is a strict prefix of a mounted one resolves when
//!   the page names it (`/actuator/webhooks`, the bare `/actuator`), but not
//!   on a line that hands the reader a request (`curl`, `wget`, or an HTTP
//!   method followed by a path), where a prefix is a URL someone sends.
//!
//! A trailing slash means the subtree below, so it needs something mounted
//! there: `/actuator/` names the family, `/actuator/health/` is a 404.
//!
//! # Corpus
//!
//! Every markdown surface a reader can end up holding: `docs/guide/`,
//! `docs/migrations/`, `skills/`, `.claude/skills/`, all of `examples/` (the
//! wiki example serves its content pages), the root README, EXAMPLES,
//! CONTRIBUTING and STABILITY pages, `docs/plugins.md`, every `*.md.tmpl`
//! (`autumn new` writes one as each new project's README), and every page a
//! published crate's `Cargo.toml` names with `readme`. `scripts/check-docs-
//! scope.sh` compares this corpus with its siblings' through `--corpus`.
//!
//! Not checked: application routes, `/_autumn/…` (built from configurable
//! roots), a non-default actuator prefix, and whether an endpoint is exposed.
//!
//! # Waivers
//!
//! A page may name a path that does not exist here, as a comparison or a
//! troubleshooting row, with a marker below the passage:
//!
//! ```text
//! <!-- route-surface-allow: /actuator/scheduledtasks — Spring Boot's name;
//!      Autumn serves it at /actuator/tasks -->
//! ```
//!
//! A reason after `—` or `:` is required. A waiver covers its own
//! blank-line-separated block and the one above it, anchored at the line the
//! marker opens on. HTML comments are blanked before paths are read, so a
//! waiver never reports itself.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use smop::sh;

use crate::docs::re::{is_py_space, pattern, py_ascii_view, py_splitlines, py_strip};

const PREFIX: &str = "/actuator";

/// How to fix each kind of defect, printed to stderr on failure.
const FIX_HINT: &str = "
FAIL: the docs tell a reader to request actuator endpoints that are not
mounted (above).

The reader gets `404 Not Found` from a running app with no hint of the right
name — and because most of the actuator is behind `[actuator] sensitive =
true`, a 404 reads like an endpoint they failed to enable rather than one that
was never there.

Fix each one where it lives:
  - renamed endpoint  -> use the current path (the `did you mean` hint is the
                         closest mounted path)
  - never existed     -> drop the claim, or name the endpoint that does the job
  - another framework's name, shown for comparison -> waive it beside the
                         passage, with the Autumn path in the reason:

      <!-- route-surface-allow: /actuator/scheduledtasks — Spring Boot's name;
           Autumn serves it at /actuator/tasks -->

  - genuinely new endpoint -> land the mount first; this gate reads the
                         `actuator_route_path(…)` call sites, so it needs no
                         snapshot update

Inspect what the gate read:  cargo xtask docs routes --list";

// ── Truth set ───────────────────────────────────────────────────────────────

pattern! {
    /// `.route(&actuator_route_path(<prefix expr>, "<suffix>"), …)`: a mount,
    /// not a mention. The prefix argument is matched loosely, since it is
    /// spelled several ways; only the suffix literal is captured.
    static ROUTE_CALL = concat!(
        r"\.route\([\s\x1c-\x1f]*&[\s\x1c-\x1f]*(?:[A-Za-z0-9_]+::)*actuator_route_path\([\s\x1c-\x1f]*",
        r#"[^,;()]*(?:\([^()]*\))?[^,;()]*,[\s\x1c-\x1f]*"([^"]*)""#,
    );
}

pattern! {
    /// `#[cfg(test)] mod <name> {`, whose body is removed before the scan.
    static CFG_TEST_MOD = r"#\[cfg\(test\)\][\s\x1c-\x1f]*(?:pub[\s\x1c-\x1f]+)?mod[\s\x1c-\x1f]+[A-Za-z0-9_]+[\s\x1c-\x1f]*\{";
}

/// `src` without its brace-balanced `#[cfg(test)] mod …` bodies.
fn strip_test_mods(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut cursor = 0;
    for m in CFG_TEST_MOD.find_iter(src) {
        if m.start() < cursor {
            continue;
        }
        out.push_str(&src[cursor..m.start()]);
        let (mut i, mut depth) = (m.end(), 1usize);
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        cursor = i;
    }
    out.push_str(&src[cursor.min(src.len())..]);
    out
}

/// The suffixes of every actuator mount in `src`.
fn mount_suffixes(src: &str) -> Vec<String> {
    ROUTE_CALL
        .captures_iter(&strip_test_mods(src))
        .map(|c| c[1].to_owned())
        .collect()
}

/// Decode as Python's `errors="ignore"` does: invalid bytes are dropped.
fn decode_ignoring(raw: &[u8]) -> String {
    raw.utf8_chunks().map(|c| c.valid()).collect()
}

/// Read a tracked text file as Python's `read_text(errors="ignore")` does,
/// with `\r\n` and `\r` read as `\n`.
fn read_text(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(decode_ignoring(&raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

/// `git ls-files -z <pathspecs>` in `root`, in git's order.
fn git_ls_files(root: &Path, pathspecs: &[&str]) -> Result<Vec<String>> {
    let listed = sh::cmd("git")
        .args(["ls-files", "-z"])
        .args(pathspecs)
        .dir(root)
        .output()
        .with_context(|| format!("listing tracked files in {}", root.display()))?;
    Ok(listed
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Every `/actuator/…` path the framework mounts under the default prefix,
/// sorted.
///
/// # Errors
///
/// Returns an error when git cannot list the source, or when no mount is
/// found: a truth set that silently empties would let every documented path
/// pass.
pub fn mounted_paths(root: &Path) -> Result<Vec<String>> {
    let mut suffixes = BTreeSet::new();
    for rel in git_ls_files(root, &["*.rs"])? {
        let src = read_text(root, &rel)?;
        if !src.contains(".route(") || !src.contains("actuator_route_path(") {
            continue;
        }
        suffixes.extend(mount_suffixes(&src));
    }
    if suffixes.is_empty() {
        bail!(
            "no `.route(&actuator_route_path(…, \"…\"))` mounts found. The actuator router or \
             its path builder was refactored; this gate has no truth set to read and would pass \
             everything. Fix ROUTE_CALL in xtask/src/docs/routes.rs."
        );
    }
    Ok(suffixes
        .into_iter()
        .map(|s| format!("{PREFIX}{s}"))
        .collect())
}

// ── Corpus ──────────────────────────────────────────────────────────────────

/// Directories whose markdown is reader-facing.
const INCLUDE_DIRS: &[&str] = &[
    "docs/guide/",
    "docs/migrations/",
    "skills/",
    "agents/",
    // A second skill tree the agent machinery loads by name; `run-autumn`
    // drives a real server with `curl` from here.
    ".claude/skills/",
    // The wiki example compiles its content pages in and serves them.
    "examples/",
];

/// Single reader-facing files.
const INCLUDE_FILES: &[&str] = &[
    "README.md",
    "EXAMPLES.md",
    "CONTRIBUTING.md",
    "STABILITY.md",
    "docs/plugins.md",
];

fn in_scope(path: &str) -> bool {
    INCLUDE_DIRS.iter().any(|d| path.starts_with(d)) || INCLUDE_FILES.contains(&path)
}

/// Python's `os.path.normpath` for a `/`-separated relative or absolute path.
fn normpath(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(seg),
        }
    }
    let joined = parts.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// Python's `os.path.join(a, b)` for `/`-separated paths.
fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_owned()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// The parent of a `/`-separated path, as `PurePosixPath.parent` spells it.
fn parent(path: &str) -> &str {
    path.rsplit_once('/')
        .map_or(".", |(p, _)| if p.is_empty() { "/" } else { p })
}

/// A value that defers to `[workspace.package]`: `{ workspace = true }`.
fn inherited(value: Option<&toml::Value>) -> bool {
    value
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("workspace"))
        .and_then(toml::Value::as_bool)
        == Some(true)
}

/// Cargo's `publish`: absent means yes, `false` and `[]` mean no.
fn published(pkg: &toml::Table, workspace: &toml::Table) -> bool {
    let mut value = pkg.get("publish");
    if inherited(value) {
        value = workspace.get("publish");
    }
    match value {
        Some(toml::Value::Boolean(false)) => false,
        Some(toml::Value::Array(registries)) => !registries.is_empty(),
        _ => true,
    }
}

/// Parsed manifests, read once each.
struct Manifests<'a> {
    root: &'a Path,
    tracked: &'a HashSet<String>,
    cache: HashMap<String, toml::Table>,
}

impl Manifests<'_> {
    fn get(&mut self, rel: &str) -> Result<&toml::Table> {
        if !self.cache.contains_key(rel) {
            let path = self.root.join(rel);
            let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
            let table: toml::Table = raw.parse().with_context(|| format!("parsing {rel}"))?;
            self.cache.insert(rel.to_owned(), table);
        }
        Ok(&self.cache[rel])
    }

    fn has_workspace(&mut self, rel: &str) -> Result<bool> {
        Ok(self.tracked.contains(rel) && self.get(rel)?.contains_key("workspace"))
    }

    /// The manifest whose `[workspace.package]` `rel` inherits from: the one
    /// `package.workspace` names, else `rel` itself when it has a
    /// `[workspace]` table, else the nearest ancestor that does.
    fn workspace_of(&mut self, rel: &str) -> Result<Option<String>> {
        let named = self
            .get(rel)?
            .get("package")
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("workspace"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        if let Some(named) = named {
            let here = parent(rel).to_owned();
            for suffix in [named.clone(), join(&named, "Cargo.toml")] {
                let cand = normpath(&join(&here, &suffix));
                if self.has_workspace(&cand)? {
                    return Ok(Some(cand));
                }
            }
        }
        if self.get(rel)?.contains_key("workspace") {
            return Ok(Some(rel.to_owned()));
        }
        let mut parts: Vec<&str> = rel.split('/').collect();
        parts.pop();
        while parts.pop().is_some() {
            let mut cand = parts.join("/");
            cand = if cand.is_empty() {
                "Cargo.toml".to_owned()
            } else {
                format!("{cand}/Cargo.toml")
            };
            if self.has_workspace(&cand)? {
                return Ok(Some(cand));
            }
        }
        Ok(None)
    }
}

/// Conventional README names Cargo discovers beside a manifest.
const README_CANDIDATES: &[&str] = &["README.md", "README.txt", "README"];

/// Every file a published crate's `Cargo.toml` names as its README.
///
/// Parsed as TOML, with Cargo's rules: `readme = false` means none; a string
/// is relative to the manifest; `workspace = true` takes the
/// `[workspace.package]` value of the package's own workspace, relative to
/// that workspace's root; an absent key discovers `README.md`, `README.txt`
/// or `README` beside the manifest. A package that does not publish is
/// skipped.
fn package_readmes(root: &Path, tracked: &HashSet<String>) -> Result<BTreeSet<String>> {
    let mut manifests: Vec<&String> = tracked
        .iter()
        .filter(|f| *f == "Cargo.toml" || f.ends_with("/Cargo.toml"))
        .collect();
    manifests.sort();
    let mut m = Manifests {
        root,
        tracked,
        cache: HashMap::new(),
    };
    let mut out = BTreeSet::new();
    for rel in manifests {
        let dir = parent(rel);
        let prefix = if dir == "." {
            String::new()
        } else {
            format!("{dir}/")
        };
        let Some(pkg) = m
            .get(rel)?
            .get("package")
            .and_then(toml::Value::as_table)
            .cloned()
        else {
            continue;
        };
        let (workspace, ws_dir) = match m.workspace_of(rel)? {
            Some(ws) => {
                let table = m
                    .get(&ws)?
                    .get("workspace")
                    .and_then(toml::Value::as_table)
                    .and_then(|w| w.get("package"))
                    .and_then(toml::Value::as_table)
                    .cloned()
                    .unwrap_or_default();
                let d = parent(&ws);
                (
                    table,
                    if d == "." {
                        String::new()
                    } else {
                        d.to_owned()
                    },
                )
            }
            None => (toml::Table::new(), String::new()),
        };
        if !published(&pkg, &workspace) {
            continue;
        }
        let named = pkg.get("readme");
        if inherited(named) {
            if let Some(path) = workspace.get("readme").and_then(toml::Value::as_str) {
                out.insert(normpath(&join(&ws_dir, path)));
            }
            continue;
        }
        match named {
            Some(toml::Value::Boolean(false)) => {}
            Some(toml::Value::String(path)) => {
                out.insert(normpath(&join(dir, path)));
            }
            _ => {
                if let Some(found) = README_CANDIDATES
                    .iter()
                    .map(|c| format!("{prefix}{c}"))
                    .find(|c| tracked.contains(c))
                {
                    out.insert(found);
                }
            }
        }
    }
    Ok(out)
}

/// The reader-facing pages, in git's order, then any published landing page
/// the markdown glob did not produce.
///
/// # Errors
///
/// Returns an error when git cannot list the tree or a manifest does not
/// parse.
pub fn corpus(root: &Path) -> Result<Vec<String>> {
    let tracked: HashSet<String> = git_ls_files(root, &[])?.into_iter().collect();
    let published = package_readmes(root, &tracked)?;
    let files: Vec<String> = git_ls_files(root, &["*.md", "*.md.tmpl"])?
        .into_iter()
        .filter(|f| in_scope(f) || f.ends_with(".md.tmpl") || published.contains(f))
        .collect();
    let seen: HashSet<&String> = files.iter().collect();
    let extra: Vec<String> = published
        .iter()
        .filter(|p| tracked.contains(*p) && !seen.contains(p))
        .cloned()
        .collect();
    Ok(files.into_iter().chain(extra).collect())
}

// ── Extraction ──────────────────────────────────────────────────────────────

pattern! {
    /// A documented path: `/actuator` and the longest run of RFC 3986 path
    /// characters (`pchar`, `/`, and the `{}` of a route parameter) after it.
    /// Stopping early would hand resolution a shorter path that resolves.
    static DOC_PATH = r"/actuator[A-Za-z0-9\-._~%!$&'()*+,;=:@{}/]*";
}

/// Sentence punctuation dropped from the end of a path. `/` is not here: a
/// trailing slash is significant.
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', ')', '"', '\'', '`', ']', '}', '>',
];

pattern! {
    /// A line that hands the reader a request: a client command, or an HTTP
    /// method followed by a path (optionally absolute). Matched on
    /// [`py_ascii_view`], so `\b` and `\s` mean what they mean in Python.
    static REQUEST_LINE = concat!(
        r"\b(?:curl|wget|xh|httpie)\b",
        r"|\b(?:GET|HEAD|POST|PUT|DELETE|CONNECT|OPTIONS|TRACE|PATCH)",
        r"[\s\x1c-\x1f]+(?:https?://[^\s\x1c-\x1f/]+)?/",
    );
}

pattern! {
    /// `<!-- route-surface-allow: /actuator/x — reason -->`, which may wrap.
    static WAIVER = concat!(
        r"(?s)<!--[\s\x1c-\x1f]*route-surface-allow:[\s\x1c-\x1f]*(/actuator[^\s\x1c-\x1f—:]*)",
        r"[\s\x1c-\x1f]*(?:—|:)[\s\x1c-\x1f]*([^\s\x1c-\x1f][^>]*?)-->",
    );
}

pattern! { static HTML_COMMENT = r"(?s)<!--.*?-->"; }

/// `text` with every HTML comment blanked, newlines kept: a comment renders
/// as nothing, so it is not a path a reader can see.
fn blank_comments(text: &str) -> String {
    HTML_COMMENT
        .replace_all(text, |c: &regex::Captures<'_>| {
            c[0].chars()
                .map(|ch| if ch == '\n' { '\n' } else { ' ' })
                .collect::<String>()
        })
        .into_owned()
}

/// Each waived path mapped to the line numbers its waivers cover: the
/// waiver's own blank-line-separated block and the one above it.
fn waived_lines(text: &str) -> HashMap<String, HashSet<usize>> {
    let lines = py_splitlines(text);
    let mut block_of = Vec::with_capacity(lines.len());
    let (mut block, mut prev_blank) = (0usize, true);
    for line in &lines {
        if py_strip(line).is_empty() {
            prev_blank = true;
            block_of.push(block);
            continue;
        }
        if prev_blank {
            block += 1;
        }
        prev_blank = false;
        block_of.push(block);
    }
    let mut covered: HashMap<String, HashSet<usize>> = HashMap::new();
    for c in WAIVER.captures_iter(text) {
        let start = c.get(0).map_or(0, |m| m.start());
        let lineno = text[..start].matches('\n').count() + 1;
        let Some(&here) = block_of.get(lineno - 1) else {
            continue;
        };
        let hits = covered.entry(c[1].to_owned()).or_default();
        hits.extend(
            block_of
                .iter()
                .enumerate()
                .filter(|&(_, &b)| b == here || b + 1 == here)
                .map(|(n, _)| n + 1),
        );
    }
    covered
}

/// `(first line number, text)` for each line, with backslash continuations
/// folded into the line the command starts on.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut held: Vec<String> = Vec::new();
    let mut held_at = None;
    for (n, line) in py_splitlines(text).into_iter().enumerate() {
        let lineno = n + 1;
        let stripped = line.trim_end_matches(is_py_space);
        if let Some(body) = stripped.strip_suffix('\\') {
            held_at.get_or_insert(lineno);
            held.push(body.to_owned());
            continue;
        }
        if let Some(at) = held_at.take() {
            held.push(line.to_owned());
            out.push((at, held.join(" ")));
            held.clear();
        } else {
            out.push((lineno, line.to_owned()));
        }
    }
    if let Some(at) = held_at {
        out.push((at, held.join(" ")));
    }
    out
}

/// One documented path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Documented {
    line: usize,
    path: String,
    requested: bool,
}

/// Every `/actuator/…` path a page shows, with whether its line is a
/// request. A malformed spelling is kept whole so resolution rejects it;
/// `/actuator-…` is a different name and is skipped.
fn documented(text: &str) -> Vec<Documented> {
    let mut out = Vec::new();
    for (line_no, line) in logical_lines(&blank_comments(text)) {
        let requested = REQUEST_LINE.is_match(&py_ascii_view(&line, false));
        for m in DOC_PATH.find_iter(&line) {
            let path = m.as_str().trim_end_matches(TRAILING);
            if path.is_empty() || path[PREFIX.len().min(path.len())..].starts_with('-') {
                continue;
            }
            out.push(Documented {
                line: line_no,
                path: path.to_owned(),
                requested,
            });
        }
    }
    out
}

// ── Resolution ──────────────────────────────────────────────────────────────

/// Whether `path` names something the framework mounts. See the module
/// docs for the three permissive rules and the trailing slash.
fn resolves(path: &str, mounted: &[String], requested: bool) -> bool {
    let subtree = path.ends_with('/');
    let segs: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    mounted.iter().any(|target| {
        let tsegs: Vec<&str> = target
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if segs.len() > tsegs.len() || (subtree && segs.len() == tsegs.len()) {
            return false;
        }
        if requested && segs.len() < tsegs.len() && !segs.contains(&"*") {
            return false;
        }
        segs.iter()
            .zip(&tsegs)
            .all(|(s, t)| t.starts_with('{') || *s == "*" || s == t)
    })
}

/// One block of [`ratio`]'s search: `seq1[lo1..hi1]` against `seq2[lo2..hi2]`.
#[derive(Clone, Copy)]
struct Span {
    lo1: usize,
    hi1: usize,
    lo2: usize,
    hi2: usize,
}

/// Python's `difflib.SequenceMatcher(None, seq1, seq2).ratio()` over bytes:
/// the same greedy longest-match blocks, including `autojunk`, which drops an
/// element of `seq2` occurring in more than 1% of a `seq2` of 200 or more.
fn ratio(seq1: &[u8], seq2: &[u8]) -> f64 {
    let mut where2: HashMap<u8, Vec<usize>> = HashMap::new();
    for (at, &byte) in seq2.iter().enumerate() {
        where2.entry(byte).or_default().push(at);
    }
    if seq2.len() >= 200 {
        let ntest = seq2.len() / 100 + 1;
        where2.retain(|_, positions| positions.len() <= ntest);
    }
    let same = |at1: usize, at2: usize| seq1[at1] == seq2[at2];
    // `find_longest_match`: the longest common run, earliest in `seq1`, then
    // earliest in `seq2`, then extended while the neighbours agree.
    let longest = |span: Span| {
        let (mut start1, mut start2, mut size) = (span.lo1, span.lo2, 0usize);
        let mut run_ending: HashMap<usize, usize> = HashMap::new();
        for (at1, byte) in seq1.iter().enumerate().take(span.hi1).skip(span.lo1) {
            let mut next: HashMap<usize, usize> = HashMap::new();
            for &at2 in where2.get(byte).map_or(&[][..], Vec::as_slice) {
                if at2 < span.lo2 {
                    continue;
                }
                if at2 >= span.hi2 {
                    break;
                }
                let run = at2
                    .checked_sub(1)
                    .and_then(|prev| run_ending.get(&prev))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                next.insert(at2, run);
                if run > size {
                    (start1, start2, size) = (at1 + 1 - run, at2 + 1 - run, run);
                }
            }
            run_ending = next;
        }
        while start1 > span.lo1 && start2 > span.lo2 && same(start1 - 1, start2 - 1) {
            start1 -= 1;
            start2 -= 1;
            size += 1;
        }
        while start1 + size < span.hi1
            && start2 + size < span.hi2
            && same(start1 + size, start2 + size)
        {
            size += 1;
        }
        (start1, start2, size)
    };
    let mut matched = 0usize;
    let mut queue = vec![Span {
        lo1: 0,
        hi1: seq1.len(),
        lo2: 0,
        hi2: seq2.len(),
    }];
    while let Some(span) = queue.pop() {
        let (start1, start2, size) = longest(span);
        if size > 0 {
            matched += size;
            if span.lo1 < start1 && span.lo2 < start2 {
                queue.push(Span {
                    hi1: start1,
                    hi2: start2,
                    ..span
                });
            }
            if start1 + size < span.hi1 && start2 + size < span.hi2 {
                queue.push(Span {
                    lo1: start1 + size,
                    lo2: start2 + size,
                    ..span
                });
            }
        }
    }
    let total = seq1.len() + seq2.len();
    if total == 0 {
        1.0
    } else {
        #[allow(clippy::cast_precision_loss, reason = "paths are short")]
        let value = 2.0 * matched as f64 / total as f64;
        value
    }
}

/// The mounted path most like `path`, for the `did you mean` hint: Python's
/// `difflib.get_close_matches(path, mounted, n=1, cutoff=0.0)`, which keeps
/// the highest ratio and, on a tie, the greatest path.
fn nearest<'m>(path: &str, mounted: &'m [String]) -> Option<&'m str> {
    mounted
        .iter()
        .map(|m| (ratio(m.as_bytes(), path.as_bytes()), m.as_str()))
        .max_by(|x, y| x.0.total_cmp(&y.0).then_with(|| x.1.cmp(y.1)))
        .map(|(_, m)| m)
}

// ── Gate ────────────────────────────────────────────────────────────────────

/// One undocumented path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defect {
    pub page: String,
    pub line: usize,
    pub path: String,
}

/// The outcome of one run.
#[derive(Debug, Default)]
pub struct Report {
    pub corpus: usize,
    pub mounted: Vec<String>,
    pub checked: usize,
    pub waived: usize,
    pub defects: Vec<Defect>,
}

/// Check every page in `pages`, read through `read`, against `mounted`.
fn check_pages(
    pages: &[String],
    mounted: Vec<String>,
    read: &dyn Fn(&str) -> Result<String>,
) -> Result<Report> {
    let mut report = Report {
        corpus: pages.len(),
        mounted,
        ..Report::default()
    };
    for page in pages {
        let text = read(page)?;
        if !text.contains(PREFIX) {
            continue;
        }
        let waivers = waived_lines(&text);
        for d in documented(&text) {
            report.checked += 1;
            if resolves(&d.path, &report.mounted, d.requested) {
                continue;
            }
            if waivers
                .get(&d.path)
                .is_some_and(|lines| lines.contains(&d.line))
            {
                report.waived += 1;
                continue;
            }
            report.defects.push(Defect {
                page: page.clone(),
                line: d.line,
                path: d.path,
            });
        }
    }
    Ok(report)
}

/// Run the gate over the checkout at `root`.
///
/// # Errors
///
/// Returns an error when git cannot list the tree, a manifest does not
/// parse, a page cannot be read, or no mount is found.
pub fn check(root: &Path) -> Result<Report> {
    let mounted = mounted_paths(root)?;
    check_pages(&corpus(root)?, mounted, &|rel| read_text(root, rel))
}

/// Run the gate and print its report. The fix hint goes to stderr.
///
/// # Errors
///
/// Returns an error when the gate cannot run, or on a failed write.
pub fn run(root: &Path, out: &mut impl Write) -> Result<bool> {
    writeln!(
        out,
        "Checking actuator paths across the reader-facing docs..."
    )?;
    let report = check(root).inspect_err(|_| eprintln!("{FIX_HINT}"))?;
    writeln!(
        out,
        "corpus: {} reader-facing markdown files",
        report.corpus
    )?;
    writeln!(
        out,
        "surface: {} mounted actuator paths",
        report.mounted.len()
    )?;
    writeln!(out, "checked: {} `/actuator/…` occurrences", report.checked)?;
    for d in &report.defects {
        let hint = nearest(&d.path, &report.mounted)
            .map(|h| format!("  (did you mean `{h}`?)"))
            .unwrap_or_default();
        writeln!(
            out,
            "{}:{}: `{}` is not mounted{hint}",
            d.page, d.line, d.path
        )?;
    }
    let waived = if report.waived > 0 {
        format!(" ({} waived)", report.waived)
    } else {
        String::new()
    };
    writeln!(out, "\ndefects: {}{waived}", report.defects.len())?;
    if report.defects.is_empty() {
        writeln!(out, "Actuator path gate OK.")?;
        Ok(true)
    } else {
        eprintln!("{FIX_HINT}");
        Ok(false)
    }
}

/// The mounted surface, one path per line (`--list`).
///
/// # Errors
///
/// As [`mounted_paths`].
pub fn surface(root: &Path) -> Result<Vec<String>> {
    mounted_paths(root)
}

/// The corpus, sorted, one path per line (`--corpus`), for
/// `scripts/check-docs-scope.sh` to compare with the sibling gates'.
///
/// # Errors
///
/// As [`corpus`].
pub fn sorted_corpus(root: &Path) -> Result<Vec<String>> {
    let mut pages = corpus(root)?;
    pages.sort();
    Ok(pages)
}

#[cfg(test)]
mod tests;
