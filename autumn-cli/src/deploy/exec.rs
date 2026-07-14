//! Injectable remote-execution layer for `autumn deploy` (issue #1607, Slice 1).
//!
//! This module turns the previously-dry-run first-deploy plan into REAL
//! execution behind an injectable [`DeployExecutor`], so the command-construction
//! and execution paths are unit-testable without a live host.
//!
//! ## What is real here
//!
//! - **Pure command construction:** [`first_deploy_ops`] turns a resolved deploy
//!   config into an ordered `Vec<DeployOp>` (prepare dirs → upload binary → write
//!   env file → write unit → link `current` → `daemon-reload` → `enable --now` →
//!   readiness poll). It performs no I/O, so tests assert the exact ordered
//!   sequence and the env-file mode.
//! - **Execution:** [`run_ops`] / [`execute_first_deploy`] iterate the ops and
//!   drive a [`DeployExecutor`]; [`execute_first_deploy`] additionally refuses to
//!   run if any preflight check failed (AC-6 fail-fast — abort *before* touching
//!   the server).
//! - **Real ssh/scp:** [`SshExecutor`] shells out to the system `ssh`/`scp`
//!   binaries via [`std::process::Command`] (no ssh crate is pulled in). The argv
//!   builders [`ssh_argv`]/[`scp_argv`] are pure functions so tests assert the
//!   exact argument vector without executing anything.
//! - **Secret redaction:** the env-file contents travel as a [`Secret`] whose
//!   `Debug`/`Display` are redacted, and secrets are only ever written to a
//!   `0600` file — never placed on a command line or into an error message.
//!
//! ## What is deferred (NOT implemented in this slice)
//!
//! - Zero-downtime cutover — reverse-proxy upstream swap or systemd
//!   socket-activation handoff (later slice).
//! - Migration execution ordering against the live host (later slice).
//! - Rollback execution and auto-rollback on a failed readiness gate — a
//!   first-deploy readiness timeout here just fails loudly (later slice).
//! - The CI end-to-end container harness that exercises the real `ssh` path
//!   (later slice).
//!
//! On a *re*deploy (a `current` release already exists) this slice deliberately
//! takes the same first-deploy path (re-pointing `current` at the new release);
//! zero-downtime cutover and rollback are follow-up slices.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{PreflightCheck, ResolvedDeployConfig};

/// A secret string (e.g. the env-file body carrying the signing secret and
/// database URL) whose `Debug`/`Display` are redacted so it can never leak into
/// a log line, a panic message, or an error's formatted output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the underlying bytes. Callers must only use this to write the
    /// secret to a `0600` file — never to log it or place it on a command line.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Contents destined for a remote file.
#[derive(Debug, Clone)]
pub enum FileContents {
    /// Non-secret content (e.g. the systemd unit); safe to appear in `Debug`.
    Plain(String),
    /// Secret content (the env file); redacted in `Debug`/`Display`.
    Secret(Secret),
}

impl FileContents {
    /// Borrow the raw bytes to stage them for upload. For [`FileContents::Secret`]
    /// this exposes the secret and must only feed a `0600` local temp file.
    #[must_use]
    fn as_str(&self) -> &str {
        match self {
            Self::Plain(s) => s,
            Self::Secret(s) => s.expose(),
        }
    }
}

/// A structured remote shell command.
///
/// `shell` is the line executed on the target host; `label` names the step for
/// logs and error messages. Command construction never places a secret *value*
/// in a `RemoteCommand` — secrets travel only as [`DeployOp::WriteFile`] payloads
/// wrapped in [`Secret`] — so a `RemoteCommand`'s `Debug` output is always safe
/// to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommand {
    /// Short, stable label naming the step.
    pub label: &'static str,
    /// The shell line executed on the remote host.
    pub shell: String,
}

impl RemoteCommand {
    fn new(label: &'static str, shell: impl Into<String>) -> Self {
        Self {
            label,
            shell: shell.into(),
        }
    }
}

/// Captured output of a completed remote command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

/// A single ordered operation in a deploy sequence.
#[derive(Debug)]
pub enum DeployOp {
    /// Run a remote shell command.
    Run(RemoteCommand),
    /// Upload an already-on-disk local file (the built release binary) to a
    /// remote path, then `chmod` it to `mode`.
    UploadFile {
        /// Short label naming the step.
        label: &'static str,
        /// Local source path (already exists on disk).
        local: PathBuf,
        /// Absolute remote destination path.
        remote_path: String,
        /// Permission bits applied after upload.
        mode: Option<u32>,
    },
    /// Write in-memory contents (the systemd unit, or the secret env file) to a
    /// remote path with a mode. The executor stages a local temp file first, so
    /// secrets never touch a command line.
    WriteFile {
        /// Short label naming the step.
        label: &'static str,
        /// Contents to write remotely (possibly a [`Secret`]).
        contents: FileContents,
        /// Absolute remote destination path.
        remote_path: String,
        /// Permission bits applied after upload (e.g. `0o600` for the env file).
        mode: Option<u32>,
    },
}

impl DeployOp {
    /// The step's label, for progress logging.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Run(cmd) => cmd.label,
            Self::UploadFile { label, .. } | Self::WriteFile { label, .. } => label,
        }
    }
}

/// Errors surfaced by the deploy executor. None of these variants ever embeds a
/// secret value — messages carry labels, remote paths, and redacted transport
/// stderr only.
#[derive(Debug, thiserror::Error)]
pub enum DeployExecError {
    /// The `ssh`/`scp` process could not be launched.
    #[error("failed to launch `{program}`: {source}")]
    Spawn {
        /// The program that failed to spawn.
        program: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// A remote command completed with a non-zero exit status.
    #[error("remote command `{label}` failed: {message}")]
    CommandFailed {
        /// The failing step's label.
        label: &'static str,
        /// A redacted, actionable message (exit status + transport stderr).
        message: String,
    },
    /// A file upload failed.
    #[error("upload to `{remote_path}` failed: {message}")]
    UploadFailed {
        /// The remote destination path.
        remote_path: String,
        /// A redacted, actionable message.
        message: String,
    },
    /// A local temp file could not be staged for upload.
    #[error("could not stage a local file for upload: {message}")]
    Stage {
        /// Detail of the staging failure.
        message: String,
    },
    /// Preflight did not pass, so execution was aborted before any remote call.
    #[error(
        "preflight failed: {failed} check(s) did not pass — aborting before touching the server"
    )]
    PreflightAborted {
        /// Number of failing preflight checks.
        failed: usize,
    },
}

/// Executes remote operations for a deploy. Injectable so tests can substitute a
/// recording fake and assert the exact ordered command sequence without a host.
pub trait DeployExecutor {
    /// Run a remote command, returning its captured output on success.
    ///
    /// # Errors
    ///
    /// Returns [`DeployExecError::Spawn`] if the transport cannot be launched and
    /// [`DeployExecError::CommandFailed`] on a non-zero remote exit status.
    fn run(&self, cmd: &RemoteCommand) -> Result<CommandOutput, DeployExecError>;

    /// Upload a local file to `remote_path` and `chmod` it to `mode` (when set).
    ///
    /// # Errors
    ///
    /// Returns [`DeployExecError::UploadFailed`] on transport failure and
    /// [`DeployExecError::CommandFailed`] if the follow-up `chmod` fails.
    fn upload(
        &self,
        local: &Path,
        remote_path: &str,
        mode: Option<u32>,
    ) -> Result<(), DeployExecError>;
}

/// The SSH target derived from a resolved `[deploy]` config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// SSH-reachable host (already validated non-blank).
    pub host: String,
    /// SSH user.
    pub user: String,
    /// SSH port.
    pub port: u16,
}

impl SshTarget {
    /// Build an SSH target from a resolved config, returning `None` when no host
    /// is configured (preflight rejects that case before we ever get here).
    #[must_use]
    pub fn from_resolved(cfg: &ResolvedDeployConfig) -> Option<Self> {
        cfg.host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|host| Self {
                host: host.to_owned(),
                user: cfg.user.clone(),
                port: cfg.ssh_port,
            })
    }
}

/// Non-interactive `ssh`/`scp` options shared by both argv builders. `BatchMode`
/// prevents any password prompt from hanging a deploy, and
/// `StrictHostKeyChecking=accept-new` pins a first-seen host key without
/// blocking on an interactive yes/no.
const SSH_BATCH_OPTS: [&str; 4] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

/// Build the argv passed to the system `ssh` binary to run `remote_shell` on the
/// target. Pure — exposed so tests assert the exact vector without executing.
#[must_use]
pub fn ssh_argv(target: &SshTarget, remote_shell: &str) -> Vec<String> {
    let mut argv = vec!["-p".to_owned(), target.port.to_string()];
    argv.extend(SSH_BATCH_OPTS.iter().map(|s| (*s).to_owned()));
    argv.push(format!("{}@{}", target.user, target.host));
    argv.push(remote_shell.to_owned());
    argv
}

/// Build the argv passed to the system `scp` binary to copy `local` to
/// `remote_path` on the target. Pure — exposed so tests assert the exact vector.
/// Note `scp` spells the port `-P` (uppercase), unlike `ssh`'s `-p`.
#[must_use]
pub fn scp_argv(target: &SshTarget, local: &Path, remote_path: &str) -> Vec<String> {
    let mut argv = vec!["-P".to_owned(), target.port.to_string()];
    argv.extend(SSH_BATCH_OPTS.iter().map(|s| (*s).to_owned()));
    argv.push(local.display().to_string());
    argv.push(format!("{}@{}:{}", target.user, target.host, remote_path));
    argv
}

/// Single-quote a path for safe interpolation into a remote shell line.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Build the ordered first-deploy operation sequence from a resolved config.
///
/// Pure — performs no I/O. The `release_id` and `server_port` are injected so the
/// output is deterministic for tests. The sequence is:
///
/// 1. prepare remote dirs (`mkdir -p {releases}/{id} {app_dir}/shared`),
/// 2. upload the release binary (mode `0755`),
/// 3. write the secret env file (mode `0600`, AC-5 — not world-readable),
/// 4. write the systemd unit,
/// 5. point `current` at the new release (first-deploy promotion; zero-downtime
///    cutover is a later slice),
/// 6. `systemctl daemon-reload`,
/// 7. `systemctl enable --now`,
/// 8. a bounded remote readiness poll of `/ready` (fails loudly on timeout — no
///    auto-rollback in this slice).
#[must_use]
pub fn first_deploy_ops(
    cfg: &ResolvedDeployConfig,
    unit: &str,
    env_file: Secret,
    binary_local: &Path,
    release_id: &str,
    server_port: u16,
) -> Vec<DeployOp> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    let remote_binary = format!("{release_dir}/{}", cfg.app_name);
    let shared_dir = format!("{}/shared", cfg.app_dir);
    let env_path = cfg.env_file();
    let current = cfg.current_symlink();
    let unit_path = format!("/etc/systemd/system/{}.service", cfg.service_name);

    vec![
        DeployOp::Run(RemoteCommand::new(
            "prepare-dirs",
            format!(
                "mkdir -p {} {}",
                shell_quote(&release_dir),
                shell_quote(&shared_dir)
            ),
        )),
        DeployOp::UploadFile {
            label: "upload-binary",
            local: binary_local.to_path_buf(),
            remote_path: remote_binary,
            mode: Some(0o755),
        },
        DeployOp::WriteFile {
            label: "write-env",
            contents: FileContents::Secret(env_file),
            remote_path: env_path,
            // 0600: secrets must not be world-readable (AC-5).
            mode: Some(0o600),
        },
        DeployOp::WriteFile {
            label: "write-unit",
            contents: FileContents::Plain(unit.to_owned()),
            remote_path: unit_path,
            mode: Some(0o644),
        },
        DeployOp::Run(RemoteCommand::new(
            "link-current",
            format!(
                "ln -sfn {} {}",
                shell_quote(&release_dir),
                shell_quote(&current)
            ),
        )),
        DeployOp::Run(RemoteCommand::new(
            "daemon-reload",
            "systemctl daemon-reload",
        )),
        DeployOp::Run(RemoteCommand::new(
            "enable-now",
            format!("systemctl enable --now {}.service", cfg.service_name),
        )),
        DeployOp::Run(RemoteCommand::new(
            "readiness-gate",
            readiness_poll_shell(server_port, cfg.readiness_timeout_secs),
        )),
    ]
}

/// Build the bounded remote readiness-poll shell line: loop on
/// `curl -fsS localhost:{port}/ready` until it succeeds or `timeout_secs`
/// elapses, exiting non-zero (which the executor maps to a failure) on timeout.
#[must_use]
fn readiness_poll_shell(port: u16, timeout_secs: u64) -> String {
    format!(
        "end=$(($(date +%s)+{timeout_secs})); \
         until curl -fsS http://127.0.0.1:{port}/ready >/dev/null 2>&1; do \
         if [ $(date +%s) -ge $end ]; then \
         echo 'readiness gate: /ready not healthy within {timeout_secs}s' >&2; exit 1; \
         fi; sleep 2; done"
    )
}

/// Execute an op sequence against `exec`, but only after preflight has passed.
///
/// If any check failed the function returns [`DeployExecError::PreflightAborted`]
/// WITHOUT making a single executor call (AC-6 — fail fast before touching the
/// server).
///
/// # Errors
///
/// Returns [`DeployExecError::PreflightAborted`] when any preflight check failed,
/// or the first executor error otherwise.
pub fn execute_first_deploy(
    checks: &[PreflightCheck],
    ops: &[DeployOp],
    exec: &impl DeployExecutor,
) -> Result<(), DeployExecError> {
    let failed = checks.iter().filter(|c| !c.passed).count();
    if failed > 0 {
        return Err(DeployExecError::PreflightAborted { failed });
    }
    run_ops(ops, exec)
}

/// Drive an op sequence against an executor, stopping at the first failure.
///
/// # Errors
///
/// Returns the first [`DeployExecError`] produced by the executor (or by staging
/// a local temp file for a [`DeployOp::WriteFile`]).
pub fn run_ops(ops: &[DeployOp], exec: &impl DeployExecutor) -> Result<(), DeployExecError> {
    for op in ops {
        // Progress logging: only the step label (never a secret) is emitted.
        eprintln!("  \u{2192} {}", op.label());
        match op {
            DeployOp::Run(cmd) => {
                exec.run(cmd)?;
            }
            DeployOp::UploadFile {
                local,
                remote_path,
                mode,
                ..
            } => {
                exec.upload(local, remote_path, *mode)?;
            }
            DeployOp::WriteFile {
                contents,
                remote_path,
                mode,
                ..
            } => {
                let staged = stage_temp_file(contents.as_str(), *mode)?;
                exec.upload(staged.path(), remote_path, *mode)?;
            }
        }
    }
    Ok(())
}

/// Stage in-memory contents into a local temp file (permissioned to `mode` on
/// unix so the secret is not briefly world-readable on the deploy host's disk
/// either) and return the handle. The file is deleted when the handle drops.
fn stage_temp_file(
    contents: &str,
    mode: Option<u32>,
) -> Result<tempfile::NamedTempFile, DeployExecError> {
    let mut file = tempfile::NamedTempFile::new().map_err(|e| DeployExecError::Stage {
        message: e.to_string(),
    })?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(file.path(), perms).map_err(|e| DeployExecError::Stage {
            message: e.to_string(),
        })?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.write_all(contents.as_bytes())
        .map_err(|e| DeployExecError::Stage {
            message: e.to_string(),
        })?;
    file.flush().map_err(|e| DeployExecError::Stage {
        message: e.to_string(),
    })?;
    Ok(file)
}

/// Real executor: shells out to the system `ssh`/`scp` binaries. No ssh crate is
/// used — the argv is built by the pure [`ssh_argv`]/[`scp_argv`] functions.
#[derive(Debug, Clone)]
pub struct SshExecutor {
    target: SshTarget,
}

impl SshExecutor {
    /// Build an executor for the given SSH target.
    #[must_use]
    pub const fn new(target: SshTarget) -> Self {
        Self { target }
    }
}

impl DeployExecutor for SshExecutor {
    fn run(&self, cmd: &RemoteCommand) -> Result<CommandOutput, DeployExecError> {
        let argv = ssh_argv(&self.target, &cmd.shell);
        let output =
            Command::new("ssh")
                .args(&argv)
                .output()
                .map_err(|source| DeployExecError::Spawn {
                    program: "ssh".to_owned(),
                    source,
                })?;
        if output.status.success() {
            Ok(CommandOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        } else {
            // Never echo the command's shell line (it may name secret paths) or
            // stdout (may echo file contents); surface only the exit status and
            // the transport's own stderr.
            Err(DeployExecError::CommandFailed {
                label: cmd.label,
                message: format!(
                    "exit status {}: {}",
                    output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            })
        }
    }

    fn upload(
        &self,
        local: &Path,
        remote_path: &str,
        mode: Option<u32>,
    ) -> Result<(), DeployExecError> {
        let argv = scp_argv(&self.target, local, remote_path);
        let output =
            Command::new("scp")
                .args(&argv)
                .output()
                .map_err(|source| DeployExecError::Spawn {
                    program: "scp".to_owned(),
                    source,
                })?;
        if !output.status.success() {
            return Err(DeployExecError::UploadFailed {
                remote_path: remote_path.to_owned(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        // scp preserves no mode guarantees across platforms, so apply the
        // requested bits with an explicit remote chmod.
        if let Some(mode) = mode {
            let chmod = RemoteCommand::new(
                "chmod",
                format!("chmod {mode:o} {}", shell_quote(remote_path)),
            );
            self.run(&chmod)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A recording fake executor: it records every `run`/`upload` call in order
    /// and returns scripted outputs, so tests assert the exact remote-command
    /// sequence (and env-file mode) without a live host.
    #[derive(Default)]
    struct RecordingExecutor {
        calls: RefCell<Vec<RecordedCall>>,
        /// Labels whose `run` should return a scripted failure (e.g. to simulate
        /// a readiness-gate timeout).
        fail_labels: Vec<&'static str>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedCall {
        Run {
            label: &'static str,
            shell: String,
        },
        Upload {
            remote_path: String,
            mode: Option<u32>,
        },
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self::default()
        }

        fn failing_on(label: &'static str) -> Self {
            Self {
                fail_labels: vec![label],
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.borrow().clone()
        }
    }

    impl DeployExecutor for RecordingExecutor {
        fn run(&self, cmd: &RemoteCommand) -> Result<CommandOutput, DeployExecError> {
            self.calls.borrow_mut().push(RecordedCall::Run {
                label: cmd.label,
                shell: cmd.shell.clone(),
            });
            if self.fail_labels.contains(&cmd.label) {
                return Err(DeployExecError::CommandFailed {
                    label: cmd.label,
                    message: "scripted failure".to_owned(),
                });
            }
            Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        fn upload(
            &self,
            _local: &Path,
            remote_path: &str,
            mode: Option<u32>,
        ) -> Result<(), DeployExecError> {
            self.calls.borrow_mut().push(RecordedCall::Upload {
                remote_path: remote_path.to_owned(),
                mode,
            });
            Ok(())
        }
    }

    fn resolved() -> ResolvedDeployConfig {
        ResolvedDeployConfig::resolve(
            &autumn_web::config::DeployConfig {
                host: Some("203.0.113.10".to_owned()),
                ..autumn_web::config::DeployConfig::default()
            },
            "myapp",
        )
    }

    fn sample_ops(env: Secret) -> Vec<DeployOp> {
        let cfg = resolved();
        let unit = super::super::render_systemd_unit(&cfg);
        first_deploy_ops(
            &cfg,
            &unit,
            env,
            Path::new("/local/target/release/myapp"),
            "20260714T120000Z",
            3000,
        )
    }

    #[test]
    fn first_deploy_produces_exact_ordered_sequence() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");

        let calls = exec.calls();
        // mkdir → upload binary → write env → write unit → link current →
        // daemon-reload → enable --now → readiness poll.
        assert_eq!(calls.len(), 8, "unexpected call sequence: {calls:#?}");

        assert!(
            matches!(&calls[0], RecordedCall::Run { label: "prepare-dirs", shell }
                if shell.contains("mkdir -p") && shell.contains("/srv/autumn/myapp/releases/20260714T120000Z")
                    && shell.contains("/srv/autumn/myapp/shared")),
            "call 0: {:?}",
            calls[0]
        );
        assert_eq!(
            calls[1],
            RecordedCall::Upload {
                remote_path: "/srv/autumn/myapp/releases/20260714T120000Z/myapp".to_owned(),
                mode: Some(0o755),
            },
            "call 1 uploads the release binary (0755)"
        );
        assert_eq!(
            calls[2],
            RecordedCall::Upload {
                remote_path: "/srv/autumn/myapp/shared/autumn.env".to_owned(),
                mode: Some(0o600),
            },
            "call 2 writes the env file with mode 0600 (AC-5)"
        );
        assert_eq!(
            calls[3],
            RecordedCall::Upload {
                remote_path: "/etc/systemd/system/myapp.service".to_owned(),
                mode: Some(0o644),
            },
            "call 3 writes the systemd unit"
        );
        assert!(
            matches!(&calls[4], RecordedCall::Run { label: "link-current", shell }
                if shell.contains("ln -sfn") && shell.contains("/srv/autumn/myapp/current")),
            "call 4: {:?}",
            calls[4]
        );
        assert!(
            matches!(&calls[5], RecordedCall::Run { label: "daemon-reload", shell }
                if shell.contains("systemctl daemon-reload")),
            "call 5: {:?}",
            calls[5]
        );
        assert!(
            matches!(&calls[6], RecordedCall::Run { label: "enable-now", shell }
                if shell.contains("systemctl enable --now myapp.service")),
            "call 6: {:?}",
            calls[6]
        );
        assert!(
            matches!(&calls[7], RecordedCall::Run { label: "readiness-gate", shell }
                if shell.contains("/ready") && shell.contains("curl")),
            "call 7: {:?}",
            calls[7]
        );
    }

    #[test]
    fn env_file_op_is_0600_and_never_echoes_the_secret() {
        let secret_body = "AUTUMN_SECURITY__SIGNING_SECRET=super-secret-signing-value\n\
                           AUTUMN_DATABASE__URL=postgres://u:p@db/app\n";
        let ops = sample_ops(Secret::new(secret_body));

        // Locate the env-file op and confirm its mode is 0600.
        let env_op = ops
            .iter()
            .find(|op| op.label() == "write-env")
            .expect("write-env op present");
        match env_op {
            DeployOp::WriteFile { mode, contents, .. } => {
                assert_eq!(*mode, Some(0o600), "env file must be mode 0600 (AC-5)");
                assert!(
                    matches!(contents, FileContents::Secret(_)),
                    "env file contents must be a Secret"
                );
            }
            other => panic!("write-env should be a WriteFile op, got {other:?}"),
        }

        // The secret value must not appear in any Debug/Display of the ops.
        let debug = format!("{ops:#?}");
        assert!(
            !debug.contains("super-secret-signing-value"),
            "secret leaked into ops Debug output"
        );
        assert!(
            !debug.contains("postgres://u:p@db/app"),
            "db url leaked into ops Debug output"
        );

        // And it must not appear in the recorded executor calls either.
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("run ok");
        let calls_debug = format!("{:#?}", exec.calls());
        assert!(
            !calls_debug.contains("super-secret-signing-value"),
            "secret leaked into recorded executor calls"
        );
    }

    #[test]
    fn secret_debug_and_display_are_redacted() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
        // The value is still recoverable for the file write.
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn ssh_and_scp_argv_are_built_correctly() {
        let target = SshTarget {
            host: "203.0.113.10".to_owned(),
            user: "deploy".to_owned(),
            port: 2222,
        };

        let ssh = ssh_argv(&target, "systemctl daemon-reload");
        assert_eq!(
            ssh,
            vec![
                "-p".to_owned(),
                "2222".to_owned(),
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
                "-o".to_owned(),
                "StrictHostKeyChecking=accept-new".to_owned(),
                "deploy@203.0.113.10".to_owned(),
                "systemctl daemon-reload".to_owned(),
            ]
        );

        let scp = scp_argv(
            &target,
            Path::new("/local/target/release/myapp"),
            "/srv/autumn/myapp/releases/r1/myapp",
        );
        assert_eq!(
            scp,
            vec![
                // scp uses -P (uppercase) for the port, not ssh's -p.
                "-P".to_owned(),
                "2222".to_owned(),
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
                "-o".to_owned(),
                "StrictHostKeyChecking=accept-new".to_owned(),
                "/local/target/release/myapp".to_owned(),
                "deploy@203.0.113.10:/srv/autumn/myapp/releases/r1/myapp".to_owned(),
            ]
        );
    }

    #[test]
    fn preflight_failure_aborts_before_any_executor_call() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        // One failing check (mirrors a hostless config where ssh_reachability
        // fails): execution must abort with zero executor calls recorded.
        let checks = vec![
            PreflightCheck::pass("signing_secret", "ok"),
            PreflightCheck::fail("ssh_reachability", "no target host configured", "set host"),
        ];

        let err = execute_first_deploy(&checks, &ops, &exec)
            .expect_err("failing preflight must abort the deploy");
        assert!(
            matches!(err, DeployExecError::PreflightAborted { failed: 1 }),
            "expected PreflightAborted, got {err:?}"
        );
        assert!(
            exec.calls().is_empty(),
            "no remote call may run when preflight fails: {:?}",
            exec.calls()
        );
    }

    #[test]
    fn passing_preflight_runs_the_full_sequence() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        let checks = vec![PreflightCheck::pass("ssh_reachability", "reachable")];
        execute_first_deploy(&checks, &ops, &exec).expect("all checks pass → deploy runs");
        assert_eq!(exec.calls().len(), 8, "the full op sequence should run");
    }

    #[test]
    fn readiness_timeout_surfaces_a_command_failure() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        // The fake fails the readiness-gate command (as a real host would on
        // timeout). run_ops must surface a non-zero DeployExecError and stop.
        let exec = RecordingExecutor::failing_on("readiness-gate");
        let err = run_ops(&ops, &exec).expect_err("readiness timeout must fail the deploy");
        assert!(
            matches!(
                err,
                DeployExecError::CommandFailed {
                    label: "readiness-gate",
                    ..
                }
            ),
            "expected a readiness-gate CommandFailed, got {err:?}"
        );
        // Everything up to and including the readiness gate ran (8 calls); no
        // auto-rollback is attempted in this slice.
        assert_eq!(exec.calls().len(), 8);
    }
}
