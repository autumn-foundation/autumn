//! Register an `autumn serve` daemon as an OS-supervised Windows service
//! (issue #1639).
//!
//! `autumn serve --daemon` gives an operator a background process. It does not
//! give them a process that survives a reboot or comes back after a crash —
//! which is the whole reason a Windows shop reaches for a service. This module
//! closes that gap with two commands and no third-party wrapper tool:
//!
//! ```text
//! autumn serve install-service     # build, register, start; auto-start at boot
//! autumn serve uninstall-service   # stop, deregister, remove state
//! ```
//!
//! The registered service runs `autumn serve run-service`, which is this
//! process acting as a Service Control Manager host: it starts the same app
//! child `--daemon` starts, records the same pidfile / address file / mode
//! marker, and on `SERVICE_CONTROL_STOP` asks for the same cooperative drain
//! `autumn serve stop` asks for. So `status` and `stop` keep working against a
//! service-hosted daemon, and the service is an ordinary entry in `services.msc`
//! and `sc.exe`.
//!
//! Everything that decides *policy* — the service name, the restart schedule,
//! the stop wait hint, whether an app exit was intentional — lives above the
//! `#[cfg(windows)]` line and is unit-tested on every platform. Only the Service
//! Control Manager calls themselves are Windows-only.

#![allow(dead_code, clippy::missing_const_for_fn)]

use crate::paths::RuntimeParts;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Lifecycle subcommand for the Windows service journey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    /// Build, register and start the service.
    Install,
    /// Stop, deregister and clean up.
    Uninstall,
    /// Run as the Service Control Manager's hosted process. Not for humans:
    /// `install` records this as the service's command line.
    Run,
}

/// Prefix every Autumn service name carries, so an operator scanning
/// `services.msc` can tell at a glance which entries this tool owns.
const SERVICE_NAME_PREFIX: &str = "autumn-";

/// The Service Control Manager caps a service name at 256 characters and
/// forbids `/` and `\`. Autumn's project identity can contain neither by
/// construction, but it is derived from a directory name, so it is sanitized
/// rather than trusted.
const MAX_SERVICE_NAME: usize = 256;

/// Escalating restart delays applied after the first, second and subsequent
/// failures. Backing off matters: an app that crashes on boot (a bad config, a
/// database that is down) would otherwise be restarted every five seconds
/// forever, filling the event log and hiding the cause.
const RESTART_DELAYS_SECS: [u64; 3] = [5, 15, 60];

/// How long with no failure before the failure count resets. A day, so three
/// crashes spread over a week are not treated as an escalating series.
const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// Headroom added to the app's own drain budget when telling the Service
/// Control Manager how long a stop will take.
///
/// The wait hint is what stops the SCM from declaring a service hung mid-drain.
/// It has to cover the drain plus the `on_shutdown` hooks that run after it —
/// the managed-Postgres teardown being the one this whole slice exists to let
/// finish — so it is sized to the same 60s ceiling `autumn dev`'s cooperative
/// stop uses.
const STOP_HOOK_HEADROOM: Duration = Duration::from_secs(60);

/// Ceiling on the reported wait hint.
///
/// The Service Control Manager carries it as milliseconds in a `u32`, and
/// `windows-service` **panics** converting anything larger. The budget comes
/// from user config, so an absurd `shutdown_timeout_secs` would otherwise take
/// down the service host mid-stop — leaving the service stuck in `StopPending`
/// until the SCM gives up. A day is far beyond any real drain.
const MAX_WAIT_HINT: Duration = Duration::from_secs(24 * 60 * 60);

/// How often the service host checks on its app child and for a stop request.
///
/// Short relative to any real drain: `autumn serve stop` creates the request
/// file and only then waits for the app to drain (a prestop grace plus an
/// in-flight drain, seconds at minimum), so the host reliably observes the
/// request before the child exits and never mistakes an operator's stop for a
/// crash.
const SUPERVISE_POLL: Duration = Duration::from_millis(100);

/// The Windows service name for a project identity (`<name>-<dirhash>`).
///
/// Deterministic, so `install`, `uninstall` and `autumn doctor` agree on which
/// service belongs to this project without the operator naming it — which is
/// what keeps the journey to one command.
#[must_use]
pub fn service_name(project_identity: &str) -> String {
    let sanitized: String = project_identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{SERVICE_NAME_PREFIX}{sanitized}");
    if name.chars().count() <= MAX_SERVICE_NAME {
        return name;
    }
    // Too long. Truncating from the end would cut the `-<dirhash>` suffix —
    // the only part that distinguishes two checkouts sharing a directory name —
    // so two long projects would collide on one SCM entry and the second
    // install, uninstall and status would all target the first's service. Trim
    // the human-readable half instead and keep the hash whole.
    let (readable, hash) = sanitized
        .rsplit_once('-')
        .map_or((sanitized.as_str(), ""), |(head, tail)| (head, tail));
    let suffix = if hash.is_empty() {
        String::new()
    } else {
        format!("-{hash}")
    };
    let room = MAX_SERVICE_NAME
        .saturating_sub(SERVICE_NAME_PREFIX.chars().count())
        .saturating_sub(suffix.chars().count());
    let readable: String = readable.chars().take(room).collect();
    format!("{SERVICE_NAME_PREFIX}{readable}{suffix}")
}

/// The human-facing name shown in `services.msc`.
#[must_use]
pub fn service_display_name(project_identity: &str) -> String {
    format!("Autumn app: {project_identity}")
}

/// The service description shown in `services.msc`.
#[must_use]
pub fn service_description(working_dir: &std::path::Path) -> String {
    format!(
        "Autumn application server for {}. Managed with `autumn serve \
         install-service` / `uninstall-service`; stop it gracefully with \
         `autumn serve stop` or the Service Control Manager.",
        working_dir.display()
    )
}

/// How long to tell the Service Control Manager a stop will take, given the
/// app's own resolved drain budget.
///
/// Under-reporting is the failure that matters: the SCM would mark the service
/// hung and the operator would see a stop "fail" while the app was still
/// draining correctly.
#[must_use]
pub fn stop_wait_hint(drain_budget_secs: u64) -> Duration {
    Duration::from_secs(drain_budget_secs)
        .saturating_add(STOP_HOOK_HEADROOM)
        .min(MAX_WAIT_HINT)
}

/// `ERROR_SERVICE_DOES_NOT_EXIST` — the Service Control Manager's "no such
/// service".
pub const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

/// Whether a failure to open a service means it is simply not registered.
///
/// Only that one error code. Everything else — `ERROR_ACCESS_DENIED` above all,
/// which is what a non-elevated shell gets when it asks for `STOP | DELETE` —
/// must surface as an error, because `uninstall-service` treats "not registered"
/// as licence to delete the daemon's records. Reading a denial as an absence
/// would remove the record of a service that is still registered `AutoStart`,
/// and its next restart would fail with no configuration to read and no way to
/// log why.
///
/// Pure, so the distinction is tested on every platform rather than only on a
/// machine where someone happens to run it unelevated.
#[must_use]
pub fn open_failure_means_absent(raw_os_error: Option<i32>) -> bool {
    raw_os_error == Some(ERROR_SERVICE_DOES_NOT_EXIST)
}

/// Whether a failure to open a service is an elevation problem, so the message
/// can say what to do rather than only what went wrong.
#[must_use]
pub fn open_failure_is_access_denied(raw_os_error: Option<i32>) -> bool {
    /// `ERROR_ACCESS_DENIED`.
    const ACCESS_DENIED: i32 = 5;
    raw_os_error == Some(ACCESS_DENIED)
}

/// Whether a stop has been asked for, given what the last poll saw.
///
/// Latching, and checked BEFORE the child is reaped. `autumn serve stop` creates
/// the request file and only then waits out the drain — seconds at minimum — so
/// the request is always visible first; latching means the cleanup that later
/// removes that file cannot turn an operator's stop into a phantom crash and
/// trigger a restart the operator did not want.
///
/// Pure, so the Windows service host's crash-versus-stop decision is unit-tested
/// on every platform rather than only by a fifteen-minute CI wait.
#[must_use]
pub fn stop_was_requested(
    already_seen: bool,
    control_asked: bool,
    request_file_exists: bool,
) -> bool {
    already_seen || control_asked || request_file_exists
}

/// Whether an app-child exit should be reported to the Service Control Manager
/// as a clean stop (no restart) rather than a failure (restart).
///
/// Two independent signals, because either alone has a gap. A stop *request* was
/// seen means an operator asked — through the SCM or through `autumn serve
/// stop`, which reaches the same file. A zero exit code means the app decided it
/// was done. A crash, a failed boot, or a force-kill satisfies neither, which is
/// exactly when an operator wants the service brought back.
#[must_use]
pub fn exit_is_intentional(stop_requested: bool, exit_code: Option<i32>) -> bool {
    stop_requested || exit_code == Some(0)
}

/// The drain budget to assume when the daemon has not recorded one.
///
/// Never zero. The service host reads `serve.addr`'s `stop_budget_secs`, and a
/// missing or unreadable record — a partial write, or a concurrent
/// `autumn serve stop` that already cleaned up — would otherwise yield `0` and
/// make every subsequent SCM stop a force-kill after the bare grace buffer. That
/// is precisely the "hard kill that skips shutdown hooks" this slice exists to
/// eliminate, reachable through `sc.exe stop`.
pub const FALLBACK_DRAIN_BUDGET_SECS: u64 = 60;

/// The drain budget to record at install time, resolved from the project's own
/// configuration under the profile the service will run.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the Windows service arm, compiled and tested everywhere"
    )
)]
fn install_budget_secs(opts: &crate::serve::ServeOptions) -> u64 {
    let base_dir = opts
        .package
        .as_deref()
        .and_then(crate::dev::find_manifest_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let profile = crate::serve::effective_profile(opts.profile.as_deref(), opts.release);
    let (prestop, shutdown) = crate::serve::resolve_shutdown_budget(&base_dir, Some(&profile));
    prestop.saturating_add(shutdown)
}

/// What `install-service` records for `run-service` to read back.
///
/// A file rather than command-line arguments because the service runs as Local
/// System: it cannot resolve the installing user's `%LOCALAPPDATA%` and must be
/// told the exact directories, and a service command line is awkward to quote
/// and awkward to change. `install` writes it inside the runtime directory,
/// which is already restricted to the owning user, SYSTEM and Administrators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// The registered service name, echoed so the host does not re-derive it.
    pub name: String,
    /// Absolute path of the built app binary the service launches. Resolved at
    /// install time so the service never runs `cargo` at boot.
    pub binary: PathBuf,
    /// The project directory the app runs in.
    pub working_dir: PathBuf,
    /// The exact directories the installing user resolved.
    pub paths: RuntimeParts,
    /// `AUTUMN_ENV` to force, when the install was profile-pinned.
    #[serde(default)]
    pub profile: Option<String>,
    /// Whether the recorded binary is an optimized release build.
    #[serde(default)]
    pub release: bool,
    /// Whether the app supervises a bundled/managed Postgres.
    #[serde(default)]
    pub bundled_pg: bool,
    /// `AUTUMN_ROLE` to force.
    #[serde(default)]
    pub role: Option<String>,
    /// `AUTUMN_JOBS__PIN` to force.
    #[serde(default)]
    pub pin: Option<Vec<String>>,
}

impl ServiceRecord {
    /// Serialize to TOML.
    ///
    /// # Errors
    ///
    /// Returns the serialization error when the record cannot be rendered.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string(self)
    }

    /// Parse from TOML.
    ///
    /// # Errors
    ///
    /// Returns the deserialization error when the contents are malformed.
    pub fn parse(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
}

/// The restart schedule registered with the Service Control Manager, as plain
/// data so the policy is readable and testable without an SCM.
#[must_use]
pub fn restart_delays() -> Vec<Duration> {
    RESTART_DELAYS_SECS
        .iter()
        .copied()
        .map(Duration::from_secs)
        .collect()
}

/// The failure-count reset window registered with the Service Control Manager.
#[must_use]
pub fn failure_reset_period() -> Duration {
    FAILURE_RESET_PERIOD
}

/// Whether a Service Control Manager handle could be opened with the access a
/// registration needs — i.e. whether this shell is elevated enough to install or
/// remove a service.
///
/// Probing the actual capability rather than inspecting the token: an operator
/// cares whether the command will work, and elevation is not the only thing that
/// can deny it (group policy, a restricted service account).
#[cfg(windows)]
#[must_use]
pub fn missing_prerequisites() -> Vec<String> {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    if ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .is_ok()
    {
        return Vec::new();
    }
    vec![
        "administrator rights to register or remove a Windows service \
         (`autumn serve install-service` / `uninstall-service`)"
            .to_owned(),
    ]
}

/// The registered service's name and Service Control Manager state, when this
/// project has one.
#[cfg(windows)]
#[must_use]
pub fn registered_service_state(project_identity: &str) -> Option<(String, String)> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let name = service_name(project_identity);
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager
        .open_service(&name, ServiceAccess::QUERY_STATUS)
        .ok()?;
    let state = service.query_status().ok()?.current_state;
    Some((name, format!("{state:?}")))
}

/// The message printed when a service command is invoked off Windows.
#[must_use]
pub fn unsupported_message(command: &str) -> String {
    format!(
        "autumn serve {command}: Windows services exist only on Windows. On \
         Linux use a systemd unit and on macOS a launchd plist; \
         `autumn serve --daemon` runs the same app under either."
    )
}

#[cfg(not(windows))]
pub fn run(action: ServiceAction, _opts: &crate::serve::ServeOptions) -> i32 {
    let command = match action {
        ServiceAction::Install => "install-service",
        ServiceAction::Uninstall => "uninstall-service",
        ServiceAction::Run => "run-service",
    };
    eprintln!("{}", unsupported_message(command));
    1
}

#[cfg(windows)]
pub use windows_impl::{restart_registered, run};

/// Restart a registered service through the Service Control Manager, when this
/// project has one.
///
/// `None` means no service is registered and the caller should take its ordinary
/// daemon path. Always `None` off Windows.
#[cfg(not(windows))]
#[must_use]
pub fn restart_registered(_project_identity: &str) -> Option<Result<(), String>> {
    None
}

#[cfg(windows)]
mod windows_impl {
    use super::{
        ServiceAction, ServiceRecord, exit_is_intentional, failure_reset_period, restart_delays,
        service_description, service_display_name, service_name, stop_wait_hint,
    };
    use crate::paths::RuntimePaths;
    use crate::process;
    use crate::serve::{self, ServeOptions};
    use std::ffi::OsString;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use windows_service::service::{
        ServiceAccess, ServiceAction as ScAction, ServiceActionType, ServiceControl,
        ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
        ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{service_control_handler, service_dispatcher};

    /// Floor on how long to wait for the Service Control Manager to report a
    /// state change.
    ///
    /// A floor, not a fixed budget: the host waits the app's own drain budget
    /// plus a grace buffer before it force-kills, so a service with a 60-second
    /// budget is still behaving correctly at 65 — and abandoning it at a fixed
    /// 60 would fail `uninstall-service` on a healthy service and leave it
    /// registered. [`scm_stop_timeout`] derives the real deadline from what the
    /// daemon recorded.
    const SCM_TRANSITION_FLOOR: Duration = Duration::from_secs(60);

    /// How long to wait for a stop the Service Control Manager is driving.
    ///
    /// The same hint the host reports to the SCM, so the two agree on what
    /// "still draining" means, floored so a daemon that recorded nothing useful
    /// still gets a reasonable wait.
    fn scm_stop_timeout(paths: Option<&RuntimePaths>) -> Duration {
        let budget = paths
            .and_then(serve::recorded_stop_budget)
            .unwrap_or(super::FALLBACK_DRAIN_BUDGET_SECS);
        stop_wait_hint(budget).max(SCM_TRANSITION_FLOOR)
    }

    /// Where the running service host finds its record. Set before the dispatcher
    /// starts, because the SCM's `service_main` takes no arguments this process
    /// can thread a value through.
    static RECORD_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

    pub fn run(action: ServiceAction, opts: &ServeOptions) -> i32 {
        match action {
            ServiceAction::Install => install(opts),
            ServiceAction::Uninstall => uninstall(opts),
            ServiceAction::Run => host(opts),
        }
    }

    /// Build the app, register the service, and start it.
    fn install(opts: &ServeOptions) -> i32 {
        let identity = serve::project_identity_for(opts.package.as_deref());
        let paths = match RuntimePaths::resolve(&identity) {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!("autumn serve install-service: {e}");
                return 1;
            }
        };
        // Creating the directories HERE, as the installing user, is what gives
        // them an owner-only ACL that still admits SYSTEM — so the Local System
        // service can reach state the user owns.
        if let Err(e) = paths.ensure_dirs() {
            eprintln!("autumn serve install-service: cannot create runtime dirs: {e}");
            return 1;
        }

        // A daemon already holds this project's pidfile, so the service's app
        // child would be refused at start and the service would report a failure
        // the operator has to go read the log to understand. Say it here.
        if let Some((pid, endpoint)) = serve::running_daemon_summary(opts.package.as_deref()) {
            eprintln!(
                "autumn serve install-service: a daemon is already running for this \
                 project (pid {pid}) on {endpoint}. Stop it with `autumn serve stop` \
                 first — the service will start its own."
            );
            return 1;
        }

        eprintln!("\u{1F342} autumn serve install-service\n");
        // Build now, not at boot: a service that ran `cargo` on start would
        // depend on a toolchain, a network and a warm cache at the worst moment.
        if !crate::dev::cargo_build(opts.package.as_deref(), opts.release) {
            eprintln!("\u{2717} Build failed. Fix the errors above and retry.");
            return 1;
        }
        let binary = crate::dev::find_binary(opts.package.as_deref(), opts.release);
        let binary = std::fs::canonicalize(&binary).unwrap_or(binary);
        let working_dir = opts
            .package
            .as_deref()
            .and_then(crate::dev::find_manifest_dir)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

        let name = service_name(&identity);
        let record = ServiceRecord {
            name: name.clone(),
            binary,
            working_dir: working_dir.clone(),
            paths: paths.parts(),
            profile: opts.profile.clone().or_else(serve::env_profile_for_record),
            release: opts.release,
            bundled_pg: opts.bundled_pg,
            // The EFFECTIVE role and pin, not the bare flags. A service runs as
            // Local System and inherits none of the installing shell's
            // environment, so `AUTUMN_ROLE=worker autumn serve install-service`
            // would otherwise register a service that quietly runs combined —
            // and an `AUTUMN_JOBS__PIN` one that drains every queue. This is the
            // same recovery `serve restart` already does through `serve.mode`.
            role: serve::effective_role_for_record(opts.role.clone()),
            pin: serve::effective_pin_for_record(opts.pin.clone()),
        };
        let record_path = paths.service_record_file();
        match record.to_toml() {
            Ok(toml) => {
                if let Err(e) = std::fs::write(&record_path, toml) {
                    eprintln!(
                        "autumn serve install-service: cannot write {}: {e}",
                        record_path.display()
                    );
                    return 1;
                }
            }
            Err(e) => {
                eprintln!("autumn serve install-service: cannot record the service config: {e}");
                return 1;
            }
        }

        if let Err(e) = register(
            &name,
            &identity,
            &working_dir,
            &record_path,
            // The same budget a `--daemon` start allows, so a first boot that
            // provisions a managed cluster is not called a failure.
            serve::start_ready_timeout(opts.bundled_pg),
            // And the same drain budget the app resolved for itself, so a
            // machine shutdown is not shorter than a `stop`.
            stop_wait_hint(super::install_budget_secs(opts)),
        ) {
            eprintln!("autumn serve install-service: {e}");
            return 1;
        }
        println!("autumn serve: registered the Windows service `{name}`");
        println!("  starts at boot, restarts after a crash");
        println!("  logs: {}", paths.log_file().display());
        println!("  address file: {}", paths.addr_file().display());
        println!("  remove it with `autumn serve uninstall-service`");
        0
    }

    /// Create the service entry, set its description and restart policy, start it.
    fn register(
        name: &str,
        identity: &str,
        working_dir: &std::path::Path,
        record_path: &std::path::Path,
        start_timeout: Duration,
        preshutdown_timeout: Duration,
    ) -> Result<(), String> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .map_err(|e| {
            format!(
                "cannot reach the Service Control Manager ({e}). Registering a \
                 service needs an elevated (Administrator) shell."
            )
        })?;
        let info = ServiceInfo {
            name: OsString::from(name),
            display_name: OsString::from(service_display_name(identity)),
            service_type: ServiceType::OWN_PROCESS,
            // The boot-start half of the journey.
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()
                .map_err(|e| format!("cannot locate the autumn executable: {e}"))?,
            launch_arguments: vec![
                OsString::from("serve"),
                OsString::from("run-service"),
                OsString::from("--service-record"),
                record_path.as_os_str().to_os_string(),
            ],
            dependencies: vec![],
            // Local System: the default account, so registration needs no
            // credentials and the journey stays one command. The runtime
            // directory's ACL admits SYSTEM for exactly this reason.
            account_name: None,
            account_password: None,
        };
        let access = ServiceAccess::CHANGE_CONFIG
            | ServiceAccess::START
            | ServiceAccess::STOP
            | ServiceAccess::QUERY_STATUS;
        let service = manager
            .create_service(&info, access)
            .map_err(|e| format!("cannot register the service `{name}`: {e}"))?;
        service
            .set_description(service_description(working_dir))
            .map_err(|e| format!("cannot set the service description: {e}"))?;
        // The crash-recovery half.
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(failure_reset_period()),
                reboot_msg: None,
                command: None,
                actions: Some(
                    restart_delays()
                        .into_iter()
                        .map(|delay| ScAction {
                            action_type: ServiceActionType::Restart,
                            delay,
                        })
                        .collect(),
                ),
            })
            .map_err(|e| format!("cannot set the service restart policy: {e}"))?;
        // Without this, only a *crash* counts as a failure. The host reports a
        // failed app child as a non-zero service exit code, which is the signal
        // that actually distinguishes "the app died" from "an operator stopped
        // it" — so it has to count.
        service
            .set_failure_actions_on_non_crash_failures(true)
            .map_err(|e| format!("cannot arm the service restart policy: {e}"))?;
        // Give the drain the same budget at machine shutdown that it gets from a
        // plain `sc stop`. Best-effort: an older Windows that refuses this still
        // gets the 180-second `PRESHUTDOWN` default, which beats the 5-second
        // `SHUTDOWN` share by a wide margin.
        let _ = service.set_preshutdown_timeout(preshutdown_timeout);
        service
            .start::<&std::ffi::OsStr>(&[])
            .map_err(|e| format!("registered the service but could not start it: {e}"))?;
        // Wait for `Running`, so the command's success means the app is serving
        // rather than that the Service Control Manager accepted a start request.
        // A service whose app fails to boot otherwise reports success here and
        // the operator finds out from the event log.
        let deadline = Instant::now() + start_timeout;
        loop {
            match service.query_status().map(|s| s.current_state) {
                Ok(ServiceState::Running) => return Ok(()),
                Ok(ServiceState::Stopped) => {
                    return Err(format!(
                        "`{name}` is registered but its app did not start. See the \
                         daemon log, then `autumn serve uninstall-service` to \
                         remove it."
                    ));
                }
                Ok(_) => {}
                Err(e) => return Err(format!("cannot query `{name}`: {e}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "`{name}` is registered but did not reach Running within {}s. \
                     See the daemon log.",
                    start_timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Restart this project's registered service through the Service Control
    /// Manager.
    ///
    /// `None` when no service is registered, which tells the caller to take its
    /// ordinary daemon path. Routing through the SCM matters because the naive
    /// `stop()` + `start()` pair does the wrong thing here: the stop makes the
    /// host report `Stopped`, and the start then launches a **detached** daemon
    /// outside the SCM — so the command reports a successful restart while the
    /// service sits stopped and the replacement app has no crash supervision,
    /// and the next boot brings up a second instance beside it.
    pub fn restart_registered(project_identity: &str) -> Option<Result<(), String>> {
        let name = service_name(project_identity);
        // Only when something is actually registered; a missing service is the
        // ordinary daemon case, not an error.
        super::registered_service_state(project_identity)?;
        Some(restart_service(&name))
    }

    /// Stop then start the SCM entry, waiting for each transition.
    fn restart_service(name: &str) -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| {
            format!(
                "cannot reach the Service Control Manager ({e}). Restarting a \
                     service needs an elevated (Administrator) shell."
            )
        })?;
        let service = manager
            .open_service(
                name,
                ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
            )
            .map_err(|e| {
                let raw = match &e {
                    windows_service::Error::Winapi(io) => io.raw_os_error(),
                    _ => None,
                };
                if super::open_failure_is_access_denied(raw) {
                    format!(
                        "cannot open `{name}`: access denied. Restarting a service \
                         needs an elevated (Administrator) shell; nothing was \
                         changed."
                    )
                } else {
                    format!("cannot open `{name}`: {e}")
                }
            })?;
        let paths = crate::paths::RuntimePaths::resolve(&serve::project_identity_for(None)).ok();
        let stop_timeout = scm_stop_timeout(paths.as_ref());
        if service
            .query_status()
            .map_err(|e| format!("cannot query `{name}`: {e}"))?
            .current_state
            != ServiceState::Stopped
        {
            service
                .stop()
                .map_err(|e| format!("cannot stop `{name}`: {e}"))?;
            wait_for_state(&service, name, ServiceState::Stopped, stop_timeout)?;
        }
        service
            .start::<&std::ffi::OsStr>(&[])
            .map_err(|e| format!("cannot start `{name}`: {e}"))?;
        // The same budget an install allows, so a restart that re-provisions a
        // managed cluster is not called a failure.
        wait_for_state(
            &service,
            name,
            ServiceState::Running,
            serve::start_ready_timeout(true),
        )?;
        println!("autumn serve: restarted the Windows service `{name}`");
        Ok(())
    }

    /// Poll until `service` reaches `wanted`, or `timeout` elapses.
    fn wait_for_state(
        service: &windows_service::service::Service,
        name: &str,
        wanted: ServiceState,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = service
                .query_status()
                .map_err(|e| format!("cannot query `{name}`: {e}"))?
                .current_state;
            if state == wanted {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "`{name}` did not reach {wanted:?} within {}s; it is {state:?}",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Stop the service, deregister it, and clean up everything it left behind.
    fn uninstall(opts: &ServeOptions) -> i32 {
        let identity = serve::project_identity_for(opts.package.as_deref());
        let name = service_name(&identity);
        let paths = RuntimePaths::resolve(&identity).ok();

        let deregistered = match deregister(&name, scm_stop_timeout(paths.as_ref())) {
            Ok(Deregistered::Removed | Deregistered::WasNotRegistered) => true,
            Err(e) => {
                eprintln!("autumn serve uninstall-service: {e}");
                false
            }
        };
        // Only once the SCM entry is gone. Removing the record while the service
        // is still registered `AutoStart` leaves a host that cannot read its own
        // configuration, cannot log why, and is restarted forever by the very
        // failure policy this command was asked to remove.
        if !deregistered {
            eprintln!(
                "autumn serve uninstall-service: `{name}` is still registered, so \
                 its state was left in place. Stop it (`sc.exe stop {name}`) and \
                 retry."
            );
            return 1;
        }
        if let Some(paths) = paths.as_ref() {
            // A managed cluster outlives the app when a stop had to escalate, so
            // reap it before removing the records that identify it. The data
            // directory is deliberately kept — the operator's database is not
            // this command's to delete.
            serve::reap_managed_postgres_for(paths);
            serve::cleanup_daemon_state(paths);
            let _ = std::fs::remove_file(paths.service_record_file());
        }
        println!("autumn serve: removed the Windows service `{name}`");
        if let Some(paths) = paths.as_ref() {
            println!(
                "  the managed-Postgres data directory was kept: {}",
                paths.pg_data_dir().display()
            );
        }
        0
    }

    /// Stop (if running) and delete the SCM entry.
    /// What [`deregister`] found.
    enum Deregistered {
        /// The SCM entry was there and is now gone.
        Removed,
        /// There was no such service — an uninstall of a half-installed project,
        /// which still has state worth cleaning up.
        WasNotRegistered,
    }

    fn deregister(name: &str, stop_timeout: Duration) -> Result<Deregistered, String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| {
            format!(
                "cannot reach the Service Control Manager ({e}). Removing a \
                     service needs an elevated (Administrator) shell."
            )
        })?;
        let service = match manager.open_service(
            name,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        ) {
            Ok(service) => service,
            Err(e) => {
                let raw = match &e {
                    windows_service::Error::Winapi(io) => io.raw_os_error(),
                    _ => None,
                };
                // Nothing registered. Not an error: the operator asked for it
                // gone and it is gone, and the daemon state is still ours to
                // clean up.
                if super::open_failure_means_absent(raw) {
                    return Ok(Deregistered::WasNotRegistered);
                }
                // Anything else — a denial above all — must NOT reach the
                // cleanup path. Deleting the record of a service that is still
                // registered `AutoStart` leaves a host that cannot read its own
                // configuration on the next restart, and cannot log why.
                if super::open_failure_is_access_denied(raw) {
                    return Err(format!(
                        "cannot open `{name}`: access denied. Removing a service \
                         needs an elevated (Administrator) shell; nothing was \
                         changed."
                    ));
                }
                return Err(format!("cannot open `{name}`: {e}"));
            }
        };
        let state = |service: &windows_service::service::Service| {
            service.query_status().map(|s| s.current_state)
        };
        if state(&service).map_err(|e| format!("cannot query `{name}`: {e}"))?
            != ServiceState::Stopped
        {
            // Stop it through the SCM so the host runs its own drain — the same
            // one `autumn serve stop` triggers — rather than being killed with
            // its `on_shutdown` hooks unrun.
            service
                .stop()
                .map_err(|e| format!("cannot stop `{name}`: {e}"))?;
            let deadline = Instant::now() + stop_timeout;
            while state(&service).unwrap_or(ServiceState::Stopped) != ServiceState::Stopped {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "`{name}` did not stop within {}s; it was left registered \
                         so its state is not removed under a running service",
                        stop_timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        service
            .delete()
            .map(|()| Deregistered::Removed)
            .map_err(|e| format!("cannot deregister `{name}`: {e}"))
    }

    /// Static callback the Service Control Manager bootstraps the service through.
    ///
    /// Both arguments are deliberately ignored, which is what keeps this function
    /// safe under the workspace's `forbid(unsafe_code)`: reading `argv` would
    /// need a raw-pointer dereference, and there is nothing there this host
    /// needs. Its configuration comes from the record file named on this
    /// process's own command line, which is stable across every `sc start`.
    extern "system" fn ffi_service_main(_argc: u32, _argv: *mut *mut u16) {
        service_main();
    }

    /// The service's entry point, once the SCM has bootstrapped it.
    fn service_main() {
        if let Err(e) = supervise() {
            // Nothing is attached to stderr inside a service, so record the
            // failure where the operator is already told to look.
            log_host_failure(&e);
        }
    }

    /// Append a host-level failure to the daemon log.
    fn log_host_failure(message: &str) {
        let Some(record) = read_record() else {
            return;
        };
        let paths = RuntimePaths::from_parts(record.paths);
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log_file())
        {
            use std::io::Write as _;
            let _ = writeln!(log, "autumn serve run-service: {message}");
        }
    }

    /// Read the record this host was pointed at.
    fn read_record() -> Option<ServiceRecord> {
        let path = RECORD_PATH.get()?;
        ServiceRecord::parse(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Enter the Service Control Manager dispatcher. Blocks until the service
    /// stops.
    fn host(opts: &ServeOptions) -> i32 {
        let Some(path) = record_path_from_argv() else {
            eprintln!(
                "autumn serve run-service: this command is run by the Windows \
                 Service Control Manager, not by hand. Use `autumn serve \
                 install-service`."
            );
            return 1;
        };
        let _ = opts;
        let Ok(record) = ServiceRecord::parse(&std::fs::read_to_string(&path).unwrap_or_default())
        else {
            eprintln!(
                "autumn serve run-service: cannot read the service record at {}. \
                 Re-run `autumn serve install-service`.",
                path.display()
            );
            return 1;
        };
        let _ = RECORD_PATH.set(path);
        match service_dispatcher::start(&record.name, ffi_service_main) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("autumn serve run-service: {e}");
                1
            }
        }
    }

    /// The `--service-record <path>` value from this process's command line.
    fn record_path_from_argv() -> Option<std::path::PathBuf> {
        let mut args = std::env::args_os();
        while let Some(arg) = args.next() {
            if arg == "--service-record" {
                return args.next().map(std::path::PathBuf::from);
            }
        }
        None
    }

    /// Watch the app until it exits or a stop is asked for.
    ///
    /// Returns its exit code (`None` when it could not be reaped) and whether a
    /// stop was ever requested — the pair [`exit_is_intentional`] grades.
    fn watch_app(
        child: &mut std::process::Child,
        paths: &RuntimePaths,
        stop_file: &std::path::Path,
        stop_requested: &AtomicBool,
        budget: u64,
        report: &impl Fn(ServiceState, ServiceExitCode, Duration) -> Result<(), String>,
    ) -> Result<(Option<i32>, bool), String> {
        let mut announced_stop = false;
        let mut saw_stop_request = false;
        loop {
            // Check for the ask BEFORE reaping the child: `autumn serve stop`
            // creates the request and only then waits out the drain, so the
            // request is always visible first — and latching it here means the
            // later cleanup that removes the file cannot turn an operator's stop
            // into a phantom crash.
            saw_stop_request = super::stop_was_requested(
                saw_stop_request,
                stop_requested.load(Ordering::SeqCst),
                stop_file.exists(),
            );
            if saw_stop_request && !announced_stop {
                // Tell the Service Control Manager how long this will take, or it
                // declares a correctly-draining service hung.
                report(
                    ServiceState::StopPending,
                    ServiceExitCode::Win32(0),
                    stop_wait_hint(budget),
                )?;
                announced_stop = true;
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok((status.code(), saw_stop_request)),
                // The child cannot be reaped; treat it as gone rather than spin.
                Err(_) => return Ok((None, saw_stop_request)),
                Ok(None) => {}
            }
            if saw_stop_request {
                // Enforce the app's own budget: `stop_child` escalates to a
                // force-kill of the process tree once it expires, exactly as
                // `autumn serve stop` does.
                return Ok((serve::stop_child(child, paths, budget), true));
            }
            std::thread::sleep(super::SUPERVISE_POLL);
        }
    }

    /// Register the Service Control Manager handler.
    ///
    /// It must return promptly, so it only records the ask — creating the same
    /// cooperative-shutdown file `autumn serve stop` creates, so an SCM stop and
    /// a CLI stop run one drain path rather than two. Reporting `StopPending` and
    /// driving the drain is [`watch_app`]'s job.
    fn register_control_handler(
        name: &str,
        stop_requested: std::sync::Arc<AtomicBool>,
        stop_file: std::path::PathBuf,
    ) -> Result<service_control_handler::ServiceStatusHandle, String> {
        let handler = move |control| match control {
            ServiceControl::Interrogate => {
                service_control_handler::ServiceControlHandlerResult::NoError
            }
            ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
                let _ = process::create_stop_request(&stop_file);
                stop_requested.store(true, Ordering::SeqCst);
                service_control_handler::ServiceControlHandlerResult::NoError
            }
            _ => service_control_handler::ServiceControlHandlerResult::NotImplemented,
        };
        service_control_handler::register(name, handler)
            .map_err(|e| format!("cannot register the service control handler: {e}"))
    }

    /// Start the app, report `Running`, then supervise until stopped or the app
    /// exits.
    fn supervise() -> Result<(), String> {
        let Some(record) = read_record() else {
            return Err("the service record could not be read".to_owned());
        };
        let paths = RuntimePaths::from_parts(record.paths.clone());
        let stop_file = paths.stop_file();

        let stop_requested = std::sync::Arc::new(AtomicBool::new(false));
        let status_handle = register_control_handler(
            &record.name,
            std::sync::Arc::clone(&stop_requested),
            stop_file.clone(),
        )?;

        let opts = ServeOptions {
            package: None,
            daemon: true,
            release: record.release,
            bundled_pg: record.bundled_pg,
            profile: record.profile.clone(),
            role: record.role.clone(),
            pin: record.pin.clone(),
        };
        let report = |state: ServiceState, exit: ServiceExitCode, wait_hint: Duration| {
            status_handle
                .set_service_status(ServiceStatus {
                    service_type: ServiceType::OWN_PROCESS,
                    current_state: state,
                    // `PRESHUTDOWN`, not `SHUTDOWN`. A machine shutdown gives
                    // every `SHUTDOWN`-accepting service a share of
                    // `WaitToKillServiceTimeout` — 5 seconds by default — and no
                    // wait hint extends it, so an app with a 30-second drain
                    // would be terminated mid-drain on every reboot and its
                    // managed cluster left for WAL recovery. `PRESHUTDOWN` runs
                    // earlier, with a 180-second default we raise to cover the
                    // app's own budget. The two flags are mutually exclusive.
                    controls_accepted: if state == ServiceState::Running {
                        ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN
                    } else {
                        ServiceControlAccept::empty()
                    },
                    exit_code: exit,
                    checkpoint: 0,
                    wait_hint,
                    process_id: None,
                })
                .map_err(|e| format!("cannot report service status: {e}"))
        };

        report(
            ServiceState::StartPending,
            ServiceExitCode::Win32(0),
            serve::start_ready_timeout(record.bundled_pg),
        )?;
        let mut child =
            match serve::start_supervised(&record.binary, &paths, &opts, Some(&record.working_dir))
            {
                Ok(child) => child,
                Err(message) => {
                    log_host_failure(&message);
                    report(
                        ServiceState::Stopped,
                        // A start that never produced a serving app is a failure, so
                        // the SCM's restart policy applies.
                        ServiceExitCode::ServiceSpecific(1),
                        Duration::default(),
                    )?;
                    return Ok(());
                }
            };
        report(
            ServiceState::Running,
            ServiceExitCode::Win32(0),
            Duration::default(),
        )?;

        // Never `unwrap_or_default()`: a zero budget turns every SCM stop into a
        // force-kill after the grace buffer alone.
        let budget =
            serve::recorded_stop_budget(&paths).unwrap_or(super::FALLBACK_DRAIN_BUDGET_SECS);
        let (app_exit_code, saw_stop_request) = watch_app(
            &mut child,
            &paths,
            &stop_file,
            &stop_requested,
            budget,
            &report,
        )?;

        if record.bundled_pg {
            serve::reap_managed_postgres_for(&paths);
        }
        serve::cleanup_daemon_state(&paths);

        let intentional = exit_is_intentional(saw_stop_request, app_exit_code);
        report(
            ServiceState::Stopped,
            if intentional {
                ServiceExitCode::Win32(0)
            } else {
                // Non-zero is what makes the SCM apply the restart policy.
                ServiceExitCode::ServiceSpecific(
                    app_exit_code
                        .and_then(|code| u32::try_from(code).ok())
                        // A code of 0 that reached here means the app was killed
                        // rather than exiting; reporting 0 would tell the SCM it
                        // stopped cleanly and suppress the restart.
                        .filter(|&code| code != 0)
                        .unwrap_or(1),
                )
            },
            Duration::default(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_is_prefixed_and_derived_from_the_project() {
        // Deterministic and project-scoped: `install`, `uninstall` and `doctor`
        // all re-derive it, so the operator never has to name the service.
        assert_eq!(service_name("mytool-a1b2c3d4"), "autumn-mytool-a1b2c3d4");
    }

    #[test]
    fn service_name_replaces_characters_the_scm_forbids() {
        // The identity's first half is a directory name, so it can carry
        // anything a directory can. A `\` or `/` would be rejected by the SCM.
        let name = service_name(r"my app/v2\x-a1b2c3d4");
        assert_eq!(name, "autumn-my_app_v2_x-a1b2c3d4");
        assert!(!name.contains('/') && !name.contains('\\') && !name.contains(' '));
    }

    #[test]
    fn service_name_stays_within_the_scm_length_limit() {
        let name = service_name(&"x".repeat(500));
        assert!(name.chars().count() <= MAX_SERVICE_NAME, "{}", name.len());
    }

    #[test]
    fn a_truncated_service_name_keeps_the_hash_that_distinguishes_checkouts() {
        // The hash suffix is the ONLY thing separating two checkouts that share
        // a directory name. Truncating from the end would drop it, so two long
        // projects would collide on one SCM entry — and the second's uninstall
        // would deregister the first's service.
        let one = service_name(&format!("{}-a1b2c3d4", "x".repeat(500)));
        let two = service_name(&format!("{}-9f8e7d6c", "x".repeat(500)));
        assert!(one.ends_with("-a1b2c3d4"), "{one}");
        assert!(two.ends_with("-9f8e7d6c"), "{two}");
        assert_ne!(one, two, "two checkouts must not share a service name");
        assert!(one.chars().count() <= MAX_SERVICE_NAME);
    }

    #[test]
    fn an_identity_with_no_hash_still_truncates_safely() {
        // Defensive: `project_identity` always appends one, but a name built
        // some other way must not panic or overrun.
        let name = service_name(&"x".repeat(500));
        assert!(name.starts_with(SERVICE_NAME_PREFIX));
        assert!(name.chars().count() <= MAX_SERVICE_NAME);
    }

    #[test]
    fn stop_wait_hint_always_exceeds_the_apps_own_budget() {
        // Under-reporting makes the SCM call a correctly-draining service hung.
        for budget in [0, 1, 35, 600] {
            assert!(
                stop_wait_hint(budget) > Duration::from_secs(budget),
                "budget {budget}"
            );
        }
    }

    #[test]
    fn stop_wait_hint_is_clamped_to_what_the_scm_can_carry() {
        // The budget comes from user config. A wrapping add would produce a tiny
        // hint; an unclamped one PANICS inside `windows-service`, which converts
        // it to `u32` milliseconds — taking down the service host mid-stop and
        // stranding the service in `StopPending`.
        let hint = stop_wait_hint(u64::MAX);
        assert_eq!(hint, MAX_WAIT_HINT);
        assert!(
            u32::try_from(hint.as_millis()).is_ok(),
            "the SCM carries the hint as u32 milliseconds"
        );
        // Real budgets are untouched.
        assert!(stop_wait_hint(35) < MAX_WAIT_HINT);
    }

    #[test]
    fn an_scm_stop_waits_at_least_as_long_as_the_host_will_drain() {
        // The host waits the app's budget plus a grace buffer before force-
        // killing. A shorter uninstall deadline abandons a service that is still
        // draining correctly and leaves it registered — the very state
        // `uninstall-service` was asked to remove.
        for budget in [0, 35, 60, 600] {
            assert!(
                stop_wait_hint(budget) >= Duration::from_secs(budget),
                "budget {budget}"
            );
        }
    }

    #[test]
    fn the_fallback_drain_budget_is_never_zero() {
        // The service host uses it when `serve.addr` cannot be read. Zero would
        // make `sc.exe stop` force-kill after the grace buffer alone — a hard
        // kill that skips shutdown hooks, which is the outcome this whole slice
        // exists to eliminate.
        assert_ne!(FALLBACK_DRAIN_BUDGET_SECS, 0);
        // And it survives into the hint the SCM is told to wait for.
        assert!(stop_wait_hint(FALLBACK_DRAIN_BUDGET_SECS) > STOP_HOOK_HEADROOM);
    }

    #[test]
    fn an_operator_stop_is_intentional_whatever_the_exit_code() {
        // A stop whose drain overran gets force-killed, so it exits non-zero.
        // Restarting it would fight the operator.
        assert!(exit_is_intentional(true, Some(1)));
        assert!(exit_is_intentional(true, None));
    }

    #[test]
    fn a_clean_exit_is_intentional_even_with_no_stop_request() {
        assert!(exit_is_intentional(false, Some(0)));
    }

    #[test]
    fn only_a_missing_service_counts_as_absent() {
        // `uninstall-service` treats "not registered" as licence to delete the
        // daemon's records. A denial read as an absence would remove the record
        // of a service still registered `AutoStart`, whose next restart then
        // fails with no configuration to read and no way to log why.
        assert!(open_failure_means_absent(Some(
            ERROR_SERVICE_DOES_NOT_EXIST
        )));
        // ERROR_ACCESS_DENIED — what a non-elevated shell gets asking for
        // STOP | DELETE.
        assert!(!open_failure_means_absent(Some(5)));
        // ERROR_INVALID_HANDLE, and an error carrying no OS code at all.
        assert!(!open_failure_means_absent(Some(6)));
        assert!(!open_failure_means_absent(None));
    }

    #[test]
    fn an_access_denial_is_named_so_the_message_can_say_what_to_do() {
        assert!(open_failure_is_access_denied(Some(5)));
        assert!(!open_failure_is_access_denied(Some(
            ERROR_SERVICE_DOES_NOT_EXIST
        )));
        assert!(!open_failure_is_access_denied(None));
    }

    #[test]
    fn a_stop_request_latches_once_seen() {
        // `autumn serve stop`'s cleanup removes the request file after the app
        // exits. Without the latch, the host's next poll would see no request,
        // read the exit as a crash, and have the SCM restart a daemon the
        // operator just stopped.
        assert!(stop_was_requested(true, false, false));
    }

    #[test]
    fn either_channel_alone_requests_a_stop() {
        // The Service Control Manager's handler sets the flag; `autumn serve
        // stop` only creates the file. Both must count, or one of the two ways
        // to stop a service-hosted daemon reads as a crash.
        assert!(stop_was_requested(false, true, false));
        assert!(stop_was_requested(false, false, true));
    }

    #[test]
    fn no_signal_at_all_is_not_a_stop() {
        assert!(!stop_was_requested(false, false, false));
    }

    #[test]
    fn a_latched_stop_survives_an_exit_code_that_looks_like_a_crash() {
        // The two halves compose: a drain that overran is force-killed (non-zero
        // exit) but is still the operator's stop, not a crash to restart.
        let saw = stop_was_requested(false, true, false);
        assert!(exit_is_intentional(saw, Some(1)));
    }

    #[test]
    fn a_crash_is_not_intentional_so_the_scm_restarts_it() {
        // This is the whole crash-recovery guarantee: an app that died on its
        // own must come back.
        assert!(!exit_is_intentional(false, Some(101)));
        // Killed by the OS: no exit code at all.
        assert!(!exit_is_intentional(false, None));
    }

    #[test]
    fn restart_delays_back_off_rather_than_hammering() {
        // An app that crashes on boot would otherwise be restarted every five
        // seconds forever, burying the cause in the event log.
        let delays = restart_delays();
        assert!(delays.len() >= 3, "{delays:?}");
        assert!(
            delays.windows(2).all(|w| w[1] > w[0]),
            "delays must increase: {delays:?}"
        );
        assert!(delays[0] > Duration::ZERO);
    }

    #[test]
    fn failure_count_resets_after_a_quiet_day() {
        // Without a reset window, three crashes spread over a week would be
        // treated as an escalating series and hit the longest delay.
        assert_eq!(failure_reset_period(), Duration::from_secs(86_400));
    }

    fn sample_record() -> ServiceRecord {
        ServiceRecord {
            name: "autumn-demo-a1b2c3d4".to_owned(),
            binary: PathBuf::from(r"C:\proj\target\debug\demo.exe"),
            working_dir: PathBuf::from(r"C:\proj"),
            paths: RuntimeParts {
                runtime: PathBuf::from(r"C:\state\run"),
                data: PathBuf::from(r"C:\state"),
                logs: PathBuf::from(r"C:\state\logs"),
                socket: PathBuf::from(r"C:\state\run\serve.sock"),
            },
            profile: Some("prod".to_owned()),
            release: true,
            bundled_pg: true,
            role: Some("web".to_owned()),
            pin: Some(vec!["critical".to_owned()]),
        }
    }

    #[test]
    fn service_record_round_trips_through_toml() {
        // The host reads this back in a different security context; a field lost
        // in the round trip is a daemon started with the wrong profile or role.
        let record = sample_record();
        let parsed = ServiceRecord::parse(&record.to_toml().expect("serialize")).expect("parse");
        assert_eq!(parsed, record);
    }

    #[test]
    fn service_record_tolerates_a_minimal_file() {
        // Every optional field defaults, so a record written by an older install
        // still starts the service rather than failing to parse.
        let minimal = r#"
            name = "autumn-demo"
            binary = "C:\\proj\\demo.exe"
            working_dir = "C:\\proj"
            [paths]
            runtime = "C:\\state\\run"
            data = "C:\\state"
            logs = "C:\\state\\logs"
            socket = "C:\\state\\run\\serve.sock"
        "#;
        let parsed = ServiceRecord::parse(minimal).expect("parse minimal record");
        assert_eq!(parsed.profile, None);
        assert!(!parsed.release);
        assert_eq!(parsed.pin, None);
    }

    #[test]
    fn the_off_windows_message_names_the_platform_equivalent() {
        // A Linux user who types this should learn what to do, not just that
        // they cannot do this.
        let message = unsupported_message("install-service");
        assert!(message.contains("systemd"), "{message}");
        assert!(message.contains("launchd"), "{message}");
        assert!(message.contains("--daemon"), "{message}");
    }
}
