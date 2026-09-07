//! `autumn upgrade` — bring an app up to a release: its own code, and its
//! framework-owned project files (issues #1629 and #1593).
//!
//! Autumn ships every 2-4 weeks and, pre-1.0, most releases can break existing
//! apps. Issue #1588 made a written migration guide a release gate, but prose
//! left both halves of the actual work by hand: a purely mechanical rename like
//! 0.6.0's `with_pool` -> `with_pool_untracked` had to be applied call site by
//! call site, and the project skeleton `autumn new` wrote — `Dockerfile`,
//! `build.rs`, `autumn.toml`, the toolchain configs — stayed frozen at whatever
//! release scaffolded it, because bumping `autumn-web` updates the library, not
//! the project.
//!
//! This command closes both. For each release between the app's recorded
//! `autumn-web` version and the target, it applies that release's
//! machine-applyable migrations (see [`migrations`]) to the app's own source,
//! and reports everything it could not safely rewrite with `file:line` and a
//! guide link rather than guessing. In the same run it reconciles the
//! framework-owned project files against the current release's scaffold (see
//! [`scaffold`]), which never touches `src/` and never overwrites a file the
//! developer edited.
//!
//! # Safety posture
//!
//! - **Preview by default.** A bare `autumn upgrade` writes nothing: it prints
//!   a per-file diff and a count of affected sites. `--apply` is the explicit
//!   write step.
//! - **Plan first, then write.** Every file is read, parsed, and rewritten in
//!   memory before a single byte is written, so a parse failure halfway through
//!   cannot leave the tree half-migrated.
//! - **Idempotent.** Rewrites match whole tokens, so a second run finds
//!   nothing; an app that never used the affected APIs reports nothing to
//!   change.
//! - **Nothing silent.** A site the engine declines to rewrite (inside a macro
//!   invocation or an attribute) is listed under `manual` with its location.
//!
//! The same posture governs the scaffold half, one step further: a file whose
//! bytes no longer match the digest [`crate::new`] recorded when it wrote them
//! is a *conflict*, reported with its diff and never written.

pub mod diff;
pub mod engine;
pub mod migrations;
pub mod scaffold;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use migrations::{AppMigration, Version};

/// Options for `autumn upgrade`.
///
/// One bool per flag, deliberately: they are independent switches the CLI layer
/// passes straight through, and grouping them into modes would put the flag
/// combinations' meaning somewhere other than where they are validated.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct UpgradeOptions {
    /// Override the detected `autumn-web` version the app is upgrading *from*.
    pub from: Option<String>,
    /// Override the version being upgraded *to* (defaults to this CLI's own
    /// version).
    pub to: Option<String>,
    /// Write the rewrites. Without it the command is preview-only.
    pub apply: bool,
    /// Emit the machine-readable report instead of the human one.
    pub json: bool,
    /// List the registry and exit, without scanning any source.
    pub list: bool,
    /// Report scaffold-file drift and exit nonzero if there is any, without
    /// scanning app code and without writing. The CI gate for scaffold
    /// freshness (issue #1593).
    pub check: bool,
    /// Framework-owned paths to record as the developer's own, so
    /// reconciliation leaves them alone from now on. Writes only the manifest.
    pub accept: Vec<String>,
}

/// A site the user has to handle themselves, with where to read about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualEntry {
    /// Repo-relative file, or `None` for a whole-change (guide-only) entry.
    pub path: Option<String>,
    /// 1-based line, or `None` for a whole-change entry.
    pub line: Option<usize>,
    /// [`AppMigration::id`].
    pub migration: &'static str,
    /// Why it was not rewritten.
    pub reason: String,
    /// Guide section to read.
    pub guide: &'static str,
}

/// One file's planned or applied rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    /// Path as displayed, relative to the scan root.
    pub path: String,
    /// Rewritten sites in this file.
    pub sites: Vec<engine::Site>,
    /// Rendered preview diff.
    pub diff: String,
    /// The full rewritten text (not serialized; used by the apply step).
    pub updated: String,
    /// The text this rewrite was computed from, kept so the apply step can
    /// prove the file has not changed underneath it.
    pub original: String,
    /// Absolute path to write to.
    pub absolute: PathBuf,
}

/// What became of the planned rewrites.
///
/// A partial apply is its own state rather than a `false`: reporting "nothing
/// was written" after some files were already rewritten is the one message
/// that could send someone looking in the wrong place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Preview only — nothing was written.
    Preview,
    /// Every planned file was written.
    Applied,
    /// The apply step failed partway. This many files were already written.
    Partial { written: usize },
}

impl Outcome {
    /// The label used in `--json`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Applied => "applied",
            Self::Partial { .. } => "partial",
        }
    }
}

/// Everything one `autumn upgrade` run found.
#[derive(Debug, Clone)]
pub struct Report {
    pub from: Version,
    pub to: Version,
    /// What the apply step actually did.
    pub outcome: Outcome,
    /// `.rs` files read.
    pub files_scanned: usize,
    /// Migrations selected for this version range.
    pub migrations: Vec<&'static AppMigration>,
    /// Files with at least one rewrite.
    pub files: Vec<FileReport>,
    /// Sites belonging to a `review`-confidence migration, listed individually.
    pub review: Vec<ManualEntry>,
    /// Sites and changes left for a human.
    pub manual: Vec<ManualEntry>,
    /// Files that could not be read or parsed, with the reason.
    pub skipped: Vec<(String, String)>,
}

impl Report {
    /// Total rewritten sites across every file in the plan.
    pub fn rewritten_sites(&self) -> usize {
        self.files.iter().map(|f| f.sites.len()).sum()
    }

    /// Sites in the files that actually reached disk.
    ///
    /// The same as [`Self::rewritten_sites`] for a complete apply and zero for
    /// a preview; for a partial apply it counts only the prefix of files
    /// written before the failure. Reported separately because they are
    /// different questions — "what did this run plan" and "what is on disk
    /// now" — and automation that gates on the wrong one treats an interrupted
    /// run as a finished one.
    pub fn written_sites(&self) -> usize {
        match self.outcome {
            Outcome::Preview => 0,
            Outcome::Applied => self.rewritten_sites(),
            Outcome::Partial { written } => self
                .files
                .iter()
                .take(written)
                .map(|file| file.sites.len())
                .sum(),
        }
    }
}

/// Directory names never scanned: build output and vendored third-party
/// sources are not the app's own code, and rewriting them is at best wasted
/// work and at worst a corrupted dependency.
/// Skipped only where a crate begins — a directory holding a `Cargo.toml`.
/// Beneath that, these are ordinary module names.
/// Exit code for `--check` when a project's framework-owned files have drifted
/// from the current release's scaffold (issue #1593).
///
/// Its own code rather than the generic `1`: within this command one exit code
/// means one thing, and `1` already means "the apply step died partway", which
/// is a materially different thing for a CI job to react to.
pub const DRIFT_EXIT_CODE: i32 = 3;

const SKIPPED_DIRS: &[&str] = &["target", "vendor", "node_modules", "dist", "tmp"];

/// Hidden directories that hold tool or VCS metadata rather than app code.
///
/// Skipping *every* dot-directory dropped source an app really compiles —
/// `#[path = ".generated/repositories.rs"] mod repositories;` is legal, and the
/// files behind it kept the old API with nothing in the report to say so. These
/// names are skipped by name instead, and any other hidden directory is
/// scanned like a normal one.
const SKIPPED_HIDDEN_DIRS: &[&str] = &[
    ".git", ".github", ".gitlab", ".hg", ".svn", ".cargo", ".vscode", ".idea", ".direnv", ".venv",
    ".tox", ".claude",
];

/// Render the human-readable report.
pub fn render_text(report: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "autumn upgrade - app-code migrations {} -> {}",
        report.from, report.to
    );

    if report.migrations.is_empty() {
        let _ = writeln!(
            out,
            "\nNothing to change: no shipped migration falls in this version range."
        );
        if report.from >= report.to {
            // Overwhelmingly the reason someone sees an empty range: the
            // dependency was bumped before running the codemods, so the app
            // now records the version it is being upgraded *to*.
            let _ = writeln!(
                out,
                "This app already records `autumn-web {}`. If you bumped the dependency\n\
                 before running the codemods, pass the release you came from:\n\
                 `autumn upgrade --from <previous-version>`.",
                report.from
            );
        }
        return out;
    }

    render_migrations(&mut out, report);
    render_diffs(&mut out, report);
    render_entries(
        &mut out,
        "Review - rewritten, but read each of these before committing",
        &report.review,
    );
    render_entries(
        &mut out,
        "Manual - not rewritten; read the guide section",
        &report.manual,
    );
    render_skipped(&mut out, report);
    render_summary(&mut out, report);
    out
}

/// The migrations selected for this version range, each with its confidence
/// label and the guide section it points at.
fn render_migrations(out: &mut String, report: &Report) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "\nMigrations in range ({}):", report.migrations.len());
    for migration in &report.migrations {
        let _ = writeln!(
            out,
            "  {:<6}  {}  {}",
            migration.confidence.label(),
            migration.id,
            migration.title
        );
        let _ = writeln!(out, "          {}", migrations::guide_url(migration.guide));
    }
}

/// The per-file diff — the product of the default, preview-only invocation.
fn render_diffs(out: &mut String, report: &Report) {
    use std::fmt::Write as _;
    if report.files.is_empty() {
        // Nothing to *rewrite*. Say which kind of nothing it is: a site only a
        // human can take is not the same answer as a clean tree, and telling
        // someone to re-run with `--apply` when `--apply` would write nothing
        // is the worst of the three.
        let human_sites = report
            .manual
            .iter()
            .filter(|entry| entry.path.is_some())
            .count();
        if human_sites > 0 {
            let _ = writeln!(
                out,
                "\nNothing to rewrite: {human_sites} site{} need{} a human — see Manual below.",
                plural(human_sites),
                if human_sites == 1 { "s" } else { "" }
            );
        } else {
            let _ = writeln!(
                out,
                "\nNothing to change: no affected call site found in {} scanned file(s).",
                report.files_scanned
            );
        }
        return;
    }
    let _ = writeln!(
        out,
        "\n{}:",
        match report.outcome {
            Outcome::Applied => "Applied",
            Outcome::Partial { .. } => "Applied in part — see the error below",
            Outcome::Preview => "Preview (nothing is written without --apply)",
        }
    );
    for file in &report.files {
        let _ = writeln!(
            out,
            "\n{} ({} site{})",
            file.path,
            file.sites.len(),
            plural(file.sites.len())
        );
        out.push_str(&file.diff);
    }
}

/// A `review` or `manual` list: one line per entry, one line per guide link.
fn render_entries(out: &mut String, heading: &str, entries: &[ManualEntry]) {
    use std::fmt::Write as _;
    if entries.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n{heading}:");
    for entry in entries {
        let _ = writeln!(
            out,
            "  {}  {} ({})",
            entry.location(),
            entry.migration,
            entry.reason
        );
        let _ = writeln!(out, "      {}", migrations::guide_url(entry.guide));
    }
}

/// Files that could not be read or parsed. They were left exactly as they were.
fn render_skipped(out: &mut String, report: &Report) {
    use std::fmt::Write as _;
    if report.skipped.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nSkipped - left untouched:");
    for (path, reason) in &report.skipped {
        let _ = writeln!(out, "  {path}: {reason}");
    }
}

/// The closing counts, and — in preview — how to actually take the changes.
fn render_summary(out: &mut String, report: &Report) {
    use std::fmt::Write as _;
    let sites = report.rewritten_sites();
    if sites == 0 {
        return;
    }
    let files = report.files.len();

    match report.outcome {
        // Whole-plan totals would claim every site landed. Only the files
        // before the failure did, so the two halves are counted separately —
        // saying "N sites rewritten" and "3 files written" in the same summary
        // is worse than either number alone.
        Outcome::Partial { written } => {
            let written_sites = report.written_sites();
            let _ = writeln!(
                out,
                "\n{written_sites} site{} in {written} file{} written before the run stopped; \
                 {} site{} in {} file{} planned and not written.",
                plural(written_sites),
                plural(written),
                sites - written_sites,
                plural(sites - written_sites),
                files - written,
                plural(files - written),
            );
            let _ = writeln!(
                out,
                "`git diff` shows exactly what landed; the error below names the file that stopped it."
            );
        }
        Outcome::Applied => {
            let _ = writeln!(
                out,
                "\n{sites} site{} in {files} file{} rewritten; {} file(s) scanned.",
                plural(sites),
                plural(files),
                report.files_scanned
            );
            let _ = writeln!(out, "Review the result with `git diff` before committing.");
        }
        Outcome::Preview => {
            let _ = writeln!(
                out,
                "\n{sites} site{} in {files} file{} would be rewritten; {} file(s) scanned.",
                plural(sites),
                plural(files),
                report.files_scanned
            );
            let _ = writeln!(
                out,
                "Nothing was written. Re-run with `--apply` to write these changes."
            );
        }
    }
}

/// `""` or `"s"`.
const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

impl ManualEntry {
    /// `path:line`, or the migration's own name for a whole-change entry.
    fn location(&self) -> String {
        match (&self.path, self.line) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.clone(),
            _ => "(whole change)".to_owned(),
        }
    }
}

/// The registry as JSON, shared by the report and `--list-migrations --json`.
fn migrations_json(migrations: &[&'static AppMigration]) -> serde_json::Value {
    serde_json::Value::Array(
        migrations
            .iter()
            .map(|migration| {
                serde_json::json!({
                    "id": migration.id,
                    "version": migration.version,
                    "title": migration.title,
                    "confidence": migration.confidence.label(),
                    "guide": migration.guide,
                    "guide_url": migrations::guide_url(migration.guide),
                    "rewrites_code": !matches!(migration.rewrite, migrations::Rewrite::GuideOnly),
                })
            })
            .collect(),
    )
}

fn manual_json(entries: &[ManualEntry]) -> serde_json::Value {
    serde_json::Value::Array(
        entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "path": entry.path,
                    "line": entry.line,
                    "migration": entry.migration,
                    "reason": entry.reason,
                    "guide": entry.guide,
                    "guide_url": migrations::guide_url(entry.guide),
                })
            })
            .collect(),
    )
}

/// Render the machine-readable report.
///
/// The scaffold section (issue #1593) is merged in by the caller, which is why
/// [`json_value`] exists separately: `autumn upgrade --json` emits one document,
/// and building it by re-parsing this function's own output would be a parser
/// round trip over data the caller already has.
#[must_use]
pub fn render_json(report: &Report) -> String {
    serde_json::to_string_pretty(&json_value(report)).unwrap_or_else(|_| "{}".to_owned())
}

/// The machine-readable report as a value, so a caller can extend it (the
/// scaffold section) without re-parsing its own output.
#[must_use]
pub fn json_value(report: &Report) -> serde_json::Value {
    let files: Vec<serde_json::Value> = report
        .files
        .iter()
        .map(|file| {
            serde_json::json!({
                "path": file.path,
                "diff": file.diff,
                "sites": file.sites.iter().map(|site| serde_json::json!({
                    "line": site.line,
                    "column": site.column,
                    "migration": site.migration,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let value = serde_json::json!({
        "from": report.from.to_string(),
        "to": report.to.to_string(),
        "outcome": report.outcome.label(),
        // Kept as a bool for anything already reading it; true only for a
        // *complete* apply, with `outcome` carrying the partial case.
        "applied": report.outcome == Outcome::Applied,
        "files_written": match report.outcome {
            Outcome::Partial { written } => written,
            Outcome::Applied => report.files.len(),
            Outcome::Preview => 0,
        },
        "files_scanned": report.files_scanned,
        // The plan, and the part of it that reached disk. Equal for a complete
        // apply; the second is what to gate on when the question is what the
        // working tree now contains.
        "rewritten_sites": report.rewritten_sites(),
        "written_sites": report.written_sites(),
        "migrations": migrations_json(&report.migrations),
        "files": files,
        "review": manual_json(&report.review),
        "manual": manual_json(&report.manual),
        "skipped": report.skipped.iter().map(|(path, reason)| serde_json::json!({
            "path": path,
            "reason": reason,
        })).collect::<Vec<_>>(),
    });
    value
}

/// Read the `autumn-web` requirement recorded by the app at `root`.
///
/// The root manifest (`[dependencies]`, then `[workspace.dependencies]`) and
/// every member manifest are read together. A *virtual* workspace root
/// declares neither, so reading only the root would abort a perfectly ordinary
/// layout with "cannot tell which version"; and a root that *does* declare one
/// can still be paired with a member that pins something older.
///
/// When they disagree, the **oldest** floor wins. That is the conservative
/// answer: a migration for a release a member is already past finds nothing to
/// do in that member (the rename it applies has already been applied), while
/// taking the newest floor would skip a member that is genuinely behind.
fn recorded_version(root: &Path) -> Option<String> {
    // The root and the members are read *together*. Returning early on the
    // root would let a workspace whose root records 0.6.0 hide a member that
    // still pins 0.5.0 — the walk then migrates that member's source with no
    // 0.5.0 -> 0.6.0 migration selected.
    use crate::doctor::AutumnWebDependency;

    // `{ workspace = true }` resolves against the enclosing workspace, and Cargo
    // finds that manifest by walking *up from the crate that inherits*. Not
    // doing so aborted a member with "cannot tell which version" over a version
    // Cargo resolves fine.
    //
    // Up from the crate, not up from the scan root: a whole workspace can sit
    // inside the scanned tree, and then the entry its members inherit lives
    // between them and the root. Searching the root's ancestors misses it, and
    // an unresolved literal `autumn-web` reads as a declaration with no version
    // — which fails detection for the entire run, not just that member.
    //
    // By the member's own key, not by the crate name: `autumn = { workspace =
    // true }` paired with a renamed workspace entry is the shape where nothing
    // on the member side mentions `autumn-web` at all.
    fn inherited(from: &Path, key: &str) -> Option<AutumnWebDependency> {
        // Canonicalised first: the scan root is usually the relative `.`, and
        // `Path::new(".").ancestors()` yields only `.` — the walk upward would
        // never leave the directory it started in.
        let absolute = std::fs::canonicalize(from).unwrap_or_else(|_| from.to_path_buf());
        absolute
            .ancestors()
            .find_map(|ancestor| crate::doctor::workspace_dependency_for(ancestor, key))
    }

    let mut lowest: Option<(Version, String)> = None;
    for directory in std::iter::once(root.to_path_buf()).chain(member_manifests(root)) {
        // *Every* declaration in the manifest, not the first: a target-specific
        // requirement can be older than the package-wide one, and the source
        // scan rewrites that target's `#[cfg]` code either way.
        for declaration in crate::doctor::autumn_web_declarations_at(&directory) {
            let declaration = match declaration {
                // Substitute the workspace entry the member inherits.
                //
                // Unresolved splits two ways. A literal `autumn-web =
                // { workspace = true }` that resolves nowhere is a manifest
                // that would not build, so asking for `--from` is the honest
                // answer. Any other key that resolves nowhere is simply some
                // other crate's inherited dependency — every `{ workspace =
                // true }` entry is collected because the member side cannot
                // tell them apart, and this is where the ones that are not
                // autumn-web drop out.
                AutumnWebDependency::Inherited(key) => match inherited(&directory, &key) {
                    Some(resolved) => resolved,
                    None if key == "autumn-web" => AutumnWebDependency::WithoutVersion,
                    None => continue,
                },
                other => other,
            };
            let requirement = match declaration {
                AutumnWebDependency::Absent | AutumnWebDependency::Inherited(_) => continue,
                // Two ways to know a crate's version is unknown rather than
                // absent: a path or git dependency, which says this crate is on
                // *some* version without saying which; and a manifest that
                // exists but cannot be read or parsed, which says nothing at
                // all. Letting a sibling decide the floor in either case would
                // migrate this crate's source against a version nobody checked
                // — and a vendored checkout is exactly the population the 0.6.0
                // rename affects.
                AutumnWebDependency::WithoutVersion | AutumnWebDependency::Unreadable => {
                    return None;
                }
                AutumnWebDependency::Version(requirement) => requirement,
            };
            // A requirement carrying no usable floor makes the whole answer a
            // guess. Dropping it and taking another declaration's version would
            // silently skip that crate — exactly what refusing an
            // upper-bound-only requirement was meant to prevent. Fail detection
            // and let the caller ask for `--from`.
            let version = migrations::parse_version_req(&requirement)?;
            if lowest.as_ref().is_none_or(|(lowest, _)| version < *lowest) {
                lowest = Some((version, requirement));
            }
        }
    }
    lowest.map(|(_, requirement)| requirement)
}

/// Directories under `root` (excluding `root` itself) that hold a `Cargo.toml`.
///
/// Deliberately every nested manifest, not Cargo's `[workspace] members` /
/// `exclude` set. The rule this keeps is *the floor is taken across exactly the
/// code this command will rewrite*: the source walk visits every directory
/// here, so a crate whose sources are about to be migrated also gets a vote on
/// which migrations apply. Deriving membership from Cargo metadata would split
/// those two sets — a crate under `exclude` would be rewritten against a floor
/// chosen without it.
///
/// The cost is a crate outside the workspace proper (an excluded fixture, a
/// standalone example) pulling the floor back or, if its requirement is
/// ambiguous, making the command ask for `--from`. Both err toward doing more
/// migration or doing none, never toward migrating against the wrong version,
/// and both are visible in the report.
fn member_manifests(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_manifests(root, root, &configured_target_dirs(root), &mut found);
    found.sort();
    found
}

/// Whether `path` is Cargo's configured output directory.
///
/// By resolved path, not by name: with the target directory redirected to
/// `out/`, an unrelated `src/out/mod.rs` is ordinary app code.
fn is_target_dir(path: &Path, target_dirs: &BTreeSet<PathBuf>) -> bool {
    !target_dirs.is_empty()
        && std::fs::canonicalize(path).is_ok_and(|resolved| target_dirs.contains(&resolved))
}

fn collect_manifests(
    root: &Path,
    dir: &Path,
    target_dirs: &BTreeSet<PathBuf>,
    found: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Exactly the exclusions the source walk uses, for exactly its reason: the
    // floor has to be taken over the same crates the rewrite covers. A blanket
    // dot-directory skip here let a hidden crate's sources be scanned and
    // rewritten while its `Cargo.toml` had no vote on which migrations ran.
    let at_crate_root = dir.join("Cargo.toml").is_file();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_metadata = SKIPPED_HIDDEN_DIRS.contains(&name.as_str());
        let is_build_output = at_crate_root && SKIPPED_DIRS.contains(&name.as_str());
        let path = entry.path();
        if is_metadata || is_build_output || is_target_dir(&path, target_dirs) {
            continue;
        }
        if path.join("Cargo.toml").is_file() && path != root {
            found.push(path.clone());
        }
        collect_manifests(root, &path, target_dirs, found);
    }
}

/// Every directory Cargo has been told to hold generated or third-party code,
/// anywhere in the scan.
///
/// `CARGO_TARGET_DIR` and `.cargo/config.toml`'s `build.target-dir` both move
/// output somewhere the `target` basename check will never look, and the walk
/// then descends into generated code — `--apply` rewriting artifacts the next
/// `cargo build` overwrites.
///
/// Collected as a set over the whole tree rather than inherited downward,
/// because a configured path need not sit under the crate that declares it:
/// `tools/helper/.cargo/config.toml` may say `target-dir = "../../build"`, and
/// that directory is reached by a different branch of the walk entirely.
///
/// Two kinds: build output (`build.target-dir`) and vendored dependencies
/// (`[source.*] directory`, which is where `cargo vendor <path>` puts them).
/// Neither is the app's own code, and rewriting the second corrupts a
/// dependency.
///
/// `CARGO_TARGET_DIR` overrides every config file's `target-dir`, but has no
/// equivalent for vendoring, so the config walk runs either way.
fn configured_target_dirs(root: &Path) -> BTreeSet<PathBuf> {
    // `CARGO_TARGET_DIR` is the dedicated variable; `CARGO_BUILD_TARGET_DIR` is
    // the generic `CARGO_BUILD_<key>` form Cargo documents for `build.target-dir`.
    // Either one overrides every config file, so both are read here.
    let forced_target_dir = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|configured| !configured.is_empty())
        .and_then(|configured| {
            let cwd = std::env::current_dir().ok()?;
            resolve_against(&cwd, &configured.to_string_lossy())
        });
    let target_dir_from_config = forced_target_dir.is_none();

    let mut found = BTreeSet::new();
    found.extend(forced_target_dir);
    let absolute_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

    // Cargo's hierarchy is the invocation directory upward, then Cargo home.
    // The two settings read here merge differently and must not be pooled:
    // `build.target-dir` is a scalar, so the *nearest* declaration wins and the
    // rest are overridden — unioning them pruned directories that are really
    // app source. The `[source.*]` tables merge into one table across every
    // level, so `replace-with` in one file can name a source another defines;
    // resolving each file alone found neither half.
    let mut levels: Vec<(PathBuf, PathBuf)> = absolute_root
        .ancestors()
        .map(|ancestor| (ancestor.join(".cargo"), ancestor.to_path_buf()))
        .collect();
    levels.extend(cargo_home().map(|home| (home.clone(), home)));

    let mut sources = Sources::new();
    let mut nearest_target = None;
    // Farthest first, so a nearer level overwrites what it declares and leaves
    // the rest of the merged table standing.
    for (config_dir, base) in levels.iter().rev() {
        let (target, declared) = config_at(config_dir, base, target_dir_from_config);
        if target.is_some() {
            nearest_target = target;
        }
        merge_sources(&mut sources, declared);
    }
    found.extend(nearest_target);
    found.extend(active_vendor_dirs(&sources));

    collect_target_dirs(&absolute_root, target_dir_from_config, &sources, &mut found);
    found
}

/// One `[source.*]` entry: where it points, and what it is replaced with.
///
/// `directory` is already resolved against the base of the config that declared
/// it, because a merged table mixes entries from levels with different bases.
#[derive(Clone, Default)]
struct SourceEntry {
    directory: Option<PathBuf>,
    replace_with: Option<String>,
}

type Sources = BTreeMap<String, SourceEntry>;

/// Overlay `nearer` onto `sources`, field by field.
fn merge_sources(sources: &mut Sources, nearer: Sources) {
    for (name, entry) in nearer {
        let merged = sources.entry(name).or_default();
        if entry.directory.is_some() {
            merged.directory = entry.directory;
        }
        if entry.replace_with.is_some() {
            merged.replace_with = entry.replace_with;
        }
    }
}

/// Whether Cargo would consult this source without something pointing at it.
///
/// The default registry, and the URL-keyed entries `cargo vendor` writes for
/// git and alternative-registry dependencies. A plain name like `vendored` is a
/// local alias: real until something is replaced *with* it, inert otherwise.
fn is_consulted_source(name: &str) -> bool {
    name == "crates-io" || name.contains("://")
}

/// The directories of every source that something is actually replaced *with*.
///
/// Source replacement is activated by `replace-with`: defining `[source.archive]
/// directory = "src"` and never replacing anything with it leaves `src` as
/// ordinary app source. A replacement may itself be replaced, so the chain is
/// followed to its end.
fn active_vendor_dirs(sources: &Sources) -> Vec<PathBuf> {
    let mut active: BTreeSet<&str> = BTreeSet::new();
    // Reachability starts at the sources Cargo actually consults, not at every
    // declared edge: `[source.unused] replace-with = "vendored"` that nothing
    // references activates nothing, and treating it as active excluded whatever
    // `vendored` pointed at — the app's own `src/`, in the reported case.
    let mut pending: Vec<&str> = sources
        .iter()
        .filter(|(name, _)| is_consulted_source(name))
        .filter_map(|(_, entry)| entry.replace_with.as_deref())
        .collect();
    // `active` doubles as the seen-set, so a config that points two sources at
    // each other terminates instead of spinning.
    while let Some(name) = pending.pop() {
        if !active.insert(name) {
            continue;
        }
        pending.extend(
            sources
                .get(name)
                .and_then(|entry| entry.replace_with.as_deref()),
        );
    }
    active
        .into_iter()
        .filter_map(|name| sources.get(name)?.directory.clone())
        .collect()
}

/// Cargo's own configuration directory — `$CARGO_HOME`, or `~/.cargo`.
///
/// The last level of the hierarchy, and the one with no `.cargo` component:
/// the file is `$CARGO_HOME/config.toml`, not `$CARGO_HOME/.cargo/config.toml`.
///
/// `BaseDirs` rather than `$HOME`: on Windows Cargo derives its home from the
/// user profile, and `$HOME` is normally unset there.
pub fn cargo_home() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("CARGO_HOME") {
        let path = PathBuf::from(configured);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    Some(directories::BaseDirs::new()?.home_dir().join(".cargo"))
}

/// Walk `dir` for `.cargo/config.toml` redirects, adding each to `found`.
///
/// Directories already known to be build output are not descended into. That
/// is partly cost — a populated target tree holds tens of thousands of
/// directories, and this walk runs on every invocation — and partly
/// correctness: a `.cargo/config.toml` that got copied or generated inside
/// build output is not this project's configuration, and honouring it can
/// exclude real source.
///
/// A redirect that points *outside* the crate declaring it is only pruned if
/// the walk happens to learn of it first; directory order is not guaranteed.
/// The source walk excludes it either way, so what is at stake there is the
/// traversal cost, not whether the directory is migrated.
fn collect_target_dirs(
    dir: &Path,
    target_dir_from_config: bool,
    inherited: &Sources,
    found: &mut BTreeSet<PathBuf>,
) {
    // A nested crate inherits the hierarchy above it, so its own `replace-with`
    // may name a source one of those levels defines.
    let (target, declared) = config_at(&dir.join(".cargo"), dir, target_dir_from_config);
    found.extend(target);
    let mut sources = inherited.clone();
    merge_sources(&mut sources, declared);
    found.extend(active_vendor_dirs(&sources));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let at_crate_root = dir.join("Cargo.toml").is_file();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // The same pruning the source walk uses: a `.cargo` inside a vendored
        // tree is not this project's configuration.
        if SKIPPED_HIDDEN_DIRS.contains(&name.as_str())
            || (at_crate_root && SKIPPED_DIRS.contains(&name.as_str()))
        {
            continue;
        }
        let path = entry.path();
        if is_target_dir(&path, found) {
            continue;
        }
        collect_target_dirs(&path, target_dir_from_config, &sources, found);
    }
}

/// Resolve `path` against `base` unless it is already absolute, then canonicalise.
fn resolve_against(base: &Path, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    std::fs::canonicalize(absolute).ok()
}

/// The `build.target-dir` and the `[source.*]` entries a Cargo config in
/// `config_dir` declares, resolved against `base`.
///
/// Cargo resolves a relative entry against the directory holding the config
/// that declares it, so that is what these resolve against — and why the
/// resolution happens here rather than after merging, when the base is gone.
/// `target_dir` is false when `CARGO_TARGET_DIR` has already decided that
/// question.
///
/// `config` is read before `config.toml`: when a project holds both, Cargo uses
/// the extensionless name and warns. Reading the other one first meant the
/// directory Cargo actually builds into was scanned as app source.
fn config_at(config_dir: &Path, base: &Path, target_dir: bool) -> (Option<PathBuf>, Sources) {
    for name in ["config", "config.toml"] {
        let Ok(content) = std::fs::read_to_string(config_dir.join(name)) else {
            continue;
        };
        let Ok(table) = toml::from_str::<toml::Table>(&content) else {
            continue;
        };
        let target = table
            .get("build")
            .and_then(|build| build.get("target-dir"))
            .and_then(toml::Value::as_str)
            .filter(|_| target_dir)
            .and_then(|configured| resolve_against(base, configured));
        // `[source.vendored-sources] directory = "third-party"` — where
        // `cargo vendor <path>` was told to put dependency sources.
        let mut sources = Sources::new();
        if let Some(declared) = table.get("source").and_then(toml::Value::as_table) {
            for (source_name, source) in declared {
                sources.insert(
                    source_name.clone(),
                    SourceEntry {
                        directory: source
                            .get("directory")
                            .and_then(toml::Value::as_str)
                            .and_then(|directory| resolve_against(base, directory)),
                        replace_with: source
                            .get("replace-with")
                            .and_then(toml::Value::as_str)
                            .map(str::to_owned),
                    },
                );
            }
        }
        return (target, sources);
    }
    (None, Sources::new())
}

/// Every `.rs` file under `root` that belongs to the app, in a stable order.
fn app_sources(root: &Path) -> SourceScan {
    let mut scan = SourceScan::default();
    collect_sources(root, &configured_target_dirs(root), &mut scan);
    scan.files.sort();
    scan.symlinks.sort();
    scan.unreadable.sort();
    scan
}

/// What a walk of the app's tree turned up.
#[derive(Debug, Default)]
struct SourceScan {
    /// Regular `.rs` files to migrate.
    files: Vec<PathBuf>,
    /// Symlinked `.rs` files and symlinked directories, reported rather than
    /// followed.
    symlinks: Vec<PathBuf>,
    /// Directories the walk could not read.
    unreadable: Vec<PathBuf>,
}

/// Collect `.rs` files, recording what was deliberately or accidentally left
/// out so the caller can report it rather than drop it silently.
fn collect_sources(dir: &Path, target_dirs: &BTreeSet<PathBuf>, scan: &mut SourceScan) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        scan.unreadable.push(dir.to_path_buf());
        return;
    };
    // `target/`, `vendor/` and friends name build output or third-party code
    // only where a crate begins. Matching the basename at *any* depth silently
    // dropped ordinary modules like `src/vendor/mod.rs` — the app then fails to
    // compile after the bump with nothing in the report to explain why.
    let at_crate_root = dir.join("Cargo.toml").is_file();

    for entry in entries.flatten() {
        // `file_type` does not follow symlinks, so a link into `target/`, a
        // link out of the project, or a cycle is neither descended into nor
        // rewritten. Links are still *reported*: quietly leaving source out of
        // the migration is the failure mode this command exists to remove.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();

        if file_type.is_dir() {
            let is_metadata = SKIPPED_HIDDEN_DIRS.contains(&name.as_str());
            let is_build_output = at_crate_root && SKIPPED_DIRS.contains(&name.as_str());
            if is_metadata || is_build_output || is_target_dir(&path, target_dirs) {
                continue;
            }
            collect_sources(&path, target_dirs, scan);
        } else if file_type.is_symlink() {
            // A symlinked *directory* has `is_dir() == false`, so gating this
            // on a `.rs` extension hid a linked `src/` entirely: no traversal,
            // no report, and a run that says "nothing to change" about an app
            // it never looked at.
            let links_to_directory =
                std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
            if links_to_directory || path.extension().is_some_and(|ext| ext == "rs") {
                scan.symlinks.push(path);
            }
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            scan.files.push(path);
        }
    }
}

/// Display form of `path` relative to `root`, with `/` separators on every
/// platform so the reported `file:line` is copy-pasteable.
fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Which types the app's own `#[repository]` traits generate.
///
/// Collected across the whole scan before any rewriting: this is what turns the
/// receiver test from a naming convention into evidence, and the trait and the
/// call site normally live in different modules. A file that cannot be read or
/// parsed contributes nothing rather than failing the scan — it is reported on
/// its own account when the rewrite pass reaches it.
fn generated_receivers(files: &[PathBuf]) -> engine::GeneratedRepositories {
    let mut generated: engine::GeneratedRepositories = files
        .iter()
        .filter_map(|path| {
            let source = std::fs::read_to_string(path).ok()?;
            let prefix = file_module_path(path);
            Some(engine::generated_repository_types(&source).into_iter().map(
                move |(name, inner)| {
                    let mut module = prefix.clone();
                    module.extend(inner);
                    (name, module)
                },
            ))
        })
        .flatten()
        .collect();
    generated.note_handwritten(
        files
            .iter()
            .filter_map(|path| {
                let source = std::fs::read_to_string(path).ok()?;
                let prefix = file_module_path(path);
                Some(
                    engine::defined_type_names(&source)
                        .into_iter()
                        .map(move |(name, inner)| {
                            let mut module = prefix.clone();
                            module.extend(inner);
                            (name, module)
                        }),
                )
            })
            .flatten(),
    );
    generated
}

/// The module path a file contributes, from Cargo's file-to-module mapping.
///
/// `src/repositories.rs` is the module `repositories`, `src/a/b.rs` is `a::b`,
/// and `mod.rs` / `lib.rs` / `main.rs` name their directory rather than
/// themselves. A file outside a `src` directory — a test or an example — is
/// treated as the crate root, which only ever makes verification more
/// permissive.
///
/// `#[path = "…"]` can move a module somewhere this does not predict. The cost
/// is a *qualified* call being reported for a human instead of rewritten, which
/// is the safe direction and is visible in the report.
fn file_module_path(path: &Path) -> Vec<String> {
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let Some(src_at) = components.iter().rposition(|component| component == "src") else {
        return Vec::new();
    };
    let mut module: Vec<String> = components[src_at + 1..].to_vec();
    let Some(last) = module.pop() else {
        return Vec::new();
    };
    let stem = last.strip_suffix(".rs").unwrap_or(&last);
    if !matches!(stem, "mod" | "lib" | "main") {
        module.push(stem.to_owned());
    }
    module
}

/// Build the report for `root` without writing anything.
fn plan(root: &Path, from: Version, to: Version) -> Report {
    let selected = migrations::migrations_between(&from, &to);
    plan_with(root, from, to, &selected)
}

/// [`plan`] over an explicit migration selection.
///
/// Split out so the `review` and multi-release paths are testable: the shipped
/// registry is a `static` with a single release in it, so a selection taken
/// from it can never exercise either.
fn plan_with(
    root: &Path,
    from: Version,
    to: Version,
    selected: &[&'static AppMigration],
) -> Report {
    let mut report = Report {
        from,
        to,
        outcome: Outcome::Preview,
        files_scanned: 0,
        migrations: selected.to_vec(),
        files: Vec::new(),
        review: Vec::new(),
        manual: Vec::new(),
        skipped: Vec::new(),
    };

    // A change with no machine-applyable rewrite is still the user's problem;
    // surface it with its guide section rather than staying silent.
    for migration in selected {
        if matches!(migration.rewrite, migrations::Rewrite::GuideOnly) {
            report.manual.push(ManualEntry {
                path: None,
                line: None,
                migration: migration.id,
                reason: "no machine-applyable rewrite".to_owned(),
                guide: migration.guide,
            });
        }
    }

    // Every reported site names a migration from this same selection, so the
    // lookup only fails if the two ever drift; the index page is the honest
    // fallback rather than a guessed confidence level.
    let selected_by_id = |id: &str| {
        selected
            .iter()
            .copied()
            .find(|migration| migration.id == id)
    };
    let guide_for = |id: &str| selected_by_id(id).map_or("docs/migrations/README.md", |m| m.guide);

    let scan = app_sources(root);
    for path in scan.symlinks {
        report.skipped.push((
            display_path(root, &path),
            "symbolic link - not followed, migrate it in its own checkout".to_owned(),
        ));
    }
    for path in scan.unreadable {
        report.skipped.push((
            display_path(root, &path),
            "directory could not be read - nothing under it was migrated".to_owned(),
        ));
    }
    let generated = generated_receivers(&scan.files);
    for path in scan.files {
        let display = display_path(root, &path);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                report.skipped.push((display, error.to_string()));
                continue;
            }
        };
        report.files_scanned += 1;
        let rewrite = match engine::rewrite_source_for_releases(&source, selected, &generated) {
            Ok(rewrite) => rewrite,
            Err(error) => {
                report.skipped.push((display, error));
                continue;
            }
        };

        for site in &rewrite.manual {
            report.manual.push(ManualEntry {
                path: Some(display.clone()),
                line: Some(site.line),
                migration: site.migration,
                reason: site.manual.map_or_else(
                    || "not rewritable".to_owned(),
                    |reason| reason.describe().to_owned(),
                ),
                guide: guide_for(site.migration),
            });
        }

        // A `review` migration rewrites, but every site it touched is listed
        // individually so a human reads them before committing.
        for site in &rewrite.rewritten {
            if selected_by_id(site.migration)
                .is_some_and(|m| m.confidence == migrations::Confidence::Review)
            {
                report.review.push(ManualEntry {
                    path: Some(display.clone()),
                    line: Some(site.line),
                    migration: site.migration,
                    reason: "rewritten - confirm the new call is what you meant".to_owned(),
                    guide: guide_for(site.migration),
                });
            }
        }

        if let Some(updated) = rewrite.updated {
            report.files.push(FileReport {
                path: display,
                sites: rewrite.rewritten,
                diff: diff::render(&source, &updated),
                updated,
                original: source,
                absolute: path,
            });
        }
    }

    report
}

/// Write every planned rewrite.
///
/// Each file is written through a sibling temporary file and renamed over the
/// original, so an interrupted write leaves the original intact rather than a
/// truncated source file.
fn write_plan(report: &Report) -> Result<(), WriteFailure> {
    for (written, file) in report.files.iter().enumerate() {
        write_one(file).map_err(|error| WriteFailure {
            path: file.path.clone(),
            error,
            written,
        })?;
    }
    Ok(())
}

/// Which file the apply step died on, and why. Without the path, a failure
/// halfway through a multi-file apply tells the user nothing about what state
/// their tree is in.
#[derive(Debug)]
struct WriteFailure {
    path: String,
    error: std::io::Error,
    /// Files successfully written before this one.
    written: usize,
}

/// Write one file through a sibling temporary file renamed over the original.
fn write_one(file: &FileReport) -> std::io::Result<()> {
    use std::io::Write as _;

    let directory = file.absolute.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        // Deliberately not `.rs`: a temporary left behind by a killed run must
        // not be picked up as app source by the next one.
        .prefix(".autumn-upgrade-")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    temp.write_all(file.updated.as_bytes())?;
    temp.flush()?;
    // Rename is atomic, but only against a *crash* if the bytes reached the
    // disk first — otherwise the rename can land before the data and leave the
    // user with an empty source file.
    temp.as_file().sync_all()?;
    // A fresh temporary file is created 0600; the rename would otherwise
    // silently tighten the mode of every migrated source file.
    if let Ok(metadata) = std::fs::metadata(&file.absolute) {
        let _ = temp.as_file().set_permissions(metadata.permissions());
    }
    // Planning reads every file before anything is written, so a formatter, a
    // generator, or the user's editor can change one inside that window. The
    // rewrite was computed from a snapshot; writing it now would silently
    // revert whatever landed since. Re-read and compare first, and skip the
    // file rather than take the newer content away.
    match std::fs::read_to_string(&file.absolute) {
        Ok(current) if current == file.original => {}
        Ok(_) => {
            return Err(std::io::Error::other(
                "file changed on disk after it was planned - re-run `autumn upgrade` to \
                 migrate it against its current contents",
            ));
        }
        Err(error) => return Err(error),
    }
    temp.persist(&file.absolute)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod write_guard_tests {
    use super::*;

    #[test]
    fn a_file_changed_after_planning_is_not_overwritten() {
        // Planning reads every file before anything is written. A formatter or
        // an editor saving inside that window would otherwise have its work
        // silently replaced by the rewrite of a stale snapshot.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let planned = FileReport {
            path: "main.rs".to_owned(),
            sites: Vec::new(),
            diff: String::new(),
            updated: "fn main() { /* rewritten */ }\n".to_owned(),
            original: "fn main() {}\n".to_owned(),
            absolute: path.clone(),
        };
        // Unchanged: the write lands.
        write_one(&planned).expect("an untouched file is written");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            planned.updated
        );

        // Changed underneath the plan: refused, and what is on disk survives.
        std::fs::write(&path, "fn main() { /* someone else */ }\n").expect("write");
        let error = write_one(&planned).expect_err("a changed file must not be overwritten");
        assert!(
            error.to_string().contains("changed on disk"),
            "the reason names the cause: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "fn main() { /* someone else */ }\n",
            "the newer contents are left alone"
        );
    }
}

/// Plan the scaffold half of the run, when the root is an Autumn project.
///
/// Framework-owned scaffold files are reconciled in the same run as the app
/// code (issue #1593), so "bring me up to this release" is one command rather
/// than two. Only in an actual Autumn project: `autumn upgrade` also runs over
/// plain crates that merely depend on autumn-web, and offering to seed a
/// Dockerfile into one of those would be nonsense.
///
/// Planned *after* the app-code step, not alongside it: `build.rs` is both a
/// framework-owned file and a `.rs` file the codemods scan, so a plan made
/// before the rewrites would be a plan about bytes that no longer exist.
///
/// The codemods' *plan* is what the scaffold half is told about, not what the
/// apply step happened to write. The two are the same set — `--apply` writes
/// the plan — and using the narrower one would make the preview disagree with
/// the run it is previewing: a bare `autumn upgrade` would offer `build.rs` as
/// a writable `update` that `--apply` then refuses. A preview that does not
/// predict its own apply is worse than no preview, and this is the one file
/// that can be in both halves at once.
fn plan_scaffold(root: &Path, target: &str, report: &Report) -> Option<scaffold::ScaffoldReport> {
    let migrated: BTreeSet<String> = report.files.iter().map(|file| file.path.clone()).collect();
    scaffold::is_project(root).then(|| scaffold::plan_after(root, target, &migrated))
}

/// Reject flag combinations whose meanings contradict each other.
///
/// Each of these would otherwise "work" while doing something other than what
/// was asked — the worst kind of CLI behaviour, because nothing reports it.
fn reject_bad_combination(opts: &UpgradeOptions) -> Option<i32> {
    if opts.check && opts.apply {
        eprintln!(
            "autumn upgrade: `--check` reports drift and writes nothing; it cannot be combined with `--apply`."
        );
        return Some(2);
    }
    // `--list-migrations` short-circuits everything below it, so accepting it
    // alongside `--check` would print the registry, exit 0, and gate nothing —
    // a CI job silently not doing its job.
    if opts.list && (opts.check || !opts.accept.is_empty()) {
        eprintln!(
            "autumn upgrade: `--list-migrations` prints the registry and exits; it cannot be \
             combined with `--check` or `--accept`."
        );
        return Some(2);
    }
    None
}

/// Record framework-owned paths as the developer's own (issue #1593).
fn accept_scaffold(root: &Path, paths: &[String], json: bool) -> i32 {
    if !scaffold::is_project(root) {
        eprintln!(
            "autumn upgrade: `{}` is not an Autumn project (no `autumn.toml`).",
            root.display()
        );
        return 2;
    }
    match scaffold::accept(root, paths) {
        Ok(manifest) => {
            // `--json` is the machine-readable mode for the whole command, so
            // this branch cannot be the one that prints prose onto a stdout a
            // caller is parsing.
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "accepted": paths,
                        "pinned": manifest.pinned,
                        "manifest": scaffold::MANIFEST_PATH,
                    }))
                    .unwrap_or_else(|_| "{}".to_owned())
                );
                return 0;
            }
            println!(
                "Accepted as yours; `autumn upgrade` will leave {} alone:",
                if paths.len() == 1 { "it" } else { "them" }
            );
            for path in paths {
                println!("  {path}");
            }
            println!(
                "Recorded in {}. Delete a line from its `pinned` list to bring a file back\n\
                 under reconciliation.",
                scaffold::MANIFEST_PATH
            );
            0
        }
        Err(error) => {
            eprintln!("autumn upgrade: {error}");
            2
        }
    }
}

/// Print one run's reports — app code, and the scaffold section when there is
/// one.
///
/// A directory that is not an Autumn project produces byte-for-byte the report
/// it always did: the scaffold section is an addition to this command, not a
/// change to it.
fn emit(report: &Report, scaffold_report: Option<&scaffold::ScaffoldReport>, json: bool) {
    match (json, scaffold_report) {
        (true, None) => println!("{}", render_json(report)),
        (true, Some(scaffold_report)) => {
            let mut value = json_value(report);
            value["scaffold"] = scaffold::json(scaffold_report);
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
            );
        }
        (false, scaffold_report) => {
            print!("{}", render_text(report));
            if let Some(scaffold_report) = scaffold_report {
                print!("{}", scaffold::render_text(scaffold_report));
            }
        }
    }
}

fn check_scaffold(root: &Path, target: &str, json: bool) -> i32 {
    if !scaffold::is_project(root) {
        eprintln!(
            "autumn upgrade: `{}` is not an Autumn project (no `autumn.toml`).\n\
             `--check` gates a project's framework-owned files against the current scaffold.",
            root.display()
        );
        return 2;
    }
    let report = scaffold::plan(root, target);
    if json {
        // The same shape a normal `--json` run emits, so one `jq
        // '.scaffold.drift'` works against both. Two shapes for one field is
        // how a CI gate ends up reading `null` and passing.
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "scaffold": scaffold::json(&report) })
            )
            .unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        print!("{}", scaffold::render_summary(&report));
    }
    // A refusal to answer is not an all-clear, whatever the reason for it — a
    // gate that goes green because the tool could not look is worse than no
    // gate at all. This project's files were written by a release newer than
    // this CLI, so reconciling them would mean downgrading them.
    if let Some(newer) = &report.scaffolded_by_newer {
        eprintln!(
            "autumn upgrade: `{}` was scaffolded by autumn-cli {newer}, newer than this one \
             ({}).\n\
             Nothing was checked: reconciling would downgrade its files. Install {newer} or later.",
            root.display(),
            env!("CARGO_PKG_VERSION")
        );
        return 2;
    }
    // Without a usable `[package] name` the scaffold cannot be rendered, so
    // there is no verdict either.
    if !report.named {
        eprintln!(
            "autumn upgrade: `{}` has no usable `[package] name` in Cargo.toml, and the\n\
             scaffold interpolates it. Nothing was checked.",
            root.display()
        );
        return 2;
    }
    if report.drifted() { DRIFT_EXIT_CODE } else { 0 }
}

/// Print the shipped codemod registry, for `--list-migrations`.
fn list_migrations(json: bool) {
    let all: Vec<&'static AppMigration> = migrations::app_migrations().iter().collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "migrations": migrations_json(&all) })
            )
            .unwrap_or_else(|_| "{}".to_owned())
        );
        return;
    }
    println!("Shipped app-code migrations ({}):", all.len());
    for migration in &all {
        println!(
            "  {:<6}  {}  {}",
            migration.confidence.label(),
            migration.id,
            migration.title
        );
        println!("          {}", migrations::guide_url(migration.guide));
    }
}

/// Run `autumn upgrade` against `root`, returning the process exit code.
///
/// `0` on any completed scan — including one that found sites only a human can
/// take, or files it could not parse, both of which the report names. `1` when
/// the apply step failed partway through, `2` for a bad argument or an
/// unreadable scan root, and [`DRIFT_EXIT_CODE`] when `--check` found scaffold
/// drift.
#[must_use]
pub fn run_in(root: &Path, opts: &UpgradeOptions) -> i32 {
    if let Some(code) = reject_bad_combination(opts) {
        return code;
    }
    if opts.list {
        list_migrations(opts.json);
        return 0;
    }

    let target = opts
        .to
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
    let Some(to) = migrations::parse_version_req(&target) else {
        eprintln!("autumn upgrade: cannot parse --to version `{target}`");
        return 2;
    };

    // A path typo must not read as "your app is already migrated" — the same
    // rule `autumn a11y verify` states for its scan root. Readability, not just
    // `is_dir`: a directory the process cannot open answers "yes" to `is_dir`,
    // and the walk would then report the root under `skipped` and exit 0 having
    // examined no source at all.
    if std::fs::read_dir(root).is_err() {
        eprintln!(
            "autumn upgrade: `{}` is not a readable directory.",
            root.display()
        );
        return 2;
    }

    // The CI gate (issue #1593). Deliberately ahead of the version resolution:
    // scaffold drift is measured against the recorded provenance manifest, not
    // against the `autumn-web` requirement, so a project whose manifest Cargo
    // cannot give a single floor for can still be gated on scaffold freshness.
    if !opts.accept.is_empty() {
        return accept_scaffold(root, &opts.accept, opts.json);
    }

    if opts.check {
        return check_scaffold(root, &target, opts.json);
    }

    let recorded = opts.from.clone().or_else(|| recorded_version(root));
    let Some(recorded) = recorded else {
        eprintln!(
            "autumn upgrade: cannot tell which autumn-web version this app is on.\n\
             Add an `autumn-web` dependency to Cargo.toml, or pass `--from <version>`."
        );
        return 2;
    };
    let Some(from) = migrations::parse_version_req(&recorded) else {
        eprintln!(
            "autumn upgrade: cannot parse the recorded autumn-web version `{recorded}`.\n\
             Pass `--from <version>` with the release this app is upgrading from."
        );
        return 2;
    };

    let mut report = plan(root, from, to);
    let mut failure = None;
    let mut scaffold_failure = None;
    if opts.apply {
        match write_plan(&report) {
            Ok(()) => report.outcome = Outcome::Applied,
            Err(write_failure) => {
                // Report first, fail second: a partial apply is exactly when
                // the user most needs to see which files were in the plan and
                // which one stopped it — and saying "nothing was written" when
                // some files already changed would be worse than saying
                // nothing at all.
                report.outcome = Outcome::Partial {
                    written: write_failure.written,
                };
                failure = Some(write_failure);
            }
        }
    }

    let mut scaffold_report = plan_scaffold(root, &target, &report);
    // Only when the app-code step completed. A half-migrated tree is not a tree
    // to start rewriting project files in.
    if opts.apply
        && failure.is_none()
        && let Some(scaffold_report) = scaffold_report.as_mut()
    {
        scaffold_failure = scaffold::apply(scaffold_report).err();
    }

    emit(&report, scaffold_report.as_ref(), opts.json);

    // `--to` selects codemods; the scaffold half can only ever reconcile to the
    // release this CLI ships templates for. Silently ignoring the flag would
    // leave a reader believing the whole run targeted what they asked for.
    if scaffold_report.is_some()
        && opts
            .to
            .as_deref()
            .is_some_and(|to| to != env!("CARGO_PKG_VERSION"))
    {
        eprintln!(
            "autumn upgrade: `--to` selects which app-code migrations run. The scaffold\n\
             files were reconciled against {}, the only scaffold this CLI ships.",
            env!("CARGO_PKG_VERSION")
        );
    }

    if let Some(WriteFailure { path, error, .. }) = failure {
        eprintln!(
            "autumn upgrade: failed while writing {path}: {error}\n\
             Files listed before it in the report above were already written; \
             `git diff` shows exactly what changed."
        );
        return 1;
    }
    if let Some(scaffold::WriteFailure {
        path,
        error,
        written,
    }) = scaffold_failure
    {
        eprintln!(
            "autumn upgrade: failed while writing the scaffold file {path}: {error}\n\
             {written} scaffold file(s) were written before it; \
             `git diff` shows exactly what changed."
        );
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::migrations::{CallForm, Confidence, Rewrite};
    use super::*;

    static AUTO: AppMigration = AppMigration {
        id: "0.6.0-pool-rename",
        version: "0.6.0",
        title: "`with_pool` is renamed to `with_pool_untracked`",
        confidence: Confidence::Auto,
        guide: "docs/migrations/0.6.0.md#rename",
        rewrite: Rewrite::CallRename {
            from: "with_pool",
            to: "with_pool_untracked",
            form: CallForm::AssociatedFunction,
            args: 1,
            receiver: None,
        },
    };

    static MANUAL: AppMigration = AppMigration {
        id: "0.6.0-jwt-secret",
        version: "0.6.0",
        title: "`jwt_secret` is now a `SecretString`",
        confidence: Confidence::Manual,
        guide: "docs/migrations/0.6.0.md#secret",
        rewrite: Rewrite::GuideOnly,
    };

    fn version(major: u64, minor: u64, patch: u64) -> Version {
        Version::new(major, minor, patch)
    }

    fn sample(outcome: Outcome) -> Report {
        Report {
            from: version(0, 5, 0),
            to: version(0, 6, 0),
            outcome,
            files_scanned: 7,
            migrations: vec![&AUTO, &MANUAL],
            files: vec![FileReport {
                path: "src/main.rs".into(),
                sites: vec![engine::Site {
                    line: 12,
                    column: 19,
                    migration: AUTO.id,
                    manual: None,
                }],
                diff: "@@ line 12 @@\n-a\n+b\n".into(),
                updated: "b\n".into(),
                original: "a\n".into(),
                absolute: PathBuf::from("/tmp/app/src/main.rs"),
            }],
            review: Vec::new(),
            manual: vec![
                ManualEntry {
                    path: Some("src/lib.rs".into()),
                    line: Some(40),
                    migration: AUTO.id,
                    reason: "inside a macro invocation".into(),
                    guide: AUTO.guide,
                },
                ManualEntry {
                    path: None,
                    line: None,
                    migration: MANUAL.id,
                    reason: "no machine-applyable rewrite".into(),
                    guide: MANUAL.guide,
                },
            ],
            skipped: vec![("src/broken.rs".into(), "expected `{`".into())],
        }
    }

    fn empty(outcome: Outcome) -> Report {
        Report {
            from: version(0, 6, 0),
            to: version(0, 6, 0),
            outcome,
            files_scanned: 3,
            migrations: Vec::new(),
            files: Vec::new(),
            review: Vec::new(),
            manual: Vec::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn rewritten_sites_totals_every_file() {
        assert_eq!(sample(Outcome::Preview).rewritten_sites(), 1);
        assert_eq!(empty(Outcome::Preview).rewritten_sites(), 0);
    }

    #[test]
    fn preview_text_shows_the_diff_the_counts_and_that_nothing_was_written() {
        let out = render_text(&sample(Outcome::Preview));
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(out.contains("@@ line 12 @@"), "{out}");
        assert!(out.contains("1 site"), "site count is reported: {out}");
        assert!(
            out.contains("--apply"),
            "the preview must name the explicit write step: {out}"
        );
        assert!(out.to_lowercase().contains("nothing was written"), "{out}");
    }

    #[test]
    fn preview_text_lists_every_manual_site_with_location_and_guide() {
        let out = render_text(&sample(Outcome::Preview));
        assert!(out.contains("src/lib.rs:40"), "{out}");
        assert!(out.contains("inside a macro invocation"), "{out}");
        assert!(out.contains("docs/migrations/0.6.0.md#rename"), "{out}");
        assert!(out.contains("docs/migrations/0.6.0.md#secret"), "{out}");
    }

    #[test]
    fn preview_text_labels_each_selected_migration_with_its_confidence() {
        let out = render_text(&sample(Outcome::Preview));
        // The fixture ids deliberately contain neither word, so these can only
        // pass if the label column is actually rendered.
        assert!(out.contains("auto    0.6.0-pool-rename"), "{out}");
        assert!(out.contains("manual  0.6.0-jwt-secret"), "{out}");
    }

    #[test]
    fn preview_text_reports_skipped_files() {
        let out = render_text(&sample(Outcome::Preview));
        assert!(out.contains("src/broken.rs"), "{out}");
        assert!(out.contains("expected `{`"), "{out}");
    }

    #[test]
    fn applied_text_says_what_was_written_and_not_what_would_be() {
        let out = render_text(&sample(Outcome::Applied));
        assert!(!out.to_lowercase().contains("nothing was written"), "{out}");
        assert!(out.contains("1 site in 1 file rewritten"), "{out}");
    }

    #[test]
    fn an_unaffected_app_reports_nothing_to_change() {
        let out = render_text(&empty(Outcome::Preview));
        assert!(
            out.to_lowercase().contains("nothing to change"),
            "an app that never used the affected APIs must say so plainly: {out}"
        );
        assert!(!out.contains("@@"), "no diff to show: {out}");
    }

    static REVIEW: AppMigration = AppMigration {
        id: "0.6.0-review-rename",
        version: "0.6.0",
        title: "a rename worth a second look",
        confidence: Confidence::Review,
        guide: "docs/migrations/0.6.0.md#review",
        rewrite: Rewrite::CallRename {
            from: "with_pool",
            to: "with_pool_untracked",
            form: CallForm::AssociatedFunction,
            args: 1,
            receiver: None,
        },
    };

    #[test]
    fn a_review_migration_rewrites_and_flags_every_site_it_touched() {
        // The shipped registry has no `review` entry, so this path is only
        // reachable through an injected selection — and it is an acceptance
        // criterion, not dead code.
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("src")).expect("src");
        std::fs::write(
            root.path().join("src/main.rs"),
            "fn a(p: Pool) {\n    Repo::with_pool(p);\n}\n",
        )
        .expect("source");

        let report = plan_with(root.path(), version(0, 5, 0), version(0, 6, 0), &[&REVIEW]);

        assert_eq!(
            report.rewritten_sites(),
            1,
            "a review migration still rewrites"
        );
        assert_eq!(report.review.len(), 1, "and flags the site it rewrote");
        assert_eq!(report.review[0].path.as_deref(), Some("src/main.rs"));
        assert_eq!(report.review[0].line, Some(2));
        assert_eq!(report.review[0].migration, REVIEW.id);
        assert_eq!(report.review[0].guide, REVIEW.guide);
        assert!(
            report.manual.is_empty(),
            "a review site is not a manual one"
        );

        let text = render_text(&report);
        assert!(text.contains("Review - rewritten"), "{text}");
        assert!(text.contains("src/main.rs:2"), "{text}");
        assert!(text.contains("review  0.6.0-review-rename"), "{text}");

        let json: serde_json::Value =
            serde_json::from_str(&render_json(&report)).expect("valid JSON");
        assert_eq!(json["review"][0]["line"], 2);
        assert_eq!(json["migrations"][0]["confidence"], "review");
    }

    #[test]
    fn an_auto_migration_flags_no_sites_for_review() {
        // The distinction between `auto` and `review` is exactly this list.
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("src")).expect("src");
        std::fs::write(
            root.path().join("src/main.rs"),
            "fn a(p: Pool) {\n    Repo::with_pool(p);\n}\n",
        )
        .expect("source");

        let report = plan_with(root.path(), version(0, 5, 0), version(0, 6, 0), &[&AUTO]);
        assert_eq!(report.rewritten_sites(), 1);
        assert!(
            report.review.is_empty(),
            "auto sites are not flagged one by one"
        );
    }

    #[test]
    fn json_report_carries_the_range_counts_and_labels() {
        let out = render_json(&sample(Outcome::Preview));
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["from"], "0.5.0");
        assert_eq!(value["to"], "0.6.0");
        assert_eq!(value["applied"], false);
        assert_eq!(value["files_scanned"], 7);
        assert_eq!(value["rewritten_sites"], 1);
        assert_eq!(value["migrations"][0]["id"], "0.6.0-pool-rename");
        assert_eq!(value["migrations"][0]["confidence"], "auto");
        assert_eq!(value["files"][0]["path"], "src/main.rs");
        assert_eq!(value["files"][0]["sites"][0]["line"], 12);
        assert_eq!(value["manual"][0]["path"], "src/lib.rs");
        assert_eq!(value["manual"][0]["line"], 40);
        assert_eq!(value["skipped"][0]["path"], "src/broken.rs");
    }

    #[test]
    fn a_partial_apply_never_claims_nothing_was_written() {
        // The failure mode this guards: some files already rewritten, and the
        // report telling the user to "re-run with --apply" as if the tree were
        // untouched.
        // Two planned files, one of them written.
        let mut report = sample(Outcome::Preview);
        report.files.push(FileReport {
            path: "src/second.rs".into(),
            sites: vec![
                engine::Site {
                    line: 4,
                    column: 9,
                    migration: AUTO.id,
                    manual: None,
                },
                engine::Site {
                    line: 7,
                    column: 9,
                    migration: AUTO.id,
                    manual: None,
                },
            ],
            diff: "@@ line 4 @@\n-a\n+b\n".into(),
            updated: "b\n".into(),
            original: "a\n".into(),
            absolute: PathBuf::from("/tmp/app/src/second.rs"),
        });
        report.outcome = Outcome::Partial { written: 1 };

        let text = render_text(&report);
        assert!(
            text.contains("1 site in 1 file written before the run stopped"),
            "only what landed is counted as written: {text}"
        );
        assert!(
            text.contains("2 sites in 1 file planned and not written"),
            "and the rest is named as not written: {text}"
        );
        assert!(
            !text.contains("3 sites in 2 files rewritten"),
            "the whole plan must not be claimed as rewritten: {text}"
        );
        assert!(
            !text.to_lowercase().contains("nothing was written"),
            "the tree was modified: {text}"
        );
        assert!(
            !text.contains("Re-run with `--apply`"),
            "the apply step already ran: {text}"
        );

        let json: serde_json::Value =
            serde_json::from_str(&render_json(&report)).expect("valid JSON");
        assert_eq!(json["outcome"], "partial");
        assert_eq!(json["files_written"], 1);
        assert_eq!(
            json["applied"], false,
            "`applied` stays a strict did-everything-land flag"
        );
        // The two site counts must not be conflated: automation gating on what
        // landed would otherwise read the whole plan as having landed.
        assert_eq!(
            json["rewritten_sites"], 3,
            "the plan is still reported in full"
        );
        assert_eq!(
            json["written_sites"], 1,
            "but only the sites in the files that reached disk are written"
        );
    }

    #[test]
    fn a_complete_apply_reports_every_file_it_wrote() {
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&sample(Outcome::Applied))).expect("valid JSON");
        assert_eq!(json["outcome"], "applied");
        assert_eq!(json["applied"], true);
        assert_eq!(json["files_written"], 1);
        assert_eq!(
            json["written_sites"], json["rewritten_sites"],
            "a complete apply writes everything it planned"
        );
    }

    #[test]
    fn a_preview_wrote_nothing_and_says_so_in_json() {
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&sample(Outcome::Preview))).expect("valid JSON");
        assert_eq!(json["outcome"], "preview");
        assert_eq!(json["applied"], false);
        assert_eq!(json["files_written"], 0);
    }

    #[test]
    fn json_report_omits_the_rewritten_file_bodies() {
        // The report is a summary, not a payload: dumping every rewritten file
        // into it would make `--json` unusable on a real app.
        let out = render_json(&sample(Outcome::Preview));
        assert!(!out.contains("\"updated\""), "{out}");
    }
}
