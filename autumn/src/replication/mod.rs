//! Continuous `SQLite` replication with point-in-time restore (issue #1628).
//!
//! A running Autumn app on the zero-ops `SQLite` tier (#1614) ships its database's
//! write-ahead log to an offsite destination as it is written — no sidecar
//! process, no external tools, no second binary to supervise. A dead machine
//! costs seconds of writes instead of one whole backup interval, and
//! `autumn db replica restore` rebuilds the database on a fresh box from nothing
//! but the destination credentials.
//!
//! ```text
//!   app process                                    destination (S3 / directory)
//!   ┌───────────────────────────┐                  ┌──────────────────────────┐
//!   │ SQLite  ──writes──▶ -wal  │                  │ generations/<gen>/       │
//!   │                      │    │  base snapshot   │   snapshot.db.gz         │
//!   │  replication thread ─┼────┼─────────────────▶│   snapshot.json          │
//!   │   • scan frame chain │    │  WAL byte ranges │   segments/<seq>-<ms>.seg│
//!   │   • ship to commits  │    │                  │   segments/…             │
//!   │   • checkpoint       │    │                  └──────────────────────────┘
//!   └───────────────────────────┘
//! ```
//!
//! # Reading order
//!
//! * [`wal`] — `SQLite`'s WAL format: header, frame checksum chain, commit
//!   boundaries. The byte-level truth everything else depends on.
//! * [`segment`] — the destination's object namespace and the self-describing
//!   segment payload.
//! * [`destination`] — the object-store seam, plus the filesystem
//!   implementation; [`s3`] is the S3-compatible one.
//! * [`engine`] — the replication loop and the checkpoint interlock that makes
//!   it safe.
//! * [`restore`] — point-in-time restore, and the verification that refuses a
//!   replica rather than handing `SQLite` a damaged WAL.
//! * [`status`] — what the operator sees: lag, generation, verification, errors.
//!
//! # What this is not
//!
//! Not a read replica, not clustering, not networked `SQLite`. #1614 keeps this
//! tier single-host and single-writer, and continuous replication *depends* on
//! that: exactly one process may checkpoint the WAL. It also does not replace
//! `autumn db backup` — snapshots stay the coarse-grained, cross-backend story;
//! replication composes with them.

pub mod destination;
pub mod engine;
pub mod restore;
#[cfg(feature = "http-client")]
pub mod s3;
pub mod segment;
pub mod sqlite;
pub mod status;
pub mod wal;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use destination::{DestinationError, FileDestination, ReplicaDestination};
pub use engine::{ReplicationError, ReplicationSettings, Replicator, TickReport};
pub use restore::{RestoreError, RestoreOutcome, RestorePlan};
pub use status::{
    HealthThresholds, INDICATOR_NAME, ReplicationHealthIndicator, ReplicationSnapshot,
    ReplicationStatus,
};

use crate::config::ReplicationConfig;
use crate::time::ClockSource;

/// How much slack past the configured RPO the health indicator allows before
/// reporting `Down`. Three RPOs absorbs one slow upload and one retried tick
/// without paging an operator over ordinary jitter.
const LAG_ALERT_MULTIPLIER: u32 = 3;

/// How long a just-started replicator may have shipped nothing before that
/// counts against it (a first base snapshot of a large database takes a while).
const STARTUP_GRACE: Duration = Duration::from_secs(120);

/// Why replication could not be started.
#[derive(Debug)]
#[non_exhaustive]
pub enum SetupError {
    /// The `[replication]` section is absent or `enabled = false`.
    Disabled,
    /// The configured database is not a `SQLite` target. Postgres deployments
    /// have mature external continuous-archiving ecosystems; this is not it.
    NotSqlite {
        /// The backend that was detected.
        backend: String,
    },
    /// The configured database is in-memory, so there is nothing to replicate.
    InMemory,
    /// The section is present but invalid.
    Config {
        /// One operator-facing message per problem.
        errors: Vec<String>,
    },
    /// A named credential environment variable is unset or empty.
    MissingCredentialEnv {
        /// The variable that config named.
        var: String,
    },
    /// Replication points at the same bucket + endpoint as the app's blob
    /// storage without opting in.
    SharedBucket {
        /// The bucket both point at.
        bucket: String,
    },
    /// Building the destination failed.
    Destination(DestinationError),
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "continuous replication is not enabled"),
            Self::NotSqlite { backend } => write!(
                f,
                "[replication] is enabled but the database is {backend}, not SQLite.\n  \
                 Continuous replication covers the zero-ops SQLite tier; Postgres \
                 deployments should use their own continuous archiving (WAL-G, pgBackRest)."
            ),
            Self::InMemory => write!(
                f,
                "[replication] is enabled but the database is in-memory, so there is \
                 nothing to replicate. Point database.url at a file-backed SQLite target."
            ),
            Self::Config { errors } => {
                writeln!(f, "Invalid [replication] configuration:")?;
                for error in errors {
                    writeln!(f, "  - {error}")?;
                }
                Ok(())
            }
            Self::MissingCredentialEnv { var } => write!(
                f,
                "Replication credential environment variable {var:?} is not set (or is \
                 empty).\n  It is NAMED by [replication.s3] access_key_id_env / \
                 secret_access_key_env; the secret itself never lives in config."
            ),
            Self::SharedBucket { bucket } => write!(
                f,
                "[replication] targets bucket {bucket:?}, the same bucket + endpoint as the \
                 app's blob storage ([storage.s3]).\n  A lifecycle rule written for user \
                 uploads would then expire your replicas. Point replication at a distinct \
                 bucket, or set replication.allow_shared_bucket = true to opt in."
            ),
            Self::Destination(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<DestinationError> for SetupError {
    fn from(e: DestinationError) -> Self {
        Self::Destination(e)
    }
}

/// Everything a caller needs to start replicating and to publish its health.
pub struct ReplicationRuntime {
    /// The loop, ready to be moved onto a dedicated thread.
    pub replicator: Replicator,
    /// The shared status handle the health indicator reads.
    pub status: Arc<ReplicationStatus>,
    /// The health indicator to register under [`INDICATOR_NAME`].
    pub indicator: Arc<ReplicationHealthIndicator>,
    /// The resolved settings (useful for logging and for the CLI).
    pub settings: ReplicationSettings,
}

impl fmt::Debug for ReplicationRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplicationRuntime")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// The database file a `SQLite` URL names, or `None` for an in-memory or
/// non-`SQLite` target.
///
/// Accepts every spelling #1614 recognizes: `sqlite:///abs/path`,
/// `sqlite:path`, `file:path` (query string and all), and rejects the in-memory
/// forms — private and shared-cache alike — because neither survives the process
/// that holds it.
#[must_use]
pub fn database_file(url: &str) -> Option<PathBuf> {
    let target = if url.starts_with("file:") {
        url.to_owned()
    } else {
        let rest = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))?;
        if rest.is_empty() {
            return None;
        }
        rest.to_owned()
    };
    if target == ":memory:"
        || target == "file::memory:"
        || target.starts_with("file::memory:?")
        || target.contains("mode=memory")
    {
        return None;
    }
    // Reduce a `file:` URI to its path component; SQLite's URI query string
    // carries options, not path.
    let path = target
        .strip_prefix("file:")
        .map_or(target.as_str(), |rest| rest);
    let path = path.split(['?', '#']).next().unwrap_or(path);
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Strip `scheme://user:pass@` userinfo out of every URL embedded in `detail`.
///
/// Third-party error types (`reqwest`, `toml`) routinely quote the input that
/// failed, and that input can be a URL with a password in it. Everything this
/// feature prints — a tick error, a health-indicator detail, a CLI diagnostic —
/// goes through here first.
#[must_use]
pub fn redact_credentials(detail: &str) -> String {
    let mut out = String::with_capacity(detail.len());
    let mut rest = detail;
    while let Some(scheme_at) = rest.find("://") {
        let Some((before, after)) = rest.split_at_checked(scheme_at.saturating_add(3)) else {
            break;
        };
        out.push_str(before);
        // Userinfo, if any, ends at the first `@` before the next delimiter.
        let boundary = after.find(['/', '?', '#', ' ']).unwrap_or(after.len());
        if let Some(at) = after
            .get(..boundary)
            .and_then(|authority| authority.find('@'))
        {
            out.push_str("<redacted>@");
            rest = after.get(at.saturating_add(1)..).unwrap_or("");
        } else {
            out.push_str(after.get(..boundary).unwrap_or(""));
            rest = after.get(boundary..).unwrap_or("");
        }
    }
    out.push_str(rest);
    out
}

/// The app's own blob-storage destination, for the distinct-destination guard.
#[derive(Debug, Clone, Copy)]
pub struct StorageDestination<'a> {
    /// The bucket the app writes user blobs to.
    pub bucket: &'a str,
    /// Its endpoint, or `None` for AWS.
    pub endpoint: Option<&'a str>,
}

/// Reduce an endpoint to a comparable `host[:port]`, so two spellings of the
/// same destination compare equal and `None` (AWS) compares equal to itself.
fn canonical_authority(endpoint: Option<&str>) -> Option<String> {
    let endpoint = endpoint.map(str::trim).filter(|e| !e.is_empty())?;
    let parsed = url::Url::parse(endpoint).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(
        parsed
            .port()
            .map_or_else(|| host.clone(), |port| format!("{host}:{port}")),
    )
}

/// Read a credential from the environment variable config *names*.
fn credential_from_env(var: Option<&String>, field: &str) -> Result<String, SetupError> {
    let name = var
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| SetupError::Config {
            errors: vec![format!("[replication.s3] {field} is unset")],
        })?;
    let value = std::env::var(name).unwrap_or_default();
    if value.trim().is_empty() {
        return Err(SetupError::MissingCredentialEnv {
            var: name.to_owned(),
        });
    }
    Ok(value)
}

/// Build the destination named by `config`.
///
/// Must be called from a blocking thread: the S3 destination uses a blocking
/// HTTP client (see [`s3`]).
///
/// # Errors
///
/// See [`SetupError`].
pub fn build_destination(
    config: &ReplicationConfig,
    storage: Option<StorageDestination<'_>>,
) -> Result<Arc<dyn ReplicaDestination>, SetupError> {
    if let Some(path) = config.path.as_ref().filter(|p| !p.trim().is_empty()) {
        return Ok(Arc::new(FileDestination::new(path.trim())?));
    }
    let Some(s3) = config.s3.as_ref() else {
        return Err(SetupError::Config {
            errors: vec!["[replication] has no destination configured".to_owned()],
        });
    };
    let bucket = s3
        .bucket
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| SetupError::Config {
            errors: vec!["[replication.s3] bucket is unset".to_owned()],
        })?;
    // Bucket AND endpoint, matching #1619's `destinations_conflict`: the same
    // bucket name at two different providers is not a collision, and a
    // path-style vs virtual-hosted spelling of one endpoint is not a difference.
    if !config.allow_shared_bucket
        && storage.is_some_and(|storage| {
            storage.bucket == bucket
                && canonical_authority(storage.endpoint)
                    == canonical_authority(s3.endpoint.as_deref())
        })
    {
        return Err(SetupError::SharedBucket {
            bucket: bucket.to_owned(),
        });
    }

    #[cfg(feature = "http-client")]
    {
        let credentials = s3::S3Credentials {
            access_key_id: credential_from_env(s3.access_key_id_env.as_ref(), "access_key_id_env")?,
            secret_access_key: credential_from_env(
                s3.secret_access_key_env.as_ref(),
                "secret_access_key_env",
            )?,
        };
        let settings = s3::S3Settings {
            bucket: bucket.to_owned(),
            region: s3.region.clone().unwrap_or_else(|| "us-east-1".to_owned()),
            endpoint: s3.endpoint.clone(),
            force_path_style: s3.force_path_style,
        };
        Ok(Arc::new(s3::S3Destination::new(settings, credentials)?))
    }
    #[cfg(not(feature = "http-client"))]
    {
        let _ = bucket;
        Err(SetupError::Config {
            errors: vec![
                "[replication.s3] needs the `http-client` feature; this build of autumn-web \
                 was compiled without it. Use a `path` destination, or enable the feature."
                    .to_owned(),
            ],
        })
    }
}

/// Resolve `[replication]` into a ready-to-run [`ReplicationRuntime`].
///
/// `storage` is the app's own blob-storage destination when it has one, used
/// only for the distinct-destination guard. `clock` is the app's injected wall
/// clock: it dates every replicated artifact and anchors the health indicator's
/// startup grace, so a test that controls time controls both.
///
/// Must be called from a blocking thread (see [`build_destination`]).
///
/// # Errors
///
/// Returns [`SetupError::Disabled`] when replication is off — the caller treats
/// that as "nothing to do", not as a failure. Every other variant is a real
/// misconfiguration worth failing loudly on.
pub fn build(
    config: &ReplicationConfig,
    database_url: &str,
    profile: &str,
    storage: Option<StorageDestination<'_>>,
    clock: Arc<dyn ClockSource>,
) -> Result<ReplicationRuntime, SetupError> {
    if !config.enabled {
        return Err(SetupError::Disabled);
    }
    let errors = config.validation_errors();
    if !errors.is_empty() {
        return Err(SetupError::Config { errors });
    }
    if !(database_url.starts_with("sqlite:") || database_url.starts_with("file:")) {
        return Err(SetupError::NotSqlite {
            backend: crate::config::DatabaseBackend::detect(database_url)
                .map_or_else(|| "an unrecognized backend".to_owned(), |b| b.to_string()),
        });
    }
    let database_path = database_file(database_url).ok_or(SetupError::InMemory)?;

    let destination = build_destination(config, storage)?;
    let status = Arc::new(ReplicationStatus::new(destination.describe()));
    let settings = ReplicationSettings {
        database_path,
        root: segment::root_prefix(config.prefix.as_deref(), profile),
        sync_interval: config.sync_interval(),
        snapshot_interval: Duration::from_secs(config.snapshot_interval_secs.max(1)),
        max_wal_bytes: config.max_wal_bytes,
        retention: Duration::from_secs(config.retention_hours.max(1).saturating_mul(3600)),
        verify_interval: config.verify_interval(),
    };
    // The startup grace runs from the injected clock, and so does every
    // artifact the replicator stamps, so a test that freezes or steps time moves
    // both together (#1797).
    let started_at = clock.now();
    let indicator = Arc::new(ReplicationHealthIndicator::new(
        Arc::clone(&status),
        HealthThresholds {
            lag_alert_after: config.rpo().saturating_mul(LAG_ALERT_MULTIPLIER),
            startup_grace: STARTUP_GRACE,
        },
        started_at,
    ));
    let replicator = Replicator::new(
        settings.clone(),
        Arc::clone(&destination),
        Arc::clone(&status),
    )
    .with_clock(clock);
    Ok(ReplicationRuntime {
        replicator,
        status,
        indicator,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock `build` is handed in these tests: real time, since none of them
    /// assert on a timestamp.
    fn test_clock() -> Arc<dyn ClockSource> {
        Arc::new(crate::time::SystemClock)
    }

    #[test]
    fn database_file_understands_every_accepted_sqlite_spelling() {
        assert_eq!(
            database_file("sqlite:///var/lib/app.db"),
            Some(PathBuf::from("/var/lib/app.db"))
        );
        assert_eq!(
            database_file("sqlite:app.db"),
            Some(PathBuf::from("app.db"))
        );
        assert_eq!(
            database_file("sqlite://data/app.db"),
            Some(PathBuf::from("data/app.db"))
        );
        assert_eq!(
            database_file("file:/srv/app.db?mode=rwc"),
            Some(PathBuf::from("/srv/app.db"))
        );
    }

    #[test]
    fn database_file_rejects_in_memory_and_postgres_targets() {
        for url in [
            "sqlite::memory:",
            "sqlite://:memory:",
            "sqlite://",
            "file::memory:",
            "file::memory:?cache=shared",
            "file:app?mode=memory&cache=shared",
            "postgres://localhost/app",
        ] {
            assert_eq!(database_file(url), None, "{url} must not name a file");
        }
    }

    fn enabled_file_config(path: &str) -> ReplicationConfig {
        ReplicationConfig {
            enabled: true,
            path: Some(path.to_owned()),
            ..ReplicationConfig::default()
        }
    }

    #[test]
    fn build_refuses_a_disabled_section() {
        let config = ReplicationConfig::default();
        assert!(matches!(
            build(&config, "sqlite:app.db", "dev", None, test_clock()),
            Err(SetupError::Disabled)
        ));
    }

    #[test]
    fn build_refuses_postgres_and_in_memory_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = enabled_file_config(&dir.path().to_string_lossy());
        assert!(matches!(
            build(
                &config,
                "postgres://localhost/app",
                "prod",
                None,
                test_clock()
            ),
            Err(SetupError::NotSqlite { .. })
        ));
        assert!(matches!(
            build(&config, "sqlite::memory:", "prod", None, test_clock()),
            Err(SetupError::InMemory)
        ));
    }

    #[test]
    fn build_refuses_a_section_with_no_destination() {
        let config = ReplicationConfig {
            enabled: true,
            ..ReplicationConfig::default()
        };
        let err =
            build(&config, "sqlite:app.db", "prod", None, test_clock()).expect_err("must refuse");
        assert!(
            format!("{err}").contains("no destination is configured"),
            "{err}"
        );
    }

    #[test]
    fn build_refuses_two_destinations() {
        let config = ReplicationConfig {
            enabled: true,
            path: Some("/tmp/replicas".to_owned()),
            s3: Some(crate::config::ReplicationS3Config {
                bucket: Some("b".to_owned()),
                access_key_id_env: Some("A".to_owned()),
                secret_access_key_env: Some("S".to_owned()),
                ..crate::config::ReplicationS3Config::default()
            }),
            ..ReplicationConfig::default()
        };
        let err =
            build(&config, "sqlite:app.db", "prod", None, test_clock()).expect_err("must refuse");
        assert!(
            format!("{err}").contains("pick exactly one destination"),
            "{err}"
        );
    }

    #[test]
    fn build_resolves_settings_from_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = enabled_file_config(&dir.path().to_string_lossy());
        config.prefix = Some("replicas".to_owned());
        config.rpo_secs = 20;
        config.retention_hours = 2;

        let runtime =
            build(&config, "sqlite:///srv/app.db", "prod", None, test_clock()).expect("runtime");
        assert_eq!(runtime.settings.database_path, PathBuf::from("/srv/app.db"));
        assert_eq!(runtime.settings.root, "replicas/prod");
        assert_eq!(runtime.settings.sync_interval, Duration::from_secs(10));
        assert_eq!(runtime.settings.retention, Duration::from_secs(7200));
        assert!(runtime.status.snapshot().destination.starts_with("file://"));
    }

    #[test]
    fn a_shared_blob_storage_bucket_needs_an_opt_in() {
        let mut config = ReplicationConfig {
            enabled: true,
            s3: Some(crate::config::ReplicationS3Config {
                bucket: Some("shared".to_owned()),
                access_key_id_env: Some("AUTUMN_TEST_REPL_KEY".to_owned()),
                secret_access_key_env: Some("AUTUMN_TEST_REPL_SECRET".to_owned()),
                ..crate::config::ReplicationS3Config::default()
            }),
            ..ReplicationConfig::default()
        };
        let same = Some(StorageDestination {
            bucket: "shared",
            endpoint: None,
        });
        assert!(matches!(
            build_destination(&config, same),
            Err(SetupError::SharedBucket { .. })
        ));

        // The same bucket NAME at a different provider is not a collision.
        let elsewhere = Some(StorageDestination {
            bucket: "shared",
            endpoint: Some("https://minio.test:9000"),
        });
        assert!(matches!(
            build_destination(&config, elsewhere),
            Err(SetupError::MissingCredentialEnv { .. })
        ));

        config.allow_shared_bucket = true;
        // Still fails, but on the credential env var — proving the bucket guard
        // is what was tripping before, not a missing credential.
        assert!(matches!(
            build_destination(&config, same),
            Err(SetupError::MissingCredentialEnv { .. })
        ));
    }

    #[test]
    fn credentials_are_stripped_from_any_embedded_url() {
        let raw = "error sending request for url (https://KEY:SECRET@minio.test:9000/b/k)";
        let redacted = redact_credentials(raw);
        assert!(!redacted.contains("SECRET"), "{redacted}");
        assert!(!redacted.contains("KEY"), "{redacted}");
        assert!(redacted.contains("minio.test:9000/b/k"), "{redacted}");
        // A URL without userinfo is left exactly as it was.
        assert_eq!(
            redact_credentials("failed for url (https://minio.test/b/k)"),
            "failed for url (https://minio.test/b/k)"
        );
        assert_eq!(redact_credentials("no url here"), "no url here");
        // Several URLs in one message are all covered.
        let two = redact_credentials("a https://u:p@x.test/1 and b postgres://u2:p2@y.test/2");
        assert!(!two.contains("p2"), "{two}");
        assert!(
            two.contains("x.test/1") && two.contains("y.test/2"),
            "{two}"
        );
    }

    #[test]
    fn endpoint_spellings_of_one_destination_compare_equal() {
        assert_eq!(canonical_authority(None), None);
        assert_eq!(
            canonical_authority(Some("https://MinIO.Test:9000")),
            canonical_authority(Some("https://minio.test:9000/"))
        );
        assert_ne!(
            canonical_authority(Some("https://minio.test:9000")),
            canonical_authority(Some("https://minio.test:9001"))
        );
    }

    #[test]
    fn sync_interval_defaults_to_half_the_rpo_with_a_one_second_floor() {
        let config = ReplicationConfig::default();
        assert_eq!(config.rpo_secs, 10, "the documented default RPO");
        assert_eq!(config.sync_interval(), Duration::from_secs(5));

        let tight = ReplicationConfig {
            rpo_secs: 1,
            ..ReplicationConfig::default()
        };
        assert_eq!(tight.sync_interval(), Duration::from_secs(1));

        let explicit = ReplicationConfig {
            sync_interval_secs: Some(30),
            ..ReplicationConfig::default()
        };
        assert_eq!(explicit.sync_interval(), Duration::from_secs(30));
    }

    #[test]
    fn verification_can_be_switched_off() {
        let mut config = ReplicationConfig::default();
        assert_eq!(config.verify_interval(), Some(Duration::from_secs(21_600)));
        config.verify_interval_secs = 0;
        assert_eq!(config.verify_interval(), None);
    }
}
