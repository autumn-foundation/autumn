//! Install planning for `autumn plugin add` — pure functions plus one
//! [`crate::generate::emit::Plan`] builder, so `--dry-run` and the Created/Modified output match
//! every other code-touching Autumn command.
//!
//! Every decision that can refuse the install (version gate, missing project,
//! unreadable builder chain) is made *before* a single
//! [`crate::generate::emit::Action`] is
//! queued, which is what makes issue #1606's "fails before any file is
//! modified" and "never leaves the app in a non-compiling state" true by
//! construction rather than by careful ordering.

use std::path::{Path, PathBuf};

use crate::generate::emit::Plan;

use super::catalog::CatalogEntry;

/// Why an install could not proceed.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The working directory is not an Autumn project root.
    #[error("not inside an Autumn project (no Cargo.toml found in current directory)")]
    NotInProject,

    /// The project's `Cargo.toml` does not depend on `autumn-web`.
    #[error(
        "this project does not depend on `autumn-web`, so there is no plugin-compatible version to resolve"
    )]
    NoAutumnWeb,

    /// The project's `Cargo.toml` has no `[package]` table: it is a virtual
    /// workspace root, which cannot own a `[dependencies]` section.
    #[error(
        "this Cargo.toml has no `[package]` table — it looks like a virtual workspace root, which cannot own dependencies, so there is nothing to install into. Run `autumn plugin add` inside one of the workspace's member crates instead — no files were changed."
    )]
    NoPackageTable,

    /// The plugin's supported `autumn-web` range excludes the app's version.
    #[error(
        "`{crate_name} {plugin_version}` supports autumn-web {supported}, but this app uses autumn-web {app_version} — no files were modified.\nUpgrade the app with `autumn upgrade`, or install a `{crate_name}` release built for autumn-web {app_version}."
    )]
    Incompatible {
        /// The plugin crate being installed.
        crate_name: String,
        /// The plugin version that would have been installed.
        plugin_version: String,
        /// The `autumn-web` range that plugin version supports.
        supported: String,
        /// The `autumn-web` version this app declares.
        app_version: String,
    },

    /// The name is neither a first-party plugin nor a `autumn-plugin-` crate.
    #[error(
        "unknown plugin `{0}` — run `autumn plugin list` to see installable plugins. Community plugins follow the `autumn-plugin-<name>` convention documented in docs/plugins.md."
    )]
    UnknownPlugin(String),

    /// The app already declares this plugin at an incompatible version.
    #[error(
        "`{crate_name}` is already declared as `{declared}`, which is not compatible with the `{installing}` this CLI installs — no files were changed.\nUpdate that line to `{crate_name} = \"{installing}\"` (or run `autumn upgrade`) and try again: leaving it would mount a {declared}-series plugin into a {installing}-series builder, which does not compile."
    )]
    DependencyVersionMismatch {
        /// The plugin crate.
        crate_name: String,
        /// The requirement the manifest already carries.
        declared: String,
        /// The version this CLI would install.
        installing: String,
    },

    /// `autumn-web` comes from a path/git checkout that no `[patch.crates-io]`
    /// redirects, so a registry plugin would link a *second* framework copy.
    #[error(
        "this app depends on `autumn-web` from a local path or git checkout with no `[patch.crates-io]` entry redirecting it — no files were changed.\nA plugin installed from crates.io would pull its own copy of `autumn-web`, so Cargo would build two different framework crates and the mount would not satisfy the local `AppBuilder`'s traits.\nAdd to your workspace Cargo.toml:\n\n    [patch.crates-io]\n    autumn-web = {{ path = \"<your autumn-web checkout>\" }}\n\nor depend on `{crate_name}` by path from the same checkout."
    )]
    UnpatchedLocalFramework {
        /// The plugin that could not be installed.
        crate_name: String,
    },

    /// The app declares `autumn-web` at different versions (target-specific
    /// tables) and no `Cargo.lock` edge says which one Cargo builds.
    #[error(
        "this app declares `autumn-web` at more than one version ({declared}) and no Cargo.lock resolves which one it builds, so `{crate_name}` cannot be checked against it — no files were changed. Declare one version, or run `cargo generate-lockfile`, then re-run."
    )]
    AmbiguousAutumnWeb {
        /// The plugin that could not be installed.
        crate_name: String,
        /// The declared requirements, comma-separated.
        declared: String,
    },

    /// The app's `autumn-web` requirement admits more than one release
    /// series and no `Cargo.lock` resolves which one it builds.
    #[error(
        "this app's `autumn-web` requirement `{declared}` spans more than one release series and no Cargo.lock resolves which one it builds, so Cargo could build `{crate_name}` against another framework copy — no files were changed. Narrow the requirement to one series, or run `cargo generate-lockfile`, then re-run."
    )]
    UnresolvedAutumnWebRange {
        /// The plugin that could not be installed.
        crate_name: String,
        /// The app's requirement.
        declared: String,
    },

    /// The app takes `autumn-web` from a local checkout whose version cannot
    /// be read, and no `Cargo.lock` resolves it.
    #[error(
        "this app's `autumn-web` comes from a path or git checkout whose version cannot be read, and no Cargo.lock resolves it, so `{crate_name}` cannot be checked against it — no files were changed. Run `cargo generate-lockfile`, then re-run."
    )]
    UnknownAutumnWebVersion {
        /// The plugin that could not be installed.
        crate_name: String,
    },

    /// crates.io returned something that is not a usable version string.
    #[error(
        "crates.io reported version `{version}` for `{crate_name}`, which is not a usable version requirement — no files were changed"
    )]
    ImplausibleVersion {
        /// The crate whose version could not be used.
        crate_name: String,
        /// The rejected version string.
        version: String,
    },

    /// Filesystem error while reading the project.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// How the app declares `autumn-web`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAutumnWeb {
    /// A readable version requirement.
    Version(String),
    /// Declared through a `path`/`git`/workspace entry with no version here.
    Unversioned,
}

/// The verdict of the version gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    /// The plugin supports the app's `autumn-web` version.
    Compatible,
    /// The plugin does not support the app's `autumn-web` version.
    Incompatible,
    /// One of the two versions could not be parsed.
    Unknown,
}

/// What `plugin add` decided to do.
#[derive(Debug)]
pub enum AddOutcome {
    /// The install can be applied.
    Installed {
        /// Filesystem actions to execute.
        plan: Box<Plan>,
        /// Post-install steps to print.
        steps: Vec<String>,
    },
    /// Dependency and mount are both already present.
    AlreadyInstalled,
    /// The dependency was added but the mount was left to the user — a
    /// community crate, whose `<Name>Plugin` the CLI can derive from the
    /// naming convention but cannot verify.
    DependencyOnly {
        /// Filesystem actions to execute (the manifest edit).
        plan: Box<Plan>,
        /// Whether this run actually added the dependency. `false` on a
        /// re-run, where the mount snippet still needs showing.
        dependency_added: bool,
        /// The `[dependencies]` line that was added.
        dependency_line: String,
        /// The convention-derived builder-chain snippet to paste.
        mount_snippet: String,
    },
    /// Nothing was changed; the user applies the printed lines by hand.
    Manual {
        /// Why the automatic edit was declined.
        reason: String,
        /// The `[dependencies]` line to add.
        dependency_line: String,
        /// The builder-chain snippet to paste.
        mount_snippet: String,
        /// Post-install steps to print.
        steps: Vec<String>,
    },
}

/// Compare the app's `autumn-web` requirement against the version of a plugin
/// release.
///
/// First-party plugins are published in lockstep with `autumn-web`, so the
/// plugin's version *is* the `autumn-web` version it was built against. The
/// comparison follows Cargo's compatibility rule, which is also
/// `STABILITY.md`'s pre-1.0 contract: below 1.0 every minor bump is breaking,
/// so the minor has to match; from 1.0 on only the major does.
#[must_use]
pub fn check_compat(app_version: &str, plugin_supports: &str) -> Compat {
    let (Some(app), Some(plugin)) = (parse_version(app_version), parse_version(plugin_supports))
    else {
        return Compat::Unknown;
    };
    let compatible = if app.0 == 0 || plugin.0 == 0 {
        app.0 == plugin.0 && app.1 == plugin.1
    } else {
        app.0 == plugin.0
    };
    if compatible {
        Compat::Compatible
    } else {
        Compat::Incompatible
    }
}

/// The `autumn-web` range a first-party plugin at `version` supports, as it is
/// printed in the incompatibility diagnostic: `0.7` below 1.0 (where the minor
/// is part of the compatibility key), `2` from 1.0 on.
#[must_use]
pub fn supported_range(version: &str) -> String {
    match parse_version(version) {
        Some((0, minor, _)) => format!("0.{minor}"),
        Some((major, _, _)) => major.to_string(),
        None => version.to_owned(),
    }
}

/// Parse `MAJOR.MINOR[.PATCH]` from a Cargo version requirement, or `None`
/// when the requirement does not pin one.
///
/// `=`, `^` and `~` are stripped: all three name a single version whose
/// compatibility is the version's own. Comparison and range requirements
/// (`>=0.6`, `>=0.7, <0.9`, `*`, `0.6 || 0.7`) are deliberately **not**
/// parsed — stripping their operator would read `>=0.6` as *exactly* 0.6 and
/// refuse an install into an app that resolves to 0.7 perfectly well. `None`
/// becomes [`Compat::Unknown`], which lets the install proceed rather than
/// refusing on a version this code cannot actually evaluate.
///
/// (`doctor::check_version_compat` strips operators instead. It is comparing
/// two *concrete* versions for a diagnostic, where a false warning costs
/// nothing; here a false negative blocks a command outright.)
pub fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.trim();
    if version.contains(['<', '>', ',', '*', '|']) {
        return None;
    }
    let version = version.trim_start_matches(['=', '^', '~', ' ']);
    let mut parts = version.split('.');
    let major: u64 = parts.next()?.trim().parse().ok()?;
    let minor: u64 = parts.next()?.trim().parse().ok()?;
    let patch: u64 = parts.next().map_or(0, |p| {
        p.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    });
    Some((major, minor, patch))
}

/// Whether `version` is safe to write into a `Cargo.toml` dependency line.
///
/// Belt-and-braces on the crates.io response: a version string is written
/// verbatim into the manifest, so anything that is not plausibly a semver
/// version is refused rather than emitted into a file the user then has to
/// repair.
#[must_use]
pub fn is_plausible_version(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Read how the project at `root` declares `autumn-web`.
///
/// # Errors
///
/// [`PluginError::NotInProject`] when there is no `Cargo.toml`, and
/// [`PluginError::NoAutumnWeb`] when it does not mention `autumn-web`.
pub fn app_autumn_web(root: &Path) -> Result<AppAutumnWeb, PluginError> {
    if !manifest_path(root).is_file() {
        return Err(PluginError::NotInProject);
    }
    let entries = package_autumn_web_entries(root);
    if entries.is_empty() {
        return Err(PluginError::NoAutumnWeb);
    }
    Ok(declared_autumn_web_versions(root)
        .into_iter()
        .next()
        .map_or(AppAutumnWeb::Unversioned, AppAutumnWeb::Version))
}

/// Every dependency table Cargo reads for a package manifest: regular, dev
/// and build, and each of those under every `[target.'cfg(…)']`.
fn dependency_tables(table: &toml::Table) -> impl Iterator<Item = &toml::Table> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let targets = table
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(toml::Table::values);
    KINDS
        .iter()
        .filter_map(|kind| table.get(*kind))
        .chain(targets.flat_map(|target| KINDS.iter().filter_map(|kind| target.get(*kind))))
        .filter_map(toml::Value::as_table)
}

/// The package's own `autumn-web` dependency entries: every dependency table
/// Cargo reads for the package at `root` (regular, dev, build and
/// target-specific), matched by key or by a `package` rename, with
/// `{ workspace = true }` entries resolved from the workspace root. A
/// `[workspace.dependencies]` default the package does not inherit is not
/// its entry. An inherited `autumn-web` the workspace does not define comes
/// back as an empty table: declared, but unresolved. The flag says whether
/// the entry was inherited: its relative `path` then resolves from the
/// workspace root, not the package.
fn package_autumn_web_entries(root: &Path) -> Vec<(toml::Value, bool)> {
    let read = |path: &Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    };
    let Some(table) = read(&manifest_path(root)) else {
        return Vec::new();
    };
    let workspace_deps = read(&workspace_root(root).join("Cargo.toml"))
        .and_then(|table| table.get("workspace")?.get("dependencies").cloned());
    let mut entries = Vec::new();
    for (key, entry) in dependency_tables(&table).flat_map(|deps| deps.iter()) {
        let inherited = entry.get("workspace").and_then(toml::Value::as_bool) == Some(true);
        let entry = if inherited {
            match workspace_deps.as_ref().and_then(|deps| deps.get(key)) {
                Some(from_root) => from_root.clone(),
                None if key == "autumn-web" => toml::Value::Table(toml::Table::new()),
                None => continue,
            }
        } else {
            entry.clone()
        };
        if key == "autumn-web"
            || entry.get("package").and_then(toml::Value::as_str) == Some("autumn-web")
        {
            entries.push((entry, inherited));
        }
    }
    entries
}

/// Whether the package declares `autumn-web` at least once with no version
/// (a path or git entry) next to a versioned declaration. Which one a build
/// takes depends on the target, so neither is the app's version by itself.
#[must_use]
pub fn mixed_autumn_web_declarations(root: &Path) -> bool {
    let entries = package_autumn_web_entries(root);
    let versioned = |entry: &toml::Value| {
        entry.is_str() || entry.get("version").and_then(toml::Value::as_str).is_some()
    };
    entries.iter().any(|(entry, _)| versioned(entry))
        && entries.iter().any(|(entry, _)| !versioned(entry))
}

/// The version of the local `autumn-web` checkout the package depends on by
/// path: its own manifest's `[package] version`, resolved from the package
/// or, for an inherited entry, the workspace root. `None` for a git
/// checkout, or when it cannot be read.
fn local_framework_version(root: &Path) -> Option<String> {
    let root_abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    package_autumn_web_entries(root)
        .iter()
        .find_map(|(entry, inherited)| {
            let path = entry.get("path")?.as_str()?;
            let base = if *inherited {
                workspace_root(&root_abs)
            } else {
                root_abs.clone()
            };
            package_version(&base.join(path))
        })
        .map(|version| version.to_string())
}

/// Whether any dependency table (regular, dev, build or target-specific)
/// declares `crate_name` from a `path`, `git` or alternate registry, a
/// `{ workspace = true }` entry resolved from the workspace root. Cargo
/// refuses one crate from two sources, so a crates.io entry cannot join it.
#[must_use]
pub fn alternate_source_anywhere(root: &Path, manifest: &str, crate_name: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(manifest) else {
        return false;
    };
    let workspace_deps = std::fs::read_to_string(workspace_root(root).join("Cargo.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .and_then(|table| table.get("workspace")?.get("dependencies").cloned());
    let want = canonical(crate_name);
    dependency_tables(&table)
        .flat_map(|deps| deps.iter())
        .filter(|(key, _)| canonical(key) == want)
        .any(|(key, entry)| {
            let entry = if entry.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                workspace_deps
                    .as_ref()
                    .and_then(|deps| deps.get(key.as_str()))
            } else {
                Some(entry)
            };
            entry.is_some_and(|entry| {
                ["path", "git", "registry", "registry-index"]
                    .iter()
                    .any(|source| entry.get(*source).is_some())
            })
        })
}

/// Every distinct `autumn-web` requirement the package at `root` declares:
/// its own, dev, build and target-specific tables, with `{ workspace = true }`
/// entries resolved. A `[workspace.dependencies]` default the package does not
/// inherit is not its declaration. Several means Cargo's choice depends on
/// the target.
#[must_use]
pub fn declared_autumn_web_versions(root: &Path) -> Vec<String> {
    let mut versions: Vec<String> = Vec::new();
    for (entry, _) in package_autumn_web_entries(root) {
        let version = match entry {
            toml::Value::String(version) => Some(version),
            toml::Value::Table(fields) => fields
                .get("version")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
        if let Some(version) = version.map(|v| v.trim().to_owned())
            && !versions.contains(&version)
        {
            versions.push(version);
        }
    }
    versions
}

/// `version` as an exact Cargo requirement: `=x.y.z`.
#[must_use]
pub fn exact_pin(version: &str) -> String {
    if version.starts_with('=') {
        version.to_owned()
    } else {
        format!("={version}")
    }
}

/// The `[dependencies]` line `plugin add` writes (and prints).
#[must_use]
pub fn dependency_line(crate_name: &str, version: &str) -> String {
    format!("{crate_name} = \"{version}\"")
}

/// Whether `manifest` already declares `crate_name` as an unconditional
/// `[dependencies]` entry.
///
/// Parsed rather than substring-matched: a commented-out line or a mention in
/// a `description` must not make an install look done.
///
/// `[dependencies]` **only** — not `[dev-dependencies]`, not
/// `[build-dependencies]`, and not `[target.'cfg(…)'.dependencies]`. The mount
/// this command writes is unconditional, so anything that does not make the
/// crate available to every application build cannot count as installed:
/// a dev-dependency is invisible to the binary, and a `cfg(windows)`
/// dependency is absent on the Linux build. Counting either would report a
/// complete install for an app that does not compile, *and* would stop the
/// command adding the entry that fixes it.
#[must_use]
pub fn dependency_present(manifest: &str, crate_name: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(manifest) else {
        return false;
    };
    table
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .is_some_and(|deps| deps.contains_key(crate_name))
}

/// The version requirement `manifest` already declares for `crate_name` under
/// `[dependencies]`, if any.
///
/// A path/git entry has no requirement to compare, so it reads as `None`.
#[must_use]
pub fn declared_dependency_version(manifest: &str, crate_name: &str) -> Option<String> {
    let table = toml::from_str::<toml::Table>(manifest).ok()?;
    let entry = table.get("dependencies")?.as_table()?.get(crate_name)?;
    match entry {
        toml::Value::String(version) => Some(version.clone()),
        toml::Value::Table(table) => table
            .get("version")
            .and_then(toml::Value::as_str)
            .map(std::borrow::ToOwned::to_owned),
        _ => None,
    }
}

/// Cargo's workspace root for the package at `dir`: the first directory from
/// `dir` up whose `Cargo.toml` has a `[workspace]` table that does not
/// `exclude` it, or `dir` itself. `dir` is made absolute first: the CLI
/// passes `.`, whose parents are empty.
#[must_use]
pub fn workspace_root(dir: &Path) -> PathBuf {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    // `[package] workspace = "../.."` names the root outright, and Cargo
    // takes it over any nearer `[workspace]`.
    let explicit = std::fs::read_to_string(dir.join("Cargo.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .and_then(|table| {
            table
                .get("package")?
                .get("workspace")?
                .as_str()
                .map(str::to_owned)
        });
    if let Some(explicit) = explicit {
        let root = dir.join(explicit);
        return root.canonicalize().unwrap_or(root);
    }
    dir.ancestors()
        .find(|d| {
            let Some(workspace) = std::fs::read_to_string(d.join("Cargo.toml"))
                .ok()
                .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
                .and_then(|table| table.get("workspace").cloned())
            else {
                return false;
            };
            // An excluded package is its own workspace; Cargo looks further up.
            // An explicit `members` entry wins over `exclude`, as in Cargo's
            // `is_excluded`: both compare as literal path prefixes, so a
            // member glob (`crates/*`) does not override an exclusion.
            let relative = dir.strip_prefix(d).unwrap_or(&dir);
            let listed = |key: &str| {
                workspace
                    .get(key)
                    .and_then(toml::Value::as_array)
                    .is_some_and(|paths| {
                        paths.iter().filter_map(toml::Value::as_str).any(|path| {
                            relative.starts_with(Path::new(path.trim_start_matches("./")))
                        })
                    })
            };
            !listed("exclude") || listed("members")
        })
        .map_or_else(|| dir.clone(), Path::to_path_buf)
}

/// The version of `crate_name` the workspace's `Cargo.lock` resolved for the
/// package at `dir` (Cargo reads the workspace root's lockfile only). `None`
/// when there is no lockfile, the crate is not locked, or it is locked at
/// more than one version.
#[must_use]
pub fn locked_version(dir: &Path, crate_name: &str) -> Option<String> {
    let text = std::fs::read_to_string(workspace_root(dir).join("Cargo.lock")).ok()?;
    let lock = toml::from_str::<toml::Table>(&text).ok()?;
    let want = canonical(crate_name);
    let mut versions = lock
        .get("package")?
        .as_array()?
        .iter()
        .filter(|p| {
            p.get("name")
                .and_then(toml::Value::as_str)
                .is_some_and(|name| canonical(name) == want)
        })
        .filter_map(|p| p.get("version").and_then(toml::Value::as_str));
    let first = versions.next()?.to_owned();
    versions.next().is_none().then_some(first)
}

/// The version of `crate_name` that `package`'s own dependency edge in the
/// workspace `Cargo.lock` resolves to. Unlike [`locked_version`] this is
/// exact when other members lock other versions. `None` when the lock has no
/// single `package` entry, or it does not depend on `crate_name`.
#[must_use]
pub fn locked_dependency_of(dir: &Path, package: &str, crate_name: &str) -> Option<String> {
    let text = std::fs::read_to_string(workspace_root(dir).join("Cargo.lock")).ok()?;
    let lock = toml::from_str::<toml::Table>(&text).ok()?;
    let packages = lock.get("package")?.as_array()?;
    let named = |want: &str| {
        let want = canonical(want);
        packages.iter().filter(move |p| {
            p.get("name")
                .and_then(toml::Value::as_str)
                .is_some_and(|name| canonical(name) == want)
        })
    };
    let mut owners = named(package);
    let owner = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    // An edge is `name`, or `name version` when the lock holds several. Two
    // edges (a renamed alias, target-specific copies) cannot be told apart.
    let mut edges = owner
        .get("dependencies")?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_str)
        .map(str::split_whitespace)
        .filter_map(|mut parts| {
            (canonical(parts.next()?) == canonical(crate_name)).then(|| parts.next())
        });
    let edge = edges.next()?;
    if edges.next().is_some() {
        return None;
    }
    if let Some(version) = edge {
        return Some(version.to_owned());
    }
    let mut versions = named(crate_name).filter_map(|p| p.get("version")?.as_str());
    let first = versions.next()?.to_owned();
    versions.next().is_none().then_some(first)
}

/// The version of `crate_name` Cargo resolved for the package at `dir`, or
/// for `package` when given: its own dependency edge in the workspace lock,
/// else [`locked_version`]. Another member on another version does not make
/// it ambiguous.
#[must_use]
pub fn locked_version_for(dir: &Path, package: Option<&str>, crate_name: &str) -> Option<String> {
    let own = || {
        let text = std::fs::read_to_string(manifest_path(dir)).ok()?;
        let table = toml::from_str::<toml::Table>(&text).ok()?;
        table
            .get("package")?
            .get("name")?
            .as_str()
            .map(str::to_owned)
    };
    package
        .map(str::to_owned)
        .or_else(own)
        .and_then(|package| locked_dependency_of(dir, &package, crate_name))
        .or_else(|| locked_version(dir, crate_name))
}

/// `manifest` with a `crate_name = { workspace = true }` entry replaced by the
/// workspace root's `[workspace.dependencies]` entry, as Cargo resolves it.
/// The root is the first ancestor of `root` (itself included) whose
/// `Cargo.toml` has a `[workspace]` table. Unchanged when the entry is not
/// inherited or cannot be resolved; the pin check then refuses it.
#[must_use]
pub fn with_inherited_dependency(root: &Path, manifest: &str, crate_name: &str) -> String {
    let Ok(mut member) = manifest.parse::<toml_edit::DocumentMut>() else {
        return manifest.to_owned();
    };
    let inherited = member
        .get("dependencies")
        .and_then(|deps| deps.get(crate_name))
        .and_then(|entry| entry.get("workspace"))
        .and_then(toml_edit::Item::as_bool)
        == Some(true);
    if !inherited {
        return manifest.to_owned();
    }
    let workspace_entry = (|| {
        let text = std::fs::read_to_string(workspace_root(root).join("Cargo.toml")).ok()?;
        let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
        doc.get("workspace")?
            .get("dependencies")?
            .get(crate_name)
            .cloned()
    })();
    let Some(entry) = workspace_entry else {
        return manifest.to_owned();
    };
    member["dependencies"][crate_name] = entry;
    member.to_string()
}

/// How `crate_name` is redirected away from its crates.io release by a
/// `[patch]` or `[replace]` table. `None` when nothing redirects it.
///
/// Cargo reads these from the workspace root manifest only (a member's own
/// table is ignored), and `[patch]` also from every `.cargo/config.toml`
/// from `root` up, and from `$CARGO_HOME`. Only a patch for the crates.io
/// source counts: `[patch."<git url>"]` overrides that source alone.
/// A `[source.crates-io] replace-with` in a config counts too: Cargo then
/// takes every crates.io crate from the named source.
#[must_use]
pub fn patched_by(root: &Path, crate_name: &str, version: &str) -> Option<String> {
    patched_by_in(root, crate_name, version, cargo_home().as_deref())
        .or_else(|| env_source_redirect(std::env::vars_os()))
}

/// A `CARGO_SOURCE_CRATES_IO_*` variable: Cargo maps it to the
/// `[source.crates-io]` config key it names (`replace-with`, `registry`, …),
/// so it redirects crates.io as a config file would.
///
/// It reads the environment as `OsString`s: `std::env::vars` panics on any
/// variable that is not UTF-8, however unrelated. A value that is not UTF-8
/// still counts as set.
fn env_source_redirect(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Option<String> {
    vars.filter_map(|(key, value)| {
        let key = key.to_str()?;
        let set = value
            .to_str()
            .map_or(!value.is_empty(), |value| !value.trim().is_empty());
        (key.starts_with("CARGO_SOURCE_CRATES_IO_") && set).then(|| key.to_owned())
    })
    .map(|key| format!("the {key} environment variable"))
    .next()
}

/// `$CARGO_HOME`, or `~/.cargo`.
fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".cargo")))
}

/// The Cargo config files that apply at `root`, each with the directory its
/// relative paths resolve from: `.cargo/config` (Cargo reads it over
/// `config.toml` when both exist) in `root` and every ancestor, then
/// `$CARGO_HOME`'s. A path resolves from the directory holding `.cargo`.
fn config_files(root: &Path, cargo_home: Option<&Path>) -> Vec<(PathBuf, PathBuf)> {
    root.ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(cargo_home.map(Path::to_path_buf))
        .map(|dir| {
            let legacy = dir.join("config");
            let path = if legacy.is_file() {
                legacy
            } else {
                dir.join("config.toml")
            };
            let base = dir.parent().map_or_else(|| dir.clone(), Path::to_path_buf);
            (path, base)
        })
        .collect()
}

/// `path` without `.` and `..` segments: the real path when it exists, else
/// a lexical normalization, so two spellings of one checkout compare equal.
fn normalized(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Where a framework dependency or patch takes `autumn-web` from: a path
/// checkout (normalized), or a git repository and the one ref it names.
#[derive(Debug, PartialEq, Eq)]
enum FrameworkSource {
    Path(PathBuf),
    Git {
        url: String,
        reference: Option<(String, String)>,
    },
    /// An alternate registry, by name (`registry`) or index (`registry-index`),
    /// and the version requirement the entry names.
    Registry {
        registry: String,
        version: Option<String>,
    },
}

/// The alternate-registry source of a dependency entry, if it names one.
fn registry_source(entry: &toml::Value) -> Option<FrameworkSource> {
    entry
        .get("registry")
        .or_else(|| entry.get("registry-index"))
        .and_then(toml::Value::as_str)
        .map(|registry| FrameworkSource::Registry {
            registry: registry.trim_end_matches('/').to_owned(),
            version: entry
                .get("version")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
        })
}

/// Whether a registry `patch` unifies the app's registry `local` entry: the
/// same registry, and a patch that supplies the version the app resolves.
/// Cargo skips a patch whose version does not fit, and takes the newest
/// version a requirement admits, so:
///
/// - with a locked version (`locked`) the app's requirement admits, the patch
///   must admit that version;
/// - without one, the patch must admit exactly the versions the app's
///   requirement does, so both resolve to the same release. A requirement
///   [`requirement_bounds`] cannot bound (a range, a wildcard) does not count.
///
/// An unreadable version on either side does not count.
fn registry_patch_unifies(
    local: &FrameworkSource,
    patch: &FrameworkSource,
    locked: Option<&str>,
) -> bool {
    let (
        FrameworkSource::Registry {
            registry: local_registry,
            version: Some(local_version),
        },
        FrameworkSource::Registry {
            registry: patch_registry,
            version: Some(patch_version),
        },
    ) = (local, patch)
    else {
        return false;
    };
    if local_registry != patch_registry {
        return false;
    }
    let Ok(patch_req) = semver::VersionReq::parse(patch_version) else {
        return false;
    };
    let resolved = locked
        .and_then(|locked| semver::Version::parse(locked).ok())
        .filter(|locked| {
            semver::VersionReq::parse(local_version).is_ok_and(|req| req.matches(locked))
        });
    resolved.map_or_else(
        || {
            requirement_bounds(local_version)
                .is_some_and(|bounds| requirement_bounds(patch_version) == Some(bounds))
        },
        |resolved| patch_req.matches(&resolved),
    )
}

/// The lowest and highest version a `^`, `~`, `=` or bare requirement admits;
/// `None` for anything else (a range, a wildcard, a pre-release).
fn requirement_bounds(req: &str) -> Option<(semver::Version, semver::Version)> {
    let req = req.trim();
    let (op, rest) = match req.chars().next()? {
        op @ ('^' | '~' | '=') => (op, req[1..].trim()),
        c if c.is_ascii_digit() => ('^', req),
        _ => return None,
    };
    let parts = rest
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let (major, minor, patch) = match parts.as_slice() {
        [major] => (*major, None, None),
        [major, minor] => (*major, Some(*minor), None),
        [major, minor, patch] => (*major, Some(*minor), Some(*patch)),
        _ => return None,
    };
    let low = semver::Version::new(major, minor.unwrap_or(0), patch.unwrap_or(0));
    let top = |minor: Option<u64>, patch: Option<u64>| {
        semver::Version::new(major, minor.unwrap_or(u64::MAX), patch.unwrap_or(u64::MAX))
    };
    let high = match op {
        // `=`: the named version; a partial one spans what it leaves out.
        '=' => top(minor, patch),
        // `~`: its minor, or its major when it names only one.
        '~' => top(minor, None),
        // `^`: the compatibility series of the leftmost non-zero part.
        _ => match minor {
            Some(minor) if major == 0 && minor > 0 => top(Some(minor), None),
            Some(0) if major == 0 => top(Some(0), patch),
            _ => top(None, None),
        },
    };
    Some((low, high))
}

/// The git source of a dependency entry: the URL without a trailing `/` or
/// `.git`, and its `rev`, `tag` or `branch`.
fn git_source(entry: &toml::Value) -> Option<FrameworkSource> {
    let url = entry.get("git")?.as_str()?;
    let url = url.trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url).to_owned();
    let reference = ["rev", "tag", "branch"].iter().find_map(|key| {
        entry
            .get(*key)
            .and_then(toml::Value::as_str)
            .map(|value| ((*key).to_owned(), value.to_owned()))
    });
    Some(FrameworkSource::Git { url, reference })
}

/// The sources the crates.io patches of `autumn-web` that Cargo applies
/// point at. Cargo takes one patch for a crate: a nearer config overrides an
/// ancestor's and `$CARGO_HOME`'s, and any config overrides the workspace
/// manifest. So only the first file, in that order, that patches it counts;
/// a patch it overrides unifies nothing.
///
/// `from` is the directory Cargo is invoked in: it reads the configs there
/// and above, so a workspace member's own `.cargo/config.toml` applies only
/// when Cargo runs inside the member, not from the workspace root.
fn framework_patch_sources(root: &Path, from: &Path) -> Vec<FrameworkSource> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let workspace = workspace_root(&root);
    let mut files = config_files(from, cargo_home().as_deref());
    files.push((workspace.join("Cargo.toml"), workspace));
    let want = canonical("autumn-web");
    let mut dirs = Vec::new();
    for (file, base) in files {
        if !dirs.is_empty() {
            break;
        }
        let Some(table) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        else {
            continue;
        };
        let Some(patch) = table.get("patch").and_then(toml::Value::as_table) else {
            continue;
        };
        for (_, entries) in patch
            .iter()
            .filter(|(source, _)| is_crates_io_source(source))
        {
            for (key, entry) in entries.as_table().into_iter().flat_map(|e| e.iter()) {
                let package = entry
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(key);
                if canonical(package) != want {
                    continue;
                }
                if let Some(path) = entry.get("path").and_then(toml::Value::as_str) {
                    dirs.push(FrameworkSource::Path(normalized(&base.join(path))));
                } else if let Some(git) = git_source(entry) {
                    dirs.push(git);
                } else if let Some(registry) = registry_source(entry) {
                    dirs.push(registry);
                }
            }
        }
    }
    dirs
}

/// The package name and version a Cargo package ID spec names:
/// `name`, `name:1.2.3`, `name@1.2.3`, or any of these after a source URL
/// and `#` (`https://github.com/rust-lang/crates.io-index#name@1.2.3`). A
/// URL whose fragment is only a version, or that has none, names the last
/// segment of its path.
fn package_id_spec(spec: &str) -> Option<(String, Option<String>)> {
    let split = |pkg: &str| {
        let (name, version) = pkg
            .split_once(['@', ':'])
            .map_or((pkg, None), |(name, version)| {
                (name, Some(version.to_owned()))
            });
        (!name.is_empty()).then(|| (name.to_owned(), version))
    };
    let (url, fragment) = spec
        .split_once('#')
        .map_or((spec, None), |(url, fragment)| (url, Some(fragment)));
    if !url.contains("://") {
        return split(fragment.unwrap_or(url));
    }
    let last_segment = || {
        let path = url.split(['?']).next().unwrap_or(url).trim_end_matches('/');
        let last = path.rsplit('/').next().unwrap_or(path);
        last.strip_suffix(".git").unwrap_or(last).to_owned()
    };
    match fragment {
        Some(version) if version.starts_with(|c: char| c.is_ascii_digit()) => {
            Some((last_segment(), Some(version.to_owned())))
        }
        Some(pkg) => split(pkg),
        None => Some((last_segment(), None)),
    }
}

/// [`patched_by`] with `$CARGO_HOME` given, so a test need not set it.
fn patched_by_in(
    root: &Path,
    crate_name: &str,
    version: &str,
    cargo_home: Option<&Path>,
) -> Option<String> {
    let want = canonical(crate_name);
    let pinned = semver::Version::parse(version.trim_start_matches('=')).ok();
    // Absolute, or the parents of `.` (what the CLI passes) are empty.
    let root = &std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let read = |path: &Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    };
    let workspace = workspace_root(root);
    let manifest = workspace.join("Cargo.toml");
    if let Some(table) = read(&manifest) {
        if let Some(source) = crates_io_patch(&table, &want, pinned.as_ref(), &workspace) {
            return Some(format!("[patch.{source}] in {}", manifest.display()));
        }
        if let Some(replace) = table.get("replace").and_then(toml::Value::as_table) {
            // A `[replace]` key is a package ID spec: `name`, `name:version`
            // or `name@version`, optionally after a source URL and `#`. It
            // replaces that version only; a bare name matches any, and a
            // partial version its whole series.
            let version = version.trim_start_matches('=');
            let hit = replace.keys().any(|key| {
                package_id_spec(key).is_some_and(|(name, spec_version)| {
                    canonical(&name) == want
                        && spec_version
                            .is_none_or(|v| v == version || version.starts_with(&format!("{v}.")))
                })
            });
            if hit {
                return Some(format!("[replace] in {}", manifest.display()));
            }
        }
    }
    for (path, base) in config_files(root, cargo_home) {
        let Some(table) = read(&path) else {
            continue;
        };
        if let Some(source) = crates_io_patch(&table, &want, pinned.as_ref(), &base) {
            return Some(format!("[patch.{source}] in {}", path.display()));
        }
        // Source replacement swaps crates.io itself: every crate, this one
        // included, comes from the named source (a vendor dir, a mirror). A
        // `[source.crates-io]` that sets `registry`, `local-registry`,
        // `directory` or `git` redefines it the same way.
        if let Some(crates_io) = table
            .get("source")
            .and_then(|source| source.get("crates-io"))
            .and_then(toml::Value::as_table)
            .filter(|crates_io| !crates_io.is_empty())
        {
            let how = crates_io
                .get("replace-with")
                .and_then(toml::Value::as_str)
                .map_or_else(
                    || "redefines crates.io".to_owned(),
                    |with| format!("replace-with = \"{}\"", super::index::sanitize(with)),
                );
            return Some(format!("[source.crates-io] {how} in {}", path.display()));
        }
        // `paths` overrides replace any crate whose name and version match a
        // local one, and which crates they cover cannot be told from here.
        if table
            .get("paths")
            .and_then(toml::Value::as_array)
            .is_some_and(|paths| !paths.is_empty())
        {
            return Some(format!("`paths` overrides in {}", path.display()));
        }
    }
    None
}

/// The `[patch.<source>]` key under which `table` patches the crates.io
/// crate `want` (canonical). A `package` rename counts: `local = { package
/// = "x", … }` patches `x`, whatever the key says. `base` is where a relative
/// `path` resolves from.
fn crates_io_patch(
    table: &toml::Table,
    want: &str,
    pinned: Option<&semver::Version>,
    base: &Path,
) -> Option<String> {
    let patch = table.get("patch")?.as_table()?;
    patch
        .iter()
        .filter(|(source, _)| is_crates_io_source(source))
        .find(|(_, entries)| {
            entries.as_table().is_some_and(|e| {
                e.iter().any(|(key, entry)| {
                    let package = entry
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key);
                    // Cargo uses a patch only when the version its source
                    // supplies meets the pin. A local checkout says which it
                    // is; otherwise a `version` that excludes the pin rules
                    // it out, and anything else may supply it.
                    let supplied = entry
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .and_then(|path| package_version(&base.join(path)));
                    let usable = match (supplied, pinned) {
                        (Some(supplied), Some(pinned)) => semver::Comparator {
                            op: semver::Op::Exact,
                            major: pinned.major,
                            minor: Some(pinned.minor),
                            patch: Some(pinned.patch),
                            pre: pinned.pre.clone(),
                        }
                        .matches(&supplied),
                        _ => entry
                            .get("version")
                            .and_then(toml::Value::as_str)
                            .and_then(|req| semver::VersionReq::parse(req).ok())
                            .zip(pinned)
                            .is_none_or(|(req, pinned)| req.matches(pinned)),
                    };
                    canonical(package) == want && usable
                })
            })
        })
        .map(|(source, _)| source.clone())
}

/// The `[package] version` of the crate at `dir`, read from its workspace
/// root's `[workspace.package]` when it is `version.workspace = true`.
fn package_version(dir: &Path) -> Option<semver::Version> {
    let read = |path: &Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    };
    let version = read(&dir.join("Cargo.toml"))?
        .get("package")?
        .get("version")?
        .clone();
    let version = match version {
        toml::Value::String(version) => version,
        inherited if inherited.get("workspace").and_then(toml::Value::as_bool) == Some(true) => {
            read(&workspace_root(dir).join("Cargo.toml"))?
                .get("workspace")?
                .get("package")?
                .get("version")?
                .as_str()?
                .to_owned()
        }
        _ => return None,
    };
    semver::Version::parse(&version).ok()
}

/// Whether a `[patch.<source>]` key names crates.io: its name, its git
/// index, or its sparse index.
fn is_crates_io_source(source: &str) -> bool {
    // Cargo canonicalizes the URL: a trailing `/` or `.git` and the case of
    // a GitHub path do not make it another source.
    let source = source.trim().trim_end_matches('/');
    let source = source
        .strip_suffix(".git")
        .unwrap_or(source)
        .trim_end_matches('/')
        .to_ascii_lowercase();
    matches!(
        source.as_str(),
        "crates-io"
            | "https://github.com/rust-lang/crates.io-index"
            | "sparse+https://index.crates.io"
    )
}

/// The `[dependencies]` key that names `crate_name` as crates.io does:
/// case and `-`/`_` do not count. `None` when no key does.
#[must_use]
pub fn declared_dependency_key(manifest: &str, crate_name: &str) -> Option<String> {
    let table = toml::from_str::<toml::Table>(manifest).ok()?;
    let want = canonical(crate_name);
    table
        .get("dependencies")?
        .as_table()?
        .keys()
        .find(|key| canonical(key) == want)
        .cloned()
}

/// A dependency key other than `crate_name` whose `package` names it
/// (`alias = { package = "crate_name", … }`), in any dependency table:
/// regular, dev, build or target-specific. A `{ workspace = true }` entry
/// takes its `package` from the workspace root of `root`. Cargo refuses a
/// second entry for the same crate under another name.
#[must_use]
pub fn aliased_dependency_key(root: &Path, manifest: &str, crate_name: &str) -> Option<String> {
    let table = toml::from_str::<toml::Table>(manifest).ok()?;
    let workspace_deps = std::fs::read_to_string(workspace_root(root).join("Cargo.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .and_then(|table| table.get("workspace")?.get("dependencies").cloned());
    let want = canonical(crate_name);
    dependency_tables(&table)
        .flat_map(|deps| deps.iter())
        .find(|(key, entry)| {
            let entry = if entry.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                workspace_deps
                    .as_ref()
                    .and_then(|deps| deps.get(key.as_str()))
            } else {
                Some(*entry)
            };
            canonical(key) != want
                && entry
                    .and_then(|entry| entry.get("package"))
                    .and_then(toml::Value::as_str)
                    .is_some_and(|package| canonical(package) == want)
        })
        .map(|(key, _)| key.clone())
}

/// Whether `manifest` takes `crate_name` from somewhere other than its
/// crates.io release: a `path`, `git` or `registry` key on its
/// `[dependencies]` entry, or a `package` key naming another crate.
#[must_use]
pub fn dependency_has_alternate_source(manifest: &str, crate_name: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(manifest) else {
        return false;
    };
    table
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .and_then(|deps| deps.get(crate_name))
        .and_then(toml::Value::as_table)
        .is_some_and(|entry| {
            let renamed = entry
                .get("package")
                .and_then(toml::Value::as_str)
                .is_some_and(|package| canonical(package) != canonical(crate_name));
            renamed
                || ["path", "git", "registry", "registry-index"]
                    .iter()
                    .any(|key| entry.contains_key(*key))
        })
}

/// A crate name as crates.io compares it: case and `-`/`_` do not count.
fn canonical(name: &str) -> String {
    super::index::canonical(name)
}

/// Whether the app's `autumn-web` comes from a path or git checkout that no
/// `[patch.crates-io]` entry redirects.
///
/// This is fatal for an install from crates.io: the plugin would depend on the
/// *registry* `autumn-web`, Cargo would build two distinct framework crates,
/// and the generated mount would not satisfy the local `AppBuilder`'s traits.
/// A `[patch.crates-io] autumn-web` entry collapses the two back into one,
/// which is exactly what this repo's own conformance gate does — so the check
/// is for a local source that is *not* patched, not for a local source.
///
/// Only what Cargo reads counts: the app's own entry, or the workspace
/// root's `[workspace.dependencies]` entry it inherits, and a patch from the
/// workspace root or a `.cargo/config.toml` ([`patched_by`]). A nearer
/// manifest that is not the app's workspace root is ignored, as Cargo does.
#[must_use]
pub fn unpatched_local_framework(root: &Path) -> bool {
    // Any of the package's entries, a `package` rename or an inherited one
    // included, that takes the framework from a checkout.
    let locals: Vec<(toml::Value, bool)> = package_autumn_web_entries(root)
        .into_iter()
        .filter(|(entry, _)| {
            ["path", "git", "registry", "registry-index"]
                .iter()
                .any(|key| entry.get(*key).is_some())
        })
        .collect();
    if locals.is_empty() {
        return false;
    }
    // A patch collapses the two copies only when it points at the same
    // source: two paths, or two git repositories or refs, are two packages
    // to Cargo, whatever their version. A path resolves from the package,
    // or from the workspace root when the entry is inherited, as Cargo does.
    let root_abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    // Cargo may run in the member or at the workspace root, and a member's
    // own config applies only in the first: the patch has to unify in both
    // (verified with cargo 1.98 metadata).
    let workspace = workspace_root(&root_abs);
    let mut contexts = vec![root_abs.clone()];
    if workspace != root_abs {
        contexts.push(workspace);
    }
    let locked = locked_version_for(root, None, "autumn-web");
    contexts
        .iter()
        .any(|from| unpatched_from(root, &root_abs, &locals, from, locked.as_deref()))
}

/// [`unpatched_local_framework`] for Cargo invoked in `from`.
fn unpatched_from(
    root: &Path,
    root_abs: &Path,
    locals: &[(toml::Value, bool)],
    from: &Path,
    locked: Option<&str>,
) -> bool {
    let patches = framework_patch_sources(root, from);
    let path_unpatched = locals
        .iter()
        .filter_map(|(entry, inherited)| Some((entry.get("path")?.as_str()?, *inherited)))
        .any(|(path, inherited)| {
            let base = if inherited {
                workspace_root(root_abs)
            } else {
                root_abs.to_path_buf()
            };
            !patches.contains(&FrameworkSource::Path(normalized(&base.join(path))))
        });
    let git_unpatched = locals
        .iter()
        .filter_map(|(entry, _)| git_source(entry))
        .any(|source| !patches.contains(&source));
    // An alternate registry's `autumn-web` is another package than
    // crates.io's, at any version, unless a patch supplies the one the app
    // resolves.
    let registry_unpatched = locals
        .iter()
        .filter(|(entry, _)| entry.get("path").is_none() && entry.get("git").is_none())
        .filter_map(|(entry, _)| registry_source(entry))
        .any(|local| {
            !patches
                .iter()
                .any(|patch| registry_patch_unifies(&local, patch, locked))
        });
    path_unpatched || git_unpatched || registry_unpatched
}

/// Whether `main_rs` already mounts `entry` **in code**.
///
/// Two independent kinds of evidence, because a single substring test is wrong
/// in both directions:
///
/// - The plugin's **type path** inside a [`CatalogEntry::mount_call`]
///   argument (`.plugin(…)` / `.with_blob_store(…)`), located by a
///   balanced-paren scan so a mount split across lines still matches. The type
///   path is only trusted *there*: on its own it is just a name, and
///   `fn configure(_: autumn_admin_plugin::AdminPlugin) {}` would otherwise
///   read as a mount and suppress the real one.
/// - The plugin's **constructor call** (`AdminPlugin::new(`) anywhere in code.
///   A plugin built into a variable and mounted as `.plugin(configured)` is
///   still a mount, and the mount call alone cannot see that. Splicing a
///   second, default-constructed mount over it would take priority in
///   `AppBuilder::plugin`'s duplicate check and silently discard the user's
///   configuration — the worse of the two failures.
///
/// Both run against [`crate::rust_source::mask_non_code`] output, so neither a
/// comment nor a string literal can spoof either one.
#[must_use]
pub fn mount_present(main_rs: &str, entry: &CatalogEntry) -> bool {
    let masked = crate::rust_source::mask_non_code(main_rs);
    masked.contains(entry.constructor)
        || mount_call_span(&masked, entry, |argument| {
            argument.contains(entry.mount_arg)
        })
        .is_some()
}

/// The `(`…`)` byte range of the first [`CatalogEntry::mount_call`] in `masked`
/// whose argument `accept` approves, as `(open_paren, close_paren)`.
///
/// One scan, shared by the two commands that have to agree on what a mount
/// *is*: `plugin add`'s [`mount_present`] (which asks only whether the type
/// path appears in the argument) and `plugin remove`'s
/// `remove::mount_span` (which additionally requires the argument to *begin*
/// with it, so a plugin nested inside another plugin's constructor is never
/// excised). Extracted so the two cannot drift: the whole manual-fallback
/// contract rests on `remove` refusing exactly the mounts it cannot excise,
/// and `add` seeing exactly the mounts that are there.
///
/// `masked` must be [`crate::rust_source::mask_non_code`] output, whose byte
/// offsets are the original's.
#[must_use]
pub fn mount_call_span(
    masked: &str,
    entry: &CatalogEntry,
    accept: impl Fn(&str) -> bool,
) -> Option<(usize, usize)> {
    let mut from = 0usize;
    while let Some(found) = masked[from..].find(entry.mount_call) {
        // The `(` the call opens with is the last byte of `mount_call`.
        let open = from + found + entry.mount_call.len() - 1;
        // Unbalanced: the source is mid-edit or unparseable, and nothing
        // further can be read reliably.
        let close = crate::rust_source::balanced_close_paren(masked, open)?;
        if accept(&masked[open + 1..close]) {
            return Some((open, close));
        }
        from = close;
    }
    None
}

/// Byte offset just past the builder-opening `autumn_web::app()` **inside the
/// body of `async fn main`**, or `None` when there is no unambiguous anchor.
///
/// Three rules, each of which exists because breaking it produces a wrong
/// edit rather than no edit:
///
/// 1. **Scoped to `main`'s body.** A sticky "have I seen `async fn main` yet"
///    flag is not enough: an app that factors its builder into a helper
///    (`fn build_app() -> AppBuilder { autumn_web::app()… }`) or that keeps a
///    `#[cfg(test)] mod tests` harness would otherwise be spliced *there* —
///    mounting the plugin into a function the binary never calls, or (for the
///    `autumn-storage-s3` mount, which awaits) into a synchronous fn, which
///    does not compile. The entry point is a free function, so the `async fn
///    main` line must sit at brace depth 0: a helper method with the same name
///    inside an `impl` block (indented, depth > 0) is not the entry the binary
///    runs, and anchoring there would splice the mount into the wrong function.
///    The body runs from the `async fn main` line to the first line that closes
///    a brace at column 0.
/// 2. **Exactly one candidate.** [`crate::rust_source::code_lines`] skips
///    comments but not string literals, so a quick-start snippet inside a raw
///    string can look like an anchor. Refusing when there is more than one
///    candidate turns that from a silent mis-splice into the documented
///    manual fallback.
/// 3. **The line must END with the opener.** A one-line chain
///    (`autumn_web::app().routes(…).run().await;`) has nowhere to splice a
///    call into.
fn builder_anchor(main_rs: &str) -> Option<usize> {
    let lines = crate::rust_source::code_lines(main_rs);
    // The mask blanks strings and comments, so only real code braces move the
    // depth; a brace inside a string or comment cannot shift it.
    let mut depth = 0usize;
    let mut main_at = None;
    for (index, (line, _)) in lines.iter().enumerate() {
        if depth == 0 && crate::rust_source::declares_async_main(line) {
            main_at = Some(index);
            break;
        }
        for byte in line.bytes() {
            match byte {
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    let main_at = main_at?;
    // The body ends at the first code line that closes a brace at column 0 —
    // the closing brace of a top-level `async fn main`.
    let body_end = lines
        .iter()
        .skip(main_at + 1)
        .position(|(line, _)| line.starts_with('}'))
        .map_or(lines.len(), |offset| main_at + 1 + offset);

    let mut candidates = lines[main_at..body_end]
        .iter()
        .filter(|(line, _)| line.trim_end().ends_with("autumn_web::app()"));
    let (line, offset) = candidates.next()?;
    if candidates.next().is_some() {
        // Ambiguous: refuse rather than guess (rule 2).
        return None;
    }
    Some(offset + line.trim_end().len())
}

/// Splice `mount` into the `AppBuilder` chain, or `None` when the chain has no
/// unambiguous anchor (a heavily customized `main.rs`).
#[must_use]
pub fn insert_mount(main_rs: &str, mount: &str) -> Option<String> {
    let anchor_end = builder_anchor(main_rs)?;
    let snippet = mount.trim_end_matches('\n');
    let mut out = String::with_capacity(main_rs.len() + snippet.len() + 1);
    out.push_str(&main_rs[..anchor_end]);
    out.push('\n');
    out.push_str(snippet);
    out.push_str(&main_rs[anchor_end..]);
    Some(out)
}

/// The post-install steps printed for `entry`: its config keys first (nothing
/// else can be done until the app is configured), then its follow-ups.
#[must_use]
pub fn steps_for(entry: &CatalogEntry) -> Vec<String> {
    let mut steps = Vec::with_capacity(entry.config_keys.len() + entry.post_install.len());
    for keys in entry.config_keys {
        steps.push(format!(
            "Add to `autumn.toml`:\n\n{}\n",
            indent(keys, "         ")
        ));
    }
    steps.extend(entry.post_install.iter().map(|step| (*step).to_owned()));
    steps
}

/// Indent every non-empty line of `text` by `prefix`.
fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `autumn-web` version the app at `root` builds with, for the
/// compatibility check of `crate_name`. What Cargo resolved wins when the
/// declaration still admits it (`">=0.7, <0.9"` locked at 0.8 is 0.8);
/// target-specific declarations of different versions, or an unversioned
/// edge beside a versioned one, are settled only by the lock. With no
/// declared or locked version, the local checkout's own version; `Ok(None)`
/// when that cannot be read either.
///
/// # Errors
///
/// [`PluginError::AmbiguousAutumnWeb`] when several declarations and no lock
/// leave the version open.
fn resolved_app_version(root: &Path, crate_name: &str) -> Result<Option<String>, PluginError> {
    let declared = declared_autumn_web_versions(root);
    let locked = locked_version_for(root, None, "autumn-web");
    let admits = |req: &str, locked: &str| {
        semver::VersionReq::parse(req)
            .ok()
            .zip(semver::Version::parse(locked).ok())
            .is_none_or(|(req, version)| req.matches(&version))
    };
    let mixed = mixed_autumn_web_declarations(root);
    let app = match (declared.as_slice(), locked) {
        ([], locked) => locked,
        ([one], Some(locked)) if mixed || admits(one, &locked) => Some(locked),
        ([one], None) if mixed => {
            return Err(PluginError::AmbiguousAutumnWeb {
                crate_name: crate_name.to_owned(),
                declared: format!("{one} and an unversioned path or git entry"),
            });
        }
        ([one], _) => Some(one.clone()),
        (_, Some(locked)) => Some(locked),
        (several, None) => {
            return Err(PluginError::AmbiguousAutumnWeb {
                crate_name: crate_name.to_owned(),
                declared: several.join(", "),
            });
        }
    };
    Ok(app.or_else(|| local_framework_version(root)))
}

/// `release`'s compatibility series as `(first, last, next_first,
/// previous_last)`: its own first and last version, the first version of the
/// next series, and the last of the previous one (none below `0.0`).
fn series_bounds(
    release: &semver::Version,
) -> (
    semver::Version,
    semver::Version,
    semver::Version,
    Option<semver::Version>,
) {
    if release.major == 0 {
        (
            semver::Version::new(0, release.minor, 0),
            semver::Version::new(0, release.minor, u64::MAX),
            semver::Version::new(0, release.minor + 1, 0),
            release
                .minor
                .checked_sub(1)
                .map(|minor| semver::Version::new(0, minor, u64::MAX)),
        )
    } else {
        (
            semver::Version::new(release.major, 0, 0),
            semver::Version::new(release.major, u64::MAX, u64::MAX),
            semver::Version::new(release.major + 1, 0, 0),
            Some(semver::Version::new(release.major - 1, u64::MAX, u64::MAX)),
        )
    }
}

/// Whether the requirement `app` admits some version in `release`'s
/// compatibility series. A requirement is one interval, so it meets the
/// series exactly when it admits one of the series' ends or its own lower
/// bound lies inside the series; that bound is a comparator's version, or
/// the one after it for `>`. An unreadable side counts as meeting it.
fn meets_series(app: &str, release: &str) -> bool {
    let (Ok(req), Ok(release)) = (
        semver::VersionReq::parse(app),
        semver::Version::parse(release.trim_start_matches('=')),
    ) else {
        return true;
    };
    let (first, last, _, _) = series_bounds(&release);
    let in_series = |v: &semver::Version| *v >= first && *v <= last;
    let lower_bounds = req.comparators.iter().flat_map(|c| {
        let base = semver::Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0));
        let after = semver::Version::new(base.major, base.minor, base.patch.saturating_add(1));
        [base, after]
    });
    [first.clone(), last.clone()]
        .into_iter()
        .chain(lower_bounds)
        .any(|v| in_series(&v) && req.matches(&v))
}

/// Whether the requirement `app` admits a release outside `release`'s
/// compatibility series. A requirement is one interval, so it spans series
/// exactly when it admits the first version of the next series or the last
/// of the previous one. A concrete (locked) version, read as `^x.y.z`, never
/// does.
fn spans_series(app: &str, release: &str) -> bool {
    let Ok(req) = semver::VersionReq::parse(app) else {
        return false;
    };
    let Ok(release) = semver::Version::parse(release.trim_start_matches('=')) else {
        return false;
    };
    let (_, _, next, previous) = series_bounds(&release);
    req.matches(&next) || previous.is_some_and(|previous| req.matches(&previous))
}

/// Plan the install of `entry` at `version` into the project at `root`.
///
/// Ordering is the contract. The project check and the version gate run before
/// a single [`crate::generate::emit::Action`] exists, so every refusal leaves
/// the app byte-identical. The builder-chain edit is computed (not applied)
/// before the manifest edit is queued, so no outcome can add a dependency whose
/// mount was never even computed — and the two writes are queued manifest-first,
/// so a mid-execute I/O failure leaves an inert dependency (it compiles; it
/// shows in `cargo tree`) instead of a mount with no dependency, which leaves
/// the app uncompilable.
///
/// # Errors
///
/// [`PluginError::NotInProject`], [`PluginError::NoAutumnWeb`],
/// [`PluginError::NoPackageTable`], [`PluginError::Incompatible`], or an I/O
/// error reading the manifest.
pub fn plan_add(
    root: &Path,
    entry: &CatalogEntry,
    version: &str,
) -> Result<AddOutcome, PluginError> {
    refuse_virtual_workspace(root)?;
    app_autumn_web(root)?;
    let app = resolved_app_version(root, entry.crate_name)?;
    // A range (`>=0.8, <0.9`) has no single series to compare, but one that
    // admits nothing in this release's series cannot build next to the
    // plugin, whose own `autumn-web` requirement is a caret on the release.
    // A range inside the series (`>=0.7.1, <0.8`) proceeds.
    let excluded = |app_version: &str| {
        parse_version(app_version).is_none() && !meets_series(app_version, version)
    };
    // `check_compat` reads versions without their prerelease, but the
    // plugin's caret `autumn-web = "<release>"` excludes a prerelease of it:
    // an app on `0.7.0-alpha.1` would build a second, stable framework.
    let prerelease_excluded = |app_version: &str| {
        semver::Version::parse(app_version.trim().trim_start_matches(['=', '^', '~', ' ']))
            .ok()
            .filter(|app| !app.pre.is_empty())
            .zip(semver::VersionReq::parse(&format!("^{}", version.trim_start_matches('='))).ok())
            .is_some_and(|(app, req)| !req.matches(&app))
    };
    if let Some(app_version) = &app
        && (check_compat(app_version, version) == Compat::Incompatible
            || excluded(app_version)
            || prerelease_excluded(app_version))
    {
        return Err(PluginError::Incompatible {
            crate_name: entry.crate_name.to_owned(),
            plugin_version: version.to_owned(),
            supported: supported_range(version),
            app_version: app_version.clone(),
        });
    }

    // An unresolved requirement that admits this release and another series
    // lets Cargo pick that series for the app and this one for the plugin.
    if let Some(app_version) = &app
        && spans_series(app_version, version)
    {
        return Err(PluginError::UnresolvedAutumnWebRange {
            crate_name: entry.crate_name.to_owned(),
            declared: app_version.clone(),
        });
    }

    if unpatched_local_framework(root) {
        return Err(PluginError::UnpatchedLocalFramework {
            crate_name: entry.crate_name.to_owned(),
        });
    }
    // A local checkout whose version cannot be read: skipping the check
    // would pin this series next to a checkout of another.
    if app.is_none() {
        return Err(PluginError::UnknownAutumnWebVersion {
            crate_name: entry.crate_name.to_owned(),
        });
    }

    let manifest = manifest_path(root);
    let manifest_src = std::fs::read_to_string(&manifest)?;
    let main_path = root.join("src").join("main.rs");
    let main_src = std::fs::read_to_string(&main_path).unwrap_or_default();

    // An existing entry at another series is not "already installed": the key
    // is there, so `ensure_cargo_dependencies` would leave the old pin in
    // place while the mount written below is this series' shape.
    if let Some(declared) = declared_dependency_version(&manifest_src, entry.crate_name)
        && check_compat(&declared, version) == Compat::Incompatible
    {
        return Err(PluginError::DependencyVersionMismatch {
            crate_name: entry.crate_name.to_owned(),
            declared,
            installing: version.to_owned(),
        });
    }

    let dependency_installed = dependency_present(&manifest_src, entry.crate_name);
    let already_mounted = mount_present(&main_src, entry);
    if dependency_installed && already_mounted {
        return Ok(AddOutcome::AlreadyInstalled);
    }

    let mounted_src = if already_mounted {
        None
    } else {
        match insert_mount(&main_src, entry.mount) {
            Some(updated) => Some(updated),
            None => {
                return Ok(AddOutcome::Manual {
                    reason: format!(
                        "could not find the `autumn_web::app()` builder chain in {} — nothing was changed",
                        main_path.display().to_string().replace('\\', "/")
                    ),
                    dependency_line: dependency_line(entry.crate_name, &exact_pin(version)),
                    mount_snippet: entry.mount.trim_end_matches('\n').to_owned(),
                    steps: steps_for(entry),
                });
            }
        }
    };

    // `Cargo.toml` is queued BEFORE `src/main.rs`. `Plan::execute` writes
    // actions in order with no rollback, so if the second write fails (a
    // read-only file, ENOSPC) this ordering leaves a dependency with no mount
    // — inert, compiling, visible in `cargo tree` — rather than a mount with
    // no dependency, which leaves the app uncompilable.
    let mut plan = Plan::new(root);
    // Exact: the trust review and the conformance record describe this
    // release, and a caret would let Cargo take a later, unreviewed patch.
    let spec = format!("\"{}\"", exact_pin(version));
    let updated_manifest = crate::generate::model::ensure_cargo_dependencies(
        &manifest_src,
        &[(entry.crate_name, spec.as_str())],
    );
    if updated_manifest != manifest_src {
        plan.modify(manifest, updated_manifest);
    }
    if let Some(updated_main) = mounted_src {
        plan.modify(main_path, updated_main);
    }
    Ok(AddOutcome::Installed {
        plan: Box::new(plan),
        steps: steps_for(entry),
    })
}

/// Plan the dependency-only install of a community crate.
///
/// The mount is derived from the naming convention and *printed*, never
/// spliced: nothing here can verify a third-party crate exposes
/// `<Name>Plugin`, and an unused dependency always compiles while a wrong
/// mount does not.
///
/// # Errors
///
/// [`PluginError::NotInProject`], [`PluginError::NoAutumnWeb`],
/// [`PluginError::NoPackageTable`], [`PluginError::ImplausibleVersion`], or an
/// I/O error reading the manifest.
pub fn plan_add_community(
    root: &Path,
    crate_name: &str,
    version: &str,
) -> Result<AddOutcome, PluginError> {
    refuse_virtual_workspace(root)?;
    app_autumn_web(root)?;
    // `=` pins a listed crate to its verified version (issue #1625).
    if !is_plausible_version(version.strip_prefix('=').unwrap_or(version)) {
        return Err(PluginError::ImplausibleVersion {
            crate_name: crate_name.to_owned(),
            version: version.to_owned(),
        });
    }
    // A crates.io plugin links the registry `autumn-web`: next to an
    // unpatched local checkout it is a second framework, locked or not.
    if unpatched_local_framework(root) {
        return Err(PluginError::UnpatchedLocalFramework {
            crate_name: crate_name.to_owned(),
        });
    }
    let manifest = manifest_path(root);
    let manifest_src = std::fs::read_to_string(&manifest)?;
    let snippet = super::catalog::community_mount_snippet(crate_name)
        .unwrap_or_else(|| "        .plugin(/* see the crate's README */)".to_owned());

    // NOT `AlreadyInstalled`: that outcome reports the dependency *and* the
    // mount as in place, and a community mount is never written — so a re-run
    // would claim a complete install and stop showing the snippet the user
    // still has to paste. The outcome stays dependency-only; only the wording
    // changes.
    let dependency_added = !dependency_present(&manifest_src, crate_name);
    let mut plan = Plan::new(root);
    if dependency_added {
        let spec = format!("\"{version}\"");
        let updated = crate::generate::model::ensure_cargo_dependencies(
            &manifest_src,
            &[(crate_name, spec.as_str())],
        );
        if updated != manifest_src {
            plan.modify(manifest, updated);
        }
    }
    Ok(AddOutcome::DependencyOnly {
        plan: Box::new(plan),
        dependency_added,
        dependency_line: dependency_line(crate_name, version),
        mount_snippet: snippet,
    })
}

/// Every Rust source file a Cargo target in `root` is built from, *outside*
/// the conventional `src`/`tests`/`benches`/`examples` trees.
///
/// Cargo lets a target live anywhere: `[[bin]] path = "cmd/server.rs"` is
/// valid, and invisible to a scan of the conventional directories. Two
/// commands need to see those files, for mirror-image reasons — `plugin
/// remove` must not strip a dependency such a target still uses, and `autumn
/// doctor` must not report a plugin mounted there as "declared but never
/// mounted" (which would fail `--strict` on a perfectly valid project). One
/// definition, so the two cannot disagree about where an app's code lives.
///
/// Returns the file each target names, plus every `.rs` file in that file's
/// own directory tree — a target's sibling modules (`cmd/routes.rs`) are part
/// of it. The tree sweep is deliberately skipped for a file sitting directly
/// at the project root, where "the whole tree" would mean the entire checkout,
/// `target/` included; the conventional sweep already covers what matters
/// there.
#[must_use]
pub fn explicit_target_sources(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for named in explicit_target_paths(root) {
        push_unique(&mut out, named.clone());
        match named.parent() {
            Some(parent) if parent != root => {
                for file in rs_files_under(parent) {
                    push_unique(&mut out, file);
                }
            }
            _ => {}
        }
    }
    out
}

/// Push `path` unless it is already there — the same file can be reached from
/// two targets (a `[[bin]]` and its `[[test]]` sharing a directory).
fn push_unique(out: &mut Vec<PathBuf>, path: PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
}

/// Every source path `root`'s manifest names explicitly: the build script and
/// any `path` on a `[lib]`, `[[bin]]`, `[[example]]`, `[[test]]` or
/// `[[bench]]` target.
fn explicit_target_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.join("build.rs")];
    let Ok(manifest) = std::fs::read_to_string(manifest_path(root)) else {
        return paths;
    };
    let Ok(table) = toml::from_str::<toml::Table>(&manifest) else {
        return paths;
    };
    // A custom build-script path (`[package] build = "…"`) replaces `build.rs`.
    if let Some(build) = table
        .get("package")
        .and_then(|package| package.get("build"))
        .and_then(toml::Value::as_str)
    {
        paths.push(root.join(build));
    }
    let mut push_path = |value: &toml::Value| {
        if let Some(path) = value.get("path").and_then(toml::Value::as_str) {
            paths.push(root.join(path));
        }
    };
    if let Some(lib) = table.get("lib") {
        push_path(lib);
    }
    for kind in ["bin", "example", "test", "bench"] {
        if let Some(targets) = table.get(kind).and_then(toml::Value::as_array) {
            for target in targets {
                push_path(target);
            }
        }
    }
    paths
}

/// Every `.rs` file under `dir`, recursively, in a stable order.
#[must_use]
pub fn rs_files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // Sorted: `read_dir` order is unspecified, and both callers report or
    // concatenate what they find.
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            out.extend(rs_files_under(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    out
}

/// `root`'s `Cargo.toml`.
#[must_use]
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join("Cargo.toml")
}

/// Whether the manifest text declares a `[package]` table — i.e. it is a
/// package manifest that can own a `[dependencies]` section, not a virtual
/// workspace root (issue #2381).
/// A virtual workspace root has no `[package]` table: `ensure_cargo_dependencies`
/// would append a `[dependencies]` section to a manifest that cannot own
/// dependencies (issue #2381). Checked before any dependency resolution, so a
/// real virtual manifest — `[workspace]` plus `[workspace.dependencies]`, no
/// top-level `[dependencies]` — gets this diagnostic rather than
/// [`PluginError::NoAutumnWeb`].
fn refuse_virtual_workspace(root: &Path) -> Result<(), PluginError> {
    let manifest = manifest_path(root);
    if !manifest.is_file() {
        return Err(PluginError::NotInProject);
    }
    if has_package_table(&std::fs::read_to_string(&manifest)?) {
        Ok(())
    } else {
        Err(PluginError::NoPackageTable)
    }
}

fn has_package_table(manifest_src: &str) -> bool {
    toml::from_str::<toml::Table>(manifest_src)
        .is_ok_and(|table| table.get("package").is_some_and(toml::Value::is_table))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// `[package] workspace` names the root, over a nearer `[workspace]`.
    #[test]
    fn an_explicit_package_workspace_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let app = outer.join("inner").join("app");
        write(
            &outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"inner/app\"]\n",
        );
        write(
            &outer.join("inner/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        );
        write(
            &app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nworkspace = \"../..\"\n",
        );
        assert_eq!(workspace_root(&app), outer.canonicalize().unwrap());
    }

    /// `{ workspace = true }` resolves against the package's workspace root,
    /// not a nearer manifest with its own `[workspace.dependencies]`.
    #[test]
    fn an_inherited_version_comes_from_the_named_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let app = outer.join("inner").join("app");
        write(
            &outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"inner/app\"]\n\n[workspace.dependencies]\nautumn-web = \"0.7\"\n",
        );
        write(
            &outer.join("inner/Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.dependencies]\nautumn-web = \"0.6\"\n",
        );
        write(
            &app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nworkspace = \"../..\"\n\n[dependencies]\nautumn-web = { workspace = true }\n",
        );
        assert!(matches!(
            app_autumn_web(&app),
            Ok(AppAutumnWeb::Version(v)) if v == "0.7"
        ));
    }

    /// An explicit member under an `exclude` prefix is still a member.
    #[test]
    fn an_explicit_member_wins_over_an_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::path::absolute(tmp.path()).unwrap();
        let app = root.join("crates").join("app");
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\"]\nexclude = [\"crates\"]\n",
        );
        write(&app.join("Cargo.toml"), "[package]\nname = \"app\"\n");
        assert_eq!(workspace_root(&app), root);
        // A member glob is not an explicit member: Cargo (1.98) keeps the
        // exclusion, and the app is its own root.
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/app\"]\n",
        );
        assert_eq!(workspace_root(&app), std::path::absolute(&app).unwrap());
    }

    /// A package an ancestor workspace excludes is its own root.
    #[test]
    fn an_excluded_package_is_its_own_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("apps").join("demo");
        write(&app.join("Cargo.toml"), "[package]\nname = \"demo\"\n");
        write(
            &tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"lib\"]\nexclude = [\"apps\"]\n",
        );
        let root = |p: &Path| std::path::absolute(p).unwrap();
        assert_eq!(workspace_root(&app), root(&app));
        write(
            &tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"apps/demo\"]\n",
        );
        assert_eq!(workspace_root(&app), root(tmp.path()));
    }

    /// The CLI passes `.`: the walk must still reach the workspace root.
    #[test]
    fn workspace_root_resolves_a_relative_dir() {
        // Tests run in the autumn-cli package; its workspace is the repo.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        assert_eq!(workspace_root(Path::new(".")), repo);
    }

    /// Only a crates.io patch redirects a crates.io dependency.
    #[test]
    fn a_patch_for_another_source_is_not_a_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        write(
            &manifest,
            "[package]\nname = \"a\"\n\n[patch.\"https://github.com/acme/repo\"]\n\
             autumn-plugin-x = { path = \"../x\" }\n",
        );
        assert_eq!(
            patched_by_in(tmp.path(), "autumn-plugin-x", "0.3.0", None),
            None
        );
        for source in [
            "crates-io",
            "\"https://github.com/rust-lang/crates.io-index\"",
            "\"sparse+https://index.crates.io/\"",
        ] {
            write(
                &manifest,
                &format!(
                    "[package]\nname = \"a\"\n\n[patch.{source}]\n\
                     autumn-plugin-x = {{ path = \"../x\" }}\n"
                ),
            );
            assert!(
                patched_by_in(tmp.path(), "autumn-plugin-x", "0.3.0", None).is_some(),
                "{source}"
            );
        }
    }

    /// A `[patch]` entry whose `version` excludes the pin cannot supply it.
    #[test]
    fn a_patch_whose_version_excludes_the_pin_is_not_a_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let with = |version: &str| {
            write(
                &tmp.path().join("Cargo.toml"),
                &format!(
                    "[package]\nname = \"a\"\n\n[patch.crates-io]\n\
                     autumn-plugin-x = {{ path = \"../x\"{version} }}\n"
                ),
            );
            patched_by_in(tmp.path(), "autumn-plugin-x", "=0.3.0", None)
        };
        assert_eq!(with(", version = \"0.2.0\""), None);
        assert!(with(", version = \"0.3.0\"").is_some());
        assert!(with("").is_some());
    }

    /// A local checkout's own version decides, not the entry's `version`
    /// requirement: `version = "0.2.0"` admits 0.2.5, but a 0.2.0 checkout
    /// cannot supply the `=0.2.5` pin.
    #[test]
    fn a_path_patch_is_judged_by_the_checkout_version() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        let checkout = |version: &str| {
            write(
                &tmp.path().join("x/Cargo.toml"),
                &format!("[package]\nname = \"autumn-plugin-x\"\nversion = \"{version}\"\n"),
            );
        };
        write(
            &app.join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[patch.crates-io]\n\
             autumn-plugin-x = { path = \"../x\", version = \"0.2.0\" }\n",
        );
        checkout("0.2.0");
        assert_eq!(patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None), None);
        checkout("0.2.5");
        assert!(patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None).is_some());
        // A config's path resolves from the directory holding its `.cargo`.
        write(&app.join("Cargo.toml"), "[package]\nname = \"a\"\n");
        write(
            &app.join(".cargo/config.toml"),
            "[patch.crates-io]\nautumn-plugin-x = { path = \"../x\" }\n",
        );
        assert!(patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None).is_some());
        checkout("0.2.0");
        assert_eq!(patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None), None);
    }

    /// A checkout that inherits its version reads it from its workspace root.
    #[test]
    fn a_path_patch_reads_an_inherited_checkout_version() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        write(
            &tmp.path().join("plugins/Cargo.toml"),
            "[workspace]\nmembers = [\"x\"]\n\n[workspace.package]\nversion = \"0.2.0\"\n",
        );
        write(
            &tmp.path().join("plugins/x/Cargo.toml"),
            "[package]\nname = \"autumn-plugin-x\"\nversion.workspace = true\n",
        );
        write(
            &app.join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[patch.crates-io]\n\
             autumn-plugin-x = { path = \"../plugins/x\" }\n",
        );
        assert_eq!(patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None), None);
        assert!(patched_by_in(&app, "autumn-plugin-x", "=0.2.0", None).is_some());
    }

    /// `[source.crates-io] replace-with` swaps crates.io itself.
    #[test]
    fn a_crates_io_source_replacement_is_a_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        write(&app.join("Cargo.toml"), "[package]\nname = \"a\"\n");
        assert_eq!(patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None), None);
        write(
            &app.join(".cargo/config.toml"),
            "[source.crates-io]\nreplace-with = \"vendored\"\n\n\
             [source.vendored]\ndirectory = \"vendor\"\n",
        );
        let found = patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None).unwrap();
        assert!(found.contains("replace-with = \"vendored\""), "{found}");
        // A direct redefinition of crates.io redirects it too.
        write(
            &app.join(".cargo/config.toml"),
            "[source.crates-io]\nregistry = \"sparse+https://mirror.example.com/\"\n",
        );
        let found = patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None).unwrap();
        assert!(found.contains("redefines crates.io"), "{found}");
    }

    /// A `[workspace.dependencies]` default the package does not inherit is
    /// not one of its declarations.
    #[test]
    fn an_uninherited_workspace_default_is_not_declared() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-web = \"0.7\"\n\n\
             [workspace]\n\n[workspace.dependencies]\nautumn-web = \"0.8\"\n",
        );
        assert_eq!(declared_autumn_web_versions(tmp.path()), ["0.7"]);
        write(
            &tmp.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-web = { workspace = true }\n\n\
             [workspace]\n\n[workspace.dependencies]\nautumn-web = \"0.8\"\n",
        );
        assert_eq!(declared_autumn_web_versions(tmp.path()), ["0.8"]);
        // A renamed inherited entry: the workspace names the package.
        write(
            &tmp.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn = { workspace = true }\n\n\
             [workspace]\n\n[workspace.dependencies]\n\
             autumn = { package = \"autumn-web\", version = \"0.7\" }\n",
        );
        assert_eq!(declared_autumn_web_versions(tmp.path()), ["0.7"]);
    }

    /// A `[replace]` key names one version: another version's replacement
    /// does not redirect the pin.
    #[test]
    fn a_replace_for_another_version_is_not_a_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        let with = |key: &str| {
            write(
                &manifest,
                &format!(
                    "[package]\nname = \"a\"\n\n[replace]\n\"{key}\" = {{ path = \"../x\" }}\n"
                ),
            );
            patched_by_in(tmp.path(), "autumn-plugin-x", "=0.3.0", None)
        };
        assert_eq!(with("autumn-plugin-x:0.2.0"), None);
        assert!(with("autumn-plugin-x:0.3.0").is_some());
        assert!(with("autumn_plugin_x").is_some());
        // Every package ID spec form Cargo accepts (verified with cargo
        // 1.98 metadata): `@`, and a source URL before `#`.
        for key in [
            "autumn-plugin-x@0.3.0",
            "autumn-plugin-x@0.3",
            "https://github.com/rust-lang/crates.io-index#autumn-plugin-x:0.3.0",
            "https://github.com/rust-lang/crates.io-index#autumn-plugin-x@0.3.0",
            "https://github.com/rust-lang/crates.io-index#autumn-plugin-x",
            "registry+https://github.com/rust-lang/crates.io-index#autumn-plugin-x@0.3.0",
            "https://github.com/acme/autumn-plugin-x.git#0.3.0",
        ] {
            assert!(with(key).is_some(), "{key}");
        }
        for key in [
            "https://github.com/rust-lang/crates.io-index#autumn-plugin-x@0.2.0",
            "https://github.com/rust-lang/crates.io-index#other@0.3.0",
        ] {
            assert_eq!(with(key), None, "{key}");
        }
    }

    /// The locked version comes from the workspace lockfile, by canonical
    /// name; two locked versions are ambiguous.
    #[test]
    fn locked_version_reads_the_workspace_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("Cargo.toml"), "[package]\nname = \"a\"\n");
        assert_eq!(locked_version(tmp.path(), "autumn-admin-plugin"), None);
        let pkg =
            |v: &str| format!("[[package]]\nname = \"autumn-admin-plugin\"\nversion = \"{v}\"\n\n");
        write(
            &tmp.path().join("Cargo.lock"),
            &format!("version = 4\n\n{}", pkg("0.7.1")),
        );
        assert_eq!(
            locked_version(tmp.path(), "autumn_admin_plugin").as_deref(),
            Some("0.7.1")
        );
        write(
            &tmp.path().join("Cargo.lock"),
            &format!("version = 4\n\n{}{}", pkg("0.7.1"), pkg("0.6.0")),
        );
        assert_eq!(locked_version(tmp.path(), "autumn-admin-plugin"), None);
    }

    /// Cargo reads manifest patches from the workspace root only: a
    /// member's own `[patch]` is ignored, the root's counts.
    #[test]
    fn only_the_workspace_root_manifest_patches() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        let patch = "[patch.crates-io]\nautumn-plugin-x = { path = \"../x\" }\n";
        write(
            &app.join("Cargo.toml"),
            &format!("[package]\nname = \"app\"\n\n{patch}"),
        );
        // Standalone: the package is its own root.
        assert!(patched_by_in(&app, "autumn-plugin-x", "0.3.0", None).is_some());
        // A member: its own table is ignored.
        write(
            &tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        );
        assert_eq!(patched_by_in(&app, "autumn-plugin-x", "0.3.0", None), None);
        write(
            &tmp.path().join("Cargo.toml"),
            &format!("[workspace]\nmembers = [\"app\"]\n\n{patch}"),
        );
        assert!(patched_by_in(&app, "autumn-plugin-x", "0.3.0", None).is_some());
    }

    /// With both `.cargo/config` and `.cargo/config.toml`, Cargo reads the
    /// extensionless one only.
    #[test]
    fn the_extensionless_cargo_config_wins() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("Cargo.toml"), "[package]\nname = \"a\"\n");
        let patch = "[patch.crates-io]\nautumn-plugin-x = { path = \"../x\" }\n";
        write(&tmp.path().join(".cargo/config.toml"), patch);
        write(&tmp.path().join(".cargo/config"), "[build]\njobs = 1\n");
        assert_eq!(
            patched_by_in(tmp.path(), "autumn-plugin-x", "0.3.0", None),
            None
        );
        std::fs::remove_file(tmp.path().join(".cargo/config")).unwrap();
        assert!(patched_by_in(tmp.path(), "autumn-plugin-x", "0.3.0", None).is_some());
    }

    /// Cargo also reads `[patch]` from `.cargo/config.toml` above the app,
    /// and from `$CARGO_HOME`.
    #[test]
    fn a_cargo_config_patch_is_a_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("work").join("app");
        write(&app.join("Cargo.toml"), "[package]\nname = \"a\"\n");
        let patch = "[patch.crates-io]\nautumn_plugin_x = { path = \"../x\" }\n";
        assert_eq!(patched_by_in(&app, "autumn-plugin-x", "0.3.0", None), None);

        write(&tmp.path().join("work/.cargo/config.toml"), patch);
        let found = patched_by_in(&app, "autumn-plugin-x", "0.3.0", None).unwrap();
        assert!(found.contains("config.toml"), "{found}");
        std::fs::remove_dir_all(tmp.path().join("work/.cargo")).unwrap();

        let home = tmp.path().join("cargo-home");
        write(&home.join("config.toml"), patch);
        assert!(patched_by_in(&app, "autumn-plugin-x", "0.3.0", Some(&home)).is_some());
        assert_eq!(
            patched_by_in(&app, "autumn-plugin-y", "0.3.0", Some(&home)),
            None
        );
    }
    use crate::plugin::catalog;

    /// The `main.rs` an `autumn new` app ships with, reduced to the shape that
    /// matters here: a builder chain opened by `autumn_web::app()`.
    const SCAFFOLD_MAIN: &str = r"use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    let app = autumn_web::app()
        .routes(routes![index])
        .migrations(MIGRATIONS);

    app
        .run()
        .await;
}
";

    const SCAFFOLD_CARGO: &str = r#"[package]
name = "demo"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.7.0"
maud = { version = "0.27", features = ["axum"] }
"#;

    fn fake_project(main_rs: &str, cargo: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), main_rs).unwrap();
        tmp
    }

    fn admin() -> &'static catalog::CatalogEntry {
        catalog::lookup("autumn-admin-plugin").expect("admin entry")
    }

    // ── AC #3: version safety ────────────────────────────────────────────────

    #[test]
    fn same_minor_series_is_compatible() {
        assert_eq!(check_compat("0.7.0", "0.7.0"), Compat::Compatible);
        assert_eq!(check_compat("0.7.3", "0.7.0"), Compat::Compatible);
        assert_eq!(check_compat("^0.7", "0.7.0"), Compat::Compatible);
    }

    /// Pre-1.0, Cargo treats every minor bump as breaking — the STABILITY.md
    /// contract this composes with.
    #[test]
    fn different_minor_series_is_incompatible_pre_1_0() {
        assert_eq!(check_compat("0.6.0", "0.7.0"), Compat::Incompatible);
        assert_eq!(check_compat("0.8.0", "0.7.0"), Compat::Incompatible);
    }

    #[test]
    fn post_1_0_only_the_major_has_to_match() {
        assert_eq!(check_compat("1.2.0", "1.5.0"), Compat::Compatible);
        assert_eq!(check_compat("2.0.0", "1.5.0"), Compat::Incompatible);
    }

    #[test]
    fn unparseable_versions_are_unknown_not_incompatible() {
        assert_eq!(check_compat("wat", "0.7.0"), Compat::Unknown);
        assert_eq!(check_compat("0.7.0", "wat"), Compat::Unknown);
    }

    #[test]
    fn supported_range_names_the_minor_series_pre_1_0() {
        assert_eq!(supported_range("0.7.0"), "0.7");
        assert_eq!(supported_range("1.4.2"), "1");
    }

    #[test]
    fn app_autumn_web_reads_the_declared_version() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        assert_eq!(
            app_autumn_web(tmp.path()).unwrap(),
            AppAutumnWeb::Version("0.7.0".to_owned())
        );
    }

    #[test]
    fn app_autumn_web_tolerates_a_path_dependency() {
        let tmp = fake_project(
            SCAFFOLD_MAIN,
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = { path = \"../autumn\" }\n",
        );
        assert_eq!(
            app_autumn_web(tmp.path()).unwrap(),
            AppAutumnWeb::Unversioned
        );
    }

    #[test]
    fn app_autumn_web_rejects_a_non_autumn_project() {
        let tmp = fake_project(
            SCAFFOLD_MAIN,
            "[package]\nname = \"demo\"\n\n[dependencies]\nserde = \"1\"\n",
        );
        assert!(matches!(
            app_autumn_web(tmp.path()).unwrap_err(),
            PluginError::NoAutumnWeb
        ));
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            app_autumn_web(empty.path()).unwrap_err(),
            PluginError::NotInProject
        ));
    }

    /// AC #3: the refusal happens **before any file is modified**, and names
    /// both versions.
    #[test]
    fn incompatible_app_version_fails_without_touching_any_file() {
        let cargo = SCAFFOLD_CARGO.replace("autumn-web = \"0.7.0\"", "autumn-web = \"0.5.0\"");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        let before_cargo = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        let before_main = std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();

        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("0.5.0"), "{message}");
        assert!(message.contains("0.7"), "{message}");
        assert!(message.contains("autumn-admin-plugin"), "{message}");

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            before_cargo
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            before_main
        );
    }

    /// A plugin already pinned to another framework series is not "already
    /// installed": `ensure_cargo_dependencies` leaves an existing key alone,
    /// so the old pin would stay while this series' mount was written in.
    #[test]
    fn an_incompatible_existing_pin_is_refused_without_editing() {
        let cargo = format!("{SCAFFOLD_CARGO}autumn-admin-plugin = \"0.6.0\"\n");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        let before = std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();

        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("0.6.0"), "{message}");
        assert!(message.contains("0.7.0"), "{message}");

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            cargo
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            before
        );
    }

    #[test]
    fn a_matching_existing_pin_is_not_refused() {
        let cargo = format!("{SCAFFOLD_CARGO}autumn-admin-plugin = \"0.7.0\"\n");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        assert!(plan_add(tmp.path(), admin(), "0.7.0").is_ok());
    }

    #[test]
    fn declared_dependency_version_reads_both_spellings() {
        assert_eq!(
            declared_dependency_version("[dependencies]\nfoo = \"1.2\"\n", "foo").as_deref(),
            Some("1.2")
        );
        assert_eq!(
            declared_dependency_version(
                "[dependencies]\nfoo = { version = \"1.2\", features = [] }\n",
                "foo"
            )
            .as_deref(),
            Some("1.2")
        );
        assert_eq!(
            declared_dependency_version("[dependencies]\nfoo = { path = \"../foo\" }\n", "foo"),
            None
        );
        assert_eq!(declared_dependency_version("[dependencies]\n", "foo"), None);
    }

    /// A crates.io plugin against an unpatched local `autumn-web` links a
    /// SECOND framework copy, so the mount cannot satisfy the local
    /// `AppBuilder`'s traits. Refuse, and say how to fix it.
    #[test]
    fn an_unpatched_local_framework_is_refused() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = { path = \"../autumn\" }\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        assert!(unpatched_local_framework(tmp.path()));

        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::UnpatchedLocalFramework { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("patch.crates-io"), "{err}");

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            cargo
        );
    }

    /// …but a `[patch.crates-io]` entry collapses the two copies back into
    /// one, which is exactly what this repo's own conformance gate does.
    #[test]
    fn a_patched_local_framework_is_allowed() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = { path = \"autumn\" }\n\n\
                     [patch.crates-io]\nautumn-web = { path = \"autumn\" }\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        write(
            &tmp.path().join("autumn/Cargo.toml"),
            "[package]\nname = \"autumn-web\"\nversion = \"0.7.0\"\n",
        );
        assert!(!unpatched_local_framework(tmp.path()));
        assert!(plan_add(tmp.path(), admin(), "0.7.0").is_ok());
    }

    /// Cargo applies one patch per crate: a config overrides the manifest
    /// (and a nearer config an ancestor's). A patch to the app's checkout
    /// that another overrides does not unify the framework.
    #[test]
    fn an_overridden_framework_patch_does_not_count() {
        let cargo = |patch: &str| {
            format!(
                "[package]\nname = \"demo\"\n\n\
                 [dependencies]\nautumn-web = {{ path = \"autumn\" }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ path = \"{patch}\" }}\n"
            )
        };
        let config = |tmp: &tempfile::TempDir, patch: &str| {
            write(
                &tmp.path().join(".cargo/config.toml"),
                &format!("[patch.crates-io]\nautumn-web = {{ path = \"{patch}\" }}\n"),
            );
        };
        // The manifest patches to the app's checkout, but the config wins.
        let overridden = fake_project(SCAFFOLD_MAIN, &cargo("autumn"));
        config(&overridden, "other");
        assert!(unpatched_local_framework(overridden.path()));
        // The config patches to the app's checkout over another manifest patch.
        let effective = fake_project(SCAFFOLD_MAIN, &cargo("other"));
        config(&effective, "autumn");
        assert!(!unpatched_local_framework(effective.path()));
    }

    /// Cargo ignores a member's own `.cargo/config.toml` when it runs from
    /// the workspace root, so a patch found only there does not unify the
    /// framework; the same patch at the root does.
    #[test]
    fn a_member_only_config_patch_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        );
        let app = tmp.path().join("app");
        write(
            &app.join("Cargo.toml"),
            "[package]\nname = \"app\"\n\n\
             [dependencies]\nautumn-web = { path = \"../autumn\" }\n",
        );
        let patch = "[patch.crates-io]\nautumn-web = { path = \"autumn\" }\n";
        write(
            &app.join(".cargo/config.toml"),
            &patch.replace("autumn\"", "../autumn\""),
        );
        assert!(unpatched_local_framework(&app));
        write(&tmp.path().join(".cargo/config.toml"), patch);
        assert!(!unpatched_local_framework(&app));
    }

    /// With no declared or locked version, a first-party install reads the
    /// local checkout's own version, and fails closed when it cannot.
    #[test]
    fn a_first_party_install_reads_the_local_checkout_version() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = { path = \"autumn\" }\n\n\
                     [patch.crates-io]\nautumn-web = { path = \"autumn\" }\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::UnknownAutumnWebVersion { .. }),
            "{err}"
        );
        write(
            &tmp.path().join("autumn/Cargo.toml"),
            "[package]\nname = \"autumn-web\"\nversion = \"0.6.0\"\n",
        );
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(matches!(err, PluginError::Incompatible { .. }), "{err}");
    }

    /// Only the app's workspace root counts: a nearer manifest that is not
    /// it (here, an inner workspace the app names past) neither makes the
    /// framework local nor patches it.
    #[test]
    fn only_the_workspace_root_decides_a_local_framework() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let app = outer.join("inner").join("app");
        write(
            &outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"inner/app\"]\n\n\
             [workspace.dependencies]\nautumn-web = { path = \"../autumn\" }\n",
        );
        write(
            &outer.join("inner/Cargo.toml"),
            "[workspace]\nmembers = []\n\n[patch.crates-io]\nautumn-web = { path = \"../x\" }\n",
        );
        write(
            &app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nworkspace = \"../..\"\n\n\
             [dependencies]\nautumn-web = { workspace = true }\n",
        );
        assert!(unpatched_local_framework(&app));
        // The patch in the real root does count.
        write(
            &outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"inner/app\"]\n\n\
             [workspace.dependencies]\nautumn-web = { path = \"../autumn\" }\n\n\
             [patch.crates-io]\nautumn-web = { path = \"../autumn\" }\n",
        );
        assert!(!unpatched_local_framework(&app));
    }

    /// A listed community crate is refused next to an unpatched local
    /// framework too, whatever the lockfile says.
    #[test]
    fn a_community_install_refuses_an_unpatched_local_framework() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = { path = \"../autumn\" }\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let err = plan_add_community(tmp.path(), "autumn-plugin-x", "=0.3.0").unwrap_err();
        assert!(
            matches!(err, PluginError::UnpatchedLocalFramework { .. }),
            "{err}"
        );
    }

    /// A renamed local framework is still local: `aw = { package =
    /// "autumn-web", path = … }`.
    #[test]
    fn a_renamed_local_framework_is_detected() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\naw = { package = \"autumn-web\", path = \"../autumn\" }\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        assert!(unpatched_local_framework(tmp.path()));
    }

    /// A patch that supplies another version than the local checkout does
    /// not collapse the two copies: Cargo skips it.
    #[test]
    fn a_patch_of_another_framework_version_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        let framework = |dir: &str, version: &str| {
            write(
                &tmp.path().join(dir).join("Cargo.toml"),
                &format!("[package]\nname = \"autumn-web\"\nversion = \"{version}\"\n"),
            );
        };
        framework("autumn", "0.7.0");
        framework("other", "0.8.0");
        let manifest = |patch: &str| {
            format!(
                "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = {{ path = \"../autumn\" }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ path = \"../{patch}\" }}\n"
            )
        };
        write(&app.join("Cargo.toml"), &manifest("other"));
        assert!(unpatched_local_framework(&app));
        write(&app.join("Cargo.toml"), &manifest("autumn"));
        assert!(!unpatched_local_framework(&app));
    }

    /// A patch to another checkout of the same version is another package
    /// to Cargo: it does not collapse the app's own copy.
    #[test]
    fn a_patch_to_another_checkout_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        for fork in ["fork-a", "fork-b"] {
            write(
                &tmp.path().join(fork).join("Cargo.toml"),
                "[package]\nname = \"autumn-web\"\nversion = \"0.7.0\"\n",
            );
        }
        let manifest = |patch: &str| {
            format!(
                "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = {{ path = \"../fork-a\" }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ path = \"{patch}\" }}\n"
            )
        };
        write(&app.join("Cargo.toml"), &manifest("../fork-b"));
        assert!(unpatched_local_framework(&app));
        // Another spelling of the same checkout is the same package.
        write(&app.join("Cargo.toml"), &manifest("../app/../fork-a"));
        assert!(!unpatched_local_framework(&app));
    }

    /// A git patch counts only for the same repository and ref.
    #[test]
    fn a_git_patch_must_match_the_apps_repository() {
        let manifest = |patch: &str| {
            format!(
                "[package]\nname = \"demo\"\n\n[dependencies]\n\
                 autumn-web = {{ git = \"https://example.com/fork-a\", tag = \"v0.7.0\" }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ {patch} }}\n"
            )
        };
        let other = fake_project(
            SCAFFOLD_MAIN,
            &manifest("git = \"https://example.com/fork-b\", tag = \"v0.7.0\""),
        );
        assert!(unpatched_local_framework(other.path()));
        let other_ref = fake_project(
            SCAFFOLD_MAIN,
            &manifest("git = \"https://example.com/fork-a\", tag = \"v0.6.0\""),
        );
        assert!(unpatched_local_framework(other_ref.path()));
        let same = fake_project(
            SCAFFOLD_MAIN,
            &manifest("git = \"https://example.com/fork-a.git\", tag = \"v0.7.0\""),
        );
        assert!(!unpatched_local_framework(same.path()));
    }

    /// An inherited path resolves from the workspace root only, even when
    /// the same relative path also exists under the member.
    #[test]
    fn an_inherited_framework_path_resolves_from_the_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        let app = root.join("app");
        for dir in [root.join("autumn"), app.join("autumn")] {
            write(
                &dir.join("Cargo.toml"),
                "[package]\nname = \"autumn-web\"\nversion = \"0.7.0\"\n",
            );
        }
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nautumn-web = { path = \"autumn\" }\n",
        );
        let manifest = |patch: &str| {
            format!(
                "[package]\nname = \"app\"\n\n[dependencies]\nautumn-web = {{ workspace = true }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ path = \"{patch}\" }}\n"
            )
        };
        // A member's own [patch] is ignored; the root's decides. Point the
        // root's patch at the member's accidental checkout: not the one used.
        write(&app.join("Cargo.toml"), &manifest("autumn"));
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nautumn-web = { path = \"autumn\" }\n\n\
             [patch.crates-io]\nautumn-web = { path = \"app/autumn\" }\n",
        );
        assert!(unpatched_local_framework(&app));
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nautumn-web = { path = \"autumn\" }\n\n\
             [patch.crates-io]\nautumn-web = { path = \"autumn\" }\n",
        );
        assert!(!unpatched_local_framework(&app));
    }

    /// The crates.io index URL with `.git`, a trailing `/` or other case is
    /// still crates.io: Cargo 1.98 applies such a patch to crates.io deps.
    #[test]
    fn a_crates_io_patch_under_another_spelling_is_a_redirect() {
        for source in [
            "\"https://github.com/rust-lang/crates.io-index.git\"",
            "\"https://github.com/rust-lang/crates.io-index/\"",
            "\"https://github.com/Rust-Lang/Crates.io-Index\"",
            "\"sparse+https://index.crates.io/\"",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write(
                &tmp.path().join("Cargo.toml"),
                &format!(
                    "[package]\nname = \"a\"\n\n[patch.{source}]\n\
                     autumn-plugin-x = {{ path = \"../x\" }}\n"
                ),
            );
            assert!(
                patched_by_in(tmp.path(), "autumn-plugin-x", "=0.3.0", None).is_some(),
                "{source}"
            );
        }
    }

    /// Cargo reads `source.crates-io.*` from the environment too.
    #[test]
    fn an_environment_crates_io_replacement_is_a_redirect() {
        let vars = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)))
                .collect::<Vec<_>>()
                .into_iter()
        };
        let found = env_source_redirect(vars(&[
            ("PATH", "/usr/bin"),
            ("CARGO_SOURCE_CRATES_IO_REPLACE_WITH", "vendor"),
        ]))
        .unwrap();
        assert!(
            found.contains("CARGO_SOURCE_CRATES_IO_REPLACE_WITH"),
            "{found}"
        );
        assert_eq!(
            env_source_redirect(vars(&[("CARGO_SOURCE_CRATES_IO_REPLACE_WITH", "")])),
            None
        );
        assert_eq!(
            env_source_redirect(vars(&[("CARGO_HOME", "/x"), ("CARGO_TARGET_DIR", "/t")])),
            None
        );
        // A non-UTF-8 variable, unrelated or not, is read without a panic; a
        // non-UTF-8 value still counts as set.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let junk = std::ffi::OsString::from_vec(vec![0xff, 0xfe]);
            let env = vec![
                (junk.clone(), junk.clone()),
                ("CARGO_SOURCE_CRATES_IO_REPLACE_WITH".into(), junk),
            ];
            assert!(env_source_redirect(env.into_iter()).is_some());
        }
    }

    /// An alternate registry's framework is another package than crates.io's.
    #[test]
    fn an_alternate_registry_framework_is_not_crates_io() {
        for key in [
            "registry = \"private\"",
            "registry-index = \"sparse+https://r.example/\"",
        ] {
            let cargo = format!(
                "[package]\nname = \"demo\"\n\n[dependencies]\n\
                 autumn-web = {{ version = \"0.7\", {key} }}\n"
            );
            let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
            assert!(unpatched_local_framework(tmp.path()), "{key}");
        }
    }

    /// A registry patch counts only when its version admits the app's.
    #[test]
    fn a_registry_patch_must_supply_the_apps_version() {
        let cargo = |patch: &str| {
            format!(
                "[package]\nname = \"demo\"\n\n[dependencies]\n\
                 autumn-web = {{ version = \"0.7\", registry = \"private\" }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ version = \"{patch}\", registry = \"private\" }}\n"
            )
        };
        let other = fake_project(SCAFFOLD_MAIN, &cargo("0.8"));
        assert!(unpatched_local_framework(other.path()));
        let same = fake_project(SCAFFOLD_MAIN, &cargo("0.7"));
        assert!(!unpatched_local_framework(same.path()));
        // Unlocked, the patch must admit exactly what the app's requirement
        // does: a wider patch may resolve to a newer release.
        let wider = fake_project(SCAFFOLD_MAIN, &cargo(">=0.7"));
        assert!(unpatched_local_framework(wider.path()));
    }

    /// A locked app is checked at the version it resolved, not its floor: a
    /// range locked at 0.8 is not unified by a patch that supplies 0.7.
    #[test]
    fn a_registry_patch_must_supply_the_locked_version() {
        let cargo = |patch: &str| {
            format!(
                "[package]\nname = \"demo\"\n\n[dependencies]\n\
                 autumn-web = {{ version = \">=0.7, <0.9\", registry = \"private\" }}\n\n\
                 [patch.crates-io]\nautumn-web = {{ version = \"{patch}\", registry = \"private\" }}\n"
            )
        };
        let locked = |tmp: &tempfile::TempDir| {
            std::fs::write(
                tmp.path().join("Cargo.lock"),
                "version = 4\n\n[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\n\
                 dependencies = [\"autumn-web\"]\n\n\
                 [[package]]\nname = \"autumn-web\"\nversion = \"0.8.2\"\n",
            )
            .unwrap();
        };
        let floor = fake_project(SCAFFOLD_MAIN, &cargo("0.7"));
        locked(&floor);
        assert!(unpatched_local_framework(floor.path()));
        let resolved = fake_project(SCAFFOLD_MAIN, &cargo("0.8"));
        locked(&resolved);
        assert!(!unpatched_local_framework(resolved.path()));
        // Unlocked, a range cannot be matched to one patch.
        let unlocked = fake_project(SCAFFOLD_MAIN, &cargo("0.8"));
        assert!(unpatched_local_framework(unlocked.path()));
    }

    #[test]
    fn requirement_bounds_follow_cargo() {
        let v = |major, minor, patch| semver::Version::new(major, minor, patch);
        let max = u64::MAX;
        for (req, low, high) in [
            ("0.7", v(0, 7, 0), v(0, 7, max)),
            ("^0.7.3", v(0, 7, 3), v(0, 7, max)),
            ("~0.7", v(0, 7, 0), v(0, 7, max)),
            ("=0.7.3", v(0, 7, 3), v(0, 7, 3)),
            ("=0.7", v(0, 7, 0), v(0, 7, max)),
            ("1.2", v(1, 2, 0), v(1, max, max)),
            ("~1", v(1, 0, 0), v(1, max, max)),
            ("0", v(0, 0, 0), v(0, max, max)),
            ("0.0", v(0, 0, 0), v(0, 0, max)),
            ("0.0.3", v(0, 0, 3), v(0, 0, 3)),
        ] {
            assert_eq!(requirement_bounds(req), Some((low, high)), "{req}");
        }
        for req in [">=0.7", ">=0.7, <0.9", "*", "0.7.*", "1.0.0-rc.1"] {
            assert_eq!(requirement_bounds(req), None, "{req}");
        }
    }

    /// An unlocked range that excludes this release refuses the install.
    #[test]
    fn a_range_that_excludes_the_release_is_refused() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = \">=0.8, <0.9\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(matches!(err, PluginError::Incompatible { .. }), "{err}");
        // Unlocked, a range that admits the release and another series is
        // refused: Cargo may build the app on 0.8 and the plugin on 0.7.
        let wide = fake_project(
            SCAFFOLD_MAIN,
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \">=0.6, <0.9\"\n",
        );
        let err = plan_add(wide.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::UnresolvedAutumnWebRange { .. }),
            "{err}"
        );
        // A range inside the release's series proceeds, even one above the
        // release: the plugin's caret `autumn-web` requirement admits it.
        for range in [">=0.7.0, <0.8", ">=0.7.1, <0.8"] {
            let narrow = fake_project(
                SCAFFOLD_MAIN,
                &format!(
                    "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"{range}\"\n"
                ),
            );
            assert!(plan_add(narrow.path(), admin(), "0.7.0").is_ok(), "{range}");
        }
    }

    #[test]
    fn meets_series_reads_the_release_series() {
        for app in [
            ">=0.7.1, <0.8",
            ">=0.7.0, <0.8",
            ">=0.6, <0.9",
            ">0.7.2, <0.7.9",
            "<0.7.3",
        ] {
            assert!(meets_series(app, "0.7.0"), "{app}");
        }
        for app in [">=0.8, <0.9", "<0.7.0", ">=0.6, <0.7", ">0.7"] {
            assert!(!meets_series(app, "0.7.0"), "{app}");
        }
    }

    #[test]
    fn spans_series_reads_the_neighbouring_series() {
        for app in [
            ">=0.6, <0.9",
            ">=0.7, <0.9",
            ">=0.6.5, <0.8",
            "0",
            "*",
            ">=0.7",
        ] {
            assert!(spans_series(app, "0.7.0"), "{app}");
        }
        for app in [
            "0.7",
            "~0.7",
            "=0.7.0",
            "0.7.3",
            ">=0.7.0, <0.8",
            "0.7.0-alpha.1",
        ] {
            assert!(!spans_series(app, "0.7.0"), "{app}");
        }
        assert!(spans_series(">=1.2, <3", "1.4.0"));
        assert!(spans_series(">=0.9, <1.5", "1.4.0"));
        assert!(!spans_series("1.2", "1.4.0"));
    }

    /// A prerelease framework is outside the stable plugin's caret, whether
    /// or not a listing gate ran first (a delisted or unlisted plugin skips
    /// it).
    #[test]
    fn a_first_party_install_refuses_a_prerelease_framework() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = \"=0.7.0-alpha.1\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(matches!(err, PluginError::Incompatible { .. }), "{err}");
        // The plugin's own prerelease is its release.
        let same = fake_project(SCAFFOLD_MAIN, cargo);
        assert!(plan_add(same.path(), admin(), "0.7.0-alpha.1").is_ok());
    }

    /// The locked framework version decides a first-party install when the
    /// declaration still admits it: a broad range locked at 0.8 is 0.8.
    #[test]
    fn a_first_party_install_reads_the_locked_framework() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = \">=0.7, <0.9\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        std::fs::write(
            tmp.path().join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             dependencies = [\"autumn-web\"]\n\n\
             [[package]]\nname = \"autumn-web\"\nversion = \"0.8.0\"\n",
        )
        .unwrap();
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(matches!(err, PluginError::Incompatible { .. }), "{err}");
    }

    /// An unversioned runtime edge next to a versioned dev edge is ambiguous:
    /// the versioned one is not the app's version by itself.
    #[test]
    fn an_unversioned_edge_beside_a_versioned_one_is_ambiguous() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [dependencies]\nautumn-web = { path = \"../autumn\" }\n\n\
                     [dev-dependencies]\nautumn-web = \"0.7\"\n\n\
                     [patch.crates-io]\nautumn-web = { path = \"../autumn\" }\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        assert!(mixed_autumn_web_declarations(tmp.path()));
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::AmbiguousAutumnWeb { .. }),
            "{err}"
        );
    }

    /// `paths` overrides in a config can replace any crate.
    #[test]
    fn a_paths_override_is_a_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        write(&app.join("Cargo.toml"), "[package]\nname = \"a\"\n");
        write(
            &app.join(".cargo/config.toml"),
            "paths = [\"../overrides\"]\n",
        );
        let found = patched_by_in(&app, "autumn-plugin-x", "=0.2.5", None).unwrap();
        assert!(found.contains("`paths` overrides"), "{found}");
    }

    /// A virtual workspace's unused default does not make it an app.
    #[test]
    fn an_unused_workspace_default_is_not_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.dependencies]\nautumn-web = \"0.7\"\n",
        );
        assert!(matches!(
            app_autumn_web(tmp.path()),
            Err(PluginError::NoAutumnWeb)
        ));
    }

    /// Target-specific declarations of different versions, with no lock to
    /// settle them, refuse a first-party install.
    #[test]
    fn a_first_party_install_refuses_split_framework_versions() {
        let cargo = "[package]\nname = \"demo\"\n\n\
                     [target.'cfg(windows)'.dependencies]\nautumn-web = \"0.7\"\n\n\
                     [target.'cfg(unix)'.dependencies]\nautumn-web = \"0.8\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::AmbiguousAutumnWeb { .. }),
            "{err}"
        );
    }

    /// A plain registry dependency is not a local checkout.
    #[test]
    fn a_registry_framework_is_not_a_local_checkout() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        assert!(!unpatched_local_framework(tmp.path()));
    }

    // ── AC #2: dependency + mount ────────────────────────────────────────────

    #[test]
    fn dependency_line_is_the_shorthand_form() {
        assert_eq!(
            dependency_line("autumn-admin-plugin", "0.7.0"),
            "autumn-admin-plugin = \"0.7.0\""
        );
    }

    #[test]
    fn plan_add_writes_the_dependency_and_the_mount() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let outcome = plan_add(tmp.path(), admin(), "0.7.0").unwrap();
        let AddOutcome::Installed { plan, steps } = outcome else {
            panic!("expected an installable plan");
        };
        plan.execute(crate::generate::Flags::default()).unwrap();

        let cargo = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("autumn-admin-plugin = \"=0.7.0\""),
            "{cargo}"
        );

        let main_rs = std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main_rs.contains(".plugin(autumn_admin_plugin::AdminPlugin::new())"),
            "{main_rs}"
        );
        assert!(main_rs.contains(".routes(routes![index])"), "{main_rs}");
        assert!(
            steps.iter().any(|s| s.contains("autumn generate admin")),
            "{steps:?}"
        );
    }

    /// The mount must land inside the builder chain, i.e. between
    /// `autumn_web::app()` and the first existing call.
    #[test]
    fn mount_lands_inside_the_builder_chain() {
        let updated = insert_mount(SCAFFOLD_MAIN, admin().mount).expect("anchor");
        let app_at = updated.find("autumn_web::app()").unwrap();
        let mount_at = updated.find(admin().mount_arg).unwrap();
        let routes_at = updated.find(".routes(").unwrap();
        assert!(app_at < mount_at && mount_at < routes_at, "{updated}");
    }

    #[test]
    fn insert_mount_preserves_everything_else() {
        let updated = insert_mount(SCAFFOLD_MAIN, admin().mount).expect("anchor");
        for line in SCAFFOLD_MAIN.lines() {
            assert!(updated.contains(line), "lost line {line:?}");
        }
    }

    /// A helper method named `main` inside an `impl` block is not the entry
    /// point: the mount must anchor to the real top-level `async fn main`,
    /// not to the helper (issue #2381 item 4).
    #[test]
    fn insert_mount_ignores_a_helper_named_main_inside_an_impl_block() {
        let source = "use autumn_web::prelude::*;\n\nimpl Server {\n    async fn main() {\n        let _probe = autumn_web::app();\n    }\n}\n\n#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .routes(routes![index]);\n\n    app.run().await;\n}\n";
        let updated = insert_mount(source, admin().mount).expect("anchor on the real main");
        assert!(mount_present(&updated, admin()));
        let real_main_at = updated.find("#[autumn_web::main]").expect("real main kept");
        let mount_at = updated
            .find(admin().mount.trim_end_matches('\n'))
            .expect("mount spliced");
        assert!(
            mount_at > real_main_at,
            "the mount landed before the real entry point — it was spliced into the impl helper"
        );
    }

    /// With only an `impl`-block helper named `main` and no free-standing
    /// entry point, there is no anchor: the command must take the manual
    /// fallback, not splice into the helper (issue #2381 item 4).
    #[test]
    fn insert_mount_refuses_when_only_an_impl_helper_is_named_main() {
        let source = "use autumn_web::prelude::*;\n\nimpl Server {\n    async fn main() {\n        autumn_web::app()\n    }\n}\n";
        assert!(
            insert_mount(source, admin().mount).is_none(),
            "anchored to a helper that is not the entry point"
        );
    }

    // ── AC #4: idempotency ───────────────────────────────────────────────────

    #[test]
    fn mount_present_ignores_comment_mentions() {
        let commented = "// autumn_admin_plugin::AdminPlugin::new()\nfn main() {}\n";
        assert!(!mount_present(commented, admin()));
        let block = "/*\nautumn_admin_plugin::AdminPlugin::new()\n*/\nfn main() {}\n";
        assert!(!mount_present(block, admin()));
        let real = "fn main() { app.plugin(autumn_admin_plugin::AdminPlugin::new()); }\n";
        assert!(mount_present(real, admin()));
    }

    /// A bare `use` import is not a mount. Counting it as one made `add`
    /// report "already installed" for an app that mounts nothing, with no way
    /// for the user to make the command work.
    #[test]
    fn an_import_alone_is_not_a_mount() {
        let imported = "use autumn_admin_plugin::AdminPlugin;\n\nfn main() {}\n";
        assert!(!mount_present(imported, admin()));
    }

    /// A hand-written mount through an import carries only the bare call, so
    /// the bare probe has to catch it — otherwise `add` splices a SECOND,
    /// default-constructed mount, and `AppBuilder::plugin` keeps that one in
    /// preference to the user's configured instance.
    #[test]
    fn a_hand_written_mount_behind_an_import_is_detected() {
        let src = "use autumn_admin_plugin::AdminPlugin;\n\nfn main() {\n    \
                   app.plugin(AdminPlugin::new().require_role(\"staff\"));\n}\n";
        assert!(mount_present(src, admin()));
    }

    #[test]
    fn second_add_changes_nothing_and_says_so() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let AddOutcome::Installed { plan, .. } = plan_add(tmp.path(), admin(), "0.7.0").unwrap()
        else {
            panic!("expected an installable plan");
        };
        plan.execute(crate::generate::Flags::default()).unwrap();

        let cargo_after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        let main_after = std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();

        let second = plan_add(tmp.path(), admin(), "0.7.0").unwrap();
        assert!(matches!(second, AddOutcome::AlreadyInstalled), "{second:?}");

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            cargo_after
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            main_after
        );
        assert_eq!(
            main_after.matches("AdminPlugin::new()").count(),
            1,
            "duplicate mount: {main_after}"
        );
    }

    /// A half-installed app (dependency by hand, no mount) must still get the
    /// mount rather than being reported as already installed.
    #[test]
    fn a_dependency_without_a_mount_is_completed() {
        let cargo = format!("{SCAFFOLD_CARGO}autumn-admin-plugin = \"0.7.0\"\n");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        let AddOutcome::Installed { plan, .. } = plan_add(tmp.path(), admin(), "0.7.0").unwrap()
        else {
            panic!("expected an installable plan");
        };
        plan.execute(crate::generate::Flags::default()).unwrap();
        let main_rs = std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main_rs.contains(admin().mount_arg), "{main_rs}");
        let cargo_after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            cargo_after.matches("autumn-admin-plugin =").count(),
            1,
            "duplicate dependency: {cargo_after}"
        );
    }

    /// A probe hidden in a string literal is not a mount. Reading one as a
    /// mount made `add` skip the insertion and still report success, leaving
    /// the app running without the plugin it said it installed.
    #[test]
    fn a_probe_inside_a_string_literal_is_not_a_mount() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    \
                   let help = \"paste AdminPlugin::new( into your builder\";\n    \
                   let app = autumn_web::app()\n        .routes(routes![index]);\n}\n";
        assert!(!mount_present(src, admin()));

        let tmp = fake_project(src, SCAFFOLD_CARGO);
        let AddOutcome::Installed { plan, .. } = plan_add(tmp.path(), admin(), "0.7.0").unwrap()
        else {
            panic!("expected an installable plan");
        };
        plan.execute(crate::generate::Flags::default()).unwrap();
        let main_rs = std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main_rs.contains(".plugin(autumn_admin_plugin::AdminPlugin::new())"),
            "{main_rs}"
        );
    }

    /// A dev- or build-dependency is not available to the application target,
    /// so it cannot count as installed: reporting one as complete leaves the
    /// app uncompilable AND declines to add the entry that would fix it.
    #[test]
    fn only_a_normal_dependency_counts_as_installed() {
        let cargo =
            format!("{SCAFFOLD_CARGO}\n[dev-dependencies]\nautumn-admin-plugin = \"0.7.0\"\n");
        assert!(!dependency_present(&cargo, "autumn-admin-plugin"));

        // Mount already present + dev-dependency only ⇒ still an install, not
        // "already installed".
        let mounted = SCAFFOLD_MAIN.replace(
            ".routes(routes![index])",
            ".plugin(autumn_admin_plugin::AdminPlugin::new())\n        .routes(routes![index])",
        );
        let tmp = fake_project(&mounted, &cargo);
        let AddOutcome::Installed { plan, .. } = plan_add(tmp.path(), admin(), "0.7.0").unwrap()
        else {
            panic!("expected an installable plan, not AlreadyInstalled");
        };
        plan.execute(crate::generate::Flags::default()).unwrap();
        let after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        let deps_section = after.split("[dev-dependencies]").next().unwrap();
        assert!(
            deps_section.contains("autumn-admin-plugin ="),
            "the normal dependency must be added: {after}"
        );
    }

    /// A target-gated dependency cannot back the unconditional mount this
    /// command writes: on any build where the predicate is false the crate is
    /// absent, so counting it as installed would report success for an app
    /// that does not compile — and would decline to add the entry that fixes
    /// it.
    #[test]
    fn a_target_gated_dependency_does_not_count_as_installed() {
        let cargo = format!(
            "{SCAFFOLD_CARGO}\n[target.'cfg(windows)'.dependencies]\nautumn-admin-plugin = \"0.7.0\"\n"
        );
        assert!(!dependency_present(&cargo, "autumn-admin-plugin"));
    }

    /// A plugin type in a signature is not a mount. Reading one as a mount
    /// suppressed the real insertion while the command reported success.
    #[test]
    fn a_type_annotation_is_not_a_mount() {
        let src = "fn configure(_: autumn_admin_plugin::AdminPlugin) {}\n\
                   #[autumn_web::main]\nasync fn main() {\n    \
                   let app = autumn_web::app()\n        .routes(routes![index]);\n}\n";
        assert!(!mount_present(src, admin()));
    }

    /// …but a plugin built into a variable and mounted through it IS a mount:
    /// splicing a second, default-constructed one would win the duplicate
    /// check and discard the user's configuration.
    #[test]
    fn a_constructor_bound_to_a_variable_is_a_mount() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    \
                   let configured = AdminPlugin::new().require_role(\"staff\");\n    \
                   let app = autumn_web::app().plugin(configured);\n}\n";
        assert!(mount_present(src, admin()));
    }

    /// A mount split across lines — the shape rustfmt produces, and the shape
    /// this command writes for `autumn-storage-s3` — must still be detected.
    #[test]
    fn a_multi_line_mount_is_detected() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    \
                   let app = autumn_web::app()\n        .plugin(\n            \
                   autumn_search::SearchPlugin::new()\n                \
                   .postgres(),\n        );\n}\n";
        let search = catalog::lookup("autumn-search").expect("search entry");
        assert!(mount_present(src, search));
    }

    /// A helper whose name merely starts with `main` is not the entry point:
    /// splicing into it mounts the plugin where the binary never runs it.
    #[test]
    fn a_helper_named_like_main_is_not_the_entry_point() {
        let src = "async fn main_loop() {\n    let a = autumn_web::app()\n        \
                   .routes(routes![index]);\n}\n\n\
                   #[autumn_web::main]\nasync fn main() {\n    \
                   main_loop().await;\n}\n";
        assert!(insert_mount(src, admin().mount).is_none());
    }

    /// A listed crate is pinned with `=`: a caret would let Cargo resolve a
    /// later, unverified patch release (issue #1625).
    #[test]
    fn a_pinned_community_version_is_written_exactly() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let AddOutcome::DependencyOnly { plan, .. } =
            plan_add_community(tmp.path(), "autumn-plugin-live-feed", "=0.3.1").unwrap()
        else {
            panic!("expected a dependency-only outcome");
        };
        plan.execute(crate::generate::Flags::default()).unwrap();
        let after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            after.contains("autumn-plugin-live-feed = \"=0.3.1\""),
            "{after}"
        );
        assert!(plan_add_community(tmp.path(), "autumn-plugin-x", "=not").is_err());
    }

    /// A community crate never gets its mount written, so a re-run stays
    /// dependency-only rather than claiming a complete install.
    #[test]
    fn a_repeated_community_add_stays_dependency_only() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let AddOutcome::DependencyOnly {
            plan,
            dependency_added,
            ..
        } = plan_add_community(tmp.path(), "autumn-plugin-live-feed", "0.3.1").unwrap()
        else {
            panic!("expected a dependency-only outcome");
        };
        assert!(dependency_added);
        plan.execute(crate::generate::Flags::default()).unwrap();
        let after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();

        let second = plan_add_community(tmp.path(), "autumn-plugin-live-feed", "0.3.1").unwrap();
        let AddOutcome::DependencyOnly {
            plan,
            dependency_added,
            mount_snippet,
            ..
        } = second
        else {
            panic!("a re-run must stay dependency-only, got {second:?}");
        };
        assert!(!dependency_added);
        assert!(
            mount_snippet.contains("LiveFeedPlugin::new()"),
            "{mount_snippet}"
        );
        plan.execute(crate::generate::Flags::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            after,
            "a re-run must change nothing"
        );
    }

    // ── AC #5: safe degradation ──────────────────────────────────────────────

    /// A single-line chain has nowhere to splice a call, so the command must
    /// decline rather than guess.
    #[test]
    fn a_single_line_builder_chain_has_no_anchor() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    autumn_web::app().routes(routes![]).run().await;\n}\n";
        assert!(insert_mount(src, admin().mount).is_none());
    }

    /// A `main.rs` that only mentions the builder inside a doc comment must
    /// not be spliced into the comment.
    #[test]
    fn a_commented_builder_is_not_an_anchor() {
        let src = "//! Quick start:\n//!\n//!     autumn_web::app()\n//!         .run()\n\nfn main() {}\n";
        assert!(insert_mount(src, admin().mount).is_none());
    }

    #[test]
    fn a_customized_main_degrades_to_printed_instructions() {
        let custom = "#[autumn_web::main]\nasync fn main() {\n    bootstrap().await;\n}\n";
        let tmp = fake_project(custom, SCAFFOLD_CARGO);
        let before_cargo = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();

        let outcome = plan_add(tmp.path(), admin(), "0.7.0").unwrap();
        let AddOutcome::Manual {
            dependency_line: dep,
            mount_snippet,
            reason,
            ..
        } = outcome
        else {
            panic!("expected the manual fallback");
        };
        assert_eq!(dep, "autumn-admin-plugin = \"=0.7.0\"");
        assert!(
            mount_snippet.contains("AdminPlugin::new()"),
            "{mount_snippet}"
        );
        assert!(!reason.is_empty());

        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            before_cargo
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            custom
        );
    }

    /// The builder must be found inside `main`'s own body. A `main.rs` that
    /// factors the builder into a helper would otherwise be spliced there —
    /// and for the `autumn-storage-s3` mount, which awaits, splicing into a
    /// synchronous fn does not compile at all.
    #[test]
    fn a_builder_in_a_helper_function_is_not_an_anchor() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    build_app().run().await;\n}\n\n\
                   fn build_app() -> autumn_web::app::AppBuilder {\n    autumn_web::app()\n        \
                   .routes(routes![index])\n}\n";
        assert!(insert_mount(src, admin().mount).is_none());
    }

    /// Same rule, the test-harness shape: splicing into `#[cfg(test)] mod
    /// tests` compiles but never mounts anything in the real binary, and the
    /// probe it leaves behind makes the next `add` report "already installed".
    #[test]
    fn a_builder_in_a_test_module_is_not_an_anchor() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    \
                   autumn_web::app().routes(routes![]).run().await;\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    fn harness() {\n        \
                   let b = autumn_web::app()\n            .routes(routes![]);\n    }\n}\n";
        assert!(insert_mount(src, admin().mount).is_none());
    }

    /// A quick-start snippet inside a raw string in `main` is not a candidate
    /// at all — the scanner masks string contents — so the real chain below it
    /// is still found and spliced.
    #[test]
    fn a_builder_inside_a_string_literal_is_not_a_candidate() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    let doc = r\"\n    \
                   autumn_web::app()\n        .run()\n\";\n    \
                   let app = autumn_web::app()\n        .routes(routes![index]);\n    \
                   app.run().await;\n}\n";
        let updated = insert_mount(src, admin().mount).expect("the real chain");
        // Spliced after the REAL builder line, not the one in the string.
        let doc_at = updated.find("let doc").unwrap();
        let mount_at = updated.find(admin().mount_arg).unwrap();
        let real_at = updated.find("let app = autumn_web::app()").unwrap();
        assert!(doc_at < real_at, "{updated}");
        assert!(real_at < mount_at, "{updated}");
    }

    /// Two REAL builder chains in `main` are genuinely ambiguous: refuse
    /// rather than pick one.
    #[test]
    fn two_real_builders_are_not_an_anchor() {
        let src = "#[autumn_web::main]\nasync fn main() {\n    \
                   let a = autumn_web::app()\n        .routes(routes![index]);\n    \
                   let b = autumn_web::app()\n        .routes(routes![other]);\n    \
                   pick(a, b).run().await;\n}\n";
        assert!(insert_mount(src, admin().mount).is_none());
    }

    /// A comparison or range requirement pins no single version, so it must
    /// not be read as an exact one: `>=0.6` resolves to 0.7 perfectly well and
    /// refusing the install would be a false negative.
    #[test]
    fn range_requirements_are_unknown_not_incompatible() {
        for requirement in [">=0.6", ">=0.7, <0.9", "*", "0.6 || 0.7", "<0.8"] {
            assert_eq!(
                check_compat(requirement, "0.7.0"),
                Compat::Unknown,
                "{requirement}"
            );
        }
    }

    #[test]
    fn exact_and_caret_requirements_still_compare() {
        assert_eq!(check_compat("=0.7.1", "0.7.0"), Compat::Compatible);
        assert_eq!(check_compat("~0.6.0", "0.7.0"), Compat::Incompatible);
    }

    /// AC #3 must hold for a workspace member too: `{ workspace = true }`
    /// resolves against the enclosing workspace rather than silently skipping
    /// the gate.
    #[test]
    fn a_workspace_inherited_dependency_still_gates() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        let member = workspace.path().join("app");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\nautumn-web = { workspace = true }\n",
        )
        .unwrap();
        std::fs::write(member.join("src/main.rs"), SCAFFOLD_MAIN).unwrap();

        assert_eq!(
            app_autumn_web(&member).unwrap(),
            AppAutumnWeb::Version("0.5.0".to_owned())
        );
        let err = plan_add(&member, admin(), "0.7.0").unwrap_err();
        assert!(err.to_string().contains("0.5.0"), "{err}");
    }

    #[test]
    fn implausible_versions_are_never_written_to_a_manifest() {
        assert!(is_plausible_version("0.7.0"));
        assert!(is_plausible_version("1.0.0-rc.1+build.5"));
        assert!(!is_plausible_version("\" }\n[dependencies]\nevil = \"1"));
        assert!(!is_plausible_version(""));
        assert!(!is_plausible_version("latest"));

        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let before = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        let err = plan_add_community(tmp.path(), "autumn-plugin-x", "not a version").unwrap_err();
        assert!(
            matches!(err, PluginError::ImplausibleVersion { .. }),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            before
        );
    }

    /// The manifest edit is queued before the mount, so a mid-execute I/O
    /// failure leaves an inert dependency rather than an uncompilable mount
    /// (issue #2381 item 2).
    #[test]
    fn the_manifest_is_queued_before_the_mount() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let AddOutcome::Installed { plan, .. } = plan_add(tmp.path(), admin(), "0.7.0").unwrap()
        else {
            panic!("expected an installable plan");
        };
        let paths: Vec<_> = plan
            .actions
            .iter()
            .map(|action| action.path().to_path_buf())
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths[0].ends_with("Cargo.toml"), "{paths:?}");
        assert!(paths[1].ends_with("main.rs"), "{paths:?}");
    }

    /// A Cargo.toml with `[workspace]` and no `[package]` — the shape that
    /// passes `app_autumn_web` when it hand-declares `autumn-web`.
    const VIRTUAL_WORKSPACE_CARGO: &str = r#"[workspace]

[dependencies]
autumn-web = "0.7.0"
"#;

    /// `plugin add` must not plan a manifest edit at a virtual workspace root:
    /// there is no `[package]` for the `[dependencies]` section to belong to
    /// (issue #2381 item 3).
    #[test]
    fn plan_add_rejects_a_virtual_workspace_root() {
        let tmp = fake_project(SCAFFOLD_MAIN, VIRTUAL_WORKSPACE_CARGO);
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::NoPackageTable),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            VIRTUAL_WORKSPACE_CARGO,
            "a refused install must leave the manifest byte-identical"
        );
    }

    /// The community path plans a manifest edit without ever reading
    /// `src/main.rs`, so it needs the same guard (issue #2381 item 3).
    #[test]
    fn plan_add_community_rejects_a_virtual_workspace_root() {
        let tmp = fake_project(SCAFFOLD_MAIN, VIRTUAL_WORKSPACE_CARGO);
        let err = plan_add_community(tmp.path(), "autumn-plugin-x", "=0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::NoPackageTable),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
            VIRTUAL_WORKSPACE_CARGO,
            "a refused install must leave the manifest byte-identical"
        );
    }

    /// A real virtual manifest declares `autumn-web` only under
    /// `[workspace.dependencies]`, so `app_autumn_web` alone would report
    /// `NoAutumnWeb`: the package-table guard must run first.
    #[test]
    fn both_add_paths_name_a_real_virtual_workspace_root() {
        const REAL_VIRTUAL_CARGO: &str = "[workspace]\nmembers = [\"app\"]\n\n\
                                          [workspace.dependencies]\nautumn-web = \"0.7.0\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, REAL_VIRTUAL_CARGO);
        let err = plan_add(tmp.path(), admin(), "0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::NoPackageTable),
            "plan_add: {err}"
        );
        let err = plan_add_community(tmp.path(), "autumn-plugin-x", "=0.7.0").unwrap_err();
        assert!(
            matches!(err, PluginError::NoPackageTable),
            "plan_add_community: {err}"
        );
    }

    #[test]
    fn plan_add_outside_a_project_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            plan_add(tmp.path(), admin(), "0.7.0").unwrap_err(),
            PluginError::NotInProject
        ));
    }

    /// Every first-party mount must find the scaffold's anchor — otherwise
    /// `autumn plugin add` degrades on the very app `autumn new` produces.
    #[test]
    fn every_first_party_mount_applies_to_a_fresh_scaffold() {
        for entry in catalog::FIRST_PARTY {
            let updated = insert_mount(SCAFFOLD_MAIN, entry.mount)
                .unwrap_or_else(|| panic!("{}: no anchor", entry.crate_name));
            assert!(
                mount_present(&updated, entry),
                "{}: mount not detected after insertion",
                entry.crate_name
            );
        }
    }
}
