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

use std::collections::{BTreeMap, BTreeSet};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// The newest release that has written digests here. Distinct from
    /// `version`, which is held back while conflicts stand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    written_by: Option<String>,
    flavor: String,
    #[serde(default)]
    i18n: bool,
    #[serde(default)]
    seed: bool,
    #[serde(default)]
    daemon: bool,
    #[serde(default)]
    bundled_pg: bool,
    /// Paths the developer has claimed as theirs. A plain list of names, on
    /// purpose: unlike the digests it is meant to be hand-editable — removing a
    /// line is how a file comes back under reconciliation.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pinned: BTreeSet<String>,
    // Last: TOML serialises a table's scalars before its sub-tables, so a
    // field declared after `files` would be emitted inside it.
    #[serde(default)]
    files: BTreeMap<String, String>,
}

/// Most tries in [`Manifest::update`].
const SWAP_ATTEMPTS: u32 = 8;

/// A swap holds the old contents in `.autumn-upgrade-<file name>.<token>.old`.
const CLAIM_SUFFIX: &str = ".old";

const FLAVOR_API: &str = "api";
const FLAVOR_FULLSTACK: &str = "fullstack";

/// What `autumn new` recorded about the scaffold it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The release this project's framework-owned files were last *fully*
    /// reconciled to — what `autumn new` wrote, or what the last conflict-free
    /// `autumn upgrade --apply` brought them to.
    ///
    /// Absent when there is no such release: an upgrade that left conflicts
    /// standing has not finished, and recording the target anyway would tell
    /// the next run — and the developer reading the file — that it had.
    pub version: Option<String>,
    /// The newest release that has written any digest in this manifest — a
    /// high-water mark, never moving backwards.
    ///
    /// Deliberately separate from [`Self::version`], which answers a different
    /// question and cannot answer this one. A conflict holds `version` back
    /// while the digests still advance to the newer templates, so an older CLI
    /// reading `version` alone sees nothing newer than itself, trusts those
    /// digests against its own older templates, and downgrades every file they
    /// describe. Which release *reconciled* the project and which release
    /// *wrote* these digests are not the same fact.
    pub written_by: Option<String>,
    /// The flags that release was invoked with, insofar as they change which
    /// framework-owned files exist and what they contain.
    pub options: GenerateOptions,
    /// Project-relative path → digest of the file as Autumn wrote it.
    pub digests: BTreeMap<String, String>,
    /// Paths the developer has accepted as their own; never reconciled.
    pub pinned: BTreeSet<String>,
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
            version: Some(version.to_owned()),
            written_by: Some(version.to_owned()),
            options,
            digests: files
                .iter()
                .map(|(path, contents)| ((*path).to_owned(), digest(contents)))
                .collect(),
            pinned: BTreeSet::new(),
        }
    }

    /// The manifest text, header comment included.
    #[must_use]
    pub fn render(&self) -> String {
        let file = ManifestFile {
            version: self.version.clone(),
            written_by: self.written_by.clone(),
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
            pinned: self.pinned.clone(),
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
            written_by: file.written_by,
            options: GenerateOptions {
                // Anything but the two flavours this CLI knows is treated as no
                // manifest at all. Falling back to `fullstack` would keep
                // trusting the recorded *digests* while rendering the wrong
                // templates against them — so an API project's `Dockerfile`,
                // untouched and matching its baseline, would classify as
                // `update` and be replaced with the fullstack one while its
                // `Cargo.toml` stayed API-shaped. A value written by a newer
                // release, or a typo, is exactly when the conservative
                // no-baseline path is wanted.
                with_api: match file.flavor.as_str() {
                    FLAVOR_API => true,
                    FLAVOR_FULLSTACK => false,
                    _ => return None,
                },
                with_i18n: file.i18n,
                with_seed: file.seed,
                with_daemon: file.daemon,
                with_bundled_pg: file.bundled_pg,
            },
            digests: file.files,
            pinned: file.pinned,
        })
        // A combination `autumn new` would have refused cannot have produced
        // this project, so the manifest is corrupt however well-formed it
        // parses. `--api` with `--daemon`, for instance, are different app
        // shapes with conflicting feature sets: trusting it would render the
        // API templates against a fullstack daemon's digests and make its
        // `Dockerfile` and `build.rs` look like safe updates. Validated with
        // `autumn new`'s own rule, so the two can never disagree about what is
        // possible.
        .filter(|manifest: &Self| crate::new::check_option_combination(manifest.options).is_ok())
        // A release string that cannot be read cannot be compared, so it cannot
        // be refused either — and a manifest left *trusted* with digests some
        // newer renderer wrote is the downgrade path all over again. Unknown
        // has to mean no baseline. Absent still does not: a project whose first
        // upgrade left conflicts standing has no `version` line at all, and its
        // digests are perfectly good.
        .filter(|manifest: &Self| {
            [manifest.version.as_deref(), manifest.written_by.as_deref()]
                .into_iter()
                .flatten()
                .all(|recorded| super::migrations::parse_version_req(recorded).is_some())
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
        // Refusing to *write* through a linked manifest was only half of it.
        // Read through one and whoever controls the target supplies the
        // digests — and a digest matching a project's current scaffold file
        // turns that file into an `update`, so the next `--apply` overwrites
        // something nobody vouched for. A manifest reachable only through a
        // link is treated as absent, which is the same conservative answer a
        // project that never had one gets.
        // Parsed from the one snapshot: a second read could land in a swap.
        match read_current(root, MANIFEST_PATH) {
            OnDisk::Text(text) => Self::parse(&text),
            _ => None,
        }
    }

    /// Write the manifest under `root`, creating `.autumn/` if needed.
    ///
    /// # Errors
    ///
    /// Fails if the path cannot be written — including when `.autumn` or the
    /// manifest itself is a symbolic link. The manifest is written by a
    /// different code path than the scaffold files and would otherwise have had
    /// none of their protection: following the link would truncate a file
    /// outside the project, invisibly to that project's own `git diff`.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        Self::update(root, |_| Ok(self.clone())).map(drop)
    }

    /// Read, change and write the manifest as one compare-and-swap.
    ///
    /// `change` gets the current manifest (`None` if absent or unparsable) and
    /// returns the new one, or an error to stop with nothing written. If another writer changes the file first, `change`
    /// runs again on the new state, so that writer's pins are kept.
    ///
    /// # Errors
    ///
    /// Fails on a symlink, unreadable text, a write error, an interrupted
    /// upgrade, or [`SWAP_ATTEMPTS`] lost swaps.
    pub fn update(
        root: &Path,
        mut change: impl FnMut(Option<Self>) -> Result<Self, String>,
    ) -> std::io::Result<Self> {
        let path = root.join(MANIFEST_PATH);
        for attempt in 1..=SWAP_ATTEMPTS {
            let (current, mode) = match read_current(root, MANIFEST_PATH) {
                OnDisk::Linked(_) => {
                    return Err(std::io::Error::other(format!(
                        "{MANIFEST_PATH} (or a directory on the way to it) is a symlink; \
                         writing through it could write outside the project"
                    )));
                }
                OnDisk::Opaque(_) => {
                    return Err(std::io::Error::other(claim_in_flight(&path).map_or_else(
                        || {
                            format!(
                                "{MANIFEST_PATH} cannot be read as text; delete it to start again"
                            )
                        },
                        |held| interrupted(&held),
                    )));
                }
                OnDisk::Absent => match claim_in_flight(&path) {
                    Some(held) => return Err(std::io::Error::other(interrupted(&held))),
                    None => (None, Publish::Create),
                },
                OnDisk::Text(text) => (Self::parse(&text), Publish::Swap(text)),
            };
            let next = change(current).map_err(std::io::Error::other)?;
            let rendered = next.render();
            // An identical rewrite would only touch the mtime.
            let unchanged = match &mode {
                Publish::Swap(text) if normalize(text) == normalize(&rendered) => {
                    Some(text.clone())
                }
                _ => None,
            };
            if let Some(text) = unchanged {
                // Still current? A writer may have replaced it since the read.
                if read_current(root, MANIFEST_PATH) == OnDisk::Text(text) {
                    return Ok(next);
                }
                back_off(attempt);
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match publish(&path, &rendered, mode) {
                Ok(()) => return Ok(next),
                Err(PublishError::Moved(_)) => back_off(attempt),
                Err(
                    PublishError::Late(error)
                    | PublishError::Retained(error)
                    | PublishError::Failed(error),
                ) => {
                    return Err(std::io::Error::other(error));
                }
            }
        }
        Err(std::io::Error::other(format!(
            "{MANIFEST_PATH} kept changing; gave up after {SWAP_ATTEMPTS} tries"
        )))
    }
}

/// What to tell the user when a swap left its claim behind.
fn interrupted(held: &Path) -> String {
    format!(
        "an interrupted upgrade left {} in place of {MANIFEST_PATH}; \
         if {MANIFEST_PATH} is still missing, rename it back. Otherwise \
         inspect it first: a newer manifest may have replaced it",
        held.display()
    )
}

/// Wait a little longer after each lost swap, so writers stop colliding.
fn back_off(attempt: u32) {
    std::thread::sleep(std::time::Duration::from_millis(5) * attempt);
}

/// A claim that a swap holds beside `path`, if one exists.
fn claim_in_flight(path: &Path) -> Option<PathBuf> {
    let prefix = claim_prefix(path)?;
    std::fs::read_dir(path.parent()?)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|held| {
            held.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                // The token has no dot, so `a.b` never claims for `a`.
                name.strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(CLAIM_SUFFIX))
                    .is_some_and(|token| !token.contains('.'))
            })
        })
}

/// The start of the claim names for `path`.
fn claim_prefix(path: &Path) -> Option<String> {
    Some(format!(
        ".autumn-upgrade-{}.",
        path.file_name()?.to_string_lossy()
    ))
}

/// Why a file cannot be written without a human looking at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The file differs from the digest Autumn recorded when it wrote it.
    Edited,
    /// Nothing was recorded for this file, so "untouched" cannot be proven.
    /// Every file in a project scaffolded before the manifest existed is here.
    NoBaseline,
    /// The file is there but could not be read as text — it is not UTF-8, or
    /// the process cannot open it. What it holds is unknown, so it is
    /// untouchable.
    Unreadable,
    /// The path, or a directory on the way to it, is a symbolic link. Writing
    /// through it would write wherever it points, which need not be inside the
    /// project at all.
    Symlink,
    /// This same run's app-code migrations cover the file. `build.rs` is both a
    /// framework-owned file and a `.rs` file the codemods scan, so the two
    /// halves can land on it in one run — and blaming the developer for an edit
    /// the tool itself makes four lines earlier would be simply false.
    ///
    /// Set from the codemods' *plan*, so a preview says the same thing the
    /// apply it is previewing will.
    MigratedThisRun,
}

impl ConflictReason {
    /// The one-line explanation printed next to the file.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Edited => "you changed this since it was scaffolded",
            Self::NoBaseline => "no recorded baseline, so an edit cannot be ruled out",
            Self::Unreadable => "on disk but unreadable as text, so its contents are unknown",
            Self::Symlink => "a symlink; writing through it could write outside the project",
            Self::MigratedThisRun => {
                "this run's app-code migrations rewrote it; reconcile it by hand"
            }
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
    /// The developer has said this file is theirs (`autumn upgrade --accept`).
    /// Reported, never written, and not drift — so a team that customises its
    /// `Dockerfile` can still hold a green `--check`.
    Pinned,
}

impl Status {
    /// Whether `--apply` writes this file.
    #[must_use]
    pub const fn is_applied(self) -> bool {
        matches!(self, Self::Add | Self::Update)
    }

    /// Whether this counts as drift for `--check`.
    ///
    /// [`Status::Removed`] and [`Status::Pinned`] do not. A CI gate that can
    /// never go green again — because someone deliberately deleted
    /// `.env.example`, or because the team's `Dockerfile` is theirs on purpose
    /// — teaches people to delete the gate.
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
            Self::Pinned => "pinned",
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
            Self::Removed => {
                "you deleted this; it is not restored (drop its line from the manifest to be offered it again)"
            }
            Self::Pinned => "you accepted this file as yours; it is left alone",
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
    /// What was at this path when the plan was made. Kept so the apply step
    /// can prove nothing has changed since.
    current: OnDisk,
    /// Rendered preview diff; empty when there is nothing to show.
    pub diff: String,
    /// Where the file would be written.
    pub absolute: PathBuf,
}

/// The name to interpolate into the rendered scaffold, or `None` when the
/// project does not say.
///
/// Read from `Cargo.toml`'s `[package] name` and from nowhere else. Two
/// temptations are deliberately refused:
///
/// - **The directory name.** `autumn.toml`, `.env.example`, the CI workflow and
///   the `Dockerfile`'s `CMD` all interpolate the project name, so guessing it
///   wrong does not merely mislabel a report — it renders a *different*
///   scaffold, and a file whose recorded digest still matches then classifies
///   as `update` and gets written. A checkout in a renamed directory would
///   quietly rewrite `COPY --from=builder /app/target/release/<name>` into an
///   image that cannot start.
/// - **A name Cargo would not accept.** The name is substituted into a YAML
///   workflow and a Dockerfile as raw text, so a `[package] name` carrying
///   newlines is a content-injection vector into files this command then
///   *writes*. [`crate::new::validate_name`] is the same rule `autumn new`
///   applies when it creates the project, so anything it rejects cannot have
///   produced the scaffold being compared against.
///
/// Without a usable name the scaffold cannot be rendered faithfully, and the
/// reconciler says so instead of comparing against a fiction.
fn project_name(root: &Path) -> Option<String> {
    let name = std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()?
        .parse::<toml::Table>()
        .ok()?
        .get("package")?
        .get("name")?
        .as_str()?
        .to_owned();
    crate::new::validate_name(&name).ok()?;
    Some(name)
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
    // Positive evidence of the fullstack CSS pipeline, any one of which the
    // API scaffold never writes. Deliberately not "a `static/` directory
    // exists": a JSON API serves its `openapi.json` or a favicon from one, and
    // `add` is the one verdict that writes with no baseline behind it — so a
    // flavour guessed from a weak signal does not mislabel a report, it seeds
    // Tailwind into an app that has no use for it. Any one of these is enough,
    // so deleting a single file cannot reclassify a fullstack project either.
    // Each of these is a file the fullstack scaffold writes and the API
    // scaffold never does. A `static/` directory — or even a `static/css/` one
    // — is not evidence: a JSON API serves its `openapi.json`, its favicon, or
    // a stylesheet for a docs page out of exactly those. Getting this wrong is
    // not a mislabelled report: `add` writes with no baseline behind it, so a
    // weak signal seeds Tailwind into an app with no view stack, and the first
    // `--apply` then records the guess as fact.
    let fullstack = root.join("tailwind.config.js").exists()
        || root.join("static/css/input.css").exists()
        || root.join("static/js/htmx.min.js").exists();
    GenerateOptions {
        with_api: !fullstack,
        with_i18n: root.join("i18n").is_dir(),
        // Never affects a framework-owned file (the seed binary lives under
        // `src/`, which is out of bounds), so there is nothing to infer.
        with_seed: false,
        with_daemon: bundled_pg || autumn_toml.contains("this app uses no database"),
        with_bundled_pg: bundled_pg,
    }
}

/// Files that a Cargo workspace owns at its root, not per crate.
///
/// `clippy.toml`, `rustfmt.toml` and `rust-toolchain.toml` are all resolved
/// from the *nearest ancestor* of the crate being built, so a crate-local copy
/// does not add to the workspace's — it **shadows** it, silently dropping its
/// lints and its MSRV pin with no diagnostic. GitHub only runs workflows from
/// the repository root, so a member's `.github/workflows/ci.yml` never runs at
/// all. Seeding any of them into a workspace member is not an upgrade; it is a
/// regression that looks like one.
const WORKSPACE_ROOT_OWNED: &[&str] = &[
    "clippy.toml",
    "rustfmt.toml",
    "rust-toolchain.toml",
    ".github/workflows/ci.yml",
    ".github/workflows/posture-gate.yml",
];

/// Whether `root` is a crate inside an enclosing Cargo workspace.
///
/// Answered from the `[workspace]` table of an ancestor manifest rather than
/// from its `members` list: a path dependency, an `exclude`d fixture and a
/// listed member all sit under the same root config, and all three shadow it
/// the same way.
#[must_use]
pub fn workspace_root_above(root: &Path) -> Option<PathBuf> {
    // A manifest carrying `[workspace]` alongside its `[package]` is the
    // standard way for a crate below another workspace to form its own — and a
    // workspace root owns the toolchain, lint, formatting and CI files no
    // matter what sits above it. Answering "member" here would take exactly
    // those out of scope and let drift in them go unreported.
    if declares_workspace(root) {
        return None;
    }
    let absolute = root.canonicalize().ok()?;
    absolute
        .ancestors()
        .skip(1)
        .find(|ancestor| declares_workspace(ancestor))
        .map(Path::to_path_buf)
}

/// Whether the `Cargo.toml` in `directory` declares a workspace.
fn declares_workspace(directory: &Path) -> bool {
    std::fs::read_to_string(directory.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .is_some_and(|table| table.contains_key("workspace"))
}

/// The current release's framework-owned files, rendered for the project at
/// `root`, or `None` when the project's name cannot be established.
#[must_use]
pub fn current_files(
    root: &Path,
    options: GenerateOptions,
) -> Option<BTreeMap<&'static str, String>> {
    let name = project_name(root)?;
    let crate_name = name.replace('-', "_");
    let vars = TemplateVars {
        project_name: &name,
        crate_name: &crate_name,
        autumn_version: env!("CARGO_PKG_VERSION"),
        rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
    };
    let mut files = framework_owned_files(&vars, options);
    if workspace_root_above(root).is_some() {
        for path in WORKSPACE_ROOT_OWNED {
            files.remove(path);
        }
    }
    Some(files)
}

/// What is at a framework-owned path right now.
///
/// Four states, not two. Collapsing "there but unreadable" into "absent" — the
/// shape `read_to_string(..).ok()` gives you — is the bug that turns a latin-1
/// `input.css` or a root-owned `.gitignore` into an `add`, and then truncates
/// it. A file whose contents cannot be read is the *last* thing that may be
/// overwritten, not the first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OnDisk {
    /// Nothing at this path.
    Absent,
    /// Readable text, line endings normalised.
    Text(String),
    /// Reached through a symbolic link. The contents are known — so a link
    /// whose target already matches the scaffold is simply current — but the
    /// path is never written, because the write would land wherever the link
    /// points, which need not be inside the project.
    Linked(Option<String>),
    /// Present, and untouchable for this reason.
    Opaque(ConflictReason),
}

/// Read the path `relative` under `root`, refusing to resolve through a link.
///
/// The whole chain is checked, not just the leaf. A project whose `.github` is
/// a symlink to a shared workflows tree would otherwise have
/// `.github/workflows/ci.yml` classified as an ordinary missing file — and
/// `--apply` would then create it outside the project, somewhere the project's
/// own `git status` can never show.
fn read_current(root: &Path, relative: &str) -> OnDisk {
    // A swap in flight leaves its path absent for a moment. Wait it out, so a
    // held file is not read as missing. A claim left by a crash costs only
    // this bounded wait.
    for attempt in 1..SWAP_ATTEMPTS {
        let now = read_current_once(root, relative);
        if now != OnDisk::Absent {
            return now;
        }
        if claim_in_flight(&root.join(relative)).is_none() {
            // The swap may have ended after the read above. Read again.
            return read_current_once(root, relative);
        }
        back_off(attempt);
    }
    let now = read_current_once(root, relative);
    // A claim that outlasts the wait is a swap that stalled or crashed. The
    // file exists, held aside: unreadable, never absent.
    if now == OnDisk::Absent && claim_in_flight(&root.join(relative)).is_some() {
        return OnDisk::Opaque(ConflictReason::Unreadable);
    }
    now
}

fn read_current_once(root: &Path, relative: &str) -> OnDisk {
    let mut cursor = root.to_path_buf();
    let components: Vec<&str> = relative.split('/').collect();
    let (_leaf, parents) = components
        .split_last()
        .expect("a framework-owned path always has at least one component");
    for directory in parents {
        cursor.push(directory);
        match std::fs::symlink_metadata(&cursor) {
            // A directory that is not there yet is not a link, and creating it
            // is exactly what an `add` of a nested file does.
            Err(_) => break,
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return OnDisk::Linked(read_text(&root.join(relative)));
            }
            Ok(_) => {}
        }
    }

    let absolute = root.join(relative);
    // `symlink_metadata` does not follow, which is the point: a link's own
    // metadata is what says it is a link. `metadata` would report the target,
    // and a dangling link would read as absent — the exact combination that
    // lets `--apply` create a file outside the project.
    match std::fs::symlink_metadata(&absolute) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OnDisk::Absent,
        Err(_) => OnDisk::Opaque(ConflictReason::Unreadable),
        Ok(metadata) if metadata.file_type().is_symlink() => OnDisk::Linked(read_text(&absolute)),
        // It exists. Reading it can still fail — a directory, a device,
        // non-UTF-8 bytes, a permission the process does not have — and every
        // one of those is untouchable rather than absent.
        Ok(_) => {
            read_text(&absolute).map_or(OnDisk::Opaque(ConflictReason::Unreadable), OnDisk::Text)
        }
    }
}

fn read_text(absolute: &Path) -> Option<String> {
    std::fs::read_to_string(absolute)
        .ok()
        .map(|text| normalize(&text))
}

/// What the app-code codemods plan for one file.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The text the codemods read.
    pub original: String,
    /// The text the codemods write.
    pub updated: String,
}

/// Reconcile `files` against what is on disk under `root`.
///
/// `migrated` maps each path the codemods rewrite to its plan.
#[must_use]
pub fn classify(
    root: &Path,
    files: &BTreeMap<&'static str, String>,
    manifest: Option<&Manifest>,
    migrated: &BTreeMap<String, Planned>,
) -> Vec<Entry> {
    let recorded = |path: &str| manifest.and_then(|manifest| manifest.digests.get(path));
    let pinned = |path: &str| manifest.is_some_and(|manifest| manifest.pinned.contains(path));

    let mut entries: Vec<Entry> = files
        .iter()
        .map(|(path, template)| {
            let absolute = root.join(path);
            let current = read_current(root, path);
            // A migrated file is judged by its planned text. In a preview, the
            // old file is still on disk. In `--apply`, the new file is already
            // there. Any other text is a later edit, so the disk wins.
            let planned = migrated.get(*path);
            let seen = match (&current, planned) {
                (OnDisk::Text(text), Some(plan))
                    if *text == normalize(&plan.original) || *text == normalize(&plan.updated) =>
                {
                    OnDisk::Text(normalize(&plan.updated))
                }
                _ => current.clone(),
            };
            let normalized = normalize(template);
            let differs = |text: &str| {
                let diff = super::diff::render(text, template);
                (text != normalized, diff)
            };
            let (status, diff) = match &seen {
                // Already what this release writes. Checked before anything
                // else, including the pin: a pinned file that happens to match
                // is current, not an exception.
                OnDisk::Text(text) | OnDisk::Linked(Some(text)) if *text == normalized => {
                    (Status::UpToDate, String::new())
                }
                // The developer has said this one is theirs.
                _ if pinned(path) => (Status::Pinned, String::new()),
                // Not there at all. Autumn wrote it once and it is gone → a
                // deliberate deletion. Never wrote it → this release added it.
                OnDisk::Absent if recorded(path).is_some() => (Status::Removed, String::new()),
                OnDisk::Absent => (Status::Add, super::diff::render("", template)),
                // There, but nothing can be said about it. Never an `add`: the
                // whole reason to look is that writing would destroy whatever
                // is really in the file.
                OnDisk::Opaque(reason) => (Status::Conflict(*reason), String::new()),
                // Reachable, but only through a link. Readable or not, it is
                // never written.
                OnDisk::Linked(text) => (
                    Status::Conflict(ConflictReason::Symlink),
                    text.as_ref()
                        .map(|text| differs(text).1)
                        .unwrap_or_default(),
                ),
                OnDisk::Text(text) => {
                    let status = match recorded(path) {
                        _ if planned.is_some() => Status::Conflict(ConflictReason::MigratedThisRun),
                        Some(baseline) if *baseline == digest(text) => Status::Update,
                        Some(_) => Status::Conflict(ConflictReason::Edited),
                        None => Status::Conflict(ConflictReason::NoBaseline),
                    };
                    (status, differs(text).1)
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
    let manifest = root.join(MANIFEST_PATH);
    // A manifest held aside by a swap still marks the project.
    root.join("autumn.toml").is_file() || manifest.is_file() || claim_in_flight(&manifest).is_some()
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
    /// Whether the project's package name could be established. When it could
    /// not, the scaffold cannot be rendered for comparison and [`Self::entries`]
    /// is empty — which is a refusal to answer, not an "all clear".
    pub named: bool,
    /// The recorded baseline, when it names a release newer than this CLI.
    ///
    /// Reconciling then means rendering *older* templates against digests a
    /// newer release wrote — so every untouched file matches its digest,
    /// classifies as a writable `update`, and `--apply` silently downgrades the
    /// `Dockerfile`, the build script and the CI workflow. Downgrades are out
    /// of scope, so the run refuses instead and [`Self::entries`] is empty.
    pub scaffolded_by_newer: Option<String>,
    /// Whether this project is a crate inside an enclosing Cargo workspace, in
    /// which case the files that workspace owns at its root are out of scope.
    pub workspace_member: bool,
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

    /// Whether the report should explain that this project has no baseline.
    ///
    /// True while any file still has none — not merely on the first run. A
    /// legacy project's first `--apply` writes a manifest covering only the
    /// files it wrote, so "a manifest exists" would drop the explanation while
    /// every remaining file was still reported as having no baseline.
    #[must_use]
    pub fn needs_baseline_note(&self) -> bool {
        !self.has_manifest
            || self
                .entries
                .iter()
                .any(|entry| entry.status == Status::Conflict(ConflictReason::NoBaseline))
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
        // Carried forward, but only for paths this release still owns. A key
        // for a file the framework no longer generates is dead weight — and a
        // hand-edited or hostile manifest should not be able to accumulate
        // entries in a file Autumn rewrites.
        let owned: BTreeSet<&str> = self
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        let mut digests: BTreeMap<String, String> = previous
            .map(|manifest| {
                manifest
                    .digests
                    .iter()
                    .filter(|(path, _)| owned.contains(path.as_str()))
                    .map(|(path, digest)| (path.clone(), digest.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for entry in &self.entries {
            if entry.status.is_applied() || entry.status == Status::UpToDate {
                digests.insert(entry.path.clone(), digest(&entry.template));
            }
        }
        // The baseline moves only once nothing is left to reconcile. Recording
        // the target while conflicts stand would tell the next run — and the
        // developer reading it — that this upgrade is finished. With nothing
        // previously recorded there is simply no such release yet, and the
        // field is left out rather than filled with a guess.
        let version = if self.conflicts().is_empty() {
            Some(self.target.clone())
        } else {
            previous.and_then(|manifest| manifest.version.clone())
        };
        // The renderer mark moves forward on every write, conflicts or not:
        // these digests describe *this* release's templates, and an older CLI
        // must be able to see that even when the baseline could not advance.
        let written_by = Some(newest(
            previous.and_then(|m| m.written_by.as_deref()),
            &self.target,
        ));
        Manifest {
            version,
            written_by,
            options: self.options,
            digests,
            pinned: previous
                .map(|manifest| manifest.pinned.clone())
                .unwrap_or_default(),
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

/// The release a manifest names, when it is newer than this CLI.
fn newer_release_recorded(manifest: Option<&Manifest>) -> Option<String> {
    manifest
        .and_then(|manifest| match (&manifest.written_by, &manifest.version) {
            (Some(written_by), Some(version)) => Some(newest(Some(version), written_by)),
            (recorded, None) | (None, recorded) => recorded.clone(),
        })
        .filter(|recorded| is_newer_than_this_cli(recorded))
}

/// Plan a reconciliation of the project at `root`.
///
/// `_target` is accepted and ignored for the scaffold half, deliberately. This
/// CLI ships exactly one set of scaffold templates — its own — so "bring me to
/// the current release" is the only reconciliation it can honestly perform, and
/// downgrades and arbitrary historical scaffolds are out of scope. `--to`
/// selects which *codemods* run; a scaffold report claiming to reconcile to
/// 0.6.0 while rendering 0.7.0's files would simply be false.
#[must_use]
pub fn plan(root: &Path, target: &str) -> ScaffoldReport {
    plan_after(root, target, &BTreeMap::new())
}

/// Plan a reconciliation, knowing which paths this run's app-code migrations
/// rewrite, and to what text.
///
/// `build.rs` is both a framework-owned file and a `.rs` file the codemods
/// scan, so one `--apply` can land on it twice. The map names the files the
/// first half rewrites and gives their planned text. The second half reports
/// those files honestly, and does not accuse the developer of an edit this
/// command made moments earlier. The diff uses the planned text, so preview and
/// apply show the same diff.
#[must_use]
pub fn plan_after(
    root: &Path,
    _target: &str,
    migrated: &BTreeMap<String, Planned>,
) -> ScaffoldReport {
    let target = env!("CARGO_PKG_VERSION").to_owned();
    let manifest = Manifest::load(root);
    let options = resolve_options(root, manifest.as_ref());
    // A baseline from a release newer than this CLI cannot be reconciled: the
    // templates here are older than the files those digests describe, so every
    // untouched file would classify as a writable `update` and `--apply` would
    // downgrade it. Nothing is rendered and nothing is classified.
    // The newest release either field names. `written_by` is the one that
    // matters — a conflict holds `version` back while the digests advance, so
    // `version` alone would let an older CLI trust newer digests — but taking
    // the newer of the two costs nothing, covers a manifest written before the
    // mark existed, and refuses an incoherent manifest whose `version` somehow
    // exceeds it rather than reasoning about which field to believe.
    let scaffolded_by_newer = newer_release_recorded(manifest.as_ref());
    let files = scaffolded_by_newer
        .is_none()
        .then(|| current_files(root, options))
        .flatten();
    ScaffoldReport {
        root: root.to_path_buf(),
        options,
        baseline: manifest.as_ref().and_then(|m| m.version.clone()),
        named: scaffolded_by_newer.is_some() || files.is_some(),
        scaffolded_by_newer,
        workspace_member: workspace_root_above(root).is_some(),
        entries: files
            .map(|files| classify(root, &files, manifest.as_ref(), migrated))
            .unwrap_or_default(),
        guide: release_guide(&target),
        target,
        has_manifest: manifest.is_some(),
        outcome: super::Outcome::Preview,
    }
}

/// The newer of two release strings, preferring `candidate` when neither
/// parses as a version.
fn newest(previous: Option<&str>, candidate: &str) -> String {
    match previous {
        // The same precedence the refusal uses. A high-water mark compared any
        // other way could move backwards across two prereleases of one triple.
        Some(previous) if is_newer(previous, candidate) => previous.to_owned(),
        _ => candidate.to_owned(),
    }
}

/// Whether `recorded` names a release newer than the one this CLI ships.
///
/// Only ever reached with a value [`Manifest::parse`] has already proven
/// readable: an unreadable one makes the whole manifest no baseline, so this
/// never has to decide what "unknown" means.
fn is_newer_than_this_cli(recorded: &str) -> bool {
    is_newer(recorded, env!("CARGO_PKG_VERSION"))
}

/// Whether `recorded` has strictly higher `SemVer` precedence than `ours`.
///
/// Deliberately **not** [`Version`](super::migrations::Version)'s own [`Ord`],
/// which distinguishes only prerelease-from-stable and treats every prerelease
/// of one triple as interchangeable. That is right for what it exists for — an
/// app on `0.8.0-rc.1` needs the same codemods as one on `0.8.0-rc.2`, so
/// selection must not split them, and a test in that module pins it — and
/// wrong here, where the question is which renderer wrote a file. Borrowing it
/// made `0.8.0-rc.2` and `0.8.0-rc.1` compare equal, so an rc.1 CLI did not
/// refuse an rc.2 project and downgraded every file it rendered.
fn is_newer(recorded: &str, ours: &str) -> bool {
    let parse = super::migrations::parse_version_req;
    let (Some(recorded), Some(ours)) = (parse(recorded), parse(ours)) else {
        return false;
    };
    precedence(&recorded, &ours) == std::cmp::Ordering::Greater
}

/// Full `SemVer` precedence, prerelease identifiers included.
fn precedence(
    left: &super::migrations::Version,
    right: &super::migrations::Version,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    (left.major, left.minor, left.patch)
        .cmp(&(right.major, right.minor, right.patch))
        .then_with(
            || match (left.prerelease.as_deref(), right.prerelease.as_deref()) {
                (None, None) => Ordering::Equal,
                // A stable release outranks any prerelease of its own triple.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => prerelease_precedence(left, right),
            },
        )
}

/// `SemVer` §11.4 precedence between two prerelease identifier sets.
fn prerelease_precedence(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        let ordering = match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            // "A larger set of pre-release fields has a higher precedence than
            // a smaller set, if all of the preceding identifiers are equal."
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => match (left.parse::<u64>(), right.parse::<u64>()) {
                // Numeric identifiers are compared numerically, so `rc.10`
                // outranks `rc.9` — which a text comparison gets backwards.
                (Ok(left), Ok(right)) => left.cmp(&right),
                // "Numeric identifiers always have lower precedence than
                // non-numeric identifiers."
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => left.cmp(right),
            },
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
}

/// Record `paths` as the developer's own, so reconciliation leaves them alone.
///
/// This is what lets a conflict *conclude*. Without it a team whose `Dockerfile`
/// is deliberately theirs can never make `autumn upgrade --check` green again —
/// and a gate that is permanently red is a gate that gets deleted.
///
/// Only the manifest is written; no project file is touched.
///
/// # Errors
///
/// Returns a message naming any path the current scaffold does not own, and
/// changes nothing. Accepting a path is a promise that reconciliation will skip
/// it, and a promise about a file this command never touches is meaningless.
pub fn accept(root: &Path, paths: &[String]) -> Result<Manifest, String> {
    // Judged inside the update, on the manifest it actually read. A load made
    // earlier can miss a manifest that a concurrent swap holds aside, and the
    // options inferred without it can disown a file the project owns.
    let mut refusal = None;
    let updated = Manifest::update(root, |current| {
        let options = resolve_options(root, current.as_ref());
        let Some(owned) = current_files(root, options) else {
            refusal = Some(
                "this project's `Cargo.toml` gives no usable `[package] name`, so the \
                 scaffold cannot be rendered and there is nothing to accept against"
                    .to_owned(),
            );
            return Err(String::new());
        };
        let unknown: Vec<&str> = paths
            .iter()
            .map(String::as_str)
            .filter(|path| !owned.contains_key(path))
            .collect();
        if !unknown.is_empty() {
            refusal = Some(format!(
                "not framework-owned in this project, so there is nothing to accept: {}",
                unknown.join(", ")
            ));
            return Err(String::new());
        }
        let mut manifest = current.unwrap_or_else(|| Manifest {
            version: None,
            written_by: None,
            options,
            digests: BTreeMap::new(),
            pinned: BTreeSet::new(),
        });
        manifest.options = options;
        manifest.pinned.extend(paths.iter().cloned());
        Ok(manifest)
    });
    match (refusal, updated) {
        (Some(refusal), _) => Err(refusal),
        (None, Ok(manifest)) => Ok(manifest),
        (None, Err(error)) => Err(format!("could not write {MANIFEST_PATH}: {error}")),
    }
}

/// Write the additions and updates in `report`, then refresh the manifest.
///
/// Each file is re-read before it is written and compared against
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
            // A file that landed with a late write beside it still landed.
            let written = written + usize::from(error.published);
            report.outcome = super::Outcome::Partial { written };
            return Err(WriteFailure {
                path: entry.path.clone(),
                error: error.message,
                written,
            });
        }
    }

    // The files are all written, so the outcome is `Applied` whatever happens
    // next — but recording the baseline is part of the job, not a courtesy.
    let written = report.applicable().len();
    report.outcome = super::Outcome::Applied;
    record_baseline(report).map_err(|error| WriteFailure {
        path: MANIFEST_PATH.to_owned(),
        error,
        written,
    })
}

/// Refresh the provenance manifest after a completed apply.
///
/// # Errors
///
/// Fails when the manifest cannot be written. Deliberately *not* best effort:
/// the files on disk are correct, but without their new digests the very next
/// `autumn upgrade --check` reports them as conflicts and exits 3. An apply
/// that guarantees a red gate afterwards has not succeeded, and returning
/// success would tell a CI script otherwise.
fn record_baseline(report: &ScaffoldReport) -> Result<(), String> {
    // A report with no entries reconciled nothing, and rebuilding the manifest
    // from no entries prunes every digest in it and stamps this CLI's version
    // over whatever was recorded. That is the baseline the whole feature rests
    // on, destroyed by a run that did not even look at the files.
    //
    // Keyed on the entries themselves rather than on *why* they are empty. That
    // distinction has already cost once: this guard was written as `!named` for
    // the unreadable-package-name case, and the newer-scaffold refusal then
    // arrived with entries empty and `named` true, walking straight past it.
    // Every reason to refuse produces an empty plan, so that is the thing to
    // check.
    if report.entries.is_empty() {
        return Ok(());
    }
    Manifest::update(&report.root, |previous| {
        // The plan is older than this manifest. Recording from it would move
        // a newer release's baseline backwards.
        if let Some(newer) = newer_release_recorded(previous.as_ref()) {
            return Err(format!("a newer release ({newer}) updated this project"));
        }
        Ok(report.next_manifest(previous.as_ref()))
    })
    .map(drop)
    .map_err(|error| {
        format!(
            "the scaffold files were written, but the baseline could not be recorded: \
             {error}. Until it is, every file this run updated will be reported as a \
             conflict and `--check` will not go green."
        )
    })
}

/// Write one entry, refusing to clobber anything that moved since the plan.
///
/// The early re-read gives a clear error. [`Publish::Swap`] then checks the
/// text again at the moment of the swap, so a later change is refused too.
///
/// Written through a temporary file in the same directory and renamed into
/// place, the way the app-code half of this command writes: a truncate-in-place
/// interrupted by Ctrl-C or ENOSPC leaves a half-written `Dockerfile` and no
/// copy of the original anywhere.
fn write_one(entry: &Entry) -> Result<(), WriteError> {
    let on_disk = read_current_absolute(entry);
    if on_disk != entry.current {
        return Err(WriteError::from(match entry.current {
            OnDisk::Absent => "something appeared at this path after the preview was \
                               computed; it was left exactly as it is"
                .to_owned(),
            _ => "this file changed after the preview was computed; \
                  it was left exactly as it is"
                .to_owned(),
        }));
    }
    // A plan is only ever built for `add` and `update`, and `read_current`
    // classifies a link or an unreadable file as a conflict, so reaching this
    // with either would be a bug rather than a race. Checked anyway: this is
    // the last line before a write, and the cost of being wrong is a file
    // outside the project.
    if matches!(on_disk, OnDisk::Opaque(_) | OnDisk::Linked(_)) {
        return Err(WriteError::from(
            "this path is a symlink or is unreadable; it was left exactly as it is".to_owned(),
        ));
    }

    let directory = entry
        .absolute
        .parent()
        .ok_or_else(|| "no parent directory".to_owned())?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;

    let mode = match &entry.current {
        OnDisk::Text(text) => Publish::Swap(text.clone()),
        _ => Publish::Create,
    };
    publish(&entry.absolute, &entry.template, mode).map_err(|error| WriteError {
        published: error.published(),
        message: error.to_string(),
    })
}

/// Why one file was not written cleanly.
struct WriteError {
    message: String,
    /// The new file is on disk, and something else needs a look.
    published: bool,
}

impl From<String> for WriteError {
    fn from(message: String) -> Self {
        Self {
            message,
            published: false,
        }
    }
}

/// Whether a publish may take a destination that already exists.
///
/// A statement about what the caller has *established*, not about what is on
/// disk right now — the gap between those two is the race this exists to catch.
/// Permissions are decided separately, from the destination itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Publish {
    /// The path was empty when the plan was made and must still be empty.
    Create,
    /// The path held this text (line endings ignored) at plan time. It must
    /// hold it at swap time.
    Swap(String),
}

/// Why a publish did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PublishError {
    /// Another writer changed the path first. Nothing of theirs was touched.
    Moved(String),
    /// The new file is in place, but a late write reached the old copy. The
    /// copy was kept. Not retried: the write must be reported.
    Late(String),
    /// The new file was not installed, and a displaced copy was kept. Not
    /// retried: the copy must be reported. Only the full swap builds it.
    #[cfg_attr(windows, allow(dead_code))]
    Retained(String),
    /// The disk refused.
    Failed(String),
}

impl PublishError {
    /// Whether the new file is already in place despite the error.
    const fn published(&self) -> bool {
        matches!(self, Self::Late(_))
    }
}

impl From<std::io::Error> for PublishError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Moved(text) | Self::Late(text) | Self::Retained(text) | Self::Failed(text)) =
            self;
        f.write_str(text)
    }
}

/// Publish `contents` at `absolute`, atomically.
///
/// Staged beside the destination and published by a single link operation,
/// never written in place. A plain `fs::write` interrupted by Ctrl-C or a full
/// disk leaves a truncated file — and for a file this command *added*, that is
/// permanent: the truncated file has no recorded baseline, so the next run
/// classifies it as a conflict and refuses to touch it.
///
/// The staging file is uniquely named and deletes itself when dropped, so this
/// function never removes a path it did not create. A predictable name would
/// have to be *reclaimed* before use, and reclaiming means deleting a file
/// whose provenance cannot be checked: someone else's, or the staging file of a
/// concurrent `autumn upgrade --apply` — defeating the very race protection
/// staging exists for. The cost is that a hard crash can leave one
/// `.autumn-upgrade-*.tmp` behind; it is inert, and never blocks a later run.
/// A crash inside a swap can leave `.autumn-upgrade-*.old`: the old file. Rename
/// it back.
///
/// [`Publish::Create`] never replaces. `rename` replaces on Unix, so a file
/// created while staging would be lost.
///
/// [`Publish::Swap`] is a compare-and-swap: it replaces the destination only
/// if it still holds the planned text, and refuses with [`PublishError::Moved`]
/// otherwise. See [`swap`].
fn publish(absolute: &Path, contents: &str, mode: Publish) -> Result<(), PublishError> {
    use std::io::Write as _;

    let directory = absolute
        .parent()
        .ok_or_else(|| PublishError::Failed("no parent directory".to_owned()))?;
    // Created through ordinary `0o666` open semantics so the process umask
    // applies, exactly as it does to the `fs::write` that `autumn new` uses.
    // `tempfile`'s own constructor deliberately creates `0600`, and deriving a
    // mode from the directory does not reproduce the umask either: a `0775`
    // checkout under umask `022` yields `0664` that way and `0644` from a real
    // write, quietly granting group write. `make_in` still supplies the unique
    // name and the delete-on-drop, so nothing this function did not create is
    // ever removed.
    let mut temp = tempfile::Builder::new()
        .prefix(".autumn-upgrade-")
        .suffix(".tmp")
        .make_in(directory, |path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
        })?;

    // An existing file keeps its mode. A new file keeps the umask mode.
    if let Ok(metadata) = std::fs::metadata(absolute) {
        let _ = temp.as_file().set_permissions(metadata.permissions());
    }

    temp.write_all(contents.as_bytes())?;
    temp.flush()?;
    // The publish is atomic, but only against a crash if the bytes reached the
    // disk first: otherwise it can land before the data and publish an empty
    // file.
    temp.as_file().sync_all()?;

    match mode {
        Publish::Create => temp
            .persist_noclobber(absolute)
            .map(drop)
            .map_err(|error| {
                if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                    PublishError::Moved(
                        "something appeared at this path while it was being written; \
                     it was left exactly as it is"
                            .to_owned(),
                    )
                } else {
                    error.error.into()
                }
            })?,
        Publish::Swap(expected) => swap(temp, absolute, &expected)?,
    }
    Ok(())
}

/// Compare-and-swap: replace the destination only if it still holds `expected`.
///
/// First rename the destination aside. This is atomic and gives this process
/// the exact file. Then compare it. On a mismatch, put it back, unless a newer
/// file took its place. While the path is absent, a new writer is not
/// overwritten, because the staged file is linked in without replacing.
/// [`claim_in_flight`] lets readers see this window.
#[cfg(not(windows))]
fn swap(
    temp: tempfile::NamedTempFile,
    absolute: &Path,
    expected: &str,
) -> Result<(), PublishError> {
    // Close the staged file first. Windows moves a file with a live handle
    // badly, and the published file must not keep one open.
    let staged = temp.into_temp_path();
    let directory = absolute
        .parent()
        .ok_or_else(|| PublishError::Failed("no parent directory".to_owned()))?;
    let claim = tempfile::Builder::new()
        .prefix(
            &claim_prefix(absolute)
                .ok_or_else(|| PublishError::Failed("no file name".to_owned()))?,
        )
        .suffix(CLAIM_SUFFIX)
        .tempfile_in(directory)?
        .into_temp_path();
    // The file only reserved a unique name. Remove it, so a crash before the
    // rename leaves no empty claim, and a claim exists only once it holds the
    // destination.
    let _ = std::fs::remove_file(&claim);
    match std::fs::rename(absolute, &claim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PublishError::Moved(
                "this file disappeared after the preview was computed".to_owned(),
            ));
        }
        Err(error) => return Err(error.into()),
    }

    // Only a plain file is read: a FIFO or device would block.
    let held = std::fs::symlink_metadata(&claim).is_ok_and(|meta| meta.file_type().is_file())
        && read_text(&claim).is_some_and(|text| text == normalize(expected));
    if !held {
        // Wrong file. Put it back. If a newer file is there, keep the old copy
        // beside it and say where.
        return Err(match claim.persist_noclobber(absolute) {
            Ok(()) => PublishError::Moved(
                "this file changed after the preview was computed; \
                 it was left exactly as it is"
                    .to_owned(),
            ),
            Err(error) => {
                // The displaced copy is the only one. Report it, never retry it.
                let restore = error.error.to_string();
                let present = error.path.exists();
                match error.path.keep() {
                    Ok(path) => PublishError::Retained(format!(
                        "this file changed twice after the preview was computed; \
                         the earlier copy is at {}",
                        path.display()
                    )),
                    Err(error) => PublishError::Failed(format!(
                        "could not restore the displaced copy ({restore}) or keep it \
                         (still there: {present}): {error}"
                    )),
                }
            }
        });
    }

    match staged.persist_noclobber(absolute) {
        Ok(()) => finish_claim(claim, expected),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // The old copy may have had a late write too. Keep it if so. The
            // staged file was not installed, so this is not a publish.
            finish_claim(claim, expected).map_err(|error| match error {
                PublishError::Late(message) => PublishError::Retained(message),
                other => other,
            })?;
            Err(PublishError::Moved(
                "something appeared at this path while it was being written; \
                 it was left exactly as it is"
                    .to_owned(),
            ))
        }
        Err(error) => {
            // Restore the old file. If the path is taken, the new file stays
            // and the old copy is kept beside it, and the error says where.
            let staging = error.error.to_string();
            Err(match claim.persist_noclobber(absolute) {
                Ok(()) => PublishError::Failed(staging),
                Err(restore) => match restore.path.keep() {
                    Ok(path) => PublishError::Retained(format!(
                        "{staging}; the earlier copy is at {}",
                        path.display()
                    )),
                    Err(kept) => PublishError::Failed(format!(
                        "{staging}; the earlier copy could not be kept: {kept}"
                    )),
                },
            })
        }
    }
}

/// Windows: re-read, compare, then replace.
///
/// The rename-aside claim is not used here. On `windows-latest` a claim file
/// went missing right after its rename, and the cause is not known. The old
/// copy cannot be put back, so the claim could lose a write. This keeps the
/// narrow window the code had before: a writer that changes the file between
/// the compare and the replace loses. [`Manifest::update`] still re-reads and
/// retries.
#[cfg(windows)]
fn swap(
    temp: tempfile::NamedTempFile,
    absolute: &Path,
    expected: &str,
) -> Result<(), PublishError> {
    let unchanged = std::fs::symlink_metadata(absolute)
        .is_ok_and(|meta| meta.file_type().is_file())
        && read_text(absolute).is_some_and(|text| text == normalize(expected));
    if !unchanged {
        return Err(PublishError::Moved(
            "this file changed after the preview was computed; \
             it was left exactly as it is"
                .to_owned(),
        ));
    }
    temp.persist(absolute)
        .map(drop)
        .map_err(|error| PublishError::Failed(error.error.to_string()))
}

/// Drop the old copy, unless it changed since the compare.
///
/// A writer that opened the old file before the swap can still write into it.
/// Keep such a copy and say where it is, so the write is not lost in silence.
#[cfg_attr(windows, allow(dead_code))]
fn finish_claim(claim: tempfile::TempPath, expected: &str) -> Result<(), PublishError> {
    match read_text(&claim) {
        Some(text) if text == normalize(expected) => return Ok(()),
        // Nothing is there, so nothing can be kept.
        None if !claim.exists() => return Ok(()),
        _ => {}
    }
    match claim.keep() {
        Ok(path) => Err(PublishError::Late(format!(
            "this file was written during the swap; the earlier copy is at {}",
            path.display()
        ))),
        // The new file is already in place, so this is still a publish.
        Err(error) => Err(PublishError::Late(format!(
            "this file was written during the swap, and the earlier copy could not \
             be kept: {error}"
        ))),
    }
}

/// Re-read what an entry's path holds now, by the same rules the plan used —
/// the parent-link check included.
fn read_current_absolute(entry: &Entry) -> OnDisk {
    // The entry's own root is its absolute path with its relative path removed,
    // so the chain checked here is exactly the chain checked at plan time.
    let mut root = entry.absolute.clone();
    for _ in 0..=entry.path.matches('/').count() {
        root.pop();
    }
    read_current(&root, &entry.path)
}

/// The human report, with a diff for every file that differs.
#[must_use]
pub fn render_text(report: &ScaffoldReport) -> String {
    render(report, true)
}

/// The human report without the per-file diffs.
///
/// What `--check` prints. A CI gate wants the verdict and the file names; the
/// diffs would put the working contents of `autumn.toml` and `.env.example` —
/// the two files people most often paste a connection string into — into a
/// build log that outlives the run.
#[must_use]
pub fn render_summary(report: &ScaffoldReport) -> String {
    render(report, false)
}

fn render(report: &ScaffoldReport, diffs: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    if let Some(newer) = &report.scaffolded_by_newer {
        let _ = writeln!(out, "\nScaffold files");
        let _ = writeln!(
            out,
            "  Skipped: this project was scaffolded by autumn-cli {newer}, which is newer\n  \
             than this one ({}). Reconciling would render older templates over newer\n  \
             files — a downgrade, which this command does not do. Install {newer} or\n  \
             later and run it again.",
            report.target
        );
        let _ = writeln!(out, "  Upgrade guide: {}", report.guide);
        return out;
    }
    if !report.named {
        let _ = writeln!(out, "\nScaffold files");
        let _ = writeln!(
            out,
            "  Skipped: this project's `Cargo.toml` does not give a usable `[package] name`,\n               and the scaffold interpolates it. Comparing against a guessed name would\n               report files as changed that are not — or worse, rewrite them."
        );
        return out;
    }
    let from = report
        .baseline
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let _ = writeln!(
        out,
        "\nScaffold files ({from} -> {target})",
        target = report.target
    );

    // Said up front, and on every run that still has an unbaselined file — not
    // only on the first. A project's first `--apply` writes a partial manifest,
    // and gating this on "a manifest exists" made the explanation vanish while
    // every remaining file was still reported as having no baseline.
    if report.needs_baseline_note() {
        let _ = writeln!(
            out,
            "  This project predates scaffold provenance, so there is no record of\n  \
             what Autumn originally wrote. Files it is missing are offered; every\n  \
             file that differs is a conflict for you to review."
        );
    }
    if report.workspace_member {
        let _ = writeln!(
            out,
            "  This crate sits inside a Cargo workspace, so the files that workspace owns\n  \
             at its root — clippy.toml, rustfmt.toml, rust-toolchain.toml and the CI\n  \
             workflow — are out of scope here: a crate-local copy would shadow the\n  \
             workspace's, not add to it. Reconcile those at the workspace root."
        );
    }

    let changed = report.changed();
    if changed.is_empty() {
        let _ = writeln!(
            out,
            "  Your framework-owned files are up to date with this release."
        );
        let _ = writeln!(out, "  Upgrade guide: {}", report.guide);
        return out;
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

    if diffs {
        for entry in &changed {
            if entry.diff.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n{} ({})", entry.path, entry.status.label());
            out.push_str(&entry.diff);
        }
    }

    render_outcome(&mut out, report, diffs);
    let _ = writeln!(out, "Upgrade guide: {}", report.guide);
    out
}

/// The closing paragraph: what happened, or what would, and how to undo it.
fn render_outcome(out: &mut String, report: &ScaffoldReport, diffs: bool) {
    use std::fmt::Write as _;

    let applicable = report.applicable().len();
    let conflicts = report.conflicts().len();
    let _ = writeln!(out);
    match report.outcome {
        // Nothing to write and nothing to resolve: everything listed is a file
        // the developer removed or accepted. "0 conflict(s) need review" as the
        // headline of a report that just listed findings reads as a bug.
        super::Outcome::Preview if applicable == 0 && conflicts == 0 => {
            let _ = writeln!(
                out,
                "Nothing to do: everything above is a file you removed or accepted as your own."
            );
        }
        // Pointing someone at `--apply` when every remaining difference is a
        // conflict sends them to run a command that would do nothing at all.
        super::Outcome::Preview if applicable == 0 => {
            let _ = writeln!(
                out,
                "{conflicts} conflict(s) need review; nothing here can be written for you."
            );
            let _ = writeln!(out, "{}", review_advice(diffs));
        }
        super::Outcome::Preview => {
            let _ = writeln!(
                out,
                "{applicable} file(s) would be written; {conflicts} conflict(s) need review."
            );
            let _ = writeln!(
                out,
                "Nothing was written. Re-run with `--apply` to take the writable ones."
            );
            let _ = writeln!(out, "{}", review_advice(diffs));
        }
        super::Outcome::Applied => {
            let _ = writeln!(
                out,
                "{applicable} file(s) written; {conflicts} conflict(s) left for you."
            );
            let _ = writeln!(out, "{REVERT_ADVICE}");
        }
        super::Outcome::Partial { written } => {
            let _ = writeln!(
                out,
                "{written} of {applicable} file(s) written before the run stopped."
            );
            let _ = writeln!(out, "{REVERT_ADVICE}");
        }
    }
    if conflicts > 0 {
        let _ = writeln!(
            out,
            "Conflicts are never overwritten. Take what you want from this release's\n\
             version, or `autumn upgrade --accept <path>` to keep yours for good."
        );
    }
}

/// How to undo an apply.
///
/// `git checkout --` restores a file that was *updated*; it does nothing for
/// one that was *added*, because git has never heard of it. Naming only the
/// first leaves the majority case — an aged project is mostly additions — with
/// advice that fails at the prompt.
const REVERT_ADVICE: &str = "`git status` and `git diff` show everything this touched: \
                             `git checkout -- <path>`\nrestores an updated file, `rm <path>` \
                             removes an added one.";

/// What to read next, given whether this report carried its diffs.
const fn review_advice(diffs: bool) -> &'static str {
    if diffs {
        "Each file's diff is above; `git status` and `git diff` show what is yours."
    } else {
        "Re-run without `--check` to see each file's diff."
    }
}

/// The machine-readable report, for CI.
#[must_use]
pub fn json(report: &ScaffoldReport) -> serde_json::Value {
    serde_json::json!({
        "baseline": report.baseline,
        "target": report.target,
        "named": report.named,
        "scaffolded_by_newer": report.scaffolded_by_newer,
        "has_manifest": report.has_manifest,
        "outcome": report.outcome.label(),
        "drift": report.drifted(),
        "workspace_member": report.workspace_member,
        // The plan, and the part of it that reached disk — the same split the
        // app-code report draws, for the same reason: "what would this do" and
        // "what is on disk now" are different questions, and a gate that reads
        // the wrong one calls a preview a completed upgrade.
        "writable": report.applicable().len(),
        "written": match report.outcome {
            super::Outcome::Preview => 0,
            super::Outcome::Applied => report.applicable().len(),
            super::Outcome::Partial { written } => written,
        },
        "conflicts": report.conflicts().len(),
        "pinned": report
            .entries
            .iter()
            .filter(|entry| entry.status == Status::Pinned)
            .count(),
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
            version: Some("0.7.0".to_owned()),
            written_by: Some("0.7.0".to_owned()),
            options: GenerateOptions {
                with_api: true,
                with_i18n: true,
                ..GenerateOptions::default()
            },
            digests,
            pinned: BTreeSet::new(),
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
            version: Some("0.7.0".to_owned()),
            written_by: Some("0.7.0".to_owned()),
            options: GenerateOptions::default(),
            digests: digests.clone(),
            pinned: BTreeSet::new(),
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
            version: Some("0.7.0".to_owned()),
            written_by: Some("0.7.0".to_owned()),
            options: GenerateOptions::default(),
            digests,
            pinned: BTreeSet::new(),
        };
        manifest.save(tmp.path()).unwrap();
        let loaded = Manifest::load(tmp.path()).expect("written manifest loads");
        assert_eq!(loaded.digests, manifest.digests);
        assert_eq!(loaded.version.as_deref(), Some("0.7.0"));
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
        let files = current_files(tmp.path(), opts).expect("named project");
        for (path, contents) in &files {
            write(tmp.path(), path, contents);
        }
        Manifest::for_files(env!("CARGO_PKG_VERSION"), opts, &files)
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
        manifest.version = Some("0.5.0".to_owned());
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
        plan(root, env!("CARGO_PKG_VERSION"))
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
        manifest.version = Some("0.5.0".to_owned());
        manifest.save(tmp.path()).unwrap();

        let report = plan_in(tmp.path());
        assert_eq!(report.baseline.as_deref(), Some("0.5.0"));
        assert_eq!(report.target, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_summary_links_the_release_upgrade_guide() {
        let tmp = scaffolded(GenerateOptions::default());
        let report = plan_in(tmp.path());
        assert!(
            report
                .guide
                .contains(&format!("docs/migrations/{}.md", env!("CARGO_PKG_VERSION"))),
            "{}",
            report.guide
        );
        assert!(render_text(&report).contains(&report.guide));
    }

    #[test]
    fn a_release_with_no_guide_links_the_guide_index() {
        // Not every release ships a migration guide, and a summary that links a
        // 404 at the moment it says "go read this" is worse than one that links
        // the index.
        let index = release_guide("9.9.0");
        assert!(index.ends_with("docs/migrations/README.md"), "{index}");
        let known = release_guide("0.7.0");
        assert!(known.ends_with("docs/migrations/0.7.0.md"), "{known}");
        // An unparsable version still resolves to something that opens.
        let junk = release_guide("not-a-version");
        assert!(junk.ends_with("docs/migrations/README.md"), "{junk}");
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

        let files = current_files(tmp.path(), GenerateOptions::default()).expect("named project");
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
        manifest.version = Some("0.5.0".to_owned());
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        let second = plan_in(tmp.path());
        assert!(!second.drifted(), "{}", render_text(&second));
        assert_eq!(second.baseline.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn apply_leaves_the_baseline_version_alone_while_conflicts_remain() {
        // Recording "you are on 0.7.0" while files are still unreconciled would
        // make the next run's report say the upgrade already happened.
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "Dockerfile", "FROM scratch\n");
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = Some("0.5.0".to_owned());
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            Manifest::load(tmp.path()).unwrap().version.as_deref(),
            Some("0.5.0")
        );
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
        assert_eq!(value["target"], env!("CARGO_PKG_VERSION"));
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

    #[test]
    fn a_file_that_is_not_readable_text_is_a_conflict_not_an_addition() {
        // `read_to_string` fails for a file that exists but is not UTF-8, and
        // conflating that with "absent" would classify it `add` and then
        // happily overwrite whatever is really in there.
        let tmp = scaffolded(GenerateOptions::default());
        fs::write(tmp.path().join(".env.example"), [0xff_u8, 0xfe, 0x00, 0x01]).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(
            status_of(&entries, ".env.example"),
            &Status::Conflict(ConflictReason::Unreadable)
        );

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read(tmp.path().join(".env.example")).unwrap(),
            vec![0xff_u8, 0xfe, 0x00, 0x01],
            "an unreadable file must survive --apply byte for byte"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_scaffold_file_is_a_conflict_and_is_never_written_through() {
        // Writing through a link can leave the project entirely, which is the
        // same reason the app-code codemods refuse to follow one.
        let tmp = scaffolded(GenerateOptions::default());
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "not mine to touch\n").unwrap();
        let link = tmp.path().join("clippy.toml");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(
            status_of(&entries, "clippy.toml"),
            &Status::Conflict(ConflictReason::Symlink)
        );

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "not mine to touch\n",
            "the link target must never be written through"
        );
    }

    #[test]
    fn the_scaffold_is_always_this_release_not_an_arbitrary_to_version() {
        // The CLI ships one set of templates: its own. `--to` selects which
        // codemods run; it cannot conjure a historical scaffold, and a report
        // claiming to reconcile to 0.6.0 while rendering 0.7.0's files would be
        // a lie. Downgrades are explicitly out of scope for this command.
        let tmp = scaffolded(GenerateOptions::default());
        let report = plan(tmp.path(), "0.6.0");
        assert_eq!(report.target, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn an_api_project_that_serves_something_from_static_is_still_an_api_project() {
        // `add` is the one verdict that writes with no baseline behind it, so a
        // wrong flavour guess does not merely mislabel a report — it seeds
        // Tailwind into a JSON API. A `static/` directory alone is not evidence
        // of a fullstack app: an API serves its `openapi.json` from one.
        let tmp = scaffolded(GenerateOptions {
            with_api: true,
            ..GenerateOptions::default()
        });
        fs::remove_dir_all(tmp.path().join(".autumn")).unwrap();
        write(tmp.path(), "static/openapi.json", "{}\n");

        let mut report = plan_in(tmp.path());
        assert!(
            !report
                .entries
                .iter()
                .any(|e| e.path == "tailwind.config.js"),
            "{:?}",
            report.changed().iter().map(|e| &e.path).collect::<Vec<_>>()
        );
        apply(&mut report).expect("apply");
        assert!(!tmp.path().join("tailwind.config.js").exists());
        assert!(!tmp.path().join("static/css/input.css").exists());
    }

    #[test]
    fn a_fullstack_project_missing_its_tailwind_config_is_still_fullstack() {
        // The mirror image: deleting one file must not silently reclassify the
        // project and take the rest of its CSS pipeline out of scope.
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_dir_all(tmp.path().join(".autumn")).unwrap();
        fs::remove_file(tmp.path().join("tailwind.config.js")).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "tailwind.config.js"), &Status::Add);
        assert_eq!(
            status_of(&entries, "static/css/input.css"),
            &Status::UpToDate
        );
    }

    #[test]
    fn a_project_whose_name_cannot_be_read_keeps_its_baseline() {
        // With no name there are no entries, and a manifest rebuilt from no
        // entries would prune every digest in it — destroying the baseline that
        // is the whole point of the file, in a run that reconciled nothing.
        let tmp = scaffolded(GenerateOptions::default());
        let before = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

        let mut report = plan_in(tmp.path());
        assert!(!report.named);
        assert!(report.entries.is_empty());
        apply(&mut report).expect("apply");

        assert_eq!(
            fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap(),
            before,
            "the baseline must survive a run that could not read the project name"
        );
    }

    #[test]
    fn a_run_that_changes_nothing_does_not_rewrite_the_manifest() {
        // `--apply` on an already-current project should leave the working tree
        // alone; rewriting an identical file is git noise on every run.
        let tmp = scaffolded(GenerateOptions::default());
        let before = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();
        let mtime = fs::metadata(tmp.path().join(MANIFEST_PATH))
            .unwrap()
            .modified()
            .unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        assert_eq!(
            fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap(),
            before
        );
        assert_eq!(
            fs::metadata(tmp.path().join(MANIFEST_PATH))
                .unwrap()
                .modified()
                .unwrap(),
            mtime,
            "an unchanged manifest must not be rewritten"
        );
    }

    #[test]
    fn an_unnamed_project_is_not_reported_as_drift_free() {
        // The refusal must not read as an all-clear: a CI gate that passes
        // because the tool could not look is worse than no gate.
        let tmp = scaffolded(GenerateOptions::default());
        fs::write(tmp.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let report = plan_in(tmp.path());
        assert!(!report.named);
        assert!(render_summary(&report).contains("Skipped"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_a_conflict_too() {
        // Checking only the leaf is not enough: `.github` symlinked to a shared
        // workflows tree lets `--apply` create a file outside the project
        // entirely, which the project's own `git status` would never show.
        let tmp = scaffolded(GenerateOptions::default());
        let outside = tmp.path().join("outside-workflows");
        fs::create_dir_all(&outside).unwrap();
        fs::remove_dir_all(tmp.path().join(".github")).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(".github")).unwrap();

        let mut report = plan_in(tmp.path());
        assert_eq!(
            status_of(&report.entries, ".github/workflows/ci.yml"),
            &Status::Conflict(ConflictReason::Symlink)
        );
        apply(&mut report).expect("apply");
        assert!(
            !outside.join("workflows/ci.yml").exists(),
            "nothing may be written through a symlinked parent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_whose_content_is_already_current_is_not_drift() {
        // Refusing to *write* through a link is right. Calling it drift when the
        // bytes already match makes `--check` red forever with no remedy.
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = scaffolded(GenerateOptions::default());
        let shared = tmp.path().join("shared-clippy.toml");
        let current = current_files(tmp.path(), GenerateOptions::default()).unwrap();
        fs::write(&shared, &current["clippy.toml"]).unwrap();
        fs::remove_file(tmp.path().join("clippy.toml")).unwrap();
        std::os::unix::fs::symlink(&shared, tmp.path().join("clippy.toml")).unwrap();
        let _ = fs::metadata(&shared).map(|m| m.permissions().mode());

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(status_of(&entries, "clippy.toml"), &Status::UpToDate);
        assert!(!drifted(&entries));
    }

    #[cfg(unix)]
    #[test]
    fn an_added_file_is_as_readable_as_one_the_scaffold_writes() {
        // `tempfile` creates 0600 by design. Renaming that into place gives an
        // added `ci.yml` or `input.css` a mode no other uid can read — invisible
        // in `git diff`, and fatal to a Docker stage that drops privileges.
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        let added = fs::metadata(tmp.path().join("rustfmt.toml"))
            .unwrap()
            .permissions()
            .mode();
        let scaffolded_by_new = fs::metadata(tmp.path().join("clippy.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            added & 0o777,
            scaffolded_by_new & 0o777,
            "an added file must be as readable as one `autumn new` writes"
        );
    }

    #[test]
    fn an_api_project_serving_its_own_stylesheet_is_still_an_api_project() {
        // `static/css/` existing is not evidence of the fullstack scaffold —
        // `static/css/input.css` is. Getting this wrong writes Tailwind into a
        // JSON API and then freezes the guess in the manifest.
        let tmp = scaffolded(GenerateOptions {
            with_api: true,
            ..GenerateOptions::default()
        });
        fs::remove_dir_all(tmp.path().join(".autumn")).unwrap();
        write(tmp.path(), "static/css/site.css", "body{}\n");

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert!(!tmp.path().join("tailwind.config.js").exists());
        assert!(!tmp.path().join("static/css/input.css").exists());
        assert!(Manifest::load(tmp.path()).unwrap().options.with_api);
    }

    #[test]
    fn a_workspace_member_is_not_given_the_files_a_workspace_root_owns() {
        // `clippy.toml`, `rustfmt.toml` and `rust-toolchain.toml` resolve from
        // the nearest ancestor, so a crate-local copy SHADOWS the workspace's —
        // silently dropping its lints and MSRV pin. `.github/` only runs from
        // the repository root. Seeding those into a member is not an upgrade.
        let outer = TempDir::new().unwrap();
        fs::write(
            outer.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        let root = outer.path().join("app");
        fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        );
        write(&root, "autumn.toml", "[server]\n");

        let report = plan(&root, "0.7.0");
        let offered: Vec<&str> = report.entries.iter().map(|e| e.path.as_str()).collect();
        for root_owned in [
            "clippy.toml",
            "rustfmt.toml",
            "rust-toolchain.toml",
            ".github/workflows/ci.yml",
        ] {
            assert!(
                !offered.contains(&root_owned),
                "{root_owned} in {offered:?}"
            );
        }
        // ...but the per-crate files are still reconciled.
        assert!(offered.contains(&"Dockerfile"), "{offered:?}");
        assert!(offered.contains(&"build.rs"), "{offered:?}");
        assert!(
            render_text(&report).contains("workspace"),
            "{}",
            render_text(&report)
        );
    }

    #[test]
    fn an_accepted_conflict_stops_holding_the_gate_red() {
        // A team that customises its Dockerfile must be able to finish the
        // review. Without this, `--check` is red forever and gets deleted.
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "Dockerfile", "FROM scratch\n# ours\n");
        assert!(plan_in(tmp.path()).drifted());

        accept(tmp.path(), &["Dockerfile".to_owned()]).expect("accept");

        let report = plan_in(tmp.path());
        assert_eq!(status_of(&report.entries, "Dockerfile"), &Status::Pinned);
        assert!(!report.drifted());
        // ...and it is still never written.
        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read_to_string(tmp.path().join("Dockerfile")).unwrap(),
            "FROM scratch\n# ours\n"
        );
        assert!(
            Manifest::load(tmp.path())
                .unwrap()
                .pinned
                .contains("Dockerfile")
        );
    }

    #[test]
    fn accepting_a_path_the_scaffold_does_not_own_is_refused() {
        let tmp = scaffolded(GenerateOptions::default());
        let error = accept(tmp.path(), &["src/main.rs".to_owned()]).expect_err("must refuse");
        assert!(error.contains("src/main.rs"), "{error}");
        assert!(Manifest::load(tmp.path()).unwrap().pinned.is_empty());
    }

    #[test]
    fn a_completed_apply_leaves_no_scratch_files_behind() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let stale = "# older\n";
        write(tmp.path(), "clippy.toml", stale);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(stale));
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        let strays: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".autumn-upgrade-"))
            .collect();
        assert!(strays.is_empty(), "scratch files left behind: {strays:?}");
    }

    #[test]
    fn a_file_that_merely_looks_like_scratch_is_neither_deleted_nor_a_blocker() {
        // Nothing outside the framework-owned set may be touched, and that
        // includes a path that happens to resemble this command's own staging
        // file — someone else's, or a concurrent `--apply`'s, whose deletion
        // would defeat the very race protection staging exists for.
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();
        let lookalike = tmp.path().join(".autumn-upgrade-rustfmt.toml.tmp");
        fs::write(&lookalike, "not this command's to delete\n").unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("a lookalike is not a blocker");

        let files = current_files(tmp.path(), GenerateOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap(),
            files["rustfmt.toml"],
            "the addition still lands"
        );
        assert_eq!(
            fs::read_to_string(&lookalike).unwrap(),
            "not this command's to delete\n",
            "an unowned file must survive untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_manifest_is_never_written_through() {
        // The manifest is written by a different path than the scaffold files,
        // and had none of their symlink protection: a symlinked
        // `.autumn/scaffold.toml` would have been followed and its target
        // outside the project truncated, invisibly to the project's own
        // `git diff`.
        let tmp = scaffolded(GenerateOptions::default());
        let outside = tmp.path().join("outside.toml");
        fs::write(&outside, "not mine to touch\n").unwrap();
        fs::remove_file(tmp.path().join(MANIFEST_PATH)).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(MANIFEST_PATH)).unwrap();

        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut report = plan_in(tmp.path());
        let failure = apply(&mut report).expect_err("the baseline cannot be recorded");
        assert_eq!(failure.path, MANIFEST_PATH);

        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "not mine to touch\n",
            "the link target must never be written through"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_autumn_directory_is_never_written_through() {
        let tmp = scaffolded(GenerateOptions::default());
        let outside = tmp.path().join("outside-dir");
        fs::create_dir_all(&outside).unwrap();
        fs::remove_dir_all(tmp.path().join(".autumn")).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(".autumn")).unwrap();

        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut report = plan_in(tmp.path());
        let failure = apply(&mut report).expect_err("the baseline cannot be recorded");
        assert_eq!(failure.path, MANIFEST_PATH);

        assert!(
            !outside.join("scaffold.toml").exists(),
            "nothing may be written through a symlinked parent"
        );
    }

    #[test]
    fn an_unrecognised_flavor_is_no_baseline_rather_than_a_guess() {
        // A `flavor` this CLI does not know — a typo, corruption, or a value a
        // newer release writes — must not silently read as `fullstack`. The
        // digests would still be trusted, so an API project's `Dockerfile`
        // would be classified `update` and replaced with the fullstack one
        // while its `Cargo.toml` stayed API-shaped.
        let tmp = scaffolded(GenerateOptions {
            with_api: true,
            ..GenerateOptions::default()
        });
        let text = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();
        fs::write(
            tmp.path().join(MANIFEST_PATH),
            text.replace("flavor = \"api\"", "flavor = \"fullstack-v2\""),
        )
        .unwrap();

        assert!(Manifest::load(tmp.path()).is_none());

        let before = fs::read_to_string(tmp.path().join("Dockerfile")).unwrap();
        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read_to_string(tmp.path().join("Dockerfile")).unwrap(),
            before,
            "an unreadable flavor must fall back to the conservative no-baseline path"
        );
        assert!(!tmp.path().join("tailwind.config.js").exists());
    }

    #[test]
    fn publishing_an_addition_never_replaces_a_destination_that_appeared() {
        // `rename` replaces the destination on Unix, so a file another process
        // creates during the staging window — after the plan's re-read, while
        // the bytes are being synced — would be silently clobbered by the very
        // step that advertises it will not.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("arrived.toml");
        fs::write(&path, "someone else got here first\n").unwrap();

        let error = publish(&path, "ours\n", Publish::Create)
            .expect_err("must refuse")
            .to_string();
        assert!(error.contains("appeared"), "{error}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "someone else got here first\n"
        );
        assert!(no_scratch_beside(&path), "no scratch left behind");
    }

    #[test]
    fn publishing_a_replacement_takes_the_destination() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("existing.toml");
        fs::write(&path, "old\n").unwrap();

        publish(&path, "new\n", Publish::Swap("old\n".to_owned())).expect("replace");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert!(no_scratch_beside(&path), "no scratch left behind");
    }

    #[test]
    fn a_swap_refuses_a_destination_that_changed_after_the_plan() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("existing.toml");
        fs::write(&path, "theirs, written after the plan\n").unwrap();

        let error = publish(&path, "new\n", Publish::Swap("planned\n".to_owned()))
            .expect_err("must refuse");
        assert!(matches!(error, PublishError::Moved(_)), "{error}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "theirs, written after the plan\n"
        );
        assert!(no_scratch_beside(&path), "no scratch left behind");
    }

    #[test]
    fn a_swap_refuses_a_destination_that_vanished() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gone.toml");

        let error = publish(&path, "new\n", Publish::Swap("planned\n".to_owned()))
            .expect_err("must refuse");
        assert!(matches!(error, PublishError::Moved(_)), "{error}");
        assert!(!path.exists());
        assert!(no_scratch_beside(&path), "no scratch left behind");
    }

    #[test]
    fn a_swap_of_matching_text_ignores_line_endings() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("crlf.toml");
        fs::write(&path, "a\r\nb\r\n").unwrap();

        publish(&path, "new\n", Publish::Swap("a\nb\n".to_owned())).expect("swap");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[test]
    fn an_update_runs_again_when_a_writer_changed_the_manifest_first() {
        let tmp = scaffolded(GenerateOptions::default());
        let mut runs = 0;
        let root = tmp.path().to_path_buf();
        let saved = Manifest::update(tmp.path(), |current| {
            runs += 1;
            let mut manifest = current.expect("manifest");
            if runs == 1 {
                // A second command pins its own file between our read and swap.
                let mut theirs = manifest.clone();
                theirs.pinned.insert("Dockerfile".to_owned());
                fs::write(root.join(MANIFEST_PATH), theirs.render()).unwrap();
            }
            manifest.pinned.insert("build.rs".to_owned());
            Ok(manifest)
        })
        .expect("update");
        assert_eq!(runs, 2);
        let pinned: Vec<&str> = saved.pinned.iter().map(String::as_str).collect();
        assert_eq!(pinned, ["Dockerfile", "build.rs"]);
        assert_eq!(Manifest::load(tmp.path()).unwrap(), saved);
    }

    #[test]
    fn an_update_gives_up_when_the_manifest_never_settles() {
        let tmp = scaffolded(GenerateOptions::default());
        let root = tmp.path().to_path_buf();
        let mut runs = 0;
        let error = Manifest::update(tmp.path(), |current| {
            runs += 1;
            let mut manifest = current.expect("manifest");
            let mut theirs = manifest.clone();
            theirs.pinned.insert(format!("racer-{runs}"));
            fs::write(root.join(MANIFEST_PATH), theirs.render()).unwrap();
            manifest.pinned.insert("mine".to_owned());
            Ok(manifest)
        })
        .expect_err("must stop");
        assert_eq!(runs, SWAP_ATTEMPTS);
        assert!(error.to_string().contains("changing"), "{error}");
        assert!(!Manifest::load(tmp.path()).unwrap().pinned.contains("mine"));
    }

    #[test]
    fn an_update_never_builds_from_nothing_while_a_claim_is_in_flight() {
        // A swap in progress leaves the manifest path absent. Reading that as
        // "no manifest" would publish a blank one and erase the history.
        let tmp = scaffolded(GenerateOptions::default());
        let manifest_path = tmp.path().join(MANIFEST_PATH);
        let held = manifest_path.with_file_name(".autumn-upgrade-scaffold.toml.claim.old");
        fs::rename(&manifest_path, &held).unwrap();

        let mut runs = 0;
        let error = Manifest::update(tmp.path(), |current| {
            runs += 1;
            Ok(current.unwrap_or_else(|| panic!("built from nothing")))
        })
        .expect_err("must not build from nothing");
        assert!(error.to_string().contains("interrupted"), "{error}");
        assert_eq!(runs, 0);
        assert!(held.exists(), "the held copy is never deleted");
    }

    #[test]
    fn an_accept_judges_ownership_by_the_manifest_not_by_a_claim_gap() {
        // While a swap holds the manifest, a plain load sees nothing and infers
        // the API flavor, which does not own the Tailwind files.
        let tmp = scaffolded(GenerateOptions::default());
        for marker in [
            "tailwind.config.js",
            "static/css/input.css",
            "static/js/htmx.min.js",
        ] {
            let _ = fs::remove_file(tmp.path().join(marker));
        }
        let manifest_path = tmp.path().join(MANIFEST_PATH);
        let held = manifest_path.with_file_name(".autumn-upgrade-scaffold.toml.claim.old");
        fs::rename(&manifest_path, &held).unwrap();
        let restore = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            fs::rename(&held, &manifest_path).unwrap();
        });

        let accepted = accept(tmp.path(), &["tailwind.config.js".to_owned()]);
        restore.join().unwrap();
        let manifest = accepted.expect("owned by the recorded flavor");
        assert!(manifest.pinned.contains("tailwind.config.js"));
        assert!(!manifest.digests.is_empty(), "the history survives");
    }

    #[test]
    fn a_load_waits_out_a_claim_instead_of_seeing_no_manifest() {
        // `--check` and `--apply` plan from `load`. During a swap the path is
        // absent, and "no manifest" would report spurious conflicts.
        let tmp = scaffolded(GenerateOptions::default());
        let manifest_path = tmp.path().join(MANIFEST_PATH);
        let held = manifest_path.with_file_name(".autumn-upgrade-scaffold.toml.claim.old");
        fs::rename(&manifest_path, &held).unwrap();
        let restore = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            fs::rename(&held, &manifest_path).unwrap();
        });

        let loaded = Manifest::load(tmp.path());
        restore.join().unwrap();
        assert!(loaded.is_some(), "the manifest was only held aside");
    }

    #[test]
    fn a_planning_read_waits_out_a_claim_on_a_scaffold_file() {
        // The same gap hides any scaffold file a swap holds aside. Without the
        // wait, its recorded digest and an absent file read as "removed".
        let tmp = scaffolded(GenerateOptions::default());
        let path = tmp.path().join("Dockerfile");
        let held = path.with_file_name(".autumn-upgrade-Dockerfile.claim.old");
        fs::rename(&path, &held).unwrap();
        let restore = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            fs::rename(&held, &path).unwrap();
        });

        let report = plan_in(tmp.path());
        restore.join().unwrap();
        assert_eq!(*status_of(&report.entries, "Dockerfile"), Status::UpToDate);
    }

    #[test]
    fn a_claim_written_through_an_open_handle_is_kept_not_deleted() {
        // A writer that opened the old file before the swap keeps writing into
        // it. Deleting that copy would lose the write without a trace.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("existing.toml");
        let claim = tempfile::Builder::new()
            .prefix(".autumn-upgrade-existing.toml.")
            .suffix(CLAIM_SUFFIX)
            .tempfile_in(tmp.path())
            .unwrap()
            .into_temp_path();
        fs::write(&claim, "planned\nand a late write\n").unwrap();
        let held = claim.to_path_buf();

        let error = finish_claim(claim, "planned\n").expect_err("must keep the copy");
        assert!(matches!(error, PublishError::Late(_)), "{error}");
        assert!(error.to_string().contains(&*held.to_string_lossy()));
        assert_eq!(
            fs::read_to_string(&held).unwrap(),
            "planned\nand a late write\n"
        );

        let clean = tempfile::Builder::new()
            .prefix(".autumn-upgrade-existing.toml.")
            .suffix(CLAIM_SUFFIX)
            .tempfile_in(tmp.path())
            .unwrap()
            .into_temp_path();
        fs::write(&clean, "planned\n").unwrap();
        let gone = clean.to_path_buf();
        finish_claim(clean, "planned\n").expect("unchanged copy is dropped");
        assert!(!gone.exists());
        assert!(!path.exists());
    }

    #[test]
    fn an_unchanged_update_still_checks_the_manifest_is_current() {
        // `change` returns what it read, so the render matches. A writer that
        // replaced the file after the read must still be seen.
        let tmp = scaffolded(GenerateOptions::default());
        let root = tmp.path().to_path_buf();
        let mut runs = 0;
        Manifest::update(tmp.path(), |current| {
            runs += 1;
            let manifest = current.expect("manifest");
            if runs == 1 {
                let mut theirs = manifest.clone();
                theirs.pinned.insert("Dockerfile".to_owned());
                fs::write(root.join(MANIFEST_PATH), theirs.render()).unwrap();
            }
            Ok(manifest)
        })
        .expect("update");
        assert_eq!(runs, 2, "the stale snapshot must not be reported as done");
    }

    #[test]
    fn a_project_known_only_by_a_held_manifest_is_still_a_project() {
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("autumn.toml")).unwrap();
        let manifest_path = tmp.path().join(MANIFEST_PATH);
        fs::rename(
            &manifest_path,
            manifest_path.with_file_name(".autumn-upgrade-scaffold.toml.claim.old"),
        )
        .unwrap();
        assert!(is_project(tmp.path()));
    }

    #[test]
    fn a_claim_that_never_ends_reads_as_unreadable_not_as_absent() {
        // A crashed swap leaves the file held aside. Calling it removed would
        // offer to restore it; unreadable is a conflict, which never writes.
        let tmp = scaffolded(GenerateOptions::default());
        let path = tmp.path().join("Dockerfile");
        fs::rename(
            &path,
            path.with_file_name(".autumn-upgrade-Dockerfile.claim.old"),
        )
        .unwrap();

        let report = plan_in(tmp.path());
        assert_eq!(
            *status_of(&report.entries, "Dockerfile"),
            Status::Conflict(ConflictReason::Unreadable)
        );
    }

    #[test]
    fn a_claim_on_one_file_does_not_block_its_siblings() {
        // A stale claim for `Dockerfile` says nothing about `rustfmt.toml`,
        // which is simply missing and is offered as an addition.
        let tmp = scaffolded(GenerateOptions::default());
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();
        fs::write(
            tmp.path().join(".autumn-upgrade-Dockerfile.stale.old"),
            "old\n",
        )
        .unwrap();

        let report = plan_in(tmp.path());
        assert_eq!(*status_of(&report.entries, "rustfmt.toml"), Status::Add);
    }

    #[test]
    fn an_apply_planned_before_a_newer_release_wrote_refuses_to_record() {
        // The plan was made against older digests. A newer release then
        // rewrote the manifest. Recording now would move it backwards.
        let tmp = scaffolded(GenerateOptions::default());
        let stale = "# older\n";
        write(tmp.path(), "clippy.toml", stale);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(stale));
        manifest.save(tmp.path()).unwrap();
        let mut report = plan_in(tmp.path());

        let mut newer = Manifest::load(tmp.path()).unwrap();
        newer.version = Some("99.0.0".to_owned());
        newer.written_by = Some("99.0.0".to_owned());
        newer.save(tmp.path()).unwrap();

        let failure = apply(&mut report).expect_err("must not record over a newer manifest");
        assert_eq!(failure.path, MANIFEST_PATH);
        let kept = Manifest::load(tmp.path()).unwrap();
        assert_eq!(kept.written_by.as_deref(), Some("99.0.0"));
    }

    #[test]
    fn only_a_late_write_error_means_the_new_file_is_already_there() {
        assert!(PublishError::Late("x".to_owned()).published());
        assert!(!PublishError::Moved("x".to_owned()).published());
        assert!(!PublishError::Retained("x".to_owned()).published());
        assert!(!PublishError::Failed("x".to_owned()).published());
    }

    #[test]
    fn a_stalled_claim_is_not_advised_to_be_renamed_over_a_manifest() {
        let tmp = scaffolded(GenerateOptions::default());
        let manifest_path = tmp.path().join(MANIFEST_PATH);
        fs::rename(
            &manifest_path,
            manifest_path.with_file_name(".autumn-upgrade-scaffold.toml.stalled.old"),
        )
        .unwrap();
        let error =
            Manifest::update(tmp.path(), |_| Err("unreachable".to_owned())).expect_err("must stop");
        let text = error.to_string();
        assert!(text.contains("still missing"), "{text}");
    }

    // Racy by design, and it needs the full swap, which Windows does not use.
    #[cfg(not(windows))]
    #[test]
    fn two_accepts_at_once_keep_both_pins() {
        for _ in 0..20 {
            let tmp = scaffolded(GenerateOptions::default());
            let root = tmp.path().to_path_buf();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let spawn = |path: &'static str| {
                let (root, barrier) = (root.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    accept(&root, &[path.to_owned()])
                })
            };
            let (left, right) = (spawn("Dockerfile"), spawn("build.rs"));
            left.join().unwrap().expect("first accept");
            right.join().unwrap().expect("second accept");
            let pinned = Manifest::load(tmp.path()).unwrap().pinned;
            assert!(pinned.contains("Dockerfile"), "{pinned:?}");
            assert!(pinned.contains("build.rs"), "{pinned:?}");
            let kept = Manifest::load(tmp.path()).unwrap();
            assert!(kept.version.is_some(), "the baseline survives");
            assert!(!kept.digests.is_empty(), "the digests survive");
            assert!(no_scratch_beside(&root.join(MANIFEST_PATH)));
        }
    }

    /// Whether the directory holding `path` is free of staging files.
    fn no_scratch_beside(path: &std::path::Path) -> bool {
        !fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".autumn-upgrade-")
            })
    }

    #[test]
    fn a_preview_and_an_apply_agree_about_build_rs() {
        // `build.rs` is the one framework-owned file the codemods also scan, so
        // it is exactly the file whose preview must not disagree with what
        // `--apply` then does. Classifying the preview against the old contents
        // reported a writable `update` for a file the apply would refuse.
        let tmp = scaffolded(GenerateOptions::default());
        // Stale but provably untouched: without the codemods in the picture
        // this is a writable `update`.
        let stale = "fn main() { /* an older release's build.rs */ }\n";
        write(tmp.path(), "build.rs", stale);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest
            .digests
            .insert("build.rs".to_owned(), digest(stale));
        manifest.save(tmp.path()).unwrap();
        assert_eq!(
            status_of(&plan_in(tmp.path()).entries, "build.rs"),
            &Status::Update
        );

        let migrated = BTreeMap::from([("build.rs".to_owned(), plan_of(stale, stale))]);
        let previewed = plan_after(tmp.path(), "0.7.0", &migrated);
        assert_eq!(
            status_of(&previewed.entries, "build.rs"),
            &Status::Conflict(ConflictReason::MigratedThisRun)
        );
        assert!(
            !previewed
                .applicable()
                .iter()
                .any(|entry| entry.path == "build.rs"),
            "a file the codemods rewrite is never a writable scaffold update"
        );
    }

    #[test]
    fn a_preview_and_an_apply_show_the_same_build_rs_diff() {
        // The codemods change `build.rs`. Its template has also changed. A
        // preview reads the old bytes from disk. `--apply` reads the new bytes.
        // Both must diff the planned bytes.
        let tmp = scaffolded(GenerateOptions::default());
        let before = "fn main() { old_call(); }\n";
        let after = "fn main() { new_call(); }\n";
        write(tmp.path(), "build.rs", before);
        let migrated = BTreeMap::from([("build.rs".to_owned(), plan_of(before, after))]);

        let preview = plan_after(tmp.path(), "0.7.0", &migrated);
        write(tmp.path(), "build.rs", after);
        let apply = plan_after(tmp.path(), "0.7.0", &migrated);

        let diff_of = |report: &ScaffoldReport| {
            report
                .entries
                .iter()
                .find(|entry| entry.path == "build.rs")
                .unwrap()
                .diff
                .clone()
        };
        assert_eq!(diff_of(&preview), diff_of(&apply));
        assert!(diff_of(&preview).contains("new_call"));
        assert!(!diff_of(&preview).contains("old_call"));
    }

    fn plan_of(original: &str, updated: &str) -> Planned {
        Planned {
            original: original.to_owned(),
            updated: updated.to_owned(),
        }
    }

    /// The verdict for `build.rs` before and after the codemods write it.
    fn build_rs_verdicts(before: &str, planned: &str) -> (Status, Status) {
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "build.rs", before);
        let migrated = BTreeMap::from([("build.rs".to_owned(), plan_of(before, planned))]);
        let verdict = |tmp: &TempDir| {
            let report = plan_after(tmp.path(), "0.7.0", &migrated);
            *status_of(&report.entries, "build.rs")
        };
        let preview = verdict(&tmp);
        write(tmp.path(), "build.rs", planned);
        (preview, verdict(&tmp))
    }

    #[test]
    fn a_codemod_that_reaches_the_template_is_up_to_date_in_both_modes() {
        let template = current_files(
            scaffolded(GenerateOptions::default()).path(),
            GenerateOptions::default(),
        )
        .unwrap()["build.rs"]
            .clone();
        let (preview, apply) = build_rs_verdicts("fn main() { old_call(); }\n", &template);
        assert_eq!(preview, Status::UpToDate);
        assert_eq!(apply, Status::UpToDate);
    }

    #[test]
    fn a_codemod_that_leaves_the_template_is_a_conflict_in_both_modes() {
        let template = current_files(
            scaffolded(GenerateOptions::default()).path(),
            GenerateOptions::default(),
        )
        .unwrap()["build.rs"]
            .clone();
        let (preview, apply) = build_rs_verdicts(&template, "fn main() { new_call(); }\n");
        let conflict = Status::Conflict(ConflictReason::MigratedThisRun);
        assert_eq!(preview, conflict);
        assert_eq!(apply, conflict);
    }

    #[test]
    fn a_crlf_planned_text_diffs_like_its_lf_form() {
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "build.rs", "fn main() { old_call(); }\n");
        let diff = |planned: &str| {
            let migrated = BTreeMap::from([(
                "build.rs".to_owned(),
                plan_of("fn main() { old_call(); }\n", planned),
            )]);
            let report = plan_after(tmp.path(), "0.7.0", &migrated);
            report
                .entries
                .into_iter()
                .find(|entry| entry.path == "build.rs")
                .unwrap()
                .diff
        };
        let lf = diff("fn main() { new_call(); }\n");
        assert_eq!(lf, diff("fn main() { new_call(); }\r\n"));
        assert!(!lf.contains('\r'));
    }

    #[test]
    fn a_file_changed_after_the_scan_keeps_its_real_diff() {
        // The plan names neither the bytes on disk nor the bytes it writes. The
        // report must not claim `up to date` for a file nobody wrote.
        let tmp = scaffolded(GenerateOptions::default());
        let template =
            current_files(tmp.path(), GenerateOptions::default()).unwrap()["build.rs"].clone();
        write(tmp.path(), "build.rs", "fn main() { edited_since(); }\n");
        let migrated = BTreeMap::from([(
            "build.rs".to_owned(),
            plan_of("fn main() { old_call(); }\n", &template),
        )]);
        let report = plan_after(tmp.path(), "0.7.0", &migrated);
        let entry = report
            .entries
            .iter()
            .find(|entry| entry.path == "build.rs")
            .unwrap();
        assert_eq!(
            entry.status,
            Status::Conflict(ConflictReason::MigratedThisRun)
        );
        assert!(entry.diff.contains("edited_since"));
    }

    #[test]
    fn a_migrated_path_the_scaffold_does_not_own_is_ignored() {
        let tmp = scaffolded(GenerateOptions::default());
        let migrated = BTreeMap::from([("src/main.rs".to_owned(), plan_of("", "fn main() {}\n"))]);
        let report = plan_after(tmp.path(), "0.7.0", &migrated);
        assert!(
            report
                .entries
                .iter()
                .all(|entry| entry.path != "src/main.rs")
        );
    }

    #[test]
    fn a_nested_workspace_root_is_a_root_not_a_member() {
        // `[package]` and `[workspace]` in one manifest is the standard way for
        // a crate below another workspace to form its own. It owns its
        // toolchain, lint, formatting and CI files — treating it as a member
        // drops exactly those out of scope, so drift in them goes unreported.
        let outer = TempDir::new().unwrap();
        fs::write(
            outer.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        let root = outer.path().join("app");
        fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[workspace]\n",
        );
        write(&root, "autumn.toml", "[server]\n");

        let report = plan(&root, "0.7.0");
        assert!(
            !report.workspace_member,
            "its own `[workspace]` makes it a root"
        );
        let offered: Vec<&str> = report.entries.iter().map(|e| e.path.as_str()).collect();
        for owned in ["clippy.toml", "rustfmt.toml", "rust-toolchain.toml"] {
            assert!(offered.contains(&owned), "{owned} missing from {offered:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_manifest_is_not_a_baseline_to_trust() {
        // Refusing to WRITE through a linked manifest was only half of it: read
        // through one, and whoever controls the target supplies the digests.
        // A digest matching the project's current file turns it into an
        // `update`, and the next `--apply` overwrites a file nobody vouched
        // for.
        let tmp = scaffolded(GenerateOptions::default());
        let outside = tmp.path().join("outside-manifest.toml");
        let real = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();
        fs::write(&outside, &real).unwrap();
        fs::remove_file(tmp.path().join(MANIFEST_PATH)).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(MANIFEST_PATH)).unwrap();

        assert!(
            Manifest::load(tmp.path()).is_none(),
            "a linked manifest vouches for nothing"
        );

        // A file it would otherwise have called `update` is a conflict instead.
        let stale = "# older\n";
        write(tmp.path(), "clippy.toml", stale);
        let mut linked = Manifest::parse(&real).unwrap();
        linked
            .digests
            .insert("clippy.toml".to_owned(), digest(stale));
        fs::write(&outside, linked.render()).unwrap();

        let entries = plan_in(tmp.path()).entries;
        assert_eq!(
            status_of(&entries, "clippy.toml"),
            &Status::Conflict(ConflictReason::NoBaseline)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_newly_created_manifest_is_as_readable_as_the_files_it_describes() {
        // The manifest is created, not replaced, on every `autumn new` and on a
        // legacy project's first `--accept`. Publishing it as a replacement
        // finds no destination permissions to copy, so `tempfile`'s 0600 would
        // stick — and a manifest another uid cannot read is discarded as
        // unreadable, which throws away every baseline it holds and turns the
        // next upgrade into a wall of conflicts.
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = scaffolded(GenerateOptions::default());
        let manifest = fs::metadata(tmp.path().join(MANIFEST_PATH))
            .unwrap()
            .permissions()
            .mode();
        let ordinary = fs::metadata(tmp.path().join("clippy.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            manifest & 0o777,
            ordinary & 0o777,
            "the manifest must be as readable as the files it describes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_apply_that_cannot_record_its_baseline_is_not_a_success() {
        // The scaffold files are written correctly, but without the baseline
        // the very next `--check` reports them as conflicts and exits 3. An
        // apply that guarantees a red gate afterwards has not succeeded, and
        // reporting exit 0 tells a CI script otherwise.
        let tmp = scaffolded(GenerateOptions::default());
        let outside = tmp.path().join("outside.toml");
        fs::write(&outside, "not mine\n").unwrap();
        fs::remove_file(tmp.path().join(MANIFEST_PATH)).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(MANIFEST_PATH)).unwrap();
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();

        let mut report = plan_in(tmp.path());
        let failure = apply(&mut report).expect_err("must not report success");
        assert_eq!(failure.path, MANIFEST_PATH);
        assert!(failure.error.contains("baseline"), "{}", failure.error);

        // The files it did write are still correct, and the run says how many.
        let files = current_files(tmp.path(), GenerateOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap(),
            files["rustfmt.toml"]
        );
        assert_eq!(failure.written, report.applicable().len());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "not mine\n");
    }

    #[test]
    fn a_project_scaffolded_by_a_newer_release_is_refused_not_downgraded() {
        // The recorded digests still match, so every untouched file looks like
        // a writable `update` against this older CLI's templates — and applying
        // them would DOWNGRADE the Dockerfile, build script and CI workflow.
        // Downgrades are out of scope; silently performing one is worse still.
        let tmp = scaffolded(GenerateOptions::default());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = Some("99.0.0".to_owned());
        manifest.save(tmp.path()).unwrap();
        // An older template on disk, matching its recorded digest exactly as a
        // newer CLI's output would.
        let newer = "# written by a newer release\n";
        write(tmp.path(), "clippy.toml", newer);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(newer));
        manifest.save(tmp.path()).unwrap();

        let report = plan_in(tmp.path());
        assert!(report.entries.is_empty(), "nothing may be reconciled");
        assert!(!report.drifted());
        let text = render_text(&report);
        assert!(text.contains("newer"), "{text}");

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read_to_string(tmp.path().join("clippy.toml")).unwrap(),
            newer,
            "a newer release's file must never be downgraded"
        );
    }

    #[test]
    fn an_impossible_option_combination_is_no_baseline() {
        // `--api` with `--daemon` is a combination `autumn new` refuses: they
        // are different app shapes with conflicting feature sets. A manifest
        // claiming both is corrupt, and trusting it renders the API templates
        // against a fullstack daemon's digests — making its `Dockerfile` and
        // `build.rs` look like safe updates.
        let tmp = scaffolded(GenerateOptions::default());
        let text = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();
        fs::write(
            tmp.path().join(MANIFEST_PATH),
            text.replace("flavor = \"fullstack\"", "flavor = \"api\"")
                .replace("daemon = false", "daemon = true"),
        )
        .unwrap();

        assert!(
            Manifest::load(tmp.path()).is_none(),
            "an incoherent option set vouches for nothing"
        );

        let before = fs::read_to_string(tmp.path().join("Dockerfile")).unwrap();
        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read_to_string(tmp.path().join("Dockerfile")).unwrap(),
            before
        );
    }

    #[test]
    fn refusing_a_newer_scaffold_leaves_its_manifest_untouched() {
        // The refusal protects the files; it must protect their provenance too.
        // A report with no entries rebuilds the manifest from nothing, pruning
        // every digest and stamping this older CLI's version over the newer
        // one — destroying the very record that triggered the refusal.
        let tmp = scaffolded(GenerateOptions::default());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = Some("99.0.0".to_owned());
        manifest.save(tmp.path()).unwrap();
        let before = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();

        let mut report = plan_in(tmp.path());
        assert!(report.scaffolded_by_newer.is_some());
        apply(&mut report).expect("apply");

        assert_eq!(
            fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap(),
            before,
            "a refused run must not rewrite the baseline it refused over"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_added_file_gets_the_mode_an_ordinary_write_would_give_it() {
        // A directory's access bits do not encode the umask: under umask 022 a
        // `0775` checkout yields `0664` from `mode & 0o666` but `0644` from an
        // ordinary write, quietly granting group write. The only way to get
        // this right is to create the file with normal 0666 semantics and let
        // the umask apply.
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = scaffolded(GenerateOptions::default());
        fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
        fs::remove_file(tmp.path().join("rustfmt.toml")).unwrap();
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.digests.remove("rustfmt.toml");
        manifest.save(tmp.path()).unwrap();

        // What an ordinary write produces in this very directory, whatever the
        // umask happens to be.
        let probe = tmp.path().join("probe.txt");
        fs::write(&probe, "x").unwrap();
        let expected = fs::metadata(&probe).unwrap().permissions().mode() & 0o777;

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        let added = fs::metadata(tmp.path().join("rustfmt.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            added, expected,
            "an added file must carry the mode an ordinary write would give it"
        );
    }

    #[test]
    fn a_partial_upgrade_by_a_newer_release_still_refuses_an_older_cli() {
        // `version` is held back while conflicts stand (so a half-finished
        // upgrade never reads as finished), but the digests advance to the
        // newer templates regardless. One field cannot mean both "the release
        // fully reconciled to" and "the newest renderer that wrote here" — and
        // reading the first as the second lets an older CLI trust newer digests
        // and downgrade the files they describe.
        let tmp = scaffolded(GenerateOptions::default());
        let newer = "# written by a newer release\n";
        write(tmp.path(), "clippy.toml", newer);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = Some("0.5.0".to_owned()); // held back by a conflict
        manifest.written_by = Some("99.0.0".to_owned()); // but 99.0.0 wrote these digests
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(newer));
        manifest.save(tmp.path()).unwrap();

        let report = plan_in(tmp.path());
        assert_eq!(report.scaffolded_by_newer.as_deref(), Some("99.0.0"));
        assert!(report.entries.is_empty());

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");
        assert_eq!(
            fs::read_to_string(tmp.path().join("clippy.toml")).unwrap(),
            newer,
            "a newer release's file must never be downgraded"
        );
    }

    #[test]
    fn the_renderer_high_water_mark_advances_even_when_the_baseline_cannot() {
        // A conflict holds `version` back; it must not hold back the record of
        // which renderer produced the digests, or the next older CLI to run
        // here has nothing to refuse on.
        let tmp = scaffolded(GenerateOptions::default());
        write(tmp.path(), "Dockerfile", "FROM scratch\n");
        let stale = "# older\n";
        write(tmp.path(), "clippy.toml", stale);
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = Some("0.5.0".to_owned());
        manifest.written_by = Some("0.5.0".to_owned());
        manifest
            .digests
            .insert("clippy.toml".to_owned(), digest(stale));
        manifest.save(tmp.path()).unwrap();

        let mut report = plan_in(tmp.path());
        apply(&mut report).expect("apply");

        let after = Manifest::load(tmp.path()).unwrap();
        assert_eq!(
            after.version.as_deref(),
            Some("0.5.0"),
            "the baseline still waits on the conflict"
        );
        assert_eq!(
            after.written_by.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "but this renderer wrote these digests"
        );
    }

    #[test]
    fn a_new_project_records_which_renderer_wrote_it() {
        let tmp = scaffolded(GenerateOptions::default());
        assert_eq!(
            Manifest::load(tmp.path()).unwrap().written_by.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn an_unparsable_recorded_version_is_no_baseline() {
        // A version that cannot be read cannot be compared, so it cannot be
        // refused either — and a manifest left trusted with digests a newer
        // renderer wrote is the downgrade path all over again. Unknown must
        // mean no baseline, which is what the doc comment on the comparison
        // already claimed and nothing implemented.
        for field in ["version", "written_by"] {
            let tmp = scaffolded(GenerateOptions::default());
            let text = fs::read_to_string(tmp.path().join(MANIFEST_PATH)).unwrap();
            fs::write(
                tmp.path().join(MANIFEST_PATH),
                text.replace(
                    &format!("{field} = \"{}\"", env!("CARGO_PKG_VERSION")),
                    &format!("{field} = \"not-a-version\""),
                ),
            )
            .unwrap();

            assert!(
                Manifest::load(tmp.path()).is_none(),
                "{field}: an unreadable version vouches for nothing"
            );

            // ...so a file a newer renderer wrote is a conflict, never an
            // update, and survives `--apply`.
            let newer = "# written by a newer release\n";
            write(tmp.path(), "clippy.toml", newer);
            let entries = plan_in(tmp.path()).entries;
            assert_eq!(
                status_of(&entries, "clippy.toml"),
                &Status::Conflict(ConflictReason::NoBaseline),
                "{field}"
            );

            let mut report = plan_in(tmp.path());
            apply(&mut report).expect("apply");
            assert_eq!(
                fs::read_to_string(tmp.path().join("clippy.toml")).unwrap(),
                newer,
                "{field}"
            );
        }
    }

    #[test]
    fn an_absent_version_is_still_a_usable_baseline() {
        // Absent is not the same as unreadable: a project whose first upgrade
        // left conflicts standing has no `version` line at all, and its digests
        // are still good.
        let tmp = scaffolded(GenerateOptions::default());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        manifest.version = None;
        manifest.save(tmp.path()).unwrap();

        let loaded = Manifest::load(tmp.path()).expect("still a baseline");
        assert!(loaded.version.is_none());
        assert!(!loaded.digests.is_empty());
    }

    fn newer(recorded: &str) -> bool {
        is_newer(recorded, "0.8.0-rc.1")
    }

    #[test]
    fn provenance_compares_prereleases_by_full_semver_precedence() {
        // `Version`'s own ordering distinguishes only prerelease-vs-stable,
        // deliberately: codemod selection treats every prerelease of a triple
        // as interchangeable. Provenance cannot. Reusing that comparator makes
        // `0.8.0-rc.2` and `0.8.0-rc.1` equal, so an rc.1 CLI does not refuse
        // an rc.2 project and downgrades every file it renders.
        assert!(newer("0.8.0-rc.2"), "rc.2 is newer than rc.1");
        assert!(!newer("0.8.0-rc.1"), "the same release is not newer");
        assert!(!newer("0.8.0-rc.0"), "rc.0 is older");

        // A stable release outranks any prerelease of its own triple.
        assert!(newer("0.8.0"));
        assert!(!is_newer("0.8.0-rc.9", "0.8.0"));

        // Numeric identifiers compare numerically, not as text.
        assert!(is_newer("0.8.0-rc.10", "0.8.0-rc.9"));
        assert!(!is_newer("0.8.0-rc.9", "0.8.0-rc.10"));

        // Numeric identifiers rank below alphanumeric ones, and a longer set of
        // fields outranks a shorter one that is otherwise equal (SemVer 11.4).
        assert!(is_newer("0.8.0-alpha", "0.8.0-1"));
        assert!(is_newer("0.8.0-rc.1", "0.8.0-rc"));

        // The triple still dominates everything.
        assert!(is_newer("0.9.0-rc.1", "0.8.0"));
        assert!(!is_newer("0.8.0", "0.9.0-rc.1"));
    }
}
