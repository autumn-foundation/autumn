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

use autumn_web::config::{AutumnConfig, DeployConfig};

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
}

impl ResolvedDeployConfig {
    /// Resolve a [`DeployConfig`] against the project name, filling in the
    /// `app_name` → `app_dir` → `service_name` default chain.
    #[must_use]
    pub fn resolve(cfg: &DeployConfig, project_name: &str) -> Self {
        let non_blank = |s: &Option<String>| {
            s.as_ref()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };

        let app_name = non_blank(&cfg.app_name).unwrap_or_else(|| project_name.to_owned());
        let app_dir = non_blank(&cfg.app_dir).unwrap_or_else(|| format!("/srv/autumn/{app_name}"));
        let service_name = non_blank(&cfg.service_name).unwrap_or_else(|| app_name.clone());

        Self {
            host: non_blank(&cfg.host),
            user: cfg.user.clone(),
            ssh_port: cfg.ssh_port,
            app_name,
            app_dir,
            service_name,
            readiness_timeout_secs: cfg.readiness_timeout_secs,
            keep_releases: cfg.keep_releases,
        }
    }

    /// Remote path to the `EnvironmentFile` holding secrets (mode `0600`), kept
    /// out of the world-readable systemd unit.
    #[must_use]
    pub fn env_file(&self) -> String {
        format!("{}/shared/autumn.env", self.app_dir)
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
                "Prune old releases, retaining the most recent {}",
                cfg.keep_releases
            ),
        ),
    ]
}

/// Build the rollback plan: point `current` back at the previous release,
/// restart the service, and re-probe `/ready`.
#[must_use]
pub fn build_rollback_plan(cfg: &ResolvedDeployConfig) -> Vec<DeployStep> {
    vec![
        DeployStep::new(
            "select-previous",
            format!(
                "Select the previous release under {} to roll back to",
                cfg.releases_dir()
            ),
        ),
        DeployStep::new(
            "repoint",
            format!(
                "Point the {} symlink back at the previous release",
                cfg.current_symlink()
            ),
        ),
        DeployStep::new(
            "restart",
            format!(
                "Restart the {} service on the previous release",
                cfg.service_name
            ),
        ),
        DeployStep::new(
            "readiness-gate",
            format!(
                "Re-probe /ready within {}s to confirm the rollback is healthy",
                cfg.readiness_timeout_secs
            ),
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
    let config = AutumnConfig::load().map_err(|e| DeployError::Config(e.to_string()))?;
    let deploy_cfg = config.deploy.clone().unwrap_or_default();
    let resolved = ResolvedDeployConfig::resolve(&deploy_cfg, &resolve_project_name());

    match action {
        DeployAction::Check => run_check(&config, &resolved),
        DeployAction::Plan => {
            print_plan(&resolved);
            Ok(())
        }
        DeployAction::Rollback => run_rollback(&config, &resolved),
        DeployAction::Up => run_up(&config, &resolved),
    }
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
        grade_signing_secret(
            config.security.signing_secret.secret.as_deref(),
            &config.security.signing_secret.previous_secrets,
            is_production_profile(config.profile.as_deref()),
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
fn is_production_profile(profile: Option<&str>) -> bool {
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
/// `EnvironmentFile`. Only the values the app needs at runtime (the signing
/// secret and the writable database URL) are emitted, and the result is wrapped
/// in [`exec::Secret`] so it is never logged (AC-5).
fn build_env_file(config: &AutumnConfig) -> exec::Secret {
    let mut body = String::new();
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
    let env_file = build_env_file(config);
    let release_id = default_release_id();
    let public_port = config.server.port;
    let proxy = proxy::KamalProxyController::new(resolved.readiness_timeout_secs);
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
                &release_id,
                &plan,
            );
            let teardown = exec::candidate_teardown_ops(resolved, &release_id, &plan);
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
    let proxy = proxy::KamalProxyController::new(resolved.readiness_timeout_secs);
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
    exec::execute_rollback(&checks, &ops, &executor)
        .map_err(|e| DeployError::Exec(e.to_string()))?;

    eprintln!("\n\u{2705} Rollback complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved_defaults() -> ResolvedDeployConfig {
        ResolvedDeployConfig::resolve(&DeployConfig::default(), "myapp")
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
        let resolved = ResolvedDeployConfig::resolve(&cfg, "ignored");
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
        let resolved = ResolvedDeployConfig::resolve(&cfg, "fallback");
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
        );
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
    fn rollback_plan_repoints_restarts_and_reprobes() {
        let resolved = resolved_defaults();
        let plan = build_rollback_plan(&resolved);
        let labels: Vec<&str> = plan.iter().map(|s| s.label).collect();
        assert!(labels.contains(&"repoint"));
        let restart = labels
            .iter()
            .position(|&l| l == "restart")
            .expect("restart step");
        let reprobe = labels
            .iter()
            .position(|&l| l == "readiness-gate")
            .expect("re-probe step");
        assert!(
            restart < reprobe,
            "restart must precede the /ready re-probe"
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
