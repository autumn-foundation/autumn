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
//!   / [`probe_deploy_state`] pick first-vs-redeploy from a remote probe.
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

/// A project config manifest file to upload into the per-release dir so
/// the deployed app loads the intended (non-secret) config instead of silently
/// falling back to built-in defaults (issue #1952).
///
/// `local` is the on-disk source in the project directory (e.g. `./autumn.toml`
/// or a profile sibling `./autumn-prod.toml`); `remote_basename` is the file name
/// written under the release dir. The systemd unit sets `AUTUMN_MANIFEST_DIR`
/// to that release dir so the app's config loader reads these files at boot —
/// coupling the config to the binary it shipped with, so a rollback reads the
/// rolled-back release's own manifest.
///
/// The RAW manifest is uploaded, NOT a flattened/merged config: the app applies
/// its `[profile.<AUTUMN_ENV>]` overlay at runtime (`AUTUMN_ENV` is set in the env
/// file), so shipping the raw manifest(s) preserves profile structure and matches
/// the repo exactly.
#[derive(Debug, Clone)]
pub struct ManifestUpload {
    /// Local source path in the project directory.
    pub local: PathBuf,
    /// File name to write under the release dir.
    pub remote_basename: String,
}

/// Build the config-manifest upload ops (issue #1952).
///
/// Each manifest is uploaded at mode `0600` (owner-only). The deployed app runs
/// as the same deploy user that owns the release dir — the user that already
/// reads the `0600` `autumn.env` — so a `0600` manifest still loads at boot,
/// while no other local account can read it. Owner-only matters because a
/// project `autumn.toml` can legitimately carry inline credentials (e.g.
/// `[security.signing_secret]`); world-readable (`0644`) would expose those to
/// every account on the host.
///
/// The manifest is written into the per-release dir (NOT the shared dir) so the
/// config is coupled to the binary it shipped with: a rollback that re-renders
/// the unit from the target release dir automatically reads that release's OWN
/// manifest, and a fresh release dir only carries the manifests uploaded THIS
/// deploy (so removing a local override and redeploying no longer leaves a
/// stale one loaded). Re-uploaded on every deploy (both first deploy and
/// cutover).
fn manifest_upload_ops(release_dir: &str, manifests: &[ManifestUpload]) -> Vec<DeployOp> {
    manifests
        .iter()
        .map(|m| DeployOp::UploadFile {
            label: "upload-config",
            local: m.local.clone(),
            remote_path: format!("{release_dir}/{}", m.remote_basename),
            // 0600: owner-only. The deploy user (which runs the app) reads it;
            // other local accounts can't, so inline config secrets stay private.
            mode: Some(0o600),
        })
        .collect()
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
    /// The installed reverse-proxy binary's CLI surface is incompatible with what
    /// the cutover requires, so the deploy is aborted BEFORE any cutover (issue
    /// #2053). The `message` is a clear, actionable, secret-free operator string
    /// (which flag/subcommand is missing + the remedy).
    #[error("{message}")]
    ProxyIncompatible {
        /// The actionable operator message built by the proxy controller.
        message: String,
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
    /// An on-demand rollback failed at or before the health-gated flip: the flip
    /// never moved traffic, so the slot the rollback restarted was disabled again
    /// and the ORIGINAL release is still serving (Slice 3). Never embeds a secret —
    /// only the failing step's label and the redacted source error.
    #[error(
        "rollback failed at `{failed_step}`; the restarted slot was disabled again — \
         the original release is still serving"
    )]
    RollbackFailed {
        /// Label of the step that failed at or before the flip.
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

/// Remote marker file recording the PREVIOUS live release — its absolute dir, the
/// slot it runs on, and the loopback port it actually listens on, stored as
/// `{dir}\t{slot}\t{port}` (older markers omit the trailing port) — so a rollback
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
/// 8. `systemctl enable {service}-blue.service && systemctl restart
///    {service}-blue.service` (restart, not `enable --now`, so an already-active
///    slot always relaunches the freshly written unit — see the op's comment),
/// 9. record the live slot marker,
/// 10. clear the previous-release marker (a first deploy has no previous),
/// 11. bounded `/ready` poll on the blue loopback port,
/// 12. route the proxy at `127.0.0.1:{blue_port}`.
#[must_use]
// One cohesive op-plan builder; each param is a distinct injected input
// (config, proxy, unit text, secret env, binary path, config manifests, release
// id, slot plan) kept as parameters for pure/testable determinism.
#[allow(clippy::too_many_arguments)]
pub fn first_deploy_ops(
    cfg: &ResolvedDeployConfig,
    proxy: &impl ProxyController,
    unit: &str,
    env_file: Secret,
    binary_local: &Path,
    manifests: &[ManifestUpload],
    release_id: &str,
    plan: &SlotPlan,
) -> Vec<DeployOp> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    let remote_binary = format!("{release_dir}/{}", cfg.app_name);
    let shared_dir = cfg.shared_dir();
    let env_path = cfg.env_file();
    let current = cfg.current_symlink();
    let unit_name = slot_unit_name(&cfg.service_name, plan.candidate_slot);
    let unit_path = format!("/etc/systemd/system/{unit_name}.service");

    let mut ops = proxy.ensure_installed_ops(plan.public_port);
    ops.push(DeployOp::Run(RemoteCommand::new(
        "prepare-dirs",
        format!(
            "mkdir -p {} {}",
            shell_quote(&release_dir),
            shell_quote(&shared_dir)
        ),
    )));
    ops.push(DeployOp::UploadFile {
        label: "upload-binary",
        local: binary_local.to_path_buf(),
        remote_path: remote_binary,
        mode: Some(0o755),
    });
    // Upload the project config manifest(s) into the per-release dir so the
    // deployed app loads the intended config instead of silent built-in defaults
    // (#1952). The manifest is coupled to the binary it shipped with: the slot
    // unit points AUTUMN_MANIFEST_DIR at this release dir, so a rollback that
    // re-renders the unit from the rolled-back release dir reads that release's
    // OWN manifest.
    ops.extend(manifest_upload_ops(&release_dir, manifests));
    ops.extend([
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
        // enable = boot-persistence; restart = start-or-relaunch. We deliberately
        // use `restart` (not `enable --now`) because an already-active slot — one
        // left running by drift — must ALWAYS relaunch the freshly written unit:
        // `enable --now` will NOT relaunch a unit that is already active, so it
        // could keep serving a stale process the readiness gate then probes.
        // `daemon-reload` ran immediately above, so `restart` loads the new unit.
        DeployOp::Run(RemoteCommand::new(
            "enable-now",
            format!(
                "systemctl enable {unit_name}.service && systemctl restart {unit_name}.service"
            ),
        )),
        DeployOp::Run(record_live_slot(
            cfg,
            plan.candidate_slot,
            plan.candidate_port,
        )),
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
/// 0. refresh the shared proxy unit (issue #2070) — rewrite the reboot-durable
///    unit so an existing host adopts it on upgrade, restarting + re-registering the
///    live upstream ONLY when the unit actually changed (an unchanged unit does
///    nothing, preserving steady-state behavior),
/// 1. prepare remote dirs,
/// 2. upload the release binary (`0755`),
/// 3. write the secret env file (`0600`, AC-5),
/// 4. write the candidate slot's systemd unit (binds `127.0.0.1:{candidate_port}`),
/// 5. `systemctl daemon-reload`,
/// 6. start the candidate via `enable` + `restart` (old release untouched;
///    restart, not `enable --now`, so an already-active slot relaunches the
///    freshly written unit — see the op's comment),
/// 7. run pending migrations BEFORE cutover (`AUTUMN_MIGRATE=1` one-shot),
/// 8. bounded `/ready` poll on the candidate's separate loopback port,
/// 9. health-gated proxy flip old→candidate (THE cutover),
/// 10. commit the state markers as ONE atomic remote op (#1938): record the
///     previous-release marker (the release being replaced: its dir + live slot)
///     BEFORE `current` moves off it, promote `current` to the new release, then
///     record the live slot marker — each marker written via temp-file + `mv`,
///     the whole thing landing or failing as a unit,
/// 11. drain (stop) the old release,
/// 12. prune old releases beyond `keep_releases`, always protecting the releases
///     `current` and the previous-release marker point at (rollback targets).
#[must_use]
// One cohesive op-plan builder; each param is a distinct injected input kept as a
// parameter for pure/testable determinism (mirrors `first_deploy_ops`).
#[allow(clippy::too_many_arguments)]
pub fn cutover_ops(
    cfg: &ResolvedDeployConfig,
    proxy: &impl ProxyController,
    unit: &str,
    env_file: Secret,
    binary_local: &Path,
    manifests: &[ManifestUpload],
    release_id: &str,
    plan: &SlotPlan,
) -> Vec<DeployOp> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    let remote_binary = format!("{release_dir}/{}", cfg.app_name);
    let shared_dir = cfg.shared_dir();
    let env_path = cfg.env_file();
    let current = cfg.current_symlink();
    let candidate_unit = slot_unit_name(&cfg.service_name, plan.candidate_slot);
    let candidate_unit_path = format!("/etc/systemd/system/{candidate_unit}.service");
    let live_unit = slot_unit_name(&cfg.service_name, plan.live_slot);

    // Refresh the SHARED proxy unit on the redeploy path too (issue #2070).
    // Previously only `first_deploy_ops` wrote the proxy unit, so a fix landed in
    // the unit (e.g. the reboot-durable `StateDirectory`/`HOME` of #2069) never
    // reached an already-provisioned host on upgrade. Prepending the idempotent
    // install (mirroring how `first_deploy_ops` starts) rewrites it on every
    // redeploy, and — kamal-proxy only — restarts + re-registers the live upstream
    // ONLY when the unit actually changed, so an existing host adopts the new unit
    // with a routeless window bounded to ~the restart (see
    // `ProxyController::refresh_installed_ops`). Writing the unit is deterministic,
    // lands at the final path, and causes no restart on its own, so it is safe to do
    // at the very start of the cutover; the live upstream re-registered on a change
    // is the slot serving RIGHT NOW (`plan.live_port`), the same target the proxy
    // already fronts.
    let mut ops = proxy.refresh_installed_ops(
        plan.public_port,
        &cfg.service_name,
        &loopback_upstream(plan.live_port),
        &proxy_unit_snapshot_path(release_id),
    );
    ops.push(DeployOp::Run(RemoteCommand::new(
        "prepare-dirs",
        format!(
            "mkdir -p {} {}",
            shell_quote(&release_dir),
            shell_quote(&shared_dir)
        ),
    )));
    ops.push(DeployOp::UploadFile {
        label: "upload-binary",
        local: binary_local.to_path_buf(),
        remote_path: remote_binary,
        mode: Some(0o755),
    });
    // Re-upload the project config manifest(s) into the per-release dir on every
    // redeploy so the config on the server always matches the shipped binary and
    // is coupled to it for rollback (#1952).
    ops.extend(manifest_upload_ops(&release_dir, manifests));
    ops.extend([
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
        // enable = boot-persistence; restart = start-or-relaunch. We deliberately
        // use `restart` (not `enable --now`) because an already-active candidate
        // slot — one left running by drift — must ALWAYS relaunch the freshly
        // written unit: `enable --now` will NOT relaunch a unit that is already
        // active, so the readiness gate below could probe a stale process instead
        // of the new release. `daemon-reload` ran above, so `restart` loads the
        // new unit.
        DeployOp::Run(RemoteCommand::new(
            "start-candidate",
            format!(
                "systemctl enable {candidate_unit}.service && \
                 systemctl restart {candidate_unit}.service"
            ),
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
        // Commit the on-disk state markers as ONE remote transaction after the flip
        // (#1938): a single SSH round-trip that either lands as a unit or fails as a
        // unit, so a failure between the flip and completing the markers can no
        // longer leave the proxy on the new release while the markers still describe
        // the old one. Internally it (1) records the release being replaced (its dir
        // + live slot) as the new "previous release" — read from `current` +
        // live-slot BEFORE they change — then (2) repoints `current` to the new
        // release, then (3) records the new live slot. Each marker file is written
        // via temp-file + `mv` (atomic rename), not a truncating redirect.
        DeployOp::Run(commit_markers_command(
            cfg,
            &release_dir,
            plan.candidate_slot,
            plan.candidate_port,
            PrevMarkerFallback {
                slot: plan.live_slot,
                port: plan.live_port,
            },
        )),
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
    ]);
    ops
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
/// absolute dir, the slot it runs on, AND the loopback port it actually listens on
/// as `{dir}\t{slot}\t{port}`, so all three are always consistent — unlike the old
/// mtime scan, which broke after an
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
    // Emit `prev:<abs-dir>\t<slot>\t<port>` from the persisted previous-release
    // marker, or `none` when the marker is absent/empty. The dir, slot, and port all
    // come from the SAME marker line, so they can never disagree.
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
    let mut parts = rest.splitn(3, '\t');
    let release_dir = parts.next().unwrap_or_default().trim().to_owned();
    if release_dir.is_empty() {
        return Err(DeployExecError::NoPreviousRelease);
    }
    // The marker records the previous release's OWN slot directly (dir + slot are
    // consistent), so no `other_slot` inference is needed.
    let slot = canonical_slot(parts.next().unwrap_or(SLOT_BLUE).trim());
    // Use the port the previous release ACTUALLY listens on, persisted in the
    // marker at its deploy time — not `slot_app_port(current server.port, slot)`,
    // which is wrong if a deploy changed `server.port` since. A marker predating
    // the port field (older 2-field format) falls back to the derived port so
    // parsing never fails.
    let port = parts
        .next()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or_else(|| slot_app_port(public_port, slot));
    Ok(RollbackTarget {
        release_dir,
        slot,
        port,
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
/// 1. re-render the target release's slot unit from the persisted marker (its dir +
///    port), so rollback never restarts a slot unit an earlier failed redeploy
///    clobbered (a slot's unit is per-slot and gets overwritten to point at a new
///    candidate on any redeploy reusing that slot),
/// 2. `systemctl daemon-reload` so the restart below loads the freshly written unit,
/// 3. bring the previous release's slot unit back up (it was drained on the last
///    cutover, not deleted),
/// 4. health-gated proxy flip back to the previous release's loopback port,
/// 5. record the previous-release marker as the release we roll back FROM (the
///    current live release: its dir + the former-live slot), BEFORE `current`
///    moves off it, so a subsequent rollback returns to it,
/// 6. repoint `current` at the previous release,
/// 7. record the live slot marker (now the previous slot),
/// 8. bounded `/ready` re-probe to confirm the rollback is healthy,
/// 9. drain (disable) the slot the rollback flipped traffic AWAY from — the slot
///    that was live before the rollback (`other_slot(target.slot)`).
///
/// Step 9 restores the invariant "only the live slot runs" (symmetric with
/// [`cutover_ops`]'s `drain-old`). Without it the just-rolled-back slot's former
/// peer keeps running its old binary and the next deploy would reuse that
/// still-running slot as its idle candidate. The slot-START ops now force a
/// `restart` (not `enable --now`), so a still-active slot is relaunched onto the
/// newly uploaded release rather than serving a stale binary — but draining here
/// is still correct: it keeps the invariant and avoids two slots running at once.
/// It runs after the `/ready` re-probe so the old slot is never torn down until
/// the rolled-back release is confirmed healthy (the same confirm-before-drain
/// ordering cutover uses).
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
    let previous_unit_path = format!("/etc/systemd/system/{previous_unit}.service");
    // Re-render the TARGET release's slot unit from the persisted marker (dir +
    // port), so rollback never depends on the slot's on-disk unit being intact. A
    // redeploy reusing this slot overwrites its unit to point at the new candidate
    // BEFORE the flip; if that redeploy fails pre-flip its teardown removes the
    // candidate dir but leaves the slot unit pointing at the now-removed dir. Left
    // as-is, `restart-previous` below would relaunch that clobbered unit
    // (ExecStart -> removed dir) instead of the retained previous release. Rendering
    // from `target.release_dir`/`target.port` (NOT the current live config) restores
    // the correct unit for the release we roll back to.
    let target_unit = super::render_app_unit(cfg, &target.release_dir, target.port, target.slot);
    // The slot that was live before this rollback — traffic just moved away from
    // it. `target.slot` is the slot we roll back TO, so the former-live slot is the
    // OTHER one.
    let rolled_back_unit = slot_unit_name(&cfg.service_name, other_slot(target.slot));
    // Fallback port for the being-rolled-back-from (former-live) slot, used only if
    // its live-slot marker predates the port field. Reconstruct the public port
    // from `target.port` (= `slot_app_port(public, target.slot)`) and re-derive the
    // other slot's port; in the normal path `commit_markers_command` copies the
    // real port straight from the live-slot marker and ignores this.
    let public_port = target
        .port
        .saturating_sub(if target.slot == SLOT_GREEN { 2 } else { 1 });
    let former_live_fallback_port = slot_app_port(public_port, other_slot(target.slot));
    vec![
        // Re-render the target slot's unit BEFORE bringing it up, so rollback can
        // never restart a slot unit that an earlier failed redeploy clobbered (see
        // the `target_unit` comment above). The unit is rendered from the target's
        // own dir + port, not the current live config.
        DeployOp::WriteFile {
            label: "write-target-unit",
            contents: FileContents::Plain(target_unit),
            remote_path: previous_unit_path,
            mode: Some(0o644),
        },
        // `restart` below loads the on-disk unit, so the freshly written unit must
        // be picked up first — rollback now rewrites a unit, so a `daemon-reload` is
        // required here (first_deploy/cutover reload for the same reason).
        DeployOp::Run(RemoteCommand::new(
            "daemon-reload",
            "systemctl daemon-reload",
        )),
        // enable = boot-persistence; restart = start-or-relaunch. We deliberately
        // use `restart` (not `enable --now`) because the previous slot may still be
        // active with a stale process under drift, and `enable --now` will NOT
        // relaunch an already-active unit — the health-gated flip below would then
        // target a stale binary. `restart` always relaunches the on-disk unit
        // (re-rendered and daemon-reloaded immediately above, so it points at the
        // target release's dir + port).
        DeployOp::Run(RemoteCommand::new(
            "restart-previous",
            format!(
                "systemctl enable {previous_unit}.service && \
                 systemctl restart {previous_unit}.service"
            ),
        )),
        // Health-gated flip back: the previous unit (brought up above) must pass
        // /ready before the proxy swaps traffic to it.
        proxy.flip_op(&cfg.service_name, &loopback_upstream(target.port)),
        // Commit the on-disk state markers as ONE remote transaction after the flip
        // (#1938), symmetric with cutover: a single SSH round-trip that lands or
        // fails as a unit, so a failure between the flip and completing the markers
        // cannot leave the proxy on the rolled-back release while the markers still
        // describe the release we rolled back FROM. Internally it (1) records the
        // release we are rolling back FROM (its dir + the former-live slot,
        // `other_slot(target.slot)`) as the new "previous release" — read from
        // `current` + live-slot BEFORE they change — then (2) repoints `current` to
        // the target release, then (3) records the target's live slot. Each marker
        // file is written via temp-file + `mv` (atomic rename), not a truncating
        // redirect.
        DeployOp::Run(commit_markers_command(
            cfg,
            &target.release_dir,
            target.slot,
            target.port,
            PrevMarkerFallback {
                slot: other_slot(target.slot),
                port: former_live_fallback_port,
            },
        )),
        DeployOp::Run(RemoteCommand::new(
            "readiness-gate",
            readiness_poll_shell(target.port, cfg.readiness_timeout_secs),
        )),
        // Traffic has moved back to the previous slot; disable the slot that was
        // live before the rollback so the invariant "only the live slot runs"
        // holds and two slots never run at once. (The slot-START ops now force a
        // `restart`, so even a still-running slot would be relaunched onto the new
        // release next deploy — but draining here is still correct.) Symmetric
        // with cutover_ops' `drain-old`.
        DeployOp::Run(RemoteCommand::new(
            "drain-rolled-back-slot",
            format!("systemctl disable --now {rolled_back_unit}.service"),
        )),
    ]
}

/// Teardown for an on-demand rollback that fails AT OR BEFORE the health-gated
/// flip (Slice 3). [`rollback_ops`]'s `restart-previous` brought the previous
/// slot's unit back up, but a step at or before `proxy-flip` failed (e.g. the
/// previous release never passes `/ready`), so traffic never moved and the
/// ORIGINAL release is still serving. This disables the slot the rollback just
/// restarted (`{service}-{target.slot}.service`), restoring the invariant "only
/// the live slot runs" and leaving the original release untouched.
///
/// It does NOT remove the target's release dir — that is a real, retained release
/// (a rollback target), not a half-written candidate, so it must survive for a
/// future rollback. This cleanup is PURE: the single `commit-markers` op in
/// [`rollback_ops`] (which records previous-release, repoints `current`, and
/// records live-slot) runs strictly AFTER the flip, so a pre-/at-flip failure has
/// touched no marker. Best-effort by construction
/// (`|| true` so a never-started unit doesn't fail the cleanup) and driven through
/// [`run_teardown`], which swallows executor errors so a flaky cleanup can never
/// mask the real rollback failure.
#[must_use]
pub fn rollback_teardown_ops(cfg: &ResolvedDeployConfig, target: &RollbackTarget) -> Vec<DeployOp> {
    let restarted_unit = slot_unit_name(&cfg.service_name, target.slot);
    vec![DeployOp::Run(RemoteCommand::new(
        "teardown-rollback-slot",
        format!("systemctl disable --now {restarted_unit}.service || true"),
    ))]
}

/// The `host:port` loopback upstream string the proxy routes at.
fn loopback_upstream(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Per-deploy scratch path holding the pre-refresh kamal-proxy unit's content hash,
/// so [`ProxyController::refresh_installed_ops`](super::proxy::ProxyController::refresh_installed_ops)
/// can decide whether the unit ACTUALLY changed on this host (issue #2070). Keyed on
/// the unique `release_id` so two shared-host deploys never race on a fixed scratch
/// path; the restart step removes it.
fn proxy_unit_snapshot_path(release_id: &str) -> String {
    format!("/tmp/autumn-kamal-proxy-unit-{release_id}.sha256")
}

/// Command that records which slot now serves live traffic AND the loopback
/// `port` its unit was rendered with, as `{slot}\t{port}` (read by the next
/// redeploy's [`probe_deploy_state`], and copied into the previous-release marker
/// by [`record_previous_release`] so a later rollback targets the real listener).
///
/// The port is persisted because it is `slot_app_port(current server.port, slot)`
/// computed AT THIS deploy — correct at deploy time even if a later deploy changes
/// `server.port`, which would make re-deriving it from the then-current config
/// wrong. Readers that only need the slot parse the FIRST field and tolerate the
/// older slot-only format.
fn record_live_slot(cfg: &ResolvedDeployConfig, slot: &str, port: u16) -> RemoteCommand {
    RemoteCommand::new(
        "record-live-slot",
        format!(
            "printf '%s\\t%s' {} {} > {}",
            shell_quote(slot),
            port,
            shell_quote(&live_slot_marker(cfg))
        ),
    )
}

/// Fallback slot + loopback port for the previous-release marker, used only when
/// the CURRENT live-slot marker is absent or predates the port field (older
/// slot-only format), so parsing never fails. In the normal path the real slot +
/// port are copied straight from the live-slot marker and these are ignored.
///
/// `slot` is the being-replaced release's live slot and `port`
/// (`= slot_app_port(current server.port, slot)`) its derived loopback port.
#[derive(Clone, Copy)]
struct PrevMarkerFallback<'a> {
    slot: &'a str,
    port: u16,
}

/// Command that commits ALL post-flip on-disk state markers as ONE remote
/// transaction (#1938), shared verbatim by [`cutover_ops`] and [`rollback_ops`]
/// so the two paths can never drift.
///
/// Collapsing what used to be three separate SSH ops (record-previous,
/// link-current, record-live-slot) into a single `&&`-joined shell line removes
/// the multi-round-trip window in which a failure between the flip and completing
/// the markers could leave the proxy serving the new/rolled-back release while the
/// markers still described the old one (drift that then mis-picks the slot on the
/// next deploy). The combined op either lands as a unit or fails as a unit, and it
/// stays positioned strictly AFTER the `proxy-flip` op so the existing "post-flip
/// failure surfaces the raw error and runs NO teardown" fail-safe still holds.
///
/// The shell runs, in this exact order (order is load-bearing — the
/// previous-release marker must read `current` + live-slot BEFORE they change):
///   1. compute `prev` from `readlink current` + the CURRENT live-slot marker
///      (falling back to [`PrevMarkerFallback`] when the marker is absent/older),
///      and — only if `prev` is non-empty — write `{dir}\t{slot}\t{port}` to a
///      unique temp file in the shared dir and `mv` it onto `previous-release`;
///   2. `ln -sfn {release_dir} current` (already atomic);
///   3. write `{slot}\t{port}` to a unique temp file in the shared dir and `mv` it
///      onto `live-slot`.
///
/// Each individual marker file is updated via temp-file + `mv` (an atomic,
/// same-filesystem rename — the temp files are `mktemp`'d in the marker's own
/// `shared/` dir), never a truncating `>` redirect onto the live marker, so a
/// reader never observes a half-written marker. The marker FORMATS are unchanged
/// (`{slot}\t{port}` for live-slot, `{dir}\t{slot}\t{port}` for previous-release),
/// and the "only write previous-release when `prev` is non-empty" guard is
/// preserved.
///
/// `release_dir` is the release `current` is repointed to; `slot`/`port` are the
/// now-live slot and its loopback port (candidate on cutover, target on rollback).
fn commit_markers_command(
    cfg: &ResolvedDeployConfig,
    release_dir: &str,
    slot: &str,
    port: u16,
    prev_fallback: PrevMarkerFallback<'_>,
) -> RemoteCommand {
    let shared = cfg.shared_dir();
    let prev_tmpl = format!("{shared}/previous-release.tmp.XXXXXX");
    let live_tmpl = format!("{shared}/live-slot.tmp.XXXXXX");
    RemoteCommand::new(
        "commit-markers",
        format!(
            "prev=$(readlink {current} 2>/dev/null); \
             live=$(cat {live_marker} 2>/dev/null); \
             lslot=$(printf '%s' \"$live\" | cut -f1); \
             lport=$(printf '%s' \"$live\" | cut -s -f2); \
             [ -n \"$lslot\" ] || lslot={prev_slot}; \
             [ -n \"$lport\" ] || lport={prev_port}; \
             if [ -n \"$prev\" ]; then \
                 ptmp=$(mktemp {prev_tmpl}) && \
                 printf '%s\\t%s\\t%s' \"$prev\" \"$lslot\" \"$lport\" > \"$ptmp\" && \
                 mv -f \"$ptmp\" {prev_marker}; \
             fi && \
             ln -sfn {release_dir} {current} && \
             ltmp=$(mktemp {live_tmpl}) && \
             printf '%s\\t%s' {slot} {port} > \"$ltmp\" && \
             mv -f \"$ltmp\" {live_marker}",
            current = shell_quote(&cfg.current_symlink()),
            live_marker = shell_quote(&live_slot_marker(cfg)),
            prev_slot = shell_quote(prev_fallback.slot),
            prev_port = prev_fallback.port,
            prev_tmpl = shell_quote(&prev_tmpl),
            prev_marker = shell_quote(&previous_release_marker(cfg)),
            release_dir = shell_quote(release_dir),
            live_tmpl = shell_quote(&live_tmpl),
            slot = shell_quote(slot),
            port = port,
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

/// Delimiter appended by the deploy-start probe between the first-vs-redeploy
/// detection and the raw `kamal-proxy list` output, so both are captured in ONE
/// remote round-trip and split apart by the Rust side. Deliberately distinctive
/// so it can never collide with a marker/slot value.
const PROXY_LIST_DELIM: &str = "---autumn-kamal-proxy-list---";

/// The outcome of the deploy-start probe: the first-vs-redeploy [`DeployMode`]
/// PLUS the raw `kamal-proxy list` output captured in the SAME remote round-trip,
/// so a drifted live-slot marker can be reconciled against the live proxy without
/// a second probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployProbe {
    /// First-vs-redeploy decision (parsed exactly as before from the marker).
    pub mode: DeployMode,
    /// Raw `kamal-proxy list` stdout (empty when the proxy could not be listed —
    /// the reconcile then falls back to the marker, fail-safe).
    pub proxy_list: String,
}

/// The full deploy-start probe: first-vs-redeploy mode AND the raw `kamal-proxy
/// list` output, captured in a SINGLE remote round-trip.
///
/// The probe shell keeps the existing `current`/`live-slot` detection byte-for-
/// byte (its meaning is unchanged), then appends a delimited section running
/// `kamal-proxy list` best-effort (`|| true`, stderr suppressed) so a missing
/// binary, dead control socket, or unlisted service can never fail the probe.
/// The two sections are split on [`PROXY_LIST_DELIM`]; when the delimiter is
/// absent (older recorded output / a scripted test) the whole stdout is treated
/// as the mode section and the proxy list is empty — the reconcile then falls
/// back to the marker.
///
/// # Errors
///
/// Returns the executor's error if the probe command cannot run.
pub fn probe_deploy_state(
    cfg: &ResolvedDeployConfig,
    exec: &impl DeployExecutor,
) -> Result<DeployProbe, DeployExecError> {
    let shell = format!(
        "if [ -L {current} ]; then printf 'redeploy:'; cat {marker} 2>/dev/null || printf '{blue}'; \
         else printf 'first'; fi; \
         printf '\\n{delim}\\n'; \
         kamal-proxy list 2>/dev/null || true",
        current = shell_quote(&cfg.current_symlink()),
        marker = shell_quote(&live_slot_marker(cfg)),
        blue = SLOT_BLUE,
        delim = PROXY_LIST_DELIM,
    );
    let out = exec.run(&RemoteCommand::new("detect-current", shell))?;
    let (mode_part, proxy_list) = out
        .stdout
        .split_once(PROXY_LIST_DELIM)
        .unwrap_or((out.stdout.as_str(), ""));
    let mode = mode_part
        .trim()
        .strip_prefix("redeploy:")
        .map_or(DeployMode::First, |marker| DeployMode::Redeploy {
            // The live-slot marker is `{slot}\t{port}` (older markers are slot-only);
            // the slot is the FIRST tab-separated field either way.
            live_slot: canonical_slot(marker.split('\t').next().unwrap_or(SLOT_BLUE)),
        });
    Ok(DeployProbe {
        mode,
        proxy_list: proxy_list.to_owned(),
    })
}

/// Fail closed on reverse-proxy CLI-surface drift BEFORE any cutover (issue #2053).
///
/// The proxy controller ([`KamalProxyController`](super::proxy::KamalProxyController))
/// consumes an UNPINNED kamal-proxy binary from host bootstrap and never checked
/// its CLI surface, so a renamed/removed subcommand or flag on `kamal-proxy deploy`
/// (the route/flip command) would break a real cutover with no warning. This runs
/// the controller's read-only compat probe (a `--help` invocation) and applies its
/// verdict:
///
/// - a controller that declares no probe ([`ProxyController::compat_probe`] →
///   `None`, e.g. a Caddy controller provisioning its own pinned binary) → `Ok(())`
///   (nothing to check);
/// - an installed binary that still supports every subcommand/flag the cutover uses
///   → `Ok(())` and **silent**, so a host that is already fine is untouched;
/// - otherwise → [`DeployExecError::ProxyIncompatible`] with a clear, actionable
///   operator message naming exactly what is missing and the remedy, aborting the
///   deploy before it touches live traffic.
///
/// It runs before the first-vs-redeploy decision, so it gates BOTH deploy paths
/// (the first deploy's `proxy-route` and the redeploy's `proxy-flip` are the same
/// `kamal-proxy deploy` command).
///
/// # Errors
///
/// Returns [`DeployExecError::ProxyIncompatible`] when the installed proxy's CLI
/// surface is incompatible, or the executor's error if the probe cannot run.
pub fn probe_proxy_compat(
    proxy: &impl ProxyController,
    exec: &impl DeployExecutor,
) -> Result<(), DeployExecError> {
    let Some(probe) = proxy.compat_probe() else {
        // No declared probe (e.g. a Caddy controller): nothing to guard.
        return Ok(());
    };
    let out = exec.run(&probe.command)?;
    // The probe folds stderr into stdout via `2>&1`; assess both defensively so a
    // stderr-only capture (a different executor) is still classified correctly.
    let combined = if out.stderr.trim().is_empty() {
        out.stdout
    } else {
        format!("{}\n{}", out.stdout, out.stderr)
    };
    probe
        .assess(&combined)
        .map_err(|message| DeployExecError::ProxyIncompatible { message })
}

/// Map a live loopback port back to its slot using the public port: blue binds
/// `public + 1`, green `public + 2` ([`slot_app_port`]). Any other offset (or a
/// port below the public port) is not a recognized slot port → `None`.
fn slot_for_port(public_port: u16, port: u16) -> Option<&'static str> {
    match port.checked_sub(public_port)? {
        1 => Some(SLOT_BLUE),
        2 => Some(SLOT_GREEN),
        _ => None,
    }
}

/// Parse the loopback port out of a `127.0.0.1:<port>` target field (optionally
/// scheme-prefixed, e.g. `http://127.0.0.1:3001`), else `None`.
///
/// Anchored to the EXACT loopback host [`loopback_upstream`] routes upstreams at,
/// so a stray `host:port` in some other `kamal-proxy list` column (a public host,
/// a timestamp) can never be mistaken for the app target. A non-empty alphanumeric
/// run immediately after the digits (e.g. `127.0.0.1:3001x`) is rejected rather
/// than partially parsed.
fn loopback_port(field: &str) -> Option<u16> {
    const HOST: &str = "127.0.0.1:";
    let start = field.find(HOST)? + HOST.len();
    let rest = &field[start..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    if rest[digits.len()..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    digits.parse().ok()
}

/// The slot `kamal-proxy list` UNAMBIGUOUSLY reports serving `service_name`, or
/// `None` when the signal is absent or unclear (→ caller falls back to the marker).
///
/// A definite slot is returned ONLY when: exactly one output row carries
/// `service_name` as a standalone whitespace field (the header row never does, and
/// a service listed twice is ambiguous → `None`), that row carries exactly one
/// distinct `127.0.0.1:<port>` target, and `port - public_port` maps to a slot
/// (1=blue, 2=green). Every other shape — service absent, no/garbled target, two
/// different target ports, a port mapping to neither slot — yields `None`.
fn proxy_live_slot(
    list_stdout: &str,
    service_name: &str,
    public_port: u16,
) -> Option<&'static str> {
    let service = service_name.trim();
    if service.is_empty() {
        return None;
    }
    let mut rows = list_stdout
        .lines()
        .filter(|line| line.split_whitespace().any(|field| field == service));
    let row = rows.next()?;
    // The service is listed more than once → ambiguous, fall back.
    if rows.next().is_some() {
        return None;
    }
    // Collect the distinct loopback target port(s) named in the row.
    let mut port: Option<u16> = None;
    for field in row.split_whitespace() {
        if let Some(p) = loopback_port(field) {
            match port {
                None => port = Some(p),
                Some(existing) if existing == p => {}
                // Two DIFFERENT loopback ports in one row → ambiguous, fall back.
                Some(_) => return None,
            }
        }
    }
    slot_for_port(public_port, port?)
}

/// The decision from reconciling the live-slot marker against the live proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotReconcile {
    /// Slot to plan the redeploy from — the proxy-authoritative slot on an
    /// unambiguous disagreement, otherwise the marker's slot (unchanged behavior).
    pub live_slot: &'static str,
    /// Whether the on-host live-slot marker should be repaired to `live_slot`.
    /// `true` ONLY on an unambiguous proxy-vs-marker disagreement.
    pub repair: bool,
    /// A loud drift-warning line, present ONLY on genuine disagreement so drift
    /// stays observable; `None` on agreement or any fall-back.
    pub warn: Option<String>,
}

/// Decide the live slot for redeploy planning by reconciling the live-slot marker
/// (`marker_slot`) against `kamal-proxy list` output — a pure, fully testable
/// function (issue #1938 drift elimination).
///
/// The residual drift #1938 addresses: an interrupted post-flip marker write can
/// leave the `live-slot` marker naming the slot the proxy is NOT serving, so the
/// next redeploy would pick the already-live slot and restart it, interrupting the
/// live upstream. kamal-proxy knows the truth (its routed target), so:
///
/// - Proxy UNAMBIGUOUSLY names a slot that DIFFERS from the marker → treat the
///   proxy as authoritative: plan from the proxy slot (so the candidate takes the
///   genuinely-idle slot), flag `repair = true`, and return a loud `warn` naming
///   the disagreement (drift stays observable, not silently papered over).
/// - Proxy agrees, OR the proxy signal is absent/ambiguous/unparseable (list
///   failed, service unlisted or listed twice, target unparseable, port maps to
///   neither slot) → keep the marker's slot EXACTLY as today: no repair, no warn,
///   no planner change. A reconcile never changes a deploy on an unclear signal.
#[must_use]
pub fn reconcile_live_slot(
    marker_slot: &str,
    proxy_list_stdout: &str,
    service_name: &str,
    public_port: u16,
) -> SlotReconcile {
    let marker_slot = canonical_slot(marker_slot);
    match proxy_live_slot(proxy_list_stdout, service_name, public_port) {
        Some(proxy_slot) if proxy_slot != marker_slot => SlotReconcile {
            live_slot: proxy_slot,
            repair: true,
            warn: Some(format!(
                "deploy state drift: live-slot marker says {marker_slot} but \
                 kamal-proxy is serving {proxy_slot}; reconciling from proxy"
            )),
        },
        _ => SlotReconcile {
            live_slot: marker_slot,
            repair: false,
            warn: None,
        },
    }
}

/// Op that repairs a drifted live-slot marker to the proxy-authoritative slot.
///
/// Prepended to the redeploy sequence (before [`commit_markers_command`] reads
/// the marker) so the marker on disk is corrected as an early, atomic deploy op.
/// Reuses [`record_live_slot`]'s single marker-writer; the persisted port is
/// `slot_app_port(public_port, slot)`, which — since the reconcile only fires when
/// the proxy port already mapped to `slot` via `public_port` — equals the port the
/// proxy reported.
#[must_use]
pub fn live_slot_marker_repair_op(
    cfg: &ResolvedDeployConfig,
    slot: &str,
    public_port: u16,
) -> DeployOp {
    let slot = canonical_slot(slot);
    DeployOp::Run(record_live_slot(
        cfg,
        slot,
        slot_app_port(public_port, slot),
    ))
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
    /// On-demand rollback: a pre-/at-flip failure means traffic never moved, so
    /// the restarted slot is disabled again and the original release keeps serving.
    RollbackFailed,
}

impl TeardownKind {
    /// The (secret-free) progress note printed when teardown starts, so each path
    /// describes the cleanup it is actually performing.
    const fn cleanup_note(self) -> &'static str {
        match self {
            Self::PreviousStillServing | Self::NoPreviousRelease => "rolling back the candidate",
            Self::RollbackFailed => "disabling the restarted slot",
        }
    }
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
/// preflight exactly like the deploy entrypoints, with a teardown of the restarted
/// slot on a failure AT OR BEFORE the health-gated flip.
///
/// The previous-release resolution ([`resolve_rollback_target`]) has already run
/// by the time this is called. `restart-previous` brings the previous slot's unit
/// back up, then the health-gated `proxy-flip` swaps traffic to it. A failure at
/// or before that flip (e.g. the previous release never passes `/ready`) means
/// traffic never moved, so `teardown` ([`rollback_teardown_ops`]) disables the
/// slot the rollback just restarted — leaving the ORIGINAL release still serving —
/// and the call fails with [`DeployExecError::RollbackFailed`]. Every marker write
/// runs after the flip, so a pre-/at-flip failure has touched no marker and the
/// cleanup is pure. A failure AFTER the flip is returned verbatim: traffic has
/// already moved and the markers are being committed, so there is no clean
/// pre-flip state to restore here (that residual is #1938's atomicity scope).
///
/// # Errors
///
/// Returns [`DeployExecError::PreflightAborted`] when any preflight check failed,
/// [`DeployExecError::RollbackFailed`] on a pre-/at-flip failure, or the
/// underlying executor error for a post-flip failure.
pub fn execute_rollback(
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
        TeardownKind::RollbackFailed,
        exec,
    )
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
    // Only genuinely blocking failures abort — a deferred check (resolved in the
    // service's own runtime) is non-blocking and never aborts the deploy.
    let failed = checks.iter().filter(|c| c.blocking()).count();
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
                "  \u{2717} {} failed — {}\u{2026}",
                op.label(),
                kind.cleanup_note()
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
                TeardownKind::RollbackFailed => DeployExecError::RollbackFailed {
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
/// The deploy/rollback entrypoints drive their sequences through
/// [`execute_with_teardown`] (which adds boundary-aware teardown on failure); this
/// plain driver is the un-gated form used by the pure-sequence tests, so it is
/// allowed to be unused in the non-test binary build.
///
/// # Errors
///
/// Returns the first [`DeployExecError`] produced by the executor (or by staging
/// a local temp file for a [`DeployOp::WriteFile`]).
#[cfg_attr(not(test), allow(dead_code))]
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
        /// [`probe_deploy_state`]'s first-vs-redeploy probe).
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
        .expect("deploy config resolves")
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
            &sample_manifests(),
            RELEASE_ID,
            &plan,
        )
    }

    /// A representative config-manifest set (base + a prod profile sibling) whose
    /// local paths need not exist — op building never touches the filesystem.
    fn sample_manifests() -> Vec<ManifestUpload> {
        vec![
            ManifestUpload {
                local: PathBuf::from("/local/autumn.toml"),
                remote_basename: "autumn.toml".to_owned(),
            },
            ManifestUpload {
                local: PathBuf::from("/local/autumn-prod.toml"),
                remote_basename: "autumn-prod.toml".to_owned(),
            },
        ]
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
            &sample_manifests(),
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
        assert_eq!(
            proxy_unit,
            "systemctl daemon-reload && systemctl enable --now kamal-proxy.service",
        );
        // The proxy unit is written directly to its FINAL path — no `.new` staging
        // path, so concurrent shared-host deploys can't race on a fixed staging
        // file (the reworked unit is invariant to per-app TLS, so there is nothing
        // to diff/restart to adopt).
        assert!(
            exec.calls().iter().any(|c| matches!(
                c,
                RecordedCall::Upload { remote_path, .. }
                    if remote_path == "/etc/systemd/system/kamal-proxy.service"
            )),
            "the proxy systemd unit is written to its final path"
        );
        let enable = exec.shell_for("enable-now").expect("enable-now ran");
        assert!(
            enable.contains("myapp-blue.service"),
            "the app runs as a slot-scoped unit: {enable}"
        );
        // FIX A: the app start FORCE-relaunches the freshly written unit —
        // `enable` (boot-persistence) + `restart` (start-or-relaunch), never
        // `enable --now`, so an already-active slot left by drift still relaunches
        // onto the new unit rather than serving a stale process.
        assert!(
            enable.contains("systemctl enable myapp-blue.service")
                && enable.contains("systemctl restart myapp-blue.service"),
            "first-deploy start must force a restart, not `enable --now`: {enable}"
        );
        assert!(
            !enable.contains("enable --now"),
            "first-deploy start must not use `enable --now` (won't relaunch an active slot): {enable}"
        );
        // daemon-reload must precede the start so `restart` loads the new unit.
        let dr = labels.iter().position(|&l| l == "daemon-reload");
        let start = labels.iter().position(|&l| l == "enable-now");
        assert!(
            dr.is_some() && start.is_some() && dr < start,
            "daemon-reload must precede the app start so restart loads the new unit"
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
    #[allow(clippy::too_many_lines)]
    fn redeploy_produces_exact_zero_downtime_sequence() {
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");

        // The full ordered run sequence (uploads interleave; asserted separately).
        // The cutover now LEADS with the proxy-unit refresh (issue #2070): the
        // snapshot of the pre-write unit, the idempotent install, then the
        // change-gated restart+re-register — before the candidate is prepared.
        // (`proxy-write-unit` is a WriteFile → uploaded, so excluded from
        // `run_labels`; asserted separately.)
        assert_eq!(
            exec.run_labels(),
            vec![
                "proxy-snapshot-unit",
                "proxy-install",
                "proxy-restart-if-changed",
                "prepare-dirs",
                "daemon-reload",
                "start-candidate",
                "migrate",
                "readiness-gate",
                "proxy-flip",
                "commit-markers",
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
        // The old (blue) release is drained.
        let drain = exec.shell_for("drain-old").expect("drain ran");
        assert!(
            drain.contains("disable --now myapp-blue.service"),
            "{drain}"
        );

        // #1938: the three post-flip markers are now committed by ONE atomic op.
        // It reads the pre-repoint `current` target and the LIVE (blue) slot marker,
        // records them as the previous release, repoints `current` to the new dir,
        // and records the candidate (green) as live — in that internal order, with
        // each marker written via a temp file + `mv` (never a truncating redirect
        // onto the live marker).
        let commit = exec
            .shell_for("commit-markers")
            .expect("commit-markers ran");
        // (1) previous-release: reads current + the live-slot marker (slot AND port,
        // so it persists the replaced release's REAL listener port), falls back to
        // the live (blue) slot, and writes {dir}\t{slot}\t{port}.
        assert!(
            commit.contains("readlink '/srv/autumn/myapp/current'")
                && commit.contains("cut -f1")
                && commit.contains("cut -s -f2")
                && commit.contains("|| lslot='blue'"),
            "commit-markers reads current + the live-slot marker for previous-release: {commit}"
        );
        assert!(
            commit.contains("printf '%s\\t%s\\t%s' \"$prev\" \"$lslot\" \"$lport\""),
            "commit-markers writes the {{dir}}\\t{{slot}}\\t{{port}} previous-release format: {commit}"
        );
        // (2) current is repointed to the new release dir.
        assert!(
            commit.contains("ln -sfn")
                && commit.contains(RELEASE_DIR)
                && commit.contains("'/srv/autumn/myapp/current'"),
            "commit-markers repoints current to the new release: {commit}"
        );
        // (3) live-slot: the candidate (green) becomes live on loopback port 3002,
        // in the exact {slot}\t{port} format.
        assert!(
            commit.contains("printf '%s\\t%s' 'green' 3002"),
            "commit-markers writes the {{slot}}\\t{{port}} live-slot format: {commit}"
        );
        // Atomic writes: each marker is updated via a unique temp file + `mv -f`,
        // and neither live marker is ever the target of a bare `>` truncation.
        assert!(
            commit.contains("$(mktemp '/srv/autumn/myapp/shared/previous-release.tmp.XXXXXX')")
                && commit.contains("mv -f \"$ptmp\" '/srv/autumn/myapp/shared/previous-release'"),
            "previous-release is written via temp-file + mv (atomic): {commit}"
        );
        assert!(
            commit.contains("$(mktemp '/srv/autumn/myapp/shared/live-slot.tmp.XXXXXX')")
                && commit.contains("mv -f \"$ltmp\" '/srv/autumn/myapp/shared/live-slot'"),
            "live-slot is written via temp-file + mv (atomic): {commit}"
        );
        assert!(
            !commit.contains("> '/srv/autumn/myapp/shared/live-slot'")
                && !commit.contains("> '/srv/autumn/myapp/shared/previous-release'"),
            "no bare `>` truncation onto a live marker (temp+mv only): {commit}"
        );
        // Internal order (load-bearing): previous-release write < current repoint <
        // live-slot write — the previous-release marker must be captured from the
        // pre-repoint state before `current`/live-slot change.
        let prev_write = commit
            .find("mv -f \"$ptmp\"")
            .expect("previous-release mv present");
        let link = commit.find("ln -sfn").expect("ln -sfn present");
        let live_write = commit
            .find("mv -f \"$ltmp\"")
            .expect("live-slot mv present");
        assert!(
            prev_write < link && link < live_write,
            "commit-markers order must be previous-release write < ln -sfn < live-slot write: {commit}"
        );

        // Ordering invariants: migrate BEFORE the flip; the flip ONLY after a
        // passing readiness gate.
        let labels = exec.run_labels();
        let pos = |l: &str| labels.iter().position(|&x| x == l).unwrap();
        // FIX A: the candidate start FORCE-relaunches the freshly written unit —
        // `enable` (boot-persistence) + `restart` (start-or-relaunch), never
        // `enable --now`, so an already-active idle slot left by drift relaunches
        // onto the new unit instead of letting the readiness gate probe a stale
        // process.
        let start_candidate = exec
            .shell_for("start-candidate")
            .expect("start-candidate ran");
        assert!(
            start_candidate.contains("systemctl enable myapp-green.service")
                && start_candidate.contains("systemctl restart myapp-green.service"),
            "candidate start must force a restart, not `enable --now`: {start_candidate}"
        );
        assert!(
            !start_candidate.contains("enable --now"),
            "candidate start must not use `enable --now`: {start_candidate}"
        );
        assert!(
            pos("daemon-reload") < pos("start-candidate"),
            "daemon-reload must precede the candidate start so restart loads the new unit"
        );
        assert!(pos("migrate") < pos("proxy-flip"), "migrate before flip");
        assert!(
            pos("readiness-gate") < pos("proxy-flip"),
            "flip only after a passing readiness gate"
        );
        assert!(
            pos("proxy-flip") < pos("commit-markers"),
            "flip before committing the state markers"
        );
        assert!(
            pos("commit-markers") < pos("drain-old"),
            "commit the markers before draining the old release"
        );
        assert!(pos("drain-old") < pos("prune"), "drain before prune");
    }

    #[test]
    fn cutover_refreshes_proxy_unit_and_restarts_only_when_changed() {
        // Issue #2070: the redeploy path must now REFRESH the shared proxy unit so a
        // reboot-durable unit reaches existing hosts on upgrade — and restart the
        // running proxy to adopt it ONLY when the unit actually changed, immediately
        // re-registering the CURRENT live upstream so :80 is routeless for only ~the
        // restart, not the whole cutover.
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));

        // (a) The unit is (re)written to its FINAL path on the redeploy path, exactly
        // like the first deploy — this is what delivers the reboot-durable unit to an
        // already-provisioned host.
        assert!(
            ops.iter().any(|op| matches!(
                op,
                DeployOp::WriteFile { label: "proxy-write-unit", remote_path, contents: FileContents::Plain(unit), .. }
                    if remote_path == "/etc/systemd/system/kamal-proxy.service"
                        && unit.contains("StateDirectory=kamal-proxy\n")
                        && unit.contains("Environment=HOME=/var/lib/kamal-proxy\n")
            )),
            "cutover must rewrite the reboot-durable proxy unit to its final path"
        );

        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        let labels = exec.run_labels();
        let pos = |l: &str| labels.iter().position(|&x| x == l);

        // (b) The proxy refresh LEADS the cutover (before the candidate is prepared)
        // and, crucially, the change-gated restart+re-register runs BEFORE the flip —
        // so on a changed unit the live route is restored up front, keeping :80 served
        // through the whole candidate-start/migrate/readiness window.
        assert!(
            pos("proxy-install").is_some(),
            "cutover installs the proxy unit"
        );
        let restart = pos("proxy-restart-if-changed").expect("restart-if-changed present");
        assert!(
            pos("proxy-snapshot-unit").unwrap() < pos("proxy-install").unwrap()
                && pos("proxy-install").unwrap() < restart,
            "snapshot → install → restart-if-changed, in order: {labels:?}"
        );
        assert!(
            restart < pos("prepare-dirs").unwrap() && restart < pos("proxy-flip").unwrap(),
            "the proxy refresh precedes the candidate work and the flip: {labels:?}"
        );

        // (c) The snapshot captures the CURRENT unit's content hash before the write.
        let snapshot = exec.shell_for("proxy-snapshot-unit").expect("snapshot ran");
        assert!(
            snapshot.contains("sha256sum '/etc/systemd/system/kamal-proxy.service'")
                && snapshot.contains("/tmp/autumn-kamal-proxy-unit-20260714T120000Z.sha256"),
            "snapshot hashes the live unit into a per-release scratch path: {snapshot}"
        );

        // (d) The restart is CHANGE-GATED (unchanged unit → no restart), only fires
        // while the proxy is active, and — only then — re-registers the CURRENT live
        // upstream (blue = 127.0.0.1:3001), socket-pinned like every other CLI call.
        let restart_sh = exec
            .shell_for("proxy-restart-if-changed")
            .expect("restart-if-changed ran");
        assert!(
            restart_sh.contains("if [ \"$new\" = \"$old\" ]; then exit 0; fi"),
            "an UNCHANGED unit must short-circuit before any restart: {restart_sh}"
        );
        assert!(
            restart_sh.contains("systemctl is-active --quiet kamal-proxy.service")
                && restart_sh.contains("systemctl restart kamal-proxy.service"),
            "on a change it restarts the live proxy: {restart_sh}"
        );
        assert!(
            restart_sh.contains(
                "env -u XDG_RUNTIME_DIR kamal-proxy deploy 'myapp' --target '127.0.0.1:3001'"
            ),
            "on a change it re-registers the CURRENT live upstream, socket-pinned: {restart_sh}"
        );
        // The re-register sits AFTER the restart within the one command (so no SSH
        // round-trip interposes), and the whole restart block is downstream of the
        // change gate.
        let gate = restart_sh
            .find("if [ \"$new\" = \"$old\" ]")
            .expect("change gate present");
        let do_restart = restart_sh
            .find("systemctl restart kamal-proxy.service")
            .expect("restart present");
        let reregister = restart_sh
            .find("kamal-proxy deploy 'myapp' --target '127.0.0.1:3001'")
            .expect("re-register present");
        assert!(
            gate < do_restart && do_restart < reregister,
            "order must be change-gate → restart → re-register: {restart_sh}"
        );
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
        assert!(
            !labels.contains(&"commit-markers"),
            "current not repointed (commit-markers never ran)"
        );
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
        assert!(
            !labels.contains(&"commit-markers"),
            "current not repointed (commit-markers never ran)"
        );
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
            // This is the real post-flip marker op (#1938 collapsed the three
            // separate marker ops into one `commit-markers`).
            DeployOp::Run(RemoteCommand::new("commit-markers", "true")),
        ];
        let teardown = vec![
            DeployOp::Run(RemoteCommand::new("teardown-candidate-unit", "true")),
            DeployOp::Run(RemoteCommand::new("teardown-candidate-dir", "true")),
        ];
        let exec = RecordingExecutor::failing_on("commit-markers");
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
                    label: "commit-markers",
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
    #[allow(clippy::too_many_lines)]
    fn deploy_rollback_produces_exact_ordered_sequence() {
        // The previous-release marker names release A on the BLUE slot directly
        // (the marker records the previous release's OWN slot), so rollback flips
        // back to blue (loopback 3001).
        let cfg = resolved();
        // The marker carries dir + slot + the release's PERSISTED port (3-field).
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue\t3001",
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

        // resolve-previous (the probe) precedes the ordered rollback ops: re-render
        // the target unit + daemon-reload → bring the previous slot back up → flip →
        // commit the markers atomically (#1938) → re-probe → drain the former-live
        // slot. (write-target-unit is a WriteFile — uploaded, so excluded from
        // `run_labels`; asserted in `rollback_rerenders_target_unit_before_restart`.)
        assert_eq!(
            exec.run_labels(),
            vec![
                "resolve-previous",
                "daemon-reload",
                "restart-previous",
                "proxy-flip",
                "commit-markers",
                "readiness-gate",
                "drain-rolled-back-slot",
            ],
            "unexpected rollback sequence"
        );

        // #1938: the post-flip markers are committed by ONE atomic op, symmetric
        // with cutover. It records the release rolled back FROM (its dir + the
        // FORMER-live green slot, read from current + the live-slot marker) as the
        // new previous-release, repoints `current` to the target release, then marks
        // the target (blue) live — in that internal order, each via temp-file + mv.
        let commit = exec
            .shell_for("commit-markers")
            .expect("commit-markers ran");
        // (1) previous-release: reads current + the live-slot marker, falls back to
        // the former-live (green) slot, and writes the {dir}\t{slot}\t{port} format.
        assert!(
            commit.contains("readlink '/srv/autumn/myapp/current'")
                && commit.contains("cut -f1")
                && commit.contains("cut -s -f2")
                && commit.contains("|| lslot='green'"),
            "commit-markers reads current + the live-slot marker for previous-release: {commit}"
        );
        assert!(
            commit.contains("printf '%s\\t%s\\t%s' \"$prev\" \"$lslot\" \"$lport\""),
            "commit-markers writes the {{dir}}\\t{{slot}}\\t{{port}} previous-release format: {commit}"
        );
        // (2) current is repointed to the target (previous) release dir.
        assert!(
            commit.contains("ln -sfn")
                && commit.contains("/srv/autumn/myapp/releases/20260713T090000Z")
                && commit.contains("'/srv/autumn/myapp/current'"),
            "commit-markers repoints current to the target release: {commit}"
        );
        // (3) live-slot: the target (blue) becomes live on loopback port 3001, in
        // the exact {slot}\t{port} format.
        assert!(
            commit.contains("printf '%s\\t%s' 'blue' 3001"),
            "commit-markers writes the {{slot}}\\t{{port}} live-slot format: {commit}"
        );
        // Atomic writes: each marker via temp-file + `mv -f`, never a bare `>`
        // truncation onto a live marker.
        assert!(
            commit.contains("$(mktemp '/srv/autumn/myapp/shared/previous-release.tmp.XXXXXX')")
                && commit.contains("mv -f \"$ptmp\" '/srv/autumn/myapp/shared/previous-release'")
                && commit.contains("$(mktemp '/srv/autumn/myapp/shared/live-slot.tmp.XXXXXX')")
                && commit.contains("mv -f \"$ltmp\" '/srv/autumn/myapp/shared/live-slot'"),
            "both markers are written via temp-file + mv (atomic): {commit}"
        );
        assert!(
            !commit.contains("> '/srv/autumn/myapp/shared/live-slot'")
                && !commit.contains("> '/srv/autumn/myapp/shared/previous-release'"),
            "no bare `>` truncation onto a live marker (temp+mv only): {commit}"
        );
        // Internal order (load-bearing): previous-release write < current repoint <
        // live-slot write.
        let prev_write = commit
            .find("mv -f \"$ptmp\"")
            .expect("previous-release mv present");
        let link = commit.find("ln -sfn").expect("ln -sfn present");
        let live_write = commit
            .find("mv -f \"$ltmp\"")
            .expect("live-slot mv present");
        assert!(
            prev_write < link && link < live_write,
            "commit-markers order must be previous-release write < ln -sfn < live-slot write: {commit}"
        );

        // The previous unit is brought up before the flip (the flip is health-
        // gated and would time out against a stopped upstream). FIX A: the start
        // FORCE-relaunches the on-disk unit — `enable` + `restart`, never
        // `enable --now` — so a previous slot left active with a stale process
        // relaunches rather than letting the health-gated flip target stale bits.
        // (Rollback now re-renders the target unit and daemon-reloads first, so the
        // restart loads the correct on-disk unit — see
        // `rollback_rerenders_target_unit_before_restart`.)
        let restart = exec.shell_for("restart-previous").expect("restart ran");
        assert!(
            restart.contains("systemctl enable myapp-blue.service")
                && restart.contains("systemctl restart myapp-blue.service"),
            "rollback force-relaunches the previous (blue) slot unit: {restart}"
        );
        assert!(
            !restart.contains("enable --now"),
            "rollback restart must not use `enable --now`: {restart}"
        );
        // The flip targets the PREVIOUS release's loopback address (blue = 3001).
        let flip = exec.shell_for("proxy-flip").expect("flip ran");
        assert!(
            flip.contains("kamal-proxy deploy") && flip.contains("--target '127.0.0.1:3001'"),
            "flip targets the previous release's address: {flip}"
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
            pos("proxy-flip") < pos("commit-markers"),
            "flip before committing the state markers"
        );
        assert!(
            pos("commit-markers") < pos("readiness-gate"),
            "commit the markers before the re-probe"
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
    fn commit_markers_writes_both_markers_atomically_via_temp_and_mv() {
        // #1938: the collapsed post-flip marker op writes each on-disk marker via a
        // unique temp file + `mv` (atomic same-filesystem rename), never a
        // truncating `>` redirect onto the live marker — so a crash mid-write can
        // never leave a reader observing a half-written marker — and it preserves
        // the exact wire formats the reader (`detect_deploy_mode` / rollback
        // resolution) parses: `{slot}\t{port}` for live-slot and
        // `{dir}\t{slot}\t{port}` for previous-release.
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        let commit = exec
            .shell_for("commit-markers")
            .expect("commit-markers ran");

        // live-slot: temp file in the marker's own shared/ dir, populated, then
        // atomically renamed onto the live marker. The `>` redirect targets ONLY the
        // temp file, and the exact `{slot}\t{port}` format is preserved.
        assert!(
            commit.contains("ltmp=$(mktemp '/srv/autumn/myapp/shared/live-slot.tmp.XXXXXX')"),
            "live-slot temp is mktemp'd in the shared dir (same-fs atomic mv): {commit}"
        );
        assert!(
            commit.contains("printf '%s\\t%s' 'green' 3002 > \"$ltmp\""),
            "live-slot payload ({{slot}}\\t{{port}}) is written to the temp file: {commit}"
        );
        assert!(
            commit.contains("mv -f \"$ltmp\" '/srv/autumn/myapp/shared/live-slot'"),
            "live-slot is committed by an atomic mv of the temp file: {commit}"
        );

        // previous-release: same temp+mv shape, guarded so it only writes when a
        // previous release exists, preserving the `{dir}\t{slot}\t{port}` format.
        assert!(
            commit
                .contains("ptmp=$(mktemp '/srv/autumn/myapp/shared/previous-release.tmp.XXXXXX')"),
            "previous-release temp is mktemp'd in the shared dir: {commit}"
        );
        assert!(
            commit.contains("printf '%s\\t%s\\t%s' \"$prev\" \"$lslot\" \"$lport\" > \"$ptmp\""),
            "previous-release payload ({{dir}}\\t{{slot}}\\t{{port}}) is written to the temp file: {commit}"
        );
        assert!(
            commit.contains("mv -f \"$ptmp\" '/srv/autumn/myapp/shared/previous-release'"),
            "previous-release is committed by an atomic mv of the temp file: {commit}"
        );
        // The "only write previous-release when a prev exists" guard is preserved.
        assert!(
            commit.contains("if [ -n \"$prev\" ]; then"),
            "previous-release write stays guarded on a non-empty prev: {commit}"
        );

        // Neither LIVE marker is ever the target of a bare truncating redirect.
        assert!(
            !commit.contains("> '/srv/autumn/myapp/shared/live-slot'")
                && !commit.contains("> '/srv/autumn/myapp/shared/previous-release'"),
            "markers are updated only via temp+mv, never a bare `>` truncation: {commit}"
        );
    }

    #[test]
    fn rollback_rerenders_target_unit_before_restart() {
        // Codex P2 (exec.rs): a redeploy reusing a slot overwrites that slot's unit
        // to point at the new candidate BEFORE the flip; if it fails pre-flip its
        // teardown removes the candidate dir but leaves the slot unit pointing at
        // the removed dir. Rollback must therefore RE-RENDER the target slot's unit
        // from the persisted marker (its own dir + port) and daemon-reload BEFORE
        // restarting, so it can never relaunch that clobbered unit.
        let cfg = resolved();
        // Target = the previous release (blue slot, persisted port 3001, its OWN
        // release dir), distinct from any current live release/config.
        let target = RollbackTarget {
            release_dir: "/srv/autumn/myapp/releases/20260713T090000Z".to_owned(),
            slot: SLOT_BLUE,
            port: 3001,
        };
        let ops = rollback_ops(&cfg, &proxy(), &target);

        // Locate the write-target-unit WriteFile and inspect its rendered contents.
        let (unit_idx, unit_path, unit_contents) = ops
            .iter()
            .enumerate()
            .find_map(|(i, op)| match op {
                DeployOp::WriteFile {
                    label: "write-target-unit",
                    remote_path,
                    contents,
                    ..
                } => Some((i, remote_path.clone(), contents.as_str().to_owned())),
                _ => None,
            })
            .expect("rollback re-renders the target unit");

        // Written to the TARGET slot's unit path (blue), not the former-live slot.
        assert_eq!(
            unit_path, "/etc/systemd/system/myapp-blue.service",
            "the re-rendered unit is written to the target slot's unit path"
        );
        // The unit points at the TARGET release's dir and persisted port — NOT a
        // current live dir/port — so rollback restores the correct ExecStart.
        assert!(
            unit_contents.contains("ExecStart=/srv/autumn/myapp/releases/20260713T090000Z/myapp"),
            "re-rendered unit ExecStart references the target release dir: {unit_contents}"
        );
        assert!(
            unit_contents.contains("WorkingDirectory=/srv/autumn/myapp/releases/20260713T090000Z"),
            "re-rendered unit WorkingDirectory is the target release dir: {unit_contents}"
        );
        assert!(
            unit_contents.contains("AUTUMN_SERVER__PORT=3001"),
            "re-rendered unit binds the target's persisted port (3001): {unit_contents}"
        );

        // Ordering: write-target-unit → daemon-reload → restart-previous.
        let label_idx = |want: &str| {
            ops.iter()
                .position(|op| op.label() == want)
                .unwrap_or_else(|| panic!("op {want} present"))
        };
        let reload_idx = label_idx("daemon-reload");
        let restart_idx = label_idx("restart-previous");
        assert!(
            unit_idx < reload_idx && reload_idx < restart_idx,
            "order must be write-target-unit ({unit_idx}) < daemon-reload ({reload_idx}) \
             < restart-previous ({restart_idx})"
        );
    }

    #[test]
    fn rollback_reads_target_release_own_manifest_and_uploads_none() {
        // #1952 P1: the manifest is coupled to the release dir, so a rollback that
        // re-renders the target slot's unit from `target.release_dir` automatically
        // sets AUTUMN_MANIFEST_DIR to THAT release dir — the app then loads the
        // rolled-back release's OWN uploaded manifest, not the latest deploy's.
        // rollback_ops takes no manifests and must upload none (the retained
        // release dir still carries the manifest shipped with that binary).
        let cfg = resolved();
        let target = RollbackTarget {
            release_dir: "/srv/autumn/myapp/releases/20260713T090000Z".to_owned(),
            slot: SLOT_BLUE,
            port: 3001,
        };
        let ops = rollback_ops(&cfg, &proxy(), &target);

        let unit_contents = ops
            .iter()
            .find_map(|op| match op {
                DeployOp::WriteFile {
                    label: "write-target-unit",
                    contents,
                    ..
                } => Some(contents.as_str().to_owned()),
                _ => None,
            })
            .expect("rollback re-renders the target unit");
        assert!(
            unit_contents.contains(
                "Environment=AUTUMN_MANIFEST_DIR=/srv/autumn/myapp/releases/20260713T090000Z"
            ),
            "rolled-back unit points AUTUMN_MANIFEST_DIR at the target release dir \
             (its own manifest): {unit_contents}"
        );

        assert!(
            !ops.iter().any(|op| op.label() == "upload-config"),
            "rollback uploads no manifest — the target release dir already carries \
             the manifest it shipped with"
        );
    }

    #[test]
    fn rollback_drains_the_slot_it_flipped_away_from() {
        // Regression (Codex P1): rolling back must disable the slot that was live
        // before the rollback (the slot traffic moved AWAY from), so the invariant
        // "only the live slot runs" holds and two slots never run at once.
        let cfg = resolved();
        // The previous-release marker names the previous release on GREEN (its own
        // slot) → we roll back TO green and must drain the former-live BLUE slot.
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tgreen\t3002",
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

    /// Resolve a rollback target from a scripted `resolve-previous` probe (blue,
    /// loopback 3001) and build its ops + teardown.
    fn sample_rollback(
        exec: &RecordingExecutor,
    ) -> (ResolvedDeployConfig, Vec<DeployOp>, Vec<DeployOp>) {
        let cfg = resolved();
        let target = resolve_rollback_target(&cfg, 3000, exec).expect("previous release resolves");
        let ops = rollback_ops(&cfg, &proxy(), &target);
        let teardown = rollback_teardown_ops(&cfg, &target);
        (cfg, ops, teardown)
    }

    #[test]
    fn rollback_flip_failure_disables_restarted_slot_and_leaves_original() {
        // FIX B: the health-gated flip fails (the previous release never passes
        // /ready). Traffic never moved, so execute_rollback disables the slot it
        // just restarted, leaves the target's release dir intact, writes NO marker,
        // and fails with RollbackFailed — the ORIGINAL release is still serving.
        let exec = RecordingExecutor::failing_on("proxy-flip").with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue\t3001",
        );
        let (_cfg, ops, teardown) = sample_rollback(&exec);
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];

        let err = execute_rollback(&checks, &ops, &teardown, &exec)
            .expect_err("a flip failure must fail the rollback");
        assert!(
            matches!(
                err,
                DeployExecError::RollbackFailed {
                    failed_step: "proxy-flip",
                    ..
                }
            ),
            "expected RollbackFailed at proxy-flip, got {err:?}"
        );

        let labels = exec.run_labels();
        // The restarted slot is disabled again.
        assert!(
            labels.contains(&"teardown-rollback-slot"),
            "the restarted slot must be disabled: {labels:?}"
        );
        let td = exec
            .shell_for("teardown-rollback-slot")
            .expect("teardown-rollback-slot ran");
        assert!(
            td.contains("disable --now myapp-blue.service"),
            "teardown disables the slot the rollback restarted: {td}"
        );
        // The target's release dir is NOT removed (it is a real, retained release,
        // not a half-written candidate) — no `rm -rf` anywhere in the sequence.
        assert!(
            !exec.calls().iter().any(|c| matches!(
                c,
                RecordedCall::Run { shell, .. } if shell.contains("rm -rf")
            )),
            "the target release dir must NOT be removed: {:?}",
            exec.calls()
        );
        // No marker was written — the single commit-markers op in rollback_ops runs
        // AFTER the flip, so a pre-/at-flip failure has touched none of them.
        assert!(
            !labels.contains(&"commit-markers"),
            "no marker may be written on a pre-/at-flip failure: {labels:?}"
        );
        // Nothing past the flip ran either.
        assert!(
            !labels.contains(&"readiness-gate") && !labels.contains(&"drain-rolled-back-slot"),
            "no post-flip op may run after a failed flip: {labels:?}"
        );
    }

    #[test]
    fn rollback_restart_previous_failure_disables_restarted_slot() {
        // FIX B: a failure BEFORE the flip (`restart-previous`) triggers the SAME
        // cleanup — the restarted slot is disabled and the original release is left
        // serving with no marker written and no flip attempted.
        let exec = RecordingExecutor::failing_on("restart-previous").with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue\t3001",
        );
        let (_cfg, ops, teardown) = sample_rollback(&exec);
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];

        let err = execute_rollback(&checks, &ops, &teardown, &exec)
            .expect_err("a restart-previous failure must fail the rollback");
        assert!(
            matches!(
                err,
                DeployExecError::RollbackFailed {
                    failed_step: "restart-previous",
                    ..
                }
            ),
            "expected RollbackFailed at restart-previous, got {err:?}"
        );

        let labels = exec.run_labels();
        let td = exec
            .shell_for("teardown-rollback-slot")
            .expect("teardown-rollback-slot ran");
        assert!(
            td.contains("disable --now myapp-blue.service"),
            "teardown disables the slot the rollback restarted: {td}"
        );
        assert!(
            !labels.contains(&"proxy-flip"),
            "no flip after a restart-previous failure: {labels:?}"
        );
        assert!(
            !labels.contains(&"commit-markers"),
            "no marker may be written: {labels:?}"
        );
        assert!(
            !exec.calls().iter().any(|c| matches!(
                c,
                RecordedCall::Run { shell, .. } if shell.contains("rm -rf")
            )),
            "the target release dir must NOT be removed: {:?}",
            exec.calls()
        );
    }

    #[test]
    fn execute_rollback_happy_path_runs_full_sequence_without_teardown() {
        // A healthy rollback runs the full ordered sequence and NEVER triggers the
        // teardown — the restarted slot stays serving.
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue\t3001",
        );
        let (_cfg, ops, teardown) = sample_rollback(&exec);
        let checks = vec![PreflightCheck::pass("ssh_reachability", "ok")];

        execute_rollback(&checks, &ops, &teardown, &exec).expect("a healthy rollback succeeds");
        let labels = exec.run_labels();
        assert_eq!(
            labels,
            vec![
                "resolve-previous",
                "daemon-reload",
                "restart-previous",
                "proxy-flip",
                "commit-markers",
                "readiness-gate",
                "drain-rolled-back-slot",
            ],
            "happy-path rollback runs the full sequence"
        );
        assert!(
            !labels.contains(&"teardown-rollback-slot"),
            "a healthy rollback must not disable the restarted slot: {labels:?}"
        );
    }

    #[test]
    fn execute_rollback_preflight_failure_aborts_before_any_call() {
        // Preflight gating is preserved by the teardown-aware execute_rollback: a
        // failing check aborts before a single remote call.
        let cfg = resolved();
        let exec = RecordingExecutor::new();
        let target = RollbackTarget {
            release_dir: "/srv/autumn/myapp/releases/20260713T090000Z".to_owned(),
            slot: SLOT_BLUE,
            port: 3001,
        };
        let ops = rollback_ops(&cfg, &proxy(), &target);
        let teardown = rollback_teardown_ops(&cfg, &target);
        let checks = vec![
            PreflightCheck::pass("signing_secret", "ok"),
            PreflightCheck::fail("ssh_reachability", "no target host configured", "set host"),
        ];
        let err = execute_rollback(&checks, &ops, &teardown, &exec)
            .expect_err("failing preflight must abort the rollback");
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
    fn resolve_rollback_target_reads_the_marker_not_the_mtime_newest_dir() {
        // Codex P1: resolution must come from the explicit previous-release MARKER,
        // not an `ls -1dt` mtime scan. The marker names release A on green; a
        // newer-mtime release B also exists on the host, but B must be IGNORED —
        // the probe reads only `shared/previous-release`, so the resolved target is
        // A (its dir + slot + port), exactly as the marker records.
        let cfg = resolved();
        let marker_a = "/srv/autumn/myapp/releases/20260101T000000Z"; // older, but the marker
        let exec = RecordingExecutor::new()
            .with_stdout("resolve-previous", format!("prev:{marker_a}\tgreen\t3002"));
        let target =
            resolve_rollback_target(&cfg, 3000, &exec).expect("marker names a previous release");
        assert_eq!(target.release_dir, marker_a, "dir comes from the marker");
        assert_eq!(
            target.slot, SLOT_GREEN,
            "slot comes from the SAME marker line"
        );
        assert_eq!(
            target.port, 3002,
            "port is read from the SAME marker line, not re-derived"
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
    fn rollback_uses_the_persisted_port_not_the_current_config() {
        // Codex P2: the previous release's app port must come from the PERSISTED
        // marker (the port it was actually rendered with at its deploy), NOT from
        // re-deriving `slot_app_port(current server.port, slot)`. Simulate a
        // server.port change since the previous deploy: the previous blue release
        // bound loopback 3001 (public 3000 back then), but the CURRENT config's
        // public port is 4000, so a re-derivation would wrongly yield 4001.
        let cfg = resolved();
        let current_public = 4000; // server.port changed since the previous deploy
        let derived_now = slot_app_port(current_public, SLOT_BLUE);
        assert_eq!(derived_now, 4001, "re-derivation would give the WRONG port");
        // The marker carries the release's REAL port (3001), recorded at its deploy.
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue\t3001",
        );
        let target = resolve_rollback_target(&cfg, current_public, &exec)
            .expect("previous release resolves");
        assert_eq!(
            target.port, 3001,
            "RollbackTarget.port is the MARKER's port, not the re-derived one"
        );
        assert_ne!(
            target.port, derived_now,
            "the persisted port must win over the current-config derivation"
        );

        // The flip and readiness re-probe both target the persisted port (3001),
        // so the proxy flips to the listener the previous unit actually binds.
        let ops = rollback_ops(&cfg, &proxy(), &target);
        run_ops(&ops, &exec).expect("recording executor never fails");
        let flip = exec.shell_for("proxy-flip").expect("flip ran");
        assert!(
            flip.contains("--target '127.0.0.1:3001'") && !flip.contains("4001"),
            "flip targets the persisted port, not the re-derived one: {flip}"
        );
        let gate = exec.shell_for("readiness-gate").expect("gate ran");
        assert!(
            gate.contains("127.0.0.1:3001/ready") && !gate.contains("4001"),
            "readiness re-probe uses the persisted port: {gate}"
        );
        // The restart force-relaunches the previous release's slot unit (blue).
        let restart = exec.shell_for("restart-previous").expect("restart ran");
        assert!(
            restart.contains("systemctl enable myapp-blue.service")
                && restart.contains("systemctl restart myapp-blue.service"),
            "restart force-relaunches the previous slot unit: {restart}"
        );
    }

    #[test]
    fn resolve_rollback_target_without_port_field_falls_back_to_derived() {
        // Backward-compat: a previous-release marker written before the port field
        // existed (2-field `{dir}\t{slot}`) must still parse — the port falls back
        // to `slot_app_port(current server.port, slot)` rather than crashing.
        let cfg = resolved();
        let exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue",
        );
        let target =
            resolve_rollback_target(&cfg, 3000, &exec).expect("legacy 2-field marker still parses");
        assert_eq!(target.slot, SLOT_BLUE);
        assert_eq!(
            target.port,
            slot_app_port(3000, SLOT_BLUE),
            "a portless marker falls back to the derived port"
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
        assert!(
            !labels.contains(&"commit-markers"),
            "current not repointed (commit-markers never ran)"
        );
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

    // Linux-only: this behavioral test shells out to `sh -c`, GNU `touch -d @epoch`,
    // `ls -1dt` (via the generated prune shell), and `std::os::unix::fs::symlink`.
    // On Windows `std::os::unix` does not exist (compile error), and on macOS the
    // BSD `touch` rejects `-d @epoch` (runtime panic). The deploy target is Ubuntu,
    // so gating to Linux is correct. Use `target_os = "linux"`, not `cfg(unix)`,
    // because macOS is unix but still fails on BSD touch.
    #[cfg(target_os = "linux")]
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
        // The mode parse is unchanged — these assert the exact first-vs-redeploy
        // behavior the marker-only path has always had (now read off the single
        // probe's `.mode`, which also captures the proxy list for the reconcile).
        //
        // No `current` symlink → first deploy.
        let first = RecordingExecutor::new().with_stdout("detect-current", "first");
        assert_eq!(
            probe_deploy_state(&cfg, &first).unwrap().mode,
            DeployMode::First
        );
        // `current` present + marker says green → redeploy onto blue candidate.
        // New-format live-slot marker is `{slot}\t{port}`: the slot is the FIRST
        // field, the trailing port must not leak into the parsed slot.
        let redeploy =
            RecordingExecutor::new().with_stdout("detect-current", "redeploy:green\t3002");
        assert_eq!(
            probe_deploy_state(&cfg, &redeploy).unwrap().mode,
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN
            }
        );
        // Backward-compat: an older slot-only marker (no port field) still parses.
        let legacy = RecordingExecutor::new().with_stdout("detect-current", "redeploy:green");
        assert_eq!(
            probe_deploy_state(&cfg, &legacy).unwrap().mode,
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN
            }
        );
        // A missing/blank marker on a redeploy defaults the live slot to blue.
        let default_blue = RecordingExecutor::new().with_stdout("detect-current", "redeploy:");
        assert_eq!(
            probe_deploy_state(&cfg, &default_blue).unwrap().mode,
            DeployMode::Redeploy {
                live_slot: SLOT_BLUE
            }
        );
    }

    /// A sample `kamal-proxy list` table serving `service` at `127.0.0.1:{port}`.
    /// Mirrors the shape kamal-proxy prints: a header row then one row per service.
    fn proxy_list_serving(service: &str, port: u16) -> String {
        format!(
            "Service   Host          Target            State    TLS\n\
             {service}     example.com   127.0.0.1:{port}   running  no\n"
        )
    }

    #[test]
    fn probe_deploy_state_captures_the_proxy_list_section() {
        let cfg = resolved();
        // Real probe stdout: the redeploy mode line, the delimiter, then the list.
        let stdout = format!(
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n{}",
            proxy_list_serving("myapp", 3001)
        );
        let exec = RecordingExecutor::new().with_stdout("detect-current", stdout);
        let probe = probe_deploy_state(&cfg, &exec).unwrap();
        assert_eq!(
            probe.mode,
            DeployMode::Redeploy {
                live_slot: SLOT_BLUE
            }
        );
        assert!(
            probe.proxy_list.contains("127.0.0.1:3001"),
            "proxy list section captured: {}",
            probe.proxy_list
        );
        // The probe shell folds `kamal-proxy list` into the SAME round-trip and
        // never lets it fail the probe (best-effort `|| true`).
        let shell = exec.shell_for("detect-current").expect("probe ran");
        assert!(
            shell.contains("kamal-proxy list") && shell.contains("|| true"),
            "probe runs kamal-proxy list best-effort: {shell}"
        );
        // No delimiter in the output (older/scripted) → empty list, mode unchanged.
        let legacy = RecordingExecutor::new().with_stdout("detect-current", "redeploy:green\t3002");
        let probe = probe_deploy_state(&cfg, &legacy).unwrap();
        assert_eq!(
            probe.mode,
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN
            }
        );
        assert!(
            probe.proxy_list.is_empty(),
            "no delimiter → empty proxy list"
        );
    }

    /// A compatible `kamal-proxy deploy --help` capture for the compat probe tests.
    fn compatible_deploy_help() -> &'static str {
        "Usage:\n  kamal-proxy deploy SERVICE [flags]\n\nFlags:\n  \
         --target host:port\n  --health-check-path string\n  --host strings\n  \
         --tls\n  --deploy-timeout duration\n  --drain-timeout duration\n"
    }

    #[test]
    fn probe_proxy_compat_passes_silently_on_a_compatible_host() {
        // A host whose kamal-proxy still has every flag the cutover uses passes the
        // probe with no error and no cutover impact (#2053 — do not break fine hosts).
        let exec =
            RecordingExecutor::new().with_stdout("proxy-compat-probe", compatible_deploy_help());
        assert!(probe_proxy_compat(&proxy(), &exec).is_ok());
        // It ran exactly the read-only `deploy --help` probe.
        assert_eq!(exec.run_labels(), vec!["proxy-compat-probe"]);
        let shell = exec.shell_for("proxy-compat-probe").expect("probe ran");
        assert_eq!(shell, "kamal-proxy deploy --help 2>&1 || true");
    }

    #[test]
    fn probe_proxy_compat_fails_closed_on_a_drifted_cli_surface() {
        // A future kamal-proxy that renamed --drain-timeout: the probe fails closed
        // with an actionable message BEFORE any cutover op runs.
        let drifted = compatible_deploy_help().replace("--drain-timeout", "--drain-window");
        let exec = RecordingExecutor::new().with_stdout("proxy-compat-probe", drifted);
        let err = probe_proxy_compat(&proxy(), &exec)
            .expect_err("a drifted CLI surface must fail closed");
        match err {
            DeployExecError::ProxyIncompatible { message } => {
                assert!(
                    message.contains("--drain-timeout"),
                    "names the flag: {message}"
                );
                assert!(message.contains("v0.9.2"), "names the pin: {message}");
                assert!(
                    message.contains("before any cutover"),
                    "states nothing was cut over: {message}",
                );
            }
            other => panic!("expected ProxyIncompatible, got {other:?}"),
        }
    }

    #[test]
    fn probe_proxy_compat_fails_closed_on_a_missing_binary() {
        // Empty output (a missing/unusable binary) fails closed rather than being
        // misread as compatible.
        let exec = RecordingExecutor::new(); // no scripted stdout → empty capture
        let err =
            probe_proxy_compat(&proxy(), &exec).expect_err("an unusable binary must fail closed");
        assert!(matches!(err, DeployExecError::ProxyIncompatible { .. }));
    }

    #[test]
    fn probe_proxy_compat_is_a_noop_when_the_controller_declares_no_probe() {
        // A controller that declares no compat probe (e.g. a Caddy controller that
        // provisions its own pinned binary) is never gated — and never even runs a
        // remote command.
        struct NoProbeController;
        impl ProxyController for NoProbeController {
            fn ensure_installed_ops(&self, _public_port: u16) -> Vec<DeployOp> {
                Vec::new()
            }
            fn route_op(&self, _service: &str, _upstream: &str) -> DeployOp {
                DeployOp::Run(RemoteCommand::new("noop", "true"))
            }
            fn flip_op(&self, _service: &str, _new_upstream: &str) -> DeployOp {
                DeployOp::Run(RemoteCommand::new("noop", "true"))
            }
            // compat_probe() uses the trait default → None.
        }
        let exec = RecordingExecutor::new();
        assert!(probe_proxy_compat(&NoProbeController, &exec).is_ok());
        assert!(
            exec.run_labels().is_empty(),
            "no probe declared → no remote command runs"
        );
    }

    #[test]
    fn reconcile_uses_proxy_slot_and_warns_on_disagreement() {
        // Marker says green, but the proxy is serving blue (127.0.0.1:3001 with
        // public 3000 → blue). The proxy is authoritative: plan from blue, repair
        // the marker, and warn loudly.
        let list = proxy_list_serving("myapp", 3001);
        let decision = reconcile_live_slot(SLOT_GREEN, &list, "myapp", 3000);
        assert_eq!(decision.live_slot, SLOT_BLUE);
        assert!(decision.repair, "a genuine disagreement repairs the marker");
        let warn = decision.warn.expect("disagreement warns");
        assert!(
            warn.contains("drift") && warn.contains(SLOT_GREEN) && warn.contains(SLOT_BLUE),
            "warn names the disagreement: {warn}"
        );
        // Planning from the reconciled slot puts the candidate on the genuinely
        // idle slot (green) — NOT the already-live blue slot.
        let plan = SlotPlan::redeploy(3000, decision.live_slot);
        assert_eq!(plan.live_slot, SLOT_BLUE);
        assert_eq!(plan.candidate_slot, SLOT_GREEN);
        assert_eq!(plan.candidate_port, 3002);

        // The repair op reuses the atomic marker writer and records the blue slot.
        let cfg = resolved();
        let op = live_slot_marker_repair_op(&cfg, decision.live_slot, 3000);
        match op {
            DeployOp::Run(cmd) => {
                assert_eq!(cmd.label, "record-live-slot");
                assert!(
                    cmd.shell.contains(SLOT_BLUE) && cmd.shell.contains("3001"),
                    "repair op writes the proxy slot+port: {}",
                    cmd.shell
                );
            }
            other => panic!("repair should be a Run op, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_agreement_keeps_marker_without_warn_or_repair() {
        // Proxy serves green (127.0.0.1:3002, public 3000) and the marker agrees.
        let list = proxy_list_serving("myapp", 3002);
        let decision = reconcile_live_slot(SLOT_GREEN, &list, "myapp", 3000);
        assert_eq!(decision.live_slot, SLOT_GREEN);
        assert!(!decision.repair, "agreement never repairs");
        assert!(decision.warn.is_none(), "agreement never warns");
    }

    #[test]
    fn reconcile_falls_back_to_marker_on_absent_or_unclear_proxy_signal() {
        // Every one of these must behave EXACTLY as the marker-only path: keep the
        // marker slot, no repair, no warn.
        let cases: &[(&str, &str)] = &[
            ("empty output", ""),
            (
                "service absent from the list",
                "Service   Target\nother   127.0.0.1:3001\n",
            ),
            (
                "target has no loopback port (unparseable)",
                "Service   Target\nmyapp   example.com:3001\n",
            ),
            (
                "loopback port maps to neither slot (public+3)",
                "Service   Target\nmyapp   127.0.0.1:3003\n",
            ),
            (
                "trailing junk after the port is not partially parsed",
                "Service   Target\nmyapp   127.0.0.1:3001x\n",
            ),
        ];
        for (name, list) in cases {
            let decision = reconcile_live_slot(SLOT_BLUE, list, "myapp", 3000);
            assert_eq!(
                decision.live_slot, SLOT_BLUE,
                "fall back keeps marker: {name}"
            );
            assert!(!decision.repair, "no repair on unclear signal: {name}");
            assert!(decision.warn.is_none(), "no warn on unclear signal: {name}");
        }
    }

    #[test]
    fn reconcile_falls_back_when_the_service_is_ambiguous() {
        // Service listed twice (with conflicting targets) → ambiguous → fall back
        // to the marker, no repair, no warn.
        let list = "Service   Target\n\
                    myapp   127.0.0.1:3001\n\
                    myapp   127.0.0.1:3002\n";
        let decision = reconcile_live_slot(SLOT_GREEN, list, "myapp", 3000);
        assert_eq!(decision.live_slot, SLOT_GREEN, "ambiguous → keep marker");
        assert!(!decision.repair);
        assert!(decision.warn.is_none());

        // Two DIFFERENT loopback ports in a SINGLE row is also ambiguous.
        let two_ports = "Service   Target             Prev\n\
                         myapp   127.0.0.1:3001   127.0.0.1:3002\n";
        let decision = reconcile_live_slot(SLOT_GREEN, two_ports, "myapp", 3000);
        assert_eq!(decision.live_slot, SLOT_GREEN);
        assert!(!decision.repair);
        assert!(decision.warn.is_none());
    }

    #[test]
    fn reconcile_scopes_strictly_to_the_target_service_name() {
        // A different service serving blue must NOT reconcile our (green) marker —
        // the row is matched by exact service field, and a substring service name
        // ("myapp" vs "myapp-staging") is not the same field.
        let list = "Service          Target\n\
                    myapp-staging   127.0.0.1:3001\n";
        let decision = reconcile_live_slot(SLOT_GREEN, list, "myapp", 3000);
        assert_eq!(
            decision.live_slot, SLOT_GREEN,
            "substring service not matched"
        );
        assert!(!decision.repair);
        assert!(decision.warn.is_none());
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

    /// #1952: the project config manifest is uploaded into the per-release dir
    /// at 0600 on a FIRST deploy (coupled to the binary), so the app loads the
    /// intended config instead of silent built-in defaults and a rollback reads
    /// the rolled-back release's own manifest.
    #[test]
    fn first_deploy_uploads_config_manifest_to_release_at_0600() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let base_path = format!("{RELEASE_DIR}/autumn.toml");
        let base = ops
            .iter()
            .find(|op| {
                matches!(op, DeployOp::UploadFile { label: "upload-config", remote_path, .. }
                    if *remote_path == base_path)
            })
            .expect("base autumn.toml is uploaded to the release dir");
        match base {
            DeployOp::UploadFile { mode, .. } => {
                assert_eq!(
                    *mode,
                    Some(0o600),
                    "config manifest must be owner-only 0600"
                );
            }
            other => panic!("upload-config should be an UploadFile op, got {other:?}"),
        }
        // The prod profile sibling is uploaded alongside it, into the release dir.
        let sibling_path = format!("{RELEASE_DIR}/autumn-prod.toml");
        assert!(
            ops.iter().any(|op| matches!(
                op,
                DeployOp::UploadFile { label: "upload-config", remote_path, mode: Some(0o600), .. }
                    if *remote_path == sibling_path
            )),
            "profile sibling autumn-prod.toml is uploaded to the release dir"
        );
        // The manifest lands before the unit is written / daemon-reloaded.
        let cfg_idx = ops
            .iter()
            .position(|op| op.label() == "upload-config")
            .expect("upload-config present");
        let reload_idx = ops
            .iter()
            .position(|op| op.label() == "daemon-reload")
            .expect("daemon-reload present");
        assert!(
            cfg_idx < reload_idx,
            "config is uploaded before daemon-reload"
        );
    }

    /// #1952: the manifest is re-uploaded on every redeploy into the per-release
    /// dir so the server config always matches the shipped binary.
    #[test]
    fn cutover_uploads_config_manifest_to_release_at_0600() {
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let base_path = format!("{RELEASE_DIR}/autumn.toml");
        assert!(
            ops.iter().any(|op| matches!(
                op,
                DeployOp::UploadFile { label: "upload-config", remote_path, mode: Some(0o600), .. }
                    if *remote_path == base_path
            )),
            "redeploy re-uploads autumn.toml to the release dir at 0600"
        );
    }

    /// #1952: with no manifests to ship, no upload-config op is emitted (the loud
    /// operator warning is handled at the call site, not in the op list).
    #[test]
    fn no_manifest_means_no_config_upload_op() {
        let cfg = resolved();
        let plan = SlotPlan::first(3000);
        let unit = super::super::render_app_unit(
            &cfg,
            RELEASE_DIR,
            plan.candidate_port,
            plan.candidate_slot,
        );
        let ops = first_deploy_ops(
            &cfg,
            &proxy(),
            &unit,
            Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"),
            Path::new("/local/target/release/myapp"),
            &[],
            RELEASE_ID,
            &plan,
        );
        assert!(
            !ops.iter().any(|op| op.label() == "upload-config"),
            "no manifest → no upload-config op"
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
        // 2 proxy ops + 10 first-deploy ops (incl. clear-previous) + 2 config-manifest
        // uploads (sample_manifests: autumn.toml + autumn-prod.toml, #1952) + 1 proxy
        // route = 15.
        assert_eq!(exec.calls().len(), 15, "the full op sequence should run");
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
