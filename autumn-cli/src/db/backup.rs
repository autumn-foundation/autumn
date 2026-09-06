//! `autumn db backup` / `autumn db restore` — logical dump/restore for
//! self-hosted, daemon, and single-binary Autumn deployments (issue #1595).
//!
//! These commands shell out to `pg_dump` / `pg_restore`, resolving the database
//! URL(s) through the **exact same** code path as `autumn migrate` / the other
//! `autumn db` commands ([`crate::migrate::resolve_primary_url`],
//! [`crate::migrate::resolve_shard_database_urls_from_sources`]) so a backup
//! captures precisely the databases the running app uses — control plus every
//! configured shard — under the active profile/env overlay. The destructive
//! `restore` is gated by the same production guard as `autumn db drop`
//! (`guard_destructive`).
//!
//! # Artifact layout (S3-bolt-on friendly — issue #1619)
//!
//! A single `backup` run writes one self-describing *run directory*:
//!
//! ```text
//! <dir>/<profile>/<timestamp>/
//!     manifest.json          # version, created_at, profile, format, targets[]
//!     control.dump           # pg_dump -Fc (default; compressed) — or control.sql (plain)
//!     shard-<name>.dump      # one per configured shard
//! ```
//!
//! The run directory is the atomic unit of both retention (`--keep N`) and any
//! future offsite upload (#1619 can enumerate `manifest.json` and push the whole
//! prefix). Nothing outside this directory is touched.
//!
//! # Zero-external-tools for managed Postgres (AC #2)
//!
//! [`PgTools::locate`](crate::db::backup::PgTools::locate) resolves
//! `pg_dump`/`pg_restore` from, in order: an
//! explicit `AUTUMN_PG_BIN_DIR`, the managed-Postgres bundle's `bin` directory
//! (derived from `AUTUMN_MANAGED_PG_DATA_DIR`, which `autumn serve --bundled-pg`
//! sets), then the `PATH`. A managed-pg daemon therefore needs no externally
//! installed client tools.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use autumn_web::alerts::{Alert, AlertChannel, AlertCondition};

use crate::db::s3::{self, S3Client, S3Config, S3Credentials};
use crate::db::sqlite_snapshot;
use crate::migrate;

/// Environment variable that pins the directory holding `pg_dump`/`pg_restore`.
/// Highest precedence in [`PgTools::locate`](crate::db::backup::PgTools::locate); lets an operator point at a
/// specific client-tools install.
const PG_BIN_DIR_ENV: &str = "AUTUMN_PG_BIN_DIR";

/// The managed-Postgres data-dir env var (`autumn serve --bundled-pg` sets it).
/// Mirrors `autumn_web::managed_pg::MANAGED_PG_DATA_DIR_ENV`; duplicated as a
/// private constant so a backup can locate the bundled client binaries without
/// depending on a managed-pg being constructed.
const MANAGED_PG_DATA_DIR_ENV: &str = "AUTUMN_MANAGED_PG_DATA_DIR";

/// Default directory (relative to the project root) that backup run directories
/// are written under when `--dir` is not given.
const DEFAULT_BACKUP_DIR: &str = "backups";

/// Upper bound on same-timestamp run-directory disambiguation attempts before a
/// backup gives up. Reaching this means ~1000 backups landed in the same second
/// for one profile, which is pathological — surfacing an error beats looping.
const MAX_RUN_DIR_ATTEMPTS: usize = 1000;

/// Marker `pg_dump` writes at the very end of a *plain* SQL dump. Its presence
/// is the integrity signal for plain-format artifacts (a truncated/partial dump
/// will not contain it).
const PLAIN_COMPLETE_MARKER: &str = "PostgreSQL database dump complete";

/// Backup artifact format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackupFormat {
    /// `pg_dump -Fc` custom archive — compressed, restored with `pg_restore`.
    /// The default: compressed on disk and integrity-checkable via
    /// `pg_restore --list`.
    #[default]
    Custom,
    /// Plain `pg_dump` SQL text (uncompressed), restored with `psql`.
    Plain,
}

impl BackupFormat {
    /// The per-target artifact file extension for this format.
    const fn extension(self) -> &'static str {
        match self {
            Self::Custom => "dump",
            Self::Plain => "sql",
        }
    }

    /// The `pg_dump` `--format` flag value for this format.
    const fn pg_dump_format_flag(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Plain => "plain",
        }
    }

    /// Parse the `--format` CLI value. Accepts the two documented spellings.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "custom" | "c" => Ok(Self::Custom),
            "plain" | "sql" | "p" => Ok(Self::Plain),
            other => Err(format!(
                "unknown backup format {other:?} (expected `custom` or `plain`)"
            )),
        }
    }
}

/// Which databases a backup/restore run operates on. Mirrors
/// [`crate::migrate::MigrateTarget`] so `--shard` / `--control-only` behave
/// identically to `autumn migrate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSelector {
    /// Control database plus every configured shard (the default).
    All,
    /// Only the control database.
    ControlOnly,
    /// A single shard, addressed by its configured name.
    Shard(String),
}

/// Arguments for `autumn db backup`.
#[derive(Debug, Clone)]
pub struct BackupArgs {
    /// Profile overlay to resolve the connection under (see `db create`).
    pub profile: Option<String>,
    /// Root directory for backup run directories (default: `./backups`).
    pub dir: Option<PathBuf>,
    /// Artifact format (default: custom/compressed).
    pub format: BackupFormat,
    /// Retention: keep only the newest N run directories for this profile,
    /// pruning older ones after a successful backup. `None` disables pruning.
    pub keep: Option<usize>,
    /// Which databases to capture.
    pub target: TargetSelector,
    /// Upload the completed run to the configured offsite destination after
    /// local verification + prune (issue #1619). The configured
    /// `backup.offsite.auto_upload = true` has the same effect without the flag.
    pub upload: bool,
}

/// Arguments for `autumn db restore`.
#[derive(Debug, Clone)]
pub struct RestoreArgs {
    /// Path to a backup run directory (or a single artifact file) to restore.
    pub artifact: PathBuf,
    /// Profile overlay to resolve the connection under.
    pub profile: Option<String>,
    /// Bypass the production guard (mirrors `autumn db drop --force`).
    pub force: bool,
    /// Restore only this shard from the artifact (mirrors `--shard`).
    pub shard: Option<String>,
    /// Treat `artifact` as an offsite reference (`<profile>/<timestamp|latest>`,
    /// with an optional `offsite:` prefix) and restore from the offsite
    /// destination instead of a local path (issue #1619). An `offsite:` prefix
    /// on `artifact` implies this even when the flag is not set.
    pub offsite: bool,
}

/// Failure modes for backup/restore. `Display` is credential-safe: no variant
/// ever embeds a resolved URL (only parsed host/port/db), matching the rest of
/// the `db` command family.
#[derive(Debug)]
pub enum BackupError {
    /// No database URL could be resolved from config or environment.
    NoUrl,
    /// A named shard was requested but not found in the resolved topology.
    UnknownShard { name: String, known: Vec<String> },
    /// `pg_dump`/`pg_restore`/`psql` was not found on PATH or in a bundle.
    ToolMissing { tool: String },
    /// A shelled-out tool exited non-zero. Carries the tool name and a
    /// credential-safe context string.
    ToolFailed { tool: String, context: String },
    /// A `SQLite` snapshot or restore failed (issue #1909). `detail` is a
    /// [`sqlite_snapshot::SnapshotError`] rendering — a file path, never a
    /// credential (`SQLite` targets carry none).
    SqliteFailed {
        /// `"backup"` or `"restore"`.
        op: &'static str,
        /// Operator-facing detail.
        detail: String,
    },
    /// A filesystem operation failed.
    Io {
        context: String,
        source: std::io::Error,
    },
    /// A produced or supplied artifact failed its integrity check.
    IntegrityFailed { detail: String },
    /// The restore artifact path does not exist or has no recognizable layout.
    BadArtifact { detail: String },
    /// A destructive restore was refused because the active profile is
    /// production and `--force` was not supplied.
    ProductionRefused { profile: String },
    /// `--upload` (or an offsite restore) was requested but no
    /// `[backup.offsite]` section is configured.
    OffsiteNotConfigured,
    /// The `[backup.offsite]` section is present but invalid (e.g. no bucket).
    OffsiteConfig { detail: String },
    /// The offsite destination points at the same bucket+endpoint as the app's
    /// user-facing blob storage and `allow_shared_bucket` was not set (AC #3).
    /// Carries only the bucket name (not a credential).
    SharedBucketRefused { bucket: String },
    /// A credential could not be read: the config names an env var that is unset
    /// (or names no env var at all). Carries the VARIABLE NAME, never a value.
    MissingCredentialEnv { var: String },
    /// The local backup succeeded but the offsite upload/verify failed — the
    /// unambiguous split outcome (AC #2/#6). The local artifact is intact.
    OffsiteUploadFailed { local_path: String, detail: String },
    /// A non-split offsite operation (list / download / config load) failed.
    /// `detail` is credential-safe (S3 status/code, never secrets).
    Offsite { op: &'static str, detail: String },
    /// An offsite restore was requested (`--offsite` or an `offsite:` prefix) but
    /// the reference is malformed. Carries the bad reference (no secrets). The
    /// restore MUST fail rather than fall through to a local artifact.
    InvalidOffsiteRef { reference: String },
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoUrl => write!(
                f,
                "No database URL found.\n  Set database.primary_url (or database.url) in autumn.toml, \
                 or set AUTUMN_DATABASE__PRIMARY_URL / AUTUMN_DATABASE__URL / DATABASE_URL."
            ),
            Self::UnknownShard { name, known } => {
                let detail = if known.is_empty() {
                    "No [[database.shards]] entries found in autumn.toml or environment.".to_owned()
                } else {
                    format!("Known shards: {}", known.join(", "))
                };
                write!(f, "Unknown shard {name:?}.\n  {detail}")
            }
            Self::ToolMissing { tool } => write!(
                f,
                "`{tool}` was not found.\n  Install the PostgreSQL client tools, set \
                 {PG_BIN_DIR_ENV} to their directory, or run against a managed-Postgres app \
                 (which bundles them)."
            ),
            Self::ToolFailed { tool, context } => {
                write!(f, "`{tool}` failed: {context}")
            }
            Self::SqliteFailed { op, detail } => write!(f, "SQLite {op} failed: {detail}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::IntegrityFailed { detail } => write!(
                f,
                "Backup integrity check failed: {detail}\n  The artifact was NOT reported as \
                 successful; any partial files were removed."
            ),
            Self::BadArtifact { detail } => write!(f, "Cannot restore: {detail}"),
            Self::ProductionRefused { profile } => write!(
                f,
                "Refusing to restore over the {profile:?} profile database.\n  \
                 Re-run with --force if you really mean it (this overwrites data)."
            ),
            Self::OffsiteNotConfigured => write!(
                f,
                "No offsite destination is configured.\n  Add a [backup.offsite] section \
                 (with [backup.offsite.s3] bucket/region/endpoint and *_env credential \
                 indirection) to autumn.toml, or set AUTUMN_BACKUP__OFFSITE__* env vars."
            ),
            Self::OffsiteConfig { detail } => {
                write!(f, "Invalid [backup.offsite] configuration: {detail}")
            }
            Self::SharedBucketRefused { bucket } => write!(
                f,
                "The offsite destination bucket {bucket:?} is the same as the app's \
                 [storage.s3] bucket at the same endpoint.\n  A shared bucket is a deliberate \
                 choice: set backup.offsite.allow_shared_bucket = true to opt in, or point the \
                 offsite backup at a distinct bucket."
            ),
            Self::MissingCredentialEnv { var } => write!(
                f,
                "Offsite credential environment variable {var:?} is not set (or the config \
                 names no *_env variable).\n  Set it to the S3 access key / secret, or fix \
                 backup.offsite.s3.access_key_id_env / secret_access_key_env."
            ),
            Self::OffsiteUploadFailed { local_path, detail } => write!(
                f,
                "Local backup OK at {local_path}\n  OFFSITE UPLOAD FAILED: {detail}\n  \
                 The local artifact is intact — re-run `autumn db backup --upload` after \
                 fixing the offsite destination."
            ),
            Self::Offsite { op, detail } => write!(f, "Offsite {op} failed: {detail}"),
            Self::InvalidOffsiteRef { reference } => write!(
                f,
                "Invalid offsite reference {reference:?}.\n  Expected \
                 offsite:<profile>/<timestamp|latest> (for example offsite:prod/latest)."
            ),
        }
    }
}

impl BackupError {
    fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

/// Which mechanism captures and restores ONE target (issue #1909).
///
/// [`BackupFormat`] grades Postgres artifacts only. A `SQLite` target is always a
/// `.sqlite` file, whatever `--format` says. Each manifest entry records its own
/// backend, so a mixed run restores every entry through the mechanism that wrote
/// it. A manifest without the field is Postgres.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TargetBackend {
    /// `pg_dump`/`pg_restore`/`psql`. The default, and what an absent manifest
    /// field means.
    #[default]
    Postgres,
    /// `SQLite`'s own `VACUUM INTO` snapshot ([`crate::db::sqlite_snapshot`]).
    Sqlite,
}

impl TargetBackend {
    /// Classify a resolved connection URL. Anything that is not an explicit
    /// `sqlite:`/`file:` target is Postgres, mirroring `autumn migrate`'s rule
    /// (`is_sqlite_target`): an unrecognized string keeps the historical path.
    fn detect(url: &str) -> Self {
        match autumn_web::config::DatabaseBackend::detect(url) {
            Some(autumn_web::config::DatabaseBackend::Sqlite) => Self::Sqlite,
            _ => Self::Postgres,
        }
    }

    /// The manifest spelling.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
        }
    }

    /// Read a manifest spelling. An empty or unknown value is Postgres, so a
    /// manifest without the field restores exactly as before.
    fn from_manifest(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("sqlite") {
            Self::Sqlite
        } else {
            Self::Postgres
        }
    }

    /// The artifact file extension for this backend and (Postgres) format.
    const fn extension(self, format: BackupFormat) -> &'static str {
        match self {
            Self::Postgres => format.extension(),
            Self::Sqlite => "sqlite",
        }
    }
}

/// A single database captured by (or to be restored from) a backup run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedTarget {
    /// Stable label: `"control"` or `"shard:<name>"`.
    label: String,
    /// The resolved connection URL (never printed).
    url: String,
    /// Which backend captures this target.
    backend: TargetBackend,
}

// ─── Entry points ───────────────────────────────────────────────────────────

/// Entry point for `autumn db backup`. Prints a credential-safe message and
/// exits non-zero on failure.
pub fn run_backup(args: &BackupArgs) {
    eprintln!("\u{1F342} autumn db backup\n");
    if let Err(e) = backup(args) {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

/// Entry point for `autumn db restore`. Prints a credential-safe message and
/// exits non-zero on failure.
pub fn run_restore(args: &RestoreArgs) {
    eprintln!("\u{1F342} autumn db restore\n");
    if let Err(e) = restore(args) {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

// ─── Backup ─────────────────────────────────────────────────────────────────

pub(super) fn backup(args: &BackupArgs) -> Result<(), BackupError> {
    let targets = resolve_targets(args.profile.as_deref(), &args.target)?;
    // Include the managed cluster's bundled tools (daemon/cron path, where
    // AUTUMN_MANAGED_PG_DATA_DIR isn't inherited) so a managed backup needs zero
    // external tools on PATH (issue #1595).
    let tools = PgTools::locate_with_extra(managed_pg_data_dir());
    // Resolve `pg_dump` only when the run holds a Postgres target (issue #1909).
    // An all-SQLite app has no client tools and needs none. A mixed run still
    // fails before any artifact is written.
    let pg_dump = if targets.iter().any(|t| t.backend == TargetBackend::Postgres) {
        Some(tools.require("pg_dump")?)
    } else {
        None
    };

    let profile = migrate::effective_profile(args.profile.as_deref());

    // Offsite pre-flight (issue #1619). Decide whether to upload with a LIGHT
    // probe that reads ONLY `[backup.offsite].auto_upload` (real env → `.env.<p>`
    // → merged TOML), WITHOUT a full `AutumnConfig` load — so a local-only backup
    // never triggers global config validation or encrypted-credentials
    // decryption (a cron job may not export AUTUMN_MASTER_KEY). Only when an
    // upload is actually requested do we do the full, strict resolve (bucket
    // required, creds env resolved, distinct-destination guard) and build the
    // client — BEFORE dumping — so a misconfigured offsite fails fast.
    let should_upload = args.upload || auto_upload_probe(&profile);
    let upload_ctx = if should_upload {
        let offsite = load_offsite(&profile)?
            .ok_or(BackupError::OffsiteNotConfigured)?
            .resolve()?;
        let client = offsite.build_client()?;
        Some((offsite, client))
    } else {
        None
    };

    let root = backup_root(args.dir.as_deref(), &profile);
    let run_dir = create_unique_run_dir(&root, &run_dir_name(&now_utc()))?;

    // Everything below writes into `run_dir`. On ANY failure we remove the
    // whole run directory so a partial/empty artifact is never left behind and
    // never counted toward retention (AC #1).
    let result = backup_into(
        &run_dir,
        &targets,
        args.format,
        pg_dump.as_deref(),
        &tools,
        &profile,
    );
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&run_dir);
        return Err(e);
    }

    eprintln!(
        "\n\u{2713} Backup complete: {} ({} target(s), {}).",
        run_dir.display(),
        targets.len(),
        run_format_note(&targets, args.format),
    );

    // Retention runs only AFTER a verified-successful backup, so a failed run
    // can never prune good history (AC #6).
    if let Some(keep) = args.keep {
        prune(&root, keep)?;
    }

    // Offsite upload runs LAST, only after local verification + prune. A failure
    // here is the split outcome (AC #2/#6): the local artifact stays intact and
    // the command exits non-zero with an unambiguous message.
    if let Some((offsite, client)) = upload_ctx {
        let local_run_id = run_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The REMOTE run id gains a globally-unique token (P2 #12) so concurrent
        // same-second backups of this profile from different hosts never collide
        // in the bucket; the local run directory keeps its #1595 `<timestamp>`.
        let remote_id = remote_run_id(&local_run_id, &remote_run_token());
        // Canonicalize the profile for the REMOTE key prefix (P2 #22) so upload,
        // `db offsite list`, and restore all address the same `<canonical>/…`
        // location regardless of alias/case spelling — while the LOCAL run dir
        // keeps its raw #1595 `<profile>/<timestamp>` layout (used above).
        let remote_profile = migrate::canonical_profile(&profile);
        if let Err(detail) = upload_run(&offsite, &client, &run_dir, &remote_profile, &remote_id) {
            let local_path = run_dir.display().to_string();
            // #1743: raise an operator alert so an unattended/cron backup with
            // [alerts] configured never fails its upload silently. Best-effort —
            // never changes the exit code; the `?`-equivalent return below still
            // exits non-zero with the same split-outcome message as before.
            emit_offsite_upload_failure_alert(&local_path, &detail);
            return Err(BackupError::OffsiteUploadFailed { local_path, detail });
        }
    }
    Ok(())
}

/// The format phrase in the completion line.
///
/// `--format` grades Postgres artifacts only. An all-SQLite run therefore reports
/// `SQLite snapshot`, not a format it ignored. A mixed run names both.
fn run_format_note(targets: &[ResolvedTarget], format: BackupFormat) -> String {
    let sqlite = targets.iter().any(|t| t.backend == TargetBackend::Sqlite);
    let postgres = targets.iter().any(|t| t.backend == TargetBackend::Postgres);
    match (postgres, sqlite) {
        (true, true) => format!("{} format + SQLite snapshot", format.pg_dump_format_flag()),
        (false, true) => "SQLite snapshot".to_owned(),
        _ => format!("{} format", format.pg_dump_format_flag()),
    }
}

/// Best-effort operator alert for a failed offsite upload (#1743).
///
/// Discriminator (chosen design): fire on ANY upload failure when `[alerts]`
/// channels are configured. Interactive users with no `[alerts]` destination
/// build zero channels, so this is a no-op for them — the interactive case
/// (message + non-zero exit) is unchanged. Loading the config here is
/// acceptable: we are already on the failure path and about to exit non-zero.
fn emit_offsite_upload_failure_alert(local_path: &str, detail: &str) {
    let config = match autumn_web::config::AutumnConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  \u{26A0} offsite-upload-failed alert skipped: could not load config: {e}");
            return;
        }
    };
    let client = autumn_web::http::Client::from_config(&config.http.client);
    let channels = crate::alert::configured_http_channels(&config.alerts, &client);
    deliver_offsite_upload_alert(&channels, local_path, detail);
}

/// Build the `ScheduledTaskFailure` alert raised when an offsite upload fails.
/// Pure (no I/O) so it can be asserted directly in tests.
fn build_offsite_upload_alert(local_path: &str, detail: &str) -> Alert {
    Alert::trigger(
        AlertCondition::ScheduledTaskFailure,
        "scheduled_task_failure:db-backup-offsite-upload",
    )
    .title("Offsite backup upload failed")
    .summary(detail)
    .detail("local_path", local_path)
    .detail("error", detail)
    .build()
}

/// Deliver the offsite-upload-failure alert through every configured channel on
/// a short-lived current-thread runtime (mirrors `autumn alert test`).
/// Best-effort: a per-channel delivery error is logged but never propagated, so
/// alerting can NEVER change the command's exit code. A no-op when `channels`
/// is empty. Takes `&[Arc<dyn AlertChannel>]` so a test can inject a capturing
/// mock channel and assert the alert that a failed upload raises.
fn deliver_offsite_upload_alert(
    channels: &[Arc<dyn AlertChannel>],
    local_path: &str,
    detail: &str,
) {
    if channels.is_empty() {
        return;
    }
    let alert = build_offsite_upload_alert(local_path, detail);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!(
                "  \u{26A0} offsite-upload-failed alert not sent: could not start runtime: {e}"
            );
            return;
        }
    };
    runtime.block_on(async {
        for channel in channels {
            if let Err(error) = channel.deliver(&alert).await {
                eprintln!(
                    "  \u{26A0} offsite-upload-failed alert not delivered via {}: {error}",
                    channel.name()
                );
            }
        }
    });
}

/// Dump every target into `run_dir`, verify each artifact, and write the
/// manifest. Returns `Err` (leaving cleanup to the caller) on the first failure.
fn backup_into(
    run_dir: &Path,
    targets: &[ResolvedTarget],
    format: BackupFormat,
    pg_dump: Option<&Path>,
    tools: &PgTools,
    profile: &str,
) -> Result<(), BackupError> {
    let mut manifest_targets = Vec::with_capacity(targets.len());
    for target in targets {
        let file_name = artifact_file_name(&target.label, target.backend, format);
        let out_path = run_dir.join(&file_name);
        eprintln!(
            "\u{2500}\u{2500} backing up {} \u{2500}\u{2500}",
            target.label
        );

        let db = parsed_db_name(&target.url);
        match target.backend {
            TargetBackend::Postgres => {
                // `pg_dump` is `Some` whenever any Postgres target is in the run
                // (resolved in `backup` above), so this arm always has it.
                let pg_dump = pg_dump.ok_or_else(|| BackupError::ToolMissing {
                    tool: "pg_dump".to_owned(),
                })?;
                run_pg_dump(pg_dump, &target.url, &out_path, format, &db)?;
            }
            TargetBackend::Sqlite => {
                sqlite_snapshot::snapshot(&target.url, &out_path).map_err(|e| {
                    BackupError::SqliteFailed {
                        op: "backup",
                        detail: e.to_string(),
                    }
                })?;
            }
        }
        verify_artifact(&out_path, target.backend, format, &db, tools)?;
        eprintln!("  \u{2713} {file_name} verified.");

        manifest_targets.push(ManifestTarget {
            label: target.label.clone(),
            file: file_name,
            database: db,
            backend: target.backend.as_str().to_owned(),
        });
    }

    let manifest = Manifest {
        autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: now_utc().to_rfc3339(),
        profile: profile.to_owned(),
        format: format.pg_dump_format_flag().to_owned(),
        targets: manifest_targets,
    };
    write_manifest(run_dir, &manifest)
}

/// Build a `Command` for a pg client tool with the connection password moved
/// out of argv and into `PGPASSWORD`. The full URL (with password) is never
/// passed as a command-line argument, so it can't leak via `ps` /
/// `/proc/<pid>/cmdline` to other local users — matching this module's
/// credential-safety stance. Returns the command plus the password-free
/// `--dbname` value the caller should pass. libpq reads `PGPASSWORD` when the
/// connection string omits the password.
fn pg_command(program: &Path, url: &str) -> (Command, String) {
    let (safe_url, password) = split_password(url);
    let mut cmd = Command::new(program);
    if let Some(pw) = password {
        cmd.env("PGPASSWORD", pw);
    }
    (cmd, safe_url)
}

/// Split a connection string into a `(password-free connstring, password)`
/// pair. Two libpq connection-string forms are handled — the URL form
/// (`postgres://user:pw@host/db`) and the keyword/value form
/// (`host=db user=app password=secret dbname=app`), both of which Autumn's
/// config validation accepts. In either form the password is moved out of the
/// string so the caller can hand it to `PGPASSWORD` instead of leaving it on the
/// argv (visible via `ps` / `/proc/<pid>/cmdline`). A string that is neither a
/// parseable URL nor recognizable keyword/value form — or that simply carries no
/// password — is returned unchanged with `None`, so behavior degrades to the
/// previous "connstring on the command line" path rather than failing.
///
/// For the URL form the returned password is **percent-decoded**:
/// `Url::password()` yields the raw (still percent-encoded) userinfo, but
/// `PGPASSWORD` is consumed by libpq as a literal string — it is *not*
/// percent-decoded. A URL like `postgres://u:p%40ss@h/db` must set
/// `PGPASSWORD=p@ss`, not `p%40ss`, or authentication fails. The username is
/// intentionally left untouched: it stays in the password-free `--dbname` URL,
/// where libpq decodes it as part of URI parsing, so double-decoding it (or
/// moving it to an env var) would be wrong.
fn split_password(url: &str) -> (String, Option<String>) {
    if let Ok(mut parsed) = url::Url::parse(url) {
        let password = parsed.password().map(|pw| {
            percent_encoding::percent_decode_str(pw)
                .decode_utf8_lossy()
                .into_owned()
        });
        if password.is_some() {
            // Clearing the password keeps the username, host, port, db, and
            // any query parameters intact.
            let _ = parsed.set_password(None);
        }
        return (parsed.to_string(), password);
    }
    // Not a URL: it may be libpq keyword/value form, where the password would
    // otherwise ride along on argv.
    split_password_kv(url)
}

/// Move any `password=...` out of a libpq keyword/value connection string,
/// returning `(stripped connstring, password)`. When the input isn't
/// recognizable keyword/value form, or carries no password, the ORIGINAL string
/// is returned untouched with `None` so a password-less connstring is passed
/// through unchanged.
fn split_password_kv(conn: &str) -> (String, Option<String>) {
    let Some(pairs) = parse_libpq_kv(conn) else {
        return (conn.to_owned(), None);
    };
    let mut password = None;
    let mut kept = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        if key == "password" {
            // Last one wins, matching libpq (later keywords override earlier).
            password = Some(value);
        } else {
            kept.push((key, value));
        }
    }
    if password.is_none() {
        return (conn.to_owned(), None);
    }
    let rebuilt = kept
        .into_iter()
        .map(|(k, v)| format!("{k}={}", quote_libpq_value(&v)))
        .collect::<Vec<_>>()
        .join(" ");
    (rebuilt, password)
}

/// Parse a libpq keyword/value connection string into `(keyword, value)` pairs.
/// Applies libpq's unquoting rules: whitespace separates pairs and may surround
/// `=`; a value may be single-quoted to contain spaces; and a backslash escapes
/// the next character (so `\'` and `\\` yield a literal quote and backslash).
/// Returns `None` when the string doesn't parse as keyword/value form (no
/// `keyword=` token, a missing `=`, or an unterminated quote), so callers fall
/// back to treating it opaquely.
fn parse_libpq_kv(conn: &str) -> Option<Vec<(String, String)>> {
    let chars: Vec<char> = conn.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut pairs = Vec::new();
    loop {
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i == len {
            break;
        }
        // Keyword: runs up to the next whitespace or `=` (keywords aren't quoted).
        let start = i;
        while i < len && !chars[i].is_whitespace() && chars[i] != '=' {
            i += 1;
        }
        if i == start {
            return None;
        }
        let keyword: String = chars[start..i].iter().collect();
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i == len || chars[i] != '=' {
            return None;
        }
        i += 1; // consume '='
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        // Value: single-quoted (may contain spaces) or bare (ends at whitespace).
        let mut value = String::new();
        if i < len && chars[i] == '\'' {
            i += 1;
            loop {
                if i == len {
                    return None; // unterminated quote
                }
                match chars[i] {
                    '\\' => {
                        i += 1;
                        if i < len {
                            value.push(chars[i]);
                            i += 1;
                        }
                    }
                    '\'' => {
                        i += 1;
                        break;
                    }
                    c => {
                        value.push(c);
                        i += 1;
                    }
                }
            }
        } else {
            while i < len && !chars[i].is_whitespace() {
                if chars[i] == '\\' {
                    i += 1;
                    if i < len {
                        value.push(chars[i]);
                        i += 1;
                    }
                } else {
                    value.push(chars[i]);
                    i += 1;
                }
            }
        }
        pairs.push((keyword, value));
    }
    (!pairs.is_empty()).then_some(pairs)
}

/// Re-serialize a libpq value, single-quoting and backslash-escaping it when it
/// is empty or contains whitespace, a quote, or a backslash, so the rebuilt
/// connection string round-trips back through [`parse_libpq_kv`] (and libpq).
fn quote_libpq_value(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || c == '\'' || c == '\\');
    if !needs_quoting {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Build the `pg_dump` argument vector for one target. Pure over its inputs so
/// the flag composition (notably the format-conditional `--clean --if-exists`)
/// is unit-testable without spawning `pg_dump`.
///
/// `--no-owner`/`--no-privileges` keep the artifact portable across roles so a
/// restore into a freshly-created database works without recreating the
/// original owner/grants.
///
/// For the **plain** format we additionally emit `--clean --if-exists`, so the
/// SQL text carries `DROP ... IF EXISTS` before each `CREATE`. A plain restore
/// just pipes the dump through `psql` under `ON_ERROR_STOP=1`; without the drops
/// it aborts with `... already exists` when restoring over a database that still
/// holds the schema (a post-data-loss or prod-overwrite drill). Baking the drops
/// into the dump makes the restore idempotent and safe on both empty and
/// populated databases. The **custom** format deliberately omits `--clean`:
/// `pg_restore --clean --if-exists` performs the drops at restore time instead.
fn pg_dump_args(format: BackupFormat, out_path: &Path, dbname: &str) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = vec![
        "--format".into(),
        format.pg_dump_format_flag().into(),
        "--no-owner".into(),
        "--no-privileges".into(),
    ];
    if matches!(format, BackupFormat::Plain) {
        args.push("--clean".into());
        args.push("--if-exists".into());
    }
    args.push("--file".into());
    args.push(out_path.as_os_str().to_owned());
    args.push("--dbname".into());
    args.push(dbname.into());
    args
}

/// Shell out to `pg_dump` for one target.
fn run_pg_dump(
    pg_dump: &Path,
    url: &str,
    out_path: &Path,
    format: BackupFormat,
    db: &str,
) -> Result<(), BackupError> {
    let (mut cmd, safe_url) = pg_command(pg_dump, url);
    let status = cmd
        .args(pg_dump_args(format, out_path, &safe_url))
        .status()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BackupError::ToolMissing {
                tool: "pg_dump".to_owned(),
            },
            _ => BackupError::ToolFailed {
                tool: "pg_dump".to_owned(),
                context: format!("could not spawn for database {db:?}: {e}"),
            },
        })?;
    if !status.success() {
        return Err(BackupError::ToolFailed {
            tool: "pg_dump".to_owned(),
            context: format!(
                "dump of database {db:?} exited {}",
                exit_desc(status.code())
            ),
        });
    }
    Ok(())
}

/// Verify a freshly-written artifact before it is counted as a success (AC #1).
///
/// * Custom: the file must be non-empty AND `pg_restore --list` must succeed
///   with a non-empty archive table of contents.
/// * Plain: the file must be non-empty AND end with `pg_dump`'s completion marker.
fn verify_artifact(
    path: &Path,
    backend: TargetBackend,
    format: BackupFormat,
    db: &str,
    tools: &PgTools,
) -> Result<(), BackupError> {
    // A SQLite artifact IS a database: grade it with `PRAGMA integrity_check`
    // (issue #1909) rather than a Postgres archive reader.
    if backend == TargetBackend::Sqlite {
        return sqlite_snapshot::verify(path).map_err(|e| BackupError::IntegrityFailed {
            detail: format!("{db}: {e}"),
        });
    }
    let len = std::fs::metadata(path)
        .map_err(BackupError::io(format!("stat {}", path.display())))?
        .len();
    if len == 0 {
        return Err(BackupError::IntegrityFailed {
            detail: format!("dump of {db:?} is empty (0 bytes)"),
        });
    }
    match format {
        BackupFormat::Custom => {
            // Resolve `pg_restore` through the SAME locator the dump/restore step
            // used (built via `locate_with_extra` with the runtime-discovered
            // managed data dir). Re-locating with a bare `PgTools::locate()` here
            // would miss the daemon/cron path's bundled binary and wrongly report
            // it missing (issue #1595).
            let pg_restore = tools.require("pg_restore")?;
            verify_custom_archive(&pg_restore, path, db)
        }
        BackupFormat::Plain => verify_plain_dump(path, db),
    }
}

/// Structural integrity check for a custom-format archive: `pg_restore --list`
/// parses the archive's TOC and fails on a truncated/corrupt file.
fn verify_custom_archive(pg_restore: &Path, path: &Path, db: &str) -> Result<(), BackupError> {
    let output = Command::new(pg_restore)
        .arg("--list")
        .arg(path)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BackupError::ToolMissing {
                tool: "pg_restore".to_owned(),
            },
            _ => BackupError::ToolFailed {
                tool: "pg_restore".to_owned(),
                context: format!("could not spawn to verify {db:?}: {e}"),
            },
        })?;
    if !output.status.success() {
        return Err(BackupError::IntegrityFailed {
            detail: format!(
                "`pg_restore --list` rejected the {db:?} archive ({})",
                exit_desc(output.status.code())
            ),
        });
    }
    // A valid-but-empty TOC (no dumpable objects) still lists header comment
    // lines; require at least one non-comment entry so a truly empty archive is
    // caught.
    let listing = String::from_utf8_lossy(&output.stdout);
    if toc_entry_count(&listing) == 0 {
        return Err(BackupError::IntegrityFailed {
            detail: format!("the {db:?} archive contains no objects"),
        });
    }
    Ok(())
}

/// Integrity check for a plain SQL dump: it must end with `pg_dump`'s completion
/// marker, which is only written once the dump finished cleanly.
fn verify_plain_dump(path: &Path, db: &str) -> Result<(), BackupError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file =
        std::fs::File::open(path).map_err(BackupError::io(format!("open {}", path.display())))?;
    let len = file
        .metadata()
        .map_err(BackupError::io(format!("stat {}", path.display())))?
        .len();

    // A 0-byte dump can't contain the completion marker: it's truncated/incomplete.
    if len == 0 {
        return Err(BackupError::IntegrityFailed {
            detail: format!("the {db:?} SQL dump is empty (likely truncated)"),
        });
    }

    // The completion marker is on the final line, so only the dump's tail needs to
    // be read — avoid loading a potentially multi-GB dump into memory. Read the
    // last ~1 KiB (clamped to the file size for tiny files).
    let read_len = len.min(1024);
    let offset = -i64::try_from(read_len).expect("read_len is at most 1024");
    file.seek(SeekFrom::End(offset))
        .map_err(BackupError::io(format!("seek {}", path.display())))?;
    let mut tail = Vec::with_capacity(read_len as usize);
    file.take(read_len)
        .read_to_end(&mut tail)
        .map_err(BackupError::io(format!("read {}", path.display())))?;
    let tail = String::from_utf8_lossy(&tail);

    if plain_dump_is_complete(&tail) {
        Ok(())
    } else {
        Err(BackupError::IntegrityFailed {
            detail: format!(
                "the {db:?} SQL dump is missing pg_dump's completion marker (likely truncated)"
            ),
        })
    }
}

// ─── Restore ────────────────────────────────────────────────────────────────

pub(super) fn restore(args: &RestoreArgs) -> Result<(), BackupError> {
    // Production guard — identical to `autumn db drop` (AC #4). This guards the
    // RESTORE-TARGET profile (`--profile`), independent of any offsite source.
    let profile = migrate::effective_profile(args.profile.as_deref());
    super::guard_destructive(&profile, args.force).map_err(|_| BackupError::ProductionRefused {
        profile: profile.clone(),
    })?;

    // Offsite restore (issue #1619): when `--offsite` is set or the artifact is
    // an `offsite:<profile>/<ts|latest>` reference, download the run to a fresh
    // temp dir and hand it to the SAME RestorePlan::discover path so the
    // identical integrity-refusal + guard/`--force` protocol applies unchanged.
    let artifact_str = args.artifact.to_string_lossy();
    // When offsite is indicated (flag or `offsite:` prefix) we commit to the
    // offsite path: a malformed reference is an error, never a silent fall
    // through to a same-named local artifact. `None` means offsite was not
    // indicated at all, so a normal local restore proceeds.
    if let Some(oref) = resolve_offsite_ref(args.offsite, &artifact_str) {
        return restore_from_offsite(args, &oref?);
    }

    let plan = RestorePlan::discover(&args.artifact, args.shard.as_deref())?;
    apply_restore_plan(&plan, args)
}

/// Verify every selected artifact, then restore each into its resolved database.
/// Shared by local and offsite restore so both go through the identical
/// verify-all-before-mutate-any protocol (AC #4).
fn apply_restore_plan(plan: &RestorePlan, args: &RestoreArgs) -> Result<(), BackupError> {
    let format = plan.format;
    let entries = plan.select(args.shard.as_deref())?;

    let targets = resolve_targets_for_restore(args.profile.as_deref(), &entries)?;
    // Same bundled-tool discovery as backup: a managed restore must also work
    // with zero external tools on PATH (issue #1595).
    let tools = PgTools::locate_with_extra(managed_pg_data_dir());

    // Refuse a backend mismatch before anything else (issue #1909). Each entry's
    // mechanism comes from the MANIFEST, the database it writes to from the
    // CONFIG, so restoring a SQLite run into a Postgres environment (or the
    // reverse) would otherwise fail deep inside a driver with an unrelated
    // message. Names labels and backends only — never a URL.
    for (entry, url) in &targets {
        let recorded = TargetBackend::from_manifest(&entry.backend);
        let configured = TargetBackend::detect(url);
        if recorded != configured {
            return Err(BackupError::BadArtifact {
                detail: format!(
                    "the {:?} artifact is a {} backup, but the configured {:?} database is \
                     {}.\n  Restore it against the backend it was taken from, or fix \
                     database.primary_url for this profile.",
                    entry.label,
                    recorded.as_str(),
                    entry.label,
                    configured.as_str()
                ),
            });
        }
    }

    // Verify EVERY artifact before mutating ANY database (AC #4): refuse to
    // start a destructive restore we can't finish.
    for (entry, _url) in &targets {
        let db = format!("(artifact) {}", entry.label);
        let backend = TargetBackend::from_manifest(&entry.backend);
        verify_artifact(&plan.dir.join(&entry.file), backend, format, &db, &tools)?;
        eprintln!("  \u{2713} {} integrity verified.", entry.file);
    }

    for (entry, url) in &targets {
        eprintln!(
            "\u{2500}\u{2500} restoring {} \u{2500}\u{2500}",
            entry.label
        );
        let artifact = plan.dir.join(&entry.file);
        let db = parsed_db_name(url);
        run_restore_one(
            &tools,
            url,
            &artifact,
            TargetBackend::from_manifest(&entry.backend),
            format,
            &db,
        )?;
        eprintln!("  \u{2713} restored {}.", entry.label);
    }

    eprintln!("\n\u{2713} Restore complete ({} target(s)).", targets.len());
    Ok(())
}

/// Restore one artifact into one database.
fn run_restore_one(
    tools: &PgTools,
    url: &str,
    artifact: &Path,
    backend: TargetBackend,
    format: BackupFormat,
    db: &str,
) -> Result<(), BackupError> {
    // A SQLite artifact replaces the data file it was taken from; no external
    // tool, and no `--clean` equivalent needed (issue #1909).
    if backend == TargetBackend::Sqlite {
        return sqlite_snapshot::restore(artifact, url).map_err(|e| BackupError::SqliteFailed {
            op: "restore",
            detail: e.to_string(),
        });
    }
    match format {
        BackupFormat::Custom => {
            let pg_restore = tools.require("pg_restore")?;
            // `--clean --if-exists` drops existing objects first so the restore
            // is an overwrite, not a merge; `--no-owner` matches the dump flags.
            let (mut cmd, safe_url) = pg_command(&pg_restore, url);
            let status = cmd
                .arg("--clean")
                .arg("--if-exists")
                .arg("--no-owner")
                .arg("--dbname")
                .arg(&safe_url)
                .arg(artifact)
                .status()
                .map_err(|e| spawn_err("pg_restore", db, &e))?;
            if !status.success() {
                return Err(BackupError::ToolFailed {
                    tool: "pg_restore".to_owned(),
                    context: format!(
                        "restore into database {db:?} exited {}",
                        exit_desc(status.code())
                    ),
                });
            }
            Ok(())
        }
        BackupFormat::Plain => {
            let psql = tools.require("psql")?;
            let (mut cmd, safe_url) = pg_command(&psql, url);
            let status = cmd
                .arg("--set")
                .arg("ON_ERROR_STOP=1")
                .arg("--dbname")
                .arg(&safe_url)
                .arg("--file")
                .arg(artifact)
                .status()
                .map_err(|e| spawn_err("psql", db, &e))?;
            if !status.success() {
                return Err(BackupError::ToolFailed {
                    tool: "psql".to_owned(),
                    context: format!(
                        "restore into database {db:?} exited {}",
                        exit_desc(status.code())
                    ),
                });
            }
            Ok(())
        }
    }
}

fn spawn_err(tool: &str, db: &str, e: &std::io::Error) -> BackupError {
    match e.kind() {
        std::io::ErrorKind::NotFound => BackupError::ToolMissing {
            tool: tool.to_owned(),
        },
        _ => BackupError::ToolFailed {
            tool: tool.to_owned(),
            context: format!("could not spawn for database {db:?}: {e}"),
        },
    }
}

/// A restore artifact resolved to a directory + its manifest entries.
struct RestorePlan {
    /// Directory holding the artifact file(s).
    dir: PathBuf,
    /// Format of the artifacts.
    format: BackupFormat,
    /// The manifest targets to restore.
    entries: Vec<ManifestTarget>,
}

impl RestorePlan {
    /// Discover a restore plan from a user-supplied path. Accepts either a run
    /// directory (containing `manifest.json`) or a single artifact file.
    ///
    /// For a bare single-file artifact there is no manifest to say which target
    /// it belongs to, so the target is resolved from an explicit `--shard`
    /// (`shard`) or, failing that, inferred from the backup writer's filename
    /// convention (see [`single_file_target_label`]). This prevents a
    /// `shard-<name>.dump` from being silently restored into the control
    /// database.
    fn discover(path: &Path, shard: Option<&str>) -> Result<Self, BackupError> {
        if path.is_dir() {
            let manifest = read_manifest(path)?;
            let format = BackupFormat::parse(&manifest.format).map_err(|detail| {
                BackupError::BadArtifact {
                    detail: format!("manifest has an invalid format: {detail}"),
                }
            })?;
            return Ok(Self {
                dir: path.to_path_buf(),
                format,
                entries: manifest.targets,
            });
        }
        if path.is_file() {
            // A bare artifact file: infer format from extension, and resolve the
            // restore target from `--shard` or the filename convention (never
            // blindly `control`, which would corrupt the control DB with shard
            // data).
            let (backend, format) =
                infer_artifact_kind(path).ok_or_else(|| BackupError::BadArtifact {
                    detail: format!(
                        "{} is neither a run directory nor a .dump/.sql/.sqlite artifact",
                        path.display()
                    ),
                })?;
            let dir = path
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = single_file_target_label(&file, shard)?;
            return Ok(Self {
                dir,
                format,
                entries: vec![ManifestTarget {
                    label,
                    file,
                    database: String::new(),
                    backend: backend.as_str().to_owned(),
                }],
            });
        }
        Err(BackupError::BadArtifact {
            detail: format!("{} does not exist", path.display()),
        })
    }

    /// Filter the plan's entries to a single shard when `--shard` is given.
    fn select(&self, shard: Option<&str>) -> Result<Vec<ManifestTarget>, BackupError> {
        let Some(shard) = shard else {
            return Ok(self.entries.clone());
        };
        let wanted = format!("shard:{shard}");
        let selected: Vec<ManifestTarget> = self
            .entries
            .iter()
            .filter(|t| t.label == wanted)
            .cloned()
            .collect();
        if selected.is_empty() {
            let known: Vec<String> = self.entries.iter().map(|t| t.label.clone()).collect();
            return Err(BackupError::UnknownShard {
                name: shard.to_owned(),
                known,
            });
        }
        Ok(selected)
    }
}

// ─── Target resolution (reuses the migrate URL-resolution path) ─────────────

/// The project-root `.env`-overlaid process environment, mirroring how
/// `autumn migrate` resolves its target URLs (issue #1684). A real env var
/// still wins over `.env`; a malformed `.env` fails loudly (with a `path:line`
/// location) rather than being silently ignored.
fn dotenv_env() -> autumn_web::dotenv::DotenvOsEnv {
    match autumn_web::dotenv::os_env_with_dotenv() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("  \u{274C} .env: {e}");
            std::process::exit(1);
        }
    }
}

/// Like [`dotenv_env`] but selects `.env.<profile>` for an explicit target
/// profile (issue #1619). The offsite path resolves config + credentials under
/// the run's profile (`--profile <p>` / `offsite:<p>/…`), so a profile-specific
/// `.env.<p>` is honored even when the ambient shell profile differs — otherwise
/// `autumn db backup --profile test --upload` from a dev/unset shell would miss
/// `.env.test`-provided offsite settings and credentials.
pub(super) fn dotenv_env_for_profile(profile: &str) -> autumn_web::dotenv::DotenvOsEnv {
    // Normalize aliases/case to the app's resolved profile FIRST (P2 #20), so
    // `--profile development` / `AUTUMN_ENV=PROD` selects `.env.dev` / `.env.prod`
    // — the same file the running app reads — instead of a literal `.env.development`.
    let profile = migrate::canonical_profile(profile);
    match autumn_web::dotenv::os_env_with_dotenv_for_profile(&profile) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("  \u{274C} .env: {e}");
            std::process::exit(1);
        }
    }
}

/// The managed-Postgres published cluster URL, used as a *fallback* control URL
/// for bundled/daemon/single-binary apps (issue #1595). Such deployments often
/// have no `DATABASE_URL` in env or `autumn.toml`; the running cluster instead
/// publishes its URL for one-off CLI commands. This reuses the SAME blessed
/// helper `autumn task`/`autumn build` use to attach to the serve daemon's
/// cluster ([`crate::serve::managed_pg_env`]), which only yields a URL when a
/// live, reachable cluster is published — so a backup can't attach to a dead or
/// foreign endpoint. `None` when no managed cluster is available.
fn managed_pg_fallback_url() -> Option<String> {
    crate::serve::managed_pg_env(None).and_then(|env| env.attach_url)
}

/// The managed-Postgres cluster's data dir, discovered via the SAME blessed
/// helper as [`managed_pg_fallback_url`]. Used to locate the daemon's bundled
/// `pg_dump`/`pg_restore`/`psql`: in the managed-daemon/cron path the backup
/// process is NOT launched with `AUTUMN_MANAGED_PG_DATA_DIR` (only `autumn serve`
/// sets it for its own children), so the env-var-driven tool locator can't find
/// the extracted bundle. Feeding this data dir to the locator as a
/// runtime-discovered candidate lets a managed backup run with zero external
/// tools on PATH (issue #1595, AC #2). `None` when no managed cluster resolves.
fn managed_pg_data_dir() -> Option<PathBuf> {
    crate::serve::managed_pg_env(None).map(|env| env.data_dir)
}

/// Apply the managed-pg published URL as a fallback for the control URL. Pure
/// over its inputs (the fallback is a closure, evaluated lazily) so the
/// precedence — explicit config/env first, managed-pg published URL second — is
/// unit-testable without a live cluster.
fn resolve_control_url<Ff>(resolved: Option<String>, fallback: Ff) -> Option<String>
where
    Ff: FnOnce() -> Option<String>,
{
    resolved.or_else(fallback)
}

/// Resolve the databases a backup run should capture, reusing the SAME
/// resolution `autumn migrate` uses so the set matches the running app exactly.
/// Every database a scrub must visit under `profile` — control plus each
/// configured shard — as `(label, url)` pairs, resolved through the **same**
/// path `backup`/`restore` use so a scrub can never miss a database a backup
/// captured (issue #1602).
///
/// # Errors
///
/// Returns [`BackupError::NoUrl`] when no control URL can be resolved.
pub(super) fn resolve_all_target_urls(
    profile: Option<&str>,
) -> Result<Vec<(String, String)>, BackupError> {
    Ok(resolve_targets(profile, &TargetSelector::All)?
        .into_iter()
        .map(|t| (t.label, t.url))
        .collect())
}

/// The profile a backup run directory was taken under, read from its
/// `manifest.json`. `None` for a bare single-file artifact (which carries no
/// manifest) or an unreadable/invalid manifest — callers treat absence as
/// "unknown provenance", never as "safe".
pub(super) fn artifact_source_profile(artifact: &Path) -> Option<String> {
    artifact
        .is_dir()
        .then(|| read_manifest(artifact).ok())
        .flatten()
        .map(|m| m.profile)
}

fn resolve_targets(
    profile: Option<&str>,
    selector: &TargetSelector,
) -> Result<Vec<ResolvedTarget>, BackupError> {
    use autumn_web::config::Env as _;

    // Resolve control AND shards through the SAME `.env`-overlaid environment
    // that `autumn migrate` uses (issue #1684), so a `.env`-provided shard
    // override is honored identically. A real env var still wins over `.env`.
    let table =
        migrate::read_autumn_toml_table_with_profile(Some(&migrate::effective_profile(profile)));
    let env = dotenv_env();
    // Explicit config/env wins; fall back to the managed-pg published URL for
    // bundled/daemon apps that don't export a DATABASE_URL (issue #1595).
    let control = resolve_control_url(
        migrate::resolve_primary_database_url_from_sources(|k| env.var(k), table.as_ref()),
        managed_pg_fallback_url,
    );
    let shards = migrate::resolve_shard_database_urls_from_sources(|k| env.var(k), table.as_ref());
    build_targets(control, shards, selector)
}

/// Pure target-selection logic, separated for unit testing. Mirrors
/// `migrate::build_targets`'s control-first / shards-in-order shape.
fn build_targets(
    control: Option<String>,
    shards: Vec<(String, String)>,
    selector: &TargetSelector,
) -> Result<Vec<ResolvedTarget>, BackupError> {
    match selector {
        TargetSelector::ControlOnly => control
            .map(|url| {
                vec![ResolvedTarget {
                    backend: TargetBackend::detect(&url),
                    label: "control".to_owned(),
                    url,
                }]
            })
            .ok_or(BackupError::NoUrl),
        TargetSelector::Shard(name) => {
            let Some((_, url)) = shards.iter().find(|(shard, _)| shard == name) else {
                return Err(BackupError::UnknownShard {
                    name: name.clone(),
                    known: shards.into_iter().map(|(n, _)| n).collect(),
                });
            };
            Ok(vec![ResolvedTarget {
                label: format!("shard:{name}"),
                url: url.clone(),
                backend: TargetBackend::detect(url),
            }])
        }
        TargetSelector::All => {
            let mut targets = Vec::new();
            if let Some(control_url) = control {
                targets.push(ResolvedTarget {
                    backend: TargetBackend::detect(&control_url),
                    label: "control".to_owned(),
                    url: control_url,
                });
            } else if shards.is_empty() {
                return Err(BackupError::NoUrl);
            }
            for (name, url) in shards {
                targets.push(ResolvedTarget {
                    backend: TargetBackend::detect(&url),
                    label: format!("shard:{name}"),
                    url,
                });
            }
            Ok(targets)
        }
    }
}

/// Resolve a live URL for each manifest entry a restore will write to, again
/// via the migrate resolution path so restore hits the same databases.
fn resolve_targets_for_restore(
    profile: Option<&str>,
    entries: &[ManifestTarget],
) -> Result<Vec<(ManifestTarget, String)>, BackupError> {
    use autumn_web::config::Env as _;

    // Same `.env`-overlaid resolution as `resolve_targets` / `autumn migrate`
    // so restore writes to the exact databases a backup would have captured.
    let table =
        migrate::read_autumn_toml_table_with_profile(Some(&migrate::effective_profile(profile)));
    let env = dotenv_env();
    // Same precedence as `resolve_targets`: explicit config/env first, then the
    // managed-pg published URL so a restore into a bundled/daemon app resolves a
    // live control URL without a manually exported DATABASE_URL (issue #1595).
    let control = resolve_control_url(
        migrate::resolve_primary_database_url_from_sources(|k| env.var(k), table.as_ref()),
        managed_pg_fallback_url,
    );
    let shards = migrate::resolve_shard_database_urls_from_sources(|k| env.var(k), table.as_ref());

    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let url = if entry.label == "control" {
            control.clone().ok_or(BackupError::NoUrl)?
        } else if let Some(shard_name) = entry.label.strip_prefix("shard:") {
            shards
                .iter()
                .find(|(n, _)| n == shard_name)
                .map(|(_, u)| u.clone())
                .ok_or_else(|| BackupError::UnknownShard {
                    name: shard_name.to_owned(),
                    known: shards.iter().map(|(n, _)| n.clone()).collect(),
                })?
        } else {
            return Err(BackupError::BadArtifact {
                detail: format!(
                    "manifest target has an unrecognized label {:?}",
                    entry.label
                ),
            });
        };
        out.push((entry.clone(), url));
    }
    Ok(out)
}

// ─── Retention ──────────────────────────────────────────────────────────────

/// Prune all but the newest `keep` run directories under `root`.
fn prune(root: &Path, keep: usize) -> Result<(), BackupError> {
    let mut runs = list_run_dirs(root)?;
    runs.sort(); // ascending by timestamped name
    let to_remove = plan_pruning(&runs, keep);
    for name in &to_remove {
        let path = root.join(name);
        std::fs::remove_dir_all(&path)
            .map_err(BackupError::io(format!("pruning {}", path.display())))?;
        eprintln!("  \u{1F5D1} pruned old backup {name}");
    }
    if !to_remove.is_empty() {
        eprintln!(
            "  \u{2139} retention: kept newest {keep}, pruned {}.",
            to_remove.len()
        );
    }
    Ok(())
}

/// List backup run-directory names directly under `root`. Only directories that
/// actually contain a `manifest.json` are counted, so retention pruning can
/// never delete an unrelated directory a user may have placed alongside the
/// backups (or a partially-written run — those are cleaned up before this runs).
/// Missing root => empty.
fn list_run_dirs(root: &Path) -> Result<Vec<String>, BackupError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in
        std::fs::read_dir(root).map_err(BackupError::io(format!("read {}", root.display())))?
    {
        let entry = entry.map_err(BackupError::io(format!("read entry in {}", root.display())))?;
        let path = entry.path();
        if path.is_dir() && path.join(MANIFEST_FILE).is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

/// Pure retention rule: given run names sorted ascending (oldest first), return
/// the names to remove so that only the newest `keep` remain. `keep == 0` is
/// treated as `keep == 1` (never prune everything from a just-run backup).
fn plan_pruning(sorted_runs: &[String], keep: usize) -> Vec<String> {
    let keep = keep.max(1);
    if sorted_runs.len() <= keep {
        return Vec::new();
    }
    let remove_count = sorted_runs.len() - keep;
    sorted_runs[..remove_count].to_vec()
}

// ─── pg tool location ───────────────────────────────────────────────────────

/// Resolves Postgres client executables (`pg_dump`, `pg_restore`, `psql`)
/// from an ordered list of candidate `bin` directories, falling back to the
/// bare command name (PATH lookup).
pub struct PgTools {
    /// Candidate `bin` directories, highest priority first.
    dirs: Vec<PathBuf>,
}

impl PgTools {
    /// Build the locator from the environment: `AUTUMN_PG_BIN_DIR`, then the
    /// managed-Postgres bundle's `bin` dir, then PATH.
    pub fn locate() -> Self {
        Self::locate_with_extra(None)
    }

    /// Like [`Self::locate`] but also considers a runtime-discovered managed
    /// Postgres data dir (e.g. from [`managed_pg_data_dir`]). The daemon/cron
    /// backup path doesn't inherit `AUTUMN_MANAGED_PG_DATA_DIR`, so without this
    /// the bundled client tools sitting next to the live cluster's data dir would
    /// be invisible and `require` would wrongly report them missing (issue #1595).
    /// The extra dir's derived `bin/` directories are appended after the
    /// env-driven candidates, so an explicit `AUTUMN_PG_BIN_DIR`/env bundle still
    /// wins.
    pub fn locate_with_extra(managed_data_dir: Option<PathBuf>) -> Self {
        let mut dirs = candidate_bin_dirs(
            std::env::var_os(PG_BIN_DIR_ENV).map(PathBuf::from),
            std::env::var_os(MANAGED_PG_DATA_DIR_ENV).map(PathBuf::from),
        );
        // Reuse the exact bundle-layout derivation for the runtime-discovered dir.
        dirs.extend(candidate_bin_dirs(None, managed_data_dir));
        Self { dirs }
    }

    /// Build a locator over an explicit list of candidate `bin` directories.
    /// Test-only seam so callers (e.g. the `doctor` check) can exercise the
    /// discovery/resolution path without mutating the process environment.
    #[cfg(test)]
    pub const fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    /// Resolve a tool to a concrete path (a candidate dir that contains it) or
    /// the bare name for PATH resolution.
    fn resolve(&self, tool: &str) -> PathBuf {
        resolve_tool_in(&self.dirs, tool)
    }

    /// Like [`Self::resolve`] but fails fast with [`BackupError::ToolMissing`]
    /// when the tool is neither in a candidate dir nor on PATH.
    pub fn require(&self, tool: &str) -> Result<PathBuf, BackupError> {
        let resolved = self.resolve(tool);
        // If we resolved to a concrete existing path, use it. Otherwise probe
        // PATH by attempting `--version`; only then decide it's missing.
        if resolved.is_absolute() {
            return Ok(resolved);
        }
        if tool_on_path(tool) {
            Ok(resolved)
        } else {
            Err(BackupError::ToolMissing {
                tool: tool.to_owned(),
            })
        }
    }
}

/// Build the ordered candidate `bin` directories for pg client tools. Pure over
/// its inputs so the ordering/derivation is unit-testable.
fn candidate_bin_dirs(
    pg_bin_dir: Option<PathBuf>,
    managed_data_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = pg_bin_dir {
        dirs.push(dir);
    }
    if let Some(data_dir) = managed_data_dir {
        // Managed Postgres extracts its binaries to `<data_dir_parent>/postgresql`
        // (see `autumn_web::managed_pg`); the executables live under its `bin/`.
        let install = data_dir.parent().unwrap_or(&data_dir).join("postgresql");
        dirs.push(install.join("bin"));
        // Some bundle layouts nest under a versioned subdirectory; include the
        // install root so a shallow search can find `*/bin`.
        dirs.extend(nested_bin_dirs(&install));
    }
    dirs
}

/// Find `*/bin` directories one level under `install` (managed-pg bundles that
/// nest the toolchain under a version directory). Best-effort; empty if the
/// directory can't be read.
fn nested_bin_dirs(install: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(install) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                out.push(entry.path().join("bin"));
            }
        }
    }
    out
}

/// Resolve `tool` against candidate dirs: the first dir that actually contains
/// the executable wins; otherwise return the bare name for PATH resolution.
fn resolve_tool_in(dirs: &[PathBuf], tool: &str) -> PathBuf {
    let exe = exe_name(tool);
    for dir in dirs {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(tool)
}

/// Platform executable file name for a bare tool name.
fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_owned()
    }
}

/// Whether a bare tool name resolves on PATH (probed via `--version`).
fn tool_on_path(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

// ─── Manifest ───────────────────────────────────────────────────────────────

/// One entry in a backup manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ManifestTarget {
    /// Stable label: `"control"` or `"shard:<name>"`.
    label: String,
    /// Artifact file name within the run directory.
    file: String,
    /// The database name captured (credential-free; for humans).
    #[serde(default)]
    database: String,
    /// Which backend produced this artifact: `"postgres"` or `"sqlite"`. An
    /// absent field means `"postgres"`. Per target, not per run, so a mixed
    /// topology restores each entry through the mechanism that wrote it.
    #[serde(default)]
    backend: String,
}

/// Self-describing metadata for a backup run. Written as `manifest.json`; the
/// contract #1619 (offsite upload) enumerates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Manifest {
    /// autumn-cli version that produced the backup.
    autumn_version: String,
    /// RFC 3339 UTC timestamp.
    created_at: String,
    /// Effective profile the URLs were resolved under.
    profile: String,
    /// `"custom"` or `"plain"`.
    format: String,
    /// The databases captured.
    targets: Vec<ManifestTarget>,
}

/// The manifest file name inside a run directory.
const MANIFEST_FILE: &str = "manifest.json";

fn write_manifest(run_dir: &Path, manifest: &Manifest) -> Result<(), BackupError> {
    let json = serde_json::to_string_pretty(manifest).map_err(|e| BackupError::Io {
        context: "serializing manifest".to_owned(),
        source: std::io::Error::other(e),
    })?;
    let path = run_dir.join(MANIFEST_FILE);
    std::fs::write(&path, json).map_err(BackupError::io(format!("writing {}", path.display())))
}

fn read_manifest(run_dir: &Path) -> Result<Manifest, BackupError> {
    let path = run_dir.join(MANIFEST_FILE);
    let json = std::fs::read_to_string(&path).map_err(|e| BackupError::BadArtifact {
        detail: format!("{} has no readable {MANIFEST_FILE}: {e}", run_dir.display()),
    })?;
    serde_json::from_str(&json).map_err(|e| BackupError::BadArtifact {
        detail: format!("{} is not a valid manifest: {e}", path.display()),
    })
}

// ─── Path / naming helpers (pure) ───────────────────────────────────────────

/// The root directory for a profile's backup run directories:
/// `<dir or ./backups>/<profile>`.
fn backup_root(dir: Option<&Path>, profile: &str) -> PathBuf {
    let base = dir.map_or_else(|| PathBuf::from(DEFAULT_BACKUP_DIR), Path::to_path_buf);
    base.join(sanitize_component(profile))
}

/// The run-directory name for a given instant: a sortable UTC timestamp.
fn run_dir_name(now: &chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Create a fresh, uniquely-named run directory under `root`, starting from
/// `base`. The leaf is created **exclusively** (`create_dir`, which fails if it
/// already exists) so a run never reuses — and silently overwrites the
/// `control.dump`/shards/`manifest.json` of — an earlier restore point. Two
/// backups for the same profile in the same second (an overlapping cron run and
/// a manual retry) would otherwise collide, since the name has only second
/// precision; on collision we disambiguate with an incrementing `-N` suffix
/// (`<base>`, `<base>-2`, `<base>-3`, …).
///
/// The suffixed names stay lexically sortable so retention ordering in
/// [`list_run_dirs`]/[`prune`] is preserved: `<base>` sorts before `<base>-2`
/// (it's a prefix), the `-N` variants sort among themselves, and every
/// same-second variant shares the `…SSZ` prefix so it all sorts before the next
/// second's `…S(S+1)Z`.
fn create_unique_run_dir(root: &Path, base: &str) -> Result<PathBuf, BackupError> {
    // The exclusive `create_dir` below won't create intermediate directories, so
    // ensure the profile root exists first.
    std::fs::create_dir_all(root)
        .map_err(BackupError::io(format!("creating {}", root.display())))?;
    for attempt in 1..=MAX_RUN_DIR_ATTEMPTS {
        let name = if attempt == 1 {
            base.to_owned()
        } else {
            format!("{base}-{attempt}")
        };
        let candidate = root.join(&name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(BackupError::io(format!("creating {}", candidate.display()))(e));
            }
        }
    }
    Err(BackupError::Io {
        context: format!("creating a unique run directory under {}", root.display()),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("exhausted {MAX_RUN_DIR_ATTEMPTS} same-timestamp attempts for {base:?}"),
        ),
    })
}

/// The artifact file name for one target and format.
fn artifact_file_name(label: &str, backend: TargetBackend, format: BackupFormat) -> String {
    let ext = backend.extension(format);
    if label == "control" {
        format!("control.{ext}")
    } else if let Some(name) = label.strip_prefix("shard:") {
        format!("shard-{}.{ext}", sanitize_component(name))
    } else {
        format!("{}.{ext}", sanitize_component(label))
    }
}

/// Sanitize a string for safe use as a single path component.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Reject components that would resolve to the current/parent directory (or
    // nothing at all): joining `"."`/`".."`/`""` onto the backup root enables
    // directory traversal out of it.
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

/// Resolve the restore target label for a *bare single-file* artifact (one with
/// no accompanying manifest to name its target).
///
/// This is the inverse of [`artifact_file_name`]'s naming convention:
/// * an explicit `--shard <name>` always wins (the file IS the artifact the user
///   pointed at, so no manifest entry is required);
/// * otherwise a `shard-<name>.*` file resolves to that shard, and a `control.*`
///   file to the control database;
/// * a `shard-*` file whose shard name can't be recovered is an error rather
///   than a silent restore into control;
/// * any other (user-renamed) file falls back to control — the backup writer
///   only ever emits `control.*` / `shard-*`, so this path carries no shard
///   cross-target hazard while preserving the historical single-file behavior.
fn single_file_target_label(file: &str, shard: Option<&str>) -> Result<String, BackupError> {
    if let Some(name) = shard {
        return Ok(format!("shard:{name}"));
    }
    let stem = Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file);
    if stem == "control" {
        return Ok("control".to_owned());
    }
    if let Some(shard_name) = stem.strip_prefix("shard-") {
        if shard_name.is_empty() {
            return Err(BackupError::BadArtifact {
                detail: format!(
                    "{file:?} looks like a shard artifact but its shard name is missing.\n  \
                     Pass --shard <name>, or restore the run directory (with its manifest.json)."
                ),
            });
        }
        return Ok(format!("shard:{shard_name}"));
    }
    Ok("control".to_owned())
}

/// Infer a bare artifact file's backend and (Postgres) format from its
/// extension — the inverse of [`artifact_file_name`].
fn infer_artifact_kind(path: &Path) -> Option<(TargetBackend, BackupFormat)> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("dump") => Some((TargetBackend::Postgres, BackupFormat::Custom)),
        Some("sql") => Some((TargetBackend::Postgres, BackupFormat::Plain)),
        Some("sqlite") => Some((TargetBackend::Sqlite, BackupFormat::default())),
        _ => None,
    }
}

/// Parse just the database name out of a connection URL (credential-safe; used
/// only for human-facing messages). Falls back to `"(database)"`.
fn parsed_db_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next().map(str::to_owned))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(database)".to_owned())
}

/// Count non-comment entries in a `pg_restore --list` TOC listing.
fn toc_entry_count(listing: &str) -> usize {
    listing
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with(';')
        })
        .count()
}

/// Whether a plain SQL dump text contains `pg_dump`'s completion marker.
fn plain_dump_is_complete(contents: &str) -> bool {
    contents.contains(PLAIN_COMPLETE_MARKER)
}

/// A human description of a process exit code.
fn exit_desc(code: Option<i32>) -> String {
    code.map_or_else(|| "with a signal".to_owned(), |c| format!("with code {c}"))
}

/// Current UTC instant (indirection kept tiny so tests can reason about naming).
fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

// ─── Offsite S3 upload / restore (issue #1619) ───────────────────────────────

/// A resolved, ready-to-use offsite destination. Built from `[backup.offsite]`
/// (via [`load_offsite`]) with the profile overlay + `AUTUMN_*` overrides
/// already applied. Holds the app's `[storage.s3]` bucket/endpoint too so the
/// distinct-destination guard (AC #3) can compare them without reloading config.
struct ResolvedOffsite {
    s3: S3Config,
    prefix: String,
    keep: Option<usize>,
    allow_shared_bucket: bool,
    access_key_id_env: Option<String>,
    secret_access_key_env: Option<String>,
    app_storage_bucket: Option<String>,
    app_storage_endpoint: Option<String>,
    /// The run's target profile — credentials are read from the `.env.<profile>`
    /// overlay for THIS profile, matching the config read (issue #1619 P2 #6).
    profile: String,
}

impl ResolvedOffsite {
    /// Build the transfer client, enforcing the distinct-destination guard
    /// (AC #3) and resolving credentials from the named env vars first, so a
    /// misconfiguration fails fast before any transfer.
    fn build_client(&self) -> Result<S3Client, BackupError> {
        if !self.allow_shared_bucket
            && destinations_conflict(
                &self.s3.bucket,
                self.s3.endpoint.as_deref(),
                self.app_storage_bucket.as_deref(),
                self.app_storage_endpoint.as_deref(),
            )
        {
            return Err(BackupError::SharedBucketRefused {
                bucket: self.s3.bucket.clone(),
            });
        }
        let creds = self.resolve_credentials()?;
        let client = S3Client::new(self.s3.clone(), creds).map_err(|e| BackupError::Offsite {
            op: "connect",
            detail: e.to_string(),
        })?;
        // Ops escape hatch: tune ONLY the multipart part size (bytes). This does
        // NOT lower the multipart threshold, so shrinking the part size can never
        // push a normal (sub-threshold) file into a multipart upload — it only
        // changes how an already-multipart transfer is chunked. The client
        // re-floors the value to S3's 5 MiB minimum.
        Ok(match self.multipart_part_size_override() {
            Some(bytes) => client.with_part_size(bytes),
            None => client,
        })
    }

    /// Read the `AUTUMN_OFFSITE_MULTIPART_PART_SIZE_BYTES` override (profile-aware,
    /// so a `.env.<profile>` value is honored). A blank/unparseable value is
    /// ignored (falls back to the client defaults).
    fn multipart_part_size_override(&self) -> Option<u64> {
        use autumn_web::config::Env as _;
        let env = dotenv_env_for_profile(&self.profile);
        env.var("AUTUMN_OFFSITE_MULTIPART_PART_SIZE_BYTES")
            .ok()
            .and_then(|v: String| v.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
    }

    /// Resolve the access key / secret from the environment variables NAMED by
    /// config. Credential *values* never appear in errors — only the var names.
    fn resolve_credentials(&self) -> Result<S3Credentials, BackupError> {
        // Read credentials from the SAME profile-specific `.env.<profile>` overlay
        // the offsite config was resolved under (issue #1619 P2 #6), so a
        // `.env.<profile>`-provided credential value is honored.
        let env = dotenv_env_for_profile(&self.profile);
        let access_key_id = read_credential_env(&env, self.access_key_id_env.as_deref())?;
        let secret_access_key = read_credential_env(&env, self.secret_access_key_env.as_deref())?;
        Ok(S3Credentials {
            access_key_id,
            secret_access_key,
        })
    }
}

/// Read a credential from the env var `var` names. Errors name only the VARIABLE
/// (never a value). A `None`/blank config var name is itself an error.
fn read_credential_env(
    env: &dyn autumn_web::config::Env,
    var: Option<&str>,
) -> Result<String, BackupError> {
    let var = var
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| BackupError::MissingCredentialEnv {
            var: "(none configured)".to_owned(),
        })?;
    env.var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| BackupError::MissingCredentialEnv {
            var: var.to_owned(),
        })
}

/// A leniently-loaded `[backup.offsite]` section: enough to learn `auto_upload`
/// WITHOUT validating the bucket/credentials, so a plain local `autumn db backup`
/// (no `--upload`, no `auto_upload`) never fails just because an incomplete
/// offsite section exists. Call [`LoadedOffsite::resolve`] — only when an upload
/// is actually requested — to get the validated, ready-to-use destination.
struct LoadedOffsite {
    offsite: autumn_web::config::OffsiteBackupConfig,
    app_storage_bucket: Option<String>,
    app_storage_endpoint: Option<String>,
    /// The target profile this section was resolved under (issue #1619 P2 #6).
    profile: String,
}

impl LoadedOffsite {
    /// Validate and resolve into a ready-to-use [`ResolvedOffsite`]. This is the
    /// STRICT step — bucket is required here — and must only run when uploading.
    fn resolve(self) -> Result<ResolvedOffsite, BackupError> {
        let Self {
            offsite,
            app_storage_bucket,
            app_storage_endpoint,
            profile,
        } = self;
        let bucket = offsite
            .s3
            .bucket
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| BackupError::OffsiteConfig {
                detail: "backup.offsite.s3.bucket is required".to_owned(),
            })?;
        // Region is only meaningful for AWS + SigV4 scope; many S3-compatible
        // endpoints ignore it. Default to us-east-1 when unset.
        let region = offsite
            .s3
            .region
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("us-east-1")
            .to_owned();
        let s3 = S3Config {
            bucket,
            region,
            endpoint: offsite
                .s3
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned),
            force_path_style: offsite.s3.force_path_style,
        };
        Ok(ResolvedOffsite {
            s3,
            prefix: normalize_offsite_prefix(offsite.prefix.as_deref()),
            keep: offsite.keep,
            allow_shared_bucket: offsite.allow_shared_bucket,
            access_key_id_env: offsite.s3.access_key_id_env,
            secret_access_key_env: offsite.s3.secret_access_key_env,
            app_storage_bucket,
            app_storage_endpoint,
            profile,
        })
    }
}

/// Decide whether a backup run should upload, reading ONLY the
/// `[backup.offsite].auto_upload` bit — WITHOUT a full `AutumnConfig` load (no
/// global validation, no encrypted-credentials decryption), so a local-only
/// backup is never blocked by unrelated config/credentials issues (issue #1619
/// P2 #8). Resolution matches the config layering: real env
/// `AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD` → `.env.<profile>` → merged TOML
/// `[backup.offsite].auto_upload` (base + `[profile.<p>]` overlay) → `false`.
fn auto_upload_probe(profile: &str) -> bool {
    use autumn_web::config::Env as _;
    // Normalize aliases/case to the app's resolved profile (P2 #20) so both the
    // dotenv overlay and the `[profile.<p>]` selection match what the app reads.
    let profile = migrate::canonical_profile(profile);
    // The profile-aware dotenv overlay covers BOTH the real env and `.env.<p>`
    // (a real env var wins inside the overlay), matching P2 #6.
    let env = dotenv_env_for_profile(&profile);
    // Read `autumn.toml` from the config/manifest dir (AUTUMN_MANIFEST_DIR),
    // matching where the full loader + the dotenv overlay look — so an installed/
    // daemon launch with a different CWD still sees `auto_upload` (P2 #15).
    let table = migrate::read_autumn_toml_table_with_profile_from_config_dir(Some(&profile));
    auto_upload_from_sources(|k| env.var(k).ok(), table.as_ref())
}

/// Pure resolution of `auto_upload` from an env lookup + a merged TOML table.
/// Env (real + `.env`) wins over TOML; default `false`. Separated so the
/// precedence is unit-testable without touching the filesystem/process env.
fn auto_upload_from_sources<F>(env_var: F, table: Option<&toml::Table>) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = env_var("AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD")
        && let Some(b) = parse_bool_flag(&raw)
    {
        return b;
    }
    table
        .and_then(|t| t.get("backup"))
        .and_then(|v| v.get("offsite"))
        .and_then(|v| v.get("auto_upload"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
}

/// Parse a boolean env flag the same way the config loader's `parse_env_bool`
/// does: `1`/`true` → true, `0`/`false` → false (case-insensitive, trimmed),
/// anything else → `None` (ignored).
fn parse_bool_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Leniently resolve the `[backup.offsite]` destination for `profile` WITHOUT a
/// full `AutumnConfig::load` — i.e. WITHOUT decrypting the app credential store
/// (`config/credentials/<profile>.toml.enc`) or running app-wide validation, so
/// offsite commands work on a minimal DR/recovery box that has only DB + offsite
/// env vars and NO `AUTUMN_MASTER_KEY` (issue #1619 P2 #9). It reads only what
/// offsite needs — the merged TOML (base + `[profile.<p>]`) plus real env and
/// `.env.<profile>` overrides — for `[backup.offsite]` AND the `[storage.s3]`
/// bucket/endpoint (the distinct-destination guard needs them).
///
/// `Ok(None)` when no offsite section is configured. NO bucket/cred validation
/// here — that is deferred to [`LoadedOffsite::resolve`] so a local backup is
/// never blocked by an incomplete offsite section.
fn load_offsite(profile: &str) -> Result<Option<LoadedOffsite>, BackupError> {
    // Normalize aliases/case to the app's resolved profile (P2 #20) so the dotenv
    // overlay, the `[profile.<p>]` selection, and the stored profile (later reused
    // for credential resolution) all match what the running app reads.
    let profile = migrate::canonical_profile(profile);
    // Real env + `.env.<profile>` overlay (profile-aware, P2 #6); merged TOML
    // (base + `[profile.<p>]`) — neither decrypts credentials nor validates.
    let env = dotenv_env_for_profile(&profile);
    // Read `autumn.toml` from the config/manifest dir (AUTUMN_MANIFEST_DIR),
    // matching the full loader + dotenv overlay — so an installed/daemon launch
    // whose CWD differs from the config dir still finds the offsite destination
    // (and enforces the distinct-bucket guard) instead of reporting it
    // unconfigured (P2 #15).
    let table = migrate::read_autumn_toml_table_with_profile_from_config_dir(Some(&profile));
    resolve_loaded_offsite(table.as_ref(), &env, &profile)
}

/// Pure core of [`load_offsite`]: build the offsite view from a merged TOML table
/// and an `Env`, applying `AUTUMN_*` overrides via the config crate's
/// env-override machinery (which does NOT decrypt credentials or run global
/// validation). Separated so the DR-box behavior is unit-testable without
/// touching the filesystem / process env / credential store.
fn resolve_loaded_offsite(
    table: Option<&toml::Table>,
    env: &dyn autumn_web::config::Env,
    profile: &str,
) -> Result<Option<LoadedOffsite>, BackupError> {
    // Deserialize the merged TOML into AutumnConfig — plain `Deserialize`, so no
    // credential decryption and no `validate()` (those live only in `load*`).
    let mut cfg = match table {
        Some(t) => {
            let toml_str = toml::to_string(t).map_err(|e| BackupError::Offsite {
                op: "config",
                detail: e.to_string(),
            })?;
            toml::from_str::<autumn_web::config::AutumnConfig>(&toml_str).map_err(|e| {
                BackupError::Offsite {
                    op: "config",
                    detail: e.to_string(),
                }
            })?
        }
        None => autumn_web::config::AutumnConfig::default(),
    };
    // Apply `AUTUMN_*` env overrides (incl. backup.offsite + storage.s3). This is
    // pure field-setting — no decryption, no validation.
    cfg.apply_env_overrides_with_env(env);

    let Some(offsite) = cfg.backup.offsite else {
        return Ok(None);
    };
    // Only treat the `[storage.s3]` bucket as the APP storage destination when the
    // resolved storage backend is actually S3 (P2 #16). If the app runs on the
    // local/disabled backend, a leftover `[storage.s3]` bucket is inert — it is
    // not where the app writes blobs — so it must NOT trip the shared-bucket guard
    // and force `allow_shared_bucket` for an offsite backup that happens to reuse
    // that bucket name.
    let storage_is_s3 = cfg.storage.backend == autumn_web::storage::StorageBackend::S3;
    let (app_storage_bucket, app_storage_endpoint) = if storage_is_s3 {
        (
            cfg.storage
                .s3
                .bucket
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_owned),
            cfg.storage
                .s3
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned),
        )
    } else {
        (None, None)
    };
    Ok(Some(LoadedOffsite {
        offsite: *offsite,
        app_storage_bucket,
        app_storage_endpoint,
        profile: profile.to_owned(),
    }))
}

/// Whether the offsite destination collides with the app's user-facing blob
/// storage: same bucket AND same canonical endpoint authority. Pure for unit
/// testing the AC #3 opt-in guard.
pub fn destinations_conflict(
    offsite_bucket: &str,
    offsite_endpoint: Option<&str>,
    app_bucket: Option<&str>,
    app_endpoint: Option<&str>,
) -> bool {
    let Some(app_bucket) = app_bucket else {
        return false;
    };
    app_bucket == offsite_bucket
        && canonical_authority(offsite_endpoint, offsite_bucket)
            == canonical_authority(app_endpoint, app_bucket)
}

/// Canonicalize an endpoint to a comparable `scheme://host[:non-default-port]`
/// authority so spellings that address the SAME S3 service compare equal:
///
/// * `None`/empty (the AWS default endpoint) and any explicit `*.amazonaws.com`
///   URL both collapse to `"aws"` — S3 bucket names are globally unique, so the
///   same bucket name is the same bucket regardless of regional endpoint.
/// * default ports (443/https, 80/http) are dropped so `host:443` == bare host.
/// * a virtual-hosted `{bucket}.` host prefix is stripped so it compares equal
///   to the path-style spelling of the same endpoint.
///
/// This prevents a genuinely shared bucket from slipping past the AC #3 guard
/// because one side wrote the endpoint out explicitly and the other left it None.
fn canonical_authority(endpoint: Option<&str>, bucket: &str) -> String {
    let raw = endpoint.unwrap_or("").trim();
    if raw.is_empty() {
        return "aws".to_owned();
    }
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let Ok(url) = url::Url::parse(&with_scheme) else {
        return raw.to_ascii_lowercase();
    };
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "amazonaws.com" || host.ends_with(".amazonaws.com") {
        return "aws".to_owned();
    }
    // Strip a virtual-hosted `{bucket}.` prefix so path-style and virtual-hosted
    // spellings of the same endpoint compare equal.
    let bucket_prefix = format!("{}.", bucket.to_ascii_lowercase());
    let host = host.strip_prefix(&bucket_prefix).unwrap_or(&host);
    let scheme = url.scheme().to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    match url.port() {
        Some(port) if Some(port) != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    }
}

/// Normalize a configured key prefix: trim surrounding whitespace and slashes so
/// object keys join cleanly (an empty prefix means "bucket root").
fn normalize_offsite_prefix(prefix: Option<&str>) -> String {
    prefix.unwrap_or("").trim().trim_matches('/').to_owned()
}

/// Join non-empty key segments with `/` (no leading/trailing slash).
fn join_key(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

/// The object key for one file of a run: `{prefix}/{profile}/{run_id}/{file}`.
fn offsite_object_key(prefix: &str, profile: &str, run_id: &str, file: &str) -> String {
    join_key(&[prefix, profile, run_id, file])
}

/// A short, globally-unique token appended to the REMOTE run id so that two
/// hosts backing up the SAME profile in the SAME second to the SAME
/// bucket/prefix never produce colliding S3 keys (issue #1619 P2 #12). Without
/// it, `create_unique_run_dir` only disambiguates within one host's filesystem,
/// so cross-host same-second runs would overwrite/interleave each other's dumps
/// and manifests and corrupt `latest`.
///
/// Derived from host + pid + wall-clock nanoseconds, hashed with the
/// already-linked `sha2`/`hex` crates (no new dependency). It is NON-sensitive —
/// pure entropy, no secret material. The LOCAL run-dir naming (issue #1595
/// `<profile>/<timestamp>/`) is deliberately left unchanged; only the remote key
/// gains the `-<token>` suffix.
fn remote_run_token() -> String {
    use sha2::{Digest, Sha256};
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = Sha256::new();
    hasher.update(host.as_bytes());
    hasher.update([0]); // domain separator between host and pid
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    // 8 hex chars (4 bytes): with the same-second timestamp already dominating
    // the run id, a cross-host collision needs the same second AND a 32-bit token
    // collision — negligible for the handful of hosts sharing one backup prefix.
    hex::encode(&hasher.finalize()[..4])
}

/// Compose the globally-unique REMOTE run id from the LOCAL run id (a #1595
/// `<timestamp>`) and a [`remote_run_token`]: `<timestamp>-<token>`. The
/// timestamp still dominates the lexical sort, so `latest` (newest by timestamp)
/// is unaffected and the token only tie-breaks same-second runs from different
/// hosts — each a valid distinct backup, neither overwriting the other.
fn remote_run_id(local_run_id: &str, token: &str) -> String {
    format!("{local_run_id}-{token}")
}

/// The list prefix that enumerates every run for a profile (trailing slash so a
/// `list-type=2` prefix match is scoped to this profile).
fn offsite_profile_prefix(prefix: &str, profile: &str) -> String {
    format!("{}/", join_key(&[prefix, profile]))
}

/// The list prefix for the files of a single run.
fn offsite_run_prefix(prefix: &str, profile: &str, run_id: &str) -> String {
    format!("{}/", join_key(&[prefix, profile, run_id]))
}

/// Upload every file of a completed run and verify each remote object matches
/// the local file (AC #2), then run remote retention (AC #5). Returns a
/// credential-safe failure detail string (the caller wraps it into the
/// split-outcome error). Never mutates the local artifact.
fn upload_run(
    offsite: &ResolvedOffsite,
    client: &S3Client,
    run_dir: &Path,
    profile: &str,
    run_id: &str,
) -> Result<(), String> {
    eprintln!("\u{2500}\u{2500} offsite upload \u{2500}\u{2500}");
    let mut files = list_run_files(run_dir).map_err(|e| e.to_string())?;
    files.sort();
    // Upload `manifest.json` LAST so the presence of a verified remote manifest
    // implies the whole run uploaded (completeness marker for `latest`
    // resolution). A stable sort keeps the artifact files in alphabetical order
    // and moves the single manifest to the end.
    files.sort_by_key(|f| f.as_str() == MANIFEST_FILE);
    if files.is_empty() {
        return Err("the completed run directory contains no files to upload".to_owned());
    }
    let mut written_keys: Vec<String> = Vec::new();
    for file in &files {
        let path = run_dir.join(file);
        let key = offsite_object_key(&offsite.prefix, profile, run_id, file);
        // `put_file_and_verify` now self-cleans the single key it just wrote on a
        // verify failure (#1760). This run-level cleanup is still needed for the
        // OTHER keys written EARLIER in the run: record every key up-front, then on
        // ANY failure best-effort delete everything this run wrote — manifest first
        // — so a run whose upload was reported FAILED never keeps a remote
        // `manifest.json` and can't become `latest` or consume a retention slot
        // (restores the P2 #2 invariant; #21).
        written_keys.push(key.clone());
        // Stream the file straight from disk (hash pre-pass + streamed body):
        // a multi-GB dump is never read into memory.
        if let Err(e) = client.put_file_and_verify(&key, &path) {
            let detail = format!("{file}: {e}");
            cleanup_failed_upload(client, &written_keys);
            return Err(detail);
        }
        eprintln!("  \u{2713} uploaded + verified {file}");
    }
    eprintln!(
        "\u{2713} Offsite upload verified: {} object(s) under {}",
        files.len(),
        offsite_run_prefix(&offsite.prefix, profile, run_id),
    );

    // Remote retention (AC #5): only after the just-uploaded run verified, and
    // never removing that run. A retention error is loud but non-fatal — the
    // verified offsite copy already exists.
    if let Some(keep) = offsite.keep
        && let Err(e) = prune_remote(offsite, client, profile, run_id, keep)
    {
        eprintln!("  \u{26A0} offsite retention skipped: {e}");
    }
    Ok(())
}

/// Best-effort removal of the objects a FAILED [`upload_run`] wrote, so a partial
/// upload never leaves a remote `manifest.json` behind — which
/// [`complete_remote_run_ids`] would otherwise count as a COMPLETE run for
/// `latest`/retention (P2 #21). The manifest key is deleted FIRST so that even if
/// a later delete fails, the completeness marker is already gone. Delete failures
/// are swallowed: the caller surfaces the ORIGINAL upload/verify error.
fn cleanup_failed_upload(client: &S3Client, written_keys: &[String]) {
    for key in order_cleanup_keys(written_keys) {
        let _ = client.delete_object(&key);
    }
}

/// Order the keys a failed upload wrote for cleanup: the run's `manifest.json`
/// FIRST (drop the completeness marker before anything else), then the rest in
/// their written order. Pure for unit testing.
fn order_cleanup_keys(written_keys: &[String]) -> Vec<String> {
    let mut keys = written_keys.to_vec();
    // `false` (manifest) sorts before `true` (everything else); stable sort keeps
    // the remaining keys in their original order.
    keys.sort_by_key(|k| !key_is_manifest(k));
    keys
}

/// Whether an object key is a run's manifest (its final path component is
/// [`MANIFEST_FILE`]).
fn key_is_manifest(key: &str) -> bool {
    key.rsplit('/').next() == Some(MANIFEST_FILE)
}

/// List the file names (not sub-directories) directly inside a run directory.
fn list_run_files(run_dir: &Path) -> Result<Vec<String>, BackupError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(run_dir)
        .map_err(BackupError::io(format!("read {}", run_dir.display())))?
    {
        let entry = entry.map_err(BackupError::io(format!(
            "read entry in {}",
            run_dir.display()
        )))?;
        if entry.path().is_file() {
            files.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(files)
}

/// Prune remote runs for a profile, keeping the newest `keep` COMPLETE runs and
/// NEVER deleting the just-uploaded `keep_run_id`. Independent of the local
/// `--keep`.
///
/// Retention is based ONLY on complete runs (those with a remote `manifest.json`,
/// the same completeness source `resolve_latest_run` uses), so a partial/failed
/// upload can't consume a keep slot and cause an older *complete* backup to be
/// pruned. Incomplete/partial prefixes are left untouched here — deleting them
/// could race a concurrent in-progress upload; GC of stale partial prefixes is a
/// possible follow-up, out of scope for this change.
fn prune_remote(
    offsite: &ResolvedOffsite,
    client: &S3Client,
    profile: &str,
    keep_run_id: &str,
    keep: usize,
) -> Result<(), String> {
    let list_prefix = offsite_profile_prefix(&offsite.prefix, profile);
    let objects = client
        .list_objects_v2(&list_prefix)
        .map_err(|e| e.to_string())?;
    let run_ids = complete_remote_run_ids(&objects, &list_prefix);
    let to_remove = plan_remote_pruning(&run_ids, keep, keep_run_id);
    for run_id in &to_remove {
        let run_prefix = offsite_run_prefix(&offsite.prefix, profile, run_id);
        let run_objects = client
            .list_objects_v2(&run_prefix)
            .map_err(|e| e.to_string())?;
        for obj in &run_objects {
            client.delete_object(&obj.key).map_err(|e| e.to_string())?;
        }
        eprintln!("  \u{1F5D1} pruned remote backup {run_id}");
    }
    Ok(())
}

/// Extract the unique, sorted run-id (timestamp) segments from listed object
/// keys under `list_prefix` (`{list_prefix}{run_id}/{file}`). Test-only: the
/// live paths (`latest` resolution and retention) use the manifest-gated
/// [`complete_remote_run_ids`] so partial runs never count.
#[cfg(test)]
fn remote_run_ids(objects: &[s3::S3Object], list_prefix: &str) -> Vec<String> {
    let mut ids: Vec<String> = objects
        .iter()
        .filter_map(|o| o.key.strip_prefix(list_prefix))
        .filter_map(|rest| rest.split('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The unique, sorted run ids under `list_prefix` that have a remote
/// `manifest.json` — i.e. COMPLETE runs. Because [`upload_run`] uploads the
/// manifest last, a `--upload` that died partway leaves artifacts but no
/// manifest, so its run id is excluded here. This keeps a partial failed upload
/// from shadowing the last good backup when resolving `latest`.
fn complete_remote_run_ids(objects: &[s3::S3Object], list_prefix: &str) -> Vec<String> {
    let mut ids: Vec<String> = objects
        .iter()
        .filter_map(|o| o.key.strip_prefix(list_prefix))
        .filter_map(|rest| {
            let mut parts = rest.splitn(2, '/');
            match (parts.next(), parts.next()) {
                (Some(run_id), Some(file)) if !run_id.is_empty() && file == MANIFEST_FILE => {
                    Some(run_id.to_owned())
                }
                _ => None,
            }
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Pure remote-retention rule: given run ids sorted ascending, keep the newest
/// `keep` and return the older ones to delete — but ALWAYS exclude
/// `keep_run_id` (the just-uploaded run) from deletion, even if retention math
/// would otherwise remove it. `keep == 0` is clamped to 1.
fn plan_remote_pruning(sorted_run_ids: &[String], keep: usize, keep_run_id: &str) -> Vec<String> {
    let keep = keep.max(1);
    if sorted_run_ids.len() <= keep {
        return Vec::new();
    }
    let remove_count = sorted_run_ids.len() - keep;
    sorted_run_ids[..remove_count]
        .iter()
        .filter(|id| id.as_str() != keep_run_id)
        .cloned()
        .collect()
}

// ─── Offsite list (`autumn db offsite list`, AC #4) ──────────────────────────

/// Entry point for `autumn db offsite list`. Prints a credential-safe message
/// and exits non-zero on failure.
pub fn run_offsite_list(profile: Option<&str>) {
    eprintln!("\u{1F342} autumn db offsite list\n");
    if let Err(e) = offsite_list(profile) {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

fn offsite_list(profile: Option<&str>) -> Result<(), BackupError> {
    // Canonicalize so the list prefix matches the profile the upload wrote under
    // (P2 #22): `list --profile production` and `--profile prod` both read `prod/`.
    let profile = migrate::canonical_profile(&migrate::effective_profile(profile));
    let offsite = load_offsite(&profile)?
        .ok_or(BackupError::OffsiteNotConfigured)?
        .resolve()?;
    let client = offsite.build_client()?;
    let list_prefix = offsite_profile_prefix(&offsite.prefix, &profile);
    let objects = client
        .list_objects_v2(&list_prefix)
        .map_err(|e| BackupError::Offsite {
            op: "list",
            detail: e.to_string(),
        })?;
    let runs = group_offsite_objects(&objects, &list_prefix);
    if runs.is_empty() {
        eprintln!("  \u{2139} No offsite backups found for profile {profile:?}.");
        return Ok(());
    }
    print!("{}", format_offsite_listing(&runs));
    Ok(())
}

/// One offsite run, grouped for listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OffsiteRun {
    run_id: String,
    files: Vec<(String, u64)>,
    total: u64,
    /// Whether the run has a remote `manifest.json` (a complete upload). A run
    /// missing it is a partial/failed upload and is flagged in the listing.
    complete: bool,
}

/// Group listed objects (`{list_prefix}{run_id}/{file}`) into runs, sorted by
/// run id ascending. A run is `complete` iff it has a `manifest.json` object.
/// Pure for unit testing.
fn group_offsite_objects(objects: &[s3::S3Object], list_prefix: &str) -> Vec<OffsiteRun> {
    let mut map: std::collections::BTreeMap<String, OffsiteRun> = std::collections::BTreeMap::new();
    for obj in objects {
        let Some(rest) = obj.key.strip_prefix(list_prefix) else {
            continue;
        };
        let mut parts = rest.splitn(2, '/');
        let (Some(run_id), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        if run_id.is_empty() || file.is_empty() {
            continue;
        }
        let run = map.entry(run_id.to_owned()).or_insert_with(|| OffsiteRun {
            run_id: run_id.to_owned(),
            files: Vec::new(),
            total: 0,
            complete: false,
        });
        run.files.push((file.to_owned(), obj.size));
        run.total += obj.size;
        if file == MANIFEST_FILE {
            run.complete = true;
        }
    }
    map.into_values().collect()
}

/// Render a run listing as a human table: timestamp, files (labels), size. An
/// incomplete run (no remote manifest) is flagged so a partial failed upload is
/// never mistaken for a restorable backup. Pure.
fn format_offsite_listing(runs: &[OffsiteRun]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    // Column shows the FULL remote run id (`<timestamp>-<token>`, P2 #12) so an
    // operator can copy it verbatim into `restore offsite:<profile>/<run id>`.
    let _ = writeln!(out, "{:<28}  {:>10}  FILES", "RUN ID", "SIZE");
    for run in runs {
        let mut labels: Vec<&str> = run.files.iter().map(|(f, _)| f.as_str()).collect();
        labels.sort_unstable();
        let mut files = labels.join(", ");
        if !run.complete {
            files.push_str("  (INCOMPLETE — no manifest, not restorable)");
        }
        let _ = writeln!(
            out,
            "{:<28}  {:>10}  {}",
            run.run_id,
            human_size(run.total),
            files,
        );
    }
    out
}

/// A compact human byte size (KiB/MiB/GiB), for the listing table.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

// ─── Offsite restore (AC #4) ─────────────────────────────────────────────────

/// A parsed offsite restore reference: which profile's run to pull, and which.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OffsiteRef {
    profile: String,
    selector: OffsiteSelector,
}

/// How the run is chosen: newest, or an explicit timestamp/run id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OffsiteSelector {
    Latest,
    Exact(String),
}

/// Resolve an offsite reference when offsite restore is indicated. Returns:
///
/// * `None` — offsite is NOT indicated (no `--offsite` flag and no `offsite:`
///   prefix); the caller routes to a normal LOCAL restore.
/// * `Some(Ok(_))` — offsite is indicated and the reference parsed.
/// * `Some(Err(_))` — offsite is indicated but the reference is malformed; the
///   caller MUST surface the error and MUST NEVER fall through to local restore
///   (otherwise `restore --offsite latest` could silently restore an unrelated
///   local path named `latest` in the cwd).
fn resolve_offsite_ref(flag: bool, artifact: &str) -> Option<Result<OffsiteRef, BackupError>> {
    let has_prefix = artifact.starts_with("offsite:");
    if !flag && !has_prefix {
        return None;
    }
    Some(
        parse_offsite_ref(artifact).ok_or_else(|| BackupError::InvalidOffsiteRef {
            reference: artifact.to_owned(),
        }),
    )
}

/// Parse `offsite:<profile>/<timestamp|latest>` (the `offsite:` prefix is
/// optional). Returns `None` when the shape is not `<profile>/<selector>`.
fn parse_offsite_ref(s: &str) -> Option<OffsiteRef> {
    let s = s.strip_prefix("offsite:").unwrap_or(s).trim();
    let (profile, selector) = s.split_once('/')?;
    let profile = profile.trim();
    let selector = selector.trim();
    if profile.is_empty() || selector.is_empty() {
        return None;
    }
    let selector = if selector.eq_ignore_ascii_case("latest") {
        OffsiteSelector::Latest
    } else {
        OffsiteSelector::Exact(selector.to_owned())
    };
    Some(OffsiteRef {
        profile: profile.to_owned(),
        selector,
    })
}

/// Whether `name` is a single, plain filename component safe to `join` onto the
/// download temp dir (P2 #13). Rejects anything that could escape the temp dir
/// BEFORE manifest validation: a name is safe only when it contains no path
/// separator (`/` OR `\` — `\` is a valid Unix filename byte but an S3 key
/// written by a Windows run could carry a traversal like `..\..\evil`), is not
/// `.`/`..`, is not absolute, and has no Windows drive/prefix. The
/// `Component::Normal(c) if c == name` check is the robust cross-platform core;
/// the explicit `\` reject covers the Unix case where `\` is not a separator.
fn is_safe_leaf_name(name: &str) -> bool {
    use std::path::{Component, Path};
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return false;
    }
    let p = Path::new(name);
    p.components().count() == 1
        && matches!(p.components().next(), Some(Component::Normal(c)) if c == name)
}

/// Download the referenced offsite run to a fresh temp dir and restore from it
/// via the identical [`RestorePlan::discover`] path (AC #4). The temp dir is
/// removed when the guard drops (after the restore completes or fails).
fn restore_from_offsite(args: &RestoreArgs, oref: &OffsiteRef) -> Result<(), BackupError> {
    // Canonicalize the profile so the REMOTE key prefix matches what upload wrote
    // (P2 #22): `offsite:production/latest` reads the same `prod/…` prefix that
    // `--profile production` uploaded under.
    let profile = migrate::canonical_profile(&oref.profile);
    let offsite = load_offsite(&profile)?
        .ok_or(BackupError::OffsiteNotConfigured)?
        .resolve()?;
    let client = offsite.build_client()?;

    let run_id = match &oref.selector {
        OffsiteSelector::Exact(sel) => resolve_exact_run(&offsite, &client, &profile, sel)?,
        OffsiteSelector::Latest => resolve_latest_run(&offsite, &client, &profile)?,
    };
    eprintln!("  \u{2139} restoring from offsite {profile}/{run_id}");

    let run_prefix = offsite_run_prefix(&offsite.prefix, &profile, &run_id);
    let objects = client
        .list_objects_v2(&run_prefix)
        .map_err(|e| BackupError::Offsite {
            op: "list",
            detail: e.to_string(),
        })?;
    if objects.is_empty() {
        return Err(BackupError::BadArtifact {
            detail: format!(
                "no offsite backup found at {profile}/{run_id} (nothing under {run_prefix})"
            ),
        });
    }

    let temp =
        tempfile::tempdir().map_err(BackupError::io("creating a temp dir for offsite restore"))?;
    for obj in &objects {
        let file = obj
            .key
            .strip_prefix(run_prefix.as_str())
            .filter(|f| is_safe_leaf_name(f))
            .ok_or_else(|| BackupError::Offsite {
                op: "download",
                detail: format!(
                    "refusing offsite object with an unsafe key layout: {} (each object name \
                     must be a single plain filename — no path separators, traversal, or drive \
                     prefix)",
                    obj.key
                ),
            })?;
        // Stream the object straight to disk (constant memory), so a multi-GB
        // dump is never buffered while downloading for restore.
        let dest = temp.path().join(file);
        let mut out = std::fs::File::create(&dest)
            .map_err(BackupError::io(format!("creating downloaded {file}")))?;
        client
            .download_object(&obj.key, &mut out)
            .map_err(|e| BackupError::Offsite {
                op: "download",
                detail: format!("{file}: {e}"),
            })?;
        eprintln!("  \u{2913} downloaded {file}");
    }

    // Hand the downloaded run to the SAME plan path so the identical
    // integrity-refusal + restore protocol applies unchanged.
    let plan = RestorePlan::discover(temp.path(), args.shard.as_deref())?;
    apply_restore_plan(&plan, args)
    // `temp` is dropped here, removing the downloaded artifacts.
}

/// Resolve an explicit offsite selector to a single complete remote run id
/// (P2 #12). The selector may be a full remote run id (`<ts>-<token>`) or a bare
/// `<ts>` timestamp (operator convenience, and the pre-token spelling): a
/// complete run matches when it equals the selector or begins with
/// `"{selector}-"`. Errors (never silently guesses) when nothing matches or when
/// several do — the latter happens when two hosts backed up the same second, and
/// the message lists the full run ids so the operator can re-run with one.
fn resolve_exact_run(
    offsite: &ResolvedOffsite,
    client: &S3Client,
    profile: &str,
    selector: &str,
) -> Result<String, BackupError> {
    let list_prefix = offsite_profile_prefix(&offsite.prefix, profile);
    let objects = client
        .list_objects_v2(&list_prefix)
        .map_err(|e| BackupError::Offsite {
            op: "list",
            detail: e.to_string(),
        })?;
    let run_ids = complete_remote_run_ids(&objects, &list_prefix);
    match match_exact_remote_run(&run_ids, selector) {
        Ok(id) => Ok(id),
        Err(candidates) if candidates.is_empty() => Err(BackupError::BadArtifact {
            detail: format!(
                "no complete offsite backup matching {selector:?} for profile {profile:?}"
            ),
        }),
        Err(candidates) => Err(BackupError::BadArtifact {
            detail: format!(
                "offsite reference {selector:?} is ambiguous for profile {profile:?}: it matches \
                 {} runs ({}).\n  Re-run restore with the full run id (see `autumn db offsite list`).",
                candidates.len(),
                candidates.join(", "),
            ),
        }),
    }
}

/// Pure selector match (P2 #12): from COMPLETE remote run ids, return the single
/// one referred to by `selector` (a full `<ts>-<token>` id, or a bare `<ts>` that
/// prefixes exactly one tokened id). `Ok(id)` on a unique match; `Err(matches)`
/// otherwise — an empty vec means none, a multi-element vec means ambiguous.
fn match_exact_remote_run(run_ids: &[String], selector: &str) -> Result<String, Vec<String>> {
    let token_prefix = format!("{selector}-");
    let matches: Vec<String> = run_ids
        .iter()
        .filter(|id| id.as_str() == selector || id.starts_with(&token_prefix))
        .cloned()
        .collect();
    if let [only] = matches.as_slice() {
        Ok(only.clone())
    } else {
        Err(matches)
    }
}

/// Resolve the newest run id for a profile from the offsite listing.
fn resolve_latest_run(
    offsite: &ResolvedOffsite,
    client: &S3Client,
    profile: &str,
) -> Result<String, BackupError> {
    let list_prefix = offsite_profile_prefix(&offsite.prefix, profile);
    let objects = client
        .list_objects_v2(&list_prefix)
        .map_err(|e| BackupError::Offsite {
            op: "list",
            detail: e.to_string(),
        })?;
    // Only consider COMPLETE runs (those with a remote manifest.json), so a
    // partial/failed upload never shadows the last good backup.
    complete_remote_run_ids(&objects, &list_prefix)
        .into_iter()
        .next_back() // sorted ascending; newest complete run is last
        .ok_or_else(|| BackupError::BadArtifact {
            detail: format!("no complete offsite backups found for profile {profile:?}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_accepts_documented_spellings() {
        assert_eq!(BackupFormat::parse("custom").unwrap(), BackupFormat::Custom);
        assert_eq!(BackupFormat::parse("C").unwrap(), BackupFormat::Custom);
        assert_eq!(BackupFormat::parse("plain").unwrap(), BackupFormat::Plain);
        assert_eq!(BackupFormat::parse("sql").unwrap(), BackupFormat::Plain);
        assert!(BackupFormat::parse("gzip").is_err());
    }

    #[test]
    fn default_format_is_custom_compressed() {
        assert_eq!(BackupFormat::default(), BackupFormat::Custom);
        assert_eq!(BackupFormat::Custom.extension(), "dump");
        assert_eq!(BackupFormat::Plain.extension(), "sql");
    }

    #[test]
    fn backup_root_nests_under_profile() {
        assert_eq!(
            backup_root(None, "dev"),
            PathBuf::from("backups").join("dev")
        );
        assert_eq!(
            backup_root(Some(Path::new("/var/backups")), "prod"),
            PathBuf::from("/var/backups").join("prod")
        );
    }

    #[test]
    fn run_dir_name_is_sortable_utc() {
        use chrono::TimeZone as _;
        let ts = chrono::Utc.with_ymd_and_hms(2026, 7, 10, 4, 5, 6).unwrap();
        assert_eq!(run_dir_name(&ts), "20260710T040506Z");
    }

    #[test]
    fn artifact_file_name_distinguishes_control_and_shards() {
        assert_eq!(
            artifact_file_name("control", TargetBackend::Postgres, BackupFormat::Custom),
            "control.dump"
        );
        assert_eq!(
            artifact_file_name(
                "shard:us_east",
                TargetBackend::Postgres,
                BackupFormat::Custom
            ),
            "shard-us_east.dump"
        );
        assert_eq!(
            artifact_file_name(
                "shard:us east",
                TargetBackend::Postgres,
                BackupFormat::Plain
            ),
            "shard-us_east.sql"
        );
        // A SQLite target's artifact is a database file, whatever `--format` said.
        assert_eq!(
            artifact_file_name("control", TargetBackend::Sqlite, BackupFormat::Plain),
            "control.sqlite"
        );
    }

    #[test]
    fn sanitize_component_replaces_unsafe_chars() {
        assert_eq!(sanitize_component("us-east_1.a"), "us-east_1.a");
        assert_eq!(sanitize_component("a/b:c"), "a_b_c");
        assert_eq!(sanitize_component(""), "unnamed");
        assert_eq!(sanitize_component("../etc"), ".._etc");
    }

    #[test]
    fn sanitize_component_rejects_traversal_components() {
        // A normal name is untouched...
        assert_eq!(sanitize_component("shard1"), "shard1");
        // ...but bare `.`, `..`, and empty (which would join to the current or
        // parent directory, escaping the backup root) fall back to "unnamed".
        assert_eq!(sanitize_component("."), "unnamed");
        assert_eq!(sanitize_component(".."), "unnamed");
        assert_eq!(sanitize_component(""), "unnamed");
    }

    #[test]
    fn build_targets_all_puts_control_first_then_shards() {
        let targets = build_targets(
            Some("postgres://localhost/app".to_owned()),
            vec![
                ("east".to_owned(), "postgres://localhost/east".to_owned()),
                ("west".to_owned(), "postgres://localhost/west".to_owned()),
            ],
            &TargetSelector::All,
        )
        .unwrap();
        let labels: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["control", "shard:east", "shard:west"]);
    }

    #[test]
    fn build_targets_shard_only_shape_is_allowed() {
        let targets = build_targets(
            None,
            vec![("east".to_owned(), "postgres://localhost/east".to_owned())],
            &TargetSelector::All,
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "shard:east");
    }

    #[test]
    fn build_targets_all_with_no_url_errors() {
        assert!(matches!(
            build_targets(None, vec![], &TargetSelector::All),
            Err(BackupError::NoUrl)
        ));
    }

    #[test]
    fn build_targets_control_only_ignores_shards() {
        let targets = build_targets(
            Some("postgres://localhost/app".to_owned()),
            vec![("east".to_owned(), "postgres://localhost/east".to_owned())],
            &TargetSelector::ControlOnly,
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "control");
    }

    #[test]
    fn build_targets_named_shard_selects_one() {
        let targets = build_targets(
            Some("postgres://localhost/app".to_owned()),
            vec![
                ("east".to_owned(), "postgres://localhost/east".to_owned()),
                ("west".to_owned(), "postgres://localhost/west".to_owned()),
            ],
            &TargetSelector::Shard("west".to_owned()),
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "shard:west");
        assert_eq!(targets[0].url, "postgres://localhost/west");
    }

    #[test]
    fn build_targets_unknown_shard_errors_with_known_list() {
        let err = build_targets(
            Some("postgres://localhost/app".to_owned()),
            vec![("east".to_owned(), "postgres://localhost/east".to_owned())],
            &TargetSelector::Shard("nope".to_owned()),
        )
        .unwrap_err();
        match err {
            BackupError::UnknownShard { name, known } => {
                assert_eq!(name, "nope");
                assert_eq!(known, vec!["east".to_owned()]);
            }
            other => panic!("expected UnknownShard, got {other:?}"),
        }
    }

    #[test]
    fn plan_pruning_keeps_newest_n() {
        let runs: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        // keep 2 => remove the 3 oldest (a, b, c).
        assert_eq!(
            plan_pruning(&runs, 2),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
        // keep >= len => remove nothing.
        assert!(plan_pruning(&runs, 5).is_empty());
        assert!(plan_pruning(&runs, 99).is_empty());
    }

    #[test]
    fn list_run_dirs_only_counts_dirs_with_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A real backup run: dir with a manifest.
        let good = root.join("20260710T040506Z");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join(MANIFEST_FILE), b"{}").unwrap();
        // An unrelated directory with no manifest must be ignored by pruning.
        std::fs::create_dir_all(root.join("notes")).unwrap();
        // A stray file is not a directory.
        std::fs::write(root.join("README.txt"), b"hi").unwrap();

        let dirs = list_run_dirs(root).unwrap();
        assert_eq!(dirs, vec!["20260710T040506Z".to_owned()]);
    }

    #[test]
    fn create_unique_run_dir_never_overwrites_same_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("dev");
        let base = "20260710T040506Z";

        // Two backups for the same profile in the same second must NOT collide.
        let first = create_unique_run_dir(&root, base).unwrap();
        // Mark the first run so we can prove it isn't overwritten/reused.
        std::fs::write(first.join(MANIFEST_FILE), b"{}").unwrap();

        let second = create_unique_run_dir(&root, base).unwrap();

        assert_ne!(first, second);
        assert_eq!(first, root.join(base));
        assert_eq!(second, root.join(format!("{base}-2")));
        // The first run's artifact is untouched (no silent overwrite).
        assert!(first.join(MANIFEST_FILE).is_file());
        // The suffixed name still sorts after the base, preserving retention order.
        assert!(base < format!("{base}-2").as_str());

        // A third collision keeps incrementing.
        let third = create_unique_run_dir(&root, base).unwrap();
        assert_eq!(third, root.join(format!("{base}-3")));
    }

    #[test]
    fn plan_pruning_never_removes_everything_on_keep_zero() {
        let runs: Vec<String> = ["a", "b"].iter().map(|s| (*s).to_owned()).collect();
        // keep 0 is clamped to 1 so a just-taken backup survives.
        assert_eq!(plan_pruning(&runs, 0), vec!["a".to_owned()]);
    }

    #[test]
    fn candidate_bin_dirs_orders_override_then_managed() {
        let dirs = candidate_bin_dirs(
            Some(PathBuf::from("/opt/pg/bin")),
            Some(PathBuf::from("/data/app/pg")),
        );
        assert_eq!(dirs[0], PathBuf::from("/opt/pg/bin"));
        // Managed install bin is `<parent>/postgresql/bin`.
        assert!(dirs.contains(&PathBuf::from("/data/app/postgresql/bin")));
    }

    #[test]
    fn candidate_bin_dirs_empty_without_env() {
        assert!(candidate_bin_dirs(None, None).is_empty());
    }

    #[test]
    fn resolve_tool_in_falls_back_to_bare_name() {
        // No candidate dir contains the tool => bare name for PATH lookup.
        assert_eq!(
            resolve_tool_in(&[PathBuf::from("/nonexistent")], "pg_dump"),
            PathBuf::from("pg_dump")
        );
    }

    #[test]
    fn resolve_tool_in_prefers_existing_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(exe_name("pg_dump"));
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_tool_in(std::slice::from_ref(&bin), "pg_dump"), exe);
    }

    #[test]
    fn managed_data_dir_candidate_locates_bundled_tools() {
        // Simulate the managed-daemon layout: the cluster's data dir sits beside
        // the extracted `postgresql/bin` bundle. In the daemon/cron path
        // AUTUMN_MANAGED_PG_DATA_DIR isn't set, so the ONLY way to find these
        // tools is by feeding the runtime-discovered data dir to the locator
        // (issue #1595). Placing fake `pg_dump`/`pg_restore` there must let the
        // locator resolve them by concrete path.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("pgdata");
        std::fs::create_dir_all(&data_dir).unwrap();
        // `candidate_bin_dirs` derives `<data_dir parent>/postgresql/bin`.
        let bundle_bin = tmp.path().join("postgresql").join("bin");
        std::fs::create_dir_all(&bundle_bin).unwrap();
        let pg_dump = bundle_bin.join(exe_name("pg_dump"));
        let pg_restore = bundle_bin.join(exe_name("pg_restore"));
        std::fs::write(&pg_dump, b"#!/bin/sh\n").unwrap();
        std::fs::write(&pg_restore, b"#!/bin/sh\n").unwrap();

        let dirs = candidate_bin_dirs(None, Some(data_dir));
        assert!(dirs.contains(&bundle_bin));
        assert_eq!(resolve_tool_in(&dirs, "pg_dump"), pg_dump);
        assert_eq!(resolve_tool_in(&dirs, "pg_restore"), pg_restore);
    }

    #[cfg(unix)]
    #[test]
    fn verify_artifact_resolves_pg_restore_from_managed_tools() {
        use std::os::unix::fs::PermissionsExt;
        // Regression for the verify path re-locating with a bare `PgTools::locate()`
        // instead of the threaded, managed-data-dir-aware tools (issue #1595). In
        // the daemon/cron layout the bundled `pg_restore` sits in
        // `<data_dir parent>/postgresql/bin` and is NOT on PATH, so it's reachable
        // only through the managed-data-dir-derived candidate dirs. If verify still
        // called `PgTools::locate()`, this archive would be reported as
        // `pg_restore` missing.
        let tmp = tempfile::tempdir().unwrap();
        let bundle_bin = tmp.path().join("postgresql").join("bin");
        std::fs::create_dir_all(&bundle_bin).unwrap();
        // A fake `pg_restore` that prints a one-entry TOC for `--list`; a real or
        // absent binary would reject this synthetic archive (or be missing), so a
        // successful verify proves the bundled binary is the one that ran.
        let pg_restore = bundle_bin.join("pg_restore");
        std::fs::write(
            &pg_restore,
            "#!/bin/sh\necho ';'\necho '215; 1259 16385 TABLE public posts app'\n",
        )
        .unwrap();
        std::fs::set_permissions(&pg_restore, std::fs::Permissions::from_mode(0o755)).unwrap();

        let data_dir = tmp.path().join("pgdata");
        std::fs::create_dir_all(&data_dir).unwrap();
        // Build the locator from the managed data dir exactly as `locate_with_extra`
        // does (via `candidate_bin_dirs`), but without touching process env so the
        // test is hermetic.
        let tools = PgTools::with_dirs(candidate_bin_dirs(None, Some(data_dir)));

        let archive = tmp.path().join("control.dump");
        std::fs::write(&archive, b"PGDMP synthetic archive").unwrap();

        verify_artifact(
            &archive,
            TargetBackend::Postgres,
            BackupFormat::Custom,
            "app",
            &tools,
        )
        .expect("verify resolves the bundled pg_restore and accepts the TOC");
    }

    #[test]
    fn toc_entry_count_ignores_comment_lines() {
        let listing = "\
;
; Archive created at 2026-07-10
;
215; 1259 16385 TABLE public posts app
216; 1259 16390 TABLE public comments app
";
        assert_eq!(toc_entry_count(listing), 2);
    }

    #[test]
    fn toc_entry_count_zero_for_header_only() {
        let listing = ";\n; only comments\n;\n";
        assert_eq!(toc_entry_count(listing), 0);
    }

    #[test]
    fn plain_dump_completion_marker_detected() {
        assert!(plain_dump_is_complete(
            "CREATE TABLE x();\n--\n-- PostgreSQL database dump complete\n--\n"
        ));
        assert!(!plain_dump_is_complete("CREATE TABLE x();\n-- truncated"));
    }

    #[test]
    fn verify_plain_dump_reads_only_tail() {
        let tmp = tempfile::tempdir().unwrap();

        // A complete dump whose body is far larger than the 1 KiB tail window
        // still verifies — the marker is on the final line and only the tail is
        // read.
        let complete = tmp.path().join("complete.sql");
        let mut body = "CREATE TABLE x();\n".repeat(4096);
        body.push_str("--\n-- PostgreSQL database dump complete\n--\n");
        std::fs::write(&complete, &body).unwrap();
        assert!(verify_plain_dump(&complete, "db").is_ok());

        // A dump missing the marker is flagged as an integrity failure, even
        // when the body far exceeds the tail window.
        let truncated = tmp.path().join("truncated.sql");
        std::fs::write(&truncated, "CREATE TABLE x();\n".repeat(4096)).unwrap();
        assert!(matches!(
            verify_plain_dump(&truncated, "db"),
            Err(BackupError::IntegrityFailed { .. })
        ));

        // A tiny (sub-1 KiB) complete dump verifies: the read window clamps to
        // the file size instead of seeking before the start.
        let tiny = tmp.path().join("tiny.sql");
        std::fs::write(&tiny, "-- PostgreSQL database dump complete\n").unwrap();
        assert!(verify_plain_dump(&tiny, "db").is_ok());

        // A 0-byte dump can't hold the marker: truncated/incomplete.
        let empty = tmp.path().join("empty.sql");
        std::fs::write(&empty, b"").unwrap();
        assert!(matches!(
            verify_plain_dump(&empty, "db"),
            Err(BackupError::IntegrityFailed { .. })
        ));
    }

    #[test]
    fn pg_dump_args_bake_clean_into_plain_only() {
        let to_str = |args: &[std::ffi::OsString]| -> Vec<String> {
            args.iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect()
        };

        // Plain dumps carry `--clean --if-exists` so the SQL includes
        // `DROP ... IF EXISTS` and a `psql` restore is idempotent on a populated DB.
        let plain = pg_dump_args(
            BackupFormat::Plain,
            Path::new("/backups/dev/run/control.sql"),
            "postgres://user@db.example.com/my_app",
        );
        let plain = to_str(&plain);
        assert!(plain.contains(&"--clean".to_owned()));
        assert!(plain.contains(&"--if-exists".to_owned()));
        assert!(
            plain
                .windows(2)
                .any(|w| w[0] == "--format" && w[1] == "plain")
        );
        // The dbname URL and file are still passed.
        assert!(
            plain
                .windows(2)
                .any(|w| w[0] == "--dbname" && w[1] == "postgres://user@db.example.com/my_app")
        );

        // Custom dumps deliberately DON'T clean at dump time — `pg_restore
        // --clean --if-exists` handles that at restore time.
        let custom = pg_dump_args(
            BackupFormat::Custom,
            Path::new("/backups/dev/run/control.dump"),
            "postgres://user@db.example.com/my_app",
        );
        let custom = to_str(&custom);
        assert!(!custom.contains(&"--clean".to_owned()));
        assert!(!custom.contains(&"--if-exists".to_owned()));
        assert!(
            custom
                .windows(2)
                .any(|w| w[0] == "--format" && w[1] == "custom")
        );
    }

    #[test]
    fn resolve_control_url_prefers_config_over_managed_pg_fallback() {
        // When config/env resolves a URL, the managed-pg fallback must NOT be
        // consulted (precedence + lazy evaluation).
        let mut fallback_called = false;
        let got = resolve_control_url(Some("postgres://cfg/db".to_owned()), || {
            fallback_called = true;
            Some("postgres://managed/db".to_owned())
        });
        assert_eq!(got.as_deref(), Some("postgres://cfg/db"));
        assert!(
            !fallback_called,
            "managed-pg fallback must not run when config/env resolves a URL"
        );

        // When config/env yields nothing, the managed-pg published URL fills in.
        let got = resolve_control_url(None, || Some("postgres://managed/db".to_owned()));
        assert_eq!(got.as_deref(), Some("postgres://managed/db"));

        // Neither source available => still None (surfaces as NoUrl upstream).
        let got = resolve_control_url(None, || None);
        assert_eq!(got, None);
    }

    // ── SQLite backend dispatch (issue #1909) ──────────────────────────────

    #[test]
    fn target_backend_detects_sqlite_and_defaults_everything_else_to_postgres() {
        for sqlite in ["sqlite:///var/lib/app.db", "sqlite:app.db", "file:app.db"] {
            assert_eq!(
                TargetBackend::detect(sqlite),
                TargetBackend::Sqlite,
                "{sqlite}"
            );
        }
        for postgres in [
            "postgres://u:p@h:5432/app",
            "postgresql://h/app",
            "host=db user=app sslmode=require",
            // An unrecognized target keeps the historical Postgres path rather
            // than being guessed at, mirroring `autumn migrate`'s rule.
            "/var/lib/app.db",
            "",
        ] {
            assert_eq!(
                TargetBackend::detect(postgres),
                TargetBackend::Postgres,
                "{postgres:?}"
            );
        }
    }

    #[test]
    fn manifest_backend_defaults_to_postgres_for_pre_1909_artifacts() {
        // A manifest written before #1909 has no `backend` field, so serde fills
        // an empty string; it must restore through the Postgres path exactly as
        // it always did.
        let legacy: Manifest = serde_json::from_str(
            r#"{"autumn_version":"0.6.0","created_at":"2026-07-10T04:05:06+00:00",
                "profile":"prod","format":"custom",
                "targets":[{"label":"control","file":"control.dump","database":"app"}]}"#,
        )
        .expect("a pre-#1909 manifest must still parse");
        assert_eq!(legacy.targets[0].backend, "");
        assert_eq!(
            TargetBackend::from_manifest(&legacy.targets[0].backend),
            TargetBackend::Postgres
        );
        assert_eq!(
            TargetBackend::from_manifest("sqlite"),
            TargetBackend::Sqlite
        );
        assert_eq!(
            TargetBackend::from_manifest("SQLite"),
            TargetBackend::Sqlite
        );
    }

    /// The mismatch guard must refuse before anything is touched, and must name
    /// the backends without ever quoting the URL (which carries a password).
    #[test]
    fn restoring_an_artifact_into_the_wrong_backend_is_refused_up_front() {
        let entry = |backend: &str| ManifestTarget {
            label: "control".to_owned(),
            file: "control.sqlite".to_owned(),
            database: "app".to_owned(),
            backend: backend.to_owned(),
        };
        let url = "postgres://app:hunter2@db.example.com/app";
        assert_ne!(
            TargetBackend::from_manifest(&entry("sqlite").backend),
            TargetBackend::detect(url),
            "the guard's premise: a SQLite artifact against a Postgres URL"
        );
        // …and a legacy manifest (no backend field) against a Postgres URL must
        // still agree, so the guard can never refuse an existing restore.
        assert_eq!(
            TargetBackend::from_manifest(&entry("").backend),
            TargetBackend::detect(url)
        );
        // A legacy manifest against a SQLite URL is a real mismatch too.
        assert_ne!(
            TargetBackend::from_manifest(&entry("").backend),
            TargetBackend::detect("sqlite://app.db")
        );
    }

    #[test]
    fn build_targets_classifies_a_sqlite_control_url() {
        let targets = build_targets(
            Some("sqlite://app.db".to_owned()),
            Vec::new(),
            &TargetSelector::All,
        )
        .expect("a lone sqlite control resolves");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].backend, TargetBackend::Sqlite);
    }

    #[test]
    fn run_format_note_reports_the_snapshot_a_sqlite_run_actually_took() {
        let sqlite = ResolvedTarget {
            label: "control".to_owned(),
            url: "sqlite://app.db".to_owned(),
            backend: TargetBackend::Sqlite,
        };
        let postgres = ResolvedTarget {
            label: "shard:east".to_owned(),
            url: "postgres://h/east".to_owned(),
            backend: TargetBackend::Postgres,
        };
        // `--format` grades Postgres artifacts only, so an all-SQLite run must not
        // claim a `custom` format it ignored.
        assert_eq!(
            run_format_note(std::slice::from_ref(&sqlite), BackupFormat::Custom),
            "SQLite snapshot"
        );
        assert_eq!(
            run_format_note(std::slice::from_ref(&postgres), BackupFormat::Plain),
            "plain format"
        );
        assert_eq!(
            run_format_note(&[postgres, sqlite], BackupFormat::Custom),
            "custom format + SQLite snapshot"
        );
    }

    /// The whole `SQLite` backup → restore contract through `backup_into` and
    /// `run_restore_one`, with **no Postgres client tools involved**: a `SQLite` app
    /// on the zero-ops tier has none, so `pg_dump` is passed as `None` here exactly
    /// as `backup` resolves it.
    #[test]
    fn sqlite_target_round_trips_through_backup_into_and_restore() {
        use diesel::connection::SimpleConnection as _;
        use diesel::{Connection as _, RunQueryDsl as _, sql_query};

        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("app.db");
        let url = format!("sqlite://{}", db.display());
        {
            let mut conn = diesel::SqliteConnection::establish(&db.to_string_lossy())
                .expect("establish sqlite");
            conn.batch_execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL); \
                 INSERT INTO t (v) VALUES ('a'), ('b');",
            )
            .expect("seed");
        }

        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).expect("mkdir");
        let targets = vec![ResolvedTarget {
            label: "control".to_owned(),
            url: url.clone(),
            backend: TargetBackend::Sqlite,
        }];
        let tools = PgTools::with_dirs(Vec::new());
        backup_into(
            &run_dir,
            &targets,
            BackupFormat::Custom,
            None,
            &tools,
            "prod",
        )
        .expect("a SQLite backup needs no pg_dump");

        let artifact = run_dir.join("control.sqlite");
        assert!(
            artifact.is_file(),
            "the artifact is a .sqlite database file"
        );
        let manifest = read_manifest(&run_dir).expect("manifest");
        assert_eq!(manifest.targets[0].backend, "sqlite");
        assert_eq!(manifest.targets[0].file, "control.sqlite");

        // Simulate data loss, then restore.
        {
            let mut conn = diesel::SqliteConnection::establish(&db.to_string_lossy())
                .expect("establish sqlite");
            conn.batch_execute("DELETE FROM t;").expect("delete");
        }
        run_restore_one(
            &tools,
            &url,
            &artifact,
            TargetBackend::Sqlite,
            BackupFormat::Custom,
            "app",
        )
        .expect("a SQLite restore needs no pg_restore");

        let mut conn =
            diesel::SqliteConnection::establish(&db.to_string_lossy()).expect("establish sqlite");
        let rows: Vec<Count> = sql_query("SELECT COUNT(*) AS n FROM t")
            .load(&mut conn)
            .expect("count");
        assert_eq!(rows[0].n, 2, "the restore must bring the rows back");
    }

    #[test]
    fn infer_format_from_extension() {
        assert_eq!(
            infer_artifact_kind(Path::new("control.dump")),
            Some((TargetBackend::Postgres, BackupFormat::Custom))
        );
        assert_eq!(
            infer_artifact_kind(Path::new("control.sql")),
            Some((TargetBackend::Postgres, BackupFormat::Plain))
        );
        // A bare `.sqlite` artifact restores through the SQLite path (#1909).
        assert_eq!(
            infer_artifact_kind(Path::new("control.sqlite")),
            Some((TargetBackend::Sqlite, BackupFormat::Custom))
        );
        assert_eq!(infer_artifact_kind(Path::new("control.txt")), None);
    }

    #[test]
    fn parsed_db_name_is_credential_safe() {
        let name = parsed_db_name("postgres://user:hunter2@db.example.com:6543/my_app");
        assert_eq!(name, "my_app");
        // Never leaks credentials.
        assert!(!name.contains("hunter2"));
        assert_eq!(parsed_db_name("not a url"), "(database)");
    }

    #[test]
    fn split_password_moves_secret_out_of_url() {
        let (safe, pw) = split_password("postgres://user:hunter2@db.example.com:6543/my_app");
        assert_eq!(pw.as_deref(), Some("hunter2"));
        // The returned URL keeps everything but the password.
        assert!(!safe.contains("hunter2"));
        assert!(safe.contains("user@db.example.com"));
        assert!(safe.contains("/my_app"));
    }

    #[test]
    fn split_password_percent_decodes_for_pgpassword() {
        // Raw password contains `@`, `:`, `/`, and a space — all of which MUST be
        // percent-encoded in the URL userinfo. libpq consumes PGPASSWORD as a
        // literal (no percent-decoding), so `split_password` must hand back the
        // DECODED bytes or authentication fails.
        let raw = "p@ss:w/ord x";
        let encoded = "p%40ss%3Aw%2Ford%20x";
        let url = format!("postgres://user:{encoded}@db.example.com:6543/my_app");

        let (safe, pw) = split_password(&url);
        assert_eq!(pw.as_deref(), Some(raw));

        // The password (encoded or decoded) never remains in the argv URL.
        assert!(!safe.contains(encoded));
        assert!(!safe.contains(raw));
        // The username is preserved (libpq decodes it from the URI itself).
        assert!(safe.contains("user@db.example.com"));
        assert!(safe.contains("/my_app"));
        // The password-free URL is still a VALID URL for `--dbname`.
        assert!(url::Url::parse(&safe).is_ok());
    }

    #[test]
    fn split_password_passthrough_when_absent_or_unparseable() {
        let (safe, pw) = split_password("postgres://user@localhost/app");
        assert_eq!(pw, None);
        assert_eq!(safe, "postgres://user@localhost/app");
        // A non-URL that isn't keyword/value form (no `keyword=`) degrades to
        // passing the string through unchanged.
        let (safe, pw) = split_password("not a url");
        assert_eq!(pw, None);
        assert_eq!(safe, "not a url");
    }

    #[test]
    fn split_password_strips_keyword_value_password() {
        // libpq keyword/value form (which Autumn's config validation accepts):
        // the password must move to PGPASSWORD, never staying on argv where `ps`
        // could read it.
        let (safe, pw) = split_password("host=db user=app password=secret dbname=app");
        assert_eq!(pw.as_deref(), Some("secret"));
        assert!(!safe.contains("password"));
        assert!(!safe.contains("secret"));
        // The remaining keywords are preserved for `--dbname`.
        assert!(safe.contains("host=db"));
        assert!(safe.contains("user=app"));
        assert!(safe.contains("dbname=app"));
        // And the stripped connstring still round-trips through the parser.
        let reparsed = parse_libpq_kv(&safe).unwrap();
        assert!(reparsed.iter().all(|(k, _)| k != "password"));
    }

    #[test]
    fn split_password_handles_single_quoted_keyword_value() {
        // A single-quoted value carries spaces and metacharacters; unquoting must
        // yield the literal password for PGPASSWORD.
        let (safe, pw) = split_password("host=db password='p a$s' dbname=app");
        assert_eq!(pw.as_deref(), Some("p a$s"));
        assert!(!safe.contains("p a$s"));
        assert!(!safe.contains("password"));
        assert!(safe.contains("host=db"));
        assert!(safe.contains("dbname=app"));
    }

    #[test]
    fn split_password_handles_escaped_quote_in_keyword_value() {
        // Backslash escapes inside a quoted value: `\'` is a literal quote and
        // `\\` a literal backslash.
        let (_safe, pw) = split_password(r"host=db password='a\'b\\c'");
        assert_eq!(pw.as_deref(), Some(r"a'b\c"));
    }

    #[test]
    fn split_password_keyword_value_without_password_passes_through() {
        // No `password=` token => the connstring is returned byte-for-byte with
        // no PGPASSWORD, so nothing changes for password-less managed clusters.
        let input = "host=db user=app dbname=app";
        let (safe, pw) = split_password(input);
        assert_eq!(pw, None);
        assert_eq!(safe, input);
    }

    #[test]
    fn restore_plan_select_filters_to_named_shard() {
        let plan = RestorePlan {
            dir: PathBuf::from("/x"),
            format: BackupFormat::Custom,
            entries: vec![
                ManifestTarget {
                    label: "control".to_owned(),
                    file: "control.dump".to_owned(),
                    database: "app".to_owned(),
                    backend: "postgres".to_owned(),
                },
                ManifestTarget {
                    label: "shard:east".to_owned(),
                    file: "shard-east.dump".to_owned(),
                    database: "east".to_owned(),
                    backend: "postgres".to_owned(),
                },
            ],
        };
        let all = plan.select(None).unwrap();
        assert_eq!(all.len(), 2);
        let one = plan.select(Some("east")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].label, "shard:east");
        assert!(matches!(
            plan.select(Some("nope")),
            Err(BackupError::UnknownShard { .. })
        ));
    }

    #[test]
    fn single_file_target_label_infers_from_filename_convention() {
        // control.* -> control.
        assert_eq!(
            single_file_target_label("control.dump", None).unwrap(),
            "control"
        );
        assert_eq!(
            single_file_target_label("control.sql", None).unwrap(),
            "control"
        );
        // shard-<name>.* -> that shard (inference is authoritative: a bare shard
        // dump is NEVER silently treated as control).
        assert_eq!(
            single_file_target_label("shard-east.dump", None).unwrap(),
            "shard:east"
        );
        // An explicit --shard wins over the filename.
        assert_eq!(
            single_file_target_label("control.dump", Some("west")).unwrap(),
            "shard:west"
        );
        // A shard-shaped name with no recoverable shard name errors rather than
        // silently defaulting to control.
        assert!(matches!(
            single_file_target_label("shard-.dump", None),
            Err(BackupError::BadArtifact { .. })
        ));
        // A user-renamed file (not produced by the backup writer) falls back to
        // control, preserving the historical single-file behavior without a shard
        // cross-target hazard.
        assert_eq!(
            single_file_target_label("mybackup.dump", None).unwrap(),
            "control"
        );
    }

    #[test]
    fn discover_bare_shard_file_resolves_to_shard_not_control() {
        // Regression: restoring `shard-east.dump` via the single-file path used to
        // label it `control` unconditionally, silently restoring shard data into
        // the control database.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shard-east.dump");
        std::fs::write(&path, b"x").unwrap();

        let plan = RestorePlan::discover(&path, None).unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].label, "shard:east");
        // The inferred label still selects cleanly with a matching --shard.
        assert_eq!(plan.select(Some("east")).unwrap()[0].label, "shard:east");
    }

    #[test]
    fn discover_bare_control_file_resolves_to_control() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("control.dump");
        std::fs::write(&path, b"x").unwrap();

        let plan = RestorePlan::discover(&path, None).unwrap();
        assert_eq!(plan.entries[0].label, "control");
    }

    #[test]
    fn discover_bare_file_honors_explicit_shard_flag() {
        let tmp = tempfile::tempdir().unwrap();
        // Even a control-named bare file is redirected when the operator explicitly
        // names a shard target.
        let path = tmp.path().join("control.dump");
        std::fs::write(&path, b"x").unwrap();

        let plan = RestorePlan::discover(&path, Some("east")).unwrap();
        assert_eq!(plan.entries[0].label, "shard:east");
    }

    #[test]
    fn manifest_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            autumn_version: "0.6.0".to_owned(),
            created_at: "2026-07-10T04:05:06+00:00".to_owned(),
            profile: "dev".to_owned(),
            format: "custom".to_owned(),
            targets: vec![ManifestTarget {
                label: "control".to_owned(),
                file: "control.dump".to_owned(),
                database: "app".to_owned(),
                backend: "postgres".to_owned(),
            }],
        };
        write_manifest(tmp.path(), &manifest).unwrap();
        let read = read_manifest(tmp.path()).unwrap();
        assert_eq!(read, manifest);
    }

    #[test]
    fn errors_are_credential_safe() {
        let e = BackupError::ProductionRefused {
            profile: "prod".to_owned(),
        };
        let s = e.to_string();
        assert!(s.contains("prod"));
        assert!(!s.contains("postgres://"));
    }

    // ─── Offsite (issue #1619) ───────────────────────────────────────────────

    #[test]
    fn offsite_object_key_joins_and_skips_empty_prefix() {
        assert_eq!(
            offsite_object_key("db", "prod", "20260710T040506Z", "control.dump"),
            "db/prod/20260710T040506Z/control.dump"
        );
        // Empty prefix => key rooted at the bucket, no leading slash.
        assert_eq!(
            offsite_object_key("", "prod", "20260710T040506Z", "manifest.json"),
            "prod/20260710T040506Z/manifest.json"
        );
    }

    #[test]
    fn normalize_offsite_prefix_trims_slashes() {
        assert_eq!(normalize_offsite_prefix(Some("/db/backups/")), "db/backups");
        assert_eq!(normalize_offsite_prefix(Some("  db  ")), "db");
        assert_eq!(normalize_offsite_prefix(None), "");
        assert_eq!(normalize_offsite_prefix(Some("")), "");
    }

    #[test]
    fn offsite_profile_and_run_prefixes_are_scoped() {
        assert_eq!(offsite_profile_prefix("db", "prod"), "db/prod/");
        assert_eq!(offsite_profile_prefix("", "prod"), "prod/");
        assert_eq!(
            offsite_run_prefix("db", "prod", "20260710T040506Z"),
            "db/prod/20260710T040506Z/"
        );
    }

    #[test]
    fn offsite_key_prefix_is_canonical_across_profile_spellings() {
        // P2 #22: alias/case profile spellings must resolve to the SAME remote key
        // prefix, so upload (`--profile production`), `db offsite list --profile
        // prod`, and `restore offsite:prod/latest` all address `db/prod/…`.
        for spelling in ["prod", "production", "PROD", "Production", "  prod  "] {
            let p = migrate::canonical_profile(spelling);
            assert_eq!(offsite_profile_prefix("db", &p), "db/prod/", "{spelling:?}");
            assert_eq!(
                offsite_run_prefix("db", &p, "r"),
                "db/prod/r/",
                "{spelling:?}"
            );
            assert_eq!(
                offsite_object_key("db", &p, "r", "manifest.json"),
                "db/prod/r/manifest.json",
                "{spelling:?}"
            );
        }
        // `prod` and `production` yield the identical prefix.
        assert_eq!(
            offsite_profile_prefix("db", &migrate::canonical_profile("prod")),
            offsite_profile_prefix("db", &migrate::canonical_profile("production")),
        );
        // dev aliases collapse the same way.
        assert_eq!(
            offsite_profile_prefix("db", &migrate::canonical_profile("development")),
            "db/dev/"
        );
    }

    #[test]
    fn destinations_conflict_requires_same_bucket_and_endpoint() {
        // Same bucket + same endpoint => conflict.
        assert!(destinations_conflict(
            "shared",
            Some("https://s3.example.test"),
            Some("shared"),
            Some("https://s3.example.test/"),
        ));
        // Same bucket but different endpoint => distinct.
        assert!(!destinations_conflict(
            "shared",
            Some("https://offsite.test"),
            Some("shared"),
            Some("https://app.test"),
        ));
        // Different bucket => distinct.
        assert!(!destinations_conflict(
            "offsite",
            Some("https://s3.example.test"),
            Some("app"),
            Some("https://s3.example.test"),
        ));
        // App storage not configured (no bucket) => never a conflict.
        assert!(!destinations_conflict("offsite", None, None, None));
        // Both AWS default endpoint (None) + same bucket => conflict.
        assert!(destinations_conflict("shared", None, Some("shared"), None));
    }

    #[test]
    fn destinations_conflict_normalizes_ports_and_aws_defaults() {
        // `:443` vs bare host on https must compare equal (default port dropped).
        assert!(destinations_conflict(
            "shared",
            Some("https://minio.example:443"),
            Some("shared"),
            Some("https://minio.example"),
        ));
        // `:80` vs bare host on http likewise.
        assert!(destinations_conflict(
            "shared",
            Some("http://gw:80"),
            Some("shared"),
            Some("http://gw"),
        ));
        // Offsite None (AWS default) vs app spelled as the explicit regional AWS
        // URL, same bucket => still a shared bucket (globally-unique name).
        assert!(destinations_conflict(
            "shared",
            None,
            Some("shared"),
            Some("https://s3.us-east-1.amazonaws.com"),
        ));
        // ...and the virtual-hosted AWS spelling too.
        assert!(destinations_conflict(
            "shared",
            None,
            Some("shared"),
            Some("https://shared.s3.us-east-1.amazonaws.com"),
        ));
        // A non-default port genuinely differs => distinct.
        assert!(!destinations_conflict(
            "shared",
            Some("https://minio.example:9000"),
            Some("shared"),
            Some("https://minio.example"),
        ));
        // Virtual-hosted `{bucket}.` prefix vs path-style same host => equal.
        assert!(destinations_conflict(
            "shared",
            Some("https://shared.minio.example"),
            Some("shared"),
            Some("https://minio.example"),
        ));
    }

    #[test]
    fn shared_bucket_refusal_message_is_credential_safe() {
        let e = BackupError::SharedBucketRefused {
            bucket: "shared".to_owned(),
        };
        let s = e.to_string();
        assert!(s.contains("shared"));
        assert!(s.contains("allow_shared_bucket"));
        assert!(!s.contains("secret"));
    }

    #[test]
    fn missing_credential_env_names_only_the_variable() {
        let e = BackupError::MissingCredentialEnv {
            var: "OFFSITE_SECRET".to_owned(),
        };
        let s = e.to_string();
        assert!(s.contains("OFFSITE_SECRET"));
        // Names the variable, never a value.
        assert!(!s.contains("hunter2"));
    }

    #[test]
    fn upload_failed_reports_split_outcome() {
        let e = BackupError::OffsiteUploadFailed {
            local_path: "/backups/prod/20260710T040506Z".to_owned(),
            detail: "S3 put returned HTTP 403 (AccessDenied)".to_owned(),
        };
        let s = e.to_string();
        assert!(s.contains("Local backup OK at /backups/prod/20260710T040506Z"));
        assert!(s.contains("OFFSITE UPLOAD FAILED"));
        assert!(s.contains("intact"));
    }

    // ── #1743: failed offsite upload raises an operator alert ─────────────────

    #[derive(Default)]
    struct CapturingChannel {
        received: Arc<std::sync::Mutex<Vec<Alert>>>,
    }

    impl AlertChannel for CapturingChannel {
        fn name(&self) -> &'static str {
            "capturing"
        }
        fn deliver<'a>(&'a self, alert: &'a Alert) -> autumn_web::alerts::AlertDeliveryFuture<'a> {
            let received = Arc::clone(&self.received);
            let cloned = alert.clone();
            Box::pin(async move {
                received.lock().expect("lock").push(cloned);
                Ok(())
            })
        }
    }

    #[test]
    fn build_offsite_upload_alert_is_scheduled_task_failure() {
        let alert = build_offsite_upload_alert(
            "/backups/prod/20260710T040506Z",
            "S3 put returned HTTP 403 (AccessDenied)",
        );
        assert_eq!(alert.condition, AlertCondition::ScheduledTaskFailure);
        assert_eq!(alert.title, "Offsite backup upload failed");
        assert_eq!(alert.summary, "S3 put returned HTTP 403 (AccessDenied)");
        assert_eq!(
            alert.dedup_key,
            "scheduled_task_failure:db-backup-offsite-upload"
        );
    }

    #[test]
    fn deliver_offsite_upload_alert_reaches_configured_channel() {
        // Failure-injection: the upload-failure path delivers a
        // ScheduledTaskFailure alert to every configured channel. A capturing
        // mock channel records what a failed upload would raise.
        let capture = Arc::new(std::sync::Mutex::new(Vec::new()));
        let channel = Arc::new(CapturingChannel {
            received: Arc::clone(&capture),
        });
        let channels: Vec<Arc<dyn AlertChannel>> = vec![channel];

        deliver_offsite_upload_alert(
            &channels,
            "/backups/prod/20260710T040506Z",
            "S3 put returned HTTP 403 (AccessDenied)",
        );

        let delivered = {
            let received = capture.lock().expect("lock");
            received.clone()
        };
        assert_eq!(delivered.len(), 1, "exactly one alert must be delivered");
        let alert = &delivered[0];
        assert_eq!(alert.condition, AlertCondition::ScheduledTaskFailure);
        assert_eq!(alert.title, "Offsite backup upload failed");
        assert_eq!(alert.summary, "S3 put returned HTTP 403 (AccessDenied)");
    }

    #[test]
    fn deliver_offsite_upload_alert_is_noop_without_channels() {
        // Interactive case (no [alerts] configured → no channels): must be a
        // no-op so behavior is unchanged. No panic, nothing delivered.
        deliver_offsite_upload_alert(&[], "/backups/prod/run", "boom");
    }

    #[test]
    fn remote_run_ids_extracts_unique_sorted_ids() {
        let list_prefix = "db/prod/";
        let objects = vec![
            s3::S3Object {
                key: "db/prod/20260710T010101Z/control.dump".to_owned(),
                size: 1,
            },
            s3::S3Object {
                key: "db/prod/20260710T010101Z/manifest.json".to_owned(),
                size: 1,
            },
            s3::S3Object {
                key: "db/prod/20260709T010101Z/control.dump".to_owned(),
                size: 1,
            },
        ];
        assert_eq!(
            remote_run_ids(&objects, list_prefix),
            vec!["20260709T010101Z".to_owned(), "20260710T010101Z".to_owned()]
        );
    }

    #[test]
    fn local_backup_does_not_validate_incomplete_offsite() {
        // P2 #1: an incomplete [backup.offsite] (no bucket) must NOT block a plain
        // local `autumn db backup`. The strict bucket check lives in resolve(),
        // which the backup path only calls when an upload is actually requested.
        let offsite = autumn_web::config::OffsiteBackupConfig::default();
        assert!(offsite.s3.bucket.is_none(), "precondition: no bucket set");
        let loaded = LoadedOffsite {
            offsite,
            app_storage_bucket: None,
            app_storage_endpoint: None,
            profile: "test".to_owned(),
        };
        // The strict validation is deferred to resolve() (only reached when
        // uploading) — proving a local-only backup never hits it.
        assert!(matches!(
            loaded.resolve(),
            Err(BackupError::OffsiteConfig { .. })
        ));
    }

    #[test]
    fn auto_upload_probe_precedence_and_repro() {
        // P2 #8: the upload decision reads ONLY `[backup.offsite].auto_upload`
        // from env / .env / merged TOML — never a full config load / credential
        // decryption. Pure `auto_upload_from_sources` proves the precedence.
        let no_env = |_: &str| None;

        // (a) REPRO: no offsite section, no env => false => NO upload, and (by
        // construction) the decision needed no AutumnConfig load / cred decrypt.
        assert!(!auto_upload_from_sources(no_env, None));
        let empty: toml::Table = toml::Table::new();
        assert!(!auto_upload_from_sources(no_env, Some(&empty)));

        // (b) auto_upload=true via TOML [backup.offsite] triggers upload.
        let toml_true: toml::Table =
            toml::from_str("[backup.offsite]\nauto_upload = true\n").unwrap();
        assert!(auto_upload_from_sources(no_env, Some(&toml_true)));

        // (b) auto_upload=true via env (real env or .env.<profile>) triggers it,
        // even with no TOML.
        let env_true =
            |k: &str| (k == "AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD").then(|| "true".to_owned());
        assert!(auto_upload_from_sources(env_true, None));

        // Env wins over TOML: env "false" overrides TOML true.
        let env_false =
            |k: &str| (k == "AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD").then(|| "false".to_owned());
        assert!(!auto_upload_from_sources(env_false, Some(&toml_true)));

        // A non-boolean env value is ignored, falling back to TOML.
        let env_junk =
            |k: &str| (k == "AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD").then(|| "yesplease".to_owned());
        assert!(auto_upload_from_sources(env_junk, Some(&toml_true)));
    }

    #[test]
    fn offsite_resolves_from_config_dir_table_not_cwd() {
        // P2 #15: an installed/daemon launch keeps autumn.toml in the config dir
        // (a different CWD). Reading from that dir — as the offsite loaders now do
        // via read_autumn_toml_table_with_profile_from_config_dir — must surface
        // both auto_upload and the offsite destination. Proven hermetically by
        // reading a temp config dir directly (this test's CWD has no such file).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[backup.offsite]\n\
             auto_upload = true\n\
             prefix = \"db\"\n\
             [backup.offsite.s3]\n\
             bucket = \"offsite-bucket\"\n\
             region = \"us-east-1\"\n\
             endpoint = \"https://minio.example:9000\"\n\
             force_path_style = true\n\
             access_key_id_env = \"OFFSITE_KEY\"\n\
             secret_access_key_env = \"OFFSITE_SECRET\"\n",
        )
        .unwrap();
        let table = migrate::read_autumn_toml_table_with_profile_in(tmp.path(), Some("dev"));
        // auto_upload is read from the config-dir table (env has nothing).
        assert!(auto_upload_from_sources(|_| None, table.as_ref()));
        // And the offsite destination resolves from that same table (no CWD file).
        let env = autumn_web::config::MockEnv::new();
        let loaded = resolve_loaded_offsite(table.as_ref(), &env, "dev").unwrap();
        assert!(
            loaded.is_some(),
            "offsite must resolve from the config-dir autumn.toml"
        );
    }

    #[test]
    fn resolved_offsite_carries_target_profile_for_credentials() {
        // P2 #6: the target profile threads through resolve() into
        // ResolvedOffsite so credential resolution reads the SAME
        // `.env.<profile>` overlay the config was loaded under.
        let mut offsite = autumn_web::config::OffsiteBackupConfig::default();
        offsite.s3.bucket = Some("offsite-bucket".to_owned());
        offsite.s3.access_key_id_env = Some("OFFSITE_KEY".to_owned());
        offsite.s3.secret_access_key_env = Some("OFFSITE_SECRET".to_owned());
        let loaded = LoadedOffsite {
            offsite,
            app_storage_bucket: None,
            app_storage_endpoint: None,
            profile: "test".to_owned(),
        };
        let resolved = loaded.resolve().expect("bucket set, resolves");
        assert_eq!(resolved.profile, "test");
    }

    #[test]
    fn offsite_resolves_without_app_credential_store() {
        // P2 #9: offsite destination resolves from TOML + env WITHOUT a full
        // AutumnConfig::load — so NO credential decryption / AUTUMN_MASTER_KEY is
        // needed. On current code this path went through load_with_env, which
        // would fail on a DR box whose config/credentials/<p>.toml.enc can't be
        // decrypted. `resolve_loaded_offsite` reads only the offsite + storage.s3
        // config, never the credential store.
        let table: toml::Table = toml::from_str(
            "[backup.offsite]\nprefix = \"db\"\n\
             [backup.offsite.s3]\nbucket = \"dr-bucket\"\nregion = \"us-east-1\"\n\
             endpoint = \"https://minio.example:9000\"\nforce_path_style = true\n\
             access_key_id_env = \"OFFSITE_KEY\"\nsecret_access_key_env = \"OFFSITE_SECRET\"\n",
        )
        .unwrap();
        // Real env supplies the S3 credential VALUES; crucially there is NO
        // AUTUMN_MASTER_KEY and no attempt to read a credential store.
        let env = autumn_web::config::MockEnv::new()
            .with("OFFSITE_KEY", "AKIA_DR")
            .with("OFFSITE_SECRET", "shh");
        let loaded = resolve_loaded_offsite(Some(&table), &env, "prod")
            .expect("resolves without a credential store")
            .expect("offsite section present");
        let resolved = loaded.resolve().expect("bucket set");
        assert_eq!(resolved.s3.bucket, "dr-bucket");
        assert_eq!(resolved.profile, "prod");

        // Env-only configuration (no TOML at all) also resolves.
        let env2 = autumn_web::config::MockEnv::new()
            .with("AUTUMN_BACKUP__OFFSITE__S3__BUCKET", "env-bucket");
        let loaded2 = resolve_loaded_offsite(None, &env2, "prod")
            .unwrap()
            .expect("materialized from env");
        assert_eq!(loaded2.offsite.s3.bucket.as_deref(), Some("env-bucket"));
    }

    #[test]
    fn shared_bucket_guard_only_applies_when_storage_backend_is_s3() {
        // P2 #16: a leftover `[storage.s3]` bucket that equals the offsite bucket
        // must NOT trip the shared-bucket guard unless the app storage backend is
        // actually S3. On backend=local/disabled the bucket is inert.
        let env = autumn_web::config::MockEnv::new();
        let make = |backend: &str| -> LoadedOffsite {
            let table: toml::Table = toml::from_str(&format!(
                "[storage]\nbackend = \"{backend}\"\n\
                 [storage.s3]\nbucket = \"shared\"\nendpoint = \"https://minio.example:9000\"\n\
                 [backup.offsite]\nprefix = \"db\"\n\
                 [backup.offsite.s3]\nbucket = \"shared\"\nregion = \"us-east-1\"\n\
                 endpoint = \"https://minio.example:9000\"\nforce_path_style = true\n"
            ))
            .unwrap();
            resolve_loaded_offsite(Some(&table), &env, "prod")
                .expect("resolves")
                .expect("offsite section present")
        };

        // backend=local: the s3 bucket is NOT the app storage destination, so the
        // guard input is empty and destinations_conflict cannot fire.
        let local = make("local");
        assert_eq!(local.app_storage_bucket, None);
        assert!(!destinations_conflict(
            "shared",
            Some("https://minio.example:9000"),
            local.app_storage_bucket.as_deref(),
            local.app_storage_endpoint.as_deref(),
        ));

        // backend=disabled: same — inert `[storage.s3]`.
        assert_eq!(make("disabled").app_storage_bucket, None);

        // backend=s3: the bucket IS the app destination, so the guard fires and
        // the offsite backup must opt in with allow_shared_bucket.
        let s3 = make("s3");
        assert_eq!(s3.app_storage_bucket.as_deref(), Some("shared"));
        assert!(destinations_conflict(
            "shared",
            Some("https://minio.example:9000"),
            s3.app_storage_bucket.as_deref(),
            s3.app_storage_endpoint.as_deref(),
        ));
    }

    #[test]
    fn key_is_manifest_matches_only_the_run_manifest() {
        assert!(key_is_manifest(
            "db/prod/20260710T040506Z-abcd1234/manifest.json"
        ));
        assert!(!key_is_manifest(
            "db/prod/20260710T040506Z-abcd1234/control.dump"
        ));
        // A file that merely contains the word is not the manifest.
        assert!(!key_is_manifest("db/prod/r/not-manifest.json.dump"));
    }

    #[test]
    fn order_cleanup_keys_deletes_manifest_first() {
        // P2 #21: on a failed upload the run manifest must be removed FIRST so the
        // completeness marker is gone even if a later delete fails — otherwise a
        // failed run could still be counted `complete` for latest/retention.
        let written = vec![
            "db/prod/r/control.dump".to_owned(),
            "db/prod/r/shard-a.dump".to_owned(),
            "db/prod/r/manifest.json".to_owned(),
        ];
        let ordered = order_cleanup_keys(&written);
        assert_eq!(ordered[0], "db/prod/r/manifest.json");
        // The remaining (non-manifest) keys keep their written order.
        assert_eq!(
            &ordered[1..],
            &[
                "db/prod/r/control.dump".to_owned(),
                "db/prod/r/shard-a.dump".to_owned()
            ]
        );

        // And downstream: with the manifest deleted, complete_remote_run_ids no
        // longer counts the run — so the failed run can't become `latest`.
        let list_prefix = "db/prod/";
        let after_cleanup = vec![s3::S3Object {
            key: "db/prod/r/control.dump".to_owned(), // manifest was cleaned up
            size: 10,
        }];
        assert!(complete_remote_run_ids(&after_cleanup, list_prefix).is_empty());
    }

    #[test]
    fn complete_remote_run_ids_only_counts_runs_with_a_manifest() {
        // P2 #2: a newer PARTIAL run (artifacts but no manifest.json) and an
        // older COMPLETE run. `latest` must pick the older complete run.
        let list_prefix = "db/prod/";
        let objects = vec![
            // Older, complete run.
            s3::S3Object {
                key: "db/prod/20260709T010101Z/control.dump".to_owned(),
                size: 10,
            },
            s3::S3Object {
                key: "db/prod/20260709T010101Z/manifest.json".to_owned(),
                size: 1,
            },
            // Newer, partial run — died before uploading the (last) manifest.
            s3::S3Object {
                key: "db/prod/20260710T010101Z/control.dump".to_owned(),
                size: 10,
            },
        ];
        // The raw run ids include the partial newer run...
        assert_eq!(
            remote_run_ids(&objects, list_prefix),
            vec!["20260709T010101Z".to_owned(), "20260710T010101Z".to_owned()]
        );
        // ...but only the complete (older) run is a candidate.
        let complete = complete_remote_run_ids(&objects, list_prefix);
        assert_eq!(complete, vec!["20260709T010101Z".to_owned()]);
        // `latest` = newest COMPLETE run = the older run, not the partial newer one.
        assert_eq!(
            complete.into_iter().next_back().as_deref(),
            Some("20260709T010101Z")
        );
    }

    #[test]
    fn remote_retention_counts_only_complete_runs() {
        // P2 #3 (Codex scenario): keep=2 with complete runs A/B, a NEWER partial
        // run C (no manifest), and the just-uploaded complete run D. Retention
        // must count only complete runs — keep the newest 2 complete (D, B),
        // prune the older complete A, and NEVER touch the partial C or D.
        let list_prefix = "db/prod/";
        let a = "20260701T000000Z"; // complete, oldest
        let b = "20260702T000000Z"; // complete
        let c = "20260703T000000Z"; // PARTIAL (no manifest), newer than B
        let d = "20260704T000000Z"; // complete, just uploaded (newest)
        let objects = vec![
            s3::S3Object {
                key: format!("{list_prefix}{a}/control.dump"),
                size: 1,
            },
            s3::S3Object {
                key: format!("{list_prefix}{a}/manifest.json"),
                size: 1,
            },
            s3::S3Object {
                key: format!("{list_prefix}{b}/control.dump"),
                size: 1,
            },
            s3::S3Object {
                key: format!("{list_prefix}{b}/manifest.json"),
                size: 1,
            },
            // Partial C: artifact only, no manifest.
            s3::S3Object {
                key: format!("{list_prefix}{c}/control.dump"),
                size: 1,
            },
            s3::S3Object {
                key: format!("{list_prefix}{d}/control.dump"),
                size: 1,
            },
            s3::S3Object {
                key: format!("{list_prefix}{d}/manifest.json"),
                size: 1,
            },
        ];

        // Retention input = complete runs only (A, B, D) — the partial C is absent.
        let complete = complete_remote_run_ids(&objects, list_prefix);
        assert_eq!(
            complete,
            vec![a.to_owned(), b.to_owned(), d.to_owned()],
            "partial run C must not count as a retention slot"
        );

        // keep=2, just-uploaded=D => prune only the oldest complete run A.
        let to_remove = plan_remote_pruning(&complete, 2, d);
        assert_eq!(to_remove, vec![a.to_owned()]);
        // The partial C is NOT pruned (never a candidate), and D is never pruned.
        assert!(!to_remove.contains(&c.to_owned()));
        assert!(!to_remove.contains(&d.to_owned()));
        // B survives (it's within the newest-2 complete).
        assert!(!to_remove.contains(&b.to_owned()));
    }

    #[test]
    fn upload_orders_manifest_last() {
        // P2 #2(a): the manifest must upload last so its remote presence marks a
        // complete run. Mirror upload_run's ordering.
        let mut files = vec![
            "manifest.json".to_owned(),
            "shard-east.dump".to_owned(),
            "control.dump".to_owned(),
        ];
        files.sort();
        files.sort_by_key(|f| f.as_str() == MANIFEST_FILE);
        assert_eq!(
            files,
            vec![
                "control.dump".to_owned(),
                "shard-east.dump".to_owned(),
                "manifest.json".to_owned(),
            ]
        );
    }

    #[test]
    fn offsite_listing_flags_incomplete_runs() {
        // P2 #2: an incomplete run (no manifest) is flagged in `db offsite list`.
        let list_prefix = "db/prod/";
        let objects = vec![s3::S3Object {
            key: "db/prod/20260710T010101Z/control.dump".to_owned(),
            size: 10,
        }];
        let runs = group_offsite_objects(&objects, list_prefix);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].complete);
        let table = format_offsite_listing(&runs);
        assert!(table.contains("INCOMPLETE"), "table was: {table}");
    }

    #[test]
    fn is_safe_leaf_name_rejects_separators_and_traversal() {
        // P2 #13: a downloaded offsite object name must be a single plain file.
        for good in ["control.dump", "manifest.json", "shard-orders.dump"] {
            assert!(is_safe_leaf_name(good), "{good:?} should be accepted");
        }
        for bad in [
            "",
            "a/b",
            "a\\b",
            "..\\..\\evil",
            "../evil",
            "C:\\evil",
            "..",
            ".",
            "/abs",
            "sub/manifest.json",
        ] {
            assert!(!is_safe_leaf_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn remote_run_token_is_eight_lowercase_hex() {
        // P2 #12: the token is 8 hex chars derived from host/pid/time entropy.
        let token = remote_run_token();
        assert_eq!(token.len(), 8, "token was {token:?}");
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "token must be lowercase hex: {token:?}"
        );
    }

    #[test]
    fn remote_run_id_appends_token_to_local_id() {
        // P2 #12: remote id = <local timestamp>-<token>; the timestamp still leads
        // so lexical sort (and thus `latest`) is unchanged.
        let id = remote_run_id("20260710T040506Z", "deadbeef");
        assert_eq!(id, "20260710T040506Z-deadbeef");
        assert!(id.starts_with("20260710T040506Z"));
    }

    #[test]
    fn match_exact_remote_run_resolves_by_full_id_and_timestamp() {
        // P2 #12: complete remote run ids carry a token; a selector matches by
        // full id OR by the bare timestamp when exactly one tokened id begins with it.
        let ids: Vec<String> = ["20260709T010101Z-aaaa1111", "20260710T040506Z-bbbb2222"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        // Full id → itself.
        assert_eq!(
            match_exact_remote_run(&ids, "20260710T040506Z-bbbb2222").unwrap(),
            "20260710T040506Z-bbbb2222"
        );
        // Bare timestamp uniquely prefixes one run → that run.
        assert_eq!(
            match_exact_remote_run(&ids, "20260710T040506Z").unwrap(),
            "20260710T040506Z-bbbb2222"
        );
        // A timestamp with no matching run → Err(empty).
        assert_eq!(
            match_exact_remote_run(&ids, "20990101T000000Z").unwrap_err(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn match_exact_remote_run_errors_on_ambiguous_same_second() {
        // Two hosts backed up the SAME second → two tokened ids share the <ts>.
        // A bare-timestamp selector is ambiguous and must list both candidates.
        let ids: Vec<String> = ["20260710T040506Z-aaaa1111", "20260710T040506Z-bbbb2222"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let err = match_exact_remote_run(&ids, "20260710T040506Z").unwrap_err();
        assert_eq!(err.len(), 2);
        assert!(err.contains(&"20260710T040506Z-aaaa1111".to_owned()));
        assert!(err.contains(&"20260710T040506Z-bbbb2222".to_owned()));
        // But the full id still resolves unambiguously.
        assert_eq!(
            match_exact_remote_run(&ids, "20260710T040506Z-aaaa1111").unwrap(),
            "20260710T040506Z-aaaa1111"
        );
    }

    #[test]
    fn complete_remote_run_ids_sort_newest_last_with_tokens() {
        // P2 #12: the <ts> dominates the lexical sort, so `latest` (newest) is the
        // last element regardless of the token tiebreak.
        let list_prefix = "db/prod/";
        let objects = vec![
            s3::S3Object {
                key: "db/prod/20260710T040506Z-zzzz9999/manifest.json".to_owned(),
                size: 1,
            },
            s3::S3Object {
                key: "db/prod/20260711T040506Z-aaaa0000/manifest.json".to_owned(),
                size: 1,
            },
        ];
        let ids = complete_remote_run_ids(&objects, list_prefix);
        assert_eq!(ids.last().unwrap(), "20260711T040506Z-aaaa0000");
    }

    #[test]
    fn plan_remote_pruning_keeps_newest_and_never_the_just_uploaded() {
        let runs: Vec<String> = ["r1", "r2", "r3", "r4", "r5"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        // Keep 2 => remove r1, r2, r3 (the 3 oldest). r5 is the just-uploaded.
        assert_eq!(
            plan_remote_pruning(&runs, 2, "r5"),
            vec!["r1".to_owned(), "r2".to_owned(), "r3".to_owned()]
        );
        // keep >= len => remove nothing.
        assert!(plan_remote_pruning(&runs, 5, "r5").is_empty());
        // keep 0 clamps to 1 (never wipe everything).
        assert_eq!(plan_remote_pruning(&runs, 0, "r5").len(), 4);
    }

    #[test]
    fn plan_remote_pruning_excludes_just_uploaded_even_if_oldest() {
        // Pathological: the just-uploaded run id sorts oldest (clock skew). It
        // must STILL never be pruned.
        let runs: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect();
        // keep 1 => would remove a, b; but "a" is the just-uploaded => keep it.
        assert_eq!(plan_remote_pruning(&runs, 1, "a"), vec!["b".to_owned()]);
    }

    #[test]
    fn parse_offsite_ref_handles_latest_and_explicit() {
        let latest = parse_offsite_ref("offsite:prod/latest").unwrap();
        assert_eq!(latest.profile, "prod");
        assert_eq!(latest.selector, OffsiteSelector::Latest);

        // Case-insensitive "latest".
        assert_eq!(
            parse_offsite_ref("prod/LATEST").unwrap().selector,
            OffsiteSelector::Latest
        );

        let exact = parse_offsite_ref("offsite:prod/20260710T040506Z").unwrap();
        assert_eq!(exact.profile, "prod");
        assert_eq!(
            exact.selector,
            OffsiteSelector::Exact("20260710T040506Z".to_owned())
        );

        // Missing pieces => None.
        assert!(parse_offsite_ref("offsite:prod").is_none());
        assert!(parse_offsite_ref("offsite:/latest").is_none());
        assert!(parse_offsite_ref("prod/").is_none());
    }

    #[test]
    fn resolve_offsite_ref_routes_local_only_when_not_indicated() {
        // (c) A bare local path with neither flag nor prefix routes to LOCAL
        // restore (None), unchanged.
        assert!(resolve_offsite_ref(false, "backups/prod/20260710T040506Z").is_none());
        assert!(resolve_offsite_ref(false, "latest").is_none());

        // (d) Well-formed offsite refs parse (Some(Ok(_))).
        let ok = resolve_offsite_ref(false, "offsite:prod/latest").unwrap();
        assert_eq!(
            ok.unwrap(),
            OffsiteRef {
                profile: "prod".to_owned(),
                selector: OffsiteSelector::Latest,
            }
        );
        let ok = resolve_offsite_ref(true, "prod/20260710T040506Z").unwrap();
        assert_eq!(
            ok.unwrap(),
            OffsiteRef {
                profile: "prod".to_owned(),
                selector: OffsiteSelector::Exact("20260710T040506Z".to_owned()),
            }
        );
    }

    #[test]
    fn resolve_offsite_ref_errors_on_malformed_when_indicated() {
        // (a) `--offsite` with a malformed ref (no `<profile>/`) must ERROR, not
        // fall through to a local artifact named `latest`.
        let err = resolve_offsite_ref(true, "latest").unwrap().unwrap_err();
        assert!(matches!(err, BackupError::InvalidOffsiteRef { .. }));
        let msg = err.to_string();
        assert!(msg.contains("latest"));
        assert!(msg.contains("offsite:<profile>/<timestamp|latest>"));

        // (b) `offsite:` prefix with a malformed body must ERROR too.
        assert!(matches!(
            resolve_offsite_ref(false, "offsite:prod").unwrap(),
            Err(BackupError::InvalidOffsiteRef { .. })
        ));
        assert!(matches!(
            resolve_offsite_ref(false, "offsite:/latest").unwrap(),
            Err(BackupError::InvalidOffsiteRef { .. })
        ));
        // Flag set but empty positional => still an offsite error, not local.
        assert!(matches!(
            resolve_offsite_ref(true, "").unwrap(),
            Err(BackupError::InvalidOffsiteRef { .. })
        ));
    }

    #[test]
    fn group_and_format_offsite_listing() {
        let list_prefix = "db/prod/";
        let objects = vec![
            s3::S3Object {
                key: "db/prod/20260710T040506Z/manifest.json".to_owned(),
                size: 128,
            },
            s3::S3Object {
                key: "db/prod/20260710T040506Z/control.dump".to_owned(),
                size: 4096,
            },
            s3::S3Object {
                key: "db/prod/20260709T040506Z/control.dump".to_owned(),
                size: 2048,
            },
        ];
        let runs = group_offsite_objects(&objects, list_prefix);
        assert_eq!(runs.len(), 2);
        // Sorted ascending by run id.
        assert_eq!(runs[0].run_id, "20260709T040506Z");
        assert_eq!(runs[1].run_id, "20260710T040506Z");
        assert_eq!(runs[1].total, 128 + 4096);

        let table = format_offsite_listing(&runs);
        assert!(table.contains("RUN ID"));
        assert!(table.contains("20260710T040506Z"));
        assert!(table.contains("control.dump"));
        assert!(table.contains("manifest.json"));
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    /// Docker/live-DB round-trip (AC #5). Ignored by default; run with a live
    /// Postgres and the pg client tools available:
    ///
    /// ```text
    /// DATABASE_URL=postgres://postgres:postgres@localhost:5432/autumn_backup_it \
    ///   cargo test -p autumn-cli --lib -- --ignored backup_restore_round_trip
    /// ```
    ///
    /// Proves seed → backup → drop rows → restore → row-level equality.
    #[test]
    #[ignore = "requires a live Postgres and pg_dump/pg_restore on PATH"]
    fn backup_restore_round_trip() {
        use diesel::connection::SimpleConnection as _;
        use diesel::{Connection as _, PgConnection, RunQueryDsl as _, sql_query};

        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        #[derive(diesel::QueryableByName)]
        struct Rt {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            id: i32,
            #[diesel(sql_type = diesel::sql_types::Text)]
            name: String,
        }

        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping round-trip");
            return;
        };

        // Seed a table with known rows.
        let mut conn = PgConnection::establish(&url).expect("connect");
        conn.batch_execute(
            "DROP TABLE IF EXISTS backup_rt; \
             CREATE TABLE backup_rt (id INT PRIMARY KEY, name TEXT NOT NULL); \
             INSERT INTO backup_rt VALUES (1,'alpha'),(2,'beta'),(3,'gamma');",
        )
        .expect("seed");

        // Backup (custom format) into a temp dir.
        let tmp = tempfile::tempdir().unwrap();
        let tools = PgTools::locate();
        let pg_dump = tools.require("pg_dump").expect("pg_dump present");
        let run_dir = tmp.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let targets = vec![ResolvedTarget {
            label: "control".to_owned(),
            url: url.clone(),
            backend: TargetBackend::Postgres,
        }];
        backup_into(
            &run_dir,
            &targets,
            BackupFormat::Custom,
            Some(&pg_dump),
            &tools,
            "dev",
        )
        .expect("backup succeeds and verifies");

        // Simulate data loss.
        conn.batch_execute("DELETE FROM backup_rt;")
            .expect("delete");
        let after_delete: Count = sql_query("SELECT COUNT(*) AS n FROM backup_rt")
            .get_result(&mut conn)
            .unwrap();
        assert_eq!(after_delete.n, 0);

        // Restore.
        let artifact = run_dir.join("control.dump");
        run_restore_one(
            &tools,
            &url,
            &artifact,
            TargetBackend::Postgres,
            BackupFormat::Custom,
            "dev",
        )
        .expect("restore succeeds");

        // Row-level equality.
        let rows: Vec<Rt> = sql_query("SELECT id, name FROM backup_rt ORDER BY id")
            .load(&mut conn)
            .unwrap();
        let got: Vec<(i32, &str)> = rows.iter().map(|r| (r.id, r.name.as_str())).collect();
        assert_eq!(got, vec![(1, "alpha"), (2, "beta"), (3, "gamma")]);

        conn.batch_execute("DROP TABLE backup_rt;").ok();
    }
}
