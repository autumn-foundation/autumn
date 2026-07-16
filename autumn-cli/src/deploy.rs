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
pub mod proxy;

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, DeployConfig, Env};

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
const DEPLOY_HOST_MISSING_HINT: &str =
    "Set `[deploy] host` in autumn.toml to your server's SSH-reachable address";

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
fn trimmed_deploy_profile(raw: &str) -> String {
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
    /// and, since #1952, the uploaded config manifest[s]). The deployed systemd
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
            detail: detail.into(),
            hint: None,
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: &'static str) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
            hint: Some(hint),
        }
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
        // Point the app's config loader at the shared dir where the uploaded
        // autumn.toml lives, so the deployed app loads the intended config
        // instead of built-in defaults (#1952). Non-secret, so it lives in the
        // unit's Environment= (not the 0600 env file).
        manifest_dir = cfg.shared_dir(),
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
        // Point the app's config loader at the shared dir where the uploaded
        // autumn.toml lives, so the deployed app loads the intended config
        // instead of built-in defaults (#1952). Non-secret → unit Environment=,
        // not the 0600 env file.
        manifest_dir = cfg.shared_dir(),
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
    let ambient_config = AutumnConfig::load().map_err(|e| DeployError::Config(e.to_string()))?;
    let deploy_cfg = ambient_config.deploy.unwrap_or_default();
    let resolved = ResolvedDeployConfig::resolve(&deploy_cfg, &resolve_project_name())
        .map_err(DeployError::Config)?;

    match action {
        // `plan` is a pure dry-run over `resolved` alone — it never grades or
        // uploads runtime VALUES, so it needs no reload under the target profile.
        DeployAction::Plan => {
            print_plan(&resolved);
            Ok(())
        }
        // `check`/`rollback`/`up` grade and (for `up`) upload the signing secret
        // and DB URL, so they must see those VALUES resolved under the TARGET
        // deploy profile — not the operator's ambient/dev config. Reload here.
        DeployAction::Check => run_check(&load_runtime_config(&resolved)?, &resolved),
        DeployAction::Rollback => run_rollback(&load_runtime_config(&resolved)?, &resolved),
        DeployAction::Up => run_up(&load_runtime_config(&resolved)?, &resolved),
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
    use autumn_web::config::OsEnv;
    // Gating base: the real OS env, but reports `AUTUMN_DOTENV=1` so
    // `should_load` loads `.env.<profile>` for a non-dev deploy profile —
    // without mutating the global process environment.
    let gating = ForcedProfileEnv {
        profile: resolved.profile.clone(),
        inner: OsEnv,
    };
    // Select the `.env.<profile>` overlay by the CANONICAL profile, so a
    // `[deploy] profile` alias like `production`/`PROD` still picks the same
    // `.env.prod` file that `AutumnConfig::load()` reads after profile
    // normalization — not `.env.production`/`.env.PROD`. Only the dotenv-overlay
    // SELECTION is canonicalized; `AUTUMN_ENV` and the TOML override-file
    // precedence (handled by `load_with_env`) still see the RAW spelling below.
    let dotenv_profile = canonicalize_deploy_profile(&resolved.profile);
    let inner = autumn_web::dotenv::os_env_with_dotenv_for_profile_using(&gating, &dotenv_profile)
        .map_err(|e| DeployError::Config(e.to_string()))?;
    let forced = ForcedProfileEnv {
        profile: resolved.profile.clone(),
        inner,
    };
    AutumnConfig::load_with_env(&forced).map_err(|e| DeployError::Config(e.to_string()))
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
fn collect_preflight(
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
        grade_ssh_reachability(
            resolved.host.as_deref(),
            resolved.ssh_port,
            SSH_PROBE_TIMEOUT,
        ),
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

fn print_plan(resolved: &ResolvedDeployConfig) {
    println!("\u{1F342} autumn deploy plan (dry-run)\n");
    println!("systemd unit ({}.service):\n", resolved.service_name);
    println!("{}", render_systemd_unit(resolved));

    println!("Deploy steps (zero-downtime):");
    for (i, step) in build_deploy_plan(resolved).iter().enumerate() {
        println!("  {}. [{}] {}", i + 1, step.label, step.description);
    }
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

/// Resolve the local project directory the deploy reads config from.
///
/// Mirrors autumn-web's `find_config_file_named`: `AUTUMN_MANIFEST_DIR` wins when
/// set (so an operator who redirects config loading also has that dir uploaded),
/// otherwise the current working directory — the project root the rest of the
/// deploy already treats as authoritative (the release binary is resolved as
/// `target/release/<app>`).
fn manifest_project_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AUTUMN_MANIFEST_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Locate the RAW project config manifest file(s) to upload for #1952: the base
/// `autumn.toml` plus, when present, the profile-override sibling
/// `autumn-<profile>.toml` for the target deploy profile.
///
/// Pure over the passed `dir`/`profile` so it is unit-testable against a temp
/// dir. We upload the raw files (NOT a flattened/merged config) because the app
/// applies its `[profile.<AUTUMN_ENV>]` overlay at runtime — `AUTUMN_ENV` is already
/// set in the uploaded env file — so shipping the raw manifest(s) preserves the
/// profile structure and matches the repo exactly.
///
/// Both the operator's raw spelling and its canonical form are checked for the
/// sibling (e.g. `[deploy] profile = "production"` uploads whichever of
/// `autumn-production.toml` / `autumn-prod.toml` exist), matching the runtime's
/// own profile-override-file lookup so the deployed app resolves config identically.
fn manifest_uploads_in(dir: &Path, profile: &str) -> Vec<exec::ManifestUpload> {
    let mut uploads = Vec::new();

    let base = dir.join("autumn.toml");
    if base.is_file() {
        uploads.push(exec::ManifestUpload {
            local: base,
            remote_basename: "autumn.toml".to_owned(),
        });
    }

    let mut sibling_names = vec![profile.trim().to_owned()];
    let canonical = canonicalize_deploy_profile(profile);
    if !sibling_names.contains(&canonical) {
        sibling_names.push(canonical);
    }
    for name in sibling_names {
        if name.is_empty() {
            continue;
        }
        let basename = format!("autumn-{name}.toml");
        if uploads.iter().any(|u| u.remote_basename == basename) {
            continue;
        }
        let sibling = dir.join(&basename);
        if sibling.is_file() {
            uploads.push(exec::ManifestUpload {
                local: sibling,
                remote_basename: basename,
            });
        }
    }

    uploads
}

/// Locate the config manifest(s) to upload for the resolved deploy, reading from
/// the local project directory ([`manifest_project_dir`]).
fn locate_manifest_uploads(resolved: &ResolvedDeployConfig) -> Vec<exec::ManifestUpload> {
    manifest_uploads_in(&manifest_project_dir(), &resolved.profile)
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
    format!("Uploading project config to shared/: {}", names.join(", "))
}

/// Perform a real deploy (issue #1607, Slices 1–3).
///
/// Runs the same preflight as `check` and aborts before touching the server if
/// anything fails (AC-6). It then probes the target ([`exec::detect_deploy_mode`])
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

fn run_up(config: &AutumnConfig, resolved: &ResolvedDeployConfig) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy up\n");

    // Fail fast: run the full preflight and abort BEFORE any remote call.
    let checks = collect_preflight(config, resolved);
    let failed = report_preflight(&checks);
    if failed > 0 {
        return Err(DeployError::PreflightFailed(failed));
    }

    // Host presence is guaranteed by the passing ssh_reachability grader above.
    let target = exec::SshTarget::from_resolved(resolved)
        .ok_or_else(|| DeployError::Config(DEPLOY_HOST_MISSING_DETAIL.to_owned()))?;
    let binary = resolve_release_binary(resolved)?;
    let env_file = build_env_file(config, resolved);
    // Locate the project config manifest(s) to upload so the deployed app loads
    // the intended config rather than silent built-in defaults (#1952), and print
    // a loud line either confirming the upload or warning when there is no
    // autumn.toml to ship.
    let manifests = locate_manifest_uploads(resolved);
    eprintln!("{}", manifest_preflight_notice(&manifests));
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
    let executor = exec::SshExecutor::new(target);
    let release_dir = format!("{}/{release_id}", resolved.releases_dir());

    // Probe the target to choose first-deploy vs zero-downtime redeploy.
    let mode = exec::detect_deploy_mode(resolved, &executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;

    let (ops, teardown, banner, is_first) = match mode {
        exec::DeployMode::First => {
            let plan = exec::SlotPlan::first(public_port);
            let unit = render_app_unit(
                resolved,
                &release_dir,
                plan.candidate_port,
                plan.candidate_slot,
            );
            let ops = exec::first_deploy_ops(
                resolved,
                &proxy,
                &unit,
                env_file,
                &binary,
                &manifests,
                &release_id,
                &plan,
            );
            // First-deploy teardown must also unlink `current` and clear the
            // live-slot marker that first_deploy_ops creates — otherwise a failed
            // first deploy leaves them behind and the next `deploy up` wrongly
            // takes the redeploy path with nothing serving.
            let teardown = exec::first_deploy_teardown_ops(resolved, &release_id, &plan);
            (ops, teardown, "first deploy".to_owned(), true)
        }
        exec::DeployMode::Redeploy { live_slot } => {
            let plan = exec::SlotPlan::redeploy(public_port, live_slot);
            let unit = render_app_unit(
                resolved,
                &release_dir,
                plan.candidate_port,
                plan.candidate_slot,
            );
            let ops = exec::cutover_ops(
                resolved,
                &proxy,
                &unit,
                env_file,
                &binary,
                &manifests,
                &release_id,
                &plan,
            );
            let teardown = exec::candidate_teardown_ops(resolved, &release_id, &plan);
            (
                ops,
                teardown,
                format!(
                    "zero-downtime redeploy ({} \u{2192} {})",
                    plan.live_slot, plan.candidate_slot
                ),
                false,
            )
        }
    };

    eprintln!(
        "Deploying release {release_id} to {} ({banner})\u{2026}\n",
        resolved.host.as_deref().unwrap_or_default()
    );
    let result = if is_first {
        exec::execute_first_deploy(&checks, &ops, &teardown, &executor)
    } else {
        exec::execute_redeploy(&checks, &ops, &teardown, &executor)
    };
    result.map_err(|e| DeployError::Exec(e.to_string()))?;

    eprintln!("\n\u{2705} Deploy complete. Roll back with `autumn deploy rollback`.");
    Ok(())
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
        // #1952: the unit points config loading at the shared dir where the
        // uploaded autumn.toml lives.
        assert!(unit.contains("Environment=AUTUMN_MANIFEST_DIR=/srv/autumn/shop/shared"));
    }

    #[test]
    fn app_unit_sets_manifest_dir_to_shared_for_uploaded_config() {
        // #1952: the deployed slot unit (the one actually written by a deploy)
        // sets AUTUMN_MANIFEST_DIR to the persistent shared dir so the app's
        // config loader reads the uploaded autumn.toml instead of built-in
        // defaults. Non-secret → an `Environment=` line, not the 0600 env file.
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
            unit.contains("Environment=AUTUMN_MANIFEST_DIR=/srv/autumn/shop/shared"),
            "slot unit must set AUTUMN_MANIFEST_DIR to the shared dir: {unit}"
        );
        // The env file (secrets) still lives in the same shared dir at 0600.
        assert!(unit.contains("EnvironmentFile=/srv/autumn/shop/shared/autumn.env"));
    }

    #[test]
    fn manifest_uploads_base_only_when_no_profile_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\nport = 8080\n")
            .expect("write autumn.toml");
        let uploads = manifest_uploads_in(dir.path(), "prod");
        assert_eq!(uploads.len(), 1, "only autumn.toml is present");
        assert_eq!(uploads[0].remote_basename, "autumn.toml");
        assert_eq!(uploads[0].local, dir.path().join("autumn.toml"));
    }

    #[test]
    fn manifest_uploads_include_profile_sibling_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-prod.toml"), "[server]\n").expect("write sibling");
        let uploads = manifest_uploads_in(dir.path(), "prod");
        let names: Vec<&str> = uploads.iter().map(|u| u.remote_basename.as_str()).collect();
        assert_eq!(names, vec!["autumn.toml", "autumn-prod.toml"]);
    }

    #[test]
    fn manifest_uploads_check_canonical_profile_sibling() {
        // `[deploy] profile = "production"` should still ship the canonical
        // `autumn-prod.toml` if that is what the repo carries, matching the
        // runtime's own override-file lookup.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("autumn.toml"), "[server]\n").expect("write base");
        std::fs::write(dir.path().join("autumn-prod.toml"), "[server]\n").expect("write sibling");
        let uploads = manifest_uploads_in(dir.path(), "production");
        assert!(
            uploads
                .iter()
                .any(|u| u.remote_basename == "autumn-prod.toml"),
            "canonical prod sibling is uploaded for raw profile `production`"
        );
    }

    #[test]
    fn manifest_uploads_empty_when_no_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uploads = manifest_uploads_in(dir.path(), "prod");
        assert!(uploads.is_empty(), "no autumn.toml → nothing to upload");
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
}
