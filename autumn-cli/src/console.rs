//! `autumn console` -- scaffold and run a pre-wired data playground binary.
//!
//! Autumn's answer to `rails console` / `manage.py shell` / `iex -S mix`
//! (issue #1039). Rust has no stable `eval`, so rather than building an
//! interpreter this follows loco.rs's edit-and-run model: the first invocation
//! scaffolds `src/bin/playground.rs` already wired with the config and
//! database-URL resolution `autumn seed`/`autumn dev` use, a constructed async
//! pool, and a checked-out connection; every invocation compiles and runs it.
//!
//! Two properties matter and are enforced by tests:
//!
//! 1. **Never clobber user work.** An existing playground is left alone unless
//!    `--force` is passed.
//! 2. **Never silently succeed.** A config or connection failure exits
//!    non-zero and prints the underlying error, from the scaffolded binary all
//!    the way out through this command's exit status.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Errors surfaced by the console runner.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConsoleError {
    #[error(
        "not an Autumn project: no `Cargo.toml` found in {0}\n\
         Run `autumn console` from a project root, or pass --package <name>.\n\
         See: docs/guide/console.md"
    )]
    NotAProject(String),

    #[error("could not parse Cargo.toml: {0}")]
    ManifestParse(String),

    #[error(
        "Cargo.toml has no `autumn-web` dependency, so this is not an Autumn \
         project\n\
         See: docs/guide/console.md"
    )]
    MissingAutumnWebDependency,

    #[error(
        "Cargo.toml's `[features]` is not a table, so the `playground` feature \
         cannot be defined; fix it by hand and re-run\n\
         See: docs/guide/console.md"
    )]
    UnusableFeaturesTable,

    #[error(
        "no package named `{0}` in this workspace; run `cargo metadata --no-deps` \
         to see the available members"
    )]
    UnknownPackage(String),
}

/// What the scaffolder did (or would do) with the playground source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaffoldOutcome {
    Created,
    Kept,
    Regenerated,
}

impl ScaffoldOutcome {
    /// The past-tense verb used when reporting the outcome to the user.
    const fn verb(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Kept => "Kept",
            Self::Regenerated => "Regenerated",
        }
    }

    /// Whether this outcome writes the template to disk.
    const fn writes_file(self) -> bool {
        matches!(self, Self::Created | Self::Regenerated)
    }
}

/// Manifest edits performed by [`ensure_manifest_wiring`].
///
/// The playground is registered as a `[[bin]]` gated behind
/// `required-features = ["playground"]`. That gate is the load-bearing part of
/// this design, and it is worth being explicit about why.
///
/// Cargo would auto-discover `src/bin/playground.rs` on its own — which puts
/// it in the **default build set**. That is unacceptable here, because the
/// playground compiles the app's own `models`/`repositories`/`policies`
/// modules into a separate crate via `#[path]`, and generated code in those
/// modules is not always self-contained: an `autumn generate scaffold --live`
/// repository renders `crate::routes::posts::paths::show(...)`, and `routes`
/// (which itself reaches into `src/main.rs`) can never be reachable from a bin
/// target. Auto-discovered, that broken target would be compiled by a bare
/// `cargo build` — the exact command `autumn dev` and `autumn build` run — so
/// scaffolding a playground would break the user's dev loop, with no
/// uninstall path.
///
/// A `required-features` gate makes that impossible: `cargo build`,
/// `cargo test`, `autumn dev`, and `autumn build` all skip the target
/// entirely, and only `autumn console` (which passes `--features playground`)
/// ever compiles it. A playground that does not compile is then a console
/// problem the user sees immediately, not a project-wide outage.
///
/// The gate also removes three problems outright: `cargo run` stays
/// unambiguous without touching `default-run` (the second binary is filtered
/// out unless the feature is on), a half-applied run cannot leave an
/// uncompilable default build, and `seed`'s implied `db` feature never reaches
/// a deliberately DB-free project's normal builds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ManifestChanges {
    pub added_bin: bool,
    pub added_feature: bool,
}

impl ManifestChanges {
    const fn any(self) -> bool {
        self.added_bin || self.added_feature
    }
}

pub const PLAYGROUND_REL_PATH: &str = "src/bin/playground.rs";
pub const PLAYGROUND_BIN_NAME: &str = "playground";

/// The cargo feature that gates the playground bin target. Enabling it turns
/// on `autumn-web/seed`, which is what the template's `SeedContext` bootstrap
/// needs.
pub const PLAYGROUND_FEATURE: &str = "playground";

/// The `autumn-web` feature the scaffolded playground needs: it bootstraps
/// through `autumn_web::seed::SeedContext`, which is gated on `seed`.
const REQUIRED_FEATURE: &str = "seed";

/// The app modules a data playground plausibly needs in scope, in declaration
/// order. Only the ones that actually exist in the project are emitted.
const APP_MODULES: &[&str] = &["schema", "models", "repositories", "policies"];

const PLAYGROUND_TEMPLATE: &str = include_str!("templates/playground.rs.tmpl");

/// Decide what to do with the playground source file.
///
/// AC5: an existing, possibly user-edited playground is never overwritten;
/// `--force` is the only path that regenerates it from the template.
pub const fn scaffold_outcome(exists: bool, force: bool) -> ScaffoldOutcome {
    match (exists, force) {
        (false, _) => ScaffoldOutcome::Created,
        (true, false) => ScaffoldOutcome::Kept,
        (true, true) => ScaffoldOutcome::Regenerated,
    }
}

/// Build the `#[path]` module declarations that make the app's data layer
/// visible from `src/bin/playground.rs`.
///
/// A Cargo binary target is a separate crate, so a freshly generated project
/// (which has no `src/lib.rs`) cannot otherwise reach `src/models/`. Declaring
/// the modules with an explicit `#[path]` compiles them into the playground
/// crate, which is what makes a model/repository round-trip possible with no
/// further wiring (AC3). Both the `src/<name>.rs` and `src/<name>/mod.rs`
/// layouts are supported; the directory layout wins when both exist.
pub fn app_module_decls(project_root: &Path) -> String {
    let src = project_root.join("src");
    let mut out = String::new();
    for name in APP_MODULES {
        let rel = if src.join(name).join("mod.rs").is_file() {
            format!("../{name}/mod.rs")
        } else if src.join(format!("{name}.rs")).is_file() {
            format!("../{name}.rs")
        } else {
            continue;
        };
        let _ = writeln!(out, "#[path = \"{rel}\"]\nmod {name};");
    }
    out
}

/// Render the playground template for `project_name`, splicing in the app
/// module declarations produced by [`app_module_decls`].
pub fn render_playground(project_name: &str, app_modules: &str) -> String {
    let rendered = PLAYGROUND_TEMPLATE
        .replace("\r\n", "\n")
        .replace("{{project_name}}", project_name)
        .replace("{{app_modules}}", app_modules.trim_end());
    // An empty `{{app_modules}}` leaves a double blank line behind; collapse
    // any run of blank lines so both shapes render identically.
    collapse_blank_lines(&rendered)
}

fn collapse_blank_lines(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut blank_run = 0usize;
    for line in src.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Read `package.name` out of a manifest, if present.
fn package_name(manifest: &str) -> Option<String> {
    manifest
        .parse::<toml_edit::DocumentMut>()
        .ok()?
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

/// Wire `Cargo.toml` so `autumn console` can build the playground — and only
/// `autumn console`.
///
/// Two edits, each independently idempotent:
///
/// 1. define the `playground` cargo feature as `["autumn-web/seed"]`, which is
///    what the template's `SeedContext` bootstrap needs;
/// 2. register the `playground` bin target behind
///    `required-features = ["playground"]`.
///
/// See [`ManifestChanges`] for why the gate matters. Note what is *not* edited:
/// the `autumn-web` dependency line is left completely alone. Routing the
/// feature through `autumn-web/seed` means we never have to rewrite a
/// dependency whose shape we do not control, and never risk moving a trailing
/// `# comment` inside a rewritten inline table.
///
/// Edits go through `toml_edit`, so comments, key order, and hand-formatted
/// arrays survive. A manifest that cannot be parsed is an error, never a
/// best-effort rewrite.
pub fn ensure_manifest_wiring(manifest: &str) -> Result<(String, ManifestChanges), ConsoleError> {
    let mut doc = manifest
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConsoleError::ManifestParse(e.to_string()))?;

    // Fail early and clearly on a project that is not an Autumn app, before
    // any edit is staged.
    if doc
        .get("dependencies")
        .and_then(|deps| deps.get("autumn-web"))
        .is_none()
    {
        return Err(ConsoleError::MissingAutumnWebDependency);
    }

    let changes = ManifestChanges {
        added_feature: ensure_playground_feature(&mut doc)?,
        added_bin: !adding_bin_would_break_autodiscovery(&doc) && ensure_playground_bin(&mut doc),
    };

    Ok((doc.to_string(), changes))
}

/// Define `[features] playground = ["autumn-web/seed"]` unless the key is
/// already present (in which case it is the user's, and we leave it).
fn ensure_playground_feature(doc: &mut toml_edit::DocumentMut) -> Result<bool, ConsoleError> {
    use toml_edit::{Array, Item, Table, Value};

    let features = doc
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(features) = features.as_table_mut() else {
        return Err(ConsoleError::UnusableFeaturesTable);
    };
    if features.contains_key(PLAYGROUND_FEATURE) {
        return Ok(false);
    }

    let mut enables = Array::new();
    enables.push(format!("autumn-web/{REQUIRED_FEATURE}"));
    features.insert(PLAYGROUND_FEATURE, Item::Value(Value::Array(enables)));
    Ok(true)
}

/// Register the playground bin target behind its feature gate, unless a target
/// with that name is already declared — which may be a user-owned entry
/// pointing somewhere else, and is never rewritten.
fn ensure_playground_bin(doc: &mut toml_edit::DocumentMut) -> bool {
    use toml_edit::{Array, ArrayOfTables, Item, Table, Value, value};

    if declared_playground_path(doc).is_some() {
        return false;
    }

    let bins = doc
        .entry("bin")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let Some(bins) = bins.as_array_of_tables_mut() else {
        return false;
    };

    let mut required = Array::new();
    required.push(PLAYGROUND_FEATURE);

    let mut entry = Table::new();
    entry.insert("name", value(PLAYGROUND_BIN_NAME));
    entry.insert("path", value(PLAYGROUND_REL_PATH));
    entry.insert("required-features", Item::Value(Value::Array(required)));
    bins.push(entry);
    true
}

/// The `path` of an already-declared `[[bin]] name = "playground"`, if any.
///
/// A user who moved the playground keeps it there: scaffolding to the default
/// location while their manifest points elsewhere would write a file Cargo
/// de-duplicates away, so `autumn console` would report creating one program
/// and then run a different one.
pub fn declared_playground_path(doc: &toml_edit::DocumentMut) -> Option<String> {
    doc.get("bin")
        .and_then(toml_edit::Item::as_array_of_tables)?
        .iter()
        .find(|t| t.get("name").and_then(toml_edit::Item::as_str) == Some(PLAYGROUND_BIN_NAME))?
        .get("path")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned)
}

/// Whether adding the *first* manual `[[bin]]` to this manifest would turn off
/// Cargo's target auto-discovery and silently drop the package's existing
/// binaries from the build.
///
/// This is an edition-2015 rule: there, `autobins` defaults to `true` only
/// while no target is declared by hand, and flips to `false` the moment one
/// is. A 2015 package with `src/main.rs` and two `src/bin/*.rs` workers and no
/// manual targets would therefore lose all three the instant we appended an
/// entry. Edition 2018 and later keep auto-discovery on unconditionally, and a
/// manifest that already declares targets (or sets `autobins = false`) has
/// nothing left to lose — both are safe.
pub fn adding_bin_would_break_autodiscovery(doc: &toml_edit::DocumentMut) -> bool {
    let Some(package) = doc.get("package").and_then(toml_edit::Item::as_table) else {
        return false;
    };
    let legacy_edition = matches!(
        package.get("edition").and_then(toml_edit::Item::as_str),
        None | Some("2015")
    );
    if !legacy_edition {
        return false;
    }
    // Already in manual-target mode? Then auto-discovery is off already and
    // there is nothing our entry could remove.
    let declares_targets = ["bin", "lib", "test", "bench", "example"]
        .iter()
        .any(|k| doc.get(k).is_some())
        || package.get("autobins").is_some();
    !declares_targets
}

/// Print an error and exit non-zero. `autumn console` never fails silently.
fn fail(err: &ConsoleError) -> ! {
    eprintln!("\u{2717} {err}");
    std::process::exit(1);
}

/// Print an I/O failure against `path` and exit non-zero.
fn fail_io(path: &Path, err: &std::io::Error) -> ! {
    eprintln!("\u{2717} could not write {}: {err}", path.display());
    std::process::exit(1);
}

/// Write `contents` to `path` via a temporary file plus a rename, so an
/// interruption (Ctrl-C, ENOSPC, a crash) can never leave a truncated file
/// behind. `Cargo.toml` in particular is unrecoverable if half-written.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.autumn-console-tmp",
        path.extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("")
    ));
    std::fs::write(&tmp, contents)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Resolve the package directory to operate on.
///
/// A `--package` we cannot resolve is a hard error. Falling back to the current
/// directory would silently scaffold into — and mutate the manifest of — a
/// *different* package than the one the user named. (`autumn seed` can afford
/// that fallback because it only reads afterwards; this command writes.)
fn resolve_project_dir(package: Option<&str>) -> PathBuf {
    let Some(pkg) = package else {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    };
    crate::seed::find_package_dir(pkg)
        .unwrap_or_else(|| fail(&ConsoleError::UnknownPackage(pkg.to_owned())))
}

/// Read and parse the project manifest, failing with an actionable error when
/// this is not an Autumn project. Runs before any write, so a rejected project
/// is left byte-identical.
fn load_manifest(manifest_path: &Path, project_dir: &Path) -> (String, toml_edit::DocumentMut) {
    if !manifest_path.is_file() {
        fail(&ConsoleError::NotAProject(
            project_dir.display().to_string(),
        ));
    }
    let manifest = std::fs::read_to_string(manifest_path).unwrap_or_else(|e| {
        eprintln!("\u{2717} could not read {}: {e}", manifest_path.display());
        std::process::exit(1);
    });
    let doc = manifest
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|e| fail(&ConsoleError::ManifestParse(e.to_string())));
    (manifest, doc)
}

/// Write the playground template to `path`, creating its parent directory.
fn write_playground(path: &Path, project_dir: &Path, manifest: &str) {
    let project_name = package_name(manifest).unwrap_or_else(|| project_dir.display().to_string());
    let source = render_playground(&project_name, &app_module_decls(project_dir));
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("\u{2717} could not create {}: {e}", parent.display());
        std::process::exit(1);
    }
    if let Err(e) = write_atomic(path, &source) {
        fail_io(path, &e);
    }
}

/// Report the manifest edits that were applied.
fn report_manifest_edits(changes: ManifestChanges) {
    let mut edits: Vec<String> = Vec::new();
    if changes.added_feature {
        edits.push(format!(
            "[features] {PLAYGROUND_FEATURE} = [\"autumn-web/{REQUIRED_FEATURE}\"]"
        ));
    }
    if changes.added_bin {
        edits.push(format!(
            "[[bin]] {PLAYGROUND_BIN_NAME} (required-features = [\"{PLAYGROUND_FEATURE}\"])"
        ));
    }
    eprintln!("  Updated Cargo.toml ({})", edits.join(", "));
}

/// Tell the user what to paste when we deliberately declined to add the bin
/// entry, rather than letting cargo's "no bin target named playground" be
/// their first clue.
fn warn_manual_bin_entry_required() {
    eprintln!(
        "  \u{26a0} This package uses Rust edition 2015, where declaring a \
         target turns off Cargo's auto-discovery of the others.\n    \
         Adding the playground entry automatically would drop your existing \
         binaries from the build, so add it yourself:\n\n      \
         [[bin]]\n      name = \"{PLAYGROUND_BIN_NAME}\"\n      path = \
         \"{PLAYGROUND_REL_PATH}\"\n      required-features = \
         [\"{PLAYGROUND_FEATURE}\"]\n"
    );
}

/// Entry point for `autumn console`.
pub fn run(profile: &str, package: Option<&str>, force: bool, scaffold_only: bool) {
    eprintln!("\u{1F342} autumn console\n");
    eprintln!("  Profile: {profile}");

    let project_dir = resolve_project_dir(package);
    let manifest_path = project_dir.join("Cargo.toml");
    let (manifest, doc) = load_manifest(&manifest_path, &project_dir);
    let (updated_manifest, changes) =
        ensure_manifest_wiring(&manifest).unwrap_or_else(|e| fail(&e));

    // Write the manifest FIRST. The playground is only ever compiled with
    // `--features playground`, so a manifest that never landed simply means
    // `autumn console` cannot build it yet — whereas a playground file written
    // against a manifest that failed to save is at worst an inert source file
    // with no target. Manifest-first degrades in the gentler direction.
    if changes.any() {
        if let Err(e) = write_atomic(&manifest_path, &updated_manifest) {
            fail_io(&manifest_path, &e);
        }
        report_manifest_edits(changes);
    }

    // Honour a user-relocated playground: if their manifest already declares
    // the bin somewhere else, scaffold there rather than writing a file Cargo
    // would de-duplicate away and never run.
    let declared = declared_playground_path(&doc);
    let playground_rel = declared
        .clone()
        .unwrap_or_else(|| PLAYGROUND_REL_PATH.to_owned());
    let playground_path = project_dir.join(&playground_rel);
    let exists = playground_path.is_file();
    // `--force` overwrites; refuse when the path is a symlink, so the write
    // cannot land on a link target elsewhere in the tree while we report
    // regenerating the file the user named.
    if exists
        && force
        && std::fs::symlink_metadata(&playground_path).is_ok_and(|m| m.file_type().is_symlink())
    {
        eprintln!(
            "\u{2717} {playground_rel} is a symlink; refusing to --force through \
             it. Remove the link first if you want a fresh playground."
        );
        std::process::exit(1);
    }

    let outcome = scaffold_outcome(exists, force);
    if outcome.writes_file() {
        write_playground(&playground_path, &project_dir, &manifest);
        eprintln!("  {} {playground_rel}", outcome.verb());
    } else {
        eprintln!(
            "  {} {playground_rel} (pass --force to regenerate it from the template)",
            outcome.verb()
        );
    }

    if !changes.added_bin && declared.is_none() && adding_bin_would_break_autodiscovery(&doc) {
        warn_manual_bin_entry_required();
    }

    if scaffold_only {
        eprintln!("\n\u{2713} Playground ready. Edit {playground_rel}, then run `autumn console`.");
        return;
    }

    eprintln!("\n  Building and running the playground...\n");

    let mut cmd = Command::new("cargo");
    // `--features playground` is what lifts the target's `required-features`
    // gate; without it Cargo skips the bin — which is exactly the property that
    // keeps a broken playground out of `autumn dev`'s builds.
    cmd.args([
        "run",
        "--bin",
        PLAYGROUND_BIN_NAME,
        "--features",
        PLAYGROUND_FEATURE,
    ]);
    if let Some(pkg) = package {
        cmd.args(["--package", pkg]);
    }
    cmd.env("AUTUMN_ENV", profile);
    cmd.env("AUTUMN_PROFILE", profile);
    // Run from the project directory so the playground's `SeedContext` reads
    // `autumn.toml` from the package root, not the workspace root.
    cmd.current_dir(&project_dir);

    match cmd.status() {
        Ok(status) if status.success() => {
            eprintln!("\n\u{2713} Playground finished.");
        }
        Ok(status) => {
            eprintln!(
                "\n\u{2717} Playground exited with a non-zero status ({}).",
                status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string())
            );
            std::process::exit(status.code().unwrap_or(1));
        }
        Err(e) => {
            eprintln!("\u{2717} Failed to run cargo: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── scaffold_outcome (AC5: idempotent, --force regenerates) ────────────

    #[test]
    fn scaffold_outcome_created_when_absent() {
        assert_eq!(scaffold_outcome(false, false), ScaffoldOutcome::Created);
    }

    #[test]
    fn scaffold_outcome_created_when_absent_even_with_force() {
        assert_eq!(scaffold_outcome(false, true), ScaffoldOutcome::Created);
    }

    #[test]
    fn scaffold_outcome_kept_when_present_without_force() {
        assert_eq!(scaffold_outcome(true, false), ScaffoldOutcome::Kept);
    }

    #[test]
    fn scaffold_outcome_regenerated_when_present_with_force() {
        assert_eq!(scaffold_outcome(true, true), ScaffoldOutcome::Regenerated);
    }

    // ── template contents (AC2, AC4) ───────────────────────────────────────

    #[test]
    fn playground_template_marks_the_your_code_here_region() {
        let src = render_playground("my-app", "");
        assert!(
            src.contains("your code here"),
            "template must carry a clearly-marked `your code here` region:\n{src}"
        );
    }

    #[test]
    fn playground_template_wires_the_shared_config_and_pool_bootstrap() {
        let src = render_playground("my-app", "");
        assert!(
            src.contains("autumn_web::seed::SeedContext"),
            "template must reuse the shared SeedContext bootstrap:\n{src}"
        );
        assert!(
            src.contains("SeedContext::build()"),
            "template must build the context (config + DB URL resolution):\n{src}"
        );
        assert!(
            src.contains("ctx.conn()"),
            "template must check out a pooled connection so a dead DB fails loudly:\n{src}"
        );
    }

    #[test]
    fn playground_template_exits_non_zero_on_config_or_connection_failure() {
        let src = render_playground("my-app", "");
        assert_eq!(
            src.matches("std::process::exit(1)").count(),
            2,
            "both the config/pool build and the connection checkout must exit \
             non-zero rather than succeed silently:\n{src}"
        );
        assert_eq!(
            src.matches("{err}").count(),
            2,
            "both failure branches must print the underlying error rather than \
             a generic panic message:\n{src}"
        );
    }

    #[test]
    fn playground_template_substitutes_the_project_name() {
        let src = render_playground("my-app", "");
        assert!(
            src.contains("my-app"),
            "template must mention the project it belongs to:\n{src}"
        );
    }

    #[test]
    fn playground_template_leaves_no_unsubstituted_placeholders() {
        let src = render_playground("my-app", "mod models;");
        assert!(
            !src.contains("{{"),
            "template must have no unsubstituted `{{{{…}}}}` tokens:\n{src}"
        );
    }

    #[test]
    fn playground_template_embeds_app_module_declarations() {
        let src = render_playground("my-app", "#[path = \"../models/mod.rs\"]\nmod models;");
        assert!(
            src.contains("#[path = \"../models/mod.rs\"]") && src.contains("mod models;"),
            "template must embed the detected app-module declarations:\n{src}"
        );
    }

    #[test]
    fn playground_template_mentions_autumn_console() {
        let src = render_playground("my-app", "");
        assert!(
            src.contains("autumn console"),
            "template header must tell the reader how it is run:\n{src}"
        );
    }

    // ── app_module_decls (AC3: model/repository APIs reachable) ────────────

    fn project_with(files: &[&str]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        for rel in files {
            let path = tmp.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "// test fixture\n").unwrap();
        }
        tmp
    }

    #[test]
    fn app_module_decls_is_empty_for_a_bare_project() {
        let tmp = project_with(&["src/main.rs"]);
        assert_eq!(app_module_decls(tmp.path()).trim(), "");
    }

    #[test]
    fn app_module_decls_detects_schema_and_models_directory() {
        let tmp = project_with(&["src/main.rs", "src/schema.rs", "src/models/mod.rs"]);
        let decls = app_module_decls(tmp.path());
        assert!(
            decls.contains("#[path = \"../schema.rs\"]") && decls.contains("mod schema;"),
            "must wire src/schema.rs into the playground crate:\n{decls}"
        );
        assert!(
            decls.contains("#[path = \"../models/mod.rs\"]") && decls.contains("mod models;"),
            "must wire src/models/mod.rs into the playground crate:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_detects_single_file_models() {
        let tmp = project_with(&["src/main.rs", "src/models.rs"]);
        let decls = app_module_decls(tmp.path());
        assert!(
            decls.contains("#[path = \"../models.rs\"]"),
            "must support the single-file src/models.rs layout:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_detects_repositories() {
        let tmp = project_with(&["src/main.rs", "src/repositories/mod.rs"]);
        let decls = app_module_decls(tmp.path());
        assert!(
            decls.contains("mod repositories;"),
            "must wire src/repositories into the playground crate:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_prefers_mod_rs_over_single_file() {
        let tmp = project_with(&["src/models.rs", "src/models/mod.rs"]);
        let decls = app_module_decls(tmp.path());
        assert_eq!(
            decls.matches("mod models;").count(),
            1,
            "must declare `models` exactly once:\n{decls}"
        );
        assert!(
            decls.contains("../models/mod.rs"),
            "the directory layout must win over the stale single file:\n{decls}"
        );
    }

    // ── ensure_manifest_wiring (AC3/AC4: the target actually builds) ───────

    const PLAIN_MANIFEST: &str = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
"#;

    #[test]
    fn ensure_manifest_wiring_gates_the_bin_behind_required_features() {
        // The load-bearing property: the playground must NOT join the default
        // build set. Auto-discovery would put it there, and a playground that
        // fails to compile (its `#[path]`-included repository referencing
        // `crate::routes`, say) would then break the bare `cargo build` that
        // `autumn dev` and `autumn build` run.
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST).unwrap();
        assert!(changes.added_bin && changes.added_feature);
        assert!(
            out.contains("[[bin]]") && out.contains("name = \"playground\""),
            "must register the playground bin target:\n{out}"
        );
        assert!(
            out.contains("path = \"src/bin/playground.rs\""),
            "bin entry must point at the scaffolded file:\n{out}"
        );
        assert!(
            out.contains("required-features = [\"playground\"]"),
            "the bin MUST be feature-gated so a bare `cargo build` skips it:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_defines_the_playground_feature() {
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST).unwrap();
        assert!(changes.added_feature);
        assert!(
            out.contains("playground = [\"autumn-web/seed\"]"),
            "the gate feature must turn on the autumn-web feature the \
             playground bootstraps through:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_never_touches_the_autumn_web_dependency() {
        // Routing through `autumn-web/seed` means the dependency line — whose
        // shape and decoration we do not control — is never rewritten. That
        // removes a whole class of corruption (e.g. a trailing `# comment`
        // being relocated inside a newly-created inline table, commenting out
        // the closing brace).
        let manifest = "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                        [dependencies]\nautumn-web = \"0.6.0\" # pinned for the demo\n";
        let (out, _) = ensure_manifest_wiring(manifest).unwrap();
        assert!(
            out.contains("autumn-web = \"0.6.0\" # pinned for the demo"),
            "the dependency line must be byte-identical:\n{out}"
        );
        assert!(
            out.parse::<toml_edit::DocumentMut>().is_ok(),
            "the rewritten manifest must still be valid TOML:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_preserves_existing_features() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
default = ["flash"]
flash = ["autumn-web/flash"]

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(changes.added_feature);
        assert!(
            out.contains("default = [\"flash\"]")
                && out.contains("flash = [\"autumn-web/flash\"]")
                && out.contains("playground = "),
            "must add `playground` alongside the project's own features:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_keeps_a_user_defined_playground_feature() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
playground = ["autumn-web/seed", "autumn-web/redis"]

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(!changes.added_feature);
        assert!(
            out.contains("\"autumn-web/redis\""),
            "a user-tuned playground feature must not be overwritten:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_is_idempotent() {
        let (once, _) = ensure_manifest_wiring(PLAIN_MANIFEST).unwrap();
        let (twice, changes) = ensure_manifest_wiring(&once).unwrap();
        assert_eq!(
            changes,
            ManifestChanges::default(),
            "a second run must report no changes"
        );
        assert_eq!(once, twice, "a second run must not alter the manifest");
    }

    #[test]
    fn ensure_manifest_wiring_leaves_other_bin_entries_untouched() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "worker"
path = "src/bin/worker.rs"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, _) = ensure_manifest_wiring(manifest).unwrap();
        assert!(
            out.contains("name = \"worker\"") && out.contains("name = \"playground\""),
            "a user's own bin targets must survive alongside ours:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_keeps_an_existing_playground_bin_entry() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "playground"
path = "custom/playground.rs"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(!changes.added_bin);
        assert!(
            out.contains("path = \"custom/playground.rs\"")
                && !out.contains("path = \"src/bin/playground.rs\""),
            "must not rewrite — or shadow — a user-owned bin entry:\n{out}"
        );
    }

    #[test]
    fn declared_playground_path_finds_a_relocated_playground() {
        // Cargo de-duplicates bin targets by name, so scaffolding to the
        // default location while the manifest points elsewhere would create a
        // file that is never compiled — `autumn console` would report creating
        // one program and then run a different one.
        let manifest = r#"[package]
name = "my-app"

[[bin]]
name = "playground"
path = "custom/playground.rs"
"#;
        let doc = manifest.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            declared_playground_path(&doc).as_deref(),
            Some("custom/playground.rs")
        );
    }

    #[test]
    fn declared_playground_path_is_none_without_an_entry() {
        let doc = PLAIN_MANIFEST.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(declared_playground_path(&doc), None);
    }

    // ── edition-2015 auto-discovery guard ──────────────────────────────────

    #[test]
    fn edition_2015_without_manual_targets_refuses_the_bin_entry() {
        // On edition 2015, `autobins` flips to false the moment any target is
        // declared by hand — so appending ours would silently drop the
        // package's `src/main.rs` and every `src/bin/*.rs` from the build.
        let manifest = r#"[package]
name = "legacy-app"
version = "0.1.0"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(
            !changes.added_bin,
            "must not append the first manual target to a 2015-edition manifest"
        );
        assert!(
            !out.contains("[[bin]]"),
            "must leave auto-discovery intact:\n{out}"
        );
        assert!(
            changes.added_feature,
            "the feature definition is still safe to add"
        );
    }

    #[test]
    fn edition_2015_with_manual_targets_accepts_the_bin_entry() {
        // Auto-discovery is already off here, so there is nothing to lose.
        let manifest = r#"[package]
name = "legacy-app"
version = "0.1.0"

[[bin]]
name = "legacy-app"
path = "src/main.rs"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (_, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(changes.added_bin);
    }

    #[test]
    fn modern_editions_always_accept_the_bin_entry() {
        for edition in ["2018", "2021", "2024"] {
            let manifest = format!(
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n\
                 [dependencies]\nautumn-web = \"0.6.0\"\n"
            );
            let (_, changes) = ensure_manifest_wiring(&manifest).unwrap();
            assert!(changes.added_bin, "edition {edition} keeps autobins on");
        }
    }

    // ── failure modes ──────────────────────────────────────────────────────

    #[test]
    fn ensure_manifest_wiring_errors_without_an_autumn_web_dependency() {
        let manifest = "[package]\nname = \"my-app\"\n\n[dependencies]\nserde = \"1\"\n";
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::MissingAutumnWebDependency)
        );
    }

    #[test]
    fn ensure_manifest_wiring_errors_on_an_unparsable_manifest() {
        let err = ensure_manifest_wiring("this is not = = toml").unwrap_err();
        assert!(matches!(err, ConsoleError::ManifestParse(_)));
    }

    #[test]
    fn ensure_manifest_wiring_errors_on_an_unusable_features_key() {
        // A `features` key we cannot extend must be an error, never a silent
        // no-op that leaves the user with a playground that cannot build and
        // nothing said about why.
        // Root-level keys must precede the first table header, so `features`
        // sits at the top — this is a top-level `features` key that is a
        // string rather than the expected `[features]` table.
        let manifest = "features = \"nope\"\n\n[package]\nname = \"my-app\"\n\
                        edition = \"2024\"\n\n[dependencies]\nautumn-web = \"0.6.0\"\n";
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::UnusableFeaturesTable)
        );
    }

    // ── error messages ─────────────────────────────────────────────────────

    #[test]
    fn not_a_project_error_names_cargo_toml_and_the_directory() {
        let msg = ConsoleError::NotAProject("/tmp/x".into()).to_string();
        assert!(
            msg.contains("Cargo.toml") && msg.contains("/tmp/x"),
            "{msg}"
        );
    }

    #[test]
    fn missing_dependency_error_mentions_autumn_web() {
        let msg = ConsoleError::MissingAutumnWebDependency.to_string();
        assert!(msg.contains("autumn-web"), "{msg}");
    }
}
