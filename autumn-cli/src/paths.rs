//! Platform-appropriate runtime/data/log directories for `autumn serve`.
//!
//! Daemon state (PID lockfile, Unix socket, address-discovery file), the
//! managed-Postgres data dir, and log files must live under per-OS standard
//! locations (XDG on Linux, `~/Library/Application Support` on macOS,
//! `%APPDATA%`/`%LOCALAPPDATA%` on Windows) — never the current working
//! directory, `/etc`, or `/tmp` (a predictable path under `/tmp` is a symlink
//! hazard for the `0600` socket).
//!
//! On Unix the directories are held at `0700` and the files at `0600`. Windows
//! has no mode bits, so [`RuntimePaths::ensure_dirs`] applies the equivalent
//! ACL — the owning user, `SYSTEM` and the local `Administrators` group, with
//! inheritance broken so nothing wider leaks in from the parent. That is the
//! ACL `%LOCALAPPDATA%` itself carries; setting it explicitly is what makes the
//! guarantee hold for an `AUTUMN_RUNTIME_DIR` pointed somewhere else.
//!
//! [`RuntimePaths::resolve`] performs the real per-OS resolution and honours
//! the `AUTUMN_RUNTIME_DIR` override used by integration tests.
//! [`RuntimePaths::from_base`] is the pure, fs-free seam the unit tests drive.

#![allow(dead_code, clippy::missing_const_for_fn)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Environment variable that overrides the runtime base directory. When set,
/// all daemon paths are rooted at `$AUTUMN_RUNTIME_DIR/<project>`. Primarily
/// used by tests to point the daemon at a `tempdir`.
pub const RUNTIME_DIR_ENV: &str = "AUTUMN_RUNTIME_DIR";

/// Qualifier/organization/application triple used to derive platform dirs.
const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "autumn";

/// Conservative cap on a Unix-domain socket path length. The kernel `sun_path`
/// limit is 104 bytes on macOS and 108 on Linux (including the NUL); staying
/// well under it leaves margin and keeps the check portable.
const MAX_UNIX_SOCKET_PATH: usize = 100;

/// Errors resolving platform directories.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    /// No home/standard directory could be determined for the current user.
    #[error("could not determine a platform data directory for this user")]
    NoPlatformDir,
}

/// Resolved set of base directories for a single project's daemon.
///
/// Accessors append fixed leaf names so the layout is identical whether the
/// paths were resolved from the OS or constructed from a test base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    /// PID lockfile and address-discovery file live here.
    runtime: PathBuf,
    /// Managed-Postgres cluster data dir is rooted here.
    data: PathBuf,
    /// Daemon log files are written here.
    logs: PathBuf,
    /// Unix socket path. Normally `<runtime>/serve.sock`, but a short fallback
    /// under a private runtime root when the natural path would exceed the OS
    /// `sun_path` limit (e.g. macOS, long usernames/package names).
    socket: PathBuf,
}

/// The socket path for `runtime`/`project`: `<runtime>/serve.sock` when that
/// fits the `sun_path` limit, otherwise a short, stable, per-project path under
/// a private runtime root (`$XDG_RUNTIME_DIR`, else the per-user temp dir).
fn resolve_socket_path(runtime: &Path, project: &str) -> PathBuf {
    let natural = runtime.join("serve.sock");
    if natural.as_os_str().len() <= MAX_UNIX_SOCKET_PATH {
        return natural;
    }
    short_socket_root()
        .join(format!("autumn-{}", short_project_id(project)))
        .join("s.sock")
}

/// A short, stable id for `project` (first 4 bytes of SHA-256, 8 hex chars) used
/// to keep the fallback socket dir unique per project but tiny.
fn short_project_id(project: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(project.as_bytes())[..4])
}

/// Private, short base dir for the fallback socket: `$XDG_RUNTIME_DIR` when set
/// (Linux), else the per-user temp dir (`$TMPDIR` on macOS is per-user-private).
fn short_socket_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// The resolved directories, in a form that can be recorded and read back.
///
/// A Windows service registered by `autumn serve install-service` runs as Local
/// System, which cannot resolve the installing user's `%LOCALAPPDATA%`. Handing
/// it the *resolved* directories, rather than a project name to re-resolve,
/// keeps the service and the user's own `autumn serve status` looking at one set
/// of files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeParts {
    /// PID lockfile, address file, mode marker, readiness and stop-request files.
    pub runtime: PathBuf,
    /// Root of the managed-Postgres data dir.
    pub data: PathBuf,
    /// Daemon log files.
    pub logs: PathBuf,
    /// Unix socket path (unused on Windows, carried so the round trip is total).
    pub socket: PathBuf,
}

impl RuntimePaths {
    /// The resolved directories, for recording.
    #[must_use]
    pub fn parts(&self) -> RuntimeParts {
        RuntimeParts {
            runtime: self.runtime.clone(),
            data: self.data.clone(),
            logs: self.logs.clone(),
            socket: self.socket.clone(),
        }
    }

    /// Rebuild from recorded directories, bypassing platform resolution.
    #[must_use]
    pub fn from_parts(parts: RuntimeParts) -> Self {
        Self {
            runtime: parts.runtime,
            data: parts.data,
            logs: parts.logs,
            socket: parts.socket,
        }
    }

    /// Resolve platform directories for `project`.
    ///
    /// Honours `AUTUMN_RUNTIME_DIR` (rooting everything at
    /// `$AUTUMN_RUNTIME_DIR/<project>`); otherwise uses the per-OS standard
    /// locations via the `directories` crate.
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::NoPlatformDir`] when no standard directory can be
    /// determined for the current user.
    pub fn resolve(project: &str) -> Result<Self, PathsError> {
        if let Some(base) = std::env::var_os(RUNTIME_DIR_ENV) {
            // Resolve a relative override against the launcher's CWD: `base_command` runs the
            // child from the package's manifest dir, and the socket and pidfile paths handed to
            // it must resolve the same there.
            let base = PathBuf::from(base);
            let base = match (base.is_absolute(), std::env::current_dir()) {
                (false, Ok(cwd)) => cwd.join(&base),
                _ => base,
            };
            return Ok(Self::from_base(&base, project));
        }

        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, project)
            .ok_or(PathsError::NoPlatformDir)?;

        // On Windows `data_dir()` is roaming `%APPDATA%`, and none of this state may roam: a
        // pidfile is machine-specific, a managed-Postgres cluster must not sync at logoff or sit
        // on a redirected UNC share, and a Local System service cannot reach a redirected folder.
        let base = if cfg!(windows) {
            dirs.data_local_dir()
        } else {
            dirs.data_dir()
        };
        // `runtime_dir()` is `Some` only on Linux when `XDG_RUNTIME_DIR` is set;
        // fall back to a `run/` subdir of the data dir on macOS/Windows and on
        // headless Linux so the daemon never writes to cwd or `/tmp`.
        let runtime = dirs
            .runtime_dir()
            .map_or_else(|| base.join("run"), Path::to_path_buf);
        let data = base.to_path_buf();
        // Prefer the XDG state dir for logs on Linux; otherwise nest under data.
        let logs = dirs
            .state_dir()
            .map_or_else(|| base.join("logs"), |s| s.join("logs"));

        let socket = resolve_socket_path(&runtime, project);
        Ok(Self {
            runtime,
            data,
            logs,
            socket,
        })
    }

    /// Construct paths rooted at `base/<project>` without touching the
    /// filesystem or environment. This is the pure unit-test seam.
    #[must_use]
    pub fn from_base(base: &Path, project: &str) -> Self {
        let root = base.join(project);
        let socket = resolve_socket_path(&root, project);
        Self {
            runtime: root.clone(),
            data: root.clone(),
            logs: root,
            socket,
        }
    }

    /// PID lockfile path (`<runtime>/serve.pid`).
    #[must_use]
    pub fn pid_file(&self) -> PathBuf {
        self.runtime.join("serve.pid")
    }

    /// Unix domain socket path (`<runtime>/serve.sock`, or a short fallback when
    /// that would exceed the OS `sun_path` limit).
    #[must_use]
    pub fn socket_file(&self) -> PathBuf {
        self.socket.clone()
    }

    /// Address-discovery file path (`<runtime>/serve.addr`).
    #[must_use]
    pub fn addr_file(&self) -> PathBuf {
        self.runtime.join("serve.addr")
    }

    /// Mode-marker file (`<runtime>/serve.mode`): the daemon's `release`/`profile`
    /// recorded separately from `serve.addr` so `restart` can recover them even
    /// if the address file is deleted/corrupted while the daemon runs.
    #[must_use]
    pub fn mode_file(&self) -> PathBuf {
        self.runtime.join("serve.mode")
    }

    /// Readiness-signal file (`<runtime>/serve.ready`). The daemon creates it
    /// after `mark_startup_complete()`; the supervisor polls for it to detect
    /// true startup completion without probing HTTP.
    #[must_use]
    pub fn ready_file(&self) -> PathBuf {
        self.runtime.join("serve.ready")
    }

    /// Cooperative-shutdown request file (`<runtime>/serve.stop`).
    ///
    /// Where there is no `SIGTERM` to send (Windows), `autumn serve stop` asks
    /// the daemon to drain by creating this file and the daemon watches for it
    /// via `AUTUMN_SHUTDOWN_SIGNAL_FILE`. A fixed leaf of the runtime dir, not a
    /// path the daemon chooses, so a `stop` in a different shell finds it.
    #[must_use]
    pub fn stop_file(&self) -> PathBuf {
        self.runtime.join("serve.stop")
    }

    /// The Windows service record (`<runtime>/serve.service.toml`): what
    /// `install-service` wrote for the Service Control Manager's hosted process
    /// to read back. Inside the runtime dir, so it inherits its owner-only ACL.
    #[must_use]
    pub fn service_record_file(&self) -> PathBuf {
        self.runtime.join("serve.service.toml")
    }

    /// Managed-Postgres cluster data directory (`<data>/pg`).
    #[must_use]
    pub fn pg_data_dir(&self) -> PathBuf {
        self.data.join("pg")
    }

    /// Daemon log file path (`<logs>/serve.log`).
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.logs.join("serve.log")
    }

    /// Create the runtime and log directories if they do not exist.
    /// Create the runtime, log, and data directories if they do not exist.
    ///
    /// The data dir is the parent of [`pg_data_dir`](Self::pg_data_dir); on
    /// Linux with `XDG_RUNTIME_DIR` set it is a distinct tree from runtime/logs,
    /// so it must be created here or managed-Postgres `initdb` would fail on a
    /// missing parent.
    ///
    /// # Errors
    ///
    /// Returns the first I/O error encountered creating a directory.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.runtime)?;
        std::fs::create_dir_all(&self.logs)?;
        std::fs::create_dir_all(&self.data)?;
        // Harden the runtime dir: it holds `serve.pid`/`serve.addr`/`serve.ready`,
        // which lifecycle commands trust. Another local user able to create or
        // replace them (permissive umask, shared `AUTUMN_RUNTIME_DIR`) could
        // redirect `stop`/`status` at the wrong process or strand the daemon.
        #[cfg(unix)]
        harden_private_dir(&self.runtime)?;
        // Windows equivalent (#1639). It covers the log and data dirs too: the `(OI)(CI)` grants
        // are inherited, so every file created inside gets an owner-only ACL.
        #[cfg(windows)]
        {
            restrict_to_owner(&self.runtime)?;
            restrict_to_owner(&self.logs)?;
            restrict_to_owner(&self.data)?;
        }
        // The socket's parent (the runtime dir, or a short fallback under a shared temp root) must
        // be `0700` before the app binds: `UnixListener::bind` briefly leaves the socket
        // world-accessible.
        let socket_parent = self.socket.parent().unwrap_or(self.runtime.as_path());
        std::fs::create_dir_all(socket_parent)?;
        #[cfg(unix)]
        harden_private_dir(socket_parent)?;
        Ok(())
    }
}

/// Enforce that `dir` is a real, owner-only (`0700`) directory.
///
/// Rejects a symlink (which a local user could repoint) and — by treating a
/// failed `chmod` as fatal rather than ignoring it — a directory owned by
/// another user, e.g. one pre-created in a shared temp root. This prevents
/// binding the daemon's control socket in an attacker-controlled location.
#[cfg(unix)]
fn harden_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to use {} for the daemon socket: not a directory",
                dir.display()
            ),
        ));
    }
    // A failed `chmod` is not enough: a root launch can `chmod` an attacker's pre-created dir to
    // `0700` while that user still owns it, so compare the owner with the effective uid.
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to use {} for the daemon socket: not owned by the current user",
                dir.display()
            ),
        ));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// The `icacls` arguments that restrict `dir` to `owner`, `SYSTEM` and the local
/// `Administrators` group.
///
/// `/inheritance:r` drops inherited ACEs, and `/grant:r` *replaces* rather than
/// adds — but only for the trustees it names. Neither touches an **explicit** ACE
/// belonging to some other SID, which is exactly what a local user who
/// pre-created this directory under a shared root would leave behind. So the
/// caller runs [`reset_dacl_args`] first: `/reset` drops every explicit ACE (and
/// restores inheritance), then this call strips the inherited ones and installs
/// our three as the entire DACL. Without that first step the directory keeps a
/// stranger's write access to `serve.service.toml` — the file naming the binary
/// a Local System service executes. `(OI)(CI)F` grants full
/// control and marks the ACE inheritable by files and subdirectories, which is
/// how the daemon log, address file and managed-Postgres cluster inherit the
/// same restriction without a per-file call.
///
/// The two built-in trustees are named by **well-known SID**, not by name:
/// `SYSTEM` and `Administrators` are localized, and a German or Japanese Windows
/// would fail to resolve them and refuse the grant. `SYSTEM` is kept because a
/// service registered by `autumn serve install-service` runs as Local System and
/// must reach the state the installing user created.
///
/// Pure, so the ACL policy is unit-tested on every platform rather than only on
/// the one that applies it.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the Windows hardening arm, compiled and tested everywhere"
    )
)]
fn owner_only_acl_args(dir: &Path, owner: &str) -> Vec<std::ffi::OsString> {
    /// `S-1-5-18` — Local System.
    const LOCAL_SYSTEM_SID: &str = "*S-1-5-18";
    /// `S-1-5-32-544` — the built-in Administrators group.
    const ADMINISTRATORS_SID: &str = "*S-1-5-32-544";
    /// Full control, inherited by contained objects and containers.
    const FULL_INHERITED: &str = ":(OI)(CI)F";

    let mut args = vec![dir.as_os_str().to_os_string()];
    args.push("/inheritance:r".into());
    for trustee in [owner, LOCAL_SYSTEM_SID, ADMINISTRATORS_SID] {
        args.push("/grant:r".into());
        args.push(format!("{trustee}{FULL_INHERITED}").into());
    }
    // Suppress the per-file success chatter; failures still print.
    args.push("/Q".into());
    args
}

/// The `icacls` arguments that drop every **explicit** ACE on `dir`.
///
/// `/T` makes it recursive. `/L` acts on a link, not its target. Without it, a file a stranger pre-created inside
/// keeps its explicit ACE, because an inherited ACE does not replace an explicit one.
///
/// `/reset` replaces the DACL with the parent's inherited one, which is the only
/// icacls operation that removes an ACE belonging to a trustee we cannot name in
/// advance. It is a step, not a destination: it leaves the directory as
/// permissive as its parent, and [`owner_only_acl_args`] runs immediately after
/// to strip those inherited entries and install ours. Under the default
/// `%LOCALAPPDATA%` root the intermediate state is already owner-only, so the
/// window this opens is real only where the operator pointed
/// `AUTUMN_RUNTIME_DIR` somewhere shared — and there it replaces a state that
/// was strictly worse.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the Windows hardening arm, compiled and tested everywhere"
    )
)]
fn reset_dacl_args(dir: &Path) -> Vec<std::ffi::OsString> {
    vec![
        dir.as_os_str().to_os_string(),
        "/reset".into(),
        "/T".into(),
        "/L".into(),
        "/Q".into(),
    ]
}

/// The trustee to grant, parsed from `whoami /user /fo csv /nh` output.
///
/// The **token's SID**, not `%USERDOMAIN%\%USERNAME%`. Those are ordinary
/// inherited environment variables, and naming an account by name breaks in
/// exactly the contexts that matter here: a build agent running as Local System
/// resolves to `WORKGROUP\MACHINE$`, which `icacls` cannot map on a
/// non-domain-joined host, and an Entra-joined device's `AzureAD\<name>` often
/// fails to resolve too. Because hardening fails closed, an unresolvable
/// trustee would not degrade the ACL — it would refuse to start the daemon at
/// all.
///
/// `icacls` accepts a SID as `*S-1-5-…`, the same spelling this module already
/// uses for the two built-in trustees.
///
/// The line looks like `"CORP\dev","S-1-5-21-…-1001"`. Pure, so the parsing is
/// tested on every platform.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the Windows hardening arm, compiled and tested everywhere"
    )
)]
fn owner_sid_from_whoami(output: &str) -> Option<String> {
    let sid = output
        .lines()
        .find(|line| line.contains("S-1-"))?
        .rsplit(',')
        .next()?
        .trim()
        .trim_matches('"')
        .trim();
    // A SID is `S-1-<authority>-<subauthorities>`; anything else must not be pasted into an ACL.
    let is_sid = sid.starts_with("S-1-")
        && sid.len() > 4
        && sid[4..].chars().all(|c| c.is_ascii_digit() || c == '-');
    is_sid.then(|| format!("*{sid}"))
}

/// A tool under `%SystemRoot%\System32`.
///
/// Absolute so a `PATH` entry — or, for `CreateProcess`, the current directory,
/// which is the operator's project — cannot shadow it. `SystemRoot` is set by
/// the OS; a caller who can rewrite our environment can already run code as us,
/// so reading it here adds no exposure.
#[cfg(windows)]
fn system32_tool(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned()))
        .join("System32")
        .join(name)
}

/// The current token's SID, for use as an `icacls` trustee.
#[cfg(windows)]
fn current_owner_sid() -> std::io::Result<String> {
    let output = std::process::Command::new(system32_tool("whoami.exe"))
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    owner_sid_from_whoami(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "could not determine this account's SID, so daemon state cannot be \
             restricted to it",
        )
    })
}

/// Restrict `dir` to its owner, `SYSTEM` and `Administrators`.
///
/// Fails **closed**, exactly as the Unix `harden_private_dir` does: the runtime
/// dir holds the records `stop`/`status` act on, so starting a daemon while they
/// stay writable by other local users is the outcome this exists to prevent.
///
/// Three steps, mirroring what the Unix arm checks:
///
/// 1. Refuse a **reparse point**. A junction needs no privilege to create and
///    `icacls` follows one, so hardening a pre-created junction would strip and
///    rewrite the ACL of whatever it points at — and the daemon would then write
///    its state there.
/// 2. Take **ownership**. An object's owner implicitly keeps `WRITE_DAC`, so a
///    directory another local user pre-created stays theirs to re-open whatever
///    DACL we write. Taking ownership from someone else needs privilege, so on a
///    foreign-owned directory this step fails and the daemon refuses — the
///    Windows analog of the Unix `st_uid` check.
/// 3. Apply the DACL.
///
/// One gap is documented rather than papered over: the directory is created
/// before it is hardened, so on a shared root a local user racing the creation
/// can open a handle under the inherited ACL and keep it (Windows checks access
/// at open, not at use). Closing that needs an explicit security descriptor at
/// creation time, which is not reachable without `unsafe`. Under the default
/// `%LOCALAPPDATA%` root there is no such race — the parent is already
/// owner-only.
#[cfg(windows)]
fn restrict_to_owner(dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to use {} for daemon state: it is a link or not a \
                 directory, so its access cannot be restricted",
                dir.display()
            ),
        ));
    }
    let owner = current_owner_sid()?;
    let icacls = system32_tool("icacls.exe");
    // Take ownership first: another owner could rewrite the DACL, and the DACL rewrite below
    // fails on a directory we do not own.
    let take_ownership = vec![
        dir.as_os_str().to_os_string(),
        "/setowner".into(),
        std::ffi::OsString::from(&owner),
        "/T".into(),
        "/L".into(),
        "/Q".into(),
    ];
    run_icacls(&icacls, dir, &take_ownership, &owner)?;
    // Clear every explicit ACE: `/grant:r` replaces grants only for the trustees it names, so a
    // stranger's pre-created ACE would otherwise keep write access.
    run_icacls(&icacls, dir, &reset_dacl_args(dir), &owner)?;
    run_icacls(&icacls, dir, &owner_only_acl_args(dir, &owner), &owner)
}

/// Run one `icacls` invocation, turning a non-zero exit into a fail-closed error.
#[cfg(windows)]
fn run_icacls(
    icacls: &Path,
    dir: &Path,
    args: &[std::ffi::OsString],
    owner: &str,
) -> std::io::Result<()> {
    let output = std::process::Command::new(icacls).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "refusing to use {} for daemon state: could not restrict it to {owner} \
             ({} exited {}): {}",
            dir.display(),
            icacls.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_from_base_layout() {
        let paths = RuntimePaths::from_base(Path::new("/var/run"), "demo");
        assert_eq!(paths.pid_file(), Path::new("/var/run/demo/serve.pid"));
        assert_eq!(paths.socket_file(), Path::new("/var/run/demo/serve.sock"));
        assert_eq!(paths.addr_file(), Path::new("/var/run/demo/serve.addr"));
        assert_eq!(paths.pg_data_dir(), Path::new("/var/run/demo/pg"));
        assert_eq!(paths.log_file(), Path::new("/var/run/demo/serve.log"));
    }

    #[test]
    fn socket_uses_natural_path_when_short() {
        let paths = RuntimePaths::from_base(Path::new("/var/run"), "demo");
        assert_eq!(paths.socket_file(), Path::new("/var/run/demo/serve.sock"));
        assert!(paths.socket_file().as_os_str().len() <= MAX_UNIX_SOCKET_PATH);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_dirs_makes_socket_parent_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("ensure_dirs");
        let parent = paths.socket_file().parent().unwrap().to_path_buf();
        let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "socket dir must be owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn harden_private_dir_rejects_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(harden_private_dir(&link).is_err());
    }

    #[test]
    fn socket_falls_back_to_short_path_when_runtime_too_long() {
        let long_base = format!("/{}", "longsegment".repeat(12)); // ~133 chars
        // Unset `XDG_RUNTIME_DIR` so the fallback lands under the temp-dir root.
        temp_env::with_var("XDG_RUNTIME_DIR", None::<&str>, || {
            let paths = RuntimePaths::from_base(Path::new(&long_base), "proj");
            let sock = paths.socket_file();
            assert!(
                sock.as_os_str().len() <= MAX_UNIX_SOCKET_PATH,
                "socket path still too long ({} bytes): {}",
                sock.as_os_str().len(),
                sock.display()
            );
            assert_ne!(sock, Path::new(&long_base).join("proj").join("serve.sock"));
            // Only the socket moves; pidfile and addr have no length limit.
            assert!(paths.pid_file().starts_with(&long_base));
            assert!(paths.addr_file().starts_with(&long_base));
        });
    }

    // `/tmp/...` is not absolute on Windows (no drive or UNC prefix).
    #[cfg(unix)]
    #[test]
    fn paths_socket_absolute_under_base() {
        let paths = RuntimePaths::from_base(Path::new("/tmp/xdg-base"), "svc");
        let sock = paths.socket_file();
        assert!(sock.is_absolute());
        assert!(sock.starts_with("/tmp/xdg-base"));
    }

    // ── Windows daemon state (#1639) ─────────────────────────────────────
    //
    // The ACL argument builders run on every platform, so they are tested off Windows too.

    #[test]
    fn stop_request_file_sits_beside_the_pidfile() {
        // `stop` runs from a different shell than `start`, so the file is a fixed leaf of the
        // runtime dir.
        let paths = RuntimePaths::from_base(Path::new("/var/run"), "demo");
        assert_eq!(paths.stop_file(), Path::new("/var/run/demo/serve.stop"));
        assert_eq!(
            paths.stop_file().parent(),
            paths.pid_file().parent(),
            "stop request and pidfile must share a directory"
        );
    }

    #[test]
    fn owner_only_acl_grants_exactly_the_owner_system_and_administrators() {
        let args = owner_only_acl_args(Path::new(r"C:\state\demo"), "*S-1-5-21-1-2-3-1001");
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(rendered[0], r"C:\state\demo");
        assert!(
            rendered.contains(&"/inheritance:r".to_owned()),
            "inherited ACEs must be dropped, or a permissive parent leaks in: {rendered:?}"
        );
        // Exactly three trustees, each granted with `/grant:r` (replace, not add).
        let grants = rendered
            .iter()
            .skip_while(|a| *a != "/grant:r")
            .filter(|a| a.contains(":(OI)(CI)F"))
            .count();
        assert_eq!(grants, 3, "{rendered:?}");
        assert_eq!(
            rendered.iter().filter(|a| *a == "/grant:r").count(),
            3,
            "every trustee needs its own /grant:r: {rendered:?}"
        );
        assert!(
            rendered.contains(&"*S-1-5-21-1-2-3-1001:(OI)(CI)F".to_owned()),
            "{rendered:?}"
        );
    }

    #[test]
    fn the_dacl_is_cleared_before_it_is_rebuilt() {
        // `/inheritance:r` drops only inherited entries and `/grant:r` touches only the trustees
        // it names. `/reset` is the only icacls operation that removes an explicit ACE left by a
        // user who pre-created the directory, who could otherwise rewrite `serve.service.toml`.
        let args = reset_dacl_args(Path::new(r"C:\state\demo"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(rendered[0], r"C:\state\demo");
        assert!(rendered.contains(&"/reset".to_owned()), "{rendered:?}");
        // `/reset` alone skips files already inside the directory.
        assert!(rendered.contains(&"/T".to_owned()), "{rendered:?}");
        // `/L` acts on a link itself, so a planted junction is not followed.
        assert!(rendered.contains(&"/L".to_owned()), "{rendered:?}");
        // No grants here: `/reset` restores inherited access, so it only prepares the DACL.
        assert!(
            !rendered.iter().any(|a| a.starts_with("/grant")),
            "{rendered:?}"
        );
    }

    #[test]
    fn owner_only_acl_names_the_builtin_trustees_by_sid_not_by_localized_name() {
        // "SYSTEM" and "Administrators" are localized names that fail to resolve on, e.g., a
        // German Windows. Well-known SIDs are locale-independent.
        let args = owner_only_acl_args(Path::new("state"), "*S-1-5-21-1-2-3-1001");
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            rendered.contains(&"*S-1-5-18:(OI)(CI)F".to_owned()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&"*S-1-5-32-544:(OI)(CI)F".to_owned()),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|a| a.starts_with("SYSTEM:")),
            "a localized trustee name must not be used: {rendered:?}"
        );
    }

    #[test]
    fn owner_sid_is_taken_from_the_token_not_the_environment() {
        // `whoami /user /fo csv /nh` prints `"DOMAIN\user","SID"`.
        assert_eq!(
            owner_sid_from_whoami(
                "\"CORP\\dev\",\"S-1-5-21-1111111111-222222222-333333333-1001\"\r\n"
            )
            .as_deref(),
            Some("*S-1-5-21-1111111111-222222222-333333333-1001")
        );
    }

    #[test]
    fn owner_sid_refuses_anything_that_is_not_a_sid() {
        // The value goes straight into an ACL, so an error, a localized header or empty output
        // must be refused.
        assert_eq!(owner_sid_from_whoami(""), None);
        assert_eq!(owner_sid_from_whoami("ERROR: Access is denied.\n"), None);
        assert_eq!(owner_sid_from_whoami("\"CORP\\dev\",\"NOT-A-SID\"\n"), None);
        assert_eq!(owner_sid_from_whoami("\"CORP\\dev\",\"S-1-5-x-1\"\n"), None);
    }

    #[cfg(windows)]
    #[test]
    fn ensure_dirs_restricts_the_runtime_dir_to_its_owner() {
        // Windows counterpart of `ensure_dirs_makes_socket_parent_private`: fails if
        // `ensure_dirs` stops calling `restrict_to_owner`.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::from_base(dir.path(), "p");
        paths.ensure_dirs().expect("ensure_dirs");
        let output = std::process::Command::new(system32_tool("icacls.exe"))
            .arg(paths.pid_file().parent().expect("runtime dir"))
            .output()
            .expect("icacls");
        let acl = String::from_utf8_lossy(&output.stdout);
        // An inherited ACL from a shared root brings in `Everyone` and `Users`.
        assert!(
            !acl.contains("S-1-1-0") && !acl.contains("Everyone"),
            "runtime dir is reachable by Everyone: {acl}"
        );
        assert!(
            !acl.contains("S-1-5-32-545") && !acl.contains("\\Users:"),
            "runtime dir is reachable by all local users: {acl}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_puts_windows_state_under_local_app_data() {
        // State must not roam (see `resolve`). The CI journey sets `AUTUMN_RUNTIME_DIR`, which
        // bypasses this path, so only this test covers it.
        temp_env::with_var(RUNTIME_DIR_ENV, None::<&str>, || {
            let paths = RuntimePaths::resolve("demo").expect("resolve");
            let local = std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA");
            assert!(
                paths.pid_file().starts_with(&local),
                "daemon state must live under %LOCALAPPDATA%: {}",
                paths.pid_file().display()
            );
            assert!(
                paths.pg_data_dir().starts_with(&local),
                "the managed cluster must not roam: {}",
                paths.pg_data_dir().display()
            );
        });
    }

    #[test]
    fn runtime_parts_round_trip_without_re_resolving() {
        // A Local System service resolves a different `%LOCALAPPDATA%`, so it must reuse the
        // recorded directories to share a tree with the user's `autumn serve status`.
        let original = RuntimePaths::from_base(Path::new("/var/run"), "demo");
        let rebuilt = RuntimePaths::from_parts(original.parts());
        assert_eq!(rebuilt, original);
        assert_eq!(rebuilt.pid_file(), original.pid_file());
        assert_eq!(rebuilt.log_file(), original.log_file());
        assert_eq!(rebuilt.pg_data_dir(), original.pg_data_dir());
    }

    #[test]
    fn service_record_lives_inside_the_restricted_runtime_dir() {
        // It names the binary a Local System service runs, so other users must not rewrite it.
        let paths = RuntimePaths::from_base(Path::new("/var/run"), "demo");
        assert_eq!(
            paths.service_record_file().parent(),
            paths.pid_file().parent()
        );
    }

    #[test]
    fn resolve_honors_env_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        temp_env::with_var(RUNTIME_DIR_ENV, Some(dir.path()), || {
            let paths = RuntimePaths::resolve("demo").expect("resolve with override");
            assert_eq!(paths.pid_file(), dir.path().join("demo").join("serve.pid"));
            assert!(paths.socket_file().starts_with(dir.path()));
        });
    }
}
