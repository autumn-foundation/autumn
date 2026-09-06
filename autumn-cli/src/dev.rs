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
use serde::{Deserialize, Serialize};
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

/// Environment variable naming the cooperative-shutdown file, mirrored from
/// `autumn_web::app::SHUTDOWN_SIGNAL_FILE_ENV`.
///
/// Mirrored rather than imported at the definition site for symmetry with this
/// module's other wire constants; `dev_shutdown_env_var_matches_the_runtime_constant`
/// pins the two together so a rename on either side fails the build's tests
/// rather than silently breaking the Windows dev loop.
// The cooperative-stop machinery below is the NON-UNIX stop path (#1616), but it
// is compiled and unit-tested on every platform on purpose: Windows is the one
// platform this project's contributors and most of its CI never run
// interactively, so logic that only compiled there would be logic nobody ever
// exercised before a user did. `allow(dead_code)` on Unix is the price of that
// coverage — the alternative is `cfg(not(unix))` guards that hide the code from
// Linux CI entirely, which is how the orphaned-Postgres bug survived.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const DEV_SHUTDOWN_SIGNAL_ENV: &str = "AUTUMN_SHUTDOWN_SIGNAL_FILE";

/// Filename parts of the cooperative-shutdown file, written beside the
/// live-reload state. The dev-loop process id goes between them — see
/// [`resolve_dev_shutdown_signal_path`].
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const DEV_SHUTDOWN_SIGNAL_PREFIX: &str = "dev-shutdown.";
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const DEV_SHUTDOWN_SIGNAL_SUFFIX: &str = ".signal";

/// Mirror of [`crate::serve::SERVE_READY_FILE_ENV`]; pinned to it by a test.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const SERVE_READY_FILE_ENV: &str = "AUTUMN_SERVE_READY_FILE";

/// Filename parts of this dev session's ready file — the app writes its
/// resolved drain budget here, and the dev loop's process id keeps two
/// concurrent `autumn dev` runs from reading each other's.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const DEV_READY_FILE_PREFIX: &str = "dev-ready.";
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const DEV_READY_FILE_SUFFIX: &str = ".state";

/// Headroom added to the app's own drain budget before `autumn dev` escalates
/// to a hard kill.
///
/// `on_shutdown` hooks run **after** the drain completes, so this — not the
/// drain budget — is what has to cover them. Sized to the managed-Postgres stop
/// ceiling (`STARTUP_TIMEOUT` in `autumn_web::managed_pg`, 60s, which
/// `postgresql_embedded` applies to `pg_ctl stop` as well as start): that hook
/// is the one this whole mechanism exists to let run.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const COOPERATIVE_STOP_HOOK_HEADROOM: Duration = Duration::from_secs(60);

/// The cooperative-stop budget for an app whose configured graceful-drain
/// budget is `drain_secs` (`prestop_grace_secs + shutdown_timeout_secs`).
///
/// A fixed constant was wrong here. An external stop takes `DrainCause::Signal`,
/// which runs the app's full lifecycle — readiness flip, prestop grace, drain —
/// and only then its `on_shutdown` hooks. Under the prod defaults (grace 5 +
/// timeout 30) an app is still legitimately shutting down at t=35s, so a
/// ten-second kill would reintroduce the orphaned postmaster this change exists
/// to fix, on a perfectly valid configuration.
///
/// Saturating: `drain_secs` comes from user config (`[server]` keys or
/// `AUTUMN_SERVER__*`), and wrapping would produce a tiny budget that hard-kills
/// instantly — the failure mode, arrived at by arithmetic.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
const fn cooperative_stop_budget(drain_secs: u64) -> Duration {
    Duration::from_secs(drain_secs).saturating_add(COOPERATIVE_STOP_HOOK_HEADROOM)
}

/// The stop budget recorded when the currently-running child was spawned, in
/// seconds. `0` means "nothing recorded".
///
/// `autumn.toml` is a watched file, so the stop that follows an edit runs
/// against the NEW config while the child being stopped is still running under
/// the old one. Recording at spawn keeps the two in step; re-resolving at stop
/// would grade the outgoing child against config it never saw — and a half-saved
/// `autumn.toml` would collapse to the defaults, force-killing an app that is
/// still legitimately draining.
static CHILD_STOP_BUDGET_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record the budget for the child about to be spawned.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn record_child_stop_budget(budget: Duration) {
    CHILD_STOP_BUDGET_SECS.store(budget.as_secs(), Ordering::SeqCst);
}

/// The budget recorded at the last spawn, if any.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn recorded_child_stop_budget() -> Option<Duration> {
    match CHILD_STOP_BUDGET_SECS.load(Ordering::SeqCst) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// The budget to stop the running child with, most authoritative first.
///
/// 1. `reported` — what the app itself said its drain budget is. Beats anything
///    the CLI can derive: an app using `AppBuilder::with_config_loader` can
///    resolve a budget no amount of TOML/env reading reproduces.
/// 2. `recorded` — resolved from config when this child was spawned. Used
///    before the app has finished booting, and never re-read at stop time, so a
///    just-edited (or half-saved) config cannot shorten the window the running
///    child is actually using.
/// 3. `resolve_now` — last resort, when neither is available.
///
/// Pure so those precedence rules are testable directly, including the one that
/// matters most: with a budget already in hand, the config is not consulted.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn stop_budget_for_running_child(
    reported: Option<Duration>,
    recorded: Option<Duration>,
    resolve_now: impl FnOnce() -> Duration,
) -> Duration {
    reported.or(recorded).unwrap_or_else(resolve_now)
}

/// This app's cooperative-stop budget, resolved from its own configuration.
///
/// Delegates to the same profile-aware resolver `autumn serve stop` uses, so the
/// two stop paths cannot disagree about how long the app is entitled to take.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn resolve_cooperative_stop_budget(package: Option<&str>) -> Duration {
    let base_dir = package
        .and_then(find_manifest_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    // `autumn dev` is always a debug build, so the build-mode default is `dev`;
    // an explicit `AUTUMN_ENV`/`AUTUMN_PROFILE` still wins, which is exactly the
    // case (a prod-profile dev run) that the old fixed budget got wrong.
    let profile = crate::serve::effective_profile(None, false);
    // Resolve through the SAME `.env` overlay `start_server` injects into the
    // child. The CLI deliberately never mutates its own environment, so a budget
    // read from `std::env` alone would miss an `AUTUMN_SERVER__*` override the
    // child does honor — and the parent would then force-kill the app in the
    // middle of a valid shutdown, skipping exactly the managed-Postgres teardown
    // this mechanism exists to protect. Real shell variables still win over
    // `.env`, which is what `DotenvOsEnv` implements; a malformed `.env` falls
    // back to the plain environment rather than failing the stop.
    let denv: Box<dyn autumn_web::config::Env> = match autumn_web::dotenv::os_env_with_dotenv() {
        Ok(env) => Box::new(env),
        Err(_) => Box::new(autumn_web::config::OsEnv),
    };
    let (prestop, shutdown) =
        crate::serve::resolve_shutdown_budget_from(&base_dir, Some(&profile), &|key| {
            denv.var(key).ok().filter(|value| !value.trim().is_empty())
        });
    cooperative_stop_budget(prestop.saturating_add(shutdown))
}

/// How the dev loop's app child was stopped.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopOutcome {
    /// The app drained and exited on its own, so its `on_shutdown` hooks ran —
    /// including managed Postgres teardown.
    Graceful,
    /// The app did not exit within the budget and was force-killed. Its hooks
    /// may not have run; the caller reports this rather than hiding it.
    Escalated,
    /// The request could not be delivered at all (the signal file was not
    /// writable), so the app was force-killed without ever being asked. Kept
    /// distinct from [`Self::Escalated`] so the warning blames the dev setup
    /// rather than accusing the app of ignoring a request it never received.
    Unreachable,
    /// There was no child to stop.
    NotRunning,
}

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
    /// The build-error payload for the currently-broken Rust build, if any.
    ///
    /// When set, every ordinary [`Self::signal`] re-emits it so a non-build
    /// reload (CSS/Tailwind/static save routed through the watch loop) keeps
    /// the compile-error overlay up instead of dismissing it and reloading the
    /// still-broken stale app. Only a green Rust build clears it, via
    /// [`Self::signal_build_success`].
    active_build_error: Option<(Vec<BuildDiagnostic>, bool)>,
}

impl DevReloadState {
    fn initialize() -> Result<Self, String> {
        let path = resolve_dev_reload_state_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }

        let state = Self {
            path,
            version: 0,
            active_build_error: None,
        };
        state.write(ReloadKind::Full)?;
        Ok(state)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Signal an ordinary reload (CSS/Tailwind/full). Bumps the version and
    /// writes fresh state.
    ///
    /// If a Rust build is currently broken (`active_build_error` is `Some`),
    /// the build-error payload is CARRIED FORWARD into the new state so the
    /// overlay survives — a CSS/Tailwind/static save while the code doesn't
    /// compile must not dismiss the overlay and reload the stale app. Only a
    /// green Rust build ([`Self::signal_build_success`]) clears it.
    fn signal(&mut self, kind: ReloadKind) -> Result<(), String> {
        if kind == ReloadKind::None {
            return Ok(());
        }

        self.version = self
            .version
            .checked_add(1)
            .ok_or("live reload version overflowed")?;

        if let Some((diagnostics, stale)) = self.active_build_error.as_ref() {
            let (diagnostics, stale) = (diagnostics.clone(), *stale);
            self.write_build_error(&diagnostics, stale)
        } else {
            self.write(kind)
        }
    }

    /// Bump the version and write a full-reload state carrying compiler
    /// diagnostics so the browser client renders a compile-error overlay
    /// instead of reloading into a broken/stale page.
    ///
    /// The payload is also stashed in `active_build_error` so subsequent
    /// ordinary [`Self::signal`] calls carry it forward and keep the overlay
    /// up until a green build clears it.
    ///
    /// `stale` is true when a previously-built binary is still serving (the
    /// browser is looking at a now-outdated page); false on a cold start (or
    /// on Windows, where the old binary must be stopped before rebuilding)
    /// where no server is up yet.
    fn signal_build_error(
        &mut self,
        diagnostics: &[BuildDiagnostic],
        stale: bool,
    ) -> Result<(), String> {
        self.version = self
            .version
            .checked_add(1)
            .ok_or("live reload version overflowed")?;

        self.active_build_error = Some((diagnostics.to_vec(), stale));
        self.write_build_error(diagnostics, stale)
    }

    /// Clear a previously-signaled build error after a GREEN Rust build, then
    /// write ordinary reload state (with NO `build_error` field) so the client
    /// dismisses the overlay and reloads into the freshly-built app.
    ///
    /// This is the ONLY path that clears the overlay: an ordinary
    /// [`Self::signal`] carries an active build error forward instead.
    fn signal_build_success(&mut self, kind: ReloadKind) -> Result<(), String> {
        self.active_build_error = None;
        // A successful build always warrants a reload to pick up the new
        // binary, so treat `None` as a full reload rather than a no-op.
        let kind = if kind == ReloadKind::None {
            ReloadKind::Full
        } else {
            kind
        };

        self.version = self
            .version
            .checked_add(1)
            .ok_or("live reload version overflowed")?;
        self.write(kind)
    }

    /// Write full-reload state carrying a `build_error` payload.
    fn write_build_error(
        &self,
        diagnostics: &[BuildDiagnostic],
        stale: bool,
    ) -> Result<(), String> {
        let body = serde_json::json!({
            "version": self.version,
            "kind": "full",
            "build_error": {
                "diagnostics": diagnostics,
                "stale": stale,
            },
        });
        std::fs::write(&self.path, body.to_string())
            .map_err(|e| format!("failed to write {}: {e}", self.path.display()))
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

/// How long `autumn dev` keeps looking for a dependency verdict (issue #1633).
///
/// Nothing waits on the audit. It starts after the initial build — running it
/// beside the build makes its `cargo metadata` contend with Cargo's
/// package-cache lock and slow the build itself — and the watch loop then polls
/// for the result without blocking. A verdict that has not arrived by this
/// deadline is dropped and the dev loop says nothing.
const DEPENDENCY_AUDIT_DEADLINE: Duration = Duration::from_secs(30);

/// Startup lines for an evaluation that may not have finished in time.
fn dependency_startup_lines(eval: Option<&crate::deps::Evaluation>) -> Vec<String> {
    eval.map(crate::deps::dev_lines).unwrap_or_default()
}

/// A dependency evaluation running beside the dev loop.
///
/// Polled, never awaited: [`report`](Self::report) returns immediately whether
/// or not a verdict has arrived, so the watch loop never stalls on the auditor.
struct DependencyAudit {
    receiver: mpsc::Receiver<crate::deps::Evaluation>,
    deadline: std::time::Instant,
    done: bool,
}

impl DependencyAudit {
    fn start(root: &Path) -> Self {
        Self {
            receiver: crate::deps::spawn_evaluation(root),
            deadline: std::time::Instant::now() + DEPENDENCY_AUDIT_DEADLINE,
            done: false,
        }
    }

    /// Print the verdict once, if one has arrived. Never blocks.
    fn report(&mut self) {
        if self.done {
            return;
        }
        match self.receiver.try_recv() {
            Ok(evaluation) => {
                self.done = true;
                for line in dependency_startup_lines(Some(&evaluation)) {
                    eprintln!("{line}");
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => self.done = true,
            // Still running. Give up once the deadline passes so a wedged
            // auditor cannot keep this poll alive for the whole session.
            Err(mpsc::TryRecvError::Empty) => {
                self.done = std::time::Instant::now() >= self.deadline;
            }
        }
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
    // Initial build. There is no prior server here (cold start), so a browser
    // overlay isn't reachable — the CLI doesn't know the app's port and no
    // process is up to answer the state endpoint. We still record the failure
    // in the state file (harmless, and dismissed by the first green build);
    // the terminal errors remain the primary feedback for this case.
    let (built, diagnostics) = cargo_build_capturing(package);

    if !built {
        eprintln!("\u{2717} Initial build failed. Fix errors and save to retry.\n");
        if let Some(state) = reload_state.as_mut()
            && let Err(error) = state.signal_build_error(&diagnostics, false)
        {
            eprintln!("  Warning: live reload signal failed: {error}");
        }
    }

    let binary = find_binary(package, false);
    let mut child = start_server(
        &binary,
        package,
        reload_state.as_ref().map(DevReloadState::path),
        show_config,
    );

    // Dependency findings (issue #1633). Started once every synchronous step
    // that runs Cargo is done — the build, and `find_binary`'s own `cargo
    // metadata` — because cargo-deny runs `cargo metadata` too and the two
    // contend for Cargo's package-cache lock. From here it is only polled,
    // never awaited, so nothing delays startup or the rebuild loop. Quiet by
    // default: an advisory-clean, policy-clean tree adds nothing.
    let mut dependency_audit = DependencyAudit::start(Path::new("."));

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

        dependency_audit.report();

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

    stop_server(&mut child, package);
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
    if plan.build {
        // The order of "stop old binary" vs. "cargo build" is platform-gated.
        //
        // On Unix/macOS, replacing a running binary file mid-build is safe
        // (inode semantics), so we build FIRST with the old binary still
        // serving. A failed rebuild then leaves the stale app up to answer the
        // live-reload endpoint and render the diagnostics overlay.
        //
        // On Windows the running `target/debug/<app>.exe` is LOCKED while the
        // process is alive, so `cargo build` can't relink over it (access
        // denied / linker failure). There we must stop the old binary BEFORE
        // building. The tradeoff: a failed Windows rebuild leaves the app down,
        // so the overlay's stale-page serving isn't available and the client
        // falls back to a normal reconnect (documented limitation).
        let stop_before_build = cfg!(windows);

        if stop_before_build {
            stop_server(child, package);
        }

        let (built, diagnostics) = cargo_build_capturing(package);

        if built {
            if !stop_before_build {
                stop_server(child, package);
            }
            if restart_server(
                package,
                child,
                reload_state.as_ref().map(|s| s.path()),
                show_config,
            ) && let Some(reload_state) = reload_state.as_mut()
                && let Err(error) = reload_state.signal_build_success(ReloadKind::Full)
            {
                // A green build is the only thing that clears the overlay.
                eprintln!("  Warning: live reload signal failed: {error}");
            }
        } else {
            eprintln!("  \u{2717} Build failed. Waiting for changes...\n");
            // On Unix the (stale) binary is still running, so it keeps
            // answering the live-reload state endpoint and the browser renders
            // the overlay. On Windows we already stopped it above, so nothing
            // is serving: `child.is_some()` is false there and the client
            // falls back to a normal reconnect.
            let stale = child.is_some();
            if let Some(reload_state) = reload_state.as_mut()
                && let Err(error) = reload_state.signal_build_error(&diagnostics, stale)
            {
                eprintln!("  Warning: live reload signal failed: {error}");
            }
        }
        return;
    }

    let mut applied_reload = ReloadKind::None;

    if plan.tailwind && tailwind_build() {
        applied_reload = applied_reload.max(ReloadKind::Css);
    }

    if plan.restart {
        stop_server(child, package);
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

    // Ordinary (non-build) reload. If a Rust build is still broken,
    // `signal` carries the build-error overlay forward instead of dismissing
    // it, so this CSS/Tailwind/static change doesn't reload the stale app.
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

/// A single compiler error extracted from `cargo build --message-format=json`.
///
/// Serialized into the live-reload state file so the browser overlay can
/// render the failure without the CLI needing to talk to the app directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BuildDiagnostic {
    /// Error code (e.g. `E0425`), when the compiler assigned one.
    code: Option<String>,
    /// Primary human-readable message (`message.message`).
    message: String,
    /// File of the primary span, empty when the error has no primary span.
    file: String,
    /// 1-based line of the primary span (0 when absent).
    line: u32,
    /// 1-based column of the primary span (0 when absent).
    column: u32,
    /// Full multi-line rendered diagnostic (`message.rendered`).
    rendered: String,
}

/// Parse `cargo build --message-format=json` stdout into ordered error
/// diagnostics.
///
/// Keeps only `compiler-message` records at `error` level (warnings are out of
/// scope) and preserves the compiler's emission order. Non-JSON lines and other
/// message kinds (artifacts, build-finished) are skipped. Mirrors the JSON
/// walking pattern used by [`crate::dev_loop_bench::cargo_executable_path`].
fn parse_build_diagnostics(stdout: &[u8]) -> Vec<BuildDiagnostic> {
    let text = String::from_utf8_lossy(stdout);
    let mut diagnostics = Vec::new();

    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if message.get("level").and_then(serde_json::Value::as_str) != Some("error") {
            continue;
        }

        let code = message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let text_message = message
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let rendered = message
            .get("rendered")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let primary = message
            .get("spans")
            .and_then(serde_json::Value::as_array)
            .and_then(|spans| {
                spans.iter().find(|span| {
                    span.get("is_primary").and_then(serde_json::Value::as_bool) == Some(true)
                })
            });
        let (file, line, column) = primary.map_or_else(
            || (String::new(), 0, 0),
            |span| {
                let file = span
                    .get("file_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let line = span
                    .get("line_start")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0);
                let column = span
                    .get("column_start")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0);
                (file, line, column)
            },
        );

        diagnostics.push(BuildDiagnostic {
            code,
            message: text_message,
            file,
            line,
            column,
            rendered,
        });
    }

    diagnostics
}

/// Run `cargo build` for the given package while capturing compiler
/// diagnostics as JSON.
///
/// Returns `(success, error_diagnostics)`. Diagnostics are echoed to stderr via
/// their `rendered` form so the terminal experience matches a plain build.
/// Used by the watch loop so a failed rebuild can surface errors as a browser
/// overlay; `serve` keeps using [`cargo_build`].
fn cargo_build_capturing(package: Option<&str>) -> (bool, Vec<BuildDiagnostic>) {
    use std::io::{BufRead, BufReader};

    let mut cmd = build_cargo_command(package, false);
    cmd.arg("--message-format=json");
    // Pipe stdout so we can read structured JSON diagnostics line by line as the
    // compiler emits them, while cargo's own progress/status output (the live
    // "Compiling ..." lines) streams straight through on inherited stderr. This
    // keeps the terminal responsive instead of freezing until the build ends.
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    eprintln!("  Compiling...");
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("  \u{2717} Failed to run cargo build: {e}");
            return (false, Vec::new());
        }
    };

    // Accumulate raw stdout so `parse_build_diagnostics` can build the returned
    // Vec once at the end. Error `rendered` blocks are echoed live in compiler
    // order as their lines arrive, so each error appears exactly once and the
    // final parse never re-prints them.
    let mut buffer = String::new();
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else {
                // A read error mid-stream: stop consuming and fall back to
                // whatever we captured so far rather than panicking.
                break;
            };
            if let Some(rendered) = compiler_message_rendered(&line) {
                eprint!("{rendered}");
            }
            buffer.push_str(&line);
            buffer.push('\n');
        }
    }

    let diagnostics = parse_build_diagnostics(buffer.as_bytes());
    match child.wait() {
        Ok(status) if status.success() => {
            eprintln!("  \u{2713} Build succeeded");
            (true, diagnostics)
        }
        Ok(_) => (false, diagnostics),
        Err(e) => {
            eprintln!("  \u{2717} cargo build failed: {e}");
            (false, diagnostics)
        }
    }
}

/// If `line` is a single `cargo build --message-format=json` record that is a
/// `compiler-message` carrying a non-empty `rendered` block, return that text so
/// it can be echoed to the terminal the instant it arrives. This deliberately
/// echoes diagnostics at *every* level (errors, warnings, notes, help, etc.) so
/// the live output matches what plain `cargo build` would print — the overlay
/// payload, built separately by [`parse_build_diagnostics`], stays errors-only.
/// Non-JSON lines and non-`compiler-message` records (artifacts, build scripts,
/// build-finished) yield `None`, as do compiler messages with no `rendered`
/// text.
fn compiler_message_rendered(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
        return None;
    }
    let rendered = value
        .get("message")?
        .get("rendered")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if rendered.is_empty() {
        return None;
    }
    Some(rendered.to_owned())
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
    package: Option<&str>,
    reload_state_path: Option<&Path>,
    show_config: bool,
) -> Option<Child> {
    // Only the non-Unix stop path consults the recorded budget; on Unix the
    // parameter is carried for a uniform signature.
    #[cfg(unix)]
    let _ = package;
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
    // See `serve::base_command`: these one-shot modes are dispatched before the
    // server starts, so an inherited flag would turn every hot-reload restart
    // into a manifest dump that exits cleanly and never serves.
    command.env_remove(crate::data_flow::DUMP_ENV);
    command.env_remove(crate::agents::DUMP_ENV);
    command.env_remove(crate::graph::DUMP_ENV);
    // Same reasoning, worse outcome (#1605): `AUTUMN_DB_RETENTION=report|purge`
    // is dispatched before the server starts, so an inherited one would make
    // every hot-reload restart enforce the retention policy and exit -- deleting
    // data on each save rather than merely failing to serve.
    crate::db::retention::clear_inherited_one_shot_env(&mut command);
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
    // Hand the app the cooperative-shutdown file so a stop can run its
    // `on_shutdown` hooks on a platform with no `SIGTERM` (#1616). Clear any
    // stale request first: a file left by a crashed `autumn dev` would drain
    // this child the moment it boots. Only wired where it is the stop
    // mechanism — on Unix `stop_server` signals, and an unset variable keeps
    // the runtime's watcher inert.
    #[cfg(not(unix))]
    if let Some(signal_path) = dev_shutdown_signal_path() {
        clear_cooperative_shutdown(signal_path);
        command.env(DEV_SHUTDOWN_SIGNAL_ENV, signal_path);
        // Resolve the child's stop budget from the config it is about to boot
        // with, and keep it for the stop. `autumn.toml` is watched, so by the
        // time this child is stopped the file may say something different — and
        // the child would still be draining on these numbers.
        record_child_stop_budget(resolve_cooperative_stop_budget(package));
    }
    // Let the app tell us its own resolved drain budget rather than making the
    // CLI guess — the seam `autumn serve` already uses, and the only way to see
    // a budget a custom `with_config_loader` produced. Clear any stale file
    // first so this child cannot be stopped on its predecessor's number.
    #[cfg(not(unix))]
    if let Some(ready) = dev_ready_file_path() {
        let _ = std::fs::remove_file(ready);
        command.env(SERVE_READY_FILE_ENV, ready);
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
fn stop_server(child: &mut Option<Child>, package: Option<&str>) {
    #[cfg(unix)]
    {
        let _ = package;
        stop_server_unix(child);
    }
    #[cfg(not(unix))]
    stop_server_cooperative(child, package);
}

/// Unix stop: `SIGTERM`, then `SIGKILL` if the app misses its drain budget.
/// This is the mechanism production uses, so the dev loop uses it too.
#[cfg(unix)]
fn stop_server_unix(child: &mut Option<Child>) {
    if let Some(proc) = child {
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
    *child = None;
}

/// Non-Unix stop: there is no `SIGTERM` here, and `Child::kill` is
/// `TerminateProcess` — it skips the app's `on_shutdown` hooks, so a managed
/// Postgres child was orphaned on every rebuild (#1616). Ask the app to drain
/// through the file it watches, and force-kill only if it will not.
#[cfg(not(unix))]
fn stop_server_cooperative(child: &mut Option<Child>, package: Option<&str>) {
    let Some(signal_path) = dev_shutdown_signal_path() else {
        // `start_server` sets `AUTUMN_SHUTDOWN_SIGNAL_FILE` from this same
        // cached value, so with no path the child was never told to watch
        // anything. Waiting out the full budget for a request nobody can
        // receive would make every rebuild slower than the hard kill this
        // replaced — so kill directly, and say why.
        if let Some(proc) = child.as_mut() {
            let _ = proc.kill();
            let _ = proc.wait();
            eprintln!("{}", unreachable_warning("cargo metadata is unreadable"));
        }
        *child = None;
        return;
    };
    let budget = stop_budget_for_running_child(
        dev_ready_file_path().and_then(child_reported_stop_budget),
        recorded_child_stop_budget(),
        || resolve_cooperative_stop_budget(package),
    );
    match stop_child_cooperatively(child, signal_path, budget) {
        StopOutcome::Escalated => eprintln!("{}", escalation_warning(budget)),
        StopOutcome::Unreachable => eprintln!(
            "{}",
            unreachable_warning("the target directory is not writable")
        ),
        StopOutcome::Graceful | StopOutcome::NotRunning => {}
    }
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
    *child = start_server(&binary, package, reload_state_path, show_config);
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

/// Where the cooperative-shutdown file lives: beside the live-reload state,
/// inside the target directory, so it is swept by `cargo clean`.
///
/// Two properties matter and neither is incidental.
///
/// **Tolerant.** This resolves through [`try_cargo_metadata`], not
/// [`resolve_target_directory`], which `exit(1)`s on unreadable metadata.
/// `Cargo.toml` is a watched file, so a half-saved manifest is an ordinary
/// dev-loop event — and on Windows the stop happens *before* the rebuild. An
/// exit from inside the stop path would leave the app, and the managed Postgres
/// cluster it owns, running with nobody left to reap them: the exact orphan this
/// mechanism exists to prevent, through a new door.
///
/// **Unique per dev-loop process.** Two `autumn dev` instances in one workspace
/// (`-p api` and `-p admin`) share a target directory. A shared signal file
/// would let one instance's rebuild drain the other instance's app, and let one
/// instance's cleanup delete the request the other is still waiting on.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn resolve_dev_shutdown_signal_path() -> Option<PathBuf> {
    let metadata = try_cargo_metadata()?;
    let target = metadata["target_directory"].as_str()?;
    Some(Path::new(target).join("autumn").join(format!(
        "{DEV_SHUTDOWN_SIGNAL_PREFIX}{}{DEV_SHUTDOWN_SIGNAL_SUFFIX}",
        std::process::id()
    )))
}

/// This dev-loop session's ready file, resolved once — the path the app writes
/// its resolved drain budget to.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn dev_ready_file_path() -> Option<&'static Path> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let metadata = try_cargo_metadata()?;
        let target = metadata["target_directory"].as_str()?;
        Some(Path::new(target).join("autumn").join(format!(
            "{DEV_READY_FILE_PREFIX}{}{DEV_READY_FILE_SUFFIX}",
            std::process::id()
        )))
    })
    .as_deref()
}

/// The stop budget the running child reported for itself, if it has booted.
///
/// The app writes its resolved *drain* budget (`prestop_grace_secs +
/// shutdown_timeout_secs`) to this file at startup-complete; hooks run after the
/// drain, so the same headroom still applies. A missing, blank, or unparseable
/// file means "no report" — never a zero budget, which would hard-kill instantly.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn child_reported_stop_budget(ready_file: &Path) -> Option<Duration> {
    let secs = std::fs::read_to_string(ready_file)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(cooperative_stop_budget(secs))
}

/// This dev-loop session's cooperative-shutdown file, resolved once.
///
/// Cached so the stop path never shells out to `cargo metadata`: a stop must be
/// cheap, and must never fail for a reason unrelated to the child it is
/// stopping.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn dev_shutdown_signal_path() -> Option<&'static Path> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(resolve_dev_shutdown_signal_path)
        .as_deref()
}

/// Ask the running app to shut down gracefully by creating the file it watches.
///
/// The runtime keys off existence alone, so an empty file is the whole protocol.
///
/// # Errors
///
/// Returns the underlying IO error if the file (or its parent) cannot be created.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn request_cooperative_shutdown(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path).map(|_| ())
}

/// Remove a shutdown request so the next child does not drain the moment it
/// boots. Best-effort: a file that is already gone is the desired state.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn clear_cooperative_shutdown(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The warning printed when a cooperative stop had to escalate to a hard kill.
///
/// Names the consequence, not just the timeout: #1616's whole complaint is that
/// a skipped teardown was invisible.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn escalation_warning(timeout: Duration) -> String {
    format!(
        "  Warning: the app did not shut down within {}s and was force-stopped. \
         Its shutdown hooks may not have run, so a managed Postgres cluster may \
         still be running and holding its data dir; the next start recovers the \
         cluster through crash recovery, which is slower than a clean stop.",
        timeout.as_secs()
    )
}

/// The warning printed when the shutdown request could not be delivered.
///
/// Distinct from [`escalation_warning`]: nothing is wrong with the app here, so
/// telling the developer it "did not shut down in time" would send them hunting
/// in the wrong place.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn unreachable_warning(error: &str) -> String {
    format!(
        "  Warning: could not write the shutdown-signal file ({error}), so the app \
         was force-stopped without being asked to drain. Its shutdown hooks did \
         not run, so a managed Postgres cluster may still be running and holding \
         its data dir; the next start recovers the cluster through crash \
         recovery, which is slower than a clean stop."
    )
}

/// Stop `child` cooperatively: request a graceful shutdown via the signal file,
/// wait up to `timeout` for it to drain, then force-kill if it has not.
///
/// This is the Windows stop path, but it is compiled on every platform so Linux
/// CI exercises it. On Unix `stop_server` keeps using `SIGTERM`, which is the
/// mechanism production uses and is strictly faster.
#[cfg_attr(
    unix,
    allow(
        dead_code,
        reason = "the non-Unix stop path, compiled and tested everywhere"
    )
)]
fn stop_child_cooperatively(
    child: &mut Option<Child>,
    signal_path: &Path,
    timeout: Duration,
) -> StopOutcome {
    let Some(proc) = child.as_mut() else {
        return StopOutcome::NotRunning;
    };

    let outcome = match request_cooperative_shutdown(signal_path) {
        Err(_) => {
            let _ = proc.kill();
            let _ = proc.wait();
            StopOutcome::Unreachable
        }
        Ok(()) if crate::process::wait_with_timeout(proc, timeout).is_ok() => StopOutcome::Graceful,
        Ok(()) => {
            let _ = proc.kill();
            let _ = proc.wait();
            StopOutcome::Escalated
        }
    };

    // Always clear the request, on both paths: the next child must not inherit
    // a stale signal and drain at boot.
    clear_cooperative_shutdown(signal_path);
    *child = None;
    outcome
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
    // ── Cooperative child shutdown (#1616) ─────────────────────────────────
    //
    // The Windows dev loop used to stop the app with `TerminateProcess`
    // (`Child::kill`), which skips the app's `on_shutdown` hooks — so a managed
    // Postgres child was orphaned on every rebuild. `stop_child_cooperatively`
    // is the fix, and it is compiled on every platform so Linux CI actually
    // exercises the logic Windows depends on. It writes the signal file
    // autumn-web watches (`AUTUMN_SHUTDOWN_SIGNAL_FILE`), waits for the child
    // to drain, and only then escalates.

    fn signal_path(dir: &Path) -> PathBuf {
        dir.join("dev-shutdown.signal")
    }

    #[test]
    fn request_cooperative_shutdown_creates_the_file_the_runtime_watches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = signal_path(tmp.path());
        request_cooperative_shutdown(&path).expect("signal file should be creatable");
        assert!(path.exists(), "the runtime keys off existence alone");
    }

    #[test]
    fn request_cooperative_shutdown_creates_missing_parent_directories() {
        // The signal lives beside the live-reload state under `target/autumn`,
        // which may not exist yet on a cold `autumn dev`.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("autumn").join("stop.signal");
        request_cooperative_shutdown(&path).expect("parents should be created");
        assert!(path.exists());
    }

    #[test]
    fn clear_cooperative_shutdown_removes_a_stale_signal() {
        // A crashed `autumn dev` can leave the file behind; the next spawn must
        // clear it or the fresh child would drain the moment it boots.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = signal_path(tmp.path());
        std::fs::write(&path, b"stop").unwrap();
        clear_cooperative_shutdown(&path);
        assert!(!path.exists());
    }

    #[test]
    fn clear_cooperative_shutdown_is_a_no_op_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        clear_cooperative_shutdown(&signal_path(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn cooperative_stop_lets_a_child_that_honours_the_signal_exit_on_its_own() {
        // Stands in for an Autumn app: polls for the signal file, then exits 0
        // the way a drained app does. The outcome must be `Graceful` — that is
        // the case where `on_shutdown` hooks (managed Postgres teardown) run.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = signal_path(tmp.path());
        let script = format!(
            "while [ ! -f {} ]; do sleep 0.05; done; exit 0",
            path.display()
        );
        let mut child = Some(
            Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .spawn()
                .expect("spawn stand-in app"),
        );

        let outcome = stop_child_cooperatively(&mut child, &path, Duration::from_secs(10));
        assert_eq!(outcome, StopOutcome::Graceful);
        assert!(child.is_none(), "a stopped child must be reaped");
        assert!(
            !path.exists(),
            "the signal must be cleared so the next child does not drain at boot"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cooperative_stop_escalates_when_the_child_ignores_the_signal() {
        // An app that hangs (or predates the signal) must still be stopped —
        // degraded, but never silently: the caller reports the escalation.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = signal_path(tmp.path());
        let mut child = Some(
            Command::new("/bin/sh")
                .arg("-c")
                .arg("trap '' TERM; sleep 30")
                .spawn()
                .expect("spawn unresponsive app"),
        );

        let outcome = stop_child_cooperatively(&mut child, &path, Duration::from_millis(300));
        assert_eq!(outcome, StopOutcome::Escalated);
        assert!(child.is_none(), "escalation must still reap the child");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cooperative_stop_reports_unreachable_when_the_request_cannot_be_written() {
        // If the signal file cannot be created, the app was never *asked* to
        // stop. Reporting that as an escalation would blame the app for
        // ignoring a request it never received — and would hide a broken dev
        // setup behind a message about shutdown hooks.
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("nested").join("stop.signal");

        let mut child = Some(
            Command::new("/bin/sh")
                .arg("-c")
                .arg("sleep 30")
                .spawn()
                .expect("spawn app"),
        );
        let outcome = stop_child_cooperatively(&mut child, &path, Duration::from_millis(200));
        assert_eq!(outcome, StopOutcome::Unreachable);
        assert!(child.is_none(), "the child must still be stopped");
    }

    #[test]
    fn unreachable_warning_blames_the_signal_file_not_the_app() {
        let warning = unreachable_warning("permission denied");
        assert!(warning.contains("permission denied"), "{warning}");
        assert!(warning.contains("shutdown hooks"), "{warning}");
        // It must NOT claim the app ignored anything.
        assert!(!warning.contains("did not shut down within"), "{warning}");
    }

    #[test]
    fn cooperative_stop_reports_not_running_without_a_child() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = signal_path(tmp.path());
        let mut child = None;
        assert_eq!(
            stop_child_cooperatively(&mut child, &path, Duration::from_secs(1)),
            StopOutcome::NotRunning
        );
    }

    // ── The stop budget must not undercut the app's own (#1616, Codex P1) ──
    //
    // A fixed ten seconds was wrong: `DrainCause::Signal` runs the app's full
    // lifecycle — readiness flip, prestop grace, drain — and THEN its
    // `on_shutdown` hooks. An app configured with the prod defaults (grace 5 +
    // timeout 30) is still legitimately shutting down at t=35s, and a managed
    // Postgres `pg_ctl stop` runs after that under its own 60s ceiling. Killing
    // at t=10s reintroduces the orphaned postmaster this change exists to fix,
    // on a perfectly valid configuration.

    #[test]
    fn the_stop_budget_never_undercuts_the_apps_configured_drain() {
        for drain_secs in [0, 1, 10, 35, 120, 3600] {
            let budget = cooperative_stop_budget(drain_secs);
            assert!(
                budget > Duration::from_secs(drain_secs),
                "a {drain_secs}s drain must not be cut short by a {budget:?} budget"
            );
        }
    }

    #[test]
    fn the_stop_budget_covers_the_managed_postgres_stop_ceiling() {
        // `on_shutdown` hooks run AFTER the drain, so the headroom — not the
        // drain budget — is what has to cover a `pg_ctl stop`. autumn-web's
        // provider gives that operation a 60s ceiling.
        assert!(
            cooperative_stop_budget(0) >= Duration::from_secs(60),
            "the headroom must cover a managed-Postgres stop"
        );
    }

    #[test]
    fn the_stop_budget_grows_with_a_longer_configured_drain() {
        assert!(cooperative_stop_budget(35) > cooperative_stop_budget(1));
    }

    #[test]
    fn the_stop_budget_saturates_instead_of_overflowing() {
        // The drain budget comes from user config (`[server]` keys, or
        // `AUTUMN_SERVER__*`), so it is attacker-adjacent arithmetic: a
        // `u64::MAX` timeout must clamp, not wrap to a tiny budget that would
        // hard-kill instantly.
        let budget = cooperative_stop_budget(u64::MAX);
        assert!(budget >= Duration::from_secs(60), "{budget:?}");
    }

    // ── The budget must belong to the RUNNING child (#1616, Codex round 3) ──
    //
    // `autumn.toml` is a watched file, so `execute_plan` reaches the stop only
    // AFTER the edit that triggered it has landed. Resolving the budget there
    // grades the outgoing child against the incoming config: lower
    // `shutdown_timeout_secs` from 300 to 1 and the parent force-kills a child
    // that is still legitimately inside its original 300s drain — and a
    // half-saved (malformed) `autumn.toml` collapses to the defaults with the
    // same effect. Either way the `on_shutdown` hooks are skipped and the
    // managed cluster is orphaned, which is the bug this all exists to prevent.

    // ── The child's OWN budget beats any CLI reconstruction (Codex round 5) ──
    //
    // An app using `AppBuilder::with_config_loader` can resolve a budget no
    // amount of TOML/env reading reproduces. The runtime already solved this for
    // `autumn serve`: the app writes its resolved drain budget to the file named
    // by `AUTUMN_SERVE_READY_FILE`, and `signal_serve_ready`'s contract says why
    // — so `stop` "waits for the budget the app will actually drain for ...
    // instead of reconstructing it from TOML/env and risking a premature
    // SIGKILL". `autumn dev` was doing exactly the reconstructing it warns about.

    #[test]
    fn a_child_reported_budget_beats_everything_the_cli_can_reconstruct() {
        let budget = stop_budget_for_running_child(
            Some(Duration::from_secs(360)),
            Some(Duration::from_secs(61)),
            || Duration::from_secs(95),
        );
        assert_eq!(budget, Duration::from_secs(360));
    }

    #[test]
    fn the_recorded_budget_is_used_when_the_child_reported_nothing() {
        // Stopped before it finished booting: no ready file yet, so the
        // spawn-time estimate is the best available answer.
        let budget = stop_budget_for_running_child(None, Some(Duration::from_secs(61)), || {
            Duration::from_secs(95)
        });
        assert_eq!(budget, Duration::from_secs(61));
    }

    #[test]
    fn resolving_is_the_last_resort() {
        let budget = stop_budget_for_running_child(None, None, || Duration::from_secs(95));
        assert_eq!(budget, Duration::from_secs(95));
    }

    #[test]
    fn the_reported_budget_gets_the_same_hook_headroom() {
        // The app reports its DRAIN budget (prestop + shutdown). Hooks run after
        // the drain, so the headroom still applies — otherwise honouring the
        // report would reintroduce the very orphan it prevents.
        let tmp = tempfile::TempDir::new().unwrap();
        let ready = tmp.path().join("dev-ready.state");
        std::fs::write(&ready, "300\n").unwrap();
        assert_eq!(
            child_reported_stop_budget(&ready),
            Some(cooperative_stop_budget(300))
        );
    }

    #[test]
    fn a_missing_or_unparseable_report_is_ignored_rather_than_misread() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ready = tmp.path().join("dev-ready.state");
        assert_eq!(child_reported_stop_budget(&ready), None, "absent");
        std::fs::write(&ready, "not-a-number").unwrap();
        assert_eq!(child_reported_stop_budget(&ready), None, "junk");
        std::fs::write(&ready, "  ").unwrap();
        assert_eq!(child_reported_stop_budget(&ready), None, "blank");
    }

    #[test]
    fn the_ready_file_env_var_is_the_one_the_runtime_reads() {
        // `autumn/src/app.rs::signal_serve_ready` reads this exact name; a
        // rename on either side silently returns `autumn dev` to guessing.
        assert_eq!(SERVE_READY_FILE_ENV, "AUTUMN_SERVE_READY_FILE");
        assert_eq!(SERVE_READY_FILE_ENV, crate::serve::SERVE_READY_FILE_ENV);
    }

    #[test]
    fn the_stop_budget_uses_the_value_recorded_when_the_child_was_spawned() {
        let recorded = Some(Duration::from_secs(300));
        let budget = stop_budget_for_running_child(None, recorded, || Duration::from_secs(95));
        assert_eq!(budget, Duration::from_secs(300));
    }

    #[test]
    fn the_stop_budget_does_not_re_read_config_when_one_was_recorded() {
        // Not just "returns the right number": it must not touch the config at
        // all, or a malformed save mid-edit could still influence the stop.
        let consulted = std::cell::Cell::new(false);
        let _ = stop_budget_for_running_child(None, Some(Duration::from_secs(300)), || {
            consulted.set(true);
            Duration::from_secs(95)
        });
        assert!(
            !consulted.get(),
            "a recorded budget must not re-read the (possibly just-edited) config"
        );
    }

    #[test]
    fn the_stop_budget_falls_back_when_nothing_was_recorded() {
        // No child was spawned through `start_server` this session (or the
        // spawn predates the recording); resolving now is the best available
        // answer and is still better than a fixed constant.
        let budget = stop_budget_for_running_child(None, None, || Duration::from_secs(95));
        assert_eq!(budget, Duration::from_secs(95));
    }

    #[test]
    fn the_resolved_budget_covers_an_unconfigured_project() {
        // Resolving against a directory with no `autumn.toml` must still yield a
        // budget that outlasts a managed-Postgres teardown, not zero.
        assert!(resolve_cooperative_stop_budget(None) >= COOPERATIVE_STOP_HOOK_HEADROOM);
    }

    #[test]
    fn escalation_warning_names_the_consequence_not_just_the_timeout() {
        // "silent degradation" is the thing #1616 forbids: if hooks were
        // skipped, the developer has to be told what that means for them.
        let warning = escalation_warning(Duration::from_secs(10));
        assert!(warning.contains("10s"), "{warning}");
        assert!(warning.contains("shutdown hooks"), "{warning}");
        assert!(warning.contains("Postgres"), "{warning}");
    }

    #[test]
    fn dev_shutdown_signal_path_sits_beside_the_live_reload_state() {
        let path = resolve_dev_shutdown_signal_path().expect("shutdown signal path");
        assert!(
            path.parent()
                .is_some_and(|p| p.ends_with(Path::new("target").join("autumn"))),
            "unexpected directory: {}",
            path.display()
        );
    }

    #[test]
    fn dev_shutdown_signal_path_is_unique_per_dev_loop_process() {
        // Two `autumn dev` instances in one workspace share a target directory.
        // A shared file would let one instance's rebuild drain the other's app.
        let path = resolve_dev_shutdown_signal_path().expect("shutdown signal path");
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a file name");
        assert!(name.starts_with(DEV_SHUTDOWN_SIGNAL_PREFIX), "{name}");
        assert!(name.ends_with(DEV_SHUTDOWN_SIGNAL_SUFFIX), "{name}");
        assert!(
            name.contains(&std::process::id().to_string()),
            "the file name must carry this process's id: {name}"
        );
    }

    #[test]
    fn dev_shutdown_signal_path_is_cached_across_calls() {
        // The stop path reads this; resolving per stop would shell out to
        // `cargo metadata` on every rebuild and could fail on a half-saved
        // Cargo.toml — inside the stop, where a failure orphans the app.
        assert_eq!(dev_shutdown_signal_path(), dev_shutdown_signal_path());
    }

    #[test]
    fn dev_shutdown_env_var_matches_the_runtime_constant() {
        assert_eq!(
            DEV_SHUTDOWN_SIGNAL_ENV,
            autumn_web::app::SHUTDOWN_SIGNAL_FILE_ENV,
            "the CLI and the runtime must agree on the wire name"
        );
    }

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
        let result = start_server(Path::new("/nonexistent/binary/path"), None, None, false);
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn start_server_returns_child_for_valid_binary() {
        let child = start_server(Path::new("/bin/sleep"), None, None, false);
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
        stop_server(&mut child, None);
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
        stop_server(&mut child, None);
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
        let mut state = DevReloadState {
            path,
            version: 0,
            active_build_error: None,
        };

        state.signal(ReloadKind::Css).expect("css signal");
        let body = std::fs::read_to_string(state.path()).expect("read css");
        assert_eq!(body, r#"{"kind":"css","version":1}"#);

        state.signal(ReloadKind::Full).expect("full signal");
        let body = std::fs::read_to_string(state.path()).expect("read full");
        assert_eq!(body, r#"{"kind":"full","version":2}"#);
    }

    #[test]
    fn parse_build_diagnostics_extracts_errors_in_order_excluding_warnings() {
        // A realistic `cargo build --message-format=json` stream: an artifact
        // line, an error, a warning (must be excluded), a non-JSON line, a
        // second error whose primary span is NOT the first span, and a
        // build-finished line.
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"app"}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"code":{"code":"E0425"},"level":"error","message":"cannot find value `foo` in this scope","rendered":"error[E0425]: cannot find value `foo`\n --> src/main.rs:3:5\n","spans":[{"file_name":"src/main.rs","line_start":3,"column_start":5,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"code":null,"level":"warning","message":"unused variable: `x`","rendered":"warning: unused variable\n","spans":[{"file_name":"src/main.rs","line_start":9,"column_start":1,"is_primary":true}]}}"#,
            "\n",
            "   Compiling app v0.1.0 (not json)\n",
            r#"{"reason":"compiler-message","message":{"code":{"code":"E0308"},"level":"error","message":"mismatched types","rendered":"error[E0308]: mismatched types\n --> src/lib.rs:12:9\n","spans":[{"file_name":"src/other.rs","line_start":1,"column_start":1,"is_primary":false},{"file_name":"src/lib.rs","line_start":12,"column_start":9,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"build-finished","success":false}"#,
            "\n",
        );

        let diags = parse_build_diagnostics(stdout.as_bytes());

        assert_eq!(diags.len(), 2, "warnings and non-error lines excluded");
        assert_eq!(
            diags[0],
            BuildDiagnostic {
                code: Some("E0425".to_owned()),
                message: "cannot find value `foo` in this scope".to_owned(),
                file: "src/main.rs".to_owned(),
                line: 3,
                column: 5,
                rendered: "error[E0425]: cannot find value `foo`\n --> src/main.rs:3:5\n"
                    .to_owned(),
            }
        );
        // Compiler order is preserved: E0425 first, then E0308.
        assert_eq!(diags[1].code, Some("E0308".to_owned()));
        assert_eq!(diags[1].message, "mismatched types");
        // Primary span wins over the (earlier) non-primary span.
        assert_eq!(diags[1].file, "src/lib.rs");
        assert_eq!(diags[1].line, 12);
        assert_eq!(diags[1].column, 9);
    }

    #[test]
    fn compiler_message_rendered_echoes_all_levels_but_not_other_records() {
        // Warning-level messages must be echoed live so the terminal matches
        // plain `cargo build` output, even though they never enter the overlay.
        let warning = r#"{"reason":"compiler-message","message":{"code":null,"level":"warning","message":"unused variable: `x`","rendered":"warning: unused variable\n"}}"#;
        assert_eq!(
            compiler_message_rendered(warning).as_deref(),
            Some("warning: unused variable\n"),
        );

        // Error-level messages are echoed too.
        let error = r#"{"reason":"compiler-message","message":{"code":{"code":"E0425"},"level":"error","message":"cannot find value `foo`","rendered":"error[E0425]: cannot find value `foo`\n"}}"#;
        assert_eq!(
            compiler_message_rendered(error).as_deref(),
            Some("error[E0425]: cannot find value `foo`\n"),
        );

        // Non-JSON progress lines yield nothing.
        assert_eq!(compiler_message_rendered("   Compiling app v0.1.0"), None);

        // Non-`compiler-message` records (artifacts, build scripts, finished)
        // are never echoed.
        assert_eq!(
            compiler_message_rendered(r#"{"reason":"compiler-artifact","target":{"name":"app"}}"#),
            None,
        );

        // A compiler message with no `rendered` text yields nothing rather than
        // an empty echo.
        assert_eq!(
            compiler_message_rendered(
                r#"{"reason":"compiler-message","message":{"level":"warning","message":"x"}}"#
            ),
            None,
        );
    }

    /// A representative single-error diagnostic list for the build-error tests.
    fn sample_build_diagnostics() -> Vec<BuildDiagnostic> {
        vec![BuildDiagnostic {
            code: Some("E0425".to_owned()),
            message: "cannot find value `foo`".to_owned(),
            file: "src/main.rs".to_owned(),
            line: 3,
            column: 5,
            rendered: "error[E0425]: cannot find value `foo`".to_owned(),
        }]
    }

    #[test]
    fn dev_reload_state_signal_build_error_writes_payload() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState {
            path,
            version: 0,
            active_build_error: None,
        };

        let diags = sample_build_diagnostics();

        state
            .signal_build_error(&diags, true)
            .expect("build error signal");
        assert_eq!(state.version, 1, "build error bumps the version");

        let body = std::fs::read_to_string(state.path()).expect("read build error");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value["version"], 1);
        assert_eq!(value["kind"], "full");
        assert_eq!(value["build_error"]["stale"], true);
        assert_eq!(value["build_error"]["diagnostics"][0]["code"], "E0425");
        assert_eq!(
            value["build_error"]["diagnostics"][0]["file"],
            "src/main.rs"
        );
        assert_eq!(value["build_error"]["diagnostics"][0]["line"], 3);
        assert_eq!(value["build_error"]["diagnostics"][0]["column"], 5);
        assert_eq!(
            value["build_error"]["diagnostics"][0]["message"],
            "cannot find value `foo`"
        );
    }

    #[test]
    fn dev_reload_state_signal_carries_build_error_across_non_build_reloads() {
        // P2 regression guard: once a Rust build is broken, an ordinary CSS
        // (or other non-build) reload must NOT dismiss the overlay. It carries
        // the build_error payload forward while still bumping the version so
        // the client re-polls but stays on the overlay instead of reloading the
        // stale app.
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState {
            path,
            version: 0,
            active_build_error: None,
        };

        let diags = sample_build_diagnostics();
        state
            .signal_build_error(&diags, true)
            .expect("build error signal");
        assert_eq!(state.version, 1);

        // A Tailwind/CSS save while Rust is broken.
        state.signal(ReloadKind::Css).expect("css signal");
        assert_eq!(state.version, 2, "carrying forward still bumps the version");
        let body = std::fs::read_to_string(state.path()).expect("read carried");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value["version"], 2);
        assert_eq!(value["kind"], "full", "build error state stays a full kind");
        assert!(
            value.get("build_error").is_some(),
            "a non-build reload must carry the build_error forward"
        );
        assert_eq!(value["build_error"]["stale"], true);
        assert_eq!(value["build_error"]["diagnostics"][0]["code"], "E0425");

        // A plain full reload (e.g. a config-only restart) also carries it.
        state.signal(ReloadKind::Full).expect("full signal");
        assert_eq!(state.version, 3);
        let body = std::fs::read_to_string(state.path()).expect("read carried again");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert!(
            value.get("build_error").is_some(),
            "a plain full reload must also carry the build_error forward"
        );
    }

    #[test]
    fn dev_reload_state_signal_build_success_clears_overlay() {
        // Only a green Rust build clears the overlay: the explicit
        // success-clear drops the build_error field and bumps the version so
        // the client dismisses the overlay and reloads the freshly-built app.
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState {
            path,
            version: 0,
            active_build_error: None,
        };

        let diags = sample_build_diagnostics();
        state
            .signal_build_error(&diags, true)
            .expect("build error signal");
        assert_eq!(state.version, 1);

        state
            .signal_build_success(ReloadKind::Full)
            .expect("success signal");
        assert_eq!(state.version, 2);
        let body = std::fs::read_to_string(state.path()).expect("read cleared");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert!(
            value.get("build_error").is_none(),
            "a green build must clear the build_error field"
        );
        assert_eq!(value["version"], 2);
        assert_eq!(value["kind"], "full");

        // After a green build, ordinary signals no longer carry an overlay.
        state.signal(ReloadKind::Css).expect("css signal");
        let body = std::fs::read_to_string(state.path()).expect("read post-clear");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert!(
            value.get("build_error").is_none(),
            "no overlay should persist once cleared by a green build"
        );
        assert_eq!(value["kind"], "css");
        assert_eq!(value["version"], 3);
    }

    #[test]
    fn dev_reload_state_signal_none_is_noop() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload file");
        let path = reload_file.path().to_path_buf();
        let mut state = DevReloadState {
            path,
            version: 41,
            active_build_error: None,
        };

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
            active_build_error: None,
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

    // ── #1633: dependency findings in the dev loop ───────────────────────────

    #[test]
    fn an_unfinished_dependency_audit_prints_nothing() {
        // The audit is abandoned on timeout. Startup neither waits for it nor
        // reports on it.
        assert!(dependency_startup_lines(None).is_empty());
    }

    #[test]
    fn a_clean_tree_prints_nothing() {
        let clean = crate::deps::Evaluation::Audited {
            findings: Vec::new(),
            checks: vec!["advisories".to_owned()],
            db_age_days: Some(1),
            auditor: "cargo-deny 0.20.2".to_owned(),
        };
        assert!(dependency_startup_lines(Some(&clean)).is_empty());
    }

    #[test]
    fn the_audit_deadline_is_bounded() {
        // A poll that outlives the session is a leak; a verdict nobody will
        // read is not worth keeping.
        assert!(DEPENDENCY_AUDIT_DEADLINE <= Duration::from_secs(60));
    }

    #[test]
    fn the_dependency_audit_starts_after_the_build_and_is_never_awaited() {
        // Running the auditor beside the build makes its `cargo metadata`
        // contend with Cargo's package-cache lock and slow the build. Awaiting
        // it anywhere would delay startup or a rebuild.
        // Only the module's own code, not this test — the needles below
        // otherwise match the assertions that look for them.
        let source = include_str!("dev.rs");
        let code = &source[..source.find("#[cfg(test)]").expect("test module")];
        let build = code
            .find("let (built, diagnostics) = cargo_build_capturing(package);")
            .expect("initial build");
        let start = code
            .find("let mut dependency_audit = DependencyAudit::start(")
            .expect("audit start");
        assert!(
            build < start,
            "the audit must start after the initial build"
        );
        assert!(
            !code.contains("await_evaluation"),
            "the dev loop must poll the audit, never await it"
        );
    }

    #[test]
    fn a_dependency_audit_that_never_answers_stops_being_polled() {
        let (sender, receiver) = mpsc::channel();
        let mut audit = DependencyAudit {
            receiver,
            deadline: std::time::Instant::now(),
            done: false,
        };
        audit.report();
        assert!(audit.done, "an expired deadline must end the polling");
        drop(sender);
    }

    #[test]
    fn a_dependency_audit_reports_its_verdict_once() {
        let (sender, receiver) = mpsc::channel();
        let mut audit = DependencyAudit {
            receiver,
            deadline: std::time::Instant::now() + Duration::from_secs(60),
            done: false,
        };
        audit.report();
        assert!(!audit.done, "a pending audit stays pending");
        sender
            .send(crate::deps::Evaluation::NoPolicy)
            .expect("send");
        audit.report();
        assert!(audit.done, "a delivered verdict ends the polling");
    }
}
