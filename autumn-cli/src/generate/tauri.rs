//! `autumn generate tauri` — scaffold a Tauri desktop wrapper around an autumn app.
//!
//! Creates a `src-tauri/` sub-project using the **sidecar model**: the autumn
//! server binary is supervised by the Tauri shell, and the webview loads the app
//! from a loopback port chosen at runtime (no hardcoded port, no orphaned process).
//! The entire existing autumn app — routes, Maud/htmx, sessions — runs unmodified;
//! the generator does **not** rewrite handlers or move to a static export.
//!
//! # Runtime dependencies on autumn features
//!
//! The staging scripts build the sidecar with two autumn features that make the
//! packaged desktop app fully self-contained:
//!
//! - **#1119 managed local Postgres** (`autumn-web/managed-pg-bundled`) — embeds the
//!   Postgres binaries so the desktop app needs no separately-installed database.
//!   The sidecar is pointed at a per-app data directory via `AUTUMN_MANAGED_PG_DATA_DIR`.
//! - **#1004 single-binary asset embed** (`autumn-web/embed-assets`) — embeds the
//!   `static/` tree into the release binary so the packaged app has no loose files.
//!
//! # Generated files
//!
//! ```text
//! src-tauri/
//!   tauri.conf.json          — Tauri v2 config (productName, bundle, sidecar ref)
//!   Cargo.toml               — standalone shell crate with its own [workspace]
//!   build.rs                 — calls tauri_build::build()
//!   src/main.rs              — #![windows_subsystem] + calls lib::run()
//!   src/lib.rs               — sidecar lifecycle: bind loopback:0, spawn sidecar,
//!                               poll /health, open webview window, kill on exit
//!   icons/icon.svg           — SVG source (reuse PWA icon if present; replace to customise)
//!   icons/32x32.png          }
//!   icons/128x128.png        } placeholder icons so `cargo tauri build` succeeds
//!   icons/128x128@2x.png     } out-of-the-box; regenerate with `cargo tauri icon`
//!   icons/icon.png           }
//!   icons/icon.ico           — Windows
//!   icons/icon.icns          — macOS
//!   stage-sidecar.sh         — Unix: build sidecar → copy to binaries/
//!   stage-sidecar.ps1        — Windows: same in PowerShell
//!   .gitignore               — /target /binaries /gen
//! ```

use std::path::Path;

use super::emit::Plan;
use super::{GenerateError, ensure_project_root};

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute the file actions for `autumn generate tauri`.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when not at a project root, or
/// [`GenerateError::Config`] if `Cargo.toml` is missing `[package].name`.
pub fn plan_tauri(project_root: &Path) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    let (package_name, package_version, bin_name, has_embed_assets, dep_key) =
        read_package_meta(project_root)?;
    let mut plan = Plan::new(project_root);
    let tauri = project_root.join("src-tauri");

    // Core Tauri project files
    plan.create(
        tauri.join("tauri.conf.json"),
        render_tauri_conf(&package_name, &package_version, &bin_name),
    );
    plan.create(
        tauri.join("Cargo.toml"),
        render_shell_cargo_toml(&package_name),
    );
    plan.create(tauri.join("build.rs"), render_build_rs());
    plan.create(
        tauri.join("src").join("main.rs"),
        render_shell_main_rs(&package_name),
    );
    plan.create(
        tauri.join("src").join("lib.rs"),
        render_shell_lib_rs(&package_name, &bin_name),
    );

    // Icons — shared with the thin-client plan.
    plan_icons(&mut plan, project_root, &tauri)?;

    // Platform-specific Tauri config overlays — Tauri CLI merges them at build/dev time.
    // beforeBuildCommand and beforeDevCommand live here so tauri.conf.json is
    // host-OS-agnostic and cargo tauri dev also stages the sidecar.
    plan.create(
        tauri.join("tauri.linux.conf.json"),
        render_tauri_linux_conf(),
    );
    plan.create(
        tauri.join("tauri.macos.conf.json"),
        render_tauri_macos_conf(),
    );
    plan.create(
        tauri.join("tauri.windows.conf.json"),
        render_tauri_windows_conf(),
    );

    // Staging scripts
    plan.create(
        tauri.join("stage-sidecar.sh"),
        render_stage_sidecar_sh(&package_name, &bin_name, has_embed_assets, &dep_key),
    );
    plan.create(
        tauri.join("stage-sidecar.ps1"),
        render_stage_sidecar_ps1(&package_name, &bin_name, has_embed_assets, &dep_key),
    );
    plan.create(tauri.join(".gitignore"), render_gitignore());

    Ok(plan)
}

/// Register the icon actions shared by the desktop and thin-client plans:
/// reuse the PWA icon when the user already ran `autumn generate pwa`, then
/// add placeholder raster icons so `cargo tauri build` works immediately.
/// Replace them with proper icons by running: `cargo tauri icon static/icons/icon.svg`
fn plan_icons(plan: &mut Plan, project_root: &Path, tauri: &Path) -> Result<(), GenerateError> {
    let icons_dir = tauri.join("icons");
    let pwa_icon_src = project_root.join("static").join("icons").join("icon.svg");
    if pwa_icon_src.is_file() {
        let contents = std::fs::read_to_string(&pwa_icon_src).map_err(GenerateError::Io)?;
        plan.create_if_absent(icons_dir.join("icon.svg"), contents);
    } else {
        plan.create_if_absent(icons_dir.join("icon.svg"), render_placeholder_icon_svg());
    }
    plan.create_bytes(icons_dir.join("32x32.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("128x128.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("128x128@2x.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("icon.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("icon.ico"), PLACEHOLDER_ICO);
    plan.create_bytes(icons_dir.join("icon.icns"), PLACEHOLDER_ICNS);
    Ok(())
}

// ── Package metadata helper ───────────────────────────────────────────────────

/// Returns `(package_name, version, bin_name)`.
///
/// `bin_name` is the Cargo binary target name used for the sidecar — it
/// matches the filename `cargo build` writes under `target/.../release/`.
/// When no `[[bin]]` section is declared, Cargo defaults to `src/main.rs`
/// with the same name as the package, so `bin_name == package_name` in that
/// common case.  When `[[bin]] name = "…"` is set, Cargo writes that name
/// instead, so staging scripts and Tauri's `.sidecar()` call must use it.
///
/// `version` resolves workspace inheritance: if `[package] version.workspace
/// = true` the function walks up the directory tree to find
/// `[workspace.package] version` in an ancestor `Cargo.toml`.
///
/// `has_embed_assets_feature` is `true` when the app's `Cargo.toml` declares
/// an `embed-assets` entry under `[features]`.  When true, staging scripts
/// pass `--features embed-assets` (the app-crate feature) rather than
/// `--features autumn-web/embed-assets` (dep path only), so that the app's
/// `#[cfg(feature = "embed-assets")]` guard on `.embedded_static()` is
/// satisfied — mirroring what `autumn build --embed` does.
/// Walk ancestor `Cargo.toml` files to find the `package` field of a
/// workspace-inherited dependency entry.
///
/// When a member has `autumn_web = { workspace = true }`, the `package` alias
/// is recorded in `[workspace.dependencies]` of an ancestor, not the member.
/// Returns `None` when the dep key is not found or has no `package` field.
fn resolve_workspace_dep_package(project_root: &Path, dep_key: &str) -> Option<String> {
    let mut dir: Option<&Path> = Some(project_root);
    while let Some(d) = dir {
        let cargo = d.join("Cargo.toml");
        if cargo.is_file()
            && let Ok(content) = std::fs::read_to_string(&cargo)
            && let Ok(ws_doc) = toml::from_str::<toml::Value>(&content)
        {
            if let Some(pkg_name) = ws_doc
                .get("workspace")
                .and_then(|w| w.get("dependencies"))
                .and_then(|deps| deps.get(dep_key))
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get("package"))
                .and_then(toml::Value::as_str)
            {
                return Some(pkg_name.to_owned());
            }
            // Stop at the workspace root even when the dep is not there.
            if ws_doc.get("workspace").is_some() {
                return None;
            }
        }
        dir = d.parent();
    }
    None
}

/// Find the `[dependencies]` key used to depend on `package_name`.
///
/// Cargo feature syntax requires the *dependency key*, not the package name.
/// An aliased dep like `autumn_web = { package = "autumn-web" }` must be
/// referenced as `autumn_web/feature`, not `autumn-web/feature`.  Handles
/// workspace-inherited deps (`autumn_web = { workspace = true }`) by walking
/// up to the workspace `Cargo.toml` to read the effective `package` there.
/// Returns `package_name` itself when no alias is found.
fn resolve_dep_key(project_root: &Path, doc: &toml::Value, package_name: &str) -> String {
    let Some(deps) = doc.get("dependencies").and_then(toml::Value::as_table) else {
        return package_name.to_owned();
    };
    for (key, val) in deps {
        // Direct dependency: key matches package name.
        if key == package_name {
            return key.clone();
        }
        // Determine the effective package name for this entry.
        let effective_pkg = if val
            .as_table()
            .and_then(|t| t.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            // Workspace-inherited: package alias lives in [workspace.dependencies].
            resolve_workspace_dep_package(project_root, key)
        } else {
            val.as_table()
                .and_then(|t| t.get("package"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        if effective_pkg.as_deref() == Some(package_name) {
            return key.clone();
        }
    }
    package_name.to_owned()
}

/// Resolve the Cargo binary target name for the Tauri sidecar.
///
/// Normalize a Cargo manifest path component-by-component, resolving both `.`
/// (current-dir) and `..` (parent-dir) segments, and accepting both `/` and `\`
/// as separators.  Returns the logical path segments as a `Vec<&str>`.
///
/// This mirrors the normalization Cargo applies when resolving `[[bin]] path =
/// "…"` entries, so that e.g. `"src/../src/main.rs"` and `"./src/main.rs"` both
/// produce `["src", "main.rs"]`.
fn normalize_manifest_path(p: &str) -> Vec<&str> {
    let mut segs: Vec<&str> = Vec::new();
    for seg in p.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                // Only pop when there is a real segment to cancel.  When the stack is
                // empty (or the top is already ".."), the path escapes the package root;
                // preserve the ".." so that "../src/main.rs" stays distinct from
                // "src/main.rs" and is never mistaken for the package main binary.
                if segs.last().is_some_and(|&s| s != "..") {
                    segs.pop();
                } else {
                    segs.push("..");
                }
            }
            s => segs.push(s),
        }
    }
    segs
}

/// Priority when `[[bin]]` entries are present:
///
///   1. `[package] default-run` — the developer's explicit selection.
///   2. An explicit `[[bin]]` whose path resolves to `src/main.rs`.
///   3. `src/main.rs` on disk (only when `autobins != false`).
///   4. Single `[[bin]]` → use it; multiple without `default-run` → error.
///
/// When there are no `[[bin]]` entries:
///
///   5. `default-run` → `src/main.rs` (autobins guard) → `src/bin/` discovery.
fn resolve_bin_name(
    project_root: &Path,
    name: &str,
    default_run: Option<&str>,
    autobins: bool,
    doc: &toml::Value,
) -> Result<String, GenerateError> {
    if let Some(bins) = doc.get("bin").and_then(toml::Value::as_array) {
        if let Some(dr) = default_run {
            return Ok(dr.to_owned());
        }
        let main_bin = bins.iter().find(|b| {
            b.get("path").and_then(|p| p.as_str()).is_some_and(|p| {
                // Fully normalize the path so that variants like `src/../src/main.rs`
                // or `./src/main.rs` all compare equal to `src/main.rs`.  We process
                // components into a stack so that `..` properly pops its parent, just
                // as Cargo does when resolving [[bin]] paths in the manifest.
                //
                // For absolute paths (Cargo permits them) strip the project root
                // first so that `/abs/path/to/project/src/main.rs` is treated
                // the same as the relative `src/main.rs`.
                let path = Path::new(p);
                if path.is_absolute() {
                    path.strip_prefix(project_root).is_ok_and(|rel| {
                        let rel_str = rel.to_string_lossy();
                        normalize_manifest_path(rel_str.as_ref()) == ["src", "main.rs"]
                    })
                } else {
                    normalize_manifest_path(p) == ["src", "main.rs"]
                }
            })
        });
        if let Some(n) = main_bin
            .and_then(|b| b.get("name"))
            .and_then(|n| n.as_str())
        {
            return Ok(n.to_owned());
        }
        if autobins && project_root.join("src/main.rs").is_file() {
            return Ok(name.to_owned());
        }
        // Count auto-discovered src/bin/ entries alongside the explicit ones so
        // that a layout like `[[bin]] path="src/server.rs"` + `src/bin/helper.rs`
        // is correctly flagged as ambiguous rather than silently picking the
        // explicit entry.
        let auto_bin_count = if autobins {
            std::fs::read_dir(project_root.join("src/bin")).map_or(0, |entries| {
                entries
                    .filter_map(std::result::Result::ok)
                    .filter(|e| {
                        let p = e.path();
                        p.extension().is_some_and(|x| x == "rs")
                            || (p.is_dir() && p.join("main.rs").is_file())
                    })
                    .count()
            })
        } else {
            0
        };
        if bins.len() + auto_bin_count > 1 {
            let mut names: Vec<&str> = bins
                .iter()
                .filter_map(|b| b.get("name").and_then(|n| n.as_str()))
                .collect();
            names.sort_unstable();
            return Err(GenerateError::Config(format!(
                "ambiguous sidecar target: found multiple binary targets ({}) \
                 and no [package] default-run is set; add `default-run = \"<bin>\"` to \
                 Cargo.toml to select one",
                names.join(", ")
            )));
        }
        return Ok(bins
            .first()
            .and_then(|b| b.get("name"))
            .and_then(|n| n.as_str())
            .map_or_else(|| name.to_owned(), str::to_owned));
    }
    if let Some(dr) = default_run {
        return Ok(dr.to_owned());
    }
    if autobins && project_root.join("src/main.rs").is_file() {
        return Ok(name.to_owned());
    }
    // Only scan src/bin/ when autobins is enabled (the default).  When
    // `autobins = false` is set in [package], Cargo does not auto-discover
    // src/bin/ entries, so scanning it here would return names that Cargo
    // itself would reject in `cargo build --bin <name>`.
    if autobins && let Ok(entries) = std::fs::read_dir(project_root.join("src/bin")) {
        let stems: Vec<String> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rs") {
                    p.file_stem().and_then(|s| s.to_str()).map(str::to_owned)
                } else if p.is_dir() && p.join("main.rs").is_file() {
                    p.file_name().and_then(|s| s.to_str()).map(str::to_owned)
                } else {
                    None
                }
            })
            .collect();
        match stems.len() {
            1 => return Ok(stems.into_iter().next().unwrap()),
            0 => {}
            _ => {
                let mut sorted = stems;
                sorted.sort();
                return Err(GenerateError::Config(format!(
                    "ambiguous sidecar target: found multiple auto-discovered \
                     src/bin/ binaries ({}) and no [package] default-run is set; \
                     add `default-run = \"<bin>\"` to Cargo.toml to select one",
                    sorted.join(", ")
                )));
            }
        }
    }
    Err(GenerateError::Config(format!(
        "no binary target found for package `{name}`: add `src/main.rs`, \
         a `[[bin]]` entry, or `default-run = \"<bin>\"` to Cargo.toml"
    )))
}

fn read_package_meta(
    project_root: &Path,
) -> Result<(String, String, String, bool, String), GenerateError> {
    let cargo_path = project_root.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_path).map_err(GenerateError::Io)?;
    let doc: toml::Value = toml::from_str(&content)
        .map_err(|e| GenerateError::Config(format!("failed to parse Cargo.toml: {e}")))?;
    let pkg = doc
        .get("package")
        .ok_or_else(|| GenerateError::Config("Cargo.toml missing [package].name".to_owned()))?;
    let name = pkg
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::to_owned)
        .ok_or_else(|| GenerateError::Config("Cargo.toml missing [package].name".to_owned()))?;

    // Resolve version, handling workspace inheritance (`version.workspace = true`).
    let version = match pkg.get("version") {
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Table(t))
            if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
        {
            resolve_workspace_version(project_root).unwrap_or_else(|| "0.1.0".to_owned())
        }
        _ => "0.1.0".to_owned(),
    };

    let default_run = pkg
        .get("default-run")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let autobins = pkg
        .get("autobins")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let bin_name = resolve_bin_name(project_root, &name, default_run.as_deref(), autobins, &doc)?;

    // Check whether the app defines an `embed-assets` Cargo feature.
    // `autumn new` generates this; it typically expands to
    // `["autumn-web/embed-assets"]`.  App code gates `.embedded_static()` on
    // `#[cfg(feature = "embed-assets")]`, so the staging script must enable the
    // *app-crate* feature — not just the dep path — to satisfy that guard.
    let has_embed_assets_feature = doc
        .get("features")
        .and_then(toml::Value::as_table)
        .is_some_and(|features| features.contains_key("embed-assets"));

    // Resolve the dependency key for autumn-web.  Apps may alias it (e.g.
    // `autumn_web = { package = "autumn-web" }` or via workspace inheritance);
    // Cargo feature selectors must use the key (`autumn_web/feature`), not the
    // package name.
    let dep_key = resolve_dep_key(project_root, &doc, "autumn-web");

    Ok((name, version, bin_name, has_embed_assets_feature, dep_key))
}

/// Walk from `project_root` upward looking for a `Cargo.toml` that declares
/// `[workspace.package] version = "…"`.  Returns `None` if not found.
fn resolve_workspace_version(project_root: &Path) -> Option<String> {
    let mut dir: Option<&Path> = Some(project_root);
    while let Some(d) = dir {
        let cargo = d.join("Cargo.toml");
        if cargo.is_file()
            && let Ok(content) = std::fs::read_to_string(&cargo)
            && let Ok(doc) = toml::from_str::<toml::Value>(&content)
            && let Some(v) = doc
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
        {
            return Some(v.to_owned());
        }
        dir = d.parent();
    }
    None
}

// ── Content renderers ─────────────────────────────────────────────────────────

/// Bundle identifier: reverse-DNS with underscores replaced by hyphens.
/// Apple's spec allows only alphanumerics, hyphens, and periods — underscores are invalid.
fn derive_identifier(package_name: &str) -> String {
    format!("com.example.{}", package_name.replace('_', "-"))
}

/// Display title: capitalise first letter of each word; split on both '-' and '_'
/// so kebab-case (`my-app` → `My App`) and `snake_case` (`my_app` → `My App`) both work.
fn derive_title(package_name: &str) -> String {
    package_name
        .split(['-', '_'])
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().to_string() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_tauri_conf(package_name: &str, version: &str, bin_name: &str) -> String {
    let identifier = derive_identifier(package_name);
    let title = derive_title(package_name);
    // beforeBuildCommand and beforeDevCommand are declared in the platform-specific
    // overlay files (tauri.linux.conf.json, tauri.macos.conf.json,
    // tauri.windows.conf.json) that Tauri CLI merges at build/dev time.
    // Keeping them out of tauri.conf.json makes the generated scaffold
    // host-OS-agnostic: it stays correct regardless of which OS generated it.
    //
    // Profile config entries always point to src-tauri/configs/, which the staging
    // script (beforeBuildCommand) populates at build time — copying real files or
    // creating empty stubs for profiles that don't yet exist.  An empty TOML file
    // is valid and results in no overrides (AutumnConfig treats it as a no-op).
    // This keeps the resource list in sync regardless of when profile files are created,
    // avoiding the silent loss of production settings when autumn-prod.toml is added
    // after `autumn generate tauri` was run.
    format!(
        r#"{{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "{title}",
  "version": "{version}",
  "identifier": "{identifier}",
  "bundle": {{
    "active": true,
    "targets": "all",
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.png",
      "icons/icon.ico",
      "icons/icon.icns"
    ],
    "externalBin": [
      "binaries/{bin_name}"
    ],
    "resources": {{
      "../autumn.toml": "autumn.toml",
      "configs/autumn-prod.toml": "autumn-prod.toml",
      "configs/autumn-production.toml": "autumn-production.toml",
      "configs/autumn-dev.toml": "autumn-dev.toml",
      "configs/autumn-development.toml": "autumn-development.toml",
      "configs/autumn-staging.toml": "autumn-staging.toml",
      "configs/autumn-test.toml": "autumn-test.toml",
      "configs/credentials": "config/credentials"
    }}
  }},
  "app": {{
    "security": {{
      "csp": null
    }}
  }}
}}
"#
    )
}

/// Platform-specific Tauri config overlays — Tauri CLI merges these at build/dev time.
/// Keeping `beforeBuildCommand` / `beforeDevCommand` here (not in `tauri.conf.json`)
/// means the generated scaffold is host-OS-agnostic.
///
/// `beforeDevCommand` uses the object form with `"wait": true` because Tauri v2 treats
/// a plain string as `{ "wait": false }` for dev commands (designed for long-running dev
/// servers). The staging script must complete before Tauri tries to spawn the sidecar,
/// so we must opt in to blocking behaviour explicitly.
fn render_tauri_linux_conf() -> String {
    r#"{
  "build": {
    "beforeBuildCommand": "bash src-tauri/stage-sidecar.sh",
    "beforeDevCommand": { "script": "bash src-tauri/stage-sidecar.sh", "wait": true }
  }
}
"#
    .to_owned()
}

fn render_tauri_macos_conf() -> String {
    r#"{
  "build": {
    "beforeBuildCommand": "bash src-tauri/stage-sidecar.sh",
    "beforeDevCommand": { "script": "bash src-tauri/stage-sidecar.sh", "wait": true }
  }
}
"#
    .to_owned()
}

fn render_tauri_windows_conf() -> String {
    r#"{
  "build": {
    "beforeBuildCommand": "powershell -ExecutionPolicy Bypass -File src-tauri\\stage-sidecar.ps1",
    "beforeDevCommand": { "script": "powershell -ExecutionPolicy Bypass -File src-tauri\\stage-sidecar.ps1", "wait": true }
  }
}
"#
    .to_owned()
}

fn render_shell_cargo_toml(package_name: &str) -> String {
    let desktop_name = format!("{package_name}-desktop");
    format!(
        r#"[package]
name = "{desktop_name}"
version = "0.0.1"
edition = "2021"

# Standalone workspace so this crate is independent from the autumn app workspace —
# no change to the root Cargo.toml is needed.
[workspace]

[build-dependencies]
tauri-build = {{ version = "2", features = [] }}

[dependencies]
tauri = {{ version = "2", features = [] }}
tauri-plugin-shell = "2"
getrandom = {{ version = "0.2", features = ["std"] }}

[profile.release]
panic = "abort"
codegen-units = 1
lto = true
opt-level = "s"
strip = true
"#
    )
}

fn render_build_rs() -> String {
    "fn main() {\n    tauri_build::build()\n}\n".to_owned()
}

fn render_shell_main_rs(package_name: &str) -> String {
    let lib_name = package_name.replace('-', "_") + "_desktop";
    format!(
        "#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]\n\
         \n\
         fn main() {{\n\
         \x20   {lib_name}::run();\n\
         }}\n"
    )
}

#[allow(clippy::too_many_lines)]
fn render_shell_lib_rs(package_name: &str, bin_name: &str) -> String {
    format!(
        r#"//! Tauri desktop shell for {package_name}.
//!
//! Lifecycle:
//!   1. Bind loopback:0 to find a free ephemeral port (no hardcoded port collision).
//!      Note: there is a brief window between dropping the listener and the sidecar
//!      binding the port; in practice this race is extremely rare on loopback.
//!   2. Spawn the autumn server sidecar with `AUTUMN_SERVER__PORT` set to that port.
//!      `AUTUMN_MANAGED_PG_DATA_DIR` is set to `<app-data-dir>/db` so the managed
//!      Postgres cluster (autumn feature #1119) persists across restarts.
//!      `AUTUMN_MANAGED_PG_ATTACH_URL` is cleared so an inherited attach URL cannot
//!      redirect the sidecar to a foreign cluster instead of the bundled one.
//!   3. Poll GET /health in a background thread until the server is ready (up to 30 s),
//!      then open the webview window pointing at http://127.0.0.1:<port>.
//!      On timeout, the app exits with a non-zero code rather than showing a blank window.
//!   4. On main window close, send SIGTERM for graceful shutdown (so on_shutdown hooks
//!      run, including ManagedPostgresPoolProvider::stop()), then force-kill after 3 s.

use std::net::TcpListener;
use tauri::{{Manager, App}};
use tauri_plugin_shell::{{ShellExt, process::{{CommandChild, CommandEvent}}}};

struct SidecarHandle(std::sync::Mutex<Option<CommandChild>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {{
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(SidecarHandle(std::sync::Mutex::new(None)))
        .setup(|app| setup(app))
        .on_window_event(|window, event| {{
            if let tauri::WindowEvent::Destroyed = event {{
                // Only shut down the sidecar when the main window closes, not on
                // secondary windows (dialogs, settings panels, etc.).
                if window.label() == "main" {{
                    let handle = window.app_handle();
                    if let Some(child) = handle
                        .state::<SidecarHandle>()
                        .0
                        .lock()
                        .unwrap()
                        .take()
                    {{
                        // On Unix: send SIGTERM so autumn's tokio signal handler
                        // runs on_shutdown hooks (including ManagedPostgresPoolProvider
                        // ::stop()), then force-kill after 5 s.
                        // AUTUMN_SERVER__PRESTOP_GRACE_SECS is set to 0 above so the
                        // listener drain is skipped; the full 5 s budget is available
                        // for on_shutdown hooks (e.g. pg_ctl stop -m fast).
                        // On Windows: autumn only handles tokio::signal::ctrl_c()
                        // (CTRL_C_EVENT).  taskkill sends WM_CLOSE/CTRL_CLOSE_EVENT
                        // which autumn does not handle; graceful shutdown via external
                        // signal is not achievable without process-group manipulation,
                        // so force-kill immediately.
                        #[cfg(unix)]
                        let graceful_pid = child.pid();
                        std::thread::spawn(move || {{
                            #[cfg(unix)]
                            {{
                                let _ = std::process::Command::new("kill")
                                    .args(["-TERM", &graceful_pid.to_string()])
                                    .status();
                                std::thread::sleep(std::time::Duration::from_secs(5));
                                let _ = child.kill();
                            }}
                            #[cfg(windows)]
                            {{
                                let _ = child.kill();
                            }}
                        }});
                    }}
                }}
            }}
        }})
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}}

fn setup(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {{
    // 1. Find a free loopback port: bind :0, read the assigned port, then drop
    //    the listener so the autumn server can bind that same address.
    //    Note: there is a brief window between dropping the listener and the sidecar
    //    binding; in practice this race is extremely rare on loopback.
    let port = {{
        let l = TcpListener::bind("127.0.0.1:0")?;
        l.local_addr()?.port()
    }};

    // 2. Persistent per-app data directories.
    //    Use subdirectories so distinct concerns don't share the same root.
    let data_root = app.path().app_data_dir()?;
    // Postgres cluster (#1119) in db/.  Create proactively; the sidecar won't if absent.
    let app_data_dir = data_root.join("db");
    std::fs::create_dir_all(&app_data_dir)?;
    // Local blob storage in blobs/.  Create before the sidecar spawns so we can
    // restrict the directory to owner-only — LocalBlobStore::new/put use
    // create_dir_all/write which inherit the process umask (typically 0755/0644),
    // leaving private uploads readable by other local accounts on multi-user systems.
    let blobs_dir = data_root.join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;
    #[cfg(unix)]
    {{
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&blobs_dir, std::fs::Permissions::from_mode(0o700))?;
    }}
    // Per-install signing secret: autumn requires one in prod mode.  Generate 32
    // random bytes on first launch and persist them so tokens survive restarts.
    let signing_secret = load_or_generate_signing_secret(&data_root)?;

    // autumn.toml is bundled as a Tauri resource (see tauri.conf.json bundle.resources).
    // The sidecar's working directory is set to resource_dir so AutumnConfig finds it.
    //
    // Why CWD and not AUTUMN_MANIFEST_DIR env var:
    //   OsEnv::var("AUTUMN_MANIFEST_DIR") returns the compile-time CARGO_MANIFEST_DIR
    //   set by #[autumn_web::main], overriding the process environment.  That path
    //   doesn't exist on the installed machine, so find_config_file_named() falls back
    //   to PathBuf::from("autumn.toml") — i.e. the current working directory.
    //   Setting CWD to resource_dir makes that CWD fallback find the bundled config.
    let resource_dir = app.path().resource_dir()?;

    // 3. Spawn the autumn server sidecar (built with autumn-web/embed-assets + managed-pg-bundled).
    //    The sidecar() argument is the binary basename matching externalBin in tauri.conf.json.
    let (mut rx, child) = app
        .shell()
        .sidecar("{bin_name}")?
        // Working directory = resource dir so autumn.toml is found via CWD fallback.
        .current_dir(&resource_dir)
        .env("AUTUMN_SERVER__HOST", "127.0.0.1")
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env(
            "AUTUMN_MANAGED_PG_DATA_DIR",
            app_data_dir.to_string_lossy().as_ref(),
        )
        // Redirect local blob storage to a writable per-app location.
        // Default storage.local.root is "target/blobs" — relative to CWD (resource_dir),
        // which is read-only in installed bundles; the app would abort before opening the window.
        // Route blobs to {{app-data-dir}}/blobs where the process always has write access.
        .env(
            "AUTUMN_STORAGE__LOCAL__ROOT",
            data_root.join("blobs").to_string_lossy().as_ref(),
        )
        // Autumn's StorageConfig::backend_plan rejects local-backend configs in prod
        // mode unless this flag is set, aborting startup before the window opens.
        // A loopback-only desktop sidecar is single-user and the storage root is
        // already in app-data, so local storage is safe and intentional here.
        .env("AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION", "true")
        // Clear any inherited attach URL so the sidecar owns its bundled Postgres
        // cluster rather than connecting to a stale or foreign database.
        // ManagedPostgresPoolProvider checks AUTUMN_MANAGED_PG_ATTACH_URL before
        // AUTUMN_MANAGED_PG_DATA_DIR and returns it without starting a local cluster;
        // an empty value is ignored by the provider.
        .env("AUTUMN_MANAGED_PG_ATTACH_URL", "")
        // Override the compile-time manifest dir so config loading reads from
        // the bundled resource dir on all machines, including the developer's
        // machine where the source tree still exists.  autumn's OsEnv::var
        // checks the process env before the #[autumn_web::main] baked-in path
        // when AUTUMN_MANIFEST_DIR is set, so this overrides both the CWD
        // fallback and the macro-injected compile-time value.
        .env(
            "AUTUMN_MANIFEST_DIR",
            resource_dir.to_string_lossy().as_ref(),
        )
        // Encrypted credentials (config/credentials/<profile>.toml.enc) are bundled
        // into resource_dir/config/credentials/ by the staging script.  Autumn loads
        // them automatically when AUTUMN_MANIFEST_DIR is set.  If the app reads
        // secrets via `config.credentials()`, provide the decryption key via either:
        //   • AUTUMN_MASTER_KEY env var (hex string), or
        //   • resource_dir/config/master.key file (hex string, one line).
        // The key file path is `<AUTUMN_MANIFEST_DIR>/config/master.key`; it must
        // be staged alongside the .toml.enc files (do NOT ship it in the installer —
        // deliver it out-of-band, e.g. via OS keychain or a secure download on first
        // launch).  Leaving both absent is safe when the app has no credentials store:
        // autumn returns CredentialsStore::default() when no .toml.enc file is found.
        // Clear any inherited Unix-socket config so the sidecar always binds
        // TCP on the loopback address the probe polls.  Without this, an
        // inherited AUTUMN_SERVER__UNIX_SOCKET or AUTUMN_SERVE_FORCE_UNIX_SOCKET
        // would make the sidecar bind a socket path while the TCP health probe
        // times out and exits.
        .env("AUTUMN_SERVER__UNIX_SOCKET", "")
        .env("AUTUMN_SERVE_FORCE_UNIX_SOCKET", "")
        // The sidecar binds only on loopback (AUTUMN_SERVER__HOST=127.0.0.1), but
        // if the app's production autumn.toml sets trusted_hosts.hosts to the
        // public domain, Autumn's trusted-host middleware would reject the webview's
        // Host: 127.0.0.1 requests with a 400.  Override to allow loopback hosts
        // unconditionally — the server is loopback-only so no external traffic
        // can reach it regardless of this setting.
        .env("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS", "127.0.0.1,localhost")
        // Per-install signing secret so autumn's prod-mode JWT signing always
        // has a valid secret and doesn't abort before binding the HTTP port.
        .env("AUTUMN_SECURITY__SIGNING_SECRET", &signing_secret)
        // Profile selection: in a debug Tauri build (`cargo tauri dev`) the
        // stage-sidecar script always produces a --release sidecar (AUTUMN_IS_DEBUG=0
        // baked in → prod profile), but the developer expects dev config (dev DB,
        // relaxed security, etc.).  Set AUTUMN_ENV=dev so the release sidecar still
        // loads dev settings.  In a release Tauri build (`cargo tauri build`) clear
        // it instead so the sidecar's baked-in AUTUMN_IS_DEBUG=0 selects prod.
        .env("AUTUMN_ENV", if cfg!(debug_assertions) {{ "dev" }} else {{ "" }})
        // Clear AUTUMN_PROFILE regardless — it is the legacy spelling of AUTUMN_ENV
        // and should never be inherited from the calling shell environment.
        .env("AUTUMN_PROFILE", "")
        // Skip the prestop listener-drain on desktop: no load balancer drains
        // connections to the loopback-only sidecar, so the 5-second default grace
        // (server.prestop_grace_secs) only delays managed-Postgres cleanup past
        // the force-kill window.  Setting it to 0 lets on_shutdown hooks (including
        // ManagedPostgresPoolProvider::stop()) run immediately after SIGTERM.
        .env("AUTUMN_SERVER__PRESTOP_GRACE_SECS", "0")
        // The webview loads the app over plain HTTP (http://127.0.0.1:<port>).
        // Autumn's prod profile sets session.secure = true, which emits the
        // `Secure` attribute on session/CSRF/flash cookies.  Browsers never send
        // Secure cookies over non-HTTPS origins, so sessions, auth, and flash
        // messages silently stop working on installed release bundles.
        // Setting secure=false is safe here: the sidecar is loopback-only and
        // no external network can reach it; cookie confidentiality is not at risk.
        .env("AUTUMN_SESSION__SECURE", "false")
        // Clear one-off mode flags inherited from the calling environment.
        // If any of these are set, AppBuilder::run() enters a non-serving mode
        // (asset fingerprinting, route dump, task execution) and exits before
        // binding the HTTP port — leaving the TCP health probe to time out.
        .env("AUTUMN_BUILD_STATIC", "")
        .env("AUTUMN_DUMP_ROUTES", "")
        .env("AUTUMN_LIST_TASKS", "")
        .env("AUTUMN_RUN_TASK", "")
        // ── Opt-in: auto-migrate managed-Postgres on first launch ──────────────
        // If this app wires ManagedPostgresPoolProvider (desktop-bundled Postgres),
        // uncomment the next line so a fresh local cluster is migrated before the
        // first request arrives.  Leave it commented out when the sidecar connects
        // to a remote / shared database — enabling it there would run pending
        // migrations against that shared DB on every desktop client launch.
        // .env("AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION", "true")
        // ───────────────────────────────────────────────────────────────────────
        .spawn()?;
    *app.state::<SidecarHandle>().0.lock().unwrap() = Some(child);

    // 4. Poll for server readiness in a background thread so setup() returns immediately
    //    and the Tauri event loop starts.  Blocking here freezes the UI and can trigger
    //    OS ANR watchdogs on macOS and Windows.
    //    We probe GET /health — the cheap readiness endpoint autumn always registers.
    //    Even if [health].path is customised to a different path, the server still
    //    accepts the TCP connection and returns a fast HTTP response (e.g. 404), which
    //    starts with "HTTP/" and is enough to confirm the server is up and routing.
    //    Using /health instead of / avoids timing out against a slow app root handler
    //    (e.g. a DB-backed dashboard that queries before writing headers).
    let handle = app.handle().clone();
    std::thread::spawn(move || {{
        // Build SocketAddr directly to avoid repeated string formatting and parse() panics.
        let addr = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            port,
        );
        let poll_timeout = std::time::Duration::from_millis(200);
        let mut ready = false;
        // 1500 × 200 ms = 300 s total — matches autumn serve's READY_TIMEOUT_MANAGED_PG.
        // A first-launch managed-Postgres cluster must initialise (pg_ctl init) and then
        // run migrations before serving HTTP; on slow disks this can take several minutes.
        for _ in 0..1500 {{
            // Fail fast: if the sidecar has already terminated (bad bundled config,
            // migration panic, missing runtime dependency, …) there is no point
            // waiting the full 300 s for a TCP connection that will never arrive.
            while let Ok(event) = rx.try_recv() {{
                match event {{
                    CommandEvent::Stdout(line) => {{
                        if let Ok(s) = std::str::from_utf8(&line) {{
                            print!("{{}}", s);
                        }}
                    }}
                    CommandEvent::Stderr(line) => {{
                        if let Ok(s) = std::str::from_utf8(&line) {{
                            eprint!("{{}}", s);
                        }}
                    }}
                    CommandEvent::Terminated(p) => {{
                        eprintln!(
                            "[{package_name}] Sidecar exited before becoming ready \
                             (code={{:?}}, signal={{:?}}) — aborting.",
                            p.code, p.signal
                        );
                        if let Some(c) = handle
                            .state::<SidecarHandle>()
                            .0
                            .lock()
                            .unwrap()
                            .take()
                        {{
                            let _ = c.kill();
                        }}
                        handle.exit(1);
                        return;
                    }}
                    _ => {{}}
                }}
            }}
            if let Ok(mut stream) =
                std::net::TcpStream::connect_timeout(&addr, poll_timeout)
            {{
                // Bound the read so a silent connection doesn't stall the loop.
                let _ = stream.set_read_timeout(Some(poll_timeout));
                use std::io::{{Read, Write}};
                let req =
                    "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
                if stream.write_all(req.as_bytes()).is_ok() {{
                    let mut buf = [0u8; 8];
                    // Any valid HTTP response (200, 301, 401, 404, …) means the server
                    // is up and routing — accept the `HTTP/` prefix regardless of status.
                    if stream.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/") {{
                        ready = true;
                        break;
                    }}
                }}
            }}
            std::thread::sleep(poll_timeout);
        }}
        if !ready {{
            eprintln!(
                "[{package_name}] Server did not become ready within 300 s — exiting."
            );
            // No window has been created yet, so WindowEvent::Destroyed cannot
            // fire.  Kill the sidecar explicitly before exiting so no orphaned
            // server process is left behind.
            if let Some(child) = handle
                .state::<SidecarHandle>()
                .0
                .lock()
                .unwrap()
                .take()
            {{
                let _ = child.kill();
            }}
            handle.exit(1);
            return;
        }}
        if let Err(e) = tauri::WebviewWindowBuilder::new(
            &handle,
            "main",
            tauri::WebviewUrl::External(
                format!("http://127.0.0.1:{{port}}").parse().unwrap(),
            ),
        )
        .title("{package_name}")
        .inner_size(1200.0, 800.0)
        .build()
        {{
            eprintln!("[{package_name}] Failed to open window: {{e}}");
            // The window was never created so Destroyed cannot clean up; kill
            // the sidecar here too.
            if let Some(child) = handle
                .state::<SidecarHandle>()
                .0
                .lock()
                .unwrap()
                .take()
            {{
                let _ = child.kill();
            }}
            handle.exit(1);
        }}
    }});

    Ok(())
}}

/// Generate a 32-byte random signing secret on first launch, persist it to
/// `{{data_root}}/signing_secret.txt`, and return it as a hex string.
/// Autumn requires a signing secret in prod mode to sign JWTs / session tokens.
/// Without this, the release sidecar calls `fail_fast_on_invalid_signing_secret`
/// and exits before binding the HTTP port, leaving the TCP probe to time out.
/// Returns `Err` (and aborts startup) on RNG failure so no predictable all-zero
/// secret is silently accepted.
fn load_or_generate_signing_secret(
    data_root: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {{
    let path = data_root.join("signing_secret.txt");
    if let Ok(s) = std::fs::read_to_string(&path) {{
        let s = s.trim().to_owned();
        if s.len() >= 32 {{
            // Harden permissions on an existing file in case it was created by
            // an older generated shell (0644) or manually without a mode flag.
            // Failure is non-fatal: log and continue with the existing secret.
            #[cfg(unix)]
            {{
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(0o600),
                ) {{
                    eprintln!(
                        "[{package_name}] warning: could not restrict signing_secret.txt \
                         permissions: {{e}}"
                    );
                }}
            }}
            return Ok(s);
        }}
    }}
    let mut bytes = [0u8; 32];
    // Propagate RNG failure — an all-zero secret would be trivially guessable.
    getrandom::getrandom(&mut bytes)?;
    let hex: String = bytes.iter().map(|b| format!("{{b:02x}}")).collect();
    // Write with restricted permissions: signing secrets must not be world-readable.
    // On Unix create the file with mode 0600; on other platforms use the default ACLs.
    // Propagate write failures: a disk-full or permission error must abort startup
    // rather than silently returning an ephemeral secret that rotates every launch.
    #[cfg(unix)]
    {{
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(hex.as_bytes()))?;
    }}
    #[cfg(not(unix))]
    {{
        std::fs::write(&path, &hex)?;
    }}
    Ok(hex)
}}
"#
    )
}

fn render_placeholder_icon_svg() -> String {
    concat!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 512 512\">\n",
        "  <!-- Placeholder app icon. Replace with your own, then run:\n",
        "       cargo tauri icon static/icons/icon.svg -->\n",
        "  <rect width=\"512\" height=\"512\" rx=\"64\" fill=\"#4F7942\"/>\n",
        "  <text x=\"256\" y=\"370\" font-size=\"280\" text-anchor=\"middle\"",
        " font-family=\"system-ui\">&#x1F342;</text>\n",
        "</svg>\n",
    )
    .to_owned()
}

const fn render_stage_configs_sh() -> &'static str {
    r#"# Stage profile config files into src-tauri/configs/ so tauri.conf.json resource
# entries are always satisfiable at bundle time.
# For alias pairs (prod/production, dev/development): AutumnConfig stops at the
# first existing file in its ordered lookup list.  Copy the available file to
# BOTH names so the profile resolves correctly regardless of AUTUMN_ENV spelling,
# avoiding an empty stub from shadowing real config in the other alias.
mkdir -p src-tauri/configs
# Ensure autumn.toml exists at the project root — tauri.conf.json always
# lists it as a bundle resource.  Projects without a config file use
# AutumnConfig defaults; an empty TOML is a valid no-op.
if [ ! -f "autumn.toml" ]; then
    : > autumn.toml
fi
# prod/production alias pair
if [ -f "autumn-prod.toml" ] && [ -f "autumn-production.toml" ]; then
    cp autumn-prod.toml src-tauri/configs/autumn-prod.toml
    cp autumn-production.toml src-tauri/configs/autumn-production.toml
elif [ -f "autumn-prod.toml" ]; then
    cp autumn-prod.toml src-tauri/configs/autumn-prod.toml
    cp autumn-prod.toml src-tauri/configs/autumn-production.toml
elif [ -f "autumn-production.toml" ]; then
    cp autumn-production.toml src-tauri/configs/autumn-prod.toml
    cp autumn-production.toml src-tauri/configs/autumn-production.toml
else
    : > src-tauri/configs/autumn-prod.toml
    : > src-tauri/configs/autumn-production.toml
fi
# dev/development alias pair (same logic)
if [ -f "autumn-dev.toml" ] && [ -f "autumn-development.toml" ]; then
    cp autumn-dev.toml src-tauri/configs/autumn-dev.toml
    cp autumn-development.toml src-tauri/configs/autumn-development.toml
elif [ -f "autumn-dev.toml" ]; then
    cp autumn-dev.toml src-tauri/configs/autumn-dev.toml
    cp autumn-dev.toml src-tauri/configs/autumn-development.toml
elif [ -f "autumn-development.toml" ]; then
    cp autumn-development.toml src-tauri/configs/autumn-dev.toml
    cp autumn-development.toml src-tauri/configs/autumn-development.toml
else
    : > src-tauri/configs/autumn-dev.toml
    : > src-tauri/configs/autumn-development.toml
fi
# Standalone profiles (no aliases)
for f in autumn-staging.toml autumn-test.toml; do
    if [ -f "$f" ]; then
        cp "$f" "src-tauri/configs/$f"
    else
        : > "src-tauri/configs/$f"
    fi
done
# Stage encrypted credentials so apps using `config.credentials()` find them at
# AUTUMN_MANIFEST_DIR/config/credentials/<profile>.toml.enc in the installed bundle.
# The staging directory is always created so the tauri.conf.json resource entry
# is satisfiable at bundle time (an empty dir is a no-op for apps with no credentials).
# Note: decryption at runtime requires the AUTUMN_MASTER_KEY env var (or the
# config/master.key file placed in the resource dir).  See the Tauri section
# of the Autumn docs for recommended key distribution strategies.
# Remove and recreate the staging directory so stale .toml.enc files from a
# previous build (deleted or rotated credentials) are not carried into the
# installer.  Autumn loads any .toml.enc it finds via AUTUMN_MANIFEST_DIR, so
# a stale file from a prior build would silently keep a revoked secret active.
rm -rf src-tauri/configs/credentials
mkdir -p src-tauri/configs/credentials
if [ -d "config/credentials" ]; then
    cp -r config/credentials/. src-tauri/configs/credentials/
fi
"#
}

fn render_stage_sidecar_sh(
    package_name: &str,
    bin_name: &str,
    has_embed_assets: bool,
    dep_key: &str,
) -> String {
    // dep_key is the [dependencies] key for autumn-web; may differ from the
    // package name when the app aliases it (e.g. `autumn_web = { package = ... }`).
    let embed_feature = if has_embed_assets {
        format!("embed-assets,{dep_key}/managed-pg-bundled")
    } else {
        format!("{dep_key}/embed-assets,{dep_key}/managed-pg-bundled")
    };
    // Only fingerprint when the app-level alias exists; `autumn build --embed` passes
    // --features embed-assets (app-level), which fails for apps without that alias.
    // We also pass managed-pg-bundled so apps wiring ManagedPostgresPoolProvider
    // (without a cfg gate) can compile during the fingerprint phase 1.
    let fingerprint = if has_embed_assets {
        format!(
            "autumn build --embed -p {package_name} --bin {bin_name} \
             --features {dep_key}/managed-pg-bundled\n"
        )
    } else {
        String::new()
    };
    let configs = render_stage_configs_sh();
    format!(
        r#"#!/usr/bin/env bash
# Build the autumn server sidecar (embedded assets + managed Postgres) and
# place it in src-tauri/binaries/ for Tauri to bundle.
# Wired into tauri.conf.json > build.beforeBuildCommand.
# Run manually: bash src-tauri/stage-sidecar.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${{BASH_SOURCE[0]}}")" && pwd)"
APP_DIR="$(dirname "$SCRIPT_DIR")"
cd "$APP_DIR"
# TAURI_ENV_TARGET_TRIPLE is set by Tauri for cross-compilation; fall back to host.
TARGET_TRIPLE="${{TAURI_ENV_TARGET_TRIPLE:-$(rustc -Vv | awk '/^host/{{print $2}}')}}";
# Resolve Cargo output dir (CARGO_TARGET_DIR or workspace target/).
TARGET_DIR="${{CARGO_TARGET_DIR:-$(cargo metadata --no-deps --format-version 1 --quiet \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}}"
mkdir -p src-tauri/binaries
{fingerprint}# universal-apple-darwin: build both Darwin slices and lipo them together.
if [ "${{TARGET_TRIPLE}}" = "universal-apple-darwin" ]; then
    for ARCH in x86_64-apple-darwin aarch64-apple-darwin; do
        cargo build --release -p {package_name} --target "$ARCH" --bin {bin_name} \
          --features {embed_feature}
    done
    lipo -create -output "src-tauri/binaries/{bin_name}-universal-apple-darwin" \
      "${{TARGET_DIR}}/x86_64-apple-darwin/release/{bin_name}" \
      "${{TARGET_DIR}}/aarch64-apple-darwin/release/{bin_name}"
    echo "Staged (universal): src-tauri/binaries/{bin_name}-universal-apple-darwin"
else
    cargo build --release -p {package_name} --target "${{TARGET_TRIPLE}}" --bin {bin_name} \
      --features {embed_feature}
    cp "${{TARGET_DIR}}/${{TARGET_TRIPLE}}/release/{bin_name}" \
       "src-tauri/binaries/{bin_name}-${{TARGET_TRIPLE}}"
    echo "Staged: src-tauri/binaries/{bin_name}-${{TARGET_TRIPLE}}"
fi
{configs}"#
    )
}

const fn render_stage_configs_ps1() -> &'static str {
    r#"# Stage profile config files into src-tauri\configs\ so tauri.conf.json resource
# entries are always satisfiable at bundle time.
# For alias pairs (prod/production, dev/development): AutumnConfig stops at the
# first existing file in its ordered lookup list.  Copy the available file to
# BOTH names so the profile resolves correctly regardless of AUTUMN_ENV spelling,
# avoiding an empty stub from shadowing real config in the other alias.
New-Item -ItemType Directory -Force -Path src-tauri\configs | Out-Null
# Ensure autumn.toml exists at the project root — tauri.conf.json always
# lists it as a bundle resource.  Projects without a config file use
# AutumnConfig defaults; an empty TOML is a valid no-op.
if (-not (Test-Path autumn.toml)) {
    New-Item -ItemType File -Force -Path autumn.toml | Out-Null
}
# prod/production alias pair
if ((Test-Path autumn-prod.toml) -and (Test-Path autumn-production.toml)) {
    Copy-Item autumn-prod.toml src-tauri\configs\autumn-prod.toml
    Copy-Item autumn-production.toml src-tauri\configs\autumn-production.toml
} elseif (Test-Path autumn-prod.toml) {
    Copy-Item autumn-prod.toml src-tauri\configs\autumn-prod.toml
    Copy-Item autumn-prod.toml src-tauri\configs\autumn-production.toml
} elseif (Test-Path autumn-production.toml) {
    Copy-Item autumn-production.toml src-tauri\configs\autumn-prod.toml
    Copy-Item autumn-production.toml src-tauri\configs\autumn-production.toml
} else {
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-prod.toml | Out-Null
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-production.toml | Out-Null
}
# dev/development alias pair (same logic)
if ((Test-Path autumn-dev.toml) -and (Test-Path autumn-development.toml)) {
    Copy-Item autumn-dev.toml src-tauri\configs\autumn-dev.toml
    Copy-Item autumn-development.toml src-tauri\configs\autumn-development.toml
} elseif (Test-Path autumn-dev.toml) {
    Copy-Item autumn-dev.toml src-tauri\configs\autumn-dev.toml
    Copy-Item autumn-dev.toml src-tauri\configs\autumn-development.toml
} elseif (Test-Path autumn-development.toml) {
    Copy-Item autumn-development.toml src-tauri\configs\autumn-dev.toml
    Copy-Item autumn-development.toml src-tauri\configs\autumn-development.toml
} else {
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-dev.toml | Out-Null
    New-Item -ItemType File -Force -Path src-tauri\configs\autumn-development.toml | Out-Null
}
# Standalone profiles (no aliases)
foreach ($f in @("autumn-staging.toml", "autumn-test.toml")) {
    if (Test-Path $f) {
        Copy-Item $f "src-tauri\configs\$f"
    } else {
        New-Item -ItemType File -Force -Path "src-tauri\configs\$f" | Out-Null
    }
}
# Stage encrypted credentials so apps using `config.credentials()` find them at
# AUTUMN_MANIFEST_DIR\config\credentials\<profile>.toml.enc in the installed bundle.
# The staging directory is always created so the tauri.conf.json resource entry
# is satisfiable at bundle time (an empty dir is a no-op for apps with no credentials).
# Note: decryption at runtime requires the AUTUMN_MASTER_KEY env var (or the
# config/master.key file placed in the resource dir).  See the Tauri section
# of the Autumn docs for recommended key distribution strategies.
# Remove and recreate the staging directory so stale .toml.enc files from a
# previous build (deleted or rotated credentials) are not carried into the
# installer.  Autumn loads any .toml.enc it finds via AUTUMN_MANIFEST_DIR, so
# a stale file from a prior build would silently keep a revoked secret active.
if (Test-Path "src-tauri\configs\credentials") {
    Remove-Item -Recurse -Force "src-tauri\configs\credentials"
}
New-Item -ItemType Directory -Force -Path "src-tauri\configs\credentials" | Out-Null
if (Test-Path "config\credentials") {
    # Guard against an empty directory: Copy-Item with a wildcard and
    # $ErrorActionPreference = "Stop" throws when there are no matches.
    $credItems = Get-ChildItem "config\credentials" -ErrorAction SilentlyContinue
    if ($credItems) {
        Copy-Item -Recurse -Force "config\credentials\*" "src-tauri\configs\credentials\"
    }
}
"#
}

fn render_stage_sidecar_ps1(
    package_name: &str,
    bin_name: &str,
    has_embed_assets: bool,
    dep_key: &str,
) -> String {
    let embed_feature = if has_embed_assets {
        format!("embed-assets,{dep_key}/managed-pg-bundled")
    } else {
        format!("{dep_key}/embed-assets,{dep_key}/managed-pg-bundled")
    };
    let fingerprint = if has_embed_assets {
        format!(
            "# Fingerprint static/ before the embed compile (mirrors autumn build --embed phases 1-2):\n\
             # compile → write .autumn-manifest.json → the cargo build below embeds it.\n\
             # --features passes managed-pg-bundled so apps wiring ManagedPostgresPoolProvider\n\
             # without a cfg gate can compile during the fingerprint phase.\n\
             autumn build --embed -p {package_name} --bin {bin_name} \
             --features {dep_key}/managed-pg-bundled\n"
        )
    } else {
        String::new()
    };
    let configs = render_stage_configs_ps1();
    format!(
        r#"# Build the autumn server sidecar with embedded assets and managed Postgres,
# then place it in src-tauri\binaries\ for Tauri to bundle.
#
# Run manually: powershell -File src-tauri\stage-sidecar.ps1
# Or set tauri.conf.json > build.beforeBuildCommand to:
#   "powershell -ExecutionPolicy Bypass -File stage-sidecar.ps1"
$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppDir = Split-Path -Parent $ScriptDir
Set-Location $AppDir
# TAURI_ENV_TARGET_TRIPLE is set by `cargo tauri build` for cross-compilation;
# fall back to the host triple when running the script manually.
$TargetTriple = $Env:TAURI_ENV_TARGET_TRIPLE
if (-not $TargetTriple) {{
    $TargetTriple = (rustc -Vv | Select-String "^host").Line.Split()[1]
}}
# Resolve the real Cargo output directory.  Workspace members share the workspace
# root's target\ and CARGO_TARGET_DIR / .cargo/config.toml can redirect it.
$TargetDir = $Env:CARGO_TARGET_DIR
if (-not $TargetDir) {{
    $TargetDir = (cargo metadata --no-deps --format-version 1 --quiet | ConvertFrom-Json).target_directory
}}
{fingerprint}New-Item -ItemType Directory -Force -Path src-tauri\binaries | Out-Null
cargo build --release -p {package_name} --target "$TargetTriple" --bin {bin_name} `
  --features {embed_feature}
Copy-Item "$TargetDir\$TargetTriple\release\{bin_name}.exe" `
          "src-tauri\binaries\{bin_name}-$TargetTriple.exe"
Write-Host "Staged: src-tauri/binaries/{bin_name}-$TargetTriple.exe"
{configs}"#
    )
}

fn render_gitignore() -> String {
    "/target\n/binaries\n/configs\n/gen\n".to_owned()
}

/// Human-readable prerequisites message printed after a successful scaffold.
pub fn render_prerequisites() -> String {
    "\
Required prerequisites for `cargo tauri build`:\n\
\n\
  1. Tauri CLI:\n\
       cargo install tauri-cli --version '^2'\n\
\n\
  2. Platform toolchain:\n\
       Linux:   sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget \\\n\
                  file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev\n\
       macOS:   xcode-select --install\n\
       Windows: Install WebView2 + Visual Studio C++ build tools\n\
\n\
  3. Stage the autumn server sidecar (also wired into beforeBuildCommand /\n\
     beforeDevCommand in the platform-specific overlay files):\n\
       bash src-tauri/stage-sidecar.sh\n\
\n\
  4. Build or develop the desktop app:\n\
       cd src-tauri && cargo tauri build\n\
       cd src-tauri && cargo tauri dev\n\
\n\
  The sidecar is built with autumn-web/embed-assets (#1004) and\n\
  autumn-web/managed-pg-bundled (#1119).  For a DB-backed app the bundled\n\
  Postgres only activates if ManagedPostgresPoolProvider is wired in your\n\
  app's pool configuration (see docs/guide/managed-pg.md).\n\
\n\
  Replace the placeholder icons before shipping:\n\
       cargo tauri icon static/icons/icon.svg   (from the app root)\n"
        .to_owned()
}

// ── Thin-client (mobile) mode — issue #1506 ──────────────────────────────────

/// Compute the file actions for `autumn generate tauri --remote-url <URL>` —
/// the **mobile thin-client** mode: the webview loads a remote HTTPS Autumn
/// server directly (no sidecar binary, no staging scripts, no local database),
/// and a `capabilities/remote-app.json` grants the remote origin access to the
/// notification/biometric/store plugins.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when not at a project root, or
/// [`GenerateError::Config`] when `remote_url` is not an acceptable HTTPS URL
/// (plain `http` is allowed only for `localhost` / `127.0.0.1` / `::1` /
/// `10.0.2.2` dev servers; URLs with embedded userinfo are rejected) or when
/// `Cargo.toml` is missing `[package].name`.
pub fn plan_tauri_thin_client(
    project_root: &Path,
    remote_url: &str,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    let validated = validate_remote_url(remote_url)?;
    // The capability grant is scoped to the URL's *origin* (scheme://host[:port],
    // no path, no trailing slash) — the exact form Tauri matches remote pages
    // against. The webview itself loads the full URL, so a path component
    // (e.g. https://example.com/app) still lands on the right page.
    let remote_origin = validated.origin().ascii_serialization();
    // Everywhere the URL lands in generated source, embed the parser's
    // normalized serialization — it percent-encodes quotes/braces/spaces and
    // strips newlines, so it is always safe inside a Rust string literal or
    // doc comment. The raw flag value is not.
    let normalized_url = validated.as_str();

    let (package_name, package_version, _bin_name, _has_embed_assets, _dep_key) =
        read_package_meta(project_root)?;
    let mut plan = Plan::new(project_root);
    let tauri = project_root.join("src-tauri");

    // Core Tauri project files — no sidecar config, no staging scripts,
    // no per-OS overlay confs: a thin client has nothing to build or stage.
    plan.create(
        tauri.join("tauri.conf.json"),
        render_thin_client_tauri_conf(&package_name, &package_version),
    );
    plan.create(
        tauri.join("Cargo.toml"),
        render_thin_client_cargo_toml(&package_name),
    );
    plan.create(tauri.join("build.rs"), render_build_rs());
    plan.create(
        tauri.join("src").join("main.rs"),
        render_thin_client_main_rs(&package_name),
    );
    plan.create(
        tauri.join("src").join("lib.rs"),
        render_thin_client_lib_rs(&package_name, normalized_url),
    );

    // Capability grant: pages served from the remote origin may invoke the
    // permitted plugin commands. Auto-discovered by tauri-build from
    // src-tauri/capabilities/.
    plan.create(
        tauri.join("capabilities").join("remote-app.json"),
        render_thin_client_capability(&remote_origin),
    );

    // iOS usage-description plist — the biometric plugin hard-requires
    // NSFaceIDUsageDescription; without it the app crashes on first Face ID use.
    plan.create(
        tauri.join("Info.ios.plist"),
        render_thin_client_ios_plist(&package_name),
    );

    // Icons — identical to the desktop path (shared helper): reuse the PWA
    // icon when the user already ran `autumn generate pwa`, placeholder
    // bytes otherwise.
    plan_icons(&mut plan, project_root, &tauri)?;

    // /gen covers the `cargo tauri android init` / `ios init` output.
    plan.create(tauri.join(".gitignore"), render_gitignore());

    Ok(plan)
}

/// Validate a `--remote-url` value: require `https`, allow `http` only for
/// dev hosts (`localhost`, `127.0.0.1`, `::1`, and `10.0.2.2` — the Android
/// emulator's alias for the host machine's loopback), and reject URLs
/// carrying embedded userinfo (`https://user:pass@…`).
fn validate_remote_url(raw: &str) -> Result<url::Url, GenerateError> {
    let parsed = url::Url::parse(raw).map_err(|e| {
        GenerateError::Config(format!(
            "--remote-url must be an https:// URL (got '{raw}'): {e}"
        ))
    })?;
    // Belt and braces: the serialization is interpolated into generated Rust
    // source, so refuse anything the parser somehow leaves unsafe there. The
    // WHATWG serialization percent-encodes quotes and strips control
    // characters, so this should be unreachable — but a parser upgrade must
    // never silently turn into generated-code injection.
    if parsed
        .as_str()
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_ascii_control())
    {
        return Err(GenerateError::Config(format!(
            "--remote-url contains characters that cannot be embedded in generated \
             code (got '{}')",
            raw.escape_debug()
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(GenerateError::Config(format!(
            "--remote-url must not embed userinfo (got '{raw}'): credentials do not \
             belong in the webview URL — use the session/auth handoff patterns in \
             docs/guide/tauri-mobile-thin-client.md instead"
        )));
    }
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" => {
            // Plain http is only acceptable against a local dev server —
            // a shipped mobile app must talk TLS to its remote origin.
            // `Url::host_str` always returns IPv6 hosts bracketed, so only
            // the `[::1]` form can occur. `10.0.2.2` is how an Android
            // emulator reaches the host machine's loopback.
            let host = parsed.host_str().unwrap_or("");
            if matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "10.0.2.2") {
                Ok(parsed)
            } else {
                Err(GenerateError::Config(format!(
                    "--remote-url must be an https:// URL (got '{raw}'): plain http is \
                     allowed only for localhost / 127.0.0.1 / ::1 / 10.0.2.2 \
                     (Android emulator) dev servers"
                )))
            }
        }
        other => Err(GenerateError::Config(format!(
            "--remote-url must be an https:// URL (got '{raw}'): unsupported scheme '{other}'"
        ))),
    }
}

/// Library target name for the thin-client shell crate: `{package_name}_mobile`
/// with hyphens underscored (Cargo library target names cannot contain hyphens).
fn mobile_lib_name(package_name: &str) -> String {
    package_name.replace('-', "_") + "_mobile"
}

/// Bundle identifier for the mobile thin client: reverse-DNS with `-` and `_`
/// stripped from the package name (`demo-app` → `com.example.demoapp`).
///
/// Unlike the desktop derivation ([`derive_identifier`], which hyphenates for
/// Apple), the mobile identifier must satisfy *both* stores: Android forbids
/// hyphens in application ids — `cargo tauri android init` panics on them
/// (tauri-apps/tauri#9707) — and Apple forbids underscores. Alphanumeric
/// segments are valid on both. Users should replace the `com.example.*`
/// placeholder with their real identifier before running `android`/`ios init`.
fn derive_mobile_identifier(package_name: &str) -> String {
    format!("com.example.{}", package_name.replace(['-', '_'], ""))
}

/// `tauri.conf.json` for the thin client: productName/version/identifier and
/// bundle icons only — no `externalBin` (no sidecar binary) and no `resources`
/// (config files live on the remote server, not in the app bundle).
///
/// `withGlobalTauri` is enabled because the pages come from a server-rendered
/// Autumn app with no npm bundler: the injected `window.__TAURI__` global is
/// how those remote pages call the granted plugins.
///
/// `csp` stays `null`: the webview renders remote pages, which carry the
/// server's own Content-Security-Policy headers.
fn render_thin_client_tauri_conf(package_name: &str, version: &str) -> String {
    let identifier = derive_mobile_identifier(package_name);
    let title = derive_title(package_name);
    format!(
        r#"{{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "{title}",
  "version": "{version}",
  "identifier": "{identifier}",
  "bundle": {{
    "active": true,
    "targets": "all",
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.png",
      "icons/icon.ico",
      "icons/icon.icns"
    ]
  }},
  "app": {{
    "withGlobalTauri": true,
    "security": {{
      "csp": null
    }}
  }}
}}
"#
    )
}

/// Standalone shell crate for the mobile thin client. `staticlib`/`cdylib`
/// crate types are required by `cargo tauri android init` / `ios init` — the
/// mobile projects link the Rust core as a library, not a binary.
fn render_thin_client_cargo_toml(package_name: &str) -> String {
    let mobile_name = format!("{package_name}-mobile");
    let lib_name = mobile_lib_name(package_name);
    format!(
        r#"[package]
name = "{mobile_name}"
version = "0.0.1"
edition = "2021"

# Standalone workspace so this crate is independent from the autumn app workspace —
# no change to the root Cargo.toml is needed.
[workspace]

# staticlib (iOS) and cdylib (Android) are required by `cargo tauri ios init` /
# `cargo tauri android init`; rlib keeps the desktop smoke-test build working.
[lib]
name = "{lib_name}"
crate-type = ["staticlib", "cdylib", "rlib"]

[build-dependencies]
tauri-build = {{ version = "2", features = [] }}

[dependencies]
tauri = {{ version = "2", features = [] }}
tauri-plugin-notification = "2"
tauri-plugin-store = "2"

# The biometric plugin only exists on Android/iOS — target-gate it so the
# thin-client shell still builds on desktop for local smoke-testing.
[target.'cfg(any(target_os = "android", target_os = "ios"))'.dependencies]
tauri-plugin-biometric = "2"

[profile.release]
panic = "abort"
codegen-units = 1
lto = true
opt-level = "s"
strip = true
"#
    )
}

fn render_thin_client_main_rs(package_name: &str) -> String {
    let lib_name = mobile_lib_name(package_name);
    format!(
        "#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]\n\
         \n\
         fn main() {{\n\
         \x20   {lib_name}::run();\n\
         }}\n"
    )
}

/// The thin-client `src/lib.rs`: register the native-capability plugins and
/// open the webview directly on the remote HTTPS Autumn server. No sidecar,
/// no port logic, no shutdown supervision — the server lives in the cloud.
fn render_thin_client_lib_rs(package_name: &str, remote_url: &str) -> String {
    let title = derive_title(package_name);
    format!(
        r#"//! Tauri mobile thin-client shell for {package_name}.
//!
//! The webview loads the remote Autumn server at {remote_url} directly —
//! there is no local sidecar binary, no local database, and no staging step.
//! Native device capabilities (notifications, biometric authentication,
//! key-value storage) are exposed to the remote pages through
//! `capabilities/remote-app.json`.
//!
//! Trust model: pages served from the remote origin can invoke every plugin
//! command that capability permits — the origin is fully trusted. Keep the
//! grant scoped to exactly one origin you control and never widen it to a
//! wildcard.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {{
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .setup(|app| {{
            // The biometric plugin only exists on Android/iOS; its dependency
            // is target-gated in Cargo.toml and its registration is gated here
            // so this shell also builds on desktop for local smoke-testing
            // (notifications and the store work there too).
            #[cfg(mobile)]
            app.handle().plugin(tauri_plugin_biometric::init())?;

            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(
                    "{remote_url}"
                        .parse()
                        .expect("remote URL was validated at generate time"),
                ),
            )
            .title("{title}")
            .build()?;
            Ok(())
        }})
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}}
"#
    )
}

/// `capabilities/remote-app.json` — grants pages served from exactly
/// `remote_origin` access to the core APIs and the notification, biometric,
/// and store plugin commands. Never emit a wildcard here: any page the
/// listed origins serve can invoke these device APIs.
///
/// `core:default` is kept (rather than a narrower core subset) because the
/// injected `window.__TAURI__` API relies on the core event/window plumbing
/// it permits; prune it only after verifying the trimmed set on a device.
fn render_thin_client_capability(remote_origin: &str) -> String {
    format!(
        r#"{{
  "identifier": "remote-autumn-app",
  "description": "Allow pages served by the remote Autumn server to use the native device plugins (notifications, biometric authentication, key-value storage).",
  "windows": ["main"],
  "remote": {{
    "urls": ["{remote_origin}"]
  }},
  "permissions": [
    "core:default",
    "notification:default",
    "biometric:default",
    "store:default"
  ]
}}
"#
    )
}

/// Minimal iOS Info plist overlay declaring `NSFaceIDUsageDescription` —
/// required by the biometric plugin before Face ID can be used; iOS kills
/// the app on first Face ID prompt without it.
fn render_thin_client_ios_plist(package_name: &str) -> String {
    let title = derive_title(package_name);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>NSFaceIDUsageDescription</key>
  <string>{title} uses Face ID to unlock your account.</string>
</dict>
</plist>
"#
    )
}

/// Human-readable prerequisites message printed after a successful
/// thin-client scaffold (`autumn generate tauri --remote-url <URL>`).
pub fn render_thin_client_prerequisites(remote_url: &str) -> String {
    // Echo the normalized serialization — the form actually embedded in the
    // scaffold — rather than the raw flag value (which may differ in case or
    // percent-encoding). The URL was already validated at plan time.
    let remote_url =
        url::Url::parse(remote_url).map_or_else(|_| remote_url.to_owned(), |u| u.to_string());
    format!(
        "\
Next steps for the mobile thin client (webview → {remote_url}):\n\
\n\
  1. Tauri CLI:\n\
       cargo install tauri-cli --version '^2'\n\
\n\
  2. Mobile Rust targets:\n\
       Android: rustup target add aarch64-linux-android armv7-linux-androideabi \\\n\
                  i686-linux-android x86_64-linux-android\n\
       iOS:     rustup target add aarch64-apple-ios aarch64-apple-ios-sim\n\
     (Android also needs the Android SDK/NDK; iOS needs Xcode on macOS.)\n\
\n\
  3. Set your real reverse-DNS bundle identifier in src-tauri/tauri.conf.json\n\
     (the scaffold derives a com.example.* placeholder), then initialise the\n\
     mobile projects (writes into src-tauri/gen/, which is .gitignore'd):\n\
       cd src-tauri && cargo tauri android init\n\
       cd src-tauri && cargo tauri ios init\n\
\n\
  4. Develop on a device or emulator:\n\
       cd src-tauri && cargo tauri android dev\n\
       cd src-tauri && cargo tauri ios dev\n\
\n\
  5. Serve your Autumn app at {remote_url} over HTTPS with a valid TLS\n\
     certificate and `session.secure = true` (AUTUMN_SESSION__SECURE=true) in\n\
     production. The capability file src-tauri/capabilities/remote-app.json\n\
     grants exactly that origin access to the native device plugins.\n\
\n\
  6. Replace the placeholder icons before shipping:\n\
       cargo tauri icon static/icons/icon.svg   (from the app root)\n\
\n\
  7. App Store note — Guideline 4.2 (Minimum Functionality) rejects bare\n\
     website wrappers. The scaffolded notification/biometric/store plugin\n\
     integrations exist to make the app genuinely app-like; wire them into\n\
     your pages before submitting. See docs/guide/tauri-mobile-thin-client.md.\n"
    )
}

// ── Placeholder icon bytes ────────────────────────────────────────────────────
// Minimal valid 1×1 RGBA PNG (autumn green #4F7942, opaque).
// Replace with proper icons using: cargo tauri icon static/icons/icon.svg
const PLACEHOLDER_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
    0x00, 0x00, 0x00, 0x0d, // IHDR length = 13
    0x49, 0x48, 0x44, 0x52, // "IHDR"
    0x00, 0x00, 0x00, 0x01, // width = 1
    0x00, 0x00, 0x00, 0x01, // height = 1
    0x08, 0x06, 0x00, 0x00, 0x00, // depth=8, colortype=6(RGBA), compress=filter=interlace=0
    0x1f, 0x15, 0xc4, 0x89, // IHDR CRC
    0x00, 0x00, 0x00, 0x0d, // IDAT length = 13
    0x49, 0x44, 0x41, 0x54, // "IDAT"
    0x78, 0x9c, 0x63, 0xf0, 0xaf, 0x74, 0xfa, 0x0f, 0x00, 0x04, 0x2f, 0x02, 0x0a, // deflate
    0x5e, 0x60, 0x4a, 0x2d, // IDAT CRC
    0x00, 0x00, 0x00, 0x00, // IEND length = 0
    0x49, 0x45, 0x4e, 0x44, // "IEND"
    0xae, 0x42, 0x60, 0x82, // IEND CRC
];

// Minimal ICO wrapping the placeholder PNG.
const PLACEHOLDER_ICO: &[u8] = &[
    0x00, 0x00, 0x01, 0x00, // ICO header: reserved=0, type=1(ICO)
    0x01, 0x00, // image count = 1
    0x00, 0x00, 0x00, 0x00, // width=0(→256), height=0(→256), palette=0, reserved=0
    0x01, 0x00, 0x20, 0x00, // planes=1, bit_count=32
    0x46, 0x00, 0x00, 0x00, // image data size = 70 bytes
    0x16, 0x00, 0x00, 0x00, // image data offset = 22 (6+16)
    // PNG data (same as PLACEHOLDER_PNG, 70 bytes)
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf0, 0xaf, 0x74, 0xfa,
    0x0f, 0x00, 0x04, 0x2f, 0x02, 0x0a, 0x5e, 0x60, 0x4a, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

// Minimal ICNS container wrapping the placeholder PNG as icp6 (PNG icon).
const PLACEHOLDER_ICNS: &[u8] = &[
    0x69, 0x63, 0x6e, 0x73, // "icns" magic
    0x00, 0x00, 0x00, 0x56, // total file size = 86
    0x69, 0x63, 0x70, 0x36, // icon type "icp6" (PNG icon)
    0x00, 0x00, 0x00, 0x4e, // entry size = 78 (8 header + 70 PNG)
    // PNG data (same as PLACEHOLDER_PNG, 70 bytes)
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf0, 0xaf, 0x74, 0xfa,
    0x0f, 0x00, 0x04, 0x2f, 0x02, 0x0a, 0x5e, 0x60, 0x4a, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fmt::Write as FmtWrite;
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::generate::Flags;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn project(name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    fn project_with_custom_bin(pkg_name: &str, bin_name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[[bin]]\nname=\"{bin_name}\"\npath=\"src/main.rs\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    /// Like `project_with_custom_bin` but the [[bin]] path uses a `./` prefix
    /// (e.g. `path = "./src/main.rs"`), which is valid Cargo TOML but must be
    /// normalized before comparing to detect the src/main.rs target.
    fn project_with_dotslash_bin(pkg_name: &str, bin_name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[[bin]]\nname=\"{bin_name}\"\npath=\"./src/main.rs\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    /// Project whose `[[bin]] path` contains a middle `.` component (`src/./main.rs`).
    fn project_with_middle_dot_bin(pkg_name: &str, bin_name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[[bin]]\nname=\"{bin_name}\"\npath=\"src/./main.rs\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    /// Project with multiple `src/bin/` files and no `src/main.rs` or [[bin]] table
    /// and no `default-run`.  Cargo cannot build `--bin <package>` because that name
    /// doesn't exist; the generator must return an error rather than silently using
    /// the package name.
    fn project_with_multi_src_bin(pkg_name: &str, bin_stems: &[&str]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        for stem in bin_stems {
            fs::write(
                tmp.path().join(format!("src/bin/{stem}.rs")),
                "fn main() {}\n",
            )
            .unwrap();
        }
        tmp
    }

    /// Project with src/main.rs (the primary autumn binary, auto-discovered by
    /// Cargo under the package name) plus an auxiliary [[bin]] for a background
    /// worker.  The generator must pick the package-named binary, not the worker.
    fn project_with_aux_bin(pkg_name: &str, worker_name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[[bin]]\nname=\"{worker_name}\"\npath=\"src/worker.rs\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        // Auto-discovered primary binary (NOT listed in [[bin]]).
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        // The explicit auxiliary bin.
        fs::write(tmp.path().join("src/worker.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    /// Project with `[package] default-run`, explicit `[[bin]]` entries for other
    /// binaries, AND `src/main.rs` present.  The generator must honour `default-run`
    /// even when `src/main.rs` exists (which would otherwise be auto-discovered under
    /// the package name).
    fn project_with_default_run_and_main_rs(
        pkg_name: &str,
        default_run: &str,
        extra_bins: &[&str],
    ) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let bin_sections = extra_bins.iter().fold(String::new(), |mut acc, b| {
            write!(acc, "\n[[bin]]\nname=\"{b}\"\npath=\"src/{b}.rs\"\n").unwrap();
            acc
        });
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 default-run=\"{default_run}\"\
                 {bin_sections}\n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        // src/main.rs is present — without the default-run check it would be
        // auto-discovered and the package name would be used instead.
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        for b in extra_bins {
            fs::write(tmp.path().join(format!("src/{b}.rs")), "fn main() {}\n").unwrap();
        }
        tmp
    }

    /// Project with `[package] default-run` and multiple explicit `[[bin]]`
    /// targets but no `src/main.rs`.  The generator must pick the `default-run`
    /// binary, not the first manifest entry.
    fn project_with_default_run(pkg_name: &str, default_run: &str, bins: &[&str]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let bin_sections = bins.iter().fold(String::new(), |mut acc, b| {
            write!(acc, "\n[[bin]]\nname=\"{b}\"\npath=\"src/{b}.rs\"\n").unwrap();
            acc
        });
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 default-run=\"{default_run}\"\n\
                 {bin_sections}\n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        for b in bins {
            fs::write(tmp.path().join(format!("src/{b}.rs")), "fn main() {}\n").unwrap();
        }
        tmp
    }

    /// Project with `autobins = false` and one explicit `[[bin]]`, plus `src/main.rs`.
    /// With autobins disabled, Cargo does NOT auto-discover `src/main.rs`; the
    /// generator must use the explicit bin name, not the package name.
    fn project_with_autobins_false(pkg_name: &str, bin_name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 autobins=false\n\
                 \n[[bin]]\nname=\"{bin_name}\"\npath=\"src/{bin_name}.rs\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        // src/main.rs exists but is NOT a Cargo target because autobins=false.
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            tmp.path().join(format!("src/{bin_name}.rs")),
            "fn main() {}\n",
        )
        .unwrap();
        tmp
    }

    /// Project with multiple explicit `[[bin]]` entries, no `default-run`, and no
    /// `src/main.rs`.  The generator must error rather than silently pick the first bin.
    fn project_with_multiple_bins_no_default(pkg_name: &str, bins: &[&str]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let bin_sections = bins.iter().fold(String::new(), |mut acc, b| {
            write!(acc, "\n[[bin]]\nname=\"{b}\"\npath=\"src/{b}.rs\"\n").unwrap();
            acc
        });
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\
                 {bin_sections}\n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        for b in bins {
            fs::write(tmp.path().join(format!("src/{b}.rs")), "fn main() {}\n").unwrap();
        }
        tmp
    }

    fn project_with_workspace_version(pkg_name: &str, ws_version: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion.workspace = true\nedition=\"2024\"\n\
                 \n[workspace]\n\n[workspace.package]\nversion=\"{ws_version}\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    /// Project that defines an `embed-assets` Cargo feature (as `autumn new`
    /// generates it), mapping to `["autumn-web/embed-assets"]`.
    fn project_with_embed_assets(pkg_name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[features]\nembed-assets = [\"autumn-web/embed-assets\"]\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    /// Project with BOTH `src/main.rs` AND `src/bin/<bin_name>.rs` but NO `[[bin]]`
    /// table — Cargo auto-discovers src/main.rs as the package-named binary; the
    /// generator must prefer that over the src/bin/ stem.
    fn project_with_main_and_src_bin(pkg_name: &str, bin_stem: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            tmp.path().join(format!("src/bin/{bin_stem}.rs")),
            "fn main() {}\n",
        )
        .unwrap();
        tmp
    }

    /// Project with a single `src/bin/<bin_name>.rs` and no `src/main.rs` or
    /// `[[bin]]` — Cargo auto-discovers the binary named after the file stem.
    fn project_with_src_bin(pkg_name: &str, bin_stem: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        fs::write(
            tmp.path().join(format!("src/bin/{bin_stem}.rs")),
            "fn main() {}\n",
        )
        .unwrap();
        tmp
    }

    /// Project with a directory-style `src/bin/<bin_stem>/main.rs` and no
    /// `src/main.rs` or `[[bin]]` — Cargo auto-discovers the binary as `<bin_stem>`.
    fn project_with_src_bin_dir(pkg_name: &str, bin_stem: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname=\"{pkg_name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
                 \n[dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join(format!("src/bin/{bin_stem}"))).unwrap();
        fs::write(
            tmp.path().join(format!("src/bin/{bin_stem}/main.rs")),
            "fn main() {}\n",
        )
        .unwrap();
        tmp
    }

    // ── plan_tauri: error cases ───────────────────────────────────────────────

    #[test]
    fn plan_tauri_requires_project_root() {
        let tmp = TempDir::new().unwrap();
        let err = plan_tauri(tmp.path()).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_tauri_errors_when_package_name_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let err = plan_tauri(tmp.path()).unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    // ── plan_tauri: file list ─────────────────────────────────────────────────

    fn has_action(tmp: &TempDir, suffix: &str) -> bool {
        let plan = plan_tauri(tmp.path()).unwrap();
        plan.actions.iter().any(|a| {
            a.path()
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with(suffix)
        })
    }

    #[test]
    fn plan_creates_tauri_conf_json() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/tauri.conf.json"));
    }

    #[test]
    fn plan_creates_shell_cargo_toml() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/Cargo.toml"));
    }

    #[test]
    fn plan_creates_build_rs() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/build.rs"));
    }

    #[test]
    fn plan_creates_src_main_rs() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/src/main.rs"));
    }

    #[test]
    fn plan_creates_src_lib_rs() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/src/lib.rs"));
    }

    #[test]
    fn plan_creates_stage_sidecar_sh() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/stage-sidecar.sh"));
    }

    #[test]
    fn plan_creates_stage_sidecar_ps1() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/stage-sidecar.ps1"));
    }

    #[test]
    fn stage_sidecar_sh_uses_cargo_metadata_for_target_dir() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        // cargo metadata resolves the real output dir for workspace members and
        // respects CARGO_TARGET_DIR overrides.
        assert!(
            sh.contains("cargo metadata"),
            "stage-sidecar.sh must use `cargo metadata` to locate the Cargo target directory"
        );
        assert!(
            sh.contains("TARGET_DIR"),
            "stage-sidecar.sh must use a TARGET_DIR variable derived from cargo metadata"
        );
        // The hardcoded relative path is no longer used — workspace builds would
        // look in the wrong place if we still had `target/$TARGET_TRIPLE/...`.
        assert!(
            !sh.contains(r#""target/$"#) && !sh.contains("\"target/${"),
            "stage-sidecar.sh must not use a hardcoded `target/` prefix for the copy"
        );
    }

    #[test]
    fn stage_sidecar_sh_handles_universal_apple_darwin() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        // universal-apple-darwin is a Tauri meta-target; cargo build --target
        // universal-apple-darwin fails because rustc doesn't know it.
        assert!(
            sh.contains("universal-apple-darwin"),
            "stage-sidecar.sh must detect universal-apple-darwin and handle it separately"
        );
        assert!(
            sh.contains("lipo"),
            "stage-sidecar.sh must combine Darwin slices with lipo for universal builds"
        );
        assert!(
            sh.contains("x86_64-apple-darwin") && sh.contains("aarch64-apple-darwin"),
            "stage-sidecar.sh must build both x86_64 and aarch64 Darwin slices"
        );
    }

    #[test]
    fn stage_sidecar_ps1_uses_cargo_metadata_for_target_dir() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", true, "autumn-web");
        assert!(
            ps1.contains("cargo metadata"),
            "stage-sidecar.ps1 must use `cargo metadata` to locate the Cargo target directory"
        );
        assert!(
            ps1.contains("TargetDir"),
            "stage-sidecar.ps1 must use a $TargetDir variable derived from cargo metadata"
        );
        assert!(
            ps1.contains("ConvertFrom-Json"),
            "stage-sidecar.ps1 must use ConvertFrom-Json to properly decode the JSON path"
        );
    }

    #[test]
    fn stage_sidecar_sh_stages_profile_configs() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        // Profile configs are staged at build time (beforeBuildCommand) so that the
        // static resource entries in tauri.conf.json are always satisfiable — even when
        // autumn-prod.toml is created after `autumn generate tauri` was run.
        assert!(
            sh.contains("src-tauri/configs"),
            "stage-sidecar.sh must create src-tauri/configs/ and populate it with \
             profile config files (or empty stubs) so tauri.conf.json resource entries resolve"
        );
        assert!(
            sh.contains("autumn-prod.toml"),
            "stage-sidecar.sh must stage autumn-prod.toml into configs/"
        );
    }

    #[test]
    fn stage_sidecar_ps1_stages_profile_configs() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", true, "autumn-web");
        assert!(
            ps1.contains("src-tauri\\configs") || ps1.contains("src-tauri/configs"),
            "stage-sidecar.ps1 must create src-tauri\\configs\\ and populate it with \
             profile config files (or empty stubs)"
        );
        assert!(
            ps1.contains("autumn-prod.toml"),
            "stage-sidecar.ps1 must stage autumn-prod.toml into configs\\"
        );
    }

    // When only one of a prod/production (or dev/development) alias pair exists, the
    // staging script must copy it to BOTH names so an empty stub can never shadow
    // real config because AutumnConfig stops at the first existing file in the lookup list.
    #[test]
    fn stage_sidecar_sh_copies_alias_to_both_names() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        // Must handle the case where only autumn-production.toml exists by copying it
        // to autumn-prod.toml as well — look for the elif + both cp lines.
        assert!(
            sh.contains("autumn-production.toml") && sh.contains("autumn-prod.toml"),
            "stage-sidecar.sh must handle prod/production alias pair explicitly"
        );
        // The alias-pair logic copies to BOTH names from a single source, so each
        // destination path must appear at least twice (once in the both-exist branch
        // and once in the single-file branch).
        assert!(
            sh.matches("autumn-prod.toml").count() >= 2,
            "stage-sidecar.sh must copy to autumn-prod.toml in multiple alias branches"
        );
        assert!(
            sh.contains("autumn-dev.toml") && sh.contains("autumn-development.toml"),
            "stage-sidecar.sh must handle dev/development alias pair explicitly"
        );
    }

    #[test]
    fn stage_sidecar_ps1_copies_alias_to_both_names() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", true, "autumn-web");
        assert!(
            ps1.contains("autumn-production.toml") && ps1.contains("autumn-prod.toml"),
            "stage-sidecar.ps1 must handle prod/production alias pair explicitly"
        );
        assert!(
            ps1.matches("autumn-prod.toml").count() >= 2,
            "stage-sidecar.ps1 must copy to autumn-prod.toml in multiple alias branches"
        );
        assert!(
            ps1.contains("autumn-dev.toml") && ps1.contains("autumn-development.toml"),
            "stage-sidecar.ps1 must handle dev/development alias pair explicitly"
        );
    }

    #[test]
    fn plan_creates_gitignore() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/.gitignore"));
    }

    #[test]
    fn plan_creates_icon_svg() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/icons/icon.svg"));
    }

    #[test]
    fn plan_creates_png_icons() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/icons/32x32.png"));
        assert!(has_action(&tmp, "src-tauri/icons/128x128.png"));
        assert!(has_action(&tmp, "src-tauri/icons/128x128@2x.png"));
        assert!(has_action(&tmp, "src-tauri/icons/icon.png"));
    }

    #[test]
    fn plan_creates_ico_and_icns() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/icons/icon.ico"));
        assert!(has_action(&tmp, "src-tauri/icons/icon.icns"));
    }

    // ── render_tauri_conf ─────────────────────────────────────────────────────

    #[test]
    fn tauri_conf_is_valid_json() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value =
            serde_json::from_str(&conf).expect("tauri.conf.json must be valid JSON");
        assert!(parsed.is_object());
    }

    #[test]
    fn tauri_conf_uses_package_version() {
        let conf = render_tauri_conf("my-app", "1.4.2", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert_eq!(
            parsed["version"].as_str(),
            Some("1.4.2"),
            "tauri.conf.json version must match [package].version from Cargo.toml, \
             not a hardcoded 0.1.0"
        );
    }

    #[test]
    fn tauri_conf_externalbin_uses_bin_name() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-server");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let ext_bin = parsed["bundle"]["externalBin"][0].as_str().unwrap_or("");
        assert!(
            ext_bin.contains("my-server"),
            "externalBin must use the [[bin]] target name, not the package name; got: {ext_bin}"
        );
        assert!(
            !ext_bin.contains("my-app"),
            "externalBin must not use the package name when [[bin]] name differs; got: {ext_bin}"
        );
    }

    #[test]
    fn stage_sidecar_sh_uses_bin_name_for_binary() {
        let sh = render_stage_sidecar_sh("my-app", "my-server", true, "autumn-web");
        assert!(
            sh.contains("my-server"),
            "stage-sidecar.sh must reference the binary target name in copy commands"
        );
        assert!(
            !sh.contains("/release/my-app") && !sh.contains("my-app-$"),
            "stage-sidecar.sh must not use the package name for the compiled binary path \
             when the [[bin]] name differs"
        );
    }

    #[test]
    fn stage_sidecar_ps1_uses_bin_name_for_binary() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-server", true, "autumn-web");
        assert!(
            ps1.contains("my-server"),
            "stage-sidecar.ps1 must reference the binary target name in copy commands"
        );
        assert!(
            !ps1.contains("release\\my-app") && !ps1.contains("my-app-$"),
            "stage-sidecar.ps1 must not use the package name for the compiled binary path \
             when the [[bin]] name differs"
        );
    }

    #[test]
    fn plan_reads_custom_bin_name_from_cargo_toml() {
        let tmp = project_with_custom_bin("my-app", "my-server");
        let plan = plan_tauri(tmp.path()).unwrap();
        // The staging script must use the [[bin]] name, not the package name.
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("my-server"),
            "stage-sidecar.sh must use [[bin]] name 'my-server' for the compiled binary"
        );
        assert!(
            !sh.contains("/release/my-app"),
            "stage-sidecar.sh must not hardcode the package name 'my-app' as the binary path"
        );
    }

    #[test]
    fn plan_normalizes_dotslash_bin_path() {
        // [[bin]] path = "./src/main.rs" (with ./ prefix) must be treated the same
        // as "src/main.rs" — both point to the same file, and the [[bin]] name must
        // override the package name just like the non-prefixed form.
        let tmp = project_with_dotslash_bin("my-app", "my-server");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("my-server"),
            "[[bin]] with path='./src/main.rs' must still resolve to the custom bin name"
        );
        assert!(
            !sh.contains("--bin my-app"),
            "generator must not fall back to package name for a ./src/main.rs [[bin]] path"
        );
    }

    #[test]
    fn plan_normalizes_middle_dot_bin_path() {
        // [[bin]] path = "src/./main.rs" (CurDir component in the middle) must
        // canonicalize to src/main.rs, identifying it as the main binary entry point.
        let tmp = project_with_middle_dot_bin("my-app", "my-server");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("my-server"),
            "[[bin]] with path='src/./main.rs' must resolve to the custom bin name (not package name)"
        );
        assert!(
            !sh.contains("--bin my-app"),
            "generator must canonicalize 'src/./main.rs' and not fall back to the package name"
        );
    }

    #[test]
    fn plan_errors_on_ambiguous_multi_src_bin() {
        // A package with multiple src/bin/ files and no default-run has no single
        // sidecar target; the generator must return an error rather than silently
        // using the package name (which cargo build --bin <package> would reject).
        let tmp = project_with_multi_src_bin("my-app", &["web", "worker"]);
        let err = plan_tauri(tmp.path()).unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(_)),
            "expected Config error for ambiguous multi-bin package, got: {err:?}"
        );
        if let GenerateError::Config(msg) = err {
            assert!(
                msg.contains("ambiguous") || msg.contains("default-run"),
                "error message must mention ambiguity and how to resolve it, got: {msg}"
            );
        }
    }

    #[test]
    fn plan_resolves_workspace_inherited_version() {
        let tmp = project_with_workspace_version("my-app", "3.1.4");
        let plan = plan_tauri(tmp.path()).unwrap();
        let conf: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("tauri.conf.json"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("tauri.conf.json action not found");
        let parsed: serde_json::Value = serde_json::from_str(conf).unwrap();
        assert_eq!(
            parsed["version"].as_str(),
            Some("3.1.4"),
            "tauri.conf.json must use the resolved workspace version, not fall back to 0.1.0"
        );
    }

    #[test]
    fn plan_aux_bin_does_not_override_main_binary() {
        // A project with src/main.rs (auto-discovered as the primary binary, package
        // name) plus an auxiliary [[bin]] for a worker should stage the package-named
        // binary, NOT the worker.
        let tmp = project_with_aux_bin("my-app", "worker");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("my-app"),
            "stage-sidecar.sh must use the package (primary) binary name when src/main.rs is auto-discovered"
        );
        assert!(
            !sh.contains("/release/worker") && !sh.contains("--bin worker"),
            "stage-sidecar.sh must not use the auxiliary [[bin]] name 'worker' as the primary target"
        );
    }

    #[test]
    fn plan_honours_default_run_over_first_bin() {
        // A package with multiple explicit [[bin]] entries and `default-run = "web"`
        // must stage the "web" binary, not the first-listed one (e.g. "seed").
        let tmp = project_with_default_run("my-app", "web", &["seed", "web", "worker"]);
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("--bin web"),
            "stage-sidecar.sh must use the default-run binary 'web', not the first [[bin]] entry"
        );
        assert!(
            !sh.contains("--bin seed"),
            "stage-sidecar.sh must not use 'seed' (first [[bin]] entry) when default-run is set"
        );
    }

    #[test]
    fn plan_honours_default_run_over_main_rs_when_explicit_bins_exist() {
        // When src/main.rs is present alongside explicit [[bin]] entries and
        // [package] default-run = "web", the generator must use "web" (not the
        // package-named auto-discovered binary from src/main.rs).
        // Matches `autumn dev`/`autumn build` behaviour which prefers default_run.
        let tmp = project_with_default_run_and_main_rs("my-app", "web", &["web", "seed"]);
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("--bin web"),
            "stage-sidecar.sh must use the default-run binary 'web' even when src/main.rs \
             also exists; the package-named auto-bin must not override default-run"
        );
        assert!(
            !sh.contains("--bin my-app"),
            "stage-sidecar.sh must not use the package name 'my-app' when default-run = 'web' \
             is set, even though src/main.rs would be auto-discovered as 'my-app'"
        );
    }

    #[test]
    fn plan_prefers_main_rs_over_src_bin_when_both_exist() {
        // A package with BOTH src/main.rs AND src/bin/worker.rs and NO [[bin]] table —
        // Cargo treats src/main.rs as the primary binary (package-named), and src/bin/
        // as an additional binary.  The generator must pick the package name, not "worker".
        let tmp = project_with_main_and_src_bin("my-app", "worker");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("my-app"),
            "when src/main.rs and src/bin/worker.rs both exist without [[bin]], \
             the package name 'my-app' must be used (not the src/bin/ stem 'worker')"
        );
        assert!(
            !sh.contains("--bin worker"),
            "staging script must not use the src/bin/ stem 'worker' when src/main.rs is present"
        );
    }

    #[test]
    fn normalize_manifest_path_resolves_dotdot_and_dot() {
        // Cargo normalizes manifest paths before matching, so `src/../src/main.rs`
        // is equivalent to `src/main.rs`.  Our helper must do the same so we
        // don't miss the main_bin match and fall through to the wrong fallback.
        assert_eq!(
            normalize_manifest_path("src/../src/main.rs"),
            vec!["src", "main.rs"],
            "src/../src/main.rs must normalize to [src, main.rs]"
        );
        assert_eq!(
            normalize_manifest_path("./src/main.rs"),
            vec!["src", "main.rs"],
            "./src/main.rs must normalize to [src, main.rs]"
        );
        assert_eq!(
            normalize_manifest_path("src/main.rs"),
            vec!["src", "main.rs"],
            "plain src/main.rs must pass through unchanged"
        );
        // Windows-style separator.
        assert_eq!(
            normalize_manifest_path(r"src\main.rs"),
            vec!["src", "main.rs"],
            r"src\main.rs must normalize to [src, main.rs]"
        );
        // Multiple consecutive .. resolve correctly.
        assert_eq!(
            normalize_manifest_path("a/b/../../src/main.rs"),
            vec!["src", "main.rs"],
            "a/b/../../src/main.rs must normalize to [src, main.rs]"
        );
        // A leading ".." escapes the package root.  The result must NOT equal
        // ["src", "main.rs"] so that an external helper bin is never mistaken
        // for the package's own main binary.
        assert_ne!(
            normalize_manifest_path("../src/main.rs"),
            vec!["src", "main.rs"],
            "../src/main.rs must preserve the leading '..' and must NOT normalize \
             to [src, main.rs]; otherwise an out-of-package helper bin is \
             incorrectly treated as the package main binary"
        );
        assert_eq!(
            normalize_manifest_path("../src/main.rs"),
            vec!["..", "src", "main.rs"],
            "../src/main.rs must normalize to ['..', 'src', 'main.rs']"
        );
        // Multiple leading ".." are all preserved.
        assert_eq!(
            normalize_manifest_path("../../lib/main.rs"),
            vec!["..", "..", "lib", "main.rs"],
            "../../lib/main.rs must preserve both leading '..' segments"
        );
    }

    #[test]
    fn plan_respects_autobins_false_ignores_main_rs() {
        // autobins=false disables src/main.rs auto-discovery.  The explicit [[bin]]
        // target must be used even though src/main.rs exists.
        let tmp = project_with_autobins_false("my-app", "web");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("--bin web"),
            "with autobins=false the explicit [[bin]] name 'web' must be used; \
             src/main.rs is not a Cargo target when autobins is disabled"
        );
        assert!(
            !sh.contains("--bin my-app"),
            "with autobins=false the package name 'my-app' must NOT be used; \
             there is no auto-discovered binary for src/main.rs"
        );
    }

    #[test]
    fn plan_respects_autobins_false_ignores_src_bin_dir() {
        // autobins=false means Cargo does NOT auto-discover src/bin/ entries.
        // With no explicit [[bin]] the package has no bin target at all, so
        // plan_tauri must error rather than silently use a package-name binary
        // that `cargo build --bin` would reject.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\nedition=\"2024\"\nautobins=false\n\
             \n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        // src/bin/web.rs exists on disk but autobins=false → NOT a Cargo target.
        fs::write(tmp.path().join("src/bin/web.rs"), "fn main() {}\n").unwrap();
        let err = plan_tauri(tmp.path()).unwrap_err().to_string();
        assert!(
            err.contains("no binary target"),
            "expected 'no binary target' error when autobins=false and no [[bin]]; got: {err}"
        );
        assert!(
            !err.contains("web"),
            "error must not mention 'web' — src/bin/web.rs is not a Cargo target with autobins=false; got: {err}"
        );
    }

    #[test]
    fn plan_errors_when_package_has_no_bin_target() {
        // A lib-only package (autobins=false, no [[bin]], no src/main.rs) has no
        // binary target to use as a sidecar.  plan_tauri must return a clear error
        // instead of fabricating a package-named binary that `cargo build --bin`
        // would reject.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"my-lib\"\nversion=\"0.1.0\"\nedition=\"2024\"\nautobins=false\n\
             \n[lib]\nname=\"my_lib\"\n\n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "").unwrap();
        let err = plan_tauri(tmp.path()).unwrap_err().to_string();
        assert!(
            err.contains("no binary target"),
            "expected a 'no binary target' error for a lib-only package; got: {err}"
        );
    }

    #[test]
    fn plan_errors_on_multiple_explicit_bins_without_default_run() {
        // Multiple [[bin]] entries with no default-run and no src/main.rs must error
        // rather than silently pick the first entry (which may be an auxiliary binary).
        let tmp = project_with_multiple_bins_no_default("my-app", &["worker", "web"]);
        let err = plan_tauri(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ambiguous") || msg.contains("default-run"),
            "expected an ambiguous-sidecar error when multiple explicit [[bin]] entries \
             exist and no default-run is set; got: {msg}"
        );
        assert!(
            msg.contains("web") && msg.contains("worker"),
            "error message must list the ambiguous bin names; got: {msg}"
        );
    }

    #[test]
    fn plan_detects_src_bin_auto_discovered_binary() {
        // A package with src/bin/web.rs and no src/main.rs or [[bin]] table —
        // Cargo auto-discovers it as binary "web" (file stem, not package name).
        let tmp = project_with_src_bin("my-app", "web");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("--bin web"),
            "staging script must use the src/bin/ file stem 'web', not the package name 'my-app'"
        );
        assert!(
            !sh.contains("--bin my-app"),
            "staging script must not use the package name when a src/bin/ binary is auto-discovered"
        );
    }

    #[test]
    fn plan_detects_src_bin_dir_style_binary() {
        // A package with src/bin/web/main.rs and no src/main.rs or [[bin]] table —
        // Cargo auto-discovers it as binary "web" (directory name, not package name).
        let tmp = project_with_src_bin_dir("my-app", "web");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("--bin web"),
            "staging script must detect src/bin/web/main.rs directory-style bin as 'web'"
        );
        assert!(
            !sh.contains("--bin my-app"),
            "staging script must not fall back to the package name for a directory-style bin"
        );
    }

    #[test]
    fn stage_sidecar_sh_uses_app_embed_feature_when_defined() {
        // When the app defines an `embed-assets` Cargo feature, the staging
        // script must pass it (not just the dep path) so that
        // `#[cfg(feature = "embed-assets")]` guards in app code are active.
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        assert!(
            sh.contains("--features embed-assets,autumn-web/managed-pg-bundled"),
            "when app has embed-assets feature the script must use the app-crate feature flag"
        );
    }

    #[test]
    fn stage_sidecar_sh_falls_back_to_dep_path_when_no_app_feature() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", false, "autumn-web");
        assert!(
            sh.contains("--features autumn-web/embed-assets,autumn-web/managed-pg-bundled"),
            "without app-level embed-assets, script must use the dep feature path"
        );
    }

    #[test]
    fn plan_uses_app_embed_feature_for_generated_scaffold() {
        // The `project()` fixture matches a scaffold generated by `autumn new`,
        // which always defines an `embed-assets` feature.
        let tmp = project_with_embed_assets("my-app");
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("--features embed-assets,"),
            "generated scaffold with embed-assets feature must use app-crate feature in staging script"
        );
    }

    #[test]
    fn new_project_template_defines_embed_assets_feature() {
        // `autumn new` must scaffold the `embed-assets` Cargo feature so that:
        //   1. `autumn generate tauri` detects it (has_embed_assets=true) and uses
        //      the app-crate feature path instead of the dep-only path.
        //   2. The `#[cfg(feature = "embed-assets")]` guards in main.rs.tmpl are
        //      activated by `--features embed-assets`, so `.embedded_static()` is
        //      wired and desktop apps ship their CSS/JS in the sidecar binary.
        let tmpl = include_str!("../templates/Cargo.toml.tmpl");
        assert!(
            tmpl.contains("embed-assets"),
            "Cargo.toml.tmpl must define an `embed-assets` feature mapping to \
             `autumn-web/embed-assets`; without it `autumn generate tauri` falls back \
             to the dep-only feature path and `.embedded_static()` is never called \
             (main.rs.tmpl guards it on #[cfg(feature = \"embed-assets\")]); \
             got:\n{tmpl}"
        );
    }

    #[test]
    fn stage_sidecar_sh_creates_autumn_toml_when_absent() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        assert!(
            sh.contains(": > autumn.toml") || sh.contains("> autumn.toml"),
            "staging script must create an empty autumn.toml when none exists \
             (tauri.conf.json always lists it as a bundle resource)"
        );
    }

    #[test]
    fn stage_sidecar_ps1_creates_autumn_toml_when_absent() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", true, "autumn-web");
        assert!(
            ps1.contains("autumn.toml"),
            "PowerShell staging script must handle a missing autumn.toml"
        );
        assert!(
            ps1.contains("New-Item") || ps1.contains("Set-Content"),
            "PowerShell staging script must create autumn.toml when absent"
        );
    }

    #[test]
    fn tauri_conf_has_identifier() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert!(
            parsed["identifier"].is_string(),
            "tauri.conf.json must have an identifier"
        );
        assert!(
            parsed["identifier"].as_str().unwrap().contains("my-app"),
            "identifier must include the package name"
        );
    }

    #[test]
    fn tauri_conf_has_product_name() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert!(
            parsed["productName"].is_string(),
            "tauri.conf.json must have productName"
        );
    }

    #[test]
    fn tauri_conf_has_external_bin() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let bins = parsed["bundle"]["externalBin"]
            .as_array()
            .expect("bundle.externalBin must be an array");
        assert!(!bins.is_empty(), "must list at least one external binary");
        assert!(
            bins.iter()
                .any(|b| b.as_str().unwrap_or("").contains("my-app")),
            "externalBin must reference the app name"
        );
    }

    #[test]
    fn tauri_conf_has_icon_array() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let icons = parsed["bundle"]["icon"]
            .as_array()
            .expect("bundle.icon must be an array");
        assert!(
            icons.len() >= 4,
            "must list at least 4 icon files, got {}",
            icons.len()
        );
    }

    #[test]
    fn tauri_conf_has_no_before_build_command() {
        // beforeBuildCommand lives in the platform-specific overlay files, not the
        // main tauri.conf.json, so the generated scaffold is host-OS-agnostic.
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert!(
            parsed["build"]["beforeBuildCommand"].is_null(),
            "beforeBuildCommand must be absent from tauri.conf.json; \
             it belongs in the platform-specific overlay files"
        );
    }

    #[test]
    fn platform_conf_files_are_in_plan() {
        let tmp = project("my-app");
        assert!(has_action(&tmp, "src-tauri/tauri.linux.conf.json"));
        assert!(has_action(&tmp, "src-tauri/tauri.macos.conf.json"));
        assert!(has_action(&tmp, "src-tauri/tauri.windows.conf.json"));
    }

    #[test]
    fn platform_conf_linux_has_before_build_and_dev_commands() {
        let conf = render_tauri_linux_conf();
        let parsed: serde_json::Value = serde_json::from_str(&conf).expect("valid JSON");
        let build_cmd = parsed["build"]["beforeBuildCommand"].as_str().unwrap_or("");
        // beforeDevCommand is the object form { script, wait: true } — not a plain string.
        let dev_script = parsed["build"]["beforeDevCommand"]["script"]
            .as_str()
            .unwrap_or("");
        let dev_wait = parsed["build"]["beforeDevCommand"]["wait"]
            .as_bool()
            .unwrap_or(false);
        assert!(
            build_cmd.contains("stage-sidecar"),
            "linux conf must have beforeBuildCommand referencing the staging script"
        );
        assert!(
            dev_script.contains("stage-sidecar"),
            "linux conf must have beforeDevCommand.script referencing the staging script"
        );
        assert!(
            dev_wait,
            "linux beforeDevCommand must set wait:true so staging completes before sidecar spawn"
        );
        assert!(
            build_cmd.contains("bash"),
            "linux beforeBuildCommand must use bash"
        );
    }

    #[test]
    fn platform_conf_macos_has_before_build_and_dev_commands() {
        let conf = render_tauri_macos_conf();
        let parsed: serde_json::Value = serde_json::from_str(&conf).expect("valid JSON");
        let build_cmd = parsed["build"]["beforeBuildCommand"].as_str().unwrap_or("");
        let dev_script = parsed["build"]["beforeDevCommand"]["script"]
            .as_str()
            .unwrap_or("");
        let dev_wait = parsed["build"]["beforeDevCommand"]["wait"]
            .as_bool()
            .unwrap_or(false);
        assert!(
            build_cmd.contains("stage-sidecar"),
            "macos conf must have beforeBuildCommand referencing the staging script"
        );
        assert!(
            dev_script.contains("stage-sidecar"),
            "macos conf must have beforeDevCommand.script referencing the staging script"
        );
        assert!(
            dev_wait,
            "macos beforeDevCommand must set wait:true so staging completes before sidecar spawn"
        );
        assert!(
            build_cmd.contains("bash"),
            "macos beforeBuildCommand must use bash"
        );
    }

    #[test]
    fn platform_conf_windows_has_before_build_and_dev_commands() {
        let conf = render_tauri_windows_conf();
        let parsed: serde_json::Value = serde_json::from_str(&conf).expect("valid JSON");
        let build_cmd = parsed["build"]["beforeBuildCommand"].as_str().unwrap_or("");
        let dev_script = parsed["build"]["beforeDevCommand"]["script"]
            .as_str()
            .unwrap_or("");
        let dev_wait = parsed["build"]["beforeDevCommand"]["wait"]
            .as_bool()
            .unwrap_or(false);
        assert!(
            build_cmd.contains("stage-sidecar"),
            "windows conf must have beforeBuildCommand referencing the staging script"
        );
        assert!(
            dev_script.contains("stage-sidecar"),
            "windows conf must have beforeDevCommand.script referencing the staging script"
        );
        assert!(
            dev_wait,
            "windows beforeDevCommand must set wait:true so staging completes before sidecar spawn"
        );
        assert!(
            build_cmd.contains("powershell") || build_cmd.contains("ps1"),
            "windows beforeBuildCommand must use PowerShell"
        );
    }

    // ── render_shell_cargo_toml ───────────────────────────────────────────────

    #[test]
    fn shell_cargo_toml_has_own_workspace_table() {
        let cargo = render_shell_cargo_toml("my-app");
        assert!(
            cargo.contains("[workspace]"),
            "shell Cargo.toml must have its own [workspace] table to be independent"
        );
    }

    #[test]
    fn shell_cargo_toml_has_tauri_dep() {
        let cargo = render_shell_cargo_toml("my-app");
        assert!(
            cargo.contains("tauri"),
            "shell Cargo.toml must depend on tauri"
        );
    }

    #[test]
    fn shell_cargo_toml_has_tauri_plugin_shell_dep() {
        let cargo = render_shell_cargo_toml("my-app");
        assert!(
            cargo.contains("tauri-plugin-shell"),
            "shell Cargo.toml must depend on tauri-plugin-shell"
        );
    }

    #[test]
    fn shell_cargo_toml_has_tauri_build_dep() {
        let cargo = render_shell_cargo_toml("my-app");
        assert!(
            cargo.contains("tauri-build"),
            "shell Cargo.toml must depend on tauri-build in build-dependencies"
        );
    }

    #[test]
    fn shell_cargo_toml_package_name_includes_app_name() {
        let cargo = render_shell_cargo_toml("my-app");
        assert!(
            cargo.contains("my-app"),
            "shell crate name must reference the app name"
        );
    }

    // ── render_shell_lib_rs (sidecar lifecycle) ───────────────────────────────

    #[test]
    fn lib_rs_binds_loopback_ephemeral_port() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("127.0.0.1:0"),
            "lib.rs must bind loopback:0 to find a free ephemeral port"
        );
    }

    #[test]
    fn lib_rs_sets_autumn_server_port_env() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_SERVER__PORT"),
            "lib.rs must pass AUTUMN_SERVER__PORT to the sidecar"
        );
    }

    #[test]
    fn lib_rs_sets_autumn_server_host_env() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_SERVER__HOST"),
            "lib.rs must pass AUTUMN_SERVER__HOST to the sidecar"
        );
    }

    #[test]
    fn lib_rs_sets_managed_pg_data_dir() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_MANAGED_PG_DATA_DIR"),
            "lib.rs must pass AUTUMN_MANAGED_PG_DATA_DIR for managed Postgres (#1119)"
        );
    }

    #[test]
    fn lib_rs_spawns_sidecar() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains(".sidecar("),
            "lib.rs must spawn the sidecar via tauri-plugin-shell"
        );
        assert!(
            lib.contains("my-app"),
            "lib.rs must reference the app name as the sidecar binary"
        );
    }

    #[test]
    fn lib_rs_polls_for_http_response() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // Probe /health — autumn always registers this endpoint and it responds cheaply.
        // Any valid HTTP response (200, 404, …) starts with "HTTP/" and signals the
        // server is up, avoiding a timeout against a slow app root handler.
        assert!(
            lib.contains("HTTP/"),
            "lib.rs readiness probe must accept any HTTP response prefix"
        );
        assert!(
            lib.contains("GET /health"),
            "lib.rs must probe GET /health — a cheap readiness endpoint autumn always \
             registers — instead of GET / which can time out for slow app root handlers"
        );
    }

    #[test]
    fn lib_rs_kills_sidecar_on_window_destroyed() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("Destroyed"),
            "lib.rs must handle WindowEvent::Destroyed"
        );
        assert!(
            lib.contains(".kill()"),
            "lib.rs must kill the sidecar child on window close"
        );
        assert!(
            lib.contains("window.label()") && lib.contains("\"main\""),
            "lib.rs must guard kill behind window.label() == \"main\" so secondary windows \
             don't prematurely terminate the sidecar"
        );
    }

    #[test]
    fn lib_rs_pg_data_dir_uses_db_subdirectory() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains(".join(\"db\")"),
            "lib.rs must isolate Postgres files in <app-data-dir>/db, not the root"
        );
    }

    #[test]
    fn lib_rs_does_not_override_health_path() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // The sidecar env must NOT set AUTUMN_HEALTH__PATH.  The probe already targets
        // GET /health directly; overriding the path via env would only move where autumn
        // registers the endpoint, not change what the probe hits.  Leave the app's
        // [health].path config untouched so developer expectations are not violated.
        assert!(
            !lib.contains("AUTUMN_HEALTH__PATH"),
            "lib.rs must NOT set AUTUMN_HEALTH__PATH — the probe targets /health directly \
             and overriding the env var only moves the framework endpoint unnecessarily"
        );
    }

    #[test]
    fn lib_rs_clears_unix_socket_env() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_SERVER__UNIX_SOCKET"),
            "lib.rs must clear AUTUMN_SERVER__UNIX_SOCKET so an inherited env var \
             cannot redirect the sidecar to a Unix socket the TCP probe cannot reach"
        );
        // AUTUMN_SERVE_FORCE_UNIX_SOCKET is a separate out-of-band override in
        // app.rs that bypasses the config-system socket setting; must also be cleared.
        assert!(
            lib.contains("AUTUMN_SERVE_FORCE_UNIX_SOCKET"),
            "lib.rs must also clear AUTUMN_SERVE_FORCE_UNIX_SOCKET — it overrides \
             AUTUMN_SERVER__UNIX_SOCKET and would still redirect to a Unix socket"
        );
    }

    #[test]
    fn lib_rs_clears_managed_pg_attach_url() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // If AUTUMN_MANAGED_PG_ATTACH_URL is inherited (e.g. from a terminal where
        // `autumn serve` is running), ManagedPostgresPoolProvider connects to that
        // existing cluster instead of starting the bundled one.  Clearing it ensures
        // the desktop app always owns its local database.
        assert!(
            lib.contains("AUTUMN_MANAGED_PG_ATTACH_URL"),
            "lib.rs must clear AUTUMN_MANAGED_PG_ATTACH_URL so an inherited attach \
             URL cannot redirect the sidecar to a foreign or stale database cluster"
        );
    }

    #[test]
    fn lib_rs_sends_graceful_shutdown_signal_before_kill() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // Unix: SIGTERM triggers autumn's tokio signal handler so on_shutdown hooks run.
        // Windows: autumn only handles ctrl_c() (CTRL_C_EVENT); taskkill sends
        // WM_CLOSE/CTRL_CLOSE_EVENT which autumn does not handle, so we force-kill
        // immediately without a delay rather than waiting 3 s for a signal that
        // never arrives.
        assert!(
            lib.contains("SIGTERM") || lib.contains("-TERM"),
            "lib.rs on_window_event must send SIGTERM on Unix before force-killing"
        );
        assert!(
            !lib.contains("Command::new(\"taskkill\")"),
            "lib.rs must not invoke taskkill — it sends WM_CLOSE/CTRL_CLOSE_EVENT which \
             autumn does not handle; use child.kill() directly on Windows"
        );
        assert!(
            lib.contains("pid()"),
            "lib.rs must call child.pid() to get the sidecar PID for the Unix kill signal"
        );
    }

    #[test]
    fn lib_rs_clears_inherited_profile_env_vars() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // AUTUMN_ENV is set conditionally: "dev" in debug Tauri builds (cargo tauri dev)
        // and "" in release Tauri builds (cargo tauri build), using cfg!(debug_assertions).
        assert!(
            lib.contains("AUTUMN_ENV") && lib.contains("cfg!(debug_assertions)"),
            "lib.rs must set AUTUMN_ENV conditionally on cfg!(debug_assertions) \
             so cargo tauri dev uses dev config and cargo tauri build uses prod config"
        );
        assert!(
            lib.contains("\"AUTUMN_PROFILE\", \"\""),
            "lib.rs must clear AUTUMN_PROFILE (legacy alias) so it is never inherited"
        );
    }

    #[test]
    fn lib_rs_sets_dev_profile_in_debug_tauri_builds() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // cargo tauri dev compiles the shell in debug mode; the sidecar is always a
        // --release binary (AUTUMN_IS_DEBUG=0 baked in → prod profile). Setting
        // AUTUMN_ENV=dev in cfg!(debug_assertions) makes the sidecar load dev config.
        assert!(
            lib.contains("\"dev\"") && lib.contains("cfg!(debug_assertions)"),
            "lib.rs must set AUTUMN_ENV=\"dev\" when cfg!(debug_assertions) so \
             cargo tauri dev loads dev config even though the sidecar is --release"
        );
    }

    #[test]
    fn lib_rs_overrides_trusted_hosts_to_loopback() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // Production autumn.toml may set trusted_hosts.hosts = ["example.com"].
        // The webview connects to http://127.0.0.1:{port} (Host: 127.0.0.1) which
        // would be rejected by the trusted-host middleware with a 400.  The shell
        // always overrides the setting to allow loopback — the server only binds
        // loopback so no external traffic reaches it regardless.
        assert!(
            lib.contains("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS"),
            "lib.rs must set AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS to override \
             any production trusted-host config that would reject the webview's \
             loopback Host header"
        );
        assert!(
            lib.contains("127.0.0.1") && lib.contains("localhost"),
            "AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS must include both 127.0.0.1 \
             and localhost so either form of the loopback address is accepted"
        );
    }

    #[test]
    fn lib_rs_clears_one_off_mode_env_vars() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        for var in &[
            "AUTUMN_BUILD_STATIC",
            "AUTUMN_DUMP_ROUTES",
            "AUTUMN_LIST_TASKS",
            "AUTUMN_RUN_TASK",
        ] {
            assert!(
                lib.contains(&format!("\"{var}\", \"\"")),
                "lib.rs must clear {var} so an inherited one-off mode flag \
                 doesn't prevent the sidecar from binding its HTTP port"
            );
        }
    }

    #[test]
    fn lib_rs_sets_prestop_grace_to_zero() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_SERVER__PRESTOP_GRACE_SECS") && lib.contains("\"0\""),
            "lib.rs must set AUTUMN_SERVER__PRESTOP_GRACE_SECS=0 so the sidecar skips \
             the listener-drain delay; without this, the default 5-second prestop grace \
             causes on_shutdown hooks (managed Postgres cleanup) to run after the \
             force-kill fires; got:\n{lib}"
        );
    }

    #[test]
    fn lib_rs_kill_timeout_exceeds_default_prestop_grace() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // The default prestop_grace_secs is 5; the force-kill must wait at least
        // that long so shutdown hooks finish even if the env override is absent.
        // Look for from_secs(N) where N >= 5.
        let has_adequate_timeout = (5u64..=60).any(|n| lib.contains(&format!("from_secs({n})")));
        assert!(
            has_adequate_timeout,
            "lib.rs kill timeout must be >= 5 s (the default prestop grace) so \
             on_shutdown hooks complete before the force-kill; got:\n{lib}"
        );
    }

    #[test]
    fn lib_rs_waits_300s_for_managed_postgres_cold_start() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // A first-launch managed-Postgres cluster must initialise (pg_ctl init) and
        // run migrations before serving HTTP; on slow disks this can take several
        // minutes.  autumn serve uses READY_TIMEOUT_MANAGED_PG = 300 s; the generated
        // shell must match so the first-launch window is not rejected prematurely.
        // We look for 1500 iterations × 200 ms = 300 s, or any explicit "300" in context.
        assert!(
            lib.contains("0..1500") || lib.contains("300 s") || lib.contains("300s"),
            "lib.rs readiness poll must allow at least 300 s (1500 × 200 ms) for \
             managed-Postgres cold-start; the current limit is too short and will kill \
             the sidecar before the cluster finishes initialising on a slow disk; \
             got a snippet:\n{}",
            lib.lines()
                .filter(|l| l.contains("150") || l.contains("1500") || l.contains("300"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn lib_rs_sets_autumn_manifest_dir() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_MANIFEST_DIR"),
            "lib.rs must set AUTUMN_MANIFEST_DIR to the Tauri resource dir so the \
             sidecar finds the bundled autumn.toml on the installed machine"
        );
    }

    #[test]
    fn lib_rs_sets_sidecar_cwd_to_resource_dir() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains(".current_dir("),
            "lib.rs must set the sidecar's working directory to resource_dir; \
             OsEnv::var(AUTUMN_MANIFEST_DIR) returns the compile-time CARGO_MANIFEST_DIR \
             (which is absent on installed machines), so AutumnConfig falls back to \
             PathBuf::from(\"autumn.toml\") — setting CWD to resource_dir makes that \
             fallback find the bundled autumn.toml"
        );
    }

    #[test]
    fn lib_rs_redirects_blob_storage_to_app_data_dir() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_STORAGE__LOCAL__ROOT"),
            "lib.rs must set AUTUMN_STORAGE__LOCAL__ROOT; default storage.local.root is \
             'target/blobs' (relative to CWD = resource_dir, which is read-only in \
             installed bundles), so apps using local blob storage would abort at startup"
        );
        assert!(
            lib.contains("data_root") && lib.contains("join(\"blobs\")"),
            "AUTUMN_STORAGE__LOCAL__ROOT must point to a writable subdirectory of \
             app_data_dir (e.g. data_root.join(\"blobs\")), not the read-only resource_dir"
        );
    }

    #[test]
    fn lib_rs_restricts_blob_storage_dir_permissions() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // The blobs/ directory must be created explicitly before the sidecar spawns
        // so we can apply 0700 on Unix — LocalBlobStore uses create_dir_all which
        // inherits the process umask (typically 0755), leaving files 0644 and readable
        // by other local accounts.
        assert!(
            lib.contains("blobs_dir") && lib.contains("create_dir_all"),
            "lib.rs must create the blobs/ directory explicitly (not rely on \
             LocalBlobStore::new) so permissions can be restricted before any \
             data is written"
        );
        assert!(
            lib.contains("set_permissions") && lib.contains("0o700"),
            "lib.rs must restrict the blobs/ directory to 0700 on Unix; otherwise \
             LocalBlobStore creates it with the process umask (typically 0755) and \
             other local accounts can read private uploads from disk"
        );
    }

    #[test]
    fn lib_rs_allows_local_storage_in_production() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION"),
            "lib.rs must set AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION=true; \
             StorageConfig::backend_plan rejects local-backend configs in prod mode \
             without this flag, aborting sidecar startup before the window opens"
        );
        assert!(
            lib.contains("\"AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION\", \"true\""),
            "AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION must be set to \"true\"; \
             the loopback-only sidecar with app-data blob root is single-user and safe"
        );
    }

    #[test]
    fn lib_rs_generates_per_install_signing_secret() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("load_or_generate_signing_secret"),
            "lib.rs must define load_or_generate_signing_secret() so a per-install signing \
             secret is generated on first launch; without it, autumn's prod mode calls \
             fail_fast_on_invalid_signing_secret and aborts before binding the HTTP port"
        );
        assert!(
            lib.contains("getrandom::getrandom"),
            "load_or_generate_signing_secret must use getrandom to produce cryptographic \
             randomness; a weak source would allow signing secret prediction"
        );
        assert!(
            lib.contains("signing_secret.txt"),
            "load_or_generate_signing_secret must persist the secret to a file so it \
             survives app restarts; re-generating on every launch would invalidate all \
             existing sessions"
        );
        // Must NOT silently continue with zero bytes on RNG failure.
        assert!(
            !lib.contains("unwrap_or(())"),
            "getrandom failure must propagate as an error, not be silently swallowed with \
             unwrap_or(()); an all-zero signing secret is trivially guessable"
        );
        // The function must return a Result so the caller can propagate the error.
        assert!(
            lib.contains("Result<String,") || lib.contains("Result<String, "),
            "load_or_generate_signing_secret must return Result<String, ...> so getrandom \
             failure aborts setup() rather than continuing with a predictable zero secret"
        );
        // Write failures must also propagate — a disk-full or permission error must abort
        // startup rather than silently returning an ephemeral secret that rotates every launch.
        assert!(
            !lib.contains("let _ = std::fs::write")
                && !lib.contains("let _ = std::fs::OpenOptions"),
            "signing-secret write errors must propagate with `?`, not be silently discarded \
             with `let _ = ...`; a failed write leaves a truncated file and the app runs \
             with an ephemeral secret that rotates on every restart"
        );
    }

    #[test]
    fn lib_rs_sets_signing_secret_env_var() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_SECURITY__SIGNING_SECRET"),
            "lib.rs must pass AUTUMN_SECURITY__SIGNING_SECRET to the sidecar; without it \
             autumn's prod mode calls fail_fast_on_invalid_signing_secret and exits before \
             binding the HTTP port, causing the TCP health probe to time out"
        );
        // The env var must be set using the local signing_secret variable.
        assert!(
            lib.contains("signing_secret"),
            "AUTUMN_SECURITY__SIGNING_SECRET must be sourced from the signing_secret local \
             variable returned by load_or_generate_signing_secret"
        );
    }

    #[test]
    fn lib_rs_hardens_existing_signing_secret_permissions() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // If the file already exists (early-return path), we must still chmod it
        // to 0600 on Unix — an existing file created by an older generated shell
        // (0644) or manually stays world-readable otherwise.
        assert!(
            lib.contains("from_mode(0o600)"),
            "load_or_generate_signing_secret must call set_permissions(0o600) on \
             an existing signing_secret.txt before returning it; a file created by \
             an older shell at 0644 would expose the JWT key to other local accounts"
        );
        // The chmod must appear BEFORE the early return, not only in the create path.
        let chmod_pos = lib.find("from_mode(0o600)").unwrap_or(usize::MAX);
        let early_return_pos = lib.find("return Ok(s)").unwrap_or(usize::MAX);
        assert!(
            chmod_pos < early_return_pos,
            "set_permissions(0o600) must appear before `return Ok(s)` so the existing \
             file is hardened on every read, not just on creation"
        );
    }

    #[test]
    fn lib_rs_documents_managed_pg_auto_migrate_opt_in() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION is safe only for desktop
        // apps that use ManagedPostgresPoolProvider (bundled Postgres).  Emitting
        // it unconditionally would run pending migrations against any remote /
        // shared database on every desktop client launch.  The generated shell must
        // include the env var as a commented-out opt-in with an explanation.
        assert!(
            lib.contains("AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION"),
            "lib.rs must mention AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION (as a \
             commented-out opt-in) so developers wiring ManagedPostgresPoolProvider \
             know to uncomment it; without it, a fresh cluster is never migrated and \
             routes fail with 500 on first desktop launch"
        );
        // Must NOT be emitted as a live .env() call — that would run migrations
        // against any remote/shared database on every desktop client launch.
        let live = lib.lines().any(|line| {
            !line.trim_start().starts_with("//")
                && line.contains("AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION")
        });
        assert!(
            !live,
            "AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION must be commented out in \
             lib.rs, not emitted as a live .env() call; enabling it unconditionally \
             would run pending migrations against any remote/shared database on every \
             desktop client launch, overriding the user's prod-config default-off"
        );
    }

    #[test]
    fn staging_sh_fingerprints_before_embed() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        let fingerprint_pos = sh.find("autumn build --embed").unwrap_or(usize::MAX);
        let cargo_pos = sh.find("cargo build --release").unwrap_or(usize::MAX);
        assert!(
            fingerprint_pos < cargo_pos,
            "stage-sidecar.sh must run `autumn build --embed` before the embed cargo build \
             to fingerprint static/ (mirror the 3-phase autumn build --embed process); \
             stale .autumn-manifest.json would bake wrong asset hashes into the sidecar"
        );
    }

    #[test]
    fn staging_sh_fingerprints_includes_package_and_bin_name() {
        let sh = render_stage_sidecar_sh("my-app", "my-server", true, "autumn-web");
        assert!(
            sh.contains("autumn build --embed -p my-app --bin my-server"),
            "stage-sidecar.sh must pass both -p <package> and --bin <bin> to autumn build \
             --embed so workspace members fingerprint the correct package and only the \
             sidecar binary is compiled (not all [[bin]] targets); got:\n{sh}"
        );
        assert!(
            sh.contains("--features autumn-web/managed-pg-bundled"),
            "fingerprint line must pass --features <dep_key>/managed-pg-bundled so apps \
             wiring ManagedPostgresPoolProvider compile in the pre-embed phase; got:\n{sh}"
        );
    }

    #[test]
    fn staging_ps1_fingerprints_before_embed() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", true, "autumn-web");
        let fingerprint_pos = ps1.find("autumn build --embed").unwrap_or(usize::MAX);
        let cargo_pos = ps1.find("cargo build --release").unwrap_or(usize::MAX);
        assert!(
            fingerprint_pos < cargo_pos,
            "stage-sidecar.ps1 must run `autumn build --embed` before the embed cargo build \
             to fingerprint static/ (mirror the 3-phase autumn build --embed process)"
        );
    }

    #[test]
    fn staging_sh_uses_dep_key_not_package_name_for_features() {
        // When the app aliases autumn-web as `autumn_web = { package = "autumn-web" }`,
        // the feature selector must use the dep key "autumn_web", not "autumn-web".
        let sh = render_stage_sidecar_sh("my-app", "my-app", false, "autumn_web");
        assert!(
            sh.contains("autumn_web/embed-assets"),
            "stage-sidecar.sh must use the dependency key 'autumn_web' in feature selectors, \
             not the package name 'autumn-web'; got: {sh}"
        );
        assert!(
            sh.contains("autumn_web/managed-pg-bundled"),
            "stage-sidecar.sh must use the dependency key 'autumn_web' for managed-pg feature; \
             got: {sh}"
        );
    }

    #[test]
    fn staging_sh_with_embed_assets_uses_dep_key_for_managed_pg() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn_web");
        // The app-level embed-assets feature stays as-is; only dep features use the key.
        assert!(
            sh.contains("embed-assets,autumn_web/managed-pg-bundled"),
            "with has_embed_assets=true and dep_key='autumn_web', the feature string must be \
             'embed-assets,autumn_web/managed-pg-bundled'"
        );
    }

    #[test]
    fn staging_ps1_uses_dep_key_not_package_name_for_features() {
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", false, "autumn_web");
        assert!(
            ps1.contains("autumn_web/embed-assets")
                && ps1.contains("autumn_web/managed-pg-bundled"),
            "stage-sidecar.ps1 must use the dependency key 'autumn_web' in feature selectors"
        );
    }

    #[test]
    fn staging_ps1_guards_empty_credentials_dir_before_copy() {
        // When config\credentials exists but is empty, Copy-Item with a wildcard
        // source ("config\credentials\*") matches nothing.  With
        // $ErrorActionPreference = "Stop" that becomes a terminating error.
        // The generated script must check for children before calling Copy-Item.
        let ps1 = render_stage_sidecar_ps1("my-app", "my-app", true, "autumn-web");
        // Must gate Copy-Item behind a Get-ChildItem / count check.
        assert!(
            ps1.contains("Get-ChildItem") || ps1.contains("Measure-Object"),
            "stage-sidecar.ps1 must guard Copy-Item with Get-ChildItem so an empty \
             config\\credentials directory does not abort the script under \
             $ErrorActionPreference = 'Stop'"
        );
        // The guard must appear before the Copy-Item call for credentials.
        let guard_pos = ps1
            .find("Get-ChildItem")
            .or_else(|| ps1.find("Measure-Object"))
            .unwrap_or(usize::MAX);
        let copy_pos = ps1.rfind("Copy-Item").unwrap_or(usize::MAX);
        assert!(
            guard_pos < copy_pos,
            "Get-ChildItem guard must appear before the Copy-Item credentials call"
        );
    }

    #[test]
    fn plan_uses_dep_key_alias_in_staging_scripts() {
        // An app that aliases autumn-web as autumn_web must generate staging scripts
        // with the correct feature key; cargo fails with "Package 'foo' does not have
        // feature 'autumn-web/managed-pg-bundled'" when the package name is used.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
             \n[dependencies]\nautumn_web = { package = \"autumn-web\", version = \"0.5\" }\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let plan = plan_tauri(tmp.path()).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("autumn_web/managed-pg-bundled"),
            "stage-sidecar.sh must use the dep key 'autumn_web' (not package name 'autumn-web') \
             when the app aliases the dependency; got: {sh}"
        );
        assert!(
            !sh.contains("autumn-web/managed-pg-bundled"),
            "stage-sidecar.sh must not contain 'autumn-web/managed-pg-bundled' when the dep is \
             aliased as 'autumn_web'; Cargo would reject it with 'Package does not have feature'"
        );
    }

    #[test]
    fn plan_uses_dep_key_for_workspace_inherited_alias() {
        // When a workspace member has `autumn_web = { workspace = true }` and the
        // workspace Cargo.toml defines `autumn_web = { package = "autumn-web" }`,
        // resolve_dep_key must return "autumn_web" (the alias key), not "autumn-web".
        let tmp = TempDir::new().unwrap();
        // Write workspace root Cargo.toml with [workspace.dependencies] alias.
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\
             \n[workspace.dependencies]\nautumn_web = { package = \"autumn-web\", version = \"0.5\" }\n",
        )
        .unwrap();
        let app_dir = tmp.path().join("app");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(
            app_dir.join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\
             \n[dependencies]\nautumn_web = { workspace = true }\n",
        )
        .unwrap();
        fs::create_dir_all(app_dir.join("src")).unwrap();
        fs::write(app_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let plan = plan_tauri(&app_dir).unwrap();
        let sh: &str = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("stage-sidecar.sh"))
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .expect("stage-sidecar.sh action not found");
        assert!(
            sh.contains("autumn_web/managed-pg-bundled"),
            "resolve_dep_key must resolve the workspace-inherited alias 'autumn_web' \
             (not 'autumn-web') when the member dep has workspace = true; got:\n{sh}"
        );
        assert!(
            !sh.contains("autumn-web/managed-pg-bundled"),
            "stage-sidecar.sh must not use the package name 'autumn-web' when it is \
             aliased as 'autumn_web' via workspace inheritance; got:\n{sh}"
        );
    }

    #[test]
    fn lib_rs_writes_signing_secret_with_restricted_permissions() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // The generated code must use platform-specific file creation so the secret
        // is not world-readable on Unix (default umask 022 → 0644).
        assert!(
            lib.contains("0o600") || lib.contains("OpenOptionsExt"),
            "load_or_generate_signing_secret must write the secret file with mode 0600 on Unix; \
             std::fs::write uses the default umask and leaves the file world-readable"
        );
        assert!(
            lib.contains("#[cfg(unix)]"),
            "signing secret write must be gated on #[cfg(unix)] to use mode 0600 there \
             while falling back to platform defaults elsewhere"
        );
    }

    #[test]
    fn tauri_conf_bundles_autumn_toml_as_resource() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        // Tauri v2 resources must be a map { source_path: dest_path }, not an array.
        let resources = parsed["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources must be a map (Tauri v2 schema requirement)");
        let has_autumn_toml = resources.iter().any(|(k, v)| {
            k.contains("autumn.toml") || v.as_str().is_some_and(|s| s.contains("autumn.toml"))
        });
        assert!(
            has_autumn_toml,
            "tauri.conf.json must bundle autumn.toml as a resource so the installed \
             sidecar can find the app's production configuration"
        );
        // No glob entries — globs cause GlobPathNotFound when no files match.
        let has_glob = resources.iter().any(|(k, _)| k.contains('*'));
        assert!(
            !has_glob,
            "tauri.conf.json must not emit resource glob entries — Tauri fails with \
             GlobPathNotFound when the glob matches no files (common for fresh projects)"
        );
        // Both alias names for prod and dev must be included so the staging script's
        // alias-pair copy logic always has a resource destination for each name,
        // and the sidecar finds the config regardless of AUTUMN_ENV spelling.
        let has_prod = resources
            .keys()
            .any(|k| k.contains("configs/") && k.contains("autumn-prod.toml"));
        assert!(
            has_prod,
            "tauri.conf.json must include configs/autumn-prod.toml so autumn-prod.toml \
             is bundled even when added after `autumn generate tauri`"
        );
        let has_production = resources
            .keys()
            .any(|k| k.contains("configs/") && k.contains("autumn-production.toml"));
        assert!(
            has_production,
            "tauri.conf.json must include configs/autumn-production.toml (prod alias)"
        );
        let has_dev = resources
            .keys()
            .any(|k| k.contains("configs/") && k.contains("autumn-dev.toml"));
        assert!(
            has_dev,
            "tauri.conf.json must include configs/autumn-dev.toml so the dev alias \
             is bundled alongside autumn-development.toml"
        );
        let has_development = resources
            .keys()
            .any(|k| k.contains("configs/") && k.contains("autumn-development.toml"));
        assert!(
            has_development,
            "tauri.conf.json must include configs/autumn-development.toml (dev alias)"
        );
    }

    #[test]
    fn tauri_conf_bundles_credentials_directory_as_resource() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let resources = parsed["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources must be a map");
        // The staging script always creates src-tauri/configs/credentials/ (possibly
        // empty), and the resource entry maps it to config/credentials/ in the bundle
        // so AutumnConfig can find .toml.enc files at AUTUMN_MANIFEST_DIR/config/credentials/.
        let has_credentials = resources.iter().any(|(k, v)| {
            k.contains("credentials") || v.as_str().is_some_and(|s| s.contains("credentials"))
        });
        assert!(
            has_credentials,
            "tauri.conf.json must bundle configs/credentials as a resource so apps \
             using config.credentials() find their .toml.enc files in the installed \
             bundle at AUTUMN_MANIFEST_DIR/config/credentials/<profile>.toml.enc"
        );
    }

    #[test]
    fn staging_sh_stages_credentials_directory() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        assert!(
            sh.contains("credentials"),
            "stage-sidecar.sh must create src-tauri/configs/credentials/ and copy \
             config/credentials/ into it so the tauri.conf.json resource entry is \
             satisfiable and .toml.enc files are bundled for installed apps"
        );
        assert!(
            sh.contains("mkdir") && sh.contains("configs/credentials"),
            "stage-sidecar.sh must always create src-tauri/configs/credentials/ \
             (even when empty) so the tauri.conf.json resource entry never causes \
             a GlobPathNotFound/missing-source error at bundle time"
        );
        assert!(
            sh.contains("AUTUMN_MASTER_KEY"),
            "stage-sidecar.sh must mention AUTUMN_MASTER_KEY so developers know how \
             to provide the decryption key for bundled .toml.enc credential files"
        );
    }

    #[test]
    fn staging_sh_clears_credentials_before_copying() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        // Stale .toml.enc files from a prior build (deleted or rotated credentials)
        // must not survive into the next installer.  The staging directory must be
        // removed and recreated, not merely appended to.
        assert!(
            sh.contains("rm -rf") && sh.contains("configs/credentials"),
            "stage-sidecar.sh must remove src-tauri/configs/credentials/ before \
             recreating it so revoked or renamed .toml.enc files from prior builds \
             are not silently bundled into the next installer"
        );
        // rm -rf must come before the mkdir that follows it.
        let rm_pos = sh.find("rm -rf").unwrap_or(usize::MAX);
        let mkdir_pos = sh
            .find("mkdir -p src-tauri/configs/credentials")
            .unwrap_or(usize::MAX);
        assert!(
            rm_pos < mkdir_pos,
            "stage-sidecar.sh must rm -rf credentials dir BEFORE mkdir; \
             otherwise the directory is never actually cleared"
        );
    }

    #[test]
    fn staging_sh_passes_package_flag_to_cargo_build() {
        let sh = render_stage_sidecar_sh("my-app", "my-app", true, "autumn-web");
        // In workspaces where `default-members` excludes the root package,
        // `cargo build --bin <name>` fails unless `-p <package>` is also passed.
        // The fingerprint phase already passes -p; the final cargo build must too.
        assert!(
            sh.contains("-p my-app"),
            "stage-sidecar.sh must pass `-p my-app` to cargo build so the correct \
             package is selected in workspaces where default-members excludes it; \
             without -p, Cargo may report 'no bin target named ...' for valid workspaces"
        );
    }

    #[test]
    fn lib_rs_documents_autumn_master_key() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("AUTUMN_MASTER_KEY"),
            "lib.rs must mention AUTUMN_MASTER_KEY so developers wiring \
             config.credentials() know how to pass the decryption key to the sidecar \
             at desktop launch time"
        );
    }

    #[test]
    fn lib_rs_documents_config_master_key_file_path() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // The key file is `<AUTUMN_MANIFEST_DIR>/config/master.key`, not
        // `.autumn-master-key`.  Documenting the wrong path leads to NoKeyFound
        // errors for developers who follow the comment.
        assert!(
            lib.contains("config/master.key"),
            "lib.rs must document the correct key file path (config/master.key) so \
             developers who follow the comment can place the key where autumn's \
             credentials resolver actually looks; the resolver checks AUTUMN_MASTER_KEY \
             env var first, then <base_dir>/config/master.key"
        );
    }

    #[test]
    fn lib_rs_disables_secure_cookies_for_loopback() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // Prod profile's session.secure=true emits the Secure attribute.
        // Browsers never send Secure cookies over plain HTTP, so sessions, auth,
        // and flash messages silently fail on http://127.0.0.1:<port>.
        assert!(
            lib.contains("AUTUMN_SESSION__SECURE"),
            "lib.rs must set AUTUMN_SESSION__SECURE=false; prod profile sets \
             session.secure=true which prevents cookies being sent over the \
             non-HTTPS loopback origin, silently breaking sessions/auth/flash"
        );
        assert!(
            lib.contains("\"AUTUMN_SESSION__SECURE\", \"false\""),
            "AUTUMN_SESSION__SECURE must be set to \"false\"; the sidecar is \
             loopback-only so Secure cookies add no security but break functionality"
        );
    }

    #[test]
    fn lib_rs_aborts_readiness_poll_on_sidecar_terminated_event() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("CommandEvent::Terminated"),
            "readiness poll must check for early sidecar termination via CommandEvent::Terminated"
        );
        assert!(
            lib.contains("try_recv"),
            "readiness poll must drain the event receiver with try_recv() each iteration"
        );
        assert!(
            lib.contains("CommandEvent"),
            "CommandEvent must be imported so Terminated variant is in scope"
        );
    }

    #[test]
    fn lib_rs_kills_sidecar_on_timeout() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        // The timeout branch must kill the sidecar before handle.exit(1);
        // check that .kill() appears more than once (once for Destroyed, once for timeout).
        let kill_count = lib.matches(".kill()").count();
        assert!(
            kill_count >= 2,
            "lib.rs must kill the sidecar in both the timeout path and the window-build \
             failure path, not only in WindowEvent::Destroyed; found {kill_count} .kill() call(s)"
        );
    }

    #[test]
    fn tauri_conf_identifier_replaces_underscores() {
        let conf = render_tauri_conf("my_app", "0.1.0", "my_app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let id = parsed["identifier"].as_str().unwrap();
        assert!(
            !id.contains('_'),
            "bundle identifier must not contain underscores (invalid per Apple spec), got: {id}"
        );
        assert!(
            id.contains("my-app"),
            "bundle identifier must use hyphens instead of underscores, got: {id}"
        );
    }

    #[test]
    fn tauri_conf_security_is_under_app() {
        let conf = render_tauri_conf("my-app", "0.1.0", "my-app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert!(
            parsed["app"]["security"].is_object(),
            "security config must be nested under 'app' (Tauri v2 schema requirement)"
        );
        assert!(
            parsed["security"].is_null(),
            "security must NOT appear at the top level (invalid in Tauri v2 schema)"
        );
    }

    // ── render_shell_main_rs ─────────────────────────────────────────────────

    #[test]
    fn shell_main_rs_has_windows_subsystem_attr() {
        let main = render_shell_main_rs("my-app");
        assert!(
            main.contains("windows_subsystem"),
            "main.rs must set windows_subsystem to suppress console on Windows"
        );
    }

    #[test]
    fn shell_main_rs_calls_run() {
        let main = render_shell_main_rs("my-app");
        assert!(
            main.contains("::run()"),
            "main.rs must call the lib's run() function"
        );
    }

    // ── render_prerequisites ─────────────────────────────────────────────────

    #[test]
    fn prerequisites_mentions_tauri_cli() {
        let prereq = render_prerequisites();
        assert!(
            prereq.contains("tauri-cli") || prereq.contains("cargo tauri"),
            "prerequisites must mention the Tauri CLI"
        );
    }

    #[test]
    fn prerequisites_mentions_linux_toolchain() {
        let prereq = render_prerequisites();
        assert!(
            prereq.contains("webkit2gtk") || prereq.contains("libwebkit"),
            "prerequisites must mention the Linux WebKit dependency"
        );
    }

    #[test]
    fn prerequisites_mentions_macos_toolchain() {
        let prereq = render_prerequisites();
        assert!(
            prereq.contains("xcode") || prereq.contains("Xcode"),
            "prerequisites must mention Xcode for macOS"
        );
    }

    #[test]
    fn prerequisites_mentions_embed_assets_feature() {
        let prereq = render_prerequisites();
        assert!(
            prereq.contains("embed-assets"),
            "prerequisites must document the embed-assets dependency (#1004)"
        );
    }

    #[test]
    fn prerequisites_mentions_managed_pg_feature() {
        let prereq = render_prerequisites();
        assert!(
            prereq.contains("managed-pg"),
            "prerequisites must document the managed-pg dependency (#1119)"
        );
    }

    // ── placeholder icons ─────────────────────────────────────────────────────

    #[test]
    fn placeholder_png_starts_with_png_signature() {
        assert_eq!(
            &PLACEHOLDER_PNG[..8],
            &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
            "placeholder PNG must start with the PNG file signature"
        );
    }

    #[test]
    fn placeholder_ico_starts_with_ico_header() {
        assert_eq!(
            &PLACEHOLDER_ICO[..4],
            &[0x00, 0x00, 0x01, 0x00],
            "placeholder ICO must start with the ICO reserved+type header"
        );
    }

    #[test]
    fn placeholder_icns_starts_with_icns_magic() {
        assert_eq!(
            &PLACEHOLDER_ICNS[..4],
            b"icns",
            "placeholder ICNS must start with the 'icns' magic bytes"
        );
    }

    // ── additive (does not touch app's src/main.rs or root Cargo.toml) ───────

    #[test]
    fn generate_then_destroy_tauri_round_trips_to_original_project_state() {
        // `plan_tauri` never pushes a `Modify` action (see
        // `plan_does_not_modify_app_main_rs`/`plan_does_not_modify_root_cargo_toml`
        // below) — everything it emits lives under the freshly-created
        // `src-tauri/` directory, so `Plan::revert`'s generic
        // Create-deletion + empty-directory pruning should already restore
        // the project exactly, with no per-generator `Revert` wiring needed.
        let tmp = project("my-app");
        let cargo_path = tmp.path().join("Cargo.toml");
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src-tauri").exists());

        plan_tauri(tmp.path())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(
            !tmp.path().join("src-tauri").exists(),
            "src-tauri/ must be fully removed and pruned"
        );
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn plan_does_not_modify_app_main_rs() {
        let tmp = project("my-app");
        let plan = plan_tauri(tmp.path()).unwrap();
        let app_main = tmp.path().join("src/main.rs");
        assert!(
            !plan.actions.iter().any(|a| a.path() == app_main.as_path()),
            "plan must not touch the app's src/main.rs"
        );
    }

    #[test]
    fn plan_does_not_modify_root_cargo_toml() {
        let tmp = project("my-app");
        let plan = plan_tauri(tmp.path()).unwrap();
        let root_cargo = tmp.path().join("Cargo.toml");
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| a.path() == root_cargo.as_path()),
            "plan must not touch the root Cargo.toml"
        );
    }

    // ── icon reuse when PWA generator already ran ─────────────────────────────

    #[test]
    fn plan_reuses_pwa_icon_when_present() {
        let tmp = project("my-app");
        // Simulate that `autumn generate pwa` already created the icon
        fs::create_dir_all(tmp.path().join("static/icons")).unwrap();
        fs::write(
            tmp.path().join("static/icons/icon.svg"),
            "<svg><!-- pwa icon --></svg>\n",
        )
        .unwrap();

        let plan = plan_tauri(tmp.path()).unwrap();
        let svg_action = plan
            .actions
            .iter()
            .find(|a| {
                a.path()
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with("icons/icon.svg")
            })
            .expect("icon.svg must be in the plan");

        if let super::super::emit::Action::CreateIfAbsent { contents, .. } = svg_action {
            assert!(
                contents.contains("pwa icon"),
                "must reuse the PWA icon content"
            );
        } else {
            panic!("icon.svg must use CreateIfAbsent action");
        }
    }

    // ── plan execution (full write to tempdir) ────────────────────────────────

    #[test]
    fn execute_creates_tauri_conf_json() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let path = tmp.path().join("src-tauri/tauri.conf.json");
        assert!(path.exists(), "src-tauri/tauri.conf.json must be created");
        let content = fs::read_to_string(&path).unwrap();
        let _: serde_json::Value =
            serde_json::from_str(&content).expect("tauri.conf.json must be valid JSON");
    }

    #[test]
    fn execute_creates_shell_cargo_toml() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src-tauri/Cargo.toml").exists());
    }

    #[test]
    fn execute_creates_lib_rs() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let lib = fs::read_to_string(tmp.path().join("src-tauri/src/lib.rs")).unwrap();
        assert!(
            lib.contains("127.0.0.1:0"),
            "lib.rs must bind ephemeral port"
        );
        assert!(lib.contains(".kill()"), "lib.rs must kill sidecar on close");
    }

    #[test]
    fn execute_creates_png_icon_files() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        for name in &["32x32.png", "128x128.png", "128x128@2x.png", "icon.png"] {
            let path = tmp.path().join("src-tauri/icons").join(name);
            assert!(path.exists(), "{name} must be created");
            let bytes = fs::read(&path).unwrap();
            assert_eq!(
                &bytes[..8],
                &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
                "{name} must be a valid PNG"
            );
        }
    }

    #[test]
    fn execute_creates_gitignore() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let gi = fs::read_to_string(tmp.path().join("src-tauri/.gitignore")).unwrap();
        assert!(gi.contains("/target"), ".gitignore must exclude /target");
        assert!(
            gi.contains("/binaries"),
            ".gitignore must exclude /binaries"
        );
        assert!(
            gi.contains("/configs"),
            ".gitignore must exclude /configs (staging area for profile config files)"
        );
    }

    #[test]
    fn execute_does_not_touch_app_main_rs() {
        let tmp = project("my-app");
        let original_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let after_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(
            original_main, after_main,
            "src/main.rs must be unchanged after generate tauri"
        );
    }

    #[test]
    fn execute_does_not_touch_root_cargo_toml() {
        let tmp = project("my-app");
        let original_cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let after_cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            original_cargo, after_cargo,
            "root Cargo.toml must be unchanged after generate tauri"
        );
    }

    #[test]
    fn execute_is_idempotent_with_force() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        // Second run with --force must not corrupt files
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();
        let conf = fs::read_to_string(tmp.path().join("src-tauri/tauri.conf.json")).unwrap();
        let _: serde_json::Value = serde_json::from_str(&conf)
            .expect("tauri.conf.json must still be valid JSON after re-run");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();
        assert!(
            !tmp.path().join("src-tauri").exists(),
            "dry-run must not create any files"
        );
    }

    #[test]
    fn collision_without_force_errors() {
        let tmp = project("my-app");
        plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let err = plan_tauri(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap_err();
        assert!(
            matches!(err, GenerateError::Collisions(_)),
            "re-running without --force must return a Collisions error"
        );
    }

    // ── lib.rs background thread + timeout behaviour ──────────────────────────

    #[test]
    fn lib_rs_uses_background_thread_for_health_poll() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("thread::spawn"),
            "lib.rs must move the health poll into a background thread so setup() \
             returns immediately and the Tauri event loop starts"
        );
    }

    #[test]
    fn lib_rs_exits_app_on_sidecar_timeout() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains(".exit("),
            "lib.rs must call handle.exit() when the sidecar fails to become ready, \
             not silently open a blank window"
        );
    }

    #[test]
    fn lib_rs_uses_connect_timeout_for_health_poll() {
        let lib = render_shell_lib_rs("my-app", "my-app");
        assert!(
            lib.contains("connect_timeout"),
            "lib.rs must use TcpStream::connect_timeout so each poll attempt is bounded"
        );
    }

    // ── productName title-case handles snake_case package names ──────────────

    #[test]
    fn tauri_conf_product_name_handles_underscore_separator() {
        let conf = render_tauri_conf("my_app", "0.1.0", "my_app");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let name = parsed["productName"].as_str().unwrap();
        assert!(
            name.contains(' '),
            "productName for 'my_app' must contain spaces (title-case), got: {name}"
        );
        assert!(
            !name.contains('_'),
            "productName for 'my_app' must not contain underscores, got: {name}"
        );
    }

    // ── resolve_dep_key unit tests ────────────────────────────────────────────

    #[test]
    fn resolve_dep_key_no_dependencies_section_returns_package_name() {
        // When Cargo.toml has no [dependencies] table, resolve_dep_key must fall
        // through the early-return path and return the raw package name unchanged.
        let doc: toml::Value =
            toml::from_str("[package]\nname=\"foo\"\nversion=\"0.1.0\"\n").unwrap();
        let result = resolve_dep_key(Path::new("/"), &doc, "autumn-web");
        assert_eq!(
            result, "autumn-web",
            "resolve_dep_key must return the package name when no [dependencies] table exists"
        );
    }

    #[test]
    fn resolve_dep_key_direct_package_alias() {
        // When a dep entry has `package = "autumn-web"` under a different key, the
        // function must return the alias key, not the package name.
        let doc: toml::Value = toml::from_str(
            "[package]\nname=\"foo\"\nversion=\"0.1.0\"\n\
             \n[dependencies]\nautumn_web = { version = \"0.5\", package = \"autumn-web\" }\n",
        )
        .unwrap();
        let result = resolve_dep_key(Path::new("/"), &doc, "autumn-web");
        assert_eq!(
            result, "autumn_web",
            "resolve_dep_key must return the alias key 'autumn_web' for package 'autumn-web'"
        );
    }

    #[test]
    fn resolve_dep_key_no_matching_dep_returns_package_name() {
        // When none of the [dependencies] entries match the target package name,
        // resolve_dep_key must return the package name itself as fallback.
        let doc: toml::Value = toml::from_str(
            "[package]\nname=\"foo\"\nversion=\"0.1.0\"\n\
             \n[dependencies]\nserde = \"1.0\"\ntokio = { version = \"1.0\" }\n",
        )
        .unwrap();
        let result = resolve_dep_key(Path::new("/"), &doc, "autumn-web");
        assert_eq!(
            result, "autumn-web",
            "resolve_dep_key must return the package name when no entry matches it"
        );
    }

    // ── resolve_workspace_dep_package unit tests ──────────────────────────────

    #[test]
    fn resolve_workspace_dep_package_stops_at_workspace_root_without_dep() {
        // When the workspace root Cargo.toml exists but has no [workspace.dependencies]
        // entry for the key, the function must stop (not walk further) and return None.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\n\
             \n[dependencies]\nautumn_web = { workspace = true }\n",
        )
        .unwrap();
        // resolve_workspace_dep_package must find the workspace root Cargo.toml,
        // see that [workspace] exists but has no matching dep, and return None.
        let result = resolve_workspace_dep_package(&app, "autumn_web");
        assert_eq!(
            result, None,
            "must return None when workspace root has no matching [workspace.dependencies] entry"
        );
    }

    #[test]
    fn resolve_workspace_dep_package_returns_none_when_no_workspace_found() {
        // When walking from the project root finds no Cargo.toml with [workspace],
        // the function exhausts all ancestors and returns None.
        let tmp = TempDir::new().unwrap();
        // No Cargo.toml with [workspace] — the function will walk up past tmp root
        // to filesystem root without finding one.
        let result = resolve_workspace_dep_package(tmp.path(), "autumn_web");
        assert_eq!(
            result, None,
            "must return None when no ancestor Cargo.toml contains [workspace]"
        );
    }

    // ── resolve_bin_name unit tests ────────────────────────────────────────────

    #[test]
    fn resolve_bin_name_default_run_without_bin_section() {
        // When [package] has default-run but no [[bin]] array, resolve_bin_name
        // must return the default-run value (the path after the [[bin]] block is
        // absent, so control reaches the second default_run check at line 328).
        let tmp = TempDir::new().unwrap();
        let doc: toml::Value = toml::from_str(
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\ndefault-run=\"webserver\"\n\
             \n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        let result = resolve_bin_name(tmp.path(), "my-app", Some("webserver"), true, &doc);
        assert_eq!(
            result.unwrap(),
            "webserver",
            "resolve_bin_name must return default_run when there is no [[bin]] section"
        );
    }

    #[test]
    fn resolve_bin_name_empty_src_bin_dir_errors_with_no_binary_target() {
        // When src/bin/ exists but contains no .rs files and no dir/main.rs,
        // the 0 => {} match arm is hit (no binary found there either) and the
        // function falls through to the final Err path.  Also exercises the
        // `None` arm in the filter_map for non-.rs files.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        // A non-.rs file: triggers the `None` arm of the filter_map.
        fs::write(tmp.path().join("src/bin/README.md"), "# ignore me").unwrap();
        let doc: toml::Value =
            toml::from_str("[package]\nname=\"my-app\"\nversion=\"0.1.0\"\n").unwrap();
        let err = resolve_bin_name(tmp.path(), "my-app", None, true, &doc).unwrap_err();
        assert!(
            err.to_string().contains("no binary target"),
            "must error about no binary target; got: {err}"
        );
    }

    #[test]
    fn resolve_bin_name_absolute_path_to_src_main_matches_package_name() {
        // Cargo permits absolute [[bin]] path values.  When the absolute path
        // points to <project_root>/src/main.rs it must be treated the same as
        // the relative "src/main.rs" so the package-name bin is chosen.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        let abs_path = root.join("src/main.rs");
        let abs_str = abs_path.to_string_lossy();
        // Escape backslashes for TOML (matters on Windows paths in tests).
        let toml_path = abs_str.replace('\\', "\\\\");
        let manifest = format!(
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\n\
             [[bin]]\nname=\"my-app\"\npath=\"{toml_path}\"\n"
        );
        let doc: toml::Value = toml::from_str(&manifest).unwrap();
        let result = resolve_bin_name(root, "my-app", None, true, &doc).unwrap();
        assert_eq!(
            result, "my-app",
            "absolute path to src/main.rs must resolve to the package-name bin"
        );
    }

    #[test]
    fn resolve_bin_name_absolute_path_outside_project_root_is_not_main() {
        // An absolute [[bin]] path that does not live under project_root cannot
        // be the package's src/main.rs; strip_prefix fails → treated as non-main.
        // Use autobins=false so the autobins guard doesn't fire and the single-bin
        // fallback picks up the "external" name directly.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        // Point the single [[bin]] to an arbitrary absolute path outside root.
        let manifest = "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\nautobins=false\n\
             [[bin]]\nname=\"external\"\npath=\"/some/other/location/main.rs\"\n";
        let doc: toml::Value = toml::from_str(manifest).unwrap();
        // The single non-main bin must be returned directly.
        let result = resolve_bin_name(root, "my-app", None, false, &doc).unwrap();
        assert_eq!(
            result, "external",
            "a single non-main absolute-path bin must be returned as-is"
        );
    }

    // ── version fallback unit test ─────────────────────────────────────────────

    #[test]
    fn plan_uses_fallback_version_when_no_version_field_in_cargo_toml() {
        // When [package] has no version field at all, the `_ => "0.1.0"` arm in
        // read_package_meta must fire and the plan must succeed with version "0.1.0".
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"no-version-app\"\nedition=\"2024\"\n\
             \n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let plan = plan_tauri(tmp.path()).unwrap();
        let conf_action = plan
            .actions
            .iter()
            .find(|a| a.path().to_string_lossy().ends_with("tauri.conf.json"))
            .expect("tauri.conf.json action must be present");
        let contents = match conf_action {
            crate::generate::emit::Action::Create { contents, .. } => contents.as_str(),
            _ => panic!("expected Create action for tauri.conf.json"),
        };
        let parsed: serde_json::Value = serde_json::from_str(contents).unwrap();
        assert_eq!(
            parsed["version"].as_str(),
            Some("0.1.0"),
            "when no version field is present, tauri.conf.json must default to '0.1.0'"
        );
    }

    // ── resolve_workspace_version unit tests ──────────────────────────────────

    #[test]
    fn resolve_workspace_version_walks_up_to_find_workspace_package_version() {
        // When the project is a workspace member and the version lives in the
        // parent Cargo.toml under [workspace.package], resolve_workspace_version
        // must walk up (hitting the dir = d.parent() line) and return the version.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\
             \n[workspace.package]\nversion = \"2.7.0\"\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        fs::create_dir_all(app.join("src")).unwrap();
        fs::write(
            app.join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion.workspace = true\nedition=\"2024\"\n\
             \n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        fs::write(app.join("src/main.rs"), "fn main() {}\n").unwrap();
        // resolve_workspace_version must walk from app/ up to root and find "2.7.0".
        let version = resolve_workspace_version(&app);
        assert_eq!(
            version.as_deref(),
            Some("2.7.0"),
            "must walk up to parent workspace and return the workspace.package.version"
        );
    }

    #[test]
    fn resolve_workspace_version_returns_none_when_no_ancestor_has_workspace_version() {
        // When no ancestor Cargo.toml has [workspace.package] version, the function
        // exhausts the directory tree and returns None (the final None after the loop).
        let tmp = TempDir::new().unwrap();
        // A bare Cargo.toml with no [workspace.package] section.
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion.workspace = true\nedition=\"2024\"\n\
             \n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        let version = resolve_workspace_version(tmp.path());
        assert_eq!(
            version, None,
            "must return None when no ancestor has [workspace.package] version"
        );
    }

    // ── Thin-client (mobile) mode tests — issue #1506 ─────────────────────────

    /// Fixture: a fresh project + its thin-client plan for the given remote URL.
    fn thin_plan(url: &str) -> (TempDir, Plan) {
        let tmp = project("my-app");
        let plan = plan_tauri_thin_client(tmp.path(), url)
            .expect("plan_tauri_thin_client must succeed for a valid https URL");
        (tmp, plan)
    }

    /// The contents of the `Create` action whose path ends with `suffix`.
    fn created_str<'a>(plan: &'a Plan, suffix: &str) -> &'a str {
        plan.actions
            .iter()
            .find(|a| {
                a.path()
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with(suffix)
            })
            .and_then(|a| {
                if let crate::generate::emit::Action::Create { contents, .. } = a {
                    Some(contents.as_str())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("expected a Create action for '{suffix}'"))
    }

    /// All planned action paths, relative to the project root, sorted.
    fn planned_paths(plan: &Plan, root: &std::path::Path) -> Vec<String> {
        let mut paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(root)
                    .expect("action paths are under the project root")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn thin_client_plan_creates_expected_file_set() {
        let (tmp, plan) = thin_plan("https://app.example.com");
        assert_eq!(
            planned_paths(&plan, tmp.path()),
            vec![
                "src-tauri/.gitignore",
                "src-tauri/Cargo.toml",
                "src-tauri/Info.ios.plist",
                "src-tauri/build.rs",
                "src-tauri/capabilities/remote-app.json",
                "src-tauri/icons/128x128.png",
                "src-tauri/icons/128x128@2x.png",
                "src-tauri/icons/32x32.png",
                "src-tauri/icons/icon.icns",
                "src-tauri/icons/icon.ico",
                "src-tauri/icons/icon.png",
                "src-tauri/icons/icon.svg",
                "src-tauri/src/lib.rs",
                "src-tauri/src/main.rs",
                "src-tauri/tauri.conf.json",
            ],
            "thin-client plan must emit exactly the mobile file set — \
             no staging scripts, no per-OS overlay confs"
        );
    }

    #[test]
    fn thin_client_capability_file_grants_remote_url() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let cap = created_str(&plan, "capabilities/remote-app.json");
        let parsed: serde_json::Value =
            serde_json::from_str(cap).expect("capability file must be valid JSON");
        assert_eq!(
            parsed["remote"]["urls"],
            serde_json::json!(["https://app.example.com"]),
            "capability must grant exactly the given remote origin (no wildcard)"
        );
        assert_eq!(
            parsed["windows"],
            serde_json::json!(["main"]),
            "capability must be scoped to the main window"
        );
    }

    #[test]
    fn thin_client_capability_file_grants_plugin_permissions() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let cap = created_str(&plan, "capabilities/remote-app.json");
        for permission in [
            "core:default",
            "notification:default",
            "biometric:default",
            "store:default",
        ] {
            assert!(
                cap.contains(permission),
                "capability file must grant '{permission}':\n{cap}"
            );
        }
    }

    #[test]
    fn thin_client_conf_has_no_sidecar_config() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let conf = created_str(&plan, "tauri.conf.json");
        assert!(
            !conf.contains("externalBin"),
            "thin-client tauri.conf.json must not reference a sidecar externalBin:\n{conf}"
        );
        assert!(
            !conf.contains("resources"),
            "thin-client tauri.conf.json must not stage sidecar resources:\n{conf}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(conf).expect("tauri.conf.json must be valid JSON");
        assert_eq!(parsed["productName"].as_str(), Some("My App"));
        assert_eq!(parsed["version"].as_str(), Some("0.1.0"));
        assert_eq!(
            parsed["identifier"].as_str(),
            Some("com.example.myapp"),
            "mobile identifier must contain no hyphens (they break \
             `cargo tauri android init` — tauri-apps/tauri#9707)"
        );
    }

    #[test]
    fn thin_client_identifier_strips_hyphens_and_underscores() {
        // Hyphens in the bundle identifier make `cargo tauri android init`
        // panic (tauri-apps/tauri#9707) and Apple forbids underscores — the
        // mobile identifier must keep only alphanumeric segments.
        for (package, expected) in [
            ("demo-app", "com.example.demoapp"),
            ("my_cool_app", "com.example.mycoolapp"),
        ] {
            let tmp = project(package);
            let plan = plan_tauri_thin_client(tmp.path(), "https://app.example.com").unwrap();
            let conf = created_str(&plan, "tauri.conf.json");
            let parsed: serde_json::Value = serde_json::from_str(conf).unwrap();
            assert_eq!(
                parsed["identifier"].as_str(),
                Some(expected),
                "'{package}' must derive an identifier valid on both Android and iOS"
            );
        }
    }

    #[test]
    fn thin_client_conf_enables_global_tauri() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let conf = created_str(&plan, "tauri.conf.json");
        let parsed: serde_json::Value = serde_json::from_str(conf).unwrap();
        assert_eq!(
            parsed["app"]["withGlobalTauri"],
            serde_json::json!(true),
            "remote server-rendered pages have no bundler — they call the \
             plugins via the injected window.__TAURI__ global:\n{conf}"
        );
    }

    #[test]
    fn thin_client_lib_rs_points_webview_at_remote_url() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let lib = created_str(&plan, "src/lib.rs");
        assert!(
            lib.contains("WebviewUrl::External"),
            "thin-client lib.rs must open the webview on an external URL:\n{lib}"
        );
        assert!(
            lib.contains("https://app.example.com"),
            "thin-client lib.rs must embed the exact remote URL:\n{lib}"
        );
        for sidecar_marker in ["tauri_plugin_shell", "SidecarHandle", "AUTUMN_SERVER__PORT"] {
            assert!(
                !lib.contains(sidecar_marker),
                "thin-client lib.rs must contain no sidecar machinery ('{sidecar_marker}')"
            );
        }
    }

    #[test]
    fn thin_client_lib_rs_registers_plugins_and_mobile_entry() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let lib = created_str(&plan, "src/lib.rs");
        assert!(
            lib.contains("tauri_plugin_notification::init()"),
            "lib.rs must register the notification plugin:\n{lib}"
        );
        assert!(
            lib.contains("tauri_plugin_store::Builder"),
            "lib.rs must register the store plugin:\n{lib}"
        );
        assert!(
            lib.contains("tauri_plugin_biometric"),
            "lib.rs must register the biometric plugin:\n{lib}"
        );
        assert!(
            lib.contains("#[cfg(mobile)]"),
            "biometric registration must be #[cfg(mobile)]-gated (plugin is mobile-only):\n{lib}"
        );
        assert!(
            lib.contains("#[cfg_attr(mobile, tauri::mobile_entry_point)]"),
            "lib.rs must keep the mobile entry point attribute:\n{lib}"
        );
    }

    #[test]
    fn thin_client_cargo_toml_declares_mobile_plugins() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let cargo = created_str(&plan, "src-tauri/Cargo.toml");
        assert!(
            cargo.contains("\"my-app-mobile\""),
            "shell crate must be named '{{package}}-mobile', not '-desktop':\n{cargo}"
        );
        assert!(
            cargo.contains("tauri-plugin-notification"),
            "Cargo.toml must depend on tauri-plugin-notification:\n{cargo}"
        );
        assert!(
            cargo.contains("tauri-plugin-store"),
            "Cargo.toml must depend on tauri-plugin-store:\n{cargo}"
        );
        assert!(
            cargo.contains(
                "[target.'cfg(any(target_os = \"android\", target_os = \"ios\"))'.dependencies]"
            ),
            "biometric dependency must be target-gated to mobile:\n{cargo}"
        );
        assert!(
            cargo.contains("tauri-plugin-biometric"),
            "Cargo.toml must depend on tauri-plugin-biometric:\n{cargo}"
        );
        assert!(
            !cargo.contains("tauri-plugin-shell"),
            "thin client has no sidecar, so tauri-plugin-shell must be absent:\n{cargo}"
        );
    }

    #[test]
    fn thin_client_emits_ios_faceid_plist() {
        let (_tmp, plan) = thin_plan("https://app.example.com");
        let plist = created_str(&plan, "Info.ios.plist");
        assert!(
            plist.contains("NSFaceIDUsageDescription"),
            "Info.ios.plist must declare NSFaceIDUsageDescription — the biometric \
             plugin hard-requires it on iOS:\n{plist}"
        );
    }

    #[test]
    fn thin_client_rejects_plain_http_url() {
        let tmp = project("my-app");
        let err = plan_tauri_thin_client(tmp.path(), "http://app.example.com")
            .expect_err("plain http remote URL must be rejected");
        match err {
            GenerateError::Config(msg) => assert!(
                msg.contains("https"),
                "error message must point at the https requirement: {msg}"
            ),
            other => panic!("expected GenerateError::Config, got: {other:?}"),
        }
    }

    #[test]
    fn thin_client_allows_http_localhost_for_dev() {
        let tmp = project("my-app");
        // 10.0.2.2 is the Android emulator's alias for the host machine's
        // loopback — the standard Android dev-loop target.
        for url in [
            "http://localhost:3000",
            "http://127.0.0.1:8080",
            "http://10.0.2.2:3000",
        ] {
            assert!(
                plan_tauri_thin_client(tmp.path(), url).is_ok(),
                "loopback http URL '{url}' must be accepted for dev"
            );
        }
    }

    #[test]
    fn thin_client_rejects_unparseable_url_and_userinfo() {
        let tmp = project("my-app");
        for url in ["not a url", "https://user:pass@app.example.com"] {
            let Err(err) = plan_tauri_thin_client(tmp.path(), url) else {
                panic!("'{url}' must be rejected as a remote URL");
            };
            assert!(
                matches!(err, GenerateError::Config(_)),
                "expected GenerateError::Config for '{url}', got: {err:?}"
            );
        }
    }

    #[test]
    fn thin_client_capability_scopes_grant_to_origin_not_full_url() {
        // The capability grant must be the path-free origin, while the
        // webview still loads the full URL including its path.
        let (_tmp, plan) = thin_plan("https://app.example.com:8443/base/");
        let cap = created_str(&plan, "capabilities/remote-app.json");
        let parsed: serde_json::Value = serde_json::from_str(cap).unwrap();
        assert_eq!(
            parsed["remote"]["urls"],
            serde_json::json!(["https://app.example.com:8443"]),
            "capability must grant the origin only — no path, no trailing slash"
        );
        let lib = created_str(&plan, "src/lib.rs");
        assert!(
            lib.contains("https://app.example.com:8443/base/"),
            "webview must load the full URL including its path:\n{lib}"
        );
    }

    #[test]
    fn thin_client_lib_rs_embeds_normalized_url_for_adversarial_inputs() {
        // `url::Url::parse` accepts inputs whose *raw* form is unsafe inside
        // a Rust string literal or `//!` doc comment (quotes, backslashes,
        // raw newlines — the parser percent-encodes or strips them only in
        // its own serialization). The generator must embed that normalized
        // serialization, never the raw input — or reject the URL outright.
        for raw in [
            "https://app.example.com/pa\"th",
            "https://app.example.com/x\\admin",
            "https://app.example.com/a\nfn evil(){}//",
            "https://app.example.com/{id}/x",
            "HTTPS://App.Example.COM/Path",
        ] {
            let tmp = project("my-app");
            let Ok(plan) = plan_tauri_thin_client(tmp.path(), raw) else {
                continue; // clean rejection is equally acceptable
            };
            let lib = created_str(&plan, "src/lib.rs");
            let normalized = url::Url::parse(raw)
                .expect("an accepted URL must reparse")
                .to_string();
            assert!(
                lib.contains(&normalized),
                "lib.rs must embed the normalized serialization '{normalized}' \
                 for raw input {raw:?}:\n{lib}"
            );
            if raw != normalized {
                assert!(
                    !lib.contains(raw),
                    "lib.rs must not embed the raw input {raw:?}:\n{lib}"
                );
            }
            assert!(
                !lib.contains('\\'),
                "generated lib.rs must contain no backslash (raw input {raw:?}):\n{lib}"
            );
            assert!(
                !lib.contains("fn evil"),
                "a raw newline in the URL must never break out of the doc \
                 comment into top-level source:\n{lib}"
            );
        }
    }

    #[test]
    fn thin_client_prerequisites_mention_mobile_init() {
        let text = render_thin_client_prerequisites("https://app.example.com");
        for anchor in [
            "tauri android init",
            "tauri ios init",
            "https://app.example.com",
            "4.2",
        ] {
            assert!(
                text.contains(anchor),
                "thin-client prerequisites must mention '{anchor}':\n{text}"
            );
        }
        // The echoed URL must be the normalized form actually granted in the
        // capability file, not the raw flag value.
        let echoed = render_thin_client_prerequisites("HTTPS://App.Example.COM");
        assert!(
            echoed.contains("https://app.example.com/"),
            "prerequisites must echo the normalized URL:\n{echoed}"
        );
    }

    #[test]
    fn desktop_plan_file_set_unchanged_by_thin_client_feature() {
        // Regression pin (issue #1506): the no-flag desktop path must stay
        // byte-identical in shape — exactly the pre-existing file list, and
        // in particular no capabilities/ entry leaking in from thin-client mode.
        let tmp = project("my-app");
        let plan = plan_tauri(tmp.path()).unwrap();
        assert_eq!(
            planned_paths(&plan, tmp.path()),
            vec![
                "src-tauri/.gitignore",
                "src-tauri/Cargo.toml",
                "src-tauri/build.rs",
                "src-tauri/icons/128x128.png",
                "src-tauri/icons/128x128@2x.png",
                "src-tauri/icons/32x32.png",
                "src-tauri/icons/icon.icns",
                "src-tauri/icons/icon.ico",
                "src-tauri/icons/icon.png",
                "src-tauri/icons/icon.svg",
                "src-tauri/src/lib.rs",
                "src-tauri/src/main.rs",
                "src-tauri/stage-sidecar.ps1",
                "src-tauri/stage-sidecar.sh",
                "src-tauri/tauri.conf.json",
                "src-tauri/tauri.linux.conf.json",
                "src-tauri/tauri.macos.conf.json",
                "src-tauri/tauri.windows.conf.json",
            ],
            "desktop `autumn generate tauri` (no --remote-url) must keep emitting \
             exactly the sidecar file set — and never a capabilities/ file"
        );
    }
}
