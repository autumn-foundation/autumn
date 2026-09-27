//! Shared child-process and PID-file helpers for `autumn dev` and
//! `autumn serve`.
//!
//! Centralizes the request-then-force-kill shutdown sequence (previously inline
//! in `dev.rs`) and adds the PID/lockfile primitives the daemon lifecycle needs:
//! liveness probing, atomic lock acquisition with stale-PID reclamation, and
//! bounded waits for a process to exit.
//!
//! The primitives are platform-neutral at the seam and platform-specific
//! underneath: Unix asks a daemon to drain with `SIGTERM` and reaps with
//! `killpg`, Windows asks with a cooperative-shutdown file and reaps with a
//! `TerminateProcess` sweep of the process tree. Everything that can be
//! compiled on both is, so Linux CI exercises the Windows decision logic.

#![allow(dead_code, clippy::missing_const_for_fn)]

use std::path::{Path, PathBuf};
// Not `cfg(unix)`-gated: `wait_with_timeout` is compiled on every platform so
// `autumn dev`'s non-Unix cooperative stop (#1616) can share the same bounded
// wait the Unix `SIGTERM` path uses.
use std::process::Child;
use std::time::Duration;

/// What an existing PID lockfile tells us about the previous owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidState {
    /// No usable PID recorded (missing or unparseable) — safe to claim.
    Free,
    /// A PID was recorded but that process is gone — stale, safe to reclaim.
    Stale,
    /// A PID was recorded and that process is alive — the daemon is running.
    Alive(u32),
}

/// Classify an existing lockfile from its parsed PID and a liveness check.
///
/// Pure decision function (no syscalls/fs) so the lock policy is unit-testable.
#[must_use]
pub const fn classify(recorded_pid: Option<u32>, alive: bool) -> PidState {
    match recorded_pid {
        Some(pid) if alive => PidState::Alive(pid),
        Some(_) => PidState::Stale,
        None => PidState::Free,
    }
}

/// Why acquiring the PID lockfile failed.
#[derive(Debug)]
pub enum AcquireError {
    /// A live daemon already holds the lock (its PID).
    AlreadyRunning(u32),
    /// An I/O error occurred manipulating the lockfile.
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(pid) => {
                write!(f, "a daemon is already running (pid {pid})")
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// A parsed PID lockfile: the recorded PID plus, when available, the process
/// start time, used to detect PID reuse after a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidRecord {
    /// Recorded process ID.
    pub pid: u32,
    /// Process start time captured when the lockfile was written, or `None`
    /// when the platform can't report it (e.g. non-Linux) or it wasn't stored.
    pub start_time: Option<u64>,
}

/// Read and parse a lockfile: `"<pid>"` or `"<pid> <start_time>"`.
#[must_use]
pub fn read_pidfile(path: &Path) -> Option<PidRecord> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut parts = contents.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let start_time = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s != 0);
    Some(PidRecord { pid, start_time })
}

/// The command name of `pid`, when the platform exposes it.
///
/// Linux reads `/proc/<pid>/comm`; Windows takes the image path's file stem, so
/// `postgres.exe` reports as `postgres` and callers compare one spelling on both.
/// Returns `None` elsewhere — and on Windows for a process we may not open —
/// where callers fall back to weaker identity checks. Used to confirm a recorded
/// PID still belongs to the expected program before signalling it, guarding
/// against PID reuse.
#[must_use]
pub fn process_command_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_owned())
    }
    #[cfg(windows)]
    {
        windows_impl::process_image_stem(pid)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

/// The working directory of `pid` (Linux `/proc/<pid>/cwd`), when the platform
/// exposes it. A `PostgreSQL` postmaster runs with its cwd set to the data
/// directory, so this lets a caller confirm a recorded PID is the cluster for a
/// *specific* data dir — not an unrelated `postgres` that reused the PID.
/// `None` elsewhere.
#[must_use]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The kernel-reported start time of `pid`, when the platform exposes it.
///
/// Linux reads field 22 (`starttime`, jiffies since boot) of
/// `/proc/<pid>/stat`; Windows reads the process creation `FILETIME`. Returns
/// `None` elsewhere, where callers fall back to a PID-only liveness check.
///
/// Windows reporting a real value is what makes the daemon lifecycle's
/// PID-reuse guards conclusive there rather than best-effort as on macOS/BSD.
#[must_use]
pub fn process_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 2 (`comm`) is parenthesized and may contain spaces/parens, so
        // split after the last ')'. The remainder begins at field 3 (`state`),
        // and `starttime` is field 22 → index 19 of the remainder.
        let rest = stat.rsplit_once(')')?.1;
        rest.split_whitespace().nth(19)?.parse::<u64>().ok()
    }
    #[cfg(windows)]
    {
        windows_impl::process_creation_time(pid)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

/// The start time of `pid` as **Unix-epoch seconds**, where the platform
/// reports it in those units.
///
/// Distinct from [`process_start_time`], whose units are platform-defined and
/// only ever compared against a value this program recorded itself. This one is
/// compared against a wall-clock stamp written by *another* program — the start
/// time in `PostgreSQL`'s `postmaster.pid` — so the units have to be real.
///
/// `None` on Linux, whose `/proc` `starttime` is jiffies since boot, and on
/// macOS, which reports nothing. Callers then fall back to whatever other
/// identity evidence they have.
#[must_use]
pub fn process_start_epoch_secs(pid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        windows_impl::process_creation_time(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// Whether `pid` is alive but **out of our reach** — running under an account
/// we cannot query.
///
/// Only meaningful where the platform normally reports a start time and did not
/// for this process. That combination means the process exists (the enumeration
/// lists every process regardless of access) while `OpenProcess` was refused —
/// which is exactly a service running as Local System, seen from the operator's
/// ordinary shell.
///
/// Callers use it to answer "running, but I cannot verify or manage it" instead
/// of "stale". The difference matters: a daemon registered with
/// `autumn serve install-service` runs as Local System, so without this an
/// unprivileged `autumn serve status` would call it stopped and `stop` would
/// delete a live service's records.
///
/// `false` on Unix. Linux always reports a start time, so an unreadable one
/// means gone; macOS never reports one, so treating "unknown" as "out of reach"
/// there would disable the PID-reuse guard the whole lifecycle rests on.
#[must_use]
pub fn process_is_out_of_reach(pid: u32) -> bool {
    #[cfg(windows)]
    {
        is_process_alive(pid) && process_start_time(pid).is_none()
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// Whether a recorded lockfile owner is still our live daemon.
///
/// Requires the PID to be alive AND — when both the recorded and current start
/// times are known — that they match, so a PID reused by an unrelated process
/// after a crash is treated as stale rather than "our daemon".
#[must_use]
pub fn is_record_alive(record: &PidRecord) -> bool {
    if !is_process_alive(record.pid) {
        return false;
    }
    match (record.start_time, process_start_time(record.pid)) {
        (Some(recorded), Some(current)) => recorded == current,
        // Unknown on either side: best-effort liveness only.
        _ => true,
    }
}

/// Atomically acquire the PID lockfile for `pid`, reclaiming a stale lock left
/// by a crashed previous run.
///
/// The record is written to a private temp file and then **hard-linked** into
/// place: `link(2)` fails atomically if the target exists (the lock), and a
/// reader always sees the fully-written record rather than an empty file mid
/// `create`+`write`. (A bare `create_new` then `write` leaves a window where a
/// racing acquirer reads an empty lockfile, misclassifies it as free, and lets
/// both starters spawn.) If the lock already exists, the recorded PID is probed:
/// a live owner yields [`AcquireError::AlreadyRunning`] (the caller should refuse
/// to start); a dead/corrupt lock is reclaimed and creation retried once.
///
/// # Errors
///
/// Returns [`AcquireError::AlreadyRunning`] if a live daemon holds the lock, or
/// [`AcquireError::Io`] on filesystem errors.
pub fn acquire_pidfile(path: &Path, pid: u32) -> Result<(), AcquireError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(AcquireError::Io)?;
    }
    // A live owner already holds the lock: say so before anything else. That is
    // the answer the caller acts on, it does not depend on our own pid, and
    // reporting it here keeps a doomed acquire from spending the start-time
    // retry budget below only to fail with a less useful error. This is a fast
    // path, not the claim — `acquire_via_link` re-checks atomically, so a lock
    // released between here and there is still reclaimed correctly.
    if let Some(existing) = read_pidfile(path)
        && is_record_alive(&existing)
    {
        return Err(AcquireError::AlreadyRunning(existing.pid));
    }
    // Record `<pid> <start_time>` so a later reader can reject a reused PID
    // (0 = start time unknown on this platform).
    let start = record_start_time(pid)?;
    // A private temp sibling, distinct per source file *and* per launcher pid so
    // two concurrent acquirers never share one (and a `serve.pid` acquire can't
    // collide with a `serve.startlock` acquire).
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let file_name = path
        .file_name()
        .ok_or_else(|| AcquireError::Io(std::io::Error::other("pidfile path has no file name")))?
        .to_string_lossy()
        .into_owned();
    let temp_name = format!(".{file_name}.tmp.{}", std::process::id());
    let temp = parent.map_or_else(|| PathBuf::from(&temp_name), |p| p.join(&temp_name));
    // Clear a leftover temp from a crashed run, then write the full record.
    let _ = std::fs::remove_file(&temp);
    if let Err(e) = std::fs::write(&temp, format!("{pid} {start}\n")) {
        return Err(AcquireError::Io(e));
    }

    let result = acquire_via_link(&temp, path);
    let _ = std::fs::remove_file(&temp);
    result
}

/// How many times to re-ask for a just-spawned process's start time, and how
/// long to wait between attempts.
///
/// Windows enumerates processes from a `ToolHelp` snapshot that can transiently
/// miss a child spawned moments ago. One miss used to be recorded as "unknown",
/// and an unknown recorded time is not merely weaker — it is *wrong* in a
/// specific way: a later `confirmed_running` sees `(None, Some(current))`, has
/// no endpoint to fall back to on Windows, and concludes the pidfile is stale.
/// `status` then reports a live daemon as stopped and `stop` deletes its records
/// without stopping it.
const START_TIME_ATTEMPTS: u32 = 5;
const START_TIME_RETRY: Duration = Duration::from_millis(50);

/// The start time to write into a pidfile for `pid`.
///
/// Retries, because the caller has just spawned this process and it must exist.
/// `0` (unknown) is only ever recorded on a platform that genuinely cannot
/// report one — macOS. Where the platform *can* report one and still does not
/// after retrying, the acquire fails rather than write a record that a later
/// reader would misclassify as stale.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when the platform reports start times but would
/// not report this one.
fn record_start_time(pid: u32) -> Result<u64, AcquireError> {
    for attempt in 0..START_TIME_ATTEMPTS {
        if let Some(start) = process_start_time(pid) {
            return Ok(start);
        }
        // A platform that never reports one (macOS) records the documented
        // "unknown" and relies on the endpoint checks instead; retrying there
        // would just add latency to every acquire.
        if !platform_reports_start_times() {
            return Ok(0);
        }
        if attempt + 1 < START_TIME_ATTEMPTS {
            std::thread::sleep(START_TIME_RETRY);
        }
    }
    Err(AcquireError::Io(std::io::Error::other(format!(
        "could not read the start time of process {pid}, so its lockfile would \
         be indistinguishable from a stale one"
    ))))
}

/// Whether this platform reports process start times at all.
///
/// Linux reads `/proc/<pid>/stat` and Windows the creation `FILETIME`; macOS and
/// the BSDs report nothing, and the lifecycle's identity checks fall back to
/// endpoint ownership there.
const fn platform_reports_start_times() -> bool {
    cfg!(any(target_os = "linux", windows))
}

/// Hard-link `temp` (already holding the full record) into `path` as the lock,
/// reclaiming a stale/corrupt lock once. Factored out so the temp file is always
/// cleaned up by the caller regardless of which branch returns.
fn acquire_via_link(temp: &Path, path: &Path) -> Result<(), AcquireError> {
    for attempt in 0..2 {
        match std::fs::hard_link(temp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let recorded = read_pidfile(path);
                let alive = recorded.as_ref().is_some_and(is_record_alive);
                match classify(recorded.map(|r| r.pid), alive) {
                    PidState::Alive(existing) => {
                        return Err(AcquireError::AlreadyRunning(existing));
                    }
                    // Stale or corrupt: reclaim and retry the atomic link. The
                    // link guarantees any existing lock is fully written, so an
                    // unparseable file here is genuine corruption, not an in-
                    // flight create. A concurrent deletion (NotFound) is fine —
                    // the next iteration's `hard_link` will win.
                    PidState::Stale | PidState::Free if attempt == 0 => {
                        if let Err(err) = std::fs::remove_file(path)
                            && err.kind() != std::io::ErrorKind::NotFound
                        {
                            return Err(AcquireError::Io(err));
                        }
                    }
                    _ => return Err(AcquireError::Io(e)),
                }
            }
            Err(e) => return Err(AcquireError::Io(e)),
        }
    }
    Err(AcquireError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not acquire pid lockfile after reclaiming stale lock",
    )))
}

/// Validate a raw PID for use with `kill(2)`: positive and representable.
#[cfg(unix)]
#[must_use]
pub fn validate_pid_for_kill(pid: u32) -> Option<libc::pid_t> {
    let cast_pid = pid.try_into().ok()?;
    if cast_pid > 0 { Some(cast_pid) } else { None }
}

/// Whether a process with `pid` is currently alive.
///
/// Unix uses `kill(pid, 0)`: success or `EPERM` (exists but not ours) means
/// alive; `ESRCH` means gone. Windows opens the process and probes its handle;
/// `ERROR_ACCESS_DENIED` is the analog of `EPERM` and also means alive, so a
/// daemon we merely cannot see never has its lock stolen.
#[cfg(unix)]
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    let Some(p) = validate_pid_for_kill(pid) else {
        return false;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None) {
        // Ok = signalable; EPERM = exists but owned by another user.
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(windows)]
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    windows_impl::is_process_alive(pid)
}

/// Fallback for a platform with neither POSIX signals nor the Win32 process
/// API: conservatively assume a recorded PID is alive so we never steal another
/// instance's lock.
#[cfg(not(any(unix, windows)))]
#[must_use]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

/// Send `SIGTERM` to `pid` for graceful shutdown.
///
/// # Errors
///
/// Returns an error if the signal cannot be delivered.
#[cfg(unix)]
pub fn signal_terminate(pid: u32) -> std::io::Result<()> {
    let p = validate_pid_for_kill(pid)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid pid"))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(p),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Force-kill `pid` with `SIGKILL`.
#[cfg(unix)]
pub fn force_kill(pid: u32) {
    if let Some(p) = validate_pid_for_kill(pid) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(p),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Force-kill the process *group* led by `pgid` with `SIGKILL`.
///
/// Daemons are spawned as their own group leader (`process_group(0)`), so the
/// recorded daemon PID is also its PGID. Signalling the group reaps the daemon's
/// descendants — e.g. a managed Postgres child — that a bare `SIGKILL` to the app
/// PID alone would orphan (holding the data dir/port). Only call this for
/// detached daemons, never a foreground child that shares our group.
#[cfg(unix)]
pub fn force_kill_group(pgid: u32) {
    if let Some(p) = validate_pid_for_kill(pgid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(p),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Force-kill `pid` with `TerminateProcess`.
#[cfg(windows)]
pub fn force_kill(pid: u32) {
    windows_impl::terminate(pid);
}

/// Terminate the process tree rooted at `pid`, deepest descendant first.
///
/// Windows has no process groups a detached daemon can be signalled through, so
/// the sweep is reconstructed from a process snapshot: [`descendants_of`]
/// applies the same PID-reuse guard the rest of this module applies, then each
/// process is terminated bottom-up so a supervisor cannot respawn a child while
/// we are still walking. The root goes last, matching `killpg`, which reaches
/// the group leader too.
///
/// Take the snapshot before killing the root: once it exits it leaves the
/// snapshot, and its children's recorded parent PID no longer resolves to a
/// creation time we can compare against.
#[cfg(windows)]
pub fn force_kill_group(pgid: u32) {
    for pid in descendants_of(pgid, &windows_impl::process_snapshot()) {
        windows_impl::terminate(pid);
    }
    windows_impl::terminate(pgid);
}

/// Fallback for a platform with neither process groups nor the Win32 process
/// API: nothing to reap.
#[cfg(not(any(unix, windows)))]
pub fn force_kill(_pid: u32) {}

#[cfg(not(any(unix, windows)))]
pub fn force_kill_group(_pgid: u32) {}

/// Wait up to `timeout` for `pid` to exit, polling its liveness.
/// Returns `true` if the process exited within the timeout.
#[must_use]
pub fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while is_process_alive(pid) {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Identity-aware [`wait_for_pid_exit`]: also returns `true` once the recorded
/// process is no longer *our* daemon — i.e. it exited, or (on platforms that
/// record a start time) the PID was reused by an unrelated process. Used before
/// escalating to `SIGKILL` so a force-kill can never land on a stranger that
/// happened to inherit the PID during the drain window.
#[must_use]
pub fn wait_for_record_exit(record: &PidRecord, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while is_record_alive(record) {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// One process in an OS snapshot, as [`descendants_of`] needs it.
///
/// `created` is a platform-specific monotonic-ish stamp (Windows: the creation
/// `FILETIME`); `0` means the platform would not tell us, matching the pidfile's
/// "start time unknown" convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcRow {
    /// The process id.
    pub pid: u32,
    /// The process id this row records as its parent.
    pub parent: u32,
    /// When the process started, or `0` when unknown.
    pub created: u64,
}

/// The descendants of `root` in `rows`, deepest first, excluding `root` itself.
///
/// Windows keeps a process's recorded parent PID after the parent exits, and
/// PIDs are recycled quickly, so "every row whose parent is `root`" would sweep
/// in strangers that merely inherited a dead daemon's number. A child cannot
/// predate its parent, so a row that started *before* the process it claims as
/// its parent belongs to that PID's previous owner and is dropped. Rows already
/// visited are skipped, so a snapshot with a parent cycle terminates. A root
/// that is not in the snapshot yields nothing at all — see below.
///
/// Pure, so the Windows reaping policy is unit-tested on every platform.
#[must_use]
pub fn descendants_of(root: u32, rows: &[ProcRow]) -> Vec<u32> {
    // The root must be in the snapshot. If it has already exited, its PID is
    // free to be recycled and every row still naming it as a parent belongs to
    // whoever held that number before — with no start time to compare against,
    // the guard below is disabled precisely where it is needed most. Sweeping
    // nothing is the only safe answer; the caller's own reapers (the managed
    // Postgres one) cover what is actually ours.
    let Some(root_created) = rows.iter().find(|r| r.pid == root).map(|r| r.created) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::from([root]);
    let mut frontier = vec![(root, root_created)];
    let mut found = Vec::new();
    while let Some((parent, parent_created)) = frontier.pop() {
        for row in rows {
            if row.parent != parent || row.pid == parent || row.pid == root {
                continue;
            }
            // Both stamps known and the child is older: `parent` is a recycled
            // PID, so this row is not ours to kill.
            if parent_created != 0 && row.created != 0 && row.created < parent_created {
                continue;
            }
            if !seen.insert(row.pid) {
                continue;
            }
            found.push(row.pid);
            frontier.push((row.pid, row.created));
        }
    }
    // Deepest first: terminating a supervisor before its children lets the
    // children linger holding whatever the supervisor was managing.
    found.reverse();
    found
}

/// How [`stop_record`] asks a daemon to begin its graceful drain.
///
/// Both arms are compiled everywhere so the file arm — the Windows one — is
/// exercised by Linux CI rather than first by a user.
#[derive(Debug, Clone, Copy)]
pub enum StopRequest<'a> {
    /// Send `SIGTERM` to the recorded PID.
    Signal,
    /// Create this file. An app started with `AUTUMN_SHUTDOWN_SIGNAL_FILE`
    /// pointed here runs the same graceful drain a signal triggers — the only
    /// portable way to ask on a platform with no `SIGTERM`.
    File(&'a Path),
}

/// Ask the process recorded by `record` to drain.
///
/// # Errors
///
/// Returns the delivery error: the signal could not be sent (e.g. the daemon is
/// another user's), or the request file could not be created.
pub fn deliver_stop_request(record: &PidRecord, request: &StopRequest<'_>) -> std::io::Result<()> {
    match *request {
        StopRequest::Signal => {
            #[cfg(unix)]
            {
                signal_terminate(record.pid)
            }
            #[cfg(not(unix))]
            {
                let _ = record;
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "this platform has no POSIX signals; use StopRequest::File",
                ))
            }
        }
        StopRequest::File(path) => create_stop_request(path),
    }
}

/// Create the cooperative-shutdown request file, and its parent directory.
///
/// The contents are never read — the file's existence is the whole signal — so
/// an empty file is correct and re-requesting is a no-op.
///
/// # Errors
///
/// Returns the I/O error if the directory or the file cannot be created.
pub fn create_stop_request(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path).map(|_| ())
}

/// Remove a shutdown request so the next daemon does not drain the moment it
/// boots. Best-effort: a file that is already gone is the desired state.
pub fn clear_stop_request(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// How long to wait for a force-killed process to actually disappear.
const KILL_SETTLE: Duration = Duration::from_secs(5);

/// What [`stop_record`] achieved.
///
/// Distinguishing these matters: only `Drained` means the app's `on_shutdown`
/// hooks ran, and the other three each need a different thing said to the
/// operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The daemon drained and exited within its budget.
    Drained,
    /// It was asked, missed its budget, and the process tree was force-killed.
    /// Shutdown hooks may not have run.
    Escalated,
    /// The drain could not be requested at all, so it was force-killed without
    /// being asked. Carries why, because nothing else will surface it.
    Unreachable(String),
    /// It is still alive and could not be killed — e.g. owned by another user.
    /// The caller must not delete its state or report success.
    Failed,
}

impl StopOutcome {
    /// Whether the daemon is gone afterwards.
    #[must_use]
    pub fn stopped(&self) -> bool {
        !matches!(self, Self::Failed)
    }
}

/// Gracefully stop the daemon identified by `record`: ask it to drain via
/// `request`, wait up to `timeout` for it to exit, then force-kill the process
/// tree if it is still alive. Returns `true` only if the recorded process is
/// gone afterwards.
///
/// Where a start time is recorded on both sides (Linux, Windows) the wait is
/// identity-aware: a PID reused mid-drain reads as "exited", so the escalation
/// never aims at a stranger. Where a start time is unavailable (macOS/BSD, or
/// legacy pidfiles) we cannot distinguish our stuck daemon from a reused PID, so
/// we enforce the timeout and escalate against the still-live PID rather than
/// report a false "stopped" and orphan a daemon that merely closed its listener
/// while still draining. (The residual risk is bounded: the wait returns the
/// instant the original exits, so reaching here with a live PID almost always
/// means our own daemon never exited.)
///
/// A failed delivery skips the drain wait: the app was never asked to drain, so
/// waiting out its budget only delays the escalation.
///
/// [`StopOutcome::Failed`] means the daemon could **not** be stopped — e.g. it is
/// owned by another user and both the signal and the kill are refused — so the
/// caller must not delete its state or report success.
#[must_use]
pub fn stop_record(
    record: &PidRecord,
    timeout: Duration,
    request: &StopRequest<'_>,
) -> StopOutcome {
    let delivery = deliver_stop_request(record, request);
    let asked = delivery.is_ok();
    if asked && wait_for_record_exit(record, timeout) {
        return StopOutcome::Drained;
    }
    if !is_record_alive(record) {
        return StopOutcome::Drained;
    }
    // The app missed its graceful-drain budget and is still our daemon, so its
    // `on_shutdown` hooks (which stop a managed Postgres child) may not have
    // run. Reap the whole tree, not just the app PID, so supervised children are
    // not orphaned holding the data dir/port. The tree sweep goes first: on
    // Windows it is reconstructed from a snapshot, and killing the root before
    // taking it loses the parentage the sweep walks.
    force_kill_group(record.pid);
    force_kill(record.pid);
    if !wait_for_record_exit(record, KILL_SETTLE) {
        return StopOutcome::Failed;
    }
    match delivery {
        // Killed after it missed its budget: hooks may not have run.
        Ok(()) => StopOutcome::Escalated,
        // Killed without ever being asked: hooks certainly did not run. The
        // caller has to say so — on Windows the request is a file create, which
        // can fail (a full volume, a directory at the path, an AV lock) while
        // the kill still succeeds, so a silent "stopped" would report a graceful
        // drain that never happened.
        Err(e) => StopOutcome::Unreachable(e.to_string()),
    }
}

/// The name of the named pipe `PostgreSQL` listens on for emulated signals on
/// Windows.
///
/// Windows has no `kill(2)`, so `PostgreSQL` emulates one: every backend serves
/// `\\.\pipe\pgsignal_<pid>`, and a single byte carrying the signal number is
/// the whole protocol (`src/port/kill.c`). Formatting it here, rather than
/// inline in the Windows-only reaper, keeps it under test on every platform.
#[must_use]
pub fn pgsignal_pipe_name(pid: u32) -> String {
    format!(r"\\.\pipe\pgsignal_{pid}")
}

/// `SIGINT` — a `PostgreSQL` "fast" shutdown: roll back active transactions and
/// exit.
const PG_SIGINT: u8 = 2;

/// Stop a managed Postgres postmaster `pid`: request a "fast" shutdown, then
/// escalate to a force-kill if it does not exit within `timeout`.
///
/// Used by `autumn serve stop` to reap a managed cluster a daemon left running
/// (its `on_shutdown` hook timed out, or it was force-killed). Postgres puts
/// itself outside the daemon's process group on Unix (`setsid`) and outside its
/// job on Windows, so this addresses it directly via the PID recorded in
/// `postmaster.pid`. Safe for an abandoned cluster with no remaining clients.
#[cfg(unix)]
pub fn stop_postmaster(pid: u32, timeout: Duration) {
    if let Some(p) = validate_pid_for_kill(pid) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(p),
            nix::sys::signal::Signal::SIGINT,
        );
    }
    if !wait_for_pid_exit(pid, timeout) {
        // A fast shutdown didn't finish in time (e.g. a stuck backend). Postgres
        // `setsid`s, so the postmaster leads its own process group with its
        // backends in it; `SIGKILL`ing only the postmaster PID can leave those
        // children holding the data dir/shared memory and block the next start.
        // Kill the whole Postgres group (the postmaster is its leader, so its PID
        // is the PGID).
        force_kill_group(pid);
        force_kill(pid);
    }
}

#[cfg(windows)]
pub fn stop_postmaster(pid: u32, timeout: Duration) {
    windows_impl::send_pg_signal(pid, PG_SIGINT);
    if !wait_for_pid_exit(pid, timeout) {
        // Same reasoning as the Unix arm: a postmaster's backends are separate
        // processes, so terminating it alone can leave them holding the data dir.
        force_kill_group(pid);
        force_kill(pid);
    }
}

/// Fallback for a platform with neither POSIX signals nor the Win32 process API.
#[cfg(not(any(unix, windows)))]
pub fn stop_postmaster(_pid: u32, _timeout: Duration) {
    let _ = PG_SIGINT;
}

/// Windows process primitives behind this module's platform-neutral seam.
///
/// The workspace forbids `unsafe`, so everything here goes through `sysinfo`'s
/// safe process API, plain `std` I/O, or a `System32` tool invoked by absolute
/// path. Every entry point returns a plain Rust value and swallows a refused
/// query into `None`/`false`, matching how the Unix arms treat a refused
/// syscall — with one deliberate exception, [`is_process_alive`], where "cannot
/// tell" must not read as "dead".
#[cfg(windows)]
mod windows_impl {
    use super::ProcRow;
    use std::io::Write as _;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    /// How many times to retry the `PostgreSQL` signal pipe, and how long to
    /// wait between attempts.
    ///
    /// The postmaster's signal thread hands each connected instance to a
    /// dispatch thread and only then creates the next one, so there is a window
    /// with no listening instance. `PostgreSQL`'s own `pgkill` retries for
    /// exactly this reason; without it a stop lands in that window, waits out
    /// the full timeout, and force-kills a cluster that was one retry away from
    /// a clean fast shutdown.
    const PG_SIGNAL_ATTEMPTS: u32 = 10;
    const PG_SIGNAL_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

    /// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, for
    /// `dwSecurityQosFlags`.
    ///
    /// Without an explicit level, opening a named pipe grants the pipe's server
    /// `SecurityImpersonation` — it may call `ImpersonateNamedPipeClient` and
    /// act as us. `stop_postmaster` is reached from the Local System service
    /// host, so a squatter on the pipe path would inherit SYSTEM. Identification
    /// lets the server learn who we are and nothing more.
    ///
    /// `SECURITY_SQOS_PRESENT` is the bit that makes Windows *read* the level at
    /// all. `OpenOptionsExt::security_qos_flags` already ORs it in (std has to,
    /// since `SECURITY_ANONYMOUS` is `0` and would otherwise be
    /// indistinguishable from "unset"), so passing it here is redundant today —
    /// but that is an implementation detail the public documentation does not
    /// promise, and this is the one line standing between a Local System reaper
    /// and an impersonation. Stating it costs nothing and cannot be read wrong.
    const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
    const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

    /// Load just `pid`'s row, or every row when `pid` is `None`.
    fn snapshot(pid: Option<u32>) -> System {
        let mut system = System::new();
        let refresh = ProcessRefreshKind::nothing();
        match pid {
            Some(pid) => {
                let only = [Pid::from_u32(pid)];
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&only),
                    true,
                    refresh.with_exe(sysinfo::UpdateKind::Always),
                );
            }
            None => {
                system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
            }
        }
        system
    }

    /// Whether `pid` names a live process.
    ///
    /// The process list comes from a `ToolHelp` snapshot, which enumerates every
    /// process regardless of whether we could open it — so a daemon owned by
    /// another user reads as alive, the analog of `kill`'s `EPERM`, and never
    /// has its lock stolen.
    ///
    /// `CreateToolhelp32Snapshot` fails transiently (`ERROR_BAD_LENGTH`) when
    /// the process table changes mid-walk, and `sysinfo` reports that as an
    /// empty list rather than an error — indistinguishable from "the process is
    /// gone". Answering "dead" there would let a second daemon start alongside a
    /// live one. So a negative answer is confirmed against a full snapshot: an
    /// empty machine-wide list is proof the snapshot failed, not that nothing is
    /// running, and we answer "alive" — the direction that never steals a lock.
    pub(super) fn is_process_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        if snapshot(Some(pid)).process(Pid::from_u32(pid)).is_some() {
            return true;
        }
        let all = snapshot(None);
        all.processes().is_empty() || all.process(Pid::from_u32(pid)).is_some()
    }

    /// When `pid` started, in seconds since the Unix epoch.
    ///
    /// Coarser than Linux's jiffy-resolution `starttime`, and `None` for a
    /// process we cannot open — notably a Local System service's app child seen
    /// from an ordinary shell — in which case callers fall back to a PID-only
    /// liveness check exactly as they do on macOS. Normalized away from `0`,
    /// which the pidfile encodes as "unknown".
    pub(super) fn process_creation_time(pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        let system = snapshot(Some(pid));
        let start = system.process(Pid::from_u32(pid))?.start_time();
        (start != 0).then_some(start)
    }

    /// The file stem of `pid`'s image (`postgres.exe` → `postgres`), lowercased
    /// so callers compare one spelling across platforms.
    pub(super) fn process_image_stem(pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        let system = snapshot(Some(pid));
        let process = system.process(Pid::from_u32(pid))?;
        let name = process
            .exe()
            .and_then(std::path::Path::file_stem)
            .map_or_else(
                || {
                    std::path::Path::new(process.name())
                        .file_stem()
                        .map(std::ffi::OsStr::to_os_string)
                },
                |stem| Some(stem.to_os_string()),
            )?;
        Some(name.to_string_lossy().to_lowercase())
    }

    /// Terminate `pid`.
    ///
    /// `taskkill.exe` by **absolute path**. `sysinfo`'s own `Process::kill()`
    /// shells out to a bare `taskkill.exe`, which `CreateProcess` resolves
    /// through the current directory — the operator's project — before
    /// `System32`. This escalation runs as Local System inside the service host,
    /// so a `taskkill.exe` checked into a project directory would run as SYSTEM
    /// instead of stopping anything. Same reasoning as `icacls` in
    /// `crate::paths`.
    ///
    /// A process we cannot open or terminate is left alone; the caller's
    /// post-kill liveness wait is what decides success.
    pub(super) fn terminate(pid: u32) {
        if pid == 0 {
            return;
        }
        let taskkill = std::path::PathBuf::from(
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned()),
        )
        .join("System32")
        .join("taskkill.exe");
        let _ = std::process::Command::new(taskkill)
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// Every process on the machine, with its recorded parent and start time —
    /// the input [`super::descendants_of`] walks.
    pub(super) fn process_snapshot() -> Vec<ProcRow> {
        snapshot(None)
            .processes()
            .values()
            .map(|process| ProcRow {
                pid: process.pid().as_u32(),
                parent: process.parent().map_or(0, sysinfo::Pid::as_u32),
                created: process.start_time(),
            })
            .collect()
    }

    /// Deliver one emulated `PostgreSQL` signal byte to `pid`'s signal pipe.
    ///
    /// Opening the path and writing one byte is the whole protocol
    /// (`src/backend/port/win32/signal.c`) — no Win32 call needed. The pipe is
    /// opened with an explicit `SECURITY_IDENTIFICATION` level so a squatter on
    /// the path cannot impersonate this (possibly Local System) process.
    ///
    /// Returns whether the byte was accepted. A cluster that already exited
    /// serves no pipe, so `false` is the normal outcome there and the caller's
    /// liveness wait handles it.
    pub(super) fn send_pg_signal(pid: u32, signal: u8) -> bool {
        use std::os::windows::fs::OpenOptionsExt as _;
        let path = super::pgsignal_pipe_name(pid);
        for attempt in 0..PG_SIGNAL_ATTEMPTS {
            let delivered = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
                .open(&path)
                .and_then(|mut pipe| pipe.write_all(&[signal]).and_then(|()| pipe.flush()))
                .is_ok();
            if delivered {
                return true;
            }
            if attempt + 1 < PG_SIGNAL_ATTEMPTS {
                std::thread::sleep(PG_SIGNAL_RETRY);
            }
        }
        false
    }
}

/// Wait for a child process with a timeout. Returns `Err(())` if it did not
/// exit before `timeout` elapsed.
///
/// # Errors
///
/// Returns `Err(())` on timeout or if the child cannot be reaped.
///
/// Built only on `try_wait`, which is portable, so this is compiled on every
/// platform: `autumn dev`'s Windows cooperative-stop path (#1616) needs the same
/// bounded wait the Unix `SIGTERM` path uses.
pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<(), ()> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return Err(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_state_free_creates() {
        assert_eq!(classify(None, false), PidState::Free);
        assert_eq!(classify(None, true), PidState::Free);
    }

    #[test]
    fn pid_state_alive_rejected() {
        assert_eq!(classify(Some(123), true), PidState::Alive(123));
    }

    #[test]
    fn pid_state_stale_reclaimed() {
        assert_eq!(classify(Some(123), false), PidState::Stale);
    }

    #[test]
    fn write_pidfile_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        let pid = std::process::id();
        acquire_pidfile(&path, pid).expect("acquire");
        assert_eq!(read_pidfile(&path).map(|r| r.pid), Some(pid));
    }

    #[test]
    fn read_pidfile_parses_pid_and_start_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        std::fs::write(&path, "4242 99887766\n").expect("write");
        let rec = read_pidfile(&path).expect("record");
        assert_eq!(rec.pid, 4242);
        assert_eq!(rec.start_time, Some(99_887_766));
    }

    #[test]
    fn read_pidfile_back_compat_pid_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        std::fs::write(&path, "4242\n").expect("write");
        let rec = read_pidfile(&path).expect("record");
        assert_eq!(rec.pid, 4242);
        assert_eq!(rec.start_time, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn record_with_mismatched_start_time_is_not_alive() {
        // Our PID is alive, but a bogus recorded start time must read as stale.
        let rec = PidRecord {
            pid: std::process::id(),
            start_time: Some(1),
        };
        assert!(!is_record_alive(&rec));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn record_with_matching_start_time_is_alive() {
        let pid = std::process::id();
        let rec = PidRecord {
            pid,
            start_time: process_start_time(pid),
        };
        assert!(is_record_alive(&rec));
    }

    #[test]
    fn create_new_rejects_live_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        // Our own PID is alive; a second acquire must be rejected — and must be
        // rejected AS `AlreadyRunning`, not as some earlier failure. The second
        // acquire below names a pid that does not exist, which is exactly the
        // shape of a caller whose child died mid-start: the live owner is still
        // the more useful answer.
        acquire_pidfile(&path, std::process::id()).expect("first acquire");
        match acquire_pidfile(&path, std::process::id() + 1) {
            Err(AcquireError::AlreadyRunning(pid)) => assert_eq!(pid, std::process::id()),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    // Reclaiming a stale lock depends on liveness detection recognizing the
    // recorded PID as gone. The non-Unix `is_process_alive` is deliberately
    // conservative (always "alive") so we never steal another instance's lock,
    // so stale reclamation is only observable on Unix.
    #[cfg(unix)]
    #[test]
    fn stale_pidfile_reclaimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        // A very high PID that is not running on any sane system.
        std::fs::write(&path, "2147483640\n").expect("seed stale pidfile");
        acquire_pidfile(&path, std::process::id()).expect("stale lock should be reclaimed");
        assert_eq!(read_pidfile(&path).map(|r| r.pid), Some(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn current_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn high_unused_pid_is_not_alive() {
        assert!(!is_process_alive(2_147_483_640));
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_timeout_succeeds_for_fast_process() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        std::thread::sleep(Duration::from_millis(50));
        assert!(wait_with_timeout(&mut child, Duration::from_secs(2)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn wait_with_timeout_times_out_for_long_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        assert!(wait_with_timeout(&mut child, Duration::from_millis(100)).is_err());
        let _ = child.kill();
        let _ = child.wait();
    }

    // ── Pidfile start-time capture (#1639) ───────────────────────────────

    #[test]
    fn a_pidfile_records_a_real_start_time_where_the_platform_has_one() {
        // An "unknown" recorded time is not just weaker on Windows — it is read
        // as *stale*, because there is no endpoint to fall back to. `status`
        // would call a live daemon stopped and `stop` would delete its records
        // without stopping it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        acquire_pidfile(&path, std::process::id()).expect("acquire");
        let rec = read_pidfile(&path).expect("record");
        assert_eq!(
            rec.start_time.is_some(),
            platform_reports_start_times(),
            "a platform that reports start times must not record `unknown`"
        );
    }

    #[test]
    fn a_start_time_for_a_process_that_does_not_exist_fails_the_acquire() {
        // Better a loud failure at start than a lockfile a later reader
        // misclassifies as stale.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.pid");
        let outcome = acquire_pidfile(&path, 2_147_483_640);
        if platform_reports_start_times() {
            assert!(
                matches!(outcome, Err(AcquireError::Io(_))),
                "expected an Io error, got {outcome:?}"
            );
            assert!(!path.exists(), "no lockfile may be left behind");
        } else {
            // macOS reports no start time for anything, so `unknown` is correct
            // there and the endpoint checks carry the identity instead.
            assert!(outcome.is_ok());
        }
    }

    // ── Portable stop request (#1639) ────────────────────────────────────
    //
    // Compiled and run on every platform, not just the one that uses the file
    // arm: Windows is the platform this project's CI almost never exercises
    // interactively, so the logic that only runs there has to be tested here.

    #[test]
    fn stop_request_file_is_created_on_delivery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("serve.stop");
        let rec = PidRecord {
            pid: std::process::id(),
            start_time: None,
        };
        deliver_stop_request(&rec, &StopRequest::File(&path)).expect("deliver");
        assert!(path.is_file(), "the drain request must be a regular file");
    }

    #[test]
    fn stop_request_file_delivery_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.stop");
        let rec = PidRecord {
            pid: std::process::id(),
            start_time: None,
        };
        deliver_stop_request(&rec, &StopRequest::File(&path)).expect("first");
        deliver_stop_request(&rec, &StopRequest::File(&path)).expect("second");
        assert!(path.is_file());
    }

    // ── Process-tree sweep (#1639) ───────────────────────────────────────

    fn row(pid: u32, parent: u32, created: u64) -> ProcRow {
        ProcRow {
            pid,
            parent,
            created,
        }
    }

    #[test]
    fn descendants_are_collected_deepest_first() {
        // 100 -> 200 -> 300, plus an unrelated 400.
        let rows = [
            row(100, 1, 10),
            row(200, 100, 20),
            row(300, 200, 30),
            row(400, 1, 40),
        ];
        assert_eq!(descendants_of(100, &rows), vec![300, 200]);
    }

    #[test]
    fn a_child_older_than_its_parent_is_a_reused_pid_not_a_descendant() {
        // 200 claims 100 as its parent but predates it: 100 is a recycled pid,
        // so 200 belongs to whoever held 100 before. Terminating it would kill
        // a stranger.
        let rows = [row(100, 1, 50), row(200, 100, 10)];
        assert!(descendants_of(100, &rows).is_empty());
    }

    #[test]
    fn the_root_is_never_reported_as_its_own_descendant() {
        // A self-parenting row (pid 0/1 quirks, or a corrupt snapshot) must not
        // make the sweep return the root and kill it twice.
        let rows = [row(100, 100, 10)];
        assert!(descendants_of(100, &rows).is_empty());
    }

    #[test]
    fn a_parent_cycle_terminates() {
        // A snapshot claiming 200's parent is 300 and 300's parent is 200 must
        // not spin forever.
        let rows = [
            row(100, 1, 10),
            row(200, 100, 20),
            row(300, 200, 30),
            row(200, 300, 20),
        ];
        let found = descendants_of(100, &rows);
        assert!(found.contains(&200) && found.contains(&300), "{found:?}");
    }

    /// A child that outlives a short stop budget, plus a thread already blocked
    /// in `wait()` on it.
    ///
    /// The reaper is not incidental. `stop_record` is written for a **detached**
    /// daemon, which the OS reparents when it exits; a test's own child instead
    /// becomes a zombie, and `kill(pid, 0)` reports a zombie as alive — so
    /// without something reaping concurrently, every escalation here would time
    /// out and report `Failed`, testing the harness rather than the code.
    ///
    /// Neither command watches the cooperative-shutdown file, which is the
    /// point: they model an app that misses its drain budget. `ping` rather than
    /// `timeout` on Windows, because `timeout` refuses a redirected stdin.
    fn long_running_child() -> (u32, std::thread::JoinHandle<()>) {
        #[cfg(unix)]
        let mut cmd = {
            let mut c = std::process::Command::new("sleep");
            c.arg("60");
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "ping -n 61 127.0.0.1"]);
            c
        };
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a long-running child");
        let pid = child.id();
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        (pid, reaper)
    }

    // ── Stop outcomes (#1639) ────────────────────────────────────────────
    //
    // Only `Drained` means the app's `on_shutdown` hooks ran. The other arms
    // each need something different said to the operator, and reporting a
    // graceful stop that never happened is the failure this distinction exists
    // to prevent.

    #[test]
    fn a_child_that_ignores_the_request_is_escalated_after_the_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stop_file = dir.path().join("serve.stop");
        let (pid, reaper) = long_running_child();
        let rec = PidRecord {
            pid,
            start_time: process_start_time(pid),
        };
        let outcome = stop_record(
            &rec,
            Duration::from_millis(200),
            &StopRequest::File(&stop_file),
        );
        assert_eq!(outcome, StopOutcome::Escalated);
        assert!(outcome.stopped());
        assert!(
            stop_file.is_file(),
            "the drain was requested before the kill"
        );
        reaper.join().expect("reaper");
    }

    #[test]
    fn an_undeliverable_request_is_reported_rather_than_passed_off_as_a_drain() {
        // A directory at the request path makes `File::create` fail. The daemon
        // is still killed — but calling that "stopped" would tell the operator
        // their in-flight requests drained when nothing was ever asked.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocked = dir.path().join("serve.stop");
        std::fs::create_dir(&blocked).expect("mkdir");
        let (pid, reaper) = long_running_child();
        let rec = PidRecord {
            pid,
            start_time: process_start_time(pid),
        };
        // A 30-second budget that is never waited out: an undeliverable request
        // must escalate immediately rather than stall the operator's `stop`.
        let began = std::time::Instant::now();
        let outcome = stop_record(&rec, Duration::from_secs(30), &StopRequest::File(&blocked));
        assert!(
            matches!(outcome, StopOutcome::Unreachable(_)),
            "expected Unreachable, got {outcome:?}"
        );
        assert!(outcome.stopped());
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "an unasked daemon must not be waited out"
        );
        reaper.join().expect("reaper");
    }

    #[test]
    fn a_dead_daemon_reports_drained_without_killing_anything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rec = PidRecord {
            pid: 2_147_483_640,
            start_time: None,
        };
        assert_eq!(
            stop_record(
                &rec,
                Duration::from_millis(50),
                &StopRequest::File(&dir.path().join("serve.stop")),
            ),
            StopOutcome::Drained
        );
    }

    // ── PostgreSQL-on-Windows signal pipe (#1639) ────────────────────────

    #[test]
    fn pgsignal_pipe_name_matches_postgres_pgkill() {
        // `src/port/kill.c` in PostgreSQL builds exactly this name; a typo here
        // would silently degrade every managed-cluster reap to a hard kill.
        assert_eq!(pgsignal_pipe_name(4242), r"\\.\pipe\pgsignal_4242");
    }

    #[cfg(unix)]
    mod havoc_proptest {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn safe_pid_cast(pid in proptest::num::u32::ANY) {
                if let Some(safe_pid) = validate_pid_for_kill(pid) {
                    assert!(safe_pid > 0, "Safe PID must be strictly positive");
                }
            }
        }
    }
}
