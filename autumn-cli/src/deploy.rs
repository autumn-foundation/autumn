//! `autumn deploy` — push-button, zero-downtime deploys to a VPS (issue #1607).
//!
//! This slice implements the locally-verifiable spine of the command:
//!
//! - **`check`** runs a preflight (SSH reachability, signing secret, database
//!   URL, and a `migrate check`) and reports pass/fail, exiting non-zero if any
//!   grader fails.
//! - **`plan`** renders the systemd service unit and the ordered zero-downtime
//!   rollout plan as a pure dry-run — it touches nothing remote.
//! - **`rollback`** prints the rollback plan (also a dry-run in this slice).
//!
//! Real remote SSH execution, live cutover/rollback, and the CI end-to-end
//! harness land in follow-ups. The plan/unit generators here are pure functions
//! so they can be unit-tested without a server, and the preflight graders are
//! shared with `autumn doctor`.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, DeployConfig};

/// Bounded timeout for the SSH-reachability preflight probe. Kept short so the
/// check fails fast on an unreachable host instead of hanging on a dropped SYN.
const SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default migrations directory scanned by the `migrate check` preflight grader.
const MIGRATIONS_DIR: &str = "migrations";

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
}

/// Which `autumn deploy` subcommand to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployAction {
    /// Run the preflight and report pass/fail.
    Check,
    /// Print the systemd unit and the ordered deploy plan (dry-run).
    Plan,
    /// Print the rollback plan (dry-run).
    Rollback,
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

/// Grade SSH reachability: a bounded, non-interactive TCP connect to the SSH
/// port. This slice does not shell out to `ssh` (that lands with real remote
/// execution) — an honest "the port is reachable" probe is sufficient here.
#[must_use]
pub fn grade_ssh_reachability(
    host: Option<&str>,
    ssh_port: u16,
    timeout: Duration,
) -> PreflightCheck {
    let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) else {
        return PreflightCheck::fail(
            "ssh_reachability",
            "no target host configured",
            "Set `[deploy] host` in autumn.toml to your server's SSH-reachable address",
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

/// Grade signing-secret presence. Never prints the secret value.
#[must_use]
pub fn grade_signing_secret(secret: Option<&str>) -> PreflightCheck {
    match secret.map(str::trim).filter(|s| !s.is_empty()) {
        Some(_) => PreflightCheck::pass("signing_secret", "signing secret is configured"),
        None => PreflightCheck::fail(
            "signing_secret",
            "no signing secret configured",
            "Set AUTUMN_SECURITY__SIGNING_SECRET (generate with `openssl rand -hex 32`)",
        ),
    }
}

/// Grade database-URL presence. Never prints the URL (it may embed credentials).
///
/// The URL is only *required* when the app is database-backed — either a
/// migrations directory exists (the same presence check [`grade_migrate_check`]
/// uses, so the two graders agree) or a `[database]` section is configured. A
/// zero-dependency, daemon-style app with neither has nothing to connect to, so
/// the grader passes with a "no database configured" note instead of failing
/// preflight unconditionally.
#[must_use]
pub fn grade_database_url(
    url: Option<&str>,
    migrations_dir: &Path,
    database_configured: bool,
) -> PreflightCheck {
    match url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(_) => PreflightCheck::pass("database_url", "database URL is configured"),
        None if !migrations_dir.exists() && !database_configured => PreflightCheck::pass(
            "database_url",
            "no database configured (nothing to connect to)",
        ),
        None => PreflightCheck::fail(
            "database_url",
            "no writable database URL: `autumn migrate` needs a primary/control \
             (`database.primary_url`) or shard-primary URL; `database.replica_url` \
             alone can't run migrations",
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

/// Build the ordered, zero-downtime deploy plan (AC-2/3/4).
///
/// The sequence encodes the framework's `/live`-`/ready`-drain contract:
/// migrations run *before* cutover (a failed migration leaves the old version
/// serving), the new release must report `/ready` within the bounded window
/// (else roll back), traffic flips only after readiness, and the old release is
/// drained and pruned last.
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
            "start",
            "Start the new release as a candidate (old version still serving traffic)",
        ),
        DeployStep::new(
            "readiness-gate",
            format!(
                "Poll the new release's /ready within {}s — roll back on timeout",
                cfg.readiness_timeout_secs
            ),
        ),
        DeployStep::new(
            "cutover",
            format!(
                "Flip the {} symlink to the new release and reload the {} service",
                cfg.current_symlink(),
                cfg.service_name
            ),
        ),
        DeployStep::new(
            "drain",
            "Drain and stop the old release once cutover completes",
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
        DeployAction::Rollback => {
            print_rollback(&resolved);
            Ok(())
        }
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
        grade_signing_secret(config.security.signing_secret.secret.as_deref()),
        grade_database_url(
            resolve_writable_db_url(&config.database),
            Path::new(MIGRATIONS_DIR),
            database_configured,
        ),
        grade_migrate_check(Path::new(MIGRATIONS_DIR)),
    ]
}

fn run_check(config: &AutumnConfig, resolved: &ResolvedDeployConfig) -> Result<(), DeployError> {
    eprintln!("\u{1F342} autumn deploy check\n");

    let checks = collect_preflight(config, resolved);
    let mut failed = 0_usize;
    for check in &checks {
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

fn print_rollback(resolved: &ResolvedDeployConfig) {
    println!("\u{1F342} autumn deploy rollback plan (dry-run)\n");
    println!("Live rollback lands in a follow-up; this prints the plan only.\n");
    println!("Rollback steps:");
    for (i, step) in build_rollback_plan(resolved).iter().enumerate() {
        println!("  {}. [{}] {}", i + 1, step.label, step.description);
    }
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
        let readiness = labels
            .iter()
            .position(|&l| l == "readiness-gate")
            .expect("readiness-gate step");
        let cutover = labels
            .iter()
            .position(|&l| l == "cutover")
            .expect("cutover step");

        // Migrations run before the readiness gate, which runs before cutover.
        assert!(
            migrate < readiness,
            "migrations must precede the readiness gate"
        );
        assert!(readiness < cutover, "readiness gate must precede cutover");

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
        let present = grade_signing_secret(Some("super-secret-value"));
        assert!(present.passed);
        assert!(!present.detail.contains("super-secret-value"));

        let missing = grade_signing_secret(None);
        assert!(!missing.passed);
        assert!(!grade_signing_secret(Some("   ")).passed);
    }

    #[test]
    fn database_url_grader_never_echoes_value() {
        // Force the DB-backed path so the missing-URL case still fails: a
        // configured `[database]` marks the app as database-backed.
        let absent = Path::new("autumn-deploy-no-such-migrations-dir-echo-guard");
        let present = grade_database_url(Some("postgres://user:pw@host/db"), absent, true);
        assert!(present.passed);
        assert!(!present.detail.contains("pw"));
        assert!(!grade_database_url(None, absent, true).passed);
    }

    #[test]
    fn database_url_grader_passes_for_db_free_app() {
        // No migrations directory and no configured `[database]`: a
        // zero-dependency daemon app has nothing to connect to, so preflight
        // must pass rather than require a URL.
        let tmp = tempfile::TempDir::new().unwrap();
        let absent_migrations = tmp.path().join("migrations");
        assert!(!absent_migrations.exists());
        let check = grade_database_url(None, &absent_migrations, false);
        assert!(
            check.passed,
            "DB-free app should pass the DB-URL preflight: {}",
            check.detail
        );
    }

    #[test]
    fn database_url_grader_fails_for_db_backed_app_without_url() {
        // A migrations directory (the same presence check `grade_migrate_check`
        // uses) marks the app as database-backed, so a missing URL must fail
        // with an actionable hint.
        let tmp = tempfile::TempDir::new().unwrap();
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir(&migrations).unwrap();
        let check = grade_database_url(None, &migrations, false);
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
        let check = grade_database_url(url, &migrations, true);
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
        let check = grade_database_url(url, &migrations, true);
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
}
