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
//! # `--offline-sync` (issue #1508, Option C)
//!
//! With [`TauriMobileOptions::offline_sync`] the scaffold becomes
//! local-first: app data lives in a `SyncStore`-backed `SQLite` database in
//! the app sandbox (the shell exports its path as `AUTUMN_SYNC__DB_PATH`),
//! a background `SyncEngine` syncs it with the remote deployment's `/sync`
//! endpoints (`AUTUMN_SYNC__REMOTE_URL`; plus an immediate pass on
//! `RunEvent::Resumed`), and the app crate gains a default `offline-sync`
//! feature and a `/sync` router mounted in the extracted `serve()` — only
//! when the app's resolved config has a database URL (e.g.
//! `AUTUMN_DATABASE__URL`), so the same binary boots fully offline on a
//! device and serves sync on the server. Without the flag the emitted
//! scaffold is byte-identical to the plain #1507 output.
//! See `docs/guide/tauri-mobile-offline-sync.md`.
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

    let AppMeta {
        package_name,
        version: package_version,
        lib_ident,
        lib_src_path,
        has_embed_assets,
    } = read_app_meta(project_root)?;
    let mut plan = Plan::new(project_root);
    let tauri = project_root.join("src-tauri");
    // The shell's own autumn-web dependency (offline sync only) must mirror
    // the app crate's actual dependency SOURCE — version, path, or git — so
    // cargo unifies the two dependency edges into one crate instance (see
    // [`shell_autumn_web_dep`]).
    let autumn_web_dep = opts
        .offline_sync
        .then(|| shell_autumn_web_dep(project_root, &mut plan));

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
        render_mobile_cargo_toml(&package_name, has_embed_assets, autumn_web_dep.as_ref()),
    );
    plan.create(tauri.join("build.rs"), render_mobile_build_rs());
    plan.create(
        tauri.join("src").join("main.rs"),
        render_mobile_main_rs(&package_name),
    );
    plan.create(
        tauri.join("src").join("lib.rs"),
        render_mobile_lib_rs(&package_name, &lib_ident, opts.offline_sync),
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
    plan_lib_extraction(
        project_root,
        &lib_ident,
        &lib_src_path,
        opts.offline_sync,
        &mut plan,
    );

    // App-crate offline-sync feature (Cargo.toml edit, offline sync only).
    if opts.offline_sync {
        plan_app_offline_sync_feature(project_root, &mut plan);
    }

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

/// The shell's own `autumn-web` dependency (offline sync only), mirroring
/// the app crate's **actual** dependency source.
///
/// The shell manifest declares its own `[workspace]`, so it inherits neither
/// the app's `[patch.crates-io]` overrides nor a parent workspace's
/// dependency table. A registry-only `autumn-web = { version = … }` edge
/// would therefore compile the shell against a DIFFERENT framework source
/// than the app for path/git/workspace-dep users — or fail outright while
/// the `offline-sync` feature is not published on crates.io. This struct
/// carries the mirrored edge (and, for patched registry deps, a mirrored
/// `[patch.crates-io]` section).
struct ShellAutumnWebDep {
    /// The full `autumn-web = { … }` line for the shell's `[dependencies]`.
    dep_entry: String,
    /// A mirrored `[patch.crates-io]` section (trailing part of the
    /// manifest), when the app's manifest — or an ancestor workspace root —
    /// patches `autumn-web` with a path/git override.
    patch_entry: Option<String>,
}

/// Lexically compute `target` relative to `dir` — no filesystem access, so
/// neither path needs to exist. `.`/`..` components are folded first;
/// returns `None` when the paths cannot be lexically related (different
/// roots/prefixes, or a `..` chain that escapes them).
fn lexical_relative(target: &Path, dir: &Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    fn normalize(p: &Path) -> Option<Vec<Component<'_>>> {
        let mut out: Vec<Component<'_>> = Vec::new();
        for component in p.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    // `..` above the root (or of a bare relative path)
                    // cannot be folded lexically.
                    _ => return None,
                },
                other => out.push(other),
            }
        }
        Some(out)
    }
    let target = normalize(target)?;
    let dir = normalize(dir)?;
    // Roots/prefixes must match for a relative walk to exist at all.
    if target.first() != dir.first() {
        return None;
    }
    let common = target
        .iter()
        .zip(dir.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut relative = std::path::PathBuf::new();
    for _ in common..dir.len() {
        relative.push("..");
    }
    for component in &target[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Some(relative)
}

/// Render the `path = "…"` or `git = "…"[, rev/branch/tag = "…"]` source part
/// of a dependency (or patch) table, adjusted so it stays valid when written
/// into `src-tauri/Cargo.toml`:
///
/// - absolute paths the USER wrote are kept as-is,
/// - paths relative to the app's own manifest gain a `../` prefix
///   (`src-tauri/` sits one level below the project root),
/// - paths declared in an ancestor manifest (workspace root) are resolved
///   against that manifest's directory and then re-relativized against the
///   generated `src-tauri/` — `src-tauri/Cargo.toml` is a CHECKED-IN file,
///   so baking the generating machine's absolute path into it would break
///   every other checkout (teammates, CI); the absolute form is only a
///   fallback when no lexical relative path exists.
///
/// Emitted paths always use forward slashes (valid on Windows too, and the
/// generator's convention). Returns `None` when the table names neither a
/// path nor a git source.
fn dep_source_from_table(
    table: &toml::value::Table,
    manifest_dir: &Path,
    project_root: &Path,
) -> Option<String> {
    if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
        let adjusted = if Path::new(path).is_absolute() {
            path.replace('\\', "/")
        } else if manifest_dir == project_root {
            format!("../{path}")
        } else {
            let target = manifest_dir.join(path);
            let src_tauri = project_root.join("src-tauri");
            lexical_relative(&target, &src_tauri)
                .unwrap_or(target)
                .display()
                .to_string()
                .replace('\\', "/")
        };
        return Some(format!("path = \"{adjusted}\""));
    }
    if let Some(git) = table.get("git").and_then(toml::Value::as_str) {
        use std::fmt::Write as _;
        let mut source = format!("git = \"{git}\"");
        for key in ["rev", "branch", "tag"] {
            if let Some(v) = table.get(key).and_then(toml::Value::as_str) {
                let _ = write!(source, ", {key} = \"{v}\"");
            }
        }
        return Some(source);
    }
    None
}

/// A dependency table's explicit `default-features` setting, honoring the
/// pre-2021 `default_features` spelling cargo still accepts. `None` when the
/// table doesn't set it.
fn table_default_features(table: &toml::value::Table) -> Option<bool> {
    ["default-features", "default_features"]
        .iter()
        .find_map(|key| table.get(*key).and_then(toml::Value::as_bool))
}

/// Resolve the app's `autumn-web` dependency VALUE, following
/// `workspace = true` inheritance to the `[workspace.dependencies]` table of
/// the nearest ancestor manifest that declares it. Returns the value, the
/// directory of the manifest that declared it (relative `path` sources
/// resolve against that directory), and — for workspace-inherited deps — the
/// MEMBER-level `default-features` setting, which the resolved workspace
/// table would otherwise erase (Cargo lets the member re-enable defaults a
/// workspace entry disabled, so the member-level value must survive
/// resolution).
fn resolve_app_autumn_web_value(
    project_root: &Path,
) -> (Option<toml::Value>, std::path::PathBuf, Option<bool>) {
    let parse = |dir: &Path| -> Option<toml::Value> {
        toml::from_str(&read_or_empty(&dir.join("Cargo.toml"))).ok()
    };
    let doc = parse(project_root);
    // Resolve by PACKAGE, not by key: apps may rename the dependency
    // (`autumn = { package = "autumn-web", ... }`), and a hard-coded
    // `autumn-web` lookup would miss it (same convention as the desktop
    // generator's `resolve_dep_key`).
    let dep_key = doc.as_ref().map_or_else(
        || "autumn-web".to_owned(),
        |d| super::tauri::resolve_dep_key(project_root, d, "autumn-web"),
    );
    let dep = doc
        .as_ref()
        .and_then(|d| d.get("dependencies")?.get(&dep_key))
        .cloned();
    if let Some(toml::Value::Table(t)) = &dep
        && t.get("workspace").and_then(toml::Value::as_bool) == Some(true)
    {
        // Capture the member-level default-features BEFORE the member table
        // is replaced by the workspace entry.
        let member_default_features = table_default_features(t);
        // Workspace-inherited: find the same dependency KEY in this
        // manifest's or the nearest ancestor's [workspace.dependencies].
        let mut dir: Option<&Path> = Some(project_root);
        while let Some(d) = dir {
            if let Some(v) = parse(d)
                .as_ref()
                .and_then(|doc| doc.get("workspace")?.get("dependencies")?.get(&dep_key))
                .cloned()
            {
                return (Some(v), d.to_path_buf(), member_default_features);
            }
            dir = d.parent();
        }
        return (None, project_root.to_path_buf(), member_default_features);
    }
    (dep, project_root.to_path_buf(), None)
}

/// Find a `[patch.crates-io]` override of the `autumn-web` PACKAGE — the
/// literal `autumn-web` key or a renamed entry
/// (`autumn_web_local = { package = "autumn-web", … }`) — in the app's
/// manifest or the nearest ancestor's (patches only apply from a workspace
/// root, but the app may be a workspace member). Returns the mirrored
/// `[patch.crates-io]` section for the shell manifest (entry key and
/// `package` rename preserved), warning when a patch exists but cannot be
/// represented.
fn mirror_autumn_web_patch(project_root: &Path, plan: &mut Plan) -> Option<String> {
    // Cargo only honors the [patch] tables of the EFFECTIVE workspace root
    // (member-local [patch] tables are ignored, with a cargo warning), so
    // mirror exclusively from that manifest: copying a member-local patch
    // would make the shell compile a framework checkout the app itself
    // never uses.
    let root = effective_workspace_root(project_root);

    // A member-local framework patch is skipped — tell the user why, since
    // "my patch wasn't mirrored" is otherwise mystifying.
    if root != project_root && autumn_web_patch_entry(project_root).is_some() {
        plan.warn(format!(
            "Cargo ignores [patch] tables outside the workspace root, so the \
             [patch.crates-io] override of autumn-web in the app's own \
             Cargo.toml was NOT mirrored into src-tauri/Cargo.toml (only the \
             patch table of {} applies). Move the patch to the workspace \
             root if the app should build against it.",
            root.join("Cargo.toml").display(),
        ));
    }

    let (key, value) = autumn_web_patch_entry(&root)?;
    let table = value.as_table();
    let source = table.and_then(|t| dep_source_from_table(t, &root, project_root));
    let Some(source) = source else {
        plan.warn(
            "the app's [patch.crates-io] override of autumn-web could not \
             be mirrored into src-tauri/Cargo.toml (only path and git \
             patches are supported). The shell declares its own \
             [workspace], so the app's patch does NOT apply there — copy \
             your [patch.crates-io] section into src-tauri/Cargo.toml \
             yourself or the shell will build against the crates.io \
             registry version of autumn-web."
                .to_owned(),
        );
        return None;
    };
    // Mirror the whole entry: original key plus its `package` rename, so
    // the emitted section patches exactly what the root's does.
    let package_part = table
        .and_then(|t| t.get("package"))
        .and_then(toml::Value::as_str)
        .map_or_else(String::new, |package| format!("package = \"{package}\", "));
    Some(format!(
        "\n[patch.crates-io]\n\
         # Mirrors the app's [patch.crates-io] override of autumn-web: the shell\n\
         # declares its own [workspace], so the app's patch would otherwise NOT\n\
         # apply here and the shell would compile a different framework source.\n\
         {key} = {{ {package_part}{source} }}\n"
    ))
}

/// The `[patch.crates-io]` entry of `dir`'s manifest that patches the
/// `autumn-web` PACKAGE — matched by effective package, not by key: a
/// renamed patch (`autumn_web_local = { package = "autumn-web", … }`) is
/// just as valid as the literal `autumn-web` key, and skipping it would
/// leave the shell on the crates.io registry while the app builds the
/// patched source (same resolve-by-package convention as the dependency
/// lookup).
fn autumn_web_patch_entry(dir: &Path) -> Option<(String, toml::Value)> {
    toml::from_str::<toml::Value>(&read_or_empty(&dir.join("Cargo.toml")))
        .ok()
        .as_ref()
        .and_then(|doc| doc.get("patch")?.get("crates-io")?.as_table())
        .and_then(|table| {
            table.iter().find(|(key, value)| {
                key.as_str() == "autumn-web"
                    || value
                        .as_table()
                        .and_then(|t| t.get("package"))
                        .and_then(toml::Value::as_str)
                        == Some("autumn-web")
            })
        })
        .map(|(key, value)| (key.clone(), value.clone()))
}

/// The manifest directory whose `[patch]` tables Cargo actually honors for
/// the app at `project_root`:
///
/// - the app itself when its manifest declares `[workspace]` (own root) or
///   no enclosing workspace exists (standalone),
/// - the target of an explicit `package.workspace = "…"` pointer,
/// - otherwise the nearest ancestor manifest declaring `[workspace]` —
///   unless that ancestor's `workspace.exclude` names the app's directory
///   (exact relative path; glob patterns are not expanded — the cheap
///   rule), in which case the app is standalone.
///
/// Full `members` glob verification is deliberately skipped: a nearest
/// ancestor root that does not include the member is a broken workspace
/// Cargo itself rejects.
fn effective_workspace_root(project_root: &Path) -> std::path::PathBuf {
    let parse = |dir: &Path| -> Option<toml::Value> {
        toml::from_str(&read_or_empty(&dir.join("Cargo.toml"))).ok()
    };
    let doc = parse(project_root);
    if doc.as_ref().is_some_and(|d| d.get("workspace").is_some()) {
        return project_root.to_path_buf();
    }
    if let Some(pointer) = doc
        .as_ref()
        .and_then(|d| d.get("package")?.get("workspace"))
        .and_then(toml::Value::as_str)
    {
        return project_root.join(pointer);
    }
    let mut dir = project_root.parent();
    while let Some(d) = dir {
        if let Some(workspace) = parse(d)
            .as_ref()
            .and_then(|doc| doc.get("workspace").cloned())
        {
            let excluded = workspace
                .get("exclude")
                .and_then(toml::Value::as_array)
                .is_some_and(|entries| {
                    entries.iter().filter_map(toml::Value::as_str).any(|entry| {
                        lexical_relative(project_root, d)
                            .is_some_and(|relative| relative == Path::new(entry))
                    })
                });
            return if excluded {
                // Excluded members are standalone: their own manifest's
                // patch table is the effective one.
                project_root.to_path_buf()
            } else {
                d.to_path_buf()
            };
        }
        dir = d.parent();
    }
    project_root.to_path_buf()
}

/// Compute the shell's `autumn-web` dependency (offline sync only), mirroring
/// the app crate's actual dependency source — see [`ShellAutumnWebDep`]:
///
/// - **path/git deps** (direct or workspace-inherited) are copied onto the
///   shell edge, with relative paths recomputed for `src-tauri/`,
/// - **registry (version) deps** keep the app's version requirement, plus a
///   mirrored `[patch.crates-io]` section when the app patches `autumn-web`,
/// - anything unrepresentable falls back to this CLI's version (the value
///   `autumn new` scaffolds) with a loud warning telling the user to edit
///   the shell manifest by hand.
fn shell_autumn_web_dep(project_root: &Path, plan: &mut Plan) -> ShellAutumnWebDep {
    let (dep_value, manifest_dir, member_default_features) =
        resolve_app_autumn_web_value(project_root);

    // `default-features = false` must survive the mirroring: cargo unifies
    // features per dependency EDGE, so a shell edge without it would
    // re-enable the framework's default features across the whole src-tauri
    // build even though the app opted out. For workspace-inherited deps the
    // MEMBER-level setting wins when present — that mirrors Cargo's
    // re-enable rule (member `default-features = true` over a workspace
    // `false`); for member `false` over workspace defaults-on Cargo today
    // IGNORES the member key with a warning slated to become a hard error,
    // and we honor the written opt-out instead — harmless either way, since
    // the app's own edge still carries the workspace value and cargo
    // unifies the edges by union. Otherwise the resolved (workspace or
    // direct) table's setting applies.
    let resolved_default_features = match &dep_value {
        Some(toml::Value::Table(t)) => table_default_features(t),
        _ => None,
    };
    let default_features_part =
        if member_default_features.or(resolved_default_features) == Some(false) {
            "default-features = false, "
        } else {
            ""
        };

    // Path/git sources are mirrored directly onto the shell's edge.
    if let Some(toml::Value::Table(t)) = &dep_value
        && let Some(source) = dep_source_from_table(t, &manifest_dir, project_root)
    {
        let version_part = t
            .get("version")
            .and_then(toml::Value::as_str)
            .map_or_else(String::new, |v| format!("version = \"{v}\", "));
        return ShellAutumnWebDep {
            dep_entry: format!(
                "autumn-web = {{ {version_part}{source}, \
                 {default_features_part}features = [\"offline-sync\"] }}"
            ),
            patch_entry: None,
        };
    }

    // Registry requirement: keep the app's version and mirror any
    // [patch.crates-io] override of autumn-web into the shell manifest.
    let version = match &dep_value {
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(toml::Value::Table(t)) => t
            .get("version")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
        _ => None,
    };
    // An alternate-registry selection must survive the mirroring too: a
    // bare version requirement would make the shell workspace resolve
    // autumn-web from crates.io — failing outright for private registries,
    // or silently compiling a different framework. `registry` only combines
    // with version deps (Cargo rejects it on path/git sources, and member
    // manifests cannot override it on workspace-inherited entries), so
    // reading it from the RESOLVED table covers both direct and
    // workspace-inherited shapes.
    let registry_part = match &dep_value {
        Some(toml::Value::Table(t)) => t
            .get("registry")
            .and_then(toml::Value::as_str)
            .map_or_else(String::new, |registry| {
                format!("registry = \"{registry}\", ")
            }),
        _ => String::new(),
    };
    let registry_entry = |req: &str| {
        format!(
            "autumn-web = {{ version = \"{req}\", \
             {registry_part}{default_features_part}features = [\"offline-sync\"] }}"
        )
    };
    let Some(version) = version else {
        plan.warn(format!(
            "could not determine the app's autumn-web dependency source — the \
             shell's src-tauri/Cargo.toml falls back to the crates.io registry \
             (autumn-web = \"{}\"), which may be a DIFFERENT framework source \
             than the app builds against (and may lack the offline-sync \
             feature). Edit the autumn-web entry in src-tauri/Cargo.toml to \
             match your app's source — path, git, or version (see \
             docs/guide/tauri-mobile-offline-sync.md).",
            env!("CARGO_PKG_VERSION"),
        ));
        return ShellAutumnWebDep {
            dep_entry: registry_entry(env!("CARGO_PKG_VERSION")),
            patch_entry: None,
        };
    };
    let patch_entry = mirror_autumn_web_patch(project_root, plan);
    ShellAutumnWebDep {
        dep_entry: registry_entry(&version),
        patch_entry,
    }
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
/// Whether the manifest's `default` feature set enables `offline-sync`,
/// directly or transitively through other features of THIS crate (entries
/// containing `/` or `:` are dependency features — they cannot turn on the
/// crate's own `#[cfg(feature = "offline-sync")]` and are ignored).
fn offline_sync_enabled_by_default(manifest: &str) -> bool {
    fn enables(
        features: &toml::value::Table,
        name: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if name == "offline-sync" {
            return true;
        }
        if !visited.insert(name.to_owned()) {
            return false;
        }
        features
            .get(name)
            .and_then(toml::Value::as_array)
            .is_some_and(|entries| {
                entries.iter().filter_map(toml::Value::as_str).any(|entry| {
                    !entry.contains('/')
                        && !entry.contains(':')
                        && enables(features, entry, visited)
                })
            })
    }
    let Ok(doc) = toml::from_str::<toml::Value>(manifest) else {
        return false;
    };
    let Some(features) = doc.get("features").and_then(toml::Value::as_table) else {
        return false;
    };
    if !features.contains_key("offline-sync") {
        return false;
    }
    features
        .get("default")
        .and_then(toml::Value::as_array)
        .is_some_and(|default| {
            let mut visited = std::collections::HashSet::new();
            default.iter().filter_map(toml::Value::as_str).any(|entry| {
                !entry.contains('/')
                    && !entry.contains(':')
                    && enables(features, entry, &mut visited)
            })
        })
}

fn plan_app_offline_sync_feature(project_root: &Path, plan: &mut Plan) {
    const FEATURES_ANCHOR: &str = "[features]\n";
    const DEFAULT_ANCHOR: &str = "default = [\"flash\"]\n";

    let cargo_path = project_root.join("Cargo.toml");
    let content = read_or_empty(&cargo_path);
    let doc = toml::from_str::<toml::Value>(&content).ok();
    // Cargo feature syntax names the DEPENDENCY KEY, not the package: a
    // renamed dep (`autumn = { package = "autumn-web", ... }`) must be
    // referenced as `autumn/offline-sync` — `autumn-web/offline-sync` would
    // be rejected by cargo as an unknown dependency.
    let dep_key = doc.as_ref().map_or_else(
        || "autumn-web".to_owned(),
        |doc| super::tauri::resolve_dep_key(project_root, doc, "autumn-web"),
    );
    let feature_line = format!("offline-sync = [\"{dep_key}/offline-sync\"]\n");
    // Detect an existing feature via the PARSED [features] table (the same
    // source of truth `offline_sync_enabled_by_default` reads), never a
    // substring scan: `offline-sync=[]` (no spaces) or a multiline array
    // would slip past a textual `"offline-sync = ["` check into the
    // fresh-insert branch below, which would then write a DUPLICATE
    // `offline-sync` key under [features] — a manifest cargo rejects.
    let feature_defined = doc
        .as_ref()
        .and_then(|doc| doc.get("features"))
        .and_then(toml::Value::as_table)
        .is_some_and(|features| features.contains_key("offline-sync"));
    if feature_defined {
        // The feature exists — but "wired" also requires it to be ENABLED
        // by `default`: the extracted serve() mounts /sync behind
        // #[cfg(feature = "offline-sync")], and the documented flow assumes
        // a plain `cargo run` deployment serves those endpoints. A defined
        // but non-default feature would ship a mobile client whose server
        // never mounts /sync.
        if offline_sync_enabled_by_default(&content) {
            return; // Already wired — keep re-runs silent and duplicate-free.
        }
        // Same anchored discipline as the fresh edit: add the feature to
        // the stock `default` line when it still matches (everything else
        // stays byte-identical), otherwise warn instead of guessing at a
        // customised manifest.
        if content.contains(DEFAULT_ANCHOR) {
            plan.modify(
                cargo_path,
                content.replacen(
                    DEFAULT_ANCHOR,
                    "default = [\"flash\", \"offline-sync\"]\n",
                    1,
                ),
            );
        } else {
            plan.warn(
                "Cargo.toml declares an `offline-sync` feature, but `default` \
                 doesn't enable it and the default line doesn't match the \
                 stock scaffold — add `offline-sync` to the `default` feature \
                 set yourself (or run the server deployment with `--features \
                 offline-sync`), or the deployed app will never mount the \
                 /sync endpoints the device syncs against \
                 (see docs/guide/tauri-mobile-offline-sync.md)."
                    .to_owned(),
            );
        }
        return;
    }
    if !content.contains(FEATURES_ANCHOR) || !content.contains(DEFAULT_ANCHOR) {
        plan.warn(format!(
            "Cargo.toml doesn't match the stock scaffold layout — skipping the \
             automatic offline-sync feature edit. Add \
             `offline-sync = [\"{dep_key}/offline-sync\"]` under [features] \
             yourself and include it in `default` (or build with \
             `--features offline-sync`), so the server-side /sync endpoints \
             compile (see docs/guide/tauri-mobile-offline-sync.md)."
        ));
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
                 {feature_line}"
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
    // is configured; on a device (no database configured) this is a no-op —
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
/// mounted only when the app's resolved configuration has a database URL:
/// the REMOTE deployment of this app (the one with Postgres) serves `/sync`
/// for every device, while the same code running in-process on a phone has
/// no database and syncs as a client instead (see `src-tauri/src/lib.rs`).
///
/// REQUIRED before shipping: put these endpoints behind authentication and
/// serve them over HTTPS only. They trust `device_id` as sent, and anyone
/// who can reach them can read and write every synced row — see the
/// middleware example in docs/guide/tauri-mobile-offline-sync.md.
///
/// MULTI-USER apps additionally need per-user data partitioning: the
/// `server::router` call below is SINGLE-TENANT — every authenticated
/// device reads and writes one shared "global" scope. Swap it for
/// `server::scoped_router` and have your auth middleware insert an
/// `autumn_web::sync::SyncScope` derived from the authenticated user (the
/// scope is derived server-side and never client-supplied) — see the
/// "Scope data per user" section of the guide.
#[cfg(feature = "offline-sync")]
async fn mount_offline_sync(app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
    use std::sync::Arc;

    use autumn_web::reexports::tokio;
    use autumn_web::sync::{LwwResolver, PgSyncBackend, server};

    // Diagnostics below use stderr: this helper runs BEFORE AppBuilder::run()
    // installs the tracing subscriber, so tracing events here would be lost.
    //
    // The database URL is resolved through the SAME layered configuration
    // the app itself boots with (autumn.toml, profile files, and the
    // AUTUMN_DATABASE__URL / AUTUMN_DATABASE__PRIMARY_URL env overrides) —
    // not from one raw env var. Caveat: a custom loader installed via
    // `with_config_loader` is NOT consulted here; deployments that must
    // serve /sync need their database URL visible to AutumnConfig::load().
    let database_url = match autumn_web::config::AutumnConfig::load() {
        Ok(config) => config.database.effective_primary_url().map(str::to_owned),
        Err(e) => {
            eprintln!("offline-sync: config load failed ({e}); /sync not mounted");
            return app;
        }
    };
    let Some(database_url) = database_url else {
        eprintln!(
            "offline-sync: no database is configured — running as a \
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

/// Plan the anchored extraction of the app's `src/main.rs` into the app's
/// **library target file** (`serve()`), with a graceful skip when the app
/// doesn't match the stock scaffold shape. With `offline_sync`, the
/// extracted `serve()` additionally mounts the server-side sync router
/// (feature-gated, and only when a database is configured — see
/// [`SYNC_MOUNT_HELPER`]).
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
    offline_sync: bool,
    plan: &mut Plan,
) {
    let main_path = project_root.join("src").join("main.rs");
    let lib_path = project_root.join(lib_src_path);
    let main_rs = read_or_empty(&main_path);

    if lib_path.exists() {
        // Re-run after a successful extraction: nothing to do, stay silent —
        // unless this run wants sync mounting and the existing lib lacks it
        // (e.g. the first run had no --offline-sync).
        if main_rs.contains(&format!("{crate_ident}::serve()")) {
            if offline_sync && !read_or_empty(&lib_path).contains("mount_offline_sync") {
                plan.warn(format!(
                    "{lib_src_path} was extracted without --offline-sync — add \
                     the server-side sync mounting yourself: gate a \
                     `mount_offline_sync(app)` call before `.run()` behind \
                     `#[cfg(feature = \"offline-sync\")]` (the full helper is in \
                     docs/guide/tauri-mobile-offline-sync.md)."
                ));
            }
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
    // Bundle identifier: reverse-DNS, valid on BOTH stores. Android forbids
    // hyphens in application-id segments (`cargo tauri android init` panics,
    // tauri-apps/tauri#9707) and Apple forbids underscores, so both are
    // stripped — the same normalization as the thin-client generator.
    let identifier = super::tauri::derive_mobile_identifier(package_name);
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
    autumn_web_dep: Option<&ShellAutumnWebDep>,
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
    // background SyncEngine, so it needs autumn-web directly — mirroring the
    // app's own dependency source (version, path, or git) so cargo unifies
    // both edges into one instance.
    let sync_dep = autumn_web_dep.map_or_else(String::new, |dep| {
        format!(
            "\n# Offline sync (issue #1508): the shell opens the local SyncStore and runs\n\
             # the background SyncEngine (autumn_web::sync). The dependency mirrors the\n\
             # app crate's own autumn-web source so cargo unifies both dependency edges.\n\
             {}\n",
            dep.dep_entry
        )
    });
    let patch_section = autumn_web_dep
        .and_then(|dep| dep.patch_entry.as_deref())
        .unwrap_or("");
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
{patch_section}"#
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
    // routes reach the SAME file the background engine syncs (see
    // start_background_sync below). In your routes, open the store ONCE
    // (e.g. behind a OnceLock) and clone it per use — clones share one
    // connection; repeated SyncStore::open calls pay setup every time.
    let sync_db = data_root.join("sync.db");
    std::env::set_var("AUTUMN_SYNC__DB_PATH", sync_db.to_string_lossy().as_ref());
"#;

/// Render the shell's `src/lib.rs`. `crate_ident` is the app's library crate
/// identifier ([`AppMeta::lib_ident`]) used for the `<crate_ident>::serve()`
/// call — `[lib].name` when the app sets one.
#[allow(clippy::too_many_lines)]
fn render_mobile_lib_rs(package_name: &str, crate_ident: &str, offline_sync: bool) -> String {
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
        // Hyphens are stripped: Android forbids them in application-id
        // segments (`cargo tauri android init` panics on them), Apple
        // forbids underscores — same normalization as the thin client.
        assert_eq!(parsed["identifier"], "com.example.myapp");
        assert_eq!(parsed["productName"], "My App");
        assert_eq!(parsed["version"], "1.2.3");
        assert!(parsed["bundle"].get("externalBin").is_none());
        assert!(parsed["bundle"].get("resources").is_none());
    }

    #[test]
    fn conf_identifier_is_android_and_ios_safe() {
        // Underscores (Apple-forbidden) and hyphens (Android-forbidden) are
        // both stripped from the identifier segment.
        let conf = render_mobile_tauri_conf("my_app", "0.1.0");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert_eq!(parsed["identifier"], "com.example.myapp");
        assert_eq!(parsed["productName"], "My App");

        let conf = render_mobile_tauri_conf("my-kebab_mix", "0.1.0");
        let parsed: serde_json::Value = serde_json::from_str(&conf).unwrap();
        assert_eq!(parsed["identifier"], "com.example.mykebabmix");
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
        let lib = render_mobile_lib_rs("my-app", "my_app", false);
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
        let lib = render_mobile_lib_rs("my-app", "my_app", false);
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
        let lib = render_mobile_lib_rs("my-app", "my_app", false);
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
        let lib = render_mobile_lib_rs("my-app", "my_app", false);
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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();

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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();
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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();

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

        let plan = plan_tauri_mobile(tmp.path(), TauriMobileOptions::default()).unwrap();
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
            render_mobile_cargo_toml("my-app", true, None),
        )
        .unwrap();
        std::fs::write(
            tauri.join("src").join("lib.rs"),
            render_mobile_lib_rs("my-app", "my_app", false),
        )
        .unwrap();
        assert!(
            ensure_no_other_mode_scaffold(mobile.path()).is_ok(),
            "same-mode regenerate must not be blocked by the guard"
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
        let mut plan = Plan::new(Path::new("."));
        let tmp = stock_app_dir("my-app");
        let dep = shell_autumn_web_dep(tmp.path(), &mut plan);
        let toml_src = render_mobile_cargo_toml("my-app", true, Some(&dep));
        assert!(
            toml_src.contains(r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#),
            "the shell must depend on autumn-web (offline-sync) at the app's requirement"
        );
        assert!(
            !toml_src.contains("[patch.crates-io]"),
            "an unpatched registry dep must not grow a patch section"
        );
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
        toml::from_str::<toml::Value>(&toml_src).expect("generated Cargo.toml must parse");
        // Without offline sync the dependency (and any sync trace) is absent.
        let no_sync = render_mobile_cargo_toml("my-app", true, None);
        assert!(!no_sync.contains("offline-sync"));
        assert!(!no_sync.contains("autumn-web ="));
    }

    /// Write `deps` verbatim under `[dependencies]` (plus optional extra
    /// top-level TOML) and resolve the shell's autumn-web dependency.
    fn shell_dep_for(deps: &str, extra: &str) -> (ShellAutumnWebDep, Vec<String>) {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
                 [dependencies]\n{deps}\n{extra}"
            ),
        )
        .unwrap();
        let mut plan = Plan::new(tmp.path());
        let dep = shell_autumn_web_dep(tmp.path(), &mut plan);
        (dep, plan.warnings)
    }

    #[test]
    fn lexical_relative_handles_ancestors_dotdots_and_foreign_roots() {
        use std::path::Path;
        // Ancestor workspace root → up out of src-tauri/ and the app dir.
        assert_eq!(
            lexical_relative(
                Path::new("/ws/vendor/autumn"),
                Path::new("/ws/app/src-tauri")
            ),
            Some(std::path::PathBuf::from("../../vendor/autumn"))
        );
        // `..` chains in the declared path fold before diffing.
        assert_eq!(
            lexical_relative(
                Path::new("/ws/vendor/../autumn/./core"),
                Path::new("/ws/app/src-tauri")
            ),
            Some(std::path::PathBuf::from("../../autumn/core"))
        );
        // Identical directories resolve to `.`.
        assert_eq!(
            lexical_relative(Path::new("/ws/app"), Path::new("/ws/app")),
            Some(std::path::PathBuf::from("."))
        );
        // Nested target below the base needs no `..` at all.
        assert_eq!(
            lexical_relative(
                Path::new("/ws/app/src-tauri/gen"),
                Path::new("/ws/app/src-tauri")
            ),
            Some(std::path::PathBuf::from("gen"))
        );
        // A `..` escaping the root cannot be folded lexically.
        assert_eq!(
            lexical_relative(Path::new("/../etc"), Path::new("/ws")),
            None
        );
    }

    #[test]
    fn offline_shell_dep_mirrors_path_source_relative_to_src_tauri() {
        let (dep, warnings) = shell_dep_for(
            "autumn-web = { path = \"../autumn\", version = \"0.9.1\" }",
            "",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", path = "../../autumn", features = ["offline-sync"] }"#,
            "a relative path dep must be recomputed for src-tauri/ (one level deeper)"
        );
        assert!(dep.patch_entry.is_none());
        assert!(warnings.is_empty(), "got {warnings:?}");
    }

    #[test]
    fn offline_shell_dep_mirrors_absolute_path_source() {
        let (dep, warnings) = shell_dep_for("autumn-web = { path = \"/src/autumn/autumn\" }", "");
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { path = "/src/autumn/autumn", features = ["offline-sync"] }"#,
        );
        assert!(warnings.is_empty(), "got {warnings:?}");
    }

    #[test]
    fn offline_shell_dep_mirrors_git_source_with_rev() {
        let (dep, warnings) = shell_dep_for(
            "autumn-web = { git = \"https://github.com/madmax983/autumn\", rev = \"abc123\" }",
            "",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { git = "https://github.com/madmax983/autumn", rev = "abc123", features = ["offline-sync"] }"#,
        );
        assert!(warnings.is_empty(), "got {warnings:?}");
    }

    #[test]
    fn offline_shell_dep_preserves_default_features_false() {
        // Feature unification is per dependency edge: a shell edge without
        // `default-features = false` would re-enable the framework's default
        // features across the whole src-tauri build.
        let (dep, warnings) = shell_dep_for(
            "autumn-web = { version = \"0.9.1\", default-features = false, \
             features = [\"maud\"] }",
            "",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", default-features = false, features = ["offline-sync"] }"#,
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        // Path deps preserve it too, and the pre-2021 `default_features`
        // spelling cargo still accepts is recognized.
        let (dep, _) = shell_dep_for(
            "autumn-web = { path = \"../autumn\", default_features = false }",
            "",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { path = "../../autumn", default-features = false, features = ["offline-sync"] }"#,
        );

        // And an edge that does NOT opt out stays unchanged.
        let (dep, _) = shell_dep_for("autumn-web = { version = \"0.9.1\" }", "");
        assert!(
            !dep.dep_entry.contains("default-features"),
            "got: {}",
            dep.dep_entry
        );
    }

    #[test]
    fn offline_shell_dep_resolves_renamed_dependency_by_package() {
        // Apps may rename the framework dep: the source must be resolved by
        // PACKAGE, and the shell edge is emitted under the real package name.
        let (dep, warnings) = shell_dep_for(
            "autumn = { package = \"autumn-web\", version = \"0.9.1\" }",
            "",
        );
        assert_eq!(
            dep.dep_entry, r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#,
            "a renamed registry dep must still resolve (not fall back)"
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        let (dep, warnings) = shell_dep_for(
            "autumn = { package = \"autumn-web\", path = \"../autumn\" }",
            "",
        );
        assert_eq!(
            dep.dep_entry, r#"autumn-web = { path = "../../autumn", features = ["offline-sync"] }"#,
            "a renamed path dep must mirror its source"
        );
        assert!(warnings.is_empty(), "got {warnings:?}");
    }

    #[test]
    fn offline_feature_edit_targets_the_actual_dependency_key() {
        // Cargo feature syntax names the dependency KEY: with a renamed dep
        // the edit must reference `<key>/offline-sync`, or cargo rejects the
        // manifest outright.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn = { package = \"autumn-web\", version = \"0.9.1\" }\n\n\
             [features]\ndefault = [\"flash\"]\nflash = [\"autumn/flash\"]\n\
             embed-assets = [\"autumn/embed-assets\"]\n",
        )
        .unwrap();
        let mut plan = Plan::new(tmp.path());
        plan_app_offline_sync_feature(tmp.path(), &mut plan);
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
        let edited = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Modify { path, contents } if path.ends_with("Cargo.toml") => {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("the feature edit must be planned");
        assert!(
            edited.contains(r#"offline-sync = ["autumn/offline-sync"]"#),
            "the feature must reference the RENAMED dependency key, got:\n{edited}"
        );
        assert!(
            !edited.contains(r#"["autumn-web/offline-sync"]"#),
            "the package name is not a valid feature dependency here"
        );
    }

    #[test]
    fn offline_shell_dep_mirrors_crates_io_patch_into_shell_manifest() {
        // The common test-harness / local-development shape: a registry dep
        // plus a [patch.crates-io] path override. The shell is its own
        // [workspace], so the patch must be mirrored explicitly.
        let (dep, warnings) = shell_dep_for(
            "autumn-web = \"0.9.1\"",
            "\n[patch.crates-io]\nautumn-web = { path = \"/src/autumn/autumn\" }\n",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#,
        );
        let patch = dep
            .patch_entry
            .clone()
            .expect("the crates-io patch must be mirrored");
        assert!(patch.contains("[patch.crates-io]"), "got: {patch}");
        assert!(
            patch.contains(r#"autumn-web = { path = "/src/autumn/autumn" }"#),
            "got: {patch}"
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        // And the rendered manifest carries the section and stays valid TOML.
        let toml_src = render_mobile_cargo_toml("my-app", true, Some(&dep));
        assert!(toml_src.contains("[patch.crates-io]"));
        toml::from_str::<toml::Value>(&toml_src).expect("generated Cargo.toml must parse");
    }

    #[test]
    fn offline_shell_dep_default_features_across_workspace_inheritance() {
        // Build a workspace whose root declares the dep, with the member
        // inheriting it via `workspace = true` plus `member_extra` keys.
        fn workspace_dep(workspace_entry: &str, member_extra: &str) -> ShellAutumnWebDep {
            let tmp = TempDir::new().unwrap();
            std::fs::write(
                tmp.path().join("Cargo.toml"),
                format!(
                    "[workspace]\nmembers = [\"app\"]\n\n\
                     [workspace.dependencies]\nautumn-web = {workspace_entry}\n"
                ),
            )
            .unwrap();
            let app = tmp.path().join("app");
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(
                app.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
                     [dependencies]\nautumn-web = {{ workspace = true{member_extra} }}\n"
                ),
            )
            .unwrap();
            let mut plan = Plan::new(&app);
            let dep = shell_autumn_web_dep(&app, &mut plan);
            assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
            dep
        }

        // Member-level opt-out survives resolution even when the workspace
        // entry leaves defaults on (previously erased by the inheritance
        // walk replacing the member table).
        let dep = workspace_dep(r#"{ version = "0.9.1" }"#, ", default-features = false");
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", default-features = false, features = ["offline-sync"] }"#,
        );
        // Legacy spelling too.
        let dep = workspace_dep(r#"{ version = "0.9.1" }"#, ", default_features = false");
        assert!(
            dep.dep_entry.contains("default-features = false"),
            "got: {}",
            dep.dep_entry
        );

        // Member sets nothing: the workspace-level value wins.
        let dep = workspace_dep(r#"{ version = "0.9.1", default-features = false }"#, "");
        assert!(
            dep.dep_entry.contains("default-features = false"),
            "workspace-level opt-out must apply, got: {}",
            dep.dep_entry
        );

        // Member re-enables defaults over a workspace opt-out — Cargo's
        // documented inheritance rule.
        let dep = workspace_dep(
            r#"{ version = "0.9.1", default-features = false }"#,
            ", default-features = true",
        );
        assert!(
            !dep.dep_entry.contains("default-features"),
            "a member re-enable must win over the workspace opt-out, got: {}",
            dep.dep_entry
        );

        // Neither side opts out: no default-features on the shell edge.
        let dep = workspace_dep(r#"{ version = "0.9.1" }"#, "");
        assert!(
            !dep.dep_entry.contains("default-features"),
            "got: {}",
            dep.dep_entry
        );
    }

    #[test]
    fn offline_feature_edit_requires_default_enablement() {
        fn manifest(features: &str) -> (TempDir, std::path::PathBuf) {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("Cargo.toml");
            std::fs::write(
                &path,
                format!(
                    "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
                     [dependencies]\nautumn-web = \"0.9.1\"\n\n\
                     [features]\n{features}"
                ),
            )
            .unwrap();
            (tmp, path)
        }
        fn planned_edit(tmp: &TempDir) -> (Option<String>, Vec<String>) {
            let mut plan = Plan::new(tmp.path());
            plan_app_offline_sync_feature(tmp.path(), &mut plan);
            let edit = plan.actions.iter().find_map(|a| match a {
                Action::Modify { contents, .. } => Some(contents.clone()),
                _ => None,
            });
            (edit, plan.warnings)
        }

        // Feature present AND directly in default: wired — untouched,
        // silent (byte-identical, since no action is planned at all).
        let (tmp, _) = manifest(
            "default = [\"flash\", \"offline-sync\"]\nflash = []\n\
             offline-sync = [\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        assert!(edit.is_none(), "wired manifests must stay untouched");
        assert!(warnings.is_empty(), "got {warnings:?}");

        // Enabled TRANSITIVELY through another default feature: also wired.
        let (tmp, _) = manifest(
            "default = [\"flash\", \"full\"]\nflash = []\n\
             full = [\"offline-sync\"]\n\
             offline-sync = [\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        assert!(
            edit.is_none(),
            "transitive default enablement counts as wired"
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        // A default entry naming only the DEP feature does not enable the
        // crate's own cfg — not wired.
        let (tmp, _) = manifest(
            "default = [\"flash\", \"autumn-web/offline-sync\"]\nflash = []\n\
             offline-sync = [\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        assert!(
            edit.is_some() || !warnings.is_empty(),
            "a dep-feature default entry must not count as wired"
        );

        // Feature present but NOT in default, stock default line: only the
        // default line is edited; the feature is not re-declared.
        let (tmp, _) = manifest(
            "default = [\"flash\"]\nflash = []\n\
             offline-sync = [\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        let edited = edit.expect("the stock default line must gain the feature");
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert!(
            edited.contains(r#"default = ["flash", "offline-sync"]"#),
            "got:\n{edited}"
        );
        assert_eq!(
            edited.matches("offline-sync = [").count(),
            1,
            "the existing feature declaration must not be duplicated:\n{edited}"
        );

        // Feature present, NOT in default, customised default line: loud
        // warning instead of a guessed rewrite; manifest untouched.
        let (tmp, _) = manifest(
            "default = [\"flash\", \"extra\"]\nflash = []\nextra = []\n\
             offline-sync = [\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        assert!(
            edit.is_none(),
            "a customised default line is never rewritten"
        );
        assert!(
            warnings.iter().any(|w| w.contains(
                "`default` \
                 doesn't enable it"
            ) || w.contains("doesn't enable it")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn offline_feature_detection_survives_spacing_and_layout_variants() {
        // Detection must key on the parsed [features] TABLE, not on the
        // exact `offline-sync = [` byte sequence: `offline-sync=[]` (no
        // spaces) or a multiline array are the same manifest to cargo, and
        // a substring miss would route them into the fresh-insert branch —
        // which would write a DUPLICATE `offline-sync` key that cargo
        // rejects outright.
        fn manifest(features: &str) -> TempDir {
            let tmp = TempDir::new().unwrap();
            std::fs::write(
                tmp.path().join("Cargo.toml"),
                format!(
                    "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
                     [dependencies]\nautumn-web = \"0.9.1\"\n\n\
                     [features]\n{features}"
                ),
            )
            .unwrap();
            tmp
        }
        fn planned_edit(tmp: &TempDir) -> (Option<String>, Vec<String>) {
            let mut plan = Plan::new(tmp.path());
            plan_app_offline_sync_feature(tmp.path(), &mut plan);
            let edit = plan.actions.iter().find_map(|a| match a {
                Action::Modify { contents, .. } => Some(contents.clone()),
                _ => None,
            });
            (edit, plan.warnings)
        }

        // No-spaces spelling, already wired: silent no-op, no duplicate.
        let tmp = manifest(
            "default = [\"flash\", \"offline-sync\"]\nflash = []\n\
             offline-sync=[\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        assert!(
            edit.is_none(),
            "`offline-sync=[…]` (no spaces) must be detected as already \
             defined, got:\n{edit:?}"
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        // No-spaces spelling, not in default: only the stock default line
        // gains the feature — the declaration is not re-inserted.
        let tmp = manifest(
            "default = [\"flash\"]\nflash = []\n\
             offline-sync=[\"autumn-web/offline-sync\"]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        let edited = edit.expect("the stock default line must gain the feature");
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert!(
            edited.contains(r#"default = ["flash", "offline-sync"]"#),
            "got:\n{edited}"
        );
        // A duplicated key would fail to parse — the sharpest duplicate check.
        toml::from_str::<toml::Value>(&edited)
            .expect("the edited manifest must stay valid TOML (no duplicate keys)");
        assert_eq!(
            edited.matches("offline-sync=[").count() + edited.matches("offline-sync = [").count(),
            1,
            "the existing declaration must not be duplicated:\n{edited}"
        );

        // Multiline array form, not in default: same — detected, no
        // duplicate declaration inserted.
        let tmp = manifest(
            "default = [\"flash\"]\nflash = []\n\
             offline-sync = [\n    \"autumn-web/offline-sync\",\n]\n",
        );
        let (edit, warnings) = planned_edit(&tmp);
        let edited = edit.expect("the stock default line must gain the feature");
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert!(
            edited.contains(r#"default = ["flash", "offline-sync"]"#),
            "got:\n{edited}"
        );
        toml::from_str::<toml::Value>(&edited)
            .expect("the edited manifest must stay valid TOML (no duplicate keys)");

        // Regression: a manifest WITHOUT the feature still gets the fresh
        // insert (detection must not report false positives either).
        let tmp = manifest("default = [\"flash\"]\nflash = []\n");
        let (edit, warnings) = planned_edit(&tmp);
        let edited = edit.expect("an absent feature must still be inserted");
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert!(
            edited.contains(r#"offline-sync = ["autumn-web/offline-sync"]"#),
            "got:\n{edited}"
        );
        assert!(
            edited.contains(r#"default = ["flash", "offline-sync"]"#),
            "got:\n{edited}"
        );
    }

    #[test]
    fn offline_shell_dep_mirrors_alternate_registry_selection() {
        // A private-registry dep must not collapse into a bare crates.io
        // version requirement on the shell edge.
        let (dep, warnings) = shell_dep_for(
            "autumn-web = { version = \"0.9.1\", registry = \"internal\" }",
            "",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", registry = "internal", features = ["offline-sync"] }"#,
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        // Combined with a default-features opt-out, both survive.
        let (dep, _) = shell_dep_for(
            "autumn-web = { version = \"0.9.1\", registry = \"internal\", default-features = false }",
            "",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", registry = "internal", default-features = false, features = ["offline-sync"] }"#,
        );

        // Workspace-inherited entries carry their registry key through the
        // inheritance walk (members cannot override it, so the resolved
        // workspace table is authoritative).
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\n\
             autumn-web = { version = \"0.9.1\", registry = \"internal\" }\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn-web = { workspace = true }\n",
        )
        .unwrap();
        let mut plan = Plan::new(&app);
        let dep = shell_autumn_web_dep(&app, &mut plan);
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", registry = "internal", features = ["offline-sync"] }"#,
        );
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);

        // No registry key: output stays byte-identical to before.
        let (dep, _) = shell_dep_for("autumn-web = { version = \"0.9.1\" }", "");
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#,
        );
    }

    #[test]
    fn offline_shell_dep_mirrors_renamed_crates_io_patch() {
        // A renamed patch entry is just as valid as the literal key: it
        // must be matched by its `package` field and mirrored whole —
        // key, rename, and source.
        let (dep, warnings) = shell_dep_for(
            "autumn-web = \"0.9.1\"",
            "\n[patch.crates-io]\naw_local = { package = \"autumn-web\", path = \"/src/autumn/autumn\" }\n",
        );
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#,
        );
        let patch = dep
            .patch_entry
            .clone()
            .expect("the renamed patch must be mirrored, not silently dropped");
        assert!(
            patch.contains(r#"aw_local = { package = "autumn-web", path = "/src/autumn/autumn" }"#),
            "the entry must be mirrored whole (key + package + source), got: {patch}"
        );
        assert!(warnings.is_empty(), "got {warnings:?}");

        // And the rendered manifest stays valid TOML.
        let toml_src = render_mobile_cargo_toml("my-app", true, Some(&dep));
        toml::from_str::<toml::Value>(&toml_src).expect("generated Cargo.toml must parse");
    }

    #[test]
    fn offline_shell_dep_mirrors_renamed_patch_from_ancestor_workspace() {
        // The renamed-patch lookup walks ancestor manifests exactly like
        // the literal-key one; a relative patch path declared at the
        // workspace root is absolutized against that root.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [patch.crates-io]\naw_local = { package = \"autumn-web\", path = \"vendor/autumn\" }\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn-web = \"0.9.1\"\n",
        )
        .unwrap();
        let mut plan = Plan::new(&app);
        let dep = shell_autumn_web_dep(&app, &mut plan);
        let patch = dep
            .patch_entry
            .expect("the ancestor workspace's renamed patch must be mirrored");
        assert!(
            patch
                .contains(r#"aw_local = { package = "autumn-web", path = "../../vendor/autumn" }"#),
            "the mirrored patch path must be relative to src-tauri/, got: {patch}"
        );
        assert!(
            !patch.contains(&tmp.path().display().to_string()),
            "no machine-specific absolute prefix may leak into the manifest"
        );
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
    }

    #[test]
    fn offline_member_local_patch_is_skipped_with_a_warning() {
        // Cargo ignores [patch] tables outside the workspace root — a
        // member-local patch must NOT be mirrored (the app never builds
        // against it), and the skip must be explained.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn-web = \"0.9.1\"\n\n\
             [patch.crates-io]\nautumn-web = { path = \"/src/ignored/autumn\" }\n",
        )
        .unwrap();
        let mut plan = Plan::new(&app);
        let dep = shell_autumn_web_dep(&app, &mut plan);
        assert!(
            dep.patch_entry.is_none(),
            "a member-local patch cargo ignores must not be mirrored, got {:?}",
            dep.patch_entry
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("outside the workspace root")),
            "the skip must be explained, got {:?}",
            plan.warnings
        );
    }

    #[test]
    fn offline_root_patch_wins_over_a_stale_member_local_one() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [patch.crates-io]\nautumn-web = { path = \"/src/effective/autumn\" }\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn-web = \"0.9.1\"\n\n\
             [patch.crates-io]\nautumn-web = { path = \"/src/stale/autumn\" }\n",
        )
        .unwrap();
        let mut plan = Plan::new(&app);
        let dep = shell_autumn_web_dep(&app, &mut plan);
        let patch = dep
            .patch_entry
            .expect("the workspace root's patch must be mirrored");
        assert!(
            patch.contains(r#"autumn-web = { path = "/src/effective/autumn" }"#),
            "the EFFECTIVE (root) patch must win, got: {patch}"
        );
        assert!(
            !patch.contains("/src/stale/autumn"),
            "the ignored member-local patch must not leak in: {patch}"
        );
    }

    #[test]
    fn offline_excluded_member_uses_its_own_patch_table() {
        // An ancestor workspace whose `exclude` names the app makes the app
        // standalone: its own patch table is the effective one.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\nexclude = [\"app\"]\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn-web = \"0.9.1\"\n\n\
             [patch.crates-io]\nautumn-web = { path = \"/src/standalone/autumn\" }\n",
        )
        .unwrap();
        let mut plan = Plan::new(&app);
        let dep = shell_autumn_web_dep(&app, &mut plan);
        let patch = dep
            .patch_entry
            .expect("an excluded (standalone) app's own patch must be mirrored");
        assert!(
            patch.contains(r#"autumn-web = { path = "/src/standalone/autumn" }"#),
            "got: {patch}"
        );
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
    }

    #[test]
    fn offline_shell_dep_resolves_workspace_inherited_path_dep() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nautumn-web = { path = \"vendor/autumn\" }\n",
        )
        .unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.2.3\"\n\n\
             [dependencies]\nautumn-web = { workspace = true }\n",
        )
        .unwrap();
        let mut plan = Plan::new(&app);
        let dep = shell_autumn_web_dep(&app, &mut plan);
        // Declared in the workspace root: re-relativized against the
        // generated src-tauri/ — src-tauri/Cargo.toml is checked in, so an
        // absolute path would break every other checkout.
        assert_eq!(
            dep.dep_entry,
            r#"autumn-web = { path = "../../vendor/autumn", features = ["offline-sync"] }"#,
        );
        assert!(
            !dep.dep_entry.contains(&tmp.path().display().to_string()),
            "no machine-specific absolute prefix may leak into the manifest"
        );
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
    }

    #[test]
    fn offline_shell_dep_warns_and_falls_back_when_source_is_unrepresentable() {
        // No autumn-web dependency at all: fall back to the CLI's own version
        // with a loud warning instead of silently inventing a registry edge.
        let (dep, warnings) = shell_dep_for("serde = \"1\"", "");
        assert_eq!(
            dep.dep_entry,
            format!(
                r#"autumn-web = {{ version = "{}", features = ["offline-sync"] }}"#,
                env!("CARGO_PKG_VERSION"),
            ),
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("could not determine the app's autumn-web dependency source")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn offline_shell_dep_warns_when_patch_is_unrepresentable() {
        let (dep, warnings) = shell_dep_for(
            "autumn-web = \"0.9.1\"",
            "\n[patch.crates-io]\nautumn-web = { version = \"0.9.2\" }\n",
        );
        assert!(dep.patch_entry.is_none());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("[patch.crates-io] override of autumn-web could not")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn offline_lib_rs_wires_store_engine_and_resume_trigger() {
        let lib = render_mobile_lib_rs("my-app", "my_app", true);
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

    /// The CHANGELOG promises the no-flag emission is byte-identical to the
    /// pre-#1508 scaffold; `render_mobile_lib_rs` now assembles the file
    /// from conditionals, so pin the default output byte-for-byte.
    #[test]
    fn default_lib_rs_matches_the_golden_snapshot() {
        let generated = render_mobile_lib_rs("golden-app", "golden_app", false);
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/tauri_mobile_default_lib.rs.golden"
        );
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(path, &generated).expect("write golden");
        }
        let golden = std::fs::read_to_string(path).expect("golden snapshot file");
        assert_eq!(
            generated, golden,
            "the DEFAULT (no --offline-sync) shell lib.rs emission changed. If \
             this is intentional, regenerate the snapshot with \
             `UPDATE_GOLDEN=1 cargo test -p autumn-cli default_lib_rs_matches` \
             and mention the template change in the commit"
        );
    }

    #[test]
    fn lib_rs_without_offline_flag_has_no_sync_wiring() {
        let lib = render_mobile_lib_rs("my-app", "my_app", false);
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
    fn shell_autumn_web_dep_reads_string_and_table_version_deps() {
        let tmp = stock_app_dir("my-app");
        let mut plan = Plan::new(tmp.path());
        assert_eq!(
            shell_autumn_web_dep(tmp.path(), &mut plan).dep_entry,
            r#"autumn-web = { version = "0.9.1", features = ["offline-sync"] }"#
        );

        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
             autumn-web = { version = \"1.2.3\", features = [\"mail\"] }\n",
        )
        .unwrap();
        assert_eq!(
            shell_autumn_web_dep(tmp.path(), &mut plan).dep_entry,
            r#"autumn-web = { version = "1.2.3", features = ["offline-sync"] }"#
        );
        assert!(plan.warnings.is_empty(), "got {:?}", plan.warnings);
    }
}
