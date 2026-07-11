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
        }
    }
}

impl BackupError {
    fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

/// A single database captured by (or to be restored from) a backup run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedTarget {
    /// Stable label: `"control"` or `"shard:<name>"`.
    label: String,
    /// The resolved connection URL (never printed).
    url: String,
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

fn backup(args: &BackupArgs) -> Result<(), BackupError> {
    let targets = resolve_targets(args.profile.as_deref(), &args.target)?;
    // Include the managed cluster's bundled tools (daemon/cron path, where
    // AUTUMN_MANAGED_PG_DATA_DIR isn't inherited) so a managed backup needs zero
    // external tools on PATH (issue #1595).
    let tools = PgTools::locate_with_extra(managed_pg_data_dir());
    let pg_dump = tools.require("pg_dump")?;

    let profile = migrate::effective_profile(args.profile.as_deref());
    let root = backup_root(args.dir.as_deref(), &profile);
    let run_dir = create_unique_run_dir(&root, &run_dir_name(&now_utc()))?;

    // Everything below writes into `run_dir`. On ANY failure we remove the
    // whole run directory so a partial/empty artifact is never left behind and
    // never counted toward retention (AC #1).
    let result = backup_into(&run_dir, &targets, args.format, &pg_dump, &tools, &profile);
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&run_dir);
        return Err(e);
    }

    eprintln!(
        "\n\u{2713} Backup complete: {} ({} target(s), {} format).",
        run_dir.display(),
        targets.len(),
        args.format.pg_dump_format_flag(),
    );

    // Retention runs only AFTER a verified-successful backup, so a failed run
    // can never prune good history (AC #6).
    if let Some(keep) = args.keep {
        prune(&root, keep)?;
    }
    Ok(())
}

/// Dump every target into `run_dir`, verify each artifact, and write the
/// manifest. Returns `Err` (leaving cleanup to the caller) on the first failure.
fn backup_into(
    run_dir: &Path,
    targets: &[ResolvedTarget],
    format: BackupFormat,
    pg_dump: &Path,
    tools: &PgTools,
    profile: &str,
) -> Result<(), BackupError> {
    let mut manifest_targets = Vec::with_capacity(targets.len());
    for target in targets {
        let file_name = artifact_file_name(&target.label, format);
        let out_path = run_dir.join(&file_name);
        eprintln!(
            "\u{2500}\u{2500} backing up {} \u{2500}\u{2500}",
            target.label
        );

        let db = parsed_db_name(&target.url);
        run_pg_dump(pg_dump, &target.url, &out_path, format, &db)?;
        verify_artifact(&out_path, format, &db, tools)?;
        eprintln!("  \u{2713} {file_name} verified.");

        manifest_targets.push(ManifestTarget {
            label: target.label.clone(),
            file: file_name,
            database: db,
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
    format: BackupFormat,
    db: &str,
    tools: &PgTools,
) -> Result<(), BackupError> {
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

fn restore(args: &RestoreArgs) -> Result<(), BackupError> {
    // Production guard — identical to `autumn db drop` (AC #4).
    let profile = migrate::effective_profile(args.profile.as_deref());
    super::guard_destructive(&profile, args.force).map_err(|_| BackupError::ProductionRefused {
        profile: profile.clone(),
    })?;

    let plan = RestorePlan::discover(&args.artifact, args.shard.as_deref())?;
    let format = plan.format;
    let entries = plan.select(args.shard.as_deref())?;

    let targets = resolve_targets_for_restore(args.profile.as_deref(), &entries)?;
    // Same bundled-tool discovery as backup: a managed restore must also work
    // with zero external tools on PATH (issue #1595).
    let tools = PgTools::locate_with_extra(managed_pg_data_dir());

    // Verify EVERY artifact before mutating ANY database (AC #4): refuse to
    // start a destructive restore we can't finish.
    for (entry, _url) in &targets {
        let db = format!("(artifact) {}", entry.label);
        verify_artifact(&plan.dir.join(&entry.file), format, &db, &tools)?;
        eprintln!("  \u{2713} {} integrity verified.", entry.file);
    }

    for (entry, url) in &targets {
        eprintln!(
            "\u{2500}\u{2500} restoring {} \u{2500}\u{2500}",
            entry.label
        );
        let artifact = plan.dir.join(&entry.file);
        let db = parsed_db_name(url);
        run_restore_one(&tools, url, &artifact, format, &db)?;
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
    format: BackupFormat,
    db: &str,
) -> Result<(), BackupError> {
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
            let format = infer_format_from_path(path).ok_or_else(|| BackupError::BadArtifact {
                detail: format!(
                    "{} is neither a run directory nor a .dump/.sql artifact",
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
            }])
        }
        TargetSelector::All => {
            let mut targets = Vec::new();
            if let Some(control_url) = control {
                targets.push(ResolvedTarget {
                    label: "control".to_owned(),
                    url: control_url,
                });
            } else if shards.is_empty() {
                return Err(BackupError::NoUrl);
            }
            for (name, url) in shards {
                targets.push(ResolvedTarget {
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
fn artifact_file_name(label: &str, format: BackupFormat) -> String {
    let ext = format.extension();
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

/// Infer a backup format from a file extension.
fn infer_format_from_path(path: &Path) -> Option<BackupFormat> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("dump") => Some(BackupFormat::Custom),
        Some("sql") => Some(BackupFormat::Plain),
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
            artifact_file_name("control", BackupFormat::Custom),
            "control.dump"
        );
        assert_eq!(
            artifact_file_name("shard:us_east", BackupFormat::Custom),
            "shard-us_east.dump"
        );
        assert_eq!(
            artifact_file_name("shard:us east", BackupFormat::Plain),
            "shard-us_east.sql"
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

        verify_artifact(&archive, BackupFormat::Custom, "app", &tools)
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

    #[test]
    fn infer_format_from_extension() {
        assert_eq!(
            infer_format_from_path(Path::new("control.dump")),
            Some(BackupFormat::Custom)
        );
        assert_eq!(
            infer_format_from_path(Path::new("control.sql")),
            Some(BackupFormat::Plain)
        );
        assert_eq!(infer_format_from_path(Path::new("control.txt")), None);
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
                },
                ManifestTarget {
                    label: "shard:east".to_owned(),
                    file: "shard-east.dump".to_owned(),
                    database: "east".to_owned(),
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
        }];
        backup_into(
            &run_dir,
            &targets,
            BackupFormat::Custom,
            &pg_dump,
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
        run_restore_one(&tools, &url, &artifact, BackupFormat::Custom, "dev")
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
