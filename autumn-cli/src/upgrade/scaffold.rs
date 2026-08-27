//! Framework-owned scaffold-file reconciliation for `autumn upgrade`
//! (issue #1593).
//!
//! `autumn new` writes a dozen framework-owned files into every project —
//! `Dockerfile`, `build.rs`, `autumn.toml`, the toolchain and style configs, the
//! CI workflow — and those templates keep evolving. Bumping `autumn-web` in
//! `Cargo.toml` updates the library, not the project skeleton, so an app
//! scaffolded on 0.5 keeps 0.5-vintage project files forever. This module is
//! the reconciler: it renders the *current* release's scaffold in memory,
//! compares it against what is on disk, and classifies every difference.
//!
//! # The question that decides everything
//!
//! "May I overwrite this file?" is really "did the developer touch it?", and
//! neither the file's contents nor its timestamp can answer that. So
//! [`autumn new`](crate::new) records a digest of every framework-owned file as
//! it writes it — the manifest at [`MANIFEST_PATH`] — and this module treats
//! that digest as the merge base:
//!
//! - on-disk bytes still match the recorded digest → the framework wrote them
//!   and nobody has since, so the new template may replace them ([`Status::Update`]);
//! - they do not match, or nothing was recorded → the developer may have edited
//!   the file, so it is a [`Status::Conflict`] and is never written.
//!
//! An app scaffolded before this feature existed has no manifest and therefore
//! no baseline. That is deliberately not an error: files it is missing entirely
//! are still offered ([`Status::Add`]), and everything else is a conflict for
//! review. Best effort, never a silent overwrite.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::new::{GenerateOptions, TemplateVars, framework_owned_files};

/// Project-relative location of the scaffold provenance manifest.
///
/// Under `.autumn/` rather than at the project root: it is machine-written
/// bookkeeping, and the root is the one directory whose listing a developer
/// reads. It is meant to be committed — its whole value is being the baseline a
/// *later* checkout compares against.
pub const MANIFEST_PATH: &str = ".autumn/scaffold.toml";

/// Line endings normalised, so a CRLF checkout is not mistaken for an edit.
///
/// `git config core.autocrlf true` rewrites every text file on checkout. Hashing
/// the bytes as they sit on disk would then report that the developer had
/// personally rewritten all twelve framework-owned files — turning the one
/// upgrade path into a wall of conflicts on exactly the platform least able to
/// diagnose it. The template renderer normalises the same way, so both sides of
/// every comparison are LF.
fn normalize(contents: &str) -> String {
    contents.replace("\r\n", "\n")
}

/// The digest recorded for one framework-owned file.
///
/// SHA-256 over the LF-normalised text, hex encoded. Not a cryptographic
/// commitment to anything — it only has to distinguish "these are the bytes
/// Autumn wrote" from "these are not", and it must be stable across hosts.
#[must_use]
pub fn digest(contents: &str) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(normalize(contents).as_bytes()))
}

/// The on-disk shape of [`MANIFEST_PATH`].
///
/// A flat DTO rather than [`Manifest`] itself: [`GenerateOptions`] is the CLI's
/// argument struct and should stay free to grow fields that mean nothing to a
/// file on disk.
///
/// One bool per scaffold flag, mirroring [`GenerateOptions`]: they are
/// independent switches with no useful grouping, and naming them individually
/// is what makes the file readable to the human whose project it describes.
#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize, Deserialize)]
struct ManifestFile {
    version: String,
    flavor: String,
    #[serde(default)]
    i18n: bool,
    #[serde(default)]
    seed: bool,
    #[serde(default)]
    daemon: bool,
    #[serde(default)]
    bundled_pg: bool,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

const FLAVOR_API: &str = "api";
const FLAVOR_FULLSTACK: &str = "fullstack";

/// What `autumn new` recorded about the scaffold it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The `autumn-cli` release whose scaffold produced these files.
    pub version: String,
    /// The flags that release was invoked with, insofar as they change which
    /// framework-owned files exist and what they contain.
    pub options: GenerateOptions,
    /// Project-relative path → digest of the file as Autumn wrote it.
    pub digests: BTreeMap<String, String>,
}

impl Manifest {
    /// The manifest describing `files` as just written.
    #[must_use]
    pub fn for_files(
        version: &str,
        options: GenerateOptions,
        files: &BTreeMap<&'static str, String>,
    ) -> Self {
        Self {
            version: version.to_owned(),
            options,
            digests: files
                .iter()
                .map(|(path, contents)| ((*path).to_owned(), digest(contents)))
                .collect(),
        }
    }

    /// The manifest text, header comment included.
    #[must_use]
    pub fn render(&self) -> String {
        let file = ManifestFile {
            version: self.version.clone(),
            flavor: if self.options.with_api {
                FLAVOR_API
            } else {
                FLAVOR_FULLSTACK
            }
            .to_owned(),
            i18n: self.options.with_i18n,
            seed: self.options.with_seed,
            daemon: self.options.with_daemon,
            bundled_pg: self.options.with_bundled_pg,
            files: self.digests.clone(),
        };
        // Serialization cannot fail for this shape (plain strings, bools, and a
        // string map), but a panic here would abort a scaffold over
        // bookkeeping, so the failure degrades to a manifest-free project —
        // which the reconciler already handles as "no baseline".
        let body = toml::to_string(&file).unwrap_or_default();
        format!(
            "# Scaffold provenance for `autumn upgrade` (issue #1593).\n\
             #\n\
             # Written by `autumn new` and refreshed by `autumn upgrade --apply`.\n\
             # Commit it: it records which release's scaffold produced this project's\n\
             # framework-owned files, and a digest of each file as Autumn wrote it, so a\n\
             # later upgrade can tell \"you edited this\" apart from \"the template moved\"\n\
             # and never overwrite your work.\n\
             #\n\
             # Deleting or hand-editing this file costs precision, not correctness: an\n\
             # upgrade with no baseline treats every changed file as a conflict to review.\n\
             \n{body}"
        )
    }

    /// Parse manifest text, or `None` if it is not a manifest.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let file: ManifestFile = toml::from_str(text).ok()?;
        Some(Self {
            version: file.version,
            options: GenerateOptions {
                with_api: file.flavor == FLAVOR_API,
                with_i18n: file.i18n,
                with_seed: file.seed,
                with_daemon: file.daemon,
                with_bundled_pg: file.bundled_pg,
            },
            digests: file.files,
        })
    }

    /// Load the manifest under `root`, or `None` when there is not a readable
    /// one.
    ///
    /// Absent and unreadable collapse into the same answer on purpose. Both mean
    /// "no baseline", and the reconciler's response to no baseline — conflict
    /// everything that differs — is already the safe one; failing the whole
    /// command over a corrupt bookkeeping file would be strictly worse than
    /// upgrading conservatively.
    #[must_use]
    pub fn load(root: &Path) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(root.join(MANIFEST_PATH)).ok()?)
    }

    /// Write the manifest under `root`, creating `.autumn/` if needed.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = root.join(MANIFEST_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.render())
    }
}

/// Why a file cannot be written without a human looking at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The file differs from the digest Autumn recorded when it wrote it.
    Edited,
    /// Nothing was recorded for this file, so "untouched" cannot be proven.
    /// Every file in a project scaffolded before the manifest existed is here.
    NoBaseline,
}

impl ConflictReason {
    /// The one-line explanation printed next to the file.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Edited => "you changed this since it was scaffolded",
            Self::NoBaseline => "no recorded baseline, so an edit cannot be ruled out",
        }
    }
}

/// What reconciling one framework-owned file would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Already identical to the current release's scaffold.
    UpToDate,
    /// The current scaffold has this file and the project does not — including
    /// every file introduced by a release later than the one that scaffolded
    /// the project.
    Add,
    /// The template moved and the project's copy is provably untouched.
    Update,
    /// The template moved and the project's copy may not be Autumn's any more.
    Conflict(ConflictReason),
    /// Autumn wrote this file once and the developer removed it. Reported so
    /// the drift is visible, never written back: deleting it was a decision.
    Removed,
}

impl Status {
    /// Whether `--apply` writes this file.
    #[must_use]
    pub const fn is_applied(self) -> bool {
        matches!(self, Self::Add | Self::Update)
    }

    /// Whether this counts as drift for `--check`.
    ///
    /// [`Status::Removed`] does not. A CI gate that can never go green again
    /// because someone deliberately deleted `.env.example` teaches people to
    /// delete the gate.
    #[must_use]
    pub const fn is_drift(self) -> bool {
        matches!(self, Self::Add | Self::Update | Self::Conflict(_))
    }

    /// The column label in the human report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UpToDate => "current",
            Self::Add => "add",
            Self::Update => "update",
            Self::Conflict(_) => "conflict",
            Self::Removed => "removed",
        }
    }

    /// The short reason printed beside the label.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::UpToDate => "matches the current scaffold",
            Self::Add => "this release's scaffold has it; your project does not",
            Self::Update => "the template changed and your copy is untouched",
            Self::Conflict(reason) => reason.describe(),
            Self::Removed => "you deleted this; it is not restored",
        }
    }
}

/// One framework-owned file, reconciled.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Project-relative, `/`-separated.
    pub path: String,
    pub status: Status,
    /// What the current release's scaffold renders for this file.
    pub template: String,
    /// The on-disk text this classification was computed from, LF-normalised.
    /// `None` when the file is absent. Kept so the apply step can prove the
    /// file has not changed since the plan was made.
    pub current: Option<String>,
    /// Rendered preview diff; empty when there is nothing to show.
    pub diff: String,
    /// Where the file would be written.
    pub absolute: PathBuf,
}

/// The name to interpolate into the rendered scaffold.
///
/// Read from `Cargo.toml`, not from the directory name: `autumn.toml` carries
/// the project name in its telemetry and database examples, and a checkout in a
/// differently-named directory (a CI workspace, a `git worktree`, a rename)
/// would otherwise report the file as edited on every run.
fn project_name(root: &Path) -> String {
    std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            table
                .get("package")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        })
        .or_else(|| {
            root.canonicalize()
                .ok()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "app".to_owned())
}

/// The scaffold options to render against, recorded if possible and inferred if
/// not.
///
/// Inference only has to be good enough to pick the right *file set* and the
/// right conditional blocks; when it is wrong the file shows up as a conflict,
/// which is the outcome a project without a manifest gets anyway.
#[must_use]
pub fn resolve_options(root: &Path, manifest: Option<&Manifest>) -> GenerateOptions {
    if let Some(manifest) = manifest {
        return manifest.options;
    }
    let autumn_toml = std::fs::read_to_string(root.join("autumn.toml")).unwrap_or_default();
    let bundled_pg = autumn_toml.contains("Managed local Postgres");
    GenerateOptions {
        // The API scaffold creates neither, and the fullstack one always
        // creates both, so their joint absence is the signal. Requiring both to
        // be missing keeps a fullstack project that merely deleted its Tailwind
        // config from being misread as an API project and losing `input.css`.
        with_api: !root.join("static").is_dir() && !root.join("tailwind.config.js").exists(),
        with_i18n: root.join("i18n").is_dir(),
        // Never affects a framework-owned file (the seed binary lives under
        // `src/`, which is out of bounds), so there is nothing to infer.
        with_seed: false,
        with_daemon: bundled_pg || autumn_toml.contains("this app uses no database"),
        with_bundled_pg: bundled_pg,
    }
}

/// The current release's framework-owned files, rendered for the project at
/// `root`.
#[must_use]
pub fn current_files(root: &Path, options: GenerateOptions) -> BTreeMap<&'static str, String> {
    let name = project_name(root);
    let crate_name = name.replace('-', "_");
    let vars = TemplateVars {
        project_name: &name,
        crate_name: &crate_name,
        autumn_version: env!("CARGO_PKG_VERSION"),
        rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
    };
    framework_owned_files(&vars, options)
}

/// Reconcile `files` against what is on disk under `root`.
#[must_use]
pub fn classify(
    root: &Path,
    files: &BTreeMap<&'static str, String>,
    manifest: Option<&Manifest>,
) -> Vec<Entry> {
    let recorded = |path: &str| manifest.and_then(|manifest| manifest.digests.get(path));

    let mut entries: Vec<Entry> = files
        .iter()
        .map(|(path, template)| {
            let absolute = root.join(path);
            let current = std::fs::read_to_string(&absolute)
                .ok()
                .map(|c| normalize(&c));
            let (status, diff) = match &current {
                // Absent. Autumn wrote it once and it is gone → a deliberate
                // deletion. Never wrote it → this release added it.
                None if recorded(path).is_some() => (Status::Removed, String::new()),
                None => (Status::Add, super::diff::render("", template)),
                Some(current) if *current == normalize(template) => {
                    (Status::UpToDate, String::new())
                }
                Some(current) => {
                    let status = match recorded(path) {
                        Some(baseline) if *baseline == digest(current) => Status::Update,
                        Some(_) => Status::Conflict(ConflictReason::Edited),
                        None => Status::Conflict(ConflictReason::NoBaseline),
                    };
                    (status, super::diff::render(current, template))
                }
            };
            Entry {
                path: (*path).to_owned(),
                status,
                template: normalize(template),
                current,
                diff,
                absolute,
            }
        })
        .collect();

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Whether `root` looks like an Autumn project at all.
///
/// `autumn.toml` is the framework's own config file and every scaffold writes
/// one; the manifest alone also counts, so a project that deleted its
/// `autumn.toml` can still be reconciled. Outside such a directory the
/// reconciler stays silent rather than offering to seed twelve files into
/// whatever the developer happens to be standing in.
#[must_use]
pub fn is_project(root: &Path) -> bool {
    root.join("autumn.toml").is_file() || root.join(MANIFEST_PATH).is_file()
}

/// The upgrade guide for the release being upgraded to.
///
/// Resolved against the shipped migration registry rather than composed from
/// the version blindly: a release with no breaking change ships no guide, and a
/// summary that links a 404 at the exact moment it tells you to go read
/// something is worse than one that links the index.
#[must_use]
pub fn release_guide(target: &str) -> String {
    let release = super::migrations::parse_version_req(target);
    let file =
        release.map(|version| format!("docs/migrations/{}.{}.0.md", version.major, version.minor));
    let known = file.as_ref().is_some_and(|file| {
        super::migrations::app_migrations()
            .iter()
            .any(|migration| migration.guide.starts_with(file.as_str()))
    });
    match file {
        Some(file) if known => super::migrations::guide_url(&file),
        _ => super::migrations::guide_url("docs/migrations/README.md"),
    }
}

/// Everything one scaffold reconciliation found.
#[derive(Debug, Clone)]
pub struct ScaffoldReport {
    /// The project this report is about.
    pub root: PathBuf,
    /// The release recorded as having scaffolded this project, when one was.
    pub baseline: Option<String>,
    /// The release being reconciled to.
    pub target: String,
    /// Whether a provenance manifest was found. Without one every changed file
    /// is a conflict, and the report says why.
    pub has_manifest: bool,
    /// The scaffold options this project was reconciled against — recorded, or
    /// inferred when there is no manifest.
    pub options: GenerateOptions,
    /// Every framework-owned file, up-to-date ones included.
    pub entries: Vec<Entry>,
    /// What the apply step actually did.
    pub outcome: super::Outcome,
    /// The release's upgrade guide.
    pub guide: String,
}

impl ScaffoldReport {
    /// Whether anything a developer can act on has drifted.
    #[must_use]
    pub fn drifted(&self) -> bool {
        drifted(&self.entries)
    }

    /// Entries `--apply` would write, in the order it writes them.
    #[must_use]
    pub fn applicable(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.status.is_applied())
            .collect()
    }

    /// Entries needing a human.
    #[must_use]
    pub fn conflicts(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.status, Status::Conflict(_)))
            .collect()
    }

    /// Every entry that is not already current, in report order.
    #[must_use]
    pub fn changed(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.status != Status::UpToDate)
            .collect()
    }

    /// The manifest to record after an apply.
    ///
    /// Digests are refreshed for every file that is now the current scaffold,
    /// and left untouched for the ones that are not: a conflict keeps its old
    /// baseline so the next run still knows the developer edited it, and a
    /// deleted file keeps its entry so it is still reported as *removed*
    /// rather than silently re-offered.
    fn next_manifest(&self, previous: Option<&Manifest>) -> Manifest {
        let mut digests = previous.map(|m| m.digests.clone()).unwrap_or_default();
        for entry in &self.entries {
            if entry.status.is_applied() || entry.status == Status::UpToDate {
                digests.insert(entry.path.clone(), digest(&entry.template));
            }
        }
        // The baseline moves only once nothing is left to reconcile. Recording
        // the target while conflicts stand would tell the next run — and the
        // developer reading it — that this upgrade is finished.
        let version = if self.conflicts().is_empty() {
            self.target.clone()
        } else {
            previous.map_or_else(|| self.target.clone(), |m| m.version.clone())
        };
        Manifest {
            version,
            options: self.options,
            digests,
        }
    }
}

/// A file the apply step refused or could not write.
#[derive(Debug, Clone)]
pub struct WriteFailure {
    /// Project-relative path of the file that stopped the run.
    pub path: String,
    /// What went wrong, ready to print.
    pub error: String,
    /// How many files were written before it.
    pub written: usize,
}

/// Plan a reconciliation of the project at `root` against release `target`.
#[must_use]
pub fn plan(root: &Path, target: &str) -> ScaffoldReport {
    let manifest = Manifest::load(root);
    let options = resolve_options(root, manifest.as_ref());
    ScaffoldReport {
        root: root.to_path_buf(),
        options,
        baseline: manifest.as_ref().map(|m| m.version.clone()),
        target: target.to_owned(),
        has_manifest: manifest.is_some(),
        entries: classify(root, &current_files(root, options), manifest.as_ref()),
        outcome: super::Outcome::Preview,
        guide: release_guide(target),
    }
}

/// Write the additions and updates in `report`, then refresh the manifest.
///
/// Each file is re-read immediately before it is written and compared against
/// what the plan was computed from. A file something else changed in between —
/// a formatter, a code generator, an editor saving — is refused rather than
/// overwritten with a decision made about different bytes. `report.outcome` is
/// updated either way, so a caller that ignores the error still reports the
/// truth about what reached disk.
///
/// # Errors
///
/// Returns the file that stopped the run and how many were written before it.
pub fn apply(report: &mut ScaffoldReport) -> Result<(), WriteFailure> {
    for (written, entry) in report
        .entries
        .iter()
        .filter(|entry| entry.status.is_applied())
        .enumerate()
    {
        if let Err(error) = write_one(entry) {
            report.outcome = super::Outcome::Partial { written };
            return Err(WriteFailure {
                path: entry.path.clone(),
                error,
                written,
            });
        }
    }

    // The manifest is bookkeeping, so a failure to write it must not read as a
    // failure to upgrade: the files are already correct and the next run simply
    // has a staler baseline.
    let previous = Manifest::load(&report.root);
    let _ = report.next_manifest(previous.as_ref()).save(&report.root);

    report.outcome = super::Outcome::Applied;
    Ok(())
}

/// Write one entry, refusing to clobber anything that moved since the plan.
fn write_one(entry: &Entry) -> Result<(), String> {
    let on_disk = std::fs::read_to_string(&entry.absolute)
        .ok()
        .map(|text| normalize(&text));
    if on_disk != entry.current {
        return Err(match entry.current {
            None => "a file appeared here after the preview was computed; \
                     it was left exactly as it is"
                .to_owned(),
            Some(_) => "this file changed after the preview was computed; \
                        it was left exactly as it is"
                .to_owned(),
        });
    }
    if let Some(parent) = entry.absolute.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&entry.absolute, &entry.template).map_err(|error| error.to_string())
}

/// The human report.
#[must_use]
pub fn render_text(report: &ScaffoldReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let from = report
        .baseline
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let _ = writeln!(
        out,
        "\nScaffold files ({from} -> {target})",
        target = report.target
    );

    let changed = report.changed();
    if changed.is_empty() {
        let _ = writeln!(
            out,
            "  Your framework-owned files are up to date with this release."
        );
        let _ = writeln!(out, "  Upgrade guide: {}", report.guide);
        return out;
    }

    if !report.has_manifest {
        let _ = writeln!(
            out,
            "  This project predates scaffold provenance, so there is no record of\n  \
             what Autumn originally wrote. Files it is missing are offered; every\n  \
             file that differs is a conflict for you to review."
        );
    }

    let width = changed.iter().map(|e| e.path.len()).max().unwrap_or(0);
    let _ = writeln!(out, "\n  {} file(s) differ:", changed.len());
    for entry in &changed {
        let _ = writeln!(
            out,
            "  {:<9} {:<width$}  {}",
            entry.status.label(),
            entry.path,
            entry.status.describe(),
        );
    }

    for entry in &changed {
        if entry.diff.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{} ({})", entry.path, entry.status.label());
        out.push_str(&entry.diff);
    }

    let applicable = report.applicable().len();
    let conflicts = report.conflicts().len();
    let _ = writeln!(out);
    match report.outcome {
        // Nothing to apply is its own message. Pointing someone at `--apply`
        // when every remaining difference is a conflict sends them to run a
        // command that would do nothing at all.
        super::Outcome::Preview if applicable == 0 => {
            let _ = writeln!(
                out,
                "{conflicts} conflict(s) need review; nothing here can be written for you."
            );
            let _ = writeln!(
                out,
                "Take what you want from the diffs above; `git diff` shows your edits and\n\
                 `git checkout -- <path>` puts any one file back."
            );
        }
        super::Outcome::Preview => {
            let _ = writeln!(
                out,
                "{applicable} file(s) would be written; {conflicts} conflict(s) need review."
            );
            let _ = writeln!(
                out,
                "Nothing was written. Re-run with `--apply` to take the writable ones, then\n\
                 review with `git diff` -- `git checkout -- <path>` puts any one file back."
            );
        }
        super::Outcome::Applied => {
            let _ = writeln!(
                out,
                "{applicable} file(s) written; {conflicts} conflict(s) left for you."
            );
            let _ = writeln!(
                out,
                "Review with `git diff`; undo any single file with `git checkout -- <path>`."
            );
        }
        super::Outcome::Partial { written } => {
            let _ = writeln!(
                out,
                "{written} of {applicable} file(s) written before the run stopped."
            );
            let _ = writeln!(
                out,
                "Review with `git diff`; undo any single file with `git checkout -- <path>`."
            );
        }
    }
    if conflicts > 0 {
        let _ = writeln!(
            out,
            "Conflicts are never overwritten. Compare each against this release's\n\
             scaffold above, take what you want, and re-run to confirm."
        );
    }
    let _ = writeln!(out, "Upgrade guide: {}", report.guide);
    out
}

/// The machine-readable report, for CI.
#[must_use]
pub fn json(report: &ScaffoldReport) -> serde_json::Value {
    serde_json::json!({
        "baseline": report.baseline,
        "target": report.target,
        "has_manifest": report.has_manifest,
        "outcome": report.outcome.label(),
        "drift": report.drifted(),
        "written": report.applicable().len(),
        "conflicts": report.conflicts().len(),
        "guide": report.guide,
        "files": report
            .entries
            .iter()
            .map(|entry| serde_json::json!({
                "path": entry.path,
                "status": entry.status.label(),
                "reason": entry.status.describe(),
                "applied": entry.status.is_applied(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Whether any entry is drift a developer can act on.
#[must_use]
pub fn drifted(entries: &[Entry]) -> bool {
    entries.iter().any(|entry| entry.status.is_drift())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    use crate::new::GenerateOptions;
    use crate::upgrade::Outcome;

    fn write(root: &std::path::Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // --- provenance manifest ---

    #[test]
    fn manifest_round_trips_through_toml() {
        let mut digests = BTreeMap::new();
        digests.insert("clippy.toml".to_owned(), digest("a\n"));
        digests.insert(".github/workflows/ci.yml".to_owned(), digest("b\n"));
        let manifest = Manifest {
            version: "0.7.0".to_owned(),
            options: GenerateOptions {
                with_api: true,
                with_i18n: true,
                ..GenerateOptions::default()
            },
            digests,
        };

        let parsed = Manifest::parse(&manifest.render()).expect("round trip");
        assert_eq!(parsed.version, manifest.version);
        assert_eq!(parsed.options, manifest.options);
        assert_eq!(parsed.digests, manifest.digests);
    }

    #[test]
    fn manifest_keys_with_dots_and_slashes_survive_the_round_trip() {
        // `.github/workflows/ci.yml` is a TOML *key* here; quoted wrong it
        // becomes a nested table and the digest is lost — which silently
        // downgrades that file to "no baseline" forever.
        let mut digests = BTreeMap::new();
        digests.insert(".github/workflows/ci.yml".to_owned(), digest("x"));
        digests.insert(".env.example".to_owned(), digest("y"));
        digests.insert("static/css/input.css".to_owned(), digest("z"));
        let manifest = Manifest {
            version: "0.7.0".to_owned(),
            options: GenerateOptions::default(),
            digests: digests.clone(),
        };
        assert_eq!(
            Manifest::parse(&manifest.render()).unwrap().digests,
            digests
        );
    }

    #[test]
    fn digest_ignores_line_ending_style() {
        // A Windows checkout (git autocrlf) must not read as "the developer
        // rewrote every framework file".
        assert_eq!(digest("a\r\nb\r\n"), digest("a\nb\n"));
        assert_ne!(digest("a\n"), digest("b\n"));
    }

    #[test]
    fn a_missing_or_unparsable_manifest_is_absent_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(Manifest::load(tmp.path()).is_none());
        write(tmp.path(), MANIFEST_PATH, "this is not : toml [[[");
        assert!(Manifest::load(tmp.path()).is_none());
    }

    #[test]
    fn manifest_is_written_and_read_back_from_disk() {
        let tmp = TempDir::new().unwrap();
        let mut digests = BTreeMap::new();
        digests.insert("clippy.toml".to_owned(), digest("a\n"));
        let manifest = Manifest {
            version: "0.7.0".to_owned(),
            options: GenerateOptions::default(),
            digests,
        };
        manifest.save(tmp.path()).unwrap();
        let loaded = Manifest::load(tmp.path()).expect("written manifest loads");
        assert_eq!(loaded.digests, manifest.digests);
        assert_eq!(loaded.version, "0.7.0");
    }

    // --- classification ---

    /// A project whose framework-owned files are exactly the current scaffold.
    fn scaffolded(opts: GenerateOptions) -> TempDir {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\n",
        );
        let files = current_files(tmp.path(), opts);
        for (path, contents) in &files {
            write(tmp.path(), path, contents);
        }
        Manifest::for_files("0.7.0", opts, &files)
            .save(tmp.path())
            .unwrap();
        tmp
    }

    fn status_of<'a>(entries: &'a [Entry], path: &str) -> &'a Status {
        &entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "no entry for {path}: {:?}",
                    entries.iter().map(|e| &e.path).collect::<Vec<_>>()
                )
            })
            .status
    }

    #[test]
    fn a_freshly_scaffolded_project_has_no_drift() {
        let tmp = scaffolded(GenerateOptions::default());
        let entries = plan_in(tmp.path()).entries;
        assert!(
            entries.iter().all(|entry| entry.status == Status::UpToDate),
            "{:?}",
            entries
                .iter()
                .filter(|e| e.status != Status::UpToDate)
                .map(|e| (&e.path, &e.status))
                .collect::<Vec<_>>()
        );
        assert!(!drifted(&entries));
    }

    #[test]
    fn a_file_the_old_release_never_generated_is_offered_as_an_addition() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rust-toolchain.toml")).unwrap();
        // The manifest is the *old* release's: it never knew this file.
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rust-toolchain.toml");
        manifest.version = "0.5.0".to_owned();
        manifest.save(tmp.path()).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "rust-toolchain.toml"), &Status::Add);
        assert!(drifted(&entries));
    }

    #[test]
    fn an_untouched_file_whose_template_moved_is_an_update() {
        let tmp = scaffolded(GenerateOptions::default());
        // Simulate "the template moved": record the digest of what is on disk,
        // then change the on-disk file to something the *old* template said
        // while keeping its recorded digest consistent with it.
        let old = "# an older release's clippy.toml\n";
        write(tmp.path(), "clippy.toml", old);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(old));
        manifest.save(tmp.path()).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "clippy.toml"), &Status::Update);
    }

    #[test]
    fn a_file_the_developer_edited_is_a_conflict_never_an_update() {
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "Dockerfile", "FROM scratch\n# my own build\n");

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(
            status_of(&entries, "Dockerfile"),
            &Status::Conflict(ConflictReason::Edited)
        );
    }

    #[test]
    fn a_project_with_no_manifest_gets_conflicts_not_silent_overwrites() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join(MANIFEST_PATH)).unwrap();
        write(tmp.path(), "clippy.toml", "# pre-manifest project\n");

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(
            status_of(&entries, "clippy.toml"),
            &Status::Conflict(ConflictReason::NoBaseline)
        );
        // ...but a file it simply never had is still offered.
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "rustfmt.toml"), &Status::Add);
    }

    #[test]
    fn a_file_the_developer_deleted_is_reported_but_not_restored() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join(".env.example")).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, ".env.example"), &Status::Removed);
        assert!(
            !entries
                .iter()
                .any(|entry| entry.path == ".env.example" && entry.status.is_applied()),
            "a deliberate deletion must never be written back"
        );
    }

    #[test]
    fn classification_never_names_a_path_under_src() {
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "src/main.rs", "fn main() {}\n");
        for entry in plan_in(tmp.path()).entries {
            assert!(!entry.path.starts_with("src/"), "{}", entry.path);
        }
    }

    #[test]
    fn api_projects_are_detected_without_a_manifest() {
        let tmp = scaffolded(GenerateOptions {
            with_api: true,
            ..GenerateOptions::default()
        });
        fs::remove_file(tmp.path().join(MANIFEST_PATH)).unwrap();
        let entries = plan_in(tmp.path()).entries;
        assert!(
            !entries
                .iter()
                .any(|entry| entry.path == "tailwind.config.js"),
            "an API project must not be offered the Tailwind config"
        );
    }

    #[test]
    fn i18n_projects_are_detected_without_a_manifest() {
        let tmp = scaffolded(GenerateOptions {
            with_i18n: true,
            ..GenerateOptions::default()
        });
        fs::remove_file(tmp.path().join(MANIFEST_PATH)).unwrap();
        write(tmp.path(), "i18n/en.ftl", "welcome = hi\n");
        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "autumn.toml"), &Status::UpToDate);
    }

    #[test]
    fn the_project_name_is_read_from_cargo_toml_not_the_directory() {
        // `autumn.toml` interpolates the project name; reading it from the
        // temp directory's random name would make it a conflict in every run.
        let tmp = scaffolded(GenerateOptions::default());
        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "autumn.toml"), &Status::UpToDate);
    }

    // --- report, apply, rendering ---

    fn plan_in(root: &std::path::Path) -> ScaffoldReport {
        plan(root, "0.7.0")
    }

    #[test]
    fn a_directory_without_autumn_files_is_not_a_project() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_project(tmp.path()));
        write(tmp.path(), "autumn.toml", "[server]\n");
        assert!(is_project(tmp.path()));
    }

    #[test]
    fn a_project_known_only_by_its_manifest_is_still_a_project() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            MANIFEST_PATH,
            "version = \"0.7.0\"\nflavor = \"api\"\n",
        );
        assert!(is_project(tmp.path()));
    }

    #[test]
    fn the_report_records_the_recorded_baseline_and_the_target() {
        let tmp = scaffolded(GenerateOptions::default());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = "0.5.0".to_owned();
        manifest.save(tmp.path()).unwrap();

        let report = plan_in(tmp.path());
        assert_eq!(report.baseline.as_deref(), Some("0.5.0"));
        assert_eq!(report.target, "0.7.0");
    }

    #[test]
    fn the_summary_links_the_release_upgrade_guide() {
        let tmp = scaffolded(GenerateOptions::default());
        let report = plan_in(tmp.path());
        assert!(
            report.guide.contains("docs/migrations/0.7.0.md"),
            "{}",
            report.guide
        );
        assert!(render_text(&report).contains(&report.guide));
    }

    #[test]
    fn a_target_with_no_release_guide_links_the_guide_index() {
        let tmp = scaffolded(GenerateOptions::default());
        let report = plan(tmp.path(), "9.9.0");
        assert!(
            report.guide.ends_with("docs/migrations/README.md"),
            "{}",
            report.guide
        );
    }

    #[test]
    fn preview_writes_nothing() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();

        let report = plan_in(tmp.path());
        assert_eq!(report.outcome, Outcome::Preview);
        assert!(!tmp.path().join("rustfmt.toml").exists());
        assert!(render_text(&report).contains("--apply"));
    }

    #[test]
    fn apply_writes_additions_and_updates_but_never_conflicts() {
        let tmp = scaffolded(GenerateOptions::default());

        // An addition: a file this release introduced.
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        // An update: untouched by the developer, template moved.
        let stale = "# an older release's clippy.toml\n";
        write(tmp.path(), "clippy.toml", stale);
        // A conflict: the developer's own Dockerfile.
        let mine = "FROM scratch\n# mine\n";
        write(tmp.path(), "Dockerfile", mine);

        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(stale));
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(report.outcome, Outcome::Applied);

        let files = current_files(tmp.path(), GenerateOptions::default());
        assert_eq!(
            fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap(),
            files["rustfmt.toml"]
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("clippy.toml")).unwrap(),
            files["clippy.toml"]
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("Dockerfile")).unwrap(),
            mine,
            "a conflict must survive --apply byte for byte"
        );
    }

    #[test]
    fn apply_creates_missing_parent_directories() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_dir_all(tmp.path().join(".github")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove(".github/workflows/ci.yml");
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert!(tmp.path().join(".github/workflows/ci.yml").is_file());
    }

    #[test]
    fn apply_refreshes_the_manifest_so_a_second_run_is_clean() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.version = "0.5.0".to_owned();
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        let second = plan_in(tmp.path());
        assert!(!second.drifted(), "{}", render_text(&second));
        assert_eq!(second.baseline.as_deref(), Some("0.7.0"));
    }

    #[test]
    fn apply_leaves_the_baseline_version_alone_while_conflicts_remain() {
        // Recording "you are on 0.7.0" while files are still unreconciled would
        // make the next run's report say the upgrade already happened.
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "Dockerfile", "FROM scratch\n");
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = "0.5.0".to_owned();
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(Manifest::load(tmp.path()).unwrap().version, "0.5.0");
    }

    #[test]
    fn apply_refuses_a_file_that_changed_after_the_plan_was_made() {
        let tmp = scaffolded(GenerateOptions::default());
        let stale = "# older\n";
        write(tmp.path(), "clippy.toml", stale);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(stale));
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        // Something else (a formatter, an editor) writes between plan and apply.
        write(tmp.path(), "clippy.toml", "# touched by someone else\n");

        let failure = apply(&mut report).expect_err("must refuse a changed file");
        assert_eq!(failure.path, "clippy.toml");
        assert_eq!(
            fs::read_to_string(tmp.path().join("clippy.toml")).unwrap(),
            "# touched by someone else\n"
        );
        assert!(matches!(report.outcome, Outcome::Partial { .. }));
    }

    #[test]
    fn apply_refuses_an_addition_that_appeared_after_the_plan_was_made() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        write(tmp.path(), "rustfmt.toml", "# mine, written just now\n");

        let failure = apply(&mut report).expect_err("must refuse to clobber");
        assert_eq!(failure.path, "rustfmt.toml");
        assert_eq!(
            fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap(),
            "# mine, written just now\n"
        );
    }

    #[test]
    fn the_text_report_names_every_drifted_file_with_its_status() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        write(tmp.path(), "Dockerfile", "FROM scratch\n");
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();

        let text = render_text(&plan_in(tmp.path()));
        assert!(text.contains("rustfmt.toml"), "{text}");
        assert!(text.contains("add"), "{text}");
        assert!(text.contains("Dockerfile"), "{text}");
        assert!(text.contains("conflict"), "{text}");
        // The revert path is documented in the report itself, not only in prose.
        assert!(text.contains("git diff"), "{text}");
        // Up-to-date files are not listed one by one.
        assert!(!text.contains("clippy.toml"), "{text}");
    }

    #[test]
    fn a_project_with_no_drift_says_so_without_listing_files() {
        let tmp = scaffolded(GenerateOptions::default());
        let text = render_text(&plan_in(tmp.path()));
        assert!(text.contains("up to date"), "{text}");
    }

    #[test]
    fn the_json_report_carries_a_status_per_file() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();

        let value = json(&plan_in(tmp.path()));
        assert_eq!(value["target"], "0.7.0");
        assert_eq!(value["drift"], true);
        let files = value["files"].as_array().unwrap();
        let entry = files
            .iter()
            .find(|file| file["path"] == "rustfmt.toml")
            .expect("rustfmt.toml in the json report");
        assert_eq!(entry["status"], "add");
        assert!(value["guide"].as_str().unwrap().contains("docs/migrations"));
    }

    #[test]
    fn a_report_with_only_conflicts_does_not_advertise_apply() {
        // `--apply` would write nothing here, and telling someone to run a
        // command that cannot help them is how a tool loses their attention.
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "Dockerfile", "FROM scratch\n");

        let text = render_text(&plan_in(tmp.path()));
        assert!(text.contains("conflict"), "{text}");
        assert!(!text.contains("--apply"), "{text}");
        assert!(text.contains("git diff"), "{text}");
    }
}
