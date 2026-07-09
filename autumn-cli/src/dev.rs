//! `autumn dev` -- watch for file changes and rebuild/restart the server.
//!
//! Orchestrates a development workflow:
//! 1. Compile the project with `cargo build`.
//! 2. Start the application binary.
//! 3. Watch source, config, migrations, and static assets for changes.
//! 4. Route each change to the cheapest valid action:
//!    - `cargo build` + restart for Rust/build changes
//!    - restart only for config and migration changes
//!    - Tailwind-only rebuilds for CSS input/config changes
//!    - browser reload only for plain static asset changes
//!
//! Debounces rapid file changes (e.g. editor save + format) to avoid
//! unnecessary rebuilds.

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Debounce interval for checking the shutdown flag in the watch loop.
const SHUTDOWN_CHECK_INTERVAL_MS: u64 = 200;

/// Set to `true` by the SIGINT handler to request a clean shutdown.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Default debounce interval for file change events.
const DEBOUNCE_MS: u64 = 500;

/// Top-level files that participate in change routing.
const WATCH_FILES: &[&str] = &[
    "autumn.toml",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "tailwind.config.js",
];

/// Directories that are always watched recursively, regardless of config.
const DEFAULT_WATCH_DIRS: &[&str] = &["src", "static", "templates", "migrations"];

/// Path of the project config file relative to the dev server's working directory.
const AUTUMN_TOML: &str = "autumn.toml";

/// `[dev]` section of `autumn.toml`. Unknown keys are ignored so future
/// additions to the section don't break older CLIs.
#[derive(Debug, Clone, Default, Deserialize)]
struct DevConfig {
    /// Extra directories to watch recursively, in addition to the defaults.
    /// Paths are relative to the project root.
    #[serde(default)]
    watch_dirs: Vec<String>,
}

/// Minimal slice of `autumn.toml` used to extract the `[dev]` section without
/// pulling in the full `autumn` config crate.
#[derive(Debug, Default, Deserialize)]
struct AutumnTomlDevSlice {
    #[serde(default)]
    dev: DevConfig,
}

const DEV_RELOAD_ENV: &str = "AUTUMN_DEV_RELOAD";
const DEV_RELOAD_STATE_ENV: &str = "AUTUMN_DEV_RELOAD_STATE";
const DEV_RELOAD_STATE_FILE: &str = "live-reload.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeEffect {
    Ignore,
    BrowserReloadOnly,
    TailwindOnly,
    RestartOnly,
    BuildRestart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum ReloadKind {
    #[default]
    None,
    Css,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ChangePlan {
    build: bool,
    restart: bool,
    tailwind: bool,
    reload: ReloadKind,
}

impl ChangePlan {
    fn is_empty(self) -> bool {
        !self.build && !self.restart && !self.tailwind && self.reload == ReloadKind::None
    }

    fn register(&mut self, effect: ChangeEffect) {
        match effect {
            ChangeEffect::Ignore => {}
            ChangeEffect::BrowserReloadOnly => {
                self.reload = self.reload.max(ReloadKind::Full);
            }
            ChangeEffect::TailwindOnly => {
                self.tailwind = true;
                self.reload = self.reload.max(ReloadKind::Css);
            }
            ChangeEffect::RestartOnly => {
                self.restart = true;
                self.reload = self.reload.max(ReloadKind::Full);
            }
            ChangeEffect::BuildRestart => {
                self.build = true;
                self.restart = true;
                self.tailwind = false;
                self.reload = ReloadKind::Full;
            }
        }
    }

    const fn finalize(mut self) -> Self {
        if self.build {
            self.tailwind = false;
        }
        self
    }
}

#[derive(Debug)]
struct DevReloadState {
    path: PathBuf,
    version: u64,
}

impl DevReloadState {
    fn initialize() -> Result<Self, String> {
        let path = resolve_dev_reload_state_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }

        let state = Self { path, version: 0 };
        state.write(ReloadKind::Full)?;
        Ok(state)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn signal(&mut self, kind: ReloadKind) -> Result<(), String> {
        if kind == ReloadKind::None {
            return Ok(());
        }

        self.version = self
            .version
            .checked_add(1)
            .ok_or("live reload version overflowed")?;
        self.write(kind)
    }

    fn write(&self, kind: ReloadKind) -> Result<(), String> {
        let kind = match kind {
            ReloadKind::None | ReloadKind::Full => "full",
            ReloadKind::Css => "css",
        };
        let body = serde_json::json!({
            "version": self.version,
            "kind": kind,
        });
        std::fs::write(&self.path, body.to_string())
            .map_err(|e| format!("failed to write {}: {e}", self.path.display()))
    }
}

/// Run the dev server with file watching.
pub fn run(package: Option<&str>, show_config: bool) {
    eprintln!("\u{1F342} autumn dev\n");

    // Warn when maintenance mode is currently active so the operator is not
    // surprised by 503 responses during local development.
    if let Some(config) = crate::maintenance::check_status(None) {
        eprintln!("  \u{26A0}\u{FE0F}  MAINTENANCE MODE IS ON");
        if let Some(msg) = &config.message {
            eprintln!("     Message: {msg}");
        }
        eprintln!("     Run `autumn maintenance off` to disable.");
        eprintln!();
    }

    // Register SIGINT handler so Ctrl+C triggers a graceful shutdown instead
    // of immediately terminating the process (and leaving the child running).
    if let Err(err) = ctrlc::set_handler(move || {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }) {
        eprintln!("  Warning: failed to set Ctrl-C handler: {err}");
    }

    let mut reload_state = match DevReloadState::initialize() {
        Ok(state) => Some(state),
        Err(error) => {
            eprintln!("  Warning: live reload disabled: {error}");
            None
        }
    };
    // Initial build
    if !cargo_build(package, false) {
        eprintln!("\u{2717} Initial build failed. Fix errors and save to retry.\n");
    }

    let binary = find_binary(package, false);
    let mut child = start_server(
        &binary,
        reload_state.as_ref().map(DevReloadState::path),
        show_config,
    );

    let normalized_dirs = sanitize_custom_watch_dirs(load_dev_config(Path::new(AUTUMN_TOML)));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let custom_watch_dirs = resolve_custom_watch_dirs(&normalized_dirs, &cwd);

    // Set up file watcher
    let (tx, rx) = mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(DEBOUNCE_MS), tx)
        .expect("failed to create file watcher");

    let watcher = debouncer.watcher();

    // Watch the default directories.
    for dir in DEFAULT_WATCH_DIRS {
        let path = Path::new(dir);
        if path.exists()
            && let Err(e) = watcher.watch(path, notify::RecursiveMode::Recursive)
        {
            eprintln!("  Warning: could not watch {dir}/: {e}");
        }
    }

    // Watch any additional directories from `[dev] watch_dirs` in autumn.toml.
    for dir in &custom_watch_dirs {
        let display = dir.relative.display();
        if let Err(e) = watcher.watch(&dir.relative, notify::RecursiveMode::Recursive) {
            eprintln!("  Warning: could not watch {display}/: {e}");
        } else {
            eprintln!("  Watching custom directory: {display}/");
        }
    }

    // Watch the project root for config and build script changes.
    if let Err(e) = watcher.watch(Path::new("."), notify::RecursiveMode::NonRecursive) {
        eprintln!("  Warning: could not watch project root: {e}");
    }

    eprintln!("  Watching for changes... (press Ctrl+C to stop)\n");

    // Main event loop – periodically checks the shutdown flag so that a
    // Ctrl+C breaks the loop and triggers
    // graceful server shutdown via `stop_server` below.
    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            eprintln!("\n  Shutting down...");
            break;
        }

        if !process_events(
            &rx,
            &custom_watch_dirs,
            package,
            &mut child,
            reload_state.as_mut(),
            show_config,
        ) {
            break;
        }
    }

    stop_server(&mut child);
}

/// Read `[dev]` from `autumn.toml`. Missing file or unparseable content
/// degrades gracefully to defaults so `autumn dev` still works.
fn load_dev_config(path: &Path) -> DevConfig {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return DevConfig::default();
    };
    parse_dev_config(&contents).unwrap_or_else(|err| {
        eprintln!(
            "  Warning: failed to parse [dev] section in {}: {err}",
            path.display()
        );
        DevConfig::default()
    })
}

fn parse_dev_config(toml_str: &str) -> Result<DevConfig, toml::de::Error> {
    let parsed: AutumnTomlDevSlice = toml::from_str(toml_str)?;
    Ok(parsed.dev)
}

/// Normalize and validate a single `[dev].watch_dirs` entry.
///
/// Returns the normalized path string (with `./` segments collapsed) on
/// success, or `Err(reason)` if the entry must be rejected. Reasons cover
/// absolute paths, parent traversal (`..`), `target/`, and dotted
/// directories (e.g. `.git`) — any of which could subscribe huge or wrong
/// trees and flood the debouncer.
fn normalize_watch_dir(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("entry is empty");
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("absolute paths are not allowed; use a project-relative path");
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                if part == std::ffi::OsStr::new("target") {
                    return Err("`target` is reserved for cargo build artifacts");
                }
                if part.to_string_lossy().starts_with('.') {
                    return Err("dotted directories (e.g. `.git`) are not allowed; \
                         the watcher would still pump their events");
                }
                normalized.push(part);
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err("parent traversal (`..`) is not allowed");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err("absolute paths are not allowed; use a project-relative path");
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err("entry resolves to an empty path");
    }

    Ok(normalized.to_string_lossy().into_owned())
}

/// A custom watch directory resolved at startup, with both the relative
/// form (passed to `watcher.watch`) and the absolute form (used to anchor
/// event matching to the project root).
///
/// `notify` backends typically dispatch absolute paths, but on some
/// platforms relative paths can also flow through, so matching tries both.
#[derive(Debug, Clone)]
struct CustomWatchDir {
    /// Relative path as configured. Passed to `watcher.watch()`.
    relative: PathBuf,
    /// Absolute (canonicalized when possible) path used to anchor event
    /// matching to the project root, so a custom dir like `views` can't
    /// false-match against an ancestor directory in the absolute path
    /// (e.g. project at `/home/alice/views/app`).
    absolute: PathBuf,
}

impl CustomWatchDir {
    /// True if `event_path` falls inside this custom watch directory.
    fn matches(&self, event_path: &Path) -> bool {
        event_path.starts_with(&self.absolute) || event_path.starts_with(&self.relative)
    }
}

/// Resolve sanitized relative watch dirs to `CustomWatchDir` entries,
/// dropping any that don't exist on disk. Logs a warning per dropped
/// entry so misconfiguration is visible.
fn resolve_custom_watch_dirs(normalized: &[String], cwd: &Path) -> Vec<CustomWatchDir> {
    normalized
        .iter()
        .filter_map(|rel| {
            let relative = PathBuf::from(rel);
            let cwd_joined = cwd.join(&relative);
            if !cwd_joined.exists() {
                eprintln!("  Warning: configured watch directory {rel}/ does not exist; skipping");
                return None;
            }
            let absolute = std::fs::canonicalize(&cwd_joined).unwrap_or(cwd_joined);
            Some(CustomWatchDir { relative, absolute })
        })
        .collect()
}

/// Filter custom watch dirs to those that are safe and not already covered by
/// the defaults. Keeps the watcher list deterministic and prevents hostile
/// entries (e.g. `target`, absolute paths, `..`) from subscribing huge trees.
fn sanitize_custom_watch_dirs(config: DevConfig) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for raw in config.watch_dirs {
        let normalized = match normalize_watch_dir(&raw) {
            Ok(value) => value,
            Err(reason) => {
                eprintln!("  Warning: ignoring [dev].watch_dirs entry {raw:?}: {reason}");
                continue;
            }
        };
        if DEFAULT_WATCH_DIRS.contains(&normalized.as_str()) {
            continue;
        }
        if !seen.contains(&normalized) {
            seen.push(normalized);
        }
    }
    seen
}

/// Process a single batch of events from the debouncer channel.
/// Returns false if the channel was closed and the loop should exit.
fn process_events(
    rx: &mpsc::Receiver<Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>>,
    custom_watch_dirs: &[CustomWatchDir],
    package: Option<&str>,
    child: &mut Option<Child>,
    reload_state: Option<&mut DevReloadState>,
    show_config: bool,
) -> bool {
    match rx.recv_timeout(Duration::from_millis(SHUTDOWN_CHECK_INTERVAL_MS)) {
        Ok(Ok(events)) => {
            let plan = plan_changes(&events, custom_watch_dirs);
            if plan.is_empty() {
                return true;
            }

            let changed = collect_relevant_changes(&events, custom_watch_dirs);
            if changed.is_empty() {
                return true;
            }

            eprintln!("\n  Changed: {}", changed.join(", "));
            eprintln!("  Action: {}", describe_plan(plan));

            execute_plan(plan, package, child, reload_state, show_config);
            true
        }
        Ok(Err(error)) => {
            eprintln!("  Watch error: {error:?}");
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => true,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!("  Watch channel error: channel disconnected");
            false
        }
    }
}

/// Execute a computed change plan.
fn execute_plan(
    plan: ChangePlan,
    package: Option<&str>,
    child: &mut Option<Child>,
    mut reload_state: Option<&mut DevReloadState>,
    show_config: bool,
) {
    let mut applied_reload = ReloadKind::None;

    if plan.build {
        stop_server(child);

        if cargo_build(package, false) {
            if restart_server(
                package,
                child,
                reload_state.as_ref().map(|s| s.path()),
                show_config,
            ) {
                applied_reload = ReloadKind::Full;
            }
        } else {
            eprintln!("  \u{2717} Build failed. Waiting for changes...\n");
            *child = None;
        }
    } else {
        if plan.tailwind && tailwind_build() {
            applied_reload = applied_reload.max(ReloadKind::Css);
        }

        if plan.restart {
            stop_server(child);
            if restart_server(
                package,
                child,
                reload_state.as_ref().map(|s| s.path()),
                show_config,
            ) {
                applied_reload = ReloadKind::Full;
            }
        } else if plan.reload == ReloadKind::Full {
            applied_reload = ReloadKind::Full;
        }
    }

    if let Some(reload_state) = reload_state.as_mut()
        && let Err(error) = reload_state.signal(applied_reload)
    {
        eprintln!("  Warning: live reload signal failed: {error}");
    }
}

/// Collect display paths for all relevant file changes from a debounced batch.
///
/// Returns an empty vec if no changes are relevant.
fn collect_relevant_changes(
    events: &[notify_debouncer_mini::DebouncedEvent],
    custom_watch_dirs: &[CustomWatchDir],
) -> Vec<String> {
    events
        .iter()
        .filter(|e| is_relevant_change(&e.path, e.kind, custom_watch_dirs))
        .map(|e| e.path.display().to_string())
        .collect()
}

fn plan_changes(
    events: &[notify_debouncer_mini::DebouncedEvent],
    custom_watch_dirs: &[CustomWatchDir],
) -> ChangePlan {
    let mut plan = ChangePlan::default();
    for event in events {
        plan.register(classify_change(&event.path, event.kind, custom_watch_dirs));
    }
    plan.finalize()
}

/// Build a `cargo build` command for the given package.
pub fn build_cargo_command(package: Option<&str>, release: bool) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    if let Some(pkg) = package {
        cmd.args(["-p", pkg]);
    }
    cmd
}

/// Run `cargo build` for the given package. Returns true on success.
pub fn cargo_build(package: Option<&str>, release: bool) -> bool {
    let mut cmd = build_cargo_command(package, release);

    eprintln!("  Compiling...");
    match cmd.status() {
        Ok(status) if status.success() => {
            eprintln!("  \u{2713} Build succeeded");
            true
        }
        Ok(_) => false,
        Err(e) => {
            eprintln!("  \u{2717} Failed to run cargo build: {e}");
            false
        }
    }
}

/// Start the application binary. Returns the child process handle.
fn start_server(
    binary: &Path,
    reload_state_path: Option<&Path>,
    show_config: bool,
) -> Option<Child> {
    eprintln!("  Starting server...\n");

    // Resolve `.env` fresh right before each spawn so a hot-reload restart
    // picks up an edited `.env`. Values are injected explicitly into the child
    // process (rather than mutating this process's environment); the child also
    // self-loads via the config overlay, so this is belt-and-suspenders and
    // makes a bare `DATABASE_URL` visible to the child. A malformed `.env`
    // fails loudly. Applied BEFORE the explicit `.env(...)` calls below so those
    // win on any key overlap.
    let dotenv_vars = match autumn_web::dotenv::resolve_process_dotenv() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  \u{274C} .env: {e}");
            std::process::exit(1);
        }
    };

    let mut command = Command::new(binary);
    command.envs(dotenv_vars);
    // Inherit stdio so tracing output (including --show-config) is visible.
    // Previously used Stdio::null(), but server logs are valuable during dev.
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    if let Some(path) = reload_state_path {
        command.env(DEV_RELOAD_ENV, "1");
        command.env(DEV_RELOAD_STATE_ENV, path);
    }
    if show_config {
        command.env("AUTUMN_SHOW_CONFIG", "1");
    }

    match command.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            eprintln!("  \u{2717} Failed to start {}: {e}", binary.display());
            None
        }
    }
}

/// Stop the running server process gracefully.
fn stop_server(child: &mut Option<Child>) {
    if let Some(proc) = child {
        // Send SIGTERM on Unix for graceful shutdown
        #[cfg(unix)]
        {
            if let Some(pid) = crate::process::validate_pid_for_kill(proc.id())
                && let Err(e) = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGTERM,
                )
            {
                eprintln!("  Warning: failed to send SIGTERM to process: {e}");
            }
            // Wait briefly for graceful shutdown before forcing
            if crate::process::wait_with_timeout(proc, Duration::from_secs(5)).is_err() {
                let _ = proc.kill();
                let _ = proc.wait();
            }
        }

        #[cfg(not(unix))]
        {
            let _ = proc.kill();
            let _ = proc.wait();
        }
    }
    *child = None;
}

/// Check if a file change event is relevant enough to trigger a rebuild.
fn is_relevant_change(
    path: &Path,
    kind: DebouncedEventKind,
    custom_watch_dirs: &[CustomWatchDir],
) -> bool {
    classify_change(path, kind, custom_watch_dirs) != ChangeEffect::Ignore
}

fn classify_change(
    path: &Path,
    kind: DebouncedEventKind,
    custom_watch_dirs: &[CustomWatchDir],
) -> ChangeEffect {
    if !matches!(kind, DebouncedEventKind::Any) {
        return ChangeEffect::Ignore;
    }

    // A root-level dotenv file the config loader reads (`.env`, `.env.local`,
    // `.env.<profile>`, `.env.<profile>.local`) must restart the server so an
    // edited `.env` takes effect — even though `should_ignore_path` would
    // otherwise discard it as a dotfile. Checked before that ignore so the
    // dotenv exception wins, while all other dotfiles stay ignored.
    if is_watched_dotenv_file(path) {
        return ChangeEffect::RestartOnly;
    }

    if should_ignore_path(path) {
        return ChangeEffect::Ignore;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return ChangeEffect::Ignore;
    };

    if WATCH_FILES.contains(&file_name)
        && matches!(file_name, "Cargo.toml" | "Cargo.lock" | "build.rs")
    {
        return ChangeEffect::BuildRestart;
    }

    if WATCH_FILES.contains(&file_name) && file_name == "tailwind.config.js" {
        return ChangeEffect::TailwindOnly;
    }

    if (WATCH_FILES.contains(&file_name) && file_name == "autumn.toml")
        || is_profile_config_file(file_name)
    {
        return ChangeEffect::RestartOnly;
    }

    if has_component(path, "src") && path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
        return ChangeEffect::BuildRestart;
    }

    if has_component(path, "templates") {
        return ChangeEffect::BuildRestart;
    }

    if has_component(path, "migrations")
        && path.extension().and_then(|ext| ext.to_str()) == Some("sql")
    {
        return ChangeEffect::RestartOnly;
    }

    if has_component(path, "static") {
        if path.ends_with(Path::new("static").join("css").join("input.css")) {
            return ChangeEffect::TailwindOnly;
        }

        return ChangeEffect::BrowserReloadOnly;
    }

    // Files inside a user-configured custom watch directory don't have known
    // semantics — restart the server and trigger a full reload so the change
    // is picked up regardless of what the directory contains.
    //
    // Matching is anchored at the project root via the resolved absolute
    // path, so an entry like `views` cannot false-match an ancestor
    // directory of the same name (e.g. project at `/home/alice/views/app`).
    // The relative form is also tried so events emitted as relative paths
    // (rare but possible on some platforms) still match.
    for dir in custom_watch_dirs {
        if dir.matches(path) {
            return ChangeEffect::RestartOnly;
        }
    }

    ChangeEffect::Ignore
}

/// Editor/tooling temp or backup suffixes (the final `.`-segment) that must not
/// trigger a restart even when they decorate a dotenv filename — e.g. a vim swap
/// file `.env.swp` or a backup `.env.tmp`. `.env.example` is a committed
/// template the loader never reads, so it is excluded too.
const DOTENV_NON_LOADED_SUFFIXES: &[&str] =
    &["swp", "swo", "swx", "swpx", "tmp", "bak", "orig", "example"];

/// Whether `name` is a dotenv file the config loader actually reads: `.env`,
/// `.env.local`, `.env.<profile>`, or `.env.<profile>.local`.
///
/// Editor/backup decorations (`.env.swp`, `.env~`, `.env.tmp`, …) are rejected
/// so a save-in-progress swap file does not cause a spurious restart.
fn is_dotenv_loader_filename(name: &str) -> bool {
    if name == ".env" {
        return true;
    }
    // Must be `.env.<something>`; a bare `.env~`/`.env.swp`-style backup that
    // lacks the `.env.` prefix is rejected here, and a trailing `~` backup
    // (e.g. `.env.local~`) is rejected explicitly.
    let Some(rest) = name.strip_prefix(".env.") else {
        return false;
    };
    if rest.is_empty() || name.ends_with('~') {
        return false;
    }
    let last_segment = rest.rsplit('.').next().unwrap_or(rest);
    !DOTENV_NON_LOADED_SUFFIXES.contains(&last_segment)
}

/// Whether `path` is a project-root dotenv file the loader reads (see
/// [`is_dotenv_loader_filename`]).
///
/// Scoped to root-level files: any dotenv-named file living under a dotted
/// directory (e.g. `.git/.env`) or `target/` is rejected, mirroring
/// [`should_ignore_path`] so only the genuine project-root `.env*` files are
/// un-ignored.
fn is_watched_dotenv_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !is_dotenv_loader_filename(name) {
        return false;
    }
    path.parent().is_none_or(|parent| {
        parent.components().all(|component| {
            if let std::path::Component::Normal(part) = component {
                let part = part.to_string_lossy();
                part != "target" && !part.starts_with('.')
            } else {
                true
            }
        })
    })
}

fn should_ignore_path(path: &Path) -> bool {
    if path.ends_with(Path::new("static").join("css").join("autumn.css")) {
        return true;
    }

    if path.ends_with(
        Path::new("target")
            .join("autumn")
            .join(DEV_RELOAD_STATE_FILE),
    ) {
        return true;
    }

    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            let name = name.to_string_lossy();
            if name == "target" || name.starts_with('.') {
                return true;
            }
        }
    }

    false
}

fn has_component(path: &Path, target: &str) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name) if name == std::ffi::OsStr::new(target)
        )
    })
}

fn is_profile_config_file(file_name: &str) -> bool {
    file_name.starts_with("autumn-")
        && Path::new(file_name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
        && file_name.len() > "autumn-.toml".len()
}

const fn describe_plan(plan: ChangePlan) -> &'static str {
    match plan {
        ChangePlan {
            build: true,
            restart: true,
            ..
        } => "cargo build + restart + full reload",
        ChangePlan {
            restart: true,
            tailwind: true,
            ..
        } => "Tailwind rebuild + restart + full reload",
        ChangePlan { restart: true, .. } => "restart + full reload",
        ChangePlan {
            tailwind: true,
            reload: ReloadKind::Css,
            ..
        } => "Tailwind rebuild + CSS reload",
        ChangePlan {
            reload: ReloadKind::Full,
            ..
        } => "browser full reload",
        _ => "no-op",
    }
}

fn restart_server(
    package: Option<&str>,
    child: &mut Option<Child>,
    reload_state_path: Option<&Path>,
    show_config: bool,
) -> bool {
    let binary = find_binary(package, false);
    *child = start_server(&binary, reload_state_path, show_config);
    child.is_some()
}

fn tailwind_build() -> bool {
    let Some(mut cmd) = build_tailwind_command() else {
        eprintln!(
            "  \u{2717} Tailwind CSS CLI not found. Run `autumn setup` or install `tailwindcss`."
        );
        return false;
    };

    eprintln!("  Rebuilding Tailwind...");
    match cmd.status() {
        Ok(status) if status.success() => {
            eprintln!("  \u{2713} Tailwind rebuild succeeded");
            true
        }
        Ok(_) => {
            eprintln!("  \u{2717} Tailwind rebuild failed");
            false
        }
        Err(error) => {
            eprintln!("  \u{2717} Failed to run Tailwind CLI: {error}");
            false
        }
    }
}

fn build_tailwind_command() -> Option<Command> {
    let tailwind = find_tailwind_cli()?;
    Some(build_tailwind_command_for(&tailwind))
}

fn build_tailwind_command_for(tailwind: &Path) -> Command {
    let mut cmd = Command::new(tailwind);
    cmd.args([
        "-i",
        "static/css/input.css",
        "-o",
        "static/css/autumn.css",
        "--content",
        "src/**/*.rs",
        "--minify",
    ]);
    cmd
}

fn find_tailwind_cli() -> Option<PathBuf> {
    let local = resolve_target_directory().ok().map(|dir| {
        dir.join("autumn").join(if cfg!(windows) {
            "tailwindcss.exe"
        } else {
            "tailwindcss"
        })
    });

    if let Some(local) = local.filter(|path| path.exists()) {
        return Some(local);
    }

    which("tailwindcss")
}

fn which(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.exists() {
            return Some(candidate);
        }
        #[cfg(target_os = "windows")]
        {
            let candidate_exe = dir.join(format!("{binary}.exe"));
            if candidate_exe.exists() {
                return Some(candidate_exe);
            }
        }
    }
    None
}

fn resolve_dev_reload_state_path() -> Result<PathBuf, String> {
    Ok(resolve_target_directory()?
        .join("autumn")
        .join(DEV_RELOAD_STATE_FILE))
}

fn resolve_target_directory() -> Result<PathBuf, String> {
    let metadata = cargo_metadata();
    metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "missing target_directory in cargo metadata".to_owned())
}

fn cargo_metadata() -> serde_json::Value {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to run cargo metadata");

    if !output.status.success() {
        eprintln!("\u{2717} Failed to read cargo metadata");
        std::process::exit(1);
    }

    serde_json::from_slice(&output.stdout).expect("parse cargo metadata")
}

/// Best-effort `cargo metadata`: returns `None` instead of exiting when the
/// workspace manifests are missing/invalid. Used by lifecycle paths (e.g.
/// `autumn serve stop`) that must keep working even with a broken `Cargo.toml`.
fn try_cargo_metadata() -> Option<serde_json::Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Directory containing a workspace member's `Cargo.toml`, resolved via
/// `cargo metadata`.
///
/// `autumn serve -p <member>` launched from a workspace root would otherwise run
/// the app with the workspace-root CWD, so the app's config loader (which falls
/// back to CWD when `AUTUMN_MANIFEST_DIR` is unset) skips the member's
/// `autumn.toml`/profile and asset dirs. Callers use this to point the child at
/// the member's directory. Best-effort: returns `None` if metadata can't be read
/// or the package isn't found, so lifecycle commands never fail solely because
/// `cargo metadata` does.
#[must_use]
pub fn find_manifest_dir(package: &str) -> Option<PathBuf> {
    let metadata = try_cargo_metadata()?;
    metadata["packages"].as_array()?.iter().find_map(|pkg| {
        if pkg["name"].as_str() == Some(package) {
            Path::new(pkg["manifest_path"].as_str()?)
                .parent()
                .map(Path::to_path_buf)
        } else {
            None
        }
    })
}

/// Resolve a binary path from parsed cargo metadata JSON.
///
/// Extracted from `find_binary` for testability. Takes the parsed
/// `cargo metadata` output and returns the path to the debug binary.
fn resolve_binary_from_metadata(
    metadata: &serde_json::Value,
    package: Option<&str>,
    cwd: &Path,
) -> Result<PathBuf, String> {
    let target_dir = metadata["target_directory"]
        .as_str()
        .ok_or("missing target_directory in metadata")?;

    let packages = metadata["packages"]
        .as_array()
        .ok_or("missing packages array in metadata")?;

    let matching_packages: Vec<_> = package.map_or_else(
        || {
            packages
                .iter()
                .filter(|pkg| {
                    let manifest = pkg["manifest_path"].as_str().unwrap_or("");
                    Path::new(manifest)
                        .parent()
                        .is_some_and(|dir| dir.starts_with(cwd))
                })
                .collect()
        },
        |pkg_name| {
            packages
                .iter()
                .filter(|pkg| pkg["name"].as_str() == Some(pkg_name))
                .collect()
        },
    );

    let bin_name = matching_packages
        .iter()
        .find_map(|pkg| {
            // Prefer `default-run` so packages with multiple binaries (e.g. a
            // `seed` binary alongside the main server) always start the right one.
            if let Some(name) = pkg["default_run"].as_str() {
                return Some(name.to_owned());
            }
            pkg["targets"].as_array()?.iter().find_map(|t| {
                let is_bin = t["kind"].as_array()?.iter().any(|k| k == "bin");
                if is_bin {
                    t["name"].as_str().map(String::from)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| {
            package.map_or_else(
                || "no binary target found in current package".to_owned(),
                |pkg_name| format!("no binary target found in package '{pkg_name}'"),
            )
        })?;

    let mut path = PathBuf::from(target_dir);
    path.push("debug");
    path.push(&bin_name);

    if cfg!(target_os = "windows") {
        path.set_extension("exe");
    }

    Ok(path)
}

/// Locate the compiled binary using `cargo metadata`.
///
/// Resolves the debug-profile path; when `release` is set, swaps the profile
/// directory to `release` (used by `autumn serve` for production builds).
pub fn find_binary(package: Option<&str>, release: bool) -> PathBuf {
    let metadata = cargo_metadata();

    let cwd = std::env::current_dir().expect("current dir");

    let path = resolve_binary_from_metadata(&metadata, package, &cwd).unwrap_or_else(|e| {
        eprintln!("\u{2717} {e}");
        if package.is_none() {
            eprintln!("  Hint: use -p <package> to specify the target package");
        }
        std::process::exit(1);
    });

    if release {
        // `.../<target>/debug/<bin>` -> `.../<target>/release/<bin>`.
        if let (Some(target_dir), Some(bin)) =
            (path.parent().and_then(Path::parent), path.file_name())
        {
            return target_dir.join("release").join(bin);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_relevant_change tests ───────────────────────────────────

    #[test]
    fn relevant_rust_file() {
        assert!(is_relevant_change(
            Path::new("src/main.rs"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_toml_config() {
        assert!(is_relevant_change(
            Path::new("autumn.toml"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_cargo_toml() {
        assert!(is_relevant_change(
            Path::new("Cargo.toml"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_css_file() {
        assert!(is_relevant_change(
            Path::new("static/css/style.css"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_generated_tailwind_output() {
        assert!(!is_relevant_change(
            Path::new("static/css/autumn.css"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn build_rs_change_requires_build_restart() {
        assert_eq!(
            classify_change(Path::new("build.rs"), DebouncedEventKind::Any, &[]),
            ChangeEffect::BuildRestart
        );
    }

    #[test]
    fn profile_config_change_requires_restart_only() {
        assert_eq!(
            classify_change(Path::new("autumn-dev.toml"), DebouncedEventKind::Any, &[]),
            ChangeEffect::RestartOnly
        );
    }

    #[test]
    fn dotenv_change_requires_restart_only() {
        for name in [".env", ".env.local", ".env.dev", ".env.production.local"] {
            assert_eq!(
                classify_change(Path::new(name), DebouncedEventKind::Any, &[]),
                ChangeEffect::RestartOnly,
                "{name} should restart the dev server",
            );
        }
    }

    #[test]
    fn dotenv_editor_temp_files_are_ignored() {
        // Editor swap/backup files must not trigger spurious restarts.
        for name in [
            ".env.swp",
            ".env.swo",
            ".env~",
            ".env.local~",
            ".env.tmp",
            ".env.bak",
        ] {
            assert_eq!(
                classify_change(Path::new(name), DebouncedEventKind::Any, &[]),
                ChangeEffect::Ignore,
                "{name} should not restart the dev server",
            );
        }
    }

    #[test]
    fn dotenv_change_ignored_for_non_any_event() {
        // A dotenv edit still only restarts on a settled `Any` event.
        assert_eq!(
            classify_change(Path::new(".env"), DebouncedEventKind::AnyContinuous, &[]),
            ChangeEffect::Ignore
        );
    }

    #[test]
    fn nested_dotenv_under_hidden_dir_is_ignored() {
        // Only root-level dotenv files are un-ignored; `.git/.env` stays ignored.
        assert_eq!(
            classify_change(Path::new(".git/.env"), DebouncedEventKind::Any, &[]),
            ChangeEffect::Ignore
        );
    }

    #[test]
    fn is_watched_dotenv_file_classifies_variants() {
        assert!(is_watched_dotenv_file(Path::new(".env")));
        assert!(is_watched_dotenv_file(Path::new(".env.local")));
        assert!(is_watched_dotenv_file(Path::new(".env.dev")));
        assert!(is_watched_dotenv_file(Path::new(".env.dev.local")));
        // Root-level file behind an absolute project path still matches.
        assert!(is_watched_dotenv_file(Path::new("/home/alice/app/.env")));
        // Non-loaded / decorated names do not.
        assert!(!is_watched_dotenv_file(Path::new(".env.swp")));
        assert!(!is_watched_dotenv_file(Path::new(".env.example")));
        assert!(!is_watched_dotenv_file(Path::new("env")));
        // A dotenv-named file under a dotted directory is not root-level.
        assert!(!is_watched_dotenv_file(Path::new(".git/.env")));
    }

    #[test]
    fn css_input_change_runs_tailwind_without_build() {
        let events = [notify_debouncer_mini::DebouncedEvent {
            path: PathBuf::from("static/css/input.css"),
            kind: DebouncedEventKind::Any,
        }];
        let plan = plan_changes(&events, &[]);
        assert_eq!(
            plan,
            ChangePlan {
                build: false,
                restart: false,
                tailwind: true,
                reload: ReloadKind::Css,
            }
        );
    }

    #[test]
    fn static_asset_change_triggers_browser_reload_only() {
        let events = [notify_debouncer_mini::DebouncedEvent {
            path: PathBuf::from("static/images/logo.png"),
            kind: DebouncedEventKind::Any,
        }];
        let plan = plan_changes(&events, &[]);
        assert_eq!(
            plan,
            ChangePlan {
                build: false,
                restart: false,
                tailwind: false,
                reload: ReloadKind::Full,
            }
        );
    }

    #[test]
    fn mixed_config_and_css_changes_restart_and_rebuild_css() {
        let events = [
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("autumn-dev.toml"),
                kind: DebouncedEventKind::Any,
            },
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("static/css/input.css"),
                kind: DebouncedEventKind::Any,
            },
        ];
        let plan = plan_changes(&events, &[]);
        assert_eq!(
            plan,
            ChangePlan {
                build: false,
                restart: true,
                tailwind: true,
                reload: ReloadKind::Full,
            }
        );
    }

    #[test]
    fn build_restart_overrides_tailwind_only_changes() {
        let events = [
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("src/main.rs"),
                kind: DebouncedEventKind::Any,
            },
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("static/css/input.css"),
                kind: DebouncedEventKind::Any,
            },
        ];
        let plan = plan_changes(&events, &[]);
        assert_eq!(
            plan,
            ChangePlan {
                build: true,
                restart: true,
                tailwind: false,
                reload: ReloadKind::Full,
            }
        );
    }

    #[test]
    fn ignores_generated_dev_reload_state_file() {
        assert_eq!(
            classify_change(
                Path::new("target/autumn/live-reload.json"),
                DebouncedEventKind::Any,
                &[],
            ),
            ChangeEffect::Ignore
        );
    }

    #[test]
    fn relevant_html_file() {
        assert!(is_relevant_change(
            Path::new("templates/index.html"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_sql_migration() {
        assert!(is_relevant_change(
            Path::new("migrations/001_init.sql"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_js_file() {
        assert!(is_relevant_change(
            Path::new("static/js/app.js"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_nested_rust_file() {
        assert!(is_relevant_change(
            Path::new("src/routes/api/handlers.rs"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_target_directory() {
        assert!(!is_relevant_change(
            Path::new("target/debug/build/main.rs"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_hidden_files() {
        assert!(!is_relevant_change(
            Path::new(".git/config"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_hidden_directory_nested() {
        assert!(!is_relevant_change(
            Path::new("src/.hidden/module.rs"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_irrelevant_extensions() {
        assert!(!is_relevant_change(
            Path::new("src/notes.txt"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_non_any_events() {
        assert!(!is_relevant_change(
            Path::new("src/main.rs"),
            DebouncedEventKind::AnyContinuous,
            &[],
        ));
    }

    #[test]
    fn cargo_lock_triggers_rebuild() {
        assert!(is_relevant_change(
            Path::new("Cargo.lock"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_file_without_extension() {
        assert!(!is_relevant_change(
            Path::new("src/Makefile"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn relevant_image_files_trigger_browser_reload() {
        assert!(is_relevant_change(
            Path::new("static/logo.png"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    #[test]
    fn ignores_target_nested_deeply() {
        assert!(!is_relevant_change(
            Path::new("target/release/deps/libfoo.rs"),
            DebouncedEventKind::Any,
            &[],
        ));
    }

    // ── collect_relevant_changes tests ─────────────────────────────

    #[test]
    fn collect_changes_filters_irrelevant() {
        let events = vec![
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("src/main.rs"),
                kind: DebouncedEventKind::Any,
            },
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("README.md"),
                kind: DebouncedEventKind::Any,
            },
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("src/lib.rs"),
                kind: DebouncedEventKind::Any,
            },
        ];
        let changed = collect_relevant_changes(&events, &[]);
        assert_eq!(changed.len(), 2);
        assert!(changed.iter().any(|c| c.contains("main.rs")));
        assert!(changed.iter().any(|c| c.contains("lib.rs")));
    }

    #[test]
    fn collect_changes_returns_empty_for_no_relevant() {
        let events = vec![
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("README.md"),
                kind: DebouncedEventKind::Any,
            },
            notify_debouncer_mini::DebouncedEvent {
                path: PathBuf::from("target/debug/app"),
                kind: DebouncedEventKind::Any,
            },
        ];
        let changed = collect_relevant_changes(&events, &[]);
        assert!(changed.is_empty());
    }

    #[test]
    fn collect_changes_handles_empty_events() {
        let changed = collect_relevant_changes(&[], &[]);
        assert!(changed.is_empty());
    }

    // ── build_cargo_command tests ──────────────────────────────────

    #[test]
    fn build_command_without_package() {
        let cmd = build_cargo_command(None, false);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(cmd.get_program(), "cargo");
        assert_eq!(args, &["build"]);
    }

    #[test]
    fn build_command_with_package() {
        let cmd = build_cargo_command(Some("my-app"), false);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(cmd.get_program(), "cargo");
        assert_eq!(args, &["build", "-p", "my-app"]);
    }

    #[test]
    fn build_command_release_adds_flag() {
        let cmd = build_cargo_command(None, true);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["build", "--release"]);
    }

    #[test]
    fn build_tailwind_command_for_sets_expected_args() {
        let cmd = build_tailwind_command_for(Path::new("tailwindcss"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(cmd.get_program(), "tailwindcss");
        assert_eq!(
            args,
            &[
                "-i",
                "static/css/input.css",
                "-o",
                "static/css/autumn.css",
                "--content",
                "src/**/*.rs",
                "--minify",
            ]
        );
    }

    // ── start_server tests ─────────────────────────────────────────

    #[test]
    fn start_server_returns_none_for_missing_binary() {
        let result = start_server(Path::new("/nonexistent/binary/path"), None, false);
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn start_server_returns_child_for_valid_binary() {
        let child = start_server(Path::new("/bin/sleep"), None, false);
        assert!(child.is_some());
        // Clean up
        let mut child = child.unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    // ── resolve_binary_from_metadata tests ─────────────────────────

    /// Build the expected binary path, accounting for `.exe` on Windows.
    fn expected_binary(path: &str) -> PathBuf {
        let mut p = PathBuf::from(path);
        if cfg!(target_os = "windows") {
            p.set_extension("exe");
        }
        p
    }

    fn sample_metadata(target_dir: &str, pkg_name: &str, manifest_dir: &str) -> serde_json::Value {
        serde_json::json!({
            "target_directory": target_dir,
            "packages": [{
                "name": pkg_name,
                "manifest_path": format!("{manifest_dir}/Cargo.toml"),
                "targets": [{
                    "name": pkg_name,
                    "kind": ["bin"],
                    "src_path": format!("{manifest_dir}/src/main.rs")
                }]
            }]
        })
    }

    #[test]
    fn resolve_binary_by_package_name() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let result =
            resolve_binary_from_metadata(&metadata, Some("hello"), Path::new("/projects/hello"));
        assert!(result.is_ok());
        let path = result.unwrap();
        assert_eq!(path, expected_binary("/tmp/target/debug/hello"));
    }

    #[test]
    fn resolve_binary_by_cwd() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/projects/hello"));
        assert!(result.is_ok());
        let path = result.unwrap();
        assert_eq!(path, expected_binary("/tmp/target/debug/hello"));
    }

    #[test]
    fn resolve_binary_package_not_found() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let result = resolve_binary_from_metadata(
            &metadata,
            Some("nonexistent"),
            Path::new("/projects/hello"),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("no binary target found in package 'nonexistent'")
        );
    }

    #[test]
    fn resolve_binary_no_match_by_cwd() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/other/directory"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("no binary target found in current package")
        );
    }

    #[test]
    fn resolve_binary_missing_target_directory() {
        let metadata = serde_json::json!({"packages": []});
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("target_directory"));
    }

    #[test]
    fn resolve_binary_missing_packages() {
        let metadata = serde_json::json!({"target_directory": "/tmp/target"});
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("packages"));
    }

    #[test]
    fn resolve_binary_skips_lib_targets() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "mylib",
                "manifest_path": "/projects/mylib/Cargo.toml",
                "targets": [{
                    "name": "mylib",
                    "kind": ["lib"],
                    "src_path": "/projects/mylib/src/lib.rs"
                }]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, Some("mylib"), Path::new("/projects/mylib"));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_binary_picks_first_bin_in_multi_target() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "multi",
                "manifest_path": "/projects/multi/Cargo.toml",
                "targets": [
                    {"name": "multi", "kind": ["lib"]},
                    {"name": "server", "kind": ["bin"]},
                    {"name": "cli", "kind": ["bin"]}
                ]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, Some("multi"), Path::new("/projects/multi"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_binary("/tmp/target/debug/server"));
    }

    // Regression: packages with multiple binaries (e.g. `seed` + main server)
    // must start the `default-run` binary, not whichever happens to be listed
    // first in `cargo metadata` targets.
    #[test]
    fn resolve_binary_prefers_default_run_over_first_target() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "todo-app",
                "manifest_path": "/projects/todo-app/Cargo.toml",
                "default_run": "todo-app",
                "targets": [
                    {"name": "seed",     "kind": ["bin"]},
                    {"name": "todo-app", "kind": ["bin"]}
                ]
            }]
        });
        let result = resolve_binary_from_metadata(
            &metadata,
            Some("todo-app"),
            Path::new("/projects/todo-app"),
        );
        assert!(result.is_ok());
        // Must return `todo-app`, not `seed` (which appears first in targets).
        assert_eq!(
            result.unwrap(),
            expected_binary("/tmp/target/debug/todo-app")
        );
    }

    #[test]
    fn resolve_binary_with_multiple_packages() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [
                {
                    "name": "app-a",
                    "manifest_path": "/projects/a/Cargo.toml",
                    "targets": [{"name": "app-a", "kind": ["bin"]}]
                },
                {
                    "name": "app-b",
                    "manifest_path": "/projects/b/Cargo.toml",
                    "targets": [{"name": "app-b", "kind": ["bin"]}]
                }
            ]
        });
        let result = resolve_binary_from_metadata(&metadata, Some("app-b"), Path::new("/projects"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_binary("/tmp/target/debug/app-b"));
    }

    // ── stop_server tests ──────────────────────────────────────────

    #[test]
    fn stop_server_with_none_is_noop() {
        let mut child: Option<Child> = None;
        stop_server(&mut child);
        assert!(child.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn stop_server_terminates_child() {
        // Spawn a long-running process, then stop it
        let proc = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let mut child = Some(proc);
        stop_server(&mut child);
        assert!(child.is_none());
    }

    // ── find_binary tests ──────────────────────────────────────────

    #[test]
    fn find_binary_resolves_workspace_package() {
        // We're running inside the autumn workspace, so this should find
        // the hello example's binary.
        let path = find_binary(Some("hello"), false);
        assert!(path.ends_with("debug/hello") || path.ends_with("debug/hello.exe"));
    }

    #[test]
    fn find_binary_release_resolves_release_dir() {
        let path = find_binary(Some("hello"), true);
        assert!(path.ends_with("release/hello") || path.ends_with("release/hello.exe"));
    }

    // ── constants tests ────────────────────────────────────────────

    #[test]
    fn debounce_interval_is_reasonable() {
        const { assert!(DEBOUNCE_MS >= 100, "debounce too short, would thrash") };
        const { assert!(DEBOUNCE_MS <= 5000, "debounce too long, sluggish UX") };
    }

    #[test]
    fn watch_dirs_are_non_empty() {
        for dir in DEFAULT_WATCH_DIRS {
            assert!(!dir.is_empty());
        }
    }

    #[test]
    fn watch_files_are_non_empty() {
        for f in WATCH_FILES {
            assert!(!f.is_empty());
        }
    }

    #[test]
    fn dev_reload_state_signal_writes_css_and_full_versions() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState { path, version: 0 };

        state.signal(ReloadKind::Css).expect("css signal");
        let body = std::fs::read_to_string(state.path()).expect("read css");
        assert_eq!(body, r#"{"kind":"css","version":1}"#);

        state.signal(ReloadKind::Full).expect("full signal");
        let body = std::fs::read_to_string(state.path()).expect("read full");
        assert_eq!(body, r#"{"kind":"full","version":2}"#);
    }

    #[test]
    fn dev_reload_state_signal_none_is_noop() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState { path, version: 41 };

        state.signal(ReloadKind::None).expect("noop signal");
        assert_eq!(state.version, 41);
        assert!(
            std::fs::read_to_string(state.path())
                .unwrap_or_default()
                .is_empty(),
            "noop signal should not write a new state file"
        );
    }

    #[test]
    fn dev_reload_state_signal_rejects_overflow() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState {
            path,
            version: u64::MAX,
        };

        let error = state
            .signal(ReloadKind::Full)
            .expect_err("overflow should fail");
        assert!(error.contains("overflowed"));
    }

    #[test]
    fn profile_config_file_requires_named_toml_suffix() {
        assert!(is_profile_config_file("autumn-dev.toml"));
        assert!(is_profile_config_file("autumn-local.TOML"));
        assert!(!is_profile_config_file("autumn-.toml"));
        assert!(!is_profile_config_file("autumn-dev.txt"));
        assert!(!is_profile_config_file("config.toml"));
    }

    #[test]
    fn has_component_matches_exact_path_components() {
        assert!(has_component(Path::new("src/routes/main.rs"), "src"));
        assert!(has_component(
            Path::new("templates/pages/index.html"),
            "templates"
        ));
        assert!(!has_component(Path::new("srcs/routes/main.rs"), "src"));
        assert!(!has_component(
            Path::new("template/index.html"),
            "templates"
        ));
    }

    #[test]
    fn describe_plan_covers_each_user_visible_action() {
        assert_eq!(
            describe_plan(ChangePlan {
                build: true,
                restart: true,
                tailwind: false,
                reload: ReloadKind::Full,
            }),
            "cargo build + restart + full reload"
        );
        assert_eq!(
            describe_plan(ChangePlan {
                build: false,
                restart: true,
                tailwind: true,
                reload: ReloadKind::Full,
            }),
            "Tailwind rebuild + restart + full reload"
        );
        assert_eq!(
            describe_plan(ChangePlan {
                build: false,
                restart: true,
                tailwind: false,
                reload: ReloadKind::Full,
            }),
            "restart + full reload"
        );
        assert_eq!(
            describe_plan(ChangePlan {
                build: false,
                restart: false,
                tailwind: true,
                reload: ReloadKind::Css,
            }),
            "Tailwind rebuild + CSS reload"
        );
        assert_eq!(
            describe_plan(ChangePlan {
                build: false,
                restart: false,
                tailwind: false,
                reload: ReloadKind::Full,
            }),
            "browser full reload"
        );
        assert_eq!(describe_plan(ChangePlan::default()), "no-op");
    }

    #[test]
    fn resolve_target_directory_returns_workspace_target() {
        let target_dir = resolve_target_directory().expect("target directory");
        assert_eq!(
            target_dir.file_name().and_then(|name| name.to_str()),
            Some("target")
        );
    }

    #[test]
    fn resolve_dev_reload_state_path_uses_target_autumn_file() {
        let path = resolve_dev_reload_state_path().expect("reload state path");
        assert!(
            path.ends_with(
                Path::new("target")
                    .join("autumn")
                    .join(DEV_RELOAD_STATE_FILE)
            )
        );
    }

    #[test]
    fn cargo_metadata_includes_target_directory_and_packages() {
        let metadata = cargo_metadata();
        assert!(metadata["target_directory"].is_string());
        assert!(metadata["packages"].is_array());
    }

    #[test]
    fn which_finds_binary_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary_name = if cfg!(windows) {
            "mocktailwind.exe"
        } else {
            "mocktailwind"
        };
        let binary = dir.path().join(binary_name);
        std::fs::write(&binary, "echo tailwind").expect("write binary");
        let path = std::env::join_paths([dir.path()]).expect("join path");
        temp_env::with_vars([("PATH", Some(path.as_os_str()))], || {
            let found = which("mocktailwind").expect("binary on PATH");
            assert_eq!(found, binary);
        });
    }

    #[test]
    fn which_returns_none_when_binary_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = std::env::join_paths([dir.path()]).expect("join path");
        temp_env::with_vars([("PATH", Some(path.as_os_str()))], || {
            assert!(which("definitely-missing-binary").is_none());
        });
    }

    // ── DevConfig parsing ──────────────────────────────────────────

    #[test]
    fn parse_dev_config_returns_default_when_section_missing() {
        let config = parse_dev_config("[server]\nport = 3000\n").expect("parse");
        assert!(config.watch_dirs.is_empty());
    }

    #[test]
    fn parse_dev_config_reads_watch_dirs() {
        let config = parse_dev_config(
            r#"
[dev]
watch_dirs = ["views", "locales"]
"#,
        )
        .expect("parse");
        assert_eq!(config.watch_dirs, vec!["views", "locales"]);
    }

    #[test]
    fn parse_dev_config_treats_empty_dev_section_as_default() {
        let config = parse_dev_config("[dev]\n").expect("parse");
        assert!(config.watch_dirs.is_empty());
    }

    #[test]
    fn parse_dev_config_rejects_non_string_watch_dirs() {
        let result = parse_dev_config("[dev]\nwatch_dirs = [42]\n");
        assert!(result.is_err());
    }

    #[test]
    fn load_dev_config_returns_default_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        let config = load_dev_config(&path);
        assert!(config.watch_dirs.is_empty());
    }

    #[test]
    fn load_dev_config_reads_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("autumn.toml");
        std::fs::write(&path, "[dev]\nwatch_dirs = [\"views\", \"locales\"]\n")
            .expect("write toml");
        let config = load_dev_config(&path);
        assert_eq!(config.watch_dirs, vec!["views", "locales"]);
    }

    // ── sanitize_custom_watch_dirs ─────────────────────────────────

    #[test]
    fn sanitize_drops_default_dirs() {
        let dirs = sanitize_custom_watch_dirs(DevConfig {
            watch_dirs: vec!["src".into(), "static".into(), "views".into()],
        });
        assert_eq!(dirs, vec!["views"]);
    }

    #[test]
    fn sanitize_drops_blanks_and_dedupes() {
        let dirs = sanitize_custom_watch_dirs(DevConfig {
            watch_dirs: vec![
                "  ".into(),
                "views".into(),
                "views".into(),
                "  locales  ".into(),
                String::new(),
            ],
        });
        assert_eq!(dirs, vec!["views", "locales"]);
    }

    // ── classify_change with custom dirs ───────────────────────────

    /// Build a `CustomWatchDir` for tests. The absolute form is anchored
    /// at the synthetic project root `/repo`.
    fn test_dir(rel: &str) -> CustomWatchDir {
        CustomWatchDir {
            relative: PathBuf::from(rel),
            absolute: PathBuf::from("/repo").join(rel),
        }
    }

    #[test]
    fn custom_watch_dir_change_triggers_restart() {
        let custom = vec![test_dir("views")];
        assert_eq!(
            classify_change(
                Path::new("views/landing.html"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::RestartOnly,
        );
    }

    #[test]
    fn custom_watch_dir_nested_change_triggers_restart() {
        let custom = vec![test_dir("locales")];
        assert_eq!(
            classify_change(
                Path::new("locales/en/messages.json"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::RestartOnly,
        );
    }

    #[test]
    fn custom_watch_dir_does_not_override_known_dirs() {
        // A path under `src` keeps its BuildRestart effect even if the user
        // also lists a custom dir.
        let custom = vec![test_dir("views")];
        assert_eq!(
            classify_change(Path::new("src/main.rs"), DebouncedEventKind::Any, &custom),
            ChangeEffect::BuildRestart,
        );
    }

    #[test]
    fn custom_watch_dir_respects_target_ignore() {
        // Even if the user names a custom dir, paths under target/ remain ignored.
        let custom = vec![test_dir("views")];
        assert_eq!(
            classify_change(
                Path::new("target/views/cached.html"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::Ignore,
        );
    }

    #[test]
    fn unknown_path_with_no_custom_dirs_is_ignored() {
        // Sanity check: without custom dirs configured, a `views/` change
        // is not picked up — it's the user's responsibility to opt in.
        assert_eq!(
            classify_change(
                Path::new("views/landing.html"),
                DebouncedEventKind::Any,
                &[],
            ),
            ChangeEffect::Ignore,
        );
    }

    #[test]
    fn custom_dir_change_plans_a_restart() {
        let events = [notify_debouncer_mini::DebouncedEvent {
            path: PathBuf::from("views/landing.html"),
            kind: DebouncedEventKind::Any,
        }];
        let custom = vec![test_dir("views")];
        let plan = plan_changes(&events, &custom);
        assert_eq!(
            plan,
            ChangePlan {
                build: false,
                restart: true,
                tailwind: false,
                reload: ReloadKind::Full,
            }
        );
    }

    #[test]
    fn custom_watch_dir_with_multi_segment_path_matches() {
        // Multi-segment dir matches both components.
        let custom = vec![test_dir("content/locales")];
        assert_eq!(
            classify_change(
                Path::new("content/locales/en/messages.json"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::RestartOnly,
        );
    }

    #[test]
    fn custom_watch_dir_does_not_match_unrelated_prefix() {
        // `views2/foo.html` must not be picked up by a `views` entry —
        // matching is component-wise, not byte-prefix.
        let custom = vec![test_dir("views")];
        assert_eq!(
            classify_change(
                Path::new("views2/foo.html"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::Ignore,
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_watch_dir_matches_absolute_event_path() {
        // `notify` backends typically dispatch absolute event paths, so
        // matching must work against the resolved absolute form.
        let custom = vec![CustomWatchDir {
            relative: PathBuf::from("views"),
            absolute: PathBuf::from("/home/user/project/views"),
        }];
        assert_eq!(
            classify_change(
                Path::new("/home/user/project/views/landing.html"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::RestartOnly,
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_watch_dir_matches_absolute_multi_segment_event_path() {
        let custom = vec![CustomWatchDir {
            relative: PathBuf::from("content/locales"),
            absolute: PathBuf::from("/home/user/project/content/locales"),
        }];
        assert_eq!(
            classify_change(
                Path::new("/home/user/project/content/locales/en/messages.json"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::RestartOnly,
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_watch_dir_does_not_match_ancestor_directory() {
        // Regression: matching is anchored to the project root via the
        // resolved absolute path. If the project itself lives under
        // `/home/alice/views/app`, a custom entry `views` must NOT match
        // the root-level `README.md` event, even though `views` appears
        // in the parent path.
        let custom = vec![CustomWatchDir {
            relative: PathBuf::from("views"),
            absolute: PathBuf::from("/home/alice/views/app/views"),
        }];
        assert_eq!(
            classify_change(
                Path::new("/home/alice/views/app/README.md"),
                DebouncedEventKind::Any,
                &custom,
            ),
            ChangeEffect::Ignore,
        );
    }

    // ── CustomWatchDir::matches ────────────────────────────────────

    #[test]
    fn custom_watch_dir_matches_relative_event_path() {
        let dir = test_dir("views");
        assert!(dir.matches(Path::new("views/file.html")));
        assert!(dir.matches(Path::new("views")));
        assert!(!dir.matches(Path::new("other/file.html")));
    }

    #[cfg(unix)]
    #[test]
    fn custom_watch_dir_matches_absolute_event_path_via_helper() {
        let dir = CustomWatchDir {
            relative: PathBuf::from("views"),
            absolute: PathBuf::from("/repo/views"),
        };
        assert!(dir.matches(Path::new("/repo/views/file.html")));
        assert!(!dir.matches(Path::new("/elsewhere/views/file.html")));
    }

    // ── resolve_custom_watch_dirs ──────────────────────────────────

    #[test]
    fn resolve_skips_missing_dirs() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(cwd.path().join("views")).expect("mkdir views");
        let resolved =
            resolve_custom_watch_dirs(&["views".to_owned(), "missing".to_owned()], cwd.path());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].relative, PathBuf::from("views"));
        assert!(resolved[0].absolute.ends_with("views"));
        assert!(resolved[0].absolute.is_absolute());
    }

    // ── normalize_watch_dir ────────────────────────────────────────

    #[test]
    fn normalize_strips_curdir_prefix() {
        assert_eq!(normalize_watch_dir("./views"), Ok("views".to_owned()));
    }

    #[test]
    fn normalize_preserves_multi_segment_paths() {
        assert_eq!(
            normalize_watch_dir("content/locales"),
            Ok("content/locales".replace('/', std::path::MAIN_SEPARATOR_STR)),
        );
    }

    #[test]
    fn normalize_rejects_empty_input() {
        assert!(normalize_watch_dir("   ").is_err());
        assert!(normalize_watch_dir("").is_err());
    }

    #[test]
    fn normalize_rejects_parent_traversal() {
        assert!(normalize_watch_dir("../up").is_err());
        assert!(normalize_watch_dir("views/../etc").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn normalize_rejects_absolute_paths_unix() {
        assert!(normalize_watch_dir("/etc/passwd").is_err());
        assert!(normalize_watch_dir("/").is_err());
    }

    #[test]
    fn normalize_rejects_target_anywhere() {
        assert!(normalize_watch_dir("target").is_err());
        assert!(normalize_watch_dir("nested/target/cache").is_err());
    }

    #[test]
    fn normalize_rejects_dotted_components() {
        // Dotted directories like `.git` would still be visited by the
        // watcher even though `should_ignore_path` filters their events
        // later, flooding the debouncer. Reject them up front.
        assert!(normalize_watch_dir(".git").is_err());
        assert!(normalize_watch_dir(".cache").is_err());
        assert!(normalize_watch_dir("nested/.hidden").is_err());
        // `./views` (CurDir prefix) is still allowed — only Normal components
        // starting with `.` are rejected.
        assert!(normalize_watch_dir("./views").is_ok());
    }

    #[test]
    fn normalize_rejects_curdir_only() {
        // `.` alone has no Normal components, so the result would be empty.
        assert!(normalize_watch_dir(".").is_err());
        assert!(normalize_watch_dir("./").is_err());
    }

    #[test]
    fn sanitize_warns_and_skips_unsafe_entries() {
        let dirs = sanitize_custom_watch_dirs(DevConfig {
            watch_dirs: vec![
                "../escape".into(),
                "target".into(),
                "views".into(),
                "./locales".into(),
            ],
        });
        let expected_locales = "locales".to_owned();
        assert_eq!(dirs, vec!["views".to_owned(), expected_locales]);
    }
}
