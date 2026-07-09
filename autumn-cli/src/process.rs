//! Shared child-process and PID-file helpers for `autumn dev` and
//! `autumn serve`.
//!
//! Centralizes the SIGTERM-then-force-kill shutdown sequence (previously inline
//! in `dev.rs`) and adds the PID/lockfile primitives the daemon lifecycle needs:
//! liveness probing, atomic lock acquisition with stale-PID reclamation, and
//! bounded waits for a process to exit.

#![allow(dead_code, clippy::missing_const_for_fn)]

use std::path::{Path, PathBuf};
#[cfg(unix)]
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

/// The command name of `pid` (Linux `/proc/<pid>/comm`), when the platform
/// exposes it. Returns `None` elsewhere, where callers fall back to weaker
/// identity checks. Used to confirm a recorded PID still belongs to the expected
/// program before signalling it, guarding against PID reuse.
#[must_use]
pub fn process_command_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_owned())
    }
    #[cfg(not(target_os = "linux"))]
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
/// `/proc/<pid>/stat`. Returns `None` elsewhere, where callers fall back to a
/// PID-only liveness check.
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
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
    // Record `<pid> <start_time>` so a later reader can reject a reused PID
    // (0 = start time unknown on this platform).
    let start = process_start_time(pid).unwrap_or(0);
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
/// Uses `kill(pid, 0)`: success or `EPERM` (exists but not ours) means alive;
/// `ESRCH` means gone.
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

/// Non-Unix fallback: conservatively assume a recorded PID is alive so we never
/// steal another instance's lock. (Daemon mode is Unix-first.)
#[cfg(not(unix))]
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

/// Non-Unix fallback: no process groups in this MVP.
#[cfg(not(unix))]
pub fn force_kill_group(_pgid: u32) {}

/// Wait up to `timeout` for `pid` to exit, polling its liveness.
/// Returns `true` if the process exited within the timeout.
#[cfg(unix)]
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
#[cfg(unix)]
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

/// Gracefully stop the daemon identified by `record`: send `SIGTERM`, wait up to
/// `timeout` for it to drain and exit, then force-kill if it is still alive.
/// Returns `true` only if the recorded process is gone afterwards.
///
/// On Linux the wait is identity-aware: a recorded start time detects a PID
/// reused mid-drain, so the `SIGKILL`/`killpg` escalation never aims at a
/// stranger. Where a start time is unavailable (macOS/BSD, or legacy pidfiles)
/// we can't distinguish our stuck daemon from a reused PID, so we enforce the
/// timeout and escalate against the still-live PID rather than risk reporting a
/// false "stopped" and orphaning a daemon that merely closed its listener while
/// still draining. (The residual risk of signalling a reused PID is bounded: the
/// wait returns the instant the original exits, so reaching here with a live PID
/// almost always means our own daemon never exited.)
///
/// A `false` return means the daemon could **not** be stopped — e.g. it is owned
/// by another user and `kill` returns `EPERM` — so the caller must not delete
/// its state or report success.
#[cfg(unix)]
#[must_use]
pub fn stop_record(record: &PidRecord, timeout: Duration, socket: &Path) -> bool {
    let _ = signal_terminate(record.pid);
    if wait_for_record_exit(record, timeout) {
        return true;
    }
    if !is_record_alive(record) {
        return true;
    }
    // Timed out with the PID still alive. A daemon can close its control socket
    // before its `on_shutdown` hooks finish (or hang outright), so a dead socket
    // is *not* proof of exit — enforce the budget with a `SIGKILL`. `socket` is
    // retained for symmetry with the non-Unix stub and possible future identity
    // checks, but is intentionally not used as an exit signal here.
    let _ = socket;
    // The app missed its graceful-drain budget and is still our daemon, so its
    // `on_shutdown` hooks (which stop a managed Postgres child) may not have run.
    // Kill the whole daemon process group, not just the app PID, so supervised
    // children are not orphaned holding the data dir/port.
    force_kill(record.pid);
    force_kill_group(record.pid);
    wait_for_record_exit(record, Duration::from_secs(5))
}

/// Stop a managed Postgres postmaster `pid`: request a "fast" shutdown (SIGINT —
/// roll back active transactions and exit), then escalate to `SIGKILL` if it
/// doesn't exit within `timeout`.
///
/// Used by `autumn serve stop` to reap a managed cluster a daemon left running
/// (its `on_shutdown` hook timed out, or it was force-killed). Postgres `setsid`s
/// itself, so it isn't in the daemon's process group; this signals it directly
/// via the pid recorded in `postmaster.pid`. Safe for an abandoned cluster with
/// no remaining clients.
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
        force_kill(pid);
        force_kill_group(pid);
    }
}

/// Non-Unix fallback.
#[cfg(not(unix))]
pub fn stop_postmaster(_pid: u32, _timeout: Duration) {}

/// Non-Unix fallback: no graceful-signal mechanism in this MVP.
#[cfg(not(unix))]
#[must_use]
pub fn stop_record(_record: &PidRecord, _timeout: Duration, _socket: &Path) -> bool {
    false
}

/// Wait for a child process with a timeout. Returns `Err(())` if it did not
/// exit before `timeout` elapsed.
///
/// # Errors
///
/// Returns `Err(())` on timeout or if the child cannot be reaped.
#[cfg(unix)]
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
        // Our own PID is alive; a second acquire must be rejected.
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
