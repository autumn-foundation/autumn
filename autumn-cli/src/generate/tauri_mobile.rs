//! `autumn generate tauri-mobile` — scaffold a Tauri **mobile** wrapper that
//! runs the autumn server in-process (issue #1507, Option B).
//!
//! Mobile sandboxes (iOS and Android) forbid spawning child processes, so the
//! desktop **sidecar model** (`autumn generate tauri`) cannot ship on mobile:
//! there is no `externalBin`, no staging script, and no supervised server
//! binary here. Instead the Autumn Axum server runs on a **background thread
//! inside the app process itself**, spawned from `tauri::Builder::default()
//! .setup(...)` in the generated `src-tauri/src/lib.rs`, and the webview loads
//! the app from `http://127.0.0.1:<port>` once a `/health` poll confirms the
//! server is up. The database is a **remote Postgres** reached over the device
//! network; the template pins conservative pool defaults (small pool, short
//! connect timeout) tuned for flaky mobile networks.
//!
//! Because the server must be callable as a library (routes are registered
//! explicitly in the app's `main()`), the generator also performs an anchored
//! extraction of the stock `src/main.rs` into `src/lib.rs::serve()`, shrinking
//! `main.rs` to a thin caller. When `main.rs` was customised and the anchor no
//! longer matches, the extraction is skipped with a warning pointing at the
//! manual steps in `docs/guide/tauri-mobile-in-process.md`.
//!
//! # Generated files
//!
//! ```text
//! src-tauri/
//!   tauri.conf.json          — Tauri v2 config (no externalBin, no sidecar)
//!   Cargo.toml               — mobile shell crate: staticlib/cdylib/rlib,
//!                               depends on the app crate via path = ".."
//!   build.rs                 — calls tauri_build::build()
//!   src/main.rs              — desktop-dev entry: calls lib::run()
//!   src/lib.rs               — in-process lifecycle: free loopback port, env
//!                               config, server thread, /health poll, webview
//!   icons/*                  — placeholder icons so builds succeed out of the box
//!   .gitignore               — /target /gen
//! src/lib.rs                 — (app crate) extracted `pub async fn serve()`
//! src/main.rs                — (app crate) rewritten as a thin serve() caller
//! ```

use std::path::Path;

use super::emit::Plan;
use super::{GenerateError, ensure_project_root, read_or_empty};

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute the file actions for `autumn generate tauri-mobile`.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when not at a project root, or
/// [`GenerateError::Config`] if `Cargo.toml` is missing `[package].name`.
pub fn plan_tauri_mobile(project_root: &Path) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    let AppMeta {
        package_name,
        version: package_version,
        lib_ident,
        lib_src_path,
        has_embed_assets,
    } = read_app_meta(project_root)?;
    let mut plan = Plan::new(project_root);
    let tauri = project_root.join("src-tauri");

    if !has_embed_assets {
        plan.warn(
            "the app's Cargo.toml declares no `embed-assets` feature — the mobile \
             shell will link the app WITHOUT embedded assets, so static files \
             (CSS, htmx, widgets JS) will 404 on a device. Add \
             `embed-assets = [\"autumn-web/embed-assets\"]` under [features] and \
             re-run with --force (see docs/guide/tauri-mobile-in-process.md)."
                .to_owned(),
        );
    }

    // Core Tauri project files
    plan.create(
        tauri.join("tauri.conf.json"),
        render_mobile_tauri_conf(&package_name, &package_version),
    );
    plan.create(
        tauri.join("Cargo.toml"),
        render_mobile_cargo_toml(&package_name, has_embed_assets),
    );
    plan.create(tauri.join("build.rs"), render_mobile_build_rs());
    plan.create(
        tauri.join("src").join("main.rs"),
        render_mobile_main_rs(&package_name),
    );
    plan.create(
        tauri.join("src").join("lib.rs"),
        render_mobile_lib_rs(&package_name, &lib_ident),
    );

    // Icons — reuse the PWA icon when the user already ran `autumn generate pwa`.
    let icons_dir = tauri.join("icons");
    let pwa_icon_src = project_root.join("static").join("icons").join("icon.svg");
    if pwa_icon_src.is_file() {
        let contents = std::fs::read_to_string(&pwa_icon_src).map_err(GenerateError::Io)?;
        plan.create_if_absent(icons_dir.join("icon.svg"), contents);
    } else {
        plan.create_if_absent(icons_dir.join("icon.svg"), render_placeholder_icon_svg());
    }
    // Placeholder raster icons so `cargo tauri build` works immediately.
    // Replace with proper icons by running: cd src-tauri && cargo tauri icon icons/icon.svg
    plan.create_bytes(icons_dir.join("32x32.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("128x128.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("128x128@2x.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("icon.png"), PLACEHOLDER_PNG);
    plan.create_bytes(icons_dir.join("icon.ico"), PLACEHOLDER_ICO);
    plan.create_bytes(icons_dir.join("icon.icns"), PLACEHOLDER_ICNS);

    plan.create(tauri.join(".gitignore"), render_mobile_gitignore());

    // App-crate lib extraction so the shell can call `<app_lib>::serve()`
    // in-process.
    plan_lib_extraction(project_root, &lib_ident, &lib_src_path, &mut plan);

    Ok(plan)
}

// ── Mixed-mode guard (mirrors generate/tauri.rs, issue #1506) ─────────────────

/// Refuse to scaffold the mobile in-process shell on top of another Tauri
/// mode's `src-tauri/`.
///
/// `autumn generate tauri` (desktop sidecar), `autumn generate tauri
/// --remote-url` (mobile thin client), and `autumn generate tauri-mobile`
/// (mobile in-process) all write to `src-tauri/`, but their file sets only
/// partially overlap — `--force` would overwrite the shared files and leave
/// the other mode's leftovers behind, some of which actively break the
/// mobile build: Tauri CLI merges stale desktop `tauri.<platform>.conf.json`
/// overlays on top of `tauri.conf.json` (re-running the sidecar staging
/// scripts), and loads every capability file under `src-tauri/capabilities/`
/// (stale thin-client grants fail `tauri-build` validation against the
/// in-process shell crate, which compiles none of those plugins). `--force`
/// means "overwrite within the same mode", never "silently mix modes", so
/// this check runs regardless of `--force`.
///
/// Called only for `autumn generate` — never for `autumn destroy`, which is
/// the documented remedy and must keep working on a mixed tree.
///
/// # Errors
/// Returns [`GenerateError::Config`] naming the conflicting marker file and
/// the matching `autumn destroy tauri [--remote-url <URL>]` command to run
/// first.
pub fn ensure_no_other_mode_scaffold(project_root: &Path) -> Result<(), GenerateError> {
    let tauri = project_root.join("src-tauri");
    let first_existing = |markers: &[&str]| -> Option<String> {
        markers
            .iter()
            .find(|m| tauri.join(m).is_file())
            .map(|m| format!("src-tauri/{m}"))
    };
    if let Some(marker) = first_existing(&super::tauri::DESKTOP_MARKERS) {
        return Err(GenerateError::Config(format!(
            "src-tauri/ already contains a desktop (sidecar) Tauri scaffold \
             ({marker} exists); refusing to scaffold the mobile in-process \
             shell on top of it — stale per-OS tauri.*.conf.json overlays \
             would keep running the sidecar staging scripts on every `cargo \
             tauri build`. Run `autumn destroy tauri` first, then re-run \
             `autumn generate tauri-mobile`. --force only overwrites files \
             within the same mode; it never mixes modes."
        )));
    }
    if let Some(marker) = first_existing(&super::tauri::THIN_CLIENT_MARKERS) {
        return Err(GenerateError::Config(format!(
            "src-tauri/ already contains a mobile thin-client Tauri scaffold \
             ({marker} exists); refusing to scaffold the mobile in-process \
             shell on top of it — Tauri loads every capability file under \
             src-tauri/capabilities/, so the stale thin-client grants would \
             fail validation against the in-process shell crate. Run `autumn \
             destroy tauri --remote-url <URL>` (the URL the thin client was \
             generated with) first, then re-run `autumn generate \
             tauri-mobile`. --force only overwrites files within the same \
             mode; it never mixes modes."
        )));
    }
    Ok(())
}

/// Human-readable prerequisites message printed after a successful scaffold.
pub fn render_mobile_prerequisites() -> String {
    "\
Required prerequisites for building the mobile app:\n\
\n\
  1. Tauri CLI:\n\
       cargo install tauri-cli --version '^2'\n\
\n\
  2. Mobile toolchain:\n\
       iOS:     Xcode + iOS simulators (macOS only), then:\n\
                  cd src-tauri && cargo tauri ios init\n\
       Android: Android Studio / SDK + NDK (set ANDROID_HOME, NDK_HOME), then:\n\
                  cd src-tauri && cargo tauri android init\n\
\n\
  3. Point the app at your remote Postgres database:\n\
       edit src-tauri/src/lib.rs and set AUTUMN_DATABASE__URL (see the\n\
       commented example in run()).\n\
\n\
  4. Develop or build:\n\
       cd src-tauri && cargo tauri ios dev        (or: cargo tauri ios build)\n\
       cd src-tauri && cargo tauri android dev    (or: cargo tauri android build)\n\
       Note: Android release builds block cleartext HTTP (incl. 127.0.0.1) by\n\
       default — permit loopback via a network_security_config.xml (see the\n\
       docs page below).\n\
\n\
  The server runs IN-PROCESS on a background thread — mobile sandboxes forbid\n\
  sidecar processes, so this scaffold intentionally has no externalBin and no\n\
  staging scripts.  Read docs/guide/tauri-mobile-in-process.md for mobile\n\
  sandboxing restrictions, remote-Postgres pool tuning for flaky networks,\n\
  security notes, and App Store guideline compliance notes.\n\
\n\
  Replace the placeholder icons before shipping:\n\
       cd src-tauri && cargo tauri icon icons/icon.svg\n"
        .to_owned()
}

// ── Package metadata helper ───────────────────────────────────────────────────

/// App-crate metadata the mobile generator needs from `Cargo.toml`.
struct AppMeta {
    /// `[package].name`, verbatim — used for the shell crate name, the bundle
    /// identifier, the display title, and the path-dependency key.
    package_name: String,
    /// `[package].version`, with workspace inheritance resolved.
    version: String,
    /// The **library crate identifier** all generated `serve()` call sites
    /// use: `[lib].name` when the app sets one, otherwise `[package].name`
    /// with dashes replaced by underscores (Cargo's default lib target name).
    lib_ident: String,
    /// The **library target source file**, relative to the project root:
    /// `[lib].path` when the app sets one, otherwise `src/lib.rs` (Cargo's
    /// default). The `serve()` extraction must target this file — a newly
    /// created `src/lib.rs` would be ignored by Cargo when `[lib].path`
    /// points elsewhere.
    lib_src_path: String,
    /// Whether the app declares an `embed-assets` feature.
    has_embed_assets: bool,
}

/// Reads [`AppMeta`] from the app's `Cargo.toml`.
///
/// A deliberately small, self-contained reader: the mobile shell has no
/// sidecar, so — unlike the desktop generator — it needs no binary-target or
/// dependency-key resolution. `version` resolves workspace inheritance
/// (`version.workspace = true`) by walking up the directory tree.
/// `lib_ident` honors a custom `[lib] name = "…"`: Cargo exposes the library
/// crate to the app binary and to path dependents under that name, so the
/// generated `<lib_ident>::serve()` calls must use it — the dash-to-underscore
/// package name would not compile for those projects. `lib_src_path` honors a
/// custom `[lib] path = "…"` the same way: Cargo compiles THAT file as the
/// library target and ignores a stray `src/lib.rs`.
/// `has_embed_assets` is `true` when the app declares an `embed-assets`
/// feature (the stock scaffold always does) — the mobile shell enables it on
/// the app dependency so CSS/JS/static assets are compiled into the binary
/// (a mobile app has no working directory to serve `static/` from).
fn read_app_meta(project_root: &Path) -> Result<AppMeta, GenerateError> {
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

    let version = match pkg.get("version") {
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Table(t))
            if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
        {
            resolve_workspace_version(project_root).unwrap_or_else(|| "0.1.0".to_owned())
        }
        _ => "0.1.0".to_owned(),
    };

    let has_embed_assets = doc
        .get("features")
        .and_then(|f| f.get("embed-assets"))
        .is_some();

    let lib_ident = doc
        .get("lib")
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
        .map_or_else(|| name.replace('-', "_"), str::to_owned);

    let lib_src_path = doc
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
        .map_or_else(|| "src/lib.rs".to_owned(), str::to_owned);

    Ok(AppMeta {
        package_name: name,
        version,
        lib_ident,
        lib_src_path,
        has_embed_assets,
    })
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

// ── App-crate lib extraction ──────────────────────────────────────────────────

/// The exact prefix of the stock scaffold's entry point (`src/templates/
/// main.rs.tmpl`). The extraction only ever fires when this anchor matches,
/// so a customised `main.rs` is never mangled (same anchored-replacement
/// discipline as `new.rs::inject_managed_pg`).
const MAIN_FN_ANCHOR: &str = "#[autumn_web::main]\nasync fn main() {";

/// What the anchor is rewritten to in the extracted `src/lib.rs`.
const SERVE_FN_HEADER: &str = "\
/// Build and run the Autumn server.
///
/// Extracted from `main()` by `autumn generate tauri-mobile` so the Tauri
/// mobile shell (src-tauri/) can run the server in-process on a background
/// thread. The `main.rs` binary still calls this on desktop.
pub async fn serve() {";

/// Plan the anchored extraction of the app's `src/main.rs` into the app's
/// **library target file** (`serve()`), with a graceful skip when the app
/// doesn't match the stock scaffold shape.
///
/// `crate_ident` is the app's **library crate identifier** ([`AppMeta::
/// lib_ident`]) — `[lib].name` when set, else the dash-to-underscore package
/// name. `lib_src_path` is the library target's source file ([`AppMeta::
/// lib_src_path`]) — `[lib].path` when set, else `src/lib.rs`: Cargo compiles
/// THAT file as the library, so extracting into a hard-coded `src/lib.rs`
/// would leave the real lib target without `serve()` (and the shell would
/// not compile) while adding an unused stray file.
///
/// Uses `Modify` for `main.rs` (an intentional rewrite, not a collision) and
/// `CreateIfAbsent` for the lib file — so `autumn destroy tauri-mobile`
/// removes the `src-tauri/` shell but leaves the extracted (still fully
/// functional) app lib in place rather than deleting a lib file that
/// `main.rs` now depends on.
fn plan_lib_extraction(
    project_root: &Path,
    crate_ident: &str,
    lib_src_path: &str,
    plan: &mut Plan,
) {
    let main_path = project_root.join("src").join("main.rs");
    let lib_path = project_root.join(lib_src_path);
    let main_rs = read_or_empty(&main_path);

    if lib_path.exists() {
        // Re-run after a successful extraction: nothing to do, stay silent.
        if main_rs.contains(&format!("{crate_ident}::serve()")) {
            return;
        }
        plan.warn(format!(
            "{lib_src_path} already exists — skipping the automatic extraction \
             of src/main.rs. The Tauri mobile shell calls \
             `{crate_ident}::serve()`; expose your app as `pub async fn \
             serve()` in {lib_src_path} by hand \
             (see docs/guide/tauri-mobile-in-process.md)."
        ));
        return;
    }

    if !main_rs.contains(MAIN_FN_ANCHOR) {
        plan.warn(format!(
            "src/main.rs doesn't match the stock scaffold layout — skipping the \
             automatic extraction into {lib_src_path}. The Tauri mobile shell \
             calls `{crate_ident}::serve()`; move your `main()` body into a \
             `pub async fn serve()` in {lib_src_path} and call it from `main()` \
             (see docs/guide/tauri-mobile-in-process.md for the exact steps)."
        ));
        return;
    }

    let lib_rs = main_rs.replace(MAIN_FN_ANCHOR, SERVE_FN_HEADER);
    plan.create_if_absent(lib_path, lib_rs);
    plan.modify(main_path, render_thin_app_main_rs(crate_ident));
}

/// The rewritten app `src/main.rs`: a thin caller of the extracted
/// `lib.rs::serve()`, keeping desktop `cargo run` behavior identical.
fn render_thin_app_main_rs(crate_ident: &str) -> String {
    format!(
        "//! Thin binary entry point — the app itself lives in `src/lib.rs` so the\n\
         //! Tauri mobile shell (`src-tauri/`) can run the server in-process.\n\
         //! Generated by `autumn generate tauri-mobile`.\n\
         \n\
         #[autumn_web::main]\n\
         async fn main() {{\n\
         \x20   {crate_ident}::serve().await;\n\
         }}\n"
    )
}

// ── Content renderers ─────────────────────────────────────────────────────────

fn render_mobile_tauri_conf(package_name: &str, version: &str) -> String {
    // Bundle identifier: reverse-DNS with underscores replaced by hyphens.
    // Apple's spec allows only alphanumerics, hyphens, and periods.
    let identifier = format!("com.example.{}", package_name.replace('_', "-"));
    // Display title: capitalise first letter of each word; split on both '-'
    // and '_' so kebab-case and snake_case both work.
    let title: String = package_name
        .split(['-', '_'])
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().to_string() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    // Deliberately NO bundle.externalBin and NO bundle.resources: the mobile
    // model runs the server in-process (mobile sandboxes forbid sidecar
    // processes) and is configured entirely via environment variables set in
    // src/lib.rs — no staged config files.
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
    "security": {{
      "csp": null
    }}
  }}
}}
"#
    )
}

fn render_mobile_cargo_toml(package_name: &str, has_embed_assets: bool) -> String {
    let shell_name = format!("{package_name}-mobile");
    // Enable the app's `embed-assets` feature so CSS/htmx/static assets are
    // compiled into the binary: a mobile app has no working directory holding
    // a `static/` tree, so disk-served assets would 404 on device.
    let app_dep = if has_embed_assets {
        format!(
            "# The `embed-assets` feature compiles the app's static/ tree (CSS, htmx,\n\
             # widgets JS) into the binary — on a device there is no on-disk static/\n\
             # directory to serve from.\n\
             {package_name} = {{ path = \"..\", features = [\"embed-assets\"] }}"
        )
    } else {
        format!("{package_name} = {{ path = \"..\" }}")
    };
    format!(
        r#"[package]
name = "{shell_name}"
version = "0.0.1"
edition = "2021"

# Standalone workspace so this crate is independent from the autumn app workspace —
# no change to the root Cargo.toml is needed.
[workspace]

[lib]
# Mobile platforms link the app as a library rather than running a binary:
#   staticlib — iOS (linked into the generated Xcode project)
#   cdylib    — Android (loaded by the generated Android activity)
#   rlib      — lets src/main.rs run the same shell on desktop (cargo tauri dev)
crate-type = ["staticlib", "cdylib", "rlib"]

[build-dependencies]
tauri-build = {{ version = "2", features = [] }}

[dependencies]
tauri = {{ version = "2", features = [] }}
# Runtime for the in-process server thread (see src/lib.rs).
tokio = {{ version = "1", features = ["rt-multi-thread"] }}
getrandom = {{ version = "0.2", features = ["std"] }}
# The autumn app itself — the server runs IN-PROCESS on a background thread
# (mobile sandboxes forbid sidecar processes), so the shell links the app
# crate directly instead of bundling a server binary.
{app_dep}

[profile.release]
panic = "abort"
codegen-units = 1
lto = true
opt-level = "s"
strip = true
"#
    )
}

fn render_mobile_build_rs() -> String {
    "fn main() {\n    tauri_build::build()\n}\n".to_owned()
}

fn render_mobile_main_rs(package_name: &str) -> String {
    let lib_name = package_name.replace('-', "_") + "_mobile";
    format!(
        "#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]\n\
         \n\
         // Desktop-dev entry point for the mobile shell; on iOS/Android the\n\
         // platform loads the library through `tauri::mobile_entry_point`.\n\
         fn main() {{\n\
         \x20   {lib_name}::run();\n\
         }}\n"
    )
}

/// Render the shell's `src/lib.rs`. `crate_ident` is the app's library crate
/// identifier ([`AppMeta::lib_ident`]) used for the `<crate_ident>::serve()`
/// call — `[lib].name` when the app sets one.
#[allow(clippy::too_many_lines)]
fn render_mobile_lib_rs(package_name: &str, crate_ident: &str) -> String {
    format!(
        r#"//! Tauri mobile shell for {package_name} — in-process backend + remote Postgres.
//!
//! Mobile sandboxes (iOS and Android) forbid spawning child processes, so the
//! desktop sidecar model cannot work here. Instead the Autumn Axum server runs
//! on a background thread INSIDE this app process:
//!
//!   1. Bind loopback:0 to find a free ephemeral port.
//!   2. Configure the server through environment variables at the top of
//!      run(), before tauri::Builder starts any platform threads.
//!   3. std::thread::spawn a dedicated tokio runtime that block_on()s
//!      {crate_ident}::serve() — the app entry extracted into src/lib.rs by
//!      `autumn generate tauri-mobile`.
//!   4. Poll GET /health in a background thread until the server is ready,
//!      then open the webview at http://127.0.0.1:<port>.
//!
//! The database is a REMOTE Postgres reached over the device network. The env
//! defaults below (small pool, short connect timeout) are tuned for flaky
//! mobile networks — see docs/guide/tauri-mobile-in-process.md for the full
//! rationale, reconnection semantics, security notes, and App Store
//! compliance notes.

use std::net::TcpListener;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {{
    // 1. Find a free loopback port: bind :0, read the assigned port, then drop
    //    the listener so the in-process server can bind that same address.
    //    (The port is briefly unbound until the server thread claims it — see
    //    the security section of docs/guide/tauri-mobile-in-process.md.)
    let port = {{
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind a loopback port");
        listener
            .local_addr()
            .expect("failed to read the loopback port")
            .port()
    }};

    // 2. Configure the server via environment variables. The server runs in
    //    THIS process, so std::env::set_var is the whole config story — no
    //    staged config files (an autumn.toml in the app repo is NOT read by
    //    this shell). This block runs at the top of run(), BEFORE
    //    tauri::Builder starts platform runtimes: set_var is only sound while
    //    no foreign threads may call getenv concurrently. Only the values
    //    that need the app sandbox path are deferred to setup().
    std::env::set_var("AUTUMN_SERVER__HOST", "127.0.0.1");
    std::env::set_var("AUTUMN_SERVER__PORT", port.to_string());
    // ── Remote Postgres over a mobile network ───────────────────────────────
    // Point the app at your remote database (require TLS in production):
    // std::env::set_var(
    //     "AUTUMN_DATABASE__URL",
    //     "postgres://app_user:PASSWORD@db.example.com:5432/app?sslmode=require",
    // );
    //
    // Conservative pool defaults for flaky mobile networks: a phone gets one
    // user's traffic, so a large server-style pool only holds open sockets
    // that die on every network transition (Wi-Fi ⇄ cellular, backgrounding).
    // The deadpool-backed pool re-checks connections on checkout and
    // re-establishes broken ones automatically, so a small pool recovers
    // faster and wastes fewer half-dead connections.
    std::env::set_var("AUTUMN_DATABASE__POOL_SIZE", "2");
    // Fail fast on NEW connection attempts when the radio is off or asleep.
    // (5 is also the framework default — pinned here so the mobile posture
    // survives future framework default changes.)
    std::env::set_var("AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS", "5");
    // ────────────────────────────────────────────────────────────────────────
    // The webview loads the app over plain HTTP on loopback; Secure cookies
    // would be silently dropped by the webview, breaking sessions and CSRF.
    // Loopback never leaves the device — but other apps ON the device can
    // reach it too (see the security section of the docs page).
    std::env::set_var("AUTUMN_SESSION__SECURE", "false");
    // Loopback-only server: accept Host: 127.0.0.1 from the webview even when
    // production config pins trusted hosts to a public domain.
    std::env::set_var("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS", "127.0.0.1,localhost");
    // Local blob storage backend rooted in the app sandbox; the root path is
    // set in setup() once the sandbox data dir is known.
    std::env::set_var("AUTUMN_STORAGE__BACKEND", "local");
    std::env::set_var("AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION", "true");
    // No load balancer drains connections to an in-process loopback server;
    // skip the prestop grace so shutdown hooks run immediately.
    std::env::set_var("AUTUMN_SERVER__PRESTOP_GRACE_SECS", "0");
    // Profile selection: dev config during `cargo tauri [ios|android] dev`,
    // prod config in release builds. Set explicitly — running serve()
    // in-process bypasses the #[autumn_web::main] entry point that would
    // otherwise bake the release/debug signal into profile detection, so
    // without this a release build would silently run the dev profile.
    if cfg!(debug_assertions) {{
        std::env::set_var("AUTUMN_ENV", "dev");
    }} else {{
        std::env::set_var("AUTUMN_ENV", "prod");
    }}
    // Clear inherited one-off mode flags and desktop-only settings: any of
    // these would make AppBuilder::run() exit before binding the HTTP port
    // (build-static/dump-routes/task modes), bind a Unix socket the health
    // probe can't reach, or attach a foreign managed-Postgres cluster.
    for var in [
        "AUTUMN_PROFILE",
        "AUTUMN_BUILD_STATIC",
        "AUTUMN_DUMP_ROUTES",
        "AUTUMN_LIST_TASKS",
        "AUTUMN_RUN_TASK",
        "AUTUMN_SERVER__UNIX_SOCKET",
        "AUTUMN_SERVE_FORCE_UNIX_SOCKET",
        "AUTUMN_MANAGED_PG_ATTACH_URL",
    ] {{
        std::env::remove_var(var);
    }}

    tauri::Builder::default()
        .setup(move |app| setup(app, port))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}}

fn setup(app: &mut tauri::App, port: u16) -> Result<(), Box<dyn std::error::Error>> {{
    // 3. Per-app data directories (inside the mobile app sandbox). These two
    //    set_var calls are the only ones that must wait for the tauri App
    //    handle (the sandbox path is not known before the Builder runs); they
    //    still execute before the server thread reads the environment.
    let data_root = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_root)?;
    // Local blob storage in blobs/ (the only writable filesystem location).
    let blobs_dir = data_root.join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;
    std::env::set_var(
        "AUTUMN_STORAGE__LOCAL__ROOT",
        blobs_dir.to_string_lossy().as_ref(),
    );
    // Per-install signing secret: autumn requires one in prod mode. Generate
    // 32 random bytes on first launch and persist them so sessions survive
    // restarts.
    let signing_secret = load_or_generate_signing_secret(&data_root)?;
    std::env::set_var("AUTUMN_SECURITY__SIGNING_SECRET", &signing_secret);

    // 4. Spawn the Autumn server on a background thread with its own tokio
    //    runtime. tauri's setup() is synchronous and must return quickly, so
    //    the server gets a dedicated OS thread; block_on parks that thread on
    //    the server future for the lifetime of the app. Mobile OSes reclaim
    //    the whole process on exit — there is no separate child to clean up.
    std::thread::spawn(move || {{
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for the in-process autumn server");
        runtime.block_on({crate_ident}::serve());
    }});

    // 5. Poll for server readiness in a background thread so setup() returns
    //    immediately and the Tauri event loop starts (blocking here can
    //    trigger the mobile OS's app-startup watchdog). We probe GET /health —
    //    the cheap readiness endpoint autumn always registers; any HTTP
    //    response proves the server is up and routing.
    let handle = app.handle().clone();
    std::thread::spawn(move || {{
        let addr = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            port,
        );
        let poll_timeout = std::time::Duration::from_millis(200);
        let mut ready = false;
        // 300 × 200 ms = 60 s. No local Postgres cluster to initialise here
        // (the DB is remote), so startup is fast; 60 s leaves headroom for
        // slow first-launch migrations without hanging a broken install
        // forever behind a blank screen.
        for _ in 0..300 {{
            if let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, poll_timeout) {{
                let _ = stream.set_read_timeout(Some(poll_timeout));
                use std::io::{{Read, Write}};
                let req = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
                if stream.write_all(req.as_bytes()).is_ok() {{
                    let mut buf = [0u8; 8];
                    // Any valid HTTP response (200, 404, …) means the server is
                    // up and routing — accept the `HTTP/` prefix regardless of
                    // status.
                    if stream.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/") {{
                        ready = true;
                        break;
                    }}
                }}
            }}
            std::thread::sleep(poll_timeout);
        }}
        if !ready {{
            eprintln!("[{package_name}] Server did not become ready within 60 s.");
            show_startup_error(
                &handle,
                "The app's built-in server did not start in time. This is \
                 usually a configuration problem — for example an unreachable \
                 or misconfigured AUTUMN_DATABASE__URL. Check the device logs \
                 for details.",
            );
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
        .build()
        {{
            eprintln!("[{package_name}] Failed to open window: {{e}}");
            show_startup_error(&handle, "The app window could not be opened.");
        }}
    }});

    Ok(())
}}

/// Surface a startup failure in a visible window instead of a silent exit or
/// an endless blank screen. Falls back to exiting when even the error window
/// cannot be built. Note: some framework-level startup failures (for example
/// a database bootstrap error) call std::process::exit directly from the
/// server thread and terminate the app before this can render — see the
/// troubleshooting notes in docs/guide/tauri-mobile-in-process.md.
fn show_startup_error(handle: &tauri::AppHandle, message: &str) {{
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head>\
         <body style=\"font-family:system-ui;margin:2rem\">\
         <h1>{package_name} could not start</h1>\
         <p>{{message}}</p></body></html>"
    );
    let opened = format!("data:text/html,{{}}", percent_encode(&html))
        .parse()
        .ok()
        .and_then(|url| {{
            tauri::WebviewWindowBuilder::new(handle, "startup-error", tauri::WebviewUrl::External(url))
                .title("{package_name}")
                .build()
                .ok()
        }})
        .is_some();
    if !opened {{
        handle.exit(1);
    }}
}}

/// Minimal percent-encoding for embedding the error page in a data: URL
/// (avoids pulling an extra dependency into the shell crate).
fn percent_encode(input: &str) -> String {{
    input
        .bytes()
        .map(|b| match b {{
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {{
                char::from(b).to_string()
            }}
            _ => format!("%{{b:02X}}"),
        }})
        .collect()
}}

/// Generate a 32-byte random signing secret on first launch, persist it to
/// `{{data_root}}/signing_secret.txt`, and return it as a hex string.
/// Autumn requires a signing secret in prod mode to sign session tokens;
/// without one the release build aborts before binding the HTTP port.
fn load_or_generate_signing_secret(
    data_root: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {{
    let path = data_root.join("signing_secret.txt");
    if let Ok(s) = std::fs::read_to_string(&path) {{
        let s = s.trim().to_owned();
        if s.len() >= 32 {{
            return Ok(s);
        }}
    }}
    let mut bytes = [0u8; 32];
    // Propagate RNG failure — an all-zero secret would be trivially guessable.
    getrandom::getrandom(&mut bytes)?;
    let hex: String = bytes.iter().map(|b| format!("{{b:02x}}")).collect();
    // The app sandbox is private to this app on iOS/Android, but restrict
    // permissions anyway where the platform supports it.
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

fn render_mobile_gitignore() -> String {
    // /gen is where `cargo tauri ios init` / `cargo tauri android init` put
    // the generated Xcode / Gradle projects.
    "/target\n/gen\n".to_owned()
}

// ── Placeholder icon bytes ────────────────────────────────────────────────────
// Duplicated from generate/tauri.rs on purpose: the desktop generator is being
// reworked concurrently (#1506), so sharing these consts would create a merge
// conflict for a few dozen bytes. Dedup into a shared `generate::icons` module
// once both land.
// Minimal valid 1×1 RGBA PNG (autumn green #4F7942, opaque).
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
    use tempfile::TempDir;

    use super::super::emit::Action;
    use super::*;

    /// Find the planned action targeting the APP crate's `src/<name>` —
    /// as opposed to the shell's `src-tauri/src/<name>`.
    fn app_src_action<'p>(plan: &'p Plan, name: &str) -> Option<&'p Action> {
        plan.actions.iter().find(|a| {
            a.path().ends_with(format!("src/{name}"))
                && !a.path().to_string_lossy().contains("src-tauri")
        })
    }

    fn app_dir(name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.2.3\"\n\n\
                 [features]\nembed-assets = [\"autumn-web/embed-assets\"]\n"
            ),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        tmp
    }

    /// A minimal stand-in for the stock `main.rs.tmpl` output, carrying the
    /// exact `MAIN_FN_ANCHOR` shape.
    fn stock_main_rs() -> String {
        "use autumn_web::prelude::*;\n\n#[autumn_web::main]\nasync fn main() {\n    \
         let app = autumn_web::app()\n        .routes(routes![]);\n    app.run().await;\n}\n"
            .to_owned()
    }

    #[test]
    fn conf_has_identifier_product_name_and_no_external_bin() {
        let conf = render_mobile_tauri_conf("my-app", "1.2.3");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert_eq!(parsed["identifier"], "com.example.my-app");
        assert_eq!(parsed["productName"], "My App");
        assert_eq!(parsed["version"], "1.2.3");
        assert!(parsed["bundle"].get("externalBin").is_none());
        assert!(parsed["bundle"].get("resources").is_none());
    }

    #[test]
    fn conf_identifier_replaces_underscores() {
        let conf = render_mobile_tauri_conf("my_app", "0.1.0");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert_eq!(parsed["identifier"], "com.example.my-app");
        assert_eq!(parsed["productName"], "My App");
    }

    #[test]
    fn cargo_toml_is_mobile_library_without_sidecar_plugin() {
        let toml_src = render_mobile_cargo_toml("my-app", true);
        assert!(toml_src.contains(r#"crate-type = ["staticlib", "cdylib", "rlib"]"#));
        assert!(
            toml_src.contains(r#"my-app = { path = "..", features = ["embed-assets"] }"#),
            "shell must build the app with embedded assets (no static/ dir on device)"
        );
        assert!(toml_src.contains(r#"name = "my-app-mobile""#));
        assert!(!toml_src.contains("tauri-plugin-shell"));
        // The rendered manifest must be valid TOML.
        toml::from_str::<toml::Value>(&toml_src).expect("generated Cargo.toml must parse");
    }

    #[test]
    fn cargo_toml_omits_embed_assets_when_app_lacks_the_feature() {
        let toml_src = render_mobile_cargo_toml("my-app", false);
        assert!(toml_src.contains(r#"my-app = { path = ".." }"#));
        assert!(!toml_src.contains("embed-assets"));
        toml::from_str::<toml::Value>(&toml_src).expect("generated Cargo.toml must parse");
    }

    #[test]
    fn plan_warns_when_app_has_no_embed_assets_feature() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("embed-assets")),
            "must warn when the app declares no embed-assets feature, got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn lib_rs_spawns_in_process_server_with_pool_defaults() {
        let lib = render_mobile_lib_rs("my-app", "my_app");
        assert!(lib.contains("tauri::Builder::default()"));
        assert!(lib.contains(".setup("));
        assert!(lib.contains("std::thread::spawn"));
        assert!(lib.contains("block_on(my_app::serve())"));
        assert!(lib.contains("tauri::mobile_entry_point"));
        // The advertised flaky-network defaults are pinned by VALUE, not just
        // by variable name (comments also name the variables).
        assert!(lib.contains(r#"set_var("AUTUMN_DATABASE__POOL_SIZE", "2")"#));
        assert!(lib.contains(r#"set_var("AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS", "5")"#));
        assert!(lib.contains("AUTUMN_DATABASE__URL"));
        assert!(!lib.contains(".sidecar("));
        assert!(!lib.contains("tauri_plugin_shell"));
    }

    #[test]
    fn lib_rs_pins_prod_profile_in_release_builds() {
        let lib = render_mobile_lib_rs("my-app", "my_app");
        // Release builds must run the prod profile: serve() bypasses the
        // #[autumn_web::main] macro that would otherwise mark release builds,
        // so leaving AUTUMN_ENV unset would silently fall back to dev
        // (permissive CORS, debug logging, no fail-fast secret checks).
        assert!(lib.contains(r#"set_var("AUTUMN_ENV", "prod")"#));
        assert!(lib.contains(r#"set_var("AUTUMN_ENV", "dev")"#));
        assert!(
            !lib.contains(r#"remove_var("AUTUMN_ENV")"#),
            "release builds must pin prod explicitly, not unset AUTUMN_ENV"
        );
    }

    #[test]
    fn lib_rs_sets_env_before_tauri_builder_starts() {
        let lib = render_mobile_lib_rs("my-app", "my_app");
        // std::env::set_var is only sound before other (platform) threads
        // exist; the static env block must precede tauri::Builder in run().
        let env_pos = lib
            .find(r#"set_var("AUTUMN_ENV""#)
            .expect("template must set AUTUMN_ENV");
        let builder_pos = lib
            .find("tauri::Builder::default()")
            .expect("template must build a tauri app");
        assert!(
            env_pos < builder_pos,
            "the env-var block must run before tauri::Builder::default()"
        );
        // Storage backend must be enabled explicitly: the prod profile
        // defaults storage.backend to \"disabled\".
        assert!(lib.contains(r#"set_var("AUTUMN_STORAGE__BACKEND", "local")"#));
    }

    #[test]
    fn lib_rs_surfaces_startup_failure_in_a_visible_error_page() {
        let lib = render_mobile_lib_rs("my-app", "my_app");
        assert!(
            lib.contains("fn show_startup_error("),
            "startup failures must surface a visible error, not a silent exit"
        );
        assert!(lib.contains("data:text/html"));
        // The health-poll timeout path routes through the error page.
        assert!(lib.contains("did not become ready"));
    }

    #[test]
    fn mobile_main_rs_calls_shell_lib() {
        let main = render_mobile_main_rs("my-app");
        assert!(main.contains("my_app_mobile::run();"));
    }

    #[test]
    fn lib_extraction_rewrites_stock_main_and_creates_lib() {
        let tmp = app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();

        let lib_action =
            app_src_action(&plan, "lib.rs").expect("plan must create the app src/lib.rs");
        let Action::CreateIfAbsent { contents, .. } = lib_action else {
            panic!("app lib.rs must be a CreateIfAbsent action, got {lib_action:?}");
        };
        assert!(contents.contains("pub async fn serve()"));
        assert!(!contents.contains("async fn main()"));

        let main_action =
            app_src_action(&plan, "main.rs").expect("plan must rewrite the app src/main.rs");
        let Action::Modify { contents, .. } = main_action else {
            panic!("app main.rs must be a Modify action, got {main_action:?}");
        };
        assert!(contents.contains("my_app::serve().await;"));
        assert!(plan.warnings.is_empty(), "no warnings on the happy path");
    }

    #[test]
    fn lib_extraction_skips_customised_main_with_docs_warning() {
        let tmp = app_dir("my-app");
        std::fs::write(
            tmp.path().join("src/main.rs"),
            "#[tokio::main]\nasync fn main() {}\n",
        )
        .unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();

        assert!(
            app_src_action(&plan, "lib.rs").is_none(),
            "no app lib.rs may be planned when the anchor is missing"
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("tauri-mobile-in-process")),
            "warning must point at the docs page, got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn lib_extraction_is_silent_when_already_extracted() {
        let tmp = app_dir("my-app");
        std::fs::write(
            tmp.path().join("src/main.rs"),
            render_thin_app_main_rs("my_app"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub async fn serve() {}\n").unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();
        assert!(plan.warnings.is_empty());
        assert!(app_src_action(&plan, "main.rs").is_none());
    }

    #[test]
    fn lib_extraction_warns_when_foreign_lib_rs_exists() {
        let tmp = app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn helper() {}\n").unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("src/lib.rs already exists")),
            "must warn about the pre-existing lib.rs, got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn plan_emits_no_sidecar_artifacts() {
        let tmp = app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();
        for action in &plan.actions {
            let p = action.path().to_string_lossy().replace('\\', "/");
            assert!(
                !p.contains("stage-sidecar"),
                "mobile plan must not stage sidecars: {p}"
            );
        }
    }

    #[test]
    fn read_app_meta_resolves_workspace_version() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n[workspace.package]\nversion = \"9.9.9\"\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion.workspace = true\n",
        )
        .unwrap();

        let meta = read_app_meta(&app).unwrap();
        assert_eq!(meta.package_name, "app");
        assert_eq!(meta.version, "9.9.9");
        assert!(
            !meta.has_embed_assets,
            "no [features] table means no embed-assets feature"
        );
    }

    #[test]
    fn read_app_meta_defaults_lib_ident_to_underscored_package_name() {
        let tmp = app_dir("my-app");
        let meta = read_app_meta(tmp.path()).unwrap();
        assert_eq!(meta.lib_ident, "my_app");
    }

    #[test]
    fn read_app_meta_honors_custom_lib_name() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [lib]\nname = \"custom_lib\"\n",
        )
        .unwrap();
        let meta = read_app_meta(tmp.path()).unwrap();
        assert_eq!(
            meta.lib_ident, "custom_lib",
            "[lib].name must win over the dash-to-underscore package name"
        );
    }

    #[test]
    fn read_app_meta_honors_custom_lib_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [lib]\npath = \"src/app_lib.rs\"\n",
        )
        .unwrap();
        let meta = read_app_meta(tmp.path()).unwrap();
        assert_eq!(
            meta.lib_src_path, "src/app_lib.rs",
            "[lib].path must win over the default src/lib.rs"
        );
        // Default when [lib].path is absent.
        let default = app_dir("my-app");
        assert_eq!(
            read_app_meta(default.path()).unwrap().lib_src_path,
            "src/lib.rs"
        );
    }

    #[test]
    fn plan_extracts_serve_into_custom_lib_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [lib]\npath = \"src/app_lib.rs\"\n\n\
             [features]\nembed-assets = [\"autumn-web/embed-assets\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();

        // serve() lands in the file Cargo actually compiles as the library.
        let lib_action =
            app_src_action(&plan, "app_lib.rs").expect("plan must create the custom-path lib file");
        let Action::CreateIfAbsent { contents, .. } = lib_action else {
            panic!("custom lib file must be a CreateIfAbsent action, got {lib_action:?}");
        };
        assert!(contents.contains("pub async fn serve()"));
        assert!(
            app_src_action(&plan, "lib.rs").is_none(),
            "no stray src/lib.rs may be planned when [lib].path points elsewhere"
        );
        let main_action =
            app_src_action(&plan, "main.rs").expect("plan must rewrite the app src/main.rs");
        let Action::Modify { contents, .. } = main_action else {
            panic!("app main.rs must be a Modify action, got {main_action:?}");
        };
        assert!(contents.contains("my_app::serve().await;"));
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
    }

    #[test]
    fn plan_warns_when_custom_lib_path_file_already_exists() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [lib]\npath = \"src/app_lib.rs\"\n\n\
             [features]\nembed-assets = [\"autumn-web/embed-assets\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();
        std::fs::write(tmp.path().join("src/app_lib.rs"), "pub fn helper() {}\n").unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("src/app_lib.rs already exists")),
            "the graceful fallback must name the ACTUAL lib target file, got {:?}",
            plan.warnings
        );
        assert!(
            app_src_action(&plan, "main.rs").is_none(),
            "main.rs must be left untouched on the fallback path"
        );
    }

    #[test]
    fn plan_uses_custom_lib_name_for_all_serve_call_sites() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [lib]\nname = \"custom_lib\"\n\n\
             [features]\nembed-assets = [\"autumn-web/embed-assets\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();

        // Shell lib.rs must call custom_lib::serve(), not my_app::serve().
        let shell_lib = plan
            .actions
            .iter()
            .find(|a| {
                a.path()
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with("src-tauri/src/lib.rs")
            })
            .expect("plan must create the shell src/lib.rs");
        let Action::Create { contents, .. } = shell_lib else {
            panic!("shell lib.rs must be a Create action, got {shell_lib:?}");
        };
        assert!(
            contents.contains("block_on(custom_lib::serve())"),
            "shell lib.rs must call the custom [lib] name"
        );
        assert!(
            !contents.contains("my_app::serve()"),
            "shell lib.rs must not call the dash-to-underscore package name"
        );

        // The rewritten app main.rs must call custom_lib::serve() too.
        let main_action =
            app_src_action(&plan, "main.rs").expect("plan must rewrite the app src/main.rs");
        let Action::Modify { contents, .. } = main_action else {
            panic!("app main.rs must be a Modify action, got {main_action:?}");
        };
        assert!(
            contents.contains("custom_lib::serve().await;"),
            "thin main.rs must call the custom [lib] name"
        );
    }

    #[test]
    fn extraction_reruns_silently_with_custom_lib_name() {
        // Re-run detection matches on `<lib_ident>::serve()` — with a custom
        // [lib] name the already-extracted thin main.rs must be recognised.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [lib]\nname = \"custom_lib\"\n\n\
             [features]\nembed-assets = [\"autumn-web/embed-assets\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/main.rs"),
            render_thin_app_main_rs("custom_lib"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub async fn serve() {}\n").unwrap();

        let plan = plan_tauri_mobile(tmp.path()).unwrap();
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
        assert!(app_src_action(&plan, "main.rs").is_none());
    }

    #[test]
    fn guard_refuses_desktop_leftovers() {
        let tmp = app_dir("my-app");
        let tauri = tmp.path().join("src-tauri");
        std::fs::create_dir_all(&tauri).unwrap();
        std::fs::write(tauri.join("stage-sidecar.sh"), "#!/bin/sh\n").unwrap();

        let err = ensure_no_other_mode_scaffold(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("desktop (sidecar)"), "got: {err}");
        assert!(err.contains("src-tauri/stage-sidecar.sh"), "got: {err}");
        assert!(err.contains("autumn destroy tauri"), "got: {err}");
    }

    #[test]
    fn guard_refuses_thin_client_leftovers() {
        let tmp = app_dir("my-app");
        let caps = tmp.path().join("src-tauri").join("capabilities");
        std::fs::create_dir_all(&caps).unwrap();
        std::fs::write(caps.join("remote-app.json"), "{}\n").unwrap();

        let err = ensure_no_other_mode_scaffold(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("thin-client"), "got: {err}");
        assert!(
            err.contains("src-tauri/capabilities/remote-app.json"),
            "got: {err}"
        );
        assert!(
            err.contains("autumn destroy tauri --remote-url"),
            "got: {err}"
        );
    }

    #[test]
    fn guard_allows_clean_tree_and_same_mode_regenerate() {
        // No src-tauri/ at all.
        let clean = app_dir("my-app");
        assert!(ensure_no_other_mode_scaffold(clean.path()).is_ok());

        // An existing MOBILE scaffold (same mode) carries none of the other
        // modes' marker files, so a --force regenerate keeps working.
        let mobile = app_dir("my-app");
        let tauri = mobile.path().join("src-tauri");
        std::fs::create_dir_all(tauri.join("src")).unwrap();
        std::fs::write(
            tauri.join("tauri.conf.json"),
            render_mobile_tauri_conf("my-app", "0.1.0"),
        )
        .unwrap();
        std::fs::write(
            tauri.join("Cargo.toml"),
            render_mobile_cargo_toml("my-app", true),
        )
        .unwrap();
        std::fs::write(
            tauri.join("src").join("lib.rs"),
            render_mobile_lib_rs("my-app", "my_app"),
        )
        .unwrap();
        assert!(
            ensure_no_other_mode_scaffold(mobile.path()).is_ok(),
            "same-mode regenerate must not be blocked by the guard"
        );
    }

    #[test]
    fn prerequisites_mention_mobile_toolchains_and_docs() {
        let prereqs = render_mobile_prerequisites();
        assert!(prereqs.contains("tauri-cli"));
        assert!(prereqs.contains("ios"));
        assert!(prereqs.contains("android"));
        assert!(prereqs.contains("tauri-mobile-in-process"));
    }
}
