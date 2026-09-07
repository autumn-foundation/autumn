//! `autumn deploy` — push-button, zero-downtime deploys to a VPS (issue #1607).
//!
//! This slice implements the locally-verifiable spine of the command:
//!
//! - **`check`** runs a preflight (SSH reachability, signing secret, database
//!   URL, and a `migrate check`) and reports pass/fail, exiting non-zero if any
//!   grader fails.
//! - **`plan`** renders the systemd service unit and the ordered zero-downtime
//!   rollout plan as a pure dry-run — it touches nothing remote.
//! - **`up`** performs a REAL deploy: it runs the same preflight as `check`
//!   (aborting before touching the server if anything fails), then drives the
//!   ordered host-prep + first-deploy or zero-downtime cutover sequence against
//!   an injectable executor (the real one shells out to system `ssh`/`scp`), with
//!   auto-rollback of the candidate on a pre-cutover failure. See [`exec`].
//! - **`rollback`** performs a REAL on-demand rollback (Slice 3, AC-4): it
//!   resolves the previous release on the target, flips the proxy back to it,
//!   repoints `current`, and re-probes `/ready` — failing loudly when there is no
//!   previous release to roll back to.
//!
//! Only the CI end-to-end harness that exercises rollback over real ssh against a
//! container remains (Slice 4). The plan/unit generators here are pure functions
//! so they can be unit-tested without a server, and the preflight graders are
//! shared with `autumn doctor`.

pub mod exec;
// #1621: the fleet planning layer is crate-internal — every item is `pub(crate)`
// and only `deploy` (and, later, its rollout driver) consumes it, so the module
// itself stays private rather than joining the `pub mod` siblings above.
mod fleet;
pub mod media;
pub mod proxy;

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, DeployConfig, Env};
use proxy::ProxyController;

/// Bounded timeout for the SSH-reachability preflight probe. Kept short so the
/// check fails fast on an unreachable host instead of hanging on a dropped SYN.
const SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default migrations directory scanned by the `migrate check` preflight grader.
const MIGRATIONS_DIR: &str = "migrations";

/// Detail shown when no deploy target host is configured. Shared by the offline
/// host-present grader and the online SSH-reachability probe so `autumn deploy
/// check` and `autumn doctor` report the missing-host case identically.
const DEPLOY_HOST_MISSING_DETAIL: &str = "no target host configured";

/// Remediation hint for a missing/blank `[deploy] host`. Shared so every surface
/// (offline `doctor`, online `deploy check`) points at the same fix.
///
/// #1621 EXTENDED this text with the fleet spelling rather than replacing it: the
/// literal substring `` `[deploy] host` `` is asserted by
/// `deploy_check_fails_fast_without_host` and quoted in operator runbooks, so it
/// must survive verbatim.
const DEPLOY_HOST_MISSING_HINT: &str = "Set `[deploy] host` in autumn.toml to your server's SSH-reachable address \
     (or `[deploy] hosts = [\"<address>\", …]` to deploy a fleet)";

/// Errors surfaced by `autumn deploy`.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    /// The project configuration could not be loaded.
    #[error("failed to load configuration: {0}")]
    Config(String),

    /// `deploy` was invoked on a platform where it is not supported natively
    /// (issue #1616). Its own variant rather than [`Self::Config`]: nothing is
    /// wrong with the configuration, and prefixing this message with "failed to
    /// load configuration" would send the operator to the wrong file.
    #[error("{0}")]
    UnsupportedPlatform(String),

    /// One or more preflight graders failed.
    #[error(
        "preflight failed: {0} check(s) did not pass — resolve the issues above and re-run \
         `autumn deploy check`"
    )]
    PreflightFailed(usize),

    /// The release binary to upload was not found on disk.
    #[error("release binary not found at {0} — run `autumn build --embed` first")]
    BinaryMissing(String),

    /// Remote execution of the first deploy failed.
    #[error("deploy execution failed: {0}")]
    Exec(String),

    /// A multi-host rollout stopped mid-fleet (issue #1621, AC-3).
    ///
    /// **Boxed deliberately.** The payload is seven fields (160 bytes) and
    /// `DeployError` is the `Err` type of essentially every function in this
    /// module, so inlining it would make each of ~30 `Result`s carry the halt
    /// report on its happy path — which `clippy::result_large_err` (armed by the
    /// workspace's `-D warnings` gate) refuses, correctly. One allocation on the
    /// rarest path is the right trade.
    #[error("{0}")]
    FleetHalted(Box<FleetHalt>),

    /// A fleet-wide `deploy rollback` did not restore every host (issue #1621).
    ///
    /// Unlike [`Self::FleetHalted`] this is not a partial-state report: a fleet
    /// rollback is **best-effort-continue**, so every host was attempted and the
    /// per-host table printed above it is the detail. Two counts are all this
    /// carries — never a host-specific string built from a driver error.
    ///
    /// Returned when ANY host was left un-rolled-back — its rollback failed, or it
    /// had no previous release to return to. Partial success is failure here: the
    /// contract is that the FLEET returns to its previous release, and a fleet where
    /// one host could not follow the others is mixed.
    #[error(
        "fleet rollback incomplete: {not_rolled_back} of {total} host(s) were not rolled back \
         — see the per-host table above. A host with no previous release recorded cannot be \
         rolled back; deploy the intended release to it explicitly, or retry one host with \
         `autumn deploy rollback --only <host>`"
    )]
    FleetRollbackFailed {
        /// Hosts left un-rolled-back (failed, or nothing to roll back to).
        not_rolled_back: usize,
        /// Hosts attempted.
        total: usize,
    },

    /// `deploy status --strict` found drift (issue #1621, AC-6).
    ///
    /// Carries no payload: the status table printed above IS the report, and this
    /// error exists so drift is alertable from cron — the default (non-`--strict`)
    /// status never fails on drift.
    #[error(
        "fleet drift detected — the status table above names each host and reason \
         (`--strict` exits non-zero on drift so it can be alerted on)"
    )]
    DriftDetected,

    /// A fleet `deploy maintenance on|off` did not reach every host (issue #1621,
    /// AC-5).
    ///
    /// Best-effort-and-aggregate: every host was attempted, the per-host table
    /// above names the ones that DID change (they are deliberately NOT reversed —
    /// that would reopen the window the operator is closing), and this carries
    /// only counts — never a host-derived string.
    #[error(
        "fleet maintenance {verb} incomplete: {failed} of {total} host(s) failed — the \
         table above names the hosts that DID change, so they can be reversed by hand \
         if the fleet must stay consistent"
    )]
    FleetMaintenanceFailed {
        /// Which verb was fanned out (`"on"` / `"off"`).
        verb: &'static str,
        /// Hosts whose change failed.
        failed: usize,
        /// Hosts attempted.
        total: usize,
    },
}

/// The partial-fleet state a halted rollout leaves behind (issue #1621, AC-3).
///
/// Every field is a host name or a `&'static str` op label — never a shell line, a
/// remote path, or a formatted driver error (a failed migration's source can embed
/// the database URL). The lists describe the FINAL state, after any automatic
/// compensation: a host whose rollback failed stays in `still_on_new`, which is the
/// whole point of reporting them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "fleet rollout halted at {failed_host} during `{failed_step}` — the remaining hosts were \
     not touched, and any migration that already ran was NOT rolled back (automatic \
     compensation restores binaries only)"
)]
pub struct FleetHalt {
    /// Host the rollout stopped on.
    pub failed_host: String,
    /// Label of the step that failed on it.
    pub failed_step: &'static str,
    /// Hosts whose previous release is serving again — either their own
    /// pre-boundary teardown or the fleet's compensating rollback.
    pub rolled_back: Vec<String>,
    /// Hosts left with nothing installed — either a torn-down first-deploy
    /// candidate or a first deploy the fleet compensated away.
    pub torn_down: Vec<String>,
    /// Hosts left running the NEW release, in rollout order.
    pub still_on_new: Vec<String>,
    /// Hosts that cut over but whose post-cutover housekeeping failed, with the
    /// step label. Recorded even when the host was later compensated, because the
    /// debris (an unwritten `shared/proxy-options` marker in particular) outlives
    /// the rollback and fails the NEXT deploy closed.
    pub degraded: Vec<(String, &'static str)>,
    /// Hosts the fleet deliberately did NOT roll back, with the reason.
    pub manual: Vec<(String, &'static str)>,
}

/// Which `autumn deploy` subcommand to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployAction {
    /// Run the preflight and report pass/fail.
    Check,
    /// Print the systemd unit and the ordered deploy plan (dry-run).
    Plan,
    /// Run the preflight, then perform a REAL on-demand rollback over SSH.
    Rollback,
    /// Run the preflight, then perform a REAL first deploy over SSH.
    Up,
    /// Report every configured host's state, read-only (issue #1621, AC-6).
    Status,
    /// Turn maintenance mode ON across every configured host (issue #1621, AC-5).
    ///
    /// The flags travel in [`DeployOptions::maintenance`] so this enum stays
    /// fieldless and `Copy`.
    MaintenanceOn,
    /// Turn maintenance mode OFF across every configured host (issue #1621, AC-5).
    MaintenanceOff,
}

/// The `autumn deploy maintenance on` flags (issue #1621, AC-5).
///
/// Exactly the flag set of the existing local `autumn maintenance on`, because the
/// two write the SAME wire format — `autumn_web::maintenance::MaintenanceConfig` —
/// and an operator who knows one must not have to learn the other. This is a plain
/// owned mirror of `MaintenanceOnOptions` (which borrows, and carries a test-only
/// path override that has no meaning for a remote host).
///
/// Carried in [`DeployOptions`] rather than as a [`DeployAction`] payload so the
/// action enum stays fieldless and `Copy`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceOnArgs {
    /// Human-readable message shown in the 503 body.
    pub message: Option<String>,
    /// CIDR blocks / IPs whose requests bypass the gate.
    pub allow_ips: Vec<String>,
    /// Let GET/HEAD/OPTIONS through while blocking writes.
    pub readonly: bool,
    /// `(header name, expected value)` that bypasses the gate.
    pub bypass_header: Option<(String, String)>,
}

/// Flags that modify a [`DeployAction`] (issue #1621, plan §3.1).
///
/// Carried **beside** the action rather than inside it: [`DeployAction`] is a
/// fieldless `Copy` enum matched 1:1 by `run_deploy_command`, and giving its
/// variants payloads would ripple into every call site and test for no gain. This
/// struct is also the natural home for the later fleet flags (`--json`,
/// `--strict`, the maintenance verb) and, in Phase 2, `--role` — so it derives
/// [`Default`] and every construction site outside the CLI uses
/// `..DeployOptions::default()`, making each addition a non-event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeployOptions {
    /// Restrict the command to these hosts, a subset of `[deploy] hosts` (the
    /// repeatable `--only`). Empty means the whole configured fleet.
    ///
    /// A **repair lever, not a convenience**: narrowing a rollout is how an
    /// operator ends up with a mixed fleet on purpose, so the CLI says so out
    /// loud whenever this excludes a configured host.
    pub only: Vec<String>,
    /// Halt and FREEZE a failed rollout instead of compensating it (`--no-rollback`).
    ///
    /// The hosts that already cut over are left exactly as they are for
    /// inspection, and named in the state table. See [`FleetUpInput::auto_rollback`].
    pub no_rollback: bool,
    /// `deploy status --json`: emit the stable machine-readable report (the repo's
    /// skills are a first-class consumer) instead of the table.
    pub json: bool,
    /// `deploy status --strict`: exit non-zero when ANY drift is detected, so
    /// drift is alertable from cron. The default status always exits 0 (it is a
    /// report, not a judgement).
    pub strict: bool,
    /// The `deploy maintenance on` flags. `Some` only for
    /// [`DeployAction::MaintenanceOn`]; `None` everywhere else (including
    /// `MaintenanceOff`, which takes no flags).
    pub maintenance: Option<MaintenanceOnArgs>,
}

/// A `[deploy]` section with all defaults resolved to concrete values.
///
/// `app_name`, `app_dir`, and `service_name` are resolved here (from the
/// project's package name) rather than during deserialization, matching the
/// documented "resolved at deploy time" behavior of [`DeployConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDeployConfig {
    /// SSH-reachable target host, if configured.
    pub host: Option<String>,
    /// SSH user.
    pub user: String,
    /// SSH port.
    pub ssh_port: u16,
    /// Resolved application name.
    pub app_name: String,
    /// Resolved remote install directory.
    pub app_dir: String,
    /// Resolved systemd unit name.
    pub service_name: String,
    /// Readiness window (seconds) before rollback.
    pub readiness_timeout_secs: u64,
    /// Prior releases retained on the host.
    pub keep_releases: u32,
    /// Profile the deployed app runs under (written to the host env file as
    /// `AUTUMN_ENV`). Defaults to the production profile (`"prod"`).
    pub profile: String,
    /// Whether the deploy-managed proxy terminates TLS on 443 (`[deploy.tls]
    /// enabled`). `false` (unchanged HTTP-only behavior) unless opted in.
    pub tls_enabled: bool,
    /// Public hostname the proxy's TLS certificate is issued for. `Some` ONLY
    /// when TLS is enabled and a non-blank host was configured; `None` when TLS
    /// is disabled.
    pub tls_host: Option<String>,
    /// Whether this deploy may install the reverse-proxy binary on a host that has
    /// none (`[deploy] install_proxy`, default `true`; issue #1607, AC-1).
    pub install_proxy: bool,
    /// Release-relative path of the `SQLite` data file, when the app is on the
    /// `SQLite` tier and its `[database]` URL names a RELATIVE file (issue
    /// #1909).
    ///
    /// A slot unit's `WorkingDirectory` is the release dir, so a relative
    /// `sqlite://./app.db` resolves INSIDE the release — a new release would
    /// start against an empty database and the old release's file would be
    /// pruned away with its directory. When this is `Some`, the deploy keeps the
    /// real file in [`Self::shared_data_dir`] and links each release at this
    /// path (see [`crate::deploy::exec::sqlite_data_link_op`]).
    ///
    /// `None` for a Postgres app, and for a `SQLite` app whose URL is already an
    /// absolute path outside the releases dir — there is nothing to relocate.
    /// Set by [`Self::with_sqlite_data_file`]; [`Self::resolve`] leaves it
    /// `None`, since it grades `[deploy]` and never reads `[database]`.
    pub sqlite_data_file: Option<String>,
}

/// Canonicalize a deploy profile string to the value the app's runtime resolver
/// would produce, so the preflight grade and the `AUTUMN_ENV` written to the
/// host env file agree on a single spelling.
///
/// Mirrors `autumn_web::config::normalize_profile_name` (the source of truth) so
/// non-canonical spellings can't slip past preflight and then be rejected at
/// host boot: trims whitespace; case-insensitively folds `production`/`prod` →
/// `"prod"` and `development`/`dev` → `"dev"`; preserves any other non-empty
/// value verbatim (custom profiles like `staging`/`QA`); and for a blank/empty
/// value falls back to the deploy default `"prod"` (matching
/// `autumn_web::config::default_deploy_profile`). Keep this in sync if the
/// runtime rules ever change.
// `pub(crate)` (not `pub`): reachable from `doctor` so it grades the deploy
// signing secret against the same normalized deploy profile, but kept
// crate-internal. In this bin-only crate `deploy` is a private module, so clippy
// flags the `pub(crate)` as redundant; we keep it to document the intended
// visibility rather than widening to `pub`.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn canonicalize_deploy_profile(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "prod".to_owned();
    }

    if trimmed.eq_ignore_ascii_case("production") {
        return "prod".to_owned();
    }
    if trimmed.eq_ignore_ascii_case("development") {
        return "dev".to_owned();
    }
    if trimmed.eq_ignore_ascii_case("prod") {
        return "prod".to_owned();
    }
    if trimmed.eq_ignore_ascii_case("dev") {
        return "dev".to_owned();
    }

    // Preserve user-specified case for custom profile names.
    trimmed.to_owned()
}

/// Trim a deploy profile string, preserving the operator's raw spelling.
///
/// This is what gets stored on [`ResolvedDeployConfig::profile`] and written to
/// `AUTUMN_ENV`, so the host's runtime override-file lookup
/// (`autumn_web::config::profile_override_file_lookup_names`) sees the exact
/// selector the operator wrote — e.g. `autumn-production.toml` is preferred over
/// `autumn-prod.toml` when the raw input was `production`. Alias folding here
/// would flip that precedence, so we deliberately do NOT fold: trims whitespace;
/// for a blank/empty value falls back to the deploy default `"prod"` (matching
/// `autumn_web::config::default_deploy_profile`); otherwise returns the trimmed
/// string verbatim (no alias folding, no case change). Grading normalizes
/// separately via [`canonicalize_deploy_profile`] at the grade site.
// `pub(crate)` (not `pub`): reachable from `doctor` so its deploy secret/DB
// value graders derive the RAW deploy-profile spelling exactly like
// [`ResolvedDeployConfig::resolve`] stores it (trimmed, empty → `prod`), rather
// than re-deriving (and drifting from) that rule. Kept crate-internal. In this
// bin-only crate `deploy` is a private module, so clippy flags the `pub(crate)`
// as redundant; we keep it to document the intended visibility.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn trimmed_deploy_profile(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "prod".to_owned();
    }
    trimmed.to_owned()
}

impl ResolvedDeployConfig {
    /// Resolve a [`DeployConfig`] against the project name, filling in the
    /// `app_name` → `app_dir` → `service_name` default chain.
    ///
    /// # Errors
    ///
    /// Returns a message when `[deploy.tls] enabled = true` but no non-blank
    /// `host` is configured — TLS cannot be provisioned without a hostname for
    /// the certificate, so the misconfiguration is rejected before any remote
    /// command runs.
    pub fn resolve(cfg: &DeployConfig, project_name: &str) -> Result<Self, String> {
        let non_blank = |s: &Option<String>| {
            s.as_ref()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };

        let app_name = non_blank(&cfg.app_name).unwrap_or_else(|| project_name.to_owned());
        let app_dir = non_blank(&cfg.app_dir).unwrap_or_else(|| format!("/srv/autumn/{app_name}"));
        let service_name = non_blank(&cfg.service_name).unwrap_or_else(|| app_name.clone());

        // TLS is opt-in: when disabled, `tls_host` stays `None` and nothing about
        // the render/deploy commands changes. When enabled, a non-blank `host` is
        // mandatory (the certificate is issued for it).
        let tls_host = if cfg.tls.enabled {
            let host = non_blank(&cfg.tls.host).ok_or_else(|| {
                "[deploy.tls] requires a non-empty `host` when enabled: set \
                 `[deploy.tls] host = \"<public-hostname>\"` in autumn.toml to the DNS \
                 name the TLS certificate should be issued for"
                    .to_owned()
            })?;
            Some(host)
        } else {
            None
        };

        Ok(Self {
            host: non_blank(&cfg.host),
            user: cfg.user.clone(),
            ssh_port: cfg.ssh_port,
            app_name,
            app_dir,
            service_name,
            readiness_timeout_secs: cfg.readiness_timeout_secs,
            keep_releases: cfg.keep_releases,
            profile: trimmed_deploy_profile(&cfg.profile),
            tls_enabled: cfg.tls.enabled,
            tls_host,
            install_proxy: cfg.install_proxy,
            sqlite_data_file: None,
        })
    }

    /// Attach the release-relative `SQLite` data-file path this deploy must keep
    /// persistent (issue #1909). See [`Self::sqlite_data_file`].
    #[must_use]
    pub fn with_sqlite_data_file(mut self, relative: Option<String>) -> Self {
        self.sqlite_data_file = relative;
        self
    }

    /// Persistent per-app dir shared across releases (holds the secret env
    /// file). The config manifest(s) uploaded for #1952 are NOT here — they are
    /// uploaded into each release's own dir ([`Self::releases_dir`]) so a
    /// rollback reads the manifest that shipped with the release it rolls back
    /// to; the deployed systemd unit points `AUTUMN_MANIFEST_DIR` at that
    /// release dir, not at this one.
    #[must_use]
    pub fn shared_dir(&self) -> String {
        format!("{}/shared", self.app_dir)
    }

    /// Remote path to the `EnvironmentFile` holding secrets (mode `0600`), kept
    /// out of the world-readable systemd unit.
    #[must_use]
    pub fn env_file(&self) -> String {
        format!("{}/autumn.env", self.shared_dir())
    }

    /// Directory holding timestamped release dirs.
    #[must_use]
    pub fn releases_dir(&self) -> String {
        format!("{}/releases", self.app_dir)
    }

    /// Symlink pointing at the currently-serving release.
    #[must_use]
    pub fn current_symlink(&self) -> String {
        format!("{}/current", self.app_dir)
    }

    /// The maintenance flag file this host's slot units read (issue #1621, R1).
    ///
    /// It lives in the SHARED dir, not a release dir: the runtime's default flag
    /// path is cwd-relative and a slot unit's `WorkingDirectory` is the release dir,
    /// so a cutover would orphan the flag and silently un-maintain the host.
    /// `shared/` is created by `prepare-dirs`, survives cutovers, rollbacks and
    /// pruning, and is seen by BOTH slots — so a flag written here means the same
    /// thing before and after a deploy. [`render_app_unit`] stamps this path into
    /// every slot unit via `AUTUMN_MAINTENANCE_FLAG_FILE`, and the fleet
    /// `deploy maintenance` fan-out writes exactly here.
    ///
    /// The file NAME comes from `autumn_web` — the CLI never re-declares a wire
    /// path the framework owns.
    #[must_use]
    pub fn maintenance_flag_file(&self) -> String {
        format!(
            "{}/{}",
            self.shared_dir(),
            autumn_web::maintenance::MAINTENANCE_FLAG_BASENAME
        )
    }

    /// Persistent directory holding the `SQLite` data file across releases
    /// (issue #1909).
    ///
    /// It sits under `shared/` for the reason the maintenance flag does: `shared/`
    /// survives cutovers, rollbacks and pruning, and both slots see it. A release
    /// dir is the opposite — replaced on every deploy, deleted by retention. The
    /// directory itself is created by the data-link op, not by `prepare-dirs`.
    #[must_use]
    pub fn shared_data_dir(&self) -> String {
        format!("{}/data", self.shared_dir())
    }

    /// Absolute path of the persistent `SQLite` data file, or `None` when this
    /// app has none to keep (Postgres, or an already-absolute `SQLite` path).
    #[must_use]
    pub fn shared_sqlite_data_file(&self) -> Option<String> {
        self.sqlite_data_file
            .as_ref()
            .map(|rel| format!("{}/{rel}", self.shared_data_dir()))
    }
}

/// Validate the `[deploy]` host spelling(s) and return the ordered target list
/// (issue #1621, AC-1).
///
/// Accepts either the historical scalar `[deploy] host` or the fleet list
/// `[deploy] hosts`, never both, and returns the trimmed addresses **in
/// declaration order** — the order of `hosts` IS the rollout order, so nothing
/// here may sort or regroup it. An empty result means neither spelling configured
/// a target; that is a valid state at rest (`deploy plan` renders without one and
/// the `ssh_reachability` grader reports it), so it is NOT an error here.
///
/// Every rule fails closed BEFORE any remote command runs and names the offending
/// key/index/value, matching the house style of the other deploy refusals.
///
/// # Errors
///
/// Returns a message when `host` and `hosts` are both configured, when a `hosts`
/// entry is blank, or when a `hosts` entry repeats (compared after trimming).
// `pub(crate)` (not private): `doctor` enumerates the deploy target list through
// THIS function so its deploy branch grades the same fleet `deploy check` does —
// re-deriving the rules there is exactly the drift the parity invariant forbids.
// In this bin-only crate `deploy` is a private module, so clippy flags the
// `pub(crate)` as redundant; it is kept to document the intended visibility.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn deploy_host_list(cfg: &DeployConfig) -> Result<Vec<String>, String> {
    let host = cfg
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty());

    // 1. Mutual exclusion. With both set the rollout order is ambiguous, so name
    //    BOTH keys and let the operator delete one.
    if host.is_some() && !cfg.hosts.is_empty() {
        return Err(
            "`[deploy] host` and `[deploy] hosts` are mutually exclusive: keep the \
             single-server `[deploy] host = \"<address>\"` or the fleet list \
             `[deploy] hosts = [\"<address>\", …]` in autumn.toml, not both (#1621)"
                .to_owned(),
        );
    }

    if cfg.hosts.is_empty() {
        return Ok(host.map(str::to_owned).into_iter().collect());
    }

    // 2. A blank entry would resolve to a hostless SSH target and blow up
    //    mid-rollout, with earlier hosts already cut over. The index is 0-based so
    //    the operator can find the line in a long list.
    let mut hosts: Vec<String> = Vec::with_capacity(cfg.hosts.len());
    for (index, entry) in cfg.hosts.iter().enumerate() {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "`[deploy] hosts` entry {index} is blank: every fleet entry must be an \
                 SSH-reachable hostname or IP (#1621)"
            ));
        }

        // 3. A duplicate deploys the same machine twice: the second pass sees its
        //    OWN new release as live, ping-pongs the blue/green slots and corrupts
        //    the previous-release chain a fleet rollback depends on. Compared after
        //    trimming; DNS aliases are a documented limitation (same as
        //    `migrate`'s `reject_duplicate_target_urls`).
        if hosts.iter().any(|seen| seen == trimmed) {
            return Err(format!(
                "`[deploy] hosts` lists `{trimmed}` more than once: each fleet host must \
                 appear exactly once — deploying the same server twice corrupts its \
                 previous-release chain (#1621)"
            ));
        }
        hosts.push(trimmed.to_owned());
    }

    Ok(hosts)
}

/// Narrow the configured host list to the `--only` selection (issue #1621, §3.2)
/// — **pure**, so the refusal lands before any config is even resolved.
///
/// Rules:
///
/// - An empty selection is the whole fleet (the default, and the only shape a
///   single-host config ever sees).
/// - The result keeps **declaration order**, not the order the flags were typed:
///   `[deploy] hosts` order IS the rollout order, and letting `--only c --only a`
///   reverse it would silently invert a rolling deploy.
/// - An unknown name is a hard error that quotes the name and lists the configured
///   hosts. Guessing (prefix match, DNS resolution) would let a typo deploy the
///   wrong machine, and silently deploying nothing would be worse still.
/// - A repeated name selects that host once.
///
/// # Errors
///
/// Returns a message naming the unmatched entry and listing the configured hosts.
fn select_hosts(configured: &[String], only: &[String]) -> Result<Vec<String>, String> {
    if only.is_empty() {
        return Ok(configured.to_vec());
    }

    for wanted in only {
        let wanted = wanted.trim();
        if !configured.iter().any(|host| host == wanted) {
            let known = if configured.is_empty() {
                "none — set `[deploy] hosts` in autumn.toml first".to_owned()
            } else {
                configured.join(", ")
            };
            return Err(format!(
                "`--only {wanted}` names a host that is not in this project's deploy \
                 configuration. Configured hosts: {known}. `--only` selects from that list \
                 verbatim — it never resolves or guesses a name, so a typo can't deploy the \
                 wrong machine (#1621)"
            ));
        }
    }

    // Filter the CONFIGURED list rather than mapping over `only`: `[deploy] hosts`
    // order is the rollout order, so `--only c --only a` must not reverse it. This
    // also de-duplicates a repeated flag for free.
    Ok(configured
        .iter()
        .filter(|host| only.iter().any(|wanted| wanted.trim() == host.as_str()))
        .cloned()
        .collect())
}

/// An ordered fleet of deploy targets, each a fully-resolved
/// [`ResolvedDeployConfig`] (issue #1621, AC-1).
///
/// The elements differ **only** in `host`: the shared shape (the `app_name` →
/// `app_dir` → `service_name` chain, the TLS-requires-host rejection, profile
/// trimming) is resolved ONCE through [`ResolvedDeployConfig::resolve`] and then
/// cloned per host. That is what makes a one-entry `hosts` list byte-for-byte the
/// historical single-server deploy *by construction* rather than by review —
/// everything below `exec::SshTarget::from_resolved` keeps working unchanged.
///
/// The list is never empty and is always in declaration (rollout) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFleet {
    /// The resolved targets, in rollout order. Guaranteed non-empty.
    pub hosts: Vec<ResolvedDeployConfig>,
}

impl ResolvedFleet {
    /// Resolve a [`DeployConfig`] into an ordered fleet against the project name.
    ///
    /// Validation runs first, in the documented order (mutual exclusion → blank
    /// entry → duplicate entry → no target at all), so a malformed fleet is
    /// refused before any host is touched.
    ///
    /// # Errors
    ///
    /// Returns a message when [`deploy_host_list`] rejects the host spelling(s),
    /// when neither `host` nor `hosts` configures a target, or when
    /// [`ResolvedDeployConfig::resolve`] rejects the shared shape (e.g.
    /// `[deploy.tls] enabled = true` with no `host`).
    // `deploy up` deliberately does NOT come through here: it resolves the shared
    // shape itself and builds the fleet with [`Self::from_targets`], so a config
    // with no target at all still reaches — and fails at — the PREFLIGHT report
    // (`ssh_reachability`), byte-identically to pre-#1621, instead of being
    // rejected earlier here with a different message and no report. This is the
    // seam the read-only fleet surfaces (`status`, `maintenance`) use.
    pub fn resolve(cfg: &DeployConfig, project_name: &str) -> Result<Self, String> {
        let addresses = deploy_host_list(cfg)?;
        if addresses.is_empty() {
            // 4. Neither spelling configured a target. Reuse the shared
            //    missing-host strings so `deploy check`, `doctor` and this seam
            //    report the case identically.
            return Err(format!(
                "{DEPLOY_HOST_MISSING_DETAIL}: {DEPLOY_HOST_MISSING_HINT}"
            ));
        }

        // Resolve the shared shape ONCE, then vary only `host`.
        let shared = ResolvedDeployConfig::resolve(cfg, project_name)?;
        Ok(Self::from_targets(&shared, &addresses))
    }

    /// Build a fleet from an ALREADY-resolved shared config plus the ordered
    /// address list from [`deploy_host_list`].
    ///
    /// The elements differ only in `host`, so a one-entry list is byte-for-byte
    /// today's single-server deploy **by construction**. An empty or single-entry
    /// list keeps `shared` verbatim — including a `None` host — so a config with no
    /// target still flows into the normal preflight report rather than being
    /// special-cased here.
    #[must_use]
    pub fn from_targets(shared: &ResolvedDeployConfig, addresses: &[String]) -> Self {
        if addresses.len() <= 1 {
            return Self {
                hosts: vec![ResolvedDeployConfig {
                    host: addresses.first().cloned().or_else(|| shared.host.clone()),
                    ..shared.clone()
                }],
            };
        }
        Self {
            hosts: addresses
                .iter()
                .map(|host| ResolvedDeployConfig {
                    host: Some(host.clone()),
                    ..shared.clone()
                })
                .collect(),
        }
    }

    /// Whether this fleet is a single server — the shape every pre-#1621 config
    /// resolves to, and the one that must behave identically to today.
    ///
    /// This answers "how many hosts is this RUN targeting?", which is **not** the
    /// same question as "is this the byte-identical pre-#1621 single-host deploy?" —
    /// a three-host config narrowed by `--only` to one host is a one-host fleet but
    /// not a single-host deploy. Use [`is_single_host_deploy`] for the latter.
    #[must_use]
    pub const fn is_single(&self) -> bool {
        self.hosts.len() == 1
    }
}

/// Whether this run is the genuine pre-#1621 **single-host deploy**: the config
/// declares ONE host and `--only` has not narrowed anything (issue #1621, AC-1).
///
/// This is the one predicate every "does this run keep the pre-#1621 shape?"
/// decision asks, so those decisions can never drift apart. It is deliberately
/// stricter than [`ResolvedFleet::is_single`]: a three-host fleet narrowed to one
/// host resolves to a one-host `ResolvedFleet`, but it is NOT a single-host deploy —
/// the other two hosts exist and are on another release, so the run keeps the fleet
/// vocabulary, the state table, the compensation semantics, and — the reason this
/// was extracted — **host attribution on every message**. An unattributed refusal
/// during a `--only` repair is exactly the wrong output at exactly the wrong moment.
/// See [`FleetUpInput::configured_host_count`].
///
/// Asked at three sites: the rollout driver ([`run_up_with`]), the per-host probe
/// refusals ([`probe_host_for_up`]), and the preflight report
/// ([`collect_fleet_preflight`]). [`run_rollback`] deliberately still branches on
/// `is_single()` alone — that is a choice of which CODE PATH runs, not of how a
/// message is worded, and narrowing a rollback to one host genuinely is a
/// single-host rollback; it is tracked separately.
const fn is_single_host_deploy(fleet: &ResolvedFleet, configured_host_count: usize) -> bool {
    fleet.is_single() && configured_host_count <= 1
}

/// A single ordered step in a deploy or rollback plan. Purely descriptive — a
/// plan is rendered, never executed, in this slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployStep {
    /// Short label (a few words) naming the step.
    pub label: &'static str,
    /// One-line description of what the step does.
    pub description: String,
}

impl DeployStep {
    fn new(label: &'static str, description: impl Into<String>) -> Self {
        Self {
            label,
            description: description.into(),
        }
    }
}

/// Result of a single preflight grader. Shared by `autumn deploy check` and
/// `autumn doctor` so both surfaces grade identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightCheck {
    /// Short stable identifier for the check.
    ///
    /// **Deliberately still `&'static str`.** It is the operator-visible key in
    /// `autumn doctor --json`, so it must not become host-specific; a fleet names
    /// the host in [`Self::scope`] instead. (Making it dynamic would need a
    /// `Cow`/`Box::leak` — an unbounded per-host-per-run leak clippy will not flag
    /// — and would change a published contract for no gain.)
    pub name: &'static str,
    /// Which fleet host this grader graded, when that is a meaningful question
    /// (issue #1621, plan §5.2).
    ///
    /// `Some(host)` only for a per-host grader in a fleet of MORE THAN ONE host;
    /// `None` for every project-wide grader and for every check in a single-host
    /// deploy — which is what keeps single-host output byte-identical to pre-#1621
    /// (AC-1). [`preflight_label`] is the single place it is rendered.
    pub scope: Option<String>,
    /// Whether the check passed.
    pub passed: bool,
    /// Whether the check could not be verified here and is deliberately
    /// **deferred to the service's own runtime** (e.g. an env/interpolation
    /// indirected `[media.ffmpeg] bin` the deploy side must not guess). A
    /// deferred check is non-passing but **non-blocking** — it is surfaced as a
    /// warning and never aborts a deploy, distinct from an honest failure. See
    /// [`PreflightCheck::blocking`].
    pub deferred: bool,
    /// Human-readable detail (what was found).
    pub detail: String,
    /// One-line remediation hint shown on failure.
    pub hint: Option<&'static str>,
}

impl PreflightCheck {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            scope: None,
            passed: true,
            deferred: false,
            detail: detail.into(),
            hint: None,
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: &'static str) -> Self {
        Self {
            name,
            scope: None,
            passed: false,
            deferred: false,
            detail: detail.into(),
            hint: Some(hint),
        }
    }

    /// Attach the fleet host this check graded (issue #1621).
    ///
    /// Always called with `None` for a single-host fleet, so the rendered line is
    /// byte-identical to pre-#1621.
    #[must_use]
    fn scoped(mut self, scope: Option<String>) -> Self {
        self.scope = scope;
        self
    }

    /// Whether this check must abort the deploy. A check blocks only when it
    /// genuinely failed — a **deferred** check (non-passing, but resolved in the
    /// service's own runtime environment) is surfaced as a warning and never
    /// blocks. This is the single predicate every preflight gate keys on so a
    /// deferred outcome can never silently pass *or* hard-fail a deploy.
    #[must_use]
    pub const fn blocking(&self) -> bool {
        !self.passed && !self.deferred
    }
}

// ── Preflight graders (AC-6) ─────────────────────────────────────────────────

/// The preflight graders BOTH `autumn deploy check` and `autumn doctor` run, in
/// the order `deploy check` reports them (issue #1621, plan §5.5).
///
/// This is the **single shared source** for the parity invariant that the two
/// surfaces grade the same things. Doctor's own operator-visible check names are
/// mechanically this list with a `deploy_` prefix — see
/// [`DOCTOR_PREFLIGHT_GRADERS`] and the test that pins the derivation.
///
/// `ssh_reachability` is the only PER-HOST grader: a fleet emits one row per host
/// (distinguished by [`PreflightCheck::scope`]) and the other three once, because
/// they grade the project, not the target.
#[allow(clippy::redundant_pub_crate)]
// Consumed by the parity tests (deploy-side and doctor-side). Kept in production
// code rather than `#[cfg(test)]` because it IS the documented shared source the
// two surfaces derive from — hiding it in a test module would invite a third,
// unchecked hand-written list.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PREFLIGHT_GRADERS: [&str; 4] = [
    "ssh_reachability",
    "signing_secret",
    "database_url",
    "migrate_check",
];

/// `autumn doctor`'s names for [`PREFLIGHT_GRADERS`], in the same order.
///
/// These strings are an operator-visible `doctor --json` contract, so they are
/// spelled out literally (a `const fn` concat is not available for `&'static str`)
/// and pinned against [`PREFLIGHT_GRADERS`] by
/// `doctor_deploy_check_names_are_the_shared_grader_names_prefixed`. The doctor
/// branch indexes THIS array rather than repeating the literals, so a rename can
/// only happen in one place.
///
/// Two doctor deploy checks are deliberately NOT here: `deploy_config` (a
/// doctor-only config-load guard) and `deploy_host` (the OFFLINE host-presence
/// grader, which exists because doctor grades a deploy target without `--online`;
/// `deploy check` subsumes it into `ssh_reachability`, which reports the identical
/// missing-host detail and hint).
#[allow(clippy::redundant_pub_crate)]
pub(crate) const DOCTOR_PREFLIGHT_GRADERS: [&str; 4] = [
    "deploy_ssh_reachability",
    "deploy_signing_secret",
    "deploy_database_url",
    "deploy_migrate_check",
];

/// Grade that a deploy target host is configured — a pure, OFFLINE check with no
/// network I/O. This is deliberately split out from [`grade_ssh_reachability`]
/// (which performs a TCP probe) so `autumn doctor` can validate host presence
/// unconditionally — even without `--online` — while keeping the actual TCP
/// connect behind `--online`. A `[deploy]` table with a missing/blank `host`
/// makes `autumn deploy check` fail immediately, so `doctor` must fail on it too
/// rather than green-lighting a config the deploy path rejects.
#[must_use]
pub fn grade_deploy_host_present(host: Option<&str>) -> PreflightCheck {
    match host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(_) => PreflightCheck::pass("deploy_host", "deploy target host is configured"),
        None => PreflightCheck::fail(
            "deploy_host",
            DEPLOY_HOST_MISSING_DETAIL,
            DEPLOY_HOST_MISSING_HINT,
        ),
    }
}

/// Grade that at least one deploy target is configured, across BOTH host
/// spellings (issue #1621) — the fleet sibling of [`grade_deploy_host_present`].
///
/// `hosts` is the validated, ordered list from [`deploy_host_list`]. Used by
/// `autumn doctor`, whose `deploy_host` check must grade a `[deploy] hosts` fleet
/// rather than reporting "no target host configured" because the scalar `host` is
/// unset.
///
/// The empty and single-host cases produce **byte-identical** detail/hint to
/// [`grade_deploy_host_present`], so single-server doctor output is unchanged; only
/// a real fleet gets the extra host enumeration. That enumeration lives in the
/// DETAIL text rather than in per-host check names because `CheckResult.name` is an
/// operator-visible `doctor --json` key (plan §5.2).
#[must_use]
pub fn grade_deploy_hosts_present(hosts: &[String]) -> PreflightCheck {
    match hosts {
        // Delegate the historical shapes to the historical grader, so their
        // detail/hint are identical BY CONSTRUCTION rather than by copy.
        [] => grade_deploy_host_present(None),
        [only] => grade_deploy_host_present(Some(only)),
        many => PreflightCheck::pass(
            "deploy_host",
            format!(
                "{} deploy target hosts configured, in rollout order: {}",
                many.len(),
                many.join(", ")
            ),
        ),
    }
}

/// Probe SSH reachability for EVERY configured host and aggregate the result into
/// ONE check (issue #1621) — the shape `autumn doctor` needs.
///
/// `deploy check` reports one `ssh_reachability` row per host (distinguished by
/// [`PreflightCheck::scope`]); doctor cannot, because its `CheckResult.name` is a
/// `&'static str` `--json` key that must stay stable and unique. So doctor grades
/// the same hosts with the same grader and folds the per-host outcomes into the
/// detail text: the NAME set stays identical between the two surfaces (the parity
/// invariant, plan §5.5) while the fleet detail is not lost.
///
/// A single-host list delegates straight through, so its detail/hint are
/// byte-identical to pre-#1621. The check fails when ANY host is unreachable — a
/// rollout touches all of them.
#[must_use]
pub fn grade_fleet_ssh_reachability(
    hosts: &[String],
    ssh_port: u16,
    timeout: Duration,
) -> PreflightCheck {
    match hosts {
        [] => grade_ssh_reachability(None, ssh_port, timeout),
        [only] => grade_ssh_reachability(Some(only.as_str()), ssh_port, timeout),
        many => {
            let graded: Vec<(&str, PreflightCheck)> = many
                .iter()
                .map(|host| {
                    (
                        host.as_str(),
                        grade_ssh_reachability(Some(host.as_str()), ssh_port, timeout),
                    )
                })
                .collect();
            let unreachable: Vec<&str> = graded
                .iter()
                .filter(|(_, check)| !check.passed)
                .map(|(host, _)| *host)
                .collect();
            if unreachable.is_empty() {
                PreflightCheck::pass(
                    "ssh_reachability",
                    format!(
                        "SSH port reachable on all {} fleet hosts: {}",
                        many.len(),
                        many.join(", ")
                    ),
                )
            } else {
                PreflightCheck::fail(
                    "ssh_reachability",
                    format!(
                        "cannot reach {} of {} fleet hosts on port {ssh_port}: {}",
                        unreachable.len(),
                        many.len(),
                        unreachable.join(", ")
                    ),
                    "Confirm the server is up, the SSH port is open, and any firewall allows your IP",
                )
            }
        }
    }
}

/// Grade SSH reachability: a bounded, non-interactive TCP connect to the SSH
/// port. This slice does not shell out to `ssh` (that lands with real remote
/// execution) — an honest "the port is reachable" probe is sufficient here.
///
/// The missing-host case reuses the same detail/hint as the offline
/// [`grade_deploy_host_present`] grader so the two surfaces stay consistent.
#[must_use]
pub fn grade_ssh_reachability(
    host: Option<&str>,
    ssh_port: u16,
    timeout: Duration,
) -> PreflightCheck {
    let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) else {
        return PreflightCheck::fail(
            "ssh_reachability",
            DEPLOY_HOST_MISSING_DETAIL,
            DEPLOY_HOST_MISSING_HINT,
        );
    };

    let addrs = match (host, ssh_port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(e) => {
            return PreflightCheck::fail(
                "ssh_reachability",
                format!("could not resolve {host}:{ssh_port}: {e}"),
                "Check that `[deploy] host` is a resolvable hostname or a valid IP address",
            );
        }
    };

    if addrs.is_empty() {
        return PreflightCheck::fail(
            "ssh_reachability",
            format!("{host}:{ssh_port} resolved to no addresses"),
            "Check that `[deploy] host` is a resolvable hostname or a valid IP address",
        );
    }

    // Probe every resolved address, not just the first. A dual-stack host may
    // resolve to an IPv6 address first that an IPv4-only client cannot reach
    // (or vice versa); the port is reachable as long as ANY address connects.
    // Fail only when all of them fail, reporting the last error.
    let mut last_err = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(_) => {
                return PreflightCheck::pass(
                    "ssh_reachability",
                    format!("SSH port reachable at {host}:{ssh_port}"),
                );
            }
            Err(e) => last_err = Some(e),
        }
    }

    let detail = last_err.map_or_else(
        || format!("cannot reach {host}:{ssh_port}"),
        |e| format!("cannot reach {host}:{ssh_port}: {e}"),
    );
    PreflightCheck::fail(
        "ssh_reachability",
        detail,
        "Confirm the server is up, the SSH port is open, and any firewall allows your IP",
    )
}

/// Grade signing-secret presence and, in production, strength. Never prints the
/// secret value.
///
/// In a non-production profile a present, non-empty secret is enough. In a
/// production profile the app boot path runs
/// [`autumn_web::security::validate_signing_secret`] and *exits* on a
/// missing/too-short/known-demo secret, so preflight reuses that exact validator
/// to reject the same values here — otherwise `deploy check` would greenlight a
/// release that immediately fails to boot (or ships a known demo secret).
///
/// The app boot path (`fail_fast_on_invalid_signing_secret`) also validates every
/// `previous_secrets` rotation entry with the same validator and exits if any is
/// weak, so preflight validates them too: a strong current secret paired with a
/// `previous_secrets = ["changeme"]` entry would otherwise pass `deploy check`
/// and then fail to boot. The rejection message names the offending rotation
/// entry by position without printing its value.
#[must_use]
pub fn grade_signing_secret(
    secret: Option<&str>,
    previous_secrets: &[String],
    is_production: bool,
) -> PreflightCheck {
    let Some(value) = secret.map(str::trim).filter(|s| !s.is_empty()) else {
        return PreflightCheck::fail(
            "signing_secret",
            "no signing secret configured",
            "Set AUTUMN_SECURITY__SIGNING_SECRET (generate with `openssl rand -hex 32`)",
        );
    };

    if is_production {
        // Reuse the exact runtime validator so preflight and app boot agree on
        // what counts as a valid production secret. The error's `Display`
        // embeds the (demo) secret value for `KnownWeakValue`, so we translate
        // each variant into a value-free message rather than formatting it.
        use autumn_web::security::{SigningSecretError, validate_signing_secret};
        if let Err(error) = validate_signing_secret(Some(value), true) {
            let detail = match error {
                SigningSecretError::MissingInProduction => {
                    "signing secret is required in production".to_owned()
                }
                SigningSecretError::TooShort { actual, required } => format!(
                    "signing secret is too short ({actual} bytes, minimum {required}) for production"
                ),
                SigningSecretError::KnownWeakValue(_) => {
                    "signing secret is a known demo/template value not allowed in production"
                        .to_owned()
                }
            };
            return PreflightCheck::fail(
                "signing_secret",
                detail,
                "Set a strong AUTUMN_SECURITY__SIGNING_SECRET (generate with `openssl rand -hex 32`)",
            );
        }

        // Rotation secrets accepted during a grace window must clear the same
        // bar as the current secret; the app boot path validates each
        // `previous_secrets` entry with the same validator and exits on the
        // first weak one. Report by 1-based position without printing the value.
        for (index, previous) in previous_secrets.iter().enumerate() {
            if let Err(error) = validate_signing_secret(Some(previous.as_str()), true) {
                let position = index + 1;
                let detail = match error {
                    SigningSecretError::MissingInProduction => format!(
                        "previous (rotation) signing secret #{position} is empty and not allowed in production"
                    ),
                    SigningSecretError::TooShort { actual, required } => format!(
                        "previous (rotation) signing secret #{position} is too short ({actual} bytes, minimum {required}) for production"
                    ),
                    SigningSecretError::KnownWeakValue(_) => format!(
                        "previous (rotation) signing secret #{position} is a known demo/template value not allowed in production"
                    ),
                };
                return PreflightCheck::fail(
                    "signing_secret",
                    detail,
                    "Remove or rotate out the weak entry in security.signing_secret.previous_secrets (each must be as strong as the current secret)",
                );
            }
        }
    }

    PreflightCheck::pass("signing_secret", "signing secret is configured")
}

/// Grade database-URL presence. Never prints the URL (it may embed credentials).
///
/// The URL is only *required* when the app is database-backed — any of:
/// - a migrations directory exists (the same presence check [`grade_migrate_check`]
///   uses, so the two graders agree), or
/// - a `[database]` section is configured, or
/// - a DB-backed runtime feature is enabled (`db_backed_runtime`) — e.g.
///   `jobs.backend = "postgres"` or `scheduler.backend = "postgres"`, whose
///   startup paths (`job::start_postgres_runtime` /
///   `scheduler::coordinator_from_config`) require a configured pool.
///
/// A zero-dependency, daemon-style app with none of these has nothing to connect
/// to, so the grader passes with a "no database configured" note instead of
/// failing preflight unconditionally.
#[must_use]
pub fn grade_database_url(
    url: Option<&str>,
    migrations_dir: &Path,
    database_configured: bool,
    db_backed_runtime: bool,
) -> PreflightCheck {
    match url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(_) => PreflightCheck::pass("database_url", "database URL is configured"),
        None if !migrations_dir.exists() && !database_configured && !db_backed_runtime => {
            PreflightCheck::pass(
                "database_url",
                "no database configured (nothing to connect to)",
            )
        }
        None => PreflightCheck::fail(
            "database_url",
            "no writable database URL: this app is database-backed (migrations, a \
             `[database]` section, or a Postgres-backed runtime feature such as \
             `jobs.backend`/`scheduler.backend`) and needs a primary/control \
             (`database.primary_url`) or shard-primary URL; `database.replica_url` \
             alone is not a writable target",
            "Set database.primary_url (or database.url) in autumn.toml, or AUTUMN_DATABASE__URL",
        ),
    }
}

/// Grade `migrate check`: reuse the migration safety classifier and fail when a
/// pending migration is unsafe for a live rolling deploy. A project with no
/// migrations directory passes (there is nothing to check).
#[must_use]
pub fn grade_migrate_check(migrations_dir: &Path) -> PreflightCheck {
    if !migrations_dir.exists() {
        return PreflightCheck::pass(
            "migrate_check",
            "no migrations directory (nothing to check)",
        );
    }

    match crate::migrate::check_migrations_in_dir(migrations_dir) {
        Ok(reports) => {
            let unsafe_names: Vec<&str> = reports
                .iter()
                .filter(|r| crate::migrate::safety::has_unsafe_findings(&r.up))
                .map(|r| r.name.as_str())
                .collect();
            if unsafe_names.is_empty() {
                PreflightCheck::pass(
                    "migrate_check",
                    format!("{} migration(s) safe for a rolling deploy", reports.len()),
                )
            } else {
                PreflightCheck::fail(
                    "migrate_check",
                    format!(
                        "migration(s) unsafe for a rolling deploy: {}",
                        unsafe_names.join(", ")
                    ),
                    "Run `autumn migrate check` and apply the expand/contract pattern before deploying",
                )
            }
        }
        Err(e) => PreflightCheck::fail(
            "migrate_check",
            format!("could not scan migrations: {e}"),
            "Run `autumn migrate check` to see the full report",
        ),
    }
}

// ── Artifact / plan generators (pure) ────────────────────────────────────────

/// Render the systemd service unit that supervises the deployed app.
///
/// An autumn app compiled with `autumn build --embed` is a standalone server
/// binary (the generated Dockerfile launches it directly as
/// `CMD ["/usr/local/bin/<app>"]`, with no `serve` subcommand). The deploy flow
/// uploads that pre-built binary into a timestamped release dir fronted by the
/// `current` symlink, so the unit execs the deployed binary at
/// `{app_dir}/current/{app_name}` directly — it must NOT run
/// `autumn serve --release`, which would rebuild the project from source.
///
/// The unit restarts on failure, comes back after reboot
/// (`WantedBy=multi-user.target`), and sources secrets from an `EnvironmentFile`
/// so they are never inlined into the world-readable unit (AC-5).
#[must_use]
pub fn render_systemd_unit(cfg: &ResolvedDeployConfig) -> String {
    format!(
        "[Unit]\n\
         Description=Autumn application: {app}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         WorkingDirectory={current}\n\
         EnvironmentFile={env_file}\n\
         Environment=AUTUMN_MANIFEST_DIR={manifest_dir}\n\
         ExecStart={current}/{app}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        app = cfg.app_name,
        user = cfg.user,
        current = cfg.current_symlink(),
        env_file = cfg.env_file(),
        // Point the app's config loader at `current` (this unit's
        // WorkingDirectory), where the active release's uploaded autumn.toml
        // lives, so the deployed app loads the intended config instead of
        // built-in defaults (#1952). The manifest is coupled to the release, not
        // the shared dir. Non-secret, so it lives in the unit's Environment= (not
        // the 0600 env file).
        manifest_dir = cfg.current_symlink(),
    )
}

/// Render the systemd unit for a specific blue/green *slot* release (issue #1607,
/// Slice 2).
///
/// Unlike [`render_systemd_unit`] (which execs the `current` symlink), a slot unit
/// pins its own `release_dir` and binds a PRIVATE loopback `app_port`, so the blue
/// and green slots can run different releases side by side across a cutover while
/// the reverse proxy owns the public port. The unit is named
/// `{service}-{slot}.service` (see [`exec::slot_unit_name`]).
#[must_use]
pub fn render_app_unit(
    cfg: &ResolvedDeployConfig,
    release_dir: &str,
    app_port: u16,
    slot: &str,
) -> String {
    format!(
        "[Unit]\n\
         Description=Autumn application: {app} ({slot})\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         WorkingDirectory={release_dir}\n\
         EnvironmentFile={env_file}\n\
         Environment=AUTUMN_MANIFEST_DIR={manifest_dir}\n\
         Environment={maintenance_env}={maintenance_flag}\n\
         Environment=AUTUMN_SERVER__HOST=127.0.0.1\n\
         Environment=AUTUMN_SERVER__PORT={app_port}\n\
         ExecStart={release_dir}/{app}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        app = cfg.app_name,
        user = cfg.user,
        env_file = cfg.env_file(),
        // Point the app's config loader at this release dir (this unit's
        // WorkingDirectory), where the uploaded autumn.toml lives, so the deployed
        // app loads the intended config instead of built-in defaults (#1952). The
        // manifest is coupled to the binary in the retained per-release dir, so a
        // rollback re-rendering this unit from the target release dir reads that
        // release's OWN manifest. Non-secret → unit Environment=, not the 0600 env
        // file.
        manifest_dir = release_dir,
        // #1621 R1: point the runtime's maintenance flag at the per-app SHARED dir
        // instead of leaving it cwd-relative (i.e. release-relative, because
        // `WorkingDirectory` above is the release dir). Without this a cutover
        // orphans an active maintenance flag and silently un-maintains the host —
        // and the blue and green slots cannot see each other's flag at all. The
        // variable name and file name are the framework's, imported (never
        // re-declared here). Non-secret → unit `Environment=`.
        maintenance_env = autumn_web::maintenance::MAINTENANCE_FLAG_FILE_ENV,
        maintenance_flag = cfg.maintenance_flag_file(),
    )
}

/// Build the ordered, zero-downtime deploy plan (AC-2/3/4).
///
/// The sequence encodes the framework's `/live`-`/ready`-drain contract:
/// migrations run *before* cutover (a failed migration leaves the old version
/// serving), the candidate must report `/ready` within the bounded window (else
/// roll back), traffic is handed over only after readiness, and the old release
/// is drained and pruned last.
///
/// **The candidate never binds the live `server.port`.** For the default TCP
/// serving config the running app binds `server.host:server.port`, so starting
/// the candidate on that same port while the old release is still serving would
/// fail with "address already in use" *before* the readiness gate could ever
/// run — the plan would not be executable. The candidate is therefore started on
/// a SEPARATE listener (a distinct port/socket from the live service), and a
/// later explicit handoff step switches live traffic to it.
///
/// **The concrete handoff mechanism is an open design decision for the execution
/// follow-up.** This slice emits a mechanism-neutral PLAN: the cutover is
/// described as a "reverse-proxy upstream swap or systemd socket-activation
/// handoff" without committing to either in code. Whatever is chosen, the plan
/// must never imply the candidate binds the live `server.port`.
#[must_use]
pub fn build_deploy_plan(cfg: &ResolvedDeployConfig) -> Vec<DeployStep> {
    vec![
        DeployStep::new(
            "build",
            "Build the embedded single-binary release locally (`autumn build --embed`)",
        ),
        // FIRST, because it is the first thing that mutates the server: the install
        // ops are spliced at the head of the host's op vector, ahead of the upload.
        DeployStep::new(
            "prepare-host",
            "Ensure the target has a working kamal-proxy — the FIRST thing that touches the \
             server. A host that already has one is untouched; a host that has none gets the \
             pinned build installed, which also installs (and leaves behind) any of `curl` \
             and a container runtime it is missing, and pulls the pinned proxy image from \
             Docker Hub. Decline with `[deploy] install_proxy = false`",
        ),
        DeployStep::new(
            "upload",
            format!(
                "Upload the binary to a new timestamped release dir under {}",
                cfg.releases_dir()
            ),
        ),
        DeployStep::new(
            "migrate",
            "Run pending migrations BEFORE the new release takes traffic — on a redeploy an \
             abort here leaves the current version serving; on a first deploy the release is \
             never started",
        ),
        DeployStep::new(
            "start-candidate",
            "Start the new release as a candidate on a SEPARATE listener (a distinct \
             port/socket from the live service) — it does NOT bind the live \
             `server.port`, so the old release keeps serving traffic uninterrupted",
        ),
        DeployStep::new(
            "readiness-gate",
            format!(
                "Poll the candidate's /ready on its separate listener within {}s — roll back on \
                 timeout",
                cfg.readiness_timeout_secs
            ),
        ),
        DeployStep::new(
            "cutover",
            format!(
                "Hand live traffic over to the candidate — reverse-proxy upstream swap or systemd \
                 socket-activation handoff (mechanism finalized in the execution slice) — then \
                 promote it by pointing the {} symlink at the new release",
                cfg.current_symlink(),
            ),
        ),
        DeployStep::new(
            "drain",
            "Drain and stop the previous release once traffic has moved to the candidate",
        ),
        DeployStep::new(
            "prune",
            format!(
                "Prune old releases, retaining the most recent {} — always keeping the releases \
                 the current symlink and previous-release marker point at (rollback targets)",
                cfg.keep_releases
            ),
        ),
    ]
}

/// Build the rollback plan, mirroring [`exec::rollback_ops`]'s actual sequence:
/// re-render the target slot's unit and `daemon-reload`, restart the previous
/// slot's unit, flip the proxy upstream back to it, record the release being
/// rolled back FROM as the new previous-release marker, repoint `current`, record
/// the live slot, re-probe `/ready`, and finally drain the slot the rollback
/// flipped traffic away from.
///
/// The order matters: the target unit is re-rendered from the persisted marker
/// (its dir + port) and `daemon-reload`ed FIRST so the restart never relaunches a
/// slot unit an earlier failed redeploy clobbered. The proxy flip is health-gated,
/// so the previous release's unit must be restarted and up *before* the flip can
/// pass, and the flip therefore precedes the `current` repoint. The former-live
/// slot is drained last, after the re-probe confirms the rolled-back release is
/// healthy.
#[must_use]
pub fn build_rollback_plan(cfg: &ResolvedDeployConfig) -> Vec<DeployStep> {
    vec![
        DeployStep::new(
            "write-target-unit",
            format!(
                "Re-render the previous release's {} slot unit from the persisted \
                 marker (its dir + port) so the restart never relaunches a slot unit \
                 an earlier failed redeploy left pointing at a removed candidate",
                cfg.service_name
            ),
        ),
        DeployStep::new(
            "daemon-reload",
            "Reload systemd so the restart below loads the freshly re-rendered unit",
        ),
        DeployStep::new(
            "restart-previous",
            format!(
                "Restart the previous release's {} slot unit so it is healthy \
                 before the proxy flips traffic back to it",
                cfg.service_name
            ),
        ),
        DeployStep::new(
            "proxy-flip",
            "Flip the reverse-proxy upstream back to the previous release \
             (health-gated on /ready before traffic moves)",
        ),
        DeployStep::new(
            "record-previous",
            "Record the release being rolled back FROM (its dir + former-live slot) \
             as the new previous-release marker so a subsequent rollback returns to it",
        ),
        DeployStep::new(
            "repoint",
            format!(
                "Point the {} symlink back at the previous release",
                cfg.current_symlink()
            ),
        ),
        DeployStep::new(
            "record-live-slot",
            "Record the previous slot as the live slot so the next deploy's mode \
             detection sees it serving",
        ),
        DeployStep::new(
            "readiness-gate",
            format!(
                "Re-probe /ready within {}s to confirm the rollback is healthy",
                cfg.readiness_timeout_secs
            ),
        ),
        DeployStep::new(
            "drain-rolled-back-slot",
            "Disable the slot that was live before the rollback (the one traffic \
             moved away from) so the next deploy sees it genuinely idle and starts \
             the new binary fresh",
        ),
    ]
}

// ── Command entrypoint ───────────────────────────────────────────────────────

/// The policy row `autumn deploy`'s remote actions are refused under, quoted
/// verbatim from [`crate::platform::POLICY`] — `tier_two_windows_error` panics
/// on a name the policy does not carry, so this is checked by every test that
/// builds a refusal.
const DEPLOY_TIER_TWO_POLICY_COMMAND: &str = "autumn deploy up / rollback / status / maintenance";

/// Whether an action opens an SSH session to a remote host.
///
/// This is the line the platform policy is drawn on (#1616). The remote actions
/// shell out to `ssh`/`sh`, and `up`/`rollback`/`maintenance` additionally stage
/// the environment file — carrying the app's signing secret and database
/// password — through `exec::stage_temp_file`, whose `chmod 0600` is
/// `#[cfg(unix)]`. On native Windows that file would be written with whatever
/// the containing directory's ACLs happen to be, which is the silent
/// degradation the policy forbids.
///
/// `check` and `plan` are on the other side of that line: `plan` renders the
/// systemd unit and step list from `resolved` alone, and `check` grades the
/// project config plus an `ssh_reachability` probe that is a portable
/// [`TcpStream::connect_timeout`] — no `ssh` binary, no staged file, nothing
/// written anywhere. They are Tier 1 on Windows, and they are precisely how a
/// Windows developer validates a deploy config *before* switching to WSL2 to
/// run it.
///
/// The match is deliberately exhaustive with no wildcard arm: a new
/// [`DeployAction`] must be classified here or the crate does not compile, so a
/// remote action can never default into "runs natively on Windows".
const fn reaches_a_remote_host(action: DeployAction) -> bool {
    match action {
        DeployAction::Check | DeployAction::Plan => false,
        DeployAction::Up
        | DeployAction::Rollback
        | DeployAction::Status
        | DeployAction::MaintenanceOn
        | DeployAction::MaintenanceOff => true,
    }
}

/// The refusal for a native-Windows `autumn deploy` action that reaches a remote
/// host, or `None` when the action is local-only or the host is supported.
///
/// Refusing up front — before any config load, SSH probe or temp file — is what
/// keeps Tier 2 from being a silent degradation (#1616).
///
/// Takes the OS family as a string so the Windows branch is covered by tests on
/// every host.
fn windows_deploy_refusal(os: &str, action: DeployAction) -> Option<String> {
    (os == "windows" && reaches_a_remote_host(action))
        .then(|| crate::platform::tier_two_windows_error(DEPLOY_TIER_TWO_POLICY_COMMAND))
}

/// Run the requested `autumn deploy` subcommand.
///
/// # Errors
///
/// Returns [`DeployError::UnsupportedPlatform`] when a remote-host action is
/// invoked on native Windows (those paths are Tier 2 / WSL2, see #1616;
/// `check` and `plan` are local-only and run natively),
/// [`DeployError::Config`] when the project config cannot be loaded, and
/// [`DeployError::PreflightFailed`] when `check` finds a failing grader.
pub fn run(action: DeployAction, options: &DeployOptions) -> Result<(), DeployError> {
    // Refuse before anything else: no config load, no SSH probe, no temp file.
    if let Some(refusal) = windows_deploy_refusal(std::env::consts::OS, action) {
        return Err(DeployError::UnsupportedPlatform(refusal));
    }
    // Ambient load (the operator's shell profile, `dev` by default), used only to read
    // `[deploy]` and compute `resolved` — in particular `resolved.profile`, the profile
    // the deployed service boots under (default `prod`). It is a lenient-unknown-roots
    // load (#2063): the deploy CLI structurally cannot know an app's plugin set, with no
    // AppBuilder and no plugin-crate dependency, so it must not fail closed on a
    // plugin-owned top-level table such as `[media]` under `strict_config`. Every core
    // section it does own stays strictly validated, and app boot — which knows the
    // plugin set through the `config_section` seam — remains the strict gate.
    let ambient_config = AutumnConfig::load_lenient_unknown_roots()
        .map_err(|e| DeployError::Config(e.to_string()))?;
    let deploy_cfg = ambient_config.deploy.unwrap_or_default();
    // #1621: refuse a malformed host spelling — `host` AND `hosts` together, a
    // blank entry, a duplicate entry — before ANY other work, so the operator sees
    // one actionable message instead of a preflight report graded against a target
    // list we could not make sense of. The ordered list is the rollout order and
    // is rendered by `plan`; the rollout DRIVER that consumes it via
    // `ResolvedFleet` lands in a later slice, so the single-host paths below are
    // deliberately untouched.
    let host_list = deploy_host_list(&deploy_cfg).map_err(DeployError::Config)?;
    // #1621: `--only` narrows the rollout to a subset of that list, in DECLARATION
    // order. Resolved here — before the config is even resolved and long before any
    // preflight probe — so a typo'd host name costs nothing and names itself.
    let targets = select_hosts(&host_list, &options.only).map_err(DeployError::Config)?;
    // A narrowed rollout is a deliberate repair lever that leaves the fleet on two
    // versions, so it is never silent.
    for line in fleet::only_selection_warning_lines(&host_list, &targets) {
        eprintln!("{line}");
    }
    let resolved = ResolvedDeployConfig::resolve(&deploy_cfg, &resolve_project_name())
        .map_err(DeployError::Config)?;

    // MediaMTX host-provisioning config (issue #1974, Slice 7) is loaded ONLY in
    // the actions that plan/provision media (`plan` and `up`). `check`/`rollback`
    // neither read nor provision `MediaMTX`, so parsing `[media]` there would let a
    // typo in `[media.mediamtx]`/`[media.ffmpeg]` fail-close a rollback that does
    // not touch media at all — a regression the round-2 fail-closed fix must not
    // introduce. The load stays fail-closed where it IS relevant (`plan`/`up`): a
    // present-but-invalid `[media]` table still aborts those two.
    match action {
        // `plan` is a pure dry-run over `resolved` alone — it never grades or
        // uploads runtime VALUES, so it needs no reload under the target profile.
        DeployAction::Plan => {
            let (media_cfg, _ffmpeg_bin) = load_media_host_config(&resolved)?;
            print_plan(&resolved, &host_list, &media_cfg);
            Ok(())
        }
        // `check`/`rollback`/`up` grade and (for `up`) upload the signing secret
        // and DB URL, so they must see those VALUES resolved under the TARGET
        // deploy profile — not the operator's ambient/dev config. Reload here.
        // `check`/`rollback` deliberately do NOT load the media config.
        DeployAction::Check => run_check(
            &load_runtime_config(&resolved)?,
            &resolved,
            &targets,
            host_list.len(),
        ),
        DeployAction::Rollback => run_rollback(
            &load_runtime_config(&resolved)?,
            &resolved,
            &targets,
            host_list.len(),
        ),
        // The read-only fleet surfaces (#1621). Both resolve through
        // `ResolvedFleet::resolve` — the seam that rejects a malformed/absent target
        // with one actionable message — and neither loads the media config (they
        // provision nothing).
        DeployAction::Status => {
            let fleet = ResolvedFleet::resolve(&deploy_cfg, &resolve_project_name())
                .map_err(DeployError::Config)?;
            // Review round 1: `status` is READ-ONLY and needs exactly one value
            // from the app config (`server.port`), so an unrelated invalid
            // production setting must not take it offline mid-incident. See
            // `status_public_port`.
            let port = status_public_port(&resolved)?;
            run_status(&port, &resolved.profile, &fleet, options)
        }
        DeployAction::MaintenanceOn | DeployAction::MaintenanceOff => {
            let fleet = ResolvedFleet::resolve(&deploy_cfg, &resolve_project_name())
                .map_err(DeployError::Config)?;
            // Review round 3: the fan-out writes the flag file each host's LIVE slot
            // unit actually polls, and deciding WHICH slot that is maps the proxy's
            // routed target port through the public port. Resolved exactly as
            // `status` resolves it — same degraded fallback — because maintenance is
            // switched mid-incident too.
            let port = status_public_port(&resolved)?;
            run_fleet_maintenance_with(
                &fleet,
                &port,
                &resolved.profile,
                action,
                options.maintenance.as_ref(),
                |cfg| {
                    exec::SshTarget::from_resolved(cfg)
                        .map(exec::SshExecutor::new)
                        .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))
                },
            )
        }
        DeployAction::Up => {
            let (media_cfg, ffmpeg_bin) = load_media_host_config(&resolved)?;
            run_up(
                &load_runtime_config(&resolved)?,
                &resolved,
                &targets,
                host_list.len(),
                &media_cfg,
                &ffmpeg_bin,
                options,
            )
        }
    }
}

/// Load the `[media.mediamtx]` host-provisioning config and the `[media.ffmpeg]
/// bin` path from the project config, resolved **under the target deploy profile**
/// (issue #1974, Slice 7; issue #2051, Finding O).
///
/// Reads the raw project TOML and deserializes only the `[media]` subtree
/// (mirroring how `autumn-media-plugin` reads its own config), so it bypasses
/// `autumn-web`'s strict `AutumnConfig` schema and adds no dependency on the
/// plugin. Crucially it applies the **same base+profile TOML layering
/// `AutumnConfig::load()` applies to the app config** — base `autumn.toml`, then
/// the inline `[profile.<name>]` sections, then `autumn-<profile>.toml` — so a
/// profiled deploy (`prod`/`staging`) provisions `MediaMTX` from the profile's
/// `[media.mediamtx]` / `[media.ffmpeg]` (`[profile.<name>.media.*]` or
/// `autumn-<profile>.toml`) instead of the base default. Without this a
/// prod-enabled `[profile.prod.media.mediamtx]` (or `autumn-prod.toml`) would be
/// ignored and `plan`/`up` would see the disabled base config, so the release
/// boots under the target profile with no media daemon provisioned. The env-var
/// override layer (`AUTUMN_MEDIA__*`) is deliberately NOT applied here — an
/// env/interpolation-indirected value is resolved from the SERVICE'S OWN
/// environment at runtime (see [`media::ffmpeg_preflight`]'s fail-closed-honest
/// deferral), which the CLI neither has nor may guess.
///
/// A missing file (or one that cannot be read) yields the disabled default plus
/// the default `FFmpeg` path, so a project that does not use autumn-media is
/// unaffected.
///
/// **Fails closed on invalid media config.** A `[media.mediamtx]` /
/// `[media.ffmpeg]` table that is *present but does not deserialize* (a
/// wrong-typed value) **in any contributing layer** is an error, not a silent
/// fallback: the merged value carries the bad type and
/// [`media::media_host_config_from_value`] returns [`DeployError::Config`] so
/// `deploy plan`/`deploy up` abort with a clear message instead of proceeding
/// WITHOUT provisioning a media-enabled app's `MediaMTX` daemon. The disabled
/// default is reserved for the genuinely *absent* case.
fn load_media_host_config(
    resolved: &ResolvedDeployConfig,
) -> Result<(media::MediaMtxHostConfig, String), DeployError> {
    load_media_host_config_in(&manifest_project_dirs(), &resolved.profile)
}

/// Pure core of [`load_media_host_config`] over an explicit ordered search path
/// and RAW deploy-profile spelling, so the base+profile media layering is
/// unit-testable against temp dirs (mirrors [`manifest_uploads_in`]).
///
/// Builds a merged `[media]` subtree exactly as `AutumnConfig::load_with_env`
/// builds the app config (minus the env-override layer, see the caller docs):
/// base `autumn.toml` ← inline `[profile.<name>]` sections ←
/// `autumn-<profile>.toml`. The base `autumn.toml` is **optional** — an
/// absent/unreadable base skips only that layer, exactly like `load_with_env`,
/// so a deploy that keeps `[media.mediamtx] enabled = true` ONLY in
/// `autumn-<profile>.toml` (with `[deploy] host` supplied via env and no base
/// file) still resolves the profile override and provisions `MediaMTX` (Finding
/// R). When no layer contributes a `[media]` subtree the `#[serde(default)]`
/// `MediaTomlRoot` deserializes to the disabled default; a base or profile file
/// that does not parse, or a merged `[media]` subtree with a wrong-typed value,
/// is a fail-closed [`DeployError::Config`].
fn load_media_host_config_in(
    dirs: &[PathBuf],
    profile_raw: &str,
) -> Result<(media::MediaMtxHostConfig, String), DeployError> {
    // Start from an empty root and deep-merge each contributing layer in the same
    // order `AutumnConfig::load_with_env` does, so the deploy-side `[media]`
    // resolution matches the app/runtime resolution for every base/profile
    // combination. The runtime seeds `merged` with `profile_defaults_as_toml`, but
    // those smart defaults carry NO `[media]` keys, so an empty root is faithful
    // for the media subtree — an absent `[media]` deserializes to the disabled
    // default via the `#[serde(default)]` `MediaTomlRoot`. The env-override layer
    // is deliberately excluded (see the caller docs).
    let mut merged = toml::Value::Table(toml::map::Map::new());

    // Layer 3: base `autumn.toml` — OPTIONAL, exactly like `load_with_env`. A
    // missing or unreadable base skips only this layer (a project without
    // autumn-media is unaffected) but does NOT skip the profile override file
    // below; a base present-but-malformed is fail-closed.
    let base_toml: Option<toml::Value> = match first_dir_with_file(dirs, "autumn.toml") {
        Some(base_path) => match std::fs::read_to_string(&base_path) {
            Ok(base_str) => {
                let base: toml::Value = toml::from_str(&base_str).map_err(|e| {
                    DeployError::Config(format!("invalid config in {}: {e}", base_path.display()))
                })?;
                deep_merge_toml(&mut merged, base.clone());
                Some(base)
            }
            Err(_) => None,
        },
        None => None,
    };

    // Layer 4: inline `[profile.<name>]` sections in the base autumn.toml (only
    // present when the base parsed), in the runtime's alias-then-canonical merge
    // order (`production` then `prod`), so a `[profile.prod.media.mediamtx]` wins
    // over the base — matching `AutumnConfig::load_with_env`.
    let canonical = canonicalize_deploy_profile(profile_raw);
    if let Some(base) = &base_toml {
        for name in profile_inline_lookup_names(&canonical) {
            if let Some(section) = profile_section_from_base_toml(base, name) {
                deep_merge_toml(&mut merged, section);
            }
        }
    }

    // Layer 5: `autumn-<profile>.toml` (first existing in the ordered lookup wins,
    // reusing the runtime's own pure override-file name resolver so the deploy
    // reads the SAME profile file the host runtime loads).
    for name in autumn_web::config::profile_override_file_lookup_names(&canonical, profile_raw) {
        let basename = format!("autumn-{name}.toml");
        if let Some(path) = first_dir_with_file(dirs, &basename) {
            let overlay_str = std::fs::read_to_string(&path).map_err(|e| {
                DeployError::Config(format!("could not read {}: {e}", path.display()))
            })?;
            let overlay: toml::Value = toml::from_str(&overlay_str).map_err(|e| {
                DeployError::Config(format!("invalid config in {}: {e}", path.display()))
            })?;
            deep_merge_toml(&mut merged, overlay);
            break;
        }
    }

    // Deserialize the merged `[media]` subtree — fail-closed on an ill-typed value
    // in any contributing layer.
    media::media_host_config_from_value(merged).map_err(|e| {
        DeployError::Config(format!(
            "invalid [media] config for deploy profile `{profile_raw}`: {e}"
        ))
    })
}

/// Inline `[profile.<name>]` lookup names for the canonical profile, mirroring
/// `autumn-web`'s private `profile_lookup_names`: canonical profiles also pull
/// their legacy alias (`prod`→`production`,`prod`; `dev`→`development`,`dev`) so
/// a `[profile.production.media.mediamtx]` is honored for a `prod` deploy, while a
/// custom profile is looked up verbatim. Merged in list order (alias first) so the
/// canonical spelling wins — identical to the runtime. Kept a local mirror (like
/// [`canonicalize_deploy_profile`] mirrors `normalize_profile_name`) because the
/// runtime helper is private.
fn profile_inline_lookup_names(canonical: &str) -> Vec<&str> {
    match canonical {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Extract a `[profile.<name>]` table from a parsed `autumn.toml` value as a
/// standalone TOML value (mirrors `autumn-web`'s private
/// `profile_section_from_base_toml`). `None` when the section is absent.
fn profile_section_from_base_toml(base: &toml::Value, profile: &str) -> Option<toml::Value> {
    base.get("profile")
        .and_then(toml::Value::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(toml::Value::as_table)
        .map(|table| toml::Value::Table(table.clone()))
}

/// Deep-merge two TOML values — tables merged recursively, non-table `overlay`
/// values replace `base` (a faithful copy of `autumn-web`'s private `deep_merge`,
/// so the deploy-side media layering matches the runtime's app-config layering
/// exactly). Bounded recursion mirrors the runtime's `MAX_MERGE_DEPTH`.
fn deep_merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    deep_merge_toml_depth(base, overlay, 0);
}

fn deep_merge_toml_depth(base: &mut toml::Value, overlay: toml::Value, depth: usize) {
    /// Matches `autumn-web`'s `MAX_MERGE_DEPTH`.
    const MAX_MERGE_DEPTH: usize = 16;
    if depth > MAX_MERGE_DEPTH {
        return;
    }
    let toml::Value::Table(overlay_table) = overlay else {
        return;
    };
    let Some(base_table) = base.as_table_mut() else {
        return;
    };
    for (key, overlay_val) in overlay_table {
        let is_recursive_merge =
            overlay_val.is_table() && base_table.get(&key).is_some_and(toml::Value::is_table);
        if is_recursive_merge {
            if let Some(base_val) = base_table.get_mut(&key) {
                deep_merge_toml_depth(base_val, overlay_val, depth + 1);
            }
        } else {
            base_table.insert(key, overlay_val);
        }
    }
}

/// An [`Env`] that forces `AUTUMN_ENV` to a specific deploy profile while
/// delegating every other lookup to an inner env.
///
/// [`AutumnConfig::load_with_env`] re-derives the active profile from
/// `resolve_profile_input`, which reads `AUTUMN_ENV`.
/// [`autumn_web::dotenv::os_env_with_dotenv_for_profile`] only selects the
/// `.env.<profile>` overlay — it does NOT set `AUTUMN_ENV` — so on a dev box the
/// loader would still resolve `dev` and never layer `[profile.prod]` /
/// `autumn-prod.toml`. Reporting `AUTUMN_ENV=<profile>` here makes the reload
/// resolve the target deploy profile, so profile-scoped prod values (the signing
/// secret, the DB URL) are loaded, graded, and uploaded.
struct ForcedProfileEnv<E: Env> {
    /// The deploy profile forced onto `AUTUMN_ENV`.
    profile: String,
    /// Inner env every other key delegates to (the profile-aware dotenv overlay).
    inner: E,
}

impl<E: Env> Env for ForcedProfileEnv<E> {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        if key == "AUTUMN_ENV" {
            return Ok(self.profile.clone());
        }
        // Report `AUTUMN_DOTENV=1` so `should_load` opts the non-dev deploy profile into
        // `.env.<profile>` loading. The operator explicitly ran `autumn deploy` to gather
        // and upload the target profile's config, and `.env.<profile>` is a documented
        // place for profile-only values, so the deploy-time reload reads it without the
        // operator exporting `AUTUMN_DOTENV` by hand. This mutates neither the global env
        // nor the deployed service's runtime, and the profile-selector-key exclusion in
        // the overlay still strips `AUTUMN_ENV`/`AUTUMN_PROFILE`/`AUTUMN_IS_DEBUG` from
        // any `.env` file.
        //
        // Synthesize `1` only when the inner env has no explicit value: an operator
        // running `AUTUMN_DOTENV=0 autumn deploy ...` is deliberately opting out, and
        // `should_load` honors `0`/`false` as the documented off switch (see dotenv.rs).
        // So delegate to the inner env when it provides `AUTUMN_DOTENV`, preserving an
        // explicit value, and fall back to `1` only when it is unset.
        if key == "AUTUMN_DOTENV" {
            return self
                .inner
                .var("AUTUMN_DOTENV")
                .or_else(|_| Ok("1".to_owned()));
        }
        self.inner.var(key)
    }
}

/// Reload the deploy config under the TARGET deploy profile.
///
/// The chicken-and-egg here: the ambient load in [`run`] learns the deploy
/// profile (`resolved.profile`), and this reload then resolves the full config
/// under it. The `.env.<profile>` overlay is selected via
/// [`autumn_web::dotenv::os_env_with_dotenv_for_profile_using`], fed a
/// [`ForcedProfileEnv`] gating base that reports `AUTUMN_DOTENV=1` so a non-dev
/// deploy profile still loads `.env.<profile>` (dotenv auto-load is otherwise
/// gated off outside `dev`/`test`). The overlay is selected by the CANONICAL
/// profile ([`canonicalize_deploy_profile`]) so a `[deploy] profile` alias like
/// `production` still reads `.env.prod` (matching `AutumnConfig::load()`), not
/// `.env.production`. A second [`ForcedProfileEnv`] wrapper forces `AUTUMN_ENV`
/// to the RAW `resolved.profile` so the loader layers `[profile.<profile>]` /
/// `autumn-<profile>.toml` on top with the operator's exact spelling. Real OS
/// env vars still win over `.env` (the
/// overlay only fills gaps), and the dotenv profile-selector-key exclusion still
/// strips `AUTUMN_ENV`/`AUTUMN_PROFILE`/`AUTUMN_IS_DEBUG` from any `.env` file,
/// matching `AutumnConfig::load()`.
fn load_runtime_config(resolved: &ResolvedDeployConfig) -> Result<AutumnConfig, DeployError> {
    let forced = deploy_profile_env_overlay(&resolved.profile)?;
    // Lenient unknown top-level roots (#2063): keep strict validation of the
    // core sections the CLI knows while accepting plugin-owned roots (e.g.
    // `[media]`) as opaque — app boot stays the authoritative strict gate.
    AutumnConfig::load_with_env_lenient_unknown_roots(&forced)
        .map_err(|e| DeployError::Config(e.to_string()))
}

/// The public port `autumn deploy status` probes each host against, plus the
/// reason it had to be resolved the hard way (issue #1621, review round 1).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusPort {
    /// The port `[server] port` resolves to under the TARGET deploy profile.
    port: u16,
    /// `None` on the normal path (the whole runtime config validated). `Some` when
    /// the config did NOT validate under the deploy profile and the port was read
    /// from the layered TOML/env without validation — the caller must say so out
    /// loud rather than report against a silently-chosen port.
    degraded: Option<String>,
}

/// Resolve the probe port for `deploy status` WITHOUT making an unrelated
/// application-config error take the fleet's only read-only incident command
/// offline (issue #1621, review round 1).
///
/// `status` needs exactly one value from the application config: `server.port`,
/// the public port kamal-proxy binds — the number every per-host slot port is
/// derived from and the one the proxy-port-mismatch drift rule compares against.
/// Routing that through [`load_runtime_config`] validated the ENTIRE config under
/// the deploy profile, so an invalid `[scheduler]`, `[mail]`, `[database]` or
/// `[security]` setting aborted the command **before a single host was probed** —
/// exactly when an operator needs to see the fleet.
///
/// The strict load stays the primary path, so a project whose config is fine gets
/// byte-identical behaviour and the identical port. Only when it fails does this
/// fall back to reading the DECLARED port out of the same base+profile TOML layers
/// (plus the same `.env.<profile>`/OS env layer) the loader itself resolves it
/// from, and it reports the degradation. It never guesses: the fallback value is
/// the operator's own declared port, or the framework default when nothing
/// declares one.
///
/// `check`, `rollback` and `up` deliberately keep the strict load: they grade and
/// upload runtime VALUES (the signing secret, the DB URL), so an invalid config
/// must stop them. `plan` never loads the runtime config at all.
fn status_public_port(resolved: &ResolvedDeployConfig) -> Result<StatusPort, DeployError> {
    let loaded = load_runtime_config(resolved);
    status_public_port_with(&manifest_project_dirs(), &resolved.profile, loaded)
}

/// Pure core of [`status_public_port`] over an explicit search path and an
/// already-attempted load, so both branches are unit-testable against temp dirs.
fn status_public_port_with(
    dirs: &[PathBuf],
    profile_raw: &str,
    loaded: Result<AutumnConfig, DeployError>,
) -> Result<StatusPort, DeployError> {
    match loaded {
        Ok(config) => Ok(StatusPort {
            port: config.server.port,
            degraded: None,
        }),
        Err(err) => {
            // The `.env.<profile>` + OS env layer, selected exactly as the strict
            // load selects it — so an `AUTUMN_SERVER__PORT` that only exists in
            // `.env.prod` still wins here, as it would at runtime. A `.env` that
            // cannot be read is still fatal: that is not an unrelated app setting.
            let overlay = deploy_profile_env_overlay(profile_raw)?;
            Ok(StatusPort {
                port: declared_server_port_in(dirs, profile_raw, &overlay)
                    .unwrap_or_else(|| AutumnConfig::default().server.port),
                degraded: Some(err.to_string()),
            })
        }
    }
}

/// Read the DECLARED `[server] port` for a deploy profile without validating (or
/// even fully deserializing) the application config (issue #1621, review round 1).
///
/// Layers exactly as [`load_media_host_config_in`] does — base `autumn.toml` ←
/// inline `[profile.<name>]` ← `autumn-<profile>.toml` — because that is the same
/// order `AutumnConfig::load_with_env` merges them in, and then applies the env
/// layer ON TOP, because `AUTUMN_SERVER__PORT` is the highest-priority layer in the
/// loader too. So the number returned here is the number the strict load would
/// have produced, minus the validation.
///
/// `None` means no layer declares a port; the caller substitutes the framework
/// default rather than inventing one. A malformed/unreadable layer is skipped
/// rather than fatal: this function runs only on the already-degraded path, where
/// refusing again would leave `deploy status` just as unavailable as before.
fn declared_server_port_in(dirs: &[PathBuf], profile_raw: &str, env: &dyn Env) -> Option<u16> {
    // Highest layer first: `apply_env_overrides` applies `AUTUMN_SERVER__PORT`
    // last, so it wins over every TOML layer. An unparseable value is ignored for
    // the same reason the loader ignores it (it warns and keeps the layered value).
    if let Ok(raw) = env.var("AUTUMN_SERVER__PORT")
        && let Ok(port) = raw.trim().parse::<u16>()
    {
        return Some(port);
    }

    let mut merged = toml::Value::Table(toml::map::Map::new());
    let base_toml: Option<toml::Value> = first_dir_with_file(dirs, "autumn.toml")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok());
    let canonical = canonicalize_deploy_profile(profile_raw);
    if let Some(base) = &base_toml {
        deep_merge_toml(&mut merged, base.clone());
        for name in profile_inline_lookup_names(&canonical) {
            if let Some(section) = profile_section_from_base_toml(base, name) {
                deep_merge_toml(&mut merged, section);
            }
        }
    }
    for name in autumn_web::config::profile_override_file_lookup_names(&canonical, profile_raw) {
        let Some(path) = first_dir_with_file(dirs, &format!("autumn-{name}.toml")) else {
            continue;
        };
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(overlay) = toml::from_str::<toml::Value>(&text)
        {
            deep_merge_toml(&mut merged, overlay);
        }
        break;
    }

    merged
        .get("server")
        .and_then(|server| server.get("port"))
        .and_then(toml::Value::as_integer)
        .and_then(|port| u16::try_from(port).ok())
}

/// Build the profile-aware env overlay that [`load_runtime_config`] resolves the
/// deploy config under — the `.env.<profile>` overlay plus a forced
/// `AUTUMN_ENV` — WITHOUT loading [`AutumnConfig`].
///
/// `profile_raw` is the operator's trimmed RAW `[deploy] profile` spelling (as
/// stored on [`ResolvedDeployConfig::profile`] by [`trimmed_deploy_profile`]).
/// The `.env.<profile>` overlay is selected by the CANONICAL profile
/// ([`canonicalize_deploy_profile`]) so a `[deploy] profile` alias like
/// `production`/`PROD` still reads `.env.prod` (matching `AutumnConfig::load()`),
/// while the returned [`ForcedProfileEnv`] forces `AUTUMN_ENV` to the RAW
/// spelling so the loader layers `[profile.<profile>]` / `autumn-<profile>.toml`
/// under the operator's exact profile name, and reports `AUTUMN_DOTENV=1` so a
/// non-dev deploy profile still loads `.env.<profile>`.
///
/// `doctor` reuses this so its deploy secret/DB value graders resolve under the
/// `[deploy] profile` EXACTLY like `autumn deploy check` — same overlay
/// selection and same forced profile — rather than replicating (and risking
/// drift from) the layering.
///
/// # Errors
/// Returns [`DeployError::Config`] when a project-root `.env` file exists but
/// cannot be read or parsed.
// `pub(crate)`: reachable from `doctor`, kept crate-internal. In this bin-only
// crate `deploy` is a private module, so clippy flags the `pub(crate)` as
// redundant; we keep it to document the intended visibility.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn deploy_profile_env_overlay(profile_raw: &str) -> Result<impl Env, DeployError> {
    use autumn_web::config::OsEnv;
    // Gating base: the real OS env, but reports `AUTUMN_DOTENV=1` so
    // `should_load` loads `.env.<profile>` for a non-dev deploy profile —
    // without mutating the global process environment.
    let gating = ForcedProfileEnv {
        profile: profile_raw.to_owned(),
        inner: OsEnv,
    };
    // Select the `.env.<profile>` overlay by the CANONICAL profile, so a
    // `[deploy] profile` alias like `production`/`PROD` still picks the same
    // `.env.prod` file that `AutumnConfig::load()` reads after profile
    // normalization — not `.env.production`/`.env.PROD`. Only the dotenv-overlay
    // SELECTION is canonicalized; `AUTUMN_ENV` and the TOML override-file
    // precedence (handled by `load_with_env`) still see the RAW spelling below.
    let dotenv_profile = canonicalize_deploy_profile(profile_raw);
    let inner = autumn_web::dotenv::os_env_with_dotenv_for_profile_using(&gating, &dotenv_profile)
        .map_err(|e| DeployError::Config(e.to_string()))?;
    Ok(ForcedProfileEnv {
        profile: profile_raw.to_owned(),
        inner,
    })
}

/// Resolve the project's package name (for the `app_name` default), falling back
/// to the current directory name and finally to `"app"`.
fn resolve_project_name() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::release::read_project_name(&cwd)
        .ok()
        .or_else(|| {
            cwd.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| "app".to_owned())
}

/// Resolve the first *writable* database URL a deploy/migration would actually
/// act on, honoring shard-only deployments.
///
/// `autumn migrate` only ever targets a writable primary: the control
/// `primary_url`/`url`, or — for a shard-only app (one or more
/// `[[database.shards]]` with no control primary) — each shard's `primary_url`
/// (see `migrate::build_targets`). It never migrates against
/// `database.replica_url`. The DB-URL preflight must agree, so it returns the
/// first writable URL that resolves (control primary → first shard primary);
/// replicas are excluded because `autumn migrate` cannot migrate against a
/// replica. The grader never prints the value, so surfacing a shard URL here is
/// safe.
fn resolve_writable_db_url(db: &autumn_web::config::DatabaseConfig) -> Option<&str> {
    db.effective_primary_url()
        .or_else(|| db.shards.first().map(|shard| shard.primary_url.as_str()))
}

/// The release-relative `SQLite` data-file path this deploy must keep persistent,
/// or `None` when there is none (issue #1909).
///
/// Only the [`SqliteDataFile::Relative`] case needs relocating; an absolute path
/// already survives, and a refused configuration is caught by
/// [`grade_sqlite_data_persistence`] before any remote command runs.
fn sqlite_relative_data_file(
    config: &AutumnConfig,
    resolved: &ResolvedDeployConfig,
) -> Option<String> {
    match classify_sqlite_data_file(resolve_writable_db_url(&config.database), resolved) {
        SqliteDataFile::Relative(rel) => Some(rel),
        SqliteDataFile::NotSqlite | SqliteDataFile::Persistent(_) | SqliteDataFile::Refused(_) => {
            None
        }
    }
}

/// How a `SQLite` app's configured data file relates to the deploy layout
/// (issue #1909).
///
/// See [`classify_sqlite_data_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteDataFile {
    /// Not a `SQLite` app — nothing to persist.
    NotSqlite,
    /// A relative path, which resolves inside the release dir. The deploy keeps
    /// the real file under `shared/data` and links each release at this path.
    Relative(String),
    /// An absolute path the deploy does not manage — outside the app dir, or in
    /// its persistent `shared/`. Already release-independent, so the deploy
    /// leaves it exactly where the operator put it.
    Persistent(String),
    /// A configuration a deploy cannot make durable. Carries the operator-facing
    /// reason.
    Refused(String),
}

/// Classify the writable database URL a deploy would ship (issue #1909).
///
/// Each refusal names a configuration whose data cannot survive a deploy.
/// Preflight catches it before any remote command runs, not after the first
/// cutover.
#[must_use]
pub fn classify_sqlite_data_file(url: Option<&str>, cfg: &ResolvedDeployConfig) -> SqliteDataFile {
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return SqliteDataFile::NotSqlite;
    };
    if autumn_web::config::DatabaseBackend::detect(url)
        != Some(autumn_web::config::DatabaseBackend::Sqlite)
    {
        return SqliteDataFile::NotSqlite;
    }
    let Some(path) = autumn_web::replication::database_file(url) else {
        return SqliteDataFile::Refused(
            "the configured SQLite database is IN-MEMORY, so it cannot survive a deploy \
             (or a restart). Point `[database] url` at a file, e.g. \
             sqlite:///srv/autumn/<app>/shared/data/app.db"
                .to_owned(),
        );
    };
    // A deploy addresses paths as text — in a systemd unit, in a shell line — so
    // one it cannot render is one it cannot ship. `file:app%FF.db` decodes to such
    // a name.
    let Some(raw) = path.to_str().map(str::to_owned) else {
        return SqliteDataFile::Refused(format!(
            "the configured SQLite database path ({}) is not valid UTF-8, so a deploy \
             cannot address it. Rename the file.",
            path.display()
        ));
    };
    // Normalize BEFORE any containment check: a lexical prefix compare on
    // `/srv/app/shared/../releases/r1/app.db` would call it durable, while the
    // kernel resolves it into `releases/`, where retention deletes it.
    let Some(text) = lexically_normalized(&raw) else {
        return SqliteDataFile::Refused(format!(
            "the configured SQLite database path {raw} climbs above the filesystem root."
        ));
    };
    if text.starts_with('/') {
        // Anything the deploy itself manages is transient — `releases/` is
        // replaced every deploy and pruned by retention, and `current` is just a
        // symlink into it. Only `shared/` survives, so only `shared/` is a
        // durable place inside the app dir. A path outside the app dir is the
        // operator's own and is left alone.
        //
        // `app_dir` is normalized too, not just the database path: `[deploy]
        // app_dir = "/srv/autumn/tmp/../myapp"` names the same directory as
        // `/srv/autumn/myapp`, and comparing the raw spelling would miss a
        // database sitting in the releases dir it resolves to.
        let app_dir = lexically_normalized(&cfg.app_dir).unwrap_or_default();
        let shared = lexically_normalized(&cfg.shared_dir()).unwrap_or_default();
        // An app dir that is not an absolute path grades nothing — `within` on an
        // empty root would swallow every absolute path.
        if app_dir.starts_with('/') && within(&text, &app_dir) && !within(&text, &shared) {
            return SqliteDataFile::Refused(format!(
                "the configured SQLite database {text} lives inside the deploy's own app \
                 directory ({app_dir}), where only `shared/` survives: `releases/` is \
                 replaced on every deploy and deleted by release retention, and `current` \
                 is a symlink into it. Move the file to {}, or outside {app_dir} \
                 altogether.",
                cfg.shared_data_dir()
            ));
        }
        return SqliteDataFile::Persistent(text);
    }
    // A relative path is resolved against the slot unit's `WorkingDirectory`,
    // which is the release dir. Normalization above already removed `.` and
    // resolved `..`; what is left must still name a file INSIDE the release dir,
    // so a leading `..` (which normalization keeps, having nothing to cancel it
    // against) and an empty path are both refused.
    if text.is_empty() || text == ".." || text.starts_with("../") {
        return SqliteDataFile::Refused(format!(
            "the configured SQLite database path {text:?} does not name a file inside the \
             release directory it is resolved against, so a deploy cannot keep it durable. \
             Use a plain relative name (sqlite://app.db) or an absolute path."
        ));
    }
    SqliteDataFile::Relative(text)
}

/// Collapse `.` and `..` lexically, and normalize separators.
///
/// Deliberately a STRING walk, not `std::path`: the path names a file on the
/// deploy TARGET, which is always a POSIX host, while `std::path` follows the
/// host this CLI runs on. On Windows `Path::new("/var/lib/app.db").is_absolute()`
/// is false, so every containment rule below would grade a Linux absolute path as
/// a relative one — `autumn deploy check` from a Windows workstation would
/// misjudge the target it is about to deploy to.
///
/// Lexical, not `canonicalize`: this process cannot stat the target. That is
/// enough for the containment rules — it closes the `shared/../releases` hole —
/// and a symlink on the host that defeats it would defeat any check made here.
///
/// Collapsing `..` is safe here because `SQLite` collapses it too, in its own
/// VFS, BEFORE calling `open(2)`. The deploy therefore links only the collapsed
/// path — `data/nested/../app.db` creates `data`, not `data/nested` — and the app
/// still opens the file the deploy linked. Raw `open(2)` would NOT: POSIX
/// resolution needs `data/nested` to exist. So this is load-bearing on `SQLite`'s
/// behaviour, not the kernel's. Re-check it before supporting a VFS that does not
/// normalize lexically.
///
/// Returns `None` when the path climbs above the filesystem root.
fn lexically_normalized(path: &str) -> Option<String> {
    let absolute = path.starts_with('/');
    let mut names: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            // An empty segment is a repeated or trailing `/`.
            "" | "." => {}
            ".." => {
                // A leading `..` on a RELATIVE path has nothing to cancel and is
                // kept, so the caller can refuse it; on an absolute path it would
                // climb above the root.
                if names.last().is_some_and(|name| *name != "..") {
                    names.pop();
                } else if absolute {
                    return None;
                } else {
                    names.push("..");
                }
            }
            name => names.push(name),
        }
    }
    let joined = names.join("/");
    Some(if absolute {
        format!("/{joined}")
    } else {
        joined
    })
}

/// Whether `path` is `root` itself or sits under it. Compares whole path
/// components, so `/srv/app/releases-archive` is not "under" `/srv/app/releases`.
fn within(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

/// Preflight grader for `SQLite` data-file durability (issue #1909).
///
/// Only emitted for a `SQLite` app, so a Postgres deploy's preflight report is
/// unchanged.
#[must_use]
pub fn grade_sqlite_data_persistence(
    url: Option<&str>,
    cfg: &ResolvedDeployConfig,
) -> Option<PreflightCheck> {
    match classify_sqlite_data_file(url, cfg) {
        SqliteDataFile::NotSqlite => None,
        SqliteDataFile::Relative(rel) => Some(PreflightCheck::pass(
            "sqlite_data_file",
            format!(
                "SQLite data file kept in {}/{rel} across releases",
                cfg.shared_data_dir()
            ),
        )),
        SqliteDataFile::Persistent(path) => Some(PreflightCheck::pass(
            "sqlite_data_file",
            format!("SQLite data file {path} is outside the releases directory"),
        )),
        SqliteDataFile::Refused(detail) => Some(PreflightCheck::fail(
            "sqlite_data_file",
            detail,
            "Set `[database] url` to a path a deploy can keep: a plain relative name \
             (sqlite://app.db, kept under the app's shared/data dir) or an absolute path \
             outside the releases directory",
        )),
    }
}

/// Collect all preflight graders for the resolved config against the loaded
/// runtime configuration.
///
/// Exactly ONE of the four graders is host-specific (`ssh_reachability`); the
/// other three grade the project, not the target. The split is made explicit by
/// [`collect_project_preflight`] so a fleet can run the host grader per host and
/// the project graders once ([`collect_fleet_preflight`]) without re-deriving —
/// and so a single-host run keeps this exact order and text.
fn collect_preflight(
    config: &AutumnConfig,
    resolved: &ResolvedDeployConfig,
) -> Vec<PreflightCheck> {
    let mut checks = vec![grade_ssh_reachability(
        resolved.host.as_deref(),
        resolved.ssh_port,
        SSH_PROBE_TIMEOUT,
    )];
    checks.extend(collect_project_preflight(config, resolved));
    checks
}

/// The preflight graders that are fleet-wide by nature: they grade the project's
/// signing secret, database URL and pending migrations, none of which vary per
/// host. A fleet runs these ONCE (issue #1621, AC-7).
fn collect_project_preflight(
    config: &AutumnConfig,
    resolved: &ResolvedDeployConfig,
) -> Vec<PreflightCheck> {
    // A `[database]` is considered configured when any connection URL is set on
    // the loaded config (primary/compat `url`, a replica-only role, or any
    // `[[database.shards]]` entry). Combined with the migrations-dir presence
    // check inside `grade_database_url`, this marks the app as database-backed
    // so a missing URL fails preflight — while a DB-free app (no URLs, no
    // shards, no migrations) passes.
    let database_configured = config.database.url.is_some()
        || config.database.primary_url.is_some()
        || config.database.replica_url.is_some()
        || config.database.has_shards();
    let mut checks = vec![
        // Grade the signing secret against the SAME profile that will be written
        // to the host env file (`AUTUMN_ENV=<resolved.profile>`, default `prod`),
        // not the local CLI runtime profile (`config.profile`, dev/None on a dev
        // box). Otherwise a weak/demo secret passes preflight locally but the
        // uploaded unit boots under `prod` and exits in
        // `fail_fast_on_invalid_signing_secret` — failing the deploy only AFTER
        // touching the host. Keeping the grader and env-file profile coherent
        // catches the weak secret locally, before any remote call.
        grade_signing_secret(
            config.security.signing_secret.secret.as_deref(),
            &config.security.signing_secret.previous_secrets,
            // `resolved.profile` holds the operator's trimmed RAW spelling (so the
            // AUTUMN_ENV value preserves override-file precedence). Normalize it to
            // the canonical profile only here, for grading, so `PROD`/`Production`/
            // ` production ` are still held to the production strength rules.
            is_production_profile(Some(&canonicalize_deploy_profile(&resolved.profile))),
        ),
        grade_database_url(
            resolve_writable_db_url(&config.database),
            Path::new(MIGRATIONS_DIR),
            database_configured,
            requires_database_pool(config),
        ),
        grade_migrate_check(Path::new(MIGRATIONS_DIR)),
    ];
    // Only a SQLite app gets this row, so a Postgres deploy's report is
    // byte-identical to pre-#1909.
    checks.extend(grade_sqlite_data_persistence(
        resolve_writable_db_url(&config.database),
        resolved,
    ));
    checks
}

/// Preflight for a whole fleet: `ssh_reachability` **per host**, then the three
/// project-wide graders **once** (issue #1621, AC-7).
///
/// A single-host fleet returns exactly [`collect_preflight`]'s vector, so its
/// report is byte-identical to pre-#1621 (AC-1). For a real fleet each per-host row
/// additionally carries [`PreflightCheck::scope`], so `report_preflight` can render
/// `✅ ssh_reachability (web-2): …` without the grader NAME — a `doctor --json`
/// contract — ever becoming host-specific.
///
/// **No fleet grader ever returns `deferred`** (plan §5.3, pinned by
/// `no_fleet_preflight_grader_returns_deferred`): `report_preflight` treats deferred
/// as a non-blocking warning while doctor maps it to a hard `Fail`, so a deferring
/// fleet grader would make `doctor --strict` reject a config `deploy check` accepts.
/// These graders pass or they fail.
///
/// The probes run SERIALLY here: they are a bounded TCP connect each, the report
/// must come out in declaration order, and threading them would put a `Sync` bound
/// on machinery the `RefCell`-based recording fake cannot satisfy. Parallelising
/// the pure TCP graders is a later, isolated change.
fn collect_fleet_preflight(
    config: &AutumnConfig,
    fleet: &ResolvedFleet,
    configured_host_count: usize,
) -> Vec<PreflightCheck> {
    let Some(shared) = fleet.hosts.first() else {
        // A fleet with no entries at all (#2274). `ResolvedFleet::from_targets` keeps a
        // `None`-host entry rather than dropping it, so an unconfigured `[deploy]`
        // reaches the arm below and reports `ssh_reachability` normally, leaving this
        // arm unreachable today. It still must not return no checks: this function is
        // the fail-fast boundary — `run_up`, `run_check`, and `run_rollback` all abort
        // on the count it produces — and zero checks reads as "0 failed", walks the run
        // past that boundary, and leaves the `fleet.hosts[0]` sites downstream to panic
        // on an empty vec. `hosts` is a `pub` field, so the non-empty promise in its doc
        // comment is not a type: the gate fails closed, in the same words the
        // single-host path uses.
        return vec![grade_ssh_reachability(None, 0, SSH_PROBE_TIMEOUT)];
    };
    if is_single_host_deploy(fleet, configured_host_count) {
        return collect_preflight(config, shared);
    }
    let mut checks: Vec<PreflightCheck> = fleet
        .hosts
        .iter()
        .map(|cfg| {
            grade_ssh_reachability(cfg.host.as_deref(), cfg.ssh_port, SSH_PROBE_TIMEOUT)
                // Scope only ever names a host in a REAL fleet — the single-host
                // branch above returns before this point.
                .scoped(cfg.host.clone())
        })
        .collect();
    checks.extend(collect_project_preflight(config, shared));
    checks
}

/// Whether the active profile is production, matching the exact rule the app boot
/// path uses in `fail_fast_on_invalid_signing_secret`
/// (`matches!(config.profile.as_deref(), Some("prod" | "production"))`).
#[must_use]
// `pub(crate)` (not `pub`): reachable from `doctor` for deploy-profile grading
// but kept crate-internal. See the note on `canonicalize_deploy_profile` re: the
// clippy allow (private module in a bin-only crate).
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn is_production_profile(profile: Option<&str>) -> bool {
    matches!(profile, Some("prod" | "production"))
}

/// Whether any enabled runtime feature requires a configured Postgres pool at
/// startup, mirroring the exact backend conditions the runtime enforces:
/// - `jobs.backend = "postgres"` → `job::start_postgres_runtime` calls
///   `state.pool().ok_or(...)` and errors without a pool.
/// - `scheduler.backend = "postgres"` → `scheduler::coordinator_from_config`
///   calls `state.pool().ok_or(...)` and errors without a pool.
///
/// Cache, channels, and idempotency have only in-memory/Redis backends (no
/// Postgres variant), so they never require a DB pool and are deliberately not
/// included here.
#[must_use]
fn requires_database_pool(config: &AutumnConfig) -> bool {
    config.jobs.backend == "postgres"
        || config.scheduler.backend == autumn_web::config::SchedulerBackend::Postgres
}

/// The label a preflight line leads with: the stable grader name, plus the fleet
/// host in parentheses when the check is scoped (issue #1621, plan §5.2).
///
/// Pure so the fleet rendering rule is testable without capturing stderr. An
/// unscoped check (every check in a single-host deploy, and every project-wide
/// grader in a fleet) renders as the bare name — byte-identical to pre-#1621.
fn preflight_label(check: &PreflightCheck) -> String {
    check.scope.as_ref().map_or_else(
        || check.name.to_owned(),
        |scope| format!("{} ({scope})", check.name),
    )
}

/// Print each preflight check as a pass/fail line and return the failure count.
/// Shared by `check` and `up` so both surfaces report graders identically.
fn report_preflight(checks: &[PreflightCheck]) -> usize {
    let mut failed = 0_usize;
    for check in checks {
        let label = preflight_label(check);
        if check.passed {
            eprintln!("\u{2705} {label}: {}", check.detail);
        } else if check.deferred {
            // Non-blocking: verified in the service's own runtime, not here.
            // Surfaced as a warning so it is visible but never aborts the deploy.
            eprintln!("\u{26A0}\u{FE0F}  {label}: {}", check.detail);
            if let Some(hint) = check.hint {
                eprintln!("   \u{2192} {hint}");
            }
        } else {
            failed += 1;
            eprintln!("\u{274C} {label}: {}", check.detail);
            if let Some(hint) = check.hint {
                eprintln!("   \u{2192} {hint}");
            }
        }
    }
    eprintln!();
    failed
}

/// Run the preflight over EVERY configured host and report it (issue #1621, AC-7).
///
/// `hosts` is the ordered target list from [`deploy_host_list`]. With more than one
/// entry the per-host `ssh_reachability` grader runs once per host — so an
/// unreachable host in position 3 is named before any host is touched — and the
/// three project-wide graders run once. With at most one entry this is the
/// pre-#1621 single-host report verbatim, down to the absent `scope` suffix (AC-1;
/// proved by `deploy_check_output_is_identical_for_host_and_a_single_entry_hosts_list`).
///
/// Also prints the same config-manifest notice `deploy up` prints (#1952
/// check/up parity) — a confirmation naming the manifest(s) that will be
/// uploaded, or a loud warning when there is no local `autumn.toml` at all.
/// Purely informational: it is not one of the graded `checks` and never
/// affects the failure count or exit code.
fn run_check(
    config: &AutumnConfig,
    resolved: &ResolvedDeployConfig,
    hosts: &[String],
    configured_host_count: usize,
) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy check\n");

    let fleet = ResolvedFleet::from_targets(resolved, hosts);
    let checks = collect_fleet_preflight(config, &fleet, configured_host_count);
    let failed = report_preflight(&checks);

    // Mirror `deploy up`'s config-manifest signal here too (#1952 check/up
    // parity): `check` is the documented way to catch a broken deploy BEFORE
    // touching the server, so an operator running only `check` must see the same
    // "no autumn.toml found" warning (or upload confirmation) `up` would print,
    // not discover it only once `up` actually runs. Purely informational — it
    // never affects `failed`/the exit code, exactly like `up` never blocks on it.
    eprintln!(
        "{}",
        manifest_preflight_notice(&locate_manifest_uploads(resolved))
    );

    if failed == 0 {
        eprintln!("\u{2705} All {} preflight check(s) passed.", checks.len());
        Ok(())
    } else {
        eprintln!(
            "\u{274C} {failed} of {} preflight check(s) failed.",
            checks.len()
        );
        Err(DeployError::PreflightFailed(failed))
    }
}

/// Print the `deploy plan` dry-run: the rendered slot unit, the ordered per-host
/// steps, the fleet rollout section (multi-host only), and the `MediaMTX` section
/// (media-enabled only).
///
/// `hosts` is the ordered target list from [`deploy_host_list`] — the rollout
/// order. When it holds at most one entry, nothing fleet-specific is printed, so
/// single-host output stays byte-identical to pre-#1621 (AC-1); the differential
/// proof is `deploy_plan_output_is_identical_for_host_and_a_single_entry_hosts_list`.
fn print_plan(
    resolved: &ResolvedDeployConfig,
    hosts: &[String],
    media_cfg: &media::MediaMtxHostConfig,
) {
    println!("\u{1F342} autumn deploy plan (dry-run)\n");
    println!("systemd unit ({}.service):\n", resolved.service_name);
    println!("{}", render_systemd_unit(resolved));

    println!("Deploy steps (zero-downtime):");
    for (i, step) in build_deploy_plan(resolved).iter().enumerate() {
        println!("  {}. [{}] {}", i + 1, step.label, step.description);
    }

    // Fleet rollout (issue #1621, AC-4): the steps above are what EACH host runs;
    // this section is the order they run in and where the single fleet-wide
    // migration lands. Printed only for a real fleet.
    if hosts.len() > 1 {
        for line in fleet::fleet_plan_lines(hosts) {
            println!("{line}");
        }
    }

    // MediaMTX host provisioning (issue #1974, Slice 7): only when the
    // `[media.mediamtx]` section is enabled — a non-media deploy prints nothing
    // new here.
    if media_cfg.enabled {
        print_media_plan(media_cfg);
    }
}

/// Print the `MediaMTX` host-provisioning section of `deploy plan` — the rendered
/// systemd unit, the ordered provisioning ops, and the CSP origins the app must
/// allow. Pure output; runs nothing.
fn print_media_plan(media_cfg: &media::MediaMtxHostConfig) {
    let controller = media::MediaMtxController::new(media_cfg.clone());
    println!(
        "\nMediaMTX host provisioning (media unit {}.service):\n",
        media_cfg.unit_name
    );
    println!("{}", media::render_mediamtx_unit(media_cfg));
    println!("MediaMTX provisioning steps:");
    for (i, op) in controller.ensure_installed_ops().iter().enumerate() {
        println!("  {}. [{}]", i + 1, op.label());
    }
    println!(
        "\nMediaMTX doctor checks at `deploy up`: {}, {}, {}, {}, {}",
        media::CHECK_MEDIAMTX_PORTS_DISTINCT,
        media::CHECK_FFMPEG_PREFLIGHT,
        media::CHECK_MEDIAMTX_BINARY,
        media::CHECK_RECORDINGS_DIR_WRITABLE,
        media::CHECK_MEDIAMTX_PORTS_AVAILABLE,
    );
    println!(
        "CSP: the app must allow these MediaMTX origins in connect-src/media-src \
         (frame-src for WebRTC): {}",
        media_cfg.required_csp_origins().join(", "),
    );
}

/// Grade the media host with the fail-closed, **non-mutating** doctor checks
/// (`FFmpeg` preflight, `MediaMTX` binary, recordings dir writable, ports
/// distinct/available) and abort the deploy on any **blocking** failure — so a
/// host that cannot serve media fails fast BEFORE the app deploy touches
/// anything. A **deferred** check (an env/interpolation-indirected
/// `[media.ffmpeg] bin` the deployed service resolves from its own runtime
/// environment) is surfaced as a warning and does **not** abort — the service
/// resolves `FFmpeg` at runtime, so blocking the deploy here would be wrong,
/// while a concrete literal `FFmpeg` path that is missing/not-executable still
/// fails closed (see [`media::ffmpeg_preflight`] and
/// [`PreflightCheck::blocking`]). A no-op when `[media.mediamtx]` is not enabled.
/// This writes and restarts nothing; the mutating provisioning is deferred to
/// [`provision_media_host`], which runs only after the app cutover succeeds.
fn check_media_host_preflight(
    media_cfg: &media::MediaMtxHostConfig,
    ffmpeg_bin: &str,
    executor: &impl exec::DeployExecutor,
) -> Result<(), DeployError> {
    if !media_cfg.enabled {
        return Ok(());
    }
    eprintln!("\u{1F3A5} media host preflight (autumn-media)\n");

    let checks = media::collect_media_doctor_checks(executor, media_cfg, ffmpeg_bin);
    let failed = report_preflight(&checks);
    if failed > 0 {
        return Err(DeployError::PreflightFailed(failed));
    }
    Ok(())
}

/// Provision `MediaMTX` as a systemd unit over the live executor — write the
/// rendered `mediamtx.yml` + unit and enable/restart the service (issue #1974,
/// Slice 7). A no-op when `[media.mediamtx]` is not enabled.
///
/// **Runs only AFTER the app deploy/cutover has fully succeeded.** The app deploy
/// rolls back on failure (readiness-gate miss, migration failure) and leaves the
/// OLD release serving; there is deliberately no media teardown restoring the
/// previous `mediamtx.yml`, so mutating (and possibly restarting) the host
/// `MediaMTX` unit BEFORE the app is committed would strand a rolled-back release
/// against a media daemon whose ports/recording-paths/matchers just moved.
/// Deferring the mutation until the deploy is committed means a failed/rolled-back
/// app deploy never writes or restarts `MediaMTX`. The non-mutating doctor checks
/// already ran up front in [`check_media_host_preflight`].
fn provision_media_host(
    media_cfg: &media::MediaMtxHostConfig,
    executor: &impl exec::DeployExecutor,
) -> Result<(), DeployError> {
    if !media_cfg.enabled {
        return Ok(());
    }
    eprintln!("\u{1F3A5} provisioning MediaMTX host (autumn-media)\n");

    let controller = media::MediaMtxController::new(media_cfg.clone());
    exec::run_ops(&controller.ensure_installed_ops(), executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;
    eprintln!(
        "\u{2705} MediaMTX provisioned ({}.service)",
        media_cfg.unit_name
    );
    Ok(())
}

/// Build the secret env-file body sourced by the systemd unit's
/// `EnvironmentFile`. Emits the runtime profile selector (`AUTUMN_ENV`, from the
/// resolved `[deploy] profile`, default `prod`) so the deployed app never
/// silently boots under the `dev` fallback, alongside the values the app needs
/// at runtime (the signing secret and the writable database URL). The result is
/// wrapped in [`exec::Secret`] so it is never logged (AC-5). The profile is a
/// plain non-secret value; secret handling is unchanged.
fn build_env_file(config: &AutumnConfig, resolved: &ResolvedDeployConfig) -> exec::Secret {
    let mut body = String::new();
    body.push_str("AUTUMN_ENV=");
    body.push_str(&resolved.profile);
    body.push('\n');
    if let Some(secret) = config
        .security
        .signing_secret
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body.push_str("AUTUMN_SECURITY__SIGNING_SECRET=");
        body.push_str(secret);
        body.push('\n');
    }
    if let Some(url) = resolve_writable_db_url(&config.database) {
        body.push_str("AUTUMN_DATABASE__URL=");
        body.push_str(url);
        body.push('\n');
    }
    exec::Secret::new(body)
}

/// Resolve the local path to the pre-built release binary this deploy uploads.
///
/// Slice 1 does NOT rebuild from source — it uploads the standalone binary
/// produced by `autumn build --embed` at `target/release/{app_name}`, failing
/// with an actionable error when it is missing.
fn resolve_release_binary(resolved: &ResolvedDeployConfig) -> Result<PathBuf, DeployError> {
    let path = PathBuf::from("target")
        .join("release")
        .join(&resolved.app_name);
    if path.exists() {
        Ok(path)
    } else {
        Err(DeployError::BinaryMissing(path.display().to_string()))
    }
}

/// Deterministic-per-second release id used to name the timestamped release dir.
/// Nondeterministic by design in production; tests inject a fixed id instead
/// through [`exec::first_deploy_ops`].
fn default_release_id() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Resolve the ordered local project directories the deploy reads config from.
///
/// Mirrors autumn-web's `find_config_file_named`, which is a *per-file*
/// manifest-dir-then-CWD fallback: `{AUTUMN_MANIFEST_DIR}/{file}` wins when it
/// exists, otherwise `{file}` relative to the CWD. So when `AUTUMN_MANIFEST_DIR`
/// is set we return `[manifest_dir, cwd]` (a file absent from the manifest dir
/// falls back to the CWD copy, exactly as the runtime would load it); when it is
/// unset we return `[cwd]` — the project root the rest of the deploy already
/// treats as authoritative (the release binary is resolved as
/// `target/release/<app>`).
fn manifest_project_dirs() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Ok(dir) = std::env::var("AUTUMN_MANIFEST_DIR")
        && !dir.trim().is_empty()
    {
        let manifest_dir = PathBuf::from(dir);
        if manifest_dir == cwd {
            return vec![cwd];
        }
        return vec![manifest_dir, cwd];
    }
    vec![cwd]
}

/// First directory in `dirs` that holds `filename` as a regular file — the
/// deploy-side mirror of the runtime's `find_config_file_named` per-file
/// manifest-dir-then-CWD fallback.
fn first_dir_with_file(dirs: &[PathBuf], filename: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(filename)).find(|p| p.is_file())
}

/// Locate the RAW project config manifest file(s) to upload for #1952: the base
/// `autumn.toml` plus, when present, the profile-override sibling
/// `autumn-<profile>.toml` for the target deploy profile.
///
/// Pure over the passed `dirs`/`profile` so it is unit-testable against temp
/// dirs. We upload the raw files (NOT a flattened/merged config) because the app
/// applies its `[profile.<AUTUMN_ENV>]` overlay at runtime — `AUTUMN_ENV` is already
/// set in the uploaded env file — so shipping the raw manifest(s) preserves the
/// profile structure and matches the repo exactly.
///
/// The sibling lookup reuses the runtime's own pure profile helpers
/// (`autumn_web::config::normalize_profile_name` +
/// `profile_override_file_lookup_names`) so the deploy picks the SAME
/// `autumn-<profile>.toml` the host runtime will load — a single source of truth
/// prevents drift. The ordered lookup list is first-existing-wins (e.g.
/// `[deploy] profile = "Production"` prefers `autumn-production.toml` over
/// `autumn-prod.toml`, matching the runtime). `dirs` is the ordered
/// manifest-dir-then-CWD search path from [`manifest_project_dirs`]; each file is
/// resolved against it with the same per-file fallback the runtime's
/// `find_config_file_named` applies.
fn manifest_uploads_in(dirs: &[PathBuf], profile: &str) -> Vec<exec::ManifestUpload> {
    let mut uploads = Vec::new();

    if let Some(base) = first_dir_with_file(dirs, "autumn.toml") {
        uploads.push(exec::ManifestUpload {
            local: base,
            remote_basename: "autumn.toml".to_owned(),
        });
    }

    // Mirror the runtime: normalize the raw profile, then walk the ordered
    // override-file lookup names and upload the FIRST that exists locally (under
    // its own name), exactly as the host runtime loads the first that exists and
    // stops. Empty profile falls back to the deploy default `"prod"` (matching
    // `canonicalize_deploy_profile` / `default_deploy_profile`).
    let normalized =
        autumn_web::config::normalize_profile_name(profile).unwrap_or_else(|| "prod".to_owned());
    for name in autumn_web::config::profile_override_file_lookup_names(&normalized, profile) {
        let basename = format!("autumn-{name}.toml");
        if let Some(local) = first_dir_with_file(dirs, &basename) {
            uploads.push(exec::ManifestUpload {
                local,
                remote_basename: basename,
            });
            break;
        }
    }

    // The capacity contract the deployed config points at (#1733). Without
    // this the release ships an `autumn.toml` naming a `capacity_contract`
    // that is not there, the remote load fails, and — because contract
    // failures deliberately fall open — the advertised admission control is
    // silently absent on exactly the deploy path the guide recommends.
    if let Some(configured) = configured_capacity_contract(dirs, profile) {
        // `ManifestUpload` places every file flat in the release directory,
        // but the uploaded config still names the path it was written with. A
        // `contracts/prod.lock` uploaded as `prod.lock` would therefore be
        // missing at the path the app looks in — the same silent fall-open
        // this block exists to prevent. Ship only a contract that already sits
        // beside the manifest; anything nested is left to the operator, loudly.
        if Path::new(&configured)
            .parent()
            .is_some_and(|p| p != Path::new(""))
        {
            eprintln!(
                "\u{26A0}\u{FE0F}  warning: [server] capacity_contract is '{configured}', which is \
                 not beside autumn.toml.\n   The release directory is flat, so this file is NOT \
                 uploaded and the deployed app will\n   fall back to unlimited admission. Move the \
                 contract next to autumn.toml, or ship it\n   yourself as part of your release."
            );
        } else if let Some(local) = first_dir_with_file(dirs, &configured) {
            uploads.push(exec::ManifestUpload {
                local,
                remote_basename: configured,
            });
        }
    }

    uploads
}

/// `[server] capacity_contract` as the deployed config will see it.
///
/// Layers exactly as the runtime does — base `autumn.toml`, then the inline
/// `[profile.<name>]` sections of that same file, then the
/// `autumn-<profile>.toml` sibling — reusing the same helpers the media-host
/// resolution already mirrors. Getting this wrong is not cosmetic: a contract
/// configured only under `[profile.prod.server]` would be left out of the
/// release while the deployed runtime still resolves it, and admission control
/// would fall open with no signal.
///
/// A key present but empty clears the setting, matching the env override.
fn configured_capacity_contract(dirs: &[PathBuf], profile: &str) -> Option<String> {
    // The base file is OPTIONAL, exactly as it is for the runtime loader: a
    // deployment may legitimately ship only `autumn-prod.toml`. Requiring it
    // here would skip the contract for such a deployment while
    // `manifest_uploads_in` still uploaded the profile file that references
    // it — the config would name a contract that is not in the release, and
    // admission would fall open.
    let mut merged = first_dir_with_file(dirs, "autumn.toml")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));

    let canonical =
        autumn_web::config::normalize_profile_name(profile).unwrap_or_else(|| "prod".to_owned());

    // Inline `[profile.<name>]` overlays from the base file, alias first so the
    // canonical spelling wins — identical to the runtime.
    for name in profile_inline_lookup_names(&canonical) {
        if let Some(section) = profile_section_from_base_toml(&merged.clone(), name) {
            deep_merge_toml(&mut merged, section);
        }
    }

    // Then the first existing `autumn-<profile>.toml` sibling.
    for name in autumn_web::config::profile_override_file_lookup_names(&canonical, profile) {
        if let Some(local) = first_dir_with_file(dirs, &format!("autumn-{name}.toml")) {
            if let Ok(text) = std::fs::read_to_string(&local)
                && let Ok(overlay) = toml::from_str::<toml::Value>(&text)
            {
                deep_merge_toml(&mut merged, overlay);
            }
            break;
        }
    }

    merged
        .get("server")
        .and_then(|server| server.get("capacity_contract"))
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Locate the config manifest(s) to upload for the resolved deploy, reading from
/// the local project directories ([`manifest_project_dirs`]).
fn locate_manifest_uploads(resolved: &ResolvedDeployConfig) -> Vec<exec::ManifestUpload> {
    manifest_uploads_in(&manifest_project_dirs(), &resolved.profile)
}

/// Format the operator-facing preflight line for the config-manifest upload
/// (#1952). Pure/testable: either a LOUD no-manifest warning (kills the previous
/// silent-defaults footgun) or a confirming line naming the uploaded file(s).
fn manifest_preflight_notice(uploads: &[exec::ManifestUpload]) -> String {
    if uploads.is_empty() {
        return "\u{26A0}\u{FE0F}  warning: no autumn.toml found in the project directory; the \
                deployed app will run built-in defaults for all non-secret settings (secrets \
                still come from the env file). Add an autumn.toml to control the deployed \
                configuration."
            .to_owned();
    }
    let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
    format!(
        "Uploading project config to the release dir: {}",
        names.join(", ")
    )
}

/// Perform a real deploy (issue #1607, Slices 1–3).
///
/// Runs the same preflight as `check` and aborts before touching the server if
/// anything fails (AC-6). It then probes the target ([`exec::probe_deploy_state`])
/// to choose between:
///
/// - **first deploy** ([`exec::first_deploy_ops`]) — install the reverse proxy on
///   the public port and stand the initial release up on a private loopback slot
///   behind it, or
/// - **zero-downtime redeploy** ([`exec::cutover_ops`]) — stand the candidate up
///   on the idle slot, run migrations before cutover, gate on `/ready`, then have
///   the proxy flip live traffic to it and drain the old release (AC-2/AC-3).
///
/// Either path carries a [`exec::candidate_teardown_ops`] plan so a failure
/// before go-live auto-rolls-back the candidate (AC-4): a redeploy leaves the old
/// release serving ([`exec::execute_redeploy`]); a first deploy has no previous
/// release and fails loudly after tearing the candidate down
/// ([`exec::execute_first_deploy`]).
/// The HTTPS port kamal-proxy binds by default (and cannot disable). A
/// public/HTTP port — or a blue/green slot port derived from it — equal to this
/// collides with the always-bound HTTPS listener.
const PROXY_HTTPS_PORT: u16 = 443;

/// Reject a deploy public/HTTP port that cannot work with the proxy topology.
///
/// One guard runs here, before any remote work, and it is **unconditional** (it
/// does not depend on whether TLS is enabled): kamal-proxy `run` binds its
/// `--https-port 443` listener by DEFAULT and that listener cannot be disabled,
/// and each blue/green slot binds a private loopback port derived from the
/// public port ([`exec::slot_app_port`]: blue = `public + 1`, green =
/// `public + 2`). If the proxy's HTTP port *or* either slot port lands on 443 it
/// collides with the always-bound HTTPS listener and the proxy can never start.
/// With blue/green at `+1`/`+2`, that rejects `public ∈ {441, 442, 443}`.
///
/// There is deliberately **no** TLS-gated port-80 requirement: per-app TLS is
/// provisioned by kamal-proxy on its always-bound 443 via `deploy --host --tls`
/// (TLS-ALPN-01, no port 80 needed), so enabling `[deploy.tls]` works on any
/// non-colliding public port — on both first deploy and redeploy — without a
/// proxy HTTP-port change.
fn validate_public_port(public_port: u16) -> Result<(), String> {
    // No port (proxy HTTP or a blue/green slot) may land on 443. Compute the slot
    // ports from the same offsets the deploy uses so the guard tracks the real
    // arithmetic instead of a hardcoded {441, 442, 443} set.
    let blue_port = exec::slot_app_port(public_port, exec::SLOT_BLUE);
    let green_port = exec::slot_app_port(public_port, exec::SLOT_GREEN);
    let collision = if public_port == PROXY_HTTPS_PORT {
        Some("the proxy's HTTP listener")
    } else if blue_port == PROXY_HTTPS_PORT {
        Some("the blue app slot")
    } else if green_port == PROXY_HTTPS_PORT {
        Some("the green app slot")
    } else {
        None
    };
    if let Some(which) = collision {
        return Err(format!(
            "deploy public/HTTP port {public_port} is invalid — {which} would land on \
             {PROXY_HTTPS_PORT}, which kamal-proxy reserves for its HTTPS listener \
             (blue/green app slots bind `public+1`/`public+2`); use 80 (the default) or \
             another port outside {}\u{2013}{PROXY_HTTPS_PORT}",
            PROXY_HTTPS_PORT - 2,
        ));
    }
    Ok(())
}

/// Refuse a concurrent `server.port` change on the **redeploy** path, BEFORE any op
/// runs — so the live release keeps serving (#2073).
///
/// The reboot-durability upgrade (#2070) makes every redeploy refresh the shared
/// kamal-proxy unit and, when that unit changed, restart `kamal-proxy run` and
/// re-register the still-live upstream. Both the restart's public bind and the
/// re-register's DERIVED loopback target are correct ONLY when the public port is
/// unchanged: if the operator changed `server.port` since the live release was
/// deployed, the restart would rebind a different public port and the re-register
/// would aim at a port nothing listens on, stranding `:80` mid-cutover. Rather than
/// try to sequence a live-safe port move here (Option C, #2073), refuse the deploy
/// at pre-flight with an actionable message.
///
/// The comparison is sourced from the INSTALLED proxy unit's `--http-port` (captured
/// by [`exec::probe_deploy_state`]), which is the ground truth of what the running
/// proxy actually binds:
///
///   - [`exec::InstalledProxyPort::Absent`] → no installed unit, i.e. a first-deploy
///     shape (the durability refresh writes it fresh). Nothing to conflict with →
///     allowed. (This branch only runs when `current` is a symlink, so this is the
///     rare shape where the proxy unit is missing but a release symlink exists.)
///   - [`exec::InstalledProxyPort::Port`] equal to the requested port → unchanged
///     redeploy (the common durability-upgrade path) → allowed.
///   - [`exec::InstalledProxyPort::Port`] DIFFERENT from the requested port → refuse,
///     naming old vs new port and the two-deploy operator sequence.
///   - [`exec::InstalledProxyPort::Unreadable`] → the unit is present but its
///     `--http-port` couldn't be read/parsed, so we can't prove the port is unchanged
///     → **fail closed** (refuse) rather than risk a mid-cutover bind failure.
fn refuse_concurrent_public_port_change(
    installed: &exec::InstalledProxyPort,
    new_public_port: u16,
) -> Result<(), String> {
    match installed {
        // No installed proxy unit — first-deploy shape; the durability refresh writes
        // it fresh at the requested port, so there is nothing to conflict with.
        exec::InstalledProxyPort::Absent => Ok(()),
        // Unchanged public port — the common redeploy / durability-upgrade path.
        exec::InstalledProxyPort::Port(current) if *current == new_public_port => Ok(()),
        exec::InstalledProxyPort::Port(current) => Err(format!(
            "Changing server.port on an existing deployment isn't supported here yet \
             (the installed kamal-proxy is on port {current}, config requests \
             {new_public_port}): first redeploy with server.port unchanged (adopts the \
             reboot-durability upgrade), then change the port in a separate deploy. \
             Tracked in #2073."
        )),
        // Fail closed: the unit is present but its --http-port is unreadable, so we
        // cannot prove the public port is unchanged — refuse rather than risk the
        // durability restart rebinding a different port and stranding `:80`.
        exec::InstalledProxyPort::Unreadable => Err(format!(
            "Cannot verify the installed kamal-proxy's HTTP port before a redeploy \
             (its systemd unit is present but its `--http-port` could not be read), so \
             a concurrent server.port change (config requests {new_public_port}) cannot \
             be ruled out and is refused to avoid stranding public traffic mid-cutover. \
             Re-provision the host (or repair the kamal-proxy unit) and retry. Live-safe \
             server.port changes are tracked in #2073."
        )),
    }
}

/// Pre-flight refuse for an UNPROVABLE `shared/proxy-options` marker on the redeploy
/// path (issue #2074), mirroring [`refuse_concurrent_public_port_change`]'s
/// `Unreadable` fail-closed arm.
///
/// The durability-refresh re-register (#2070/#2071) re-registers the still-live OLD
/// release; #2074 preserves that release's own TLS/host by reading them back from the
/// `shared/proxy-options` marker. When the marker is:
///
///   - [`exec::ProxyOptionsMarker::Options`] → the old options are known → preserve
///     them (no refuse);
///   - [`exec::ProxyOptionsMarker::Absent`] → a legacy host (or first deploy) that
///     never wrote the marker → **allowed**: proceed as legacy (re-register with the
///     new config and write the marker this deploy — see the redeploy arm). Refusing
///     here would block the FIRST redeploy of every pre-existing host, the deadlock
///     #2074 explicitly rejects;
///   - [`exec::ProxyOptionsMarker::Unreadable`] → the marker is present but its
///     `{tls}\t{host}` value couldn't be parsed, so the old options can't be proved →
///     **fail closed** (refuse): a concurrent `deploy.tls.host` change can't be safely
///     preserved across the one-time restart, and stamping the wrong host onto the
///     live release on a rollback is worse than refusing.
fn refuse_unprovable_proxy_options(marker: &exec::ProxyOptionsMarker) -> Result<(), String> {
    match marker {
        exec::ProxyOptionsMarker::Absent | exec::ProxyOptionsMarker::Options(_) => Ok(()),
        exec::ProxyOptionsMarker::Unreadable => Err(
            "Cannot verify the last-deployed proxy TLS/host options before a redeploy \
             (the shared/proxy-options marker is present but unreadable), so a concurrent \
             deploy.tls.host change cannot be preserved across the one-time reboot-durability \
             restart and is refused to avoid stranding the live release behind the wrong \
             TLS/host on a rollback. Redeploy with deploy.tls unchanged to repair the marker, \
             then change the host in a separate deploy. Tracked in #2074."
                .to_owned(),
        ),
    }
}

// ── Fleet-wide fail-closed refusals (issue #1621, §4.8) ──────────────────────
//
// All three are **pure predicates over the CONFIGURED host count**, evaluated in
// the rollout prologue before a single remote command runs, and all three are
// gated on `configured > 1` rather than on the number of hosts this run happens to
// touch: `--only` narrows a rollout, it does not change the topology that makes
// these unsafe. Each names the config key and the tracking issue, matching the
// house style of `reject_sqlite_sharding_topology` (`migrate.rs`).

/// Refuse a multi-host deploy backed by `SQLite` (issue #1621, §4.8).
///
/// `build_env_file` ships a byte-identical `AUTUMN_DATABASE__URL` to every host, so
/// a `sqlite://` URL means N independent database FILES — one per machine. There is
/// no advisory lock across them, "migrate exactly once" degenerates to "migrated on
/// exactly one host's database", and every host serves a different dataset behind
/// the same load balancer. That is data-destroying by construction, not a
/// performance footgun, so it fails closed.
fn refuse_sqlite_fleet(configured_hosts: usize, writable_db_sqlite: bool) -> Result<(), String> {
    if configured_hosts > 1 && writable_db_sqlite {
        return Err(
            "\u{2717} A multi-host deploy cannot run on SQLite. `[deploy] hosts` lists more \
             than one server and the configured `[database]` URL is a sqlite:// target, but \
             every host receives the SAME database URL — which means N independent database \
             FILES, one per machine: no shared schema, no advisory lock to serialize \
             migrations, and each host serving a different dataset behind your load \
             balancer. Point `[database] url` at a Postgres server every host can reach, or \
             deploy a single host with `[deploy] host`. Tracking: #1621."
                .to_owned(),
        );
    }
    Ok(())
}

/// Refuse a multi-host deploy with `[media.mediamtx] enabled` (issue #1621, §4.8).
///
/// `provision_media_host` runs after the cutover and has **no** teardown or
/// rollback path by design. Fanning it out would give N `MediaMTX` daemons on
/// identical ports with divergent recording sets, and nothing to undo them with
/// when the app rollout is compensated.
fn refuse_media_fleet(configured_hosts: usize, media_enabled: bool) -> Result<(), String> {
    if configured_hosts > 1 && media_enabled {
        return Err(
            "\u{2717} A multi-host deploy cannot provision MediaMTX. `[deploy] hosts` lists \
             more than one server and `[media.mediamtx] enabled = true`, but host media \
             provisioning has no teardown or rollback path: fanning it out would leave one \
             MediaMTX daemon per host on identical ports, with divergent recording sets and \
             nothing to undo them with when the app rollout is rolled back. Deploy media on \
             a single host with `[deploy] host`, or set `[media.mediamtx] enabled = false` \
             and provision the media host separately. Tracking: #1621."
                .to_owned(),
        );
    }
    Ok(())
}

/// Refuse a multi-host deploy with `[deploy.tls] enabled` (issue #1621, §4.8).
///
/// kamal-proxy is PER HOST — it binds that host's public port; it is not a fleet
/// load balancer. N hosts would each attempt ACME for the same `[deploy.tls] host`
/// from behind the operator's load balancer, only one can answer a given challenge,
/// and the rest burn Let's Encrypt failed-validation and duplicate-certificate rate
/// limits. Behind a load balancer, TLS terminates at the load balancer.
fn refuse_tls_fleet(configured_hosts: usize, tls_enabled: bool) -> Result<(), String> {
    if configured_hosts > 1 && tls_enabled {
        return Err(
            "\u{2717} A multi-host deploy cannot terminate TLS per host. `[deploy] hosts` \
             lists more than one server and `[deploy.tls] enabled = true`, but the \
             deploy-managed proxy runs on EVERY host: each one would request a certificate \
             for the same `[deploy.tls] host` from behind your load balancer, only one can \
             answer a given ACME challenge, and the rest burn Let's Encrypt \
             failed-validation and duplicate-certificate rate limits. Terminate TLS at the \
             load balancer that fronts the fleet and set `[deploy.tls] enabled = false`. \
             Tracking: #1621."
                .to_owned(),
        );
    }
    Ok(())
}

/// The loud line printed when a fleet deploys an app that auto-applies migrations
/// at boot (issue #1621, §4.8).
///
/// A **warning, not a refusal**: it is a real hazard but not a certainty, and
/// making it a refusal (or a preflight grader) would break existing single-host
/// users' scale-up and force a doctor-parity change for a rule the rollout itself
/// does not depend on.
const FLEET_AUTO_MIGRATE_WARNING: &str = "`[database] auto_migrate` is on and this deploy targets more than one host: every \
     host applies migrations at boot, so the hosts RACE each other during the rollout \
     and a checksum mismatch exits the process under `Restart=on-failure` — a crash \
     loop mid-rollout. Turn it off for fleets and let the rollout's single `migrate` \
     step own the schema (#1621).";

/// Whether the fleet must be warned about boot-time auto-migration (issue #1621,
/// §4.8) — pure.
///
/// `auto_migrate` supersedes the `auto_migrate_in_production` back-compat alias
/// (`auto_migrate` wins when set), mirroring the runtime's own resolution. The
/// alias is honoured without checking the profile: a deploy defaults to `prod`, and
/// under-warning about a crash loop is worse than over-warning.
fn fleet_auto_migrate_warning(
    configured_hosts: usize,
    auto_migrate: Option<bool>,
    auto_migrate_in_production: bool,
) -> Option<&'static str> {
    let enabled = auto_migrate.unwrap_or(auto_migrate_in_production);
    (configured_hosts > 1 && enabled).then_some(FLEET_AUTO_MIGRATE_WARNING)
}

/// The loud line printed when a fleet deploys an app whose background-work
/// backends are per-process (issue #1621, AC-8).
///
/// A **warning, not a refusal**, for the same reason as
/// [`FLEET_AUTO_MIGRATE_WARNING`] and more so: `[jobs] backend = "local"` and
/// `[scheduler] backend = "in_process"` are the DEFAULTS every un-configured app
/// runs, and they are exactly right on one host. Refusing would break every
/// existing single-host user's scale-up — the one flow AC-8's guide exists to
/// make easy — for a hazard the rollout itself does not depend on.
///
/// Composed rather than constant because it names only the key(s) that actually
/// apply; see [`fleet_shared_backend_warning`].
const FLEET_SHARED_BACKEND_WARNING_TAIL: &str = "each host then runs its OWN copy, so work enqueued on one host is only ever run by \
     that host, whatever is still queued dies when the next rollout drains that host's \
     old slot, `unique`/`concurrency` limits stop being enforced fleet-wide, and every \
     scheduled task fires once PER HOST. Move them to a shared backend (`postgres` or \
     `redis`) before you rely on background work across the fleet \u{2014} see \
     docs/guide/fleet-deploys.md (#1621).";

/// Whether the fleet must be warned that its background-work backends are
/// per-process (issue #1621, AC-8) — pure.
///
/// `jobs_backend` is compared case-insensitively after trimming, and anything that
/// is not the in-process `local` default is treated as durable: a backend this CLI
/// does not know about is the operator having configured something deliberately,
/// and over-warning them every deploy is worse than staying quiet.
///
/// Returns `None` for a single-host config — the in-process defaults are correct
/// there, and this must be silent on the byte-identical N==1 path (AC-1).
fn fleet_shared_backend_warning(
    configured_hosts: usize,
    jobs_backend: &str,
    scheduler_in_process: bool,
) -> Option<String> {
    if configured_hosts <= 1 {
        return None;
    }
    let jobs_local = jobs_backend.trim().eq_ignore_ascii_case("local");
    let keys = match (jobs_local, scheduler_in_process) {
        (true, true) => {
            "`[jobs] backend = \"local\"` (an in-process queue) and `[scheduler] \
                         backend = \"in_process\"` (a per-process timer) are in effect"
        }
        (true, false) => "`[jobs] backend = \"local\"` (an in-process queue) is in effect",
        (false, true) => {
            "`[scheduler] backend = \"in_process\"` (a per-process timer) is in effect"
        }
        (false, false) => return None,
    };
    Some(format!(
        "{keys} and this deploy targets more than one host \u{2014} they are PER HOST: \
         {FLEET_SHARED_BACKEND_WARNING_TAIL}"
    ))
}

/// Whether the resolved writable database URL names a `SQLite` backend (issue
/// #1621, §4.8).
///
/// Resolves the same URL the deploy would SHIP to every host
/// ([`resolve_writable_db_url`]) and reduces it to one bool **here**, so the URL —
/// which carries credentials — never enters the rollout driver's inputs, its
/// errors, or its output.
fn writable_db_is_sqlite(db: &autumn_web::config::DatabaseConfig) -> bool {
    resolve_writable_db_url(db).is_some_and(|url| {
        matches!(
            autumn_web::config::DatabaseBackend::detect(url),
            Some(autumn_web::config::DatabaseBackend::Sqlite)
        )
    })
}

/// Perform a real deploy of the whole configured fleet (issue #1607, Slices 1–3;
/// issue #1621).
///
/// Runs the same preflight as `check` — for EVERY configured host — and aborts
/// before touching any server if anything fails (AC-6/AC-7). It then mints ONE
/// release id for the whole run, probes every host read-only, plans the rollout,
/// and replaces the hosts ONE AT A TIME in `[deploy] hosts` order.
///
/// A single-host config is a one-host fleet: same ops, same output, same errors as
/// before #1621. The prologue here is the part that is fleet-wide by nature
/// (preflight, release id, release binary, env file, manifests, port validation);
/// everything host-shaped lives in [`run_up_with`], which takes those results as
/// data so the rollout loop is unit-testable against per-host fakes with no host,
/// no filesystem and no clock.
fn run_up(
    config: &AutumnConfig,
    resolved: &ResolvedDeployConfig,
    hosts: &[String],
    configured_host_count: usize,
    media_cfg: &media::MediaMtxHostConfig,
    ffmpeg_bin: &str,
    options: &DeployOptions,
) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy up\n");

    // Attach the SQLite data-file contract (#1909) BEFORE the fleet is built, so
    // every host's config carries it and every host links the same path. A
    // Postgres app resolves `None` and nothing below changes.
    let resolved = &resolved
        .clone()
        .with_sqlite_data_file(sqlite_relative_data_file(config, resolved));

    // The rollout targets, in declaration order. A single-host config resolves to
    // a one-host fleet whose only element IS today's `resolved`, so everything
    // below is the pre-#1621 sequence at N = 1.
    let fleet = ResolvedFleet::from_targets(resolved, hosts);

    // Fail fast: run the full preflight and abort BEFORE any remote call. Every
    // host is graded here, so an unreachable host in position 3 is reported before
    // host 1 is touched.
    let checks = collect_fleet_preflight(config, &fleet, configured_host_count);
    let failed = report_preflight(&checks);
    if failed > 0 {
        return Err(DeployError::PreflightFailed(failed));
    }

    let binary = resolve_release_binary(resolved)?;
    let env_file = build_env_file(config, resolved);
    // Locate the project config manifest(s) to upload so the deployed app loads
    // the intended config rather than silent built-in defaults (#1952), and print
    // a loud line either confirming the upload or warning when there is no
    // autumn.toml to ship. The same bytes go to every host.
    let manifests = locate_manifest_uploads(resolved);
    eprintln!("{}", manifest_preflight_notice(&manifests));
    // Exactly ONE release id per fleet run (#1621): every host's `current` symlink
    // then resolves to the same release, so drift reporting is meaningful and a
    // rollback has a single target. Minting per host would give un-comparable
    // version identities and permanent reported drift.
    let release_id = default_release_id();
    let public_port = config.server.port;
    // kamal-proxy `run` binds 443 for its HTTPS listener BY DEFAULT (regardless of
    // any app's TLS flag, and it cannot be disabled), so a public/HTTP port whose
    // proxy-HTTP or blue/green slot port lands on 443 would collide and the proxy
    // would fail to start. Reject that up front with an actionable message instead
    // of a cryptic runtime bind failure on the host. TLS imposes no port-80
    // requirement — it is provisioned per app on the always-bound 443.
    validate_public_port(public_port).map_err(DeployError::Config)?;
    let proxy = proxy::KamalProxyController::new(resolved.readiness_timeout_secs)
        .with_tls_host(resolved.tls_host.clone());

    run_up_with(
        &FleetUpInput {
            fleet: &fleet,
            proxy: &proxy,
            checks: &checks,
            env_file: &env_file,
            binary: &binary,
            manifests: &manifests,
            release_id: &release_id,
            public_port,
            media_cfg,
            ffmpeg_bin,
            configured_host_count,
            // Every fact is reduced to a bool HERE so the database URL (credentials
            // and all) never reaches the driver, its errors, or its output.
            db: FleetDatabaseFacts {
                sqlite: writable_db_is_sqlite(&config.database),
                auto_migrate: config.database.auto_migrate,
                auto_migrate_in_production: config.database.auto_migrate_in_production,
            },
            jobs_backend: &config.jobs.backend,
            scheduler_in_process: matches!(
                config.scheduler.backend,
                autumn_web::config::SchedulerBackend::InProcess
            ),
            // AC-3's default: a halted rollout compensates the hosts that already
            // cut over rather than leaving the fleet on two versions. `--no-rollback`
            // is halt-and-freeze: stop, report, change nothing else.
            auto_rollback: !options.no_rollback,
        },
        // Host presence is guaranteed by the passing ssh_reachability grader above,
        // so this `None` arm is unreachable in practice; it keeps the pre-#1621
        // error verbatim rather than panicking on an invariant proved elsewhere.
        |cfg| {
            exec::SshTarget::from_resolved(cfg)
                .map(exec::SshExecutor::new)
                .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))
        },
    )
}

/// What this project's `[database]` config means for a fleet rollout (issue #1621,
/// §4.8).
///
/// Grouped rather than passed as loose flags because they answer ONE question
/// together — "what happens to the schema during this rollout?" — and because of
/// what is deliberately absent: the database URL itself. It carries credentials, so
/// every fact here is reduced to a bool at the point the URL is resolved, and the
/// rollout driver, its errors and its output can never quote it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FleetDatabaseFacts {
    /// Whether that URL names a `SQLite` backend (the §4.8 refusal).
    sqlite: bool,
    /// `[database] auto_migrate`, verbatim (the §4.8 warning).
    auto_migrate: Option<bool>,
    /// `[database] auto_migrate_in_production` — the back-compat alias
    /// `auto_migrate` supersedes.
    auto_migrate_in_production: bool,
}

/// Everything the fleet rollout loop needs, already resolved by [`run_up`].
///
/// Every field is either fleet-wide by construction (`release_id`, `env_file`,
/// `binary`, `manifests`, `public_port` — the same bytes reach every host) or the
/// fleet itself. Passing them as data rather than re-deriving them inside the loop
/// is what makes the loop testable: nothing here reads the clock, the filesystem,
/// or a socket.
struct FleetUpInput<'a, P: ProxyController> {
    /// The rollout targets, in order. Never empty.
    fleet: &'a ResolvedFleet,
    /// The proxy controller. kamal-proxy is PER HOST (it binds that host's public
    /// port); it is not a fleet load balancer, so one controller value describes
    /// every host's local proxy.
    proxy: &'a P,
    /// The preflight results the per-host executor gates on.
    checks: &'a [PreflightCheck],
    /// The secret env-file body, byte-identical on every host.
    env_file: &'a exec::Secret,
    /// Local path to the release binary uploaded to every host.
    binary: &'a Path,
    /// Config manifests uploaded into every host's release dir.
    manifests: &'a [exec::ManifestUpload],
    /// The ONE release id for this run.
    release_id: &'a str,
    /// The app's public port, fronted by each host's proxy.
    public_port: u16,
    /// `MediaMTX` host provisioning config.
    media_cfg: &'a media::MediaMtxHostConfig,
    /// Resolved `FFmpeg` binary path for the media preflight.
    ffmpeg_bin: &'a str,
    /// How many hosts the CONFIG declares, before `--only` narrowed `fleet`
    /// (issue #1621, §3.2/§4.8).
    ///
    /// Two things turn on this rather than on `fleet.hosts.len()`:
    ///
    /// - The §4.8 refusals describe a TOPOLOGY (one `SQLite` file per machine, N
    ///   `MediaMTX` daemons, N ACME clients racing for one certificate). Deploying
    ///   a subset of that topology does not make it safe, so `--only` must not be a
    ///   way to sneak past them.
    /// - The byte-identical single-host path (AC-1) is for a config that declares
    ///   ONE host — not for a three-host fleet narrowed to one. A narrowed rollout
    ///   keeps the fleet framing, the state table and the compensation semantics,
    ///   because the other two hosts still exist and are still on another release.
    configured_host_count: usize,
    /// What this project's `[database]` config means for the rollout — resolved
    /// once in the prologue, never re-read here.
    db: FleetDatabaseFacts,
    /// `[jobs] backend`, verbatim — for the per-host background-work warning
    /// (AC-8). Not a secret and not host-derived; see
    /// [`fleet_shared_backend_warning`].
    jobs_backend: &'a str,
    /// Whether `[scheduler] backend` is the per-process timer (the default), for
    /// the same warning.
    scheduler_in_process: bool,
    /// Whether a halted rollout automatically compensates the hosts that already
    /// cut over (issue #1621, §4.7). `true` in production; `false` is halt-and-freeze
    /// — the rollout stops and reports, leaving every host exactly as it is for
    /// inspection. The operator-facing `--no-rollback` flag that sets it lands with
    /// the fleet CLI surface; today it is an internal seam so the freeze path is
    /// tested, not dead.
    auto_rollback: bool,
}

/// One host's read-only probe result: everything the rollout needs to know about
/// that host, decided BEFORE any host is touched.
struct HostProbeState {
    /// First deploy vs redeploy.
    mode: fleet::HostMode,
    /// That host's own blue/green slot layout.
    slots: exec::SlotPlan,
    /// Whether that host's live-slot marker drifted and must be repaired as the
    /// first op of its sequence.
    repair: bool,
    /// Proxy options the durability re-register must preserve on that host (#2074).
    reregister_options: exec::ProxyServiceOptions,
    /// Operator-facing description of the path this host takes.
    banner: String,
    /// Whether this host has no usable reverse-proxy binary and this deploy must
    /// install one as the FIRST op of that host's turn (issue #1607, AC-1).
    needs_proxy_install: bool,
}

/// Whether this host's deploy may prepare the target by installing a missing proxy
/// binary — the `[deploy] install_proxy` knob, projected onto the executor-level
/// type (issue #1607, AC-1).
const fn provisioning(cfg: &ResolvedDeployConfig) -> exec::ProxyProvisioning {
    if cfg.install_proxy {
        exec::ProxyProvisioning::Auto
    } else {
        exec::ProxyProvisioning::Disabled
    }
}

/// Probe ONE host read-only and apply every fail-closed refusal to it.
///
/// This runs for EVERY host before the first host is mutated (issue #1621, §4.3),
/// which is what makes a fleet rollout all-or-nothing at the refusal level: a
/// drifted host in position 3 refuses the rollout while hosts 1 and 2 are still
/// serving their old releases, instead of being discovered after two cutovers.
///
/// The refusals themselves are the UNCHANGED single-host guards; the only fleet
/// addition is that their messages name the host when there is more than one (a
/// single-host deploy keeps the exact pre-#1621 text).
fn probe_host_for_up<E, P>(
    cfg: &ResolvedDeployConfig,
    input: &FleetUpInput<'_, P>,
    executor: &E,
) -> Result<HostProbeState, DeployError>
where
    E: exec::DeployExecutor,
    P: ProxyController,
{
    let host = cfg.host.as_deref().unwrap_or_default();
    // The SAME question the driver asks (`is_single_host_deploy`), not
    // `fleet.is_single()`: a `--only`-narrowed repair on a multi-host fleet must
    // keep naming the host it is refusing to touch.
    let single = is_single_host_deploy(input.fleet, input.configured_host_count);
    // Host attribution is carried in the MESSAGE, never in an op label: labels are
    // `&'static str` and are load-bearing for the auto-rollback boundary lookup and
    // for every exact-vector test (#1621).
    let scoped = |message: String| {
        if single {
            message
        } else {
            format!("host {host}: {message}")
        }
    };

    // Decide whether the host has a WORKING reverse proxy (#1607 AC-1 host prep,
    // #2053 CLI-drift guard). READ-ONLY: a compatible binary passes silently, a
    // drifted one aborts here and is never replaced, and a host with none is merely
    // MARKED for preparation — the install itself is the first op of that host's own
    // turn, so this phase still mutates nothing anywhere in the fleet.
    let needs_proxy_install = matches!(
        exec::assess_proxy_readiness(input.proxy, executor, provisioning(cfg))
            .map_err(|e| DeployError::Exec(scoped(e.to_string())))?,
        exec::ProxyReadiness::NeedsInstall
    );

    // Probe the target to choose first-deploy vs zero-downtime redeploy. The same
    // round-trip also captures `kamal-proxy list` so a drifted live-slot marker
    // can be reconciled against the live proxy before slot-selection (#1938).
    let probe = exec::probe_deploy_state(cfg, executor)
        .map_err(|e| DeployError::Exec(scoped(e.to_string())))?;

    // Refuse a release-dir collision (#1621, §4.9). The release id has one-second
    // granularity, so a fast retry re-uses it; re-uploading into the dir
    // `shared/previous-release` still points at would make the "previous release"
    // hold the NEW binary and roll FORWARD on the next rollback. Nothing detects
    // that afterwards, so it is refused before anything is written.
    match exec::probe_release_dir(cfg, input.release_id, executor)
        .map_err(|e| DeployError::Exec(scoped(e.to_string())))?
    {
        exec::ReleaseDirState::Absent => {}
        exec::ReleaseDirState::Present => {
            return Err(DeployError::Config(scoped(format!(
                "release directory {}/{} already exists — a previous run of this release \
                 id already wrote into it, and re-uploading would put the NEW binary in \
                 the directory `shared/previous-release` points at (so a rollback would \
                 roll FORWARD). Wait a second and re-run `autumn deploy up`, or remove \
                 that directory if you are certain it is stale. Tracked in #1621.",
                cfg.releases_dir(),
                input.release_id,
            ))));
        }
        exec::ReleaseDirState::Unreadable => {
            return Err(DeployError::Config(scoped(format!(
                "cannot verify whether release directory {}/{} already exists (the probe \
                 returned neither sentinel), so a release-id collision cannot be ruled \
                 out and the deploy is refused rather than risk overwriting the release \
                 `shared/previous-release` points at. Tracked in #1621.",
                cfg.releases_dir(),
                input.release_id,
            ))));
        }
    }

    match probe.mode {
        exec::DeployMode::First => Ok(HostProbeState {
            mode: fleet::HostMode::from_deploy_mode(&probe.mode),
            slots: exec::SlotPlan::first(input.public_port),
            repair: false,
            // A first deploy registers the NEW config's options; there is no old
            // release whose options could need preserving.
            reregister_options: input.proxy.proxy_service_options(),
            banner: "first deploy".to_owned(),
            needs_proxy_install,
        }),
        exec::DeployMode::Redeploy { live_slot } => {
            // Pre-flight refuse (#2073): the reboot-durability restart-refresh (#2070)
            // re-execs `kamal-proxy run` on the public port and re-registers the
            // still-live upstream at its derived loopback port — both correct only while
            // the public port is unchanged. If the operator changed `server.port` since
            // the live release was deployed, that restart would rebind a different public
            // port and the re-register would aim at a dead loopback port, stranding `:80`
            // mid-cutover with no auto-recovery. Refuse here, before any op runs, sourced
            // from the installed proxy unit's `--http-port` captured in the deploy-start
            // probe, so the live release keeps serving. A live-safe port change is future
            // work. No installed unit means a first deploy and is allowed; an unreadable
            // unit fails closed.
            refuse_concurrent_public_port_change(&probe.installed_proxy_port, input.public_port)
                .map_err(|message| DeployError::Config(scoped(message)))?;
            // Pre-flight refuse (#2074): if the `shared/proxy-options` marker is present
            // but unreadable, we cannot prove the OLD release's TLS/host, so a concurrent
            // `deploy.tls.host` change can't be safely preserved across the durability
            // restart — fail closed BEFORE any op runs, so the live release keeps serving.
            refuse_unprovable_proxy_options(&probe.last_proxy_options)
                .map_err(|message| DeployError::Config(scoped(message)))?;
            // Choose the options the durability-refresh re-register carries for the
            // still-live OLD release (#2074). PRESERVE the marker's recorded options
            // when known; on an ABSENT marker fall back to the NEW config (proceed as
            // legacy — the durability-upgrade deploy's kamal-proxy table is empty, so
            // there is nothing to preserve, and refusing would deadlock every
            // pre-#2074 host's first redeploy). The marker is (re)written by
            // `cutover_ops` this deploy, so the next redeploy is fully protected. The
            // Unreadable case is already refused above, so it never reaches here.
            let reregister_options = match &probe.last_proxy_options {
                exec::ProxyOptionsMarker::Options(old) => old.clone(),
                _ => input.proxy.proxy_service_options(),
            };
            // Reconcile the (possibly stale) live-slot marker against the live
            // proxy before choosing the candidate slot. On an UNAMBIGUOUS
            // proxy-vs-marker disagreement the proxy is authoritative (so the
            // candidate takes the genuinely-idle slot and never restarts the live
            // one); on any absent/unclear proxy signal this is exactly the
            // marker-based behavior as before (#1938, fail-safe).
            let reconcile = exec::reconcile_live_slot(
                live_slot,
                &probe.proxy_list,
                &cfg.service_name,
                input.public_port,
            );
            if let Some(warn) = &reconcile.warn {
                // Loud + observable: drift is surfaced, not silently papered over.
                // (autumn-cli installs no tracing subscriber, so the deploy module
                // reports operator-facing state via eprintln! throughout.)
                eprintln!("\u{26A0}\u{FE0F}  {}", scoped(warn.clone()));
            }
            let slots = exec::SlotPlan::redeploy(input.public_port, reconcile.live_slot);
            let banner = format!(
                "zero-downtime redeploy ({} \u{2192} {})",
                slots.live_slot, slots.candidate_slot
            );
            Ok(HostProbeState {
                mode: fleet::HostMode::from_deploy_mode(&probe.mode),
                slots,
                repair: reconcile.repair,
                reregister_options,
                banner,
                needs_proxy_install,
            })
        }
    }
}

/// Drive the rollout: probe every host, plan, then replace the hosts ONE AT A TIME
/// (issue #1621, AC-2/AC-3/AC-4).
///
/// The executor factory is injected (generic, never `dyn`, and with no `Send`/`Sync`
/// bound) so a test can drive the whole loop — probe, plan, per-host execute — with
/// one scripted fake per host. Production hands it an `SshExecutor` per target.
///
/// Structure that is load-bearing rather than stylistic:
///
/// - **Every host is probed before ANY host is mutated.** All the fail-closed
///   refusals live in that phase, so a drifted host in position 3 refuses the whole
///   rollout with hosts 1 and 2 untouched.
/// - **Each host's ops are executed by their OWN `execute_*` call.** Two hosts' op
///   vectors are never concatenated: `execute_with_teardown` resolves the
///   auto-rollback boundary with the FIRST matching label, so a flat vector would
///   classify every later host's pre-flip failure as post-boundary and silently
///   disable teardown.
/// - **A failure HALTS the rollout, then compensates** (issue #1621, §4.6/§4.7).
///   Hosts after the failing one are never touched, and the hosts that already cut
///   over are rolled back in reverse order so the fleet converges on ONE version —
///   binaries only; a migration that already ran is never undone. The one exception
///   is a post-boundary HOUSEKEEPING failure (`record-proxy-options`, `drain-old`,
///   `prune`): the host is live and healthy on the new release, so the rollout
///   warns, marks it degraded, and CONTINUES. A rollout whose only failures are
///   housekeeping therefore succeeds (`Ok`) with degraded hosts named in the state
///   table — rolling a whole fleet back because an `rm -rf` of old release dirs
///   failed would be a self-inflicted outage.
///
/// **N = 1 is exempt from all of it.** A single-host config takes the pre-#1621
/// path verbatim: the raw executor error, no fleet vocabulary, no state table, no
/// compensation (there is no other host to converge with, and its own boundary
/// teardown already ran). That is AC-1, and it is why the `single` branches below
/// are not cosmetic.
#[allow(clippy::too_many_lines)]
fn run_up_with<E, P, F>(input: &FleetUpInput<'_, P>, make_executor: F) -> Result<(), DeployError>
where
    E: exec::DeployExecutor,
    P: ProxyController,
    F: Fn(&ResolvedDeployConfig) -> Result<E, DeployError>,
{
    let fleet = input.fleet;
    // The pre-#1621 path is for a config that declares ONE host — see
    // [`FleetUpInput::configured_host_count`] for why a `--only`-narrowed fleet is
    // deliberately NOT single.
    let single = is_single_host_deploy(fleet, input.configured_host_count);
    let total = fleet.hosts.len();

    // ── FLEET-WIDE FAIL-CLOSED REFUSALS (§4.8) ───────────────────────────────
    // First thing in the driver, before an executor is even built: these describe
    // topologies that cannot be deployed safely at all, so the honest answer is to
    // refuse with zero remote commands rather than discover it after a cutover.
    refuse_sqlite_fleet(input.configured_host_count, input.db.sqlite)
        .map_err(DeployError::Config)?;
    refuse_media_fleet(input.configured_host_count, input.media_cfg.enabled)
        .map_err(DeployError::Config)?;
    // `first()` rather than `[0]`: the shared shape is identical across the fleet,
    // and an empty `hosts` — which `collect_fleet_preflight` now refuses to let a
    // run reach at all (#2274) — must still surface as the missing-host config
    // error every other hostless path returns, never as an index panic.
    let shared = fleet
        .hosts
        .first()
        .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))?;
    refuse_tls_fleet(input.configured_host_count, shared.tls_enabled)
        .map_err(DeployError::Config)?;
    // A warning, not a refusal: hosts racing to auto-migrate at boot is a real
    // hazard but not a certainty, and refusing would break an existing single-host
    // user's scale-up.
    if let Some(warning) = fleet_auto_migrate_warning(
        input.configured_host_count,
        input.db.auto_migrate,
        input.db.auto_migrate_in_production,
    ) {
        eprintln!("\u{26A0}\u{FE0F}  {warning}\n");
    }
    // Same rule, same reason (AC-8): the in-process job queue and scheduler are
    // the DEFAULTS every un-configured app runs and are correct on one host, so a
    // fleet is warned rather than refused.
    if let Some(warning) = fleet_shared_backend_warning(
        input.configured_host_count,
        input.jobs_backend,
        input.scheduler_in_process,
    ) {
        eprintln!("\u{26A0}\u{FE0F}  {warning}\n");
    }

    // ONE executor per host, built up front and reused for that host's read-only
    // probe AND its execution.
    let executors = fleet
        .hosts
        .iter()
        .map(&make_executor)
        .collect::<Result<Vec<E>, DeployError>>()?;

    // MediaMTX host preflight (#1974 Slice 7): non-mutating doctor checks run before the
    // app deploy, so a bad media host fails fast with nothing written. The mutating
    // provisioning — write config and unit, restart — is deferred until after the app
    // cutover succeeds, so a rolled-back app deploy never touches the host MediaMTX unit.
    // No-op unless enabled (see fn).
    //
    // Single-host by construction rather than by branch: `provision_media_host` has no
    // teardown or rollback path, so a media-enabled fleet is refused outright above
    // (§4.8). Reaching here with media enabled proves the config declares exactly one
    // host, and `executors[0]` is it. With media disabled this and the provisioning call
    // below are both no-ops, so no host-count branch is needed.
    check_media_host_preflight(input.media_cfg, input.ffmpeg_bin, &executors[0])?;

    // ── ALL-HOSTS PROBE (read-only, serial, rollout order) ───────────────────
    // Nothing anywhere in the fleet is mutated by this phase.
    let probes = fleet
        .hosts
        .iter()
        .zip(&executors)
        .map(|(cfg, executor)| probe_host_for_up(cfg, input, executor))
        .collect::<Result<Vec<HostProbeState>, DeployError>>()?;

    let modes: Vec<fleet::HostMode> = probes.iter().map(|probe| probe.mode).collect();
    let plan = fleet::plan_fleet(fleet, &modes).map_err(|e| DeployError::Config(e.to_string()))?;

    if !single {
        for line in fleet::fleet_rollout_lines(&plan, input.release_id) {
            eprintln!("{line}");
        }
        eprintln!();
    }

    // ── EXECUTION (serial, one host at a time, rollout order) ────────────────
    let mut outcomes = vec![fleet::HostOutcome::Untouched; total];
    // Housekeeping debris is recorded OUTSIDE the outcome slot: a degraded host that
    // is later compensated becomes `CompensatedRollback`, and the failed
    // `record-proxy-options` that will fail its NEXT deploy closed must survive that
    // overwrite.
    let mut degraded: Vec<(String, &'static str)> = Vec::new();
    // Set when the rollout stops. Compensation runs after the loop, not inside it,
    // so the compensation set is computed from the FINAL per-host outcomes.
    let mut halt: Option<(String, &'static str)> = None;
    for (index, host_plan) in plan.hosts.iter().enumerate() {
        let cfg = &fleet.hosts[index];
        let state = &probes[index];
        let executor = &executors[index];

        let release_dir = format!("{}/{}", cfg.releases_dir(), input.release_id);
        let unit = render_app_unit(
            cfg,
            &release_dir,
            state.slots.candidate_port,
            state.slots.candidate_slot,
        );
        let mut ops = fleet::host_ops(
            host_plan,
            &fleet::HostOpsInput {
                cfg,
                proxy: input.proxy,
                unit: &unit,
                env_file: input.env_file,
                binary_local: input.binary,
                manifests: input.manifests,
                release_id: input.release_id,
                slots: &state.slots,
                reregister_options: &state.reregister_options,
            },
        );
        // Host preparation (#1607, AC-1): this host has no usable reverse-proxy binary,
        // so install one as the first op of its turn, ahead of the proxy unit that
        // supervises it and ahead of everything else. Running it here rather than during
        // the read-only probe phase is what keeps "no host is touched until every host is
        // graded" true, and it makes a failed install an ordinary pre-cutover failure:
        // the rollout halts, this host is compensated like any other, and no host that
        // already cut over is left behind.
        // Repair the drifted marker as an early op — before the cutover's
        // record-previous-release reads it — so the on-disk marker matches the proxy
        // truth even if the rest of the deploy is interrupted.
        if state.repair {
            ops.insert(
                0,
                exec::live_slot_marker_repair_op(cfg, state.slots.live_slot, input.public_port),
            );
        }
        // Host preparation goes in AFTER the repair insert so it ends up ahead of
        // it: a host needing both is one with no proxy binary AND a drifted marker,
        // and installing the proxy is what makes every later op — the marker repair
        // included — worth attempting at all.
        if state.needs_proxy_install {
            eprintln!(
                "{}host preparation: {}",
                if single {
                    String::new()
                } else {
                    format!("host {}: ", host_plan.host)
                },
                input.proxy.binary_install_note(),
            );
            let install = input.proxy.binary_install_ops().unwrap_or_default();
            ops.splice(0..0, install);
        }
        // First-deploy teardown must also unlink `current` and clear the live-slot
        // marker that first_deploy_ops creates — otherwise a failed first deploy
        // leaves them behind and the next `deploy up` wrongly takes the redeploy
        // path with nothing serving.
        let teardown = match host_plan.mode {
            fleet::HostMode::First => {
                exec::first_deploy_teardown_ops(cfg, input.release_id, &state.slots)
            }
            fleet::HostMode::Redeploy => {
                exec::candidate_teardown_ops(cfg, input.release_id, &state.slots)
            }
        };

        if single {
            eprintln!(
                "Deploying release {} to {} ({})\u{2026}\n",
                input.release_id, host_plan.host, state.banner,
            );
        } else {
            eprintln!(
                "[{}/{total} {}] deploying release {} ({})\u{2026}",
                index + 1,
                host_plan.host,
                input.release_id,
                state.banner,
            );
        }

        let result = match host_plan.mode {
            fleet::HostMode::First => {
                exec::execute_first_deploy(input.checks, &ops, &teardown, executor)
            }
            fleet::HostMode::Redeploy => {
                exec::execute_redeploy(input.checks, &ops, &teardown, executor)
            }
        };

        match result {
            Ok(()) => {
                outcomes[index] = fleet::HostOutcome::Serving;
                if !single {
                    if host_plan.migrate == exec::MigrateStep::Run {
                        eprintln!("\u{26A0}\u{FE0F}  {}", fleet::FLEET_SCHEMA_MOVED_NOTE);
                    }
                    eprintln!(
                        "\u{2705} [{}/{total} {}] serving {}\n",
                        index + 1,
                        host_plan.host,
                        input.release_id,
                    );
                }
            }
            Err(err) => {
                // A one-host fleet keeps today's error verbatim: the per-host
                // executor already told the whole story, and inventing a fleet
                // vocabulary for one host would change pre-#1621 output. This
                // returns BEFORE any classification, degrade-and-continue or
                // compensation, so N = 1 is byte-identical on every failure shape,
                // post-boundary ones included.
                if single {
                    return Err(DeployError::Exec(err.to_string()));
                }
                let failed_step = fleet::failed_step_label(&err);
                outcomes[index] = fleet::classify_host_outcome(&err);
                // Post-boundary housekeeping: the proxy is already serving the new
                // release on this host and only bookkeeping failed. Warn, record the
                // debris, and keep rolling — the alternative is an outage caused by
                // a failed `rm -rf` (#1621, §4.6).
                if let fleet::HostOutcome::Degraded { label } = outcomes[index] {
                    degraded.push((host_plan.host.clone(), label));
                    eprintln!(
                        "\u{26A0}\u{FE0F}  [{}/{total} {}] serving {} \u{2014} but `{label}` \
                         failed AFTER the cutover. Traffic is healthy, so the rollout \
                         continues; repair this host (a redeploy does) before the next \
                         deploy, which will refuse it.\n",
                        index + 1,
                        host_plan.host,
                        input.release_id,
                    );
                    continue;
                }
                eprintln!(
                    "\n\u{274C} rollout halted at {} (`{failed_step}`) \u{2014} the remaining \
                     hosts were not touched.\n",
                    host_plan.host,
                );
                halt = Some((host_plan.host.clone(), failed_step));
                break;
            }
        }
    }

    if let Some((failed_host, failed_step)) = halt {
        if input.auto_rollback {
            compensate_fleet(input, &plan, &probes, &executors, &mut outcomes);
        }
        // Print the per-host state on the way out — AFTER compensation, so the table
        // describes where the fleet actually IS, and even when the compensating
        // rollback itself failed. That is precisely the case where the operator has
        // no other source of truth.
        for line in fleet::fleet_summary_lines(&plan, &outcomes, input.release_id) {
            eprintln!("{line}");
        }
        return Err(fleet_halted(
            &plan,
            &outcomes,
            &degraded,
            failed_host,
            failed_step,
        ));
    }

    // App deploy is committed on every host here: the cutovers succeeded and there
    // is no longer a rollback path. Only NOW provision the host MediaMTX unit
    // (write config/unit + restart) — deferring the mutation past this point means
    // a failed/rolled-back app deploy above never wrote or restarted MediaMTX
    // (#1974 Slice 7). No-op unless enabled (see fn), and a media-enabled fleet is
    // refused in the prologue (§4.8), so `executors[0]` is the only host whenever
    // this does anything at all.
    {
        let executor = &executors[0];
        provision_media_host(input.media_cfg, executor)?;
    }

    if single {
        eprintln!("\n\u{2705} Deploy complete. Roll back with `autumn deploy rollback`.");
    } else {
        for line in fleet::fleet_summary_lines(&plan, &outcomes, input.release_id) {
            eprintln!("{line}");
        }
        // Every host serves the new release either way; a degraded host is live and
        // healthy, so this is a success — but claiming an unqualified "complete"
        // when a host's `record-proxy-options` never landed would hide the thing
        // that fails its NEXT deploy closed. And on a `--only`-narrowed run `total`
        // is the SELECTED count, so "all N hosts" would read as a full-fleet
        // success while the skipped hosts sit on their old release.
        eprintln!(
            "{}",
            fleet::fleet_completion_line(
                input.release_id,
                total,
                input.configured_host_count,
                degraded.len(),
            )
        );
    }
    Ok(())
}

/// Compensate a halted rollout: undo every host that is already on the new release,
/// in strict REVERSE rollout order (issue #1621, §4.7, AC-3).
///
/// Three rules are load-bearing:
///
/// 1. **Best-effort CONTINUE.** A failed rollback on host j does not stop host j-1.
///    Stopping would leave MORE hosts on the new release — a bigger mixed fleet,
///    which is the harm AC-3 names.
/// 2. **Never swallowed.** Each host's result is recorded into its outcome slot and
///    reported. This deliberately does NOT reuse [`exec::run_ops`]'s teardown
///    sibling `run_teardown`, whose `let _ = …` is right for cleanup that must not
///    mask a real failure and actively dangerous at fleet scale.
/// 3. **Binaries only.** [`exec::rollback_ops`] and
///    [`exec::first_deploy_teardown_ops`] contain no `migrate` op by construction,
///    so a migration that already ran stays applied. The summary says so out loud.
///
/// Nothing here is new machinery: a redeploy host is compensated with exactly the
/// on-demand `autumn deploy rollback` primitives, and a completed FIRST deploy with
/// the first-deploy teardown (it has no previous release to return to).
fn compensate_fleet<E, P>(
    input: &FleetUpInput<'_, P>,
    plan: &fleet::FleetPlan,
    probes: &[HostProbeState],
    executors: &[E],
    outcomes: &mut [fleet::HostOutcome],
) where
    E: exec::DeployExecutor,
    P: ProxyController,
{
    let modes: Vec<fleet::HostMode> = plan.hosts.iter().map(|host| host.mode).collect();
    let actions = fleet::fleet_rollback_set(outcomes, &modes);
    if actions.is_empty() {
        return;
    }

    eprintln!(
        "\u{21A9}\u{FE0F}  Compensating {} host(s) in reverse rollout order \u{2014} BINARIES \
         only; a migration that already ran is NOT rolled back.\n",
        actions.len(),
    );
    for action in actions {
        let index = match action {
            fleet::RollbackAction::Rollback(index)
            | fleet::RollbackAction::Teardown(index)
            | fleet::RollbackAction::Manual(index, _) => index,
        };
        let cfg = &input.fleet.hosts[index];
        let host = plan.hosts[index].host.as_str();
        outcomes[index] = match action {
            fleet::RollbackAction::Rollback(_) => {
                eprintln!(
                    "\u{21A9}\u{FE0F}  [{host}] rolling back to the previous release\u{2026}"
                );
                compensate_rollback(cfg, input, &executors[index])
            }
            fleet::RollbackAction::Teardown(_) => {
                eprintln!(
                    "\u{21A9}\u{FE0F}  [{host}] removing this run's first deploy \
                     (no previous release to return to)\u{2026}"
                );
                compensate_teardown(cfg, input, &probes[index].slots, &executors[index])
            }
            fleet::RollbackAction::Manual(_, reason) => {
                for line in fleet::manual_recovery_lines(cfg, reason) {
                    eprintln!("{line}");
                }
                // An ambiguous-marker host already describes itself more precisely
                // than "manual" does; keep the specific state.
                if matches!(outcomes[index], fleet::HostOutcome::AmbiguousMarkers) {
                    fleet::HostOutcome::AmbiguousMarkers
                } else {
                    fleet::HostOutcome::Manual { reason }
                }
            }
        };
        eprintln!();
    }
}

/// Roll ONE host back to its previous release, with the target-dir precheck
/// (issue #1621, §4.7).
///
/// Every refusal fails CLOSED to [`fleet::HostOutcome::Manual`]: the host stays on
/// the new release and is reported, which is strictly better than flipping it at a
/// release dir we cannot prove exists — that failure lands post-boundary, with no
/// teardown, turning one broken host into two.
fn compensate_rollback<E, P>(
    cfg: &ResolvedDeployConfig,
    input: &FleetUpInput<'_, P>,
    executor: &E,
) -> fleet::HostOutcome
where
    E: exec::DeployExecutor,
    P: ProxyController,
{
    let target = match exec::resolve_rollback_target(cfg, input.public_port, executor) {
        Ok(target) => target,
        // The host records no previous release (a first deploy clears the marker,
        // and a host deployed before the marker existed simply has none).
        Err(exec::DeployExecError::NoPreviousRelease) => {
            return manual_outcome(cfg, fleet::MANUAL_NO_PREVIOUS_RELEASE);
        }
        // The probe itself could not run: we know nothing, so we change nothing.
        Err(_) => {
            return manual_outcome(cfg, fleet::MANUAL_ROLLBACK_TARGET_UNPROVABLE);
        }
    };
    match exec::probe_rollback_target_dir(&target.release_dir, executor) {
        Ok(exec::ReleaseDirState::Present) => {}
        Ok(exec::ReleaseDirState::Absent) => {
            return manual_outcome(cfg, fleet::MANUAL_ROLLBACK_TARGET_MISSING);
        }
        Ok(exec::ReleaseDirState::Unreadable) | Err(_) => {
            return manual_outcome(cfg, fleet::MANUAL_ROLLBACK_TARGET_UNPROVABLE);
        }
    }

    let ops = exec::rollback_ops(cfg, input.proxy, &target);
    // Same teardown as the on-demand rollback: a failure at or before the
    // health-gated flip disables the slot it just restarted, so the host is left
    // serving what it was serving when compensation started.
    let teardown = exec::rollback_teardown_ops(cfg, &target);
    match exec::execute_rollback(input.checks, &ops, &teardown, executor) {
        Ok(()) => fleet::HostOutcome::CompensatedRollback,
        Err(err) => {
            let failed_step = fleet::failed_step_label(&err);
            eprintln!(
                "\u{274C} [{}] the compensating rollback FAILED at `{failed_step}` \u{2014} this \
                 host is still on {}. The remaining hosts are still compensated.",
                cfg.host.as_deref().unwrap_or_default(),
                input.release_id,
            );
            fleet::HostOutcome::CompensationFailed { failed_step }
        }
    }
}

/// Remove ONE host's just-completed FIRST deploy (issue #1621, §4.7).
///
/// A first deploy has no `shared/previous-release` marker, so there is nothing to
/// roll back to: the honest compensation is the first-deploy teardown, which stops
/// the slot unit, removes this run's release dir, and clears the `current` symlink
/// and slot markers — leaving the host in the nothing-installed state that makes
/// the next `deploy up` correctly take the First path again.
///
/// Driven through [`exec::run_ops`], not `run_teardown`: at fleet scale a silently
/// swallowed cleanup failure is how a host ends up half-removed with nobody told.
///
/// **Known residue:** [`ProxyController`] has no deregister op, so this host's
/// kamal-proxy still holds a route for the service pointing at the stopped slot —
/// its public port answers 502 rather than refusing the connection until it is
/// deployed again. Removing the route needs a new controller method (and its own
/// exact-vector tests); the state table names the host so this is never a surprise.
fn compensate_teardown<E, P>(
    cfg: &ResolvedDeployConfig,
    input: &FleetUpInput<'_, P>,
    slots: &exec::SlotPlan,
    executor: &E,
) -> fleet::HostOutcome
where
    E: exec::DeployExecutor,
    P: ProxyController,
{
    let ops = exec::first_deploy_teardown_ops(cfg, input.release_id, slots);
    match exec::run_ops(&ops, executor) {
        Ok(()) => fleet::HostOutcome::CompensatedTeardown,
        Err(err) => {
            let failed_step = fleet::failed_step_label(&err);
            eprintln!(
                "\u{274C} [{}] removing the first deploy FAILED at `{failed_step}` \u{2014} this \
                 host is still on {}. The remaining hosts are still compensated.",
                cfg.host.as_deref().unwrap_or_default(),
                input.release_id,
            );
            fleet::HostOutcome::CompensationFailed { failed_step }
        }
    }
}

/// Decline the automatic rollback for one host, print the by-hand recovery block,
/// and record the reason.
fn manual_outcome(cfg: &ResolvedDeployConfig, reason: &'static str) -> fleet::HostOutcome {
    for line in fleet::manual_recovery_lines(cfg, reason) {
        eprintln!("{line}");
    }
    fleet::HostOutcome::Manual { reason }
}

/// Build the typed halt error from the recorded per-host outcomes (issue #1621,
/// AC-3).
///
/// Secrets discipline: every field is a host name or a `&'static str` op label —
/// never a shell line, a remote path, or a formatted driver error (a migration
/// failure's source can embed the database URL).
///
/// The lists describe the FINAL state, so this must be built after compensation:
/// a host the fleet rolled back is `rolled_back`, and a host whose rollback FAILED
/// stays in `still_on_new` — reporting the attempt instead of the result is how an
/// operator ends up believing a mixed fleet converged.
///
/// `degraded` is passed in rather than derived, because a degraded host that was
/// later compensated no longer carries that state in its outcome slot and its
/// housekeeping debris outlives the rollback.
fn fleet_halted(
    plan: &fleet::FleetPlan,
    outcomes: &[fleet::HostOutcome],
    degraded: &[(String, &'static str)],
    failed_host: String,
    failed_step: &'static str,
) -> DeployError {
    let named = |want: fn(&fleet::HostOutcome) -> bool| -> Vec<String> {
        plan.hosts
            .iter()
            .zip(outcomes)
            .filter(|(_, outcome)| want(outcome))
            .map(|(host, _)| host.host.clone())
            .collect()
    };
    DeployError::FleetHalted(Box::new(FleetHalt {
        failed_host,
        failed_step,
        rolled_back: named(|o| {
            matches!(
                o,
                fleet::HostOutcome::RolledBack { .. } | fleet::HostOutcome::CompensatedRollback
            )
        }),
        torn_down: named(|o| {
            matches!(
                o,
                fleet::HostOutcome::TornDown { .. } | fleet::HostOutcome::CompensatedTeardown
            )
        }),
        // Shared with the summary table's own list, so the halt error and the state
        // table can never disagree about which hosts are still forward.
        still_on_new: named(fleet::HostOutcome::on_new_release),
        degraded: degraded.to_vec(),
        manual: plan
            .hosts
            .iter()
            .zip(outcomes)
            .filter_map(|(host, outcome)| match outcome {
                fleet::HostOutcome::AmbiguousMarkers => {
                    Some((host.host.clone(), fleet::MANUAL_AMBIGUOUS_MARKERS))
                }
                fleet::HostOutcome::Manual { reason } => Some((host.host.clone(), *reason)),
                _ => None,
            })
            .collect(),
    }))
}

/// Perform a real on-demand rollback (issue #1607, Slice 3, AC-4).
///
/// Loads config via the same dotenv-aware path and runs the same preflight as
/// `up`, aborting before touching the server on any failure. It then resolves the
/// previous release on the target ([`exec::resolve_rollback_target`]) — failing
/// loudly and non-zero via [`exec::DeployExecError::NoPreviousRelease`] when
/// there is nothing to roll back to — and drives [`exec::rollback_ops`]: bring the
/// previous slot's unit back up, flip the proxy back to it, repoint `current`, and
/// re-probe `/ready`.
/// `hosts` is the ordered target list, already narrowed by `--only` (issue #1621).
/// With more than one entry this becomes a FLEET rollback: every host, in strict
/// REVERSE declaration order, best-effort-continue. With one entry (or none — a
/// config with no target still flows into the preflight report) it is the
/// pre-#1621 single-host path verbatim.
fn run_rollback(
    config: &AutumnConfig,
    resolved: &ResolvedDeployConfig,
    hosts: &[String],
    configured_host_count: usize,
) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy rollback\n");

    // Same SQLite data-file contract as `up` (#1909): a rollback target deployed
    // BEFORE the data file was adopted into `shared/` holds no file at that path
    // any more, so the rolled-back release is re-linked at it rather than
    // starting against a fresh, empty database.
    let resolved = &resolved
        .clone()
        .with_sqlite_data_file(sqlite_relative_data_file(config, resolved));

    // The rollback targets, in declaration order. A single-host config (either
    // spelling) resolves to a one-host fleet whose only element IS today's
    // `resolved`, so the branch below is the pre-#1621 sequence at N = 1.
    let fleet = ResolvedFleet::from_targets(resolved, hosts);
    // Which rollback path this run takes, decided ONCE and reused by the preflight
    // gate below and the driver branch further down, so the two can never disagree.
    // Note this is `is_single()`, not `is_single_host_deploy`: rolling ONE host back
    // — whether that is the whole config or a `--only` narrowing of it — really is a
    // single-host rollback, and the fleet driver has no second host to converge with.
    let fleet_rollback = !fleet.is_single();

    // Fail fast: the same preflight as `up`, before any remote call. Every host is
    // graded, so an unreachable host in position 3 is reported before host 3 — or
    // any other host — is touched. The GATE is narrower than `up`'s, though: see
    // [`blocking_rollback_failures`].
    let checks = collect_fleet_preflight(config, &fleet, configured_host_count);
    let failed = report_preflight(&checks);
    if blocking_rollback_failures(&checks, fleet_rollback) > 0 {
        return Err(DeployError::PreflightFailed(failed));
    }
    if failed > 0 {
        eprintln!(
            "\u{26A0}\u{FE0F}  {failed} host(s) above did not answer the reachability probe. A \
             ROLLBACK continues without them \u{2014} stopping would leave the hosts that ARE \
             reachable on the release you are leaving \u{2014} but each is reported as NOT \
             rolled back and this command still exits non-zero.\n"
        );
    }

    // Show what a rollback will do before it runs (descriptive, not a dry-run gate).
    // These are the steps EACH host runs; the fleet ordering is printed below them.
    eprintln!("Rollback steps:");
    for (i, step) in build_rollback_plan(resolved).iter().enumerate() {
        eprintln!("  {}. [{}] {}", i + 1, step.label, step.description);
    }
    eprintln!();

    let public_port = config.server.port;
    let proxy = proxy::KamalProxyController::new(resolved.readiness_timeout_secs)
        .with_tls_host(resolved.tls_host.clone());

    if fleet_rollback {
        return run_fleet_rollback_with(&checks, &fleet, &proxy, public_port, |cfg| {
            exec::SshTarget::from_resolved(cfg)
                .map(exec::SshExecutor::new)
                .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))
        });
    }

    // Same rule as the `up` driver (#2274): a hostless fleet is a config error with
    // a message, not an index panic.
    let single = fleet
        .hosts
        .first()
        .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))?;
    let target = exec::SshTarget::from_resolved(single)
        .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))?;
    let executor = exec::SshExecutor::new(target);

    // Resolve the previous release to roll back to (non-zero error if none).
    let rollback_target = exec::resolve_rollback_target(single, public_port, &executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;
    eprintln!(
        "Rolling back {} to {} ({})\u{2026}\n",
        single.host.as_deref().unwrap_or_default(),
        rollback_target.release_dir,
        rollback_target.slot,
    );

    let ops = exec::rollback_ops(single, &proxy, &rollback_target);
    // If the rollback fails at or before the health-gated flip, disable the slot it
    // restarted so the original release is left cleanly serving (the flip never
    // moved traffic, and no marker is written until after the flip).
    let teardown = exec::rollback_teardown_ops(single, &rollback_target);
    exec::execute_rollback(&checks, &ops, &teardown, &executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;

    eprintln!("\n\u{2705} Rollback complete.");
    Ok(())
}

/// The grader name a per-host reachability row carries. Kept as one constant so
/// the rollback gate below and the skip it implies can never key on different
/// spellings than [`grade_ssh_reachability`] emits.
const SSH_REACHABILITY_CHECK: &str = "ssh_reachability";

/// Whether a preflight check is a PER-HOST reachability row of a real fleet
/// (issue #1621): the grader name plus a scope, which
/// [`collect_fleet_preflight`] attaches only for a fleet of more than one host.
fn is_per_host_reachability(check: &PreflightCheck) -> bool {
    check.name == SSH_REACHABILITY_CHECK && check.scope.is_some()
}

/// How many preflight failures must ABORT a fleet rollback before it touches
/// anything (issue #1621, §3.2).
///
/// `deploy up` refuses the whole rollout if any host fails — AC-7 — because a
/// rollout is a CHANGE, and starting one against a fleet it cannot finish is how a
/// fleet ends up mixed. A rollback is the opposite kind of command: it is recovery,
/// its documented contract is best-effort-continue
/// ([`run_fleet_rollback_with`]), and the host that stopped answering SSH is very
/// often the reason the operator is rolling back at all. Refusing to move the
/// healthy majority off a bad release because one host is dead leaves MORE hosts on
/// the release being abandoned — the exact outcome that contract exists to prevent.
///
/// So exactly one class is downgraded to a reported, non-blocking row: a per-host
/// `ssh_reachability` failure in a real fleet. It is still printed by
/// [`report_preflight`], that host is still skipped and reported as NOT rolled back,
/// and the command still exits non-zero via [`DeployError::FleetRollbackFailed`].
///
/// Everything else keeps the hard gate. A project-wide grader describes the release
/// being rolled back TO rather than one host's reachability, and a SINGLE-host
/// rollback keeps the pre-#1621 gate verbatim (AC-1) — which is what
/// `fleet_rollback` is for: the softening only makes sense when
/// [`run_fleet_rollback_with`] is the code that actually runs, because only it
/// skips-and-reports the unreachable host. The single-host path would sail past the
/// gate straight into an SSH it already knows will fail, so for it EVERY failure
/// blocks, exactly as before #1621. (Pre-#1621 this fell out of the rows being
/// unscoped; since a `--only`-narrowed run now scopes its rows for attribution, the
/// gate keys on the path instead of inferring it.)
fn blocking_rollback_failures(checks: &[PreflightCheck], fleet_rollback: bool) -> usize {
    checks
        .iter()
        .filter(|check| check.blocking() && !(fleet_rollback && is_per_host_reachability(check)))
        .count()
}

/// Roll EVERY host in `fleet` back to its previous release, newest first (issue
/// #1621, §3.2).
///
/// The executor factory is injected (generic, never `dyn`, no `Send`/`Sync` bound)
/// so the whole loop is unit-testable against one scripted fake per host.
///
/// Three deliberate differences from the rollout driver, all following from the
/// fact that a rollback is a RECOVERY action rather than a change:
///
/// 1. **Reverse declaration order.** The rollout replaces hosts front to back, so
///    the hosts at the back have been on the new release for the shortest time;
///    undoing them first shrinks the mixed window from the newest end and mirrors
///    the compensation order exactly.
/// 2. **Best-effort continue, never halt.** A host that fails does not stop the
///    others: stopping would leave MORE hosts on the release the operator is trying
///    to leave. Every failure is recorded and reported — never swallowed.
/// 3. **No previous release is not fatal — to the OTHER hosts.** A host that has
///    never been redeployed (or was just added to the fleet) has no marker to roll
///    back to. It is reported as a SKIP rather than as a failed rollback, and it
///    never stops the rest of the fleet.
///
/// **Exit semantics (deliberate):** the command succeeds only when EVERY host
/// rolled back. A host that failed and a host that had nothing to roll back to are
/// distinguished in the table and in the operator's remedy, but both exit non-zero,
/// because the contract of a fleet rollback is that the FLEET returns to its
/// previous release — and a fleet where one host could not follow the others is
/// mixed. Reporting that as success is how a mixed fleet survives an incident
/// unnoticed.
fn run_fleet_rollback_with<E, P, F>(
    checks: &[PreflightCheck],
    fleet: &ResolvedFleet,
    proxy: &P,
    public_port: u16,
    make_executor: F,
) -> Result<(), DeployError>
where
    E: exec::DeployExecutor,
    P: ProxyController,
    F: Fn(&ResolvedDeployConfig) -> Result<E, DeployError>,
{
    let total = fleet.hosts.len();
    // ONE executor per host, built up front: a host with no SSH target is a config
    // error, and finding it here means nothing has been flipped yet.
    let executors = fleet
        .hosts
        .iter()
        .map(&make_executor)
        .collect::<Result<Vec<E>, DeployError>>()?;

    let names: Vec<String> = fleet
        .hosts
        .iter()
        .map(|cfg| cfg.host.clone().unwrap_or_default())
        .collect();
    eprintln!(
        "Rolling {total} hosts back to their previous release, ONE AT A TIME, NEWEST FIRST \
         (the reverse of `[deploy] hosts` order):\n"
    );

    // Every slot is overwritten below — the loop is exhaustive and, unlike the
    // rollout driver, never breaks early — so the seed value is unobservable. It is
    // the conservative one regardless: "nothing was rolled back here".
    let mut outcomes = vec![fleet::RollbackOutcome::NoPreviousRelease; total];
    for index in (0..total).rev() {
        let cfg = &fleet.hosts[index];
        let host = names[index].as_str();
        eprintln!("[{}/{total} {host}] rolling back\u{2026}", index + 1);
        // Preflight is fleet-wide, but a per-host reachability row belongs to ONE
        // host: forwarding another host's failed row would abort this host's
        // `execute_rollback` for a fault that is not its own
        // (see `blocking_rollback_failures`).
        let host_checks: Vec<PreflightCheck> = checks
            .iter()
            .filter(|check| {
                !is_per_host_reachability(check) || check.scope.as_deref() == Some(host)
            })
            .cloned()
            .collect();
        outcomes[index] = if host_checks.iter().any(PreflightCheck::blocking) {
            // Reported, never touched: its preflight row is already on screen, and
            // probing a host that did not answer a TCP connect would only stall the
            // hosts queued behind it.
            eprintln!(
                "\u{274C} [{host}] did not answer the preflight reachability probe \u{2014} \
                 this host was NOT rolled back. The remaining hosts still are."
            );
            fleet::RollbackOutcome::Failed {
                failed_step: SSH_REACHABILITY_CHECK,
            }
        } else {
            rollback_one_host(
                &host_checks,
                cfg,
                proxy,
                public_port,
                host,
                &executors[index],
            )
        };
        eprintln!();
    }

    for line in fleet::fleet_rollback_summary_lines(&names, &outcomes) {
        eprintln!("{line}");
    }

    let rolled_back = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, fleet::RollbackOutcome::RolledBack))
        .count();
    // Non-zero unless EVERY host came back: a fleet that half-rolled-back is mixed,
    // and reporting that as success is how a mixed fleet survives an incident
    // unnoticed. The table above distinguishes "failed" from "nothing to roll back
    // to", which is what the operator's next move turns on.
    if rolled_back < total {
        return Err(DeployError::FleetRollbackFailed {
            not_rolled_back: total - rolled_back,
            total,
        });
    }
    eprintln!("\n\u{2705} Fleet rollback complete \u{2014} all {total} hosts restored.");
    Ok(())
}

/// Roll ONE host back to its previous release for a fleet-wide rollback.
///
/// Uses the UNCHANGED on-demand primitives ([`exec::resolve_rollback_target`] →
/// [`exec::rollback_ops`] → [`exec::execute_rollback`], with the same
/// [`exec::rollback_teardown_ops`]), so a fleet rollback is literally N single-host
/// rollbacks and can never drift from the one the operator runs by hand.
///
/// Returns an outcome instead of an error because the caller continues past a
/// failure: the report is the aggregate, and nothing here is swallowed. Only the
/// `&'static str` step LABEL is printed — never the driver's message, which can
/// carry remote output.
fn rollback_one_host<E, P>(
    checks: &[PreflightCheck],
    cfg: &ResolvedDeployConfig,
    proxy: &P,
    public_port: u16,
    host: &str,
    executor: &E,
) -> fleet::RollbackOutcome
where
    E: exec::DeployExecutor,
    P: ProxyController,
{
    let target = match exec::resolve_rollback_target(cfg, public_port, executor) {
        Ok(target) => target,
        Err(exec::DeployExecError::NoPreviousRelease) => {
            eprintln!(
                "\u{26A0}\u{FE0F}  [{host}] no previous release recorded \u{2014} nothing to \
                 roll back to on this host; it keeps serving what it is serving now."
            );
            return fleet::RollbackOutcome::NoPreviousRelease;
        }
        Err(err) => {
            let failed_step = fleet::failed_step_label(&err);
            eprintln!(
                "\u{274C} [{host}] could not resolve the previous release (`{failed_step}`) \
                 \u{2014} this host was NOT rolled back. The remaining hosts still are."
            );
            return fleet::RollbackOutcome::Failed { failed_step };
        }
    };
    eprintln!(
        "  \u{2192} {host} returns to {} ({})",
        target.release_dir, target.slot,
    );

    let ops = exec::rollback_ops(cfg, proxy, &target);
    // Same teardown as the on-demand rollback: a failure at or before the
    // health-gated flip disables the slot it just restarted, so this host is left
    // serving exactly what it served before the attempt.
    let teardown = exec::rollback_teardown_ops(cfg, &target);
    match exec::execute_rollback(checks, &ops, &teardown, executor) {
        Ok(()) => fleet::RollbackOutcome::RolledBack,
        Err(err) => {
            let failed_step = fleet::failed_step_label(&err);
            eprintln!(
                "\u{274C} [{host}] the rollback FAILED at `{failed_step}` \u{2014} this host \
                 was not restored. The remaining hosts still are."
            );
            fleet::RollbackOutcome::Failed { failed_step }
        }
    }
}

// ── `deploy status` (issue #1621, AC-6) ──────────────────────────────────────

/// Probe every fleet host read-only and collect its status row (issue #1621).
///
/// Serial and deterministic (declaration order), like every other fleet loop here:
/// threading the probes would put a `Sync` bound on `DeployExecutor` that the
/// `RefCell`-based test fake cannot satisfy, and status output must come out in
/// rollout order anyway. Parallelism is a later, isolated optimisation.
///
/// A host whose probe fails becomes an `unreachable` ROW, never an abort:
/// `deploy status` exists for incidents, and an incident is exactly when a host
/// may not answer. Only an executor-construction failure (a config with no SSH
/// target at all) aborts, because then NO host could be probed.
fn collect_fleet_status<E, F>(
    fleet: &ResolvedFleet,
    public_port: u16,
    make_executor: F,
) -> Result<Vec<fleet::HostStatus>, DeployError>
where
    E: exec::DeployExecutor,
    F: Fn(&ResolvedDeployConfig) -> Result<E, DeployError>,
{
    let mut statuses = Vec::with_capacity(fleet.hosts.len());
    for cfg in &fleet.hosts {
        let executor = make_executor(cfg)?;
        // The probe error is deliberately not printed: it can carry remote stderr,
        // and "unreachable" in the table is the operator-facing fact.
        let status = exec::probe_host_status(cfg, public_port, &executor).map_or_else(
            |_| fleet::HostStatus::unreachable(cfg.host.clone().unwrap_or_default()),
            |probe| fleet::HostStatus::from_probe(cfg, public_port, &probe),
        );
        statuses.push(status);
    }
    Ok(statuses)
}

/// The stable `deploy status --json` shape (issue #1621, AC-6).
///
/// The repo's skills are a first-class consumer, so the field set is a documented
/// contract:
///
/// - `hosts`: one object per host, in `[deploy] hosts` (rollout) order, with
///   `host` (string), `reachable` (bool), `mode`
///   (`"deployed"`/`"not deployed"`/`"unknown"`/`"unreachable"`), `release`
///   (string or null when unknown), `live_slot` (string or null), `ready` (HTTP
///   status number or null), `maintenance` (`true`/`false` as the host's RUNNING
///   slot unit observes it, or `null` when the CLI could not prove which flag file
///   that unit polls — see `fleet::DRIFT_MAINTENANCE_UNPROVEN`), `proxy_port`
///   (number or null),
///   `last_deploy` (AC-6's third fact: `{result, at}` — `result` is `"deployed"`,
///   `"rolled back"` or `"torn down"` (a first deploy the driver compensated away,
///   so the host serves nothing), `at` the host's UTC timestamp or null — and the
///   whole object is null when the host has never completed a cutover or the marker
///   could not be read), and `drift` (array of reason strings, empty when none).
///   `last_deploy` is the host's own last COMPLETED action, not a verdict on the
///   last rollout: a deploy that failed before cutover never rewrites it (see
///   `fleet::LAST_DEPLOY_SCOPE_NOTE`).
/// - `version_drift` (bool), `state_drift` (array of `{host, reason}`), and
///   `drifted` (bool — the `--strict` condition).
///
/// Everything here is a host name, an enum word, a number, or a `&'static str`
/// reason — never a shell line, a remote capture, or a driver error, so no secret
/// can reach it.
fn fleet_status_json(
    statuses: &[fleet::HostStatus],
    report: &fleet::DriftReport,
) -> serde_json::Value {
    let hosts: Vec<serde_json::Value> = statuses
        .iter()
        .map(|status| {
            let mode = if status.reachable {
                match status.mode {
                    Some(fleet::HostMode::First) => "not deployed",
                    Some(fleet::HostMode::Redeploy) => "deployed",
                    None => "unknown",
                }
            } else {
                "unreachable"
            };
            serde_json::json!({
                "host": status.host,
                "reachable": status.reachable,
                "mode": mode,
                "release": match &status.release {
                    fleet::ReleaseId::Known(id) => serde_json::Value::String(id.clone()),
                    fleet::ReleaseId::Unknown => serde_json::Value::Null,
                },
                "live_slot": status.live_slot,
                "ready": status.ready_code,
                // Three-valued since #1621 review round 1: `true`/`false` are
                // proved states of the file the RUNNING slot unit polls, and
                // `null` is "the CLI could not prove which file that is". Both
                // false and null stay falsy for a consumer that tests the field
                // directly, so `maintenance == true` never widens.
                "maintenance": match status.maintenance {
                    exec::MaintenanceStatus::On => serde_json::Value::Bool(true),
                    exec::MaintenanceStatus::Off => serde_json::Value::Bool(false),
                    exec::MaintenanceStatus::Unknown => serde_json::Value::Null,
                },
                "proxy_port": match status.installed_proxy_port {
                    exec::InstalledProxyPort::Port(port) => {
                        serde_json::Value::Number(port.into())
                    }
                    _ => serde_json::Value::Null,
                },
                "last_deploy": status.last_deploy.as_ref().map_or(
                    serde_json::Value::Null,
                    |last| serde_json::json!({ "result": last.result, "at": last.at }),
                ),
                "drift": report
                    .state_drift
                    .iter()
                    .filter(|(host, _)| *host == status.host)
                    .map(|(_, reason)| *reason)
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({
        "hosts": hosts,
        "version_drift": report.version_drift,
        "state_drift": report
            .state_drift
            .iter()
            .map(|(host, reason)| serde_json::json!({ "host": host, "reason": reason }))
            .collect::<Vec<_>>(),
        "drifted": report.drifted(),
    })
}

/// `autumn deploy status`: probe every host read-only and render the shared
/// per-host table (or `--json`), exiting non-zero on drift only under `--strict`
/// (issue #1621, AC-6).
/// Say out loud that the public port was resolved WITHOUT validating the app
/// config, and what this command does about it (issue #1621, review round 1).
///
/// Never silent, and never on stdout: a `--json` consumer keeps the documented
/// shape while a human still sees WHY the port was resolved the hard way and WHICH
/// port the command is working against. `continues` is the caller's own second
/// line, because `status` (read-only) and `maintenance` (which uses the port only
/// to identify the live slot unit) owe the operator different sentences.
fn warn_degraded_port(port: &StatusPort, profile: &str, continues: &str) {
    let Some(reason) = &port.degraded else {
        return;
    };
    eprintln!(
        "\u{26A0}\u{FE0F}  this project's configuration does not validate under the \
         `{profile}` deploy profile: {reason}"
    );
    eprintln!("{continues}\n");
}

fn run_status(
    port: &StatusPort,
    profile: &str,
    fleet: &ResolvedFleet,
    options: &DeployOptions,
) -> Result<(), DeployError> {
    if !options.json {
        eprintln!("\u{1F342} autumn deploy status\n");
    }
    warn_degraded_port(
        port,
        profile,
        &format!(
            "   `deploy status` is read-only, so it continues against the DECLARED \
             `[server] port = {}` for that profile (read without validation). \
             `autumn deploy check`/`up` still refuse to run until the config is fixed.",
            port.port
        ),
    );
    let statuses = collect_fleet_status(fleet, port.port, |cfg| {
        exec::SshTarget::from_resolved(cfg)
            .map(exec::SshExecutor::new)
            .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))
    })?;
    let report = fleet::fleet_drift(&statuses);
    if options.json {
        // Reports (like `plan`) go to stdout; a `Value` cannot fail to serialise.
        println!(
            "{}",
            serde_json::to_string_pretty(&fleet_status_json(&statuses, &report))
                .expect("a serde_json::Value always serializes")
        );
    } else {
        for line in fleet::fleet_status_lines(&statuses, &report) {
            println!("{line}");
        }
    }
    if options.strict && report.drifted() {
        return Err(DeployError::DriftDetected);
    }
    Ok(())
}

// ── `deploy maintenance on|off` fan-out (issue #1621, AC-5) ──────────────────

/// Serialize the maintenance flag body from the CLI flags (issue #1621).
///
/// Goes through `autumn_web::maintenance::MaintenanceConfig` — the exact struct
/// the RUNTIME deserializes — and the same pretty-printed JSON
/// `MaintenanceState::save_to_file` writes locally, so the fleet fan-out and the
/// local `autumn maintenance on` can never disagree about the wire format.
fn maintenance_flag_body(args: &MaintenanceOnArgs) -> String {
    let config = autumn_web::maintenance::MaintenanceConfig {
        message: args.message.clone(),
        allow_ips: args.allow_ips.clone(),
        readonly: args.readonly,
        bypass_header: args.bypass_header.clone(),
    };
    serde_json::to_string_pretty(&config).expect("MaintenanceConfig always serializes")
}

/// Fan `maintenance on|off` out over every fleet host (issue #1621, AC-5).
///
/// **Best-effort-and-aggregate — deliberately the OPPOSITE of the rollout
/// driver** (plan §6.2). Rollout halts and compensates because a mixed-version
/// fleet is the harm; a control-plane switch that succeeded on 2 of 3 hosts must
/// NOT be rolled back (that pushes users back into the live window the operator is
/// closing) and must NOT stop at the failure (the remaining hosts are still
/// serving). Every host is attempted, every outcome is named in the table, and the
/// command exits non-zero when ANY host failed.
///
/// Per amendment A2 each `on` writes up to two flag paths, shared one FIRST:
///
/// - `{app_dir}/shared/autumn-maintenance.json` — the authoritative path the
///   #1621 slot units point `AUTUMN_MAINTENANCE_FLAG_FILE` at; survives cutovers.
///   Always written first, so a #1621 unit reacts within its 500 ms poll even if
///   the second write fails.
/// - the file the host's **live slot unit** actually makes the app poll, when that
///   is a DIFFERENT file — a unit deployed before #1621 carries no override and
///   polls its own `WorkingDirectory`-relative `tmp/autumn-maintenance.json`.
///   Resolved from the proxy-selected live slot's unit, exactly as `deploy status`
///   resolves the flag it REPORTS (review round 3); never from the `current`
///   symlink, which can name a different release than the unit that is running.
///
/// `off` removes the same set (`rm -f`, absent files are not an error).
///
/// `public_port` is what maps the proxy's routed target back to a slot, so it is
/// resolved exactly as `deploy status` resolves it — degraded fallback included,
/// because a maintenance window gets closed mid-incident and an unrelated invalid
/// setting must not stand in the way.
///
/// The executor factory is injected (generic, never `dyn`, no `Send`/`Sync`
/// bound) so the whole loop is unit-testable against one scripted fake per host.
fn run_fleet_maintenance_with<E, F>(
    fleet: &ResolvedFleet,
    port: &StatusPort,
    profile: &str,
    action: DeployAction,
    args: Option<&MaintenanceOnArgs>,
    make_executor: F,
) -> Result<(), DeployError>
where
    E: exec::DeployExecutor,
    F: Fn(&ResolvedDeployConfig) -> Result<E, DeployError>,
{
    let on = matches!(action, DeployAction::MaintenanceOn);
    let verb = if on { "on" } else { "off" };
    eprintln!("\u{1F342} autumn deploy maintenance {verb}\n");
    warn_degraded_port(
        port,
        profile,
        &format!(
            "   `deploy maintenance` continues against the DECLARED `[server] port = {}` \
             for that profile (read without validation); it uses that port only to identify \
             which slot unit each host is running. `autumn deploy check`/`up` still refuse \
             to run until the config is fixed.",
            port.port
        ),
    );

    let total = fleet.hosts.len();
    // ONE executor per host, built up front: a host with no SSH target is a config
    // error, and finding it here means no host has been half-changed yet.
    let executors = fleet
        .hosts
        .iter()
        .map(&make_executor)
        .collect::<Result<Vec<E>, DeployError>>()?;
    let names: Vec<String> = fleet
        .hosts
        .iter()
        .map(|cfg| cfg.host.clone().unwrap_or_default())
        .collect();

    let default_args = MaintenanceOnArgs::default();
    let body = maintenance_flag_body(args.unwrap_or(&default_args));

    let mut outcomes = Vec::with_capacity(total);
    for (index, cfg) in fleet.hosts.iter().enumerate() {
        eprintln!(
            "[{}/{total} {}] maintenance {verb}\u{2026}",
            index + 1,
            names[index],
        );
        outcomes.push(maintenance_one_host(
            cfg,
            port.port,
            on,
            &body,
            &executors[index],
        ));
        eprintln!();
    }

    for line in fleet::fleet_maintenance_summary_lines(&names, &outcomes, on) {
        eprintln!("{line}");
    }

    let failed = outcomes.iter().filter(|outcome| outcome.failed()).count();
    if failed > 0 {
        return Err(DeployError::FleetMaintenanceFailed {
            verb: if on { "on" } else { "off" },
            failed,
            total,
        });
    }
    Ok(())
}

/// What the write path could prove about the file this host's RUNNING unit polls
/// (issue #1621, review round 3).
enum LiveFlagTarget {
    /// Nothing is promoted on this host, so no unit is serving and the shared write
    /// is the whole job.
    NoRelease,
    /// The live slot's unit resolves to exactly this file.
    Path(String),
    /// A release IS live, but its unit could not be read — so which file the
    /// application polls is NOT proved. Fail closed: never guessed from `current`.
    Unproved,
}

/// Apply `maintenance on|off` to ONE host, returning what happened — never an
/// error, because the caller continues past a failure (issue #1621, §6.2).
///
/// Two read-only probes first: `detect-current` (the same shared probe `up` runs)
/// for the live-slot decision, then `detect-maintenance-flag` — the SAME on-host
/// resolution `deploy status` reads the maintenance verdict with
/// ([`exec::probe_live_maintenance_flag`]) — for the file that slot's unit actually
/// makes the app poll. Resolving the write target the way the read path resolves
/// its answer is the point: the `current` symlink and the running unit disagree
/// whenever a proxy flip landed and `commit-markers` did not, and a flag written
/// under `current` is then a file nothing polls (review round 3).
///
/// Then the writes/removals per amendment A2, shared path FIRST.
///
/// Only op LABELS ever leave this function — the raw error can carry remote
/// stderr, and the aggregate report must not.
fn maintenance_one_host<E: exec::DeployExecutor>(
    cfg: &ResolvedDeployConfig,
    public_port: u16,
    on: bool,
    body: &str,
    executor: &E,
) -> fleet::MaintenanceOutcome {
    let Ok(probe) = exec::probe_deploy_state(cfg, executor) else {
        return fleet::MaintenanceOutcome::Failed {
            failed_step: "detect-current",
        };
    };
    let target = match probe.reconcile(cfg, public_port) {
        None => LiveFlagTarget::NoRelease,
        // An unreadable unit AND an unrunnable probe both mean the same thing here:
        // nothing is proved. The shared write still goes ahead below — it is the
        // authoritative path and costs nothing to be right about — but the host is
        // reported as only partially changed.
        Some(reconcile) => {
            match exec::probe_live_maintenance_flag(cfg, reconcile.live_slot, executor) {
                Ok(Some(flag)) => LiveFlagTarget::Path(flag.path),
                Ok(None) | Err(_) => LiveFlagTarget::Unproved,
            }
        }
    };

    let shared = cfg.maintenance_flag_file();
    // The file the live unit polls, ONLY when it is not the shared one already
    // being written: a #1621 unit points at the shared path, and writing a second
    // copy nothing reads would just leave litter for the next operator to wonder at.
    let live_path = match &target {
        LiveFlagTarget::Path(path) if *path != shared => Some(path.clone()),
        _ => None,
    };

    let mut ops: Vec<exec::DeployOp> = Vec::new();
    if on {
        // Shared (authoritative) flag first: a #1621 unit reacts within 500 ms of
        // this single write, so the window starts closing even if the write below
        // fails (amendment A2).
        ops.push(exec::DeployOp::WriteFile {
            label: "maintenance-write-shared",
            contents: exec::FileContents::Plain(body.to_owned()),
            remote_path: shared,
            // 0600: the body can carry the `--bypass-header NAME:VALUE` token (a
            // credential that skips the 503 gate) and the `--allow-ips` list, and
            // this mode is applied to the local staging copy too. The flag is
            // written and read by the same `cfg.user` the slot unit runs as, so
            // 0600 is sufficient — same rule as the 0600 env file (AC-5).
            mode: Some(0o600),
        });
        if let Some(path) = &live_path {
            // The dir holding a pre-#1621 unit's flag (its `WorkingDirectory`'s
            // `tmp/`) may not exist yet — the runtime only creates it when IT
            // writes a flag — so mkdir -p first.
            if let Some(parent) = remote_parent_dir(path) {
                ops.push(exec::DeployOp::Run(exec::RemoteCommand::new(
                    "maintenance-prepare-live-flag-dir",
                    format!("mkdir -p {}", exec::shell_quote(parent)),
                )));
            }
            ops.push(exec::DeployOp::WriteFile {
                label: "maintenance-write-live-flag",
                contents: exec::FileContents::Plain(body.to_owned()),
                remote_path: path.clone(),
                // 0600 for the same reason as the shared write above.
                mode: Some(0o600),
            });
        }
    } else {
        // `rm -f` both paths in one op: absent files are the NORMAL case for at
        // least one of them (a host has either the new unit or the old), so a
        // missing file must never fail the `off`.
        let mut paths = exec::shell_quote(&shared);
        if let Some(path) = &live_path {
            paths.push(' ');
            paths.push_str(&exec::shell_quote(path));
        }
        ops.push(exec::DeployOp::Run(exec::RemoteCommand::new(
            "maintenance-clear",
            format!("rm -f {paths}"),
        )));
    }

    for (index, op) in ops.iter().enumerate() {
        if exec::run_ops(std::slice::from_ref(op), executor).is_err() {
            // The op label is known HERE regardless of the error's shape (an
            // upload failure carries no label), so the report can always name the
            // step without quoting the error.
            let failed_step = op.label();
            // Op 0 is the shared path in both directions, so failing it leaves the
            // host genuinely UNCHANGED; failing anything after it means the shared
            // flag landed but the running unit's own file did not.
            return if index == 0 {
                fleet::MaintenanceOutcome::Failed { failed_step }
            } else {
                fleet::MaintenanceOutcome::LiveUnitUnchanged { failed_step }
            };
        }
    }
    match target {
        LiveFlagTarget::NoRelease => fleet::MaintenanceOutcome::AppliedSharedOnly,
        LiveFlagTarget::Path(_) => fleet::MaintenanceOutcome::Applied,
        // Fail closed. The shared flag changed, but a pre-#1621 unit would ignore
        // it, so this host must NOT be reported as maintained (or as released from
        // maintenance) when nothing proved which file it reads.
        LiveFlagTarget::Unproved => fleet::MaintenanceOutcome::LiveUnitUnchanged {
            failed_step: "detect-maintenance-flag",
        },
    }
}

/// The parent directory of a remote absolute path, or `None` when it has none.
fn remote_parent_dir(path: &str) -> Option<&str> {
    let (parent, _) = path.rsplit_once('/')?;
    (!parent.is_empty()).then_some(parent)
}

#[cfg(test)]
mod tests {
    // ── Tier 2 fail-fast on Windows (#1616) ────────────────────────────────
    //
    // The remote actions shell out to `ssh`/`sh`, and `up`, `rollback`, and
    // `maintenance` stage secrets with Unix file modes — `exec::stage_temp_file` skips
    // its `chmod 0600` off Unix — so a native Windows deploy would write the environment
    // file carrying the signing secret and database password without an owner-only mode.
    // That is exactly the silent degradation the platform policy forbids, so those
    // actions refuse up front.
    //
    // `check` and `plan` are the other half, and the half that first shipped wrong: they
    // are local-only, so refusing them claimed something untrue about the command and
    // took away the only way a Windows developer can validate a deploy config before
    // switching to WSL2.

    /// Every [`DeployAction`], for the completeness check below. The real gate
    /// is the wildcard-free match in `reaches_a_remote_host` — a new variant
    /// fails to compile until it is classified; this array only keeps the two
    /// lists in this module honest about covering it.
    const ALL_DEPLOY_ACTIONS: [DeployAction; 7] = [
        DeployAction::Check,
        DeployAction::Plan,
        DeployAction::Rollback,
        DeployAction::Up,
        DeployAction::Status,
        DeployAction::MaintenanceOn,
        DeployAction::MaintenanceOff,
    ];

    /// The actions that open an SSH session to a remote host. These are the
    /// Tier 2 surface: `up`/`rollback`/`maintenance` also stage secrets through
    /// `exec::stage_temp_file`, whose `chmod 0600` is `#[cfg(unix)]`.
    const REMOTE_ACTIONS: [DeployAction; 5] = [
        DeployAction::Up,
        DeployAction::Rollback,
        DeployAction::Status,
        DeployAction::MaintenanceOn,
        DeployAction::MaintenanceOff,
    ];

    /// The actions that never touch a remote host: `plan` renders the unit from
    /// `resolved` alone, and `check` grades config plus a portable
    /// `TcpStream::connect_timeout` reachability probe.
    const LOCAL_ACTIONS: [DeployAction; 2] = [DeployAction::Check, DeployAction::Plan];

    #[test]
    fn windows_deploys_are_refused_with_the_policy_message() {
        for action in REMOTE_ACTIONS {
            let refusal =
                windows_deploy_refusal("windows", action).expect("windows must be refused");
            assert!(refusal.contains("Tier 2 (WSL2)"), "{action:?}: {refusal}");
            assert!(
                refusal.contains(crate::platform::POLICY_DOC_URL),
                "{action:?}: {refusal}"
            );
        }
    }

    #[test]
    fn the_locally_verifiable_spine_runs_natively_on_windows() {
        // `deploy check` and `deploy plan` do NOT shell out to ssh and stage no
        // secrets — they grade config and render the unit. Refusing them would
        // take away the one way a Windows developer can validate a deploy config
        // before switching to WSL2 to run it, and it would be a false claim: they
        // work. (This is the regression the first green `windows-tier1` run hid,
        // because `Test (windows-latest)` had failed to link before reaching the
        // `integration::deploy::*` suite that proves it.)
        for action in LOCAL_ACTIONS {
            assert!(
                windows_deploy_refusal("windows", action).is_none(),
                "{action:?} is local-only and must run natively on Windows"
            );
        }
    }

    #[test]
    fn unix_deploys_are_not_refused() {
        for action in REMOTE_ACTIONS.iter().chain(LOCAL_ACTIONS.iter()).copied() {
            assert!(
                windows_deploy_refusal("linux", action).is_none(),
                "{action:?}"
            );
            assert!(
                windows_deploy_refusal("macos", action).is_none(),
                "{action:?}"
            );
        }
    }

    #[test]
    fn every_deploy_action_is_classified() {
        // A new `DeployAction` must be deliberately placed in one list or the
        // other. Without this, an added remote action would silently default to
        // "runs natively on Windows" — the silent degradation #1616 forbids.
        let classified = REMOTE_ACTIONS.len() + LOCAL_ACTIONS.len();
        assert_eq!(
            classified,
            ALL_DEPLOY_ACTIONS.len(),
            "every DeployAction must be classified as remote or local"
        );
        for action in ALL_DEPLOY_ACTIONS {
            assert!(
                REMOTE_ACTIONS.contains(&action) || LOCAL_ACTIONS.contains(&action),
                "{action:?} is not classified"
            );
        }
    }

    #[test]
    fn the_refusal_is_an_error_so_it_exits_non_zero() {
        // Fail *fast*: this must surface as an error the caller propagates,
        // never a warning printed before the deploy proceeds anyway.
        let err = DeployError::UnsupportedPlatform(
            windows_deploy_refusal("windows", DeployAction::Up).unwrap(),
        );
        assert!(err.to_string().contains("Tier 2 (WSL2)"));
    }

    #[test]
    fn the_refusal_does_not_masquerade_as_a_config_failure() {
        // `DeployError::Config` renders as "failed to load configuration: …",
        // which would send the operator hunting through autumn.toml for a
        // problem that is not there.
        let err = DeployError::UnsupportedPlatform(
            windows_deploy_refusal("windows", DeployAction::Up).unwrap(),
        );
        assert!(
            !err.to_string().contains("failed to load configuration"),
            "{err}"
        );
    }

    use super::*;

    fn resolved_defaults() -> ResolvedDeployConfig {
        ResolvedDeployConfig::resolve(&DeployConfig::default(), "myapp")
            .expect("deploy config resolves")
    }

    #[test]
    fn public_port_443_is_rejected_as_a_proxy_https_collision() {
        // kamal-proxy binds 443 for HTTPS by default (and can't disable it), so a
        // public/HTTP port of 443 collides and the proxy can't start — the guard
        // rejects it up front with an actionable message. It is unconditional (not
        // TLS-gated).
        let err = validate_public_port(443).expect_err("port 443 must be rejected");
        assert!(
            err.contains("443")
                && err.contains("HTTPS listener")
                && err.contains("proxy's HTTP listener"),
            "collision message must name 443/the HTTPS listener/the proxy HTTP port, got: {err}",
        );
        // A normal public port (the default 80, or any other) is accepted.
        assert!(validate_public_port(80).is_ok());
        assert!(validate_public_port(8080).is_ok());
    }

    #[test]
    fn public_ports_whose_slots_reach_443_are_rejected() {
        // Blue/green slots bind `public+1`/`public+2`, so 441 (green slot → 443)
        // and 442 (blue slot → 443) collide with the proxy's always-bound HTTPS
        // listener just as 443 itself does. All three are rejected unconditionally.
        let green = validate_public_port(441).expect_err("441 (green slot=443) rejected");
        assert!(
            green.contains("green app slot") && green.contains("443"),
            "441 message must name the green slot landing on 443, got: {green}",
        );
        let blue = validate_public_port(442).expect_err("442 (blue slot=443) rejected");
        assert!(
            blue.contains("blue app slot") && blue.contains("443"),
            "442 message must name the blue slot landing on 443, got: {blue}",
        );
        assert!(validate_public_port(443).is_err());
        // A safe non-{441,442,443} port is accepted — regardless of TLS, since the
        // guard is TLS-independent and TLS imposes no port requirement.
        assert!(validate_public_port(440).is_ok());
        assert!(validate_public_port(444).is_ok());
        assert!(validate_public_port(8080).is_ok());
    }

    #[test]
    fn tls_does_not_gate_the_public_port() {
        // There is NO TLS-gated port-80 requirement: per-app TLS is provisioned by
        // kamal-proxy on its always-bound 443 (TLS-ALPN-01), so a normal public
        // port is accepted whether or not `[deploy.tls]` is enabled. Only the
        // unconditional 443 collision applies.
        assert!(validate_public_port(80).is_ok());
        assert!(validate_public_port(3000).is_ok());
        assert!(validate_public_port(8080).is_ok());
    }

    #[test]
    fn redeploy_refuses_a_concurrent_server_port_change() {
        // #2073 pre-flight refuse. The installed proxy unit binds port 80; the operator
        // now redeploys with `server.port = 8080` — a concurrent public-port change the
        // durability restart-refresh (#2070) can't perform live-safely, so it is refused
        // BEFORE any op runs, naming old vs new port and the two-deploy sequence.
        let err = refuse_concurrent_public_port_change(&exec::InstalledProxyPort::Port(80), 8080)
            .expect_err("a changed server.port must be refused on the redeploy path");
        assert!(
            err.contains("80") && err.contains("8080"),
            "the refuse message must name the installed (80) and requested (8080) ports: {err}",
        );
        assert!(
            err.contains("server.port unchanged") && err.contains("#2073"),
            "the refuse message must spell out the operator sequence and reference the \
             tracking issue: {err}",
        );
    }

    #[test]
    fn redeploy_allows_an_unchanged_server_port() {
        // The common durability-upgrade path: the installed unit's `--http-port` equals
        // the requested port, so the redeploy proceeds (the restart re-execs on the SAME
        // port, no collision, derived == actual live port).
        assert!(
            refuse_concurrent_public_port_change(&exec::InstalledProxyPort::Port(80), 80).is_ok(),
            "an unchanged-port redeploy must be allowed",
        );
        // And with a non-default port that also happens to match.
        assert!(
            refuse_concurrent_public_port_change(&exec::InstalledProxyPort::Port(3000), 3000)
                .is_ok(),
            "an unchanged non-default port must also be allowed",
        );
    }

    #[test]
    fn redeploy_fails_closed_on_an_unreadable_installed_port() {
        // The installed unit is present but its `--http-port` couldn't be read/parsed,
        // so we cannot prove the public port is unchanged — refuse (fail closed) rather
        // than risk the durability restart rebinding a different port and stranding `:80`.
        let err = refuse_concurrent_public_port_change(&exec::InstalledProxyPort::Unreadable, 80)
            .expect_err("an unreadable installed proxy port must fail closed");
        assert!(
            err.contains("could not be read") && err.contains("#2073"),
            "the fail-closed message must explain the unreadable unit and reference the \
             tracking issue: {err}",
        );
    }

    #[test]
    fn redeploy_allows_when_no_proxy_unit_is_installed() {
        // No installed proxy unit at all is a first-deploy shape (the durability refresh
        // writes it fresh at the requested port) — nothing to conflict with, so the
        // refuse guard passes regardless of the requested port.
        assert!(
            refuse_concurrent_public_port_change(&exec::InstalledProxyPort::Absent, 8080).is_ok(),
            "an absent installed proxy unit must not trigger the refuse",
        );
    }

    // --- proxy-options marker refuse / preserve decision (issue #2074) --------

    #[test]
    fn redeploy_fails_closed_on_an_unreadable_proxy_options_marker() {
        // The `shared/proxy-options` marker is present but unparseable, so the OLD
        // release's TLS/host can't be proved — a concurrent `deploy.tls.host` change
        // can't be safely preserved across the durability restart. Refuse (fail closed)
        // BEFORE any op runs, with the two-deploy repair guidance and the #2074 ref.
        let err = refuse_unprovable_proxy_options(&exec::ProxyOptionsMarker::Unreadable)
            .expect_err("an unreadable proxy-options marker must fail closed");
        assert!(
            err.contains("proxy-options") && err.contains("unreadable"),
            "the fail-closed message must name the unreadable proxy-options marker: {err}",
        );
        assert!(
            err.contains("deploy.tls unchanged") && err.contains("#2074"),
            "the message must spell out the two-deploy repair and reference #2074: {err}",
        );
    }

    #[test]
    fn redeploy_allows_an_absent_proxy_options_marker() {
        // A legacy host (or first deploy) never wrote the marker. Refusing would block
        // the FIRST redeploy of every pre-existing host — the deadlock #2074 rejects —
        // so an absent marker is ALLOWED (proceed as legacy: the deploy re-registers
        // with the new config and writes the marker, self-healing the next redeploy).
        assert!(
            refuse_unprovable_proxy_options(&exec::ProxyOptionsMarker::Absent).is_ok(),
            "an absent proxy-options marker must not trigger the refuse",
        );
    }

    #[test]
    fn redeploy_allows_a_readable_proxy_options_marker() {
        // A present, parseable marker is exactly the preserve path — it never refuses,
        // whether the recorded options match the new config or not (the re-register
        // simply carries the recorded options).
        let unchanged = exec::ProxyOptionsMarker::Options(exec::ProxyServiceOptions {
            tls: true,
            host: Some("app.example.com".to_owned()),
        });
        assert!(
            refuse_unprovable_proxy_options(&unchanged).is_ok(),
            "an unchanged readable marker must be allowed (preserve is a no-op)",
        );
        let changed = exec::ProxyOptionsMarker::Options(exec::ProxyServiceOptions {
            tls: false,
            host: None,
        });
        assert!(
            refuse_unprovable_proxy_options(&changed).is_ok(),
            "a readable marker whose options differ from the new config is still allowed \
             (the old options are preserved, not refused)",
        );
    }

    #[test]
    fn resolve_fills_default_chain_from_project_name() {
        let resolved = resolved_defaults();
        assert_eq!(resolved.app_name, "myapp");
        assert_eq!(resolved.app_dir, "/srv/autumn/myapp");
        assert_eq!(resolved.service_name, "myapp");
        assert_eq!(resolved.user, "root");
        assert_eq!(resolved.ssh_port, 22);
        assert_eq!(resolved.readiness_timeout_secs, 60);
        assert_eq!(resolved.keep_releases, 3);
        assert_eq!(resolved.host, None);
        assert_eq!(resolved.profile, "prod");
        // TLS is opt-in: an absent `[deploy.tls]` table leaves it disabled.
        assert!(!resolved.tls_enabled);
        assert_eq!(resolved.tls_host, None);
    }

    #[test]
    fn tls_absent_table_leaves_tls_disabled() {
        // The default `DeployConfig` (no `[deploy.tls]`) resolves to TLS off with
        // no host — byte-for-byte the historical HTTP-only behavior.
        let resolved = resolved_defaults();
        assert!(!resolved.tls_enabled);
        assert_eq!(resolved.tls_host, None);
    }

    #[test]
    fn tls_enabled_with_host_resolves_tls_host() {
        let cfg = DeployConfig {
            tls: autumn_web::config::DeployTlsConfig {
                enabled: true,
                host: Some("app.example.com".to_owned()),
            },
            ..DeployConfig::default()
        };
        let resolved =
            ResolvedDeployConfig::resolve(&cfg, "myapp").expect("enabled + host resolves");
        assert!(resolved.tls_enabled);
        assert_eq!(resolved.tls_host.as_deref(), Some("app.example.com"));
    }

    #[test]
    fn tls_enabled_without_host_is_a_resolve_error() {
        // Enabling TLS without a hostname cannot provision a certificate — a hard
        // resolve-time error, before any remote command runs.
        for host in [None, Some(String::new()), Some("   ".to_owned())] {
            let cfg = DeployConfig {
                tls: autumn_web::config::DeployTlsConfig {
                    enabled: true,
                    host,
                },
                ..DeployConfig::default()
            };
            let err = ResolvedDeployConfig::resolve(&cfg, "myapp")
                .expect_err("enabled TLS without a host must be rejected");
            assert!(
                err.contains("[deploy.tls]") && err.contains("host"),
                "error should name the section and the missing key, got: {err}",
            );
        }
    }

    #[test]
    fn host_preparation_is_on_by_default_and_declinable() {
        // #1607 (AC-1): the documented target-host precondition is at most a stock
        // Ubuntu LTS with SSH access, so preparing the host is the DEFAULT. The
        // opt-out exists for operators who provision the proxy themselves.
        assert!(
            resolved_defaults().install_proxy,
            "a bare `[deploy]` table must prepare the host"
        );
        assert_eq!(
            provisioning(&resolved_defaults()),
            exec::ProxyProvisioning::Auto
        );

        let declined = ResolvedDeployConfig::resolve(
            &DeployConfig {
                install_proxy: false,
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("declining host prep resolves");
        assert!(!declined.install_proxy);
        assert_eq!(provisioning(&declined), exec::ProxyProvisioning::Disabled);
    }

    // ── fleet host list (#1621) ───────────────────────────────────

    /// A `[deploy]` config carrying only the fleet list, so the fleet tests read
    /// as the TOML an operator would write.
    fn fleet_cfg(hosts: &[&str]) -> DeployConfig {
        DeployConfig {
            hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
            ..DeployConfig::default()
        }
    }

    #[test]
    fn fleet_resolve_keeps_declaration_order_as_the_rollout_order() {
        // #1621 (AC-1/AC-2): the order of `[deploy] hosts` IS the rollout order —
        // it is a documented contract, not an implementation detail, so resolution
        // must never sort or dedupe-reorder it.
        let fleet = ResolvedFleet::resolve(&fleet_cfg(&["web-3", "web-1", "web-2"]), "myapp")
            .expect("a well-formed fleet resolves");
        let hosts: Vec<Option<&str>> = fleet.hosts.iter().map(|h| h.host.as_deref()).collect();
        assert_eq!(
            hosts,
            vec![Some("web-3"), Some("web-1"), Some("web-2")],
            "fleet resolution must preserve declaration order, got: {hosts:?}"
        );
        assert!(
            !fleet.is_single(),
            "a 3-host fleet must not report as single-host, got: {:?}",
            fleet.hosts.len()
        );
    }

    #[test]
    fn fleet_resolve_rejects_host_and_hosts_set_together_naming_both_keys() {
        // #1621 (AC-1): the two spellings are mutually exclusive — with both set
        // the rollout order is ambiguous, so this fails closed BEFORE any remote
        // command runs, naming both keys so the operator knows what to delete.
        let cfg = DeployConfig {
            host: Some("203.0.113.10".to_owned()),
            hosts: vec!["web-1.example.com".to_owned()],
            ..DeployConfig::default()
        };
        let err = ResolvedFleet::resolve(&cfg, "myapp")
            .expect_err("host + hosts together must be rejected");
        assert!(
            err.contains("[deploy] host") && err.contains("[deploy] hosts"),
            "the mutual-exclusion error must name BOTH keys, got: {err}"
        );
        assert!(
            err.contains("#1621"),
            "fail-closed refusals cite the tracking issue, got: {err}"
        );
    }

    #[test]
    fn fleet_resolve_rejects_a_blank_hosts_entry_naming_its_index() {
        // #1621 (AC-1): a blank entry would resolve to a hostless SSH target and
        // fail mid-rollout with hosts already cut over. The index is 0-based and
        // named so the operator can find the offending line in a long list.
        for (index, hosts) in [
            (0_usize, vec![String::new(), "web-2".to_owned()]),
            (1, vec!["web-1".to_owned(), "   ".to_owned()]),
            (
                2,
                vec!["web-1".to_owned(), "web-2".to_owned(), "\t".to_owned()],
            ),
        ] {
            let cfg = DeployConfig {
                hosts,
                ..DeployConfig::default()
            };
            let err = ResolvedFleet::resolve(&cfg, "myapp")
                .expect_err("a blank hosts entry must be rejected");
            assert!(
                err.contains("[deploy] hosts") && err.contains(&format!("{index}")),
                "the blank-entry error must name the key and the 0-based index {index}, got: {err}"
            );
        }
    }

    #[test]
    fn fleet_resolve_rejects_duplicate_hosts_after_trimming_naming_the_value() {
        // #1621 (AC-1/AC-3): deploying the same machine twice makes the second
        // pass see its OWN new release as live, ping-pongs the blue/green slots and
        // corrupts the previous-release chain a fleet rollback depends on. Trim
        // first so `"web-1"` and `" web-1 "` are recognised as the same host.
        // (Literal duplicates only — DNS aliases are a documented limitation.)
        let cfg = fleet_cfg(&["web-1.example.com", "web-2", " web-1.example.com "]);
        let err = ResolvedFleet::resolve(&cfg, "myapp")
            .expect_err("a duplicate hosts entry must be rejected");
        assert!(
            err.contains("web-1.example.com"),
            "the duplicate error must name the repeated value, got: {err}"
        );
        assert!(
            err.contains("[deploy] hosts"),
            "the duplicate error must name the config key, got: {err}"
        );
    }

    #[test]
    fn fleet_resolve_without_any_host_keeps_the_deploy_host_message() {
        // #1621 (AC-1): with neither key set the operator sees the SAME
        // missing-host text as before, EXTENDED to mention the fleet spelling. The
        // literal substring "[deploy] host" is asserted by
        // `deploy_check_fails_fast_without_host` and quoted in operator runbooks,
        // so it must survive the extension verbatim.
        let err = ResolvedFleet::resolve(&DeployConfig::default(), "myapp")
            .expect_err("no host and no hosts must be rejected");
        assert!(
            err.contains("[deploy] host"),
            "the missing-target error must keep the historical `[deploy] host` \
             substring, got: {err}"
        );
        assert!(
            err.contains("hosts"),
            "the missing-target error must also offer the fleet spelling, got: {err}"
        );

        // A blank scalar host is the same case as an absent one.
        let blank = DeployConfig {
            host: Some("   ".to_owned()),
            ..DeployConfig::default()
        };
        let blank_err = ResolvedFleet::resolve(&blank, "myapp")
            .expect_err("a blank host must be rejected like an absent one");
        assert!(
            blank_err.contains("[deploy] host"),
            "a blank host must report the missing-target message, got: {blank_err}"
        );
    }

    #[test]
    fn single_entry_hosts_resolves_to_the_same_view_as_host() {
        // #1621 (AC-1, proof P5): a one-entry `hosts` list is byte-for-byte the
        // historical single-server deploy. Value equality against
        // `ResolvedDeployConfig::resolve` of the equivalent `host` config is the
        // structural guarantee — everything below `SshTarget::from_resolved` then
        // behaves identically by construction.
        let single = DeployConfig {
            host: Some("203.0.113.10".to_owned()),
            app_name: Some("shop".to_owned()),
            user: "deploy".to_owned(),
            ssh_port: 2222,
            readiness_timeout_secs: 90,
            keep_releases: 5,
            profile: "staging".to_owned(),
            ..DeployConfig::default()
        };
        let fleet_cfg = DeployConfig {
            host: None,
            hosts: vec!["203.0.113.10".to_owned()],
            ..single.clone()
        };

        let expected =
            ResolvedDeployConfig::resolve(&single, "myapp").expect("single host resolves");
        let fleet =
            ResolvedFleet::resolve(&fleet_cfg, "myapp").expect("single-entry fleet resolves");
        assert!(
            fleet.is_single(),
            "a one-entry hosts list must report as single-host, got {} hosts",
            fleet.hosts.len()
        );
        assert_eq!(
            fleet.hosts[0], expected,
            "a one-entry `hosts` list must resolve to exactly the `host` view; \
             fleet: {:?}, single: {expected:?}",
            fleet.hosts[0]
        );
    }

    /// A minimal loaded config for the preflight graders — no DB, no migrations,
    /// no secret — so the three project graders return stable, host-independent
    /// results and the assertions below are about SCOPE, not grading.
    fn preflight_config() -> AutumnConfig {
        AutumnConfig::default()
    }

    // ── SQLite data-file persistence (issue #1909) ─────────────────────────

    fn deploy_cfg() -> ResolvedDeployConfig {
        ResolvedDeployConfig::resolve(
            &DeployConfig {
                host: Some("203.0.113.10".to_owned()),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("deploy config resolves")
    }

    #[test]
    fn classify_sqlite_data_file_separates_relative_absolute_and_not_sqlite() {
        let cfg = deploy_cfg();
        // A relative path resolves against the slot unit's WorkingDirectory (the
        // release dir), so it is the case the deploy has to relocate.
        for relative in ["sqlite://app.db", "sqlite://./app.db", "sqlite:data/app.db"] {
            assert!(
                matches!(
                    classify_sqlite_data_file(Some(relative), &cfg),
                    SqliteDataFile::Relative(_)
                ),
                "{relative}"
            );
        }
        // Spellings that name the same file normalize to one `ln -s` target.
        for spelling in ["sqlite://./app.db", "sqlite://app.db"] {
            assert_eq!(
                classify_sqlite_data_file(Some(spelling), &cfg),
                SqliteDataFile::Relative("app.db".to_owned()),
                "{spelling}"
            );
        }
        for spelling in [
            "sqlite://data/app.db",
            "sqlite://data//app.db",
            "sqlite://data/./app.db",
            "sqlite://data/nested/../app.db",
        ] {
            assert_eq!(
                classify_sqlite_data_file(Some(spelling), &cfg),
                SqliteDataFile::Relative("data/app.db".to_owned()),
                "{spelling}"
            );
        }
        // A `file:` URI is percent-decoded, because that is what SQLite opens.
        assert_eq!(
            classify_sqlite_data_file(Some("file:app%20data.db?mode=rwc"), &cfg),
            SqliteDataFile::Relative("app data.db".to_owned())
        );
        // An absolute path outside the app dir already survives a deploy.
        assert_eq!(
            classify_sqlite_data_file(Some("sqlite:///var/lib/myapp/app.db"), &cfg),
            SqliteDataFile::Persistent("/var/lib/myapp/app.db".to_owned())
        );
        // …as does one the operator put in the deploy's own persistent dir.
        assert_eq!(
            classify_sqlite_data_file(Some("sqlite:///srv/autumn/myapp/shared/data/app.db"), &cfg),
            SqliteDataFile::Persistent("/srv/autumn/myapp/shared/data/app.db".to_owned())
        );
        // Postgres and an unconfigured URL are not this grader's business.
        for other in [Some("postgres://u:p@h/app"), Some("   "), None] {
            assert_eq!(
                classify_sqlite_data_file(other, &cfg),
                SqliteDataFile::NotSqlite,
                "{other:?}"
            );
        }
    }

    /// The deploy target is always a POSIX host, so path grading must not follow
    /// the host this CLI runs on. `std::path` would: on Windows
    /// `Path::new("/srv/app.db").is_absolute()` is false, so every absolute
    /// target would be graded as a relative one and `autumn deploy check` from a
    /// Windows workstation would misjudge the Linux host it deploys to.
    #[test]
    fn path_grading_uses_posix_rules_on_every_host() {
        assert_eq!(
            lexically_normalized("/srv/autumn/myapp/shared/../releases/r1/app.db"),
            Some("/srv/autumn/myapp/releases/r1/app.db".to_owned())
        );
        assert_eq!(
            lexically_normalized("./data//nested/../app.db"),
            Some("data/app.db".to_owned())
        );
        assert_eq!(lexically_normalized("/a/../.."), None, "climbs above root");
        assert_eq!(
            lexically_normalized("../app.db"),
            Some("../app.db".to_owned())
        );
        assert_eq!(lexically_normalized("."), Some(String::new()));
        // A leading `/` means absolute here, whatever the host says.
        assert!(
            lexically_normalized("/var/lib/app.db").is_some_and(|text| text.starts_with('/')),
            "a POSIX absolute path must stay absolute"
        );
    }

    #[test]
    fn classify_sqlite_data_file_refuses_what_a_deploy_cannot_keep() {
        let cfg = deploy_cfg();
        // In-memory: gone at the next restart, deploy or no deploy.
        let memory = classify_sqlite_data_file(Some("sqlite::memory:"), &cfg);
        let SqliteDataFile::Refused(detail) = memory else {
            panic!("an in-memory database must be refused, got {memory:?}");
        };
        assert!(detail.contains("IN-MEMORY"), "{detail}");

        // Anything the deploy manages is transient: `releases/` is pruned by
        // retention, and `current` is only a symlink into it — a path through
        // `current` is inside the live release dir, whatever it looks like.
        for inside in [
            "sqlite:///srv/autumn/myapp/releases/20260714T120000Z/app.db",
            "sqlite:///srv/autumn/myapp/current/app.db",
            "sqlite:///srv/autumn/myapp/app.db",
        ] {
            let got = classify_sqlite_data_file(Some(inside), &cfg);
            let SqliteDataFile::Refused(detail) = got else {
                panic!("{inside} must be refused, got {got:?}");
            };
            assert!(
                detail.contains("/srv/autumn/myapp") && detail.contains("shared/data"),
                "the refusal must name the problem and the fix: {detail}"
            );
        }
        // …but a sibling path that merely SHARES the prefix is fine.
        assert!(matches!(
            classify_sqlite_data_file(Some("sqlite:///srv/autumn/myapp-staging/app.db"), &cfg),
            SqliteDataFile::Persistent(_)
        ));

        // `..` must be resolved BEFORE the containment check: this one reads as
        // `shared/` lexically but the kernel resolves it into `releases/`.
        let sneaky = classify_sqlite_data_file(
            Some("sqlite:///srv/autumn/myapp/shared/../releases/r1/app.db"),
            &cfg,
        );
        let SqliteDataFile::Refused(detail) = sneaky else {
            panic!("a path that resolves into releases/ must be refused, got {sneaky:?}");
        };
        assert!(
            detail.contains("/srv/autumn/myapp/releases/r1/app.db"),
            "the refusal must name the RESOLVED path: {detail}"
        );
        // The mirror image: a `..` that resolves back OUT of the app dir is fine,
        // and the normalized text is what the deploy uses.
        assert_eq!(
            classify_sqlite_data_file(
                Some("sqlite:///srv/autumn/myapp/../shared-data/app.db"),
                &cfg
            ),
            SqliteDataFile::Persistent("/srv/autumn/shared-data/app.db".to_owned())
        );
        // A path that climbs above the root is not a path.
        assert!(matches!(
            classify_sqlite_data_file(Some("sqlite:///../../app.db"), &cfg),
            SqliteDataFile::Refused(_)
        ));

        // A run of spaces mid-sentence means a `\\` continuation was dropped when
        // the message was written (same guard as `DeployError`'s own).
        for refused in [
            "sqlite::memory:",
            "sqlite:///srv/autumn/myapp/current/app.db",
            "sqlite://..",
        ] {
            if let SqliteDataFile::Refused(detail) = classify_sqlite_data_file(Some(refused), &cfg)
            {
                assert!(!detail.contains("   "), "{refused}: {detail}");
            }
        }

        // A relative path must name a FILE inside the release dir: `..` climbs out
        // of it, and a `.`-only path names the directory itself, which the link op
        // would then try to replace.
        for bad in [
            "sqlite://../../app.db",
            "sqlite://.",
            "sqlite://./",
            "sqlite://data/../..",
        ] {
            let got = classify_sqlite_data_file(Some(bad), &cfg);
            assert!(
                matches!(got, SqliteDataFile::Refused(_)),
                "{bad} must be refused, got {got:?}"
            );
        }
    }

    /// `[deploy] app_dir` can carry lexical aliases too. Comparing the raw
    /// spelling would let a database sitting in the releases dir it resolves to
    /// pass as durable, and release retention would then delete it.
    #[test]
    fn classify_sqlite_data_file_normalizes_the_app_dir_as_well() {
        let aliased = ResolvedDeployConfig::resolve(
            &DeployConfig {
                host: Some("203.0.113.10".to_owned()),
                app_dir: Some("/srv/autumn/tmp/../myapp/".to_owned()),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("resolves");

        let inside = classify_sqlite_data_file(
            Some("sqlite:///srv/autumn/myapp/releases/r1/app.db"),
            &aliased,
        );
        let SqliteDataFile::Refused(detail) = inside else {
            panic!("a database in the resolved releases dir must be refused, got {inside:?}");
        };
        assert!(
            detail.contains("/srv/autumn/myapp"),
            "the refusal must name the RESOLVED app dir: {detail}"
        );
        // The deploy's own persistent dir still passes, aliased spelling or not.
        assert!(matches!(
            classify_sqlite_data_file(
                Some("sqlite:///srv/autumn/myapp/shared/data/app.db"),
                &aliased
            ),
            SqliteDataFile::Persistent(_)
        ));
        // …and a path genuinely outside it is still the operator's own.
        assert!(matches!(
            classify_sqlite_data_file(Some("sqlite:///var/lib/myapp/app.db"), &aliased),
            SqliteDataFile::Persistent(_)
        ));
    }

    /// A deploy addresses paths as text, so one it cannot render it cannot ship.
    /// `file:app%FF.db` decodes to exactly such a name.
    #[cfg(unix)]
    #[test]
    fn classify_sqlite_data_file_refuses_a_non_utf8_path() {
        let got = classify_sqlite_data_file(Some("file:app%FF.db"), &deploy_cfg());
        let SqliteDataFile::Refused(detail) = got else {
            panic!("a non-UTF-8 path must be refused, got {got:?}");
        };
        assert!(detail.contains("not valid UTF-8"), "{detail}");
    }

    #[test]
    fn sqlite_data_persistence_grader_is_emitted_only_for_a_sqlite_app() {
        let cfg = deploy_cfg();

        // A Postgres deploy's preflight report must be byte-identical to
        // pre-#1909: no extra row at all.
        assert!(grade_sqlite_data_persistence(Some("postgres://h/app"), &cfg).is_none());
        assert!(grade_sqlite_data_persistence(None, &cfg).is_none());

        let pass = grade_sqlite_data_persistence(Some("sqlite://app.db"), &cfg)
            .expect("a SQLite app is graded");
        assert_eq!(pass.name, "sqlite_data_file");
        assert!(pass.passed, "a relative path is kept, not refused");
        assert!(
            pass.detail.contains("/srv/autumn/myapp/shared/data/app.db"),
            "the pass must name where the file is kept: {}",
            pass.detail
        );

        for refused in [
            "sqlite::memory:",
            "sqlite:///srv/autumn/myapp/current/app.db",
        ] {
            let fail =
                grade_sqlite_data_persistence(Some(refused), &cfg).expect("a SQLite app is graded");
            assert!(!fail.passed, "{refused} must fail preflight");
        }
    }

    #[test]
    fn preflight_scope_is_none_for_one_host_and_the_host_for_a_fleet() {
        // #1621 (T1.24, plan §5.2): `scope` is the additive field that lets a fleet
        // name WHICH host a per-host grader graded, without touching
        // `PreflightCheck::name` — an operator-visible `doctor --json` contract.
        //
        // It is `None` for a single-host fleet, which is what keeps `deploy check`
        // output byte-identical to pre-#1621 (AC-1, artifact 4). Do not "simplify"
        // this to always-Some.
        let config = preflight_config();

        let one = ResolvedFleet::resolve(&fleet_cfg(&["web-1.example.test"]), "myapp")
            .expect("one-host fleet resolves");
        let checks = collect_fleet_preflight(&config, &one, 1);
        assert!(
            checks.iter().all(|check| check.scope.is_none()),
            "a single-host fleet must carry no scope (AC-1): {:?}",
            checks
                .iter()
                .map(|c| (c.name, &c.scope))
                .collect::<Vec<_>>()
        );

        let many = ResolvedFleet::resolve(
            &fleet_cfg(&["web-1.example.test", "web-2.example.test"]),
            "myapp",
        )
        .expect("two-host fleet resolves");
        let checks = collect_fleet_preflight(&config, &many, 2);
        let scoped: Vec<(&str, Option<&str>)> = checks
            .iter()
            .map(|check| (check.name, check.scope.as_deref()))
            .collect();
        assert_eq!(
            scoped
                .iter()
                .filter(|(name, _)| *name == "ssh_reachability")
                .map(|(_, scope)| *scope)
                .collect::<Vec<_>>(),
            vec![Some("web-1.example.test"), Some("web-2.example.test")],
            "each per-host grader must name its host, in rollout order: {scoped:?}"
        );
        // The project-wide graders grade the PROJECT, not a host, so they stay
        // unscoped even in a fleet — scoping them would imply they were run N times.
        assert!(
            scoped
                .iter()
                .filter(|(name, _)| *name != "ssh_reachability")
                .all(|(_, scope)| scope.is_none()),
            "project-wide graders must stay unscoped: {scoped:?}"
        );
    }

    #[test]
    fn an_empty_fleet_still_fails_the_preflight_gate() {
        // #2274: `collect_fleet_preflight` is the fail-fast boundary — `run_up`,
        // `run_check`, and `run_rollback` all abort on the count it produces. Handing
        // back zero checks for a hostless fleet would report "0 failed", let the run walk
        // past that boundary, and leave the three `fleet.hosts[0]` sites downstream to
        // panic on an empty vec. `ResolvedFleet::from_targets` keeps a `None` host rather
        // than dropping the entry, so every real fleet has at least one element and this
        // arm is unreachable today — but `hosts` is a `pub` field on a `pub` struct, so
        // the invariant is a promise, not a type. The gate fails closed instead of
        // trusting it.
        let empty = ResolvedFleet { hosts: vec![] };
        let checks = collect_fleet_preflight(&preflight_config(), &empty, 0);
        assert!(
            checks
                .iter()
                .any(|check| check.blocking() && check.detail.contains(DEPLOY_HOST_MISSING_DETAIL)),
            "a fleet with no target must fail the preflight gate, not return no checks: {:?}",
            checks
                .iter()
                .map(|c| (c.name, c.passed, &c.detail))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_fleet_preflight_grader_returns_deferred() {
        // #1621 (T1.16, plan §5.3): `deploy_preflight_result` maps `deferred` →
        // `CheckStatus::Fail` while `report_preflight` treats it as a NON-blocking
        // warning, so a deferred fleet grader would mean `doctor --strict` fails a
        // config `deploy check` passes. The resolution is a rule, not a
        // reconciliation: FLEET graders pass or they fail — they never defer.
        // (`deferred` remains in use for the media graders, which are single-host by
        // construction; see the `[media]` fleet refusal.)
        let config = preflight_config();
        for hosts in [
            vec!["web-1.example.test"],
            vec!["web-1.example.test", "web-2.example.test"],
            vec![
                "web-1.example.test",
                "web-2.example.test",
                "web-3.example.test",
            ],
        ] {
            let fleet =
                ResolvedFleet::resolve(&fleet_cfg(&hosts), "myapp").expect("fleet resolves");
            for check in collect_fleet_preflight(&config, &fleet, hosts.len()) {
                assert!(
                    !check.deferred,
                    "fleet grader `{}` must never defer (scope {:?}): {}",
                    check.name, check.scope, check.detail
                );
            }
        }
    }

    #[test]
    fn preflight_report_label_appends_the_scope_only_when_present() {
        // #1621 (plan §5.2): `✅ ssh_reachability (web-2): …`. The NAME is unchanged
        // and stays the stable identifier; the host rides in parentheses after it,
        // so a single-host run (scope `None`) prints the pre-#1621 label verbatim.
        let plain = PreflightCheck::pass("ssh_reachability", "reachable");
        assert_eq!(preflight_label(&plain), "ssh_reachability");
        let scoped = plain.scoped(Some("web-2.example.test".to_owned()));
        assert_eq!(
            preflight_label(&scoped),
            "ssh_reachability (web-2.example.test)"
        );
        assert_eq!(
            scoped.name, "ssh_reachability",
            "the stable identifier must not absorb the host"
        );
    }

    #[test]
    fn doctor_deploy_check_names_are_the_shared_grader_names_prefixed() {
        // #1621 (T2.5, plan §5.5): doctor's deploy checks and `deploy check`'s
        // graders are enumerated from ONE ordered list, and doctor's operator-visible
        // `--json` names are exactly that list with a `deploy_` prefix. Deriving the
        // mapping mechanically (rather than trusting two hand-maintained literal
        // lists to stay in step) is what turns the comment-enforced parity invariant
        // into a machine-enforced one.
        assert_eq!(
            PREFLIGHT_GRADERS.len(),
            DOCTOR_PREFLIGHT_GRADERS.len(),
            "the two lists must stay the same length"
        );
        for (grader, doctor_name) in PREFLIGHT_GRADERS.iter().zip(DOCTOR_PREFLIGHT_GRADERS) {
            assert_eq!(
                doctor_name,
                format!("deploy_{grader}"),
                "doctor's name for `{grader}` must be the prefixed grader name"
            );
        }
        // …and the list is exactly what a fleet preflight emits (the per-host grader
        // once per host, then the project graders once).
        let fleet = ResolvedFleet::resolve(
            &fleet_cfg(&["web-1.example.test", "web-2.example.test"]),
            "myapp",
        )
        .expect("fleet resolves");
        let emitted: Vec<&str> = collect_fleet_preflight(&preflight_config(), &fleet, 2)
            .iter()
            .map(|check| check.name)
            .collect();
        assert_eq!(
            emitted,
            vec![
                "ssh_reachability",
                "ssh_reachability",
                "signing_secret",
                "database_url",
                "migrate_check"
            ],
            "the fleet preflight emits the per-host grader once per host, then the \
             project graders once"
        );
        let mut distinct = emitted.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let mut expected = PREFLIGHT_GRADERS.to_vec();
        expected.sort_unstable();
        assert_eq!(distinct, expected);
    }

    #[test]
    fn fleet_resolve_trims_each_host_like_the_single_host_path() {
        // #1621 (AC-1): `ResolvedDeployConfig::resolve` trims the scalar `host`,
        // so the fleet list must trim each entry identically — otherwise a stray
        // space produces a different SSH target under the two spellings.
        let padded = fleet_cfg(&["  203.0.113.10  "]);
        let fleet = ResolvedFleet::resolve(&padded, "myapp").expect("padded fleet resolves");
        let expected = ResolvedDeployConfig::resolve(
            &DeployConfig {
                host: Some("  203.0.113.10  ".to_owned()),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("padded single host resolves");
        assert_eq!(
            fleet.hosts[0], expected,
            "each fleet entry must be trimmed exactly like the scalar host, got: {:?}",
            fleet.hosts[0]
        );
    }

    #[test]
    fn fleet_resolve_shares_the_single_host_defaults_and_tls_validation() {
        // #1621 (AC-1): the fleet resolves the SHARED shape once through
        // `ResolvedDeployConfig::resolve`, so the app_name → app_dir →
        // service_name chain and the TLS-requires-host rejection are literally the
        // same code — they cannot drift between the two spellings.
        let fleet = ResolvedFleet::resolve(&fleet_cfg(&["web-1", "web-2"]), "myapp")
            .expect("a well-formed fleet resolves");
        for host in &fleet.hosts {
            assert_eq!(host.app_name, "myapp", "got: {host:?}");
            assert_eq!(host.app_dir, "/srv/autumn/myapp", "got: {host:?}");
            assert_eq!(host.service_name, "myapp", "got: {host:?}");
            assert_eq!(host.profile, "prod", "got: {host:?}");
        }

        // `[deploy.tls] enabled` without a host is rejected for a fleet exactly as
        // it is for a single server — before any host is touched.
        let cfg = DeployConfig {
            hosts: vec!["web-1".to_owned(), "web-2".to_owned()],
            tls: autumn_web::config::DeployTlsConfig {
                enabled: true,
                host: None,
            },
            ..DeployConfig::default()
        };
        let err = ResolvedFleet::resolve(&cfg, "myapp")
            .expect_err("enabled TLS without a host must be rejected for a fleet too");
        assert!(
            err.contains("[deploy.tls]") && err.contains("host"),
            "the fleet must reuse the single-host TLS rejection, got: {err}"
        );
    }

    #[test]
    fn fleet_of_one_renders_todays_unit() {
        // #1621 (AC-1, proof P7): the rendered systemd unit — the artifact that
        // actually lands on the server — must be byte-identical under both
        // spellings. This is the observable downstream of P5.
        let single = DeployConfig {
            host: Some("203.0.113.10".to_owned()),
            app_name: Some("shop".to_owned()),
            user: "deploy".to_owned(),
            ..DeployConfig::default()
        };
        let fleet_spelling = DeployConfig {
            host: None,
            hosts: vec!["203.0.113.10".to_owned()],
            ..single.clone()
        };

        let today = ResolvedDeployConfig::resolve(&single, "shop").expect("single host resolves");
        let fleet =
            ResolvedFleet::resolve(&fleet_spelling, "shop").expect("single-entry fleet resolves");

        let release_dir = "/srv/autumn/shop/releases/20240101120000";
        let today_unit = render_app_unit(&today, release_dir, 3001, "blue");
        let fleet_unit = render_app_unit(&fleet.hosts[0], release_dir, 3001, "blue");
        assert_eq!(
            fleet_unit, today_unit,
            "a one-entry fleet must render today's slot unit verbatim;\nfleet:\n{fleet_unit}\ntoday:\n{today_unit}"
        );

        // The `current`-symlink renderer stays in lockstep too.
        let today_service = render_systemd_unit(&today);
        let fleet_service = render_systemd_unit(&fleet.hosts[0]);
        assert_eq!(
            fleet_service, today_service,
            "a one-entry fleet must render today's service unit verbatim;\nfleet:\n{fleet_service}\ntoday:\n{today_service}"
        );
    }

    #[test]
    fn build_env_file_sets_production_profile_by_default() {
        // With no `[deploy] profile` set, the host env file must pin the app to
        // the production profile so the deployed service never silently boots
        // under the `dev` fallback.
        let config = AutumnConfig::default();
        let resolved = ResolvedDeployConfig::resolve(&DeployConfig::default(), "myapp")
            .expect("deploy config resolves");
        let env_file = build_env_file(&config, &resolved);
        assert!(
            env_file.expose().lines().any(|l| l == "AUTUMN_ENV=prod"),
            "env file should pin AUTUMN_ENV=prod by default, got: {:?}",
            env_file.expose()
        );
    }

    #[test]
    fn deploy_status_reads_its_probe_port_under_the_deploy_profile_without_validating() {
        // #1621 review round 1 (Codex 3). `deploy status` is the read-only command an
        // operator reaches for mid-incident, and it needs exactly one value from the
        // application config: `server.port`. Routing that through `load_runtime_config`
        // validated the whole config under the deploy profile, so an unrelated invalid
        // production setting aborted the command before a single host was probed. The
        // port must still come from the same profile resolution the deploy itself uses,
        // or status reports against the wrong port, so the fallback layers base ←
        // `[profile.prod]` ← `autumn-prod.toml` ← env, exactly like the loader.
        use autumn_web::config::MockEnv;

        let dir = tempfile::TempDir::new().expect("temp project dir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\n\
             port = 8080\n\
             \n\
             [profile.prod.server]\n\
             port = 9090\n",
        )
        .expect("write autumn.toml");
        let dirs = vec![dir.path().to_path_buf()];

        // The PROFILE's port wins over the base one — a status graded against 8080
        // would derive the wrong slot ports and report a bogus proxy-port mismatch.
        assert_eq!(
            declared_server_port_in(&dirs, "prod", &MockEnv::new()),
            Some(9090),
        );
        // …and the alias spelling resolves the same file/section set.
        assert_eq!(
            declared_server_port_in(&dirs, "production", &MockEnv::new()),
            Some(9090),
        );
        // A different profile falls through to the base declaration.
        assert_eq!(
            declared_server_port_in(&dirs, "staging", &MockEnv::new()),
            Some(8080),
        );
        // `AUTUMN_SERVER__PORT` is the loader's highest layer, so it wins here too.
        assert_eq!(
            declared_server_port_in(
                &dirs,
                "prod",
                &MockEnv::new().with("AUTUMN_SERVER__PORT", "7070")
            ),
            Some(7070),
        );
        // The profile override FILE layers on top of the inline section, matching
        // `AutumnConfig::load_with_env`'s merge order.
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[server]\nport = 9191\n",
        )
        .expect("write autumn-prod.toml");
        assert_eq!(
            declared_server_port_in(&dirs, "prod", &MockEnv::new()),
            Some(9191),
        );
        // Nothing declared anywhere → `None`, so the caller substitutes the
        // framework default instead of this function inventing one.
        let empty = tempfile::TempDir::new().expect("temp dir");
        assert_eq!(
            declared_server_port_in(&[empty.path().to_path_buf()], "prod", &MockEnv::new()),
            None,
        );
    }

    #[test]
    fn deploy_status_degrades_instead_of_aborting_on_an_unrelated_config_error() {
        // #1621 review round 1 (Codex 3), the seam. A config error under the deploy
        // profile must NOT take the read-only fleet report offline; it must be
        // reported, with the probe still graded against the profile's declared port.
        let dir = tempfile::TempDir::new().expect("temp project dir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nport = 8080\n\n[profile.prod.server]\nport = 9090\n",
        )
        .expect("write autumn.toml");
        let dirs = vec![dir.path().to_path_buf()];

        let degraded = status_public_port_with(
            &dirs,
            "prod",
            Err(DeployError::Config(
                "scheduler.max_concurrency must be greater than 0".to_owned(),
            )),
        )
        .expect("a read-only status must not abort on an unrelated config error");
        assert_eq!(degraded.port, 9090, "the PROFILE's declared port is used");
        assert!(
            degraded
                .degraded
                .as_deref()
                .is_some_and(|reason| reason.contains("scheduler.max_concurrency")),
            "the degradation must be reported, never silent: {:?}",
            degraded.degraded
        );

        // The happy path is unchanged: a config that validates supplies the port and
        // reports no caveat at all.
        let ok = status_public_port_with(
            &dirs,
            "prod",
            Ok(AutumnConfig {
                server: autumn_web::config::ServerConfig {
                    port: 4321,
                    ..autumn_web::config::ServerConfig::default()
                },
                ..AutumnConfig::default()
            }),
        )
        .expect("a valid config resolves");
        assert_eq!(
            ok,
            StatusPort {
                port: 4321,
                degraded: None,
            },
        );
    }

    #[test]
    fn build_env_file_honors_deploy_profile_override() {
        // A non-prod target (e.g. staging) sets `[deploy] profile` and the host
        // env file carries that value verbatim.
        let config = AutumnConfig::default();
        let cfg = DeployConfig {
            profile: "staging".to_owned(),
            ..DeployConfig::default()
        };
        let resolved =
            ResolvedDeployConfig::resolve(&cfg, "myapp").expect("deploy config resolves");
        let env_file = build_env_file(&config, &resolved);
        assert!(
            env_file.expose().lines().any(|l| l == "AUTUMN_ENV=staging"),
            "env file should honor the [deploy] profile override, got: {:?}",
            env_file.expose()
        );
    }

    #[test]
    fn runtime_config_loads_profile_scoped_prod_values_for_env_file_and_grading() {
        // Regression (P1 follow-up to #1956): `autumn deploy` must reload its config
        // under the target deploy profile (default `prod`), so a signing secret and DB
        // URL that live only under `[profile.prod]` are loaded, graded, and uploaded
        // rather than the operator's ambient dev values. This exercises the exact seam
        // `run` → `load_runtime_config` uses: a `ForcedProfileEnv`, which reports
        // `AUTUMN_ENV=<deploy profile>`, fed to `AutumnConfig::load_with_env`, so the
        // loader layers `[profile.prod]` on top. `AUTUMN_MANIFEST_DIR` points config
        // loading at the temp project without mutating the process env or CWD.
        use autumn_web::config::MockEnv;

        let dir = tempfile::TempDir::new().expect("temp project dir");
        // Base/dev values are weak/placeholder and DIFFERENT from prod. The prod
        // signing secret (64 hex chars, not a demo value) and prod DB URL live
        // ONLY under `[profile.prod]`, exactly the combo that motivated the fix.
        let prod_secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let prod_db_url = "postgres://prod_user:prod_pw@proddb.internal/app";
        std::fs::write(
            dir.path().join("autumn.toml"),
            format!(
                "[deploy]\n\
                 host = \"deploy.example.test\"\n\
                 \n\
                 [database]\n\
                 primary_url = \"postgres://dev:dev@localhost/devapp\"\n\
                 \n\
                 [security.signing_secret]\n\
                 secret = \"changeme\"\n\
                 \n\
                 [profile.prod.database]\n\
                 primary_url = \"{prod_db_url}\"\n\
                 \n\
                 [profile.prod.security.signing_secret]\n\
                 secret = \"{prod_secret}\"\n"
            ),
        )
        .expect("write autumn.toml");

        // Deploy profile defaults to `prod`; no host so the ssh_reachability
        // grader fails fast without any network I/O (we only assert on the
        // signing-secret grade below).
        let resolved =
            ResolvedDeployConfig::resolve(&DeployConfig::default(), "demoapp").expect("resolves");
        assert_eq!(resolved.profile, "prod");

        // Force `AUTUMN_ENV=prod` (as `load_runtime_config` does) and point config
        // loading at the temp dir via a MockEnv-supplied AUTUMN_MANIFEST_DIR.
        let forced = ForcedProfileEnv {
            profile: resolved.profile.clone(),
            inner: MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap()),
        };
        let runtime_config =
            AutumnConfig::load_with_env(&forced).expect("load config under prod profile");

        // The env file the deploy would upload carries the PROD values and pins
        // AUTUMN_ENV to the deploy profile — never the dev/base placeholders.
        let env_file = build_env_file(&runtime_config, &resolved);
        let body = env_file.expose();
        assert!(
            body.lines().any(|l| l == "AUTUMN_ENV=prod"),
            "env file must pin AUTUMN_ENV to the deploy profile, got: {body:?}"
        );
        assert!(
            body.lines()
                .any(|l| l == format!("AUTUMN_SECURITY__SIGNING_SECRET={prod_secret}")),
            "env file must carry the PROD signing secret, got: {body:?}"
        );
        assert!(
            body.lines()
                .any(|l| l == format!("AUTUMN_DATABASE__URL={prod_db_url}")),
            "env file must carry the PROD database URL, got: {body:?}"
        );
        // The dev/base placeholders must NOT leak into the uploaded env file.
        assert!(
            !body.contains("changeme"),
            "env file must not ship the dev/base signing secret"
        );
        assert!(
            !body.contains("devapp"),
            "env file must not ship the dev/base database URL"
        );

        // Preflight grades the PROD signing secret: a strong prod secret PASSES
        // even though the weak base `changeme` would fail production grading. This
        // proves the weak base secret never leaks through the grade.
        let signing = collect_preflight(&runtime_config, &resolved)
            .into_iter()
            .find(|c| c.name == "signing_secret")
            .expect("preflight includes a signing_secret check");
        assert!(
            signing.passed,
            "the strong PROD signing secret must pass preflight: {}",
            signing.detail
        );
    }

    #[test]
    fn forced_profile_env_overrides_only_autumn_env() {
        // The wrapper reports the forced deploy profile for AUTUMN_ENV and
        // delegates every other key to the inner env unchanged.
        use autumn_web::config::MockEnv;

        let forced = ForcedProfileEnv {
            profile: "prod".to_owned(),
            inner: MockEnv::new()
                .with("AUTUMN_ENV", "dev")
                .with("SOME_OTHER_KEY", "kept"),
        };
        assert_eq!(forced.var("AUTUMN_ENV").as_deref(), Ok("prod"));
        assert_eq!(forced.var("SOME_OTHER_KEY").as_deref(), Ok("kept"));
        assert!(forced.var("UNSET_KEY").is_err());
    }

    #[test]
    fn forced_profile_env_honors_explicit_autumn_dotenv() {
        // The wrapper synthesizes `AUTUMN_DOTENV=1` only when the inner env has
        // NO explicit value. An operator running `AUTUMN_DOTENV=0 autumn deploy`
        // (or `false`) is deliberately opting OUT of `.env.<profile>` loading, so
        // the explicit value must be delegated through unchanged (`should_load`
        // honors `0`/`false` as the documented off switch).
        use autumn_web::config::MockEnv;

        // Explicit `0` is preserved (off).
        let off = ForcedProfileEnv {
            profile: "prod".to_owned(),
            inner: MockEnv::new().with("AUTUMN_DOTENV", "0"),
        };
        assert_eq!(off.var("AUTUMN_DOTENV").as_deref(), Ok("0"));
        // AUTUMN_ENV is still forced to the deploy profile.
        assert_eq!(off.var("AUTUMN_ENV").as_deref(), Ok("prod"));

        // Unset inner value → synthesize `1` (opt the deploy profile into the
        // `.env.<profile>` overlay).
        let unset = ForcedProfileEnv {
            profile: "prod".to_owned(),
            inner: MockEnv::new(),
        };
        assert_eq!(unset.var("AUTUMN_DOTENV").as_deref(), Ok("1"));

        // Explicit truthy values are also delegated verbatim.
        let on = ForcedProfileEnv {
            profile: "prod".to_owned(),
            inner: MockEnv::new().with("AUTUMN_DOTENV", "true"),
        };
        assert_eq!(on.var("AUTUMN_DOTENV").as_deref(), Ok("true"));
    }

    #[test]
    fn preflight_grades_signing_secret_against_resolved_deploy_profile() {
        // Regression: preflight must grade the signing secret against the SAME
        // profile that `build_env_file` writes to the host env file
        // (`AUTUMN_ENV=<resolved.profile>`, default `prod`), NOT the local CLI
        // runtime profile (`config.profile`, dev/None on a dev box). Otherwise a
        // weak/demo secret sails through `deploy check`/`deploy up` locally, but
        // the uploaded unit boots under `prod` and exits in
        // `fail_fast_on_invalid_signing_secret` — failing the deploy only AFTER
        // touching the host. The fix catches the weak secret locally.
        let find_signing = |checks: &[PreflightCheck]| {
            checks
                .iter()
                .find(|c| c.name == "signing_secret")
                .expect("preflight includes a signing_secret check")
                .clone()
        };

        // A weak/demo secret on a dev box: `config.profile` is None (non-prod),
        // so the OLD behavior (grading against `config.profile`) would PASS.
        let mut config = AutumnConfig::default();
        config.security.signing_secret.secret = Some("changeme".to_owned());
        assert!(
            !is_production_profile(config.profile.as_deref()),
            "guard: the local runtime profile is non-production in this test"
        );

        // Default deploy profile is `prod` (what the env file will pin), so the
        // weak secret must FAIL preflight locally, before any remote call.
        let prod_resolved =
            ResolvedDeployConfig::resolve(&DeployConfig::default(), "myapp").expect("resolves");
        assert_eq!(prod_resolved.profile, "prod");
        let prod_check = find_signing(&collect_preflight(&config, &prod_resolved));
        assert!(
            !prod_check.passed,
            "weak secret must fail preflight under the default (prod) deploy \
             profile: {}",
            prod_check.detail
        );
        // Never echo the secret value, even a known demo one.
        assert!(!prod_check.detail.contains("changeme"));

        // With a non-production deploy profile (e.g. staging) the same weak
        // secret still passes — a dev/staging deploy is not held to the
        // production strength rules, and the env file pins that same profile.
        let staging_resolved = ResolvedDeployConfig::resolve(
            &DeployConfig {
                profile: "staging".to_owned(),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("deploy config resolves");
        let staging_check = find_signing(&collect_preflight(&config, &staging_resolved));
        assert!(
            staging_check.passed,
            "weak secret should pass preflight under a non-production deploy \
             profile: {}",
            staging_check.detail
        );
    }

    #[test]
    fn resolve_preserves_raw_profile_alias_while_grading_normalized() {
        // Regression: the app's runtime resolver
        // (`autumn_web::config::normalize_profile_name`) trims and case-folds profile
        // aliases, so `PROD`, `Production`, and ` production ` all boot under the
        // canonical `prod`. But the host's override-file lookup
        // (`profile_override_file_lookup_names`) keys off the raw selector spelling: it
        // prefers `autumn-production.toml` over `autumn-prod.toml` when the raw input
        // was `production`. So `resolved.profile`, and the `AUTUMN_ENV` value written
        // from it, must preserve the operator's trimmed raw spelling rather than the
        // alias-folded form, or override-file precedence flips on hosts carrying both
        // files. Grading is normalized separately, at the grade site, so a weak secret
        // under any alias still fails preflight locally.
        let find_signing = |checks: &[PreflightCheck]| {
            checks
                .iter()
                .find(|c| c.name == "signing_secret")
                .expect("preflight includes a signing_secret check")
                .clone()
        };

        let mut config = AutumnConfig::default();
        config.security.signing_secret.secret = Some("changeme".to_owned());

        // Non-canonical production spellings preserve the operator's trimmed raw
        // spelling in `resolved.profile` / AUTUMN_ENV, yet still grade as
        // production (canonicalize-at-grade) and thus FAIL the weak secret.
        for (raw, expected) in [
            ("PROD", "PROD"),
            ("Production", "Production"),
            (" production ", "production"),
        ] {
            let resolved = ResolvedDeployConfig::resolve(
                &DeployConfig {
                    profile: raw.to_owned(),
                    ..DeployConfig::default()
                },
                "myapp",
            )
            .expect("deploy config resolves");
            assert_eq!(
                resolved.profile, expected,
                "profile {raw:?} should preserve the trimmed raw spelling"
            );
            // The value written to AUTUMN_ENV is that same trimmed raw spelling,
            // so the host's override-file precedence is preserved.
            let expected_env = format!("AUTUMN_ENV={expected}");
            assert!(
                build_env_file(&config, &resolved)
                    .expose()
                    .lines()
                    .any(|l| l == expected_env),
                "env file should pin {expected_env:?} for raw profile {raw:?}"
            );
            // Grading normalizes the raw spelling, so it still grades production.
            assert!(
                is_production_profile(Some(&canonicalize_deploy_profile(&resolved.profile))),
                "normalized profile must grade as production for raw profile {raw:?}"
            );
            let check = find_signing(&collect_preflight(&config, &resolved));
            assert!(
                !check.passed,
                "weak secret must fail preflight for production spelling {raw:?}: {}",
                check.detail
            );
            assert!(!check.detail.contains("changeme"));
        }

        // A custom profile is preserved verbatim and grades non-production, so
        // the same weak secret passes (custom deploys aren't held to prod rules).
        let staging = ResolvedDeployConfig::resolve(
            &DeployConfig {
                profile: "staging".to_owned(),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("deploy config resolves");
        assert_eq!(staging.profile, "staging");
        assert!(!is_production_profile(Some(&staging.profile)));
        let staging_check = find_signing(&collect_preflight(&config, &staging));
        assert!(
            staging_check.passed,
            "weak secret should pass preflight under the custom `staging` profile: {}",
            staging_check.detail
        );
    }

    #[test]
    fn resolve_honors_explicit_overrides() {
        let cfg = DeployConfig {
            host: Some("203.0.113.10".to_owned()),
            app_name: Some("web".to_owned()),
            app_dir: Some("/opt/web".to_owned()),
            service_name: Some("web-svc".to_owned()),
            ..DeployConfig::default()
        };
        let resolved =
            ResolvedDeployConfig::resolve(&cfg, "ignored").expect("deploy config resolves");
        assert_eq!(resolved.app_name, "web");
        assert_eq!(resolved.app_dir, "/opt/web");
        assert_eq!(resolved.service_name, "web-svc");
        assert_eq!(resolved.host.as_deref(), Some("203.0.113.10"));
    }

    #[test]
    fn resolve_blank_overrides_fall_back_to_defaults() {
        let cfg = DeployConfig {
            app_name: Some("   ".to_owned()),
            app_dir: Some(String::new()),
            ..DeployConfig::default()
        };
        let resolved =
            ResolvedDeployConfig::resolve(&cfg, "fallback").expect("deploy config resolves");
        assert_eq!(resolved.app_name, "fallback");
        assert_eq!(resolved.app_dir, "/srv/autumn/fallback");
    }

    #[test]
    fn systemd_unit_contains_service_paths_and_env_file() {
        let cfg = ResolvedDeployConfig::resolve(
            &DeployConfig {
                app_name: Some("shop".to_owned()),
                service_name: Some("shop-web".to_owned()),
                user: "deploy".to_owned(),
                ..DeployConfig::default()
            },
            "shop",
        )
        .expect("deploy config resolves");
        let unit = render_systemd_unit(&cfg);
        assert!(unit.contains("Description=Autumn application: shop"));
        assert!(unit.contains("User=deploy"));
        assert!(unit.contains("WorkingDirectory=/srv/autumn/shop/current"));
        // The unit execs the uploaded standalone app binary at the `current`
        // symlink directly — never `autumn serve --release` (which would rebuild
        // from source rather than run the pre-built release binary).
        assert!(unit.contains("ExecStart=/srv/autumn/shop/current/shop"));
        assert!(!unit.contains("autumn serve --release"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        // Secrets come from an EnvironmentFile, never inlined into the unit.
        assert!(unit.contains("EnvironmentFile=/srv/autumn/shop/shared/autumn.env"));
        assert!(!unit.contains("Environment=SECRET"));
        // #1952: `render_systemd_unit` execs the `current` symlink, so it points
        // config loading at `current` (matching its WorkingDirectory), where the
        // active release's uploaded autumn.toml is reachable.
        assert!(unit.contains("Environment=AUTUMN_MANIFEST_DIR=/srv/autumn/shop/current"));
    }

    #[test]
    fn app_unit_points_the_maintenance_flag_at_the_shared_dir() {
        // #1621 R1, the deliberate cross-cutting register entry: the runtime's
        // maintenance flag path is cwd-relative and the slot unit's WorkingDirectory is
        // the release dir, so before this line a cutover orphaned the flag and silently
        // un-maintained the host. Pointing the override at the per-app `shared/` dir —
        // which `prepare-dirs` already creates, which survives cutovers, rollbacks, and
        // pruning, and which both slots see — is what makes `autumn deploy maintenance
        // on` mean anything. The line is added to both host spellings equally, since it
        // comes from `render_app_unit`, which the fleet driver and the single-host path
        // share, so the AC-1 differential invariant is untouched.
        let cfg = ResolvedDeployConfig::resolve(
            &DeployConfig {
                app_name: Some("shop".to_owned()),
                ..DeployConfig::default()
            },
            "shop",
        )
        .expect("deploy config resolves");
        let unit = render_app_unit(&cfg, "/srv/autumn/shop/releases/r1", 3001, "blue");
        assert!(
            unit.contains(
                "Environment=AUTUMN_MAINTENANCE_FLAG_FILE=/srv/autumn/shop/shared/autumn-maintenance.json"
            ),
            "slot unit must point the maintenance flag at the shared dir: {unit}"
        );
        // It must NOT land under the release dir — that is the defect being fixed.
        assert!(
            !unit.contains("AUTUMN_MAINTENANCE_FLAG_FILE=/srv/autumn/shop/releases"),
            "the maintenance flag must not be release-scoped: {unit}"
        );
        // The path the unit stamps IS the one the fan-out writes to.
        assert!(unit.contains(&format!(
            "Environment=AUTUMN_MAINTENANCE_FLAG_FILE={}",
            cfg.maintenance_flag_file()
        )));
        // …and it uses the framework's own env-var name and file name, never a
        // CLI-local re-declaration.
        assert!(unit.contains(autumn_web::maintenance::MAINTENANCE_FLAG_FILE_ENV));
        assert!(
            cfg.maintenance_flag_file()
                .ends_with(autumn_web::maintenance::MAINTENANCE_FLAG_BASENAME)
        );
    }

    #[test]
    fn app_unit_sets_manifest_dir_to_release_for_uploaded_config() {
        // #1952: the deployed slot unit (the one actually written by a deploy)
        // sets AUTUMN_MANIFEST_DIR to THIS release dir — where the manifest is
        // uploaded, coupled to the binary — so the app's config loader reads the
        // uploaded autumn.toml instead of built-in defaults, and a rollback that
        // re-renders the unit from the target release dir reads that release's OWN
        // manifest. It matches the unit's WorkingDirectory. Non-secret → an
        // `Environment=` line, not the 0600 env file.
        let cfg = ResolvedDeployConfig::resolve(
            &DeployConfig {
                app_name: Some("shop".to_owned()),
                ..DeployConfig::default()
            },
            "shop",
        )
        .expect("deploy config resolves");
        let release_dir = "/srv/autumn/shop/releases/r1";
        let unit = render_app_unit(&cfg, release_dir, 3001, "blue");
        assert!(
            unit.contains(&format!("Environment=AUTUMN_MANIFEST_DIR={release_dir}")),
            "slot unit must set AUTUMN_MANIFEST_DIR to the release dir: {unit}"
        );
        // It matches the unit's WorkingDirectory (both pinned to the release dir).
        assert!(unit.contains(&format!("WorkingDirectory={release_dir}")));
        // The env file (secrets) still lives in the shared dir at 0600.
        assert!(unit.contains("EnvironmentFile=/srv/autumn/shop/shared/autumn.env"));
    }

    #[test]
    fn render_systemd_unit_sets_manifest_dir_to_current_symlink() {
        // #1952: `autumn deploy render` (the `render_systemd_unit` renderer) execs
        // the `current` symlink, so its AUTUMN_MANIFEST_DIR must point at `current`
        // (matching its WorkingDirectory) — where the active release's manifest is
        // reachable — keeping the two renderers in lockstep.
        let cfg = ResolvedDeployConfig::resolve(
            &DeployConfig {
                app_name: Some("shop".to_owned()),
                ..DeployConfig::default()
            },
            "shop",
        )
        .expect("deploy config resolves");
        let unit = render_systemd_unit(&cfg);
        let current = cfg.current_symlink();
        assert!(
            unit.contains(&format!("Environment=AUTUMN_MANIFEST_DIR={current}")),
            "render_systemd_unit must set AUTUMN_MANIFEST_DIR to the current symlink: {unit}"
        );
        assert!(unit.contains(&format!("WorkingDirectory={current}")));
    }

    #[test]
    fn manifest_uploads_base_only_when_no_profile_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\nport = 8080\n")
            .expect("write autumn.toml");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "prod");
        assert_eq!(uploads.len(), 1, "only autumn.toml is present");
        assert_eq!(uploads[0].remote_basename, "autumn.toml");
        assert_eq!(uploads[0].local, dir.path().join("autumn.toml"));
    }

    #[test]
    fn manifest_uploads_include_the_configured_capacity_contract() {
        // A release that ships `autumn.toml` naming a `capacity_contract` but
        // not the contract itself would silently disable the admission control
        // it advertises: the remote load fails and deliberately falls back to
        // unlimited (#1733).
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\ncapacity_contract = \"capacity.lock\"\n",
        )
        .expect("write base");
        std::fs::write(dir.path().join("capacity.lock"), "version = 1\n").expect("write contract");
        let dirs = vec![dir.path().to_path_buf()];

        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "capacity.lock"]);
    }

    #[test]
    fn manifest_uploads_skip_a_capacity_contract_that_is_not_configured() {
        // A stray capacity.lock nobody points at is not part of the release.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("capacity.lock"), "version = 1\n").expect("write contract");
        let dirs = vec![dir.path().to_path_buf()];

        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml"]);
    }

    #[test]
    fn manifest_uploads_read_the_contract_from_an_inline_profile_overlay() {
        // A contract configured only under `[profile.prod.server]` is still
        // resolved by the deployed runtime, so it must still travel with the
        // release — otherwise admission control falls open with no signal.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\n\n[profile.prod.server]\ncapacity_contract = \"capacity.lock\"\n",
        )
        .expect("write base");
        std::fs::write(dir.path().join("capacity.lock"), "version = 1\n").expect("write contract");
        let dirs = vec![dir.path().to_path_buf()];

        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "capacity.lock"]);
    }

    #[test]
    fn manifest_uploads_read_the_contract_from_a_profile_only_manifest() {
        // A deployment may ship only `autumn-prod.toml`. The runtime loads it
        // regardless, and `manifest_uploads_in` uploads it — so the contract it
        // names has to travel too.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[server]\ncapacity_contract = \"capacity.lock\"\n",
        )
        .expect("write sibling");
        std::fs::write(dir.path().join("capacity.lock"), "version = 1\n").expect("write contract");
        let dirs = vec![dir.path().to_path_buf()];

        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn-prod.toml", "capacity.lock"]);
    }

    #[test]
    fn manifest_uploads_refuse_to_flatten_a_nested_contract_path() {
        // The release directory is flat, but the uploaded config still names
        // `contracts/prod.lock`. Uploading it as `prod.lock` would leave the
        // app looking at a path that does not exist — the very fall-open this
        // upload exists to prevent — so it is skipped with a warning instead.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\ncapacity_contract = \"contracts/prod.lock\"\n",
        )
        .expect("write base");
        std::fs::create_dir_all(dir.path().join("contracts")).expect("mkdir");
        std::fs::write(dir.path().join("contracts/prod.lock"), "version = 1\n")
            .expect("write contract");
        let dirs = vec![dir.path().to_path_buf()];

        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml"]);
    }

    #[test]
    fn manifest_uploads_honour_a_profile_sibling_clearing_the_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\ncapacity_contract = \"capacity.lock\"\n",
        )
        .expect("write base");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[server]\ncapacity_contract = \"\"\n",
        )
        .expect("write sibling");
        std::fs::write(dir.path().join("capacity.lock"), "version = 1\n").expect("write contract");
        let dirs = vec![dir.path().to_path_buf()];

        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "autumn-prod.toml"]);
    }

    #[test]
    fn manifest_uploads_include_profile_sibling_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-prod.toml"), "[server]\n").expect("write sibling");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "autumn-prod.toml"]);
    }

    #[test]
    fn manifest_uploads_normalize_production_to_canonical_sibling() {
        // `[deploy] profile = "Production"` must resolve the SAME override file
        // the host runtime loads first. The runtime's ordered lookup for a raw
        // `Production` is ["production", "prod"], so a local `autumn-production.toml`
        // is uploaded under that exact name — never `autumn-Production.toml`.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-production.toml"), "[server]\n")
            .expect("write sibling");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "Production");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "autumn-production.toml"]);
        assert!(
            !uploads
                .iter()
                .any(|u| u.remote_basename == "autumn-Production.toml"),
            "must not upload the raw-cased spelling"
        );
    }

    #[test]
    fn manifest_uploads_check_canonical_profile_sibling() {
        // `[deploy] profile = "production"` should still ship the canonical
        // `autumn-prod.toml` if that is what the repo carries, matching the
        // runtime's own override-file lookup (production → ["production","prod"];
        // production absent, prod present → prod wins).
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-prod.toml"), "[server]\n").expect("write sibling");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "production");
        assert!(
            uploads
                .iter()
                .any(|u| u.remote_basename == "autumn-prod.toml"),
            "canonical prod sibling is uploaded for raw profile `production`"
        );
    }

    #[test]
    fn manifest_uploads_raw_prod_prefers_prod_sibling_first() {
        // Raw `prod` → ordered lookup ["prod","production"], so when only the
        // canonical `autumn-prod.toml` exists it is chosen.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-prod.toml"), "[server]\n").expect("write sibling");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "autumn-prod.toml"]);
    }

    #[test]
    fn manifest_uploads_custom_profile_uses_verbatim_sibling() {
        // A custom profile has no aliases: raw `staging` → ["staging"], uploaded
        // as `autumn-staging.toml`.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-staging.toml"), "[server]\n")
            .expect("write sibling");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "staging");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "autumn-staging.toml"]);
    }

    #[test]
    fn manifest_uploads_first_lookup_name_wins_when_both_present() {
        // When BOTH `autumn-production.toml` and `autumn-prod.toml` exist, the one
        // matching the runtime's FIRST lookup name for the raw spelling is chosen
        // (first-existing-wins), so the deploy uploads exactly what the host loads.
        // Raw `production` → ["production","prod"] → production wins.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-production.toml"), "[server]\n")
            .expect("write production");
        std::fs::write(dir.path().join("autumn-prod.toml"), "[server]\n").expect("write prod");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "production");
        let siblings: Vec<&str> = uploads
            .iter()
            .map(|u| u.remote_basename.as_str())
            .filter(|n| n.starts_with("autumn-"))
            .collect();
        assert_eq!(
            siblings,
            vec!["autumn-production.toml"],
            "only the first-lookup-name sibling is uploaded, matching the runtime"
        );
    }

    #[test]
    fn manifest_uploads_base_falls_back_to_cwd_dir() {
        // Mirror the runtime's find_config_file_named per-file fallback: when the
        // primary (manifest) dir has no autumn.toml but a fallback (CWD) dir does,
        // the base manifest is picked up from the fallback dir. The sibling honors
        // the same fallback.
        let manifest_dir = tempfile::tempdir().expect("manifest dir");
        let cwd = tempfile::tempdir().expect("cwd dir");
        // Manifest dir is empty; CWD carries the real config.
        std::fs::write(cwd.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(cwd.path().join("autumn-prod.toml"), "[server]\n").expect("write sibling");
        let dirs = vec![manifest_dir.path().to_path_buf(), cwd.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "prod");
        assert_eq!(
            uploads.len(),
            2,
            "base + sibling picked up from the CWD fallback"
        );
        assert_eq!(uploads[0].remote_basename, "autumn.toml");
        assert_eq!(uploads[0].local, cwd.path().join("autumn.toml"));
        assert_eq!(uploads[1].remote_basename, "autumn-prod.toml");
        assert_eq!(uploads[1].local, cwd.path().join("autumn-prod.toml"));
    }

    #[test]
    fn manifest_uploads_primary_dir_wins_over_fallback() {
        // When both the manifest dir and the CWD fallback have autumn.toml, the
        // primary (manifest) dir wins — matching find_config_file_named.
        let manifest_dir = tempfile::tempdir().expect("manifest dir");
        let cwd = tempfile::tempdir().expect("cwd dir");
        std::fs::write(manifest_dir.path().join("autumn.toml"), "[server]\n").expect("primary");
        std::fs::write(cwd.path().join("autumn.toml"), "[server]\n").expect("fallback");
        let dirs = vec![manifest_dir.path().to_path_buf(), cwd.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "prod");
        assert_eq!(uploads[0].local, manifest_dir.path().join("autumn.toml"));
    }

    #[test]
    fn manifest_uploads_empty_when_no_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = vec![dir.path().to_path_buf()];
        let uploads = manifest_uploads_in(&dirs, "prod");
        assert!(uploads.is_empty(), "no autumn.toml → nothing to upload");
    }

    // ── Media config resolves under the deploy profile (Finding O) ───────────

    #[test]
    fn media_config_no_manifest_is_disabled_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = vec![dir.path().to_path_buf()];
        let (cfg, bin) = load_media_host_config_in(&dirs, "prod").expect("loads");
        assert!(!cfg.enabled, "no autumn.toml → controller disabled");
        assert_eq!(bin, media::DEFAULT_FFMPEG_BIN);
    }

    #[test]
    fn media_config_honors_inline_profile_section() {
        // `[profile.prod.media.mediamtx]` enables media over a base that omits it —
        // a profiled deploy must see the profile's config, not the base default.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[media.ffmpeg]\nbin = \"/usr/bin/ffmpeg\"\n\
             [profile.prod.media.mediamtx]\nenabled = true\napi_port = 19997\n",
        )
        .expect("write base");
        let dirs = vec![dir.path().to_path_buf()];

        // prod: the inline profile section is layered on.
        let (prod, _) = load_media_host_config_in(&dirs, "prod").expect("loads prod");
        assert!(prod.enabled, "prod profile enables media");
        assert_eq!(prod.api_port, 19997);

        // dev: no matching profile section → base default (media disabled).
        let (dev, _) = load_media_host_config_in(&dirs, "dev").expect("loads dev");
        assert!(
            !dev.enabled,
            "dev has no profile media section → base default"
        );
    }

    #[test]
    fn media_config_honors_profile_override_file() {
        // The `autumn-prod.toml` override file enables media over a base that omits
        // it, matching the runtime's Layer-5 override.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\nport = 3000\n")
            .expect("write base");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[media.mediamtx]\nenabled = true\nrtmp_port = 11935\n",
        )
        .expect("write prod override");
        let dirs = vec![dir.path().to_path_buf()];

        let (prod, _) = load_media_host_config_in(&dirs, "prod").expect("loads prod");
        assert!(prod.enabled, "autumn-prod.toml enables media");
        assert_eq!(prod.rtmp_port, 11935);
    }

    #[test]
    fn media_config_honors_profile_override_file_without_base_autumn_toml() {
        // Finding R: the base `autumn.toml` is OPTIONAL, exactly like
        // `AutumnConfig::load_with_env`. A deploy that supplies `[deploy] host` via
        // env and keeps `[media.mediamtx] enabled = true` ONLY in `autumn-prod.toml`
        // (with NO base autumn.toml) must still resolve the profile override and
        // provision MediaMTX — previously the loader early-returned the disabled
        // default the moment no base file existed, silently skipping the override.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[media.mediamtx]\nenabled = true\nrtmp_port = 11935\n",
        )
        .expect("write prod override");
        let dirs = vec![dir.path().to_path_buf()];

        let (prod, _) = load_media_host_config_in(&dirs, "prod").expect("loads prod");
        assert!(
            prod.enabled,
            "profile-only media config is provisioned with no base autumn.toml"
        );
        assert_eq!(prod.rtmp_port, 11935);

        // dev: no `autumn-dev.toml` and no base → disabled default (the override
        // file is profile-scoped, so a different profile is unaffected).
        let (dev, _) = load_media_host_config_in(&dirs, "dev").expect("loads dev");
        assert!(!dev.enabled, "dev has no override file → disabled default");
    }

    #[test]
    fn media_config_profile_layer_overrides_base_value() {
        // Base enables media on one api_port; the profile override wins.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[media.mediamtx]\nenabled = true\napi_port = 9997\n",
        )
        .expect("write base");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[media.mediamtx]\napi_port = 29997\n",
        )
        .expect("write prod override");
        let dirs = vec![dir.path().to_path_buf()];

        let (prod, _) = load_media_host_config_in(&dirs, "prod").expect("loads prod");
        assert!(
            prod.enabled,
            "base enablement is preserved under deep merge"
        );
        assert_eq!(
            prod.api_port, 29997,
            "profile override wins over the base value"
        );
    }

    #[test]
    fn media_config_base_only_when_profile_has_no_media_layer() {
        // No inline `[profile.prod]` and no autumn-prod.toml → the base config is
        // read verbatim under the profile.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[media.mediamtx]\nenabled = true\napi_port = 9001\n",
        )
        .expect("write base");
        let dirs = vec![dir.path().to_path_buf()];

        let (prod, _) = load_media_host_config_in(&dirs, "prod").expect("loads prod");
        assert!(prod.enabled);
        assert_eq!(prod.api_port, 9001);
    }

    #[test]
    fn media_config_fails_closed_on_invalid_value_in_active_profile_layer() {
        // A present-but-ill-typed value in the ACTIVE profile layer aborts, even
        // though the base is valid — the fail-closed rule spans every layer.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[media.mediamtx]\nenabled = true\n",
        )
        .expect("write base");
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[media.mediamtx]\napi_port = \"nope\"\n",
        )
        .expect("write bad prod override");
        let dirs = vec![dir.path().to_path_buf()];

        let err = load_media_host_config_in(&dirs, "prod").expect_err("must fail closed");
        assert!(
            matches!(err, DeployError::Config(_)),
            "invalid profile-layer media config aborts the deploy: {err:?}"
        );
    }

    #[test]
    fn manifest_notice_warns_loudly_when_no_manifest() {
        let notice = manifest_preflight_notice(&[]);
        assert!(
            notice.contains("no autumn.toml") && notice.contains("built-in defaults"),
            "no-manifest notice must loudly warn about silent defaults: {notice}"
        );
    }

    #[test]
    fn manifest_notice_confirms_uploaded_files() {
        let uploads = vec![
            exec::ManifestUpload {
                local: PathBuf::from("/x/autumn.toml"),
                remote_basename: "autumn.toml".to_owned(),
            },
            exec::ManifestUpload {
                local: PathBuf::from("/x/autumn-prod.toml"),
                remote_basename: "autumn-prod.toml".to_owned(),
            },
        ];
        let notice = manifest_preflight_notice(&uploads);
        assert!(notice.contains("autumn.toml"));
        assert!(notice.contains("autumn-prod.toml"));
        assert!(!notice.contains("no autumn.toml"));
    }

    #[test]
    fn deploy_plan_runs_migrations_before_cutover_with_bounded_readiness_gate() {
        let resolved = resolved_defaults();
        let plan = build_deploy_plan(&resolved);
        let labels: Vec<&str> = plan.iter().map(|s| s.label).collect();

        let migrate = labels
            .iter()
            .position(|&l| l == "migrate")
            .expect("migrate step");
        let candidate = labels
            .iter()
            .position(|&l| l == "start-candidate")
            .expect("start-candidate step");
        let readiness = labels
            .iter()
            .position(|&l| l == "readiness-gate")
            .expect("readiness-gate step");
        let cutover = labels
            .iter()
            .position(|&l| l == "cutover")
            .expect("cutover step");
        let drain = labels
            .iter()
            .position(|&l| l == "drain")
            .expect("drain step");

        // (c) Migrations run before the readiness gate, which runs before cutover.
        assert!(
            migrate < readiness,
            "migrations must precede the readiness gate"
        );
        assert!(
            migrate < cutover,
            "migrations must precede cutover (a failed migration leaves the old version serving)"
        );
        assert!(readiness < cutover, "readiness gate must precede cutover");

        // (a) The candidate is started on a SEPARATE/distinct listener and the
        // step must NOT claim it binds the live `server.port` — starting on the
        // live port while the old release still serves would fail with
        // address-in-use before the readiness gate could run.
        let candidate_desc = &plan[candidate].description;
        let candidate_lc = candidate_desc.to_lowercase();
        assert!(
            candidate_lc.contains("separate") || candidate_lc.contains("distinct"),
            "candidate must start on a separate/distinct listener: {candidate_desc}"
        );
        assert!(
            candidate_lc.contains("does not bind") || candidate_lc.contains("not bind"),
            "candidate step must state it does NOT bind the live port: {candidate_desc}"
        );

        // (b) An explicit traffic-handoff/cutover step runs AFTER the readiness
        // gate and BEFORE draining the old release.
        assert!(
            readiness < cutover,
            "handoff must follow the readiness gate"
        );
        assert!(
            cutover < drain,
            "traffic handoff must precede draining the old release"
        );
        let cutover_desc = &plan[cutover].description.to_lowercase();
        assert!(
            cutover_desc.contains("hand") || cutover_desc.contains("traffic"),
            "cutover must describe a traffic handoff: {}",
            plan[cutover].description
        );

        // The readiness gate is bounded by the configured timeout and mentions
        // rollback on timeout (AC-4).
        let gate = &plan[readiness].description;
        assert!(
            gate.contains("60s"),
            "readiness gate should be bounded: {gate}"
        );
        assert!(
            gate.to_lowercase().contains("roll back"),
            "gate should roll back on timeout: {gate}"
        );

        // The plan ends by pruning to keep_releases.
        assert_eq!(plan.last().expect("non-empty plan").label, "prune");
        assert!(plan.last().unwrap().description.contains('3'));
    }

    #[test]
    fn rollback_plan_matches_rollback_ops_sequence() {
        let resolved = resolved_defaults();
        let plan = build_rollback_plan(&resolved);
        let labels: Vec<&str> = plan.iter().map(|s| s.label).collect();
        // The printed plan must mirror `exec::rollback_ops`' actual order.
        assert_eq!(
            labels,
            vec![
                "write-target-unit",
                "daemon-reload",
                "restart-previous",
                "proxy-flip",
                "record-previous",
                "repoint",
                "record-live-slot",
                "readiness-gate",
                "drain-rolled-back-slot",
            ],
            "printed rollback plan must match rollback_ops' execution order"
        );
        let pos = |l: &str| labels.iter().position(|&x| x == l).expect("step present");
        // Re-render the target unit and daemon-reload before restarting, so the
        // restart never relaunches a slot unit clobbered by an earlier failed
        // redeploy. Restart the previous unit before the health-gated flip, and flip
        // before repointing `current`; re-probe before draining the former-live slot.
        assert!(
            pos("write-target-unit") < pos("daemon-reload")
                && pos("daemon-reload") < pos("restart-previous"),
            "the target unit is re-rendered and daemon-reloaded before the restart"
        );
        assert!(
            pos("restart-previous") < pos("proxy-flip"),
            "previous unit must be up before the health-gated flip"
        );
        assert!(
            pos("proxy-flip") < pos("record-previous"),
            "flip must precede recording the previous-release marker"
        );
        assert!(
            pos("record-previous") < pos("repoint"),
            "the previous-release marker is recorded before `current` moves off it"
        );
        assert!(
            pos("proxy-flip") < pos("repoint"),
            "flip must precede the current-symlink repoint"
        );
        assert!(
            pos("restart-previous") < pos("readiness-gate"),
            "restart must precede the /ready re-probe"
        );
        assert!(
            pos("readiness-gate") < pos("drain-rolled-back-slot"),
            "the former-live slot is drained last, after the /ready re-probe"
        );
    }

    #[test]
    fn fleet_plan_matches_fleet_ops_sequence() {
        // #1621 (AC-4, T1.26): the forward twin of
        // `rollback_plan_matches_rollback_ops_sequence`. `build_deploy_plan` and the
        // fleet section it is printed with are DESCRIPTIVE — nothing links them to
        // `exec::cutover_ops` at compile time — so this guards the three claims the
        // printed fleet plan actually makes against the ops that really run.
        let fleet = ResolvedFleet::resolve(&fleet_cfg(&["web-a", "web-b", "web-c"]), "myapp")
            .expect("a well-formed fleet resolves");
        let modes = [
            fleet::HostMode::Redeploy,
            fleet::HostMode::Redeploy,
            fleet::HostMode::Redeploy,
        ];
        let plan = fleet::plan_fleet(&fleet, &modes).expect("a well-formed fleet plans");

        // (1) HOST ORDERING. The rendered section, the executable plan and the
        // resolved fleet all agree on declaration order — the documented rollout
        // contract.
        let hosts: Vec<String> = fleet
            .hosts
            .iter()
            .map(|h| h.host.clone().unwrap_or_default())
            .collect();
        let planned: Vec<String> = plan.hosts.iter().map(|h| h.host.clone()).collect();
        assert_eq!(
            planned, hosts,
            "the executable fleet plan must keep declaration order"
        );
        let rendered = fleet::fleet_plan_lines(&hosts).join("\n");
        let mut previous = 0usize;
        for host in &hosts {
            let at = rendered
                .find(host.as_str())
                .unwrap_or_else(|| panic!("{host} must be named in the plan:\n{rendered}"));
            assert!(
                at >= previous,
                "the printed fleet plan must list hosts in rollout order:\n{rendered}"
            );
            previous = at;
        }

        // (2) SINGLE MIGRATE. The section promises the migration runs once; the real
        // flattened op labels must carry exactly one `migrate`, before the first
        // cutover boundary anywhere in the fleet.
        let flat = fleet::test_support::fleet_op_labels(&fleet, &plan);
        assert_eq!(
            flat.iter().filter(|l| **l == "migrate").count(),
            1,
            "the plan promises one migration; the ops must schedule one: {flat:?}"
        );
        assert_eq!(
            rendered
                .matches(fleet::FLEET_MIGRATE_PLACEMENT_NOTE)
                .count(),
            1,
            "the migrate placement is one fleet-wide note, not a per-host line:\n{rendered}"
        );
        let migrate = flat
            .iter()
            .position(|l| *l == "migrate")
            .expect("the fleet migrates");
        let boundary = flat
            .iter()
            .position(|l| *l == "proxy-flip" || *l == "proxy-route")
            .expect("the fleet cuts over");
        assert!(
            migrate < boundary,
            "the plan's `[migrate] < [cutover]` claim must hold in the real ops: \
             migrate at {migrate}, boundary at {boundary}, labels: {flat:?}"
        );

        // (3) Step-label subset. Every printed step label either names a real executed
        // op or is one of the five deliberately mechanism-neutral labels: `build` is
        // local, `upload` executes as `upload-binary`, `cutover` as `proxy-flip`,
        // `drain` as `drain-old`, and `prepare-host` as the probe-gated `install-proxy`,
        // which runs only on a host with no proxy binary and so is legitimately absent
        // from this already-equipped fleet's ops. Partitioning, rather than a bare
        // `contains`, means a new plan step forces a deliberate decision here instead of
        // drifting away from the ops.
        let step_labels: Vec<&str> = build_deploy_plan(&fleet.hosts[0])
            .iter()
            .map(|s| s.label)
            .collect();
        let (executed, neutral): (Vec<&str>, Vec<&str>) =
            step_labels.iter().partition(|l| flat.contains(l));
        assert_eq!(
            executed,
            vec!["migrate", "start-candidate", "readiness-gate", "prune"],
            "these printed steps must name real executed ops: {step_labels:?} vs {flat:?}"
        );
        assert_eq!(
            neutral,
            vec!["build", "prepare-host", "upload", "cutover", "drain"],
            "only the documented mechanism-neutral plan labels may be absent from \
             the ops: {step_labels:?} vs {flat:?}"
        );
        // The plan's last step is `prune`, and so is the fleet's last real op.
        assert_eq!(
            step_labels.last().copied(),
            Some("prune"),
            "the printed plan ends with prune"
        );
        assert_eq!(
            flat.last().copied(),
            Some("prune"),
            "the fleet's last host ends with prune: {flat:?}"
        );
    }

    #[test]
    fn deploy_host_present_grader_is_offline_and_flags_missing_host() {
        // Present, non-blank host → passes with no network I/O.
        let ok = grade_deploy_host_present(Some("203.0.113.10"));
        assert!(ok.passed);
        assert_eq!(ok.name, "deploy_host");

        // Missing host → fails offline with the actionable `[deploy] host` hint,
        // matching the message `deploy check` uses.
        let missing = grade_deploy_host_present(None);
        assert!(!missing.passed, "missing host must fail offline");
        assert_eq!(missing.name, "deploy_host");
        assert_eq!(missing.detail, DEPLOY_HOST_MISSING_DETAIL);
        assert!(missing.hint.unwrap().contains("[deploy] host"));

        // Blank/whitespace host is treated the same as unset.
        let blank = grade_deploy_host_present(Some("   "));
        assert!(!blank.passed, "blank host must fail offline");
        assert_eq!(blank.detail, DEPLOY_HOST_MISSING_DETAIL);

        // Consistency: the offline host-present grader and the online SSH probe
        // report the missing-host case identically.
        let probe = grade_ssh_reachability(None, 22, Duration::from_millis(50));
        assert_eq!(probe.detail, missing.detail);
        assert_eq!(probe.hint, missing.hint);
    }

    #[test]
    fn ssh_reachability_fails_fast_without_host() {
        let check = grade_ssh_reachability(None, 22, Duration::from_millis(50));
        assert!(!check.passed);
        assert_eq!(check.name, "ssh_reachability");
        assert!(check.hint.unwrap().contains("[deploy] host"));

        // A blank host is treated the same as unset.
        let blank = grade_ssh_reachability(Some("   "), 22, Duration::from_millis(50));
        assert!(!blank.passed);
    }

    #[test]
    fn ssh_reachability_fails_when_all_addresses_unreachable() {
        // Offline + deterministic: the loopback with a port nothing listens on
        // refuses the connect on every resolved address, so the grader must FAIL
        // (not pass). Using a literal IP avoids DNS so the test never depends on
        // the resolver; port 1 is privileged and never bound by a test harness.
        let check = grade_ssh_reachability(Some("127.0.0.1"), 1, Duration::from_millis(100));
        assert!(!check.passed, "closed port should fail: {}", check.detail);
        assert_eq!(check.name, "ssh_reachability");
        assert!(check.detail.contains("cannot reach"));
    }

    #[test]
    fn signing_secret_grader_never_echoes_value() {
        let present = grade_signing_secret(Some("super-secret-value"), &[], false);
        assert!(present.passed);
        assert!(!present.detail.contains("super-secret-value"));

        let missing = grade_signing_secret(None, &[], false);
        assert!(!missing.passed);
        assert!(!grade_signing_secret(Some("   "), &[], false).passed);
    }

    #[test]
    fn signing_secret_grader_non_production_accepts_weak_secret() {
        // Outside production, a present, non-empty secret is enough: a dev/staging
        // deploy must not be blocked by the production strength rules.
        assert!(grade_signing_secret(Some("changeme"), &[], false).passed);
        assert!(grade_signing_secret(Some("short"), &[], false).passed);
        // A weak rotation secret is likewise fine outside production.
        assert!(grade_signing_secret(Some("changeme"), &["also-weak".to_owned()], false).passed);
    }

    #[test]
    fn signing_secret_grader_production_rejects_weak_secret() {
        // In production the grader reuses the runtime validator
        // (`autumn_web::security::validate_signing_secret`), which the app boot
        // path also runs before binding. A known demo value and a too-short
        // secret must both FAIL preflight so `deploy check` never greenlights a
        // release that would exit on startup.
        let demo = grade_signing_secret(Some("changeme"), &[], true);
        assert!(!demo.passed, "demo value must fail in production");
        assert!(demo.hint.is_some());
        // Never echo the secret value, even a known demo one.
        assert!(!demo.detail.contains("changeme"));

        let short = grade_signing_secret(Some("too-short"), &[], true);
        assert!(!short.passed, "too-short secret must fail in production");
        assert!(!short.detail.contains("too-short"));
    }

    #[test]
    fn signing_secret_grader_production_accepts_strong_secret() {
        // A 64-hex-char secret (openssl rand -hex 32) clears the runtime
        // validator's minimum length and is not a demo value.
        let strong = "a".repeat(64);
        let check = grade_signing_secret(Some(&strong), &[], true);
        assert!(
            check.passed,
            "strong production secret should pass: {}",
            check.detail
        );
        assert!(!check.detail.contains(&strong));
    }

    #[test]
    fn signing_secret_grader_production_missing_fails() {
        let check = grade_signing_secret(None, &[], true);
        assert!(!check.passed);
        assert!(check.hint.is_some());
    }

    #[test]
    fn signing_secret_grader_production_rejects_weak_previous_secret() {
        // The app boot path validates each `previous_secrets` entry with the same
        // rule and exits if any is weak, so a strong CURRENT secret paired with a
        // weak rotation entry must FAIL preflight rather than boot-crash later.
        let strong = "a".repeat(64);
        let check = grade_signing_secret(Some(&strong), &["changeme".to_owned()], true);
        assert!(
            !check.passed,
            "weak previous_secrets entry must fail in production"
        );
        assert!(check.hint.is_some());
        // Never echo the rotation secret value.
        assert!(!check.detail.contains("changeme"));
        // The message identifies it as a previous/rotation secret.
        assert!(
            check.detail.contains("previous"),
            "detail should name the rotation secret: {}",
            check.detail
        );
    }

    #[test]
    fn signing_secret_grader_production_accepts_strong_previous_secrets() {
        // Strong current + strong rotation entries pass.
        let strong = "a".repeat(64);
        let previous = vec!["b".repeat(64), "c".repeat(64)];
        let check = grade_signing_secret(Some(&strong), &previous, true);
        assert!(
            check.passed,
            "strong current + strong previous should pass: {}",
            check.detail
        );
    }

    #[test]
    fn signing_secret_grader_non_production_ignores_weak_previous_secret() {
        // Outside production, rotation entries are not strength-checked.
        let check = grade_signing_secret(Some("dev-secret"), &["changeme".to_owned()], false);
        assert!(check.passed);
    }

    #[test]
    fn database_url_grader_never_echoes_value() {
        // Force the DB-backed path so the missing-URL case still fails: a
        // configured `[database]` marks the app as database-backed.
        let absent = Path::new("autumn-deploy-no-such-migrations-dir-echo-guard");
        let present = grade_database_url(Some("postgres://user:pw@host/db"), absent, true, false);
        assert!(present.passed);
        assert!(!present.detail.contains("pw"));
        assert!(!grade_database_url(None, absent, true, false).passed);
    }

    #[test]
    fn database_url_grader_passes_for_db_free_app() {
        // No migrations directory and no configured `[database]`: a
        // zero-dependency daemon app has nothing to connect to, so preflight
        // must pass rather than require a URL.
        let tmp = tempfile::TempDir::new().unwrap();
        let absent_migrations = tmp.path().join("migrations");
        assert!(!absent_migrations.exists());
        let check = grade_database_url(None, &absent_migrations, false, false);
        assert!(
            check.passed,
            "DB-free app should pass the DB-URL preflight: {}",
            check.detail
        );
    }

    #[test]
    fn database_url_grader_fails_for_db_backed_runtime_without_url() {
        // No migrations dir and no `[database]` section, but a DB-backed runtime
        // feature (e.g. `jobs.backend = "postgres"` or `scheduler.backend =
        // "postgres"`) is enabled. Its startup path requires a configured pool,
        // so a missing writable URL must FAIL preflight rather than taking the
        // DB-free pass branch and failing at boot.
        let tmp = tempfile::TempDir::new().unwrap();
        let absent_migrations = tmp.path().join("migrations");
        assert!(!absent_migrations.exists());
        let check = grade_database_url(None, &absent_migrations, false, true);
        assert!(
            !check.passed,
            "DB-backed runtime feature without a URL must fail preflight"
        );
        assert!(
            check.hint.is_some(),
            "the failure should carry an actionable remediation hint"
        );
    }

    #[test]
    fn database_url_grader_passes_for_db_backed_runtime_with_url() {
        // The same DB-backed runtime feature WITH a writable primary URL passes.
        let tmp = tempfile::TempDir::new().unwrap();
        let absent_migrations = tmp.path().join("migrations");
        assert!(!absent_migrations.exists());
        let check = grade_database_url(
            Some("postgres://user:pw@host/db"),
            &absent_migrations,
            false,
            true,
        );
        assert!(
            check.passed,
            "DB-backed runtime feature with a URL should pass: {}",
            check.detail
        );
        assert!(!check.detail.contains("pw"));
    }

    #[test]
    fn database_url_grader_fails_for_db_backed_app_without_url() {
        // A migrations directory (the same presence check `grade_migrate_check`
        // uses) marks the app as database-backed, so a missing URL must fail
        // with an actionable hint.
        let tmp = tempfile::TempDir::new().unwrap();
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir(&migrations).unwrap();
        let check = grade_database_url(None, &migrations, false, false);
        assert!(
            !check.passed,
            "DB-backed app without a URL should fail preflight"
        );
        assert!(
            check.hint.is_some(),
            "the failure should carry an actionable remediation hint"
        );
    }

    #[test]
    fn shard_only_config_resolves_a_usable_db_url() {
        use autumn_web::config::{DatabaseConfig, ShardConfig};

        // A shard-only app: no control `primary_url`/`url`/`replica_url`, but one
        // `[[database.shards]]` entry with a `primary_url`. `autumn migrate` would
        // target that shard, so the preflight must treat the app as having a
        // usable database and PASS even with a migrations directory present.
        let db = DatabaseConfig {
            shards: vec![ShardConfig {
                name: "shard0".to_owned(),
                primary_url: "postgres://user:pw@shard0/app".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(db.effective_primary_url().is_none());

        let url = resolve_writable_db_url(&db);
        assert_eq!(url, Some("postgres://user:pw@shard0/app"));

        // Migrations dir present + database configured (has_shards) → DB-backed.
        let tmp = tempfile::TempDir::new().unwrap();
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir(&migrations).unwrap();
        let check = grade_database_url(url, &migrations, true, false);
        assert!(
            check.passed,
            "shard-only app with migrations should pass the DB-URL preflight: {}",
            check.detail
        );
        // The grader must never echo the shard credentials.
        assert!(!check.detail.contains("pw"));
    }

    #[test]
    fn replica_only_config_resolves_no_writable_url_and_fails_with_migrations() {
        use autumn_web::config::DatabaseConfig;

        // A replica-only app: only `database.replica_url` is set — no control
        // `primary_url`/`url` and no `[[database.shards]]`. `autumn migrate` only
        // ever targets a writable primary/control or shard-primary URL (see
        // `migrate::build_targets`); it can never migrate against a replica. So
        // the writable-URL resolver must return None, and a project with a
        // `migrations/` dir must FAIL the DB-URL preflight rather than passing on
        // a replica the migration step can't use.
        let db = DatabaseConfig {
            replica_url: Some("postgres://user:pw@replica/app".to_owned()),
            ..Default::default()
        };
        assert!(db.effective_primary_url().is_none());

        let url = resolve_writable_db_url(&db);
        assert_eq!(
            url, None,
            "replica_url alone is not a writable migration target"
        );

        // Migrations dir present + database configured (replica_url set) →
        // DB-backed, but no writable URL → FAIL with an actionable hint.
        let tmp = tempfile::TempDir::new().unwrap();
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir(&migrations).unwrap();
        let check = grade_database_url(url, &migrations, true, false);
        assert!(
            !check.passed,
            "replica-only app with migrations must fail the DB-URL preflight"
        );
        assert!(
            check.hint.is_some(),
            "the failure should carry an actionable remediation hint"
        );
        // The grader must never echo the replica credentials.
        assert!(!check.detail.contains("pw"));
    }

    #[test]
    fn migrate_check_passes_when_no_migrations_dir() {
        let dir = std::env::temp_dir().join("autumn-deploy-no-such-migrations-dir-xyz");
        let check = grade_migrate_check(&dir);
        assert!(
            check.passed,
            "absent migrations dir should pass: {}",
            check.detail
        );
    }

    #[test]
    fn is_production_profile_matches_runtime_rule() {
        assert!(is_production_profile(Some("prod")));
        assert!(is_production_profile(Some("production")));
        assert!(!is_production_profile(Some("dev")));
        assert!(!is_production_profile(Some("staging")));
        assert!(!is_production_profile(None));
    }

    #[test]
    fn requires_database_pool_detects_postgres_backends() {
        use autumn_web::config::{AutumnConfig, SchedulerBackend};

        // Default (local jobs, in-process scheduler): no pool required.
        let mut config = AutumnConfig::default();
        assert!(!requires_database_pool(&config));

        // jobs.backend = "postgres" → pool required.
        config.jobs.backend = "postgres".to_owned();
        assert!(requires_database_pool(&config));

        // scheduler.backend = postgres → pool required.
        let mut config = AutumnConfig::default();
        config.scheduler.backend = SchedulerBackend::Postgres;
        assert!(requires_database_pool(&config));

        // A non-postgres jobs backend (redis/local) does not require a PG pool.
        let mut config = AutumnConfig::default();
        config.jobs.backend = "redis".to_owned();
        assert!(!requires_database_pool(&config));
    }

    #[test]
    fn rollback_dispatch_does_not_load_media_config() {
        // Finding F: the media host config is loaded ONLY in the plan/up action
        // paths. `check`/`rollback` neither read nor provision MediaMTX, so a typo
        // in `[media.mediamtx]`/`[media.ffmpeg]` must not fail-close a rollback (a
        // regression the round-2 fail-closed load would otherwise reintroduce). The
        // load is filesystem-coupled (`manifest_project_dirs()`), so we assert the
        // dispatch source itself: the fallible load call appears only inside the
        // Plan and Up match arms, never in Check/Rollback.
        let src = include_str!("deploy.rs");
        // Build the needle at runtime so this test's own source text (which mentions
        // the fn name) can never be miscounted as a call site. The loader now takes
        // the resolved deploy config (Finding O), so match the call shape, not `()`.
        let needle = ["load_media_host_config", "(&resolved)?"].concat();
        let call_sites = src.matches(needle.as_str()).count();
        assert_eq!(
            call_sites, 2,
            "expected exactly two fallible load-media call sites (Plan + Up), found {call_sites}",
        );

        // Isolate `run()`'s match arms and prove Rollback/Check don't load media.
        let run_body = src
            .split("pub fn run(action: DeployAction)")
            .nth(1)
            .expect("run() present");
        let rollback_arm = run_body
            .split("DeployAction::Rollback =>")
            .nth(1)
            .expect("Rollback arm present");
        // Rollback dispatch line runs to the next arm.
        let rollback_line = rollback_arm
            .split("DeployAction::Up")
            .next()
            .expect("Up arm follows Rollback");
        assert!(
            !rollback_line.contains("load_media_host_config"),
            "the Rollback arm must not load the media config",
        );
        let check_arm = run_body
            .split("DeployAction::Check =>")
            .nth(1)
            .and_then(|s| s.split("DeployAction::Rollback").next())
            .expect("Check arm present");
        assert!(
            !check_arm.contains("load_media_host_config"),
            "the Check arm must not load the media config",
        );
    }

    #[test]
    fn media_provisioning_is_deferred_past_app_cutover_in_up() {
        // Finding K: the mutating MediaMTX provisioning must run only after the app
        // deploy and cutover commit. The app deploy rolls back on failure — readiness
        // gate, migrations — and leaves the old release serving, and there is
        // deliberately no media teardown, so writing and restarting the host MediaMTX
        // unit before the app is committed would strand a rolled-back release against a
        // moved media daemon. This asserts the source order of the `up` path: the
        // non-mutating preflight precedes the rollout loop, and `provision_media_host`
        // is invoked only after that loop has run every host to completion.
        //
        // #1621 re-anchored the middle marker. `run_up` is now a fleet-shaped driver
        // (`run_up` prologue plus `run_up_with` loop), so the commit point is the
        // per-host execution loop rather than a single `result.map_err(...)?` line — a
        // strictly stronger statement: provisioning must follow the last host's cutover,
        // not merely one host's.
        let src = include_str!("deploy.rs");
        let up_body = src
            .split("fn run_up(")
            .nth(1)
            .and_then(|s| s.split("\nfn run_rollback(").next())
            .expect("run_up body present");

        let preflight_at = up_body
            .find("check_media_host_preflight(input.media_cfg")
            .expect("preflight call present on the up path");
        // The per-host cutovers happen inside this loop. A failed host breaks out of
        // it and returns from the halt block immediately below — after its own
        // teardown and after any fleet compensation (#1621 slice 4) — so nothing
        // past that point runs.
        let commit_at = up_body
            .find("for (index, host_plan) in plan.hosts.iter().enumerate()")
            .expect("per-host execution loop present on the up path");
        let provision_at = up_body
            .find("provision_media_host(input.media_cfg, executor)?;")
            .expect("deferred provisioning call present on the up path");

        assert!(
            preflight_at < commit_at,
            "media preflight must precede the per-host execution loop",
        );
        assert!(
            commit_at < provision_at,
            "MediaMTX provisioning must run only AFTER every host's cutover commits — so a \
             rolled-back or halted rollout (which returns from the loop's halt path) never \
             reaches it",
        );

        // The preflight itself is non-mutating: it runs the doctor checks but must
        // NOT drive the controller ops (those live only in provision_media_host).
        let preflight_fn = src
            .split("fn check_media_host_preflight(")
            .nth(1)
            .and_then(|s| s.split("\nfn provision_media_host(").next())
            .expect("check_media_host_preflight present");
        assert!(
            preflight_fn.contains("collect_media_doctor_checks"),
            "preflight must run the doctor checks",
        );
        assert!(
            !preflight_fn.contains("ensure_installed_ops"),
            "preflight must NOT drive the mutating controller ops (write/restart)",
        );

        // And the mutating provisioning is reachable only on the post-commit path.
        let after_commit = &up_body[commit_at..];
        assert!(
            after_commit.contains("provision_media_host(input.media_cfg, executor)?;"),
            "provisioning must sit on the post-commit path (unreachable when a host's \
             deploy errors out and the rollout halts)",
        );
    }

    // ── Fleet rollout driver (issue #1621, slice 3) ──────────────────────────
    //
    // These drive the WHOLE loop — prologue probe → plan → per-host execute —
    // against one strict recording fake per host, through the `run_up_with` seam.
    // Nothing here contacts a host, resolves a binary, or reads the filesystem:
    // every filesystem-coupled prologue step (preflight, binary resolution,
    // manifest location, release-id minting) is resolved by `run_up` and handed in
    // as data, which is what makes the rollout itself unit-testable.

    /// The read-only probe labels a rollout is allowed to run before (and without)
    /// mutating anything. Enumerated here, not imported, so a new mutating op can
    /// never quietly join the allowlist: the "zero mutations" assertions below are
    /// only as strong as this list is short.
    const READ_ONLY_PROBES: [&str; 3] =
        ["proxy-compat-probe", "detect-current", "probe-release-dir"];

    /// The exact ordered `Run` labels of today's single-host zero-downtime
    /// redeploy — the same vector `redeploy_produces_exact_zero_downtime_sequence`
    /// (`exec.rs`) pins, restated here so a fleet host's sequence is asserted
    /// against the protected contract and not merely against itself.
    const REDEPLOY_RUN_LABELS: [&str; 13] = [
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
    ];

    /// The exact ordered `Run` labels ONE host records in a fleet-wide rollback:
    /// the read-only previous-release probe, then the UNCHANGED on-demand
    /// `rollback_ops` sequence (`write-target-unit` is an upload, so it is not a
    /// `Run`). Restated here rather than imported so a fleet rollback can never
    /// quietly grow a step the single-host rollback does not have.
    const FLEET_ROLLBACK_RUN_LABELS: [&str; 7] = [
        "resolve-previous",
        "daemon-reload",
        "restart-previous",
        "proxy-flip",
        "commit-markers",
        "readiness-gate",
        "drain-rolled-back-slot",
    ];

    const FLEET_RELEASE_ID: &str = "20260714T120000Z";
    const FLEET_PUBLIC_PORT: u16 = 3000;

    /// The halt report inside a [`DeployError::FleetHalted`], or a panic naming
    /// what came instead. Every fleet-failure test asserts on the same shape, so
    /// unwrapping it once keeps those assertions about the FLEET rather than about
    /// the boxing.
    fn fleet_halt_of(err: &DeployError) -> &FleetHalt {
        match err {
            DeployError::FleetHalted(halt) => halt,
            other => panic!("expected a fleet halt, got: {other:?}"),
        }
    }

    fn fleet_of(hosts: &[&str]) -> ResolvedFleet {
        ResolvedFleet::resolve(
            &DeployConfig {
                hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("a well-formed fleet resolves")
    }

    /// Probe stdout for a host already serving on the blue slot. The unit/options
    /// delimiters are deliberately absent, so the installed proxy port and the
    /// proxy-options marker both degrade to `Absent` — "nothing to conflict with",
    /// the shape a legacy host presents.
    fn redeploy_probe() -> String {
        "redeploy:blue\t3001\n---autumn-kamal-proxy-list---\n".to_owned()
    }

    /// Probe stdout for a host with nothing installed yet.
    fn first_deploy_probe() -> String {
        "first\n---autumn-kamal-proxy-list---\n".to_owned()
    }

    /// Probe stdout whose `shared/proxy-options` marker is PRESENT but unparseable
    /// (no tab field) — the fail-closed `#2074` shape.
    fn unreadable_proxy_options_probe() -> String {
        "redeploy:blue\t3001\n\
         ---autumn-kamal-proxy-list---\n\
         ---autumn-kamal-proxy-unit---\n\
         --http-port 3000\n\
         ---autumn-kamal-proxy-options---\n\
         garbage\n"
            .to_owned()
    }

    /// A `kamal-proxy deploy --help` capture carrying every flag the cutover uses,
    /// so the compat probe passes.
    fn compatible_deploy_help() -> &'static str {
        "Usage:\n  kamal-proxy deploy SERVICE [flags]\n\nFlags:\n  \
         --target host:port\n  --health-check-path string\n  --host strings\n  \
         --tls\n  --deploy-timeout duration\n  --drain-timeout duration\n  \
         --force\n"
    }

    fn fleet_manifests() -> Vec<exec::ManifestUpload> {
        vec![exec::ManifestUpload {
            local: PathBuf::from("/local/autumn.toml"),
            remote_basename: "autumn.toml".to_owned(),
        }]
    }

    /// Script one host as a healthy redeploy: compatible proxy, serving on blue,
    /// no release-dir collision.
    fn script_redeploy(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(host, "proxy-compat-probe", compatible_deploy_help())
            .script(host, "detect-current", redeploy_probe())
            .script(host, "probe-release-dir", "absent")
    }

    /// Script one host as a healthy FIRST deploy.
    fn script_first_deploy(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(host, "proxy-compat-probe", compatible_deploy_help())
            .script(host, "detect-current", first_deploy_probe())
            .script(host, "probe-release-dir", "absent")
    }

    /// Script one BARE host (issue #1607, AC-1): the compat probe answers nothing,
    /// which is what a host with no kamal-proxy at `/usr/local/bin` looks like.
    fn script_bare_first_deploy(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(host, "proxy-compat-probe", "")
            .script(host, "detect-current", first_deploy_probe())
            .script(host, "probe-release-dir", "absent")
    }

    #[test]
    fn a_bare_host_installs_the_proxy_as_the_first_op_of_its_own_turn() {
        // #1607 (AC-1): "the command performs any remaining host preparation
        // itself". The install must sit at the head of that host's OWN turn, not in
        // the all-hosts probe phase — otherwise a fleet whose last host is
        // unreachable would already have modified the earlier hosts before grading
        // finished, breaking "no host is touched until every host is graded".
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_bare_first_deploy(recorder, "web-a");
        recorder = script_bare_first_deploy(recorder, "web-b");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("a bare fleet deploys after preparing each host");

        for host in ["web-a", "web-b"] {
            let labels = recorder.run_labels_for(host);
            let install = labels
                .iter()
                .position(|l| *l == "install-proxy")
                .unwrap_or_else(|| panic!("host {host} must be prepared: {labels:?}"));
            let first_mutating = recorder
                .first_mutating(host, &READ_ONLY_PROBES)
                .expect("a deployed host mutates something");
            assert_eq!(
                labels[install..install + 2].to_vec(),
                vec!["install-proxy", "proxy-install"],
                "the install must precede the proxy unit it supervises on {host}: {labels:?}"
            );
            assert!(
                recorder
                    .positions_of("install-proxy")
                    .contains(&first_mutating),
                "host preparation must be the FIRST thing that touches {host} \
                 (its first mutating call was at tape position {first_mutating})"
            );
        }

        // Both hosts finished being GRADED before either was prepared: every
        // read-only probe on the fleet tape precedes the first install.
        let first_install = *recorder
            .positions_of("install-proxy")
            .first()
            .expect("the fleet prepares a host");
        for label in READ_ONLY_PROBES {
            let last_probe = *recorder
                .positions_of(label)
                .last()
                .unwrap_or_else(|| panic!("every host runs {label}"));
            assert!(
                last_probe < first_install,
                "the probe phase must finish before anything is installed: {label} at \
                 {last_probe}, first install at {first_install}"
            );
        }
    }

    #[test]
    fn a_bare_redeploy_host_is_prepared_before_its_marker_repair() {
        // A host can need BOTH: no proxy binary and a drifted live-slot marker. Host
        // preparation must still come first — the marker repair (and everything
        // after it) is only worth attempting once the proxy the host is supposed to
        // be running actually exists.
        let fleet = fleet_of(&["web-a"]);
        // The marker says blue (3001) but the proxy is serving green (public + 2 =
        // 3002), so the deploy reconciles onto the proxy's truth and repairs the
        // marker as an early op.
        let drifted = "redeploy:blue\t3001\n\
             ---autumn-kamal-proxy-list---\n\
             Service   Host          Target            State    TLS\n\
             myapp     example.com   127.0.0.1:3002   running  no\n\
             ---autumn-kamal-proxy-unit---\n--http-port 3000\n"
            .to_owned();
        let recorder = fleet::test_support::FleetRecorder::new()
            .script("web-a", "proxy-compat-probe", "")
            .script("web-a", "detect-current", drifted)
            .script("web-a", "probe-release-dir", "absent");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("a bare, drifted host deploys after being prepared");

        let labels = recorder.run_labels_for("web-a");
        let install = labels
            .iter()
            .position(|l| *l == "install-proxy")
            .expect("a bare host is prepared");
        let repair = labels
            .iter()
            .position(|l| *l == "record-live-slot")
            .expect("a drifted marker is repaired");
        assert!(
            install < repair,
            "host preparation must precede the marker repair: {labels:?}"
        );
        let first_mutating = recorder
            .first_mutating("web-a", &READ_ONLY_PROBES)
            .expect("a deployed host mutates something");
        assert!(
            recorder
                .positions_of("install-proxy")
                .contains(&first_mutating),
            "host preparation is still the FIRST thing that touches the host"
        );
    }

    #[test]
    fn declining_host_preparation_refuses_a_bare_host_without_touching_it() {
        // #1607: `[deploy] install_proxy = false` is the documented opt-out. It must
        // reach the assessor through the REAL call site — a hardcoded `Auto` there
        // would silently ignore the setting — and a declined host must be left
        // completely alone, not half-prepared.
        let mut cfg = fleet_cfg(&["web-a"]);
        cfg.install_proxy = false;
        let fleet = ResolvedFleet::resolve(&cfg, "myapp").expect("a one-host fleet resolves");
        let recorder = fleet::test_support::FleetRecorder::new()
            .script("web-a", "proxy-compat-probe", "")
            .script("web-a", "detect-current", first_deploy_probe())
            .script("web-a", "probe-release-dir", "absent");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a bare host with host prep declined must refuse");
        let message = err.to_string();
        assert!(
            message.contains("install_proxy"),
            "the refusal must name the setting in force: {message}"
        );
        assert_eq!(
            recorder.run_labels_for("web-a"),
            vec!["proxy-compat-probe"],
            "a declined host must be left untouched"
        );
    }

    #[test]
    fn a_failed_host_preparation_halts_the_rollout_and_touches_no_later_host() {
        // Host preparation is an ordinary PRE-cutover op, so its failure must behave
        // like any other: this host is compensated, and the rest of the fleet is
        // never reached.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_bare_first_deploy(recorder, "web-a");
        recorder = script_bare_first_deploy(recorder, "web-b");
        recorder = recorder.fail("web-a", "install-proxy");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a failed host preparation must fail the rollout");
        assert!(
            err.to_string().contains("install-proxy"),
            "the failing step is named: {err}"
        );
        // web-a never got past its first op: nothing was uploaded, nothing started,
        // and the only ops after the failure are the first-deploy teardown that
        // compensates it.
        let web_a = recorder.run_labels_for("web-a");
        let (probes, rest) = web_a.split_at(READ_ONLY_PROBES.len());
        assert_eq!(probes, READ_ONLY_PROBES);
        assert_eq!(rest[0], "install-proxy");
        assert!(
            rest[1..].iter().all(|l| l.starts_with("teardown-")),
            "only compensation may follow a failed host preparation: {web_a:?}"
        );
        // …and web-b was only ever GRADED, never touched.
        assert_eq!(
            recorder.run_labels_for("web-b"),
            READ_ONLY_PROBES.to_vec(),
            "a halt must leave every later host untouched"
        );
    }

    #[test]
    fn a_host_that_already_has_the_proxy_is_never_prepared() {
        // Idempotence, at the driver level: the compat probe answers, so nothing is
        // installed on either host.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_first_deploy(recorder, "web-a");
        recorder = script_redeploy(recorder, "web-b");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("an equipped fleet deploys");

        assert!(
            recorder.positions_of("install-proxy").is_empty(),
            "an equipped host must never be re-prepared"
        );
    }

    /// The release a compensated redeploy host rolls BACK to — the blue release it
    /// was serving before this run flipped it to green.
    const PREVIOUS_RELEASE_DIR: &str = "/srv/autumn/myapp/releases/20260101T000000Z";

    /// Script the two read-only probes a compensating rollback runs on a host: the
    /// previous-release marker, and the §4.7 precheck that the dir it names still
    /// exists. `target_dir` is the precheck's answer (`present`/`absent`/garbage).
    fn script_compensation(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
        target_dir: &'static str,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(
                host,
                "resolve-previous",
                format!("prev:{PREVIOUS_RELEASE_DIR}\tblue\t3001"),
            )
            .script(host, "probe-rollback-target", target_dir)
    }

    /// The ordered `Run` labels of a compensating rollback on one host: the two
    /// read-only probes, then the unchanged on-demand `rollback_ops` sequence
    /// (`write-target-unit` is an upload, so it is not a `Run`).
    const COMPENSATION_RUN_LABELS: [&str; 8] = [
        "resolve-previous",
        "probe-rollback-target",
        "daemon-reload",
        "restart-previous",
        "proxy-flip",
        "commit-markers",
        "readiness-gate",
        "drain-rolled-back-slot",
    ];

    /// The `Run` labels a host records for a full redeploy plus the compensating
    /// rollback that followed it.
    fn redeploy_then_compensated(migrated: bool) -> Vec<&'static str> {
        READ_ONLY_PROBES
            .iter()
            .copied()
            .chain(
                REDEPLOY_RUN_LABELS
                    .iter()
                    .copied()
                    .filter(|label| migrated || *label != "migrate"),
            )
            .chain(COMPENSATION_RUN_LABELS)
            .collect()
    }

    /// Build a driver input over `fleet` with every fleet-wide value fixed, so the
    /// per-host op vectors are byte-comparable against the single-host builders.
    struct FleetFixture {
        env_file: exec::Secret,
        manifests: Vec<exec::ManifestUpload>,
        proxy: proxy::KamalProxyController,
        media_cfg: media::MediaMtxHostConfig,
        binary: PathBuf,
    }

    impl FleetFixture {
        fn new() -> Self {
            Self {
                env_file: exec::Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=topsecret\n"),
                manifests: fleet_manifests(),
                proxy: proxy::KamalProxyController::new(60),
                media_cfg: media::MediaMtxHostConfig::default(),
                binary: PathBuf::from("/local/target/release/myapp"),
            }
        }

        /// The production shape: a halt compensates the hosts that already cut over.
        fn input<'a>(
            &'a self,
            fleet: &'a ResolvedFleet,
        ) -> FleetUpInput<'a, proxy::KamalProxyController> {
            FleetUpInput {
                fleet,
                proxy: &self.proxy,
                checks: &[],
                env_file: &self.env_file,
                binary: &self.binary,
                manifests: &self.manifests,
                release_id: FLEET_RELEASE_ID,
                public_port: FLEET_PUBLIC_PORT,
                media_cfg: &self.media_cfg,
                ffmpeg_bin: "ffmpeg",
                // No `--only`: the rollout covers everything the config declares.
                configured_host_count: fleet.hosts.len(),
                db: FleetDatabaseFacts::default(),
                // Durable by default in the fakes: the AC-8 warning has its own
                // unit test, and it must not add a line to every driver tape.
                jobs_backend: "postgres",
                scheduler_in_process: false,
                auto_rollback: true,
            }
        }

        /// Halt-and-freeze: stop at the first failure and change nothing else, for
        /// inspection (#1621, §4.7 — the internal seam behind the `--no-rollback`
        /// flag the fleet CLI surface adds).
        fn input_frozen<'a>(
            &'a self,
            fleet: &'a ResolvedFleet,
        ) -> FleetUpInput<'a, proxy::KamalProxyController> {
            FleetUpInput {
                auto_rollback: false,
                ..self.input(fleet)
            }
        }

        /// The exact ordered calls today's SINGLE-host `run_up` would record for
        /// `cfg` on the given path — built from the same unchanged per-host
        /// builders, driven through the same plain recording fake.
        fn todays_calls(
            &self,
            cfg: &ResolvedDeployConfig,
            mode: fleet::HostMode,
            migrate: exec::MigrateStep,
        ) -> Vec<exec::test_support::RecordedCall> {
            let slots = match mode {
                fleet::HostMode::First => exec::SlotPlan::first(FLEET_PUBLIC_PORT),
                fleet::HostMode::Redeploy => {
                    exec::SlotPlan::redeploy(FLEET_PUBLIC_PORT, exec::SLOT_BLUE)
                }
            };
            let release_dir = format!("{}/{FLEET_RELEASE_ID}", cfg.releases_dir());
            let unit = render_app_unit(
                cfg,
                &release_dir,
                slots.candidate_port,
                slots.candidate_slot,
            );
            let ops = match mode {
                fleet::HostMode::First => exec::first_deploy_ops(
                    cfg,
                    &self.proxy,
                    &unit,
                    self.env_file.clone(),
                    &self.binary,
                    &self.manifests,
                    FLEET_RELEASE_ID,
                    &slots,
                    migrate,
                ),
                fleet::HostMode::Redeploy => exec::cutover_ops(
                    cfg,
                    &self.proxy,
                    &unit,
                    self.env_file.clone(),
                    &self.binary,
                    &self.manifests,
                    FLEET_RELEASE_ID,
                    &slots,
                    &self.proxy.proxy_service_options(),
                    migrate,
                ),
            };
            let recorder = exec::test_support::RecordingExecutor::new();
            exec::run_ops(&ops, &recorder).expect("the recording fake never fails");
            recorder.calls()
        }
    }

    #[test]
    fn fleet_halted_carries_only_host_names_and_static_labels() {
        // #1621: `FleetHalted` is the one error type that describes partial fleet
        // state, so it is the one most tempting to enrich with "context". Every
        // field is a host name or a `&'static str` op label — never a shell line, a
        // remote path, or a formatted source error (a failed migration's driver
        // error can embed the database URL, which is exactly why
        // `apply_pending_or_exit` redacts it).
        let err = fleet_halted(
            &fleet::FleetPlan {
                hosts: vec![
                    fleet::HostPlan {
                        host: "web-a".to_owned(),
                        mode: fleet::HostMode::Redeploy,
                        migrate: exec::MigrateStep::Run,
                    },
                    fleet::HostPlan {
                        host: "web-b".to_owned(),
                        mode: fleet::HostMode::Redeploy,
                        migrate: exec::MigrateStep::Skip,
                    },
                ],
            },
            &[
                fleet::HostOutcome::Degraded { label: "prune" },
                fleet::HostOutcome::AmbiguousMarkers,
            ],
            // #1621 slice 4: the degraded log and the manual list are the two
            // richest new fields, so the secrets assertion below covers them too.
            &[("web-a".to_owned(), "prune")],
            "web-b".to_owned(),
            "migrate",
        );

        let rendered = format!("{err}\n{err:?}");
        assert!(
            rendered.contains("web-b") && rendered.contains("migrate"),
            "the halt must name the failing host and step: {rendered}"
        );
        assert!(
            rendered.contains("prune") && rendered.contains("web-a"),
            "the degraded log must name the host and the housekeeping label: {rendered}"
        );
        assert!(
            rendered.contains(fleet::MANUAL_AMBIGUOUS_MARKERS),
            "an ambiguous-marker host must be reported as needing a human: {rendered}"
        );
        assert!(
            rendered.contains("NOT rolled back"),
            "the halt must state that the schema did not come back with the binaries: \
             {rendered}"
        );
        for secret in [
            "postgres://",
            "topsecret",
            "AUTUMN_SECURITY__SIGNING_SECRET",
            "systemd-run",
        ] {
            assert!(
                !rendered.contains(secret),
                "FleetHalted must never carry `{secret}`: {rendered}"
            );
        }
    }

    #[test]
    fn fleet_of_one_matches_todays_single_host_sequence() {
        // #1621 (AC-1, T1.6 / plan P1). The hard invariant: a one-host fleet runs
        // the SAME ops, in the same order, with the same rendered shell, as the
        // pre-fleet single-host path — proven differentially against the unchanged
        // per-host builders, not against a remembered vector.
        let fleet = fleet_of(&["203.0.113.10"]);
        let recorder = script_redeploy(fleet::test_support::FleetRecorder::new(), "203.0.113.10");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("a scripted one-host fleet deploys cleanly");

        // (a) the exact protected label vector, behind the read-only prologue.
        let expected_labels: Vec<&'static str> = READ_ONLY_PROBES
            .iter()
            .copied()
            .chain(REDEPLOY_RUN_LABELS)
            .collect();
        assert_eq!(
            recorder.run_labels_for("203.0.113.10"),
            expected_labels,
            "a one-host fleet must run today's exact zero-downtime sequence"
        );

        // (b) and byte-for-byte the same calls (shells, upload paths, modes) as the
        // single-host builders produce — the differential form of AC-1.
        let observed = recorder.calls_for("203.0.113.10");
        let (probes, mutating) = observed.split_at(READ_ONLY_PROBES.len());
        assert_eq!(
            probes
                .iter()
                .filter_map(exec::test_support::RecordedCall::run_label)
                .collect::<Vec<_>>(),
            READ_ONLY_PROBES.to_vec(),
            "the rollout prologue must be read-only probes only"
        );
        assert_eq!(
            mutating,
            fixture
                .todays_calls(
                    &fleet.hosts[0],
                    fleet::HostMode::Redeploy,
                    exec::MigrateStep::Run
                )
                .as_slice(),
            "a one-host fleet's ops must be byte-identical to today's single-host ops"
        );
    }

    #[test]
    fn fleet_of_one_first_deploy_matches_todays_single_host_sequence() {
        // #1621 (AC-1, T1.6): the same differential proof on the first-deploy path.
        // Since #1607 (AC-3) that path carries the migrate op too, so a one-entry
        // `hosts` list is byte-identical to the single-host first deploy INCLUDING
        // its pre-start migration.
        let fleet = fleet_of(&["203.0.113.10"]);
        let recorder = fleet::test_support::FleetRecorder::new()
            .script(
                "203.0.113.10",
                "proxy-compat-probe",
                compatible_deploy_help(),
            )
            .script("203.0.113.10", "detect-current", first_deploy_probe())
            .script("203.0.113.10", "probe-release-dir", "absent");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("a scripted one-host first deploy runs cleanly");

        let observed = recorder.calls_for("203.0.113.10");
        let (_, mutating) = observed.split_at(READ_ONLY_PROBES.len());
        assert_eq!(
            mutating,
            fixture
                .todays_calls(
                    &fleet.hosts[0],
                    fleet::HostMode::First,
                    exec::MigrateStep::Run
                )
                .as_slice(),
            "a one-host first deploy must be byte-identical to today's single-host first deploy"
        );
        assert!(
            recorder.run_labels_for("203.0.113.10").contains(&"migrate"),
            "the first-deploy path migrates before the release takes traffic (#1607)"
        );
    }

    #[test]
    fn rolling_order_replaces_hosts_strictly_in_sequence() {
        // #1621 (AC-2, T1.11). The whole safety claim of a rolling deploy is an
        // ORDERING claim: host k+1 is not touched until host k has cut over, so the
        // rest of the fleet keeps serving throughout. Asserted on the fleet-wide
        // tape, which is the only structure that can express it.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("a scripted three-host fleet rolls out cleanly");

        // (a) each host runs its OWN complete redeploy vector; only the first
        // carries the fleet's single migration.
        for (index, host) in hosts.iter().enumerate() {
            let expected: Vec<&'static str> = READ_ONLY_PROBES
                .iter()
                .copied()
                .chain(
                    REDEPLOY_RUN_LABELS
                        .iter()
                        .copied()
                        .filter(|l| index == 0 || *l != "migrate"),
                )
                .collect();
            assert_eq!(
                recorder.run_labels_for(host),
                expected,
                "{host} must run the full per-host sequence ({}the migration)",
                if index == 0 { "with " } else { "without " }
            );
        }

        // (b) strict sequencing: host k+1's first MUTATING op comes after host k's
        // proxy-flip. (Read-only probes all run up front, before any host is
        // touched — that is the all-hosts preflight probe, not a mutation.)
        for pair in hosts.windows(2) {
            let (previous, next) = (pair[0], pair[1]);
            let flip = recorder
                .index_of(previous, "proxy-flip")
                .unwrap_or_else(|| panic!("{previous} must cut over"));
            let first_mutation = recorder
                .first_mutating(next, &READ_ONLY_PROBES)
                .unwrap_or_else(|| panic!("{next} must be deployed"));
            assert!(
                flip < first_mutation,
                "{next} must not be touched until {previous} has cut over: \
                 {previous}'s proxy-flip at {flip}, {next}'s first mutation at {first_mutation}"
            );
        }
    }

    #[test]
    fn unreadable_marker_on_any_host_aborts_before_any_mutation() {
        // #1621 (AC-7, T1.17). Every per-host fail-closed refusal is evaluated for
        // ALL hosts BEFORE the first host is touched. Host 3's `shared/proxy-options`
        // marker is present but unparseable (#2074), so the whole rollout is
        // refused — with hosts 1 and 2 untouched, still serving their old releases.
        let fleet = fleet_of(&["web-a", "web-b", "web-c"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-b"] {
            recorder = script_redeploy(recorder, host);
        }
        recorder = recorder
            .script("web-c", "proxy-compat-probe", compatible_deploy_help())
            .script("web-c", "detect-current", unreadable_proxy_options_probe())
            .script("web-c", "probe-release-dir", "absent");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("an unprovable proxy-options marker must refuse the rollout");
        let message = err.to_string();
        assert!(
            message.contains("web-c"),
            "the refusal must name the offending HOST, got: {message}"
        );
        assert!(
            message.contains("proxy-options") && message.contains("#2074"),
            "the refusal must keep the single-host #2074 message, got: {message}"
        );

        let mutating = recorder.mutating(&READ_ONLY_PROBES);
        assert!(
            mutating.is_empty(),
            "a refused rollout must mutate NOTHING anywhere in the fleet, got: {mutating:?}"
        );
    }

    #[test]
    fn existing_release_dir_on_any_host_refuses_the_rollout() {
        // #1621 (AC-3, T1.19). The release id has one-second granularity, so a fast
        // retry can reuse it. Re-uploading into a dir `shared/previous-release`
        // still points at would make the "previous release" hold the NEW binary and
        // roll FORWARD. Refuse before touching anything, naming the host.
        let fleet = fleet_of(&["web-a", "web-b", "web-c"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-c"] {
            recorder = script_redeploy(recorder, host);
        }
        recorder = recorder
            .script("web-b", "proxy-compat-probe", compatible_deploy_help())
            .script("web-b", "detect-current", redeploy_probe())
            .script("web-b", "probe-release-dir", "present");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("an existing release dir must refuse the rollout");
        let message = err.to_string();
        assert!(
            message.contains("web-b") && message.contains(FLEET_RELEASE_ID),
            "the refusal must name the host and the colliding release, got: {message}"
        );

        let mutating = recorder.mutating(&READ_ONLY_PROBES);
        assert!(
            mutating.is_empty(),
            "a refused rollout must mutate NOTHING anywhere in the fleet, got: {mutating:?}"
        );
    }

    #[test]
    fn a_failed_host_halts_the_rollout_and_leaves_later_hosts_untouched() {
        // #1621 (AC-3). Host 2's readiness gate fails, a pre-boundary failure, so its
        // candidate is torn down by the per-host auto-rollback and its old release keeps
        // serving. The rollout then halts: host 3 is never touched. Host 1 is left on
        // the new release and named, so the operator can reverse it.
        //
        // Slice 4 pins this against the halt-and-freeze seam (`auto_rollback: false`,
        // the internal form of `--no-rollback`): freezing is exactly the "stop and let
        // me look" behaviour, so it must keep reporting partial state truthfully. The
        // default path, which compensates web-a instead of leaving it forward, is
        // `a_pre_boundary_failure_rolls_the_already_cut_over_hosts_back`.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = recorder.fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input_frozen(&fleet), |cfg| {
            Ok(recorder.executor(cfg))
        })
        .expect_err("a mid-rollout failure must halt the rollout");

        let halt = fleet_halt_of(&err);
        assert!(
            halt.degraded.is_empty() && halt.manual.is_empty(),
            "a clean pre-boundary halt needs no degraded/manual rows: {:?} / {:?}",
            halt.degraded,
            halt.manual
        );
        assert_eq!(
            halt.failed_host, "web-b",
            "the halt must name the failing host"
        );
        assert_eq!(
            halt.failed_step, "readiness-gate",
            "the halt must name the failing step"
        );
        assert_eq!(
            halt.rolled_back,
            vec!["web-b".to_owned()],
            "a pre-boundary failure rolls the failing host's candidate back"
        );
        assert!(
            halt.torn_down.is_empty(),
            "a redeploy host is rolled back, not torn down: {:?}",
            halt.torn_down
        );
        assert_eq!(
            halt.still_on_new,
            vec!["web-a".to_owned()],
            "the already-cut-over hosts must be named so they can be compensated"
        );

        // Host 1 completed its cutover.
        assert!(
            recorder.run_labels_for("web-a").contains(&"prune"),
            "web-a must have completed its cutover"
        );
        // Host 2 stopped at the readiness gate and tore its candidate down.
        let web_b = recorder.run_labels_for("web-b");
        assert!(
            web_b.contains(&"readiness-gate")
                && !web_b.contains(&"proxy-flip")
                && web_b.contains(&"teardown-candidate-unit")
                && web_b.contains(&"teardown-candidate-dir"),
            "web-b must fail at the gate, never flip, and tear its candidate down: {web_b:?}"
        );
        // Host 3 was probed in the all-hosts prologue and then never touched.
        let web_c: Vec<_> = recorder
            .mutating(&READ_ONLY_PROBES)
            .into_iter()
            .filter(|(host, _)| host == "web-c")
            .collect();
        assert!(
            web_c.is_empty(),
            "the rollout must halt: web-c must be mutated in no way, got: {web_c:?}"
        );
    }

    // ── Fleet compensation (issue #1621, slice 4, §4.6-4.7) ──────────────────

    #[test]
    fn a_pre_boundary_failure_rolls_the_already_cut_over_hosts_back() {
        // #1621 (AC-3, T1.13 case a). Host 2's readiness gate fails BEFORE its flip,
        // so its own auto-rollback tears the candidate down and its old release
        // keeps serving — but host 1 has already cut over. Left alone, the fleet
        // runs two versions at rest, which is the exact failure AC-3 forbids. So the
        // rollout compensates host 1 with the UNCHANGED on-demand rollback
        // primitives, and host 3 — never reached — stays untouched.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder =
            script_compensation(recorder, "web-a", "present").fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a mid-rollout failure must halt the rollout");

        // Host 1 ran its full cutover and then the full rollback sequence — the same
        // ops `autumn deploy rollback` runs, in the same order, nothing invented.
        assert_eq!(
            recorder.run_labels_for("web-a"),
            redeploy_then_compensated(true),
            "web-a must be rolled back with the on-demand rollback primitives"
        );
        assert!(
            recorder
                .mutating(&READ_ONLY_PROBES)
                .iter()
                .all(|(host, _)| host != "web-c"),
            "web-c was never reached, so compensation must not touch it either"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(
            halt.rolled_back,
            vec!["web-a".to_owned(), "web-b".to_owned()],
            "both the compensated host and the self-torn-down host are back on their \
             previous release"
        );
        assert!(
            halt.still_on_new.is_empty() && halt.torn_down.is_empty() && halt.manual.is_empty(),
            "the fleet converged: nothing left forward, nothing manual — got {:?} / {:?} / {:?}",
            halt.still_on_new,
            halt.torn_down,
            halt.manual
        );
    }

    #[test]
    fn a_post_boundary_functional_failure_compensates_newest_first_including_itself() {
        // #1621 (AC-3, T1.13 case b). Host 2's ssh transport drops during `drain-old`
        // — AFTER its flip, so traffic already moved and no teardown ran. That host
        // IS on the new release, so the compensation set includes it, and the order
        // is strictly newest-cutover-first: undoing from the newest end shrinks the
        // mixed window instead of widening it.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = script_compensation(recorder, "web-a", "present");
        recorder =
            script_compensation(recorder, "web-b", "present").transport_fail("web-b", "drain-old");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a post-boundary failure must halt the rollout");

        // Reverse order, asserted on the fleet-wide tape: `restart-previous` belongs
        // only to `rollback_ops`, so it is the unambiguous marker of each host's
        // compensation.
        let web_b = recorder
            .index_of("web-b", "restart-previous")
            .expect("web-b must be compensated — it is live on the new release");
        let web_a = recorder
            .index_of("web-a", "restart-previous")
            .expect("web-a must be compensated");
        assert!(
            web_b < web_a,
            "compensation must run in REVERSE rollout order: web-b's rollback at \
             {web_b}, web-a's at {web_a}"
        );
        assert!(
            recorder.index_of("web-c", "restart-previous").is_none(),
            "web-c never cut over, so it must not be rolled back"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(halt.failed_host, "web-b");
        assert_eq!(
            halt.failed_step, "ssh-transport",
            "a dropped transport attributes to the transport, never to a fabricated \
             step label"
        );
        assert_eq!(
            halt.rolled_back,
            vec!["web-a".to_owned(), "web-b".to_owned()],
            "the post-boundary host is compensated TOO — it is on the new release"
        );
        assert!(
            halt.still_on_new.is_empty(),
            "the fleet must converge on one version: {:?}",
            halt.still_on_new
        );
    }

    #[test]
    fn a_completed_first_deploy_is_compensated_by_teardown_not_rollback() {
        // #1621 (AC-3, T1.13 case c). A first-deploy host has no
        // `shared/previous-release` marker, so there is nothing to roll back TO:
        // `resolve_rollback_target` would fail. Its honest compensation is the
        // first-deploy teardown, which returns it to nothing-installed — the state
        // that makes the next `deploy up` correctly take the First path again. A
        // half-installed host an LB may already be probing is worse than a clean
        // absence.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_first_deploy(recorder, "web-a");
        recorder = script_redeploy(recorder, "web-b").fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a mid-rollout failure must halt the rollout");

        let web_a = recorder.run_labels_for("web-a");
        for teardown in [
            "teardown-candidate-unit",
            "teardown-candidate-dir",
            "teardown-current-symlink",
            "teardown-slot-markers",
        ] {
            assert!(
                web_a.contains(&teardown),
                "a compensated first deploy must run `{teardown}`: {web_a:?}"
            );
        }
        assert!(
            !web_a.contains(&"resolve-previous") && !web_a.contains(&"restart-previous"),
            "a first-deploy host has no previous release, so it must never be sent \
             through the rollback primitives: {web_a:?}"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(
            halt.torn_down,
            vec!["web-a".to_owned()],
            "the compensated first deploy is reported as torn down, not rolled back"
        );
        assert_eq!(
            halt.rolled_back,
            vec!["web-b".to_owned()],
            "the failing redeploy host's own candidate teardown still stands"
        );
        assert!(
            halt.still_on_new.is_empty(),
            "nothing may be left on the new release"
        );
    }

    #[test]
    fn a_compensated_first_deploy_stops_reporting_a_successful_last_deploy() {
        // #1621 (AC-6, audit gap G3). `CompensatedTeardown` returns web-a to
        // nothing-installed, but its first deploy had already written
        // `shared/last-deploy = deployed <ts>` via `record-proxy-options`. Left alone,
        // `deploy status` would show `last deploy: deployed <ts>` for a host carrying no
        // release at all — the worst possible moment for a wrong value, since the
        // operator is inspecting the fleet precisely because the rollout halted. The
        // teardown's last op rewrites the marker to `torn down`, so the column reports
        // what actually happened.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_first_deploy(recorder, "web-a");
        recorder = script_redeploy(recorder, "web-b").fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a mid-rollout failure must halt the rollout");

        let calls = recorder.calls_for("web-a");
        let last_deploy_writes: Vec<(&'static str, &str)> = calls
            .iter()
            .filter_map(|call| match call {
                exec::test_support::RecordedCall::Run { label, shell }
                    if shell.contains("shared/last-deploy") =>
                {
                    Some((*label, shell.as_str()))
                }
                _ => None,
            })
            .collect();

        let (last_label, last_shell) = *last_deploy_writes
            .last()
            .expect("the compensated host writes the last-deploy marker at least once");
        assert_eq!(
            last_label, "teardown-last-deploy",
            "the TEARDOWN must have the last word on this host's last-deploy marker, \
             got: {last_deploy_writes:?}"
        );
        assert!(
            last_shell.contains("'torn down'") && !last_shell.contains("'deployed'"),
            "a torn-down host must not report a successful deploy: {last_shell}"
        );
        // Non-vacuous: the first deploy really did claim `deployed` before the
        // compensation ran, so this is a correction, not an absence.
        assert!(
            last_deploy_writes
                .iter()
                .any(|(label, shell)| *label == "record-proxy-options"
                    && shell.contains("'deployed'")),
            "the first deploy must have recorded `deployed` first: {last_deploy_writes:?}"
        );
    }

    #[test]
    fn a_failed_compensation_does_not_stop_the_earlier_hosts() {
        // #1621 (AC-3, T1.13 case d). Host 3 fails post-boundary; compensation runs
        // c → b → a and host 2's rollback fails at `restart-previous`. Stopping there
        // would leave MORE hosts on the new release — a BIGGER mixed fleet — so the
        // driver continues to host 1 and reports host 2 honestly. This is the one
        // place the repo's `let _ = run_one(..)` teardown idiom must NOT be reused:
        // a swallowed compensation failure is how a fleet silently stays mixed.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = script_compensation(recorder, "web-a", "present");
        recorder =
            script_compensation(recorder, "web-b", "present").fail("web-b", "restart-previous");
        recorder =
            script_compensation(recorder, "web-c", "present").transport_fail("web-c", "drain-old");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a post-boundary failure must halt the rollout");

        // Host 2's rollback failed at or before the flip, so its restarted slot was
        // disabled again and it is still serving the NEW release.
        let web_b = recorder.run_labels_for("web-b");
        assert!(
            web_b.contains(&"restart-previous")
                && !web_b
                    .iter()
                    .skip_while(|l| **l != "restart-previous")
                    .any(|l| *l == "commit-markers")
                && web_b.contains(&"teardown-rollback-slot"),
            "web-b's rollback must fail before its flip and disable the slot it \
             restarted: {web_b:?}"
        );
        // …and host 1 is still compensated afterwards.
        let web_a = recorder
            .index_of("web-a", "restart-previous")
            .expect("web-a must still be compensated after web-b's rollback failed");
        let web_b_attempt = recorder
            .index_of("web-b", "restart-previous")
            .expect("web-b's rollback was attempted");
        assert!(
            web_b_attempt < web_a,
            "the failed rollback must not stop the earlier host: web-b at \
             {web_b_attempt}, web-a at {web_a}"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(
            halt.rolled_back,
            vec!["web-a".to_owned(), "web-c".to_owned()],
            "every host whose compensation SUCCEEDED is reported as rolled back"
        );
        assert_eq!(
            halt.still_on_new,
            vec!["web-b".to_owned()],
            "a host whose compensation failed is still on the new release and must be \
             named as such — reporting the attempt instead of the result is how an \
             operator believes a mixed fleet converged"
        );
    }

    #[test]
    fn a_missing_rollback_target_dir_declines_the_rollback() {
        // #1621 (AC-3, T1.13 case e / §4.7). `prune` runs per host, so hosts with
        // divergent deploy history legitimately retain different release sets: the
        // previous-release marker can name a dir this host no longer has. Flipping
        // at it anyway starts a unit whose ExecStart points nowhere, passes systemd,
        // and fails the readiness gate POST-boundary with no teardown — one broken
        // host becomes two. So the precheck declines, and the host is reported.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-b"] {
            recorder = script_redeploy(recorder, host);
        }
        recorder = script_compensation(recorder, "web-a", "absent").fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a mid-rollout failure must halt the rollout");

        let web_a = recorder.run_labels_for("web-a");
        assert!(
            web_a.contains(&"probe-rollback-target"),
            "the precheck must actually run: {web_a:?}"
        );
        assert!(
            !web_a.contains(&"restart-previous") && !web_a.contains(&"drain-rolled-back-slot"),
            "a declined rollback must run NO rollback ops: {web_a:?}"
        );
        assert_eq!(
            web_a.iter().filter(|label| **label == "proxy-flip").count(),
            1,
            "the declined host must keep exactly its cutover flip — zero further \
             flips: {web_a:?}"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(
            halt.manual,
            vec![("web-a".to_owned(), fleet::MANUAL_ROLLBACK_TARGET_MISSING)],
            "the declined host must be reported with its reason"
        );
        assert_eq!(
            halt.still_on_new,
            vec!["web-a".to_owned()],
            "a host that was NOT rolled back is still on the new release"
        );
        assert_eq!(
            halt.rolled_back,
            vec!["web-b".to_owned()],
            "the failing host's own teardown still stands"
        );
    }

    #[test]
    fn a_commit_markers_failure_is_never_auto_rolled_back() {
        // #1621 (§4.6, T1.15). `commit-markers` writes previous-release + `current` +
        // live-slot as ONE remote transaction. A failure inside it can leave any
        // subset applied, so `resolve_rollback_target` may name the WRONG release —
        // rolling back could move that host FORWARD, or onto a release it never
        // served. Fail closed: halt, do not touch that host, print the by-hand
        // recovery. Earlier hosts are still compensated.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-b"] {
            recorder = script_redeploy(recorder, host);
        }
        recorder =
            script_compensation(recorder, "web-a", "present").fail("web-b", "commit-markers");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("an ambiguous-marker failure must halt the rollout");

        let web_b = recorder.run_labels_for("web-b");
        for rollback_op in [
            "resolve-previous",
            "probe-rollback-target",
            "restart-previous",
            "drain-rolled-back-slot",
        ] {
            assert!(
                !web_b.contains(&rollback_op),
                "an ambiguous-marker host must see ZERO rollback-flavoured ops, got \
                 `{rollback_op}`: {web_b:?}"
            );
        }
        assert_eq!(
            recorder.run_labels_for("web-a"),
            redeploy_then_compensated(true),
            "the earlier host is still compensated"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(halt.failed_step, "commit-markers");
        assert_eq!(
            halt.manual,
            vec![("web-b".to_owned(), fleet::MANUAL_AMBIGUOUS_MARKERS)],
            "the ambiguous host must be reported as needing a human"
        );
        assert_eq!(
            halt.still_on_new,
            vec!["web-b".to_owned()],
            "it was not rolled back, so it is still on the new release"
        );
        assert_eq!(halt.rolled_back, vec!["web-a".to_owned()]);
    }

    #[test]
    fn a_transport_failure_at_commit_markers_is_never_auto_rolled_back() {
        // #1621 (§4.6, T1.15). The guard above must key on the OP that was running,
        // not on the error shape that carried the failure. `DeployExecError::Spawn`
        // carries no label of its own (`failed_step_label` synthesizes
        // "ssh-transport"), so a dropped transport during `commit-markers` used to
        // classify as Functional and auto-roll-back a host whose marker triple was
        // never written — landing it on whatever `shared/previous-release` still
        // named, which can be a release older than the one every other compensated
        // host returns to, and reporting it as "previous release restored".
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-b"] {
            recorder = script_redeploy(recorder, host);
        }
        recorder = script_compensation(recorder, "web-a", "present")
            .transport_fail("web-b", "commit-markers");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("an ambiguous-marker failure must halt the rollout");

        let web_b = recorder.run_labels_for("web-b");
        for rollback_op in [
            "resolve-previous",
            "probe-rollback-target",
            "restart-previous",
            "drain-rolled-back-slot",
        ] {
            assert!(
                !web_b.contains(&rollback_op),
                "a host whose `commit-markers` failed must see ZERO rollback-flavoured \
                 ops whatever the error shape, got `{rollback_op}`: {web_b:?}"
            );
        }
        assert_eq!(
            recorder.run_labels_for("web-a"),
            redeploy_then_compensated(true),
            "the earlier host is still compensated"
        );

        let halt = fleet_halt_of(&err);
        assert_eq!(
            halt.manual,
            vec![("web-b".to_owned(), fleet::MANUAL_AMBIGUOUS_MARKERS)],
            "the host with an unprovable marker triple must be reported as needing a human"
        );
        assert_eq!(
            halt.still_on_new,
            vec!["web-b".to_owned()],
            "it was not rolled back, so it is still on the new release"
        );
        assert_eq!(halt.rolled_back, vec!["web-a".to_owned()]);
    }

    #[test]
    fn a_housekeeping_failure_degrades_the_host_and_continues_the_rollout() {
        // #1621 (§4.6, T1.15). `prune` runs AFTER the flip: the proxy is already
        // serving the new release, so a failed `rm -rf` of old release dirs affects
        // no traffic at all. Rolling three hosts back over it would be a
        // self-inflicted outage. The rollout warns, marks the host degraded, and
        // CONTINUES — and the deploy SUCCEEDS, because every host really is serving
        // the new release.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = recorder.fail("web-b", "prune");
        let fixture = FleetFixture::new();

        run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect("a post-cutover housekeeping failure must not fail the rollout");

        assert_eq!(
            recorder.run_labels_for("web-c"),
            READ_ONLY_PROBES
                .iter()
                .copied()
                .chain(
                    REDEPLOY_RUN_LABELS
                        .iter()
                        .copied()
                        .filter(|label| *label != "migrate")
                )
                .collect::<Vec<_>>(),
            "the rollout must CONTINUE past a degraded host"
        );
        let web_b = recorder.run_labels_for("web-b");
        assert!(
            web_b.contains(&"prune")
                && !web_b.contains(&"teardown-candidate-unit")
                && !web_b.contains(&"restart-previous"),
            "a degraded host is live and healthy: nothing may be torn down or rolled \
             back on it: {web_b:?}"
        );
        assert!(
            recorder.positions_of("resolve-previous").is_empty(),
            "a rollout that only degraded must compensate NOTHING anywhere"
        );
    }

    #[test]
    fn compensation_never_touches_the_schema() {
        // #1621 (AC-3, T1.12). Auto-rollback is BINARY-ONLY. `rollback_ops` and
        // `first_deploy_teardown_ops` contain no `migrate` op by construction, and
        // that must stay true as they evolve: `rolling_deploy_blocked` grades
        // `up.sql` only, so an automatic `migrate down` would run exactly the SQL
        // nothing reviews, unattended, mid-flip. The error text says so too, because
        // an operator watching binaries go back will otherwise assume the schema did.
        let hosts = ["web-a", "web-b"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = script_compensation(recorder, "web-a", "present");
        recorder =
            script_compensation(recorder, "web-b", "present").transport_fail("web-b", "drain-old");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a post-boundary failure must halt the rollout");

        // Exactly one `migrate` in the WHOLE run — the fleet's single scheduled one,
        // on the first host's cutover — and none of it after compensation began.
        let migrations = recorder.positions_of("migrate");
        assert_eq!(
            migrations.len(),
            1,
            "the fleet migrates once and compensation adds none: {migrations:?}"
        );
        let compensation_started = recorder
            .positions_of("resolve-previous")
            .first()
            .copied()
            .expect("compensation ran");
        assert!(
            migrations[0] < compensation_started,
            "the only migration must predate compensation: migrate at {}, compensation \
             from {compensation_started}",
            migrations[0]
        );
        assert!(
            err.to_string().contains("NOT rolled back"),
            "the halt must state that the schema did not come back with the binaries: {err}"
        );
    }

    #[test]
    fn a_single_host_failure_keeps_todays_error_and_compensates_nothing() {
        // #1621 (AC-1). N = 1 is exempt from the whole fleet vocabulary: a
        // single-host deploy that fails must return the pre-#1621 error verbatim —
        // no classification, no degrade-and-continue, no compensation, no state
        // table. `prune` is the sharpest case: in a FLEET it degrades and the deploy
        // succeeds, while for one host it must stay exactly today's failure.
        let fleet = fleet_of(&["203.0.113.10"]);
        let recorder = script_redeploy(fleet::test_support::FleetRecorder::new(), "203.0.113.10")
            .fail("203.0.113.10", "prune");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a single-host post-boundary failure must still fail the deploy");

        match &err {
            DeployError::Exec(message) => assert_eq!(
                *message,
                exec::DeployExecError::CommandFailed {
                    label: "prune",
                    message: "scripted failure".to_owned(),
                }
                .to_string(),
                "the single-host path must surface the raw executor error verbatim"
            ),
            other => panic!("N = 1 must never produce fleet vocabulary, got: {other:?}"),
        }
        assert_eq!(
            recorder.run_labels_for("203.0.113.10"),
            READ_ONLY_PROBES
                .iter()
                .copied()
                .chain(REDEPLOY_RUN_LABELS)
                .collect::<Vec<_>>(),
            "the one host runs today's exact sequence and nothing more — no \
             compensation, no extra probe"
        );
    }

    // ── Fleet-wide fail-closed refusals (issue #1621, slice 5, §4.8) ─────────

    /// A resolved fleet with `[deploy.tls]` enabled for a public hostname — the
    /// shape §4.8 refuses for more than one host.
    fn tls_fleet_of(hosts: &[&str]) -> ResolvedFleet {
        ResolvedFleet::resolve(
            &DeployConfig {
                hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
                tls: autumn_web::config::DeployTlsConfig {
                    enabled: true,
                    host: Some("shop.example.com".to_owned()),
                },
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("a well-formed TLS fleet resolves")
    }

    #[test]
    fn the_fleet_refusals_name_their_config_key_and_spare_a_single_host() {
        // #1621 (§4.8, T1.22). Three topologies that cannot be deployed safely to
        // more than one host at all. Each refusal is a PURE predicate over the
        // CONFIGURED host count, so it is decided before any executor exists — and
        // each names the config key the operator must change plus the tracking
        // issue, matching `reject_sqlite_sharding_topology`'s house style.
        let sqlite = refuse_sqlite_fleet(3, true).expect_err("a sqlite fleet must be refused");
        assert!(
            sqlite.contains("sqlite") && sqlite.contains("#1621"),
            "the sqlite refusal must name the backend and the issue: {sqlite}"
        );
        assert!(
            sqlite.contains("[database]"),
            "the sqlite refusal must name the config section to change: {sqlite}"
        );

        let media = refuse_media_fleet(3, true).expect_err("a media fleet must be refused");
        assert!(
            media.contains("[media.mediamtx]") && media.contains("#1621"),
            "the media refusal must name the config key and the issue: {media}"
        );

        let tls = refuse_tls_fleet(3, true).expect_err("a TLS fleet must be refused");
        assert!(
            tls.contains("[deploy.tls]") && tls.contains("#1621"),
            "the TLS refusal must name the config key and the issue: {tls}"
        );
        assert!(
            tls.contains("load balancer"),
            "the TLS refusal must point at LB-terminated TLS: {tls}"
        );

        // None of the three constrains a single-host deploy — that is exactly
        // today's supported topology (AC-1).
        assert!(refuse_sqlite_fleet(1, true).is_ok());
        assert!(refuse_media_fleet(1, true).is_ok());
        assert!(refuse_tls_fleet(1, true).is_ok());
        // Nor a fleet that does not have the offending feature turned on.
        assert!(refuse_sqlite_fleet(3, false).is_ok());
        assert!(refuse_media_fleet(3, false).is_ok());
        assert!(refuse_tls_fleet(3, false).is_ok());
    }

    #[test]
    fn writable_db_is_sqlite_reads_the_url_the_deploy_would_ship() {
        // The refusal grades the SAME url `build_env_file` writes to every host, so
        // it must follow the primary/compat/shard resolution rather than peeking at
        // one field. Reduced to a bool here so the URL never reaches the driver.
        use autumn_web::config::DatabaseConfig;
        let sqlite = DatabaseConfig {
            url: Some("sqlite://app.db".to_owned()),
            ..DatabaseConfig::default()
        };
        assert!(writable_db_is_sqlite(&sqlite));

        let postgres = DatabaseConfig {
            url: Some("postgres://user:pw@db.internal/app".to_owned()),
            ..DatabaseConfig::default()
        };
        assert!(!writable_db_is_sqlite(&postgres));
        assert!(!writable_db_is_sqlite(&DatabaseConfig::default()));
    }

    #[test]
    fn a_sqlite_fleet_is_refused_before_any_remote_command() {
        // #1621 (T1.22). `build_env_file` ships a byte-identical database URL to
        // every host, so a `sqlite://` fleet is N independent database FILES — no
        // shared schema, no advisory lock, and every host serving a different
        // dataset behind one load balancer. Refused with ZERO remote commands: not
        // even the read-only probes run.
        let fleet = fleet_of(&["web-a", "web-b", "web-c"]);
        let recorder = fleet::test_support::FleetRecorder::new();
        let fixture = FleetFixture::new();
        let input = FleetUpInput {
            db: FleetDatabaseFacts {
                sqlite: true,
                ..FleetDatabaseFacts::default()
            },
            ..fixture.input(&fleet)
        };

        let err = run_up_with(&input, |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a sqlite fleet must be refused");

        match &err {
            DeployError::Config(message) => assert!(
                message.contains("sqlite") && message.contains("#1621"),
                "the refusal must name the backend and the issue: {message}"
            ),
            other => panic!("expected a config refusal, got: {other:?}"),
        }
        assert!(
            recorder.mutating(&[]).is_empty(),
            "a refused fleet must run NO remote command at all, probes included: {:?}",
            recorder.mutating(&[])
        );
    }

    #[test]
    fn a_media_fleet_is_refused_before_any_remote_command() {
        // #1621 (T1.22). `provision_media_host` has no teardown or rollback path by
        // design, so fanning it out gives N MediaMTX daemons on identical ports with
        // divergent recording sets and nothing to undo them with when the app
        // rollout is compensated. Refused in the prologue — which is also what makes
        // the driver's single unguarded `executors[0]` media call correct.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let recorder = fleet::test_support::FleetRecorder::new();
        let mut fixture = FleetFixture::new();
        fixture.media_cfg.enabled = true;
        let input = fixture.input(&fleet);

        let err = run_up_with(&input, |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a media-enabled fleet must be refused");

        match &err {
            DeployError::Config(message) => assert!(
                message.contains("[media.mediamtx]") && message.contains("#1621"),
                "the refusal must name the config key and the issue: {message}"
            ),
            other => panic!("expected a config refusal, got: {other:?}"),
        }
        assert!(
            recorder.mutating(&[]).is_empty(),
            "a refused fleet must run NO remote command at all, probes included: {:?}",
            recorder.mutating(&[])
        );
    }

    #[test]
    fn a_tls_fleet_is_refused_before_any_remote_command() {
        // #1621 (T1.22). kamal-proxy is PER HOST, so N hosts would each attempt ACME
        // for the same `[deploy.tls] host` from behind the operator's load balancer:
        // only one can answer a challenge and the rest burn Let's Encrypt rate
        // limits. Behind a load balancer, TLS terminates at the load balancer.
        let fleet = tls_fleet_of(&["web-a", "web-b"]);
        let recorder = fleet::test_support::FleetRecorder::new();
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a TLS-enabled fleet must be refused");

        match &err {
            DeployError::Config(message) => assert!(
                message.contains("[deploy.tls]") && message.contains("load balancer"),
                "the refusal must name the config key and point at LB-terminated TLS: \
                 {message}"
            ),
            other => panic!("expected a config refusal, got: {other:?}"),
        }
        assert!(
            recorder.mutating(&[]).is_empty(),
            "a refused fleet must run NO remote command at all, probes included: {:?}",
            recorder.mutating(&[])
        );
    }

    #[test]
    fn the_refusals_grade_the_configured_topology_not_the_narrowed_rollout() {
        // #1621 (§4.8). `--only` narrows which hosts a run TOUCHES; it does not
        // change the topology. A three-host sqlite fleet is just as broken when one
        // host is deployed, so the refusal is keyed on the CONFIGURED count and
        // `--only` can never be used to sneak past it.
        let fleet = fleet_of(&["web-b"]);
        let recorder = fleet::test_support::FleetRecorder::new();
        let fixture = FleetFixture::new();
        let input = FleetUpInput {
            configured_host_count: 3,
            db: FleetDatabaseFacts {
                sqlite: true,
                ..FleetDatabaseFacts::default()
            },
            ..fixture.input(&fleet)
        };

        let err = run_up_with(&input, |cfg| Ok(recorder.executor(cfg)))
            .expect_err("`--only` must not defeat a topology refusal");
        assert!(
            matches!(&err, DeployError::Config(m) if m.contains("sqlite")),
            "expected the sqlite refusal, got: {err:?}"
        );
    }

    #[test]
    fn auto_migrate_warns_for_a_fleet_and_stays_silent_for_one_host() {
        // #1621 (§4.8, last paragraph). A WARNING, not a refusal: with
        // `auto_migrate` on, every host applies migrations at BOOT, so the hosts
        // race each other mid-rollout and a checksum mismatch exits the process
        // under `Restart=on-failure` — a crash loop. Real hazard, not a certainty,
        // so it is said loudly and the deploy proceeds.
        let warning = fleet_auto_migrate_warning(3, Some(true), false)
            .expect("a fleet with auto_migrate on must be warned");
        assert!(
            warning.contains("auto_migrate") && warning.contains("#1621"),
            "the warning must name the key and the issue: {warning}"
        );
        assert!(
            warning.contains("boot") && warning.contains("crash loop"),
            "the warning must name the mechanism (boot races) and the symptom: {warning}"
        );

        // The back-compat alias is honoured, and `auto_migrate` supersedes it — the
        // runtime's own resolution rule.
        assert!(fleet_auto_migrate_warning(3, None, true).is_some());
        assert!(
            fleet_auto_migrate_warning(3, Some(false), true).is_none(),
            "an explicit `auto_migrate = false` wins over the alias"
        );

        // Silent for a single host (that is today's supported shape) and silent when
        // the app does not auto-migrate at all.
        assert!(fleet_auto_migrate_warning(1, Some(true), true).is_none());
        assert!(fleet_auto_migrate_warning(3, None, false).is_none());
    }

    #[test]
    fn per_host_job_and_scheduler_backends_warn_for_a_fleet_and_stay_silent_for_one_host() {
        // #1621. `[jobs] backend` defaults to `local`, an in-process queue, and
        // `[scheduler] backend` to `in_process`, a per-process timer. On one host that
        // is correct and is what every unconfigured app runs; on N hosts it silently
        // means N independent queues and N schedulers — enqueued work runs only on the
        // host that took the request, pending work dies with every `drain-old`,
        // `unique`/`concurrency` stop being enforced fleet-wide, and every cron fires N
        // times. A warning, never a refusal: these are the defaults, so refusing would
        // break every existing single-host user's scale-up (same rule as
        // `FLEET_AUTO_MIGRATE_WARNING`).
        let warning = fleet_shared_backend_warning(3, "local", true)
            .expect("a fleet on the in-process defaults must be warned");
        assert!(
            warning.contains("[jobs] backend")
                && warning.contains("[scheduler] backend")
                && warning.contains("#1621"),
            "the warning must name both keys and the issue: {warning}"
        );
        assert!(
            warning.to_lowercase().contains("per host") && warning.contains("drain"),
            "the warning must name the mechanism (a queue per host, drained away): {warning}"
        );

        // Each backend alone still warns, naming only the key that applies.
        let jobs_only = fleet_shared_backend_warning(2, "local", false)
            .expect("a per-host job queue alone must warn");
        assert!(
            jobs_only.contains("[jobs] backend") && !jobs_only.contains("[scheduler] backend"),
            "a durable scheduler must not be named as a problem: {jobs_only}"
        );
        let scheduler_only = fleet_shared_backend_warning(2, "postgres", true)
            .expect("a per-host scheduler alone must warn");
        assert!(
            scheduler_only.contains("[scheduler] backend")
                && !scheduler_only.contains("[jobs] backend"),
            "a durable job backend must not be named as a problem: {scheduler_only}"
        );

        // Silent for a single host — the in-process defaults are exactly right
        // there, and a warning on every `autumn deploy up` would be noise.
        assert!(fleet_shared_backend_warning(1, "local", true).is_none());
        // Silent for durable backends, whatever their spelling/case.
        assert!(fleet_shared_backend_warning(5, "postgres", false).is_none());
        assert!(fleet_shared_backend_warning(5, " Redis ", false).is_none());
    }

    // ── `--only` (issue #1621, slice 5, §3.2) ────────────────────────────────

    #[test]
    fn only_selects_in_declaration_order_and_rejects_an_unknown_host() {
        // #1621 (§3.2). `[deploy] hosts` order IS the rollout order, so the
        // selection is filtered from the CONFIGURED list rather than built from the
        // flag order: `--only web-c --only web-a` must not silently reverse a
        // rolling deploy. An unknown name is a hard error naming itself — guessing
        // (prefix, DNS) could deploy the wrong machine, and deploying nothing
        // silently would be worse.
        let configured: Vec<String> = ["web-a", "web-b", "web-c"]
            .iter()
            .map(|h| (*h).to_owned())
            .collect();

        assert_eq!(
            select_hosts(&configured, &[]).expect("an empty selection is the whole fleet"),
            configured,
            "no `--only` must select the whole fleet, in order"
        );
        assert_eq!(
            select_hosts(&configured, &["web-c".to_owned(), "web-a".to_owned()])
                .expect("a valid selection resolves"),
            vec!["web-a".to_owned(), "web-c".to_owned()],
            "the selection must keep DECLARATION order, not flag order"
        );
        assert_eq!(
            select_hosts(&configured, &["web-b".to_owned(), "web-b".to_owned()])
                .expect("a repeated name is not an error"),
            vec!["web-b".to_owned()],
            "a repeated `--only` selects that host once"
        );

        let err = select_hosts(&configured, &["web-9".to_owned()])
            .expect_err("an unknown host must be refused");
        assert!(
            err.contains("web-9") && err.contains("--only"),
            "the error must quote the unmatched value and the flag: {err}"
        );
        for host in &configured {
            assert!(
                err.contains(host.as_str()),
                "the error must list the configured hosts so the operator can fix the \
                 typo: {err}"
            );
        }
    }

    #[test]
    fn only_restricts_the_rollout_and_moves_the_migration_with_it() {
        // #1621 (§3.2). `--only web-b` on a three-host fleet touches web-b and nothing
        // else — not even the read-only probes run against the skipped hosts, which is
        // deliberate: an excluded host's drifted marker must not refuse a repair deploy
        // aimed at a different host. Migrate placement is computed over the selected
        // hosts, so web-b, the first redeploy in this rollout, migrates; over the full
        // fleet web-a would have. That is the honest reading of "exactly once" for a
        // narrowed rollout — the schema is fleet-wide, and the hosts left out are not
        // being deployed at all.
        let selected = select_hosts(
            &["web-a", "web-b", "web-c"]
                .iter()
                .map(|h| (*h).to_owned())
                .collect::<Vec<_>>(),
            &["web-b".to_owned()],
        )
        .expect("web-b is in the configured fleet");
        let fleet = ResolvedFleet::from_targets(&resolved_defaults(), &selected);
        let recorder = script_redeploy(fleet::test_support::FleetRecorder::new(), "web-b");
        let fixture = FleetFixture::new();
        let input = FleetUpInput {
            configured_host_count: 3,
            ..fixture.input(&fleet)
        };

        run_up_with(&input, |cfg| Ok(recorder.executor(cfg)))
            .expect("a narrowed rollout deploys the selected host");

        assert_eq!(
            recorder.run_labels_for("web-b"),
            READ_ONLY_PROBES
                .iter()
                .copied()
                .chain(REDEPLOY_RUN_LABELS)
                .collect::<Vec<_>>(),
            "the selected host runs the full redeploy, migration included"
        );
        for skipped in ["web-a", "web-c"] {
            assert!(
                recorder.calls_for(skipped).is_empty(),
                "`--only` must not touch {skipped} at all — not even a read-only probe"
            );
        }
    }

    #[test]
    fn a_narrowed_rollout_keeps_fleet_semantics_rather_than_the_single_host_path() {
        // #1621 (AC-1 boundary). The byte-identical pre-#1621 path is for a config
        // that declares ONE host. A three-host fleet narrowed to one is NOT that: the
        // other two hosts exist and are on another release, so the run keeps the
        // fleet vocabulary, the state table and the compensation semantics. Proven
        // by the error TYPE: the single-host path returns the raw executor error,
        // the fleet path a `FleetHalted` report.
        let fleet = fleet_of(&["web-b"]);
        let recorder = script_redeploy(fleet::test_support::FleetRecorder::new(), "web-b")
            .fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();
        let input = FleetUpInput {
            configured_host_count: 3,
            ..fixture.input(&fleet)
        };

        let err = run_up_with(&input, |cfg| Ok(recorder.executor(cfg)))
            .expect_err("the selected host's failure must fail the deploy");

        let halt = fleet_halt_of(&err);
        assert_eq!(halt.failed_host, "web-b");
        assert_eq!(halt.failed_step, "readiness-gate");
    }

    #[test]
    fn a_narrowed_probe_refusal_still_names_its_host() {
        // #1621 (AC-1 boundary, audit gap G10). "Is this really a single-host
        // deploy?" has ONE answer, `is_single_host_deploy`, and every site asks it
        // the same way. `probe_host_for_up` used to ask `fleet.is_single()` alone,
        // so a three-host fleet narrowed to one lost the `host {x}: ` prefix on a
        // probe-phase refusal — an unattributed error during a repair operation on
        // a fleet whose other hosts are on another release.
        let narrowed = fleet_of(&["web-b"]);
        let recorder = fleet::test_support::FleetRecorder::new()
            .script("web-b", "proxy-compat-probe", compatible_deploy_help())
            .script("web-b", "detect-current", redeploy_probe())
            .script("web-b", "probe-release-dir", "present");
        let fixture = FleetFixture::new();
        let input = FleetUpInput {
            configured_host_count: 3,
            ..fixture.input(&narrowed)
        };

        let narrowed_message = run_up_with(&input, |cfg| Ok(recorder.executor(cfg)))
            .expect_err("an existing release dir must refuse the rollout")
            .to_string();
        assert!(
            narrowed_message.contains("host web-b: release directory"),
            "a narrowed run keeps fleet attribution on probe refusals, got: \
             {narrowed_message}"
        );

        // …and a GENUINE one-host config is byte-identical to the pre-#1621 text:
        // no prefix, nothing else changed (AC-1).
        let single = fleet_of(&["web-b"]);
        let single_recorder = fleet::test_support::FleetRecorder::new()
            .script("web-b", "proxy-compat-probe", compatible_deploy_help())
            .script("web-b", "detect-current", redeploy_probe())
            .script("web-b", "probe-release-dir", "present");
        let single_message = run_up_with(&fixture.input(&single), |cfg| {
            Ok(single_recorder.executor(cfg))
        })
        .expect_err("an existing release dir must refuse the rollout")
        .to_string();
        assert!(
            !single_message.contains("host web-b: "),
            "a genuine single-host refusal must keep the unattributed pre-#1621 \
             text, got: {single_message}"
        );
        assert_eq!(
            narrowed_message.replace("host web-b: ", ""),
            single_message,
            "the two must differ ONLY by the attribution prefix"
        );
    }

    #[test]
    fn a_narrowed_preflight_still_scopes_its_reachability_row() {
        // #1621 (AC-1 boundary, audit gap G10). The same predicate governs the
        // preflight report: `deploy up --only web-2` on a three-host config is not
        // the single-host path, so its `ssh_reachability` row still names the host
        // it graded. A genuine one-host config stays unscoped (AC-1).
        let config = preflight_config();
        let narrowed = ResolvedFleet::resolve(&fleet_cfg(&["web-2.example.test"]), "myapp")
            .expect("a narrowed fleet resolves");

        let scoped = collect_fleet_preflight(&config, &narrowed, 3);
        assert_eq!(
            scoped
                .iter()
                .filter(|check| check.name == SSH_REACHABILITY_CHECK)
                .map(|check| check.scope.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("web-2.example.test")],
            "a narrowed run must keep host attribution: {:?}",
            scoped
                .iter()
                .map(|c| (c.name, &c.scope))
                .collect::<Vec<_>>()
        );

        let genuine = collect_fleet_preflight(&config, &narrowed, 1);
        assert!(
            genuine.iter().all(|check| check.scope.is_none()),
            "a genuine one-host config must stay unscoped (AC-1): {:?}",
            genuine
                .iter()
                .map(|c| (c.name, &c.scope))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_rollback_freezes_the_fleet_instead_of_compensating() {
        // #1621 (§4.7). `--no-rollback` is halt-and-FREEZE: the rollout stops and
        // reports, and every host is left exactly as it is for inspection. web-b
        // fails post-boundary (a dropped transport at `drain-old`, which carries no
        // op label and so classifies as FUNCTIONAL), so both web-a and web-b are on
        // the new release — and, unlike the default path, they STAY there.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = recorder.transport_fail("web-b", "drain-old");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input_frozen(&fleet), |cfg| {
            Ok(recorder.executor(cfg))
        })
        .expect_err("a post-boundary failure must fail the deploy");

        let halt = fleet_halt_of(&err);
        assert_eq!(
            halt.still_on_new,
            vec!["web-a".to_owned(), "web-b".to_owned()],
            "a frozen fleet reports every host left on the new release"
        );
        assert!(
            halt.rolled_back.is_empty() && halt.torn_down.is_empty(),
            "freezing compensates NOTHING: {:?} / {:?}",
            halt.rolled_back,
            halt.torn_down
        );
        // The observable proof: web-a flipped exactly once (its cutover). A
        // compensating rollback would flip it a second time.
        assert_eq!(
            recorder
                .positions_of("proxy-flip")
                .iter()
                .filter(|_| true)
                .count(),
            2,
            "exactly two flips happened (web-a's and web-b's cutovers) — no host was \
             flipped back"
        );
        assert!(
            !recorder
                .run_labels_for("web-a")
                .contains(&"restart-previous"),
            "a frozen rollout must not roll web-a back"
        );

        // And the flag is actually wired to the seam this test drives.
        assert!(
            include_str!("deploy.rs").contains("auto_rollback: !options.no_rollback"),
            "`--no-rollback` must map to the `auto_rollback` driver seam"
        );
    }

    // ── Fleet-wide `deploy rollback` (issue #1621, slice 5, §3.2) ────────────

    /// Script one host's previous-release marker for a fleet-wide rollback.
    fn script_previous_release(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
    ) -> fleet::test_support::FleetRecorder {
        recorder.script(
            host,
            "resolve-previous",
            format!("prev:{PREVIOUS_RELEASE_DIR}\tblue\t3001"),
        )
    }

    /// Drive a fleet-wide rollback against a scripted recorder.
    fn drive_fleet_rollback(
        fleet: &ResolvedFleet,
        recorder: &fleet::test_support::FleetRecorder,
    ) -> Result<(), DeployError> {
        let proxy = proxy::KamalProxyController::new(60);
        run_fleet_rollback_with(&[], fleet, &proxy, FLEET_PUBLIC_PORT, |cfg| {
            Ok(recorder.executor(cfg))
        })
    }

    // ── `deploy status` (issue #1621, AC-6) ──────────────────────────────────

    /// Full five-section probe stdout: redeploy on blue, proxy serving blue,
    /// public port 3000, TLS off, on `release`.
    fn status_probe(release: &str) -> String {
        format!(
            "redeploy:blue\t3001\n\
             ---autumn-kamal-proxy-list---\n\
             Service   Host          Target            State    TLS\n\
             myapp     example.com   127.0.0.1:3001    running  no\n\
             ---autumn-kamal-proxy-unit---\n--http-port 3000\n\
             ---autumn-kamal-proxy-options---\n0\t\n\
             ---autumn-current-release---\n/srv/autumn/myapp/releases/{release}"
        )
    }

    /// Script a host's two read-only status probes.
    /// The shared maintenance flag path a #1621 slot unit points
    /// `AUTUMN_MAINTENANCE_FLAG_FILE` at for the `myapp` fleet fixtures — the
    /// fourth `probe-host-status` section resolves to it on a healthy host.
    const STATUS_SHARED_FLAG: &str = "/srv/autumn/myapp/shared/autumn-maintenance.json";

    fn script_status(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
        release: &str,
        ready: &str,
        maintenance: bool,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(host, "detect-current", status_probe(release))
            .script(
                host,
                "probe-host-status",
                format!(
                    "{ready}\n---autumn-host-status---\n{flag}\n---autumn-host-status---\n\
                     deployed\t2026-07-14T12:00:03Z\n---autumn-host-status---\n\
                     {STATUS_SHARED_FLAG}\n{flag}",
                    flag = if maintenance { "maintenance-on" } else { "" }
                ),
            )
    }

    /// `script_status`, but with an explicit last-deploy marker body (AC-6's third
    /// fact) — `""` scripts a host whose marker is absent/unreadable.
    fn script_status_last_deploy(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
        release: &str,
        last_deploy: &str,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(host, "detect-current", status_probe(release))
            .script(
                host,
                "probe-host-status",
                format!(
                    "200\n---autumn-host-status---\n\n---autumn-host-status---\n{last_deploy}\n\
                     ---autumn-host-status---\n{STATUS_SHARED_FLAG}\n"
                ),
            )
    }

    fn drive_status(
        fleet: &ResolvedFleet,
        recorder: &fleet::test_support::FleetRecorder,
    ) -> Result<Vec<fleet::HostStatus>, DeployError> {
        collect_fleet_status(fleet, FLEET_PUBLIC_PORT, |cfg| Ok(recorder.executor(cfg)))
    }

    #[test]
    fn fleet_status_reports_every_host_read_only_and_in_declaration_order() {
        // #1621 (AC-6): `deploy status` is the surface an operator reaches for
        // mid-incident, so it (a) touches nothing, (b) reports EVERY host, and (c)
        // keeps declaration order — the same order a rollout would replace them in.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_status(recorder, host, "20260714T120000Z", "200", false);
        }

        let statuses =
            drive_status(&fleet, &recorder).expect("a read-only status never fails the command");

        assert_eq!(
            statuses.iter().map(|s| s.host.as_str()).collect::<Vec<_>>(),
            hosts.to_vec(),
            "status must keep `[deploy] hosts` order"
        );
        for host in hosts {
            assert_eq!(
                recorder.run_labels_for(host),
                vec!["detect-current", "probe-host-status"],
                "{host}: status must run ONLY the two read-only probes"
            );
        }
        assert!(
            recorder
                .mutating(&["detect-current", "probe-host-status"])
                .is_empty(),
            "`deploy status` must never mutate a host"
        );
        let report = fleet::fleet_drift(&statuses);
        assert!(!report.drifted(), "a converged fleet reports no drift");
    }

    #[test]
    fn fleet_status_reports_an_unreachable_host_as_a_row_not_an_abort() {
        // A dropped SSH transport on host 2 must NOT abort the report: the fleet's
        // state is exactly what the operator is trying to learn, and hosts 1 and 3
        // still have answers. The unreachable host is a row with an unknown release,
        // and contributes NO drift (a network outage is not a deploy defect).
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_status(recorder, "web-a", "r1", "200", false);
        recorder = script_status(recorder, "web-b", "r1", "200", false)
            .transport_fail("web-b", "detect-current");
        recorder = script_status(recorder, "web-c", "r1", "200", false);

        let statuses = drive_status(&fleet, &recorder).expect("status reports");
        assert_eq!(statuses.len(), 3, "every host gets a row");
        assert!(!statuses[1].reachable, "host 2 is reported unreachable");
        assert!(statuses[0].reachable && statuses[2].reachable);
        let report = fleet::fleet_drift(&statuses);
        assert!(
            !report.version_drift && report.state_drift.is_empty(),
            "an unreachable host must not manufacture drift: {report:?}"
        );
        assert_eq!(report.unknown_releases(), vec!["web-b"]);
    }

    #[test]
    fn fleet_status_surfaces_version_drift_across_mixed_releases() {
        // #1621 (T1.20 at the driver level): two hosts on different releases is the
        // mixed fleet a rolling deploy exists to avoid, and `--strict` is what makes
        // it alertable from cron.
        let hosts = ["web-a", "web-b"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_status(recorder, "web-a", "20260714T120000Z", "200", false);
        recorder = script_status(recorder, "web-b", "20260714T130000Z", "200", true);

        let statuses = drive_status(&fleet, &recorder).expect("status reports");
        let report = fleet::fleet_drift(&statuses);
        assert!(report.version_drift, "different releases are drift");
        assert_eq!(
            statuses[1].maintenance,
            exec::MaintenanceStatus::On,
            "the flag file is read per host"
        );
        assert!(
            statuses[1].ready_code == Some(200),
            "maintenance does NOT change readiness — the two are orthogonal"
        );

        let json = fleet_status_json(&statuses, &report);
        assert_eq!(json["version_drift"], serde_json::json!(true));
        assert_eq!(
            json["hosts"][0]["release"],
            serde_json::json!("20260714T120000Z")
        );
        assert_eq!(json["hosts"][1]["maintenance"], serde_json::json!(true));
        assert_eq!(json["hosts"][1]["ready"], serde_json::json!(200));
    }

    #[test]
    fn status_reports_the_last_deploy_result_per_host() {
        // #1621 AC-6, third per-host fact. Its motivating case: a rollout halts,
        // compensation succeeds, and ten minutes later every host reads back
        // `deployed / R1 / ready 200 / no drift` — a healthy converged fleet with
        // no trace that the last deploy failed and was compensated. `last deploy`
        // is what distinguishes those two fleets.
        let fleet = fleet_of(&["web-a", "web-b", "web-c"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder =
            script_status_last_deploy(recorder, "web-a", "r1", "rolled back\t2026-07-14T12:31:10Z");
        recorder =
            script_status_last_deploy(recorder, "web-b", "r1", "deployed\t2026-07-14T12:00:03Z");
        // A host that has never completed a cutover (or whose marker is
        // unreadable) reports the honest absence, never a fabricated result.
        recorder = script_status_last_deploy(recorder, "web-c", "r1", "");

        let statuses = drive_status(&fleet, &recorder).expect("status reports");
        assert_eq!(
            statuses[0].last_deploy.as_ref().map(|l| l.result.as_str()),
            Some("rolled back"),
        );
        assert_eq!(
            statuses[0]
                .last_deploy
                .as_ref()
                .and_then(|l| l.at.as_deref()),
            Some("2026-07-14T12:31:10Z"),
        );
        assert_eq!(
            statuses[1].last_deploy.as_ref().map(|l| l.result.as_str()),
            Some("deployed"),
        );
        assert_eq!(statuses[2].last_deploy, None);

        let report = fleet::fleet_drift(&statuses);
        // Reported, NOT counted as drift: the fleet is genuinely converged on r1,
        // and turning a past rollback into a standing alert would page forever.
        assert!(!report.drifted(), "a past rollback is not fleet drift");

        let rendered = fleet::fleet_status_lines(&statuses, &report).join("\n");
        assert!(
            rendered.contains("last deploy: rolled back 2026-07-14T12:31:10Z")
                && rendered.contains("last deploy: deployed 2026-07-14T12:00:03Z")
                && rendered.contains("last deploy: ?"),
            "every host's last-deploy cell must be rendered:\n{rendered}"
        );
        // The column's LIMITS are printed where it is read, not only in the guide:
        // a pre-cutover failure never rewrites the marker, so this is the host's
        // own last completed action rather than a verdict on the last rollout.
        assert!(
            rendered.contains(fleet::LAST_DEPLOY_SCOPE_NOTE),
            "the status table must state what `last deploy` does and does not know:\n{rendered}"
        );

        // And the machine-readable surface carries it as a documented object.
        let json = fleet_status_json(&statuses, &report);
        assert_eq!(
            json["hosts"][0]["last_deploy"],
            serde_json::json!({ "result": "rolled back", "at": "2026-07-14T12:31:10Z" }),
        );
        assert_eq!(json["hosts"][2]["last_deploy"], serde_json::Value::Null);
    }

    #[test]
    fn fleet_status_json_shape_is_stable_and_carries_no_secret() {
        // The repo's own skills are a first-class consumer of `--json`, so the field
        // names are a contract. Nothing here is derived from a shell line or a driver
        // error, so no secret can reach it.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        recorder = script_status(recorder, "web-a", "r1", "200", false);
        recorder = script_status(recorder, "web-b", "r1", "503", false);
        let statuses = drive_status(&fleet, &recorder).expect("status reports");
        let json = fleet_status_json(&statuses, &fleet::fleet_drift(&statuses));

        let host = &json["hosts"][0];
        for field in [
            "host",
            "reachable",
            "mode",
            "release",
            "live_slot",
            "ready",
            "maintenance",
            "proxy_port",
            "last_deploy",
            "drift",
        ] {
            assert!(
                host.get(field).is_some(),
                "the documented per-host field `{field}` must be present: {json}"
            );
        }
        for field in ["hosts", "version_drift", "state_drift", "drifted"] {
            assert!(
                json.get(field).is_some(),
                "the documented top-level field `{field}` must be present: {json}"
            );
        }
        assert_eq!(host["host"], serde_json::json!("web-a"));
        assert_eq!(host["mode"], serde_json::json!("deployed"));
        assert_eq!(json["hosts"][1]["ready"], serde_json::json!(503));
        let rendered = json.to_string();
        for secret in ["AUTUMN_SECURITY__SIGNING_SECRET", "postgres://", "curl"] {
            assert!(
                !rendered.contains(secret),
                "the status JSON must never carry `{secret}`: {rendered}"
            );
        }
    }

    // ── `deploy maintenance on|off` fan-out (issue #1621, AC-5) ──────────────

    /// The remote paths a maintenance `on` writes on one host, per amendment A2.
    const MAINTENANCE_SHARED_PATH: &str = "/srv/autumn/myapp/shared/autumn-maintenance.json";
    const MAINTENANCE_LEGACY_PATH: &str =
        "/srv/autumn/myapp/releases/r1/tmp/autumn-maintenance.json";
    /// The flag path a pre-#1621 unit resolves to when it is running a DIFFERENT
    /// release than the one `current` names — the round-3 disagreement.
    const MAINTENANCE_LIVE_UNIT_PATH: &str =
        "/srv/autumn/myapp/releases/r2/tmp/autumn-maintenance.json";

    fn drive_maintenance(
        fleet: &ResolvedFleet,
        recorder: &fleet::test_support::FleetRecorder,
        verb: DeployAction,
        args: Option<&MaintenanceOnArgs>,
    ) -> Result<(), DeployError> {
        let port = StatusPort {
            port: FLEET_PUBLIC_PORT,
            degraded: None,
        };
        run_fleet_maintenance_with(fleet, &port, "prod", verb, args, |cfg| {
            Ok(recorder.executor(cfg))
        })
    }

    /// Script one host for the maintenance fan-out: the shared deploy probe, plus
    /// the flag path that host's LIVE slot unit resolves to (`""` scripts a unit
    /// that could not be read).
    fn script_maintenance(
        recorder: fleet::test_support::FleetRecorder,
        host: &str,
        release: &str,
        unit_flag: &str,
    ) -> fleet::test_support::FleetRecorder {
        recorder
            .script(host, "detect-current", status_probe(release))
            .script(
                host,
                "detect-maintenance-flag",
                if unit_flag.is_empty() {
                    String::new()
                } else {
                    format!("{unit_flag}\n")
                },
            )
    }

    /// The remote paths `host` was asked to upload to, in order.
    fn upload_paths(recorder: &fleet::test_support::FleetRecorder, host: &str) -> Vec<String> {
        recorder
            .calls_for(host)
            .iter()
            .filter_map(|call| match call {
                exec::test_support::RecordedCall::Upload { remote_path, .. } => {
                    Some(remote_path.clone())
                }
                exec::test_support::RecordedCall::Run { .. } => None,
            })
            .collect()
    }

    #[test]
    fn fleet_maintenance_on_writes_both_flag_paths_on_every_host() {
        // #1621 (AC-5, amendment A2): a host deployed BEFORE this release runs a slot
        // unit with no `AUTUMN_MAINTENANCE_FLAG_FILE`, so its runtime still polls the
        // cwd-relative `{release_dir}/tmp/…`. Writing only the shared path would make
        // `deploy maintenance on` silently do nothing on exactly the hosts an operator
        // is most likely to be maintaining. So both paths are written: the shared one
        // is authoritative for redeployed hosts, the release-scoped one keeps the
        // not-yet-redeployed ones working.
        let hosts = ["web-a", "web-b"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_maintenance(recorder, host, "r1", MAINTENANCE_LEGACY_PATH);
        }

        let args = MaintenanceOnArgs {
            message: Some("Upgrading the database".to_owned()),
            readonly: true,
            ..MaintenanceOnArgs::default()
        };
        drive_maintenance(&fleet, &recorder, DeployAction::MaintenanceOn, Some(&args))
            .expect("every host accepts the write");

        for host in hosts {
            let paths = upload_paths(&recorder, host);
            assert!(
                paths.iter().any(|p| p == MAINTENANCE_SHARED_PATH),
                "{host} must get the shared (authoritative) flag: {paths:?}"
            );
            assert!(
                paths.iter().any(|p| p == MAINTENANCE_LEGACY_PATH),
                "{host} must also get the release-scoped flag for pre-#1621 units: {paths:?}"
            );
            // The release-scoped `tmp/` dir may not exist yet.
            assert!(
                recorder
                    .run_labels_for(host)
                    .contains(&"maintenance-prepare-live-flag-dir"),
                "{host} must create the dir holding the running unit's flag before \
                 writing into it"
            );
        }
    }

    #[test]
    fn fleet_maintenance_flag_is_written_0600_and_never_echoes_the_bypass_token() {
        // #1621 review finding 8. The flag body embeds `--bypass-header NAME:VALUE`
        // — a credential that lets a request skip the 503 gate for the whole
        // maintenance window — plus the `--allow-ips` list. Both the remote file
        // and the local staging copy inherit the op's mode, so 0644 exposed the
        // token to every local account on every fleet host AND in the operator's
        // own `/tmp`. The file is written and read by the same `cfg.user` that
        // `render_app_unit` emits as `User=`, so 0600 works exactly as it does for
        // the env file carrying the signing secret (`exec::env_file` / AC-5).
        let hosts = ["web-a", "web-b"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_maintenance(recorder, host, "r1", MAINTENANCE_LEGACY_PATH);
        }

        let args = MaintenanceOnArgs {
            message: Some("Upgrading the database".to_owned()),
            allow_ips: vec!["10.0.0.0/8".to_owned()],
            readonly: true,
            bypass_header: Some(("X-Dev-Bypass".to_owned(), "s3cr3t-bypass".to_owned())),
        };
        drive_maintenance(&fleet, &recorder, DeployAction::MaintenanceOn, Some(&args))
            .expect("every host accepts the write");

        for host in hosts {
            let uploads: Vec<(String, Option<u32>)> = recorder
                .calls_for(host)
                .iter()
                .filter_map(|call| match call {
                    exec::test_support::RecordedCall::Upload { remote_path, mode } => {
                        Some((remote_path.clone(), *mode))
                    }
                    exec::test_support::RecordedCall::Run { .. } => None,
                })
                .collect();
            assert_eq!(
                uploads.len(),
                2,
                "{host} writes the shared and the release-scoped flag: {uploads:?}"
            );
            for (path, mode) in &uploads {
                assert_eq!(
                    *mode,
                    Some(0o600),
                    "{host}: {path} carries the bypass token, so it must not be world-readable"
                );
            }
        }

        // The token must never ride in an op label or a rendered shell line.
        let calls_debug = format!("{:#?}", recorder.calls_for("web-a"));
        assert!(
            !calls_debug.contains("s3cr3t-bypass"),
            "the bypass token leaked into the recorded executor calls: {calls_debug}"
        );
    }

    #[test]
    fn fleet_maintenance_on_writes_shared_only_when_no_release_is_promoted() {
        // A host whose first deploy has not happened yet runs no slot unit at all, so
        // no application polls a release-local flag and the shared write IS the whole
        // job — the next deploy installs a #1621 unit that honours it. The outcome says
        // shared-only rather than pretending two files landed, and unlike the unproved
        // case (where a unit IS serving) this is a success, not a failure. Also proves
        // the fan-out works for an N==1 "fleet" (the deploy-config'd remote host, as
        // distinct from the LOCAL `autumn maintenance`).
        let fleet = fleet_of(&["web-a"]);
        let recorder = fleet::test_support::FleetRecorder::new().script(
            "web-a",
            "detect-current",
            "first\n---autumn-kamal-proxy-list---\n",
        );

        drive_maintenance(
            &fleet,
            &recorder,
            DeployAction::MaintenanceOn,
            Some(&MaintenanceOnArgs::default()),
        )
        .expect("a shared-only write is a success, not a failure");

        let paths = upload_paths(&recorder, "web-a");
        assert_eq!(
            paths,
            vec![MAINTENANCE_SHARED_PATH.to_owned()],
            "only the shared flag can be written without a resolvable release"
        );
        assert!(
            !recorder
                .run_labels_for("web-a")
                .contains(&"maintenance-prepare-live-flag-dir"),
            "no running unit means no second flag dir to prepare"
        );
    }

    #[test]
    fn fleet_maintenance_writes_the_flag_the_running_unit_polls_not_the_symlink_target() {
        // #1621 review round 3. `current` and the RUNNING unit can disagree: the proxy
        // flip landed and `commit-markers` did not, so the symlink still names the OLD
        // release while the live slot unit keeps serving from another one. Deriving the
        // legacy flag path from `current` then writes a file NOTHING polls — `on`
        // reports success while the application carries on serving traffic. The write
        // path must resolve the file from the proxy-selected live slot's unit, exactly
        // as the status probe reads it, so read and write agree by construction.
        let fleet = fleet_of(&["web-a"]);
        let recorder = script_maintenance(
            fleet::test_support::FleetRecorder::new(),
            "web-a",
            "r1",
            MAINTENANCE_LIVE_UNIT_PATH,
        );

        drive_maintenance(
            &fleet,
            &recorder,
            DeployAction::MaintenanceOn,
            Some(&MaintenanceOnArgs::default()),
        )
        .expect("both writes land");

        assert_eq!(
            upload_paths(&recorder, "web-a"),
            vec![
                MAINTENANCE_SHARED_PATH.to_owned(),
                MAINTENANCE_LIVE_UNIT_PATH.to_owned(),
            ],
            "the shared flag first (A2), then the file the RUNNING unit polls — and \
             never the stale `current` target"
        );
    }

    #[test]
    fn fleet_maintenance_on_fails_closed_when_the_running_units_flag_is_unproved() {
        // The live slot's unit could not be read, so which file the application polls
        // is NOT proved. The shared (authoritative) write still happens — a #1621 unit
        // reacts to it within 500 ms — but the host must not be reported as maintained:
        // a pre-#1621 unit ignores the shared flag entirely, so claiming success here
        // is exactly the lie the round-3 finding is about.
        let fleet = fleet_of(&["web-a"]);
        let recorder =
            script_maintenance(fleet::test_support::FleetRecorder::new(), "web-a", "r1", "");

        let err = drive_maintenance(
            &fleet,
            &recorder,
            DeployAction::MaintenanceOn,
            Some(&MaintenanceOnArgs::default()),
        )
        .expect_err("an unproved live-unit flag must not exit 0");
        assert!(
            err.to_string().contains("1 of 1"),
            "the host counts as failed in the aggregate: {err}"
        );
        assert_eq!(
            upload_paths(&recorder, "web-a"),
            vec![MAINTENANCE_SHARED_PATH.to_owned()],
            "the shared flag is still written first; nothing is GUESSED for the rest"
        );
    }

    #[test]
    fn fleet_maintenance_off_never_claims_to_clear_a_flag_it_cannot_locate() {
        // The mirror of the `on` case: `off` that cannot prove which file the running
        // unit polls must not report the host as un-maintained, and must not pretend to
        // have removed a release-local flag whose path it only guessed from `current`.
        let fleet = fleet_of(&["web-a"]);
        let recorder =
            script_maintenance(fleet::test_support::FleetRecorder::new(), "web-a", "r1", "");

        drive_maintenance(&fleet, &recorder, DeployAction::MaintenanceOff, None)
            .expect_err("an unproved live-unit flag must not exit 0");

        let shell = recorder
            .calls_for("web-a")
            .iter()
            .find_map(|call| match call {
                exec::test_support::RecordedCall::Run { label, shell }
                    if *label == "maintenance-clear" =>
                {
                    Some(shell.clone())
                }
                _ => None,
            })
            .expect("the shared flag is still cleared");
        assert!(
            shell.contains(MAINTENANCE_SHARED_PATH),
            "the shared flag is still removed: {shell}"
        );
        assert!(
            !shell.contains(MAINTENANCE_LEGACY_PATH),
            "a path derived from `current` must not be removed on a hunch: {shell}"
        );
    }

    #[test]
    fn fleet_maintenance_row_says_the_running_units_flag_was_not_changed() {
        // The row for a partially-changed host must say BOTH true things: the shared
        // flag did change (so an operator reversing by hand knows), and the file the
        // running unit polls did NOT (so nobody reads the row as "this host is
        // maintained").
        let rendered = fleet::fleet_maintenance_summary_lines(
            &["web-a".to_owned()],
            &[fleet::MaintenanceOutcome::LiveUnitUnchanged {
                failed_step: "detect-maintenance-flag",
            }],
            true,
        )
        .join("\n");
        assert!(
            rendered.contains("detect-maintenance-flag"),
            "the row names the step that could not be proved:\n{rendered}"
        );
        assert!(
            rendered.contains("shared flag") && rendered.contains("RUNNING"),
            "the row must distinguish the shared flag from the running unit's:\n{rendered}"
        );
    }

    #[test]
    fn fleet_maintenance_serializes_the_framework_wire_format() {
        // The flag file's shape is `autumn_web::maintenance::MaintenanceConfig` — the
        // struct the RUNTIME deserializes. The CLI serialises that type rather than
        // hand-rolling JSON, so the two can never drift.
        let args = MaintenanceOnArgs {
            message: Some("Upgrading".to_owned()),
            allow_ips: vec!["10.0.0.0/8".to_owned()],
            readonly: true,
            bypass_header: Some(("X-Bypass".to_owned(), "tok".to_owned())),
        };
        let body = maintenance_flag_body(&args);
        let parsed: autumn_web::maintenance::MaintenanceConfig =
            serde_json::from_str(&body).expect("the body must be a MaintenanceConfig");
        assert_eq!(parsed.message.as_deref(), Some("Upgrading"));
        assert!(parsed.readonly);
        assert_eq!(parsed.allow_ips, vec!["10.0.0.0/8"]);
        assert_eq!(
            parsed
                .bypass_header
                .as_ref()
                .map(|(n, v)| (n.as_str(), v.as_str())),
            Some(("X-Bypass", "tok"))
        );
    }

    #[test]
    fn fleet_maintenance_off_removes_both_flag_paths() {
        let hosts = ["web-a", "web-b"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_maintenance(recorder, host, "r1", MAINTENANCE_LEGACY_PATH);
        }

        drive_maintenance(&fleet, &recorder, DeployAction::MaintenanceOff, None)
            .expect("every host clears cleanly");

        for host in hosts {
            let shell = recorder
                .calls_for(host)
                .iter()
                .find_map(|call| match call {
                    exec::test_support::RecordedCall::Run { label, shell }
                        if *label == "maintenance-clear" =>
                    {
                        Some(shell.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{host} must run the clear op"));
            assert!(
                shell.contains(MAINTENANCE_SHARED_PATH) && shell.contains(MAINTENANCE_LEGACY_PATH),
                "{host} must clear BOTH paths: {shell}"
            );
            assert!(
                shell.contains("rm -f"),
                "clearing an absent flag must not be an error: {shell}"
            );
            assert!(
                upload_paths(&recorder, host).is_empty(),
                "`off` writes nothing"
            );
        }
    }

    #[test]
    fn fleet_maintenance_attempts_every_host_and_reports_the_ones_that_failed() {
        // #1621 (T1.21, plan §6.2): control-plane fan-out is best-effort-and-aggregate,
        // the OPPOSITE of the rollout driver. `maintenance on` that succeeded on 2 of 3
        // hosts must NOT be rolled back — that would push users back into the very live
        // window the operator is trying to close — and it must NOT stop at host 2,
        // because host 3 is still serving. Every host is attempted, all three are named
        // with their state, and the command exits non-zero.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_maintenance(recorder, host, "r1", MAINTENANCE_LEGACY_PATH);
        }
        recorder = recorder.fail_upload("web-b", MAINTENANCE_SHARED_PATH);

        let err = drive_maintenance(
            &fleet,
            &recorder,
            DeployAction::MaintenanceOn,
            Some(&MaintenanceOnArgs::default()),
        )
        .expect_err("a failed host must exit non-zero");
        let message = err.to_string();
        assert!(
            message.contains("1 of 3"),
            "the error must count the failures against the fleet: {message}"
        );

        // Hosts 1 and 3 were still attempted AND still wrote — the point of
        // best-effort-and-aggregate.
        for host in ["web-a", "web-c"] {
            assert!(
                upload_paths(&recorder, host)
                    .iter()
                    .any(|p| p == MAINTENANCE_SHARED_PATH),
                "{host} must still be attempted (and changed) after web-b failed"
            );
        }
        assert!(
            upload_paths(&recorder, "web-b")
                .iter()
                .any(|p| p == MAINTENANCE_SHARED_PATH),
            "the failing host is attempted too, not skipped"
        );
        // Nothing about the failure leaks a shell line or a remote message.
        for noise in ["scripted upload failure", "rm -f", "printf"] {
            assert!(
                !message.contains(noise),
                "the aggregate error must carry no remote detail: {message}"
            );
        }
    }

    #[test]
    fn fleet_maintenance_rows_name_every_host_and_the_orthogonality_note() {
        // Every host is named with what happened to it, so an operator who has to
        // reverse a partial change knows which hosts changed. And the output states —
        // every time — that maintenance mode does not drain a host from the LB, because
        // the opposite assumption causes an outage.
        let rendered = fleet::fleet_maintenance_summary_lines(
            &["web-a".to_owned(), "web-b".to_owned(), "web-c".to_owned()],
            &[
                fleet::MaintenanceOutcome::Applied,
                fleet::MaintenanceOutcome::Failed {
                    failed_step: "maintenance-write-shared",
                },
                fleet::MaintenanceOutcome::AppliedSharedOnly,
            ],
            true,
        )
        .join("\n");
        for host in ["web-a", "web-b", "web-c"] {
            assert!(rendered.contains(host), "every host is named:\n{rendered}");
        }
        assert!(
            rendered.contains(fleet::MAINTENANCE_DOES_NOT_DRAIN_NOTE),
            "the orthogonality note must be printed:\n{rendered}"
        );
        assert!(
            rendered.contains("maintenance-write-shared"),
            "a failure names its step label:\n{rendered}"
        );
        assert!(
            rendered.contains("Changed anyway: web-a, web-c"),
            "the hosts that DID change must be named so they can be reversed:\n{rendered}"
        );
    }

    #[test]
    fn fleet_rollback_rolls_back_every_host_newest_first() {
        // #1621 (§3.2). `deploy rollback` on a `[deploy] hosts` config rolls back the
        // WHOLE fleet, in strict REVERSE declaration order: the hosts that cut over
        // last have been on the new release for the shortest time, so undoing them
        // first shrinks the mixed window from the newest end — the same order fleet
        // compensation uses.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_previous_release(recorder, host);
        }

        drive_fleet_rollback(&fleet, &recorder).expect("every host rolls back cleanly");

        for host in hosts {
            assert_eq!(
                recorder.run_labels_for(host),
                FLEET_ROLLBACK_RUN_LABELS.to_vec(),
                "{host} must run the UNCHANGED on-demand rollback sequence"
            );
        }
        let flips: Vec<usize> = hosts
            .iter()
            .map(|host| {
                recorder
                    .index_of(host, "proxy-flip")
                    .unwrap_or_else(|| panic!("{host} must flip back"))
            })
            .collect();
        assert!(
            flips[2] < flips[1] && flips[1] < flips[0],
            "hosts must roll back newest-first (web-c, web-b, web-a), got positions {flips:?}"
        );
    }

    #[test]
    fn fleet_rollback_rolls_the_reachable_hosts_back_past_a_dead_peer() {
        // #1621 (§3.2 vs AC-7). `deploy up` refuses the whole rollout when any host
        // fails preflight — a rollout is a change, and starting one it cannot finish
        // is how a fleet ends up mixed. A ROLLBACK is the opposite: it is recovery,
        // and the host that stopped answering SSH is very often the reason the
        // operator is rolling back. Hard-gating there leaves the healthy majority on
        // the bad release — MORE hosts on the release being abandoned, which is the
        // outcome the best-effort-continue contract exists to prevent.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-c"] {
            recorder = script_previous_release(recorder, host);
        }
        // web-b never answered the preflight TCP probe. It is NOT scripted at all, so
        // the strict recorder panics if the rollback so much as probes it.
        let checks = vec![
            PreflightCheck::fail(
                "ssh_reachability",
                "cannot reach web-b:22",
                "check the host",
            )
            .scoped(Some("web-b".to_owned())),
            PreflightCheck::pass("migrate_check", "ok"),
        ];
        let proxy = proxy::KamalProxyController::new(60);

        let err = run_fleet_rollback_with(&checks, &fleet, &proxy, FLEET_PUBLIC_PORT, |cfg| {
            Ok(recorder.executor(cfg))
        })
        .expect_err("a fleet that did not fully converge must exit non-zero");

        assert!(
            matches!(
                &err,
                DeployError::FleetRollbackFailed {
                    not_rolled_back: 1,
                    total: 3
                }
            ),
            "the unreachable host is reported as NOT rolled back, got: {err:?}"
        );
        for host in ["web-a", "web-c"] {
            assert_eq!(
                recorder.run_labels_for(host),
                FLEET_ROLLBACK_RUN_LABELS.to_vec(),
                "{host} is reachable and must be rolled back despite web-b being dead"
            );
        }
        assert!(
            recorder.run_labels_for("web-b").is_empty(),
            "an unreachable host must not be touched at all: {:?}",
            recorder.run_labels_for("web-b")
        );
    }

    #[test]
    fn a_rollback_still_hard_gates_on_a_project_wide_preflight_failure() {
        // …and the downgrade above is narrow by construction: only a PER-HOST
        // reachability row is survivable. A project-wide grader describes the release
        // being rolled back TO, not one host's reachability, so it keeps the
        // pre-#1621 hard gate — as does every check of a single-host rollback, whose
        // rows are unscoped.
        let reachable_fleet = vec![
            PreflightCheck::fail(
                "ssh_reachability",
                "cannot reach web-b:22",
                "check the host",
            )
            .scoped(Some("web-b".to_owned())),
        ];
        assert_eq!(
            blocking_rollback_failures(&reachable_fleet, true),
            0,
            "a per-host reachability failure must not block the reachable hosts"
        );

        let mut project_wide = reachable_fleet;
        project_wide.push(PreflightCheck::fail(
            "migrate_check",
            "migrations do not compile",
            "fix them",
        ));
        assert_eq!(
            blocking_rollback_failures(&project_wide, true),
            1,
            "a project-wide grader still aborts the whole command"
        );

        // The single-host path is untouched: every failure blocks it (pre-#1621
        // behavior verbatim).
        let single = vec![PreflightCheck::fail(
            "ssh_reachability",
            "cannot reach web-a:22",
            "check the host",
        )];
        assert_eq!(
            blocking_rollback_failures(&single, false),
            1,
            "a single-host rollback must keep aborting before touching the server"
        );
        // …including when the row IS scoped. A `--only`-narrowed rollback scopes its
        // reachability row for attribution (#1621 audit gap G10) but still runs the
        // single-host path, which has no skip-and-report step — so downgrading the
        // row there would send it straight into an SSH the preflight already knows
        // will fail. The gate keys on the PATH, never on the scope alone.
        let narrowed = vec![
            PreflightCheck::fail(
                "ssh_reachability",
                "cannot reach web-b:22",
                "check the host",
            )
            .scoped(Some("web-b".to_owned())),
        ];
        assert_eq!(
            blocking_rollback_failures(&narrowed, false),
            1,
            "a narrowed single-host rollback must still abort before touching the server"
        );
    }

    #[test]
    fn fleet_rollback_skips_a_host_with_no_previous_release_and_rolls_the_rest_back() {
        // #1621 (§3.2). A host that has never been redeployed — a first deploy clears
        // the marker, and a host just added to the fleet never had one — has nothing to
        // roll back to. That is a reported skip rather than a failed rollback, and it
        // must not stop the other hosts: halting there would leave them on the release
        // the operator is trying to leave. It does still make the command exit non-zero.
        // The contract of a fleet rollback is that the fleet returns to its previous
        // release, and a fleet where one host cannot follow the others is mixed —
        // reporting that as success is how a mixed fleet survives an incident unnoticed.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in ["web-a", "web-c"] {
            recorder = script_previous_release(recorder, host);
        }
        // web-b's marker is absent: the probe answers `none`.
        recorder = recorder.script("web-b", "resolve-previous", "none");

        let err = drive_fleet_rollback(&fleet, &recorder)
            .expect_err("a fleet that did not fully converge must exit non-zero");
        assert!(
            matches!(
                &err,
                DeployError::FleetRollbackFailed {
                    not_rolled_back: 1,
                    total: 3
                }
            ),
            "exactly one host must be reported as un-rolled-back, got: {err:?}"
        );
        assert!(
            err.to_string().contains("no previous release"),
            "the error must explain the skipped case, not just count it: {err}"
        );

        assert_eq!(
            recorder.run_labels_for("web-b"),
            vec!["resolve-previous"],
            "a host with no previous release must be probed and then left ALONE"
        );
        for host in ["web-a", "web-c"] {
            assert_eq!(
                recorder.run_labels_for(host),
                FLEET_ROLLBACK_RUN_LABELS.to_vec(),
                "{host} must still roll back — a skip is not a halt"
            );
        }
    }

    #[test]
    fn fleet_rollback_continues_past_a_failed_host_and_exits_non_zero() {
        // #1621 (§3.2). Best-effort continue, never swallowed: web-c's rollback
        // fails at its readiness gate, and web-b and web-a are still rolled back —
        // stopping would leave MORE hosts on the release being abandoned. The
        // command still exits non-zero, with counts only (never a driver message).
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_previous_release(recorder, host);
        }
        recorder = recorder.fail("web-c", "readiness-gate");

        let err = drive_fleet_rollback(&fleet, &recorder)
            .expect_err("a failed host must fail the fleet rollback");

        match &err {
            DeployError::FleetRollbackFailed {
                not_rolled_back,
                total,
            } => {
                assert_eq!((*not_rolled_back, *total), (1, 3));
            }
            other => panic!("expected a fleet-rollback failure, got: {other:?}"),
        }
        for host in ["web-a", "web-b"] {
            assert!(
                recorder.run_labels_for(host).contains(&"proxy-flip"),
                "{host} must still be rolled back after web-c failed"
            );
        }
        let rendered = format!("{err}\n{err:?}");
        for secret in [
            "postgres://",
            "topsecret",
            "AUTUMN_SECURITY__SIGNING_SECRET",
        ] {
            assert!(
                !rendered.contains(secret),
                "the fleet-rollback error must never carry `{secret}`: {rendered}"
            );
        }
    }

    #[test]
    fn a_fleet_rollback_that_rolled_nothing_back_is_an_error() {
        // #1621 (§3.2). Every host reports no previous release, so nothing happened
        // at all. Exiting zero would tell the operator their fleet was rolled back
        // when it was not — the one outcome worse than failing.
        let hosts = ["web-a", "web-b"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = recorder.script(host, "resolve-previous", "none");
        }

        let err = drive_fleet_rollback(&fleet, &recorder)
            .expect_err("a rollback that rolled nothing back must not report success");
        assert!(
            matches!(
                &err,
                DeployError::FleetRollbackFailed {
                    not_rolled_back: 2,
                    total: 2
                }
            ),
            "expected all-hosts-unrolled, got: {err:?}"
        );
    }

    #[test]
    fn operator_facing_fleet_error_text_carries_no_line_continuation_gutter() {
        // #1621 review finding 12. These messages are `\`-continued multi-line
        // literals; a missing continuation backslash bakes the source indentation
        // into the message as a long run of spaces mid-sentence. `DriftDetected`
        // has no placeholders, so its Display output is the literal byte-for-byte.
        assert_eq!(
            DeployError::DriftDetected.to_string(),
            "fleet drift detected \u{2014} the status table above names each host and reason \
             (`--strict` exits non-zero on drift so it can be alerted on)",
        );
        assert_eq!(
            DeployError::FleetMaintenanceFailed {
                verb: "on",
                failed: 1,
                total: 3,
            }
            .to_string(),
            "fleet maintenance on incomplete: 1 of 3 host(s) failed \u{2014} the table above \
             names the hosts that DID change, so they can be reversed by hand if the fleet \
             must stay consistent",
        );
        for rendered in [
            DeployError::DriftDetected.to_string(),
            DeployError::FleetMaintenanceFailed {
                verb: "off",
                failed: 2,
                total: 5,
            }
            .to_string(),
        ] {
            assert!(
                !rendered.contains("   "),
                "a run of spaces mid-sentence means a `\\` continuation was dropped: {rendered}",
            );
        }
    }
}
