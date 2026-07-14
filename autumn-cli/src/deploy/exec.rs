//! Injectable remote-execution layer for `autumn deploy` (issue #1607, Slices 1–2).
//!
//! This module turns the dry-run deploy plan into REAL execution behind an
//! injectable [`DeployExecutor`], so the command-construction and execution paths
//! are unit-testable without a live host.
//!
//! ## What is real here
//!
//! - **First deploy (Slice 1, proxy-fronted in Slice 2):** [`first_deploy_ops`]
//!   installs the reverse proxy on the public port, stands the initial release up
//!   on a PRIVATE loopback slot port, and routes the proxy at it — so a later
//!   redeploy has a live upstream to flip away from.
//! - **Zero-downtime redeploy (Slice 2, AC-2/AC-3):** [`cutover_ops`] is the pure,
//!   ordered cutover sequence: upload → write the candidate's env (0600) + unit on
//!   a SEPARATE loopback port → start the candidate (old release keeps serving) →
//!   run pending migrations BEFORE cutover → bounded `/ready` gate on the
//!   candidate → health-gated proxy flip old→candidate → promote `current` → drain
//!   the old release → prune. Because [`run_ops`] stops at the first failure, a
//!   failed migration or a readiness timeout aborts BEFORE the flip with the old
//!   release still serving (AC-3 / AC-2 safety).
//! - **Blue/green slots:** the candidate always takes the slot the live release is
//!   NOT using (see [`SlotPlan`]), so the candidate never binds the live port and
//!   the two releases run side by side across the cutover.
//! - **Execution:** [`run_ops`] / [`execute_first_deploy`] / [`execute_redeploy`]
//!   iterate the ops and drive a [`DeployExecutor`]; the `execute_*` entrypoints
//!   refuse to run if any preflight check failed (AC-6 fail-fast). [`DeployMode`]
//!   / [`detect_deploy_mode`] pick first-vs-redeploy from a remote probe.
//! - **Proxy:** the kamal-proxy CLI lives entirely in
//!   [`KamalProxyController`](super::proxy::KamalProxyController); the cutover
//!   orchestration here talks only to the [`ProxyController`] trait, so a Caddy
//!   controller could replace it without touching this file.
//! - **Real ssh/scp:** [`SshExecutor`] shells out to the system `ssh`/`scp`
//!   binaries via [`std::process::Command`] (no ssh crate is pulled in). The argv
//!   builders [`ssh_argv`]/[`scp_argv`] are pure functions.
//! - **Secret redaction:** the env-file contents travel as a [`Secret`] whose
//!   `Debug`/`Display` are redacted, and secrets are only ever written to a
//!   `0600` file — never placed on a command line or into an error message. The
//!   migrate one-shot sources secrets via a systemd `EnvironmentFile`, not argv.
//!
//! ## What is deferred (NOT implemented in this slice)
//!
//! - **Rollback execution** and auto-rollback on a failed readiness gate — a
//!   readiness timeout or failed migration here fails loudly with the OLD release
//!   still serving; automatically re-pointing traffic back is Slice 3.
//! - The **CI end-to-end container harness** that exercises the real `ssh` path is
//!   Slice 4 — live ssh is not exercised by these unit tests.
//! - A **Caddy** [`ProxyController`](super::proxy::ProxyController) — kamal-proxy
//!   is the confirmed proxy; Caddy is only the documented swappable alternative.
//!
//! The migrate step is fully real: the one-shot runs the uploaded release with
//! `AUTUMN_MIGRATE=1`, which the app runtime honors (alongside its other
//! env-gated one-shot modes such as `AUTUMN_BUILD_STATIC=1`) by applying pending
//! embedded migrations with the same locked applier a normal boot uses and
//! exiting 0/non-zero WITHOUT starting the server — so a failed migration aborts
//! before cutover (AC-3).

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::proxy::ProxyController;
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
    pub(crate) fn new(label: &'static str, shell: impl Into<String>) -> Self {
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
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The blue deploy slot.
pub const SLOT_BLUE: &str = "blue";
/// The green deploy slot.
pub const SLOT_GREEN: &str = "green";

/// The PRIVATE loopback port a slot's app release binds. The public port is owned
/// by the reverse proxy; blue takes `public + 1`, green `public + 2`, so the
/// candidate never collides with the proxy's public port or the live slot's port.
#[must_use]
pub fn slot_app_port(public_port: u16, slot: &str) -> u16 {
    let offset = if slot == SLOT_GREEN { 2 } else { 1 };
    public_port.saturating_add(offset)
}

/// The other slot (blue ↔ green).
#[must_use]
pub fn other_slot(slot: &str) -> &'static str {
    if slot == SLOT_BLUE {
        SLOT_GREEN
    } else {
        SLOT_BLUE
    }
}

/// Normalize an arbitrary slot string to one of the two known slots (defaulting
/// to blue) so a corrupt marker can never name a third slot.
fn canonical_slot(slot: &str) -> &'static str {
    if slot.trim() == SLOT_GREEN {
        SLOT_GREEN
    } else {
        SLOT_BLUE
    }
}

/// systemd unit name for a slot's app release (without the `.service` suffix).
#[must_use]
pub fn slot_unit_name(service: &str, slot: &str) -> String {
    format!("{service}-{slot}")
}

/// Remote marker file recording which slot currently serves live traffic, so the
/// next redeploy can pick the OTHER slot for the candidate.
fn live_slot_marker(cfg: &ResolvedDeployConfig) -> String {
    format!("{}/shared/live-slot", cfg.app_dir)
}

/// The resolved blue/green slot layout for a deploy.
///
/// The candidate always takes the slot the live release is NOT using, so both
/// releases run side by side across the cutover and the candidate never binds the
/// live port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotPlan {
    /// Public port fronted by the proxy (the app's configured `server.port`).
    pub public_port: u16,
    /// Slot the candidate release takes.
    pub candidate_slot: &'static str,
    /// Loopback port the candidate binds.
    pub candidate_port: u16,
    /// Slot the currently-live release uses (equals `candidate_slot` on a first
    /// deploy, where there is nothing to drain yet).
    pub live_slot: &'static str,
    /// Loopback port the currently-live release binds.
    pub live_port: u16,
}

impl SlotPlan {
    /// First deploy: the initial release takes the blue slot.
    #[must_use]
    pub fn first(public_port: u16) -> Self {
        Self {
            public_port,
            candidate_slot: SLOT_BLUE,
            candidate_port: slot_app_port(public_port, SLOT_BLUE),
            live_slot: SLOT_BLUE,
            live_port: slot_app_port(public_port, SLOT_BLUE),
        }
    }

    /// Redeploy: the candidate takes the slot the live release is NOT using.
    #[must_use]
    pub fn redeploy(public_port: u16, live_slot: &str) -> Self {
        let live_slot = canonical_slot(live_slot);
        let candidate_slot = other_slot(live_slot);
        Self {
            public_port,
            candidate_slot,
            candidate_port: slot_app_port(public_port, candidate_slot),
            live_slot,
            live_port: slot_app_port(public_port, live_slot),
        }
    }
}

/// Build the ordered FIRST-deploy operation sequence (Slice 1 + proxy front).
///
/// Pure — performs no I/O; `release_id` and the [`SlotPlan`] are injected for
/// determinism. The initial release takes the blue slot on a private loopback
/// port and the proxy is installed on the public port and routed at it, so a
/// later redeploy has a live upstream to flip away from. The sequence:
///
/// 1. install + supervise the proxy on the public port,
/// 2. prepare remote dirs,
/// 3. upload the release binary (`0755`),
/// 4. write the secret env file (`0600`, AC-5),
/// 5. write the blue slot's systemd unit (binds `127.0.0.1:{blue_port}`),
/// 6. point `current` at the new release,
/// 7. `systemctl daemon-reload`,
/// 8. `systemctl enable --now {service}-blue.service`,
/// 9. record the live slot marker,
/// 10. bounded `/ready` poll on the blue loopback port,
/// 11. route the proxy at `127.0.0.1:{blue_port}`.
#[must_use]
pub fn first_deploy_ops(
    cfg: &ResolvedDeployConfig,
    proxy: &impl ProxyController,
    unit: &str,
    env_file: Secret,
    binary_local: &Path,
    release_id: &str,
    plan: &SlotPlan,
) -> Vec<DeployOp> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    let remote_binary = format!("{release_dir}/{}", cfg.app_name);
    let shared_dir = format!("{}/shared", cfg.app_dir);
    let env_path = cfg.env_file();
    let current = cfg.current_symlink();
    let unit_name = slot_unit_name(&cfg.service_name, plan.candidate_slot);
    let unit_path = format!("/etc/systemd/system/{unit_name}.service");

    let mut ops = proxy.ensure_installed_ops(plan.public_port);
    ops.extend([
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
            format!("systemctl enable --now {unit_name}.service"),
        )),
        DeployOp::Run(record_live_slot(cfg, plan.candidate_slot)),
        DeployOp::Run(RemoteCommand::new(
            "readiness-gate",
            readiness_poll_shell(plan.candidate_port, cfg.readiness_timeout_secs),
        )),
    ]);
    // Route the proxy at the freshly-ready initial release (first deploy has no
    // prior upstream to flip away from).
    ops.push(proxy.route_op(&cfg.service_name, &loopback_upstream(plan.candidate_port)));
    ops
}

/// Build the ordered zero-downtime REDEPLOY cutover sequence (Slice 2, AC-2/AC-3).
///
/// Pure — performs no I/O; `release_id` and the redeploy [`SlotPlan`] are injected
/// for determinism. The candidate is stood up on the idle slot's separate
/// loopback port while the live release keeps serving; migrations run BEFORE the
/// flip; the proxy flip only swaps traffic after the candidate passes its
/// readiness gate. Because [`run_ops`] stops at the first failure, a failed
/// migration or readiness timeout aborts here with the old release still serving
/// (no flip, no drain, no promote). The sequence:
///
/// 1. prepare remote dirs,
/// 2. upload the release binary (`0755`),
/// 3. write the secret env file (`0600`, AC-5),
/// 4. write the candidate slot's systemd unit (binds `127.0.0.1:{candidate_port}`),
/// 5. `systemctl daemon-reload`,
/// 6. start the candidate (old release untouched),
/// 7. run pending migrations BEFORE cutover (`AUTUMN_MIGRATE=1` one-shot),
/// 8. bounded `/ready` poll on the candidate's separate loopback port,
/// 9. health-gated proxy flip old→candidate (THE cutover),
/// 10. promote `current` to the new release,
/// 11. record the live slot marker,
/// 12. drain (stop) the old release,
/// 13. prune old releases beyond `keep_releases`.
#[must_use]
pub fn cutover_ops(
    cfg: &ResolvedDeployConfig,
    proxy: &impl ProxyController,
    unit: &str,
    env_file: Secret,
    binary_local: &Path,
    release_id: &str,
    plan: &SlotPlan,
) -> Vec<DeployOp> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    let remote_binary = format!("{release_dir}/{}", cfg.app_name);
    let shared_dir = format!("{}/shared", cfg.app_dir);
    let env_path = cfg.env_file();
    let current = cfg.current_symlink();
    let candidate_unit = slot_unit_name(&cfg.service_name, plan.candidate_slot);
    let candidate_unit_path = format!("/etc/systemd/system/{candidate_unit}.service");
    let live_unit = slot_unit_name(&cfg.service_name, plan.live_slot);

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
            mode: Some(0o600),
        },
        DeployOp::WriteFile {
            label: "write-candidate-unit",
            contents: FileContents::Plain(unit.to_owned()),
            remote_path: candidate_unit_path,
            mode: Some(0o644),
        },
        DeployOp::Run(RemoteCommand::new(
            "daemon-reload",
            "systemctl daemon-reload",
        )),
        DeployOp::Run(RemoteCommand::new(
            "start-candidate",
            format!("systemctl enable --now {candidate_unit}.service"),
        )),
        // Migrations run BEFORE the flip. `systemd-run --wait` returns the
        // child's exit status, so a failed migration surfaces a non-zero error
        // that stops run_ops before the flip — old release still serving (AC-3).
        DeployOp::Run(release_migrate_command(cfg, &release_dir)),
        DeployOp::Run(RemoteCommand::new(
            "readiness-gate",
            readiness_poll_shell(plan.candidate_port, cfg.readiness_timeout_secs),
        )),
        // THE cutover: the proxy health-checks the candidate then atomically swaps
        // live traffic to it and drains the old target. Only reached after a
        // passing readiness gate (AC-2).
        proxy.flip_op(&cfg.service_name, &loopback_upstream(plan.candidate_port)),
        DeployOp::Run(RemoteCommand::new(
            "link-current",
            format!(
                "ln -sfn {} {}",
                shell_quote(&release_dir),
                shell_quote(&current)
            ),
        )),
        DeployOp::Run(record_live_slot(cfg, plan.candidate_slot)),
        DeployOp::Run(RemoteCommand::new(
            "drain-old",
            format!("systemctl disable --now {live_unit}.service"),
        )),
        DeployOp::Run(RemoteCommand::new(
            "prune",
            prune_releases_shell(&cfg.releases_dir(), cfg.keep_releases),
        )),
    ]
}

/// The `host:port` loopback upstream string the proxy routes at.
fn loopback_upstream(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Command that records which slot now serves live traffic (read by the next
/// redeploy's [`detect_deploy_mode`]).
fn record_live_slot(cfg: &ResolvedDeployConfig, slot: &str) -> RemoteCommand {
    RemoteCommand::new(
        "record-live-slot",
        format!(
            "printf '%s' {} > {}",
            shell_quote(slot),
            shell_quote(&live_slot_marker(cfg))
        ),
    )
}

/// Command that runs the uploaded release's pending migrations as a one-shot,
/// BEFORE cutover.
///
/// Uses `systemd-run --wait` so (a) the migration's exit status propagates (a
/// failure aborts the deploy before the flip — AC-3), and (b) secrets reach the
/// process via the systemd `EnvironmentFile`, never on the command line. The
/// `AUTUMN_MIGRATE=1` trigger is honored by the app runtime
/// (`AppBuilder::run` → `run_migrate_only_mode`), which applies pending embedded
/// migrations with the same locked applier a normal boot uses and exits without
/// starting the server.
fn release_migrate_command(cfg: &ResolvedDeployConfig, release_dir: &str) -> RemoteCommand {
    let bin = format!("{release_dir}/{}", cfg.app_name);
    RemoteCommand::new(
        "migrate",
        format!(
            "systemd-run --wait --collect --quiet --unit={service}-migrate \
             --property=EnvironmentFile={env} --setenv=AUTUMN_MIGRATE=1 {bin}",
            service = cfg.service_name,
            env = shell_quote(&cfg.env_file()),
            bin = shell_quote(&bin),
        ),
    )
}

/// Prune shell: keep the newest `keep` release dirs, delete the rest. `ls -1dt`
/// lists dirs newest-first; `tail -n +{keep+1}` skips the newest `keep`.
#[must_use]
fn prune_releases_shell(releases_dir: &str, keep: u32) -> String {
    format!(
        "cd {} && ls -1dt */ 2>/dev/null | tail -n +{} | xargs -r rm -rf",
        shell_quote(releases_dir),
        keep + 1,
    )
}

/// Whether the target is a first deploy or a redeploy (and, if so, which slot is
/// currently live).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployMode {
    /// No promoted `current` release yet — take the first-deploy path.
    First,
    /// A `current` release already exists — take the zero-downtime cutover path.
    Redeploy {
        /// Slot the currently-live release serves on.
        live_slot: &'static str,
    },
}

/// Probe the target over the executor to decide first-vs-redeploy: a redeploy is
/// signalled by an existing `current` symlink, and the live slot is read from the
/// marker (defaulting to blue if the marker is missing).
///
/// # Errors
///
/// Returns the executor's error if the probe command cannot run.
pub fn detect_deploy_mode(
    cfg: &ResolvedDeployConfig,
    exec: &impl DeployExecutor,
) -> Result<DeployMode, DeployExecError> {
    let shell = format!(
        "if [ -L {current} ]; then printf 'redeploy:'; cat {marker} 2>/dev/null || printf '{blue}'; \
         else printf 'first'; fi",
        current = shell_quote(&cfg.current_symlink()),
        marker = shell_quote(&live_slot_marker(cfg)),
        blue = SLOT_BLUE,
    );
    let out = exec.run(&RemoteCommand::new("detect-current", shell))?;
    Ok(out
        .stdout
        .trim()
        .strip_prefix("redeploy:")
        .map_or(DeployMode::First, |slot| DeployMode::Redeploy {
            live_slot: canonical_slot(slot),
        }))
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

/// Execute a zero-downtime redeploy cutover sequence, gated on preflight exactly
/// like [`execute_first_deploy`].
///
/// The safety of the cutover comes from [`run_ops`] stopping at the first
/// failure: a failed migration or a readiness timeout returns before the proxy
/// flip, leaving the old release serving (AC-2/AC-3). Auto-rollback is Slice 3.
///
/// # Errors
///
/// Returns [`DeployExecError::PreflightAborted`] when any preflight check failed,
/// or the first executor error otherwise.
pub fn execute_redeploy(
    checks: &[PreflightCheck],
    ops: &[DeployOp],
    exec: &impl DeployExecutor,
) -> Result<(), DeployExecError> {
    execute_first_deploy(checks, ops, exec)
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
        /// Scripted stdout returned for a given command label.
        stdout_by_label: Vec<(&'static str, String)>,
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

        /// Script the stdout returned for a given command label (used to drive
        /// [`detect_deploy_mode`]'s first-vs-redeploy probe).
        fn with_stdout(mut self, label: &'static str, stdout: impl Into<String>) -> Self {
            self.stdout_by_label.push((label, stdout.into()));
            self
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.borrow().clone()
        }

        /// Labels of the recorded `Run` calls, in order (upload calls excluded).
        fn run_labels(&self) -> Vec<&'static str> {
            self.calls
                .borrow()
                .iter()
                .filter_map(|c| match c {
                    RecordedCall::Run { label, .. } => Some(*label),
                    RecordedCall::Upload { .. } => None,
                })
                .collect()
        }

        /// The shell recorded for the first `Run` with `label`, if any.
        fn shell_for(&self, label: &str) -> Option<String> {
            self.calls.borrow().iter().find_map(|c| match c {
                RecordedCall::Run { label: l, shell } if *l == label => Some(shell.clone()),
                _ => None,
            })
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
            let stdout = self
                .stdout_by_label
                .iter()
                .find(|(l, _)| *l == cmd.label)
                .map(|(_, out)| out.clone())
                .unwrap_or_default();
            Ok(CommandOutput {
                stdout,
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

    const RELEASE_ID: &str = "20260714T120000Z";
    const RELEASE_DIR: &str = "/srv/autumn/myapp/releases/20260714T120000Z";

    fn proxy() -> super::super::proxy::KamalProxyController {
        super::super::proxy::KamalProxyController::new(60)
    }

    /// First-deploy ops: initial release on the blue slot (loopback 3001) behind
    /// the proxy on the public port 3000.
    fn sample_ops(env: Secret) -> Vec<DeployOp> {
        let cfg = resolved();
        let plan = SlotPlan::first(3000);
        let unit = super::super::render_app_unit(
            &cfg,
            RELEASE_DIR,
            plan.candidate_port,
            plan.candidate_slot,
        );
        first_deploy_ops(
            &cfg,
            &proxy(),
            &unit,
            env,
            Path::new("/local/target/release/myapp"),
            RELEASE_ID,
            &plan,
        )
    }

    /// Redeploy cutover ops: the live release is on blue, so the candidate takes
    /// green (loopback 3002).
    fn sample_cutover_ops(env: Secret) -> Vec<DeployOp> {
        let cfg = resolved();
        let plan = SlotPlan::redeploy(3000, SLOT_BLUE);
        let unit = super::super::render_app_unit(
            &cfg,
            RELEASE_DIR,
            plan.candidate_port,
            plan.candidate_slot,
        );
        cutover_ops(
            &cfg,
            &proxy(),
            &unit,
            env,
            Path::new("/local/target/release/myapp"),
            RELEASE_ID,
            &plan,
        )
    }

    #[test]
    fn first_deploy_installs_and_routes_proxy() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        let labels = exec.run_labels();

        // The proxy is installed (host-prep) and, once the app is ready, the
        // proxy is routed at the initial release — so a later redeploy has a live
        // upstream to flip away from.
        let install = labels
            .iter()
            .position(|&l| l == "proxy-install")
            .expect("proxy is installed on first deploy");
        let route = labels
            .iter()
            .position(|&l| l == "proxy-route")
            .expect("proxy is routed at the initial release");
        let readiness = labels
            .iter()
            .position(|&l| l == "readiness-gate")
            .expect("readiness gate");
        assert!(install < route, "proxy is installed before it is routed");
        assert!(
            readiness < route,
            "proxy is routed only after the app reports /ready"
        );

        // The proxy unit is written to the public port; the app unit is a
        // slot-scoped unit on a SEPARATE loopback port (never the public port).
        let proxy_unit = exec.shell_for("proxy-install").expect("proxy-install ran");
        assert!(proxy_unit.contains("enable --now kamal-proxy.service"));
        assert!(
            exec.calls().iter().any(|c| matches!(
                c,
                RecordedCall::Upload { remote_path, .. }
                    if remote_path == "/etc/systemd/system/kamal-proxy.service"
            )),
            "the proxy systemd unit is written"
        );
        let enable = exec.shell_for("enable-now").expect("enable-now ran");
        assert!(
            enable.contains("myapp-blue.service"),
            "the app runs as a slot-scoped unit: {enable}"
        );
        // Blue binds public+1 = 3001 (loopback), not the public 3000.
        let gate = exec.shell_for("readiness-gate").expect("gate ran");
        assert!(gate.contains("127.0.0.1:3001/ready"), "gate: {gate}");
        let route_cmd = exec.shell_for("proxy-route").expect("route ran");
        assert!(
            route_cmd.contains("--target 127.0.0.1:3001"),
            "proxy routes at the blue loopback port: {route_cmd}"
        );
    }

    #[test]
    fn redeploy_produces_exact_zero_downtime_sequence() {
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");

        // The full ordered run sequence (uploads interleave; asserted separately).
        assert_eq!(
            exec.run_labels(),
            vec![
                "prepare-dirs",
                "daemon-reload",
                "start-candidate",
                "migrate",
                "readiness-gate",
                "proxy-flip",
                "link-current",
                "record-live-slot",
                "drain-old",
                "prune",
            ],
            "unexpected cutover sequence"
        );

        // The candidate's env (0600) and unit are written on a SEPARATE loopback
        // port before it starts.
        assert!(
            exec.calls().iter().any(|c| matches!(
                c,
                RecordedCall::Upload { remote_path, mode }
                    if remote_path == "/srv/autumn/myapp/shared/autumn.env" && *mode == Some(0o600)
            )),
            "candidate env written 0600"
        );
        assert!(
            exec.calls().iter().any(|c| matches!(
                c,
                RecordedCall::Upload { remote_path, .. }
                    if remote_path == "/etc/systemd/system/myapp-green.service"
            )),
            "candidate unit is the idle (green) slot"
        );
        // The candidate binds green = public+2 = 3002 and the flip targets it.
        let gate = exec.shell_for("readiness-gate").expect("gate ran");
        assert!(gate.contains("127.0.0.1:3002/ready"), "gate: {gate}");
        let flip = exec.shell_for("proxy-flip").expect("flip ran");
        assert!(
            flip.contains("kamal-proxy deploy") && flip.contains("--target 127.0.0.1:3002"),
            "flip targets the candidate: {flip}"
        );
        // The old (blue) release is drained; current is promoted to the new dir.
        let drain = exec.shell_for("drain-old").expect("drain ran");
        assert!(
            drain.contains("disable --now myapp-blue.service"),
            "{drain}"
        );
        let promote = exec.shell_for("link-current").expect("promote ran");
        assert!(promote.contains(RELEASE_DIR) && promote.contains("/srv/autumn/myapp/current"));

        // Ordering invariants: migrate BEFORE the flip; the flip ONLY after a
        // passing readiness gate.
        let labels = exec.run_labels();
        let pos = |l: &str| labels.iter().position(|&x| x == l).unwrap();
        assert!(pos("migrate") < pos("proxy-flip"), "migrate before flip");
        assert!(
            pos("readiness-gate") < pos("proxy-flip"),
            "flip only after a passing readiness gate"
        );
        assert!(
            pos("proxy-flip") < pos("link-current"),
            "flip before promote"
        );
        assert!(
            pos("link-current") < pos("drain-old"),
            "promote before drain"
        );
        assert!(pos("drain-old") < pos("prune"), "drain before prune");
    }

    #[test]
    fn failed_migration_aborts_before_cutover_leaving_old_serving() {
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        // The migrate one-shot fails (as a bad migration would on the host).
        let exec = RecordingExecutor::failing_on("migrate");
        let err = run_ops(&ops, &exec).expect_err("a failed migration must abort the deploy");
        assert!(
            matches!(
                err,
                DeployExecError::CommandFailed {
                    label: "migrate",
                    ..
                }
            ),
            "expected a migrate CommandFailed, got {err:?}"
        );
        // AC-3: no cutover happened — no flip, no drain, no promote — so the old
        // release is untouched and still serving.
        let labels = exec.run_labels();
        assert!(
            !labels.contains(&"proxy-flip"),
            "no flip after a failed migration"
        );
        assert!(!labels.contains(&"drain-old"), "old release not drained");
        assert!(!labels.contains(&"link-current"), "current not repointed");
        assert!(!labels.contains(&"prune"), "nothing pruned");
    }

    #[test]
    fn readiness_timeout_before_cutover_does_not_flip() {
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        // The candidate never reports /ready within the window.
        let exec = RecordingExecutor::failing_on("readiness-gate");
        let err = run_ops(&ops, &exec).expect_err("a readiness timeout must abort the deploy");
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
        // AC-2 safety: traffic never flips to an unhealthy candidate. Auto-rollback
        // itself is Slice 3; here the old release simply keeps serving.
        let labels = exec.run_labels();
        assert!(
            !labels.contains(&"proxy-flip"),
            "no flip on readiness timeout"
        );
        assert!(!labels.contains(&"drain-old"), "old release not drained");
        assert!(!labels.contains(&"link-current"), "current not repointed");
    }

    #[test]
    fn prune_keeps_exactly_keep_releases() {
        let cfg = resolved();
        assert_eq!(cfg.keep_releases, 3, "default keep_releases");
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("run ok");
        let prune = exec.shell_for("prune").expect("prune ran");
        // `tail -n +4` keeps the newest 3 release dirs and deletes the rest.
        assert!(
            prune.contains("ls -1dt */") && prune.contains("tail -n +4"),
            "prune must keep exactly keep_releases (3): {prune}"
        );
        assert!(
            prune.contains("/srv/autumn/myapp/releases"),
            "prune operates on the releases dir: {prune}"
        );
    }

    #[test]
    fn detect_deploy_mode_picks_first_vs_redeploy() {
        let cfg = resolved();
        // No `current` symlink → first deploy.
        let first = RecordingExecutor::new().with_stdout("detect-current", "first");
        assert_eq!(detect_deploy_mode(&cfg, &first).unwrap(), DeployMode::First);
        // `current` present + marker says green → redeploy onto blue candidate.
        let redeploy = RecordingExecutor::new().with_stdout("detect-current", "redeploy:green");
        assert_eq!(
            detect_deploy_mode(&cfg, &redeploy).unwrap(),
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN
            }
        );
        // A missing/blank marker on a redeploy defaults the live slot to blue.
        let default_blue = RecordingExecutor::new().with_stdout("detect-current", "redeploy:");
        assert_eq!(
            detect_deploy_mode(&cfg, &default_blue).unwrap(),
            DeployMode::Redeploy {
                live_slot: SLOT_BLUE
            }
        );
    }

    #[test]
    fn slot_plan_puts_candidate_on_the_idle_slot() {
        let plan = SlotPlan::redeploy(3000, SLOT_BLUE);
        assert_eq!(plan.candidate_slot, SLOT_GREEN);
        assert_eq!(plan.candidate_port, 3002);
        assert_eq!(plan.live_slot, SLOT_BLUE);
        assert_eq!(plan.live_port, 3001);
        // The candidate never binds the public port.
        assert_ne!(plan.candidate_port, plan.public_port);

        let flipped = SlotPlan::redeploy(3000, SLOT_GREEN);
        assert_eq!(flipped.candidate_slot, SLOT_BLUE);
        assert_eq!(flipped.candidate_port, 3001);

        let first = SlotPlan::first(3000);
        assert_eq!(first.candidate_slot, SLOT_BLUE);
        assert_eq!(first.candidate_port, 3001);
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
        // 2 proxy ops + 9 first-deploy ops + 1 proxy route = 12.
        assert_eq!(exec.calls().len(), 12, "the full op sequence should run");
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
        // The deploy stopped at the readiness gate: the proxy is never routed at
        // an unhealthy first release (no auto-rollback in this slice).
        assert!(
            !exec.run_labels().contains(&"proxy-route"),
            "proxy must not be routed after a failed readiness gate"
        );
    }
}
