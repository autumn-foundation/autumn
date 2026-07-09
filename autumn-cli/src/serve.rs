//! `autumn serve` — run the app as a production (non-watch) server, optionally
//! as a managed background daemon.
//!
//! Distinct from `autumn dev`: no file watching and no hot-reload. Instead it
//! provides a daemon lifecycle (`--daemon`, `stop`, `status`, `restart`) backed
//! by a PID lockfile, binds a Unix domain socket under a platform runtime dir,
//! and writes an address-discovery file so a thin client (or an agent) can find
//! the running service. Graceful shutdown reuses the app's existing lame-duck
//! drain via `SIGTERM`.

#![allow(dead_code, clippy::missing_const_for_fn)]

use crate::paths::RuntimePaths;
use crate::process::{self, AcquireError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Env var the managed-Postgres provider reads to locate its data dir. Mirrors
/// `autumn_web::managed_pg::MANAGED_PG_DATA_DIR_ENV`; redefined here because the
/// CLI doesn't build the `managed-pg` feature and so can't reference the const.
pub const MANAGED_PG_DATA_DIR_ENV: &str = "AUTUMN_MANAGED_PG_DATA_DIR";
/// Env var that makes the provider *attach* to an already-running cluster at the
/// given URL instead of starting its own. Mirrors
/// `autumn_web::managed_pg::MANAGED_PG_ATTACH_URL_ENV`.
pub const MANAGED_PG_ATTACH_URL_ENV: &str = "AUTUMN_MANAGED_PG_ATTACH_URL";

/// Lifecycle subcommand for `autumn serve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeAction {
    /// Stop the running daemon.
    Stop,
    /// Report whether the daemon is running and where it is reachable.
    Status,
    /// Stop (if running) then start in the background.
    Restart,
}

/// Options shared across `serve` invocations.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Package to build/run (for workspaces).
    pub package: Option<String>,
    /// Run in the background as a managed daemon.
    pub daemon: bool,
    /// Build in release mode (optimized production binary).
    pub release: bool,
    /// Recorded in the address file; set when the app bundles managed Postgres.
    pub bundled_pg: bool,
    /// Profile to force on the spawned app via `AUTUMN_ENV`. `None` for a normal
    /// start (the child inherits the shell's environment); set by `restart` to
    /// restore the original daemon's profile when the restart shell doesn't have
    /// one.
    pub profile: Option<String>,
}

/// How long to wait for a freshly-spawned daemon to become reachable.
///
/// The app writes its readiness file only after startup *migrations* complete,
/// and those can legitimately block up to `migrate::DEFAULT_LOCK_WAIT_TIMEOUT`
/// (60 s) waiting for the advisory migration lock when another runner holds it.
/// Keep this comfortably above that wait so a healthy daemon queued behind a
/// concurrent migration isn't killed before its own lock-wait can resolve.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// Readiness budget when managed Postgres must provision on first boot
/// (download/extract + `initdb` can take well over the default before the app
/// binds its socket).
const READY_TIMEOUT_MANAGED_PG: Duration = Duration::from_secs(300);
/// Extra seconds added to the app's configured shutdown budget before a
/// graceful `stop` escalates to `SIGKILL`.
const STOP_GRACE_BUFFER: Duration = Duration::from_secs(5);

/// Contents of the address-discovery file (`serve.addr`): how a client reaches
/// the running daemon. Serialized as TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrFile {
    /// PID of the server process.
    pub pid: u32,
    /// Transport: `"unix"` or `"tcp"`.
    pub transport: String,
    /// Socket path (unix) or `host:port` (tcp).
    pub address: String,
    /// Unix-epoch seconds when the daemon was started.
    pub started_at: u64,
    /// Whether the app supervises a bundled/managed Postgres.
    pub managed_pg: bool,
    /// Whether the daemon was built/started in release mode. Recovered by
    /// `restart` so a bare `autumn serve restart` keeps the optimized binary and
    /// the corresponding (prod) profile defaults. Defaults to `false` for address
    /// files written before this field existed.
    #[serde(default)]
    pub release: bool,
    /// The graceful-drain budget (`prestop_grace_secs + shutdown_timeout_secs`)
    /// resolved from the daemon's *own* environment and profile at start time.
    /// `stop` uses this as the authoritative budget so it never derives one from
    /// the (possibly different) `stop` invocation's environment. `None` for
    /// address files written before this field existed.
    #[serde(default)]
    pub stop_budget_secs: Option<u64>,
    /// The explicit profile (`AUTUMN_ENV`/`AUTUMN_PROFILE`) the daemon was
    /// started with, recorded so `restart` can restore it. `None` when the daemon
    /// relied on the build-mode default (or for older address files).
    #[serde(default)]
    pub profile: Option<String>,
}

impl AddrFile {
    /// Serialize to TOML.
    #[must_use]
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("AddrFile serializes to TOML")
    }

    /// Parse from TOML.
    ///
    /// # Errors
    ///
    /// Returns a TOML deserialization error if the contents are malformed.
    pub fn parse(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

/// Entry point dispatched from `main::run_command`.
pub fn run(action: Option<ServeAction>, opts: &ServeOptions) {
    #[cfg(unix)]
    let code = run_unix(action, opts);
    #[cfg(not(unix))]
    let code = run_non_unix(action, opts);
    std::process::exit(code);
}

/// Non-Unix entry point. The background-daemon lifecycle (`--daemon`, `stop`,
/// `status`, `restart`) is built on Unix domain sockets and POSIX signals and is
/// not supported here, but a plain foreground `autumn serve` still works: it
/// builds and runs the app binary, which binds TCP per its config.
#[cfg(not(unix))]
fn run_non_unix(action: Option<ServeAction>, opts: &ServeOptions) -> i32 {
    if action.is_some() || opts.daemon {
        eprintln!(
            "autumn serve: daemon mode (--daemon, stop, status, restart) is \
             currently supported on Unix only (Linux/macOS). Plain `autumn serve` \
             runs the app in the foreground on this platform."
        );
        return 1;
    }
    // Plain foreground server: builds and runs on the configured transport,
    // deferring (and, when not bundling Postgres, skipping) runtime-path setup.
    start_foreground(opts)
}

/// Unix implementation of `run` (daemon lifecycle).
#[cfg(unix)]
fn run_unix(action: Option<ServeAction>, opts: &ServeOptions) -> i32 {
    match action {
        None => start(opts),
        Some(ServeAction::Stop) => stop(opts),
        Some(ServeAction::Status) => status(opts),
        Some(ServeAction::Restart) => {
            // `restart` is a fresh invocation, so `opts` reflects the restart
            // command's flags — not the original `start`. Recover managed-PG mode
            // and release mode from the running daemon's address file so a bare
            // `restart` keeps the project data dir, the longer readiness budget,
            // the optimized binary, and the matching profile defaults. Always
            // relaunch in the background.
            let running = running_daemon_addr(opts.package.as_deref());
            // Recover managed-PG mode. A parseable address file is authoritative
            // (respect an explicit `managed_pg = false` even if a `pg/PG_VERSION`
            // from a prior bundled run still lingers). Only when there is no
            // usable address file do we infer managed mode from an initialized
            // cluster in the project data dir, so a bare `restart` after a crash
            // keeps `AUTUMN_MANAGED_PG_DATA_DIR`/the long readiness budget.
            let keep_managed = opts.bundled_pg
                || running.as_ref().map_or_else(
                    || managed_cluster_present(opts.package.as_deref()),
                    |a| a.managed_pg,
                );
            // The address file is authoritative when present; only fall back to
            // the `serve.mode` marker for release/profile when it is
            // missing/corrupt. A best-effort marker left over from an older
            // release run must not force a parseable dev daemon back to release.
            let recorded_mode = running_daemon_mode(opts.package.as_deref());
            let keep_release = opts.release
                || running.as_ref().map_or_else(
                    || recorded_mode.as_ref().is_some_and(|m| m.release),
                    |a| a.release,
                );
            // Preserve the original daemon's profile: prefer one set on *this*
            // restart's environment, else what the running daemon recorded —
            // address file first, the mode marker only when there is no address
            // file — so a bare `restart` doesn't silently fall back to `dev`.
            let keep_profile = env_profile().or_else(|| {
                running.as_ref().map_or_else(
                    || recorded_mode.as_ref().and_then(|m| m.profile.clone()),
                    |a| a.profile.clone(),
                )
            });
            let daemon_opts = ServeOptions {
                daemon: true,
                bundled_pg: keep_managed,
                release: keep_release,
                profile: keep_profile,
                ..opts.clone()
            };
            // Stop using the *recovered* mode, not the bare restart flags: when
            // `serve.addr` is missing/corrupt, `stop`'s budget falls back to
            // recomputing from release/profile, and the restart shell's defaults
            // would otherwise derive a short `dev` budget and SIGKILL a release
            // daemon before it finishes draining.
            let _ = stop(&daemon_opts);
            start(&daemon_opts)
        }
    }
}

/// Parse the address file under `paths`, if present and well-formed.
fn read_addr_file(paths: &RuntimePaths) -> Option<AddrFile> {
    std::fs::read_to_string(paths.addr_file())
        .ok()
        .and_then(|s| AddrFile::parse(&s).ok())
}

/// The currently-recorded daemon's address file, if present and parseable.
/// Best-effort: `None` if anything can't be read.
fn running_daemon_addr(package: Option<&str>) -> Option<AddrFile> {
    let paths = RuntimePaths::resolve(&project_identity(package)).ok()?;
    read_addr_file(&paths)
}

/// Whether an initialized managed-Postgres cluster already exists for this
/// project — its data dir has a `PG_VERSION` (present in every `PostgreSQL` data
/// directory after `initdb`). Used by `restart` to recover `--bundled-pg` mode
/// when the address file is gone.
fn managed_cluster_present(package: Option<&str>) -> bool {
    RuntimePaths::resolve(&project_identity(package))
        .is_ok_and(|p| p.pg_data_dir().join("PG_VERSION").exists())
}

/// Whether a Unix socket at `path` has a live listener (a `connect` succeeds).
/// Used as a portable daemon-identity check where the OS can't verify a recorded
/// process start time (e.g. macOS).
#[cfg(unix)]
fn socket_is_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn socket_is_live(_path: &Path) -> bool {
    false
}

/// The PID of the process listening on the Unix socket at `path`, via
/// `SO_PEERCRED`. `None` when it can't be determined — not connectable, or a
/// platform without a peer-PID syscall (macOS/BSD).
#[cfg(target_os = "linux")]
fn socket_owner_pid(path: &Path) -> Option<u32> {
    let stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    let cred =
        nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials).ok()?;
    u32::try_from(cred.pid()).ok()
}

#[cfg(not(target_os = "linux"))]
fn socket_owner_pid(_path: &Path) -> Option<u32> {
    None
}

/// Best-effort check that the daemon serving on `socket` is `pid`: a definitive
/// `SO_PEERCRED` match on Linux, otherwise socket liveness (so a reused PID that
/// is genuinely not the listener is rejected where the kernel can tell us).
fn socket_identity_matches(socket: &Path, pid: u32) -> bool {
    socket_owner_pid(socket).map_or_else(|| socket_is_live(socket), |owner| owner == pid)
}

/// Whether `rec` identifies our live daemon, guarding against PID reuse.
///
/// A recorded start time that matches the live process is conclusive (Linux).
/// Otherwise — no recorded start time, or a platform that can't report one such
/// as macOS — the PID alone is ambiguous, so require the daemon's socket to have
/// a live listener: an unrelated process that reused the PID would not be
/// listening there. This stops `status`/`stop` from acting on a reused PID.
///
/// `startup_in_progress` (the startup lock still exists) covers the boot window:
/// the pidfile is written before the app binds its socket, so on macOS/BSD a
/// live PID with no listener yet would otherwise be misread as a dead stale
/// pidfile and removed mid-startup.
fn confirmed_running(rec: &process::PidRecord, socket: &Path, startup_in_progress: bool) -> bool {
    if !process::is_process_alive(rec.pid) {
        return false;
    }
    match (rec.start_time, process::process_start_time(rec.pid)) {
        // Identity is known on both sides (Linux): trust the comparison and do
        // *not* fall back to the socket — a reused PID with a different start
        // time is stale even if some daemon happens to be listening.
        (Some(recorded), Some(current)) => recorded == current,
        // Identity unknown (no recorded start time, or a platform like macOS
        // that can't report one): a start still in progress means the live PID
        // is our daemon binding its socket; otherwise confirm the PID actually
        // owns the socket (SO_PEERCRED on Linux, else liveness) so a reused PID
        // isn't accepted.
        _ => startup_in_progress || socket_identity_matches(socket, rec.pid),
    }
}

/// Resolve the daemon PID for lifecycle commands. Prefers the authoritative
/// pidfile (which carries a start time for PID-reuse detection); if it is
/// missing or corrupt, falls back to the address file's PID — but only when the
/// socket's listener is *definitively* that PID, so a removed pidfile doesn't
/// leave the daemon unstoppable while never signalling the wrong process.
///
/// The address file persists after its daemon dies (until `cleanup`), so its PID
/// is untrustworthy on its own: it may have been reused. Unlike the pidfile path
/// in [`confirmed_running`], a bare live-socket probe is *not* enough here —
/// some unrelated process could own the socket while the recorded PID is a
/// reused stranger. Require `SO_PEERCRED` proof; where peer-PID is unavailable
/// (macOS/BSD), report "not running" rather than risk signalling the wrong PID.
fn lifecycle_target(paths: &RuntimePaths, socket: &Path) -> Option<process::PidRecord> {
    let pidfile_rec = process::read_pidfile(&paths.pid_file());
    // A definitive socket owner (Linux `SO_PEERCRED`) is the live daemon. Prefer
    // it over a parseable-but-*stale* pidfile (e.g. one overwritten/restored
    // while the daemon is healthy) so a damaged pidfile doesn't make a running
    // daemon unmanageable. Keep the pidfile record when it matches the owner so
    // its recorded start time is retained.
    if let Some(owner) = socket_owner_pid(socket) {
        // Keep the pidfile record when it matches the live owner (retains start_time).
        if let Some(rec) = pidfile_rec
            && rec.pid == owner
        {
            return Some(rec);
        }
        // Trust the socket owner over a missing/stale pidfile only when this
        // project has daemon state corroborating it (a pidfile or address file).
        // Without that, an unrelated same-user process that reused or squatted on
        // the socket path must not be mistaken for our daemon and signalled.
        if pidfile_rec.is_some() || read_addr_file(paths).is_some() {
            return Some(process::PidRecord {
                pid: owner,
                start_time: process::process_start_time(owner),
            });
        }
        return None;
    }
    // No peer-PID (macOS/BSD, or no live listener): trust the pidfile if present.
    if let Some(rec) = pidfile_rec {
        return Some(rec);
    }
    // Last resort: the address file — but only when a peer-PID proves it owns the
    // socket (it doesn't here, since `socket_owner_pid` was `None`), so this
    // reports "not running" rather than risk signalling a reused PID.
    let addr = read_addr_file(paths)?;
    (socket_owner_pid(socket) == Some(addr.pid)).then_some(process::PidRecord {
        pid: addr.pid,
        start_time: None,
    })
}

/// Resolve the project's runtime paths, exiting on failure.
fn resolve_paths(package: Option<&str>) -> RuntimePaths {
    let project = project_identity(package);
    RuntimePaths::resolve(&project).unwrap_or_else(|e| {
        eprintln!("autumn serve: {e}");
        std::process::exit(1);
    })
}

/// The transient startup lock `launch_daemon_child` holds (with its own pid)
/// from spawn until the daemon is ready.
fn start_lock_path(paths: &RuntimePaths) -> PathBuf {
    paths.pid_file().with_file_name("serve.startlock")
}

/// Whether a daemon start is genuinely in progress: the startup lock exists
/// *and* the launcher that wrote it is still alive. Validating the recorded
/// launcher (not mere file existence) means a `serve.startlock` left by a killed
/// launcher isn't mistaken for an in-flight boot — which would otherwise let
/// `confirmed_running` trust a reused PID without a socket-ownership check.
fn startup_in_progress(paths: &RuntimePaths) -> bool {
    process::read_pidfile(&start_lock_path(paths)).is_some_and(|rec| process::is_record_alive(&rec))
}

/// Derive a stable project identity for namespacing runtime dirs.
///
/// The base name is the explicit `-p` package, else the project directory's
/// name. A short hash of the (workspace-anchored) project directory is appended
/// so two unrelated checkouts that share a name never collide on the same
/// pidfile / socket / managed-Postgres data dir.
///
/// The name is deliberately **manifest-independent**: it never reads
/// `Cargo.toml`'s `[package].name`, so a daemon's namespace stays the same even
/// if the manifest later becomes temporarily unparseable/unreadable (which would
/// otherwise flip the name and make `status`/`stop` target a different, empty
/// namespace).
fn project_identity(package: Option<&str>) -> String {
    let name = package.map_or_else(
        || {
            canonical_cwd().file_name().map_or_else(
                || "autumn-app".to_owned(),
                |n| n.to_string_lossy().into_owned(),
            )
        },
        ToOwned::to_owned,
    );
    format!("{name}-{}", project_dir_hash(package))
}

/// A short, stable hash distinguishing same-named projects in different
/// locations.
///
/// For a workspace member selected with `-p`, the namespace is anchored to the
/// workspace root plus the package name, so it is identical whether the
/// lifecycle command runs from the workspace root or the member directory
/// (otherwise `start -p api` and `stop -p api` from different CWDs would target
/// different daemons). Falls back to the canonicalized CWD when no package is
/// given.
///
/// This is deliberately **metadata-free**: it walks the filesystem rather than
/// spawning `cargo metadata`, so `stop`/`status` resolve the same namespace as
/// `start` even when the toolchain is slow, offline, or unavailable — and a
/// daemon started under one resolution is never orphaned by a later one.
fn project_dir_hash(package: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    // NUL separates the two fields so no root path / package name pair can be
    // confused for another (paths cannot contain NUL).
    let key = package.map_or_else(
        || canonical_cwd().display().to_string(),
        |pkg| format!("{}\0{pkg}", identity_root().display()),
    );
    // A fixed SHA-256 over the key — `DefaultHasher` is explicitly not stable
    // across std/toolchain versions, so a CLI upgrade while a daemon is running
    // must not change the namespace (which would orphan it). First 4 bytes
    // (8 hex) keep the resulting Unix socket path within `sun_path`.
    hex::encode(&Sha256::digest(key.as_bytes())[..4])
}

/// The canonicalized current working directory, falling back to the raw cwd and
/// finally `"."` so this never fails.
fn canonical_cwd() -> PathBuf {
    std::env::current_dir()
        .ok()
        .and_then(|d| std::fs::canonicalize(&d).ok().or(Some(d)))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The directory that anchors a `-p <member>` namespace.
///
/// Walks ancestors of the CWD looking for the nearest `Cargo.toml` whose
/// manifest declares a `[workspace]` table — that workspace root is stable from
/// anywhere inside the tree. If none is found (a standalone crate), the nearest
/// ancestor containing any `Cargo.toml` is used; failing that, the CWD itself.
fn identity_root() -> PathBuf {
    let cwd = canonical_cwd();
    workspace_anchor_from(&cwd).unwrap_or(cwd)
}

/// Pure ancestor walk for [`identity_root`]: returns the workspace root (nearest
/// ancestor whose `Cargo.toml` has a `[workspace]` table), else the nearest
/// ancestor containing any `Cargo.toml`, else `None`. Separated from CWD lookup
/// so it can be unit-tested against a fixture tree.
///
/// If a `Cargo.toml` above us is unparsable but still contains the `[workspace]`
/// header, we treat it as a *transiently-broken workspace root* and anchor to
/// that exact dir (the nearest such one). This keeps the namespace stable while
/// the root manifest is mid-edit during a `-p` lifecycle command from a member —
/// otherwise the anchor would flip to the member and `status`/`stop` would target
/// a different dir. A broken manifest *without* `[workspace]` is ignored so an
/// unrelated stray file up the tree can't hijack a standalone project's anchor.
fn workspace_anchor_from(start: &Path) -> Option<PathBuf> {
    let mut nearest_manifest: Option<PathBuf> = None;
    let mut broken_workspace_dir: Option<PathBuf> = None;
    for dir in start.ancestors() {
        let Ok(contents) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        if nearest_manifest.is_none() {
            nearest_manifest = Some(dir.to_path_buf());
        }
        match toml::from_str::<toml::Table>(&contents) {
            // A nearer transiently-broken workspace root (the one being edited)
            // takes precedence over an outer valid `[workspace]`: in a nested
            // checkout the daemon was started under the inner root, so anchoring
            // to the outer one would make lifecycle commands miss it.
            Ok(table) if table.contains_key("workspace") => {
                return broken_workspace_dir.or_else(|| Some(dir.to_path_buf()));
            }
            // A *transiently-broken* workspace root: unparsable but still carrying
            // the `[workspace]` header. Anchor to this exact dir (the nearest such
            // root), not the topmost manifest in the ancestry — a higher unrelated
            // `Cargo.toml` above it must not pull the anchor up to the outer
            // project, or lifecycle commands would target a different runtime dir.
            // An unrelated broken `Cargo.toml` (no `[workspace]`) is ignored so it
            // can't hijack a standalone project's anchor.
            Err(_) if contents.contains("[workspace]") => {
                if broken_workspace_dir.is_none() {
                    broken_workspace_dir = Some(dir.to_path_buf());
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    broken_workspace_dir.or(nearest_manifest)
}

/// Build, then start the server (foreground or daemon). Returns the exit code.
fn start(opts: &ServeOptions) -> i32 {
    if opts.daemon {
        start_daemon(opts)
    } else {
        start_foreground(opts)
    }
}

/// Build and launch the background daemon: requires the runtime dirs (pidfile,
/// socket, log) and rejects a second start while a live daemon holds the lock.
fn start_daemon(opts: &ServeOptions) -> i32 {
    let paths = resolve_paths(opts.package.as_deref());
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("autumn serve: cannot create runtime dirs: {e}");
        return 1;
    }

    // Reject a second start while a live daemon holds the lock. Use the
    // socket-confirmed identity check so a stale pidfile whose PID was reused by
    // an unrelated process (notably on macOS, where start time isn't available)
    // doesn't block a legitimate start.
    if let Some(rec) = process::read_pidfile(&paths.pid_file())
        && confirmed_running(&rec, &paths.socket_file(), startup_in_progress(&paths))
    {
        eprintln!(
            "autumn serve: already running (pid {}). \
             Use `autumn serve stop` or `autumn serve restart`.",
            rec.pid
        );
        return 1;
    }

    // No live daemon owns this namespace (checked above), so a managed Postgres
    // still bound to the data dir is an orphan from a crashed prior daemon —
    // `postgresql_embedded` won't attach to it and the app's boot would fail.
    // Reap it here, where the daemon lifecycle is known to be gone, rather than
    // in the provider (which can't tell an orphan from a live direct-use app).
    //
    // Skip while another start holds a live startup lock: that concurrent start
    // may already be *provisioning* this cluster (it took `serve.startlock`
    // before writing `serve.pid`), and reaping would kill its postmaster. This
    // losing start will be rejected by the startup-lock acquire below anyway.
    //
    // Also skip while a server is live on the socket: the pidfile-only check
    // above misses a healthy daemon whose `serve.pid` is missing/corrupt, and
    // reaping would cut that daemon off from its database before
    // `launch_daemon_child`'s live-socket guard rejects this start.
    if opts.bundled_pg && !startup_in_progress(&paths) && !socket_is_live(&paths.socket_file()) {
        reap_managed_postgres(&paths);
    }

    eprintln!("\u{1F342} autumn serve\n");
    if !crate::dev::cargo_build(opts.package.as_deref(), opts.release) {
        eprintln!("\u{2717} Build failed. Fix the errors above and retry.");
        return 1;
    }
    let binary = crate::dev::find_binary(opts.package.as_deref(), opts.release);
    spawn_daemon(&binary, &paths, opts)
}

/// Build and run a plain foreground server. Unlike the daemon, this needs no
/// pidfile/socket/log directory; only managed Postgres needs a persistent data
/// dir. Resolving the platform runtime dirs is therefore deferred and skipped
/// entirely for a non-bundled TCP server, so foreground `autumn serve` works in
/// minimal containers without a discoverable home/platform directory.
fn start_foreground(opts: &ServeOptions) -> i32 {
    eprintln!("\u{1F342} autumn serve\n");
    if !crate::dev::cargo_build(opts.package.as_deref(), opts.release) {
        eprintln!("\u{2717} Build failed. Fix the errors above and retry.");
        return 1;
    }
    let binary = crate::dev::find_binary(opts.package.as_deref(), opts.release);

    let paths = if opts.bundled_pg {
        let paths = resolve_paths(opts.package.as_deref());
        if let Err(e) = paths.ensure_dirs() {
            eprintln!("autumn serve: cannot create runtime dirs: {e}");
            return 1;
        }
        Some(paths)
    } else {
        None
    };
    run_foreground(&binary, paths.as_ref(), opts)
}

/// Base command for the app binary: for daemon starts bind the Unix socket via
/// config env, and (for managed-Postgres apps) point the bundled cluster at a
/// persistent dir. `paths` is `None` for a plain foreground server that needs no
/// runtime directories.
fn base_command(binary: &Path, paths: Option<&RuntimePaths>, opts: &ServeOptions) -> Command {
    let mut cmd = Command::new(binary);
    // `autumn serve` is the cluster *owner*: it must provision and supervise its
    // own managed Postgres, never attach to someone else's. Clear any inherited
    // `AUTUMN_MANAGED_PG_ATTACH_URL` (e.g. leaked from the launching shell or a
    // prior task/build run) so the provider can't take the attach branch and
    // leave the daemon recorded as managed-PG while it never starts/stops a
    // cluster (and `stop()` no-ops because `attached` is set).
    cmd.env_remove(MANAGED_PG_ATTACH_URL_ENV);
    // For a workspace member selected with `-p`, run the child from the member's
    // manifest dir so its `autumn.toml`/profile and asset dirs resolve correctly
    // instead of the workspace-root CWD. Set both `current_dir` (covers CWD-
    // relative assets) and `AUTUMN_MANIFEST_DIR` (the config loader's explicit
    // override) so the member's config is loaded regardless of build mode.
    if let Some(pkg) = opts.package.as_deref()
        && let Some(dir) = crate::dev::find_manifest_dir(pkg)
    {
        cmd.current_dir(&dir);
        cmd.env("AUTUMN_MANIFEST_DIR", &dir);
    }
    // The Unix-domain socket is the daemon's private transport (for address-file
    // discovery and the thin client). Only force it for daemon starts: a plain
    // foreground `autumn serve` must stay on its configured `server.host`/`port`
    // (the app rejects `unix_socket` off-Unix anyway), so it remains reachable at
    // the expected TCP address like any production server.
    #[cfg(unix)]
    if opts.daemon
        && let Some(paths) = paths
    {
        // The standard nested-config env var feeds the default loader. We also
        // set a dedicated out-of-band override the framework re-applies *after*
        // config loading, so a custom `with_config_loader` that ignores env
        // can't drop the daemon's socket and strand it on TCP.
        cmd.env("AUTUMN_SERVER__UNIX_SOCKET", paths.socket_file());
        cmd.env("AUTUMN_SERVE_FORCE_UNIX_SOCKET", paths.socket_file());
        // The app creates this file right after startup completes; the
        // supervisor polls it to detect readiness without an HTTP probe.
        cmd.env("AUTUMN_SERVE_READY_FILE", paths.ready_file());
    }
    if opts.bundled_pg
        && let Some(paths) = paths
    {
        cmd.env(MANAGED_PG_DATA_DIR_ENV, paths.pg_data_dir());
    }
    // Restore an explicit profile (set by `restart`) so the relaunched daemon
    // loads the same config as the original even when the restart shell didn't
    // set `AUTUMN_ENV`.
    if let Some(profile) = &opts.profile {
        cmd.env("AUTUMN_ENV", profile);
    } else if opts.release && env_profile().is_none() {
        // A `--release` serve is a production run, and `stop`/the address file
        // already treat release as the `prod` profile. Without this the child's
        // config loader would default to `dev` (CSRF/HSTS off, ~1s shutdown
        // budget). Mirror `effective_profile(None, true)`; don't override an
        // `AUTUMN_ENV`/`AUTUMN_PROFILE` the caller already set.
        cmd.env("AUTUMN_ENV", "prod");
    }
    cmd
}

/// Run the server in the foreground on its configured transport. On Unix this
/// `exec`s the app, replacing this process so there is no supervisor hop:
/// SIGTERM/SIGINT from systemd/Docker/k8s and the exit code flow straight to the
/// server and its graceful drain runs. No daemon pidfile/socket/address-file
/// machinery applies to a foreground run.
fn run_foreground(binary: &Path, paths: Option<&RuntimePaths>, opts: &ServeOptions) -> i32 {
    let mut cmd = base_command(binary, paths, opts);
    eprintln!("  Running {} in the foreground", binary.display());
    eprintln!("  Press Ctrl+C to stop\n");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns on failure; on success this process *becomes* the
        // app, so termination signals reach it directly.
        let err = cmd.exec();
        eprintln!("\u{2717} Failed to start {}: {err}", binary.display());
        1
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(e) => {
                eprintln!("\u{2717} Failed to start {}: {e}", binary.display());
                1
            }
        }
    }
}

/// Create (truncating) the daemon log file with owner-only (`0600`) permissions
/// on Unix. The log captures the child's stdout/stderr — request data, panics,
/// and diagnostics — so other local users who can traverse the log dir must not
/// be able to read it.
fn create_private_log(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    // `mode(0o600)` only applies when *creating* the file; an existing log
    // (from an earlier version or manual creation) keeps its old, possibly
    // world-readable mode, so tighten it explicitly after opening — and fail
    // *closed* if we can't. The child's stdout/stderr (request data, panics,
    // diagnostics) goes here, so in a shared log dir we must not start while it
    // stays readable to other users.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Launch the detached daemon child and record its pidfile, under a separate
/// startup lock. On success returns the running child plus the still-held
/// startup-lock path (the caller releases it only once the daemon is ready, so a
/// concurrent start can't race in during readiness). `Err(exit_code)` on failure
/// (after cleaning up and releasing the lock). The pidfile only ever holds the
/// child's pid, so concurrent lifecycle commands never see the launcher.
fn launch_daemon_child(
    binary: &Path,
    paths: &RuntimePaths,
    opts: &ServeOptions,
    socket: &Path,
) -> Result<(std::process::Child, PathBuf), i32> {
    // A live listener already owning the socket means a daemon is serving here
    // even if the pidfile is missing or stale; refuse so readiness can't latch
    // onto the pre-existing listener (mirrors the pidfile guard).
    #[cfg(unix)]
    if std::os::unix::net::UnixStream::connect(socket).is_ok() {
        eprintln!(
            "autumn serve: already running (a live server owns {}). \
             Use `autumn serve stop` or `autumn serve restart`.",
            socket.display()
        );
        return Err(1);
    }

    // Separate startup lock (claimed with our own pid) so two concurrent starts
    // can't both spawn — but it is NOT the pidfile, so a concurrent `stop`/
    // `status` during startup never signals the launcher.
    let start_lock = start_lock_path(paths);
    match process::acquire_pidfile(&start_lock, std::process::id()) {
        Ok(()) => {}
        Err(AcquireError::AlreadyRunning(existing)) => {
            eprintln!("autumn serve: another `autumn serve` is starting (pid {existing}).");
            return Err(1);
        }
        Err(AcquireError::Io(e)) => {
            eprintln!("autumn serve: cannot take startup lock: {e}");
            return Err(1);
        }
    }
    let log = match create_private_log(&paths.log_file()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "autumn serve: cannot open log file {}: {e}",
                paths.log_file().display()
            );
            let _ = std::fs::remove_file(&start_lock);
            cleanup(paths, socket);
            return Err(1);
        }
    };
    let Ok(log_err) = log.try_clone() else {
        eprintln!("autumn serve: cannot duplicate log handle");
        let _ = std::fs::remove_file(&start_lock);
        cleanup(paths, socket);
        return Err(1);
    };

    // Clear any readiness file left by a previous run so `wait_for_ready` only
    // returns on the file the child we are about to spawn creates.
    let _ = std::fs::remove_file(paths.ready_file());

    let mut cmd = base_command(binary, Some(paths), opts);
    cmd.stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("\u{2717} Failed to start daemon: {e}");
            let _ = std::fs::remove_file(&start_lock);
            cleanup(paths, socket);
            return Err(1);
        }
    };
    let pid = child.id();

    // The live-socket guard at the top proved no daemon is serving this socket,
    // and we hold the startup lock, so any existing pidfile is from a daemon
    // that has already exited. On platforms that don't record a process start
    // time (macOS/BSD, or an older pidfile), `acquire_pidfile` can't tell that
    // crashed daemon from an unrelated process that reused its PID, and would
    // reject the start as `AlreadyRunning` — permanently blocking restart until
    // the pidfile is deleted by hand. Since the socket is provably dead, clear
    // such an unverifiable pidfile so the acquire below can reclaim it.
    #[cfg(unix)]
    if process::read_pidfile(&paths.pid_file()).is_some_and(|r| r.start_time.is_none()) {
        let _ = std::fs::remove_file(paths.pid_file());
    }

    // Record the real pidfile (child's pid). Keep the startup lock held — the
    // caller releases it only after the daemon is ready, so a concurrent start
    // can't spawn a second child while this one is still binding.
    if let Err(e) = process::acquire_pidfile(&paths.pid_file(), pid) {
        match e {
            AcquireError::AlreadyRunning(existing) => {
                eprintln!("autumn serve: already running (pid {existing}).");
            }
            AcquireError::Io(e) => eprintln!("autumn serve: cannot write pidfile: {e}"),
        }
        let _ = child.kill();
        process::force_kill_group(pid);
        let _ = child.wait();
        let _ = std::fs::remove_file(&start_lock);
        cleanup(paths, socket);
        return Err(1);
    }
    Ok((child, start_lock))
}

/// Spawn the server detached into the background and supervise readiness.
fn spawn_daemon(binary: &Path, paths: &RuntimePaths, opts: &ServeOptions) -> i32 {
    let socket = paths.socket_file();
    let (mut child, start_lock) = match launch_daemon_child(binary, paths, opts, &socket) {
        Ok(launched) => launched,
        Err(code) => return code,
    };
    let pid = child.id();
    let log_path = paths.log_file();
    // The startup lock is held until the daemon is ready, then released here on
    // every path so a concurrent start can't race in during the bind window.
    let release_lock = || {
        let _ = std::fs::remove_file(&start_lock);
    };

    let ready_timeout = if opts.bundled_pg {
        READY_TIMEOUT_MANAGED_PG
    } else {
        READY_TIMEOUT
    };
    if wait_for_ready(&paths.ready_file(), &mut child, ready_timeout) {
        if let Err(e) = write_addr_file(paths, pid, &socket, opts) {
            // Without the discovery file the daemon is unreachable to clients;
            // treat it like the pidfile failure — stop the child and fail.
            eprintln!(
                "autumn serve: cannot write address file {}: {e}",
                paths.addr_file().display()
            );
            let _ = child.kill();
            process::force_kill_group(pid);
            let _ = child.wait();
            // Postgres `setsid`s out of the daemon's group, so the group kill
            // above can't reach it; reap it directly via `postmaster.pid`.
            if opts.bundled_pg {
                reap_managed_postgres(paths);
            }
            release_lock();
            cleanup(paths, &socket);
            return 1;
        }
        release_lock();
        println!(
            "autumn serve: started (pid {pid}) on unix:{}",
            socket.display()
        );
        println!("  address file: {}", paths.addr_file().display());
        println!("  logs: {}", log_path.display());
        // Detach: leave the running child to be reparented to init on exit.
        // (Dropping the handle does not signal or reap the live process.)
        drop(child);
        0
    } else {
        eprintln!(
            "autumn serve: daemon did not become ready within {}s; see {}",
            ready_timeout.as_secs(),
            log_path.display()
        );
        // Kill the whole group: a startup that timed out may have spawned
        // children that `Child::kill()` alone would leave orphaned.
        let _ = child.kill();
        process::force_kill_group(pid);
        let _ = child.wait();
        // A managed Postgres started during `--bundled-pg` provisioning leaves
        // the daemon's process group (it `setsid`s itself) and no `serve.addr`
        // was written for a later `stop` to consult, so the group kill above
        // can't reach it. Reap it directly via `postmaster.pid` so a readiness
        // timeout doesn't strand the cluster on the data dir/port.
        if opts.bundled_pg {
            reap_managed_postgres(paths);
        }
        release_lock();
        cleanup(paths, &socket);
        1
    }
}

/// Apply OS-specific flags to detach the spawned process from the terminal.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // New process group: terminal job-control signals (Ctrl-C) won't reach it.
    cmd.process_group(0);
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut Command) {}

/// Poll until the daemon signals startup-complete by creating its readiness
/// file, the child exits/crashes, or the timeout elapses.
///
/// The app writes `ready_file` (path passed to it via `AUTUMN_SERVE_READY_FILE`)
/// immediately after `mark_startup_complete()` — i.e. once the socket is bound
/// and serving *and* startup hooks/migrations have finished. Waiting on that
/// file, rather than probing the socket over HTTP, means "ready" reflects real
/// readiness with no dependence on the app's HTTP middleware (the startup
/// barrier, maintenance mode, rate limiting, or custom health paths). The
/// `child.try_wait()` check fails fast if the daemon dies during startup (a
/// zombie keeps `kill(pid, 0)` alive, which would otherwise hang the timeout).
fn wait_for_ready(ready_file: &Path, child: &mut std::process::Child, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            // Child exited/crashed before signalling readiness — fail fast.
            Ok(Some(_)) | Err(_) => return false,
            Ok(None) => {}
        }
        if ready_file.exists() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The graceful-drain budget (seconds) the daemon wrote into its readiness file
/// — its own resolved `prestop_grace_secs + shutdown_timeout_secs`, reflecting
/// any custom `with_config_loader` the CLI can't see. `None` if the file is
/// absent or doesn't parse (e.g. an app predating the readiness protocol).
fn child_reported_budget(paths: &RuntimePaths) -> Option<u64> {
    std::fs::read_to_string(paths.ready_file())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Write the address-discovery file with `0600` permissions.
///
/// # Errors
///
/// Returns the I/O error if the file cannot be written — a daemon without its
/// discovery file is unreachable to thin clients/agents, so the caller treats
/// this as a failed start.
fn write_addr_file(
    paths: &RuntimePaths,
    pid: u32,
    socket: &Path,
    opts: &ServeOptions,
) -> std::io::Result<()> {
    let addr = AddrFile {
        pid,
        transport: "unix".to_owned(),
        address: socket.display().to_string(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        managed_pg: opts.bundled_pg,
        release: opts.release,
        // Prefer the budget the child reported in its readiness file (its own
        // *resolved* config, including any custom `with_config_loader`); fall
        // back to reconstructing it from the daemon's env/profile only if the
        // child didn't report one. Either way `stop` reads it from here so it
        // never re-derives a budget from a different shell.
        stop_budget_secs: Some(
            child_reported_budget(paths).unwrap_or_else(|| resolved_stop_budget_secs(opts)),
        ),
        // Record the explicit profile (a `restart` override, else this shell's
        // env) so a later `restart` can restore it.
        profile: opts.profile.clone().or_else(env_profile),
    };
    let path = paths.addr_file();
    std::fs::write(&path, addr.to_toml())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    // Persist release/profile in a separate marker too, so a later `restart` can
    // recover them even if `serve.addr` is deleted/corrupted while the daemon is
    // running. Best-effort: the address file is the primary record, so a failure
    // here doesn't fail the start.
    write_mode_file(paths, opts);
    Ok(())
}

/// The daemon's build/profile mode, recorded alongside `serve.addr` (see
/// [`RuntimePaths::mode_file`]) as a resilient fallback for `restart`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModeFile {
    /// Whether the daemon was built/started in release mode.
    #[serde(default)]
    release: bool,
    /// The explicit profile (`AUTUMN_ENV`/`AUTUMN_PROFILE`) the daemon used.
    #[serde(default)]
    profile: Option<String>,
}

/// Write the `serve.mode` marker (`0600`). Best-effort.
fn write_mode_file(paths: &RuntimePaths, opts: &ServeOptions) {
    let mode = ModeFile {
        release: opts.release,
        profile: opts.profile.clone().or_else(env_profile),
    };
    let Ok(toml) = toml::to_string(&mode) else {
        return;
    };
    let path = paths.mode_file();
    if std::fs::write(&path, toml).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// The recorded `serve.mode` for this project, if present and parseable.
fn running_daemon_mode(package: Option<&str>) -> Option<ModeFile> {
    let paths = RuntimePaths::resolve(&project_identity(package)).ok()?;
    let contents = std::fs::read_to_string(paths.mode_file()).ok()?;
    toml::from_str(&contents).ok()
}

/// Stop the running daemon. Returns the exit code.
fn stop(opts: &ServeOptions) -> i32 {
    let paths = resolve_paths(opts.package.as_deref());
    let socket = paths.socket_file();
    let Some(rec) = lifecycle_target(&paths, &socket) else {
        println!("autumn serve: not running");
        return 0;
    };
    // Read the address file up front: even when the app PID is stale, a managed
    // cluster it launched can still be alive (Postgres `setsid`s out of the
    // daemon's process group, so it survives the app's crash/kill).
    let addr = read_addr_file(&paths);
    if !confirmed_running(&rec, &socket, startup_in_progress(&paths)) {
        // A concurrent `autumn serve --daemon` can reclaim this stale pidfile and
        // bring up a successor between the check above and the cleanup below.
        // Bail out (touching nothing) if the pidfile no longer records `rec`, a
        // start is now in progress, or the socket went live — otherwise we'd
        // delete the successor's state and reap its just-started Postgres.
        if process::read_pidfile(&paths.pid_file()).is_some_and(|r| r.pid != rec.pid)
            || startup_in_progress(&paths)
            || socket_is_live(&socket)
        {
            println!("autumn serve: not running (a newer daemon has since started)");
            return 0;
        }
        // The daemon is gone, but if it owned a managed cluster reap that too —
        // otherwise a crash before `stop` leaves Postgres holding the data
        // dir/port until manual cleanup. Reap when the address file says managed
        // *or* is absent (a `--bundled-pg` daemon still in `setup_database` may
        // have started the postmaster before writing `serve.addr`); the reaper
        // is a no-op when there's no `postmaster.pid`, so this is safe for
        // non-managed daemons too.
        if addr.as_ref().is_none_or(|a| a.managed_pg) {
            reap_managed_postgres(&paths);
        }
        println!("autumn serve: not running (removed stale files)");
        cleanup(&paths, &socket);
        return 0;
    }

    // Prefer the budget the daemon recorded at start (resolved from its own
    // env/profile); fall back to recomputing it (release flag + config) only for
    // daemons started before that field was recorded.
    let recorded_release = addr.as_ref().is_some_and(|a| a.release);
    let recorded_budget = addr.as_ref().and_then(|a| a.stop_budget_secs);
    if !process::stop_record(
        &rec,
        stop_timeout(opts, recorded_release, recorded_budget),
        &socket,
    ) {
        // The daemon is still alive and we couldn't signal it (e.g. it is owned
        // by another user — `kill` returned `EPERM`). Do NOT remove its state or
        // report success: that would orphan a running daemon and lie to scripts.
        eprintln!(
            "autumn serve: could not stop the daemon (pid {}); it may be owned by \
             another user or unresponsive. Leaving its state in place.",
            rec.pid
        );
        return 1;
    }
    // A concurrent `start` can reclaim the now-stale pidfile and bring up a
    // successor (even a new managed cluster on the same data dir) while we were
    // draining the old daemon. If the pidfile no longer records the pid we just
    // stopped, that successor owns this namespace — leave its Postgres and its
    // pid/addr/ready files alone instead of reaping/removing them.
    if process::read_pidfile(&paths.pid_file()).is_some_and(|r| r.pid != rec.pid) {
        println!("autumn serve: stopped (a newer daemon has since started)");
        return 0;
    }
    // For a managed-Postgres daemon, reap the cluster too: the app's `on_shutdown`
    // hook can be cancelled when its drain budget is exhausted (or skipped on a
    // forced exit), and Postgres `setsid`s itself so a process-group kill won't
    // reach it. Stop it directly via its `postmaster.pid` so it doesn't keep
    // holding the data dir/port after `stop` reports success. Reap when the
    // address file says managed *or* is absent (a daemon stopped mid-boot, before
    // `serve.addr` was written, can still have a live postmaster); the reaper is
    // a no-op without a `postmaster.pid`.
    if addr.as_ref().is_none_or(|a| a.managed_pg) {
        reap_managed_postgres(&paths);
    }
    cleanup(&paths, &socket);
    println!("autumn serve: stopped");
    0
}

/// The PID of this cluster's live postmaster, or `None` when none is running.
///
/// Reads the data dir's `postmaster.pid` and applies PID-reuse guards: the live
/// process must actually be `postgres` and (where the platform exposes it) be
/// `chdir`'d into *this* data dir, since a postmaster `chdir`s to its data dir.
/// A `postmaster.pid` left by a crashed/`SIGKILL`ed cluster keeps its old PID,
/// which the OS may have recycled — these checks stop us from mistaking an
/// unrelated process (or a *different* cluster) for this one.
fn live_postmaster_pid(data_dir: &Path) -> Option<u32> {
    let pidfile = data_dir.join("postmaster.pid");
    let pid = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.lines().next()?.trim().parse::<u32>().ok())?;
    if !process::is_process_alive(pid) {
        return None;
    }
    if process::process_command_name(pid).is_some_and(|name| name != "postgres") {
        return None;
    }
    if let Some(cwd) = process::process_cwd(pid) {
        let same_dir = std::fs::canonicalize(&cwd)
            .ok()
            .zip(std::fs::canonicalize(data_dir).ok())
            .is_some_and(|(live, ours)| live == ours);
        if !same_dir {
            return None;
        }
    }
    Some(pid)
}

/// Reap a managed Postgres cluster the daemon may have left running. If a live
/// postmaster owns the data dir, requests a fast shutdown and escalates. No-op
/// when the cluster already stopped cleanly (pidfile absent or postmaster gone).
fn reap_managed_postgres(paths: &RuntimePaths) {
    let data_dir = paths.pg_data_dir();
    // Whether or not a postmaster is still up, drop the published URL: this
    // reaper bypasses `ManagedPostgresPoolProvider::stop()` (the only path that
    // normally removes it), so a leftover `.autumn-pg-url` could make a later
    // `task`/`build` attach to a dead — or, if the port was reused, foreign —
    // endpoint instead of starting its own cluster.
    let url_file = data_dir
        .parent()
        .unwrap_or(&data_dir)
        .join(".autumn-pg-url");
    let _ = std::fs::remove_file(&url_file);
    let Some(pid) = live_postmaster_pid(&data_dir) else {
        return;
    };
    process::stop_postmaster(pid, Duration::from_secs(10));
}

/// Managed-Postgres environment for a one-off `autumn task`/`autumn build` run
/// (see [`managed_pg_env`]).
pub struct ManagedPgEnv {
    /// The daemon's cluster data dir, always set so a one-off run and the daemon
    /// agree on the cluster location.
    pub data_dir: PathBuf,
    /// The running cluster's connection URL, set only when a live cluster is
    /// detected — the run then *attaches* instead of starting its own.
    pub attach_url: Option<String>,
}

/// Resolve the managed-Postgres environment for a one-off run of `package` so it
/// shares the serve daemon's cluster. `None` when the project's runtime paths
/// can't be resolved (the run then uses the provider's own per-project default),
/// or when a live postmaster holds the data dir but we can't attach to it (so the
/// child must not be pointed at the locked dir). Otherwise returns the data dir
/// and, when a reachable cluster is published, its attach URL.
#[must_use]
pub fn managed_pg_env(package: Option<&str>) -> Option<ManagedPgEnv> {
    // Respect an explicit operator override: if the environment already pins the
    // managed-PG data dir (e.g. a CI/ops run targeting an isolated cluster), don't
    // clobber it or redirect the run to the CLI's platform cluster. Returning
    // `None` leaves the child to inherit the caller's `AUTUMN_MANAGED_PG_DATA_DIR`.
    if std::env::var_os(MANAGED_PG_DATA_DIR_ENV).is_some() {
        return None;
    }
    let paths = RuntimePaths::resolve(&project_identity(package)).ok()?;
    let data_dir = paths.pg_data_dir();

    // Only attach when a live postmaster actually owns this data dir. The identity
    // guard in `live_postmaster_pid` (PID alive + process is `postgres` + its cwd
    // is this data dir, where observable) prevents trusting a stale `.autumn-pg-url`
    // whose old random port was reused by a foreign listener — a bare TCP probe
    // can't tell our cluster from a stranger.
    if live_postmaster_pid(&data_dir).is_some() {
        // A live postmaster holds the dir. Attach only when its published URL is
        // actually reachable; otherwise don't point the child at the *locked* dir
        // (URL missing/unpublished, or a best-effort publish failure) — starting a
        // second postmaster there would deadlock. Let it use its own default.
        return reachable_published_url(&data_dir).map(|url| ManagedPgEnv {
            data_dir,
            attach_url: Some(url),
        });
    }

    // A daemon start is in flight (the startup lock is held by a live launcher)
    // but it hasn't started its postmaster or published a URL yet — there's no
    // `postmaster.pid` to catch above. Sharing the data dir now would let this
    // run `initdb`/start against the directory the daemon is provisioning and
    // corrupt or deadlock it, so fall back to the child's own default cluster.
    if startup_in_progress(&paths) {
        return None;
    }

    // Nothing holds the data dir: safe to share its location so this run and a
    // future `serve` use the same cluster.
    Some(ManagedPgEnv {
        data_dir,
        attach_url: None,
    })
}

/// The published connection URL for this project's managed cluster, returned only
/// when its endpoint is reachable. Reads the URL file the provider writes next to
/// the data dir, then confirms a live postmaster answers there so a stale
/// `.autumn-pg-url` (crash / PID reuse) can't make a run attach to a dead cluster.
fn reachable_published_url(data_dir: &Path) -> Option<String> {
    // The provider publishes the URL beside the data dir. Keep this filename in
    // sync with `autumn_web::managed_pg::PUBLISHED_URL_FILE` (the CLI doesn't
    // build the `managed-pg` feature, so it can't reference the const).
    let url_file = data_dir.parent().unwrap_or(data_dir).join(".autumn-pg-url");
    let contents = std::fs::read_to_string(url_file).ok()?;
    let url = contents.trim();
    if url.is_empty() || !published_url_reachable(url) {
        return None;
    }
    Some(url.to_owned())
}

/// Whether the `host:port` in a `postgresql://…` URL accepts a TCP connection
/// within a short timeout — a portable, definitive "the cluster is up" probe.
fn published_url_reachable(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Some(hostport) = pg_url_host_port(url) else {
        return false;
    };
    let Ok(addrs) = hostport.to_socket_addrs() else {
        return false;
    };
    addrs
        .take(4)
        .any(|sa| std::net::TcpStream::connect_timeout(&sa, Duration::from_millis(500)).is_ok())
}

/// Extract `host:port` from a `postgresql://[user[:pass]@]host:port/db` URL.
/// Returns `None` unless an explicit numeric port is present (the managed
/// provider always emits one; a unix-socket URL has none and isn't TCP-probable).
fn pg_url_host_port(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?']).next()?;
    // Drop any `user:pass@` userinfo (split at the last '@' before the host).
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let (_, port) = hostport.rsplit_once(':')?;
    if port.parse::<u16>().is_err() {
        return None;
    }
    Some(hostport.to_owned())
}

/// Graceful-stop budget before escalating to `SIGKILL`.
///
/// Prefers `recorded_budget` — the drain budget the daemon resolved from its own
/// environment and profile at start and persisted in the address file — so a
/// `stop` from a different shell can't derive a shorter budget from its own env.
/// For daemons started before that field existed, falls back to recomputing it
/// here: base `autumn.toml` ← `[profile.<name>]` ← `autumn-<profile>.toml`
/// ← `AUTUMN_SERVER__*`, with the profile taken from `AUTUMN_ENV`/`AUTUMN_PROFILE`
/// else inferred (`prod` for a release daemon, else `dev`). A small buffer is
/// added on top of the configured drain.
fn stop_timeout(
    opts: &ServeOptions,
    recorded_release: bool,
    recorded_budget: Option<u64>,
) -> Duration {
    if let Some(secs) = recorded_budget {
        return Duration::from_secs(secs) + STOP_GRACE_BUFFER;
    }
    let base_dir = opts
        .package
        .as_deref()
        .and_then(crate::dev::find_manifest_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let profile = effective_profile(None, recorded_release);
    let (prestop, shutdown) = resolve_shutdown_budget(&base_dir, Some(&profile));
    Duration::from_secs(prestop + shutdown) + STOP_GRACE_BUFFER
}

/// The drain budget (`prestop_grace_secs + shutdown_timeout_secs`) resolved from
/// the *current* (daemon's) environment and profile. Called at start so the
/// value persisted in the address file reflects the daemon's own settings.
fn resolved_stop_budget_secs(opts: &ServeOptions) -> u64 {
    let base_dir = opts
        .package
        .as_deref()
        .and_then(crate::dev::find_manifest_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let profile = effective_profile(opts.profile.as_deref(), opts.release);
    let (prestop, shutdown) = resolve_shutdown_budget(&base_dir, Some(&profile));
    prestop + shutdown
}

/// The active profile: an explicit override (`profile_override`, e.g. a `restart`
/// restoration), then the environment (`AUTUMN_ENV`/`AUTUMN_PROFILE`), then the
/// app's build-mode default (`prod` for a release build, else `dev`).
fn effective_profile(profile_override: Option<&str>, release: bool) -> String {
    profile_override
        .map(ToOwned::to_owned)
        .or_else(env_profile)
        .unwrap_or_else(|| {
            if release {
                "prod".to_owned()
            } else {
                "dev".to_owned()
            }
        })
}

/// The active profile selector from the environment (`AUTUMN_ENV`, then the
/// legacy `AUTUMN_PROFILE`), if set to a non-empty value.
fn env_profile() -> Option<String> {
    std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Resolve `(prestop_grace_secs, shutdown_timeout_secs)` with the app's layering
/// for the given active `profile`. Defaults match the prod/dev profile
/// smart-defaults for these keys.
fn resolve_shutdown_budget(base_dir: &Path, profile: Option<&str>) -> (u64, u64) {
    let mut prestop = 5u64;
    let mut shutdown = 30u64;

    // Base autumn.toml [server], then inline [profile.<name>].server overrides.
    if let Ok(contents) = std::fs::read_to_string(base_dir.join("autumn.toml"))
        && let Ok(table) = toml::from_str::<toml::Table>(&contents)
    {
        apply_server_slice(table.get("server"), &mut prestop, &mut shutdown);
        if let Some(prof) = profile {
            for name in profile_aliases(prof) {
                let section = table
                    .get("profile")
                    .and_then(|p| p.get(name.as_str()))
                    .and_then(|p| p.get("server"));
                apply_server_slice(section, &mut prestop, &mut shutdown);
            }
        }
    }

    // autumn-<profile>.toml [server] overrides. Only the first existing file is
    // loaded, in the app loader's preference order (the explicitly-selected
    // spelling first, else the canonical name), so we match the daemon's budget.
    if let Some(prof) = profile {
        for name in profile_file_lookup(prof) {
            if let Ok(contents) =
                std::fs::read_to_string(base_dir.join(format!("autumn-{name}.toml")))
                && let Ok(table) = toml::from_str::<toml::Table>(&contents)
            {
                apply_server_slice(table.get("server"), &mut prestop, &mut shutdown);
                break;
            }
        }
    }

    // Env overrides win (highest priority; read identically by the daemon).
    if let Some(v) = env_u64("AUTUMN_SERVER__PRESTOP_GRACE_SECS") {
        prestop = v;
    }
    if let Some(v) = env_u64("AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS") {
        shutdown = v;
    }

    (prestop, shutdown)
}

/// Overlay `prestop_grace_secs`/`shutdown_timeout_secs` from a `[server]` slice.
fn apply_server_slice(server: Option<&toml::Value>, prestop: &mut u64, shutdown: &mut u64) {
    let Some(server) = server else { return };
    if let Some(v) = server
        .get("prestop_grace_secs")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u64::try_from(v).ok())
    {
        *prestop = v;
    }
    if let Some(v) = server
        .get("shutdown_timeout_secs")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u64::try_from(v).ok())
    {
        *shutdown = v;
    }
}

/// Inline `[profile.<name>]` section names plus legacy aliases, matching the
/// app's `profile_lookup_names` (all matching sections are merged in order, so
/// the canonical spelling, applied last, wins).
fn profile_aliases(profile: &str) -> Vec<String> {
    match profile.trim().to_ascii_lowercase().as_str() {
        "prod" | "production" => vec!["production".to_owned(), "prod".to_owned()],
        "dev" | "development" => vec!["development".to_owned(), "dev".to_owned()],
        _ => vec![profile.trim().to_owned()],
    }
}

/// Ordered `autumn-<name>.toml` lookup names, mirroring the app's
/// `profile_override_file_lookup_names`: only the first existing file is loaded,
/// preferring the explicitly-selected spelling, else the canonical name first.
fn profile_file_lookup(raw_profile: &str) -> Vec<String> {
    let raw = raw_profile.trim();
    match raw.to_ascii_lowercase().as_str() {
        "production" => vec!["production".to_owned(), "prod".to_owned()],
        "prod" => vec!["prod".to_owned(), "production".to_owned()],
        "development" => vec!["development".to_owned(), "dev".to_owned()],
        "dev" => vec!["dev".to_owned(), "development".to_owned()],
        _ => vec![raw.to_owned()],
    }
}

/// Parse a `u64` env var, ignoring empty or invalid values.
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse::<u64>().ok()
}

/// Report daemon status. Exit code 0 = running, 3 = stopped.
fn status(opts: &ServeOptions) -> i32 {
    let paths = resolve_paths(opts.package.as_deref());
    let socket = paths.socket_file();
    let Some(rec) = lifecycle_target(&paths, &socket) else {
        println!("autumn serve: stopped");
        return 3;
    };
    if confirmed_running(&rec, &socket, startup_in_progress(&paths)) {
        let address =
            read_addr_file(&paths).map_or_else(|| socket.display().to_string(), |a| a.address);
        println!("autumn serve: running (pid {}) on unix:{address}", rec.pid);
        0
    } else {
        println!(
            "autumn serve: stopped (stale pidfile at {})",
            paths.pid_file().display()
        );
        3
    }
}

/// Best-effort removal of the pidfile, address file, mode marker, readiness
/// file, and socket.
fn cleanup(paths: &RuntimePaths, socket: &Path) {
    let _ = std::fs::remove_file(paths.pid_file());
    let _ = std::fs::remove_file(paths.addr_file());
    let _ = std::fs::remove_file(paths.mode_file());
    let _ = std::fs::remove_file(paths.ready_file());
    remove_socket_if_not_live(socket);
}

/// Unlink the socket file only when it is a stale socket we can safely reclaim.
///
/// Two guards mirror the app's own bind-path policy:
/// - Skip a path that is **not a socket** (a regular file or other type the app
///   would refuse to clobber) so we never delete something we didn't create.
/// - Skip a socket still owned by a **live listener** (a `connect` succeeds) —
///   e.g. a foreign daemon, or our child that refused to bind over it and exited
///   — so we don't make that service unreachable.
///
/// After our own daemon exits its socket is a dead socket file (connect refused)
/// and is removed.
#[cfg(unix)]
fn remove_socket_if_not_live(socket: &Path) {
    use std::os::unix::fs::FileTypeExt;
    // Not a socket, or missing/unreadable: leave it untouched (never clobber a
    // regular file we didn't create).
    let Ok(meta) = std::fs::symlink_metadata(socket) else {
        return;
    };
    if !meta.file_type().is_socket() {
        return;
    }
    // Reclaim only a provably-stale socket: a connect refused with no listener
    // (`ECONNREFUSED`) or whose path vanished (`NotFound`). A successful connect
    // (live listener) or `EACCES`/`EPERM` (a mode/ACL-restricted socket whose
    // liveness we can't prove) is left alone, mirroring the app's bind-path
    // refusal so we never unlink a service that's still reachable to someone.
    if std::os::unix::net::UnixStream::connect(socket).is_err_and(|e| {
        matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        )
    }) {
        let _ = std::fs::remove_file(socket);
    }
}

#[cfg(not(unix))]
fn remove_socket_if_not_live(_socket: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(transport: &str, address: &str, managed_pg: bool) -> AddrFile {
        AddrFile {
            pid: 4242,
            transport: transport.to_owned(),
            address: address.to_owned(),
            started_at: 1_700_000_000,
            managed_pg,
            release: false,
            stop_budget_secs: Some(35),
            profile: Some("dev".to_owned()),
        }
    }

    #[test]
    fn addr_file_release_defaults_false_for_legacy_files() {
        // Address files written before the `release` field omit it; parsing must
        // not fail and must default to false.
        let legacy = "pid = 7\ntransport = \"unix\"\naddress = \"/tmp/s.sock\"\n\
                      started_at = 1700000000\nmanaged_pg = false\n";
        let parsed = AddrFile::parse(legacy).expect("parse legacy addr file");
        assert!(!parsed.release);
    }

    #[test]
    fn addr_file_release_roundtrips() {
        let mut a = sample("unix", "/tmp/s.sock", true);
        a.release = true;
        let parsed = AddrFile::parse(&a.to_toml()).expect("parse");
        assert!(parsed.release);
    }

    #[test]
    fn serialize_addr_file_unix_roundtrips() {
        let a = sample("unix", "/run/user/1000/autumn/demo/serve.sock", false);
        let parsed = AddrFile::parse(&a.to_toml()).expect("parse");
        assert_eq!(a, parsed);
    }

    #[test]
    fn serialize_addr_file_tcp_roundtrips() {
        let a = sample("tcp", "127.0.0.1:3000", true);
        let parsed = AddrFile::parse(&a.to_toml()).expect("parse");
        assert_eq!(a, parsed);
    }

    #[test]
    fn addr_file_is_valid_toml() {
        let a = sample("unix", "/tmp/x.sock", false);
        let table: toml::Table = toml::from_str(&a.to_toml()).expect("valid toml");
        for key in [
            "pid",
            "transport",
            "address",
            "started_at",
            "managed_pg",
            "release",
            "stop_budget_secs",
            "profile",
        ] {
            assert!(table.contains_key(key), "missing key {key}");
        }
    }

    #[test]
    fn addr_file_stop_budget_defaults_none_for_legacy_files() {
        // Files written before the field omit it; parsing must default to None.
        let legacy = "pid = 7\ntransport = \"unix\"\naddress = \"/tmp/s.sock\"\n\
                      started_at = 1700000000\nmanaged_pg = false\nrelease = false\n";
        let parsed = AddrFile::parse(legacy).expect("parse legacy addr file");
        assert_eq!(parsed.stop_budget_secs, None);
    }

    #[test]
    fn project_identity_prefers_explicit_package() {
        let id = project_identity(Some("my-svc"));
        // Base name from the explicit package, plus a project-dir hash suffix
        // so unrelated checkouts with the same package name don't collide.
        assert!(id.starts_with("my-svc-"), "got {id}");
        assert!(id.len() > "my-svc-".len());
    }

    #[test]
    fn workspace_anchor_is_stable_from_root_and_member() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize root");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/api\"]\n",
        )
        .expect("write workspace manifest");
        let member = root.join("crates/api");
        std::fs::create_dir_all(&member).expect("create member dir");
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\n",
        )
        .expect("write member manifest");

        // The namespace anchor must be the workspace root whether resolved from
        // the root or from the member directory, so `start -p api` and
        // `stop -p api` from different CWDs target the same daemon.
        assert_eq!(workspace_anchor_from(&root), Some(root.clone()));
        assert_eq!(workspace_anchor_from(&member), Some(root));
    }

    #[test]
    fn workspace_anchor_falls_back_to_nearest_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize root");
        // A standalone crate with no `[workspace]` table anywhere above.
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        assert_eq!(workspace_anchor_from(&nested), Some(root));
    }

    #[test]
    fn pg_url_host_port_extracts_authority() {
        assert_eq!(
            pg_url_host_port("postgresql://postgres:secret@localhost:54213/autumn"),
            Some("localhost:54213".to_owned())
        );
        // No userinfo.
        assert_eq!(
            pg_url_host_port("postgres://127.0.0.1:5432/db"),
            Some("127.0.0.1:5432".to_owned())
        );
        // Missing/invalid port → not TCP-probable.
        assert_eq!(pg_url_host_port("postgresql://localhost/autumn"), None);
        assert_eq!(pg_url_host_port("postgresql://localhost:notaport/db"), None);
        // Not a URL.
        assert_eq!(pg_url_host_port("/var/run/pg.sock"), None);
    }

    #[test]
    fn workspace_anchor_ignores_unrelated_broken_parent_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize root");
        // A broken `Cargo.toml` up the tree that is NOT a workspace root (e.g. a
        // stray/half-written file in a parent dir) must not hijack the anchor.
        std::fs::write(root.join("Cargo.toml"), "this is = not [ valid toml")
            .expect("write broken parent manifest");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).expect("create project dir");
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"proj\"\nversion = \"0.1.0\"\n",
        )
        .expect("write project manifest");
        // The anchor is the standalone project, not the broken parent.
        assert_eq!(workspace_anchor_from(&project), Some(project));
    }

    #[test]
    fn workspace_anchor_stable_when_root_manifest_unparsable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize root");
        // The workspace root manifest is temporarily broken (mid-edit) but still
        // carries the recognizable `[workspace]` header, so it's treated as a
        // transiently-broken root rather than an unrelated broken manifest.
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\n  \"crates/api\"",
        )
        .expect("write broken root manifest");
        let member = root.join("crates/api");
        std::fs::create_dir_all(&member).expect("create member dir");
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\n",
        )
        .expect("write member manifest");
        // The anchor must still be the broken workspace root, not the member,
        // so the namespace doesn't flip while the root manifest is unparsable.
        assert_eq!(workspace_anchor_from(&member), Some(root));
    }

    #[test]
    fn workspace_anchor_broken_root_not_pulled_up_to_outer_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        // An unrelated outer crate sits *above* the (broken) workspace root.
        std::fs::write(
            outer.join("Cargo.toml"),
            "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
        )
        .expect("write outer manifest");
        let ws = outer.join("ws");
        let member = ws.join("crates/api");
        std::fs::create_dir_all(&member).expect("create dirs");
        // The intended workspace root is transiently broken (mid-edit).
        std::fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\nmembers = [\n  \"crates/api\"",
        )
        .expect("write broken ws manifest");
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\n",
        )
        .expect("write member manifest");
        // Anchor must be the broken workspace dir, not the outer manifest above it,
        // or lifecycle commands would target the wrong runtime dir.
        assert_eq!(workspace_anchor_from(&member), Some(ws));
    }

    #[test]
    fn workspace_anchor_inner_broken_root_wins_over_outer_valid_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        // A *valid* outer workspace sits above a transiently-broken inner one.
        std::fs::write(
            outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"inner\"]\n",
        )
        .expect("write outer ws manifest");
        let inner = outer.join("inner");
        let member = inner.join("crates/api");
        std::fs::create_dir_all(&member).expect("create dirs");
        std::fs::write(
            inner.join("Cargo.toml"),
            "[workspace]\nmembers = [\n  \"crates/api\"",
        )
        .expect("write broken inner ws manifest");
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\n",
        )
        .expect("write member manifest");
        // The inner (broken) workspace root the daemon was started under wins; the
        // walk must not return the outer valid workspace.
        assert_eq!(workspace_anchor_from(&member), Some(inner));
    }

    // Env vars touched by these tests; cleared so a polluted outer environment
    // can't skew the budget resolution.
    const SHUTDOWN_ENV: [&str; 4] = [
        "AUTUMN_ENV",
        "AUTUMN_PROFILE",
        "AUTUMN_SERVER__PRESTOP_GRACE_SECS",
        "AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS",
    ];

    fn clear_shutdown_env() -> Vec<(&'static str, Option<&'static str>)> {
        SHUTDOWN_ENV.iter().map(|k| (*k, None)).collect()
    }

    #[test]
    fn shutdown_budget_defaults_when_no_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        temp_env::with_vars(clear_shutdown_env(), || {
            assert_eq!(resolve_shutdown_budget(dir.path(), None), (5, 30));
        });
    }

    #[test]
    fn shutdown_budget_reads_base_server_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nprestop_grace_secs = 10\nshutdown_timeout_secs = 100\n",
        )
        .expect("write");
        temp_env::with_vars(clear_shutdown_env(), || {
            assert_eq!(resolve_shutdown_budget(dir.path(), None), (10, 100));
        });
    }

    #[test]
    fn shutdown_budget_env_override_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nprestop_grace_secs = 10\nshutdown_timeout_secs = 100\n",
        )
        .expect("write");
        let mut vars = clear_shutdown_env();
        vars.push(("AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS", Some("7")));
        temp_env::with_vars(vars, || {
            // Env wins for shutdown; prestop still comes from the file.
            assert_eq!(resolve_shutdown_budget(dir.path(), None), (10, 7));
        });
    }

    #[test]
    fn shutdown_budget_layers_profile_file_and_inline_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nprestop_grace_secs = 10\nshutdown_timeout_secs = 100\n\
             [profile.prod.server]\nprestop_grace_secs = 15\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[server]\nshutdown_timeout_secs = 200\n",
        )
        .expect("write");
        temp_env::with_vars(clear_shutdown_env(), || {
            // prestop from inline [profile.prod], shutdown from autumn-prod.toml.
            assert_eq!(resolve_shutdown_budget(dir.path(), Some("prod")), (15, 200));
        });
    }

    #[test]
    fn profile_aliases_cover_canonical_and_custom() {
        assert_eq!(profile_aliases("production"), vec!["production", "prod"]);
        assert_eq!(profile_aliases("DEV"), vec!["development", "dev"]);
        assert_eq!(profile_aliases("staging"), vec!["staging"]);
    }

    #[test]
    fn profile_file_lookup_prefers_selected_spelling() {
        // Mirrors the app loader: the canonical `prod` is preferred unless the
        // user explicitly selected the `production` spelling.
        assert_eq!(profile_file_lookup("prod"), vec!["prod", "production"]);
        assert_eq!(
            profile_file_lookup("production"),
            vec!["production", "prod"]
        );
        assert_eq!(profile_file_lookup("dev"), vec!["dev", "development"]);
        assert_eq!(profile_file_lookup("staging"), vec!["staging"]);
    }

    #[test]
    fn shutdown_budget_profile_file_prefers_prod_over_production() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nshutdown_timeout_secs = 30\n",
        )
        .expect("write");
        // Both spellings present; with AUTUMN_ENV=prod the app loads autumn-prod
        // first, so our budget must too.
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[server]\nshutdown_timeout_secs = 111\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("autumn-production.toml"),
            "[server]\nshutdown_timeout_secs = 222\n",
        )
        .expect("write");
        temp_env::with_vars(clear_shutdown_env(), || {
            assert_eq!(resolve_shutdown_budget(dir.path(), Some("prod")).1, 111);
        });
    }

    #[test]
    fn effective_profile_prefers_override_then_env_then_build_mode() {
        temp_env::with_vars(
            [("AUTUMN_ENV", None::<&str>), ("AUTUMN_PROFILE", None)],
            || {
                assert_eq!(effective_profile(Some("staging"), false), "staging");
                assert_eq!(effective_profile(None, true), "prod");
                assert_eq!(effective_profile(None, false), "dev");
            },
        );
        temp_env::with_vars(
            [("AUTUMN_ENV", Some("qa")), ("AUTUMN_PROFILE", None::<&str>)],
            || {
                // Env beats the build-mode default; an explicit override beats env.
                assert_eq!(effective_profile(None, true), "qa");
                assert_eq!(effective_profile(Some("explicit"), true), "explicit");
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_running_false_for_dead_pid() {
        let rec = process::PidRecord {
            pid: 2_147_483_640,
            start_time: None,
        };
        assert!(!confirmed_running(
            &rec,
            std::path::Path::new("/nonexistent/serve.sock"),
            false,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_running_true_while_startup_in_progress() {
        // During boot the pidfile exists before the socket binds; a live PID with
        // the startup lock present must read as running (not a dead stale lock).
        let rec = process::PidRecord {
            pid: std::process::id(),
            start_time: None,
        };
        assert!(confirmed_running(
            &rec,
            std::path::Path::new("/nonexistent/serve.sock"),
            true,
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn confirmed_running_true_for_self_via_start_time() {
        // A matching start time is conclusive without needing a live socket.
        let pid = std::process::id();
        let rec = process::PidRecord {
            pid,
            start_time: process::process_start_time(pid),
        };
        assert!(confirmed_running(
            &rec,
            std::path::Path::new("/nonexistent/serve.sock"),
            false,
        ));
    }

    #[test]
    fn lifecycle_target_prefers_pidfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        std::fs::write(paths.pid_file(), "4242 0\n").expect("write pid");
        let rec = lifecycle_target(&paths, &paths.socket_file()).expect("target");
        assert_eq!(rec.pid, 4242);
    }

    #[test]
    fn lifecycle_target_addr_fallback_requires_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        // Address file present, pidfile absent, and no live listener on the
        // socket: we must not treat the recorded PID as the daemon.
        let addr = sample("unix", &paths.socket_file().display().to_string(), false);
        std::fs::write(paths.addr_file(), addr.to_toml()).expect("write addr");
        assert!(lifecycle_target(&paths, &paths.socket_file()).is_none());
    }

    #[test]
    fn child_reported_budget_reads_ready_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        // Absent file → no reported budget (CLI falls back to its own resolve).
        assert_eq!(child_reported_budget(&paths), None);
        std::fs::write(paths.ready_file(), "42").expect("write ready");
        assert_eq!(child_reported_budget(&paths), Some(42));
        // Non-numeric content is ignored rather than misread.
        std::fs::write(paths.ready_file(), "not-a-number").expect("write ready");
        assert_eq!(child_reported_budget(&paths), None);
    }
}
