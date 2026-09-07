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
//! - A **Caddy** [`ProxyController`] — kamal-proxy
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

use super::proxy::{ProxyCompatFailure, ProxyController};
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
    /// A step failed strictly AFTER the go-live boundary: traffic had already
    /// moved, so nothing was torn down and the candidate is live. The wrapper
    /// exists for ONE reason — to name the op that was actually running (issue
    /// #1621, §4.6).
    ///
    /// Several executor errors carry no label of their own ([`Self::Spawn`],
    /// [`Self::UploadFailed`], [`Self::Stage`]), and post-boundary the fleet's
    /// never-auto-roll-back guard is keyed on the op label: a dropped transport
    /// during `commit-markers` must fail closed exactly like a non-zero exit from
    /// it, because either way the marker triple may be mid-transaction and the
    /// rollback target unprovable.
    ///
    /// `Display` delegates to the wrapped error verbatim, so every operator-facing
    /// message — the single-host path's included — is byte-for-byte what it was
    /// before this wrapper existed. Only the fleet classifier looks inside.
    #[error("{source}")]
    PostCutover {
        /// Label of the op that was running when the failure landed.
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
///
/// **The three liveness options are load-bearing for fleets (issue #1621, R2).**
/// [`SshExecutor::run`] shells out with `Command::output()`, which has no timeout,
/// and the deploy preflight is a bare TCP connect — it proves a host accepts a
/// connection, not that its SSH daemon will ever answer. A host that accepts TCP
/// and then hangs (a wedged sshd, a black-holing firewall, a box mid-freeze) would
/// therefore block the deploy **forever**. For one server that is a stuck deploy
/// someone Ctrl-Cs. For a fleet it is a rollout frozen mid-flight with *k* hosts
/// already on the new release and the rest on the old one — the mixed fleet the
/// whole rollout design exists to prevent, entered by way of a hang rather than a
/// failure, and with no error for the driver to compensate. `ConnectTimeout` bounds
/// the handshake; `ServerAliveInterval`/`ServerAliveCountMax` bound a session that
/// goes silent AFTER connecting (60s of silence ends it), which is the shape a long
/// `migrate` or binary upload actually fails in. Turning an infinite hang into a
/// finite error is what lets the fleet driver halt and compensate.
///
/// `scp` forwards `-o` to its own ssh transport, so uploads — the longest single
/// operation in a deploy — get the same bounds.
const SSH_BATCH_OPTS: [&str; 10] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
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

/// Remote marker file recording the proxy `ServiceOptions` (TLS on/off + host) the
/// LAST forward deploy registered with kamal-proxy, stored as `{tls}\t{host}`
/// (`1\tapp.example.com` for TLS-on, `0\t` for TLS-off/removed) — so the next
/// redeploy's durability-refresh re-register can PRESERVE the old release's own
/// TLS/host instead of stamping the new config's onto the still-live old release
/// (issue #2074). kamal-proxy exposes no `ServiceOptions` read-back, so this
/// host-side marker is the only way to recover what was actually registered.
/// Mirrors [`live_slot_marker`]'s path convention (both live under `shared/`).
fn proxy_options_marker(cfg: &ResolvedDeployConfig) -> String {
    format!("{}/shared/proxy-options", cfg.app_dir)
}

/// Remote marker file recording the LAST state-changing deploy action this host
/// completed, stored as `{result}\t{utc-timestamp}` (issue #1621, AC-6).
///
/// AC-6 asks `deploy status` for the per-host *last deploy result*, and no other
/// on-host artefact answers it: `current`, `live-slot` and `previous-release` all
/// describe the release a host is serving, never how it got there. After a halted
/// rollout that compensated cleanly, every host reads back as healthy and
/// converged — which is exactly the state the operator wants distinguished from a
/// fleet that simply deployed.
///
/// **What it knows, precisely.** It is written by the ops that COMPLETE a cutover
/// ([`commit_markers_command`] and [`record_proxy_options`]), so it records the
/// last action that actually moved this host: [`LAST_DEPLOY_DEPLOYED`] or
/// [`LAST_DEPLOY_ROLLED_BACK`]. A deploy that fails BEFORE the cutover boundary
/// never reaches those ops (the host keeps serving its old release and is torn
/// down), so the marker still names the previous action — it is the host's last
/// completed action, not a verdict on the last rollout. `deploy status` says so
/// where it prints it.
///
/// Mirrors [`live_slot_marker`]'s path convention (all markers live under
/// `shared/`), so it survives cutovers and is pruned with nothing.
fn last_deploy_marker(cfg: &ResolvedDeployConfig) -> String {
    format!("{}/shared/last-deploy", cfg.app_dir)
}

/// Marker word for a host that last completed a forward deploy (first deploy or
/// redeploy cutover). See [`last_deploy_marker`].
pub const LAST_DEPLOY_DEPLOYED: &str = "deployed";

/// Marker word for a host whose last completed action was a rollback — an
/// operator-invoked `autumn deploy rollback`, or the fleet driver compensating a
/// halted rollout (AC-3). See [`last_deploy_marker`].
pub const LAST_DEPLOY_ROLLED_BACK: &str = "rolled back";

/// Marker word for a host whose install was REMOVED again: a first deploy that was
/// torn down, either by its own pre-boundary failure or by the fleet driver
/// compensating a halted rollout (`CompensatedTeardown`). See
/// [`first_deploy_teardown_ops`] for why a teardown REWRITES this marker instead of
/// deleting it. See [`last_deploy_marker`].
pub const LAST_DEPLOY_TORN_DOWN: &str = "torn down";

/// Shell fragment that records `result` into the [`last_deploy_marker`], written
/// atomically (mktemp + `mv -f`) like every other marker.
///
/// **Advisory by construction.** The fragment is a `{ … || true; }` group, so a
/// failure to write it can never fail the op it is appended to. That is
/// deliberate: it rides on `commit-markers` — whose failure is the one the fleet
/// driver refuses to auto-roll-back from, because a partially-applied marker
/// triple makes the rollback target unprovable — and a cosmetic status field must
/// not be able to push a host into that state.
fn record_last_deploy_fragment(cfg: &ResolvedDeployConfig, result: &str) -> String {
    let shared = cfg.shared_dir();
    let tmpl = format!("{shared}/last-deploy.tmp.XXXXXX");
    format!(
        "{{ rtmp=$(mktemp {tmpl}) && printf '%s\\t%s' {result} \
         \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" > \"$rtmp\" && mv -f \"$rtmp\" {marker} || true; }}",
        tmpl = shell_quote(&tmpl),
        result = shell_quote(result),
        marker = shell_quote(&last_deploy_marker(cfg)),
    )
}

/// The last completed deploy action a host reports, parsed from the
/// [`last_deploy_marker`] (issue #1621, AC-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastDeploy {
    /// The recorded action word — [`LAST_DEPLOY_DEPLOYED`] or
    /// [`LAST_DEPLOY_ROLLED_BACK`]. Kept as the raw string so a marker written by
    /// a newer CLI is reported verbatim rather than degrading to "unknown".
    pub result: String,
    /// When it was recorded, as the host's UTC `date -u +%Y-%m-%dT%H:%M:%SZ`, or
    /// `None` for a marker written before the timestamp field existed.
    pub at: Option<String>,
}

/// Parse a [`last_deploy_marker`] body (`{result}\t{timestamp}`).
///
/// Degrades to `None` for an empty/whitespace-only body — a missing marker (a host
/// that has never completed a cutover) and an unreadable one are the same "we
/// cannot tell", never a fabricated result.
fn parse_last_deploy(raw: &str) -> Option<LastDeploy> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (result, at) = raw.split_once('\t').unwrap_or((raw, ""));
    let result = result.trim();
    if result.is_empty() {
        return None;
    }
    Some(LastDeploy {
        result: result.to_owned(),
        at: Some(at.trim())
            .filter(|at| !at.is_empty())
            .map(ToOwned::to_owned),
    })
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

/// Whether a cutover runs the pending-migration one-shot (issue #1621, AC-4).
///
/// A fleet's schema is **fleet-wide**, so a rollout migrates exactly once. A naive
/// per-host loop would run the `AUTUMN_MIGRATE=1` one-shot on every host: the
/// Postgres session advisory lock (`autumn/src/migrate.rs`) keeps that *correct*,
/// but hosts 2..N each pay the lock wait and, far worse, a migration failing on
/// host 2 **after** host 1 has already cut over is precisely the mixed-version
/// fleet #1621 forbids. So the fleet driver schedules the migration on ONE host
/// and builds every other host's cutover with [`Self::Skip`].
///
/// The op stays **inside** [`cutover_ops`] rather than being hoisted into a fleet
/// pre-phase: in its historical position (between `start-candidate` and
/// `readiness-gate`) it sits PRE-boundary relative to `proxy-flip`, so a failed
/// migration keeps the entire existing, already-tested auto-rollback path for free
/// — [`execute_with_teardown`] runs `candidate_teardown_ops`, the old release keeps
/// serving, and the caller gets `CandidateRolledBack { failed_step: "migrate" }`.
/// A standalone pre-phase would have to re-implement candidate teardown from
/// scratch for zero gain.
///
/// This is an **enum, not a `bool`**, deliberately: Phase 2 of #1621 (role-based
/// rollout — a designated migrator host, worker roles) needs to express more than
/// yes/no and must not force a second signature break on [`cutover_ops`], the
/// most exact-vector-asserted builder in the deploy path.
///
/// [`first_deploy_ops`] takes the SAME parameter (issue #1607, AC-3): a first
/// deploy migrates too — its pending migrations run before the initial release
/// ever starts — so a fleet still runs exactly ONE migration whatever mix of
/// first deploys and redeploys it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateStep {
    /// Run the pending-migration one-shot before the flip — today's behavior, in
    /// today's exact position.
    Run,
    /// Omit ONLY the migrate op; every other step keeps its identity and relative
    /// position (the cutover boundary in particular). Used for every host in a
    /// fleet run except the one that carries the migration.
    Skip,
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
/// 7. `systemctl daemon-reload` (loads the unit; starts nothing),
/// 8. run pending migrations (`AUTUMN_MIGRATE=1` one-shot) — [`MigrateStep::Run`]
///    for a single-host first deploy and for the fleet host carrying the
///    migration, [`MigrateStep::Skip`] for the rest,
/// 9. `systemctl enable {service}-blue.service && systemctl restart
///    {service}-blue.service` (restart, not `enable --now`, so an already-active
///    slot always relaunches the freshly written unit — see the op's comment),
/// 10. record the live slot marker,
/// 11. clear the previous-release marker (a first deploy has no previous),
/// 12. bounded `/ready` poll on the blue loopback port,
/// 13. route the proxy at `127.0.0.1:{blue_port}`.
///
/// # Why the migration sits EARLIER here than in [`cutover_ops`]
///
/// The redeploy path starts the candidate first and migrates while the old
/// release keeps serving. A first deploy has no old release to protect and no
/// running process to keep warm, so the migration runs BEFORE the unit is started:
/// an app booted against a schema that was never applied can crash-loop under
/// systemd's restart policy long before the readiness gate reports anything
/// useful. Both positions are pre-cutover, which is what AC-3 requires — nothing
/// takes traffic until the migration has succeeded.
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
    migrate: MigrateStep,
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
    // Keep the SQLite data file out of the release dir (#1909). Immediately after
    // `prepare-dirs` and before the migrate one-shot, so the migration and the app
    // open the same file.
    ops.extend(sqlite_data_link_op(cfg, &release_dir).map(DeployOp::Run));
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
    ]);
    // Pending migrations run before the initial release is even started (#1607, AC-3).
    // `systemd-run --wait` returns the child's exit status, so a failed migration
    // surfaces a non-zero error that stops `run_ops` here: the slot never starts, the
    // proxy is never routed at it, and the caller's first-deploy teardown removes the
    // half-written release.
    //
    // It sits after `daemon-reload`, which only reloads unit files and starts nothing,
    // rather than before it, so `daemon-reload` is unambiguously a pre-migrate step on
    // both builders — see [`PRE_MIGRATE_LABELS`], which the fleet summary uses to tell
    // "died before migrating" from "the schema moved". `MigrateStep::Skip` omits only
    // this op, since a fleet migrates exactly once; see [`MigrateStep`].
    if matches!(migrate, MigrateStep::Run) {
        ops.push(DeployOp::Run(release_migrate_command(cfg, &release_dir)));
    }
    ops.extend([
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
        // Record the proxy TLS/host options this deploy registers (issue #2074), so
        // the NEXT redeploy's durability-refresh re-register can preserve THIS
        // release's own options across the one-time reboot-durability restart.
        DeployOp::Run(record_proxy_options(cfg, &proxy.proxy_service_options())),
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
/// 7. run pending migrations BEFORE cutover (`AUTUMN_MIGRATE=1` one-shot) — the
///    ONLY step this builder parameterises: `migrate` is [`MigrateStep::Run`] for
///    a single-host deploy and for the one fleet host that carries the migration,
///    and [`MigrateStep::Skip`] for every other fleet host (issue #1621, AC-4).
///    `Skip` omits this op and nothing else,
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
    reregister_options: &ProxyServiceOptions,
    migrate: MigrateStep,
) -> Vec<DeployOp> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    let remote_binary = format!("{release_dir}/{}", cfg.app_name);
    let shared_dir = cfg.shared_dir();
    let env_path = cfg.env_file();
    let current = cfg.current_symlink();
    let candidate_unit = slot_unit_name(&cfg.service_name, plan.candidate_slot);
    let candidate_unit_path = format!("/etc/systemd/system/{candidate_unit}.service");
    let live_unit = slot_unit_name(&cfg.service_name, plan.live_slot);

    // Refresh the shared proxy unit on the redeploy path too (#2070). Previously only
    // `first_deploy_ops` wrote the proxy unit, so a fix landed in the unit — the
    // reboot-durable `StateDirectory`/`HOME` of #2069, say — never reached an
    // already-provisioned host on upgrade. Prepending the idempotent install, mirroring
    // how `first_deploy_ops` starts, rewrites it on every redeploy and, for kamal-proxy
    // only, restarts and re-registers the live upstream when the unit actually changed,
    // so an existing host adopts the new unit with a routeless window bounded to about
    // the restart (see `ProxyController::refresh_installed_ops`). Writing the unit is
    // deterministic, lands at the final path, and causes no restart on its own, so it is
    // safe at the very start of the cutover.
    //
    // The live upstream re-registered on a change is the release serving right now,
    // targeted at the derived live-slot port (`plan.live_port`). That derived port is
    // correct here because the redeploy path refuses a concurrent `server.port` change at
    // pre-flight (#2073, `refuse_concurrent_public_port_change`), so the public port is
    // unchanged and the derived live port necessarily equals the port the live release
    // binds; a live-safe port change is future work. The candidate and flip below use the
    // new derived candidate port, which the new release genuinely binds.
    //
    // The re-register carries `reregister_options` — the old release's own TLS and host,
    // recovered from the `shared/proxy-options` marker (#2074) — not the new config's, so
    // a later-op failure and rollback leaves the still-live old release on its own host
    // and TLS. The candidate flip below still uses the controller's new `tls_host`.
    let mut ops = proxy.refresh_installed_ops(
        plan.public_port,
        &cfg.service_name,
        &loopback_upstream(plan.live_port),
        &proxy_unit_snapshot_path(release_id),
        reregister_options,
    );
    ops.push(DeployOp::Run(RemoteCommand::new(
        "prepare-dirs",
        format!(
            "mkdir -p {} {}",
            shell_quote(&release_dir),
            shell_quote(&shared_dir)
        ),
    )));
    // Keep the SQLite data file out of the release dir (#1909) — same position as
    // on the first-deploy path, and still before the migrate one-shot.
    ops.extend(sqlite_data_link_op(cfg, &release_dir).map(DeployOp::Run));
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
    ]);
    // Migrations run before the flip. `systemd-run --wait` returns the child's exit
    // status, so a failed migration surfaces a non-zero error that stops `run_ops` before
    // the flip, leaving the old release serving (AC-3). #1621: this is the one op a fleet
    // parameterises. Its position is unchanged — between `start-candidate` and
    // `readiness-gate`, so pre-boundary, and the existing candidate-teardown path still
    // covers a failed migration — and `MigrateStep::Skip` omits only this op, for hosts
    // 2..N of a fleet whose shared schema the first redeploying host already migrated.
    // See [`MigrateStep`].
    if matches!(migrate, MigrateStep::Run) {
        ops.push(DeployOp::Run(release_migrate_command(cfg, &release_dir)));
    }
    ops.extend([
        DeployOp::Run(RemoteCommand::new(
            "readiness-gate",
            readiness_poll_shell(plan.candidate_port, cfg.readiness_timeout_secs),
        )),
        // THE cutover: the proxy health-checks the candidate then atomically swaps
        // live traffic to it and drains the old target. Only reached after a
        // passing readiness gate (AC-2).
        proxy.flip_op(&cfg.service_name, &loopback_upstream(plan.candidate_port)),
        // Commit the on-disk state markers as one remote transaction after the flip
        // (#1938): a single SSH round-trip that lands as a unit or fails as a unit, so a
        // failure between the flip and completing the markers can no longer leave the
        // proxy on the new release while the markers describe the old one. It records the
        // release being replaced — its dir and live slot — as the new "previous release",
        // read from `current` and the live slot before they change; then repoints
        // `current` to the new release; then records the new live slot. Each marker file
        // is written via temp file plus `mv`, an atomic rename, not a truncating redirect.
        DeployOp::Run(commit_markers_command(
            cfg,
            &release_dir,
            plan.candidate_slot,
            plan.candidate_port,
            PrevMarkerFallback {
                slot: plan.live_slot,
                port: plan.live_port,
            },
            LAST_DEPLOY_DEPLOYED,
        )),
        // Record the proxy TLS/host options this cutover registered on the new
        // release (issue #2074) — the controller's NEW `tls_host`, NOT the preserved
        // `reregister_options` — so the NEXT redeploy preserves the options THIS
        // release actually serves behind. Written after the marker commit (the flip
        // has landed), atomically via mktemp + `mv`.
        DeployOp::Run(record_proxy_options(cfg, &proxy.proxy_service_options())),
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
///
/// It deliberately does NOT touch the [`last_deploy_marker`], unlike
/// [`first_deploy_teardown_ops`]. A torn-down CANDIDATE leaves the PREVIOUS release
/// serving and never moved traffic, so the marker's existing record still describes
/// the release that is actually live; rewriting it would report a change that never
/// happened and erase the true one.
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
/// **It also records `torn down` in the [`last_deploy_marker`]** (issue #1621,
/// AC-6). A first deploy writes `deployed` into that marker as part of
/// [`record_proxy_options`], so a host the fleet driver compensates with this
/// teardown — `HostOutcome::CompensatedTeardown`, i.e. nothing installed — would
/// otherwise keep reporting `last deploy: deployed <ts>` in `deploy status` with no
/// release on it at all. That is a wrong value in the column an operator reads
/// first while triaging a halted rollout.
///
/// The marker is REWRITTEN rather than deleted, deliberately. An absent marker
/// renders `last deploy: ?`, which is also what a host that was never deployed (or
/// whose marker write failed) shows — so clearing it would erase precisely the fact
/// triage needs: this host WAS taken back down, on purpose, at this time. The
/// alternative loses information; this one adds it, and `mode: not deployed` plus
/// the `DRIFT_HOST_NOT_DEPLOYED` reason already sit beside it in the same row.
///
/// Two safety properties, both load-bearing: the record is the LAST op, so an
/// earlier failure stops [`run_ops`] before the marker is rewritten and the host
/// keeps its previous — still true — record; and the write is the same advisory
/// `{ … || true; }` fragment the cutover uses, so it can never turn a clean
/// compensation into `HostOutcome::CompensationFailed`.
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
    // AC-6: correct the last-deploy marker the first deploy already wrote, so a
    // host with nothing installed can never report a successful deploy. LAST, and
    // advisory — see this function's doc comment for why both matter and why the
    // marker is rewritten rather than removed.
    ops.push(DeployOp::Run(RemoteCommand::new(
        "teardown-last-deploy",
        record_last_deploy_fragment(cfg, LAST_DEPLOY_TORN_DOWN),
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
    // Re-render the target release's slot unit from the persisted marker — dir and port
    // — so rollback never depends on the slot's on-disk unit being intact. A redeploy
    // reusing this slot overwrites its unit to point at the new candidate before the
    // flip; if that redeploy fails pre-flip, its teardown removes the candidate dir but
    // leaves the slot unit pointing at the now-removed dir. Left as-is, `restart-previous`
    // below would relaunch that clobbered unit, whose ExecStart names a removed dir,
    // instead of the retained previous release. Rendering from `target.release_dir` and
    // `target.port`, not the current live config, restores the correct unit.
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
    let mut ops = Vec::new();
    // Re-link the rollback target at the shared SQLite data file (#1909) before its
    // unit is written or started. A release deployed BEFORE the file was adopted
    // into `shared/` no longer holds one at that path, so without this the
    // rolled-back release would boot against a fresh, empty database.
    ops.extend(sqlite_data_link_op(cfg, &target.release_dir).map(DeployOp::Run));
    ops.extend([
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
        // Commit the on-disk state markers as one remote transaction after the flip
        // (#1938), symmetric with cutover: a single SSH round-trip that lands or fails as
        // a unit, so a failure between the flip and completing the markers cannot leave
        // the proxy on the rolled-back release while the markers still describe the
        // release we rolled back from. It records the release being rolled back from —
        // its dir and the former-live slot, `other_slot(target.slot)` — as the new
        // "previous release", read from `current` and the live slot before they change;
        // then repoints `current` to the target release; then records the target's live
        // slot. Each marker file is written via temp file plus `mv`, an atomic rename.
        DeployOp::Run(commit_markers_command(
            cfg,
            &target.release_dir,
            target.slot,
            target.port,
            PrevMarkerFallback {
                slot: other_slot(target.slot),
                port: former_live_fallback_port,
            },
            LAST_DEPLOY_ROLLED_BACK,
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
    ]);
    ops
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
/// so [`ProxyController::refresh_installed_ops`]
/// can decide whether the unit ACTUALLY changed on this host (issue #2070). Keyed on
/// the unique `release_id` so two shared-host deploys never race on a fixed scratch
/// path; the restart step removes it.
fn proxy_unit_snapshot_path(release_id: &str) -> String {
    format!("/tmp/autumn-kamal-proxy-unit-{release_id}.sha256")
}

/// Command that records which slot now serves live traffic AND the loopback
/// `port` its unit was rendered with, as `{slot}\t{port}` (read by the next
/// redeploy's [`probe_deploy_state`], and copied into the previous-release marker
/// by `record_previous_release` so a later rollback targets the real listener).
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

/// Command that records the proxy `ServiceOptions` (TLS on/off + host) THIS deploy
/// registered with kamal-proxy into the `shared/proxy-options` marker (issue
/// #2074), so the NEXT redeploy's durability-refresh re-register can preserve the
/// old release's own TLS/host instead of stamping the new config's onto it.
///
/// Written atomically (mktemp + `mv -f` into the marker's own `shared/` dir, an
/// atomic same-filesystem rename), matching [`commit_markers_command`], so a
/// concurrent next-deploy probe never observes a half-written marker. The value is
/// [`ProxyServiceOptions::marker_value`] (`{tls}\t{host}`), shell-quoted whole so a
/// host with special chars can't break out of the command.
fn record_proxy_options(
    cfg: &ResolvedDeployConfig,
    options: &ProxyServiceOptions,
) -> RemoteCommand {
    let shared = cfg.shared_dir();
    let tmpl = format!("{shared}/proxy-options.tmp.XXXXXX");
    RemoteCommand::new(
        "record-proxy-options",
        format!(
            "otmp=$(mktemp {tmpl}) && printf '%s' {value} > \"$otmp\" && mv -f \"$otmp\" {marker} \
             && {last_deploy}",
            tmpl = shell_quote(&tmpl),
            value = shell_quote(&options.marker_value()),
            marker = shell_quote(&proxy_options_marker(cfg)),
            // AC-6: this op is the LAST marker write on both forward paths (first
            // deploy and redeploy cutover), so it is where "this host completed a
            // deploy" is true. Advisory — see `record_last_deploy_fragment`.
            last_deploy = record_last_deploy_fragment(cfg, LAST_DEPLOY_DEPLOYED),
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
    last_deploy_result: &str,
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
             mv -f \"$ltmp\" {live_marker} && \
             {last_deploy}",
            // AC-6: the last-deploy marker rides on the transaction that COMPLETES
            // the cutover, so it is the one place both the redeploy and the
            // rollback path agree on. Advisory — its failure can never turn a
            // successful marker commit into the AmbiguousMarkers state (see
            // `record_last_deploy_fragment`).
            last_deploy = record_last_deploy_fragment(cfg, last_deploy_result),
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

/// Op labels that both per-host builders emit strictly BEFORE their `migrate` op.
///
/// A host whose deploy failed at one of these never reached its migration, so the
/// schema cannot have moved — which is what lets the fleet summary
/// (`fleet::schema_moved`) stop claiming "the migration that already ran was NOT
/// rolled back" for a rollout that died while uploading. Any label NOT listed here
/// is treated as "at or after the migration", so a new step defaults to the
/// conservative side.
///
/// The list is kept honest by `pre_migrate_labels_match_both_builders`, which
/// derives the true pre-`migrate` prefix from `first_deploy_ops` and `cutover_ops`
/// and asserts it against this constant — so adding, removing or moving an op
/// fails that test rather than silently making the summary lie.
pub const PRE_MIGRATE_LABELS: &[&str] = &[
    "install-proxy",
    "proxy-snapshot-unit",
    "proxy-write-unit",
    "proxy-install",
    "proxy-restart-if-changed",
    "prepare-dirs",
    // #1909: the SQLite data-file link, emitted only for a SQLite app.
    "link-data",
    "upload-binary",
    "upload-config",
    "write-env",
    "write-unit",
    "write-candidate-unit",
    "link-current",
    "daemon-reload",
    "start-candidate",
    // Transport-level attributions for the uploads above (`failed_step_label`
    // reports a fixed label rather than the remote path): every upload on both
    // deploy paths precedes `migrate`.
    "upload",
    "stage-local-file",
];

/// Whether a host that failed at `failed_step` had already run its migration.
///
/// Conservative by design: only a label this crate KNOWS runs before `migrate`
/// ([`PRE_MIGRATE_LABELS`]) proves the schema did not move. Anything else — the
/// `migrate` op itself, everything after it, and any label this list does not
/// recognise — counts as "it may have".
#[must_use]
pub fn failed_before_migrating(failed_step: &str) -> bool {
    PRE_MIGRATE_LABELS.contains(&failed_step)
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
///
/// It also mirrors the slot unit ([`super::render_app_unit`]) in two ways, so the
/// one-shot resolves everything exactly as the release it gates will:
///
/// * `AUTUMN_MANIFEST_DIR` = the release dir, so it loads the SAME uploaded
///   `autumn.toml` the release boots with (#1952). Without it the migration ran
///   against built-in defaults plus the env file, so a config-only database
///   topology — `[[database.shards]]`, a `primary_url` that lives only in the
///   manifest — was invisible to it and it could migrate a different set of targets
///   than the app then uses.
/// * `--working-directory` = the release dir, matching the unit's
///   `WorkingDirectory`. A transient `systemd-run` unit otherwise starts in the
///   manager's default directory (`/`), so a RELATIVE database URL — a supported
///   single-host `sqlite://./app.db`, say — resolved to a different file for the
///   migration than for the app, which would then start against an unmigrated
///   database.
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
             --working-directory={workdir} --property=EnvironmentFile={env} \
             --setenv=AUTUMN_MIGRATE=1 --setenv=AUTUMN_MANIFEST_DIR={manifest} {bin}",
            service = cfg.service_name,
            workdir = shell_quote(release_dir),
            env = shell_quote(&cfg.env_file()),
            manifest = shell_quote(release_dir),
            bin = shell_quote(&bin),
        ),
    )
}

/// The op that makes a `SQLite` data file survive a deploy (issue #1909), or
/// `None` when there is nothing to keep (a Postgres app, or an absolute path
/// the deploy does not manage).
///
/// A slot unit's `WorkingDirectory` is the release dir, so a relative
/// `sqlite://app.db` resolves inside a directory that is replaced on every
/// deploy and deleted by retention. So the real file lives under `shared/data`,
/// and the release is linked at the path the app resolves. This op creates that
/// directory; `shared/` is never pruned and both slots see it. `SQLite` follows
/// the symlink when it names the `-wal`/`-shm`/`-journal` sidecars, so they land
/// beside the shared file too.
///
/// It runs immediately after `prepare-dirs`, and so BEFORE the migrate one-shot.
/// A migration that ran first would apply to a file in the release dir that the
/// app never opens.
///
/// Three steps, in order:
///
/// 1. **Refuse to relocate a live database.** An app deployed before this
///    contract holds a real file in the release that is still serving. Moving it
///    is not safe while that app runs: `SQLite` derives the `-wal` name from the
///    path it resolved, so a connection opened before the move and one opened
///    after would use two different write-ahead logs for one database. There is
///    also no atomic move — between the `mv` and the link, a new pooled
///    connection creates an empty database at the old path. So the deploy stops
///    and tells the operator to stop the app and move the file once, by hand.
///
///    A `current` that is a **symlink** is refused on the same terms. It is not
///    tested for, because it cannot be anything else: the shared file is absent
///    in this branch, so a link pointing AT it dangles and fails `-e`. Any link
///    that gets here points at a database the operator keeps elsewhere, and
///    linking past it would serve an empty one and orphan theirs.
/// 2. **Set aside a stale real file.** A rollback target from before that
///    migration still holds its own database. It is moved beside the shared file
///    as `<file>.superseded`, under `shared/`, where retention never reaches it.
///    The op refuses rather than overwrite an existing one, so it can never
///    destroy a database.
/// 3. **Link this release.**
///
/// Every interpolated path is shell-quoted. Each `mv` carries `|| exit 1`: a
/// silently failed one leaves a dangling link, and the app then creates an empty
/// database.
#[must_use]
pub fn sqlite_data_link_op(cfg: &ResolvedDeployConfig, release_dir: &str) -> Option<RemoteCommand> {
    let relative = cfg.sqlite_data_file.as_ref()?;
    let shared = cfg.shared_sqlite_data_file()?;
    let superseded = format!("{shared}.superseded");
    let shared_parent = parent_dir(&shared);
    let in_release = format!("{release_dir}/{relative}");
    let release_parent = parent_dir(&in_release);
    let current = format!("{}/{relative}", cfg.current_symlink());
    let service = &cfg.service_name;

    // Diagnostics are built HERE, from raw paths, and shell-quoted ONCE as whole
    // words. Interpolating already-quoted paths inside a double-quoted `echo`
    // would leave them expandable — single quotes are literal there — so a
    // database path containing `$(…)` would run a command on the deploy host.
    let refusal = format!(
        "autumn deploy: {current} is a live SQLite database. The data file must live at \
         {shared} to survive a deploy, and moving it while the app runs is not safe, so \
         this deploy stopped."
    );
    // The recovery line is a command the operator PASTES AND RUNS, so its own
    // operands must be shell-quoted too. Quoting only the outer `echo` makes the
    // text safe to print, not safe to run: a path holding `$(…)` would execute on
    // paste, and one holding a space would split into two `mv` arguments. The
    // trailing `*` stays OUTSIDE the quotes so it still globs the sidecars.
    let recovery = format!(
        "Run this on the host once, then deploy again: systemctl stop \
         {blue_q} {green_q}; mv {current_q}* {shared_parent_q}/",
        blue_q = shell_quote(&format!("{}.service", slot_unit_name(service, SLOT_BLUE))),
        green_q = shell_quote(&format!("{}.service", slot_unit_name(service, SLOT_GREEN))),
        current_q = shell_quote(&current),
        shared_parent_q = shell_quote(&shared_parent),
    );
    // A `current` that is a SYMLINK is refused too, and needs its own message:
    // it points at a database the operator manages elsewhere, and `mv` on the
    // link would move the link, not that database.
    let linked_refusal = format!(
        "autumn deploy: {current} is a symlink to a SQLite database outside \
         {shared}. The data file must live there to survive a deploy, so this \
         deploy stopped rather than link past it and serve an empty database."
    );
    // Each file moves to its EXACT shared name, not merely into the shared
    // directory: the link target may carry a different basename, and landing the
    // database next to the name the deploy expects rather than at it leaves the
    // next deploy creating an empty one — the very loss this refusal prevents.
    let linked_recovery = format!(
        "Run this on the host once, then deploy again: systemctl stop \
         {blue_q} {green_q}; src=$(readlink -f {current_q}); mv \"$src\" {shared_q}; \
         for s in -wal -shm -journal; do [ -e \"$src$s\" ] && mv \"$src$s\" {shared_q}$s; \
         done; rm -f {current_q}",
        blue_q = shell_quote(&format!("{}.service", slot_unit_name(service, SLOT_BLUE))),
        green_q = shell_quote(&format!("{}.service", slot_unit_name(service, SLOT_GREEN))),
        current_q = shell_quote(&current),
        shared_q = shell_quote(&shared),
    );
    let occupied =
        format!("autumn deploy: refusing to move {in_release} aside: {superseded} already exists");

    Some(RemoteCommand::new(
        "link-data",
        format!(
            "mkdir -p {shared_parent_q} {release_parent_q} && \
             if [ ! -e {shared_q} ] && [ -e {current_q} ]; then \
             if [ -L {current_q} ]; then \
             echo {linked_refusal_q} >&2; echo {linked_recovery_q} >&2; \
             else echo {refusal_q} >&2; echo {recovery_q} >&2; fi; exit 1; \
             fi && \
             if [ -e {in_release_q} ] && [ ! -L {in_release_q} ]; then \
             if [ -e {superseded_q} ]; then echo {occupied_q} >&2; exit 1; fi; \
             mv -f {in_release_q} {superseded_q} || exit 1; \
             for s in -wal -shm -journal; do \
             if [ -e {in_release_q}$s ]; then \
             mv -f {in_release_q}$s {superseded_q}$s || exit 1; fi; \
             done; \
             fi && \
             rm -f {in_release_q} && ln -s {shared_q} {in_release_q}",
            shared_parent_q = shell_quote(&shared_parent),
            release_parent_q = shell_quote(&release_parent),
            shared_q = shell_quote(&shared),
            superseded_q = shell_quote(&superseded),
            current_q = shell_quote(&current),
            in_release_q = shell_quote(&in_release),
            refusal_q = shell_quote(&refusal),
            recovery_q = shell_quote(&recovery),
            linked_refusal_q = shell_quote(&linked_refusal),
            linked_recovery_q = shell_quote(&linked_recovery),
            occupied_q = shell_quote(&occupied),
        ),
    ))
}

/// The parent directory of a remote path, or `.` when it has none.
fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(|| ".".to_owned(), |(head, _)| head.to_owned())
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

/// The proxy `ServiceOptions` (TLS on/off + host) a forward deploy registered
/// with kamal-proxy (issue #2074), recorded in the `shared/proxy-options` marker
/// so the next redeploy's durability-refresh re-register can PRESERVE the old
/// release's own TLS/host rather than stamp the new config's onto the still-live
/// old release. `host` is `Some` iff `tls` (kamal-proxy only carries a `--host`
/// when `--tls` is on); a removed host is representable as `{tls:false,host:None}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyServiceOptions {
    /// Whether the deploy registered TLS (`--host <host> --tls`).
    pub tls: bool,
    /// The TLS host, `Some` iff `tls` (empty/absent when TLS is off).
    pub host: Option<String>,
}

impl ProxyServiceOptions {
    /// Serialize to the `shared/proxy-options` marker value: `{tls}\t{host}`, with
    /// `tls` as `1`/`0` and `host` empty when TLS is off. Round-trips through
    /// [`parse_proxy_options`]. The single-string form (rather than two printf args)
    /// keeps the write DRY with the parser and the tests.
    #[must_use]
    fn marker_value(&self) -> String {
        format!(
            "{}\t{}",
            if self.tls { "1" } else { "0" },
            self.host.as_deref().unwrap_or("")
        )
    }
}

/// The `shared/proxy-options` marker state captured in the deploy-start probe
/// round-trip (issue #2074), mirroring [`InstalledProxyPort`]. The redeploy path
/// uses it to decide the durability-refresh re-register's TLS/host:
///
/// - [`Self::Absent`] (no marker / a legacy host) → proceed as legacy: re-register
///   with the NEW config options and WRITE the marker this deploy (self-heals);
/// - [`Self::Options`] → PRESERVE these OLD options on the re-register of the
///   still-live old release (the candidate flip still uses the new config);
/// - [`Self::Unreadable`] (present but unparseable) → FAIL CLOSED (refuse) — the old
///   options can't be proved, so a concurrent host change can't be safely preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyOptionsMarker {
    /// No `shared/proxy-options` marker on disk (a first-deploy shape, or a
    /// pre-#2074 host that never wrote one). Treated as legacy — see the enum docs.
    Absent,
    /// The marker file is present but its `{tls}\t{host}` value could not be parsed
    /// (missing field, bad TLS token, or TLS-on with an empty host). The refuse
    /// guard FAILS CLOSED here — the old options can't be proved.
    Unreadable,
    /// The parsed options the last forward deploy registered.
    Options(ProxyServiceOptions),
}

/// Parse the probe's proxy-options section into a [`ProxyOptionsMarker`] (#2074).
///
/// The section is `cat shared/proxy-options` (empty when the file is absent). The
/// marker value is `{tls}\t{host}` (see [`ProxyServiceOptions::marker_value`]):
///
/// - empty (absent file, or an empty marker) → [`ProxyOptionsMarker::Absent`];
/// - `1\t<non-empty host>` → `Options{tls:true, host:Some(host)}`;
/// - `0\t` (host ignored) → `Options{tls:false, host:None}`;
/// - anything else — no tab (missing field), a non-`{0,1}` TLS token, or `1\t` with
///   an empty host → [`ProxyOptionsMarker::Unreadable`] (fail closed).
///
/// Only surrounding newlines are trimmed (NOT the tab), so a well-formed TLS-off
/// `0\t` is not mistaken for a fieldless `0`.
fn parse_proxy_options(section: &str) -> ProxyOptionsMarker {
    let s = section.trim_matches(|c| c == '\n' || c == '\r');
    if s.is_empty() {
        return ProxyOptionsMarker::Absent;
    }
    // Exactly two tab fields; a missing tab (fieldless) can't be trusted → fail closed.
    let mut parts = s.splitn(2, '\t');
    let tls_field = parts.next().unwrap_or_default();
    let Some(host_field) = parts.next() else {
        return ProxyOptionsMarker::Unreadable;
    };
    match tls_field {
        "1" if !host_field.is_empty() => ProxyOptionsMarker::Options(ProxyServiceOptions {
            tls: true,
            host: Some(host_field.to_owned()),
        }),
        "0" => ProxyOptionsMarker::Options(ProxyServiceOptions {
            tls: false,
            host: None,
        }),
        // `1\t` with an empty host, a bad TLS token, or any other shape: unprovable.
        _ => ProxyOptionsMarker::Unreadable,
    }
}

/// The `--http-port` state of the currently-installed kamal-proxy systemd unit,
/// captured in the same deploy-start probe round-trip (#2073). The redeploy path
/// uses it to REFUSE a concurrent `server.port` change before touching the proxy:
/// the reboot-durability restart-refresh (#2070) re-execs `kamal-proxy run` and
/// re-registers the still-live upstream at its DERIVED port, which is only correct
/// when the public port is unchanged — so a mismatch must fail the pre-flight
/// rather than strand `:80` mid-cutover. Supporting a live-safe port change is
/// tracked separately (Option C).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledProxyPort {
    /// No proxy unit file on disk (a first-deploy shape — the durability refresh
    /// writes it fresh). The refuse guard treats this as "nothing to conflict with".
    Absent,
    /// The unit file is present but its `run --http-port {N}` value could not be
    /// read/parsed (missing flag, non-numeric, out of range, or ambiguous). The
    /// refuse guard FAILS CLOSED here — derived correctness can't be guaranteed.
    Unreadable,
    /// The port the installed unit's `ExecStart … run --http-port {N}` binds.
    Port(u16),
}

/// Delimiter appended by the deploy-start probe between the first-vs-redeploy
/// detection and the raw `kamal-proxy list` output, so both are captured in ONE
/// remote round-trip and split apart by the Rust side. Deliberately distinctive
/// so it can never collide with a marker/slot value.
const PROXY_LIST_DELIM: &str = "---autumn-kamal-proxy-list---";

/// Delimiter appended after the `kamal-proxy list` section, before the installed
/// proxy unit's `--http-port` grep (#2073), so the three sections ride in ONE
/// round-trip. Its ABSENCE (older recorded output / a scripted test that stubs
/// only the mode) is treated as [`InstalledProxyPort::Absent`] — a conservative
/// "nothing to conflict with" so the refuse guard never fires on synthetic input.
const PROXY_UNIT_DELIM: &str = "---autumn-kamal-proxy-unit---";

/// Sentinel the probe prints in the unit section when the proxy unit file does
/// NOT exist on disk — distinguishing [`InstalledProxyPort::Absent`] (no unit) from
/// [`InstalledProxyPort::Unreadable`] (unit present but no parseable `--http-port`).
const NO_PROXY_UNIT_SENTINEL: &str = "---autumn-no-proxy-unit---";

/// Delimiter appended after the installed-unit `--http-port` grep, before the
/// `shared/proxy-options` marker `cat` (#2074), so all four sections ride in ONE
/// round-trip. Its ABSENCE (older recorded output / a scripted test) leaves an
/// empty options section → [`ProxyOptionsMarker::Absent`] (proceed as legacy — the
/// refuse guard never fires on synthetic input).
const PROXY_OPTIONS_DELIM: &str = "---autumn-kamal-proxy-options---";

/// Delimiter appended after the `shared/proxy-options` marker, before
/// `readlink -f {app_dir}/current` (issue #1621, AC-6), so all five sections ride
/// in ONE round-trip. Its ABSENCE (a host deployed before this feature, older
/// recorded output, or a scripted test) leaves an empty section →
/// [`DeployProbe::current_release_dir`] `None` — "unknown", never a guessed id.
const CURRENT_RELEASE_DELIM: &str = "---autumn-current-release---";

/// The release id encoded in a resolved `current` release dir: its basename
/// (issue #1621, AC-6).
///
/// `None` for an empty or root-only path, so an unreadable symlink can never be
/// mistaken for a release named `""` — the drift report treats an unknown release
/// as a DISTINCT reported state and never as drift, and that distinction depends on
/// this returning `None` rather than something empty-but-`Some`.
#[must_use]
pub fn release_id_from_dir(dir: &str) -> Option<&str> {
    let trimmed = dir.trim().trim_end_matches('/');
    let id = trimmed.rsplit('/').next().unwrap_or_default();
    (!id.is_empty()).then_some(id)
}

/// The outcome of the deploy-start probe: the first-vs-redeploy [`DeployMode`],
/// the raw `kamal-proxy list` output, AND the installed proxy unit's `--http-port`
/// state — all captured in the SAME remote round-trip, so a drifted live-slot
/// marker can be reconciled against the live proxy and a concurrent `server.port`
/// change refused, both without a second probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployProbe {
    /// First-vs-redeploy decision (parsed exactly as before from the marker).
    pub mode: DeployMode,
    /// Raw `kamal-proxy list` stdout (empty when the proxy could not be listed —
    /// the reconcile then falls back to the marker, fail-safe).
    pub proxy_list: String,
    /// The installed kamal-proxy unit's `--http-port` (#2073), used by the redeploy
    /// path to refuse a concurrent `server.port` change before touching the proxy.
    pub installed_proxy_port: InstalledProxyPort,
    /// The proxy TLS/host options the last forward deploy recorded (#2074), used by
    /// the redeploy path to PRESERVE the old release's options on the durability
    /// re-register — or FAIL CLOSED when the marker is present but unreadable.
    pub last_proxy_options: ProxyOptionsMarker,
    /// The release dir the host's `current` symlink resolves to (#1621, AC-6); its
    /// basename is the deployed release id ([`release_id_from_dir`]).
    ///
    /// `None` when the symlink is absent, dangling, or the probe output predates
    /// this section — reported as "unknown", never guessed. `deploy status` and the
    /// fleet `maintenance` fan-out read it; the rollout path ignores it.
    pub current_release_dir: Option<String>,
}

impl DeployProbe {
    /// The live-slot decision for this host, reconciled against the running proxy —
    /// pure, and `None` for a host with no promoted release at all.
    ///
    /// The ONE seam every fleet surface decides "which slot, and therefore which
    /// unit, is live" through: `deploy status` READS the maintenance flag off it and
    /// the `deploy maintenance` fan-out WRITES to it (review round 3). Sharing it is
    /// what stops the read path and the write path from disagreeing about which file
    /// matters — a `current` symlink can name a different release than the unit the
    /// proxy is actually serving (a flip that landed with a `commit-markers` that
    /// did not), and only the unit's own view is true for the running app.
    #[must_use]
    pub fn reconcile(&self, cfg: &ResolvedDeployConfig, public_port: u16) -> Option<SlotReconcile> {
        match &self.mode {
            DeployMode::First => None,
            DeployMode::Redeploy { live_slot } => Some(reconcile_live_slot(
                live_slot,
                &self.proxy_list,
                &cfg.service_name,
                public_port,
            )),
        }
    }
}

/// Parse the probe's unit section into an [`InstalledProxyPort`] (#2073).
///
/// The section is the stdout the probe shell prints between [`PROXY_UNIT_DELIM`]
/// and end-of-output:
///
/// - exactly the [`NO_PROXY_UNIT_SENTINEL`] → [`InstalledProxyPort::Absent`] (the
///   `[ -f … ]` test failed: no unit file);
/// - a single `--http-port {N}` line with an in-range `u16` → [`InstalledProxyPort::Port`];
/// - anything else — empty (unit present but `grep` matched nothing), multiple
///   matches, or a non-`u16` number → [`InstalledProxyPort::Unreadable`] (fail closed).
fn parse_installed_proxy_port(section: &str) -> InstalledProxyPort {
    let trimmed = section.trim();
    if trimmed == NO_PROXY_UNIT_SENTINEL {
        return InstalledProxyPort::Absent;
    }
    // The unit exists (the probe printed no sentinel) but we must find EXACTLY one
    // `--http-port {N}` match — an empty section (no match) or multiple matches can't
    // be trusted to name the bound port, so fail closed.
    let mut lines = trimmed.lines().map(str::trim).filter(|l| !l.is_empty());
    let (Some(single), None) = (lines.next(), lines.next()) else {
        return InstalledProxyPort::Unreadable;
    };
    // `single` is like `--http-port 80`; take the trailing integer.
    single
        .rsplit_once(char::is_whitespace)
        .and_then(|(_, n)| n.trim().parse::<u16>().ok())
        .map_or(InstalledProxyPort::Unreadable, InstalledProxyPort::Port)
}

/// The full deploy-start probe: first-vs-redeploy mode AND the raw `kamal-proxy
/// list` output, captured in a SINGLE remote round-trip.
///
/// The probe shell keeps the existing `current`/`live-slot` detection byte-for-
/// byte (its meaning is unchanged), then appends a delimited section running
/// `env -u XDG_RUNTIME_DIR kamal-proxy list` best-effort (`|| true`, stderr
/// suppressed) so a missing binary, dead control socket, or unlisted service can
/// never fail the probe. The `env -u XDG_RUNTIME_DIR` control-socket pin mirrors
/// `deploy_shell` (issue #1948 item 4): without it the SSH session's `pam_systemd`
/// `XDG_RUNTIME_DIR=/run/user/0` points the CLI at a different socket than the
/// supervised `kamal-proxy run` service (which has no `XDG_RUNTIME_DIR` → `/tmp`),
/// so on a real pam host the list would silently come back empty — disabling both
/// the #1938 drift reconcile and the observed-port path.
/// The mode section is split off on [`PROXY_LIST_DELIM`]; the remainder is split on
/// [`PROXY_UNIT_DELIM`] into the proxy list and the installed-unit section, which is
/// itself split on [`PROXY_OPTIONS_DELIM`] into the `--http-port` grep and the
/// `shared/proxy-options` marker `cat` (#2074). When a delimiter is absent (older
/// recorded output / a scripted test) that trailing section is empty and the probe
/// degrades safely — an empty proxy list (reconcile falls back to the marker),
/// [`InstalledProxyPort::Absent`], and [`ProxyOptionsMarker::Absent`] (each treated
/// as "nothing to conflict with" / "proceed as legacy").
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
         env -u XDG_RUNTIME_DIR kamal-proxy list 2>/dev/null || true; \
         printf '\\n{unit_delim}\\n'; \
         if [ -f {unit} ]; then grep -hoE -e '--http-port[[:space:]]+[0-9]+' {unit} 2>/dev/null || true; \
         else printf '%s' '{no_unit}'; fi; \
         printf '\\n{opts_delim}\\n'; \
         cat {opts_marker} 2>/dev/null || true; \
         printf '\\n{current_delim}\\n'; \
         readlink -f {current} 2>/dev/null || true",
        current = shell_quote(&cfg.current_symlink()),
        marker = shell_quote(&live_slot_marker(cfg)),
        blue = SLOT_BLUE,
        delim = PROXY_LIST_DELIM,
        unit_delim = PROXY_UNIT_DELIM,
        unit = shell_quote(super::proxy::KAMAL_PROXY_UNIT_PATH),
        no_unit = NO_PROXY_UNIT_SENTINEL,
        opts_delim = PROXY_OPTIONS_DELIM,
        opts_marker = shell_quote(&proxy_options_marker(cfg)),
        current_delim = CURRENT_RELEASE_DELIM,
    );
    let out = exec.run(&RemoteCommand::new("detect-current", shell))?;
    let (mode_part, rest) = out
        .stdout
        .split_once(PROXY_LIST_DELIM)
        .unwrap_or((out.stdout.as_str(), ""));
    // Split the proxy list from the installed-unit section. A missing unit delimiter
    // (older/scripted output) → empty unit + options sections → `Absent`/`Absent`
    // (never a spurious refuse, always proceed-as-legacy).
    let (proxy_list, installed_proxy_port, last_proxy_options, current_release_dir) =
        match rest.split_once(PROXY_UNIT_DELIM) {
            Some((list, after_unit)) => {
                // The installed-unit section further splits into the `--http-port` grep and
                // the proxy-options marker; a missing options delimiter → empty → `Absent`.
                let (unit_section, after_opts) = after_unit
                    .split_once(PROXY_OPTIONS_DELIM)
                    .unwrap_or((after_unit, ""));
                // …and the options section further splits into the marker `cat` and the
                // `readlink -f current` result (#1621). A missing delimiter (a host
                // deployed before this feature, or a scripted test) leaves an empty
                // current section → `None` = "release unknown", never a guessed id.
                let (opts_section, current_section) = after_opts
                    .split_once(CURRENT_RELEASE_DELIM)
                    .unwrap_or((after_opts, ""));
                (
                    list,
                    parse_installed_proxy_port(unit_section),
                    parse_proxy_options(opts_section),
                    parse_current_release(current_section),
                )
            }
            None => (
                rest,
                InstalledProxyPort::Absent,
                ProxyOptionsMarker::Absent,
                None,
            ),
        };
    let mode = mode_part
        .trim()
        .strip_prefix("redeploy:")
        .map_or(DeployMode::First, |marker| {
            // The live-slot marker is `{slot}\t{port}` (older markers are slot-only);
            // the slot is the FIRST tab-separated field either way. The persisted port
            // (SECOND field, when present) is not read here — the cutover re-register
            // uses the DERIVED port, which the pre-flight refuse guard (#2073) proves
            // equals the actual live port by rejecting any concurrent `server.port`
            // change. The marker keeps persisting the port for forward-compatibility.
            let live_slot = canonical_slot(marker.split('\t').next().unwrap_or(SLOT_BLUE));
            DeployMode::Redeploy { live_slot }
        });
    Ok(DeployProbe {
        mode,
        proxy_list: proxy_list.to_owned(),
        installed_proxy_port,
        last_proxy_options,
        current_release_dir,
    })
}

/// Parse the probe's `readlink -f {app_dir}/current` section (#1621, AC-6).
///
/// Empty (absent/dangling symlink, or a probe capture predating this section) →
/// `None`. Anything else is the resolved release DIR, trimmed of surrounding
/// whitespace/newlines. Deliberately NOT fail-closed: this section is read-only
/// reporting, and refusing to report a status because a symlink is unreadable would
/// make `deploy status` useless on exactly the drifted host it exists to surface.
fn parse_current_release(section: &str) -> Option<String> {
    let trimmed = section.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Delimiter separating the two facts only `deploy status` needs — the live slot's
/// `/ready` HTTP code and the maintenance-flag presence — in one round-trip
/// (issue #1621, AC-6).
const HOST_STATUS_DELIM: &str = "---autumn-host-status---";

/// Sentinel the status probe prints when a maintenance flag file exists.
const MAINTENANCE_ON_SENTINEL: &str = "maintenance-on";

/// Whether a host is in maintenance, **as the unit it is actually running sees
/// it** (issue #1621, review round 1).
///
/// Deliberately three-valued rather than a `bool`. The flag file the runtime polls
/// is chosen by `AUTUMN_MAINTENANCE_FLAG_FILE` (see
/// [`autumn_web::maintenance::flag_file_path_from`]), which slot units only carry
/// from #1621 onwards — so on a host whose unit predates this feature the app polls
/// a release-local path the fleet switch does not own. Reading one fixed path and
/// calling the answer `on`/`off` therefore lies in both directions: `off` for a
/// host that is maintained, and `ON` for a host still taking traffic. When the CLI
/// cannot prove WHICH file the running unit polls it says so instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceStatus {
    /// The file the running unit polls exists: the app is serving maintenance.
    On,
    /// The file the running unit polls does not exist: the app is serving traffic.
    Off,
    /// The live slot unit could not be read, so the file the app polls is unknown.
    /// **Fails closed** — never rendered as a confident `ON`/`off`.
    Unknown,
}

/// Which maintenance flag file the host's live slot unit resolves to (issue #1621,
/// review round 1).
///
/// Reported alongside [`MaintenanceStatus`] because it is the actionable half: a
/// host that does not poll the shared path will have its flag orphaned by the next
/// cutover, and a fleet-wide `deploy maintenance on` cannot reach it reliably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceFlagSource {
    /// The unit declares `AUTUMN_MAINTENANCE_FLAG_FILE` and it is exactly the
    /// per-app shared path this CLI manages — the #1621 shape.
    Shared,
    /// The unit resolves to some OTHER file: no override at all (a pre-#1621 unit,
    /// polling `WorkingDirectory`-relative `tmp/autumn-maintenance.json`), or an
    /// override pointing somewhere this CLI does not write.
    Unshared,
    /// The unit could not be read, so nothing about the flag path is proved.
    Unknown,
}

/// The maintenance flag file the app on one slot ACTUALLY polls, resolved on the
/// host from that slot's unit (issue #1621, review round 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMaintenanceFlag {
    /// The path the unit makes the runtime poll: its `AUTUMN_MAINTENANCE_FLAG_FILE`
    /// when non-blank, else its `WorkingDirectory` joined with the cwd-relative
    /// legacy path — the same rule
    /// [`autumn_web::maintenance::flag_file_path_from`] applies at runtime.
    pub path: String,
    /// Whether that file existed at probe time.
    pub present: bool,
}

/// Shell that resolves, ON THE HOST, the maintenance flag file `live_slot`'s unit
/// makes the app poll — and whether it exists (issue #1621, review round 3).
///
/// The single copy of that rule. `deploy status` embeds it in its batched status
/// round-trip to REPORT the flag, and the `deploy maintenance` fan-out runs it via
/// [`probe_live_maintenance_flag`] to decide where to WRITE; a second copy is
/// exactly how the two would drift back apart into reporting one file and writing
/// another.
///
/// Prints nothing at all when the unit cannot be read — the caller's fail-closed
/// signal, never a fallback to a path the running app may not poll.
fn live_maintenance_flag_shell(cfg: &ResolvedDeployConfig, live_slot: &str) -> String {
    format!(
        "if [ -f {unit} ]; then \
         autumn_mf=$(sed -n 's|^Environment={flag_env}=||p' {unit} 2>/dev/null | tail -n 1); \
         autumn_wd=$(sed -n 's|^WorkingDirectory=||p' {unit} 2>/dev/null | tail -n 1); \
         if [ -z \"$autumn_mf\" ] && [ -n \"$autumn_wd\" ]; then \
         autumn_mf=\"$autumn_wd/{legacy_rel}\"; fi; \
         if [ -n \"$autumn_mf\" ]; then printf '%s\\n' \"$autumn_mf\"; \
         if [ -f \"$autumn_mf\" ]; then printf '%s' '{on}'; fi; fi; fi",
        unit = shell_quote(&format!(
            "/etc/systemd/system/{}.service",
            slot_unit_name(&cfg.service_name, live_slot)
        )),
        flag_env = autumn_web::maintenance::MAINTENANCE_FLAG_FILE_ENV,
        legacy_rel = autumn_web::maintenance::MAINTENANCE_FLAG_FILE,
        on = MAINTENANCE_ON_SENTINEL,
    )
}

/// Parse what [`live_maintenance_flag_shell`] printed: `{path}\n[{sentinel}]`.
///
/// `None` — a blank or absent path — means the unit could not be read. It is
/// deliberately NOT degraded to the shared path: the whole point is that a
/// pre-#1621 unit polls somewhere else entirely.
fn parse_live_maintenance_flag(section: &str) -> Option<LiveMaintenanceFlag> {
    let mut lines = section.trim_start_matches('\n').lines();
    let path = lines.next().unwrap_or_default().trim();
    if path.is_empty() {
        return None;
    }
    Some(LiveMaintenanceFlag {
        path: path.to_owned(),
        present: lines
            .next()
            .is_some_and(|line| line.trim() == MAINTENANCE_ON_SENTINEL),
    })
}

/// Resolve the maintenance flag file the host's live slot unit polls, in ONE
/// read-only round-trip (issue #1621, review round 3).
///
/// `Ok(None)` is the fail-closed answer — the unit is absent or unreadable, so
/// nothing about the running app's flag path is proved and the caller must not act
/// as though it were.
///
/// # Errors
///
/// Returns the executor's error only when the probe command cannot run at all.
pub fn probe_live_maintenance_flag(
    cfg: &ResolvedDeployConfig,
    live_slot: &str,
    exec: &impl DeployExecutor,
) -> Result<Option<LiveMaintenanceFlag>, DeployExecError> {
    let out = exec.run(&RemoteCommand::new(
        "detect-maintenance-flag",
        live_maintenance_flag_shell(cfg, live_slot),
    ))?;
    Ok(parse_live_maintenance_flag(&out.stdout))
}

/// Everything `autumn deploy status` reads from ONE host (issue #1621, AC-6).
///
/// Deliberately a superset of [`DeployProbe`] rather than an extension of it:
/// `deploy up` must NOT pay for a `curl` and a flag-file `test` on every host, and
/// the fields below are meaningless to a rollout. Keeping them here is what lets
/// [`probe_deploy_state`] stay exactly as costly as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatusProbe {
    /// The shared deploy-state probe: mode + live-slot marker, `kamal-proxy list`,
    /// the installed proxy port, the proxy-options marker and the `current` release.
    pub deploy: DeployProbe,
    /// HTTP status the live slot's loopback `/ready` answered with, or `None` when
    /// nothing answered (connection refused, no `curl`, a timeout, an
    /// unparseable code). `None` is "we could not tell", never "unhealthy".
    pub ready_code: Option<u16>,
    /// Whether the SHARED maintenance flag file exists on this host.
    ///
    /// Read from `{app_dir}/shared/…` — the release-independent path the #1621 slot
    /// units point `AUTUMN_MAINTENANCE_FLAG_FILE` at — so the answer survives a
    /// cutover, which is the whole reason that override exists. It is the state of
    /// ONE file, not a verdict: see [`Self::maintenance`] for what the RUNNING unit
    /// observes.
    pub shared_maintenance_flag: bool,
    /// Maintenance as the host's live slot unit actually observes it — the fact
    /// `deploy status` reports (issue #1621, review round 1).
    pub maintenance: MaintenanceStatus,
    /// Which file that verdict came from.
    pub maintenance_flag_source: MaintenanceFlagSource,
    /// The last state-changing deploy action this host COMPLETED, from the
    /// `shared/last-deploy` marker (AC-6), or `None` when the marker is absent or
    /// unreadable.
    ///
    /// See [`last_deploy_marker`] for exactly what it does and does not know: a
    /// deploy that failed before the cutover boundary never rewrites it, so it is
    /// the host's last completed action rather than a verdict on the last rollout.
    pub last_deploy: Option<LastDeploy>,
}

impl HostStatusProbe {
    /// The live-slot decision for this host, reconciled against the running proxy —
    /// pure, and `None` for a host with no promoted release at all.
    ///
    /// Shared with the rollout path's slot selection ([`reconcile_live_slot`]) and
    /// with the `deploy maintenance` fan-out through [`DeployProbe::reconcile`], so
    /// `status` reports the same slot a deploy would plan from — and the same one
    /// maintenance writes to — including the same proxy-over-marker precedence on a
    /// disagreement.
    #[must_use]
    pub fn reconcile(&self, cfg: &ResolvedDeployConfig, public_port: u16) -> Option<SlotReconcile> {
        self.deploy.reconcile(cfg, public_port)
    }
}

/// The read-only status probe: the shared deploy-state round-trip PLUS the live
/// slot's `/ready` code and the maintenance-flag presence (issue #1621, AC-6).
///
/// Used ONLY by `deploy status`, never by `up` — see [`HostStatusProbe`]. Both
/// commands it runs are read-only (`readlink`/`cat`/`grep`, then `curl` and
/// `[ -f ]`), so this can be pointed at a production fleet mid-incident.
///
/// The readiness probe hits the LIVE slot's loopback port (the same
/// `http://127.0.0.1:{slot_port}/ready` the deploy's readiness gate polls), not the
/// public port: the public port is kamal-proxy's, and asking the proxy would report
/// the proxy's health rather than the release's. It is bounded by `--max-time` so a
/// hung app cannot stall a fleet-wide status, and its failure is never fatal —
/// `deploy status` must report the whole fleet or it is useless during the incident
/// it exists for.
///
/// # Errors
///
/// Returns the executor's error only when a probe command cannot run at all — the
/// caller renders that host as unreachable rather than aborting the fleet report.
pub fn probe_host_status(
    cfg: &ResolvedDeployConfig,
    public_port: u16,
    exec: &impl DeployExecutor,
) -> Result<HostStatusProbe, DeployExecError> {
    let deploy = probe_deploy_state(cfg, exec)?;
    // Poll whichever slot the proxy is actually serving; on a host with nothing
    // promoted yet, blue is the slot a first deploy takes, and the probe simply
    // reports that nothing answered.
    let live_slot = deploy
        .reconcile(cfg, public_port)
        .map_or(SLOT_BLUE, |reconcile| reconcile.live_slot);
    let shared_flag = cfg.maintenance_flag_file();
    let shell = format!(
        "curl -o /dev/null -s -m 5 -w '%{{http_code}}' http://127.0.0.1:{port}/ready 2>/dev/null \
         || true; \
         printf '\\n{delim}\\n'; \
         if [ -f {flag} ]; then printf '%s' '{on}'; fi; \
         printf '\\n{delim}\\n'; \
         cat {last_deploy} 2>/dev/null || true; \
         printf '\\n{delim}\\n'; \
         {unit_flag}",
        port = slot_app_port(public_port, live_slot),
        delim = HOST_STATUS_DELIM,
        flag = shell_quote(&shared_flag),
        on = MAINTENANCE_ON_SENTINEL,
        // AC-6, third fact: the last COMPLETED deploy action. Folded into this
        // existing round-trip rather than a new per-host ssh — `deploy status` is
        // run mid-incident across the whole fleet, and every extra round-trip is
        // paid N times.
        last_deploy = shell_quote(&last_deploy_marker(cfg)),
        // Review round 1: resolve, ON THE HOST, the flag file the LIVE SLOT UNIT
        // actually makes the app poll — doing it from the unit rather than from
        // `current` is what makes the answer true for a unit rendered before #1621,
        // which carries no override line at all. Folded into this round-trip (round
        // 3 moved the fragment itself into `live_maintenance_flag_shell`, shared
        // with the `deploy maintenance` WRITE path) so `deploy status` stays at the
        // same two round-trips per host.
        unit_flag = live_maintenance_flag_shell(cfg, live_slot),
    );
    let out = exec.run(&RemoteCommand::new("probe-host-status", shell))?;
    // Section-count tolerant: a host still running a pre-#1621 probe shape (or a
    // fixture written against it) simply reports no last-deploy section, and a
    // capture predating the unit section leaves the maintenance verdict `Unknown`.
    let mut sections = out.stdout.split(HOST_STATUS_DELIM);
    let ready_section = sections.next().unwrap_or_default();
    let shared_flag_section = sections.next().unwrap_or_default();
    let last_deploy_section = sections.next().unwrap_or_default();
    let unit_flag_section = sections.next().unwrap_or_default();
    // The unit section is `{resolved path}\n[{sentinel}]`. A blank/absent path is
    // "the unit could not be read" — fail closed rather than fall back to the
    // shared path, whose state the running unit may well not observe.
    let (maintenance, maintenance_flag_source) =
        match parse_live_maintenance_flag(unit_flag_section) {
            None => (MaintenanceStatus::Unknown, MaintenanceFlagSource::Unknown),
            Some(flag) => (
                if flag.present {
                    MaintenanceStatus::On
                } else {
                    MaintenanceStatus::Off
                },
                if flag.path == shared_flag {
                    MaintenanceFlagSource::Shared
                } else {
                    MaintenanceFlagSource::Unshared
                },
            ),
        };
    Ok(HostStatusProbe {
        deploy,
        // `curl` writes `000` when it never got a response; that is "nothing
        // answered", not an HTTP status, so it degrades to `None` like every other
        // unreadable capture.
        ready_code: ready_section
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|code| *code > 0),
        shared_maintenance_flag: shared_flag_section.trim() == MAINTENANCE_ON_SENTINEL,
        maintenance,
        maintenance_flag_source,
        last_deploy: parse_last_deploy(last_deploy_section),
    })
}

/// Sentinel [`probe_release_dir`] prints when `{releases_dir}/{release_id}` already
/// exists on the host.
const RELEASE_DIR_PRESENT: &str = "present";

/// Sentinel [`probe_release_dir`] prints when the release dir is free.
const RELEASE_DIR_ABSENT: &str = "absent";

/// Whether this run's release dir already exists on a host (issue #1621, §4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseDirState {
    /// The release dir is free — the normal case.
    Absent,
    /// The release dir already exists: a previous run of THIS release id already
    /// wrote into it. Refused (see [`probe_release_dir`]).
    Present,
    /// The probe printed neither sentinel, so the dir's state cannot be proved.
    /// **Fails closed** — the collision below is destructive and silent.
    Unreadable,
}

/// Read-only probe: does `{releases_dir}/{release_id}` already exist on this host?
/// (issue #1621, §4.9.)
///
/// `release_id` is a UTC timestamp with **one-second granularity**
/// (`default_release_id`), and exactly ONE id is minted per fleet run. A fast
/// retry within the same second therefore re-uses the id and would upload a NEW
/// binary into the release dir `shared/previous-release` still points at — so the
/// host's "previous release" would hold the new binary and a rollback would roll
/// *forward*, silently. There is no marker that can detect this after the fact,
/// so the deploy refuses up front, before anything is written.
///
/// It is deliberately its OWN round-trip rather than a fifth section on
/// [`probe_deploy_state`]: that probe's shell is pinned by exact-content tests and
/// its sections are all about the *live* release, while this one is about the
/// *candidate* dir and must fail closed rather than degrade to `Absent`.
///
/// # Errors
///
/// Returns the executor's error if the probe command cannot run.
pub fn probe_release_dir(
    cfg: &ResolvedDeployConfig,
    release_id: &str,
    exec: &impl DeployExecutor,
) -> Result<ReleaseDirState, DeployExecError> {
    let release_dir = format!("{}/{release_id}", cfg.releases_dir());
    probe_dir_state("probe-release-dir", &release_dir, exec)
}

/// Read-only probe: does the release dir a fleet compensation is about to roll a
/// host back TO still exist? (issue #1621, §4.7.)
///
/// The same `[ -d … ]` shell as [`probe_release_dir`] under a DISTINCT label, so a
/// tape can tell the two apart and the strict test fake can require it to be
/// scripted. It is a separate call rather than a flag because it asks the opposite
/// question about the opposite directory: the deploy refuses when this run's
/// candidate dir is PRESENT, while compensation refuses when the rollback target is
/// ABSENT.
///
/// It exists because `prune` runs per host and hosts with divergent deploy history
/// legitimately retain different sets: `resolve_rollback_target` can name a dir a
/// later prune already removed. Rolling back to it anyway would write a slot unit
/// whose `ExecStart` points nowhere, start it successfully as far as systemd is
/// concerned, and then fail the readiness gate — POST-boundary, with no teardown,
/// turning a one-host incident into a two-host one. Missing → the caller declines
/// the automatic rollback and reports the host.
///
/// # Errors
///
/// Returns the executor's error if the probe command cannot run.
pub fn probe_rollback_target_dir(
    release_dir: &str,
    exec: &impl DeployExecutor,
) -> Result<ReleaseDirState, DeployExecError> {
    probe_dir_state("probe-rollback-target", release_dir, exec)
}

/// The shared read-only `[ -d … ]` directory probe behind [`probe_release_dir`] and
/// [`probe_rollback_target_dir`]: two printf sentinels, nothing else, and any other
/// capture fails closed to [`ReleaseDirState::Unreadable`] (an empty capture is the
/// exact trap every other probe in this file degrades on).
fn probe_dir_state(
    label: &'static str,
    dir: &str,
    exec: &impl DeployExecutor,
) -> Result<ReleaseDirState, DeployExecError> {
    let shell = format!(
        "if [ -d {dir} ]; then printf '%s' '{RELEASE_DIR_PRESENT}'; \
         else printf '%s' '{RELEASE_DIR_ABSENT}'; fi",
        dir = shell_quote(dir),
    );
    let out = exec.run(&RemoteCommand::new(label, shell))?;
    Ok(match out.stdout.trim() {
        RELEASE_DIR_PRESENT => ReleaseDirState::Present,
        RELEASE_DIR_ABSENT => ReleaseDirState::Absent,
        // Fail closed: an unexpected capture cannot prove the dir's state.
        _ => ReleaseDirState::Unreadable,
    })
}

/// Run the controller's compat probe once and return its raw verdict.
///
/// `Ok(None)` means the controller declares no probe (nothing to check and no
/// remote command run). The outer `Result` is the TRANSPORT result — an ssh that
/// could not run at all — kept separate from the verdict so a caller can react to
/// "no binary" without conflating it with "the host is unreachable".
fn run_proxy_compat_probe(
    proxy: &impl ProxyController,
    exec: &impl DeployExecutor,
) -> Result<Option<Result<(), ProxyCompatFailure>>, DeployExecError> {
    let Some(probe) = proxy.compat_probe() else {
        // No declared probe (e.g. a Caddy controller): nothing to guard.
        return Ok(None);
    };
    let out = exec.run(&probe.command)?;
    // The probe folds stderr into stdout via `2>&1`; assess both defensively so a
    // stderr-only capture (a different executor) is still classified correctly.
    let combined = if out.stderr.trim().is_empty() {
        out.stdout
    } else {
        format!("{}\n{}", out.stdout, out.stderr)
    };
    Ok(Some(probe.assess(&combined)))
}

/// Whether this deploy may PREPARE the target host by installing a missing proxy
/// binary (issue #1607, AC-1), or must leave the host untouched.
///
/// Resolved from `[deploy] install_proxy` (default `true`). The opt-out exists for
/// operators who provision the proxy themselves — a pinned corporate build, a
/// package they maintain, a host they do not want a container runtime on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyProvisioning {
    /// Install the proxy binary when — and only when — the host has none.
    Auto,
    /// Never install anything; a missing binary is an actionable failure.
    Disabled,
}

/// What the read-only proxy assessment found on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyReadiness {
    /// A working proxy binary is installed (or the controller declares no probe):
    /// nothing to prepare.
    Ready,
    /// The host has no usable proxy binary, and this deploy may install one. The
    /// caller prepends [`ProxyController::binary_install_ops`] to that host's op
    /// vector — see [`assess_proxy_readiness`].
    NeedsInstall,
}

/// Decide, WITHOUT mutating the target, whether the host already has a working
/// reverse proxy or needs one installed (issue #1607, AC-1; the CLI-drift guard is
/// issue #2053).
///
/// AC-1 puts the target-host precondition at "at most a stock Ubuntu LTS with SSH
/// access", so a missing kamal-proxy is something this command fixes rather than
/// something it demands the operator fix first. This function only *decides*; the
/// fix itself is an ordinary op at the head of that host's sequence, which is what
/// keeps the fleet's all-hosts probe phase strictly read-only ("no host is touched
/// until every host is graded") and lets a failed install ride the same halt +
/// compensation path as any other pre-cutover failure.
///
/// The three cases:
///
/// - **Working binary** (or no probe declared) → [`ProxyReadiness::Ready`], silent,
///   nothing beyond the read-only probe ran. A host that is already fine is
///   untouched.
/// - **No usable binary**, host prep allowed and the controller can install one →
///   [`ProxyReadiness::NeedsInstall`].
/// - **A binary that responds but has drifted** (renamed/removed subcommand or
///   flag), or a missing binary this deploy may not or cannot fix → fail closed
///   with the controller's actionable message. A responding binary is NEVER
///   replaced: it is somebody's working install, possibly shared with another app
///   on the host.
///
/// # Errors
///
/// Returns [`DeployExecError::ProxyIncompatible`] when the installed proxy's CLI
/// surface has drifted, or when the host has no usable proxy and this deploy may
/// not install one (`[deploy] install_proxy = false`, or a controller that declares
/// no installer); or the executor's error if the probe cannot run.
pub fn assess_proxy_readiness(
    proxy: &impl ProxyController,
    exec: &impl DeployExecutor,
    provisioning: ProxyProvisioning,
) -> Result<ProxyReadiness, DeployExecError> {
    let Some(verdict) = run_proxy_compat_probe(proxy, exec)? else {
        return Ok(ProxyReadiness::Ready);
    };
    let Err(failure) = verdict else {
        return Ok(ProxyReadiness::Ready);
    };

    // Only an ABSENT binary is host prep's business, and only when this deploy is
    // allowed to prepare the host and the controller knows how.
    if !failure.binary_missing {
        // A responding binary whose CLI surface drifted: never ours to replace.
        return Err(DeployExecError::ProxyIncompatible {
            message: failure.message,
        });
    }
    match (provisioning, proxy.binary_install_ops()) {
        (ProxyProvisioning::Auto, Some(_)) => Ok(ProxyReadiness::NeedsInstall),
        // The host COULD have been prepared, but the operator declined it. Say which
        // setting is in force, so the message names the reason this deploy stopped
        // rather than describing the branch that is not running.
        (ProxyProvisioning::Disabled, _) => Err(DeployExecError::ProxyIncompatible {
            message: format!(
                "{} (`[deploy] install_proxy = false` declines host preparation, so this \
                 deploy will not install it for you)",
                failure.message,
            ),
        }),
        // The controller has no installer at all (it expects its binary to arrive
        // some other way): report the missing binary unchanged.
        (ProxyProvisioning::Auto, None) => Err(DeployExecError::ProxyIncompatible {
            message: failure.message,
        }),
    }
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

/// The single loopback target port `kamal-proxy list` UNAMBIGUOUSLY reports
/// serving `service_name`, or `None` when the signal is absent or unclear.
///
/// This is the proxy's OBSERVED current upstream target — the port the live slot
/// is ACTUALLY routed to right now — and is the ground truth of what is being
/// served, independent of the public port or any persisted marker (#2071). It is
/// returned ONLY when exactly one output row carries `service_name` as a standalone
/// whitespace field (the header row never does, and a service listed twice is
/// ambiguous → `None`) and that row carries exactly one distinct `127.0.0.1:<port>`
/// target. Deliberately does NOT require the port to map to a slot band under the
/// current public port: on a `server.port` change the live release still binds its
/// OLD port, which no longer maps to a slot relative to the NEW public port, yet is
/// exactly the port we must preserve.
#[must_use]
pub fn proxy_live_target_port(list_stdout: &str, service_name: &str) -> Option<u16> {
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
    port
}

/// The slot `kamal-proxy list` UNAMBIGUOUSLY reports serving `service_name`, or
/// `None` when the signal is absent or unclear (→ caller falls back to the marker).
///
/// A definite slot is returned ONLY when [`proxy_live_target_port`] resolves an
/// unambiguous single `127.0.0.1:<port>` target AND `port - public_port` maps to a
/// slot (1=blue, 2=green). Every other shape — service absent, no/garbled target,
/// two different target ports, a port mapping to neither slot — yields `None`.
fn proxy_live_slot(
    list_stdout: &str,
    service_name: &str,
    public_port: u16,
) -> Option<&'static str> {
    slot_for_port(
        public_port,
        proxy_live_target_port(list_stdout, service_name)?,
    )
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
            match boundary {
                // Past a KNOWN boundary: traffic already moved, so the candidate is
                // live and nothing may be torn down. The error is attributed to the
                // op that was running — several executor errors carry no label of
                // their own, and post-boundary policy is decided by op
                // (`DeployExecError::PostCutover`, issue #1621 §4.6).
                Some(b) if index > b => {
                    return Err(DeployExecError::PostCutover {
                        failed_step: op.label(),
                        source: Box::new(source),
                    });
                }
                // Fail safe: if the boundary op was never found (`None`), we do NOT
                // know whether the candidate is already live, so we must never tear
                // it down — a missing/mislabeled boundary surfaces the raw error
                // instead of risking teardown of a possibly-live app, and does not
                // claim to know which side of the cutover the failure landed on.
                None => return Err(source),
                // At or before a known boundary: teardown is safe.
                Some(_) => {}
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
/// plain driver is the un-gated form, used by the pure-sequence tests and by the
/// fleet compensation path (issue #1621, §4.7), which tears a completed first
/// deploy down and must **report** a failed step rather than swallow it the way
/// [`run_teardown`] deliberately does.
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

/// The recording executor fake, shared across the deploy module's test suites.
///
/// Lives outside `mod tests` (and is `pub(crate)`) so the fleet rollout driver's
/// tests can drive N of these — one per host — against the SAME fake as the
/// single-host op-sequence tests, instead of growing a third near-duplicate copy
/// (issue #1621, plan §9.2). `#[cfg(test)]`, so nothing here reaches a release
/// build.
#[cfg(test)]
// In this bin-only crate `deploy` is a private module, so clippy flags every
// `pub(crate)` here as redundant; they are kept to document the intended
// visibility (crate-internal test support) rather than widening to `pub`.
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod test_support {
    use super::{CommandOutput, DeployExecError, DeployExecutor, Path, RemoteCommand};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The fleet-wide `(host, call)` tape shared by every host's executor in one
    /// rollout — the structure cross-host ordering assertions read.
    pub(crate) type FleetTape = Rc<RefCell<Vec<(String, RecordedCall)>>>;

    /// Command labels whose **stdout is parsed** by the caller, i.e. the read-only
    /// probes. An unscripted probe is the single most dangerous silent hole in this
    /// fake: `run` returns `Ok` with EMPTY stdout for anything unscripted, and
    /// [`super::probe_deploy_state`] reads an empty section as
    /// [`super::DeployMode::First`] / `Absent`. A fleet test that forgets to script
    /// host N's probe would therefore exercise the first-deploy branch and still
    /// pass. [`RecordingExecutor::strict`] turns that into a loud panic.
    pub(crate) const PROBE_LABELS: [&str; 7] = [
        "proxy-compat-probe",
        "detect-current",
        "probe-release-dir",
        "probe-rollback-target",
        "resolve-previous",
        "probe-host-status",
        // Round 3: the maintenance WRITE path parses this one to decide which file
        // the running unit polls. Unscripted, it reads as "the unit could not be
        // read" and the fan-out would fail closed for the wrong reason.
        "detect-maintenance-flag",
    ];

    /// One recorded executor call. Uploads carry no local path: op building is
    /// pure, so the local path is an input the assertions never need.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum RecordedCall {
        /// A `run` call.
        Run {
            /// The op's stable `&'static str` label.
            label: &'static str,
            /// The rendered remote shell line.
            shell: String,
        },
        /// An `upload` call (a `UploadFile` or a staged `WriteFile`).
        Upload {
            /// Remote destination path.
            remote_path: String,
            /// Requested mode, when the op set one.
            mode: Option<u32>,
        },
    }

    impl RecordedCall {
        /// The label of a `Run` call, or `None` for an upload.
        pub(crate) const fn run_label(&self) -> Option<&'static str> {
            match self {
                Self::Run { label, .. } => Some(*label),
                Self::Upload { .. } => None,
            }
        }
    }

    /// A recording fake executor: it records every `run`/`upload` call in order
    /// and returns scripted outputs, so tests assert the exact remote-command
    /// sequence (and env-file mode) without a live host.
    #[derive(Default)]
    pub(crate) struct RecordingExecutor {
        calls: RefCell<Vec<RecordedCall>>,
        /// Labels whose `run` should return a scripted failure (e.g. to simulate
        /// a readiness-gate timeout).
        fail_labels: Vec<&'static str>,
        /// #1621: labels whose `run` should fail as a TRANSPORT error — ssh itself
        /// could not be launched — rather than as a remote non-zero exit. The
        /// distinction matters to the fleet: [`super::DeployExecError::Spawn`]
        /// carries no op label, so it classifies as a FUNCTIONAL post-boundary
        /// failure (fail closed) where a `CommandFailed` on the same step might be
        /// mere housekeeping.
        transport_fail_labels: Vec<&'static str>,
        /// Scripted stdout returned for a given command label.
        stdout_by_label: Vec<(&'static str, String)>,
        /// #1621: remote-path fragments whose `upload` should fail. Uploads carry
        /// no label, so failure is keyed on the destination path — the only
        /// identity an upload has. This is what lets a test fail a `WriteFile` op
        /// (the maintenance fan-out's flag write) the way a dying scp would.
        upload_fail_paths: Vec<String>,
        /// #1621: the host this executor targets, recorded onto the shared fleet
        /// tape. Empty for the single-host fakes, which never set a tape.
        host: String,
        /// #1621: a tape shared by every host's executor in one fleet run, so
        /// CROSS-host interleaving ("host B's first mutating op came after host A's
        /// `proxy-flip`") is assertable — something a per-host call list cannot
        /// express.
        tape: Option<FleetTape>,
        /// #1621: panic on an unscripted [`PROBE_LABELS`] entry instead of
        /// silently returning empty stdout.
        strict: bool,
    }

    impl RecordingExecutor {
        /// A fake that scripts nothing and fails nothing.
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// A fake whose `run` fails for `label`.
        pub(crate) fn failing_on(label: &'static str) -> Self {
            Self {
                fail_labels: vec![label],
                ..Self::default()
            }
        }

        /// Chainable sibling of [`Self::failing_on`], so one fake can fail on more
        /// than one label (a fleet script needs per-host failure injection).
        pub(crate) fn failing(mut self, label: &'static str) -> Self {
            self.fail_labels.push(label);
            self
        }

        /// Make one host's `label` fail as a TRANSPORT error (the ssh process could
        /// not be launched) instead of a remote non-zero exit (#1621).
        ///
        /// This is the only realistic way to produce a FUNCTIONAL post-boundary
        /// failure against today's op set: every op after `proxy-flip` is either
        /// housekeeping or `commit-markers`, so the "the host is live on the new
        /// release and something that matters broke" case arrives as a dropped
        /// transport rather than a labelled remote failure.
        pub(crate) fn transport_failing(mut self, label: &'static str) -> Self {
            self.transport_fail_labels.push(label);
            self
        }

        /// Script the stdout returned for a given command label (used to drive
        /// [`super::probe_deploy_state`]'s first-vs-redeploy probe).
        pub(crate) fn with_stdout(
            mut self,
            label: &'static str,
            stdout: impl Into<String>,
        ) -> Self {
            self.stdout_by_label.push((label, stdout.into()));
            self
        }

        /// Make `upload` fail for any remote path containing `fragment` (#1621).
        ///
        /// The upload is still RECORDED first, mirroring `run`'s scripted
        /// failures, so a test can assert the attempt happened.
        pub(crate) fn failing_upload(mut self, fragment: impl Into<String>) -> Self {
            self.upload_fail_paths.push(fragment.into());
            self
        }

        /// Fail LOUDLY (panic) on an unscripted read-only probe label instead of
        /// returning empty stdout, which every probe parser degrades to
        /// "absent / first deploy" (issue #1621, plan §9.2).
        pub(crate) fn strict(mut self) -> Self {
            self.strict = true;
            self
        }

        /// Record this executor's calls onto a fleet-wide `tape` under `host`, in
        /// addition to its own per-host list.
        pub(crate) fn recording_as(mut self, host: impl Into<String>, tape: FleetTape) -> Self {
            self.host = host.into();
            self.tape = Some(tape);
            self
        }

        /// Every recorded call, in order.
        pub(crate) fn calls(&self) -> Vec<RecordedCall> {
            self.calls.borrow().clone()
        }

        /// Labels of the recorded `Run` calls, in order (upload calls excluded).
        pub(crate) fn run_labels(&self) -> Vec<&'static str> {
            self.calls
                .borrow()
                .iter()
                .filter_map(RecordedCall::run_label)
                .collect()
        }

        /// The shell recorded for the first `Run` with `label`, if any.
        pub(crate) fn shell_for(&self, label: &str) -> Option<String> {
            self.calls.borrow().iter().find_map(|c| match c {
                RecordedCall::Run { label: l, shell } if *l == label => Some(shell.clone()),
                _ => None,
            })
        }

        /// Push one call onto the per-host list and, when set, the fleet tape.
        fn record(&self, call: RecordedCall) {
            if let Some(tape) = &self.tape {
                tape.borrow_mut().push((self.host.clone(), call.clone()));
            }
            self.calls.borrow_mut().push(call);
        }
    }

    impl DeployExecutor for RecordingExecutor {
        fn run(&self, cmd: &RemoteCommand) -> Result<CommandOutput, DeployExecError> {
            self.record(RecordedCall::Run {
                label: cmd.label,
                shell: cmd.shell.clone(),
            });
            if self.transport_fail_labels.contains(&cmd.label) {
                return Err(DeployExecError::Spawn {
                    program: "ssh".to_owned(),
                    source: std::io::Error::other("scripted transport failure"),
                });
            }
            if self.fail_labels.contains(&cmd.label) {
                return Err(DeployExecError::CommandFailed {
                    label: cmd.label,
                    message: "scripted failure".to_owned(),
                });
            }
            let scripted = self
                .stdout_by_label
                .iter()
                .find(|(l, _)| *l == cmd.label)
                .map(|(_, out)| out.clone());
            assert!(
                !(self.strict && scripted.is_none() && PROBE_LABELS.contains(&cmd.label)),
                "unscripted probe `{}`{}: this fake would return EMPTY stdout, which every \
                 probe parser degrades to \"absent / first deploy\" — script it with \
                 `.with_stdout(\"{}\", …)` (issue #1621)",
                cmd.label,
                if self.host.is_empty() {
                    String::new()
                } else {
                    format!(" on host {}", self.host)
                },
                cmd.label,
            );
            Ok(CommandOutput {
                stdout: scripted.unwrap_or_default(),
                stderr: String::new(),
            })
        }

        fn upload(
            &self,
            _local: &Path,
            remote_path: &str,
            mode: Option<u32>,
        ) -> Result<(), DeployExecError> {
            self.record(RecordedCall::Upload {
                remote_path: remote_path.to_owned(),
                mode,
            });
            if self
                .upload_fail_paths
                .iter()
                .any(|fragment| remote_path.contains(fragment.as_str()))
            {
                return Err(DeployExecError::CommandFailed {
                    label: "upload",
                    message: "scripted upload failure".to_owned(),
                });
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{RecordedCall, RecordingExecutor};
    use super::*;

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

    /// [`resolved`] plus the #1909 `SQLite` data-file contract: a relative
    /// `sqlite://app.db`, which is the shape that needs relocating.
    fn resolved_sqlite() -> ResolvedDeployConfig {
        resolved().with_sqlite_data_file(Some("app.db".to_owned()))
    }

    const RELEASE_ID: &str = "20260714T120000Z";
    const RELEASE_DIR: &str = "/srv/autumn/myapp/releases/20260714T120000Z";

    fn proxy() -> super::super::proxy::KamalProxyController {
        super::super::proxy::KamalProxyController::new(60)
    }

    /// First-deploy ops: initial release on the blue slot (loopback 3001) behind
    /// the proxy on the public port 3000, carrying the pre-start migration
    /// ([`MigrateStep::Run`] — what a single-host first deploy always does).
    fn sample_ops(env: Secret) -> Vec<DeployOp> {
        sample_ops_with(env, MigrateStep::Run)
    }

    /// [`sample_ops`] with an explicit [`MigrateStep`], so the fleet's
    /// skip-the-migrate-op parameterisation of the FIRST-deploy path is assertable
    /// alongside the single-host `Run` sequence.
    fn sample_ops_with(env: Secret, migrate: MigrateStep) -> Vec<DeployOp> {
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
            migrate,
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
    /// green (loopback 3002). The cutover re-registers the still-live OLD release at
    /// the DERIVED live-slot port (`plan.live_port`, blue = 3001) — correct because
    /// the redeploy path refuses a concurrent `server.port` change at pre-flight
    /// (#2073), so the public port is unchanged and derived == actual.
    fn sample_cutover_ops(env: Secret) -> Vec<DeployOp> {
        sample_cutover_ops_with(env, MigrateStep::Run)
    }

    /// [`sample_cutover_ops`] with an explicit [`MigrateStep`] (issue #1621), so the
    /// fleet's skip-the-migrate-op parameterisation is assertable while every
    /// existing exact-vector call site keeps building today's `Run` sequence.
    fn sample_cutover_ops_with(env: Secret, migrate: MigrateStep) -> Vec<DeployOp> {
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
            // The default `proxy()` terminates no TLS, so the still-live old release's
            // preserved options are TLS-off (matching what it registered).
            &ProxyServiceOptions {
                tls: false,
                host: None,
            },
            migrate,
        )
    }

    #[test]
    fn detect_current_prints_no_proxy_sentinel_literally() {
        // The `detect-current` probe emits the "no proxy unit" sentinel, whose
        // value begins with `--`. It MUST be printed as a literal string argument
        // (`printf '%s' '<sentinel>'`), never as printf's format string
        // (`printf '<sentinel>'`) — otherwise POSIX printf parses the leading `--`
        // as an invalid option and exits non-zero, breaking deploy on any fresh
        // target that has no kamal-proxy unit (the `else` branch always runs).
        let cfg = resolved();
        let exec = RecordingExecutor::new();
        // Scripted-empty stdout; we only care about the emitted shell command.
        let _ = probe_deploy_state(&cfg, &exec);
        let shell = exec
            .shell_for("detect-current")
            .expect("detect-current ran");
        let safe = format!("printf '%s' '{NO_PROXY_UNIT_SENTINEL}'");
        let unsafe_form = format!("printf '{NO_PROXY_UNIT_SENTINEL}'");
        assert!(
            shell.contains(&safe),
            "the no-proxy sentinel must be printed via `printf '%s'`: {shell}"
        );
        assert!(
            !shell.contains(&unsafe_form),
            "the no-proxy sentinel must not be printed as a bare printf format \
             string (its `--` prefix would be parsed as an invalid option): {shell}"
        );
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

    /// AC-3 (issue #1607) on the FIRST deploy: pending migrations must run before
    /// the new version takes traffic. The first deploy used to skip migrations
    /// entirely, so a database-backed app's very first `deploy up` health-gated and
    /// routed an app whose schema had never been applied.
    #[test]
    fn first_deploy_runs_pending_migrations_before_the_app_starts() {
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        let labels = exec.run_labels();
        let pos = |needle: &str| labels.iter().position(|&l| l == needle);

        let migrate = pos("migrate").expect("the first deploy runs pending migrations");
        // BEFORE the app starts (stricter than the redeploy path, which starts the
        // candidate first): a first deploy has no live release to protect, and an
        // app booted against an unmigrated schema can crash-loop under systemd
        // before the gate ever runs.
        assert!(
            migrate < pos("enable-now").expect("the app starts"),
            "migrations must run before the first release starts: {labels:?}"
        );
        // ... and therefore before it is health-gated and takes traffic.
        assert!(
            migrate < pos("readiness-gate").expect("readiness gate")
                && migrate < pos("proxy-route").expect("proxy is routed"),
            "migrations must precede the readiness gate and the route: {labels:?}"
        );
        // The env file the one-shot reads, and the binary it runs, must be uploaded
        // BEFORE it — an existence check alone would stay green if either upload
        // moved after the migration.
        let calls = exec.calls();
        let migrate_call = calls
            .iter()
            .position(|c| c.run_label() == Some("migrate"))
            .expect("migrate is recorded");
        for path in [
            "/srv/autumn/myapp/shared/autumn.env",
            "/srv/autumn/myapp/releases/20260714T120000Z/myapp",
        ] {
            let uploaded = calls
                .iter()
                .position(|c| {
                    matches!(c, RecordedCall::Upload { remote_path, .. } if remote_path == path)
                })
                .unwrap_or_else(|| panic!("{path} is uploaded"));
            assert!(
                uploaded < migrate_call,
                "{path} must be in place before the migrate one-shot runs: {calls:?}"
            );
        }
        // The same real, blocking one-shot the redeploy path uses: `--wait` is what
        // makes a failed migration stop `run_ops` before anything takes traffic.
        let shell = exec.shell_for("migrate").expect("migrate ran");
        assert!(
            shell.contains("systemd-run --wait")
                && shell.contains("--setenv=AUTUMN_MIGRATE=1")
                && shell.contains("/srv/autumn/myapp/releases/20260714T120000Z/myapp"),
            "the first deploy runs the real migrate one-shot from the release dir: {shell}"
        );
    }

    /// The migrate one-shot must resolve its configuration and its RELATIVE paths
    /// exactly as the release it gates will, on both deploy paths.
    #[test]
    fn the_migrate_one_shot_matches_the_slot_unit_directory_and_manifest() {
        let release_dir = "/srv/autumn/myapp/releases/20260714T120000Z";
        let unit = super::super::render_app_unit(&resolved(), release_dir, 3001, SLOT_BLUE);
        for (path, ops) in [
            (
                "first deploy",
                sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n")),
            ),
            (
                "redeploy",
                sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n")),
            ),
        ] {
            let exec = RecordingExecutor::new();
            run_ops(&ops, &exec).expect("recording executor never fails");
            let shell = exec.shell_for("migrate").expect("migrate ran");
            // A transient `systemd-run` unit otherwise starts in the manager's
            // default directory, so a relative database URL (a supported single-host
            // `sqlite://./app.db`) would be migrated in one place and read in
            // another. The unit's own `WorkingDirectory` is the contract to match.
            assert!(
                shell.contains(&format!("--working-directory='{release_dir}'")),
                "{path}: the one-shot must run in the release dir, like the slot \
                 unit: {shell}"
            );
            assert!(
                unit.contains(&format!("WorkingDirectory={release_dir}")),
                "{path}: the slot unit's WorkingDirectory is the contract being \
                 matched: {unit}"
            );
            // …and it must load the same manifest the unit points the app at, so a
            // config-only database topology is not invisible to the migration.
            assert!(
                shell.contains(&format!("--setenv=AUTUMN_MANIFEST_DIR='{release_dir}'")),
                "{path}: the one-shot must load the release's own manifest: {shell}"
            );
            assert!(
                unit.contains(&format!("Environment=AUTUMN_MANIFEST_DIR={release_dir}")),
                "{path}: the slot unit's manifest dir is the contract being matched"
            );
        }
    }

    /// The fleet parameterisation of the FIRST-deploy path (#1621's rule, now that a
    /// first deploy migrates): `Skip` omits ONLY the migrate op and leaves every
    /// other step's identity and relative position untouched.
    #[test]
    fn first_deploy_migrate_skip_omits_only_the_migrate_op() {
        let run = sample_ops_with(
            Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"),
            MigrateStep::Run,
        );
        let skip = sample_ops_with(
            Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"),
            MigrateStep::Skip,
        );
        let exec_run = RecordingExecutor::new();
        run_ops(&run, &exec_run).expect("recording executor never fails");
        let exec_skip = RecordingExecutor::new();
        run_ops(&skip, &exec_skip).expect("recording executor never fails");

        let with: Vec<&str> = exec_run
            .run_labels()
            .into_iter()
            .filter(|l| *l != "migrate")
            .collect();
        assert_eq!(
            with,
            exec_skip.run_labels(),
            "Skip must remove the migrate op and nothing else"
        );
        assert!(
            !exec_skip.run_labels().contains(&"migrate"),
            "Skip must not schedule a migration"
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
                "record-proxy-options",
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
    fn cutover_ops_skip_omits_only_the_migrate_op() {
        // #1621 (AC-4): a fleet's schema is fleet-wide, so it migrates exactly once, and
        // hosts 2..N build their cutover with `MigrateStep::Skip`. Skipping must remove
        // the `migrate` op and nothing else: the boundary label (`proxy-flip`) keeps its
        // identity and every other step keeps its relative position, or
        // `execute_with_teardown`'s boundary lookup — and with it the per-host
        // auto-rollback the fleet driver depends on — would silently change meaning on
        // every host after the first. The assertion is differential, deriving `Skip`'s
        // vector from `Run`'s, so it can never drift from the exact vector pinned by
        // `redeploy_produces_exact_zero_downtime_sequence`.
        let labels = |ops: &[DeployOp]| ops.iter().map(DeployOp::label).collect::<Vec<_>>();
        let run = labels(&sample_cutover_ops_with(
            Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"),
            MigrateStep::Run,
        ));
        let skip = labels(&sample_cutover_ops_with(
            Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"),
            MigrateStep::Skip,
        ));

        assert_eq!(
            run.iter().filter(|l| **l == "migrate").count(),
            1,
            "MigrateStep::Run must emit exactly one migrate op, got: {run:?}"
        );
        let expected: Vec<&'static str> = run.iter().copied().filter(|l| *l != "migrate").collect();
        assert_eq!(
            skip, expected,
            "MigrateStep::Skip must be MigrateStep::Run minus exactly the `migrate` \
             op\n  run:  {run:?}\n  skip: {skip:?}"
        );
        assert!(
            !skip.contains(&"migrate"),
            "a skipped cutover must carry no migrate op, got: {skip:?}"
        );
        assert!(
            skip.contains(&"proxy-flip"),
            "skipping the migration must not disturb the cutover boundary, got: {skip:?}"
        );
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
        // It is a superset of the candidate teardown, PLUS the marker cleanup, PLUS
        // the `torn down` last-deploy record (#1621 audit gap G3) — see
        // `first_deploy_teardown_records_the_torn_down_result` for why the marker is
        // rewritten rather than removed.
        assert_eq!(
            labels,
            vec![
                "teardown-candidate-unit",
                "teardown-candidate-dir",
                "teardown-current-symlink",
                "teardown-slot-markers",
                "teardown-last-deploy",
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
    fn first_deploy_teardown_records_the_torn_down_result() {
        // #1621 (AC-6, audit gap G3). A first-deploy teardown returns the host to nothing
        // installed — that is what `CompensatedTeardown` means. Leaving
        // `shared/last-deploy` untouched made `deploy status` report `last deploy:
        // deployed <ts>` for a host carrying no release at all: a wrong value in the
        // column an operator reads first when inspecting a halted rollout. The teardown
        // records `torn down` rather than deleting the marker, because an absent marker
        // renders `last deploy: ?`, the same as a host that was never deployed, so
        // clearing it would erase exactly the fact triage needs — this host was taken back
        // down, on purpose, at this time.
        let cfg = resolved();
        let plan = SlotPlan::first(3000);
        let teardown = first_deploy_teardown_ops(&cfg, RELEASE_ID, &plan);
        let exec = RecordingExecutor::new();
        run_teardown(&teardown, &exec);

        let shell = exec
            .shell_for("teardown-last-deploy")
            .expect("teardown-last-deploy ran");
        assert!(
            shell.contains("printf '%s\\t%s' 'torn down'")
                && shell.contains("date -u +%Y-%m-%dT%H:%M:%SZ"),
            "the teardown must record `torn down` plus a UTC timestamp: {shell}"
        );
        assert!(
            shell.contains("mv -f \"$rtmp\" '/srv/autumn/myapp/shared/last-deploy'"),
            "it must land on the same marker `deploy status` reads: {shell}"
        );
        assert!(
            !shell.contains("'deployed'"),
            "a torn-down host must never claim a successful deploy: {shell}"
        );
        // ADVISORY, like the cutover's own fragment: `compensate_teardown` drives
        // these ops through `run_ops`, so a marker write that could fail would turn
        // a clean compensation into `CompensationFailed`.
        assert!(
            shell.contains("|| true; }"),
            "the teardown's last-deploy write must never be able to fail the op: {shell}"
        );

        // It runs LAST, so it only claims a teardown that actually completed: if an
        // earlier op fails, `run_ops` stops before the marker is rewritten and the
        // host keeps its previous — still true — record.
        let labels: Vec<&str> = teardown.iter().map(DeployOp::label).collect();
        assert_eq!(
            labels.last().copied(),
            Some("teardown-last-deploy"),
            "the last-deploy record must be the final teardown op: {labels:?}"
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
        // …and it must not touch the last-deploy marker either (#1621 audit gap G3):
        // a torn-down CANDIDATE leaves the PREVIOUS release serving, so the marker's
        // existing record still describes the release that is actually live.
        assert!(
            !labels.contains(&"teardown-last-deploy"),
            "redeploy teardown must not rewrite the last-deploy marker — the previous \
             release is still serving: {labels:?}"
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
        // field, the trailing port must not leak into the parsed slot (and the port
        // itself is no longer parsed into the mode — the derived port is used, proven
        // correct by the #2073 refuse guard).
        let redeploy =
            RecordingExecutor::new().with_stdout("detect-current", "redeploy:green\t3002");
        assert_eq!(
            probe_deploy_state(&cfg, &redeploy).unwrap().mode,
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN,
            }
        );
        // Backward-compat: an older slot-only marker (no port field) still parses.
        let legacy = RecordingExecutor::new().with_stdout("detect-current", "redeploy:green");
        assert_eq!(
            probe_deploy_state(&cfg, &legacy).unwrap().mode,
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN,
            }
        );
        // A missing/blank marker on a redeploy defaults the live slot to blue.
        let default_blue = RecordingExecutor::new().with_stdout("detect-current", "redeploy:");
        assert_eq!(
            probe_deploy_state(&cfg, &default_blue).unwrap().mode,
            DeployMode::Redeploy {
                live_slot: SLOT_BLUE,
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
                live_slot: SLOT_BLUE,
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
        // The list invocation must be control-socket-pinned exactly like
        // `deploy_shell` (issue #1948 item 4): without `env -u XDG_RUNTIME_DIR`
        // the SSH session's pam_systemd `XDG_RUNTIME_DIR` points the CLI at a
        // different socket than the supervised `kamal-proxy run` service, so on a
        // real pam host the list comes back silently empty — disabling the #1938
        // drift reconcile and the observed-port path. Pin it to `kamal-proxy list`.
        assert!(
            shell.contains("env -u XDG_RUNTIME_DIR kamal-proxy list"),
            "probe socket-pins the kamal-proxy list invocation: {shell}"
        );
        // No delimiter in the output (older/scripted) → empty list, mode unchanged,
        // and the installed-proxy-port degrades to Absent (never a spurious refuse).
        let legacy = RecordingExecutor::new().with_stdout("detect-current", "redeploy:green\t3002");
        let probe = probe_deploy_state(&cfg, &legacy).unwrap();
        assert_eq!(
            probe.mode,
            DeployMode::Redeploy {
                live_slot: SLOT_GREEN,
            }
        );
        assert!(
            probe.proxy_list.is_empty(),
            "no delimiter → empty proxy list"
        );
        assert_eq!(
            probe.installed_proxy_port,
            InstalledProxyPort::Absent,
            "a missing unit delimiter degrades to Absent (no spurious refuse)"
        );
    }

    #[test]
    fn probe_deploy_state_captures_the_installed_proxy_port() {
        let cfg = resolved();
        // Realistic full probe stdout: mode, the list section, then the unit section
        // carrying the installed unit's `run --http-port 80` grep result.
        let stdout = format!(
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n{}\n\
             ---autumn-kamal-proxy-unit---\n--http-port 80",
            proxy_list_serving("myapp", 3001)
        );
        let exec = RecordingExecutor::new().with_stdout("detect-current", stdout);
        let probe = probe_deploy_state(&cfg, &exec).unwrap();
        assert_eq!(
            probe.installed_proxy_port,
            InstalledProxyPort::Port(80),
            "the installed unit's --http-port is captured"
        );
        // The proxy list section is still cleanly split off (the unit delimiter does
        // not leak into it).
        assert!(
            probe.proxy_list.contains("127.0.0.1:3001")
                && !probe.proxy_list.contains("--http-port"),
            "the list section is split off from the unit section: {}",
            probe.proxy_list
        );
        // The probe shell reads the installed unit's --http-port, guarded on the unit
        // file existing, from the shared kamal-proxy unit path.
        let shell = exec.shell_for("detect-current").expect("probe ran");
        assert!(
            shell.contains("--http-port")
                && shell.contains("/etc/systemd/system/kamal-proxy.service")
                && shell.contains("[ -f "),
            "probe greps the installed unit's --http-port guarded on the file: {shell}"
        );

        // Unit file absent → the probe prints the no-unit sentinel → Absent.
        let absent = RecordingExecutor::new().with_stdout(
            "detect-current",
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n\
             ---autumn-kamal-proxy-unit---\n---autumn-no-proxy-unit---",
        );
        assert_eq!(
            probe_deploy_state(&cfg, &absent)
                .unwrap()
                .installed_proxy_port,
            InstalledProxyPort::Absent,
            "the no-unit sentinel parses to Absent"
        );

        // Unit present but no parseable --http-port (empty unit section) → Unreadable
        // (fail closed).
        let unreadable = RecordingExecutor::new().with_stdout(
            "detect-current",
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n\
             ---autumn-kamal-proxy-unit---\n",
        );
        assert_eq!(
            probe_deploy_state(&cfg, &unreadable)
                .unwrap()
                .installed_proxy_port,
            InstalledProxyPort::Unreadable,
            "a present unit with no readable --http-port fails closed as Unreadable"
        );
    }

    #[test]
    fn probe_release_dir_is_read_only_and_fails_closed() {
        // #1621 (§4.9): the release id has one-second granularity and exactly one
        // is minted per run, so a fast retry re-uses it. Uploading into a release
        // dir `shared/previous-release` still points at would put the NEW binary
        // behind the "previous release" and make a rollback roll FORWARD — silently
        // and undetectably. So the deploy probes for the collision up front.
        let cfg = resolved();

        let absent = RecordingExecutor::new().with_stdout("probe-release-dir", "absent");
        assert_eq!(
            probe_release_dir(&cfg, RELEASE_ID, &absent).unwrap(),
            ReleaseDirState::Absent,
            "a free release dir is the normal case"
        );

        // The probe is READ-ONLY: a `[ -d … ]` test and two printfs, nothing else,
        // aimed at this run's release dir.
        let shell = absent.shell_for("probe-release-dir").expect("probe ran");
        assert_eq!(
            shell,
            format!(
                "if [ -d '{RELEASE_DIR}' ]; then printf '%s' 'present'; \
                     else printf '%s' 'absent'; fi"
            ),
            "the collision probe must be a read-only directory test",
        );
        assert_eq!(
            absent.run_labels(),
            vec!["probe-release-dir"],
            "the probe runs exactly one command and mutates nothing"
        );

        let present = RecordingExecutor::new().with_stdout("probe-release-dir", "present\n");
        assert_eq!(
            probe_release_dir(&cfg, RELEASE_ID, &present).unwrap(),
            ReleaseDirState::Present,
            "an existing release dir is reported"
        );

        // Neither sentinel → we cannot PROVE the dir is free, and the failure mode
        // is destructive, so fail closed rather than degrade to Absent.
        let garbled = RecordingExecutor::new().with_stdout("probe-release-dir", "bash: -c: line 0");
        assert_eq!(
            probe_release_dir(&cfg, RELEASE_ID, &garbled).unwrap(),
            ReleaseDirState::Unreadable,
            "an unexpected capture must fail closed, never degrade to Absent"
        );
        let empty = RecordingExecutor::new();
        assert_eq!(
            probe_release_dir(&cfg, RELEASE_ID, &empty).unwrap(),
            ReleaseDirState::Unreadable,
            "empty output must fail closed (the trap every other probe degrades on)"
        );
    }

    #[test]
    fn probe_rollback_target_dir_is_read_only_and_distinctly_labelled() {
        // #1621 (§4.7): before a fleet compensation flips a host back, it proves the
        // release dir it is about to point at still exists — `prune` runs per host,
        // so the previous-release marker can legitimately name a dir this host no
        // longer has. Rolling back to a removed dir writes a unit whose ExecStart
        // points nowhere, starts "successfully", and then fails the readiness gate
        // POST-boundary with no teardown: a second broken host.
        let previous = "/srv/autumn/myapp/releases/20260101T000000Z";

        let present = RecordingExecutor::new().with_stdout("probe-rollback-target", "present");
        assert_eq!(
            probe_rollback_target_dir(previous, &present).unwrap(),
            ReleaseDirState::Present,
            "a retained rollback target is the normal case"
        );
        assert_eq!(
            present
                .shell_for("probe-rollback-target")
                .expect("probe ran"),
            format!(
                "if [ -d '{previous}' ]; then printf '%s' 'present'; \
                 else printf '%s' 'absent'; fi"
            ),
            "the target probe must be the same read-only directory test",
        );
        assert_eq!(
            present.run_labels(),
            vec!["probe-rollback-target"],
            "the probe runs exactly one command, under its OWN label so a tape can \
             tell it apart from the candidate-dir collision probe, and mutates nothing"
        );

        let absent = RecordingExecutor::new().with_stdout("probe-rollback-target", "absent\n");
        assert_eq!(
            probe_rollback_target_dir(previous, &absent).unwrap(),
            ReleaseDirState::Absent,
            "a pruned rollback target must be reported so the caller declines"
        );
        // Fail closed in the SAME direction as every other probe: proving nothing is
        // not proving it is there.
        let garbled =
            RecordingExecutor::new().with_stdout("probe-rollback-target", "bash: -c: line 0");
        assert_eq!(
            probe_rollback_target_dir(previous, &garbled).unwrap(),
            ReleaseDirState::Unreadable,
            "an unexpected capture must fail closed, never degrade to Present"
        );
    }

    #[test]
    fn strict_recording_executor_refuses_to_fake_an_unscripted_probe() {
        // #1621 (plan §9.2): the fake returns Ok+EMPTY stdout for anything
        // unscripted, and every probe parser reads empty as "absent / first
        // deploy". A fleet test that forgot to script host N's probe would
        // therefore exercise the first-deploy branch and PASS. Strict mode turns
        // that silent hole into a panic.
        let cfg = resolved();
        let strict = RecordingExecutor::new().strict();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = probe_deploy_state(&cfg, &strict);
        }));
        assert!(
            panicked.is_err(),
            "an unscripted probe must fail loudly under strict mode"
        );

        // A scripted probe is unaffected, and non-probe labels never need scripting.
        let scripted = RecordingExecutor::new()
            .strict()
            .with_stdout("detect-current", "first\n");
        assert_eq!(
            probe_deploy_state(&cfg, &scripted).unwrap().mode,
            DeployMode::First,
            "strict mode changes nothing for a scripted probe"
        );
        assert!(
            run_ops(
                &[DeployOp::Run(RemoteCommand::new("prune", "true"))],
                &RecordingExecutor::new().strict()
            )
            .is_ok(),
            "strict mode only guards labels whose stdout is parsed"
        );
    }

    #[test]
    fn parse_installed_proxy_port_classifies_the_unit_section() {
        // Sentinel → Absent.
        assert_eq!(
            parse_installed_proxy_port("---autumn-no-proxy-unit---"),
            InstalledProxyPort::Absent
        );
        // A single `--http-port N` match → Port(N) (whitespace tolerant).
        assert_eq!(
            parse_installed_proxy_port("--http-port 80"),
            InstalledProxyPort::Port(80)
        );
        assert_eq!(
            parse_installed_proxy_port("  --http-port    3000  "),
            InstalledProxyPort::Port(3000)
        );
        // Empty (unit present, no match) → Unreadable (fail closed).
        assert_eq!(
            parse_installed_proxy_port("   "),
            InstalledProxyPort::Unreadable
        );
        // Ambiguous (two matches) → Unreadable (fail closed).
        assert_eq!(
            parse_installed_proxy_port("--http-port 80\n--http-port 8080"),
            InstalledProxyPort::Unreadable
        );
        // Non-u16 / overflow → Unreadable.
        assert_eq!(
            parse_installed_proxy_port("--http-port 99999"),
            InstalledProxyPort::Unreadable
        );
    }

    // --- proxy-options marker (issue #2074) ----------------------------------

    #[test]
    fn proxy_options_marker_round_trips_through_serialize_and_parse() {
        // TLS on: `1\t<host>` serializes and parses back to the same options.
        let tls_on = ProxyServiceOptions {
            tls: true,
            host: Some("app.example.com".to_owned()),
        };
        let on_marker = tls_on.marker_value();
        assert_eq!(on_marker, "1\tapp.example.com");
        assert_eq!(
            parse_proxy_options(&on_marker),
            ProxyOptionsMarker::Options(tls_on),
        );

        // TLS off (host removed): `0\t` serializes and parses back to TLS-off.
        let tls_off = ProxyServiceOptions {
            tls: false,
            host: None,
        };
        assert_eq!(tls_off.marker_value(), "0\t");
        assert_eq!(
            parse_proxy_options(&tls_off.marker_value()),
            ProxyOptionsMarker::Options(tls_off.clone()),
        );

        // A surrounding newline (as `cat` would yield through the probe wrapping) is
        // tolerated without dropping the trailing tab of a TLS-off marker.
        assert_eq!(
            parse_proxy_options(&format!("\n{}\n", tls_off.marker_value())),
            ProxyOptionsMarker::Options(tls_off),
        );
    }

    #[test]
    fn record_proxy_options_writes_the_marker_atomically() {
        let cfg = resolved();
        // TLS-on: the op mktemp's in shared/, printfs the `1\t<host>` value, and mv's
        // it onto shared/proxy-options (atomic same-fs rename, never a truncating `>`).
        let cmd = record_proxy_options(
            &cfg,
            &ProxyServiceOptions {
                tls: true,
                host: Some("app.example.com".to_owned()),
            },
        );
        assert_eq!(cmd.label, "record-proxy-options");
        assert!(
            cmd.shell
                .contains("mktemp '/srv/autumn/myapp/shared/proxy-options.tmp.XXXXXX'"),
            "mktemp's a temp file in the marker's own shared dir: {}",
            cmd.shell,
        );
        assert!(
            cmd.shell.contains("printf '%s' '1\tapp.example.com'"),
            "writes the {{tls}}\\t{{host}} marker value: {}",
            cmd.shell,
        );
        assert!(
            cmd.shell
                .contains("mv -f \"$otmp\" '/srv/autumn/myapp/shared/proxy-options'"),
            "mv's the temp file onto the final marker path (atomic): {}",
            cmd.shell,
        );
        // TLS-off writes the host-less `0\t` form.
        let off = record_proxy_options(
            &cfg,
            &ProxyServiceOptions {
                tls: false,
                host: None,
            },
        );
        assert!(
            off.shell.contains("printf '%s' '0\t'"),
            "TLS-off writes `0\\t` (empty host — a removed host is representable): {}",
            off.shell,
        );
    }

    #[test]
    fn parse_proxy_options_classifies_the_marker_section() {
        // Empty (absent file, or an empty marker) → Absent (proceed as legacy).
        assert_eq!(parse_proxy_options(""), ProxyOptionsMarker::Absent);
        assert_eq!(parse_proxy_options("\n\n"), ProxyOptionsMarker::Absent);

        // TLS-on with a host → Options{tls:true}.
        assert_eq!(
            parse_proxy_options("1\ta.com"),
            ProxyOptionsMarker::Options(ProxyServiceOptions {
                tls: true,
                host: Some("a.com".to_owned()),
            }),
        );
        // TLS-off → Options{tls:false, host:None}.
        assert_eq!(
            parse_proxy_options("0\t"),
            ProxyOptionsMarker::Options(ProxyServiceOptions {
                tls: false,
                host: None,
            }),
        );

        // Fail-closed shapes → Unreadable:
        //  - no tab at all (a fieldless / truncated marker),
        assert_eq!(
            parse_proxy_options("garbage"),
            ProxyOptionsMarker::Unreadable
        );
        //  - a non-{0,1} TLS token,
        assert_eq!(parse_proxy_options("2\tx"), ProxyOptionsMarker::Unreadable);
        //  - TLS-on with an EMPTY host (can't preserve a host we don't have).
        assert_eq!(parse_proxy_options("1\t"), ProxyOptionsMarker::Unreadable);
    }

    #[test]
    fn cutover_records_proxy_options_after_the_marker_commit() {
        // The cutover appends a `record-proxy-options` op AFTER `commit-markers`
        // (post-flip), persisting the options THIS deploy registered so the next
        // redeploy can preserve them (issue #2074).
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        let labels = exec.run_labels();
        let commit = labels
            .iter()
            .position(|&l| l == "commit-markers")
            .expect("commit-markers ran");
        let record = labels
            .iter()
            .position(|&l| l == "record-proxy-options")
            .expect("record-proxy-options ran");
        assert!(
            commit < record,
            "record-proxy-options is written after the marker commit: {labels:?}",
        );
        // The default `proxy()` is TLS-off, so the recorded value is `0\t`.
        let shell = exec
            .shell_for("record-proxy-options")
            .expect("record-proxy-options ran");
        assert!(
            shell.contains("printf '%s' '0\t'"),
            "records the TLS-off options the default controller registered: {shell}",
        );
    }

    #[test]
    fn first_deploy_records_proxy_options() {
        // The first-deploy path also writes the marker (after record-live-slot), so a
        // brand-new host is protected from its second redeploy onward (issue #2074).
        let ops = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        let labels = exec.run_labels();
        let live = labels
            .iter()
            .position(|&l| l == "record-live-slot")
            .expect("record-live-slot ran");
        let record = labels
            .iter()
            .position(|&l| l == "record-proxy-options")
            .expect("record-proxy-options ran on first deploy");
        assert!(
            live < record,
            "record-proxy-options follows record-live-slot on first deploy: {labels:?}",
        );
    }

    #[test]
    fn probe_deploy_state_captures_the_proxy_options_marker() {
        let cfg = resolved();
        // Full probe stdout: mode, list, unit `--http-port`, then the proxy-options
        // section carrying a TLS-on marker.
        let stdout = format!(
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n{}\n\
             ---autumn-kamal-proxy-unit---\n--http-port 80\n\
             ---autumn-kamal-proxy-options---\n1\tapp.example.com",
            proxy_list_serving("myapp", 3001)
        );
        let exec = RecordingExecutor::new().with_stdout("detect-current", stdout);
        let probe = probe_deploy_state(&cfg, &exec).unwrap();
        assert_eq!(
            probe.last_proxy_options,
            ProxyOptionsMarker::Options(ProxyServiceOptions {
                tls: true,
                host: Some("app.example.com".to_owned()),
            }),
            "the proxy-options marker is captured",
        );
        // The unit `--http-port` section is still cleanly split off from the options.
        assert_eq!(probe.installed_proxy_port, InstalledProxyPort::Port(80));
        // The probe shell cats the proxy-options marker behind its delimiter.
        let shell = exec.shell_for("detect-current").expect("probe ran");
        assert!(
            shell.contains("---autumn-kamal-proxy-options---")
                && shell.contains("cat '/srv/autumn/myapp/shared/proxy-options'"),
            "probe cats the proxy-options marker behind its delimiter: {shell}",
        );

        // A missing options delimiter (older recorded output) → Absent (legacy).
        let legacy = RecordingExecutor::new().with_stdout(
            "detect-current",
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n\
             ---autumn-kamal-proxy-unit---\n--http-port 80",
        );
        assert_eq!(
            probe_deploy_state(&cfg, &legacy)
                .unwrap()
                .last_proxy_options,
            ProxyOptionsMarker::Absent,
            "a missing options delimiter degrades to Absent (proceed as legacy)",
        );

        // Present-but-unparseable marker → Unreadable (fail closed).
        let unreadable = RecordingExecutor::new().with_stdout(
            "detect-current",
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n\
             ---autumn-kamal-proxy-unit---\n--http-port 80\n\
             ---autumn-kamal-proxy-options---\ngarbage",
        );
        assert_eq!(
            probe_deploy_state(&cfg, &unreadable)
                .unwrap()
                .last_proxy_options,
            ProxyOptionsMarker::Unreadable,
            "an unparseable proxy-options marker fails closed as Unreadable",
        );
    }

    #[test]
    fn probe_deploy_state_captures_the_current_release_dir() {
        // #1621 (AC-6, plan §7.1): `autumn deploy status` needs each host's DEPLOYED
        // RELEASE, and the only honest source is the `current` symlink's target — no
        // runtime endpoint reports a release id (the probe body's `version` is the
        // FRAMEWORK crate version, identical on every host forever). It rides as a
        // FIFTH delimited section on the existing probe, so `status` costs the same
        // one ssh round-trip `up` already pays and it works retroactively on hosts
        // deployed before this feature.
        let cfg = resolved();
        let stdout = format!(
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n{}\n\
             ---autumn-kamal-proxy-unit---\n--http-port 80\n\
             ---autumn-kamal-proxy-options---\n0\t\n\
             ---autumn-current-release---\n/srv/autumn/myapp/releases/20260714T120000Z",
            proxy_list_serving("myapp", 3001)
        );
        let exec = RecordingExecutor::new().with_stdout("detect-current", stdout);
        let probe = probe_deploy_state(&cfg, &exec).unwrap();
        assert_eq!(
            probe.current_release_dir.as_deref(),
            Some("/srv/autumn/myapp/releases/20260714T120000Z"),
        );
        assert_eq!(
            release_id_from_dir(probe.current_release_dir.as_deref().unwrap()),
            Some("20260714T120000Z"),
            "the release id is the release dir's basename"
        );
        // The earlier sections are still cleanly split off — the new delimiter must
        // not leak into the proxy-options marker.
        assert_eq!(
            probe.last_proxy_options,
            ProxyOptionsMarker::Options(ProxyServiceOptions {
                tls: false,
                host: None,
            })
        );
        assert_eq!(probe.installed_proxy_port, InstalledProxyPort::Port(80));

        // The probe shell resolves the symlink behind its own delimiter, best-effort.
        let shell = exec.shell_for("detect-current").expect("probe ran");
        assert!(
            shell.contains("---autumn-current-release---")
                && shell.contains("readlink -f '/srv/autumn/myapp/current'"),
            "probe resolves the current symlink behind its delimiter: {shell}"
        );
        // The LABEL is unchanged: it is load-bearing for every exact-vector test and
        // for the strict test fake's probe allow-list.
        assert_eq!(
            exec.run_labels(),
            vec!["detect-current"],
            "the probe's op label must not change when a section is added"
        );
    }

    #[test]
    fn probe_deploy_state_degrades_when_the_current_release_section_is_missing() {
        // #1621: a host deployed before this feature (or a scripted test that stubs
        // only the earlier sections) has no fifth delimiter. That degrades to
        // "unknown", exactly like every other section — never to a guessed release id,
        // because drift reporting treats Unknown as a DISTINCT state and never as
        // drift. A false "these hosts differ" at 3 am is worse than no warning.
        let cfg = resolved();
        let legacy = RecordingExecutor::new().with_stdout(
            "detect-current",
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n\
             ---autumn-kamal-proxy-unit---\n--http-port 80",
        );
        assert!(
            probe_deploy_state(&cfg, &legacy)
                .unwrap()
                .current_release_dir
                .is_none(),
            "a missing fifth delimiter degrades to unknown"
        );

        // Present but empty (`readlink` on a dangling/absent symlink prints nothing).
        let empty = RecordingExecutor::new().with_stdout(
            "detect-current",
            "first\n---autumn-kamal-proxy-list---\n\
             ---autumn-kamal-proxy-unit---\n---autumn-no-proxy-unit---\n\
             ---autumn-kamal-proxy-options---\n\n---autumn-current-release---\n",
        );
        let probe = probe_deploy_state(&cfg, &empty).unwrap();
        assert_eq!(probe.mode, DeployMode::First);
        assert!(
            probe.current_release_dir.is_none(),
            "an empty current section is unknown, not an empty release id"
        );
    }

    /// Full five-section `detect-current` stdout for a redeploy host on `release`.
    fn status_probe_stdout(release: &str, live_port: u16) -> String {
        format!(
            "redeploy:blue\t{live_port}\n---autumn-kamal-proxy-list---\n{}\n\
             ---autumn-kamal-proxy-unit---\n--http-port 3000\n\
             ---autumn-kamal-proxy-options---\n0\t\n\
             ---autumn-current-release---\n/srv/autumn/myapp/releases/{release}",
            proxy_list_serving("myapp", live_port)
        )
    }

    /// #1621 AC-6, third per-host fact. `deploy status` must report the last
    /// deploy RESULT, and no other on-host artefact answers it — `current`,
    /// `live-slot` and `previous-release` all describe which release a host
    /// serves, never how it got there, so a cleanly compensated halt reads back as
    /// a healthy converged fleet. The marker is written by the ops that COMPLETE a
    /// cutover, so it rides on round-trips that already happen.
    #[test]
    fn the_cutover_and_the_rollback_record_the_last_deploy_result() {
        let cfg = resolved();
        let marker = "'/srv/autumn/myapp/shared/last-deploy'";

        // Forward cutover: `commit-markers` (the transaction that completes the
        // flip) records "deployed", and so does `record-proxy-options` — the last
        // marker write on BOTH forward paths, which is how a FIRST deploy (whose
        // ops contain no `commit-markers`) gets one too.
        let ops = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let exec = RecordingExecutor::new();
        run_ops(&ops, &exec).expect("recording executor never fails");
        for label in ["commit-markers", "record-proxy-options"] {
            let shell = exec.shell_for(label).expect("op ran");
            assert!(
                shell.contains("mktemp '/srv/autumn/myapp/shared/last-deploy.tmp.XXXXXX'")
                    && shell.contains(&format!("mv -f \"$rtmp\" {marker}")),
                "{label} must write the last-deploy marker atomically: {shell}"
            );
            assert!(
                shell.contains("printf '%s\\t%s' 'deployed'")
                    && shell.contains("date -u +%Y-%m-%dT%H:%M:%SZ"),
                "{label} must record `deployed` plus a UTC timestamp: {shell}"
            );
            // ADVISORY: the write is a `{ … || true; }` group, so it can never
            // fail the op it rides on. That matters most on `commit-markers`,
            // whose failure is the one the fleet driver refuses to auto-roll-back
            // from — a cosmetic status field must not be able to push a host into
            // that state.
            assert!(
                shell.contains("|| true; }"),
                "the last-deploy write must be advisory, never able to fail its op: {shell}"
            );
        }

        // A first deploy takes the same `record-proxy-options` route.
        let first = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let first_exec = RecordingExecutor::new();
        run_ops(&first, &first_exec).expect("recording executor never fails");
        let first_record = first_exec
            .shell_for("record-proxy-options")
            .expect("first deploy records proxy options");
        assert!(
            first_record.contains("printf '%s\\t%s' 'deployed'") && first_record.contains(marker),
            "a first deploy must record its result too: {first_record}"
        );

        // Rollback records the OPPOSITE word through the same shared op, so a
        // compensated host is distinguishable from one that simply deployed —
        // exactly the state that is invisible once the rollout's output scrolls
        // away.
        let rb_exec = RecordingExecutor::new().with_stdout(
            "resolve-previous",
            "prev:/srv/autumn/myapp/releases/20260713T090000Z\tblue\t3001",
        );
        let target =
            resolve_rollback_target(&cfg, 3000, &rb_exec).expect("previous release resolves");
        let rb_ops = rollback_ops(&cfg, &proxy(), &target);
        run_ops(&rb_ops, &rb_exec).expect("recording executor never fails");
        let rb = rb_exec
            .shell_for("commit-markers")
            .expect("commit-markers ran");
        assert!(
            rb.contains("printf '%s\\t%s' 'rolled back'") && rb.contains(marker),
            "a rollback must record `rolled back`, not `deployed`: {rb}"
        );
    }

    #[test]
    fn the_status_probe_reads_the_last_deploy_marker_and_degrades_to_unknown() {
        // The marker rides on the EXISTING status round-trip — `deploy status` is
        // run fleet-wide mid-incident, so a new per-host ssh would be paid N times.
        let cfg = resolved();
        let probe_with = |stdout: &str| {
            let exec = RecordingExecutor::new()
                .with_stdout(
                    "detect-current",
                    status_probe_stdout("20260714T120000Z", 3001),
                )
                .with_stdout("probe-host-status", stdout.to_owned());
            (probe_host_status(&cfg, 3000, &exec).unwrap(), exec)
        };

        let (probe, exec) = probe_with(
            "200\n---autumn-host-status---\n\n---autumn-host-status---\n\
             rolled back\t2026-07-14T12:31:10Z",
        );
        assert_eq!(
            probe.last_deploy,
            Some(LastDeploy {
                result: "rolled back".to_owned(),
                at: Some("2026-07-14T12:31:10Z".to_owned()),
            })
        );
        // Still read-only, still one round-trip, and still the shared path.
        let shell = exec
            .shell_for("probe-host-status")
            .expect("status probe ran");
        assert!(
            shell.contains("cat '/srv/autumn/myapp/shared/last-deploy'"),
            "the marker is read from the release-independent shared dir: {shell}"
        );
        assert_eq!(
            exec.run_labels(),
            vec!["detect-current", "probe-host-status"]
        );

        // A marker that predates the timestamp field still reports its result.
        let (older, _) =
            probe_with("200\n---autumn-host-status---\n\n---autumn-host-status---\ndeployed");
        assert_eq!(
            older.last_deploy,
            Some(LastDeploy {
                result: "deployed".to_owned(),
                at: None,
            })
        );

        // Absent/empty marker, and a host whose probe shape predates the section:
        // both are "we could not tell", never a fabricated result.
        for stdout in [
            "200\n---autumn-host-status---\n\n---autumn-host-status---\n",
            "200\n---autumn-host-status---\nmaintenance-on",
        ] {
            let (unknown, _) = probe_with(stdout);
            assert_eq!(
                unknown.last_deploy, None,
                "an unreadable marker must not be reported as a result: {stdout:?}"
            );
        }
    }

    #[test]
    fn probe_host_status_consults_the_live_slot_unit_for_the_maintenance_flag_path() {
        // #1621 review round 1 (Codex 2). The status probe used to read only the shared
        // flag path. A host still running a slot unit rendered before #1621 has no
        // `Environment=AUTUMN_MAINTENANCE_FLAG_FILE=` line, so the app it runs polls the
        // cwd-relative, release-local `tmp/autumn-maintenance.json` instead — and `deploy
        // status` reported the shared path's state for it anyway. It could print
        // `maintenance off` for a host that is actually maintained, and `maintenance ON`
        // for one whose legacy write failed and which is therefore still taking traffic.
        // The probe must instead ask the live slot unit which file the running app polls,
        // resolving it exactly as `maintenance::flag_file_path_from` does — the override
        // when set, else `WorkingDirectory` plus the legacy relative path — and report
        // that file's presence.
        let cfg = resolved();
        let exec = RecordingExecutor::new()
            .with_stdout("detect-current", status_probe_stdout(RELEASE_ID, 3001))
            .with_stdout("probe-host-status", "200\n---autumn-host-status---\n");
        probe_host_status(&cfg, 3000, &exec).expect("status probe runs");
        let shell = exec
            .shell_for("probe-host-status")
            .expect("status probe ran");
        assert!(
            shell.contains("'/etc/systemd/system/myapp-blue.service'"),
            "the status probe must read the LIVE SLOT UNIT to learn which flag file \
             the running app polls: {shell}"
        );
        assert!(
            shell.contains(autumn_web::maintenance::MAINTENANCE_FLAG_FILE_ENV),
            "the probe must resolve the unit's flag-file override: {shell}"
        );
        assert!(
            shell.contains(autumn_web::maintenance::MAINTENANCE_FLAG_FILE),
            "the probe must fall back to the legacy cwd-relative path for a unit \
             that declares no override: {shell}"
        );
        // Still read-only: exactly the two probe labels, nothing mutating.
        assert_eq!(
            exec.run_labels(),
            vec!["detect-current", "probe-host-status"],
            "`deploy status` must never mutate a host"
        );
    }

    #[test]
    fn the_status_read_and_the_maintenance_write_resolve_the_flag_with_one_shell() {
        // #1621 review round 3. `deploy status` REPORTS the flag file the live slot
        // unit polls; the `deploy maintenance` fan-out WRITES it. While the write
        // path derived its path from the `current` symlink instead, the two
        // disagreed exactly when it matters — a proxy flip that landed with a
        // `commit-markers` that did not leaves `current` naming another release than
        // the unit that is running — so `maintenance on` could report success while
        // the application carried on serving traffic. One shell fragment, used by
        // both, is what makes that class of drift unrepresentable.
        let cfg = resolved();
        let exec = RecordingExecutor::new()
            .with_stdout("detect-current", status_probe_stdout(RELEASE_ID, 3001))
            .with_stdout("probe-host-status", "200\n---autumn-host-status---\n")
            .with_stdout(
                "detect-maintenance-flag",
                "/srv/autumn/myapp/releases/r9/tmp/autumn-maintenance.json\n",
            );
        probe_host_status(&cfg, 3000, &exec).expect("status probe runs");
        let status_shell = exec
            .shell_for("probe-host-status")
            .expect("status probe ran");
        let resolution = live_maintenance_flag_shell(&cfg, SLOT_BLUE);
        assert!(
            status_shell.contains(&resolution),
            "`deploy status` must resolve the flag through the shared fragment: \
             {status_shell}"
        );

        let flag = probe_live_maintenance_flag(&cfg, SLOT_BLUE, &exec)
            .expect("the flag probe runs")
            .expect("the unit resolved a path");
        assert_eq!(
            exec.shell_for("detect-maintenance-flag"),
            Some(resolution),
            "the write path must run the SAME resolution, not a second copy of it"
        );
        assert_eq!(
            flag.path, "/srv/autumn/myapp/releases/r9/tmp/autumn-maintenance.json",
            "the resolved path is the unit's, whatever `current` happens to say"
        );
        assert!(!flag.present, "no sentinel line means the file is absent");

        // Fails closed: an unreadable unit prints nothing, and that is never
        // degraded into "the shared path".
        let blank = RecordingExecutor::new().with_stdout("detect-maintenance-flag", "");
        assert_eq!(
            probe_live_maintenance_flag(&cfg, SLOT_BLUE, &blank).expect("the probe runs"),
            None,
            "an unreadable unit proves nothing about which file the app polls"
        );
    }

    /// Build a `probe-host-status` capture from its four sections: the `/ready`
    /// code, the shared-flag sentinel, the last-deploy marker, and the resolved
    /// unit flag path + its own presence sentinel.
    fn host_status_capture(ready: &str, shared_on: bool, unit_section: &str) -> String {
        format!(
            "{ready}\n{HOST_STATUS_DELIM}\n{shared}\n{HOST_STATUS_DELIM}\n\n{unit_section}",
            shared = if shared_on {
                MAINTENANCE_ON_SENTINEL
            } else {
                ""
            },
        )
    }

    #[test]
    fn probe_host_status_reports_the_flag_state_the_running_unit_observes() {
        // #1621 review round 1 (Codex 2), the semantics. In every case the reported
        // state is the one the RUNNING unit sees, not the one the shared path holds.
        let cfg = resolved();
        let legacy_flag = format!(
            "{RELEASE_DIR}/{}",
            autumn_web::maintenance::MAINTENANCE_FLAG_FILE
        );
        let probe_with_sections = |shared_on: bool, unit_section: String| {
            let exec = RecordingExecutor::new()
                .with_stdout("detect-current", status_probe_stdout(RELEASE_ID, 3001))
                .with_stdout(
                    "probe-host-status",
                    host_status_capture("200", shared_on, &unit_section),
                );
            probe_host_status(&cfg, 3000, &exec).expect("status probe runs")
        };

        // 1. A pre-#1621 unit whose LEGACY flag is present while the shared flag is
        //    absent: the app IS maintained. Reporting `off` here is the defect.
        let maintained = probe_with_sections(
            false,
            format!("{HOST_STATUS_DELIM}\n{legacy_flag}\n{MAINTENANCE_ON_SENTINEL}"),
        );
        assert_eq!(maintained.maintenance, MaintenanceStatus::On);
        assert_eq!(
            maintained.maintenance_flag_source,
            MaintenanceFlagSource::Unshared,
            "a unit with no override polls a path the fleet switch does not own",
        );

        // 2. The same host after a `maintenance on` whose legacy write FAILED: the
        //    shared flag exists but the running app never sees it, so the host is
        //    still taking traffic. `off` is the truthful answer — a confident `ON`
        //    would send the operator into a live window believing it closed.
        let shared_only =
            probe_with_sections(true, format!("{HOST_STATUS_DELIM}\n{legacy_flag}\n"));
        assert_eq!(shared_only.maintenance, MaintenanceStatus::Off);
        assert_eq!(
            shared_only.maintenance_flag_source,
            MaintenanceFlagSource::Unshared
        );

        // 3. A #1621 unit: the resolved path IS the shared one, and its state is
        //    reported with confidence — the pre-existing behaviour, unchanged.
        let shared_path = cfg.maintenance_flag_file();
        let modern = probe_with_sections(
            true,
            format!("{HOST_STATUS_DELIM}\n{shared_path}\n{MAINTENANCE_ON_SENTINEL}"),
        );
        assert_eq!(modern.maintenance, MaintenanceStatus::On);
        assert_eq!(
            modern.maintenance_flag_source,
            MaintenanceFlagSource::Shared
        );
        let cleared = probe_with_sections(false, format!("{HOST_STATUS_DELIM}\n{shared_path}\n"));
        assert_eq!(cleared.maintenance, MaintenanceStatus::Off);
        assert_eq!(
            cleared.maintenance_flag_source,
            MaintenanceFlagSource::Shared
        );
    }

    #[test]
    fn probe_host_status_fails_closed_when_the_flag_path_cannot_be_proved() {
        // #1621 review round 1 (Codex 2), the fail-closed half. A host whose live
        // slot unit could not be read at all (absent, unreadable, or a capture that
        // predates this section) leaves the CLI unable to say WHICH file the running
        // app polls. That is "we could not tell" — never a confident `ON`/`off`,
        // even though the shared flag below is set.
        let cfg = resolved();
        for unit_section in [
            // No unit section at all (a capture from a CLI predating this probe).
            String::new(),
            // Section present but empty: no unit file on the host.
            format!("{HOST_STATUS_DELIM}\n"),
            // Section present but the path line is blank: the unit declared neither
            // an override nor a `WorkingDirectory`, so nothing was probed.
            format!("{HOST_STATUS_DELIM}\n   \n"),
        ] {
            let exec = RecordingExecutor::new()
                .with_stdout("detect-current", status_probe_stdout(RELEASE_ID, 3001))
                .with_stdout(
                    "probe-host-status",
                    host_status_capture("200", true, &unit_section),
                );
            let probe = probe_host_status(&cfg, 3000, &exec).expect("status probe runs");
            assert_eq!(
                probe.maintenance,
                MaintenanceStatus::Unknown,
                "an unprovable flag path must not render as a confident on/off: \
                 {unit_section:?}"
            );
            assert_eq!(
                probe.maintenance_flag_source,
                MaintenanceFlagSource::Unknown
            );
        }
    }

    #[test]
    fn probe_host_status_reads_readiness_and_maintenance_off_the_live_slot() {
        // #1621 (AC-6, plan §7.1): `deploy status` needs two facts `up` must never
        // pay for — the live slot's `/ready` code and whether the SHARED maintenance
        // flag exists. They ride on their own labelled round-trip so
        // `probe_deploy_state` (and therefore every `deploy up`) is unchanged.
        let cfg = resolved();
        let exec = RecordingExecutor::new()
            .with_stdout(
                "detect-current",
                status_probe_stdout("20260714T120000Z", 3001),
            )
            .with_stdout(
                "probe-host-status",
                format!(
                    "200\n---autumn-host-status---\nmaintenance-on\n---autumn-host-status---\n\n\
                     ---autumn-host-status---\n{}\nmaintenance-on",
                    resolved().maintenance_flag_file()
                ),
            );
        let probe = probe_host_status(&cfg, 3000, &exec).unwrap();
        assert_eq!(probe.ready_code, Some(200));
        assert!(probe.shared_maintenance_flag);
        assert_eq!(probe.maintenance, MaintenanceStatus::On);
        assert_eq!(
            probe.deploy.current_release_dir.as_deref(),
            Some("/srv/autumn/myapp/releases/20260714T120000Z")
        );

        // It polls the LIVE slot's loopback port (blue = public + 1), not the public
        // port — the public port is kamal-proxy's, and asking it would report the
        // proxy's health rather than the release's.
        let shell = exec
            .shell_for("probe-host-status")
            .expect("status probe ran");
        assert!(
            shell.contains("http://127.0.0.1:3001/ready"),
            "readiness is polled on the live slot's loopback port: {shell}"
        );
        // Bounded, so one hung app cannot stall a fleet-wide status.
        assert!(shell.contains("-m 5"), "the curl must be bounded: {shell}");
        // The maintenance flag is read from the RELEASE-INDEPENDENT shared path —
        // reading a release dir would report "off" for every host after a cutover,
        // which is the very defect the shared path exists to fix.
        assert!(
            shell.contains("'/srv/autumn/myapp/shared/autumn-maintenance.json'"),
            "the maintenance flag is read from the shared dir: {shell}"
        );
        assert!(
            !shell.contains("/releases/"),
            "the status probe must not look for a release-scoped flag: {shell}"
        );
        // Read-only: exactly the two probe labels, nothing mutating.
        assert_eq!(
            exec.run_labels(),
            vec!["detect-current", "probe-host-status"],
            "`deploy status` must never mutate a host"
        );
    }

    #[test]
    fn probe_host_status_degrades_to_unknown_readiness_rather_than_failing() {
        // A host with no `curl`, a refused connection, or a timeout writes `000`
        // (or nothing). That is "we could not tell" — reporting it as an HTTP status
        // would be a lie, and failing the probe would make `deploy status` useless on
        // exactly the broken host it exists to surface.
        let cfg = resolved();
        for capture in [
            "000\n---autumn-host-status---\n",
            "---autumn-host-status---\n",
            "",
        ] {
            let exec = RecordingExecutor::new()
                .with_stdout("detect-current", status_probe_stdout("r1", 3001))
                .with_stdout("probe-host-status", capture);
            let probe = probe_host_status(&cfg, 3000, &exec).unwrap();
            assert_eq!(
                probe.ready_code, None,
                "capture {capture:?} must degrade to unknown readiness"
            );
            assert!(
                !probe.shared_maintenance_flag,
                "no sentinel means the shared flag is absent, never unknown-as-on"
            );
            // …and with no unit section at all the VERDICT is `Unknown`, not a
            // confident `off` (review round 1, Codex 2).
            assert_eq!(probe.maintenance, MaintenanceStatus::Unknown);
        }
    }

    #[test]
    fn probe_host_status_polls_the_slot_the_proxy_is_actually_serving() {
        // The live-slot marker can drift from what kamal-proxy serves (an interrupted
        // post-flip marker write). `status` reconciles exactly like the rollout path
        // does, so it reports the slot a deploy would plan from — polling the marker's
        // slot instead would report the IDLE release's readiness.
        let cfg = resolved();
        let drifted = format!(
            "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n{}\n\
             ---autumn-kamal-proxy-unit---\n--http-port 3000\n\
             ---autumn-kamal-proxy-options---\n0\t\n\
             ---autumn-current-release---\n/srv/autumn/myapp/releases/r1",
            // the proxy is serving GREEN (public + 2) while the marker says blue
            proxy_list_serving("myapp", 3002)
        );
        let exec = RecordingExecutor::new()
            .with_stdout("detect-current", drifted)
            .with_stdout("probe-host-status", "200\n---autumn-host-status---\n");
        let probe = probe_host_status(&cfg, 3000, &exec).unwrap();
        let reconcile = probe
            .reconcile(&cfg, 3000)
            .expect("a redeploy host reconciles");
        assert_eq!(reconcile.live_slot, SLOT_GREEN);
        assert!(reconcile.repair, "the disagreement is reported as drift");
        let shell = exec
            .shell_for("probe-host-status")
            .expect("status probe ran");
        assert!(
            shell.contains("http://127.0.0.1:3002/ready"),
            "readiness must follow the PROXY's slot, not the marker's: {shell}"
        );
    }

    #[test]
    fn release_id_from_dir_takes_the_basename_and_rejects_junk() {
        assert_eq!(
            release_id_from_dir("/srv/autumn/myapp/releases/20260714T120000Z"),
            Some("20260714T120000Z")
        );
        // Trailing slash (some readlink/shell shapes) still yields the id.
        assert_eq!(
            release_id_from_dir("/srv/autumn/myapp/releases/20260714T120000Z/"),
            Some("20260714T120000Z")
        );
        assert_eq!(release_id_from_dir(""), None);
        assert_eq!(release_id_from_dir("/"), None);
    }

    /// A compatible `kamal-proxy deploy --help` capture for the compat probe tests.
    fn compatible_deploy_help() -> &'static str {
        "Usage:\n  kamal-proxy deploy SERVICE [flags]\n\nFlags:\n  \
         --target host:port\n  --health-check-path string\n  --host strings\n  \
         --tls\n  --deploy-timeout duration\n  --drain-timeout duration\n  \
         --force\n"
    }

    #[test]
    fn a_controller_with_no_compat_probe_is_never_gated_or_probed() {
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
            // compat_probe() and binary_install_ops() use the trait defaults → None.
        }
        let exec = RecordingExecutor::new();
        assert_eq!(
            assess_proxy_readiness(&NoProbeController, &exec, ProxyProvisioning::Auto)
                .expect("a controller with no probe is never gated"),
            ProxyReadiness::Ready
        );
        assert!(
            exec.run_labels().is_empty(),
            "no probe declared → no remote command runs"
        );
    }

    #[test]
    fn a_drifted_cli_surface_fails_closed_with_the_flag_and_the_pin() {
        // #2053: a future kamal-proxy that renamed --drain-timeout fails closed with
        // an actionable message BEFORE any cutover op runs.
        let drifted = compatible_deploy_help().replace("--drain-timeout", "--drain-window");
        let exec = RecordingExecutor::new().with_stdout("proxy-compat-probe", drifted);
        let err = assess_proxy_readiness(&proxy(), &exec, ProxyProvisioning::Auto)
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
    fn pre_migrate_labels_match_both_builders() {
        // `PRE_MIGRATE_LABELS` is what lets the fleet summary stop claiming a
        // migration ran on a rollout that died while uploading. It is only true if
        // it actually matches the builders, so derive the real pre-`migrate` prefix
        // from both and check it — a moved, added or renamed op fails HERE rather
        // than making the summary lie at 3 a.m.
        let first = sample_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        let redeploy = sample_cutover_ops(Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"));
        for (path, ops) in [("first deploy", &first), ("redeploy", &redeploy)] {
            let labels: Vec<&str> = ops.iter().map(DeployOp::label).collect();
            let migrate = labels
                .iter()
                .position(|l| *l == "migrate")
                .unwrap_or_else(|| panic!("{path} runs a migration: {labels:?}"));
            for label in &labels[..migrate] {
                assert!(
                    PRE_MIGRATE_LABELS.contains(label),
                    "{path}: `{label}` runs before `migrate` but is missing from \
                     PRE_MIGRATE_LABELS — a host that failed there would be reported as \
                     having moved the schema"
                );
            }
            for label in &labels[migrate..] {
                assert!(
                    !PRE_MIGRATE_LABELS.contains(label),
                    "{path}: `{label}` runs at or after `migrate` but is listed as \
                     PRE-migrate — a host that failed there would be reported as NOT \
                     having moved the schema, which is the dangerous direction"
                );
            }
        }
        // The driver splices host preparation ahead of everything (#1607), so it is
        // pre-migrate too even though no builder emits it.
        assert!(failed_before_migrating("install-proxy"));
        // Anything unrecognised errs toward "the schema may have moved".
        assert!(!failed_before_migrating("readiness-gate"));
        assert!(!failed_before_migrating("some-future-op"));
    }

    // ── Host preparation: provisioning the proxy binary (#1607, AC-1) ────────

    #[test]
    fn a_compatible_host_needs_no_preparation_and_is_never_touched() {
        // AC-1's host prep is probe-gated and idempotent: a host that already has a
        // working kamal-proxy runs the read-only probe and NOTHING else — no apt, no
        // image pull, no binary replaced under a live proxy.
        let exec =
            RecordingExecutor::new().with_stdout("proxy-compat-probe", compatible_deploy_help());
        assert_eq!(
            assess_proxy_readiness(&proxy(), &exec, ProxyProvisioning::Auto)
                .expect("a compatible host passes"),
            ProxyReadiness::Ready
        );
        assert_eq!(exec.run_labels(), vec!["proxy-compat-probe"]);
    }

    #[test]
    fn a_bare_host_is_assessed_as_needing_preparation_without_being_mutated() {
        // AC-1: "at most a stock Ubuntu LTS with SSH access — the command performs
        // any remaining host preparation itself". The ASSESSMENT stays read-only so
        // the fleet's all-hosts probe phase still touches nothing; the install runs
        // as an op at the head of that host's own turn.
        let exec = RecordingExecutor::new(); // empty capture → no binary
        assert_eq!(
            assess_proxy_readiness(&proxy(), &exec, ProxyProvisioning::Auto)
                .expect("a bare host is preparable"),
            ProxyReadiness::NeedsInstall
        );
        assert_eq!(
            exec.run_labels(),
            vec!["proxy-compat-probe"],
            "assessing a host must not mutate it"
        );
    }

    #[test]
    fn host_prep_never_replaces_a_working_but_drifted_binary() {
        // A binary that RESPONDS but has drifted flags is somebody's working
        // install — possibly shared with another app on the host. Replacing it
        // silently is not ours to do, so this stays the fail-closed path it has
        // always been.
        let drifted = compatible_deploy_help().replace("--drain-timeout", "--drain-window");
        let exec = RecordingExecutor::new().with_stdout("proxy-compat-probe", drifted);
        let err = assess_proxy_readiness(&proxy(), &exec, ProxyProvisioning::Auto)
            .expect_err("CLI drift must fail closed");
        assert!(matches!(err, DeployExecError::ProxyIncompatible { .. }));
    }

    #[test]
    fn host_prep_can_be_declined_and_then_says_exactly_what_to_install() {
        // An operator who provisions kamal-proxy themselves sets
        // `[deploy] install_proxy = false`; the deploy must then fail with the
        // manual remedy rather than touching their host.
        let exec = RecordingExecutor::new(); // empty capture → no binary
        let err = assess_proxy_readiness(&proxy(), &exec, ProxyProvisioning::Disabled)
            .expect_err("a missing binary with host prep declined must fail closed");
        let DeployExecError::ProxyIncompatible { message } = err else {
            panic!("expected ProxyIncompatible");
        };
        assert!(
            message.contains("install_proxy"),
            "names the opt-out that is in force: {message}"
        );
        assert!(
            message.contains(super::super::proxy::KAMAL_PROXY_KNOWN_GOOD_VERSION),
            "names the version to install: {message}"
        );
    }

    #[test]
    fn a_controller_that_cannot_prepare_a_host_reports_the_missing_binary() {
        // A controller with a compat probe but no installer (it expects its binary
        // to arrive some other way) must not silently pass a host with none.
        struct NoInstallerController(super::super::proxy::KamalProxyController);
        impl ProxyController for NoInstallerController {
            fn ensure_installed_ops(&self, public_port: u16) -> Vec<DeployOp> {
                self.0.ensure_installed_ops(public_port)
            }
            fn route_op(&self, service: &str, upstream: &str) -> DeployOp {
                self.0.route_op(service, upstream)
            }
            fn flip_op(&self, service: &str, new_upstream: &str) -> DeployOp {
                self.0.flip_op(service, new_upstream)
            }
            fn compat_probe(&self) -> Option<super::super::proxy::ProxyCompatProbe> {
                self.0.compat_probe()
            }
            // binary_install_ops() uses the trait default → None.
        }
        let exec = RecordingExecutor::new();
        let err = assess_proxy_readiness(
            &NoInstallerController(proxy()),
            &exec,
            ProxyProvisioning::Auto,
        )
        .expect_err("a controller that cannot prepare a host must fail closed");
        assert!(matches!(err, DeployExecError::ProxyIncompatible { .. }));
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
    fn proxy_live_target_port_extracts_the_observed_upstream_port() {
        // The observed port is returned even when it does NOT map to a slot band
        // under the CURRENT public port — the #2071 legacy/port-change case. Here the
        // live blue release still binds 3001 while the operator's new public port is
        // 5000, so `slot_for_port(5000, 3001)` is None yet the observed port is 3001.
        let list = proxy_list_serving("myapp", 3001);
        assert_eq!(proxy_live_target_port(&list, "myapp"), Some(3001));
        // And `proxy_live_slot` (slot-mapped) correctly reports None for that same
        // list under the new public port, proving the observed port is the ONLY
        // signal that survives a `server.port` change.
        assert_eq!(proxy_live_slot(&list, "myapp", 5000), None);
        // Under the matching public port it still maps to the slot as before.
        assert_eq!(proxy_live_slot(&list, "myapp", 3000), Some(SLOT_BLUE));

        // Unclear signals → None (mirrors proxy_live_slot's fall-back conditions):
        // service absent, listed twice, no target, two different ports, empty name.
        assert_eq!(proxy_live_target_port(&list, "other"), None);
        assert_eq!(proxy_live_target_port(&list, ""), None);
        let twice = format!(
            "{}{}",
            proxy_list_serving("myapp", 3001),
            "myapp   x   127.0.0.1:3002\n"
        );
        assert_eq!(proxy_live_target_port(&twice, "myapp"), None);
        let two_ports = "Service   Target             Prev\n\
                         myapp   127.0.0.1:3001   127.0.0.1:3002\n";
        assert_eq!(proxy_live_target_port(two_ports, "myapp"), None);
        assert_eq!(proxy_live_target_port("", "myapp"), None);
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
            MigrateStep::Run,
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
        // #1621 (R2, T1.23) DELIBERATELY updated this vector: the three liveness
        // options below are new, and they apply to single-host and fleet deploys
        // alike. `SshExecutor::run` uses `Command::output()` with no timeout and the
        // preflight is a bare TCP connect, so before this a host that accepted TCP
        // and then hung the SSH handshake blocked the deploy FOREVER — which at
        // fleet scale means a permanently half-flipped fleet with no timeout to
        // recover from. Assert the exact argv (never `contains`), so any future
        // change to the option set is a conscious one.
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
                "-o".to_owned(),
                "ConnectTimeout=10".to_owned(),
                "-o".to_owned(),
                "ServerAliveInterval=15".to_owned(),
                "-o".to_owned(),
                "ServerAliveCountMax=4".to_owned(),
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
                // scp forwards -o to its ssh transport, so an upload — the longest
                // single operation in a deploy — gets the same liveness guarantees.
                "-o".to_owned(),
                "ConnectTimeout=10".to_owned(),
                "-o".to_owned(),
                "ServerAliveInterval=15".to_owned(),
                "-o".to_owned(),
                "ServerAliveCountMax=4".to_owned(),
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
        // 2 proxy ops + 12 first-deploy ops (incl. clear-previous, the #2074
        // record-proxy-options and the #1607 pre-start migrate) + 2 config-manifest
        // uploads (sample_manifests: autumn.toml + autumn-prod.toml, #1952) + 1
        // proxy route = 17.
        assert_eq!(exec.calls().len(), 17, "the full op sequence should run");
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

    // ── SQLite data-file persistence (issue #1909) ─────────────────────────

    /// A Postgres app emits no data-link op at all, so its op sequence is
    /// byte-identical to pre-#1909.
    #[test]
    fn no_data_link_op_without_a_sqlite_data_file() {
        assert!(sqlite_data_link_op(&resolved(), RELEASE_DIR).is_none());
        let labels: Vec<&str> = sample_ops(Secret::new("X=1\n"))
            .iter()
            .map(DeployOp::label)
            .collect();
        assert!(!labels.contains(&"link-data"), "{labels:?}");
    }

    /// The link op is what makes the data file outlive the release: the real file
    /// sits in `shared/data`, the release dir only holds a symlink at the path the
    /// app resolves.
    #[test]
    fn the_data_link_op_points_the_release_at_the_shared_file() {
        let cfg = resolved_sqlite();
        let op = sqlite_data_link_op(&cfg, RELEASE_DIR).expect("a SQLite app links its data file");
        assert_eq!(op.label, "link-data");
        assert!(
            op.shell.contains(
                "ln -s '/srv/autumn/myapp/shared/data/app.db' \
                    '/srv/autumn/myapp/releases/20260714T120000Z/app.db'"
            ),
            "the release path must be a link to the shared file: {}",
            op.shell
        );
        // The shared dir must exist before the link is made, and a stale entry at
        // the release path must be cleared or `ln` would link INSIDE a directory.
        assert!(
            op.shell
                .contains("mkdir -p '/srv/autumn/myapp/shared/data'"),
            "{}",
            op.shell
        );
        assert!(
            op.shell
                .contains("rm -f '/srv/autumn/myapp/releases/20260714T120000Z/app.db'"),
            "{}",
            op.shell
        );
    }

    /// An app deployed before this contract holds a real file in the release that
    /// is still serving. Moving it while that app runs is not safe — `SQLite`
    /// derives the `-wal` name from the path it resolved, and there is no atomic
    /// move — so the deploy stops and names the one-time manual step.
    #[test]
    fn the_data_link_op_refuses_to_relocate_a_live_database() {
        let op = sqlite_data_link_op(&resolved_sqlite(), RELEASE_DIR).expect("linked");
        // The refusal fires when the shared file is absent and `current`
        // resolves to something. `-L` then only picks WHICH message — it must
        // not gate the refusal, or a legacy symlinked database is linked past.
        assert!(
            op.shell
                .contains("[ ! -e '/srv/autumn/myapp/shared/data/app.db' ]")
                && op
                    .shell
                    .contains("[ -e '/srv/autumn/myapp/current/app.db' ]"),
            "the refusal must fire when the shared file is absent and the current \
             release still holds a database: {}",
            op.shell
        );
        assert!(
            op.shell.contains("exit 1"),
            "it must stop the deploy: {}",
            op.shell
        );
        // The message must name the fix, including the units to stop and the
        // move. Its operands are shell-quoted so the line is safe to paste, and
        // the whole line is one `echo` word, so those quotes appear escaped.
        assert!(
            op.shell
                .contains(r"systemctl stop '\''myapp-blue.service'\'' '\''myapp-green.service'\''"),
            "the message must name the units to stop: {}",
            op.shell
        );
        assert!(
            op.shell.contains(
                r"mv '\''/srv/autumn/myapp/current/app.db'\''* '\''/srv/autumn/myapp/shared/data'\''/"
            ),
            "the message must name the move, sidecars included: {}",
            op.shell
        );
        // Nothing in the op moves a live database itself.
        assert!(
            !op.shell
                .contains("mv '/srv/autumn/myapp/current/app.db' '/srv/autumn/myapp/shared"),
            "the op must never relocate the live database itself: {}",
            op.shell
        );
    }

    /// Every interpolated value is a shell-quoted WORD. A quoted path nested
    /// inside a double-quoted `echo` would still expand — single quotes are
    /// literal there — so a database path holding `$(…)` would run a command on
    /// the deploy host.
    #[test]
    fn the_data_link_op_never_expands_a_configured_path() {
        let hostile = resolved().with_sqlite_data_file(Some("$(touch pwned).db".to_owned()));
        let op = sqlite_data_link_op(&hostile, RELEASE_DIR).expect("linked");
        // A double quote may appear only INSIDE a single-quoted word. The
        // hazard is a shell-quoted path sitting in an expandable position, not
        // the character itself — the symlink recovery prints a deliberate
        // `"$(readlink -f '…')"` for the operator, inert until they paste it.
        for (index, _) in op.shell.match_indices('"') {
            assert!(
                inside_single_quotes(&op.shell, index),
                "a double quote outside single quotes makes paths expandable: {}",
                op.shell
            );
        }
        // The substitution survives only inside single quotes, where it is inert.
        for (index, _) in op.shell.match_indices("$(touch pwned)") {
            assert!(
                inside_single_quotes(&op.shell, index),
                "every occurrence must sit inside a single-quoted word: {}",
                op.shell
            );
        }
        // `$s`, our own loop variable, is the only thing left expandable.
        assert!(op.shell.contains("for s in -wal -shm -journal"));
    }

    /// Is byte `index` inside a single-quoted word?
    ///
    /// The generated script uses single quotes only (asserted separately), so
    /// POSIX rules reduce to two: outside quotes a backslash escapes the next
    /// character, and inside them nothing escapes. That is why a naive quote
    /// count is wrong: the escape idiom that puts a literal quote inside a quoted
    /// word closes, emits an escaped quote, then reopens, and that middle quote
    /// must not toggle.
    fn inside_single_quotes(shell: &str, index: usize) -> bool {
        let mut quoted = false;
        let mut chars = shell.char_indices();
        while let Some((i, c)) = chars.next() {
            if i >= index {
                break;
            }
            if quoted {
                if c == '\'' {
                    quoted = false;
                }
            } else if c == '\\' {
                chars.next();
            } else if c == '\'' {
                quoted = true;
            }
        }
        quoted
    }

    /// The recovery line is a command the operator pastes and runs, so quoting
    /// the `echo` around it is not enough: its own operands must be quoted, or
    /// the substitution runs on paste and a path with a space splits the `mv`.
    ///
    /// The whole line is one `echo` word, so the operand quoting appears here in
    /// its escaped form, which is what the outer quote turns it into.
    #[test]
    fn the_data_link_op_prints_a_recovery_command_that_is_safe_to_paste() {
        let hostile = resolved().with_sqlite_data_file(Some("$(touch pwned).db".to_owned()));
        let op = sqlite_data_link_op(&hostile, RELEASE_DIR).expect("linked");
        assert!(
            op.shell
                .contains(r"mv '\''/srv/autumn/myapp/current/$(touch pwned).db'\''*"),
            "the pasted `mv` must carry the path as a quoted word: {}",
            op.shell
        );

        // A path holding a space must reach `mv` as ONE argument. The `*` stays
        // outside the quotes so it still globs the sidecars.
        let spaced = resolved().with_sqlite_data_file(Some("app data.db".to_owned()));
        let op = sqlite_data_link_op(&spaced, RELEASE_DIR).expect("linked");
        assert!(
            op.shell
                .contains(r"mv '\''/srv/autumn/myapp/current/app data.db'\''*"),
            "the source must be one quoted word: {}",
            op.shell
        );
    }

    /// A `current` that is a SYMLINK to an operator-managed database must be
    /// refused too. Linking past it points the release at a shared file that
    /// does not exist; the migration then creates an empty one and cutover
    /// serves it while the real database is orphaned.
    #[test]
    fn the_data_link_op_refuses_a_legacy_symlinked_database() {
        let op = sqlite_data_link_op(&resolved_sqlite(), RELEASE_DIR).expect("linked");
        // The guard turns only on the shared file being absent and `current`
        // resolving to something. A link pointing at the shared file dangles
        // here, so it fails `-e` and never reaches the refusal.
        assert!(
            op.shell.contains(
                "if [ ! -e '/srv/autumn/myapp/shared/data/app.db' ] && \
                 [ -e '/srv/autumn/myapp/current/app.db' ]; then"
            ),
            "an existing `current` must be refused whether or not it is a link: {}",
            op.shell
        );
        assert!(
            !op.shell
                .contains("[ ! -L '/srv/autumn/myapp/current/app.db' ]; then"),
            "the symlink case must not be excluded from the refusal: {}",
            op.shell
        );
        // It gets its own message: `mv` on a link moves the link, not the
        // database, so the real-file recovery would be wrong here.
        assert!(
            op.shell
                .contains("is a symlink to a SQLite database outside"),
            "the symlink case needs its own refusal: {}",
            op.shell
        );
        // The target moves to the EXACT shared name. Moving it merely INTO the
        // shared directory keeps a differently-named target's basename, and the
        // next deploy then creates an empty database at the name it does expect.
        assert!(
            op.shell.contains(
                r#"src=$(readlink -f '\''/srv/autumn/myapp/current/app.db'\''); mv "$src" '\''/srv/autumn/myapp/shared/data/app.db'\''"#
            ),
            "the symlink recovery must move the target to the exact shared path: {}",
            op.shell
        );
        // Sidecars follow it, each to the matching shared name.
        assert!(
            op.shell.contains(
                r#"do [ -e "$src$s" ] && mv "$src$s" '\''/srv/autumn/myapp/shared/data/app.db'\''$s"#
            ),
            "each sidecar must move to the matching shared name: {}",
            op.shell
        );
    }

    /// The op must never delete a database file. A rollback target deployed
    /// before the migration still holds a real one at that path; it is moved
    /// aside, not removed.
    #[test]
    fn the_data_link_op_never_deletes_a_real_database_file() {
        let op = sqlite_data_link_op(&resolved_sqlite(), RELEASE_DIR).expect("linked");
        assert!(
            !op.shell.contains("rm -rf"),
            "no recursive delete may touch a database path: {}",
            op.shell
        );
        assert!(
            op.shell.contains(
                "if [ -e '/srv/autumn/myapp/releases/20260714T120000Z/app.db' ] && \
                 [ ! -L '/srv/autumn/myapp/releases/20260714T120000Z/app.db' ]"
            ),
            "a real file must be distinguished from a stale link: {}",
            op.shell
        );
        // It is moved beside the SHARED file, under `shared/`, where release
        // retention never reaches it.
        assert!(
            op.shell.contains(
                "mv -f '/srv/autumn/myapp/releases/20260714T120000Z/app.db' \
                 '/srv/autumn/myapp/shared/data/app.db.superseded'"
            ),
            "a real file must be moved aside into shared/: {}",
            op.shell
        );
        // …and never over an existing one.
        assert!(
            op.shell
                .contains("if [ -e '/srv/autumn/myapp/shared/data/app.db.superseded' ]"),
            "an existing superseded copy must be refused, not overwritten: {}",
            op.shell
        );
        assert!(
            op.shell.contains("already exists"),
            "and it must say so: {}",
            op.shell
        );
    }

    /// Both journal modes leave sidecars. WAL leaves `-wal`/`-shm`; the default
    /// rollback journal leaves `-journal`, and `VACUUM INTO` writes its output in
    /// that mode whatever the source used. All three must move with the database.
    #[test]
    fn the_data_link_op_moves_every_sidecar_kind() {
        let op = sqlite_data_link_op(&resolved_sqlite(), RELEASE_DIR).expect("linked");
        assert!(
            op.shell.contains("for s in -wal -shm -journal"),
            "the move-aside step must cover every sidecar: {}",
            op.shell
        );
    }

    /// The link must exist before the migrate one-shot runs, on BOTH deploy paths:
    /// a migration applied to a file in the release dir is a migration the app
    /// never sees.
    #[test]
    fn the_data_link_precedes_the_migration_on_both_deploy_paths() {
        let cfg = resolved_sqlite();
        let plan = SlotPlan {
            live_slot: SLOT_GREEN,
            live_port: 3002,
            candidate_slot: SLOT_BLUE,
            candidate_port: 3001,
            public_port: 3000,
        };
        let unit = super::super::render_app_unit(&cfg, RELEASE_DIR, 3001, SLOT_BLUE);
        for (path, ops) in [
            (
                "first deploy",
                first_deploy_ops(
                    &cfg,
                    &proxy(),
                    &unit,
                    Secret::new("X=1\n"),
                    Path::new("/tmp/app"),
                    &[],
                    RELEASE_ID,
                    &plan,
                    MigrateStep::Run,
                ),
            ),
            (
                "redeploy",
                cutover_ops(
                    &cfg,
                    &proxy(),
                    &unit,
                    Secret::new("X=1\n"),
                    Path::new("/tmp/app"),
                    &[],
                    RELEASE_ID,
                    &plan,
                    &ProxyServiceOptions {
                        tls: false,
                        host: None,
                    },
                    MigrateStep::Run,
                ),
            ),
        ] {
            let labels: Vec<&str> = ops.iter().map(DeployOp::label).collect();
            let link = labels
                .iter()
                .position(|l| *l == "link-data")
                .unwrap_or_else(|| panic!("{path}: no link-data op in {labels:?}"));
            let prepare = labels
                .iter()
                .position(|l| *l == "prepare-dirs")
                .expect("prepare-dirs");
            let migrate = labels
                .iter()
                .position(|l| *l == "migrate")
                .expect("migrate");
            assert!(
                prepare < link && link < migrate,
                "{path}: the link must sit between prepare-dirs and migrate: {labels:?}"
            );
        }
    }

    /// A rollback target deployed before adoption no longer holds the file at that
    /// path, so the rolled-back release must be re-linked before it is started.
    #[test]
    fn rollback_relinks_the_target_release_before_starting_it() {
        let target = RollbackTarget {
            release_dir: "/srv/autumn/myapp/releases/20260713T120000Z".to_owned(),
            slot: SLOT_GREEN,
            port: 3002,
        };
        let ops = rollback_ops(&resolved_sqlite(), &proxy(), &target);
        let labels: Vec<&str> = ops.iter().map(DeployOp::label).collect();
        let link = labels
            .iter()
            .position(|l| *l == "link-data")
            .unwrap_or_else(|| panic!("no link-data op in {labels:?}"));
        let start = labels
            .iter()
            .position(|l| *l == "restart-previous")
            .expect("restart-previous");
        assert!(
            link < start,
            "the link must precede the restart: {labels:?}"
        );
        assert!(
            ops.iter().any(|op| matches!(
                op,
                DeployOp::Run(c)
                    if c.shell
                        .contains("'/srv/autumn/myapp/releases/20260713T120000Z/app.db'")
            )),
            "the rollback must link the TARGET release dir, not the current one"
        );
        // A Postgres rollback is unchanged.
        let plain: Vec<&str> = rollback_ops(&resolved(), &proxy(), &target)
            .iter()
            .map(DeployOp::label)
            .collect();
        assert!(!plain.contains(&"link-data"), "{plain:?}");
    }

    /// Release retention removes release DIRS. `rm -rf` unlinks a symlink rather
    /// than following it, so pruning a release can never reach the shared data
    /// file — but only as long as the prune shell never opts into following.
    #[test]
    fn pruning_a_release_never_follows_the_data_symlink() {
        let shell = prune_releases_shell(
            "/srv/autumn/myapp/releases",
            "/srv/autumn/myapp/current",
            "/srv/autumn/myapp/shared/previous-release",
            3,
        );
        assert!(shell.contains("rm -rf"), "{shell}");
        for follows in ["-follow", "-L ", "--dereference", "cp -L"] {
            assert!(
                !shell.contains(follows),
                "the prune must not follow symlinks ({follows}): {shell}"
            );
        }
    }
}
