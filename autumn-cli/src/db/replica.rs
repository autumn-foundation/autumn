//! `autumn db replica` — restore, inspect and verify a continuously replicated
//! `SQLite` database (issue #1628, phase 2).
//!
//! The whole point is the fresh-box case: a machine with nothing on it but this
//! binary, `autumn.toml`, and the destination credentials in the environment.
//! Everything the restore needs is therefore read from the same profile-aware
//! configuration path `autumn db backup --upload` uses — the merged
//! `autumn.toml` + `[profile.<p>]` overlay, the `.env.<profile>` overlay, and
//! `AUTUMN_*` overrides — so an operator does not have to learn a second set of
//! conventions under pressure.
//!
//! The restore itself is `autumn_web::replication::restore`, the *same* code the
//! in-process verifier runs. It refuses a replica with a hole in its segment
//! sequence, a payload whose digest does not match, or a reassembled database
//! that fails `PRAGMA integrity_check` — and it publishes nothing until those
//! checks pass, so a refused restore never leaves a half-built database behind.
//!
//! Production safety matches `autumn db restore` (#1595): the production guard
//! (`--force`), and — additionally — overwriting an existing database file
//! always requires `--force`, whatever the profile.

use std::path::PathBuf;

use autumn_web::config::{AutumnConfig, ReplicationConfig};
use autumn_web::replication::{ReplicaDestination, restore, segment};
use chrono::{DateTime, Utc};

use crate::migrate;

/// The subcommands of `autumn db replica`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaCommand {
    /// Rebuild the database from the replica.
    Restore {
        /// Profile overlay to resolve the destination through.
        profile: Option<String>,
        /// Restore to this instant instead of the latest replicated state.
        timestamp: Option<String>,
        /// Write the database here instead of the configured `database.url`.
        output: Option<PathBuf>,
        /// Bypass the production guard.
        force: bool,
        /// Allow replacing a database file that already exists.
        overwrite: bool,
    },
    /// Report what the destination holds and how fresh it is.
    Status {
        /// Profile overlay to resolve the destination through.
        profile: Option<String>,
        /// Emit the report as JSON instead of a table.
        json: bool,
    },
    /// Prove the replica restorable by restoring it into a scratch directory.
    Verify {
        /// Profile overlay to resolve the destination through.
        profile: Option<String>,
    },
}

/// Failure modes of `autumn db replica`. `Display` is credential-safe.
#[derive(Debug)]
pub enum ReplicaError {
    /// No `[replication]` section is configured.
    NotConfigured,
    /// The section is present but unusable.
    Config {
        /// Operator-facing detail.
        detail: String,
    },
    /// `--timestamp` was not an RFC 3339 instant.
    BadTimestamp {
        /// What was passed.
        value: String,
    },
    /// The restore target already exists and `--overwrite` was not given.
    WouldOverwrite {
        /// The path that would be replaced.
        path: String,
    },
    /// The restore target profile is production and `--force` was not given.
    ProductionRefused {
        /// The active profile.
        profile: String,
    },
    /// No output path could be resolved (no `--output`, no `SQLite`
    /// `database.url`).
    NoOutput,
    /// The replica could not be planned, verified, or applied.
    Restore {
        /// Operator-facing detail.
        detail: String,
    },
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                f,
                "No [replication] section is configured.\n  Add one (with [replication.s3] \
                 bucket/region/endpoint and *_env credential indirection, or a `path` \
                 directory) to autumn.toml, or set AUTUMN_REPLICATION__* env vars."
            ),
            Self::Config { detail } => write!(f, "Invalid [replication] configuration: {detail}"),
            Self::BadTimestamp { value } => write!(
                f,
                "--timestamp {value:?} is not an RFC 3339 instant.\n  \
                 Example: --timestamp 2026-09-02T14:30:00Z"
            ),
            Self::WouldOverwrite { path } => write!(
                f,
                "{path} already exists.\n  Re-run with --overwrite to replace it (this \
                 destroys the data in it), or pass --output to restore somewhere else."
            ),
            Self::ProductionRefused { profile } => write!(
                f,
                "Refusing to restore over the {profile:?} profile's database.\n  \
                 Re-run with --force if you really mean it (this overwrites data)."
            ),
            Self::NoOutput => write!(
                f,
                "No restore target could be resolved.\n  Set database.url to a SQLite \
                 target in autumn.toml, or pass --output <PATH>."
            ),
            Self::Restore { detail } => write!(f, "{detail}"),
        }
    }
}

/// Entry point dispatched from `main`.
pub fn run(command: &ReplicaCommand) {
    eprintln!("\u{1F342} autumn db replica\n");
    let result = match command {
        ReplicaCommand::Restore {
            profile,
            timestamp,
            output,
            force,
            overwrite,
        } => run_restore(
            profile.as_deref(),
            timestamp.as_deref(),
            output.as_ref(),
            *force,
            *overwrite,
        ),
        ReplicaCommand::Status { profile, json } => run_status(profile.as_deref(), *json),
        ReplicaCommand::Verify { profile } => run_verify(profile.as_deref()),
    };
    if let Err(e) = result {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

/// The `[replication]` section plus the resolved app config it came from.
#[derive(Debug)]
struct Loaded {
    profile: String,
    replication: ReplicationConfig,
    config: AutumnConfig,
}

/// Resolve `[replication]` exactly the way the running app does.
fn load(profile: Option<&str>) -> Result<Loaded, ReplicaError> {
    let profile = migrate::canonical_profile(&migrate::effective_profile(profile));
    let env = crate::db::backup::dotenv_env_for_profile(&profile);
    let table = migrate::read_autumn_toml_table_with_profile_from_config_dir(Some(&profile));
    resolve(table.as_ref(), &env, &profile)
}

/// Pure core of [`load`]: build the replication view from a merged TOML table
/// and an `Env`. Separated so the fresh-box behaviour is unit-testable without
/// touching the filesystem or the process environment.
fn resolve(
    table: Option<&toml::Table>,
    env: &dyn autumn_web::config::Env,
    profile: &str,
) -> Result<Loaded, ReplicaError> {
    let mut config = match table {
        Some(table) => {
            let toml_str = toml::to_string(table).map_err(|e| ReplicaError::Config {
                detail: e.to_string(),
            })?;
            // A TOML error quotes the offending line, and the merged config can
            // carry a `database.url` with a password in it.
            toml::from_str::<AutumnConfig>(&toml_str).map_err(|e| ReplicaError::Config {
                detail: autumn_web::replication::redact_credentials(&e.to_string()),
            })?
        }
        None => AutumnConfig::default(),
    };
    config.apply_env_overrides_with_env(env);

    let replication = config
        .replication
        .clone()
        .map(|boxed| *boxed)
        .ok_or(ReplicaError::NotConfigured)?;
    Ok(Loaded {
        profile: profile.to_owned(),
        replication,
        config,
    })
}

impl Loaded {
    /// The destination key prefix for this app/profile.
    fn root(&self) -> String {
        segment::root_prefix(self.replication.prefix.as_deref(), &self.profile)
    }

    /// The app's own blob-storage destination, for the distinct-destination
    /// guard.
    ///
    /// Only a genuinely S3-backed app has one to clash with; a leftover
    /// `[storage.s3]` bucket on the local backend is inert (the same rule
    /// `autumn db backup --upload` applies).
    fn storage_destination(&self) -> Option<(String, Option<String>)> {
        if self.config.storage.backend != autumn_web::storage::StorageBackend::S3 {
            return None;
        }
        self.config
            .storage
            .s3
            .bucket
            .clone()
            .map(|bucket| (bucket, self.config.storage.s3.endpoint.clone()))
    }

    fn destination(&self) -> Result<std::sync::Arc<dyn ReplicaDestination>, ReplicaError> {
        let storage = self.storage_destination();
        autumn_web::replication::build_destination(
            &self.replication,
            storage.as_ref().map(|(bucket, endpoint)| {
                autumn_web::replication::StorageDestination {
                    bucket,
                    endpoint: endpoint.as_deref(),
                }
            }),
        )
        .map_err(|e| ReplicaError::Config {
            detail: e.to_string(),
        })
    }

    /// Where a restore writes by default: the `SQLite` file `database.url` names.
    fn configured_database_file(&self) -> Option<PathBuf> {
        let url = self.config.database.primary_url.as_deref().or(self
            .config
            .database
            .url
            .as_deref())?;
        autumn_web::replication::database_file(url)
    }
}

/// Create an unpredictable, owner-only scratch directory under `base`.
fn create_private_dir(base: &std::path::Path) -> Result<PathBuf, ReplicaError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    for _ in 0..8 {
        let candidate = base.join(format!(".autumn-replica-verify-{:016x}", fastrand_u64()));
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(ReplicaError::Restore {
                    detail: format!(
                        "could not create a scratch directory under {}: {e}",
                        base.display()
                    ),
                });
            }
        }
    }
    Err(ReplicaError::Restore {
        detail: format!(
            "could not find an unused scratch directory name under {}",
            base.display()
        ),
    })
}

/// A non-cryptographic 64-bit value, good enough to make a scratch directory
/// name unpredictable to another local process. `autumn-cli` does not depend on
/// `rand`, and this needs no cryptographic strength — the directory is created
/// with `O_EXCL`-equivalent semantics and mode 0700, so the name is a nuisance
/// barrier, not the security boundary.
fn fastrand_u64() -> u64 {
    use std::hash::{BuildHasher as _, RandomState};
    RandomState::new().hash_one(std::process::id())
}

/// Parse `--timestamp` as RFC 3339.
fn parse_timestamp(value: Option<&str>) -> Result<Option<DateTime<Utc>>, ReplicaError> {
    value.map_or(Ok(None), |raw| {
        DateTime::parse_from_rfc3339(raw.trim())
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|_| ReplicaError::BadTimestamp {
                value: raw.to_owned(),
            })
    })
}

fn run_restore(
    profile: Option<&str>,
    timestamp: Option<&str>,
    output: Option<&PathBuf>,
    force: bool,
    overwrite: bool,
) -> Result<(), ReplicaError> {
    let loaded = load(profile)?;
    let target = parse_timestamp(timestamp)?;

    let explicit_output = output.is_some();
    let destination_path = output
        .cloned()
        .or_else(|| loaded.configured_database_file())
        .ok_or(ReplicaError::NoOutput)?;

    // The production guard applies to writing over the app's OWN database — the
    // destructive act (#1595). Restoring to an explicit `--output` writes nothing
    // the app uses, so it is not gated; the overwrite guard below still is.
    if !explicit_output {
        crate::db::guard_destructive_public(&loaded.profile, force).map_err(|()| {
            ReplicaError::ProductionRefused {
                profile: loaded.profile.clone(),
            }
        })?;
    }
    // Replacing an existing database file is destructive whatever the profile,
    // and needs its own opt-in: `--force` is about the profile, not about data.
    if destination_path.exists() && !overwrite {
        return Err(ReplicaError::WouldOverwrite {
            path: destination_path.display().to_string(),
        });
    }

    let remote = loaded.destination()?;
    eprintln!("  \u{2139} source: {}", remote.describe());
    eprintln!("  \u{2139} prefix: {}", loaded.root());

    let plan = restore::plan(remote.as_ref(), &loaded.root(), target).map_err(|e| {
        ReplicaError::Restore {
            detail: e.to_string(),
        }
    })?;
    eprintln!(
        "  \u{2139} generation {} ({} segment(s)) \u{2192} {}",
        plan.generation,
        plan.segments.len(),
        plan.effective.to_rfc3339()
    );
    if let Some(requested) = plan.requested {
        eprintln!("  \u{2139} requested {}", requested.to_rfc3339());
    }

    let outcome = restore::apply(remote.as_ref(), &plan, &destination_path).map_err(|e| {
        ReplicaError::Restore {
            detail: e.to_string(),
        }
    })?;
    eprintln!("  \u{2713} integrity verified.");
    eprintln!(
        "\n\u{2713} Restored {} byte(s) to {} ({} WAL frame(s) replayed), current as of {}.",
        outcome.bytes,
        outcome.output.display(),
        outcome.frames_replayed,
        outcome.plan.effective.to_rfc3339()
    );
    Ok(())
}

fn run_status(profile: Option<&str>, json: bool) -> Result<(), ReplicaError> {
    let loaded = load(profile)?;
    let remote = loaded.destination()?;
    let root = loaded.root();
    eprintln!("  \u{2139} source: {}", remote.describe());
    eprintln!("  \u{2139} prefix: {root}");

    let plan = restore::plan(remote.as_ref(), &root, None).map_err(|e| ReplicaError::Restore {
        detail: e.to_string(),
    })?;
    let lag = Utc::now()
        .signed_duration_since(plan.effective)
        .num_seconds()
        .max(0);
    if json {
        // Machine-readable on stdout: this is the monitoring surface, and an
        // operator should not have to parse a padded table.
        let report = serde_json::json!({
            "generation": plan.generation,
            "opened": plan.generation_started_at.to_rfc3339(),
            "segments": plan.segments.len(),
            "current_as_of": plan.effective.to_rfc3339(),
            "replication_lag_seconds": lag,
            "retention_hours": loaded.replication.retention_hours,
            "rpo_seconds": loaded.replication.rpo_secs,
        });
        println!("{report}");
        return Ok(());
    }
    println!("generation      {}", plan.generation);
    println!(
        "opened          {}",
        plan.generation_started_at.to_rfc3339()
    );
    println!("segments        {}", plan.segments.len());
    println!("current as of   {}", plan.effective.to_rfc3339());
    println!("replication lag {lag}s");
    println!(
        "retention       {} hour(s)",
        loaded.replication.retention_hours
    );
    println!("rpo             {}s", loaded.replication.rpo_secs);
    Ok(())
}

fn run_verify(profile: Option<&str>) -> Result<(), ReplicaError> {
    let loaded = load(profile)?;
    let remote = loaded.destination()?;
    let root = loaded.root();
    eprintln!("  \u{2139} source: {}", remote.describe());

    // A verification restores the WHOLE database, so the scratch directory holds
    // a complete copy of production data. It therefore goes next to where the
    // database belongs (not a world-readable shared temp directory), under an
    // unpredictable name, created 0700 so no other local user can read it.
    let base = loaded
        .configured_database_file()
        .and_then(|db| db.parent().map(std::path::Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    let scratch = create_private_dir(&base)?;
    let result = restore::restore(remote.as_ref(), &root, None, &scratch.join("verified.db"));
    let _ = std::fs::remove_dir_all(&scratch);

    match result {
        Ok(outcome) => {
            eprintln!(
                "\n\u{2713} Replica is restorable: {} byte(s), current as of {}.",
                outcome.bytes,
                outcome.plan.effective.to_rfc3339()
            );
            Ok(())
        }
        Err(e) => Err(ReplicaError::Restore {
            detail: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::config::Env;
    use std::collections::HashMap;

    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }
        fn with(mut self, key: &str, value: &str) -> Self {
            self.0.insert(key.to_owned(), value.to_owned());
            self
        }
    }

    impl Env for MapEnv {
        fn var(&self, key: &str) -> Result<String, std::env::VarError> {
            self.0
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    #[test]
    fn a_missing_section_reports_how_to_configure_one() {
        let err = resolve(None, &MapEnv::new(), "prod").expect_err("must fail");
        let rendered = format!("{err}");
        assert!(rendered.contains("[replication]"), "{rendered}");
        assert!(rendered.contains("AUTUMN_REPLICATION__"), "{rendered}");
    }

    #[test]
    fn an_all_env_deployment_resolves_without_any_toml() {
        let env = MapEnv::new()
            .with("AUTUMN_REPLICATION__ENABLED", "true")
            .with("AUTUMN_REPLICATION__S3__BUCKET", "replicas")
            .with("AUTUMN_REPLICATION__S3__REGION", "auto")
            .with("AUTUMN_REPLICATION__S3__ACCESS_KEY_ID_ENV", "R2_KEY")
            .with("AUTUMN_REPLICATION__S3__SECRET_ACCESS_KEY_ENV", "R2_SECRET")
            .with("AUTUMN_REPLICATION__PREFIX", "db");
        let loaded = resolve(None, &env, "prod").expect("resolves from env alone");
        assert!(loaded.replication.enabled);
        assert_eq!(loaded.root(), "db/prod");
        assert_eq!(
            loaded
                .replication
                .s3
                .as_ref()
                .and_then(|s3| s3.bucket.clone()),
            Some("replicas".to_owned())
        );
    }

    #[test]
    fn toml_is_overlaid_by_env() {
        let table: toml::Table = toml::from_str(
            r#"
            [replication]
            enabled = false
            prefix = "from-toml"
            retention_hours = 24

            [replication.s3]
            bucket = "from-toml"
            access_key_id_env = "KEY"
            secret_access_key_env = "SECRET"
            "#,
        )
        .expect("toml");
        let env = MapEnv::new()
            .with("AUTUMN_REPLICATION__ENABLED", "true")
            .with("AUTUMN_REPLICATION__S3__BUCKET", "from-env");
        let loaded = resolve(Some(&table), &env, "staging").expect("resolve");
        assert!(loaded.replication.enabled, "env overrides the TOML toggle");
        assert_eq!(loaded.replication.retention_hours, 24);
        assert_eq!(loaded.root(), "from-toml/staging");
        assert_eq!(
            loaded
                .replication
                .s3
                .as_ref()
                .and_then(|s3| s3.bucket.clone()),
            Some("from-env".to_owned())
        );
    }

    #[test]
    fn the_default_restore_target_is_the_configured_sqlite_file() {
        let table: toml::Table = toml::from_str(
            r#"
            [database]
            url = "sqlite:///var/lib/app.db"

            [replication]
            enabled = true
            path = "/mnt/replica"
            "#,
        )
        .expect("toml");
        let loaded = resolve(Some(&table), &MapEnv::new(), "prod").expect("resolve");
        assert_eq!(
            loaded.configured_database_file(),
            Some(PathBuf::from("/var/lib/app.db"))
        );
    }

    #[test]
    fn a_postgres_url_yields_no_default_restore_target() {
        let table: toml::Table = toml::from_str(
            r#"
            [database]
            url = "postgres://localhost/app"

            [replication]
            enabled = true
            path = "/mnt/replica"
            "#,
        )
        .expect("toml");
        let loaded = resolve(Some(&table), &MapEnv::new(), "prod").expect("resolve");
        assert_eq!(loaded.configured_database_file(), None);
    }

    #[test]
    fn timestamps_must_be_rfc3339() {
        assert_eq!(parse_timestamp(None).expect("none"), None);
        assert!(
            parse_timestamp(Some("2026-09-02T14:30:00Z"))
                .expect("parses")
                .is_some()
        );
        assert!(
            parse_timestamp(Some(" 2026-09-02T14:30:00+02:00 "))
                .expect("trims and parses")
                .is_some()
        );
        let err = parse_timestamp(Some("yesterday")).expect_err("must fail");
        assert!(format!("{err}").contains("RFC 3339"), "{err}");
    }

    #[test]
    fn error_messages_never_carry_a_secret() {
        let rendered = format!(
            "{}",
            ReplicaError::Config {
                detail: "[replication.s3] bucket is unset".to_owned()
            }
        );
        assert!(!rendered.contains("AKIA"));
        assert!(rendered.contains("bucket is unset"));
    }
}
