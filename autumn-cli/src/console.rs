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
        "Cargo.toml's `[features] playground` is not an array, so \
         `autumn-web/seed` cannot be added to it; rename that feature or make \
         it a list and re-run\n\
         See: docs/guide/console.md"
    )]
    UnusablePlaygroundFeature,

    #[error(
        "no package named `{0}` in this workspace; run `cargo metadata --no-deps` \
         to see the available members"
    )]
    UnknownPackage(String),

    // ── isolation guards ───────────────────────────────────────────────────
    //
    // The playground compiles the app's own modules into a separate crate and
    // is not always self-contained, so it must never join the default build
    // set — otherwise a broken playground breaks `cargo build`, and with it
    // `autumn dev`. Each of these is a configuration we cannot make safe on
    // the user's behalf, so we refuse before touching anything rather than
    // scaffold a project-wide outage.
    #[error(
        "`[features] default` enables `playground`, so the playground would be \
         built by a plain `cargo build` — the isolation `autumn console` relies \
         on. Remove `playground` from the default feature list (directly or via \
         `{0}`) and re-run.\n\
         See: docs/guide/console.md"
    )]
    PlaygroundFeatureIsDefault(String),

    #[error(
        "Cargo.toml already declares `[[bin]] name = \"playground\"` without \
         `required-features = [\"playground\"]`, so it would be built by a plain \
         `cargo build`. Add this line to that entry and re-run:\n\n    \
         required-features = [\"playground\"]\n\n\
         See: docs/guide/console.md"
    )]
    PlaygroundBinNotGated,

    #[error(
        "Cargo.toml's `[[bin]] name = \"playground\"` requires the feature \
         `{0}`, which `autumn console` does not enable — it activates the \
         default features plus `playground`, and `required-features` is an \
         all-of list, so Cargo would refuse to build the target. Either drop \
         `{0}` from that entry, or make `playground` enable it:\n\n    \
         [features]\n    playground = [\"autumn-web/seed\", \"{0}\"]\n\n\
         See: docs/guide/console.md"
    )]
    PlaygroundBinGateUnsatisfiable(String),

    #[error(
        "this package uses Rust edition 2015, where declaring a target turns off \
         Cargo's auto-discovery of the others — so `autumn console` cannot add \
         the playground entry without dropping your existing binaries from the \
         build. A scaffolded `src/bin/playground.rs` would meanwhile be \
         auto-discovered as an ungated binary and join every normal build, so \
         nothing has been written. Add this to Cargo.toml yourself, then re-run:\
         \n\n    [[bin]]\n    name = \"playground\"\n    path = \
         \"src/bin/playground.rs\"\n    required-features = [\"playground\"]\n\n\
         See: docs/guide/console.md"
    )]
    CannotIsolateOnEdition2015,
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
/// `playground_rel` is the playground's path relative to the package root.
/// `#[path]` resolves relative to the *directory holding the file*, so the
/// declarations have to be rebased whenever the playground is not at the
/// default location — a user-relocated `custom/playground.rs` would otherwise
/// get `#[path = "../schema.rs"]`, which points at a package-root `schema.rs`
/// that does not exist.
pub fn app_module_decls(project_root: &Path, playground_rel: &str) -> String {
    let src = project_root.join("src");
    let playground_dir = lexically_normalize(&project_root.join(playground_rel.replace('\\', "/")));
    let playground_dir = playground_dir
        .parent()
        .unwrap_or(&playground_dir)
        .to_path_buf();

    let mut out = String::new();
    for name in APP_MODULES {
        let target = if src.join(name).join("mod.rs").is_file() {
            src.join(name).join("mod.rs")
        } else if src.join(format!("{name}.rs")).is_file() {
            src.join(format!("{name}.rs"))
        } else {
            continue;
        };
        let rel = relative_path_from(&playground_dir, &target);
        let _ = writeln!(out, "#[path = \"{rel}\"]\nmod {name};");
    }
    out
}

/// Resolve `.` and `x/..` segments without touching the filesystem.
///
/// The module file may not exist yet and symlinks must not be resolved, so
/// `canonicalize` is the wrong tool; this is the purely lexical equivalent.
/// A leading `..` that cannot be collapsed is preserved, which is what lets a
/// `[[bin]] path` pointing outside the package still produce a correct answer.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The `/`-separated path that reaches `to` from directory `from_dir`.
///
/// Both sides are normalised first, so a `[[bin]]` path written as
/// `./custom/playground.rs` yields the same answer as `custom/playground.rs`
/// — counting raw components would have charged an extra `../` for the `.`.
fn relative_path_from(from_dir: &Path, to: &Path) -> String {
    let from = lexically_normalize(from_dir);
    let to = lexically_normalize(to);

    let mut from_parts = from.components().peekable();
    let mut to_parts = to.components().peekable();
    while from_parts.peek().is_some() && from_parts.peek() == to_parts.peek() {
        from_parts.next();
        to_parts.next();
    }

    let mut rel = "../".repeat(from_parts.count());
    let tail: PathBuf = to_parts.collect();
    rel.push_str(&tail.to_string_lossy().replace('\\', "/"));
    rel
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

    // Every isolation guard runs before the first edit is staged, so a manifest
    // we refuse is returned to the caller byte-identical.
    if let Some(via) = default_feature_chain_to_playground(&doc) {
        return Err(ConsoleError::PlaygroundFeatureIsDefault(via));
    }
    if let Some(entry) = declared_playground_bin(&doc)
        && let Some(problem) = playground_bin_gate_problem(&doc, entry)
    {
        return Err(problem);
    }
    // Nothing declares the target yet and we cannot add it safely: a scaffolded
    // file would be auto-discovered as an ungated binary.
    if declared_playground_bin(&doc).is_none() && adding_bin_would_break_autodiscovery(&doc) {
        return Err(ConsoleError::CannotIsolateOnEdition2015);
    }

    let changes = ManifestChanges {
        added_feature: ensure_playground_feature(&mut doc)?,
        added_bin: ensure_playground_bin(&mut doc),
    };

    Ok((doc.to_string(), changes))
}

/// Why an existing `[[bin]] name = "playground"` cannot be driven by this
/// command, if it cannot.
///
/// `required-features` is an **all-of** list: Cargo builds the target only when
/// every feature on it is active. The runner activates the default set plus
/// `playground`, so a gate is usable only when it names `playground` *and*
/// every other entry is reachable from that activation. A gate containing an
/// unreachable feature (`["playground", "tools"]` where `playground` does not
/// enable `tools`) looks gated but can never run — checking merely that
/// `playground` appears somewhere in the list is not enough.
fn playground_bin_gate_problem(
    doc: &toml_edit::DocumentMut,
    entry: &toml_edit::Table,
) -> Option<ConsoleError> {
    let required: Vec<String> = entry
        .get("required-features")
        .and_then(toml_edit::Item::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str())
        .map(str::to_owned)
        .collect();

    if !required.iter().any(|f| f == PLAYGROUND_FEATURE) {
        return Some(ConsoleError::PlaygroundBinNotGated);
    }

    // What `cargo run --features playground` actually turns on. We never pass
    // `--no-default-features`, so `default` is active too.
    let activated = feature_closure(doc, &["default", PLAYGROUND_FEATURE]);
    required
        .into_iter()
        .find(|f| f != PLAYGROUND_FEATURE && !activated.contains(f))
        .map(ConsoleError::PlaygroundBinGateUnsatisfiable)
}

/// The feature chain by which `[features] default` reaches `playground`, if it
/// does — `default` itself, or the intermediate feature that pulls it in.
///
/// A `playground` feature that is on by default makes
/// `required-features = ["playground"]` vacuous: the target rejoins the default
/// build set and the whole isolation guarantee evaporates.
fn default_feature_chain_to_playground(doc: &toml_edit::DocumentMut) -> Option<String> {
    let features = doc.get("features").and_then(toml_edit::Item::as_table)?;
    let mut queue: Vec<String> = vec!["default".to_owned()];
    let mut seen: Vec<String> = Vec::new();

    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        let enables = enabled_by(features, &name);
        seen.push(name.clone());

        if enables.iter().any(|e| e == PLAYGROUND_FEATURE) {
            return Some(name);
        }
        queue.extend(enables);
    }
    None
}

/// Every package feature activated by turning on `roots`, transitively.
fn feature_closure(doc: &toml_edit::DocumentMut, roots: &[&str]) -> Vec<String> {
    let Some(features) = doc.get("features").and_then(toml_edit::Item::as_table) else {
        return roots.iter().map(|r| (*r).to_owned()).collect();
    };
    let mut queue: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    let mut seen: Vec<String> = Vec::new();

    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        queue.extend(enabled_by(features, &name));
        seen.push(name);
    }
    seen
}

/// The package's own features that `name` enables directly. `dep/feat` edges
/// are skipped: they turn on a dependency's feature, never one of ours, so they
/// can never satisfy a `required-features` entry.
fn enabled_by(features: &toml_edit::Table, name: &str) -> Vec<String> {
    features
        .get(name)
        .and_then(toml_edit::Item::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter(|edge| !edge.contains('/'))
        .map(str::to_owned)
        .collect()
}

/// Ensure `[features] playground` exists and enables `autumn-web/seed`.
///
/// The edge is *merged into* whatever is already there rather than replacing
/// it, and a `playground` feature that already has the edge is left untouched.
/// Both halves matter:
///
/// * Skipping a pre-existing `playground` feature entirely would scaffold a
///   playground that cannot compile — the template imports
///   `autumn_web::seed::SeedContext`, which only exists with `autumn-web`'s
///   `seed` feature on. `playground = []` is a valid manifest, so this is a
///   real shape, not a hypothetical one.
/// * Overwriting it would throw away whatever else the user's feature enables.
///
/// Adding one array element does neither.
fn ensure_playground_feature(doc: &mut toml_edit::DocumentMut) -> Result<bool, ConsoleError> {
    use toml_edit::{Array, Item, Table, Value};

    let features = doc
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(features) = features.as_table_mut() else {
        return Err(ConsoleError::UnusableFeaturesTable);
    };

    let enables = features
        .entry(PLAYGROUND_FEATURE)
        .or_insert_with(|| Item::Value(Value::Array(Array::new())));
    let Some(enables) = enables.as_array_mut() else {
        return Err(ConsoleError::UnusablePlaygroundFeature);
    };

    let edge = format!("autumn-web/{REQUIRED_FEATURE}");
    if enables.iter().any(|f| f.as_str() == Some(edge.as_str())) {
        return Ok(false);
    }
    enables.push(edge.as_str());
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

/// The *effective* path of an already-declared `[[bin]] name = "playground"`,
/// if the manifest declares one at all.
///
/// Two callers depend on this, and both break if it under-reports:
///
/// * `ensure_playground_bin` uses it to decide whether a target already
///   exists. Cargo rejects a manifest with two same-named binaries outright —
///   `cargo metadata` itself fails, so *every* cargo command in the project
///   stops working. Missing an existing entry here would brick the project.
/// * `run` uses it so a user who relocated their playground keeps it there,
///   rather than us writing a file Cargo de-duplicates away and never runs.
///
/// `path` is therefore optional, not required: `[[bin]] name = "playground"`
/// with no `path` is a perfectly valid declaration for which Cargo infers
/// `src/bin/playground.rs` — the same location we would have used.
pub fn declared_playground_path(doc: &toml_edit::DocumentMut) -> Option<String> {
    Some(
        declared_playground_bin(doc)?
            .get("path")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or(PLAYGROUND_REL_PATH)
            .to_owned(),
    )
}

/// The already-declared `[[bin]] name = "playground"` entry, if any.
fn declared_playground_bin(doc: &toml_edit::DocumentMut) -> Option<&toml_edit::Table> {
    doc.get("bin")
        .and_then(toml_edit::Item::as_array_of_tables)?
        .iter()
        .find(|t| t.get("name").and_then(toml_edit::Item::as_str) == Some(PLAYGROUND_BIN_NAME))
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
///
/// `rel` is `path` relative to the package root; the `#[path]` module
/// declarations are rebased against it so a relocated playground still reaches
/// `src/`.
fn write_playground(path: &Path, rel: &str, project_dir: &Path, manifest: &str) {
    let project_name = package_name(manifest).unwrap_or_else(|| project_dir.display().to_string());
    let source = render_playground(&project_name, &app_module_decls(project_dir, rel));
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
    let playground_rel =
        declared_playground_path(&doc).unwrap_or_else(|| PLAYGROUND_REL_PATH.to_owned());
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
        write_playground(&playground_path, &playground_rel, &project_dir, &manifest);
        eprintln!("  {} {playground_rel}", outcome.verb());
    } else {
        eprintln!(
            "  {} {playground_rel} (pass --force to regenerate it from the template)",
            outcome.verb()
        );
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
        assert_eq!(app_module_decls(tmp.path(), PLAYGROUND_REL_PATH).trim(), "");
    }

    #[test]
    fn app_module_decls_detects_schema_and_models_directory() {
        let tmp = project_with(&["src/main.rs", "src/schema.rs", "src/models/mod.rs"]);
        let decls = app_module_decls(tmp.path(), PLAYGROUND_REL_PATH);
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
        let decls = app_module_decls(tmp.path(), PLAYGROUND_REL_PATH);
        assert!(
            decls.contains("#[path = \"../models.rs\"]"),
            "must support the single-file src/models.rs layout:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_detects_repositories() {
        let tmp = project_with(&["src/main.rs", "src/repositories/mod.rs"]);
        let decls = app_module_decls(tmp.path(), PLAYGROUND_REL_PATH);
        assert!(
            decls.contains("mod repositories;"),
            "must wire src/repositories into the playground crate:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_prefers_mod_rs_over_single_file() {
        let tmp = project_with(&["src/models.rs", "src/models/mod.rs"]);
        let decls = app_module_decls(tmp.path(), PLAYGROUND_REL_PATH);
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
    fn ensure_manifest_wiring_merges_seed_into_an_existing_playground_feature() {
        // `playground = []` is a valid manifest. Leaving it alone would scaffold
        // a playground that cannot compile: the template imports
        // `autumn_web::seed::SeedContext`, which needs `autumn-web/seed`.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
playground = []

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(changes.added_feature);
        assert!(
            out.contains("\"autumn-web/seed\""),
            "the seed edge must be merged into an existing playground feature:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_merge_preserves_a_user_tuned_playground_feature() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
playground = ["autumn-web/redis"]

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(changes.added_feature);
        assert!(
            out.contains("\"autumn-web/redis\"") && out.contains("\"autumn-web/seed\""),
            "merging must add the edge without dropping the user's own:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_errors_on_a_non_array_playground_feature() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
playground = "nonsense"

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::UnusablePlaygroundFeature)
        );
    }

    #[test]
    fn ensure_manifest_wiring_recognises_a_path_less_playground_bin() {
        // `[[bin]] name = "playground"` with no `path` is valid — Cargo infers
        // `src/bin/playground.rs`. Failing to see it and appending a second
        // entry makes `cargo metadata` reject the manifest outright, which
        // breaks EVERY cargo command in the project.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "playground"
required-features = ["playground"]

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, changes) = ensure_manifest_wiring(manifest).unwrap();
        assert!(!changes.added_bin, "the existing target must be recognised");
        assert_eq!(
            out.matches("name = \"playground\"").count(),
            1,
            "a duplicate binary name makes the manifest unparsable to cargo:\n{out}"
        );
    }

    // ── isolation guards (Codex review, a07f2f1..) ────────────────────────

    #[test]
    fn ensure_manifest_wiring_rejects_a_default_enabled_playground_feature() {
        // `default = ["playground"]` satisfies `required-features` on every
        // build, so the gate becomes vacuous and the playground rejoins the
        // default build set.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
default = ["flash", "playground"]
flash = []

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::PlaygroundFeatureIsDefault(
                "default".to_owned()
            ))
        );
    }

    #[test]
    fn ensure_manifest_wiring_rejects_a_transitively_default_playground_feature() {
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
default = ["bundle"]
bundle = ["playground"]

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::PlaygroundFeatureIsDefault(
                "bundle".to_owned()
            )),
            "the chain must be followed through intermediate features"
        );
    }

    #[test]
    fn ensure_manifest_wiring_default_scan_terminates_on_a_feature_cycle() {
        // A self-referential feature list must not hang the scan.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
default = ["a"]
a = ["b"]
b = ["a"]

[dependencies]
autumn-web = "0.6.0"
"#;
        assert!(ensure_manifest_wiring(manifest).is_ok());
    }

    #[test]
    fn ensure_manifest_wiring_rejects_an_ungated_existing_playground_bin() {
        // Reusing an ungated target would put the seed-dependent template into
        // every normal `cargo build`.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "playground"
path = "src/bin/playground.rs"

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::PlaygroundBinNotGated)
        );
    }

    #[test]
    fn ensure_manifest_wiring_rejects_a_partially_satisfiable_gate() {
        // `required-features` is an ALL-of list. The runner enables the default
        // set plus `playground`, so a gate naming `tools` as well can never be
        // satisfied — checking only that `playground` appears is not enough.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
playground = ["autumn-web/seed"]
tools = []

[[bin]]
name = "playground"
required-features = ["playground", "tools"]

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::PlaygroundBinGateUnsatisfiable(
                "tools".to_owned()
            ))
        );
    }

    #[test]
    fn ensure_manifest_wiring_accepts_a_gate_whose_extras_playground_enables() {
        // Same shape, but `playground` pulls `tools` in — so `--features
        // playground` does satisfy the whole gate and the target is usable.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
playground = ["autumn-web/seed", "tools"]
tools = []

[[bin]]
name = "playground"
required-features = ["playground", "tools"]

[dependencies]
autumn-web = "0.6.0"
"#;
        assert!(ensure_manifest_wiring(manifest).is_ok());
    }

    #[test]
    fn ensure_manifest_wiring_accepts_a_gate_extra_that_is_a_default_feature() {
        // The runner does not pass --no-default-features, so a default feature
        // in the gate is satisfied too.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[features]
default = ["flash"]
flash = []
playground = ["autumn-web/seed"]

[[bin]]
name = "playground"
required-features = ["playground", "flash"]

[dependencies]
autumn-web = "0.6.0"
"#;
        assert!(ensure_manifest_wiring(manifest).is_ok());
    }

    #[test]
    fn ensure_manifest_wiring_rejects_a_foreign_gate_on_the_playground_bin() {
        // `--features playground` never satisfies `tools`, so cargo would
        // silently decline to run the target.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "playground"
required-features = ["tools"]

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::PlaygroundBinNotGated)
        );
    }

    #[test]
    fn isolation_guards_leave_the_manifest_untouched() {
        // Guards run before the first staged edit, so a refused project keeps
        // its manifest byte-identical.
        for manifest in [
            "[package]\nname = \"a\"\nedition = \"2024\"\n\n[features]\ndefault = [\"playground\"]\n\n[dependencies]\nautumn-web = \"0.6.0\"\n",
            "[package]\nname = \"a\"\nedition = \"2024\"\n\n[[bin]]\nname = \"playground\"\n\n[dependencies]\nautumn-web = \"0.6.0\"\n",
        ] {
            assert!(
                ensure_manifest_wiring(manifest).is_err(),
                "expected a refusal for:\n{manifest}"
            );
        }
    }

    // ── #[path] rebasing for a relocated playground ───────────────────────

    #[test]
    fn app_module_decls_rebase_for_a_relocated_playground() {
        let tmp = project_with(&["src/main.rs", "src/schema.rs"]);
        let decls = app_module_decls(tmp.path(), "custom/playground.rs");
        assert!(
            decls.contains("#[path = \"../src/schema.rs\"]"),
            "a playground one level deep must reach src/ with a single hop:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_rebase_for_a_deeply_nested_playground() {
        let tmp = project_with(&["src/main.rs", "src/schema.rs"]);
        let decls = app_module_decls(tmp.path(), "tools/dev/pg.rs");
        assert!(
            decls.contains("#[path = \"../../src/schema.rs\"]"),
            "two directories deep needs two hops back to the package root:\n{decls}"
        );
    }

    #[test]
    fn app_module_decls_normalize_a_dot_prefixed_playground_path() {
        // `./custom/playground.rs` is a valid `[[bin]] path`. Counting raw
        // components charges an extra `../` for the leading `.`, which would
        // aim the declaration a directory too high.
        let tmp = project_with(&["src/main.rs", "src/schema.rs"]);
        assert_eq!(
            app_module_decls(tmp.path(), "./custom/playground.rs"),
            app_module_decls(tmp.path(), "custom/playground.rs"),
            "a `./` prefix must not change the rebased path"
        );
    }

    #[test]
    fn app_module_decls_handle_a_playground_outside_the_package() {
        // A `[[bin]] path` may point outside the package root; the ascent then
        // has to climb back down into it by name rather than blindly counting.
        let tmp = project_with(&["src/main.rs", "src/schema.rs"]);
        let decls = app_module_decls(tmp.path(), "../tools/playground.rs");
        let leaf = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            decls.contains(&format!("../{leaf}/src/schema.rs")),
            "must descend back into the package by name:\n{decls}"
        );
    }

    #[test]
    fn relative_path_from_resolves_dot_and_parent_segments() {
        assert_eq!(
            relative_path_from(Path::new("/a/b/src/bin"), Path::new("/a/b/src/schema.rs")),
            "../schema.rs"
        );
        assert_eq!(
            relative_path_from(Path::new("/a/b/./custom"), Path::new("/a/b/src/schema.rs")),
            "../src/schema.rs"
        );
        assert_eq!(
            relative_path_from(
                Path::new("/a/b/x/../custom"),
                Path::new("/a/b/src/schema.rs")
            ),
            "../src/schema.rs"
        );
    }

    #[test]
    fn lexically_normalize_collapses_without_touching_the_filesystem() {
        assert_eq!(lexically_normalize(Path::new("a/./b")), Path::new("a/b"));
        assert_eq!(lexically_normalize(Path::new("a/b/../c")), Path::new("a/c"));
        // A leading `..` has nothing to collapse into and must survive.
        assert_eq!(lexically_normalize(Path::new("../a")), Path::new("../a"));
    }

    #[test]
    fn declared_playground_path_infers_the_default_for_a_path_less_entry() {
        let doc = r#"[[bin]]
name = "playground"
"#
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
        assert_eq!(
            declared_playground_path(&doc).as_deref(),
            Some(PLAYGROUND_REL_PATH),
            "a path-less entry resolves to the location Cargo infers"
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
required-features = ["playground"]

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
        // Refused outright: we cannot add the gated entry, and a scaffolded
        // file would be auto-discovered as an UNGATED binary that joins every
        // normal build — the exact outage the gate exists to prevent.
        assert_eq!(
            ensure_manifest_wiring(manifest),
            Err(ConsoleError::CannotIsolateOnEdition2015)
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
