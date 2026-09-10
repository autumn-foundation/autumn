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

/// Environment variable naming the file an app writes when startup completes.
///
/// Its contents are the app's *resolved* graceful-drain budget in seconds
/// (`prestop_grace_secs + shutdown_timeout_secs`) — see
/// `autumn_web::app::signal_serve_ready`. Both `autumn serve` and `autumn dev`
/// pass it, so a supervisor waits for the budget the app will ACTUALLY drain
/// for, including one a custom `with_config_loader` resolved, instead of
/// reconstructing it from TOML/env.
pub const SERVE_READY_FILE_ENV: &str = "AUTUMN_SERVE_READY_FILE";
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
    /// Process role forwarded to the app binary via `AUTUMN_ROLE` (`"web"`,
    /// `"worker"`, or `"combined"`). `None` leaves the child to resolve its own
    /// role from its environment/config (defaulting to combined).
    pub role: Option<String>,
    /// Job queues this process is pinned to, forwarded to the app binary via
    /// `AUTUMN_JOBS__PIN` (issue #1623, AC3).
    ///
    /// `None` (the default) leaves the variable untouched, so the child resolves
    /// `[jobs] pin` from its own config/environment — today's behavior: drain
    /// every configured queue. `Some(queues)` forwards them. `Some(vec![])` is
    /// **explicitly unpinned**: the app keys off the variable's *presence*, so an
    /// empty `AUTUMN_JOBS__PIN` clears a `[jobs] pin` set in `autumn.toml`. That
    /// distinction has to survive here, or `serve restart` silently re-pins a
    /// daemon that was deliberately started unpinned.
    pub pin: Option<Vec<String>>,
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
    #[cfg(any(unix, windows))]
    let code = run_lifecycle(action, opts);
    #[cfg(not(any(unix, windows)))]
    let code = run_without_lifecycle(action, opts);
    std::process::exit(code);
}

/// The refusal printed on a platform with neither POSIX signals nor the Windows
/// process/service APIs the daemon lifecycle is built on.
///
/// Unix and Windows both run the full lifecycle (#1639); this covers whatever is
/// left. Compiled on every platform so its wording is covered by tests rather
/// than only on the host that prints it.
fn daemon_unsupported_message() -> String {
    format!(
        "autumn serve: the background daemon lifecycle (--daemon / stop / status \
         / restart) is not supported on {}.\n  Plain `autumn serve` (foreground) \
         runs natively here.",
        std::env::consts::OS
    )
}

/// Entry point for a platform without the daemon lifecycle. A plain foreground
/// `autumn serve` still works: it builds and runs the app binary, which binds
/// TCP per its config.
#[cfg(not(any(unix, windows)))]
fn run_without_lifecycle(action: Option<ServeAction>, opts: &ServeOptions) -> i32 {
    if action.is_some() || opts.daemon {
        eprintln!("{}", daemon_unsupported_message());
        return 1;
    }
    start_foreground(opts)
}

/// The daemon lifecycle. Identical on Unix and Windows at this level: the two
/// diverge only in how the daemon's endpoint is chosen and how it is asked to
/// drain, both of which are behind helpers below.
#[cfg(any(unix, windows))]
fn run_lifecycle(action: Option<ServeAction>, opts: &ServeOptions) -> i32 {
    match action {
        None => start(opts),
        Some(ServeAction::Stop) => stop(opts),
        Some(ServeAction::Status) => status(opts),
        Some(ServeAction::Restart) => {
            // A registered Windows service owns this project's lifecycle, so
            // restart it through the Service Control Manager. The daemon path
            // below would stop the service host (its app drains, the host reports
            // `Stopped`) and then launch a DETACHED daemon outside the SCM —
            // reporting a successful restart while leaving the service stopped,
            // the replacement app unsupervised, and the next boot bringing up a
            // second instance beside it.
            if let Some(outcome) =
                crate::service::restart_registered(&project_identity(opts.package.as_deref()))
            {
                return match outcome {
                    Ok(()) => 0,
                    Err(message) => {
                        eprintln!("autumn serve restart: {message}");
                        1
                    }
                };
            }
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
            // Preserve the original daemon's process role: an explicit `--role` on
            // *this* restart wins, otherwise restore what the running daemon
            // recorded. The role lives only in the `serve.mode` marker (not the
            // address file), so a bare `restart` doesn't silently drop the daemon
            // back to the combined default.
            let keep_role = opts
                .role
                .clone()
                .or_else(|| recorded_mode.as_ref().and_then(|m| m.role.clone()));
            // Same for the queue pin (#1623), with the same precedence
            // `keep_profile` uses just above: an explicit `--pin` on *this*
            // restart wins (spelled `autumn serve --pin critical restart` — like
            // `--role`, it is an argument of `serve`, not of `restart`), then the
            // restart shell's own `AUTUMN_JOBS__PIN`, and only then what the
            // running daemon recorded.
            //
            // Consulting the live environment before the recording matters:
            // without it, re-pointing a tier by exporting a new
            // `AUTUMN_JOBS__PIN` and running `restart` would be silently
            // overridden by the pin recorded at the daemon's original start —
            // and re-recorded, making the stale pin sticky forever. Restoring
            // the recording is still what stops a bare `restart` from turning a
            // pinned worker tier into an unpinned one that drains every queue.
            let keep_pin = resolve_restart_pin(
                opts.pin.clone(),
                env_pin(),
                recorded_mode.as_ref().and_then(|m| m.pin.clone()),
            );
            let daemon_opts = ServeOptions {
                daemon: true,
                bundled_pg: keep_managed,
                release: keep_release,
                profile: keep_profile,
                role: keep_role,
                pin: keep_pin,
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

/// Where a running daemon accepts traffic.
///
/// Unix daemons bind a per-project Unix socket the CLI chooses; Windows daemons
/// bind their configured `server.host:port` and report it back through the
/// readiness file. Both are carried by `serve.addr` so a thin client reads one
/// shape on either platform.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonEndpoint {
    /// `"unix"` or `"tcp"`.
    transport: String,
    /// Socket path (unix) or `host:port` (tcp).
    address: String,
}

/// The transports the CLI knows how to probe. An endpoint outside this set is
/// rejected rather than recorded: `status` would otherwise report a daemon it
/// can never confirm, and `stop` would trust an address it cannot reach.
const KNOWN_TRANSPORTS: [&str; 2] = ["unix", "tcp"];

/// How long to wait for a TCP connect when probing a daemon's liveness. Short:
/// the endpoint is on loopback, so anything slower is not a live listener.
const ENDPOINT_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

impl DaemonEndpoint {
    /// Parse a readiness-file endpoint line: `<transport> <address>`.
    ///
    /// Splits on the FIRST space only, so a Unix socket path containing spaces
    /// survives. Returns `None` for an unknown transport or a missing address.
    fn parse(line: &str) -> Option<Self> {
        let (transport, address) = line.trim().split_once(' ')?;
        let address = address.trim();
        if address.is_empty() || !KNOWN_TRANSPORTS.contains(&transport) {
            return None;
        }
        Some(Self {
            transport: transport.to_owned(),
            address: address.to_owned(),
        })
    }

    /// Whether a listener is answering here.
    fn is_live(&self) -> bool {
        match self.transport.as_str() {
            "unix" => socket_is_live(Path::new(&self.address)),
            "tcp" => tcp_is_live(&self.address),
            _ => false,
        }
    }
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

/// Whether a TCP listener is answering at `address` (`host:port`).
///
/// Every resolved address is tried: a daemon bound to `localhost` may be on
/// `::1` while the first resolution is `127.0.0.1`, and reporting "not running"
/// for a daemon that is plainly up would let a second start double-bind.
fn tcp_is_live(address: &str) -> bool {
    use std::net::ToSocketAddrs as _;
    address.to_socket_addrs().is_ok_and(|addrs| {
        addrs
            .into_iter()
            .any(|addr| std::net::TcpStream::connect_timeout(&addr, ENDPOINT_PROBE_TIMEOUT).is_ok())
    })
}

/// The endpoint the CLI *forces* on the daemon, where it chooses one.
///
/// Unix: the per-project socket path, known before the daemon exists. Windows:
/// `None` — the app binds its own configured `server.host`/`port` and reports
/// back, so nothing here can know it in advance.
#[cfg_attr(
    unix,
    allow(
        clippy::unnecessary_wraps,
        reason = "infallible on Unix, `None` on Windows — one signature, two answers"
    )
)]
fn forced_endpoint(paths: &RuntimePaths) -> Option<DaemonEndpoint> {
    #[cfg(unix)]
    {
        Some(DaemonEndpoint {
            transport: "unix".to_owned(),
            address: paths.socket_file().display().to_string(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        None
    }
}

/// The endpoint this project's daemon serves on, when it can be known: the one
/// the CLI forced, else the one a running daemon recorded.
///
/// Used for liveness and identity checks, never to *write* the address file —
/// falling back to `serve.addr` there would let a stale record from a crashed
/// predecessor describe a fresh daemon that bound a different port.
fn daemon_endpoint(paths: &RuntimePaths) -> Option<DaemonEndpoint> {
    forced_endpoint(paths).or_else(|| {
        read_addr_file(paths)
            .and_then(|addr| DaemonEndpoint::parse(&format!("{} {}", addr.transport, addr.address)))
    })
}

/// Whether a listener is answering at the endpoint the CLI *chose* for this
/// project.
///
/// Deliberately [`forced_endpoint`], not [`daemon_endpoint`]. On Unix the two
/// are the same and a live listener on our socket path is conclusive. On Windows
/// `daemon_endpoint` falls back to whatever `serve.addr` recorded — a TCP
/// `host:port` shared with every other project that never changed the default —
/// so a bare connect proves only that *something* is listening. Letting that
/// veto a start would wedge a project whose daemon crashed: any other listener
/// on the port makes `--daemon` refuse and `stop` decline to clean up, with no
/// documented way out. Windows single-instance rests instead on the pidfile,
/// whose start-time identity check is real there, plus the OS refusing a second
/// bind.
fn forced_endpoint_is_live(paths: &RuntimePaths) -> bool {
    forced_endpoint(paths).is_some_and(|endpoint| endpoint.is_live())
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

/// Best-effort check that the daemon serving this project's endpoint is `pid`: a
/// definitive `SO_PEERCRED` match on Linux, otherwise endpoint liveness (so a
/// reused PID that is genuinely not the listener is rejected where the kernel
/// can tell us).
fn endpoint_identity_matches(paths: &RuntimePaths, pid: u32) -> bool {
    endpoint_owner_pid(paths).map_or_else(|| forced_endpoint_is_live(paths), |owner| owner == pid)
}

/// The PID owning this project's daemon endpoint, where the OS will say.
///
/// Only Linux's `SO_PEERCRED` on a Unix socket answers this. Windows records a
/// process start time instead, which makes [`confirmed_running`] conclusive
/// there without needing a peer PID at all.
fn endpoint_owner_pid(paths: &RuntimePaths) -> Option<u32> {
    let endpoint = daemon_endpoint(paths)?;
    (endpoint.transport == "unix")
        .then(|| socket_owner_pid(Path::new(&endpoint.address)))
        .flatten()
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
/// the pidfile is written before the app binds its endpoint, so on macOS/BSD a
/// live PID with no listener yet would otherwise be misread as a dead stale
/// pidfile and removed mid-startup.
fn confirmed_running(
    rec: &process::PidRecord,
    paths: &RuntimePaths,
    startup_in_progress: bool,
) -> bool {
    if !process::is_process_alive(rec.pid) {
        return false;
    }
    match (rec.start_time, process::process_start_time(rec.pid)) {
        // Identity is known on both sides (Linux, Windows): trust the comparison
        // and do *not* fall back to the endpoint — a reused PID with a different
        // start time is stale even if some daemon happens to be listening.
        (Some(recorded), Some(current)) => recorded == current,
        // Identity unknown (no recorded start time, or a platform like macOS
        // that can't report one): a start still in progress means the live PID
        // is our daemon binding its endpoint; otherwise confirm the PID actually
        // owns the endpoint (SO_PEERCRED on Linux, else liveness) so a reused PID
        // isn't accepted.
        //
        // The last arm covers a daemon we can see but not query — a Windows
        // service running as Local System, read from the operator's ordinary
        // shell. Calling that "stale" would report a running service as stopped
        // and let `stop` delete its records; calling it "running" costs only that
        // `stop` then tries and fails to kill it, and correctly refuses.
        _ => {
            startup_in_progress
                || endpoint_identity_matches(paths, rec.pid)
                || process::process_is_out_of_reach(rec.pid)
        }
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
fn lifecycle_target(paths: &RuntimePaths) -> Option<process::PidRecord> {
    let pidfile_rec = process::read_pidfile(&paths.pid_file());
    // A definitive socket owner (Linux `SO_PEERCRED`) is the live daemon. Prefer
    // it over a parseable-but-*stale* pidfile (e.g. one overwritten/restored
    // while the daemon is healthy) so a damaged pidfile doesn't make a running
    // daemon unmanageable. Keep the pidfile record when it matches the owner so
    // its recorded start time is retained.
    if let Some(owner) = endpoint_owner_pid(paths) {
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
    // endpoint (it doesn't here, since `endpoint_owner_pid` was `None`), so this
    // reports "not running" rather than risk signalling a reused PID.
    let addr = read_addr_file(paths)?;
    (endpoint_owner_pid(paths) == Some(addr.pid)).then_some(process::PidRecord {
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
        && confirmed_running(&rec, &paths, startup_in_progress(&paths))
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
    if opts.bundled_pg && !startup_in_progress(&paths) && !forced_endpoint_is_live(&paths) {
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
    // `AUTUMN_DUMP_DATA_FLOW=1` is dispatched in `AppBuilder::run` *before* the
    // server ever binds a listener, and `Command` inherits this process's
    // environment. One left over in the launching shell would make the child
    // print the data-flow manifest and exit 0 -- a `serve` that reports success
    // and serves nothing (#1654 review round 5).
    cmd.env_remove(crate::data_flow::DUMP_ENV);
    // `AUTUMN_DUMP_AGENT_AUTHORITY=1` (#1691) sits one rung further up the same
    // ladder, with the same consequence.
    cmd.env_remove(crate::agents::DUMP_ENV);
    cmd.env_remove(crate::graph::DUMP_ENV);
    // Same hazard, sharper consequence (#1605). `AUTUMN_DB_RETENTION=report|purge`
    // is the internal one-shot mode behind `autumn db retention`, and it is
    // dispatched in `AppBuilder::run` *before* the server binds a listener. One
    // left over in the launching shell -- from an earlier `autumn db retention
    // --purge` in the same terminal, or a wrapping script -- would make
    // `autumn serve` enforce the retention policy and exit instead of serving.
    // Unlike a stray manifest dump, that silently DELETES data. The companion
    // knobs go too: they are inert without the mode variable, but leaving a
    // half-set internal protocol in a server's environment invites the next
    // mode check to trip over it.
    crate::db::retention::clear_inherited_one_shot_env(&mut cmd);
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
    if opts.daemon
        && let Some(paths) = paths
    {
        // The app creates this file right after startup completes; the
        // supervisor polls it to detect readiness without an HTTP probe, then
        // reads back the drain budget and the address the app actually bound.
        cmd.env(SERVE_READY_FILE_ENV, paths.ready_file());
        // The Unix-domain socket is the daemon's private transport (for
        // address-file discovery and the thin client). Only force it for daemon
        // starts: a plain foreground `autumn serve` must stay on its configured
        // `server.host`/`port`, so it remains reachable at the expected TCP
        // address like any production server.
        //
        // Windows has no Unix socket to force, so a Windows daemon keeps its
        // configured `server.host`/`port` — which is also what a self-hosting
        // operator wants — and reports the bound address back through the
        // readiness file instead of being told it in advance.
        #[cfg(unix)]
        {
            // The standard nested-config env var feeds the default loader. We
            // also set a dedicated out-of-band override the framework re-applies
            // *after* config loading, so a custom `with_config_loader` that
            // ignores env can't drop the daemon's socket and strand it on TCP.
            cmd.env("AUTUMN_SERVER__UNIX_SOCKET", paths.socket_file());
            cmd.env("AUTUMN_SERVE_FORCE_UNIX_SOCKET", paths.socket_file());
        }
        // Where there is no `SIGTERM` to send, `autumn serve stop` asks for the
        // drain by creating this file — the same graceful path a signal takes,
        // rather than a `TerminateProcess` that skips `on_shutdown` hooks and
        // orphans a managed Postgres child (#1639).
        #[cfg(not(unix))]
        cmd.env(autumn_web::app::SHUTDOWN_SIGNAL_FILE_ENV, paths.stop_file());
    }
    if opts.bundled_pg
        && let Some(paths) = paths
    {
        cmd.env(MANAGED_PG_DATA_DIR_ENV, paths.pg_data_dir());
    }
    // Select the process role for the spawned app. A worker-role daemon still
    // binds its socket/port for liveness probes, so the daemon plumbing above
    // needs no special-casing — just forward the role the app reads at startup.
    if let Some(role) = &opts.role {
        cmd.env("AUTUMN_ROLE", role);
    }
    // Queue pinning (#1623, AC3). The app already reads a comma-separated
    // `AUTUMN_JOBS__PIN` in `Config::apply_jobs_env_overrides`, so the flag
    // forwards that exact spelling rather than inventing a second one. An empty
    // pin leaves the variable untouched (AC4): the child then reads `[jobs] pin`
    // from its own config, and an inherited `AUTUMN_JOBS__PIN` still applies.
    if let Some(pin) = &opts.pin {
        cmd.env("AUTUMN_JOBS__PIN", pin.join(","));
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
/// concurrent start can't race in during readiness). `Err(())` on failure, after
/// printing the reason, cleaning up and releasing the lock. The pidfile only
/// ever holds the child's pid, so concurrent lifecycle commands never see the
/// launcher.
fn launch_daemon_child(
    binary: &Path,
    paths: &RuntimePaths,
    opts: &ServeOptions,
    socket: &Path,
    working_dir: Option<&Path>,
) -> Result<(std::process::Child, PathBuf), String> {
    // A live listener already owning the endpoint means a daemon is serving here
    // even if the pidfile is missing or stale; refuse so readiness can't latch
    // onto the pre-existing listener (mirrors the pidfile guard).
    if let Some(endpoint) = forced_endpoint(paths)
        && endpoint.is_live()
    {
        return Err(format!(
            "autumn serve: already running (a live server owns {}:{}). \
             Use `autumn serve stop` or `autumn serve restart`.",
            endpoint.transport, endpoint.address
        ));
    }

    // Separate startup lock (claimed with our own pid) so two concurrent starts
    // can't both spawn — but it is NOT the pidfile, so a concurrent `stop`/
    // `status` during startup never signals the launcher.
    let start_lock = start_lock_path(paths);
    match process::acquire_pidfile(&start_lock, std::process::id()) {
        Ok(()) => {}
        Err(AcquireError::AlreadyRunning(existing)) => {
            return Err(format!(
                "autumn serve: another `autumn serve` is starting (pid {existing})."
            ));
        }
        Err(AcquireError::Io(e)) => {
            return Err(format!("autumn serve: cannot take startup lock: {e}"));
        }
    }
    let log = match create_private_log(&paths.log_file()) {
        Ok(f) => f,
        Err(e) => {
            let message = format!(
                "autumn serve: cannot open log file {}: {e}",
                paths.log_file().display()
            );
            let _ = std::fs::remove_file(&start_lock);
            cleanup(paths, socket);
            return Err(message);
        }
    };
    let Ok(log_err) = log.try_clone() else {
        let _ = std::fs::remove_file(&start_lock);
        cleanup(paths, socket);
        return Err("autumn serve: cannot duplicate log handle".to_owned());
    };

    // Clear any readiness file left by a previous run so `wait_for_ready` only
    // returns on the file the child we are about to spawn creates — and reads
    // back THIS child's budget and bound address, not the last one's.
    let _ = std::fs::remove_file(paths.ready_file());
    // Clear a stop request left by a crashed or force-killed predecessor. The
    // app drains as soon as it sees this file, so a stale one would make the
    // daemon we are about to start shut down the moment it boots.
    process::clear_stop_request(&paths.stop_file());

    let mut cmd = base_command(binary, Some(paths), opts);
    // The Service Control Manager starts its hosted process from
    // `C:\Windows\System32`, so the app would look for `autumn.toml` and its
    // asset dirs there. Point it at the project the install recorded.
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
        cmd.env("AUTUMN_MANIFEST_DIR", dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let message = format!("\u{2717} Failed to start daemon: {e}");
            let _ = std::fs::remove_file(&start_lock);
            cleanup(paths, socket);
            return Err(message);
        }
    };
    let pid = child.id();

    // The live-endpoint guard at the top proved no daemon is serving here, and
    // we hold the startup lock, so any existing pidfile is from a daemon that
    // has already exited. On platforms that don't record a process start time
    // (macOS/BSD, or an older pidfile), `acquire_pidfile` can't tell that
    // crashed daemon from an unrelated process that reused its PID, and would
    // reject the start as `AlreadyRunning` — permanently blocking restart until
    // the pidfile is deleted by hand. Since the endpoint is provably dead, clear
    // such an unverifiable pidfile so the acquire below can reclaim it.
    if process::read_pidfile(&paths.pid_file()).is_some_and(|r| r.start_time.is_none()) {
        let _ = std::fs::remove_file(paths.pid_file());
    }

    // Record the real pidfile (child's pid). Keep the startup lock held — the
    // caller releases it only after the daemon is ready, so a concurrent start
    // can't spawn a second child while this one is still binding.
    if let Err(e) = process::acquire_pidfile(&paths.pid_file(), pid) {
        let message = match e {
            AcquireError::AlreadyRunning(existing) => {
                format!("autumn serve: already running (pid {existing}).")
            }
            AcquireError::Io(e) => format!("autumn serve: cannot write pidfile: {e}"),
        };
        process::force_kill_group(pid);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&start_lock);
        cleanup(paths, socket);
        return Err(message);
    }
    Ok((child, start_lock))
}

/// Spawn the server detached into the background and supervise readiness.
fn spawn_daemon(binary: &Path, paths: &RuntimePaths, opts: &ServeOptions) -> i32 {
    match start_supervised(binary, paths, opts, None) {
        Ok(child) => {
            let pid = child.id();
            let where_it_serves = read_addr_file(paths).map_or_else(
                || paths.socket_file().display().to_string(),
                |addr| format!("{}:{}", addr.transport, addr.address),
            );
            println!("autumn serve: started (pid {pid}) on {where_it_serves}");
            println!("  address file: {}", paths.addr_file().display());
            println!("  logs: {}", paths.log_file().display());
            // Detach: leave the running child to be reparented to init on exit.
            // (Dropping the handle does not signal or reap the live process.)
            drop(child);
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// How long a start waits for the daemon to report ready.
#[must_use]
pub fn start_ready_timeout(bundled_pg: bool) -> Duration {
    if bundled_pg {
        READY_TIMEOUT_MANAGED_PG
    } else {
        READY_TIMEOUT
    }
}

/// Launch the app child, wait for it to report ready, and record its pidfile,
/// address file and mode marker. Returns the **live** child on success.
///
/// Shared by `autumn serve --daemon` and the Windows service host, so a
/// service-hosted daemon leaves exactly the state `status` and `stop` expect
/// rather than a second, parallel set of records. `working_dir` overrides where
/// the child runs, which the service host needs because it is started by the
/// Service Control Manager from `C:\Windows\System32`.
///
/// # Errors
///
/// Returns the message to show the operator. Everything this function created is
/// cleaned up first, including a managed Postgres the failed start left holding
/// its data dir.
pub fn start_supervised(
    binary: &Path,
    paths: &RuntimePaths,
    opts: &ServeOptions,
    working_dir: Option<&Path>,
) -> Result<std::process::Child, String> {
    let socket = paths.socket_file();
    let (mut child, start_lock) = launch_daemon_child(binary, paths, opts, &socket, working_dir)?;
    let pid = child.id();
    let log_path = paths.log_file();
    // The startup lock is held until the daemon is ready, then released here on
    // every path so a concurrent start can't race in during the bind window.
    let release_lock = || {
        let _ = std::fs::remove_file(&start_lock);
    };
    // Everything a failed start has to undo, in one place so no exit path
    // forgets a piece.
    let abandon = |child: &mut std::process::Child| {
        // Kill the whole tree: a startup that timed out may have spawned
        // children that `Child::kill()` alone would leave orphaned.
        process::force_kill_group(pid);
        let _ = child.kill();
        let _ = child.wait();
        // A managed Postgres puts itself outside the daemon's group/job, so the
        // sweep above can't reach it and no `serve.addr` was written for a later
        // `stop` to consult. Reap it directly via `postmaster.pid` so a failed
        // start doesn't strand the cluster on the data dir/port.
        if opts.bundled_pg {
            reap_managed_postgres(paths);
        }
        release_lock();
        cleanup(paths, &socket);
    };

    let ready_timeout = start_ready_timeout(opts.bundled_pg);
    if !wait_for_ready(&paths.ready_file(), &mut child, ready_timeout) {
        abandon(&mut child);
        return Err(format!(
            "autumn serve: daemon did not become ready within {}s; see {}",
            ready_timeout.as_secs(),
            log_path.display()
        ));
    }
    if let Err(e) = write_addr_file(paths, pid, opts) {
        // Without the discovery file the daemon is unreachable to clients;
        // treat it like the pidfile failure — stop the child and fail.
        let message = format!(
            "autumn serve: cannot write address file {}: {e}",
            paths.addr_file().display()
        );
        abandon(&mut child);
        return Err(message);
    }
    release_lock();
    Ok(child)
}

/// Ask `child` to drain, wait out `budget_secs`, then force-kill its tree.
/// Returns the child's exit code, or `None` when it could not be reaped.
///
/// The Windows service host's stop path. It asks the same way `autumn serve
/// stop` asks — by creating the cooperative-shutdown file the child watches —
/// so an SCM stop and a CLI stop run one drain, not two.
pub fn stop_child(
    child: &mut std::process::Child,
    paths: &RuntimePaths,
    budget_secs: u64,
) -> Option<i32> {
    let _ = process::create_stop_request(&paths.stop_file());
    let budget = Duration::from_secs(budget_secs) + STOP_GRACE_BUFFER;
    if process::wait_with_timeout(child, budget).is_err() {
        process::force_kill_group(child.id());
        let _ = child.kill();
    }
    child.wait().ok().and_then(|status| status.code())
}

/// The drain budget this daemon recorded at start, from its address file.
#[must_use]
pub fn recorded_stop_budget(paths: &RuntimePaths) -> Option<u64> {
    read_addr_file(paths).and_then(|addr| addr.stop_budget_secs)
}

/// Reap a managed Postgres cluster this project's daemon may have left running.
pub fn reap_managed_postgres_for(paths: &RuntimePaths) {
    reap_managed_postgres(paths);
}

/// Remove this project's daemon records.
pub fn cleanup_daemon_state(paths: &RuntimePaths) {
    cleanup(paths, &paths.socket_file());
}

/// This project's stable identity, as the runtime dirs and the Windows service
/// name are both derived from.
#[must_use]
pub fn project_identity_for(package: Option<&str>) -> String {
    project_identity(package)
}

/// This project's running daemon, as `(pid, "<transport>:<address>")`, or `None`
/// when nothing is running.
///
/// Confirms the same way `status` does — a live process whose recorded identity
/// still matches — so `autumn doctor` cannot report a daemon that `status` calls
/// stopped.
#[must_use]
pub fn running_daemon_summary(package: Option<&str>) -> Option<(u32, String)> {
    let paths = RuntimePaths::resolve(&project_identity(package)).ok()?;
    let rec = lifecycle_target(&paths)?;
    if !confirmed_running(&rec, &paths, startup_in_progress(&paths)) {
        return None;
    }
    let where_it_serves = read_addr_file(&paths)
        .map(|a| format!("{}:{}", a.transport, a.address))
        .or_else(|| daemon_endpoint(&paths).map(|e| format!("{}:{}", e.transport, e.address)))
        .unwrap_or_else(|| "unknown".to_owned());
    Some((rec.pid, where_it_serves))
}

/// The role a daemon started here would actually run, folding in an
/// `AUTUMN_ROLE` selected through the environment.
///
/// `install-service` records this rather than the bare `--role` flag: a service
/// runs as Local System and inherits none of the installing shell's
/// environment, so `AUTUMN_ROLE=worker autumn serve install-service` would
/// otherwise register a service that silently runs the combined role.
#[must_use]
pub fn effective_role_for_record(explicit: Option<String>) -> Option<String> {
    effective_role_from(explicit, std::env::var("AUTUMN_ROLE").ok())
}

/// The queue pin a daemon started here would actually run, folding in an
/// `AUTUMN_JOBS__PIN` selected through the environment. Same reasoning as
/// [`effective_role_for_record`]: without it a pinned worker tier is registered
/// as a service that drains every queue.
#[must_use]
pub fn effective_pin_for_record(explicit: Option<Vec<String>>) -> Option<Vec<String>> {
    effective_pin_from(explicit, std::env::var("AUTUMN_JOBS__PIN").ok())
}

/// The profile selected by this shell's environment, for recording at install./// The profile selected by this shell's environment, for recording at install.
#[must_use]
pub fn env_profile_for_record() -> Option<String> {
    env_profile()
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
    parse_ready_payload(&std::fs::read_to_string(paths.ready_file()).ok()?).0
}

/// The endpoint the daemon reported in its readiness file — where it *actually*
/// bound. `None` for an app predating the address line, or an unreadable file.
fn child_reported_endpoint(paths: &RuntimePaths) -> Option<DaemonEndpoint> {
    parse_ready_payload(&std::fs::read_to_string(paths.ready_file()).ok()?).1
}

/// Parse the readiness file: drain budget on line one, bound endpoint on line
/// two. Both are optional and independent — a corrupt budget must not cost us
/// the address, and an app predating the address line still reports its budget.
///
/// The writer is `autumn_web::app::serve_ready_payload`; this module's tests
/// round-trip against it rather than against a copied literal.
fn parse_ready_payload(contents: &str) -> (Option<u64>, Option<DaemonEndpoint>) {
    let mut lines = contents.lines();
    let budget = lines.next().and_then(|l| l.trim().parse::<u64>().ok());
    let endpoint = lines.next().and_then(DaemonEndpoint::parse);
    (budget, endpoint)
}

/// Write the address-discovery file with `0600` permissions.
///
/// # Errors
///
/// Returns the I/O error if the file cannot be written — a daemon without its
/// discovery file is unreachable to thin clients/agents, so the caller treats
/// this as a failed start.
fn write_addr_file(paths: &RuntimePaths, pid: u32, opts: &ServeOptions) -> std::io::Result<()> {
    // Prefer where the child says it actually bound: `server.port = 0` resolves
    // in the kernel, a socket adopted from a predecessor keeps that process's
    // port, and on Windows the CLI never chose the address at all. Fall back to
    // the endpoint the CLI forced, which covers an app built before the address
    // line existed — and is `None` on Windows, where nothing else knows it.
    let endpoint = child_reported_endpoint(paths)
        .or_else(|| forced_endpoint(paths))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the app did not report the address it bound; rebuild it against \
                 this version of autumn-web",
            )
        })?;
    let addr = AddrFile {
        pid,
        transport: endpoint.transport,
        address: endpoint.address,
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
    /// The process role (`AUTUMN_ROLE`) the daemon was started with, recorded so
    /// `restart` can restore it. `None` when the daemon ran the default combined
    /// role (or for mode files written before this field existed).
    #[serde(default)]
    role: Option<String>,
    /// The job queues (`AUTUMN_JOBS__PIN`) the daemon was pinned to, recorded so
    /// `restart` can restore them. `None` when the daemon ran unpinned (or for
    /// mode files written before this field existed).
    #[serde(default)]
    pin: Option<Vec<String>>,
}

/// Write the `serve.mode` marker (`0600`). Best-effort.
fn write_mode_file(paths: &RuntimePaths, opts: &ServeOptions) {
    let mode = ModeFile {
        release: opts.release,
        profile: opts.profile.clone().or_else(env_profile),
        // Persist the *effective* role: the explicit `--role`, else the
        // `AUTUMN_ROLE` an env-selected daemon inherited, so a later bare
        // `restart` recovers it instead of dropping back to combined.
        role: effective_role_from(opts.role.clone(), std::env::var("AUTUMN_ROLE").ok()),
        // Persist the *effective* queue pin for the same reason (#1623): a bare
        // `restart` must not silently relaunch a pinned worker tier as an
        // unpinned one that drains everything.
        pin: effective_pin_from(opts.pin.clone(), std::env::var("AUTUMN_JOBS__PIN").ok()),
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
    let Some(rec) = lifecycle_target(&paths) else {
        println!("autumn serve: not running");
        return 0;
    };
    // Read the address file up front: even when the app PID is stale, a managed
    // cluster it launched can still be alive (Postgres `setsid`s out of the
    // daemon's process group, so it survives the app's crash/kill).
    let addr = read_addr_file(&paths);
    if !confirmed_running(&rec, &paths, startup_in_progress(&paths)) {
        // A concurrent `autumn serve --daemon` can reclaim this stale pidfile and
        // bring up a successor between the check above and the cleanup below.
        // Bail out (touching nothing) if the pidfile no longer records `rec`, a
        // start is now in progress, or the socket went live — otherwise we'd
        // delete the successor's state and reap its just-started Postgres.
        if process::read_pidfile(&paths.pid_file()).is_some_and(|r| r.pid != rec.pid)
            || startup_in_progress(&paths)
            || forced_endpoint_is_live(&paths)
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
    // How to ask. Unix sends `SIGTERM`; Windows has none, so the daemon was
    // started watching `serve.stop` and creating it requests the identical
    // graceful drain — readiness flip, prestop grace, in-flight drain, then
    // `on_shutdown` hooks. Either way `stop_record` escalates to a force-kill of
    // the process tree only once the recorded budget expires.
    let stop_file = paths.stop_file();
    #[cfg(unix)]
    let request = {
        let _ = &stop_file;
        process::StopRequest::Signal
    };
    #[cfg(not(unix))]
    let request = process::StopRequest::File(&stop_file);
    let outcome = process::stop_record(
        &rec,
        stop_timeout(opts, recorded_release, recorded_budget),
        &request,
    );
    match &outcome {
        process::StopOutcome::Drained => {}
        // Killed after it missed its budget, or without ever being asked. Either
        // way its `on_shutdown` hooks may not have run, so say so rather than
        // print a bare "stopped" — a managed cluster left holding its data dir
        // is exactly what the operator needs to know about.
        process::StopOutcome::Escalated => eprintln!(
            "autumn serve: the daemon (pid {}) did not finish draining within its \
             budget and was force-stopped. Its shutdown hooks may not have run.",
            rec.pid
        ),
        process::StopOutcome::Unreachable(why) => eprintln!(
            "autumn serve: could not ask the daemon (pid {}) to drain ({why}), so \
             it was force-stopped without draining. In-flight requests were cut \
             and its shutdown hooks did not run.",
            rec.pid
        ),
        process::StopOutcome::Failed => {
            // Still alive and we couldn't stop it (e.g. owned by another user).
            // Do NOT remove its state or report success: that would orphan a
            // running daemon and lie to scripts.
            eprintln!(
                "autumn serve: could not stop the daemon (pid {}); it may be owned \
                 by another user or unresponsive. Leaving its state in place.",
                rec.pid
            );
            return 1;
        }
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
    let contents = std::fs::read_to_string(data_dir.join("postmaster.pid")).ok()?;
    let lock = PostmasterLock::parse(&contents)?;
    // The file says which data dir it describes; require it to be ours. Portable
    // (Windows exposes no process cwd), and stronger than the cwd check it
    // replaces because it holds even for a postmaster we cannot open.
    if !lock.describes(data_dir) {
        return None;
    }
    if !process::is_process_alive(lock.pid) {
        return None;
    }
    if process::process_command_name(lock.pid).is_some_and(|name| name != "postgres") {
        return None;
    }
    // Bind the PID to *this* postmaster. Without it a recycled PID whose image
    // happens to be named `postgres` is signalled and force-killed — and the
    // signal goes through a named pipe whose server could be a squatter, from a
    // reaper that runs as Local System inside the service host.
    if let Some(recorded) = lock.start_epoch_secs
        && let Some(live) = process::process_start_epoch_secs(lock.pid)
        && live.abs_diff(recorded) > POSTMASTER_START_SKEW_SECS
    {
        return None;
    }
    // Belt and braces where the platform still offers it.
    if let Some(cwd) = process::process_cwd(lock.pid) {
        let same_dir = std::fs::canonicalize(&cwd)
            .ok()
            .zip(std::fs::canonicalize(data_dir).ok())
            .is_some_and(|(live, ours)| live == ours);
        if !same_dir {
            return None;
        }
    }
    Some(lock.pid)
}

/// How far a live process's creation time may differ from the start time
/// `postmaster.pid` records before we stop believing they are the same process.
///
/// Not zero: `PostgreSQL` writes `time(NULL)` from inside the postmaster, after
/// the process was created, and the OS reports creation time at second
/// resolution. A few seconds of slack keeps a legitimate cluster reapable while
/// still making a recycled PID overwhelmingly unlikely to pass.
const POSTMASTER_START_SKEW_SECS: u64 = 5;

/// The fields of `postmaster.pid` this CLI reads.
///
/// `PostgreSQL`'s lock file is line-oriented and its layout is fixed by
/// `src/include/miscadmin.h`: PID, data directory, start time, port, and so on.
/// Parsing it here — rather than reading only line one — is what lets the reaper
/// confirm both *which cluster* the file describes and *whether the PID is still
/// that postmaster*, on every platform.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PostmasterLock {
    /// Line 1: the postmaster's process id.
    pid: u32,
    /// Line 2: the data directory it opened.
    data_dir: PathBuf,
    /// Line 3: when it started, in Unix-epoch seconds. `None` for a truncated
    /// file (a cluster still writing it, or an old `PostgreSQL`).
    start_epoch_secs: Option<u64>,
}

impl PostmasterLock {
    /// Parse the lock file. `None` when line one is not a PID — the only field
    /// without which nothing can be done.
    fn parse(contents: &str) -> Option<Self> {
        let mut lines = contents.lines();
        let pid = lines.next()?.trim().parse::<u32>().ok()?;
        let data_dir = PathBuf::from(lines.next().unwrap_or_default().trim());
        let start_epoch_secs = lines.next().and_then(|l| l.trim().parse::<u64>().ok());
        Some(Self {
            pid,
            data_dir,
            start_epoch_secs,
        })
    }

    /// Whether this file describes the cluster at `data_dir`.
    ///
    /// Compared after canonicalization so a symlinked or differently-spelled
    /// path still matches. A file with no data-dir line (truncated mid-write) is
    /// accepted: refusing would strand a cluster the reaper exists to clean up.
    fn describes(&self, data_dir: &Path) -> bool {
        if self.data_dir.as_os_str().is_empty() {
            return true;
        }
        std::fs::canonicalize(&self.data_dir)
            .ok()
            .zip(std::fs::canonicalize(data_dir).ok())
            .map_or_else(
                || self.data_dir == data_dir,
                |(theirs, ours)| theirs == ours,
            )
    }
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
pub fn effective_profile(profile_override: Option<&str>, release: bool) -> String {
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

/// The effective process role to record for `restart` recovery: the explicit
/// `--role` when set, else the `AUTUMN_ROLE` value the daemon selected via its
/// environment (trimmed, blank treated as absent).
///
/// `AUTUMN_ROLE=worker autumn serve --daemon` picks the role through the env the
/// child inherits, so `opts.role` is `None`; recording only `opts.role` would let
/// a later bare `autumn serve restart` (from a shell without `AUTUMN_ROLE`)
/// silently relaunch as the combined default. Separated from the env read so the
/// precedence/normalization is unit-testable without mutating the process
/// environment.
fn effective_role_from(explicit: Option<String>, env_value: Option<String>) -> Option<String> {
    explicit.or_else(|| {
        env_value
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// Parse an `AUTUMN_JOBS__PIN` value the way the app does
/// (`Config::apply_jobs_env_overrides`): split on commas, trim, drop blanks — so
/// the CLI and the app never disagree about which queues a daemon was pinned to.
fn parse_pin_value(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The `AUTUMN_JOBS__PIN` in *this* process's environment, if the variable is
/// set at all. `Some(vec![])` for a present-but-empty value — the app treats
/// that as an explicit unpin, not as an absent variable.
fn env_pin() -> Option<Vec<String>> {
    std::env::var("AUTUMN_JOBS__PIN")
        .ok()
        .map(|v| parse_pin_value(&v))
}

/// The queue pin a `restart` relaunches with (issue #1623): the explicit `--pin`
/// on *this* restart, else the restart shell's own `AUTUMN_JOBS__PIN`, else what
/// the running daemon recorded in `serve.mode`.
///
/// The live environment has to come before the recording: without it,
/// re-pointing a worker tier by exporting a new `AUTUMN_JOBS__PIN` and running
/// `restart` is silently overridden by the pin captured at the daemon's original
/// start — and then re-recorded, making the stale pin permanent. Restoring the
/// recording is still what stops a bare `restart` from turning a pinned tier into
/// an unpinned one that drains every queue.
fn resolve_restart_pin(
    explicit: Option<Vec<String>>,
    env: Option<Vec<String>>,
    recorded: Option<Vec<String>>,
) -> Option<Vec<String>> {
    explicit.or(env).or(recorded)
}

/// The effective queue pin to record for `restart` recovery (issue #1623): the
/// explicit `--pin` when given, else the `AUTUMN_JOBS__PIN` the daemon selected
/// via its environment.
///
/// `None` means no pin was chosen at all, so a later `restart` leaves the
/// variable alone and the child reads `[jobs] pin` from its config. `Some(vec![])`
/// means the daemon was *explicitly* unpinned — the app keys off the variable's
/// presence, so `AUTUMN_JOBS__PIN=` clears a `[jobs] pin` from `autumn.toml`.
/// Collapsing those two into `None` would let a bare `restart` bring back a
/// config pin the operator had deliberately overridden.
///
/// Separated from the env read so the precedence/normalization is unit-testable
/// without mutating the process environment.
fn effective_pin_from(
    explicit: Option<Vec<String>>,
    env_value: Option<String>,
) -> Option<Vec<String>> {
    explicit.or_else(|| env_value.map(|v| parse_pin_value(&v)))
}

/// Resolve `(prestop_grace_secs, shutdown_timeout_secs)` with the app's layering
/// for the given active `profile`. Defaults match the prod/dev profile
/// smart-defaults for these keys.
pub fn resolve_shutdown_budget(base_dir: &Path, profile: Option<&str>) -> (u64, u64) {
    resolve_shutdown_budget_from(base_dir, profile, &|key| std::env::var(key).ok())
}

/// [`resolve_shutdown_budget`] with the environment layer supplied by the
/// caller.
///
/// `autumn dev` needs this: it injects a project `.env` into the app child
/// (`start_server`) but deliberately never mutates its own environment, so a
/// budget resolved against `std::env` alone misses an `AUTUMN_SERVER__*`
/// override the child will actually honor — and the parent then force-kills the
/// app in the middle of a valid shutdown, skipping the managed-Postgres teardown
/// this whole mechanism exists to protect (#1616). It passes a `.env`-overlaid
/// lookup instead. Injecting the environment also makes the layering testable
/// without mutating process-global state.
pub fn resolve_shutdown_budget_from(
    base_dir: &Path,
    profile: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> (u64, u64) {
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

    // Env overrides win (highest priority; read identically by the daemon). A
    // malformed or blank value is ignored rather than collapsing the budget to
    // zero, which would hard-kill the app instantly.
    if let Some(v) = env_u64_from(env, "AUTUMN_SERVER__PRESTOP_GRACE_SECS") {
        prestop = v;
    }
    if let Some(v) = env_u64_from(env, "AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS") {
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
    env_u64_from(&|k| std::env::var(k).ok(), key)
}

/// `env_u64` against a caller-supplied environment.
fn env_u64_from(env: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<u64> {
    env(key)?.trim().parse::<u64>().ok()
}

/// Report daemon status. Exit code 0 = running, 3 = stopped.
fn status(opts: &ServeOptions) -> i32 {
    let paths = resolve_paths(opts.package.as_deref());
    let Some(rec) = lifecycle_target(&paths) else {
        println!("autumn serve: stopped");
        return 3;
    };
    if confirmed_running(&rec, &paths, startup_in_progress(&paths)) {
        // `<transport>:<address>` on both platforms — `unix:/run/.../serve.sock`
        // or `tcp:127.0.0.1:3000` — so a script reads one shape everywhere.
        let where_it_serves = read_addr_file(&paths)
            .map(|a| format!("{}:{}", a.transport, a.address))
            .or_else(|| daemon_endpoint(&paths).map(|e| format!("{}:{}", e.transport, e.address)))
            .unwrap_or_else(|| "unknown".to_owned());
        println!(
            "autumn serve: running (pid {}) on {where_it_serves}",
            rec.pid
        );
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
/// file, cooperative-stop request, and socket.
fn cleanup(paths: &RuntimePaths, socket: &Path) {
    let _ = std::fs::remove_file(paths.pid_file());
    let _ = std::fs::remove_file(paths.addr_file());
    let _ = std::fs::remove_file(paths.mode_file());
    let _ = std::fs::remove_file(paths.ready_file());
    // A request left behind would drain the next daemon the moment it boots.
    process::clear_stop_request(&paths.stop_file());
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
    // ── Readiness protocol and the daemon endpoint (issue #1639) ───────────
    //
    // On Unix the CLI chooses the daemon's endpoint (the socket path it forces
    // on the child), so it is known before the daemon exists. On Windows the app
    // binds its configured address and reports back through the readiness file,
    // and this is the parser that reads it. Compiled and tested on every
    // platform so the Windows arm is not the one nobody exercises.

    #[test]
    fn ready_payload_round_trips_the_runtime_writer() {
        // Against the runtime's own writer, not a hand-copied literal: two
        // hand-written halves of a wire format drift, a round trip cannot.
        let written = autumn_web::app::serve_ready_payload(35, "tcp 127.0.0.1:3000");
        let (budget, endpoint) = parse_ready_payload(&written);
        assert_eq!(budget, Some(35));
        assert_eq!(
            endpoint,
            Some(DaemonEndpoint {
                transport: "tcp".to_owned(),
                address: "127.0.0.1:3000".to_owned(),
            })
        );
    }

    #[test]
    fn ready_payload_reads_a_budget_only_file_from_an_older_app() {
        // An app built before the address line still reports its drain budget,
        // and `stop` must keep honouring it rather than fall back to guessing.
        let (budget, endpoint) = parse_ready_payload("35\n");
        assert_eq!(budget, Some(35));
        assert_eq!(endpoint, None);
    }

    #[test]
    fn ready_payload_keeps_a_socket_path_containing_spaces_intact() {
        let written = autumn_web::app::serve_ready_payload(1, "unix /home/a b/serve.sock");
        let (_, endpoint) = parse_ready_payload(&written);
        assert_eq!(
            endpoint.map(|e| e.address),
            Some("/home/a b/serve.sock".to_owned())
        );
    }

    #[test]
    fn ready_payload_rejects_an_unknown_transport() {
        // The transport selects how liveness is probed, so recording one we
        // cannot probe would make `status` report a daemon it can never confirm.
        let (_, endpoint) = parse_ready_payload("5\nquic 127.0.0.1:3000");
        assert_eq!(endpoint, None);
    }

    #[test]
    fn ready_payload_rejects_a_transport_with_no_address() {
        let (_, endpoint) = parse_ready_payload("5\ntcp");
        assert_eq!(endpoint, None);
        let (_, endpoint) = parse_ready_payload("5\ntcp   ");
        assert_eq!(endpoint, None);
    }

    #[test]
    fn ready_payload_survives_a_corrupt_budget_without_losing_the_address() {
        // Half a file is better than none: the address still lets `status` and
        // the discovery file work while `stop` falls back to recomputing.
        let (budget, endpoint) = parse_ready_payload("not-a-number\ntcp 127.0.0.1:9\n");
        assert_eq!(budget, None);
        assert_eq!(endpoint.map(|e| e.address), Some("127.0.0.1:9".to_owned()));
    }

    #[test]
    fn child_reported_endpoint_reads_the_ready_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "proj");
        std::fs::create_dir_all(paths.ready_file().parent().unwrap()).expect("mkdir");
        std::fs::write(
            paths.ready_file(),
            autumn_web::app::serve_ready_payload(12, "tcp 127.0.0.1:4321"),
        )
        .expect("write ready file");
        assert_eq!(
            child_reported_endpoint(&paths).map(|e| e.address),
            Some("127.0.0.1:4321".to_owned())
        );
        assert_eq!(child_reported_budget(&paths), Some(12));
    }

    // ── Endpoint liveness (issue #1639) ────────────────────────────────────

    #[test]
    fn a_bound_tcp_port_reads_as_live_and_a_closed_one_does_not() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        assert!(tcp_is_live(&addr), "a bound port must read as live");
        drop(listener);
        assert!(!tcp_is_live(&addr), "a closed port must not read as live");
    }

    #[test]
    fn an_unresolvable_address_is_not_live_rather_than_hanging() {
        assert!(!tcp_is_live("not a host:port"));
        assert!(!tcp_is_live(""));
    }

    #[test]
    fn an_endpoint_probes_the_transport_it_names() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        assert!(
            DaemonEndpoint {
                transport: "tcp".to_owned(),
                address: addr.clone(),
            }
            .is_live()
        );
        // The same address read as a unix socket path is not a live socket, so a
        // corrupt transport field cannot make a TCP listener look like ours.
        assert!(
            !DaemonEndpoint {
                transport: "unix".to_owned(),
                address: addr,
            }
            .is_live()
        );
    }

    // ── PostgreSQL lock file (issue #1639) ─────────────────────────────────
    //
    // The reaper signals and force-kills the PID this file names, from a path
    // that runs as Local System under a registered service. Binding that PID to
    // *this* cluster is what stops a recycled one being hit.

    #[test]
    fn a_postmaster_lock_reads_pid_data_dir_and_start_time() {
        // The layout is fixed by PostgreSQL's `src/include/miscadmin.h`.
        let lock = PostmasterLock::parse("4242\n/var/lib/pg\n1750000000\n5432\n").expect("parse");
        assert_eq!(lock.pid, 4242);
        assert_eq!(lock.data_dir, Path::new("/var/lib/pg"));
        assert_eq!(lock.start_epoch_secs, Some(1_750_000_000));
    }

    #[test]
    fn a_postmaster_lock_without_a_pid_is_unusable() {
        assert_eq!(PostmasterLock::parse(""), None);
        assert_eq!(PostmasterLock::parse("not-a-pid\n/var/lib/pg\n"), None);
    }

    #[test]
    fn a_truncated_postmaster_lock_still_yields_its_pid() {
        // A cluster caught mid-write must still be reapable — that is the state
        // this reaper exists to clean up.
        let lock = PostmasterLock::parse("4242\n").expect("parse");
        assert_eq!(lock.pid, 4242);
        assert_eq!(lock.start_epoch_secs, None);
        assert!(
            lock.describes(Path::new("/anywhere")),
            "no data dir recorded"
        );
    }

    #[test]
    fn a_postmaster_lock_for_another_cluster_is_rejected() {
        // Without this the reaper would signal whatever PID a foreign lock file
        // happens to name.
        let lock = PostmasterLock::parse("4242\n/some/other/cluster\n1750000000\n").expect("parse");
        assert!(!lock.describes(Path::new("/var/lib/pg")));
    }

    #[test]
    fn a_postmaster_lock_matches_its_own_dir_through_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("pg");
        std::fs::create_dir(&real).expect("mkdir");
        let recorded = std::fs::canonicalize(&real).expect("canonicalize");
        let lock = PostmasterLock::parse(&format!("4242\n{}\n1750000000\n", recorded.display()))
            .expect("parse");
        assert!(lock.describes(&real));
    }

    // ── Windows daemon wiring (issue #1639) ────────────────────────────────

    #[test]
    fn a_daemon_start_hands_the_child_its_readiness_file() {
        // The readiness file is the whole startup handshake — and on Windows the
        // only channel that reports the bound address — so a daemon start must
        // always pass it, on every platform.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "proj");
        let opts = ServeOptions {
            daemon: true,
            ..serve_opts_with_role(None)
        };
        let cmd = base_command(Path::new("/bin/true"), Some(&paths), &opts);
        assert_eq!(
            env_value(&cmd, SERVE_READY_FILE_ENV),
            Some(paths.ready_file().display().to_string())
        );
    }

    #[test]
    fn a_foreground_start_hands_the_child_no_daemon_wiring() {
        // A plain `autumn serve` is not supervised: readiness signalling and a
        // file-triggered drain would both be unowned machinery.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "proj");
        let opts = serve_opts_with_role(None);
        let cmd = base_command(Path::new("/bin/true"), Some(&paths), &opts);
        assert_eq!(env_value(&cmd, SERVE_READY_FILE_ENV), None);
        assert_eq!(
            env_value(&cmd, autumn_web::app::SHUTDOWN_SIGNAL_FILE_ENV),
            None
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_daemon_start_hands_the_child_its_cooperative_stop_file() {
        // Where there is no `SIGTERM`, this variable is the ONLY way `stop` can
        // ask for a graceful drain. Without it every stop would be a hard kill.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "proj");
        let opts = ServeOptions {
            daemon: true,
            ..serve_opts_with_role(None)
        };
        let cmd = base_command(Path::new("app.exe"), Some(&paths), &opts);
        assert_eq!(
            env_value(&cmd, autumn_web::app::SHUTDOWN_SIGNAL_FILE_ENV),
            Some(paths.stop_file().display().to_string())
        );
    }

    #[test]
    fn cleanup_removes_the_stop_request() {
        // A request left behind would drain the next daemon the moment it boots.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "proj");
        std::fs::create_dir_all(paths.stop_file().parent().unwrap()).expect("mkdir");
        std::fs::write(paths.stop_file(), "").expect("seed stop request");
        cleanup(&paths, &paths.socket_file());
        assert!(!paths.stop_file().exists());
    }

    #[test]
    fn daemon_refusal_names_the_platform_and_the_native_alternative() {
        // Unix and Windows both run the lifecycle natively (#1639), so this
        // message is for whatever is left. A developer who hits it needs to know
        // foreground `autumn serve` still works before concluding nothing does.
        let message = daemon_unsupported_message();
        assert!(message.contains(std::env::consts::OS), "{message}");
        assert!(
            message.contains("autumn serve") && message.contains("foreground"),
            "{message}"
        );
    }

    #[test]
    fn daemon_refusal_does_not_send_a_windows_developer_to_wsl2() {
        // The refusal predates #1639, when the lifecycle was Tier 2 on Windows.
        // Pointing at WSL2 now would send a Windows operator away from a journey
        // that runs natively on the machine they are already on.
        let message = daemon_unsupported_message();
        assert!(!message.contains("WSL2"), "{message}");
        assert!(!message.contains("Tier 2"), "{message}");
    }

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
    fn shutdown_budget_consults_an_injected_env_source() {
        // `autumn dev` injects a project `.env` into the app child but never
        // loads it into its own process environment, so a budget resolved
        // against `std::env` alone misses an `AUTUMN_SERVER__*` override the
        // child will actually honor — and the parent then force-kills the app
        // mid-shutdown (#1616, Codex round 2). The resolver therefore takes its
        // environment as a parameter.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nprestop_grace_secs = 10\nshutdown_timeout_secs = 100\n",
        )
        .expect("write");
        let injected = |key: &str| match key {
            "AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS" => Some("250".to_owned()),
            _ => None,
        };
        temp_env::with_vars(clear_shutdown_env(), || {
            assert_eq!(
                resolve_shutdown_budget_from(dir.path(), None, &injected),
                (10, 250),
                "the injected env must override the TOML value"
            );
        });
    }

    #[test]
    fn shutdown_budget_from_falls_back_to_toml_when_the_env_source_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nprestop_grace_secs = 10\nshutdown_timeout_secs = 100\n",
        )
        .expect("write");
        let empty = |_: &str| None;
        temp_env::with_vars(clear_shutdown_env(), || {
            assert_eq!(
                resolve_shutdown_budget_from(dir.path(), None, &empty),
                (10, 100)
            );
        });
    }

    #[test]
    fn shutdown_budget_from_ignores_a_malformed_injected_value() {
        // A `.env` line like `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS=soon` must
        // leave the configured value alone rather than collapsing to zero,
        // which would hard-kill the app instantly.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nprestop_grace_secs = 10\nshutdown_timeout_secs = 100\n",
        )
        .expect("write");
        let junk = |key: &str| match key {
            "AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS" => Some("soon".to_owned()),
            "AUTUMN_SERVER__PRESTOP_GRACE_SECS" => Some(String::new()),
            _ => None,
        };
        temp_env::with_vars(clear_shutdown_env(), || {
            assert_eq!(
                resolve_shutdown_budget_from(dir.path(), None, &junk),
                (10, 100)
            );
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
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        let rec = process::PidRecord {
            pid: 2_147_483_640,
            start_time: None,
        };
        assert!(!confirmed_running(&rec, &paths, false));
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_running_true_while_startup_in_progress() {
        // During boot the pidfile exists before the socket binds; a live PID with
        // the startup lock present must read as running (not a dead stale lock).
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        let rec = process::PidRecord {
            pid: std::process::id(),
            start_time: None,
        };
        assert!(confirmed_running(&rec, &paths, true));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn confirmed_running_true_for_self_via_start_time() {
        // A matching start time is conclusive without needing a live endpoint.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        let pid = std::process::id();
        let rec = process::PidRecord {
            pid,
            start_time: process::process_start_time(pid),
        };
        assert!(confirmed_running(&rec, &paths, false));
    }

    #[test]
    fn lifecycle_target_prefers_pidfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        std::fs::write(paths.pid_file(), "4242 0\n").expect("write pid");
        let rec = lifecycle_target(&paths).expect("target");
        assert_eq!(rec.pid, 4242);
    }

    #[test]
    fn lifecycle_target_addr_fallback_requires_a_live_endpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        // Address file present, pidfile absent, and no live listener on the
        // socket: we must not treat the recorded PID as the daemon.
        let addr = sample("unix", &paths.socket_file().display().to_string(), false);
        std::fs::write(paths.addr_file(), addr.to_toml()).expect("write addr");
        assert!(lifecycle_target(&paths).is_none());
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

    fn serve_opts_with_role(role: Option<&str>) -> ServeOptions {
        ServeOptions {
            package: None,
            daemon: false,
            release: false,
            bundled_pg: false,
            profile: None,
            role: role.map(str::to_owned),
            pin: None,
        }
    }

    /// Look up the value the command would set for `key`, if any. `get_envs`
    /// yields `(key, Some(value))` for sets and `(key, None)` for removals.
    fn env_value(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs().find_map(|(k, v)| {
            (k == std::ffi::OsStr::new(key))
                .then(|| v.map(|v| v.to_string_lossy().into_owned()))
                .flatten()
        })
    }

    #[test]
    fn base_command_clears_an_inherited_data_flow_dump_flag() {
        // #1654 review round 5. `AppBuilder::run` dispatches the data-flow dump
        // before the server binds a listener, and `Command` inherits this
        // process's environment, so an `AUTUMN_DUMP_DATA_FLOW=1` left in the
        // launching shell made the child print a manifest and exit 0 --
        // `autumn serve` reporting success while serving nothing.
        let opts = serve_opts_with_role(None);
        let cmd = base_command(Path::new("/bin/true"), None, &opts);
        let entry = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(crate::data_flow::DUMP_ENV));
        assert_eq!(
            entry,
            Some((std::ffi::OsStr::new(crate::data_flow::DUMP_ENV), None)),
            "the flag must be explicitly removed (present with a None value), not \
             merely absent from the overrides -- absent means inherited: {entry:?}"
        );
    }

    #[test]
    fn base_command_clears_an_inherited_retention_mode() {
        // #1605 review round 10. `AUTUMN_DB_RETENTION=purge` is the internal
        // one-shot mode behind `autumn db retention`, dispatched in
        // `AppBuilder::run` before the server binds a listener. Inherited from
        // the launching shell it turns `autumn serve` into a destructive purge
        // that exits 0 -- the data-flow hazard above, except it deletes data.
        let opts = serve_opts_with_role(None);
        let cmd = base_command(Path::new("/bin/true"), None, &opts);
        for var in [
            "AUTUMN_DB_RETENTION",
            "AUTUMN_DB_RETENTION_DATASET",
            "AUTUMN_RETENTION_DRY_RUN",
        ] {
            let entry = cmd
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(var));
            assert_eq!(
                entry,
                Some((std::ffi::OsStr::new(var), None)),
                "{var} must be explicitly removed (present with a None value), not \
                 merely absent from the overrides -- absent means inherited",
            );
        }
    }

    #[test]
    fn base_command_forwards_role_env() {
        for role in ["web", "worker", "combined"] {
            let opts = serve_opts_with_role(Some(role));
            let cmd = base_command(Path::new("/bin/true"), None, &opts);
            assert_eq!(
                env_value(&cmd, "AUTUMN_ROLE").as_deref(),
                Some(role),
                "serve --role {role} must forward AUTUMN_ROLE={role} to the app binary",
            );
        }
    }

    #[test]
    fn base_command_omits_role_env_by_default() {
        let opts = serve_opts_with_role(None);
        let cmd = base_command(Path::new("/bin/true"), None, &opts);
        // With no explicit role the CLI must not set AUTUMN_ROLE, so the child
        // resolves its own role from its environment/config (combined default).
        assert!(
            cmd.get_envs()
                .all(|(k, _)| k != std::ffi::OsStr::new("AUTUMN_ROLE")),
            "no --role must leave AUTUMN_ROLE unset for the app binary",
        );
    }

    fn serve_opts_with_pin(pin: &[&str]) -> ServeOptions {
        ServeOptions {
            pin: Some(pin.iter().map(|s| (*s).to_owned()).collect()),
            ..serve_opts_with_role(Some("worker"))
        }
    }

    /// Issue #1623, AC3: `autumn serve --pin critical,default` must reach the app
    /// binary. The app reads the pin from `AUTUMN_JOBS__PIN` (comma-separated,
    /// `Config::apply_jobs_env_overrides`), so the CLI forwards it in that exact
    /// spelling rather than inventing a second one.
    #[test]
    fn base_command_forwards_pin_env() {
        let opts = serve_opts_with_pin(&["critical", "default"]);
        let cmd = base_command(Path::new("/bin/true"), None, &opts);
        assert_eq!(
            env_value(&cmd, "AUTUMN_JOBS__PIN").as_deref(),
            Some("critical,default"),
            "serve --pin must forward AUTUMN_JOBS__PIN as a comma-separated list",
        );
    }

    /// AC4: an app that configures nothing new keeps today's behavior. With no
    /// `--pin` the CLI must leave `AUTUMN_JOBS__PIN` entirely alone, so the child
    /// resolves `jobs.pin` from its own config/environment.
    #[test]
    fn base_command_omits_pin_env_by_default() {
        let opts = serve_opts_with_role(None);
        let cmd = base_command(Path::new("/bin/true"), None, &opts);
        assert!(
            cmd.get_envs()
                .all(|(k, _)| k != std::ffi::OsStr::new("AUTUMN_JOBS__PIN")),
            "no --pin must leave AUTUMN_JOBS__PIN unset for the app binary",
        );
    }

    /// A `serve --pin` start must persist the pin into `serve.mode` so a bare
    /// `serve restart` recovers it, mirroring the `--role` path. Dropping it
    /// would silently relaunch a pinned worker tier as an unpinned one — exactly
    /// the starvation #1623 exists to prevent.
    #[test]
    fn write_mode_file_persists_pin_for_restart_recovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        let opts = serve_opts_with_pin(&["critical"]);
        write_mode_file(&paths, &opts);

        let contents = std::fs::read_to_string(paths.mode_file()).expect("read mode file");
        let recovered: ModeFile = toml::from_str(&contents).expect("parse mode file");
        assert_eq!(
            recovered.pin.as_deref(),
            Some(["critical".to_owned()].as_slice()),
            "restart must recover the persisted --pin",
        );
    }

    /// Regression for the review finding on the restart path: re-pointing a
    /// worker tier by exporting a new `AUTUMN_JOBS__PIN` and running
    /// `serve restart` must take the new pin, not be overridden by the one
    /// recorded when the daemon first started (which would also be re-recorded,
    /// making the stale pin permanent). Mirrors `keep_profile`'s precedence.
    #[test]
    fn restart_precedence_prefers_the_flag_then_the_live_env_then_the_recording() {
        let recorded = Some(vec!["critical".to_owned()]);
        let live_env = Some(vec!["bulk".to_owned(), "default".to_owned()]);
        let flag = Some(vec!["reports".to_owned()]);

        assert_eq!(
            resolve_restart_pin(flag.clone(), live_env.clone(), recorded.clone()),
            flag,
            "an explicit --pin on this restart wins",
        );
        assert_eq!(
            resolve_restart_pin(None, live_env.clone(), recorded.clone()),
            live_env,
            "otherwise the restart shell's own AUTUMN_JOBS__PIN wins over the recording — \
             without this, re-pointing a tier by exporting a new pin is overridden by the \
             one captured at the daemon's original start, and then re-recorded",
        );
        assert_eq!(
            resolve_restart_pin(None, None, recorded.clone()),
            recorded,
            "and only with neither does the recorded pin come back, so a bare restart never \
             turns a pinned tier into an unpinned one",
        );
        assert_eq!(
            resolve_restart_pin(None, None, None),
            None,
            "nothing anywhere leaves AUTUMN_JOBS__PIN untouched for the child",
        );
        // An explicit unpin is a real choice at every layer, not an absent one.
        assert_eq!(
            resolve_restart_pin(None, Some(Vec::new()), recorded),
            Some(Vec::new()),
            "an explicitly-empty live pin must not fall through to the recording",
        );
    }

    #[test]
    fn mode_file_pin_defaults_none_for_legacy_files() {
        // Mode files written before the `pin` field omit it; parsing must not
        // fail and must default to None (the daemon ran unpinned).
        let legacy = "release = true\nprofile = \"prod\"\nrole = \"worker\"\n";
        let parsed: ModeFile = toml::from_str(legacy).expect("parse legacy mode file");
        assert_eq!(parsed.pin, None);
    }

    /// Precedence mirrors `effective_role_from`: an explicit `--pin` wins, else
    /// the `AUTUMN_JOBS__PIN` the daemon inherited is recorded so a later bare
    /// `serve restart` from a shell without it still relaunches pinned. Blank
    /// and whitespace-only entries are dropped exactly as the app's own env
    /// parser drops them, so the two never disagree about what was pinned.
    #[test]
    fn effective_pin_prefers_flag_then_env() {
        assert_eq!(
            effective_pin_from(Some(vec!["critical".to_owned()]), Some("bulk".to_owned())),
            Some(vec!["critical".to_owned()]),
            "an explicit --pin must win over an inherited AUTUMN_JOBS__PIN",
        );
        assert_eq!(
            effective_pin_from(None, Some(" critical , , default ".to_owned())),
            Some(vec!["critical".to_owned(), "default".to_owned()]),
            "an inherited AUTUMN_JOBS__PIN is split/trimmed like the app parses it",
        );
        assert_eq!(
            effective_pin_from(None, None),
            None,
            "no flag and no env records no pin",
        );
    }

    /// The app keys off the *presence* of `AUTUMN_JOBS__PIN`, so a
    /// present-but-empty value is an explicit unpin that clears a `[jobs] pin`
    /// from `autumn.toml`. Recording that as "no pin" would let a bare
    /// `serve restart` bring the config pin back and silently change which
    /// queues the daemon drains.
    #[test]
    fn an_explicitly_empty_pin_is_recorded_as_unpinned_not_as_absent() {
        assert_eq!(
            effective_pin_from(None, Some(String::new())),
            Some(Vec::new()),
            "a present-but-empty AUTUMN_JOBS__PIN means explicitly unpinned",
        );
        assert_eq!(
            effective_pin_from(None, Some("  ,  ".to_owned())),
            Some(Vec::new()),
            "a present all-blank AUTUMN_JOBS__PIN is likewise an explicit unpin",
        );

        // …and it must round-trip through the command as an empty variable, not
        // as an absent one.
        let opts = ServeOptions {
            pin: Some(Vec::new()),
            ..serve_opts_with_role(Some("worker"))
        };
        let cmd = base_command(Path::new("/bin/true"), None, &opts);
        assert_eq!(
            env_value(&cmd, "AUTUMN_JOBS__PIN").as_deref(),
            Some(""),
            "an explicit unpin must set AUTUMN_JOBS__PIN to the empty string, not leave it unset",
        );
    }

    #[test]
    fn mode_file_role_defaults_none_for_legacy_files() {
        // Mode files written before the `role` field omit it; parsing must not
        // fail and must default to None (the daemon ran the combined role).
        let legacy = "release = true\nprofile = \"prod\"\n";
        let parsed: ModeFile = toml::from_str(legacy).expect("parse legacy mode file");
        assert_eq!(parsed.role, None);
        assert!(parsed.release);
        assert_eq!(parsed.profile.as_deref(), Some("prod"));
    }

    #[test]
    fn write_mode_file_persists_role_for_restart_recovery() {
        // A `serve --role worker` start must persist the role into `serve.mode`
        // so a bare `serve restart` can recover it (mirroring the profile path)
        // instead of silently dropping back to the combined default.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("dirs");
        let opts = serve_opts_with_role(Some("worker"));
        write_mode_file(&paths, &opts);

        // Recover exactly as `restart`'s `running_daemon_mode` would: read and
        // parse the marker, then take its role.
        let contents = std::fs::read_to_string(paths.mode_file()).expect("read mode file");
        let recovered: ModeFile = toml::from_str(&contents).expect("parse mode file");
        assert_eq!(
            recovered.role.as_deref(),
            Some("worker"),
            "restart must recover the persisted --role",
        );
    }

    #[test]
    fn effective_role_prefers_flag_then_env() {
        // `AUTUMN_ROLE=worker autumn serve --daemon` selects the role via the env
        // the child inherits (so `opts.role` is None); write_mode_file records the
        // effective role through this seam so a bare `serve restart` from a shell
        // without `AUTUMN_ROLE` recovers it instead of relaunching as combined.
        // The crate forbids `unsafe`, so this exercises the pure precedence/
        // normalization core rather than mutating the real process environment.

        // An explicit `--role` always wins over the environment.
        assert_eq!(
            effective_role_from(Some("web".to_owned()), Some("worker".to_owned())).as_deref(),
            Some("web"),
        );
        // No flag: fall back to a non-blank `AUTUMN_ROLE`, trimmed — this is the
        // env-selected-daemon case that `restart` must recover.
        assert_eq!(
            effective_role_from(None, Some("worker".to_owned())).as_deref(),
            Some("worker"),
        );
        assert_eq!(
            effective_role_from(None, Some("  worker  ".to_owned())).as_deref(),
            Some("worker"),
        );
        // A blank or absent `AUTUMN_ROLE` leaves the role unset (combined default),
        // so the CLI never records a bogus empty role.
        assert_eq!(effective_role_from(None, Some("   ".to_owned())), None);
        assert_eq!(effective_role_from(None, None), None);
    }
}
