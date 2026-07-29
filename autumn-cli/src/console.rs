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
         `{0}` from that entry, or declare `{0}` and have `playground` enable \
         it:\n\n    [features]\n    {0} = []\n    playground = \
         [\"autumn-web/seed\", \"{0}\"]\n\n\
         Note `default` is not implicitly present: a gate naming a feature the \
         manifest never declares can never be satisfied.\n\
         See: docs/guide/console.md"
    )]
    PlaygroundBinGateUnsatisfiable(String),

    #[error(
        "the playground would be written to `{0}`, which is outside this \
         package. `autumn console` only ever writes inside the package \
         directory, so nothing has been created. This happens when a \
         `[[bin]] path` points outside the package, or when a directory on the \
         way (e.g. `src/bin`) is a symlink leading out of the checkout.\n\
         See: docs/guide/console.md"
    )]
    PlaygroundPathEscapesPackage(String),

    #[error(
        "the playground destination is unusable: {0}. Nothing has been \
         written — remove or rename it and re-run.\n\
         See: docs/guide/console.md"
    )]
    PlaygroundDestinationUnusable(String),

    #[error(
        "Cargo.toml has a root-level `bin` key that is not a `[[bin]]` table \
         array, so the playground target cannot be registered — and an \
         unregistered `src/bin/playground.rs` would be auto-discovered as an \
         ungated binary and join every normal build. Replace it with `[[bin]]` \
         sections and re-run.\n\
         See: docs/guide/console.md"
    )]
    UnusableBinTable,

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

/// How many times `write_atomic` retries an exclusive temp-file create before
/// giving up. Collisions only happen against our own concurrent runs, so a
/// handful is ample.
const MAX_TEMP_FILE_ATTEMPTS: u32 = 16;

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
    // Resolve both ends through symlinks before measuring between them. The
    // kernel applies `..` AFTER following a link, so a playground reached via
    // `custom -> tools/bin` has its `#[path]` values resolved from
    // `tools/bin/`, not from `custom/`. Measuring lexically would aim every
    // declaration one directory tree sideways.
    let playground_dir = resolve_through_symlinks(&playground_dir);

    let mut out = String::new();
    for name in APP_MODULES {
        let target = if src.join(name).join("mod.rs").is_file() {
            src.join(name).join("mod.rs")
        } else if src.join(format!("{name}.rs")).is_file() {
            src.join(format!("{name}.rs"))
        } else {
            continue;
        };
        let rel = relative_path_from(&playground_dir, &resolve_through_symlinks(&target));
        let _ = writeln!(out, "#[path = \"{rel}\"]\nmod {name};");
    }
    out
}

/// The real location of `path`, following symlinks as far as the filesystem
/// can — falling back to the deepest ancestor that does exist plus the
/// remaining components, since the playground's directory need not exist yet.
fn resolve_through_symlinks(path: &Path) -> PathBuf {
    let normalized = lexically_normalize(path);
    if let Ok(real) = normalized.canonicalize() {
        return real;
    }

    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = normalized.as_path();
    while let Some(parent) = probe.parent() {
        if let Some(name) = probe.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(real) = parent.canonicalize() {
            let mut resolved = real;
            resolved.extend(suffix.iter().rev());
            return resolved;
        }
        probe = parent;
    }
    normalized
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
pub fn ensure_manifest_wiring(
    manifest: &str,
    inherited_edition: Option<&str>,
) -> Result<(String, ManifestChanges), ConsoleError> {
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
    // A `bin` key we cannot append to (e.g. a valid root-level `bin = []`,
    // which toml_edit exposes as a plain array) means the target can never be
    // registered — and the scaffolded file would then be auto-discovered
    // ungated, rejoining the default build. Refuse rather than report success.
    if doc
        .get("bin")
        .is_some_and(|b| b.as_array_of_tables().is_none())
    {
        return Err(ConsoleError::UnusableBinTable);
    }
    // Nothing declares the target yet and we cannot add it safely: a scaffolded
    // file would be auto-discovered as an ungated binary.
    if declared_playground_bin(&doc).is_none()
        && adding_bin_would_break_autodiscovery(&doc, inherited_edition)
    {
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
    // `--no-default-features`, so the default set is active too — but only if
    // the package declares one. A gate naming a feature that does not exist
    // can never be satisfied: cargo warns "`default` is not present in
    // [features] section" and refuses to build the target. Seeding `default`
    // unconditionally would accept exactly that unrunnable gate.
    let mut roots = vec![PLAYGROUND_FEATURE];
    if declares_feature(doc, "default") {
        roots.push("default");
    }
    let activated = feature_closure(doc, &roots);
    required
        .into_iter()
        .find(|f| f != PLAYGROUND_FEATURE && !activated.contains(f))
        .map(ConsoleError::PlaygroundBinGateUnsatisfiable)
}

/// Whether the package declares `[features] <name>` itself.
///
/// A `required-features` entry naming a feature the manifest never defines is
/// unsatisfiable — including `default`, which is not implicitly present.
fn declares_feature(doc: &toml_edit::DocumentMut, name: &str) -> bool {
    doc.get("features")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|features| features.contains_key(name))
}

/// The feature chain by which `[features] default` reaches `playground`, if it
/// does — `default` itself, or the intermediate feature that pulls it in.
///
/// A `playground` feature that is on by default makes
/// `required-features = ["playground"]` vacuous: the target rejoins the default
/// build set and the whole isolation guarantee evaporates.
fn default_feature_chain_to_playground(doc: &toml_edit::DocumentMut) -> Option<String> {
    let features = doc
        .get("features")
        .and_then(toml_edit::Item::as_table_like)?;
    let mut queue: Vec<String> = vec!["default".to_owned()];
    let mut seen: Vec<String> = Vec::new();

    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        let enables = enabled_by(doc, features, &name);
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
    let Some(features) = doc.get("features").and_then(toml_edit::Item::as_table_like) else {
        return roots.iter().map(|r| (*r).to_owned()).collect();
    };
    let mut queue: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    let mut seen: Vec<String> = Vec::new();

    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        queue.extend(enabled_by(doc, features, &name));
        seen.push(name);
    }
    seen
}

/// The package features that `name` enables directly.
///
/// The `/` forms are not simply "someone else's features":
///
/// * `dep/feat` (**strong**) activates the dependency `dep` as well as its
///   feature — but it turns on a package feature named `dep` only when `dep`
///   is an **optional** dependency, because the thing it activates is the
///   implicit `dep = ["dep:dep"]` feature Cargo synthesises for optional
///   dependencies. Against a *required* dependency there is no such feature,
///   and a same-named feature the package happens to declare itself stays off.
///   Both halves are verified against cargo with a deliberately broken target
///   gated on `required-features = ["playground"]`: with an optional
///   dependency the target is built (feature active, with or without an
///   explicit `[features] playground` entry), with a required one it is
///   skipped (feature inactive).
/// * `dep?/feat` (**weak**) only adds `feat` if `dep` is already active, so it
///   never activates anything on its own.
/// * `dep:name` selects an optional dependency without creating a feature of
///   ours to follow.
fn enabled_by(
    doc: &toml_edit::DocumentMut,
    features: &dyn toml_edit::TableLike,
    name: &str,
) -> Vec<String> {
    features
        .get(name)
        .and_then(toml_edit::Item::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter_map(|edge| match edge.split_once('/') {
            // Weak: contributes nothing by itself.
            Some((dep, _)) if dep.ends_with('?') => None,
            // Strong: activates the implicit feature named after the
            // dependency — which exists only if that dependency is optional.
            Some((dep, _)) => is_optional_dependency(doc, dep).then(|| dep.to_owned()),
            None if edge.starts_with("dep:") => None,
            None => Some(edge.to_owned()),
        })
        .collect()
}

/// Whether *any* dependency table declares `name` as an optional dependency,
/// for which Cargo synthesises an implicit `name = ["dep:name"]` feature.
///
/// Cargo creates that implicit feature for optional dependencies wherever they
/// are declared — `[dependencies]`, `[build-dependencies]`, and the
/// target-specific `[target.'cfg(...)'.dependencies]` tables alike — so
/// scanning only the top level would miss a real manifest and suppress an edge
/// Cargo then demands, rejecting the manifest outright.
fn is_optional_dependency(doc: &toml_edit::DocumentMut, name: &str) -> bool {
    // Every lookup goes through `as_table_like`, which covers BOTH the header
    // form (`[target.'cfg(unix)'.dependencies]`, a `Table`) and the inline form
    // (`target = { 'cfg(unix)' = { dependencies = { … } } }`, an
    // `InlineTable`). Enumerating representations is what left the two earlier
    // versions of this scan incomplete: the same manifest written a different
    // valid way slipped through each time.
    fn declares_optional(table: Option<&toml_edit::Item>, name: &str) -> bool {
        table
            .and_then(toml_edit::Item::as_table_like)
            .and_then(|deps| deps.get(name))
            .and_then(toml_edit::Item::as_table_like)
            .is_some_and(|dep| {
                dep.get("optional")
                    .and_then(toml_edit::Item::as_bool)
                    .unwrap_or(false)
            })
    }

    if declares_optional(doc.get("dependencies"), name)
        || declares_optional(doc.get("build-dependencies"), name)
    {
        return true;
    }

    doc.get("target")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|targets| {
            targets.iter().any(|(_, cfg)| {
                cfg.as_table_like().is_some_and(|cfg| {
                    declares_optional(cfg.get("dependencies"), name)
                        || declares_optional(cfg.get("build-dependencies"), name)
                })
            })
        })
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

    let optional_dep_named_playground = is_optional_dependency(doc, PLAYGROUND_FEATURE);

    let features = doc
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(features) = features.as_table_like_mut() else {
        return Err(ConsoleError::UnusableFeaturesTable);
    };

    // An optional dependency named `playground` gets an implicit
    // `playground = ["dep:playground"]` feature from Cargo. Declaring the key
    // explicitly suppresses that, and Cargo then rejects the manifest outright
    // ("optional dependency `playground` is not included in any feature"),
    // breaking every command. Carry the edge over when we create the key; an
    // already-explicit feature is the user's and Cargo has already validated
    // it, so it is left alone.
    let seed_with_dep = !features.contains_key(PLAYGROUND_FEATURE) && optional_dep_named_playground;
    let enables = features.entry(PLAYGROUND_FEATURE).or_insert_with(|| {
        let mut initial = Array::new();
        if seed_with_dep {
            initial.push(format!("dep:{PLAYGROUND_FEATURE}"));
        }
        Item::Value(Value::Array(initial))
    });
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
pub fn adding_bin_would_break_autodiscovery(
    doc: &toml_edit::DocumentMut,
    inherited_edition: Option<&str>,
) -> bool {
    let Some(package) = doc.get("package").and_then(toml_edit::Item::as_table) else {
        return false;
    };
    if !package_is_edition_2015(package, inherited_edition) {
        return false;
    }
    // Only *binary* configuration proves bin auto-discovery is already off.
    // A `[lib]`, `[[test]]`, or `[[example]]` says nothing about it: an
    // edition-2015 package with `[lib]` and no `[[bin]]` still auto-discovers
    // `src/main.rs` and `src/bin/*.rs`, so treating the library as evidence
    // would let us append an entry that silently drops every one of them.
    let bin_config_present = doc.get("bin").is_some() || package.get("autobins").is_some();
    !bin_config_present
}

/// Whether the package is edition 2015, resolving the workspace-inherited
/// form.
///
/// `edition` may be absent (genuinely 2015), a literal string, or the
/// inherited `edition.workspace = true` shape — for which `as_str()` yields
/// `None`. Reading that `None` as "2015" would misclassify most modern
/// workspace members and refuse them for a legacy hazard they do not have, so
/// the inherited case defers to the workspace root's value.
fn package_is_edition_2015(package: &toml_edit::Table, inherited_edition: Option<&str>) -> bool {
    package.get("edition").is_none_or(|item| {
        item.as_str().map_or_else(
            // Inherited: believe the workspace root. When it cannot be read,
            // assume modern — `[workspace.package]` inheritance postdates
            // edition 2015 by years, so an inherited 2015 is vanishingly rare
            // while inherited 2021/2024 is the common case.
            || inherited_edition == Some("2015"),
            |edition| edition == "2015",
        )
    })
}

/// The `[workspace.package] edition` a member would inherit, found by walking
/// up from the package directory to the workspace root.
fn workspace_inherited_edition(project_dir: &Path) -> Option<String> {
    for dir in project_dir.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let Ok(doc) = std::fs::read_to_string(&manifest)
            .ok()?
            .parse::<toml_edit::DocumentMut>()
        else {
            continue;
        };
        if let Some(edition) = doc
            .get("workspace")
            .and_then(|w| w.get("package"))
            .and_then(|p| p.get("edition"))
            .and_then(toml_edit::Item::as_str)
        {
            return Some(edition.to_owned());
        }
    }
    None
}

/// Whether the playground would be written outside the package directory.
///
/// Two distinct escapes, both of which have to be caught before any directory
/// is created:
///
/// * a **declared** `[[bin]] path` that climbs out (`../tools/playground.rs`) —
///   caught lexically, since the file need not exist yet;
/// * a **symlinked ancestor**, e.g. a checked-out `src/bin` pointing outside
///   the repository — caught by canonicalizing the deepest ancestor that does
///   exist. The final-component symlink check on `--force` cannot see this:
///   the link is a directory partway up the path, and it bites on
///   `--scaffold-only` too.
fn playground_path_escapes_package(project_dir: &Path, playground_rel: &str) -> bool {
    let root = lexically_normalize(project_dir);
    let intended = lexically_normalize(&project_dir.join(playground_rel.replace('\\', "/")));
    if !intended.starts_with(&root) {
        return true;
    }

    let real_root = root.canonicalize().unwrap_or(root);
    let mut ancestor = intended.parent();
    while let Some(dir) = ancestor {
        if let Ok(real) = dir.canonicalize() {
            return !real.starts_with(&real_root);
        }
        ancestor = dir.parent();
    }
    false
}

/// What makes the playground destination unwritable, if anything.
///
/// `scaffold_outcome` keys off `is_file()`, which is false for a *directory*
/// sitting at the playground path exactly as it is for nothing at all — so the
/// run would treat it as "create" and only discover the problem inside
/// `write_playground`, as a raw I/O error, after `Cargo.toml` had already been
/// committed. Same for a non-directory squatting on an ancestor we would have
/// to create (a file at `src/bin`), which makes `create_dir_all` fail.
///
/// Both leave the feature and the `[[bin]]` entry persisted with no source
/// file behind them, which is precisely the no-write-on-refusal behaviour the
/// docs promise. Detecting them here keeps every rejection ahead of the first
/// byte written.
fn playground_destination_problem(project_dir: &Path, playground_rel: &str) -> Option<String> {
    let root = lexically_normalize(project_dir);
    let path = lexically_normalize(&project_dir.join(playground_rel.replace('\\', "/")));

    // `symlink_metadata` rather than `exists`, so a dangling symlink counts as
    // present: `is_file()` is false for one, and a plain write would silently
    // replace the link instead of the file the user thinks they named.
    if path.symlink_metadata().is_ok() && !path.is_file() {
        return Some(format!(
            "`{playground_rel}` exists but is not a regular file"
        ));
    }

    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        if dir == root || !dir.starts_with(&root) {
            break;
        }
        if dir.symlink_metadata().is_ok() && !dir.is_dir() {
            return Some(format!(
                "`{}` exists but is not a directory, so the playground's parent \
                 directory cannot be created",
                relative_path_from(&root, dir)
            ));
        }
        ancestor = dir.parent();
    }
    None
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
    use std::io::Write as _;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("autumn-console");

    // The scratch file is created with O_EXCL, which fails outright if the
    // path already exists — including when it exists as a *symlink*. A
    // deterministic name opened with a plain write would instead follow such a
    // link and truncate whatever it points at, anywhere on the filesystem, and
    // the rename would then install the link in place of the real file. A
    // checked-out repository can contain any name we might pick, so the
    // exclusive create (not the name) is what makes this safe; the pid and
    // counter only avoid colliding with our own concurrent runs.
    for attempt in 0..MAX_TEMP_FILE_ATTEMPTS {
        let tmp = dir.join(format!(
            ".{stem}.autumn-console-{}-{attempt}.tmp",
            std::process::id()
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };

        let written = file
            .write_all(contents.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not create a temporary file next to {}",
            path.display()
        ),
    ))
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
    let (updated_manifest, changes) = ensure_manifest_wiring(
        &manifest,
        workspace_inherited_edition(&project_dir).as_deref(),
    )
    .unwrap_or_else(|e| fail(&e));

    // EVERY rejection has to happen before the first byte is written. The
    // manifest edit used to land here, ahead of the path checks below, so a
    // project refused for an out-of-package playground was left with a
    // modified `Cargo.toml` while the error said "nothing has been created".
    // Resolve and validate the destination first; only then commit anything.
    //
    // Honour a user-relocated playground: if their manifest already declares
    // the bin somewhere else, scaffold there rather than writing a file Cargo
    // would de-duplicate away and never run.
    let playground_rel =
        declared_playground_path(&doc).unwrap_or_else(|| PLAYGROUND_REL_PATH.to_owned());
    if playground_path_escapes_package(&project_dir, &playground_rel) {
        fail(&ConsoleError::PlaygroundPathEscapesPackage(playground_rel));
    }
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
    // Only a run that will actually write the file needs a usable destination:
    // an existing playground we are keeping is left alone either way.
    let outcome = scaffold_outcome(exists, force);
    if outcome.writes_file()
        && let Some(problem) = playground_destination_problem(&project_dir, &playground_rel)
    {
        fail(&ConsoleError::PlaygroundDestinationUnusable(problem));
    }

    // Validation is complete. Write the manifest before the source file: the
    // playground only ever compiles with `--features playground`, so a manifest
    // that never landed just means `autumn console` cannot build it yet,
    // whereas a source file written against an unsaved manifest is an inert
    // file with no target. Manifest-first degrades in the gentler direction.
    if changes.any() {
        if let Err(e) = write_atomic(&manifest_path, &updated_manifest) {
            fail_io(&manifest_path, &e);
        }
        report_manifest_edits(changes);
    }

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
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST, None).unwrap();
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
        let (out, changes) = ensure_manifest_wiring(PLAIN_MANIFEST, None).unwrap();
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
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
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
        let (out, changes) = ensure_manifest_wiring(manifest, None).unwrap();
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
        let (out, changes) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(!changes.added_feature);
        assert!(
            out.contains("\"autumn-web/redis\""),
            "a user-tuned playground feature must not be overwritten:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_is_idempotent() {
        let (once, _) = ensure_manifest_wiring(PLAIN_MANIFEST, None).unwrap();
        let (twice, changes) = ensure_manifest_wiring(&once, None).unwrap();
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
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
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
        let (out, changes) = ensure_manifest_wiring(manifest, None).unwrap();
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
        let (out, changes) = ensure_manifest_wiring(manifest, None).unwrap();
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
            ensure_manifest_wiring(manifest, None),
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
        let (out, changes) = ensure_manifest_wiring(manifest, None).unwrap();
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
            ensure_manifest_wiring(manifest, None),
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
            ensure_manifest_wiring(manifest, None),
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
        assert!(ensure_manifest_wiring(manifest, None).is_ok());
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
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::PlaygroundBinNotGated)
        );
    }

    // ── isolation guards, round 3 ─────────────────────────────────────────

    #[test]
    fn edition_2015_lib_is_not_proof_that_bin_discovery_is_off() {
        // Verified against cargo: with `[lib]` and no `[[bin]]`, an
        // edition-2015 package still auto-discovers src/main.rs and
        // src/bin/*.rs. Appending our entry turned targets
        // [g-app, g_app, worker] into [g_app, playground].
        let manifest = r#"[package]
name = "legacy-app"
version = "0.1.0"
edition = "2015"

[lib]
name = "legacy_app"

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::CannotIsolateOnEdition2015),
            "a [lib] says nothing about bin auto-discovery"
        );
    }

    #[test]
    fn edition_2015_with_an_existing_bin_entry_may_add_ours() {
        let manifest = r#"[package]
name = "legacy-app"
version = "0.1.0"
edition = "2015"

[[bin]]
name = "worker"
path = "src/bin/worker.rs"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (_, changes) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(
            changes.added_bin,
            "bin discovery is already off, so nothing more can be lost"
        );
    }

    #[test]
    fn inherited_edition_is_resolved_before_applying_the_2015_rule() {
        // `edition.workspace = true` yields None from as_str(); reading that as
        // "2015" would refuse a modern workspace member for a hazard it has no.
        let manifest = r#"[package]
name = "member"
version = "0.1.0"
edition.workspace = true

[dependencies]
autumn-web = "0.6.0"
"#;
        let (_, changes) = ensure_manifest_wiring(manifest, Some("2024")).unwrap();
        assert!(changes.added_bin, "an inherited 2024 edition is not legacy");

        assert_eq!(
            ensure_manifest_wiring(manifest, Some("2015")),
            Err(ConsoleError::CannotIsolateOnEdition2015),
            "an inherited 2015 edition still gets the legacy treatment"
        );
    }

    #[test]
    fn playground_feature_preserves_an_implicit_optional_dependency() {
        // Cargo synthesises `playground = ["dep:playground"]` for an optional
        // dependency of that name. Declaring the key without the edge makes
        // cargo reject the manifest: "optional dependency `playground` is not
        // included in any feature" — verified against cargo.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
playground = { version = "0.1", optional = true }
"#;
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(
            out.contains("\"dep:playground\""),
            "the implicit optional-dependency edge must survive:\n{out}"
        );
        assert!(
            out.contains("\"autumn-web/seed\""),
            "and the seed edge must still be added:\n{out}"
        );
    }

    #[test]
    fn playground_feature_preserves_a_target_specific_optional_dependency() {
        // Cargo synthesises the implicit feature for optional dependencies in
        // `[target.'cfg(...)'.dependencies]` too — verified against cargo,
        // which reports `{'playground': ['dep:playground']}` for this manifest.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"

[target.'cfg(unix)'.dependencies]
playground = { version = "0.1", optional = true }
"#;
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(
            out.contains("\"dep:playground\""),
            "a target-specific optional dependency has the same implicit \
             feature and must be preserved:\n{out}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_rejects_an_unusable_bin_key() {
        // A root-level `bin = []` is valid TOML that cargo accepts, but it is
        // not an array-of-tables, so the gated entry can never be appended and
        // the scaffolded file would be auto-discovered UNGATED.
        let manifest = r#"bin = []

[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::UnusableBinTable)
        );
    }

    #[test]
    fn playground_feature_does_not_invent_a_dep_edge_for_a_required_dependency() {
        // Only an *optional* dependency gets an implicit feature.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
playground = "0.1"
"#;
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(
            !out.contains("dep:playground"),
            "a required dependency has no implicit feature to preserve:\n{out}"
        );
    }

    #[test]
    fn playground_feature_preserves_an_inline_table_target_dependency() {
        // The same optional dependency, written in Cargo's inline-table form.
        // Two earlier versions of this scan enumerated representations and each
        // missed one, so the lookup is now shape-agnostic (`as_table_like`).
        let manifest = r#"target = { 'cfg(unix)' = { dependencies = { playground = { version = "0.1", optional = true } } } }

[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(
            out.contains("\"dep:playground\""),
            "an inline-table target dependency must be seen too:\n{out}"
        );
    }

    #[test]
    fn playground_feature_preserves_an_inline_top_level_dependency_table() {
        let manifest = r#"dependencies = { autumn-web = "0.6.0", playground = { version = "0.1", optional = true } }

[package]
name = "my-app"
version = "0.1.0"
edition = "2024"
"#;
        let (out, _) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(
            out.contains("\"dep:playground\""),
            "an inline `dependencies` table must be seen too:\n{out}"
        );
    }

    // ── symlink-resolved module paths ─────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn app_module_decls_resolve_a_symlinked_playground_directory() {
        // `custom -> tools/bin`, both inside the package. Containment holds, so
        // this is not refused — but the kernel applies `..` after following the
        // link, so a lexically-measured `../src/schema.rs` would land in
        // `tools/src/`. Verified against the filesystem below.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tools/bin")).unwrap();
        std::fs::write(root.join("src/schema.rs"), "// schema\n").unwrap();
        std::os::unix::fs::symlink("tools/bin", root.join("custom")).unwrap();

        let decls = app_module_decls(root, "custom/playground.rs");
        let emitted = decls
            .lines()
            .find_map(|l| l.strip_prefix("#[path = \"")?.strip_suffix("\"]"))
            .expect("a schema declaration");

        assert!(
            root.join("custom").join(emitted).exists(),
            "the emitted path must resolve to a real file when joined onto the \
             playground's directory as rustc sees it; got {emitted}"
        );
    }

    #[test]
    fn ensure_manifest_wiring_rejects_a_strong_dependency_feature_edge_to_playground() {
        // `default = ["playground/std"]` is a STRONG edge: it activates the
        // optional dependency `playground`, and with it the implicit package
        // feature of the same name — so the gate is on in every normal build.
        // Verified against cargo:
        //   {'default': ['playground/std'], 'playground': ['dep:playground']}
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
playground = { version = "0.1", optional = true }

[features]
default = ["playground/std"]
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::PlaygroundFeatureIsDefault(
                "default".to_owned()
            ))
        );
    }

    #[test]
    fn strong_edge_to_a_required_dependency_does_not_activate_a_local_feature() {
        // The mirror of the case above, and the reason the strong-edge rule is
        // conditional rather than blanket: a strong edge activates the package
        // feature named after the dependency only when that dependency is
        // OPTIONAL, because what it really activates is the implicit
        // `playground = ["dep:playground"]` feature Cargo synthesises for one.
        // Against a required dependency no such feature exists, and a
        // same-named feature the package declares itself stays off — so this
        // project is properly isolated and must be accepted.
        //
        // Verified against cargo with a deliberately broken target gated on
        // `required-features = ["playground"]`: the build succeeds, i.e. the
        // target was skipped, i.e. the local feature was never activated.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
playground = { version = "0.1" }

[features]
default = ["playground/std"]
playground = []
"#;
        let (updated, changes) = ensure_manifest_wiring(manifest, None).expect("accepted");
        assert!(changes.added_bin, "the gated bin target should be added");
        assert!(updated.contains("required-features = [\"playground\"]"));
    }

    #[test]
    fn ensure_manifest_wiring_allows_a_weak_dependency_feature_edge() {
        // `playground?/std` is WEAK: it adds `std` only if `playground` is
        // already active, so it never activates the gate by itself. Treating
        // both forms alike would refuse a project that is perfectly safe.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"
playground = { version = "0.1", optional = true }

[features]
default = ["playground?/std"]
"#;
        assert!(
            ensure_manifest_wiring(manifest, None).is_ok(),
            "a weak edge must not be read as activating the gate"
        );
    }

    #[test]
    fn ensure_manifest_wiring_ignores_an_unrelated_strong_edge() {
        // `autumn-web/mail` names a required dependency, so there is no
        // implicit `autumn-web` package feature and nothing to follow.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6.0"

[features]
default = ["autumn-web/mail"]
"#;
        assert!(ensure_manifest_wiring(manifest, None).is_ok());
    }

    #[test]
    fn an_inline_root_features_table_is_usable() {
        // `features = { playground = [] }` at the document root is a valid
        // manifest — cargo reports `features = {'playground': []}` for it — but
        // toml_edit exposes it as an InlineTable, so an `as_table_mut()` check
        // rejected EVERY console invocation on such a project. Same class of
        // mistake as the dependency scan: enumerating representations instead
        // of going through `as_table_like`.
        //
        // NOTE the key sits above the first table header. Placed after
        // `[package]` it would nest inside it and be inert — which is how a
        // first attempt at this repro accidentally passed.
        let manifest = r#"features = { playground = [] }

[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[dependencies]
autumn-web = "0.6.0"
"#;
        let (updated, changes) = ensure_manifest_wiring(manifest, None).expect("accepted");
        assert!(changes.added_feature, "the seed edge should be merged in");
        assert!(
            updated.contains("autumn-web/seed"),
            "inline table should be edited in place: {updated}"
        );
    }

    #[test]
    fn an_inline_root_features_table_is_still_scanned_for_a_default_gate() {
        // The read paths matter as much as the write: with an inline table the
        // default-enables-playground guard used to see no features at all and
        // wave the project through.
        let manifest = r#"features = { default = ["playground"], playground = [] }

[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[dependencies]
autumn-web = "0.6.0"
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::PlaygroundFeatureIsDefault(
                "default".to_owned()
            ))
        );
    }

    #[test]
    fn a_gate_naming_an_undeclared_default_feature_is_rejected() {
        // `default` is not implicitly present. Verified against cargo:
        //   warning: invalid feature `default` in required-features of target
        //            `playground`: `default` is not present in [features]
        //   error: target `playground` requires the features: `playground`, `default`
        // so the target can never run and the gate must be refused.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[dependencies]
autumn-web = "0.6.0"

[features]
playground = []

[[bin]]
name = "playground"
path = "src/bin/playground.rs"
required-features = ["playground", "default"]
"#;
        assert_eq!(
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::PlaygroundBinGateUnsatisfiable(
                "default".to_owned()
            ))
        );
    }

    #[test]
    fn a_gate_naming_a_declared_default_feature_is_accepted() {
        // The accept case the fix must not swallow: when the package really
        // does declare `default`, the runner activates it (no
        // `--no-default-features` is ever passed), so the gate is satisfiable.
        let manifest = r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[dependencies]
autumn-web = "0.6.0"

[features]
default = []
playground = []

[[bin]]
name = "playground"
path = "src/bin/playground.rs"
required-features = ["playground", "default"]
"#;
        assert!(
            ensure_manifest_wiring(manifest, None).is_ok(),
            "a declared `default` is activated by the runner and must be accepted"
        );
    }

    // ── path containment ──────────────────────────────────────────────────

    #[test]
    fn playground_path_inside_the_package_is_accepted() {
        let tmp = project_with(&["src/main.rs"]);
        assert!(!playground_path_escapes_package(
            tmp.path(),
            PLAYGROUND_REL_PATH
        ));
        assert!(!playground_path_escapes_package(
            tmp.path(),
            "custom/playground.rs"
        ));
    }

    #[test]
    fn a_usable_destination_reports_no_problem() {
        // The guard below must not fire on the ordinary cases: nothing there
        // yet, or an existing regular file we are about to regenerate.
        let tmp = project_with(&["src/main.rs"]);
        assert_eq!(
            playground_destination_problem(tmp.path(), PLAYGROUND_REL_PATH),
            None
        );
        std::fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        std::fs::write(tmp.path().join(PLAYGROUND_REL_PATH), "fn main() {}").unwrap();
        assert_eq!(
            playground_destination_problem(tmp.path(), PLAYGROUND_REL_PATH),
            None
        );
    }

    #[test]
    fn a_directory_at_the_playground_path_is_reported() {
        // `is_file()` is false for a directory exactly as it is for nothing at
        // all, so without this guard the run reads it as "create", commits the
        // manifest, and only then fails with `Is a directory (os error 21)` —
        // leaving the feature and `[[bin]]` entry behind with no source file.
        let tmp = project_with(&["src/main.rs"]);
        std::fs::create_dir_all(tmp.path().join(PLAYGROUND_REL_PATH)).unwrap();
        let problem = playground_destination_problem(tmp.path(), PLAYGROUND_REL_PATH)
            .expect("a directory at the destination is a problem");
        assert!(problem.contains("not a regular file"), "{problem}");
    }

    #[test]
    fn a_file_where_the_parent_directory_belongs_is_reported() {
        // The other half: `create_dir_all` fails with `File exists` when a
        // non-directory squats on an ancestor.
        let tmp = project_with(&["src/main.rs"]);
        std::fs::write(tmp.path().join("src/bin"), "not a directory").unwrap();
        let problem = playground_destination_problem(tmp.path(), PLAYGROUND_REL_PATH)
            .expect("a file at src/bin is a problem");
        assert!(problem.contains("not a directory"), "{problem}");
        assert!(problem.contains("src/bin"), "{problem}");
    }

    #[test]
    fn playground_path_climbing_out_of_the_package_is_rejected() {
        let tmp = project_with(&["src/main.rs"]);
        assert!(playground_path_escapes_package(
            tmp.path(),
            "../tools/playground.rs"
        ));
        assert!(playground_path_escapes_package(
            tmp.path(),
            "src/../../elsewhere/playground.rs"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn playground_path_through_a_symlinked_ancestor_is_rejected() {
        // A checked-out `src/bin -> /elsewhere` writes outside the repository
        // even though the declared path is the perfectly ordinary default.
        // Only canonicalizing an existing ancestor catches this.
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("app");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("src/bin")).unwrap();

        assert!(
            playground_path_escapes_package(&project, PLAYGROUND_REL_PATH),
            "a symlinked ancestor directory must be treated as an escape"
        );
    }

    // ── write_atomic ──────────────────────────────────────────────────────

    #[test]
    fn write_atomic_does_not_write_through_a_symlinked_scratch_file() {
        // A checked-out repo can contain any name we might pick for the temp
        // file. Opening a deterministic path with a plain write would follow
        // such a link and truncate its target anywhere on disk.
        let tmp = TempDir::new().unwrap();
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, "SECRET").unwrap();

        let target = tmp.path().join("Cargo.toml");
        std::fs::write(&target, "original").unwrap();

        // Pre-plant links across the whole space of names we might choose.
        for attempt in 0..MAX_TEMP_FILE_ATTEMPTS {
            let decoy = tmp.path().join(format!(
                ".Cargo.toml.autumn-console-{}-{attempt}.tmp",
                std::process::id()
            ));
            #[cfg(unix)]
            std::os::unix::fs::symlink(&victim, &decoy).unwrap();
            #[cfg(not(unix))]
            std::fs::write(&decoy, "decoy").unwrap();
        }

        assert!(
            write_atomic(&target, "new contents").is_err(),
            "every candidate name is taken, so the write must fail rather than \
             follow a link"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "SECRET",
            "the symlink target must never be written through"
        );
    }

    #[test]
    fn write_atomic_replaces_the_target_and_cleans_up() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("Cargo.toml");
        std::fs::write(&target, "old").unwrap();

        write_atomic(&target, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("autumn-console"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files must not linger: {leftovers:?}"
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
            ensure_manifest_wiring(manifest, None),
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
        assert!(ensure_manifest_wiring(manifest, None).is_ok());
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
        assert!(ensure_manifest_wiring(manifest, None).is_ok());
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
            ensure_manifest_wiring(manifest, None),
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
                ensure_manifest_wiring(manifest, None).is_err(),
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
        let (out, changes) = ensure_manifest_wiring(manifest, None).unwrap();
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
            ensure_manifest_wiring(manifest, None),
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
        let (_, changes) = ensure_manifest_wiring(manifest, None).unwrap();
        assert!(changes.added_bin);
    }

    #[test]
    fn modern_editions_always_accept_the_bin_entry() {
        for edition in ["2018", "2021", "2024"] {
            let manifest = format!(
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n\
                 [dependencies]\nautumn-web = \"0.6.0\"\n"
            );
            let (_, changes) = ensure_manifest_wiring(&manifest, None).unwrap();
            assert!(changes.added_bin, "edition {edition} keeps autobins on");
        }
    }

    // ── failure modes ──────────────────────────────────────────────────────

    #[test]
    fn ensure_manifest_wiring_errors_without_an_autumn_web_dependency() {
        let manifest = "[package]\nname = \"my-app\"\n\n[dependencies]\nserde = \"1\"\n";
        assert_eq!(
            ensure_manifest_wiring(manifest, None),
            Err(ConsoleError::MissingAutumnWebDependency)
        );
    }

    #[test]
    fn ensure_manifest_wiring_errors_on_an_unparsable_manifest() {
        let err = ensure_manifest_wiring("this is not = = toml", None).unwrap_err();
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
            ensure_manifest_wiring(manifest, None),
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
