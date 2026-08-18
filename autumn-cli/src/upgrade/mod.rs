//! `autumn upgrade` — apply each release's mechanical app-code migrations
//! (issue #1629).
//!
//! Autumn ships every 2-4 weeks and, pre-1.0, most releases can break existing
//! apps. Issue #1588 made a written migration guide a release gate, and #1593
//! reconciles framework-owned scaffold files — but both leave the user's own
//! Rust code out of bounds, so a purely mechanical rename like 0.6.0's
//! `with_pool` -> `with_pool_untracked` still had to be hand-applied call site
//! by call site from prose.
//!
//! This command closes that half. For each release between the app's recorded
//! `autumn-web` version and the target, it applies that release's
//! machine-applyable migrations (see [`migrations`]) to the app's own source,
//! and reports everything it could not safely rewrite with `file:line` and a
//! guide link rather than guessing.
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
//! `autumn upgrade` is also where #1593's scaffold reconciliation lands when it
//! ships; this slice is the app-code step only.

pub mod diff;
pub mod engine;
pub mod migrations;

use std::path::{Path, PathBuf};

use migrations::{AppMigration, Version};

/// Options for `autumn upgrade`.
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
    /// Total rewritten sites across every file.
    pub fn rewritten_sites(&self) -> usize {
        self.files.iter().map(|f| f.sites.len()).sum()
    }
}

/// Directory names never scanned: build output and vendored third-party
/// sources are not the app's own code, and rewriting them is at best wasted
/// work and at worst a corrupted dependency.
const SKIPPED_DIRS: &[&str] = &["target", "vendor", "node_modules", "dist", "tmp"];

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
        let _ = writeln!(out, "          {}", migration.guide);
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
        let _ = writeln!(out, "      {}", entry.guide);
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
            let written_sites: usize = report
                .files
                .iter()
                .take(written)
                .map(|file| file.sites.len())
                .sum();
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
                })
            })
            .collect(),
    )
}

/// Render the machine-readable report.
pub fn render_json(report: &Report) -> String {
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
        "rewritten_sites": report.rewritten_sites(),
        "migrations": migrations_json(&report.migrations),
        "files": files,
        "review": manual_json(&report.review),
        "manual": manual_json(&report.manual),
        "skipped": report.skipped.iter().map(|(path, reason)| serde_json::json!({
            "path": path,
            "reason": reason,
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
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

    let mut lowest: Option<(Version, String)> = None;
    for directory in std::iter::once(root.to_path_buf()).chain(member_manifests(root)) {
        let requirement = match crate::doctor::read_autumn_web_dependency_at(&directory) {
            AutumnWebDependency::Absent => continue,
            // Declared as a path or git dependency: this crate is on *some*
            // version of autumn-web and the manifest does not say which.
            // Letting a sibling decide the floor would migrate this crate's
            // source against a version nobody checked — and a vendored
            // checkout is exactly the population the 0.6.0 rename affects.
            AutumnWebDependency::WithoutVersion => return None,
            AutumnWebDependency::Version(requirement) => requirement,
        };
        // A manifest that *declares* `autumn-web` with a requirement carrying no
        // usable floor makes the whole answer a guess. Dropping it and taking
        // some other manifest's version would silently skip that member —
        // exactly what refusing an upper-bound-only requirement was meant to
        // prevent. Fail detection and let the caller ask for `--from`.
        let version = migrations::parse_version_req(&requirement)?;
        if lowest.as_ref().is_none_or(|(lowest, _)| version < *lowest) {
            lowest = Some((version, requirement));
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
    collect_manifests(root, root, &mut found);
    found.sort();
    found
}

fn collect_manifests(root: &Path, dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.join("Cargo.toml").is_file() && path != root {
            found.push(path.clone());
        }
        collect_manifests(root, &path, found);
    }
}

/// Every `.rs` file under `root` that belongs to the app, in a stable order.
fn app_sources(root: &Path) -> SourceScan {
    let mut scan = SourceScan::default();
    collect_sources(root, &mut scan);
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
    /// Symlinked `.rs` files, reported rather than followed.
    symlinks: Vec<PathBuf>,
    /// Directories the walk could not read.
    unreadable: Vec<PathBuf>,
}

/// Collect `.rs` files, recording what was deliberately or accidentally left
/// out so the caller can report it rather than drop it silently.
fn collect_sources(dir: &Path, scan: &mut SourceScan) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        scan.unreadable.push(dir.to_path_buf());
        return;
    };
    for entry in entries.flatten() {
        // `file_type` does not follow symlinks, so a link into `target/`, a
        // link out of the project, or a cycle is neither descended into nor
        // rewritten. A linked `.rs` file is still *reported*: quietly leaving a
        // source file out of the migration is the failure mode this command
        // exists to remove.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_sources(&path, scan);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            if file_type.is_file() {
                scan.files.push(path);
            } else if file_type.is_symlink() {
                scan.symlinks.push(path);
            }
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

/// Build the report for `root` without writing anything.
fn plan(root: &Path, from: Version, to: Version) -> Report {
    plan_with(root, from, to, &migrations::migrations_between(from, to))
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
        let rewrite = match engine::rewrite_source_for_releases(&source, selected) {
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
    temp.persist(&file.absolute)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(())
}

/// Run `autumn upgrade` against `root`, returning the process exit code.
///
/// `0` on any completed scan — including one that found sites only a human can
/// take, or files it could not parse, both of which the report names. `1` when
/// the apply step failed partway through, `2` for a bad argument or an
/// unreadable scan root.
#[must_use]
pub fn run_in(root: &Path, opts: &UpgradeOptions) -> i32 {
    if opts.list {
        let all: Vec<&'static AppMigration> = migrations::app_migrations().iter().collect();
        if opts.json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "migrations": migrations_json(&all) })
                )
                .unwrap_or_else(|_| "{}".to_owned())
            );
        } else {
            println!("Shipped app-code migrations ({}):", all.len());
            for migration in &all {
                println!(
                    "  {:<6}  {}  {}",
                    migration.confidence.label(),
                    migration.id,
                    migration.title
                );
                println!("          {}", migration.guide);
            }
        }
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

    // A path typo must not read as "your app is already migrated" — the same
    // rule `autumn a11y verify` states for its scan root.
    if !root.is_dir() {
        eprintln!(
            "autumn upgrade: `{}` is not a readable directory.",
            root.display()
        );
        return 2;
    }

    let mut report = plan(root, from, to);
    let mut failure = None;
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

    if opts.json {
        println!("{}", render_json(&report));
    } else {
        print!("{}", render_text(&report));
    }

    if let Some(WriteFailure { path, error, .. }) = failure {
        eprintln!(
            "autumn upgrade: failed while writing {path}: {error}\n\
             Files listed before it in the report above were already written; \
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
        Version {
            major,
            minor,
            patch,
        }
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
    }

    #[test]
    fn a_complete_apply_reports_every_file_it_wrote() {
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&sample(Outcome::Applied))).expect("valid JSON");
        assert_eq!(json["outcome"], "applied");
        assert_eq!(json["applied"], true);
        assert_eq!(json["files_written"], 1);
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
