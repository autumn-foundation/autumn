//! Install planning for `autumn plugin add` — pure functions plus one
//! [`emit::Plan`] builder, so `--dry-run` and the Created/Modified output match
//! every other code-touching Autumn command.
//!
//! Every decision that can refuse the install (version gate, missing project,
//! unreadable builder chain) is made *before* a single [`emit::Action`] is
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

/// Parse `MAJOR.MINOR[.PATCH]`, tolerating a leading requirement operator and
/// a pre-release/build suffix. Mirrors `doctor::check_version_compat`'s parser
/// so the two commands can never disagree about what a version means.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let version = version
        .trim()
        .trim_start_matches(['=', '^', '~', '>', '<', ' ']);
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
    let declarations = crate::doctor::autumn_web_declarations_at(root);
    let mut declared = false;
    for declaration in &declarations {
        match declaration {
            crate::doctor::AutumnWebDependency::Version(version) => {
                return Ok(AppAutumnWeb::Version(version.clone()));
            }
            crate::doctor::AutumnWebDependency::WithoutVersion => declared = true,
            // `Inherited` comes back for ANY `{ workspace = true }` entry, not
            // just this crate's — the scan cannot tell them apart by itself —
            // so only the entry actually keyed `autumn-web` counts here.
            crate::doctor::AutumnWebDependency::Inherited(key) => {
                declared |= key == "autumn-web";
            }
            crate::doctor::AutumnWebDependency::Absent
            | crate::doctor::AutumnWebDependency::Unreadable => {}
        }
    }
    if declared {
        Ok(AppAutumnWeb::Unversioned)
    } else {
        Err(PluginError::NoAutumnWeb)
    }
}

/// The `[dependencies]` line `plugin add` writes (and prints).
#[must_use]
pub fn dependency_line(crate_name: &str, version: &str) -> String {
    format!("{crate_name} = \"{version}\"")
}

/// Whether `manifest` already declares `crate_name` in any dependency table.
///
/// Parsed rather than substring-matched: a commented-out line or a mention in
/// a `description` must not make an install look done.
#[must_use]
pub fn dependency_present(manifest: &str, crate_name: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(manifest) else {
        return false;
    };
    ["dependencies", "dev-dependencies", "build-dependencies"]
        .iter()
        .filter_map(|kind| table.get(*kind))
        .filter_map(toml::Value::as_table)
        .any(|deps| deps.contains_key(crate_name))
}

/// Whether `main_rs` already mounts the plugin **in code** — a mention inside
/// a comment (a README snippet pasted into the file) does not count.
///
/// Sharing [`crate::rust_source::for_each_code_line`] with the anchor scan is
/// deliberate: if the two disagreed about what a comment is, a doc-comment
/// mention could suppress both the mount and the warning about it.
#[must_use]
pub fn mount_present(main_rs: &str, probe: &str) -> bool {
    crate::rust_source::for_each_code_line(main_rs, |line, _offset| {
        line.contains(probe).then_some(())
    })
    .is_some()
}

/// Byte offset just past the builder-opening `autumn_web::app()` inside
/// `main()`, or `None` when there is no unambiguous anchor.
///
/// Only a line that *ends* with the opener is an anchor. A one-line chain
/// (`autumn_web::app().routes(…).run().await;`) has no place to splice a call
/// into, and a mention inside a comment is not code at all — in both cases the
/// caller degrades to printed instructions rather than guessing (AC #5).
fn builder_anchor(main_rs: &str) -> Option<usize> {
    let mut seen_main = false;
    crate::rust_source::for_each_code_line(main_rs, |line, offset| {
        if line.trim().contains("async fn main") {
            seen_main = true;
        }
        (seen_main && line.trim_end().ends_with("autumn_web::app()"))
            .then(|| offset + line.trim_end().len())
    })
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

/// Plan the install of `entry` at `version` into the project at `root`.
///
/// Ordering is the contract: the project check and the version gate run before
/// a single [`crate::generate::emit::Action`] exists, and the builder-chain
/// edit is computed (not applied) before the manifest edit is queued — so
/// every refusal leaves the app byte-identical, and no outcome can add a
/// dependency whose mount never landed.
///
/// # Errors
///
/// [`PluginError::NotInProject`], [`PluginError::NoAutumnWeb`],
/// [`PluginError::Incompatible`], or an I/O error reading the manifest.
pub fn plan_add(
    root: &Path,
    entry: &CatalogEntry,
    version: &str,
) -> Result<AddOutcome, PluginError> {
    if let AppAutumnWeb::Version(app_version) = app_autumn_web(root)?
        && check_compat(&app_version, version) == Compat::Incompatible
    {
        return Err(PluginError::Incompatible {
            crate_name: entry.crate_name.to_owned(),
            plugin_version: version.to_owned(),
            supported: supported_range(version),
            app_version,
        });
    }

    let manifest = manifest_path(root);
    let manifest_src = std::fs::read_to_string(&manifest)?;
    let main_path = root.join("src").join("main.rs");
    let main_src = std::fs::read_to_string(&main_path).unwrap_or_default();

    let dependency_installed = dependency_present(&manifest_src, entry.crate_name);
    let already_mounted = mount_present(&main_src, entry.probe);
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
                    dependency_line: dependency_line(entry.crate_name, version),
                    mount_snippet: entry.mount.trim_end_matches('\n').to_owned(),
                    steps: steps_for(entry),
                });
            }
        }
    };

    let mut plan = Plan::new(root);
    let spec = format!("\"{version}\"");
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
/// [`PluginError::NotInProject`], [`PluginError::NoAutumnWeb`], or an I/O
/// error reading the manifest.
pub fn plan_add_community(
    root: &Path,
    crate_name: &str,
    version: &str,
) -> Result<AddOutcome, PluginError> {
    app_autumn_web(root)?;
    let manifest = manifest_path(root);
    let manifest_src = std::fs::read_to_string(&manifest)?;
    let snippet = super::catalog::community_mount_snippet(crate_name)
        .unwrap_or_else(|| "        .plugin(/* see the crate's README */)".to_owned());

    if dependency_present(&manifest_src, crate_name) {
        return Ok(AddOutcome::AlreadyInstalled);
    }
    let mut plan = Plan::new(root);
    let spec = format!("\"{version}\"");
    let updated = crate::generate::model::ensure_cargo_dependencies(
        &manifest_src,
        &[(crate_name, spec.as_str())],
    );
    if updated != manifest_src {
        plan.modify(manifest, updated);
    }
    Ok(AddOutcome::DependencyOnly {
        plan: Box::new(plan),
        dependency_line: dependency_line(crate_name, version),
        mount_snippet: snippet,
    })
}

/// `root`'s `Cargo.toml`.
#[must_use]
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join("Cargo.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(cargo.contains("autumn-admin-plugin = \"0.7.0\""), "{cargo}");

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
        let mount_at = updated.find(admin().probe).unwrap();
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

    // ── AC #4: idempotency ───────────────────────────────────────────────────

    #[test]
    fn mount_present_ignores_comment_mentions() {
        let commented = "// autumn_admin_plugin::AdminPlugin::new()\nfn main() {}\n";
        assert!(!mount_present(commented, admin().probe));
        let block = "/*\nautumn_admin_plugin::AdminPlugin::new()\n*/\nfn main() {}\n";
        assert!(!mount_present(block, admin().probe));
        let real = "fn main() { app.plugin(autumn_admin_plugin::AdminPlugin::new()); }\n";
        assert!(mount_present(real, admin().probe));
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
        assert!(main_rs.contains(admin().probe), "{main_rs}");
        let cargo_after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            cargo_after.matches("autumn-admin-plugin =").count(),
            1,
            "duplicate dependency: {cargo_after}"
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
        assert_eq!(dep, "autumn-admin-plugin = \"0.7.0\"");
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
                mount_present(&updated, entry.probe),
                "{}: mount not detected after insertion",
                entry.crate_name
            );
        }
    }
}
