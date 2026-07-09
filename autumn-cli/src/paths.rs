//! Platform-appropriate runtime/data/log directories for `autumn serve`.
//!
//! Daemon state (PID lockfile, Unix socket, address-discovery file), the
//! managed-Postgres data dir, and log files must live under per-OS standard
//! locations (XDG on Linux, `~/Library/Application Support` on macOS,
//! `%APPDATA%`/`%LOCALAPPDATA%` on Windows) — never the current working
//! directory, `/etc`, or `/tmp` (a predictable path under `/tmp` is a symlink
//! hazard for the `0600` socket).
//!
//! [`RuntimePaths::resolve`] performs the real per-OS resolution and honours
//! the `AUTUMN_RUNTIME_DIR` override used by integration tests.
//! [`RuntimePaths::from_base`] is the pure, fs-free seam the unit tests drive.

#![allow(dead_code, clippy::missing_const_for_fn)]

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

impl RuntimePaths {
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
            // Make a relative override absolute (against the launcher's CWD) so
            // the socket/pidfile paths handed to the child resolve identically
            // even though `base_command` runs the child from the package's
            // manifest dir.
            let base = PathBuf::from(base);
            let base = match (base.is_absolute(), std::env::current_dir()) {
                (false, Ok(cwd)) => cwd.join(&base),
                _ => base,
            };
            return Ok(Self::from_base(&base, project));
        }

        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, project)
            .ok_or(PathsError::NoPlatformDir)?;

        // `runtime_dir()` is `Some` only on Linux when `XDG_RUNTIME_DIR` is set;
        // fall back to a `run/` subdir of the data dir on macOS/Windows and on
        // headless Linux so the daemon never writes to cwd or `/tmp`.
        let runtime = dirs
            .runtime_dir()
            .map_or_else(|| dirs.data_dir().join("run"), Path::to_path_buf);
        let data = dirs.data_dir().to_path_buf();
        // Prefer the XDG state dir for logs on Linux; otherwise nest under data.
        let logs = dirs
            .state_dir()
            .map_or_else(|| dirs.data_dir().join("logs"), |s| s.join("logs"));

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
        // Harden the directory the control socket is bound in (the runtime dir,
        // or a short fallback under a shared temp root). Making it `0700` *before*
        // the app binds means no other local user can reach the socket during the
        // brief window where `UnixListener::bind` leaves it world-accessible.
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
    // Reject a directory owned by another user. Relying on `chmod` to fail isn't
    // enough: a privileged (root) launch can `chmod` an attacker-pre-created dir
    // to `0700` while it stays owned/traversable by that user, so check `st_uid`
    // against the effective uid explicitly.
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
        // A base long enough to push `<runtime>/serve.sock` past the limit.
        let long_base = format!("/{}", "longsegment".repeat(12)); // ~133 chars
        // Force the temp-dir root so the short path is deterministic/short.
        temp_env::with_var("XDG_RUNTIME_DIR", None::<&str>, || {
            let paths = RuntimePaths::from_base(Path::new(&long_base), "proj");
            let sock = paths.socket_file();
            assert!(
                sock.as_os_str().len() <= MAX_UNIX_SOCKET_PATH,
                "socket path still too long ({} bytes): {}",
                sock.as_os_str().len(),
                sock.display()
            );
            // It is *not* the (too-long) natural path …
            assert_ne!(sock, Path::new(&long_base).join("proj").join("serve.sock"));
            // … while pidfile/addr stay in the long runtime dir (no length limit).
            assert!(paths.pid_file().starts_with(&long_base));
            assert!(paths.addr_file().starts_with(&long_base));
        });
    }

    // Uses a Unix-style absolute base; `/tmp/...` is not absolute on Windows
    // (no drive/UNC prefix), so the absoluteness assertion is unix-specific.
    #[cfg(unix)]
    #[test]
    fn paths_socket_absolute_under_base() {
        let paths = RuntimePaths::from_base(Path::new("/tmp/xdg-base"), "svc");
        let sock = paths.socket_file();
        assert!(sock.is_absolute());
        assert!(sock.starts_with("/tmp/xdg-base"));
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
