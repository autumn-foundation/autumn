//! `autumn new --starter` — curated built-in and community (git) starters.
//!
//! A *starter* scaffolds a complete, runnable application archetype rather than
//! the minimal base project `autumn new <name>` produces. Built-in starters are
//! vetted, version-locked, and embedded in the CLI (no network fetch, no
//! confirmation). Community starters are fetched from a git URL (or `owner/repo`
//! shorthand) — their provenance is printed and confirmed before anything is
//! fetched or applied. A local directory acts as a community starter too, which
//! lets fixtures and authors test a starter without publishing it.
//!
//! Built-in and community starters share one [`manifest`] format
//! (`autumn-starter.toml`) and one render path — the same `{{…}}` substitution
//! [`crate::new`] uses for the base project (issue #993).

pub mod builtin;
pub mod git;
pub mod manifest;

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use include_dir::Dir;

use crate::generate::Flags;
use crate::generate::emit::Plan;
use crate::new::{TemplateVars, render_template, validate_name};
use git::GitSource;
use manifest::{MANIFEST_FILE, Manifest};

/// Errors that can occur while resolving, fetching, or applying a starter.
#[derive(Debug, thiserror::Error)]
pub enum StarterError {
    /// The `--starter` value is not a valid source.
    #[error("{0}")]
    InvalidSource(String),

    /// Both an `@ref` suffix and `--starter-ref` were supplied.
    #[error("a starter ref was given twice (an '@ref' suffix and --starter-ref); supply only one")]
    AmbiguousRef,

    /// `--starter-ref` was given for a built-in or local-path starter.
    #[error("--starter-ref only applies to git starters")]
    RefNotApplicable,

    /// The project directory already exists.
    #[error("directory '{0}' already exists")]
    AlreadyExists(String),

    /// The project name is not a valid Rust package name.
    #[error("invalid project name '{0}': {1}")]
    InvalidName(String, String),

    /// The starter has no `autumn-starter.toml`, or it is malformed.
    #[error("invalid starter manifest: {0}")]
    Manifest(String),

    /// The system `git` binary was not found on `PATH`.
    #[error(
        "git is required to fetch a community starter but was not found on PATH; \
         install git or use a built-in starter (autumn new --list-starters)"
    )]
    GitNotInstalled,

    /// `git clone` failed.
    #[error("{0}")]
    CloneFailed(String),

    /// A non-built-in starter needs confirmation but stdin is not interactive.
    #[error(
        "fetching a community starter requires confirmation; re-run with --yes to \
         proceed non-interactively"
    )]
    ConfirmationRequired,

    /// The user declined the confirmation prompt.
    #[error("aborted: starter not applied")]
    Aborted,

    /// Filesystem error.
    #[error("{0}")]
    Io(#[from] io::Error),

    /// Error from the underlying project emission engine.
    #[error("{0}")]
    Emit(#[from] crate::generate::GenerateError),
}

/// Where a `--starter` value resolves to.
#[derive(Debug)]
pub enum Resolved {
    /// A curated, embedded starter.
    Builtin(&'static builtin::Builtin),
    /// A community starter on the local filesystem.
    LocalDir(PathBuf),
    /// A community starter in a git repository.
    Git(GitSource),
}

impl Resolved {
    /// Whether the starter must be confirmed before it is fetched/applied.
    /// Built-in starters apply without a network fetch or prompt.
    #[must_use]
    pub const fn requires_confirmation(&self) -> bool {
        !matches!(self, Self::Builtin(_))
    }
}

/// Resolve a `--starter` value (+ optional `--starter-ref`) into a source.
///
/// Resolution order: exact built-in name → existing local directory → git
/// (full URL or `owner/repo` shorthand).
///
/// # Errors
/// See [`StarterError`] variants for the failure modes (ambiguous/non-applicable
/// ref, invalid source).
pub fn resolve(value: &str, ref_override: Option<&str>) -> Result<Resolved, StarterError> {
    if let Some(b) = builtin::find(value) {
        if ref_override.is_some() {
            return Err(StarterError::RefNotApplicable);
        }
        return Ok(Resolved::Builtin(b));
    }
    if Path::new(value).is_dir() {
        if ref_override.is_some() {
            return Err(StarterError::RefNotApplicable);
        }
        return Ok(Resolved::LocalDir(PathBuf::from(value)));
    }
    let source = git::resolve(value, ref_override)?;
    Ok(Resolved::Git(source))
}

/// A starter loaded into memory: its manifest plus every template file.
#[derive(Debug)]
struct StarterContents {
    manifest: Manifest,
    files: Vec<StarterFile>,
}

/// One file from a starter tree (path relative to the starter root, raw bytes).
#[derive(Debug)]
struct StarterFile {
    rel_path: String,
    bytes: Vec<u8>,
}

/// Outcome of the provenance-confirmation decision, separated from IO so it can
/// be unit-tested without a real terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfirmMode {
    /// Proceed without prompting (`--yes`, or a built-in).
    Proceed,
    /// Prompt the user interactively.
    Prompt,
    /// Refuse: confirmation is required but stdin is not a TTY and `--yes` was
    /// not given.
    NeedsYesFlag,
}

/// Decide how to confirm a non-built-in starter, given the `--yes` flag and
/// whether stdin is interactive. Pure — the IO lives in [`confirm`].
#[must_use]
pub const fn confirm_mode(yes: bool, interactive: bool) -> ConfirmMode {
    if yes {
        ConfirmMode::Proceed
    } else if interactive {
        ConfirmMode::Prompt
    } else {
        ConfirmMode::NeedsYesFlag
    }
}

/// Human-readable provenance line for a non-built-in starter.
fn provenance(resolved: &Resolved) -> String {
    match resolved {
        Resolved::Builtin(b) => format!("Built-in starter '{}'", b.name),
        Resolved::LocalDir(path) => {
            let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
            format!("Community starter (local path): {}", abs.display())
        }
        Resolved::Git(source) => {
            let reference = source.reference.as_deref().unwrap_or("<default branch>");
            format!("Community starter (git): {} (ref: {reference})", source.url)
        }
    }
}

/// Print the resolved provenance and confirm before fetching/applying.
fn confirm(provenance: &str, yes: bool) -> Result<(), StarterError> {
    println!("{provenance}");
    match confirm_mode(yes, io::stdin().is_terminal()) {
        ConfirmMode::Proceed => Ok(()),
        ConfirmMode::NeedsYesFlag => Err(StarterError::ConfirmationRequired),
        ConfirmMode::Prompt => {
            print!("Proceed? [y/N] ");
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Ok(())
            } else {
                Err(StarterError::Aborted)
            }
        }
    }
}

/// Print the list of built-in starters (for `autumn new --list-starters`).
pub fn print_list() {
    println!("Available built-in starters:\n");
    let width = builtin::BUILTINS
        .iter()
        .map(|b| b.name.len())
        .max()
        .unwrap_or(0);
    for b in builtin::BUILTINS {
        println!("  {:<width$}  {}", b.name, b.description, width = width);
    }
    println!("\nScaffold one with:   autumn new <name> --starter <starter>");
    println!("Community starters:  autumn new <name> --starter <git-url|owner/repo>[@ref] [--yes]");
}

/// Entry point for `autumn new <name> --starter <value>`.
///
/// Exits the process non-zero with a diagnostic on any error, mirroring
/// [`crate::new::run`].
pub fn run(name: &str, starter: &str, starter_ref: Option<&str>, yes: bool, flags: Flags) {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("Error: cannot determine current directory: {e}");
        std::process::exit(1);
    });
    if let Err(e) = run_inner(name, starter, starter_ref, yes, flags, &cwd) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn run_inner(
    name: &str,
    starter: &str,
    starter_ref: Option<&str>,
    yes: bool,
    flags: Flags,
    parent_dir: &Path,
) -> Result<(), StarterError> {
    validate_name(name).map_err(|e| {
        StarterError::InvalidName(name.to_owned(), strip_name_error(&e.to_string()))
    })?;

    let project_dir = parent_dir.join(name);
    if project_dir.exists() {
        return Err(StarterError::AlreadyExists(name.to_owned()));
    }

    let resolved = resolve(starter, starter_ref)?;

    // Surface provenance and confirm before fetching/applying anything that did
    // not ship with the CLI (built-ins are vetted and apply silently).
    if resolved.requires_confirmation() {
        confirm(&provenance(&resolved), yes)?;
    }

    // Any git clone goes into a tempdir that must outlive scaffolding; hold it
    // here and drop it explicitly once the files have been emitted.
    let mut clone_guard: Option<tempfile::TempDir> = None;
    let contents = match &resolved {
        Resolved::Builtin(b) => {
            println!("Scaffolding built-in starter '{}'", b.name);
            load_from_embedded(b.dir)?
        }
        Resolved::LocalDir(path) => load_from_dir(path)?,
        Resolved::Git(source) => {
            let tmp = tempfile::TempDir::new()?;
            git::clone_into(source, tmp.path())?;
            let contents = load_from_dir(tmp.path())?;
            clone_guard = Some(tmp);
            contents
        }
    };

    let crate_name = name.replace('-', "_");
    let vars = TemplateVars {
        project_name: name,
        crate_name: &crate_name,
        autumn_version: env!("CARGO_PKG_VERSION"),
        rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
    };

    println!("\nCreating `{name}`:");
    scaffold(&contents, &vars, &project_dir, flags)?;
    drop(clone_guard);

    if !flags.dry_run {
        print_post_scaffold(&contents.manifest, &vars);
    }
    Ok(())
}

/// `NewError::InvalidName` renders as `invalid project name '<n>': <reason>`;
/// extract just the reason so we don't double up the prefix.
fn strip_name_error(msg: &str) -> String {
    msg.rsplit(": ").next().unwrap_or(msg).to_owned()
}

/// Recursively collect every embedded file, paths relative to the starter root.
fn collect_embedded(dir: &Dir<'_>, out: &mut Vec<StarterFile>) {
    for f in dir.files() {
        out.push(StarterFile {
            rel_path: f.path().to_string_lossy().replace('\\', "/"),
            bytes: f.contents().to_vec(),
        });
    }
    for d in dir.dirs() {
        collect_embedded(d, out);
    }
}

fn load_from_embedded(dir: &Dir<'_>) -> Result<StarterContents, StarterError> {
    let manifest_src = dir
        .get_file(MANIFEST_FILE)
        .ok_or_else(|| StarterError::Manifest(format!("missing {MANIFEST_FILE}")))?
        .contents_utf8()
        .ok_or_else(|| StarterError::Manifest(format!("{MANIFEST_FILE} is not valid UTF-8")))?;
    let manifest = Manifest::parse(manifest_src).map_err(StarterError::Manifest)?;

    let mut all = Vec::new();
    collect_embedded(dir, &mut all);
    Ok(finish_contents(manifest, all))
}

/// Directory names that are always skipped when collecting a community starter
/// from the local filesystem — they contain build artefacts, not source files.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "dist", "build"];

/// Recursively read a starter from the filesystem, skipping common artefact dirs.
fn collect_dir(root: &Path, cur: &Path, out: &mut Vec<StarterFile>) -> io::Result<()> {
    for entry in fs::read_dir(cur)? {
        let entry = entry?;
        if SKIP_DIRS.contains(&entry.file_name().to_str().unwrap_or("")) {
            continue;
        }
        // Skip symlinks entirely: following them risks infinite recursion (a link
        // to an ancestor) and reading/writing files outside the starter root.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_dir(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(StarterFile {
                rel_path: rel,
                bytes: fs::read(&path)?,
            });
        }
    }
    Ok(())
}

fn load_from_dir(root: &Path) -> Result<StarterContents, StarterError> {
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest_src = fs::read_to_string(&manifest_path).map_err(|_| {
        StarterError::Manifest(format!(
            "missing {MANIFEST_FILE} at the starter root ({})",
            root.display()
        ))
    })?;
    let manifest = Manifest::parse(&manifest_src).map_err(StarterError::Manifest)?;

    let mut all = Vec::new();
    collect_dir(root, root, &mut all)?;
    Ok(finish_contents(manifest, all))
}

/// Drop the manifest from the file list and sort for deterministic emission.
fn finish_contents(manifest: Manifest, mut all: Vec<StarterFile>) -> StarterContents {
    all.retain(|f| f.rel_path != MANIFEST_FILE);
    all.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    StarterContents {
        manifest,
        files: all,
    }
}

/// Whether `rel_path` should be emitted verbatim (no template substitution).
fn is_verbatim(manifest: &Manifest, rel_path: &str) -> bool {
    manifest.starter.verbatim.iter().any(|p| p == rel_path)
}

/// Render and emit a loaded starter into `project_dir`.
fn scaffold(
    contents: &StarterContents,
    vars: &TemplateVars<'_>,
    project_dir: &Path,
    flags: Flags,
) -> Result<(), StarterError> {
    let mut plan = Plan::new(project_dir);
    for file in &contents.files {
        // Defence-in-depth: reject absolute paths or `..` components so a
        // malicious starter cannot write outside the target project directory.
        let rel_path = Path::new(&file.rel_path);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(StarterError::InvalidSource(format!(
                "malicious path in starter: {}",
                file.rel_path
            )));
        }
        let target = project_dir.join(rel_path);
        // Verbatim or non-UTF-8 files are copied byte-for-byte; substituting
        // them would corrupt binary assets.
        if is_verbatim(&contents.manifest, &file.rel_path) {
            plan.create_bytes(target, file.bytes.clone());
        } else {
            match std::str::from_utf8(&file.bytes) {
                Ok(text) => plan.create(target, render_template(text, vars)),
                Err(_) => plan.create_bytes(target, file.bytes.clone()),
            }
        }
    }
    plan.execute(flags)?;
    Ok(())
}

/// Print the manifest's post-scaffold notes (with `{{…}}` substituted).
fn print_post_scaffold(manifest: &Manifest, vars: &TemplateVars<'_>) {
    if let Some(notes) = &manifest.starter.post_scaffold_notes {
        println!("\n{}", render_template(notes, vars));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_resolves_without_confirmation() {
        let r = resolve("saas", None).unwrap();
        assert!(matches!(r, Resolved::Builtin(_)));
        assert!(!r.requires_confirmation());
    }

    #[test]
    fn builtin_rejects_ref() {
        assert!(matches!(
            resolve("saas", Some("v1")).unwrap_err(),
            StarterError::RefNotApplicable
        ));
    }

    #[test]
    fn local_dir_resolves_as_community() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = resolve(tmp.path().to_str().unwrap(), None).unwrap();
        assert!(matches!(r, Resolved::LocalDir(_)));
        assert!(r.requires_confirmation());
    }

    #[test]
    fn unknown_value_resolves_as_git_shorthand() {
        let r = resolve("owner/repo", None).unwrap();
        match r {
            Resolved::Git(src) => {
                assert_eq!(src.url, "https://github.com/owner/repo.git");
                assert!(r_requires_confirmation_git());
            }
            _ => panic!("expected git source"),
        }
    }

    fn r_requires_confirmation_git() -> bool {
        Resolved::Git(GitSource {
            url: String::new(),
            reference: None,
        })
        .requires_confirmation()
    }

    #[test]
    fn confirm_mode_yes_proceeds() {
        assert_eq!(confirm_mode(true, false), ConfirmMode::Proceed);
        assert_eq!(confirm_mode(true, true), ConfirmMode::Proceed);
    }

    #[test]
    fn confirm_mode_non_tty_without_yes_needs_flag() {
        assert_eq!(confirm_mode(false, false), ConfirmMode::NeedsYesFlag);
    }

    #[test]
    fn confirm_mode_tty_without_yes_prompts() {
        assert_eq!(confirm_mode(false, true), ConfirmMode::Prompt);
    }

    #[test]
    fn builtin_list_includes_saas() {
        assert!(builtin::BUILTINS.iter().any(|b| b.name == "saas"));
    }

    fn fixture_minimal() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/starters/minimal")
    }

    #[test]
    fn scaffold_local_fixture_substitutes_and_copies_verbatim() {
        let contents = load_from_dir(&fixture_minimal()).unwrap();
        assert_eq!(contents.manifest.starter.name, "minimal");

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("acme-app");
        let crate_name = "acme_app".to_owned();
        let vars = TemplateVars {
            project_name: "acme-app",
            crate_name: &crate_name,
            autumn_version: env!("CARGO_PKG_VERSION"),
            rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
        };
        scaffold(&contents, &vars, &dest, Flags::default()).unwrap();

        // Template tokens substituted in text files.
        let cargo = fs::read_to_string(dest.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"acme_app\""), "got: {cargo}");
        let main_rs = fs::read_to_string(dest.join("src/main.rs")).unwrap();
        assert!(main_rs.contains("acme-app"));
        assert!(
            !main_rs.contains("{{"),
            "no tokens should remain: {main_rs}"
        );

        // The manifest itself is never emitted into the project.
        assert!(!dest.join(MANIFEST_FILE).exists());

        // The verbatim binary asset is copied byte-for-byte — not substituted
        // (it still contains the literal `{{project_name}}` and its non-UTF-8
        // bytes are intact).
        let original = fs::read(fixture_minimal().join("static/logo.bin")).unwrap();
        let copied = fs::read(dest.join("static/logo.bin")).unwrap();
        assert_eq!(
            copied, original,
            "verbatim binary asset must not be altered"
        );
        assert!(
            std::str::from_utf8(&copied).is_err(),
            "fixture asset is binary"
        );
    }

    #[test]
    fn missing_manifest_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "no manifest here").unwrap();
        assert!(matches!(
            load_from_dir(tmp.path()).unwrap_err(),
            StarterError::Manifest(_)
        ));
    }

    /// The embedded `saas` starter, rendered with project name `saas`, must
    /// reproduce the committed `examples/saas/` tree exactly — so the flagship
    /// starter and the drift-gated example can never diverge silently.
    ///
    /// `Cargo.toml` is excluded: the committed example uses an in-workspace path
    /// dependency (so it compiles in-repo) while the shipped starter uses a
    /// versioned dependency. Everything else must match byte-for-byte.
    #[test]
    fn embedded_saas_matches_example_saas() {
        let contents = load_from_embedded(&builtin::SAAS).unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("saas");
        let crate_name = "saas".to_owned();
        let vars = TemplateVars {
            project_name: "saas",
            crate_name: &crate_name,
            autumn_version: env!("CARGO_PKG_VERSION"),
            rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
        };
        scaffold(&contents, &vars, &dest, Flags::default()).unwrap();

        let example_root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/saas");

        // Every rendered file matches the committed example.
        for file in &contents.files {
            if file.rel_path == "Cargo.toml" {
                continue;
            }
            let example_path = example_root.join(&file.rel_path);
            let rendered = fs::read(dest.join(&file.rel_path)).unwrap();
            let committed = fs::read(&example_path).unwrap_or_else(|_| {
                panic!(
                    "examples/saas is missing {} — regenerate it from the starter",
                    file.rel_path
                )
            });
            assert_eq!(
                rendered, committed,
                "drift between embedded saas starter and examples/saas at {}",
                file.rel_path
            );
        }

        // …and the example has no stray files the starter does not produce
        // (ignoring build artefacts and the generated CSS).
        let mut starter_paths: std::collections::BTreeSet<String> =
            contents.files.iter().map(|f| f.rel_path.clone()).collect();
        starter_paths.insert("Cargo.toml".to_owned());
        let mut stack = vec![example_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let name = entry.file_name();
                if name == "target" || name == ".git" {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(&example_root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                // app.css is a build artefact (Tailwind output), not a source file.
                if rel == "static/css/app.css" {
                    continue;
                }
                // tests/system/smoke.rs is workspace-internal e2e tooling (issue
                // #1192) that depends on the path-only `example-e2e` crate — it
                // has no meaning outside the autumn monorepo, same category of
                // divergence as Cargo.toml's path-vs-versioned dependency above.
                if rel == "tests/system/smoke.rs" {
                    continue;
                }
                assert!(
                    starter_paths.contains(&rel),
                    "examples/saas has {rel} which the embedded starter does not produce"
                );
            }
        }
    }
}
