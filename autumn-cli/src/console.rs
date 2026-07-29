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
    #[error("not an Autumn project: no `Cargo.toml` found in {0}")]
    NotAProject(String),

    #[error("could not parse Cargo.toml: {0}")]
    ManifestParse(String),

    #[error("Cargo.toml has no `autumn-web` dependency")]
    MissingAutumnWebDependency,
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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ManifestChanges {
    pub added_bin: bool,
    pub added_seed_feature: bool,
    pub added_default_run: bool,
}

impl ManifestChanges {
    const fn any(self) -> bool {
        self.added_bin || self.added_seed_feature || self.added_default_run
    }
}

pub const PLAYGROUND_REL_PATH: &str = "src/bin/playground.rs";
pub const PLAYGROUND_BIN_NAME: &str = "playground";

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

/// Wire `Cargo.toml` so the scaffolded playground actually builds.
///
/// Three edits, each independently idempotent:
///
/// 1. enable the `autumn-web` `seed` feature the template's `SeedContext`
///    bootstrap needs;
/// 2. register the `playground` bin target;
/// 3. set `default-run` when the package has a `src/main.rs` and does not
///    already declare one — without it, adding a second binary would make a
///    bare `cargo run` ambiguous and break the project we are supposed to help
///    with.
///
/// Edits go through `toml_edit`, so comments, key order, and hand-formatted
/// arrays survive. A manifest that cannot be parsed is an error, never a
/// best-effort rewrite.
pub fn ensure_manifest_wiring(
    manifest: &str,
    has_main_rs: bool,
) -> Result<(String, ManifestChanges), ConsoleError> {
    let mut doc = manifest
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConsoleError::ManifestParse(e.to_string()))?;

    let changes = ManifestChanges {
        added_seed_feature: ensure_seed_feature(&mut doc)?,
        added_bin: ensure_playground_bin(&mut doc),
        added_default_run: has_main_rs && ensure_default_run(&mut doc),
    };

    Ok((doc.to_string(), changes))
}

/// Add `"seed"` to the `autumn-web` dependency's feature list, handling the
/// plain-string, inline-table, and `[dependencies.autumn-web]` shapes.
///
/// Returns `true` when the manifest was changed.
fn ensure_seed_feature(doc: &mut toml_edit::DocumentMut) -> Result<bool, ConsoleError> {
    use toml_edit::{Item, Value};

    let dep = doc
        .get_mut("dependencies")
        .and_then(|deps| deps.get_mut("autumn-web"))
        .ok_or(ConsoleError::MissingAutumnWebDependency)?;

    match dep {
        // `autumn-web = "0.6.0"` — promote to an inline table so the feature
        // has somewhere to live, preserving the pinned version.
        Item::Value(Value::String(version)) => {
            let mut table = toml_edit::InlineTable::new();
            table.insert("version", Value::String(version.clone()));
            let mut features = toml_edit::Array::new();
            features.push(REQUIRED_FEATURE);
            table.insert("features", Value::Array(features));
            *dep = Item::Value(Value::InlineTable(table));
            Ok(true)
        }
        // `autumn-web = { version = "…", features = [...] }`
        Item::Value(Value::InlineTable(table)) => {
            let features = table
                .entry("features")
                .or_insert_with(|| Value::Array(toml_edit::Array::new()));
            Ok(push_feature(features.as_array_mut()))
        }
        // `[dependencies.autumn-web]` as its own table.
        Item::Table(table) => {
            let features = table
                .entry("features")
                .or_insert_with(|| Item::Value(Value::Array(toml_edit::Array::new())));
            Ok(push_feature(features.as_array_mut()))
        }
        _ => Err(ConsoleError::MissingAutumnWebDependency),
    }
}

/// Push [`REQUIRED_FEATURE`] onto a features array unless it is already there.
/// A `features` key that is not an array is left alone (`false`).
fn push_feature(features: Option<&mut toml_edit::Array>) -> bool {
    let Some(features) = features else {
        return false;
    };
    if features
        .iter()
        .any(|f| f.as_str() == Some(REQUIRED_FEATURE))
    {
        return false;
    }
    features.push(REQUIRED_FEATURE);
    true
}

/// Register `[[bin]] name = "playground"` unless a target with that name is
/// already declared (which may be a user-owned entry pointing elsewhere —
/// never rewrite it).
fn ensure_playground_bin(doc: &mut toml_edit::DocumentMut) -> bool {
    use toml_edit::{ArrayOfTables, Item, Table, value};

    let bins = doc
        .entry("bin")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let Some(bins) = bins.as_array_of_tables_mut() else {
        return false;
    };
    if bins
        .iter()
        .any(|t| t.get("name").and_then(|n| n.as_str()) == Some(PLAYGROUND_BIN_NAME))
    {
        return false;
    }

    let mut entry = Table::new();
    entry.insert("name", value(PLAYGROUND_BIN_NAME));
    entry.insert("path", value(PLAYGROUND_REL_PATH));
    bins.push(entry);
    true
}

/// Set `default-run` to the package name so a bare `cargo run` keeps working
/// now that the package has more than one binary. Never overrides an existing
/// value.
fn ensure_default_run(doc: &mut toml_edit::DocumentMut) -> bool {
    let Some(package) = doc
        .get_mut("package")
        .and_then(toml_edit::Item::as_table_mut)
    else {
        return false;
    };
    if package.contains_key("default-run") {
        return false;
    }
    let Some(name) = package
        .get("name")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned)
    else {
        return false;
    };
    package.insert("default-run", toml_edit::value(name));
    true
}

/// Print an error and exit non-zero. `autumn console` never fails silently.
fn fail(err: &ConsoleError) -> ! {
    eprintln!("\u{2717} {err}");
    std::process::exit(1);
}

/// Entry point for `autumn console`.
pub fn run(profile: &str, package: Option<&str>, force: bool, scaffold_only: bool) {
    eprintln!("\u{1F342} autumn console\n");
    eprintln!("  Profile: {profile}");

    let project_dir: PathBuf = package
        .and_then(crate::seed::find_package_dir)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Validate the project *before* writing anything, so a failed run leaves
    // the working tree untouched.
    let manifest_path = project_dir.join("Cargo.toml");
    if !manifest_path.is_file() {
        fail(&ConsoleError::NotAProject(
            project_dir.display().to_string(),
        ));
    }
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("\u{2717} could not read {}: {e}", manifest_path.display());
            std::process::exit(1);
        }
    };
    let has_main_rs = project_dir.join("src/main.rs").is_file();
    let (updated_manifest, changes) = match ensure_manifest_wiring(&manifest, has_main_rs) {
        Ok(result) => result,
        Err(e) => fail(&e),
    };

    // Scaffold (or deliberately keep) the playground source.
    let playground_path = project_dir.join(PLAYGROUND_REL_PATH);
    let outcome = scaffold_outcome(playground_path.is_file(), force);
    if outcome.writes_file() {
        let project_name =
            package_name(&manifest).unwrap_or_else(|| project_dir.display().to_string());
        let source = render_playground(&project_name, &app_module_decls(&project_dir));
        if let Some(parent) = playground_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!("\u{2717} could not create {}: {e}", parent.display());
            std::process::exit(1);
        }
        if let Err(e) = std::fs::write(&playground_path, source) {
            eprintln!(
                "\u{2717} could not write {}: {e}",
                playground_path.display()
            );
            std::process::exit(1);
        }
        eprintln!("  {} {PLAYGROUND_REL_PATH}", outcome.verb());
    } else {
        eprintln!(
            "  Using existing {PLAYGROUND_REL_PATH} \
             (pass --force to regenerate it from the template)"
        );
    }

    if changes.any() {
        if let Err(e) = std::fs::write(&manifest_path, updated_manifest) {
            eprintln!("\u{2717} could not write {}: {e}", manifest_path.display());
            std::process::exit(1);
        }
        let mut edits: Vec<&str> = Vec::new();
        if changes.added_bin {
            edits.push("[[bin]] playground");
        }
        if changes.added_seed_feature {
            edits.push("autumn-web feature \"seed\"");
        }
        if changes.added_default_run {
            edits.push("default-run");
        }
        eprintln!("  Updated Cargo.toml ({})", edits.join(", "));
    }

    if scaffold_only {
        eprintln!(
            "\n\u{2713} Playground ready. Edit {PLAYGROUND_REL_PATH}, \
             then run `autumn console`."
        );
        return;
    }

    eprintln!("\n  Building and running the playground...\n");

    let mut cmd = Command::new("cargo");
    cmd.args(["run", "--bin", PLAYGROUND_BIN_NAME]);
    if let Some(pkg) = package {
        cmd.args(["--package", pkg]);
    }
    cmd.env("AUTUMN_ENV", profile);
    cmd.env("AUTUMN_PROFILE", profile);
    // Run from the project directory so the playground's `SeedContext` reads
    // `autumn.toml` from the package root, not the workspace root.
    cmd.current_dir(&project_dir);

    match cmd.status() {
        Ok(status) if status.success() => {}
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
    fn ensure_manifest_wiring_adds_the_playground_bin_entry() {
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST, true).unwrap();
        assert!(changes.added_bin);
        assert!(
            out.contains("[[bin]]") && out.contains("name = \"playground\""),
            "must register the playground bin target:\n{out}"
        );
        assert!(
            out.contains("path = \"src/bin/playground.rs\""),
            "bin entry must point at the scaffolded file:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_adds_the_seed_feature_to_a_plain_dependency() {
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST, true).unwrap();
        assert!(changes.added_seed_feature);
        assert!(
            out.contains("\"seed\""),
            "the playground uses autumn_web::seed, so the feature must be on:\n{out}"
        );
        assert!(
            out.contains("0.6.0"),
            "the pinned version must survive the rewrite:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_sets_default_run_when_main_rs_exists() {
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST, true).unwrap();
        assert!(changes.added_default_run);
        assert!(
            out.contains("default-run = \"my-app\""),
            "a second bin makes bare `cargo run` ambiguous without default-run:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_skips_default_run_without_main_rs() {
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST, false).unwrap();
        assert!(!changes.added_default_run);
        assert!(
            !out.contains("default-run"),
            "must not name a bin target that does not exist:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_is_idempotent() {
        let (once, _) = ensure_manifest_wiring(PLAIN_MANIFEST, true).unwrap();
        let (twice, changes) = ensure_manifest_wiring(&once, true).unwrap();
        assert_eq!(
            changes,
            ManifestChanges::default(),
            "a second run must report no changes"
        );
        assert_eq!(once, twice, "a second run must not alter the manifest");
    }

    #[test]
    fn ensure_manifest_wiring_preserves_existing_features() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"

[dependencies]
autumn-web = { version = "0.6.0", features = ["i18n"] }
"#;
        let (out, changes) = ensure_manifest_wiring(manifest, true).unwrap();
        assert!(changes.added_seed_feature);
        assert!(
            out.contains("\"i18n\"") && out.contains("\"seed\""),
            "must add `seed` alongside the existing features:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_handles_a_dependency_table_section() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"

[dependencies.autumn-web]
version = "0.6.0"
default-features = false
features = ["db"]
"#;
        let (out, changes) = ensure_manifest_wiring(manifest, true).unwrap();
        assert!(changes.added_seed_feature);
        assert!(
            out.contains("\"db\"") && out.contains("\"seed\""),
            "must extend the [dependencies.autumn-web] table:\n{out}"
        );
        assert!(
            out.contains("default-features = false"),
            "must not disturb unrelated keys:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_handles_a_path_dependency() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"

[dependencies]
autumn-web = { path = "../../autumn" }
"#;
        let (out, changes) = ensure_manifest_wiring(manifest, true).unwrap();
        assert!(changes.added_seed_feature);
        assert!(
            out.contains("path = \"../../autumn\"") && out.contains("\"seed\""),
            "must keep the path and add the feature:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_keeps_an_existing_default_run() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
default-run = "server"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest, true).unwrap();
        assert!(!changes.added_default_run);
        assert!(
            out.contains("default-run = \"server\"") && !out.contains("default-run = \"my-app\""),
            "must not override the user's default-run:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_keeps_an_existing_playground_bin_entry() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"

[[bin]]
name = "playground"
path = "custom/playground.rs"

[dependencies]
autumn-web = { version = "0.6.0", features = ["seed"] }
"#;
        let (out, changes) = ensure_manifest_wiring(manifest, false).unwrap();
        assert!(!changes.added_bin);
        assert!(
            out.contains("path = \"custom/playground.rs\""),
            "must not rewrite a user-owned bin entry:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_errors_without_an_autumn_web_dependency() {
        let manifest = "[package]\nname = \"my-app\"\n\n[dependencies]\nserde = \"1\"\n";
        assert_eq!(
            ensure_manifest_wiring(manifest, true),
            Err(ConsoleError::MissingAutumnWebDependency)
        );
    }

    #[test]
    fn ensure_manifest_wiring_errors_on_an_unparsable_manifest() {
        let err = ensure_manifest_wiring("this is not = = toml", true).unwrap_err();
        assert!(matches!(err, ConsoleError::ManifestParse(_)));
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
