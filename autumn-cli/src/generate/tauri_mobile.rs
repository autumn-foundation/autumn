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

/// Options for [`plan_tauri_mobile`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TauriMobileOptions {
    /// Wire local-first offline storage + background sync (issue #1508):
    /// a `SyncStore`-backed `SQLite` database in the app sandbox, a background
    /// `SyncEngine` against `AUTUMN_SYNC__REMOTE_URL`, and the server-side
    /// `/sync` router in the extracted app crate (feature `offline-sync` on
    /// `autumn-web`).
    pub offline_sync: bool,
}

/// Compute the file actions for `autumn generate tauri-mobile`, honouring
/// `opts` (`--offline-sync`).
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when not at a project root, or
/// [`GenerateError::Config`] if `Cargo.toml` is missing `[package].name`.
pub fn plan_tauri_mobile(
    project_root: &Path,
    opts: TauriMobileOptions,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    let (package_name, package_version, has_embed_assets) = read_app_meta(project_root)?;
    let mut plan = Plan::new(project_root);
    let tauri = project_root.join("src-tauri");
    // The shell's own autumn-web dependency (offline sync only) must carry
    // the same version requirement as the app crate's, so cargo unifies the
    // two dependency edges into one crate instance.
    let autumn_web_req = opts
        .offline_sync
        .then(|| read_app_autumn_web_req(project_root));

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
        render_mobile_cargo_toml(&package_name, has_embed_assets, autumn_web_req.as_deref()),
    );
    plan.create(tauri.join("build.rs"), render_mobile_build_rs());
    plan.create(
        tauri.join("src").join("main.rs"),
        render_mobile_main_rs(&package_name),
    );
    plan.create(
        tauri.join("src").join("lib.rs"),
        render_mobile_lib_rs(&package_name, opts.offline_sync),
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

    // App-crate lib extraction so the shell can call `<app>::serve()` in-process.
    plan_lib_extraction(project_root, &package_name, opts.offline_sync, &mut plan);

    // App-crate offline-sync feature (Cargo.toml edit, offline sync only).
    if opts.offline_sync {
        plan_app_offline_sync_feature(project_root, &mut plan);
    }

    Ok(plan)
}

/// Human-readable prerequisites message printed after a successful scaffold,
/// honouring `opts` (`--offline-sync` swaps the remote-connection step).
pub fn render_mobile_prerequisites(opts: TauriMobileOptions) -> String {
    // Step 3 is where the app meets the network: a direct remote-Postgres
    // connection by default, the background sync engine under --offline-sync
    // (where the device needs no database connection at all).
    let connect_step = if opts.offline_sync {
        "3. Point the background sync engine at your remote deployment:\n\
         edit src-tauri/src/lib.rs and set AUTUMN_SYNC__REMOTE_URL to the\n\
         /sync mount of your deployed app (see the commented block in run()).\n\
         Deploy the SAME app with AUTUMN_DATABASE__URL set so it serves the\n\
         /sync endpoints — and mount them behind auth before shipping. The\n\
         device itself needs no database connection; read\n\
         docs/guide/tauri-mobile-offline-sync.md for the offline walkthrough.\n"
    } else {
        "3. Point the app at your remote Postgres database:\n\
         edit src-tauri/src/lib.rs and set AUTUMN_DATABASE__URL (see the\n\
         commented example in run()).\n"
    };
    format!(
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
{connect_step}\
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
    )
}

// ── Package metadata helper ───────────────────────────────────────────────────

/// Returns `(package_name, version, has_embed_assets)` from the app's
/// `Cargo.toml`.
///
/// A deliberately small, self-contained reader: the mobile shell has no
/// sidecar, so — unlike the desktop generator — it needs no binary-target or
/// dependency-key resolution. `version` resolves workspace inheritance
/// (`version.workspace = true`) by walking up the directory tree.
/// `has_embed_assets` is `true` when the app declares an `embed-assets`
/// feature (the stock scaffold always does) — the mobile shell enables it on
/// the app dependency so CSS/JS/static assets are compiled into the binary
/// (a mobile app has no working directory to serve `static/` from).
fn read_app_meta(project_root: &Path) -> Result<(String, String, bool), GenerateError> {
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

    Ok((name, version, has_embed_assets))
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

/// The app crate's `autumn-web` version requirement (`autumn-web = "0.6.0"`,
/// or the `version` key of a table dependency). The offline-sync shell pins
/// its own direct `autumn-web` dependency to the SAME requirement so cargo
/// unifies both dependency edges into one crate instance. Falls back to this
/// CLI's version — the value `autumn new` scaffolds into the app manifest.
fn read_app_autumn_web_req(project_root: &Path) -> String {
    let content = read_or_empty(&project_root.join("Cargo.toml"));
    toml::from_str::<toml::Value>(&content)
        .ok()
        .and_then(|doc| match doc.get("dependencies")?.get("autumn-web")? {
            toml::Value::String(s) => Some(s.clone()),
            toml::Value::Table(t) => t
                .get("version")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

// ── App-crate offline-sync feature (Cargo.toml edit) ──────────────────────────

/// Plan the app-crate `Cargo.toml` edit for `--offline-sync`: declare
/// `offline-sync = ["autumn-web/offline-sync"]` and put it in the `default`
/// feature set, so a plain `cargo run` server deployment of the same app
/// mounts the `/sync` endpoints the device syncs against.
///
/// Anchored + idempotent, following the `plan_lib_extraction` discipline: a
/// manifest that already declares the feature is left untouched (silent
/// `--force` re-runs), and a customised manifest gets a warning with the
/// manual steps instead of a guessed edit. Like the `main.rs` extraction,
/// this is an [`super::emit::Action::Modify`] that `autumn destroy` never
/// reverses — the extracted `serve()` keeps compiling either way (its sync
/// code is gated on the feature).
fn plan_app_offline_sync_feature(project_root: &Path, plan: &mut Plan) {
    const FEATURES_ANCHOR: &str = "[features]\n";
    const DEFAULT_ANCHOR: &str = "default = [\"flash\"]\n";
    const FEATURE_LINE: &str = "offline-sync = [\"autumn-web/offline-sync\"]\n";

    let cargo_path = project_root.join("Cargo.toml");
    let content = read_or_empty(&cargo_path);
    if content.contains("offline-sync = [") {
        return; // Already wired — keep re-runs silent and duplicate-free.
    }
    if !content.contains(FEATURES_ANCHOR) || !content.contains(DEFAULT_ANCHOR) {
        plan.warn(
            "Cargo.toml doesn't match the stock scaffold layout — skipping the \
             automatic offline-sync feature edit. Add \
             `offline-sync = [\"autumn-web/offline-sync\"]` under [features] \
             yourself and include it in `default` (or build with \
             `--features offline-sync`), so the server-side /sync endpoints \
             compile (see docs/guide/tauri-mobile-offline-sync.md)."
                .to_owned(),
        );
        return;
    }
    let edited = content
        .replacen(
            DEFAULT_ANCHOR,
            "default = [\"flash\", \"offline-sync\"]\n",
            1,
        )
        .replacen(
            FEATURES_ANCHOR,
            &format!(
                "[features]\n\
                 # Offline sync (autumn generate tauri-mobile --offline-sync): local\n\
                 # SyncStore storage plus the server-side /sync router in serve(). In\n\
                 # the default set so plain `cargo run` server deployments mount /sync.\n\
                 {FEATURE_LINE}"
            ),
            1,
        );
    plan.modify(cargo_path, edited);
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

/// The stock scaffold's final `AppBuilder::run` call — the anchor after
/// which nothing may run, so the `--offline-sync` mounting is inserted
/// immediately before it (same anchored-edit discipline as
/// [`MAIN_FN_ANCHOR`]).
const RUN_CALL_ANCHOR: &str = "    app\n        .run()\n        .await;";

/// The `--offline-sync` call inserted into `serve()` before
/// [`RUN_CALL_ANCHOR`].
const SYNC_MOUNT_CALL: &str = r#"    // Offline sync (issue #1508): mount the /sync endpoints when a database
    // is configured; on a device (no AUTUMN_DATABASE__URL) this is a no-op —
    // the app boots fully offline as a sync CLIENT (see src-tauri/src/lib.rs).
    #[cfg(feature = "offline-sync")]
    let app = mount_offline_sync(app).await;

"#;

/// The `--offline-sync` server-side mounting helper appended to the
/// extracted `src/lib.rs`.
const SYNC_MOUNT_HELPER: &str = r#"
/// Mount the offline-sync server endpoints (`POST /sync/push`, `GET /sync/pull`).
///
/// Added by `autumn generate tauri-mobile --offline-sync`. The endpoints are
/// mounted only when `AUTUMN_DATABASE__URL` is set: the REMOTE deployment of
/// this app (the one with Postgres) serves `/sync` for every device, while
/// the same code running in-process on a phone has no database and syncs as
/// a client instead (see `src-tauri/src/lib.rs`). Before shipping, mount
/// these endpoints behind authentication — they trust `device_id` as sent
/// (see docs/guide/tauri-mobile-offline-sync.md).
#[cfg(feature = "offline-sync")]
async fn mount_offline_sync(app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
    use std::sync::Arc;

    use autumn_web::reexports::tokio;
    use autumn_web::sync::{LwwResolver, PgSyncBackend, server};

    // Diagnostics below use stderr: this helper runs BEFORE AppBuilder::run()
    // installs the tracing subscriber, so tracing events here would be lost.
    let Ok(database_url) = std::env::var("AUTUMN_DATABASE__URL") else {
        eprintln!(
            "offline-sync: AUTUMN_DATABASE__URL is not set — running as a \
             sync client only; the remote deployment serves /sync"
        );
        return app;
    };
    let backend = Arc::new(PgSyncBackend::new(database_url));
    // Idempotent DDL for the sync shadow tables. A temporarily unreachable
    // database must not prevent the app from starting: log and continue —
    // /sync requests fail until the schema exists (restart once the database
    // is reachable, or run the DDL from a deploy step).
    let schema_backend = Arc::clone(&backend);
    match tokio::task::spawn_blocking(move || schema_backend.ensure_schema()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("offline-sync: could not ensure the sync schema (/sync will fail): {e}");
        }
        Err(e) => eprintln!("offline-sync: sync schema task failed: {e}"),
    }
    app.nest("/sync", server::router(backend, Arc::new(LwwResolver)))
}
"#;

/// Plan the anchored extraction of the app's `src/main.rs` into
/// `src/lib.rs::serve()`, with a graceful skip when the app doesn't match
/// the stock scaffold shape. With `offline_sync`, the extracted `serve()`
/// additionally mounts the server-side sync router (feature-gated, and only
/// when a database is configured — see [`SYNC_MOUNT_HELPER`]).
///
/// Uses `Modify` for `main.rs` (an intentional rewrite, not a collision) and
/// `CreateIfAbsent` for `lib.rs` — so `autumn destroy tauri-mobile` removes
/// the `src-tauri/` shell but leaves the extracted (still fully functional)
/// app lib in place rather than deleting a `src/lib.rs` that `main.rs` now
/// depends on.
fn plan_lib_extraction(
    project_root: &Path,
    package_name: &str,
    offline_sync: bool,
    plan: &mut Plan,
) {
    let crate_ident = package_name.replace('-', "_");
    let main_path = project_root.join("src").join("main.rs");
    let lib_path = project_root.join("src").join("lib.rs");
    let main_rs = read_or_empty(&main_path);

    if lib_path.exists() {
        // Re-run after a successful extraction: nothing to do, stay silent —
        // unless this run wants sync mounting and the existing lib lacks it
        // (e.g. the first run had no --offline-sync).
        if main_rs.contains(&format!("{crate_ident}::serve()")) {
            if offline_sync && !read_or_empty(&lib_path).contains("mount_offline_sync") {
                plan.warn(
                    "src/lib.rs was extracted without --offline-sync — add the \
                     server-side sync mounting yourself: gate a \
                     `mount_offline_sync(app)` call before `.run()` behind \
                     `#[cfg(feature = \"offline-sync\")]` (the full helper is in \
                     docs/guide/tauri-mobile-offline-sync.md)."
                        .to_owned(),
                );
            }
            return;
        }
        plan.warn(format!(
            "src/lib.rs already exists — skipping the automatic extraction of \
             src/main.rs. The Tauri mobile shell calls `{crate_ident}::serve()`; \
             expose your app as `pub async fn serve()` in src/lib.rs by hand \
             (see docs/guide/tauri-mobile-in-process.md)."
        ));
        return;
    }

    if !main_rs.contains(MAIN_FN_ANCHOR) {
        plan.warn(format!(
            "src/main.rs doesn't match the stock scaffold layout — skipping the \
             automatic extraction into src/lib.rs. The Tauri mobile shell calls \
             `{crate_ident}::serve()`; move your `main()` body into a \
             `pub async fn serve()` in src/lib.rs and call it from `main()` \
             (see docs/guide/tauri-mobile-in-process.md for the exact steps)."
        ));
        return;
    }

    let mut lib_rs = main_rs.replace(MAIN_FN_ANCHOR, SERVE_FN_HEADER);
    if offline_sync {
        if lib_rs.contains(RUN_CALL_ANCHOR) {
            lib_rs = lib_rs.replacen(
                RUN_CALL_ANCHOR,
                &format!("{SYNC_MOUNT_CALL}{RUN_CALL_ANCHOR}"),
                1,
            );
            lib_rs.push_str(SYNC_MOUNT_HELPER);
        } else {
            plan.warn(
                "src/main.rs doesn't end in the stock `app.run().await` shape — \
                 skipping the automatic /sync mounting in the extracted \
                 src/lib.rs. Add it yourself before `.run()` (the full helper \
                 is in docs/guide/tauri-mobile-offline-sync.md)."
                    .to_owned(),
            );
        }
    }
    plan.create_if_absent(lib_path, lib_rs);
    plan.modify(main_path, render_thin_app_main_rs(&crate_ident));
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

fn render_mobile_cargo_toml(
    package_name: &str,
    has_embed_assets: bool,
    autumn_web_req: Option<&str>,
) -> String {
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
    // Offline sync: the shell itself opens the local SyncStore and runs the
    // background SyncEngine, so it needs autumn-web directly — pinned to the
    // app's own requirement so cargo unifies both edges into one instance.
    let sync_dep = autumn_web_req.map_or_else(String::new, |req| {
        format!(
            "\n# Offline sync (issue #1508): the shell opens the local SyncStore and runs\n\
             # the background SyncEngine (autumn_web::sync). The version matches the app\n\
             # crate's autumn-web requirement so cargo unifies both dependency edges.\n\
             autumn-web = {{ version = \"{req}\", features = [\"offline-sync\"] }}\n"
        )
    });
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
{sync_dep}
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

/// `--offline-sync` addition to the shell lib.rs module docs.
const SYNC_LIB_DOC: &str = "\
//!
//! OFFLINE SYNC (--offline-sync): app data lives in a local SyncStore-backed
//! SQLite database inside the app sandbox, and a background SyncEngine
//! reconciles it with the remote deployment's /sync endpoints whenever the
//! network allows — the device itself needs NO direct database connection.
//! See docs/guide/tauri-mobile-offline-sync.md.
";

/// `--offline-sync` addition to the env-var block in the shell's `run()`.
const SYNC_ENV_BLOCK: &str = r#"    // ── Offline sync (issue #1508) ──────────────────────────────────────────
    // Local-first data: app state lives in a SyncStore-backed SQLite file in
    // the app sandbox (AUTUMN_SYNC__DB_PATH, exported in setup() once the
    // sandbox path is known) and a background SyncEngine syncs it with your
    // remote Autumn deployment. Point the engine at the remote /sync mount:
    // std::env::set_var(
    //     "AUTUMN_SYNC__REMOTE_URL",
    //     "https://app.example.com/sync",
    // );
    // With offline sync the device needs NO direct database connection: leave
    // AUTUMN_DATABASE__URL unset and the in-process server boots without
    // Postgres — the app works fully offline and converges with the remote in
    // the background. Full guide: docs/guide/tauri-mobile-offline-sync.md.
    // ────────────────────────────────────────────────────────────────────────
"#;

/// `--offline-sync` addition to the shell's `setup()` (the sync database
/// path needs the sandbox data dir).
const SYNC_SETUP_BLOCK: &str = r#"    // Offline sync: local-first app data lives in a SyncStore-backed SQLite
    // file inside the sandbox. AUTUMN_SYNC__DB_PATH is exported so the app's
    // routes open the SAME store the background engine syncs (see
    // start_background_sync below).
    let sync_db = data_root.join("sync.db");
    std::env::set_var("AUTUMN_SYNC__DB_PATH", sync_db.to_string_lossy().as_ref());
"#;

#[allow(clippy::too_many_lines)]
fn render_mobile_lib_rs(package_name: &str, offline_sync: bool) -> String {
    let crate_ident = package_name.replace('-', "_");
    let sync_doc = if offline_sync { SYNC_LIB_DOC } else { "" };
    let sync_env_block = if offline_sync { SYNC_ENV_BLOCK } else { "" };
    let sync_setup = if offline_sync { SYNC_SETUP_BLOCK } else { "" };
    let sync_thread_line = if offline_sync {
        "        start_background_sync(&runtime, sync_db);\n"
    } else {
        ""
    };
    // The offline-sync shell observes run-loop events (RunEvent::Resumed →
    // immediate sync pass), so it must go through build().run(callback); the
    // default shell keeps the simpler run(context).
    let tauri_run_tail = if offline_sync {
        format!(
            r#"    tauri::Builder::default()
        .setup(move |app| setup(app, port))
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {{
            // Connectivity-regain trigger: mobile OSes freeze the process
            // (and its timers) in the background, and connectivity usually
            // returns together with the foreground. On resume, kick one
            // immediate sync pass instead of waiting out the background
            // interval/backoff.
            if let tauri::RunEvent::Resumed = event {{
                if let Some((handle, engine)) = SYNC_KICK.get() {{
                    let engine = engine.clone();
                    handle.spawn(async move {{
                        if let Err(e) = engine.sync_once().await {{
                            eprintln!(
                                "[{package_name}] Resume sync failed (next background pass retries): {{e}}"
                            );
                        }}
                    }});
                }}
            }}
        }});"#
        )
    } else {
        r#"    tauri::Builder::default()
        .setup(move |app| setup(app, port))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");"#
            .to_owned()
    };
    let sync_helpers = if offline_sync {
        format!(
            r#"
/// Handle to the background sync engine, set once from the server thread so
/// the tauri run-loop callback can trigger an immediate sync pass on
/// RunEvent::Resumed. Never set when AUTUMN_SYNC__REMOTE_URL is unset
/// (offline-only mode).
static SYNC_KICK: std::sync::OnceLock<(tokio::runtime::Handle, autumn_web::sync::SyncEngine)> =
    std::sync::OnceLock::new();

/// Open the local sync store and spawn the background sync engine on the
/// server runtime (issue #1508). The app keeps working fully offline: local
/// reads and writes always hit the SyncStore, and when
/// AUTUMN_SYNC__REMOTE_URL is configured the engine pushes/pulls every 30 s
/// (exponential backoff while the remote is unreachable), so the app
/// converges automatically when connectivity returns. See
/// docs/guide/tauri-mobile-offline-sync.md.
fn start_background_sync(runtime: &tokio::runtime::Runtime, sync_db: std::path::PathBuf) {{
    let store = match autumn_web::sync::SyncStore::open(&sync_db) {{
        Ok(store) => store,
        Err(e) => {{
            eprintln!("[{package_name}] Failed to open the offline sync store: {{e}}");
            return;
        }}
    }};
    let Ok(remote_url) = std::env::var("AUTUMN_SYNC__REMOTE_URL") else {{
        eprintln!(
            "[{package_name}] AUTUMN_SYNC__REMOTE_URL is not set — running \
             offline-only (local SyncStore, no background sync)."
        );
        return;
    }};
    let engine =
        autumn_web::sync::SyncEngine::new(store, autumn_web::sync::SyncConfig::new(remote_url));
    // spawn_background must be entered from inside the runtime; the returned
    // JoinHandle detaches on drop (dropping never cancels the task).
    let _sync_task =
        runtime.block_on(async {{ engine.spawn_background(std::time::Duration::from_secs(30)) }});
    let _ = SYNC_KICK.set((runtime.handle().clone(), engine));
}}
"#
        )
    } else {
        String::new()
    };
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
{sync_doc}
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
{sync_env_block}    // The webview loads the app over plain HTTP on loopback; Secure cookies
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

{tauri_run_tail}
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
{sync_setup}    // Per-install signing secret: autumn requires one in prod mode. Generate
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
{sync_thread_line}        runtime.block_on({crate_ident}::serve());
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
{sync_helpers}
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
    /// exact `MAIN_FN_ANCHOR` shape and the template's multi-line
    /// `RUN_CALL_ANCHOR` ending.
    fn stock_main_rs() -> String {
        "use autumn_web::prelude::*;\n\n#[autumn_web::main]\nasync fn main() {\n    \
         let app = autumn_web::app()\n        .routes(routes![]);\n\n    \
         app\n        .run()\n        .await;\n}\n"
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
        let toml_src = render_mobile_cargo_toml("my-app", true, None);
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
        let toml_src = render_mobile_cargo_toml("my-app", false, None);
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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("embed-assets")),
            "must warn when the app declares no embed-assets feature, got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn lib_rs_spawns_in_process_server_with_pool_defaults() {
        let lib = render_mobile_lib_rs("my-app", false);
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
        let lib = render_mobile_lib_rs("my-app", false);
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
        let lib = render_mobile_lib_rs("my-app", false);
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
        let lib = render_mobile_lib_rs("my-app", false);
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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();

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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();

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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();
        assert!(plan.warnings.is_empty());
        assert!(app_src_action(&plan, "main.rs").is_none());
    }

    #[test]
    fn lib_extraction_warns_when_foreign_lib_rs_exists() {
        let tmp = app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn helper() {}\n").unwrap();

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();
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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();
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

        let (name, version, has_embed_assets) = read_app_meta(&app).unwrap();
        assert_eq!(name, "app");
        assert_eq!(version, "9.9.9");
        assert!(
            !has_embed_assets,
            "no [features] table means no embed-assets feature"
        );
    }

    #[test]
    fn prerequisites_mention_mobile_toolchains_and_docs() {
        let prereqs = render_mobile_prerequisites(TauriMobileOptions::default());
        assert!(prereqs.contains("tauri-cli"));
        assert!(prereqs.contains("ios"));
        assert!(prereqs.contains("android"));
        assert!(prereqs.contains("tauri-mobile-in-process"));
        assert!(
            !prereqs.contains("AUTUMN_SYNC__REMOTE_URL"),
            "without --offline-sync the prerequisites must not mention sync"
        );
    }

    // ── --offline-sync (issue #1508) ───────────────────────────────────────

    const OFFLINE: TauriMobileOptions = TauriMobileOptions { offline_sync: true };

    /// An app dir whose Cargo.toml matches the stock scaffold's dependency
    /// and feature shape (what `plan_app_offline_sync_feature` anchors on).
    fn stock_app_dir(name: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.2.3\"\n\n\
                 [dependencies]\nautumn-web = \"0.9.1\"\n\n\
                 [features]\ndefault = [\"flash\"]\nflash = [\"autumn-web/flash\"]\n\
                 embed-assets = [\"autumn-web/embed-assets\"]\n"
            ),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        tmp
    }

    #[test]
    fn offline_cargo_toml_adds_matching_autumn_web_dependency() {
        let toml_src = render_mobile_cargo_toml("my-app", true, Some("0.9.1"));
        assert!(
            toml_src.contains(r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#),
            "the shell must depend on autumn-web (offline-sync) at the app's requirement"
        );
        toml::from_str::<toml::Value>(&toml_src).expect("generated Cargo.toml must parse");
        // Without offline sync the dependency (and any sync trace) is absent.
        let plain = render_mobile_cargo_toml("my-app", true, None);
        assert!(!plain.contains("offline-sync"));
        assert!(!plain.contains("autumn-web ="));
    }

    #[test]
    fn offline_lib_rs_wires_store_engine_and_resume_trigger() {
        let lib = render_mobile_lib_rs("my-app", true);
        // Local store in the sandbox, exported for the app's routes.
        assert!(lib.contains(r#"let sync_db = data_root.join("sync.db");"#));
        assert!(lib.contains(r#""AUTUMN_SYNC__DB_PATH""#));
        assert!(lib.contains("autumn_web::sync::SyncStore::open(&sync_db)"));
        // Engine on the server runtime, configured from the environment.
        assert!(lib.contains(r#"std::env::var("AUTUMN_SYNC__REMOTE_URL")"#));
        assert!(lib.contains("autumn_web::sync::SyncConfig::new(remote_url)"));
        assert!(lib.contains(".spawn_background(std::time::Duration::from_secs(30))"));
        assert!(lib.contains("start_background_sync(&runtime, sync_db);"));
        // Connectivity-regain trigger through the run-loop callback.
        assert!(lib.contains(".build(tauri::generate_context!())"));
        assert!(lib.contains("tauri::RunEvent::Resumed"));
        assert!(lib.contains("engine.sync_once().await"));
        assert!(lib.contains("docs/guide/tauri-mobile-offline-sync.md"));
        // Offline startup: the direct database URL stays a commented example.
        assert!(!lib.contains("\n    std::env::set_var(\"AUTUMN_DATABASE__URL\""));
    }

    #[test]
    fn lib_rs_without_offline_flag_has_no_sync_wiring() {
        let lib = render_mobile_lib_rs("my-app", false);
        assert!(!lib.contains("AUTUMN_SYNC"));
        assert!(!lib.contains("sync_once"));
        assert!(!lib.contains("SYNC_KICK"));
        assert!(
            lib.contains(".run(tauri::generate_context!())"),
            "the default shell must keep the simple run(context) tail"
        );
    }

    #[test]
    fn offline_plan_edits_app_cargo_toml_features() {
        let tmp = stock_app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path(), OFFLINE).unwrap();
        let cargo_action = plan
            .actions
            .iter()
            .find(|a| {
                a.path().ends_with("Cargo.toml")
                    && !a.path().to_string_lossy().contains("src-tauri")
            })
            .expect("plan must edit the app Cargo.toml");
        let Action::Modify { contents, .. } = cargo_action else {
            panic!("app Cargo.toml must be a Modify action, got {cargo_action:?}");
        };
        assert!(contents.contains(r#"offline-sync = ["autumn-web/offline-sync"]"#));
        assert!(contents.contains(r#"default = ["flash", "offline-sync"]"#));
        toml::from_str::<toml::Value>(contents).expect("edited Cargo.toml must stay valid TOML");
    }

    #[test]
    fn offline_plan_skips_cargo_toml_edit_when_already_wired() {
        let tmp = stock_app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();
        let cargo_path = tmp.path().join("Cargo.toml");
        let wired = read_or_empty(&cargo_path).replace(
            "default = [\"flash\"]",
            "default = [\"flash\", \"offline-sync\"]",
        ) + "offline-sync = [\"autumn-web/offline-sync\"]\n";
        std::fs::write(&cargo_path, wired).unwrap();

        let plan = plan_tauri_mobile(tmp.path(), OFFLINE).unwrap();
        assert!(
            !plan.actions.iter().any(|a| a.path().ends_with("Cargo.toml")
                && !a.path().to_string_lossy().contains("src-tauri")),
            "an already-wired Cargo.toml must not be edited again"
        );
        assert!(plan.warnings.is_empty(), "re-runs must stay silent");
    }

    #[test]
    fn offline_plan_warns_on_customised_cargo_toml_features() {
        // No `default = ["flash"]` anchor — the feature edit must be skipped
        // with a warning, never guessed.
        let tmp = app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path(), OFFLINE).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("offline-sync") && w.contains("[features]")),
            "must warn with manual steps on a customised Cargo.toml, got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn offline_extraction_mounts_sync_router_in_serve() {
        let tmp = stock_app_dir("my-app");
        std::fs::write(tmp.path().join("src/main.rs"), stock_main_rs()).unwrap();

        let plan = plan_tauri_mobile(tmp.path(), OFFLINE).unwrap();
        let lib_action =
            app_src_action(&plan, "lib.rs").expect("plan must create the app src/lib.rs");
        let Action::CreateIfAbsent { contents, .. } = lib_action else {
            panic!("app lib.rs must be a CreateIfAbsent action, got {lib_action:?}");
        };
        assert!(contents.contains("let app = mount_offline_sync(app).await;"));
        assert!(contents.contains("async fn mount_offline_sync"));
        assert!(contents.contains("PgSyncBackend::new(database_url)"));
        assert!(contents.contains("ensure_schema()"));
        assert!(
            contents.contains(r#".nest("/sync", server::router(backend, Arc::new(LwwResolver)))"#)
        );
        // The mount call must precede the final run().
        let mount = contents.find("mount_offline_sync(app)").unwrap();
        let run = contents.find(".run()").unwrap();
        assert!(mount < run, "sync mounting must happen before .run()");
    }

    #[test]
    fn offline_prerequisites_swap_in_the_sync_step() {
        let prereqs = render_mobile_prerequisites(OFFLINE);
        assert!(prereqs.contains("AUTUMN_SYNC__REMOTE_URL"));
        assert!(prereqs.contains("tauri-mobile-offline-sync"));
        assert!(
            !prereqs.contains("set AUTUMN_DATABASE__URL (see the"),
            "the direct-Postgres step must be replaced, not duplicated"
        );
    }

    #[test]
    fn read_app_autumn_web_req_reads_string_and_table_deps() {
        let tmp = stock_app_dir("my-app");
        assert_eq!(read_app_autumn_web_req(tmp.path()), "0.9.1");

        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
             autumn-web = { version = \"1.2.3\", features = [\"mail\"] }\n",
        )
        .unwrap();
        assert_eq!(read_app_autumn_web_req(tmp.path()), "1.2.3");

        // No autumn-web dependency at all → fall back to the CLI's version.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(
            read_app_autumn_web_req(tmp.path()),
            env!("CARGO_PKG_VERSION")
        );
    }
}
