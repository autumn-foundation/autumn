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
    #[error(
        "fleet rollout halted at {failed_host} during `{failed_step}` — the remaining hosts were \
         not touched"
    )]
    FleetHalted {
        /// Host the rollout stopped on.
        failed_host: String,
        /// Label of the step that failed on it.
        failed_step: &'static str,
        /// Hosts whose candidate was rolled back (their previous release still serves).
        rolled_back: Vec<String>,
        /// Hosts whose first-deploy candidate was torn down (nothing serves there).
        torn_down: Vec<String>,
        /// Hosts left running the NEW release, in rollout order.
        still_on_new: Vec<String>,
    },
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
        })
    }

    /// Persistent per-app dir shared across releases (holds the secret env file
    /// and, since #1952, the uploaded config manifest\[s\]). The deployed systemd
    /// unit sets `AUTUMN_MANIFEST_DIR` to this path so the app's config loader
    /// reads the uploaded `autumn.toml` here at boot.
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
fn deploy_host_list(cfg: &DeployConfig) -> Result<Vec<String>, String> {
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
    #[cfg_attr(not(test), allow(dead_code))]
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
    #[must_use]
    pub const fn is_single(&self) -> bool {
        self.hosts.len() == 1
    }
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
    pub name: &'static str,
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
            passed: true,
            deferred: false,
            detail: detail.into(),
            hint: None,
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: &'static str) -> Self {
        Self {
            name,
            passed: false,
            deferred: false,
            detail: detail.into(),
            hint: Some(hint),
        }
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
        DeployStep::new(
            "upload",
            format!(
                "Upload the binary to a new timestamped release dir under {}",
                cfg.releases_dir()
            ),
        ),
        DeployStep::new(
            "migrate",
            "Run pending migrations BEFORE cutover — abort here leaves the current version serving",
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

/// Run the requested `autumn deploy` subcommand.
///
/// # Errors
///
/// Returns [`DeployError::Config`] when the project config cannot be loaded and
/// [`DeployError::PreflightFailed`] when `check` finds a failing grader.
pub fn run(action: DeployAction) -> Result<(), DeployError> {
    // Ambient load (the operator's shell profile, `dev` by default): used ONLY to
    // read `[deploy]` and compute `resolved` — in particular `resolved.profile`,
    // the profile the deployed service will actually boot under (default `prod`).
    // Lenient-unknown-roots load (#2063): the deploy CLI structurally cannot
    // know an app's plugin set (no AppBuilder, no plugin-crate dep), so it must
    // NOT fail-close on a plugin-owned top-level config table (e.g. `[media]`)
    // under `strict_config`. It keeps strict validation of every core section
    // it DOES own; app boot — which knows the plugin set via the `config_section`
    // seam — remains the strict gate for plugin roots.
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
        DeployAction::Check => run_check(&load_runtime_config(&resolved)?, &resolved),
        DeployAction::Rollback => run_rollback(&load_runtime_config(&resolved)?, &resolved),
        DeployAction::Up => {
            let (media_cfg, ffmpeg_bin) = load_media_host_config(&resolved)?;
            run_up(
                &load_runtime_config(&resolved)?,
                &resolved,
                &host_list,
                &media_cfg,
                &ffmpeg_bin,
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
        // Report `AUTUMN_DOTENV=1` so `should_load` opts the (non-dev) deploy
        // profile into `.env.<profile>` loading. The operator explicitly ran
        // `autumn deploy` to gather + upload the target profile's config, and
        // `.env.<profile>` is a documented place for profile-only values, so
        // the deploy-time reload reads it without requiring the operator to
        // export `AUTUMN_DOTENV` by hand. This does not mutate the global env
        // and does not touch the deployed service's runtime. The
        // profile-selector-key exclusion in the dotenv overlay still strips
        // `AUTUMN_ENV`/`AUTUMN_PROFILE`/`AUTUMN_IS_DEBUG` from any `.env` file.
        //
        // But only SYNTHESIZE `1` when the inner env has no explicit value: an
        // operator who runs `AUTUMN_DOTENV=0 autumn deploy ...` (or `false`) is
        // deliberately opting OUT of `.env.<profile>` loading, and `should_load`
        // honors `0`/`false` as the documented off switch (see dotenv.rs). So
        // delegate to the inner env when it provides `AUTUMN_DOTENV`, preserving
        // an explicit `0`/`false`/`1`/`true`, and fall back to `1` only when it
        // is unset.
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
    vec![
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
    ]
}

/// Preflight for a whole fleet: `ssh_reachability` **per host**, then the three
/// project-wide graders **once** (issue #1621, AC-7).
///
/// A single-host fleet returns exactly [`collect_preflight`]'s vector, so its
/// report is byte-identical to pre-#1621 (AC-1). For a real fleet the per-host
/// rows are distinguished by the grader's own detail text, which already names the
/// host (`SSH port reachable at web-2:22`) — the structured `scope` field that
/// `doctor --json` will also carry lands with the CLI-surface slice.
///
/// The probes run SERIALLY here: they are a bounded TCP connect each, the report
/// must come out in declaration order, and threading them would put a `Sync` bound
/// on machinery the `RefCell`-based recording fake cannot satisfy. Parallelising
/// the pure TCP graders is a later, isolated change.
fn collect_fleet_preflight(config: &AutumnConfig, fleet: &ResolvedFleet) -> Vec<PreflightCheck> {
    let Some(shared) = fleet.hosts.first() else {
        return Vec::new();
    };
    if fleet.is_single() {
        return collect_preflight(config, shared);
    }
    let mut checks: Vec<PreflightCheck> = fleet
        .hosts
        .iter()
        .map(|cfg| grade_ssh_reachability(cfg.host.as_deref(), cfg.ssh_port, SSH_PROBE_TIMEOUT))
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

/// Print each preflight check as a pass/fail line and return the failure count.
/// Shared by `check` and `up` so both surfaces report graders identically.
fn report_preflight(checks: &[PreflightCheck]) -> usize {
    let mut failed = 0_usize;
    for check in checks {
        if check.passed {
            eprintln!("\u{2705} {}: {}", check.name, check.detail);
        } else if check.deferred {
            // Non-blocking: verified in the service's own runtime, not here.
            // Surfaced as a warning so it is visible but never aborts the deploy.
            eprintln!("\u{26A0}\u{FE0F}  {}: {}", check.name, check.detail);
            if let Some(hint) = check.hint {
                eprintln!("   \u{2192} {hint}");
            }
        } else {
            failed += 1;
            eprintln!("\u{274C} {}: {}", check.name, check.detail);
            if let Some(hint) = check.hint {
                eprintln!("   \u{2192} {hint}");
            }
        }
    }
    eprintln!();
    failed
}

fn run_check(config: &AutumnConfig, resolved: &ResolvedDeployConfig) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy check\n");

    let checks = collect_preflight(config, resolved);
    let failed = report_preflight(&checks);

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

    uploads
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
    media_cfg: &media::MediaMtxHostConfig,
    ffmpeg_bin: &str,
) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy up\n");

    // The rollout targets, in declaration order. A single-host config resolves to
    // a one-host fleet whose only element IS today's `resolved`, so everything
    // below is the pre-#1621 sequence at N = 1.
    let fleet = ResolvedFleet::from_targets(resolved, hosts);

    // Fail fast: run the full preflight and abort BEFORE any remote call. Every
    // host is graded here, so an unreachable host in position 3 is reported before
    // host 1 is touched.
    let checks = collect_fleet_preflight(config, &fleet);
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
            writable_db_configured: resolve_writable_db_url(&config.database).is_some(),
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
    /// Whether a writable database URL is configured — the fleet warns loudly when
    /// it is and the rollout schedules no migration at all.
    writable_db_configured: bool,
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
    let single = input.fleet.is_single();
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

    // Fail closed on kamal-proxy CLI-surface drift BEFORE any cutover (#2053): the
    // controller consumes an UNPINNED kamal-proxy from host bootstrap, so a
    // renamed/removed subcommand or flag on `kamal-proxy deploy` would otherwise
    // break a real cutover with no warning. A compatible binary passes silently;
    // an incompatible one aborts here (read-only `--help` probe, nothing mutated).
    exec::probe_proxy_compat(input.proxy, executor)
        .map_err(|e| DeployError::Exec(scoped(e.to_string())))?;

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
        }),
        exec::DeployMode::Redeploy { live_slot } => {
            // Pre-flight refuse (#2073): the reboot-durability restart-refresh (#2070)
            // re-execs `kamal-proxy run` on the public port and re-registers the
            // still-live upstream at its DERIVED loopback port — both correct ONLY
            // when the public port is unchanged. If the operator changed `server.port`
            // since the live release was deployed, that restart would rebind a
            // different public port and the re-register would aim at a dead loopback
            // port, stranding `:80` mid-cutover with no auto-recovery. Refuse here —
            // BEFORE any op runs, sourced from the INSTALLED proxy unit's `--http-port`
            // (captured in the deploy-start probe) — so the live release keeps serving.
            // A live-safe port change is future work (Option C, #2073). No installed
            // unit → first-deploy shape (allowed); an unreadable unit → fail closed.
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
/// - **A failure HALTS the rollout.** Hosts after the failing one are never
///   touched. Compensating the hosts that already cut over is the next slice; the
///   typed [`DeployError::FleetHalted`] already names them.
#[allow(clippy::too_many_lines)]
fn run_up_with<E, P, F>(input: &FleetUpInput<'_, P>, make_executor: F) -> Result<(), DeployError>
where
    E: exec::DeployExecutor,
    P: ProxyController,
    F: Fn(&ResolvedDeployConfig) -> Result<E, DeployError>,
{
    let fleet = input.fleet;
    let single = fleet.is_single();
    let total = fleet.hosts.len();

    // ONE executor per host, built up front and reused for that host's read-only
    // probe AND its execution.
    let executors = fleet
        .hosts
        .iter()
        .map(&make_executor)
        .collect::<Result<Vec<E>, DeployError>>()?;

    // MediaMTX host preflight (#1974 Slice 7) — non-mutating doctor checks run
    // BEFORE the app deploy so a bad media host fails fast without anything being
    // written. The MUTATING provisioning (write config/unit + restart) is deferred
    // until after the app cutover succeeds (below) so a rolled-back app deploy
    // never touches the host MediaMTX unit. No-op unless enabled (see fn).
    //
    // Single-host only for now: `provision_media_host` has no teardown/rollback
    // path, so fanning it out would leave N media daemons on identical ports with
    // divergent recording sets and nothing to undo them with. Until the fleet-wide
    // refusal lands a fleet therefore provisions NO media and prints nothing new.
    //
    // slice 5: refusal — a media-enabled fleet (`[media.mediamtx] enabled` with
    // more than one host) must be REFUSED in the prologue, alongside the sqlite and
    // TLS fleet refusals, rather than silently skipped here.
    if single {
        check_media_host_preflight(input.media_cfg, input.ffmpeg_bin, &executors[0])?;
    }

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
        for line in
            fleet::fleet_rollout_lines(&plan, input.release_id, input.writable_db_configured)
        {
            eprintln!("{line}");
        }
        eprintln!();
    }

    // ── EXECUTION (serial, one host at a time, rollout order) ────────────────
    let mut outcomes = vec![fleet::HostOutcome::Untouched; total];
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
        // Repair the drifted marker as an early op — before the cutover's
        // record-previous-release reads it — so the on-disk marker matches the
        // proxy truth even if the rest of the deploy is later interrupted.
        if state.repair {
            ops.insert(
                0,
                exec::live_slot_marker_repair_op(cfg, state.slots.live_slot, input.public_port),
            );
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
                outcomes[index] = fleet::classify_failure(&err);
                // A one-host fleet keeps today's error verbatim: the per-host
                // executor already told the whole story, and inventing a fleet
                // vocabulary for one host would change pre-#1621 output.
                if single {
                    return Err(DeployError::Exec(err.to_string()));
                }
                let failed_step = fleet::failed_step_label(&err);
                eprintln!(
                    "\n\u{274C} rollout halted at {} (`{failed_step}`) \u{2014} the remaining \
                     hosts were not touched.\n",
                    host_plan.host,
                );
                // Print the per-host state on the way out: a halted rollout is
                // exactly when the operator has no other source of truth.
                for line in fleet::fleet_summary_lines(&plan, &outcomes, input.release_id) {
                    eprintln!("{line}");
                }
                return Err(fleet_halted(
                    &plan,
                    &outcomes,
                    host_plan.host.clone(),
                    failed_step,
                ));
            }
        }
    }

    // App deploy is committed on every host here: the cutovers succeeded and there
    // is no longer a rollback path. Only NOW provision the host MediaMTX unit
    // (write config/unit + restart) — deferring the mutation past this point means
    // a failed/rolled-back app deploy above never wrote or restarted MediaMTX
    // (#1974 Slice 7). No-op unless enabled (see fn).
    if single {
        let executor = &executors[0];
        provision_media_host(input.media_cfg, executor)?;
    }

    if single {
        eprintln!("\n\u{2705} Deploy complete. Roll back with `autumn deploy rollback`.");
    } else {
        for line in fleet::fleet_summary_lines(&plan, &outcomes, input.release_id) {
            eprintln!("{line}");
        }
        eprintln!(
            "\n\u{2705} Fleet deploy complete \u{2014} all {total} hosts serving {}.",
            input.release_id,
        );
    }
    Ok(())
}

/// Build the typed halt error from the recorded per-host outcomes (issue #1621,
/// AC-3).
///
/// Secrets discipline: every field is a host name or a `&'static str` op label —
/// never a shell line, a remote path, or a formatted driver error (a migration
/// failure's source can embed the database URL).
fn fleet_halted(
    plan: &fleet::FleetPlan,
    outcomes: &[fleet::HostOutcome],
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
    DeployError::FleetHalted {
        failed_host,
        failed_step,
        rolled_back: named(|o| matches!(o, fleet::HostOutcome::RolledBack { .. })),
        torn_down: named(|o| matches!(o, fleet::HostOutcome::TornDown { .. })),
        still_on_new: named(|o| {
            matches!(
                o,
                fleet::HostOutcome::Serving | fleet::HostOutcome::LiveOnNew { .. }
            )
        }),
    }
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
fn run_rollback(config: &AutumnConfig, resolved: &ResolvedDeployConfig) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy rollback\n");

    // Fail fast: same preflight/gate as `up`, before any remote call.
    let checks = collect_preflight(config, resolved);
    let failed = report_preflight(&checks);
    if failed > 0 {
        return Err(DeployError::PreflightFailed(failed));
    }

    // Show what a rollback will do before it runs (descriptive, not a dry-run gate).
    eprintln!("Rollback steps:");
    for (i, step) in build_rollback_plan(resolved).iter().enumerate() {
        eprintln!("  {}. [{}] {}", i + 1, step.label, step.description);
    }
    eprintln!();

    let target = exec::SshTarget::from_resolved(resolved)
        .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))?;
    let public_port = config.server.port;
    let proxy = proxy::KamalProxyController::new(resolved.readiness_timeout_secs)
        .with_tls_host(resolved.tls_host.clone());
    let executor = exec::SshExecutor::new(target);

    // Resolve the previous release to roll back to (non-zero error if none).
    let rollback_target = exec::resolve_rollback_target(resolved, public_port, &executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;
    eprintln!(
        "Rolling back {} to {} ({})\u{2026}\n",
        resolved.host.as_deref().unwrap_or_default(),
        rollback_target.release_dir,
        rollback_target.slot,
    );

    let ops = exec::rollback_ops(resolved, &proxy, &rollback_target);
    // If the rollback fails at or before the health-gated flip, disable the slot it
    // restarted so the original release is left cleanly serving (the flip never
    // moved traffic, and no marker is written until after the flip).
    let teardown = exec::rollback_teardown_ops(resolved, &rollback_target);
    exec::execute_rollback(&checks, &ops, &teardown, &executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;

    eprintln!("\n\u{2705} Rollback complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
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
        // Regression (P1 follow-up to #1956): `autumn deploy` must reload its
        // config under the TARGET deploy profile (default `prod`), so a signing
        // secret and DB URL that live ONLY under `[profile.prod]` are loaded,
        // graded, and uploaded — instead of the operator's ambient/dev values.
        //
        // This exercises the exact seam `run` → `load_runtime_config` uses: a
        // `ForcedProfileEnv` (which reports `AUTUMN_ENV=<deploy profile>`) fed to
        // `AutumnConfig::load_with_env`, so the loader layers `[profile.prod]` on
        // top. `AUTUMN_MANIFEST_DIR` points config loading at the temp project
        // WITHOUT mutating the process env or CWD (a `MockEnv` inner supplies it).
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
        // (`autumn_web::config::normalize_profile_name`) trims and case-folds
        // profile aliases, so `PROD`, `Production`, and ` production ` all boot
        // under the canonical `prod`. But the host's override-file lookup
        // (`profile_override_file_lookup_names`) keys off the RAW selector
        // spelling — it prefers `autumn-production.toml` over `autumn-prod.toml`
        // when the raw input was `production`. So `resolved.profile` (and the
        // `AUTUMN_ENV` value written from it) must preserve the operator's trimmed
        // raw spelling, NOT the alias-folded form, or override-file precedence
        // flips on hosts that have both files. Grading is normalized separately
        // (canonicalize at the grade site) so a weak secret under `PROD`/
        // `Production`/` production ` still FAILS preflight locally.
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

        // (3) STEP-LABEL SUBSET. Every printed step label either names a real
        // executed op or is one of the four deliberately MECHANISM-NEUTRAL labels
        // (`build` is local, `upload` executes as `upload-binary`, `cutover` as
        // `proxy-flip`, `drain` as `drain-old`). Partitioning — rather than a bare
        // `contains` — means a NEW plan step forces a deliberate decision here
        // instead of silently drifting away from the ops.
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
            vec!["build", "upload", "cutover", "drain"],
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
        // Finding K: the MUTATING MediaMTX provisioning must run only AFTER the app
        // deploy/cutover commits. The app deploy rolls back on failure (readiness
        // gate, migrations) and leaves the OLD release serving, and there is
        // deliberately no media teardown — so writing/restarting the host MediaMTX
        // unit BEFORE the app is committed would strand a rolled-back release
        // against a moved media daemon. We assert the source order of the `up`
        // path: the non-mutating preflight precedes the rollout loop, and
        // `provision_media_host` is invoked only after that loop has run every
        // host to completion.
        //
        // #1621 re-anchored the middle marker. `run_up` is now a fleet-shaped
        // driver (`run_up` prologue + `run_up_with` loop), so the commit point is
        // no longer a single `result.map_err(...)?` line but the per-host execution
        // LOOP — a strictly stronger statement: provisioning must follow the last
        // host's cutover, not merely one host's.
        let src = include_str!("deploy.rs");
        let up_body = src
            .split("fn run_up(")
            .nth(1)
            .and_then(|s| s.split("\nfn run_rollback(").next())
            .expect("run_up body present");

        let preflight_at = up_body
            .find("check_media_host_preflight(input.media_cfg")
            .expect("preflight call present on the up path");
        // The per-host cutovers happen inside this loop; a failed host returns from
        // inside it (after the per-host teardown) before anything below runs.
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
             rolled-back or halted rollout (which returns from inside the loop) never reaches it",
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
             deploy errors out inside the rollout loop)",
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

    const FLEET_RELEASE_ID: &str = "20260714T120000Z";
    const FLEET_PUBLIC_PORT: u16 = 3000;

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
                writable_db_configured: true,
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
                fleet::HostOutcome::Serving,
                fleet::HostOutcome::RolledBack {
                    failed_step: "migrate",
                },
            ],
            "web-b".to_owned(),
            "migrate",
        );

        let rendered = format!("{err}\n{err:?}");
        assert!(
            rendered.contains("web-b") && rendered.contains("migrate"),
            "the halt must name the failing host and step: {rendered}"
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
        // #1621 (AC-1, T1.6): the same differential proof on the first-deploy path,
        // which carries NO migrate op — an all-first-deploy fleet migrates nowhere,
        // exactly today's documented single-host limitation.
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
                    exec::MigrateStep::Skip
                )
                .as_slice(),
            "a one-host first deploy must be byte-identical to today's single-host first deploy"
        );
        assert!(
            !recorder.run_labels_for("203.0.113.10").contains(&"migrate"),
            "the first-deploy path carries no migrate op"
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
        // #1621 (AC-3). Host 2's readiness gate fails — a PRE-boundary failure, so
        // its candidate is torn down by the existing per-host auto-rollback and its
        // old release keeps serving. The rollout then HALTS: host 3 is never
        // touched. Host 1 is left on the new release; compensating it back is the
        // next slice's job, and the typed error already names it as such.
        let hosts = ["web-a", "web-b", "web-c"];
        let fleet = fleet_of(&hosts);
        let mut recorder = fleet::test_support::FleetRecorder::new();
        for host in hosts {
            recorder = script_redeploy(recorder, host);
        }
        recorder = recorder.fail("web-b", "readiness-gate");
        let fixture = FleetFixture::new();

        let err = run_up_with(&fixture.input(&fleet), |cfg| Ok(recorder.executor(cfg)))
            .expect_err("a mid-rollout failure must halt the rollout");

        match &err {
            DeployError::FleetHalted {
                failed_host,
                failed_step,
                rolled_back,
                torn_down,
                still_on_new,
            } => {
                assert_eq!(failed_host, "web-b", "the halt must name the failing host");
                assert_eq!(
                    *failed_step, "readiness-gate",
                    "the halt must name the failing step"
                );
                assert_eq!(
                    rolled_back,
                    &vec!["web-b".to_owned()],
                    "a pre-boundary failure rolls the failing host's candidate back"
                );
                assert!(
                    torn_down.is_empty(),
                    "a redeploy host is rolled back, not torn down: {torn_down:?}"
                );
                assert_eq!(
                    still_on_new,
                    &vec!["web-a".to_owned()],
                    "the already-cut-over hosts must be named so they can be compensated"
                );
            }
            other => panic!("expected a fleet halt, got: {other:?}"),
        }

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
}
