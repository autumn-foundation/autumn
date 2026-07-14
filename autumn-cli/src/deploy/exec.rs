//! Injectable remote-execution layer for `autumn deploy` (issue #1607, Slices 1–3).
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
//! - **Rollback (Slice 3, AC-4):** two forms are real. On a failed redeploy gate
//!   (migrate or readiness — anything at or before the health-gated flip) traffic
//!   never moved, so [`execute_redeploy`] AUTO-rolls-back: it drives
//!   [`candidate_teardown_ops`] (stop + disable the candidate slot unit, remove
//!   its release dir), leaves the proxy on the still-serving old release, and
//!   fails with [`DeployExecError::CandidateRolledBack`]. On demand,
//!   [`resolve_rollback_target`] finds the previous release and [`rollback_ops`] /
//!   [`execute_rollback`] bring its slot back up, flip the proxy to it, repoint
//!   `current`, and re-probe `/ready`; with no previous release the resolve fails
//!   with [`DeployExecError::NoPreviousRelease`] and nothing destructive runs. A
//!   FIRST deploy has no previous release, so a pre-go-live failure tears the
//!   candidate down and fails with [`DeployExecError::FirstDeployTornDown`].
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
//! - The **CI end-to-end container harness** that exercises the real `ssh` path
//!   against a live container — actually driving a rollback over ssh end-to-end —
//!   is Slice 4; live ssh is not exercised by these unit tests.
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
    /// An on-demand rollback found no prior release to roll back to (Slice 3).
    #[error(
        "no previous release to roll back to — the current release is the only one on the target"
    )]
    NoPreviousRelease,
    /// A redeploy gate failed before (or at) the cutover flip, so the candidate
    /// was torn down and the previously-serving release was left untouched
    /// (Slice 3 auto-rollback, AC-4). Never embeds a secret — only the failing
    /// step's label and the redacted source error.
    #[error(
        "redeploy failed at `{failed_step}`; the candidate was auto-rolled-back and torn down — \
         the previous release is still serving"
    )]
    CandidateRolledBack {
        /// Label of the step that failed before the flip.
        failed_step: &'static str,
        /// The underlying failure (its `Display` is already redacted).
        #[source]
        source: Box<Self>,
    },
    /// A FIRST deploy failed before the app went live. There is no previous
    /// release to roll back to, so the just-started candidate was torn down and
    /// the deploy fails loudly (Slice 3 — the honest first-deploy path).
    #[error(
        "first deploy failed at `{failed_step}`; there is no previous release to roll back to — \
         the candidate was torn down, nothing is serving"
    )]
    FirstDeployTornDown {
        /// Label of the step that failed before the app went live.
        failed_step: &'static str,
        /// The underlying failure (its `Display` is already redacted).
        #[source]
        source: Box<Self>,
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

/// Remote marker file recording the PREVIOUS live release — its absolute dir and
/// the slot it runs on, stored as `{dir}\t{slot}` — so an on-demand rollback
/// returns to the release `current` pointed at before the last promote, instead
/// of inferring it from release-dir mtimes (which is wrong after an
/// A→B→rollback→A→deploy-C history). Written whenever `current` is repointed to a
/// DIFFERENT release (redeploy cutover and rollback); cleared on a first deploy.
/// Mirrors [`live_slot_marker`]'s path convention (both live under `shared/`).
fn previous_release_marker(cfg: &ResolvedDeployConfig) -> String {
    format!("{}/shared/previous-release", cfg.app_dir)
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
/// 10. clear the previous-release marker (a first deploy has no previous),
/// 11. bounded `/ready` poll on the blue loopback port,
/// 12. route the proxy at `127.0.0.1:{blue_port}`.
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
        // A first deploy has no previous release: clear any stale previous-release
        // marker so an on-demand rollback correctly reports NoPreviousRelease
        // rather than pointing at a since-removed release from a prior lifecycle.
        DeployOp::Run(RemoteCommand::new(
            "clear-previous",
            format!("rm -f {}", shell_quote(&previous_release_marker(cfg))),
        )),
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
/// 10. record the previous-release marker (the release being replaced: its dir +
///     live slot), BEFORE `current` moves off it,
/// 11. promote `current` to the new release,
/// 12. record the live slot marker,
/// 13. drain (stop) the old release,
/// 14. prune old releases beyond `keep_releases`, always protecting the releases
///     `current` and the previous-release marker point at (rollback targets).
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
        // Persist the release being replaced (the current live release: its dir +
        // live slot) as the new "previous release" so a later rollback returns to
        // it. Must run BEFORE `link-current` repoints the symlink, since it reads
        // the pre-repoint `current` target.
        DeployOp::Run(record_previous_release(cfg, plan.live_slot)),
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
            prune_releases_shell(
                &cfg.releases_dir(),
                &current,
                &previous_release_marker(cfg),
                cfg.keep_releases,
            ),
        )),
    ]
}

/// Ops that tear down a failed candidate so a retry starts clean (Slice 3,
/// AC-4). The candidate slot unit is stopped and disabled and its release dir is
/// removed; the old release and the proxy's upstream are left untouched.
///
/// Pure — `release_id` and the [`SlotPlan`] are injected. Both ops are
/// best-effort by construction (`|| true` on the unit teardown so a
/// never-enabled/half-started unit doesn't fail the cleanup), and they are also
/// driven through [`run_teardown`], which swallows executor errors — so a flaky
/// cleanup can never mask the real deploy failure. Removing the release dir means
/// a re-run uploads a fresh copy rather than reusing a half-written one.
#[must_use]
pub fn candidate_teardown_ops(
    cfg: &ResolvedDeployConfig,
    release_id: &str,
    plan: &SlotPlan,
) -> Vec<DeployOp> {
    let candidate_unit = slot_unit_name(&cfg.service_name, plan.candidate_slot);
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    vec![
        DeployOp::Run(RemoteCommand::new(
            "teardown-candidate-unit",
            format!("systemctl disable --now {candidate_unit}.service || true"),
        )),
        DeployOp::Run(RemoteCommand::new(
            "teardown-candidate-dir",
            format!("rm -rf {}", shell_quote(&release_dir)),
        )),
    ]
}

/// Teardown for a failed FIRST deploy (Slice 3). A first deploy has no previous
/// release, so on a pre-go-live failure the just-created state must be undone
/// COMPLETELY — otherwise the next `deploy up` sees the leftover `current`
/// symlink and live-slot marker that [`first_deploy_ops`] created and wrongly
/// takes the redeploy path with nothing actually serving.
///
/// This is the redeploy [`candidate_teardown_ops`] (stop+disable the candidate
/// slot unit, remove its release dir) PLUS removal of the markers first-deploy
/// created: the `current` symlink and the live-slot marker (and the
/// previous-release marker for good measure — first deploy clears rather than
/// writes it, but `rm -f` is harmless). Every op is best-effort (`rm -f` never
/// fails on a missing path) and driven through [`run_teardown`], which swallows
/// executor errors so a flaky cleanup can never mask the real deploy failure.
///
/// This must NOT be used for a redeploy: the redeploy teardown deliberately
/// leaves the old release's `current`/live-slot markers intact because that old
/// release is still serving.
#[must_use]
pub fn first_deploy_teardown_ops(
    cfg: &ResolvedDeployConfig,
    release_id: &str,
    plan: &SlotPlan,
) -> Vec<DeployOp> {
    let mut ops = candidate_teardown_ops(cfg, release_id, plan);
    ops.push(DeployOp::Run(RemoteCommand::new(
        "teardown-current-symlink",
        format!("rm -f {}", shell_quote(&cfg.current_symlink())),
    )));
    ops.push(DeployOp::Run(RemoteCommand::new(
        "teardown-slot-markers",
        format!(
            "rm -f {} {}",
            shell_quote(&live_slot_marker(cfg)),
            shell_quote(&previous_release_marker(cfg)),
        ),
    )));
    ops
}

/// The previous release an on-demand rollback repoints to, resolved from the
/// target by [`resolve_rollback_target`].
///
/// In the blue/green scheme the previous release ran on the slot recorded in the
/// previous-release marker (the slot NOT currently live; it was drained, not
/// deleted, as long as pruning kept it). Rolling back therefore means bringing
/// that slot's unit back up and flipping the proxy to its port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackTarget {
    /// Absolute release dir the `current` symlink is repointed at.
    pub release_dir: String,
    /// Slot the previous release runs on (the slot NOT currently live).
    pub slot: &'static str,
    /// Loopback port the previous release binds.
    pub port: u16,
}

/// Probe the target to resolve the previous release for an on-demand rollback.
///
/// Reads the explicit **previous-release marker** (`shared/previous-release`),
/// written whenever `current` was last repointed to a different release (a
/// redeploy cutover or a prior rollback). The marker stores the previous release's
/// absolute dir AND the slot it runs on as `{dir}\t{slot}`, so the two are always
/// consistent — unlike the old mtime scan, which broke after an
/// A→B→rollback→A→deploy-C history (the mtime-newest non-current dir is B, but the
/// true prior-live release is A). Returns [`DeployExecError::NoPreviousRelease`]
/// when the marker is absent or empty (a first deploy clears it, and a deployment
/// predating this marker simply has none), so the caller fails loudly with a
/// non-zero status instead of destroying anything.
///
/// # Errors
///
/// Returns [`DeployExecError::NoPreviousRelease`] when there is nothing to roll
/// back to, or the executor's error if the probe command cannot run.
pub fn resolve_rollback_target(
    cfg: &ResolvedDeployConfig,
    public_port: u16,
    exec: &impl DeployExecutor,
) -> Result<RollbackTarget, DeployExecError> {
    // Emit `prev:<abs-dir>\t<slot>` from the persisted previous-release marker, or
    // `none` when the marker is absent/empty. Both the dir and the slot come from
    // the SAME marker line, so they can never disagree.
    let shell = format!(
        "prev=$(cat {marker} 2>/dev/null); \
         if [ -z \"$prev\" ]; then printf 'none'; else printf 'prev:%s' \"$prev\"; fi",
        marker = shell_quote(&previous_release_marker(cfg)),
    );
    let out = exec.run(&RemoteCommand::new("resolve-previous", shell))?;
    let stdout = out.stdout.trim();
    let Some(rest) = stdout.strip_prefix("prev:") else {
        return Err(DeployExecError::NoPreviousRelease);
    };
    let mut parts = rest.splitn(2, '\t');
    let release_dir = parts.next().unwrap_or_default().trim().to_owned();
    if release_dir.is_empty() {
        return Err(DeployExecError::NoPreviousRelease);
    }
    // The marker records the previous release's OWN slot directly (dir + slot are
    // consistent), so no `other_slot` inference is needed.
    let slot = canonical_slot(parts.next().unwrap_or(SLOT_BLUE).trim());
    Ok(RollbackTarget {
        release_dir,
        slot,
        port: slot_app_port(public_port, slot),
    })
}

/// Build the ordered on-demand ROLLBACK sequence (Slice 3, AC-4).
///
/// Pure — the [`RollbackTarget`] is resolved up front by
/// [`resolve_rollback_target`] and injected for determinism. The previous unit is
/// brought back up on its slot BEFORE the flip because the proxy flip is
/// health-gated (`kamal-proxy deploy` blocks on the target's `/ready`) and would
/// time out against a stopped upstream. The sequence:
///
/// 1. bring the previous release's slot unit back up (it was drained on the last
///    cutover, not deleted),
/// 2. health-gated proxy flip back to the previous release's loopback port,
/// 3. record the previous-release marker as the release we roll back FROM (the
///    current live release: its dir + the former-live slot), BEFORE `current`
///    moves off it, so a subsequent rollback returns to it,
/// 4. repoint `current` at the previous release,
/// 5. record the live slot marker (now the previous slot),
/// 6. bounded `/ready` re-probe to confirm the rollback is healthy,
/// 7. drain (disable) the slot the rollback flipped traffic AWAY from — the slot
///    that was live before the rollback (`other_slot(target.slot)`).
///
/// Step 6 restores the invariant "only the live slot runs" (symmetric with
/// [`cutover_ops`]'s `drain-old`). Without it the just-rolled-back slot keeps
/// running its old binary; the next deploy reads the live-slot marker, reuses that
/// still-running slot as its idle candidate, and `systemctl enable --now` does NOT
/// restart an already-active unit — so readiness and the proxy flip would target
/// the rolled-back binary instead of the newly uploaded release. It runs after the
/// `/ready` re-probe so the old slot is never torn down until the rolled-back
/// release is confirmed healthy (the same confirm-before-drain ordering cutover
/// uses).
///
/// The caller runs [`resolve_rollback_target`] first, so its `resolve-previous`
/// probe precedes these ops in the recorded sequence.
#[must_use]
pub fn rollback_ops(
    cfg: &ResolvedDeployConfig,
    proxy: &impl ProxyController,
    target: &RollbackTarget,
) -> Vec<DeployOp> {
    let previous_unit = slot_unit_name(&cfg.service_name, target.slot);
    // The slot that was live before this rollback — traffic just moved away from
    // it. `target.slot` is the slot we roll back TO, so the former-live slot is the
    // OTHER one.
    let rolled_back_unit = slot_unit_name(&cfg.service_name, other_slot(target.slot));
    let current = cfg.current_symlink();
    vec![
        DeployOp::Run(RemoteCommand::new(
            "restart-previous",
            format!("systemctl enable --now {previous_unit}.service"),
        )),
        // Health-gated flip back: the previous unit (brought up above) must pass
        // /ready before the proxy swaps traffic to it.
        proxy.flip_op(&cfg.service_name, &loopback_upstream(target.port)),
        // Persist the release we are rolling back FROM (the current live release:
        // its dir + the former-live slot) as the new "previous release", so a
        // subsequent rollback returns to it. Must run BEFORE `link-current`
        // repoints the symlink, since it reads the pre-repoint `current` target.
        // The former-live slot is `other_slot(target.slot)` (the slot traffic just
        // moved away from).
        DeployOp::Run(record_previous_release(cfg, other_slot(target.slot))),
        DeployOp::Run(RemoteCommand::new(
            "link-current",
            format!(
                "ln -sfn {} {}",
                shell_quote(&target.release_dir),
                shell_quote(&current)
            ),
        )),
        DeployOp::Run(record_live_slot(cfg, target.slot)),
        DeployOp::Run(RemoteCommand::new(
            "readiness-gate",
            readiness_poll_shell(target.port, cfg.readiness_timeout_secs),
        )),
        // Traffic has moved back to the previous slot; disable the slot that was
        // live before the rollback so the invariant "only the live slot runs"
        // holds. Otherwise the next deploy reuses this still-running slot as its
        // idle candidate and `enable --now` won't restart it, serving the
        // rolled-back binary. Symmetric with cutover_ops' `drain-old`.
        DeployOp::Run(RemoteCommand::new(
            "drain-rolled-back-slot",
            format!("systemctl disable --now {rolled_back_unit}.service"),
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

/// Command that records the release being replaced (its absolute dir + the `slot`
/// it runs on) as the new "previous release", read by a later on-demand rollback's
/// [`resolve_rollback_target`]. It reads the CURRENT symlink's target — the
/// release that is live right now, about to be replaced — so it MUST run before
/// the `link-current` op repoints the symlink. `slot` is the slot that release
/// runs on (the live slot on a redeploy; the former-live slot on a rollback), so
/// the marker's dir and slot are always consistent. If `current` is somehow absent
/// the marker is left untouched (a rollback then degrades to no-previous-release
/// rather than pointing at a bogus dir).
fn record_previous_release(cfg: &ResolvedDeployConfig, slot: &str) -> RemoteCommand {
    RemoteCommand::new(
        "record-previous",
        format!(
            "prev=$(readlink {current} 2>/dev/null); \
             if [ -n \"$prev\" ]; then printf '%s\\t%s' \"$prev\" {slot} > {marker}; fi",
            current = shell_quote(&cfg.current_symlink()),
            slot = shell_quote(slot),
            marker = shell_quote(&previous_release_marker(cfg)),
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
    // Scope the transient unit to this release so overlapping deploys (or a prior
    // run whose unit was not yet collected) never collide on "Unit already
    // exists". The release id is the final path component of the release dir.
    let release_id = release_dir.rsplit('/').next().unwrap_or(release_dir);
    RemoteCommand::new(
        "migrate",
        format!(
            "systemd-run --wait --collect --quiet --unit={service}-migrate-{release_id} \
             --property=EnvironmentFile={env} --setenv=AUTUMN_MIGRATE=1 {bin}",
            service = cfg.service_name,
            env = shell_quote(&cfg.env_file()),
            bin = shell_quote(&bin),
        ),
    )
}

/// Prune shell: keep the newest `keep` release dirs, delete the rest — but NEVER
/// delete the two releases rollback depends on, regardless of their mtime.
///
/// A pure mtime prune (`ls -1dt` newest-first, drop the newest `keep`, `rm -rf`
/// the rest) is unsafe once a rollback is in the history: after
/// `A→B→rollback-to-A→deploy-C` the `current` symlink and the
/// `shared/previous-release` marker point at an OLDER release (`A`) while newer
/// abandoned dirs (`B`) exist, so a naive prune would delete `A` and leave
/// `current` / the rollback target dangling — the next `deploy rollback` could
/// not start it.
///
/// So this resolves the two PROTECTED release basenames first and excludes them
/// from the candidate set before applying the count bound:
///   - `cur` = basename of what `current` resolves to (`readlink -f`), and
///   - `prev` = basename of the dir field (first tab-separated field) of the
///     previous-release marker, when that file exists.
///
/// The remaining (non-protected) dirs are listed newest-first and everything past
/// the newest `keep` of THEM is removed. The protected dirs always survive, even
/// when they are the oldest by mtime; `keep` bounds only the non-protected count.
///
/// `keep` is still clamped to at least 1 so at least one non-protected release is
/// retained. Empty `cur`/`prev` (missing symlink or marker) make their
/// `grep -vxF ""` a no-op that excludes nothing. POSIX-sh safe; interpolated
/// paths are shell-quoted.
#[must_use]
fn prune_releases_shell(
    releases_dir: &str,
    current: &str,
    previous_marker: &str,
    keep: u32,
) -> String {
    let keep = keep.max(1);
    format!(
        "cd {dir} && \
         cur=$(basename \"$(readlink -f {current} 2>/dev/null)\" 2>/dev/null); \
         prev=; \
         if [ -f {marker} ]; then prev=$(basename \"$(cut -f1 {marker} 2>/dev/null)\" 2>/dev/null); fi; \
         ls -1dt */ 2>/dev/null | sed 's:/$::' | grep -vxF \"$cur\" | grep -vxF \"$prev\" \
         | tail -n +{tail} | xargs -r rm -rf",
        dir = shell_quote(releases_dir),
        current = shell_quote(current),
        marker = shell_quote(previous_marker),
        tail = keep + 1,
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

/// Which flavor of auto-rollback a failed pre-cutover step triggers — the two
/// differ only in the honest error they surface (a redeploy has a previous
/// release still serving; a first deploy does not).
#[derive(Debug, Clone, Copy)]
enum TeardownKind {
    /// Redeploy: the previous release keeps serving after the candidate is torn
    /// down (AC-4).
    PreviousStillServing,
    /// First deploy: no previous release exists, so nothing serves afterward.
    NoPreviousRelease,
}

/// Execute a FIRST-deploy op sequence, gated on preflight, with an honest
/// teardown of the just-started candidate if it fails before going live.
///
/// If any preflight check failed the function returns
/// [`DeployExecError::PreflightAborted`] WITHOUT making a single executor call
/// (AC-6 — fail fast before touching the server). If a step fails at or before
/// the go-live boundary (`proxy-route`), `teardown` runs (best-effort) and the
/// call fails with [`DeployExecError::FirstDeployTornDown`] — there is no
/// previous release to fall back to, so the path fails loudly.
///
/// # Errors
///
/// Returns [`DeployExecError::PreflightAborted`] when any preflight check failed,
/// [`DeployExecError::FirstDeployTornDown`] on a pre-go-live failure, or the
/// underlying executor error for a post-go-live failure.
pub fn execute_first_deploy(
    checks: &[PreflightCheck],
    ops: &[DeployOp],
    teardown: &[DeployOp],
    exec: &impl DeployExecutor,
) -> Result<(), DeployExecError> {
    execute_with_teardown(
        checks,
        ops,
        teardown,
        "proxy-route",
        TeardownKind::NoPreviousRelease,
        exec,
    )
}

/// Execute a zero-downtime redeploy cutover sequence, gated on preflight, with an
/// automatic rollback of the candidate on a failure before the cutover (AC-4).
///
/// A failed migration or a readiness timeout — anything at or before the
/// `proxy-flip` boundary — never flips traffic (the flip is health-gated and
/// only reached after `/ready` passes). Slice 3 turns that abort into an
/// explicit, clean auto-rollback: `teardown` stops and disables the candidate
/// slot unit and removes the candidate release dir, the proxy is left pointing at
/// the still-serving old release, and the call fails with
/// [`DeployExecError::CandidateRolledBack`]. A failure AFTER the flip (promote,
/// drain, prune) does not tear the candidate down — it is already live.
///
/// # Errors
///
/// Returns [`DeployExecError::PreflightAborted`] when any preflight check failed,
/// [`DeployExecError::CandidateRolledBack`] on a pre-flip failure, or the
/// underlying executor error for a post-flip failure.
pub fn execute_redeploy(
    checks: &[PreflightCheck],
    ops: &[DeployOp],
    teardown: &[DeployOp],
    exec: &impl DeployExecutor,
) -> Result<(), DeployExecError> {
    execute_with_teardown(
        checks,
        ops,
        teardown,
        "proxy-flip",
        TeardownKind::PreviousStillServing,
        exec,
    )
}

/// Execute an on-demand rollback op sequence ([`rollback_ops`]), gated on
/// preflight exactly like the deploy entrypoints.
///
/// The previous-release resolution ([`resolve_rollback_target`]) has already run
/// by the time this is called, so the ops here simply drive the repoint; a
/// failure surfaces the underlying executor error (there is no further fallback
/// to roll back to).
///
/// # Errors
///
/// Returns [`DeployExecError::PreflightAborted`] when any preflight check failed,
/// or the first executor error otherwise.
pub fn execute_rollback(
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

/// Shared driver for the deploy entrypoints: gate on preflight, then run `ops`
/// one at a time; if a step fails at or before `boundary_label` run `teardown`
/// (best-effort — its own errors are swallowed so they can't mask the real
/// failure) and surface the honest rollback error named by `kind`. A failure
/// after the boundary is returned verbatim (the candidate is already live).
fn execute_with_teardown(
    checks: &[PreflightCheck],
    ops: &[DeployOp],
    teardown: &[DeployOp],
    boundary_label: &str,
    kind: TeardownKind,
    exec: &impl DeployExecutor,
) -> Result<(), DeployExecError> {
    let failed = checks.iter().filter(|c| !c.passed).count();
    if failed > 0 {
        return Err(DeployExecError::PreflightAborted { failed });
    }
    // The go-live op (flip/route) is the point of no return: a failure at or
    // before it means traffic never moved, so tearing the candidate down is safe.
    let boundary = ops.iter().position(|op| op.label() == boundary_label);
    for (index, op) in ops.iter().enumerate() {
        if let Err(source) = run_one(op, exec) {
            // Fail safe: if the boundary op was never found (`None`), we do NOT
            // know whether the candidate is already live, so we must never tear
            // it down — a missing/mislabeled boundary surfaces the raw error
            // instead of risking teardown of a possibly-live app. Only a known
            // boundary index at or after the failing step permits teardown.
            let at_or_before_boundary = boundary.is_some_and(|b| index <= b);
            if !at_or_before_boundary {
                return Err(source);
            }
            eprintln!(
                "  \u{2717} {} failed — rolling back the candidate\u{2026}",
                op.label()
            );
            run_teardown(teardown, exec);
            let failed_step = op.label();
            let source = Box::new(source);
            return Err(match kind {
                TeardownKind::PreviousStillServing => DeployExecError::CandidateRolledBack {
                    failed_step,
                    source,
                },
                TeardownKind::NoPreviousRelease => DeployExecError::FirstDeployTornDown {
                    failed_step,
                    source,
                },
            });
        }
    }
    Ok(())
}

/// Drive an op sequence against an executor, stopping at the first failure.
///
/// # Errors
///
/// Returns the first [`DeployExecError`] produced by the executor (or by staging
/// a local temp file for a [`DeployOp::WriteFile`]).
pub fn run_ops(ops: &[DeployOp], exec: &impl DeployExecutor) -> Result<(), DeployExecError> {
    for op in ops {
        run_one(op, exec)?;
    }
    Ok(())
}

/// Run a single op against the executor, emitting its (secret-free) label first.
fn run_one(op: &DeployOp, exec: &impl DeployExecutor) -> Result<(), DeployExecError> {
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
    Ok(())
}

/// Run best-effort teardown ops: each is attempted and its errors are ignored so
/// a flaky cleanup step can never mask the real deploy failure being reported.
fn run_teardown(ops: &[DeployOp], exec: &impl DeployExecutor) {
    for op in ops {
        let _ = run_one(op, exec);
    }
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
            route_cmd.contains("--target '127.0.0.1:3001'"),
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
                "record-previous",
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
            flip.contains("kamal-proxy deploy") && flip.contains("--target '127.0.0.1:3002'"),
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

        // The previous-release marker records the release being replaced: it reads
        // the pre-repoint `current` target and writes it + the LIVE (old, blue)
        // slot to shared/previous-release.
        let record_prev = exec
            .shell_for("record-previous")
            .expect("record-previous ran");
        assert!(
            record_prev.contains("readlink '/srv/autumn/myapp/current'")
                && record_prev.contains("/srv/autumn/myapp/shared/previous-release"),
            "record-previous reads current and writes the previous-release marker: {record_prev}"
        );
        assert!(
            record_prev.contains("'blue'"),
            "the replaced release ran on the live (blue) slot: {record_prev}"
        );

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
            pos("proxy-flip") < pos("record-previous"),
            "flip before recording the previous-release marker"
        );
        assert!(
            pos("record-previous") < pos("link-current"),
            "record the previous-release marker before `current` moves off it"
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

    /// The candidate-teardown ops for the redeploy sample (candidate on green).
    fn sample_teardown_ops() -> Vec<DeployOp> {
        let cfg = resolved();
        let plan = SlotPlan::redeploy(3000, SLOT_BLUE);
        candidate_teardown_ops(&cfg, RELEASE_ID, &plan)
    }

    #[test]
    fn redeploy_migration_failure_leaves_old_serving_and_cleans_candidate() {
        // Updated from the Slice 2 abort test: a failed migration now AUTO-rolls-
        // back the candidate (AC-4) rather than just aborting.
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let teardown = sample_teardown_ops();
        // The migrate one-shot fails (as a bad migration would on the host).
        let exec = RecordingExecutor::failing_on("migrate");
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];
        let err = execute_redeploy(&checks, &ops, &teardown, &exec)
            .expect_err("a failed migration must fail the deploy");
        // The error names migrate as the failed step and reports the candidate was
        // rolled back with the previous release still serving.
        assert!(
            matches!(
                err,
                DeployExecError::CandidateRolledBack {
                    failed_step: "migrate",
                    ..
                }
            ),
            "expected a CandidateRolledBack at migrate, got {err:?}"
        );

        let labels = exec.run_labels();
        // AC-3/AC-4: no cutover happened — no flip, no drain, no promote — so the
        // old release is untouched and still serving.
        assert!(
            !labels.contains(&"proxy-flip"),
            "no flip after a failed migration"
        );
        assert!(!labels.contains(&"drain-old"), "old release not drained");
        assert!(!labels.contains(&"link-current"), "current not repointed");
        assert!(!labels.contains(&"prune"), "nothing pruned");
        // The candidate is explicitly torn down: its slot unit is stopped/disabled
        // and its release dir removed so a retry starts clean.
        assert!(
            labels.contains(&"teardown-candidate-unit")
                && labels.contains(&"teardown-candidate-dir"),
            "candidate must be torn down: {labels:?}"
        );
        let td_unit = exec
            .shell_for("teardown-candidate-unit")
            .expect("teardown-candidate-unit ran");
        assert!(
            td_unit.contains("disable --now myapp-green.service"),
            "teardown stops+disables the candidate (green) slot unit: {td_unit}"
        );
        let td_dir = exec
            .shell_for("teardown-candidate-dir")
            .expect("teardown-candidate-dir ran");
        assert!(
            td_dir.contains("rm -rf") && td_dir.contains(RELEASE_DIR),
            "teardown removes the candidate release dir: {td_dir}"
        );
    }

    #[test]
    fn redeploy_readiness_failure_auto_rolls_back_and_tears_down_candidate() {
        // Updated from the Slice 2 abort test: a readiness timeout now AUTO-rolls-
        // back the candidate (AC-4) rather than just aborting.
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let teardown = sample_teardown_ops();
        // The candidate never reports /ready within the window.
        let exec = RecordingExecutor::failing_on("readiness-gate");
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];
        let err = execute_redeploy(&checks, &ops, &teardown, &exec)
            .expect_err("a readiness timeout must fail the deploy");
        assert!(
            matches!(
                err,
                DeployExecError::CandidateRolledBack {
                    failed_step: "readiness-gate",
                    ..
                }
            ),
            "expected a CandidateRolledBack at readiness-gate, got {err:?}"
        );

        let labels = exec.run_labels();
        // AC-2 safety: traffic never flips to an unhealthy candidate, and the old
        // release keeps serving.
        assert!(
            !labels.contains(&"proxy-flip"),
            "no flip on readiness timeout"
        );
        assert!(!labels.contains(&"drain-old"), "old release not drained");
        assert!(!labels.contains(&"link-current"), "current not repointed");
        // The failed candidate is torn down.
        assert!(
            labels.contains(&"teardown-candidate-unit")
                && labels.contains(&"teardown-candidate-dir"),
            "candidate must be torn down: {labels:?}"
        );
        let td_unit = exec
            .shell_for("teardown-candidate-unit")
            .expect("teardown-candidate-unit ran");
        assert!(
            td_unit.contains("disable --now myapp-green.service"),
            "teardown stops+disables the candidate (green) slot unit: {td_unit}"
        );
        let td_dir = exec
            .shell_for("teardown-candidate-dir")
            .expect("teardown-candidate-dir ran");
        assert!(
            td_dir.contains(RELEASE_DIR),
            "teardown removes the candidate release dir: {td_dir}"
        );
    }

    #[test]
    fn teardown_fails_safe_when_boundary_missing() {
        // Fail-safe guard (FIX): if the go-live boundary op is absent or
        // mislabeled, we cannot know whether the candidate is already live, so a
        // LATE failure must surface the raw error and NEVER run teardown — tearing
        // down a possibly-live app would be catastrophic.
        let ops = vec![
            DeployOp::Run(RemoteCommand::new("upload-release", "true")),
            // Mislabeled: the real boundary label is "proxy-flip", so the position
            // lookup resolves to `None` (boundary unknown).
            DeployOp::Run(RemoteCommand::new("prxy-flip-typo", "true")),
            // A LATE op — after where the flip would have moved traffic — fails.
            DeployOp::Run(RemoteCommand::new("record-live-slot", "true")),
        ];
        let teardown = vec![
            DeployOp::Run(RemoteCommand::new("teardown-candidate-unit", "true")),
            DeployOp::Run(RemoteCommand::new("teardown-candidate-dir", "true")),
        ];
        let exec = RecordingExecutor::failing_on("record-live-slot");
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];

        let err = execute_with_teardown(
            &checks,
            &ops,
            &teardown,
            "proxy-flip",
            TeardownKind::PreviousStillServing,
            &exec,
        )
        .expect_err("a late failure must still surface an error");

        // The raw executor error is surfaced verbatim — NOT wrapped as a rollback.
        assert!(
            matches!(
                err,
                DeployExecError::CommandFailed {
                    label: "record-live-slot",
                    ..
                }
            ),
            "an unknown boundary must surface the raw error, not a teardown wrapper: {err:?}"
        );

        // No teardown ran: the possibly-live candidate is left untouched.
        let labels = exec.run_labels();
        assert!(
            !labels.iter().any(|l| l.starts_with("teardown")),
            "a missing/mislabeled boundary must never trigger teardown: {labels:?}"
        );
        assert!(
            !labels.contains(&"teardown-candidate-unit")
                && !labels.contains(&"teardown-candidate-dir"),
            "candidate must not be disabled or removed on an unknown boundary: {labels:?}"
        );
    }

    #[test]
    fn deploy_rollback_produces_exact_ordered_sequence() {
        // The previous-release marker names release A on the BLUE slot directly
        // (the marker records the previous release's OWN slot), so rollback flips
        // back to blue (loopback 3001).
        let cfg = resolved();
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue",
        );
        let target = resolve_rollback_target(&cfg, 3000, &exec).expect("previous release resolves");
        assert_eq!(target.slot, SLOT_BLUE);
        assert_eq!(target.port, 3001);
        assert_eq!(
            target.release_dir,
            "/srv/autumn/myapp/releases/20260713T090000Z"
        );

        let ops = rollback_ops(&cfg, &proxy(), &target);
        run_ops(&ops, &exec).expect("recording executor never fails");

        // resolve-previous (the probe) precedes the ordered rollback ops: bring the
        // previous slot back up → health-gated flip to it → record the release we
        // roll back FROM as the new previous → promote → mark live → re-probe
        // /ready → drain the former-live slot.
        assert_eq!(
            exec.run_labels(),
            vec![
                "resolve-previous",
                "restart-previous",
                "proxy-flip",
                "record-previous",
                "link-current",
                "record-live-slot",
                "readiness-gate",
                "drain-rolled-back-slot",
            ],
            "unexpected rollback sequence"
        );

        // The previous-release marker now records the release rolled back FROM: it
        // reads the pre-repoint `current` and writes it + the FORMER-live (green)
        // slot, so a subsequent rollback returns to it.
        let record_prev = exec
            .shell_for("record-previous")
            .expect("record-previous ran");
        assert!(
            record_prev.contains("readlink '/srv/autumn/myapp/current'")
                && record_prev.contains("/srv/autumn/myapp/shared/previous-release"),
            "record-previous reads current and writes the previous-release marker: {record_prev}"
        );
        assert!(
            record_prev.contains("'green'"),
            "the rolled-back-from release ran on the former-live (green) slot: {record_prev}"
        );

        // The previous unit is brought up before the flip (the flip is health-
        // gated and would time out against a stopped upstream).
        let restart = exec.shell_for("restart-previous").expect("restart ran");
        assert!(
            restart.contains("enable --now myapp-blue.service"),
            "rollback brings the previous (blue) slot unit up: {restart}"
        );
        // The flip targets the PREVIOUS release's loopback address (blue = 3001).
        let flip = exec.shell_for("proxy-flip").expect("flip ran");
        assert!(
            flip.contains("kamal-proxy deploy") && flip.contains("--target '127.0.0.1:3001'"),
            "flip targets the previous release's address: {flip}"
        );
        // `current` is repointed at the previous release dir.
        let promote = exec.shell_for("link-current").expect("promote ran");
        assert!(
            promote.contains("/srv/autumn/myapp/releases/20260713T090000Z")
                && promote.contains("/srv/autumn/myapp/current"),
            "current is repointed to the previous release: {promote}"
        );
        // The re-probe targets the previous release's port.
        let gate = exec.shell_for("readiness-gate").expect("gate ran");
        assert!(gate.contains("127.0.0.1:3001/ready"), "gate: {gate}");

        // Ordering invariants.
        let labels = exec.run_labels();
        let pos = |l: &str| labels.iter().position(|&x| x == l).unwrap();
        assert!(
            pos("restart-previous") < pos("proxy-flip"),
            "the previous unit is up before the flip"
        );
        assert!(
            pos("proxy-flip") < pos("record-previous"),
            "flip before recording the previous-release marker"
        );
        assert!(
            pos("record-previous") < pos("link-current"),
            "record the previous-release marker before `current` moves off it"
        );
        assert!(
            pos("proxy-flip") < pos("link-current"),
            "flip before promote"
        );
        assert!(
            pos("link-current") < pos("readiness-gate"),
            "promote before the re-probe"
        );
        // The slot the rollback flipped traffic AWAY from (green, the former-live
        // slot) is drained AFTER the re-probe confirms the rolled-back release is
        // healthy — never the slot we rolled back to (blue).
        let drain = exec.shell_for("drain-rolled-back-slot").expect("drain ran");
        assert!(
            drain.contains("disable --now myapp-green.service"),
            "rollback drains the former-live (green) slot: {drain}"
        );
        assert!(
            !drain.contains("myapp-blue.service"),
            "rollback must NOT drain the slot it rolled back to (blue): {drain}"
        );
        assert!(
            pos("readiness-gate") < pos("drain-rolled-back-slot"),
            "drain the former-live slot only after confirming the rollback is healthy"
        );
    }

    #[test]
    fn rollback_drains_the_slot_it_flipped_away_from() {
        // Regression (Codex P1): rolling back must disable the slot that was live
        // before the rollback (the slot traffic moved AWAY from). Otherwise it keeps
        // running its old binary; the next deploy reuses it as the idle candidate
        // and `enable --now` won't restart it, so readiness and the proxy flip would
        // target the rolled-back binary instead of the newly uploaded release.
        let cfg = resolved();
        // The previous-release marker names the previous release on GREEN (its own
        // slot) → we roll back TO green and must drain the former-live BLUE slot.
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tgreen",
        );
        let target = resolve_rollback_target(&cfg, 3000, &exec).expect("previous release resolves");
        assert_eq!(target.slot, SLOT_GREEN, "roll back to the non-live slot");

        let ops = rollback_ops(&cfg, &proxy(), &target);
        run_ops(&ops, &exec).expect("recording executor never fails");

        let labels = exec.run_labels();
        let pos = |l: &str| labels.iter().position(|&x| x == l).unwrap();

        // The former-live slot's unit is disabled...
        let drain = exec.shell_for("drain-rolled-back-slot").expect("drain ran");
        assert!(
            drain.contains("disable --now myapp-blue.service"),
            "drain targets other_slot(previous) = the former-live blue slot: {drain}"
        );
        assert!(
            !drain.contains("myapp-green.service"),
            "drain must NOT target the slot we rolled back to (green): {drain}"
        );
        // ...and only AFTER the proxy flip has already moved traffic away.
        assert!(
            pos("proxy-flip") < pos("drain-rolled-back-slot"),
            "drain the former-live slot only after the flip moves traffic off it"
        );
        assert!(
            pos("readiness-gate") < pos("drain-rolled-back-slot"),
            "drain only after the rolled-back release is confirmed healthy"
        );
    }

    #[test]
    fn resolve_rollback_target_reads_the_marker_not_the_mtime_newest_dir() {
        // Codex P1: resolution must come from the explicit previous-release MARKER,
        // not an `ls -1dt` mtime scan. The marker names release A on green; a
        // newer-mtime release B also exists on the host, but B must be IGNORED —
        // the probe reads only `shared/previous-release`, so the resolved target is
        // A (its dir + slot + port), exactly as the marker records.
        let cfg = resolved();
        let marker_a = "/srv/autumn/myapp/releases/20260101T000000Z"; // older, but the marker
        let exec = RecordingExecutor::new()
            .with_stdout("resolve-previous", format!("prev:{marker_a}\tgreen"));
        let target =
            resolve_rollback_target(&cfg, 3000, &exec).expect("marker names a previous release");
        assert_eq!(target.release_dir, marker_a, "dir comes from the marker");
        assert_eq!(
            target.slot, SLOT_GREEN,
            "slot comes from the SAME marker line"
        );
        assert_eq!(
            target.port,
            slot_app_port(3000, SLOT_GREEN),
            "port derives from the marker's slot"
        );
        // The probe is a single read of the previous-release marker — no `ls -1dt`
        // mtime scan and no release-dir listing.
        let probe = exec.shell_for("resolve-previous").expect("probe ran");
        assert!(
            probe.contains("/srv/autumn/myapp/shared/previous-release"),
            "the probe reads the previous-release marker: {probe}"
        );
        assert!(
            !probe.contains("ls -1dt"),
            "the mtime scan must be gone: {probe}"
        );
    }

    #[test]
    fn resolve_rollback_target_absent_marker_degrades_to_no_previous() {
        // A deployment predating this marker (or right after a first deploy, which
        // clears it) has no previous-release marker: the probe emits `none` and
        // resolution degrades to NoPreviousRelease — it must not crash.
        let cfg = resolved();
        let exec = RecordingExecutor::new().with_stdout("resolve-previous", "none");
        let err = resolve_rollback_target(&cfg, 3000, &exec)
            .expect_err("an absent marker must error, not crash");
        assert!(
            matches!(err, DeployExecError::NoPreviousRelease),
            "expected NoPreviousRelease, got {err:?}"
        );
    }

    #[test]
    fn rollback_with_no_previous_release_errors() {
        // The target has only the current release (the probe emits `none`): there
        // is nothing to roll back to.
        let cfg = resolved();
        let exec = RecordingExecutor::new().with_stdout("resolve-previous", "none");
        let err =
            resolve_rollback_target(&cfg, 3000, &exec).expect_err("no previous release must error");
        assert!(
            matches!(err, DeployExecError::NoPreviousRelease),
            "expected NoPreviousRelease, got {err:?}"
        );
        // Only the read-only probe ran — no flip, no promote, nothing destructive.
        let labels = exec.run_labels();
        assert_eq!(labels, vec!["resolve-previous"], "only the probe may run");
        assert!(!labels.contains(&"proxy-flip"), "no flip");
        assert!(!labels.contains(&"link-current"), "current not repointed");
        assert!(
            !labels.iter().any(|l| l.starts_with("teardown")),
            "nothing torn down"
        );
    }

    #[test]
    fn first_deploy_readiness_failure_tears_down_candidate_no_previous() {
        // A FIRST deploy has no previous release: a readiness failure before the
        // app goes live tears the just-started candidate down and fails loudly.
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let cfg = resolved();
        let plan = SlotPlan::first(3000);
        let teardown = first_deploy_teardown_ops(&cfg, RELEASE_ID, &plan);
        let exec = RecordingExecutor::failing_on("readiness-gate");
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];
        let err = execute_first_deploy(&checks, &ops, &teardown, &exec)
            .expect_err("first-deploy readiness failure must fail the deploy");
        assert!(
            matches!(
                err,
                DeployExecError::FirstDeployTornDown {
                    failed_step: "readiness-gate",
                    ..
                }
            ),
            "expected FirstDeployTornDown at readiness-gate, got {err:?}"
        );
        let labels = exec.run_labels();
        // The proxy is never routed at an unhealthy first release, and the just-
        // started candidate (blue) is torn down.
        assert!(!labels.contains(&"proxy-route"), "no route on failed gate");
        let td_unit = exec
            .shell_for("teardown-candidate-unit")
            .expect("teardown-candidate-unit ran");
        assert!(
            td_unit.contains("disable --now myapp-blue.service"),
            "teardown stops+disables the first-deploy (blue) slot unit: {td_unit}"
        );
    }

    #[test]
    fn first_deploy_teardown_unlinks_current_and_marker() {
        // Codex P2: a failed FIRST deploy must undo the `current` symlink AND the
        // live-slot marker that first_deploy_ops created — not just the candidate
        // unit/dir — otherwise the next `deploy up` sees `-L current` and wrongly
        // takes the redeploy path with nothing serving.
        let cfg = resolved();
        let plan = SlotPlan::first(3000);
        let teardown = first_deploy_teardown_ops(&cfg, RELEASE_ID, &plan);
        let labels: Vec<&str> = teardown.iter().map(DeployOp::label).collect();
        // It is a superset of the candidate teardown, PLUS the marker cleanup.
        assert_eq!(
            labels,
            vec![
                "teardown-candidate-unit",
                "teardown-candidate-dir",
                "teardown-current-symlink",
                "teardown-slot-markers",
            ],
            "first-deploy teardown must also clean current + markers: {labels:?}"
        );

        // Drive the teardown and inspect the recorded shells.
        let exec = RecordingExecutor::new();
        run_teardown(&teardown, &exec);
        let rm_current = exec
            .shell_for("teardown-current-symlink")
            .expect("teardown-current-symlink ran");
        assert!(
            rm_current.contains("rm -f '/srv/autumn/myapp/current'"),
            "teardown unlinks the current symlink: {rm_current}"
        );
        let rm_markers = exec
            .shell_for("teardown-slot-markers")
            .expect("teardown-slot-markers ran");
        assert!(
            rm_markers.contains("/srv/autumn/myapp/shared/live-slot")
                && rm_markers.contains("/srv/autumn/myapp/shared/previous-release"),
            "teardown removes the live-slot and previous-release markers: {rm_markers}"
        );
    }

    #[test]
    fn redeploy_teardown_leaves_current_and_markers_intact() {
        // The REDEPLOY teardown must NOT remove the old release's current/live-slot
        // markers — the old release is still serving after a candidate rollback.
        let teardown = sample_teardown_ops();
        let labels: Vec<&str> = teardown.iter().map(DeployOp::label).collect();
        assert_eq!(
            labels,
            vec!["teardown-candidate-unit", "teardown-candidate-dir"],
            "redeploy teardown is candidate-only: {labels:?}"
        );
        assert!(
            !labels.contains(&"teardown-current-symlink"),
            "redeploy teardown must not unlink current: {labels:?}"
        );
        assert!(
            !labels.contains(&"teardown-slot-markers"),
            "redeploy teardown must not remove the live-slot marker: {labels:?}"
        );
    }

    #[test]
    fn prune_keeps_exactly_keep_releases() {
        let cfg = resolved();
        assert_eq!(cfg.keep_releases, 3, "default keep_releases");
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("run ok");
        let prune = exec.shell_for("prune").expect("prune ran");
        // `tail -n +4` keeps the newest 3 NON-protected release dirs and deletes
        // the rest.
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
    fn prune_preserves_current_and_previous_targets() {
        // The prune must resolve the releases `current` and the previous-release
        // marker point at and exclude those basenames from the delete set, so a
        // rollback target that is OLD by mtime is never pruned (the
        // A→B→rollback→A→deploy-C hazard).
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("run ok");
        let prune = exec.shell_for("prune").expect("prune ran");

        // Resolves `current` via readlink and the marker's first (dir) field.
        assert!(
            prune.contains("readlink -f '/srv/autumn/myapp/current'"),
            "prune resolves the current symlink target: {prune}"
        );
        assert!(
            prune.contains("cut -f1 '/srv/autumn/myapp/shared/previous-release'"),
            "prune reads the previous-release marker dir field: {prune}"
        );
        // Excludes both protected basenames from the candidate set BEFORE the
        // count bound (`tail`), so they survive regardless of mtime.
        let exclude = prune.find("grep -vxF \"$cur\"");
        let exclude_prev = prune.find("grep -vxF \"$prev\"");
        let tail = prune.find("tail -n +");
        assert!(
            exclude.is_some() && exclude_prev.is_some(),
            "prune excludes both protected basenames: {prune}"
        );
        assert!(
            exclude < tail && exclude_prev < tail,
            "protected exclusions precede the keep-count bound: {prune}"
        );
    }

    #[test]
    fn prune_shell_protects_current_and_previous_dirs_end_to_end() {
        // Behavioral proof: run the generated prune shell against a real tempdir of
        // fake release dirs, with `current` -> the newest dir and the marker naming
        // the OLDEST dir. Both protected dirs must survive; an old UNPROTECTED dir
        // must be removed.
        let root = std::env::temp_dir().join(format!(
            "autumn-prune-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let releases = root.join("releases");
        let shared = root.join("shared");
        std::fs::create_dir_all(&releases).expect("mk releases");
        std::fs::create_dir_all(&shared).expect("mk shared");

        // mtime newest -> oldest: cur_new, stale_new, stale_old, prev_old.
        let dirs = [
            ("prev_old", 100u64), // oldest, PROTECTED via marker
            ("stale_old", 200),   // unprotected, should be REMOVED
            ("stale_new", 300),   // unprotected, newest non-protected -> kept
            ("cur_new", 400),     // newest, PROTECTED via current symlink
        ];
        for (name, mtime) in dirs {
            let d = releases.join(name);
            std::fs::create_dir(&d).expect("mk release dir");
            // Set an explicit mtime (GNU touch, epoch seconds) so newest-first
            // ordering is deterministic without sleeping between creates.
            let ok = std::process::Command::new("touch")
                .arg("-m")
                .arg("-d")
                .arg(format!("@{mtime}"))
                .arg(&d)
                .status()
                .expect("run touch")
                .success();
            assert!(ok, "touch must set mtime");
        }

        let current = root.join("current");
        std::os::unix::fs::symlink(releases.join("cur_new"), &current).expect("symlink current");
        let marker = shared.join("previous-release");
        std::fs::write(
            &marker,
            format!("{}\tblue", releases.join("prev_old").display()),
        )
        .expect("write marker");

        // keep=1 bounds the NON-protected set to its single newest (stale_new),
        // so stale_old (older, unprotected) is pruned.
        let shell = prune_releases_shell(
            releases.to_str().unwrap(),
            current.to_str().unwrap(),
            marker.to_str().unwrap(),
            1,
        );
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&shell)
            .status()
            .expect("run prune shell");
        assert!(status.success(), "prune shell exited non-zero");

        assert!(
            releases.join("cur_new").exists(),
            "current target must survive"
        );
        assert!(
            releases.join("prev_old").exists(),
            "previous-release target must survive even though it is the oldest"
        );
        assert!(
            releases.join("stale_new").exists(),
            "newest non-protected release is retained by keep=1"
        );
        assert!(
            !releases.join("stale_old").exists(),
            "old unprotected release must be pruned"
        );

        std::fs::remove_dir_all(&root).ok();
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

        let err = execute_first_deploy(&checks, &ops, &[], &exec)
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
        execute_first_deploy(&checks, &ops, &[], &exec).expect("all checks pass → deploy runs");
        // 2 proxy ops + 10 first-deploy ops (incl. clear-previous) + 1 proxy route = 13.
        assert_eq!(exec.calls().len(), 13, "the full op sequence should run");
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
